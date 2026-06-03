//! A safe, device-agnostic HDV **PCI device**: build the `HDV_PCI_DEVICE_INTERFACE`
//! vtable, marshal HDV's `extern "system"` callbacks (delivered on HDV's own
//! threads) into a safe [`PciOps`] trait, and own the create→teardown lifetime.
//!
//! This is the reusable substrate the attach spike exercises and that
//! `virtio-hdv` (milestone 2) builds on by implementing [`PciOps`] over OpenVMM's
//! `VirtioPciDevice`. It knows nothing about virtio.
//!
//! # Lifetime / ownership
//! The device's behaviour lives in a heap [`Context`] whose raw pointer is handed
//! to HDV as the `deviceContext`. HDV owns it for the device's lifetime and hands
//! it back to the `Teardown` callback, which reclaims the box. The vtable lives
//! inside that same `Context`, so it stays valid exactly as long as HDV may call
//! it. Dropping the [`PciDevice`] drops its [`DeviceHost`], which calls
//! `HdvTeardownDeviceHost` → our `Teardown` trampoline → frees the `Context`.

use crate::{Device, DeviceHost, Error, Result};
use hdv_sys as sys;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

/// `E_FAIL` — returned from a callback whose Rust body panicked, so a panic never
/// unwinds across the FFI boundary into HDV.
const E_FAIL: sys::HRESULT = 0x8000_4005u32 as sys::HRESULT;

/// The product's **well-known** device class/instance GUIDs. These are fixed
/// constants (Decision A): a consumer must declare its HCS **`FlexibleIov`** slot
/// with them so the guest gets a VPCI bus for the device — the slot's `EmulatorId`
/// must equal [`HVFS_DEVICE_CLASS_ID`] (the HDV `DeviceClassId`) and the slot's
/// map-key GUID must equal [`HVFS_DEVICE_INSTANCE_ID`] (the HDV `DeviceInstanceId`).
/// Format them for the JSON document with [`guid_to_string`].
///
// GUID assignment is settled, not open work (docs/roadmap.md, shipped): the device
// **instance** id is caller-supplied over the C ABI; the **class** id must be the
// platform-mandated `VIRTIO_FS_DEVICE_CLASS_ID` (a custom class is refused for a 2nd
// virtio-fs device); and the device-**host** id is a per-process constant (Model A).
// These `HVFS_*` constants remain as defaults used by tests and the pre-ABI paths.
pub const HVFS_DEVICE_CLASS_ID: sys::GUID = sys::GUID {
    Data1: 0xa7e1_1e40,
    Data2: 0x0001,
    Data3: 0x4a7e,
    Data4: [0x9c, 0x00, 0xa7, 0xe1, 0x00, 0x00, 0x00, 0x01],
};
pub const HVFS_DEVICE_INSTANCE_ID: sys::GUID = sys::GUID {
    Data1: 0xa7e1_1e40,
    Data2: 0x0001,
    Data3: 0x4a7e,
    Data4: [0x9c, 0x00, 0xa7, 0xe1, 0x00, 0x00, 0x00, 0x02],
};

/// Device-**host** identity for the proxy path — the `ctx` argument of
/// `HdvInitializeDeviceHostForProxy`, which (per disassembly) reads it as a 16-byte
/// GUID and is **not** nullable. Distinct from the per-device class/instance ids.
pub const HVFS_DEVICE_HOST_ID: sys::GUID = sys::GUID {
    Data1: 0xa7e1_1e40,
    Data2: 0x0001,
    Data3: 0x4a7e,
    Data4: [0x9c, 0x00, 0xa7, 0xe1, 0x00, 0x00, 0x00, 0x03],
};

/// virtio-fs's **well-known** device type id (`872270E1-A899-4AF6-B454-7193634435AD`,
/// WSL's `VIRTIO_FS_DEVICE_ID`) — the `DeviceClassId`/`EmulatorId` **every** virtio-fs
/// `FlexibleIov` device must use. This is the platform-mandated class for the
/// device-per-share model: the hotplug spike found a *custom* class id works for one
/// device but the VID rejects a **second** with `ERROR_HV_INVALID_PARAMETER`, whereas
/// the well-known id lets N devices coexist (distinguished only by a unique instance
/// id), exactly as WSL's single `wsldevicehost` carries N virtio-fs devices.
pub const VIRTIO_FS_DEVICE_CLASS_ID: sys::GUID = sys::GUID {
    Data1: 0x872270E1,
    Data2: 0xA899,
    Data3: 0x4AF6,
    Data4: [0xB4, 0x54, 0x71, 0x93, 0x63, 0x44, 0x35, 0xAD],
};

/// Format a GUID in canonical lowercase `8-4-4-4-12` form (no braces) — the form
/// the HCS schema uses for `FlexibleIov` device keys and `EmulatorId`.
pub fn guid_to_string(g: &sys::GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.Data1,
        g.Data2,
        g.Data3,
        g.Data4[0],
        g.Data4[1],
        g.Data4[2],
        g.Data4[3],
        g.Data4[4],
        g.Data4[5],
        g.Data4[6],
        g.Data4[7],
    )
}

/// Parse a canonical `8-4-4-4-12` GUID string (case-insensitive; optional surrounding
/// braces) into a [`sys::GUID`]; `None` on any malformed input. The inverse of
/// [`guid_to_string`] — used to accept a caller-supplied `DeviceInstanceId` over the
/// C ABI and re-emit it in canonical form for the HCS document.
pub fn guid_from_string(s: &str) -> Option<sys::GUID> {
    let s = s.trim();
    let s = s
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .unwrap_or(s);
    let p: Vec<&str> = s.split('-').collect();
    if p.len() != 5
        || [8, 4, 4, 4, 12] != [p[0].len(), p[1].len(), p[2].len(), p[3].len(), p[4].len()]
    {
        return None;
    }
    let mut data4 = [0u8; 8];
    let tail = format!("{}{}", p[3], p[4]); // the 8 trailing bytes, as 16 hex digits
    for (i, b) in data4.iter_mut().enumerate() {
        *b = u8::from_str_radix(&tail[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(sys::GUID {
        Data1: u32::from_str_radix(p[0], 16).ok()?,
        Data2: u16::from_str_radix(p[1], 16).ok()?,
        Data3: u16::from_str_radix(p[2], 16).ok()?,
        Data4: data4,
    })
}

/// The PCI identity + probed BAR sizes a device reports to HDV's `GetDetails`.
/// `probed_bars[i]` is the value BAR *i* returns to a sizing probe (all-ones
/// write): `0` for an absent BAR, or e.g. `0xFFFF_F000` for a 4 KiB 32-bit
/// non-prefetchable memory BAR.
#[derive(Clone, Copy, Debug)]
pub struct PciDetails {
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision_id: u8,
    pub prog_if: u8,
    pub sub_class: u8,
    pub base_class: u8,
    pub sub_vendor_id: u16,
    pub sub_system_id: u16,
    pub probed_bars: [u32; sys::HDV_PCI_BAR_COUNT as usize],
}

/// The device behaviour HDV drives. Every method runs on an HDV worker thread; a
/// panic in any of them is caught at the FFI boundary and turned into an error to
/// HDV, never an abort.
pub trait PciOps: Send + Sync {
    /// PnP identity + probed BAR sizes (the `GetDetails` callback).
    fn details(&self) -> PciDetails;
    /// Read one dword of PCI config space at the given byte `offset`.
    fn read_config(&self, offset: u32) -> u32;
    /// Write one dword of PCI config space at the given byte `offset`.
    fn write_config(&self, offset: u32, value: u32);
    /// BAR MMIO read; default returns zeroes (enough for enumeration).
    fn read_bar(&self, bar: u8, offset: u64, data: &mut [u8]) {
        let _ = (bar, offset);
        data.fill(0);
    }
    /// BAR MMIO write; default ignores.
    fn write_bar(&self, bar: u8, offset: u64, data: &[u8]) {
        let _ = (bar, offset, data);
    }
    /// Device-specific configuration strings (`SetConfiguration`); default ignores.
    fn set_configuration(&self, values: &[Vec<u16>]) {
        let _ = values;
    }
    /// Bring the device online (`Start`); default succeeds.
    fn start(&self) -> bool {
        true
    }
    /// Take the device offline (`Stop`); default no-op.
    fn stop(&self) {}
}

/// Heap-resident device state owned by HDV for the device's lifetime. Holds the
/// behaviour plus the vtable HDV calls through (kept here so it outlives every
/// callback and is freed together in `Teardown`).
struct Context {
    ops: Box<dyn PciOps>,
    vtable: sys::HDV_PCI_DEVICE_INTERFACE,
}

impl Context {
    fn new(ops: Box<dyn PciOps>) -> Box<Self> {
        Box::new(Self {
            ops,
            vtable: sys::HDV_PCI_DEVICE_INTERFACE {
                Version: sys::HDV_PCI_INTERFACE_VERSION::Version1,
                Initialize: Some(tramp_initialize),
                Teardown: Some(tramp_teardown),
                SetConfiguration: Some(tramp_set_configuration),
                GetDetails: Some(tramp_get_details),
                Start: Some(tramp_start),
                Stop: Some(tramp_stop),
                ReadConfigSpace: Some(tramp_read_config),
                WriteConfigSpace: Some(tramp_write_config),
                ReadInterceptedMemory: Some(tramp_read_mem),
                WriteInterceptedMemory: Some(tramp_write_mem),
            },
        })
    }
}

/// Recover a shared reference to the device behaviour from the `deviceContext`.
///
/// # Safety
/// `ctx` must be the live `*mut Context` pointer we passed to
/// `HdvCreateDeviceInstance` (HDV keeps it valid until `Teardown`).
unsafe fn ops_of<'a>(ctx: sys::PVOID) -> &'a dyn PciOps {
    let c = unsafe { &*(ctx as *const Context) };
    &*c.ops
}

/// Run an HRESULT-returning callback body, converting a panic into `E_FAIL` so it
/// never crosses the FFI boundary.
fn guard_hr(f: impl FnOnce() -> sys::HRESULT) -> sys::HRESULT {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(E_FAIL)
}

unsafe extern "system" fn tramp_initialize(_ctx: sys::PVOID) -> sys::HRESULT {
    sys::S_OK
}

unsafe extern "system" fn tramp_teardown(ctx: sys::PVOID) {
    // Reclaim the Context (and the vtable inside it). After this returns HDV must
    // not call any callback again. Swallow panics — Teardown cannot fail.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !ctx.is_null() {
            drop(unsafe { Box::from_raw(ctx as *mut Context) });
        }
    }));
}

unsafe extern "system" fn tramp_set_configuration(
    ctx: sys::PVOID,
    count: u32,
    values: *const sys::LPCWSTR,
) -> sys::HRESULT {
    guard_hr(|| {
        let ops = unsafe { ops_of(ctx) };
        let mut owned: Vec<Vec<u16>> = Vec::new();
        if !values.is_null() {
            for i in 0..count as usize {
                let p = unsafe { *values.add(i) };
                owned.push(unsafe { wide_to_vec(p) });
            }
        }
        ops.set_configuration(&owned);
        sys::S_OK
    })
}

unsafe extern "system" fn tramp_get_details(
    ctx: sys::PVOID,
    pnp_id: *mut sys::HDV_PCI_PNP_ID,
    probed_bars_count: u32,
    probed_bars: *mut u32,
) -> sys::HRESULT {
    guard_hr(|| {
        let d = unsafe { ops_of(ctx) }.details();
        if !pnp_id.is_null() {
            unsafe {
                *pnp_id = sys::HDV_PCI_PNP_ID {
                    VendorID: d.vendor_id,
                    DeviceID: d.device_id,
                    RevisionID: d.revision_id,
                    ProgIf: d.prog_if,
                    SubClass: d.sub_class,
                    BaseClass: d.base_class,
                    SubVendorID: d.sub_vendor_id,
                    SubSystemID: d.sub_system_id,
                };
            }
        }
        if !probed_bars.is_null() {
            let n = (probed_bars_count as usize).min(d.probed_bars.len());
            for (i, v) in d.probed_bars.iter().take(n).enumerate() {
                unsafe { *probed_bars.add(i) = *v };
            }
        }
        sys::S_OK
    })
}

unsafe extern "system" fn tramp_start(ctx: sys::PVOID) -> sys::HRESULT {
    guard_hr(|| {
        if unsafe { ops_of(ctx) }.start() {
            sys::S_OK
        } else {
            E_FAIL
        }
    })
}

unsafe extern "system" fn tramp_stop(ctx: sys::PVOID) {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { ops_of(ctx) }.stop()));
}

unsafe extern "system" fn tramp_read_config(
    ctx: sys::PVOID,
    offset: u32,
    value: *mut u32,
) -> sys::HRESULT {
    guard_hr(|| {
        let v = unsafe { ops_of(ctx) }.read_config(offset);
        if !value.is_null() {
            unsafe { *value = v };
        }
        sys::S_OK
    })
}

unsafe extern "system" fn tramp_write_config(
    ctx: sys::PVOID,
    offset: u32,
    value: u32,
) -> sys::HRESULT {
    guard_hr(|| {
        unsafe { ops_of(ctx) }.write_config(offset, value);
        sys::S_OK
    })
}

unsafe extern "system" fn tramp_read_mem(
    ctx: sys::PVOID,
    bar: sys::HDV_PCI_BAR_SELECTOR,
    offset: u64,
    length: u64,
    value: *mut u8,
) -> sys::HRESULT {
    guard_hr(|| {
        if !value.is_null() && length > 0 {
            let buf = unsafe { std::slice::from_raw_parts_mut(value, length as usize) };
            unsafe { ops_of(ctx) }.read_bar(bar as u8, offset, buf);
        }
        sys::S_OK
    })
}

unsafe extern "system" fn tramp_write_mem(
    ctx: sys::PVOID,
    bar: sys::HDV_PCI_BAR_SELECTOR,
    offset: u64,
    length: u64,
    value: *const u8,
) -> sys::HRESULT {
    guard_hr(|| {
        if !value.is_null() && length > 0 {
            let buf = unsafe { std::slice::from_raw_parts(value, length as usize) };
            unsafe { ops_of(ctx) }.write_bar(bar as u8, offset, buf);
        }
        sys::S_OK
    })
}

/// Copy a NUL-terminated wide string into an owned `Vec<u16>` (without the NUL).
///
/// # Safety
/// `p` must be null or a valid pointer to a NUL-terminated UTF-16 string.
unsafe fn wide_to_vec(p: sys::LPCWSTR) -> Vec<u16> {
    if p.is_null() {
        return Vec::new();
    }
    let mut len = 0usize;
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    unsafe { std::slice::from_raw_parts(p, len) }.to_vec()
}

/// A PCI device attached to a guest over HDV. Holds a (possibly shared) reference
/// to the [`DeviceHost`]. Because HDV allows only **one device host per VM** (the
/// hotplug spike found a second `from_proxy` is rejected), multiple devices share
/// one host via `Arc`; the host — and with it every device still on it — is torn
/// down when the last `Arc` ref drops (`HdvTeardownDeviceHost`).
pub struct PciDevice {
    // The behaviour `Context` is owned by HDV and reclaimed in the `Teardown`
    // trampoline, so it is not held here. Per-device removal (without dropping the
    // host) is driven host-side by removing the device's FlexibleIov slot.
    host: Arc<DeviceHost>,
    device: Device,
}

impl PciDevice {
    /// Create a PCI device instance on `host`, driven by `ops`, with the
    /// well-known [`HVFS_DEVICE_INSTANCE_ID`]. Non-blocking; HDV then calls `ops`
    /// on its own threads as the guest touches the device.
    pub fn create(host: DeviceHost, ops: Box<dyn PciOps>) -> Result<Self> {
        Self::create_with_instance(host, ops, &HVFS_DEVICE_INSTANCE_ID)
    }

    /// Like [`create`](Self::create) but with a caller-chosen `DeviceInstanceId`,
    /// so more than one such device can coexist in a guest — each needs a distinct
    /// instance id and a matching `FlexibleIov` slot (map-key == this id). The
    /// class id stays [`HVFS_DEVICE_CLASS_ID`].
    //
    // The class id and device-host id are fixed by design, not pending work:
    // virtio-fs must use the platform-mandated class (a custom one is refused for a
    // 2nd device) and the device-host id is a per-process constant (Model A). Only
    // the instance id varies — which is what the hotplug spike uses to stand up two
    // devices at once. See docs/roadmap.md ("GUID assignment is settled").
    pub fn create_with_instance(
        host: DeviceHost,
        ops: Box<dyn PciOps>,
        instance_id: &sys::GUID,
    ) -> Result<Self> {
        Self::create_shared(Arc::new(host), ops, &HVFS_DEVICE_CLASS_ID, instance_id)
    }

    /// Like [`create_with_instance`](Self::create_with_instance) but on a **shared**
    /// device host and with a caller-chosen `class_id` (`DeviceClassId`/`EmulatorId`),
    /// so several devices can live on the one host HDV permits per VM (the hotplug
    /// spike's multi-share model). For >1 concurrent virtio-fs device the `class_id`
    /// must be the **well-known** [`VIRTIO_FS_DEVICE_CLASS_ID`]: the spike found a
    /// *custom* class works for one device but the VID rejects a second with
    /// `ERROR_HV_INVALID_PARAMETER`, while the well-known id lets N coexist
    /// (distinguished only by `instance_id`), as WSL's single `wsldevicehost` does.
    /// The caller keeps the `Arc` and hands a clone per device; the host — and every
    /// device on it — is torn down when the last clone drops.
    pub fn create_shared(
        host: Arc<DeviceHost>,
        ops: Box<dyn PciOps>,
        class_id: &sys::GUID,
        instance_id: &sys::GUID,
    ) -> Result<Self> {
        let ctx = Box::into_raw(Context::new(ops));
        let mut device: sys::HDV_DEVICE = std::ptr::null_mut();
        // SAFETY: `host.raw()` is a live device host; `ctx` points at a live
        // Context whose `vtable` we pass as the interface; both stay valid for the
        // device lifetime (Context is reclaimed only in Teardown). On failure we
        // reclaim the Context ourselves below.
        let code = unsafe {
            sys::HdvCreateDeviceInstance(
                host.raw(),
                sys::HDV_DEVICE_TYPE::Pci,
                class_id,
                instance_id,
                &(*ctx).vtable as *const sys::HDV_PCI_DEVICE_INTERFACE as *const c_void,
                ctx as sys::PVOID,
                &mut device,
            )
        };
        if code < 0 {
            // HDV never took ownership — reclaim the Context.
            drop(unsafe { Box::from_raw(ctx) });
            return Err(Error(code));
        }
        Ok(Self {
            host,
            device: Device(device),
        })
    }

    /// The HDV device handle (for guest-memory / doorbell / interrupt ops in
    /// milestone 2).
    pub fn device(&self) -> Device {
        self.device
    }

    /// The (shared) device host this device lives on.
    pub fn host(&self) -> &DeviceHost {
        &self.host
    }

    /// A new reference to the shared device host — for creating sibling devices on
    /// the same host (only one host per VM is permitted).
    pub fn host_arc(&self) -> Arc<DeviceHost> {
        self.host.clone()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the GUID <-> canonical-string helpers. `guid_from_string`
    //! validates the caller-supplied `instance_id` at the C ABI boundary, and
    //! `guid_to_string` emits the form the HCS `FlexibleIov` document requires, so
    //! their correctness is load-bearing. Comparisons round-trip through
    //! `guid_to_string` to avoid relying on `GUID: PartialEq`.
    use super::*;

    #[test]
    fn to_string_is_canonical_lowercase_no_braces() {
        assert_eq!(
            guid_to_string(&VIRTIO_FS_DEVICE_CLASS_ID),
            "872270e1-a899-4af6-b454-7193634435ad"
        );
    }

    #[test]
    fn well_known_constants_round_trip() {
        for g in [
            HVFS_DEVICE_CLASS_ID,
            HVFS_DEVICE_INSTANCE_ID,
            HVFS_DEVICE_HOST_ID,
            VIRTIO_FS_DEVICE_CLASS_ID,
        ] {
            let s = guid_to_string(&g);
            let parsed = guid_from_string(&s).expect("canonical string must parse");
            assert_eq!(guid_to_string(&parsed), s, "round-trip must be stable");
        }
    }

    #[test]
    fn from_string_accepts_braces_and_uppercase() {
        let canonical = "872270e1-a899-4af6-b454-7193634435ad";
        let variants = [
            "872270E1-A899-4AF6-B454-7193634435AD",     // uppercase
            "{872270e1-a899-4af6-b454-7193634435ad}",   // braces
            "  872270e1-a899-4af6-b454-7193634435ad  ", // surrounding whitespace
            "{872270E1-A899-4AF6-B454-7193634435AD}",   // braces + uppercase
        ];
        for v in variants {
            let g = guid_from_string(v).unwrap_or_else(|| panic!("should parse: {v:?}"));
            assert_eq!(guid_to_string(&g), canonical, "variant {v:?}");
        }
    }

    #[test]
    fn from_string_rejects_malformed() {
        let bad = [
            "",                                           // empty
            "not-a-guid",                                 // nonsense
            "872270e1-a899-4af6-b454",                    // too few groups
            "872270e1-a899-4af6-b454-7193634435ad-extra", // too many groups
            "872270e1a899-4af6-b454-7193634435ad",        // wrong group lengths
            "gggggggg-a899-4af6-b454-7193634435ad",       // non-hex digits
            "872270e1-a899-4af6-b454-7193634435a",        // last group too short
        ];
        for b in bad {
            assert!(guid_from_string(b).is_none(), "should reject {b:?}");
        }
    }

    #[test]
    fn class_id_differs_from_instance_id() {
        // The product's class and instance constants must not collide.
        assert_ne!(
            guid_to_string(&HVFS_DEVICE_CLASS_ID),
            guid_to_string(&HVFS_DEVICE_INSTANCE_ID)
        );
    }
}

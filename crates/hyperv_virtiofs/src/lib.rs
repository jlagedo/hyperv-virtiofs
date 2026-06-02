//! `hyperv_virtiofs` — the public C ABI. This is the whole product surface: a
//! standalone, host-agnostic DLL ("virtiofsd for Hyper-V") that attaches an
//! OpenVMM virtio-fs device to an externally-owned HCS/Hyper-V guest over HDV.
//!
//! It contains **no** consumer concepts (no Atelier, no sessions, no broker). Its
//! vocabulary is purely virtio-fs/HDV: compute systems, tags, directory maps,
//! read-only. Any host can drive it via the C ABI; Atelier binds it with Go's
//! `syscall.NewLazyDLL` (no cgo), exactly as it binds `computecore.dll`.
//!
//! ## ABI contract (mirrored in `include/hyperv_virtiofs.h`)
//! - Every function returns `i32`: `0` = OK, `< 0` = error; on error,
//!   [`hvfs_last_error`] holds a thread-local message.
//! - Handles are opaque (`hvfs_device*`), never passed by value.
//! - Every `*const c_char` is **borrowed**: the caller owns inputs; outputs are
//!   thread-local or delivered via the logger callback. Nothing crosses the
//!   allocator boundary, so there is no `free` footgun.
//! - **No panic ever crosses the boundary**: every entry point runs inside
//!   [`catch_unwind`]; a panic becomes [`HVFS_ERR_PANIC`].
//!
//! Status: `attach`/`detach` are live — `hvfs_attach` opens the compute system,
//! registers an HDV device host over the ExternalRestricted proxy path, and
//! attaches an OpenVMM virtio-fs device, so a guest mounts the share through this
//! ABI. `hvfs_set_shares` still returns [`HVFS_ERR_NOT_IMPLEMENTED`] (live re-share
//! is a follow-up; the initial share comes in via `device_json`).

// The public type names mirror the C ABI (snake_case) on purpose.
#![allow(non_camel_case_types)]

use hcs_sys::{HcsCloseComputeSystem, HcsOpenComputeSystem, GENERIC_ALL, HCS_SYSTEM};
use hdv::pci::HVFS_DEVICE_HOST_ID;
use hdv::proxy::DeviceHostSupport;
use hdv::DeviceHost;
use serde::Deserialize;
use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr;
use virtio_hdv::VirtioHdvDevice;

/// Bump on any breaking change to the C ABI. The consumer checks this at load.
pub const HVFS_ABI_VERSION: u32 = 1;

// Status codes. Keep in sync with the header.
pub const HVFS_OK: i32 = 0;
pub const HVFS_ERR_INVALID_ARG: i32 = -1;
pub const HVFS_ERR_NOT_IMPLEMENTED: i32 = -2;
pub const HVFS_ERR_PANIC: i32 = -3;
pub const HVFS_ERR_HDV: i32 = -4;

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(msg: impl Into<Vec<u8>>) {
    let c = CString::new(msg).unwrap_or_else(|_| CString::new("error").unwrap());
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(c));
}

/// Run an FFI body under `catch_unwind`, converting a panic into `HVFS_ERR_PANIC`
/// so it can never abort the host process across the ABI boundary.
fn guard(f: impl FnOnce() -> i32) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(_) => {
            set_last_error("panic caught at FFI boundary");
            HVFS_ERR_PANIC
        }
    }
}

/// Borrow a C string argument as `&str`, or record an error and bail.
fn cstr<'a>(p: *const c_char, what: &str) -> Result<&'a str, i32> {
    if p.is_null() {
        set_last_error(format!("null argument: {what}"));
        return Err(HVFS_ERR_INVALID_ARG);
    }
    // SAFETY: caller contract — `p` is a valid NUL-terminated C string for the
    // duration of the call (borrowed, not retained).
    match unsafe { CStr::from_ptr(p) }.to_str() {
        Ok(s) => Ok(s),
        Err(_) => {
            set_last_error(format!("non-UTF-8 argument: {what}"));
            Err(HVFS_ERR_INVALID_ARG)
        }
    }
}

/// The `device_json` contract for [`hvfs_attach`] — the initial single share plus
/// the guest RAM size. (Live multi-share updates are [`hvfs_set_shares`]'s job,
/// still a follow-up.)
#[derive(Deserialize)]
struct DeviceConfig {
    /// virtio-fs mount tag the guest uses: `mount -t virtiofs <tag> …`.
    tag: String,
    /// Host directory to share.
    path: String,
    /// Read-only request. Accepted but **not yet enforced** by the FUSE backend.
    #[serde(default)]
    ro: bool,
    /// Guest RAM in MiB — the GPA ceiling the virtqueues may reference. Supplied by
    /// the caller because `HcsGetComputeSystemProperties` isn't bound; it must match
    /// the compute system's actual memory.
    memory_mb: u64,
}

/// RAII for an opened `HCS_SYSTEM`: closes it on drop, so every early return in
/// [`hvfs_attach`] releases the handle and the device's teardown closes it last.
struct OwnedSystem(HCS_SYSTEM);

impl Drop for OwnedSystem {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from `HcsOpenComputeSystem` and is closed once.
            unsafe { HcsCloseComputeSystem(self.0) };
        }
    }
}

/// Encode a `&str` as a NUL-terminated UTF-16 string for the HCS wide-char API.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Opaque device handle. Box-allocated; freed by [`hvfs_detach`].
pub struct hvfs_device {
    // Field order = drop order. The HDV device must tear down *before* the COM
    // support object it was registered through, and *before* the HCS handle closes.
    _device: VirtioHdvDevice,
    _support: DeviceHostSupport,
    _system: OwnedSystem,
}

// SAFETY: the handle is returned to C as an opaque pointer and may be detached from
// a different thread than the one that attached it. Everything it owns is a
// process-global, thread-agnostic handle — the HDV device/host, the `HCS_SYSTEM`,
// and the `IVmDeviceHostSupport` COM object — none tied to a thread; on drop the
// support object is only released, never called back into. So moving the box across
// threads is sound.
unsafe impl Send for hvfs_device {}

/// Returns the ABI version the DLL implements. Call right after load; refuse on
/// mismatch with [`HVFS_ABI_VERSION`] in the header you compiled against.
#[no_mangle]
pub extern "C" fn hvfs_abi_version() -> u32 {
    HVFS_ABI_VERSION
}

/// Attach a virtio-fs device to an externally-owned HCS compute system, by id.
/// Non-blocking: serving runs on the DLL's own threads. On success `*out` holds a
/// handle to pass to [`hvfs_set_shares`] / [`hvfs_detach`].
///
/// `device_json` is the initial share + guest memory:
/// `{ "tag": "ws", "path": "C:\\host\\dir", "ro": false, "memory_mb": 512 }`
/// — `tag` is the virtio-fs mount tag, `path` the host directory, `ro` is accepted
/// but not yet enforced, and `memory_mb` must equal the compute system's RAM.
///
/// The caller's compute-system document **must** pre-declare a `FlexibleIov` slot
/// whose map-key GUID is the well-known `HVFS_DEVICE_INSTANCE_ID` and whose
/// `EmulatorId` is `HVFS_DEVICE_CLASS_ID`, `HostingModel` `"ExternalRestricted"`
/// (see `hdv::pci`). Those device GUIDs are fixed today — making them caller-chosen
/// is a tracked follow-up.
///
/// # Safety
/// `hcs_system_id` and `device_json` must be valid NUL-terminated C strings;
/// `out` must be a valid, writable `*mut *mut hvfs_device`.
#[no_mangle]
pub unsafe extern "C" fn hvfs_attach(
    hcs_system_id: *const c_char,
    device_json: *const c_char,
    out: *mut *mut hvfs_device,
) -> i32 {
    guard(|| {
        if out.is_null() {
            set_last_error("null out parameter");
            return HVFS_ERR_INVALID_ARG;
        }
        // SAFETY: checked non-null above; caller guarantees writability.
        unsafe { *out = ptr::null_mut() };

        let id = match cstr(hcs_system_id, "hcs_system_id") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let device_json = match cstr(device_json, "device_json") {
            Ok(s) => s,
            Err(e) => return e,
        };

        // TODO(caller-guids): device GUIDs (host/class/instance) are the fixed
        // well-known constants in `hdv::pci`; the consumer must mirror them in its
        // FlexibleIov slot. Tracked follow-up: accept overrides here in device_json.
        let cfg: DeviceConfig = match serde_json::from_str(device_json) {
            Ok(c) => c,
            Err(e) => {
                set_last_error(format!("device_json parse error: {e}"));
                return HVFS_ERR_INVALID_ARG;
            }
        };
        let _ = cfg.ro; // accepted but not yet enforced (see doc comment)

        // Turn the caller's system id into a live HCS_SYSTEM handle, wrapped at once
        // so any early return below closes it.
        let mut system: HCS_SYSTEM = ptr::null_mut();
        let idw = wide(id);
        // SAFETY: `idw` is a valid NUL-terminated wide string; `system` is a valid out ptr.
        let hr = unsafe { HcsOpenComputeSystem(idw.as_ptr(), GENERIC_ALL, &mut system) };
        if hr < 0 {
            set_last_error(format!(
                "HcsOpenComputeSystem failed: HRESULT {:#010x}",
                hr as u32
            ));
            return HVFS_ERR_HDV;
        }
        let owned = OwnedSystem(system);

        // Register an HDV device host against that system over the proven
        // ExternalRestricted proxy path, then create the virtio-fs device on it.
        // SAFETY: `owned` (hence the system handle) outlives `support` and the device.
        let support = unsafe { DeviceHostSupport::new(owned.0) };
        // SAFETY: HVFS_DEVICE_HOST_ID is a live GUID constant; `support` is a live
        // IVmDeviceHostSupport that outlives the returned device host.
        let host =
            match unsafe { DeviceHost::from_proxy(&HVFS_DEVICE_HOST_ID, support.as_iunknown()) } {
                Ok(h) => h,
                Err(e) => {
                    set_last_error(format!("HdvInitializeDeviceHostForProxy failed: {e}"));
                    return HVFS_ERR_HDV;
                }
            };

        let device = match VirtioHdvDevice::attach(
            host,
            Path::new(&cfg.path),
            &cfg.tag,
            cfg.memory_mb.saturating_mul(1024 * 1024),
        ) {
            Ok(d) => d,
            Err(e) => {
                set_last_error(format!("attach virtio-fs over HDV: {e}"));
                return HVFS_ERR_HDV;
            }
        };

        let dev = Box::new(hvfs_device {
            _device: device,
            _support: support,
            _system: owned,
        });
        // SAFETY: `out` checked non-null above and guaranteed writable by the caller.
        unsafe { *out = Box::into_raw(dev) };
        HVFS_OK
    })
}

/// Replace the device's directory map. `shares_json` is `{"tag":{"path":..,
/// "ro":bool}, ...}` — the moral equivalent of VZ's `SetShare`. Pushed live; an
/// empty map detaches all directories without tearing the device down.
///
/// # Safety
/// `dev` must be a handle from [`hvfs_attach`]; `shares_json` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn hvfs_set_shares(dev: *mut hvfs_device, shares_json: *const c_char) -> i32 {
    guard(|| {
        if dev.is_null() {
            set_last_error("null device handle");
            return HVFS_ERR_INVALID_ARG;
        }
        let _shares = match cstr(shares_json, "shares_json") {
            Ok(s) => s,
            Err(e) => return e,
        };
        // TODO(spike-3): parse + validate (jail each path, RESOLVE_BENEATH-equiv),
        // then push the map into the running FUSE server.
        set_last_error("hvfs_set_shares: not yet implemented");
        HVFS_ERR_NOT_IMPLEMENTED
    })
}

/// Detach the device and free the handle. Idempotent-safe for a non-null handle
/// obtained from [`hvfs_attach`]; joins the serving threads.
///
/// # Safety
/// `dev` must be a handle from [`hvfs_attach`] and not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn hvfs_detach(dev: *mut hvfs_device) -> i32 {
    guard(|| {
        if dev.is_null() {
            return HVFS_OK;
        }
        // SAFETY: caller contract — `dev` came from hvfs_attach and is detached once.
        drop(unsafe { Box::from_raw(dev) });
        HVFS_OK
    })
}

/// Thread-local message for the most recent failing call on this thread, or NULL.
/// Borrowed: valid until the next ABI call on the same thread; never freed by the
/// caller.
#[no_mangle]
pub extern "C" fn hvfs_last_error() -> *const c_char {
    LAST_ERROR.with(|e| match &*e.borrow() {
        Some(c) => c.as_ptr(),
        None => ptr::null(),
    })
}

/// Logger callback: `(level, message, ctx)`. `level` follows syslog severities.
pub type hvfs_log_fn = Option<extern "C" fn(level: c_int, msg: *const c_char, ctx: *mut c_void)>;

/// Install a process-global logger. Set once at load, before [`hvfs_attach`].
///
/// # Safety
/// `cb` must remain a valid function pointer for the process lifetime; `ctx` is
/// passed back verbatim and must outlive any logging.
#[no_mangle]
pub unsafe extern "C" fn hvfs_set_logger(cb: hvfs_log_fn, ctx: *mut c_void) {
    // TODO: store cb/ctx in a global and route the device-host log stream to it.
    let _ = (cb, ctx);
}

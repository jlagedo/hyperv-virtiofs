//! `hyperv_virtiofs` — the public C ABI. This is the whole product surface: a
//! standalone, host-agnostic DLL ("virtiofsd for Hyper-V") that attaches an
//! OpenVMM virtio-fs device to an externally-owned HCS/Hyper-V guest over HDV.
//!
//! It contains **no** consumer concepts (no Atelier, no sessions, no broker). Its
//! vocabulary is purely virtio-fs/HDV: compute systems, device hosts, shares, tags,
//! read-only. Any host can drive it via the C ABI; Atelier binds it with Go's
//! `syscall.NewLazyDLL` (no cgo), exactly as it binds `computecore.dll`.
//!
//! ## ABI contract (mirrored in `include/hyperv_virtiofs.h`)
//! - Every function returns `i32`: `0` = OK, `< 0` = error; on error,
//!   [`hvfs_last_error`] holds a thread-local message.
//! - Handles are opaque (`hvfs_host*`, `hvfs_share*`), never passed by value.
//! - Every `*const c_char` is **borrowed**: the caller owns inputs; outputs are
//!   thread-local or delivered via the logger callback. Nothing crosses the
//!   allocator boundary, so there is no `free` footgun.
//! - **No panic ever crosses the boundary**: every entry point runs inside
//!   [`catch_unwind`]; a panic becomes [`HVFS_ERR_PANIC`].
//!
//! ## Object model (the real shape of the stack — `docs/share-abi.md`)
//! A [`hvfs_host`] is one HDV **device host** registered against an externally-owned
//! compute system (one per VM — the platform rejects a second). N [`hvfs_share`]s
//! ride it, each a **virtio-fs device == one shared directory**, hot-added to the
//! running guest. The ABI exposes this directly rather than imitating any one
//! consumer's share semantics:
//! - [`hvfs_host_open`] registers the device host (before the system is started);
//! - [`hvfs_add_share`] hot-adds one device per share (after start);
//! - [`hvfs_remove_share`] is best-effort: live `FlexibleIov` Remove is unsupported
//!   on current Windows, so it returns [`HVFS_ERR_UNSUPPORTED`] and the guest device
//!   is reclaimed when the compute system is torn down (`hvfs_host_close` + the
//!   caller stopping the VM) — the same reclaim-at-recycle WSL relies on;
//! - [`hvfs_host_close`] tears down every device, the host, and the system handle.

// The public type names mirror the C ABI (snake_case) on purpose.
#![allow(non_camel_case_types)]

mod logging;

use hcs_sys::{
    HcsCloseComputeSystem, HcsCloseOperation, HcsCreateOperation, HcsModifyComputeSystem,
    HcsOpenComputeSystem, HcsWaitForOperationResult, GENERIC_ALL, HCS_SYSTEM,
};
use hdv::pci::{guid_from_string, guid_to_string, HVFS_DEVICE_HOST_ID, VIRTIO_FS_DEVICE_CLASS_ID};
use hdv::proxy::DeviceHostSupport;
use hdv::DeviceHost;
use serde::Deserialize;
use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex};
use virtio_hdv::VirtioHdvDevice;

/// Bump on any breaking change to the C ABI. The consumer checks this at load.
/// v2: the device-bundled `hvfs_attach`/`hvfs_set_shares`/`hvfs_detach` surface was
/// replaced by the host/share model (`hvfs_host_open` + `hvfs_add_share` + …).
pub const HVFS_ABI_VERSION: u32 = 2;

// Status codes. Keep in sync with the header.
pub const HVFS_OK: i32 = 0;
pub const HVFS_ERR_INVALID_ARG: i32 = -1;
pub const HVFS_ERR_NOT_IMPLEMENTED: i32 = -2;
pub const HVFS_ERR_PANIC: i32 = -3;
pub const HVFS_ERR_HDV: i32 = -4;
/// The platform refused the operation (e.g. live `FlexibleIov` Remove). Distinct from
/// [`HVFS_ERR_NOT_IMPLEMENTED`] (which means *this DLL* hasn't built it): the request
/// is well-formed and the DLL tried, but Windows said no.
pub const HVFS_ERR_UNSUPPORTED: i32 = -5;

/// `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)` — what `HcsModifyComputeSystem` Remove
/// returns for a `FlexibleIov` device on current Windows.
const HRESULT_NOT_SUPPORTED: i32 = 0x8007_0032u32 as i32;

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(msg: impl Into<Vec<u8>>) {
    let c = CString::new(msg).unwrap_or_else(|_| CString::new("error").unwrap());
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(c));
}

/// Record a failure on both diagnostic channels at once and return its code: the
/// thread-local [`hvfs_last_error`] string *and* a `tracing` error event (which
/// reaches the [`hvfs_set_logger`] callback). Use this for genuine failures so the
/// two channels never drift apart.
fn fail(code: i32, msg: impl Into<String>) -> i32 {
    let msg = msg.into();
    tracing::error!(code, "{msg}");
    set_last_error(msg);
    code
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

/// The `host_json` contract for [`hvfs_host_open`].
#[derive(Deserialize)]
struct HostConfig {
    /// Guest RAM in MiB — the GPA ceiling each device's virtqueues may reference.
    /// Supplied by the caller because `HcsGetComputeSystemProperties` isn't bound; it
    /// must match the compute system's actual memory.
    memory_mb: u64,
}

/// The `share_json` contract for [`hvfs_add_share`].
#[derive(Deserialize)]
struct ShareConfig {
    /// virtio-fs mount tag the guest uses: `mount -t virtiofs <tag> …`.
    tag: String,
    /// Host directory to share.
    path: String,
    /// The device's unique `DeviceInstanceId` (canonical `8-4-4-4-12` GUID). Caller
    /// owns uniqueness across the compute system; the device *class* is fixed (the
    /// well-known virtio-fs id). A collision is rejected by the VID.
    instance_id: String,
    /// Read-only request. `true` currently returns [`HVFS_ERR_NOT_IMPLEMENTED`] — the
    /// FUSE backend does not yet enforce read-only, and the ABI won't claim a
    /// guarantee it can't keep.
    #[serde(default)]
    ro: bool,
}

/// Build an HCS `ModifySettingRequest` that adds (`Add`) or removes (`Remove`) a
/// `FlexibleIov` device slot keyed by `instance_id`, with the virtio-fs `EmulatorId`.
/// WSL's `DeviceHostProxy` sends the same `Settings` for both Add and Remove.
fn slot_request(request_type: &str, instance_id: &str, emulator_id: &str) -> String {
    serde_json::json!({
        "ResourcePath": format!("VirtualMachine/Devices/FlexibleIov/{instance_id}"),
        "RequestType": request_type,
        "Settings": { "EmulatorId": emulator_id, "HostingModel": "ExternalRestricted" },
    })
    .to_string()
}

/// Drive one `HcsModifyComputeSystem` to completion. Returns the failing `HRESULT`
/// (sync return *or* async operation result) so callers can distinguish a platform
/// refusal (`HRESULT_NOT_SUPPORTED`) from a real error.
///
/// # Safety
/// `system` must be a live compute-system handle for the duration of the call.
unsafe fn hcs_modify(system: HCS_SYSTEM, request: &str) -> Result<(), i32> {
    // SAFETY: `system` is live (caller contract); `op` is created/closed here; the
    // request wide-string outlives the synchronous `HcsModifyComputeSystem` call.
    unsafe {
        let op = HcsCreateOperation(ptr::null(), None);
        if op.is_null() {
            return Err(0); // 0 is not a real failing HRESULT — signals op-create failure
        }
        let hr = HcsModifyComputeSystem(system, op, wide(request).as_ptr(), ptr::null_mut());
        let mut result: hcs_sys::PWSTR = ptr::null_mut();
        let whr = HcsWaitForOperationResult(op, 30_000, &mut result);
        HcsCloseOperation(op);
        if hr < 0 {
            return Err(hr);
        }
        if whr < 0 {
            return Err(whr);
        }
        Ok(())
    }
}

/// RAII for an opened `HCS_SYSTEM`: closes it on drop, so every early return in
/// [`hvfs_host_open`] releases the handle and the host's teardown closes it last.
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

/// Opaque **device-host** handle: one per compute system, owning the single HDV
/// device host the platform permits and the shares opened on it. Box-allocated;
/// freed by [`hvfs_host_close`].
pub struct hvfs_host {
    // Field order = drop order, and it is load-bearing: shares (their HDV devices)
    // tear down before the device host, which tears down before the COM support
    // object it was registered through, which releases before the HCS handle closes.
    shares: Mutex<Vec<*mut hvfs_share>>,
    host: Arc<DeviceHost>,
    /// Held only to keep the `IVmDeviceHostSupport` COM object alive for the device
    /// host's lifetime (released, never called back into, on drop).
    _support: DeviceHostSupport,
    owned: OwnedSystem,
    /// Per-device aperture ceiling (the compute system's RAM in bytes).
    memory_bytes: u64,
}

impl hvfs_host {
    fn system(&self) -> HCS_SYSTEM {
        self.owned.0
    }
}

/// Opaque **share** handle: one virtio-fs device == one shared directory, hot-added
/// to the running guest. Borrows its [`hvfs_host`] (the host must outlive it). Freed
/// by [`hvfs_remove_share`], or reclaimed by [`hvfs_host_close`].
pub struct hvfs_share {
    /// The HDV virtio-fs device; holds a clone of the host's `Arc<DeviceHost>`, so the
    /// device host stays alive at least as long as this share.
    _device: VirtioHdvDevice,
    /// The host's compute-system handle, borrowed for this share's own Remove. Valid
    /// while the host is open (the contract).
    system: HCS_SYSTEM,
    /// Back-reference to the owning host, to de-register on remove. Never dereferenced
    /// during the share's own `Drop` (only by `hvfs_remove_share`, while the host is
    /// guaranteed open).
    host: *const hvfs_host,
    /// Canonical `DeviceInstanceId` and `EmulatorId` GUID strings, for the getter and
    /// the Remove request.
    instance_id: CString,
    emulator_id: CString,
}

// SAFETY: both handles are returned to C as opaque pointers and may be used from a
// different thread than the one that created them. Everything they own is a
// process-global, thread-agnostic handle — the HDV device/host, the `HCS_SYSTEM`, and
// the `IVmDeviceHostSupport` COM object — none tied to a thread; the share registry is
// behind a `Mutex`. So moving the boxes across threads is sound.
unsafe impl Send for hvfs_host {}
unsafe impl Send for hvfs_share {}

/// Returns the ABI version the DLL implements. Call right after load; refuse on
/// mismatch with [`HVFS_ABI_VERSION`] in the header you compiled against.
#[no_mangle]
pub extern "C" fn hvfs_abi_version() -> u32 {
    HVFS_ABI_VERSION
}

/// Register an HDV device host against an externally-owned HCS compute system, by id.
/// Exactly one device host is permitted per compute system; every share rides it.
/// On success `*out` holds a [`hvfs_host`] handle for [`hvfs_add_share`] /
/// [`hvfs_host_close`].
///
/// **Call before the compute system is started** — the device host is registered at
/// VM bringup (as WSL does). `host_json` is `{ "memory_mb": 512 }`, the system's RAM
/// (the GPA ceiling each device's virtqueues may reference; it must equal the system's
/// actual memory). The caller's create document must declare **no** `FlexibleIov`
/// slots — shares are hot-added at runtime by [`hvfs_add_share`].
///
/// # Safety
/// `hcs_system_id` and `host_json` must be valid NUL-terminated C strings; `out` must
/// be a valid, writable `*mut *mut hvfs_host`.
#[no_mangle]
pub unsafe extern "C" fn hvfs_host_open(
    hcs_system_id: *const c_char,
    host_json: *const c_char,
    out: *mut *mut hvfs_host,
) -> i32 {
    guard(|| {
        logging::init();
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
        let host_json = match cstr(host_json, "host_json") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let cfg: HostConfig = match serde_json::from_str(host_json) {
            Ok(c) => c,
            Err(e) => {
                set_last_error(format!("host_json parse error: {e}"));
                return HVFS_ERR_INVALID_ARG;
            }
        };

        // Open the caller's system, wrapped at once so any early return closes it.
        let mut system: HCS_SYSTEM = ptr::null_mut();
        let idw = wide(id);
        // SAFETY: `idw` is a valid NUL-terminated wide string; `system` is a valid out ptr.
        let hr = unsafe { HcsOpenComputeSystem(idw.as_ptr(), GENERIC_ALL, &mut system) };
        if hr < 0 {
            return fail(
                HVFS_ERR_HDV,
                format!("HcsOpenComputeSystem failed: HRESULT {:#010x}", hr as u32),
            );
        }
        let owned = OwnedSystem(system);

        // Register the single HDV device host over the ExternalRestricted proxy path.
        // SAFETY: `owned` (hence the system handle) outlives `support` and the host.
        let support = unsafe { DeviceHostSupport::new(owned.0) };
        // SAFETY: HVFS_DEVICE_HOST_ID is a live GUID constant; `support` is a live
        // IVmDeviceHostSupport that outlives the returned device host.
        let host =
            match unsafe { DeviceHost::from_proxy(&HVFS_DEVICE_HOST_ID, support.as_iunknown()) } {
                Ok(h) => h,
                Err(e) => {
                    return fail(
                        HVFS_ERR_HDV,
                        format!("HdvInitializeDeviceHostForProxy failed: {e}"),
                    );
                }
            };

        let h = Box::new(hvfs_host {
            shares: Mutex::new(Vec::new()),
            host: Arc::new(host),
            _support: support,
            owned,
            memory_bytes: cfg.memory_mb.saturating_mul(1024 * 1024),
        });
        tracing::info!(system = %id, memory_mb = cfg.memory_mb, "device host registered");
        // SAFETY: `out` checked non-null above and guaranteed writable by the caller.
        unsafe { *out = Box::into_raw(h) };
        HVFS_OK
    })
}

/// Hot-add one virtio-fs device (== one shared directory) to the **running** compute
/// system. On success `*out` holds a [`hvfs_share`] handle.
///
/// `share_json` is `{ "tag": "ws", "path": "C:\\host\\dir", "instance_id": "<guid>",
/// "ro": false }` — `tag` is the virtio-fs mount tag, `path` the host directory,
/// `instance_id` the device's **required** unique `DeviceInstanceId` (the caller owns
/// uniqueness; the device class is the well-known virtio-fs id, not caller-chosen).
/// `ro: true` currently returns [`HVFS_ERR_NOT_IMPLEMENTED`] (read-only is not yet
/// enforced).
///
/// # Safety
/// `host` must be a live handle from [`hvfs_host_open`]; `share_json` a valid
/// NUL-terminated C string; `out` a valid, writable `*mut *mut hvfs_share`.
#[no_mangle]
pub unsafe extern "C" fn hvfs_add_share(
    host: *mut hvfs_host,
    share_json: *const c_char,
    out: *mut *mut hvfs_share,
) -> i32 {
    guard(|| {
        logging::init();
        if out.is_null() {
            set_last_error("null out parameter");
            return HVFS_ERR_INVALID_ARG;
        }
        // SAFETY: checked non-null above; caller guarantees writability.
        unsafe { *out = ptr::null_mut() };
        if host.is_null() {
            set_last_error("null host handle");
            return HVFS_ERR_INVALID_ARG;
        }
        // SAFETY: caller contract — `host` came from hvfs_host_open and is still open.
        let h = unsafe { &*host };

        let share_json = match cstr(share_json, "share_json") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let cfg: ShareConfig = match serde_json::from_str(share_json) {
            Ok(c) => c,
            Err(e) => {
                set_last_error(format!("share_json parse error: {e}"));
                return HVFS_ERR_INVALID_ARG;
            }
        };
        if cfg.ro {
            set_last_error(
                "ro:true not supported — the FUSE backend does not yet enforce read-only",
            );
            return HVFS_ERR_NOT_IMPLEMENTED;
        }
        // Parse the caller-supplied instance id and canonicalize it, so the device,
        // the FlexibleIov slot key, and the stored handle all agree byte-for-byte.
        let instance_guid = match guid_from_string(&cfg.instance_id) {
            Some(g) => g,
            None => {
                set_last_error(format!(
                    "instance_id is not a valid GUID: {}",
                    cfg.instance_id
                ));
                return HVFS_ERR_INVALID_ARG;
            }
        };
        let instance_str = guid_to_string(&instance_guid);
        let emulator_str = guid_to_string(&VIRTIO_FS_DEVICE_CLASS_ID);

        // (1) Create the HDV virtio-fs device on the shared host. The well-known class
        // id is what lets multiple virtio-fs devices coexist (the hotplug spike).
        let device = match VirtioHdvDevice::attach_shared(
            h.host.clone(),
            Path::new(&cfg.path),
            &cfg.tag,
            h.memory_bytes,
            &VIRTIO_FS_DEVICE_CLASS_ID,
            &instance_guid,
        ) {
            Ok(d) => d,
            Err(e) => {
                return fail(HVFS_ERR_HDV, format!("attach virtio-fs over HDV: {e}"));
            }
        };

        // (2) Hot-add the FlexibleIov slot; the VID resolves it to the device above.
        let req = slot_request("Add", &instance_str, &emulator_str);
        // SAFETY: the host (hence its system handle) is live for this call.
        if let Err(hr) = unsafe { hcs_modify(h.system(), &req) } {
            // The Add failed; drop the device we just created (host-side cleanup).
            drop(device);
            return fail(
                HVFS_ERR_HDV,
                format!(
                    "HcsModifyComputeSystem Add FlexibleIov: HRESULT {:#010x}",
                    hr as u32
                ),
            );
        }

        tracing::info!(tag = %cfg.tag, instance_id = %instance_str, "share added");
        let share = Box::new(hvfs_share {
            _device: device,
            system: h.system(),
            host: host as *const hvfs_host,
            instance_id: CString::new(instance_str).unwrap_or_default(),
            emulator_id: CString::new(emulator_str).unwrap_or_default(),
        });
        let raw = Box::into_raw(share);
        // Register so hvfs_host_close can reclaim a share the caller didn't remove.
        if let Ok(mut v) = h.shares.lock() {
            v.push(raw);
        }
        // SAFETY: `out` checked non-null above and guaranteed writable by the caller.
        unsafe { *out = raw };
        HVFS_OK
    })
}

/// The share's on-wire identity: its `FlexibleIov` `DeviceInstanceId` (canonical GUID
/// string). Borrowed, valid for the share's lifetime, never freed by the caller.
/// Returns NULL for a null handle.
///
/// # Safety
/// `share` must be NULL or a live handle from [`hvfs_add_share`].
#[no_mangle]
pub unsafe extern "C" fn hvfs_share_instance_id(share: *const hvfs_share) -> *const c_char {
    if share.is_null() {
        return ptr::null();
    }
    // SAFETY: caller contract — `share` is a live handle; the CString lives as long.
    unsafe { (*share).instance_id.as_ptr() }
}

/// Best-effort live remove + host-side teardown of one share. Issues a `FlexibleIov`
/// Remove; on current Windows the platform refuses it and this returns
/// [`HVFS_ERR_UNSUPPORTED`] — the guest-visible device then persists until the compute
/// system is torn down (reclaim-at-recycle), though host-side resources are released
/// now. The share handle is **freed** on [`HVFS_OK`] and [`HVFS_ERR_UNSUPPORTED`]; on
/// any other error the handle stays valid (retryable, and reclaimed by
/// [`hvfs_host_close`]). A null handle is a no-op ([`HVFS_OK`]).
///
/// # Safety
/// `share` must be NULL or a live handle from [`hvfs_add_share`], whose host is still
/// open, and must not be used again after a freeing return.
#[no_mangle]
pub unsafe extern "C" fn hvfs_remove_share(share: *mut hvfs_share) -> i32 {
    guard(|| {
        logging::init();
        if share.is_null() {
            return HVFS_OK;
        }
        // SAFETY: caller contract — live handle, host still open.
        let s = unsafe { &*share };
        let instance = s.instance_id.to_string_lossy();
        let req = slot_request("Remove", &instance, &s.emulator_id.to_string_lossy());
        // SAFETY: the share borrows a system handle live while its host is open.
        let outcome = match unsafe { hcs_modify(s.system, &req) } {
            Ok(()) => {
                tracing::info!(instance_id = %instance, "share removed");
                HVFS_OK
            }
            Err(hr) if hr == HRESULT_NOT_SUPPORTED => {
                let msg = "FlexibleIov live Remove unsupported on this Windows; \
                     device reclaimed when the compute system is torn down";
                tracing::warn!(instance_id = %instance, "{msg}");
                set_last_error(msg);
                HVFS_ERR_UNSUPPORTED
            }
            Err(hr) => fail(
                HVFS_ERR_HDV,
                format!(
                    "HcsModifyComputeSystem Remove FlexibleIov: HRESULT {:#010x}",
                    hr as u32
                ),
            ),
        };

        // Only free on success/unsupported; a genuine error leaves the handle valid.
        if outcome == HVFS_OK || outcome == HVFS_ERR_UNSUPPORTED {
            // De-register from the host (it outlives the share per contract).
            if !s.host.is_null() {
                // SAFETY: `s.host` points at a live hvfs_host (must outlive its shares).
                if let Ok(mut v) = unsafe { (*s.host).shares.lock() } {
                    v.retain(|&p| p != share);
                }
            }
            // SAFETY: caller contract — `share` came from hvfs_add_share, freed once.
            drop(unsafe { Box::from_raw(share) });
        }
        outcome
    })
}

/// Tear down every remaining share's device, then the device host, then close the
/// compute-system handle. Invalidates all share handles opened on this host. A null
/// handle is a no-op ([`HVFS_OK`]). Per-share live removal is not attempted here —
/// the devices are reclaimed when the caller subsequently stops/terminates the VM.
///
/// # Safety
/// `host` must be NULL or a live handle from [`hvfs_host_open`], not used afterwards;
/// no share handle from it may be used after this call.
#[no_mangle]
pub unsafe extern "C" fn hvfs_host_close(host: *mut hvfs_host) -> i32 {
    guard(|| {
        logging::init();
        if host.is_null() {
            return HVFS_OK;
        }
        // SAFETY: caller contract — `host` came from hvfs_host_open and is closed once.
        let h = unsafe { Box::from_raw(host) };
        // Free any shares the caller didn't remove (drops their HDV device handles).
        if let Ok(mut v) = h.shares.lock() {
            tracing::info!(reclaimed_shares = v.len(), "closing device host");
            for p in v.drain(..) {
                if !p.is_null() {
                    // SAFETY: each `p` is a live hvfs_share from hvfs_add_share, freed once.
                    drop(unsafe { Box::from_raw(p) });
                }
            }
        }
        // Dropping `h` tears down the device host (last Arc ref), releases the COM
        // support object, and closes the HCS handle — in that field order.
        drop(h);
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

/// Install a process-global logger. Call once at load, ideally before
/// [`hvfs_host_open`]. `cb` receives `(level, message, ctx)`: `level` is a syslog
/// severity (3=err, 4=warning, 6=info, 7=debug/trace); `message` is a borrowed,
/// NUL-terminated string valid only for the duration of the call; `ctx` is returned
/// verbatim. The logger receives both this DLL's own events and the underlying
/// OpenVMM device-host stream. `cb = NULL` disables delivery.
///
/// Best-effort: the DLL installs a process-global log subscriber only if the host
/// process hasn't already installed one. If it has, the host owns routing and the
/// callback may receive nothing. Verbosity follows `RUST_LOG` (default: info).
///
/// # Safety
/// `cb` must remain a valid function pointer for the process lifetime; `ctx` is
/// passed back verbatim and must outlive any logging.
#[no_mangle]
pub unsafe extern "C" fn hvfs_set_logger(cb: hvfs_log_fn, ctx: *mut c_void) {
    // Store the sink, then ensure the process-global subscriber is installed so
    // both our events and the reused OpenVMM crates' events reach the callback.
    // Wrapped in catch_unwind to honor the no-panic-across-the-boundary contract.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        logging::set_sink(cb, ctx);
        logging::init();
    }));
}

#[cfg(test)]
mod tests {
    //! Offline unit tests for the C ABI's deterministic surface — argument
    //! validation, JSON contracts, request shaping, and the panic guard. None of
    //! these touch HCS/HDV, so they run on any host (including CI). The live
    //! device paths are proven by the gated e2e ladder (`docs/testing.md`).
    use super::*;

    // ---- version + status codes -------------------------------------------------

    #[test]
    fn abi_version_is_two() {
        assert_eq!(hvfs_abi_version(), 2);
        assert_eq!(hvfs_abi_version(), HVFS_ABI_VERSION);
    }

    #[test]
    fn status_codes_are_distinct_and_signed() {
        let codes = [
            HVFS_OK,
            HVFS_ERR_INVALID_ARG,
            HVFS_ERR_NOT_IMPLEMENTED,
            HVFS_ERR_PANIC,
            HVFS_ERR_HDV,
            HVFS_ERR_UNSUPPORTED,
        ];
        // All errors are negative; OK is the only zero; none collide.
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "status codes must be distinct");
            }
        }
        assert_eq!(HVFS_OK, 0);
        assert!(codes[1..].iter().all(|c| *c < 0), "errors must be < 0");
    }

    // ---- guard(): no panic crosses the boundary ---------------------------------

    #[test]
    fn guard_passes_through_normal_return() {
        assert_eq!(guard(|| HVFS_OK), HVFS_OK);
        assert_eq!(guard(|| HVFS_ERR_HDV), HVFS_ERR_HDV);
    }

    #[test]
    fn guard_converts_panic_to_panic_code() {
        // Silence the default panic backtrace for this one deliberate panic.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let code = guard(|| panic!("boom across FFI"));
        std::panic::set_hook(prev);
        assert_eq!(code, HVFS_ERR_PANIC);
        // The guard records a diagnostic for hvfs_last_error.
        let msg = LAST_ERROR.with(|e| e.borrow().clone());
        assert!(msg.is_some(), "panic should set a thread-local last_error");
    }

    // ---- cstr(): borrow + validate C string args --------------------------------

    #[test]
    fn cstr_rejects_null() {
        assert_eq!(cstr(ptr::null(), "x"), Err(HVFS_ERR_INVALID_ARG));
    }

    #[test]
    fn cstr_accepts_valid_utf8() {
        let c = CString::new("hello").unwrap();
        assert_eq!(cstr(c.as_ptr(), "x"), Ok("hello"));
    }

    #[test]
    fn cstr_rejects_non_utf8() {
        // 0xFF is never valid UTF-8; CString allows it (no interior NUL).
        let c = CString::new(vec![0xFFu8, 0xFE]).unwrap();
        assert_eq!(cstr(c.as_ptr(), "x"), Err(HVFS_ERR_INVALID_ARG));
    }

    // ---- host_json / share_json contracts ---------------------------------------

    #[test]
    fn host_config_parses_memory_mb() {
        let c: HostConfig = serde_json::from_str(r#"{"memory_mb": 512}"#).unwrap();
        assert_eq!(c.memory_mb, 512);
    }

    #[test]
    fn host_config_requires_memory_mb() {
        assert!(serde_json::from_str::<HostConfig>(r#"{}"#).is_err());
        assert!(serde_json::from_str::<HostConfig>(r#"{"memory_mb": "no"}"#).is_err());
        assert!(serde_json::from_str::<HostConfig>(r#"not json"#).is_err());
    }

    #[test]
    fn share_config_full_and_ro_default() {
        let c: ShareConfig = serde_json::from_str(
            r#"{"tag":"ws","path":"C:\\h","instance_id":"c1c1c1c1-3333-4333-8333-333333333333"}"#,
        )
        .unwrap();
        assert_eq!(c.tag, "ws");
        assert_eq!(c.path, "C:\\h");
        assert_eq!(c.instance_id, "c1c1c1c1-3333-4333-8333-333333333333");
        assert!(!c.ro, "ro defaults to false when omitted");

        let ro: ShareConfig =
            serde_json::from_str(r#"{"tag":"t","path":"p","instance_id":"i","ro":true}"#).unwrap();
        assert!(ro.ro);
    }

    #[test]
    fn share_config_requires_core_fields() {
        // Missing tag / path / instance_id are all errors.
        assert!(serde_json::from_str::<ShareConfig>(r#"{"path":"p","instance_id":"i"}"#).is_err());
        assert!(serde_json::from_str::<ShareConfig>(r#"{"tag":"t","instance_id":"i"}"#).is_err());
        assert!(serde_json::from_str::<ShareConfig>(r#"{"tag":"t","path":"p"}"#).is_err());
    }

    // ---- slot_request(): the HcsModifyComputeSystem document --------------------

    #[test]
    fn slot_request_shapes_add_and_remove() {
        for rt in ["Add", "Remove"] {
            let doc = slot_request(rt, "11112222-3333-4444-5555-666677778888", "emul-id");
            let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
            assert_eq!(v["RequestType"], rt);
            assert_eq!(
                v["ResourcePath"],
                "VirtualMachine/Devices/FlexibleIov/11112222-3333-4444-5555-666677778888"
            );
            assert_eq!(v["Settings"]["EmulatorId"], "emul-id");
            assert_eq!(v["Settings"]["HostingModel"], "ExternalRestricted");
        }
    }

    // ---- wide(): UTF-16 NUL-terminated encoding ---------------------------------

    #[test]
    fn wide_is_nul_terminated_utf16() {
        assert_eq!(wide("AB"), vec![0x41u16, 0x42, 0x00]);
        assert_eq!(wide(""), vec![0x00u16]);
    }

    // ---- null-argument contracts on the exported entry points -------------------
    // These return before any HCS/HDV call, so they're safe to run anywhere.

    #[test]
    fn host_open_rejects_null_out() {
        // SAFETY: passing a null out pointer is exactly the contract under test.
        let rc = unsafe { hvfs_host_open(ptr::null(), ptr::null(), ptr::null_mut()) };
        assert_eq!(rc, HVFS_ERR_INVALID_ARG);
    }

    #[test]
    fn host_open_rejects_null_id_and_clears_out() {
        // A non-null garbage value the call must overwrite with NULL before returning.
        let mut out: *mut hvfs_host = ptr::NonNull::dangling().as_ptr();
        // SAFETY: valid writable out slot; null id is the contract under test.
        let rc = unsafe { hvfs_host_open(ptr::null(), ptr::null(), &mut out) };
        assert_eq!(rc, HVFS_ERR_INVALID_ARG);
        assert!(out.is_null(), "out must be nulled before any early return");
    }

    #[test]
    fn host_open_rejects_bad_json_before_touching_platform() {
        let id = CString::new("nonexistent-system").unwrap();
        let bad = CString::new("{ not valid json").unwrap();
        let mut out: *mut hvfs_host = ptr::null_mut();
        // SAFETY: valid C strings + out slot; the parse failure returns before HCS.
        let rc = unsafe { hvfs_host_open(id.as_ptr(), bad.as_ptr(), &mut out) };
        assert_eq!(rc, HVFS_ERR_INVALID_ARG);
        assert!(out.is_null());
    }

    #[test]
    fn add_share_rejects_null_out_and_host() {
        // SAFETY: null out is the contract under test.
        let rc = unsafe { hvfs_add_share(ptr::null_mut(), ptr::null(), ptr::null_mut()) };
        assert_eq!(rc, HVFS_ERR_INVALID_ARG);

        let mut out: *mut hvfs_share = ptr::null_mut();
        // SAFETY: valid out slot; null host is the contract under test.
        let rc = unsafe { hvfs_add_share(ptr::null_mut(), ptr::null(), &mut out) };
        assert_eq!(rc, HVFS_ERR_INVALID_ARG);
        assert!(out.is_null());
    }

    #[test]
    fn instance_id_of_null_share_is_null() {
        // SAFETY: null is an explicitly allowed input.
        assert!(unsafe { hvfs_share_instance_id(ptr::null()) }.is_null());
    }

    #[test]
    fn remove_and_close_null_handles_are_ok() {
        // SAFETY: null handles are explicit no-ops per the contract.
        assert_eq!(unsafe { hvfs_remove_share(ptr::null_mut()) }, HVFS_OK);
        assert_eq!(unsafe { hvfs_host_close(ptr::null_mut()) }, HVFS_OK);
    }

    // ---- hvfs_set_logger: events reach the callback, null is safe --------------
    // The callback records into the `AtomicUsize` handed to it via `ctx`, which
    // also exercises the opaque-context plumbing. Note tracing's global subscriber
    // is process-wide and set-once: other tests calling `init()` is fine (the
    // sink is read live), and this is the only test that mutates the sink.
    extern "C" fn counting_cb(_level: c_int, msg: *const c_char, ctx: *mut c_void) {
        if msg.is_null() || ctx.is_null() {
            return;
        }
        // SAFETY: `ctx` is the `&AtomicUsize` passed below, valid for the call.
        let counter = unsafe { &*(ctx as *const std::sync::atomic::AtomicUsize) };
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn set_logger_delivers_events_and_none_is_safe() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = AtomicUsize::new(0);
        // SAFETY: `&hits` outlives the window in which the callback is installed
        // (we clear the sink before `hits` is dropped at end of scope).
        unsafe { hvfs_set_logger(Some(counting_cb), &hits as *const _ as *mut c_void) };
        tracing::error!(marker = "unit-test", "delivery probe");
        assert!(
            hits.load(Ordering::SeqCst) >= 1,
            "the installed callback should have received the event"
        );
        // A null callback disables delivery and must not crash.
        // SAFETY: null callback is an explicitly allowed input.
        unsafe { hvfs_set_logger(None, ptr::null_mut()) };
        tracing::error!("after-none must not reach a callback");
    }

    #[test]
    fn last_error_is_null_on_a_fresh_thread() {
        // LAST_ERROR is thread-local; a brand-new thread has never set it.
        let p = std::thread::spawn(|| hvfs_last_error() as usize)
            .join()
            .unwrap();
        assert_eq!(p, 0, "a fresh thread's last_error must be NULL");
    }

    #[test]
    fn last_error_roundtrips_a_message() {
        set_last_error("a diagnostic");
        let p = hvfs_last_error();
        assert!(!p.is_null());
        // SAFETY: `p` is the thread-local CString we just set, valid until the next call.
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "a diagnostic");
    }
}

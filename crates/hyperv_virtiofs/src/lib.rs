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
//! Status: SKELETON. The ABI is real and stable; `attach`/`set_shares` return
//! [`HVFS_ERR_NOT_IMPLEMENTED`] until the HDV handshake (design §7) is wired.

// The public type names mirror the C ABI (snake_case) on purpose.
#![allow(non_camel_case_types)]

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

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

/// Opaque device handle. Box-allocated; freed by [`hvfs_detach`].
pub struct hvfs_device {
    _device: virtio_hdv::VirtioHdvDevice,
}

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

        let _id = match cstr(hcs_system_id, "hcs_system_id") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let _device = match cstr(device_json, "device_json") {
            Ok(s) => s,
            Err(e) => return e,
        };

        // TODO(spike-1): DeviceHost::open(id) -> VirtioHdvDevice::new(host, 0x1a)
        // -> Box::into_raw into *out. Until the HDV handshake is proven:
        set_last_error("hvfs_attach: HDV handshake not yet implemented (design §7 unknown #1)");
        HVFS_ERR_NOT_IMPLEMENTED
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

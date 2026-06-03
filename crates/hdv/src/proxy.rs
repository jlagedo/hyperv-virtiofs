//! The host-side `IVmDeviceHostSupport` COM object for the FlexibleIov
//! `ExternalRestricted` **proxy registration** (the path our in-process attach
//! spikes were missing; see `docs/hdv-proxy-abi.md`).
//!
//! Flow (both halves in *one* process — no COM surrogate, unlike WSL):
//! 1. [`DeviceHostSupport::new`] builds this COM object holding the `HCS_SYSTEM`.
//! 2. [`DeviceHost::from_proxy`](crate::DeviceHost::from_proxy) calls
//!    `HdvInitializeDeviceHostForProxy(ctx, thisAsIUnknown, &host)`. Internally HDV
//!    `QueryInterface`s us for `IVmDeviceHostSupport`, builds the device host,
//!    wraps it as an `IVmDeviceHost`, and calls back our
//!    [`register_device_host`] — which forwards to `HdvProxyDeviceHost`, wiring the
//!    device host to the partition.
//! 3. The caller then `HdvCreateDeviceInstance`s the device and hot-adds the
//!    matching `FlexibleIov` slot (`HcsModifyComputeSystem`).
//!
//! We implement **only** `IVmDeviceHostSupport`; the `IVmDeviceHost` is produced by
//! HDV and handed to us as a parameter, so we never author it.

use hdv_sys as sys;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

use sys::{E_FAIL, E_NOINTERFACE, E_POINTER};

/// `IID_IUnknown` `{00000000-0000-0000-C000-000000000046}`.
const IID_IUNKNOWN: sys::GUID = sys::GUID {
    Data1: 0,
    Data2: 0,
    Data3: 0,
    Data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
};
/// `IID_IVmDeviceHostSupport` `{e31aa49b-0914-465e-b145-1b9ba13efb10}` (from WSL
/// `windowsdefs.idl`; confirmed as the IID `HdvInitializeDeviceHostForProxy`
/// `QueryInterface`s for, by disassembly).
const IID_IVMDEVICEHOSTSUPPORT: sys::GUID = sys::GUID {
    Data1: 0xe31a_a49b,
    Data2: 0x0914,
    Data3: 0x465e,
    Data4: [0xb1, 0x45, 0x1b, 0x9b, 0xa1, 0x3e, 0xfb, 0x10],
};

/// COM vtable for `IVmDeviceHostSupport` (IUnknown's 3 slots + `RegisterDeviceHost`).
#[repr(C)]
struct IVmDeviceHostSupportVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const sys::GUID, *mut *mut c_void) -> sys::HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    register_device_host:
        unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *mut u64) -> sys::HRESULT,
}

/// Heap object whose first field is the COM vtable pointer, so a `*mut Inner` is a
/// valid `IVmDeviceHostSupport*` / `IUnknown*`.
#[repr(C)]
struct Inner {
    vtable: *const IVmDeviceHostSupportVtbl,
    refcount: AtomicU32,
    /// The compute system to proxy the device host onto (passed to
    /// `HdvProxyDeviceHost`). A borrowed HCS handle; the caller keeps it alive.
    system: sys::HCS_SYSTEM,
    called: AtomicBool,
    register_hr: AtomicI32,
    ipc_section: AtomicU64,
    pid: AtomicU32,
}

static VTBL: IVmDeviceHostSupportVtbl = IVmDeviceHostSupportVtbl {
    query_interface: qi,
    add_ref,
    release,
    register_device_host,
};

unsafe extern "system" fn qi(
    this: *mut c_void,
    riid: *const sys::GUID,
    ppv: *mut *mut c_void,
) -> sys::HRESULT {
    catch_unwind(AssertUnwindSafe(|| {
        if ppv.is_null() {
            return E_POINTER;
        }
        if riid.is_null() {
            unsafe { *ppv = std::ptr::null_mut() };
            return E_POINTER;
        }
        let iid = unsafe { *riid };
        if iid == IID_IUNKNOWN || iid == IID_IVMDEVICEHOSTSUPPORT {
            unsafe { *ppv = this };
            unsafe { &*(this as *const Inner) }
                .refcount
                .fetch_add(1, Ordering::Relaxed);
            sys::S_OK
        } else {
            unsafe { *ppv = std::ptr::null_mut() };
            E_NOINTERFACE
        }
    }))
    .unwrap_or(E_FAIL)
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    unsafe { &*(this as *const Inner) }
        .refcount
        .fetch_add(1, Ordering::Relaxed)
        + 1
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    let prev = unsafe { &*(this as *const Inner) }
        .refcount
        .fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        // Last reference — reclaim the box.
        drop(unsafe { Box::from_raw(this as *mut Inner) });
        0
    } else {
        prev - 1
    }
}

/// The device host (built inside `HdvInitializeDeviceHostForProxy`) calls this to
/// register itself; we forward the handed `IVmDeviceHost` to `HdvProxyDeviceHost`,
/// binding it to our compute system.
unsafe extern "system" fn register_device_host(
    this: *mut c_void,
    device_host: *mut c_void,
    pid: u32,
    ipc_section_handle: *mut u64,
) -> sys::HRESULT {
    catch_unwind(AssertUnwindSafe(|| {
        let inner = unsafe { &*(this as *const Inner) };
        inner.called.store(true, Ordering::Relaxed);
        inner.pid.store(pid, Ordering::Relaxed);
        // SAFETY: `device_host` is the IVmDeviceHost (as IUnknown) HDV handed us;
        // `ipc_section_handle` is HDV's out ptr.
        let code =
            unsafe { sys::HdvProxyDeviceHost(inner.system, device_host, pid, ipc_section_handle) };
        inner.register_hr.store(code, Ordering::Relaxed);
        if code >= 0 && !ipc_section_handle.is_null() {
            inner
                .ipc_section
                .store(unsafe { *ipc_section_handle }, Ordering::Relaxed);
        }
        code
    }))
    .unwrap_or(E_FAIL)
}

/// A live `IVmDeviceHostSupport` COM object. Hand [`as_iunknown`](Self::as_iunknown)
/// to [`DeviceHost::from_proxy`](crate::DeviceHost::from_proxy); keep this alive
/// until **after** the device host is torn down (HDV holds a reference, but ours is
/// the backstop). Dropping releases our reference.
pub struct DeviceHostSupport {
    inner: *mut Inner,
}

impl DeviceHostSupport {
    /// Create the support object bound to `system`. Starts at refcount 1 (our ref).
    ///
    /// # Safety
    /// `system` must outlive this object and every device host registered through it.
    pub unsafe fn new(system: sys::HCS_SYSTEM) -> Self {
        let inner = Box::into_raw(Box::new(Inner {
            vtable: &VTBL as *const IVmDeviceHostSupportVtbl,
            refcount: AtomicU32::new(1),
            system,
            called: AtomicBool::new(false),
            register_hr: AtomicI32::new(0),
            ipc_section: AtomicU64::new(0),
            pid: AtomicU32::new(0),
        }));
        Self { inner }
    }

    /// This object as an `IUnknown*` to pass to `HdvInitializeDeviceHostForProxy`.
    pub fn as_iunknown(&self) -> sys::PVOID {
        self.inner as sys::PVOID
    }

    /// Whether HDV has invoked `RegisterDeviceHost` yet.
    pub fn was_registered(&self) -> bool {
        unsafe { &*self.inner }.called.load(Ordering::Relaxed)
    }

    /// The `HRESULT` our last `HdvProxyDeviceHost` returned (only meaningful once
    /// [`was_registered`](Self::was_registered)).
    pub fn register_hr(&self) -> sys::HRESULT {
        unsafe { &*self.inner }.register_hr.load(Ordering::Relaxed)
    }

    /// The IPC section handle `HdvProxyDeviceHost` produced (0 if none/failed).
    pub fn ipc_section(&self) -> u64 {
        unsafe { &*self.inner }.ipc_section.load(Ordering::Relaxed)
    }

    /// The device-host process id HDV reported (our own PID for an in-process host).
    pub fn device_host_pid(&self) -> u32 {
        unsafe { &*self.inner }.pid.load(Ordering::Relaxed)
    }
}

impl Drop for DeviceHostSupport {
    fn drop(&mut self) {
        // Release our reference (frees the box iff HDV has dropped its refs too).
        unsafe { ((*(*self.inner).vtable).release)(self.inner as *mut c_void) };
    }
}

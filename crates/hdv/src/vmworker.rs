//! Owner-side doorbell registration through the **VM worker process** — the
//! route that works where [`Device::register_doorbell`](crate::Device::register_doorbell)
//! is denied (`E_ACCESSDENIED` on an `ExternalRestricted` device host, by design).
//!
//! A process that owns the compute system resolves the FlexibleIov device
//! *inside the worker* and registers the doorbell on it:
//!
//! ```text
//! GetVmWorkerProcess(runtime_id, IVmVirtualDeviceAccess)        vmwpctrl.dll
//!   → GetDevice(FLEXIO_DEVICE_ID, instance_id)
//!     → QueryInterface(IVmFiovGuestMemoryFastNotification)
//!       → RegisterDoorbell / UnregisterDoorbell
//! ```
//!
//! Same flow as WSL's `DeviceHostProxy` (the reference implementation), minus
//! the cross-process broker — our caller is both VM owner and device host.
//!
//! [`FiovDoorbells::connect`] requires the device's FlexibleIov slot to exist
//! (i.e. **after** the `HcsModifyComputeSystem` Add), which is always true at
//! doorbell-registration time: the guest can only set DRIVER_OK on a device it
//! can see. Keep the connection for the device's lifetime — once the device is
//! being removed, the worker no longer resolves it, so unregistration must go
//! through the stored interface (the WSL-documented caveat).

use crate::{Error, Result};
use hdv_sys as sys;
use std::ffi::c_void;
use std::ptr;
use vmworker_sys as wp;

/// `HRESULT_FROM_WIN32(ERROR_MOD_NOT_FOUND)` — `vmwpctrl.dll` or its
/// `GetVmWorkerProcess` export is absent on this host.
const HR_MOD_NOT_FOUND: sys::HRESULT = 0x8007_007Eu32 as sys::HRESULT;

fn hr(code: sys::HRESULT) -> Result<()> {
    if code >= 0 {
        Ok(())
    } else {
        Err(Error(code))
    }
}

/// Pin the process's multi-threaded apartment once, so COM proxy calls work
/// from whatever thread the transport registers doorbells on (we are a DLL —
/// the host process's COM state is not ours to assume). The cookie is held for
/// the process lifetime on purpose; failure is tolerated (the subsequent COM
/// call then reports the real error).
fn ensure_mta() {
    static MTA: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    MTA.get_or_init(|| {
        let mut cookie: wp::CO_MTA_USAGE_COOKIE = ptr::null_mut();
        // SAFETY: `cookie` is a valid out pointer; the cookie is deliberately
        // never decremented (MTA pinned until process exit).
        let _ = unsafe { wp::CoIncrementMTAUsage(&mut cookie) };
    });
}

/// Release a COM pointer on drop. Every interface here starts with the
/// IUnknown vtable, so a raw pointer can be released through that prefix.
struct ComGuard(*mut c_void);

impl ComGuard {
    /// # Safety
    /// `p` must be a live COM interface pointer whose vtable starts with
    /// IUnknown; the guard consumes the caller's reference.
    unsafe fn new(p: *mut c_void) -> Self {
        Self(p)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: construction contract — live interface pointer, our ref.
            unsafe { ((*(*self.0.cast::<*const wp::IUnknownVtbl>())).Release)(self.0) };
        }
    }
}

/// A FlexibleIov device's `IVmFiovGuestMemoryFastNotification`, resolved from
/// the VM worker process. One per device; lives until the device goes away.
pub struct FiovDoorbells {
    notification: *mut wp::IVmFiovGuestMemoryFastNotification,
}

// SAFETY: the interface pointer is an RPC proxy acquired in the (pinned)
// multi-threaded apartment — see `ensure_mta` — so it may be called and
// released from any thread.
unsafe impl Send for FiovDoorbells {}
unsafe impl Sync for FiovDoorbells {}

impl FiovDoorbells {
    /// Resolve `instance_id`'s fast-notification interface from the worker
    /// process of the VM identified by `runtime_id` (the HCS **runtime** id
    /// from the system's `Properties`, *not* the configuration GUID).
    ///
    /// The caller must own the compute system; the FlexibleIov slot for
    /// `instance_id` must already be added.
    pub fn connect(runtime_id: &sys::GUID, instance_id: &sys::GUID) -> Result<Self> {
        ensure_mta();
        let get_worker = wp::get_vm_worker_process().ok_or(Error(HR_MOD_NOT_FOUND))?;

        let mut access_raw: *mut c_void = ptr::null_mut();
        // SAFETY: both GUIDs are live references; `access_raw` is a valid out
        // pointer. The export's signature is pinned in `vmworker-sys`.
        hr(unsafe {
            get_worker(
                runtime_id,
                &wp::IID_IVM_VIRTUAL_DEVICE_ACCESS,
                &mut access_raw,
            )
        })?;
        if access_raw.is_null() {
            // The out parameter is `_Outptr_opt_` — guard the success-but-null case.
            return Err(Error(sys::E_NOINTERFACE));
        }
        // SAFETY: `access_raw` is the IVmVirtualDeviceAccess reference the call
        // just handed us; the guard owns it.
        let access = unsafe { ComGuard::new(access_raw) };

        let mut device_raw: *mut c_void = ptr::null_mut();
        let vtbl = access.0.cast::<wp::IVmVirtualDeviceAccess>();
        // SAFETY: `access` is live; GetDevice is slot 4 per the pinned vtable
        // declaration; GUIDs and the out pointer are valid.
        hr(unsafe {
            ((*(*vtbl).lpVtbl).GetDevice)(
                access.0,
                &wp::FLEXIO_DEVICE_ID,
                instance_id,
                &mut device_raw,
            )
        })?;
        if device_raw.is_null() {
            return Err(Error(sys::E_NOINTERFACE));
        }
        // SAFETY: as above — the returned device reference is ours to release.
        let device = unsafe { ComGuard::new(device_raw) };

        let mut notif_raw: *mut c_void = ptr::null_mut();
        // SAFETY: `device` is live; QueryInterface is slot 0 of every vtable.
        hr(unsafe {
            ((*(*device.0.cast::<*const wp::IUnknownVtbl>())).QueryInterface)(
                device.0,
                &wp::IID_IVM_FIOV_GUEST_MEMORY_FAST_NOTIFICATION,
                &mut notif_raw,
            )
        })?;
        if notif_raw.is_null() {
            return Err(Error(sys::E_NOINTERFACE));
        }
        // `access` and `device` release here; the notification interface holds
        // its own reference to the device object.
        Ok(Self {
            notification: notif_raw.cast(),
        })
    }

    /// Arm a doorbell: the guest's write of `trigger_value` to `bar`+`offset`
    /// signals `event` from the kernel. `flags` are the `HDV_DOORBELL_FLAG_*`
    /// values (the FIOV layer shares them). The COM proxy duplicates `event`
    /// into the worker process (`system_handle(sh_event)`).
    ///
    /// # Safety
    /// `event` must be a valid Win32 event handle that outlives the
    /// registration (until [`unregister_doorbell`](Self::unregister_doorbell)).
    pub unsafe fn register_doorbell(
        &self,
        bar: sys::HDV_PCI_BAR_SELECTOR,
        offset: u64,
        trigger_value: u64,
        flags: u64,
        event: sys::HANDLE,
    ) -> Result<()> {
        // SAFETY: `notification` is live for `self`'s lifetime; the caller
        // guarantees the event handle. HDV_PCI_BAR_SELECTOR and
        // FIOV_BAR_SELECTOR share their numeric values (Bar0..Bar5 = 0..5).
        hr(unsafe {
            ((*(*self.notification).lpVtbl).RegisterDoorbell)(
                self.notification.cast(),
                bar as wp::FIOV_BAR_SELECTOR,
                offset,
                trigger_value,
                flags,
                event,
            )
        })
    }

    /// Remove a doorbell previously armed with the same `(bar, offset,
    /// trigger_value, flags)` tuple.
    pub fn unregister_doorbell(
        &self,
        bar: sys::HDV_PCI_BAR_SELECTOR,
        offset: u64,
        trigger_value: u64,
        flags: u64,
    ) -> Result<()> {
        // SAFETY: `notification` is live for `self`'s lifetime.
        hr(unsafe {
            ((*(*self.notification).lpVtbl).UnregisterDoorbell)(
                self.notification.cast(),
                bar as wp::FIOV_BAR_SELECTOR,
                offset,
                trigger_value,
                flags,
            )
        })
    }
}

impl Drop for FiovDoorbells {
    fn drop(&mut self) {
        // SAFETY: our reference, released exactly once. If the worker process
        // is already gone (VM teardown) this releases the local proxy only.
        unsafe { ((*(*self.notification).lpVtbl).base.Release)(self.notification.cast()) };
    }
}

//! Safe RAII over `hdv-sys`. Handles are owned by `Drop` types; apertures unmap
//! themselves. This layer is **device-agnostic** — it knows nothing about virtio.
//! The virtio specifics live one crate up, in `virtio-hdv`.
//!
//! Split of responsibilities:
//! - [`DeviceHost`] owns the HDV device host bound to one compute system.
//! - [`Device`] is a non-owning handle to the device HDV created for us (HDV owns
//!   its lifetime via the `Initialize`/`Teardown` vtable callbacks); it exposes
//!   the device-side operations: guest memory, MSI delivery, doorbells.
//! - [`Aperture`] owns one guest-memory mapping and destroys it on drop.

use hdv_sys as sys;
use std::ffi::c_void;

pub mod pci;
pub mod proxy;

/// Re-export the raw GUID type so consumers can name device class/instance/host
/// ids (e.g. to derive distinct per-device instance ids) without depending on
/// `hdv-sys` directly.
pub use sys::GUID;

/// An HDV call failed. Carries the raw `HRESULT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub sys::HRESULT);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HDV call failed: HRESULT {:#010x}", self.0 as u32)
    }
}
impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn hr(code: sys::HRESULT) -> Result<()> {
    if code >= 0 {
        Ok(())
    } else {
        Err(Error(code))
    }
}

/// Owns an HDV device host bound to one externally-owned compute system.
/// Dropping it tears down every device created from it.
pub struct DeviceHost {
    handle: sys::HDV_HOST,
}

// The handle is owned exclusively and HDV's host APIs are thread-safe.
unsafe impl Send for DeviceHost {}
unsafe impl Sync for DeviceHost {}

impl DeviceHost {
    /// Initialize a device host against a compute system the caller owns
    /// (`HdvInitializeDeviceHost`). `compute_system` is an `HCS_SYSTEM` handle
    /// obtained from `HcsOpenComputeSystem`/`HcsCreateComputeSystem`.
    ///
    /// # Safety
    /// `compute_system` must be a valid, open `HCS_SYSTEM` handle that outlives
    /// this device host.
    pub unsafe fn open(compute_system: sys::HCS_SYSTEM) -> Result<Self> {
        let mut handle: sys::HDV_HOST = std::ptr::null_mut();
        // SAFETY: caller guarantees a valid HCS_SYSTEM; `handle` is a valid out ptr.
        hr(unsafe { sys::HdvInitializeDeviceHost(compute_system, &mut handle) })?;
        Ok(Self { handle })
    }

    /// Like [`open`](Self::open) but via `HdvInitializeDeviceHostEx`, so flags can
    /// be passed — notably [`HDV_DEVICE_HOST_FLAGS::InitializeComSecurity`], which
    /// the FlexibleIov resource-reservation path appears to require (vmwp invokes
    /// the emulator over COM).
    ///
    /// # Safety
    /// As [`open`](Self::open).
    pub unsafe fn open_ex(
        compute_system: sys::HCS_SYSTEM,
        flags: sys::HDV_DEVICE_HOST_FLAGS,
    ) -> Result<Self> {
        let mut handle: sys::HDV_HOST = std::ptr::null_mut();
        // SAFETY: caller guarantees a valid HCS_SYSTEM; `handle` is a valid out ptr.
        hr(unsafe { sys::HdvInitializeDeviceHostEx(compute_system, flags, &mut handle) })?;
        Ok(Self { handle })
    }

    /// Create a **proxied** device host via `HdvInitializeDeviceHostForProxy`: the
    /// host is bound to a compute system indirectly, through the
    /// `IVmDeviceHostSupport` callback (`device_host_support`), which forwards the
    /// device host to `HdvProxyDeviceHost` — the `ExternalRestricted` FlexibleIov
    /// path (see [`proxy`] + `docs/hdv-proxy-abi.md`). `device_host_id` is the host
    /// identity GUID HDV reads from the first argument — disassembly shows it is
    /// copied via a 16-byte `movups`, so it **must** be a valid `*const GUID`, not
    /// null.
    ///
    /// # Safety
    /// `device_host_id` must point to a live `GUID`; `device_host_support` must be a
    /// live `IVmDeviceHostSupport` `IUnknown*` (e.g.
    /// [`proxy::DeviceHostSupport::as_iunknown`]) that outlives this device host.
    pub unsafe fn from_proxy(
        device_host_id: *const sys::GUID,
        device_host_support: sys::PVOID,
    ) -> Result<Self> {
        let mut handle: sys::HDV_HOST = std::ptr::null_mut();
        // SAFETY: caller guarantees a valid GUID + IVmDeviceHostSupport; `handle` is
        // a valid out ptr.
        hr(unsafe {
            sys::HdvInitializeDeviceHostForProxy(
                device_host_id as sys::PVOID,
                device_host_support,
                &mut handle,
            )
        })?;
        Ok(Self { handle })
    }

    /// Raw host handle, for passing to `HdvCreateDeviceInstance`.
    pub fn raw(&self) -> sys::HDV_HOST {
        self.handle
    }
}

impl Drop for DeviceHost {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: `handle` came from a successful HdvInitializeDeviceHost and
            // is dropped exactly once.
            unsafe { sys::HdvTeardownDeviceHost(self.handle) };
        }
    }
}

/// A non-owning handle to an HDV device. HDV owns the device's lifetime (created
/// by `HdvCreateDeviceInstance`, torn down via the vtable `Teardown` callback);
/// this wrapper just exposes the device-side operations safely. `Copy` because it
/// is a borrowed handle, freely passed to the worker threads HDV calls us on.
#[derive(Clone, Copy)]
pub struct Device(pub sys::HDV_DEVICE);

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    /// Copy `buf.len()` bytes from guest physical memory (`HdvReadGuestMemory`).
    pub fn read_guest_memory(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
        // SAFETY: device handle valid for the device lifetime; buffer is in bounds.
        hr(unsafe { sys::HdvReadGuestMemory(self.0, gpa, buf.len() as u32, buf.as_mut_ptr()) })
    }

    /// Copy `buf` into guest physical memory (`HdvWriteGuestMemory`).
    pub fn write_guest_memory(&self, gpa: u64, buf: &[u8]) -> Result<()> {
        // SAFETY: as above; buffer is read-only and in bounds.
        hr(unsafe { sys::HdvWriteGuestMemory(self.0, gpa, buf.len() as u32, buf.as_ptr()) })
    }

    /// Map a guest physical range into this process (`HdvCreateGuestMemoryAperture`),
    /// returning a self-unmapping [`Aperture`].
    pub fn create_aperture(&self, gpa: u64, len: u32, write_protected: bool) -> Result<Aperture> {
        let mut mapped: sys::PVOID = std::ptr::null_mut();
        // SAFETY: valid device; `mapped` is a valid out ptr.
        hr(unsafe {
            sys::HdvCreateGuestMemoryAperture(
                self.0,
                gpa,
                len,
                write_protected as sys::BOOL,
                &mut mapped,
            )
        })?;
        Ok(Aperture {
            device: self.0,
            ptr: mapped,
            len: len as usize,
        })
    }

    /// Inject an MSI into the guest (`HdvDeliverGuestInterrupt`). The
    /// `(msi_address, msi_data)` pair is what the guest programmed into the
    /// device's MSI-X table.
    pub fn deliver_interrupt(&self, msi_address: u64, msi_data: u32) -> Result<()> {
        // SAFETY: valid device handle.
        hr(unsafe { sys::HdvDeliverGuestInterrupt(self.0, msi_address, msi_data) })
    }

    /// Arm a doorbell: when the guest writes `trigger_value` to `bar`+`offset`,
    /// `event` is signaled (`HdvRegisterDoorbell`). Pass `flags` from the
    /// `HDV_DOORBELL_FLAG_*` constants. `event` is a Win32 event `HANDLE`.
    ///
    /// # Safety
    /// `event` must be a valid Win32 event handle that outlives the registration
    /// (until [`unregister_doorbell`](Self::unregister_doorbell)).
    pub unsafe fn register_doorbell(
        &self,
        bar: sys::HDV_PCI_BAR_SELECTOR,
        offset: u64,
        trigger_value: u64,
        flags: u64,
        event: sys::HANDLE,
    ) -> Result<()> {
        // SAFETY: valid device; caller guarantees a valid event handle.
        hr(unsafe { sys::HdvRegisterDoorbell(self.0, bar, offset, trigger_value, flags, event) })
    }

    /// Remove a previously armed doorbell (`HdvUnregisterDoorbell`).
    pub fn unregister_doorbell(
        &self,
        bar: sys::HDV_PCI_BAR_SELECTOR,
        offset: u64,
        trigger_value: u64,
        flags: u64,
    ) -> Result<()> {
        // SAFETY: valid device handle.
        hr(unsafe { sys::HdvUnregisterDoorbell(self.0, bar, offset, trigger_value, flags) })
    }
}

/// A guest-memory mapping owned by this process. Destroys itself on drop.
pub struct Aperture {
    device: sys::HDV_DEVICE,
    ptr: sys::PVOID,
    len: usize,
}

unsafe impl Send for Aperture {}
unsafe impl Sync for Aperture {}

impl Aperture {
    /// Base of the mapping. Offset into it corresponds to the guest physical
    /// address passed to [`Device::create_aperture`].
    pub fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }

    /// Mapped length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Aperture {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: `ptr` came from HdvCreateGuestMemoryAperture on `device`
            // and is destroyed exactly once.
            unsafe { sys::HdvDestroyGuestMemoryAperture(self.device, self.ptr) };
        }
    }
}

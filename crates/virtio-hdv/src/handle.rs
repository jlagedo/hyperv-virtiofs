//! A late-bound, shareable [`hdv::Device`] handle.
//!
//! The HDV device handle (`requestor`, needed for guest-memory access and MSI
//! delivery) is only known *after* `HdvCreateDeviceInstance` returns — but the
//! `GuestMemoryAccess` and `SignalMsi` objects that need it must be built
//! *before* the create call (they are wired into the `VirtioPciDevice` whose
//! vtable we hand to HDV). This cell bridges that ordering gap: build the seam
//! objects against a clone of an empty [`DeviceHandle`], create the device, then
//! [`DeviceHandle::set`] the handle. By the time the guest first touches the
//! device (Start → config → MMIO → interrupts) the handle is populated.

use hdv::Device;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

/// A shareable slot holding the HDV device handle once it exists.
///
/// Cloneable (it is an `Arc` inside); all clones observe the same handle. The
/// stored pointer is HDV's opaque `HDV_DEVICE`, valid for the device's lifetime
/// (HDV owns it; we only borrow). Access before [`set`](Self::set) yields `None`.
#[derive(Clone)]
pub struct DeviceHandle(Arc<AtomicPtr<core::ffi::c_void>>);

impl DeviceHandle {
    /// A new, empty handle (returns `None` from [`get`](Self::get) until set).
    pub fn new() -> Self {
        Self(Arc::new(AtomicPtr::new(std::ptr::null_mut())))
    }

    /// Publish the device handle. Call exactly once, right after
    /// `HdvCreateDeviceInstance` succeeds.
    pub fn set(&self, device: Device) {
        self.0.store(device.0, Ordering::Release);
    }

    /// The device handle, or `None` if not yet set.
    pub fn get(&self) -> Option<Device> {
        let p = self.0.load(Ordering::Acquire);
        if p.is_null() {
            None
        } else {
            Some(Device(p))
        }
    }
}

impl Default for DeviceHandle {
    fn default() -> Self {
        Self::new()
    }
}

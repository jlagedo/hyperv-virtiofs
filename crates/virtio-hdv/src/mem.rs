//! Guest memory for the virtio device, backed by HDV guest-memory copies.
//!
//! OpenVMM's virtio queues read descriptors and data through a [`GuestMemory`],
//! which we build from this [`HdvGuestMemory`]. We return `mapping() = None`, so
//! every access takes the *fallback* path — a synchronous `HdvReadGuestMemory` /
//! `HdvWriteGuestMemory` copy through the HDV device handle. That is the simplest
//! correct backing; it is also a copy per access. A zero-copy aperture backing
//! (`HdvCreateGuestMemoryAperture` → a real `mapping()`) is the documented perf
//! follow-up — correctness first, throughput later.
//!
//! [`GuestMemory`]: guestmem::GuestMemory

use crate::handle::DeviceHandle;
use guestmem::{GuestMemory, GuestMemoryAccess, GuestMemoryBackingError};
use hdv_sys as sys;
use std::ptr::NonNull;

/// `GuestMemoryAccess` backed by HDV copy calls against a late-bound device.
pub struct HdvGuestMemory {
    handle: DeviceHandle,
    /// Upper bound on valid guest physical addresses (the guest RAM size).
    max_address: u64,
}

impl HdvGuestMemory {
    /// Wrap into an OpenVMM [`GuestMemory`]. `max_address` is the guest RAM size
    /// (the largest GPA the virtqueues may reference).
    pub fn into_guest_memory(handle: DeviceHandle, max_address: u64) -> GuestMemory {
        GuestMemory::new(
            "hdv",
            Self {
                handle,
                max_address,
            },
        )
    }
}

/// The device handle isn't bound yet (access before `HdvCreateDeviceInstance`).
#[derive(Debug)]
struct NotBound;
impl std::fmt::Display for NotBound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HDV device handle not bound yet")
    }
}
impl std::error::Error for NotBound {}

/// An `HdvReadGuestMemory`/`HdvWriteGuestMemory` call failed.
#[derive(Debug)]
struct HdvCopyFailed(sys::HRESULT);
impl std::fmt::Display for HdvCopyFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HDV guest-memory copy failed: HRESULT {:#010x}",
            self.0 as u32
        )
    }
}
impl std::error::Error for HdvCopyFailed {}

// SAFETY: `mapping()` returns `None`, so the unsafe mapping/bitmap contract is
// vacuous; all access goes through the fallback methods, which copy exactly
// `len` bytes via HDV and report failure rather than touching invalid memory.
unsafe impl GuestMemoryAccess for HdvGuestMemory {
    fn mapping(&self) -> Option<NonNull<u8>> {
        None
    }

    fn max_address(&self) -> u64 {
        self.max_address
    }

    unsafe fn read_fallback(
        &self,
        addr: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let device = self
            .handle
            .get()
            .ok_or_else(|| GuestMemoryBackingError::other(addr, NotBound))?;
        // SAFETY: caller guarantees `dest[..len]` is valid for write; we pass it
        // straight to HDV, which fills exactly `len` bytes on success.
        let hr = unsafe { sys::HdvReadGuestMemory(device.0, addr, len as u32, dest) };
        if hr >= 0 {
            Ok(())
        } else {
            Err(GuestMemoryBackingError::other(addr, HdvCopyFailed(hr)))
        }
    }

    unsafe fn write_fallback(
        &self,
        addr: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let device = self
            .handle
            .get()
            .ok_or_else(|| GuestMemoryBackingError::other(addr, NotBound))?;
        // SAFETY: caller guarantees `src[..len]` is valid for read; HDV copies it
        // into guest memory.
        let hr = unsafe { sys::HdvWriteGuestMemory(device.0, addr, len as u32, src) };
        if hr >= 0 {
            Ok(())
        } else {
            Err(GuestMemoryBackingError::other(addr, HdvCopyFailed(hr)))
        }
    }
}

//! Guest memory for the virtio device, backed by HDV **guest-memory apertures**.
//!
//! OpenVMM's virtio queues read descriptors/rings and read-write data buffers
//! through a [`GuestMemory`], which we build from this [`HdvApertureMem`]. We
//! return `mapping() = None`, so every access takes the *fallback* path, which we
//! service by mapping the target guest page range into this process with
//! `HdvCreateGuestMemoryAperture` and copying through the mapped VA.
//!
//! Why apertures and not `HdvReadGuestMemory`/`HdvWriteGuestMemory`: those copy
//! APIs return `E_ACCESSDENIED` for the guest's virtqueue/DMA memory — they don't
//! carry device DMA rights. WSL's own `wsldevicehost.dll` uses
//! `HdvCreateGuestMemoryAperture` exclusively (the copy APIs never appear in its
//! decompile), confirming apertures are *the* DMA path.
//!
//! **Aperture coherency (the subtle part).** We keep **one persistent aperture**
//! over the low guest RAM, created once and reused, rather than re-mapping per
//! access. HDV apertures are backed by an *evictable page cache* (WSL runs an
//! `HdvGuestMemoryEvictionWorker` thread to manage exactly this — see the
//! decompile), so they are not a plain coherent shared mapping. Two consequences,
//! both observed on the rig:
//!   - **Re-mapping per access is worse** (~40% mount-and-do-IO success): the
//!     create/destroy churn thrashes the cache, so the device intermittently reads
//!     a stale descriptor and stalls following a bad chain.
//!   - **One long-lived mapping is much better** (~80%): far less churn, but a
//!     residual window remains where a guest write isn't yet reflected in the
//!     host's cached page.
//!
//! The principled fix is to participate in HDV's eviction/invalidation protocol
//! (à la WSL's worker) so the mapping is fully coherent. Until then the residual
//! staleness is masked by the interrupt re-arm net (`lib.rs`) plus the spike
//! test's boot retry — enough to reliably demonstrate the end-to-end path.
//!
//! [`GuestMemory`]: guestmem::GuestMemory

use crate::handle::DeviceHandle;
use guestmem::{GuestMemory, GuestMemoryAccess, GuestMemoryBackingError};
use hdv::Aperture;
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock};

const PAGE: u64 = 4096;
/// Size of the single persistent low-memory aperture (3 GiB — fits a `u32`
/// `byteCount` and covers where the virtqueues/buffers live).
const PERSIST: u64 = 0xC000_0000;

/// `GuestMemoryAccess` backed by HDV apertures against a late-bound device.
pub struct HdvApertureMem {
    handle: DeviceHandle,
    /// Upper bound on valid guest physical addresses (the guest RAM size).
    max_address: u64,
    /// One persistent aperture over `[0, PERSIST)`, created on first access.
    low: OnceLock<PersistAperture>,
    /// Guards lazy creation against the race of two first-accessors.
    init: Mutex<()>,
}

/// A long-lived aperture plus its mapped base pointer. `Send`/`Sync` because the
/// pointer is only dereferenced under the page being valid for the device life.
struct PersistAperture {
    _ap: Aperture,
    base_ptr: *mut u8,
    len: u64,
}
unsafe impl Send for PersistAperture {}
unsafe impl Sync for PersistAperture {}

impl HdvApertureMem {
    /// Wrap into an OpenVMM [`GuestMemory`]. `max_address` is the guest RAM size
    /// (the largest GPA the virtqueues may reference).
    pub fn into_guest_memory(handle: DeviceHandle, max_address: u64) -> GuestMemory {
        GuestMemory::new(
            "hdv",
            Self {
                handle,
                max_address,
                low: OnceLock::new(),
                init: Mutex::new(()),
            },
        )
    }

    /// Run `f` with a host pointer to guest physical `[addr, addr+len)`.
    /// Accesses within the persistent low aperture use it; anything above falls
    /// back to a fresh one-shot aperture.
    fn with_mapping<R>(
        &self,
        addr: u64,
        len: usize,
        f: impl FnOnce(*mut u8) -> R,
    ) -> Result<R, GuestMemoryBackingError> {
        let device = self
            .handle
            .get()
            .ok_or_else(|| GuestMemoryBackingError::other(addr, NotBound))?;
        let end = addr
            .checked_add(len as u64)
            .ok_or_else(|| GuestMemoryBackingError::other(addr, BadRange))?;

        // Persistent path for the low region.
        if end <= PERSIST.min(self.max_address) {
            if self.low.get().is_none() {
                let _g = self.init.lock().unwrap();
                if self.low.get().is_none() {
                    let len = PERSIST.min(self.max_address);
                    let ap = device
                        .create_aperture(0, len as u32, false)
                        .map_err(|e| GuestMemoryBackingError::other(addr, ApertureFailed(e.0)))?;
                    let base_ptr = ap.as_ptr() as *mut u8;
                    let _ = self.low.set(PersistAperture {
                        _ap: ap,
                        base_ptr,
                        len,
                    });
                }
            }
            let p = self.low.get().unwrap();
            if end <= p.len {
                // SAFETY: `addr..end` lies within `[0, p.len)` mapped by the aperture.
                let ptr = unsafe { p.base_ptr.add(addr as usize) };
                return Ok(f(ptr));
            }
        }

        // Fallback: fresh one-shot aperture for high memory.
        let base = (addr / PAGE) * PAGE;
        let span = (end - base).div_ceil(PAGE) * PAGE;
        let ap = device
            .create_aperture(base, span as u32, false)
            .map_err(|e| GuestMemoryBackingError::other(addr, ApertureFailed(e.0)))?;
        // SAFETY: `addr..end` lies within `[base, base+span)` mapped by `ap`.
        let ptr = unsafe { (ap.as_ptr() as *mut u8).add((addr - base) as usize) };
        Ok(f(ptr))
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

/// The access range is degenerate (overflows the address space).
#[derive(Debug)]
struct BadRange;
impl std::fmt::Display for BadRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("guest access range overflows")
    }
}
impl std::error::Error for BadRange {}

/// `HdvCreateGuestMemoryAperture` failed.
#[derive(Debug)]
struct ApertureFailed(i32);
impl std::fmt::Display for ApertureFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HdvCreateGuestMemoryAperture failed: HRESULT {:#010x}",
            self.0 as u32
        )
    }
}
impl std::error::Error for ApertureFailed {}

// SAFETY: `mapping()` returns `None`, so the unsafe mapping/bitmap contract is
// vacuous; all access goes through the fallback methods, which copy exactly
// `len` bytes through an HDV aperture covering the target range.
unsafe impl GuestMemoryAccess for HdvApertureMem {
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
        let r = self.with_mapping(addr, len, |src| {
            // SAFETY: `src[..len]` is the mapped aperture range; `dest[..len]` is
            // valid for write per the caller's contract.
            unsafe { std::ptr::copy_nonoverlapping(src, dest, len) };
        });
        match &r {
            Ok(()) => crate::trace::trace!("read_guest gpa={addr:#x} len={len} ok"),
            Err(e) => crate::trace::trace!("read_guest gpa={addr:#x} len={len} FAILED: {e:?}"),
        }
        r
    }

    unsafe fn write_fallback(
        &self,
        addr: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let r = self.with_mapping(addr, len, |dest| {
            // SAFETY: `dest[..len]` is the mapped aperture range; `src[..len]` is
            // valid for read per the caller's contract.
            unsafe { std::ptr::copy_nonoverlapping(src, dest, len) };
        });
        match &r {
            Ok(()) => crate::trace::trace!("write_guest gpa={addr:#x} len={len} ok"),
            Err(e) => crate::trace::trace!("write_guest gpa={addr:#x} len={len} FAILED: {e:?}"),
        }
        r
    }
}

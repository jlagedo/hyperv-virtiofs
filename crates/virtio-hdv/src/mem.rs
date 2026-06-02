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
//! decompile), confirming apertures are *the* DMA path. Apertures are cached per
//! aligned region so repeated access to the rings doesn't re-map every time.
//!
//! [`GuestMemory`]: guestmem::GuestMemory

use crate::handle::DeviceHandle;
use guestmem::{GuestMemory, GuestMemoryAccess, GuestMemoryBackingError};
use hdv::Aperture;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::Mutex;

/// Region size for cached apertures (2 MiB, page-aligned). Each access maps the
/// covering region once and reuses it.
const CHUNK: u64 = 2 * 1024 * 1024;
const PAGE: u64 = 4096;

/// `GuestMemoryAccess` backed by HDV apertures against a late-bound device.
pub struct HdvApertureMem {
    handle: DeviceHandle,
    /// Upper bound on valid guest physical addresses (the guest RAM size).
    max_address: u64,
    /// Cached apertures keyed by their CHUNK-aligned base GPA.
    cache: Mutex<HashMap<u64, Aperture>>,
}

impl HdvApertureMem {
    /// Wrap into an OpenVMM [`GuestMemory`]. `max_address` is the guest RAM size
    /// (the largest GPA the virtqueues may reference).
    pub fn into_guest_memory(handle: DeviceHandle, max_address: u64) -> GuestMemory {
        GuestMemory::new(
            "hdv",
            Self {
                handle,
                max_address,
                cache: Mutex::new(HashMap::new()),
            },
        )
    }

    /// Run `f` with a host pointer to guest physical `[addr, addr+len)`, mapping
    /// (and caching) the covering aperture as needed. The pointer is valid only
    /// for the duration of `f`.
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

        // Fast path: the access fits inside one cached CHUNK.
        if len as u64 <= CHUNK && addr / CHUNK == (end - 1) / CHUNK {
            let base = (addr / CHUNK) * CHUNK;
            let mut cache = self.cache.lock().unwrap();
            if !cache.contains_key(&base) {
                let len32 = CHUNK.min(self.max_address.saturating_sub(base)).max(PAGE) as u32;
                let ap = device
                    .create_aperture(base, len32, false)
                    .map_err(|e| GuestMemoryBackingError::other(addr, ApertureFailed(e.0)))?;
                cache.insert(base, ap);
            }
            let ap = &cache[&base];
            // SAFETY: `addr..end` lies within `[base, base+len32)` mapped by `ap`.
            let ptr = unsafe { (ap.as_ptr() as *mut u8).add((addr - base) as usize) };
            return Ok(f(ptr));
        }

        // Slow path: access spans a CHUNK boundary — map an exact, page-aligned,
        // one-shot aperture (not cached).
        let base = (addr / PAGE) * PAGE;
        let span = ((end - base) + PAGE - 1) / PAGE * PAGE;
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

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
//! **Aperture coherency — the on-demand cache (the subtle part).** An HDV aperture
//! is a *direct* VID map of a guest physical page range
//! (`HdvCreateGuestMemoryAperture` → `HDV::ExtensibleDevice::CreateGuestMemoryAperture`
//! → `VidMapMemoryBlockPageRangeEx`; see `docs/hdv-aperture-internals.md`). A page
//! that is **already backed** when mapped stays coherent thereafter; a page that is
//! **not yet backed** at map time does *not* become coherent when the guest later
//! backs it. So mapping a large region *early* (as a single persistent aperture did)
//! captures mostly-unbacked pages and reads them stale forever.
//!
//! The fix, faithful to WSL's closed `hyper_v_hdv` crate and using only the
//! documented API, is a **per-range cache mapped on demand**: map the *exact*
//! accessed range (page-aligned base, page-rounded length) only when it is first
//! touched — by which point the guest has backed it — and **reuse** the mapping on
//! later hits. Hot ring/descriptor reads become cache hits. The cache is bounded by
//! a simple LRU count cap; on `ERROR_NOT_ENOUGH_QUOTA` (`0x80070718`, the same
//! signal WSL's `HdvGuestMemoryEvictionWorker` services) we evict the
//! least-recently-used entry and retry, synchronously.
//!
//! **The "staleness" was mostly a ceiling bug.** The long-blamed `file_selftest`
//! flakiness ("aperture snapshot staleness on sustained I/O") was traced to
//! [`ram_size_to_max_gpa`]: `max_address` was set to the flat RAM size, but Hyper-V
//! remaps part of guest RAM **above 4 GiB**, so high-RAM DMA buffers were rejected
//! by `guestmem` (before this file even ran) and the FUSE server returned `-EIO`.
//! With that fixed, the on-demand cache below carries 64 MiB transfers with zero
//! aperture failures. Genuine map-time coherence (map only *backed* pages, reuse)
//! still matters and is what this cache provides; it is no longer the bottleneck.
//!
//! **Instrumentation.** All diagnostics here are `tracing` events, routed by the
//! subscriber the `hyperv_virtiofs` cdylib installs (to the `hvfs_set_logger`
//! callback and/or stderr — see that crate's `logging` module). `VIRTIO_HDV_APERTURE_STATS=1`
//! enables the cache-stats event (DEBUG, target `virtio_hdv::aperture`:
//! ops/hits/creates/evicts/quota_retries/create_fails/bad_range/not_bound/live/
//! peak_live/max_span), emitted by op-count *or* wall-clock so even a stalled run
//! emits; aperture-create failures and out-of-`max_address` accesses are
//! rate-limited WARNs (guest-triggerable). `VIRTIO_HDV_TRACE=1` raises this crate to
//! TRACE for the per-access firehose, including byte dumps of small (≤64 B) guest
//! reads/writes (rings, descriptors, FUSE headers) — the tool that pinned down the
//! EIO above.
//!
//! [`GuestMemory`]: guestmem::GuestMemory

use crate::handle::DeviceHandle;
use guestmem::{GuestMemory, GuestMemoryAccess, GuestMemoryBackingError};
use hdv::Aperture;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

const PAGE: u64 = 4096;
/// `HRESULT_FROM_WIN32(ERROR_NOT_ENOUGH_QUOTA)` — aperture quota momentarily
/// exhausted. WSL's eviction worker frees quota in the background and the acquire
/// path retries; we evict an LRU entry and retry inline.
const ERROR_NOT_ENOUGH_QUOTA: u32 = 0x8007_0718;
/// Upper bound on cached apertures. Each is a live VID page-range map, so this
/// caps host VA / quota use. Hot entries (rings, descriptors) stay resident; cold
/// data-buffer ranges are evicted LRU.
const MAX_ENTRIES: usize = 1024;
/// Print a stats summary at most this often, by op count *or* wall-clock —
/// whichever comes first (when stats are enabled). The wall-clock floor guarantees
/// we still get a summary on a *stalled* run that never reaches the op count, which
/// is exactly the case we need data on.
const STATS_EVERY: u64 = 4096;
const STATS_EVERY_MS: u128 = 500;

/// Whether aperture stats logging is enabled (`VIRTIO_HDV_APERTURE_STATS`), read once.
fn stats_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VIRTIO_HDV_APERTURE_STATS").is_some())
}

/// Cheap counters for the aperture cache, for diagnosing coherence/quota behaviour.
/// The failure kinds are split so a stalled/erroring run tells us *which* seam broke
/// — an out-of-range GPA (`bad_range`), a pre-bind access (`not_bound`), or a real
/// `HdvCreateGuestMemoryAperture` rejection (`create_fails`, with its HRESULT logged
/// inline) — rather than collapsing them all into one opaque error count.
#[derive(Default)]
struct Stats {
    ops: AtomicU64,
    hits: AtomicU64,
    creates: AtomicU64,
    evicts: AtomicU64,
    quota_retries: AtomicU64,
    create_fails: AtomicU64,
    bad_range: AtomicU64,
    not_bound: AtomicU64,
    live: AtomicUsize,
    peak_live: AtomicUsize,
    /// Largest single mapped span (bytes) — flags an unexpectedly huge request.
    max_span: AtomicU64,
}

impl Stats {
    fn note_live(&self, n: usize) {
        self.live.store(n, Relaxed);
        self.peak_live.fetch_max(n, Relaxed);
    }

    /// Emit the counters as one structured `tracing` event (DEBUG, target
    /// `virtio_hdv::aperture`). `phase` is `"periodic"` or `"final"`.
    fn emit(&self, phase: &'static str) {
        tracing::debug!(
            target: "virtio_hdv::aperture",
            phase,
            ops = self.ops.load(Relaxed),
            hits = self.hits.load(Relaxed),
            creates = self.creates.load(Relaxed),
            evicts = self.evicts.load(Relaxed),
            quota_retries = self.quota_retries.load(Relaxed),
            create_fails = self.create_fails.load(Relaxed),
            bad_range = self.bad_range.load(Relaxed),
            not_bound = self.not_bound.load(Relaxed),
            live = self.live.load(Relaxed),
            peak_live = self.peak_live.load(Relaxed),
            max_span = self.max_span.load(Relaxed),
            "aperture stats"
        );
    }
}

/// One cached aperture mapping plus a recency stamp for LRU eviction.
struct CachedAperture {
    _ap: Aperture,
    /// Host VA of the mapping's base (the page-aligned guest base). Stored as
    /// `usize` so the struct stays `Send`/`Sync` via `Aperture`'s own bounds.
    base: usize,
    last_used: u64,
}

/// On-demand cache of guest-memory apertures, keyed by the page-aligned
/// `(base, len)` of the range mapped.
#[derive(Default)]
struct Cache {
    map: HashMap<(u64, u64), CachedAperture>,
    /// Monotonic recency clock for LRU.
    clock: u64,
}

impl Cache {
    /// Return the host base VA for a cached `(base, len)`, mapping it on demand.
    /// `err_addr` is the original access address, only for error reporting.
    fn get_or_create(
        &mut self,
        device: &hdv::Device,
        base: u64,
        len: u64,
        err_addr: u64,
        stats: &Stats,
    ) -> Result<usize, GuestMemoryBackingError> {
        self.clock += 1;
        let now = self.clock;
        if let Some(c) = self.map.get_mut(&(base, len)) {
            c.last_used = now;
            stats.hits.fetch_add(1, Relaxed);
            return Ok(c.base);
        }
        if self.map.len() >= MAX_ENTRIES {
            self.evict_one(stats);
        }
        // Map the range; on quota exhaustion, evict LRU and retry (bounded).
        let mut evictions = 0;
        let ap = loop {
            match device.create_aperture(base, len as u32, false) {
                Ok(ap) => break ap,
                Err(e) => {
                    if e.0 as u32 != ERROR_NOT_ENOUGH_QUOTA
                        || self.map.is_empty()
                        || evictions >= MAX_ENTRIES
                    {
                        stats.create_fails.fetch_add(1, Relaxed);
                        // Guest-triggerable (e.g. a descriptor into an unbacked range),
                        // so rate-limit it. hresult is structured for grepping.
                        crate::ratelimit::warn_ratelimited!(
                            gpa = base,
                            len,
                            live = self.map.len(),
                            hresult = e.0 as u32,
                            "HdvCreateGuestMemoryAperture failed"
                        );
                        return Err(GuestMemoryBackingError::other(
                            err_addr,
                            ApertureFailed(e.0),
                        ));
                    }
                    stats.quota_retries.fetch_add(1, Relaxed);
                    self.evict_one(stats);
                    evictions += 1;
                }
            }
        };
        let host_base = ap.as_ptr() as usize;
        self.map.insert(
            (base, len),
            CachedAperture {
                _ap: ap,
                base: host_base,
                last_used: now,
            },
        );
        stats.creates.fetch_add(1, Relaxed);
        stats.max_span.fetch_max(len, Relaxed);
        stats.note_live(self.map.len());
        Ok(host_base)
    }

    /// Drop the least-recently-used entry (its `Aperture` Drop unmaps it).
    fn evict_one(&mut self, stats: &Stats) {
        if let Some((&key, _)) = self.map.iter().min_by_key(|(_, c)| c.last_used) {
            self.map.remove(&key);
            stats.evicts.fetch_add(1, Relaxed);
            stats.note_live(self.map.len());
        }
    }
}

/// `GuestMemoryAccess` backed by an on-demand cache of HDV apertures against a
/// late-bound device.
pub struct HdvApertureMem {
    handle: DeviceHandle,
    /// Upper bound on valid guest physical addresses (the guest RAM size).
    max_address: u64,
    cache: Mutex<Cache>,
    stats: Stats,
    /// Start instant + last-emit stamp (ms since start) for wall-clock-throttled
    /// stats, so even a stalled run that never reaches `STATS_EVERY` ops emits.
    start: Instant,
    last_emit_ms: AtomicU64,
}

/// Base of the guest's high-memory region. Hyper-V (like QEMU) splits guest RAM
/// around the 32-bit MMIO hole: RAM below the low-memory ceiling sits under 4 GiB,
/// and the **remainder is remapped to start at 4 GiB**. So a guest with more than
/// the low-RAM region references DMA buffers at GPAs *above* 4 GiB.
const HIGH_RAM_BASE: u64 = 4 << 30;

/// Map a guest **RAM size** to the highest guest-physical address its virtqueues
/// may reference — the value `max_address` must admit.
///
/// The trap: with the 4 GiB MMIO-hole remap, the top GPA is **not** the flat RAM
/// size. A 4 GiB guest's high RAM lives in `[4 GiB, 4 GiB + (ram − low))`, so the
/// top is *above* 4 GiB even though total RAM is exactly 4 GiB. Setting
/// `max_address = ram_size` therefore rejects every buffer that lands in high RAM,
/// which OpenVMM surfaces (before it ever calls our fallback, so it's invisible to
/// the aperture stats) as the FUSE server returning **-EIO** — the flaky
/// `ls: Invalid argument` we chased to here.
///
/// We don't know Hyper-V's exact low/high split, so we use a provably-safe upper
/// bound: `4 GiB + ram_size`. Since the high region is `ram − low < ram`, its top
/// is strictly below `4 GiB + ram`, so this admits all real RAM for any split. The
/// slack pages in `[low, 4 GiB)` (the hole) and above true-top are simply unbacked;
/// the device never DMAs there, and if a corrupt descriptor pointed there the
/// aperture create would fail cleanly (now counted as `create_fails`). The ceiling
/// is a pure validation bound — nothing is allocated against it.
fn ram_size_to_max_gpa(ram_size: u64) -> u64 {
    HIGH_RAM_BASE.saturating_add(ram_size)
}

impl HdvApertureMem {
    /// Wrap into an OpenVMM [`GuestMemory`]. `ram_size` is the guest RAM size; the
    /// admissible GPA ceiling (`max_address`) is derived from it via
    /// [`ram_size_to_max_gpa`] to account for the high-memory remap above 4 GiB.
    pub fn into_guest_memory(handle: DeviceHandle, ram_size: u64) -> GuestMemory {
        GuestMemory::new(
            "hdv",
            Self {
                handle,
                max_address: ram_size_to_max_gpa(ram_size),
                cache: Mutex::new(Cache::default()),
                stats: Stats::default(),
                start: Instant::now(),
                last_emit_ms: AtomicU64::new(0),
            },
        )
    }

    /// Emit a stats summary if either threshold (op count or wall-clock) is due.
    /// Cheap on the hot path: a single relaxed add, then a clock read only when the
    /// op-count test misses.
    fn maybe_emit_stats(&self) {
        if !stats_on() {
            return;
        }
        let n = self.stats.ops.fetch_add(1, Relaxed);
        let due_by_ops = n % STATS_EVERY == STATS_EVERY - 1;
        let due_by_time = {
            let elapsed = self.start.elapsed().as_millis();
            let last = self.last_emit_ms.load(Relaxed) as u128;
            elapsed.saturating_sub(last) >= STATS_EVERY_MS
                && self
                    .last_emit_ms
                    .compare_exchange(last as u64, elapsed as u64, Relaxed, Relaxed)
                    .is_ok()
        };
        if due_by_ops || due_by_time {
            self.stats.emit("periodic");
        }
    }

    /// Run `f` with a host pointer to guest physical `[addr, addr+len)`. The range
    /// is served by a single aperture covering its page-aligned span (mapped on
    /// demand, cached, LRU-evicted). The cache lock is held across `f` so the
    /// mapping cannot be evicted while in use.
    fn with_mapping<R>(
        &self,
        addr: u64,
        len: usize,
        f: impl FnOnce(*mut u8) -> R,
    ) -> Result<R, GuestMemoryBackingError> {
        let device = self.handle.get().ok_or_else(|| {
            self.stats.not_bound.fetch_add(1, Relaxed);
            GuestMemoryBackingError::other(addr, NotBound)
        })?;
        let end = addr.checked_add(len as u64).ok_or_else(|| {
            self.stats.bad_range.fetch_add(1, Relaxed);
            GuestMemoryBackingError::other(addr, BadRange)
        })?;
        if end > self.max_address {
            self.stats.bad_range.fetch_add(1, Relaxed);
            // Guest-triggerable (a descriptor past guest RAM), so rate-limit it.
            crate::ratelimit::warn_ratelimited!(
                gpa = addr,
                len,
                end,
                max_address = self.max_address,
                "guest access exceeds max_address"
            );
            return Err(GuestMemoryBackingError::other(addr, BadRange));
        }

        // Page-align the base down and round the end up, so we map exactly the
        // touched page(s) — only ever pages the guest has already backed.
        let base = (addr / PAGE) * PAGE;
        let span = (end - base).div_ceil(PAGE) * PAGE;
        let span = span.min(self.max_address - base); // clamp to guest RAM (page-aligned)

        let r = {
            let mut cache = self.cache.lock().unwrap();
            let host_base = cache.get_or_create(&device, base, span, addr, &self.stats)?;
            // SAFETY: `addr..end` lies within `[base, base+span)` mapped by the cached
            // aperture, which stays alive while we hold the cache lock.
            let ptr = unsafe { (host_base as *mut u8).add((addr - base) as usize) };
            f(ptr)
        };

        // Periodic stats summary (by op count or wall-clock; cheap + gated).
        self.maybe_emit_stats();
        Ok(r)
    }
}

impl Drop for HdvApertureMem {
    fn drop(&mut self) {
        if stats_on() {
            self.stats.emit("final");
        }
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

/// The access range is degenerate (overflows the address space) or exceeds guest RAM.
#[derive(Debug)]
struct BadRange;
impl std::fmt::Display for BadRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("guest access range overflows or exceeds guest RAM")
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
            Ok(()) => {
                // Dump the bytes of *small* reads (rings, descriptors, FUSE headers)
                // so a stale/incoherent control read is visible as wrong content —
                // e.g. an avail.idx that never advances despite guest kicks.
                if len <= 64 && tracing::enabled!(tracing::Level::TRACE) {
                    // SAFETY: we just filled `dest[..len]`; reading it back is sound.
                    let bytes = unsafe { std::slice::from_raw_parts(dest, len) };
                    tracing::trace!("read_guest gpa={addr:#x} len={len} ok {bytes:02x?}");
                } else {
                    tracing::trace!("read_guest gpa={addr:#x} len={len} ok");
                }
            }
            // The failure kind (bad_range / not_bound / create_fails) is already
            // counted at its source in `with_mapping`; just trace here.
            Err(e) => tracing::trace!("read_guest gpa={addr:#x} len={len} FAILED: {e:?}"),
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
            Ok(()) => {
                if len <= 64 && tracing::enabled!(tracing::Level::TRACE) {
                    // SAFETY: `src[..len]` is valid for read per the caller's contract.
                    let bytes = unsafe { std::slice::from_raw_parts(src, len) };
                    tracing::trace!("write_guest gpa={addr:#x} len={len} ok {bytes:02x?}");
                } else {
                    tracing::trace!("write_guest gpa={addr:#x} len={len} ok");
                }
            }
            Err(e) => tracing::trace!("write_guest gpa={addr:#x} len={len} FAILED: {e:?}"),
        }
        r
    }
}

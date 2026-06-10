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
//! **Sharding for concurrency.** The cache is split into `SHARDS` independent shards
//! (routed by guest page number), each an `RwLock` map of `Arc<Aperture>` values with
//! a per-shard atomic recency clock. A **hit** — 99.9%+ of accesses — takes the shard
//! lock *shared*, restamps recency through atomics, and clones the `Arc` out; only a
//! miss (map + insert, with LRU evict) takes it exclusively. The copy then runs with
//! no lock held, and a concurrent eviction merely drops the cache's `Arc` while the
//! in-flight copy keeps the mapping alive (the unmap defers to the last holder). So
//! parallel request handling is never re-serialised here: not by the map lock, not by
//! the recency bump, and not by the stats counters, which are skipped entirely unless
//! `VIRTIO_HDV_APERTURE_STATS` is set. The map's hasher is a hand-rolled multiply-mix
//! (the keys are page-aligned ranges; SipHash buys nothing here).
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, RwLock};
use std::time::Instant;

const PAGE: u64 = 4096;
/// `HRESULT_FROM_WIN32(ERROR_NOT_ENOUGH_QUOTA)` — aperture quota momentarily
/// exhausted. WSL's eviction worker frees quota in the background and the acquire
/// path retries; we evict an LRU entry and retry inline.
const ERROR_NOT_ENOUGH_QUOTA: u32 = 0x8007_0718;
/// Upper bound on cached apertures across the whole cache. Each is a live VID
/// page-range map, so this caps host VA / quota use. Hot entries (rings,
/// descriptors) stay resident; cold data-buffer ranges are evicted LRU. The cap is
/// enforced **per shard** ([`PER_SHARD_MAX`]), so the global total stays
/// ~`MAX_ENTRIES` (`SHARDS * PER_SHARD_MAX`).
const MAX_ENTRIES: usize = 1024;
/// Number of cache shards (power of two). Each shard is independently locked, so
/// accesses to different shards never contend, and hits within a shard share its
/// read lock — the prerequisite for parallel request handling (the lock, not the
/// copy, is the only serial point).
const SHARDS: usize = 16;
/// Per-shard entry cap, so the global total stays bounded by [`MAX_ENTRIES`].
const PER_SHARD_MAX: usize = MAX_ENTRIES / SHARDS;
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
    /// Whether counters are recorded at all (`VIRTIO_HDV_APERTURE_STATS`), captured
    /// once at construction. When off, every counter below is skipped — under
    /// parallel request handling even relaxed `fetch_add`s on shared counters are a
    /// contended cache line on the hot path.
    enabled: bool,
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
    fn new() -> Self {
        Self {
            enabled: stats_on(),
            ..Default::default()
        }
    }

    /// Bump `counter` iff stats are enabled — every cache counter routes through
    /// here so the disabled case costs one predictable branch, no shared write.
    #[inline]
    fn count(&self, counter: &AtomicU64) {
        if self.enabled {
            counter.fetch_add(1, Relaxed);
        }
    }

    /// One aperture was inserted: bump the global live count and the high-water mark.
    /// (Per-shard `map.len()` is no longer the global count, so we track it directly.)
    fn inc_live(&self) {
        if self.enabled {
            let n = self.live.fetch_add(1, Relaxed) + 1;
            self.peak_live.fetch_max(n, Relaxed);
        }
    }

    /// One aperture was evicted from a shard's map.
    fn dec_live(&self) {
        if self.enabled {
            self.live.fetch_sub(1, Relaxed);
        }
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

/// Hand-rolled multiply-mix hasher for the shard maps, replacing SipHash. The keys
/// are page-aligned `(base, len)` pairs; a guest influencing which of ≤64 host-side
/// buckets an entry lands in is harmless, so DoS-resistant hashing buys nothing on
/// the 99.9% hit path.
#[derive(Clone, Copy, Default)]
struct PageHashBuilder;

struct PageHasher(u64);

impl std::hash::BuildHasher for PageHashBuilder {
    type Hasher = PageHasher;
    fn build_hasher(&self) -> PageHasher {
        PageHasher(0)
    }
}

impl std::hash::Hasher for PageHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        // Generic fallback — unused by the `(u64, u64)` key, kept total for safety.
        for &b in bytes {
            self.write_u64(b.into());
        }
    }

    fn write_u64(&mut self, n: u64) {
        // splitmix64-style: multiply by a large odd constant, fold the high bits
        // down so page-aligned keys (entropy in the middle bits) spread.
        let x = (self.0 ^ n).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = x ^ (x >> 32);
    }
}

/// One cached aperture mapping plus a recency stamp for LRU eviction.
///
/// The aperture is held in an [`Arc`] so a lookup can hand out a cheap clone and the
/// caller can copy through it **after** releasing the cache lock: eviction drops the
/// cache's `Arc`, but the in-flight caller's clone keeps the mapping alive until the
/// copy finishes (the actual unmap in `Aperture::drop` is deferred to the last
/// holder). This is what makes the copy safe outside the lock.
///
/// `last_used` is atomic so the **hit** path can restamp recency under the shard's
/// *read* lock; only within-shard ordering matters (eviction is per-shard). Generic
/// over the aperture handle (`A = Arc<Aperture>`) purely so eviction is unit-testable
/// without a live HDV device.
struct CachedEntry<A> {
    ap: A,
    last_used: AtomicU64,
}

/// The map inside one shard: page-aligned `(base, len)` → cached aperture.
type ShardMap<A> = HashMap<(u64, u64), CachedEntry<A>, PageHashBuilder>;

/// One of [`SHARDS`] independent slices of the on-demand aperture cache.
///
/// The hit path takes `cache` **shared** (`RwLock::read`) and bumps recency through
/// the atomics; only miss / insert / evict takes it exclusively. The recency `clock`
/// is per-shard, not global, because LRU only ever compares entries within one shard.
struct Shard<A = Arc<Aperture>> {
    cache: RwLock<ShardMap<A>>,
    /// Monotonic recency clock for LRU, ticked outside the map lock.
    clock: AtomicU64,
}

// Hand-rolled so `Shard<Arc<Aperture>>: Default` (derive would bound `A: Default`).
impl<A> Default for Shard<A> {
    fn default() -> Self {
        Self {
            cache: RwLock::new(HashMap::default()),
            clock: AtomicU64::new(0),
        }
    }
}

impl<A: Clone> Shard<A> {
    /// The hit path: shared lock, atomic recency restamp, clone out.
    ///
    /// Poison-tolerant (as is every lock below): this runs on the un-guarded OpenVMM
    /// worker threads, so a poisoned shard (from a panic already caught at a guarded
    /// boundary) must not unwind here — the cache state is structurally valid.
    fn lookup(&self, key: (u64, u64)) -> Option<A> {
        let map = self.cache.read().unwrap_or_else(|e| e.into_inner());
        let entry = map.get(&key)?;
        entry
            .last_used
            .store(self.clock.fetch_add(1, Relaxed) + 1, Relaxed);
        Some(entry.ap.clone())
    }

    /// Drop the least-recently-used entry. If no other access still holds an `Arc`
    /// clone, the entry's `Aperture` Drop unmaps it here; otherwise the unmap is
    /// deferred until that in-flight copy releases its clone.
    fn evict_one(map: &mut ShardMap<A>, stats: &Stats) {
        if let Some((&key, _)) = map.iter().min_by_key(|(_, c)| c.last_used.load(Relaxed)) {
            map.remove(&key);
            stats.count(&stats.evicts);
            stats.dec_live();
        }
    }
}

impl Shard<Arc<Aperture>> {
    /// The miss path: map `(base, len)` on demand under the **write** lock.
    /// `err_addr` is the original access address, only for error reporting. The
    /// returned `Arc` keeps the mapping alive even if a later access evicts the
    /// cache entry, so the caller may copy through it without holding the lock.
    #[allow(clippy::too_many_arguments)]
    fn get_or_create(
        &self,
        device: &hdv::Device,
        base: u64,
        len: u64,
        err_addr: u64,
        stats: &Stats,
        alive: &Arc<AtomicBool>,
    ) -> Result<Arc<Aperture>, GuestMemoryBackingError> {
        let mut map = self.cache.write().unwrap_or_else(|e| e.into_inner());
        let now = self.clock.fetch_add(1, Relaxed) + 1;
        // Re-check under the write lock: a concurrent miss on the same key can have
        // inserted while we waited, and we must adopt that entry, not map a duplicate.
        if let Some(c) = map.get(&(base, len)) {
            c.last_used.store(now, Relaxed);
            stats.count(&stats.hits);
            return Ok(c.ap.clone());
        }
        if map.len() >= PER_SHARD_MAX {
            Self::evict_one(&mut map, stats);
        }
        // Map the range; on quota exhaustion, evict LRU and retry (bounded).
        let mut evictions = 0;
        let ap = loop {
            match device.create_aperture(base, len as u32, false) {
                Ok(ap) => break ap,
                Err(e) => {
                    if e.0 as u32 != ERROR_NOT_ENOUGH_QUOTA
                        || map.is_empty()
                        || evictions >= PER_SHARD_MAX
                    {
                        stats.count(&stats.create_fails);
                        // Guest-triggerable (e.g. a descriptor into an unbacked range),
                        // so rate-limit it. hresult is structured for grepping.
                        crate::ratelimit::warn_ratelimited!(
                            gpa = base,
                            len,
                            live = map.len(),
                            hresult = e.0 as u32,
                            "HdvCreateGuestMemoryAperture failed"
                        );
                        return Err(GuestMemoryBackingError::other(
                            err_addr,
                            ApertureFailed(e.0),
                        ));
                    }
                    stats.count(&stats.quota_retries);
                    Self::evict_one(&mut map, stats);
                    evictions += 1;
                }
            }
        };
        // Stamp the host-liveness flag so this aperture's drop skips the unmap
        // once the device is torn down (the cache finalises on a worker thread,
        // which can otherwise race teardown and fault in `vmdevicehost`).
        let mut ap = ap;
        ap.set_liveness(alive.clone());
        let ap = Arc::new(ap);
        map.insert(
            (base, len),
            CachedEntry {
                ap: ap.clone(),
                last_used: AtomicU64::new(now),
            },
        );
        stats.count(&stats.creates);
        if stats.enabled {
            stats.max_span.fetch_max(len, Relaxed);
        }
        stats.inc_live();
        Ok(ap)
    }
}

/// `GuestMemoryAccess` backed by an on-demand cache of HDV apertures against a
/// late-bound device.
pub struct HdvApertureMem {
    handle: DeviceHandle,
    /// Host-liveness flag, stamped onto every cached [`Aperture`] so the cache's
    /// asynchronous teardown skips `HdvDestroyGuestMemoryAperture` once the
    /// device is gone (see [`hdv::DeviceHost::alive_flag`]).
    alive: Arc<AtomicBool>,
    /// Upper bound on valid guest physical addresses (the guest RAM size).
    max_address: u64,
    /// The aperture cache, split into [`SHARDS`] independently-locked shards so
    /// concurrent accesses to different shards don't contend (and hits within one
    /// shard share its read lock).
    shards: [Shard; SHARDS],
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
    pub fn into_guest_memory(
        handle: DeviceHandle,
        ram_size: u64,
        alive: Arc<AtomicBool>,
    ) -> GuestMemory {
        GuestMemory::new(
            "hdv",
            Self {
                handle,
                alive,
                max_address: ram_size_to_max_gpa(ram_size),
                shards: std::array::from_fn(|_| Shard::default()),
                stats: Stats::new(),
                start: Instant::now(),
                last_emit_ms: AtomicU64::new(0),
            },
        )
    }

    /// Emit a stats summary if either threshold (op count or wall-clock) is due.
    /// Cheap on the hot path: a single relaxed add, then a clock read only when the
    /// op-count test misses.
    fn maybe_emit_stats(&self) {
        if !self.stats.enabled {
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

    /// The cache shard owning a page-aligned `base`. Routes by page number so
    /// consecutive pages spread across shards evenly.
    fn shard(&self, base: u64) -> &Shard {
        &self.shards[(base >> 12) as usize & (SHARDS - 1)]
    }

    /// Run `f` with a host pointer to guest physical `[addr, addr+len)`. The range
    /// is served by a single aperture covering its page-aligned span (mapped on
    /// demand, cached, LRU-evicted). A hit holds the shard's **read** lock only long
    /// enough to clone the `Arc` (recency is restamped atomically); a miss maps under
    /// the write lock. Either way the `Arc<Aperture>` keeps the mapping alive across
    /// `f`, so the copy runs **without** the lock and can't be invalidated by a
    /// concurrent eviction.
    fn with_mapping<R>(
        &self,
        addr: u64,
        len: usize,
        f: impl FnOnce(*mut u8) -> R,
    ) -> Result<R, GuestMemoryBackingError> {
        let device = self.handle.get().ok_or_else(|| {
            self.stats.count(&self.stats.not_bound);
            GuestMemoryBackingError::other(addr, NotBound)
        })?;
        let end = addr.checked_add(len as u64).ok_or_else(|| {
            self.stats.count(&self.stats.bad_range);
            GuestMemoryBackingError::other(addr, BadRange)
        })?;
        if end > self.max_address {
            self.stats.count(&self.stats.bad_range);
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

        // Hit: shard read lock only (the 99.9% path). Miss: write lock + map inside
        // `get_or_create`. Either way the lock is released before the copy below.
        let shard = self.shard(base);
        let ap = match shard.lookup((base, span)) {
            Some(ap) => {
                self.stats.count(&self.stats.hits);
                ap
            }
            None => shard.get_or_create(&device, base, span, addr, &self.stats, &self.alive)?,
        };
        let host_base = ap.as_ptr() as usize;
        // SAFETY: `addr..end` lies within `[base, base+span)` mapped by `ap`. The
        // `Arc<Aperture>` clone we hold keeps that mapping alive across `f`, even if
        // another thread evicts the cache entry concurrently (the unmap defers to the
        // last `Arc` holder).
        let ptr = unsafe { (host_base as *mut u8).add((addr - base) as usize) };
        let r = f(ptr);

        // Periodic stats summary (by op count or wall-clock; cheap + gated).
        self.maybe_emit_stats();
        Ok(r)
    }
}

impl Drop for HdvApertureMem {
    fn drop(&mut self) {
        if self.stats.enabled {
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

#[cfg(test)]
impl<A: Clone> Shard<A> {
    /// Test-only insert mirroring the miss path's evict-if-full + recency stamp,
    /// without needing a live HDV device to create real apertures.
    fn insert(&self, key: (u64, u64), value: A, stats: &Stats) {
        let mut map = self.cache.write().unwrap_or_else(|e| e.into_inner());
        let now = self.clock.fetch_add(1, Relaxed) + 1;
        if map.len() >= PER_SHARD_MAX {
            Self::evict_one(&mut map, stats);
        }
        map.insert(
            key,
            CachedEntry {
                ap: value,
                last_used: AtomicU64::new(now),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Filling a shard past `PER_SHARD_MAX` evicts exactly the least-recently-used
    /// entry — including when a read-path `lookup` (not an insert) refreshed it.
    #[test]
    fn shard_evicts_lru_past_cap() {
        let shard: Shard<u64> = Shard::default();
        let stats = Stats::new();
        for i in 0..PER_SHARD_MAX as u64 {
            shard.insert((i * PAGE, PAGE), i, &stats);
        }
        // Touch the oldest entry through the hit path; entry 1 becomes the LRU.
        assert_eq!(shard.lookup((0, PAGE)), Some(0));
        shard.insert((4096 * PAGE, PAGE), 4096, &stats);
        let map = shard.cache.read().unwrap();
        assert_eq!(map.len(), PER_SHARD_MAX);
        assert!(map.contains_key(&(0, PAGE)), "refreshed entry must survive");
        assert!(
            !map.contains_key(&(PAGE, PAGE)),
            "LRU entry must be evicted"
        );
        assert!(map.contains_key(&(4096 * PAGE, PAGE)));
    }

    /// The multiply-mix hasher must spread consecutive page-aligned keys (the real
    /// key distribution) without collisions.
    #[test]
    fn page_hasher_spreads_consecutive_pages() {
        use std::hash::BuildHasher;
        let hashes: std::collections::HashSet<u64> = (0..1024)
            .map(|i| PageHashBuilder.hash_one((i * PAGE, PAGE)))
            .collect();
        assert_eq!(hashes.len(), 1024);
    }
}

# virtio-fs performance roadmap

**Implementation guide** for the data-path optimisation work. Each open item below is specced to
the point of "open the file and write it": the types to add, the API to reuse, the lock/borrow
discipline, and how to validate. Yardstick is [`perf-baseline.md`](perf-baseline.md) — re-run it
after each step and compare *ratios/deltas*, not absolute MB/s.

Source we link is pinned at rev `3d1207b`; a read-only worktree of it lives at `E:\dev\openvmm-pin`
(see [Source map](#source-map)). Validated against that source 2026-06-09: every API shape A relies
on (`try_next`/`poll_kick`/`complete`/`queue_state` signatures, `VirtioQueueCallbackWork: Send`,
public constructibility of `VirtioQueue`/`PolledWait`, `Session`/`VirtioFs` `Send + Sync`) was
confirmed verbatim; the caveats found are folded in below where they bite.

## The bottleneck

One request worker per device, draining one queue with a **serial** loop that runs the blocking
FUSE/host-FS call inline before popping the next descriptor. That's `virtiofsd --thread-pool-size=1`;
the cure is request-level parallelism on a single queue (target ~5–8× on 4k / metadata / random).
The aperture copy and host NVMe are the real floor below that.

Everything here is **in-repo, no OpenVMM patch**: we replace the device we hand the transport and
finish our own aperture cache. The serial loop lives *inside* OpenVMM's `VirtioFsDevice`; we stop
using it.

## Plan at a glance

| # | Item | Where | Gate |
|---|---|---|---|
| **F** ✅ | Sharded aperture cache + `Arc<Aperture>` values (copy runs lock-free) | `mem.rs` | done |
| **A′** | Aperture hit-path: `RwLock` + atomic recency + per-shard stats + fast hasher | `mem.rs` | none — do first |
| **A** | `OffloadVirtioFsDevice`: pop → thread-pool dispatch → channel-back → complete | new modules in `virtio-hdv` | needs A′ |
| **R** | Interrupt re-arm: drop per-tick alloc + activity-gated backoff | `lib.rs`, `interrupt.rs` | alongside A |
| **E/D** | `attr`/`entry` timeouts; `FUSE_WRITEBACK_CACHE` — **coherence tradeoffs** | wrap the `Fuse` backend | policy decision |
| **B** | Multiqueue (each queue = A internally) | `OffloadVirtioFsDevice` | guest kernel ≥ 6.10 |
| ~~C~~ | ~~async lxutil~~ | — | dropped — A makes it moot |

Order: **A′ → A (+R) → measure → decide E/D → maybe B.**

---

## A′ — aperture hit-path (`crates/virtio-hdv/src/mem.rs`)

F moved the memcpy out of the lock, but a cache **hit** (99.92% of accesses) still takes the shard
`Mutex` *exclusively* to bump LRU recency, and bumps a shared `Stats` atomic. Under A, N workers
re-serialise on the few shards that own the hot ring/descriptor pages. Make hits a shared read.

Changes:

1. **Shard = `RwLock<Cache>` + per-shard `AtomicU64` clock.** Recency only needs to be comparable
   *within* a shard (eviction is per-shard), so a per-shard clock avoids any global atomic.
2. **`CachedAperture { ap: Arc<Aperture>, last_used: AtomicU64 }`.** Hit path: take the **read**
   lock, `clock.fetch_add(1, Relaxed)`, `entry.last_used.store(n, Relaxed)`, clone the `Arc`,
   release. Miss / insert / evict: drop the read lock, take the **write** lock (the 0.08% path),
   and **re-check the key first** — two threads can miss the same page concurrently, and the loser
   must adopt the winner's entry, not map a duplicate aperture.
3. **Gate stats off the hot path.** Read `VIRTIO_HDV_APERTURE_STATS` once into a `bool`; skip all
   counter `fetch_add`s when off (today `stats.hits` increments unconditionally — a contended
   cache line under A). Or make counters per-shard and sum on emit.
4. **Hand-rolled page hasher.** Replace the `HashMap`'s SipHash with a ~15-line multiply-hasher
   `BuildHasher` keyed on the page number. Safe: the map is ≤64 entries/shard, not
   security-sensitive. (No new crate — CLAUDE.md.)

Leave `evict_one`'s O(n) scan as-is (n ≤ 64). `HdvApertureMem` stays `Send + Sync` (all `&self`).
The shape matches sharded-`RwLock` prior art (`quick-cache` is exactly this, with
`shards = parallelism × 4`); moka-style lock-free read buffers are overkill at ≤64 entries/shard.

**Validate:** baseline shows **no regression** (the win only appears under A); aperture stats stay
clean (`live/peak` unchanged, `fails/bad/notbound = 0`); a unit test that fills a shard past
`PER_SHARD_MAX` still evicts LRU.

---

## A — `OffloadVirtioFsDevice` (the ~8×)

Our own `virtio::VirtioDevice` that drains one request queue and runs the blocking
`fuse::Session::dispatch` on a worker pool. **Reuses** the entire FUSE server + host-FS backend;
we write only the queue/worker glue.

### New files (`crates/virtio-hdv/src/`)

| File | Contents |
|---|---|
| `payload.rs` | The two descriptor-chain adapters (`PayloadReader: io::Read + fuse::RequestReader`, `PayloadWriter: io::Write`) **copied ~verbatim** from OpenVMM `virtiofs/src/virtio_util.rs` (193 lines, public-API-only; upstream names `VirtioPayloadReader`/`VirtioPayloadWriter`), plus a `PayloadReplySender: fuse::ReplySender` that writes IoSlices via `PayloadWriter` and records `bytes_written` (mirror of `VirtioReplySender` in `virtio.rs:303-328`). |
| `pool.rs` | Hand-rolled blocking worker pool: N OS threads over `Arc<(Mutex<VecDeque<Job>>, Condvar)>` + shutdown flag. `submit(Job)`, `drain`, join on drop. Size from `VIRTIO_HDV_WORKERS` (default `clamp(cpus, 4..=16)`; `0` = inline, which can win at low queue depth). |
| `offload.rs` | `OffloadVirtioFsDevice` + the per-queue dispatcher. |

### The worker model (no `Mutex<VirtioQueue>` — single-owner queue)

The queue's `try_next`/`complete`/`poll_kick` all take `&mut self`, and the used ring has **no
internal locking** — synchronization is purely the `&mut` borrow — so **the queue stays owned by
its dispatcher task**; pool threads never touch it (this isn't a style choice, it's the only sound
model). Work flows out to the pool and results flow back over a channel; the dispatcher is the only
place `complete` is called.

**Out-of-order completion** is spec-legal (`VIRTIO_F_IN_ORDER` is never advertised) and both ring
impls write the work item's own id (`queue/split.rs:230-253` the descriptor index,
`queue/packed.rs:215-226` the buffer id) — but upstream's serial loop has only ever completed in
pop order, so A exercises OOO in this code **for the first time**. Note `traits()` advertises
`ring_packed`, so a modern Linux guest is likely negotiating the *packed* ring already. If e2e
shows used-ring stalls under A, the first lever is to stop advertising `ring_packed` (guest falls
back to split).

Per request queue, `start_queue` spawns one dispatcher task on our `VmTaskDriver`:

```text
dispatcher (single async task, owns VirtioQueue):
  loop {
    // 1. drain everything currently available — no await, no borrow held across await
    while let Some(work) = queue.try_next()? {
        inflight.fetch_add(1);
        pool.submit(Job { work, mem: mem.clone(), session: session.clone(), done: done_tx.clone() });
    }
    // 2. park until either a guest kick OR a completion, then loop
    poll_fn(|cx| {
        let mut progressed = false;
        while let Poll::Ready(Some((work, bytes))) = done_rx.poll_next_unpin(cx) {
            queue.complete(work, bytes);          // signals the MSI via the queue's notify
            inflight.fetch_sub(1);
            progressed = true;
        }
        match queue.poll_kick(cx) {               // re-arms the queue-event waker
            Poll::Ready(()) => Poll::Ready(()),
            Poll::Pending if progressed => Poll::Ready(()),
            Poll::Pending => Poll::Pending,       // both wakers (done_rx + kick) registered
        }
    }).await;
  }

pool worker (OS thread, blocking):
  let reader = PayloadReader::new(&mem, &work);
  match fuse::Request::new(reader) {
    Ok(req) => {
        let mut tx = PayloadReplySender::new(&mem, &work);
        session.dispatch(req, &mut tx, None);     // mapper = None (shmem_size = 0, no DAX)
        done.unbounded_send((work, tx.bytes_written));
    }
    Err(_) => { notify_corruption(); done.unbounded_send((work, 0)); }
  }
```

- `done` channel: `futures::channel::mpsc::unbounded` (futures is already a dep). Sender is `Send`;
  `VirtioQueueCallbackWork` is plain data (`Send`), so it crosses threads and back cleanly.
  Unbounded is safe by construction: a work item must be popped to be submitted, so inflight — and
  the channel — is capped by the queue size. No extra backpressure needed.
- **Lock/borrow rule:** never hold a queue borrow across `.await`. The `poll_fn` closure borrows
  `queue` only during each poll; `try_next` drains synchronously. This is the whole correctness
  crux of A.
- `mem: GuestMemory` and `session: Arc<fuse::Session>` are cheap to clone and `Sync`; pool threads
  write replies straight into guest memory (→ our aperture cache).
- **FUSE_INIT is deliberately not concurrent-safe** upstream: `Session` answers racy INITs with
  EIO (`fuse/src/session.rs:455`). Benign — the kernel serializes INIT before any other traffic,
  and a hostile guest spamming INIT gets EIO, not a panic. Don't "fix" that EIO.

### `VirtioDevice` impl

`OffloadVirtioFsDevice` must be `InspectMut + Send` (derive `InspectMut`, `#[inspect(skip)]` the
internals). Mirror `VirtioFsDevice`:

- `traits()`: `device_id = FS`, features `ring_event_idx | ring_indirect_desc | ring_packed`,
  `max_queues = 2`, `device_register_length = size_of(config)`. Config space = `{ tag: [u8;36],
  num_request_queues: u32 = 1 }` (keep single-queue; B raises this later).
- `read_registers_u32` / `write_registers_u32`: serve the config bytes (copy `VirtioFsDevice`).
- `start_queue(idx, resources, features, initial_state)`: build the queue exactly as
  `VirtioFsDevice` does —
  `VirtioQueue::new(*features, resources.params, resources.guest_memory.clone(), resources.notify,
  PolledWait::new(&driver, resources.event)?, initial_state)` — then spawn the dispatcher above.
  Handle queue 0 (hiprio) the same way (one pool serves all queues), as `VirtioFsDevice` does.
- `stop_queue(idx)`: signal the dispatcher to stop accepting new work, **quiesce** (await
  `inflight == 0` so in-flight replies complete), then return `queue.queue_state()`. The quiesce is
  **correctness-critical, not hygiene**: a `queue_state()` captured while works are
  popped-but-uncompleted loses those descriptors permanently — the guest's requests hang after
  restore.
- `reset`: drop all dispatchers, `session.destroy()`.

### Build it from (reuse)

- `fuse::Session::new(VirtioFs::new(workspace, opts))` — all FUSE ops + host I/O, unchanged.
  `dispatch(&self)` is `Sync`; `VirtioFs`/`lxutil` are `Send + Sync` and safe under concurrent
  calls (verified).
- `virtio::{VirtioQueue, QueueResources, VirtioQueueCallbackWork, DeviceTraits}`.

### Integration (`crates/virtio-hdv/src/lib.rs`)

Replace the two lines that build the device:

```rust
// before:
let fs_device = VirtioFsDevice::new(&driver_source, tag, fs, 0, None);
let mut pci_dev = VirtioPciDevice::new(Box::new(fs_device), …)?;
// after:
let dev = OffloadVirtioFsDevice::new(&driver_source, tag, fs, /*shmem*/0);
let mut pci_dev = VirtioPciDevice::new(Box::new(dev), …)?;
```

Everything else — `VirtioPciDevice`, the `PciOps`/HDV bridge, MSI, apertures, BAR routing — is
untouched. Gate behind `VIRTIO_HDV_WORKERS` (set `0`/unset → current inline behaviour) so we A/B on
the baseline.

Land A as two commits with separated risk: **A1** = `payload.rs` + `pool.rs` (verbatim copy +
hand-rolled pool, fully unit-testable, zero integration risk); **A2** = `offload.rs` + this wiring
(all the integration risk lives here, and the `VIRTIO_HDV_WORKERS` gate doubles as the A/B switch —
`0` vs `N` isolates A's delta even with A′ already landed). R rides with A2.

**Validate:** the e2e ladder (`file_selftest`, `PROOF_COMPLETE_PASS`) must stay green — it's the
correctness gate for the new worker. Then the perf baseline should show 4k / metadata / random
climb while 1M-block holds; aperture stats stay clean. Watch for: stuck `inflight` on stop,
descriptors never completed (used-ring stall), reply-write past the writable payload.

---

## R — interrupt re-arm (`lib.rs` `RearmNet`, `interrupt.rs`)

Completion volume rises sharply under A, so fix the re-arm net's waste:

- **Drop the per-tick `Vec` allocation** (`lib.rs` re-arm loop currently `clone()`s `seen` every
  5 ms): reuse one buffer — `local.clear(); local.extend_from_slice(&guard); drop(guard);` then
  deliver.
- **Activity-gated backoff:** `signal_msi` stamps an `AtomicU64 last_activity`. Tick tight (5 ms)
  for a window after the last completion; back off (→ ~50–100 ms) when idle. Keeps the lost-MSI
  safety net but stops machine-gunning an idle guest. The 5 ms tick is a latency-vs-CPU knob — keep
  it tight *under load*, loose only when quiet.

**Validate:** idle CPU drops; under load, no latency regression vs A-without-backoff.

---

## E / D — coherence knobs (POLICY — decide before coding)

Both are reachable **without a patch** by wrapping the `Fuse` backend in our own type that
delegates to `VirtioFs` — with a price tag: the `Fuse` trait has ~40 default-`ENOSYS` methods, so
the wrapper must mechanically forward every method `VirtioFs` implements (~300 lines of
delegation) or silently degrade those ops:

- **E** — rewrite `ENTRY_TIMEOUT` / `ATTRIBUTE_TIMEOUT` in the `fuse_entry_out` / `fuse_attr_out`
  replies before returning them.
- **D** — set `info.want |= FUSE_WRITEBACK_CACHE` in our wrapping `Fuse::init` (if the kernel is
  capable).

But each **spends a coherence guarantee OpenVMM kept on purpose** (`entry_timeout = 0` for
rename/out-of-band-mutation correctness; writeback changes durability/mtime semantics). **Gate:
who writes the share?** Guest-sole-writer → safe. Concurrent host writer → needs explicit
invalidation (host `ReadDirectoryChangesW` watcher → FUSE `notify_inval_*`), which is real work.
Do **not** implement until the consumer's ownership model is fixed. This is a product decision, not
a tuning knob.

---

## B — multiqueue (guest-gated)

Once A exists, multiqueue is just A per queue: bump `max_queues` and the `num_request_queues` our
device reports, and run a dispatcher+pool per request queue. **Needs guest kernel ≥ 6.10** (FUSE
virtio-fs multi-queue landed there, with ~5–5.5× reported upstream — which also corroborates this
plan's parallelism headroom; older guests use one request queue regardless). Lowest priority —
revisit only if A leaves a parallelism ceiling.

---

## Validation

```pwsh
.\test\run-perf-baseline.ps1 -ApertureStats      # ratios vs perf-baseline.md
.\test\run-e2e.ps1 -Test file_selftest           # correctness gate for A's worker
```

F already landed with no baseline regression (expected — the request path was still
single-threaded; F's win is unlocked by A). A′ should likewise be no-regression. A is where the
4k / metadata / random numbers move.

## Source map

- **Pinned worktree:** `E:\dev\openvmm-pin` (rev `3d1207b`) — read the exact linked source here.
- **Reuse (public API):**
  `virtio::{VirtioDevice, VirtioPciDevice, VirtioQueue, QueueResources, VirtioQueueCallbackWork (`payload`, `read_at_offset`, `write_at_offset`), DeviceTraits}`;
  `fuse::{Session, Fuse, Request, RequestReader, ReplySender, Mapper}`;
  `virtiofs::VirtioFs`; `guestmem::GuestMemory`.
  Queue API: `VirtioQueue::{try_next, poll_kick, complete, queue_state}` (`virtio/src/common.rs`).
- **Copy (private module, public-API-only, 193 lines):** `virtiofs/src/virtio_util.rs` →
  `crates/virtio-hdv/src/payload.rs`.
- **Do *not* use:** the serial worker loop at `virtiofs/src/virtio.rs:236` — that's what A replaces.
- **Hardcoded in upstream (E/D context):** `attr/entry` timeouts `virtiofs/src/lib.rs:38-43`;
  FUSE flags / `max_write` `fuse/src/session.rs:19-39`.

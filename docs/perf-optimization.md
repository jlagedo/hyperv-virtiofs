# virtio-fs data-path optimization

Where the per-request/metadata latency actually goes, and the ranked menu of fixes. Companion
to [`perf-baseline.md`](perf-baseline.md) (the numbers this is measured against) and the DAX
note in [`roadmap.md`](roadmap.md). All findings are traced to source, not inferred.

> Status: **analysis** — strategy **F is implemented and validated** (see
> [F: implemented](#f-implemented--validated)); the rest is analysis. Re-run the baseline after
> any change and compare ratios/deltas, not absolute MB/s.

## The hotpath

A single FUSE request (e.g. one `stat`) is serial at every stage:

```
guest kick (MSI-X doorbell → BAR0 MMIO write)
  └─ virtio-hdv/src/lib.rs  write_bar()                         [LOCK #1: Arc<Mutex<VirtioPciDevice>>]
       └─ poll task on DefaultPool::spawn_on_thread("virtio-hdv")   ← ONE OS thread (lib.rs:195)
            └─ VirtioFsWorker::run()   (openvmm virtiofs/src/virtio.rs:230)
                 loop {
                   let work = queue.next().await;                ← pop ONE descriptor
                   let bytes = process_virtiofs_request(work);   ← SYNCHRONOUS, blocks
                   queue.complete(work, bytes);                  ← only then, next iteration
                 }
                    └─ fuse Session::dispatch()  (sync match, session.rs:72)
                         └─ VirtioFs::get_attr → inode.lstat()
                              └─ lxutil NtReadFile / NtWriteFile / lstat   ← BLOCKING syscall
                                 // openvmm lxutil/windows/mod.rs:1164  "// TODO: Async I/O"
                 (every guest-memory touch inside the request:)
                    └─ HdvApertureMem::read/write_fallback → with_mapping()
                         [LOCK #3: Mutex<Cache>] held across the memcpy   (virtio-hdv/src/mem.rs:352)
```

### Three nested serializations (all confirmed in source)

| Layer | Where | Evidence |
|---|---|---|
| (a) one OS thread per device | `virtio-hdv/src/lib.rs:195` `DefaultPool::spawn_on_thread` | single-threaded `pal_async` pool drives poll + the queue worker |
| (b) one request queue, serial loop | openvmm `virtiofs/src/virtio.rs:73` `num_request_queues: 1`; worker loop has **no** `spawn` / `FuturesUnordered` / `select!` | pops one descriptor, awaits the handler to completion, then pops the next |
| (c) blocking syscalls, no offload | openvmm `lxutil/windows/mod.rs:1164,1212` literally `// TODO: Async I/O`; `NtReadFile`/`NtWriteFile`/`lstat` run inline | the single thread parks on the host filesystem |

This is structurally **identical to `virtiofsd --thread-pool-size=1`**, which the upstream
literature measures at **~7,995 IOPS random-4k-read vs ~65,200 at `--thread-pool-size=64`
(~8×)** — almost exactly our 4k/metadata gap. The baseline's "finding #1" is now mechanically
explained: we *are* the pool-size-1 case.

## Two findings the source dig added

1. **Metadata is throttled by cache *policy*, not only by concurrency.** OpenVMM hardcodes
   `attr_timeout = 1ms` and `entry_timeout = 0` (`virtiofs/src/lib.rs:38-43`), deliberately, for
   rename correctness. virtiofsd's default is **5 s** (cache=auto = 1 s). With near-zero
   timeouts the guest VFS re-issues `lookup`/`getattr` for almost every path access — exactly
   what dominates `git status` / build-tree / `node_modules` walks. The **cold** baseline can't
   see this (distinct files, nothing to cache), so the real-workload metadata cost is *larger*
   than the baseline shows **and is separable from concurrency**.

2. **The aperture lock is a latent, not current, bottleneck.** `Mutex<Cache>` (`mem.rs:352`) is
   held across each memcpy. Today it is uncontended (one thread) — which is *why* baseline
   finding #3 said the cache is not the bottleneck. The instant concurrency is added, N workers
   serialize on this one mutex. It is an **enabler dependency**, not a standalone win.
   `HdvApertureMem` is already `Send + Sync` (all `&self`), so the host-memory path is otherwise
   concurrency-ready.

## What we own vs. what is pinned upstream

| Knob | Location | Ours? |
|---|---|---|
| executor (thread count) | `virtio-hdv/src/lib.rs:195` | **ours** |
| aperture cache + its lock | `virtio-hdv/src/mem.rs` | **ours** |
| host path / fs backend | `virtio-hdv/src/lib.rs` (`VirtioFs::new`) | **ours** |
| serial worker loop | openvmm `virtiofs/src/virtio.rs:230` | pinned dep |
| `num_request_queues: 1` | openvmm `virtiofs/src/virtio.rs:73` | pinned dep |
| FUSE init flags, `max_write` (1 MiB) | openvmm `fuse/src/session.rs:19-34,495` | pinned dep |
| `attr_timeout` / `entry_timeout` | openvmm `virtiofs/src/lib.rs:38-43` | pinned dep |
| blocking → async syscalls | openvmm `lxutil/windows/mod.rs` | pinned dep |

The two highest-leverage levers live in the pinned OpenVMM crates. "Reuse, don't reimplement"
(CLAUDE.md) still holds — these are small **carried patches on a fork-pin**, or upstream
contributions, not reimplementations — but adopting them is a genuine decision.

## Strategy menu (leverage ÷ effort)

| # | Strategy | Moves | Effort | Fork? |
|---|---|---|---|---|
| **A** | **Thread-pool offload in the worker.** Pop descriptor → hand `Work` to a host worker pool → run the blocking syscall in parallel → return result over a channel → complete on the worker task. The virtiofsd model. | 4k / metadata / random (~5–8× ceiling) | Med | openvmm `virtio.rs` + **our** `mem.rs` lock |
| **F** ✅ | **Shard the aperture cache + `Arc<Aperture>` values** (99.92% hits = reads). *Prerequisite for A/B* — otherwise LOCK #3 re-serializes the parallel workers. **Done** — see [below](#f-implemented--validated). | enables A/B | Low–Med | ours only |
| **E** | **Raise `attr`/`entry` timeouts** (e.g. 1 s; `FUSE_AUTO_INVAL_DATA` is already negotiated; handle the rename case with explicit invalidation). | real-workload metadata (large; benchmark-invisible) | Low (2 constants + caveat) | openvmm, tiny |
| **D** | **`FUSE_WRITEBACK_CACHE`** in the init flags — coalesces small writes guest-side. | `seqwrite_4k` | Low | openvmm `session.rs` |
| **B** | **Multiqueue + multithreaded executor.** `num_request_queues > 1` + replace `spawn_on_thread` with a multi-thread pool. | bulk + parallel (Linux 6.10: ~5×) | High | openvmm + our executor; **needs guest kernel ≥ 6.10** |
| **C** | **Async / overlapped syscalls in lxutil** (the `// TODO: Async I/O`) — drive `NtReadFile` completions on the existing IOCP pool; concurrency with no extra threads. | everything, cleanly | High (deep lxutil surgery) | openvmm, large |

**Relationships:**

- **A is the headline** — the proven ~8× lever, works on a *single queue with no guest changes*.
  Gated on **F** (else the cache mutex caps it). Complicated by `queue.next(&mut self)` /
  `queue.complete(&mut self)` both borrowing the same `VirtioQueue`: completion must return to
  the worker task (pop → dispatch-to-pool → channel-back → complete), keeping queue access
  single-threaded while parallelizing the *work*.
- **E + D are the cheapest real-world wins** and orthogonal to concurrency — tiny patches that
  move metadata and 4k-writes for *actual* workloads before the executor is touched. **E likely
  beats A for `git`/build trees**, which re-stat the same inodes.
- **B (multiqueue)** is the weakest fit: needs guest ≥ 6.10, even virtiofsd upstream lacks the
  device side, and each queue is still internally serial — so B only pays off *combined with* A
  or C.
- **C** is architecturally cleanest (no thread explosion; each pool thread is a real host worker)
  but the largest change and upstream-only.

## Recommended sequence

1. **F** — shard the aperture cache. Ours, low-risk, unblocks everything; re-run the
   baseline to confirm no regression. ✅ **Done** — see [below](#f-implemented--validated).
2. **E + D** — timeouts + writeback flag. Tiny openvmm patches; measure metadata and
   `seqwrite_4k`. Best $/line for real workloads.
3. **A** — thread-pool offload. The structural ~8× for 4k / metadata / random, now safe because
   F has landed.
4. Defer **B / C** — bigger, guest-version- or upstream-gated; revisit only if A + F leave a
   parallelism ceiling.

## F: implemented & validated

Landed in `crates/virtio-hdv/src/mem.rs` (the recommended combo from the build-vs-buy review —
hand-rolled, no new dependency):

- **`Arc<Aperture>` values (1a).** A lookup hands back a cheap `Arc` clone; the memcpy runs with
  the lock released. A concurrent eviction drops only the cache's `Arc`, so the in-flight copy
  keeps the mapping alive and the unmap defers to the last holder — no use-after-free, no lock
  held across the copy.
- **Sharded lock (2a).** One global `Mutex<Cache>` → `[Mutex<Cache>; 16]`, routed by guest page
  number (`(base >> 12) & 15`). The `MAX_ENTRIES` cap is enforced per shard (`PER_SHARD_MAX = 64`)
  so the global total is unchanged.
- **Kept (3a + 4).** Simple per-shard LRU; the bounded `ERROR_NOT_ENOUGH_QUOTA` evict-and-retry
  loop, now per shard. Plus a global live-count (`inc_live`/`dec_live`) since per-shard `len()` is
  no longer the global count, and a poison-tolerant shard lock (the lock is taken on the un-guarded
  OpenVMM worker thread).

**Validation** (live, E: NVMe, 3/3 clean, vs. the [baseline](perf-baseline.md)): every metric
within the run-to-run band — **no regression**, exactly as expected since the OpenVMM worker is
still single-threaded (F's win is gated on strategy A). Aperture stats confirm correctness:

```
            ops        hit%     creates evicts quota_retries  fails/bad/notbound  live/peak  max_span
baseline  1,122,304   99.92%      924    413      413              0/0/0           511/511     4096
post-F    1,122,304   99.92%      868    357      357              0/0/0           511/511     4096
```

`live/peak = 511/511` matches the pre-sharding count **exactly** (proves the global counter is
right); `fails/bad/notbound = 0` (per-shard quota handling added no new failure mode); hit rate
identical. F is in place as the concurrency enabler; the measurable win arrives with **A**.

## References

- virtiofsd thread-pool-size — <https://www.mail-archive.com/virtio-fs@redhat.com/msg03251.html>
- `--thread-pool-size=0` (inline can win at low queue depth) — <https://listman.redhat.com/archives/virtio-fs/2020-November/msg00093.html>
- Linux 6.10 virtio-fs multiqueue ~5× (device-side support required) — <https://www.phoronix.com/news/Linux-6.10-FUSE>
- Proxmox cache-mode / thread A-B numbers — <https://forum.proxmox.com/threads/virtiofsd-in-pve-8-0-x.130531/>
- weka thread/DAX planning (per-thread cost framing) — <https://github.com/weka/virtiofs-bench/blob/main/notes/threads_and_dax_planning.md>
- FUSE writeback-cache semantics — <https://www.kernel.org/doc/Documentation/filesystems/fuse-io.txt>

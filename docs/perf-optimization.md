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
| **A′** ✅ | Aperture hit-path: `RwLock` + atomic recency + gated stats + fast hasher | `mem.rs` | done — baseline 3/3 clean, no regression |
| **A** ✅ | `OffloadVirtioFsDevice`: pop → thread-pool dispatch → channel-back → complete, + adaptive inline at QD 1 | `offload.rs`, `pool.rs`, `payload.rs` | done — e2e 8/8 green; +47–103% at depth, QD-1 parity ([results](#measured-outcome-2026-06-10)) |
| **R** ✅ | Interrupt re-arm: drop per-tick alloc + activity-gated backoff | `lib.rs`, `interrupt.rs` | done — rode with A |
| **P** ✅ | Request-path stage profiling (`VIRTIO_HDV_REQ_STATS`) | `reqstats.rs` | done — [profile](#where-the-residual-microseconds-go-request-path-profile-2026-06-10) |
| **K** ⛔ | Queue kicks via **HDV** doorbells (`HdvRegisterDoorbell`) | `doorbell.rs` | platform-blocked: `E_ACCESSDENIED` on restricted hosts (by design). Kept as auto-fallback; superseded by **K2** |
| **K2** 🎯 | Queue kicks via the **VM-worker** channel (`GetVmWorkerProcess` → `IVmFiovGuestMemoryFastNotification::RegisterDoorbell`) | `vmworker-sys` (new) + `doorbell.rs` | **top lever** — researched & viable ([writeup](#the-vm-worker-channel--the-real-doorbell-route-and-dax--2026-06-10)); needs a ~1-day rig spike |
| **E/D** | `attr`/`entry` timeouts; `FUSE_WRITEBACK_CACHE` — **coherence tradeoffs** | wrap the `Fuse` backend | policy decision — strong independent lever (removes round trips) |
| **DAX** | Section-backed BAR via `IVmFiovGuestMmioMappings::CreateSectionBackedMmioRange` (same channel as K2) | `vmworker-sys` + `mem.rs` | the read/mmap order-of-magnitude lever; rides K2's plumbing |
| **B** | Multiqueue (each queue = A internally) | `OffloadVirtioFsDevice` | de-prioritised — profile shows the dispatcher is nowhere near binding |
| ~~MSI batch~~ | ~~one MSI per completion drain~~ | — | dropped — ≤9 µs × 80% of ops, QD>1 only; invisible where the ceiling lives |
| ~~C~~ | ~~async lxutil~~ | — | dropped — A makes it moot |

Order: **A′ ✅ → A (+R) ✅ → measured ✅ → profile ✅ → HDV doorbell ⛔ → research VM-worker channel ✅ → K2 spike (next) → E/D / DAX.**

## Measured outcome (2026-06-10)

A landed gated behind `VIRTIO_HDV_WORKERS` (unset/`0` = upstream serial device). Full e2e
ladder 8/8 green at `workers=8`. The headline matrix (medians of 3, `jobs=8`; serial-device
vs offload, 2 vs 8 guest vCPUs — `HVFS_PB_CPUS`):

| ops/s | serial 2cpu | serial 8cpu | offload 2cpu | offload 8cpu | offload vs serial @8cpu |
|---|---:|---:|---:|---:|---:|
| par_create | 2976 | 2949 | 4901 | 5988 | **+103%** |
| par_stat | 8695 | 8928 | 9708 | 13157 | **+47%** |
| par_read | 5000 | 5000 | 5555 | 7352 | **+47%** |

What the campaign established:

1. **QD 1 cannot show A — by protocol.** FUSE is synchronous per op, so every
   single-threaded guest workload has exactly one request outstanding; the original bench
   was QD-1 in *all* phases. The first A/B measured the pool's thread-hop as a ~10%
   *regression* on 4k/metadata. Fixed by **adaptive inline** in the dispatcher (a lone
   request on an idle queue runs inline, upstream-style; pool engages at batch > 1 or
   work in flight) — QD-1 phases now at exact serial parity.
2. **The serial device is device-bound; the offload device scales.** Serial is flat from
   2 → 8 vCPUs (the worker is the ceiling); offload gains +22–36% more from 8 vCPUs.
   New `PB_PAR_*` bench phases (8 parallel guest jobs, **shell builtins only** — a fork
   per op measures guest process spawn, not host concurrency: host FUSE RTT is ~115 µs,
   a guest fork ~1.5 ms) are what exposed this.
3. **The residual ceiling is the still-serial per-op path**, not FUSE dispatch: the one
   dispatcher task does pop + complete + one `HdvDeliverGuestInterrupt` per completion,
   and every request crosses the aperture fallback several times (descriptor read,
   payload read, reply write, used-ring write). The profile below decomposed it.

## Where the residual microseconds go (request-path profile, 2026-06-10)

`VIRTIO_HDV_REQ_STATS=1` (`reqstats.rs`, `run-perf-baseline.ps1 -ReqStats`) times every
host-side stage of a request; zero overhead when off (bench at parity with stats on).
Captured at `workers=8`, 84k ops across all phases (91% lone batches = QD-1 dominated):

| stage | mean | n | what it is |
|---|---:|---:|---|
| pop (`try_next` drain) | 1 µs | 74k | descriptor + payload reads via aperture hits |
| FUSE dispatch, inline (QD-1) | 20 µs | 64k | parse + host-FS call + reply write |
| FUSE dispatch, pool (parallel) | 32 µs | 21k | same, under host-FS concurrency |
| pool_wait + done_hop | 6 + 8 µs | 21k | the thread-hop tax (pool path only) |
| complete (used-ring write + MSI) | 8 µs | 84k | |
| `HdvDeliverGuestInterrupt` alone | 9 µs | 67k | **delivers/ops = 80%** — event_idx suppresses only 20% |

Host-side total per QD-1 op ≈ **30 µs**. The cleanest serial yardstick — `seqwrite_4k`
at 41 MB/s ≈ 10.6k ops/s ≈ **95 µs/op** — leaves **~65 µs/op outside every measured host
stage**: the guest driver plus the **kick path** (a BAR0 MMIO write delivered as a guest
exit + cross-process HDV vtable call into `write_bar`) plus the IOCP wake. Consequences:

- **Not dispatcher-bound**: the dispatcher's serial budget is ~10 µs/op (~100k ops/s
  ceiling) vs 13k observed at depth → **B (multiqueue) is not the next lever**.
- **Not MSI-bound**: batching one MSI per drain would save ≤9 µs on 80% of ops, only at
  QD>1 → **dropped**.
- The kick path is the dominant non-FS cost → the doorbell experiment below.

### Doorbell experiment (K) — platform-blocked, kept as auto-fallback

`doorbell.rs` implements OpenVMM's `DoorbellRegistration` seam on `HdvRegisterDoorbell`
(the VID consumes the guest's notify write **in kernel** and signals the queue event
directly — deleting the user-mode intercept round trip per request). Getting the
transport to even *attempt* registration surfaced two config-emulator truths worth
keeping (both fixed in `lib.rs`):

1. **The VID owns the guest-facing config space**: the guest's command-register write
   never reaches `write_config`, so the internal emulator never saw memory-space decode
   enabled, `bar_address(0)` stayed `None`, and `install_doorbells` silently no-oped.
   Fixed by enabling decode (bit 1 only — pre-enabled bus master makes the VID reject
   the device) right after `HdvCreateDeviceInstance`.
2. **The VID re-calls `GetDetails` at FlexibleIov hot-add**, and `pci_core` ignores BAR
   writes while decode is on — so the BAR-sizing probe must clear/restore the command
   register around itself or enumeration breaks (observed: guest boots, share never
   mounts).

Outcome: `HdvRegisterDoorbell` → **`E_ACCESSDENIED (0x80070005)`** on our
`ExternalRestricted` device host. This is structural, not a bug: WSL's own restricted
device host never calls it either — doorbells are registered by the **privileged
broker** (`DeviceHostProxy.cpp`: `vmwpctrl.dll!GetVmWorkerProcess` →
`IVmVirtualDeviceAccess::GetDevice` → `IVmFiovGuestMemoryFastNotification::RegisterDoorbell`).
The code stays in, default-on with a `VIRTIO_HDV_DOORBELL=0` kill switch: registration
failure falls back to the MMIO intercept transparently (verified — baseline 3/3 at
parity, e2e green), and it lights up automatically in any process where HDV grants it.

**The real route — not parked, just relocated:** the doorbell `E_ACCESSDENIED` is a
signpost, not a wall. The owner side of the VM *can* register doorbells through the VM
worker process, and we are the owner. The deep dive into WSL source + the decompiles +
Microsoft Learn is written up in full below — it is the highest-value remaining lever and
it also unlocks DAX.

---

## The VM-worker channel — the real doorbell route (and DAX) — 2026-06-10

> Researched against three independent sources that all agree: Microsoft's open-source
> **WSL** (`E:\dev\WSL`, MIT), the decompiled **`vmdevicehost.dll`** / **`wsldevicehost.dll`**
> (`E:\tmp\rev`), and **Microsoft Learn** (the interface pages are officially documented —
> verified live). This is "advanced/undocumented" only at the edges; the core is in the docs
> and shipping in WSL.

### The key realisation

`HdvRegisterDoorbell` returning `E_ACCESSDENIED` is **by design for restricted hosts**, and
WSL hits exactly the same wall. Confirmed in the decompile: `wsldevicehost.dll`'s own device
host branches on a broker pointer (`DeviceHost+0x70`) — if a broker is present it forwards the
doorbell to it and **never calls `HdvRegisterDoorbell`**; only a non-restricted host takes the
HDV path. And in `vmdevicehost.dll`, `HDV::ExtensibleDevice::RegisterDoorbell` throws
`E_ACCESSDENIED` immediately when the restricted flag (`ExtensibleDevice+0x58`, sourced from
`DeviceHost+0x60` at create) is set; only a non-restricted device is ever handed the
`IVmDeviceVirtualizationServices` forwarding pointer (`+0x68`, QI'd for IID
`5bb5ff1d-7db6-4651-9681-f7f37e037b3c` in `ExtensibleDevice::Initialize`). So the restricted
in-process path **cannot** get a real doorbell — the only route is through the VM worker
process, reached from a process that **owns the VM**.

We own the compute system. So we can do exactly what WSL's broker does.

### What WSL actually does (reference implementation)

`src/windows/common/DeviceHostProxy.cpp` (`DeviceHostProxy::RegisterDoorbell`), verbatim shape:

```
own the compute system (we already do)
  → GetVmWorkerProcess(runtimeId, IID_IVmVirtualDeviceAccess, &deviceAccess)   // vmwpctrl.dll
    → deviceAccess->GetDevice(FLEXIO_DEVICE_ID, instanceId, &device)
      → device.query<IVmFiovGuestMemoryFastNotification>()
        → ->RegisterDoorbell(barSelector, barOffset, triggerValue, flags, eventHandle)
```

Cache the `IVmFiovGuestMemoryFastNotification` per device — on Unregister the device can no
longer be re-fetched from the worker (it's being removed), so the stored pointer is required.
WSL caps doorbells at **8 per device** (`DEVICE_HOST_PROXY_DOORBELL_LIMIT`) as a security gate;
virtio-9p uses 1, wsldevicehost uses 2 (we need 1–2: hiprio + request queue).

### The exact constants (copy these)

| Symbol | Value | Source / confidence |
|---|---|---|
| `IVmVirtualDeviceAccess` IID | `3e57bd3c-5a5d-4bdc-a0a6-5b4193d4b719` | WSL IDL (open source) |
| `IVmFiovGuestMemoryFastNotification` IID | `f5dfbec1-b9f3-4b26-bf6f-c251448bcf7a` | WSL IDL; **methods on MS Learn (verified)** |
| `IVmFiovGuestMmioMappings` IID (DAX) | `9d416457-abbc-46cf-8b93-901c68bec627` | WSL IDL; **MS Learn** |
| `IVmDeviceVirtualizationServices` IID | `5bb5ff1d-7db6-4651-9681-f7f37e037b3c` | `vmdevicehost.dll` decompile (in-host forwarder) |
| `FLEXIO_DEVICE_ID` (GetDevice category) | `a8679153-843f-467f-ad7e-f429328f7568` | WSL `wdk.h` (marked undocumented) |
| `GetVmWorkerProcess` (in `vmwpctrl.dll`) | `STDAPI (REFGUID vmRuntimeId, REFIID, IUnknown**)` | WSL (undocumented; WSL ships on it) |
| `HdvProxyDeviceHost` (in `vmdevicehost.dll`) | `(HCS_SYSTEM, PVOID host_IUnknown, DWORD pid, UINT64* ipcSection)` | **MS Learn**; the ExternalRestricted register call |
| `FIOV_BAR_SELECTOR` | `FIOV_BAR0..5`, `FIOV_ROMBAR` | WSL IDL (Learn omits ROMBAR) |
| `FiovMmioMappingFlags` | `None=0, Writeable=1, Executable=2` | WSL IDL |

`RegisterDoorbell` signature (MS Learn, verified):
`HRESULT RegisterDoorbell(FIOV_BAR_SELECTOR BarIndex, UINT64 BarOffset, UINT64 TriggerValue, UINT64 Flags, [system_handle(sh_event)] HANDLE NotificationEvent)`.
`GetDevice` (MS Learn, verified): `HRESULT GetDevice(REFGUID CategoryID, REFGUID DeviceID, IUnknown** Device)`.

### Why this is *simpler* for us than for WSL

WSL needs a whole cross-process broker (`DeviceHostProxy`, LRPC, handle marshalling) because
its device host is sandboxed in a **separate** low-trust process from the VM owner. In our
deployment the elevated service is **both** the VM owner and the host of this DLL — one process.
So we skip the broker entirely: call `GetVmWorkerProcess` directly, pass the queue `Event`
handle without cross-process marshalling, register. **No second process, no RPC, no extra
thread.** It slots into the existing `doorbell.rs` `DoorbellRegistration` seam as the path
taken when `HdvRegisterDoorbell` returns `E_ACCESSDENIED`.

Preconditions already satisfied by our flow: we own the compute system; shares are already
`FlexibleIov` / `ExternalRestricted`; we already expose the device `instance_id`
(`hvfs_share_instance_id`). The one new input is the VM **runtime ID**, which the consumer
(service) knows because it created the VM — likely a small ABI/threading addition.

### The strategic bonus: the same channel is the DAX path

`GetDevice` also yields **`IVmFiovGuestMmioMappings`**, whose `CreateSectionBackedMmioRange`
maps a host memory section straight into a guest BAR page range — exactly the mechanism for a
DAX window (`shmem_size > 0`), the deferred "next order of magnitude on reads/mmap" lever. So
one piece of COM plumbing unlocks **both** remaining heavyweight levers: the doorbell kick
*and* DAX.

### Risks / the easy-to-get-wrong list

- **Undocumented exports:** `GetVmWorkerProcess`, `vmwpctrl.dll`, `FLEXIO_DEVICE_ID` are not in
  any SDK header (WSL marks them so) — but WSL ships in production on them, so they're
  ABI-stable in practice. The MMIO-intercept fallback stays as insurance against a future
  Windows change.
- **Vtable-offset trap:** `IVmVirtualDeviceAccess` has **two reserved slots before `GetDevice`**
  (it sits at vtable index 3). Hand-declare the vtable wrong and you call the wrong slot. WSL's
  IDL has the exact layout to copy.
- **Runtime ID ≠ config GUID:** `GetVmWorkerProcess` wants the HCS *runtime* ID, not the VM
  config GUID. Mixing them = silent failure.
- **COM in Rust:** hand-rolled vtable FFI for ~3 interfaces + event-handle duplication. Real but
  bounded `unsafe`; fits our `*-sys` / safe-wrapper layering (a new `vmworker-sys` + thin safe
  wrapper). No new external crates.

### Proposed spike (before committing the feature)

~1 day on the rig: thread the runtime ID in; add `vmworker-sys` (`GetVmWorkerProcess` + the
three COM interfaces with the correct vtable layout); in `doorbell.rs`'s `E_ACCESSDENIED`
branch, try the worker path. **Success = registration returns `S_OK` *and* the request-path
profile (`VIRTIO_HDV_REQ_STATS`) shows the kick/deliver cost dropping** — the proof the doorbell
is live, not just accepted. If it works, the same shim is ~80% of the DAX groundwork.

### Re-ranked levers (supersedes the profile's first cut)

1. **VM-worker doorbell** (this section) — directly attacks the platform-bound ~65 µs kick
   floor, documented-enough that Microsoft's own product depends on it. **Top lever.**
2. **E/D** (guest attr/entry caching + writeback) — removes round trips entirely; still
   valuable and independent of the above, pending the coherence-policy decision.
3. **DAX** via `IVmFiovGuestMmioMappings` — same channel as #1; the read/mmap order-of-magnitude
   lever, now with a concrete API.
4. ~~B (multiqueue)~~, ~~MSI batch~~ — stay parked; the profile shows neither is binding.

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

A/B the offload device with `VIRTIO_HDV_WORKERS` (unset = serial, `8` = pool) and sweep
guest parallelism with `HVFS_PB_JOBS` / vCPUs with `HVFS_PB_CPUS`. Only the `par_*` rows
can move under A — the serial rows are QD-1 by protocol and gate *parity*, not gains
(see [Measured outcome](#measured-outcome-2026-06-10)).

F and A′ landed with no baseline regression (expected — the request path was still
single-threaded; their win is unlocked by A). A landed at +47–103% on the parallel
phases with serial parity everywhere else.

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

### VM-worker channel sources (K2 / DAX)

- **WSL (open source, MIT), `E:\dev\WSL`** — the reference broker implementation:
  - `src/windows/common/DeviceHostProxy.{h,cpp}` — `RegisterDoorbell`/`UnregisterDoorbell`,
    `GetVmWorkerProcess` → `GetDevice` → `IVmFiovGuestMemoryFastNotification`, the 8-doorbell cap.
  - `src/windows/service/inc/windowsdefs.idl` — the IDL with IIDs for `IVmVirtualDeviceAccess`,
    `IVmFiovGuestMemoryFastNotification`, `IVmFiovGuestMmioMappings`; `FIOV_BAR_SELECTOR`.
  - `src/windows/inc/wdk.h` — `GetVmWorkerProcess`, `HdvProxyDeviceHost`, `FLEXIO_DEVICE_ID`.
  - `src/windows/common/hcs_schema.h` — `FlexibleIoDevice` / `ExternalRestricted`.
- **Decompiles, `E:\tmp\rev`** — `vmdevicehost.dll.c` (`HDV::ExtensibleDevice::RegisterDoorbell`
  E_ACCESSDENIED gate at `+0x58`; `IVmDeviceVirtualizationServices` forwarder at `+0x68`,
  IID `5bb5ff1d-…`) and `wsldevicehost.dll.c` (broker-vs-HDV branch at `DeviceHost+0x70`).
- **Microsoft Learn (officially documented, verified):**
  `IVmFiovGuestMemoryFastNotification::RegisterDoorbell`, `IVmVirtualDeviceAccess::GetDevice`,
  `IVmFiovGuestMmioMappings`, `HdvProxyDeviceHost`, `HDV_PCI_BAR_SELECTOR`
  (learn.microsoft.com `/windows/win32/devnotes/` and `/virtualization/api/hcs/reference/hdv/`).

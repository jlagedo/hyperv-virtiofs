//! `OffloadVirtioFsDevice` — request-level parallelism for virtio-fs
//! (strategy A, `docs/perf-optimization.md`).
//!
//! Our own [`VirtioDevice`] replacing OpenVMM's `VirtioFsDevice`, whose worker
//! runs every blocking FUSE/host-FS call inline before popping the next
//! descriptor (`virtiofs/src/virtio.rs:236` — virtiofsd with
//! `--thread-pool-size=1`). We reuse the entire FUSE server + host-FS backend
//! (`fuse::Session` and `VirtioFs` are `Send + Sync`); only the queue/worker
//! glue is ours.
//!
//! **The worker model — single-owner queue.** `VirtioQueue`'s
//! `try_next`/`poll_kick`/`complete` all take `&mut self` and the used ring has
//! no internal locking, so the queue stays owned by one dispatcher task per
//! queue; pool threads never touch it. The dispatcher pops everything
//! available, fans each work item out to the [`WorkerPool`], and is the only
//! place `complete` is called — completions flow back over an mpsc channel
//! (unbounded is safe: a work item must be popped to be submitted, so inflight
//! is capped by the queue size). Completion order is whatever the pool
//! finishes first — out-of-order is spec-legal (`VIRTIO_F_IN_ORDER` is never
//! advertised) and both ring formats carry the work item's own id.
//!
//! **Adaptive inline.** The pool's thread-hop (~tens of µs per request) only
//! pays off when requests overlap. At queue depth 1 — every single-threaded
//! guest workload — it is pure added latency, so a lone request on an
//! otherwise-idle queue is dispatched inline on the dispatcher task (exactly
//! the upstream serial device's behaviour); the pool engages only when a
//! drained batch exceeds one or work is already in flight.
//!
//! **Lock/borrow rule (the correctness crux):** never hold a queue borrow
//! across an `.await`. The `poll_fn` closure borrows the queue only inside
//! each poll; `try_next` drains synchronously between polls.
//!
//! **Stop quiesces.** `stop_queue` must not capture `queue_state()` while
//! works are popped-but-uncompleted — those descriptors would be lost and the
//! guest's requests would hang after restore. The dispatcher therefore stops
//! popping on the stop signal, drains completions until `inflight == 0`, and
//! only then hands the queue back.

use crate::payload::{PayloadReader, PayloadReplySender};
use crate::pool::WorkerPool;
use anyhow::Context as _;
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt, StreamExt};
use guestmem::GuestMemory;
use inspect::InspectMut;
use pal_async::task::{Spawn, Task};
use pal_async::wait::PolledWait;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Poll;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use virtio::{
    DeviceTraits, DeviceTraitsSharedMemory, QueueResources, VirtioDevice, VirtioQueue,
    VirtioQueueCallbackWork,
};
use vmcore::vm_task::{VmTaskDriver, VmTaskDriverSource};

/// Virtio-fs config space: `tag: [u8; 36]` + `num_request_queues: u32` (LE),
/// prebuilt as bytes (mirrors upstream's `VirtioFsDeviceConfig` without the
/// zerocopy dep).
const TAG_LEN: usize = 36;
const CONFIG_LEN: usize = TAG_LEN + 4;

/// A started queue: the dispatcher task plus its stop signal. The task owns
/// the `VirtioQueue` and returns it (quiesced) when stopped.
struct QueueWorker {
    stop: oneshot::Sender<()>,
    task: Task<VirtioQueue>,
}

/// Virtio-fs device that dispatches FUSE requests on a blocking worker pool.
/// Mirrors `VirtioFsDevice`'s transport-facing behaviour (traits, config
/// space, queue setup); differs only in who runs the FUSE call.
pub struct OffloadVirtioFsDevice {
    task_name: Box<str>,
    driver: VmTaskDriver,
    config: [u8; CONFIG_LEN],
    session: Arc<fuse::Session>,
    pool: Arc<WorkerPool>,
    queues: Vec<Option<QueueWorker>>,
    notify_corruption: Arc<dyn Fn() + Sync + Send>,
}

impl InspectMut for OffloadVirtioFsDevice {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond().field("task_name", &*self.task_name);
    }
}

impl OffloadVirtioFsDevice {
    /// Mirror of `VirtioFsDevice::new`, plus the pool size. No DAX: the device
    /// advertises no shared memory and dispatches with `mapper = None`.
    pub fn new<Fs>(
        driver_source: &VmTaskDriverSource,
        tag: &str,
        fs: Fs,
        workers: usize,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
    ) -> Self
    where
        Fs: 'static + fuse::Fuse + Send + Sync,
    {
        let mut config = [0u8; CONFIG_LEN];
        // Copy the tag into the config space (truncate it for now if too long).
        let length = std::cmp::min(tag.len(), TAG_LEN);
        config[..length].copy_from_slice(&tag.as_bytes()[..length]);
        config[TAG_LEN..].copy_from_slice(&1u32.to_le_bytes()); // num_request_queues

        Self {
            task_name: format!("virtiofs-offload-{}", tag).into(),
            driver: driver_source.simple(),
            config,
            session: Arc::new(fuse::Session::new(fs)),
            pool: Arc::new(WorkerPool::new(workers)),
            queues: Vec::new(),
            notify_corruption: notify_corruption.unwrap_or_else(|| Arc::new(|| {})),
        }
    }
}

impl VirtioDevice for OffloadVirtioFsDevice {
    fn traits(&self) -> DeviceTraits {
        // Identical to upstream `VirtioFsDevice` so the guest sees the same
        // device (the dispatch model is invisible to it). Note ring_packed:
        // a modern Linux guest likely negotiates the packed ring; if e2e ever
        // shows used-ring stalls under parallel completion, dropping it (guest
        // falls back to split) is the first lever.
        DeviceTraits {
            device_id: virtio::spec::VirtioDeviceType::FS,
            device_features: VirtioDeviceFeatures::new()
                .with_ring_event_idx(true)
                .with_ring_indirect_desc(true)
                .with_ring_packed(true),
            max_queues: 2, // hiprio + one request queue
            device_register_length: CONFIG_LEN as u32,
            shared_memory: DeviceTraitsSharedMemory { id: 0, size: 0 },
        }
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        // Guest-reachable: bounds-check instead of slicing (never panic on
        // boundary data; a partial trailing read returns 0).
        let offset = offset as usize;
        self.config
            .get(offset..offset + 4)
            .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    async fn write_registers_u32(&mut self, offset: u16, val: u32) {
        // Guest-triggerable, so rate-limit (the config space is read-only).
        crate::ratelimit::warn_ratelimited!(offset, val, "unknown virtio-fs register write");
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        // Queue construction is exactly upstream's.
        let queue_event = PolledWait::new(&self.driver, resources.event)
            .context("failed to create polled wait")?;
        let queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory.clone(),
            resources.notify,
            queue_event,
            initial_state,
        )
        .context("failed to create virtio queue")?;

        let (stop_tx, stop_rx) = oneshot::channel();
        let dispatcher = Dispatcher {
            queue,
            mem: resources.guest_memory,
            session: self.session.clone(),
            pool: self.pool.clone(),
            notify_corruption: self.notify_corruption.clone(),
        };
        let task = self.driver.spawn(
            format!("{}-q{idx}", self.task_name),
            dispatcher.run(stop_rx),
        );

        let idx = idx as usize;
        if idx >= self.queues.len() {
            self.queues.resize_with(idx + 1, || None);
        }
        self.queues[idx] = Some(QueueWorker {
            stop: stop_tx,
            task,
        });
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        // Idempotent per the trait contract: a never-started or already-stopped
        // queue is an empty slot.
        let worker = self.queues.get_mut(idx as usize)?.take()?;
        let _ = worker.stop.send(());
        // The dispatcher quiesces (inflight == 0) before returning the queue,
        // so this state loses no descriptors.
        let queue = worker.task.await;
        Some(queue.queue_state())
    }

    async fn reset(&mut self) {
        // The transport stops all queues before reset, but be defensive: stop
        // any stragglers so no dispatcher outlives the session it dispatches to.
        for slot in &mut self.queues {
            if let Some(worker) = slot.take() {
                let _ = worker.stop.send(());
                let _ = worker.task.await;
            }
        }
        self.session.destroy();
    }
}

impl Drop for OffloadVirtioFsDevice {
    fn drop(&mut self) {
        // Device teardown is the "run is over" signal for the request-path
        // profile (cumulative counters; a no-op unless VIRTIO_HDV_REQ_STATS).
        crate::reqstats::global().emit_final();
    }
}

/// Per-queue dispatcher state; `run` consumes it and gives the queue back on
/// stop.
struct Dispatcher {
    queue: VirtioQueue,
    mem: GuestMemory,
    session: Arc<fuse::Session>,
    pool: Arc<WorkerPool>,
    notify_corruption: Arc<dyn Fn() + Sync + Send>,
}

impl Dispatcher {
    /// pop → pool → channel-back → complete, until stopped (then quiesce).
    async fn run(mut self, mut stop_rx: oneshot::Receiver<()>) -> VirtioQueue {
        // Request-path profiling (VIRTIO_HDV_REQ_STATS): every timer below is
        // `None` when disabled, so the production path never reads the clock.
        // The channel's third field is the job-end stamp for the done-hop stage.
        let stats = crate::reqstats::global();
        let (done_tx, mut done_rx) =
            mpsc::unbounded::<(VirtioQueueCallbackWork, u32, Option<std::time::Instant>)>();
        let mut inflight: usize = 0;
        // Reused drain buffer: popping the batch before dispatching is what
        // lets us see its size (the adaptive-inline test below).
        let mut batch: Vec<VirtioQueueCallbackWork> = Vec::new();
        // Stop requested: pop nothing more, drain completions to zero, return.
        let mut stopping = false;
        // Queue error (guest corruption): pop nothing more, but keep
        // completing in-flight work and stay responsive to stop. Mirrors
        // upstream, which parks the worker on queue errors until reset.
        let mut failed = false;
        loop {
            // 1. Drain everything currently available — synchronous, no await,
            //    no queue borrow held across a suspension point.
            if !stopping && !failed {
                let t_pop = stats.now();
                loop {
                    match self.queue.try_next() {
                        Ok(Some(work)) => batch.push(work),
                        Ok(None) => break,
                        Err(err) => {
                            tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "failed processing virtio queue"
                            );
                            failed = true;
                            break;
                        }
                    }
                }
                if !batch.is_empty() {
                    stats.rec_pop(t_pop, batch.len());
                }
            }

            // 2a. Adaptive inline: a lone request on an otherwise-idle queue
            //     (queue depth 1 — every single-threaded guest workload) runs
            //     right here on the dispatcher task, exactly like the upstream
            //     serial device. The pool's thread-hop only buys anything when
            //     there is a second request to overlap with; at QD=1 it is
            //     pure added latency (measured ~10% on 4k/metadata).
            if inflight == 0 && batch.len() == 1 {
                let work = batch.pop().expect("len == 1");
                // Same panic containment as the pool path: complete with 0
                // bytes rather than losing the descriptor.
                let t_dispatch = stats.now();
                let bytes = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    dispatch_one(&self.session, &self.mem, &work, &*self.notify_corruption)
                }))
                .unwrap_or_else(|_| {
                    crate::ratelimit::warn_ratelimited!(
                        "panic in FUSE dispatch (contained); completing 0 bytes"
                    );
                    0
                });
                stats.rec_inline(t_dispatch);
                let t_complete = stats.now();
                self.queue.complete(work, bytes);
                stats.rec_complete(t_complete);
                // Anything that arrived while we were blocked is picked up by
                // the next drain — and, being a batch > 1, goes to the pool.
                continue;
            }

            // 2b. Fan the batch out to the pool.
            for work in batch.drain(..) {
                inflight += 1;
                let mem = self.mem.clone();
                let session = self.session.clone();
                let notify = self.notify_corruption.clone();
                let done = done_tx.clone();
                let t_submit = stats.now();
                self.pool.submit(Box::new(move || {
                    stats.rec_pool_wait(t_submit);
                    // Contain panics *here* (not just in the pool) so `work`
                    // always flows back: a lost work item is a descriptor that
                    // never completes — a guest request that hangs forever.
                    let t_dispatch = stats.now();
                    let bytes = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        dispatch_one(&session, &mem, &work, &*notify)
                    }))
                    .unwrap_or_else(|_| {
                        crate::ratelimit::warn_ratelimited!(
                            "panic in FUSE dispatch (contained); completing 0 bytes"
                        );
                        0
                    });
                    stats.rec_pool(t_dispatch);
                    let _ = done.unbounded_send((work, bytes, stats.now()));
                }));
            }
            if stopping && inflight == 0 {
                return self.queue;
            }

            // 2. Park until a guest kick, a completion, or stop — whichever
            //    first. All wakers are registered in one poll pass.
            poll_fn(|cx| {
                let mut progressed = false;
                while let Poll::Ready(Some((work, bytes, t_done))) = done_rx.poll_next_unpin(cx) {
                    stats.rec_done_hop(t_done);
                    // Signals the MSI via the queue's notify interrupt.
                    let t_complete = stats.now();
                    self.queue.complete(work, bytes);
                    stats.rec_complete(t_complete);
                    inflight -= 1;
                    progressed = true;
                }
                // A oneshot must not be polled again once resolved.
                if !stopping && stop_rx.poll_unpin(cx).is_ready() {
                    stopping = true;
                    progressed = true;
                }
                if stopping || failed {
                    // No kick re-arm: nothing new will be popped. Wake only
                    // for completions (or the stop that just landed).
                    return if progressed {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    };
                }
                match self.queue.poll_kick(cx) {
                    Poll::Ready(()) => Poll::Ready(()),
                    Poll::Pending if progressed => Poll::Ready(()),
                    Poll::Pending => Poll::Pending, // both wakers registered
                }
            })
            .await;
        }
    }
}

/// One blocking FUSE round-trip on a pool thread: parse the request from the
/// descriptor chain, dispatch to the (thread-safe) session, write the reply
/// into guest memory. Mirrors upstream `process_virtiofs_request` with
/// `mapper = None` (no DAX). Returns the reply byte count for the used ring.
fn dispatch_one(
    session: &fuse::Session,
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    notify_corruption: &(dyn Fn() + Sync + Send),
) -> u32 {
    let reader = PayloadReader::new(mem, work);
    let request = match fuse::Request::new(reader) {
        Ok(request) => request,
        Err(e) => {
            // Guest-triggerable (malformed FUSE header), so rate-limit. There
            // is no error reply either — the unique ID is unknown.
            crate::ratelimit::warn_ratelimited!(
                error = &e as &dyn std::error::Error,
                "invalid FUSE message"
            );
            notify_corruption();
            return 0;
        }
    };

    let mut sender = PayloadReplySender::new(mem, work);
    session.dispatch(request, &mut sender, None);
    sender.bytes_written
}

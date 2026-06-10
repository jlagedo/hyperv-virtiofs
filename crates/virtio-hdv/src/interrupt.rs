//! MSI delivery: route OpenVMM's `SignalMsi` to `HdvDeliverGuestInterrupt`.
//!
//! When the guest's virtio driver gets a used-ring completion, `VirtioPciDevice`
//! signals its MSI-X target with the `(address, data)` pair the guest programmed
//! into the MSI-X table. We forward that straight to HDV, which injects the
//! interrupt into the partition.
//!
//! We also record every distinct `(address, data)` we deliver into a shared set,
//! which the [`crate`]'s interrupt re-arm safety net periodically re-delivers —
//! see the note in `lib.rs` (it covers a missed-interrupt window caused by the
//! copy/snapshot semantics of HDV guest-memory apertures).

use crate::handle::DeviceHandle;
use pci_core::msi::SignalMsi;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// What the MSI path shares with the re-arm net: the distinct `(address, data)`
/// pairs delivered so far, plus an **activity stamp** the net uses to gate its
/// tick (tight while completions flow, backed off when the device is quiet).
pub struct InterruptLog {
    /// Distinct MSI `(address, data)` pairs delivered so far.
    pairs: Mutex<Vec<(u64, u32)>>,
    /// Clock base for `last_activity_ms` (ms-since-start fits an atomic).
    start: Instant,
    /// ms-since-`start` of the most recent delivery — the activity signal.
    last_activity_ms: AtomicU64,
}

/// Shared handle to the [`InterruptLog`].
pub type SeenInterrupts = Arc<InterruptLog>;

impl InterruptLog {
    pub fn new() -> SeenInterrupts {
        Arc::new(Self {
            pairs: Mutex::new(Vec::new()),
            start: Instant::now(),
            last_activity_ms: AtomicU64::new(0),
        })
    }

    /// Record a delivery: remember a never-seen pair, stamp activity.
    fn record(&self, address: u64, data: u32) {
        {
            // Poison-tolerant: MSI delivery runs un-guarded; a poisoned lock
            // (panic caught at a guarded boundary) must not unwind it. The set
            // is structurally valid.
            let mut pairs = self.pairs.lock().unwrap_or_else(|e| e.into_inner());
            if !pairs.contains(&(address, data)) {
                pairs.push((address, data));
            }
        }
        self.last_activity_ms
            .store(self.start.elapsed().as_millis() as u64, Relaxed);
    }

    /// Copy the pairs into `buf`, reusing its capacity — the re-arm net's
    /// per-tick read, kept allocation-free (it used to clone the Vec every
    /// 5 ms).
    pub fn snapshot_into(&self, buf: &mut Vec<(u64, u32)>) {
        buf.clear();
        let pairs = self.pairs.lock().unwrap_or_else(|e| e.into_inner());
        buf.extend_from_slice(&pairs);
    }

    /// Milliseconds since the last [`record`](Self::record) (u64::MAX-ish if
    /// nothing was ever delivered — reads as "long idle", which is right).
    pub fn idle_ms(&self) -> u64 {
        (self.start.elapsed().as_millis() as u64)
            .saturating_sub(self.last_activity_ms.load(Relaxed))
    }
}

/// A `SignalMsi` target that injects via the late-bound HDV device.
pub struct HdvSignalMsi {
    handle: DeviceHandle,
    seen: SeenInterrupts,
}

impl HdvSignalMsi {
    pub fn new(handle: DeviceHandle, seen: SeenInterrupts) -> Self {
        Self { handle, seen }
    }
}

impl SignalMsi for HdvSignalMsi {
    fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
        // `devid` (requester id) is HDV's concern, not ours — it owns the BDF.
        // Drop the interrupt if the device handle isn't bound yet (can't happen
        // once the guest is running, since binding precedes Start).
        if let Some(device) = self.handle.get() {
            self.seen.record(address, data);
            // A failed injection is logged-and-dropped: there is no caller to
            // propagate to (this is a fire-and-forget notification path).
            let stats = crate::reqstats::global();
            let t_deliver = stats.now();
            let r = device.deliver_interrupt(address, data);
            stats.rec_deliver(t_deliver);
            tracing::trace!("deliver_interrupt addr={address:#x} data={data:#x} -> {r:?}");
        } else {
            tracing::trace!(
                "deliver_interrupt addr={address:#x} data={data:#x} DROPPED (no handle)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `record` dedups pairs and stamps activity; `snapshot_into` reuses the
    /// caller's buffer.
    #[test]
    fn log_dedups_and_stamps() {
        let log = InterruptLog::new();
        log.record(0xfee0_0000, 1);
        log.record(0xfee0_0000, 1);
        log.record(0xfee0_0000, 2);
        let mut buf = vec![(0u64, 0u32); 8]; // pre-dirtied: snapshot must clear
        log.snapshot_into(&mut buf);
        assert_eq!(buf, vec![(0xfee0_0000, 1), (0xfee0_0000, 2)]);
        // Just recorded: idle time is ~0 (allow scheduler slop).
        assert!(log.idle_ms() < 1000);
    }
}

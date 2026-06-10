//! Guest→host queue kicks via HDV **doorbells** (`HdvRegisterDoorbell`).
//!
//! Without this, every queue notify is a BAR0 MMIO write the platform delivers
//! to our user-mode `WriteDeviceMemory` callback — a guest exit plus a
//! cross-process HDV round trip *per request*. The request-path profile
//! (`VIRTIO_HDV_REQ_STATS`, see `docs/perf-optimization.md`) measured all
//! host-side dispatch stages at ~30 µs of a ~95 µs QD-1 op: most of the
//! remainder rides the kick/wake path this module attacks. A doorbell is the
//! platform's purpose-built alternative: the VID consumes the guest's notify
//! write **in kernel** and signals our queue event directly.
//!
//! OpenVMM's transport already does the rest: when the guest sets DRIVER_OK,
//! `install_doorbells` registers one doorbell per queue at the BAR0 notify
//! offset with trigger value = queue index, against the same per-queue `Event`
//! the MMIO notify handler signals. We implement its [`DoorbellRegistration`]
//! seam with **two routes**, tried in order:
//!
//! 1. `HdvRegisterDoorbell` — in-process, but **denied by design**
//!    (`E_ACCESSDENIED`) on the `ExternalRestricted` device hosts the product
//!    uses. Kept first because it is free where HDV grants it.
//! 2. The **VM worker channel** ([`hdv::vmworker`]): the owner of the compute
//!    system resolves the FlexibleIov device inside the worker process and
//!    registers the doorbell there — the same route WSL's broker takes. Needs
//!    the VM **runtime id**, threaded in from the host open.
//!
//! A failed registration is reported (`Err`) and the transport simply keeps
//! the MMIO-intercept path — same event, just slower — so this degrades
//! gracefully wherever both routes reject us.
//!
//! `VIRTIO_HDV_DOORBELL=0` disables registration outright (the A/B switch for
//! benchmarking the intercept path).
//!
//! [`DoorbellRegistration`]: guestmem::DoorbellRegistration

use crate::handle::DeviceHandle;
use guestmem::DoorbellRegistration;
use hdv::vmworker::FiovDoorbells;
use hdv_sys::{
    HDV_DOORBELL_FLAG_TRIGGER_ANY_VALUE, HDV_DOORBELL_FLAG_TRIGGER_SIZE_ANY,
    HDV_DOORBELL_FLAG_TRIGGER_SIZE_BYTE, HDV_DOORBELL_FLAG_TRIGGER_SIZE_DWORD,
    HDV_DOORBELL_FLAG_TRIGGER_SIZE_QWORD, HDV_DOORBELL_FLAG_TRIGGER_SIZE_WORD,
    HDV_PCI_BAR_SELECTOR,
};
use pal_event::Event;
use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle};
use std::sync::{Arc, Mutex};

/// Whether doorbell registration is enabled (`VIRTIO_HDV_DOORBELL`, default
/// **on**; `0`/`false`/`off` selects the MMIO-intercept kick path), read once.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            std::env::var("VIRTIO_HDV_DOORBELL").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// [`DoorbellRegistration`] backed by `HdvRegisterDoorbell` on the late-bound
/// HDV device, with the VM-worker channel as fallback. Built before the device
/// exists (like the memory and MSI seams); registration only ever happens at
/// guest DRIVER_OK, long after bind — and after the FlexibleIov hot-add, which
/// is what makes the worker's `GetDevice` resolvable.
pub struct HdvDoorbells {
    handle: DeviceHandle,
    worker: Option<WorkerLink>,
}

/// Everything needed to reach the device through the VM worker process,
/// connected lazily on the first registration that needs it. The connection is
/// cached for the device's lifetime: once the device is being removed the
/// worker can no longer resolve it, so unregistration must go through the
/// stored interface (each guard holds an `Arc` to it).
struct WorkerLink {
    runtime_id: hdv::GUID,
    instance_id: hdv::GUID,
    conn: Mutex<Option<Arc<FiovDoorbells>>>,
}

impl WorkerLink {
    /// The cached worker connection, connecting on first use. A failed connect
    /// is retried on the next registration (the worker may not be reachable
    /// yet); rate-limited warn on failure.
    fn connect(&self) -> Option<Arc<FiovDoorbells>> {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if conn.is_none() {
            match FiovDoorbells::connect(&self.runtime_id, &self.instance_id) {
                Ok(c) => *conn = Some(Arc::new(c)),
                Err(e) => {
                    crate::ratelimit::warn_ratelimited!(
                        error = &e as &dyn std::error::Error,
                        "VM worker doorbell channel connect failed"
                    );
                }
            }
        }
        conn.clone()
    }
}

impl HdvDoorbells {
    /// `worker` is `Some((runtime_id, instance_id))` when the caller owns the
    /// compute system and wants the VM-worker fallback route.
    pub fn new(handle: DeviceHandle, worker: Option<(hdv::GUID, hdv::GUID)>) -> Self {
        Self {
            handle,
            worker: worker.map(|(runtime_id, instance_id)| WorkerLink {
                runtime_id,
                instance_id,
                conn: Mutex::new(None),
            }),
        }
    }
}

/// Map the transport's internal doorbell GPA back to HDV's `(bar, offset)`.
/// The transport computes it from the *internal* BAR bases we programmed into
/// the config emulator (`program_internal_bars`), so the mapping is exact —
/// HDV re-translates the selector to wherever the VID placed the guest-facing
/// BAR.
fn bar_and_offset(guest_address: u64) -> Option<(HDV_PCI_BAR_SELECTOR, u64)> {
    const BAR_SPAN: u64 = 0x1000;
    if (crate::BAR0_BASE..crate::BAR0_BASE + BAR_SPAN).contains(&guest_address) {
        Some((HDV_PCI_BAR_SELECTOR::Bar0, guest_address - crate::BAR0_BASE))
    } else if (crate::BAR2_BASE..crate::BAR2_BASE + BAR_SPAN).contains(&guest_address) {
        Some((HDV_PCI_BAR_SELECTOR::Bar2, guest_address - crate::BAR2_BASE))
    } else {
        None
    }
}

/// The write-size trigger flag for a `length`-byte doorbell write.
fn size_flag(length: Option<u32>) -> u64 {
    match length {
        Some(1) => HDV_DOORBELL_FLAG_TRIGGER_SIZE_BYTE,
        Some(2) => HDV_DOORBELL_FLAG_TRIGGER_SIZE_WORD,
        Some(4) => HDV_DOORBELL_FLAG_TRIGGER_SIZE_DWORD,
        Some(8) => HDV_DOORBELL_FLAG_TRIGGER_SIZE_QWORD,
        _ => HDV_DOORBELL_FLAG_TRIGGER_SIZE_ANY,
    }
}

/// Which channel a registration went through — unregistration must take the
/// same one.
enum Route {
    /// `HdvRegisterDoorbell` on the late-bound device handle.
    Hdv(DeviceHandle),
    /// The VM worker channel; the `Arc` keeps the worker interface alive past
    /// device removal (it cannot be re-fetched then).
    Worker(Arc<FiovDoorbells>),
}

/// A live registration; unregisters on drop. Holds a duplicated `Event`
/// handle so the HANDLE given to the platform stays valid for the
/// registration's lifetime (the `register_doorbell` safety contract).
struct DoorbellGuard {
    route: Route,
    bar: HDV_PCI_BAR_SELECTOR,
    offset: u64,
    trigger_value: u64,
    flags: u64,
    _event: Event,
}

impl Drop for DoorbellGuard {
    fn drop(&mut self) {
        // Guards drop at device reset (device alive) or transport teardown
        // (inside HDV's Teardown callback — the same handle-validity window
        // the MSI path already relies on). Failure is log-and-drop: there is
        // nothing to propagate to from a drop. Guest-triggerable cadence
        // (guards drop on device reset), so rate-limit.
        let result = match &self.route {
            Route::Hdv(handle) => {
                let Some(device) = handle.get() else { return };
                device.unregister_doorbell(self.bar, self.offset, self.trigger_value, self.flags)
            }
            Route::Worker(conn) => {
                conn.unregister_doorbell(self.bar, self.offset, self.trigger_value, self.flags)
            }
        };
        if let Err(e) = result {
            crate::ratelimit::warn_ratelimited!(
                error = &e as &dyn std::error::Error,
                offset = self.offset,
                "doorbell unregister failed"
            );
        }
    }
}

impl DoorbellRegistration for HdvDoorbells {
    fn register_doorbell(
        &self,
        guest_address: u64,
        value: Option<u64>,
        length: Option<u32>,
        event: &Event,
    ) -> io::Result<Box<dyn Send + Sync>> {
        if !enabled() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "doorbells disabled (VIRTIO_HDV_DOORBELL=0)",
            ));
        }
        let Some((bar, offset)) = bar_and_offset(guest_address) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "doorbell address outside the internal BAR windows",
            ));
        };
        let device = self.handle.get().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "HDV device not bound yet")
        })?;

        let mut flags = size_flag(length);
        if value.is_none() {
            flags |= HDV_DOORBELL_FLAG_TRIGGER_ANY_VALUE;
        }
        let trigger_value = value.unwrap_or(0);

        // Duplicate the event handle; the guard keeps it alive until unregister.
        let event = event.clone();
        let raw_event = event.as_handle().as_raw_handle();

        // Route 1: HdvRegisterDoorbell — free where HDV grants it, denied by
        // design (E_ACCESSDENIED) on the restricted hosts the product uses.
        //
        // SAFETY: `event` is a valid Win32 event handle, owned by the guard
        // below, which unregisters before releasing it.
        let route = match unsafe {
            device.register_doorbell(bar, offset, trigger_value, flags, raw_event)
        } {
            Ok(()) => {
                tracing::debug!(offset, trigger_value, "HDV doorbell registered");
                Route::Hdv(self.handle.clone())
            }
            Err(hdv_err) => {
                // Route 2: the VM worker channel (the WSL-broker route; see
                // docs/perf-optimization.md "The VM-worker channel").
                let conn = self.worker.as_ref().and_then(|w| w.connect());
                match conn {
                    // SAFETY: same event-lifetime contract as above; the COM
                    // proxy duplicates the handle into the worker process.
                    Some(conn) => match unsafe {
                        conn.register_doorbell(bar, offset, trigger_value, flags, raw_event)
                    } {
                        Ok(()) => {
                            tracing::info!(
                                offset,
                                trigger_value,
                                "doorbell registered via the VM worker channel"
                            );
                            Route::Worker(conn)
                        }
                        Err(e) => {
                            // Both routes refused — the transport keeps the
                            // MMIO-intercept kick. Say so, because the perf
                            // difference matters. Rate-limited: registration
                            // re-runs on every guest DRIVER_OK transition.
                            crate::ratelimit::warn_ratelimited!(
                                hdv_error = &hdv_err as &dyn std::error::Error,
                                worker_error = &e as &dyn std::error::Error,
                                offset,
                                trigger_value,
                                "doorbell registration failed on both routes; queue kicks stay on the MMIO intercept"
                            );
                            return Err(io::Error::other(e));
                        }
                    },
                    None => {
                        crate::ratelimit::warn_ratelimited!(
                            error = &hdv_err as &dyn std::error::Error,
                            offset,
                            trigger_value,
                            "HdvRegisterDoorbell failed and no VM worker route; queue kicks stay on the MMIO intercept"
                        );
                        return Err(io::Error::other(hdv_err));
                    }
                }
            }
        };

        Ok(Box::new(DoorbellGuard {
            route,
            bar,
            offset,
            trigger_value,
            flags,
            _event: event,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Internal-BAR GPA → (selector, offset) translation, including rejection
    /// outside the windows.
    #[test]
    fn bar_translation() {
        assert!(matches!(
            bar_and_offset(crate::BAR0_BASE + 0x38),
            Some((HDV_PCI_BAR_SELECTOR::Bar0, 0x38))
        ));
        assert!(matches!(
            bar_and_offset(crate::BAR2_BASE),
            Some((HDV_PCI_BAR_SELECTOR::Bar2, 0))
        ));
        assert!(bar_and_offset(crate::BAR0_BASE - 1).is_none());
        assert!(bar_and_offset(crate::BAR2_BASE + 0x1000).is_none());
    }

    /// Write-size flag mapping (unknown sizes degrade to ANY).
    #[test]
    fn size_flags() {
        assert_eq!(size_flag(Some(2)), HDV_DOORBELL_FLAG_TRIGGER_SIZE_WORD);
        assert_eq!(size_flag(Some(8)), HDV_DOORBELL_FLAG_TRIGGER_SIZE_QWORD);
        assert_eq!(size_flag(None), HDV_DOORBELL_FLAG_TRIGGER_SIZE_ANY);
        assert_eq!(size_flag(Some(3)), HDV_DOORBELL_FLAG_TRIGGER_SIZE_ANY);
    }
}

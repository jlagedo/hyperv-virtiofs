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
//! seam on HDV. A failed registration is reported (`Err`) and the transport
//! simply keeps the MMIO-intercept path — same event, just slower — so this
//! degrades gracefully on platforms where `HdvRegisterDoorbell` rejects us.
//!
//! `VIRTIO_HDV_DOORBELL=0` disables registration outright (the A/B switch for
//! benchmarking the intercept path).
//!
//! [`DoorbellRegistration`]: guestmem::DoorbellRegistration

use crate::handle::DeviceHandle;
use guestmem::DoorbellRegistration;
use hdv_sys::{
    HDV_DOORBELL_FLAG_TRIGGER_ANY_VALUE, HDV_DOORBELL_FLAG_TRIGGER_SIZE_ANY,
    HDV_DOORBELL_FLAG_TRIGGER_SIZE_BYTE, HDV_DOORBELL_FLAG_TRIGGER_SIZE_DWORD,
    HDV_DOORBELL_FLAG_TRIGGER_SIZE_QWORD, HDV_DOORBELL_FLAG_TRIGGER_SIZE_WORD,
    HDV_PCI_BAR_SELECTOR,
};
use pal_event::Event;
use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle};

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
/// HDV device. Built before the device exists (like the memory and MSI seams);
/// registration only ever happens at guest DRIVER_OK, long after bind.
pub struct HdvDoorbells {
    handle: DeviceHandle,
}

impl HdvDoorbells {
    pub fn new(handle: DeviceHandle) -> Self {
        Self { handle }
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

/// A live registration; unregisters on drop. Holds a duplicated `Event`
/// handle so the HANDLE given to HDV stays valid for the registration's
/// lifetime (the `register_doorbell` safety contract).
struct DoorbellGuard {
    handle: DeviceHandle,
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
        // nothing to propagate to from a drop.
        if let Some(device) = self.handle.get() {
            if let Err(e) =
                device.unregister_doorbell(self.bar, self.offset, self.trigger_value, self.flags)
            {
                // Guest-triggerable cadence (guards drop on device reset), so
                // rate-limit.
                crate::ratelimit::warn_ratelimited!(
                    error = &e as &dyn std::error::Error,
                    offset = self.offset,
                    "HdvUnregisterDoorbell failed"
                );
            }
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
        // SAFETY: `event` is a valid Win32 event handle, owned by the guard
        // below, which unregisters before releasing it.
        unsafe {
            device.register_doorbell(
                bar,
                offset,
                trigger_value,
                flags,
                event.as_handle().as_raw_handle(),
            )
        }
        .map_err(|e| {
            // Registration failing is the graceful-degradation path (the
            // transport keeps the MMIO-intercept kick) — but say so, because
            // the perf difference matters. Rate-limited: registration re-runs
            // on every guest DRIVER_OK transition. Known case: ExternalRestricted
            // device hosts get E_ACCESSDENIED — HDV reserves doorbells for the
            // VM-owner side (WSL routes them through its privileged broker via
            // IVmFiovGuestMemoryFastNotification, see DeviceHostProxy.cpp).
            crate::ratelimit::warn_ratelimited!(
                error = &e as &dyn std::error::Error,
                offset,
                trigger_value,
                "HdvRegisterDoorbell failed; queue kicks stay on the MMIO intercept"
            );
            io::Error::other(e)
        })?;

        tracing::debug!(offset, trigger_value, "HDV doorbell registered");
        Ok(Box::new(DoorbellGuard {
            handle: self.handle.clone(),
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

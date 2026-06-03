//! OpenVMM's virtio transport, carried over **HDV** instead of OpenVMM's own
//! PCI/VPCI stack — the open counterpart to WSL's closed `wsldevicehost.dll`.
//!
//! [`VirtioHdvDevice::attach`] stands up a virtio-fs device on an HDV device
//! host and presents it to a stock HCS guest. The flow:
//!
//! 1. Build OpenVMM's public [`VirtioPciDevice`] fronting a [`VirtioFsDevice`]
//!    (which fronts a host directory via [`VirtioFs`]).
//! 2. Back its three host seams with HDV:
//!    - guest memory (DMA) ← `HdvCreateGuestMemoryAperture` ([`mem`]),
//!    - MSI-X delivery ← `HdvDeliverGuestInterrupt` ([`interrupt`]),
//!    - the PCI config space + BAR MMIO ← HDV's device-vtable callbacks, via the
//!      generic [`hdv::pci::PciOps`] this crate implements below.
//! 3. Create the device on the host through the proven [`hdv::pci`] /
//!    [`hdv::proxy`] path and publish the HDV device handle (see [`handle`]).
//!
//! **Scope (milestone 2, first proof):** no DAX — `shmem_size = 0`, so there is
//! no shared-memory BAR and no `shared_mem_mapper` / `HdvCreateSectionBackedMmioRange`
//! on the critical path. Notify rides the BAR0 MMIO intercept (no doorbell yet).
//! Both are deliberate simplifications, documented inline.
//!
//! [`VirtioPciDevice`]: virtio::VirtioPciDevice
//! [`VirtioFsDevice`]: virtiofs::virtio::VirtioFsDevice
//! [`VirtioFs`]: virtiofs::VirtioFs

mod handle;
mod interrupt;
mod mem;
mod ratelimit;

pub use handle::DeviceHandle;

use chipset_device::io::IoResult;
use chipset_device::mmio::{ExternallyManagedMmioIntercepts, MmioIntercept};
use chipset_device::pci::PciConfigSpace;
use chipset_device::poll_device::PollDevice;
use futures::executor::block_on;
use hdv::pci::{PciDetails, PciDevice, PciOps};
use hdv::DeviceHost;
use interrupt::{HdvSignalMsi, SeenInterrupts};
use mem::HdvApertureMem;
use pal_async::task::{Spawn, Task};
use pal_async::{DefaultDriver, DefaultPool};
use pci_core::bus_range::AssignedBusRange;
use pci_core::msi::MsiConnection;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::thread::JoinHandle;
use std::time::Duration;
use virtio::{PciInterruptModel, VirtioPciDevice};
use virtiofs::virtio::VirtioFsDevice;
use virtiofs::VirtioFs;
use vmcore::vm_task::{SingleDriverBackend, VmTaskDriverSource};

/// Building or attaching the virtio-fs HDV device failed.
#[derive(Debug)]
pub enum AttachError {
    /// Opening the host directory for the FUSE backend failed.
    Fs(String),
    /// Building the OpenVMM virtio PCI transport failed.
    Transport(std::io::Error),
    /// An HDV call (device-instance creation) failed.
    Hdv(hdv::Error),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::Fs(e) => write!(f, "virtio-fs backend: {e}"),
            AttachError::Transport(e) => write!(f, "virtio PCI transport: {e}"),
            AttachError::Hdv(e) => write!(f, "HDV: {e}"),
        }
    }
}
impl std::error::Error for AttachError {}

/// A live virtio-fs device attached to a guest over HDV. Holds the HDV device
/// (whose drop tears it down) plus, inside its `PciOps` context, every object
/// that must outlive it (the async pool, the device, the poll pump).
pub struct VirtioHdvDevice {
    // Field order = drop order: stop the re-arm net first (so it can't inject into
    // a half-torn-down device), then tear down the device.
    _rearm: RearmNet,
    // `PciDevice`'s Drop tears down the HDV device host → `Teardown` → drops the
    // boxed `Ops` (and all its keepalives).
    pci: PciDevice,
}

/// Background interrupt re-arm net. Periodically re-delivers every MSI the device
/// has signalled, so a completion interrupt lost to the copy/snapshot semantics of
/// HDV apertures (the device can briefly read a stale `used_event` and suppress the
/// real interrupt) is recovered within the tick. Spurious MSIs are harmless: a
/// virtio guest just re-scans the used ring and finds nothing new. This is a
/// safety net for the proof; the principled fix is a coherent, eviction-managed
/// guest-memory mapping (as WSL's `HdvGuestMemoryEvictionWorker` implies).
struct RearmNet {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RearmNet {
    /// Spawn the net. `handle` must already be bound (post-create).
    fn spawn(handle: DeviceHandle, seen: SeenInterrupts) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("virtio-hdv-rearm".into())
            .spawn(move || {
                while !stop2.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                    let Some(device) = handle.get() else { continue };
                    let pairs = seen.lock().unwrap().clone();
                    for (address, data) in pairs {
                        let _ = device.deliver_interrupt(address, data);
                    }
                }
            })
            .expect("spawn rearm thread");
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for RearmNet {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl VirtioHdvDevice {
    /// Attach a virtio-fs share of `workspace` (host directory) to the guest
    /// behind `host`, advertised under `tag`, with the well-known device instance
    /// id. `guest_mem_size` is the guest RAM size — the upper bound on GPAs the
    /// virtqueues may reference.
    pub fn attach(
        host: DeviceHost,
        workspace: &Path,
        tag: &str,
        guest_mem_size: u64,
    ) -> Result<Self, AttachError> {
        Self::attach_with_instance(
            host,
            workspace,
            tag,
            guest_mem_size,
            &hdv::pci::HVFS_DEVICE_INSTANCE_ID,
        )
    }

    /// Like [`attach`](Self::attach) but with a caller-chosen `DeviceInstanceId`,
    /// on its own (owned) device host.
    pub fn attach_with_instance(
        host: DeviceHost,
        workspace: &Path,
        tag: &str,
        guest_mem_size: u64,
        instance_id: &hdv::GUID,
    ) -> Result<Self, AttachError> {
        Self::attach_shared(
            Arc::new(host),
            workspace,
            tag,
            guest_mem_size,
            &hdv::pci::HVFS_DEVICE_CLASS_ID,
            instance_id,
        )
    }

    /// Attach a virtio-fs device on a **shared** device host, so several shares can
    /// coexist in one guest (HDV permits only one device host per VM — see the
    /// hotplug spike). Each concurrent device needs a **distinct `class_id` +
    /// `instance_id`** and a matching `FlexibleIov` slot (a second slot reusing an
    /// `EmulatorId` is rejected with `ERROR_HV_INVALID_PARAMETER`). The caller keeps
    /// the `Arc<DeviceHost>` and passes a clone per device.
    pub fn attach_shared(
        host: Arc<DeviceHost>,
        workspace: &Path,
        tag: &str,
        guest_mem_size: u64,
        class_id: &hdv::GUID,
        instance_id: &hdv::GUID,
    ) -> Result<Self, AttachError> {
        // A background IOCP pool drives the device's async tasks (queue workers,
        // deferred config reads) for its whole lifetime.
        let (pool_thread, driver) = DefaultPool::spawn_on_thread("virtio-hdv");

        // The HDV device handle is bound after create; the memory + MSI seams
        // read it from this shared cell.
        let handle = DeviceHandle::new();
        let guest_memory = HdvApertureMem::into_guest_memory(handle.clone(), guest_mem_size);

        let seen: SeenInterrupts = Arc::new(Mutex::new(Vec::new()));
        let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);
        msi_conn.connect(Arc::new(HdvSignalMsi::new(handle.clone(), seen.clone())));

        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));

        let fs = VirtioFs::new(workspace, None).map_err(|e| AttachError::Fs(e.to_string()))?;
        // shmem_size = 0: no DAX window, hence no BAR4 / shared_mem_mapper.
        let fs_device = VirtioFsDevice::new(&driver_source, tag, fs, 0, None);

        let mut pci_dev = VirtioPciDevice::new(
            Box::new(fs_device),
            &driver,
            guest_memory,
            PciInterruptModel::Msix(msi_conn.target()),
            None,                                 // no doorbell: notify via BAR0 MMIO intercept
            &mut ExternallyManagedMmioIntercepts, // HDV owns BAR placement; we route by (bar,offset)
            None,                                 // no shared-memory mapper (shmem_size == 0)
        )
        .map_err(AttachError::Transport)?;

        // HDV (the VMBus VPCI VID) owns guest-facing BAR placement: it sizes BARs
        // from `GetDetails`, assigns them out-of-band over VMBus, and delivers MMIO
        // pre-decoded as `(bar, offset)`. The guest never reads or writes our config
        // BAR registers (confirmed on the rig) — so they're invisible to it. We give
        // the emulator its own internal BAR bases purely so `find_bar` can resolve
        // the address we rebuild from HDV's `(bar, offset)`. We do NOT touch the
        // command register — the guest enables memory space itself, which fires
        // `update_mmio_enabled` against these bases. (Pre-enabling bus master here
        // makes the VID reject the device before enumeration.)
        program_internal_bars(&mut pci_dev);

        let dev = Arc::new(Mutex::new(pci_dev));

        // Pump the device's poll function: each `poll_device` call re-arms its
        // waker, so this future re-runs whenever the device has async work —
        // advancing the queue state machine and replaying stalled MMIO (which
        // completes the deferred reads our `read_bar` blocks on). It holds the
        // device lock only across each poll, never while parked.
        let poll_dev = dev.clone();
        let poll_task = driver.spawn("virtio-hdv-poll", async move {
            std::future::poll_fn(|cx| {
                poll_dev.lock().unwrap().poll_device(cx);
                Poll::<()>::Pending
            })
            .await
        });

        let ops = Ops {
            dev,
            _pool_thread: pool_thread,
            _driver: driver,
            _msi_conn: msi_conn,
            _driver_source: driver_source,
            _poll_task: poll_task,
        };

        let pci = PciDevice::create_shared(host, Box::new(ops), class_id, instance_id)
            .map_err(AttachError::Hdv)?;
        // Now the guest-memory + MSI seams can reach the device.
        handle.set(pci.device());

        // Start the interrupt re-arm net only after the handle is bound.
        let rearm = RearmNet::spawn(handle, seen);

        Ok(Self { _rearm: rearm, pci })
    }

    /// The underlying HDV device handle.
    pub fn device(&self) -> hdv::Device {
        self.pci.device()
    }
}

/// The [`PciOps`] HDV drives, bridging its PCI vtable to OpenVMM's
/// [`VirtioPciDevice`]. Also the home of every object that must live as long as
/// the device (HDV owns this box until `Teardown`).
struct Ops {
    dev: Arc<Mutex<VirtioPciDevice>>,
    // Keepalives — dropped together when HDV tears the device down.
    _pool_thread: JoinHandle<()>,
    _driver: DefaultDriver,
    _msi_conn: MsiConnection,
    _driver_source: VmTaskDriverSource,
    _poll_task: Task<()>,
}

impl PciOps for Ops {
    fn details(&self) -> PciDetails {
        let mut dev = self.dev.lock().unwrap();
        let id = cfg_read(&mut dev, 0x00); // device<<16 | vendor
        let class = cfg_read(&mut dev, 0x08); // class(24) | revision
        let sub = cfg_read(&mut dev, 0x2c); // subsystem<<16 | subvendor

        // Standard PCI BAR sizing: write all-ones, read back the size mask,
        // restore. HDV uses these to allocate the BARs' guest address space.
        let mut probed = [0u32; 6];
        for (i, slot) in probed.iter_mut().enumerate() {
            let off = 0x10 + (i as u16) * 4;
            let orig = cfg_read(&mut dev, off);
            cfg_write(&mut dev, off, 0xFFFF_FFFF);
            *slot = cfg_read(&mut dev, off);
            cfg_write(&mut dev, off, orig);
        }

        PciDetails {
            vendor_id: id as u16,
            device_id: (id >> 16) as u16,
            revision_id: class as u8,
            prog_if: (class >> 8) as u8,
            sub_class: (class >> 16) as u8,
            base_class: (class >> 24) as u8,
            sub_vendor_id: sub as u16,
            sub_system_id: (sub >> 16) as u16,
            probed_bars: probed,
        }
    }

    fn read_config(&self, offset: u32) -> u32 {
        let v = cfg_read(&mut self.dev.lock().unwrap(), offset as u16);
        tracing::trace!("read_config off={offset:#x} -> {v:#010x}");
        v
    }

    fn write_config(&self, offset: u32, value: u32) {
        tracing::trace!("write_config off={offset:#x} val={value:#010x}");
        cfg_write(&mut self.dev.lock().unwrap(), offset as u16, value);
    }

    fn read_bar(&self, bar: u8, offset: u64, data: &mut [u8]) {
        let mut dev = self.dev.lock().unwrap();
        let Some(base) = bar_base(&mut dev, bar) else {
            tracing::trace!(
                "read_bar bar={bar} off={offset:#x} len={} -> NO BAR BASE",
                data.len()
            );
            data.fill(0xff);
            return;
        };
        tracing::trace!(
            "read_bar bar={bar} off={offset:#x} len={} addr={:#x}",
            data.len(),
            base + offset
        );
        match dev.mmio_read(base + offset, data) {
            IoResult::Ok => tracing::trace!("  -> ok {data:02x?}"),
            IoResult::Err(e) => {
                tracing::trace!("  -> err {e:?}");
                data.fill(0xff);
            }
            IoResult::Defer(token) => {
                // Release the lock so the poll pump can complete this read.
                drop(dev);
                tracing::trace!("  -> defer, blocking…");
                if block_on(token.read_future(data)).is_err() {
                    data.fill(0xff);
                }
                tracing::trace!("  -> defer done {data:02x?}");
            }
        }
    }

    fn write_bar(&self, bar: u8, offset: u64, data: &[u8]) {
        let mut dev = self.dev.lock().unwrap();
        let Some(base) = bar_base(&mut dev, bar) else {
            tracing::trace!("write_bar bar={bar} off={offset:#x} -> NO BAR BASE");
            return;
        };
        tracing::trace!(
            "write_bar bar={bar} off={offset:#x} addr={:#x} {data:02x?}",
            base + offset
        );
        match dev.mmio_write(base + offset, data) {
            IoResult::Ok | IoResult::Err(_) => {}
            IoResult::Defer(token) => {
                drop(dev);
                tracing::trace!("  -> defer, blocking…");
                let _ = block_on(token.write_future());
                tracing::trace!("  -> defer done");
            }
        }
    }

    fn start(&self) -> bool {
        true
    }
}

/// Read one config dword (treat a deferred/failed read as 0, the PCI convention).
fn cfg_read(dev: &mut VirtioPciDevice, off: u16) -> u32 {
    let mut v = 0u32;
    if let IoResult::Ok = dev.pci_cfg_read(off, &mut v) {
        v
    } else {
        0
    }
}

/// Write one config dword (ignore deferral/failure — config writes are sync here).
fn cfg_write(dev: &mut VirtioPciDevice, off: u16, val: u32) {
    let _ = dev.pci_cfg_write(off, val);
}

/// Synthetic, internal BAR bases programmed into the config emulator so that
/// `find_bar` can resolve HDV's pre-decoded `(bar, offset)` deliveries. These are
/// never exposed to the guest (HDV owns the guest-facing BARs); they only key the
/// emulator's address→region routing. Page-aligned, non-overlapping, above 4 GiB
/// to avoid colliding with anything the emulator inspects.
/// Internal BAR bases for `find_bar` routing — never seen by the guest (the VID
/// places the guest-facing BARs out-of-band). Page-aligned, non-overlapping.
const BAR0_BASE: u64 = 0x1_0000_0000;
const BAR2_BASE: u64 = 0x1_0000_1000;

/// Program the internal BAR bases (BAR0/BAR2, 64-bit memory BARs → two dwords
/// each) into the config emulator. Leaves the command register alone — the guest
/// enables memory space, which is what activates `find_bar`. Run once, pre-live.
fn program_internal_bars(dev: &mut VirtioPciDevice) {
    cfg_write(dev, 0x10, BAR0_BASE as u32);
    cfg_write(dev, 0x14, (BAR0_BASE >> 32) as u32);
    cfg_write(dev, 0x18, BAR2_BASE as u32);
    cfg_write(dev, 0x1c, (BAR2_BASE >> 32) as u32);
}

/// The internal base for an HDV BAR selector index (BAR0 = transport, BAR2 =
/// MSI-X). `None` for BARs the device doesn't use.
fn bar_base(_dev: &mut VirtioPciDevice, bar: u8) -> Option<u64> {
    match bar {
        0 => Some(BAR0_BASE),
        2 => Some(BAR2_BASE),
        _ => None,
    }
}

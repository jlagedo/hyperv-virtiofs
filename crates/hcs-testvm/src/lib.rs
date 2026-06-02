//! Stand up a throwaway **Rocky Linux** compute system on Hyper-V via HCS, with a
//! direct kernel boot (no disk — the rootfs rides in the initramfs), and capture
//! its serial console over a named pipe. The whole point is to hand a live
//! `HCS_SYSTEM` handle to HDV so the `virtio-hdv` transport can be validated
//! against a real guest (the spike-1 rig).
//!
//! Windows + Hyper-V only. Off-Windows this is inert (the FFI is stubbed) so the
//! workspace still checks on other hosts.

#![cfg(windows)]

use hcs_sys as hcs;
use hdv::pci::{PciDetails, PciOps};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};

// Named-pipe open mode (PIPE_ACCESS_DUPLEX). PIPE_TYPE_BYTE / PIPE_READMODE_BYTE /
// PIPE_WAIT are all 0, so the pipe-mode argument below is simply 0.
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// An HCS `FlexibleIov` device slot. Declaring one gives the Linux guest a VPCI
/// bus over VMBus (enumerated by `pci-hyperv`) onto which an in-process HDV device
/// surfaces. The GUIDs must match the HDV device: `instance_guid` is the slot's
/// map-key and equals the HDV `DeviceInstanceId`; `emulator_id` equals the HDV
/// `DeviceClassId`.
#[derive(Clone)]
pub struct FlexibleIovSlot {
    pub instance_guid: String,
    pub emulator_id: String,
    pub hosting_model: String,
}

impl FlexibleIovSlot {
    /// A slot with the only attested `HostingModel`, `ExternalRestricted` (the
    /// in-process attach spike confirmed `HdvInitializeDeviceHost` works under it
    /// for a system we own).
    pub fn new(instance_guid: impl Into<String>, emulator_id: impl Into<String>) -> Self {
        Self {
            instance_guid: instance_guid.into(),
            emulator_id: emulator_id.into(),
            hosting_model: "ExternalRestricted".into(),
        }
    }
}

/// A deliberately driverless PCI device for the attach spikes: vendor `1af4`
/// (Red Hat/virtio) + device id `1100`, which lies **outside** every virtio range
/// so the guest enumerates it but binds no driver — a clean enumeration proof, no
/// virtio semantics. Class `0xff` (unassigned) and no BARs keep the surface
/// minimal. Shared by the in-process (`tests/attach.rs`) and out-of-process
/// (`tests/attach_oop.rs` + `src/bin/attach_child.rs`) spikes so both attach a
/// byte-identical device.
pub struct SpikeDevice;

impl SpikeDevice {
    pub const VENDOR: u16 = 0x1af4;
    pub const DEVICE: u16 = 0x1100;
}

impl PciOps for SpikeDevice {
    fn details(&self) -> PciDetails {
        PciDetails {
            vendor_id: Self::VENDOR,
            device_id: Self::DEVICE,
            revision_id: 0x01,
            prog_if: 0x00,
            sub_class: 0x00,
            base_class: 0xff, // "unassigned" class → no kernel driver claims it
            sub_vendor_id: Self::VENDOR,
            sub_system_id: 0x0040,
            probed_bars: [0; 6], // no BARs — simplest enumerable device
        }
    }

    fn read_config(&self, offset: u32) -> u32 {
        // Coherent Type-0 header, robust whether HDV synthesizes these registers
        // from `details()` or routes the reads to us.
        match offset {
            0x00 => ((Self::DEVICE as u32) << 16) | Self::VENDOR as u32,
            0x08 => 0xff00_0001, // class 0xff0000, revision 0x01
            0x2c => (0x0040u32 << 16) | Self::VENDOR as u32, // subsystem
            _ => 0,              // no BARs, no caps, no interrupt
        }
    }

    fn write_config(&self, _offset: u32, _value: u32) {
        // Nothing writable matters for enumeration.
    }
}

/// What to boot.
pub struct RockyConfig {
    pub kernel_path: String,
    pub initrd_path: String,
    pub kernel_cmdline: String,
    pub memory_mb: u32,
    pub processor_count: u32,
    /// Optional HDV device slot to declare in the compute-system document.
    pub flexible_iov: Option<FlexibleIovSlot>,
}

impl RockyConfig {
    /// Defaults matching the OpenVMM proof spike (console on ttyS0; generous RAM
    /// because the whole distro rides in the initramfs).
    pub fn new(kernel_path: impl Into<String>, initrd_path: impl Into<String>) -> Self {
        Self {
            kernel_path: kernel_path.into(),
            initrd_path: initrd_path.into(),
            kernel_cmdline: "console=ttyS0".into(),
            memory_mb: 4096,
            processor_count: 2,
            flexible_iov: None,
        }
    }

    /// Declare a `FlexibleIov` device slot (so an HDV device gets a VPCI bus to
    /// appear on). See [`FlexibleIovSlot`].
    pub fn with_flexible_iov(mut self, slot: FlexibleIovSlot) -> Self {
        self.flexible_iov = Some(slot);
        self
    }
}

/// A running (or attempted) Rocky compute system. Dropping it terminates and
/// closes the system.
pub struct RockyVm {
    id: String,
    system: hcs::HCS_SYSTEM,
    console: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
    pipe: isize,
}

// HCS_SYSTEM is an owned handle used only from the owning thread / under HCS's
// own synchronization.
unsafe impl Send for RockyVm {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn build_document(cfg: &RockyConfig, console_pipe: &str) -> String {
    let mut devices = serde_json::json!({
        "ComPorts": { "0": { "NamedPipe": console_pipe } }
    });
    if let Some(slot) = &cfg.flexible_iov {
        // Devices.FlexibleIov is a map keyed by the device instance GUID.
        let mut map = serde_json::Map::new();
        map.insert(
            slot.instance_guid.clone(),
            serde_json::json!({
                "EmulatorId": slot.emulator_id,
                "HostingModel": slot.hosting_model,
            }),
        );
        devices["FlexibleIov"] = serde_json::Value::Object(map);
    }

    serde_json::json!({
        "Owner": "hyperv-virtiofs-test",
        "SchemaVersion": { "Major": 2, "Minor": 1 },
        "ShouldTerminateOnLastHandleClosed": true,
        "VirtualMachine": {
            "Chipset": {
                "LinuxKernelDirect": {
                    "KernelFilePath": cfg.kernel_path,
                    "InitRdPath": cfg.initrd_path,
                    "KernelCmdLine": cfg.kernel_cmdline,
                }
            },
            "ComputeTopology": {
                "Memory": { "SizeInMB": cfg.memory_mb },
                "Processor": { "Count": cfg.processor_count }
            },
            "Devices": devices
        }
    })
    .to_string()
}

/// Read a `PWSTR` HCS result document (UTF-16, NUL-terminated) into a String.
unsafe fn pwstr_to_string(p: hcs::PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    // SAFETY: HCS guarantees a NUL-terminated wide string.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(p, len) };
    String::from_utf16_lossy(slice)
}

/// Run one HCS operation to completion, returning the result document or an error
/// string (with the HRESULT and any HCS error document).
unsafe fn run_op(op: hcs::HCS_OPERATION, what: &str, timeout_ms: u32) -> Result<String, String> {
    let mut result: hcs::PWSTR = std::ptr::null_mut();
    // SAFETY: valid op handle; result is a valid out ptr.
    let hr = unsafe { hcs::HcsWaitForOperationResult(op, timeout_ms, &mut result) };
    let doc = unsafe { pwstr_to_string(result) };
    if hr >= 0 {
        Ok(doc)
    } else {
        Err(format!("{what} failed: HRESULT {:#010x}; {doc}", hr as u32))
    }
}

impl RockyVm {
    /// Create the compute system **without starting it**. The guest is not yet
    /// running, but the system exists and [`system_handle`](Self::system_handle)
    /// is valid — so an HDV device host can attach a device *before boot* (the
    /// device is then present for the guest's first PCI enumeration). Pair with
    /// [`start`](Self::start), or use [`boot`](Self::boot) for create+start.
    pub fn create(cfg: &RockyConfig) -> Result<Self, String> {
        let id = format!(
            "hvfs-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let pipe_name = format!(r"\\.\pipe\{id}-com1");

        // Named-pipe server for the guest COM1; HCS connects as the client.
        let console = Arc::new(Mutex::new(Vec::new()));
        let (pipe, reader) = spawn_console(&pipe_name, console.clone())?;

        let document = build_document(cfg, &pipe_name);

        // SAFETY: all handles below are checked; wide strings outlive the calls.
        let system = unsafe {
            let create_op = hcs::HcsCreateOperation(std::ptr::null(), None);
            if create_op.is_null() {
                return Err("HcsCreateOperation returned null".into());
            }
            let mut system: hcs::HCS_SYSTEM = std::ptr::null_mut();
            let hr = hcs::HcsCreateComputeSystem(
                wide(&id).as_ptr(),
                wide(&document).as_ptr(),
                create_op,
                std::ptr::null(),
                &mut system,
            );
            if hr < 0 {
                let _ = run_op(create_op, "create", 30_000);
                hcs::HcsCloseOperation(create_op);
                return Err(format!(
                    "HcsCreateComputeSystem: HRESULT {:#010x}",
                    hr as u32
                ));
            }
            let created = run_op(create_op, "create", 60_000);
            hcs::HcsCloseOperation(create_op);
            created?;
            system
        };

        Ok(Self {
            id,
            system,
            console,
            reader: Some(reader),
            pipe,
        })
    }

    /// Start the already-created compute system, returning once HCS reports the
    /// start operation complete. The guest is then booting; use
    /// [`wait_for_console`](Self::wait_for_console) to watch its serial output.
    pub fn start(&self) -> Result<(), String> {
        // SAFETY: `self.system` is a live, created compute system handle.
        unsafe {
            let start_op = hcs::HcsCreateOperation(std::ptr::null(), None);
            if start_op.is_null() {
                return Err("HcsCreateOperation returned null".into());
            }
            let hr = hcs::HcsStartComputeSystem(self.system, start_op, std::ptr::null());
            if hr < 0 {
                hcs::HcsCloseOperation(start_op);
                return Err(format!(
                    "HcsStartComputeSystem: HRESULT {:#010x}",
                    hr as u32
                ));
            }
            let started = run_op(start_op, "start", 60_000);
            hcs::HcsCloseOperation(start_op);
            started?;
        }
        Ok(())
    }

    /// Create and start the compute system in one step (the common path). On a
    /// start failure the created system is cleaned up by [`Drop`].
    pub fn boot(cfg: &RockyConfig) -> Result<Self, String> {
        let vm = Self::create(cfg)?;
        vm.start()?;
        Ok(vm)
    }

    /// The live compute-system handle, for `HdvInitializeDeviceHost`.
    pub fn system_handle(&self) -> hcs::HCS_SYSTEM {
        self.system
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Snapshot of the serial console captured so far.
    pub fn console(&self) -> String {
        String::from_utf8_lossy(&self.console.lock().unwrap()).into_owned()
    }

    /// Poll the console until `needle` appears or `timeout` elapses.
    pub fn wait_for_console(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.console().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }
}

impl Drop for RockyVm {
    fn drop(&mut self) {
        // SAFETY: `system` is a live handle owned by this struct, torn down once.
        unsafe {
            let op = hcs::HcsCreateOperation(std::ptr::null(), None);
            if !op.is_null() {
                hcs::HcsTerminateComputeSystem(self.system, op, std::ptr::null());
                let _ = run_op(op, "terminate", 30_000);
                hcs::HcsCloseOperation(op);
            }
            hcs::HcsCloseComputeSystem(self.system);
            if self.pipe != INVALID_HANDLE_VALUE as isize {
                CloseHandle(self.pipe as *mut c_void);
            }
        }
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

/// Create the named-pipe server and a reader thread that drains the guest's COM1
/// into `console`. Returns the server handle and the thread.
fn spawn_console(
    pipe_name: &str,
    console: Arc<Mutex<Vec<u8>>>,
) -> Result<(isize, JoinHandle<()>), String> {
    let wname = wide(pipe_name);
    // SAFETY: valid wide name; default security; single-instance byte pipe.
    let handle = unsafe {
        CreateNamedPipeW(
            wname.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            0, // PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT, all 0
            1,
            64 * 1024,
            64 * 1024,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(format!("CreateNamedPipeW({pipe_name}) failed"));
    }
    let raw = handle as isize;
    let reader = std::thread::spawn(move || {
        let h = raw as *mut c_void;
        // Wait for HCS to connect the guest's COM port.
        // SAFETY: valid pipe handle owned by this thread for the read loop.
        let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
        if connected == 0 {
            // ERROR_PIPE_CONNECTED means a client beat us to it — still fine.
            let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            if err != ERROR_PIPE_CONNECTED {
                return;
            }
        }
        let mut buf = [0u8; 8192];
        loop {
            let mut read = 0u32;
            // SAFETY: valid handle and buffer; `read` receives the byte count.
            let ok = unsafe {
                ReadFile(
                    h,
                    buf.as_mut_ptr() as *mut _,
                    buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || read == 0 {
                break;
            }
            console
                .lock()
                .unwrap()
                .extend_from_slice(&buf[..read as usize]);
        }
    });
    Ok((raw, reader))
}

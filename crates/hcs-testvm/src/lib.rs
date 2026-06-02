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

/// What to boot.
pub struct RockyConfig {
    pub kernel_path: String,
    pub initrd_path: String,
    pub kernel_cmdline: String,
    pub memory_mb: u32,
    pub processor_count: u32,
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
        }
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
            "Devices": {
                "ComPorts": { "0": { "NamedPipe": console_pipe } }
            }
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
    /// Create and start the compute system, returning once HCS reports the start
    /// operation complete. The guest is then booting; use [`wait_for_console`] to
    /// watch its serial output.
    pub fn boot(cfg: &RockyConfig) -> Result<Self, String> {
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
        unsafe {
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
            run_op(create_op, "create", 60_000)?;
            hcs::HcsCloseOperation(create_op);

            let start_op = hcs::HcsCreateOperation(std::ptr::null(), None);
            let hr = hcs::HcsStartComputeSystem(system, start_op, std::ptr::null());
            if hr < 0 {
                hcs::HcsCloseOperation(start_op);
                hcs::HcsCloseComputeSystem(system);
                return Err(format!(
                    "HcsStartComputeSystem: HRESULT {:#010x}",
                    hr as u32
                ));
            }
            let start = run_op(start_op, "start", 60_000);
            hcs::HcsCloseOperation(start_op);
            if let Err(e) = start {
                hcs::HcsCloseComputeSystem(system);
                return Err(e);
            }

            Ok(Self {
                id,
                system,
                console,
                reader: Some(reader),
                pipe,
            })
        }
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

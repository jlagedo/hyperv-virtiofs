//! One-host-per-process worker for the Model-A concurrency proof
//! (`tests/concurrent_processes.rs`).
//!
//! This is the supported deployment shape in miniature: a single process that loads the
//! ABI, registers **one** device host against **one** VM, hot-adds **one** share, and
//! drives the guest to mount it — exactly what a host daemon would run per VM (as WSL
//! runs one `wsldevicehost` surrogate per device). The parent test spawns several of
//! these at once to prove independent device hosts in **separate processes** coexist on
//! the same machine.
//!
//! Usage: `host_child <tag> <instance_id_guid>`. Prints `CHILD_PASS tag=<tag>` on stdout
//! and exits 0 on success; `CHILD_FAIL …` and exits 1 otherwise. Progress goes to stderr
//! (the parent inherits it). Kernel/initrd come from `HVFS_KERNEL`/`HVFS_INITRD` or the
//! in-repo default (see `hcs_testvm::artifact_paths`).

#[cfg(windows)]
mod imp {
    use hcs_testvm::{RockyConfig, RockyVm};
    use hyperv_virtiofs::{
        hvfs_add_share, hvfs_host, hvfs_host_close, hvfs_host_open, hvfs_last_error, hvfs_share,
        HVFS_OK,
    };
    use std::ffi::{CStr, CString};
    use std::ptr;
    use std::time::Duration;

    fn last_error() -> String {
        let p = hvfs_last_error();
        if p.is_null() {
            return "(none)".into();
        }
        // SAFETY: thread-local DLL string, valid until the next ABI call on this thread.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    /// One attempt: create a VM, open a host, start, hot-add the share, wait for the
    /// guest mount. Returns whether the guest mounted the share.
    fn try_once(
        kernel: &str,
        initrd: &str,
        ws: &std::path::Path,
        tag: &str,
        instance: &str,
    ) -> bool {
        let mut cfg = RockyConfig::new(kernel, initrd);
        cfg.kernel_cmdline = format!("console=ttyS0 atelier.hptags={tag}");

        let vm = match RockyVm::create(&cfg) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[{tag}] create failed: {e}");
                return false;
            }
        };
        eprintln!(
            "[{tag}] compute system {} (pid {})",
            vm.id(),
            std::process::id()
        );

        let id_c = CString::new(vm.id()).unwrap();
        let host_json = CString::new(format!(r#"{{"memory_mb":{}}}"#, cfg.memory_mb)).unwrap();
        let mut host: *mut hvfs_host = ptr::null_mut();
        // SAFETY: valid C strings + writable out slot.
        let rc = unsafe { hvfs_host_open(id_c.as_ptr(), host_json.as_ptr(), &mut host) };
        if rc != HVFS_OK || host.is_null() {
            eprintln!("[{tag}] hvfs_host_open rc={rc}: {}", last_error());
            return false;
        }

        let mounted = (|| {
            if let Err(e) = vm.start() {
                eprintln!("[{tag}] start failed: {e}");
                return false;
            }
            if !vm.wait_for_console("GUEST_READY", Duration::from_secs(90)) {
                eprintln!("[{tag}] no GUEST_READY");
                return false;
            }
            let ws_path = ws.to_str().unwrap();
            let share_json = serde_json::json!({
                "tag": tag, "path": ws_path, "instance_id": instance, "ro": false,
            })
            .to_string();
            let share_json_c = CString::new(share_json).unwrap();
            let mut share: *mut hvfs_share = ptr::null_mut();
            // SAFETY: live host; valid C string; writable out slot.
            let arc = unsafe { hvfs_add_share(host, share_json_c.as_ptr(), &mut share) };
            if arc != HVFS_OK {
                eprintln!("[{tag}] hvfs_add_share rc={arc}: {}", last_error());
                return false;
            }
            vm.wait_for_console(
                &format!("HOTPLUG_MOUNT_PASS tag={tag}"),
                Duration::from_secs(45),
            )
        })();

        if !mounted {
            eprintln!("[{tag}] console tail:\n{}", vm.console());
        }
        // SAFETY: `host` came from a successful hvfs_host_open; closed once.
        let _ = unsafe { hvfs_host_close(host) };
        mounted
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.len() != 3 {
            eprintln!("usage: host_child <tag> <instance_id_guid>");
            std::process::exit(2);
        }
        let tag = &args[1];
        let instance = &args[2];

        let (kernel, initrd) = hcs_testvm::artifact_paths();
        if !std::path::Path::new(&kernel).exists() || !std::path::Path::new(&initrd).exists() {
            println!("CHILD_FAIL tag={tag} reason=missing-artifacts");
            std::process::exit(1);
        }

        let ws = std::env::temp_dir().join(format!("atelier-cp-{tag}"));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).expect("create workspace");
        std::fs::write(ws.join("SENTINEL.txt"), format!("process share {tag}\n")).unwrap();

        const ATTEMPTS: usize = 4;
        for attempt in 1..=ATTEMPTS {
            eprintln!("[{tag}] --- attempt {attempt}/{ATTEMPTS} ---");
            if try_once(&kernel, &initrd, &ws, tag, instance) {
                println!("CHILD_PASS tag={tag}");
                std::process::exit(0);
            }
        }
        println!("CHILD_FAIL tag={tag} reason=no-mount");
        std::process::exit(1);
    }
}

fn main() {
    #[cfg(windows)]
    imp::main();
    #[cfg(not(windows))]
    {
        eprintln!("host_child is Windows-only");
        std::process::exit(1);
    }
}

//! **Out-of-process HDV attach spike** — the task #16 A-vs-B experiment.
//!
//! Identical to `attach.rs`, except the HDV device host lives in a CHILD PROCESS
//! (`src/bin/attach_child.rs`) that this test spawns and owns, rather than in the
//! test process itself. The in-process variant gets as far as our `Initialize`
//! returning `S_OK` and then dies in the FlexibleIov VID's
//! `FinishReservingResources` (0x8000FFFF). This asks the narrower question: does
//! moving the emulator into a *separate process the host spawns* satisfy the
//! reservation, or must HCS itself launch the `ExternalRestricted` emulator?
//!
//!   * start succeeds + `1af4:1100` enumerated → **Variant A** (we spawn+own it).
//!   * start still fails at `FinishReservingResources` → **Variant B** (HCS must
//!     launch the registered emulator; adopt the WSL-style model, plan §6.2).
//!
//! RESULT (2026-06-02): **Variant B.** The child's device host registers and HDV
//! invokes our `Initialize` *in the child*, yet `HcsStartComputeSystem` fails
//! **byte-for-byte identically** to the in-process spike (`0x8000FFFF`,
//! `FinishReservingResources`, emulator `A7E1…0001` `'Unknown'`). The failure is
//! invariant to the process boundary — so spawning our own helper does not help;
//! the missing piece is the emulator *registration/identity* contract HCS resolves
//! the `EmulatorId` through. This test stays as the standing reproduction; see plan
//! §7 #3 / task #16. Next forensic target: how `wslservice`/`wsldevicehost` register.
//!
//! `#[ignore]` — needs a Hyper-V-capable host + the Rocky artifacts. Run it:
//!   $env:HVFS_KERNEL="E:\dev\spike\out\vmlinuz"
//!   $env:HVFS_INITRD="E:\dev\spike\out\initramfs.cpio.gz"
//!   cargo test -p hcs-testvm --test attach_oop -- --ignored --nocapture
#![cfg(windows)]

use hcs_testvm::{FlexibleIovSlot, RockyConfig, RockyVm};
use hdv::pci::{guid_to_string, SPIKE_CLASS_ID, SPIKE_INSTANCE_ID};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn attaches_hdv_pci_device_out_of_process() {
    let kernel =
        std::env::var("HVFS_KERNEL").unwrap_or_else(|_| r"E:\dev\spike\out\vmlinuz".into());
    let initrd = std::env::var("HVFS_INITRD")
        .unwrap_or_else(|_| r"E:\dev\spike\out\initramfs.cpio.gz".into());
    assert!(
        std::path::Path::new(&kernel).exists(),
        "kernel not found: {kernel}"
    );
    assert!(
        std::path::Path::new(&initrd).exists(),
        "initrd not found: {initrd}"
    );

    // Same FlexibleIov slot as the in-process spike; the GUIDs must match the HDV
    // device the child creates (map-key == DeviceInstanceId, EmulatorId ==
    // DeviceClassId — both minted in `hdv::pci`).
    let cfg = RockyConfig::new(kernel, initrd).with_flexible_iov(FlexibleIovSlot::new(
        guid_to_string(&SPIKE_INSTANCE_ID),
        guid_to_string(&SPIKE_CLASS_ID),
    ));

    // Create (but do not start). The device host is opened by the CHILD, not here.
    let vm = RockyVm::create(&cfg).expect("create Rocky compute system");
    eprintln!("compute system id: {}", vm.id());

    // Spawn the out-of-process emulator, handing it the compute-system id. Its
    // stderr is inherited (so its `[hdv::pci]` traces and any failure show here);
    // its stdout carries the readiness handshake.
    let mut child = Command::new(env!("CARGO_BIN_EXE_attach_child"))
        .arg(vm.id())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn attach_child");

    // Block until the child reports the HDV device is created (emulator
    // registered) BEFORE starting the VM — otherwise FinishReservingResources
    // races the child's HdvCreateDeviceInstance. A failing child exits (closing
    // stdout), so `lines.next()` returns `None` promptly rather than hanging.
    let stdout = child.stdout.take().expect("child stdout");
    let mut lines = BufReader::new(stdout).lines();
    let ready_deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while Instant::now() < ready_deadline {
        match lines.next() {
            Some(Ok(line)) => {
                eprintln!("attach_child> {line}");
                if line.contains("HVFS_CHILD_READY") {
                    ready = true;
                    break;
                }
            }
            Some(Err(e)) => panic!("reading attach_child stdout: {e}"),
            None => break, // child exited before signalling ready
        }
    }
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "attach_child never reported ready — it likely failed to open the device \
             host or create the device (see its stderr above)"
        );
    }

    // The emulator is live in the child process. Start the guest.
    let start = vm.start();

    // Whatever the outcome, don't leak the child process.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        start.expect(
            "HcsStartComputeSystem (FinishReservingResources is the task-#16 gate: \
             a failure here means out-of-process alone is not enough → Variant B)",
        );
        let needle = "1af4:1100";
        let seen = vm.wait_for_console(needle, Duration::from_secs(90));
        eprintln!(
            "===== guest console =====\n{}\n=========================",
            vm.console()
        );
        assert!(
            seen,
            "guest kernel never enumerated the HDV PCI device [{needle}] within timeout"
        );
    }));

    let _ = child.kill();
    let _ = child.wait();

    if let Err(e) = outcome {
        std::panic::resume_unwind(e);
    }
}

//! **Model-A concurrency proof: independent device hosts in separate processes.**
//!
//! The supported deployment is one device host per process (`docs/share-abi.md`,
//! "Deployment model"): a host daemon runs each VM's device host in its own process, as
//! WSL runs one `wsldevicehost` surrogate per device. This test guarantees that shape
//! works — it spawns two `host_child` processes at once, each opening its own host on its
//! own VM and hot-adding a share, and requires **both** to mount their share in the guest
//! concurrently.
//!
//! This is the topology we *do* vouch for. (We deliberately do **not** test, support, or
//! lock around multiple device hosts in one process: that path races in the closed
//! platform — registration, then teardown — in ways we can't fully verify, since WSL
//! never ships it. See `hdv::DeviceHost::from_proxy` and the roadmap.)
//!
//!   .\test\build-guest-artifacts.ps1   # one-time: build test\guest\out artifacts
//!   cargo test -p hcs-testvm --test concurrent_processes -- --ignored --nocapture
//! (Override artifact paths with $env:HVFS_KERNEL / $env:HVFS_INITRD; see docs/testing.md.)
#![cfg(windows)]

use std::process::{Command, Stdio};

/// One worker: a tag and a distinct instance-id GUID for its share.
struct Worker {
    tag: &'static str,
    instance: &'static str,
}

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn two_device_hosts_in_separate_processes() {
    // Artifact presence is also checked in each child, but fail fast here with a clear
    // message if they're missing.
    let (kernel, initrd) = hcs_testvm::artifact_paths();
    assert!(
        std::path::Path::new(&kernel).exists(),
        "kernel not found: {kernel}"
    );
    assert!(
        std::path::Path::new(&initrd).exists(),
        "initrd not found: {initrd}"
    );

    let workers = [
        Worker {
            tag: "cp1",
            instance: "c0000001-0001-4001-8001-000000000001",
        },
        Worker {
            tag: "cp2",
            instance: "c0000002-0002-4002-8002-000000000002",
        },
    ];

    // Spawn all children at once so their device-host registrations and guest boots
    // genuinely overlap; inherit stderr so each child's progress is visible live.
    let children: Vec<_> = workers
        .iter()
        .map(|w| {
            eprintln!("spawning host_child {} (instance {})", w.tag, w.instance);
            Command::new(env!("CARGO_BIN_EXE_host_child"))
                .arg(w.tag)
                .arg(w.instance)
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("spawn host_child")
        })
        .collect();

    // Each child is bounded (it retries a fixed number of times, then exits), so
    // wait_with_output returns without an external timeout.
    let outcomes: Vec<(bool, String)> = children
        .into_iter()
        .map(|c| {
            let out = c.wait_with_output().expect("wait host_child");
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            (
                out.status.success() && stdout.contains("CHILD_PASS"),
                stdout,
            )
        })
        .collect();

    for (w, (ok, stdout)) in workers.iter().zip(&outcomes) {
        eprintln!("child {}: ok={ok} stdout={}", w.tag, stdout.trim());
    }
    assert!(
        outcomes.iter().all(|(ok, _)| *ok),
        "all per-process device hosts must mount their share concurrently"
    );
    eprintln!(
        "PASS: {} device hosts in separate processes mounted concurrently",
        workers.len()
    );
}

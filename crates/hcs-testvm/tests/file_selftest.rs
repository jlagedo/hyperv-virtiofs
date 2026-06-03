//! **File-level edge cases + micro-benchmark over virtio-fs, end-to-end.**
//!
//! Drives the shipped C ABI exactly like `attach_abi.rs` (host_open pre-start →
//! start → hot-add one share), but boots the guest with `atelier.fileperf` so its
//! self-test (`test/guest/init`, `run_selftest`) runs on the freshly mounted share.
//! The guest writes files and prints structured sentinels; this test then verifies
//! them — including recomputing, **on the host**, the sha256 of a multi-MiB file the
//! guest wrote, proving guest→host write-through coherence (not just that a mount
//! succeeded).
//!
//! Verified:
//!   - large-file integrity   (host sha256 == guest sha256, sizes match)
//!   - throughput             (sequential write + cache-cold read, MB/s reported)
//!   - many-files metadata     (500 files created and visible on the host)
//!   - special / unicode names (spaces, nested dirs, non-ASCII)
//!
//! **Retry:** like the other live tests, the attach+boot is retried — HDV apertures
//! are an evictable cache, so a fraction of boots stall (see docs/testing.md).
//!
//!   .\test\build-guest-artifacts.ps1   # one-time: build test\guest\out artifacts
//!   cargo test -p hcs-testvm --test file_selftest -- --ignored --nocapture
//! (Override artifact paths with $env:HVFS_KERNEL / $env:HVFS_INITRD; see docs/testing.md.)
#![cfg(windows)]

use hcs_testvm::{RockyConfig, RockyVm};
use hyperv_virtiofs::{
    hvfs_add_share, hvfs_host, hvfs_host_close, hvfs_host_open, hvfs_last_error, hvfs_remove_share,
    hvfs_share, HVFS_ERR_UNSUPPORTED, HVFS_OK,
};
use sha2::{Digest, Sha256};
use std::ffi::{CStr, CString};
use std::ptr;
use std::time::Duration;

const TAG: &str = "fp1";
const INSTANCE_ID: &str = "f17e5717-0001-4001-8001-000000000001";

fn last_error() -> String {
    let p = hvfs_last_error();
    if p.is_null() {
        return "(none)".into();
    }
    // SAFETY: thread-local DLL string, valid until the next ABI call on this thread.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn make_workspace() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("atelier-virtiofs-fileperf-ws");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workspace dir");
    std::fs::write(dir.join("SENTINEL.txt"), b"file self-test workspace\n")
        .expect("write sentinel");
    dir
}

/// First console line containing `needle`, if any.
fn line_with<'a>(console: &'a str, needle: &str) -> Option<&'a str> {
    console.lines().find(|l| l.contains(needle))
}

/// Extract the value of a `key=value` token from a whitespace-separated line.
fn kv(line: &str, key: &str) -> Option<String> {
    line.split_whitespace()
        .find_map(|tok| tok.strip_prefix(&format!("{key}="))) // "key=val" -> "val"
        .map(|v| v.to_string())
}

fn sha256_hex(path: &std::path::Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn guest_file_selftest_over_virtiofs() {
    let (kernel, initrd) = hcs_testvm::artifact_paths();
    assert!(
        std::path::Path::new(&kernel).exists(),
        "kernel not found: {kernel}"
    );
    assert!(
        std::path::Path::new(&initrd).exists(),
        "initrd not found: {initrd}"
    );

    let ws = make_workspace();
    eprintln!("host workspace: {}", ws.display());

    const ATTEMPTS: usize = 6;
    for attempt in 1..=ATTEMPTS {
        eprintln!("--- attempt {attempt}/{ATTEMPTS} ---");
        if try_selftest(&kernel, &initrd, &ws) {
            eprintln!("PASS on attempt {attempt}");
            return;
        }
    }
    panic!("guest file self-test did not complete (no SELFTEST_DONE) in {ATTEMPTS} attempts");
}

fn try_selftest(kernel: &str, initrd: &str, ws: &std::path::Path) -> bool {
    let mut cfg = RockyConfig::new(kernel, initrd);
    // Keep the self-test's transfers modest: large sustained I/O is the most exposed to
    // HDV aperture-cache staleness, and the goal here is integrity + a measurable rate,
    // not a stress test. (Tunable via the guest cmdline without a rebuild.)
    cfg.kernel_cmdline = format!(
        "console=ttyS0 atelier.hptags={TAG} atelier.fileperf=1 atelier.bigmb=4 atelier.perfmb=8 atelier.manyn=100"
    );

    let vm = RockyVm::create(&cfg).expect("create Rocky compute system");
    eprintln!("compute system id: {}", vm.id());

    let id_c = CString::new(vm.id()).expect("system id has no interior NUL");
    let host_json = serde_json::json!({ "memory_mb": cfg.memory_mb }).to_string();
    let host_json_c = CString::new(host_json).expect("host_json has no interior NUL");

    let mut host: *mut hvfs_host = ptr::null_mut();
    // SAFETY: valid C strings + writable out slot.
    let rc = unsafe { hvfs_host_open(id_c.as_ptr(), host_json_c.as_ptr(), &mut host) };
    assert_eq!(rc, HVFS_OK, "hvfs_host_open failed: {}", last_error());
    assert!(!host.is_null());

    vm.start().expect("start Rocky compute system");

    let done = if !vm.wait_for_console("GUEST_READY", Duration::from_secs(90)) {
        eprintln!(
            "===== guest console (no GUEST_READY) =====\n{}",
            vm.console()
        );
        false
    } else {
        // Hot-add the share; the guest mounts it and (because of atelier.fileperf)
        // runs the file self-test on it.
        let ws_path = ws.to_str().expect("utf-8 workspace path");
        let share_json = serde_json::json!({
            "tag": TAG, "path": ws_path, "instance_id": INSTANCE_ID, "ro": false,
        })
        .to_string();
        let share_json_c = CString::new(share_json).expect("share_json has no interior NUL");
        let mut share: *mut hvfs_share = ptr::null_mut();
        // SAFETY: live host; valid C string; writable out slot.
        let arc = unsafe { hvfs_add_share(host, share_json_c.as_ptr(), &mut share) };
        assert_eq!(arc, HVFS_OK, "hvfs_add_share failed: {}", last_error());
        assert!(!share.is_null());

        // The self-test writes ~24 MiB (8 MiB integrity + 16 MiB perf) + 500 files, then
        // re-reads cache-cold — give it room.
        let done = vm.wait_for_console(
            &format!("SELFTEST_DONE tag={TAG}"),
            Duration::from_secs(120),
        );
        if !done {
            eprintln!(
                "===== guest console (self-test did not finish) =====\n{}",
                vm.console()
            );
        }

        // Best-effort remove (OK or UNSUPPORTED) — frees the share handle.
        // SAFETY: live share handle whose host is still open.
        let rrc = unsafe { hvfs_remove_share(share) };
        assert!(
            rrc == HVFS_OK || rrc == HVFS_ERR_UNSUPPORTED,
            "remove rc={rrc}"
        );
        done
    };

    let passed = done && verify_results(&vm.console(), ws);

    // SAFETY: `host` came from a successful hvfs_host_open; closed once.
    let crc = unsafe { hvfs_host_close(host) };
    assert_eq!(crc, HVFS_OK, "hvfs_host_close failed");
    passed
}

/// Assert every self-test sentinel passed and cross-check the artifacts the guest
/// wrote against the host-visible share directory. Returns false (rather than
/// panicking) for the *liveness* sentinels so the caller can retry a stalled boot;
/// panics on a genuine **correctness** failure (e.g. a sha mismatch), which a retry
/// would not fix.
fn verify_results(console: &str, ws: &std::path::Path) -> bool {
    // (1) Large-file write-through integrity — the strongest check.
    let info = match line_with(console, "SELFTEST_LARGEFILE tag=") {
        Some(l) => l,
        None => {
            eprintln!("missing SELFTEST_LARGEFILE line — retrying");
            return false;
        }
    };
    let guest_sha = kv(info, "sha256").expect("largefile line carries sha256");
    let guest_size: u64 = kv(info, "size")
        .expect("largefile line carries size")
        .parse()
        .unwrap();
    assert!(
        console.contains("SELFTEST_LARGEFILE_PASS"),
        "guest reported a large-file integrity failure: {info}"
    );

    let big = ws.join("selftest_big.bin");
    let host_meta = std::fs::metadata(&big).expect("host can stat the guest-written big file");
    assert_eq!(
        host_meta.len(),
        guest_size,
        "host size {} != guest size {guest_size}",
        host_meta.len()
    );
    let host_sha = sha256_hex(&big).expect("host sha256 of big file");
    assert_eq!(
        host_sha, guest_sha,
        "guest->host write-through CORRUPTION: host sha {host_sha} != guest sha {guest_sha}"
    );
    eprintln!("large-file integrity: {guest_size} bytes, sha256 {guest_sha} matches host ✓");

    // (2) Throughput.
    if let Some(perf) = line_with(console, "PERF tag=") {
        let w = kv(perf, "write_MBps").unwrap_or_default();
        let r = kv(perf, "read_MBps").unwrap_or_default();
        eprintln!("throughput: write {w} MB/s, read {r} MB/s  ({perf})");
        let wv: u64 = w.parse().unwrap_or(0);
        let rv: u64 = r.parse().unwrap_or(0);
        assert!(
            wv > 0 && rv > 0,
            "implausible throughput (write={wv} read={rv} MB/s)"
        );
    } else {
        eprintln!("missing PERF line — retrying");
        return false;
    }

    // (3) Many small files — visible on the host.
    assert!(
        console.contains("SELFTEST_MANYFILES_PASS"),
        "guest reported a many-files failure"
    );
    // The guest reports how many it created (`want=`); the host must see the same.
    let want: usize = line_with(console, "SELFTEST_MANYFILES tag=")
        .and_then(|l| kv(l, "want"))
        .and_then(|v| v.parse().ok())
        .expect("manyfiles line carries want=");
    let many_dir = ws.join("selftest_many");
    let host_count = std::fs::read_dir(&many_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        host_count, want,
        "host sees {host_count} files, expected {want}"
    );
    eprintln!("many-files: {want} files visible on host ✓");

    // (4) Special / unicode / nested names.
    assert!(
        console.contains("SELFTEST_SPECIALNAMES_PASS"),
        "guest reported a special-names failure"
    );
    let names = ws.join("selftest names");
    for rel in [
        "space file.txt",
        "nested/deep/leaf.txt",
        "uni_café_文件.txt",
    ] {
        let p = names.join(rel);
        assert!(
            p.exists(),
            "host cannot see special-named file: {}",
            p.display()
        );
    }
    eprintln!("special/unicode/nested names: visible on host ✓");

    true
}

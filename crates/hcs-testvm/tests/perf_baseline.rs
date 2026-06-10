//! **Performance baseline over virtio-fs, end-to-end.**
//!
//! Drives the shipped C ABI exactly like `file_selftest.rs` (host_open pre-start →
//! start → hot-add one share), but boots the guest with `atelier.perfbench` so the
//! guest's `run_perfbench` (`test/guest/init`) measures the data path and prints
//! structured `PB_*` sentinels. This test parses them across **repeated** runs and
//! writes a baseline report (`target/perf-baseline/baseline.{md,json}`) — the numbers
//! we compare optimisation work against.
//!
//! What it measures (per run, see `run_perfbench`):
//!   - sequential write   (fsync'd, 1M + 4k block — data-path throughput)
//!   - sequential read    cache-cold (drop_caches; the true device-read rate)
//!   - sequential read    warm (page cache; bounds what the device path costs)
//!   - random 4k read     cache-cold (aperture-cache churn worst case)
//!   - metadata           create / stat / readdir / delete ops/sec (FUSE round-trips)
//!   - parallel           create / stat / random-4k from J guest jobs — the only
//!     phases that can show request-level parallelism (strategy A); the serial
//!     phases are queue-depth-1 by construction
//!
//! Run it (needs Hyper-V + the perfbench initramfs; see docs/testing.md):
//!   $env:HVFS_INITRD = "test\guest\out\initramfs.perfbench.cpio.gz"
//!   $env:VIRTIO_HDV_APERTURE_STATS = "1"   # optional: host aperture-cache stats to stderr
//!   cargo test -p hcs-testvm --release --test perf_baseline -- --ignored --nocapture
//!
//! Tunables (env): HVFS_PB_SEQMB (64), HVFS_PB_META (1000), HVFS_PB_RAND (300),
//! HVFS_PB_JOBS (8), HVFS_PB_REPEATS (3), HVFS_PB_ATTEMPTS (4).
#![cfg(windows)]

use hcs_testvm::{RockyConfig, RockyVm};
use hyperv_virtiofs::{
    hvfs_add_share, hvfs_host, hvfs_host_close, hvfs_host_open, hvfs_last_error, hvfs_remove_share,
    hvfs_share, HVFS_ERR_UNSUPPORTED, HVFS_OK,
};
use std::ffi::{CStr, CString};
use std::ptr;
use std::time::Duration;

const TAG: &str = "pb1";
const INSTANCE_ID: &str = "f17e5717-0002-4002-8002-000000000002";

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn last_error() -> String {
    let p = hvfs_last_error();
    if p.is_null() {
        return "(none)".into();
    }
    // SAFETY: thread-local DLL string, valid until the next ABI call on this thread.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn make_workspace() -> std::path::PathBuf {
    // HVFS_PB_WS picks the share's backing directory (and thus the physical disk under
    // test). Default: a repo-local `target/` dir — i.e. the **same drive as the repo**, not
    // the system temp dir. `%TEMP%` is often on a slower/older volume (e.g. a SATA system
    // disk) than the dev drive, which would understate the data path; co-locating with the
    // repo keeps the baseline on the working drive and stays portable (no hard-coded letter).
    let base = std::env::var_os("HVFS_PB_WS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
                .join("perf-share")
        });
    let dir = base.join("atelier-virtiofs-perfbench-ws");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workspace dir");
    std::fs::write(dir.join("SENTINEL.txt"), b"perf baseline workspace\n").expect("write sentinel");
    dir
}

/// Extract the value of a `key=value` token from a whitespace-separated line.
fn kv(line: &str, key: &str) -> Option<String> {
    line.split_whitespace()
        .find_map(|tok| tok.strip_prefix(&format!("{key}=")))
        .map(str::to_string)
}

fn kv_f64(line: &str, key: &str) -> Option<f64> {
    kv(line, key).and_then(|v| v.parse().ok())
}

/// One measured metric: canonical name, unit, value.
struct Sample {
    name: String,
    unit: &'static str,
    value: f64,
}

/// Parse every `PB_*` line in one run's console into canonical samples.
fn parse_run(console: &str) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut push = |name: String, unit, value: Option<f64>| {
        if let Some(v) = value {
            out.push(Sample {
                name,
                unit,
                value: v,
            });
        }
    };
    for line in console.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("PB_SEQWRITE") {
            let bs = kv(rest, "bs").unwrap_or_default();
            push(format!("seqwrite_{bs}"), "MB/s", kv_f64(rest, "MBps"));
        } else if let Some(rest) = l.strip_prefix("PB_SEQREAD_COLD") {
            let bs = kv(rest, "bs").unwrap_or_default();
            push(format!("seqread_cold_{bs}"), "MB/s", kv_f64(rest, "MBps"));
        } else if let Some(rest) = l.strip_prefix("PB_SEQREAD_WARM") {
            let bs = kv(rest, "bs").unwrap_or_default();
            push(format!("seqread_warm_{bs}"), "MB/s", kv_f64(rest, "MBps"));
        } else if let Some(rest) = l.strip_prefix("PB_RANDREAD") {
            push("randread_4k".into(), "IOPS", kv_f64(rest, "iops"));
            push("randread_4k_bw".into(), "MB/s", kv_f64(rest, "MBps"));
        } else if let Some(rest) = l.strip_prefix("PB_META_CREATE") {
            push("meta_create".into(), "ops/s", kv_f64(rest, "ops"));
        } else if let Some(rest) = l.strip_prefix("PB_META_STAT") {
            push("meta_stat".into(), "ops/s", kv_f64(rest, "ops"));
        } else if let Some(rest) = l.strip_prefix("PB_META_READDIR") {
            push("meta_readdir".into(), "ms", kv_f64(rest, "ms"));
        } else if let Some(rest) = l.strip_prefix("PB_META_DELETE") {
            push("meta_delete".into(), "ops/s", kv_f64(rest, "ops"));
        } else if let Some(rest) = l.strip_prefix("PB_PAR_CREATE") {
            push("par_create".into(), "ops/s", kv_f64(rest, "ops"));
        } else if let Some(rest) = l.strip_prefix("PB_PAR_STAT") {
            push("par_stat".into(), "ops/s", kv_f64(rest, "ops"));
        } else if let Some(rest) = l.strip_prefix("PB_PAR_READ") {
            push("par_read".into(), "ops/s", kv_f64(rest, "ops"));
        }
    }
    out
}

/// Canonical metric order for the report (so it reads top-to-bottom sensibly).
const METRIC_ORDER: &[&str] = &[
    "seqwrite_1M",
    "seqwrite_4k",
    "seqread_cold_1M",
    "seqread_cold_4k",
    "seqread_warm_1M",
    "randread_4k",
    "randread_4k_bw",
    "meta_create",
    "meta_stat",
    "meta_readdir",
    "meta_delete",
    "par_create",
    "par_stat",
    "par_read",
];

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

#[test]
#[ignore = "requires Hyper-V + perfbench artifacts; run with --ignored"]
fn perf_baseline_over_virtiofs() {
    let (kernel, initrd) = hcs_testvm::artifact_paths();
    assert!(
        std::path::Path::new(&kernel).exists(),
        "kernel not found: {kernel}"
    );
    assert!(
        std::path::Path::new(&initrd).exists(),
        "initrd not found: {initrd} (set HVFS_INITRD to the perfbench initramfs)"
    );

    let seqmb = env_u32("HVFS_PB_SEQMB", 64);
    let meta = env_u32("HVFS_PB_META", 1000);
    let rand = env_u32("HVFS_PB_RAND", 300);
    let jobs = env_u32("HVFS_PB_JOBS", 8).max(1);
    let repeats = env_u32("HVFS_PB_REPEATS", 3).max(1);
    let attempts = env_u32("HVFS_PB_ATTEMPTS", 4).max(1);

    let ws = make_workspace();
    eprintln!("host workspace: {}", ws.display());
    eprintln!(
        "params: seqmb={seqmb} meta={meta} rand={rand} jobs={jobs} repeats={repeats} initrd={initrd}"
    );

    // metric name -> unit, and metric name -> collected values across runs.
    let mut units: std::collections::HashMap<String, &'static str> =
        std::collections::HashMap::new();
    let mut collected: std::collections::HashMap<String, Vec<f64>> =
        std::collections::HashMap::new();
    let mut runs_ok = 0u32;

    for run in 1..=repeats {
        eprintln!("===== run {run}/{repeats} =====");
        let mut got = None;
        for attempt in 1..=attempts {
            eprintln!("--- run {run} attempt {attempt}/{attempts} ---");
            if let Some(samples) = try_perfbench(&kernel, &initrd, &ws, seqmb, meta, rand, jobs) {
                got = Some(samples);
                break;
            }
        }
        match got {
            Some(samples) => {
                runs_ok += 1;
                for s in samples {
                    units.insert(s.name.clone(), s.unit);
                    collected.entry(s.name).or_default().push(s.value);
                }
            }
            None => eprintln!("run {run} did not complete after {attempts} attempts (skipping)"),
        }
    }

    assert!(
        runs_ok > 0,
        "no perf run completed (no PERFBENCH_DONE) — see console above"
    );

    // ---- Build the report ----
    let mut md = String::new();
    md.push_str("# virtio-fs performance baseline\n\n");
    md.push_str("Generated by `perf_baseline.rs` (guest `run_perfbench`). Non-DAX aperture-cache data path.\n\n");
    md.push_str(&format!(
        "- params: `seqmb={seqmb}` `meta={meta}` `rand={rand}` `jobs={jobs}` · runs ok: **{runs_ok}/{repeats}** · attempts/run: {attempts}\n",
    ));
    md.push_str(&format!(
        "- kernel: `{}`\n\n",
        std::path::Path::new(&kernel).display()
    ));
    md.push_str("| metric | median | min | max | unit | n |\n|---|---:|---:|---:|---|---:|\n");

    let mut json_metrics = serde_json::Map::new();
    // Stable order first, then any extras.
    let mut names: Vec<String> = METRIC_ORDER.iter().map(|s| s.to_string()).collect();
    for k in collected.keys() {
        if !names.contains(k) {
            names.push(k.clone());
        }
    }
    for name in &names {
        let Some(values) = collected.get(name) else {
            continue;
        };
        if values.is_empty() {
            continue;
        }
        let unit = units.get(name).copied().unwrap_or("");
        let med = median(values);
        let mn = values.iter().cloned().fold(f64::INFINITY, f64::min);
        let mx = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        md.push_str(&format!(
            "| {name} | {med:.0} | {mn:.0} | {mx:.0} | {unit} | {} |\n",
            values.len()
        ));
        json_metrics.insert(
            name.clone(),
            serde_json::json!({
                "median": med, "min": mn, "max": mx, "unit": unit, "samples": values,
            }),
        );
    }

    eprintln!("\n{md}");

    // ---- Write artifacts under <repo>/target/perf-baseline/ ----
    let out_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("perf-baseline");
    let _ = std::fs::create_dir_all(&out_dir);
    let md_path = out_dir.join("baseline.md");
    let json_path = out_dir.join("baseline.json");
    let json = serde_json::json!({
        "params": { "seqmb": seqmb, "meta": meta, "rand": rand, "jobs": jobs, "repeats": repeats, "runs_ok": runs_ok },
        "metrics": json_metrics,
    });
    if let Err(e) = std::fs::write(&md_path, &md) {
        eprintln!("WARN: could not write {}: {e}", md_path.display());
    } else {
        eprintln!("baseline written: {}", md_path.display());
    }
    if let Err(e) = std::fs::write(&json_path, serde_json::to_string_pretty(&json).unwrap()) {
        eprintln!("WARN: could not write {}: {e}", json_path.display());
    } else {
        eprintln!("baseline written: {}", json_path.display());
    }
}

/// One full boot+benchmark cycle. Returns parsed samples on success, `None` if the
/// boot stalled or the benchmark didn't finish (caller retries).
fn try_perfbench(
    kernel: &str,
    initrd: &str,
    ws: &std::path::Path,
    seqmb: u32,
    meta: u32,
    rand: u32,
    jobs: u32,
) -> Option<Vec<Sample>> {
    let mut cfg = RockyConfig::new(kernel, initrd);
    cfg.kernel_cmdline = format!(
        "console=ttyS0 atelier.hptags={TAG} atelier.perfbench=1 \
         atelier.pb_seqmb={seqmb} atelier.pb_meta={meta} atelier.pb_rand={rand} \
         atelier.pb_jobs={jobs}"
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

    let samples = if !vm.wait_for_console("GUEST_READY", Duration::from_secs(90)) {
        eprintln!(
            "===== guest console (no GUEST_READY) =====\n{}",
            vm.console()
        );
        None
    } else {
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

        // The benchmark sweeps writes/reads + 4×meta phases; give it room.
        let done = vm.wait_for_console(
            &format!("PERFBENCH_DONE tag={TAG}"),
            Duration::from_secs(300),
        );
        if !done {
            eprintln!(
                "===== guest console (perfbench did not finish) =====\n{}",
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

        done.then(|| parse_run(&vm.console()))
    };

    // SAFETY: `host` came from a successful hvfs_host_open; closed once.
    let crc = unsafe { hvfs_host_close(host) };
    assert_eq!(crc, HVFS_OK, "hvfs_host_close failed");

    // A run that finished but parsed nothing is a failure worth retrying.
    samples.filter(|s| !s.is_empty())
}

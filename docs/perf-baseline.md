# virtio-fs performance baseline

The reference numbers for **file-operation performance** of the current (non-DAX,
aperture-cache) data path, captured on a live Hyper-V VM through the shipped C ABI. This is
the yardstick optimisation work is measured against — re-run after any change to the data
path and compare.

> Status: **baseline captured** (see [Results](#results)). Methodology and harness are
> reproducible; numbers are host-specific (absolute values matter less than the *ratios*
> between phases and the *deltas* across changes).

## What it measures and why

The benchmark separates the two axes that decide where optimisation pays off (see
[`roadmap.md`](roadmap.md) DAX note and the data-path review in
[`perf-optimization.md`](perf-optimization.md)):

| Phase | Sentinel | Exercises | Reads on |
|---|---|---|---|
| Sequential write, fsync'd (1M / 4k block) | `PB_SEQWRITE` | queue → aperture copy → host write | data path |
| Sequential read, **cache-cold** (1M / 4k) | `PB_SEQREAD_COLD` | true device read rate (drop_caches forces refetch) | data path |
| Sequential read, warm (1M) | `PB_SEQREAD_WARM` | guest page cache (bounds what the device path costs) | — |
| Random 4k read, cache-cold | `PB_RANDREAD` | aperture-cache reuse/eviction worst case | aperture cache |
| Metadata create / stat / readdir / delete | `PB_META_*` | FUSE round-trip latency (what DAX does **not** help) | request queue |
| Parallel create / stat / open+read, J jobs | `PB_PAR_*` | host request **concurrency** (strategy A) | request dispatch |

Block-size pairs (1M vs 4k) expose per-request overhead; the cold/warm read gap bounds the
device-path cost; the metadata phases isolate FUSE round-trip cost from data throughput.

The serial phases are **queue-depth-1 by protocol** (FUSE is synchronous per op from one
thread), so they can never show request-level parallelism — that's what the `PB_PAR_*`
phases exist for. Their per-op work is strictly shell **builtins** (`echo >`, `[ -s ]`,
`read -r <`): a fork+exec per op on the small guest measures process spawn (~1.5 ms), not
the host path (~115 µs FUSE RTT). Knobs: `HVFS_PB_JOBS` (default 8), `HVFS_PB_CPUS`
(guest vCPUs), `VIRTIO_HDV_WORKERS` (host dispatch: unset = serial, N = offload pool).
Reference (serial device, 2 vCPU, jobs=8): `par_create 2976` · `par_stat 8695` ·
`par_read 5000` ops/s — see `perf-optimization.md` → *Measured outcome* for the offload
matrix (+47–103% at 8 vCPUs).

## How to run

```pwsh
# One command: (re)builds the perfbench initramfs in WSL if needed, runs 3x, writes the report.
.\test\run-perf-baseline.ps1                       # defaults: seqmb=64 meta=1000 rand=300 repeats=3
.\test\run-perf-baseline.ps1 -ApertureStats        # also capture host aperture-cache stats
.\test\run-perf-baseline.ps1 -Seqmb 128 -Repeats 5
```

Under the hood (all reproducible, no Docker):
- **Guest half** — `test/guest/init` `run_perfbench`, gated on the `atelier.perfbench` cmdline
  flag, prints the `PB_*` sentinels. It deliberately `drop_caches` between phases to measure
  the real device path (unlike `run_selftest`, which stays warm for integrity).
- **Perfbench initramfs** — `test/guest/repack-perfbench-initramfs.sh` repacks the stock
  `out/initramfs.cpio.gz` with the updated `init` (cpio/gzip in WSL as root; the kernel and
  rootfs are unchanged). Produced as `out/initramfs.perfbench.cpio.gz`.
- **Host half** — `crates/hcs-testvm/tests/perf_baseline.rs` drives the C ABI exactly like the
  e2e ladder (`hvfs_host_open` pre-start → start → `hvfs_add_share`), boots the guest
  `repeats` times (retrying stalled boots), parses the sentinels, and writes
  `target/perf-baseline/baseline.{md,json}` (medians + spread). This page is the curated copy.

The share's **backing directory** (the disk under test) defaults to a repo-local
`target/perf-share` — i.e. the **same drive as the repo**, not `%TEMP%` (which is often a slower
system volume). Override with `-WorkspaceDir <path>` / `HVFS_PB_WS` to test a specific disk.

Tunables (env, or the script params): `HVFS_PB_SEQMB`, `HVFS_PB_META`, `HVFS_PB_RAND`,
`HVFS_PB_REPEATS`, `HVFS_PB_WS`.

## Results

Capture: **2026-06-03**, Win11 26200, `seqmb=64 meta=1000 rand=300`, **3/3 runs clean**. Share
backed by a **local NVMe SSD** (the repo drive — the default workspace). Host-specific; treat the
ratios and the spread as the signal, not absolute MB/s.

| metric | median | min | max | unit |
|---|---:|---:|---:|---|
| seqwrite_1M | 516 | 508 | 639 | MB/s |
| seqwrite_4k | 43 | 42 | 44 | MB/s |
| seqread_cold_1M | 906 | 894 | 932 | MB/s |
| seqread_cold_4k | 46 | 45 | 47 | MB/s |
| seqread_warm_1M | 871 | 849 | 894 | MB/s |
| randread_4k | 660 | 657 | 668 | IOPS |
| randread_4k (bw) | 2 | 2 | 2 | MB/s |
| meta_create | 1200 | 1161 | 1253 | ops/s |
| meta_stat | 623 | 611 | 643 | ops/s |
| meta_readdir (1000 entries) | 27 | 27 | 28 | ms |
| meta_delete | 565 | 557 | 583 | ops/s |

### Host aperture cache (VIRTIO_HDV_APERTURE_STATS, representative run)

```
ops=1,122,304  hits=1,121,380  → hit rate 99.92%
creates=924  evicts=413  quota_retries=413
create_fails=0  bad_range=0  not_bound=0   (clean — no guest-triggered errors)
live=511  peak_live=511 (cap 1024)   max_span=4096  (every aperture is a single 4 KiB page)
```

### Guest vs host-native on the same disk — the virtualization tax

Native file I/O on the same NVMe volume, same workload (host write is durably flushed; host read
is page-cache-served), versus the guest through virtio-fs:

| operation | host-native | guest (virtio-fs) | guest / host |
|---|---:|---:|---:|
| seq write 1M | 1085 MB/s | 516 MB/s | 0.48× |
| seq read 1M | 2667 MB/s (cache) | 906 MB/s | 0.34× |
| meta create | 3311 ops/s | 1200 ops/s | 0.36× |
| meta stat | 11628 ops/s | 623 ops/s | **0.05×** |
| meta delete | 5952 ops/s | 565 ops/s | **0.09×** |

The gap is the virtio-fs path itself — aperture copy + FUSE round-trip + the single-threaded host
executor. It is modest for bulk transfer (~2–3×) and large for small/metadata ops (~10–20×).

### Key findings (what the baseline tells us)

1. **Block size dominates, not bandwidth.** 1M-block hits ~500 MB/s write / ~900 MB/s read; the
   *same* bytes at 4k collapse to ~43–46 MB/s — a **>10× penalty**. The cost is **per-request
   overhead** (one FUSE round-trip + memory op per request on a single-threaded executor), not
   copy throughput. Highest-value optimisation target (data-path review finding #1).
2. **The device read path is already efficient.** cold-1M (906) ≈ warm-1M (871) — the aperture
   path is at page-cache speed for bulk reads. So a DAX window would add little to *bulk read
   throughput*; its win would be `mmap` semantics and small/random access, not sequential MB/s.
3. **The aperture cache is not a bottleneck.** 99.9% hit, all single-page (`max_span=4096`), zero
   errors, 511 live (half the cap). The "containment caching" idea (review finding #2) would help
   little — accesses are already ≤1 page, so exact-match keying isn't fragmenting anything.
   **De-prioritise it.**
4. **Latency-bound small ops are the real ceiling.** random-4k ≈ **660 IOPS (~1.5 ms/op)** and
   metadata ≈ **560–1200 ops/s (~1–2 ms/op)** — and `stat` runs **~20× slower than host-native**.
   All FUSE round-trips serialised on the single host IOCP thread. Throughput-*per-op*, not
   bytes-per-second, bounds real file-tree workloads (build trees, `git status`, `node_modules`).

**Direction this sets:** optimisation effort should target **per-request overhead / request
concurrency** (the single-threaded executor + inline blocking FUSE dispatch — data-path review
finding #1), which moves the 4k-block, metadata, and random numbers. The aperture cache (#2) and
DAX are **not** the levers this baseline points at.

## Reading the numbers

- **Write vs cold-read**: both traverse the aperture-copy path; large gaps point at the
  read/write asymmetry or host-FS cost.
- **1M vs 4k**: a big drop at 4k = per-request overhead dominates (single-threaded executor,
  per-op aperture lookup) — finding #1/#2 in the data-path review.
- **cold vs warm read**: the gap is the device-path cost; it bounds the ceiling DAX could
  remove for bulk reads.
- **metadata ops/s**: FUSE round-trip throughput; DAX does **not** help here, so if this is the
  dominant cost for a workload, data-path optimisation (not DAX) is the lever.
- **random 4k cold**: stresses aperture reuse/eviction; pair with `-ApertureStats` to see
  hit-rate / evicts / quota-retries under it.

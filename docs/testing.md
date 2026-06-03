# Testing

This project has two tiers of tests:

| Tier | What | Where | Runs on CI? |
|---|---|---|---|
| **Unit / build gates** | compile, `clippy -D warnings`, `fmt`, header-freshness, `cargo test --workspace` (incl. the C ABI + GUID unit tests) | every crate | ✅ yes (`windows-latest`) |
| **End-to-end (e2e)** | boot a real Rocky Linux VM under Hyper-V and drive the stack through to the C ABI | `crates/hcs-testvm/tests/` | ❌ no — needs a Hyper-V host + guest artifacts |

The unit tier is covered by `cargo test --workspace` and the CI gates in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) — see [Unit tests](#unit-tests)
below for what it now covers. **The rest of this document is about the e2e tier** — how to
reproduce it on your own machine, what each test proves, and the contract that keeps it
trustworthy.

The e2e tests are marked `#[ignore]` so they never run accidentally (or on hosted CI,
which has no nested virtualization). You opt in explicitly.

---

## What the e2e suite proves

A *ladder* — each rung boots a throwaway VM and validates one more layer of the stack.
A green ladder is the project's central trust claim: **a stock EL-family Linux guest
mounts a host directory over our open HDV virtio-fs bridge, driven through the shipped C
ABI, with live multi-share.**

```
boot ─▶ attach_proxy ─▶ attach_virtiofs ─▶ hotplug ─▶ attach_abi
 rig     transport        cold mount        live add    the product
```

---

## Prerequisites

1. **Windows 11 with Hyper-V / Windows Hypervisor Platform enabled.** The tests create
   real compute systems via HCS (`computecore.dll`) and attach devices via HDV
   (`vmdevicehost.dll`). A non-virtualization-capable host cannot run them.
   - Enable: *Settings → Optional features → More Windows features →* check
     **Hyper-V** and **Windows Hypervisor Platform**, reboot. (Or
     `Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All` in an
     elevated shell.)
2. **The Rust toolchain + `protoc`** — same as the [build prerequisites](../README.md#build).
3. **For building the guest artifacts:** WSL2 with a Linux distro, and a `docker` CLI
   reachable from inside it (Docker Desktop with WSL integration, or Docker Engine in the
   distro). The artifact build runs entirely in a container; nothing is installed on the
   Windows host. *(You only need this once, or whenever you change `test/guest/init`.)*

---

## Quickstart

```pwsh
# 1. Build the guest artifacts (once; ~minutes, needs network + Docker).
.\test\build-guest-artifacts.ps1

# 2. Run the whole ladder with a PASS/FAIL summary.
.\test\run-e2e.ps1
```

`run-e2e.ps1` builds the artifacts automatically if they're missing, points
`HVFS_KERNEL` / `HVFS_INITRD` at them, runs the ladder in order, and exits non-zero if
any rung fails.

Useful flags:

```pwsh
.\test\run-e2e.ps1 -List                  # show the ladder, don't run
.\test\run-e2e.ps1 -Test attach_abi       # run one rung
.\test\run-e2e.ps1 -Build                 # force-rebuild artifacts first
.\test\run-e2e.ps1 -IncludeNegativeSpikes # also run the expected-fail spikes
```

---

## The guest artifacts

The tests boot two files (git-ignored; built, not committed):

| Artifact | ~Size | What |
|---|---|---|
| `test/guest/out/vmlinuz` | 16 MB | a **stock** Rocky Linux 10 kernel (bzImage, exactly as the distro ships it — no patches) |
| `test/guest/out/initramfs.cpio.gz` | ~500 MB | the full Rocky rootfs **+** our `test/guest/init` self-test, packed as an initramfs |

They're built by [`test/guest/build-rocky-initramfs.sh`](../test/guest/build-rocky-initramfs.sh),
which runs inside a `rockylinux/rockylinux:10` container: it `dnf install`s a kernel +
module set, drops `test/guest/init` in as PID 1, and `cpio | gzip`s the whole root into
the initramfs. The PowerShell wrapper [`test/build-guest-artifacts.ps1`](../test/build-guest-artifacts.ps1)
just invokes that script through WSL and verifies the output.

That a *stock, unmodified* EL10 kernel mounts our share is the point: virtio-fs support
is in-box (`virtiofs.ko`), so no guest changes are needed — only the host-side device our
DLL provides.

The artifacts are deliberately **not** committed (the initramfs is ~500 MB and the kernel
version drifts with the base image). The two inputs that *define* them — the build script
and the `init` self-test — **are** committed, so anyone can reproduce a byte-equivalent
boot.

---

## The console-sentinel contract

The guest half of every test is `test/guest/init`. It runs as PID 1, exercises the
device, and prints fixed banner strings to the serial console (COM1). The Rust half
([`crates/hcs-testvm/src/lib.rs`](../crates/hcs-testvm/src/lib.rs) captures COM1 over a
named pipe) waits for those exact strings. **The sentinels are the test contract.**

| Sentinel (printed by `init`) | Meaning | Consumed by |
|---|---|---|
| `OPENVMM-VIRTIOFS-SPIKE` | userspace reached | `boot.rs` |
| `1af4:1100` | the driverless PCI device enumerated (via kernel dmesg) | `attach*.rs` |
| `PROOF_COMPLETE_PASS` | cold virtio-fs mount succeeded + sentinel file read | `attach_virtiofs.rs` |
| `GUEST_READY` | boot proof done; hot-plug watch loop armed | `hotplug.rs`, `attach_abi.rs` |
| `HOTPLUG_MOUNT_PASS tag=<t>` | a hot-added device for tag `<t>` mounted | `hotplug.rs`, `attach_abi.rs` |
| `HOTPLUG_REMOVE_OK tag=<t>` | a removed device's mount went stale and was unmounted | `hotplug.rs` (stage 3) |
| `SELFTEST_LARGEFILE … sha256=…` | a multi-MiB file written; the host re-checks this digest | `file_selftest.rs` |
| `SELFTEST_LARGEFILE_PASS tag=<t>` | guest wrote + hashed the file (the host does the integrity cross-check) | `file_selftest.rs` |
| `PERF tag=<t> … write_MBps=… read_MBps=…` | sequential throughput numbers | `file_selftest.rs` |
| `SELFTEST_MANYFILES_PASS tag=<t>` | the requested number of small files created (metadata path) | `file_selftest.rs` |
| `SELFTEST_SPECIALNAMES_PASS tag=<t>` | space / unicode / nested names round-trip | `file_selftest.rs` |
| `SELFTEST_DONE tag=<t>` | the opt-in self-test finished | `file_selftest.rs` |

The self-test sentinels are printed **only** when the kernel command line carries
`atelier.fileperf` (the host sets it for `file_selftest.rs` only), so the other tests are
unaffected.

> ⚠️ **If you rename or change a sentinel, you must update the test that waits for it
> *and* rebuild the artifacts** (`test/build-guest-artifacts.ps1`) — the `init` script is
> baked into the initramfs at build time. A stale initramfs is the most common cause of a
> test that hangs until timeout.

The host passes the candidate hot-plug tags to the guest on the kernel command line
(`atelier.hptags=hp1,hp2`); the watch loop in `init` tries to mount each.

---

## The test ladder

All live in `crates/hcs-testvm/tests/`. Status reflects the last live run (2026-06-02 on
Windows 11 26200).

| Test | Proves | Gate | Retry | Status |
|---|---|---|---|---|
| **`boot`** | the rig itself: Rocky boots under HCS to userspace | `OPENVMM-VIRTIOFS-SPIKE` | — | ✅ PASS |
| **`attach_proxy`** | the `ExternalRestricted` FlexibleIov **proxy** handshake (a driverless device enumerates over VMBus VPCI) — see [`hdv-proxy-abi.md`](hdv-proxy-abi.md) | `1af4:1100` | — | ✅ PASS |
| **`attach_virtiofs`** | the real `VirtioHdvDevice` transport: guest **mounts + reads** a file, device attached **before** start | `PROOF_COMPLETE_PASS` | 4× | ✅ PASS |
| **`hotplug`** | live device-per-share: ① hot-add one device to a running VM, ② two devices concurrently, ③ best-effort remove — see [`hotplug-spike.md`](hotplug-spike.md) | `HOTPLUG_MOUNT_PASS tag=…` | 4× | ✅ PASS (remove is platform-blocked; expected) |
| **`attach_abi`** | the **shipped front door**: drives the exported C functions end-to-end (`hvfs_host_open` → `hvfs_add_share` → `hvfs_share_instance_id` → `hvfs_remove_share` → `hvfs_host_close`) — the authoritative ABI proof | `HOTPLUG_MOUNT_PASS tag=hp1` | 4× | ✅ PASS |
| **`edge_cases`** | the C ABI **rejects bad input** against a *live* device host: `ro:true` → `NOT_IMPLEMENTED`, non-GUID `instance_id` / malformed JSON / missing field / null arg → `INVALID_ARG`, and the host stays healthy. Needs only a *created* (not started) VM — fast, no retries | (none — assert on return codes) | — | ✅ PASS |
| **`concurrent_processes`** | **Model A**: spawns two `host_child` processes at once, each opening its own device host on its own VM and hot-adding a share; requires **both** to mount concurrently. Proves the supported "one device host per process" deployment ([`share-abi.md`](share-abi.md#deployment-model--one-device-host-per-process-model-a)) | both children print `CHILD_PASS` | per-child 4× | ✅ PASS |
| **`file_selftest`** | **data-path integrity** over virtio-fs: multi-MiB **write-through integrity** (host recomputes the guest's sha256), sequential write/read MB/s, a tunable count of small files, and space/unicode/nested names, all cross-checked against the host-visible share. Heaviest, longest sustained I/O of any rung. Promoted to the ladder once the `max_address` ceiling bug (below) was fixed — passes reliably, incl. **64 MiB + 500 files** at ~750–970 MB/s read | `SELFTEST_DONE tag=…` | 6× | ✅ PASS |

`run-e2e.ps1` runs exactly these rungs and they are reliably green. Sizes for `file_selftest` are
tunable from the guest cmdline (`atelier.bigmb`/`perfmb`/`manyn`) without a rebuild. One test lives
**outside** the green ladder:

- **`diag_cold_multidevice`** *(diagnostic)* — asserts the platform limitation that two
  *custom*-class FlexibleIov devices are rejected at power-on (`0xC0350005`), i.e. *why* the
  well-known class id is required. Passes today. Run with
  `cargo test -p hcs-testvm --test diag_cold_multidevice -- --ignored --nocapture`.

### Capturing diagnostics

All runtime diagnostics are structured `tracing` events (see `CLAUDE.md`, "Logging &
diagnostics"); the `hyperv_virtiofs` cdylib installs the subscriber that routes them. Two ways
to see them:

- **stderr** — set a dev env var before the run. `VIRTIO_HDV_TRACE=1` raises the transport to
  TRACE (the per-access data-path firehose, incl. ≤64 B byte dumps of rings/descriptors/FUSE
  headers); `VIRTIO_HDV_APERTURE_STATS=1` emits the aperture-cache stats event; `RUST_LOG`
  controls verbosity otherwise (default info). The e2e tests run with `--nocapture`, so these
  land in the test output (e.g. `cargo test … attach_abi -- --ignored --nocapture` with
  `VIRTIO_HDV_TRACE=1` prints `… TRACE virtio_hdv: read_config …`).
- **a C callback** — a consumer (or the C example) passes `hvfs_set_logger(cb, ctx)`; the same
  event stream arrives with a syslog `level`. Best-effort: if the host process already owns a
  global `tracing` subscriber, that one wins.

### The "aperture-coherence limitation" was a `max_address` bug (RESOLVED)

For most of the project the `file_selftest` flakiness — mounts fine, then errors mid-I/O, worse on
sustained transfers — was blamed on HDV aperture **snapshot semantics**. That was a misdiagnosis.
Byte-level data-path tracing (`VIRTIO_HDV_TRACE` + `VIRTIO_HDV_APERTURE_STATS`) showed the device
replying **`-EIO`** to FUSE (surfacing as `ls: Invalid argument`) because a descriptor buffer at GPA
**≈4.04 GiB** was rejected **before** the aperture path even ran (`bad_range=0`, zero aperture
failures over ~254 k ops).

Root cause: Hyper-V splits guest RAM around the 32-bit MMIO hole — low RAM below ~3.75 GiB, the rest
**remapped above 4 GiB** — so a 4 GiB guest's DMA buffers can sit at `0x1_xxxx_xxxx`. We set
OpenVMM's `max_address` to the flat RAM size (`memory_mb · 1 MiB = 4 GiB`), so high-RAM buffers
exceeded the ceiling and `guestmem` rejected them → `-EIO`. Flaky because only some allocations land
high; **sustained I/O is most exposed**, which is why it looked like "staleness on large transfers."

Fixed in `crates/virtio-hdv/src/mem.rs` (`ram_size_to_max_gpa`: `max_address = 4 GiB + ram_size`, a
safe upper bound for any split). `file_selftest` now passes reliably including 64 MiB transfers; the
on-demand aperture cache runs at ~99.9 % hit rate with zero failures. The HCS-config experiments
(`Memory.PinBackingPages` rejected `0xC037002E`; `AllowOvercommit:false` "still stalls") were
chasing a backing-coherence problem that didn't exist — those failures were this ceiling bug. An HDV
aperture is a direct `VidMapMemoryBlockPageRangeEx` of *backed* pages and is coherent; the closed
`HdvGuestMemoryEvictionWorker` is quota management, **not** a coherence protocol (see
[`hdv-aperture-internals.md`](hdv-aperture-internals.md)).

**Why some tests still retry.** A separate, much rarer effect: HDV guest-memory apertures are an
evictable cache (no DAX yet; see [the roadmap](roadmap.md)), so a fraction of *boots* stall on the
early-boot guest-memory mapping. Each attempt is a fresh ~5 s VM, so 1–2 retries are normal; needing
all attempts every run is a smell worth investigating.

### Negative spikes (expected to fail)

Two tests are *standing reproductions of a platform limitation*, not pass criteria. They
**assert success and therefore fail by design** on current Windows — kept so the
constraint stays documented and re-checkable across Windows versions. They are excluded
from the ladder unless you pass `-IncludeNegativeSpikes`.

| Test | Question it pins | Result |
|---|---|---|
| **`attach`** | does an *in-process* `HdvInitializeDeviceHost` (no proxy) satisfy the start reservation? | ❌ no — `HcsStartComputeSystem` fails in `FinishReservingResources` (`0x8000FFFF`) |
| **`attach_oop`** | does moving the emulator to a *child process we spawn* fix it? | ❌ no — fails identically; HCS wants to launch the registered emulator itself (the proxy path, which `attach_proxy` then proves) |

---

## Unit tests

Deterministic, offline, and run on CI (`cargo test --workspace`) — no Hyper-V needed.
They cover the parts of the stack that *don't* require a live VM, so a regression in the
ABI's contract is caught on every push:

- **C ABI surface** (`crates/hyperv_virtiofs/src/lib.rs`, `mod tests`): the ABI version,
  status-code distinctness, the `catch_unwind` **panic guard** (a panicking body becomes
  `HVFS_ERR_PANIC` and never crosses the boundary), C-string borrowing/validation,
  `host_json` / `share_json` parsing (required fields, `ro` default), the
  `HcsModifyComputeSystem` request shaping, and the **null-argument contracts** for every
  exported entry point (each returns `INVALID_ARG`/`OK` and nulls its out-param without
  touching the platform).
- **GUID helpers** (`crates/hdv/src/pci.rs`, `mod tests`): `guid_to_string` canonical form,
  `guid_from_string` accepting braces/uppercase/whitespace and rejecting malformed input,
  and round-trip stability for the well-known constants. These validate the caller-supplied
  `instance_id` at the ABI boundary, so their correctness is load-bearing.

```pwsh
cargo test --workspace            # all unit tests
cargo test -p hyperv_virtiofs -p hdv --lib   # just the ABI + GUID tests
```

The live device paths these can't reach (real `hvfs_host_open`, device attach, mount) are
proven by the e2e ladder above; `edge_cases.rs` additionally re-checks the ABI's rejection
paths against a *real* `hvfs_host`.

## Running tests manually (without the runner)

```pwsh
# Point at the artifacts (or let the tests default to test\guest\out\).
$env:HVFS_KERNEL = "$PWD\test\guest\out\vmlinuz"
$env:HVFS_INITRD = "$PWD\test\guest\out\initramfs.cpio.gz"

# One target (use --nocapture to watch the guest serial console live):
cargo test -p hcs-testvm --test attach_abi -- --ignored --nocapture

# A single function within a multi-test target:
cargo test -p hcs-testvm --test hotplug guest_hot_mounts_two_devices_concurrently -- --ignored --nocapture

# A whole multi-test target (e.g. hotplug's three tests) MUST run serially —
# each test registers an HDV device host and the platform permits only one at a time:
cargo test -p hcs-testvm --test hotplug -- --ignored --nocapture --test-threads=1
```

> **Always pass `--test-threads=1` for these e2e targets.** A `cargo test` target is one
> process, and running its tests in parallel would register **multiple device hosts in one
> process** — which is the unsupported topology (the supported model is one device host per
> process; see [`share-abi.md`](share-abi.md#deployment-model--one-device-host-per-process-model-a)).
> In-process overlap races the platform (`from_proxy` → `E_ACCESSDENIED 0x80070005`) and can
> crash on teardown. The runner (`run-e2e.ps1`) sets `--test-threads=1` for you; the danger
> is only when invoking `cargo test` by hand on a multi-`#[test]` target like `hotplug`. The
> `concurrent_processes` test deliberately uses *separate processes* to exercise concurrency
> the supported way.

If `HVFS_KERNEL` / `HVFS_INITRD` are unset, the tests default to `test/guest/out/`
(resolved from the crate's location), so a plain `cargo test … -- --ignored` works after
a build with no env vars at all. See `hcs_testvm::artifact_paths()`.

---

## How the rig works

`crates/hcs-testvm` ([`src/lib.rs`](../crates/hcs-testvm/src/lib.rs)) is the test-only
harness. `RockyVm`:

- builds an HCS create document (`SchemaVersion {2,7}`, Windows 11) with a
  `LinuxKernelDirect` boot pointing at the artifacts and COM1 wired to a named pipe;
- `create()` (no start), `start()`, `boot()` (both), and `modify()` (hot-add a
  `FlexibleIov` slot via `HcsModifyComputeSystem`);
- spawns a reader thread draining COM1 into an in-memory buffer, exposed via
  `wait_for_console(needle, timeout)`;
- tears the system down in `Drop` (terminate + close).

Each test wires an HDV device host onto `vm.system_handle()` (the proxy path for the
virtio-fs tests), attaches a device, then asserts on the guest console. `attach_oop`
additionally uses the `attach_child` helper binary
([`src/bin/attach_child.rs`](../crates/hcs-testvm/src/bin/attach_child.rs)).

---

## Troubleshooting

| Symptom | Likely cause / fix |
|---|---|
| `kernel not found` / `initrd not found` | Artifacts not built. Run `.\test\build-guest-artifacts.ps1` (or `.\test\run-e2e.ps1 -Build`). |
| `docker is not reachable inside WSL` | Enable Docker Desktop's WSL integration for your distro, or install Docker Engine in it. Verify with `wsl docker version`. |
| `wsl.exe not found` | Install WSL2 (`wsl --install`) + a Linux distro, or build the artifacts on any Linux box with Docker and copy `out/` over. |
| Test hangs until timeout, never sees its sentinel | Most often a **stale initramfs** after editing `init` — rebuild artifacts. Otherwise inspect the captured console (`--nocapture` prints it on failure). |
| `HcsStartComputeSystem … 0x8000FFFF` in `attach`/`attach_oop` | Expected — those are the negative spikes (see above). |
| `from_proxy … 0xC0370030` | Only **one** HDV device host is allowed per VM; share one `Arc<DeviceHost>` across devices (this is what `hotplug` stage 2 does). |
| `from_proxy … 0x80070005` (`E_ACCESSDENIED`) + a possible crash | Two device hosts were registered **in one process** — the unsupported topology. Use one device host per process (Model A); in tests, pass `--test-threads=1` (the runner does). |
| Live remove returns `0x80070032` (`ERROR_NOT_SUPPORTED`) | Expected — the platform refuses `FlexibleIov` Remove; devices reclaim at VM teardown ([`share-abi.md`](share-abi.md), [`roadmap.md`](roadmap.md)). |
| Flaky: passes after a couple of retries | Normal — a fraction of boots stall on the early-boot guest-memory aperture (see "Why some tests still retry"). Consistent need for all attempts is worth a look. |

---

## Why these don't run on hosted CI

GitHub-hosted `windows-latest` runners don't expose nested virtualization, so HCS/HDV
can't create a VM. The build, lint, format, header-freshness, and unit-test gates *do*
run there. To get the e2e ladder into CI you'd need a **self-hosted runner on a
Hyper-V-capable Windows host** with the guest artifacts staged (or built in a pre-step);
the same `test\run-e2e.ps1` would drive it. That's tracked as future work in
[`roadmap.md`](roadmap.md).

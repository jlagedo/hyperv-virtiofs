# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

"virtiofsd for Hyper-V." A standalone C-ABI DLL (`hyperv_virtiofs.dll`) that attaches an
[OpenVMM](https://github.com/microsoft/openvmm) **virtio-fs** device to *any* HCS/Hyper-V
guest via the Windows **Host Device Virtualization (HDV)** API — sharing a host directory
into a VM over virtio-fs with no 9p and no in-box device support. It is the open
re-implementation of the device-host glue that ships closed inside WSL's
`wsldevicehost.dll`.

**Windows-only product.** The lower crates compile (inertly) off-Windows so the workspace
checks elsewhere, but nothing runs without Hyper-V.

## Build & test

Prerequisites: Rust 1.95+ (pinned in `rust-toolchain.toml`) and **`protoc`** on `PATH`
(or `PROTOC` env var) — the reused OpenVMM crates pull `mesh → prost → protobuf`, whose
build needs the Protocol Buffers compiler. The OpenVMM crates are git deps pinned to one
revision in the workspace `Cargo.toml`; the first build fetches that large tree.

```pwsh
cargo build --release        # -> target/release/hyperv_virtiofs.dll (+ .dll.lib, .pdb)
cargo test --workspace       # unit tests only; integration tests are #[ignore]
cargo clippy --workspace --all-targets -- -D warnings   # CI gate (warnings = error)
cargo fmt --all --check                                  # CI gate
```

### Regenerating the C header (required after any ABI change)

`include/hyperv_virtiofs.h` is **generated, committed, and CI-verified** — CI fails if it
drifts from the Rust source. After changing any `#[no_mangle]` signature or public ABI
type in `crates/hyperv_virtiofs/src/lib.rs`, regenerate and commit:

```pwsh
cbindgen --config cbindgen.toml --crate hyperv_virtiofs --output include/hyperv_virtiofs.h
```

### Integration tests (live VM — gated)

All tests in `crates/hcs-testvm/tests/` are `#[ignore]` because they need a Hyper-V host
and Rocky Linux kernel/initramfs artifacts (not on CI). The whole e2e tier — prerequisites,
the ladder, the guest↔test sentinel contract, and troubleshooting — is documented in
**`docs/testing.md`**; the reproducible tooling lives in **`test/`**:

```pwsh
.\test\build-guest-artifacts.ps1   # once: build test\guest\out\ (Rocky kernel + initramfs, via WSL+Docker)
.\test\run-e2e.ps1                 # run the ladder, print a PASS/FAIL summary
.\test\run-e2e.ps1 -Test attach_abi   # or one rung
```

Artifact paths resolve via `hcs_testvm::artifact_paths()`: `HVFS_KERNEL` / `HVFS_INITRD`
override, else the in-repo `test/guest/out/` default — so a plain
`cargo test -p hcs-testvm --test attach_abi -- --ignored --nocapture` works after a build,
no env vars needed. The artifacts are git-ignored (built, not committed); the two inputs
that define them — `test/guest/build-rocky-initramfs.sh` and the guest `test/guest/init`
self-test — are committed.

**The guest `init` and the Rust tests are coupled by exact console sentinels** (e.g.
`PROOF_COMPLETE_PASS`, `GUEST_READY`, `HOTPLUG_MOUNT_PASS tag=…`). Changing a sentinel
means updating the consuming test *and* rebuilding the artifacts — see the contract table
in `docs/testing.md`.

Test ladder (each proves one more layer; `docs/` has the design notes):
- `boot.rs` — Rocky boots under HCS, reaches userspace (validates the rig).
- `attach_proxy.rs` — the `ExternalRestricted` FlexibleIov proxy handshake (`docs/hdv-proxy-abi.md`).
- `attach_virtiofs.rs` — the real `VirtioHdvDevice` transport: guest **mounts** and reads a file (device attached *before* start).
- `hotplug.rs` — hot-add a device-per-share to an *already-running* VM (`docs/hotplug-spike.md`).
- `attach_abi.rs` — the shipped front door: drives the exported C functions end-to-end (the authoritative ABI proof).
- `edge_cases.rs` — the C ABI rejects bad share input (`ro`/GUID/JSON/null) against a *live* host; needs only a created (not started) VM, so it's fast.
- `file_selftest.rs` — **green-ladder rung**: data-path integrity + throughput over virtio-fs — multi-MiB write-through integrity (host recomputes the guest's sha256), MB/s, many-files, unicode/nested names, via the guest's opt-in `atelier.fileperf` self-test in `test/guest/init`. Covers up to 64 MiB transfers (`docs/testing.md`).
- `concurrent_processes.rs` — **Model A** proof: two `host_child` processes, each its own device host + VM, hot-add a share and mount concurrently. The supported deployment is **one device host per process** (see `docs/share-abi.md`); driving multiple hosts in one process is out of scope (the in-process proxy path races — `from_proxy` → `0x80070005`).
- `attach.rs` / `attach_oop.rs` — **negative spikes**: they assert success and so *fail by
  design* on current Windows, standing as reproductions of why the proxy path is required.
  Excluded from `run-e2e.ps1` unless `-IncludeNegativeSpikes`.
- `diag_cold_multidevice.rs` — standalone **diagnostic** (passes): asserts two *custom*-class
  cold devices are rejected at power-on (`0xC0350005`), i.e. why the well-known class id is
  required. Not in the green ladder.

Offline **unit tests** (run on CI via `cargo test --workspace`) cover the ABI's
deterministic surface — `hyperv_virtiofs` (`src/lib.rs` `mod tests`: panic guard, null/arg
contracts, `host_json`/`share_json` parsing, request shaping) and `hdv::pci` GUID
parse/format round-trips.

These tests retry attach+boot a few times on purpose: HDV guest-memory apertures are an
evictable cache, so a fraction of boots stall — each retry is a fresh VM.

## Architecture

The product surface is **only** the C ABI in `include/hyperv_virtiofs.h` — five calls,
no consumer-specific concepts (only compute systems, device hosts, shares, tags,
read-only). Bindings live with each *consumer*, not here (Go consumers use
`syscall.NewLazyDLL`, no cgo). `examples/c/main.c` is the authoritative reference;
`examples/go/` is illustrative only.

### Crate layering (bottom-up; lower three are a reusable, device-agnostic HDV toolkit)

| Crate | Responsibility |
|---|---|
| `hdv-sys` | Raw FFI to the HDV API (`vmdevicehost.dll`): device hosts, guest-memory apertures, doorbells, the proxy ABI. |
| `hdv` | Safe RAII over `hdv-sys` (`DeviceHost`, `Device`, `Aperture`, `pci`, `proxy`). Knows nothing about virtio. |
| `virtio-hdv` | Adapts OpenVMM's public `VirtioPciDevice`/`VirtioFsDevice` onto HDV: implements `hdv::pci::PciOps`, backing guest memory ← apertures, MSI-X ← `HdvDeliverGuestInterrupt`, PCI config/BAR MMIO ← HDV vtable callbacks. Device-neutral transport. |
| `hyperv_virtiofs` | The `cdylib` (+ `rlib` for in-process tests). Wires OpenVMM's `virtiofs` onto `virtio-hdv` and exposes the C ABI. **The whole product.** |
| `hcs-sys` | Raw FFI to HCS (`computecore.dll`): open/modify/close compute systems, operations. |
| `hcs-testvm` | Test-only rig: stands up a throwaway Rocky VM via HCS with direct kernel boot, captures serial over a named pipe, hands a live `HCS_SYSTEM` to HDV. Not shipped. |

`virtio-hdv` is the open re-implementation of one internal WSL file (`virtio_hdv.rs`) over
otherwise-public OpenVMM crates. The OpenVMM virtio device model, queue/guest-memory
machinery, and FUSE server are **reused** (pinned git deps), not reimplemented.

### Object model (`docs/share-abi.md`, ABI v2)

- A `hvfs_host` is **one HDV device host** per compute system (the platform rejects a
  second), registered via the `ExternalRestricted` proxy path **before the VM starts**.
- N `hvfs_share`s ride it, each **one virtio-fs device == one shared directory**,
  **hot-added at runtime** (after start) via `HcsModifyComputeSystem` Add of a
  `FlexibleIov` slot. Multiple shares coexist via the well-known virtio-fs class id +
  caller-supplied unique `instance_id`.
- ABI flow: `hvfs_host_open` → (caller starts VM) → `hvfs_add_share` →
  `hvfs_share_instance_id` → `hvfs_remove_share` → `hvfs_host_close`.

## ABI contract rules (load-bearing — do not weaken)

- Every fn returns `i32`: `0` = OK, `< 0` = error. On error, `hvfs_last_error()` holds a
  **thread-local** message (borrowed, valid until the next call on that thread).
- Handles (`hvfs_host*`, `hvfs_share*`) are opaque, never passed by value.
- **Every `const char*` is borrowed** — caller owns inputs, outputs are thread-local or
  via the logger callback. Nothing crosses the allocator boundary; there is no `free`.
- **No Rust panic ever crosses the boundary**: every entry point runs inside `guard()`
  (`catch_unwind`) and returns `HVFS_ERR_PANIC`. `panic = "unwind"` is pinned in *both*
  release and dev profiles in the workspace `Cargo.toml` precisely to keep this contract.
- The ABI won't claim a guarantee it can't keep: `ro: true` returns
  `HVFS_ERR_NOT_IMPLEMENTED`; live `FlexibleIov` Remove returns `HVFS_ERR_UNSUPPORTED`
  (the platform refuses it — devices are reclaimed at VM teardown, as WSL relies on).
- Bump `HVFS_ABI_VERSION` (currently `2`) on any breaking ABI change.
- In `hvfs_host`, **struct field order is drop order and is load-bearing**: shares' HDV
  devices tear down before the device host, before the COM support object, before the HCS
  handle closes.

## Logging & diagnostics (the pattern to follow)

Follow OpenVMM's convention — **structured `tracing`, never ad-hoc prints, and the emitter
never decides the destination**:

- **Emit with `tracing` macros**, not `println!`/`eprintln!`: `tracing::info!/warn!/error!/
  debug!/trace!` with **key-value fields**, not pre-formatted strings — e.g.
  `tracing::warn!(gpa, len, "aperture map failed")`.
- **Rate-limit anything a guest can trigger repeatedly** so it can't spam the log — use the
  local `crate::ratelimit::warn_ratelimited!` in `virtio-hdv` (our stand-in for OpenVMM's
  `tracelimit`). Bare `tracing::warn!` is for one-shot/host-side events.
- **`println!`/`eprintln!` is only for**: user-facing CLI/program output, `build.rs` cargo
  directives, dev tooling, and tests (e.g. `hcs-testvm`). **Never** as the logging mechanism
  for library/runtime code.
- **Routing is a separate concern, set once.** Call sites only emit; *where* logs go is
  decided by the `tracing_subscriber` the `hyperv_virtiofs` cdylib installs
  (`crates/hyperv_virtiofs/src/logging.rs`): the `hvfs_set_logger` C callback, and stderr
  when a dev env var is set (`VIRTIO_HDV_TRACE` firehose, `VIRTIO_HDV_APERTURE_STATS` cache
  stats; otherwise `RUST_LOG`, default info). Adding a sink never touches a call site.

## Known platform constraints

- Live share **removal** is platform-blocked (`FlexibleIov` Remove returns
  `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)` = `0x80070032`); reclaim happens at teardown.
- No DAX yet: `shmem_size = 0`, so no shared-memory BAR. Guest memory is reached via an
  **on-demand per-range aperture cache** (`crates/virtio-hdv/src/mem.rs`); `max_address` is
  derived as `4 GiB + ram_size` (`ram_size_to_max_gpa`) to admit RAM remapped above the 4 GiB
  MMIO hole. An interrupt re-arm net + boot retry guard a rarer boot-stall/EVENT_IDX window.
- **One device host per process (Model A).** The DLL registers a device host in the calling
  process; the supported deployment runs each VM's device host in its own process (as WSL
  does). Registering multiple device hosts in one process is unsupported — the in-process
  proxy registration races the closed platform (`HdvInitializeDeviceHostForProxy` →
  `E_ACCESSDENIED 0x80070005`), and we deliberately don't lock around it (it would imply a
  multi-host-per-process safety the platform's teardown/IPC/aperture paths don't give us).
  See `docs/share-abi.md` ("Deployment model").

Open work (live removal, `ro` enforcement, self-hosted e2e CI) is tracked in
`docs/roadmap.md`.

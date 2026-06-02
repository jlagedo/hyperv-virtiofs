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
and Rocky Linux kernel/initramfs artifacts (not on CI). Run one explicitly with env vars
pointing at the guest artifacts:

```pwsh
$env:HVFS_KERNEL="E:\dev\spike\out\vmlinuz"
$env:HVFS_INITRD="E:\dev\spike\out\initramfs.cpio.gz"
cargo test -p hcs-testvm --test attach_abi -- --ignored --nocapture
```

Test ladder (each proves one more layer; `docs/` has the design notes):
- `boot.rs` — Rocky boots under HCS, reaches userspace (validates the rig).
- `attach_proxy.rs` — the `ExternalRestricted` FlexibleIov proxy handshake (`docs/hdv-proxy-abi.md`).
- `attach_oop.rs` — the in-process-vs-child-process experiment that established the proxy model is required.
- `attach_virtiofs.rs` — the real `VirtioHdvDevice` transport: guest **mounts** and reads a file (device attached *before* start).
- `hotplug.rs` — hot-add a device-per-share to an *already-running* VM (`docs/hotplug-spike.md`).
- `attach_abi.rs` — the shipped front door: drives the exported C functions end-to-end (the authoritative ABI proof).

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

## Known platform constraints

- Live share **removal** is platform-blocked (`FlexibleIov` Remove returns
  `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)` = `0x80070032`); reclaim happens at teardown.
- No DAX yet: `shmem_size = 0`, so no shared-memory BAR. Guest-memory coherence currently
  relies on a persistent aperture + interrupt re-arm + boot retry to mask aperture-cache
  staleness.
- `hvfs_set_logger` is a no-op stub today.

Open work (live removal, `ro` enforcement, caller-supplied GUIDs, logger wiring, coherent
guest memory) is tracked in `docs/roadmap.md`.

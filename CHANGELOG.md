# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The project is pre-1.0 (`0.x`): the C ABI is versioned separately via
`HVFS_ABI_VERSION` (currently `2`), and minor releases may make breaking changes.

## [Unreleased]

## [0.1.0] - 2026-06-03

First public release: `hyperv_virtiofs.dll`, a Windows-only C-ABI library that
attaches an OpenVMM virtio-fs device to any HCS/Hyper-V guest via the Host Device
Virtualization (HDV) API — an open re-implementation of the glue in WSL's closed
`wsldevicehost.dll`.

### Added

- **C ABI v2 (`HVFS_ABI_VERSION = 2`)** — the host/share object model
  (`include/hyperv_virtiofs.h`): `hvfs_host_open`, `hvfs_add_share`,
  `hvfs_share_instance_id`, `hvfs_remove_share`, `hvfs_host_close`,
  `hvfs_last_error`, `hvfs_set_logger`, `hvfs_abi_version`.
  - One `hvfs_host` per compute system (HDV device host registered via the
    `ExternalRestricted` proxy path before VM start).
  - N `hvfs_share`s, each a virtio-fs device == one directory, hot-added after
    start via `HcsModifyComputeSystem` Add of a `FlexibleIov` slot.
- **HDV toolkit crates** — `hdv-sys` (raw FFI), `hdv` (safe RAII over device
  host / instance / apertures / doorbells / PCI), `virtio-hdv` (OpenVMM virtio
  transport over HDV: aperture-cached guest memory, MSI-X, BAR MMIO).
- **Reuse of OpenVMM** virtio / virtiofs / pci_core / guestmem crates (pinned git
  rev), rather than reimplementing the device model or FUSE server.
- **Guest memory via an on-demand aperture cache** (no DAX; `shmem_size = 0`),
  with `max_address = 4 GiB + ram_size` so RAM remapped above the 4 GiB MMIO hole
  is addressable.
- **Structured `tracing` diagnostics**, routed through the caller-supplied
  `hvfs_set_logger` callback (and stderr when `VIRTIO_HDV_TRACE` /
  `VIRTIO_HDV_APERTURE_STATS` / `RUST_LOG` is set). Guest-triggerable events are
  rate-limited.
- **Panic-safe boundary** — entry points run under `guard()` → `HVFS_ERR_PANIC`;
  HDV callbacks under `guard_hr()` → `E_FAIL`. No panic can abort the host
  process; `panic = "unwind"` is pinned in both profiles.
- **End-to-end validation** — a live-VM test ladder (gated, `#[ignore]`) that
  boots a throwaway Rocky Linux guest via HCS and proves cold mount → hot add →
  C ABI round-trip, including edge and concurrency rungs (`docs/testing.md`).
- **Documentation** — README with architecture diagram, `docs/share-abi.md`
  (ABI contract), `docs/hdv-proxy-abi.md`, `docs/hdv-aperture-internals.md`,
  `docs/hotplug-spike.md`, `docs/roadmap.md`; C and Go consumer examples.
- **CI** — fmt / clippy (`-D warnings`) / build / test gates, a cbindgen
  header-freshness check, and an automated release job that publishes the DLL,
  import lib, PDB, and header on `v*` tags.

### Known limitations

- **Live share removal is platform-blocked** (`FlexibleIov` Remove →
  `0x80070032`); `hvfs_remove_share` returns `HVFS_ERR_UNSUPPORTED`. Reclaim
  happens at VM teardown.
- **Read-only shares are not implemented** — `ro: true` returns
  `HVFS_ERR_NOT_IMPLEMENTED`.
- **One device host per process** (Model A): multiple hosts in one process is
  unsupported (in-process proxy registration races). Run each VM's device host in
  its own process.
- **No DAX** — guest access goes through the aperture cache, not a shared-memory
  window.

[Unreleased]: https://github.com/jlagedo/hyperv-virtiofs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jlagedo/hyperv-virtiofs/releases/tag/v0.1.0

# CLAUDE.md

## What this is

"virtiofsd for Hyper-V": a **Windows-only** C-ABI DLL (`hyperv_virtiofs.dll`) that attaches
an OpenVMM virtio-fs device to any HCS/Hyper-V guest via the **Host Device Virtualization
(HDV)** API. Open re-implementation of the glue in WSL's closed `wsldevicehost.dll`. The
OpenVMM virtio/virtiofs crates are **reused** (pinned git deps), not reimplemented.

Lower crates compile off-Windows (so `cargo check` works elsewhere) but nothing runs
without Hyper-V.

## Build & test

- **`protoc` must be on `PATH`** (or set `PROTOC`) — the build won't compile without it.
- Toolchain is pinned in `rust-toolchain.toml`.

```pwsh
cargo build --release        # -> target/release/hyperv_virtiofs.dll
cargo test --workspace       # unit tests only; integration tests are #[ignore]
cargo clippy --workspace --all-targets -- -D warnings   # CI gate (warnings = error)
cargo fmt --all --check                                  # CI gate
```

**After any ABI change** (a `#[no_mangle]` signature or public type in
`crates/hyperv_virtiofs/src/lib.rs`), regenerate the committed header — CI fails on drift:

```pwsh
cbindgen --config cbindgen.toml --crate hyperv_virtiofs --output include/hyperv_virtiofs.h
```

### Integration tests (live VM, gated)

`crates/hcs-testvm/tests/` are `#[ignore]` (need a Hyper-V host + guest artifacts). Full
tier in **`docs/testing.md`**.

```pwsh
.\test\build-guest-artifacts.ps1   # once: build guest kernel+initramfs (WSL+Docker); git-ignored
.\test\run-e2e.ps1                 # run the ladder; -Test <name> for one rung
```

- The guest `test/guest/init` and the Rust tests are coupled by exact console sentinels
  (`PROOF_COMPLETE_PASS`, `GUEST_READY`, …). Changing one means updating the test *and*
  rebuilding artifacts.
- Tests retry attach+boot on purpose (HDV apertures are an evictable cache; some boots
  stall) — don't "fix" the retries.

## Architecture

Product surface is **only** the C ABI in `include/hyperv_virtiofs.h` (five calls). Consumer
bindings live with each consumer, not here; `examples/c/main.c` is authoritative.

Crate layering (bottom-up; lower three are a device-agnostic HDV toolkit):

| Crate | Responsibility |
|---|---|
| `hdv-sys` | Raw FFI to HDV (`vmdevicehost.dll`). |
| `hdv` | Safe RAII over `hdv-sys` (`DeviceHost`, `Device`, `Aperture`, `pci`, `proxy`); no virtio. |
| `virtio-hdv` | Adapts OpenVMM's `VirtioPciDevice`/`VirtioFsDevice` onto HDV (`PciOps`, apertures, MSI-X, BAR MMIO). Device-neutral. |
| `hyperv_virtiofs` | The `cdylib`; wires OpenVMM's `virtiofs` onto `virtio-hdv`, exposes the C ABI. **The product.** |
| `hcs-sys` | Raw FFI to HCS (`computecore.dll`). |
| `hcs-testvm` | Test-only rig (throwaway Rocky VM via HCS). Not shipped. |

### Object model (`docs/share-abi.md`, ABI v2)

- One `hvfs_host` = one HDV device host per compute system (platform rejects a second),
  registered via the `ExternalRestricted` proxy path **before the VM starts**.
- N `hvfs_share`s, each one virtio-fs device == one directory, **hot-added after start** via
  `HcsModifyComputeSystem` Add of a `FlexibleIov` slot. They coexist via the well-known
  class id + a caller-supplied `instance_id`.
- Flow: `hvfs_host_open` → caller starts VM → `hvfs_add_share` → `hvfs_share_instance_id`
  → `hvfs_remove_share` → `hvfs_host_close`.

## ABI contract rules (do not weaken)

- Every fn returns `i32` (`0` = OK, `< 0` = error); `hvfs_last_error()` holds a thread-local
  borrowed message.
- Handles are opaque, never passed by value. Every `const char*` is borrowed — nothing
  crosses the allocator boundary, there is no `free`.
- **No panic crosses the boundary**: entry points run in `guard()` → `HVFS_ERR_PANIC`.
  `panic = "unwind"` is pinned in both profiles to keep this — don't change it.
- Don't claim guarantees we can't keep: `ro: true` → `HVFS_ERR_NOT_IMPLEMENTED`; live Remove
  → `HVFS_ERR_UNSUPPORTED`.
- Bump `HVFS_ABI_VERSION` on any breaking change.
- In `hvfs_host`, **struct field order is drop order** (shares → device host → COM object →
  HCS handle) — load-bearing.

## Trust boundaries & safety

The guest is untrusted; guest-triggerable paths (config/BAR MMIO, DMA ranges, MSI, queues)
must never panic out of bounds.

- **Never abort the host** — we're a DLL in someone else's process. Entry points → `guard()`
  → `HVFS_ERR_PANIC`; HDV callbacks → `guard_hr()` → `E_FAIL`. Never use `panic = "abort"`.
- **No `.unwrap()`/`.expect()` on boundary data.** Bounds-check guest DMA; unknown protocol
  values are a no-op, not a panic. `Mutex::lock().unwrap()` is fine on a guarded path, not on
  the un-guarded worker threads (re-arm, poll, MSI).
- **Typed errors, hand-rolled** (`hdv::Error`/`Result`, `AttachError`, C `i32` +
  `hvfs_last_error()`). Don't add `thiserror`/`anyhow`.
- **Isolate `unsafe`**: raw FFI in `*-sys`, safe RAII in `hdv`; every `unsafe` gets a
  `# Safety` comment.
- Prefer hand-rolled utilities over new external crates.

## Logging & diagnostics

Structured `tracing`, never ad-hoc prints; the emitter never picks the destination.

- Emit `tracing` macros with key-value fields (`tracing::warn!(gpa, len, "...")`), not
  `println!`/`eprintln!`.
- Rate-limit guest-triggerable events with `crate::ratelimit::warn_ratelimited!`.
- `println!`/`eprintln!` only in CLI output, `build.rs`, dev tooling, tests.
- Routing is set once in `crates/hyperv_virtiofs/src/logging.rs` (the `hvfs_set_logger`
  callback + stderr when `VIRTIO_HDV_TRACE`/`VIRTIO_HDV_APERTURE_STATS`/`RUST_LOG` is set).

## Known platform constraints

- Live share **removal** is platform-blocked (`FlexibleIov` Remove → `0x80070032`); reclaim
  happens at VM teardown.
- No DAX (`shmem_size = 0`): guest memory via an on-demand aperture cache
  (`crates/virtio-hdv/src/mem.rs`). `max_address = 4 GiB + ram_size` (`ram_size_to_max_gpa`)
  to admit RAM above the 4 GiB MMIO hole — **don't set it to flat `ram_size`**.
- **One device host per process (Model A):** each VM's device host runs in its own process.
  Multiple hosts in one process is unsupported (in-process proxy registration races →
  `E_ACCESSDENIED 0x80070005`); we deliberately don't lock around it. See `docs/share-abi.md`.

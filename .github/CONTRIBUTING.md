# Contributing

Thanks for your interest in `hyperv-virtiofs` — an open virtio-fs device host for
Hyper-V/HCS guests over the Windows HDV API. This guide covers how to build, test,
and propose changes.

## Before you start

- **Open work lives in one place.** Everything left to develop, confirm, or unblock
  is tracked in [`docs/roadmap.md`](../docs/roadmap.md). Check it before filing a
  feature request or starting work — your idea may already be scoped there.
- **The contract is the C ABI.** The public surface is
  [`include/hyperv_virtiofs.h`](../include/hyperv_virtiofs.h), generated from the Rust
  source. Changes that alter it are deliberate and reviewed carefully (see below).
- **For anything non-trivial, open an issue first.** A short discussion saves a wasted
  PR, especially for ABI, platform, or design changes.

## Prerequisites

- A **Rust 1.95+** toolchain (`x86_64-pc-windows-msvc`). The pinned version is in
  [`rust-toolchain.toml`](../rust-toolchain.toml).
- **`protoc`** — the reused OpenVMM crates pull `mesh → prost → protobuf`, whose build
  needs the Protocol Buffers compiler. Install it and put it on `PATH`, or set the
  `PROTOC` env var to the binary.
- A **Windows** host. The build and unit tests run on `windows-latest` in CI; the
  end-to-end attach tests additionally need Hyper-V/HCS and a guest image.

The first build fetches the pinned OpenVMM tree (large) and compiles it.

## Build, test, and lint

These are exactly the gates CI runs (`.github/workflows/ci.yml`). Run them locally
before opening a PR:

```pwsh
cargo fmt --all --check                                   # formatting
cargo clippy --workspace --all-targets -- -D warnings     # lints (warnings are errors)
cargo build --workspace --release                         # build the cdylib + crates
cargo test --workspace                                    # tests
```

If you changed anything that affects the ABI, **regenerate the header** and commit the
result — CI fails if the committed header drifts from the Rust source:

```pwsh
cbindgen --config cbindgen.toml --crate hyperv_virtiofs --output include/hyperv_virtiofs.h
```

## Where things live

| Crate | Responsibility |
|---|---|
| `hdv-sys` | Raw FFI to the HDV API. |
| `hdv` | Safe RAII over `hdv-sys`. Device-agnostic. |
| `virtio-hdv` | OpenVMM virtio transport carried over HDV. Device-neutral. |
| `hyperv_virtiofs` | The `cdylib`: wires OpenVMM's `virtiofs` onto `virtio-hdv`; exposes the C ABI. |
| `hcs-sys` / `hcs-testvm` | HCS bindings and the end-to-end attach test harness. |

The lower three crates are a reusable HDV device toolkit; virtio-fs is the first device
on top. See the README's *Crate layering* section for the full rationale.

## Coding conventions

- **Match the surrounding code** — naming, comment density, and idiom. Read the file
  you're editing before changing it.
- **No panic crosses the C ABI.** Every ABI entry point runs under `catch_unwind` and
  returns an error code. Keep it that way; don't add a path that can unwind across the
  boundary.
- **Be honest in the contract.** A capability the code can't yet keep is *refused*, not
  faked — e.g. `ro: true` returns `HVFS_ERR_NOT_IMPLEMENTED` rather than silently
  mounting read-write. Prefer an explicit error over an unfulfilled promise.
- **Borrowed strings.** Every `const char*` returned across the ABI is borrowed; callers
  must not `free` it. Don't change that ownership model without an ABI bump.

## Changing the ABI

The shipped DLL + header is a contract consumers **pin**. If your change touches
`include/hyperv_virtiofs.h`:

1. Open an issue describing the change and why it's needed.
2. Update the Rust source and regenerate the header (above).
3. Bump `hvfs_abi_version` if the change is not backward-compatible, and note it in the
   PR and [`docs/share-abi.md`](../docs/share-abi.md).

## Submitting a pull request

1. Branch off `main`.
2. Make the change; keep commits focused. This repo uses
   [Conventional Commits](https://www.conventionalcommits.org/) (e.g.
   `feat(abi): …`, `fix(virtio-hdv): …`, `docs(readme): …`) — match the existing log.
3. Ensure all CI gates pass locally.
4. Open the PR with a clear description of *what* changed and *why*, and link any
   related issue. If behavior changed, say so plainly.

## Reporting bugs and requesting features

Use the issue templates. For anything security-sensitive, follow
[`SECURITY.md`](SECURITY.md) instead of opening a public issue.

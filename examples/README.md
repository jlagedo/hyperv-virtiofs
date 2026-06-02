# Examples

Reference consumers of the `hyperv_virtiofs.dll` C ABI. The public contract is
[`include/hyperv_virtiofs.h`](../include/hyperv_virtiofs.h); these show how to drive it.

> Language bindings deliberately do **not** live in this repo — the agnostic contract is
> the C ABI, and each consumer binds it natively. Copy the pattern that fits your host;
> don't import these as a module.

## `c/` — authoritative C reference

The canonical, build-against-the-header example. Demonstrates the full v2 lifecycle:
version check → `hvfs_host_open` (before VM start) → `hvfs_add_share` (after start) →
`hvfs_remove_share` → `hvfs_host_close`.

```pwsh
cargo build --release   # produces target/release/hyperv_virtiofs.dll.lib
cl /I ..\..\include main.c ..\..\target\release\hyperv_virtiofs.dll.lib
```

## `go/` — illustrative Go snippet

Shows the intended Go binding shape: load the DLL with `syscall.NewLazyDLL` (**no cgo**)
and call the ABI directly, the same way a Windows host binds `computecore.dll`. It is a
**pattern to copy**, not a published module.

```pwsh
# from examples/go, with hyperv_virtiofs.dll on the DLL search path
go run .
```

## Conventions every consumer should follow

- **Check the ABI version at load** (`hvfs_abi_version()` vs `HVFS_ABI_VERSION`) and
  refuse on mismatch.
- **Load the DLL by absolute path, never a bare name** — consumers are typically
  elevated services, and a bare name invites DLL preloading. See
  [Go's Windows DLL guidance](https://go.dev/wiki/WindowsDLLs).
- **Treat every returned `const char*` as borrowed** — do not free it, and copy it out
  before the next ABI call on the same thread (e.g. `hvfs_last_error`).
- **Call `hvfs_host_open` before starting the compute system; `hvfs_add_share` after.**

These examples target a real compute system id; against a system that isn't set up,
calls will return an error code plus `hvfs_last_error()` text — that's expected.

# `test/` — end-to-end suite

Reproducible end-to-end tests that boot a real Rocky Linux VM under Hyper-V and drive the
stack through to the C ABI. **Full guide: [`docs/testing.md`](../docs/testing.md).**

```pwsh
.\test\build-guest-artifacts.ps1   # once: build the guest kernel + initramfs
.\test\run-e2e.ps1                 # run the ladder, print PASS/FAIL
```

## Layout

| Path | What |
|---|---|
| `guest/init` | the guest PID-1 **self-test** — prints the console sentinels the Rust tests assert on |
| `guest/build-rocky-initramfs.sh` | builds `vmlinuz` + `initramfs.cpio.gz` in a Rocky container (Linux/WSL) |
| `guest/out/` | built artifacts (git-ignored; rebuild, don't commit) |
| `build-guest-artifacts.ps1` | Windows wrapper: drives the build via WSL + Docker |
| `run-e2e.ps1` | runs the `#[ignore]`d ladder in `crates/hcs-testvm/tests/`, summarizes results |

The Rust test bodies live in [`crates/hcs-testvm/tests/`](../crates/hcs-testvm/tests/);
the rig that boots the VM is [`crates/hcs-testvm/src/lib.rs`](../crates/hcs-testvm/src/lib.rs).
Two of them go beyond the basic ladder: `edge_cases.rs` checks the C ABI's rejection paths
against a live host, and `file_selftest.rs` validates large-file write-through integrity,
throughput, many-files, and unicode names (via the `atelier.fileperf` self-test in
`guest/init`). Offline **unit tests** for the ABI + GUID helpers run on CI with
`cargo test --workspace`.

> The guest `init` and the Rust tests are **coupled by exact console strings**. Change a
> sentinel in one place and you must change the other *and* rebuild the artifacts. See the
> sentinel contract in [`docs/testing.md`](../docs/testing.md#the-console-sentinel-contract).

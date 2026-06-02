<!-- Thanks for contributing! See .github/CONTRIBUTING.md before opening. -->

## What & why

<!-- What does this change, and why is it needed? Link any related issue. -->

Closes #

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Documentation
- [ ] Refactor / internal

## Checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo build --workspace --release` passes
- [ ] `cargo test --workspace` passes
- [ ] Commits follow Conventional Commits

## ABI impact

- [ ] **This change does not touch the C ABI.**
- [ ] It does — I regenerated `include/hyperv_virtiofs.h` with cbindgen and committed it.
- [ ] It is a breaking change — I bumped `hvfs_abi_version` and noted it in `docs/share-abi.md`.

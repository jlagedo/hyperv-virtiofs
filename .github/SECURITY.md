# Security Policy

`hyperv-virtiofs` ships a native Windows DLL that runs a **virtual device host** for
Hyper-V/HCS guests and maps **host directories** into VMs over virtio-fs. It sits on a
trust boundary between a host process and a guest, so security reports are taken
seriously.

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.**

Instead, report it privately through GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability):
open the repository's **Security** tab and choose **Report a vulnerability**.

Please include:

- A description of the issue and its impact.
- Steps to reproduce, or a proof of concept.
- The affected version (release tag or commit) and your Windows build number.

We'll acknowledge your report, investigate, and keep you updated on remediation. Please
give us a reasonable window to release a fix before any public disclosure.

## Scope

Relevant to this project's threat model:

- **Host ↔ guest boundary** — anything that lets a guest read or write outside its
  declared share path, escape a (future) read-only mount, or reach host memory it
  shouldn't through the HDV aperture / DMA path.
- **DLL loading** — consumers should load the DLL by **absolute path**, never a bare
  name (see the README). Reports of preloading/hijacking exposure are in scope.
- **ABI misuse leading to memory unsafety** — note that no Rust panic is allowed to
  cross the C ABI; a path that aborts or corrupts the host process is a bug.

Out of scope: vulnerabilities in the upstream
[OpenVMM](https://github.com/microsoft/openvmm) crates themselves (report those
upstream) and in the Windows HDV/HCS platform (report to Microsoft).

## Supported versions

This project is pre-1.0 (`0.x`). Security fixes target the latest release and `main`.
Pin to a tagged release and watch the repository for security advisories.

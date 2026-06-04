#!/bin/bash
# Repack out/initramfs.cpio.gz with the current ./init (which carries run_perfbench)
# into out/initramfs.perfbench.cpio.gz, for the performance baseline (perf_baseline.rs).
#
# This is a pure cpio/gzip repack — NO Docker. The stock kernel (out/vmlinuz) and rootfs
# already exist; we only need to swap in the updated /init, so we unpack the existing
# initramfs, replace ./init, and repack. Must run as root (initramfs holds device nodes):
#   wsl -d <distro> -u root -- bash /mnt/<drive>/.../test/guest/repack-perfbench-initramfs.sh
# Driven by test/run-perf-baseline.ps1. See docs/testing.md.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
SRC="$SCRIPT_DIR/out/initramfs.cpio.gz"
INIT="$SCRIPT_DIR/init"
OUT="$SCRIPT_DIR/out/initramfs.perfbench.cpio.gz"

[ -f "$SRC" ]  || { echo "stock initramfs not found: $SRC (build it first)" >&2; exit 1; }
[ -f "$INIT" ] || { echo "guest init not found: $INIT" >&2; exit 1; }
grep -q run_perfbench "$INIT" || { echo "init lacks run_perfbench — nothing to repack" >&2; exit 1; }

# Work on disk-backed /var/tmp (not tmpfs) so the ~2 GiB extract doesn't pressure RAM.
WORK="$(mktemp -d -p /var/tmp pbinitramfs.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/root"
cd "$WORK/root"

echo "extracting $SRC"
zcat "$SRC" | cpio -idm --quiet
echo "files extracted: $(find . | wc -l)"

install -m0755 "$INIT" ./init
echo "init swapped (run_perfbench present: $(grep -c run_perfbench ./init))"

echo "repacking -> $OUT"
find . | cpio --quiet -o -H newc | gzip -1 > "$OUT"
ls -la "$OUT"
echo "DONE"

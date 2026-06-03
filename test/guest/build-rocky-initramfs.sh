#!/bin/bash
# Builds the self-testing Rocky Linux direct-boot artifacts the end-to-end test
# ladder boots (see docs/testing.md). Output, next to this script under out/:
#   out/vmlinuz             - stock Rocky kernel (bzImage, as shipped by the image)
#   out/initramfs.cpio.gz   - the full Rocky rootfs + our ./init self-test, as initramfs
#
# Runs entirely inside a throwaway Docker container, so the only host requirement is
# a working `docker` CLI (Docker Desktop / Docker Engine). Reproducible on any clone:
# nothing here is machine-specific. Drive it from Windows via
# test/build-guest-artifacts.ps1, or run it directly under Linux/WSL.
#
# Overridable via env:
#   HVFS_ROCKY_IMAGE   container image to harvest the kernel + modules from
#                      (default: rockylinux/rockylinux:10)
#   HVFS_GUEST_INIT    path to the guest init script   (default: ./init beside this script)
#   HVFS_GUEST_OUT     output directory                (default: ./out  beside this script)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
IMAGE="${HVFS_ROCKY_IMAGE:-rockylinux/rockylinux:10}"
INIT="${HVFS_GUEST_INIT:-$SCRIPT_DIR/init}"
OUT="${HVFS_GUEST_OUT:-$SCRIPT_DIR/out}"
CONTAINER="hvfs-rockyspike-$$"

[ -f "$INIT" ] || { echo "guest init script not found: $INIT" >&2; exit 1; }
command -v docker >/dev/null || { echo "docker not found on PATH" >&2; exit 1; }
mkdir -p "$OUT"

echo "=== building guest artifacts ==="
echo "    image:  $IMAGE"
echo "    init:   $INIT"
echo "    out:    $OUT"

cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

docker rm -f "$CONTAINER" 2>/dev/null || true
docker run -d --name "$CONTAINER" "$IMAGE" sleep infinity >/dev/null
docker cp "$INIT" "$CONTAINER:/init.real"
docker exec "$CONTAINER" bash -c '
  set -e
  dnf -y install --setopt=install_weak_deps=False \
      kernel-core kernel-modules kernel-modules-core util-linux kmod findutils gzip cpio file >/dev/null
  KVER=$(ls /lib/modules | head -1)
  echo "KVER=$KVER"
  install -m0755 /init.real /init
  if [ -f /lib/modules/$KVER/vmlinuz ]; then cp /lib/modules/$KVER/vmlinuz /vmlinuz; else cp /boot/vmlinuz-$KVER /vmlinuz; fi
  file /vmlinuz
  ls -la /lib/modules/$KVER/kernel/fs/fuse/ 2>/dev/null || true
  cd /
  find . -xdev -path ./init.real -prune -o -print0 \
    | cpio --null --quiet -o -H newc | gzip -1 > /initramfs.cpio.gz
  echo "--- artifact sizes ---"; ls -la /vmlinuz /initramfs.cpio.gz
'
docker cp "$CONTAINER:/vmlinuz"           "$OUT/vmlinuz"
docker cp "$CONTAINER:/initramfs.cpio.gz" "$OUT/initramfs.cpio.gz"
echo "=== DONE; artifacts in $OUT ==="
ls -la "$OUT"

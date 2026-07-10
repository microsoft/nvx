#!/usr/bin/env bash
#
# Builds the microvm Linux artifacts (PVH vmlinux + Alpine initramfs) with Docker and exports
# them onto the host with `docker build --output`. This is the portable counterpart to
# scripts/build-linux-artifacts.ps1, usable wherever Docker (Linux engine) is available.
#
#   DEST=build KVER=6.18.38 AVER=3.24.1 ABRANCH=v3.24 scripts/build-linux-artifacts.sh
#
# Produces $DEST/vmlinux and $DEST/initramfs.cpio.gz.
#
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${DEST:-build}"
KVER="${KVER:-6.18.38}"
AVER="${AVER:-3.24.1}"
ABRANCH="${ABRANCH:-v3.24}"

command -v docker >/dev/null 2>&1 || {
    echo "!! docker not found on PATH" >&2
    exit 1
}

echo ">> building Linux artifacts into '$DEST' (kernel $KVER, Alpine $AVER)"
docker build \
    -f "$REPO/docker/Dockerfile" \
    --target artifacts \
    --build-arg "KVER=$KVER" \
    --build-arg "AVER=$AVER" \
    --build-arg "ABRANCH=$ABRANCH" \
    --output "type=local,dest=$DEST" \
    "$REPO"

echo ">> done: $DEST/vmlinux, $DEST/initramfs.cpio.gz"

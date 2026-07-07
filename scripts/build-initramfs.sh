#!/usr/bin/env bash
#
# Builds the RAM initramfs from an Alpine mini root filesystem. The guest boots entirely
# from this image (no block device). PID 1 is a small /init that mounts the pseudo
# filesystems, prints a recognizable banner, and execs an interactive shell on the console.
#
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
AVER="${AVER:-3.24.1}"
ABRANCH="${ABRANCH:-v3.24}"
WORK="${WORK:-$HOME/build/initramfs}"
OUT="${OUT:-$HOME/build/initramfs.cpio.gz}"
ROOT="$WORK/root"

mkdir -p "$WORK"
cd "$WORK"

TARBALL="alpine-minirootfs-${AVER}-x86_64.tar.gz"
if [ ! -f "$TARBALL" ]; then
    echo ">> downloading $TARBALL"
    curl -fL --retry 3 -O \
        "https://dl-cdn.alpinelinux.org/alpine/${ABRANCH}/releases/x86_64/${TARBALL}"
fi

rm -rf "$ROOT"
mkdir -p "$ROOT"
tar -xzf "$TARBALL" -C "$ROOT"

# Install the init (PID 1) from the alpine/ component of this repo.
install -m 0755 "$REPO/alpine/init" "$ROOT/init"

( cd "$ROOT" && find . | cpio --quiet -o -H newc | gzip -9 > "$OUT" )
echo ">> built $OUT ($(du -h "$OUT" | cut -f1))"

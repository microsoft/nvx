#!/usr/bin/env bash
#
# Builds a RAM initramfs containing a full CPython interpreter and a hello-world app, used to
# demonstrate snapshot/restore. Python is installed into the Alpine mini root filesystem with
# Alpine's own static-ish `apk` (invoked through the musl loader, so it runs on a non-Alpine
# host). Work entirely under $HOME/build.
#
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
AVER="${AVER:-3.24.1}"
ABRANCH="${ABRANCH:-v3.24}"
WORK="${WORK:-$HOME/build/initramfs-python}"
OUT="${OUT:-$HOME/build/initramfs-python.cpio.gz}"
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

# Install python3 + the pandas/numpy stack into the target root. `apk` is a musl binary, so
# invoke it through the rootfs's dynamic loader with the rootfs libraries on the search path --
# this works on a glibc host without chroot/root.
echo ">> installing python3 + pandas/numpy into the rootfs"
LD="$ROOT/lib/ld-musl-x86_64.so.1"
LD_LIBRARY_PATH="$ROOT/lib:$ROOT/usr/lib" "$LD" "$ROOT/sbin/apk" \
    --root "$ROOT" --no-cache --no-interactive add python3 py3-numpy py3-pandas

install -m 0755 "$REPO/alpine/init.python" "$ROOT/init"
install -m 0755 "$REPO/alpine/hello.py" "$ROOT/hello.py"
install -m 0755 "$REPO/alpine/repl.py" "$ROOT/repl.py"
install -m 0755 "$REPO/alpine/net-hello.py" "$ROOT/net-hello.py"
install -m 0755 "$REPO/alpine/net-pandas.py" "$ROOT/net-pandas.py"

( cd "$ROOT" && find . | cpio --quiet -o -H newc | gzip -9 > "$OUT" )
echo ">> built $OUT ($(du -h "$OUT" | cut -f1))"

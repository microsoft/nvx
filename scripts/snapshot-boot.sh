#!/usr/bin/env bash
#
# Direct-boots an *interactive* Python interpreter straight from a snapshot: restore the VM
# captured at the point a warmed CPython interpreter (alpine/repl.py) requested a snapshot and
# resume it into a live ">>>" prompt on this console, skipping the kernel boot and the entire
# Python startup. Unlike snapshot-demo.sh (a benchmark that always recaptures), this reuses the
# snapshot: it is captured once on first use via a one-off cold boot, so every later
# `make snapshot-boot` drops you at the interpreter prompt in milliseconds.
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/microvm"
KERNEL="${KERNEL:-$HOME/build/vmlinux}"
INITRD="${INITRD:-$HOME/build/initramfs-python.cpio.gz}"
SNAP="${SNAP:-$HOME/build/pyrepl}"
MEM="${MEM:-256}"
# `pyapp=repl.py` tells the initramfs init to run the interactive REPL app rather than the
# hello-world demo. Capture the snapshot with the console quiet so nothing pollutes it.
CMDLINE="console=ttyS0 quiet loglevel=0 reboot=t panic=-1 pyapp=repl.py"

[ -x "$BIN" ] || { echo "build the VMM first: make release"; exit 1; }

# Capture the snapshot on first use: cold-boot the interpreter once and let repl.py request a
# snapshot (control port 0x605), which the VMM captures into $SNAP before exiting. Reused as
# long as $SNAP/{state,mem}.bin exist -- remove the directory to force a fresh capture.
if [ ! -f "$SNAP/state.bin" ] || [ ! -f "$SNAP/mem.bin" ]; then
    [ -f "$KERNEL" ] || { echo "missing kernel: $KERNEL (make kernel)"; exit 1; }
    [ -f "$INITRD" ] || { echo "missing python initramfs: $INITRD (make python-initramfs)"; exit 1; }
    echo ">> no snapshot at $SNAP yet; capturing a warmed interpreter (one-off cold boot)"
    rm -rf "$SNAP"
    timeout 40 "$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" \
        --snapshot "$SNAP" --quiet --cmdline "$CMDLINE" < /dev/null
fi

echo ">> resuming interactive Python interpreter from snapshot $SNAP (Ctrl-D or exit() to quit)"
exec "$BIN" --restore "$SNAP" --mem "$MEM"

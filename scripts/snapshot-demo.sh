#!/usr/bin/env bash
#
# Demonstrates and measures snapshot/restore with the Python pandas/numpy benchmark app:
#   1. cold-boot the VM and time reaching the DataFrame result (kernel boot + Python startup +
#      the pandas/numpy import);
#   2. take a VMM snapshot at the fully warmed point the app requests one (pandas/numpy imported
#      and the DataFrame computation already exercised);
#   3. restore from the snapshot N times and time reaching the DataFrame result.
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/microvm"
KERNEL="${KERNEL:-$HOME/build/vmlinux}"
INITRD="${INITRD:-$HOME/build/initramfs-python.cpio.gz}"
SNAP="${SNAP:-$HOME/build/pysnap}"
MEM="${MEM:-512}"
N="${N:-8}"
MARKER="{'x': 10, 'y': 30}"
CMDLINE="console=ttyS0 quiet loglevel=0 reboot=t panic=-1"

[ -x "$BIN" ]    || { echo "build the VMM first: make release"; exit 1; }
[ -f "$KERNEL" ] || { echo "missing kernel: $KERNEL (make kernel)"; exit 1; }
[ -f "$INITRD" ] || { echo "missing python initramfs: $INITRD (make python-initramfs)"; exit 1; }

median() {
    python3 -c "import sys;v=sorted(float(x) for x in sys.stdin.read().split());\
print('%7.1f ms  (min %.1f, max %.1f, n=%d)'%(v[len(v)//2],v[0],v[-1],len(v)) if v else 'NO DATA')"
}

echo "== cold boot -> pandas result (kernel boot + Python startup + pandas/numpy import) =="
for _ in $(seq 1 "$N"); do
    timeout 40 "$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" \
        --exit-on-boot --quiet --boot-marker "$MARKER" --cmdline "$CMDLINE" 2>&1 \
        | grep -oE 'cold-start: [0-9.]+' | grep -oE '[0-9.]+$'
done | median | sed 's/^/  cold:    /'

echo "== taking snapshot at the fully warmed point (pandas/numpy imported + computation warmed) =="
rm -rf "$SNAP"
timeout 40 "$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" \
    --snapshot "$SNAP" --quiet --cmdline "$CMDLINE" >/dev/null 2>&1
echo "  snapshot: $SNAP ($(du -sh "$SNAP" | cut -f1); mem.bin $(du -h "$SNAP/mem.bin" | cut -f1))"

echo "== restore -> pandas result (resume the warmed interpreter) =="
for _ in $(seq 1 "$N"); do
    timeout 30 "$BIN" --restore "$SNAP" --mem "$MEM" \
        --exit-on-boot --quiet --boot-marker "$MARKER" 2>&1 \
        | grep -oE 'restore: [0-9.]+' | grep -oE '[0-9.]+$'
done | median | sed 's/^/  restore: /'

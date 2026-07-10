#!/usr/bin/env bash
#
# Benchmarks boot-to-interactive-shell for the plain Alpine initramfs two ways, across a range of
# guest RAM sizes: a cold PVH boot (kernel boot + init to the ALPINE-MICROVM-BOOT-OK banner) versus
# a restore from a snapshot captured at the shell-ready point. Prints the median of N runs each and
# the resulting speedup. Requires a built VMM, kernel, and (shellsnap-capable) initramfs.
#
# The snapshot is captured with the "shellsnap" cmdline token, which makes alpine/init ask the VMM
# for a snapshot (I/O port 0x605) just before it prints the banner; on restore the guest resumes
# there and re-emits the banner, so the cold and restore paths are timed to the very same marker.
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/microvm"
KERNEL="${KERNEL:-$HOME/build/vmlinux}"
INITRD="${INITRD:-$HOME/build/initramfs.cpio.gz}"
SNAPDIR="${SNAPDIR:-$HOME/build/shellsnap}"
MEMS="${MEMS:-64 128 256 512}"
N="${N:-15}"
MARKER="ALPINE-MICROVM-BOOT-OK"
CMDLINE="earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1"

[ -x "$BIN" ]    || { echo "build the VMM first: make release"; exit 1; }
[ -f "$KERNEL" ] || { echo "missing kernel: $KERNEL (make kernel)"; exit 1; }
[ -f "$INITRD" ] || { echo "missing initrd: $INITRD (make initramfs)"; exit 1; }

# One cold boot: guest start -> banner. Emits the cold-start time in ms (or nothing on failure).
cold_once() { # $1=mem
    timeout 30 "$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem "$1" \
        --exit-on-boot --quiet --boot-marker "$MARKER" --cmdline "$CMDLINE" < /dev/null 2>&1 \
        | grep -oE 'cold-start: [0-9.]+' | grep -oE '[0-9.]+$' || true
}

# One restore: resume from snapshot -> banner. Emits the restore time in ms (or nothing on failure).
restore_once() { # $1=mem  $2=snapshot-dir
    timeout 30 "$BIN" --restore "$2" --mem "$1" \
        --exit-on-boot --quiet --boot-marker "$MARKER" < /dev/null 2>&1 \
        | grep -oE 'restore: [0-9.]+' | grep -oE '[0-9.]+$' || true
}

# Print a per-size summary (median/min/max, cold fast/slow split, and speedup) from the samples.
# Cold boot is bimodal on this micro-VM: a subset of cold boots stall ~1 s in early-boot TSC
# calibration (the kernel falls back to busy-waiting on the emulated PIT), so the samples split
# into a calibration-free "fast path" and a "slow path" cluster around SPLIT_MS. Restore skips
# the kernel boot entirely and is single-mode.
summarize() { # $1=mem  $2=cold-samples  $3=restore-samples
    python3 - "$1" "$2" "$3" <<'PY'
import sys
mem = sys.argv[1]
cold = sorted(float(x) for x in sys.argv[2].split())
rest = sorted(float(x) for x in sys.argv[3].split())
SPLIT = 900.0  # ms; cleanly separates the ~0.5 s fast path from the ~1.5 s PIT-stall path
def med(v): return v[len(v)//2]
def line(name, v):
    if not v:
        return "  %-20s: NO DATA" % name
    return "  %-20s: median %7.1f ms   (min %.1f, max %.1f, n=%d)" % (
        name, med(v), v[0], v[-1], len(v))
print("== %s MiB ==" % mem)
print(line("cold boot", cold))
fast = [x for x in cold if x < SPLIT]
slow = [x for x in cold if x >= SPLIT]
if fast and slow:
    print("       fast path %7.1f ms (n=%d)  |  slow path %7.1f ms (n=%d, +~%.0f ms TSC PIT-calib)"
          % (med(fast), len(fast), med(slow), len(slow), med(slow) - med(fast)))
print(line("snapshot restore", rest))
if cold and rest and med(rest) > 0:
    base = med(fast) if fast else med(cold)
    print("  %-20s: %.0fx (fast-path cold) .. %.0fx (median cold) faster via snapshot"
          % ("speedup", base / med(rest), med(cold) / med(rest)))
print()
PY
}

echo "boot-to-shell: cold PVH boot vs snapshot restore, median of $N runs, 1 vCPU"
echo "marker : \"$MARKER\""
echo "kernel : $KERNEL"
echo "initrd : $INITRD"
echo

for MEM in $MEMS; do
    # Cold-boot samples.
    cold=""
    for _ in $(seq 1 "$N"); do cold="$cold $(cold_once "$MEM")"; done

    # Capture a snapshot at the shell-ready point (one-off), then restore from it N times.
    SNAP="$SNAPDIR-$MEM"
    rm -rf "$SNAP"
    timeout 30 "$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" \
        --snapshot "$SNAP" --quiet --cmdline "$CMDLINE shellsnap" < /dev/null >/dev/null 2>&1 || true
    if [ ! -f "$SNAP/state.bin" ] || [ ! -f "$SNAP/mem.bin" ]; then
        echo "== $MEM MiB =="
        echo "  snapshot capture FAILED (no $SNAP/{state,mem}.bin)"
        echo
        continue
    fi

    rest=""
    for _ in $(seq 1 "$N"); do rest="$rest $(restore_once "$MEM" "$SNAP")"; done

    summarize "$MEM" "$cold" "$rest"
done

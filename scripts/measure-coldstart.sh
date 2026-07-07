#!/usr/bin/env bash
#
# Measures cold-start time (first guest instruction -> boot marker) for several console
# configurations and prints the median of N runs each. Requires a built VMM, kernel, and
# initramfs.
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/microvm"
KERNEL="${KERNEL:-$HOME/build/vmlinux}"
INITRD="${INITRD:-$HOME/build/initramfs.cpio.gz}"
MEM="${MEM:-512}"
N="${N:-6}"

[ -x "$BIN" ]    || { echo "build the VMM first: make release"; exit 1; }
[ -f "$KERNEL" ] || { echo "missing kernel: $KERNEL"; exit 1; }
[ -f "$INITRD" ] || { echo "missing initrd: $INITRD"; exit 1; }

median() {
    python3 -c "import sys;v=sorted(float(x) for x in sys.stdin.read().split());\
print('%8.1f ms  (min %.0f, max %.0f, n=%d)'%(v[len(v)//2],v[0],v[-1],len(v)) if v else 'NO DATA')"
}

measure() { # $1=marker  $2..=extra args
    local marker="$1"; shift
    for _ in $(seq 1 "$N"); do
        timeout 30 "$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" \
            --exit-on-boot --boot-marker "$marker" "$@" 2>&1 \
            | grep -oE 'cold-start: [0-9.]+' | grep -oE '[0-9.]+$'
    done | median
}

RUNINIT="Run /init as init process"
BANNER="ALPINE-MICROVM-BOOT-OK"

echo "cold-start (guest start -> marker), median of $N runs, ${MEM} MiB, 1 vCPU"
echo
echo "portb console (0xE9 -> hvc0), to kernel->userspace handoff, full logs:"
printf "  loud (rendered)    : %s\n" "$(measure "$RUNINIT" --cmdline 'earlycon=xe9 console=hvc0 reboot=t panic=-1')"
printf "  quiet (discarded)  : %s\n" "$(measure "$RUNINIT" --quiet --cmdline 'earlycon=xe9 console=hvc0 reboot=t panic=-1')"
echo
echo "end-to-end (to interactive shell):"
printf "  loud full logs     : %s\n" "$(measure "$BANNER" --cmdline 'earlycon=xe9 console=hvc0 reboot=t panic=-1')"
printf "  silent (quiet klog): %s\n" "$(measure "$BANNER" --cmdline 'earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1')"

FAST='clocksource=kvm-clock tsc=reliable no_timer_check random.trust_cpu=on rcupdate.rcu_expedited=1 nokaslr mitigations=off cryptomgr.notests earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1'
echo
echo "fastest (silent, 128 MiB, tuned cmdline):"
for _ in $(seq 1 "$N"); do
    timeout 30 "$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem 128 \
        --exit-on-boot --quiet --boot-marker "$BANNER" --cmdline "$FAST" 2>&1 \
        | grep -oE 'cold-start: [0-9.]+' | grep -oE '[0-9.]+$'
done | median | sed 's/^/  fast               : /'

#!/usr/bin/env bash
#
# Benchmarks networking across snapshot/restore: how long to reach a *working-network* guest by
#   1. cold boot with --net (kernel boot + virtio-net probe + guest ifconfig), versus
#   2. restore from a snapshot of a warmed, network-configured guest (the NIC's transport state is
#      captured and the host TAP is recreated identically, so the link resumes immediately).
#
# The snapshot is captured once: a one-off cold boot where alpine/init (with the `netsnap` token)
# configures the link, checks it, and asks the VMM for a snapshot (outb to port 0x605). On
# restore, execution resumes right after that point, the guest re-checks the link (now over the
# freshly recreated TAP) and prints NETSNAP-RESTORE-OK -- the marker this script times to. The
# link check is a single sysfs carrier read rather than a ping, so the marker reflects link resume
# time, not ping process-startup overhead.
#
# Networking needs privileges for the host TAP: run as root or allow passwordless `sudo ip`.
#
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/microvm"
KERNEL="${KERNEL:-$HOME/build/vmlinux}"
INITRD="${INITRD:-$HOME/build/initramfs.cpio.gz}"
SNAP="${SNAP:-$HOME/build/netsnap}"
MEM="${MEM:-256}"
N="${N:-10}"
CORES="${CORES:-1}"   # vCPUs for cold boots only; snapshot capture and --restore stay single-core
NET="${NET:-10.0.0.2/24}"
BANNER="ALPINE-MICROVM-BOOT-OK"
RESTORE_MARKER="NETSNAP-RESTORE-OK"
CMDLINE="earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1"

[ -x "$BIN" ]    || { echo "build the VMM first: make release"; exit 1; }
[ -f "$KERNEL" ] || { echo "missing kernel: $KERNEL (make kernel)"; exit 1; }
[ -f "$INITRD" ] || { echo "missing initrd: $INITRD (make initramfs)"; exit 1; }

median() {
    python3 -c "import sys;v=sorted(float(x) for x in sys.stdin.read().split());\
print('%7.1f ms  (min %.1f, max %.1f, n=%d)'%(v[len(v)//2],v[0],v[-1],len(v)) if v else 'NO DATA')"
}

cleanup_taps() {
    for t in $(ip -o link show 2>/dev/null | grep -oE 'llx[0-9]+' | sort -u); do
        sudo -n ip tuntap del dev "$t" mode tap 2>/dev/null || true
    done
}
trap cleanup_taps EXIT

echo "networking + snapshot benchmark, median of $N, ${MEM} MiB, ${CORES} vCPU cold / 1 vCPU restore, --net $NET"
echo

echo "== cold boot -> working-network shell (kernel boot + virtio-net + ifconfig) =="
{ for _ in $(seq 1 "$N"); do
    timeout 40 "$BIN" --num-cores "$CORES" --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" --net "$NET" \
        --exit-on-boot --quiet --boot-marker "$BANNER" --cmdline "$CMDLINE" 2>&1 \
        | grep -oE 'cold-start: [0-9.]+' | grep -oE '[0-9.]+$'
done; } | median | sed 's/^/  cold  (guest start -> marker): /'

echo "== capture a warmed, network-configured snapshot (one-off) =="
rm -rf "$SNAP"
timeout 40 "$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" --net "$NET" \
    --snapshot "$SNAP" --quiet --cmdline "$CMDLINE netsnap" < /dev/null >/dev/null 2>&1
[ -f "$SNAP/state.bin" ] || { echo "  snapshot capture failed"; exit 1; }
echo "  snapshot: $SNAP ($(du -sh "$SNAP" | cut -f1); mem.bin $(du -h "$SNAP/mem.bin" | cut -f1))"

echo "== restore -> working-network shell (resume + host TAP recreate + verify link) =="
{ for _ in $(seq 1 "$N"); do
    timeout 30 "$BIN" --restore "$SNAP" --mem "$MEM" \
        --exit-on-boot --quiet --boot-marker "$RESTORE_MARKER" < /dev/null 2>&1 \
        | grep -oE 'restore: [0-9.]+' | grep -oE '[0-9.]+$'
done; } | median | sed 's/^/  restore (guest resume -> marker): /'

echo "== end-to-end wall-clock (process start -> exit; includes host TAP setup) =="
wall() { # $@ = args ; prints ms
    local t0 t1
    t0=$(date +%s%3N)
    timeout 40 "$BIN" "$@" >/dev/null 2>&1
    t1=$(date +%s%3N)
    echo $((t1 - t0))
}
{ for _ in $(seq 1 "$N"); do
    wall --restore "$SNAP" --mem "$MEM" --exit-on-boot --quiet --boot-marker "$RESTORE_MARKER" < /dev/null
done; } | median | sed 's/^/  restore wall-clock          : /'

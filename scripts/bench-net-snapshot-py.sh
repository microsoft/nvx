#!/usr/bin/env bash
#
# Benchmarks snapshot/restore of a *networked* Python guest for two workloads:
#   (a) net-hello.py  -- a bare CPython interpreter, and
#   (b) net-pandas.py -- a warmed numpy/pandas interpreter,
# each of which proves the link works (a real HTTP round-trip to a host helper server) right before
# the timing marker, so a run that reaches the marker has resumed with a *live* NIC.
#
# For each app it times: cold boot to the marker (kernel boot + Python startup [+ pandas import] +
# NIC config + link check) versus restoring a snapshot captured at the warmed point. Networking
# needs privileges for the host TAP: run as root or allow passwordless `sudo ip`.
#
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/microvm"
KERNEL="${KERNEL:-$HOME/build/vmlinux}"
INITRD="${INITRD:-$HOME/build/initramfs-python.cpio.gz}"
MEM="${MEM:-512}"
N="${N:-8}"
NET="${NET:-10.0.0.2/24}"
PORT="${PORT:-8099}"
CMDLINE="earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1"

[ -x "$BIN" ]    || { echo "build the VMM first: make release"; exit 1; }
[ -f "$KERNEL" ] || { echo "missing kernel: $KERNEL (make kernel)"; exit 1; }
[ -f "$INITRD" ] || { echo "missing python initramfs: $INITRD (make python-initramfs)"; exit 1; }

median() {
    python3 -c "import sys;v=sorted(float(x) for x in sys.stdin.read().split());\
print('%7.1f ms  (min %.1f, max %.1f, n=%d)'%(v[len(v)//2],v[0],v[-1],len(v)) if v else 'NO DATA')"
}

WWW="$(mktemp -d)"
echo "HELLO-HOST" > "$WWW/index.html"
cleanup() {
    rm -rf "$WWW"
    for t in $(ip -o link show 2>/dev/null | grep -oE 'llx[0-9a-z]+' | sort -u); do
        sudo -n ip tuntap del dev "$t" mode tap 2>/dev/null || true
    done
}
trap cleanup EXIT

# Host helper server: the guest apps do an HTTP GET to the gateway (10.0.0.1) to prove the link.
( cd "$WWW" && timeout 900 python3 -m http.server --bind 0.0.0.0 "$PORT" >/dev/null 2>&1 & )
ready=""
for _ in $(seq 1 25); do
    if python3 -c "import urllib.request,sys;sys.exit(0 if b'HELLO-HOST' in urllib.request.urlopen('http://127.0.0.1:$PORT/',timeout=1).read() else 1)" 2>/dev/null; then
        ready=1; break
    fi
    sleep 0.2
done
[ -n "$ready" ] || { echo "host helper server did not come up on port $PORT (already in use?)"; exit 1; }

bench_app() { # $1=app file  $2=marker  $3=label
    local app="$1" marker="$2" label="$3" snap="$HOME/build/nspy_${1%.py}"
    echo "== $label =="

    { for _ in $(seq 1 "$N"); do
        timeout 60 "$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" --net "$NET" \
            --exit-on-boot --quiet --boot-marker "$marker" --cmdline "$CMDLINE pyapp=$app" 2>&1 \
            | grep -oE 'cold-start: [0-9.]+' | grep -oE '[0-9.]+$'
    done; } | median | sed 's/^/  cold    (guest start  -> marker): /'

    rm -rf "$snap"
    timeout 60 "$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" --net "$NET" \
        --snapshot "$snap" --quiet --cmdline "$CMDLINE pyapp=$app" < /dev/null >/dev/null 2>&1
    [ -f "$snap/state.bin" ] || { echo "  snapshot capture failed"; return 1; }
    echo "  snapshot: $(du -sh "$snap" | cut -f1) (mem.bin $(du -h "$snap/mem.bin" | cut -f1))"

    { for _ in $(seq 1 "$N"); do
        timeout 40 "$BIN" --restore "$snap" --mem "$MEM" \
            --exit-on-boot --quiet --boot-marker "$marker" < /dev/null 2>&1 \
            | grep -oE 'restore: [0-9.]+' | grep -oE '[0-9.]+$'
    done; } | median | sed 's/^/  restore (guest resume -> marker): /'

    # Confirm the marker really is the OK variant (network verified), one loud run.
    local out
    out=$(timeout 40 "$BIN" --restore "$snap" --mem "$MEM" \
        --exit-on-boot --boot-marker "$marker" --log-level warn < /dev/null 2>&1 | grep -aoE "$marker[A-Za-z' :{},0-9]*" | head -1)
    echo "  verified: ${out:-<marker not seen>}"
    rm -rf "$snap"
}

echo "networked Python snapshot benchmark, median of $N, ${MEM} MiB, 1 vCPU, --net $NET"
echo
bench_app net-hello.py  "HELLOPY-NET OK"  "(a) hello-world Python (bare interpreter + live NIC)"
echo
bench_app net-pandas.py "PANDASPY-NET OK" "(b) numpy + pandas app (warmed interpreter + live NIC)"

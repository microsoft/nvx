#!/usr/bin/env bash
#
# Benchmarks the virt-fs (see the VMM's --mount): guest I/O throughput to the mounted filesystem
# and the persistence round-trip of a read-write --mount-image. Requires a built VMM, kernel, and
# initramfs (the same ones `make run` uses), plus `mke2fs` (e2fsprogs) for read-write images.
#
# Two parts:
#   1. Sequential write/read throughput as observed inside the guest (busybox dd), for a
#      read-write mount that is *ephemeral* (--mount-rw, image held in guest memory) and one that
#      is *persistent* (--mount-image, image mapped MAP_SHARED from a host file).
#   2. Persistence round-trip on a persistent --mount-image: the wall-clock to create the image
#      and write a payload the first time, versus reusing the image afterwards, verifying via a
#      checksum that the guest's writes survived across VM runs (and in the host image file).
#
# All parsing is done on the host: the guest only runs plain dd/cksum/echo (no shell pipelines),
# which keeps the workload robust.
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/microvm"
KERNEL="${KERNEL:-$HOME/build/vmlinux}"
INITRD="${INITRD:-$HOME/build/initramfs.cpio.gz}"
MEM="${MEM:-512}"
N="${N:-5}"
CORES="${CORES:-1}"   # vCPUs per guest run (--vcpus)
PAYLOAD_MB="${PAYLOAD_MB:-64}"
# Give the ext4 image comfortable headroom over the payload.
IMG_MB="${IMG_MB:-$((PAYLOAD_MB * 2 + 64))}"
CMDLINE="console=hvc0 quiet loglevel=0 reboot=t panic=-1"

[ -x "$BIN" ]    || { echo "build the VMM first: make release"; exit 1; }
[ -f "$KERNEL" ] || { echo "missing kernel: $KERNEL (make kernel)"; exit 1; }
[ -f "$INITRD" ] || { echo "missing initrd: $INITRD (make initramfs)"; exit 1; }
command -v mke2fs >/dev/null || { echo "missing mke2fs (install e2fsprogs)"; exit 1; }

WORK="$(mktemp -d)"
SEED="$WORK/seed"
PERSIST_IMG="$WORK/persist.img"
ROUND_IMG="$WORK/round.img"
mkdir -p "$SEED"
echo "virt-fs benchmark seed" > "$SEED/README"
trap 'rm -rf "$WORK"' EXIT

# Reduces a stream of dd rate tokens (e.g. "355.6MB/s", "1.2GB/s", "0B/s") to a median MB/s.
rate_median() {
    python3 -c '
import sys, re
def mbps(t):
    m = re.match(r"([0-9.]+)\s*([KMG]?)B/s", t.strip())
    if not m:
        return None
    v = float(m.group(1))
    return v * {"": 1e-6, "K": 1e-3, "M": 1.0, "G": 1e3}[m.group(2)]
vals = sorted(x for x in (mbps(l) for l in sys.stdin if l.strip()) if x and x > 0)
print("%8.1f MB/s  (min %.0f, max %.0f, n=%d)" % (vals[len(vals)//2], vals[0], vals[-1], len(vals))
      if vals else "     n/a  (too fast for busybox dd to time)")
'
}

# Reduces a stream of millisecond samples to a median line.
ms_median() {
    python3 -c '
import sys
v = sorted(float(x) for x in sys.stdin.read().split())
print("%7.0f ms  (min %.0f, max %.0f, n=%d)" % (v[len(v)//2], v[0], v[-1], len(v)) if v else "NO DATA")
'
}

now_ms() { date +%s%3N; }

# Boots the guest, feeds it the script on stdin, and prints the guest console output. The
# trailing newline matters: command substitution strips it from the script, and without it the
# guest shell never receives the final "reboot -f" line and would hang until the timeout.
guest_run() { # $1=script  $2..=extra microvm args
    local script="$1"; shift
    printf '%s\n' "$script" | timeout 120 "$BIN" --vcpus "$CORES" --kernel "$KERNEL" --initrd "$INITRD" \
        --mem "$MEM" --log-level off --cmdline "$CMDLINE" "$@" 2>&1 || true
}

# Extracts the dd throughput token (e.g. "355.6MB/s") from the Nth "copied" line of guest output.
dd_rate() { # $1=guest output  $2=occurrence (1=write, 2=read)
    printf '%s\n' "$1" | grep -a 'copied' | sed -n "${2}p" | grep -oE '[0-9.]+ ?[KMG]?B/s' | tail -1
}

# Guest workload: write then read PAYLOAD_MB through the mount (plain dd, no pipelines).
io_script() {
    cat <<EOF
dd if=/dev/zero of=/mnt/host/bench.bin bs=1M count=$PAYLOAD_MB conv=fsync 2>&1
sync
echo 3 > /proc/sys/vm/drop_caches 2>/dev/null
dd if=/mnt/host/bench.bin of=/dev/null bs=1M 2>&1
sync
reboot -f
EOF
}

echo "virt-fs benchmark: ${PAYLOAD_MB} MiB payload, ${MEM} MiB guest, ${CORES} vCPU, image ${IMG_MB} MiB, median of $N runs"
echo

# ---- Part 1: guest-observed sequential throughput -------------------------------------------
run_io_bench() { # $1=label  $2..=mount args
    local label="$1"; shift
    local out w r
    w="$WORK/w"; r="$WORK/r"; : > "$w"; : > "$r"
    for _ in $(seq 1 "$N"); do
        out="$(guest_run "$(io_script)" "$@")"
        dd_rate "$out" 1 >> "$w"
        dd_rate "$out" 2 >> "$r"
    done
    printf "  %-27s write %s\n" "$label" "$(rate_median < "$w")"
    printf "  %-27s read  %s\n" "" "$(rate_median < "$r")"
}

echo "== sequential throughput (guest dd, conv=fsync writes) =="
run_io_bench "rw ephemeral (in-memory)" --mount "$SEED" --mount-rw --mount-size "$IMG_MB"
run_io_bench "rw persistent (file-backed)" --mount "$SEED" --mount-image "$PERSIST_IMG" --mount-size "$IMG_MB"
rm -f "$PERSIST_IMG"
echo

# ---- Part 2: persistence round-trip on a --mount-image --------------------------------------
create_script() {
    cat <<EOF
dd if=/dev/zero of=/mnt/host/data.bin bs=1M count=$PAYLOAD_MB 2>/dev/null
cksum /mnt/host/data.bin
sync
reboot -f
EOF
}
verify_script() {
    cat <<EOF
cksum /mnt/host/data.bin 2>/dev/null
reboot -f
EOF
}
# Extracts the cksum checksum for /mnt/host/data.bin from guest output.
data_cksum() { printf '%s\n' "$1" | grep -a 'data.bin' | grep -oE '^[0-9]+' | head -1; }

echo "== persistence round-trip (rw --mount-image) =="
rm -f "$ROUND_IMG"
t0="$(now_ms)"
create_out="$(guest_run "$(create_script)" --mount "$SEED" --mount-image "$ROUND_IMG" --mount-size "$IMG_MB")"
t1="$(now_ms)"
create_ck="$(data_cksum "$create_out")"
printf "  create image + write %s MiB       : %s ms\n" "$PAYLOAD_MB" "$((t1 - t0))"
printf "  host image on disk                : %s (%s)\n" "$(basename "$ROUND_IMG")" "$(du -h "$ROUND_IMG" | cut -f1)"

reuse_ms="$WORK/reuse"; : > "$reuse_ms"; ok=0
for _ in $(seq 1 "$N"); do
    t0="$(now_ms)"
    verify_out="$(guest_run "$(verify_script)" --mount "$SEED" --mount-image "$ROUND_IMG" --mount-size "$IMG_MB")"
    t1="$(now_ms)"
    echo "$((t1 - t0))" >> "$reuse_ms"
    [ -n "$create_ck" ] && [ "$(data_cksum "$verify_out")" = "$create_ck" ] && ok=$((ok + 1))
done
printf "  reuse image + verify (cold each)  : %s\n" "$(ms_median < "$reuse_ms")"
printf "  payload survived across runs      : %s/%s runs (cksum %s)\n" "$ok" "$N" "${create_ck:-?}"

# Host-side confirmation that the guest's write is really in the image file.
if command -v debugfs >/dev/null; then
    host_bytes="$(debugfs -R "stat /data.bin" "$ROUND_IMG" 2>/dev/null | sed -n 's/.*Size: \([0-9]*\).*/\1/p' | head -1)"
    printf "  host sees /data.bin in image      : %s bytes (debugfs)\n" "${host_bytes:-not found}"
fi

#!/usr/bin/env bash
#
# End-to-end profiling smoke test: boots the micro-VM with guest sampling (and, when the host
# `perf` recorder is usable, host tracing) enabled, then asserts the recorder lifecycle actually
# produced usable artifacts:
#
#   * the guest folded profile exists and contains at least one sampled stack, and
#   * when `perf record` works on this machine, the Linux host trace is *published* with a valid
#     `PERFILE2` header.
#
# The host assertion guards the recorder lifecycle specifically: `perf record` finalizes perf.data
# and then terminates via the SIGTERM nvx sends it (not a zero exit), so a naive success-only gate
# would silently discard every Linux host trace. This test fails if that regresses.
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/microvm"
KERNEL="${KERNEL:-$HOME/build/vmlinux-profiling}"
INITRD="${INITRD:-$HOME/build/initramfs.cpio.gz}"
MARKER="ALPINE-MICROVM-BOOT-OK"

[ -x "$BIN" ]     || { echo "!! build the VMM first: cargo build --release"; exit 1; }
[ -f "$KERNEL" ]  || { echo "!! missing kernel: $KERNEL (run python3 scripts/nvx.py build-kernel --profiling)"; exit 1; }
[ -f "$INITRD" ]  || { echo "!! missing initrd: $INITRD (run python3 scripts/nvx.py build-initramfs)"; exit 1; }

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
folded="$workdir/smoke.folded"
log="$workdir/run.log"

# Probe whether `perf record` can actually capture on this machine (it needs adequate
# perf_event_paranoid / CAP_PERFMON). Only then do we require nvx to publish a host trace.
host_probe="$workdir/probe.perf.data"
host_capable=0
if command -v perf >/dev/null 2>&1 \
    && perf record -o "$host_probe" -- true >/dev/null 2>&1 \
    && [ -s "$host_probe" ] \
    && head -c 8 "$host_probe" | grep -q "PERFILE2"; then
    host_capable=1
fi

host_args=()
if [ "$host_capable" = "1" ]; then
    echo ">> perf record is usable; enabling host tracing"
    host_args=(--host-profile)
else
    echo "::notice::perf record unavailable/unpermitted; running guest-only profiling smoke test"
fi

printf 'cat /etc/alpine-release\nreboot -f\n' | timeout 120 "$BIN" \
    --kernel "$KERNEL" --initrd "$INITRD" --mem 512 \
    --cmdline "earlycon=xe9 console=hvc0 reboot=t panic=-1" \
    --guest-profile "$folded" --profile-hz 997 --kernel-symbols "$KERNEL" \
    "${host_args[@]}" > "$log" 2>&1 || true

fail() { echo "FAIL: $1"; echo "--- last 25 lines ---"; tail -25 "$log"; exit 1; }

grep -q "$MARKER" "$log" || fail "guest did not reach userspace ($MARKER)"

# Guest folded profile must exist and carry at least one positive-count sample line.
[ -f "$folded" ] || fail "guest folded profile was not written: $folded"
if ! awk 'NF>=2 && $NF ~ /^[0-9]+$/ && $NF+0 > 0 { found=1 } END { exit found?0:1 }' "$folded"; then
    fail "guest folded profile has no sampled stacks: $folded"
fi
samples="$(awk 'NF>=2 && $NF ~ /^[0-9]+$/ { n += $NF } END { print n+0 }' "$folded")"
echo "PASS: guest profile has $samples sample(s)"

# When host tracing was enabled, the recorder must have published a finalized perf.data.
if [ "$host_capable" = "1" ]; then
    host_trace="$workdir/smoke.host.perf.data"
    [ -f "$host_trace" ] || fail "host trace was not published: $host_trace (recorder lifecycle regression)"
    head -c 8 "$host_trace" | grep -q "PERFILE2" \
        || fail "published host trace is not a finalized perf.data (missing PERFILE2 header)"
    echo "PASS: host trace published with valid PERFILE2 header"
fi

echo "PASS: profiling smoke test"

#!/usr/bin/env bash
#
# End-to-end boot smoke test: boots the micro-VM and asserts that the guest reaches
# userspace (the initramfs /init prints a known marker). Exits non-zero on failure, so it
# can be used as a CI-style check of the success criteria.
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/microvm"
KERNEL="${KERNEL:-$HOME/build/vmlinux}"
INITRD="${INITRD:-$HOME/build/initramfs.cpio.gz}"
MARKER="ALPINE-MICROVM-BOOT-OK"

[ -x "$BIN" ]     || { echo "!! build the VMM first: cargo build --release"; exit 1; }
[ -f "$KERNEL" ]  || { echo "!! missing kernel: $KERNEL (run scripts/build-kernel.sh)"; exit 1; }
[ -f "$INITRD" ]  || { echo "!! missing initrd: $INITRD (run scripts/build-initramfs.sh)"; exit 1; }

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

printf 'cat /etc/alpine-release\nreboot -f\n' | timeout 90 "$BIN" \
    --kernel "$KERNEL" --initrd "$INITRD" --mem 512 \
    --cmdline "console=ttyS0 reboot=t panic=-1" > "$log" 2>&1 || true

if grep -q "$MARKER" "$log"; then
    echo "PASS: guest reached userspace ($MARKER)"
    grep -m1 -E '^3\.[0-9]+\.[0-9]+' "$log" | sed 's/^/  alpine-release: /' || true
    grep -m1 -E 'Linux .* x86_64' "$log" | sed 's/^/  uname: /' || true
    exit 0
fi

echo "FAIL: '$MARKER' not found in boot output"
echo "--- last 25 lines ---"
tail -25 "$log"
exit 1

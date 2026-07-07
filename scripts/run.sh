#!/usr/bin/env bash
#
# Convenience launcher for the micro-VM.
#
set -euo pipefail

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/microvm"
KERNEL="${KERNEL:-$HOME/build/vmlinux}"
INITRD="${INITRD:-$HOME/build/initramfs.cpio.gz}"
MEM="${MEM:-512}"
CMDLINE="${CMDLINE:-earlycon=xe9 console=hvc0 reboot=t panic=-1}"

exec "$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" --cmdline "$CMDLINE"

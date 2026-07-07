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

args=(--kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" --cmdline "$CMDLINE")

# Optionally export a host directory to the guest as a virt-fs. Set MOUNT to the host directory
# (and optionally MOUNT_TARGET, default /mnt/host). Set MOUNT_RW=1 for a writable (ext4) mount,
# or MOUNT_IMAGE=<file> for a writable mount whose changes persist to that host image file.
if [ -n "${MOUNT:-}" ]; then
    args+=(--mount "$MOUNT" --mount-target "${MOUNT_TARGET:-/mnt/host}")
    [ -n "${MOUNT_RW:-}" ] && args+=(--mount-rw)
    [ -n "${MOUNT_IMAGE:-}" ] && args+=(--mount-image "$MOUNT_IMAGE")
fi

# Optionally attach a virt-net NIC and expose a host network endpoint to the guest. Set NET to the
# guest IP/prefix (e.g. NET=10.0.0.2/24); the host side of the point-to-point TAP link takes the
# first address of the subnet (10.0.0.1) and becomes the guest's gateway. Creating the host TAP
# needs privileges (run as root or allow passwordless `sudo ip`).
if [ -n "${NET:-}" ]; then
    args+=(--net "$NET")
fi

exec "$BIN" "${args[@]}"

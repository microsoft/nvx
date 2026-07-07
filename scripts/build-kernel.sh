#!/usr/bin/env bash
#
# Builds the minimal PVH Linux kernel (uncompressed vmlinux) used by the micro-VM.
#
# The configuration in kernel/config-microvm disables PCI, ACPI, and legacy PC hardware
# probing, enables the PVH boot entry (CONFIG_PVH), the KVM paravirt clock, a 16550 serial
# console, and initramfs support. Alpine's linux-lts is essentially a vanilla LTS kernel, so
# we build the matching vanilla source from kernel.org.
#
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
KVER="${KVER:-6.18.38}"
WORK="${WORK:-$HOME/build/kernel}"
OUT="${OUT:-$HOME/build/vmlinux}"
CONFIG="$REPO/kernel/config-microvm"

mkdir -p "$WORK"
cd "$WORK"

SRC="linux-${KVER}"
if [ ! -d "$SRC" ]; then
    echo ">> downloading linux-${KVER}"
    curl -fL --retry 3 -O "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KVER}.tar.xz"
    tar -xf "linux-${KVER}.tar.xz"
fi

cd "$SRC"

# Apply the microvm kernel modification (the 0xE9 "portb" debug earlycon). Idempotent: the
# grep guard skips it if the source already contains the driver.
PATCH="$REPO/kernel/patches/0001-microvm-xe9-earlycon.patch"
if ! grep -q early_xe9_write arch/x86/kernel/early_printk.c; then
    echo ">> applying $(basename "$PATCH")"
    patch -p1 < "$PATCH"
fi

cp "$CONFIG" .config
make olddefconfig
echo ">> building vmlinux with $(nproc) jobs"
make -j"$(nproc)" vmlinux

cp -f vmlinux "$OUT"
echo ">> built $OUT"
readelf -n vmlinux 2>/dev/null | grep -iA1 'Xen' | grep -i '0x00000012' >/dev/null \
    && echo ">> PVH entry note present" \
    || echo "!! WARNING: PVH entry note not found — check CONFIG_PVH"

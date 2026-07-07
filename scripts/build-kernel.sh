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

KVER="${KVER:-6.18.38}"
WORK="${WORK:-$HOME/build/kernel}"
OUT="${OUT:-$HOME/build/vmlinux}"
CONFIG="$(cd "$(dirname "$0")/.." && pwd)/kernel/config-microvm"

mkdir -p "$WORK"
cd "$WORK"

SRC="linux-${KVER}"
if [ ! -d "$SRC" ]; then
    echo ">> downloading linux-${KVER}"
    curl -fL --retry 3 -O "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KVER}.tar.xz"
    tar -xf "linux-${KVER}.tar.xz"
fi

cd "$SRC"

# Add the Nanvix-style per-byte debug console on I/O port 0xE9 (select with earlycon=xe9),
# used to redirect kernel logs onto the low-overhead "portb" path. Idempotent.
EP="arch/x86/kernel/early_printk.c"
if ! grep -q early_xe9_write "$EP"; then
    echo ">> adding xe9 (port 0xE9) earlycon to $EP"
    grep -q 'linux/serial_core.h' "$EP" \
        || sed -i '0,/#include <linux\/console.h>/s//#include <linux\/console.h>\n#include <linux\/serial_core.h>/' "$EP"
    cat >> "$EP" <<'XE9'

/* microvm: Nanvix-style per-byte debug console on I/O port 0xE9. Select with earlycon=xe9. */
static void early_xe9_write(struct console *console, const char *s, unsigned int count)
{
	while (count--)
		outb(*s++, 0xe9);
}

static int __init early_xe9_setup(struct earlycon_device *device, const char *options)
{
	device->con->write = early_xe9_write;
	return 0;
}
EARLYCON_DECLARE(xe9, early_xe9_setup);
XE9
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

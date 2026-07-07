#!/usr/bin/env bash
#
# Builds the minimal PVH Linux kernel (uncompressed vmlinux) used by the micro-VM.
#
# The configuration in kernel/config-microvm disables PCI, ACPI, legacy PC hardware probing,
# and the 16550 UART (CONFIG_SERIAL_8250), and enables the PVH boot entry (CONFIG_PVH), the
# KVM paravirt clock, and initramfs support. The guest console is the microvm "portb" hvc
# driver (kernel/hvc_xe9.c, installed below) plus the earlycon=xe9 output console. Alpine's
# linux-lts is essentially a vanilla LTS kernel, so we build the matching vanilla source from
# kernel.org.
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

# Install the microvm "portb" hvc console driver (bidirectional console over ports 0xE9/0xEA
# that replaces the 16550 UART). Copy the source in, register it in the hvc Makefile, and add
# a Kconfig symbol that selects the hvc core. All three steps are idempotent.
echo ">> installing hvc_xe9 console driver"
cp -f "$REPO/kernel/hvc_xe9.c" drivers/tty/hvc/hvc_xe9.c
# shellcheck disable=SC2016  # `$(CONFIG_HVC_XE9)` is a Make variable and must be written literally.
grep -q 'hvc_xe9.o' drivers/tty/hvc/Makefile \
    || echo 'obj-$(CONFIG_HVC_XE9)		+= hvc_xe9.o' >> drivers/tty/hvc/Makefile
grep -q 'HVC_XE9' drivers/tty/hvc/Kconfig || cat >> drivers/tty/hvc/Kconfig <<'EOF'

config HVC_XE9
	bool "microvm portb (0xE9/0xEA) hypervisor console"
	depends on X86
	select HVC_DRIVER
	help
	  Bidirectional hypervisor console for the microvm VMM: output is one
	  outb per byte to I/O port 0xE9, input is polled from 0xEA/0xE9. It
	  registers as hvc0 and replaces the 16550 UART. Select with console=hvc0.
EOF

cp "$CONFIG" .config
make olddefconfig
echo ">> building vmlinux with $(nproc) jobs"
make -j"$(nproc)" vmlinux

cp -f vmlinux "$OUT"
echo ">> built $OUT"
readelf -n vmlinux 2>/dev/null | grep -iA1 'Xen' | grep -i '0x00000012' >/dev/null \
    && echo ">> PVH entry note present" \
    || echo "!! WARNING: PVH entry note not found — check CONFIG_PVH"

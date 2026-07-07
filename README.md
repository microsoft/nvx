# microvm

A minimal, single-core **x86_64 KVM micro-VM** that boots a Linux (Alpine) kernel through
the **PVH boot protocol**, entirely from a **RAM initramfs** — no PCI, no ACPI, no block
device. The only emulated device is a 16550 serial console.

It is a standalone extraction and reworking of the **KVM (Linux) backend of the
[Nanvix Micro-VM (`uservm`)](https://github.com/nanvix/nanvix/tree/dev/src/uservm)**,
stripped of the Nanvix-specific paravirtual ABI (magic control registers, credits,
snapshotting, control plane, profiler, host filesystem, ...) and given the x86_64 Linux boot
support required to run a real Linux kernel.

## What it does

```
$ make release kernel initramfs      # build the VMM, the kernel, and the initramfs
$ make run                           # boot Alpine to a shell over the serial console
...
[    1.745978] Run /init as init process

==========================================
 ALPINE-MICROVM-BOOT-OK: 3.24.1
 uname: Linux (none) 6.18.38 ... x86_64 Linux
==========================================

/ # cat /etc/alpine-release
3.24.1
/ # id
uid=0(root) gid=0(root)
/ # reboot -f
```

## Design

- **Single core, single thread.** One vCPU driven by a synchronous `KVM_RUN` loop.
- **PVH boot** (`XEN_ELFNOTE_PHYS32_ENTRY` + `hvm_start_info`). The VMM loads an uncompressed
  `vmlinux`, locates the PVH 32-bit entry note, and enters the guest in 32-bit protected mode
  with `%ebx` pointing at the boot info. The kernel itself switches to long mode. This avoids
  the real-mode/bzImage setup path entirely.
- **RAM-only root filesystem.** The initramfs is loaded into guest memory and passed as a PVH
  module; the kernel unpacks it and runs `/init`. There is no virtio and no block device.
- **Minimal device model.** A 16550A UART at I/O port `0x3F8` (`console=ttyS0`, IRQ 4) is the
  only device. Every other port floats (reads return all-ones, writes are dropped), which lets
  a PCI-less/ACPI-less kernel skip legacy probes (i8042, CMOS/RTC, POST codes, ...). The
  in-kernel KVM irqchip (PIC + IOAPIC) and PIT provide interrupts and the timer; `kvm-clock`
  provides time.

### Source layout

| Path | Responsibility |
|------|----------------|
| `src/main.rs`         | CLI and entry point |
| `src/vmm.rs`          | VM setup, the vCPU run loop, console input thread, TTY handling |
| `src/memory.rs`       | Guest RAM as KVM user-memory regions (MMIO-gap aware) |
| `src/vcpu.rs`         | vCPU creation, CPUID, and the PVH entry register/segment state |
| `src/boot/pvh.rs`     | `vmlinux` ELF loader, PVH note parsing, `hvm_start_info` layout |
| `src/boot/params.rs`  | PVH boot-parameter structures |
| `src/devices/serial.rs` | 16550A UART model |
| `src/devices/mod.rs`  | PMIO device bus |
| `src/irq.rs`          | In-kernel irqchip + PIT |
| `src/layout.rs`       | Guest-physical memory map constants |
| `kernel/config-microvm` | Minimal Linux kernel configuration |
| `scripts/`            | Kernel / initramfs build and run helpers |

## The kernel ("modified Alpine kernel")

`kernel/config-microvm` is a minimal x86_64 configuration built from the vanilla LTS source
that Alpine's `linux-lts` tracks. Relative to a stock kernel it **removes PCI and PC-like
hardware detection** and everything that a paravirtual micro-VM does not need:

- `# CONFIG_PCI is not set`, `# CONFIG_ACPI is not set`
- no `CONFIG_XEN` (standalone PVH under KVM), no MP table / ISA / legacy platform probing
- no framebuffer/VGA, USB, sound, HID, ATA/SCSI, network drivers, loadable modules

and **enables** exactly what is required to boot:

- `CONFIG_PVH=y` (PVH entry), `CONFIG_KVM_GUEST=y` + paravirt clock
- `CONFIG_SERIAL_8250=y` / `CONFIG_SERIAL_8250_CONSOLE=y` (`ttyS0`)
- `CONFIG_BLK_DEV_INITRD=y`, `CONFIG_DEVTMPFS=y`, `CONFIG_TMPFS=y`

`scripts/build-kernel.sh` downloads the matching kernel source, applies this config, and
builds an uncompressed `vmlinux` with the PVH entry note.

## The initramfs (RAM filesystem)

`scripts/build-initramfs.sh` unpacks the official Alpine mini root filesystem, adds an `/init`
that mounts `proc`/`sys`/`dev`/`tmpfs` and execs a shell, and packs it as a gzipped `newc`
cpio archive. The whole userland lives in RAM.

## Building and running

Requirements: a Linux host with `/dev/kvm` accessible to your user, a stable Rust toolchain
(edition 2024), and — for building the kernel — `flex`, `bison`, `libelf-dev`, `bc`, `cpio`.

```
make release        # build the VMM (cargo build --release)
make kernel         # build $HOME/build/vmlinux           (~minutes)
make initramfs      # build $HOME/build/initramfs.cpio.gz
make run            # boot it

make test           # unit tests (no KVM required)
make selftest       # tiny protected-mode program through the real entry path
```

Run directly:

```
./target/release/microvm \
    --kernel  $HOME/build/vmlinux \
    --initrd  $HOME/build/initramfs.cpio.gz \
    --mem     512 \
    --cmdline "console=ttyS0 reboot=t panic=-1"
```

### CLI

| Flag | Default | Meaning |
|------|---------|---------|
| `--kernel <path>`  | (required)                       | Uncompressed `vmlinux` (PVH) |
| `--initrd <path>`  | (none)                           | RAM initramfs image |
| `--cmdline <str>`  | `console=ttyS0 reboot=t panic=-1`| Kernel command line |
| `--mem <MiB>`      | `512`                            | Guest RAM |
| `--selftest`       |                                  | Run the protected-mode self-test and exit |

## Notes

- **Exiting the VM:** `reboot -f` (a triple-fault reset with `reboot=t`) is caught by the VMM,
  which exits cleanly. `poweroff` has no effect because the kernel has no ACPI/power management
  and simply halts.
- When standard input is a terminal it is put in raw mode for an authentic serial console;
  when input is piped (scripts, CI) it is left alone.

## Provenance & license

MIT licensed. Derived from the KVM backend of the Nanvix Micro-VM (`uservm`), © The
Maintainers of Nanvix (MIT). See `LICENSE`.

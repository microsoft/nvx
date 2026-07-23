# Project Organization

NVX is one Rust VMM with compile-time Linux/KVM and Windows/WHP backends, plus reproducible guest
kernel and Alpine initramfs builders. Both backends expose the same small x86-64 machine and enter
the same Linux kernel through the PVH boot protocol.

## Boot flow

1. The VMM allocates guest RAM, leaving the conventional 3-4 GiB MMIO gap.
2. The ELF loader reads the uncompressed `vmlinux`, finds `XEN_ELFNOTE_PHYS32_ENTRY`, and copies
   loadable segments to their guest physical addresses.
3. The VMM places the kernel command line, optional initramfs module descriptor, PVH memory map, and
   `hvm_start_info` in low memory.
4. The bootstrap vCPU enters the PVH 32-bit protected-mode entry with `%ebx` pointing to
   `hvm_start_info`; Linux performs its own transition to long mode.
5. Linux unpacks the initramfs and runs `/init`, which mounts pseudo-filesystems, configures optional
   virt-fs and networking, prints the boot marker, and starts a shell or Python application.

KVM SMP adds an Intel MP table. Linux enumerates the additional processors and starts them through
INIT-SIPI-SIPI using KVM's in-kernel LAPIC. WHP currently exposes one vCPU.

## Machine model

| Area | Model |
| --- | --- |
| Firmware and buses | No BIOS runtime, UEFI, PCI, or ACPI. |
| CPU | One x86-64 vCPU by default; up to 254 on KVM, one on WHP. |
| Boot | PVH 32-bit entry into an uncompressed ELF `vmlinux`. |
| Root filesystem | Gzipped cpio initramfs unpacked into RAM. |
| Console | Portb `hvc0`: TX on `0xE9`, RX status/data on `0xEA`/`0xE9`. |
| Clock at boot | Minimal MC146818-compatible RTC registers at `0x70`/`0x71`. |
| Shutdown and snapshot | Guest writes to PMIO ports `0x604` and `0x605`. |
| Interrupts on KVM | In-kernel PIC, IOAPIC, LAPIC, and PIT; `kvm-clock` at runtime. |
| Interrupts on WHP | WHP LAPIC plus minimal userspace PIC, PIT, and RTC; TSC clocksource. |
| Optional network | One virtio-net device on virtio-mmio at `0xD0000000`. |
| Optional virt-fs | A filesystem image mapped above guest RAM and exposed through `phram`. |

The always-on device model is intentionally small. Virtio is only present when networking is
requested; virt-fs is a mapped image rather than a general block controller.

## Guest physical layout

The shared constants live in `src/layout.rs`.

| Address or range | Use |
| --- | --- |
| `0x00000500` | Bootstrap GDT. |
| `0x00000520` | Empty bootstrap IDT. |
| `0x00006000` | PVH `hvm_start_info`. |
| `0x00006040` | PVH initramfs module descriptor. |
| `0x00007000` | PVH memory map. |
| `0x00020000` | Kernel command line, up to 64 KiB. |
| `0x00100000` and above | Kernel ELF segments and ordinary low RAM. |
| `0xC0000000..0xFFFFFFFF` | MMIO gap, including virtio-net and APIC windows. |
| `0x100000000` and above | RAM that does not fit below the MMIO gap. |

The kernel ELF chooses its own physical load addresses. The initramfs is placed in available RAM,
and optional virt-fs storage is mapped beyond RAM reported to Linux so the page allocator cannot
overwrite it.

## Backend differences

### Linux and KVM

KVM supplies CPU virtualization, in-kernel interrupt controllers, the PIT, and paravirtual time.
The VMM owns guest-memory mappings, PVH setup, PMIO exits, optional TAP-backed virtio-net, and
optional filesystem-image mappings. Guest RAM restores use a private mapping of `mem.bin`, so pages
are faulted in lazily and writes do not modify the snapshot.

### Windows and WHP

WHP supplies CPU virtualization, guest-memory mappings, and local APIC emulation, but not a PC
chipset. NVX therefore implements the minimum PIC, PIT, and RTC behavior Linux needs. A host timer
raises PIT IRQ0, and the VMM clears the LAPIC in-service state for the virtual-wire guest after the
8259 end-of-interrupt. CPUID exposes a measured TSC frequency so Linux can use the TSC clocksource.

Virtio MMIO instructions are serviced with the WHP instruction emulator. The network device uses
master-PIC IRQ5; KVM uses IRQ10 and an `irqfd`. WHP restores map `mem.bin` copy-on-write with Windows
file mappings, retaining the same lazy and replayable memory semantics.

## Source tree

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | VMM CLI, logging setup, validation, and backend dispatch. |
| `src/console.rs` | Shared console output, markers, byte counts, and timing. |
| `src/layout.rs` | Shared guest physical memory constants. |
| `src/boot/` | ELF/PVH loader, boot structures, parameters, and KVM MP table. |
| `src/devices/` | Shared PMIO bus, portb console, shutdown/snapshot controls, and KVM RTC. |
| `src/vmm.rs` | KVM VM creation, vCPU loops, console input, and lifecycle. |
| `src/vcpu.rs`, `src/irq.rs` | KVM vCPU state, CPUID, irqchip, and PIT setup. |
| `src/memory.rs`, `src/snapshot.rs` | KVM RAM and full VM capture/restore. |
| `src/net.rs`, `src/virtfs.rs` | KVM TAP-backed virtio-net and SquashFS/ext4 virt-fs. |
| `src/whp/mod.rs` | WHP partition creation, vCPU loop, I/O dispatch, and timer/input threads. |
| `src/whp/vcpu.rs`, `src/whp/memory.rs` | WHP processor entry state and guest RAM mappings. |
| `src/whp/pic.rs`, `pit.rs`, `rtc.rs` | Minimal WHP legacy interrupt, timer, and wall-clock devices. |
| `src/whp/net.rs`, `slirp.rs` | WHP virtio-net and standalone user-mode NAT. |
| `src/whp/xdp.rs`, `src/l2bridge.rs` | AF_XDP data plane and strict external network contract. |
| `src/whp/emulator.rs` | WHP instruction-emulator wrapper for virtio MMIO. |
| `src/whp/snapshot.rs` | WHP processor, APIC, device, and memory snapshot state. |
| `src/whp/virtfs.rs` | Pure-Rust FAT image and Windows file mapping for virt-fs. |
| `kernel/config-microvm` | Minimal Linux configuration shared by both backends. |
| `kernel/hvc_xe9.c` | Bidirectional portb `hvc0` driver installed by the builder. |
| `kernel/patches/` | Linux source patch for `earlycon=xe9`. |
| `alpine/` | Base and Python PID 1 scripts, snapshot helper, and sample workloads. |
| `scripts/nvx.py` | Cross-platform build, run, smoke-test, snapshot, and benchmark entry point. |
| `scripts/nvx_tools/` | Shared Python workflows and host backend adapters. |
| `scripts/performance.py` | Metric collection, history persistence, and regression gating. |
| `docker/Dockerfile` | Reproducible Linux guest-artifact builds on any Docker host. |
| `.github/workflows/ci.yml` | KVM, WHP, optional HCN/AF_XDP, and performance CI. |
| `data/performance/` | Per-backend p50 history persisted from successful main builds. |

## Guest kernel configuration

`kernel/config-microvm` builds in only the facilities required by this machine model:

| Configuration area | Reason |
| --- | --- |
| `PVH` | Enter through the ELF PVH note without a bzImage real-mode path. |
| `HYPERVISOR_GUEST`, `PARAVIRT`, `KVM_GUEST` | Use KVM paravirtual facilities when available and remain inert on WHP. |
| `SERIAL_EARLYCON`, `HVC_XE9` | Provide early and interactive output over the portb console. |
| `BLK_DEV_INITRD`, `DEVTMPFS`, `TMPFS` | Run the entire base userspace from initramfs. |
| `NET`, `INET`, `VIRTIO_MMIO`, `VIRTIO_NET` | Support the optional single IPv4 NIC. |
| `MTD_PHRAM`, `MTD_BLOCK`, SquashFS, ext4, VFAT | Mount backend-specific virt-fs images. |
| `SMP`, `X86_MPPARSE`, `X86_X2APIC` | Enumerate KVM vCPUs from the MP table. |
| `HZ_100`, `NO_HZ_IDLE` | Keep timer traffic low. |

Modules, PCI, ACPI, USB, storage-controller stacks, graphics, sound, suspend, hibernation, NUMA,
machine checks, tracing, and most debug facilities are disabled. Avoiding `PM_TRACE_RTC` is
particularly important because its early wall-clock probing can add long timeouts on a minimal
machine.

## Provenance

The Linux/KVM backend was extracted and reworked from the KVM backend of the Nanvix Micro-VM
(`uservm`). NVX replaces Nanvix-specific paravirtual interfaces with a stock Linux PVH boot path
and provides the WHP-native counterpart. The repository is MIT licensed; see `LICENSE`.

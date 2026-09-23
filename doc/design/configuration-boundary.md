# Configuration boundary

[Design index](../design.md)

Machine identity is explicit rather than inferred from a kernel, device, or
hypervisor choice. OpenVMM carries it through CLI, worker, Petri, and snapshot
configuration. TTRPC exposes one microVM profile with numeric value 2; retired
value 1 is reserved and rejected. Validation
occurs before host resources are opened and again at the worker boundary.

The microVM requires:

- an x86-64 guest;
- one NUMA node;
- Linux direct boot with Intel MP 1.4 tables;
- KVM, MSHV, or WHP;
- no VTL2, isolation, nested virtualization, or Hyper-V enlightenments; and
- the exact [chipset and device inventory](machine-and-device-abi.md).

It accepts exactly 1, 2, 4, or 8 vCPUs
in one socket and one die, with one core per vCPU, no SMT, xAPIC mode, and
contiguous APIC IDs starting at zero. Its Linux direct layout places the MP
floating pointer at `0x0`, MP configuration table at `0x400`, GDT at `0x1000`,
zero page at `0x2000`, and reserves `0x30000..0x30fff` for interrupt status.

Snapshot capture may declare an immutable RAM capacity at least as large as the
active base RAM. When it does, both values must be 128-MiB aligned. A snapshot
without a declared capacity cannot select a different RAM size at restore.

Only role-bearing block devices are supported; ordinary unroled `--virtio-blk`
is rejected. Snapshot capture with blocks requires one to three
read-only lower layers followed by writable scratch, all backed by cached,
regular raw files with nonzero 512-byte-aligned geometry. Blockless
snapshots are also supported and do not use sandbox tier metadata.

It rejects UEFI, PCAT, IGVM, caller-supplied ACPI, SMBIOS, device tree,
PCI/PCIe, VPCI, VMBus, ISA DMA, IDE, floppy, VMGS, graphics, VGA firmware,
debugger resources, and devices outside the profile. The profile itself emits
the [fixed Linux direct MP-table metadata](cold-boot.md#linux-direct-mp-table-loader).
This is an allowlist: the implementation builds a microVM directly instead of
constructing a standard PC and removing unwanted devices.

Restore is stricter than cold boot. Guest-visible configuration is read from
the snapshot's machine contract. Restore-time input may select the same backend
kind recorded by the snapshot, supply required attachments, and select
processor and RAM activation targets explicitly allowed by the contract. Those
process-local targets do not change processor capacity, RAM capacity, command
line, device placement, feature masks, or filesystem and network identity.

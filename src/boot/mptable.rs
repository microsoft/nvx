// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # Intel MultiProcessor (MP) Table
//!
//! Builds a minimal Intel MP Specification v1.4 table and writes it into low guest RAM so a
//! device-less, ACPI-less PVH guest can enumerate and bring up more than one vCPU.
//!
//! The guest Linux kernel (built with `CONFIG_X86_MPPARSE=y`, which the micro-VM kernel has, and
//! independent of `CONFIG_ACPI`) scans a handful of fixed physical-address windows for the
//! floating-pointer signature `"_MP_"`. We place the whole table at [`MP_TABLE_BASE`] (`0x9FC00`,
//! the top of base RAM), which is the second window the kernel scans and the location Firecracker
//! and cloud-hypervisor use. From the table the kernel reads one processor entry per vCPU
//! (APIC id = vCPU index), sets `smp_found_config`, and later drives the normal INIT-SIPI-SIPI
//! bring-up — which the in-kernel KVM LAPIC services without any further VMM involvement.
//!
//! Only the CPU entries matter for bring-up; the bus / IO-APIC / interrupt-source entries mirror a
//! conventional single-IO-APIC ISA layout so the table validates and the kernel's IRQ plumbing is
//! well-formed. Layout produced (contiguous from `MP_TABLE_BASE`):
//!
//! ```text
//! mpf_intel   (16 B)     "_MP_", physptr -> mpc_table, spec 4
//! mpc_table   (44 B)     "PCMP", lapic = 0xFEE00000, length/count/checksum back-filled
//! mpc_cpu     (20 B) x N apicid = index, BSP flag on vCPU 0
//! mpc_bus     ( 8 B)     ISA bus 0
//! mpc_ioapic  ( 8 B)     IO-APIC at 0xFEC00000
//! mpc_intsrc  ( 8 B) x16 ISA IRQ 0..15 -> IO-APIC
//! mpc_lintsrc ( 8 B) x 2 ExtINT -> LINT0, NMI -> LINT1 (broadcast)
//! ```

use ::anyhow::{Result, bail};

use crate::memory::GuestMemory;

/// Guest-physical address of the MP floating pointer (`"_MP_"`). `0x9FC00` is the top 1 KiB of
/// conventional base RAM, one of the windows the guest kernel scans. The configuration table
/// follows immediately and, even at [`MAX_SUPPORTED_CPUS`], ends below 1 MiB and remains clear of
/// every other micro-VM boot structure (boot GDT/IDT, PVH info, cmdline, and kernel image).
pub const MP_TABLE_BASE: u64 = 0x0009_FC00;

/// Default local APIC physical base address (must be non-zero or the kernel rejects the table).
const APIC_DEFAULT_PHYS_BASE: u32 = 0xFEE0_0000;
/// Default IO-APIC physical base address.
const IO_APIC_DEFAULT_PHYS_BASE: u32 = 0xFEC0_0000;
/// Reported local/IO APIC version (integrated APIC).
const APIC_VERSION: u8 = 0x14;

/// MP entry type: processor.
const MP_PROCESSOR: u8 = 0;
/// MP entry type: bus.
const MP_BUS: u8 = 1;
/// MP entry type: IO-APIC.
const MP_IOAPIC: u8 = 2;
/// MP entry type: IO interrupt assignment.
const MP_INTSRC: u8 = 3;
/// MP entry type: local interrupt assignment.
const MP_LINTSRC: u8 = 4;

/// Processor flag: entry describes an available CPU.
const CPU_ENABLED: u8 = 0x01;
/// Processor flag: entry describes the bootstrap processor.
const CPU_BOOTPROCESSOR: u8 = 0x02;

/// IO-APIC flag: the IO-APIC is usable.
const MPC_APIC_USABLE: u8 = 0x01;

/// Interrupt type: vectored ("INT").
const MP_INT: u8 = 0;
/// Interrupt type: NMI.
const MP_NMI: u8 = 1;
/// Interrupt type: external 8259A-compatible ("ExtINT").
const MP_EXTINT: u8 = 3;

/// Number of legacy ISA interrupt lines routed to the IO-APIC.
const NUM_ISA_IRQS: u8 = 16;

/// The MP table uses 8-bit APIC ids and reserves one for the IO-APIC, so at most 254 CPUs.
pub const MAX_SUPPORTED_CPUS: u8 = 254;

/// Appends `value` little-endian to `buf`.
fn push_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_le_bytes());
}

/// Appends `value` little-endian to `buf`.
fn push_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

/// Two's-complement checksum byte that makes the sum of all bytes in `slice` (including it) zero.
fn checksum(slice: &[u8]) -> u8 {
    let sum: u8 = slice.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    0u8.wrapping_sub(sum)
}

/// Builds the serialized MP table for `num_cpus` vCPUs.
fn build(num_cpus: u8) -> Result<Vec<u8>> {
    if num_cpus == 0 {
        bail!("MP table requires at least one CPU");
    }
    if num_cpus > MAX_SUPPORTED_CPUS {
        bail!("MP table supports at most {MAX_SUPPORTED_CPUS} CPUs (requested {num_cpus})");
    }

    // The configuration table begins immediately after the 16-byte floating pointer.
    let mpc_base: u32 = (MP_TABLE_BASE as u32) + 16;

    let mut buf: Vec<u8> = Vec::new();

    // --- MP floating pointer structure (16 bytes) ---
    buf.extend_from_slice(b"_MP_"); // signature
    push_u32(&mut buf, mpc_base); // physical address of the configuration table
    buf.push(1); // length, in 16-byte paragraphs
    buf.push(4); // specification revision: 1.4
    buf.push(0); // checksum placeholder (offset 10)
    buf.push(0); // feature1 = 0: a configuration table is present (not a default config)
    buf.extend_from_slice(&[0u8; 4]); // feature2..5
    debug_assert_eq!(buf.len(), 16);
    buf[10] = checksum(&buf[0..16]);

    // --- MP configuration table header (44 bytes); fields back-filled after the entries ---
    let hdr = buf.len();
    buf.extend_from_slice(b"PCMP"); // signature
    push_u16(&mut buf, 0); // base table length (offset hdr+4) — back-filled
    buf.push(4); // specification revision: 1.4
    buf.push(0); // checksum (offset hdr+7) — back-filled
    buf.extend_from_slice(b"NVX     "); // OEM id (8 bytes)
    buf.extend_from_slice(b"MICROVM     "); // product id (12 bytes)
    push_u32(&mut buf, 0); // OEM table pointer
    push_u16(&mut buf, 0); // OEM table size
    push_u16(&mut buf, 0); // entry count (offset hdr+34) — back-filled
    push_u32(&mut buf, APIC_DEFAULT_PHYS_BASE); // local APIC address
    push_u16(&mut buf, 0); // extended table length
    buf.push(0); // extended table checksum
    buf.push(0); // reserved
    debug_assert_eq!(buf.len() - hdr, 44);

    let mut entry_count: u16 = 0;

    // --- Processor entries: one per vCPU, APIC id = index (20 bytes each) ---
    for apic_id in 0..num_cpus {
        buf.push(MP_PROCESSOR);
        buf.push(apic_id); // local APIC id
        buf.push(APIC_VERSION);
        buf.push(if apic_id == 0 {
            CPU_ENABLED | CPU_BOOTPROCESSOR
        } else {
            CPU_ENABLED
        });
        push_u32(&mut buf, 0x0000_0600); // CPU signature (family 6)
        push_u32(&mut buf, 0x0000_0201); // feature flags (APIC | FPU)
        push_u32(&mut buf, 0); // reserved[0]
        push_u32(&mut buf, 0); // reserved[1]
        entry_count += 1;
    }

    // --- Bus entry: a single ISA bus (8 bytes) ---
    buf.push(MP_BUS);
    buf.push(0); // bus id
    buf.extend_from_slice(b"ISA   "); // bus type string (6 bytes)
    entry_count += 1;

    // --- IO-APIC entry (8 bytes). Its APIC id is the first id after all the CPUs. ---
    let ioapic_id: u8 = num_cpus;
    buf.push(MP_IOAPIC);
    buf.push(ioapic_id);
    buf.push(APIC_VERSION);
    buf.push(MPC_APIC_USABLE);
    push_u32(&mut buf, IO_APIC_DEFAULT_PHYS_BASE);
    entry_count += 1;

    // --- IO interrupt assignments: ISA IRQ i -> IO-APIC pin i (8 bytes each) ---
    for irq in 0..NUM_ISA_IRQS {
        buf.push(MP_INTSRC);
        buf.push(MP_INT); // interrupt type
        push_u16(&mut buf, 0); // flags: bus-default polarity/trigger
        buf.push(0); // source bus id (the ISA bus above)
        buf.push(irq); // source bus IRQ
        buf.push(ioapic_id); // destination IO-APIC id
        buf.push(irq); // destination IO-APIC pin
        entry_count += 1;
    }

    // --- Local interrupt assignments: ExtINT -> LINT0, NMI -> LINT1 (8 bytes each) ---
    buf.push(MP_LINTSRC);
    buf.push(MP_EXTINT);
    push_u16(&mut buf, 0);
    buf.push(0); // source bus id
    buf.push(0); // source bus IRQ
    buf.push(0); // destination local APIC id (0 = BSP)
    buf.push(0); // destination LINT pin: LINT0
    entry_count += 1;

    buf.push(MP_LINTSRC);
    buf.push(MP_NMI);
    push_u16(&mut buf, 0);
    buf.push(0);
    buf.push(0);
    buf.push(0xFF); // destination local APIC id 0xFF = broadcast to all CPUs
    buf.push(1); // destination LINT pin: LINT1
    entry_count += 1;

    // --- Back-fill the configuration-table header ---
    let table_len: u16 = (buf.len() - hdr) as u16;
    buf[hdr + 4..hdr + 6].copy_from_slice(&table_len.to_le_bytes());
    buf[hdr + 34..hdr + 36].copy_from_slice(&entry_count.to_le_bytes());
    // The configuration-table checksum covers the whole table (header + entries).
    let cksum: u8 = checksum(&buf[hdr..]);
    buf[hdr + 7] = cksum;

    Ok(buf)
}

/// Builds the MP table for `num_cpus` vCPUs and writes it into guest RAM at [`MP_TABLE_BASE`].
///
/// The bytes are assembled in a host buffer (with the floating-pointer checksum, the config-table
/// length, entry count, and config-table checksum back-filled) and copied into guest RAM in one
/// shot, so the guest never observes a partially written table.
pub fn write(mem: &GuestMemory, num_cpus: u8) -> Result<()> {
    let buf: Vec<u8> = build(num_cpus)?;
    mem.write_slice(MP_TABLE_BASE, &buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floating_pointer_is_well_formed() {
        let buf = build(4).unwrap();
        assert_eq!(&buf[0..4], b"_MP_");
        assert_eq!(buf[8], 1); // length in paragraphs
        assert_eq!(buf[9], 4); // spec 1.4
        // Floating-pointer checksum: the 16 bytes sum to zero.
        assert_eq!(buf[0..16].iter().fold(0u8, |a, &b| a.wrapping_add(b)), 0);
    }

    #[test]
    fn config_table_is_well_formed() {
        let buf = build(8).unwrap();
        let hdr = 16;
        assert_eq!(&buf[hdr..hdr + 4], b"PCMP");
        // Whole configuration table sums to zero.
        assert_eq!(buf[hdr..].iter().fold(0u8, |a, &b| a.wrapping_add(b)), 0);
        // Reported length matches the actual table span.
        let len = u16::from_le_bytes([buf[hdr + 4], buf[hdr + 5]]) as usize;
        assert_eq!(len, buf.len() - hdr);
    }

    #[test]
    fn one_processor_entry_per_cpu_with_bsp_flag() {
        for n in [1u8, 2, 4, 8, MAX_SUPPORTED_CPUS] {
            let buf = build(n).unwrap();
            let mut off = 16 + 44; // start of the first processor entry
            for apic_id in 0..n {
                assert_eq!(buf[off], MP_PROCESSOR);
                assert_eq!(buf[off + 1], apic_id);
                let flag = buf[off + 3];
                if apic_id == 0 {
                    assert_eq!(flag, CPU_ENABLED | CPU_BOOTPROCESSOR);
                } else {
                    assert_eq!(flag, CPU_ENABLED);
                }
                off += 20;
            }
        }
    }

    #[test]
    fn cpu_count_boundaries_are_enforced() {
        assert!(build(0).is_err());
        assert!(build(MAX_SUPPORTED_CPUS).is_ok());
        assert!(build(MAX_SUPPORTED_CPUS + 1).is_err());
    }
}

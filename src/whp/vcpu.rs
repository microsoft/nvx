// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The PVH entry register/segment state is the WHP analogue of the KVM backend of the Nanvix
// Micro-VM (uservm), Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Virtual Processor (WHP)
//!
//! Configures the single guest vCPU's register state to enter a Linux kernel through the
//! 32-bit PVH entry point. Per the PVH boot ABI the processor starts in 32-bit protected mode
//! with paging disabled, flat 4 GiB code/data segments, and `%ebx` pointing at the
//! `hvm_start_info` structure; the kernel itself later switches to long mode.
//!

use ::anyhow::{
    Context,
    Result,
};
use ::windows::Win32::System::Hypervisor::{
    WHV_PARTITION_HANDLE,
    WHV_REGISTER_NAME,
    WHV_REGISTER_VALUE,
    WHV_X64_SEGMENT_REGISTER,
    WHV_X64_SEGMENT_REGISTER_0,
    WHV_X64_TABLE_REGISTER,
    WHvSetVirtualProcessorRegisters,
    WHvX64RegisterCr0,
    WHvX64RegisterCr3,
    WHvX64RegisterCr4,
    WHvX64RegisterCs,
    WHvX64RegisterDs,
    WHvX64RegisterEfer,
    WHvX64RegisterEs,
    WHvX64RegisterFs,
    WHvX64RegisterGdtr,
    WHvX64RegisterGs,
    WHvX64RegisterIdtr,
    WHvX64RegisterLdtr,
    WHvX64RegisterRbx,
    WHvX64RegisterRflags,
    WHvX64RegisterRip,
    WHvX64RegisterRsp,
    WHvX64RegisterSs,
    WHvX64RegisterTr,
};

use crate::layout::{
    BOOT_GDT_ADDR,
    BOOT_GDT_MAX,
    BOOT_IDT_ADDR,
};
use crate::whp::memory::GuestMemory;

/// `CR0.PE` — protected mode enable.
const X86_CR0_PE: u64 = 0x1;
/// `CR0.NE` — numeric-error reporting. Required to be set in a VMX guest CR0 (a fixed-1 bit),
/// so it is included even though the PVH ABI only mandates `PE`.
const X86_CR0_NE: u64 = 0x20;
/// Reserved `RFLAGS` bit that must always be set.
const RFLAGS_RESERVED: u64 = 0x2;

/// Segment access-byte + granularity flags, packed exactly as the WHP segment `Attributes`
/// field expects (`Type:4 | S:1 | DPL:2 | P:1 | .. | AVL:1 | L:1 | DB:1 | G:1`). These match
/// the values used for the KVM boot GDT.
const SEG_ATTR_CODE: u16 = 0xc09b;
const SEG_ATTR_DATA: u16 = 0xc093;
const SEG_ATTR_TSS: u16 = 0x008b;

///
/// # Description
///
/// Programs the vCPU register state for the PVH entry point.
///
/// # Parameters
///
/// - `partition`: The partition handle owning the vCPU.
/// - `vp_index`: Index of the vCPU to program.
/// - `mem`: Guest memory, used to install the boot GDT.
/// - `entry`: Guest-physical PVH entry point (`%eip`).
/// - `start_info_gpa`: Guest-physical address of `hvm_start_info` (`%ebx`).
///
pub fn setup_pvh(
    partition: WHV_PARTITION_HANDLE,
    vp_index: u32,
    mem: &GuestMemory,
    entry: u64,
    start_info_gpa: u64,
) -> Result<()> {
    // Boot GDT: null descriptor, flat 32-bit code, flat 32-bit data, and a TSS. The guest
    // caches are set directly below, but a valid GDTR/GDT must still exist for the kernel's
    // early `lgdt`.
    let gdt: [u64; 4] = [
        gdt_entry(0, 0, 0),
        gdt_entry(0xc09b, 0, 0x000f_ffff),
        gdt_entry(0xc093, 0, 0x000f_ffff),
        gdt_entry(0x008b, 0, 0x67),
    ];
    assert!(gdt.len() <= BOOT_GDT_MAX);
    for (i, entry) in gdt.iter().enumerate() {
        mem.write_slice(BOOT_GDT_ADDR + (i * 8) as u64, &entry.to_le_bytes())?;
    }
    // Empty IDT (interrupts are masked at PVH entry).
    mem.write_slice(BOOT_IDT_ADDR, &0u64.to_le_bytes())?;

    // Flat 4 GiB code/data with a byte-granular limit, a present TSS, and a null LDTR.
    let names: [WHV_REGISTER_NAME; 18] = [
        WHvX64RegisterCs,
        WHvX64RegisterDs,
        WHvX64RegisterEs,
        WHvX64RegisterFs,
        WHvX64RegisterGs,
        WHvX64RegisterSs,
        WHvX64RegisterTr,
        WHvX64RegisterLdtr,
        WHvX64RegisterGdtr,
        WHvX64RegisterIdtr,
        WHvX64RegisterCr0,
        WHvX64RegisterCr3,
        WHvX64RegisterCr4,
        WHvX64RegisterEfer,
        WHvX64RegisterRflags,
        WHvX64RegisterRip,
        WHvX64RegisterRbx,
        WHvX64RegisterRsp,
    ];
    let values: [WHV_REGISTER_VALUE; 18] = [
        segment(0, 0xffff_ffff, 0x08, SEG_ATTR_CODE),
        segment(0, 0xffff_ffff, 0x10, SEG_ATTR_DATA),
        segment(0, 0xffff_ffff, 0x10, SEG_ATTR_DATA),
        segment(0, 0xffff_ffff, 0x10, SEG_ATTR_DATA),
        segment(0, 0xffff_ffff, 0x10, SEG_ATTR_DATA),
        segment(0, 0xffff_ffff, 0x10, SEG_ATTR_DATA),
        segment(0, 0x67, 0x18, SEG_ATTR_TSS),
        segment(0, 0, 0, 0),
        table(BOOT_GDT_ADDR, (size_of::<u64>() * gdt.len() - 1) as u16),
        table(BOOT_IDT_ADDR, 0),
        reg64(X86_CR0_PE | X86_CR0_NE),
        reg64(0),
        reg64(0),
        reg64(0),
        reg64(RFLAGS_RESERVED),
        reg64(entry),
        reg64(start_info_gpa),
        reg64(0),
    ];

    // SAFETY: `names` and `values` are parallel arrays of the same length that outlive the
    // call; the partition and vCPU index are valid.
    unsafe {
        WHvSetVirtualProcessorRegisters(
            partition,
            vp_index,
            names.as_ptr(),
            names.len() as u32,
            values.as_ptr(),
        )
        .context("WHvSetVirtualProcessorRegisters (PVH entry state) failed")?;
    }

    Ok(())
}

/// Builds a segment register value with the given base, byte-granular limit, selector, and
/// packed attribute word.
fn segment(base: u64, limit: u32, selector: u16, attributes: u16) -> WHV_REGISTER_VALUE {
    WHV_REGISTER_VALUE {
        Segment: WHV_X64_SEGMENT_REGISTER {
            Base: base,
            Limit: limit,
            Selector: selector,
            Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
                Attributes: attributes,
            },
        },
    }
}

/// Builds a descriptor-table register value (for `GDTR`/`IDTR`).
fn table(base: u64, limit: u16) -> WHV_REGISTER_VALUE {
    WHV_REGISTER_VALUE {
        Table: WHV_X64_TABLE_REGISTER {
            Pad: [0; 3],
            Limit: limit,
            Base: base,
        },
    }
}

/// Builds a 64-bit scalar register value.
fn reg64(value: u64) -> WHV_REGISTER_VALUE {
    WHV_REGISTER_VALUE { Reg64: value }
}

/// Builds a raw 64-bit GDT descriptor from access `flags`, `base`, and `limit`.
fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((u64::from(base) & 0xff00_0000) << 32)
        | ((u64::from(flags) & 0x0000_f0ff) << 40)
        | ((u64::from(limit) & 0x000f_0000) << 32)
        | ((u64::from(base) & 0x00ff_ffff) << 16)
        | (u64::from(limit) & 0x0000_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_segment_attributes_match_the_descriptor_flags() {
        // The WHP `Attributes` word is laid out identically to the descriptor access+flags
        // byte pair the KVM boot GDT uses, so they must be numerically equal.
        assert_eq!(SEG_ATTR_CODE, 0xc09b);
        assert_eq!(SEG_ATTR_DATA, 0xc093);
        assert_eq!(SEG_ATTR_TSS, 0x008b);
    }

    #[test]
    fn gdt_entry_encodes_a_flat_code_segment() {
        // Flat 4 GiB code descriptor: base 0, granular limit 0xF_FFFF, access 0xc09b.
        let code = gdt_entry(0xc09b, 0, 0x000f_ffff);
        assert_eq!(code, 0x00cf_9b00_0000_ffff);
    }
}

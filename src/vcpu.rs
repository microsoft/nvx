// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The vCPU management approach is derived from the KVM backend of the Nanvix Micro-VM
// (uservm), Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Virtual Processor
//!
//! Creation and configuration of the single guest vCPU, including the register state
//! required to enter a Linux kernel through the 32-bit PVH entry point.
//!

use ::anyhow::{
    Context,
    Result,
};
use ::kvm_bindings::{
    KVM_MAX_CPUID_ENTRIES,
    kvm_regs,
    kvm_segment,
    kvm_sregs,
};
use ::kvm_ioctls::{
    Kvm,
    VcpuFd,
    VmFd,
};

use crate::layout::{
    BOOT_GDT_ADDR,
    BOOT_GDT_MAX,
    BOOT_IDT_ADDR,
};
use crate::memory::GuestMemory;

/// `CR0.PE` — protected mode enable.
const X86_CR0_PE: u64 = 0x1;
/// Reserved `RFLAGS` bit that must always be set.
const RFLAGS_RESERVED: u64 = 0x2;

/// The single guest virtual processor.
pub struct Vcpu {
    /// KVM vCPU handle.
    pub fd: VcpuFd,
}

impl Vcpu {
    /// Creates vCPU `id` and programs its CPUID from the host-supported set.
    pub fn new(kvm: &Kvm, vm_fd: &VmFd, id: u64) -> Result<Self> {
        let fd: VcpuFd = vm_fd
            .create_vcpu(id)
            .context("KVM_CREATE_VCPU failed")?;

        let cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .context("KVM_GET_SUPPORTED_CPUID failed")?;
        fd.set_cpuid2(&cpuid).context("KVM_SET_CPUID2 failed")?;

        Ok(Self { fd })
    }

    ///
    /// # Description
    ///
    /// Configures the vCPU register state for the PVH entry point.
    ///
    /// Per the PVH boot ABI the processor starts in 32-bit protected mode with paging
    /// disabled, flat 4 GiB code/data segments, and `%ebx` pointing at the
    /// `hvm_start_info` structure. The kernel itself later switches to long mode.
    ///
    /// # Parameters
    ///
    /// - `mem`: Guest memory, used to install the boot GDT.
    /// - `entry`: Guest-physical PVH entry point (`%eip`).
    /// - `start_info_gpa`: Guest-physical address of `hvm_start_info` (`%ebx`).
    ///
    pub fn setup_pvh(&self, mem: &GuestMemory, entry: u64, start_info_gpa: u64) -> Result<()> {
        // Boot GDT: null descriptor, flat 32-bit code, flat 32-bit data, and a TSS.
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

        let mut sregs: kvm_sregs = self.fd.get_sregs().context("KVM_GET_SREGS failed")?;

        let code_seg: kvm_segment = kvm_segment_from_gdt(gdt[1], 1);
        let data_seg: kvm_segment = kvm_segment_from_gdt(gdt[2], 2);
        let tss_seg: kvm_segment = kvm_segment_from_gdt(gdt[3], 3);
        sregs.cs = code_seg;
        sregs.ds = data_seg;
        sregs.es = data_seg;
        sregs.fs = data_seg;
        sregs.gs = data_seg;
        sregs.ss = data_seg;
        sregs.tr = tss_seg;

        sregs.gdt.base = BOOT_GDT_ADDR;
        sregs.gdt.limit = (size_of::<u64>() * gdt.len() - 1) as u16;
        sregs.idt.base = BOOT_IDT_ADDR;
        sregs.idt.limit = (size_of::<u64>() - 1) as u16;

        // 32-bit protected mode, paging disabled.
        sregs.cr0 = X86_CR0_PE;
        sregs.cr3 = 0;
        sregs.cr4 = 0;
        sregs.efer = 0;

        self.fd.set_sregs(&sregs).context("KVM_SET_SREGS failed")?;

        let regs: kvm_regs = kvm_regs {
            rflags: RFLAGS_RESERVED,
            rip: entry,
            rbx: start_info_gpa,
            ..Default::default()
        };
        self.fd.set_regs(&regs).context("KVM_SET_REGS failed")?;

        Ok(())
    }
}

/// Builds a raw 64-bit GDT descriptor from access `flags`, `base`, and `limit`.
fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((u64::from(base) & 0xff00_0000) << 32)
        | ((u64::from(flags) & 0x0000_f0ff) << 40)
        | ((u64::from(limit) & 0x000f_0000) << 32)
        | ((u64::from(base) & 0x00ff_ffff) << 16)
        | (u64::from(limit) & 0x0000_ffff)
}

/// Converts a raw GDT descriptor into a KVM segment cache for `table_index`.
///
/// The `limit` returned to KVM is the *byte-granular* effective limit: VMX interprets the
/// VMCS segment limit directly, so a granular (4 KiB) descriptor must be expanded here.
fn kvm_segment_from_gdt(entry: u64, table_index: u8) -> kvm_segment {
    let base: u64 = ((entry & 0xff00_0000_0000_0000) >> 32)
        | ((entry & 0x0000_00ff_0000_0000) >> 16)
        | ((entry & 0x0000_0000_ffff_0000) >> 16);
    let g: u8 = ((entry & 0x0080_0000_0000_0000) >> 55) as u8;
    let raw_limit: u32 =
        (((entry & 0x000f_0000_0000_0000) >> 32) | (entry & 0x0000_0000_0000_ffff)) as u32;
    let limit: u32 = if g != 0 {
        (raw_limit << 12) | 0xfff
    } else {
        raw_limit
    };
    let present: u8 = ((entry & 0x0000_8000_0000_0000) >> 47) as u8;

    kvm_segment {
        base,
        limit,
        selector: u16::from(table_index) * 8,
        type_: ((entry & 0x0000_0f00_0000_0000) >> 40) as u8,
        present,
        dpl: ((entry & 0x0000_6000_0000_0000) >> 45) as u8,
        db: ((entry & 0x0040_0000_0000_0000) >> 54) as u8,
        s: ((entry & 0x0000_1000_0000_0000) >> 44) as u8,
        l: ((entry & 0x0020_0000_0000_0000) >> 53) as u8,
        g,
        avl: ((entry & 0x0010_0000_0000_0000) >> 52) as u8,
        unusable: u8::from(present == 0),
        padding: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_code_segment_has_byte_granular_limit() {
        // This is the setup that a PVH guest enters with. The effective (byte-granular)
        // limit MUST be 4 GiB, otherwise the first instruction fetch above 1 MiB #GPs.
        let code = kvm_segment_from_gdt(gdt_entry(0xc09b, 0, 0x000f_ffff), 1);
        assert_eq!(code.limit, 0xffff_ffff, "granularity must expand the limit");
        assert_eq!(code.base, 0);
        assert_eq!(code.selector, 0x08);
        assert_eq!(code.db, 1);
        assert_eq!(code.l, 0);
        assert_eq!(code.g, 1);
        assert_eq!(code.s, 1);
        assert_eq!(code.present, 1);
        assert_eq!(code.type_, 0xb);
        assert_eq!(code.unusable, 0);
    }

    #[test]
    fn flat_data_segment_is_writable() {
        let data = kvm_segment_from_gdt(gdt_entry(0xc093, 0, 0x000f_ffff), 2);
        assert_eq!(data.limit, 0xffff_ffff);
        assert_eq!(data.selector, 0x10);
        assert_eq!(data.type_, 0x3);
        assert_eq!(data.s, 1);
    }

    #[test]
    fn tss_segment_keeps_small_limit() {
        // A non-granular descriptor keeps its literal limit.
        let tss = kvm_segment_from_gdt(gdt_entry(0x008b, 0, 0x67), 3);
        assert_eq!(tss.limit, 0x67);
        assert_eq!(tss.g, 0);
        assert_eq!(tss.type_, 0xb);
        assert_eq!(tss.present, 1);
    }
}

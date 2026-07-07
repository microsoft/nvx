// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # Guest Memory Layout
//!
//! Defines the guest-physical memory map used by the micro-VM. The layout follows the
//! conventions used by minimal PVH-capable VMMs (Firecracker / cloud-hypervisor): low
//! scratch structures for the boot GDT and the PVH `hvm_start_info`, the kernel command
//! line, then the kernel image and the initramfs in high RAM.
//!

/// Size of a 4 KiB page.
pub const PAGE_SIZE: u64 = 0x1000;

/// Guest-physical address of the boot GDT.
pub const BOOT_GDT_ADDR: u64 = 0x0000_0500;
/// Maximum number of 8-byte entries in the boot GDT.
pub const BOOT_GDT_MAX: usize = 4;

/// Guest-physical address of the boot IDT (empty; interrupts are disabled at PVH entry).
pub const BOOT_IDT_ADDR: u64 = 0x0000_0520;

/// Guest-physical address of the PVH `hvm_start_info` structure.
pub const PVH_START_INFO_ADDR: u64 = 0x0000_6000;
/// Guest-physical address of the PVH module list (initramfs descriptor).
pub const PVH_MODLIST_ADDR: u64 = 0x0000_6040;
/// Guest-physical address of the PVH memory map table.
pub const PVH_MEMMAP_ADDR: u64 = 0x0000_7000;

/// Guest-physical address of the kernel command line.
pub const CMDLINE_ADDR: u64 = 0x0002_0000;
/// Maximum length of the kernel command line, in bytes.
pub const CMDLINE_MAX_LEN: usize = 0x1_0000;

/// Start of the high MMIO gap (also the top of usable low RAM). Guest RAM never
/// overlaps this region so that the in-kernel LAPIC (0xFEE0_0000) and IOAPIC
/// (0xFEC0_0000) remain accessible.
pub const MMIO_GAP_START: u64 = 0xC000_0000;

/// Address where 64-bit RAM resumes above the 4 GiB boundary.
pub const RAM_64BIT_START: u64 = 0x1_0000_0000;

/// First byte of the high (>= 1 MiB) RAM region. The kernel ELF is loaded at its own
/// physical load addresses, which for an x86_64 `vmlinux` start at 1 MiB.
pub const HIMEM_START: u64 = 0x0010_0000;

/// Rounds `value` down to the nearest multiple of `align` (a power of two).
pub const fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

/// Rounds `value` up to the nearest multiple of `align` (a power of two).
pub const fn align_up(value: u64, align: u64) -> u64 {
    align_down(value + align - 1, align)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_down_rounds_toward_zero() {
        assert_eq!(align_down(0x1234, PAGE_SIZE), 0x1000);
        assert_eq!(align_down(0x1000, PAGE_SIZE), 0x1000);
        assert_eq!(align_down(0x0fff, PAGE_SIZE), 0x0000);
    }

    #[test]
    fn align_up_rounds_away_from_zero() {
        assert_eq!(align_up(0x1234, PAGE_SIZE), 0x2000);
        assert_eq!(align_up(0x1000, PAGE_SIZE), 0x1000);
        assert_eq!(align_up(0x0001, PAGE_SIZE), 0x1000);
    }
}

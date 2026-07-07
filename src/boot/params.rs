// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # PVH Boot Parameters
//!
//! Definitions of the data structures exchanged with a Linux kernel booted through the
//! x86/PVH boot protocol. See the Xen public header `arch-x86/hvm/start_info.h` and the
//! Linux `Documentation/virt/arch/x86/pvh.rst`.
//!
//! At the PVH entry point the vCPU is in 32-bit protected mode with paging disabled and
//! `%ebx` holds the guest-physical address of an [`HvmStartInfo`] structure.
//!

/// Magic value stored in [`HvmStartInfo::magic`] ("xEn3" little-endian).
pub const XEN_HVM_START_MAGIC_VALUE: u32 = 0x336e_c578;

/// Memory map entry type: usable RAM.
pub const XEN_HVM_MEMMAP_TYPE_RAM: u32 = 1;
/// Memory map entry type: reserved / unusable.
#[allow(dead_code)]
pub const XEN_HVM_MEMMAP_TYPE_RESERVED: u32 = 2;

/// ELF note type identifying the 32-bit PVH entry point (`XEN_ELFNOTE_PHYS32_ENTRY`).
pub const XEN_ELFNOTE_PHYS32_ENTRY: u32 = 18;

/// Boot information handed to the guest at the PVH entry point.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HvmStartInfo {
    /// Magic number (`XEN_HVM_START_MAGIC_VALUE`).
    pub magic: u32,
    /// Version of this structure (1 to include the memory map fields).
    pub version: u32,
    /// SIF_* flags. Unused here.
    pub flags: u32,
    /// Number of modules (the initramfs counts as one module).
    pub nr_modules: u32,
    /// Guest-physical address of the module list.
    pub modlist_paddr: u64,
    /// Guest-physical address of the NUL-terminated command line.
    pub cmdline_paddr: u64,
    /// Guest-physical address of the ACPI RSDP (0 if none).
    pub rsdp_paddr: u64,
    /// Guest-physical address of the memory map table.
    pub memmap_paddr: u64,
    /// Number of entries in the memory map table.
    pub memmap_entries: u32,
    /// Reserved; must be zero.
    pub reserved: u32,
}

/// A single module descriptor (used for the initramfs).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HvmModlistEntry {
    /// Guest-physical address of the module.
    pub paddr: u64,
    /// Size of the module, in bytes.
    pub size: u64,
    /// Guest-physical address of the module's command line (0 if none).
    pub cmdline_paddr: u64,
    /// Reserved; must be zero.
    pub reserved: u64,
}

/// A single entry of the PVH memory map (an e820-like descriptor).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HvmMemmapTableEntry {
    /// Base guest-physical address of the region.
    pub addr: u64,
    /// Size of the region, in bytes.
    pub size: u64,
    /// Region type (`XEN_HVM_MEMMAP_TYPE_*`).
    pub type_: u32,
    /// Reserved; must be zero.
    pub reserved: u32,
}

/// Returns the raw little-endian bytes of a `repr(C)` POD value.
///
/// # Safety
///
/// `T` must be a plain-old-data `repr(C)` type without meaningful padding and without
/// pointers/references. All structures in this module satisfy that requirement.
pub fn as_bytes<T: Copy>(value: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` POD structure and we only read `size_of::<T>()` bytes from
    // a live reference, producing an immutable view with the same lifetime.
    unsafe { ::core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structures_match_the_pvh_abi_layout() {
        assert_eq!(size_of::<HvmStartInfo>(), 56);
        assert_eq!(size_of::<HvmModlistEntry>(), 32);
        assert_eq!(size_of::<HvmMemmapTableEntry>(), 24);
    }

    #[test]
    fn as_bytes_is_little_endian() {
        let entry = HvmMemmapTableEntry {
            addr: 0x1122_3344_5566_7788,
            size: 0,
            type_: XEN_HVM_MEMMAP_TYPE_RAM,
            reserved: 0,
        };
        let bytes = as_bytes(&entry);
        assert_eq!(bytes.len(), 24);
        assert_eq!(&bytes[0..8], &0x1122_3344_5566_7788_u64.to_le_bytes());
        assert_eq!(&bytes[16..20], &1u32.to_le_bytes());
    }
}

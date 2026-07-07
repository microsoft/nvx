// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # PVH Kernel Loader
//!
//! Loads an uncompressed `vmlinux` ELF image into guest RAM, locates the PVH 32-bit entry
//! point (`XEN_ELFNOTE_PHYS32_ENTRY`), loads the initramfs, and lays out the
//! [`HvmStartInfo`] boot structures the kernel reads at entry.
//!

use ::anyhow::{
    Context,
    Result,
    bail,
};

use crate::boot::params::{
    HvmMemmapTableEntry,
    HvmModlistEntry,
    HvmStartInfo,
    XEN_ELFNOTE_PHYS32_ENTRY,
    XEN_HVM_MEMMAP_TYPE_RAM,
    XEN_HVM_START_MAGIC_VALUE,
    as_bytes,
};
use crate::layout::{
    CMDLINE_ADDR,
    CMDLINE_MAX_LEN,
    HIMEM_START,
    MMIO_GAP_START,
    PAGE_SIZE,
    PVH_MEMMAP_ADDR,
    PVH_MODLIST_ADDR,
    PVH_START_INFO_ADDR,
    align_down,
    align_up,
};
use crate::memory::GuestMemory;

/// ELF program header type: loadable segment.
const PT_LOAD: u32 = 1;
/// ELF program header type: auxiliary note.
const PT_NOTE: u32 = 4;

/// Result of loading the kernel image.
pub struct LoadedKernel {
    /// Guest-physical address of the PVH 32-bit entry point.
    pub pvh_entry: u64,
    /// Guest-physical address just past the highest byte occupied by the kernel image.
    pub kernel_end: u64,
}

/// Guest-physical placement of the initramfs.
#[derive(Clone, Copy)]
pub struct InitrdRegion {
    /// Base guest-physical address.
    pub addr: u64,
    /// Size in bytes.
    pub size: u64,
}

/// Reads a little-endian `u16` at `off` from `buf`.
fn rd_u16(buf: &[u8], off: usize) -> Result<u16> {
    let end: usize = off + 2;
    let slice: &[u8] = buf.get(off..end).context("truncated ELF (u16)")?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

/// Reads a little-endian `u32` at `off` from `buf`.
fn rd_u32(buf: &[u8], off: usize) -> Result<u32> {
    let slice: &[u8] = buf.get(off..off + 4).context("truncated ELF (u32)")?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Reads a little-endian `u64` at `off` from `buf`.
fn rd_u64(buf: &[u8], off: usize) -> Result<u64> {
    let slice: &[u8] = buf.get(off..off + 8).context("truncated ELF (u64)")?;
    let mut arr: [u8; 8] = [0; 8];
    arr.copy_from_slice(slice);
    Ok(u64::from_le_bytes(arr))
}

///
/// # Description
///
/// Loads a 64-bit `vmlinux` ELF image into guest RAM and returns its PVH entry point.
///
/// Each `PT_LOAD` segment is copied to its physical (`p_paddr`) address; BSS is already
/// zero because guest RAM is freshly mapped. The `PT_NOTE` segments are scanned for the
/// `XEN_ELFNOTE_PHYS32_ENTRY` note that carries the 32-bit PVH entry point.
///
pub fn load_kernel(mem: &GuestMemory, image: &[u8]) -> Result<LoadedKernel> {
    // Validate the ELF identification bytes.
    if image.len() < 64 || &image[0..4] != b"\x7fELF" {
        bail!("kernel is not an ELF image");
    }
    if image[4] != 2 {
        bail!("kernel is not a 64-bit ELF image (EI_CLASS != ELFCLASS64)");
    }
    if image[5] != 1 {
        bail!("kernel is not little-endian (EI_DATA != ELFDATA2LSB)");
    }
    let e_machine: u16 = rd_u16(image, 18)?;
    if e_machine != 0x3e {
        bail!("kernel is not an x86_64 ELF image (e_machine={e_machine:#x})");
    }

    let e_phoff: u64 = rd_u64(image, 32)?;
    let e_phentsize: u16 = rd_u16(image, 54)?;
    let e_phnum: u16 = rd_u16(image, 56)?;
    if e_phentsize < 56 {
        bail!("unexpected ELF program header size ({e_phentsize})");
    }

    let mut pvh_entry: Option<u64> = None;
    let mut kernel_end: u64 = 0;

    for i in 0..e_phnum {
        let ph: usize = usize::try_from(e_phoff).context("e_phoff overflow")?
            + usize::from(e_phentsize) * usize::from(i);
        let p_type: u32 = rd_u32(image, ph)?;
        let p_offset: u64 = rd_u64(image, ph + 8)?;
        let p_paddr: u64 = rd_u64(image, ph + 24)?;
        let p_filesz: u64 = rd_u64(image, ph + 32)?;
        let p_memsz: u64 = rd_u64(image, ph + 40)?;

        match p_type {
            PT_LOAD => {
                let off: usize = usize::try_from(p_offset).context("p_offset overflow")?;
                let fsz: usize = usize::try_from(p_filesz).context("p_filesz overflow")?;
                let bytes: &[u8] = image
                    .get(off..off + fsz)
                    .context("PT_LOAD segment out of file bounds")?;
                mem.write_slice(p_paddr, bytes)
                    .with_context(|| format!("loading PT_LOAD segment at gpa={p_paddr:#x}"))?;
                kernel_end = kernel_end.max(p_paddr + p_memsz);
            },
            PT_NOTE => {
                if pvh_entry.is_none() {
                    let off: usize = usize::try_from(p_offset).context("note p_offset overflow")?;
                    let fsz: usize = usize::try_from(p_filesz).context("note p_filesz overflow")?;
                    let notes: &[u8] = image
                        .get(off..off + fsz)
                        .context("PT_NOTE segment out of file bounds")?;
                    pvh_entry = find_pvh_entry(notes)?;
                }
            },
            _ => {},
        }
    }

    let pvh_entry: u64 = pvh_entry.context(
        "kernel does not contain a PVH entry note (XEN_ELFNOTE_PHYS32_ENTRY); \
         rebuild with CONFIG_PVH=y",
    )?;

    Ok(LoadedKernel {
        pvh_entry,
        kernel_end: align_up(kernel_end, PAGE_SIZE),
    })
}

/// Scans a `PT_NOTE` payload for the `Xen`/`XEN_ELFNOTE_PHYS32_ENTRY` note and returns the
/// 32-bit PVH entry point it carries.
fn find_pvh_entry(notes: &[u8]) -> Result<Option<u64>> {
    let mut pos: usize = 0;
    while pos + 12 <= notes.len() {
        let namesz: usize = rd_u32(notes, pos)? as usize;
        let descsz: usize = rd_u32(notes, pos + 4)? as usize;
        let ntype: u32 = rd_u32(notes, pos + 8)?;
        let name_off: usize = pos + 12;
        let desc_off: usize = name_off + namesz.next_multiple_of(4);
        let desc_end: usize = desc_off + descsz;
        if desc_end > notes.len() {
            break;
        }

        let name: &[u8] = &notes[name_off..name_off + namesz];
        if ntype == XEN_ELFNOTE_PHYS32_ENTRY && name.starts_with(b"Xen") {
            let desc: &[u8] = &notes[desc_off..desc_end];
            let entry: u64 = match desc.len() {
                l if l >= 8 => rd_u64(desc, 0)?,
                4 => u64::from(rd_u32(desc, 0)?),
                other => bail!("unexpected PVH entry note descriptor size ({other})"),
            };
            return Ok(Some(entry));
        }

        pos = desc_end.next_multiple_of(4);
    }
    Ok(None)
}

///
/// # Description
///
/// Loads the initramfs into high guest RAM, just below the top of low memory and above the
/// kernel image, and returns its placement.
///
pub fn load_initramfs(
    mem: &GuestMemory,
    image: &[u8],
    kernel_end: u64,
    ram_size: u64,
) -> Result<InitrdRegion> {
    let size: u64 = image.len() as u64;
    let low_ram_end: u64 = ram_size.min(MMIO_GAP_START);
    if size > low_ram_end {
        bail!("initramfs ({size} bytes) does not fit in guest RAM");
    }

    let addr: u64 = align_down(low_ram_end - size, PAGE_SIZE);
    if addr < kernel_end.max(HIMEM_START) {
        bail!(
            "not enough guest RAM for initramfs (need it above {:#x}, only reached {addr:#x}); \
             increase --mem",
            kernel_end.max(HIMEM_START)
        );
    }

    mem.write_slice(addr, image).context("loading initramfs")?;
    Ok(InitrdRegion { addr, size })
}

///
/// # Description
///
/// Writes the command line and the PVH boot structures ([`HvmStartInfo`], the memory map,
/// and the optional initramfs module descriptor) into guest RAM.
///
/// # Returns
///
/// The guest-physical address of the [`HvmStartInfo`] structure, to be placed in `%ebx`.
///
pub fn configure(mem: &GuestMemory, cmdline: &str, initrd: Option<InitrdRegion>) -> Result<u64> {
    // Command line (NUL-terminated).
    let cmdline_bytes: &[u8] = cmdline.as_bytes();
    if cmdline_bytes.len() + 1 > CMDLINE_MAX_LEN {
        bail!("kernel command line is too long ({} bytes)", cmdline_bytes.len());
    }
    mem.write_slice(CMDLINE_ADDR, cmdline_bytes)?;
    mem.write_slice(CMDLINE_ADDR + cmdline_bytes.len() as u64, &[0u8])?;

    // Memory map: describe every RAM region as usable RAM.
    let ram_regions: Vec<(u64, u64)> = mem.ram_regions();
    let mut memmap_addr: u64 = PVH_MEMMAP_ADDR;
    for (base, size) in &ram_regions {
        let entry: HvmMemmapTableEntry = HvmMemmapTableEntry {
            addr: *base,
            size: *size,
            type_: XEN_HVM_MEMMAP_TYPE_RAM,
            reserved: 0,
        };
        mem.write_obj(memmap_addr, &entry)?;
        memmap_addr += size_of::<HvmMemmapTableEntry>() as u64;
    }

    // Module list (initramfs, if any).
    let nr_modules: u32 = match initrd {
        Some(region) => {
            let entry: HvmModlistEntry = HvmModlistEntry {
                paddr: region.addr,
                size: region.size,
                cmdline_paddr: 0,
                reserved: 0,
            };
            mem.write_obj(PVH_MODLIST_ADDR, &entry)?;
            1
        },
        None => 0,
    };

    // Start info.
    let start_info: HvmStartInfo = HvmStartInfo {
        magic: XEN_HVM_START_MAGIC_VALUE,
        version: 1,
        flags: 0,
        nr_modules,
        modlist_paddr: if nr_modules > 0 { PVH_MODLIST_ADDR } else { 0 },
        cmdline_paddr: CMDLINE_ADDR,
        rsdp_paddr: 0,
        memmap_paddr: PVH_MEMMAP_ADDR,
        memmap_entries: ram_regions.len() as u32,
        reserved: 0,
    };
    mem.write_slice(PVH_START_INFO_ADDR, as_bytes(&start_info))?;

    Ok(PVH_START_INFO_ADDR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_note(buf: &mut Vec<u8>, name: &[u8], ntype: u32, desc: &[u8]) {
        buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        buf.extend_from_slice(&ntype.to_le_bytes());
        buf.extend_from_slice(name);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
        buf.extend_from_slice(desc);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
    }

    #[test]
    fn finds_pvh_entry_after_unrelated_note() {
        let mut notes = Vec::new();
        push_note(&mut notes, b"GNU\0", 3, &[1, 2, 3, 4]);
        push_note(&mut notes, b"Xen\0", XEN_ELFNOTE_PHYS32_ENTRY, &0x0028_060b_u64.to_le_bytes());
        assert_eq!(find_pvh_entry(&notes).unwrap(), Some(0x0028_060b));
    }

    #[test]
    fn absent_pvh_entry_returns_none() {
        let mut notes = Vec::new();
        push_note(&mut notes, b"GNU\0", 3, &[9, 9, 9, 9]);
        assert_eq!(find_pvh_entry(&notes).unwrap(), None);
    }
}

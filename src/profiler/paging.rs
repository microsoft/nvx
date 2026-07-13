// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! x86_64 4-level (PML4) and 5-level (PML5, `CR4.LA57`) guest page-table walking and address-space
//! classification for stack sampling.

/// Mask selecting the page-directory base of `CR3`, discarding PCID and flag/reserved bits so the
/// same address space maps to one id regardless of PCID or control-bit noise.
pub(crate) const CR3_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

/// Returns whether `addr` is a canonical x86-64 virtual address.
///
/// The canonical width depends on the paging mode of the sampled address space: 48 bits with
/// 4-level paging, 57 bits when the guest runs with 5-level paging (`CR4.LA57`). A 48-bit check on
/// an LA57 guest would wrongly reject valid high user addresses (bits 47..56), truncating stacks.
pub(crate) fn is_canonical(addr: u64, la57: bool) -> bool {
    if la57 {
        // Bits 63:57 must replicate bit 56: the 8-bit field 63:56 is all-zero or all-one.
        let top = addr >> 56;
        top == 0 || top == 0xff
    } else {
        // Bits 63:48 must replicate bit 47: the 17-bit field 63:47 is all-zero or all-one.
        let top = addr >> 47;
        top == 0 || top == 0x1_ffff
    }
}

/// The address-space id a sample belongs to.
///
/// A user-space leaf (`RIP` in the lower canonical half) is attributed to its process via the
/// normalized `CR3`; a kernel leaf (upper half) is shared across processes and maps to `0`, so
/// kernel stacks aggregate globally.
pub(crate) fn address_space_of(rip: u64, cr3: u64) -> u64 {
    if rip >> 63 == 0 {
        cr3 & CR3_ADDR_MASK
    } else {
        0
    }
}

/// A guest frame-pointer walker with a one-page translation cache.
///
/// The two reads of every frame — and consecutive frames — nearly always land on the same guest
/// stack page. Caching the last translated page turns each repeat access into a single physical
/// read instead of a fresh 4-level page walk.
pub(crate) struct PageWalk {
    cr3: u64,
    la57: bool,
    cache_gva_page: Option<u64>,
    cache_gpa_page: u64,
}

impl PageWalk {
    pub(crate) fn new(cr3: u64, la57: bool) -> Self {
        Self {
            cr3,
            la57,
            cache_gva_page: None,
            cache_gpa_page: 0,
        }
    }

    /// Reads a little-endian `u64` at guest-virtual address `gva`, translating through the cached
    /// page mapping when possible.
    pub(crate) fn read_u64<F>(&mut self, gva: u64, read_phys: &mut F) -> Option<u64>
    where
        F: FnMut(u64, &mut [u8]) -> bool,
    {
        // An 8-byte read that would straddle two pages cannot be served by a single-page
        // translation (the adjacent virtual page may map to a different physical page), so reject it
        // rather than return bytes stitched across the wrong physical page.
        if gva & 0xfff > 0x1000 - 8 {
            return None;
        }
        let page = gva & !0xfff;
        let gpa = if self.cache_gva_page == Some(page) {
            self.cache_gpa_page | (gva & 0xfff)
        } else {
            let gpa = gva_to_gpa(self.cr3, gva, self.la57, read_phys)?;
            self.cache_gva_page = Some(page);
            self.cache_gpa_page = gpa & !0xfff;
            gpa
        };
        read_phys_u64(gpa, read_phys)
    }
}

fn read_phys_u64<F>(gpa: u64, read_phys: &mut F) -> Option<u64>
where
    F: FnMut(u64, &mut [u8]) -> bool,
{
    let mut b = [0u8; 8];
    if !read_phys(gpa, &mut b) {
        return None;
    }
    Some(u64::from_le_bytes(b))
}

fn gva_to_gpa<F>(cr3: u64, gva: u64, la57: bool, read_phys: &mut F) -> Option<u64>
where
    F: FnMut(u64, &mut [u8]) -> bool,
{
    const P: u64 = 1 << 0;
    const PS: u64 = 1 << 7;
    const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

    // With 5-level paging (`CR4.LA57`) `CR3` points at the PML5; resolve one extra level down to
    // the PML4 base first. With 4-level paging `CR3` points directly at the PML4.
    let pml4 = if la57 {
        let pml5 = cr3 & ADDR_MASK;
        let pml5_idx = ((gva >> 48) & 0x1ff) * 8;
        let pml5e = read_phys_u64(pml5 + pml5_idx, read_phys)?;
        if pml5e & P == 0 {
            return None;
        }
        pml5e & ADDR_MASK
    } else {
        cr3 & ADDR_MASK
    };

    let pml4_idx = ((gva >> 39) & 0x1ff) * 8;
    let pdpt_idx = ((gva >> 30) & 0x1ff) * 8;
    let pd_idx = ((gva >> 21) & 0x1ff) * 8;
    let pt_idx = ((gva >> 12) & 0x1ff) * 8;

    let pml4e = read_phys_u64(pml4 + pml4_idx, read_phys)?;
    if pml4e & P == 0 {
        return None;
    }
    let pdpt = pml4e & ADDR_MASK;

    let pdpte = read_phys_u64(pdpt + pdpt_idx, read_phys)?;
    if pdpte & P == 0 {
        return None;
    }
    if pdpte & PS != 0 {
        let base = pdpte & 0x000f_ffff_c000_0000;
        return Some(base + (gva & 0x3fff_ffff));
    }
    let pd = pdpte & ADDR_MASK;

    let pde = read_phys_u64(pd + pd_idx, read_phys)?;
    if pde & P == 0 {
        return None;
    }
    if pde & PS != 0 {
        let base = pde & 0x000f_ffff_ffe0_0000;
        return Some(base + (gva & 0x1f_ffff));
    }
    let pt = pde & ADDR_MASK;

    let pte = read_phys_u64(pt + pt_idx, read_phys)?;
    if pte & P == 0 {
        return None;
    }
    let base = pte & ADDR_MASK;
    Some(base + (gva & 0xfff))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_canonical_rejects_non_canonical_addresses() {
        // 4-level paging: 48-bit canonical addresses.
        assert!(is_canonical(0x0000_0000_0040_1000, false));
        assert!(is_canonical(0xffff_ffff_8100_0000, false));
        assert!(!is_canonical(0x0000_8000_0000_0000, false));
        assert!(!is_canonical(0xdead_beef_dead_beef, false));
        // 5-level paging: 57-bit canonical. A high user address that uses bits 47..56 is
        // non-canonical under a 48-bit check but canonical under LA57.
        assert!(!is_canonical(0x0000_8000_0000_0000, false));
        assert!(is_canonical(0x0000_8000_0000_0000, true));
        assert!(is_canonical(0x00ff_ffff_ffff_f000, true));
        assert!(!is_canonical(0x0100_0000_0000_0000, true));
        assert!(is_canonical(0xffff_ffff_8100_0000, true));
    }

    #[test]
    fn gva_to_gpa_translates_4k_page() {
        let pml4: u64 = 0x1000;
        let pdpt: u64 = 0x2000;
        let pd: u64 = 0x3000;
        let pt: u64 = 0x4000;
        let page: u64 = 0x5000;

        let mut mem = vec![0u8; 0x7000];
        fn wr(mem: &mut [u8], gpa: u64, val: u64) {
            let off = gpa as usize;
            mem[off..off + 8].copy_from_slice(&val.to_le_bytes());
        }
        wr(&mut mem, pml4, pdpt | 1);
        wr(&mut mem, pdpt, pd | 1);
        wr(&mut mem, pd, pt | 1);
        wr(&mut mem, pt, page | 1);

        let mut read_phys = |gpa: u64, out: &mut [u8]| {
            let off = gpa as usize;
            if off + out.len() > mem.len() {
                return false;
            }
            out.copy_from_slice(&mem[off..off + out.len()]);
            true
        };

        let gva = 0x123u64;
        let got = gva_to_gpa(pml4, gva, false, &mut read_phys);
        assert_eq!(got, Some(page + 0x123));
    }

    #[test]
    fn gva_to_gpa_translates_5_level_page() {
        // With LA57, CR3 points at the PML5; the walk resolves one extra level before the PML4.
        let pml5: u64 = 0x1000;
        let pml4: u64 = 0x2000;
        let pdpt: u64 = 0x3000;
        let pd: u64 = 0x4000;
        let pt: u64 = 0x5000;
        let page: u64 = 0x6000;

        let mut mem = vec![0u8; 0x8000];
        fn wr(mem: &mut [u8], gpa: u64, val: u64) {
            let off = gpa as usize;
            mem[off..off + 8].copy_from_slice(&val.to_le_bytes());
        }
        // A high user address that exercises a non-zero PML5 index and stays canonical under LA57.
        let gva: u64 = (1u64 << 48) | 0x789;
        let pml5_idx = ((gva >> 48) & 0x1ff) * 8;
        wr(&mut mem, pml5 + pml5_idx, pml4 | 1);
        wr(&mut mem, pml4 + ((gva >> 39) & 0x1ff) * 8, pdpt | 1);
        wr(&mut mem, pdpt + ((gva >> 30) & 0x1ff) * 8, pd | 1);
        wr(&mut mem, pd + ((gva >> 21) & 0x1ff) * 8, pt | 1);
        wr(&mut mem, pt + ((gva >> 12) & 0x1ff) * 8, page | 1);

        let mut read_phys = |gpa: u64, out: &mut [u8]| {
            let off = gpa as usize;
            if off + out.len() > mem.len() {
                return false;
            }
            out.copy_from_slice(&mem[off..off + out.len()]);
            true
        };

        // 5-level walk resolves; a 4-level walk of the same CR3 would decode the wrong levels.
        assert_eq!(gva_to_gpa(pml5, gva, true, &mut read_phys), Some(page + 0x789));
    }

    #[test]
    fn read_u64_rejects_page_straddling_access() {
        // A read whose 8 bytes cross a page boundary is refused before any physical access: the
        // adjacent virtual page may map elsewhere, so a single-page translation cannot serve it.
        let mut walk = PageWalk::new(0x1000, false);
        let mut read_phys =
            |_gpa: u64, _out: &mut [u8]| panic!("must not translate a page-straddling read");
        assert_eq!(walk.read_u64(0xffc, &mut read_phys), None);
        assert_eq!(walk.read_u64(0xfff, &mut read_phys), None);
        assert_eq!(walk.read_u64(0x1ff9, &mut read_phys), None);
    }

    #[test]
    fn address_space_of_classifies_user_and_kernel() {
        // A user-half leaf address is keyed by its normalized CR3 (PCID/flag bits masked off).
        let cr3 = 0x1234_5678_9abc_dfffu64;
        assert_eq!(
            address_space_of(0x0000_0000_0040_1000, cr3),
            cr3 & CR3_ADDR_MASK
        );
        // A kernel-half leaf address shares address space 0 regardless of CR3, so kernel stacks
        // aggregate across processes.
        assert_eq!(address_space_of(0xffff_ffff_8100_0000, cr3), 0);
    }
}

// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! ELF symbol loading, executable-section bounding, cross-image overlap detection, and resolution.

use ::std::fs;

use ::anyhow::Result;
use ::goblin::elf::Elf;
use ::log::warn;

use super::SymbolSource;

#[derive(Clone, Debug)]
pub(crate) struct Symbol {
    addr: u64,
    size: u64,
    /// End of the symbol's containing executable section (relocated). An upper bound on
    /// resolution so an address past the section is never attributed to this symbol.
    sec_end: u64,
    name: String,
    /// Whether the symbol is `STT_FUNC` (preferred over `STT_NOTYPE` aliases at the same address).
    is_func: bool,
    /// Index of the source image, used to detect ambiguous overlaps between images.
    image: u32,
}

/// Exclusive end of the guest range `pc` may be attributed to symbol `s`.
///
/// A sized symbol owns `[addr, addr + size)`, clamped to its executable section end; a zero-size
/// symbol with a known section owns `[addr, sec_end)`; a symbol with no extent information at all
/// is unbounded (only the nearest preceding symbol may claim such an address).
fn resolve_end(s: &Symbol) -> u64 {
    let sized = if s.size != 0 {
        s.addr.saturating_add(s.size)
    } else {
        0
    };
    match (sized, s.sec_end) {
        (0, 0) => u64::MAX,
        (0, se) => se,
        (sz, 0) => sz,
        (sz, se) => sz.min(se),
    }
}

/// Precomputed per-table interval-ownership index for constant-time resolution.
///
/// For each symbol index `i`, records the furthest [`resolve_end`] of any *sized* symbol in
/// `symbols[0..=i]` and which symbol achieves it. Because the table is sorted by address, every
/// symbol at or before the one nearest a program counter starts at or below it, so the furthest-
/// reaching sized symbol whose extent passes the counter provably encloses it. This lets
/// [`resolve`] attribute an address that falls past its nearest symbol to an enclosing function in
/// O(1) instead of scanning backward — which is quadratic across many samples and, for a mis-based
/// image whose symbols never reach the sampled addresses, would rescan the whole table per frame.
///
/// `symbols` must be sorted and bounded by [`dedup_and_bound_symbols`] first.
pub(crate) struct ReachIndex {
    /// `reach[i]` = the maximum [`resolve_end`] over sized symbols in `symbols[0..=i]` (`0` if none
    /// so far). Monotonically non-decreasing.
    reach: Vec<u64>,
    /// `arg[i]` = index of the sized symbol achieving `reach[i]` (`usize::MAX` if none so far).
    arg: Vec<usize>,
}

impl ReachIndex {
    /// Builds the reach index for a sorted, bounded symbol table.
    pub(crate) fn build(symbols: &[Symbol]) -> Self {
        let mut reach = Vec::with_capacity(symbols.len());
        let mut arg = Vec::with_capacity(symbols.len());
        let mut best_end = 0u64;
        let mut best_idx = usize::MAX;
        for (i, s) in symbols.iter().enumerate() {
            // Only sized symbols may enclose an address past a nearer symbol's end: a zero-size
            // label has no reliable extent to bracket a nested inner symbol.
            if s.size != 0 {
                let end = resolve_end(s);
                if end > best_end {
                    best_end = end;
                    best_idx = i;
                }
            }
            reach.push(best_end);
            arg.push(best_idx);
        }
        Self { reach, arg }
    }
}

/// Resolves a sampled program counter to a symbol name.
///
/// Returns a raw `0x...` hex string when `pc` falls before the first symbol or outside every
/// symbol's extent, so an address is never misattributed to a neighboring symbol. When the nearest
/// preceding symbol has already ended, resolution falls back to the enclosing sized symbol of
/// furthest extent whose range still contains `pc` (e.g. an outer function that brackets an inner
/// label), rather than giving up — nested same-image symbols therefore resolve to a containing
/// function. `reach` is the [`ReachIndex`] for `symbols`, which makes that fallback constant-time.
pub(crate) fn resolve(symbols: &[Symbol], reach: &ReachIndex, pc: u64) -> String {
    let idx = symbols.partition_point(|s| s.addr <= pc);
    if idx == 0 {
        return format!("0x{pc:016x}");
    }
    // The nearest preceding symbol owns `pc` if `pc` lies within its extent (which may reach to the
    // end of its section for a zero-size label).
    let nearest = &symbols[idx - 1];
    if pc < resolve_end(nearest) {
        return nearest.name.clone();
    }
    // The nearest symbol ended before `pc`. Attribute `pc` to the sized symbol of furthest extent
    // at or before `idx` when that extent still passes `pc`: every such symbol starts at or below
    // `pc` (the table is address-sorted), so one reaching past `pc` encloses it. Looked up in O(1)
    // from the precomputed reach index rather than by scanning backward for it.
    let owner = reach.arg[idx - 1];
    if owner != usize::MAX && pc < reach.reach[idx - 1] {
        return symbols[owner].name.clone();
    }
    format!("0x{pc:016x}")
}

/// Loads executable-code symbols from one ELF image loaded at guest runtime address `load_base`.
///
/// Only symbols located in executable sections are kept, so absolute constants and non-code data
/// labels never masquerade as functions. The full symbol table (`.symtab`) is preferred; the
/// dynamic table (`.dynsym`) is used only as a fallback for stripped binaries.
///
/// `load_base` is the *absolute* runtime address the image's first loadable segment maps to, as
/// documented for the `@0x<base>` CLI suffix. Symbols are relocated by a computed slide
/// `bias = load_base - min(PT_LOAD.p_vaddr)`, so both a PIE (whose lowest `PT_LOAD` vaddr is 0, giving
/// `bias == load_base`) and a relocated `ET_EXEC` (linked at, e.g., `0x400000`) resolve to the right
/// runtime addresses — an `ET_EXEC` moved to `0x600000` maps its `0x400000` symbols to `0x600000`,
/// not `0xa00000`.
pub(crate) fn load_symbols(source: &SymbolSource, image: u32) -> Result<Vec<Symbol>> {
    let bytes = fs::read(&source.path)?;
    let elf = Elf::parse(&bytes)?;

    let is_pie = elf.header.e_type == goblin::elf::header::ET_DYN;
    if source.load_base.is_none() && is_pie {
        warn!(
            "{}: position-independent image without a load base (append `@0x<base>` to \
             the symbol path); sampled addresses may not resolve",
            source.path.display()
        );
    }
    // Slide from the image's linked low address to its actual runtime base, so `load_base` means
    // the absolute address the image is loaded at (matching the CLI documentation) rather than an
    // additive offset applied on top of already-absolute symbol values.
    let bias = relocation_bias(source.load_base, min_load_vaddr(&elf));

    let mut out = collect_exec_syms(&elf, &elf.syms, &elf.strtab, bias, image);
    if out.is_empty() {
        out = collect_exec_syms(&elf, &elf.dynsyms, &elf.dynstrtab, bias, image);
    }
    Ok(out)
}

/// The relocation slide applied to an image's symbol addresses.
///
/// `load_base` is the *absolute* guest runtime load address of the image; `link_base` is the lowest
/// `PT_LOAD` vaddr it was linked at. The slide is `load_base - link_base`, so a PIE (linked at 0)
/// slides by exactly `load_base`, and a relocated `ET_EXEC` (linked at a nonzero address) slides so
/// its already-absolute symbols land at their runtime addresses rather than being double-offset.
/// With no `load_base` the image is used at its link addresses (slide 0).
fn relocation_bias(load_base: Option<u64>, link_base: u64) -> u64 {
    match load_base {
        Some(runtime_base) => runtime_base.wrapping_sub(link_base),
        None => 0,
    }
}

/// The lowest `p_vaddr` among the image's loadable (`PT_LOAD`) segments, i.e. its linked base.
///
/// Used to convert an absolute runtime load base into a relocation slide. Returns 0 when the image
/// has no `PT_LOAD` segments (so the slide equals the given runtime base — the natural PIE case).
fn min_load_vaddr(elf: &Elf) -> u64 {
    elf.program_headers
        .iter()
        .filter(|ph| ph.p_type == goblin::elf::program_header::PT_LOAD)
        .map(|ph| ph.p_vaddr)
        .min()
        .unwrap_or(0)
}

/// Collects `STT_FUNC`/executable-section symbols from one symbol table, relocated by `bias`.
fn collect_exec_syms(
    elf: &Elf,
    syms: &goblin::elf::Symtab,
    strtab: &goblin::strtab::Strtab,
    bias: u64,
    image: u32,
) -> Vec<Symbol> {
    let mut out: Vec<Symbol> = Vec::new();
    for sym in syms {
        if sym.st_value == 0 {
            continue;
        }
        let typ = sym.st_type();
        if typ != goblin::elf::sym::STT_FUNC && typ != goblin::elf::sym::STT_NOTYPE {
            continue;
        }
        let Some(sh) = exec_section(elf, sym.st_shndx) else {
            continue;
        };
        // The section end bounds resolution so an address past the section is never attributed to
        // a symbol in it (e.g. a zero-size symbol at the tail of `.text`).
        let sec_end = bias.wrapping_add(sh.sh_addr.wrapping_add(sh.sh_size));
        if let Some(name) = strtab.get_at(sym.st_name) {
            if name.is_empty() {
                continue;
            }
            out.push(Symbol {
                addr: bias.wrapping_add(sym.st_value),
                size: sym.st_size,
                sec_end,
                name: name.to_string(),
                is_func: typ == goblin::elf::sym::STT_FUNC,
                image,
            });
        }
    }
    out
}

/// Returns the executable (`SHF_EXECINSTR`) section header for index `shndx`, if any.
///
/// Rejects the undefined section (`SHN_UNDEF`) and the reserved indices (`SHN_ABS`, `SHN_COMMON`,
/// …) so absolute constants and common symbols are never treated as code.
fn exec_section<'e>(
    elf: &'e Elf,
    shndx: usize,
) -> Option<&'e goblin::elf::section_header::SectionHeader> {
    const SHN_LORESERVE: usize = 0xff00;
    if shndx == 0 || shndx >= SHN_LORESERVE {
        return None;
    }
    let sh = elf.section_headers.get(shndx)?;
    if sh.sh_flags & u64::from(goblin::elf::section_header::SHF_EXECINSTR) != 0 {
        Some(sh)
    } else {
        None
    }
}

/// Orders symbols for address resolution.
///
/// Sorted by ascending address; at equal addresses `STT_FUNC` is preferred over `STT_NOTYPE`
/// (so a real function is not shadowed by a zero-size label such as `_stext`), then the widest
/// extent, then name for stability.
pub(crate) fn sort_symbols(symbols: &mut [Symbol]) {
    symbols.sort_by(|a, b| {
        a.addr
            .cmp(&b.addr)
            .then(b.is_func.cmp(&a.is_func))
            .then(b.size.cmp(&a.size))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Collapses same-address aliases to the best (first) entry and gives zero-size symbols an
/// implicit, section-bounded extent.
///
/// `symbols` must be sorted by [`sort_symbols`]. Run [`check_image_overlap`] *before* this: dedup
/// merges symbols that share an address, so a cross-image collision must be rejected beforehand.
pub(crate) fn dedup_and_bound_symbols(symbols: &mut Vec<Symbol>) {
    // Keep the first (best) entry of each equal-address run.
    symbols.dedup_by(|a, b| a.addr == b.addr);
    fill_implicit_sizes(symbols);
}

/// Gives zero-size symbols an implicit extent and clamps every extent to its executable section.
///
/// `symbols` must be sorted by ascending address. A zero-size symbol reaches to the next distinct
/// address, and no symbol is allowed to extend past the end of its containing executable section,
/// so [`GuestProfiler::resolve`] never attributes an address far past a symbol (or past `.text`)
/// to that symbol.
fn fill_implicit_sizes(symbols: &mut [Symbol]) {
    let len = symbols.len();
    for i in 0..len {
        let sec_room = symbols[i].sec_end.saturating_sub(symbols[i].addr);
        if symbols[i].size != 0 {
            // Clamp an over-large declared size to the section end.
            if sec_room != 0 && symbols[i].size > sec_room {
                symbols[i].size = sec_room;
            }
            continue;
        }
        let mut j = i + 1;
        while j < len && symbols[j].addr == symbols[i].addr {
            j += 1;
        }
        let next_gap = if j < len {
            symbols[j].addr.saturating_sub(symbols[i].addr)
        } else {
            u64::MAX
        };
        let sec_bound = if sec_room == 0 { u64::MAX } else { sec_room };
        let bound = next_gap.min(sec_bound);
        if bound != u64::MAX {
            symbols[i].size = bound;
        }
    }
}

/// Exclusive end of the guest range a symbol can be attributed to.
///
/// Mirrors [`GuestProfiler::resolve`]: a sized symbol owns `[addr, addr + size)` (clamped to its
/// executable section), while a zero-size symbol may claim anything up to the end of its section,
/// so it owns `[addr, sec_end)`. Used for overlap detection *before* [`fill_implicit_sizes`] runs.
fn symbol_extent_end(s: &Symbol) -> u64 {
    if s.size != 0 {
        let end = s.addr.saturating_add(s.size);
        if s.sec_end != 0 {
            end.min(s.sec_end)
        } else {
            end
        }
    } else if s.sec_end > s.addr {
        s.sec_end
    } else {
        s.addr.saturating_add(1)
    }
}

/// Fails if two different images can be attributed the same guest address.
///
/// Each symbol owns the half-open range `[addr, `[`symbol_extent_end`]`)`. This sweeps those
/// per-image ranges and rejects any point covered by more than one image. A pairwise `windows(2)`
/// scan is insufficient: a wide symbol from one image can be separated in address order from an
/// overlapping symbol of another image by nested same-image labels (e.g. image A `[0x1000,0x2000)`
/// with an inner label `[0x1100,0x1110)` that sorts between A's outer symbol and image B at
/// `0x1800`), and different-address zero-size symbols in overlapping sections evade an
/// adjacent-only range test.
///
/// Overlap makes address-only resolution ambiguous and cannot be disambiguated without
/// per-address-space information, so it is rejected rather than silently producing misattributed
/// frames; the fix is to supply per-image load bases (`path@0x<base>`).
pub(crate) fn check_image_overlap(symbols: &[Symbol]) -> Result<()> {
    /// One boundary of a symbol's guest range in the address-space sweep.
    struct Edge {
        pos: u64,
        // Ends sort before starts at the same position so ranges that merely touch
        // (`[a, p)` then `[p, b)`) are not treated as overlapping.
        is_start: bool,
        image: u32,
        sym: usize,
    }

    let Some(max_image) = symbols.iter().map(|s| s.image).max() else {
        return Ok(());
    };
    let image_count = max_image as usize + 1;

    let mut edges: Vec<Edge> = Vec::with_capacity(symbols.len() * 2);
    for (sym, s) in symbols.iter().enumerate() {
        let end = symbol_extent_end(s);
        if end <= s.addr {
            continue;
        }
        edges.push(Edge {
            pos: s.addr,
            is_start: true,
            image: s.image,
            sym,
        });
        edges.push(Edge {
            pos: end,
            is_start: false,
            image: s.image,
            sym,
        });
    }
    // Ascending by position; at equal positions ends (`false`) precede starts (`true`).
    edges.sort_by(|a, b| a.pos.cmp(&b.pos).then(a.is_start.cmp(&b.is_start)));

    let mut open = vec![0u32; image_count]; // count of currently-open ranges per image
    let mut rep: Vec<Option<usize>> = vec![None; image_count]; // a representative open symbol
    let mut distinct = 0usize; // number of images with at least one open range

    for e in &edges {
        let img = e.image as usize;
        if e.is_start {
            if open[img] == 0 {
                distinct += 1;
            }
            open[img] += 1;
            rep[img] = Some(e.sym);
            if distinct >= 2 {
                // Some other image also has an open range here; name it for the diagnostic.
                let other = rep
                    .iter()
                    .enumerate()
                    .find_map(|(i, r)| if i != img { *r } else { None })
                    .expect("a second image must be open when distinct >= 2");
                let a = &symbols[e.sym];
                let b = &symbols[other];
                ::anyhow::bail!(
                    "symbol images overlap in the guest address space (`{}` @0x{:x}..0x{:x} \
                     [image {}] overlaps `{}` @0x{:x}..0x{:x} [image {}]); address-only resolution \
                     would be ambiguous — supply per-image load bases with `path@0x<base>`",
                    a.name,
                    a.addr,
                    symbol_extent_end(a),
                    a.image,
                    b.name,
                    b.addr,
                    symbol_extent_end(b),
                    b.image,
                );
            }
        } else {
            open[img] -= 1;
            if open[img] == 0 {
                distinct -= 1;
                rep[img] = None;
            }
        }
    }
    Ok(())
}

/// Builds a fully-specified [`Symbol`] for cross-module tests (fields are module-private).
#[cfg(test)]
pub(crate) fn make_symbol(addr: u64, size: u64, name: &str) -> Symbol {
    Symbol {
        addr,
        size,
        sec_end: u64::MAX,
        name: name.to_string(),
        is_func: true,
        image: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_previous_symbol_range() {
        let symbols = vec![
            Symbol {
                addr: 0x1000,
                size: 0x20,
                sec_end: 0x10000,
                name: "a".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x1100,
                size: 0,
                sec_end: 0x10000,
                name: "b".to_string(),
                is_func: true,
                image: 0,
            },
        ];
        let reach = ReachIndex::build(&symbols);
        assert_eq!(resolve(&symbols, &reach, 0x1000), "a");
        assert_eq!(resolve(&symbols, &reach, 0x101f), "a");
        assert_eq!(resolve(&symbols, &reach, 0x1020), "0x0000000000001020");
        assert_eq!(resolve(&symbols, &reach, 0x1101), "b");
    }

    #[test]
    fn resolve_stops_at_section_end() {
        let symbols = vec![Symbol {
            addr: 0x1000,
            size: 0,
            sec_end: 0x1100,
            name: "tail".to_string(),
            is_func: true,
            image: 0,
        }];
        // Within the section the trailing symbol resolves; past the section end it does not.
        let reach = ReachIndex::build(&symbols);
        assert_eq!(resolve(&symbols, &reach, 0x10ff), "tail");
        assert_eq!(resolve(&symbols, &reach, 0x1100), "0x0000000000001100");
    }

    #[test]
    fn resolve_falls_back_to_enclosing_outer_symbol() {
        // An outer function brackets an inner label. An address after the inner label but still
        // inside the outer function must resolve to the outer function, not to a raw address.
        let symbols = vec![
            Symbol {
                addr: 0x1000,
                size: 0x1000,
                sec_end: 0x10000,
                name: "outer".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x1100,
                size: 0x10,
                sec_end: 0x10000,
                name: "inner".to_string(),
                is_func: true,
                image: 0,
            },
        ];
        // Inside the inner label -> inner.
        let reach = ReachIndex::build(&symbols);
        assert_eq!(resolve(&symbols, &reach, 0x1108), "inner");
        // Past the inner label but still within the outer function -> falls back to outer.
        assert_eq!(resolve(&symbols, &reach, 0x1200), "outer");
        // Before the inner label but within the outer function -> outer.
        assert_eq!(resolve(&symbols, &reach, 0x1050), "outer");
        // Past the outer function entirely -> unresolved.
        assert_eq!(resolve(&symbols, &reach, 0x2000), "0x0000000000002000");
    }

    #[test]
    fn resolve_gap_falls_back_to_furthest_enclosing_symbol() {
        // A wide outer function brackets two small inner functions. An address in the gap after an
        // inner function but still inside the outer must resolve to the enclosing outer, looked up
        // in constant time from the reach index regardless of how many inner symbols precede it.
        let symbols = vec![
            Symbol {
                addr: 0x1000,
                size: 0x900,
                sec_end: 0x10000,
                name: "outer".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x1100,
                size: 0x10,
                sec_end: 0x10000,
                name: "inner_a".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x1200,
                size: 0x10,
                sec_end: 0x10000,
                name: "inner_b".to_string(),
                is_func: true,
                image: 0,
            },
        ];
        let reach = ReachIndex::build(&symbols);
        // Inside the last inner function.
        assert_eq!(resolve(&symbols, &reach, 0x1208), "inner_b");
        // In the gap after the inner functions but within the outer (0x1000..0x1900) -> outer.
        assert_eq!(resolve(&symbols, &reach, 0x1500), "outer");
        // Past the outer function entirely -> unresolved.
        assert_eq!(resolve(&symbols, &reach, 0x1900), "0x0000000000001900");
    }

    #[test]
    fn resolve_past_every_symbol_is_raw() {
        // A PC beyond the furthest symbol extent (e.g. a mis-based PIE image whose low symbols
        // never reach the high sampled address) resolves to a raw address in O(1), without a
        // backward scan for an enclosing symbol that cannot exist.
        let mut symbols = vec![make_symbol(0x1000, 0x40, "a"), make_symbol(0x1100, 0x40, "b")];
        sort_symbols(&mut symbols);
        dedup_and_bound_symbols(&mut symbols);
        let reach = ReachIndex::build(&symbols);
        assert_eq!(resolve(&symbols, &reach, 0x9000_0000), "0x0000000090000000");
        // A resolvable address still resolves.
        assert_eq!(resolve(&symbols, &reach, 0x1110), "b");
    }

    #[test]
    fn fill_implicit_sizes_bounds_zero_size_symbols() {
        let mut symbols = vec![
            Symbol {
                addr: 0x1000,
                size: 0,
                sec_end: 0x2000,
                name: "a".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x1040,
                size: 0,
                sec_end: 0x2000,
                name: "b".to_string(),
                is_func: true,
                image: 0,
            },
        ];
        fill_implicit_sizes(&mut symbols);
        // Bounded by the next distinct address.
        assert_eq!(symbols[0].size, 0x40);
        // The final symbol has no successor, so it is bounded by its executable-section end.
        assert_eq!(symbols[1].size, 0x2000 - 0x1040);
    }

    #[test]
    fn dedup_prefers_stt_func_over_notype_alias() {
        // A zero-size NOTYPE label (e.g. `_stext`) aliased with a real function at the same address
        // must resolve to the function, not the label.
        let mut symbols = vec![
            Symbol {
                addr: 0xffff_ffff_8100_0000,
                size: 0,
                sec_end: 0xffff_ffff_8200_0000,
                name: "_stext".to_string(),
                is_func: false,
                image: 0,
            },
            Symbol {
                addr: 0xffff_ffff_8100_0000,
                size: 0x40,
                sec_end: 0xffff_ffff_8200_0000,
                name: "startup_64".to_string(),
                is_func: true,
                image: 0,
            },
        ];
        sort_symbols(&mut symbols);
        dedup_and_bound_symbols(&mut symbols);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "startup_64");
        assert_eq!(symbols[0].size, 0x40);
    }

    #[test]
    fn check_image_overlap_rejects_overlapping_images() {
        let symbols = vec![
            Symbol {
                addr: 0x1000,
                size: 0x100,
                sec_end: 0x2000,
                name: "a".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x1080,
                size: 0x10,
                sec_end: 0x2000,
                name: "b".to_string(),
                is_func: true,
                image: 1,
            },
        ];
        assert!(check_image_overlap(&symbols).is_err());
    }

    #[test]
    fn check_image_overlap_allows_disjoint_images() {
        let symbols = vec![
            Symbol {
                addr: 0x1000,
                size: 0x80,
                sec_end: 0x2000,
                name: "a".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x1080,
                size: 0x10,
                sec_end: 0x2000,
                name: "b".to_string(),
                is_func: true,
                image: 1,
            },
        ];
        assert!(check_image_overlap(&symbols).is_ok());
    }

    #[test]
    fn check_image_overlap_rejects_same_address_across_images() {
        // Two images each with only a zero-size symbol at the same address: dedup would merge them
        // into one entry, so the collision must be detected *before* deduplication.
        let mut symbols = vec![
            Symbol {
                addr: 0x1000,
                size: 0,
                sec_end: 0x2000,
                name: "a".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x1000,
                size: 0,
                sec_end: 0x2000,
                name: "b".to_string(),
                is_func: true,
                image: 1,
            },
        ];
        sort_symbols(&mut symbols);
        assert!(check_image_overlap(&symbols).is_err());
    }

    #[test]
    fn check_image_overlap_rejects_nested_symbol_hidden_overlap() {
        // Image 0 spans [0x1000,0x2000) with a small inner label at 0x1100 that sorts between the
        // outer symbol and image 1 at 0x1800. An adjacent-pair scan compares only the inner label
        // with image 1 and misses the real overlap; the sweep must still reject it.
        let mut symbols = vec![
            Symbol {
                addr: 0x1000,
                size: 0x1000,
                sec_end: 0x2000,
                name: "a_outer".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x1100,
                size: 0x10,
                sec_end: 0x2000,
                name: "a_inner".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x1800,
                size: 0x10,
                sec_end: 0x2000,
                name: "b".to_string(),
                is_func: true,
                image: 1,
            },
        ];
        sort_symbols(&mut symbols);
        assert!(check_image_overlap(&symbols).is_err());
    }

    #[test]
    fn check_image_overlap_rejects_overlapping_sections_zero_size() {
        // Zero-size symbols at *different* addresses whose executable sections overlap
        // ([0x1000,0x2000) vs [0x1500,0x2500)). No adjacent pair has a nonzero-size range spanning
        // the gap, so only a section-aware sweep rejects them.
        let mut symbols = vec![
            Symbol {
                addr: 0x1000,
                size: 0,
                sec_end: 0x2000,
                name: "a".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x1500,
                size: 0,
                sec_end: 0x2500,
                name: "b".to_string(),
                is_func: true,
                image: 1,
            },
        ];
        sort_symbols(&mut symbols);
        assert!(check_image_overlap(&symbols).is_err());
    }

    #[test]
    fn check_image_overlap_allows_other_image_in_single_image_gap() {
        // Image 0 has two disjoint sections with a wide gap; image 1 sits entirely in that gap.
        // A per-image bounding box would falsely reject this, but the per-range sweep must allow
        // it — the images never actually cover the same address.
        let mut symbols = vec![
            Symbol {
                addr: 0x1000,
                size: 0x80,
                sec_end: 0x1100,
                name: "a0".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x9000,
                size: 0x80,
                sec_end: 0x9100,
                name: "a1".to_string(),
                is_func: true,
                image: 0,
            },
            Symbol {
                addr: 0x5000,
                size: 0x80,
                sec_end: 0x5100,
                name: "b".to_string(),
                is_func: true,
                image: 1,
            },
        ];
        sort_symbols(&mut symbols);
        assert!(check_image_overlap(&symbols).is_ok());
    }

    #[test]
    fn relocation_bias_treats_load_base_as_absolute() {
        // An ET_EXEC linked at 0x400000 and relocated to 0x600000 must slide by 0x200000, so a
        // symbol at 0x401000 maps to 0x601000 — not 0xa00000 (which an additive base would give).
        assert_eq!(relocation_bias(Some(0x600000), 0x400000), 0x200000);
        // A PIE is linked at 0, so the slide equals the load base.
        assert_eq!(relocation_bias(Some(0x5555_5555_4000), 0), 0x5555_5555_4000);
        // No load base means the image is used at its link addresses (no slide).
        assert_eq!(relocation_bias(None, 0x400000), 0);
    }
}

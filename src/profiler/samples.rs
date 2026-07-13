// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Guest sample collection, frame-pointer stack walking, and folded-stack output.

use ::std::collections::HashMap;
use ::std::collections::hash_map::Entry;
use ::std::fs;
use ::std::io::{BufWriter, Write};
use ::std::path::{Path, PathBuf};

use ::anyhow::Result;
use ::log::warn;

use super::ProfilingConfig;
use super::ensure_parent_dir;
use super::paging::{PageWalk, address_space_of, is_canonical};
use super::symbols::{
    ReachIndex, Symbol, check_image_overlap, dedup_and_bound_symbols, load_symbols, resolve,
    sort_symbols,
};

/// Guest sample collector + symbol resolver.
pub struct GuestProfiler {
    cfg: ProfilingConfig,
    /// Kernel symbols (upper-half addresses). The kernel is a single shared image, so kernel
    /// frames from every guest process resolve against this one table.
    kernel_syms: Vec<Symbol>,
    /// User symbols (lower-half addresses). Only used to resolve user frames when a single user
    /// address space is observed; with multiple processes the provided ELF(s) cannot be attributed
    /// to a specific address space, so user frames are left raw (see [`GuestProfiler::write_folded`]).
    user_syms: Vec<Symbol>,
    /// Distinct `(address-space, raw-address-stack)` keys folded to their sample counts.
    /// Aggregating incrementally keeps memory bounded by the number of *distinct* stacks rather
    /// than the total sample count. The address-space id (a normalized `CR3`) disambiguates
    /// user-space samples from different processes; see [`GuestProfiler::record_sample`].
    stacks: HashMap<(u64, Vec<u64>), u64>,
    /// Samples dropped once [`MAX_RETAINED_STACK_BYTES`] of distinct stacks were retained. Reported
    /// as a synthetic `[overflow]` frame so a truncated profile is never silently under-counted.
    overflow: u64,
    /// Estimated live bytes held by the retained distinct stacks (see [`stack_retention_bytes`]),
    /// tracked incrementally so ingestion can bound memory by bytes rather than a raw stack count.
    retained_bytes: usize,
}

/// Upper bound on the estimated live memory (bytes) retained for distinct stacks. A byte budget
/// bounds guest-profiler memory to a fixed size regardless of individual stack depth: a maximum-
/// depth stack costs far more per entry than a shallow one, so a raw stack *count* cap could still
/// grow memory several-fold. The folded string output materialized at finalization scales with the
/// same byte volume, so bounding retained bytes bounds finalization memory too.
const MAX_RETAINED_STACK_BYTES: usize = 64 << 20;

/// Maximum frame-pointer chain depth walked per sample.
const MAX_STACK_DEPTH: usize = 64;

/// Approximate live-memory cost of retaining one distinct stack: the `HashMap` entry (address-space
/// id, sample count, and bucket/control slack) plus the heap `Vec<u64>` holding its frame
/// addresses. Used to bound retained memory by bytes rather than by a raw stack count.
fn stack_retention_bytes(depth: usize) -> usize {
    const ENTRY_OVERHEAD: usize = 48;
    ENTRY_OVERHEAD + depth * ::std::mem::size_of::<u64>()
}

impl GuestProfiler {
    /// Creates a guest profiler from runtime configuration.
    pub fn new(cfg: ProfilingConfig) -> Result<Self> {
        // Kernel and user symbols are kept in *separate* tables. The kernel occupies the upper
        // canonical half and is one shared image, so it never collides with user images and needs
        // no overlap check. User images live in the lower half; only they are checked for mutual
        // overlap. Keeping the tables apart is what lets a run supply both `vmlinux` and a user ELF
        // without the two being falsely flagged as overlapping, and ensures an upper-half PC is
        // only ever resolved against kernel symbols (and vice versa).
        let mut kernel_syms: Vec<Symbol> = Vec::new();
        if let Some(source) = &cfg.kernel_symbols {
            kernel_syms.extend(load_symbols(source, 0)?);
        }
        sort_symbols(&mut kernel_syms);
        dedup_and_bound_symbols(&mut kernel_syms);

        let mut user_syms: Vec<Symbol> = Vec::new();
        for (image, source) in cfg.user_symbols.iter().enumerate() {
            user_syms.extend(load_symbols(source, image as u32)?);
        }
        sort_symbols(&mut user_syms);
        // Reject ambiguous cross-image overlaps *before* collapsing same-address aliases: dedup
        // merges symbols that share an address, which would hide a cross-image collision (two user
        // images each with a symbol at the same address) from the overlap check.
        check_image_overlap(&user_syms)?;
        dedup_and_bound_symbols(&mut user_syms);

        Ok(Self {
            cfg,
            kernel_syms,
            user_syms,
            stacks: HashMap::new(),
            overflow: 0,
            retained_bytes: 0,
        })
    }

    /// Resolves a single program counter to a symbol name.
    ///
    /// Upper-half (kernel) PCs resolve against the kernel table. Lower-half (user) PCs resolve
    /// against the user table only when `resolve_user` is set, i.e. exactly one user address space
    /// was sampled and the provided user ELF(s) can be unambiguously attributed to it. With several
    /// user processes the mapping from ELF to address space is unknown, so user frames are left as
    /// raw addresses rather than mislabeled with another process's symbols.
    fn resolve_frame(
        &self,
        pc: u64,
        resolve_user: bool,
        kernel_reach: &ReachIndex,
        user_reach: &ReachIndex,
    ) -> String {
        if pc >> 63 == 1 {
            resolve(&self.kernel_syms, kernel_reach, pc)
        } else if resolve_user {
            resolve(&self.user_syms, user_reach, pc)
        } else {
            format!("0x{pc:016x}")
        }
    }

    /// Records one stack sample from interrupted guest register state.
    ///
    /// `read_phys` must read `buf.len()` bytes from guest-physical address `gpa` and return
    /// whether the read succeeded. `la57` is the guest's paging mode at sample time (`CR4.LA57`):
    /// it selects a 5-level vs 4-level page walk and the 57- vs 48-bit canonical-address check, so
    /// stacks resolve correctly on hosts where the guest enabled 5-level paging.
    ///
    /// A user-space sample (leaf `RIP` in the lower canonical half) is keyed by its normalized
    /// `CR3` so samples from *different* processes — which legitimately reuse the same virtual
    /// addresses — are never merged. Kernel samples (upper half) share address space `0`, so
    /// kernel stacks aggregate across all processes as expected. `write_folded` then tags user
    /// frames per address space only when more than one is observed.
    pub fn record_sample<F>(&mut self, rip: u64, rbp: u64, cr3: u64, la57: bool, mut read_phys: F)
    where
        F: FnMut(u64, &mut [u8]) -> bool,
    {
        let mut frames: Vec<u64> = vec![rip];
        let mut fp: u64 = rbp;
        // Cache the last frame-pointer page translation: successive frames (and the two reads per
        // frame) usually land on the same guest stack page, so this collapses a per-read 4-level
        // page walk into a single translation for the whole page — cutting a maximum-depth sample
        // from ~960 physical reads to a small constant multiple of the depth. `la57` selects the
        // guest's paging mode (5-level `CR4.LA57` vs 4-level) for both the walk and canonicality.
        let mut walk = PageWalk::new(cr3, la57);

        for _ in 0..MAX_STACK_DEPTH {
            // A frame pointer must be canonical and 8-byte aligned to address a real stack frame.
            // Rejecting a misaligned `fp` also guarantees neither 8-byte read below straddles a page
            // boundary (4096 is a multiple of 8), so one page translation covers the whole read.
            if !is_canonical(fp, la57) || fp & 0x7 != 0 {
                break;
            }
            // The return address sits one word above the saved frame pointer. Near the top of the
            // canonical lower half `fp + 8` can become non-canonical or wrap; reject such a slot
            // rather than translate a bogus address.
            let Some(ret_slot) = fp.checked_add(8).filter(|slot| is_canonical(*slot, la57)) else {
                break;
            };
            let Some(next_fp) = walk.read_u64(fp, &mut read_phys) else {
                break;
            };
            let Some(ret) = walk.read_u64(ret_slot, &mut read_phys) else {
                break;
            };
            // Only accept plausible return addresses. A frame-pointer walk over a corrupt or
            // not-yet-established chain can read arbitrary stack data as a "return address"; a
            // non-canonical or null value is never executable code, so dropping it avoids
            // fabricating frames.
            if is_canonical(ret, la57) && ret != 0 {
                frames.push(ret);
            }
            if next_fp <= fp || next_fp.wrapping_sub(fp) > (1 << 20) {
                break;
            }
            fp = next_fp;
        }

        let key = (address_space_of(rip, cr3), frames);
        let cost = stack_retention_bytes(key.1.len());
        // Bound memory by retained bytes: keep counting stacks already seen, but once retaining a
        // genuinely new stack would exceed the byte budget, divert it into the overflow tally
        // instead of growing without limit.
        match self.stacks.entry(key) {
            Entry::Occupied(mut e) => {
                *e.get_mut() += 1;
            }
            Entry::Vacant(e) => {
                if self.retained_bytes + cost > MAX_RETAINED_STACK_BYTES {
                    self.overflow += 1;
                    return;
                }
                self.retained_bytes += cost;
                e.insert(1);
            }
        }
    }

    /// Writes folded stacks to `guest_profile_path`.
    ///
    /// Emits *raw* guest stacks (leaf-last, root-first). The `[GUEST]` root frame is applied by the
    /// post-processing merge step so that guest and host stacks are prefixed uniformly in one place.
    pub fn write_folded(&self) -> Result<()> {
        ensure_parent_dir(&self.cfg.guest_profile_path)?;

        // Tag user frames with their address space only when several are present; a single
        // user process (the common case) needs no disambiguation and stays uncluttered.
        let mut user_asids: Vec<u64> = self
            .stacks
            .keys()
            .map(|(asid, _)| *asid)
            .filter(|asid| *asid != 0)
            .collect();
        user_asids.sort_unstable();
        user_asids.dedup();
        let tag_user = user_asids.len() > 1;
        // Resolve user frames only when there is at most one user address space: the provided user
        // ELF(s) cannot be attributed to a specific process, so with several address spaces every
        // user frame is left raw rather than mislabeled with another process's symbols. Kernel
        // frames (upper half) always resolve because the kernel image is shared across processes.
        let resolve_user = user_asids.len() <= 1;
        // Build each table's interval-ownership index once so per-frame resolution attributes an
        // address past its nearest symbol to an enclosing function in O(1) instead of scanning the
        // whole table for each such frame.
        let kernel_reach = ReachIndex::build(&self.kernel_syms);
        let user_reach = ReachIndex::build(&self.user_syms);

        let mut counts: HashMap<String, u64> = HashMap::new();
        for ((asid, frames), &count) in &self.stacks {
            let mut names: Vec<String> = Vec::with_capacity(frames.len() + 1);
            // Root a multi-process user stack under its address space so distinct processes never
            // conflate even when they resolve to the same virtual-address symbol names.
            if tag_user && *asid != 0 {
                names.push(format!("[as 0x{asid:012x}]"));
            }
            for &pc in frames.iter().rev() {
                names.push(self.resolve_frame(pc, resolve_user, &kernel_reach, &user_reach));
            }
            let folded = names.join(";");
            *counts.entry(folded).or_insert(0) += count;
        }
        if self.overflow != 0 {
            *counts.entry("[overflow]".to_string()).or_insert(0) += self.overflow;
        }

        let mut lines: Vec<(String, u64)> = counts.into_iter().collect();
        lines.sort_by(|a, b| a.0.cmp(&b.0));

        // Invalidate any run-id sidecar from a previous run *before* republishing the profile: if
        // writing the new sidecar then fails (or the process dies mid-write), no stale sidecar is
        // left behind. Provenance therefore fails safe — a missing guest run id refuses the merge —
        // rather than a stale id matching an old host trace left at the same path.
        let run_path = run_id_sidecar_path(&self.cfg.guest_profile_path);
        let _ = fs::remove_file(&run_path);

        // Stream the folded lines straight to the temp file through a BufWriter rather than
        // concatenating them into one large String first: at the retention cap the combined output
        // is hundreds of MiB, and a separate in-memory output buffer would double that peak. The
        // temp is created exclusively under a randomized name so an elevated profiler writing into
        // a world-writable directory can never be redirected through a pre-planted symlink.
        let (tmp_path, file) = create_exclusive_temp(&self.cfg.guest_profile_path)?;
        {
            let mut w = BufWriter::new(file);
            for (stack, count) in &lines {
                writeln!(w, "{stack} {count}")?;
            }
            w.flush()?;
        }
        if let Err(e) = fs::rename(&tmp_path, &self.cfg.guest_profile_path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e.into());
        }

        // Publish the provenance sidecar atomically (write a temp, then rename) so a partial write
        // can never leave a truncated or mismatched run id next to the profile. Best-effort: a
        // guest-only render still succeeds even if the sidecar cannot be written — provenance then
        // simply refuses to merge a host trace it cannot verify.
        let sidecar_res = create_exclusive_temp(&run_path).and_then(|(run_tmp, mut f)| {
            f.write_all(format!("run={}\n", self.cfg.run_id).as_bytes())?;
            f.flush()?;
            drop(f);
            fs::rename(&run_tmp, &run_path).inspect_err(|_| {
                let _ = fs::remove_file(&run_tmp);
            })
        });
        if let Err(e) = sidecar_res {
            warn!(
                "failed to write profile run-id sidecar {}: {e}",
                run_path.display()
            );
        }
        Ok(())
    }
}

/// Creates a fresh, exclusively-opened temporary file in `final_path`'s directory.
///
/// Uses a randomized name plus `create_new` (`O_CREAT | O_EXCL`), so a pre-existing file or symlink
/// at the candidate path is never opened, truncated, or followed: an elevated profiler writing into
/// a world-writable output directory cannot be redirected through a planted symlink to clobber an
/// unrelated host file. Returns the temp path and its open handle; the caller renames it onto the
/// final path once written.
fn create_exclusive_temp(final_path: &Path) -> ::std::io::Result<(PathBuf, fs::File)> {
    let dir = final_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = final_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("profile");
    for _ in 0..16 {
        let candidate = dir.join(format!(".{stem}.{}.tmp", unique_nonce()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => return Ok((candidate, f)),
            Err(e) if e.kind() == ::std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(::std::io::Error::new(
        ::std::io::ErrorKind::AlreadyExists,
        "could not create a unique temporary file after several attempts",
    ))
}

/// A per-process-unique nonce for temporary file names: process id, a monotonic wall-clock reading,
/// and an in-process counter, so concurrent writers within one process never collide either.
fn unique_nonce() -> String {
    use ::std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = ::std::process::id();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = ::std::time::SystemTime::now()
        .duration_since(::std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{pid:x}-{nanos:x}-{seq:x}")
}

/// Provenance sidecar path for a folded profile: the folded filename with `.run` appended
/// (e.g. `profile.folded` -> `profile.folded.run`).
fn run_id_sidecar_path(folded: &Path) -> PathBuf {
    let mut name = folded
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".run");
    folded.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::paging::CR3_ADDR_MASK;
    use super::super::symbols::make_symbol;

    #[test]
    fn record_sample_aggregates_and_drops_bogus_returns() {
        // Identity-mapped page tables so guest-virtual == guest-physical for the walk.
        let pml4: u64 = 0x1000;
        let pdpt: u64 = 0x2000;
        let pd: u64 = 0x3000;
        let pt: u64 = 0x4000;
        // Two stack frames at 0x5000 (inner) and 0x5010 (outer) plus a bogus non-canonical return.
        let mut mem = vec![0u8; 0x8000];
        fn wr(mem: &mut [u8], gpa: u64, val: u64) {
            let off = gpa as usize;
            mem[off..off + 8].copy_from_slice(&val.to_le_bytes());
        }
        // Identity map the low 2 MiB via a single 4 KiB-granular table chain covering these pages.
        wr(&mut mem, pml4, pdpt | 1);
        wr(&mut mem, pdpt, pd | 1);
        wr(&mut mem, pd, pt | 1);
        for page in 0..8u64 {
            wr(&mut mem, pt + page * 8, (page * 0x1000) | 1);
        }
        // Frame at 0x5000: saved-rbp -> 0x5010, return -> 0x401000 (canonical).
        wr(&mut mem, 0x5000, 0x5010);
        wr(&mut mem, 0x5008, 0x0040_1000);
        // Frame at 0x5010: saved-rbp -> 0 (stop), return -> non-canonical (must be dropped).
        wr(&mut mem, 0x5010, 0x0);
        wr(&mut mem, 0x5018, 0x0000_8000_0000_0000);

        let mut profiler = GuestProfiler {
            cfg: ProfilingConfig {
                guest_profile_path: PathBuf::from("x.folded"),
                sample_hz: 997,
                kernel_symbols: None,
                user_symbols: vec![],
                host_profile: false,
                wpr_profile: None,
                run_id: "test".to_string(),
            },
            kernel_syms: vec![],
            user_syms: vec![],
            stacks: HashMap::new(),
            overflow: 0,
            retained_bytes: 0,
        };

        let read = |gpa: u64, out: &mut [u8]| {
            let off = gpa as usize;
            if off + out.len() > mem.len() {
                return false;
            }
            out.copy_from_slice(&mem[off..off + out.len()]);
            true
        };
        profiler.record_sample(0x0040_2000, 0x5000, pml4, false, read);

        // rip + one valid canonical return; the non-canonical return is dropped.
        let ((asid, frames), count) = profiler.stacks.iter().next().unwrap();
        assert_eq!(*count, 1);
        // rip is a user-space address, so the sample is keyed by its normalized CR3.
        assert_eq!(*asid, pml4 & CR3_ADDR_MASK);
        assert_eq!(frames, &vec![0x0040_2000, 0x0040_1000]);
    }

    #[test]
    fn record_sample_stops_on_unaligned_frame_pointer() {
        // A misaligned frame pointer cannot address a real saved-rbp/return-address pair (and an
        // unaligned 8-byte read could straddle a page), so the walk stops immediately and keeps
        // only the interrupted leaf — without performing any physical read.
        let mut profiler = GuestProfiler {
            cfg: ProfilingConfig {
                guest_profile_path: PathBuf::from("x.folded"),
                sample_hz: 997,
                kernel_symbols: None,
                user_symbols: vec![],
                host_profile: false,
                wpr_profile: None,
                run_id: "test".to_string(),
            },
            kernel_syms: vec![],
            user_syms: vec![],
            stacks: HashMap::new(),
            overflow: 0,
            retained_bytes: 0,
        };
        let read = |_gpa: u64, _out: &mut [u8]| panic!("must not read through an unaligned fp");
        profiler.record_sample(0x0040_2000, 0x5001, 0x1000, false, read);
        let ((_asid, frames), count) = profiler.stacks.iter().next().unwrap();
        assert_eq!(*count, 1);
        assert_eq!(frames, &vec![0x0040_2000]);
    }

    fn temp_folded_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nvx-{}-{}.folded", tag, std::process::id()))
    }

    fn profiler_with_stacks(
        path: PathBuf,
        stacks: HashMap<(u64, Vec<u64>), u64>,
        overflow: u64,
    ) -> GuestProfiler {
        GuestProfiler {
            cfg: ProfilingConfig {
                guest_profile_path: path,
                sample_hz: 997,
                kernel_symbols: None,
                user_symbols: vec![],
                host_profile: false,
                wpr_profile: None,
                run_id: "test".to_string(),
            },
            kernel_syms: vec![],
            user_syms: vec![],
            stacks,
            overflow,
            retained_bytes: 0,
        }
    }

    #[test]
    fn write_folded_tags_distinct_user_address_spaces() {
        // Two user processes sample the *same* virtual address; they must stay separate and each be
        // rooted under its own address space.
        let asid_a = 0xa000u64;
        let asid_b = 0xb000u64;
        let mut stacks: HashMap<(u64, Vec<u64>), u64> = HashMap::new();
        stacks.insert((asid_a, vec![0x0040_1000]), 3);
        stacks.insert((asid_b, vec![0x0040_1000]), 5);
        stacks.insert((0, vec![0xffff_ffff_8100_0000]), 2);

        let path = temp_folded_path("tags");
        let profiler = profiler_with_stacks(path.clone(), stacks, 0);
        profiler.write_folded().unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(run_id_sidecar_path(&path));

        // The shared user address is not merged: it appears once per address space, each tagged.
        assert_eq!(content.matches("0x0000000000401000").count(), 2);
        assert_eq!(content.matches("[as 0x").count(), 2);
        assert!(content.contains(&format!("[as 0x{asid_a:012x}]")));
        assert!(content.contains(&format!("[as 0x{asid_b:012x}]")));
        // The kernel stack (address space 0) is never tagged.
        assert!(content.contains("0xffffffff81000000 2"));
    }

    #[test]
    fn write_folded_single_user_space_is_untagged() {
        // A single user process is the common case and needs no address-space disambiguation.
        let mut stacks: HashMap<(u64, Vec<u64>), u64> = HashMap::new();
        stacks.insert((0xa000, vec![0x0040_1000]), 3);
        stacks.insert((0, vec![0xffff_ffff_8100_0000]), 2);

        let path = temp_folded_path("single");
        let profiler = profiler_with_stacks(path.clone(), stacks, 0);
        profiler.write_folded().unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(run_id_sidecar_path(&path));

        assert!(!content.contains("[as 0x"));
        assert!(content.contains("0x0000000000401000 3"));
    }

    #[test]
    fn write_folded_reports_overflow() {
        // Dropped samples past the distinct-stack cap surface as a synthetic `[overflow]` frame so
        // a truncated profile is never silently under-counted.
        let mut stacks: HashMap<(u64, Vec<u64>), u64> = HashMap::new();
        stacks.insert((0, vec![0xffff_ffff_8100_0000]), 4);

        let path = temp_folded_path("overflow");
        let profiler = profiler_with_stacks(path.clone(), stacks, 7);
        profiler.write_folded().unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(run_id_sidecar_path(&path));

        assert!(content.contains("[overflow] 7"));
    }

    fn profiler_with_symbols(
        path: PathBuf,
        kernel_syms: Vec<Symbol>,
        user_syms: Vec<Symbol>,
        stacks: HashMap<(u64, Vec<u64>), u64>,
    ) -> GuestProfiler {
        GuestProfiler {
            cfg: ProfilingConfig {
                guest_profile_path: path,
                sample_hz: 997,
                kernel_symbols: None,
                user_symbols: vec![],
                host_profile: false,
                wpr_profile: None,
                run_id: "test".to_string(),
            },
            kernel_syms,
            user_syms,
            stacks,
            overflow: 0,
            retained_bytes: 0,
        }
    }

    #[test]
    fn resolves_kernel_and_user_when_single_user_space() {
        // Kernel frames resolve against the kernel table; user frames resolve against the user
        // table because exactly one user address space is present.
        let kernel = vec![make_symbol(0xffff_ffff_8100_0000, 0x100, "kernel_fn")];
        let user = vec![make_symbol(0x0040_1000, 0x100, "user_fn")];
        let mut stacks: HashMap<(u64, Vec<u64>), u64> = HashMap::new();
        stacks.insert((0xa000, vec![0x0040_1010]), 3);
        stacks.insert((0, vec![0xffff_ffff_8100_0010]), 2);

        let path = temp_folded_path("resolve-single");
        let profiler = profiler_with_symbols(path.clone(), kernel, user, stacks);
        profiler.write_folded().unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(run_id_sidecar_path(&path));

        assert!(content.contains("user_fn 3"));
        assert!(content.contains("kernel_fn 2"));
    }

    #[test]
    fn leaves_user_frames_raw_with_multiple_user_spaces() {
        // With several user address spaces the provided user ELF cannot be attributed to a specific
        // process, so user frames stay raw; kernel frames still resolve (the kernel image is shared).
        let kernel = vec![make_symbol(0xffff_ffff_8100_0000, 0x100, "kernel_fn")];
        let user = vec![make_symbol(0x0040_1000, 0x100, "user_fn")];
        let mut stacks: HashMap<(u64, Vec<u64>), u64> = HashMap::new();
        stacks.insert((0xa000, vec![0x0040_1010]), 3);
        stacks.insert((0xb000, vec![0x0040_1010]), 5);
        stacks.insert((0, vec![0xffff_ffff_8100_0010]), 2);

        let path = temp_folded_path("resolve-multi");
        let profiler = profiler_with_symbols(path.clone(), kernel, user, stacks);
        profiler.write_folded().unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(run_id_sidecar_path(&path));

        // The user symbol name is never emitted; the user frame appears as a raw address instead.
        assert!(!content.contains("user_fn"));
        assert!(content.contains("0x0000000000401010"));
        // Kernel frames still resolve regardless of user-space ambiguity.
        assert!(content.contains("kernel_fn 2"));
    }

    #[test]
    fn write_folded_publishes_run_id_sidecar() {
        // The `.run` provenance sidecar records the run id so full-mode post-processing can require
        // the host trace to belong to the same run.
        let mut stacks: HashMap<(u64, Vec<u64>), u64> = HashMap::new();
        stacks.insert((0, vec![0xffff_ffff_8100_0000]), 1);

        let path = temp_folded_path("runid");
        let profiler = profiler_with_stacks(path.clone(), stacks, 0);
        profiler.write_folded().unwrap();
        let sidecar = run_id_sidecar_path(&path);
        let content = fs::read_to_string(&sidecar).unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&sidecar);

        assert_eq!(content, "run=test\n");
    }
}

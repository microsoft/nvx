// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # Guest Profiler
//!
//! Host-side guest sampling support shared by both backends.
//!
//! The backends periodically interrupt the vCPU, read `(RIP, RBP, CR3, CR4.LA57)`, and call
//! [`GuestProfiler::record_sample`]. This module walks the guest frame-pointer chain through
//! guest page tables (x86_64 4-level or 5-level paging), resolves sampled PCs against
//! host-provided ELF symbol files, and writes folded stacks for flamegraph generation.
//!

mod host;
mod paging;
mod samples;
mod symbols;

pub use self::host::HostTraceSession;
pub use self::samples::GuestProfiler;

use ::std::fs;
use ::std::path::Path;
use ::std::path::PathBuf;

/// A symbol-source ELF plus the guest runtime address at which its image is loaded.
///
/// `load_base`, when set, is the *absolute* guest runtime load address of the image's first
/// loadable segment. Symbols are relocated by `load_base - min(PT_LOAD.p_vaddr)`, which resolves
/// both position-independent images (`ET_DYN`, i.e. PIE/ASLR, linked at 0 so the slide equals
/// `load_base`) and relocated fixed-address executables (`ET_EXEC` linked at a nonzero address).
/// For a fixed-address image loaded at its link address (e.g. a non-relocatable `vmlinux`), leave
/// `load_base` as `None` so its absolute symbol values are used unchanged.
#[derive(Clone, Debug)]
pub struct SymbolSource {
    /// ELF file providing the symbol table.
    pub path: PathBuf,
    /// Absolute guest runtime load address of the image, if relocated or position-independent.
    pub load_base: Option<u64>,
}

/// Runtime profiling configuration.
#[derive(Clone, Debug)]
pub struct ProfilingConfig {
    /// Output folded-stack file path.
    pub guest_profile_path: PathBuf,
    /// Sampling frequency in Hz.
    pub sample_hz: u32,
    /// Optional guest-kernel ELF symbol source.
    pub kernel_symbols: Option<SymbolSource>,
    /// Optional guest-user ELF symbol source(s).
    pub user_symbols: Vec<SymbolSource>,
    /// Whether host profiling should be enabled (platform-specific orchestration).
    pub host_profile: bool,
    /// Windows-only WPR recording-profile override. Accepts a bundled profile name
    /// (`NvxCpuScheduling` [default] or `NvxCpu`), a built-in WPR profile (e.g. `CPU`), or an
    /// explicit `path.wprp!ProfileName`. `None` selects the bundled default. Ignored on Linux.
    pub wpr_profile: Option<String>,
    /// Unique per-invocation provenance id. Written alongside both the guest folded profile
    /// (`<folded>.run`) and the host trace (`<trace>.pid`), so `full`-mode post-processing can
    /// require the two artifacts to belong to the *same* run and never merge a stale host trace
    /// left by a previous run.
    pub run_id: String,
}

/// Creates the parent directory of `path`, if any, so trace/profile writers can create files.
pub(crate) fn ensure_parent_dir(path: &Path) -> ::std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

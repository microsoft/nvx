// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # microvm
//!
//! A minimal x86_64 micro-VM (single processor by default, optional functional KVM SMP via
//! `--vcpus`) that boots a Linux (Alpine) kernel through the PVH boot protocol from a RAM
//! initramfs. There is no PCI, no ACPI, and no block device: the always-on PMIO model is a
//! bidirectional "portb" console (backing `hvc0`) plus minimal RTC wall-clock registers.
//!

// Portable modules, shared by both backends.
mod boot;
mod console;
mod devices;
#[cfg(any(target_os = "windows", test))]
mod l2bridge;
mod layout;
mod profiler;

// Linux backend: KVM-based VMM.
#[cfg(target_os = "linux")]
mod irq;
#[cfg(target_os = "linux")]
mod memory;
#[cfg(target_os = "linux")]
mod net;
#[cfg(target_os = "linux")]
mod snapshot;
#[cfg(target_os = "linux")]
mod vcpu;
#[cfg(target_os = "linux")]
mod virtfs;
#[cfg(target_os = "linux")]
mod vmm;

// Windows backend: Windows Hypervisor Platform (WHP) VMM.
#[cfg(target_os = "windows")]
mod whp;
#[cfg(target_os = "windows")]
mod windows_terminal;

use ::std::path::PathBuf;

use ::anyhow::{Context, Result, bail};
use ::clap::Parser;
use ::log::LevelFilter;

const PVH_DEFAULT_CMDLINE: &str = "earlycon=xe9 console=hvc0 reboot=t panic=-1";

/// Maximum guest sampling frequency. 8190 Hz is a sane upper bound that keeps sampling overhead
/// and the retained-sample memory footprint bounded while still giving fine resolution; higher
/// rates yield diminishing detail at rapidly growing cost. The guest sampler runs its own host-side
/// timer independent of any host-trace recorder, so this cap is purely a resource guard.
const MAX_PROFILE_HZ: u32 = 8_190;

/// Command-line arguments.
#[derive(Parser, Debug)]
#[command(
    name = "microvm",
    about = "Minimal x86_64 micro-VM that PVH-boots Linux from a RAM initramfs (optional KVM SMP via --vcpus)."
)]
struct Args {
    /// Path to the uncompressed `vmlinux` (PVH) kernel image.
    #[arg(long)]
    kernel: Option<PathBuf>,

    /// Path to the initramfs image (the RAM root filesystem).
    #[arg(long)]
    initrd: Option<PathBuf>,

    /// Kernel command line.
    #[arg(long)]
    cmdline: Option<String>,

    /// Guest RAM size, in MiB.
    #[arg(long, default_value_t = 512)]
    mem: u64,

    /// Suppress terminal rendering of guest console output. Also silences all VMM logging
    /// unless `--log-level` is given explicitly (i.e. `--quiet` alone is fully silent).
    #[arg(long)]
    quiet: bool,

    /// Logging verbosity: off, error, warn, info, debug, or trace. Use `off` to suppress
    /// all logging. Defaults to `info` (or `off` when `--quiet` is set). `RUST_LOG`
    /// overrides this.
    #[arg(long)]
    log_level: Option<String>,

    /// Stop the VM as soon as the boot marker is seen, and report the cold-start time.
    #[arg(long)]
    exit_on_boot: bool,

    /// Console substring that marks boot completion (for cold-start timing).
    #[arg(long, default_value = "ALPINE-MICROVM-BOOT-OK")]
    boot_marker: String,

    /// Additional console substring to time from the first guest instruction. Repeat as
    /// `--timing-marker LABEL=TEXT`; results are printed when the VM exits.
    #[arg(long, value_name = "LABEL=TEXT")]
    timing_marker: Vec<console::TimingMarker>,

    /// For redirected stdin on a cold boot, wait until the boot marker appears before forwarding
    /// input to the guest console. Interactive terminals and restores are never delayed.
    #[arg(long)]
    defer_stdin_until_boot: bool,

    /// Take a snapshot into this directory when the guest requests one, then exit.
    #[arg(long)]
    snapshot: Option<PathBuf>,

    /// Restore and resume the VM from this snapshot directory instead of booting a kernel.
    #[arg(long)]
    restore: Option<PathBuf>,

    /// Windows named pipe used to report that snapshot state and devices are fully restored.
    #[arg(long, value_name = "PIPE", requires = "restore")]
    restore_ready_pipe: Option<String>,

    /// Export this host directory to the guest as a filesystem, mounted at `--mount-target`.
    /// Read-only (SquashFS) by default; pass `--mount-rw` (or `--mount-image`) to mount it
    /// read-write (ext4). Surfaced in the guest as an MTD/block device.
    #[arg(long, value_name = "DIR")]
    mount: Option<PathBuf>,

    /// Guest mount point for the `--mount` directory.
    #[arg(long, value_name = "PATH", default_value = "/mnt/host")]
    mount_target: String,

    /// Mount the `--mount` filesystem read-write (ext4) instead of read-only (SquashFS). Without
    /// `--mount-image` the writable image is held in guest memory, so changes are discarded when
    /// the VM stops.
    #[arg(long)]
    mount_rw: bool,

    /// Back a read-write `--mount` with this host file (implies `--mount-rw`). The ext4 image is
    /// created from `--mount` the first time and reused afterwards, and guest writes are flushed
    /// back to it, so changes persist across runs.
    #[arg(long, value_name = "FILE")]
    mount_image: Option<PathBuf>,

    /// Size (in MiB) of the writable ext4 image, giving the guest headroom to create files.
    /// Applies to `--mount-rw`; ignored for a read-only mount or an existing `--mount-image`.
    #[arg(long, value_name = "MiB")]
    mount_size: Option<u64>,

    /// Run the platform backend's protected-mode self-test instead of booting.
    #[arg(long)]
    selftest: bool,

    /// Attach a virtio-net NIC and expose a host network endpoint to the guest. The value is the
    /// guest's IPv4 address and subnet (e.g. `--net 10.0.0.2/24`); the host side of the link takes
    /// the first address of that subnet (10.0.0.1) and becomes the guest's gateway. Creating and
    /// configuring the host TAP needs privileges — run the VMM as root or allow passwordless
    /// `sudo ip`.
    #[arg(long, value_name = "IP/PREFIX")]
    net: Option<String>,

    /// Path to a versioned external L2Bridge network manifest. This is mutually exclusive with
    /// `--net`, which remains the standalone SLIRP configuration.
    #[arg(long, value_name = "JSON", conflicts_with = "net")]
    net_config: Option<PathBuf>,

    /// Attach to this pre-existing, user-owned host TAP instead of creating one per run. The TAP
    /// must already be configured (MAC, address, up); the VMM binds to it with a single ioctl (no
    /// privileged `ip` calls) and leaves it in place on exit. This lets `--restore` resume a
    /// networked guest without any per-run TAP setup. Pair with `--net` on a cold boot; on
    /// `--restore` the guest addressing comes from the snapshot.
    #[arg(long, value_name = "NAME")]
    net_tap: Option<String>,

    /// Number of vCPUs to create (functional SMP). Default 1 (single processor). With N > 1 the
    /// VMM writes an Intel MP table so the guest kernel enumerates all N vCPUs and brings the
    /// application processors online via the normal INIT-SIPI-SIPI path
    /// (serviced by the in-kernel LAPIC). On `--restore` the processor count comes from the
    /// snapshot. `--snapshot` captures a consistent VM-wide cut of all N processors. Maximum 254
    /// (8-bit APIC ids).
    #[arg(long, default_value_t = 1)]
    vcpus: usize,

    /// Write guest folded stacks to this file and enable guest sampling.
    #[arg(long, value_name = "FILE")]
    guest_profile: Option<PathBuf>,

    /// Guest sampling frequency in Hertz.
    #[arg(long, value_name = "HZ", default_value_t = 997)]
    profile_hz: u32,

    /// Guest-kernel ELF symbol file used to resolve sampled PCs. Append `@0x<base>` to give the
    /// absolute guest runtime load address of a relocated or position-independent (PIE/ASLR) image.
    #[arg(long, value_name = "ELF[@BASE]")]
    kernel_symbols: Option<String>,

    /// Guest-user ELF symbol file(s), comma-separated or repeated. Append `@0x<base>` per file to
    /// give the absolute guest runtime load address of a relocated or position-independent image.
    #[arg(long, value_name = "ELF[@BASE]", value_delimiter = ',')]
    user_symbols: Vec<String>,

    /// Capture host-side profiling traces for correlation.
    #[arg(long)]
    host_profile: bool,

    /// Windows only: WPR recording profile for host tracing. Accepts a bundled profile name
    /// (`NvxCpuScheduling` [default] — CPU samples plus scheduling events; or `NvxCpu` — leaner,
    /// CPU samples only), a built-in WPR profile (e.g. `CPU`, `GeneralProfile`), or an explicit
    /// `path.wprp!ProfileName`. Overrides the `NVX_WPR_PROFILE` environment variable.
    #[arg(long, value_name = "NAME|FILE!NAME")]
    wpr_profile: Option<String>,
}

/// Parses a logging level name into a [`LevelFilter`].
fn parse_level(name: &str) -> Result<LevelFilter> {
    match name.to_ascii_lowercase().as_str() {
        "off" => Ok(LevelFilter::Off),
        "error" => Ok(LevelFilter::Error),
        "warn" => Ok(LevelFilter::Warn),
        "info" => Ok(LevelFilter::Info),
        "debug" => Ok(LevelFilter::Debug),
        "trace" => Ok(LevelFilter::Trace),
        other => bail!("invalid --log-level '{other}' (use off|error|warn|info|debug|trace)"),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Determine the logging level: explicit --log-level wins, otherwise --quiet suppresses
    // all logging and the default is `info`. RUST_LOG still overrides via `parse_default_env`.
    let level: LevelFilter = match &args.log_level {
        Some(name) => parse_level(name)?,
        None if args.quiet => LevelFilter::Off,
        None => LevelFilter::Info,
    };
    ::env_logger::Builder::new()
        .filter_level(level)
        .parse_default_env()
        .init();

    if args.selftest {
        return selftest(&args);
    }

    if args.mem == 0 {
        bail!("--mem must be greater than zero");
    }
    if args.vcpus == 0 {
        bail!("--vcpus must be greater than zero");
    }
    let mem_bytes: u64 = args
        .mem
        .checked_mul(1024 * 1024)
        .context("--mem is too large")?;

    if args.mount.is_none()
        && (args.mount_rw || args.mount_image.is_some() || args.mount_size.is_some())
    {
        bail!("--mount-rw, --mount-image and --mount-size require --mount <dir>");
    }

    let profiling = build_profiling_config(&args)?;
    dispatch(args, mem_bytes, profiling)
}

fn build_profiling_config(args: &Args) -> Result<Option<profiler::ProfilingConfig>> {
    #[cfg(not(target_os = "windows"))]
    if args.wpr_profile.is_some() {
        bail!("--wpr-profile is only supported on Windows (it selects the WPR host-trace profile)");
    }
    if args.guest_profile.is_none()
        && (args.kernel_symbols.is_some()
            || !args.user_symbols.is_empty()
            || args.host_profile
            || args.wpr_profile.is_some())
    {
        bail!(
            "--kernel-symbols, --user-symbols, --host-profile and --wpr-profile require \
             --guest-profile <file>"
        );
    }
    let Some(guest_profile_path) = &args.guest_profile else {
        return Ok(None);
    };
    if args.wpr_profile.is_some() && !args.host_profile {
        bail!("--wpr-profile requires --host-profile");
    }
    if args.profile_hz == 0 {
        bail!("--profile-hz must be greater than zero");
    }
    if args.profile_hz > MAX_PROFILE_HZ {
        bail!("--profile-hz must be at most {}", MAX_PROFILE_HZ);
    }

    let kernel_symbols = match &args.kernel_symbols {
        Some(spec) => Some(parse_symbol_source(spec)?),
        None => None,
    };
    let user_symbols = args
        .user_symbols
        .iter()
        .map(|spec| parse_symbol_source(spec))
        .collect::<Result<Vec<_>>>()?;

    Ok(Some(profiler::ProfilingConfig {
        guest_profile_path: guest_profile_path.clone(),
        sample_hz: args.profile_hz,
        kernel_symbols,
        user_symbols,
        host_profile: args.host_profile,
        wpr_profile: args.wpr_profile.clone(),
        run_id: generate_run_id(),
    }))
}

/// Generates a unique provenance id for one profiling invocation.
///
/// Combines the wall-clock time with the process id so a guest folded profile and its host trace
/// produced by the same run share an id that a later run cannot collide with; `full`-mode
/// post-processing uses it to refuse a host trace that does not belong to the current run.
fn generate_run_id() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{pid:x}")
}

/// Parses a `path[@base]` symbol-source specification.
///
/// The optional `@base` suffix (hexadecimal `0x…` or decimal) gives the *absolute* guest runtime
/// address the image is loaded at. Symbols are relocated by `base - min(PT_LOAD.p_vaddr)`, so it
/// resolves both a PIE (linked at 0) and a relocated `ET_EXEC` (linked at a nonzero address). It is
/// only treated as a base when the suffix parses as a number, so paths that legitimately contain
/// `@` are preserved.
fn parse_symbol_source(spec: &str) -> Result<profiler::SymbolSource> {
    if let Some((path, base)) = spec.rsplit_once('@') {
        if !path.is_empty() {
            if let Some(load_base) = parse_u64_auto(base) {
                return Ok(profiler::SymbolSource {
                    path: PathBuf::from(path),
                    load_base: Some(load_base),
                });
            }
        }
    }
    Ok(profiler::SymbolSource {
        path: PathBuf::from(spec),
        load_base: None,
    })
}

/// Parses an unsigned integer written as `0x…` hexadecimal or plain decimal.
fn parse_u64_auto(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse::<u64>().ok(),
    }
}

/// Runs the KVM protected-mode self-test.
#[cfg(target_os = "linux")]
fn selftest(_args: &Args) -> Result<()> {
    vmm::selftest()
}

/// Runs the WHP protected-mode self-test.
#[cfg(target_os = "windows")]
fn selftest(_args: &Args) -> Result<()> {
    whp::selftest()
}

/// Builds the backend configuration and runs the VM (Linux / KVM backend).
#[cfg(target_os = "linux")]
fn dispatch(
    args: Args,
    mem_bytes: u64,
    profiling: Option<profiler::ProfilingConfig>,
) -> Result<()> {
    if args.net_config.is_some() {
        bail!("--net-config is only available on the Windows/WHP backend");
    }
    if args.restore_ready_pipe.is_some() {
        bail!("--restore-ready-pipe is only available on the Windows/WHP backend");
    }
    // Parse the optional virt-net endpoint (guest IP/prefix), deriving the host gateway.
    let net: Option<net::NetConfig> = match &args.net {
        Some(spec) => Some(net::NetConfig::parse(spec)?),
        None => None,
    };

    if args.net_tap.is_some() && args.net.is_none() && args.restore.is_none() {
        bail!("--net-tap requires --net (cold boot) or --restore (a networked snapshot)");
    }

    if args.vcpus > crate::boot::mptable::MAX_SUPPORTED_CPUS as usize {
        bail!(
            "--vcpus supports at most {} CPUs (the MP table uses 8-bit APIC ids)",
            crate::boot::mptable::MAX_SUPPORTED_CPUS
        );
    }

    vmm::run(vmm::Config {
        kernel: args.kernel,
        initrd: args.initrd,
        cmdline: args
            .cmdline
            .unwrap_or_else(|| PVH_DEFAULT_CMDLINE.to_string()),
        mem_bytes,
        quiet: args.quiet,
        exit_on_boot: args.exit_on_boot,
        boot_marker: args.boot_marker,
        timing_markers: args.timing_marker,
        defer_stdin_until_boot: args.defer_stdin_until_boot,
        snapshot: args.snapshot,
        restore: args.restore,
        mount: args.mount,
        mount_target: args.mount_target,
        mount_rw: args.mount_rw,
        mount_image: args.mount_image,
        mount_size: args.mount_size,
        net,
        net_tap: args.net_tap,
        vcpus: args.vcpus,
        profiling,
    })
}

/// Builds the backend configuration and runs the VM (Windows / WHP backend).
///
/// The WHP backend implements the core PVH boot path (kernel + RAM initramfs + portb console),
/// snapshot/restore, virt-net (`--net`) through a user-mode NAT, and virt-fs (`--mount`) via a
/// pure-Rust FAT image. The TAP-attach option (`--net-tap`, which is Linux-specific) and
/// multi-vCPU (`--vcpus`, KVM-only) configurations are rejected here rather than silently ignored.
#[cfg(target_os = "windows")]
fn dispatch(
    args: Args,
    mem_bytes: u64,
    profiling: Option<profiler::ProfilingConfig>,
) -> Result<()> {
    if args.net_tap.is_some() {
        bail!("--net-tap is only available on the Linux/KVM backend (WHP uses a user-mode NAT)");
    }
    if args.vcpus > 1 {
        bail!("--vcpus > 1 is only available on the Linux/KVM backend (WHP is single-vCPU)");
    }

    let net: Option<whp::NetConfig> = match &args.net {
        Some(spec) => Some(whp::NetConfig::parse(spec)?),
        None => None,
    };
    let net_config: Option<l2bridge::L2BridgeConfig> = match &args.net_config {
        Some(path) => Some(l2bridge::L2BridgeConfig::from_path(path)?),
        None => None,
    };

    whp::run(whp::Config {
        kernel: args.kernel,
        initrd: args.initrd,
        cmdline: args
            .cmdline
            .unwrap_or_else(|| PVH_DEFAULT_CMDLINE.to_string()),
        mem_bytes,
        quiet: args.quiet,
        exit_on_boot: args.exit_on_boot,
        boot_marker: args.boot_marker,
        timing_markers: args.timing_marker,
        defer_stdin_until_boot: args.defer_stdin_until_boot,
        snapshot: args.snapshot,
        restore: args.restore,
        restore_ready_pipe: args.restore_ready_pipe,
        net,
        net_config,
        mount: args.mount,
        mount_target: args.mount_target,
        mount_rw: args.mount_rw,
        mount_image: args.mount_image,
        mount_size: args.mount_size,
        profiling,
    })
}

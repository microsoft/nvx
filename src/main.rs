// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # microvm
//!
//! A minimal x86_64 micro-VM (single processor by default, optional functional KVM SMP via
//! `--vcpus`) that boots a Linux (Alpine) kernel through the PVH boot protocol from a RAM
//! initramfs. There is no PCI, no ACPI, and no block device: the only always-on emulated device
//! is a bidirectional "portb" console (backing `hvc0`).
//!

// Portable modules, shared by both backends.
mod boot;
mod console;
mod devices;
mod layout;

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

use ::std::path::PathBuf;

use ::anyhow::{Context, Result, bail};
use ::clap::Parser;
use ::log::LevelFilter;

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
    #[arg(long, default_value = "earlycon=xe9 console=hvc0 reboot=t panic=-1")]
    cmdline: String,

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

    /// Take a snapshot into this directory when the guest requests one (control port 0x605),
    /// then exit.
    #[arg(long)]
    snapshot: Option<PathBuf>,

    /// Restore and resume the VM from this snapshot directory instead of booting a kernel.
    #[arg(long)]
    restore: Option<PathBuf>,

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

    /// Run a tiny protected-mode self-test instead of booting a kernel.
    #[arg(long)]
    selftest: bool,

    /// Attach a virtio-net NIC and expose a host network endpoint to the guest. The value is the
    /// guest's IPv4 address and subnet (e.g. `--net 10.0.0.2/24`); the host side of the link takes
    /// the first address of that subnet (10.0.0.1) and becomes the guest's gateway. Creating and
    /// configuring the host TAP needs privileges — run the VMM as root or allow passwordless
    /// `sudo ip`.
    #[arg(long, value_name = "IP/PREFIX")]
    net: Option<String>,

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
        return selftest();
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

    dispatch(args, mem_bytes)
}

/// Runs the protected-mode self-test on the active backend.
#[cfg(target_os = "linux")]
fn selftest() -> Result<()> {
    vmm::selftest()
}

/// Runs the protected-mode self-test on the active backend.
#[cfg(target_os = "windows")]
fn selftest() -> Result<()> {
    whp::selftest()
}

/// Builds the backend configuration and runs the VM (Linux / KVM backend).
#[cfg(target_os = "linux")]
fn dispatch(args: Args, mem_bytes: u64) -> Result<()> {
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
        cmdline: args.cmdline,
        mem_bytes,
        quiet: args.quiet,
        exit_on_boot: args.exit_on_boot,
        boot_marker: args.boot_marker,
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
    })
}

/// Builds the backend configuration and runs the VM (Windows / WHP backend).
///
/// The WHP backend implements the core PVH boot path (kernel + RAM initramfs + portb console),
/// snapshot/restore, virt-net (`--net`) through a user-mode NAT, and virt-fs (`--mount`) via a
/// pure-Rust FAT image. The TAP-attach option (`--net-tap`, which is Linux-specific) and
/// multi-vCPU (`--vcpus`, KVM-only) configurations are rejected here rather than silently ignored.
#[cfg(target_os = "windows")]
fn dispatch(args: Args, mem_bytes: u64) -> Result<()> {
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

    whp::run(whp::Config {
        kernel: args.kernel,
        initrd: args.initrd,
        cmdline: args.cmdline,
        mem_bytes,
        quiet: args.quiet,
        exit_on_boot: args.exit_on_boot,
        boot_marker: args.boot_marker,
        snapshot: args.snapshot,
        restore: args.restore,
        net,
        mount: args.mount,
        mount_target: args.mount_target,
        mount_rw: args.mount_rw,
        mount_image: args.mount_image,
        mount_size: args.mount_size,
    })
}

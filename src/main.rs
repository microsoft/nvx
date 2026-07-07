// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # microvm
//!
//! A minimal, single-core x86_64 KVM micro-VM that boots a Linux (Alpine) kernel through
//! the PVH boot protocol from a RAM initramfs. There is no PCI, no ACPI, and no block
//! device: the only emulated device is a 16550 serial console.
//!

mod boot;
mod console;
mod devices;
mod irq;
mod layout;
mod memory;
mod vcpu;
mod vmm;

use ::std::path::PathBuf;

use ::anyhow::{
    Context,
    Result,
    bail,
};
use ::clap::Parser;

/// Command-line arguments.
#[derive(Parser, Debug)]
#[command(
    name = "microvm",
    about = "Minimal single-core x86_64 KVM micro-VM that PVH-boots Linux from a RAM initramfs."
)]
struct Args {
    /// Path to the uncompressed `vmlinux` (PVH) kernel image.
    #[arg(long)]
    kernel: Option<PathBuf>,

    /// Path to the initramfs image (the RAM root filesystem).
    #[arg(long)]
    initrd: Option<PathBuf>,

    /// Kernel command line.
    #[arg(long, default_value = "console=ttyS0 reboot=t panic=-1")]
    cmdline: String,

    /// Guest RAM size, in MiB.
    #[arg(long, default_value_t = 512)]
    mem: u64,

    /// Suppress terminal rendering of guest console output (measures a silent boot).
    #[arg(long)]
    quiet: bool,

    /// Stop the VM as soon as the boot marker is seen, and report the cold-start time.
    #[arg(long)]
    exit_on_boot: bool,

    /// Console substring that marks boot completion (for cold-start timing).
    #[arg(long, default_value = "ALPINE-MICROVM-BOOT-OK")]
    boot_marker: String,

    /// Run a tiny protected-mode self-test instead of booting a kernel.
    #[arg(long)]
    selftest: bool,
}

fn main() -> Result<()> {
    ::env_logger::Builder::from_env(::env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    if args.selftest {
        return vmm::selftest();
    }

    if args.mem == 0 {
        bail!("--mem must be greater than zero");
    }
    let mem_bytes: u64 = args
        .mem
        .checked_mul(1024 * 1024)
        .context("--mem is too large")?;

    let kernel: PathBuf = args.kernel.context("--kernel is required")?;

    vmm::run(vmm::Config {
        kernel,
        initrd: args.initrd,
        cmdline: args.cmdline,
        mem_bytes,
        quiet: args.quiet,
        exit_on_boot: args.exit_on_boot,
        boot_marker: args.boot_marker,
    })
}

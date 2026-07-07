// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The VM run-loop structure is derived from the KVM backend of the Nanvix Micro-VM
// (uservm), Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Virtual Machine Monitor
//!
//! Ties together guest memory, the interrupt controller, the vCPU, and the device bus, and
//! drives the single-core execution loop. Bytes typed on the host console are delivered to
//! the guest console device by a dedicated input thread that wakes the vCPU with `SIGUSR1`.
//!

use ::std::fs;
use ::std::io::{
    self,
    Read,
};
use ::std::path::{
    Path,
    PathBuf,
};
use ::std::sync::atomic::{
    AtomicBool,
    AtomicU64,
    Ordering,
};
use ::std::sync::{
    Arc,
    Mutex,
};
use ::std::thread;
use ::std::thread::JoinHandle;

use ::anyhow::{
    Context,
    Result,
    anyhow,
    bail,
};
use ::kvm_ioctls::{
    Kvm,
    VcpuExit,
};
use ::log::{
    debug,
    error,
    info,
    warn,
};

use crate::boot::pvh;
use crate::console::Console;
use crate::devices::{
    DeviceBus,
    PioAction,
};
use crate::devices::portb::PortConsole;
use crate::irq;
use crate::memory::GuestMemory;
use crate::net::{
    self,
    VirtioNet,
};
use crate::snapshot::{
    self,
    Snapshot,
};
use crate::vcpu::Vcpu;
use crate::virtfs;

/// Guest-physical address of the emulated task-state segment required by VT-x.
const TSS_ADDRESS: usize = 0xfffb_d000;

/// Restores the controlling terminal to its original mode on drop.
///
/// When standard input is a terminal it is switched to raw mode so that keystrokes reach
/// the guest console immediately and without local echo, while output post-processing is kept
/// so newlines still carriage-return (no "staircase"). When standard input is not a terminal
/// (e.g. a pipe) this is a no-op, so scripted runs are unaffected.
struct TtyGuard {
    fd: ::libc::c_int,
    saved: Option<::libc::termios>,
}

impl TtyGuard {
    fn new() -> Self {
        let fd: ::libc::c_int = ::libc::STDIN_FILENO;
        // SAFETY: `isatty`/`tcgetattr`/`cfmakeraw`/`tcsetattr` operate on a valid fd and a
        // stack-allocated `termios`.
        unsafe {
            if ::libc::isatty(fd) == 1 {
                let mut termios: ::libc::termios = ::core::mem::zeroed();
                if ::libc::tcgetattr(fd, &mut termios) == 0 {
                    let saved: ::libc::termios = termios;
                    ::libc::cfmakeraw(&mut termios);
                    // `cfmakeraw` clears OPOST, which also drops ONLCR (map `\n` -> `\r\n` on
                    // output). Without it every newline the VMM emits while the guest runs -- its
                    // own log lines and the guest console stream -- only line-feeds and never
                    // returns to column 0, so the terminal "staircases". Re-enable output
                    // post-processing; input stays raw so keystrokes still reach the guest.
                    termios.c_oflag |= ::libc::OPOST | ::libc::ONLCR;
                    ::libc::tcsetattr(fd, ::libc::TCSANOW, &termios);
                    return Self { fd, saved: Some(saved) };
                }
            }
        }
        Self { fd, saved: None }
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        if let Some(saved) = self.saved {
            // SAFETY: `saved` was produced by `tcgetattr` on the same fd.
            unsafe {
                ::libc::tcsetattr(self.fd, ::libc::TCSANOW, &saved);
            }
        }
    }
}

/// Configuration for a micro-VM instance.
pub struct Config {
    /// Path to the uncompressed `vmlinux` (PVH) kernel image (not needed when restoring).
    pub kernel: Option<PathBuf>,
    /// Optional path to the initramfs image.
    pub initrd: Option<PathBuf>,
    /// Kernel command line.
    pub cmdline: String,
    /// Guest RAM size, in bytes.
    pub mem_bytes: u64,
    /// Suppress terminal rendering of guest console output (still counted and scanned).
    pub quiet: bool,
    /// Stop the VM as soon as the boot marker is observed (for cold-start measurement).
    pub exit_on_boot: bool,
    /// Console substring whose appearance marks boot completion.
    pub boot_marker: String,
    /// Directory to write a snapshot to when the guest requests one (control port `0x605`).
    pub snapshot: Option<PathBuf>,
    /// Directory to restore the VM from instead of cold-booting a kernel.
    pub restore: Option<PathBuf>,
    /// Host directory to export to the guest as a virt-fs (`--mount`).
    pub mount: Option<PathBuf>,
    /// Guest mount point for the `--mount` directory.
    pub mount_target: String,
    /// Export the `--mount` directory read-write (ext4) instead of read-only (SquashFS).
    pub mount_rw: bool,
    /// Optional host file backing a read-write `--mount` (implies read-write; persists writes).
    pub mount_image: Option<PathBuf>,
    /// Optional size (MiB) of the writable ext4 image (headroom for guest writes).
    pub mount_size: Option<u64>,
    /// Optional virt-net endpoint (`--net`): the guest IP/prefix and derived host gateway.
    pub net: Option<net::NetConfig>,
    /// Optional pre-existing host TAP to attach to instead of creating one (`--net-tap`).
    pub net_tap: Option<String>,
}

/// A running virt-net NIC: the shared device model plus the TAP descriptor the receive thread
/// reads from. The backing [`HostTap`](crate::net::HostTap) is kept alive separately by the
/// caller for the VM's lifetime.
struct NetDevice {
    /// The virtio-net device, shared between the vCPU thread (MMIO/TX) and the receive thread.
    dev: Arc<Mutex<VirtioNet>>,
    /// Raw TAP file descriptor, polled by the receive thread.
    tap_fd: ::std::os::fd::RawFd,
}

/// Runs a tiny 32-bit self-test program through the same `setup_pvh` entry path to validate
/// the VMM's protected-mode setup independently of the kernel image.
pub fn selftest() -> Result<()> {
    let kvm: Kvm = Kvm::new().context("opening /dev/kvm")?;
    let vm_fd = kvm.create_vm().context("KVM_CREATE_VM failed")?;
    vm_fd.set_tss_address(TSS_ADDRESS).context("KVM_SET_TSS_ADDR failed")?;
    let mem: GuestMemory = GuestMemory::new(&vm_fd, 16 << 20)?;
    let mut vcpu: Vcpu = Vcpu::new(&kvm, &vm_fd, 0)?;

    // 32-bit program: write "HI\n" to COM1 (0x3f8), then HLT.
    //   mov edx, 0x3f8 ; mov al,'H'; out dx,al ; mov al,'I'; out dx,al ;
    //   mov al,0x0a   ; out dx,al ; hlt
    let prog: [u8; 15] = [
        0xba, 0xf8, 0x03, 0x00, 0x00, // mov edx, 0x3f8
        0xb0, b'H', 0xee, // mov al,'H'; out dx,al
        0xb0, b'I', 0xee, // mov al,'I'; out dx,al
        0xb0, 0x0a, 0xee, // mov al,'\n'; out dx,al
        0xf4, // hlt
    ];
    let entry: u64 = 0x0010_0000;
    mem.write_slice(entry, &prog)?;
    vcpu.setup_pvh(&mem, entry, 0x6000)?;

    info!("selftest: running tiny 32-bit program at {entry:#x}");
    loop {
        match vcpu.fd.run() {
            Ok(VcpuExit::IoOut(port, data)) => {
                if port == 0x3f8 {
                    for &byte in data {
                        print!("{}", byte as char);
                    }
                    let _ = io::Write::flush(&mut io::stdout());
                }
            },
            Ok(VcpuExit::Hlt) => {
                info!("selftest: guest halted as expected — VMM protected-mode setup OK");
                break;
            },
            Ok(other) => {
                error!("selftest: unexpected exit {other:?}");
                dump_vcpu(&vcpu);
                break;
            },
            Err(e) => {
                error!("selftest: KVM_RUN error: {e}");
                dump_vcpu(&vcpu);
                break;
            },
        }
    }
    Ok(())
}

///
/// # Description
///
/// Creates and runs a micro-VM until the guest halts, resets, or is snapshotted. Either
/// cold-boots a kernel or restores from a snapshot directory, depending on [`Config`].
///
pub fn run(cfg: Config) -> Result<()> {
    if let Some(dir) = cfg.restore.clone() {
        run_restore(cfg, &dir)
    } else {
        run_cold(cfg)
    }
}

/// Cold-boots a kernel + initramfs via the PVH protocol.
fn run_cold(cfg: Config) -> Result<()> {
    let kernel_path: &PathBuf = cfg.kernel.as_ref().context("--kernel is required")?;
    let kernel: Vec<u8> = fs::read(kernel_path)
        .with_context(|| format!("reading kernel image {kernel_path:?}"))?;
    let initrd: Option<Vec<u8>> = match &cfg.initrd {
        Some(path) => {
            Some(fs::read(path).with_context(|| format!("reading initramfs {path:?}"))?)
        },
        None => None,
    };

    // Bring up the VM, its memory, interrupt controller, and vCPU.
    let kvm: Kvm = Kvm::new().context("opening /dev/kvm")?;
    if kvm.get_api_version() != 12 {
        bail!("unexpected KVM API version {}", kvm.get_api_version());
    }
    let vm_fd = kvm.create_vm().context("KVM_CREATE_VM failed")?;
    vm_fd
        .set_tss_address(TSS_ADDRESS)
        .context("KVM_SET_TSS_ADDR failed")?;

    let mem: GuestMemory = GuestMemory::new(&vm_fd, cfg.mem_bytes)?;
    let ram_size: u64 = mem.ram_size();
    irq::setup(&vm_fd)?;
    let mut vcpu: Vcpu = Vcpu::new(&kvm, &vm_fd, 0)?;

    // Load the kernel, the initramfs, and the PVH boot structures.
    let loaded = pvh::load_kernel(&mem, &kernel)?;
    info!(
        "loaded kernel: pvh_entry={:#x}, kernel_end={:#x}",
        loaded.pvh_entry, loaded.kernel_end
    );
    let initrd_region = match &initrd {
        Some(bytes) => {
            let region = pvh::load_initramfs(&mem, bytes, loaded.kernel_end, ram_size)?;
            info!("loaded initramfs: addr={:#x}, size={:#x}", region.addr, region.size);
            Some(region)
        },
        None => None,
    };
    // Optionally export a host directory to the guest as a virt-fs. The filesystem image is
    // mapped into guest memory above reported RAM and pointed at via the kernel command line;
    // `_virtfs` owns that mapping (and, for a persistent read-write export, flushes it) and must
    // stay alive until the guest stops.
    let mut cmdline: String = cfg.cmdline.clone();
    let _virtfs: Option<virtfs::VirtFs> = match &cfg.mount {
        Some(dir) => {
            let opts = virtfs::Options {
                dir,
                target: &cfg.mount_target,
                writable: cfg.mount_rw || cfg.mount_image.is_some(),
                image: cfg.mount_image.as_deref(),
                size: cfg.mount_size.map(|mib| mib << 20),
            };
            let (fs, fragment) = virtfs::load(&vm_fd, ram_size, opts)?;
            cmdline.push(' ');
            cmdline.push_str(&fragment);
            Some(fs)
        },
        None => None,
    };

    // Optionally attach a virt-net NIC (see --net). Its guest-visible location (a virtio-mmio
    // window) and the addresses its `init` should use are handed to the guest on the kernel
    // command line, mirroring the virt-fs approach above.
    if let Some(ncfg) = &cfg.net {
        cmdline.push(' ');
        cmdline.push_str(&ncfg.cmdline_fragment());
    }

    let start_info_gpa: u64 = pvh::configure(&mem, &cmdline, initrd_region)?;
    vcpu.setup_pvh(&mem, loaded.pvh_entry, start_info_gpa)?;

    let (console, bus) = build_io(&cfg, None);

    // Bring up the virt-net NIC: register its irqfd, create/configure the host TAP, and build the
    // shared device model. `_net_tap` owns the TAP interface (and tears it down on drop) and must
    // outlive the guest and the receive thread.
    let (net_dev, _net_tap): (Option<NetDevice>, Option<net::HostTap>) = match &cfg.net {
        Some(ncfg) => {
            let irq = net::register_irq(&vm_fd)?;
            let tap = net::HostTap::for_config(ncfg, cfg.net_tap.as_deref())?;
            let dev = Arc::new(Mutex::new(VirtioNet::new(mem.ram(), tap.raw_fd(), irq, ncfg.mac)));
            let tap_fd = tap.raw_fd();
            (Some(NetDevice { dev, tap_fd }), Some(tap))
        },
        None => (None, None),
    };

    info!("starting guest (mem={} MiB, cmdline={:?})", ram_size >> 20, cmdline);
    execute(&cfg, &vm_fd, &mut vcpu, &mem, &console, &bus, false, net_dev)
}

/// Restores and resumes a VM from a snapshot directory.
fn run_restore(cfg: Config, dir: &Path) -> Result<()> {
    let snap: Snapshot = Snapshot::read(dir)?;

    if cfg.net.is_some() {
        warn!("--net is ignored when restoring: networking is rebuilt from the snapshot itself");
    }

    let kvm: Kvm = Kvm::new().context("opening /dev/kvm")?;
    if kvm.get_api_version() != 12 {
        bail!("unexpected KVM API version {}", kvm.get_api_version());
    }
    let vm_fd = kvm.create_vm().context("KVM_CREATE_VM failed")?;
    vm_fd
        .set_tss_address(TSS_ADDRESS)
        .context("KVM_SET_TSS_ADDR failed")?;

    // Copy-on-write map the saved RAM image, recreate the irqchip/PIT, and reprogram CPUID.
    let mem: GuestMemory = GuestMemory::restore(&vm_fd, &dir.join("mem.bin"), snap.ram_size())?;
    irq::setup(&vm_fd)?;
    let mut vcpu: Vcpu = Vcpu::new(&kvm, &vm_fd, 0)?;

    // Reload the captured processor and VM state.
    snap.apply_vm(&vm_fd)?;
    snap.apply_vcpu(&vcpu.fd)?;

    // Rebuild the virt-net NIC from the snapshot: recreate the (identically addressed) host TAP,
    // re-register the irqfd, and reload the device's transport state so it resumes in lockstep
    // with the ring state already present in the restored guest RAM. `_net_tap` keeps the TAP
    // alive for the VM's lifetime.
    let (net_dev, _net_tap): (Option<NetDevice>, Option<net::HostTap>) =
        match restore_net(&vm_fd, &mem, snap.net_state(), cfg.net_tap.as_deref())? {
            Some((dev, tap)) => (Some(dev), Some(tap)),
            None => (None, None),
        };

    let (console, bus) = build_io(&cfg, Some(snap.con_state()));
    info!("resuming guest from snapshot {dir:?} (mem={} MiB)", snap.ram_size() >> 20);
    execute(&cfg, &vm_fd, &mut vcpu, &mem, &console, &bus, true, net_dev)
}

/// Rebuilds the virt-net NIC from serialized snapshot state, or returns `None` if the snapshot
/// had no NIC. Recreates the host TAP (same addressing and MAC as the original), registers the
/// irqfd, reloads the device's transport indices, and nudges the guest to resume traffic.
fn restore_net(
    vm_fd: &::kvm_ioctls::VmFd,
    mem: &GuestMemory,
    net_state: &[u8],
    net_tap: Option<&str>,
) -> Result<Option<(NetDevice, net::HostTap)>> {
    if net_state.is_empty() {
        return Ok(None);
    }
    let (ncfg, consumed) = net::NetConfig::from_header(net_state)?;
    let irq = net::register_irq(vm_fd)?;
    let tap = net::HostTap::for_config(&ncfg, net_tap)?;
    let tap_fd = tap.raw_fd();
    let mut dev = VirtioNet::new(mem.ram(), tap_fd, irq, ncfg.mac);
    dev.load(&net_state[consumed..])?;
    dev.resume();
    info!("virt-net: NIC restored (guest {}/{})", ncfg.guest_ip, ncfg.prefix);
    let dev = Arc::new(Mutex::new(dev));
    Ok(Some((NetDevice { dev, tap_fd }, tap)))
}

/// Builds the shared console sink, the portb console device, and the device bus. When
/// `con_state` is provided (restore path) the device's pending input queue is reloaded from it.
fn build_io(cfg: &Config, con_state: Option<&[u8]>) -> (Arc<Mutex<Console>>, DeviceBus) {
    let console: Arc<Mutex<Console>> =
        Arc::new(Mutex::new(Console::new(cfg.quiet, &cfg.boot_marker)));
    let con: Arc<Mutex<PortConsole>> =
        Arc::new(Mutex::new(PortConsole::new(Arc::clone(&console))));
    if let Some(state) = con_state {
        con.lock().expect("console poisoned").restore(state);
    }
    let bus: DeviceBus = DeviceBus::new(Arc::clone(&con));
    (console, bus)
}

/// Drives the single-core execution loop shared by the cold-boot and restore paths.
// The loop needs the full VM context (config, KVM handles, memory, the console/device bus, and the
// optional NIC); grouping these purely to satisfy the argument-count lint would not aid clarity.
#[allow(clippy::too_many_arguments)]
fn execute(
    cfg: &Config,
    vm_fd: &::kvm_ioctls::VmFd,
    vcpu: &mut Vcpu,
    mem: &GuestMemory,
    console: &Arc<Mutex<Console>>,
    bus: &DeviceBus,
    resumed: bool,
    net: Option<NetDevice>,
) -> Result<()> {
    install_signal_handlers();
    let _tty_guard: TtyGuard = TtyGuard::new();
    let vcpu_tid: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    spawn_input_thread(bus.console(), Arc::clone(&vcpu_tid));
    // SAFETY: `pthread_self` merely returns the calling thread's identifier.
    vcpu_tid.store(unsafe { ::libc::pthread_self() } as u64, Ordering::SeqCst);

    // Start the virt-net receive thread, if a NIC is attached. It feeds host frames into the
    // guest and is joined on shutdown (before guest memory is released).
    let net_stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let net_rx: Option<JoinHandle<()>> = net.as_ref().map(|nd| {
        net::spawn_rx_thread(Arc::clone(&nd.dev), nd.tap_fd, Arc::clone(&net_stop))
    });

    console.lock().expect("console poisoned").mark_start();

    // Deferred KVM_RUN error: stored so the receive thread is still joined on the way out.
    let mut run_err: Option<::anyhow::Error> = None;

    loop {
        // Flush buffered console output before re-entering the guest. The portb console is
        // interrupt-less (the guest's hvc driver polls), so there is no IRQ line to service.
        console.lock().expect("console poisoned").flush();

        match vcpu.fd.run() {
            Ok(VcpuExit::IoIn(port, data)) => bus.pio_read(port, data),
            Ok(VcpuExit::IoOut(port, data)) => match bus.pio_write(port, data) {
                PioAction::None => {},
                PioAction::Shutdown => {
                    info!("guest requested shutdown");
                    break;
                },
                PioAction::Snapshot => {
                    if take_snapshot(cfg, vm_fd, vcpu, mem, bus, console, net.as_ref())? {
                        break;
                    }
                },
            },
            // Guest MMIO to the virt-net device window (virtio-mmio). Other MMIO reads float to
            // zero and writes are dropped, matching the unoccupied-bus behaviour of PMIO.
            Ok(VcpuExit::MmioRead(addr, data)) => match net_mmio_offset(&net, addr) {
                Some((nd, off)) => nd.dev.lock().expect("virt-net poisoned").mmio_read(off, data),
                None => data.iter_mut().for_each(|b| *b = 0),
            },
            Ok(VcpuExit::MmioWrite(addr, data)) => {
                if let Some((nd, off)) = net_mmio_offset(&net, addr) {
                    nd.dev.lock().expect("virt-net poisoned").mmio_write(off, data);
                }
            },
            Ok(VcpuExit::Hlt) => {
                info!("guest halted");
                break;
            },
            Ok(VcpuExit::Shutdown) => {
                // A PVH/no-ACPI guest reboots via triple fault, which surfaces here. Treat
                // it as a normal termination of the VM.
                info!("guest reset (reboot/halt)");
                break;
            },
            Ok(VcpuExit::InternalError) => {
                error!("KVM internal error");
                dump_vcpu(vcpu);
                break;
            },
            Ok(VcpuExit::FailEntry(reason, cpu)) => {
                error!("KVM fail entry (reason={reason:#x}, cpu={cpu})");
                dump_vcpu(vcpu);
                break;
            },
            Ok(other) => debug!("unhandled vcpu exit: {other:?}"),
            // A host-thread signal (console input) interrupted KVM_RUN: loop to refresh
            // the interrupt line and re-enter the guest.
            Err(e) if e.errno() == ::libc::EINTR => {},
            Err(e) => {
                run_err = Some(anyhow!("KVM_RUN failed: {e}"));
                break;
            },
        }

        if cfg.exit_on_boot && console.lock().expect("console poisoned").booted() {
            info!("boot marker seen — stopping guest (--exit-on-boot)");
            break;
        }
    }

    // Stop and join the receive thread before guest memory (which it DMAs into) is dropped.
    net_stop.store(true, Ordering::SeqCst);
    if let Some(handle) = net_rx {
        let _ = handle.join();
    }

    console.lock().expect("console poisoned").flush();

    if let Some(err) = run_err {
        return Err(err);
    }

    // Report the boot/restore time independently of the logging level so it is available
    // even when all logging is suppressed.
    if cfg.exit_on_boot {
        let console = console.lock().expect("console poisoned");
        if let Some(elapsed) = console.cold_start() {
            let label: &str = if resumed { "restore" } else { "cold-start" };
            eprintln!(
                "{label}: {:.1} ms to marker ({} console bytes emitted)",
                elapsed.as_secs_f64() * 1000.0,
                console.bytes_out()
            );
        }
    }
    Ok(())
}

/// If `addr` falls in the virt-net MMIO window, returns the device and the register offset.
fn net_mmio_offset(net: &Option<NetDevice>, addr: u64) -> Option<(&NetDevice, u64)> {
    let nd: &NetDevice = net.as_ref()?;
    if (net::NET_MMIO_BASE..net::NET_MMIO_BASE + net::NET_MMIO_SIZE).contains(&addr) {
        Some((nd, addr - net::NET_MMIO_BASE))
    } else {
        None
    }
}

/// Takes a snapshot when the guest requests one. Returns `true` if the VM should stop.
fn take_snapshot(
    cfg: &Config,
    vm_fd: &::kvm_ioctls::VmFd,
    vcpu: &Vcpu,
    mem: &GuestMemory,
    bus: &DeviceBus,
    console: &Arc<Mutex<Console>>,
    net: Option<&NetDevice>,
) -> Result<bool> {
    let Some(dir) = &cfg.snapshot else {
        warn!("guest requested a snapshot but --snapshot was not given; ignoring");
        return Ok(false);
    };
    console.lock().expect("console poisoned").flush();
    let con_state: Vec<u8> = bus.console().lock().expect("console poisoned").snapshot();

    // With a NIC attached, hold its lock across the whole capture so the receive thread cannot
    // mutate guest RAM or the virtqueues while we dump them, and serialize its transport state
    // (endpoint header + device indices) alongside the rest.
    match (net, &cfg.net) {
        (Some(nd), Some(ncfg)) => {
            let dev = nd.dev.lock().expect("virt-net poisoned");
            let mut net_state: Vec<u8> = ncfg.save_header();
            net_state.extend(dev.save());
            snapshot::write(dir, &vcpu.fd, vm_fd, mem, &con_state, &net_state)
                .with_context(|| format!("writing snapshot to {dir:?}"))?;
        },
        _ => {
            snapshot::write(dir, &vcpu.fd, vm_fd, mem, &con_state, &[])
                .with_context(|| format!("writing snapshot to {dir:?}"))?;
        },
    }
    info!("snapshot written to {dir:?}");
    Ok(true)
}

/// No-op `SIGUSR1` handler used solely to interrupt `KVM_RUN`.
extern "C" fn sigusr1_handler(_signum: ::libc::c_int) {}

/// Dumps guest register state for diagnosing early boot faults.
fn dump_vcpu(vcpu: &Vcpu) {
    match (vcpu.fd.get_regs(), vcpu.fd.get_sregs()) {
        (Ok(regs), Ok(sregs)) => {
            error!(
                "  rip={:#018x} rsp={:#018x} rbp={:#018x} rflags={:#010x}",
                regs.rip, regs.rsp, regs.rbp, regs.rflags
            );
            error!(
                "  rax={:#018x} rbx={:#018x} rcx={:#018x} rdx={:#018x}",
                regs.rax, regs.rbx, regs.rcx, regs.rdx
            );
            error!(
                "  rsi={:#018x} rdi={:#018x}",
                regs.rsi, regs.rdi
            );
            error!(
                "  cr0={:#018x} cr3={:#018x} cr4={:#018x} efer={:#018x}",
                sregs.cr0, sregs.cr3, sregs.cr4, sregs.efer
            );
            error!(
                "  cs: sel={:#06x} base={:#x} limit={:#x} db={} l={} g={} type={:#x} present={}",
                sregs.cs.selector,
                sregs.cs.base,
                sregs.cs.limit,
                sregs.cs.db,
                sregs.cs.l,
                sregs.cs.g,
                sregs.cs.type_,
                sregs.cs.present
            );
        },
        _ => error!("  <failed to read vcpu registers>"),
    }
}

/// Installs the VMM's signal handlers:
///
/// - a no-op `SIGUSR1` handler (without `SA_RESTART`) so a console-input notification can
///   interrupt `KVM_RUN`; and
/// - `SIG_IGN` for the job-control stop signals `SIGTTIN`/`SIGTTOU`.
///
/// The latter matters whenever the VMM runs in a **background process group** — for example
/// under `timeout(1)` or inside a shell pipeline — while stdin/stdout is still the controlling
/// terminal. In that situation the input thread's `read` (`SIGTTIN`) and `TtyGuard`'s
/// `tcsetattr` (`SIGTTOU`) would, by default, stop the whole process (freezing the vCPU and
/// hanging the guest). Ignoring these signals makes those calls fail with `EIO` instead, so
/// the guest keeps running (it simply forgoes interactive console input, which such runs do
/// not use).
fn install_signal_handlers() {
    // SAFETY: We install a trivial, async-signal-safe (no-op) handler for SIGUSR1 and set
    // SIGTTIN/SIGTTOU to SIG_IGN; all operate on process-global signal dispositions.
    unsafe {
        let mut action: ::libc::sigaction = ::core::mem::zeroed();
        action.sa_sigaction = sigusr1_handler as *const () as usize;
        action.sa_flags = 0;
        ::libc::sigemptyset(&mut action.sa_mask);
        ::libc::sigaction(::libc::SIGUSR1, &action, ::core::ptr::null_mut());

        let mut ignore: ::libc::sigaction = ::core::mem::zeroed();
        ignore.sa_sigaction = ::libc::SIG_IGN;
        ignore.sa_flags = 0;
        ::libc::sigemptyset(&mut ignore.sa_mask);
        ::libc::sigaction(::libc::SIGTTIN, &ignore, ::core::ptr::null_mut());
        ::libc::sigaction(::libc::SIGTTOU, &ignore, ::core::ptr::null_mut());
    }
}

/// Spawns a thread that forwards host stdin to the guest console, waking the vCPU per input.
fn spawn_input_thread(con: Arc<Mutex<PortConsole>>, vcpu_tid: Arc<AtomicU64>) {
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 256];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    con.lock().expect("console poisoned").enqueue(&buf[..n]);
                    let tid: u64 = vcpu_tid.load(Ordering::SeqCst);
                    if tid != 0 {
                        // SAFETY: `tid` identifies the live vCPU thread; delivering SIGUSR1
                        // runs the installed no-op handler and interrupts KVM_RUN.
                        unsafe {
                            ::libc::pthread_kill(tid as ::libc::pthread_t, ::libc::SIGUSR1);
                        }
                    }
                },
            }
        }
    });
}

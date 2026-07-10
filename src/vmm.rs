// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The VM run-loop structure is derived from the KVM backend of the Nanvix Micro-VM
// (uservm), Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Virtual Machine Monitor
//!
//! Ties together guest memory, the interrupt controller, the vCPU(s), and the device bus, and
//! drives the boot-processor execution loop (plus, for `--num-cores > 1`, one thread per
//! application processor). Bytes typed on the host console are delivered to the guest console
//! device by a dedicated input thread that wakes the boot vCPU with `SIGUSR1`.
//!

use ::std::fs;
use ::std::io::{self, Read};
use ::std::path::{Path, PathBuf};
use ::std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ::std::sync::{Arc, Mutex};
use ::std::thread;
use ::std::thread::JoinHandle;
use ::std::time::{
    Duration,
    Instant,
};

use ::anyhow::{Context, Result, anyhow, bail};
use ::kvm_ioctls::{Cap, Kvm, VcpuExit};
use ::log::{debug, error, info, warn};

use crate::boot::pvh;
use crate::console::Console;
use crate::devices::portb::PortConsole;
use crate::devices::{DeviceBus, PioAction};
use crate::irq;
use crate::memory::GuestMemory;
use crate::net::{self, VirtioNet};
use crate::snapshot::{self, Snapshot};
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
                    return Self {
                        fd,
                        saved: Some(saved),
                    };
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
    /// Number of processor cores / vCPUs to create (`--num-cores`). 1 keeps the single-processor
    /// path. With N > 1 the VMM writes an Intel MP table and brings up N-1 application processors
    /// so the guest runs functional SMP.
    pub num_cores: usize,
}

/// A running virt-net NIC: the shared device model plus the TAP descriptor the receive thread
/// reads from. The backing [`HostTap`](crate::net::HostTap) is kept alive separately by the
/// caller for the VM's lifetime.
struct NetDevice {
    /// The virtio-net device, shared between the vCPU threads, the receive thread, and the
    /// transmit thread.
    dev: Arc<Mutex<VirtioNet>>,
    /// Raw TAP file descriptor, polled by the receive thread.
    tap_fd: ::std::os::fd::RawFd,
    /// The TX doorbell `ioeventfd`: KVM signals it when the guest kicks the transmit queue, and
    /// the transmit worker waits on it. Kept alive so the ioeventfd stays registered.
    tx_evt: Arc<::vmm_sys_util::eventfd::EventFd>,
    /// The serialized NIC configuration header (guest/host addressing and MAC). Captured here so
    /// a snapshot can re-emit it without consulting `--net`, which is absent on the restore path
    /// (so a snapshot taken from a restored VM still records its NIC).
    cfg_header: Vec<u8>,
}

/// VM-wide supervisor shared by every vCPU thread (the boot processor and all application
/// processors). Any vCPU that reaches a terminal condition — a guest shutdown/reset, a fatal KVM
/// exit, or a panic — records it here and kicks the other vCPUs so the whole VM stops together,
/// rather than leaving the guest running with fewer processors than it believes it has.
///
/// Only flat, cheap state lives here (an atomic stop flag, an optional fatal message, and the
/// registered vCPU thread ids); the caller keeps any richer bookkeeping to itself. This mirrors
/// cloud-hypervisor (`AtomicBool` flags + a supervisor that owns the reason) and OpenVMM
/// (`Arc<Halt>` woken by any VP).
struct VmControl {
    /// Set once any vCPU asks the VM to stop; polled by every vCPU loop.
    stop: AtomicBool,
    /// The first fatal error reported by any vCPU (a normal shutdown leaves this `None`).
    fatal: Mutex<Option<String>>,
    /// Registered vCPU thread ids, used to force-exit peers from `KVM_RUN` via `SIGRTMIN`.
    tids: Mutex<Vec<u64>>,
    /// Start gate: application-processor threads park until the boot processor releases them, so
    /// on restore no AP runs (and none can inject an IPI) until every vCPU has been created and
    /// its state applied.
    released: AtomicBool,
    /// Set once any vCPU observes a guest snapshot request; every vCPU then quiesces so the boot
    /// processor can take a consistent VM-wide cut.
    snapshot_requested: AtomicBool,
    /// Number of application processors that have quiesced (parked out of `KVM_RUN`) for a
    /// snapshot. The boot processor waits for this to reach the AP count before pausing devices.
    ap_paused: AtomicUsize,
    /// Released by the boot processor once all vCPUs are quiesced and the device threads are
    /// stopped; each application processor then captures its own state.
    capture_now: AtomicBool,
    /// Number of application processors that have captured their state into `saved`.
    ap_captured: AtomicUsize,
    /// Per-vCPU captured state (indexed by vCPU id), filled during a snapshot.
    saved: Mutex<Vec<Option<snapshot::VcpuState>>>,
    /// The number of vCPUs in this VM (the boot processor plus the application processors). Used
    /// by the snapshot coordinator instead of the CLI `--num-cores`, which is not meaningful on the
    /// restore path (the processor count comes from the snapshot there).
    vcpu_count: usize,
}

impl VmControl {
    fn new(vcpu_count: usize) -> Self {
        Self {
            stop: AtomicBool::new(false),
            fatal: Mutex::new(None),
            tids: Mutex::new(Vec::new()),
            released: AtomicBool::new(false),
            snapshot_requested: AtomicBool::new(false),
            ap_paused: AtomicUsize::new(0),
            capture_now: AtomicBool::new(false),
            ap_captured: AtomicUsize::new(0),
            saved: Mutex::new((0..vcpu_count).map(|_| None).collect()),
            vcpu_count,
        }
    }

    /// The number of vCPUs (boot processor plus application processors) in this VM.
    fn vcpu_count(&self) -> usize {
        self.vcpu_count
    }

    /// Registers the calling vCPU thread so peers can kick it.
    fn register(&self, tid: u64) {
        self.tids.lock().expect("vm control poisoned").push(tid);
    }

    /// Releases the application processors from their start gate.
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }

    /// Whether the application processors have been released from the start gate.
    fn is_released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }

    /// Whether any vCPU has requested the VM to stop.
    fn should_stop(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Requests a clean stop of the whole VM (guest shutdown/reset, boot marker, snapshot).
    fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Requests a stop and records a fatal error (kept only if it is the first one).
    fn request_fatal(&self, msg: String) {
        let mut fatal = self.fatal.lock().expect("vm control poisoned");
        if fatal.is_none() {
            *fatal = Some(msg);
        }
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Takes the recorded fatal error, if any.
    fn take_fatal(&self) -> Option<String> {
        self.fatal.lock().expect("vm control poisoned").take()
    }

    /// Signals that a guest snapshot has been requested (from any vCPU).
    fn request_snapshot(&self) {
        self.snapshot_requested.store(true, Ordering::SeqCst);
    }

    /// Whether a snapshot has been requested and no stop is pending.
    fn snapshot_pending(&self) -> bool {
        self.snapshot_requested.load(Ordering::SeqCst) && !self.should_stop()
    }

    /// Records an application processor's captured state at its vCPU index.
    fn store_saved(&self, index: usize, state: snapshot::VcpuState) {
        if let Some(slot) = self.saved.lock().expect("vm control poisoned").get_mut(index) {
            *slot = Some(state);
        }
    }

    /// Consumes every captured vCPU state in index order, failing if any is missing.
    fn take_saved(&self) -> Result<Vec<snapshot::VcpuState>> {
        let mut slots = self.saved.lock().expect("vm control poisoned");
        let mut out: Vec<snapshot::VcpuState> = Vec::with_capacity(slots.len());
        for (idx, slot) in slots.iter_mut().enumerate() {
            out.push(slot.take().with_context(|| format!("vCPU {idx} state was not captured"))?);
        }
        Ok(out)
    }

    /// Sends `SIGRTMIN` to every registered vCPU thread except `self_tid`, forcing them out of a
    /// blocking `KVM_RUN` so they observe the stop request. The stop flag must already be set.
    fn kick_others(&self, self_tid: u64) {
        for &tid in self.tids.lock().expect("vm control poisoned").iter() {
            if tid != 0 && tid != self_tid {
                // SAFETY: `tid` is a registered live vCPU thread id; the SIGRTMIN handler sets
                // `immediate_exit` and interrupts KVM_RUN.
                unsafe {
                    ::libc::pthread_kill(tid as ::libc::pthread_t, sigrtmin());
                }
            }
        }
    }
}

/// Owns every application-processor thread and guarantees that none can outlive the VM resources
/// it accesses. This guard also covers partial startup and unwinding paths.
struct ApThreads {
    control: Arc<VmControl>,
    threads: Vec<(u64, JoinHandle<()>, Arc<AtomicU64>)>,
}

impl ApThreads {
    fn new(control: Arc<VmControl>) -> Self {
        Self {
            control,
            threads: Vec::new(),
        }
    }

    fn push(&mut self, id: u64, handle: JoinHandle<()>, tid: Arc<AtomicU64>) {
        self.threads.push((id, handle, tid));
    }

    /// Requests a VM-wide stop, repeatedly wakes APs parked in `KVM_RUN`, and joins them.
    fn stop_and_join(&mut self) {
        self.control.request_stop();
        for (id, handle, tid) in self.threads.drain(..) {
            // Retry until the thread finishes, covering the startup race where it has not yet
            // published its pthread id. Stop signalling once it exits so the id cannot be reused.
            while !handle.is_finished() {
                let tid: u64 = tid.load(Ordering::SeqCst);
                if tid != 0 {
                    // SAFETY: while the handle is unfinished, `tid` identifies its AP thread;
                    // the installed kick handler interrupts a blocking KVM_RUN.
                    unsafe {
                        ::libc::pthread_kill(tid as ::libc::pthread_t, sigrtmin());
                    }
                }
                thread::sleep(::std::time::Duration::from_millis(2));
            }
            if handle.join().is_err() {
                self.control
                    .request_fatal(format!("AP{id} thread panicked during setup"));
            }
        }
    }
}

impl Drop for ApThreads {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// Services a guest MMIO read to the virt-net window from any vCPU. Reads outside the window float
/// to all-ones (matching an unoccupied bus).
fn net_mmio_read(net: Option<&Arc<Mutex<VirtioNet>>>, addr: u64, data: &mut [u8]) {
    let in_window = (net::NET_MMIO_BASE..net::NET_MMIO_BASE + net::NET_MMIO_SIZE).contains(&addr);
    match (net, in_window) {
        (Some(dev), true) => {
            let off = addr - net::NET_MMIO_BASE;
            dev.lock().expect("virt-net poisoned").mmio_read(off, data);
        }
        // Unclaimed MMIO reads float to 0xFF (matches cloud-hypervisor's sentinel).
        _ => data.iter_mut().for_each(|b| *b = 0xff),
    }
}

/// Services a guest MMIO write to the virt-net window from any vCPU. Writes outside the window are
/// dropped (matching an unoccupied bus).
fn net_mmio_write(net: Option<&Arc<Mutex<VirtioNet>>>, addr: u64, data: &[u8]) {
    let in_window = (net::NET_MMIO_BASE..net::NET_MMIO_BASE + net::NET_MMIO_SIZE).contains(&addr);
    if let (Some(dev), true) = (net, in_window) {
        let off = addr - net::NET_MMIO_BASE;
        dev.lock().expect("virt-net poisoned").mmio_write(off, data);
    }
}

/// Runs a tiny 32-bit self-test program through the same `setup_pvh` entry path to validate
/// the VMM's protected-mode setup independently of the kernel image.
pub fn selftest() -> Result<()> {
    let kvm: Kvm = Kvm::new().context("opening /dev/kvm")?;
    let vm_fd = kvm.create_vm().context("KVM_CREATE_VM failed")?;
    vm_fd
        .set_tss_address(TSS_ADDRESS)
        .context("KVM_SET_TSS_ADDR failed")?;
    let mem: GuestMemory = GuestMemory::new(&vm_fd, 16 << 20)?;
    let mut vcpu: Vcpu = Vcpu::new(&kvm, &vm_fd, 0, 1)?;

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
            }
            Ok(VcpuExit::Hlt) => {
                info!("selftest: guest halted as expected — VMM protected-mode setup OK");
                break;
            }
            Ok(other) => {
                error!("selftest: unexpected exit {other:?}");
                dump_vcpu(&vcpu);
                break;
            }
            Err(e) => {
                error!("selftest: KVM_RUN error: {e}");
                dump_vcpu(&vcpu);
                break;
            }
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

/// Adds the virtual TSC frequency unless the caller already supplied an override.
fn append_tsc_early_khz(cmdline: &mut String, tsc_khz: u32) {
    if cmdline
        .split_ascii_whitespace()
        .any(|arg| arg.starts_with("tsc_early_khz="))
    {
        return;
    }
    if cmdline
        .as_bytes()
        .last()
        .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        cmdline.push(' ');
    }
    cmdline.push_str(&format!("tsc_early_khz={tsc_khz}"));
}

/// Cold-boots a kernel + initramfs via the PVH protocol.
fn run_cold(cfg: Config) -> Result<()> {
    let kernel_path: &PathBuf = cfg.kernel.as_ref().context("--kernel is required")?;
    let kernel: Vec<u8> =
        fs::read(kernel_path).with_context(|| format!("reading kernel image {kernel_path:?}"))?;
    let initrd: Option<Vec<u8>> = match &cfg.initrd {
        Some(path) => Some(fs::read(path).with_context(|| format!("reading initramfs {path:?}"))?),
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
    let mut vcpu: Vcpu = Vcpu::new(&kvm, &vm_fd, 0, cfg.num_cores)?;

    // Load the kernel, the initramfs, and the PVH boot structures.
    let loaded = pvh::load_kernel(&mem, &kernel)?;
    info!(
        "loaded kernel: pvh_entry={:#x}, kernel_end={:#x}",
        loaded.pvh_entry, loaded.kernel_end
    );
    let initrd_region = match &initrd {
        Some(bytes) => {
            let region = pvh::load_initramfs(&mem, bytes, loaded.kernel_end, ram_size)?;
            info!(
                "loaded initramfs: addr={:#x}, size={:#x}",
                region.addr, region.size
            );
            Some(region)
        }
        None => None,
    };
    // Optionally export a host directory to the guest as a virt-fs. The filesystem image is
    // mapped into guest memory above reported RAM and pointed at via the kernel command line;
    // `_virtfs` owns that mapping (and, for a persistent read-write export, flushes it) and must
    // stay alive until the guest stops.
    let mut cmdline: String = cfg.cmdline.clone();
    // Linux discovers the TSC frequency before kvm-clock is initialized. Give it KVM's actual
    // virtual rate so it never has to rely on timing-sensitive PIT/delay-loop calibration.
    if kvm.check_extension(Cap::GetTscKhz) {
        let tsc_khz: u32 = vcpu
            .fd
            .get_tsc_khz()
            .context("KVM_GET_TSC_KHZ failed")?;
        if tsc_khz == 0 {
            warn!("KVM reported a zero TSC frequency; guest will use timer calibration");
        } else {
            append_tsc_early_khz(&mut cmdline, tsc_khz);
        }
    } else {
        warn!("KVM_GET_TSC_KHZ is unavailable; guest will use timer calibration");
    }
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
        }
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
    // Share the device bus with the application-processor threads: the guest's hvc0 console
    // kthread can run on any CPU, so console (port `0xE9`) writes may be issued from an AP and
    // must reach the same console device the boot processor uses.
    let bus: Arc<DeviceBus> = Arc::new(bus);

    // Bring up the virt-net NIC: register its irqfd and TX ioeventfd, create/configure the host
    // TAP, and build the shared device model. `_net_tap` owns the TAP interface (and tears it down
    // on drop) and must outlive the guest and the receive/transmit threads.
    let (net_dev, _net_tap): (Option<NetDevice>, Option<net::HostTap>) = match &cfg.net {
        Some(ncfg) => {
            let irq = net::register_irq(&vm_fd)?;
            let tx_evt = net::register_tx_ioeventfd(&vm_fd)?;
            let tap = net::HostTap::for_config(ncfg, cfg.net_tap.as_deref())?;
            let dev = Arc::new(Mutex::new(VirtioNet::new(
                mem.ram(),
                tap.raw_fd(),
                irq,
                ncfg.mac,
            )));
            let tap_fd = tap.raw_fd();
            let cfg_header = ncfg.save_header();
            (Some(NetDevice { dev, tap_fd, tx_evt, cfg_header }), Some(tap))
        },
        None => (None, None),
    };

    info!(
        "starting guest (mem={} MiB, cmdline={:?})",
        ram_size >> 20,
        cmdline
    );

    // Functional multi-core SMP (see `--num-cores`). Emit an Intel MP table so the guest kernel
    // enumerates every vCPU, then create the application processors (APs) and run each on its own
    // host thread. Each AP is left in KVM's default `KVM_MP_STATE_UNINITIALIZED` state: its
    // `KVM_RUN` blocks until the boot processor's guest kernel issues INIT-SIPI-SIPI, which the
    // in-kernel KVM LAPIC (created before any vCPU) services, waking the AP at the SIPI vector to
    // run the kernel's secondary-CPU trampoline. Only cold boot brings up APs; `--restore` resumes
    // a single processor.
    if cfg.num_cores > 1 {
        crate::boot::mptable::write(&mem, cfg.num_cores as u8)
            .context("writing Intel MP table for SMP")?;
    }

    // The VM supervisor, shared by the boot processor and every application processor, so any
    // vCPU that stops (guest shutdown/reset, fatal exit, panic) stops the whole VM.
    let control: Arc<VmControl> = Arc::new(VmControl::new(cfg.num_cores));
    // Install the kick/console signal handlers before spawning any application-processor thread,
    // so an AP can never receive SIGRTMIN (whose default disposition would kill the process)
    // before its handler exists.
    install_signal_handlers();
    // The virt-net device, shared so any vCPU can service its MMIO window (the guest's drivers can
    // touch it from any CPU). The boot processor keeps `net_dev` (which also owns the RX thread).
    let net_shared: Option<Arc<Mutex<VirtioNet>>> = net_dev.as_ref().map(|nd| Arc::clone(&nd.dev));

    let mut ap_threads: ApThreads = ApThreads::new(Arc::clone(&control));
    for id in 1..cfg.num_cores as u64 {
        let ap: Vcpu = Vcpu::new(&kvm, &vm_fd, id, cfg.num_cores)
            .with_context(|| format!("creating application-processor vcpu {id}"))?;
        let tid: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
        let ap_bus: Arc<DeviceBus> = Arc::clone(&bus);
        let ap_console: Arc<Mutex<Console>> = Arc::clone(&console);
        let ap_net: Option<Arc<Mutex<VirtioNet>>> = net_shared.clone();
        let ap_control: Arc<VmControl> = Arc::clone(&control);
        let exit_on_boot: bool = cfg.exit_on_boot;
        let snapshot_enabled: bool = cfg.snapshot.is_some();
        let ap_tid: Arc<AtomicU64> = Arc::clone(&tid);
        let handle: JoinHandle<()> = thread::Builder::new()
            .name(format!("vcpu-{id}"))
            .spawn(move || {
                run_ap(
                    ap,
                    id,
                    ap_bus,
                    ap_net,
                    ap_console,
                    exit_on_boot,
                    snapshot_enabled,
                    ap_control,
                    ap_tid,
                )
            })
            .with_context(|| format!("spawning application-processor vcpu {id}"))?;
        ap_threads.push(id, handle, tid);
    }
    if cfg.num_cores > 1 {
        info!(
            "SMP: created {} vCPUs ({} application processor(s) on their own threads)",
            cfg.num_cores,
            cfg.num_cores - 1
        );
    }

    // Release the application processors: on cold boot they immediately block in `KVM_RUN`
    // waiting for the guest's INIT-SIPI-SIPI, so the gate only orders them after this point.
    control.release();

    let result = execute(
        &cfg,
        &vm_fd,
        &mut vcpu,
        &mem,
        &console,
        bus.as_ref(),
        false,
        net_dev,
        Arc::clone(&control),
    );

    // The boot processor has stopped; force APs parked in `KVM_RUN` to observe that stop before
    // any guest memory or VM resources can be released.
    ap_threads.stop_and_join();

    // Surface a fatal condition reported by any vCPU (the boot processor's own error already
    // flows through `result`); a clean guest shutdown/reset leaves no fatal recorded.
    match (result, control.take_fatal()) {
        (Err(e), _) => Err(e),
        (Ok(()), Some(msg)) => Err(anyhow!("vcpu fatal: {msg}")),
        (Ok(()), None) => Ok(()),
    }
}

/// Drives an application-processor vCPU for functional SMP. The AP is created in KVM's default
/// `KVM_MP_STATE_UNINITIALIZED` state, so its first `KVM_RUN` blocks until the guest boot processor
/// sends INIT-SIPI-SIPI (serviced by the in-kernel LAPIC), which wakes it at the SIPI vector to run
/// the kernel's secondary-CPU trampoline. The thread then loops running guest code until the VM
/// supervisor asks it to stop.
///
/// Both port I/O and MMIO are forwarded to the shared device bus / virt-net window: the guest can
/// touch any device from any CPU (e.g. the hvc0 console kthread, which backs the `--exit-on-boot`
/// marker, may run on an AP). A terminal exit (guest shutdown/reset, or a fatal KVM exit) is
/// reported to the supervisor, which stops the whole VM.
#[allow(clippy::too_many_arguments)]
fn run_ap(
    mut ap: Vcpu,
    idx: u64,
    bus: Arc<DeviceBus>,
    net: Option<Arc<Mutex<VirtioNet>>>,
    console: Arc<Mutex<Console>>,
    exit_on_boot: bool,
    snapshot_enabled: bool,
    control: Arc<VmControl>,
    tid: Arc<AtomicU64>,
) {
    // SAFETY: `pthread_self` merely returns the calling thread's identifier.
    let my_tid: u64 = unsafe { ::libc::pthread_self() } as u64;
    tid.store(my_tid, Ordering::SeqCst);
    control.register(my_tid);
    let _kick_guard: KickGuard = KickGuard::arm(&mut ap);

    // Run the guest loop under a panic guard: an application-processor thread that unwinds must
    // still stop the whole VM (and let the boot processor surface a fatal error) rather than
    // silently vanish, leaving the guest running with fewer processors than it believes it has.
    let loop_control: Arc<VmControl> = Arc::clone(&control);
    let panicked: bool = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
        let control = loop_control;
        // Requests a whole-VM stop, wakes the other vCPUs, and reports an optional fatal error.
        let stop_vm = |fatal: Option<String>| {
            match fatal {
                Some(msg) => control.request_fatal(msg),
                None => control.request_stop(),
            }
            control.kick_others(my_tid);
        };

        // Wait at the start gate before running any guest code: on restore, no AP runs (and so
        // none can inject an IPI into a peer) until every vCPU has been created and restored and
        // the boot processor releases them together.
        while !control.is_released() {
            if control.should_stop() {
                return;
            }
            thread::sleep(Duration::from_micros(200));
        }

        loop {
            // Clear any pending immediate-exit, then re-check the stop flag before re-entering the
            // guest; a kick delivered after this point makes the next KVM_RUN return at once.
            ap.fd.set_kvm_immediate_exit(0);
            if control.should_stop() {
                break;
            }
            // A snapshot has been requested: quiesce and capture this processor's state, then park
            // until the boot processor completes the cut. This AP is out of `KVM_RUN` here, so its
            // register state is a consistent point in the VM-wide cut.
            if control.snapshot_pending() {
                if let Err(e) = ap_snapshot(&mut ap, idx, &control) {
                    stop_vm(Some(format!("AP{idx} snapshot capture failed: {e}")));
                }
                break;
            }
            match ap.fd.run() {
                Ok(VcpuExit::IoIn(port, data)) => bus.pio_read(port, data),
                Ok(VcpuExit::IoOut(port, data)) => match bus.pio_write(port, data) {
                    PioAction::None => {},
                    PioAction::Shutdown => {
                        stop_vm(None);
                        break;
                    },
                    // The snapshot can be triggered from any CPU. Announce it and kick the other
                    // vCPUs; the boot processor coordinates the actual capture.
                    PioAction::Snapshot => {
                        if snapshot_enabled {
                            control.request_snapshot();
                            control.kick_others(my_tid);
                        }
                    },
                },
                Ok(VcpuExit::MmioRead(addr, data)) => net_mmio_read(net.as_ref(), addr, data),
                Ok(VcpuExit::MmioWrite(addr, data)) => net_mmio_write(net.as_ref(), addr, data),
                // Linux APs HLT in the idle loop; with the in-kernel LAPIC, KVM re-blocks until the
                // next interrupt, so looping here does not busy-spin.
                Ok(VcpuExit::Hlt) => {}
                Ok(VcpuExit::Shutdown) => {
                    stop_vm(None);
                    break;
                }
                Ok(VcpuExit::FailEntry(reason, cpu)) => {
                    stop_vm(Some(format!(
                        "AP{idx} fail entry (reason={reason:#x}, cpu={cpu})"
                    )));
                    break;
                }
                Ok(VcpuExit::InternalError) => {
                    stop_vm(Some(format!("AP{idx} KVM internal error")));
                    break;
                }
                Ok(other) => debug!("AP{idx} unhandled vcpu exit: {other:?}"),
                // Interrupted by the stop-kick (or a spurious signal): re-check `stop` and re-enter.
                Err(e) if e.errno() == ::libc::EINTR => {}
                // The UNINITIALIZED -> INIT_RECEIVED transition surfaces as EAGAIN once; retry.
                Err(e) if e.errno() == ::libc::EAGAIN => {}
                Err(e) => {
                    stop_vm(Some(format!("AP{idx} KVM_RUN failed: {e}")));
                    break;
                }
            }
            // The boot marker may be emitted from this AP (the hvc0 console kthread can run on any
            // CPU). If so, stop the VM so the boot processor's `--exit-on-boot` fires even if it is
            // idle in `KVM_RUN`.
            if exit_on_boot && console.lock().expect("console poisoned").booted() {
                stop_vm(None);
                break;
            }
        }
    }))
    .is_err();

    if panicked {
        control.request_fatal(format!("AP{idx} panicked"));
        control.kick_others(my_tid);
    }
    // Clear the armed kick pointer before `ap` (and its kvm_run mmap) is dropped.
    disarm_kick();
}

/// Quiesces an application processor for a snapshot and captures its architectural state.
///
/// The vCPU is already out of `KVM_RUN` when this is called. It announces that it has paused,
/// waits for the boot processor to quiesce every vCPU and stop the device threads (`capture_now`),
/// completes any pending I/O so its register state is self-consistent, captures its own state, and
/// then parks until the whole-VM stop signals that the cut is complete. Returns early without
/// capturing if the snapshot is aborted.
fn ap_snapshot(ap: &mut Vcpu, idx: u64, control: &VmControl) -> Result<()> {
    control.ap_paused.fetch_add(1, Ordering::SeqCst);
    while !control.capture_now.load(Ordering::SeqCst) {
        if control.should_stop() {
            return Ok(());
        }
        thread::sleep(Duration::from_micros(200));
    }
    ap.drain_pending_io().with_context(|| format!("draining AP{idx} I/O for snapshot"))?;
    let state: snapshot::VcpuState = snapshot::VcpuState::capture(&ap.fd, idx)
        .with_context(|| format!("capturing AP{idx} state"))?;
    control.store_saved(idx as usize, state);
    control.ap_captured.fetch_add(1, Ordering::SeqCst);
    while !control.should_stop() {
        thread::sleep(Duration::from_micros(500));
    }
    Ok(())
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

    // Copy-on-write map the saved RAM image and recreate the irqchip/PIT.
    let n_vcpus: usize = snap.vcpu_count();
    let mem: GuestMemory = GuestMemory::restore(&vm_fd, &dir.join("mem.bin"), snap.ram_size())?;
    irq::setup(&vm_fd)?;

    // Restore barrier: create *every* vCPU and reload its state before any of them runs. An
    // application processor started early could otherwise send an IPI to a not-yet-created peer
    // (KVM would drop it) or raise a kick before its handler exists. Each vCPU's CPUID topology
    // is reprogrammed by `Vcpu::new`; the MP table already lives in the restored guest RAM.
    let mut vcpus: Vec<Vcpu> = Vec::with_capacity(n_vcpus);
    for id in 0..n_vcpus {
        vcpus.push(
            Vcpu::new(&kvm, &vm_fd, id as u64, n_vcpus)
                .with_context(|| format!("creating vcpu {id} for restore"))?,
        );
    }
    snap.apply_vm(&vm_fd)?;
    for (id, v) in vcpus.iter().enumerate() {
        snap.apply_vcpu(&v.fd, id).with_context(|| format!("restoring vcpu {id}"))?;
    }
    // Synchronize the timestamp counter across all processors (write one reference TSC to each)
    // so a task migrating between them never sees time move backwards, then arm each processor's
    // TSC-deadline timer against the now-correct TSC (doing it earlier would schedule the deadline
    // against a near-zero TSC and delay the first timer by the guest's pre-snapshot uptime).
    let fds: Vec<&::kvm_ioctls::VcpuFd> = vcpus.iter().map(|v| &v.fd).collect();
    snap.sync_tsc(&fds)?;
    snap.arm_tsc_deadlines(&fds)?;
    drop(fds);

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
    let bus: Arc<DeviceBus> = Arc::new(bus);
    info!(
        "resuming guest from snapshot {dir:?} (mem={} MiB, {n_vcpus} vCPU(s))",
        snap.ram_size() >> 20
    );
    // Install the signal handlers before spawning any application processor.
    install_signal_handlers();
    let control: Arc<VmControl> = Arc::new(VmControl::new(n_vcpus));
    let net_shared: Option<Arc<Mutex<VirtioNet>>> =
        net_dev.as_ref().map(|nd| Arc::clone(&nd.dev));

    // The boot processor is index 0; the rest run as application processors, parked at the start
    // gate until every vCPU is created and restored and the boot processor releases them together.
    let mut bsp: Vcpu = vcpus.remove(0);
    let mut ap_threads: ApThreads = ApThreads::new(Arc::clone(&control));
    for (offset, ap) in vcpus.into_iter().enumerate() {
        let id: u64 = (offset + 1) as u64;
        let tid: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
        let ap_bus: Arc<DeviceBus> = Arc::clone(&bus);
        let ap_console: Arc<Mutex<Console>> = Arc::clone(&console);
        let ap_net: Option<Arc<Mutex<VirtioNet>>> = net_shared.clone();
        let ap_control: Arc<VmControl> = Arc::clone(&control);
        let exit_on_boot: bool = cfg.exit_on_boot;
        let snapshot_enabled: bool = cfg.snapshot.is_some();
        let ap_tid: Arc<AtomicU64> = Arc::clone(&tid);
        let handle: JoinHandle<()> = thread::Builder::new()
            .name(format!("vcpu-{id}"))
            .spawn(move || {
                run_ap(
                    ap,
                    id,
                    ap_bus,
                    ap_net,
                    ap_console,
                    exit_on_boot,
                    snapshot_enabled,
                    ap_control,
                    ap_tid,
                )
            })
            .with_context(|| format!("spawning application-processor vcpu {id}"))?;
        ap_threads.push(id, handle, tid);
    }
    if n_vcpus > 1 {
        info!("SMP restore: {n_vcpus} vCPUs ({} application processor(s))", n_vcpus - 1);
    }

    // Release the application processors and run the boot processor.
    control.release();
    let result = execute(
        &cfg,
        &vm_fd,
        &mut bsp,
        &mem,
        &console,
        bus.as_ref(),
        true,
        net_dev,
        Arc::clone(&control),
    );

    // Stop the VM and force any application processor still in `KVM_RUN` to exit, then join.
    ap_threads.stop_and_join();

    match (result, control.take_fatal()) {
        (Err(e), _) => Err(e),
        (Ok(()), Some(msg)) => Err(anyhow!("vcpu fatal: {msg}")),
        (Ok(()), None) => Ok(()),
    }
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
    let tx_evt = net::register_tx_ioeventfd(vm_fd)?;
    let tap = net::HostTap::for_config(&ncfg, net_tap)?;
    let tap_fd = tap.raw_fd();
    let mut dev = VirtioNet::new(mem.ram(), tap_fd, irq, ncfg.mac);
    dev.load(&net_state[consumed..])?;
    dev.resume();
    info!(
        "virt-net: NIC restored (guest {}/{})",
        ncfg.guest_ip, ncfg.prefix
    );
    // Preserve the config header so a snapshot taken from this restored VM re-records the NIC.
    let cfg_header = ncfg.save_header();
    let dev = Arc::new(Mutex::new(dev));
    Ok(Some((NetDevice { dev, tap_fd, tx_evt, cfg_header }, tap)))
}

/// Builds the shared console sink, the portb console device, and the device bus. When
/// `con_state` is provided (restore path) the device's pending input queue is reloaded from it.
fn build_io(cfg: &Config, con_state: Option<&[u8]>) -> (Arc<Mutex<Console>>, DeviceBus) {
    let console: Arc<Mutex<Console>> =
        Arc::new(Mutex::new(Console::new(cfg.quiet, &cfg.boot_marker)));
    let con: Arc<Mutex<PortConsole>> = Arc::new(Mutex::new(PortConsole::new(Arc::clone(&console))));
    if let Some(state) = con_state {
        con.lock().expect("console poisoned").restore(state);
    }
    let bus: DeviceBus = DeviceBus::new(Arc::clone(&con));
    (console, bus)
}

/// Drives the boot-processor execution loop shared by the cold-boot and restore paths.
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
    control: Arc<VmControl>,
) -> Result<()> {
    let _tty_guard: TtyGuard = TtyGuard::new();
    // The kick handler must already be installed by the caller (before any AP thread is spawned).
    let _kick_guard: KickGuard = KickGuard::arm(vcpu);
    let vcpu_tid: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    spawn_input_thread(bus.console(), Arc::clone(&vcpu_tid));
    // SAFETY: `pthread_self` merely returns the calling thread's identifier.
    let self_tid: u64 = unsafe { ::libc::pthread_self() } as u64;
    vcpu_tid.store(self_tid, Ordering::SeqCst);
    control.register(self_tid);

    // Start the virt-net receive and transmit threads, if a NIC is attached. The receive thread
    // feeds host frames into the guest; the transmit thread drains the TX queue when the guest
    // rings the doorbell (delivered via the TX ioeventfd). Both are joined on shutdown, before
    // guest memory is released.
    let net_stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let mut net_rx: Option<JoinHandle<()>> = net.as_ref().map(|nd| {
        net::spawn_rx_thread(Arc::clone(&nd.dev), nd.tap_fd, Arc::clone(&net_stop))
    });
    let mut net_tx: Option<JoinHandle<()>> = net.as_ref().map(|nd| {
        net::spawn_tx_thread(Arc::clone(&nd.dev), Arc::clone(&nd.tx_evt), Arc::clone(&net_stop))
    });

    // Whether the guest's snapshot control-port write should be honored (only when a snapshot
    // destination was given). A stray request without `--snapshot` is warned about and ignored.
    let snapshot_enabled: bool = cfg.snapshot.is_some();

    console.lock().expect("console poisoned").mark_start();

    // Deferred KVM_RUN error: stored so the receive thread is still joined on the way out.
    let mut run_err: Option<::anyhow::Error> = None;

    loop {
        // Clear any pending immediate-exit before re-checking the stop conditions, then re-enter
        // the guest. A kick delivered after this point sets it again, so the next KVM_RUN returns
        // at once rather than blocking (closing the check-then-run lost-wakeup race).
        vcpu.fd.set_kvm_immediate_exit(0);
        // Stop if any vCPU (this one or an application processor) has asked the VM to stop, or if
        // the boot marker has appeared (possibly emitted from an AP while this thread is between
        // KVM_RUN calls, so check here too, not only after an exit).
        if control.should_stop() {
            break;
        }
        // A snapshot has been requested (by this vCPU or an application processor): the boot
        // processor coordinates a consistent VM-wide cut, then the VM stops.
        if control.snapshot_pending() {
            match coordinate_snapshot(
                cfg, vm_fd, vcpu, mem, bus, console, net.as_ref(), &control, &net_stop,
                &mut net_rx, &mut net_tx,
            ) {
                Ok(()) => info!("snapshot written to {:?}", cfg.snapshot),
                Err(e) => run_err = Some(e),
            }
            control.request_stop();
            control.kick_others(self_tid);
            break;
        }
        if cfg.exit_on_boot && console.lock().expect("console poisoned").booted() {
            info!("boot marker seen — stopping guest (--exit-on-boot)");
            control.request_stop();
            control.kick_others(self_tid);
            break;
        }

        // Flush buffered console output before re-entering the guest. The portb console is
        // interrupt-less (the guest's hvc driver polls), so there is no IRQ line to service.
        console.lock().expect("console poisoned").flush();

        match vcpu.fd.run() {
            Ok(VcpuExit::IoIn(port, data)) => bus.pio_read(port, data),
            Ok(VcpuExit::IoOut(port, data)) => match bus.pio_write(port, data) {
                PioAction::None => {}
                PioAction::Shutdown => {
                    info!("guest requested shutdown");
                    control.request_stop();
                    control.kick_others(self_tid);
                    break;
                }
                PioAction::Snapshot => {
                    if snapshot_enabled {
                        // Announce the request and force the application processors out of
                        // KVM_RUN (a HLTed AP in tickless idle would otherwise never observe the
                        // flag). The coordination runs at the loop top so both boot-processor- and
                        // AP-initiated snapshots follow one path.
                        control.request_snapshot();
                        control.kick_others(self_tid);
                    } else {
                        warn!(
                            "guest requested a snapshot but --snapshot was not given; ignoring"
                        );
                    }
                }
            },
            // Guest MMIO: the virt-net window is serviced from any vCPU; other reads float to
            // all-ones and writes are dropped, matching the unoccupied PMIO bus.
            Ok(VcpuExit::MmioRead(addr, data)) => {
                net_mmio_read(net.as_ref().map(|nd| &nd.dev), addr, data)
            }
            Ok(VcpuExit::MmioWrite(addr, data)) => {
                net_mmio_write(net.as_ref().map(|nd| &nd.dev), addr, data)
            }
            Ok(VcpuExit::Hlt) => {
                info!("guest halted");
                control.request_stop();
                control.kick_others(self_tid);
                break;
            }
            Ok(VcpuExit::Shutdown) => {
                // A PVH/no-ACPI guest reboots via triple fault, which surfaces here. Treat
                // it as a normal termination of the VM.
                info!("guest reset (reboot/halt)");
                control.request_stop();
                control.kick_others(self_tid);
                break;
            }
            Ok(VcpuExit::InternalError) => {
                error!("KVM internal error");
                dump_vcpu(vcpu);
                run_err = Some(anyhow!("KVM internal error"));
                control.request_stop();
                control.kick_others(self_tid);
                break;
            }
            Ok(VcpuExit::FailEntry(reason, cpu)) => {
                error!("KVM fail entry (reason={reason:#x}, cpu={cpu})");
                dump_vcpu(vcpu);
                run_err = Some(anyhow!("KVM fail entry (reason={reason:#x}, cpu={cpu})"));
                control.request_stop();
                control.kick_others(self_tid);
                break;
            }
            Ok(other) => debug!("unhandled vcpu exit: {other:?}"),
            // A host-thread signal (console input, or the supervisor's stop kick) interrupted
            // KVM_RUN: loop to re-check the stop/boot conditions and re-enter the guest.
            Err(e) if e.errno() == ::libc::EINTR => {}
            Err(e) => {
                run_err = Some(anyhow!("KVM_RUN failed: {e}"));
                control.request_stop();
                control.kick_others(self_tid);
                break;
            }
        }

        if cfg.exit_on_boot && console.lock().expect("console poisoned").booted() {
            info!("boot marker seen — stopping guest (--exit-on-boot)");
            control.request_stop();
            control.kick_others(self_tid);
            break;
        }
    }

    // Stop and join the receive and transmit threads before guest memory (which they DMA into or
    // out of) is dropped.
    net_stop.store(true, Ordering::SeqCst);
    if let Some(handle) = net_rx {
        let _ = handle.join();
    }
    if let Some(handle) = net_tx {
        let _ = handle.join();
    }

    console.lock().expect("console poisoned").flush();

    // Clear this thread's armed kick pointer before returning (the caller drops the vCPU and its
    // kvm_run mmap once execution ends).
    disarm_kick();

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

/// Coordinates a consistent, VM-wide snapshot from the boot processor.
///
/// By the time this runs the boot processor is already out of `KVM_RUN`. It waits for every
/// application processor to quiesce (each parks in [`ap_snapshot`] once it observes the request),
/// then stops and joins the device threads so nothing can inject an interrupt or DMA into guest
/// RAM, and only *then* reads any state: the boot processor captures its own registers, releases
/// the APs to capture theirs, collects all per-vCPU states in index order, and writes the
/// snapshot (guest RAM, the paravirtual clock, the interrupt controller/PIT, and the console/NIC
/// device state). The waits are bounded and abort if any vCPU stops, so a failed processor can
/// never wedge the writer.
#[allow(clippy::too_many_arguments)]
fn coordinate_snapshot(
    cfg: &Config,
    vm_fd: &::kvm_ioctls::VmFd,
    vcpu: &mut Vcpu,
    mem: &GuestMemory,
    bus: &DeviceBus,
    console: &Arc<Mutex<Console>>,
    net: Option<&NetDevice>,
    control: &VmControl,
    net_stop: &Arc<AtomicBool>,
    net_rx: &mut Option<JoinHandle<()>>,
    net_tx: &mut Option<JoinHandle<()>>,
) -> Result<()> {
    // The processor count comes from the supervisor, not the CLI `--num-cores`: on the restore path
    // the latter is not meaningful (the count was recovered from the snapshot).
    let n_aps: usize = control.vcpu_count().saturating_sub(1);

    // 1. Wait until every application processor has quiesced out of KVM_RUN. No vCPU is running
    //    the guest past this point, so none can inject an IPI into a peer's LAPIC.
    wait_bounded(|| control.ap_paused.load(Ordering::SeqCst) >= n_aps, control, "vCPUs to quiesce")?;

    // 2. Pause the devices: stop and join the receive/transmit threads so neither can raise an
    //    interrupt or touch guest RAM while the interrupt controller and memory are captured.
    net_stop.store(true, Ordering::SeqCst);
    if let Some(handle) = net_rx.take() {
        let _ = handle.join();
    }
    if let Some(handle) = net_tx.take() {
        let _ = handle.join();
    }

    // 3. Complete the boot processor's pending I/O (the OUT that triggered the snapshot) so its
    //    register state is past that instruction, then capture it (index 0).
    vcpu.drain_pending_io().context("draining boot-processor I/O for snapshot")?;
    let bsp: snapshot::VcpuState =
        snapshot::VcpuState::capture(&vcpu.fd, 0).context("capturing boot-processor state")?;
    control.store_saved(0, bsp);

    // 4. The cut is now fixed; release the application processors to capture their own state.
    control.capture_now.store(true, Ordering::SeqCst);
    wait_bounded(
        || control.ap_captured.load(Ordering::SeqCst) >= n_aps,
        control,
        "vCPUs to capture state",
    )?;

    // 5. Collect every processor's state in index order and write the snapshot.
    let states: Vec<snapshot::VcpuState> = control.take_saved()?;
    write_snapshot(cfg, vm_fd, mem, bus, console, net, &states)
}

/// Polls `done` until it is true, aborting if any vCPU requests a stop (a failed peer) or a
/// generous deadline elapses, so the snapshot coordinator can never block forever.
fn wait_bounded(
    mut done: impl FnMut() -> bool,
    control: &VmControl,
    what: &str,
) -> Result<()> {
    let deadline: Instant = Instant::now() + Duration::from_secs(10);
    while !done() {
        if control.should_stop() {
            bail!("snapshot aborted while waiting for {what} (a vCPU stopped)");
        }
        if Instant::now() >= deadline {
            bail!("snapshot timed out waiting for {what}");
        }
        thread::sleep(Duration::from_micros(200));
    }
    Ok(())
}

/// Serializes the device state and writes the snapshot to `--snapshot`. Called only after every
/// vCPU is quiesced and the device threads are stopped, so the capture is consistent.
fn write_snapshot(
    cfg: &Config,
    vm_fd: &::kvm_ioctls::VmFd,
    mem: &GuestMemory,
    bus: &DeviceBus,
    console: &Arc<Mutex<Console>>,
    net: Option<&NetDevice>,
    states: &[snapshot::VcpuState],
) -> Result<()> {
    let dir = cfg.snapshot.as_ref().context("--snapshot destination missing")?;
    console.lock().expect("console poisoned").flush();
    let con_state: Vec<u8> = bus.console().lock().expect("console poisoned").snapshot();

    // Serialize the NIC's config header (from the device itself, so it is present on the restore
    // path too) followed by its transport state.
    let net_state: Vec<u8> = match net {
        Some(nd) => {
            let dev = nd.dev.lock().expect("virt-net poisoned");
            let mut s: Vec<u8> = nd.cfg_header.clone();
            s.extend(dev.save());
            s
        },
        None => Vec::new(),
    };
    snapshot::write(dir, states, vm_fd, mem, &con_state, &net_state)
        .with_context(|| format!("writing snapshot to {dir:?}"))?;
    Ok(())
}

thread_local! {
    /// Raw pointer to the calling vCPU thread's `kvm_run.immediate_exit` byte, armed by
    /// [`arm_kick`] before the thread enters its run loop. The kick handler writes it so a
    /// `SIGRTMIN`/`SIGUSR1` that races the stop-check still forces the *next* `KVM_RUN` to return
    /// at once instead of blocking (OpenVMM / rust-vmm's lost-wakeup fix). Null on non-vCPU
    /// threads, where the handler is a no-op.
    static KICK_IMMEDIATE_EXIT: ::std::cell::Cell<*mut u8> =
        const { ::std::cell::Cell::new(::core::ptr::null_mut()) };
}

/// Keeps the calling vCPU thread's kick pointer armed only while its `VcpuFd` mapping is live.
struct KickGuard;

impl KickGuard {
    /// Arms the signal handler so it can force the next `KVM_RUN` to return immediately. The
    /// thread-local is initialized here so later handler reads are just a load.
    fn arm(vcpu: &mut Vcpu) -> Self {
        let ptr: *mut u8 = &mut vcpu.fd.get_kvm_run().immediate_exit as *mut u8;
        KICK_IMMEDIATE_EXIT.with(|cell| cell.set(ptr));
        Self
    }
}

impl Drop for KickGuard {
    fn drop(&mut self) {
        // Clear the pointer before the owning `VcpuFd` can be dropped. This also makes late
        // console-input signals harmless after the run loop has returned.
        KICK_IMMEDIATE_EXIT.with(|cell| cell.set(::core::ptr::null_mut()));
    }
}

/// Clears the calling vCPU thread's armed kick pointer. Called when the thread leaves its run
/// loop, before its `VcpuFd` (and the `kvm_run` mmap the pointer targets) is dropped, so a late
/// teardown `SIGRTMIN` can never write through a stale pointer to unmapped memory.
fn disarm_kick() {
    KICK_IMMEDIATE_EXIT.with(|c| c.set(::core::ptr::null_mut()));
}

/// Signal handler for the `SIGUSR1` console-input wake and the `SIGRTMIN` vCPU kick. It sets the
/// calling thread's `kvm_run.immediate_exit` so a `KVM_RUN` entered concurrently returns without
/// blocking, and also interrupts an already-blocked `KVM_RUN` with `EINTR`.
extern "C" fn kick_handler(_signum: ::libc::c_int) {
    let ptr: *mut u8 = KICK_IMMEDIATE_EXIT.with(|c| c.get());
    if !ptr.is_null() {
        // SAFETY: `ptr` addresses this thread's live `kvm_run.immediate_exit` byte in the KVM
        // mmap (kept alive for the thread's lifetime by its owning `VcpuFd`); a single volatile
        // byte write is async-signal-safe.
        unsafe { ptr.write_volatile(1) };
    }
}

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
            error!("  rsi={:#018x} rdi={:#018x}", regs.rsi, regs.rdi);
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
        }
        _ => error!("  <failed to read vcpu registers>"),
    }
}

/// Installs the VMM's signal handlers:
///
/// - a `SIGUSR1`/`SIGRTMIN` handler (without `SA_RESTART`) that sets `immediate_exit` and
///   interrupts `KVM_RUN`; and
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
    // SAFETY: We install an async-signal-safe handler (a single volatile byte write via a
    // thread-local pointer) for SIGUSR1 and SIGRTMIN and set SIGTTIN/SIGTTOU to SIG_IGN; all
    // operate on process-global signal dispositions.
    unsafe {
        let mut action: ::libc::sigaction = ::core::mem::zeroed();
        action.sa_sigaction = kick_handler as *const () as usize;
        action.sa_flags = 0;
        ::libc::sigemptyset(&mut action.sa_mask);
        // SIGUSR1: wake the boot processor for console input.
        ::libc::sigaction(::libc::SIGUSR1, &action, ::core::ptr::null_mut());
        // SIGRTMIN: the VM supervisor's vCPU kick (force a thread out of KVM_RUN so it can observe
        // the stop request). A real-time signal is used for the kick, matching cloud-hypervisor /
        // OpenVMM, so it never collides with SIGUSR1's console-input role.
        ::libc::sigaction(sigrtmin(), &action, ::core::ptr::null_mut());

        let mut ignore: ::libc::sigaction = ::core::mem::zeroed();
        ignore.sa_sigaction = ::libc::SIG_IGN;
        ignore.sa_flags = 0;
        ::libc::sigemptyset(&mut ignore.sa_mask);
        ::libc::sigaction(::libc::SIGTTIN, &ignore, ::core::ptr::null_mut());
        ::libc::sigaction(::libc::SIGTTOU, &ignore, ::core::ptr::null_mut());
    }
}

/// The real-time signal number used for the vCPU kick (`SIGRTMIN`, resolved at runtime).
fn sigrtmin() -> ::libc::c_int {
    ::libc::SIGRTMIN()
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
                        // runs the installed kick handler and interrupts KVM_RUN.
                        unsafe {
                            ::libc::pthread_kill(tid as ::libc::pthread_t, ::libc::SIGUSR1);
                        }
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsc_frequency_is_added_to_kernel_command_line() {
        let mut cmdline = String::from("console=hvc0");
        append_tsc_early_khz(&mut cmdline, 2_447_770);
        assert_eq!(cmdline, "console=hvc0 tsc_early_khz=2447770");
    }

    #[test]
    fn explicit_tsc_frequency_is_preserved() {
        let mut cmdline = String::from("tsc_early_khz=123 console=hvc0");
        append_tsc_early_khz(&mut cmdline, 2_447_770);
        assert_eq!(cmdline, "tsc_early_khz=123 console=hvc0");
    }

    #[test]
    fn tsc_frequency_can_start_an_empty_command_line() {
        let mut cmdline = String::new();
        append_tsc_early_khz(&mut cmdline, 2_447_770);
        assert_eq!(cmdline, "tsc_early_khz=2447770");
    }
}

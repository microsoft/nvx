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
//! the guest UART by a dedicated input thread that wakes the vCPU with `SIGUSR1`.
//!

use ::std::fs;
use ::std::io::{
    self,
    Read,
};
use ::std::path::PathBuf;
use ::std::sync::atomic::{
    AtomicU64,
    Ordering,
};
use ::std::sync::{
    Arc,
    Mutex,
};
use ::std::thread;

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
};

use crate::boot::pvh;
use crate::devices::DeviceBus;
use crate::devices::serial::Serial;
use crate::irq::{
    self,
    SERIAL_IRQ,
};
use crate::memory::GuestMemory;
use crate::vcpu::Vcpu;

/// Guest-physical address of the emulated task-state segment required by VT-x.
const TSS_ADDRESS: usize = 0xfffb_d000;

/// Restores the controlling terminal to its original mode on drop.
///
/// When standard input is a terminal it is switched to raw mode so that keystrokes reach
/// the guest console immediately and without local echo. When standard input is not a
/// terminal (e.g. a pipe) this is a no-op, so scripted runs are unaffected.
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
    /// Path to the uncompressed `vmlinux` (PVH) kernel image.
    pub kernel: PathBuf,
    /// Optional path to the initramfs image.
    pub initrd: Option<PathBuf>,
    /// Kernel command line.
    pub cmdline: String,
    /// Guest RAM size, in bytes.
    pub mem_bytes: u64,
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
/// Creates and runs a micro-VM until the guest halts or resets.
///
pub fn run(cfg: Config) -> Result<()> {
    let kernel: Vec<u8> = fs::read(&cfg.kernel)
        .with_context(|| format!("reading kernel image {:?}", cfg.kernel))?;
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
    let start_info_gpa: u64 = pvh::configure(&mem, &cfg.cmdline, initrd_region)?;
    vcpu.setup_pvh(&mem, loaded.pvh_entry, start_info_gpa)?;

    // Wire up the console UART and the host input thread.
    let serial: Arc<Mutex<Serial>> = Arc::new(Mutex::new(Serial::new(Box::new(io::stdout()))));
    let bus: DeviceBus = DeviceBus::new(Arc::clone(&serial));

    install_sigusr1_handler();
    let _tty_guard: TtyGuard = TtyGuard::new();
    let vcpu_tid: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    spawn_input_thread(bus.serial(), Arc::clone(&vcpu_tid));
    // SAFETY: `pthread_self` merely returns the calling thread's identifier.
    vcpu_tid.store(unsafe { ::libc::pthread_self() } as u64, Ordering::SeqCst);

    info!("starting guest (mem={} MiB, cmdline={:?})", ram_size >> 20, cfg.cmdline);

    loop {
        // Reflect the UART interrupt state onto the ISA line before (re)entering the guest.
        let pending: bool = serial.lock().expect("serial poisoned").interrupt_pending();
        vm_fd
            .set_irq_line(SERIAL_IRQ, pending)
            .context("KVM_IRQ_LINE failed")?;

        match vcpu.fd.run() {
            Ok(VcpuExit::IoIn(port, data)) => bus.pio_read(port, data),
            Ok(VcpuExit::IoOut(port, data)) => bus.pio_write(port, data),
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
                dump_vcpu(&vcpu);
                break;
            },
            Ok(VcpuExit::FailEntry(reason, cpu)) => {
                error!("KVM fail entry (reason={reason:#x}, cpu={cpu})");
                dump_vcpu(&vcpu);
                break;
            },
            Ok(other) => debug!("unhandled vcpu exit: {other:?}"),
            // A host-thread signal (console input) interrupted KVM_RUN: loop to refresh
            // the interrupt line and re-enter the guest.
            Err(e) if e.errno() == ::libc::EINTR => {},
            Err(e) => return Err(anyhow!("KVM_RUN failed: {e}")),
        }
    }

    Ok(())
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

/// Installs the `SIGUSR1` handler without `SA_RESTART` so it interrupts `KVM_RUN`.
fn install_sigusr1_handler() {
    // SAFETY: We install a trivial, async-signal-safe (no-op) handler for SIGUSR1.
    unsafe {
        let mut action: ::libc::sigaction = ::core::mem::zeroed();
        action.sa_sigaction = sigusr1_handler as *const () as usize;
        action.sa_flags = 0;
        ::libc::sigemptyset(&mut action.sa_mask);
        ::libc::sigaction(::libc::SIGUSR1, &action, ::core::ptr::null_mut());
    }
}

/// Spawns a thread that forwards host stdin to the guest UART, waking the vCPU per input.
fn spawn_input_thread(serial: Arc<Mutex<Serial>>, vcpu_tid: Arc<AtomicU64>) {
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 256];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    serial.lock().expect("serial poisoned").enqueue(&buf[..n]);
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

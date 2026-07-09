// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The VM setup and run-loop structure is the Windows Hypervisor Platform (WHP) analogue of the
// KVM backend of the Nanvix Micro-VM (uservm), Copyright(c) The Maintainers of Nanvix,
// MIT License.

//!
//! # Virtual Machine Monitor (WHP)
//!
//! The Windows backend. It PVH-boots the same Linux (Alpine) kernel + RAM initramfs as the
//! KVM backend, but drives the guest through the **Windows Hypervisor Platform** instead of
//! `/dev/kvm`.
//!
//! ## What WHP provides, and what this backend adds
//!
//! WHP virtualizes the CPU and memory and emulates the **local APIC** in the hypervisor, but
//! unlike KVM it has no in-hypervisor PIT, PIC, IOAPIC, `kvm-clock`, or legacy chipset. A PVH
//! guest with no ACPI/MP tables runs its APIC in "virtual wire, no configuration" mode, in which
//! Linux uses the **i8253 PIT on IRQ0** as its clock-event device — so, to keep the project's
//! device-less philosophy while still giving the guest a working timer, this backend adds:
//!
//! - the in-hypervisor **LAPIC** (`WHvX64LocalApicEmulationModeXApic`) for interrupt delivery
//!   and `HLT` handling;
//! - a minimal **8259 PIC** ([`pic`]) so the kernel detects the legacy interrupt controller and
//!   wires up IRQ0, plus a host **timer thread** that produces the PIT tick: it raises IRQ0 and
//!   the run loop injects it as a fixed local-APIC vector, completing the acknowledge with a
//!   local-APIC EOI applied through the interrupt-controller-state API (WHP's XApic registers are
//!   not otherwise writable);
//! - a minimal **RTC/CMOS** ([`rtc`]) so the kernel's wall-clock read does not spin on a floating
//!   port, and a hang-safe calibration **PIT counter** ([`pit`]);
//! - a **CPUID intercept** that hands the guest a known TSC frequency (measured from the host TSC
//!   at start-up, via CPUID leaves `0x15`/`0x16`) so it uses the TSC as a reliable clocksource,
//!   and forces the "always running APIC timer" bit (leaf `6`);
//! - the shared **portb console** (`0xE9`/`0xEA`) and control ports, decoded natively from WHP
//!   I/O-port exits (no instruction emulator needed, since the guest uses single-byte `in`/`out`).
//!
//! Snapshot/restore, virt-fs and virt-net are KVM-specific and are not implemented here.

mod memory;
mod pic;
mod pit;
mod rtc;
mod vcpu;

use ::core::ffi::c_void;
use ::std::fs;
use ::std::io::{
    self,
    Read,
};
use ::std::path::PathBuf;
use ::std::sync::atomic::{
    AtomicBool,
    Ordering,
};
use ::std::sync::{
    Arc,
    Mutex,
};
use ::std::thread;
use ::std::time::Duration;

use ::anyhow::{
    Context,
    Result,
    bail,
};
use ::log::{
    debug,
    error,
    info,
    warn,
};
use ::windows::Win32::System::Console::{
    CONSOLE_MODE,
    ENABLE_ECHO_INPUT,
    ENABLE_LINE_INPUT,
    ENABLE_PROCESSED_OUTPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    GetConsoleMode,
    GetStdHandle,
    STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE,
    SetConsoleMode,
};
use ::windows::Win32::Foundation::HANDLE;
use ::windows::Win32::System::Hypervisor::{
    WHV_EXTENDED_VM_EXITS,
    WHV_INTERRUPT_CONTROL,
    WHV_PARTITION_HANDLE,
    WHV_PARTITION_PROPERTY_CODE,
    WHV_REGISTER_NAME,
    WHV_REGISTER_VALUE,
    WHV_RUN_VP_EXIT_CONTEXT,
    WHvCancelRunVirtualProcessor,
    WHvCreatePartition,
    WHvCreateVirtualProcessor,
    WHvDeletePartition,
    WHvDeleteVirtualProcessor,
    WHvGetCapability,
    WHvGetVirtualProcessorInterruptControllerState,
    WHvGetVirtualProcessorRegisters,
    WHvRequestInterrupt,
    WHvPartitionPropertyCodeCpuidExitList,
    WHvPartitionPropertyCodeExtendedVmExits,
    WHvPartitionPropertyCodeLocalApicEmulationMode,
    WHvPartitionPropertyCodeProcessorCount,
    WHvRunVirtualProcessor,
    WHvRunVpExitReasonCanceled,
    WHvRunVpExitReasonMemoryAccess,
    WHvRunVpExitReasonNone,
    WHvRunVpExitReasonUnrecoverableException,
    WHvRunVpExitReasonX64Cpuid,
    WHvRunVpExitReasonX64Halt,
    WHvRunVpExitReasonX64IoPortAccess,
    WHvSetPartitionProperty,
    WHvSetVirtualProcessorInterruptControllerState,
    WHvSetVirtualProcessorRegisters,
    WHvSetupPartition,
    WHvX64LocalApicEmulationModeXApic,
    WHvX64RegisterCr0,
    WHvX64RegisterCr3,
    WHvX64RegisterCr4,
    WHvX64RegisterEfer,
    WHvX64RegisterRax,
    WHvX64RegisterRbx,
    WHvX64RegisterRcx,
    WHvX64RegisterRdx,
    WHvX64RegisterRflags,
    WHvX64RegisterRip,
    WHvX64RegisterRsp,
    WHvCapabilityCodeHypervisorPresent,
};

use crate::boot::pvh;
use crate::console::Console;
use crate::devices::portb::PortConsole;
use crate::devices::{
    DeviceBus,
    PioAction,
};
use crate::whp::memory::GuestMemory;
use crate::whp::pic::Pic;
use crate::whp::pit::Pit;
use crate::whp::rtc::Rtc;

/// Index of the single guest virtual processor.
const VP_INDEX: u32 = 0;

/// Configuration for a WHP micro-VM instance.
pub struct Config {
    /// Path to the uncompressed `vmlinux` (PVH) kernel image.
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
}

/// A WHP partition (VM) with an optional single vCPU, torn down in order on drop.
struct Partition {
    /// Native partition handle.
    handle: WHV_PARTITION_HANDLE,
    /// Whether vCPU 0 has been created (so it can be deleted on drop).
    vcpu_created: bool,
}

impl Partition {
    /// Creates and initializes a partition.
    fn new() -> Result<Self> {
        // SAFETY: `WHvCreatePartition` returns a freshly created partition handle.
        let handle: WHV_PARTITION_HANDLE =
            unsafe { WHvCreatePartition() }.context("WHvCreatePartition failed")?;
        Ok(Self {
            handle,
            vcpu_created: false,
        })
    }

    /// Sets a scalar/POD partition property.
    fn set_property<T: Copy>(&self, code: WHV_PARTITION_PROPERTY_CODE, value: &T) -> Result<()> {
        // SAFETY: `value` points to `size_of::<T>()` valid bytes for the duration of the call.
        unsafe {
            WHvSetPartitionProperty(
                self.handle,
                code,
                (value as *const T).cast::<c_void>(),
                size_of::<T>() as u32,
            )
            .with_context(|| format!("WHvSetPartitionProperty({}) failed", code.0))?;
        }
        Ok(())
    }

    /// Sets a partition property whose value is a slice (e.g. the CPUID exit list).
    fn set_property_slice<T: Copy>(
        &self,
        code: WHV_PARTITION_PROPERTY_CODE,
        values: &[T],
    ) -> Result<()> {
        // SAFETY: `values` points to `len * size_of::<T>()` valid bytes for the call.
        unsafe {
            WHvSetPartitionProperty(
                self.handle,
                code,
                values.as_ptr().cast::<c_void>(),
                (std::mem::size_of_val(values)) as u32,
            )
            .with_context(|| format!("WHvSetPartitionProperty({}) [slice] failed", code.0))?;
        }
        Ok(())
    }

    /// Finalizes partition configuration.
    fn setup(&self) -> Result<()> {
        // SAFETY: The partition handle is valid.
        unsafe {
            WHvSetupPartition(self.handle).context("WHvSetupPartition failed")?;
        }
        Ok(())
    }

    /// Creates the single guest vCPU.
    fn create_vcpu(&mut self) -> Result<()> {
        // SAFETY: The partition is set up and `VP_INDEX` is unused.
        unsafe {
            WHvCreateVirtualProcessor(self.handle, VP_INDEX, 0)
                .context("WHvCreateVirtualProcessor failed")?;
        }
        self.vcpu_created = true;
        Ok(())
    }
}

impl Drop for Partition {
    fn drop(&mut self) {
        // SAFETY: The handle is valid; deleting the vCPU before the partition is required.
        unsafe {
            if self.vcpu_created {
                let _ = WHvDeleteVirtualProcessor(self.handle, VP_INDEX);
            }
            let _ = WHvDeletePartition(self.handle);
        }
    }
}

/// Restores the console mode on drop; puts stdin in char-at-a-time mode and enables ANSI
/// output while the guest runs. A no-op when standard handles are not consoles (pipes/files).
struct ConsoleGuard {
    stdin: Option<(HANDLE, CONSOLE_MODE)>,
    stdout: Option<(HANDLE, CONSOLE_MODE)>,
}

impl ConsoleGuard {
    fn new() -> Self {
        let stdin: Option<(HANDLE, CONSOLE_MODE)> = configure_console(
            STD_INPUT_HANDLE,
            |m| CONSOLE_MODE(m.0 & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0)),
        );
        let stdout: Option<(HANDLE, CONSOLE_MODE)> = configure_console(STD_OUTPUT_HANDLE, |m| {
            CONSOLE_MODE(m.0 | ENABLE_PROCESSED_OUTPUT.0 | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0)
        });
        Self { stdin, stdout }
    }
}

impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        for entry in [self.stdin.take(), self.stdout.take()].into_iter().flatten() {
            // SAFETY: `entry.0` is a console handle previously returned by `GetStdHandle`.
            unsafe {
                let _ = SetConsoleMode(entry.0, entry.1);
            }
        }
    }
}

/// Reads a standard handle's console mode, applies `f`, and installs the result. Returns the
/// `(handle, saved_mode)` pair to restore later, or `None` if the handle is not a console.
fn configure_console(
    which: ::windows::Win32::System::Console::STD_HANDLE,
    f: impl FnOnce(CONSOLE_MODE) -> CONSOLE_MODE,
) -> Option<(HANDLE, CONSOLE_MODE)> {
    // SAFETY: All three console calls take a valid handle and a stack-allocated mode.
    unsafe {
        let handle: HANDLE = GetStdHandle(which).ok()?;
        let mut mode: CONSOLE_MODE = CONSOLE_MODE(0);
        GetConsoleMode(handle, &mut mode).ok()?;
        if SetConsoleMode(handle, f(mode)).is_ok() {
            Some((handle, mode))
        } else {
            None
        }
    }
}

/// Verifies that the Windows Hypervisor Platform is present and enabled.
fn ensure_whp_available() -> Result<()> {
    let mut present: u32 = 0;
    let mut written: u32 = 0;
    // SAFETY: `present`/`written` are valid out-pointers sized as declared.
    let hr = unsafe {
        WHvGetCapability(
            WHvCapabilityCodeHypervisorPresent,
            (&mut present as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
            Some(&mut written),
        )
    };
    if hr.is_err() || present == 0 {
        bail!(
            "the Windows Hypervisor Platform is not available. Enable it with \
             `Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform` \
             (and ensure virtualization is enabled in firmware), then reboot."
        );
    }
    Ok(())
}

///
/// # Description
///
/// Creates and runs a WHP micro-VM until the guest halts, resets, or the boot marker is seen.
///
pub fn run(cfg: Config) -> Result<()> {
    ensure_whp_available()?;

    let kernel_path: &PathBuf = cfg.kernel.as_ref().context("--kernel is required")?;
    let kernel: Vec<u8> =
        fs::read(kernel_path).with_context(|| format!("reading kernel image {kernel_path:?}"))?;
    let initrd: Option<Vec<u8>> = match &cfg.initrd {
        Some(path) => {
            Some(fs::read(path).with_context(|| format!("reading initramfs {path:?}"))?)
        },
        None => None,
    };

    // Measure the host TSC frequency now; it is handed to the guest via CPUID so Linux can use
    // the TSC as a known-frequency clocksource without probing legacy timers.
    let tsc_hz: u64 = measure_tsc_hz();
    info!("measured host TSC frequency: {} MHz", tsc_hz / 1_000_000);

    let mut partition: Partition = Partition::new()?;
    partition.set_property(WHvPartitionPropertyCodeProcessorCount, &1u32)?;

    // Intercept CPUID (bit 0 of the extended VM-exit set) so the leaves below can be tailored.
    let extended: WHV_EXTENDED_VM_EXITS = WHV_EXTENDED_VM_EXITS { AsUINT64: 1 };
    partition.set_property(WHvPartitionPropertyCodeExtendedVmExits, &extended)?;
    let cpuid_leaves: [u32; 3] = [0x0000_0006, 0x0000_0015, 0x0000_0016];
    partition.set_property_slice(WHvPartitionPropertyCodeCpuidExitList, &cpuid_leaves)?;

    // Emulate the local APIC in the hypervisor: the guest's only interrupt source and timer.
    if let Err(e) = partition.set_property(
        WHvPartitionPropertyCodeLocalApicEmulationMode,
        &WHvX64LocalApicEmulationModeXApic,
    ) {
        warn!("could not enable in-hypervisor LAPIC emulation ({e:#}); the guest timer will not work");
    }

    partition.setup()?;

    let mem: GuestMemory = GuestMemory::new(partition.handle, cfg.mem_bytes)?;
    let ram_size: u64 = mem.ram_size();
    partition.create_vcpu()?;

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
    vcpu::setup_pvh(partition.handle, VP_INDEX, &mem, loaded.pvh_entry, start_info_gpa)?;

    let (console, bus) = build_io(&cfg);
    info!(
        "starting guest (mem={} MiB, cmdline={:?})",
        ram_size >> 20,
        cfg.cmdline
    );
    execute(&cfg, &partition, &console, &bus, tsc_hz)
}

///
/// # Description
///
/// Runs a tiny 32-bit self-test program through the same PVH entry path to validate the WHP
/// backend's protected-mode setup independently of a kernel image.
///
pub fn selftest() -> Result<()> {
    ensure_whp_available()?;

    let mut partition: Partition = Partition::new()?;
    partition.set_property(WHvPartitionPropertyCodeProcessorCount, &1u32)?;
    partition.setup()?;
    let mem: GuestMemory = GuestMemory::new(partition.handle, 16 << 20)?;
    partition.create_vcpu()?;

    // 32-bit program: write "HI\n" to COM1 (0x3f8), then HLT.
    let prog: [u8; 15] = [
        0xba, 0xf8, 0x03, 0x00, 0x00, // mov edx, 0x3f8
        0xb0, b'H', 0xee, // mov al,'H'; out dx,al
        0xb0, b'I', 0xee, // mov al,'I'; out dx,al
        0xb0, 0x0a, 0xee, // mov al,'\n'; out dx,al
        0xf4, // hlt
    ];
    let entry: u64 = 0x0010_0000;
    mem.write_slice(entry, &prog)?;
    vcpu::setup_pvh(partition.handle, VP_INDEX, &mem, entry, 0x6000)?;

    info!("selftest: running tiny 32-bit program at {entry:#x}");
    let handle = partition.handle;
    let mut exit: WHV_RUN_VP_EXIT_CONTEXT = WHV_RUN_VP_EXIT_CONTEXT::default();
    loop {
        run_vcpu(handle, &mut exit)?;
        if exit.ExitReason == WHvRunVpExitReasonX64IoPortAccess {
            // SAFETY: The exit reason selects the `IoPortAccess` arm of the union.
            let io = unsafe { exit.Anonymous.IoPortAccess };
            if io.PortNumber == 0x3f8 && unsafe { io.AccessInfo.AsUINT32 } & 1 != 0 {
                print!("{}", (io.Rax as u8) as char);
                let _ = io::Write::flush(&mut io::stdout());
            }
            advance_rip(handle, &exit)?;
        } else if exit.ExitReason == WHvRunVpExitReasonX64Halt {
            info!("selftest: guest halted as expected — WHP protected-mode setup OK");
            break;
        } else {
            error!("selftest: unexpected exit reason {}", exit.ExitReason.0);
            dump_vcpu(handle);
            break;
        }
    }
    Ok(())
}

/// Builds the shared console sink, the portb console device, and the device bus.
fn build_io(cfg: &Config) -> (Arc<Mutex<Console>>, DeviceBus) {
    let console: Arc<Mutex<Console>> =
        Arc::new(Mutex::new(Console::new(cfg.quiet, &cfg.boot_marker)));
    let con: Arc<Mutex<PortConsole>> =
        Arc::new(Mutex::new(PortConsole::new(Arc::clone(&console))));
    let bus: DeviceBus = DeviceBus::new(Arc::clone(&con));
    (console, bus)
}

/// Drives the single-core execution loop.
fn execute(
    cfg: &Config,
    partition: &Partition,
    console: &Arc<Mutex<Console>>,
    bus: &DeviceBus,
    tsc_hz: u64,
) -> Result<()> {
    let handle = partition.handle;
    let _guard: ConsoleGuard = ConsoleGuard::new();
    let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // The emulated 8259 PIC, i8253 PIT and RTC/CMOS are owned by this (the vCPU) thread — the
    // only thread that touches WHP vCPU state. The timer thread merely nudges it via
    // `WHvCancelRunVirtualProcessor` (the one WHP call safe from another thread), which keeps
    // all register and interrupt-injection calls serialized on this thread.
    let mut pic: Pic = Pic::new();
    let mut pit: Pit = Pit::new();
    let mut rtc: Rtc = Rtc::new();

    // Host heartbeat: the timer thread sets this flag and cancels the run roughly every
    // `CONFIG_HZ` period, so the loop injects the guest's IRQ0 (the PIT tick) even while the
    // vCPU is parked at `HLT`. It is joined on shutdown (below) so no cancel can outlive the
    // partition. The input thread makes no WHP calls, so it can stay detached.
    let timer_tick: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    spawn_input_thread(bus.console(), Arc::clone(&stop));
    let timer_thread = spawn_timer_thread(handle, Arc::clone(&timer_tick), Arc::clone(&stop));

    console.lock().expect("console poisoned").mark_start();

    let mut exit: WHV_RUN_VP_EXIT_CONTEXT = WHV_RUN_VP_EXIT_CONTEXT::default();
    let mut run_err: Option<::anyhow::Error> = None;

    loop {
        // Flush buffered console output before re-entering the guest. The portb console is
        // interrupt-less (the guest's hvc driver polls), so there is no IRQ line to service.
        console.lock().expect("console poisoned").flush();

        if let Err(e) = run_vcpu(handle, &mut exit) {
            run_err = Some(e);
            break;
        }

        // Deliver a pending timer tick (raised by the timer thread) as the guest's IRQ0.
        if timer_tick.swap(false, Ordering::AcqRel) {
            inject_irq0(&mut pic, handle);
        }

        let reason = exit.ExitReason;
        if reason == WHvRunVpExitReasonX64IoPortAccess {
            match handle_io(&mut pit, &mut rtc, &mut pic, bus, handle, &exit) {
                Ok(PioAction::None) => {},
                Ok(PioAction::Shutdown) => {
                    info!("guest requested shutdown");
                    break;
                },
                Ok(PioAction::Snapshot) => {
                    warn!("guest requested a snapshot, which the WHP backend does not support");
                },
                Err(e) => {
                    run_err = Some(e);
                    break;
                },
            }
        } else if reason == WHvRunVpExitReasonX64Cpuid {
            if let Err(e) = handle_cpuid(handle, &exit, tsc_hz) {
                run_err = Some(e);
                break;
            }
        } else if reason == WHvRunVpExitReasonX64Halt {
            // With the LAPIC emulated, WHP normally parks the vCPU inside the run call until an
            // interrupt (so the timer thread's cancel returns us here to inject IRQ0). If a host
            // ever surfaces a bare Halt instead, a short sleep bounds CPU use until the next tick.
            thread::sleep(Duration::from_millis(1));
        } else if reason == WHvRunVpExitReasonCanceled || reason == WHvRunVpExitReasonNone {
            // Woken by the timer or input thread, or nothing to do: loop and re-enter.
        } else if reason == WHvRunVpExitReasonUnrecoverableException {
            // A PVH/no-ACPI guest reboots via triple fault, which surfaces here. Treat it as a
            // normal termination of the VM (matching `reboot=t`).
            info!("guest reset (reboot/triple fault)");
            break;
        } else if reason == WHvRunVpExitReasonMemoryAccess {
            // The device-less guest touches only RAM and the (hypervisor-emulated) LAPIC, so an
            // MMIO exit means an unmapped access we cannot service without an instruction
            // emulator. Report and stop rather than spin re-faulting the same instruction.
            error!("unhandled guest MMIO access");
            dump_vcpu(handle);
            break;
        } else {
            debug!("unhandled vcpu exit reason {}", reason.0);
        }

        if cfg.exit_on_boot && console.lock().expect("console poisoned").booted() {
            info!("boot marker seen — stopping guest (--exit-on-boot)");
            break;
        }
    }

    // Stop and join the timer thread *before* returning, so that its last
    // `WHvCancelRunVirtualProcessor` cannot run after the caller drops the partition (whose
    // `Drop` calls `WHvDeletePartition`). The join waits at most one tick (~10 ms).
    stop.store(true, Ordering::SeqCst);
    let _ = timer_thread.join();
    console.lock().expect("console poisoned").flush();

    if let Some(err) = run_err {
        return Err(err);
    }

    // Report the boot time independently of the logging level so it is available even when all
    // logging is suppressed.
    if cfg.exit_on_boot {
        let console = console.lock().expect("console poisoned");
        if let Some(elapsed) = console.cold_start() {
            eprintln!(
                "cold-start: {:.1} ms to marker ({} console bytes emitted)",
                elapsed.as_secs_f64() * 1000.0,
                console.bytes_out()
            );
        }
    }
    Ok(())
}

/// Runs the vCPU once, filling `exit` with the exit context.
fn run_vcpu(handle: WHV_PARTITION_HANDLE, exit: &mut WHV_RUN_VP_EXIT_CONTEXT) -> Result<()> {
    // SAFETY: `exit` is a valid, writable buffer of the declared size for the call.
    unsafe {
        WHvRunVirtualProcessor(
            handle,
            VP_INDEX,
            (exit as *mut WHV_RUN_VP_EXIT_CONTEXT).cast::<c_void>(),
            size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
        )
        .context("WHvRunVirtualProcessor failed")?;
    }
    Ok(())
}

/// Services a guest I/O-port access exit, advancing `RIP` past the instruction.
fn handle_io(
    pit: &mut Pit,
    rtc: &mut Rtc,
    pic: &mut Pic,
    bus: &DeviceBus,
    handle: WHV_PARTITION_HANDLE,
    exit: &WHV_RUN_VP_EXIT_CONTEXT,
) -> Result<PioAction> {
    // SAFETY: The caller only invokes this on an `X64IoPortAccess` exit.
    let io = unsafe { exit.Anonymous.IoPortAccess };
    let info: u32 = unsafe { io.AccessInfo.AsUINT32 };
    let is_write: bool = info & 1 != 0;
    let mut size: usize = ((info >> 1) & 0x7) as usize;
    if size == 0 || size > 8 {
        size = 1;
    }
    let port: u16 = io.PortNumber;

    let mut action: PioAction = PioAction::None;
    if is_write {
        let bytes: [u8; 8] = io.Rax.to_le_bytes();
        if Pit::owns(port) {
            for &b in &bytes[..size] {
                pit.write(port, b);
            }
        } else if Rtc::owns(port) {
            for &b in &bytes[..size] {
                rtc.write(port, b);
            }
        } else if Pic::owns(port) {
            let mut eoi = false;
            for &b in &bytes[..size] {
                eoi |= pic.write(port, b);
            }
            if eoi {
                lapic_eoi(handle);
            }
        } else {
            action = bus.pio_write(port, &bytes[..size]);
        }
        advance_rip(handle, exit)?;
    } else {
        let mut data: [u8; 8] = [0; 8];
        if Pit::owns(port) {
            data[0] = pit.read(port);
        } else if Rtc::owns(port) {
            data[0] = rtc.read(port);
        } else if Pic::owns(port) {
            data[0] = pic.read(port);
        } else {
            bus.pio_read(port, &mut data[..size]);
        }
        // Merge the read value into RAX, preserving the bytes above the access size.
        let mask: u64 = if size >= 8 {
            u64::MAX
        } else {
            (1u64 << (size * 8)) - 1
        };
        let read_val: u64 = u64::from_le_bytes(data);
        let rax: u64 = (io.Rax & !mask) | (read_val & mask);
        let next_rip: u64 = exit.VpContext.Rip.wrapping_add(instruction_length(exit));
        set_registers(
            handle,
            &[
                (WHvX64RegisterRax, rax),
                (WHvX64RegisterRip, next_rip),
            ],
        )?;
    }
    Ok(action)
}

/// Services a CPUID exit, tailoring the timer-relevant leaves and advancing `RIP`.
fn handle_cpuid(
    handle: WHV_PARTITION_HANDLE,
    exit: &WHV_RUN_VP_EXIT_CONTEXT,
    tsc_hz: u64,
) -> Result<()> {
    // SAFETY: The caller only invokes this on an `X64Cpuid` exit.
    let cpuid = unsafe { exit.Anonymous.CpuidAccess };
    let leaf: u32 = cpuid.Rax as u32;
    let mut eax: u32 = cpuid.DefaultResultRax as u32;
    let mut ebx: u32 = cpuid.DefaultResultRbx as u32;
    let mut ecx: u32 = cpuid.DefaultResultRcx as u32;
    let mut edx: u32 = cpuid.DefaultResultRdx as u32;

    match leaf {
        // Leaf 6: force ARAT (bit 2) so Linux trusts the LAPIC timer in idle.
        0x0000_0006 => eax |= 1 << 2,
        // Leaf 0x15: TSC/"core crystal" ratio. Numerator=denominator=1, crystal=TSC Hz, so the
        // guest computes tsc_khz = TSC Hz / 1000 and marks the frequency as known.
        0x0000_0015 => {
            let (num, crystal): (u32, u32) = if tsc_hz <= u64::from(u32::MAX) {
                (1, tsc_hz as u32)
            } else {
                (2, (tsc_hz / 2) as u32)
            };
            eax = 1;
            ebx = num;
            ecx = crystal;
            edx = 0;
        },
        // Leaf 0x16: nominal processor frequency information (MHz).
        0x0000_0016 => {
            let mhz: u32 = (tsc_hz / 1_000_000) as u32;
            eax = mhz;
            ebx = mhz;
            ecx = 100;
            edx = 0;
        },
        _ => {},
    }

    let next_rip: u64 = exit.VpContext.Rip.wrapping_add(instruction_length(exit));
    set_registers(
        handle,
        &[
            (WHvX64RegisterRax, u64::from(eax)),
            (WHvX64RegisterRbx, u64::from(ebx)),
            (WHvX64RegisterRcx, u64::from(ecx)),
            (WHvX64RegisterRdx, u64::from(edx)),
            (WHvX64RegisterRip, next_rip),
        ],
    )
}

/// Returns the length (in bytes) of the instruction that caused the exit.
fn instruction_length(exit: &WHV_RUN_VP_EXIT_CONTEXT) -> u64 {
    u64::from(exit.VpContext._bitfield & 0x0f)
}

/// Advances `RIP` past the instruction that caused `exit`.
fn advance_rip(handle: WHV_PARTITION_HANDLE, exit: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<()> {
    let next_rip: u64 = exit.VpContext.Rip.wrapping_add(instruction_length(exit));
    set_registers(handle, &[(WHvX64RegisterRip, next_rip)])
}

/// Performs a best-effort local-APIC end-of-interrupt on the vCPU (writing the APIC EOI
/// register clears the highest-priority in-service vector). WHP rejects the write when no
/// vector is in service, which is harmless here (there is simply nothing to complete), so the
/// error is ignored rather than propagated.
/// Completes the guest's IRQ0 at the local APIC.
///
/// Under WHP's XApic emulation the individual APIC registers (including the EOI register) are
/// not reachable via `WHvSetVirtualProcessorRegisters`, and there is no ExtINT injection type,
/// so a virtual-wire IRQ0 delivered as a fixed vector would leave its in-service bit set with no
/// way for the guest (which issues only the 8259 EOI) to clear it. The APIC save/restore API is
/// reachable, though: read the local-APIC state, clear the highest in-service vector's bit (the
/// one just acknowledged), and write it back — the effect of an EOI, letting the next tick be
/// delivered. Best-effort: any failure is ignored.
fn lapic_eoi(handle: WHV_PARTITION_HANDLE) {
    /// Offset of the in-service register (ISR) in WHP's local-APIC state: eight contiguous
    /// 32-bit words (vectors 0..255), i.e. a 4-byte stride rather than the 16-byte MMIO stride.
    const APIC_ISR: usize = 0x100;

    let mut state: [u8; 4096] = [0; 4096];
    let mut written: u32 = 0;
    // SAFETY: `state` is a 4096-byte buffer; `written` receives the actual size.
    let got = unsafe {
        WHvGetVirtualProcessorInterruptControllerState(
            handle,
            VP_INDEX,
            state.as_mut_ptr().cast::<c_void>(),
            state.len() as u32,
            Some(&mut written),
        )
    };
    if got.is_err() || (written as usize) < APIC_ISR + 0x20 {
        return;
    }

    // Clear the highest-priority in-service vector (the one just acknowledged), scanning the
    // eight ISR words from the highest vectors down.
    let mut cleared = false;
    for reg in (0..8).rev() {
        let off: usize = APIC_ISR + reg * 4;
        let mut word =
            u32::from_le_bytes([state[off], state[off + 1], state[off + 2], state[off + 3]]);
        if word != 0 {
            let highest: u32 = 31 - word.leading_zeros();
            word &= !(1 << highest);
            state[off..off + 4].copy_from_slice(&word.to_le_bytes());
            cleared = true;
            break;
        }
    }
    if !cleared {
        return;
    }

    // SAFETY: `state`/`written` describe the buffer just read back from WHP.
    unsafe {
        let _ = WHvSetVirtualProcessorInterruptControllerState(
            handle,
            VP_INDEX,
            state.as_ptr().cast::<c_void>(),
            written,
        );
    }
}

/// Sets a batch of 64-bit scalar registers on the vCPU.
fn set_registers(handle: WHV_PARTITION_HANDLE, pairs: &[(WHV_REGISTER_NAME, u64)]) -> Result<()> {
    let names: Vec<WHV_REGISTER_NAME> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<WHV_REGISTER_VALUE> =
        pairs.iter().map(|p| WHV_REGISTER_VALUE { Reg64: p.1 }).collect();
    // SAFETY: `names` and `values` are parallel arrays of equal length that outlive the call.
    unsafe {
        WHvSetVirtualProcessorRegisters(
            handle,
            VP_INDEX,
            names.as_ptr(),
            names.len() as u32,
            values.as_ptr(),
        )
        .context("WHvSetVirtualProcessorRegisters failed")?;
    }
    Ok(())
}

/// Spawns the host heartbeat: every `CONFIG_HZ` period it flags a pending timer tick and cancels
/// the vCPU run so the run loop delivers the guest's IRQ0 (the i8253 PIT tick).
///
/// A PVH guest with no ACPI/MP tables runs its APIC in "virtual wire, no configuration" mode,
/// where Linux does not use the local-APIC timer and instead relies on the PIT wired to IRQ0.
/// WHP has no in-hypervisor PIT/PIC to generate that interrupt, so the VMM produces the tick.
/// Only `WHvCancelRunVirtualProcessor` is called here — it is safe from another thread and wakes
/// a parked vCPU; the IRQ0 injection itself is done by the run loop on the vCPU thread, so all
/// register/interrupt state stays touched from a single thread. The 10 ms cadence matches
/// `CONFIG_HZ` (100 Hz) so jiffies advance correctly if the tick is ever periodic (early boot);
/// once the kernel switches to one-shot high-resolution mode, jiffies track the TSC clocksource
/// and the exact cadence no longer matters.
fn spawn_timer_thread(
    handle: WHV_PARTITION_HANDLE,
    timer_tick: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(10));
            // Re-check after sleeping so a cancel is never issued once shutdown has begun (the
            // partition may be about to be deleted).
            if stop.load(Ordering::SeqCst) {
                break;
            }
            timer_tick.store(true, Ordering::Release);
            // SAFETY: `handle` identifies the still-live partition (the run loop joins this
            // thread before dropping it); cancelling a run is always safe.
            unsafe {
                let _ = WHvCancelRunVirtualProcessor(handle, VP_INDEX, 0);
            }
        }
    })
}

/// Injects the guest's IRQ0 (the timer line) if the emulated PIC can deliver it right now.
///
/// Runs on the vCPU thread. The tick is delivered as a fixed local-APIC interrupt at the vector
/// the guest programmed the 8259 with; the guest's 8259 end-of-interrupt is completed with a
/// matching local-APIC EOI in [`handle_io`], since a virtual-wire guest issues only the former.
fn inject_irq0(pic: &mut Pic, handle: WHV_PARTITION_HANDLE) {
    let Some(vector) = pic.raise_irq0() else {
        return;
    };
    let interrupt: WHV_INTERRUPT_CONTROL = WHV_INTERRUPT_CONTROL {
        // Type = Fixed, DestinationMode = Physical, TriggerMode = Edge (all zero).
        _bitfield: 0,
        Destination: 0,
        Vector: u32::from(vector),
    };
    // SAFETY: `interrupt` is a valid control block of the declared size; the partition is live.
    unsafe {
        let _ = WHvRequestInterrupt(handle, &interrupt, size_of::<WHV_INTERRUPT_CONTROL>() as u32);
    }
}

/// Spawns a thread that forwards host stdin to the guest console.
///
/// It makes no WHP calls — it only enqueues bytes into the shared console device — so it can be
/// left detached without any partition-handle lifetime hazard. The guest observes the input on
/// its next `hvc` poll, which the timer tick keeps running; it does not need an explicit wake.
fn spawn_input_thread(con: Arc<Mutex<PortConsole>>, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 256];
        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    con.lock().expect("console poisoned").enqueue(&buf[..n]);
                },
            }
        }
    });
}

/// Measures the host TSC frequency (Hz) against the performance counter.
fn measure_tsc_hz() -> u64 {
    use ::windows::Win32::System::Performance::{
        QueryPerformanceCounter,
        QueryPerformanceFrequency,
    };

    let mut qpf: i64 = 1;
    let mut q0: i64 = 0;
    let mut q1: i64 = 0;
    // SAFETY: All out-pointers are valid, writable `i64`s.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut qpf);
        let _ = QueryPerformanceCounter(&mut q0);
    }
    // SAFETY: `_rdtsc` is always available on x86_64.
    let t0: u64 = unsafe { ::core::arch::x86_64::_rdtsc() };
    thread::sleep(Duration::from_millis(20));
    // SAFETY: `_rdtsc` is always available on x86_64.
    let t1: u64 = unsafe { ::core::arch::x86_64::_rdtsc() };
    // SAFETY: valid out-pointer.
    unsafe {
        let _ = QueryPerformanceCounter(&mut q1);
    }

    let dq: i64 = (q1 - q0).max(1);
    let dt: u128 = u128::from(t1.wrapping_sub(t0));
    let hz: u128 = dt * qpf as u128 / dq as u128;
    // Guard against a wildly implausible reading (e.g. a de-scheduled measurement window).
    if (500_000_000..=10_000_000_000).contains(&hz) {
        hz as u64
    } else {
        2_000_000_000
    }
}

/// Dumps guest register state for diagnosing early boot faults.
fn dump_vcpu(handle: WHV_PARTITION_HANDLE) {
    let names: [WHV_REGISTER_NAME; 8] = [
        WHvX64RegisterRip,
        WHvX64RegisterRsp,
        WHvX64RegisterRflags,
        WHvX64RegisterRax,
        WHvX64RegisterCr0,
        WHvX64RegisterCr3,
        WHvX64RegisterCr4,
        WHvX64RegisterEfer,
    ];
    let mut values: [WHV_REGISTER_VALUE; 8] = [WHV_REGISTER_VALUE::default(); 8];
    // SAFETY: `names`/`values` are equal-length arrays valid for the call.
    let ok = unsafe {
        WHvGetVirtualProcessorRegisters(
            handle,
            VP_INDEX,
            names.as_ptr(),
            names.len() as u32,
            values.as_mut_ptr(),
        )
        .is_ok()
    };
    if !ok {
        error!("  <failed to read vcpu registers>");
        return;
    }
    // SAFETY: Every queried register was requested as a 64-bit scalar.
    unsafe {
        error!(
            "  rip={:#018x} rsp={:#018x} rflags={:#010x} rax={:#018x}",
            values[0].Reg64, values[1].Reg64, values[2].Reg64, values[3].Reg64
        );
        error!(
            "  cr0={:#018x} cr3={:#018x} cr4={:#018x} efer={:#018x}",
            values[4].Reg64, values[5].Reg64, values[6].Reg64, values[7].Reg64
        );
    }
}

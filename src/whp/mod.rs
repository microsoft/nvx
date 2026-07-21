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
//! Snapshot/restore, virt-fs and virt-net are implemented here. `--net` is a standalone
//! user-mode SLIRP backend; `--net-config` is an explicit external L2Bridge contract and never
//! silently falls back to SLIRP.

mod emulator;
mod memory;
mod net;
mod pic;
mod pit;
mod rtc;
mod slirp;
mod snapshot;
mod vcpu;
mod virtfs;
mod xdp;

use ::core::ffi::c_void;
use ::std::fs;
use ::std::io::{self, Read};
use ::std::path::{Path, PathBuf};
use ::std::sync::atomic::{AtomicBool, Ordering};
use ::std::sync::{Arc, Mutex};
use ::std::thread;
use ::std::time::{Duration, Instant};

use ::anyhow::{Context, Result, bail};
use ::log::{debug, error, info, warn};
use ::windows::Win32::System::Hypervisor::{
    WHV_EXTENDED_VM_EXITS, WHV_INTERRUPT_CONTROL, WHV_MEMORY_ACCESS_CONTEXT, WHV_PARTITION_HANDLE,
    WHV_PARTITION_PROPERTY_CODE, WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_RUN_VP_EXIT_CONTEXT,
    WHV_VP_EXIT_CONTEXT, WHvCancelRunVirtualProcessor, WHvCapabilityCodeHypervisorPresent,
    WHvCreatePartition, WHvCreateVirtualProcessor, WHvDeletePartition, WHvDeleteVirtualProcessor,
    WHvGetCapability, WHvGetVirtualProcessorInterruptControllerState,
    WHvGetVirtualProcessorRegisters, WHvPartitionPropertyCodeCpuidExitList,
    WHvPartitionPropertyCodeExtendedVmExits, WHvPartitionPropertyCodeLocalApicEmulationMode,
    WHvPartitionPropertyCodeProcessorCount, WHvRequestInterrupt, WHvRunVirtualProcessor,
    WHvRunVpExitReasonCanceled, WHvRunVpExitReasonMemoryAccess, WHvRunVpExitReasonNone,
    WHvRunVpExitReasonUnrecoverableException, WHvRunVpExitReasonX64Cpuid,
    WHvRunVpExitReasonX64Halt, WHvRunVpExitReasonX64IoPortAccess, WHvSetPartitionProperty,
    WHvSetVirtualProcessorInterruptControllerState, WHvSetVirtualProcessorRegisters,
    WHvSetupPartition, WHvX64LocalApicEmulationModeXApic, WHvX64RegisterCr0, WHvX64RegisterCr3,
    WHvX64RegisterCr4, WHvX64RegisterEfer, WHvX64RegisterRax, WHvX64RegisterRbx, WHvX64RegisterRcx,
    WHvX64RegisterRdx, WHvX64RegisterRflags, WHvX64RegisterRip, WHvX64RegisterRsp,
};

use crate::boot::pvh;
use crate::console::{Console, TimingMarker};
use crate::devices::portb::PortConsole;
use crate::devices::{DeviceBus, PioAction};
use crate::hcs::{HcnNetworkConfig, OwnedHcnNetwork};
use crate::l2bridge::{ExternalIdentity, L2BridgeConfig};
use crate::whp::emulator::{Emulator, MmioHandler};
use crate::whp::memory::GuestMemory;
pub use crate::whp::net::NetConfig;
use crate::whp::net::{FrameBackend, VirtioNet};
use crate::whp::pic::Pic;
use crate::whp::pit::Pit;
use crate::whp::rtc::Rtc;
use crate::whp::snapshot::Snapshot;
use crate::windows_terminal::ConsoleGuard;

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
    /// Additional named console substrings timed from the first guest instruction.
    pub timing_markers: Vec<TimingMarker>,
    /// Delay redirected cold-boot stdin until the boot marker appears.
    pub defer_stdin_until_boot: bool,
    /// Directory to write a snapshot to when the guest requests one (control port `0x605`).
    pub snapshot: Option<PathBuf>,
    /// Directory to restore the VM from instead of cold-booting a kernel.
    pub restore: Option<PathBuf>,
    /// Agent-hosted pipe signaled after snapshot and device state are restored.
    pub restore_ready_pipe: Option<String>,
    /// Optional virt-net endpoint (`--net`): the guest IP/prefix and derived host gateway.
    pub net: Option<NetConfig>,
    /// Optional external L2Bridge data-plane contract (`--net-config`).
    pub net_config: Option<L2BridgeConfig>,
    /// Host directory to export to the guest as a virt-fs (`--mount`).
    pub mount: Option<PathBuf>,
    /// Guest mount point for the `--mount` directory.
    pub mount_target: String,
    /// Export the `--mount` directory read-write instead of read-only.
    pub mount_rw: bool,
    /// Optional host file backing a read-write `--mount` (implies read-write; persists writes).
    pub mount_image: Option<PathBuf>,
    /// Optional size (MiB) of the writable image (headroom for guest writes).
    pub mount_size: Option<u64>,
}

/// A running virt-net NIC: the shared device model, raw-frame backend, and snapshot-safe identity.
struct Nic {
    /// The virtio-net device, shared between the vCPU thread (MMIO/TX) and the RX pump thread.
    dev: Arc<Mutex<VirtioNet>>,
    /// Data plane shared by the TX device path and RX pump.
    backend: Arc<dyn FrameBackend>,
    /// Snapshot header containing guest identity only, never host attachment state.
    snapshot_header: Vec<u8>,
    /// HCN must outlive every AF_XDP handle that targets its host vNIC.
    _hcn_network: Option<OwnedHcnNetwork>,
}

impl Nic {
    /// Builds the standalone SLIRP NIC.
    fn build_slirp(mem: &Arc<GuestMemory>, ncfg: &NetConfig) -> Self {
        let slirp = slirp::start(ncfg);
        let backend: Arc<dyn FrameBackend> = slirp;
        let dev: Arc<Mutex<VirtioNet>> = Arc::new(Mutex::new(VirtioNet::new(
            Arc::clone(mem),
            Arc::clone(&backend),
            ncfg.mac,
        )));
        Nic {
            dev,
            backend,
            snapshot_header: ncfg.save_header(),
            _hcn_network: None,
        }
    }

    /// Builds the external NIC. `xdp::start` must report readiness before this returns.
    fn build_l2bridge(mem: &Arc<GuestMemory>, config: &L2BridgeConfig) -> Result<Self> {
        // The Agent owns and hosts this pipe; NVX is only a client. Connect before initializing
        // XDP so an initialization failure can be reported, then do not enter the vCPU loop until
        // the Agent explicitly acknowledges the ready data plane with StartVm.
        let pipe = xdp::ControlPipe::connect(&config.runtime.control_pipe)?;
        let mut effective_config = config.clone();
        let mut hcn_network = None;
        let mut arp_proxy = None;
        if config.provisions_hcn_vnic() {
            let provisioned = (|| -> Result<OwnedHcnNetwork> {
                let hcn_config = HcnNetworkConfig::from_l2bridge(config)?;
                let mut network = OwnedHcnNetwork::create(hcn_config)?;
                let attachment = network.attach_to_host()?;
                effective_config.attachment.interface_index = attachment.interface_index;
                effective_config.attachment.interface_luid = attachment.interface_luid;
                arp_proxy = Some(xdp::ArpProxy {
                    guest_mac: config.mac()?,
                    guest_ip: config.guest_bootstrap.ipv4.address.parse()?,
                    gateway_mac: attachment.gateway_mac,
                    gateway_ip: config.guest_bootstrap.ipv4.gateway.parse()?,
                });
                Ok(network)
            })();
            match provisioned {
                Ok(network) => hcn_network = Some(network),
                Err(error) => {
                    pipe.data_plane_error(&format!("{error:#}"));
                    return Err(error);
                }
            }
        }

        let started = match xdp::start(&effective_config, arp_proxy) {
            Ok(started) => started,
            Err(error) => {
                pipe.data_plane_error(&format!("{error:#}"));
                return Err(error);
            }
        };
        if let Err(error) =
            pipe.data_plane_ready(&started.queues, started.interface_luid)
        {
            pipe.data_plane_error(&format!("{error:#}"));
            return Err(error);
        }
        if let Err(error) = pipe.wait_start_vm() {
            pipe.data_plane_error(&format!("{error:#}"));
            return Err(error);
        }

        let backend = started.backend;
        let identity = config.external_identity()?;
        let dev = Arc::new(Mutex::new(VirtioNet::new(
            Arc::clone(mem),
            Arc::clone(&backend),
            identity.mac,
        )));
        Ok(Self {
            dev,
            backend,
            snapshot_header: NetConfig::save_external_header(&identity),
            _hcn_network: hcn_network,
        })
    }
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
/// Creates and runs a WHP micro-VM: either cold-boots a kernel or resumes from a snapshot
/// directory, depending on [`Config`].
///
pub fn run(cfg: Config) -> Result<()> {
    ensure_whp_available()?;
    match cfg.restore.clone() {
        Some(dir) => run_restore(cfg, &dir),
        None => run_cold(cfg),
    }
}

/// Creates a partition configured identically for both cold boot and restore: one vCPU, CPUID
/// interception for the timer-relevant leaves, and in-hypervisor XApic emulation.
fn create_partition() -> Result<Partition> {
    let partition: Partition = Partition::new()?;
    partition.set_property(WHvPartitionPropertyCodeProcessorCount, &1u32)?;

    // Intercept CPUID (bit 0 of the extended VM-exit set) so the leaves below can be tailored.
    let extended: WHV_EXTENDED_VM_EXITS = WHV_EXTENDED_VM_EXITS { AsUINT64: 1 };
    partition.set_property(WHvPartitionPropertyCodeExtendedVmExits, &extended)?;
    let cpuid_leaves: [u32; 3] = [0x0000_0006, 0x0000_0015, 0x0000_0016];
    partition.set_property_slice(WHvPartitionPropertyCodeCpuidExitList, &cpuid_leaves)?;

    // Emulate the local APIC in the hypervisor: the guest's only interrupt-delivery mechanism.
    if let Err(e) = partition.set_property(
        WHvPartitionPropertyCodeLocalApicEmulationMode,
        &WHvX64LocalApicEmulationModeXApic,
    ) {
        warn!(
            "could not enable in-hypervisor LAPIC emulation ({e:#}); the guest timer will not work"
        );
    }

    partition.setup()?;
    Ok(partition)
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

    // Measure the host TSC frequency now; it is handed to the guest via CPUID so Linux can use
    // the TSC as a known-frequency clocksource without probing legacy timers.
    let tsc_hz: u64 = measure_tsc_hz();
    info!("measured host TSC frequency: {} MHz", tsc_hz / 1_000_000);

    let mut partition: Partition = create_partition()?;
    let mem: Arc<GuestMemory> = Arc::new(GuestMemory::new(partition.handle, cfg.mem_bytes)?);
    let ram_size: u64 = mem.ram_size();
    partition.create_vcpu()?;

    // Append the selected NIC's guest configuration. `--net` remains standalone SLIRP; an
    // external manifest transports the exact CNI values without deriving a gateway or MAC.
    let mut cmdline: String = match (&cfg.net, &cfg.net_config) {
        (Some(ncfg), None) => format!("{} {}", cfg.cmdline, ncfg.cmdline_fragment()),
        (None, Some(config)) => format!("{} {}", cfg.cmdline, config.guest_cmdline_fragment()?),
        (None, None) => cfg.cmdline.clone(),
        (Some(_), Some(_)) => bail!("--net and --net-config are mutually exclusive"),
    };

    // Optionally export a host directory to the guest as a virt-fs. The FAT image is mapped into
    // guest memory above reported RAM and pointed at via the kernel command line; `_virtfs` owns
    // that mapping (and, for a persistent read-write export, flushes it) and must stay alive until
    // the guest stops.
    let _virtfs: Option<virtfs::VirtFs> = match &cfg.mount {
        Some(dir) => {
            let opts = virtfs::Options {
                dir,
                target: &cfg.mount_target,
                writable: cfg.mount_rw || cfg.mount_image.is_some(),
                image: cfg.mount_image.as_deref(),
                size: cfg.mount_size.map(|mib| mib << 20),
            };
            let (fs, fragment) = virtfs::load(partition.handle, ram_size, opts)?;
            cmdline.push(' ');
            cmdline.push_str(&fragment);
            Some(fs)
        }
        None => None,
    };
    if cfg.net_config.is_some() && cmdline.len() > crate::l2bridge::MAX_GUEST_CMDLINE {
        bail!(
            "external guest network configuration exceeds the {}-byte kernel command-line limit",
            crate::l2bridge::MAX_GUEST_CMDLINE
        );
    }

    // Load the kernel, the initramfs, and the PVH boot structures.
    let loaded = pvh::load_kernel(&*mem, &kernel)?;
    info!(
        "loaded kernel: pvh_entry={:#x}, kernel_end={:#x}",
        loaded.pvh_entry, loaded.kernel_end
    );
    let initrd_region = match &initrd {
        Some(bytes) => {
            let region = pvh::load_initramfs(&*mem, bytes, loaded.kernel_end, ram_size)?;
            info!(
                "loaded initramfs: addr={:#x}, size={:#x}",
                region.addr, region.size
            );
            Some(region)
        }
        None => None,
    };
    let start_info_gpa: u64 = pvh::configure(&*mem, &cmdline, initrd_region)?;
    vcpu::setup_pvh(
        partition.handle,
        VP_INDEX,
        &mem,
        loaded.pvh_entry,
        start_info_gpa,
    )?;

    // Build the NIC before entering the vCPU loop. An external backend must reach data-plane
    // readiness here; failure aborts the launch rather than falling back to standalone SLIRP.
    let nic: Option<Nic> = match (&cfg.net, &cfg.net_config) {
        (Some(ncfg), None) => {
            info!(
                "virt-net: standalone SLIRP NIC at {:#x} (guest {}/{}, gateway {})",
                net::NET_MMIO_BASE,
                ncfg.guest_ip,
                ncfg.prefix,
                ncfg.host_ip
            );
            Some(Nic::build_slirp(&mem, ncfg))
        }
        (None, Some(config)) => {
            if config.provisions_hcn_vnic() {
                info!(
                    "virt-net: provisioning HCN AF_XDP NIC at {:#x} (MTU {})",
                    net::NET_MMIO_BASE,
                    config.device.mtu
                );
            } else {
                info!(
                    "virt-net: initializing external L2Bridge NIC at {:#x} (ifIndex {}, MTU {})",
                    net::NET_MMIO_BASE,
                    config.attachment.interface_index,
                    config.device.mtu
                );
            }
            Some(Nic::build_l2bridge(&mem, config)?)
        }
        (None, None) => None,
        (Some(_), Some(_)) => bail!("--net and --net-config are mutually exclusive"),
    };

    let (console, bus) = build_io(&cfg, None);
    info!(
        "starting guest (mem={} MiB, cmdline={:?})",
        ram_size >> 20,
        cmdline
    );
    execute(
        &cfg,
        &partition,
        &mem,
        Pic::new(),
        Pit::new(),
        Rtc::new(),
        &console,
        &bus,
        tsc_hz,
        nic,
    )
}

/// Restores and resumes a VM from a snapshot directory.
fn run_restore(cfg: Config, dir: &Path) -> Result<()> {
    let snap: Snapshot = Snapshot::read(dir)?;
    let tsc_hz: u64 = measure_tsc_hz();

    let mut partition: Partition = create_partition()?;
    let mem: Arc<GuestMemory> = Arc::new(GuestMemory::restore(
        partition.handle,
        &dir.join("mem.bin"),
        snap.ram_size(),
    )?);
    partition.create_vcpu()?;
    snap.apply(partition.handle)?;

    // Rebuild the emulated devices from the saved state.
    let mut pic: Pic = Pic::new();
    pic.load(snap.pic());
    let mut pit: Pit = Pit::new();
    pit.load(snap.pit());
    let mut rtc: Rtc = Rtc::new();
    rtc.load(snap.rtc());

    // An external snapshot is restored only with a fresh external manifest. The data-plane
    // backend is initialized and ready before device state is restored and the vCPU can resume.
    let nic: Option<Nic> = build_restored_nic(&mem, snap.net(), cfg.net_config.as_ref())?;
    if nic.is_none() && cfg.net_config.is_some() {
        bail!("--net-config was supplied but this snapshot has no NIC");
    }

    let (console, bus) = build_io(&cfg, Some(snap.console()));
    info!(
        "resuming guest from snapshot {dir:?} (mem={} MiB)",
        snap.ram_size() >> 20
    );
    execute(
        &cfg, &partition, &mem, pic, pit, rtc, &console, &bus, tsc_hz, nic,
    )
}

/// Rebuilds the NIC from serialized snapshot state, or returns `None` if the snapshot had no NIC.
fn build_restored_nic(
    mem: &Arc<GuestMemory>,
    net_state: &[u8],
    external: Option<&L2BridgeConfig>,
) -> Result<Option<Nic>> {
    if net_state.is_empty() {
        return Ok(None);
    }
    let (nic, consumed): (Nic, usize) = if NetConfig::is_external_snapshot(net_state) {
        let config = external.context(
            "an external L2Bridge snapshot requires a fresh --net-config; refusing SLIRP fallback",
        )?;
        let (saved, consumed) = NetConfig::load_external_header(net_state)?;
        let current: ExternalIdentity = config.external_identity()?;
        if !saved.equivalent(&current) {
            bail!(
                "external L2Bridge restore identity mismatch (MAC, MTU, or guest bootstrap changed)"
            );
        }
        (Nic::build_l2bridge(mem, config)?, consumed)
    } else {
        if external.is_some() {
            bail!("a standalone SLIRP snapshot cannot be restored with --net-config");
        }
        let (ncfg, consumed) = NetConfig::from_header(net_state)?;
        (Nic::build_slirp(mem, &ncfg), consumed)
    };
    {
        let mut dev = nic.dev.lock().expect("virt-net poisoned");
        dev.load(&net_state[consumed..])?;
        dev.resume();
    }
    info!("virt-net: NIC restored after backend readiness");
    Ok(Some(nic))
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

/// Builds the shared console sink, the portb console device, and the device bus. When
/// `con_state` is provided (restore path), the device's pending input queue is reloaded from it.
fn build_io(cfg: &Config, con_state: Option<&[u8]>) -> (Arc<Mutex<Console>>, DeviceBus) {
    let console: Arc<Mutex<Console>> = Arc::new(Mutex::new(Console::with_timing_markers(
        cfg.quiet,
        &cfg.boot_marker,
        &cfg.timing_markers,
    )));
    let con: Arc<Mutex<PortConsole>> =
        Arc::new(Mutex::new(PortConsole::new(Arc::clone(&console))));
    if let Some(state) = con_state {
        con.lock().expect("console poisoned").restore(state);
    }
    let bus: DeviceBus = DeviceBus::new(Arc::clone(&con));
    (console, bus)
}

/// Drives the single-core execution loop shared by the cold-boot and restore paths.
// The loop needs the full VM context (config, partition, memory, the console/device bus, the
// emulated devices, and the TSC frequency); grouping these purely to satisfy the argument-count
// lint would not aid clarity.
#[allow(clippy::too_many_arguments)]
fn execute(
    cfg: &Config,
    partition: &Partition,
    mem: &GuestMemory,
    mut pic: Pic,
    mut pit: Pit,
    mut rtc: Rtc,
    console: &Arc<Mutex<Console>>,
    bus: &DeviceBus,
    tsc_hz: u64,
    nic: Option<Nic>,
) -> Result<()> {
    let handle = partition.handle;
    let guard: ConsoleGuard = ConsoleGuard::new();
    let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // The emulated 8259 PIC, i8253 PIT and RTC/CMOS are owned by this (the vCPU) thread — the
    // only thread that touches WHP vCPU state. The timer thread merely nudges it via
    // `WHvCancelRunVirtualProcessor` (the one WHP call safe from another thread), which keeps
    // all register and interrupt-injection calls serialized on this thread.

    // The virtio-net NIC (if any) needs WHP's instruction emulator to service its MMIO window, and
    // a receive pump that feeds NAT frames into the guest and wakes this loop to inject the NIC's
    // IRQ. The pump is joined on shutdown, like the timer thread.
    let emulator: Option<Emulator> = match &nic {
        Some(_) => Some(Emulator::new()?),
        None => None,
    };
    if let Some(nic) = nic.as_ref() {
        nic.backend
            .check_health()
            .context("network backend was not ready before vCPU execution")?;
        if nic.backend.health() != net::BackendHealth::Ready {
            bail!("network backend was not ready before vCPU execution");
        }
    }
    let net_stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let mut net_pump: Option<thread::JoinHandle<()>> = nic.as_ref().map(|n| {
        spawn_net_rx(
            Arc::clone(&n.dev),
            Arc::clone(&n.backend),
            handle,
            Arc::clone(&net_stop),
        )
    });

    // Host heartbeat: the timer thread sets this flag and cancels the run roughly every
    // `CONFIG_HZ` period, so the loop injects the guest's IRQ0 (the PIT tick) even while the
    // vCPU is parked at `HLT`. It is joined on shutdown (below) so no cancel can outlive the
    // partition. The input thread makes no WHP calls, so it can stay detached.
    let timer_pending: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let defer_input: bool =
        cfg.defer_stdin_until_boot && !guard.stdin_is_console() && cfg.restore.is_none();
    spawn_input_thread(
        bus.console(),
        Arc::clone(console),
        Arc::clone(&stop),
        defer_input,
    );
    let timer_thread =
        spawn_timer_thread(handle, Arc::clone(&timer_pending), Arc::clone(&stop));

    console.lock().expect("console poisoned").mark_start();

    let mut exit: WHV_RUN_VP_EXIT_CONTEXT = WHV_RUN_VP_EXIT_CONTEXT::default();
    let mut run_err: Option<::anyhow::Error> = None;
    let mut prefer_timer = true;
    if let Some(path) = cfg.restore_ready_pipe.as_deref() {
        let pipe = xdp::ControlPipe::connect(path)?;
        pipe.restore_ready()?;
    }

    loop {
        // Flush buffered console output before re-entering the guest. The portb console is
        // interrupt-less (the guest's hvc driver polls), so there is no IRQ line to service.
        console.lock().expect("console poisoned").flush();

        if let Err(e) = run_vcpu(handle, &mut exit) {
            run_err = Some(e);
            break;
        }
        if let Some(nic) = nic.as_ref()
            && let Err(error) = nic.backend.check_health()
        {
            run_err = Some(error.context("network backend failed while the VM was running"));
            break;
        }
        if let Some(nic) = nic.as_ref() {
            nic.dev.lock().expect("virt-net poisoned").process_tx();
        }

        // Deliver a pending timer tick (raised by the timer thread) as the guest's IRQ0, and
        // re-check the NIC on the same cadence so a receive interrupt that could not be injected
        // earlier (e.g. the line was briefly masked) self-heals within one tick.
        service_pending_irqs(
            &timer_pending,
            &nic,
            &mut pic,
            handle,
            &mut prefer_timer,
        );

        let reason = exit.ExitReason;
        if reason == WHvRunVpExitReasonX64IoPortAccess {
            match handle_io(&mut pit, &mut rtc, &mut pic, bus, handle, &exit) {
                Ok(result) => {
                    if result.pic_eoi {
                        service_pending_irqs(
                            &timer_pending,
                            &nic,
                            &mut pic,
                            handle,
                            &mut prefer_timer,
                        );
                    }
                    match result.action {
                        PioAction::None => {}
                        PioAction::Shutdown => {
                            info!("guest requested shutdown");
                            break;
                        }
                        PioAction::Snapshot => {
                            if cfg.snapshot.is_some() {
                                if let Some(n) = nic.as_ref()
                                    && let Err(error) =
                                        n.backend.quiesce(Duration::from_secs(2))
                                {
                                    run_err = Some(
                                        error.context(
                                            "quiescing network backend for snapshot",
                                        ),
                                    );
                                    break;
                                }
                                // The quiesced backend produces no more frames. Join the pump
                                // before copying RAM to exclude a concurrent process_rx write.
                                net_stop.store(true, Ordering::SeqCst);
                                if let Some(pump) = net_pump.take() {
                                    let _ = pump.join();
                                }
                            }
                            match take_snapshot(
                                cfg,
                                handle,
                                mem,
                                &pic,
                                &pit,
                                &rtc,
                                console,
                                bus,
                                nic.as_ref(),
                            ) {
                                Ok(true) => break,
                                Ok(false) => {}
                                Err(e) => {
                                    run_err = Some(e);
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    run_err = Some(e);
                    break;
                }
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
            // Woken by the timer, input thread, or the NIC receive pump: service a pending NIC
            // interrupt so a just-delivered frame is signalled to the guest with low latency.
            service_pending_irqs(
                &timer_pending,
                &nic,
                &mut pic,
                handle,
                &mut prefer_timer,
            );
        } else if reason == WHvRunVpExitReasonUnrecoverableException {
            // A PVH/no-ACPI guest reboots via triple fault, which surfaces here. Treat it as a
            // normal termination of the VM (matching `reboot=t`).
            info!("guest reset (reboot/triple fault)");
            break;
        } else if reason == WHvRunVpExitReasonMemoryAccess {
            // A guest access to the (unmapped) virtio-mmio window faults out here. Drive WHP's
            // instruction emulator to decode it and dispatch to the NIC; without a NIC there is
            // nothing to service, so report and stop rather than spin re-faulting.
            match (&emulator, &nic) {
                (Some(emu), Some(n)) => {
                    if let Err(e) = handle_mmio(emu, handle, &exit, n) {
                        run_err = Some(e);
                        break;
                    }
                    // A transmit notification (QueueNotify) may have raised the NIC's interrupt.
                    service_pending_irqs(
                        &timer_pending,
                        &nic,
                        &mut pic,
                        handle,
                        &mut prefer_timer,
                    );
                }
                _ => {
                    error!("unhandled guest MMIO access");
                    dump_vcpu(handle);
                    break;
                }
            }
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

    if let Some(nic) = nic.as_ref() {
        // Keep the pump alive while stopping a bounded backend so any final guest-bound frame can
        // drain instead of deadlocking a worker in delivery-aware backpressure.
        nic.backend.shutdown();
    }
    net_stop.store(true, Ordering::SeqCst);
    if let Some(pump) = net_pump.take() {
        let _ = pump.join();
    }
    if let Some(nic) = nic.as_ref() {
        let counters = nic.backend.counters();
        info!(
            "virt-net counters: tx accepted={}, tx dropped={}, rx received={}, rx dropped={}, errors={}, driver rx dropped={}, driver rx truncated={}, driver rx invalid={}, driver tx invalid={}",
            counters.guest_tx_accepted,
            counters.guest_tx_dropped,
            counters.guest_rx_received,
            counters.guest_rx_dropped,
            counters.backend_errors,
            counters.driver_rx_dropped,
            counters.driver_rx_truncated,
            counters.driver_rx_invalid_descriptors,
            counters.driver_tx_invalid_descriptors,
        );
    }
    console.lock().expect("console poisoned").flush();

    if let Some(err) = run_err {
        return Err(err);
    }

    if !cfg.timing_markers.is_empty() {
        let console = console.lock().expect("console poisoned");
        for (label, elapsed) in console.timings() {
            eprintln!("timing-marker: {label} {:.1} ms", elapsed.as_secs_f64() * 1000.0);
        }
    }

    // Report the boot time independently of the logging level so it is available even when all
    // logging is suppressed. On the restore path it is labelled "restore" (matching the KVM
    // backend), so benchmark scripts can distinguish a cold boot from a resume.
    if cfg.exit_on_boot {
        let console = console.lock().expect("console poisoned");
        if let Some(elapsed) = console.cold_start() {
            let label: &str = if cfg.restore.is_some() {
                "restore"
            } else {
                "cold-start"
            };
            eprintln!(
                "{label}: {:.1} ms to marker ({} console bytes emitted)",
                elapsed.as_secs_f64() * 1000.0,
                console.bytes_out()
            );
        }
    }
    Ok(())
}

/// Result of one guest PIO access.
struct PioResult {
    action: PioAction,
    pic_eoi: bool,
}

/// Serializes the full VM state when the guest requests a snapshot (control port 0x605). Returns
/// `Ok(true)` when the snapshot was written and the VM should stop; `Ok(false)` when snapshotting
/// was requested but not configured (so the guest keeps running).
// Capturing a consistent snapshot needs the whole VM context (config, partition, memory, the
// emulated devices, and the console/device bus); bundling them purely to reduce the argument
// count would not improve clarity.
#[allow(clippy::too_many_arguments)]
fn take_snapshot(
    cfg: &Config,
    handle: WHV_PARTITION_HANDLE,
    mem: &GuestMemory,
    pic: &Pic,
    pit: &Pit,
    rtc: &Rtc,
    console: &Arc<Mutex<Console>>,
    bus: &DeviceBus,
    nic: Option<&Nic>,
) -> Result<bool> {
    let dir = match &cfg.snapshot {
        Some(dir) => dir,
        None => {
            warn!("guest requested a snapshot but --snapshot was not given; ignoring");
            return Ok(false);
        }
    };
    let capture_start = Instant::now();

    // Flush any buffered guest output before capturing the console's pending input queue so the
    // restored VM neither loses emitted bytes nor replays already-consumed ones.
    console.lock().expect("console poisoned").flush();
    let con_state: Vec<u8> = bus.console().lock().expect("console poisoned").snapshot();
    let pic_bytes: Vec<u8> = pic.save();
    let pit_bytes: Vec<u8> = pit.save();
    let rtc_bytes: Vec<u8> = rtc.save();
    // The caller quiesced the frame backend and joined the RX pump before entering here.
    let net_state: Vec<u8> = match nic {
        Some(n) => {
            let dev = n.dev.lock().expect("virt-net poisoned");
            let mut s: Vec<u8> = n.snapshot_header.clone();
            s.extend(dev.save());
            s
        }
        None => Vec::new(),
    };
    let devices = snapshot::DeviceState {
        pic: &pic_bytes,
        pit: &pit_bytes,
        rtc: &rtc_bytes,
        console: &con_state,
        net: &net_state,
    };
    snapshot::write(dir, handle, mem, &devices)
        .with_context(|| format!("writing snapshot to {}", dir.display()))?;
    if !cfg.timing_markers.is_empty() {
        eprintln!("snapshot-capture: {:.1} ms", capture_start.elapsed().as_secs_f64() * 1000.0);
    }
    info!("snapshot written to {}", dir.display());
    Ok(true)
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
) -> Result<PioResult> {
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
    let mut pic_eoi = false;
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
            let mut eoi_vector: Option<u8> = None;
            for &b in &bytes[..size] {
                eoi_vector = pic.write(port, b).or(eoi_vector);
            }
            if let Some(vector) = eoi_vector {
                lapic_eoi(handle, vector);
                pic_eoi = true;
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
            &[(WHvX64RegisterRax, rax), (WHvX64RegisterRip, next_rip)],
        )?;
    }
    Ok(PioResult { action, pic_eoi })
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
        }
        // Leaf 0x16: nominal processor frequency information (MHz).
        0x0000_0016 => {
            let mhz: u32 = (tsc_hz / 1_000_000) as u32;
            eax = mhz;
            ebx = mhz;
            ecx = 100;
            edx = 0;
        }
        _ => {}
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

/// Performs a best-effort local-APIC end-of-interrupt for `vector` on the vCPU.
///
/// Under WHP's XApic emulation the individual APIC registers (including the EOI register) are
/// not reachable via `WHvSetVirtualProcessorRegisters`, and there is no ExtINT injection type,
/// so a virtual-wire IRQ0 delivered as a fixed vector would leave its in-service bit set with no
/// way for the guest (which issues only the 8259 EOI) to clear it. The APIC save/restore API is
/// reachable, though: read the local-APIC state, clear the exact vector just acknowledged by the
/// PIC, and write it back — the effect of an EOI without
/// accidentally clearing a concurrently in-service timer or NIC vector. Best-effort: any failure
/// is ignored.
fn legacy_apic_isr_offset(vector: u8) -> usize {
    const APIC_ISR: usize = 0x100;
    APIC_ISR + usize::from(vector / 32) * size_of::<u32>()
}

fn lapic_eoi(handle: WHV_PARTITION_HANDLE, vector: u8) {
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
    if got.is_err() || (written as usize) < legacy_apic_isr_offset(u8::MAX) + size_of::<u32>() {
        return;
    }

    let bit: u32 = u32::from(vector % 32);
    let off: usize = legacy_apic_isr_offset(vector);
    let mut word = u32::from_le_bytes([state[off], state[off + 1], state[off + 2], state[off + 3]]);
    if word & (1 << bit) == 0 {
        return;
    }
    word &= !(1 << bit);
    state[off..off + 4].copy_from_slice(&word.to_le_bytes());

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
    let values: Vec<WHV_REGISTER_VALUE> = pairs
        .iter()
        .map(|p| WHV_REGISTER_VALUE { Reg64: p.1 })
        .collect();
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
    timer_pending: Arc<AtomicBool>,
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
            timer_pending.store(true, Ordering::Release);
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
fn inject_irq0(pic: &mut Pic, handle: WHV_PARTITION_HANDLE) -> bool {
    let Some(vector) = pic.raise_irq0() else {
        return false;
    };
    let interrupt: WHV_INTERRUPT_CONTROL = WHV_INTERRUPT_CONTROL {
        // Type = Fixed, DestinationMode = Physical, TriggerMode = Edge (all zero).
        _bitfield: 0,
        Destination: 0,
        Vector: u32::from(vector),
    };
    // SAFETY: `interrupt` is a valid control block of the declared size; the partition is live.
    let result = unsafe {
        WHvRequestInterrupt(
            handle,
            &interrupt,
            size_of::<WHV_INTERRUPT_CONTROL>() as u32,
        )
    };
    if let Err(e) = result {
        pic.cancel_irq(0);
        error!("timer: WHvRequestInterrupt failed for IRQ0 vector {vector:#04x}: {e}");
        return false;
    }
    true
}

fn service_timer_irq(
    timer_pending: &AtomicBool,
    pic: &mut Pic,
    handle: WHV_PARTITION_HANDLE,
) -> bool {
    if !timer_pending.swap(false, Ordering::AcqRel) {
        return false;
    }
    if inject_irq0(pic, handle) {
        true
    } else {
        timer_pending.store(true, Ordering::Release);
        false
    }
}

/// Injects the NIC's interrupt (master-PIC line [`net::NET_IRQ`]) if the emulated PIC can deliver
/// it now. Runs on the vCPU thread; the guest's 8259 end-of-interrupt is completed with a matching
/// local-APIC EOI in [`handle_io`], exactly like the timer's IRQ0. When the line is masked (the
/// guest is mid-handler) the injection is skipped; the device keeps its `interrupt_status`
/// asserted and [`service_nic_irq`] retries on the next event/tick, so the notification is not lost.
fn inject_net_irq(pic: &mut Pic, handle: WHV_PARTITION_HANDLE) -> bool {
    let Some(vector) = pic.raise_irq(net::NET_IRQ as u8) else {
        return false;
    };
    let interrupt: WHV_INTERRUPT_CONTROL = WHV_INTERRUPT_CONTROL {
        _bitfield: 0,
        Destination: 0,
        Vector: u32::from(vector),
    };
    // SAFETY: `interrupt` is a valid control block of the declared size; the partition is live.
    let result = unsafe {
        WHvRequestInterrupt(
            handle,
            &interrupt,
            size_of::<WHV_INTERRUPT_CONTROL>() as u32,
        )
    };
    if let Err(e) = result {
        pic.cancel_irq(net::NET_IRQ as u8);
        error!(
            "virt-net: WHvRequestInterrupt failed for IRQ{} vector {vector:#04x}: {e}",
            net::NET_IRQ
        );
        return false;
    }
    true
}

/// Injects the NIC's IRQ if the device currently has an unacknowledged interrupt asserted. Called
/// on interrupt-relevant events (a receive-pump wake, a transmit notification, and each timer
/// tick) rather than every loop iteration, so it neither spins nor starves the receive pump of the
/// device lock.
fn service_nic_irq(nic: &Option<Nic>, pic: &mut Pic, handle: WHV_PARTITION_HANDLE) -> bool {
    if let Some(n) = nic
        && n.dev.lock().expect("virt-net poisoned").irq_asserted()
    {
        return inject_net_irq(pic, handle);
    }
    false
}

fn service_pending_irqs(
    timer_pending: &AtomicBool,
    nic: &Option<Nic>,
    pic: &mut Pic,
    handle: WHV_PARTITION_HANDLE,
    prefer_timer: &mut bool,
) {
    let timer_waiting = timer_pending.load(Ordering::Acquire);
    let nic_waiting = nic.as_ref().is_some_and(|n| {
        n.dev.lock().expect("virt-net poisoned").irq_asserted()
    });

    if timer_waiting && nic_waiting {
        if *prefer_timer {
            if service_timer_irq(timer_pending, pic, handle) {
                *prefer_timer = false;
            } else if service_nic_irq(nic, pic, handle) {
                *prefer_timer = true;
            }
        } else {
            if service_nic_irq(nic, pic, handle) {
                *prefer_timer = true;
            } else if service_timer_irq(timer_pending, pic, handle) {
                *prefer_timer = false;
            }
        }
    } else if timer_waiting {
        if service_timer_irq(timer_pending, pic, handle) {
            *prefer_timer = false;
        }
    } else if nic_waiting && service_nic_irq(nic, pic, handle) {
        *prefer_timer = true;
    }
}

/// Services a guest MMIO exit by emulating the faulting instruction against the NIC.
fn handle_mmio(
    emu: &Emulator,
    handle: WHV_PARTITION_HANDLE,
    exit: &WHV_RUN_VP_EXIT_CONTEXT,
    nic: &Nic,
) -> Result<()> {
    let vp: &WHV_VP_EXIT_CONTEXT = &exit.VpContext;
    // SAFETY: the exit reason selects the `MemoryAccess` arm of the union.
    let mmio: &WHV_MEMORY_ACCESS_CONTEXT = unsafe { &exit.Anonymous.MemoryAccess };
    let mut handler: NetMmio<'_> = NetMmio { nic };
    emu.emulate(handle, vp, mmio, &mut handler)
}

/// MMIO dispatcher used by the instruction emulator: routes accesses in the virtio-mmio window to
/// the NIC; other reads float to zero and other writes are dropped (the unoccupied-bus behaviour).
struct NetMmio<'a> {
    nic: &'a Nic,
}

impl MmioHandler for NetMmio<'_> {
    fn mmio(&mut self, gpa: u64, is_write: bool, data: &mut [u8]) {
        if (net::NET_MMIO_BASE..net::NET_MMIO_BASE + net::NET_MMIO_SIZE).contains(&gpa) {
            let off: u64 = gpa - net::NET_MMIO_BASE;
            let mut dev = self.nic.dev.lock().expect("virt-net poisoned");
            if is_write {
                dev.mmio_write(off, data);
            } else {
                dev.mmio_read(off, data);
            }
        } else if !is_write {
            data.iter_mut().for_each(|b| *b = 0);
        }
    }
}

/// Spawns the NIC receive pump: it drains NAT frames destined for the guest, scatters each into
/// the RX virtqueue, and wakes the vCPU loop (via a cross-thread-safe cancel) to inject the NIC's
/// IRQ. A frame that cannot be delivered yet (the RX ring has no free buffer) is held and retried
/// rather than dropped, so a TCP segment is never silently lost (there is no retransmission). It
/// is joined before snapshots and shutdown so its guest-memory writes and vCPU cancel cannot
/// outlive the protected VM state. Backend workers have an independent lifecycle.
fn spawn_net_rx(
    dev: Arc<Mutex<VirtioNet>>,
    backend: Arc<dyn FrameBackend>,
    handle: WHV_PARTITION_HANDLE,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // A frame accepted from the NAT but not yet deliverable (RX ring full); retried first.
        let mut pending: Option<Vec<u8>> = None;
        while !stop.load(Ordering::SeqCst) && backend.health() == net::BackendHealth::Ready {
            // Retry a previously-undeliverable frame before taking a new one.
            if let Some(frame) = pending.take() {
                if stop.load(Ordering::SeqCst) || backend.health() != net::BackendHealth::Ready {
                    break;
                }
                if dev.lock().expect("virt-net poisoned").process_rx(&frame) {
                    wake_vcpu(handle);
                } else {
                    pending = Some(frame);
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
            }
            if let Some(frame) = backend.recv_timeout(Duration::from_millis(20)) {
                if stop.load(Ordering::SeqCst) || backend.health() != net::BackendHealth::Ready {
                    break;
                }
                if dev.lock().expect("virt-net poisoned").process_rx(&frame) {
                    wake_vcpu(handle);
                } else {
                    pending = Some(frame);
                }
            }
        }
    })
}

/// Wakes the (possibly parked) vCPU so the run loop services a pending NIC interrupt. Only
/// `WHvCancelRunVirtualProcessor` is used, which is safe from another thread; the pump is joined
/// before the partition is dropped, so the handle stays valid.
fn wake_vcpu(handle: WHV_PARTITION_HANDLE) {
    // SAFETY: the partition is live until the receive pump is joined (before the partition drops).
    let result = unsafe { WHvCancelRunVirtualProcessor(handle, VP_INDEX, 0) };
    if let Err(e) = result {
        error!("virt-net: failed to wake vCPU after RX: {e}");
    }
}

/// Spawns a thread that forwards host stdin to the guest console.
///
/// It makes no WHP calls — it only enqueues bytes into the shared console device — so it can be
/// left detached without any partition-handle lifetime hazard. The guest observes the input on
/// its next `hvc` poll, which the timer tick keeps running; it does not need an explicit wake.
fn spawn_input_thread(
    con: Arc<Mutex<PortConsole>>,
    console: Arc<Mutex<Console>>,
    stop: Arc<AtomicBool>,
    defer_until_boot: bool,
) {
    thread::spawn(move || {
        if defer_until_boot {
            while !stop.load(Ordering::SeqCst)
                && !console.lock().expect("console poisoned").booted()
            {
                thread::sleep(Duration::from_millis(1));
            }
            if stop.load(Ordering::SeqCst) {
                return;
            }
            // The marker is emitted immediately before PID 1 execs the interactive shell.
            thread::sleep(Duration::from_millis(10));
        }

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
                }
            }
        }
    });
}

/// Measures the host TSC frequency (Hz) against the performance counter.
fn measure_tsc_hz() -> u64 {
    use ::windows::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
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

#[cfg(test)]
mod tests {
    use super::legacy_apic_isr_offset;

    #[test]
    fn legacy_lapic_state_packs_isr_words() {
        assert_eq!(legacy_apic_isr_offset(0), 0x100);
        assert_eq!(legacy_apic_isr_offset(31), 0x100);
        assert_eq!(legacy_apic_isr_offset(32), 0x104);
        assert_eq!(legacy_apic_isr_offset(255), 0x11c);
    }
}

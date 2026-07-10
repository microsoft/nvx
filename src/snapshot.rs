// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The set of processor/VM state captured here follows the approach of the Nanvix Micro-VM
// (uservm) KVM snapshot support, Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Snapshot / Restore
//!
//! Captures and restores the full state of the micro-VM: guest RAM, the vCPU register file
//! (GPRs, segments, FPU, XCRs, LAPIC, MP state, pending events, debug registers, and a set
//! of model-specific registers), the in-kernel interrupt controller and PIT, the KVM
//! paravirtual clock, the portb console device, and — when a NIC is attached (`--net`) — the
//! virt-net device's transport state so networking resumes across a restore.
//!
//! A snapshot is a directory containing two files:
//! - `mem.bin` — the raw contents of guest RAM (see [`GuestMemory::snapshot_ram`]);
//! - `state.bin` — the framed processor / device / VM state written by [`write`].
//!
//! For an SMP guest the snapshot records **one [`VcpuState`] per processor**; the capture is a
//! consistent VM-wide cut (every vCPU is quiesced out of `KVM_RUN` and the device threads are
//! stopped before any state is read — see `vmm::coordinate_snapshot`). Each vCPU's full extended
//! (`XSAVE`) register state is captured so AVX/AVX-512 contents survive a restore, and on restore
//! a single reference TSC is written to every processor so their timestamp counters stay in sync.
//!

use ::std::path::Path;

use ::anyhow::{
    Context,
    Result,
    bail,
};
use ::kvm_bindings::{
    Msrs,
    kvm_clock_data,
    kvm_debugregs,
    kvm_fpu,
    kvm_irqchip,
    kvm_lapic_state,
    kvm_mp_state,
    kvm_msr_entry,
    kvm_pit_state2,
    kvm_regs,
    kvm_sregs,
    kvm_vcpu_events,
    kvm_xcrs,
    kvm_xsave,
};
use ::kvm_ioctls::{
    VcpuFd,
    VmFd,
};

use crate::boot::params::as_bytes;
use crate::memory::GuestMemory;

/// Magic identifying a snapshot `state.bin` file ("MVMSNAP2"). Version 2 holds an array of
/// per-vCPU states (SMP) plus each vCPU's XSAVE region; version 1 (single-vCPU, no XSAVE) is no
/// longer produced or read.
const MAGIC: &[u8; 8] = b"MVMSNAP2";

/// IA32_TSC MSR index. Captured per vCPU but, on restore, overwritten with a single reference
/// value across all processors so their timestamp counters stay synchronized.
const IA32_TSC: u32 = 0x0000_0010;

/// IA32_TSC_DEADLINE MSR index. It is an *absolute* TSC value at which the local-APIC timer
/// fires, so it must be restored only after each vCPU's TSC has been synchronized — otherwise KVM
/// would arm the deadline against the fresh (near-zero) TSC and delay the first timer by roughly
/// the guest's pre-snapshot uptime.
const IA32_TSC_DEADLINE: u32 = 0x0000_06e0;

/// Size in bytes of the fixed `KVM_GET_XSAVE` region (`kvm_xsave.region`, `[u32; 1024]`).
const XSAVE_REGION_BYTES: usize = 4096;

/// KVM irqchip identifiers.
const KVM_IRQCHIP_PIC_MASTER: u32 = 0;
const KVM_IRQCHIP_PIC_SLAVE: u32 = 1;
const KVM_IRQCHIP_IOAPIC: u32 = 2;

/// Model-specific registers captured in a snapshot. These cover the state a booted Linux
/// guest keeps in MSRs: syscall entry points, FS/GS bases, SYSENTER, PAT, MISC_ENABLE, the
/// TSC and TSC-deadline, and the KVM paravirtual-clock/features MSRs.
const SNAPSHOT_MSRS: &[u32] = &[
    0x0000_0010, // IA32_TSC
    0x0000_0174, // IA32_SYSENTER_CS
    0x0000_0175, // IA32_SYSENTER_ESP
    0x0000_0176, // IA32_SYSENTER_EIP
    0x0000_01a0, // IA32_MISC_ENABLE
    0x0000_0277, // IA32_CR_PAT
    0x0000_06e0, // IA32_TSC_DEADLINE
    0xc000_0080, // EFER
    0xc000_0081, // STAR
    0xc000_0082, // LSTAR
    0xc000_0083, // CSTAR
    0xc000_0084, // SYSCALL_MASK (SFMASK)
    0xc000_0100, // FS_BASE
    0xc000_0101, // GS_BASE
    0xc000_0102, // KERNEL_GS_BASE
    0xc000_0103, // TSC_AUX
    0x4b56_4d00, // MSR_KVM_WALL_CLOCK_NEW
    0x4b56_4d01, // MSR_KVM_SYSTEM_TIME_NEW
    0x4b56_4d02, // MSR_KVM_ASYNC_PF_EN
    0x4b56_4d03, // MSR_KVM_STEAL_TIME
    0x4b56_4d04, // MSR_KVM_PV_EOI_EN
    0x4b56_4d06, // MSR_KVM_ASYNC_PF_INT
];

//==================================================================================================
// Framing helpers
//==================================================================================================

/// Appends a length-prefixed blob to `buf`.
fn put_blob(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend((bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// A cursor over a `state.bin` byte buffer.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end: usize = self.pos.checked_add(n).context("snapshot read overflow")?;
        let slice: &[u8] = self.data.get(self.pos..end).context("snapshot truncated")?;
        self.pos = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32> {
        let b: &[u8] = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b: &[u8] = self.take(8)?;
        let mut a: [u8; 8] = [0; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    fn blob(&mut self) -> Result<&'a [u8]> {
        let len: usize = self.u32()? as usize;
        self.take(len)
    }
}

/// Reads a POD value from `bytes` (which must be at least `size_of::<T>()` long).
fn read_pod<T: Copy>(bytes: &[u8]) -> Result<T> {
    if bytes.len() < size_of::<T>() {
        bail!("snapshot blob too small for {}", ::core::any::type_name::<T>());
    }
    // SAFETY: `T` is a `repr(C)` POD; we zero-initialise and copy exactly `size_of::<T>()`
    // bytes into it from a sufficiently large source.
    let mut value: T = unsafe { ::core::mem::zeroed() };
    unsafe {
        ::core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (&mut value as *mut T).cast::<u8>(),
            size_of::<T>(),
        );
    }
    Ok(value)
}

//==================================================================================================
// Per-vCPU state
//==================================================================================================

/// The full architectural state of a single virtual processor: the register files, LAPIC, MP
/// state, pending events, debug registers, TSC frequency, the captured model-specific registers,
/// and the extended (`XSAVE`) register region. One of these is captured per vCPU.
pub struct VcpuState {
    /// The vCPU's index (0 is the boot processor). Restored to the matching vCPU.
    id: u64,
    regs: kvm_regs,
    sregs: kvm_sregs,
    fpu: kvm_fpu,
    xcrs: kvm_xcrs,
    /// Extended state (`KVM_GET_XSAVE` region bytes; empty if the host did not provide it).
    xsave: Vec<u8>,
    lapic: kvm_lapic_state,
    mp_state: kvm_mp_state,
    vcpu_events: kvm_vcpu_events,
    debugregs: kvm_debugregs,
    tsc_khz: u32,
    msrs: Vec<(u32, u64)>,
}

impl VcpuState {
    /// Captures the current architectural state of `vcpu` (index `id`). The caller must have
    /// already quiesced the vCPU (it is out of `KVM_RUN` and no device or peer vCPU can mutate
    /// its state) so the capture is part of a consistent VM-wide cut.
    pub fn capture(vcpu: &VcpuFd, id: u64) -> Result<Self> {
        let regs = vcpu.get_regs().context("KVM_GET_REGS")?;
        let sregs = vcpu.get_sregs().context("KVM_GET_SREGS")?;
        let fpu = vcpu.get_fpu().context("KVM_GET_FPU")?;
        let xcrs = vcpu.get_xcrs().context("KVM_GET_XCRS")?;
        // Extended (AVX/AVX-512) register state. A capture failure is fatal rather than silently
        // producing an empty region that would corrupt the guest's vector registers on restore.
        let xsave_data = vcpu.get_xsave().context("KVM_GET_XSAVE")?;
        let xsave: Vec<u8> = as_bytes(&xsave_data.region).to_vec();
        let lapic = vcpu.get_lapic().context("KVM_GET_LAPIC")?;
        let mp_state = vcpu.get_mp_state().context("KVM_GET_MP_STATE")?;
        let vcpu_events = vcpu.get_vcpu_events().context("KVM_GET_VCPU_EVENTS")?;
        let debugregs = vcpu.get_debug_regs().context("KVM_GET_DEBUGREGS")?;
        let tsc_khz: u32 = vcpu.get_tsc_khz().unwrap_or(0);

        // Model-specific registers (read individually so an unsupported MSR does not truncate
        // the rest).
        let mut msrs: Vec<(u32, u64)> = Vec::new();
        for &index in SNAPSHOT_MSRS {
            let mut m: Msrs = Msrs::from_entries(&[kvm_msr_entry {
                index,
                ..Default::default()
            }])
            .context("building Msrs")?;
            if let Ok(1) = vcpu.get_msrs(&mut m) {
                let entry: kvm_msr_entry = m.as_slice()[0];
                msrs.push((entry.index, entry.data));
            }
        }

        Ok(Self {
            id,
            regs,
            sregs,
            fpu,
            xcrs,
            xsave,
            lapic,
            mp_state,
            vcpu_events,
            debugregs,
            tsc_khz,
            msrs,
        })
    }

    /// Serializes this vCPU state as a single length-prefixed blob into `buf`.
    fn serialize(&self, buf: &mut Vec<u8>) {
        let mut v: Vec<u8> = Vec::new();
        v.extend(self.id.to_le_bytes());
        put_blob(&mut v, as_bytes(&self.regs));
        put_blob(&mut v, as_bytes(&self.sregs));
        put_blob(&mut v, as_bytes(&self.fpu));
        put_blob(&mut v, as_bytes(&self.xcrs));
        put_blob(&mut v, &self.xsave);
        put_blob(&mut v, as_bytes(&self.lapic));
        put_blob(&mut v, as_bytes(&self.mp_state));
        put_blob(&mut v, as_bytes(&self.vcpu_events));
        put_blob(&mut v, as_bytes(&self.debugregs));
        v.extend(self.tsc_khz.to_le_bytes());
        v.extend((self.msrs.len() as u32).to_le_bytes());
        for (index, data) in &self.msrs {
            v.extend(index.to_le_bytes());
            v.extend(data.to_le_bytes());
        }
        put_blob(buf, &v);
    }

    /// Parses a vCPU state from the blob written by [`VcpuState::serialize`].
    fn parse(bytes: &[u8]) -> Result<Self> {
        let mut r: Reader<'_> = Reader::new(bytes);
        let id: u64 = r.u64()?;
        let regs: kvm_regs = read_pod(r.blob()?)?;
        let sregs: kvm_sregs = read_pod(r.blob()?)?;
        let fpu: kvm_fpu = read_pod(r.blob()?)?;
        let xcrs: kvm_xcrs = read_pod(r.blob()?)?;
        let xsave: Vec<u8> = r.blob()?.to_vec();
        let lapic: kvm_lapic_state = read_pod(r.blob()?)?;
        let mp_state: kvm_mp_state = read_pod(r.blob()?)?;
        let vcpu_events: kvm_vcpu_events = read_pod(r.blob()?)?;
        let debugregs: kvm_debugregs = read_pod(r.blob()?)?;
        let tsc_khz: u32 = r.u32()?;
        let n_msrs: u32 = r.u32()?;
        let mut msrs: Vec<(u32, u64)> = Vec::with_capacity(n_msrs as usize);
        for _ in 0..n_msrs {
            msrs.push((r.u32()?, r.u64()?));
        }
        Ok(Self {
            id,
            regs,
            sregs,
            fpu,
            xcrs,
            xsave,
            lapic,
            mp_state,
            vcpu_events,
            debugregs,
            tsc_khz,
            msrs,
        })
    }

    /// Returns the vCPU's captured `IA32_TSC` value, if present.
    fn tsc(&self) -> Option<u64> {
        self.msrs.iter().find(|(i, _)| *i == IA32_TSC).map(|(_, d)| *d)
    }

    /// Returns the vCPU's captured `IA32_TSC_DEADLINE` value, if present.
    fn tsc_deadline(&self) -> Option<u64> {
        self.msrs.iter().find(|(i, _)| *i == IA32_TSC_DEADLINE).map(|(_, d)| *d)
    }

    /// Applies this state to `vcpu`. CPUID must already have been programmed (via `Vcpu::new`).
    /// `IA32_TSC` and `IA32_TSC_DEADLINE` are intentionally **not** written here: the former is
    /// synchronized across all vCPUs by [`Snapshot::sync_tsc`] and the latter is armed by
    /// [`Snapshot::arm_tsc_deadlines`], both only after every processor has been restored.
    fn apply(&self, vcpu: &VcpuFd) -> Result<()> {
        // Register files first.
        vcpu.set_sregs(&self.sregs).context("KVM_SET_SREGS")?;
        vcpu.set_regs(&self.regs).context("KVM_SET_REGS")?;
        vcpu.set_fpu(&self.fpu).context("KVM_SET_FPU")?;
        // Extended state, after CR4.OSXSAVE (set_sregs) and XCR0 (set_xcrs) are in place. These
        // are hard failures: a silently dropped XCR0/XSAVE would leave the guest's AVX/AVX-512
        // registers corrupted while restore reported success.
        vcpu.set_xcrs(&self.xcrs).context("KVM_SET_XCRS")?;
        if self.xsave.len() < XSAVE_REGION_BYTES {
            bail!("snapshot xsave region is {} bytes (expected {XSAVE_REGION_BYTES})", self.xsave.len());
        }
        let mut xsave: kvm_xsave = kvm_xsave::default();
        // SAFETY: `xsave.region` is `[u32; 1024]` (XSAVE_REGION_BYTES); we copy exactly that many
        // bytes into it from a sufficiently large source, then hand it to KVM_SET_XSAVE.
        unsafe {
            ::core::ptr::copy_nonoverlapping(
                self.xsave.as_ptr(),
                xsave.region.as_mut_ptr().cast::<u8>(),
                XSAVE_REGION_BYTES,
            );
            vcpu.set_xsave(&xsave).context("KVM_SET_XSAVE")?;
        }

        // Restore the LAPIC (and MP state) *before* the MSRs: KVM only honors an
        // IA32_TSC_DEADLINE write once the local timer's LVTT is already in TSC-deadline mode, so
        // the interrupt controller must be in place first or the saved per-CPU timer is left
        // unarmed. This ordering matches cloud-hypervisor.
        vcpu.set_lapic(&self.lapic).context("KVM_SET_LAPIC")?;
        vcpu.set_mp_state(self.mp_state).context("KVM_SET_MP_STATE")?;

        // TSC frequency before the MSRs that depend on the timebase. A failure here is surfaced
        // (a wrong TSC frequency silently corrupts guest timekeeping) rather than discarded.
        if self.tsc_khz != 0 {
            vcpu.set_tsc_khz(self.tsc_khz).context("KVM_SET_TSC_KHZ")?;
        }

        // Model-specific registers. Skip the two timebase MSRs handled separately across all
        // vCPUs: IA32_TSC (synchronized by `Snapshot::sync_tsc`) and IA32_TSC_DEADLINE (an
        // absolute TSC value armed by `Snapshot::arm_tsc_deadlines` only after the TSC is set).
        for &(index, data) in &self.msrs {
            if index == IA32_TSC || index == IA32_TSC_DEADLINE {
                continue;
            }
            let msrs: Msrs = Msrs::from_entries(&[kvm_msr_entry {
                index,
                data,
                ..Default::default()
            }])
            .context("building Msrs")?;
            // Best effort: an incidental MSR the host will not accept must not abort the restore
            // (the timebase-critical MSRs above are checked explicitly).
            let _ = vcpu.set_msrs(&msrs);
        }

        vcpu.set_vcpu_events(&self.vcpu_events).context("KVM_SET_VCPU_EVENTS")?;
        vcpu.set_debug_regs(&self.debugregs).context("KVM_SET_DEBUGREGS")?;
        Ok(())
    }
}

//==================================================================================================
// Save
//==================================================================================================

///
/// # Description
///
/// Writes a complete snapshot of the VM to the directory `dir`. `states` holds one captured
/// [`VcpuState`] per processor (element 0 is the boot processor); the caller must have taken a
/// consistent VM-wide cut (all vCPUs quiesced and device threads stopped) before capturing them.
///
pub fn write(
    dir: &Path,
    states: &[VcpuState],
    vm: &VmFd,
    mem: &GuestMemory,
    con_state: &[u8],
    net_state: &[u8],
) -> Result<()> {
    if states.is_empty() {
        bail!("snapshot requires at least one vCPU state");
    }
    ::std::fs::create_dir_all(dir).with_context(|| format!("creating snapshot dir {dir:?}"))?;

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.extend(mem.ram_size().to_le_bytes());

    // Per-vCPU architectural state (one blob each).
    buf.extend((states.len() as u32).to_le_bytes());
    for state in states {
        state.serialize(&mut buf);
    }

    // KVM paravirtual clock (VM-wide).
    put_blob(&mut buf, as_bytes(&vm.get_clock().context("KVM_GET_CLOCK")?));

    // In-kernel interrupt controller (PIC master/slave, IOAPIC).
    for id in [KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE, KVM_IRQCHIP_IOAPIC] {
        let mut chip: kvm_irqchip = kvm_irqchip {
            chip_id: id,
            ..Default::default()
        };
        vm.get_irqchip(&mut chip).context("KVM_GET_IRQCHIP")?;
        put_blob(&mut buf, as_bytes(&chip));
    }

    // Programmable interval timer.
    put_blob(&mut buf, as_bytes(&vm.get_pit2().context("KVM_GET_PIT2")?));

    // portb console device (pending input queue).
    put_blob(&mut buf, con_state);

    // virt-net device state (empty unless a NIC was attached).
    put_blob(&mut buf, net_state);

    // Persist RAM and state.
    mem.snapshot_ram(&dir.join("mem.bin"))?;
    ::std::fs::write(dir.join("state.bin"), &buf).context("writing state.bin")?;
    Ok(())
}

//==================================================================================================
// Load
//==================================================================================================

/// A snapshot read from disk, ready to be applied to a freshly created VM/vCPUs.
pub struct Snapshot {
    ram_size: u64,
    vcpus: Vec<VcpuState>,
    clock: kvm_clock_data,
    irqchips: [kvm_irqchip; 3],
    pit: kvm_pit_state2,
    con_state: Vec<u8>,
    net_state: Vec<u8>,
}

impl Snapshot {
    /// Reads and parses the `state.bin` of a snapshot directory.
    pub fn read(dir: &Path) -> Result<Self> {
        let data: Vec<u8> =
            ::std::fs::read(dir.join("state.bin")).context("reading state.bin")?;
        let mut r: Reader<'_> = Reader::new(&data);

        if r.take(8)? != MAGIC {
            bail!("not a microvm snapshot (bad magic)");
        }
        let ram_size: u64 = r.u64()?;

        // Per-vCPU architectural state.
        let n_vcpus: u32 = r.u32()?;
        if n_vcpus == 0 {
            bail!("snapshot records no vCPUs");
        }
        let mut vcpus: Vec<VcpuState> = Vec::with_capacity(n_vcpus as usize);
        for _ in 0..n_vcpus {
            vcpus.push(VcpuState::parse(r.blob()?)?);
        }

        let clock: kvm_clock_data = read_pod(r.blob()?)?;
        let irqchips: [kvm_irqchip; 3] = [
            read_pod(r.blob()?)?,
            read_pod(r.blob()?)?,
            read_pod(r.blob()?)?,
        ];
        let pit: kvm_pit_state2 = read_pod(r.blob()?)?;
        let con_state: Vec<u8> = r.blob()?.to_vec();
        let net_state: Vec<u8> = r.blob()?.to_vec();

        Ok(Self {
            ram_size,
            vcpus,
            clock,
            irqchips,
            pit,
            con_state,
            net_state,
        })
    }

    /// Returns the guest RAM size recorded in the snapshot.
    pub fn ram_size(&self) -> u64 {
        self.ram_size
    }

    /// Returns the number of processors recorded in the snapshot.
    pub fn vcpu_count(&self) -> usize {
        self.vcpus.len()
    }

    /// Returns the serialized portb console device state (pending input queue).
    pub fn con_state(&self) -> &[u8] {
        &self.con_state
    }

    /// Returns the serialized virt-net device state (empty if the VM had no NIC).
    pub fn net_state(&self) -> &[u8] {
        &self.net_state
    }

    /// Applies the VM-wide state (irqchip, PIT, clock) to `vm`. The irqchip and PIT must
    /// already have been created.
    pub fn apply_vm(&self, vm: &VmFd) -> Result<()> {
        for chip in &self.irqchips {
            vm.set_irqchip(chip).context("KVM_SET_IRQCHIP")?;
        }
        vm.set_pit2(&self.pit).context("KVM_SET_PIT2")?;

        // Restore only the clock value; leave flags at zero so KVM does not expect an
        // accompanying real-time field.
        let clock: kvm_clock_data = kvm_clock_data {
            clock: self.clock.clock,
            ..Default::default()
        };
        vm.set_clock(&clock).context("KVM_SET_CLOCK")?;
        Ok(())
    }

    /// Applies the `index`-th processor's state to `vcpu`. CPUID must already have been
    /// programmed (via `Vcpu::new`). `IA32_TSC` is written later by [`Snapshot::sync_tsc`].
    pub fn apply_vcpu(&self, vcpu: &VcpuFd, index: usize) -> Result<()> {
        let state: &VcpuState = self
            .vcpus
            .get(index)
            .with_context(|| format!("snapshot has no vCPU {index}"))?;
        state.apply(vcpu)
    }

    /// Synchronizes the timestamp counter across all restored vCPUs by writing the boot
    /// processor's captured `IA32_TSC` to every processor. Because the writes happen back to back
    /// from a single thread, KVM aligns their TSC offsets, so a task migrating between processors
    /// never observes time moving backwards (matching cloud-hypervisor's TSC synchronization).
    /// `vcpus` must be ordered to match the snapshot's processor indices. A failed or short write
    /// is surfaced rather than leaving the CPUs unsynchronized.
    pub fn sync_tsc(&self, vcpus: &[&VcpuFd]) -> Result<()> {
        let Some(reference) = self.vcpus.first().and_then(VcpuState::tsc) else {
            return Ok(());
        };
        for (idx, vcpu) in vcpus.iter().enumerate() {
            let msrs: Msrs = Msrs::from_entries(&[kvm_msr_entry {
                index: IA32_TSC,
                data: reference,
                ..Default::default()
            }])
            .context("building IA32_TSC Msrs")?;
            let written = vcpu.set_msrs(&msrs).context("KVM_SET_MSRS(IA32_TSC)")?;
            if written != 1 {
                bail!("failed to synchronize IA32_TSC on vCPU {idx} (wrote {written}/1)");
            }
        }
        Ok(())
    }

    /// Arms each restored vCPU's local-APIC TSC-deadline timer by writing its captured
    /// `IA32_TSC_DEADLINE`. Must be called **after** [`Snapshot::sync_tsc`] so the absolute
    /// deadline is compared against the correct (restored) TSC rather than a near-zero one, which
    /// would otherwise delay the first timer by roughly the guest's pre-snapshot uptime. `vcpus`
    /// must be ordered to match the snapshot's processor indices; a failed write is surfaced.
    pub fn arm_tsc_deadlines(&self, vcpus: &[&VcpuFd]) -> Result<()> {
        for (idx, vcpu) in vcpus.iter().enumerate() {
            let Some(deadline) = self.vcpus.get(idx).and_then(VcpuState::tsc_deadline) else {
                continue;
            };
            let msrs: Msrs = Msrs::from_entries(&[kvm_msr_entry {
                index: IA32_TSC_DEADLINE,
                data: deadline,
                ..Default::default()
            }])
            .context("building IA32_TSC_DEADLINE Msrs")?;
            let written = vcpu.set_msrs(&msrs).context("KVM_SET_MSRS(IA32_TSC_DEADLINE)")?;
            if written != 1 {
                bail!("failed to arm IA32_TSC_DEADLINE on vCPU {idx} (wrote {written}/1)");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_round_trips_through_reader() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend(0x1234_5678_9abc_def0_u64.to_le_bytes());
        put_blob(&mut buf, &[1, 2, 3, 4]);
        put_blob(&mut buf, b"");
        put_blob(&mut buf, &[0xaa; 40]);

        let mut r = Reader::new(&buf);
        assert_eq!(r.take(8).unwrap(), MAGIC);
        assert_eq!(r.u64().unwrap(), 0x1234_5678_9abc_def0);
        assert_eq!(r.blob().unwrap(), &[1, 2, 3, 4]);
        assert_eq!(r.blob().unwrap(), b"");
        assert_eq!(r.blob().unwrap(), &[0xaa; 40][..]);
    }

    #[test]
    fn reader_rejects_truncated_blob() {
        // Claims a 16-byte blob but provides only 4 bytes.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend(16u32.to_le_bytes());
        buf.extend_from_slice(&[0; 4]);
        let mut r = Reader::new(&buf);
        assert!(r.blob().is_err());
    }

    #[test]
    fn read_pod_rejects_short_input() {
        let bytes = [0u8; 3];
        assert!(read_pod::<u64>(&bytes).is_err());
    }

    #[test]
    fn vcpu_state_round_trips_through_serialize() {
        let mut regs = kvm_regs::default();
        regs.rip = 0xdead_beef;
        let state = VcpuState {
            id: 3,
            regs,
            sregs: kvm_sregs::default(),
            fpu: kvm_fpu::default(),
            xcrs: kvm_xcrs::default(),
            xsave: vec![0xab; XSAVE_REGION_BYTES],
            lapic: kvm_lapic_state::default(),
            mp_state: kvm_mp_state::default(),
            vcpu_events: kvm_vcpu_events::default(),
            debugregs: kvm_debugregs::default(),
            tsc_khz: 2_400_000,
            msrs: vec![(IA32_TSC, 0x1234), (0xc000_0080, 0xd01)],
        };

        let mut buf: Vec<u8> = Vec::new();
        state.serialize(&mut buf);
        // `serialize` writes a single length-prefixed blob; unwrap it before parsing.
        let mut r = Reader::new(&buf);
        let parsed = VcpuState::parse(r.blob().unwrap()).unwrap();

        assert_eq!(parsed.id, 3);
        assert_eq!(parsed.regs.rip, 0xdead_beef);
        assert_eq!(parsed.tsc_khz, 2_400_000);
        assert_eq!(parsed.xsave.len(), XSAVE_REGION_BYTES);
        assert_eq!(parsed.tsc(), Some(0x1234));
        assert_eq!(parsed.msrs.len(), 2);
    }
}

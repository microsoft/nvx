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
//! paravirtual clock, and the emulated serial device.
//!
//! A snapshot is a directory containing two files:
//! - `mem.bin` — the raw contents of guest RAM (see [`GuestMemory::snapshot_ram`]);
//! - `state.bin` — the framed processor / device / VM state written by [`write`].
//!
//! Snapshots are taken at a guest **syscall boundary** (the guest triggers one by writing a
//! control port from userspace). At that point the vector registers are not live, so
//! restoring only the legacy FXSAVE area via `KVM_SET_FPU` — `KVM_SET_XSAVE` is not exposed
//! by `kvm-ioctls` — is sufficient.
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
};
use ::kvm_ioctls::{
    VcpuFd,
    VmFd,
};

use crate::boot::params::as_bytes;
use crate::memory::GuestMemory;

/// Magic identifying a snapshot `state.bin` file ("MVMSNAP1").
const MAGIC: &[u8; 8] = b"MVMSNAP1";

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
// Save
//==================================================================================================

///
/// # Description
///
/// Writes a complete snapshot of the VM to the directory `dir`.
///
pub fn write(
    dir: &Path,
    vcpu: &VcpuFd,
    vm: &VmFd,
    mem: &GuestMemory,
    serial_state: &[u8],
) -> Result<()> {
    ::std::fs::create_dir_all(dir).with_context(|| format!("creating snapshot dir {dir:?}"))?;

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.extend(mem.ram_size().to_le_bytes());

    // vCPU register state.
    put_blob(&mut buf, as_bytes(&vcpu.get_regs().context("KVM_GET_REGS")?));
    put_blob(&mut buf, as_bytes(&vcpu.get_sregs().context("KVM_GET_SREGS")?));
    put_blob(&mut buf, as_bytes(&vcpu.get_fpu().context("KVM_GET_FPU")?));
    put_blob(&mut buf, as_bytes(&vcpu.get_xcrs().context("KVM_GET_XCRS")?));
    put_blob(&mut buf, as_bytes(&vcpu.get_lapic().context("KVM_GET_LAPIC")?));
    put_blob(&mut buf, as_bytes(&vcpu.get_mp_state().context("KVM_GET_MP_STATE")?));
    put_blob(&mut buf, as_bytes(&vcpu.get_vcpu_events().context("KVM_GET_VCPU_EVENTS")?));
    put_blob(&mut buf, as_bytes(&vcpu.get_debug_regs().context("KVM_GET_DEBUGREGS")?));

    // TSC frequency (best effort).
    let tsc_khz: u32 = vcpu.get_tsc_khz().unwrap_or(0);
    buf.extend(tsc_khz.to_le_bytes());

    // Model-specific registers (read individually so an unsupported MSR does not truncate
    // the rest).
    let mut msr_pairs: Vec<(u32, u64)> = Vec::new();
    for &index in SNAPSHOT_MSRS {
        let mut msrs: Msrs = Msrs::from_entries(&[kvm_msr_entry {
            index,
            ..Default::default()
        }])
        .context("building Msrs")?;
        if let Ok(1) = vcpu.get_msrs(&mut msrs) {
            let entry: kvm_msr_entry = msrs.as_slice()[0];
            msr_pairs.push((entry.index, entry.data));
        }
    }
    buf.extend((msr_pairs.len() as u32).to_le_bytes());
    for (index, data) in &msr_pairs {
        buf.extend(index.to_le_bytes());
        buf.extend(data.to_le_bytes());
    }

    // KVM paravirtual clock.
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

    // Emulated serial device.
    put_blob(&mut buf, serial_state);

    // Persist RAM and state.
    mem.snapshot_ram(&dir.join("mem.bin"))?;
    ::std::fs::write(dir.join("state.bin"), &buf).context("writing state.bin")?;
    Ok(())
}

//==================================================================================================
// Load
//==================================================================================================

/// A snapshot read from disk, ready to be applied to a freshly created VM/vCPU.
pub struct Snapshot {
    ram_size: u64,
    regs: kvm_regs,
    sregs: kvm_sregs,
    fpu: kvm_fpu,
    xcrs: kvm_xcrs,
    lapic: kvm_lapic_state,
    mp_state: kvm_mp_state,
    vcpu_events: kvm_vcpu_events,
    debugregs: kvm_debugregs,
    tsc_khz: u32,
    msrs: Vec<(u32, u64)>,
    clock: kvm_clock_data,
    irqchips: [kvm_irqchip; 3],
    pit: kvm_pit_state2,
    serial_state: Vec<u8>,
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
        let regs: kvm_regs = read_pod(r.blob()?)?;
        let sregs: kvm_sregs = read_pod(r.blob()?)?;
        let fpu: kvm_fpu = read_pod(r.blob()?)?;
        let xcrs: kvm_xcrs = read_pod(r.blob()?)?;
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

        let clock: kvm_clock_data = read_pod(r.blob()?)?;
        let irqchips: [kvm_irqchip; 3] = [
            read_pod(r.blob()?)?,
            read_pod(r.blob()?)?,
            read_pod(r.blob()?)?,
        ];
        let pit: kvm_pit_state2 = read_pod(r.blob()?)?;
        let serial_state: Vec<u8> = r.blob()?.to_vec();

        Ok(Self {
            ram_size,
            regs,
            sregs,
            fpu,
            xcrs,
            lapic,
            mp_state,
            vcpu_events,
            debugregs,
            tsc_khz,
            msrs,
            clock,
            irqchips,
            pit,
            serial_state,
        })
    }

    /// Returns the guest RAM size recorded in the snapshot.
    pub fn ram_size(&self) -> u64 {
        self.ram_size
    }

    /// Returns the serialized serial-device state.
    pub fn serial_state(&self) -> &[u8] {
        &self.serial_state
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

    /// Applies the processor state to `vcpu`. CPUID must already have been programmed.
    pub fn apply_vcpu(&self, vcpu: &VcpuFd) -> Result<()> {
        // MSRs first (EFER etc.), then the register files.
        for &(index, data) in &self.msrs {
            let msrs: Msrs = Msrs::from_entries(&[kvm_msr_entry {
                index,
                data,
                ..Default::default()
            }])
            .context("building Msrs")?;
            // Best effort: an MSR the host will not accept must not abort the restore.
            let _ = vcpu.set_msrs(&msrs);
        }

        if self.tsc_khz != 0 {
            let _ = vcpu.set_tsc_khz(self.tsc_khz);
        }

        vcpu.set_sregs(&self.sregs).context("KVM_SET_SREGS")?;
        vcpu.set_regs(&self.regs).context("KVM_SET_REGS")?;
        vcpu.set_fpu(&self.fpu).context("KVM_SET_FPU")?;
        let _ = vcpu.set_xcrs(&self.xcrs);
        vcpu.set_lapic(&self.lapic).context("KVM_SET_LAPIC")?;
        vcpu.set_mp_state(self.mp_state).context("KVM_SET_MP_STATE")?;
        vcpu.set_vcpu_events(&self.vcpu_events).context("KVM_SET_VCPU_EVENTS")?;
        vcpu.set_debug_regs(&self.debugregs).context("KVM_SET_DEBUGREGS")?;
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
}

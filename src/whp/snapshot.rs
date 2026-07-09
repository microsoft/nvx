// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The set of processor/VM state captured here follows the approach of the KVM snapshot support
// of the Nanvix Micro-VM (uservm), Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Snapshot / Restore (WHP)
//!
//! Captures and restores the full state of the WHP micro-VM: guest RAM, the vCPU register file
//! (GPRs, segments, tables, control/debug registers, and the model-specific registers a booted
//! Linux keeps), the FPU/SSE/AVX/CET register file (as an XSAVE area), the emulated local-APIC
//! state, and the emulated legacy devices (8259 PIC, i8253 PIT, MC146818 RTC, and the portb
//! console's pending input queue).
//!
//! A snapshot is a directory containing two files:
//! - `mem.bin` — the raw contents of guest RAM (see [`GuestMemory::snapshot_ram`]);
//! - `state.bin` — the framed processor / APIC / device state written by [`write`].
//!
//! Snapshots are taken at a guest **syscall boundary**: the guest requests one by writing the
//! control port `0x605` from userspace, the VMM services that `out` (advancing `RIP` past it),
//! then captures state — so a restore resumes on the instruction *after* the request.
//!

use ::std::path::Path;

use ::anyhow::{
    Context,
    Result,
    bail,
};
use ::windows::Win32::System::Hypervisor::{
    WHV_PARTITION_HANDLE,
    WHV_REGISTER_NAME,
    WHV_REGISTER_VALUE,
    WHvGetVirtualProcessorInterruptControllerState,
    WHvGetVirtualProcessorRegisters,
    WHvGetVirtualProcessorXsaveState,
    WHvSetVirtualProcessorInterruptControllerState,
    WHvSetVirtualProcessorRegisters,
    WHvSetVirtualProcessorXsaveState,
};

use crate::whp::memory::GuestMemory;

/// Index of the single guest virtual processor.
const VP_INDEX: u32 = 0;

/// Magic identifying a WHP snapshot `state.bin` file ("WHPSNAP1").
const MAGIC: &[u8; 8] = b"WHPSNAP1";

/// Upper bound on the guest's XSAVE-area size, in bytes. The state actually captured (x87, SSE,
/// AVX, and the CET components this guest enables) is well under 1 KiB; this leaves generous room.
const XSAVE_MAX: usize = 8192;

/// The set of virtual-processor registers captured in a snapshot, as raw `WHV_REGISTER_NAME`
/// values. The ranges are contiguous in the WHP enumeration:
/// - `0..=39`: GPRs + RIP/RFLAGS, the segment and table registers, CR0/CR2/CR3/CR4/CR8,
///   DR0-3/DR6/DR7, and XCR0;
/// - `0x2000..=0x200B`: TSC, EFER, KERNEL_GS_BASE, APIC_BASE, PAT, SYSENTER_{CS,EIP,ESP},
///   STAR/LSTAR/CSTAR/SFMASK; plus TSC_AUX (`WHvX64RegisterTscAux` == `0x207B`) and IA32_XSS
///   (`WHvX64RegisterXss` == `0x208B`, which enables the supervisor XSAVE components — the guest
///   restores the CET_S component with XRSTORS, which `#GP`s unless IA32_XSS enables it).
///
/// The FPU/SSE/AVX/CET register file is **not** captured here: those components are saved and
/// restored as one consistent XSAVE area via [`WHvGetVirtualProcessorXsaveState`] /
/// [`WHvSetVirtualProcessorXsaveState`], which preserves the extended state (AVX YMM-high and the
/// control-flow components) that a per-register capture would drop — reproducing which caused the
/// restored guest to `#GP` inside `restore_fpregs_from_fpstate` (XRSTOR).
fn snapshot_reg_names() -> Vec<WHV_REGISTER_NAME> {
    let mut names: Vec<WHV_REGISTER_NAME> = Vec::new();
    for code in 0..=39 {
        names.push(WHV_REGISTER_NAME(code));
    }
    for code in 0x2000..=0x200b {
        names.push(WHV_REGISTER_NAME(code));
    }
    names.push(WHV_REGISTER_NAME(0x207b));
    names.push(WHV_REGISTER_NAME(0x208b));
    names
}

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

    fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
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

/// Returns the raw 16 bytes of a register value.
fn reg_bytes(value: &WHV_REGISTER_VALUE) -> [u8; 16] {
    // SAFETY: `WHV_REGISTER_VALUE` is a 16-byte `repr(C)` POD union.
    unsafe { *(value as *const WHV_REGISTER_VALUE).cast::<[u8; 16]>() }
}

/// Reconstructs a register value from its raw 16 bytes.
fn reg_from_bytes(bytes: &[u8]) -> Result<WHV_REGISTER_VALUE> {
    if bytes.len() < 16 {
        bail!("snapshot register value is truncated");
    }
    let mut value: WHV_REGISTER_VALUE = WHV_REGISTER_VALUE::default();
    // SAFETY: `value` is a 16-byte POD and `bytes` is at least 16 bytes long.
    unsafe {
        ::core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (&mut value as *mut WHV_REGISTER_VALUE).cast::<u8>(),
            16,
        );
    }
    Ok(value)
}

//==================================================================================================
// Save
//==================================================================================================

/// Serialized emulated-device state captured alongside the processor state.
pub struct DeviceState<'a> {
    pub pic: &'a [u8],
    pub pit: &'a [u8],
    pub rtc: &'a [u8],
    pub console: &'a [u8],
}

///
/// # Description
///
/// Writes a complete snapshot of the VM to the directory `dir`.
///
pub fn write(
    dir: &Path,
    handle: WHV_PARTITION_HANDLE,
    mem: &GuestMemory,
    devices: &DeviceState<'_>,
) -> Result<()> {
    ::std::fs::create_dir_all(dir).with_context(|| format!("creating snapshot dir {dir:?}"))?;

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.extend(mem.ram_size().to_le_bytes());

    // vCPU register file.
    let names: Vec<WHV_REGISTER_NAME> = snapshot_reg_names();
    let mut values: Vec<WHV_REGISTER_VALUE> = vec![WHV_REGISTER_VALUE::default(); names.len()];
    // SAFETY: `names`/`values` are equal-length arrays valid for the call.
    unsafe {
        WHvGetVirtualProcessorRegisters(
            handle,
            VP_INDEX,
            names.as_ptr(),
            names.len() as u32,
            values.as_mut_ptr(),
        )
        .context("WHvGetVirtualProcessorRegisters (snapshot) failed")?;
    }
    buf.extend((names.len() as u32).to_le_bytes());
    for (name, value) in names.iter().zip(&values) {
        buf.extend(name.0.to_le_bytes());
        buf.extend_from_slice(&reg_bytes(value));
    }

    // FPU/SSE/AVX/CET register file, as one XSAVE area. Capturing it whole (rather than as
    // individual XMM/MMX registers) keeps the extended components consistent with the guest's
    // XCR0 so the restored guest's XRSTOR does not fault.
    let mut xsave: Vec<u8> = vec![0; XSAVE_MAX];
    let mut xsave_written: u32 = 0;
    // SAFETY: `xsave` is a writable buffer of `XSAVE_MAX` bytes; `xsave_written` receives the
    // number of bytes actually produced.
    unsafe {
        WHvGetVirtualProcessorXsaveState(
            handle,
            VP_INDEX,
            xsave.as_mut_ptr().cast::<::core::ffi::c_void>(),
            xsave.len() as u32,
            &mut xsave_written,
        )
        .context("WHvGetVirtualProcessorXsaveState (snapshot) failed")?;
    }
    put_blob(&mut buf, &xsave[..xsave_written as usize]);

    // Emulated local-APIC state.
    let mut apic: [u8; 4096] = [0; 4096];
    let mut written: u32 = 0;
    // SAFETY: `apic` is a 4096-byte buffer; `written` receives the actual size.
    unsafe {
        WHvGetVirtualProcessorInterruptControllerState(
            handle,
            VP_INDEX,
            apic.as_mut_ptr().cast::<::core::ffi::c_void>(),
            apic.len() as u32,
            Some(&mut written),
        )
        .context("WHvGetVirtualProcessorInterruptControllerState (snapshot) failed")?;
    }
    put_blob(&mut buf, &apic[..written as usize]);

    // Emulated legacy devices.
    put_blob(&mut buf, devices.pic);
    put_blob(&mut buf, devices.pit);
    put_blob(&mut buf, devices.rtc);
    put_blob(&mut buf, devices.console);

    // Persist RAM and state.
    mem.snapshot_ram(&dir.join("mem.bin"))?;
    ::std::fs::write(dir.join("state.bin"), &buf).context("writing state.bin")?;
    Ok(())
}

//==================================================================================================
// Load
//==================================================================================================

/// A snapshot read from disk, ready to be applied to a freshly created partition and vCPU.
pub struct Snapshot {
    ram_size: u64,
    regs: Vec<(WHV_REGISTER_NAME, WHV_REGISTER_VALUE)>,
    xsave: Vec<u8>,
    apic: Vec<u8>,
    pic: Vec<u8>,
    pit: Vec<u8>,
    rtc: Vec<u8>,
    console: Vec<u8>,
}

impl Snapshot {
    /// Reads and parses the `state.bin` of a snapshot directory.
    pub fn read(dir: &Path) -> Result<Self> {
        let data: Vec<u8> =
            ::std::fs::read(dir.join("state.bin")).context("reading state.bin")?;
        let mut r: Reader<'_> = Reader::new(&data);

        if r.take(8)? != MAGIC {
            bail!("not a WHP microvm snapshot (bad magic)");
        }
        let ram_size: u64 = r.u64()?;

        let n_regs: u32 = r.u32()?;
        let mut regs: Vec<(WHV_REGISTER_NAME, WHV_REGISTER_VALUE)> =
            Vec::with_capacity(n_regs as usize);
        for _ in 0..n_regs {
            let name: WHV_REGISTER_NAME = WHV_REGISTER_NAME(r.i32()?);
            let value: WHV_REGISTER_VALUE = reg_from_bytes(r.take(16)?)?;
            regs.push((name, value));
        }

        let xsave: Vec<u8> = r.blob()?.to_vec();
        let apic: Vec<u8> = r.blob()?.to_vec();
        let pic: Vec<u8> = r.blob()?.to_vec();
        let pit: Vec<u8> = r.blob()?.to_vec();
        let rtc: Vec<u8> = r.blob()?.to_vec();
        let console: Vec<u8> = r.blob()?.to_vec();

        Ok(Self {
            ram_size,
            regs,
            xsave,
            apic,
            pic,
            pit,
            rtc,
            console,
        })
    }

    /// Returns the guest RAM size recorded in the snapshot.
    pub fn ram_size(&self) -> u64 {
        self.ram_size
    }

    /// Serialized 8259 PIC state.
    pub fn pic(&self) -> &[u8] {
        &self.pic
    }

    /// Serialized i8253 PIT state.
    pub fn pit(&self) -> &[u8] {
        &self.pit
    }

    /// Serialized RTC/CMOS state.
    pub fn rtc(&self) -> &[u8] {
        &self.rtc
    }

    /// Serialized portb console state (pending input queue).
    pub fn console(&self) -> &[u8] {
        &self.console
    }

    /// Applies the processor and local-APIC state to the vCPU. The partition must be set up, the
    /// vCPU created, and its CPUID configured beforehand.
    pub fn apply(&self, handle: WHV_PARTITION_HANDLE) -> Result<()> {
        let names: Vec<WHV_REGISTER_NAME> = self.regs.iter().map(|(n, _)| *n).collect();
        let values: Vec<WHV_REGISTER_VALUE> = self.regs.iter().map(|(_, v)| *v).collect();
        // SAFETY: `names`/`values` are equal-length arrays valid for the call; the values form a
        // consistent processor state captured from a running guest.
        unsafe {
            WHvSetVirtualProcessorRegisters(
                handle,
                VP_INDEX,
                names.as_ptr(),
                names.len() as u32,
                values.as_ptr(),
            )
            .context("WHvSetVirtualProcessorRegisters (restore) failed")?;
        }

        // Restore the FPU/SSE/AVX/CET state after the register file, so XCR0 and CR4 are already
        // in place and the extended components load consistently.
        if !self.xsave.is_empty() {
            // SAFETY: `xsave` is the XSAVE-area blob captured from the same guest.
            unsafe {
                WHvSetVirtualProcessorXsaveState(
                    handle,
                    VP_INDEX,
                    self.xsave.as_ptr().cast::<::core::ffi::c_void>(),
                    self.xsave.len() as u32,
                )
                .context("WHvSetVirtualProcessorXsaveState (restore) failed")?;
            }
        }

        if !self.apic.is_empty() {
            // SAFETY: `apic` is the local-APIC state blob captured from the same guest.
            unsafe {
                WHvSetVirtualProcessorInterruptControllerState(
                    handle,
                    VP_INDEX,
                    self.apic.as_ptr().cast::<::core::ffi::c_void>(),
                    self.apic.len() as u32,
                )
                .context("WHvSetVirtualProcessorInterruptControllerState (restore) failed")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_list_is_the_expected_size() {
        // 40 core (GPRs/segments/tables/CRs/DRs/XCR0) + 12 MSR + TSC_AUX + IA32_XSS. The
        // FPU/SSE/AVX/CET file is captured separately as an XSAVE area, not as individual
        // registers.
        assert_eq!(snapshot_reg_names().len(), 54);
    }

    #[test]
    fn blob_round_trips_through_reader() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend(0x1234_5678_9abc_def0_u64.to_le_bytes());
        put_blob(&mut buf, &[1, 2, 3, 4]);
        put_blob(&mut buf, b"");

        let mut r = Reader::new(&buf);
        assert_eq!(r.take(8).unwrap(), MAGIC);
        assert_eq!(r.u64().unwrap(), 0x1234_5678_9abc_def0);
        assert_eq!(r.blob().unwrap(), &[1, 2, 3, 4]);
        assert_eq!(r.blob().unwrap(), b"");
    }

    #[test]
    fn reader_rejects_truncated_blob() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend(16u32.to_le_bytes());
        buf.extend_from_slice(&[0; 4]);
        let mut r = Reader::new(&buf);
        assert!(r.blob().is_err());
    }
}

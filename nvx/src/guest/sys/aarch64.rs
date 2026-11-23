//! ARM EL0 architectural counter reads and the restricted NVX platform device.

use std::arch::asm;
use std::ffi::CStr;
use std::fmt;
use std::num::NonZeroU64;

use nix::sys::stat::{SFlag, fstat};

use super::{Errno, FileDescriptor, FileLock, Mode, OpenFlags, open};
use crate::guest::counter::{ArmBindingContext, ClockContext, ClockError};

/// Driver-owned restricted platform endpoint, never a physical-memory device.
const PLATFORM_DEVICE: &CStr = c"/dev/nvx-platform";

/// Device access or identity admission failed before a clock context became usable.
#[derive(Debug)]
pub(crate) enum ClockBindingError {
    /// Opening or reading the actual platform/counter failed.
    Device(Errno),
    /// The complete identity record did not match the observed counter binding.
    Invalid(ClockError),
}

impl fmt::Display for ClockBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device(error) => write!(formatter, "ARM platform time binding: {}", error.desc()),
            Self::Invalid(error) => write!(formatter, "ARM platform time binding: {error}"),
        }
    }
}

impl std::error::Error for ClockBindingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Device(error) => Some(error),
            Self::Invalid(error) => Some(error),
        }
    }
}

/// Register offsets of the restricted platform page, not x86 I/O-port numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Register {
    /// Four-byte identity FIFO reads and one-byte diagnostic writes.
    Data,
    /// One-byte capability reads and selector writes.
    Status,
    /// Four-byte time-sample FIFO reads.
    TimeWindow,
    /// One-byte guest exit-status request.
    Shutdown,
}

impl Register {
    /// Returns the driver ABI offset, on which the driver also enforces scalar width.
    const fn offset(self) -> u64 {
        match self {
            Self::Data => 0,
            Self::Status => 4,
            Self::TimeWindow => 8,
            Self::Shutdown => 12,
        }
    }
}

/// Reads the unscaled architectural counter rate, retaining every Hz.
pub(crate) fn counter_frequency() -> Result<NonZeroU64, Errno> {
    let value: u64;
    // SAFETY: CNTFRQ_EL0 is a read-only architectural EL0 register. No address or memory is used.
    unsafe {
        asm!("mrs {value}, cntfrq_el0", value = out(reg) value,
             options(nomem, nostack, preserves_flags));
    }
    if value > u64::from(u32::MAX) {
        return Err(Errno::EINVAL);
    }
    NonZeroU64::new(value).ok_or(Errno::EINVAL)
}

/// Reads the guest's virtual counter after prior instructions, never the host's clock.
pub(crate) fn counter_ticks() -> u64 {
    let value: u64;
    // SAFETY: CNTVCT_EL0 is an EL0-readable counter on the admitted guest. ISB orders the read
    // with surrounding instructions; the asm touches no address and preserves condition flags.
    unsafe {
        asm!("isb", "mrs {value}, cntvct_el0", "isb",
             value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Orders a probe timestamp against the memory exchanges that it measures.
pub(crate) fn counter_ticks_ordered() -> u64 {
    let value: u64;
    // SAFETY: the barriers and read-only EL0 counter access no address. Omitting `nomem`
    // also keeps the compiler from moving the probe's shared-memory accesses across the read.
    unsafe {
        asm!("dsb ish", "isb", "mrs {value}, cntvct_el0", "isb",
             value = out(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Owns only the kernel's restricted platform device, with no arbitrary physical-memory map.
///
/// The driver enforces CAP_SYS_RAWIO, exact register widths and offsets. Closing the owner
/// releases the descriptor; O_CLOEXEC prevents a workload from inheriting it across exec.
pub(crate) struct PlatformDevice {
    /// Close-on-exec character descriptor whose ownership bounds platform access.
    descriptor: FileDescriptor,
}

impl PlatformDevice {
    /// Opens without following a symlink and admits only a character-device descriptor.
    pub(crate) fn open() -> Result<Self, Errno> {
        let descriptor = open(
            PLATFORM_DEVICE,
            OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC | OpenFlags::O_NOFOLLOW,
            Mode::empty(),
        )?;
        Self::from_descriptor(descriptor)
    }

    /// Checks the opened descriptor itself, not a path that could have changed.
    fn from_descriptor(descriptor: FileDescriptor) -> Result<Self, Errno> {
        let kind = SFlag::from_bits_truncate(fstat(descriptor.borrowed())?.st_mode) & SFlag::S_IFMT;
        if kind != SFlag::S_IFCHR {
            return Err(Errno::ENODEV);
        }
        Ok(Self { descriptor })
    }

    /// Performs one exact-width transaction, rejecting a short transfer rather than retrying it.
    fn read<const N: usize>(&self, register: Register) -> Result<[u8; N], Errno> {
        let mut bytes = [0; N];
        if self.descriptor.pread(&mut bytes, register.offset())? == N {
            Ok(bytes)
        } else {
            Err(Errno::EIO)
        }
    }

    /// Performs one byte write and rejects a short transfer.
    fn write(&self, register: Register, value: u8) -> Result<(), Errno> {
        match self.descriptor.pwrite(&[value], register.offset())? {
            1 => Ok(()),
            _ => Err(Errno::EIO),
        }
    }

    /// Reads the platform capability/status byte.
    pub(crate) fn status(&self) -> Result<u8, Errno> {
        self.read::<1>(Register::Status).map(|bytes| bytes[0])
    }

    /// Selects a platform transaction under the caller's window lock.
    pub(crate) fn select(&self, selector: u8) -> Result<(), Errno> {
        self.write(Register::Status, selector)
    }

    /// Reads one little-endian word from the selected identity/generation FIFO.
    pub(crate) fn read_data_word(&self) -> Result<u32, Errno> {
        self.read(Register::Data).map(u32::from_le_bytes)
    }

    /// Reads one little-endian word from the selected time-sample window.
    pub(crate) fn read_time_word(&self) -> Result<u32, Errno> {
        self.read(Register::TimeWindow).map(u32::from_le_bytes)
    }

    /// Emits diagnostics as the driver's required one-byte transactions.
    pub(crate) fn write_console(&self, bytes: &[u8]) -> Result<(), Errno> {
        for &byte in bytes {
            self.write(Register::Data, byte)?;
        }
        Ok(())
    }

    /// Requests VMM shutdown with the guest's exit status.
    pub(crate) fn shutdown(&self, status: u8) -> Result<(), Errno> {
        self.write(Register::Shutdown, status)
    }

    /// Ends the VM through this retained owner, even after switching to a root without the node.
    pub(crate) fn power_off(self, status: u8) -> ! {
        let requested = self.shutdown(status);
        let closed = self.close();
        finish_power_off(requested.and(closed))
    }

    /// Explicitly releases access and reports a close error.
    pub(crate) fn close(self) -> Result<(), Errno> {
        self.descriptor.close()
    }

    /// The caller holds the shared platform-window lock over selection and every word.
    pub(crate) fn time_binding(&self) -> Result<[u8; 96], Errno> {
        self.select(0xa8)?;
        let mut identity = [0; 96];
        for word in identity.chunks_exact_mut(4) {
            word.copy_from_slice(&self.read_data_word()?.to_le_bytes());
        }
        Ok(identity)
    }

    /// Reads the actual selected-profile identity under the caller's existing window lock.
    /// No rate from a public status report, host profile or zero-LAPIC heuristic is admitted here.
    pub(crate) fn read_clock_context(
        &self,
        window_lock: &FileLock,
    ) -> Result<ClockContext, ClockBindingError> {
        self.read_clock_context_with_counter(window_lock, counter_frequency)
    }

    /// Validates the selected identity against an independently supplied counter read.
    fn read_clock_context_with_counter(
        &self,
        _window_lock: &FileLock,
        frequency: impl FnOnce() -> Result<NonZeroU64, Errno>,
    ) -> Result<ClockContext, ClockBindingError> {
        if self.status().map_err(ClockBindingError::Device)? & 0x40 == 0 {
            return Err(ClockBindingError::Device(Errno::EOPNOTSUPP));
        }
        let observed = frequency().map_err(ClockBindingError::Device)?;
        let identity = self.time_binding().map_err(ClockBindingError::Device)?;
        let binding = ArmBindingContext::from_identity(&identity, observed.get())
            .map_err(ClockBindingError::Invalid)?;
        Ok(ClockContext::arm(binding))
    }
}

/// Requests shutdown through the restricted device, without returning to a failed guest.
pub(crate) fn power_off(status: u8) -> ! {
    let outcome = PlatformDevice::open().and_then(|device| {
        let requested = device.shutdown(status);
        let closed = device.close();
        requested.and(closed)
    });
    finish_power_off(outcome)
}

/// Reports a failed shutdown request, then waits for the platform to end the VM.
fn finish_power_off(outcome: Result<(), Errno>) -> ! {
    if let Err(error) = outcome {
        super::write_standard_error(
            format!("nvx: platform shutdown: {}\n", error.desc()).as_bytes(),
        );
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests;

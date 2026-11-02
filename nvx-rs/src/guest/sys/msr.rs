//! Owned access to Linux's per-CPU model-specific-register devices, on x86-64.

use std::fs::{File, OpenOptions};
use std::mem::size_of;
use std::os::unix::fs::FileExt;

use super::{Errno, errno_of};

/// Whether an MSR device opens for reading only or for reading and writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MsrAccess {
    /// Reads registers.
    Read,
    /// Reads and writes registers.
    ReadWrite,
}

/// An owned `/dev/cpu/N/msr` file, whose offsets are register numbers.
///
/// The standard library opens the file, close-on-exec, and reads and writes it with `pread` and
/// `pwrite`. The `msr` driver turns a register access that faults with `#GP` into `EIO`, which the
/// time tools check for the registers that must fault.
#[derive(Debug)]
pub(crate) struct MsrFile(File);

impl MsrFile {
    /// Returns the path of the MSR device of `cpu`.
    pub(crate) fn path(cpu: usize) -> String {
        format!("/dev/cpu/{cpu}/msr")
    }

    /// Opens the MSR device of `cpu`, which needs `CAP_SYS_RAWIO`.
    pub(crate) fn open(cpu: usize, access: MsrAccess) -> Result<Self, Errno> {
        OpenOptions::new()
            .read(true)
            .write(access == MsrAccess::ReadWrite)
            .open(Self::path(cpu))
            .map(Self)
            .map_err(|error| errno_of(&error))
    }

    /// Reads the 64-bit register `register`. A short read is `EIO`.
    pub(crate) fn read(&self, register: u32) -> Result<u64, Errno> {
        let mut bytes = [0; size_of::<u64>()];
        self.0
            .read_exact_at(&mut bytes, u64::from(register))
            .map_err(|error| errno_of(&error))?;
        Ok(u64::from_ne_bytes(bytes))
    }

    /// Writes `value` to the 64-bit register `register`. A short write is `EIO`.
    pub(crate) fn write(&self, register: u32, value: u64) -> Result<(), Errno> {
        self.0
            .write_all_at(&value.to_ne_bytes(), u64::from(register))
            .map_err(|error| errno_of(&error))
    }
}

#[cfg(test)]
mod tests;

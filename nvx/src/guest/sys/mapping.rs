//! Owned memory mappings with explicit and best-effort unmapping.

use std::ffi::c_void;
use std::num::NonZeroUsize;
use std::ptr::NonNull;

use nix::sys::mman::{MapFlags, ProtFlags};

use super::{Errno, FileDescriptor};

/// One writable shared file or device mapping.
///
/// The constructor records the nonzero mapped length and successful pointer. [`unmap`](Self::unmap)
/// consumes the mapping and reports a failure for code paths where the C tool called `perror`;
/// [`Drop`] makes every earlier return best-effort RAII cleanup.
pub(crate) struct MemoryMapping {
    /// Mapping base while it remains owned.
    address: Option<NonNull<c_void>>,
    /// Number of mapped bytes.
    length: NonZeroUsize,
}

impl MemoryMapping {
    /// Maps `length` bytes of `descriptor` read-write and shared at `offset`.
    pub(crate) fn shared_read_write(
        descriptor: &FileDescriptor,
        length: NonZeroUsize,
        offset: u64,
    ) -> Result<Self, Errno> {
        // SAFETY: the descriptor remains open for the call, `length` is nonzero, no fixed address
        // is requested, and the returned mapping is owned and unmapped exactly once by this type.
        let address = unsafe {
            nix::sys::mman::mmap(
                None,
                length,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                descriptor.borrowed(),
                offset as _,
            )
        }?;
        Ok(Self {
            address: Some(address),
            length,
        })
    }

    /// Stores a native-endian 32-bit value at `offset`, even when it is unaligned.
    ///
    /// Returns `false` without writing unless all four bytes lie inside the mapping. The mapping
    /// is writable by construction. The single x86 store mirrors the volatile C device access
    /// and prevents the compiler from splitting, eliding, or combining it.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn write_u32(&mut self, offset: usize, value: u32) -> bool {
        if offset
            .checked_add(size_of::<u32>())
            .is_none_or(|end| end > self.length.get())
        {
            return false;
        }
        let address = self.address.expect("owned mapping has an address");
        // SAFETY: the checked four-byte range lies inside this writable mapping. x86 permits an
        // unaligned 32-bit store; the instruction has exactly the volatile device-store behavior
        // required by the C implementation.
        unsafe {
            core::arch::asm!(
                "mov dword ptr [{target}], {value:e}",
                target = in(reg) address.as_ptr().cast::<u8>().add(offset),
                value = in(reg) value,
                options(nostack, preserves_flags),
            );
        }
        true
    }

    /// Copies `bytes` into the mapping at `offset`.
    ///
    /// Returns `false` without writing unless every byte lies inside the mapping. The mapping is
    /// writable by construction.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn write_bytes(&mut self, offset: usize, bytes: &[u8]) -> bool {
        if offset
            .checked_add(bytes.len())
            .is_none_or(|end| end > self.length.get())
        {
            return false;
        }
        let address = self.address.expect("owned mapping has an address");
        // SAFETY: the checked range lies inside this writable mapping, which no reference
        // aliases, and `bytes` cannot overlap it, because this type lends out no view of it.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                address.as_ptr().cast::<u8>().add(offset),
                bytes.len(),
            );
        }
        true
    }

    /// Unmaps this mapping and reports the kernel error, if any.
    pub(crate) fn unmap(mut self) -> Result<(), Errno> {
        let address = self.address.take().expect("mapping is owned");
        // SAFETY: this type owns the exact mapping and length, removes the address before the
        // call, and therefore cannot unmap it a second time from `Drop`.
        unsafe { nix::sys::mman::munmap(address, self.length.get()) }
    }
}

impl Drop for MemoryMapping {
    fn drop(&mut self) {
        let Some(address) = self.address.take() else {
            return;
        };
        // SAFETY: this type still owns the exact mapping. Drop is the best-effort cleanup path,
        // matching the C tools' ignored cleanup failures on earlier errors.
        let _ = unsafe { nix::sys::mman::munmap(address, self.length.get()) };
    }
}

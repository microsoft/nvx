// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # Boot
//!
//! PVH boot protocol support: parameter structures and the kernel/initramfs loader.
//!

#[cfg(target_os = "linux")]
pub mod mptable;
pub mod params;
pub mod pvh;

use ::anyhow::Result;

use self::params::as_bytes;

/// Minimal guest-memory writer used by the backend-agnostic PVH loader.
///
/// Each backend's guest-memory type implements it: the KVM `GuestMemory` on Linux and the
/// WHP `GuestMemory` on Windows. This keeps [`pvh`] free of any hypervisor dependency so the
/// same loader lays out the kernel, initramfs, and `hvm_start_info` for both backends.
pub trait GuestWrite {
    /// Copies `data` into guest RAM starting at guest-physical address `gpa`.
    fn write_slice(&self, gpa: u64, data: &[u8]) -> Result<()>;

    /// Returns the guest-physical `(base, size)` pairs of every RAM region, ordered by base.
    fn ram_regions(&self) -> Vec<(u64, u64)>;

    /// Writes a `repr(C)` POD value into guest RAM at `gpa` (little-endian).
    fn write_obj<T: Copy>(&self, gpa: u64, value: &T) -> Result<()> {
        self.write_slice(gpa, as_bytes(value))
    }
}

// The KVM guest memory (Linux backend) satisfies the loader's needs by forwarding to its
// inherent methods; inherent methods win name resolution, so there is no recursion.
#[cfg(target_os = "linux")]
impl GuestWrite for crate::memory::GuestMemory {
    fn write_slice(&self, gpa: u64, data: &[u8]) -> Result<()> {
        self.write_slice(gpa, data)
    }

    fn ram_regions(&self) -> Vec<(u64, u64)> {
        self.ram_regions()
    }
}

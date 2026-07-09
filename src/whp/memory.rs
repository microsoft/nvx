// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The guest-memory layout (MMIO-gap split, region-per-mapping) mirrors the KVM backend of the
// Nanvix Micro-VM (uservm), Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Guest Memory (WHP)
//!
//! Host-allocated, WHP-registered guest RAM. Memory is allocated as one or two host mappings
//! (a low region below the MMIO gap and, if needed, a high region above 4 GiB) with
//! `VirtualAlloc`, and each is registered with the partition through `WHvMapGpaRange` as a
//! read/write/execute guest-physical range.
//!

use ::core::ffi::c_void;

use ::anyhow::{
    Context,
    Result,
    bail,
};
use ::windows::Win32::System::Hypervisor::{
    WHV_MAP_GPA_RANGE_FLAGS,
    WHV_PARTITION_HANDLE,
    WHvMapGpaRange,
    WHvMapGpaRangeFlagExecute,
    WHvMapGpaRangeFlagRead,
    WHvMapGpaRangeFlagWrite,
    WHvUnmapGpaRange,
};
use ::windows::Win32::System::Memory::{
    MEM_COMMIT,
    MEM_RELEASE,
    MEM_RESERVE,
    PAGE_READWRITE,
    VirtualAlloc,
    VirtualFree,
};

use crate::boot::GuestWrite;
use crate::layout::{
    MMIO_GAP_START,
    RAM_64BIT_START,
};

/// A contiguous guest-physical memory region backed by a `VirtualAlloc` mapping.
struct MemoryRegion {
    /// Base guest-physical address.
    guest_phys: u64,
    /// Size of the region, in bytes.
    size: usize,
    /// Host virtual address of the backing mapping.
    host_addr: *mut u8,
    /// Owning partition (needed to unmap the range on drop).
    partition: WHV_PARTITION_HANDLE,
}

// SAFETY: The backing mappings are owned exclusively by this process for the VM's lifetime and
// are only accessed through the synchronised `GuestMemory` API and by the guest vCPU.
unsafe impl Send for MemoryRegion {}
unsafe impl Sync for MemoryRegion {}

impl Drop for MemoryRegion {
    fn drop(&mut self) {
        // SAFETY: `guest_phys`/`size` describe a range mapped by `map_region`, and `host_addr`
        // is the `VirtualAlloc` base for that region.
        unsafe {
            let _ = WHvUnmapGpaRange(self.partition, self.guest_phys, self.size as u64);
            let _ = VirtualFree(self.host_addr.cast::<c_void>(), 0, MEM_RELEASE);
        }
    }
}

/// Guest RAM: a set of host mappings registered with a WHP partition.
pub struct GuestMemory {
    /// The backing regions, ordered by guest-physical base address.
    regions: Vec<MemoryRegion>,
    /// Total amount of guest RAM, in bytes.
    ram_size: u64,
}

impl GuestMemory {
    ///
    /// # Description
    ///
    /// Allocates `ram_size` bytes of guest RAM and registers it with `partition`. If
    /// `ram_size` exceeds the start of the MMIO gap, the RAM is split into a low region below
    /// the gap and a high region starting at 4 GiB, so the guest-physical MMIO window (the
    /// in-hypervisor LAPIC lives at `0xFEE0_0000`) is never shadowed by RAM.
    ///
    pub fn new(partition: WHV_PARTITION_HANDLE, ram_size: u64) -> Result<Self> {
        if ram_size == 0 {
            bail!("guest RAM size must be non-zero");
        }

        let mut regions: Vec<MemoryRegion> = Vec::new();

        let low_size: u64 = ram_size.min(MMIO_GAP_START);
        regions.push(Self::map_region(partition, 0, low_size)?);

        if ram_size > MMIO_GAP_START {
            let high_size: u64 = ram_size - MMIO_GAP_START;
            regions.push(Self::map_region(partition, RAM_64BIT_START, high_size)?);
        }

        Ok(Self { regions, ram_size })
    }

    /// Allocates one host mapping and registers it with the partition as guest RAM.
    fn map_region(
        partition: WHV_PARTITION_HANDLE,
        guest_phys: u64,
        size: u64,
    ) -> Result<MemoryRegion> {
        let size: usize = usize::try_from(size).context("region size overflows usize")?;

        // SAFETY: A standard reserve+commit request; the returned pointer is checked for null
        // before any use.
        let host_addr: *mut c_void =
            unsafe { VirtualAlloc(None, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        if host_addr.is_null() {
            bail!("VirtualAlloc failed for guest memory region (size={size})");
        }
        let host_addr: *mut u8 = host_addr.cast::<u8>();

        let flags: WHV_MAP_GPA_RANGE_FLAGS = WHV_MAP_GPA_RANGE_FLAGS(
            WHvMapGpaRangeFlagRead.0 | WHvMapGpaRangeFlagWrite.0 | WHvMapGpaRangeFlagExecute.0,
        );

        // SAFETY: `host_addr` is a live, writable mapping of `size` bytes that outlives the
        // partition's use of the guest-physical range.
        unsafe {
            WHvMapGpaRange(
                partition,
                host_addr.cast::<c_void>(),
                guest_phys,
                size as u64,
                flags,
            )
            .with_context(|| format!("WHvMapGpaRange failed (gpa={guest_phys:#x}, size={size})"))?;
        }

        Ok(MemoryRegion {
            guest_phys,
            size,
            host_addr,
            partition,
        })
    }

    /// Returns the total amount of guest RAM, in bytes.
    pub fn ram_size(&self) -> u64 {
        self.ram_size
    }

    /// Returns the guest-physical `(base, size)` pairs of every RAM region, ordered by base.
    pub fn ram_regions(&self) -> Vec<(u64, u64)> {
        self.regions
            .iter()
            .map(|r| (r.guest_phys, r.size as u64))
            .collect()
    }

    /// Returns the host pointer for guest-physical address `gpa`, ensuring that `len` bytes
    /// starting there fall within a single region.
    fn host_ptr(&self, gpa: u64, len: usize) -> Result<*mut u8> {
        for region in &self.regions {
            let end: u64 = region.guest_phys + region.size as u64;
            if gpa >= region.guest_phys && gpa < end {
                let offset: u64 = gpa - region.guest_phys;
                if offset + len as u64 > region.size as u64 {
                    bail!("access at gpa={gpa:#x} len={len} spans past a RAM region");
                }
                // SAFETY: `offset + len` is within the region bounds checked above.
                return Ok(unsafe { region.host_addr.add(offset as usize) });
            }
        }
        bail!("no guest RAM mapped at gpa={gpa:#x}")
    }

    /// Copies `data` into guest RAM starting at guest-physical address `gpa`.
    pub fn write_slice(&self, gpa: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let dst: *mut u8 = self.host_ptr(gpa, data.len())?;
        // SAFETY: `dst` is valid for `data.len()` bytes (bounds checked in `host_ptr`); source
        // and destination do not overlap and are byte-aligned.
        unsafe {
            ::core::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
        }
        Ok(())
    }

    /// Reads `data.len()` bytes from guest RAM at guest-physical address `gpa` into `data`.
    #[allow(dead_code)]
    pub fn read_slice(&self, gpa: u64, data: &mut [u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let src: *mut u8 = self.host_ptr(gpa, data.len())?;
        // SAFETY: `src` is valid for `data.len()` bytes (bounds checked in `host_ptr`).
        unsafe {
            ::core::ptr::copy_nonoverlapping(src, data.as_mut_ptr(), data.len());
        }
        Ok(())
    }
}

// The PVH loader writes the kernel, initramfs and boot structures through this trait. The
// inherent methods win name resolution, so the forwarding calls do not recurse.
impl GuestWrite for GuestMemory {
    fn write_slice(&self, gpa: u64, data: &[u8]) -> Result<()> {
        self.write_slice(gpa, data)
    }

    fn ram_regions(&self) -> Vec<(u64, u64)> {
        self.ram_regions()
    }
}

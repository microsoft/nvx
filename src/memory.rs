// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The guest-memory management approach is derived from the KVM backend of the Nanvix
// Micro-VM (uservm), Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Guest Memory
//!
//! Host-allocated, KVM-registered guest RAM. Memory is allocated as one or two anonymous
//! mappings (a low region below the MMIO gap and, if needed, a high region above 4 GiB)
//! and registered with KVM as user memory regions.
//!

use ::anyhow::{
    Context,
    Result,
    bail,
};
use ::kvm_bindings::kvm_userspace_memory_region;
use ::kvm_ioctls::VmFd;

use crate::layout::{
    MMIO_GAP_START,
    RAM_64BIT_START,
};

/// A contiguous guest-physical memory region backed by a host mapping.
struct MemoryRegion {
    /// Base guest-physical address.
    guest_phys: u64,
    /// Size of the region, in bytes.
    size: usize,
    /// Host virtual address of the backing mapping.
    host_addr: *mut u8,
    /// KVM memory slot index.
    slot: u32,
}

// SAFETY: The backing mappings are owned exclusively by this process for the VM's lifetime
// and are only accessed through the synchronised `GuestMemory` API and by the guest vCPU.
unsafe impl Send for MemoryRegion {}
unsafe impl Sync for MemoryRegion {}

impl Drop for MemoryRegion {
    fn drop(&mut self) {
        // SAFETY: `host_addr`/`size` describe a mapping produced by `mmap` in `map_region`.
        unsafe {
            ::libc::munmap(self.host_addr.cast::<::libc::c_void>(), self.size);
        }
    }
}

/// Guest RAM: a set of host mappings registered with KVM.
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
    /// Allocates `ram_size` bytes of guest RAM and registers it with KVM. If `ram_size`
    /// exceeds the start of the MMIO gap, the RAM is split into a low region below the gap
    /// and a high region starting at 4 GiB.
    ///
    pub fn new(vm_fd: &VmFd, ram_size: u64) -> Result<Self> {
        if ram_size == 0 {
            bail!("guest RAM size must be non-zero");
        }

        let mut regions: Vec<MemoryRegion> = Vec::new();
        let mut next_slot: u32 = 0;

        let low_size: u64 = ram_size.min(MMIO_GAP_START);
        regions.push(Self::map_region(vm_fd, next_slot, 0, low_size)?);
        next_slot += 1;

        if ram_size > MMIO_GAP_START {
            let high_size: u64 = ram_size - MMIO_GAP_START;
            regions.push(Self::map_region(vm_fd, next_slot, RAM_64BIT_START, high_size)?);
        }

        Ok(Self { regions, ram_size })
    }

    ///
    /// # Description
    ///
    /// Reconstructs guest RAM from a snapshot file using a copy-on-write (`MAP_PRIVATE`)
    /// file mapping. Pages are faulted in lazily on first access, so a restore does not pay
    /// an upfront copy of the whole image.
    ///
    /// The layout (split at the MMIO gap) mirrors [`new`](Self::new); the snapshot file is the
    /// concatenation of the regions in ascending guest-physical order.
    ///
    pub fn restore(vm_fd: &VmFd, path: &::std::path::Path, ram_size: u64) -> Result<Self> {
        use ::std::os::fd::AsRawFd;

        let file: ::std::fs::File =
            ::std::fs::File::open(path).with_context(|| format!("opening RAM image {path:?}"))?;
        let fd: ::libc::c_int = file.as_raw_fd();

        let mut regions: Vec<MemoryRegion> = Vec::new();
        let mut next_slot: u32 = 0;
        let mut file_off: u64 = 0;

        let low_size: u64 = ram_size.min(MMIO_GAP_START);
        regions.push(Self::map_region_backed(vm_fd, next_slot, 0, low_size, fd, file_off)?);
        next_slot += 1;
        file_off += low_size;

        if ram_size > MMIO_GAP_START {
            let high_size: u64 = ram_size - MMIO_GAP_START;
            regions.push(Self::map_region_backed(
                vm_fd,
                next_slot,
                RAM_64BIT_START,
                high_size,
                fd,
                file_off,
            )?);
        }

        Ok(Self { regions, ram_size })
    }

    ///
    /// # Description
    ///
    /// Writes the full contents of guest RAM to `path`, as the concatenation of the regions
    /// in ascending guest-physical order (the format consumed by [`restore`](Self::restore)).
    ///
    pub fn snapshot_ram(&self, path: &::std::path::Path) -> Result<()> {
        use ::std::io::Write;
        let file: ::std::fs::File =
            ::std::fs::File::create(path).with_context(|| format!("creating RAM image {path:?}"))?;
        let mut writer: ::std::io::BufWriter<::std::fs::File> = ::std::io::BufWriter::new(file);
        for region in &self.regions {
            // SAFETY: `host_addr`/`size` describe a live mapping owned by this region.
            let bytes: &[u8] =
                unsafe { ::core::slice::from_raw_parts(region.host_addr, region.size) };
            writer.write_all(bytes).context("writing RAM image")?;
        }
        writer.flush().context("flushing RAM image")?;
        Ok(())
    }

    /// Allocates one anonymous host mapping and registers it with KVM as memory `slot`.
    fn map_region(vm_fd: &VmFd, slot: u32, guest_phys: u64, size: u64) -> Result<MemoryRegion> {
        Self::map_region_backed(vm_fd, slot, guest_phys, size, -1, 0)
    }

    /// Allocates one host mapping (anonymous if `fd < 0`, otherwise a `MAP_PRIVATE` mapping of
    /// `fd` at `file_off`) and registers it with KVM as memory `slot`.
    fn map_region_backed(
        vm_fd: &VmFd,
        slot: u32,
        guest_phys: u64,
        size: u64,
        fd: ::libc::c_int,
        file_off: u64,
    ) -> Result<MemoryRegion> {
        let size: usize = usize::try_from(size).context("region size overflows usize")?;

        let (flags, off): (::libc::c_int, ::libc::off_t) = if fd < 0 {
            (::libc::MAP_PRIVATE | ::libc::MAP_ANONYMOUS | ::libc::MAP_NORESERVE, 0)
        } else {
            (::libc::MAP_PRIVATE | ::libc::MAP_NORESERVE, file_off as ::libc::off_t)
        };

        // SAFETY: Standard mapping request; the returned pointer is checked against MAP_FAILED
        // before use.
        let host_addr: *mut ::libc::c_void = unsafe {
            ::libc::mmap(
                ::core::ptr::null_mut(),
                size,
                ::libc::PROT_READ | ::libc::PROT_WRITE,
                flags,
                fd,
                off,
            )
        };
        if host_addr == ::libc::MAP_FAILED {
            bail!("mmap failed for guest memory region (size={size})");
        }
        let host_addr: *mut u8 = host_addr.cast::<u8>();

        let region: kvm_userspace_memory_region = kvm_userspace_memory_region {
            slot,
            flags: 0,
            guest_phys_addr: guest_phys,
            memory_size: size as u64,
            userspace_addr: host_addr as u64,
        };

        // SAFETY: `region` describes a live, writable host mapping that outlives the VM's use
        // of the memory slot.
        unsafe {
            vm_fd
                .set_user_memory_region(region)
                .context("KVM_SET_USER_MEMORY_REGION failed")?;
        }

        Ok(MemoryRegion {
            guest_phys,
            size,
            host_addr,
            slot,
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
                    bail!(
                        "access at gpa={gpa:#x} len={len} spans past region (slot={})",
                        region.slot
                    );
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
        // SAFETY: `dst` is valid for `data.len()` bytes (bounds checked in `host_ptr`), the
        // source and destination do not overlap, and both are byte-aligned.
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

    /// Writes a `repr(C)` POD value into guest RAM at guest-physical address `gpa`.
    pub fn write_obj<T: Copy>(&self, gpa: u64, value: &T) -> Result<()> {
        self.write_slice(gpa, crate::boot::params::as_bytes(value))
    }
}

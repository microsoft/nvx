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

        // Guest RAM is mostly zero (a fresh boot touches only a fraction of it), so write the
        // image sparsely: each region emits only its non-zero pages and leaves a hole for every
        // run of zero pages. Restore maps the file copy-on-write unchanged -- holes fault in as
        // zero pages -- so the on-disk artifact shrinks to the guest's actual footprint with no
        // format change.
        let mut file: ::std::fs::File =
            ::std::fs::File::create(path).with_context(|| format!("creating RAM image {path:?}"))?;
        let mut total: u64 = 0;
        for region in &self.regions {
            // SAFETY: `host_addr`/`size` describe a live mapping owned by this region.
            let bytes: &[u8] =
                unsafe { ::core::slice::from_raw_parts(region.host_addr, region.size) };
            write_sparse(&mut file, bytes).context("writing RAM image")?;
            total += region.size as u64;
        }
        // A trailing zero run only advances the offset via seek, so pin the file to RAM size.
        file.set_len(total).context("sizing RAM image")?;
        file.flush().context("flushing RAM image")?;
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

    /// Returns a cheap, cloneable, thread-safe accessor for guest RAM, used by device models
    /// (e.g. the virt-net NIC) to perform virtqueue DMA from their own threads.
    ///
    /// The accessor borrows the same host mappings as this [`GuestMemory`]; the caller must keep
    /// the owning `GuestMemory` alive for as long as any [`GuestRam`] (or thread using one) can
    /// still touch guest RAM.
    pub fn ram(&self) -> GuestRam {
        let views: Vec<RegionView> = self
            .regions
            .iter()
            .map(|r| RegionView {
                guest_phys: r.guest_phys,
                size: r.size as u64,
                host_addr: r.host_addr as usize,
            })
            .collect();
        GuestRam {
            regions: ::std::sync::Arc::new(views),
        }
    }
}

/// A single RAM region as seen by [`GuestRam`]: a guest-physical range and its host base address.
#[derive(Clone, Copy)]
struct RegionView {
    guest_phys: u64,
    size: u64,
    host_addr: usize,
}

/// A thread-safe, cheaply cloneable view of guest RAM for device DMA.
///
/// Translates guest-physical addresses to host pointers into the same mappings owned by
/// [`GuestMemory`]. All accesses are bounds-checked against a single region (guest-physical DMA
/// never legitimately spans a region boundary here).
#[derive(Clone)]
pub struct GuestRam {
    regions: ::std::sync::Arc<Vec<RegionView>>,
}

// SAFETY: The backing mappings are stable for the VM's lifetime and are accessed only through
// bounds-checked pointer copies; the owning `GuestMemory` guarantees they outlive every access.
unsafe impl Send for GuestRam {}
unsafe impl Sync for GuestRam {}

impl GuestRam {
    /// Returns the host pointer for `gpa`, ensuring `len` bytes fit within a single region.
    fn host_ptr(&self, gpa: u64, len: usize) -> Option<*mut u8> {
        for region in self.regions.iter() {
            let end: u64 = region.guest_phys + region.size;
            if gpa >= region.guest_phys && gpa < end {
                let offset: u64 = gpa - region.guest_phys;
                if offset + len as u64 > region.size {
                    return None;
                }
                // SAFETY: `offset + len` is within the region bounds checked above.
                return Some(unsafe { (region.host_addr as *mut u8).add(offset as usize) });
            }
        }
        None
    }

    /// Reads `buf.len()` bytes from guest RAM at `gpa`. Returns `false` if the range is unmapped.
    #[must_use]
    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        if buf.is_empty() {
            return true;
        }
        match self.host_ptr(gpa, buf.len()) {
            // SAFETY: `src` is valid for `buf.len()` bytes (bounds checked in `host_ptr`).
            Some(src) => unsafe {
                ::core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), buf.len());
                true
            },
            None => false,
        }
    }

    /// Writes `data` into guest RAM at `gpa`. Returns `false` if the range is unmapped.
    #[must_use]
    pub fn write(&self, gpa: u64, data: &[u8]) -> bool {
        if data.is_empty() {
            return true;
        }
        match self.host_ptr(gpa, data.len()) {
            // SAFETY: `dst` is valid for `data.len()` bytes (bounds checked in `host_ptr`).
            Some(dst) => unsafe {
                ::core::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
                true
            },
            None => false,
        }
    }

    /// Reads a little-endian `u16` from guest RAM at `gpa` (0 if unmapped).
    pub fn read_u16(&self, gpa: u64) -> u16 {
        let mut b: [u8; 2] = [0; 2];
        let _ = self.read(gpa, &mut b);
        u16::from_le_bytes(b)
    }

    /// Writes a little-endian `u16` to guest RAM at `gpa`.
    pub fn write_u16(&self, gpa: u64, value: u16) {
        let _ = self.write(gpa, &value.to_le_bytes());
    }

    /// Reads a little-endian `u32` from guest RAM at `gpa` (0 if unmapped).
    pub fn read_u32(&self, gpa: u64) -> u32 {
        let mut b: [u8; 4] = [0; 4];
        let _ = self.read(gpa, &mut b);
        u32::from_le_bytes(b)
    }

    /// Writes a little-endian `u32` to guest RAM at `gpa`.
    pub fn write_u32(&self, gpa: u64, value: u32) {
        let _ = self.write(gpa, &value.to_le_bytes());
    }

    /// Reads a little-endian `u64` from guest RAM at `gpa` (0 if unmapped).
    pub fn read_u64(&self, gpa: u64) -> u64 {
        let mut b: [u8; 8] = [0; 8];
        let _ = self.read(gpa, &mut b);
        u64::from_le_bytes(b)
    }
}

/// Writes `bytes` to `file` at its current offset, punching a hole (via a forward seek) for each
/// run of zero pages instead of writing zeros, so the resulting file is sparse. The file offset
/// always advances by `bytes.len()`; the caller must `set_len` afterwards so a trailing hole is
/// reflected in the file size. Runs of like (zero / non-zero) pages are coalesced into a single
/// seek/write to keep the syscall count low.
fn write_sparse(file: &mut ::std::fs::File, bytes: &[u8]) -> Result<()> {
    use ::std::io::{
        Seek,
        SeekFrom,
        Write,
    };
    const PAGE: usize = 4096;
    let is_zero = |chunk: &[u8]| chunk.iter().all(|&b| b == 0);
    let mut off: usize = 0;
    while off < bytes.len() {
        let zero: bool = is_zero(&bytes[off..(off + PAGE).min(bytes.len())]);
        // Extend the current run of same-kind pages.
        let mut run_end: usize = (off + PAGE).min(bytes.len());
        while run_end < bytes.len() {
            let next: usize = (run_end + PAGE).min(bytes.len());
            if is_zero(&bytes[run_end..next]) != zero {
                break;
            }
            run_end = next;
        }
        if zero {
            file.seek(SeekFrom::Current((run_end - off) as i64))
                .context("seeking past zero pages")?;
        } else {
            file.write_all(&bytes[off..run_end]).context("writing non-zero pages")?;
        }
        off = run_end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_sparse;

    /// A sparse write must reproduce the exact original bytes on read-back: full non-zero pages,
    /// zero holes, a partially populated page, and a trailing zero run all round-trip.
    #[test]
    fn sparse_write_round_trips() {
        const PAGE: usize = 4096;
        let mut buf: Vec<u8> = vec![0u8; PAGE * 8];
        // page 0: fully non-zero
        for (i, b) in buf[..PAGE].iter_mut().enumerate() {
            *b = (i % 251 + 1) as u8;
        }
        // page 3: fully non-zero
        buf[PAGE * 3..PAGE * 4].fill(0xAB);
        // page 5: a single non-zero byte -- a partially populated page must still be preserved
        buf[PAGE * 5 + 123] = 7;
        // pages 1, 2, 4, 6, 7 stay zero (holes), including a trailing zero run.

        let path =
            ::std::env::temp_dir().join(format!("microvm-sparse-{}.bin", ::std::process::id()));
        {
            let mut f = ::std::fs::File::create(&path).unwrap();
            write_sparse(&mut f, &buf).unwrap();
            // Mirror snapshot_ram: pin the length so the trailing hole is materialised.
            f.set_len(buf.len() as u64).unwrap();
        }
        let back = ::std::fs::read(&path).unwrap();
        let _ = ::std::fs::remove_file(&path);
        assert_eq!(back.len(), buf.len(), "restored image must be full RAM size");
        assert_eq!(back, buf, "sparse round-trip must reproduce the original bytes");
    }
}

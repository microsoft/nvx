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
use ::windows::Win32::Foundation::{
    CloseHandle,
    HANDLE,
};
use ::windows::Win32::System::IO::DeviceIoControl;
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
    CreateFileMappingW,
    FILE_MAP_COPY,
    MEM_COMMIT,
    MEM_RELEASE,
    MEM_RESERVE,
    MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile,
    PAGE_READWRITE,
    PAGE_WRITECOPY,
    UnmapViewOfFile,
    VirtualAlloc,
    VirtualFree,
};
use ::windows::core::PCWSTR;

use crate::boot::GuestWrite;
use crate::layout::{
    MMIO_GAP_START,
    RAM_64BIT_START,
};

/// How a [`MemoryRegion`]'s host backing was obtained, so it can be released correctly.
enum Backing {
    /// Private, demand-zero memory from `VirtualAlloc` (cold boot). Released with `VirtualFree`.
    Reserved,
    /// A copy-on-write view of the snapshot file from `MapViewOfFile` (restore). Released with
    /// `UnmapViewOfFile`.
    Mapped,
}

/// A contiguous guest-physical memory region backed by a host mapping.
struct MemoryRegion {
    /// Base guest-physical address.
    guest_phys: u64,
    /// Size of the region, in bytes.
    size: usize,
    /// Host virtual address of the backing mapping.
    host_addr: *mut u8,
    /// Owning partition (needed to unmap the range on drop).
    partition: WHV_PARTITION_HANDLE,
    /// How the host backing was obtained (selects the correct release call on drop).
    backing: Backing,
}

// SAFETY: The backing mappings are owned exclusively by this process for the VM's lifetime and
// are only accessed through the synchronised `GuestMemory` API and by the guest vCPU.
unsafe impl Send for MemoryRegion {}
unsafe impl Sync for MemoryRegion {}

impl Drop for MemoryRegion {
    fn drop(&mut self) {
        // SAFETY: `guest_phys`/`size` describe a range mapped by `map_region`/`map_region_view`,
        // and `host_addr` is the corresponding `VirtualAlloc` base or `MapViewOfFile` view.
        unsafe {
            let _ = WHvUnmapGpaRange(self.partition, self.guest_phys, self.size as u64);
            match self.backing {
                Backing::Reserved => {
                    let _ = VirtualFree(self.host_addr.cast::<c_void>(), 0, MEM_RELEASE);
                },
                Backing::Mapped => {
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: self.host_addr.cast::<c_void>(),
                    });
                },
            }
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

    ///
    /// # Description
    ///
    /// Reconstructs guest RAM from a snapshot file **without** an upfront copy, by mapping the
    /// file copy-on-write (`FILE_MAP_COPY`) into one view per region and registering each view
    /// with the partition. Guest pages fault in lazily from the file on first access, and writes
    /// go to private (pagefile-backed) copies, so the snapshot file is never modified and a
    /// restore pays only for the pages the guest actually touches — mirroring the KVM backend's
    /// `MAP_PRIVATE` restore. The region layout and file order match [`new`](Self::new).
    ///
    pub fn restore(
        partition: WHV_PARTITION_HANDLE,
        path: &::std::path::Path,
        ram_size: u64,
    ) -> Result<Self> {
        use ::std::os::windows::io::AsRawHandle;

        if ram_size == 0 {
            bail!("guest RAM size must be non-zero");
        }

        // Keep the file open until the views are mapped; the mapping holds its own reference to
        // the underlying section afterwards, so the file handle may be closed on return.
        let file: ::std::fs::File =
            ::std::fs::File::open(path).with_context(|| format!("opening RAM image {path:?}"))?;
        let file_handle: HANDLE = HANDLE(file.as_raw_handle());

        // A copy-on-write section spanning the whole file (max-size 0 means "use the file size").
        let mapping: HANDLE = unsafe {
            CreateFileMappingW(file_handle, None, PAGE_WRITECOPY, 0, 0, PCWSTR::null())
        }
        .with_context(|| format!("CreateFileMapping for RAM image {path:?}"))?;

        let build = || -> Result<Vec<MemoryRegion>> {
            let mut regions: Vec<MemoryRegion> = Vec::new();
            let low_size: u64 = ram_size.min(MMIO_GAP_START);
            regions.push(Self::map_region_view(partition, mapping, 0, 0, low_size)?);
            if ram_size > MMIO_GAP_START {
                let high_size: u64 = ram_size - MMIO_GAP_START;
                regions.push(Self::map_region_view(
                    partition,
                    mapping,
                    MMIO_GAP_START,
                    RAM_64BIT_START,
                    high_size,
                )?);
            }
            Ok(regions)
        };
        let regions: Result<Vec<MemoryRegion>> = build();

        // Each mapped view holds its own reference to the section, so our handle can be released
        // now regardless of success. On error the partial `regions` drop (unmapping their views).
        // SAFETY: `mapping` is the handle just returned by `CreateFileMappingW`.
        unsafe {
            let _ = CloseHandle(mapping);
        }

        Ok(Self {
            regions: regions?,
            ram_size,
        })
    }

    ///
    /// # Description
    ///
    /// Writes the full contents of guest RAM to `path`, as the concatenation of the regions in
    /// ascending guest-physical order (the format consumed by [`restore`](Self::restore)). Runs
    /// of zero pages are skipped with a seek, so the on-disk image is sparse where the guest has
    /// not touched RAM.
    ///
    pub fn snapshot_ram(&self, path: &::std::path::Path) -> Result<()> {
        use ::std::io::Write;

        let mut file: ::std::fs::File = ::std::fs::File::create(path)
            .with_context(|| format!("creating RAM image {path:?}"))?;
        // Mark the file sparse so the zero runs skipped by `write_sparse` become real holes on
        // disk. Unlike Unix, Windows does not create holes from seeks unless the file is sparse,
        // so without this the "skipped" zero pages would be physically allocated. Best-effort:
        // a filesystem that does not support sparse files just yields a full-size image.
        set_sparse(&file);
        let mut total: u64 = 0;
        for region in &self.regions {
            // SAFETY: `host_addr`/`size` describe a live mapping owned by this region.
            let bytes: &[u8] =
                unsafe { ::core::slice::from_raw_parts(region.host_addr, region.size) };
            write_sparse(&mut file, bytes).context("writing RAM image")?;
            total += region.size as u64;
        }
        file.set_len(total).context("sizing RAM image")?;
        file.flush().context("flushing RAM image")?;
        Ok(())
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

        // SAFETY: `host_addr` is a live, writable mapping of `size` bytes that outlives the
        // partition's use of the guest-physical range.
        unsafe {
            WHvMapGpaRange(
                partition,
                host_addr.cast::<c_void>(),
                guest_phys,
                size as u64,
                rwx_flags(),
            )
            .with_context(|| format!("WHvMapGpaRange failed (gpa={guest_phys:#x}, size={size})"))?;
        }

        Ok(MemoryRegion {
            guest_phys,
            size,
            host_addr,
            partition,
            backing: Backing::Reserved,
        })
    }

    /// Maps one copy-on-write view of the snapshot section at file offset `file_off` and
    /// registers it with the partition as guest RAM at `guest_phys`.
    fn map_region_view(
        partition: WHV_PARTITION_HANDLE,
        mapping: HANDLE,
        file_off: u64,
        guest_phys: u64,
        size: u64,
    ) -> Result<MemoryRegion> {
        let size: usize = usize::try_from(size).context("region size overflows usize")?;

        // SAFETY: `mapping` is a live copy-on-write section; the offset is within it and 64 KiB
        // aligned (0 or `MMIO_GAP_START`), and `size` bytes remain from there.
        let view: MEMORY_MAPPED_VIEW_ADDRESS = unsafe {
            MapViewOfFile(
                mapping,
                FILE_MAP_COPY,
                (file_off >> 32) as u32,
                (file_off & 0xffff_ffff) as u32,
                size,
            )
        };
        if view.Value.is_null() {
            bail!("MapViewOfFile failed (offset={file_off:#x}, size={size})");
        }
        let host_addr: *mut u8 = view.Value.cast::<u8>();

        // SAFETY: `host_addr` is a live, writable (copy-on-write) view of `size` bytes.
        if let Err(e) = unsafe {
            WHvMapGpaRange(
                partition,
                view.Value,
                guest_phys,
                size as u64,
                rwx_flags(),
            )
        } {
            // SAFETY: `view` was just returned by `MapViewOfFile` and is not yet mapped elsewhere.
            unsafe {
                let _ = UnmapViewOfFile(view);
            }
            return Err(e)
                .with_context(|| format!("WHvMapGpaRange (view) failed (gpa={guest_phys:#x})"));
        }

        Ok(MemoryRegion {
            guest_phys,
            size,
            host_addr,
            partition,
            backing: Backing::Mapped,
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

    /// Reads a little-endian `u16` from guest RAM at `gpa` (0 if out of range).
    pub fn read_u16(&self, gpa: u64) -> u16 {
        let mut b: [u8; 2] = [0; 2];
        let _ = self.read_slice(gpa, &mut b);
        u16::from_le_bytes(b)
    }

    /// Reads a little-endian `u32` from guest RAM at `gpa` (0 if out of range).
    pub fn read_u32(&self, gpa: u64) -> u32 {
        let mut b: [u8; 4] = [0; 4];
        let _ = self.read_slice(gpa, &mut b);
        u32::from_le_bytes(b)
    }

    /// Reads a little-endian `u64` from guest RAM at `gpa` (0 if out of range).
    pub fn read_u64(&self, gpa: u64) -> u64 {
        let mut b: [u8; 8] = [0; 8];
        let _ = self.read_slice(gpa, &mut b);
        u64::from_le_bytes(b)
    }

    /// Writes a little-endian `u16` to guest RAM at `gpa` (ignored if out of range).
    pub fn write_u16(&self, gpa: u64, val: u16) {
        let _ = self.write_slice(gpa, &val.to_le_bytes());
    }

    /// Writes a little-endian `u32` to guest RAM at `gpa` (ignored if out of range).
    pub fn write_u32(&self, gpa: u64, val: u32) {
        let _ = self.write_slice(gpa, &val.to_le_bytes());
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

/// The read/write/execute flags used when registering a guest RAM range with the partition.
fn rwx_flags() -> WHV_MAP_GPA_RANGE_FLAGS {
    WHV_MAP_GPA_RANGE_FLAGS(
        WHvMapGpaRangeFlagRead.0 | WHvMapGpaRangeFlagWrite.0 | WHvMapGpaRangeFlagExecute.0,
    )
}

/// Marks `file` as a sparse file so that later seeks over zero runs leave real holes on disk
/// instead of physically-allocated zeros. Best-effort: errors (e.g. an unsupported filesystem)
/// are ignored, leaving a correct but fully-allocated image.
fn set_sparse(file: &::std::fs::File) {
    use ::std::os::windows::io::AsRawHandle;
    /// `FSCTL_SET_SPARSE` control code (`CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 49, METHOD_BUFFERED,
    /// FILE_SPECIAL_ACCESS)` == `0x0009_00C4`).
    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    let handle: HANDLE = HANDLE(file.as_raw_handle());
    let mut returned: u32 = 0;
    // SAFETY: `handle` is a live, writable file handle; `FSCTL_SET_SPARSE` reads and writes no
    // buffers, so the null in/out pointers are correct.
    unsafe {
        let _ = DeviceIoControl(handle, FSCTL_SET_SPARSE, None, 0, None, 0, Some(&mut returned), None);
    }
}

/// Writes `bytes` to `file`, seeking over runs of zero pages instead of writing them so the
/// resulting file is sparse. The file offset always advances by `bytes.len()`; the caller must
/// `set_len` afterwards so a trailing zero run is reflected in the file size.
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

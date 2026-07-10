// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The virt-fs mechanism -- packing a filesystem image, mapping it into guest RAM, and telling
// the guest where to find it -- is derived from the RAMFS device of the Nanvix Micro-VM
// (uservm), Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Virtual Filesystem (WHP)
//!
//! Exposes a host directory to the guest as a mountable device, mirroring the KVM backend's
//! [`crate::virtfs`]. The directory is packed into a filesystem image, the image is mapped into a
//! dedicated guest-physical region placed *above* the RAM reported to the guest (so the kernel
//! never allocates over it), and the guest is told where the image lives through the kernel
//! command line.
//!
//! On the guest side the region is surfaced by the in-kernel `phram` MTD driver (configured via
//! `phram.phram=<name>,<base>,<len>`), which the `mtdblock` layer turns into a block device
//! (`/dev/mtdblock0`). PID 1 then mounts it at the requested mount point (`virtfs_dir=<path>`)
//! with the type and mode the VMM selected (`virtfs_fs=<type> virtfs_mode=<ro|rw>`).
//!
//! Unlike the KVM backend -- which shells out to `mksquashfs`/`mke2fs` to build SquashFS/ext4
//! images -- Windows has no such tools, so the image is a **FAT filesystem built in pure Rust**
//! (the `fatfs` crate) and the guest mounts it as `vfat` (`CONFIG_VFAT_FS`). Two modes:
//!
//! - **read-only** (default `--mount`): the directory is packed into a FAT image mapped from
//!   private host memory (`VirtualAlloc`), mounted `-o ro`.
//! - **read-write** (`--mount-rw`, or implied by `--mount-image`): the directory is packed into a
//!   FAT image the guest mounts read-write. Without a backing file the image is mapped from
//!   private memory, so writes work but are discarded when the VM stops. With `--mount-image
//!   <file>` the image is mapped shared from that host file (`CreateFileMapping(PAGE_READWRITE)` +
//!   `MapViewOfFile(FILE_MAP_WRITE)`), so guest writes are flushed back and persist across runs
//!   (the file is created from the directory the first time and reused afterwards).
//!

use ::core::ffi::c_void;
use ::std::fs;
use ::std::io::{
    Cursor,
    Write as _,
};
use ::std::path::Path;

use ::anyhow::{
    Context,
    Result,
    bail,
};
use ::fatfs::{
    FileSystem,
    FormatVolumeOptions,
    FsOptions,
    ReadWriteSeek,
    format_volume,
};
use ::log::info;
use ::windows::Win32::Foundation::{
    CloseHandle,
    HANDLE,
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
    CreateFileMappingW,
    FILE_MAP_READ,
    FILE_MAP_WRITE,
    FlushViewOfFile,
    MEM_COMMIT,
    MEM_RELEASE,
    MEM_RESERVE,
    MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile,
    PAGE_READWRITE,
    UnmapViewOfFile,
    VirtualAlloc,
    VirtualFree,
};
use ::windows::core::PCWSTR;

use crate::layout::{
    MMIO_GAP_START,
    PAGE_SIZE,
    RAM_64BIT_START,
    align_up,
};

/// Name the guest `phram` device is registered under (it appears as MTD `virtfs`).
const VIRTFS_NAME: &str = "virtfs";

/// Minimum size of a read-write FAT image: enough for the filesystem metadata plus headroom for
/// the guest to create files.
const RW_IMAGE_MIN_BYTES: u64 = 16 << 20;

/// Minimum size of a read-only FAT image: FAT needs a few reserved/table sectors, so give even a
/// tiny directory comfortable headroom.
const RO_IMAGE_MIN_BYTES: u64 = 4 << 20;

/// What (and how) to export to the guest.
pub struct Options<'a> {
    /// Host directory to export.
    pub dir: &'a Path,
    /// Guest mount point (e.g. `/mnt/host`).
    pub target: &'a str,
    /// Mount the filesystem read-write instead of read-only.
    pub writable: bool,
    /// Optional host file backing a writable export; guest writes persist to it (implies
    /// `writable`).
    pub image: Option<&'a Path>,
    /// Optional size, in bytes, for a writable image (headroom for guest writes). Ignored for a
    /// read-only export and for an existing `image` file (which keeps its own size).
    pub size: Option<u64>,
}

///
/// # Description
///
/// Returns the guest-physical base at which the virt-fs image is mapped: the first page-aligned
/// address strictly above all guest RAM (and above the 4 GiB MMIO gap). Because this region is
/// never reported to the guest as usable RAM, the kernel will not allocate over the image; the
/// guest reaches it through the `phram` driver's `ioremap`.
///
pub fn virtfs_base(mem_bytes: u64) -> u64 {
    let high_ram: u64 = mem_bytes.saturating_sub(MMIO_GAP_START);
    RAM_64BIT_START + align_up(high_ram, PAGE_SIZE)
}

///
/// # Description
///
/// Builds the kernel command-line fragment that points the guest at the virt-fs image and tells
/// it how to mount it: `phram.phram=<name>,<base>,<len> virtfs_dir=<target> virtfs_fs=<fstype>
/// virtfs_mode=<mode>`. `len` is the size of the mapped region (a multiple of the page size).
///
pub fn cmdline_fragment(base: u64, len: u64, target: &str, fstype: &str, mode: &str) -> String {
    format!(
        "phram.phram={VIRTFS_NAME},{base:#x},{len:#x} \
         virtfs_dir={target} virtfs_fs={fstype} virtfs_mode={mode}"
    )
}

/// Sums the sizes of the regular files under `dir` (recursively), best-effort.
fn dir_apparent_size(dir: &Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => {
                    total = total.saturating_add(dir_apparent_size(&entry.path()))
                },
                Ok(ft) if ft.is_file() => {
                    if let Ok(md) = entry.metadata() {
                        total = total.saturating_add(md.len());
                    }
                },
                _ => {},
            }
        }
    }
    total
}

/// Returns the size, in bytes, of the FAT image to build for `dir`: the caller's `requested` size
/// (writable only) or a default, never smaller than the room needed to hold the seeded directory
/// plus filesystem metadata/headroom, clamped to a sensible minimum and page-aligned.
fn fat_image_size(dir: &Path, requested: Option<u64>, writable: bool) -> u64 {
    let used: u64 = dir_apparent_size(dir);
    let (needed, floor): (u64, u64) = if writable {
        (used.saturating_mul(2).saturating_add(8 << 20), RW_IMAGE_MIN_BYTES)
    } else {
        (used.saturating_mul(2).saturating_add(2 << 20), RO_IMAGE_MIN_BYTES)
    };
    let raw: u64 = requested.unwrap_or(floor).max(needed).max(floor);
    align_up(raw, PAGE_SIZE)
}

/// Recursively copies the host directory `host` into the FAT directory `node`, creating
/// subdirectories and files (parents are created before their contents).
fn populate<T: ReadWriteSeek>(node: &::fatfs::Dir<'_, T>, host: &Path) -> Result<()> {
    for entry in fs::read_dir(host).with_context(|| format!("reading {host:?}"))? {
        let entry = entry?;
        let name: String = entry.file_name().to_string_lossy().into_owned();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            let sub = node
                .create_dir(&name)
                .with_context(|| format!("creating FAT dir {name:?}"))?;
            populate(&sub, &entry.path())?;
        } else if ft.is_file() {
            let data: Vec<u8> = fs::read(entry.path())
                .with_context(|| format!("reading {:?}", entry.path()))?;
            let mut f = node
                .create_file(&name)
                .with_context(|| format!("creating FAT file {name:?}"))?;
            f.write_all(&data).with_context(|| format!("writing FAT file {name:?}"))?;
            f.flush().with_context(|| format!("flushing FAT file {name:?}"))?;
        }
    }
    Ok(())
}

/// Formats `size` bytes of `storage` as FAT and populates it from `dir`.
fn format_and_fill<T: ReadWriteSeek>(mut storage: T, dir: &Path) -> Result<()> {
    format_volume(&mut storage, FormatVolumeOptions::new()).context("formatting FAT image")?;
    let fs: FileSystem<T> =
        FileSystem::new(storage, FsOptions::new()).context("opening FAT image")?;
    populate(&fs.root_dir(), dir)?;
    // Dropping the filesystem flushes its buffered metadata back to the storage.
    fs.unmount().context("finalizing FAT image")?;
    Ok(())
}

/// Builds a `size`-byte FAT image from `dir` in memory and returns its bytes.
fn build_fat_bytes(dir: &Path, size: u64) -> Result<Vec<u8>> {
    if !dir.is_dir() {
        bail!("--mount path {dir:?} is not a directory");
    }
    let size: usize = usize::try_from(size).context("virt-fs image size overflows usize")?;
    let mut buf: Vec<u8> = vec![0u8; size];
    format_and_fill(Cursor::new(&mut buf[..]), dir)?;
    Ok(buf)
}

/// Builds a `size`-byte FAT image from `dir` directly into the host file `path`.
fn build_fat_file(dir: &Path, path: &Path, size: u64) -> Result<()> {
    if !dir.is_dir() {
        bail!("--mount path {dir:?} is not a directory");
    }
    let file: fs::File = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("creating read-write image {path:?}"))?;
    file.set_len(size).with_context(|| format!("sizing read-write image {path:?}"))?;
    format_and_fill(&file, dir)?;
    file.sync_all().with_context(|| format!("flushing read-write image {path:?}"))?;
    Ok(())
}

/// Ensures a persistent FAT image exists at `path` and returns its page-aligned size. A missing
/// or empty file is created from `dir` (using `size` if given); an existing one is reused
/// unchanged so guest edits from earlier runs survive.
fn prepare_fat_image_file(dir: &Path, path: &Path, size: Option<u64>) -> Result<u64> {
    let existing: u64 = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if existing > 0 {
        if !existing.is_multiple_of(PAGE_SIZE) {
            bail!(
                "existing --mount-image {path:?} size ({existing} bytes) is not a multiple of \
                 the page size ({PAGE_SIZE})"
            );
        }
        info!("virt-fs: reusing existing read-write image {path:?} ({existing} bytes)");
        return Ok(existing);
    }
    let size: u64 = fat_image_size(dir, size, true);
    build_fat_file(dir, path, size)?;
    Ok(size)
}

/// How a [`VirtFs`] mapping's host backing was obtained, so it can be released correctly.
enum Backing {
    /// Private, demand-zero memory from `VirtualAlloc` (ephemeral). Released with `VirtualFree`.
    Anonymous,
    /// A shared, file-backed view from `MapViewOfFile` (persistent). Flushed and released with
    /// `FlushViewOfFile` + `UnmapViewOfFile`.
    FileMapped,
}

///
/// # Description
///
/// A virt-fs image mapped into guest memory. Owns the backing host mapping and the WHP GPA
/// registration, and tears both down on drop (flushing first when the mapping is file-backed), so
/// it must be kept alive for as long as the guest may access the image.
///
pub struct VirtFs {
    /// Owning partition (needed to unmap the GPA range on drop).
    partition: WHV_PARTITION_HANDLE,
    /// Base guest-physical address the image is mapped at.
    guest_phys: u64,
    /// Size of the mapping, in bytes (page-aligned).
    size: usize,
    /// Host virtual address of the backing mapping.
    host_addr: *mut u8,
    /// How the host backing was obtained (selects the correct release path on drop).
    backing: Backing,
}

// SAFETY: The backing mapping is owned exclusively by this process for the VM's lifetime and is
// only accessed by the guest vCPU through the registered guest-physical range.
unsafe impl Send for VirtFs {}
unsafe impl Sync for VirtFs {}

impl Drop for VirtFs {
    fn drop(&mut self) {
        // SAFETY: `guest_phys`/`size` describe a range registered in a constructor, and
        // `host_addr` is the corresponding `VirtualAlloc` base or `MapViewOfFile` view.
        unsafe {
            let _ = WHvUnmapGpaRange(self.partition, self.guest_phys, self.size as u64);
            match self.backing {
                Backing::Anonymous => {
                    let _ = VirtualFree(self.host_addr.cast::<c_void>(), 0, MEM_RELEASE);
                },
                Backing::FileMapped => {
                    let view: MEMORY_MAPPED_VIEW_ADDRESS =
                        MEMORY_MAPPED_VIEW_ADDRESS { Value: self.host_addr.cast::<c_void>() };
                    // Flush guest writes back to the backing file before tearing the view down.
                    let _ = FlushViewOfFile(self.host_addr.cast::<c_void>(), self.size);
                    let _ = UnmapViewOfFile(view);
                },
            }
        }
    }
}

impl VirtFs {
    /// Size of the mapped region, in bytes (the value handed to `phram` as its length).
    fn len(&self) -> u64 {
        self.size as u64
    }

    /// Registers `host_addr`/`size` with `partition` as a read/write/execute guest-physical range
    /// at `base`.
    fn register(
        partition: WHV_PARTITION_HANDLE,
        host_addr: *mut u8,
        base: u64,
        size: usize,
    ) -> Result<()> {
        // SAFETY: `host_addr` is a live, writable mapping of `size` bytes that the caller keeps
        // alive for as long as the partition may access the range.
        unsafe {
            WHvMapGpaRange(partition, host_addr.cast::<c_void>(), base, size as u64, rwx_flags())
                .with_context(|| format!("WHvMapGpaRange failed for virt-fs (gpa={base:#x})"))
        }
    }

    /// Maps `bytes` into a fresh private, page-aligned region and registers it at `base`. The
    /// region is zero-padded beyond the image; the FAT boot sector records the true geometry, so
    /// the padding is ignored.
    fn map_anonymous(partition: WHV_PARTITION_HANDLE, base: u64, bytes: &[u8]) -> Result<Self> {
        let size: usize = align_up(bytes.len() as u64, PAGE_SIZE) as usize;

        // SAFETY: A standard reserve+commit request; the pointer is checked for null before use.
        let host_addr: *mut c_void =
            unsafe { VirtualAlloc(None, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        if host_addr.is_null() {
            bail!("VirtualAlloc failed for virt-fs image (size={size})");
        }
        let host_addr: *mut u8 = host_addr.cast::<u8>();

        // SAFETY: The destination is valid for `size >= bytes.len()` bytes and does not overlap
        // the source.
        unsafe {
            ::core::ptr::copy_nonoverlapping(bytes.as_ptr(), host_addr, bytes.len());
        }

        if let Err(e) = Self::register(partition, host_addr, base, size) {
            // SAFETY: `host_addr` is the base just returned by `VirtualAlloc`.
            unsafe {
                let _ = VirtualFree(host_addr.cast::<c_void>(), 0, MEM_RELEASE);
            }
            return Err(e);
        }
        Ok(Self { partition, guest_phys: base, size, host_addr, backing: Backing::Anonymous })
    }

    /// Maps the whole of file `path` (`size` bytes) shared so guest writes are flushed back to it,
    /// and registers it at `base`.
    fn map_shared_file(
        partition: WHV_PARTITION_HANDLE,
        base: u64,
        path: &Path,
        size: u64,
    ) -> Result<Self> {
        use ::std::os::windows::io::AsRawHandle;

        let size: usize = usize::try_from(size).context("virt-fs image size overflows usize")?;
        let file: fs::File = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening read-write image {path:?}"))?;
        let file_handle: HANDLE = HANDLE(file.as_raw_handle());

        // A read-write section spanning the whole file (max-size 0 means "use the file size").
        let mapping: HANDLE =
            unsafe { CreateFileMappingW(file_handle, None, PAGE_READWRITE, 0, 0, PCWSTR::null()) }
                .with_context(|| format!("CreateFileMapping for read-write image {path:?}"))?;

        // SAFETY: `mapping` is a live read-write section; the whole file is mapped from offset 0.
        let view: MEMORY_MAPPED_VIEW_ADDRESS =
            unsafe { MapViewOfFile(mapping, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, size) };
        // The view holds its own reference to the section, so the handle can be closed now.
        // SAFETY: `mapping` is the handle just returned by `CreateFileMappingW`.
        unsafe {
            let _ = CloseHandle(mapping);
        }
        if view.Value.is_null() {
            bail!("MapViewOfFile failed for read-write image {path:?} (size={size})");
        }
        let host_addr: *mut u8 = view.Value.cast::<u8>();

        if let Err(e) = Self::register(partition, host_addr, base, size) {
            // SAFETY: `view` was just returned by `MapViewOfFile` and is not mapped elsewhere.
            unsafe {
                let _ = UnmapViewOfFile(view);
            }
            return Err(e);
        }
        Ok(Self { partition, guest_phys: base, size, host_addr, backing: Backing::FileMapped })
    }
}

///
/// # Description
///
/// Builds the requested export image (a FAT filesystem), maps it into guest memory above the
/// guest's RAM, and returns the live mapping together with the kernel command-line fragment that
/// tells the guest where the image is, where to mount it, and how.
///
/// # Parameters
///
/// - `partition`: The WHP partition, used to register the image's guest-physical range.
/// - `mem_bytes`: Total guest RAM, in bytes (determines where the image is placed).
/// - `opts`: What to export and how (directory, mount point, access mode, optional backing file).
///
pub fn load(
    partition: WHV_PARTITION_HANDLE,
    mem_bytes: u64,
    opts: Options,
) -> Result<(VirtFs, String)> {
    let base: u64 = virtfs_base(mem_bytes);

    let (virtfs, mode): (VirtFs, &str) = if opts.writable {
        match opts.image {
            Some(path) => {
                // Persistent read-write: format (or reuse) the backing file and map it shared.
                let size: u64 = prepare_fat_image_file(opts.dir, path, opts.size)?;
                let fs: VirtFs = VirtFs::map_shared_file(partition, base, path, size)?;
                info!(
                    "virt-fs: exported {:?} as read-write FAT at gpa {base:#x}, len {:#x}, backed \
                     by {path:?}; mounting at {}",
                    opts.dir,
                    fs.len(),
                    opts.target
                );
                (fs, "rw")
            },
            None => {
                // Ephemeral read-write: format an image and map it from private memory.
                let size: u64 = fat_image_size(opts.dir, opts.size, true);
                let image: Vec<u8> = build_fat_bytes(opts.dir, size)?;
                let fs: VirtFs = VirtFs::map_anonymous(partition, base, &image)?;
                info!(
                    "virt-fs: exported {:?} as read-write FAT ({} bytes, ephemeral) at gpa \
                     {base:#x}, len {:#x}; mounting at {}",
                    opts.dir,
                    image.len(),
                    fs.len(),
                    opts.target
                );
                (fs, "rw")
            },
        }
    } else {
        // Read-only: pack a FAT image and map it from private memory.
        let size: u64 = fat_image_size(opts.dir, None, false);
        let image: Vec<u8> = build_fat_bytes(opts.dir, size)?;
        let fs: VirtFs = VirtFs::map_anonymous(partition, base, &image)?;
        info!(
            "virt-fs: exported {:?} as read-only FAT ({} bytes) at gpa {base:#x}, len {:#x}; \
             mounting at {}",
            opts.dir,
            image.len(),
            fs.len(),
            opts.target
        );
        (fs, "ro")
    };

    let fragment: String = cmdline_fragment(base, virtfs.len(), opts.target, "vfat", mode);
    Ok((virtfs, fragment))
}

/// The read/write/execute flags used when registering the image's guest-physical range.
fn rwx_flags() -> WHV_MAP_GPA_RANGE_FLAGS {
    WHV_MAP_GPA_RANGE_FLAGS(
        WHvMapGpaRangeFlagRead.0 | WHvMapGpaRangeFlagWrite.0 | WHvMapGpaRangeFlagExecute.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The image is always placed at a page-aligned address at or above the 4 GiB boundary, and
    /// strictly above any high (>4 GiB) RAM the guest is given.
    #[test]
    fn base_is_placed_above_reported_ram() {
        // Small guest: no high RAM, so the image sits exactly at the 4 GiB boundary.
        assert_eq!(virtfs_base(512 << 20), RAM_64BIT_START);
        // Large guest: image starts above the high RAM region, page-aligned.
        let mem: u64 = 6u64 << 30;
        let base: u64 = virtfs_base(mem);
        let high_ram: u64 = mem - MMIO_GAP_START;
        assert_eq!(base, RAM_64BIT_START + high_ram);
        assert_eq!(base % PAGE_SIZE, 0);
    }

    /// A writable image respects the requested size but never drops below the metadata floor, and
    /// is always page-aligned.
    #[test]
    fn image_size_is_clamped_and_aligned() {
        let dir: &Path = Path::new(".");
        let big: u64 = fat_image_size(dir, Some(200 << 20), true);
        assert_eq!(big % PAGE_SIZE, 0);
        assert!(big >= 200 << 20);
        // A tiny request is raised to the read-write floor.
        let small: u64 = fat_image_size(dir, Some(1), true);
        assert!(small >= RW_IMAGE_MIN_BYTES);
    }

    /// The command-line fragment carries the phram triple and the mount tokens.
    #[test]
    fn fragment_has_phram_and_mount_tokens() {
        let frag: String = cmdline_fragment(0x1_0000_0000, 0x100_0000, "/mnt/host", "vfat", "rw");
        assert!(frag.contains("phram.phram=virtfs,0x100000000,0x1000000"));
        assert!(frag.contains("virtfs_dir=/mnt/host"));
        assert!(frag.contains("virtfs_fs=vfat"));
        assert!(frag.contains("virtfs_mode=rw"));
    }
}

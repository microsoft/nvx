// Copyright(c) The microvm authors.
// Licensed under the MIT License.
//
// The virt-fs mechanism -- packing a filesystem image, mapping it into guest RAM, and telling
// the guest where to find it -- is derived from the RAMFS device of the Nanvix Micro-VM
// (uservm), Copyright(c) The Maintainers of Nanvix, MIT License.

//!
//! # Virtual Filesystem (virt-fs)
//!
//! Exposes a host directory to the guest as a mountable device. The directory is packed into a
//! filesystem image on the host, the image is mapped into a dedicated guest-physical memory
//! region (placed *above* the RAM reported to the guest, so the kernel never allocates over it),
//! and the guest is told where the image lives through the kernel command line.
//!
//! On the guest side the region is surfaced by the in-kernel `phram` MTD driver (configured via
//! `phram.phram=<name>,<base>,<len>`), which the `mtdblock` layer turns into a block device
//! (`/dev/mtdblock0`). PID 1 then mounts it at the requested mount point (passed as
//! `virtfs_dir=<path>`), using the filesystem type and access mode the VMM selected
//! (`virtfs_fs=<type> virtfs_mode=<ro|rw>`).
//!
//! Two access modes are supported:
//!
//! - **read-only** (default): the directory is packed into a compact, read-only **SquashFS**
//!   image and mapped from anonymous host memory.
//! - **read-write** (`--mount-rw`, or implied by `--mount-image`): the directory is packed into
//!   an **ext4** image the guest mounts read-write. Without a backing file the image is mapped
//!   from anonymous memory, so writes work but are discarded when the VM stops. With
//!   `--mount-image <file>` the ext4 image is mapped `MAP_SHARED` from that host file, so guest
//!   writes are flushed back to it and persist across runs (the file is created from the
//!   directory the first time and reused afterwards).
//!
//! This mirrors how the Nanvix micro-VM ("uservm") exposes its RAMFS: the VMM places a
//! filesystem image in guest memory and hands the guest its base and size, and the guest mounts
//! it. Here the "registers" are kernel-command-line tokens rather than fixed low-memory words,
//! which is the idiomatic way to point a Linux `phram` device at a memory range.
//!

use ::std::fs;
use ::std::path::Path;
use ::std::process::Command;

use ::anyhow::{
    Context,
    Result,
    bail,
};
use ::kvm_bindings::kvm_userspace_memory_region;
use ::kvm_ioctls::VmFd;
use ::log::info;

use crate::layout::{
    MMIO_GAP_START,
    PAGE_SIZE,
    RAM_64BIT_START,
    align_up,
};

/// Name the guest `phram` device is registered under (it appears as MTD `virtfs`).
const VIRTFS_NAME: &str = "virtfs";

/// KVM memory slot used for the virt-fs image. Guest RAM uses slots 0 (low) and 1 (high), so a
/// distinct, higher index keeps the image mapping clear of them.
const VIRTFS_SLOT: u32 = 8;

/// Default guest mount point used when `--mount-target` is not given.
pub const DEFAULT_MOUNT_TARGET: &str = "/mnt/host";

/// Minimum size of a read-write ext4 image: enough for the filesystem's own metadata (a small
/// journal, inode/block tables) plus headroom for the guest to create files.
const RW_IMAGE_MIN_BYTES: u64 = 16 << 20;

/// What (and how) to export to the guest.
pub struct Options<'a> {
    /// Host directory to export.
    pub dir: &'a Path,
    /// Guest mount point (e.g. `/mnt/host`).
    pub target: &'a str,
    /// Mount the filesystem read-write (ext4) instead of read-only (SquashFS).
    pub writable: bool,
    /// Optional host file backing a writable export; guest writes persist to it (implies
    /// `writable`).
    pub image: Option<&'a Path>,
}

///
/// # Description
///
/// Returns the guest-physical base at which the virt-fs image is mapped: the first page-aligned
/// address strictly above all guest RAM (and above the 4 GiB MMIO gap). Because this region is
/// never reported to the guest as usable RAM, the kernel will not allocate over the image; the
/// guest reaches it through an `ioremap` performed by the `phram` driver.
///
pub fn virtfs_base(mem_bytes: u64) -> u64 {
    // Bytes of guest RAM that live above the 4 GiB boundary (0 for small guests).
    let high_ram: u64 = mem_bytes.saturating_sub(MMIO_GAP_START);
    RAM_64BIT_START + align_up(high_ram, PAGE_SIZE)
}

///
/// # Description
///
/// Builds the kernel command-line fragment that points the guest at the virt-fs image and tells
/// it how to mount it: `phram.phram=<name>,<base>,<len> virtfs_dir=<target> virtfs_fs=<fstype>
/// virtfs_mode=<mode>`.
///
/// `len` must be a multiple of the page size (the `phram` erase size) and is the size of the
/// mapped region, not necessarily the exact image length.
///
pub fn cmdline_fragment(base: u64, len: u64, target: &str, fstype: &str, mode: &str) -> String {
    format!(
        "phram.phram={VIRTFS_NAME},{base:#x},{len:#x} \
         virtfs_dir={target} virtfs_fs={fstype} virtfs_mode={mode}"
    )
}

///
/// # Description
///
/// Packs the host directory `dir` into a read-only, gzip-compressed SquashFS image and returns
/// the raw image bytes. Requires `mksquashfs` (the `squashfs-tools` package) on the host.
///
/// SquashFS is a natural fit for the read-only export: it is a compact, read-only filesystem
/// produced directly from a directory tree, and the guest mounts it without any journal recovery
/// or writeback.
///
pub fn build_squashfs(dir: &Path) -> Result<Vec<u8>> {
    if !dir.is_dir() {
        bail!("--mount path {dir:?} is not a directory");
    }

    let tmp: ::std::path::PathBuf =
        ::std::env::temp_dir().join(format!("microvm-virtfs-{}.sqfs", ::std::process::id()));
    // A stale image from a previous crashed run would otherwise be appended to.
    let _ = fs::remove_file(&tmp);

    let status = Command::new("mksquashfs")
        .arg(dir)
        .arg(&tmp)
        // `-all-root` maps every file to uid/gid 0 so the guest (running as root) can read the
        // tree regardless of host ownership; `-noappend` rewrites the image; `-comp gzip`
        // matches the guest's CONFIG_SQUASHFS_ZLIB decompressor.
        .args([
            "-comp",
            "gzip",
            "-all-root",
            "-noappend",
            "-no-progress",
            "-quiet",
        ])
        .status()
        .context("failed to run mksquashfs (is the squashfs-tools package installed?)")?;
    if !status.success() {
        let _ = fs::remove_file(&tmp);
        bail!("mksquashfs failed for {dir:?} (exit status {status})");
    }

    let image: Vec<u8> =
        fs::read(&tmp).with_context(|| format!("reading squashfs image {tmp:?}"))?;
    let _ = fs::remove_file(&tmp);

    if image.len() < 4 || &image[0..4] != b"hsqs" {
        bail!("mksquashfs did not produce a valid SquashFS image for {dir:?}");
    }
    Ok(image)
}

///
/// # Description
///
/// Returns the size, in bytes, of the ext4 image to build for read-write exports of `dir`: the
/// directory's apparent size plus generous headroom, clamped to a sensible minimum and rounded
/// up to a whole number of pages.
///
fn ext4_image_size(dir: &Path) -> u64 {
    let used: u64 = dir_apparent_size(dir);
    let raw: u64 = RW_IMAGE_MIN_BYTES.max(used.saturating_mul(2).saturating_add(8 << 20));
    align_up(raw, PAGE_SIZE)
}

/// Sums the sizes of the regular files under `dir` (recursively), best-effort.
fn dir_apparent_size(dir: &Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => total = total.saturating_add(dir_apparent_size(&entry.path())),
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

///
/// # Description
///
/// Builds an ext4 image of exactly `size` bytes at `path`, populated from the directory `dir`.
/// Requires `mke2fs` (the `e2fsprogs` package). The file is (re)created at the requested size and
/// `mke2fs` formats the whole device.
///
fn build_ext4_file(dir: &Path, path: &Path, size: u64) -> Result<()> {
    if !dir.is_dir() {
        bail!("--mount path {dir:?} is not a directory");
    }

    // Pre-size the backing file so `mke2fs` formats a device of exactly `size` bytes.
    let file: fs::File =
        fs::File::create(path).with_context(|| format!("creating ext4 image {path:?}"))?;
    file.set_len(size).with_context(|| format!("sizing ext4 image {path:?}"))?;
    drop(file);

    let status = Command::new("mke2fs")
        .args(["-q", "-F", "-t", "ext4", "-b", "4096", "-L", VIRTFS_NAME, "-d"])
        .arg(dir)
        .arg(path)
        .status()
        .context("failed to run mke2fs (is the e2fsprogs package installed?)")?;
    if !status.success() {
        let _ = fs::remove_file(path);
        bail!("mke2fs failed for {dir:?} (exit status {status})");
    }
    Ok(())
}

/// Builds a throwaway ext4 image from `dir` and returns its bytes (for an ephemeral, anonymously
/// mapped read-write export).
fn build_ext4_bytes(dir: &Path) -> Result<Vec<u8>> {
    let size: u64 = ext4_image_size(dir);
    let tmp: ::std::path::PathBuf =
        ::std::env::temp_dir().join(format!("microvm-virtfs-{}.ext4", ::std::process::id()));
    build_ext4_file(dir, &tmp, size)?;
    let image: Result<Vec<u8>> =
        fs::read(&tmp).with_context(|| format!("reading ext4 image {tmp:?}"));
    let _ = fs::remove_file(&tmp);
    image
}

/// Ensures a persistent ext4 image exists at `path` and returns its page-aligned size. A missing
/// or empty file is created from `dir`; an existing one is reused unchanged so guest edits from
/// earlier runs survive.
fn prepare_ext4_image_file(dir: &Path, path: &Path) -> Result<u64> {
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
    let size: u64 = ext4_image_size(dir);
    build_ext4_file(dir, path, size)?;
    Ok(size)
}

///
/// # Description
///
/// A virt-fs image mapped into guest memory. Owns the backing host mapping and unmaps it on drop
/// (flushing first when the mapping is a shared, file-backed one), so it must be kept alive for
/// as long as the guest may access the image.
///
pub struct VirtFs {
    /// Host virtual address of the backing mapping.
    host_addr: *mut u8,
    /// Size of the backing mapping, in bytes (page-aligned).
    size: usize,
    /// Whether the mapping is `MAP_SHARED` and file-backed (dirty pages must be flushed on drop).
    shared: bool,
}

// SAFETY: The backing mapping is owned exclusively by this process for the VM's lifetime and is
// only accessed by the guest vCPU through the registered KVM memory slot.
unsafe impl Send for VirtFs {}
unsafe impl Sync for VirtFs {}

impl Drop for VirtFs {
    fn drop(&mut self) {
        // SAFETY: `host_addr`/`size` describe the mapping produced by `mmap` in the constructors.
        unsafe {
            if self.shared {
                // Flush guest writes back to the backing file before tearing the mapping down.
                ::libc::msync(self.host_addr.cast::<::libc::c_void>(), self.size, ::libc::MS_SYNC);
            }
            ::libc::munmap(self.host_addr.cast::<::libc::c_void>(), self.size);
        }
    }
}

impl VirtFs {
    /// Registers this mapping with `vm_fd` as the virt-fs memory slot at guest-physical `base`.
    fn register(&self, vm_fd: &VmFd, base: u64) -> Result<()> {
        let region: kvm_userspace_memory_region = kvm_userspace_memory_region {
            slot: VIRTFS_SLOT,
            flags: 0,
            guest_phys_addr: base,
            memory_size: self.size as u64,
            userspace_addr: self.host_addr as u64,
        };
        // SAFETY: `region` describes the live mapping owned by `self`, which outlives the VM's use
        // of the memory slot (the caller keeps it alive).
        unsafe {
            vm_fd
                .set_user_memory_region(region)
                .context("KVM_SET_USER_MEMORY_REGION failed for virt-fs image")?;
        }
        Ok(())
    }

    /// Maps `bytes` into a fresh anonymous, page-aligned region and registers it at `base`. The
    /// region is padded with zeros beyond the image; the filesystem superblock records the true
    /// length, so the padding is ignored.
    fn map_anonymous(vm_fd: &VmFd, base: u64, bytes: &[u8]) -> Result<Self> {
        let size: usize = align_up(bytes.len() as u64, PAGE_SIZE) as usize;

        // SAFETY: A standard anonymous mapping request; the pointer is checked against MAP_FAILED.
        let host_addr: *mut ::libc::c_void = unsafe {
            ::libc::mmap(
                ::core::ptr::null_mut(),
                size,
                ::libc::PROT_READ | ::libc::PROT_WRITE,
                ::libc::MAP_PRIVATE | ::libc::MAP_ANONYMOUS | ::libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if host_addr == ::libc::MAP_FAILED {
            bail!("mmap failed for virt-fs image (size={size})");
        }
        let host_addr: *mut u8 = host_addr.cast::<u8>();
        let this: Self = Self { host_addr, size, shared: false };

        // SAFETY: The destination is valid for `size >= bytes.len()` bytes and does not overlap
        // the source; both are byte-aligned.
        unsafe {
            ::core::ptr::copy_nonoverlapping(bytes.as_ptr(), host_addr, bytes.len());
        }
        this.register(vm_fd, base)?;
        Ok(this)
    }

    /// Maps the whole of file `path` (`size` bytes) `MAP_SHARED` so guest writes are flushed back
    /// to it, and registers it at `base`.
    fn map_shared_file(vm_fd: &VmFd, base: u64, path: &Path, size: u64) -> Result<Self> {
        use ::std::os::fd::AsRawFd;

        let size: usize = usize::try_from(size).context("virt-fs image size overflows usize")?;
        let file: fs::File = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening read-write image {path:?}"))?;

        // SAFETY: A standard shared file mapping; the pointer is checked against MAP_FAILED. The
        // mapping keeps the file alive in the kernel, so the `File` handle may be dropped after.
        let host_addr: *mut ::libc::c_void = unsafe {
            ::libc::mmap(
                ::core::ptr::null_mut(),
                size,
                ::libc::PROT_READ | ::libc::PROT_WRITE,
                ::libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if host_addr == ::libc::MAP_FAILED {
            bail!("mmap failed for virt-fs image {path:?} (size={size})");
        }
        let this: Self = Self { host_addr: host_addr.cast::<u8>(), size, shared: true };
        this.register(vm_fd, base)?;
        Ok(this)
    }

    /// Size of the mapped region, in bytes (the value handed to `phram` as its length).
    fn len(&self) -> u64 {
        self.size as u64
    }
}

///
/// # Description
///
/// Builds the requested export image, maps it into guest memory as a dedicated KVM memory slot
/// above the guest's RAM, and returns the live mapping together with the kernel command-line
/// fragment that tells the guest where the image is, where to mount it, and how.
///
/// # Parameters
///
/// - `vm_fd`: The VM, used to register the image's memory slot.
/// - `mem_bytes`: Total guest RAM, in bytes (determines where the image is placed).
/// - `opts`: What to export and how (directory, mount point, access mode, optional backing file).
///
/// # Returns
///
/// Upon success, returns the owning [`VirtFs`] mapping and the command-line fragment to append.
///
pub fn load(vm_fd: &VmFd, mem_bytes: u64, opts: Options) -> Result<(VirtFs, String)> {
    let base: u64 = virtfs_base(mem_bytes);

    let (virtfs, fstype, mode): (VirtFs, &str, &str) = if opts.writable {
        match opts.image {
            Some(path) => {
                // Persistent read-write: format (or reuse) the backing file and map it shared.
                let size: u64 = prepare_ext4_image_file(opts.dir, path)?;
                let fs: VirtFs = VirtFs::map_shared_file(vm_fd, base, path, size)?;
                info!(
                    "virt-fs: exported {:?} as read-write ext4 at gpa {base:#x}, len {:#x}, \
                     backed by {path:?}; mounting at {}",
                    opts.dir,
                    fs.len(),
                    opts.target
                );
                (fs, "ext4", "rw")
            },
            None => {
                // Ephemeral read-write: format an image and map it from anonymous memory.
                let image: Vec<u8> = build_ext4_bytes(opts.dir)?;
                let fs: VirtFs = VirtFs::map_anonymous(vm_fd, base, &image)?;
                info!(
                    "virt-fs: exported {:?} as read-write ext4 ({} bytes, ephemeral) at gpa \
                     {base:#x}, len {:#x}; mounting at {}",
                    opts.dir,
                    image.len(),
                    fs.len(),
                    opts.target
                );
                (fs, "ext4", "rw")
            },
        }
    } else {
        // Read-only: pack a SquashFS image and map it from anonymous memory.
        let image: Vec<u8> = build_squashfs(opts.dir)?;
        let fs: VirtFs = VirtFs::map_anonymous(vm_fd, base, &image)?;
        info!(
            "virt-fs: exported {:?} as read-only SquashFS ({} bytes) at gpa {base:#x}, len \
             {:#x}; mounting at {}",
            opts.dir,
            image.len(),
            fs.len(),
            opts.target
        );
        (fs, "squashfs", "ro")
    };

    let fragment: String = cmdline_fragment(base, virtfs.len(), opts.target, fstype, mode);
    Ok((virtfs, fragment))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The image is always placed at a page-aligned address at or above the 4 GiB boundary, and
    /// strictly above any high (>4 GiB) RAM the guest is given.
    #[test]
    fn base_is_placed_above_reported_ram() {
        // Small guest: no RAM above 4 GiB, so the image sits exactly at 4 GiB.
        assert_eq!(virtfs_base(512 << 20), RAM_64BIT_START);
        assert_eq!(virtfs_base(MMIO_GAP_START), RAM_64BIT_START);

        // Large guest: 4 GiB of RAM => 1 GiB lands above the gap, so the image sits at 5 GiB.
        let base: u64 = virtfs_base(4u64 << 30);
        assert_eq!(base, RAM_64BIT_START + (1u64 << 30));
        assert_eq!(base % PAGE_SIZE, 0);
        assert!(base >= RAM_64BIT_START);
    }

    /// The command-line fragment carries a page-aligned length and hex-formatted addresses in the
    /// exact form the built-in `phram` parser expects, plus the mount-point/type/mode tokens.
    #[test]
    fn cmdline_fragment_has_expected_form() {
        let ro = cmdline_fragment(0x1_0000_0000, 0x10_0000, "/mnt/host", "squashfs", "ro");
        assert_eq!(
            ro,
            "phram.phram=virtfs,0x100000000,0x100000 virtfs_dir=/mnt/host \
             virtfs_fs=squashfs virtfs_mode=ro"
        );

        let rw = cmdline_fragment(0x1_4000_0000, 0x100_0000, "/data", "ext4", "rw");
        assert_eq!(
            rw,
            "phram.phram=virtfs,0x140000000,0x1000000 virtfs_dir=/data \
             virtfs_fs=ext4 virtfs_mode=rw"
        );
    }

    /// The read-write image size is clamped to the minimum and page-aligned for a small tree.
    #[test]
    fn ext4_image_size_is_clamped_and_aligned() {
        let dir = ::std::env::temp_dir().join(format!("microvm-virtfs-sz-{}", ::std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a"), b"small").unwrap();

        let size = ext4_image_size(&dir);
        let _ = fs::remove_dir_all(&dir);

        assert!(size >= RW_IMAGE_MIN_BYTES);
        assert_eq!(size % PAGE_SIZE, 0);
    }

    /// `build_squashfs` packs a directory tree into a mountable SquashFS image (magic `hsqs`).
    /// Skips gracefully when `mksquashfs` is unavailable so the suite still passes without it.
    #[test]
    fn build_squashfs_produces_squashfs() {
        if Command::new("mksquashfs").arg("-version").output().is_err() {
            eprintln!("skipping: mksquashfs not installed");
            return;
        }

        let dir =
            ::std::env::temp_dir().join(format!("microvm-virtfs-sq-{}", ::std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("hello.txt"), b"hello virt-fs\n").unwrap();
        fs::write(dir.join("sub").join("nested"), b"nested\n").unwrap();

        let image = build_squashfs(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert!(image.len() >= 4);
        assert_eq!(&image[0..4], b"hsqs", "image must start with the SquashFS magic");
    }

    /// `build_ext4_bytes` packs a directory tree into a mountable ext4 image (superblock magic
    /// `0xEF53` at offset 0x438). Skips gracefully when `mke2fs` is unavailable.
    #[test]
    fn build_ext4_produces_ext4() {
        if Command::new("mke2fs").arg("-V").output().is_err() {
            eprintln!("skipping: mke2fs not installed");
            return;
        }

        let dir =
            ::std::env::temp_dir().join(format!("microvm-virtfs-e4-{}", ::std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("hello.txt"), b"hello rw\n").unwrap();

        let image = build_ext4_bytes(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert!(image.len() as u64 >= RW_IMAGE_MIN_BYTES);
        assert!(image.len() >= 0x43a);
        assert_eq!(&image[0x438..0x43a], &[0x53, 0xef], "image must carry the ext4 magic");
    }

    /// A non-directory `--mount` argument is rejected before any image build is attempted.
    #[test]
    fn build_squashfs_rejects_non_directory() {
        let file =
            ::std::env::temp_dir().join(format!("microvm-virtfs-nf-{}", ::std::process::id()));
        fs::write(&file, b"x").unwrap();
        let err = build_squashfs(&file).unwrap_err();
        let _ = fs::remove_file(&file);
        assert!(err.to_string().contains("is not a directory"));
    }
}

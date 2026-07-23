# Virt-fs

`--mount <dir>` exposes a host directory to the guest without emulating a general-purpose block
controller. NVX packs the directory into a filesystem image, maps that image into a dedicated
guest-physical range above reported RAM, and describes the range on the kernel command line.

The guest's built-in `phram` driver maps the range, `mtdblock` exposes `/dev/mtdblock0`, and PID 1
mounts it at `--mount-target` (default `/mnt/host`).

## Backend formats

The CLI is shared, but image formats are host-specific:

| Host | Read-only | Read-write | Host builder |
| --- | --- | --- | --- |
| Linux/KVM | SquashFS | ext4 | `mksquashfs` or `mke2fs` |
| Windows/WHP | FAT mounted as VFAT | FAT mounted as VFAT | Rust `fatfs` crate |

The kernel configuration includes `phram`, `mtdblock`, SquashFS, ext4, and VFAT so one guest kernel
can mount either backend's image.

## Access modes

| Options | Mode | Lifetime |
| --- | --- | --- |
| `--mount <dir>` | Read-only | Rebuilt from the directory for this launch. |
| `--mount <dir> --mount-rw` | Read-write | Private memory; guest changes are discarded on exit. |
| `--mount <dir> --mount-image <file>` | Read-write | Shared host file; guest changes persist. |

`--mount-image` implies `--mount-rw`. A missing or empty image is seeded from the host directory.
An existing non-empty image is reused unchanged, so later host-directory changes are not imported.
`--mount-size <MiB>` controls a newly created writable image and is ignored when reusing an existing
one. Writable images have a 16 MiB minimum and are automatically enlarged when seeded content
needs more room.

## Examples

Read-only export:

```console
./target/release/microvm \
  --kernel "$HOME/build/vmlinux" \
  --initrd "$HOME/build/initramfs.cpio.gz" \
  --mount ./shared \
  --mount-target /mnt/host
```

Ephemeral read-write export:

```console
./target/release/microvm \
  --kernel "$HOME/build/vmlinux" \
  --initrd "$HOME/build/initramfs.cpio.gz" \
  --mount ./shared \
  --mount-rw \
  --mount-size 192
```

Persistent read-write export:

```console
./target/release/microvm \
  --kernel "$HOME/build/vmlinux" \
  --initrd "$HOME/build/initramfs.cpio.gz" \
  --mount ./shared \
  --mount-image ./host.img \
  --mount-size 192
```

The workflow wrapper exposes the same settings:

```console
python scripts/nvx.py run \
  --mount ./shared \
  --mount-target /mnt/host \
  --mount-image ./host.img \
  --mount-size 192
```

On Windows, use PowerShell path syntax and `microvm.exe`; no external filesystem builder is needed.

Inside the guest:

```console
/ # mount | grep /mnt/host
/ # ls -la /mnt/host
/ # sync
```

Run `sync` before shutdown when persistence matters. NVX also flushes a persistent shared mapping
as it tears the device down.

## Linux host requirements

Install `squashfs-tools` for the default read-only mode and `e2fsprogs` for writable images:

```console
sudo apt-get install -y squashfs-tools e2fsprogs
```

An ext4 image can be inspected without mounting it as root:

```console
debugfs -R 'ls -l /' host.img
debugfs -R 'cat /path/in/image' host.img
```

## Implementation details

For guest RAM size $R$, the image starts at the first page-aligned address above all high RAM and
the 4 GiB MMIO gap. The range is omitted from the PVH usable-memory map, so Linux reaches it only
through `ioremap` in `phram` and cannot allocate ordinary pages over it.

NVX appends tokens equivalent to:

```text
phram.phram=virtfs,<base>,<length> \
virtfs_dir=<target> virtfs_fs=<type> virtfs_mode=<ro|rw>
```

KVM registers a separate userspace memory slot. WHP registers a GPA range backed by either private
`VirtualAlloc` memory or a shared Windows file mapping. Persistent KVM images use `MAP_SHARED`;
persistent WHP images use `CreateFileMapping(PAGE_READWRITE)` and `MapViewOfFile(FILE_MAP_WRITE)`.

Run `python scripts/nvx.py bench-virtfs` to measure sequential guest I/O and verify that a payload
survives image reuse. Metric names and sample counts are in [Benchmark Reference](benchmark.md).

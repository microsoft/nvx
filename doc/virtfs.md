# Virt-fs

`--mount <dir>` exposes a host directory live to guest Linux through a virtio-fs device. NVX does
not build a filesystem image, reserve guest RAM for one, or copy data back when the VM exits.
Guest filesystem requests are FUSE messages carried over virtio queues and translated directly to
operations in the selected host directory.

The guest mounts tag `nvx` at `--mount-target` (default `/mnt/host`). The same device and FUSE
protocol implementation is used by Linux/KVM and Windows/WHP.

## Access modes

| Options | Guest access | Host behavior |
| --- | --- | --- |
| `--mount <dir>` | Read-only | Host changes are visible to the running guest. |
| `--mount <dir> --mount-rw` | Read-write | Guest changes modify the host directory immediately. |

Read-only mode is enforced both by the guest mount and by the VMM. Mutating FUSE requests return
`EROFS`, so a guest cannot bypass the policy by changing its local mount flags.

## Examples

```console
./target/release/microvm \
  --kernel "$HOME/build/vmlinux" \
  --initrd "$HOME/build/initramfs.cpio.gz" \
  --mount ./shared \
  --mount-target /mnt/host
```

Add `--mount-rw` when guest writes should reach `./shared`:

```console
python scripts/nvx.py run --mount ./shared --mount-rw
```

On Windows, use PowerShell path syntax and `microvm.exe`. Neither backend needs an external
filesystem builder.

Inside the guest:

```console
/ # mount | grep /mnt/host
/ # ls -la /mnt/host
/ # printf 'written by guest\n' > /mnt/host/result.txt
```

## Live behavior

NVX returns zero entry and attribute cache lifetimes and opens regular files with FUSE direct I/O.
This favors coherent host/guest visibility over guest page-cache performance: a host rewrite is
observed by a subsequent guest read, and a completed guest write is already in the host file. An
application still needs its normal synchronization when host and guest modify the same file
concurrently.

Each open directory gets a stable name snapshot so FUSE cookies remain valid while the host tree is
changing. Paths are held relative to a pinned export-root handle, guest names containing parent
components or Windows path separators are rejected, and host symlinks/reparse points are never
followed while resolving guest operations. Linux mutations use descriptor-relative `*at` syscalls;
Windows holds non-delete-sharing parent handles through each namespace operation. Host access is
performed with the VMM process's permissions.

## Platform translation

On Linux, metadata and file operations map directly to their POSIX host equivalents. On Windows,
NVX translates Win32 behavior into the Linux FUSE ABI:

- directories, regular files, symlinks, timestamps, sizes, and read-only attributes become Linux
  inode attributes;
- guest uid and gid are reported as root because NTFS has no matching numeric ownership model;
  `chown` and exact POSIX `chmod` requests return `EOPNOTSUPP` instead of claiming a change that
  cannot be represented;
- rename uses `SetFileInformationByHandle(FileRenameInfoEx)` with POSIX replacement semantics,
  deletion uses `FileDispositionInfoEx`, capacity comes from `GetDiskFreeSpaceExW`, and Win32
  errors are converted to Linux errno values;
- unsupported special files, extended attributes, and remote locking return `EOPNOTSUPP` or
  `ENOSYS`; creating Windows symlinks still depends on host policy and developer-mode privileges.

## Snapshots

Snapshots store the virtio transport indices plus FUSE node IDs, relative path aliases, stable
directory cookies, and reopenable file/directory handles. Restore requires a fresh `--mount <dir>`;
the saved read-only/read-write policy is restored, and handles are reopened relative to that root
before any vCPU resumes. Capture and restore fail closed when aliases no longer identify one host
object, when an open object has no surviving name, or when a referenced path is missing. Host
namespace mutation should still be quiesced while taking a snapshot when a deterministic cut is
required.

## Device details

The device is virtio ID 26 on a version-2 virtio-mmio transport at GPA `0xD0001000`, IRQ 6. It
exposes the standard 36-byte tag and one request queue in addition to the high-priority queue. NVX
negotiates FUSE 7.31 without DAX; request and response payloads use ordinary guest RAM descriptors.

The VMM appends tokens equivalent to:

```text
virtio_mmio.device=0x1000@0xd0001000:6 \
virtfs_dir=<target> virtfs_tag=nvx virtfs_mode=<ro|rw>
```

Run `python scripts/nvx.py bench-virtfs` to measure sequential live I/O and perform a bidirectional
host/guest exchange in one running VM. Metric names are in [Benchmark Reference](benchmark.md).

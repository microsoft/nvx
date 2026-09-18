# Build

Drive supported workflows directly through `scripts/nvx.py`; no Make wrapper
is required.

The portable workflow downloads the pinned Linux archive, verifies its
SHA-256, applies every patch in `kernel/patches`, and builds Linux plus the
selected guest artifacts in Docker. Alpine remains the default. OpenVMM builds
on the host:

```bash
python3 scripts/nvx.py build-guest
python3 scripts/nvx.py build-openvmm
```

Build every guest artifact, including the Ubuntu EROFS distro layer, with:

```bash
python3 scripts/nvx.py build-guest --guest all
```

The OpenVMM restore step excludes the compatibility IGVM artifact, which NVX
does not build or package, so builds do not depend on unrelated upstream
workflow artifacts.

OpenVMM's microVM tests build their own minimal Xen PVH guest from source in
the OpenVMM checkout. They do not consume `build/vmlinux` or
`build/initramfs.cpio.gz`. NVX uses those two artifacts only for its Linux and
device correctness tests, benchmarks, and packaged runtime.

On a Linux host, build either initramfs directly:

```bash
python3 scripts/nvx.py build-initramfs --guest alpine
python3 scripts/nvx.py build-initramfs --guest ubuntu
```

The default build produces:

```text
build/vmlinux
build/vmlinux.config
build/vmlinux.provenance.json
build/initramfs.cpio.gz
build/initramfs.cpio.gz.packages.json
build/initramfs.provenance.json
build/openvmm.provenance.json
openvmm/target/release/openvmm[.exe]
```

The provenance sidecars bind the kernel to its pinned archive, patch set,
input configuration, generated configuration, and output hash; bind the
initramfs and package manifest to the pinned Alpine inputs and source files;
and bind OpenVMM to the exact clean gitlink revision and executable hash.
Packaging rejects missing, dirty, stale, or mismatched provenance.

Ubuntu adds:

```text
build/initramfs-ubuntu.cpio.gz
build/initramfs-ubuntu.cpio.gz.packages.json
build/ubuntu-distro.erofs
build/ubuntu-distro.erofs.manifest.json
```

Prepare the Ubuntu sandbox layer separately on Linux without replacing an
existing output:

```bash
python3 scripts/nvx.py build-distro-layer \
  --guest ubuntu \
  --output build/ubuntu-distro.erofs
```

Pass `--replace` only when intentionally rebuilding that path. The native
Ubuntu build requires `zstd`, and EROFS conversion additionally requires
`mkfs.erofs` from `erofs-utils`. The builder verifies Ubuntu Base and every
supplemental `.deb` before safe extraction and never executes binaries or
maintainer scripts from the Ubuntu root.

Check both Ubuntu outputs for deterministic rebuilds with:

```bash
python3 scripts/nvx.py verify-guest-determinism --guest ubuntu
```

Run the two test layers separately:

```bash
python3 scripts/nvx.py test-openvmm --backend kvm
python3 scripts/nvx.py test-microvm --backend kvm
python3 scripts/nvx.py test-microvm --backend kvm --guest ubuntu
```

The first command needs only the OpenVMM checkout. The second needs the
standard build outputs above and writes complete per-scenario logs under
`build/test-results/microvm` by default.

The Alpine initramfs includes the sandbox PID-1 bootstrap, its container namespace
helpers, the static `nvx-device-io` benchmark helper, and the static
`nvx-port-io` restore packet helper under `/sbin`. The matching kernel enables
virtio-blk, compressed EROFS, overlayfs, ext4 scratch, memory cgroups, and
cgroup BPF. The build fails if `olddefconfig` drops any required option. The
APK manifest records the `blkid` and `util-linux` tools used by the bootstrap
plus the device helper's source and binary SHA-256 values.

The platform configuration also enables Unix-domain sockets for local guest
IPC and seccomp filters for workload syscall policies. Overlayfs does not
unconditionally follow redirect metadata. These are kernel capabilities, not
product-agent configuration; the same requirements are checked after
`olddefconfig` and when verifying source and generated configurations.

The Ubuntu initramfs uses Ubuntu userland with the NVX kernel. It is not an
Ubuntu-kernel or systemd VM. Its distribution-neutral package manifest records
the Ubuntu Base and supplemental binary/source identities, license metadata,
rootfs SHA-256, and NVX helper provenance.

The native kernel build caches the verified and patched source under
`.cache/linux`, uses `O=build/linux`, runs `olddefconfig`, exports the exact
generated config as `build/vmlinux.config`, and fails if the Xen PVH note is
absent. Changing an archive hash or patch invalidates both source and object
caches; changing the input configuration invalidates the object cache.

Release packaging stages and verifies a complete output before replacing an
existing `dist/` version. Its `SOURCE-MANIFEST.json` records the package
version and exact hashes for OpenVMM, Linux, the generated kernel config, and
the unchanged Alpine initramfs. The OpenVMM section advertises microVM ABI 2,
control-session protocol 1, and contract
`nvx-microvm-v2-control-v1`; product guest-agent metadata is intentionally not
part of this platform manifest.

A binary release directory has this layout:

```text
bin/openvmm[.exe]
guest/vmlinux
guest/vmlinux.config
guest/initramfs.cpio.gz
guest/initramfs.cpio.gz.packages.json
provenance/openvmm.provenance.json
provenance/initramfs.provenance.json
provenance/vmlinux.provenance.json
licenses/LICENSE-OPENVMM
licenses/COPYING-LINUX
LICENSE
README.md
SOURCE-MANIFEST.json
THIRD_PARTY_NOTICES.md
SHA256SUMS
```

Packages built with `--include-source` additionally contain `source/`.
`SHA256SUMS` has sorted `SHA256  relative/path` entries using POSIX separators
for every packaged file, including `SOURCE-MANIFEST.json`, except
`SHA256SUMS` itself.

## Building the packaged Linux source

The Linux corresponding-source archive contains the patched
`linux-6.18.38/` tree and the exact `vmlinux.config` used for the distributed
kernel.

On a Linux host with the kernel build dependencies installed, run from the
archive root:

```bash
mkdir build
cp vmlinux.config build/.config
make -C linux-6.18.38 O="$PWD/build" olddefconfig
make -C linux-6.18.38 O="$PWD/build" -j"$(nproc)" vmlinux
```

The normal repository workflow performs the same build through
`scripts/nvx.py build-kernel` or the Docker artifact target.

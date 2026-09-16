# Build

Drive supported workflows directly through `scripts/nvx.py`; no Make wrapper
is required.

The portable workflow downloads the pinned Linux archive, verifies its
SHA-256, applies every patch in `kernel/patches`, and builds Linux plus the
Alpine initramfs in Docker. OpenVMM builds on the host:

```bash
python3 scripts/nvx.py build-guest
python3 scripts/nvx.py build-openvmm
```

The OpenVMM restore step excludes the compatibility IGVM artifact, which NVX
does not build or package, so builds do not depend on unrelated upstream
workflow artifacts.

OpenVMM's microVM tests build their own minimal Xen PVH guest from source in
the OpenVMM checkout. They do not consume `build/vmlinux` or
`build/initramfs.cpio.gz`. NVX uses those two artifacts only for its Linux and
device correctness tests, benchmarks, and packaged runtime.

On a Linux host, build the guest directly:

```bash
python3 scripts/nvx.py build-guest --native
```

The standard build produces:

```text
build/vmlinux
build/vmlinux.config
build/initramfs.cpio.gz
build/initramfs.cpio.gz.packages.json
openvmm/target/release/openvmm[.exe]
```

Run the two test layers separately:

```bash
python3 scripts/nvx.py test-openvmm --backend kvm
python3 scripts/nvx.py test-microvm --backend kvm
```

The first command needs only the OpenVMM checkout. The second needs the
standard build outputs above and writes complete per-scenario logs under
`build/test-results/microvm` by default.

An NVX guest-agent binary is a separately built, optional input. Stage it only
with its expected SHA-256:

```bash
python3 scripts/nvx.py stage-agent \
  --input /path/to/nvx-agent \
  --sha256 <64-hex-digit-sha256>
```

The command requires a static x86-64 ELF and writes `build/nvx-agent` plus its
pin. It does not install or select the agent in the initramfs; that cutover is a
separate integration step after the PID-1 runtime is complete.

The initramfs includes the sandbox PID-1 bootstrap, its container namespace
helpers, the static `nvx-device-io` benchmark helper, and the static
`nvx-port-io` restore packet helper under `/sbin`. The matching kernel enables
virtio-blk, compressed EROFS, overlayfs, ext4 scratch, memory cgroups, cgroup
BPF, seccomp filters, and safe overlay redirect handling. The build fails if
`olddefconfig` drops any required option. The APK manifest records the `blkid`
and `util-linux` tools used by the bootstrap plus the device helper's source
and binary SHA-256 values.

The native kernel build caches the verified and patched source under
`.cache/linux`, uses `O=build/linux`, runs `olddefconfig`, exports the exact
generated config as `build/vmlinux.config`, and fails if the Xen PVH note is
absent. Changing an archive hash or patch invalidates both source and object
caches.

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

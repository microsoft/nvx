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

These are the ABI-v1/legacy artifacts. They continue to use `alpine/init` and,
when selected by the legacy sandbox token, `nvx-init-agent`.

The broker artifact uses the reviewed ACI-03 source revision
`cc18066124b798b11d556e3b1983e84a39c75543`. That revision computes the
running `/proc/self/exe` SHA-256 and compares it with the authenticated
manifest, includes the required typed admission and gap semantics, and gives
control traffic priority over downstream console traffic. Build
`guestagent-nvx` from a clean checkout at that revision with locked
dependencies, an external target directory, and static musl:

```bash
cd ACI.Sandbox.GuestAgent.Rust
export CARGO_TARGET_DIR=/outside/source/nvx-agent-target
export NVX_VALIDATION_OUT=/outside/source/nvx-agent-validation
rustup target add x86_64-unknown-linux-musl
sh scripts/validate-nvx-agent.sh
sha256sum \
  "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/nvx-agent"
```

The ACI validator rejects `PT_INTERP`, forbidden CLH/runc/tonic dependencies,
and binaries larger than 16 MiB. The release input is the persistent staged
artifact whose SHA-256 is
`18ebe55ea7c53c186d4a145debd6ba6f185a68307c30ec1fe691124dfcb969e9`,
size is `1,852,256` bytes, and ELF GNU build ID is
`207979ffc6b07d08976fe604a47b299ab986135d`. The source revision records
provenance; it is not sufficient byte identity. The current linker build ID is
affected by the Cargo target path, so a build from the same source into a
different target directory can have another digest. Do not claim source-only
byte reproducibility. Stage only the reviewed external input:

```bash
python3 scripts/nvx.py stage-agent \
  --input build/nvx-agent-cc18066-input \
  --sha256 18ebe55ea7c53c186d4a145debd6ba6f185a68307c30ec1fe691124dfcb969e9
```

The command requires a static x86-64 ELF and writes `build/nvx-agent` plus its
pin. It rejects missing inputs, any digest or size other than the reviewed
external input, a dynamic or wrong-arch ELF, and the size limit. Build the
distinct broker image explicitly:

```bash
python3 scripts/nvx.py build-agent-initramfs       # Docker
# or on Linux:
python3 scripts/nvx.py build-agent-initramfs --native
```

Native initramfs construction automatically uses
`~/.cache/nvx/native-work/<profile>` rather than the repository filesystem.
Set `NVX_NATIVE_WORK_DIR` to select another native Linux work root. The command
rejects DrvFS/9P, NTFS, CIFS, and other filesystems that cannot preserve the
mode and symlink probe. The verified archive and package manifest are copied
atomically to `build/` only after construction and readback succeed. Docker
builds use the container's native Linux filesystem.

This produces `build/initramfs-agent.cpio.gz` and its package manifest without
changing `build/initramfs.cpio.gz`. `/init` is a symlink to the verified
`/sbin/nvx-agent`, so the kernel invokes the agent as PID 1 with no arguments.
The verifier models kernel extraction in archive order. It rejects duplicate,
case-colliding, or non-canonical paths; missing, symlink, or non-directory
ancestors; type replacement; untrusted or escaping symlinks; repeated inode
identities and regular-file hardlinks; unsafe entry types or modes; flattened
directory modes; wrong ownership; incorrect Alpine password/group/shadow
modes; and a wrong agent or PID-1 identity. The packer emits every file with
an independent inode identity and link count one. The broker image retains the
legacy helper files so its rootfs differs only by the intended `/init`
replacement and `/sbin/nvx-agent` addition.

The matching kernel assertions cover cgroup-v2 memory, pids, CPU weight,
freezer and BPF; BPF and seccomp syscalls/filters; EROFS, overlay, ext4 and GPT;
virtio block/console/MMIO; devtmpfs, PTYs, proc/sysfs/tmpfs; and mount, PID,
UTS, and IPC namespaces. CFS bandwidth remains disabled because CPU quota is
unused. Linux 6.18 supplies `clone3`, pidfds, `openat2`, and `close_range`
unconditionally; their runtime availability is exercised by ACI-04 rather
than represented by obsolete/nonexistent Kconfig switches.

The executable SHA-256 above is the authoritative byte identity used by the
manifest contract; source revision and ELF build ID remain separate fields.
The source revision does not reproduce or authenticate binary bytes by itself.

Both initramfs profiles retain the sandbox helpers, including the static
`nvx-device-io` benchmark helper and static `nvx-port-io` restore packet helper
under `/sbin`. The APK manifest records the device helper's source and binary
SHA-256 values.

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

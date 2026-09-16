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
build/vmlinux.provenance.json
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

These are the simple-profile artifacts. They continue to use `alpine/init` and,
when selected by the `nvx_sandbox` token, `nvx-init-agent`.

The broker initramfs uses the pinned agent identity and runtime contract in
`SOURCE-MANIFEST.json`. Obtain the supplied executable through a trusted
artifact channel, then authenticate the checkout and derive the expected
executable digest from the manifest:

```bash
AGENT_INPUT=/path/to/supplied/nvx-agent
AGENT_SHA256="$(
  python3 -c 'import json; print(json.load(open("SOURCE-MANIFEST.json", encoding="utf-8"))["guest_agent"]["external_input_sha256"])'
)"
python3 scripts/nvx.py stage-agent \
  --input "$AGENT_INPUT" \
  --sha256 "$AGENT_SHA256"
```

This command does not accept an arbitrary executable. The supplied file must
match the manifest's pinned SHA-256 and size, be a static compatible x86-64 ELF
without `PT_INTERP` or dynamic dependencies, and satisfy the unchanged source
revision, build ID, target, startup-mode, protocol-schema, and runtime-contract
pins used during packaging. The source revision records provenance but does
not establish byte identity. `stage-agent` copies the input with a bounded
size check before hashing and writes `build/nvx-agent` plus its digest pin.
Build the distinct broker image explicitly:

```bash
python3 scripts/nvx.py build-agent-initramfs       # Docker
# or on Linux:
python3 scripts/nvx.py build-agent-initramfs --native
```

The `guest_agent.runtime_contract` value
`startup-modes-session-operations` is descriptive, unversioned agent-capability
metadata. It is not a new ABI or protocol version. The separately versioned
microVM ABI remains 2, the OpenVMM control-session protocol remains 1, and the
guest-agent protocol schema remains 2.

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
The broker initramfs contains exactly four entries: the root directory,
`/init`, `/sbin`, and the verified `/sbin/nvx-agent`. The verifier models
kernel extraction in archive order and rejects additional entries, duplicate,
case-colliding, or non-canonical paths, unsafe types or modes, wrong
ownership, repeated inode identities, hardlinks, escaping symlinks, and a
wrong agent or PID-1 identity. The simple shell image remains an independent
Alpine profile with its existing utilities and validation.

The simple profile permits privileged helper modes only for the exact executable
SHA-256 pins and modes in `scripts/nvx_tools/build.py`, with normalized root
ownership. Updating these Alpine helpers requires an explicit pin update;
substituted binaries are rejected. The broker profile has no such exceptions.

The broker's matching kernel assertions cover cgroup-v2 memory, pids, CPU weight,
freezer and BPF; BPF and seccomp syscalls/filters; overlay, ext4 and GPT;
virtio block/console/MMIO;
devtmpfs, PTYs, proc/sysfs/tmpfs; and mount, PID, UTS, and IPC namespaces.
CFS bandwidth remains disabled because CPU quota is unused. Linux 6.18
supplies `clone3`, pidfds, `openat2`, and `close_range` unconditionally; their
runtime availability belongs to broker integration validation rather than
obsolete or nonexistent Kconfig switches.

The executable SHA-256 recorded in `SOURCE-MANIFEST.json` is the authoritative
byte identity used by the manifest contract; source revision and ELF build ID
remain separate fields. The source revision does not reproduce or authenticate
binary bytes by itself.

The simple initramfs retains the sandbox helpers, including the static
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

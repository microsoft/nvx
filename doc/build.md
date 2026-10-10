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

On a Linux host, `--native` builds the kernel and the selected Alpine or Ubuntu
initramfs without Docker, using the host packages that
[Setup](setup.md#prerequisites) lists. Azure Linux builds only in Docker, so a
native build rejects `--guest azurelinux` and `--guest all`. Because only
`--guest all` builds the Ubuntu EROFS distro layer, build it after a native
build with `build-distro-layer`, described below.

```bash
python3 scripts/nvx.py build-guest --native
python3 scripts/nvx.py build-guest --native --guest ubuntu
```

The native build keeps the Linux archive, the Ubuntu Base archive, and the
supplemental Ubuntu packages under `downloads/` in the cache directory,
`.cache/` or `$NVX_CACHE_DIR`. It reuses a file there whose SHA-256 matches its
pin and downloads it again otherwise, so staging the pinned archives in that
directory lets a host without access to their download servers build natively.
The Alpine minirootfs and packages still come from the Alpine servers.

The OpenVMM restore step excludes the compatibility IGVM artifact, which NVX
does not build or package, so builds do not depend on unrelated upstream
workflow artifacts.

OpenVMM builds do not require access to runtime hypervisor devices. By default,
Windows builds the native MSVC executable and Linux builds the native GNU
target. Both `build-openvmm` and `build` accept `--backend`: `kvm` selects GNU,
`mshv` selects the statically linked musl target on Linux, and `whp` selects
MSVC on Windows. For example, this builds musl without requiring `/dev/mshv`:

```bash
python3 scripts/nvx.py build-openvmm --backend mshv
```

The combined `build` command rejects unsupported OS/backend combinations before
producing guest artifacts. Guest-only and source-only commands do not select an
OpenVMM build target.

When invoked through NVX, OpenVMM's custom TTRPC lifecycle, SMP, and snapshot
test uses the ACPI-free, MP-enabled `build/vmlinux` and
`build/initramfs.cpio.gz` artifacts. The phase-1 lifecycle and TTRPC interface
tests continue to use OpenVMM's packaged guest artifacts.

On a Linux host, build the Alpine or Ubuntu initramfs directly:

```bash
python3 scripts/nvx.py build-initramfs --guest alpine
python3 scripts/nvx.py build-initramfs --guest ubuntu
```

Build the Azure Linux initramfs through Docker:

```bash
python3 scripts/nvx.py build-initramfs --guest azurelinux
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

Programmatic callers can select the backend and override the output destination
through [`OpenVmmBuildConfig`](../scripts/nvx_tools/build_config.py). Native and
musl builds normalize the newly built executable to that destination before
recording its provenance. Host OS detection and backend validation live in
[`build.py`](../scripts/nvx_tools/build.py), not in the configuration object.

Fixed build inputs and defaults live in
[`build_constants.py`](../scripts/nvx_tools/build_constants.py). Its namespace
classes group kernel, OpenVMM, Alpine, Ubuntu, initramfs, Docker, cache-tool,
and release settings. For example, Python callers use
`KernelBuildConstants.VERSION` and `AlpineBuildConstants.MINIROOTFS_SHA256`;
the previous module-level constants are not re-exported. Shared repository
and artifact directories belong to `BuildConstants`.

Keep per-invocation choices and overrides in the existing build configuration
objects. Environment-dependent cache locations and host-dependent executable
selection are still resolved by their helpers when a configuration is created,
not frozen into the constants module.

The provenance sidecars bind the kernel to its pinned archive, patch set,
input configuration, generated configuration, and output hash; bind the
initramfs and package manifest to the pinned Alpine inputs and source files;
and bind OpenVMM to the exact clean gitlink revision and executable hash.
The Alpine source-file inputs include the constants module. Packaging rejects
missing, dirty, stale, or mismatched provenance.

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
maintainer scripts from the Ubuntu root. The Docker builder pins its Debian
base image by digest and installs an exact `erofs-utils` version so clean EROFS
builds use the same encoder.

Check both Ubuntu outputs for deterministic rebuilds with:

```bash
python3 scripts/nvx.py verify-guest-determinism --guest ubuntu
```

Azure Linux builds only in Docker and adds:

```text
build/initramfs-azurelinux.cpio.gz
build/initramfs-azurelinux.cpio.gz.packages.json
```

Run the two test layers separately:

```bash
python3 scripts/nvx.py test-openvmm --backend kvm
python3 scripts/nvx.py test-microvm --backend kvm
python3 scripts/nvx.py test-microvm --backend kvm --guest ubuntu
```

Both commands need the standard guest build outputs above. The second writes
complete per-scenario logs under
`build/test-results/microvm` by default.

The Alpine initramfs includes the sandbox PID-1 bootstrap, its container namespace
helpers, the static `nvx-device-io` benchmark helper, the static `nvx-port-io`
port helper (single-byte port reads and writes, and the VM generation ID read),
the static `nvx-time-probe` guest time probe, and the
static `nvx-time` guest time component under `/sbin`. The probe measures TSC and
clock read latency, reports the guest's clock, clocksource, TSC flags, and
hypervisor identity, and runs the cross-vCPU warp test that the
[time ABI](design/time-abi.md#cross-vcpu-skew-bound) uses to enforce its 1 µs
skew bound. `nvx-time` implements the time ABI's
[guest obligations](design/time-abi.md#guest-obligations): the conformance
checks, the violation watcher, the restore packet v4 and time-sample parsers,
the snapshot agent's time steps, and the wall-clock discipline. Before
shell-ready it only steps the clock to host UTC; the other boot checks run in
the background, and the production console shows no time ABI line except a
violation event. `nvx-time status` prints every recorded check (boot, capture,
and restore) and the runtime state on demand, after waiting for pending
checks, for CI and test harnesses. Its boot check
also fails when the kernel's boot-time W+X audit reports a writable and
executable mapping. `nvx-time exhaustive` runs the CI-only exhaustive check
(checks X1 to X6 on every online CPU); it reports and exits, and production
boots never run it. The
`--report-only` flag (or the `nvx_time_abi=report-only` kernel command-line
token) reports violations without powering off, for runs against VMMs that
predate the ABI. Both time tools are linked statically against musl
(`musl-gcc`): with static glibc each would add about 700 KB to the initramfs,
and every unpacked kilobyte delays cold boot. The
[kernel configuration checks](#kernel-configuration-checks) cover the kernel
features that the sandbox bootstrap needs. The APK manifest records the
`blkid` and `util-linux` tools used by the bootstrap plus the device helper's
source and binary SHA-256 values.

The Ubuntu initramfs uses Ubuntu userland with the NVX kernel. It is not an
Ubuntu-kernel or systemd VM. Its distribution-neutral package manifest records
the Ubuntu Base and supplemental binary/source identities, license metadata,
rootfs SHA-256, and NVX helper provenance.

The kernel build, `build-kernel`, which the Docker build also runs, caches the
verified and patched source under `.cache/linux`, uses `O=build/linux`, runs
`olddefconfig`, stops unless the generated config passes the
[kernel configuration checks](#kernel-configuration-checks), and exports it as
`build/vmlinux.config`. The time-ABI settings keep `CONFIG_HYPERVISOR_GUEST`
and `CONFIG_PARAVIRT` on and `CONFIG_HYPERV`, `CONFIG_KVM_GUEST`, and
`CONFIG_CPU_FREQ` off. The guest identifies the
hypervisor only through the time ABI's Hyper-V identity, so the KVM guest code,
kvmclock, and the haltpoll idle driver would be dead code. Without cpufreq,
`intel_pstate` cannot probe MSRs the microVM does not implement, so no `#GP`
traces are printed with interrupts disabled during boot. `CONFIG_SCHED_MC_PRIO`
is off as well because it selects both cpufreq and `intel_pstate`. Production
kernels must not enable the soft-lockup or hung-task detectors. Changing an
archive hash or patch invalidates both source and object caches; changing the
input configuration invalidates the object cache.

The kernel keeps the code it generates at run time read-only. On x86, only
`CONFIG_STRICT_MODULE_RWX` makes such memory read-only and executable
(`CONFIG_ARCH_HAS_EXECMEM_ROX`), and it requires `CONFIG_MODULES`. Without it,
the thunk pages of the Indirect Target Selection (ITS) mitigation stay writable
and executable when the mitigation patches indirect branches at boot, which it
does unless Spectre v2 uses retpolines. The kernel therefore supports modules
but never builds or ships one:

- the build fails if any option is set to `m`;
- `CONFIG_MODPROBE_PATH` is empty, so the kernel never starts a module helper;
- `CONFIG_TRIM_UNUSED_KSYMS` drops every symbol export, about 200 KB that would
  otherwise push the kernel's data past the next 2 MiB boundary;
- init writes 1 to `/proc/sys/kernel/modules_disabled` before anything else
  runs.

When the mitigation patches indirect branches, the read-only executable cache
takes one 2 MiB block of guest memory. `CONFIG_DEBUG_WX` audits the kernel page
tables at boot, and the `nvx-time` boot check fails if the audit reports a
writable and executable mapping.

## Kernel configuration checks

The kernel build checks the configuration that `olddefconfig` generates
against fixed groups of settings, so a configuration or version change cannot
silently drop a feature that the machine profile or a guest needs. The groups
live in `KernelBuildConstants` in
[`build_constants.py`](../scripts/nvx_tools/build_constants.py).

| Group | Required settings | Purpose |
| --- | --- | --- |
| Direct boot | ACPI and PVH off; MP-table parsing, local APIC, IOAPIC, and virtio-mmio devices from the command line | Boot through the [ACPI-free MP-table loader](design/cold-boot.md#linux-direct-mp-table-loader) |
| Virtio console | Virtio console and HVC driver over virtio-mmio | Drive the [virtio consoles](design/machine-and-device-abi.md#console) |
| Sandbox | virtio-blk; ext4; EROFS with zstd compression; overlayfs that does not always follow redirects; memory and PID cgroups; BPF and cgroup BPF; seccomp filters; Unix-domain sockets; network namespaces and veth | Layer EROFS over ext4 scratch and confine the workload in the [sandbox bootstrap](design/sandbox-filesystem-and-agent-architecture.md#implemented-filesystem-bootstrap) |
| Network | IPv4; IPv6 without the SIT tunnel driver; virtio-net | Configure the dual-stack NIC of the [portable network profile](design/machine-and-device-abi.md#network); SIT would add an `sit0` device that decapsulates IPv6 from IPv4 packets |
| Shared interrupt status | `CONFIG_VIRTIO_MMIO_SHARED_STATUS` from the kernel patches | Read [shared virtio interrupt status](design/machine-and-device-abi.md#fixed-virtio-mmio-transport) |
| Time ABI | Hypervisor-guest and paravirtualization support on; Hyper-V, KVM guest, and cpufreq off | Meet the [time ABI](design/time-abi.md), as described above |
| Hardening | Strict kernel and module RWX with read-only executable memory, the ITS mitigation, an empty modprobe path, trimmed symbol exports, the boot-time W+X audit, and no option built as a module | Keep runtime code read-only, as described above |
| Watchdogs | Soft-lockup and hung-task detectors off, or on in the [CI debug kernel](#ci-debug-kernel) | Keep the detectors out of production kernels |

`build-kernel` runs every check, and packaging runs them again on the
packaged config. `scripts/nvx.py verify` checks the checked-in
`kernel/config-microvm` for the direct-boot, sandbox, network, time-ABI, and
hardening groups plus the xe9 console and virtio-fs. When
`build/vmlinux.config` exists, it also checks that file for the direct-boot,
sandbox, and network groups plus the xe9 console.

## CI debug kernel

CI proves that snapshot restores never trip the kernel watchdogs with a debug
variant of the same kernel:

```bash
python3 scripts/nvx.py build-guest --debug-kernel
python3 scripts/nvx.py build-kernel --debug   # native, Linux only
```

The variant applies the
[`kernel/config-microvm-debug`](../kernel/config-microvm-debug) fragment on top
of `kernel/config-microvm`. Each assignment in the fragment replaces the
matching base assignment before `olddefconfig`. It enables `DEBUG_KERNEL`, the
soft-lockup and hung-task detectors, and extra RCU stall diagnostics. It pins
the production RCU stall timeouts and turns off the debug options that
`DEBUG_KERNEL` would otherwise enable by default. The build fails if the
detectors are missing from the generated config. CI can shorten the soft-lockup
and hung-task thresholds for one boot with `watchdog_thresh=` and
`sysctl.kernel.hung_task_timeout_secs=`. The RCU stall timeout stays at the
production value because the time ABI's
[conformance check C8](design/time-abi.md#conformance-checks-and-the-nvx-time-abi-marker)
requires it.

The variant builds in `build/linux-debug`, so it never invalidates the
production object cache, and it produces:

```text
build/vmlinux-debug
build/vmlinux-debug.config
build/vmlinux-debug.provenance.json
```

Its provenance has the production fields plus a `debug_config_fragment` path
and SHA-256. The debug kernel is a CI artifact and is never packaged. The
`build-guest-artifacts` action caches and builds it only when its
`debug-kernel` input is `true`.

Release packaging needs the OpenVMM binary, the guest artifacts in the layout
below, and the OpenVMM, kernel, and Alpine initramfs provenance sidecars, so run
`build-openvmm` and `build-guest --guest all` first. A source-inclusive package
does not need the Azure Linux artifacts. Packaging stages and verifies a
complete output before replacing an existing `dist/` version, which requires
`--force`. Its `SOURCE-MANIFEST.json` records the package version and exact
hashes for OpenVMM, Linux, the generated kernel config, each packaged guest
initramfs and its package manifest, and the Ubuntu distro layer and its
manifest. The OpenVMM section advertises microVM ABI 2,
control-session protocol 1, and contract
`nvx-microvm-v2-control-v2`; product guest-agent metadata is intentionally not
part of this platform manifest.

A binary release directory has this layout:

```text
bin/openvmm[.exe]
guest/vmlinux
guest/vmlinux.config
guest/initramfs.cpio.gz
guest/initramfs.cpio.gz.packages.json
guest/initramfs-ubuntu.cpio.gz
guest/initramfs-ubuntu.cpio.gz.packages.json
guest/ubuntu-distro.erofs
guest/ubuntu-distro.erofs.manifest.json
guest/initramfs-azurelinux.cpio.gz
guest/initramfs-azurelinux.cpio.gz.packages.json
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

Packages built with `--include-source` additionally contain `source/` and omit
the Azure Linux guest artifacts, whose corresponding source is not collected.
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

# Ubuntu guest support

[Design index](../design.md)

> Status: **Implemented**
>
> Initial target: Ubuntu Base 26.04.1 LTS, amd64
>
> Last updated: September 18, 2026

## Summary

NVX should support Ubuntu userland without replacing the specialized NVX
kernel or changing the OpenVMM microVM machine profile. The first implementation
should provide two related but distinct Ubuntu experiences:

1. an Ubuntu initramfs selected by `nvx.py run`, for an interactive Ubuntu root
   shell and distro-level smoke testing; and
2. an Ubuntu EROFS `distro` layer selected by `nvx.py sandbox`, for ordinary
   non-root Ubuntu workloads under the existing Alpine control initramfs.

Both profiles use the same `build/vmlinux` artifact. That kernel contains the
Xen PVH entry point, xe9 early and interactive consoles, fixed virtio-mmio
discovery, shared interrupt-status support, and the microVM LAPIC-frequency
override. Ubuntu does not provide or replace the guest kernel.

A conventional Ubuntu virtual machine with systemd as PID 1 and a writable
root disk is not part of the initial feature. It needs a separate machine and
lifecycle design because it conflicts with current sandbox identity,
capability, networking, snapshot, and outer-agent assumptions.

## Decision

The implementation will use one common Ubuntu rootfs preparation pipeline and
produce two different artifacts from it:

| Artifact | Purpose | Boot supervisor |
| --- | --- | --- |
| `initramfs-ubuntu.cpio.gz` | Interactive `nvx.py run --guest ubuntu` | NVX `/init` inside the Ubuntu initramfs |
| `ubuntu-distro.erofs` | `distro` lower layer for `nvx.py sandbox` | Existing Alpine initramfs and `nvx-init-agent` |

The current Alpine initramfs remains:

- the default for `nvx.py run`;
- the only outer control initramfs for `nvx.py sandbox`;
- the canonical guest for the existing correctness and performance suites; and
- available under the current `build/initramfs.cpio.gz` filename.

The Ubuntu work must not modify the OpenVMM microVM ABI. In particular, it must
not add firmware, PCI, a generic block-device allocator, or distro-specific
devices.

## Motivation

The current repository uses Alpine for two independent reasons:

- it supplies the small root filesystem used by `nvx.py run`; and
- it supplies the trusted control environment that mounts sandbox layers,
  applies resource policy, and supervises the workload.

Ubuntu support does not require both roles to move together. Keeping the
sandbox control environment on Alpine avoids coupling the trusted agent to an
Ubuntu release and preserves the existing snapshot contract. Providing an
Ubuntu initramfs separately gives developers an Ubuntu shell when they need to
validate glibc, Debian packages, or Ubuntu-specific behavior.

Ubuntu Base is preferable to the Ubuntu cloud disk for both artifacts. Ubuntu
Base is a filesystem archive intended for construction of custom images. The
cloud image is a QCow2 UEFI/GPT disk with its own kernel, initramfs, bootloader,
cloud-init policy, and partition layout. Those assumptions do not match the
firmwareless Xen PVH boot used by the NVX microVM.

## Goals

- Boot Ubuntu userland with the existing NVX `vmlinux` on KVM, MSHV, and WHP.
- Preserve Alpine behavior, artifact names, tests, and performance history by
  default.
- Make the selected guest explicit in build and run commands.
- Use immutable, SHA-256-verified Ubuntu and package inputs.
- Produce deterministic initramfs and EROFS artifacts.
- Preserve the current fixed-device and snapshot ABI.
- Support the existing console, static network, HostFs mount, and clean-exit
  behavior in the Ubuntu initramfs.
- Support ordinary non-root Ubuntu binaries in the sandbox overlay.
- Record exact Ubuntu binary-package provenance.
- Provide corresponding source before distributing an Ubuntu guest artifact.

## Non-goals

- Booting the Ubuntu kernel.
- Booting GRUB, UEFI, or a stock Ubuntu cloud disk.
- Supporting architectures other than x86-64 in the first implementation.
- Running systemd as PID 1 in the outer microVM.
- Running a privileged or root sandbox workload.
- Running systemd as a sandbox entrypoint under the current drop-all-
  capabilities profile.
- Supporting package installation into the immutable EROFS layer at sandbox
  launch time.
- Changing the OpenVMM microVM block-device layout.
- Making Ubuntu the default guest.
- Adding Ubuntu to performance baselines before it has a stable, separate
  benchmark contract.

## Current constraints

### Kernel and machine profile

The NVX kernel already builds the drivers required by an Ubuntu initramfs or
ext4/EROFS root:

- Xen PVH direct boot;
- devtmpfs, procfs, sysfs, and tmpfs;
- virtio-mmio, virtio-blk, virtio-net, virtio-console, and virtio-fs;
- ext4, EROFS, and overlayfs;
- namespaces and cgroup v2 facilities used by the sandbox; and
- the xe9 and shared-status patches required by the microVM ABI.

The kernel intentionally omits functionality found in a general Ubuntu kernel,
including loadable modules, PCI, IPv6, user namespaces, fanotify, and
SquashFS. Ubuntu support therefore means Ubuntu userland under the NVX kernel
policy, not compatibility with every Ubuntu workload or host-integration
feature.

### Build pipeline

Before Ubuntu support, [`scripts/nvx_tools/build.py`](../../scripts/nvx_tools/build.py):

- has an Alpine-specific `AlpineBuildConfig`;
- downloads one Alpine minirootfs;
- installs packages through `apk`;
- writes an APK-specific package manifest; and
- always writes `build/initramfs.cpio.gz`.

[`scripts/nvx.py`](../../scripts/nvx.py) previously selected that initramfs for
`run`, `sandbox`, correctness tests, benchmarks, and release packaging.
[`docker/Dockerfile`](../../docker/Dockerfile), the guest-artifact action, and
[`scripts/nvx_tools/release.py`](../../scripts/nvx_tools/release.py) also assume
one initramfs and one package-manifest format.

### Sandbox runtime

The sandbox already accepts an arbitrary read-only EROFS `distro` layer plus
writable ext4 scratch. The outer Alpine agent resolves the layer by its fixed
MMIO role and enters the assembled overlay with `chroot`.

The outer helper deliberately injects its own loader, `setpriv`, and libraries
into `/.nvx-agent` before entering the workload. This allows glibc Ubuntu
binaries to run while the trusted outer agent remains musl-based. The current
[`guest/alpine/nvx-container-enter`](../../guest/alpine/nvx-container-enter) is therefore
compatible with an Ubuntu workload layer but is not compatible with replacing
the outer Alpine initramfs itself.

The current sandbox also requires:

- a non-root numeric UID and GID;
- exactly one matching passwd entry and one matching primary group;
- an absolute home directory that exists in the assembled root;
- no supplementary groups;
- `no_new_privs`; and
- all capability sets cleared.

Ubuntu's `nobody` entry uses UID/GID 65534 and home `/nonexistent`, but Ubuntu
Base does not create that directory. The converter must create it or create a
different explicitly selected workload identity.

## Upstream inputs

The first version will pin:

| Field | Value |
| --- | --- |
| Distribution | Ubuntu Base |
| Release | 26.04.1 LTS |
| Architecture | amd64 |
| Archive | `ubuntu-base-26.04.1-base-amd64.tar.gz` |
| SHA-256 | `a496a960472ce474a59590b8987d3a1135d3cbef1991f3b1abe8cacfea8bf85a` |
| Source directory | `https://cdimage.ubuntu.com/ubuntu-base/releases/26.04/release/` |

The version, URL, and digest must live in `SOURCE-MANIFEST.json` and matching
constants in the build implementation. `scripts/nvx.py verify` must reject a
mismatch between those two sources of configuration.

The build must verify the archive digest before extraction. Signature
verification of Ubuntu's `SHA256SUMS` is desirable defense in depth, but it
does not replace the repository-owned digest pin.

Release updates are explicit source changes. The builder must not resolve a
moving `latest`, codename-only directory, OCI tag, or package repository state.

## Artifact layout

Existing Alpine paths remain stable:

```text
build/vmlinux
build/vmlinux.config
build/initramfs.cpio.gz
build/initramfs.cpio.gz.packages.json
```

Ubuntu adds:

```text
build/initramfs-ubuntu.cpio.gz
build/initramfs-ubuntu.cpio.gz.packages.json
build/ubuntu-distro.erofs
build/ubuntu-distro.erofs.manifest.json
```

The EROFS UUID is recorded in its manifest. It must be deterministic for an
identical normalized filesystem tree and build configuration. A suitable
initial rule is to derive the UUID from the first 128 bits of a domain-
separated SHA-256 digest over:

- Ubuntu release and architecture;
- verified input archive digest;
- verified supplemental package digests;
- guest customization file digests; and
- the converter format version.

The manifest records the full SHA-256 input digest in addition to the UUID, so
the UUID is never treated as a content-integrity proof.

## Build architecture

### Guest descriptors

Replace implicit Alpine selection with a small typed guest descriptor. The
descriptor should provide:

- stable CLI name;
- distribution and release name;
- architecture;
- initramfs artifact names;
- package-manifest format;
- rootfs preparation function;
- minimum default memory;
- boot marker; and
- whether the guest is eligible for the sandbox outer-control role.

Initial descriptors:

| Guest | Initramfs | Default memory | Sandbox control |
| --- | --- | ---: | --- |
| `alpine` | `initramfs.cpio.gz` | 128 MiB | yes |
| `ubuntu` | `initramfs-ubuntu.cpio.gz` | 256 MiB initially | no |

The Ubuntu memory default is an engineering starting point, not a permanent
ABI. Ubuntu Base contains approximately 86 MiB of uncompressed regular-file
payload before supplemental packages and NVX helpers. CI must measure the
actual boot high-water mark before documenting a supported minimum.

### Common build operations

The following work should be extracted from the Alpine-specific builder:

1. verified download and cache handling;
2. safe archive extraction;
3. guest script and static-helper installation;
4. rootfs metadata normalization;
5. initramfs packing;
6. common helper provenance;
7. output naming; and
8. deterministic-build validation.

Distribution-specific code remains responsible for:

- preparing the initial root tree;
- adding the required package closure;
- validating distro-specific filesystem assumptions; and
- writing package provenance.

### Safe Ubuntu extraction

The Ubuntu extractor must not call `tar` directly on an unchecked archive and
then mutate paths through archive-controlled symlinks. Before extraction it
must reject:

- absolute member paths;
- `..` path traversal;
- hard links whose target escapes the root;
- symlinks used to redirect later writes outside the root;
- sockets;
- unexpected device nodes; and
- duplicate paths with incompatible types.

After extraction, every repository-owned modification must resolve beneath the
prepared root. The builder must never execute a binary from the extracted
Ubuntu root on the host.

The EROFS converter additionally applies the metadata policy from
[sandbox filesystem and agent architecture](../design/sandbox-filesystem-and-agent-architecture.md#image-preparation-and-distribution-proposed):

- reject or normalize setuid and setgid bits;
- reject device nodes unless explicitly synthesized by policy;
- deny arbitrary `trusted.overlay.*` and `user.overlay.*` attributes;
- allow file capabilities, ACLs, and security labels only through an explicit
  allowlist; and
- represent OCI-style deletions only through converter-owned whiteouts and
  opaque-directory markers.

The initial Ubuntu Base conversion does not need whiteouts because it consumes
one flattened rootfs archive. The converter format should still reserve a
versioned policy for later OCI-layer input.

### Supplemental package closure

Ubuntu Base already includes the core tools used by most NVX scripts:

- `coreutils`, `grep`, `sed`, and `mawk`;
- `mount` and `util-linux`;
- `setpriv`, `unshare`, `nsenter`, `chroot`, `mountpoint`, `blkid`, and
  `blockdev`; and
- `e2fsprogs`.

It does not include:

- `busybox` or `busybox-static`;
- `net-tools`;
- `iputils-ping`;
- `netcat-openbsd`;
- `systemd`;
- `udev`; or
- `iproute2`.

The Ubuntu initramfs needs the first four packages to preserve current `/init`
behavior without a separate networking rewrite. The implementation must not
run an unpinned `apt-get update && apt-get install` during a release build.

Instead, add `ubuntu/packages.lock.json` containing the complete binary package
closure. Each entry contains:

- binary package name;
- source package name and version;
- binary version and architecture;
- immutable download URL;
- `.deb` SHA-256;
- installed license metadata location; and
- dependency relationship used to include it.

The builder downloads and verifies every `.deb`, extracts payload and control
metadata without executing maintainer scripts, and performs the small,
explicit post-install actions needed by NVX. For example, it may create the
selected `nc` alternative symlink itself. Any package that cannot be installed
correctly without a maintainer script must either be replaced by an NVX static
helper or moved to a controlled, isolated package-build stage with separately
defined reproducibility and trust requirements.

This lock file is part of the rootfs fingerprint and CI cache key.

### Rootfs customization

Both Ubuntu outputs start from the same prepared and verified tree. The builder
then:

1. creates `/root`, `/tmp`, `/run`, and `/nonexistent` with fixed ownership and
   modes;
2. replaces `/etc/resolv.conf` with an empty regular file that the NVX network
   bootstrap may populate;
3. validates a unique `root` account and a unique UID/GID 65534 account;
4. installs the common NVX `/init` and `/sbin` helpers;
5. installs the static benchmark and lifecycle helpers required by the
   selected test profile;
6. removes package caches, temporary files, logs, host keys, random seeds, and
   machine-specific identifiers;
7. validates all absolute symlinks required by the usr-merged filesystem;
8. normalizes uid, gid, mode, and timestamps according to the artifact policy;
   and
9. writes package and helper provenance before packing the artifact.

The builder must not install an Ubuntu kernel, initramfs-tools, GRUB, cloud-init,
or firmware packages.

### Common versus distro-specific guest files

Most files formerly under `alpine/` are NVX guest files rather than Alpine
policy. The implementation keeps genuinely common sources in a neutral
`guest/` directory:

```text
guest/common/init
guest/common/nvx-exit
guest/common/nvx-hostmount
guest/common/nvx-init-agent
guest/common/nvx-managed-agent.c
guest/common/nvx-*.c
guest/alpine/
guest/ubuntu/
```

The move should preserve history where practical and update build, source,
quality, and CI path filters in the same change.

`nvx-container-enter` remains Alpine-control-specific because it copies the
musl loader and Alpine-linked libraries. It stays under `guest/alpine/` until a
static, libc-independent privilege-drop helper replaces that mechanism.

The Ubuntu initramfs is not eligible for `nvx.py sandbox`, so it does not need
an Ubuntu equivalent in the initial implementation.

## Initramfs boot profile

### CLI

Add guest selection to build and run commands:

```text
python scripts/nvx.py build-initramfs --guest {alpine,ubuntu}
python scripts/nvx.py build-guest --guest {alpine,ubuntu,all}
python scripts/nvx.py run --guest {alpine,ubuntu}
```

Defaults:

- `build-initramfs`: `alpine`;
- `build-guest`: `alpine`;
- `run`: `alpine`; and
- `build`: `alpine` unless release or CI explicitly requests `all`.

`--guest all` is valid only for build commands. Restore does not accept a new
guest selection because the captured machine contract and guest RAM determine
the restored guest.

When memory is not explicitly supplied, `run` resolves the guest-specific
default. An explicit memory value remains allowed, but CI establishes and
documents the minimum supported Ubuntu value.

### Boot flow

The Ubuntu initramfs boot remains identical to Alpine at the machine level:

1. OpenVMM loads `build/vmlinux` through the Xen PVH loader.
2. OpenVMM loads the selected Ubuntu initramfs as the optional PVH module.
3. OpenVMM prepends its xe9/hvc console parameters and fixed virtio-mmio
   discovery.
4. `/init` mounts procfs, sysfs, devtmpfs, and tmpfs.
5. `/init` configures loopback, optional static networking, and optional
   HostFs.
6. `/init` handles an explicit `nvx_exec` workload or opens a root shell.
7. `/sbin/nvx-exit` reports the final status to OpenVMM.

The selected guest startup emits a new stable marker after its interactive
shell is ready:

```text
NVX-GUEST-BOOT-OK: ubuntu
```

Alpine continues to emit `ALPINE-MICROVM-BOOT-OK` for compatibility and may
also emit the generic marker.

### Feature parity

The first supported Ubuntu initramfs must provide:

- interactive hvc0 console input and output;
- `nvx_exec` execution and status propagation;
- one-shot and managed lifecycle support where those paths do not require the
  Alpine sandbox container helper;
- static IPv4 configuration through the portable network profile;
- DNS through the derived portable gateway;
- HostFs mounting;
- clean `/sbin/nvx-exit`;
- SMP boot with 1, 2, 4, and 8 vCPUs; and
- snapshot and restore for blockless test scenarios.

Alpine-only benchmark scenarios that depend on extra shell tools may remain
Alpine-only initially. The test selector must reject unsupported Ubuntu
scenarios explicitly rather than silently substituting Alpine or skipping
assertions.

## Ubuntu sandbox layer profile

### Converter command

Add a Linux-only image preparation command:

```text
python3 scripts/nvx.py build-distro-layer \
  --guest ubuntu \
  --output build/ubuntu-distro.erofs
```

The command:

- prepares the common Ubuntu rootfs;
- applies the EROFS metadata policy;
- invokes `mkfs.erofs`;
- writes the deterministic UUID and SHA-256 metadata;
- does not create scratch; and
- never modifies an existing output unless an explicit replacement option is
  provided.

Scratch remains independently prepared with `mkfs.ext4`, consistent with the
existing sandbox contract.

### Launch

The resulting artifact is used through the existing role-bearing interface:

```bash
python3 scripts/nvx.py sandbox \
  --layer distro,/var/lib/nvx/ubuntu-distro.erofs,UUID \
  --scratch /var/lib/nvx/scratch.ext4 \
  --entrypoint /bin/sh \
  --workload-user 65534:65534 \
  --memory-mib 256
```

The converter creates `/nonexistent`, so the existing default UID/GID has a
valid home. Production images should prefer a dedicated non-root application
account and pass its numeric identity explicitly.

The workload shares the outer guest's network namespace but receives private
mount, PID, and UTS namespaces. It does not receive root privileges or
capabilities. Package installation and service management must happen while
building the immutable image, not from the running sandbox.

### Systemd workloads

Systemd is not a supported entrypoint under the initial Ubuntu layer profile.
As described in
[remaining production work](../design/remaining-production-work.md#workload-compatibility-and-volumes-proposed),
systemd requires a separate compatibility profile with:

- delegated cgroup hierarchy;
- an explicit capability policy;
- private device policy;
- mount propagation rules;
- read-only and masked system paths;
- stop-signal and shutdown handling; and
- additional agent supervision semantics.

That profile must be designed and tested independently. Detecting `/sbin/init`
or a systemd package in an image must not silently relax the current sandbox
security policy.

## Package provenance and corresponding source

### Binary package manifest

`initramfs-ubuntu.cpio.gz.packages.json` and the EROFS manifest use a
distribution-neutral envelope:

```json
{
  "format": 1,
  "guest": "ubuntu",
  "release": "26.04.1",
  "architecture": "amd64",
  "rootfs_sha256": "...",
  "packages": [],
  "helpers": {}
}
```

Each package record contains at least:

- binary name;
- binary version;
- architecture;
- source package name and version;
- installed status;
- `.deb` SHA-256 when added by NVX;
- declared license metadata path; and
- whether it came from Ubuntu Base or the supplemental lock.

The builder parses `/var/lib/dpkg/status` and supplemental `.deb` control data.
It must fail on duplicate package/version records, missing source-package
mapping required for source collection, or unsupported architecture.

### Source collection

Ubuntu artifacts must not be added to binary releases until
`collect-sources` can materialize their corresponding source.

Add an Ubuntu source collector that:

1. reads every Ubuntu package manifest in the release;
2. deduplicates exact source package name/version pairs;
3. downloads the matching `.dsc` and referenced source members;
4. authenticates each source index through signed `InRelease` metadata and a
   pinned Ubuntu archive keyring, then verifies every checksum recorded in the
   `.dsc`;
5. records the Ubuntu archive location and source metadata, retaining signed
   historical snapshot metadata and raw Launchpad publishing-history responses
   used for superseded versions;
6. includes repository-owned guest scripts and lock files in the project
   source archive; and
7. writes a deterministic Ubuntu source bundle and `SHA256SUMS`.

The release staging command validates that:

- every Ubuntu binary package maps to a collected source package or an
  explicit non-copyleft binary-only exception reviewed in
  `THIRD_PARTY_NOTICES.md`;
- the rootfs and supplemental package locks match the built manifests; and
- the Ubuntu source bundle is present when `--include-source` is selected.

Binary-only packaging must update its warning to mention Linux, Alpine, and
Ubuntu corresponding source.

## Docker and native builds

The default Docker build adds distinct targets:

```text
ubuntu-initramfs
ubuntu-initramfs-artifacts
ubuntu-distro-layer
all-guest-artifacts
```

The base build image includes the tools needed to extract `.deb` payloads and
run `mkfs.erofs`. It does not execute the extracted Ubuntu rootfs.

Native Ubuntu builds are Linux-only and require the same explicit tools used by
the Docker path. Tool checks must produce actionable errors. Windows users use
the Docker build and may run the resulting initramfs through WHP.

The Docker and native paths must consume the same rootfs and package lock,
normalization code, and packer. They must not maintain separate shell recipes
that can drift.

## CI design

### Caching

Use separate cache keys for:

- the NVX kernel;
- the Alpine initramfs;
- the Ubuntu prepared rootfs;
- the Ubuntu initramfs; and
- the Ubuntu EROFS layer.

The Ubuntu key includes:

- release and architecture;
- Ubuntu Base archive SHA-256;
- supplemental package-lock digest;
- guest common files;
- Ubuntu customization files;
- build implementation;
- Dockerfile; and
- converter format version.

Kernel changes invalidate runtime testing but do not need to invalidate the
prepared Ubuntu rootfs cache.

### Build jobs

The shared artifact job continues to build Alpine unconditionally when
artifact-backed tests are required. It builds Ubuntu when:

- Ubuntu guest inputs changed;
- common guest inputs changed;
- the kernel changed;
- release packaging is running; or
- a manually requested full guest validation is running.

Artifact upload keeps Alpine filenames unchanged and uploads Ubuntu files
under their distinct names.

### Correctness coverage

Keep the existing Alpine correctness suite unchanged. Add bounded Ubuntu
scenarios:

| Scenario | Required assertion |
| --- | --- |
| Initramfs boot | Generic Ubuntu boot marker and clean exit |
| Identity | `/etc/os-release` identifies the pinned Ubuntu release |
| Console | hvc0 accepts input and preserves output |
| SMP | 1, 2, 4, and 8 online CPUs as selected |
| Network | Static address, gateway, DNS, and outbound TCP |
| HostFs | Read-only and read-write mount behavior |
| `nvx_exec` | Exit status and output propagation |
| Blockless snapshot | Capture and restore preserve Ubuntu execution |
| Sandbox layer | Ubuntu binary runs as requested non-root UID/GID |
| Overlay | Lower file is readable and scratch-backed write is visible |
| Sandbox restore | Ubuntu EROFS identity and writable scratch remain valid |

Run the initramfs boot, network, HostFs, and sandbox layer smoke scenarios on
KVM, MSHV, and WHP. Run broader guest-only tests once on Linux/KVM unless a
backend-specific contract requires the full matrix.

Ubuntu is not added to performance regression gates in the initial change.
Record artifact sizes and boot timing as diagnostics only.

### Determinism coverage

CI must build the Ubuntu initramfs and EROFS layer twice from empty output
directories and compare SHA-256 values. A mismatch fails with a file-metadata
diagnostic rather than publishing either artifact.

## Release and download behavior

The first releasable implementation includes Ubuntu artifacts in the existing
platform release archives for simplicity:

```text
guest/vmlinux
guest/vmlinux.config
guest/initramfs.cpio.gz
guest/initramfs.cpio.gz.packages.json
guest/initramfs-ubuntu.cpio.gz
guest/initramfs-ubuntu.cpio.gz.packages.json
guest/ubuntu-distro.erofs
guest/ubuntu-distro.erofs.manifest.json
```

`download` installs all packaged guest artifacts atomically after checksum
verification. `run --guest ubuntu` fails clearly if the selected Ubuntu
artifact is absent; it never falls back to Alpine.

If release-size measurements show that repeating Ubuntu artifacts in every
platform archive is material, a later change may publish a versioned,
platform-independent guest bundle. That optimization must preserve atomic
version matching between OpenVMM, the NVX kernel, guest scripts, and machine
ABI.

## Documentation changes

Implementation must update:

- [`doc/build.md`](../build.md) with guest selection and artifact outputs;
- [`doc/run.md`](../run.md) with Ubuntu initramfs and layer examples;
- [`doc/usage.md`](../usage.md) with new CLI options;
- [`doc/distribution.md`](../distribution.md) with Ubuntu package and source
  delivery;
- [`doc/ci.md`](../ci.md) with Ubuntu smoke coverage;
- [`doc/project-structure.md`](../project-structure.md) if guest sources move
  out of `alpine/`;
- `SOURCE-MANIFEST.json`;
- `THIRD_PARTY_NOTICES.md`; and
- any design page that describes Alpine as the only supported userland.

Documentation must use "Ubuntu userland with the NVX kernel" and must not imply
that NVX boots or redistributes the Ubuntu kernel.

## Implementation sequence

### Phase 1: Generic guest selection

1. Introduce guest descriptors and artifact-path selection.
2. Add `--guest` to build and run commands with Alpine defaults.
3. Preserve existing Alpine artifact names and behavior.
4. Refactor common rootfs installation and packing without changing Alpine
   output.
5. Add unit tests proving default CLI and generated OpenVMM commands are
   unchanged.

If generic multi-distribution work lands through #110 first, reuse its
descriptor, CLI, cache, and manifest abstractions instead of creating an
Ubuntu-specific parallel hierarchy.

### Phase 2: Ubuntu initramfs

1. Add the Ubuntu source manifest and supplemental package lock.
2. Implement safe extraction and Ubuntu rootfs validation.
3. Install common NVX scripts and helpers.
4. Generate Ubuntu package provenance.
5. Pack the deterministic initramfs.
6. Add Ubuntu build and run unit tests.
7. Add KVM, MSHV, and WHP boot smoke coverage.

### Phase 3: Ubuntu sandbox layer

1. Implement the Linux-only EROFS converter and metadata policy.
2. Generate deterministic UUID and artifact manifests.
3. Add Ubuntu non-root identity and overlay tests.
4. Add snapshot and restore coverage using `distro` plus scratch.
5. Document image preparation and launch.

### Phase 4: Distribution

1. Implement Ubuntu corresponding-source collection.
2. Extend package staging, checksums, and download installation.
3. Add release validation for Ubuntu manifests and source bundles.
4. Publish Ubuntu artifacts only after all source and license gates pass.

### Deferred: Full Ubuntu systemd guest

A future full-OS profile may boot the NVX kernel directly against a writable
ext4 root disk with `root=/dev/vda`. That work requires a separate design for:

- a neutral root-disk role instead of reusing the sandbox `scratch` name;
- systemd and udev package construction;
- an NVX network generator that consumes `virtnet_*` configuration;
- hvc0 getty and login policy;
- readiness and clean shutdown;
- root-disk identity and update policy;
- mutable-root snapshot semantics;
- kernel feature compatibility; and
- security expectations for a privileged outer guest.

It must not be introduced as an undocumented special case of
`--microvm-sandbox-block scratch`.

## Acceptance criteria

Ubuntu support is complete when:

- Alpine remains the default and its existing artifact hashes change only when
  an intentional common-file refactor requires it;
- `build-initramfs --guest ubuntu` produces deterministic verified artifacts;
- `run --guest ubuntu` boots the pinned Ubuntu userland with `build/vmlinux`
  on KVM, MSHV, and WHP;
- Ubuntu networking, HostFs, console input, `nvx_exec`, and clean exit pass;
- the Ubuntu EROFS layer runs an ordinary non-root workload through
  `nvx.py sandbox`;
- overlay writes use ext4 scratch and do not mutate the EROFS image;
- Ubuntu layer snapshot and restore pass on supported backends;
- unsupported systemd and privileged workloads fail with explicit messages;
- release packaging never silently omits Ubuntu corresponding source;
- `scripts/nvx.py verify` validates every Ubuntu version and digest pin; and
- documentation clearly distinguishes Ubuntu initramfs, Ubuntu sandbox
  workload, and deferred full-systemd guest support.

## Risks and mitigations

| Risk | Mitigation |
| --- | --- |
| Ubuntu substantially increases initramfs memory and boot time | Use guest-specific memory defaults, measure high-water marks, and keep performance baselines Alpine-only |
| Mutable package repositories break reproducibility | Use a committed binary-package lock with exact URLs and SHA-256 digests |
| Archive paths or metadata escape the build root | Use a safe extractor and deny-by-default metadata policy |
| Guest-selection refactor changes Alpine behavior | Preserve filenames and defaults; compare generated commands and run the full Alpine suite |
| Ubuntu outer initramfs reaches Alpine-specific sandbox code | Keep `sandbox` pinned to the Alpine control initramfs and reject unsupported selection |
| Ubuntu Base account layout fails sandbox identity validation | Create `/nonexistent` and test UID/GID 65534 explicitly |
| Users infer stock Ubuntu compatibility | Document the NVX kernel feature policy and reject unsupported profiles clearly |
| Release bundles grow across all platforms | Measure first; move guest artifacts to a versioned shared bundle only if necessary |
| Parallel Azure Linux work creates duplicate abstractions | Reuse the first merged generic guest descriptor, manifest, and CI model |

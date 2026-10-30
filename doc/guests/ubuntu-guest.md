# Ubuntu guest support

NVX runs Ubuntu userland on its own Linux kernel. It builds two artifacts from
one pinned Ubuntu Base root filesystem: an initramfs that `nvx-rs run` boots
for an interactive Ubuntu shell, and an EROFS `distro` layer that
`nvx-rs sandbox` runs as a non-root workload. Neither artifact boots an Ubuntu
kernel, firmware, or systemd, and neither changes the
[machine and device ABI](../design/2-virtual-machine-architecture.md).

[Build](../build.md), [Run](../run.md), and the
[command-line reference](../usage.md) cover the commands. This chapter
describes how the pieces fit together and why.

## Roles

| Artifact | Used by | Boot supervisor |
| --- | --- | --- |
| `initramfs-ubuntu.cpio.gz` | `nvx-rs run --guest ubuntu` | The common NVX `/init` inside the Ubuntu initramfs |
| `ubuntu-distro.erofs` | `nvx-rs sandbox --layer distro,...` | The Alpine control initramfs and its init agent |

Alpine remains the default guest of `run` and the only control environment of
`sandbox`. The sandbox launcher always boots the Alpine initramfs, and the
microVM test harness rejects the sandbox-control scenarios for Ubuntu. Keeping
the trusted agent on Alpine decouples it from Ubuntu releases and leaves the
sandbox snapshot contract unchanged.

NVX starts from Ubuntu Base, a plain root filesystem archive, rather than an
Ubuntu cloud image. A cloud image is a UEFI/GPT disk with its own kernel,
bootloader, and cloud-init policy, none of which the firmwareless Linux direct
boot uses.

## Kernel policy

Both artifacts boot the same `build/vmlinux` as Alpine. That kernel provides
what Ubuntu userland needs on this machine:

- Linux direct boot with Intel MP tables and no ACPI;
- devtmpfs, procfs, sysfs, and tmpfs;
- virtio-mmio block, network, console, and filesystem devices;
- ext4, EROFS, and overlayfs; and
- the cgroups and the PID, mount, UTS, IPC, and network namespaces that the
  sandbox uses.

Its [patches](../../kernel/patches) add the xe9 early and interactive consoles
and the shared virtio-mmio interrupt status. The
[kernel configuration](../../kernel/config-microvm) omits PCI, user
namespaces, fanotify, and SquashFS. It enables module support only so that
strict module memory protection keeps runtime code read-only; NVX ships no
modules, and `/init` disables module loading before it starts any other
process. Ubuntu support therefore means Ubuntu userland under the NVX kernel
policy, not compatibility with every Ubuntu workload.

## Root filesystem builder

The Ubuntu root filesystem builder prepares one tree for both artifacts. It
runs natively on Linux or inside the Docker build, which also serves Windows
hosts, and both paths run the same builder. Its inputs are pinned:

- **Ubuntu Base** 26.04.1 for amd64, by URL and SHA-256.
  `SOURCE-MANIFEST.json` repeats the pin, and `scripts/nvx.py verify` fails
  when the two disagree.
- **Supplemental packages** from the lock in
  [`ubuntu/packages.lock.json`](../../ubuntu/packages.lock.json), which adds
  `busybox-static`, `iputils-ping`, `net-tools`, and `netcat-openbsd` with
  their library dependencies so the common `/init` works unchanged. Each entry
  records the package's exact URL, SHA-256, version, source package, and
  dependencies. The builder never resolves packages from a live repository.

The builder then:

1. verifies the base archive digest and extracts it with its own extractor,
   which rejects absolute or parent-relative member paths, links that escape
   the root, writes through a linked directory, device nodes, FIFOs, sockets,
   and duplicate paths of different types;
2. checks that `/etc/os-release` names the pinned release;
3. verifies each supplemental `.deb` against the lock, extracts it with the
   same extractor, and records it in the package database. It never runs
   maintainer scripts, and it fails when a package carries maintainer actions
   outside a reviewed per-package list;
4. creates `/root`, `/tmp`, `/run`, and `/nonexistent` with fixed modes,
   leaves an empty `/etc/resolv.conf` for the network bootstrap, and adds the
   `nc`, `netcat`, `wget`, and `mdev` links;
5. removes package caches, logs, temporary files, machine IDs, random seeds,
   and SSH host keys;
6. requires one `root` account, a `nobody` account with UID 65534 and home
   `/nonexistent`, a `nogroup` group, and usr-merged `/bin`, `/sbin`, `/lib`,
   and `/lib64` links; and
7. installs the NVX scripts from `guest/common` and `guest/ubuntu`, and the
   nine Rust guest binaries from `nvx-rs`.

The builder never executes a binary from the Ubuntu tree on the host. Both
artifacts are packed with root ownership, fixed timestamps, and a stable file
order, so a rebuild from the same inputs is byte-identical.

## Ubuntu initramfs

The Ubuntu initramfs boots like the Alpine one at the machine level: OpenVMM
loads the kernel and initramfs through the
[Linux direct MP-table loader](../design/5.1-boot-protocol.md#511-linux-direct-loader),
and the common [`/init`](../../guest/common/init) runs as PID 1. It disables
module loading, runs the [time ABI](../design/3.2-clock-and-time.md) boot step, and then
either runs an `nvx_exec` workload or mounts the HostFs share, configures
static networking, and starts the managed agent or an interactive shell. For
Ubuntu that shell is Bash, whose startup file prints
`NVX-GUEST-BOOT-OK: ubuntu` once it is ready for input.

The guest defaults to 512 MiB because the kernel unpacks the initramfs into a
tmpfs capped at half of RAM; [Run](../run.md#run-openvmm-directly) gives the
measured minimum. A restore takes the guest from the captured RAM and machine
contract, so it needs no guest selection.

The initramfs supports the console, `nvx_exec`, one-shot and managed
lifecycles, static IPv4 and IPv6 networking, HostFs, SMP, and blockless
snapshots, including the guest obligations of the time ABI. The test harness
rejects the `console-snapshot` scenario for Ubuntu instead of substituting
Alpine.

It installs the same nine Rust guest binaries as Alpine and Azure Linux under
their `/sbin/nvx-*` names. Docker copies all nine from one digest-pinned Rust
builder, while native and determinism builds use the shared fresh-target
routine. The package manifest records one aggregate `nvx-rs` source digest
and each installed binary's digest.

## Ubuntu sandbox layer

[`build-distro-layer`](../usage.md#build-distro-layer) turns the prepared tree
into `ubuntu-distro.erofs` on Linux. It currently accepts only `--guest ubuntu`
and refuses to overwrite an existing layer without `--replace`. Before
packing, it applies the layer's metadata policy:

- only regular files, directories, and symbolic links;
- setuid and setgid bits cleared;
- no extended attributes, so no file capabilities, ACLs, security labels, or
  overlay attributes; and
- no `/sbin/init` and no `systemd` package.

The EROFS filesystem UUID is the first 128 bits of the layer's input digest, a
domain-separated SHA-256 over the Ubuntu release and architecture, the base
archive digest, the supplemental package digests, and the guest customization
files. The layer manifest records the full input digest beside the UUID and
the artifact's own SHA-256, so the UUID is never treated as an integrity
proof.

The layer runs under the Alpine control initramfs as the sandbox's read-only
`distro` layer over a writable scratch disk, which is formatted separately
with `mkfs.ext4`; see the
[experimental sandbox](../run.md#experimental-single-workload-sandbox). For a
one-shot workload, the Alpine
[container entry helper](../../guest/alpine/nvx-container-enter) injects its
musl loader, `setpriv`, and libraries into `/.nvx-agent`, so glibc Ubuntu
binaries run while the trusted agent stays musl-based. A managed sandbox's
agent, a static binary, enters each execution itself and runs the workload
without them. The workload gets private mount, PID, and UTS namespaces and
shares the guest's network namespace. It runs as a fixed non-root identity, by
default `65534:65534`, the Ubuntu `nobody` account whose home the builder
creates, with no supplementary groups, no capabilities, and `no_new_privs`.

The sandbox rejects systemd entrypoints, and it rejects a distro layer whose
manifest lists the `systemd` package. Systemd needs a separate compatibility
profile.

## Provenance and corresponding source

Each Ubuntu artifact has a package manifest beside it:
`initramfs-ubuntu.cpio.gz.packages.json` and
`ubuntu-distro.erofs.manifest.json`. Both record the release, architecture,
root filesystem digest, input digest, artifact digest, and NVX helpers. Each
package entry names the binary and source package versions, the license file,
the `.deb` digest for supplemental packages, and whether it came from Ubuntu
Base or the lock.

Release staging rejects an Ubuntu manifest that does not match its artifact
and the current inputs. Platform releases carry all four Ubuntu files under
`guest/`, with the sandbox scratch template, `ubuntu-smoke-scratch.ext4`,
which has no manifest; `nvx-rs download` installs them all, and
`run --guest ubuntu` fails when the initramfs is missing instead of falling
back to Alpine.
[Package and source delivery](../distribution.md) describes how
`collect-sources` materializes the corresponding Ubuntu source.

## Validation

CI builds the Ubuntu artifacts, with the sandbox scratch template, when their
inputs change and rebuilds them twice to compare their digests; locally,
[`verify-guest-determinism`](../usage.md#verify-guest-determinism) does the
same. The [microVM test workflow](../../.github/workflows/run-nvx-microvm-tests.yml)
runs Ubuntu boot, identity, console, lifecycle, network, and filesystem
snapshot scenarios on every backend, broader lifecycle, network snapshot, SMP,
snapshot, and workload-identity scenarios on Linux/KVM, and sandbox layer and
live-share smoke tests on every backend. Ubuntu is not part of the
performance gates.

## Full Ubuntu systemd guest (Proposed)

A full-OS profile could boot the NVX kernel against a writable ext4 root disk
with systemd as PID 1. It needs its own design for a neutral root-disk role
rather than the sandbox `scratch` role, systemd and udev packaging, network
configuration from the `virtnet_*` tokens, console login, readiness and clean
shutdown, mutable-root snapshots, and the security expectations of a
privileged outer guest.

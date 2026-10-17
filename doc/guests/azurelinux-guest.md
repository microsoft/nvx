# Azure Linux guest support

NVX runs Azure Linux 3.0 userland on its own Linux kernel. It builds one
artifact, `initramfs-azurelinux.cpio.gz`, which `nvx.py run --guest azurelinux`
boots for an interactive shell or a workload. The artifact boots no Azure Linux
kernel, firmware, or systemd, and it does not change the
[machine and device ABI](../design/2-virtual-machine-architecture.md).

[Build](../build.md), [Run](../run.md), and the
[command-line reference](../usage.md) cover the commands. This chapter
describes how the pieces fit together and why.

## Role

The Azure Linux initramfs is a workload guest, not a sandbox control
environment. Alpine remains the default guest of `run` and the only control
environment of `sandbox`:

- the initramfs omits the sandbox init agent, so the common `/init` refuses
  sandbox mode;
- the microVM test harness rejects the sandbox-control scenarios for Azure
  Linux; and
- `build-distro-layer` accepts only Ubuntu, so there is no Azure Linux
  sandbox layer.

A restore takes the guest from the captured RAM and machine contract, so `run`
rejects `--guest azurelinux` together with `--restore-snapshot`.

## Kernel policy

The initramfs boots the same `build/vmlinux` as Alpine and Ubuntu. The
[Ubuntu chapter](ubuntu-guest.md#kernel-policy) describes what that kernel
provides and omits. Azure Linux support therefore means Azure Linux userland
under the NVX kernel policy, not compatibility with every Azure Linux
workload.

## Root filesystem builder

The initramfs is built only with Docker: `build-initramfs --guest azurelinux`
and `build-guest` with Azure Linux selected use the Docker build, and
`build-guest --native` refuses Azure Linux. The
[Dockerfile](../../docker/Dockerfile) defines the build from pinned inputs:

- **Base image**: the Azure Linux 3.0 `base/core` container image from MCR,
  pinned by digest. `SOURCE-MANIFEST.json` repeats the release, image, and
  package-lock digest, and `scripts/nvx.py verify` fails when they disagree
  with the build constants.
- **Supplemental packages** from the lock in
  [`azurelinux/packages.lock.json`](../../azurelinux/packages.lock.json),
  which adds `busybox` and `util-linux`, the source of `setpriv`, with their
  dependencies. Each entry records the RPM's name, version, release,
  architecture, URL, and SHA-256. The lock must name the same release,
  architecture, and base image as the build, and every URL must point into the
  Azure Linux 3.0 base repository on `packages.microsoft.com`.

The build then:

1. downloads each locked RPM and verifies its SHA-256;
2. inside the pinned base image, imports the Microsoft RPM signing key,
   checks every RPM signature, installs the RPMs, and requires `ldd`,
   `setpriv`, and BusyBox to be present;
3. records the installed RPM inventory for the package manifest;
4. installs the common NVX `/init`, the guest scripts it needs, and the same
   nine static Rust binaries as Alpine and Ubuntu, so they do not depend on
   Azure Linux libraries;
5. links the BusyBox applets that the shared guest scripts expect, including
   `/bin/sh`; every other command resolves to the Azure Linux packages;
6. gives the `nobody` account, UID 65534, the empty home `/nonexistent`
   instead of `/dev/null`, because workload identities need an existing home
   directory; and
7. removes the RPM and package-manager databases and caches, which record
   install-time state.

The archive is packed with root ownership, timestamps set to the Unix epoch, a
sorted file order, and no compression timestamp, so a rebuild from the same
inputs is intended to be byte-identical. Unlike the Ubuntu artifacts, no
automated check compares two Azure Linux rebuilds.

## Azure Linux initramfs

The Azure Linux initramfs boots like the Alpine one at the machine level:
OpenVMM loads the kernel and initramfs through the
[Linux direct MP-table loader](../design/5.1-boot-protocol.md#511-linux-direct-loader),
and the common [`/init`](../../guest/common/init) runs as PID 1. It runs the
same boot steps as for the other guests, including the
[time ABI](../design/3.2-clock-and-time.md) boot step, and then runs an `nvx_exec`
workload, starts the managed agent, or opens a shell. The managed agent runs
the workload directly in the initramfs root, not in a separate layer. On the
shell path, `/init` reads the guest identity from `/etc/os-release`, stops on
an unknown one, prints `NVX-GUEST-BOOT-OK: azurelinux`, and starts the
BusyBox shell.

The guest defaults to 512 MiB, like Ubuntu, because the kernel unpacks the
initramfs into a RAM filesystem capped at half of guest memory; see
[Run](../run.md).

## Provenance and corresponding source

The package manifest `initramfs-azurelinux.cpio.gz.packages.json` records the
release, architecture, base image, input digest, and artifact SHA-256. Each
package entry names the RPM's version, release, architecture, license, and
source RPM. The manifest also records the shared aggregate Rust source digest
and each of the nine installed guest-binary digests. The input digest is a
domain-separated SHA-256 over the release, architecture, base image, and the
checkout files that define the build: the Dockerfile, package lock, build
modules, guest scripts, complete `nvx-rs` guest input set, and
`rust-toolchain.toml`.

Release staging rejects a manifest that does not match the artifact, the base
image, or the current inputs. Only binary-only packages carry the two Azure
Linux files; the published platform releases are binary-only, and `download`
installs them. `collect-sources` does not collect Azure Linux corresponding
source, so `package --include-source` omits the guest; see
[Package and source delivery](../distribution.md) and the
[release layout](../build.md).

## Validation

CI rebuilds the initramfs in Docker when its inputs change. The
[microVM test workflow](../../.github/workflows/run-nvx-microvm-tests.yml)
runs Azure Linux boot, identity, console exit, network, network snapshot,
workload identity, and managed lifecycle scenarios with one processor on every
backend. Azure Linux is not part of the performance gates.

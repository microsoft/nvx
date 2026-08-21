# NVX OpenVMM distribution

This repository builds and packages a small Alpine Linux guest for OpenVMM's
`microvm` machine. It keeps the exact Linux and Alpine source pins, complete
kernel patch series, guest-owned sources, build tooling, and benchmark
workloads in Git. Large upstream source trees are verified and materialized
under ignored build/cache directories only when building or preparing a
release. OpenVMM is the only private component and is pinned as the `openvmm`
Git submodule.

Access to `https://github.com/nanvix/openvmm` is required to initialize or
update the submodule. Cloning this repository does not grant access to it.

## Repository layout

| Path | Purpose |
| --- | --- |
| `kernel` | Reproducible configs and complete Linux patch series |
| `alpine` | PID 1, lifecycle helpers, virtio-fs integration, and workloads |
| `openvmm` | Private OpenVMM submodule pinned to `microvm/profile` |
| `benchmarks` | OpenVMM-native coordinator, methodology, and baselines |
| `scripts/nvx_tools` | Retained NVX build and benchmark implementation |
| `scripts/nvx.py` | Supported build, run, benchmark, and packaging CLI |
| `.cache/linux` | Generated verified/patched Linux tree; ignored by Git |
| `build/sources` | Generated Linux and Alpine release sources; ignored by Git |

## Initialize

Clone with submodules, or initialize after cloning:

```bash
git clone --recurse-submodules <umbrella-repository-url> nvx
cd nvx
python3 scripts/nvx.py verify
```

On Windows PowerShell, use `python` in place of `python3` if that is the
installed launcher:

```powershell
git clone --recurse-submodules <umbrella-repository-url> nvx
Set-Location nvx
python scripts\nvx.py verify
```

If the checkout already exists:

```bash
python3 scripts/nvx.py init
```

## Prerequisites

For Windows/WHP, install Rust stable, Visual Studio 2022 C++ build tools, the
Windows SDK, Python 3.10 or newer, Git, and Docker Desktop using Linux
containers. Enable **Windows Hypervisor Platform** and reboot:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -All
```

For Debian/Ubuntu with KVM:

```bash
sudo apt-get update
sudo apt-get install -y \
  bc binutils bison build-essential ca-certificates cpio curl flex gzip \
  libelf-dev libssl-dev python3 rsync tar xz-utils
sudo usermod -aG kvm "$USER"
```

Log out and back in after changing groups, then check access with
`test -r /dev/kvm && test -w /dev/kvm`.

## Continuous integration

The GitHub Actions workflow builds guest artifacts on Linux, then builds and
exercises OpenVMM on self-hosted runners labeled `linux, kvm` and
`windows, whp`. It uses the read-only OpenVMM deploy key stored in the
`OPENVMM_DEPLOY_KEY` Actions secret to fetch the private submodule at its
pinned commit. Each platform publishes benchmark tables and JSON/CSV artifacts;
pull requests gate regressions against recent history, and successful pushes to
`main` append their p50 values under `benchmarks/baselines/performance`. The
runners require the platform prerequisites listed above.

## Build

The portable workflow downloads the pinned Linux archive, verifies its
SHA-256, applies every patch in `kernel/patches`, and builds Linux plus the
Alpine initramfs in Docker. OpenVMM builds on the host:

```bash
python3 scripts/nvx.py build-guest
python3 scripts/nvx.py build-openvmm
```

On a Linux host, build the guest directly:

```bash
python3 scripts/nvx.py build-guest --native
```

Optional profiling variant:

```bash
python3 scripts/nvx.py build-guest --profiling
```

`--profiling` adds `build/vmlinux-profiling` while retaining the standard
`build/vmlinux` used by `run`.

The standard build produces:

```text
build/vmlinux
build/vmlinux.config
build/initramfs.cpio.gz
build/initramfs.cpio.gz.packages.json
openvmm/target/release/openvmm[.exe]
```

The native kernel build caches the verified and patched source under
`.cache/linux`, uses `O=build/linux`, runs `olddefconfig`, exports the exact
generated config as `build/vmlinux.config`, and fails if the Xen PVH note is
absent. Changing an archive hash or patch invalidates both source and object
caches.

## Run

The CLI chooses WHP on Windows and KVM on Linux:

```bash
python3 scripts/nvx.py run
```

A successful boot prints `ALPINE-MICROVM-BOOT-OK` and opens a root shell.
Exit cleanly from the guest with:

```sh
/sbin/nvx-exit 0
```

Pass extra guest options without changing the generated device ABI:

```bash
python3 scripts/nvx.py run \
  --memory-mib 256 \
  --net 10.0.0.2/24 \
  --cmdline "quiet loglevel=0"
```

### virtio-fs host mapping

The microVM supports one mapping with a fixed `microvm` tag. The initramfs
mounts it automatically:

```bash
python3 scripts/nvx.py run --mount "/mnt/host,/absolute/host/share,rw"
```

PowerShell example:

```powershell
python scripts\nvx.py run `
  --mount "/mnt/host,C:\Users\me\microvm-share,rw"
```

Use `ro` for read-only access. The guest target must be an absolute Linux path.
Host paths containing commas are unsupported. To expose multiple directories,
place them under one exported host root. Snapshot restore requires the same
target, mode, and underlying host directory.

## Benchmarks

The supported coordinator measures OpenVMM process launch through the
`ALPINE-MICROVM-BOOT-OK` marker, peak RSS, teardown, snapshot capture, and
restore:

```bash
python3 scripts/nvx.py benchmark -- --suite boot --backend whp
python3 scripts/nvx.py benchmark -- --suite e2e --backend kvm
```

Historical Linux/KVM and Windows/WHP CSVs are in `benchmarks/baselines`.
`benchmarks/NVX-METHODOLOGY.md` documents the inherited workload definitions.
The files in `scripts/nvx_tools` preserve the original NVX workload and parser
implementations; `benchmarks/openvmm.py` is the runner that uses OpenVMM's CLI
and marker-driven lifecycle.

## Package and source delivery

Stage a binary release and SHA-256 manifest:

```bash
python3 scripts/nvx.py package --binary-only
```

Binary-only mode requires an explicit acknowledgement because the matching
Linux and Alpine source must be published separately.

First materialize the release sources:

```bash
python3 scripts/nvx.py collect-sources
```

This produces a patched Linux corresponding-source archive under
`build/sources/linux` and exact Alpine recipes/upstream sources under
`build/sources/alpine`. Then stage the binary release with three separate
source artifacts:

```bash
python3 scripts/nvx.py package --include-source
```

The release contains:

```text
source/nvx-project-source-0.1.0.tar.gz
source/nvx-linux-source-6.18.38.tar.gz
source/nvx-alpine-source-0.1.0.tar.gz
```

Linux is GPL-2.0-only, so a distributor of `vmlinux` must make its complete
corresponding source available. Alpine packages retain their individual
licenses. The collector uses the exact aports commit embedded in every
installed APK and runs `abuild fetch` plus `abuild verify`. If Python
initramfs variants are present, `collect-sources` includes their package
manifests automatically.

OpenVMM is MIT licensed: retain its notice, but its source does not have to be
published merely because it is aggregated with Linux. See
`THIRD_PARTY_NOTICES.md`.

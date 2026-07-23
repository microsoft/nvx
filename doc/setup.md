# Host Setup

NVX runs natively on Linux through KVM and on Windows through Windows Hypervisor Platform (WHP).
The VMM is built on the host. The guest kernel and initramfs are Linux artifacts and can either be
built natively on Linux or produced with Docker on any supported host.

## Common requirements

- An x86-64 machine with hardware virtualization enabled in firmware.
- Git.
- Python 3.10 or newer.
- A stable Rust toolchain. The repository's `rust-toolchain.toml` selects `stable`; edition 2024
  requires Rust 1.85 or newer.
- Internet access when downloading Rust crates, Linux sources, or Alpine packages.

Clone the repository and check the shared workflow CLI:

```console
git clone https://github.com/nanvix/nvx.git
cd nvx
python scripts/nvx.py --help
```

On Linux, use `python3` in place of `python` if that is how Python is installed.

## Linux and KVM

The Linux backend requires `/dev/kvm` to exist and be readable and writable by the current user.
On distributions that use a `kvm` group, add the user to that group and start a new login session:

```console
sudo usermod -aG kvm "$USER"
test -r /dev/kvm && test -w /dev/kvm
```

For Debian 12 or Ubuntu, the native kernel and initramfs build dependencies are:

```console
sudo apt-get update
sudo apt-get install -y \
  build-essential flex bison libelf-dev libssl-dev bc cpio patch \
  xz-utils gzip tar curl ca-certificates python3
```

Optional features need additional host tools:

| Feature | Requirement |
| --- | --- |
| Read-only virt-fs | `mksquashfs` from `squashfs-tools` |
| Read-write virt-fs | `mke2fs` and `debugfs` from `e2fsprogs` |
| Managed TAP networking | `ip` from `iproute2`, plus root or passwordless `sudo ip` |
| Container artifact build | Docker with the Linux engine |

The VMM itself does not need root access. Elevated access is only needed when it creates and
configures a TAP for `--net`; attaching a correctly configured, user-owned TAP with `--net-tap`
does not run privileged `ip` commands.

## Windows and WHP

Use Windows 10 or 11 x64 with virtualization enabled in firmware. Enable Windows Hypervisor
Platform from an elevated PowerShell session, then reboot:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
```

Install these host tools:

- Rust stable for the `x86_64-pc-windows-msvc` host, including the MSVC C++ build tools and a
  Windows SDK.
- Python 3.10 or newer.
- Docker Desktop using Linux containers. Docker is needed to build the Linux guest artifacts, not
  to run the WHP VMM.

WHP standalone networking uses an in-process user-mode NAT and needs neither a TAP driver nor
administrator rights. The optional HCN and AF_XDP backend has additional privileged setup; see
[Networking](networking.md#external-hcn-and-af_xdp-networking).

## Verify the host

The following commands validate the toolchain without booting a guest:

```console
rustc --version
cargo --version
python --version
cargo build --release
cargo test --release
```

On a configured virtualization host, also run the backend's protected-mode self-test:

```console
# Linux
./target/release/microvm --selftest
```

```powershell
# Windows
.\target\release\microvm.exe --selftest
```

The self-test exercises the KVM or WHP protected-mode entry path but does not require a guest
kernel or initramfs. Continue with [Building NVX](build.md) to produce those artifacts.

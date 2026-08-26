# Setup

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

Python 3.10 or newer and Git are required on every platform.

For Debian/Ubuntu hosts:

```bash
sudo apt-get update
sudo apt-get install -y \
  bc binutils bison build-essential ca-certificates cpio curl flex gzip \
  libelf-dev libssl-dev python3 rsync tar xz-utils
```

### Linux / KVM

Enable Intel VT-x or AMD-V in the host firmware. Linux normally loads the matching KVM kernel
module automatically; `/dev/kvm` should exist after boot. Add the current user to the `kvm` group:

```bash
sudo usermod -aG kvm "$USER"
```

Log out and back in after changing groups, then check access with
`test -r /dev/kvm && test -w /dev/kvm`.

### Linux / MSHV

MSHV requires Linux running as a Microsoft Hypervisor root partition with an MSHV-enabled kernel.
The kernel must expose `/dev/mshv`; if its MSHV root driver is a module, load it with:

```bash
sudo modprobe mshv_root
```

Grant the current session read/write access and verify the device:

```bash
sudo chmod a+rw /dev/mshv
test -r /dev/mshv && test -w /dev/mshv
```

Azure Linux self-hosted runners also need the native toolchain used to build
the pinned cargo-nextest release:

```bash
sudo tdnf install -y \
  gcc glibc-devel kernel-headers binutils make \
  pkgconf pkgconf-pkg-config openssl-devel libarchive
cargo install --locked cargo-nextest --version 0.9.133
```

### Windows / WHP

Enable hardware virtualization in the host firmware. Then enable **Windows Hypervisor Platform**
from an elevated PowerShell session and reboot:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -All
```

Building NVX locally also requires Rust stable, Visual Studio 2022 C++ build tools, the Windows
SDK, and Docker Desktop using Linux containers.

## Remote agent hosts

The NVX agent skills can run and diagnose NVX on preconfigured SSH hosts. Remote development is
opt-in: copy the tracked host inventory template to the ignored `.nvx-hosts.json` file at the
repository root.

On Linux:

```bash
cp scripts/nvx-hosts.example.json .nvx-hosts.json
```

On Windows PowerShell:

```powershell
Copy-Item scripts\nvx-hosts.example.json .nvx-hosts.json
```

Edit each profile in `.nvx-hosts.json`:

- `version` must be `2`.
- The profile name is the name supplied to the `nvx-host-connect` skill.
- `ssh_target` is one destination or alias configured in the developer's SSH configuration.
- `backend` is exactly one of `kvm`, `mshv`, or `whp`.
- `host_type` identifies the server itself and is exactly one of `bare-metal` or
  `virtual-machine`.
- `remote_repo` is the absolute path to the NVX checkout on that host. It may be omitted so the
  agent asks for it when connecting.
- `notes` is optional developer-local context.

Inventory version 2 makes `host_type` required. Existing version 1 inventories are
not accepted because the resolver cannot reliably infer the host type. To migrate,
add `host_type` to every profile and change `version` from `1` to `2`; the resolver
reports these steps when it encounters a version 1 inventory.

Keep authentication in the SSH agent and SSH configuration. Do not put passwords, tokens,
private keys, passphrases, or SSH options in `.nvx-hosts.json`. The file is ignored by Git.

List and validate the configured profiles on Linux with:

```bash
python3 .github/skills/nvx-host-connect/scripts/hosts.py
```

On Windows PowerShell, use:

```powershell
python .github\skills\nvx-host-connect\scripts\hosts.py
```

If the inventory is absent or contains no hosts, the resolver disables remote access and reports
that only local NVX development is enabled.

## Development

Install the pinned Python development tools:

```bash
python3 -m pip install --requirement requirements-dev.txt
```

Install ShellCheck 0.11.0 and shfmt 3.12.0 using the platform package manager
to match the versions enforced in CI.

Run all lint and formatting checks before submitting a change:

```bash
python3 -m ruff check scripts benchmarks
shellcheck --shell=sh \
  alpine/init alpine/nvx-container-enter alpine/nvx-container-launch \
  alpine/nvx-exit alpine/nvx-hostmount alpine/nvx-init-agent \
  alpine/nvx-snapshot
python3 -m pyright --pythonplatform Linux
python3 -m pyright --pythonplatform Windows
python3 -m ruff format --check scripts benchmarks
shfmt -d -ln posix -i 4 -ci \
  alpine/init alpine/nvx-container-enter alpine/nvx-container-launch \
  alpine/nvx-exit alpine/nvx-hostmount alpine/nvx-init-agent \
  alpine/nvx-snapshot
```

Pyright runs in strict mode for both Linux and Windows platform APIs.

Apply the configured Python and POSIX shell formatters with:

```bash
python3 -m ruff format scripts benchmarks
shfmt -w -ln posix -i 4 -ci \
  alpine/init alpine/nvx-container-enter alpine/nvx-container-launch \
  alpine/nvx-exit alpine/nvx-hostmount alpine/nvx-init-agent \
  alpine/nvx-snapshot
```

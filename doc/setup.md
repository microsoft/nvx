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
  alpine/init alpine/nvx-exit alpine/nvx-hostmount alpine/nvx-snapshot
python3 -m pyright --pythonplatform Linux
python3 -m pyright --pythonplatform Windows
python3 -m ruff format --check scripts benchmarks
shfmt -d -ln posix -i 4 -ci \
  alpine/init alpine/nvx-exit alpine/nvx-hostmount alpine/nvx-snapshot
```

Pyright runs in strict mode for both Linux and Windows platform APIs.

Apply the configured Python and POSIX shell formatters with:

```bash
python3 -m ruff format scripts benchmarks
shfmt -w -ln posix -i 4 -ci \
  alpine/init alpine/nvx-exit alpine/nvx-hostmount alpine/nvx-snapshot
```

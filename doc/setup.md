# Setup

## Initialize

Building from source and `nvx.py verify` need the `openvmm` submodule, which the
[Quick Start](../README.md#quick-start) clone omits because its prebuilt package
does not need it. Clone with submodules, or initialize after cloning:

```bash
git clone --recurse-submodules https://github.com/microsoft/nvx.git nvx
cd nvx
python3 scripts/nvx.py verify
```

On Windows PowerShell, use `python` in place of `python3` if that is the
installed launcher:

```powershell
git clone --recurse-submodules https://github.com/microsoft/nvx.git nvx
Set-Location nvx
python scripts\nvx.py verify
```

If the checkout already exists:

```bash
python3 scripts/nvx.py init
```

## Prerequisites

Git is required on every platform, and Python 3.10 or newer to build, test,
benchmark, or package NVX. Building `nvx-rs`, OpenVMM, or the other Rust crates
requires [rustup](https://rustup.rs/), which installs the Rust release that
[`rust-toolchain.toml`](../rust-toolchain.toml) pins on first use. NVX runs its
microVMs and sandboxes with `nvx-rs`, which Cargo builds; see
[Usage](usage.md#nvx-rs). Its [`download`](usage.md#download) of a release, and
the download of a checkout's first run, use the host's `curl` and `tar`, which
Windows 10 and later include.

By default, a microVM boots on the built-in
[CPU profile](usage.md#cpu-profiles) of its host's CPU: four Intel
generations and AMD's EPYC Milan, Genoa, and Turin, on a hypervisor that
supports that profile. On another Intel or AMD CPU, `--cpu-profile auto`, the
default, falls back to a development profile derived from the host and warns
at every cold boot, asking for the CPU's fingerprint so that a built-in
profile can serve it; `nvx-rs run --cpu-profile host` selects that profile
explicitly, for example where the hypervisor does not support the built-in
profile (`E_PROFILE_UNSUPPORTED`, whose message names this option). OpenVMM
verifies a host profile as it verifies a built-in profile: the cold boot
still fails with `E_PROFILE_UNSUPPORTED` if the hypervisor lacks a CPU
feature that the time ABI requires. CPUs of other vendors have no CPU
profiles.

For Debian/Ubuntu hosts:

```bash
sudo apt-get update
sudo apt-get install -y \
  bc binutils bison build-essential ca-certificates cpio curl e2fsprogs \
  erofs-utils flex gzip libelf-dev libssl-dev python3 rsync tar xz-utils zstd
```

Native guest builds use the exact Rust release in `rust-toolchain.toml` and
its musl standard-library target:

```bash
toolchain=$(sed -n 's/^channel = "\([^"]*\)".*/\1/p' rust-toolchain.toml)
rustup toolchain install "$toolchain" --profile minimal
rustup target add --toolchain "$toolchain" x86_64-unknown-linux-musl
```

Both modes of the Linux setup script, and Copilot setup, install this target
automatically. `build-essential` remains necessary for the kernel and as
Rust's linker driver; no separate guest libc compiler is required.

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

RPM-based MSHV hosts also need the native toolchain used to build the
cargo-nextest release that
[`scripts/setup/tool-versions.conf`](../scripts/setup/tool-versions.conf) pins:

```bash
sudo tdnf install -y \
  gcc glibc-devel kernel-headers binutils make \
  pkgconf pkgconf-pkg-config openssl-devel libarchive
cargo install --locked cargo-nextest --version \
  "$(sed -n 's/^cargo_nextest\.version=//p' scripts/setup/tool-versions.conf)"
```

The hypervisor and the kernel also decide what NVX's benchmarks measure. CI measures the
Linux/MSHV series on Azure Linux 3 with hypervisor build `10.0.26100.9444`, from `mshv`
`26100.9444.2609032029.1`, and the `6.18.34.mshv3-1.azl3` kernel: the releases that
[`setup-mshv.sh`](../scripts/setup/azure/linux/setup-mshv.sh) pins and that the
[expected benchmark host](benchmarks.md#expected-benchmark-hosts) requires. The older `mshv2`
software of #415, such as the `6.6.137.mshv2-2.azl3` kernel, measured snapshot-restore
guest-exit teardown about four times slower than the `6.6.148-200.azl3` kernel, and the 8-vCPU
512 MiB shell restore 1.7 times slower, so compare a host's results with CI's only on the same
software. On Azure Linux, `rpm -q mshv kernel-mshv` reports the installed releases, and
benchmark results record the running hypervisor's build.

### Windows / WHP

Enable hardware virtualization in the host firmware. Then enable **Windows Hypervisor Platform**
from an elevated PowerShell session and reboot:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -All
```

Building NVX locally also requires rustup, Visual Studio 2022 C++ build tools, the Windows
SDK, and Docker Desktop using Linux containers.

## Automated environment bootstrap

The scripts in [`scripts/setup`](../scripts/setup/README.md) prepare Linux/KVM, Linux/MSHV, and
Windows/WHP development or GitHub Actions hosts. On Linux, `setup-linux.sh` configures a
development host for the selected backend by default and an Actions runner with
`--runner-only`, as `setup-windows-whp.ps1` does on Windows with `-RunnerOnly`. They install the
platform build prerequisites, configure hypervisor access, and provide non-mutating validation
modes. Runner registration tokens are read from standard input so they do not appear in command
history.

Linux can also emit a revision-bound, checksummed guest artifact bundle for Windows. The Windows
script verifies that bundle before copying it into the checkout and building OpenVMM natively.

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

The inventory schema is intentionally unversioned. Its top-level object contains
only `hosts`; do not add a `version` field.

Edit each profile in `.nvx-hosts.json`:

- The profile name is the name supplied to the `nvx-host-connect` skill.
- `ssh_target` is one destination or alias configured in the developer's SSH configuration.
- `backend` is exactly one of `kvm`, `mshv`, or `whp`.
- `host_type` is required, identifies the server itself, and is exactly one of
  `baremetal` or `virtual-machine`.
- `remote_repo` is the absolute path to the NVX checkout on that host. It may be omitted so the
  agent asks for it when connecting.
- `notes` is optional developer-local context.

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
python3 -m ruff check scripts .github/specula
shellcheck --shell=sh \
  guest/common/init guest/alpine/nvx-container-enter \
  guest/alpine/nvx-container-launch \
  guest/common/nvx-hostmount guest/common/nvx-identity-probe \
  guest/common/nvx-init-agent \
  guest/common/nvx-container-enter-azurelinux \
  guest/common/nvx-container-launch-azurelinux \
  guest/common/nvx-sandbox-smoke \
  guest/common/nvx-snapshot guest/common/nvx-virtio-restore-probe \
  scripts/setup/setup-linux.sh
shellcheck --shell=bash \
  .github/specula/setup-runner.sh guest/ubuntu/nvx-bashrc
python3 -m pyright --pythonplatform Linux
python3 -m pyright --pythonplatform Windows
python3 -m ruff format --check scripts .github/specula
shfmt -d -ln posix -i 4 -ci \
  guest/common/init guest/alpine/nvx-container-enter \
  guest/alpine/nvx-container-launch \
  guest/common/nvx-hostmount guest/common/nvx-identity-probe \
  guest/common/nvx-init-agent \
  guest/common/nvx-container-enter-azurelinux \
  guest/common/nvx-container-launch-azurelinux \
  guest/common/nvx-sandbox-smoke \
  guest/common/nvx-snapshot guest/common/nvx-virtio-restore-probe \
  scripts/setup/setup-linux.sh
shfmt -d -ln bash -i 4 -ci \
  .github/specula/setup-runner.sh guest/ubuntu/nvx-bashrc
python3 scripts/check_markdown_links.py
```

Pyright runs in strict mode for both Linux and Windows platform APIs. The link
check reads the files that Git tracks, so stage a new file before you run it.

### Adversarial campaign controller

The optional `test-adversarial` command additionally requires GitHub Copilot
CLI to be installed and authenticated before the run. Validate the controller
without changing authentication:

```bash
copilot --version
```

The harness performs its own non-interactive authenticated smoke prompt and
fails preflight if it cannot complete. It deliberately does not install
Copilot CLI or run `copilot login`. The repository's
`.github/workflows/copilot-setup-steps.yml` prepares Copilot cloud agent
sessions but does not install or authenticate Copilot CLI, so it does not
satisfy this prerequisite.

Production campaigns also require an administrator-owned, no-argument
executor wrapper that provisions a separate disposable target and forwards
the bounded executor protocol. See
[Copilot-driven adversarial testing](adversarial-testing.md)
for the containment and credential requirements, and for the Rust toolchain
and nvx-rs that the target needs.

Apply the configured Python and POSIX shell formatters with:

```bash
python3 -m ruff format scripts .github/specula
shfmt -w -ln posix -i 4 -ci \
  guest/common/init guest/alpine/nvx-container-enter \
  guest/alpine/nvx-container-launch \
  guest/common/nvx-hostmount guest/common/nvx-identity-probe \
  guest/common/nvx-init-agent \
  guest/common/nvx-container-enter-azurelinux \
  guest/common/nvx-container-launch-azurelinux \
  guest/common/nvx-sandbox-smoke \
  guest/common/nvx-snapshot guest/common/nvx-virtio-restore-probe \
  scripts/setup/setup-linux.sh
shfmt -w -ln bash -i 4 -ci \
  .github/specula/setup-runner.sh guest/ubuntu/nvx-bashrc
```

# Environment bootstrap

These scripts prepare an existing NVX checkout on a supported host:

- `setup-linux-mshv.sh` configures Linux/MSHV build and device prerequisites.
- `setup-windows-whp.ps1` configures Windows/WHP build prerequisites.
- `setup-linux-runner.sh` configures a Linux/KVM or Linux/MSHV Actions runner.

They do not create machines or initialize a checkout. The runner modes consume
a short-lived registration token from standard input and do not store repository
credentials in the scripts. Initialize the OpenVMM submodule before using the
development-host modes.

## GitHub Actions runners

Generate a repository runner registration token on an authenticated workstation,
then stream it to the target without placing it in shell history. On Linux:

```bash
scp scripts/setup/setup-linux-runner.sh HOST:/tmp/setup-linux-runner.sh
gh api --method POST repos/microsoft/nvx/actions/runners/registration-token \
  --jq .token |
  ssh HOST '/tmp/setup-linux-runner.sh --backend kvm --runner-name azure-kvm-3 --runner-token-stdin'
```

For Windows/WHP, stage `setup-windows-whp.ps1` and run it over SSH from
PowerShell:

```powershell
$token = gh api --method POST `
  repos/microsoft/nvx/actions/runners/registration-token --jq .token
$token | ssh HOST powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File C:\setup-windows-whp.ps1 -RunnerOnly `
  -RunnerName azure-windows-3 -RunnerTokenStdin
$token = $null
```

Both scripts pin and verify the Actions runner package. Linux runner labels are
`linux`, the selected backend, `virtual-machine`, and the runner name. Windows
labels are `windows`, `whp`, `virtual-machine`, and the runner name.
Runner services receive an explicit tool PATH. On Windows, the Rust toolchain
is read-only to the service account while Cargo registry and Git caches use the
runner's per-job temporary directory.
On both platforms, runner and toolchain executables are administrator-owned and
read-only to jobs, automatic runner updates are disabled, and writable runner
state is confined to `_work`.
Persistent runners do not have Docker access. Guest artifacts are built with
Docker on a GitHub-hosted runner instead.

Validate an installed runner without changing the host:

```bash
sh scripts/setup/setup-linux-runner.sh \
  --backend kvm --runner-name azure-kvm-3 --check-only
```

```powershell
.\scripts\setup\setup-windows-whp.ps1 `
  -RunnerOnly -RunnerName azure-windows-3 -CheckOnly
```

## Linux / MSHV

The Linux script supports distributions with `apt-get`, `dnf`, or `tdnf`. It
installs native build dependencies, stable Rust 1.95 or newer, and
cargo-nextest 0.9.133. It installs Docker with Buildx and configures
group-based access to Docker and `/dev/mshv`.

Run the complete bootstrap from the checkout:

```bash
sh scripts/setup/setup-linux-mshv.sh
```

Changing group membership exits with status 20. Reconnect and run the script
again so the new membership applies.

Validate dependencies without changing the host or requiring build artifacts:

```bash
sh scripts/setup/setup-linux-mshv.sh --check-only --skip-build
```

Omit `--skip-build` to also validate the built artifacts and generated MSHV
command. Use `--workspace PATH` when the script is outside the checkout.

To produce a revision-bound guest bundle for a Windows build:

```bash
sh scripts/setup/setup-linux-mshv.sh \
    --guest-bundle /path/to/nvx-guest-artifacts
```

The destination must be absent or empty. The bundle contains the kernel,
initramfs, package manifest, checkout revision, and SHA-256 checksums.
Use `--bundle-only` with `--guest-bundle` to package artifacts produced by a
separate `build-guest` invocation without provisioning or rebuilding the host.

## Windows / WHP

Run the complete bootstrap from an elevated Windows PowerShell session. It uses
WinGet to install missing tools, installs stable Rust 1.95 or newer and
cargo-nextest 0.9.133, enables Windows Hypervisor Platform, and builds OpenVMM.
It never reboots automatically.

Pass a guest bundle produced on Linux to complete the build validation:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass `
  -File .\scripts\setup\setup-windows-whp.ps1 `
  -GuestArtifactsDirectory .\nvx-guest-artifacts
```

The script verifies every checksum and requires the bundle revision to match
the checkout. A required reboot exits with status 3010. Missing guest artifacts
exit with status 21 after the host tools and OpenVMM build are ready.

Validate dependencies without changing the host or requiring build artifacts:

```powershell
.\scripts\setup\setup-windows-whp.ps1 -CheckOnly -SkipBuild
```

Omit `-SkipBuild` to also validate the guest artifacts, OpenVMM binary, and
generated WHP command. Use `-Workspace PATH` when the script is outside the
checkout.

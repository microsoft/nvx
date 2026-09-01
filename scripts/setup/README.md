# Environment bootstrap

These scripts prepare an existing NVX checkout on a supported host:

- `setup-linux-mshv.sh` configures Linux/MSHV build and device prerequisites.
- `setup-windows-whp.ps1` configures Windows/WHP build prerequisites.

They do not create machines, initialize a checkout, or manage repository
credentials. Initialize the OpenVMM submodule before running either script.

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

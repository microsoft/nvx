# Environment bootstrap

These scripts prepare an existing NVX checkout on a supported host:

- `setup-linux-mshv.sh` configures Linux/MSHV build and device prerequisites.
- `setup-windows-whp.ps1` configures Windows/WHP build prerequisites.
- `setup-linux-runner.sh` configures a Linux/KVM or Linux/MSHV Actions runner.

They do not create machines or initialize a checkout. The runner modes consume
a short-lived registration token from standard input and do not store repository
credentials in the scripts. Initialize the OpenVMM submodule before using the
development-host modes.

Each script reads the versions, release artifacts, and checksums of the tools
that it installs from [`tool-versions.conf`](tool-versions.conf) in its own
directory, so stage the manifest next to a script that runs outside a
checkout; see [Tool versions](#tool-versions).

## GitHub Actions runners

Generate a repository runner registration token on an authenticated workstation,
then stream it to the target without placing it in shell history. On Linux:

```bash
scp scripts/setup/setup-linux-runner.sh scripts/setup/tool-versions.conf HOST:/tmp/
gh api --method POST repos/microsoft/nvx/actions/runners/registration-token \
  --jq .token |
  ssh HOST '/tmp/setup-linux-runner.sh --backend kvm --runner-name azure-kvm-3 --runner-token-stdin'
```

For Windows/WHP, stage `setup-windows-whp.ps1` and `tool-versions.conf` in one
directory and run the script over SSH from PowerShell:

```powershell
scp scripts/setup/setup-windows-whp.ps1 scripts/setup/tool-versions.conf HOST:C:/
$token = gh api --method POST `
  repos/microsoft/nvx/actions/runners/registration-token --jq .token
$token | ssh HOST powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File C:\setup-windows-whp.ps1 -RunnerOnly `
  -RunnerName azure-windows-3 -RunnerTokenStdin
$token = $null
```

Both scripts verify the Actions runner package against the manifest's
checksum. Linux runner labels are `linux`, the selected backend, and
`virtual-machine`. Windows labels are
`windows`, `whp`, and `virtual-machine`. Runner names remain unique identities
but are not registered as labels.
On Linux, omit `--runner-name` to prepare host dependencies without reconciling
an existing runner's service or cache directory. Pass its name in a subsequent
invocation to update the service; `--check-only` still validates runner state.
Rustup bootstrap binaries are versioned and SHA-256 verified before execution;
Linux provisioning also installs `zstd` for native Actions cache archives.
Both runner setup scripts install a pinned, SHA-256-verified `sccache` binary.
Runner services use a persistent `_work/_sccache` directory with a 10-GiB
limit, disable Cargo incremental compilation, and expose `sccache` through
`RUSTC_WRAPPER`. CI uses clean Cargo target directories and reports per-job
cache statistics instead of restoring compiled `target/` trees.
Supply a fresh registration token again when migrating an existing runner or
changing its name, backend, or labels; provisioning replaces the registration
and records the expected label set in a protected local marker.
Runner services receive an explicit tool PATH. On Windows, the Rust toolchain
is read-only to the service account while Cargo registry and Git caches use the
runner's per-job temporary directory.
Windows runner provisioning also enables the full Hyper-V feature so the
licensed in-box PCAT and SVGA firmware required by OpenVMM VMM tests is
available under `System32`.
On both platforms, runner and toolchain executables are administrator-owned and
read-only to jobs, automatic runner updates are disabled, and writable runner
state is confined to `_work`.
Windows runner provisioning also creates a `nvx-benchmark-scratch` directory on
the largest non-system NTFS volume, or at `-BenchmarkScratchDirectory`, and
publishes it as the machine-level `NVX_BENCHMARK_SCRATCH` variable. Network
Service receives Modify access to that directory tree, as for `_work/_sccache`.
CI places benchmark snapshots and guest RAM backing files there so their
flushes avoid the burst-limited system disk. Check mode requires the directory
when a data volume exists. Changing the storage behind benchmark scratch or the
runner workspace is a performance-platform change: apply it to every Windows
runner at once and reset the affected history, as
[CI collection](../../doc/benchmarks.md#ci-collection) describes.
Persistent runners do not have Docker access. Guest artifacts are built with
Docker on a GitHub-hosted runner instead.
Linux provisioning runs through the SSH administrator, but the listener and
workflow jobs run as the dedicated `nvx-runner` account, which has neither sudo
nor Docker access.
Linux provisioning and check mode warn, but don't stop, when the host CPU
doesn't expose an invariant TSC (`nonstop_tsc` in `/proc/cpuinfo`). The flag
is evidence only. Before the time ABI, guests on such an Azure VM hit
cross-vCPU TSC warps during CPU activation (#265); under it, the guest warp
probe (H6) checks their skew against the ABI's 1 µs bound, so qualify such a
VM with `python3 scripts/nvx.py doctor --backend <backend>` before
registering it. CI's microVM and platform jobs run the probe on every runner.
Persistent runners execute pushes and same-repository pull requests only. Fork
pull requests remain on GitHub-hosted jobs until a maintainer stages the change
on a trusted repository branch.

Validate an installed runner without changing the host:

```bash
sh scripts/setup/setup-linux-runner.sh \
  --backend kvm --runner-name azure-kvm-3 --check-only
```

```powershell
.\scripts\setup\setup-windows-whp.ps1 `
  -RunnerOnly -RunnerName azure-windows-3 -CheckOnly
```

### Rust toolchain

The runner scripts install the Rust release that `rust.toolchain` in the
manifest pins, which equals the release in the repository's
[`rust-toolchain.toml`](../../rust-toolchain.toml), with its OpenVMM
targets, into the read-only trusted toolchain, and check mode requires exactly
that release. The `validate-runner` action fails a job on a runner that lacks
it, so update every runner before a change to the pin merges. A pass without a
runner name installs the new release next to the existing ones and leaves the
runner service and the default toolchain unchanged; jobs select the release
through `rust-toolchain.toml`. Stage the updated script and manifest, then run
that pass and validate the runner:

```bash
scp scripts/setup/setup-linux-runner.sh scripts/setup/tool-versions.conf HOST:/tmp/
ssh HOST 'sh /tmp/setup-linux-runner.sh --backend kvm'
ssh HOST 'sh /tmp/setup-linux-runner.sh --backend kvm --runner-name azure-kvm-3 --check-only'
```

```powershell
scp scripts/setup/setup-windows-whp.ps1 scripts/setup/tool-versions.conf HOST:C:/
ssh HOST powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File C:\setup-windows-whp.ps1 -RunnerOnly
ssh HOST powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File C:\setup-windows-whp.ps1 -RunnerOnly -RunnerName azure-windows-3 -CheckOnly
```

## Linux / MSHV

The Linux script supports distributions with `apt-get`, `dnf`, or `tdnf`. It
installs native build dependencies and the Rust and cargo-nextest releases
that the manifest pins. It installs Docker with Buildx and configures
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
WinGet to install missing tools, installs the Rust and cargo-nextest releases
that the manifest pins, enables Windows Hypervisor Platform, and builds OpenVMM.
Runner-only provisioning additionally enables Hyper-V for its in-box PCAT and
SVGA firmware. The script never reboots automatically.

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

## Tool versions

[`tool-versions.conf`](tool-versions.conf) pins the Rust toolchain, rustup,
cargo-nextest, sccache, and Actions runner releases that the setup scripts and
the [Specula runner setup](../../.github/specula/README.md) install, and the
SHA-256 checksum of each release artifact that they download; none of them
repeats a pin. Each line is a `#` comment or `KEY=VALUE` without spaces:

| Key | Value |
| --- | --- |
| `TOOL.version` | Exact `MAJOR.MINOR.PATCH` release |
| `rust.toolchain` | Rust release, equal to the channel in `rust-toolchain.toml` |
| `TOOL.artifacts.PLATFORM.name` | Artifact path in the release, where `{version}` stands for `TOOL.version` |
| `TOOL.artifacts.PLATFORM.sha256` | Lowercase SHA-256 checksum of that artifact |

`PLATFORM` is `linux-x86_64` or `windows-x86_64`. The scripts read the
manifest as data, with nothing but the shell or PowerShell, right after they
parse their options and before they inspect or change the host. They exit with
an error when the manifest is missing or malformed, lacks a pin that they
need, or names another platform; the error names the manifest and, for a
malformed entry, its line. They run or install a downloaded artifact only after
it matches its checksum.

To upgrade a tool, change its version and the checksum of each of its
artifacts in the manifest. The scripts download rustup from
`https://static.rust-lang.org/rustup/archive/VERSION/NAME`, sccache from
`https://github.com/mozilla/sccache/releases/download/vVERSION/NAME`, and the
Actions runner from
`https://github.com/actions/runner/releases/download/vVERSION/NAME`. A Rust
upgrade also changes `rust-toolchain.toml`; see
[Rust toolchain](../../doc/ci.md#rust-toolchain). `scripts/test_tool_versions.py`
checks on Linux and Windows that the manifest pins every tool for both
platforms, that the shell and PowerShell readers agree on it, and that no
script, workflow, or setup guide hard-codes a pin. Then rerun the scripts to
update existing hosts and runners.

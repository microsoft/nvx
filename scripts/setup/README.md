# Environment bootstrap

These scripts prepare a supported host for NVX:

- `setup-linux.sh` configures a Linux/KVM or Linux/MSHV host.
- `setup-windows-whp.ps1` configures a Windows/WHP host.

Each script has two modes. By default, it prepares a development host: an
existing NVX checkout from which a developer builds, tests, and runs NVX.
Runner-only mode, which `--runner-only` on Linux and `-RunnerOnly` on Windows
select, prepares a persistent GitHub Actions runner for NVX's CI jobs instead,
and needs no checkout.

For Azure VMSS bootstrapping, see [Azure provisioning](azure/README.md). The
scripts in this directory remain usable on supported hosts outside Azure.

They do not create machines or initialize a checkout. Runner-only mode
consumes a short-lived registration token from standard input and does not
store repository credentials in the scripts. Initialize the OpenVMM submodule
before using development-host mode.

Each script reads the versions, release artifacts, and checksums of the tools
that it installs from [`tool-versions.conf`](tool-versions.conf) in its own
directory, so stage the manifest next to a script that runs outside a
checkout; see [Tool versions](#tool-versions).

## Linux modes

`setup-linux.sh` requires `--backend kvm` or `--backend mshv` in both modes. It
supports distributions with `apt-get`, `dnf`, or `tdnf`, such as Ubuntu and
Azure Linux. Both modes install the same native build dependencies, the
pinned rustup, Rust release, and cargo-nextest, and a udev rule that lets only
the backend's group open its device, `/dev/kvm` or `/dev/mshv`; on MSHV, they
load `mshv_root` when the device is missing. They differ in everything else:

| | Development host (default) | Runner (`--runner-only`) |
| --- | --- | --- |
| Runs as | Root, or a user whose `sudo` may prompt | The SSH administrator with passwordless `sudo`, never root |
| NVX checkout | Required: the one that contains the script, or `--workspace PATH` | Not used |
| Rust tools | The current user's, with the musl target of the guest binaries | Root-owned and read-only to jobs under `/opt/nvx`, with OpenVMM's test targets and the pinned sccache |
| Docker | Installs Docker with Buildx unless the host has a Docker CLI, and adds the user to `docker` | Keeps `nvx-runner` out of `docker` |
| Backend device | Adds the user to the backend's group | Makes the backend's group the only supplementary group of `nvx-runner` |
| Completes with | A build of NVX and its guests, unless `--skip-build`, and `NVX_SETUP_COMPLETE=1` | With `--runner-name`, a registered runner service, and `NVX_RUNNER_SETUP_COMPLETE=1` |
| `--check-only` prints | `NVX_SETUP_CHECK=ok` | `NVX_RUNNER_SETUP_COMPLETE=1` |

Each mode rejects the other's options, so a misplaced option fails before the
script inspects or changes the host. [Linux development
hosts](#linux-development-hosts) and [GitHub Actions
runners](#github-actions-runners) describe the modes.

### Migrating from the former Linux scripts

`setup-linux.sh` replaces `setup-linux-mshv.sh` and `setup-linux-runner.sh`.
Stage it with `tool-versions.conf` instead of them, and translate their
invocations:

| Former invocation | Replacement |
| --- | --- |
| `setup-linux-mshv.sh [OPTIONS]` | `setup-linux.sh --backend mshv [OPTIONS]` |
| `setup-linux-runner.sh --backend BACKEND [OPTIONS]` | `setup-linux.sh --runner-only --backend BACKEND [OPTIONS]` |

Their options, output markers, and exit statuses are otherwise unchanged.
Development-host mode also accepts `--backend kvm` now, keeps a Docker
installation that the host already has, and its check mode also requires the
musl target that it installs. A runner that the former script configured needs
no migration: runner-only mode manages the same account, toolchain,
installation, and service.

## GitHub Actions runners

Generate a repository runner registration token on an authenticated workstation,
then stream it to the target without placing it in shell history. On Linux:

```bash
scp scripts/setup/setup-linux.sh scripts/setup/tool-versions.conf HOST:/tmp/
gh api --method POST repos/microsoft/nvx/actions/runners/registration-token \
  --jq .token |
  ssh HOST 'sh /tmp/setup-linux.sh --runner-only --backend kvm --runner-name azure-kvm-1 --runner-token-stdin'
```

For Windows/WHP, stage `setup-windows-whp.ps1` and `tool-versions.conf` in one
directory and run the script over SSH from PowerShell:

```powershell
scp scripts/setup/setup-windows-whp.ps1 scripts/setup/tool-versions.conf HOST:C:/
$token = gh api --method POST `
  repos/microsoft/nvx/actions/runners/registration-token --jq .token
$token | ssh HOST powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File C:\setup-windows-whp.ps1 -RunnerOnly `
  -RunnerName nvx-windows-3 -RunnerTokenStdin
$token = $null
```

Both scripts verify the Actions runner package against the manifest's
checksum. Linux runner labels start with `linux`, the selected backend, and
`virtual-machine`; Windows labels start with `windows`, `whp`, and
`virtual-machine`. The scripts also assign exactly one runner role. By default
they add `general`, which every non-benchmark backend job requires.
`--benchmark-runner` on Linux and `-BenchmarkRunner` on Windows add
`benchmark` instead, never `general`. Exactly one runner of each backend's pool
uses that switch, so only the platform job can run there and every run measures
the backend's performance series on the same VM. Moving the role to another
runner changes the series; see
[CI collection](../../doc/benchmarks.md#ci-collection). Provision the benchmark
runner with the switch, and pass it when checking that runner: a label that was
only added through the API is lost when provisioning replaces the registration.
Runner names remain unique identities but are not registered as labels. Use
`--extra-runner-labels LABELS` on Linux or `-ExtraRunnerLabels LABELS` on
Windows to add comma-separated pool labels without removing the required
labels. `general` and `benchmark` are reserved for the role switch. Other
labels may contain letters, digits, underscores, periods, and hyphens, and must
not duplicate another label case-insensitively. Changing a configured runner's
labels requires a fresh registration token; check mode requires the same role
and additional labels.

Roll out the `general` selector to an existing fleet before merging a workflow
that requires it:

1. Add `general` to every non-benchmark runner. The labels endpoint is additive,
   so this is harmless while existing jobs still target only the shared
   platform labels. For each runner ID, run:

   ```bash
   gh api --method POST \
     repos/microsoft/nvx/actions/runners/RUNNER_ID/labels \
     -f 'labels[]=general'
   ```

   Do not add `general` to a runner that carries `benchmark`.
2. After every general runner has the label, merge the workflow change. Reversing
   these steps leaves non-benchmark jobs with no matching runner.

A runner labeled only through the API keeps its previous labels in
`.nvx-labels`, so check mode reports its labels as not validated, and its next
provisioning run needs a registration token to register it again with
`general`.

Each series also names its benchmark runner's host in `data/<platform>-host.json`,
and its platform job fails before it measures on a runner whose CPU, processors,
memory, VM size, OS build, or benchmark disks and volumes differ from it. After
maintenance that can change them, such as a redeploy, resize, reimage, or OS
update, check the runner's next platform job. If its host check fails, accept the
new host and reset the series' history in one pull request, as
[Expected benchmark hosts](../../doc/benchmarks.md#expected-benchmark-hosts)
describes; moving the label to another runner takes the same pull request unless
that runner's host matches.
On Linux, omit `--runner-name` to prepare host dependencies without reconciling
an existing runner's service or cache directory. Pass its name in a subsequent
invocation to update the service; `--check-only` still validates runner state.
On Windows, an initial `-RunnerOnly` pass may create `_work` for sccache before
registration; a subsequent named pass accepts that directory but refuses
unrelated partial runner installations.
Rustup bootstrap binaries are versioned and SHA-256 verified before execution;
Linux provisioning also installs `zstd` for native Actions cache archives.
Both scripts' runner-only modes install a pinned, SHA-256-verified `sccache`
binary.
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
runner at once, and accept the benchmark runner's new host and reset its series'
history, as [Expected benchmark hosts](../../doc/benchmarks.md#expected-benchmark-hosts)
describes.
Persistent runners do not have Docker access. Guest artifacts are built with
Docker on a GitHub-hosted runner instead.
Linux provisioning runs through the SSH administrator, but the listener and
workflow jobs run as the dedicated `nvx-runner` account, which has neither sudo
nor Docker access.
Linux provisioning and check mode warn, but don't stop, when the host CPU
doesn't expose an invariant TSC (`nonstop_tsc` in `/proc/cpuinfo`). The flag
is evidence only. Before the time ABI, guests on such hosts hit
cross-vCPU TSC warps during CPU activation (#265); under it, the guest warp
probe (H6) checks their skew against the ABI's 1 µs bound, so qualify such a
VM with `python3 scripts/nvx.py doctor --backend <backend>` before
registering it. CI's microVM and platform jobs run the probe on every runner.
Persistent runners execute pushes, merge groups, scheduled and manually
dispatched full runs, and same-repository pull requests only. Fork pull
requests remain on GitHub-hosted jobs until a maintainer stages the change on a
trusted repository branch, or queues it: a merge group runs in the repository
whatever repository its pull requests come from, so a queued fork's pull
request runs its code on these runners, with the OpenVMM deploy key, before it
merges (see [Merge queue](../../doc/ci.md#merge-queue)).

Validate an installed runner without changing the host:

```bash
sh scripts/setup/setup-linux.sh \
  --runner-only --backend kvm --runner-name azure-kvm-1 --check-only
```

```powershell
.\scripts\setup\setup-windows-whp.ps1 `
  -RunnerOnly -RunnerName nvx-windows-3 -CheckOnly
```

### Rust toolchain

Runner-only mode installs the Rust release that `rust.toolchain` in the
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
scp scripts/setup/setup-linux.sh scripts/setup/tool-versions.conf HOST:/tmp/
ssh HOST 'sh /tmp/setup-linux.sh --runner-only --backend kvm'
ssh HOST 'sh /tmp/setup-linux.sh --runner-only --backend kvm --runner-name azure-kvm-1 --check-only'
```

```powershell
scp scripts/setup/setup-windows-whp.ps1 scripts/setup/tool-versions.conf HOST:C:/
ssh HOST powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File C:\setup-windows-whp.ps1 -RunnerOnly
ssh HOST powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File C:\setup-windows-whp.ps1 -RunnerOnly -RunnerName nvx-windows-3 -CheckOnly
```

## Linux development hosts

Development-host mode prepares a Linux/KVM or Linux/MSHV checkout for the user
who runs the script. It installs native build dependencies, Docker with Buildx,
and the Rust and cargo-nextest releases that the manifest pins, and configures
group-based access to Docker and to the backend's device. A host that already
has a Docker CLI keeps its Docker, such as Docker's own `docker-ce`, which the
distribution's packages would replace; install Buildx for it if it lacks the
plugin. The udev rule lets only the backend's group open the device, so
another user of the host needs that group too, even where the device was open
to everyone before.

Run the complete bootstrap from the checkout, naming the host's backend:

```bash
sh scripts/setup/setup-linux.sh --backend mshv
```

Changing group membership exits with status 20. Reconnect and run the script
again so the new membership applies.

Validate dependencies without changing the host or requiring build artifacts:

```bash
sh scripts/setup/setup-linux.sh --backend kvm --check-only --skip-build
```

Omit `--skip-build` to also validate the built artifacts and the command that
NVX generates for the backend. Use `--workspace PATH` when the script is
outside the checkout.

To produce a revision-bound guest bundle for a Windows build:

```bash
sh scripts/setup/setup-linux.sh --backend mshv \
    --guest-bundle /path/to/nvx-guest-artifacts
```

The destination must be absent or empty. The bundle contains the kernel, the
Alpine, Ubuntu, and Azure Linux initramfs images and their package manifests,
the Ubuntu distro layer and its manifest, the sandbox scratch template, the
build provenance of the kernel and the Alpine initramfs, which managed
sandboxes read as their runtime identity, the checkout revision, and SHA-256
checksums.
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
it matches its checksum. They install the Rust toolchain with
`--no-self-update`, so rustup never replaces itself with a release that the
manifest doesn't pin; on Windows, that replacement would also race the
script's next rustup command.

To upgrade a tool, change its version and the checksum of each of its
artifacts in the manifest. The scripts download rustup from
`https://static.rust-lang.org/rustup/archive/VERSION/NAME`, sccache from
`https://github.com/mozilla/sccache/releases/download/vVERSION/NAME`, and the
Actions runner from
`https://github.com/actions/runner/releases/download/vVERSION/NAME`. A Rust
upgrade also changes `rust-toolchain.toml`; see
[Rust toolchain](../../doc/ci.md#rust-toolchain). `scripts/test_tool_versions.py`
checks on Linux and Windows that the manifest pins every tool for both
platforms, that the shell and PowerShell readers agree on it, that no setup
lets rustup update itself, and that no script, workflow, or setup guide
hard-codes a pin. Then rerun the scripts to update existing hosts and runners.
They install rustup only where it is missing, so a new rustup pin reaches new
hosts only.

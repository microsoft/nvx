# Azure CI runner provisioning

These Azure-specific adapters configure **existing** NVX CI virtual machine
scale sets. They package the provider-neutral
[`setup-linux.sh`](../setup-linux.sh), which the Linux bootstrap runs in
runner-only mode, or [`setup-windows-whp.ps1`](../setup-windows-whp.ps1)
together with the adjacent
[`tool-versions.conf`](../tool-versions.conf). That manifest and
[`rust-toolchain.toml`](../../../rust-toolchain.toml) must pin the same release.
Do not maintain a second copy of the runner setup or its tool versions here.

| Component | Responsibility |
| --- | --- |
| `linux/bootstrap-runner.sh` | Validate Azure instance identity, prepare the attached disk and backend, derive a runner name, and register using managed identity and Key Vault. |
| `linux/block-runner-metadata.sh` | Block the dedicated runner account from Azure instance metadata with a verified, reboot-persistent IPv4 owner rule. |
| `linux/setup-data-disk.sh` | Find an Azure managed disk by LUN, preserve recognized ext4 data, and initialize only a blank or owned incomplete disk. |
| `linux/setup-mshv.sh`, `linux/verify-mshv.sh` | Optional Azure Linux 3 Dom0 boot preparation and post-boot verification. This differs from `../setup-linux.sh`, which requires an MSHV kernel that is already running. |
| `windows/bootstrap-runner.ps1` | Validate instance identity; set up disk, WHP/Hyper-V features, reboot continuation, and the Network Service runner. |
| `windows/setup-data-disk.ps1` | Safely prepare an Azure managed NTFS data disk and handle temporary-drive/pagefile conflicts. |
| `windows/vmss-bootstrap.ps1`, `windows/install-package.ps1` | Run through Custom Script, configure OpenSSH, verify and unpack the source bundle, and start Windows bootstrap. |
| `build_package.py` | Validate configuration and generate deterministic guest payloads, hashes, and a partial VMSS model patch without accessing Azure or GitHub. |

## Configuration

`linux/config.example.json` and `windows/config.example.json` are the only
configurations tracked here. Copy an example **outside the repository** for
each intended scale set and replace its placeholder values. Do not apply an
example directly or commit deployed resource inventories. A configuration
contains no credential: it refers to a Key Vault **name** and secret **name**,
not their values.

Both examples define `subscriptionId`, `resourceGroup`, `vmssName`, the
pre-existing `adminUsername`, a GitHub `repository` (`owner/name`),
`keyVaultName`, `bootstrapSecretName`, `runnerNamePrefix`,
`runnerNumberOffset`, `runnerDirectory`, `runnerLabels`, and `dataDisk`.
The default offset is 1; use 0 only if your pool deliberately numbers the
first instance zero. Azure instance metadata must match the subscription,
resource group, and VMSS in the configuration. The runner name is
`<runnerNamePrefix>-<numeric VMSS instance ID + runnerNumberOffset>`. It is
not inferred from an inventory position or supplied to the generic setup
scripts until the instance identity is verified.

`runnerLabels` must start with the generic CI labels. For Linux, use
`["linux", "<kvm|mshv>", "virtual-machine"]`; for Windows, use
`["windows", "whp", "virtual-machine"]`. Append optional pool labels after
these three. With no role label, the generic setup assigns `general`; an
explicit `general` has the same result. Append `benchmark` instead for the one
dedicated benchmark runner in each backend pool. The two role labels are
mutually exclusive. The adapter removes the role from the optional labels and
selects the generic setup's benchmark switch when needed; the setup script then
registers exactly one role along with the required platform labels. On either
platform, a configuration with an explicit `general` label can set
`benchmarkRunnerName` to one full runner name; only that instance receives
`benchmark` instead of `general`. The name uses the same prefix and numeric
offset as every other runner in the pool. A dedicated benchmark scale set can
continue to use the global `benchmark` label. Changing
the name, backend, or labels of a registered runner requires a fresh
registration token. Add `general` to existing non-benchmark runners before
merging workflows that require it, in the order documented in the
[runner bootstrap guide](../README.md#github-actions-runners).

Linux additionally selects `backend` (`kvm` or `mshv`), `kernelMode`, and
`dataDisk.mountPoint` below `/mnt` or `/srv`. `kernelMode: "preinstalled"`
expects the selected device to be ready. `azurelinux-mshv` installs the
Azure Linux 3 MSHV host stack that `linux/setup-mshv.sh` pins, the exact
releases of `mshv`, `mshv-bootloader-lx`, `kernel-mshv`, and `edk2-hvloader`,
logs them, and fails unless exactly those releases are installed. It then
checks the Dom0 boot entry, requests a reboot,
and verifies `/dev/mshv` afterwards; it is not an Ubuntu kernel installer.
The pins are the hypervisor and kernel that the Linux/MSHV benchmark series
measures, so a reimage reinstalls them instead of the newest releases. Move
them only in the pull request that accepts the new expected benchmark host
([Expected benchmark hosts](../../../doc/benchmarks.md#expected-benchmark-hosts)),
and regenerate the package for the scale set. If Azure Linux stops publishing
a pinned release, provisioning fails until the pin moves.
It does not install or require cloud-hypervisor or Kata.
Place `runnerDirectory` under the mounted data directory if runner work
and cache must use that disk; merely mounting it does not move `_work`.
The administrator must not be `root` or the reserved job account `nvx-runner`.
Disk setup installs the distribution's partitioning utility when it is
missing, verifies an existing mount before changing fstab, and verifies the
selected device again after mounting it. MSHV setup accepts only a Dom0
default wired into the active GRUB configuration; a saved entry alone is
insufficient.
An unmarked disk with no recognized signature must also pass a full-device
zero comparison before initialization; this can add I/O to first provisioning.
The owned interrupted-initialization recovery path remains separate.

Windows uses WHP and defines `dataDisk.driveLetter`,
`dataDisk.temporaryDriveFallback`, optional
`benchmarkScratchDirectory` on the managed data volume, `sshDefaultShell`,
`publicKeys`.
Omitted or null scratch paths resolve to
`<driveLetter>:\nvx-benchmark-scratch` before validation and are passed
explicitly to setup. Scratch must neither contain the runner installation nor
overlap it outside its writable `_work` subtree. Windows paths cannot contain
trailing-period components or reserved DOS device basenames, including names
with extensions. A nonempty public-key array becomes the configured
administrator authorized-key set; an empty array preserves existing keys.
Windows RAW disks must pass a full-device zero scan with bounded memory before
initialization; the continuation budget matches the runner's four-hour setup
budget. Existing unmarked GPT disks with no data partition are preserved,
not claimed as interrupted setup. A fresh marker is created only after verified
initialization in that invocation.
The extension verifies and stages the package, then immediately starts an
independent SYSTEM startup task so long-running bootstrap does not consume
the extension's execution budget. Bootstrap resumes through that task after
disk or feature reboots, then removes it after successful registration.
GitHub jobs run as Network Service with access to Azure instance metadata blocked.
SYSTEM installs the block first, checks the scale set's instance identity, and
stores only the VM size and disk performance settings under
`C:\ProgramData\NvxCiHostMetadata\compute.json`. The directory and file
allow Network Service to read, but not modify, this sanitized snapshot;
benchmark host telemetry uses it without querying instance metadata.
Missing or invalid snapshots fail the benchmark host check on provisioned
Windows runners. Older Windows hosts without this directory continue to use
instance metadata until they receive the updated bootstrap.
Linux bootstrap similarly writes only the VM size and disk performance settings
to `/var/lib/nvx-ci-host-metadata/compute.json`, owned by root and readable but
not writable by jobs. Provisioned Linux and Windows jobs use these snapshots;
a missing or invalid snapshot fails the host check without querying metadata.
Linux jobs run as `nvx-runner`, whose metadata block is installed before
registration. The runner service requires the block on every start; root
retains metadata access for bootstrap and token retrieval. Before every runner
start, a root-only `bootstrap-runner.sh --refresh-metadata` hook refreshes the
snapshot without running disk setup, provisioning, or registration. A failed
refresh prevents the runner service from starting. Metadata-rule updates
keep existing protection until a replacement is installed and verified; failures
stop setup without deleting that protection. Successful Linux registration
disables the provisioning unit without restarting the runner;
the metadata-block unit remains enabled. Windows requires an unrestricted,
all-profile metadata block for Network Service and rejects existing rules
with narrower filters. It also requires running firewall services, enforcing
profiles, and the effective rule in `ActiveStore`; a stored rule alone is
insufficient. Windows selects
managed NVMe disks by physical drive number and the documented
[Azure namespace mapping](https://learn.microsoft.com/en-us/azure/virtual-machines/enable-nvme-remote-faqs)
(namespace ID = data-disk LUN + 1); unknown or ambiguous mappings stop setup.

Both platforms require an attached managed disk at `dataDisk.lun` (0–63),
a suitable nested-virtualization-capable image, network access to the
platform's prerequisites, an existing administrator account, and a
system-assigned identity allowed to read the configured Key Vault secret.
The secret must contain a GitHub credential authorized to request runner
registration tokens for the configured repository. These adapters target
Azure public cloud and GitHub.com.

## Offline package generation

Install the package requirement and validate an edited configuration:

```powershell
python -m pip install -r scripts\setup\azure\requirements.txt
python scripts\setup\azure\build_package.py `
    --config <local-linux-config.json> --validate-only
python scripts\setup\azure\build_package.py `
    --config <local-linux-config.json> --output <directory-outside-nvx>
```

For Windows, pass the edited Windows configuration instead. Packaging makes
no Azure or GitHub API calls. It rejects unknown or credential-like
configuration values, generates SHA-256 hashes for the exact source files
and payload, and enforces Azure's 65,535-byte decoded custom-data limit.
Packaged PowerShell source uses canonical LF line endings so checkout-specific
CRLF conversion does not change the distributed bytes or hashes.
Embedded gzip files use a fixed timestamp, no filename, and a host-neutral
header rather than the platform-dependent shortcut in Python 3.11/3.12.
The output directory must be outside the repository.
It must be new or empty so files from another platform cannot be reused.

| Output | Role |
| --- | --- |
| `cloud-init.yaml` (Linux) | Root-owned setup scripts, tool manifest, private configuration, and a startup service. |
| `custom-data.json`, `sources.zip` (Windows) | Custom Script input and the hash-verified source/configuration bundle. |
| `extension-command.txt` (Windows) | Command for a compatible VMSS Custom Script extension to invoke the payload. |
| `model-patch.json`, `manifest.json` | Partial VMSS custom-data patch and exact target, toolchain, size, and hash evidence. |

Review the manifest's resource ID, existing VMSS provisioning logic, and
the extension configuration before using a patch. This tooling does not
create scale sets, disks, vaults, or identities, attach disks, deploy an
extension, reimage instances, or update a running runner. Azure may omit
customData from model reads; a successful model update alone is not proof
that an existing guest has run the new setup.

To stage initialization without upgrading running instances, first set the VMSS
model upgrade policy to `Manual`, retaining
`automaticOSUpgradePolicy.enableAutomaticOSUpgrade: true`. Then apply only the
generated custom-data model patch; do not call `update-instances`, reimage, or
restart. Remove obsolete bootstrap-only extensions from the staged model when
their responsibilities have moved into this package, preserving unrelated
extensions. Automatic OS image upgrades remain enabled and apply the latest model
when the next image arrives. Regenerate and stage the package whenever its
configuration or source changes: a repository commit does not update an already
embedded payload.

Disk initialization can format **blank** managed disks or partitions bearing
the helper's interrupted-initialization marker; existing incompatible or
ambiguous layouts fail closed. Bootstrap can enable virtualization features,
re-register a runner, and reboot an instance. Never run it on a busy or
shared runner without a separately approved maintenance window.

On Windows, `NVX_CLOUD_RUNNER_BOOTSTRAP_STAGED=1` acknowledges only the
scheduled-task handoff, not runner readiness. Follow
`C:\ProgramData\NvxBootstrap\setup-runner.log` and the
`Nvx-Setup-GitHub-Runner` task for bootstrap attempts. Operational failures emit
`NVX_CLOUD_RUNNER_SETUP_FAILED=1`, log the error, and preserve a failing task
result for retries; they do not change the already completed extension's
staging result.

`NVX_CLOUD_RUNNER_REBOOT_SCHEDULED=1` means setup will continue after boot,
not that registration has succeeded. Require
`NVX_CLOUD_RUNNER_SETUP_COMPLETE=1`, the generic setup script's checks under
the runner identity, and the expected online GitHub registration.

Offline validation:

```powershell
python -m unittest scripts.test_setup_azure scripts.test_tool_versions -v
```

The tests exercise configuration, exact package contents and size limits,
identity-to-name mapping, secret retrieval with mocked endpoints, and
installer behavior without touching a VMSS. Validate real disk, feature
reboots, and runner registration on an isolated new instance before rolling
out a configuration change.

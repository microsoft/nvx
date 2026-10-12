# Azure CI runner provisioning

These Azure-specific adapters configure **existing** NVX CI virtual machine
scale sets. They package the provider-neutral
[`setup-linux-runner.sh`](../setup-linux-runner.sh) or
[`setup-windows-whp.ps1`](../setup-windows-whp.ps1) together with the adjacent
[`tool-versions.conf`](../tool-versions.conf). That manifest and
[`rust-toolchain.toml`](../../../rust-toolchain.toml) must pin the same release.
Do not maintain a second copy of the runner setup or its tool versions here.

| Component | Responsibility |
| --- | --- |
| `linux/bootstrap-runner.sh` | Validate Azure instance identity, prepare the attached disk and backend, derive a runner name, and register using managed identity and Key Vault. |
| `linux/setup-data-disk.sh` | Find an Azure managed disk by LUN, preserve recognized ext4 data, and initialize only a blank or owned incomplete disk. |
| `linux/setup-mshv.sh`, `linux/verify-mshv.sh` | Optional Azure Linux 3 Dom0 boot preparation and post-boot verification. This differs from the checkout/build helper `../setup-linux-mshv.sh`. |
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
these three. The adapter passes **only** those additional labels to the
generic setup scripts, which always retain the required platform labels.
Changing the name, backend, or labels of a registered runner requires a
fresh registration token.

Linux additionally selects `backend` (`kvm` or `mshv`), `kernelMode`, and
`dataDisk.mountPoint` below `/mnt` or `/srv`. `kernelMode: "preinstalled"`
expects the selected device to be ready. `azurelinux-mshv` installs the
Azure Linux 3 MSHV host stack, checks the Dom0 boot entry, requests a reboot,
and verifies `/dev/mshv` afterwards; it is not an Ubuntu kernel installer.
Place `runnerDirectory` under the mounted data directory if runner work
and cache must use that disk; merely mounting it does not move `_work`.

Windows uses WHP and defines `dataDisk.driveLetter`,
`dataDisk.temporaryDriveFallback`, optional
`benchmarkScratchDirectory` on the managed data volume, `sshDefaultShell`,
and `publicKeys`. A nonempty public-key array becomes the configured
administrator authorized-key set; an empty array preserves existing keys.
Bootstrap resumes through a SYSTEM startup task after disk or feature
reboots, then removes the task after successful registration. GitHub jobs
run as Network Service with access to Azure instance metadata blocked.

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
The output directory must be outside the repository.

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

Disk initialization can format **blank** managed disks or partitions bearing
the helper's interrupted-initialization marker; existing incompatible or
ambiguous layouts fail closed. Bootstrap can enable virtualization features,
re-register a runner, and reboot an instance. Never run it on a busy or
shared runner without a separately approved maintenance window.

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

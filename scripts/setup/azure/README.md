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
| `nvx_azure/` | Standard-library Python package that guests run as `python -I -B <bootstrap directory> <command>`. `cli.py` dispatches the commands; `common.py` holds the shared identity, label, metadata-snapshot, HTTP/token, and checked-subprocess helpers. |
| `nvx_azure/linux_bootstrap.py` | Validate Azure instance identity, refresh the compute snapshot, prepare the attached disk and backend, derive a runner name, and register using managed identity and Key Vault. |
| `nvx_azure/linux_firewall.py` | Block the dedicated runner account from Azure instance metadata with a verified, reboot-persistent IPv4 owner rule. |
| `nvx_azure/linux_disk.py` | Find an Azure managed disk by LUN, preserve recognized ext4 data, and initialize only a blank or owned incomplete disk. |
| `nvx_azure/linux_mshv.py` | Optional Azure Linux 3 Dom0 boot preparation and post-boot verification. This differs from `../setup-linux.sh`, which requires an MSHV kernel that is already running. |
| `nvx_azure/windows_stage.py` | Run from Custom Script: verify and unpack the package, configure OpenSSH, and hand bootstrap to an independent SYSTEM task. |
| `nvx_azure/windows_bootstrap.py` | Validate instance identity; set up the disk, WHP/Hyper-V features, reboot continuation, and the Network Service runner. |
| `nvx_azure/windows_firewall.py` | Block Network Service from Azure instance metadata with a verified rule that the effective firewall policy enforces. |
| `nvx_azure/windows_disk.py` | Safely prepare an Azure managed NTFS data disk and handle temporary-drive/pagefile conflicts. |
| `nvx_azure/windows_system.py` | Run the PowerShell adapters and share registry, reboot, file, and locking helpers. |
| `windows/vmss-bootstrap.ps1` | Custom Script entry shim: protect the bootstrap directory, install the pinned Python, verify the package, and run `windows-stage`. |
| `windows/firewall.ps1`, `windows/openssh.ps1`, `windows/storage.ps1`, `windows/system.ps1` | Adapters that run the cmdlets a Python controller names, taking a JSON request and returning a JSON result. |
| `build_package.py` | Validate configuration and generate deterministic guest payloads, hashes, and a partial VMSS model patch without accessing Azure or GitHub. |

Linux guests run every step as
`/usr/bin/python3 -I -B /opt/nvx-bootstrap <command>`. Cloud-init installs the
distribution's `python3` and writes all guest files from the same payload
before the provisioning unit starts after `cloud-final.service`. Isolated mode
ignores `PYTHON*` variables and user site-packages, `-B` keeps bytecode out of
the root-owned directory, and the package needs only the standard library of
Python 3.10 or later; the packager's PyYAML dependency never reaches a guest.
External storage, firewall, package, and boot utilities remain checked
subprocesses that receive explicit argument lists.

Windows images include no Python, and the runner toolchain installs its own
only later, so the Custom Script extension runs a small PowerShell entry shim,
`windows/vmss-bootstrap.ps1`, that establishes trust before any Python runs. It
creates `C:\ProgramData\NvxBootstrap` protected from the start, writable only
by SYSTEM and Administrators, marks it, and uses an existing directory again
only with that mark. Earlier installers protected the directory only after
creating it, so another account could have added entries first: the shim moves
such a directory aside, and refuses one that another account can change. It
installs python.org's embeddable Python 3.14.10 for x64 into the directory's
`python` subdirectory from the URL and SHA-256 that `WINDOWS_PYTHON` in
`build_package.py` pins; the digest is the one in python.org's [release
manifest](https://www.python.org/ftp/python/3.14.10/windows-3.14.10.json). A
later run reuses that installation only while its recorded archive digest and
reported version still match the pin. The shim then checks the package's
SHA-256 from the payload, unpacks it with Python's own `tarfile` extractor and
its `data` filter, and runs `windows-stage` from that temporary verified
staging directory, which the shim removes afterwards. The scheduled task runs
`windows-bootstrap` from the persistent `C:\ProgramData\NvxBootstrap`
directory. This interpreter serves only the bootstrap: it is on no `PATH`,
jobs never use it, and SYSTEM runs both entry points as
`<python> -I -B <entry-directory> <command>`. To change the interpreter, pin
another python.org release and its manifest digest.

PowerShell remains only for Windows interfaces that the standard library lacks:
the Storage, NetSecurity, ScheduledTasks, DISM, Appx, CIM, and file-ACL
cmdlets. Each adapter runs the operation that a Python controller names, reads a
JSON request on standard input, writes its JSON result on a marked line of
standard output, and decides nothing.
The registry, files, processes, HTTP, the raw-disk zero scan, and every policy
decision stay in Python.

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
Azure Linux 3 MSHV host stack that `nvx_azure/linux_mshv.py` pins, the exact
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
The task also completes data-disk setup after the pagefile reboot; staging
removes the `NvxRunner-SetupDataDisk` task that earlier bootstraps registered
for that continuation. One bootstrap runs at a time: the task ignores a new
start while one runs, and bootstrap holds `bootstrap.lock` in its directory.
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
start, a root-only `linux-refresh-metadata` command refreshes the
snapshot without running disk setup, provisioning, or registration. A failed
refresh prevents the runner service from starting. Metadata-rule updates
keep existing protection until a replacement is installed and verified; failures
stop setup without deleting that protection. Successful Linux registration
disables the provisioning unit without restarting the runner;
the metadata-block unit remains enabled. Windows requires an unrestricted,
all-profile metadata block for Network Service and rejects existing rules
with narrower filters. It also requires running firewall services, enforcing
profiles, and the effective rule in `ActiveStore`, enforced in the active
profile; a stored rule alone is insufficient. Because the rule applies to every
profile, it may report the profiles of no current network as inactive. Windows selects
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
Packaged Python and shell sources must use LF line endings, and packaged
PowerShell source uses canonical LF line endings, so checkout-specific CRLF
conversion does not change the distributed bytes or hashes.
Embedded gzip files use a fixed timestamp, no filename, and a host-neutral
header rather than the platform-dependent shortcut in Python 3.11/3.12.
The Windows package is a USTAR archive of regular files in name order with
fixed metadata, compressed as one xz stream to fit the custom-data limit.
The output directory must be outside the repository.
It must be new or empty so files from another platform cannot be reused.

| Output | Role |
| --- | --- |
| `cloud-init.yaml` (Linux) | Root-owned Python bootstrap package, generic setup script, tool manifest, private configuration, and startup and metadata-block units. |
| `custom-data.json`, `sources.tar.xz` (Windows) | Custom Script input with the entry shim and Python pin, and the hash-verified package of configuration, setup sources, Python modules, and adapters. |
| `extension-command.txt` (Windows) | Command paired with the Windows custom data for a compatible VMSS Custom Script extension. |
| `model-patch.json`, `manifest.json` | Partial VMSS custom-data patch and exact target, toolchain, size, and hash evidence. |

The Windows custom data and `extension-command.txt` are one versioned unit:
the command decompresses the entry shim that the payload holds. Apply both in
the same VMSS model update. Never stage new Windows custom data with an older
extension command, which would write compressed bytes as a PowerShell script.

Review the manifest's resource ID, existing VMSS provisioning logic, and
the extension configuration before using a patch. This tooling does not
create scale sets, disks, vaults, or identities, attach disks, deploy an
extension, reimage instances, or update a running runner. Azure may omit
customData from model reads; a successful model update alone is not proof
that an existing guest has run the new setup.

To stage initialization without upgrading running instances, first set the VMSS
model upgrade policy to `Manual`, retaining
`automaticOSUpgradePolicy.enableAutomaticOSUpgrade: true`. On Linux, apply only
the generated custom-data model patch. On Windows, apply that patch and set the
compatible Custom Script extension's `commandToExecute` to the exact contents
of `extension-command.txt` in the same model update. Do not call
`update-instances`, reimage, or restart. Remove obsolete bootstrap-only
extensions from the staged model when
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
scheduled-task handoff, not runner readiness; staging logs to
`C:\WindowsAzure\Logs\vmss-bootstrap.log`. Follow
`C:\ProgramData\NvxBootstrap\setup-runner.log`,
`C:\WindowsAzure\Logs\setup-datadisk.log`, and the
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
python -m unittest scripts.test_setup_azure scripts.test_setup_azure_linux scripts.test_setup_azure_windows scripts.test_tool_versions -v
```

The tests exercise configuration, exact package contents and size limits,
identity-to-name mapping, secret retrieval with mocked endpoints, and each
guest controller's decisions against fake commands and adapters, without
touching a VMSS or the host's disks, firewall, boot configuration, or services.
On Windows they also run each adapter and the entry shim in Windows PowerShell
against mocked cmdlets. Validate real
disk, feature reboots, and runner registration on an isolated new instance
before rolling out a configuration change.

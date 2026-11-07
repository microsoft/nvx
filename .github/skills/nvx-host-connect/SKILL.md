---
name: nvx-host-connect
description: "Select a developer-configured NVX host, connect over SSH, identify Linux/KVM, Linux/MSHV, or Windows/WHP support, verify its checkout, and prepare artifacts. Use before remote NVX work or when diagnosing configured SSH host prerequisites."
argument-hint: "[host-profile] [revision]"
---

# Connect to an NVX Host

Establish a verified remote NVX session that another skill can use. Stop after
reporting the connection context and prepared artifacts; use a focused skill to run,
debug, or benchmark NVX.

## Local Host Inventory

Remote development is enabled only by the ignored `.nvx-hosts.json` inventory at
the repository root. Developers create it from `scripts/nvx-hosts.example.json` as
described in the [setup guide](../../../doc/setup.md), and keep
connection and authentication details in SSH configuration. Never place passwords,
tokens, private keys, passphrases, or SSH options in the inventory.

Before asking for an SSH target, list the configured profiles from the local
workspace:

```bash
python3 scripts/nvx_hosts.py
```

```powershell
python scripts\nvx_hosts.py
```

From a linked worktree, `nvx_hosts.py` reads the main worktree's inventory,
since Git does not copy the ignored inventory into linked worktrees.

Treat the resolver output as authoritative:

- If `remote_enabled` is `false`, inform the agent that remote hosts are not
  configured and only local NVX development is enabled. Stop this skill and use
  `nvx-run`, `nvx-debug`, or `nvx-benchmark` locally.
- If profiles are available and none was requested, ask the user to select one.
- Resolve the selected profile by passing its name to the same command. Connect only
  to the returned `ssh_target` and treat its `backend`, `platform`, and `host_type`
  as authoritative connection context. Do not infer whether the server is bare metal
  or a virtual machine from platform probes.
- If `remote_repo` is `null`, ask for the absolute checkout path. Ask only for
  fields missing from the selected profile.
- Do not connect to an ad hoc SSH target or a profile absent from the inventory.

Example profile resolution:

```bash
profile=kvm-lab # Replace with a configured profile name.
python3 scripts/nvx_hosts.py "$profile"
```

```powershell
$profile = 'kvm-lab' # Replace with a configured profile name.
python scripts\nvx_hosts.py $profile
```

## Required Inputs

Resolve these before changing or running anything:

- A host profile resolved from `.nvx-hosts.json`.
- Absolute path to the NVX checkout when the profile does not define `remote_repo`.
- Intended source revision or whether the existing remote checkout is authoritative.
- Artifact source: packaged release, fresh build, or existing remote build.

Ask only for missing values. Do not ask for passwords, private keys, tokens, or
passphrases in chat. Use the user's configured SSH agent or identity. If SSH asks
for a secret, pause and have the user enter it directly in the terminal.

## Safety Rules

- Do not disable SSH host-key checking. Stop on a changed host key.
- Do not run `git reset`, `git clean`, checkout another revision, pull, overwrite
  files, or update submodules in a dirty remote checkout.
- Prefer a separate checkout or worktree, such as the temporary worktree of
  [section 4](#4-establish-revision-and-artifacts), when the requested revision
  differs from the remote checkout. Get approval before transferring uncommitted
  files.
- Do not enable Windows features, change device permissions, edit `sudoers`, load
  kernel modules, reboot, or install packages without explicit approval.
- Do not use `sudo` merely to bypass KVM or MSHV access failures. Report the exact
  failed access check.
- Use the profile backend explicitly and verify that the host supports it. Never
  select `both` on a remote host or silently override the inventory.
- Check for an existing OpenVMM process. Never terminate a process that this session
  did not start.

## 1. Open a Persistent Session

Prefer one persistent interactive SSH session so shell state, the repository path,
and long-running command output remain visible. Use `ssh -tt` when a direct SSH
command must expose an interactive guest. Avoid complex cross-platform remote
one-liners; enter the remote shell and then use its native syntax.

```console
ssh <resolved-ssh-target>
```

If authentication needs a secret, let the user type it into the terminal. Determine
the OS by trying `python3` first on Linux and `python` first on Windows:

```console
python3 -c "import platform; print(platform.system(), platform.release(), platform.machine())"
python -c "import platform; print(platform.system(), platform.release(), platform.machine())"
```

Use exactly one matching host branch below.

## 2. Probe Linux/KVM or Linux/MSHV

```bash
cd -- '<remote-repo>'
uname -srmo
python3 --version
git rev-parse --show-toplevel
git rev-parse HEAD
git status --short
python3 scripts/nvx.py verify

for device in /dev/kvm /dev/mshv; do
    if test -e "$device"; then
        test -r "$device" && readable=yes || readable=no
        test -w "$device" && writable=yes || writable=no
        printf '%s readable=%s writable=%s\n' "$device" "$readable" "$writable"
    fi
done
pgrep -af openvmm || true
```

| Backend | Required check | Platform ID |
| --- | --- | --- |
| KVM | `/dev/kvm` exists and is readable and writable | `linux-kvm` |
| MSHV | `/dev/mshv` exists and is readable and writable | `linux-mshv` |

If both devices are usable and the backend was not specified, ask which one to use.
Do not infer MSHV merely because the host runs under a Microsoft hypervisor.

## 3. Probe Windows/WHP

```powershell
Set-Location -LiteralPath '<remote-repo>'
[Environment]::OSVersion.VersionString
python --version
git rev-parse --show-toplevel
git rev-parse HEAD
git status --short
python scripts\nvx.py verify
Get-CimInstance Win32_ComputerSystem |
    Select-Object Manufacturer, Model, HypervisorPresent
Get-Process openvmm -ErrorAction SilentlyContinue
```

Windows supports only `whp`, with platform ID `windows-whp`. If useful and the
current account can query optional features without elevation, inspect the feature:

```powershell
Get-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform |
    Select-Object FeatureName, State
```

An access-denied result is not a reason to elevate automatically. A smoke run is
the definitive WHP runtime check.

## 4. Establish Revision and Artifacts

Record `git rev-parse HEAD` and `git status --short`. Compare the commit with the
requested revision. If the checkout is dirty, preserve it and ask whether to use it
as-is or create an isolated checkout. Never silently test a different revision.

To test another revision, such as a branch that you pushed, without touching the
checkout, add a temporary detached worktree beside it. First check in the
checkout that `git remote get-url origin` resolves to `microsoft/nvx` and that
`git config --get submodule.openvmm.url`, when set, names `microsoft/openvmm`,
since the worktree fetches from both, and ask before correcting either. Then fetch
the branch, add the worktree at the exact revision, and initialize its submodule
from the checkout's module store, leaving out `--reference` when that store does
not exist:

```console
git fetch origin <branch>
git worktree add --detach <worktree> <revision>
git -C <worktree> submodule update --init --reference <checkout>/.git/modules/openvmm openvmm
```

Continue in the worktree, starting with `nvx.py verify` there. When the task ends,
remove it from the checkout with `git worktree remove --force <worktree>`: Git
removes a worktree that holds a submodule only with `--force`, which also discards
its changes and builds.

Choose artifact preparation according to the task:

- For packaged NVX artifacts, use `nvx-rs download` with the explicit backend.
  It installs that backend's package from the release built from `HEAD` when
  available, otherwise from the nearest released first-parent ancestor that has
  the package. Report the selected release as the artifact provenance.
- For guest, OpenVMM, or build changes, use the relevant `build-guest`,
  `build-openvmm`, or `build` command.
- `nvx-rs`, which downloads releases and runs NVX's microVMs and sandboxes,
  never comes with the package: build it with Cargo from the repository root
  first, and again after `nvx-rs` changes.
- For coordinator-only changes, existing matching release artifacts may be reused
  after reporting their provenance.
- For the public sandbox scenarios of `test-microvm`, also provide the scratch
  template, `build/ubuntu-smoke-scratch.ext4`, which `download` installs from the
  releases that ship it. For an older release, format it on a Linux host with
  `python3 scripts/nvx.py build-scratch-template`, which refuses to run on
  Windows, and copy it from there to a Windows host.

Linux examples:

```bash
cargo build --release --locked --features cli --manifest-path nvx-rs/Cargo.toml \
    --target-dir nvx-rs/target
nvx-rs/target/release/nvx-rs download --hypervisor kvm
nvx-rs/target/release/nvx-rs download --hypervisor mshv
python3 scripts/nvx.py build
```

Windows examples:

```powershell
cargo build --release --locked --features cli --manifest-path nvx-rs/Cargo.toml `
    --target-dir nvx-rs/target
nvx-rs\target\release\nvx-rs.exe download --hypervisor whp
python scripts\nvx.py build
```

Do not download over artifacts the user wants to preserve: `download` replaces
them all. `nvx-rs run` and `sandbox` download a release only into a checkout
that holds no artifacts; `NVX_AUTO_DOWNLOAD=0` turns that off. Verify the
generated command and required files without starting a VM:

```bash
backend=kvm # Use the resolved profile backend.
nvx-rs/target/release/nvx-rs run --hypervisor "$backend" --dry-run
```

```powershell
nvx-rs\target\release\nvx-rs.exe run --hypervisor whp --dry-run
```

## Handoff

Keep the SSH session open and report:

- SSH host alias, remote repository path, and any temporary worktree.
- Detected OS, selected backend, platform ID, and resolved host type.
- Commit and whether the checkout is dirty.
- Artifact source and completed preparation command.
- Dry-run result and any existing OpenVMM process.
- Any privilege or platform limitation.

Then load exactly one of
[nvx-run](../nvx-run/SKILL.md),
[nvx-debug](../nvx-debug/SKILL.md), or
[nvx-benchmark](../nvx-benchmark/SKILL.md).

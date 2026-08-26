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
python3 .github/skills/nvx-host-connect/scripts/hosts.py
```

```powershell
python .github\skills\nvx-host-connect\scripts\hosts.py
```

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
python3 .github/skills/nvx-host-connect/scripts/hosts.py "$profile"
```

```powershell
$profile = 'kvm-lab' # Replace with a configured profile name.
python .github\skills\nvx-host-connect\scripts\hosts.py $profile
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
- Prefer a separate checkout or worktree when the requested revision differs from
  the remote checkout. Get approval before transferring uncommitted files.
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

Choose artifact preparation according to the task:

- For the latest packaged NVX release, use `download` with the explicit backend.
- For guest, OpenVMM, or build changes, use the relevant `build-guest`,
  `build-openvmm`, or `build` command.
- For coordinator-only changes, existing matching release artifacts may be reused
  after reporting their provenance.

Linux examples:

```bash
python3 scripts/nvx.py download --hypervisor kvm
python3 scripts/nvx.py download --hypervisor mshv
python3 scripts/nvx.py build
```

Windows examples:

```powershell
python scripts\nvx.py download --hypervisor whp
python scripts\nvx.py build
```

Do not download over artifacts the user wants to preserve. Verify the generated
command and required files without starting a VM:

```bash
backend=kvm # Use the resolved profile backend.
python3 scripts/nvx.py run --hypervisor "$backend" --dry-run
```

```powershell
python scripts\nvx.py run --hypervisor whp --dry-run
```

## Handoff

Keep the SSH session open and report:

- SSH host alias and remote repository path.
- Detected OS, selected backend, platform ID, and resolved host type.
- Commit and whether the checkout is dirty.
- Artifact source and completed preparation command.
- Dry-run result and any existing OpenVMM process.
- Any privilege or platform limitation.

Then load exactly one of
[nvx-run](../nvx-run/SKILL.md),
[nvx-debug](../nvx-debug/SKILL.md), or
[nvx-benchmark](../nvx-benchmark/SKILL.md).

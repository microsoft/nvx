---
name: nvx-run
description: "Run NVX interactively on a local or SSH-connected Linux/KVM, Linux/MSHV, or Windows/WHP host. Use for boot smoke tests, guest shell sessions, virtio-fs mounts, networking, kernel command-line options, and clean guest shutdown."
argument-hint: "[local|ssh-target] [repo] [kvm|mshv|whp] [run options]"
---

# Run NVX

Boot NVX in the appropriate local or remote execution context and verify guest
behavior.

## 1. Choose the Execution Context

Use the current local workspace when the user requests local execution or the local
host provides the required backend. Use SSH when the user names a remote host, the
required backend is unavailable locally, or the target machine itself matters.

For local execution:

- Run from the NVX repository root.
- Record the OS, `git rev-parse HEAD`, and `git status --short`.
- Run `python3 scripts/nvx.py verify` on Linux or
  `python scripts\nvx.py verify` on Windows.
- Select `kvm` or `mshv` on Linux and confirm the matching device is readable and
  writable. Select only `whp` on Windows.
- Preserve a dirty checkout and report it. Do not silently change revisions or
  replace artifacts.

For SSH execution, load and complete
[nvx-host-connect](../nvx-host-connect/SKILL.md), then reuse its persistent session,
repository directory, backend, revision, and artifact provenance. Keep standard
input attached to the guest; use `ssh -tt` if opening a direct interactive command.

Use `scripts/nvx.py run` rather than invoking OpenVMM directly. If artifacts are
missing, choose an explicit `download --hypervisor <backend>` for packaged artifacts
or the relevant build command for checkout changes. Do not overwrite artifacts the
user wants to preserve.

## 2. Assemble the Run

Start with an explicit backend and add only options required by the task:

```bash
backend=kvm # Use mshv when required.
python3 scripts/nvx.py run --hypervisor "$backend" --dry-run
```

```powershell
python scripts\nvx.py run --hypervisor whp --dry-run
```

Supported examples:

```bash
python3 scripts/nvx.py run --hypervisor kvm \
    --memory-mib 256 --net 10.0.0.2/24 --network-profile portable \
    --cmdline "quiet loglevel=0"
python3 scripts/nvx.py run --hypervisor mshv \
    --mount "/mnt/host,/absolute/host/share,rw"
```

```powershell
python scripts\nvx.py run --hypervisor whp `
    --mount "/mnt/host,C:\Users\me\microvm-share,rw"
```

For `--mount`, the guest target must be an absolute Linux path. Host paths containing
commas are unsupported. Use `ro` unless guest writes are required.

## 3. Check for Conflicts

Immediately before booting, check for an existing OpenVMM process in the selected
execution context:

```bash
pgrep -af openvmm || true
```

```powershell
Get-Process openvmm -ErrorAction SilentlyContinue
```

Do not start a competing VM unless the user confirms the existing process is
expected and the host has sufficient resources. Never terminate a process the
current session did not start.

## 4. Run Interactively

```bash
backend=kvm # Use mshv when required; append the approved options.
python3 scripts/nvx.py run --hypervisor "$backend"
```

```powershell
# Append the options approved from the dry run when needed.
python scripts\nvx.py run --hypervisor whp
```

Success requires the `ALPINE-MICROVM-BOOT-OK` marker and a responsive root shell.
Exercise the requested behavior, then exit cleanly inside the guest:

```sh
/sbin/nvx-exit 0
```

Record the host process exit status. If the guest hangs, interrupt only the command
started by the current terminal or SSH session and preserve its output. Do not issue
a host-wide `killall` or an unscoped `Stop-Process`.

## Report

Report whether execution was local or SSH, the host, backend, commit, dirty status,
artifact source, exact run command, boot marker result, guest checks, and host exit
status. For a failure, preserve the terminal output and continue with
[nvx-debug](../nvx-debug/SKILL.md).

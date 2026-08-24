---
name: nvx-debug
description: "Debug NVX on a local or SSH-connected Linux/KVM, Linux/MSHV, or Windows/WHP host. Use for boot failures, missing readiness markers, OpenVMM errors, benchmark acceptance failures, guest hangs, platform access errors, and detailed diagnostics."
argument-hint: "[local|ssh-target] [repo] [kvm|mshv|whp] <failure>"
---

# Debug NVX

Reproduce one NVX failure in the appropriate local or remote execution context while
preserving enough evidence to identify its controlling layer.

## 1. Choose the Execution Context

Reproduce on the host where the failure occurred unless the user explicitly wants
to test another host. Use the current workspace for local failures. When SSH is
required, load and complete [nvx-host-connect](../nvx-host-connect/SKILL.md), then
reuse its persistent session, repository directory, backend, revision, and artifact
provenance.

For local execution, run from the repository root and record the OS, Python version,
`git rev-parse HEAD`, `git status --short`, selected backend, and artifact source.
Run `scripts/nvx.py verify` with the platform's Python launcher. Confirm `/dev/kvm`
or `/dev/mshv` access on Linux; Windows supports only WHP. Preserve dirty checkouts
and do not repair host configuration implicitly.

## 2. Capture the Baseline

Before retrying, preserve:

- Local or SSH context, host, detected OS, backend, and platform ID.
- Exact commit, dirty status, and artifact source.
- Python version and `run --dry-run` output.
- Exact failing command, exit status, and complete terminal error.
- Whether an OpenVMM process remains after the failure.

Change one variable at a time. Never terminate an OpenVMM process the current
session did not start.

## 3. Trace an Interactive Run

Set OpenVMM logging in the selected execution shell and rerun the same command:

```bash
export OPENVMM_LOG=debug
backend=kvm # Use mshv when required; append the original options.
python3 scripts/nvx.py run --hypervisor "$backend"
```

```powershell
$env:OPENVMM_LOG = 'debug'
# Append the options from the original failing command when needed.
python scripts\nvx.py run --hypervisor whp
```

Use `trace` only when `debug` does not expose the failing component. Keep the guest
interactive; do not pipe the command through a logger that can detach standard
input. The terminal transcript is the primary debug record. Exit a responsive guest
with `/sbin/nvx-exit 0`.

Use `scripts/nvx.py run --dry-run` to inspect the generated OpenVMM command. Invoke
that printed command directly only when low-level debugging requires options the NVX
wrapper does not expose, and record every deviation.

## 4. Reduce a Benchmark Failure

Reduce a benchmark failure to a one-run acceptance case before modifying the
measured suite:

```bash
backend=kvm # Use mshv when required.
python3 scripts/nvx.py benchmark --suite e2e --backend "$backend" \
    --warmups 1 --runs 1 --skip-build --output data/runs/debug-e2e.json
```

```powershell
python scripts\nvx.py benchmark --suite e2e --backend whp `
    --warmups 1 --runs 1 --skip-build --output data\runs\debug-e2e.json
```

The benchmark coordinator intentionally suppresses most OpenVMM logging to protect
measurements. Missing detailed benchmark logs are not a logging failure. Reproduce
the boot through `run` when OpenVMM traces are needed.

## Stopping Conditions

| Failure | Action |
| --- | --- |
| SSH authentication or host-key failure | Stop and let the user repair trust or credentials. |
| Missing or inaccessible hypervisor device | Report the exact device check without elevating. |
| WHP unavailable | Report the smoke-run error; enabling WHP requires an administrator and reboot. |
| Dirty or wrong revision | Stop mutation; use it only with approval or prepare an isolated checkout. |
| Missing artifacts | Choose explicit `download` or `build` based on the requested provenance. |
| Boot marker timeout | Trace the same backend and command before widening scope. |
| SSH disconnect | Check for the original process and output before starting another run. |

Apply SSH-specific conditions only to remote execution.

## Report

Report the execution context, smallest reproduction, observed versus expected
behavior, relevant trace lines, the layer that appears to control the failure, and
the next discriminating check. Include all changed options and unresolved privilege
or transport limitations.

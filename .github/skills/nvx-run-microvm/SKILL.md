---
name: nvx-run-microvm
description: "Boot an NVX microVM with nvx-rs on Linux/KVM, Linux/MSHV, or Windows/WHP, exercise the guest, and debug failed boots. Use for boot smoke tests, guest shell sessions, virtio-fs mounts, networking, kernel command lines, missing boot markers, OpenVMM errors, guest hangs, and benchmark acceptance failures."
argument-hint: "[local|host-profile] [kvm|mshv|whp] [run options or failure]"
---

# Run And Debug An NVX MicroVM

Prepare the host with [nvx-prepare-host](../nvx-prepare-host/SKILL.md) first,
and reuse its host, checkout, backend, revision, and artifacts. Run the
commands below from that checkout in an interactive terminal, locally or in
`ssh -tt <ssh_target>` on a remote host. They read the same in bash and
PowerShell, except where noted; use `python` instead of `python3` on Windows.

## Run

1. Assemble the run with only the options that the task needs, and check it:

   ```console
   nvx-rs/target/release/nvx-rs run --hypervisor BACKEND --dry-run
   ```

   [Usage](../../../doc/usage.md#run) documents the options, such as
   `--memory-mib`, `--net 10.0.0.2/24 --network-profile portable`,
   `--cmdline "quiet loglevel=0"`, and `--mount "/mnt/host,HOST_PATH,ro"`, whose
   guest path is absolute and whose mode follows the last comma, so the host
   path may hold commas. Mount `ro` unless the guest must write.
2. Check with `python3 scripts/host_probe.py --no-verify` that no OpenVMM
   process runs. Start a competing VM only when the user confirms that the
   existing one is expected and that the host has room for both.
3. Run the same command without `--dry-run`, with standard input attached to
   the guest; never pipe it through a logger.
4. Success is the guest's `NVX-GUEST-BOOT-OK: <guest>` marker, such as
   `NVX-GUEST-BOOT-OK: alpine` for the default guest, and a responsive root
   shell. Exercise the request, exit with `/sbin/nvx-exit 0` in the guest, and
   record the host's exit status. When the guest hangs, interrupt only the
   command that this terminal started; never run a host-wide `killall` or an
   unscoped `Stop-Process`.

## Debug A Failure

1. Before retrying, keep the baseline: local or SSH execution, the host,
   backend, platform, commit, dirty state, artifact source, Python version, the
   `--dry-run` output, the exact command, its exit status and complete error,
   and any OpenVMM process left behind.
2. Change one variable at a time. Rerun the same command with OpenVMM logging,
   `export OPENVMM_LOG=debug` in bash or `$env:OPENVMM_LOG = 'debug'` in
   PowerShell, and use `trace` only when `debug` does not show the failing
   component. The terminal transcript is the record.
3. Run the OpenVMM command that `--dry-run` prints directly only for options
   that `nvx-rs` lacks, and record every deviation.
4. Reduce a benchmark acceptance failure to one run before changing the
   measured suite:
   `python3 scripts/nvx.py benchmark --suite e2e --backend BACKEND --warmups 1 --runs 1 --skip-build --output <fresh file>`.
   The benchmark suppresses most OpenVMM logging to protect its measurements,
   so reproduce the boot through `run` for traces.

| Failure | Action |
| --- | --- |
| SSH authentication or host key | Stop; the user repairs trust or credentials |
| Missing or inaccessible hypervisor device | Report the probe's check; do not elevate |
| WHP unavailable | Report it; enabling WHP needs an administrator and a reboot |
| Dirty or wrong revision | Stop changing it; use a temporary worktree or ask |
| Missing artifacts | `download` or build them, by the provenance that the task needs |
| Boot marker timeout | Trace the same backend and command before widening the scope |
| SSH disconnect | Look for the original process and its output before another run |

## Report

Report local or SSH execution, the host, backend, commit, dirty state, artifact
source, exact command, boot marker, guest checks, and the host's exit status.
For a failure, add the smallest reproduction, the observed against the expected
behavior, the relevant trace lines, the layer that appears to control the
failure, the next discriminating check, every changed option, and any privilege
or transport limitation.

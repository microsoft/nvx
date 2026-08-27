---
name: nvx-benchmark
description: "Benchmark NVX on a local or SSH-connected Linux/KVM, Linux/MSHV, or Windows/WHP host and collect fresh performance results. Use for acceptance runs, canonical performance suites, p50 collection, benchmark artifacts, and platform performance comparisons."
argument-hint: "[local|ssh-target] [repo] [kvm|mshv|whp] [suite/options]"
---

# Benchmark NVX

Run a reproducible NVX benchmark in the appropriate local or remote execution
context and retain only the fresh result set.

## 1. Choose the Execution Context

Use the current local workspace when it provides the target backend and the user
does not require another machine. Use SSH when the user names a remote host, the
backend is unavailable locally, or measurements must come from a particular host.

For local execution, run from the repository root and record the OS, commit, dirty
status, backend, artifact provenance, power mode, and host load. Run
`scripts/nvx.py verify` with the platform's Python launcher. Confirm `/dev/kvm` or
`/dev/mshv` access on Linux; Windows supports only WHP.

For SSH execution, load and complete
[nvx-host-connect](../nvx-host-connect/SKILL.md), then reuse its persistent session,
repository directory, backend, commit, dirty status, and artifact provenance.

Benchmarks require a release OpenVMM binary. Use a quiet, idle host, keep power mode
stable, and record all non-default options. Select one backend explicitly; never use
`both`. Do not run another workload concurrently and never terminate an OpenVMM
process the current session did not start.

## 2. Run Acceptance

Check for an existing OpenVMM process in the selected execution context, then run
one measured `e2e` acceptance case. Continue only if it succeeds.

```bash
backend=kvm # Use mshv when required.
python3 scripts/nvx.py benchmark --suite e2e --backend "$backend" \
    --warmups 1 --runs 1 --skip-build --output data/runs/benchmark-e2e.json
```

```powershell
python scripts\nvx.py benchmark --suite e2e --backend whp `
    --warmups 1 --runs 1 --skip-build --output data\runs\benchmark-e2e.json
```

If acceptance fails, preserve its output and load
[nvx-debug](../nvx-debug/SKILL.md). Do not collect performance data from a failing
host.

## 3. Create a Fresh Result Directory

Never reuse an output directory; stale workload logs can produce invalid collection
results. Use the branch matching the selected backend.

### Linux/KVM

```bash
commit="$(git rev-parse HEAD)"
run_id="$(date -u +%Y%m%dT%H%M%SZ)-$(git rev-parse --short HEAD)"
host_type=baremetal # Use the host type resolved by nvx-host-connect.
platform="linux-kvm-${host_type}"
output_dir="data/runs/manual/${run_id}-${platform}"
python3 scripts/nvx.py benchmark --suite performance --backend kvm \
    --runs 5 --virtfs-runs 3 --skip-build --output-dir "$output_dir"
python3 scripts/nvx.py performance collect --platform "$platform" \
    --commit "$commit" --input-dir "$output_dir" --output-dir "$output_dir/results" \
    --require-network --require-shell-snapshot --require-shared-suite \
    --summary "$output_dir/summary.md"
printf 'NVX_RESULTS=%s\n' "$output_dir"
```

The network workload uses the in-process portable profile and does not require
TAP creation or host firewall changes.

### Linux/MSHV

```bash
commit="$(git rev-parse HEAD)"
run_id="$(date -u +%Y%m%dT%H%M%SZ)-$(git rev-parse --short HEAD)"
host_type=baremetal # Use the host type resolved by nvx-host-connect.
platform="linux-mshv-${host_type}"
output_dir="data/runs/manual/${run_id}-${platform}"
python3 scripts/nvx.py benchmark --suite performance --backend mshv \
    --runs 5 --virtfs-runs 3 --skip-build --output-dir "$output_dir"
python3 scripts/nvx.py performance collect --platform "$platform" \
    --commit "$commit" --input-dir "$output_dir" --output-dir "$output_dir/results" \
    --require-network --require-shell-snapshot --require-shared-suite \
    --summary "$output_dir/summary.md"
printf 'NVX_RESULTS=%s\n' "$output_dir"
```

The network workload uses the in-process portable profile and does not require
TAP creation or host firewall changes.

### Windows/WHP

```powershell
$commit = (git rev-parse HEAD).Trim()
$runId = "$(Get-Date -AsUTC -Format 'yyyyMMddTHHmmssZ')-$((git rev-parse --short HEAD).Trim())"
$hostType = "baremetal" # Use the host type resolved by nvx-host-connect.
$platform = "windows-whp-$hostType"
$outputDir = "data\runs\manual\$runId-$platform"
python scripts\nvx.py benchmark --suite performance --backend whp `
    --runs 5 --virtfs-runs 3 --skip-build --output-dir $outputDir
python scripts\nvx.py performance collect --platform $platform `
    --commit $commit --input-dir $outputDir --output-dir "$outputDir\results" `
    --require-network --require-shell-snapshot --require-shared-suite `
    --summary "$outputDir\summary.md"
Write-Output "NVX_RESULTS=$outputDir"
```

Do not compare measurements from different machines or materially different host
configurations as though they were a regression pair.

## 4. Retain Results

For local execution, inspect the fresh `NVX_RESULTS` directory directly; do not copy
it elsewhere unless the user requests an export.

For SSH execution, retrieve only that fresh directory with the user's configured
`scp` or `sftp`, quoting both paths. For Windows OpenSSH, inspect the path with
`sftp` because drive-letter spelling depends on the server's SFTP root. Do not copy
the full checkout or credential files. Preserve the remote copy unless the user asks
for cleanup.

Verify that the retained result set contains raw workload logs, `summary.md`, and
collected CSV files.

## Report

Report whether execution was local or SSH, the host, host type, performance series,
platform ID, backend, commit, dirty status, artifact source, host conditions, exact
commands, exit status, benchmark summary, and result paths. For remote execution,
report both local and remote paths. Name every skipped metric, changed option,
privilege limitation, or interrupted run.

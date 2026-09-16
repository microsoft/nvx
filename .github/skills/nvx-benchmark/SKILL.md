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

Resolve the host type before constructing a platform ID. For SSH, use the exact
`host_type` returned by `nvx-host-connect`. For local execution, use the user-provided
host type or ask whether the host is `baremetal` or `virtual-machine`; do not infer it
from device or hypervisor probes. Set `NVX_HOST_TYPE` to that value in the execution
shell before using the commands below.

## 2. Select The Measurement Profile

Use one profile and report it by name:

- **Smoke** checks functionality with zero warmups and one retained run. Never persist,
  gate, or compare smoke output as performance evidence.
- **CI-equivalent** reproduces the exact counts, affinity, timeout, and teardown mode
  from [run-benchmark](../../actions/run-benchmark/action.yml) at the selected revision.
  Read that action at runtime; CI sampling can change and must not be copied from an
  older run or this skill.
- **Canonical manual** uses the complete performance workload profile documented in
  [benchmarks](../../../doc/benchmarks.md), followed by device I/O with five warmups,
  30 retained attempts, ten-second windows, and 512 MiB backing objects.

The commands below use the canonical manual profile. When the user requests CI parity,
substitute every sampling and host-control value from the selected revision's action
and record the substitutions. Compare revisions only with the same profile and host.

## 3. Run Acceptance In A Fresh Result Directory

Check for an existing OpenVMM process in the selected execution context, then run
the `e2e` acceptance sample set. Create the fresh result root first so acceptance and
performance evidence cannot be mixed with an earlier run. Continue only if acceptance
succeeds.

```bash
backend=kvm # Use mshv when required.
host_type="${NVX_HOST_TYPE:-}"
case "$host_type" in
    baremetal|virtual-machine) ;;
    *) printf 'NVX_HOST_TYPE must be baremetal or virtual-machine\n' >&2; exit 2 ;;
esac
platform="linux-${backend}-${host_type}"
commit="$(git rev-parse HEAD)"
run_id="$(date -u +%Y%m%dT%H%M%SZ)-$(git rev-parse --short HEAD)"
output_dir="data/runs/manual/${run_id}-${platform}"
test ! -e "$output_dir"
mkdir -p "$output_dir"
acceptance_output="$output_dir/benchmark-e2e.json"
python3 scripts/nvx.py benchmark --suite e2e --backend "$backend" \
    --platform "$platform" --processors 1 --warmups 1 --runs 10 \
    --skip-build --output "$acceptance_output"
```

```powershell
$hostType = $env:NVX_HOST_TYPE
if ($hostType -notin @('baremetal', 'virtual-machine')) {
    throw 'NVX_HOST_TYPE must be baremetal or virtual-machine'
}
$platform = "windows-whp-$hostType"
$commit = (git rev-parse HEAD).Trim()
$runId = "$(Get-Date -AsUTC -Format 'yyyyMMddTHHmmssZ')-$((git rev-parse --short HEAD).Trim())"
$outputDir = "data\runs\manual\$runId-$platform"
if (Test-Path -LiteralPath $outputDir) {
    throw "fresh result directory already exists: $outputDir"
}
New-Item -ItemType Directory -Path $outputDir | Out-Null
$acceptanceOutput = "$outputDir\benchmark-e2e.json"
python scripts\nvx.py benchmark --suite e2e --backend whp `
    --platform $platform --processors 1 --warmups 1 --runs 10 `
    --skip-build --output $acceptanceOutput
```

If acceptance fails, preserve its output and load
[nvx-debug](../nvx-debug/SKILL.md). Do not collect performance data from a failing
host.

## 4. Collect The Canonical Manual Profile

Never reuse an output directory; stale workload logs can produce invalid collection
results. Reuse the fresh variables established by the matching acceptance branch.

### Linux/KVM

```bash
performance_output_dir="$output_dir/performance"
python3 scripts/nvx.py benchmark --suite performance --backend kvm \
    --platform "$platform" --processors 1 --warmups 3 \
    --runs 5 --virtfs-runs 3 --skip-build --output-dir "$performance_output_dir"
python3 scripts/nvx.py performance collect --platform "$platform" \
    --commit "$commit" --input-dir "$performance_output_dir" \
    --output-dir "$performance_output_dir/results" \
    --require-network --require-shell-snapshot --require-shared-suite \
    --lifecycle-input "$acceptance_output" \
    --summary "$output_dir/summary.md"
device_output_dir="$output_dir/device-io"
python3 scripts/nvx.py benchmark --suite device-io --backend kvm \
    --platform "$platform" --processors 1 --warmups 5 --runs 30 \
    --device-io-duration-seconds 10 --device-io-size-mib 512 --skip-build \
    --output-dir "$device_output_dir"
device_results_dir="$device_output_dir/results"
python3 scripts/nvx.py performance collect --platform "$platform" \
    --commit "$commit" --input-dir "$device_output_dir" \
    --output-dir "$device_results_dir" --summary "$output_dir/summary.md"
python3 scripts/nvx.py performance persist --source-dir "$device_results_dir" \
    --history-dir "$performance_output_dir/results"
printf 'NVX_RESULTS=%s\n' "$output_dir"
```

The network workload uses the in-process portable profile and does not require
TAP creation or host firewall changes.

### Linux/MSHV

```bash
performance_output_dir="$output_dir/performance"
python3 scripts/nvx.py benchmark --suite performance --backend mshv \
    --platform "$platform" --processors 1 --warmups 3 \
    --runs 5 --virtfs-runs 3 --skip-build --output-dir "$performance_output_dir"
python3 scripts/nvx.py performance collect --platform "$platform" \
    --commit "$commit" --input-dir "$performance_output_dir" \
    --output-dir "$performance_output_dir/results" \
    --require-network --require-shell-snapshot --require-shared-suite \
    --lifecycle-input "$acceptance_output" \
    --summary "$output_dir/summary.md"
device_output_dir="$output_dir/device-io"
python3 scripts/nvx.py benchmark --suite device-io --backend mshv \
    --platform "$platform" --processors 1 --warmups 5 --runs 30 \
    --device-io-duration-seconds 10 --device-io-size-mib 512 --skip-build \
    --output-dir "$device_output_dir"
device_results_dir="$device_output_dir/results"
python3 scripts/nvx.py performance collect --platform "$platform" \
    --commit "$commit" --input-dir "$device_output_dir" \
    --output-dir "$device_results_dir" --summary "$output_dir/summary.md"
python3 scripts/nvx.py performance persist --source-dir "$device_results_dir" \
    --history-dir "$performance_output_dir/results"
printf 'NVX_RESULTS=%s\n' "$output_dir"
```

The network workload uses the in-process portable profile and does not require
TAP creation or host firewall changes.

### Windows/WHP

```powershell
$performanceOutputDir = "$outputDir\performance"
python scripts\nvx.py benchmark --suite performance --backend whp `
    --platform $platform --processors 1 --warmups 3 `
    --runs 5 --virtfs-runs 3 --skip-build --output-dir $performanceOutputDir
python scripts\nvx.py performance collect --platform $platform `
    --commit $commit --input-dir $performanceOutputDir `
    --output-dir "$performanceOutputDir\results" `
    --require-network --require-shell-snapshot --require-shared-suite `
    --lifecycle-input $acceptanceOutput `
    --summary "$outputDir\summary.md"
$deviceOutputDir = "$outputDir\device-io"
python scripts\nvx.py benchmark --suite device-io --backend whp `
    --platform $platform --processors 1 --warmups 5 --runs 30 `
    --device-io-duration-seconds 10 --device-io-size-mib 512 --skip-build `
    --output-dir $deviceOutputDir
$deviceResultsDir = "$deviceOutputDir\results"
python scripts\nvx.py performance collect --platform $platform `
    --commit $commit --input-dir $deviceOutputDir `
    --output-dir $deviceResultsDir --summary "$outputDir\summary.md"
python scripts\nvx.py performance persist --source-dir $deviceResultsDir `
    --history-dir "$performanceOutputDir\results"
Write-Output "NVX_RESULTS=$outputDir"
```

Do not compare measurements from different machines or materially different host
configurations as though they were a regression pair.

## 5. Retain Results

For local execution, inspect the fresh `NVX_RESULTS` directory directly; do not copy
it elsewhere unless the user requests an export.

For SSH execution, retrieve only that fresh directory with the user's configured
`scp` or `sftp`, quoting both paths. For Windows OpenSSH, inspect the path with
`sftp` because drive-letter spelling depends on the server's SFTP root. Do not copy
the full checkout or credential files. Preserve the remote copy unless the user asks
for cleanup.

Verify that the retained result set contains the acceptance JSON, raw logs under
`performance/`, `device-io/device-io.log`, `summary.md`, collected CSV files, and
`benchmark-metadata.json` records with the expected commit, platform, sampling counts,
and artifact hashes.

## Report

Report whether execution was local or SSH, the host, host type, measurement profile,
performance series, platform ID, backend, commit, dirty status, artifact source, host
conditions, warmups, retained runs, device window and size, exact commands, exit status,
benchmark summary, and result paths. For remote execution, report both local and remote
paths. Name every skipped metric, changed option, privilege limitation, or interrupted
run.

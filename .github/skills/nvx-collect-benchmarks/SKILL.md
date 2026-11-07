---
name: nvx-collect-benchmarks
description: "Collect fresh NVX benchmark results on a prepared Linux/KVM, Linux/MSHV, or Windows/WHP host with the smoke, CI-equivalent, or canonical profile. Use for acceptance runs, performance suites, p50 collection, benchmark artifacts, and same-host performance comparisons."
argument-hint: "[local|host-profile] [kvm|mshv|whp] [smoke|ci|canonical]"
---

# Collect NVX Benchmarks

Commands use `python3`; use `python` on Windows.

1. Prepare the host with [nvx-prepare-host](../nvx-prepare-host/SKILL.md), and
   reuse its checkout, backend, revision, artifacts, and `host_type`. For a
   local run, ask whether the host is `baremetal` or `virtual-machine` unless
   the user said so; never infer it from probes.
2. Choose one profile, and name it in the report:
   - `smoke` checks that the suites work; never persist, gate, or compare it.
   - `ci` reproduces CI's sampling, which the tool reads from the measured
     revision's [run-benchmark](../../actions/run-benchmark/action.yml) with
     PyYAML, which `scripts/setup/azure/requirements.txt` installs.
   - `canonical` is the manual baseline of
     [Benchmark](../../../doc/benchmarks.md#running-locally); add
     `--with-multi-vcpu` for CI's restore runs at 2, 4, and 8 vCPUs.

   Compare results only between runs of the same profile on the same host.
3. Measure on a quiet, idle host with a stable power mode and a release
   OpenVMM, with nothing else running. Run this checkout's coordinator against
   the prepared checkout, which may predate it, and which provides the
   `scripts/nvx.py` that measures. Locally:

   ```console
   python3 scripts/benchmark_series.py --repo CHECKOUT --profile PROFILE --backend BACKEND --host-type HOST_TYPE
   ```

   On a remote host, `nvx_hosts.py` runs it there over one SSH connection,
   whose loss ends the run:

   ```console
   python3 scripts/nvx_hosts.py benchmark HOST --cwd CHECKOUT -- --profile PROFILE --backend BACKEND --host-type HOST_TYPE
   ```

   `--dry-run` prints the plan. The tool refuses to start while an OpenVMM
   process runs; writes a fresh `data/runs/manual/<run>-<platform>` directory
   in the measured checkout; runs acceptance first and stops when it fails;
   validates each attempt and remeasures an unstable one once, as CI does;
   collects the p50 CSV files and `summary.md`; checks the result set and its
   host provenance; and prints `NVX_RESULTS=<directory>`.
4. When acceptance fails, keep its output and debug it with
   [nvx-run-microvm](../nvx-run-microvm/SKILL.md); never collect performance
   data from a failing host.
5. Inspect a local result directory in place. From a remote host, copy only
   that fresh directory with the user's `scp` or `sftp`, quoting both paths, and
   check the path's spelling with `sftp` on Windows OpenSSH, whose drive
   letters depend on the server's root. Never copy the checkout or credential
   files, and keep the remote copy unless the user asks to remove it.

## Report

Report local or SSH execution, the host, host type, profile, platform, backend,
commit, dirty state, artifact source, host conditions, the exact command and
its exit status, the summary's results, and the local and remote result paths.
Name every skipped metric, changed option, privilege limitation, and
interrupted run.

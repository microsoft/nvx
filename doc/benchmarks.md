# Benchmark

The supported OpenVMM benchmark coordinator provides acceptance and diagnostic suites plus a
23-metric non-Python workload suite on Linux/KVM, Linux/MSHV, and Windows/WHP. At one vCPU, CI
combines those metrics with eight 128 MiB shell lifecycle metrics and reports all 31 median (p50)
values for each host-typed performance series. At 2, 4, and 8 vCPUs, CI records only
`shell_snapshot_restore_512_mib`. Latency and resident-memory metrics are lower-is-better;
throughput metrics are higher-is-better.

The suite uses the base Alpine guest. Python application snapshots, the Python-agent console
workload, and its snapshot-prefetch experiment are intentionally excluded because the supported
guest build does not include a Python initramfs.

Human-readable timing summaries and lifecycle JSON report p50 and nearest-rank p95. The tracked
platform CSVs continue to persist and gate p50 so the historical schema remains unchanged.

Lifecycle results report peak resident set size (RSS) for the measured OpenVMM process. Linux
reads the process high-water mark; Windows reads the cumulative peak working set. RSS includes
resident guest-memory mappings. CI persists and gates p50 RSS and reports both p50 and maximum RSS
in its lifecycle diagnostics.

Use this page for metric names and methodology. Current historical p50 values live in
`data/`; timings copied into old discussions or commit messages are not baselines. Bare-metal and
virtual-machine results have separate histories and must not be compared as one regression series.
ABI version and processor count are also separate history dimensions. Legacy
CSV rows are interpreted as ABI v1 with one vCPU; they are never used as an
ABI-v2 one-vCPU baseline.
The OpenVMM benchmark coordinator is implemented in `scripts/nvx_tools/benchmark.py` and exposed
through the supported NVX CLI.

| CI performance series | Backend | Host type |
| --- | --- | --- |
| `linux-kvm-baremetal` | KVM | Bare metal |
| `linux-mshv-baremetal` | MSHV | Bare metal |
| `linux-mshv-virtual-machine` | MSHV | Virtual machine |
| `windows-whp-baremetal` | WHP | Bare metal |
| `windows-whp-virtual-machine` | WHP | Virtual machine |

CI runs the complete acceptance and performance suites at one vCPU under microVM ABI v2. It then
runs only the 512 MiB shell snapshot restore at `2`, `4`, and `8` vCPUs. This produces 34 p50
values per series and 170 values across the five-series matrix. Counts run sequentially on each
host so benchmark workloads never overlap on the same physical host.

## Running locally

Build the release VMM and guest artifacts first. See [Build](build.md).

Run the acceptance and diagnostic suites with:

```console
python3 scripts/nvx.py benchmark --suite boot --backend whp
python3 scripts/nvx.py benchmark --suite e2e --backend kvm --platform linux-kvm-baremetal --processors 8 --memory-mib 128 --output data/runs/linux-kvm-baremetal/microvm-v2/8vcpu/acceptance.json
```

Run the complete performance suite with:

```console
# Linux/KVM: run all 23 metrics and write collector-compatible logs
python3 scripts/nvx.py benchmark --suite performance --backend kvm --platform linux-kvm-baremetal --processors 8 --runs 5 --virtfs-runs 3 --skip-build --output-dir data/runs/linux-kvm-baremetal/microvm-v2/8vcpu

# Linux/MSHV: run all 23 metrics
python3 scripts/nvx.py benchmark --suite performance --backend mshv --platform linux-mshv-baremetal --processors 8 --runs 5 --virtfs-runs 3 --skip-build --output-dir data/runs/linux-mshv-baremetal/microvm-v2/8vcpu

# Windows/WHP
python scripts\nvx.py benchmark --suite performance --backend whp --platform windows-whp-baremetal --processors 8 --runs 5 --virtfs-runs 3 --skip-build --output-dir data\runs\windows-whp-baremetal\microvm-v2\8vcpu
```

Run the restore-only shape used by CI for higher-vCPU coverage with:

```console
python3 scripts/nvx.py benchmark --suite shell-snapshot-restore --backend kvm --platform linux-kvm-baremetal --processors 8 --shell-memories 512 --warmups 1 --runs 5 --skip-build --output-dir data/runs/linux-kvm-baremetal/microvm-v2/8vcpu
python3 scripts/nvx.py performance collect --platform linux-kvm-baremetal --commit HEAD --input-dir data/runs/linux-kvm-baremetal/microvm-v2/8vcpu --output-dir data/results --require-shell-snapshot-restore-512
```

Run one workload by selecting `cold-start`, `virtfs`, `shell-snapshot`, or `network-snapshot`
instead of `performance`. Use `--shell-memories 64 128 256 512`,
`--payload-mib 64`, and
`--net 10.0.0.2/24 --network-profile portable` to override their defaults. Run
`python scripts/nvx.py benchmark --help` for the complete option surface.

The network benchmarks select the same in-process portable data plane on KVM,
MSHV, and WHP. They do not create TAP devices or require host firewall rules.

Each workload directory includes `benchmark-metadata.json` with the platform,
backend, ABI, processor count, host affinity set, memory sizes, artifact
revisions, warmups, and measured run counts. Collection rejects mismatched
lifecycle/workload metadata and duplicate topology rows.

Use a fixed affinity set containing one logical processor per physical core
and at least `N+2` processors for an `N`-vCPU guest. The additional processors
cover VMM and device work. The default selector follows this policy; an
explicit undersized `--cpus` set is rejected before measurement.

The two pinned virtual-machine CI runners expose four cores as eight sibling
logical CPUs. Their virtual-machine series deliberately use the fixed `0-7`
set with `--host-cpu-reserve 0`, including sibling CPUs and sharing capacity
between guest, VMM, and device work. These constrained nested-host results are
kept separate from the bare-metal series; other runs retain the two-CPU
reserve. CI gives the constrained series a 40-second guest-marker deadline;
completed measurements still record their actual latency.

### MSHV lifecycle diagnostics

The Linux/MSHV backend emits an opt-in `MSHV_SET_GUEST_MEMORY completed` event from the existing
`mshv map user memory` span. The span identifies the guest range and permissions; the event records
`elapsed_us` and `success`. On x86_64, `MSHV_CREATE_VCPU completed` reports BSP creation after RAM
attachment. The backend-neutral `post-memory partition finalization completed` event includes BSP
creation and capability discovery. These scopes are nested and must not be summed. Enable
`virt_mshv=info,openvmm_core::worker::dispatch=info` only for diagnostic `run` invocations.
Benchmark measurements force OpenVMM logging off to avoid changing the measured path.

Collect a completed suite with:

```console
python3 scripts/nvx.py performance collect --platform linux-kvm-baremetal --commit HEAD --input-dir data/runs/linux-kvm-baremetal/microvm-v2/8vcpu --output-dir data/results --require-network --require-shell-snapshot --require-shared-suite --lifecycle-input data/runs/linux-kvm-baremetal/microvm-v2/8vcpu/acceptance.json
```

## Benchmark commands

| Benchmark | Command | Description |
| --- | --- | --- |
| Shell lifecycle | `benchmark --suite e2e` | Measures cold start, snapshot generation, snapshot restore, teardown, and peak RSS using a shell-ready guest. |
| All supported non-Python workloads | `benchmark --suite performance` | Runs 23 metrics and writes collector-compatible logs. |
| Cold start | `benchmark --suite cold-start` | Measures a quiet shell-ready baseline and isolated one-parameter kernel command-line variants. |
| Virtual file system | `benchmark --suite virtfs` | Measures live host-directory throughput and verifies host-to-guest plus guest-to-host visibility in one running VM. |
| Shell snapshot | `benchmark --suite shell-snapshot` | Compares cold boot with shell-ready snapshot restore at 64, 128, 256, and 512 MiB. |
| Shell snapshot restore | `benchmark --suite shell-snapshot-restore` | Captures an unmeasured shell-ready snapshot and measures only restore latency for the selected memory sizes. |
| Network snapshot | `benchmark --suite network-snapshot` | Compares a network-ready cold boot with snapshot restore and verifies gateway connectivity. |

## Kernel command lines

Each benchmark boots the guest with a fixed kernel command line. Two bases recur below:

- `BASE` = `earlycon=xe9 console=hvc0 reboot=t panic=-1`
- `QUIET` = `earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1`

OpenVMM owns `BASE`; each `--cmdline` value below is appended to it. Restore phases do not pass a
command line and resume the one captured in the snapshot. OpenVMM also appends device-discovery
tokens (`virtio_mmio.device=...` for an attached mount or NIC, plus `tsc_early_khz=...`) and may
replace `console=hvc0` with `console=hvc1` when a virtio console is selected.

| Benchmark | Phase | Kernel command line |
| --- | --- | --- |
| Shell lifecycle | cold/capture | `BASE` plus the combined `BASE_TUNING` parameters; CI uses 128 MiB. |
| Shell lifecycle | restore | restore (from the measured shell-ready snapshot) |
| Cold start | baseline | `QUIET` |
| Cold start | tuning variant | `QUIET` plus one of `clocksource=<backend>`, `tsc=reliable`, `no_timer_check`, `random.trust_cpu=on`, `rcupdate.rcu_expedited=1`, `nokaslr`, `mitigations=off`, or `cryptomgr.notests` |
| Virtual file system | guest runs | `QUIET` |
| Shell snapshot | cold | `QUIET` |
| Shell snapshot | capture | `QUIET shellsnap` |
| Shell snapshot | restore | restore (from snapshot) |
| Network snapshot | cold | `QUIET virtnet_probe=<gateway>` |
| Network snapshot | capture | `QUIET virtnet_probe=<gateway> netsnap` |
| Network snapshot | restore | restore (from snapshot) |

## Canonical metrics

### Shell lifecycle

CI runs one warmup and three measured samples with a 128 MiB guest. Each phase uses a fresh
OpenVMM process. The final measured snapshot is retained for the restore samples.

| Metric | Unit | Description |
| --- | --- | --- |
| `openvmm_cold_start` | ms | Immediately before OpenVMM process creation through `ALPINE-MICROVM-BOOT-OK`. |
| `openvmm_snapshot_generation` | ms | Immediately before dispatching guest `nvx-snapshot` through the first host observation of the atomically published snapshot directory. |
| `openvmm_snapshot_restore` | ms | Immediately before restored OpenVMM process creation through `OPENVMM-SNAPSHOT-RESTORE-OK`. |
| `openvmm_cold_start_guest_exit_teardown` | ms | Dispatch of guest `nvx-exit 0` after the cold-start marker through successful OpenVMM process exit. |
| `openvmm_snapshot_restore_guest_exit_teardown` | ms | Dispatch of guest `nvx-exit 0` after the restore marker through successful OpenVMM process exit. |
| `openvmm_cold_start_peak_rss` | MiB | Per-process peak RSS through the cold-start marker. |
| `openvmm_snapshot_generation_peak_rss` | MiB | Per-process peak RSS for the snapshot-generating process. |
| `openvmm_snapshot_restore_peak_rss` | MiB | Per-process peak RSS through the restore marker. |

Warmups are excluded from every aggregate. The CSV stores and gates p50 values. Lifecycle
diagnostics additionally report timing minimum, maximum, and sample count plus peak-RSS maximum.
Collection rejects host-termination semantics, missing samples, and any guest-exit teardown
timeout.
Lifecycle capture runs a deterministic affinity-pinned worker on every vCPU
before the snapshot request and again after restore continuation. The probe is
outside the snapshot-generation timing interval. Each worker completes only
after its CPU's LAPIC counter advances, avoiding fixed-duration guest sleeps.

### Cold start

These nine scenarios normally use five samples. All measure milliseconds from OpenVMM process
launch to the shell-ready console marker at the configured memory size. The tuning scenarios append
exactly one kernel parameter to the quiet baseline.

| Metric | Description |
| --- | --- |
| `cold_start_base` | Quiet baseline with no additional tuning parameter. |
| `cold_start_clocksource` | Baseline plus `clocksource=kvm-clock` on KVM or `clocksource=tsc` on MSHV/WHP. |
| `cold_start_tsc_reliable` | Baseline plus `tsc=reliable`. |
| `cold_start_no_timer_check` | Baseline plus `no_timer_check`. |
| `cold_start_random_trust_cpu` | Baseline plus `random.trust_cpu=on`. |
| `cold_start_rcu_expedited` | Baseline plus `rcupdate.rcu_expedited=1`. |
| `cold_start_nokaslr` | Baseline plus `nokaslr`. |
| `cold_start_mitigations_off` | Baseline plus `mitigations=off`. |
| `cold_start_cryptomgr_notests` | Baseline plus `cryptomgr.notests`. |

### Virtual file system

Throughput scenarios normally use three samples and a 64 MiB payload. The round-trip scenario
measures complete process wall time while host and guest exchange files through one running VM.

| Metric | Unit | Description |
| --- | --- | --- |
| `virtfs_live_write` | MB/s | Sequential guest `dd` write with `fsync` directly into the host directory. |
| `virtfs_live_read` | MB/s | Sequential guest read from the host file after dropping guest page cache. |
| `virtfs_live_roundtrip` | ms | Guest creates a marker observed by the host, then observes a host rewrite before that VM exits. |

### Shell snapshot

Each memory size normally uses five cold boots and five restores. Values are
milliseconds to the shell-ready `ALPINE-MICROVM-BOOT-OK` marker.

| Metric | Description |
| --- | --- |
| `shell_snapshot_cold_64_mib` | OpenVMM launch to a shell-ready guest with 64 MiB of memory. |
| `shell_snapshot_restore_64_mib` | Restore process launch to a shell-ready 64 MiB snapshot. |
| `shell_snapshot_cold_128_mib` | OpenVMM launch to a shell-ready guest with 128 MiB of memory. |
| `shell_snapshot_restore_128_mib` | Restore process launch to a shell-ready 128 MiB snapshot. |
| `shell_snapshot_cold_256_mib` | OpenVMM launch to a shell-ready guest with 256 MiB of memory. |
| `shell_snapshot_restore_256_mib` | Restore process launch to a shell-ready 256 MiB snapshot. |
| `shell_snapshot_cold_512_mib` | OpenVMM launch to a shell-ready guest with 512 MiB of memory. |
| `shell_snapshot_restore_512_mib` | Restore process launch to a shell-ready 512 MiB snapshot. |

### Network snapshot

These scenarios normally use five samples. A run is accepted only after the network verification
marker is observed. Cold boot and restore both time to one successful ICMP echo to the configured
gateway; the one-second timeout bounds a failed probe without adding an interval between
successful packets.

| Metric | Description |
| --- | --- |
| `network_snapshot_cold` | OpenVMM launch to the network-ready marker after interface configuration and a real connectivity probe. |
| `network_snapshot_restore` | Restore process launch to the verified restored-network marker. |
| `network_snapshot_restore_wall` | End-to-end host process wall time for restoring the snapshot, rebuilding the network backend, verifying connectivity, and exiting. |

## CI collection

`python scripts/nvx.py performance collect --require-shared-suite` rejects a workload result
unless it contains exactly the 23 shared metrics. Supplying `--lifecycle-input` requires and merges
the eight lifecycle metrics, producing the 31-metric one-vCPU result used by CI. Higher-vCPU
collection uses `--require-shell-snapshot-restore-512`, which requires exactly
`shell_snapshot_restore_512_mib` plus canonical one-warmup/five-sample metadata for a 2-, 4-, or
8-vCPU guest. Each backend job publishes its p50 tables and one-vCPU lifecycle diagnostics to
`$GITHUB_STEP_SUMMARY`. Pull-request regression checks compare KVM, MSHV, and WHP results with the
latest base-branch history.

The current workflow collects 10 measured lifecycle samples after one warmup.
The regression gate compares the target p50 with the median of the latest 10
p50 values on the pull request's base branch and requires all 10
matching history points. A metric regresses only when it is more than 50%
worse. Lower-is-better millisecond metrics must also be more than 10 ms
slower; higher-is-better metrics use the percentage comparison alone.
Missing or insufficient history is a warmup, not a failure. Successful `dev`
builds append collected results to topology-specific files in `data/`. Every
new ABI-v2/count series begins as a warmup baseline before its regression gate
has enough matching history.

## Lifecycle methodology

The lifecycle benchmark uses the optimized 128 MiB shell-ready guest as one baseline across KVM,
MSHV, and WHP. Host timing starts immediately before `Popen`, so cold-start and restore values
include OpenVMM process startup and VM construction. Snapshot-generation timing starts immediately
before the host writes `nvx-snapshot` to the guest shell and ends when the host first observes the
atomically published snapshot directory; publication is polled every 1 ms after dispatch.

After a cold-start marker, the host dispatches guest `nvx-exit 0` and measures
until the OpenVMM process exits successfully. Snapshot capture instead queues
the restore marker and `nvx-exit 0` together after the capture boundary.
Restore teardown therefore begins at the standalone marker line and does not
depend on a second host-to-guest poll of the interrupt-less `hvc0` console.
CI allows up to 15 seconds for process exit before rejecting a measured
sample. Snapshot-source exit after publication is retained in the raw JSON as
a diagnostic but is not the guest-exit teardown metric.

## Cold-start methodology

Cold-start timing begins immediately before launching the OpenVMM process and ends when the
shell-ready console substring appears. It therefore includes host process startup, VM construction,
and guest execution. Every scenario suppresses routine kernel logging and captures console output
without rendering it, so terminal rendering is not part of the measurement.

The `earlycon=xe9` and `hvc0` paths use one `outb` per byte. They replace a 16550-style path that
would normally require a line-status read plus a data write. The VMM scans each emitted byte
synchronously for markers. `OPENVMM_LOG=off` suppresses VMM logs, while `quiet loglevel=0`
suppresses most output in the guest.

The baseline command line is:

```text
earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1
```

Each other scenario appends only the parameter named in the metric table. This isolates that
parameter's effect; the benchmark does not combine the tunings or change guest memory between
scenarios. Security-reducing parameters such as `nokaslr` and `mitigations=off` are measured for
trusted, single-tenant deployments and are not general recommendations.

Major cold-start contributors identified while building the current kernel configuration were:

- `PM_TRACE_RTC` wall-clock probing, which can wait on absent or minimal RTC behavior;
- initialization and probing for hardware subsystems the machine does not expose;
- per-page kernel metadata initialization as guest RAM grows;
- console VM exits.

The checked-in kernel therefore omits unused PCI, storage, graphics, sound, power-management,
tracing, and debug stacks, keeps a minimal RTC implementation, and uses a low tick rate with
tickless idle. Treat these as design constraints when changing `kernel/config-microvm`; validate
both boot correctness and the relevant cold-start metrics.

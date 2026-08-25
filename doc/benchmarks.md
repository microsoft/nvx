# Benchmark

The supported OpenVMM benchmark coordinator provides acceptance and diagnostic suites plus the
23-metric non-Python performance suite used by CI on Linux/KVM, Linux/MSHV, and Windows/WHP. CI
reports the median (p50) for each metric in the job summary. Latency metrics are lower-is-better;
throughput metrics are higher-is-better.

The suite uses the base Alpine guest. Python application snapshots, the Python-agent console
workload, and its snapshot-prefetch experiment are intentionally excluded because the supported
guest build does not include a Python initramfs.

Raw benchmark logs also report peak resident set size (RSS) for the measured OpenVMM process.
Linux reads the process high-water mark; Windows reads the cumulative peak working set. RSS
includes resident guest-memory mappings and is diagnostic rather than regression-gated.

Use this page for metric names and methodology. Current historical p50 values live in
`data/`; timings copied into old discussions or commit messages are
not baselines. The OpenVMM benchmark coordinator is implemented in
`scripts/nvx_tools/benchmark.py` and exposed through the supported NVX CLI.

## Running locally

Build the release VMM and guest artifacts first. See [Build](build.md).

Run the acceptance and diagnostic suites with:

```console
python3 scripts/nvx.py benchmark --suite boot --backend whp
python3 scripts/nvx.py benchmark --suite e2e --backend kvm
```

Run the complete performance suite with:

```console
# Linux/KVM: run all 23 metrics and write collector-compatible logs
python3 scripts/nvx.py benchmark --suite performance --backend kvm --runs 5 --virtfs-runs 3 --skip-build --output-dir data/runs/linux-kvm

# Linux/MSHV: run all 23 metrics
python3 scripts/nvx.py benchmark --suite performance --backend mshv --runs 5 --virtfs-runs 3 --skip-build --output-dir data/runs/linux-mshv

# Windows/WHP
python scripts\nvx.py benchmark --suite performance --backend whp --runs 5 --virtfs-runs 3 --skip-build --output-dir data\runs\windows-whp
```

Run one workload by selecting `cold-start`, `virtfs`, `shell-snapshot`, or `network-snapshot`
instead of `performance`. Use `--shell-memories 64 128 256 512`,
`--payload-mib 64`, and `--net 10.0.0.2/24` to override their defaults. Run
`python scripts/nvx.py benchmark --help` for the complete option surface.

The KVM and MSHV network benchmarks require root or non-interactive `sudo ip` access so OpenVMM
can create a managed TAP. The host input policy must permit ICMP echo requests to the managed TAP
gateway; a default-drop firewall needs an explicit rule outside NVX. WHP uses its in-process
user-mode NAT backend.

Collect a completed suite with:

```console
python3 scripts/nvx.py performance collect --platform linux-kvm --commit HEAD --input-dir data/runs/linux-kvm --output-dir data/results --require-network --require-shell-snapshot --require-shared-suite
```

## Benchmark commands

| Benchmark | Command | Description |
| --- | --- | --- |
| All supported non-Python workloads | `benchmark --suite performance` | Runs 23 metrics and writes collector-compatible logs. |
| Cold start | `benchmark --suite cold-start` | Measures a quiet shell-ready baseline and isolated one-parameter kernel command-line variants. |
| Virtual file system | `benchmark --suite virtfs` | Measures live host-directory throughput and verifies host-to-guest plus guest-to-host visibility in one running VM. |
| Shell snapshot | `benchmark --suite shell-snapshot` | Compares cold boot with shell-ready snapshot restore at 64, 128, 256, and 512 MiB. |
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

`python scripts/nvx.py performance collect --require-shared-suite` rejects a backend result
unless it contains exactly the 23 shared metrics. Each backend job publishes its p50 table to
`$GITHUB_STEP_SUMMARY`. Pull-request regression checks compare KVM, MSHV, and WHP results with the
latest base-branch history.

The current workflow uses the latest 10 p50 samples on the pull request's base branch. A metric
regresses only when it is more than 50% worse. Lower-is-better millisecond metrics must also be
more than 10 ms slower; higher-is-better metrics use the percentage comparison alone. A missing
history is a warmup, not a failure. Successful `dev` builds append collected results to `data/`.

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

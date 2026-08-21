# Benchmark Reference

> **Archived NVX reference:** command examples below describe the original NVX
> VMM CLI and are retained to define workload semantics. They are not commands
> for this repository's supported launcher. Use `scripts/nvx.py benchmark` and
> `benchmarks/openvmm.py --help` to run the OpenVMM-native implementation.

NVX runs the same 27-metric benchmark suite on Linux/KVM and Windows/WHP. CI reports the median
(p50) for each metric in the job summary. Latency metrics are lower-is-better; throughput metrics
are higher-is-better.

The console output, virtual file system, network snapshot, and networked Python commands also
report the p50 peak resident set size (RSS) of the measured `nvx` process. Linux obtains the peak
from the child's `wait4` resource record; Windows samples the process's cumulative peak working
set. RSS includes resident guest-memory mappings and is currently a diagnostic rather than one of
the 27 regression-gated metrics.

Use this page for metric names and methodology. Current historical p50 values live in
`data/performance/`; timings copied into old discussions or commit messages are not baselines.

## Running locally

Build the release VMM and required guest artifacts first. See [Building NVX](build.md).

```console
# Linux
python3 scripts/nvx.py measure-coldstart --runs 5
python3 scripts/nvx.py bench-virtfs --runs 3
python3 scripts/nvx.py bench-snapshot-shell --runs 5 --memories "64 128 256 512"
python3 scripts/nvx.py bench-net-snapshot --runs 5
```

Use `python scripts\nvx.py ...` on Windows. KVM network benchmarks require permission to
configure a TAP; the WHP standalone network uses user-mode NAT.

Useful overrides include `KERNEL`, `INITRD`, `MEM`, `CORES`, `N`, `SNAP`, `NET`, and `PAYLOAD_MB`.
Each subcommand also accepts explicit options; run
`python scripts/nvx.py <command> --help` for the exact surface.

## Benchmark commands

| Benchmark | Command | Description |
| --- | --- | --- |
| Cold start | `measure-coldstart` | Measures a quiet shell-ready baseline and isolated one-parameter kernel command-line variants. |
| Virtual file system | `bench-virtfs` | Measures live host-directory throughput and verifies host-to-guest plus guest-to-host visibility in one running VM. |
| Shell snapshot | `bench-snapshot-shell` | Compares cold boot with shell-ready snapshot restore at 64, 128, 256, and 512 MiB. |
| Network snapshot | `bench-net-snapshot` | Compares a network-ready cold boot with snapshot restore and verifies gateway connectivity. |

## Kernel command lines

Each benchmark boots the guest with a fixed kernel command line. Two bases recur below:

- `BASE` = `earlycon=xe9 console=hvc0 reboot=t panic=-1`
- `QUIET` = `earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1`

Restore phases do not pass a command line; they resume the one captured in the snapshot. The VMM
also appends device-discovery tokens (`virtio_mmio.device=…` for an attached mount or NIC, plus
`tsc_early_khz=…`) and may replace `console=hvc0` with `console=hvc1` when a virtio console is
selected.

| Benchmark | Phase | Kernel command line |
| --- | --- | --- |
| Cold start | baseline | `QUIET` |
| Cold start | tuning variant | `QUIET` plus one of `clocksource=<backend>`, `tsc=reliable`, `no_timer_check`, `random.trust_cpu=on`, `rcupdate.rcu_expedited=1`, `nokaslr`, `mitigations=off`, or `cryptomgr.notests` |
| Virtual file system | guest runs | `console=hvc0 quiet loglevel=0 reboot=t panic=-1` (no `earlycon=xe9`) |
| Shell snapshot | cold | `QUIET` |
| Shell snapshot | capture | `QUIET shellsnap` |
| Shell snapshot | restore | restore (from snapshot) |
| Network snapshot | cold | `QUIET virtnet_probe=<gateway>` |
| Network snapshot | capture | `QUIET virtnet_probe=<gateway> netsnap` |
| Network snapshot | restore | restore (from snapshot) |

## Canonical metrics

### Console output

`bench-console-output` requires snapshots of the same agent image captured with
`--console=virtio` and `--console=portb`. The agent is only a deterministic workload generator;
its stdin/stdout/stderr are the ordinary inherited `/dev/console` descriptors. Every run sends the same bounded request,
writes position-dependent bytes through alternating stderr/stdout syscalls, and rejects any byte or
ordering mismatch on the merged terminal stream. The 64 KiB case uses one stdout syscall so Linux
and the virtqueue must split it into bounded descriptor batches. The reported latency spans process launch, snapshot restore, workload
output, and clean VM exit; effective throughput is payload bytes divided by that end-to-end time.
The default sizes are 1, 4, 16, and 64 KiB. To include an external NanVix IKC result in the same table, pass a command that
writes the expected concatenated stdout-then-stderr bytes to stdout, for example
`--nanvix-command "path/to/ikc-bench --bytes {bytes}"`.
When MXC release artifacts are available, `--nanvix-wxc path/to/wxc-exec.exe` runs equivalent
exact-size Python payloads through its NanVix MicroVM backend directly; the two NanVix options are
mutually exclusive.

The `portb` row measures the guest console implementation embedded in the supplied portb
snapshot. Current `main` uses one `outb` per byte because the KVM/OpenVMM stack does not preserve
wide PIO writes. To reproduce the historical four-byte baseline referenced by issue 29, capture
`--portb-snapshot` with the `outl`-enabled kernel from commit `a322df4` and run the same command.

### Cold start

These nine scenarios normally use five samples. All measure milliseconds from the first guest
instruction to the shell-ready console marker at the configured memory size. The tuning scenarios
append exactly one kernel parameter to the quiet baseline.

| Metric | Description |
| --- | --- |
| `cold_start_base` | Quiet baseline with no additional tuning parameter. |
| `cold_start_clocksource` | Baseline plus `clocksource=kvm-clock` on KVM or `clocksource=tsc` on WHP. |
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

### Python snapshots

These scenarios normally use five samples and a 512 MiB guest.

| Metric | Description |
| --- | --- |
| `python_pandas_cold` | Guest start through the generic trampoline, pandas/NumPy import, and DataFrame computation. |
| `python_pandas_restore` | Untrained trampoline resume through pandas/NumPy import and DataFrame computation. |
| `python_hello_cold` | Guest start through the generic trampoline to the pure `hello world` marker. |
| `python_hello_restore` | Untrained trampoline resume to the pure `hello world` marker. |

### Shell snapshot

Each memory size normally uses five cold boots and five restores. Values are
milliseconds to the shell-ready `ALPINE-MICROVM-BOOT-OK` marker.

| Metric | Description |
| --- | --- |
| `shell_snapshot_cold_64_mib` | Cold boot to a shell-ready guest with 64 MiB of memory. |
| `shell_snapshot_restore_64_mib` | Restore a shell-ready 64 MiB snapshot. |
| `shell_snapshot_cold_128_mib` | Cold boot to a shell-ready guest with 128 MiB of memory. |
| `shell_snapshot_restore_128_mib` | Restore a shell-ready 128 MiB snapshot. |
| `shell_snapshot_cold_256_mib` | Cold boot to a shell-ready guest with 256 MiB of memory. |
| `shell_snapshot_restore_256_mib` | Restore a shell-ready 256 MiB snapshot. |
| `shell_snapshot_cold_512_mib` | Cold boot to a shell-ready guest with 512 MiB of memory. |
| `shell_snapshot_restore_512_mib` | Restore a shell-ready 512 MiB snapshot. |

### Network snapshot

These scenarios normally use five samples. A run is accepted only after the
network verification marker is observed. Cold boot and restore both time to one
successful ICMP echo to the configured gateway; the one-second timeout bounds a
failed sample without adding an interval between successful packets.

| Metric | Description |
| --- | --- |
| `network_snapshot_cold` | Guest start to the network-ready marker after interface configuration and a real connectivity probe. |
| `network_snapshot_restore` | Guest resume to the verified restored-network marker. |
| `network_snapshot_restore_wall` | End-to-end host process wall time for restoring the snapshot, rebuilding the network backend, verifying connectivity, and exiting. |

## CI collection

`scripts/performance.py collect --require-shared-suite` rejects a backend result
unless it contains exactly the 27 shared metrics above. Each backend job publishes its
p50 table to `$GITHUB_STEP_SUMMARY`. Pull-request regression checks compare KVM
and WHP results with the latest base-branch history.

The current workflow uses the latest 10 p50 samples on the pull request's base branch. A metric
regresses only when it is more than 50% worse. Lower-is-better millisecond metrics must also be
more than 10 ms slower; higher-is-better metrics use the percentage comparison alone. A missing
history is a warmup, not a failure. Successful main builds append collected results to
`data/performance/`.

## Cold-start methodology

Cold-start timing begins at the first guest instruction and ends when the shell-ready console
substring appears. Every scenario suppresses routine kernel logging and discards console output
on the host so terminal rendering is not part of the measurement.

The `earlycon=xe9` and `hvc0` paths use one `outb` per byte. They replace a 16550-style path that
would normally require a line-status read plus a data write. The VMM scans each emitted byte
synchronously for markers. `--quiet` discards those bytes in the VMM, while `quiet loglevel=0`
suppresses most of them in the guest.

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

## WHP hot snapshot-page methodology

`bench-snapshot-prefetch` explicitly trains with prefetch disabled, then interleaves
`--snapshot-prefetch=off` and `auto` warm-cache restores to the same marker. For cold-cache
acceptance, alternate individual off/auto invocations and empty the Windows standby list before
each run from an administrator shell (for example, Sysinternals `RAMMap64.exe -Et`). Do not automate
cache eviction on shared hosts.

Collect at least five training restores and ten A/B samples. Retain the
`snapshot-hot-pages:` aggregate diagnostic, first-VP-to-marker `restore:` time, and external process
wall time. `auto` overlaps population with the guest, so the VMM metric includes contention but not
VMM setup; process wall time remains the end-to-end acceptance metric. The optimization target is
at least 30% lower cold first-entry-to-marker median (20% at p95), at least 20% lower cold
process-start-to-marker, no more than 5% warm total regression, and at least 80% fewer demand faults
for selected pages. See [Snapshots](design/snapshots.md#whp-hot-page-profiles) for the exact
generation commands, limits, and Windows requirements.

On KVM under WSL, place snapshot output on the native Linux filesystem. The VMM demand-pages
`mem.bin`, and a snapshot under `/mnt/c` measures DrvFs latency rather than the restore path. The
benchmark's temporary workload mount already uses the native temporary directory by default.

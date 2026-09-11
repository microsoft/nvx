# Benchmark

The supported OpenVMM benchmark coordinator provides acceptance and diagnostic suites, a
23-metric microVM non-Python workload suite, and five device operation-rate metrics on
Linux/KVM, Linux/MSHV, and Windows/WHP. At one vCPU, CI combines those workloads with eight
128 MiB shell lifecycle metrics and reports all 36 median (p50) values. At 2, 4, and 8 vCPUs, CI records only
`shell_snapshot_restore_512_mib`. Latency and resident-memory metrics are lower-is-better;
throughput and operation-rate metrics are higher-is-better.

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
New runs always emit ABI value 2 and remain stored under `microvm-v2` paths so
they cannot collide with legacy unsuffixed ABI-1 history.
The OpenVMM benchmark coordinator is implemented in `scripts/nvx_tools/benchmark.py` and exposed
through the supported NVX CLI. Guest payloads live in `scripts/nvx_tools/benchmark_scripts`; the
coordinator fills the `.sh.in` templates before use.

| CI performance series | Backend | Host type |
| --- | --- | --- |
| `linux-kvm-baremetal` | KVM | Bare metal |
| `linux-mshv-baremetal` | MSHV | Bare metal |
| `linux-mshv-virtual-machine` | MSHV | Virtual machine |
| `windows-whp-baremetal` | WHP | Bare metal |
| `windows-whp-virtual-machine` | WHP | Virtual machine |

CI runs the complete acceptance and performance suites and the five device metrics at one vCPU
under the canonical microVM, and only the 512 MiB shell snapshot restore at `2`,
`4`, and `8` vCPUs. This produces 39 p50 values per series and 195 values across the five-series
matrix. Counts run sequentially on each host so benchmark workloads never overlap on the same
physical host.

## Running locally

Build the release VMM and guest artifacts first. See [Build](build.md).

Run the acceptance and diagnostic suites with:

```console
python3 scripts/nvx.py benchmark --suite boot --backend whp
python3 scripts/nvx.py benchmark --suite e2e --backend kvm --platform linux-kvm-baremetal --processors 1 --memory-mib 128 --output data/runs/linux-kvm-baremetal/microvm-v2/1vcpu/acceptance.json
```

Run the complete performance suite with:

```console
# Linux/KVM: run all 23 metrics and write collector-compatible logs
python3 scripts/nvx.py benchmark --suite performance --backend kvm --platform linux-kvm-baremetal --processors 1 --runs 5 --virtfs-runs 3 --skip-build --output-dir data/runs/linux-kvm-baremetal/microvm-v2/1vcpu

# Linux/MSHV: run all 23 metrics
python3 scripts/nvx.py benchmark --suite performance --backend mshv --platform linux-mshv-baremetal --processors 1 --runs 5 --virtfs-runs 3 --skip-build --output-dir data/runs/linux-mshv-baremetal/microvm-v2/1vcpu

# Windows/WHP
python scripts\nvx.py benchmark --suite performance --backend whp --platform windows-whp-baremetal --processors 1 --runs 5 --virtfs-runs 3 --skip-build --output-dir data\runs\windows-whp-baremetal\microvm-v2\1vcpu
```

Run the canonical device operation-rate suite with its default five warmups, 30 retained attempts
per device, ten-second operation windows, and 512 MiB backing objects:

```console
# Linux/KVM; use --backend mshv and the matching platform on Linux/MSHV.
python3 scripts/nvx.py benchmark --suite device-io --backend kvm --platform linux-kvm-baremetal --processors 1 --skip-build --output-dir data/runs/linux-kvm-baremetal/microvm-v2/1vcpu/device-io
python3 scripts/nvx.py performance collect --platform linux-kvm-baremetal --commit HEAD --input-dir data/runs/linux-kvm-baremetal/microvm-v2/1vcpu/device-io --output-dir data/results

# Windows/WHP.
python scripts\nvx.py benchmark --suite device-io --backend whp --platform windows-whp-baremetal --processors 1 --skip-build --output-dir data\runs\windows-whp-baremetal\microvm-v2\1vcpu\device-io
python scripts\nvx.py performance collect --platform windows-whp-baremetal --commit HEAD --input-dir data\runs\windows-whp-baremetal\microvm-v2\1vcpu\device-io --output-dir data\results
```

For a smoke test, pass `--warmups 0 --runs 1 --device-io-duration-seconds 1`.

Run the restore-only shape used by CI for higher-vCPU coverage with:

```console
python3 scripts/nvx.py benchmark --suite shell-snapshot-restore --backend kvm --platform linux-kvm-baremetal --processors 8 --shell-memories 512 --warmups 1 --runs 5 --skip-build --output-dir data/runs/linux-kvm-baremetal/microvm-v2/8vcpu
python3 scripts/nvx.py performance collect --platform linux-kvm-baremetal --commit HEAD --input-dir data/runs/linux-kvm-baremetal/microvm-v2/8vcpu --output-dir data/results --require-shell-snapshot-restore-512
```

Measure restore-time vCPU activation from one immutable capacity-8 snapshot
that boots with only CPU 0 online:

```console
python3 scripts/nvx.py benchmark --suite snapshot-restore-vcpu --backend kvm --processors 8 --memory-mib 512 --warmups 1 --runs 5 --skip-build --output-dir data/runs/restore-vcpu-kvm
```

Use `--backend mshv` on Linux/MSHV or `--backend whp` on Windows/WHP. The suite
captures once with `maxcpus=1`, then restores that same artifact with online
targets 1, 2, 4, and 8. Each sample reaches its marker only after the guest has
verified the requested online prefix. Capture and restore use `--timeout`
(10 seconds by default). Results are diagnostic and are not fed into the
historical fixed-vCPU performance CSVs.

For an explicit MSHV target, OpenVMM instantiates and binds only the requested
VP prefix while retaining full-capacity topology and saved-state validation.
MSHV restores without a target, and all KVM and WHP restores, instantiate the
full capacity. A reduced-prefix MSHV runtime cannot be saved again.

Add `--snapshot-profile` to print p50 and p95 lifecycle phases for every online
target. The diagnostic retains its pre-banner capture so one immutable
boot-online-1 snapshot can serve every target. Its total restore latency is not
comparable with the lifecycle-aligned `shell-snapshot-restore` metric; compare
host-side lifecycle phases to separate VP binding and worker construction from
guest resume behavior:

```console
python3 scripts/nvx.py benchmark --suite snapshot-restore-vcpu --backend mshv --processors 8 --memory-mib 128 --warmups 1 --runs 5 --snapshot-profile --skip-build --output-dir data/runs/restore-vcpu-mshv-capacity-8
```

Measure restore-time memory activation from one immutable 512 MiB snapshot
with 2 GiB of ABI-reserved capacity:

```console
python3 scripts/nvx.py benchmark --suite snapshot-restore-memory --backend kvm --processors 1 --warmups 1 --runs 5 --snapshot-profile --skip-build --output-dir data/runs/restore-memory-kvm
```

Use `--backend mshv` on Linux/MSHV or `--backend whp` on Windows/WHP. The
suite restores the same base snapshot at 512 MiB, 1 GiB, and 2 GiB. It reports
guest-observed add-and-online latency separately from process-launch-to-ready
latency, OpenVMM peak RSS, and optional host lifecycle phases. Expansion
ranges are registered before restored execution; the guest marker is emitted
only after every 128 MiB memory block is online.

The explicit 512 MiB target is the zero-expansion control. Its private status
does not select the restore packet or request a snapshot boundary, so its
launch-to-ready latency should remain within measurement noise of restoring a
snapshot captured directly at 512 MiB.

Run the diagnostic snapshot lifecycle matrix with:

```console
# Linux/KVM or Linux/MSHV
python3 scripts/nvx.py benchmark --suite snapshot-profile --backend kvm --warmups 1 --runs 5 --output data/runs/linux-kvm-baremetal/snapshot-profile.json

# Windows/WHP
python scripts\nvx.py benchmark --suite snapshot-profile --backend whp --warmups 1 --runs 5 --output data\runs\windows-whp-baremetal\snapshot-profile.json
```

The default matrix profiles 64, 128, 256, 512, and 1024 MiB snapshots with both warm and cold
restore artifacts. Use `--shell-memories` to select sizes and `--cache-state warm`, `cold`, or
`both` to select cache conditions. The suite enables OpenVMM profiling for these diagnostic runs;
other paths leave full profiling disabled unless `--snapshot-profile` is explicit.
Snapshot capture always collects the OpenVMM clock records needed for its generation metric.

Run the non-canonical virtio restore diagnostic with a fresh output directory:

```console
python3 scripts/nvx.py benchmark --suite device-restore-profile --backend kvm --platform linux-kvm-baremetal --processors 1 --warmups 1 --runs 5 --skip-build --output-dir data/runs/device-restore-kvm
python scripts\nvx.py benchmark --suite device-restore-profile --backend whp --platform windows-whp-baremetal --processors 1 --warmups 1 --runs 5 --skip-build --output-dir data\runs\device-restore-whp
```

By default the suite runs console, network, and virtio-fs in both active and deferred modes. Use
`--restore-devices` and `--restore-modes` to select a subset. Deferred capture unbinds the selected
built-in guest driver before `nvx-snapshot`; restore rebinds it and verifies I/O. Devices are
located by the microVM ABI slots and virtio device IDs, not by unstable `virtioN` numbering:
network at `0xd0000000`/ID 1, virtio-fs at `0xd0001000`/ID 26, and console at
`0xd0002000`/ID 3. Probe control markers use port-B (`/dev/hvc0`) independently of the selected
device. Before rebinding, deferred mode sends a descriptor-free queue-0 MMIO notification through
`/sbin/nvx-mmio-write` to verify staged-kick ordering. Console verification writes through
`/dev/hvc1`; network uses the portable profile and a gateway ping; virtio-fs reads a host seed and
writes a host-visible result after remount.

The output directory contains `device-restore-profile.json`, `benchmark-metadata.json`, and raw
capture/restore logs. The JSON retains every measured sample, p50/p95 launch-to-ready and
trigger-to-first-I/O timing, peak RSS, guest markers, structured restore event order, queue-start
counts, staged-kick dispatch counts, and the required zero stale/premature callback assertion.
Only this suite enables the `virtio_restore=debug` event stream. Its result is diagnostic: it is
not accepted by `performance collect`, persisted to `data/*.csv`, or used by `performance gate`.
Host-observed marker timing includes serial delivery and scheduler delay, and repeated restores
reuse one fresh snapshot per scenario; compare results only on the same host under equivalent
load and power conditions.

Run one workload by selecting `cold-start`, `device-io`, `virtfs`, `shell-snapshot`, or `network-snapshot`
instead of `performance`. Use `--shell-memories 64 128 256 512`,
`--payload-mib 64`, and
`--net 10.0.0.2/24 --network-profile portable` to override their defaults. Run
`python scripts/nvx.py benchmark --help` for the complete option surface.

The network benchmarks select the same in-process portable data plane on KVM,
MSHV, and WHP. They do not create TAP devices or require host firewall rules.

Each workload directory includes `benchmark-metadata.json` with the platform, backend, ABI,
processor count, host affinity set, memory sizes, artifact revisions, repository dirty state,
warmups, measured run counts, and SHA-256 values for the VMM, kernel, initramfs, and coordinator.
Device metadata also records helper source and packaged-binary hashes. Collection rejects
mismatched lifecycle/workload metadata and duplicate topology rows.

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
| Device operation rates | `benchmark --suite device-io` | Measures five random storage and UDP round-trip operation rates with resumable raw attempts. |
| Cold start | `benchmark --suite cold-start` | Measures a quiet shell-ready baseline and isolated one-parameter kernel command-line variants. |
| Virtual file system | `benchmark --suite virtfs` | Measures live host-directory throughput and verifies host-to-guest plus guest-to-host visibility in one running VM. |
| Shell snapshot | `benchmark --suite shell-snapshot` | Compares cold boot with lifecycle-aligned snapshot restore at 64, 128, 256, and 512 MiB. |
| Shell snapshot restore | `benchmark --suite shell-snapshot-restore` | Captures an unmeasured lifecycle-aligned snapshot and measures only restore latency for the selected memory sizes. |
| Restore-time vCPU activation | `benchmark --suite snapshot-restore-vcpu --processors 8` | Restores one boot-online-1, capacity-8 snapshot at online targets 1/2/4/8 and reports latency plus peak RSS. |
| Network snapshot | `benchmark --suite network-snapshot` | Compares a network-ready cold boot with snapshot restore and verifies gateway connectivity. |
| Snapshot lifecycle profile | `benchmark --suite snapshot-profile` | Retains raw capture and restore phase records and summarizes 64/128/256/512/1024 MiB warm/cold restores. |
| Virtio device restore profile | `benchmark --suite device-restore-profile` | Verifies active and driver-unbound deferred restore for console, network, and virtio-fs; emits standalone diagnostic JSON and raw logs. |

## Kernel command lines

Each benchmark boots the guest with a fixed kernel command line. Two bases recur below:

- `BASE` = `earlycon=xe9 console=hvc0 reboot=t panic=-1`
- `QUIET` = `earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1`

OpenVMM owns `BASE`; each `--cmdline` value below is appended to it. Restore phases do not pass a
command line and resume the one captured in the snapshot. OpenVMM also appends device-discovery
tokens (`virtio_mmio.device=...` for an attached mount or NIC) and may replace `console=hvc0` with
`console=hvc1` when a virtio console is selected. Snapshot-source boots additionally receive the
backend-derived `tsc_early_khz=...` token. All cold microVM boots also receive
`lapic_timer_hz=...` when the backend reports its LAPIC frequency. The NVX kernel uses
that rate without calibrating or verifying the counting LAPIC against emulated PIT
interrupts, whose delivery can be delayed or coalesced by host scheduling.

| Benchmark | Phase | Kernel command line |
| --- | --- | --- |
| Shell lifecycle | cold/capture | `BASE` plus the combined `BASE_TUNING` parameters; CI uses 128 MiB. |
| Shell lifecycle | restore | restore (from the measured shell-ready snapshot) |
| Cold start | baseline | `QUIET` |
| Cold start | tuning variant | `QUIET` plus one of `clocksource=<backend>`, `tsc=reliable`, `no_timer_check`, `random.trust_cpu=on`, `rcupdate.rcu_expedited=1`, `nokaslr`, `mitigations=off`, or `cryptomgr.notests` |
| Virtual file system | guest runs | `QUIET` |
| Device operation rates | guest runs | `QUIET`; virtio-net uses the portable profile at `10.0.0.2/24` |
| Shell snapshot | cold | `QUIET` |
| Shell snapshot | capture | `QUIET`; host-driven after the boot marker and SMP/LAPIC probe |
| Shell snapshot | restore | restore through the post-restore SMP/LAPIC probe and lifecycle restore marker |
| Network snapshot | cold | `QUIET virtnet_probe=<gateway>` |
| Network snapshot | capture | `QUIET virtnet_probe=<gateway> netsnap` |
| Network snapshot | restore | restore (from snapshot) |

## Canonical metrics

### Shell lifecycle

CI runs one warmup and ten measured samples with a 128 MiB guest. Each phase uses a fresh
OpenVMM process. The final measured snapshot is retained for the restore samples.

| Metric | Unit | Description |
| --- | --- | --- |
| `openvmm_cold_start` | ms | Immediately before OpenVMM process creation through `ALPINE-MICROVM-BOOT-OK`. |
| `openvmm_snapshot_generation` | ms | OpenVMM process-clock interval from `capture.input_gate` start through `capture.publication_commit`; excludes host-to-guest console delivery and publication polling. |
| `openvmm_snapshot_restore` | ms | Immediately before restored OpenVMM process creation through `OPENVMM-SNAPSHOT-RESTORE-OK`. |
| `openvmm_cold_start_guest_exit_teardown` | ms | Dispatch of guest `nvx-exit 0` after the cold-start marker through successful OpenVMM process exit. |
| `openvmm_snapshot_restore_guest_exit_teardown` | ms | Host observation of the restore marker through successful OpenVMM process exit, with guest `nvx-exit 0` already queued in the capture controller. |
| `openvmm_cold_start_peak_rss` | MiB | Per-process peak RSS through the cold-start marker. |
| `openvmm_snapshot_generation_peak_rss` | MiB | Per-process peak RSS for the snapshot-generating process. |
| `openvmm_snapshot_restore_peak_rss` | MiB | Per-process peak RSS through the restore marker. |

Warmups are excluded from every aggregate. The CSV stores and gates p50 values. Lifecycle
diagnostics additionally report timing minimum, maximum, and sample count plus peak-RSS maximum.
Collection rejects host-termination semantics, legacy console-timed capture results, restore
results without prequeued guest exit, missing samples, and any guest-exit teardown timeout.
Lifecycle capture runs a deterministic affinity-pinned worker on every vCPU
before the snapshot request. Explicit correctness scenarios also stage a post-restore probe.
The capture probe is outside the snapshot-generation timing interval. Each worker proves that it
executed on its assigned vCPU and observes that CPU's local APIC counter advance. Workers poll
for at most 10,000 counter reads, so a stalled timer fails without relying on guest sleeps or a
working guest clock to bound the check. Pre/post interrupt snapshots also require every counter
to advance and remain at least as large as the worker's observed value.
The check remains strict even on a one-vCPU guest: falling back to the PIT after a failed
LAPIC calibration is not success. A counter frozen at 12 can indicate Linux's
`APIC timer disabled due to verification failure`; increasing the poll budget cannot repair it.
CI runs `test-microvm --scenario smp-lapic --processors 1 2 4 8` before acceptance.
This repeats the normal SMP probe with `lapic=notscdeadline`, covering the counting
LAPIC even on hosts that normally use TSC-deadline timers. The ordinary `smp` scenario
retains the default timer selection.
WHP capture waits for Linux to replace the transitional `tsc-early` clocksource with
its stable selected clocksource before starting this SMP validation.
The coordinator stages the probe and a capture controller in guest memory. The
controller runs the first probe, blocks in `read`, and invokes `nvx-snapshot` when the
host sends the trigger. The controller always emits `NVX-SNAPSHOT-DISPATCHED` immediately before
`nvx-snapshot`. On restore it completes any staged validation, prints the restore marker, and
executes the prequeued `nvx-exit 0` in guest-exit mode. Host-termination mode leaves the guest
running until the host terminates it.

The interrupt-less `hvc0` console polls even when the controller blocks in `read`. Its delivery
delay is retained in the non-gating `request_to_publication_*` JSON fields; a profiled capture
also separates `console_command_round_trip` and host-observed `guest_dispatch_to_publication`.
Neither observer interval is the generation metric. The canonical `samples_ms`/`p50_ms` and
profile phase `capture.snapshot_generation` use the same OpenVMM clock interval.

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

### Device operation rates

The dedicated suite always uses the canonical microVM and attaches the block
backing object as the writable `scratch` role. There is no ABI selector or
legacy unroled `--virtio-blk` control. Every guest has 256 MiB RAM and runs the same static, dependency-free
x86-64 helper from the reproducible initramfs. Canonical runs execute five warmups followed by 30
retained attempts for each device. Every operation window lasts ten seconds against a 512 MiB
backing object.

| Metric | Unit | Timed work |
| --- | --- | --- |
| `virtio_blk_random_read_iops` | ops/s | QD1 aligned 4 KiB random `pread64` with `O_DIRECT`. |
| `virtio_blk_random_write_iops` | ops/s | QD1 aligned 4 KiB random `pwrite64` with `O_DIRECT`. |
| `virtio_fs_random_read_iops` | ops/s | QD1 buffered 4 KiB random `pread64` after `sync` and a guest page-cache drop. |
| `virtio_fs_random_write_iops` | ops/s | QD1 buffered 4 KiB random `pwrite64`; the final flush starts after timing ends. |
| `virtio_net_udp_roundtrip_ops` | ops/s | Completed 64-byte UDP echo request/response transactions through portable virtio-net. |

Storage offsets use the same deterministic pseudo-random sequence in every backend. The host UDP
server binds the host's primary IPv4 address, requires no TAP or firewall changes, and stops before
the suite returns. The rate is derived during aggregation as
$\mathrm{ops/s}=N\times10^9/t_{ns}$ from integer operation counts and monotonic elapsed nanoseconds.

Backing objects are reused across attempts. On Windows, extending the virtio-fs file sets its
length without initializing its contents, so the first random writes can include host filesystem
initialization and zero-fill costs. A discarded warmup keeps those cold-file costs out of the
steady-state operation rates; a single cold attempt is only a smoke test.

`device-io.log` stores one versioned JSON record per warmup or retained attempt. Missing,
duplicate, malformed, or zero-work helper output turns that attempt into an explicit failure;
failed attempts stay in the log and never enter p50 or p95. Warmup and retained indices are fixed,
so a failure cannot shift sampling. Re-running with identical metadata skips completed identities
and resumes the first absent attempt. The selected ABI is part of metadata and
the default output directory, so v1 and v2 records cannot mix. Changed controls or provenance are rejected. Temporary raw
disks, shared directories, the UDP server, and each benchmark-owned VMM are cleaned on success,
failure, or interruption.

### Shell snapshot

Each memory size normally uses five cold boots and five restores. Cold values
run through the shell-ready `ALPINE-MICROVM-BOOT-OK` marker. The unmeasured
capture then runs the same SMP/LAPIC probe as the shell lifecycle benchmark
before requesting the snapshot. Restore values run through that probe and the
standalone `OPENVMM-SNAPSHOT-RESTORE-OK` marker. Capture and restore boundaries
therefore match the shell lifecycle benchmark; the kernel command lines remain
different so the cold metrics are not interchangeable.

This lifecycle-aligned methodology supersedes the earlier pre-banner
`shellsnap` capture. Historical `shell_snapshot_restore_*` values produced by
that methodology are not comparable with newly collected values.

| Metric | Description |
| --- | --- |
| `shell_snapshot_cold_64_mib` | OpenVMM launch to a shell-ready guest with 64 MiB of memory. |
| `shell_snapshot_restore_64_mib` | Restore process launch through lifecycle-aligned verification of a 64 MiB snapshot. |
| `shell_snapshot_cold_128_mib` | OpenVMM launch to a shell-ready guest with 128 MiB of memory. |
| `shell_snapshot_restore_128_mib` | Restore process launch through lifecycle-aligned verification of a 128 MiB snapshot. |
| `shell_snapshot_cold_256_mib` | OpenVMM launch to a shell-ready guest with 256 MiB of memory. |
| `shell_snapshot_restore_256_mib` | Restore process launch through lifecycle-aligned verification of a 256 MiB snapshot. |
| `shell_snapshot_cold_512_mib` | OpenVMM launch to a shell-ready guest with 512 MiB of memory. |
| `shell_snapshot_restore_512_mib` | Restore process launch through lifecycle-aligned verification of a 512 MiB snapshot. |

### Snapshot lifecycle profile

This diagnostic suite captures a fresh snapshot at each selected memory size, then restores that
artifact under each selected cache condition. A warm run sequentially reads `manifest.bin`,
`state.bin`, and `memory.bin` before every restore. Linux cold runs apply
`POSIX_FADV_DONTNEED` to each artifact. Windows cold runs restore from a fresh unbuffered
`robocopy /J` clone. The selected mechanism is recorded as `cache_control` alongside each result.

Snapshot capture always enables `OPENVMM_STARTUP_PROFILE=1` to measure generation on OpenVMM's
process-relative clock. Missing, duplicate, wrong-process, or non-monotonic capture boundaries
are errors, not a fallback to console timing. Full profile retention and host resource counters
remain opt-in through `--snapshot-profile` or the `snapshot-profile` suite. Other benchmark
subprocesses remove the variable unless profiling is requested. OpenVMM emits one ASCII record per phase
with the versioned `OPENVMM_SNAPSHOT_PROFILE_V1` prefix. Each record contains:

- `operation`, `phase`, and `exclusive`, where exclusive phases are disjoint intervals and
	non-exclusive phases are cumulative milestones that may contain nested work;
- monotonic `duration_ns` and process-relative `process_elapsed_ns`, plus the emitting `pid`;
- phase-specific `logical_bytes`, `allocated_bytes`, `gpa_faults`, and `populated_bytes` when
	available.

With full profiling, the coordinator retains every record in `profile.raw_samples`. It derives
`capture.snapshot_generation` from the OpenVMM clock and adds observer-defined
`process_startup`, `console_input_dispatch`, `console_command_round_trip`,
`guest_dispatch_to_publication`, `request_to_publication`, `source_teardown`,
`resume_to_readiness`, and `process_launch_to_readiness` boundaries without mixing them into
OpenVMM-exclusive intervals. `console_input_dispatch` measures the synchronous write of the
snapshot command and prequeued restore script to the OpenVMM console and records the payload size.
All captures emit a guest marker immediately before `nvx-snapshot`;
`console_command_round_trip` ends when the host observes that marker, and
`guest_dispatch_to_publication` spans that observation through snapshot publication.
Each observed record also includes available process counters. Linux reports RSS, peak RSS,
minor and major faults, total page faults, and, when `smaps_rollup` is available, private dirty
and private RSS bytes. Windows reports working set, peak working set, private commit, and page
faults.

`profile.phases` groups samples by `operation.phase` and stores raw `samples_ms` plus p50,
nearest-rank p95, minimum, and maximum. The top-level JSON path is
`snapshot_profile_matrix.<backend>.<memory_mib>`. Each memory entry contains the capture result and
`restore.warm` and/or `restore.cold`, including its cache control and full restore result. Warmups
are excluded from raw samples and summaries.

Capture records isolate guest quiesce, state save, mapped-memory and memory-handle flushes, each
publication step, publication observation, and source teardown. Restore records isolate artifact
open and preparation, COW section and mapping/view creation, prototype and final partition work,
GPA registration, partition-unit creation, VP-thread binding, saved-state restore, state-unit time
advance, per-VP TSC advance, backend clock advance, restored-VP stopping, input gating, device
start, generation-ID creation, the cumulative gated guest-repair interval, and guest resume to
readiness. State and memory SHA stages are intentionally absent from the final capture and restore
path.

`startup.vp_thread_bind` is the exclusive wall interval for all VP threads to bind. Nested
`startup.vp_bind_bsp` and `startup.vp_bind_ap_<INDEX>` records are non-exclusive per-VP intervals;
compare their endpoints with the aggregate interval to expose serialized backend VP creation.

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
the eight lifecycle metrics, producing a 31-metric one-vCPU result. CI collects the five device
operation-rate metrics from their separate raw-log directory and merges them by ABI, processor
count, commit, and metric, producing the final 36-metric ABI-2 one-vCPU result. Higher-vCPU
collection uses `--require-shell-snapshot-restore-512`, which requires exactly
`shell_snapshot_restore_512_mib` plus canonical one-warmup/five-sample metadata for a 2-, 4-, or
8-vCPU guest. A `device-io` directory is recognized from metadata and must contain exactly its five
metrics and every configured attempt. Each backend job publishes its p50 tables and one-vCPU lifecycle diagnostics to
`$GITHUB_STEP_SUMMARY`. Pull-request regression checks compare KVM, MSHV, and WHP results with the
latest base-branch history.

Linux CI uses a reduced device contract of zero warmups and one retained attempt. Windows CI
discards one warmup and retains five attempts so file initialization cannot dominate its p50.
Both CI contracts use one-second operation windows. Canonical baseline collection uses the
full `5 + 30` contract.

The current workflow collects 10 measured lifecycle samples after one warmup.
The regression gate compares the target p50 with the median of the latest 10
p50 values on the pull request's base branch and requires all 10
matching history points. A metric regresses only when it is more than 50%
worse. Lower-is-better millisecond metrics must also be more than 10 ms
slower; higher-is-better metrics use the percentage comparison alone.
Missing or insufficient history is a warmup, not a failure. Successful `dev`
builds append collected results to topology-specific files in `data/`. Every
new ABI/count series begins as a warmup baseline before its regression gate
has enough matching history.

The correction for issue #248 resets only `openvmm_snapshot_generation` and
`openvmm_snapshot_restore_guest_exit_teardown` histories across all five host series,
including their legacy ABI-1 rows. Those old console-contaminated measurements are removed,
not compared with the new timing boundaries. All other history rows and gate thresholds are
unchanged. CI passes `--history-reset-dir data` to honor metrics explicitly removed from a
tracked candidate history file even before the reset merges into the base branch. After merge,
the affected metrics start the normal ten-point warmup again; a missing candidate file alone
does not reset a base-branch history.

The Windows device-sampling correction resets only `virtio_fs_random_read_iops` and
`virtio_fs_random_write_iops` in the ABI-2 one-vCPU bare-metal and virtual-machine histories.
The old cold-file rates are not comparable with warmed-file measurements. The same selective
reset mechanism starts a new ten-point history for these metrics; other metrics and all gate
thresholds remain unchanged.

## Lifecycle methodology

The lifecycle benchmark uses the optimized 128 MiB shell-ready guest as one baseline across KVM,
MSHV, and WHP. Host timing starts immediately before `Popen`, so cold-start and restore values
include OpenVMM process startup and VM construction. Snapshot generation uses OpenVMM's monotonic
process clock from the beginning of input gating, after the guest requests the snapshot, through
atomic publication. It does not include delivery of `nvx-snapshot` over the polling console.
Host request-to-publication and first publication observation (polled every 1 ms) remain diagnostics.

After a cold-start marker, the host dispatches guest `nvx-exit 0` and measures
until the OpenVMM process exits successfully; cold-start teardown still includes console
delivery. Snapshot capture instead queues
the restore marker and `nvx-exit 0` together after the capture boundary.
Restore teardown therefore begins at the standalone marker line and does not
depend on a second host-to-guest poll of the interrupt-less `hvc0` console.
It is a host-observed marker-to-exit interval, including remaining guest shutdown and observer
scheduling, not a measurement of VMM-internal teardown alone.
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

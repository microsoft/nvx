# Benchmark

The supported OpenVMM benchmark coordinator provides acceptance and diagnostic suites, a
21-metric microVM non-Python workload suite, and five device operation-rate metrics on
Linux/KVM, Linux/MSHV, and Windows/WHP. At one vCPU, CI combines those workloads with eight
128 MiB shell lifecycle metrics and reports all 34 median (p50) values. At 2, 4, and 8 vCPUs, CI records only
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

The coordinator samples peak RSS once, at the readiness marker, while OpenVMM is still running.
A prequeued guest exit can end OpenVMM before that sample. The coordinator then discards and
repeats the attempt, up to three attempts per measured sample, and reports the number of discarded
attempts in `peak_rss_remeasured_count`. It never substitutes another reading: a sample taken
before the marker omits the restore, Linux exit accounting includes the coordinator's pre-exec
image, and a Windows reading after exit includes teardown.

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
| `linux-kvm-virtual-machine` | KVM | Virtual machine |
| `linux-mshv-virtual-machine` | MSHV | Virtual machine |
| `windows-whp-virtual-machine` | WHP | Virtual machine |

The series names carry no CPU vendor: every CI microVM runner is an Azure virtual machine with an
Intel Xeon CPU (Ice Lake-SP or Emerald Rapids), so each series has one vendor's samples. No CI
microVM runner has an AMD CPU, so no AMD series exists yet. An AMD runner needs a built-in AMD
[CPU profile](usage.md#cpu-profiles), because CI never uses host profiles and `benchmark` refuses a
CPU that no built-in profile serves, and only Milan, Genoa,
and Turin CPUs have one so far. Such a runner boots its guests on that profile through AMD-V, so its
timings would skew an Intel series' baseline. [#410](https://github.com/microsoft/nvx/issues/410)
tracks AMD runners. Before an AMD runner joins CI, give it series of its
own, named with an `-amd` suffix such as `linux-kvm-virtual-machine-amd`, in `PLATFORM_NAMES` and
`OPENVMM_BACKENDS` in `scripts/nvx_tools/performance.py` and in the CI matrix, so their history
lives in separate `data/` files. Local measurements on AMD hosts can use the names above, because
only CI records history.

CI runs the complete acceptance and performance suites and the five device metrics at one vCPU
under the canonical microVM, and only the 512 MiB shell snapshot restore at `2`,
`4`, and `8` vCPUs. This produces 37 p50 values per series, 34 at one vCPU and one at each higher
count, and 111 values across the three-series matrix. Counts run sequentially on each host so
benchmark workloads never overlap on the same physical host.

## Running locally

Build the release VMM and guest artifacts first. See [Build](build.md).

Run the acceptance and diagnostic suites with:

```console
python3 scripts/nvx.py benchmark --suite boot --backend whp
python3 scripts/nvx.py benchmark --suite e2e --backend kvm --platform linux-kvm-baremetal --processors 1 --memory-mib 128 --output data/runs/linux-kvm-baremetal/microvm-v2/1vcpu/acceptance.json
```

Run the complete performance suite with:

```console
# Linux/KVM: run all 21 metrics and write collector-compatible logs
python3 scripts/nvx.py benchmark --suite performance --backend kvm --platform linux-kvm-baremetal --processors 1 --runs 5 --virtfs-runs 3 --skip-build --output-dir data/runs/linux-kvm-baremetal/microvm-v2/1vcpu

# Linux/MSHV: run all 21 metrics
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

Measure the same activation from one platform snapshot, the clone tier that
a fleet restores with a different size each time:

```console
python3 scripts/nvx.py benchmark --suite platform-snapshot-restore-vcpu --backend kvm --processors 8 --memory-mib 512 --warmups 1 --runs 5 --skip-build --output-dir data/runs/platform-restore-vcpu-kvm
```

A platform command line carries no tenant configuration, so the suite cannot
use the pre-banner capture: the capture boots with `maxcpus=1` as its only
command-line option, a distro layer, and a scratch disk, and the guest captures
from its shell with `--snapshot-tier platform`. Each restore reattaches the
blocks and reaches its marker after the guest finishes the platform repair,
which brings the target prefix online, refreshes the clone's identity, and
acknowledges the restore gate. Its latency is therefore comparable between
targets and backends, not with `snapshot-restore-vcpu`. `--snapshot-profile`
adds the lifecycle phases of each target, and the results stay out of the
performance CSVs.

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

The default matrix profiles 128, 256, 512, and 1024 MiB snapshots with both warm and cold
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
instead of `performance`. Use `--shell-memories 128 256 512`,
`--payload-mib 64`, and
`--net 10.0.0.2/24 --network-profile portable` to override their defaults. Run
`python scripts/nvx.py benchmark --help` for the complete option surface.

The network benchmarks select the same in-process portable data plane on KVM,
MSHV, and WHP. They do not create TAP devices or require host firewall rules.

Each workload directory includes `benchmark-metadata.json` with the platform, backend, ABI,
processor count, host affinity set, memory sizes, artifact revisions, repository dirty state,
warmups, measured run counts, and SHA-256 values for the VMM, kernel, initramfs, and coordinator.
Device metadata also records the aggregate Rust guest-source digest, pinned
toolchain and target, and the packaged `nvx-device-io` binary hash from the
initramfs manifest. Collection rejects mismatched lifecycle/workload metadata
and duplicate topology rows.

Use a fixed affinity set containing one logical processor per physical core
and at least `N+2` processors for an `N`-vCPU guest. The additional processors
cover VMM and device work. The default selector follows this policy; an
explicit undersized `--cpus` set is rejected before measurement.

The virtual-machine CI runners expose four cores as eight sibling logical
CPUs. Their virtual-machine series deliberately use the fixed `0-7` set with
`--host-cpu-reserve 0`, including sibling CPUs and sharing capacity between
guest, VMM, and device work. These constrained nested-host results are kept
separate from the bare-metal series; other runs retain the two-CPU reserve. CI
gives the constrained series a 40-second guest-marker deadline; completed
measurements still record their actual latency.

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
| All supported non-Python workloads | `benchmark --suite performance` | Runs 21 metrics and writes collector-compatible logs. |
| Device operation rates | `benchmark --suite device-io` | Measures five random storage and UDP round-trip operation rates with resumable raw attempts. |
| Cold start | `benchmark --suite cold-start` | Measures a quiet shell-ready baseline and isolated one-parameter kernel command-line variants. |
| Virtual file system | `benchmark --suite virtfs` | Measures live host-directory throughput and verifies host-to-guest plus guest-to-host visibility in one running VM. |
| Shell snapshot | `benchmark --suite shell-snapshot` | Compares cold boot with lifecycle-aligned snapshot restore at 128, 256, and 512 MiB. |
| Shell snapshot restore | `benchmark --suite shell-snapshot-restore` | Captures an unmeasured lifecycle-aligned snapshot and measures only restore latency for the selected memory sizes. |
| Restore-time vCPU activation | `benchmark --suite snapshot-restore-vcpu --processors 8` | Restores one boot-online-1, capacity-8 snapshot at online targets 1/2/4/8 and reports latency plus peak RSS. |
| Platform restore-time vCPU activation | `benchmark --suite platform-snapshot-restore-vcpu --processors 8` | Restores one boot-online-1, capacity-8 platform snapshot at online targets 1/2/4/8, through the platform repair, and reports latency plus peak RSS. |
| Network snapshot | `benchmark --suite network-snapshot` | Compares a network-ready cold boot with snapshot restore and verifies gateway connectivity. |
| Snapshot lifecycle profile | `benchmark --suite snapshot-profile` | Retains raw capture and restore phase records and summarizes 128/256/512/1024 MiB warm/cold restores. |
| Virtio device restore profile | `benchmark --suite device-restore-profile` | Verifies active and driver-unbound deferred restore for console, network, and virtio-fs; emits standalone diagnostic JSON and raw logs. |

## Kernel command lines

Each benchmark boots the guest with a fixed kernel command line. Two bases recur below:

- `BASE` = `earlycon=xe9 console=hvc0 reboot=t panic=-1`
- `QUIET` = `earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1`

OpenVMM owns `BASE`; each `--cmdline` value below is appended to it. Restore phases do not pass a
command line and resume the one captured in the snapshot. OpenVMM also appends device-discovery
tokens (`virtio_mmio.device=...` for an attached mount or NIC) and may replace `console=hvc0` with
`console=hvc1` when a virtio console is selected. It appends no clock token: under the
[time ABI](design/3.2-clock-and-time.md#324-rates) the NVX kernel reads the TSC and LAPIC timer rates from the
hypervisor identity's frequency MSRs and never calibrates either clock, and OpenVMM rejects a
supplied `tsc_early_khz=` or `lapic_timer_hz=` token (`E_CMDLINE_CLOCK_TOKEN`). No benchmark
command line tunes the clock or clocksource, except the `cold_start_clocksource` variant below.

| Benchmark | Phase | Kernel command line |
| --- | --- | --- |
| Shell lifecycle | cold/capture | `BASE` plus `BASE_TUNING`: `random.trust_cpu=on rcupdate.rcu_expedited=1 nokaslr mitigations=off cryptomgr.notests quiet loglevel=0`; CI uses 128 MiB. |
| Shell lifecycle | restore | restore (from the measured shell-ready snapshot) |
| Cold start | baseline | `QUIET` |
| Cold start | tuning variant | `QUIET` plus one of `clocksource=tsc`, `tsc=reliable`, `no_timer_check`, `random.trust_cpu=on`, `rcupdate.rcu_expedited=1`, `nokaslr`, `mitigations=off`, or `cryptomgr.notests` |
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

Warmups are excluded from every aggregate. The CSV stores and gates p50 values. CI retains the
per-phase lifecycle profile, and diagnostics additionally report timing minimum, maximum, and
sample count plus peak-RSS maximum. Collection rejects host-termination semantics, legacy
console-timed capture results, restore results without prequeued guest exit, missing samples, and
any guest-exit teardown timeout. With at least ten samples, it also rejects snapshot-generation
series whose p50 is more than 25% above p25; this prevents a transient host stall from entering
performance history without hiding a uniformly slower product result.
Every launch is scanned for the guest's time ABI output as in the
[microVM correctness jobs](ci.md): a violation event or a time ABI power-off fails the run.
Each teardown that can return a sample (a guest exit with status 0, a host termination, or a
teardown timeout) first reads the console to its end, so an event that the guest prints after
the readiness marker, even on an unterminated last line, still fails the run. A host-terminated
launch, or one killed after a teardown timeout, fails on a time ABI power-off status but not on
the status of its termination. A guest exit with a nonzero status fails the run regardless; the
scan reads what the console delivers within a second, so that a time ABI power-off is reported
with its event.
When the harness kills OpenVMM, after a timeout or a failure, it waits up to 30 s for the
process to exit. A killed process exits only once the I/O that its threads wait for completes,
so a stalled disk can hold it, while a hung microVM otherwise exits within tens of
milliseconds: an exit that takes longer than a second prints a warning with its time, and a
process that still runs after 30 s fails the run's cleanup. Such a process can also keep its
output reader from stopping, so every later cleanup step still runs, and the error that led to
the kill keeps its message and OpenVMM output with each cleanup failure appended under
`--- cleanup failures ---`. A process that the harness gave up on is not killed again.
Benchmarks never ask the guest for its time ABI status, as the correctness jobs do after cold
boots and restores, so the guest prints nothing extra. Up to the marker, the scan only parses
output the harness reads anyway, and the harness reads the rest of the console only after it
records a sample's intervals, so the scan adds nothing to them. The guest's boot check, which
powers the guest off with status 193 if it fails, is part of `openvmm_cold_start`.
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
The time ABI hides the TSC-deadline timer on every backend, so every guest uses the one-shot
counting LAPIC, and the `smp` scenario that the microVM correctness jobs run at 1, 2, 4, and 8
vCPUs covers it. The benchmarks run no LAPIC gate of their own (#286), and no guest boots with
`lapic=notscdeadline`. `test-microvm --scenario smp-lapic` remains for explicit local use: it runs
`smp` and also asserts that no CPU lists `tsc_deadline_timer`, that each online CPU's clockevent
device is the counting `lapic`, and that the boot line reports the backend's LAPIC rate for every
CPU. No default suite or CI job runs it, so nothing runs twice.
Captures do not wait for a clocksource: under the time ABI, Linux registers the `tsc`
clocksource at `device_initcall`, before any guest work, so no capture can observe the
transitional `tsc-early` window.
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
| `cold_start_clocksource` | Baseline plus `clocksource=tsc` on every backend. Before the time ABI, KVM measured `clocksource=kvm-clock`, so KVM history before that change measures a different clocksource. |
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

`virtfs.log` follows the summaries of each series with its retained samples in measurement order.
A host slowdown that delays part of a series splits its samples into two groups, which the median
alone hides.

### Device operation rates

The dedicated suite always uses the canonical microVM and attaches the block
backing object as the writable `scratch` role. There is no ABI selector or
legacy unroled `--virtio-blk` control. Every guest has 256 MiB RAM and runs the
same static Rust x86-64 binary from the reproducible initramfs.
Canonical runs execute five warmups followed by 30
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

An attempt that times out also describes the host in its error before the harness kills
OpenVMM: the files of the attempt's directory, with their sizes and modification times; the
OpenVMM processes that still run; and, on Windows, the scratch volume's write rate, latency,
queue, and busy time, with the CPU, cache, and Defender activity, over 3 s, from the counters of
the [snapshot lifecycle profile's](#snapshot-lifecycle-profile) storage telemetry. A stalled
volume can block each of them, so each that does not finish within 10 s says so instead.
`nvx-device-io` reports a file benchmark's progress on standard error, which the guest console
shows: `nvx-device-io: opened` once it has opened, and when creating, sized the file,
`nvx-device-io: transferred N blocks` once its timed loop ends, and, for a write,
`nvx-device-io: flushed` or `nvx-device-io: flush failed`, so the console of a hung attempt
shows the phase that it hung in.

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

The 64 MiB pair, `shell_snapshot_cold_64_mib` and
`shell_snapshot_restore_64_mib`, was retired in #116. The platform CSVs in
`data/` keep its results only as history; the last ones are `b8912df`'s.

| Metric | Description |
| --- | --- |
| `shell_snapshot_cold_128_mib` | OpenVMM launch to a shell-ready guest with 128 MiB of memory. |
| `shell_snapshot_restore_128_mib` | Restore process launch through lifecycle-aligned verification of a 128 MiB snapshot. |
| `shell_snapshot_cold_256_mib` | OpenVMM launch to a shell-ready guest with 256 MiB of memory. |
| `shell_snapshot_restore_256_mib` | Restore process launch through lifecycle-aligned verification of a 256 MiB snapshot. |
| `shell_snapshot_cold_512_mib` | OpenVMM launch to a shell-ready guest with 512 MiB of memory. |
| `shell_snapshot_restore_512_mib` | Restore process launch through lifecycle-aligned verification of a 512 MiB snapshot. |

Each memory section of `shell-snapshot.log`, and of `shell-snapshot-restore.log` for the
restore-only suite, lists the retained restore samples in measurement order after the restore
summary.

On MSHV and WHP, collection rejects a restore series of at least ten samples that a transient
host stall split, with the limits that guard snapshot generation: a p50 more than 25% above p25,
or a gap of more than 25% between adjacent sorted samples with at least two samples on each side.
Such a series must list its samples, and the list must match the summary's count and median.
`performance validate-workload` applies the same check without collecting, so that CI can
remeasure the series, as [CI collection](#ci-collection) describes. KVM restore series are
exempt: the KVM CI runners restore a snapshot in two modes about twice apart, so 1,933 of the
6,192 KVM restore series that CI measured from 2026-10-04 to 2026-10-27 exceed those limits
without a stall, against 4 of 6,144 on MSHV and 2 of 5,646 on WHP (#345).

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
subprocesses remove the variable unless profiling is requested. OpenVMM writes one ASCII record per
phase to stderr with the versioned `OPENVMM_SNAPSHOT_PROFILE_V1` prefix. Each record contains:

- `operation`, `phase`, and `exclusive`, where exclusive phases are disjoint intervals and
	non-exclusive phases are cumulative milestones that may contain nested work;
- monotonic `duration_ns` and process-relative `process_elapsed_ns`, plus the emitting `pid`;
- phase-specific `logical_bytes`, `allocated_bytes`, `gpa_faults`, and `populated_bytes` when
	available.

OpenVMM relays the guest console to stdout from its own thread, with no ordering against its
stderr writes, so either stream can split a line of the other when they share a terminal or
pipe. Snapshot capture and launch measurements therefore read stderr through a separate pipe:
they parse profile records only from stderr and match guest markers only on the console. Their
logs and error reports interleave the two streams by whole lines. Because the streams are read
independently, a record written before a guest marker can arrive after it. Snapshot capture, and
launch measurements before they return a sample, read both streams to their end within the
configured timeout and fail if either stream does not end.

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
and minor, major, and total page faults from `/proc/<pid>/status` and `/proc/<pid>/stat`,
which the kernel fills from counters that it maintains. The coordinator reads them while
OpenVMM runs the intervals that the benchmark measures, so it reads nothing that walks
OpenVMM's memory map. Reading `smaps_rollup` there delayed OpenVMM's quiesce and state save
on MSHV, which raised the snapshot-generation p50 by about 30% on the MSHV CI runner and more
than doubled it on a bare-metal host, and spread its samples until the stability guard
rejected some series (#383). Windows reports working set, peak working set, private commit,
and page faults.

On Windows, each retained capture sample also carries `storage_telemetry`. From just before the
host requests the snapshot until the source process exits, a coordinator thread samples
cumulative host counters about every 50 ms: read and write bytes, request counts, response
times, idle time, and queue depth of the scratch, workspace, and system volumes; CPU idle,
kernel, and user time; and the system cache's dirty pages and threshold, lazy-writer and flush
pages, and modified- and mapped-page-writer pages. About every 500 ms, and in the first and last
samples, it adds the CPU time and I/O transfers of the Microsoft Defender processes
`MsMpEng.exe` and `MpDefenderCoreService.exe`. CI jobs run as Network Service, which cannot read
performance counters, so the sampler uses only interfaces that any account can query: volume
performance IOCTLs, `GetSystemTimes`, and `NtQuerySystemInformation`. Windows maintains disk
counters only on demand, and the volume IOCTL returns none until a consumer asks for them, so the
sampler keeps the disk performance WMI data block, which every account may query, open while it
samples; it changes no system setting. A failed read is recorded
in `errors` and never fails the benchmark. The raw `samples` are retained, and `phases`
summarizes `capture.snapshot_generation` and `capture.mapped_memory_flush`. Each counter group,
a volume, CPU times, cache counters, or Defender's processes, spans the samples nearest to each
phase's start and end that carry it, so a failed read does not hide what neighboring samples
recorded. The observer clock is aligned with OpenVMM's by the smallest observed-minus-reported
time of the capture's records. Each summary reports the sampled interval it spans; per-volume
throughput, IOPS, average latency, average queue length, busy time, and peak queue depth; CPU busy
time; the lazy-writer, flush, and page-writer rates; the peak dirty cache and its threshold; and
Defender CPU time and I/O. A counter group whose samples differ from the summary's, such as
Defender's sparser process counters, reports its own interval.
Linux captures do not flush a file-backed guest RAM image, take about 1 to 3 ms, and record no
storage telemetry.

`profile.phases` groups samples by `operation.phase` and stores raw `samples_ms` plus p50,
nearest-rank p95, minimum, and maximum. The top-level JSON path is
`snapshot_profile_matrix.<backend>.<memory_mib>`. Each memory entry contains the capture result and
`restore.warm` and/or `restore.cold`, including its cache control and full restore result. Warmups
are excluded from raw samples and summaries.

Capture records isolate guest quiesce, state save, mapped-memory and memory-handle flushes, each
publication step, publication observation, and source teardown. Restore records isolate artifact
open and preparation, COW section and mapping/view creation, prototype and final partition work,
GPA registration, VP-thread binding, saved-state restore, device start, generation-ID creation,
the cumulative gated guest-repair interval, and guest resume to readiness. State and memory SHA
stages are intentionally absent from the final capture and restore path.

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
unless it contains exactly the 21 shared metrics. Supplying `--lifecycle-input` requires and merges
the eight lifecycle metrics, producing a 29-metric one-vCPU result. CI collects the five device
operation-rate metrics from their separate raw-log directory and merges them by ABI, processor
count, commit, and metric, producing the final 34-metric ABI-2 one-vCPU result. Higher-vCPU
collection uses `--require-shell-snapshot-restore-512`, which requires exactly
`shell_snapshot_restore_512_mib` plus canonical one-warmup/five-sample metadata for a 2-, 4-, or
8-vCPU guest. A `device-io` directory is recognized from metadata and must contain exactly its five
metrics and every configured attempt. Each backend job publishes its p50 tables and one-vCPU lifecycle diagnostics to
`$GITHUB_STEP_SUMMARY`. Pull-request regression checks compare KVM, MSHV, and WHP results with the
latest base-branch history.

These jobs consume, gate, publish, or persist the KVM benchmark results (#286); MSHV and WHP
follow the same flow with their own platform jobs:
- `Platform / Linux / KVM / Virtual machine` (`platform-kvm`) runs the benchmarks and publishes the
  results: the run-scoped `benchmark-linux-kvm-virtual-machine-<run-id>` artifact, the per-attempt
  diagnostics artifact, and the p50 tables in its step summary.
- `Performance regression gate` (`performance-gate`, pull requests only) consumes the three
  platform artifacts once all three platform jobs succeeded and gates them against the base
  branch's history. It publishes and persists nothing.
- `Persist performance baseline` (`performance-persist`, `dev` pushes only) consumes them and
  persists them to `data/*.csv`.
- `Publish development release` (`release`, `dev` pushes only) doesn't read them, but it and
  `performance-persist` run only when both NVX CLI jobs and the three platform jobs succeeded
  and every correctness job, `nvx-microvm-tests-kvm` included, succeeded or was skipped.
- `Required status check` fails unless every job that the change schedules, including
  `platform-kvm`, `performance-gate`, and `nvx-microvm-tests-kvm`, has its expected result.

Each platform job runs on its backend's dedicated benchmark runner. The job targets the
pool's labels plus `benchmark`, which exactly one runner of each pool carries instead of
`general`. Every other backend job requires `general`, so it cannot occupy the
benchmark runner. Every run therefore measures the series on the same VM without waiting for
an unrelated build or test, and the gate compares a pull request with history from that VM.
Platform jobs from overlapping runs still serialize on their one eligible runner. The pools
mix VMs whose results differ by more than the run-to-run noise: over the same `dev`
commits, the WHP runners on Emerald Rapids hosts measured about 10% slower than those on Ice
Lake hosts, and MSHV runners in another scale set measured guest-exit teardown several times
slower (#415). A pooled series compared each pull request with whichever runners had measured
the base-branch window, so runner placement alone could fail the gate. List the benchmark
runners with
`gh api repos/microsoft/nvx/actions/runners --jq '.runners[] | select(any(.labels[]; .name == "benchmark")) | .name'`,
and provision them with the benchmark switch of the
[runner setup scripts](../scripts/setup/README.md#github-actions-runners). Before it
measures, each platform job checks that its runner is the host that the series' history comes
from, as [Expected benchmark hosts](#expected-benchmark-hosts) describes. Moving the label to
another runner is therefore a performance-platform change: unless the new runner's host
matches in every required field, its platform jobs fail until a pull request accepts its host
and resets the series' history.

Linux and Windows CI use a reduced device contract of one warmup and ten retained attempts.
Both CI contracts use one-second operation windows. Canonical baseline collection uses the full
`5 + 30` contract.

The current workflow collects 10 measured lifecycle samples after one warmup.
Linux and Windows CI validate the lifecycle result before starting the remaining
benchmark suites. Snapshot-generation instability is reported with temporary-failure
exit status 75; CI remeasures it once on the same runner. Each attempt is preserved in
the benchmark artifact as `acceptance-attempt-1.json` or
`acceptance-attempt-2.json`, including its lifecycle profiles. Only a validated
attempt is copied to `acceptance.json` for collection; a stale accepted result is
removed before measuring. Other validation failures stop immediately, and a second
unstable result still fails the job. The stability guard rejects a p50 more than
25% above p25 and also rejects an adjacent gap above 25% when at least two samples
lie on each side.
Singleton outliers remain tolerated, while a host stall cannot publish a bimodal
series into the history. On the Linux CI runners,
snapshot generation takes about 1 ms on KVM and about 3 ms on MSHV, so a
sub-millisecond CPU stall in two samples is enough to cross the gap limit.

Each validation also appends the attempt's file, SHA-256 digest, outcome (`valid`,
`unstable`, or `invalid`), exit status, and error to `acceptance-attempts.json`,
which the acceptance step removes, with any earlier attempt files, before measuring.
The publish step passes that log to `performance collect --lifecycle-attempt-log`.
Collection fails before it writes a CSV unless `acceptance.json` is a byte-identical
copy of exactly one valid attempt; it then prints the attempt that supplied the
published lifecycle metrics. The step summary lists every attempt with its outcome,
snapshot-generation p50, and whether it was published, and reports the storage
telemetry of each attempt, rejected ones included.

CI likewise validates the [shell-snapshot restore series](#shell-snapshot) after the
one-vCPU `performance` suite and after each 2-, 4-, and 8-vCPU `shell-snapshot-restore`
suite. Each attempt writes its logs to its own `performance-attempt-N` or
`shell-snapshot-restore-attempt-N` directory, and `performance validate-workload`
checks it. Status 75 on the first attempt remeasures the whole suite once, and only a
validated attempt's files are copied to the result directory for collection. Other
validation failures stop immediately, and a second unstable attempt fails the job. On
KVM, whose restore series have no stability guard, validation accepts every attempt.

Before updating the run-scoped `benchmark-<platform>-<run-id>` artifact, each platform
uploads its raw results and lifecycle profiles to the immutable
`benchmark-diagnostics-<platform>-<run-id>-attempt-<run-attempt>` artifact, including
after a benchmark failure. Both artifacts are retained for one day. Rerunning a
workflow cannot overwrite an earlier attempt's diagnostics. Downstream gates still
use the run-scoped artifact so a failed-jobs-only rerun can reuse successful
platform results from an earlier workflow attempt. The `acceptance-attempt-N.json`
files and the `performance-attempt-N` and `shell-snapshot-restore-attempt-N`
directories identify the bounded remeasurements within one workflow attempt, not the
workflow's `github.run_attempt`.

A storage stall on a Windows runner leaves events that a reimage of the runner erases, so when a
Windows platform job's benchmark fails, the job saves the storage events of its window in the
diagnostics artifact's `storage-events` directory. The window starts with the job's first step,
which records the time in `NVX_JOB_STARTED_AT`, and ends as the events are saved. The diagnostics
upload, which runs whatever failed, keeps them, and both uploads precede the scratch removal,
which a stalled volume can hold up past the job's timeout, when no later step runs.
`python scripts\nvx.py performance storage-events --since <time> --output-dir <dir>` exports,
with `wevtutil`, the `Microsoft-Windows-Storage-Storport/Operational` events and the System
log's events of the storage drivers and file systems as rendered XML, in
`storport-operational.xml` and `system-storage.xml`, and writes `storage-events.txt`, which the
job log repeats up to its list of every event. It counts each log's events by ID, and then lists
the notable ones: I/Os that took longer than Storport's threshold, 30 seconds by default (event
557), request timeouts (500), resets (550), the resets that storage miniports such as `storvsc`
issue (129), and the hourly latency summaries (505) that counted I/Os above a second, by latency
bucket. Storport names each device by its SCSI port, path, target, and LUN, which host
provenance records for the benchmark's volumes, so the summary names the volume, such as
`workspace/scratch F:`, of each device that it knows. A stalled volume can stall what reads it
too, so the command abandons a query after 120 seconds, and probes the volumes only once the
events are saved, for at most 30 seconds, after which the summary says why it names none. It also
abandons a write of its files after 30 seconds, as the workspace can share that volume, and the
summary in the job log and a warning then name the file. I/Os
slower than Storport's threshold also raise a `Slow storage I/O` warning with their count and
the slowest, and a log that can't be read raises a `Storage events` warning. Network Service,
the runners' account, may read both logs as a service account; locally, reading the Storport log
takes an elevated prompt or membership in Event Log Readers.

Lifecycle JSON results and workload `benchmark-metadata.json` files record host
provenance under `host`: the runner name from `RUNNER_NAME`, the machine name, the
operating system with its kernel release or Windows build and cumulative-update
revision (UBR), the hypervisor that a Linux host runs under, the CPU model with its
vendor, family, model, stepping, and microcode revision, the logical processor count,
and physical memory, and the
volume, file system, size, system-volume flag, disk number, and SCSI address behind
the workspace, temporary, and scratch directories. On x86-64 Linux, the coordinator runs
CPUID without privileges, from a private page that it makes executable only after writing
the code into it. It records the hypervisor's vendor under `hypervisor.vendor`, `null`
without a hypervisor, and a Microsoft hypervisor's version under `hypervisor.build`, such
as `10.0.26100.9444`. A kernel that forbids executable anonymous memory leaves a
`hypervisor.error`, and other hosts record no hypervisor. The disk number and SCSI address
tell the Azure OS disk from a data-disk LUN or the temporary disk. On a possible Azure
VM, the coordinator queries the instance metadata service once, directly rather than
through a proxy and with up to three attempts when the service doesn't respond, and
keeps only the VM size; each managed disk's SKU, size, caching
policy, write-accelerator and ephemeral settings, and data-disk LUN; the throughput
and IOPS limits that Ultra Disks report; and the temporary disk's size. It never
records names, resource or VM identifiers, the subscription, tags, network addresses,
or credentials, and records an unreachable service as `azure.error`. Acceptance and
diagnostic suites print the provenance before they measure, and the step summary
includes it as the benchmark host table. A device-io resume ignores `host` when it
compares controls and provenance. The runner and machine names identify the benchmark
runner that measured a result.

When investigating instability, compare each attempt's
`snapshot_capture.<backend>.profile.raw_samples` with its `samples_ms`. For example, a
slow `capture.mapped_memory_flush` with otherwise stable capture phases localizes
the delay to host-side mapped RAM flushing, not guest boot or snapshot restore.
Extra time in `capture.quiesce`, `capture.save_state`, or between profiled phases
while the flush and publication phases stay flat instead points to a host CPU stall,
or to a process that reads OpenVMM's memory map during the capture, as the profile's
own `smaps_rollup` reads did until #383. A delay that recurs on an idle bare-metal
host of the same backend is not a host stall.
Reproduce with the exact executable and guest artifact hashes on the same host
before attributing the delay to a source change. Keep the stability thresholds and
bounded remeasurement unchanged when collecting diagnostic evidence.

On Windows, the job log prints each measured capture's `capture.mapped_memory_flush`
time with the host activity during it, and the step summary tabulates the same
telemetry for every attempt: the scratch volume's write throughput, average write
latency, average queue length, and busy time; writes to the other monitored
volumes; CPU busy time; the peak dirty cache; lazy-writer throughput; and Defender
CPU time. Compare slow flushes with fast ones in the same series:

- Disk throttling: the slow flushes write scratch at a lower, flat rate with longer
  latency and queue while it stays busy, and the other columns stay quiet. Compare
  that rate with the published limits of the disk SKU and size in the host table.
- Scanning: Defender CPU time or I/O rises during the slow flushes.
- Writeback: lazy writes, the dirty cache, or writes to the other volumes rise
  during the slow flushes.
- Unrelated host load: CPU busy time rises while scratch stays idle for part of
  the flush.

In the JSON, the flush's own page writes appear in `memory.cache_flush_mib_per_second`,
and the raw samples show how the rates change within a flush.

Snapshot generation is bounded by the write throughput of the benchmark's
temporary directory, because capture flushes guest RAM through a backing file
created there. The Windows backing file stays dense so restore keeps the
captured pages cached. Because NTFS zero-fills a dense file below any write
past its valid data length, OpenVMM first marks every guest RAM page modified
and then flushes the range with one ascending write sequence, so a 128 MiB
guest writes at most 128 MiB per capture and the flush lasts about that size
divided by the scratch volume's write throughput.
The Windows CI runners' 128 GiB Premium SSD system disk also holds the runner
work tree, caches, and builds. Under load it alternates between its burst limit
of about 173 MB/s and its baseline of about 102 MB/s in blocks of tens of
seconds, which splits one lifecycle series into flush clusters near 0.95 and
1.6 seconds. Windows CI therefore passes `--scratch-dir` with a per-job
directory under the `NVX_BENCHMARK_SCRATCH` root that runner provisioning
creates on the data volume. Acceptance JSON records the directory as
`controls.scratch_directory`, and workload metadata records it as
`scratch_directory`. When the root is not provisioned, CI warns and uses the
system temporary directory. Windows-coordinated KVM workers receive the WSL
translation of the same directory instead of falling back to WSL's `/tmp`.
Use `--scratch-dir` for manual runs whose temporary directory shares a volume
with other I/O-heavy work.

Treat a change to the storage behind benchmark scratch or the runner workspace as a
performance-platform change. This includes a different VM size, disk SKU, size,
performance tier, caching policy, or bursting setting, and moving scratch between
the OS, data, and temporary disks. Because snapshot generation is bounded by the
scratch volume's write throughput, such a change shifts
`openvmm_snapshot_generation` and can shift the other lifecycle metrics. Apply it to
every runner of the pool at once, so that the benchmark runner and any runner that
later takes over its label share one storage regime. The expected host records the
VM size and the SKU, size, and caching of the disks behind these volumes, so the
benchmark runner's next platform job fails its host check; in the same pull request,
accept the new host and reset the series' history, as
[Expected benchmark hosts](#expected-benchmark-hosts) describes. For a change that the
host doesn't record, such as a performance tier or bursting setting, reset at least the
affected metrics by removing them from the series' history files, such as
`data/windows-whp-virtual-machine-microvm-v2-1vcpu.csv`, so that the gate restarts
their ten-point warmup instead of comparing the new storage with the old baseline.
Results measured before such a reset commit are not persisted into the reset
series, so a `dev` run that finishes after the reset cannot refill it.

The regression gate compares the target p50 with the median of the latest 10
p50 values on the pull request's base branch and requires all 10
matching history points. A metric regresses only when it is more than 50%
worse. Lower-is-better millisecond metrics must also be more than 10 ms
slower; higher-is-better metrics use the percentage comparison alone.
[`ci.yml`](../.github/workflows/ci.yml) sets these thresholds;
[`performance gate`](usage.md#performance-gate) defaults to 40% and 5 ms when
it runs without them.
Missing or insufficient history is a warmup, not a failure. Successful `dev`
builds append collected results to topology-specific files in `data/`. Every
new ABI/count series begins as a warmup baseline before its regression gate
has enough matching history.

Only the virtual-machine topologies exercised by CI have tracked histories. CI passes
`--history-reset-dir data` to honor metrics explicitly removed from a tracked candidate history
file even before the reset merges into the base branch, and to compare the candidate expected
hosts with the base branch's. After merge, reset metrics start the
normal ten-point warmup again; a missing candidate file alone does not reset a base-branch
history.

### Expected benchmark hosts

Each tracked series names the host that measured its history in `data/<platform>-host.json`,
such as [`data/windows-whp-virtual-machine-host.json`](../data/windows-whp-virtual-machine-host.json).
The file is the identity of that host's provenance: the runner and machine names; the CPU's
vendor, family, model, stepping, brand string, and microcode revision; the logical processors
and memory; the operating system with its kernel release, or its Windows build and UBR; on
Linux/MSHV, the build of the Microsoft hypervisor beneath the kernel; the Azure VM size and the
Azure disks that can back the benchmark's volumes; and the mount point, file system, and
system-volume flag of the workspace, temporary, and scratch volumes. The OS disk counts when a
volume is the system volume, and the data and temporary disks when a volume is on another
disk, so the data disk of a Linux runner, whose scratch is on tmpfs, doesn't. Paths, device
names, SCSI addresses, and volume sizes are left out because they differ between identical
VMs.

Before its first benchmark, the platform job's `Check Linux benchmark host` or `Check Windows
benchmark host` step runs `nvx.py performance check-host`, which compares the runner with the
file. Each field is required or advisory:

| Property | Required | Advisory |
| --- | --- | --- |
| CPU | Vendor, family, model, stepping, and brand string | Microcode revision |
| Processors and memory | Logical processors; memory within 5% | |
| Linux | Kernel major and minor version and flavor, the release tokens that contain letters, such as `mshv3` and `azl3` | Kernel patch release |
| Linux/MSHV | Hypervisor build, the whole version of CPUID leaf `0x40000002`, such as `10.0.26100.9444` | |
| Windows | Build, such as `10.0.26100` | Cumulative-update revision (UBR) |
| Azure | VM size; each disk's SKU, size, caching, write accelerator, ephemeral option, and limits | |
| Volumes | Mount point, file system, and system-volume flag | |
| Runner | | Runner and machine names |

A required difference fails the job before it measures. Its error annotation lists every
difference, and the step summary compares each property and shows the runner's host file,
which the diagnostics artifact also keeps as `<platform>-host.json`. The memory tolerance
absorbs kernel reservations, which differ between VMs of one size: the KVM and MSHV runners,
both `Standard_D8s_v5`, report 31.3 and 30.9 GiB. The kernel flavor is required because the
`azure-mshv-scus` runners of #415, on `6.6.137.mshv2-2.azl3` rather than `6.6.148-200.azl3`,
measured guest-exit teardown several times slower. The hypervisor build is required because
the hypervisor comes from the `mshv` package and the kernel from `kernel-mshv`, so an update
can replace one without the other, and the kernel release doesn't show a new hypervisor
(#980). The provenance reads it from CPUID without privileges, as `major.minor.build.service`,
the version that OpenVMM's CPU fingerprint records, and the service number counts:
`10.0.26100.9444` and `10.0.26100.30000` are both build 26100. A hypervisor that can't be read
records an unknown build, which matches none, not even another unknown one. KVM and Windows
hosts don't record it: a KVM VM's hypervisor is its cloud host's, and a Windows build and UBR
identify Windows' own hypervisor.
An advisory difference, such as a microcode or Windows cumulative update, a kernel patch
release, or a VM replaced by an identical one, only warns. To acknowledge it, replace the file
with the runner's host; that needs no history reset. Host files and results that predate the
hypervisor build don't record it, which also only warns, so adding the build to an expected
host needs no history reset either.

`performance collect` writes the host that results record next to them as
`<platform>-host.json`. It refuses inputs of which only some record a host, and refuses to
collect results of two hosts, or results with and without a recorded host, into one directory. `performance persist` refuses to append results
whose recorded host differs from the history's `<platform>-host.json` in a required field, or
that record no host for a history that records one. `Persist performance baseline` also passes
`--require-host`, which requires both host files, as a backstop for `dev` pushes.

To accept a new host, for example after a redeploy, resize, reimage, or OS update changed a
required property, or after the `benchmark` label moved to a runner whose host differs, open a
pull request that, in one change:

1. Replaces `data/<platform>-host.json` with the runner's host from the failed check's step
   summary or diagnostics artifact.
2. Resets the series by truncating each of its history files, such as
   `data/<platform>-microvm-v2-1vcpu.csv`, to its header line.
3. For a Linux/MSHV host with another hypervisor build or kernel, moves the pins of the MSHV
   packages in [`setup-mshv.sh`](../scripts/setup/azure/linux/setup-mshv.sh) to the new host's
   releases, which a test compares with the host file. Runners install the pinned releases
   whenever they're provisioned, as after a reimage, so that a reimage alone never moves the
   series to another hypervisor
   ([Azure CI runner provisioning](../scripts/setup/azure/README.md#configuration)).

The pull request's platform job compares the runner with the new file, and the gate treats the
reset metrics as warmup. The gate compares the pull request's expected hosts with the base
branch's and fails when a required property changed while the series' history files keep any
row, of any metric, so that the new host's results never extend the previous host's history.
After the merge, `dev` pushes restart the series' ten-point warmup.

## Lifecycle methodology

The lifecycle benchmark uses the optimized 128 MiB shell-ready guest as one baseline across KVM,
MSHV, and WHP. Host timing starts immediately before `Popen`, so cold-start and restore values
include OpenVMM process startup and VM construction. On Linux, that startup includes the execs of
`setpriv` and of a shell that checks that the coordinator still runs, which make the coordinator's
exit kill OpenVMM (see [Cancellation](ci.md#cancellation)). On a Xeon Silver 4114 host, they added
2.1 ms to the time until OpenVMM ran with Dash as `/bin/sh`, and 3.5 ms with Bash, which the Azure
Linux runners use. Snapshot generation uses OpenVMM's monotonic
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

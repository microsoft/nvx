# Continuous integration

The GitHub Actions workflow has two microVM test layers on Azure-hosted
self-hosted KVM, MSHV, and WHP virtual machines. Each backend has a pool of
three runners labeled by operating system, backend, and `virtual-machine`.
Jobs target the shared backend labels so any available matching runner can
execute them. This allows the backend lanes to execute concurrently without
binding a workload to a specific host. `openvmm-vmm-tests` downloads the NVX
guest artifacts and uses the Linux-direct kernel and Alpine initramfs to
exercise OpenVMM's Linux MP-table lifecycle, TTRPC, and snapshot contracts.
`openvmm-unit-tests` runs the OpenVMM unit and documentation tests independently
on the same backend matrix.
Failed `openvmm-vmm-tests` jobs upload Petri's `test_results` directory,
including guest and VMM logs, screenshots, and watchdog inspection data.
These seven-day artifacts are named
`openvmm-vmm-tests-<os>-<backend>-<run-id>-<run-attempt>`, so a successful rerun
does not replace the failed attempt's diagnostics. Linux collects them from
`openvmm/target/vmm_tests/test_results`; Windows uses
`<runner-temp>/<backend>/test_results`.
The `nvx-microvm-tests-{kvm,mshv,whp}` jobs consume the NVX Linux kernel and
the NVX Linux kernel plus the selected Alpine or Ubuntu initramfs and exercises
Linux, SMP, virtio, sandbox, and snapshot behavior through the public OpenVMM
CLI. Alpine-control-only scenarios remain explicit and are rejected for the
Ubuntu initramfs. Each job also boots the Azure Linux initramfs through its
one-vCPU smoke set, under the same time ABI checks; the debug-kernel jobs skip
it, as they skip the Ubuntu tests. Failure logs from the NVX layer are uploaded
per backend.
Every harness launch, in the tests and the benchmarks, scans the OpenVMM
console for the guest's [time ABI](design/time-abi.md) output, and the tests
scan every virtio console they read over TCP the same way, to its end. An
`NVX-TIME-ABI-VIOLATION` event or a failed `NVX-TIME-ABI` conformance line
fails the scenario at once with the guest's code and detail. Guests keep the
console quiet, because every console byte costs a port exit. Only the initial
clock step precedes the guest's boot marker; the other boot checks finish
afterwards and still power off with status 193 on failure, possibly in the
middle of a scenario. A guest prints one `NVX-TIME-ABI` line per recorded
check phase (boot, then capture and restore after a restore) and a runtime
line only when `/sbin/nvx-time status` asks, after waiting up to 30 s for
pending checks. The test runners ask after every cold boot whose shell is on
a console the harness reads, the OpenVMM console or a virtio console over
TCP, before any other input, and wait for the query to exit, so the console's
echo of later input cannot split its lines. Cold boots with no shell there are
only scanned: one-shot workloads, and the managed lifecycle and `pause-resume`
scenarios, where init starts the managed agent instead of a shell. A failing boot check still powers them
off with status 193, which fails the run. The query must
exit 0 with a passing `NVX-TIME-ABI` boot line, which reports ABI version 1,
generation 0, a plausible TSC rate, and the backend's LAPIC rate; a missing
line means the guest image or OpenVMM does not implement the ABI, a check
still pending after the guest's 30 s wait fails, and a report-only guest
(`NVX-TIME-REPORT`) is never accepted. A guest image whose `nvx-time` has no
`status` subcommand fails with that reason. The runtime line only records
the wall-clock discipline's state. Benchmarks never ask, so their measured
intervals stay quiet. An OpenVMM exit
status of 193, 194, or 195 is reported as the guest's time ABI conformance,
runtime-violation, or restore-repair power-off, together with the event that
preceded it, instead of as a generic exit status.
The `smp`, `smp-snapshot`, and `restore-processors` scenarios run the guest
warp probe, `/sbin/nvx-time-probe warp --bound-ns 1000`, over every pair of
online CPUs after each boot or restore, in two rounds with all vCPUs halted
for 1 s between them, so that a host without an invariant TSC corrects the
guest TSC as idle host CPUs wake (#265). Every pair in every round must stay
within the time ABI's 1 µs
[cross-vCPU skew bound](design/time-abi.md#cross-vcpu-skew-bound) for both the
backward TSC step and the ping-pong offset; a stalled pair or an inconclusive
measurement also fails. `smp` boots each requested processor
count. `smp-snapshot` captures one snapshot per requested count and restores
it once, and the first snapshot a second time to prove that a restore leaves it
reusable. `restore-processors` captures one boot-online CPU with capacity 8 and
probes the CPUs that each restore target activated. After the 1/2/4/8-CPU
restores, it restores the same snapshot once without `--restore-processors`.
After the warp probe, every `smp-snapshot` and `restore-processors` restore
runs `/sbin/nvx-time status`, which waits for the restore's deferred checks;
it must exit 0 with a passing `NVX-TIME-ABI ... phase=restore` line that
reports the backend's LAPIC rate, the restored CPU count, and generation 1:
every scenario captures a cold-booted guest, and OpenVMM cannot capture a
restored one. The query also prints the source's boot and capture lines,
which keep the CPU count their checks covered, so only the restore line must
match the restored CPU count.
Every restore runs with OpenVMM lifecycle profiling and must report exactly one
`startup.vp_thread_bind` record. Its `startup.vp_bind_*` records must show that
an explicit MSHV target binds exactly VPs `0..N-1`, while untargeted MSHV
restores and all KVM and WHP restores bind the full capacity.
Captures do not wait for a clocksource: the time ABI registers `tsc` at
`device_initcall` on every backend, so the transitional `tsc-early` window that
once let the clocksource watchdog compare `tsc-early` with jiffies across a
restore (#253) never reaches the guest's userspace.
A restore fails as soon as its guest prints `NVX-RESTORE-PROCESSORS-FAIL` or
`NVX-WARP-PROBE-FAIL`, rather than waiting for the phase timeout; a failed
probe also powers the guest off with status 97. Restore-processor logs record
OpenVMM's `time ABI rates declared` event, which reports the identity MSR
route, the TSC synchronization method, the native and declared TSC rates, and
the rate deviation from the snapshot.
The `restore-downtime` scenario covers the time ABI's long-downtime case. It
captures four snapshots, at 1 and 8 vCPUs, each with and without
`rcupdate.rcu_expedited=1`, then waits 30 s, longer than the guest's 21 s RCU
stall timeout, and restores each one. Every restored guest must run the warp
probe, report a passing restore line through `nvx-time status` before the
harness stages its check, report `/sys/kernel/rcu_stall_count` as 0 two
seconds later, and show an uptime of at least 30 s, which proves that
monotonic time advanced by the downtime. The captures share one downtime
window, so the scenario adds about a minute per backend.
The `pause-resume` scenario covers [host pause](design/time-abi.md#host-pause).
It boots the largest requested vCPU count with the managed lifecycle and
OpenVMM's state-control endpoint (`--microvm-state-control`), which
authenticates hosts with the control console's capability. A host that does
not authenticate, or presents the wrong capability, must get no response.
While a managed workload spins on one vCPU, the harness pauses the VM for
30 s, longer than the guest's 21 s RCU stall timeout and the debug kernel's
20 s soft-lockup threshold, and then twice for 2 s. It repeats every pause and
resume request, which must change nothing the second time, queries the run
state while paused, and requires the transition count to rise by one on every
change. On Linux hosts, the OpenVMM process may use at most a tenth of a
pause, plus 0.2 s, in CPU time while paused. Afterwards the workload must
finish, having started at least 0.5 s of guest uptime before the long pause
and stopped after it, guest uptime must have advanced by the time the VM ran to
within 1 s, and `/sys/kernel/rcu_stall_count` must read 0. Host UTC kept
running, so the guest's wall clock must return to within 1.5 s of host UTC
within 150 s, through at least one wall-clock discipline step that
`/run/nvx/time/state` records, with no new violation there. A second host,
connected after the first one disconnects, must see the same broker instance
and transition count, and pauses the VM again; a third host must then find it
still paused. Finally the harness terminates OpenVMM, which must still be
running and must exit with the termination status. The scenario prints one
`NVX-PAUSE-RESUME:` line with its measurements and adds about a minute per
backend.
The `time-abi-conformance` scenario boots the largest requested vCPU count and
runs the guest's exhaustive CI check, `/sbin/nvx-time exhaustive`, which the
boot check leaves to CI. On every online CPU it checks every leaf
`0x40000006..=0x400000ff` and every base `0x40000100..=0x4000ff00` for another
hypervisor signature, every `C3` MSR, the write rules of
`HV_X64_MSR_TSC_INVARIANT_CONTROL`, writes to the read-only identity MSRs, and
reads of `IA32_TSC_ADJUST` and `IA32_TSC_DEADLINE` (checks `X1` to `X6`). The
harness requires a passing `NVX-TIME-ABI-EXHAUSTIVE` line for every check on
every CPU, a summary with `status=ok`, the requested CPU count, and no
failures, and exit status 0; a failure lists each failing check with the
guest's detail. The guest command fits on one console line, so the console's
echo of it ends before the check prints.
The `snapshot-core` scenario first sends a snapshot request to a guest that
OpenVMM launched without a snapshot destination. OpenVMM releases the request,
so it returns in the source, which must continue exactly once. Before the
request, the guest's snapshot agent saved and overrode the stall detectors'
settings, and it must restore them when the request returns
(`nvx-time cancel-capture`). Afterwards, `nvx-time status` must exit 0 with a
passing boot line at generation 0 and no restore line, and
`/sys/module/rcupdate/parameters/rcu_cpu_stall_suppress` must read 0.

At the end of a passing run, `test-microvm` prints one `NVX-TIME-ABI-EVIDENCE:`
line and adds it to the GitHub job summary, because CI keeps the guest logs only
for failed jobs: the number of warp probe runs with their worst
`max_abs_offset_ns` and `max_backward_ns`, the count and `elapsed_us` range of
the newest check that each `nvx-time status` query reported (boot after a cold
boot, restore after a restore, and capture after `snapshot-core`'s released
request), the exhaustive check's summary, and the
`restore-downtime` stall counts. Where the guest reports a check's CPU time
(`cpu_us`), the line also gives its range and how many checks exceed the
backend's [CPU-time budget](design/time-abi.md#performance-expectations-and-acceptance-gate)
for their phase, which the spec sets from guest measurements. The spec exempts
the first capture after a rolled-back capture in the same VM process; CI never
makes one, because each VM process sends OpenVMM at most one snapshot request
and a failed capture fails its scenario.
`CHECK_CPU_BUDGET_US` in `scripts/nvx_tools/time_abi.py` holds the budgets, one
per backend and phase. `elapsed_us` is wall time, including waits behind the
workload, and has no budget. CI gates on neither: the performance gate is the
A/B comparison outside CI.

The `nvx-microvm-debug-{kvm,mshv,whp}` jobs run `test-microvm --debug-kernel`
on the CI debug kernel (`build/vmlinux-debug`, built from
`kernel/config-microvm-debug`), whose soft-lockup and hung-task detectors
production kernels leave out. It selects the same-host restore scenarios
`smp`, `smp-snapshot`, `restore-processors`, `restore-downtime`, and
`snapshot-tiers`, and the host pause scenario `pause-resume`. The jobs skip
every other test step, because those boot the
production kernel, which the `nvx-microvm-tests-{kvm,mshv,whp}` jobs already
cover: the public managed execution configuration, the Ubuntu and Azure Linux
guest tests, the Ubuntu sandbox layer and live-share smoke tests, the
caller-owned live-share test, and the `aci_edge_sandboxes` lifecycle test. Any
RCU stall, soft lockup, or hung task makes the guest's time ABI watcher power
off with status 194, which fails the run. Every guest those scenarios restore
asks `nvx-time status` before it exits
(`snapshot-tiers` after its tier assertions), which waits for the restore's
deferred checks and must report a passing restore line, so no restore leaves
its checks pending. The one exception is `snapshot-tiers`' gate-timeout check,
whose guest OpenVMM stops at the restore gate on purpose. The harness refuses
a kernel whose `vmlinux-debug.config` lacks the detectors, because the guest's
`C11` check passes vacuously without them. To bound the cost, pull requests
run the debug kernel on KVM only and `dev` pushes run it on every backend;
each job takes about six minutes on its own runner, in parallel with the
other microVM jobs. The jobs gate the required status check, the development
release, and performance persistence. The GitHub-hosted `debug-kernel` job
builds the debug kernel beside the shared `artifacts` job
(`build-guest-artifacts` with `guest-images: "false"`) and caches it under its
own key, so a kernel rebuild delays only the debug jobs, and a failed debug
kernel build fails the required status check and blocks the release.

Every job that uses the `validate-runner` action first requires an invariant
TSC on a Linux runner (`nonstop_tsc` in `/proc/cpuinfo`) and fails without
one, as before the time ABI. It then qualifies the runner for the time ABI
with `nvx.py doctor --checks H1 H2 H4 --ci-schedule` (see [Host
qualification](#host-qualification)): the backend, the CPU fingerprint and
generation, and the TSC rate stability on its short schedule. The microVM and
platform jobs run it after downloading OpenVMM and add OpenVMM's CPU profile
check (H2) and preflight (H3), and keep the fingerprint as an artifact when
the profile check fails; the other jobs pass `--no-openvmm`. This takes a few
seconds. It reports the CPU generation, the CPU profile that `auto` selects,
and the measured TSC rate in the log and the job summary, and fails the job
with a stable code when the runner is not qualified, for example
`E_PROFILE_HOST_UNKNOWN` on an unknown CPU generation. The doctor gates only
on measured properties, alike on every backend: it records the host OS's
invariant-TSC flags and clocksource as evidence, and the guest warp probe in
the microVM scenarios measures the skew that a host without an invariant TSC
causes. On such an MSHV runner VM, never-restored guests hit cross-vCPU TSC
warps when an idle host CPU woke (#211, #265), which is why the probe schedule
includes idle gaps. The `nonstop_tsc` gate stays in front of it until every
job that runs guests also runs the warp probe: only the microVM jobs do, while
the OpenVMM vmm-tests and the platform benchmarks run guests after host-level
checks that such a host can pass. Runner labels do not encode the generation;
per-PR CI captures and restores on one runner, so generations never mix.

The warp probe replaced the `restore-tsc-sync` scenario, its test-only
`clearcpuid=tsc_adjust` kernel option, the guest's scan of the kernel log for
TSC warp and instability messages, and the fresh-boot TSC control that
classified those failures (#211, #265). Under the time ABI the guest's TSC is
`tsc_reliable`, so Linux skips its CPU-online warp check and never logs the
messages that guard looked for; the probe measures the 1 µs bound on every CPU
pair instead.

The `console-exit` scenario delays host console reads for two seconds after
snapshot restore to exercise output backpressure. For each requested processor
count it requires byte-exact delivery of a 64 KiB payload and the final marker,
and preserves guest exit statuses 0 and 37. This checks both device and host-relay
draining without adding sleeps to the measured benchmark workloads.
The harness waits for the output reader's EOF notification even after the
process exits, so delayed final output chunks cannot create a false failure.

Shared guest artifacts are built with Docker on a GitHub-hosted Ubuntu runner.
The kernel, Alpine initramfs, Ubuntu initramfs, and Ubuntu EROFS layer use
separate cache keys. Ubuntu keys include the Canonical archive pin,
supplemental package lock, common guest sources, shared download and guest
descriptor modules, converter implementation, and Dockerfile. Artifact upload
retains the Alpine filenames and adds the distinct Ubuntu filenames. Each
backend also boots the Ubuntu initramfs and runs
`/sbin/nvx-sandbox-smoke` from the Ubuntu EROFS layer as UID/GID 65534 over a
fresh ext4 scratch copy. The same entrypoint then verifies a live virtio-fs
share inside the container: a read-write `/workspace` share with a denied
subdirectory must round-trip guest writes to the host, and a read-only
`/opt/hostedtoolcache` share must reject writes and symbolic links. In the
read-write share, the guest also exercises the symbolic-link primitives that
package managers use: it creates an npm-style relative `.bin` link to an
executable and runs it through the link, renames, replaces, and removes links,
and creates absolute, dangling, outside-the-share, and denied-path links. The
host then checks that every target is preserved exactly (on Windows, by
decoding each WSL-style reparse point, which Windows does not follow) and that
no link read or modified host data outside the share or in the denied path. A
managed sandbox then repeats
the read-write check through `provision`, `start`, `exec`, and `stop`, and must
report a successful outcome with a cleanly unmounted scratch filesystem, which
shows that `stop` unmounted the share and overlay first. Each backend then
attaches a read-write `/workspace` share and a read-only
`/opt/hostedtoolcache` share to one sandbox at the same time, each with its
own denied subdirectory. The smoke script runs the read-write and read-only
checks on the two shares and verifies that a link in the read-write share
cannot write into the read-only share; the host requires the guest's writes in
the workspace, an unchanged tool cache, and unchanged denied files, both for a
one-shot run and for a managed sandbox whose format-4 configuration must
persist both shares and whose `stop` must leave a cleanly unmounted scratch
filesystem. On Linux, a
world-writable share then runs with `--mount-owner caller`. The runners grant
OpenVMM neither `CAP_SETUID` nor `CAP_SETGID`, so it can neither assume the
65534 workload identity nor drop the backend group through which it opens
`/dev/kvm` or `/dev/mshv`. The smoke script's `eperm` mode therefore
requires reading and listing the share to fail with `EPERM` and writes to
fail, and the host requires the share to be unchanged, both for a one-shot
run and for a managed sandbox whose configuration must persist the mode. The `filesystem-owner` scenario of
`test-microvm` likewise requires guest root and a foreign guest identity to
fail with `EPERM` on the runners, and a root-owned export to be rejected
before boot; on Windows, `--mount-owner caller` must be rejected before boot.
The OpenVMM unit tests check the host identity that each request runs as,
including guest root squashed to an export owner other than OpenVMM. Where
OpenVMM can drop its supplementary groups, the scenario instead requires
files that guest root creates to belong to the share owner rather than to
root, and squashed root to fail to chown a file, give it another of OpenVMM's
groups, or create a device node. When the test process also passes
`CAP_SETUID` and `CAP_SETGID` to OpenVMM, as on a privileged developer host,
the scenario requires the foreign identity to own the files it creates.
Linux/KVM runs the
broader Ubuntu SMP, managed lifecycle, network snapshot, blockless snapshot,
and workload-identity set.

OpenVMM release executables and provenance are built once by the independently
addressable `build-openvmm-linux-gnu`, `build-openvmm-linux-musl`, and
`build-openvmm-windows-msvc` producer jobs. KVM workloads and MSHV microVM tests
consume the GNU artifact, MSHV platform workloads consume the musl artifact,
and WHP workloads consume the Windows MSVC artifact. Each workload can start
after its compatible OpenVMM producer and the shared guest-artifact job finish,
without waiting for unrelated OpenVMM targets.

All three producers call the same Python build workflow, passing the validated
runner backend explicitly through `build-openvmm --backend`. The backend is
carried in `OpenVmmBuildConfig`; the build workflow maps KVM, MSHV, or WHP to
GNU, musl, or MSVC without probing runtime devices. CI therefore retains its
musl build for MSHV without maintaining a separate shell build path.

The kernel, debug kernel, and initramfs cache keys include
[`docker/Dockerfile`](../docker/Dockerfile), which pins the build image and its
toolchain and in which every guest artifact builds, and
[`build_config.py`](../scripts/nvx_tools/build_config.py) and
[`build_constants.py`](../scripts/nvx_tools/build_constants.py), so a toolchain,
shared build configuration, or constant change invalidates cached guest
artifacts and their provenance. The Ubuntu distro layer shares the Ubuntu input
hash.

The producer handoff uses one-day workflow artifacts rather than caches. Each
consumer downloads both the normalized executable and its build provenance,
then restores executable permissions on Linux. Once the required artifacts are
ready, benchmarks run in parallel with the NVX test layer and use any available
runner in the matching backend pool. All three use virtual-machine performance
series and the constrained eight-CPU affinity policy. Development releases and
performance baseline updates still require every applicable test and benchmark
lane to pass.
The microVM correctness jobs gate every use of benchmark results, so the
benchmark action runs no correctness scenario of its own (#286). `Required
status check` requires each `nvx-microvm-tests-*` job that the change schedules
to succeed. `Publish development release` and `Persist performance
baseline` run only on `dev` pushes in which every microVM test job succeeded or
was skipped. The pull-request `Performance regression gate` reads only the
platform jobs' results and publishes nothing. The counting LAPIC that the
benchmarks depend on is covered by the `smp` scenario of those jobs.
`smp-lapic` repeats `smp` with the counting-LAPIC facts asserted, for explicit
local use only; no CI job runs it. [Benchmarks](benchmarks.md#ci-collection)
lists the jobs that consume, gate, publish, or persist the results. The workflow uses the read-only OpenVMM deploy key stored in the
`OPENVMM_DEPLOY_KEY` Actions secret to fetch the private submodule at its pinned
commit. Shared guest binaries and development release packages move through
short-lived workflow artifacts alongside the OpenVMM handoff and benchmark
results. Caches only accelerate reproducible build inputs and outputs; consumers
do not depend on them as a handoff.
Pull requests gate regressions against recent matching-platform history, and
successful pushes to `dev` append their p50 values under `data/`. Metadata-only
performance jobs use GitHub-hosted Ubuntu runners. Provisioning instructions
are in the [runner bootstrap guide](../scripts/setup/README.md).

Persistent runners accept pushes and same-repository pull requests only. Fork
pull requests run the GitHub-hosted validation jobs but do not execute code on
the Azure runner fleet. A maintainer must stage an external contribution on a
trusted repository branch before running the backend matrices.

## Host qualification

`python3 scripts/nvx.py doctor --backend <kvm|mshv|whp>` qualifies a host for
the [time ABI](design/time-abi.md#host-qualification). It runs checks H1 to H7
in order, prints one `NVX-DOCTOR: check=<id> status=<pass|fail> detail="..."`
line per check, and exits with status 1 if any check fails. `--checks` selects
a subset, and `--summary` appends a Markdown table with the CPU generation,
profile, rates, and skew metrics to a file such as `$GITHUB_STEP_SUMMARY`. H4
and H6 run on the spec's long qualification schedules, which take about three
minutes; `--ci-schedule` selects CI's short ones. A failure that matches a time
ABI failure code starts its detail with the code in brackets, for example
`[E_PROFILE_HOST_UNKNOWN]`. A check fails, and never passes, when it can't
read a fact it gates on: missing or unrecognized tool output fails it, and an
error in one check fails only that check. Only evidence fields, such as the
microcode and the invariant-TSC flags, may read `unknown`.

| Check | Implementation |
| --- | --- |
| H1 | `/dev/kvm` or `/dev/mshv` is readable and writable, and a KVM host has no `/dev/mshv`; on Windows, `WHvGetCapability` reports a hypervisor |
| H2 | Vendor, family, model, stepping, microcode, and OS build from `/proc/cpuinfo` or the Windows registry. Then `openvmm --hypervisor <backend> --cpu-fingerprint <path>` writes the host's CPU fingerprint (by default `nvx-cpu-fingerprint-<backend>.json` in the probe directory; `--cpu-fingerprint` overrides it), checks it against the profile that `auto` selects from OpenVMM's catalog, which shares one profile per generation across backends, and prints one `NVX-CPU-PROFILE:` line. H2 reports the generation and the profile from that line, so a profile that an OpenVMM pin adds qualifies its hosts without an NVX change. H2 requires exit status 0, `status=pass`, the same backend, and a catalog profile of the reported generation, reports the profile and surface digests, and otherwise fails with OpenVMM's code, for example `[E_PROFILE_HOST_UNKNOWN]` on a CPU that no profile serves or `[E_PROFILE_UNSUPPORTED]` naming every unsupported CPUID bit. On WHP, OpenVMM checks the entries outside the profile on a probe partition configured from the profile, as a cold boot does. With `--no-openvmm`, H2 maps the host with NVX's copy of the catalog instead: `skylake-sp` (6/85, steppings 0 to 4, `intel.skylake-sp.v1`), `icelake-sp` (6/106, `intel.icelake-sp.v1`), `emeraldrapids` (6/207, `intel.emeraldrapids.v1`), `alderlake` (6/151 and 6/154, `intel.alderlake.v1`), or `milan` (AMD 25/1, `amd.milan.v1`). Any other CPU, including Cascade Lake and Cooper Lake, fails with `E_PROFILE_HOST_UNKNOWN`. A unit test keeps the copy equal to the profiles of the OpenVMM submodule's pinned revision, which it reads from the gitlink's commit, so the CI jobs validate the NVX CLI after they check out OpenVMM. The host OS's invariant-TSC flags (`constant_tsc nonstop_tsc`, or the CPUID bit on Windows) are recorded as evidence and never fail the check, because they don't decide what a guest observes: Azure WHP hosts show the CPUID bit but cannot offer invariant TSC to partitions, and their guests measure tens of nanoseconds of skew |
| H3 | `openvmm --x-time-abi-verify` builds the partition and runs the time ABI preflight without running the guest. Its `NVX-TIME-ABI-VERIFY:` line must report `status=ok` for the backend, plausible declared and native TSC rates, the backend's LAPIC rate, and a `cpu_profile` that is a catalog profile, `<vendor>.<generation>.v<revision>` of any generation but a host profile's `host`, and, when H2 runs too, a revision of the profile H2 names. A failed preflight reports OpenVMM's code, for example `[E_TSC_SYNC_UNSUPPORTED]` |
| H4 | Samples of the TSC against the host's monotonic clocks, with sleeps between them so the host's CPUs idle: 13 samples 10 s apart, or 3 samples 1 s apart with `--ci-schedule`. Each sample reads the TSC between two reads of a clock, keeping the tightest of 64 brackets; its uncertainty is half the bracket plus half the clock's resolution. A clock that returns the same value to consecutive reads is coarser than one read, so the probe takes its smallest step as its resolution: Hyper-V's reference TSC page advances the Linux clocks in 100 ns steps although `clock_getres` reports 1 ns. The interval stability is judged against a clock that time synchronization never steers, `CLOCK_MONOTONIC_RAW` on Linux and `QueryPerformanceCounter` on Windows: every interval between consecutive samples must be conclusive within 0.25 ppm, and the interval rates must agree within 1 ppm. chrony's frequency updates move `CLOCK_MONOTONIC`'s rate by up to several ppm between seconds on the Azure runners, which says nothing about the TSC. The rate over the whole window is measured against the disciplined clock, `CLOCK_MONOTONIC` on Linux, and must lie within 100 ppm of the rate H3 reports when H3 runs in the same invocation. A Linux host's clocksource is recorded as evidence |
| H5 | Pinned-thread ping-pong rounds over every pair of host CPUs; `max_abs_offset_ns` is at most 1,000, the measurement is conclusive, and no pair stalls |
| H6 | A microVM with the largest supported vCPU count up to 8 reports a valid `NVX-TIME-ABI` boot line through `nvx-time status` and runs `nvx-time-probe warp` over every CPU pair five times, with all vCPUs halted for 0.1, 1, 5, and 1 s between the runs, so that a host without an invariant TSC corrects the guest TSC as idle host CPUs wake (#265); then a 1-vCPU microVM runs it once. Every run stays within 1,000 ns. `--ci-schedule` runs CI's schedule instead: two runs 1 s apart |
| H7 | `adjtimex` reports no `STA_UNSYNC` and no `TIME_ERROR` on Linux; `w32tm /query /status` names a synchronized source on Windows. When H7 can't read either, its detail starts with `cannot verify host UTC synchronization`: `adjtimex` fails or reports a clock state Linux doesn't define, or `w32tm` fails or prints no English `Source` and `Leap Indicator` lines, as in a Windows display language other than English |

H2, H4, and H5 use a dependency-free host probe,
[`host_time_probe.rs`](../scripts/nvx_tools/host_time_probe.rs), which the
doctor builds with `rustc` once per source version into
`$RUNNER_TOOL_CACHE/nvx-host-time-probe` (or `build/host-time-probe` outside
CI). H2's profile check, H3, and H6 need the OpenVMM binary, and H3 and H6
also need the guest artifacts; `--openvmm`, `--kernel`, and `--initrd`
override their default build paths. `--no-openvmm` qualifies a host without
OpenVMM: H2 then checks the CPU identity and generation only, and H3 and H6
cannot run.

The doctor gates on measured properties, alike on every backend: the guest
warp probe at 1 µs with its idle gaps (H6), the TSC rate stability (H4), and
the CPU profile (H2 and H3). The host OS's invariant-TSC flags and clocksource
are evidence only in the doctor. In CI, `validate-runner` keeps the
`nonstop_tsc` gate on Linux runners in front of it, because only the microVM
jobs run H6, and runs the cheap host-level checks H1, H2, and H4 in every job
that uses it, on the short `--ci-schedule`. The microVM and platform jobs run
it after downloading OpenVMM and the guest artifacts and add H2's CPU profile
check and H3, so that H4 also compares the measured rate with the backend's
native rate; the other jobs pass `--no-openvmm`. The guest warp probe needs a
time ABI boot, so the microVM boot and restore scenarios run CI's warp
schedule after every boot and restore and assert its verdict. H5 and H7 remain
available for interactive qualification.

## Rust crate

The required `aci-edge-sandboxes` job checks the [`aci_edge_sandboxes` crate](../aci_edge_sandboxes/README.md) on
GitHub-hosted Ubuntu and Windows runners through the
[`check-aci-edge-sandboxes`](../.github/actions/check-aci-edge-sandboxes/action.yml) action. The action
runs rustfmt, Clippy with warnings denied, and rustdoc, all on the Rust
toolchain that MXC pins. It also runs the unit, mock, and fake-OpenVMM
integration tests, checks the declared minimum Rust version, and cross-checks
the macOS build that MXC compiles. The fake-OpenVMM tests drive the real
OpenVMM backend through its control protocol without a hypervisor.
The action also validates bundled artifact staging with a fixture package.
The development release job depends on `aci-edge-sandboxes` and requires its combined
Linux/Windows result to be successful; failed, cancelled, or skipped crate
checks cannot publish a release.

Each `nvx-microvm-tests-{kvm,mshv,whp}` job then runs
`nvx.py test-aci-edge-sandboxes` on its self-hosted runner; the debug-kernel
jobs skip it, because it boots the production kernel. This command drives a
complete provision, start, exec, stop, start, and deprovision cycle of the
Alpine guest with the crate's OpenVMM backend. It also checks cancellation,
that guest state lasts only until a stop, that a start terminates the VM of an
interrupted earlier start, host path mappings, egress rules, and per-execution
environments and working directories. On failure
it keeps the OpenVMM log under `build/test-results/aci-edge-sandboxes-<backend>`, which is
uploaded with the other microVM logs. Changes under `aci_edge_sandboxes/` therefore trigger
the backend matrices.

## Adversarial campaigns

The separate
[`adversarial.yml`](../.github/workflows/adversarial.yml) workflow runs
Copilot-driven campaigns only on trusted manual dispatches or schedules from
`dev`. It is not part of pull-request CI. The workflow's dedicated
`nvx-adversarial-controller` runner must already have an authenticated Copilot
CLI and an administrator-owned executor wrapper named by the
`NVX_ADVERSARIAL_EXECUTOR` repository variable. The workflow does not install
Copilot or initiate login.

The wrapper provisions a distinct disposable KVM, MSHV, or WHP target with no
production or GitHub credentials and forwards only the typed executor
protocol. Existing persistent microVM and performance runners are not valid
adversarial targets. Loss of the target heartbeat, a policy oracle, or a
teardown/post-campaign boot failure fails the job and requires quarantine and
reimage.

Normal Actions artifacts contain only the guest-text-free public summary,
catalogued case identifiers, and replay manifest. The external provisioner
must collect controller transcripts and complete target logs into
access-controlled security storage. See
[Copilot-driven adversarial testing](design/copilot-adversarial-testing.md)
for the architecture and operational contract.

## Copilot environments

[`copilot-setup-steps.yml`](../.github/workflows/copilot-setup-steps.yml)
prepares the GitHub-hosted runner of Copilot cloud agent sessions, including
sessions started from the GitHub web interface. The session's 59-minute limit
includes these steps, so they only install pinned tools, restore caches, and
stage downloads; they never compile NVX or OpenVMM. The workflow initializes
the public OpenVMM submodule without a deploy key, grants access to the
runner's `/dev/kvm`, installs the Python development tools into a virtual
environment on the image's Python 3, which `validate-nvx` also uses, and
installs Rust and cargo-nextest at the versions that
`check-aci-edge-sandboxes`, the crate manifest, and the Linux runner bootstrap
pin. It restores the shared guest artifacts
through the `restore-only` input of
[`build-guest-artifacts`](../.github/actions/build-guest-artifacts/action.yml)
and the KVM OpenVMM binary that `build-openvmm` caches for the pinned revision,
so agents can run microVM tests without rebuilding either. The guest artifact
restore computes its keys with the same action that saves them. The binary
restore repeats the key, paths, and gzip compression of the cache entry that
`build-openvmm` saves, and a test in `scripts/test_nvx_tools.py` fails when
they differ, because a mismatch would otherwise only cause a silent cache miss.
Because the agent firewall blocks `cdn.kernel.org` and `cdimage.ubuntu.com`,
even inside containers, the workflow also installs the native guest build
prerequisites, allows the unprivileged user namespaces that Alpine's `apk` uses
for package triggers, and stages the pinned Linux and Ubuntu Base archives, so
agents can rebuild guest artifacts with `build-guest --native`. When a runner
has a separate `/mnt` disk with more free space than `/`, it mounts the
workspace there. A failed step would make Copilot skip every later setup step,
so each step after checkout continues on error. The last step sets
`NVX_COPILOT_SETUP` to `complete` or `incomplete`, lists failed step IDs in
`NVX_COPILOT_SETUP_FAILED`, and fails the run when setup is incomplete.
Copilot always runs the version on `dev`, even for sessions based on other
branches, so changes reach agent sessions only after they merge. Pushes run it
as a normal workflow for validation when they change the workflow or a file
that its steps take versions, requirements, metadata, or code from: the
`build-guest-artifacts` action, the files that pin its Rust, cargo-nextest,
and shell linter versions, `requirements-dev.txt`, `SOURCE-MANIFEST.json`,
`.gitmodules`, the OpenVMM submodule pin, and the NVX CLI and its modules.

Copilot code review uses
[`copilot-code-review.yml`](../.github/workflows/copilot-code-review.yml)
instead. Reviews do not build code, so it only checks out the repository and
installs the `gh-aw` extension for the MCP server in `.github/mcp.json`.

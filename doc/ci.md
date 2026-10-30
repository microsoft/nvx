# Continuous integration

The GitHub Actions workflow has two microVM test layers on Azure-hosted
self-hosted KVM, MSHV, and WHP virtual machines. Each backend has a pool of
runners labeled by operating system, backend, and `virtual-machine`, plus one
of two mutually exclusive roles; see
[Job graph and runner capacity](#job-graph-and-runner-capacity) for their
sizes. Every runner but one per backend carries `general`, which every
non-benchmark backend job requires, so those jobs can use any of them without
binding a workload to a specific host. The remaining runner carries `benchmark`
instead of `general` and is dedicated to the platform job. This keeps unrelated
long jobs from delaying the platform measurement and makes every run measure
its performance series on the same VM
(see [Benchmarks](benchmarks.md#ci-collection)). Before it measures, each
platform job also checks that its runner is still the host that the series'
history comes from (see
[Expected benchmark hosts](benchmarks.md#expected-benchmark-hosts)). The OpenVMM VMM tests download only the NVX
Linux-direct kernel and Alpine initramfs (see
[Guest artifact handoff](#guest-artifact-handoff)) and use them to
exercise OpenVMM's Linux MP-table lifecycle, TTRPC, and snapshot contracts on
each backend. `openvmm-unit-tests` runs the OpenVMM unit and documentation
tests once per operating system, on a GitHub-hosted Ubuntu runner and a WHP
runner. The tests that need `/dev/kvm` or `/dev/mshv` are ignored, so the KVM
and MSHV runners used to run the same 4,820 tests, and the Linux leg needs no
hypervisor runner; see
[Job graph and runner capacity](#job-graph-and-runner-capacity).
Failed VMM-test jobs, which [OpenVMM VMM tests](#openvmm-vmm-tests) describes,
upload Petri's `test_results` directory,
including guest and VMM logs, screenshots, and watchdog inspection data.
These seven-day artifacts are named
`openvmm-vmm-tests-<os>-<backend>[-shard-<n>]-<run-id>-<run-attempt>`, where
each shard of the WHP tests adds `-shard-<n>` (see
[OpenVMM VMM test shards](#openvmm-vmm-test-shards)), so a successful rerun
does not replace the failed attempt's diagnostics. Each job collects them from
`<runner-temp>/openvmm-vmm-tests/test_results`, in the test content that it
runs. Every VMM-test job also uploads the
[test duration report](usage.md#test-duration-report) of its run, whether its
tests pass or fail, as the 30-day
`openvmm-vmm-test-durations-<os>-<backend>[-shard-<n>]-<run-id>-<run-attempt>`
artifact. A job that is cancelled usually ends before its run writes one, so
its upload step only warns.
Two jobs per backend exercise Linux, SMP, virtio, sandbox, and snapshot
behavior through the public OpenVMM CLI, on the NVX Linux kernel plus the
selected Alpine, Ubuntu, or Azure Linux initramfs. Both run on the backend's
`general` runners, with the same OpenVMM and `nvx-rs` builds. The
`nvx-microvm-tests-{kvm,mshv,whp}` jobs run the default suite on the Alpine
initramfs, each in shards of about equal duration that run in parallel, one
runner each (see [MicroVM test shards](#microvm-test-shards)). Beside them,
the `nvx-microvm-guests-{kvm,mshv,whp}` jobs run the
guest smoke and sandbox steps, which need no result of that suite: the public
managed sandbox acceptance, the Ubuntu guest tests, the one-vCPU Azure Linux
smoke set, the Ubuntu sandbox steps, and the runtime adapter scenarios. Only
those steps read the Ubuntu and Azure
Linux artifacts, so only the guest and sandbox jobs wait for them (see
[Guest artifact handoff](#guest-artifact-handoff)). Every guest runs under the
same time ABI checks. Alpine-control-only scenarios remain explicit and are
rejected for the Ubuntu initramfs. The debug-kernel jobs skip the Ubuntu and
Azure Linux tests. Each job uploads its failure logs under a name of its own:
`nvx-microvm-tests-<backend>-shard-<n>` for shard `<n>` of the default suite,
`nvx-microvm-tests-<backend>-guests` for the guest and sandbox steps, and
`nvx-microvm-tests-<backend>-debug` for the debug kernel.
Every microVM job, the guest and sandbox and debug-kernel ones and each shard
too, also uploads the
[scenario duration report](usage.md#scenario-duration-report) of each
`test-microvm` step, whether the job passes, fails, or is cancelled. The
30-day artifact is named
`nvx-microvm-durations-<os>-<backend>-<kernel>[-guests|-shard-<n>]-<run-id>-<run-attempt>`,
where `<kernel>` is `default` or `debug`, the guest and sandbox jobs add
`-guests`, and the shards `-shard-<n>`, so that each job and attempt keeps its
own.
Every harness launch, in the tests and the benchmarks, scans the OpenVMM
console for the guest's [time ABI](design/3.2-clock-and-time.md) output, and the tests
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
only scanned: one-shot workloads, and the managed lifecycle, where init starts
the managed agent instead of a shell. A failing boot check still powers them
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
The guest's terminal also echoes console input that arrives while a command
runs into whatever that command prints (#785), so the test runners never
type a guest script line by line. They stage it whole in a file with a
here-document, then run it with the last input line. When a scenario parses
the script's output, that line first prints `NVX-GUEST-SCRIPT-START` and then
sources the script into the shell, and the scenario reads only what follows.
A staged script that waits for a line from the host turns the terminal's
echo off before it announces that it waits.
The `smp`, `smp-snapshot`, and `restore-processors` scenarios run the guest
warp probe, `/sbin/nvx-time-probe warp --bound-ns 1000`, over every pair of
online CPUs after each boot or restore, in two rounds with all vCPUs halted
for 1 s between them, so that a host without an invariant TSC corrects the
guest TSC as idle host CPUs wake (#265). Every pair in every round must stay
within the time ABI's 1 µs
[cross-vCPU skew bound](design/3.2-clock-and-time.md#327-cross-processor-consistency) for both the
backward TSC step and the ping-pong offset; a stalled pair or an inconclusive
measurement also fails. `smp` boots each requested processor
count. `smp-snapshot` captures one snapshot per requested count and restores
it once, and the first snapshot a second time to prove that a restore leaves it
reusable. `restore-processors` captures one boot-online CPU with capacity 8 and
probes the CPUs that each restore target activated. After the 1/2/4/8-CPU
restores, it restores the same snapshot once without `--restore-processors`.
`platform-restore-processors` does the same for a platform snapshot of
capacity 4, with the requested counts up to 4: its capture boots with
`maxcpus=1` and sandbox blocks, each restore must also report the platform
repair, and OpenVMM must refuse the counts 0, 3, and 8 before the guest runs.
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
The `time-abi-conformance` scenario boots the largest requested vCPU count and
runs the guest's exhaustive CI check, `/sbin/nvx-time exhaustive`, which the
boot check leaves to CI. On every online CPU it checks every leaf
`0x40000006..=0x400000ff` and every base `0x40000100..=0x4000ff00` for another
hypervisor signature, every identity MSR that the boot check reads, the write
rules of `HV_X64_MSR_TSC_INVARIANT_CONTROL`, writes to the read-only identity
MSRs, and reads of `IA32_TSC_ADJUST` and `IA32_TSC_DEADLINE` (checks `X1` to
`X6`). The harness requires a passing `NVX-TIME-ABI-EXHAUSTIVE` line for every
check on every CPU, a summary with `status=ok`, the requested CPU count, and
no failures, and exit status 0; a failure lists each failing check with the
guest's detail. The guest command fits on one console line, so the console's
echo of it ends before the check prints.
The `cpu-profile-fallback` scenario covers `--cpu-profile auto`'s fallback to a
host profile on a CPU that no built-in profile serves, on runners that
built-in profiles serve. It cold boots one vCPU with OpenVMM's
`host-cpu-unknown` test hook, which makes `auto` treat the host CPU as
unknown, and requires exactly one `NVX-CPU-PROFILE-FALLBACK:` warning that
names a host profile with its digest and links the CPU profile request form,
a cold boot that declares that profile, and passing boot checks. It then
captures the guest and restores it with `auto` and no hook: the restore must
take the snapshot's host profile without another warning, and pass the warp
probe and its restore checks.
The `snapshot-core` scenario first sends a snapshot request to a guest that
OpenVMM launched without a snapshot destination. OpenVMM releases the request,
so it returns in the source, which must continue exactly once. Before the
request, the guest's snapshot agent saved and overrode the stall detectors'
settings, and it must restore them when the request returns
(`nvx-time cancel-capture`). Afterwards, `nvx-time status` must exit 0 with a
passing boot line at generation 0 and no restore line, and
`/sys/module/rcupdate/parameters/rcu_cpu_stall_suppress` must read 0.
The `platform-zero-fill` scenario covers a platform restore whose `distro`
image is shorter than the snapshot's and whose fresh scratch is absent. It
captures a platform snapshot with a 1 MiB `distro` placeholder and a 1 MiB
scratch disk, then restores it with a 601-sector `distro` image, which ends
inside a 4 KiB page, and `--restore-absent-scratch`. Reading the devices
rather than its page cache, the restored guest must find a 1 MiB `distro`
device that returns the image and then zeros, including in the page across
the image's end, and a writable 1 MiB scratch device that returns zeros and
fails a write with an I/O error. Before the guest runs, OpenVMM must refuse a
`distro` image longer than the snapshot's, an absent scratch together with a
scratch disk, a platform restore with neither, and `--restore-absent-scratch`
on a cold boot, and no restore may change the image or the snapshot.
`scratch-snapshot` also requires OpenVMM to refuse `--restore-absent-scratch`
for its `workload-start` snapshot, whose scratch is paired, because only a
platform snapshot's fresh scratch may be absent.
The `platform-network-policy` scenario covers a NIC template, a platform
snapshot whose NIC was captured with the complete deny-all policy
`--network-egress deny --host-loopback deny`. It restores the template with
`--restore-network-policy` four times: under the template's own deny-all,
under egress rules that admit one host service, with explicit host loopback, a
forward, and private-network ingress, and with a proxy at the other host
service. Each clone must keep the template's MAC, IPv4, and IPv6 addresses,
reach through its gateway exactly the host-loopback services that its policy
admits, which must receive no other request, and serve the host through the
forward only when its policy has one. Before the guest runs, OpenVMM must
refuse a restore of the template that keeps its policy but passes another or
explicit host loopback, a new address, a proxy that is not the gateway, a
forward whose host port another socket holds, the option on a platform
snapshot captured with plain deny-all egress, which leaves host loopback
allowed, and the option on a cold boot, and no restore may change the
template.
The `platform-filesystem` scenario covers a share that a platform restore
attaches to the filesystem slot that the capture left dormant, as a clone's
export does. It captures a platform snapshot without `--mount` and restores it
with a `--mount` of a host directory. A tiered restore starts the clone's
devices behind the post-restore input gate, so the restored guest uses the
share only after its platform repair has acknowledged the restore: it must
mount the share, read the file that the host wrote, and write one that the
host then reads, and the restore may not change the snapshot. Until OpenVMM's
virtio-fs queue workers waited for the gate to release (#904), the clone's
mount succeeded but its first read never returned.
The `platform-aggregate-export` scenario covers an aggregate of host
directories and files that a platform restore attaches to the filesystem slot
that the capture left dormant, as a clone's export does. It captures a
platform snapshot without a filesystem and restores it with the aggregate of
the `filesystem-shares` scenario: a read-write workspace and a read-only tool
cache, each with a denied path, a read-write data directory whose root exposes
only its allowed paths and whose notes alone are writable, and a read-only and
a read-write file. The clone keeps its captured command line, which names no
filesystem, so its guest must mount the aggregate itself, find the five
children in its read-only root, which only root can enter, check each child's
kind, mode, and policy, and bind a directory and a file child, and the host
must then find exactly the writes that the policy admits. Before the guest
runs, OpenVMM must refuse the restores whose aggregates a cold boot refuses,
with overlapping directories, a file inside a directory child, a child of the
wrong kind, a repeated name, a policy path of a file child, one outside every
child, or a relative one, and on a Windows host caller ownership, and no
restore may change the snapshot. `filesystem-snapshot` also requires OpenVMM
to refuse an aggregate for the dormant slot of its untiered snapshot, which
binds only a `--mount`, because only a platform snapshot's dormant slot binds
an aggregate.
The `filesystem-snapshot` scenario's blockless capture of a live share covers
the write-back of shared mappings (#555). A guest process dirties a page of a
file in the share through a shared mapping with `nvx-device-io map` and holds
the mapping through the capture, so the page reaches the host only through the
guest agents' write-back: after the capture, the host file must hold the
guest's write. The host then changes the file in place before each of two
restores, and each restored guest, which unmaps the page and reads the file,
must leave the host's change in place: a capture that kept the page dirty
would make every restore write the guest's capture-time data back.

At the end of a passing run, `test-microvm` prints one `NVX-TIME-ABI-EVIDENCE:`
line and adds it to the GitHub job summary, because CI keeps the guest logs only
for failed jobs: the number of warp probe runs with their worst
`max_abs_offset_ns` and `max_backward_ns`, the count and `elapsed_us` range of
the newest check that each `nvx-time status` query reported (boot after a cold
boot, restore after a restore, and capture after `snapshot-core`'s released
request), the exhaustive check's summary, and the
`restore-downtime` stall counts. Where the guest reports a check's CPU time
(`cpu_us`), the line also gives its range and how many checks exceed the
backend's CPU-time budget for their phase. The budgets come from guest
measurements and exempt the first capture after a rolled-back capture in the
same VM process; CI never makes one, because each VM process sends OpenVMM at
most one snapshot request and a failed capture fails its scenario.
`CHECK_CPU_BUDGET_US` in `scripts/nvx_tools/time_abi.py` holds the budgets, one
per backend and phase. `elapsed_us` is wall time, including waits behind the
workload, and has no budget. CI gates on neither: the performance gate is the
A/B comparison outside CI.

The `nvx-microvm-debug-{kvm,mshv,whp}` jobs run `test-microvm --debug-kernel`
on the CI debug kernel (`build/vmlinux-debug`, built from
`kernel/config-microvm-debug`), whose soft-lockup and hung-task detectors
production kernels leave out. It selects the same-host restore scenarios
`smp`, `smp-snapshot`, `restore-processors`, `restore-downtime`, and
`snapshot-tiers`. The jobs skip every other test step, because those boot the
production kernel, which the `nvx-microvm-guests-{kvm,mshv,whp}` jobs already
cover: the public managed execution configuration, the Ubuntu and Azure Linux
guest tests, the Ubuntu sandbox layer and live-share smoke tests, and the
caller-owned live-share test. Any
RCU stall, soft lockup, or hung task makes the guest's time ABI watcher power
off with status 194, which fails the run. Every guest those scenarios restore
asks `nvx-time status` before it exits
(`snapshot-tiers` after its tier assertions), which waits for the restore's
deferred checks and must report a passing restore line, so no restore leaves
its checks pending. The one exception is `snapshot-tiers`' gate-timeout check,
whose guest OpenVMM stops at the restore gate on purpose. The harness refuses
a kernel whose `vmlinux-debug.config` lacks the detectors, because the guest's
boot check of those detectors passes vacuously. To bound the cost, pull requests
and merge groups run the debug kernel on KVM only, while `dev` pushes and full
runs run it on every backend; each job takes about five minutes on its own
runner, in parallel with the other microVM jobs. The jobs gate the required status check, the
development release, and performance persistence. The GitHub-hosted `debug-kernel` job
builds the debug kernel beside the other guest artifact jobs
(`build-guest-artifacts` with `components: debug-kernel`) and caches it under its
own key, so a kernel rebuild delays only the debug jobs, and a failed debug
kernel build fails the required status check and blocks the release. The
debug jobs also wait for `artifacts`, but not for the Ubuntu and Azure Linux
images (see [Guest artifact handoff](#guest-artifact-handoff)).

Every job that uses the `validate-runner` action first ends, on Linux, any
OpenVMM process that its runner's account still runs, with a warning that
names it, and fails if one survives SIGKILL or the processes can't be listed.
A runner runs one job at a time, so such a process is left from an
earlier job. Before a cancellation reached the test harness (see
[Cancellation](#cancellation)), the harness of a cancelled job kept running
and could start a VM after the runner's orphan cleanup had listed the job's
processes; the cleanup then killed the harness, and the VM ran on for 8 days
(#768). It still ends a VM that escapes both the harness's cleanup and its
parent-death signal. The action next removes the account's `nvx-*` and
`openvmm-*` directories in Python's temporary directory, where the test
harness runs its scenarios, with a notice that names each one, and fails if it
can't list them: a harness that something killed outright left them behind,
and on October 28, 2026, the ten Linux runners held 64 of them (#946). It then
reports the Linux runner's kernel, CPU, clocksource, and TSC flags, and
qualifies the runner
for the time ABI with `nvx.py doctor --checks H1 H2 H4 --ci-schedule` (see
[Host qualification](#host-qualification)): the backend, the CPU fingerprint
and generation, and the TSC rate stability on its short schedule. The microVM
and platform jobs run it after downloading OpenVMM and add OpenVMM's CPU
profile check (H2) and preflight (H3), and keep the fingerprint as an artifact
when the profile check fails; the other jobs pass `--no-openvmm`. This takes a
few seconds, and up to a minute more when H4 re-reads chronyd's frequency
estimate. It reports the CPU generation, the built-in CPU profile that
`auto` selects, and the measured TSC rate in the log and the job summary, and
fails the job with a stable code when the runner is not qualified, for example
`E_PROFILE_HOST_UNKNOWN` on an unknown CPU generation, where `auto` would fall
back to a host profile, which CI never uses. The platform jobs also
run the guest warp probe (H6) on CI's schedule, which adds about 25 s, because
their benchmarks boot guests without the microVM scenarios' probes. The doctor
gates only on measured properties, alike on every backend: it records the host
OS's invariant-TSC flags and clocksource as evidence, and the guest warp probe
measures the skew that a host without an invariant TSC causes. Before the time
ABI, never-restored guests on such an MSHV runner VM, an Ice Lake 8370C, hit
cross-vCPU TSC warps when an idle host CPU woke (#211, #265), which is why the
probe schedules include idle gaps. Under the time ABI, its guests' warp probes
measure at most 36 ns, so `validate-runner` only notes a missing
`nonstop_tsc`, and the Linux runner setup script only warns. The OpenVMM
vmm-tests run no warp probe: they boot OpenVMM's own test guests, whose
verdicts don't depend on the skew bound. Runner labels do not encode the
generation; per-PR CI captures and restores on one runner, so generations
never mix.

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

The `command-line-limit` scenario covers the x86 guest kernel's 2,048-byte
command-line buffer. OpenVMM appends its own tokens after the caller's, ending
with the network bootstrap, so they are the first that the kernel would cut
from a command line that doesn't fit, and OpenVMM refuses an effective command
line longer than 2,047 bytes instead. With a NIC attached, the scenario boots
once with a one-byte padding token to learn the effective command line from
the guest, then pads the caller's tokens until it holds 2,047 bytes. The
guest's `/proc/cmdline` must equal the first one with the longer padding, and
the guest must have installed the IPv6 default route that the last token,
`virtnet_gw6=`, names. OpenVMM must refuse a command line one byte longer
before boot, with an error that names its length and the limit.

Shared guest artifacts are built with Docker on a GitHub-hosted Ubuntu runner.
The kernel, Alpine initramfs, Ubuntu initramfs, and Ubuntu EROFS layer use
separate cache keys. The EROFS layer's entry also holds the sandbox scratch
template, which the layer's Docker target formats in the pinned Debian image
with [`build-scratch-template`](usage.md#build-scratch-template). Every guest
image key includes the shared binary-build
modules, the Dockerfile, and `.dockerignore`; the Rust builder image itself is
pinned by digest. Every guest image key also ends with the path-aware digest
of the Rust guest binaries' inputs that the guest package manifests record,
which `nvx.py guest-binary-source-identity` prints, because the images carry
only those binaries of `nvx-rs` and `hashFiles` hashes contents only. The
inputs are the files that the guest binaries' build reads:
`rust-toolchain.toml`; the crate's manifest, lockfile, build script, and Cargo
configuration; every file under `nvx-rs/src/` but the host side, which is the
`host` and `testing` modules and the `nvx-rs` and `nvx-fake-openvmm` binaries,
and the test modules, which are the `tests.rs` files, the `tests` directories,
and the managed agent's `loopback` harness; and the two Python modules that
define the build. `GuestRustBuildConstants` in
[`build_constants.py`](../scripts/nvx_tools/build_constants.py) selects them,
and every guest build fails when Cargo's dep-info for the nine executables
lists another file, such as a host module that guest code starts to compile.
A change to the host side or a test of `nvx-rs` therefore rebuilds no guest
image, and a change to any file that a guest binary compiles rebuilds them
all. Ubuntu keys also include the Canonical archive pin, supplemental
package lock, common guest sources, shared download and guest descriptor
modules, and converter implementation. Each component is handed to the jobs
that read it as a workflow artifact of its own, under its build filenames (see
[Guest artifact handoff](#guest-artifact-handoff)). Each
backend also boots the Ubuntu initramfs and runs
`/sbin/nvx-sandbox-smoke` from the Ubuntu EROFS layer as UID/GID 65534 over a
fresh ext4 scratch copy, with a 32 MiB `--memory-max` and an 8-process
`--pids-max`. The probe verifies the
[sandbox security profile](run.md#experimental-single-workload-sandbox): the
fixed identity, empty capability sets and `no_new_privs`, private mount, PID,
and UTS namespaces, the private device view and read-only sysfs, the
container cgroup, the overlay root over read-only image layers with its upper
directory on scratch, and that the memory controller kills an allocation of
twice the limit and the pids controller stops the workload at eight
processes. The live-share runs below repeat the profile checks. Each
backend also runs the public `managed-exec-config`, `sandbox-lifecycle`,
`sandbox-proxy-only`, and `sandbox-ram-upper` scenarios of `test-microvm`,
which drive `nvx-rs sandbox run`, `provision`, `start`,
`exec`, `stop`, and `deprovision` as subprocesses (see
[Run](run.md#managed-lifecycle)): request-scoped working directories and
environments, timeouts, nonzero exits, stdout and stderr, bounded output and
outcome reports, warm-guest state across requests and restarts, identity
refusal, fail-closed lifecycle transitions, a crashed OpenVMM's stale runtime
state, teardown that leaves no OpenVMM process, control endpoint,
capability, or runtime record while the layer and scratch artifacts survive,
a proxy-only network, whose guest reaches a test proxy on host loopback at
its gateway and no other destination, and sandboxes whose overlay upper is a
tmpfs in guest memory, with no scratch image.
Their bounded evidence is part of the backend's failure logs. The same
entrypoint then verifies a live virtio-fs
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
attaches a read-write `/workspace` share, a read-only `/opt/hostedtoolcache`
share, a read-only `/config/settings.json` file, and a read-write
`/results/output.txt` file to one sandbox at the same time, as the children
of one aggregate virtio-fs device; each directory has its own denied
subdirectory, and each file lies beside a file that no share exposes. The
smoke script runs the read-write and read-only
checks on the two shares and verifies that a link in the read-write share
cannot write into the read-only share. It requires each file to be a
`virtiofs` bind mount with its mode, alone in its guest directory, the
read-only file to refuse writes with `EROFS`, and the workload to rewrite the
read-write file but neither remove nor rename either one. The host requires
the guest's writes in the workspace and the read-write file, an unchanged tool
cache, read-only file, and denied files, and unchanged files beside the shared
files, both for a
one-shot run and for a managed sandbox whose format-6 configuration must
persist every share and its kind and whose `stop` must leave a cleanly
unmounted scratch
filesystem. Each backend then attaches a read-write `/workspace` share whose
[access policy](run.md#access-policy) denies `nvx-denied`, allows
`nvx-denied/nvx-allowed`, and makes only `nvx-writable` writable, with every
host directory and file writable by every identity so that only OpenVMM can
refuse a write. The smoke script's `policy` mode requires the workload's
writes to succeed only in `nvx-writable` and to fail with `EROFS` everywhere
else, a hard link of a read-only file into `nvx-writable` to fail, and
`nvx-denied` to list only `nvx-allowed`, whose file stays readable while
`nvx-denied/secret` stays hidden. The host requires the guest's writes in
`nvx-writable` and no other change to the share, both for a one-shot run and
for a managed sandbox whose format-5 configuration must persist the denied,
allowed, and writable paths and whose `stop` must leave a cleanly unmounted
scratch filesystem. The default `filesystem-policy` scenario of `test-microvm`
covers the same policy as guest root, whose permission checks mode bits never
stop: every mutation outside the writable paths, through a second mount of
the tag as well, fails with `EROFS`, a hard link fails with `EXDEV`, a denied
path nested inside an allowed path stays hidden, invalid policies are rejected
before boot, and a snapshot restore requires the same policy and keeps a
writable handle open across the capture. On Linux, a
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
toolchain and in which every guest artifact builds,
[`.dockerignore`](../.dockerignore), which Docker applies to the build context
before `COPY`, and
[`build_config.py`](../scripts/nvx_tools/build_config.py) and
[`build_constants.py`](../scripts/nvx_tools/build_constants.py), so a toolchain,
shared build configuration, or constant change invalidates cached guest
artifacts and their provenance. The Ubuntu distro layer shares the Ubuntu input
hash.

The producer handoff uses one-day workflow artifacts rather than caches. Each
consumer downloads both the normalized executable and its build provenance,
then restores executable permissions on Linux.

The microVM test jobs run their sandboxes and microVMs with
[`nvx-rs`](../nvx-rs/README.md#the-nvx-rs-cli), and compile their egress
policies with it, so they take `nvx-rs` the same way, and every release
package ships it. Each NVX CLI job builds it for its operating system with
`--release --locked --features cli`, puts it at `nvx-rs/target/release/`,
where the microVM tests look for it, and runs the CLI tests with it, with `NVX_RS`
naming it for the tests that run it directly: `nvx-cli-linux` builds a static
`x86_64-unknown-linux-musl` executable, which runs on every self-hosted Linux
runner, and `nvx-cli-windows` an `x86_64-pc-windows-msvc` executable with a
static C runtime, which needs no Visual C++ redistributable. Each job records
the build's provenance with
[`record-nvx-rs-provenance`](usage.md#record-nvx-rs-provenance), and, once the
tests pass, uploads the executable and the provenance as the one-day
`nvx-rs-linux-musl-executable` and `nvx-rs-linux-musl-provenance` artifacts, or
the `nvx-rs-windows-msvc-*` pair. The workflows that take them name the pair by
its base name, `nvx-rs-linux-musl` or `nvx-rs-windows-msvc`, as they name the
OpenVMM artifacts. Each microVM test job of that operating system downloads
the executable into `nvx-rs/target/release/` and restores its executable
permission on Linux, where the scenarios and the sandbox steps run it. The
platform jobs run no sandbox or microVM with it, but their
[`package-release`](../.github/actions/package-release/action.yml) step
downloads both, the provenance into `build/`, so that each package ships the
`nvx-rs` of its operating system. The `managed-lifecycle` scenario also builds
and runs the ignored `nvx-rs` test `tests/managed_lifecycle.rs` with Cargo on
the runner.

Once the required artifacts are
ready, benchmarks run in parallel with the NVX test layer, each on its backend's
benchmark runner. All three use virtual-machine performance
series and the constrained eight-CPU affinity policy. Development releases and
performance baseline updates still require every applicable test and benchmark
lane to pass.
The microVM correctness jobs gate every use of benchmark results, so the
benchmark action runs no correctness scenario of its own (#286). `Required
status check` requires each `nvx-microvm-tests-*` and `nvx-microvm-guests-*`
job that the change schedules to succeed, and an `nvx-microvm-tests-*` job
succeeds only when each of its shards does. `Publish development release` and
`Persist performance baseline` run only on `dev` pushes in which every microVM
test job succeeded or was skipped. The pull-request `Performance regression
gate` reads only the platform jobs' results and publishes nothing. The
counting LAPIC that the benchmarks depend on is covered by the `smp` scenario
of those jobs.
`smp-lapic` repeats `smp` with the counting-LAPIC facts asserted, for explicit
local use only; no CI job runs it. [Benchmarks](benchmarks.md#ci-collection)
lists the jobs that consume, gate, publish, or persist the results. The workflow uses the read-only OpenVMM deploy key stored in the
`OPENVMM_DEPLOY_KEY` Actions secret to fetch the private submodule at its pinned
commit. Shared guest binaries and development release packages move through
short-lived workflow artifacts alongside the OpenVMM handoff and benchmark
results. Caches only accelerate reproducible build inputs and outputs; consumers
do not depend on them as a handoff.
Pull requests and merge groups gate regressions against recent
matching-platform history, and successful pushes to `dev` append their p50
values under `data/`. A pull request or a merge group that changes no input of
the platform jobs skips them and the gate
(see [Change classification](#change-classification)). Metadata-only
performance jobs use GitHub-hosted Ubuntu runners. Provisioning instructions
are in the [runner bootstrap guide](../scripts/setup/README.md).

Persistent runners accept pushes, merge groups, scheduled and manually
dispatched full runs, and same-repository pull requests only. Fork pull
requests run the GitHub-hosted validation jobs but do not execute code on the
Azure runner fleet. A maintainer must stage an external contribution on a
trusted repository branch before running the backend matrices. A merge group,
though, runs in the repository whatever repository its pull requests come
from, so queueing a fork's pull request runs its code on the fleet, as the
`dev` push that merged it would (see [Merge queue](#merge-queue)).

## Job graph and runner capacity

A `dev` push, a pull request that is ready for review, a merge group, or a
scheduled or manually dispatched full run starts these jobs at once: `quality`,
`nvx-rs`, `nvx-cli-linux`, and `nvx-cli-windows` on GitHub-hosted runners, and
`openvmm-changes`, which classifies a push, pull request, or merge group and
selects every suite for a full run (see
[Change classification](#change-classification)). A merge group also starts
`cancel-superseded-merge-groups` on a GitHub-hosted runner, which nothing
waits for (see [Superseded merge groups](#superseded-merge-groups)). When the
classification finishes, the jobs that it selects start: `artifacts`,
`distro-artifacts`, and `debug-kernel` restore or build the guest artifacts
on GitHub-hosted runners (see
[Guest artifact handoff](#guest-artifact-handoff)), the three OpenVMM binary
producers restore or build OpenVMM on a KVM, an MSHV, and a WHP runner,
`build-openvmm-vmm-tests-linux` and `build-openvmm-vmm-tests-windows` restore
or build the Linux VMM tests on a KVM runner and the WHP ones on a WHP runner
(see [OpenVMM VMM test cache](#openvmm-vmm-test-cache)), and
`openvmm-unit-tests` starts on a GitHub-hosted and a WHP runner.
`openvmm-vmm-tests-linux` waits for `artifacts`, which builds the kernel and the
Alpine initramfs, and the Linux VMM-test build, and `openvmm-vmm-tests-windows`,
whose legs run the [shards](#openvmm-vmm-test-shards) of the WHP tests, for
`artifacts` and the Windows VMM-test build. Each microVM test and platform
job waits for `artifacts`, its OpenVMM producer, and the NVX CLI job of its
operating system. The microVM guest and sandbox jobs and the platform jobs
also wait for `distro-artifacts`, and the debug-kernel jobs for
`debug-kernel`. `Performance regression gate`,
`Publish development release`, `Persist performance baseline`, and `Required
status check` wait for the jobs whose results they check, and in a full run of
`dev`, `Package nvx crate` waits for `Required status check` and
`Publish nvx crate` for `Package nvx crate` (see
[crates.io releases](#cratesio-releases)). Required handoffs
between these jobs use same-run workflow artifacts; caches only skip
rebuilding their inputs.

For a same-repository pull request, `guest-artifact-reuse` also waits for
`artifacts` and `distro-artifacts`, and rehearses the
[guest artifact reuse](#guest-artifact-reuse) of the push that merges it;
only `Required status check` waits for it.

The NVX CLI tests do not depend on the backend, so they run once per operating
system in `nvx-cli-linux` and `nvx-cli-windows` rather than in every
hypervisor job. Each checks out OpenVMM, whose pinned CPU profiles a test
reads, and uses Python 3.12, the release that the self-hosted runners provide;
the Windows job also installs the pinned Rust release with the minimal profile
for the curl shim test and the `nvx-rs` build. The tests that run Windows
PowerShell, or PowerShell 7 where it is installed, give a launch 120 s by
default, `POWERSHELL_TIMEOUT` in `scripts/powershell_testing.py`: a launch
takes a few seconds at most, but a GitHub-hosted runner has held one up for
more than 30 s (#1030). Only the storage events test, whose save step queries
the host's event logs for up to 120 s each, gives its launches 300 s. A launch
that outlasts its limit fails its test with what PowerShell printed until the
test killed it, which the test also writes to stderr at once: unittest reports
failures only once the run ends, which a job that reaches its own time limit
never does. Each job first builds the
`nvx-rs` executable that the microVM tests run, tests with it, and hands it to the
microVM test and platform jobs of its operating system, as described above.
Each also runs the ignored `nvx-rs` test `tests/openvmm_vectors.rs`, which
compares the submodule's control-session protocol vectors, and the lines of its
platform allowlist of kernel command-line tokens, with the copies that the
protocol and template tests read in the `nvx-rs` job, which cannot check out
OpenVMM, and the versions of OpenVMM's snapshot format, saved-state schema,
time ABI, and restore packet with those of `nvx::contract`;
see [OpenVMM's vectors](../nvx-rs/tests/vectors/README.md#openvmms-vectors).
The ignored test `tests/openvmm_cpu_profiles.rs` likewise compares the
submodule's built-in CPU profiles with the copy of their identities from which
`nvx-rs run` guides a failed cold boot on a CPU that none serves.
Fork pull requests lack the OpenVMM deploy key, so the CLI jobs run, like the
hypervisor jobs, only for pushes, merge groups, full runs, and same-repository
pull requests.
Because the microVM and platform jobs of each operating system wait for its CLI
job, a broken CLI does not occupy the hypervisor runners.
`Required status check`, the development release, and performance persistence
require both CLI jobs.

The KVM and MSHV pools have five runners each, and the WHP pool has seven. In
each pool, one runner carries `benchmark` and runs only the platform job, and
the others carry `general` and run all non-benchmark backend jobs. Platform
jobs from overlapping workflow runs still measure one after another on their
backend's single benchmark runner, but they never wait behind an unrelated
build or test.

The role split follows an October 15-20, 2026 scheduling audit, when each pool
had five runners. Before the split, the benchmark runners spent about one third
of their assignments on non-platform work; `azure-windows-5`, for example, ran
102 platform jobs and 62 build or test jobs. Platform jobs queued for p50 6.3,
p90 22.2, and at most 34.1 minutes on WHP, for p90 15.7 and at most 21.1
minutes on KVM, and for p90 11.2 and at most 18.7 minutes on MSHV. The WHP
platform job finished last in about a third of `dev` pushes and nearly half of
full pull-request runs. Replaying the observed arrivals and durations with
dedicated benchmark runners reduced WHP platform wait to about p90 3.6 and at
most 8.4 minutes.

The WHP jobs are the longest: the Windows VMM-test build and the OpenVMM unit
tests compile OpenVMM on Windows for about 15 and 13 minutes, so overlapping
runs queue on WHP first. Dedicating the benchmark runner takes it out of its
pool's general capacity. In the replay, which left four general WHP runners,
the WHP non-benchmark p90 wait rose from about three to five minutes, while the
KVM and MSHV non-benchmark waits stayed near zero. The WHP pool has since grown
to six general runners, more than the five that served these jobs before the
split. A change that adds a job or a shard needs general runners to run it in
parallel; otherwise it only queues.

The OpenVMM unit tests need no hypervisor. In October 2026 (#697), on
GitHub-hosted runners, which have four vCPUs and keep no sccache, the Linux
suite took 12 to 18 minutes from scratch on four runners, against 6.5 on an
MSHV runner, and the Windows suite took 32 minutes from scratch and 21 with a
warm sccache, against 15 on a WHP runner. Every hosted run passed the same
5,294 Linux or 4,933 Windows tests as the self-hosted runners. The Linux leg
therefore runs on `ubuntu-latest`: even at its slowest, it would have finished
before the rest of the run in each of the 208 successful runs that ran the
suite from October 8 to 21, and it leaves about 900 MSHV runner-minutes a week,
about 15% of that pool's busy time, to the jobs that need a hypervisor. It
installs the pinned Rust release and cargo-nextest, keeps Cargo's home in the
job's temporary directory, as the self-hosted runners do, and skips
`validate-runner` and sccache, which only they have. The Windows leg stays on
WHP: a GitHub-hosted runner would have delayed 69% of the runs that ran it, and
still 13% with a warm sccache, which would take over 2 GiB of the repository's
10 GB Actions cache, already full. Larger GitHub-hosted runners, which public
repositories pay for, or a self-hosted Windows runner without a hypervisor
would cost about as much as another general WHP runner.

The WHP microVM tests bound every run that skips both OpenVMM suites, so each
backend runs them in two jobs, and the first of them in shards (see
[MicroVM test shards](#microvm-test-shards)). In reference run
[39585909588](https://github.com/microsoft/nvx/actions/runs/39585909588), the
single WHP job took 15.1 minutes: 0.8 minutes of setup, 10.1 minutes of
default suite, and 4.2 minutes of guest smoke and sandbox steps, which KVM ran
in 3.2 of its 11.4 minutes. Each `nvx-microvm-guests-*` job runs those steps
beside the default suite after a setup of its own, so it holds one more
general runner per backend for about three to five minutes. The default suite
waits only for `artifacts`, so a cold build of the Ubuntu or Azure Linux
images, about 4 minutes, delays only the guest and sandbox job, which takes
about as long as a shard of the default suite. A full-matrix run then needs
five of KVM's four general runners at once: one for the Linux VMM-test build
and then its KVM leg, and four for the two shards, the guest and sandbox job,
and the debug-kernel job, so the last of them to start waits for the first to
end, about three minutes later; a run that skips the VMM tests fills the four.
MSHV's four hold the two shards, the guest and sandbox job, and, on `dev`
pushes and full runs, the debug-kernel job, and its VMM-test leg starts after
the Linux build, by when the guest and sandbox job has ended. On WHP, a run
needs at most seven of the six general runners at once: one for the OpenVMM unit
tests, up to four for the microVM jobs, and one for the Windows VMM-test build
and then two for the [shards of the WHP VMM tests](#openvmm-vmm-test-shards).
The build ended after the last WHP microVM job in 10 of the 13 runs since
[#783](https://github.com/microsoft/nvx/pull/783) that ran both, so the shards
usually take runners that the microVM jobs freed, and a run then uses at most
six. A run that restores the Windows build from the
[cache](#openvmm-vmm-test-cache) instead ends it within about a minute, so its
shards run beside the microVM jobs. A `dev` push can then need all seven, so
the last job to start waits for one to end; a pull request or merge group needs
at most six. When a pool has fewer
free general runners than a backend's microVM jobs, the jobs that start last
wait for the first to end, so the backend's microVM tests take up to about
twice as long as one of its jobs, still less than the unsharded suite.

## MicroVM test shards

`nvx.py test-microvm --shard INDEX/COUNT` runs one of COUNT shards of the
default Alpine suite on the default kernel, which holds the scenarios that
`test-microvm` runs without `--scenario` but the public sandbox scenarios,
which the guest and sandbox jobs run beside the Ubuntu artifacts that they
read. The partition is
[`microvm_shards.json`](../scripts/nvx_tools/microvm_shards.json): for each
backend, the median duration of every default scenario, the scenarios of each
shard, and how long each shard takes. The backends differ too much for one
partition to balance them all: `console-snapshot` takes 49.1 seconds on KVM
but 1.0 on MSHV and 3.9 on WHP, and `console-exit` 25.2 seconds on KVM but 87.5
on WHP, so each backend has a partition of its own.
[`plan-microvm-shards`](usage.md#plan-microvm-shards) plans them from the
[scenario duration reports](usage.md#scenario-duration-report) of CI runs, so
that the shards of a backend take about as long as each other, whatever number
of scenarios each runs.

CI runs each shard in a job of its own. The default-suite job of each backend,
`nvx-microvm-tests-<backend>`, is a matrix of its shards, which passes each leg
its `matrix.shard` and the matrix's `strategy.job-total` as `--shard
INDEX/COUNT`, on the backend's `general` runners. A leg fails when the
partition gives its backend another number of shards than the matrix runs,
and a test checks that each backend's matrix runs exactly the shards of its
partition, so that the legs run every default scenario exactly once on each
backend, with every processor count. A shard that fails does not cancel the
others (`fail-fast: false`), so each keeps its failure logs and its duration
report, under names of its own, and *Re-run failed jobs* reruns only the
shards that failed. For a `dev` run, use that partial rerun only under the
[procedure below](#run-concurrency). GitHub expands the matrix only once the
job's condition holds, so a backend's shards run together or not at all. The
job's result, which `Required status check`, `Publish development release`, and
`Persist performance baseline` read, succeeds only when every shard succeeds.
The guest and sandbox jobs and the debug-kernel jobs are not sharded: the
debug-kernel jobs take three to five minutes, about as long as a shard, and
still run on KVM only for pull requests and merge groups and on every backend
for `dev` pushes and full runs.

The first plan took the medians of the reports of 13 runs from October 21 and
22, 2026. It started each backend from the two shards that
[#696](https://github.com/microsoft/nvx/issues/696) proposed, the restore,
snapshot, and time ABI scenarios and the network, filesystem, and lifecycle
scenarios, which took 248 and 210 seconds on KVM, 203 and 213 on MSHV, and 287
and 309 on WHP, and on each backend the planner swapped one scenario of the
first shard for one of the second. Planned again from the reports of the 18
runs that the file lists, which add two later `dev` pushes, another pull
request's run, and two runs of [#763](https://github.com/microsoft/nvx/pull/763),
which added the shards, every scenario stayed in its shard:

| Backend | Default suite | Shard 1 | Shard 2 | Swapped from the proposed shards |
| --- | ---: | ---: | ---: | --- |
| KVM | 457.7 s | 228.8 s | 228.9 s | `smp` for `filesystem-comma-paths` |
| MSHV | 416.0 s | 208.0 s | 208.0 s | `time-abi-conformance` for `directional-network-policy` |
| WHP | 601.5 s | 300.4 s | 301.1 s | `snapshot-core` for `host-loopback-policy` |

Each backend runs two shards. A default-suite job spends about 0.7 minutes
outside the suite, setting up and uploading, so in the runs of
[#763](https://github.com/microsoft/nvx/pull/763) a shard job took 4.4 to 4.5
minutes on KVM, 4.2 to 4.9 on MSHV, and 5.7 to 6.4 on WHP, where the single
default-suite job had taken 8.1 to 8.6, 7.6 to 7.7, and 10.4 to 11.1. That is
about as long as the guest and sandbox jobs beside them, which took 3.7 to
4.1, 2.9 to 4.1, and 5.2 to 6.4 minutes in the runs since
[#695](https://github.com/microsoft/nvx/issues/695) split them off. A third
shard would take about 3.2, 3.0, and 4.0 minutes. On WHP it would end before
the guest and sandbox job, so it would not shorten the backend's tests, and on
KVM, where the two shards, the guest and sandbox job, and the debug-kernel job
fill the four general runners, and on MSHV, where they fill them for `dev`
pushes and full runs, it would mostly wait for a runner.

A sharded run fails before it runs a scenario unless the file puts every
default scenario in exactly one shard of every backend, with its duration,
and splits the backend's suite into the run's COUNT shards. Tests in
`scripts/test_microvm_tests.py` check the same, and that the file is what the
planner makes of its own durations, so a change that adds, removes, or
renames a default scenario fails them until it plans the shards again:

1. Download the scenario duration reports of recent CI runs that ran the
   default suite, such as the last ten `dev` pushes, for each run:

   ```bash
   gh run download RUN_ID --repo microsoft/nvx \
       --pattern 'nvx-microvm-durations-*' --dir build/durations/RUN_ID
   ```

2. Plan the shards from them:

   ```bash
   python3 scripts/nvx.py plan-microvm-shards --report build/durations
   ```

   A scenario that no report measures keeps its duration in the file. A new
   scenario has none, so give it an estimate, such as
   `--seconds command-line-limit=2`, which the report of a local
   `test-microvm --scenario command-line-limit` run gives. A regeneration from
   CI's reports replaces the estimate once CI has run the scenario.

3. Commit the file with the change. The planner prints each backend's shards
   and the scenarios that changed shard. A scenario stays in its shard unless
   moving it, or swapping it for a scenario of another shard, shortens the
   longest shard by a second or more, so new medians that only move within
   the run-to-run noise leave the shards as they are. To change a backend's
   number of shards, pass `--shards BACKEND=COUNT` and change the matrix of its
   default-suite job in `ci.yml` to list the shards from 1 to COUNT.

## Guest artifact handoff

Three GitHub-hosted jobs restore or build the guest artifacts side by side,
each with the
[`build-guest-artifacts`](../.github/actions/build-guest-artifacts/action.yml)
action, whose `components` input selects what it builds: `artifacts` the
kernel and the Alpine initramfs, `distro-artifacts` the Ubuntu initramfs, its
EROFS distro layer, and the sandbox scratch template, together with the Azure
Linux initramfs, and `debug-kernel` the CI debug kernel. Each job uploads each
of its components as a one-day workflow artifact of its own. Each job that
boots or reads guest artifacts downloads, by name and into `build/`, only the
components that its steps read, and waits only for the jobs that upload them:

| Artifact | Files in `build/` | Producer | Jobs that download it |
| --- | --- | --- | --- |
| `guest-kernel` | `vmlinux`, `vmlinux.config`, `vmlinux.provenance.json` | `artifacts` | VMM tests, microVM tests, debug-kernel jobs, platform jobs |
| `guest-alpine` | `initramfs.cpio.gz`, its `.packages.json`, `initramfs.provenance.json` | `artifacts` | VMM tests, microVM tests, debug-kernel jobs, platform jobs |
| `guest-ubuntu` | `initramfs-ubuntu.cpio.gz` and its `.packages.json` | `distro-artifacts` | microVM guest and sandbox jobs, platform jobs |
| `guest-ubuntu-distro` | `ubuntu-distro.erofs` and its `.manifest.json`, and `ubuntu-smoke-scratch.ext4`, the sandbox scratch template | `distro-artifacts` | microVM guest and sandbox jobs, platform jobs |
| `guest-azurelinux` | `initramfs-azurelinux.cpio.gz` and its `.packages.json` | `distro-artifacts` | microVM guest and sandbox jobs, platform jobs |
| `guest-debug-kernel` | `vmlinux-debug`, `vmlinux-debug.config`, `vmlinux-debug.provenance.json` | `debug-kernel` | debug-kernel jobs |

The OpenVMM VMM tests boot only the kernel and the Alpine initramfs, and their
Linux build boots no guest, so it downloads no component. The
debug-kernel jobs run the Alpine scenarios on the debug kernel and qualify
their runner, through the doctor's preflight (H3), with the kernel and the
Alpine initramfs. The microVM default suites boot only those two, and the
microVM guest and sandbox jobs boot every guest and run sandboxes from the
Ubuntu distro layer over copies of the scratch template, which comes with the
layer: the layer's Docker target formats it, its cache entry holds it, and the
Ubuntu record gives its digest, so a merge group or a push reuses it with the
layer. The microVM test workflow's own default, an unsharded default run, which
no CI job selects, runs the public sandbox scenarios too, so it also downloads
the layer. The
platform jobs boot the kernel and the Alpine initramfs to qualify their runner
and to benchmark, and package every guest image, and the scratch template, into
their release. A missing artifact fails its download step, so no job runs
without a component that it reads. The microVM test jobs also set
`NVX_AUTO_DOWNLOAD=0`, so that `nvx-rs` never replaces a missing artifact with
a release's, as it does in a checkout without artifacts; see
[`download`](usage.md#download). A test in `scripts/test_nvx_tools.py` maps
each step of these jobs, and of that unsharded default run, to the guest files
that it reads, through the `nvx.py` command or local action that it runs and
the `build/` paths that it names, and fails when a job lacks a component that a
step reads, downloads one that none reads, or waits for a guest artifact job
that uploads none of its components. It also checks that each guest artifact
job builds exactly the components that it uploads, and that no component
builds in two jobs.

`distro-artifacts` also builds the runtime adapter's guest agent, which is no
guest component: a static musl executable that it uploads as
`runtime-adapter-guest` for the Windows guest and sandbox job, which downloads
it into `build/runtime-adapter/`. The Linux guest and sandbox jobs build their
own (see [Rust crates](#rust-crates)).

Each job restores its components from their caches, and builds, verifies, and
saves those that miss, as one job did before. The Ubuntu determinism check,
the provenance that each build records, and the cache keys are unchanged. A
cold `artifacts` job, with a warm kernel, takes about 1.3 minutes, so a miss
of the Ubuntu or Azure Linux images, whose build and determinism check take
about 4 minutes, no longer delays the VMM tests, which are the longest job of
most runs that select them. `distro-artifacts` builds the base and guest-binary
stages of the guest Docker build again, about 45 seconds of GitHub-hosted time,
and its consumers wait for it about as long as they waited for the single guest
artifact job before. From October 19 to 21, 2026, before the guest image keys
covered only the guest binaries' inputs, 159 of 200 guest artifact jobs missed
the cache, nearly all for the Alpine, Ubuntu, and Azure Linux images;
[#703](https://github.com/microsoft/nvx/issues/703) records the measurements
and how many of those misses the narrower keys remove. A `dev` push installs
the components that its pull request's run built instead of building them,
when they check out; see [Guest artifact reuse](#guest-artifact-reuse).

## Guest artifact reuse

GitHub scopes the caches that a pull request's runs save to the pull request,
so the `dev` push that merges it cannot restore them, and used to rebuild
every guest image whose inputs the pull request had changed. From October 10
to 21, 2026, when one job built every guest image, 50 of the 87 `dev` pushes
that ran it built at least one image, and the job took a median of 5.4
minutes, and at most 11.3, instead of 0.3, which delayed every job that waits
for it. A push therefore reuses the components that its pull request's run
already built, when they check out, and so does the pull request's
[merge group](#merge-queue), which can't restore them either.

`artifacts` and `distro-artifacts` each record their components. The last
steps of
[`build-guest-artifacts`](../.github/actions/build-guest-artifacts/action.yml)
run [`record-guest-artifacts`](usage.md#record-guest-artifacts), which writes
the guest artifact record, `build/guest-artifacts.json`, with the exact keys of
the components' cache entries and the SHA-256 of each of their files, and
upload it for one day under the name that the action's `record-artifact` input
gives: `kernel-alpine-record` for the kernel and the Alpine initramfs, and
`ubuntu-azurelinux-record` for the Ubuntu and Azure Linux images. No job of the
run downloads a record. On a push or a merge group, after the cache restores
and before any build, [`reuse-guest-artifacts`](usage.md#reuse-guest-artifacts)
runs in each of the two jobs when a file of its components is missing. It
takes the merged pull request's head from the second parent of the run's merge
commit, which the queue builds as a push's, and the latest `pull_request` run
of `ci.yml` for that head whose head repository is `microsoft/nvx`, so never a
fork's run. It downloads the job's record from
that run, unless the record has expired, which it does after one day, or lacks
the SHA-256 digest that GitHub records at upload, and compares its keys with
the run's own keys for the job's components, so the keys of the other job's
components never matter. Only when they are equal does it download the
components' artifacts, `guest-kernel` and `guest-alpine`, or `guest-ubuntu`,
`guest-ubuntu-distro`, and `guest-azurelinux`, under the same conditions. It
installs their files only when each artifact matches its digest, every file
matches the record, and the kernel and initramfs provenance, or the Ubuntu and
Azure Linux manifests, match the files and the run's checkout, as
[`package`](usage.md#package) requires. Then it replaces every file of the
job's components, not only the missing ones, because a cache restore that fails
partway can leave a truncated file behind and still report a miss; the files
that the cache restored have the same keys. The job then skips the builds of
its components and, in `distro-artifacts`, the Ubuntu determinism check, which
the run that built the images passed, as for a cache hit. A push still saves
the components in its caches for later runs, and either one records them and
uploads them as its own artifacts, which its consumers download as before, so
no job depends on another run's artifact or on a cache. The debug kernel is
never reused.

Every other outcome builds the components that missed the cache, as before: a
merge commit without a same-repository pull request run, an expired record or
artifact or one without a digest, a run from before the records, other keys,
as after another merge changed the job's guest inputs, a failed request or
download, a file that does not check out, or a failed installation, which puts
back the files that it replaced. After two minutes, a timer stops a stalled
download or check in the same way, but never an installation, which it could
cut short with its files half put back. The step continues on error and never
fails the run. It reports why in the job summary and as a notice, or as a
warning for a fault. `artifacts`, `distro-artifacts`, and the rehearsal below
add `actions: read`, which reading runs and downloading their artifacts needs,
to the workflow's `contents: read`.

Pull requests can't exercise the reuse itself, so the `guest-artifact-reuse`
job (`Rehearse guest artifact reuse`) rehearses it for each same-repository
pull request once both guest artifact jobs succeed, beside the jobs that wait
for them. It runs `reuse-guest-artifacts --dry-run` for the pull request's
head and each job's components, record, and keys, which selects the run in
progress, downloads the records and the artifacts that the two jobs just
uploaded, checks them as a merge group or a push would, and reports in the job
summary whether those would reuse them, without installing anything. A reason
not to reuse fails nothing, but it warns, because a run's own artifacts should
always check out. The change detector selects the rehearsal with the guest
artifacts, and `Required status check` requires it to succeed for a
same-repository pull request that builds them, so a pull request that breaks
the reuse command can't merge. Its merge group doesn't rehearse the reuse
again, but reuses the artifacts for real (see [Merge queue](#merge-queue)).

## Run concurrency

Each pull request runs in a concurrency group of its own, and a push to it
cancels the run of the previous head, whether that run waits or runs. Each
merge group's run also has a concurrency group of its own (see
[Merge queue](#merge-queue)). Pushes to `dev` share one group and run one at a
time in the order in which they arrive. The group keeps up to 100 waiting runs
(`queue: max`), so a merge does not cancel the waiting run of an earlier
merge. Until October 2026 the group
kept a single waiting run, which each newer push replaced: 12 of the 199 `dev`
pushes from October 4 to 21 lost their run that way, five of them merged
within four minutes on October 20. Those commits got no development release
and no performance results, and because the detector compares a push with its
previous commit, the run that followed classified only its own changes.
GitHub rejects `queue: max` together with `cancel-in-progress: true`, so the
workflow derives both settings from the event. Each scheduled or manually
dispatched full run has a group of its own (see
[Change classification](#change-classification)).

A queued run waits for the runs ahead of it. Replaying the `dev` pushes of
October 4 to 21 one at a time, at their measured durations and the median one
for those that were cancelled, half would have started at once, one in ten
would have waited at least 50 minutes, and the longest wait, on October 20,
would have been about three hours.

A rerun attempt of an earlier `dev` run joins this same group. Before running
`gh run rerun RUN_ID --failed`, query both blocking states explicitly, so the
result limit is applied after the status filter:

```bash
gh run list --workflow ci.yml --branch dev --status queued --limit 1
gh run list --workflow ci.yml --branch dev --status in_progress --limit 1
```

Continue only when both commands print no run, and never cancel a waiting
rerun. At 15:05:07Z on
October 22, 2026, failed-job rerun attempt 2 of
[run 39788615461](https://github.com/microsoft/nvx/actions/runs/39788615461),
for commit `911d5d86e`, joined while `dev` run 39801222132 was in progress. It
ended cancelled at 15:09:32Z without starting a job, and GitHub recorded no
cause. In contrast, failed-job rerun attempt 3 of
[run 39777444440](https://github.com/microsoft/nvx/actions/runs/39777444440),
for commit `0e73e460f`, started its failed job normally at 15:47Z, when no other
`dev` run waited or ran.

Partial reruns act on the latest attempt. After the empty attempt of run
39788615461, another `--failed` request failed with `This workflow run cannot
be retried`, and `--job 119871110545` failed with `cannot be rerun`, because
that latest attempt held no failed job. The only remaining retry is the
all-jobs `gh run rerun RUN_ID`. It repeats the jobs that the original push and
its change classification select, not the full matrix of a scheduled or manual
run. For a non-documentation `dev` push such as run 39788615461, those jobs
include the platform benchmarks, development release, and performance
persistence. For
an older commit, the release job uses that run's `GITHUB_SHA` to create or
verify its
SHA-specific development release. Performance persistence records that same
SHA after applying the results to the current `dev` history: it drops rows
already present for the same platform, ABI, processor count, commit, and
metric, but otherwise appends them and pushes a baseline update.

## Merge queue

`dev` requires `Required status check`, but not pull requests that are up to
date with `dev`, so a pull request's run tests its head merged into `dev` as
`dev` was when the run started, and two pull requests that each pass their runs
can break `dev` together. That happened on October 25, 2026:
[#825](https://github.com/microsoft/nvx/pull/825) added CLI tests that
expected the old egress error messages, and
[#828](https://github.com/microsoft/nvx/pull/828), merged 22 minutes later,
changed them; each passed CI without the other, and the `nvx-rs` jobs failed
in every `dev` run for about nine hours, until
[#830](https://github.com/microsoft/nvx/pull/830) updated the tests
([#895](https://github.com/microsoft/nvx/issues/895)). Since October 30, 2026,
`dev` therefore requires GitHub's merge queue, which closes that gap. It tests
each queued pull request in a merge group: the merge of the pull request into
the latest `dev` and the pull requests ahead of it in the queue, on a branch
`gh-readonly-queue/dev/pr-NUMBER-BASE`. It merges a group only once the
group's required check passes, by moving `dev` to the group's head, so it
merges only commits that a run tested. When a group fails, it removes the pull
request from the queue and rebuilds the groups behind it without it.

`ci.yml` therefore runs for the `merge_group` events of `dev`, whose
`checks_requested` activity asks for a group's checks; without them, the queue
would wait for a `Required status check` that never reports, until its timeout
removed the pull request. A group's run checks out the group's head, which
`GITHUB_SHA` names, and the change detector compares it with
`merge_group.base_sha`, the commit on which the queue built the group: the
latest `dev` for the first group, and the head of the group ahead of it for
every other. Each group's run therefore classifies the change of its own pull
request, as the queue merges it, while the groups ahead check theirs. A suite
whose inputs the pull request leaves alone gives the same result on the group
as on the group ahead, so the queue must require every entry of a group to pass
its checks (see [Queue settings](#queue-settings)): it then merges no group
while a group ahead of it fails.

A group's run then checks what the run of a same-repository pull request
checks, but the rehearsal of the [guest artifact reuse](#guest-artifact-reuse),
which the pull request's run made: each suite runs only when one of its inputs
changed, the platform jobs and the performance regression gate after them
included, the debug kernel runs on KVM alone, and the OpenVMM VMM tests are
restored from the [cache](#openvmm-vmm-test-cache) that `dev` pushes save. A
group publishes no development release and persists no performance results,
which the `dev` push of the merged commit does, and saves no cache: GitHub
scopes a cache entry to the ref that saved it, and no other run can restore an
entry of a group's branch, which GitHub deletes as soon as it merges or drops
the group. A group's run can restore the entries of `dev` alone, not those of
its pull request's runs, so, like the `dev` push that merges the pull request,
it installs the guest artifacts that the pull request's run built instead of
building them again, when they check out (see
[Guest artifact reuse](#guest-artifact-reuse)); a pull request that changes
the key of the OpenVMM VMM tests, though, builds them in its group too. Each
group's run has a concurrency group of its own, keyed by the run rather than
the group's ref, so it neither waits for nor cancels another: a pull request
that leaves the queue and returns to it on the same base gets its earlier
group's ref again, and a group keyed by the ref would hold the new run until
the replaced one ended. `Required status check` passes `merge_group` to
[`check-required-ci`](usage.md#check-required-ci), which expects those jobs.

A merge group runs in the repository, with the repository's secrets, whatever
repository its pull requests come from: GitHub builds the group's branch
there, and
[GitHub's own documentation repository](https://github.com/github/docs/blob/main/.github/workflows/restrict-merge-queue.yml)
relies on its merge groups' secrets for forks' pull requests. Its run
therefore runs the repository's jobs, on the self-hosted runners and with the
OpenVMM deploy key, for a fork's pull request too, whose own runs ran only the
GitHub-hosted ones, and `check-required-ci` expects them whatever
`--same-repository` says. Queueing a fork's pull request thus runs its code on
the fleet before it merges, as the `dev` push that merged it did after, and
that code includes the workflow: a group's run takes `ci.yml`, with its jobs'
steps and the permissions of their tokens, from the group's head. `dev`
therefore requires the approval of a pull request's most recent push
(`require_last_push_approval`), so that someone other than its pusher approves
the code that a group runs with the repository's secrets, at the cost of
another approval whenever a pull request's author pushes after its approval.
The maintainers whom the review rule exempts can queue a pull request without
any approval, though (see [Reviews and bypasses](#reviews-and-bypasses)), so
the code that they queue is reviewed only when they wait for its review.

### Superseded merge groups

The queue builds a pull request a new group, on another base, whenever the
queue ahead of it changes: when a group ahead fails, when a pull request ahead
leaves the queue or another jumps it, and whenever `dev` changes outside the
queue, as with each push to `dev` that a bypass actor makes. It deletes the
branch of the group that it replaces, but GitHub doesn't cancel that group's
run, which holds its runners until it ends although the queue never reads its
checks again
([community discussion](https://github.com/orgs/community/discussions/137976)).
A test repository's queue showed both on October 30, 2026: a push to its base
branch while three groups ran replaced all three within 20 seconds and deleted
their branches, and the runs of the three replaced groups ran on to their end,
six minutes later. A concurrency group per pull request can't cancel them: the
only field of a group's event that names its pull request, the group's ref,
also names the group's base, which no expression can strip, and keyed on a
field that a rebuilt group shares, such as its commit message, a late event
for a replaced group could cancel the live group's run instead.

The first job of each group's run, `cancel-superseded-merge-groups`
(`Cancel superseded merge group runs`), therefore runs
[`cancel-superseded-merge-groups`](usage.md#cancel-superseded-merge-groups). It
lists the workflow's unfinished `merge_group` runs and then the branches of the
pull request's groups, and cancels the runs of the pull request's groups whose
branches are gone or hold another head than the run's. The queue builds a pull
request's groups one after another, so the group whose branch holds a run's
head is the live one, whichever run started first; a pull request that leaves
the queue and returns to it on the same base gets its earlier group's branch
again, with another head, so a branch that exists may still name a replaced
run's group. The job holds its own run to the same test, but cancels it last:
GitHub can deliver a replaced group's event late, after the run of the group
that replaced it looked, and only the late run itself then finds that it is
superseded. A pull request that leaves the queue gets no new group, so the run
of its last group runs on, unless the pull request returns to the queue, whose
new group then cancels it, or the run's own job looks only after it left.

The job runs on a GitHub-hosted runner with `actions: write`, which cancelling
a run needs, beside the workflow's `contents: read`. That token can cancel any
of the repository's runs, so the job checks out the group's base branch, `dev`,
and runs the command that has merged there rather than the group's copy: a
change to the command takes effect once it merges, and one that changes its
options can fail the job in its own groups. The job's steps still come from the
group's `ci.yml`, though, like every job's (see [Merge queue](#merge-queue)),
so the checkout only keeps the command's unmerged code from the token and is no
isolation boundary: a queued pull request that changes the job's steps, or
gives another job the same permission, gets such a token, and only the review
of its latest push guards against that. Nothing waits for the job, and a failed
request only warns, and never stops the other cancellations, so it never holds
the queue.

### Queue settings

`dev`'s `Default Branch` ruleset requires the queue with a `merge_queue` rule,
which an administrator added on October 30, 2026, once `ci.yml` on `dev` ran
for merge groups: a group's run takes `ci.yml` from the group's head, and a
`ci.yml` without the trigger reports no check, so the queue would have held
every pull request until its timeout. The rule's settings:

- **Merge method:** merge commits (`MERGE`), the only method that `dev`
  allows. The queue builds each group's head as the merge commit of its pull
  request's head into the group's base, `Merge pull request #NUMBER from
  OWNER/BRANCH`, and moves `dev` to the head of the last group that it merges,
  so each pull request keeps a merge commit of its own, whose second parent is
  its head. The [guest artifact reuse](#guest-artifact-reuse) of the group's
  run and of the `dev` push finds the pull request's run through that parent;
  a squash or a rebase would leave none, and both would rebuild every guest
  artifact that the pull request changed.
- **Require all queue entries to pass required checks:** on
  (`grouping_strategy: ALLGREEN`). Each group's run checks only its own pull
  request's change, so the queue must not merge a group while a group ahead of
  it fails, as it may when only the head of a group must pass (`HEADGREEN`).
- **Status check timeout:** well beyond the slowest run of a group, which a
  pull request's run measures. From October 16 to 29, 2026, the first
  attempts of the 400 successful pull request runs that ran the platform jobs
  took a median of 26.3 minutes from the creation of their first job to the
  end of their last, 51.2 at the 90th percentile, and 100.5 at the 99th, waits
  for runners included; the 26 that ran other self-hosted jobs but not the
  platform jobs took 13.2, 24.8, and 35.6. A timeout of 180 minutes removes
  only the groups whose runs a lost runner or an outage holds.
- **Build concurrency:** 2. A group's run that selects every suite can occupy
  every `general` KVM runner at once (see
  [Job graph and runner capacity](#job-graph-and-runner-capacity)), so a
  second group can only prepare behind it, and more would only queue for the
  same runners while every push to `dev` outside the queue rebuilds them all
  (see [Pushes outside the queue](#pushes-outside-the-queue)).
- **Merge limits:** at least one pull request, without a wait, so that the
  queue merges each group once it and the groups ahead of it pass, and at most
  one, rather than GitHub's default of five, since October 30, 2026. The
  limits bound the queue's merges, not its groups' builds
  ([GitHub's documentation](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue)),
  so groups that pass together still merge one after another, each in a push
  of its own that moves `dev` to the group's head, on which the queue built
  the group behind it. Each pull request thus reaches `dev` in a push of its
  own. A successful run for a non-documentation change publishes its
  development release and measures its performance. A failure identifies the
  newly merged pull request, but may stem from an earlier change or the
  environment.
  A push that merged several groups would leave the merge commits before its
  head without a run, a release, or performance results of their own. The
  cost is a push run for each pull request that would have shared a push, as
  every merge had before the queue; when the pull request changes more than
  documentation, the run's platform jobs hold each pool's single `benchmark`
  runner for a median of 5.3 to 8.6 minutes (see
  [Development releases and performance history](#development-releases-and-performance-history)).

### Reviews and bypasses

`dev`'s review rule has a ruleset of its own, `Default Branch reviews`: an
approval from `@microsoft/nvx-maintainers` of the pull request's most recent
push, every review thread resolved, and merge commits alone. `Default Branch`
holds the queue, `Required status check`, and the rules against deleting `dev`
or rewriting its history. The deploy key and two maintainers bypass
`Default Branch` (`bypass_mode: always`): their pushes to `dev`, and their
merges with `gh pr merge --merge --admin`, skip the queue, and GitHub records
each bypass. `Default Branch reviews` exempts the two maintainers instead
(`bypass_mode: exempt`): GitHub doesn't evaluate the review rule for them and
records no bypass, so they can queue a pull request without waiting for its
review, and it reaches `dev` through a merge group like any other. The deploy
key, which pushes the performance history and the module diagrams to `dev`
without a pull request, bypasses the review rule as before.

The queue admits a pull request only when the review rule allows whoever queues
it, and a bypass doesn't count. A test repository showed this on October 30,
2026. Queueing an unapproved pull request failed with
`Waiting on required approvals` whenever the review rule let the maintainer
bypass it, with `always`, in the queue's ruleset or in one of its own, or with
`pull_request`; where the queue's ruleset held the bypass,
`gh pr merge --merge --admin` merged the pull request past the queue. Queueing
succeeded, and the queue merged the pull request through a merge group, only
when the review rule's ruleset exempted the maintainer. An exemption from a
ruleset that also held the queue would exempt them from the queue too, so the
review rule has a ruleset of its own. The exemption applies to whoever queues a
pull request, not to its author.

Queue a pull request with `gh pr merge NUMBER` or with **Merge when ready** on
its page. Each adds the pull request to the queue once it can merge, and
enables auto-merge before, which queues it then. With a queue, `gh pr merge`
always queues through auto-merge, which the repository therefore allows.
`gh pr merge NUMBER --merge --admin` merges a pull request past the queue, as a
push outside it (see [Pushes outside the queue](#pushes-outside-the-queue)),
for when the queue itself is broken. Like `gh pr merge` without `--admin`, it
does nothing for a pull request in the queue, so remove such a pull request
from the queue first, with **Remove from queue** on its page.

### Pushes outside the queue

The queue isn't the only way into `dev`: the deploy key with which
`Persist performance baseline` and the module diagrams workflow push, and two
user accounts, bypass `Default Branch`, whose `merge_queue` rule binds them no
more than its other rules (see [Reviews and bypasses](#reviews-and-bypasses)).
From October 16 to 29, 2026, `dev` took 234 baseline commits and 23 diagram
commits beside its 272 merges. Each such push changes the base of every group
in the queue, which the queue then rebuilds on the new tip (see
[Superseded merge groups](#superseded-merge-groups)), so it costs every group
the run that it had made so far. A baseline commit landed a median of 29.2
minutes after the merge whose run it records, 16.1 at the 10th percentile and
99.9 at the 90th, about as long as a group's run, and the merges came a median
of 27.6 minutes apart, 148 of 271 within 30 minutes of the one before. On a
busy day, the baseline commit of nearly every merge would thus rebuild the
group that the queue builds next, which would then take about two runs to
merge. The performance history's commits should therefore leave `dev`, for
instance to a branch of their own from which the gate reads the history, or
wait for the nightly full run, so that `dev` takes pushes outside the queue
only rarely ([#1053](https://github.com/microsoft/nvx/issues/1053)).

### Cost

With the queue, each change runs CI three times: in its pull request, in its
merge group, and in the `dev` push that merges it. A group's run selects what
its pull request's run selected, on the latest `dev`, and so takes about as
long (see [Queue settings](#queue-settings)). The tail of those runs is the
wait of their platform jobs for their pools' single `benchmark` runners, which
the pull requests' and the pushes' platform jobs share: the longest of a run's
platform jobs waited from its creation to its start a median of 6.3 minutes,
28.8 at the 90th percentile, and 78.3 at the 99th. The queue holds a group
that runs them, and every group behind it, that much longer. The push could
in principle reuse the group's results, since the queue moves `dev` to the head
of the group that it merges, which that group's run tested, and merges one
pull request per push (see [Queue settings](#queue-settings)), so the push
carries exactly the change that the group's run checked. It doesn't: a push
still publishes the development release and records the performance history,
and so runs the platform jobs for every change but documentation, and it saves
the caches that later runs restore, which no group can. Skipping a push's test
suites when the run of the group that it merged passed is left for later.

## Cancellation

When a run is cancelled, as a push to a pull request cancels the run of its
previous head, the runner
[cancels each running step](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-cancellation):
it sends SIGINT to the step's shell, SIGTERM if the shell still runs 7.5 s
later, and kills the shell's process tree another 2.5 s later. After the
job's last step, the runner's orphan cleanup kills every process that the job
started and that still runs. `nvx.py` handles SIGTERM as it handles SIGINT:
either one interrupts its command, which then stops the OpenVMM processes and
removes the scratch directories that it started and created, and exits with
status 130 (see [Commands](usage.md#commands)).

The runner signals only the step's shell, though, and Bash runs a trap only
once the command in its foreground ends, so a harness that a Linux step ran in
the foreground never learned of the cancellation: it ran on until the orphan
cleanup killed it, which left the scratch directories of its scenario behind.
The Linux steps that run `test-microvm` therefore `exec` it, so that it is the
step's process. The Linux steps of `run-benchmark`, which run the harness among
other commands, `exec` it as their last command or source
`.github/actions/run-benchmark/forward-signals.sh`, whose `run_harness` runs
the harness in the background, with the SIGINT that Bash ignores there
restored, and passes SIGINT and SIGTERM on to it as SIGTERM, which, unlike
SIGINT, the harness never ignores, not even before it has restored SIGINT. A
signal that arrives as `run_harness` starts the harness, before it knows the
harness's PID, reaches the harness once it does. The step then ends once the
harness exits, even if the signal came too late to stop it, and a signal that
arrives between two harness runs ends the step at once.

On Linux, OpenVMM also ends with `nvx.py` when something kills `nvx.py`
before its cleanup runs, as the orphan cleanup does. Where the host has
util-linux's `setpriv`, as the runners do, `nvx.py` starts OpenVMM through
`setpriv --pdeathsig KILL`, so that the kernel kills OpenVMM when `nvx.py`
exits, and through a shell that starts OpenVMM only if `nvx.py` is still its
parent, since an `nvx.py` that died before `setpriv` armed the signal sent
none. A contained process tree, such as `nvx-rs run` and the
OpenVMM that it starts, runs under a supervisor that the kernel sends SIGTERM
when `nvx.py` exits, and which then kills the whole tree. The supervisor runs
without the job's `RUNNER_TRACKING_ID`, which it gives back to the command,
because the orphan cleanup kills the processes whose environment holds it and
could kill the supervisor before it ended a tree that the cleanup didn't list,
such as an OpenVMM that started after the listing. The kernel sends
these signals when the thread that started the process exits, so only a
process that the main thread starts gets them, and `nvx.py` starts its VMs
from its main thread. The `validate-runner` action of the runner's next job
ends the OpenVMM processes and removes the scratch directories that still
escape all of this.

## OpenVMM VMM tests

OpenVMM's `vmm-tests-run` builds the tests that a filter selects and then runs
them. In build-only mode, it instead writes everything that the tests need to
a test content directory, from which `vmm-tests-run-target` runs a filter's
tests on another host: the tests' nextest archive, OpenVMM, the guest agents,
the test firmware and kernels, and the `flowey_hvlite` that runs them. The
content is relocatable, because `vmm-tests-run-target` remaps the archive's
workspace to the directory, which also serves as the repository root whose
nextest configuration the tests read. The KVM and MSHV tests run the same
`x86_64-unknown-linux-gnu` build: their jobs resolved the same artifacts, and
the GNU OpenVMM that a KVM runner builds already runs the MSHV microVM tests
on the same Azure Linux 3 release. CI therefore builds the tests once per
operating system and hands each build to the jobs of its backends:

- `build-openvmm-vmm-tests-linux` runs
  [`build-openvmm-tests`](usage.md#build-openvmm-tests) for KVM and MSHV,
  which builds the union of their filters, on a general KVM runner, whose
  sccache holds the Linux test build. It restores, and on `dev` pushes saves,
  the `vmm-tests` OpenVMM input and Cargo caches, and uploads the content as
  the one-day `openvmm-vmm-tests-linux` artifact, which keeps the content's
  hidden `.config` and replaces an earlier attempt's content. It needs no
  guest artifacts, so it starts with the OpenVMM binary producers instead of
  waiting for `artifacts`. While nothing that determines the content changes,
  it restores the content from the
  [OpenVMM VMM test cache](#openvmm-vmm-test-cache) instead of building it.
- `openvmm-vmm-tests-linux` runs a KVM and an MSHV leg once the build and
  `artifacts` succeed. Each downloads the content, the kernel, and the Alpine
  initramfs, and runs [`test-openvmm`](usage.md#test-openvmm) on the content,
  which runs its backend's own filter with the content's
  `vmm-tests-run-target`. The legs check out no OpenVMM, restore no cache, and
  compile nothing, and a rerun of a failed leg downloads the build of the
  earlier attempt instead of rebuilding.
- `build-openvmm-vmm-tests-windows` builds the WHP tests the same way on a
  general WHP runner, whose sccache holds the Windows test build, with the
  Windows `vmm-tests` caches, and uploads the content as the one-day
  `openvmm-vmm-tests-windows` artifact. It also starts with the OpenVMM binary
  producers, and also restores the content from the cache instead of
  building it while nothing that determines it changes.
- `openvmm-vmm-tests-windows` runs the WHP tests, in the
  [shards](#openvmm-vmm-test-shards) of its matrix, once that build and
  `artifacts` succeed. Like a Linux leg, each shard downloads the content, the
  kernel, and the Alpine initramfs, runs `test-openvmm` on the content, and
  checks out, restores, and compiles nothing. On Windows, though, the pinned
  `vmm-tests-run-target` checks for Hyper-V whatever tests it runs: it
  requires `Get-VM` to succeed and the `AllowFirmwareLoadFromFile` and
  `EnableAdditionalComPorts` registry values to be set, or administrator
  rights to set them. The WHP runners run jobs as Network Service, which is
  not a Hyper-V administrator, on hosts whose setup sets neither value. No WHP
  test needs Hyper-V, so `vmm-tests-run`, which derives the dependencies from
  the selected tests, checked nothing when this job built and ran them in one
  step. `test-openvmm` therefore takes the runner's remaining steps itself:
  it downloads the content's one image, `sample-vmgs.vhd`, from OpenVMM's
  public test image container, sets the environment that Petri reads, and
  runs the archive's tests with the runner setup's `cargo-nextest`, as the
  runner does with the release that it downloads. The job needs neither the
  curl shim nor a GitHub token, which only flowey's downloads use.

The content's manifest, `nvx-openvmm-tests.json`, records the OpenVMM revision,
the Rust release, the target and the build options, each backend's filter, the
runner and its options, and every file with its size and SHA-256, so that a
test job refuses content built from another OpenVMM revision or for another
filter, and so that the content describes the inputs on which reusing it would
depend. The builds and their test jobs run together: a run that selects the
VMM tests, and so builds the guest artifacts, runs all four VMM jobs, and any
other run skips all four. `Required status check` expects
exactly that, and `Publish development release` and `Persist performance
baseline` require the four jobs to succeed, or all four to be skipped
because the change selected no VMM tests, so that a failed or unexpectedly
skipped build or test blocks them. The WHP job succeeds only when each of its
shards does.

## OpenVMM VMM test cache

What determines the VMM tests' build changes far less often than the tests
run: replaying the change classifier over the 42 `dev` merges through
[#659](https://github.com/microsoft/nvx/pull/659), the VMM tests would have run
on 33 of them, but the OpenVMM pin changed in none. Each producer therefore
caches its test content across runs, and restores it instead of building it
while nothing that determines it changes
([#692](https://github.com/microsoft/nvx/issues/692)).

[`openvmm-test-cache-key`](usage.md#openvmm-test-cache-key) keys the content
of a producer's backends as `openvmm-vmm-tests-vVERSION-TARGET-REVISION-DIGEST`.
TARGET is the build's target, `linux-x64` or `windows-x64`; REVISION is the
OpenVMM revision that the checkout's gitlink pins; and DIGEST is the SHA-256
of every field of the content's manifest that describes what makes a clean
build of that revision: the manifest's format, the Rust release that
`rust-toolchain.toml` pins, the target, the `vmm-tests-run` build options, a
release build without optional features, and each backend's filter and their
union. A change to any of them, such as a pin promotion, a new Rust release,
or a test that a filter adds, therefore builds the tests again. VERSION,
`OPENVMM_TEST_CACHE_VERSION` in [`ci.py`](../scripts/nvx_tools/ci.py), covers
what none of them captures: increment it when what a build makes, or what a
reuse checks, changes otherwise, such as the environment in which
`build-openvmm-tests` builds the tests. Like the OpenVMM binary cache, the key
leaves out the C toolchains and other tools of the runners, so a runner
upgrade reuses the content built before it until the key changes, and the
full runs below check the build every day.

On a push, a pull request, or a merge group, a producer computes the key before
it checks out OpenVMM and restores the newest entry of the key into the
directory that it uploads. [`reuse-openvmm-tests`](usage.md#reuse-openvmm-tests) keeps that
content only if it is exactly what a fresh build of the key makes: its
manifest must record every field that the key digests, for a clean checkout,
and the runner and options that its run script runs, which the command reads
from the script again, and the directory must hold exactly the files that the
manifest lists, with their sizes, SHA-256, and executable bits. The test jobs
therefore run the same nextest archive with the same options and filters, and
so exactly the tests, that a fresh build would give them. On such a hit, the
producer skips the OpenVMM checkout, the retrying curl, the OpenVMM input and
Cargo caches, sccache, and the build, and uploads the restored content as the
same-run artifact that the test jobs download as before; the test jobs never
read the cache. Otherwise, the reuse removes whatever the restore left, with a
warning annotation when it refuses an entry or a partial restore, and the
producer builds the tests as before. The restore step continues on error, so
a miss, an eviction, a failed restore, or a refused entry only costs the
build.

After a build, [`check-openvmm-tests`](usage.md#check-openvmm-tests) fails the
producer unless the content passes the same check, so that neither the cache
nor a test job gets a build that a reuse would refuse. Only `dev` pushes save
the content, as they save the other OpenVMM caches, and only after a build:
under the key followed by the run ID and attempt, so that each build makes an
entry of its own, and the restore, which takes the newest entry whose key
starts with the key, takes the latest build. An entry that a later reuse
refuses, for example after a change to the check that kept the version,
therefore blocks nothing: the next `dev` push builds the tests and saves a
newer entry. Pull requests and merge groups restore the entries that `dev`
pushes save but save none, so a pull request that changes the key builds the
tests in each of its runs and in its merge group's. Scheduled and manually
dispatched full runs neither restore nor save the content: they build the
tests every day, so that a breakage of the build that the key doesn't cover,
such as a toolchain change on the runners, shows within a day rather than at
the next pin promotion.

This change's validation run,
[39988095577](https://github.com/microsoft/nvx/actions/runs/39988095577), saved
its builds from its pull request through a temporary change. The 228.5 MiB of
Linux content made a 124.0 MiB entry, which the KVM runners compress with gzip,
as they do their other caches, and the 132.6 MiB of Windows content a 37.3 MiB
entry, which the WHP runners compress with zstd: 161.3 MiB a key, 1.6% of the
repository's 10 GB of Actions caches. Those were full, at 10.4 GB in 89
entries on October 23, 2026, so each new entry displaces the least recently
used ones. These were stale: the guest image entries of merged pull requests,
and the OpenVMM input and Cargo entries of the previous OpenVMM pin, about
2.4 GiB, which no run restores any more. A hit, in turn, restores neither the
`vmm-tests` OpenVMM input entry, about 945 MiB per operating system, nor its
Cargo entry, 161 MiB on Linux and 261 MiB on Windows, so those age out between
pin promotions as well. On a miss, the key, the restore, and the two checks
added under five seconds to each producer, whose builds took 340 and 712
seconds, and a `dev` push's save adds about 10 and 4 seconds more. Rerun on
other runners, the producers hit, and took 20 and 36 seconds instead of 6.5
and 14.2 minutes: 3 seconds to restore each entry, at about 118 and 25 MB/s,
under 1 and 2 seconds for the reuse's check, and 8 and 7 seconds to upload the
content. The test jobs then ran the same 61 KVM, 60 MSHV, and 15 and 14 WHP
tests, all passing, from the restored content as from the build.

## OpenVMM VMM test shards

`nvx.py test-openvmm --shard INDEX/COUNT` runs one of COUNT shards of a
backend's VMM tests. The partition is
[`openvmm_test_shards.json`](../scripts/nvx_tools/openvmm_test_shards.json):
for each sharded backend, the test threads with which nextest runs its tests,
the median duration of each test that its filter selects, and the tests of
each shard, with the time that nextest takes to run them. A shard runs the
backend's filter and its own tests both as its filter, so it never runs a
test that the backend's filter leaves out. Only the WHP filter names each of
its tests; the KVM and MSHV filters select theirs by pattern, so no partition
can list them, and only the WHP tests have shards.

nextest runs a job's tests in parallel, so unlike the scenarios of the
[microVM shards](#microvm-test-shards), which run one after another, a shard
does not take the sum of its tests' durations. In the WHP jobs of the 124
runs that the file lists, from October 22 and 23, 2026, the 29 tests took a
median 278 seconds, although their median durations add up to 769, and each
test's duration varied little between runs. nextest starts the tests in name
order, each as soon as it has the test threads that it requires, and never
one before an earlier one. OpenVMM's nextest configuration has a VMM test
require 2 threads, and a `heavy` test 12, so with the eight test threads that
nextest runs by default on the WHP runners' eight logical processors, four
tests run at a time, and each of the two `boot_heavy` tests runs alone: it
waits for every earlier test to end, and every later test waits for it. A
model of that scheduling gives each of the 124 runs' wall time to within 0.2
seconds from its tests' durations, and
[`plan-openvmm-test-shards`](usage.md#plan-openvmm-test-shards) balances the
shards' modeled times. Two shards of equal sums of durations would take 177
and 175 seconds, because each would hold a heavy test that stalls its other
tests. The first plan instead put both heavy tests with shorter tests in one
shard, which took 119.8 seconds, and the two FreeBSD tests, the longest at
87.9 and 83.9 seconds, with the PCIe tests in the other, which took 123.9. A
test checks the model's test threads and the path of the JUnit report that
`test-openvmm` reads against OpenVMM's pinned nextest configuration.

A shard job spends a median 35 seconds beside its tests in the runs since
[#783](https://github.com/microsoft/nvx/pull/783): 24 to set up, of which the
content's download takes 2 to 5, about 6 in `test-openvmm` around the tests,
mostly for nextest to extract the archive and list the tests, and 5 to end.
Two shards therefore take about 2.7 minutes each instead of the single job's
5.1, at about the same WHP runner time, since the shards also stall less
behind the heavy tests. Applied to each of the 124 runs' own durations, the
longer shard takes a median 125.8 seconds, a p90 131.3, and at most 154.6,
against the 278.0, 287.6, and 309.4 that the unsharded runs took. In the 14
first-attempt runs since #783 that ran the VMM tests, all from October 23, two
shards would have ended the WHP lane a median 2.5 minutes sooner, 20.1
minutes after the run's first job started instead of 22.5. A third shard
would take 88 seconds, the floor that the longest test sets, and so save at
most another 36 seconds for another runner and another setup. The KVM lane,
though, the Linux build and the 10.5-minute KVM leg, ended a median 20.4
minutes after the run's first job started, and would have ended after the
two shards in 8 of those 14 runs, so a third shard would have shortened the
VMM tests of at most six of them, by at most 0.6 minutes. A fourth would save
nothing.

The Linux legs are not sharded. Their filters select tests by pattern, which
a partition could list only from each archive's tests at run time. In run
[39948600574](https://github.com/microsoft/nvx/actions/runs/39948600574), the
KVM leg's 61 tests took 611 seconds, four of them 190 to 267 seconds each. In
four of the eight runs above in which the KVM lane would have ended last, the
Linux build or the KVM leg had waited three minutes or more for one of KVM's
four general runners, so a second KVM leg would often have waited for one too.
Every VMM-test job uploads its test duration report, so that later runs show
whether the KVM lane ends last often enough, and by enough, to shard it too.

CI runs each shard in a job of its own. `openvmm-vmm-tests-windows` is a matrix
of the WHP shards, which passes each leg its `matrix.shard` and the matrix's
`strategy.job-total` as `--shard INDEX/COUNT`, on WHP's `general` runners.
Each leg downloads the content of the one Windows build, which its rerun
downloads again instead of rebuilding. A leg fails before it runs a test
unless the file puts each test of every sharded filter in exactly one shard,
with its duration, and splits the WHP tests into the run's COUNT shards, and a
test checks that the matrix runs exactly the shards of the partition, so that
the legs run every WHP test exactly once. A shard that fails does not cancel
the other (`fail-fast: false`), so each keeps its diagnostics and its test
duration report under names of its own, and *Re-run failed jobs* reruns only
the shards that failed. For a `dev` run, use that partial rerun only under the
[procedure above](#run-concurrency). GitHub expands the matrix only once the
job's condition holds, so the shards run together or not at all, and the
job's fork, draft, change, and cancellation conditions are those of the
single job before. The job's result, which `Required status check`, `Publish
development release`, and `Persist performance baseline` read, succeeds only
when every shard succeeds, so a shard that fails or is cancelled blocks them.

The first plan took the medians of the durations that nextest printed in the
WHP jobs of the 124 successful runs that the file lists, from 03:01 UTC on
October 22 to 12:01 UTC on October 23, 2026, since no job recorded a test
duration report before the shards, with the WHP runners' eight test threads.
A sharded run fails before it runs a test unless the file puts each test of
every sharded filter in exactly one shard, with its duration, and splits the
backend's tests into the run's COUNT shards. Tests in
`scripts/test_nvx_tools.py` check the same, that the file is what the planner
makes of its own durations, and that the shards' filters select each test of
the WHP filter exactly once, so a change that adds a test to the WHP filter,
as [#135](https://github.com/microsoft/nvx/issues/135) would, removes one, or
renames one fails them until it plans the shards again:

1. Download the test duration reports of recent CI runs that ran the WHP
   tests, such as the last ten:

   ```bash
   gh run download RUN_ID --repo microsoft/nvx \
       --pattern 'openvmm-vmm-test-durations-*' --dir build/vmm-durations/RUN_ID
   ```

2. Plan the shards from them:

   ```bash
   python3 scripts/nvx.py plan-openvmm-test-shards --report build/vmm-durations
   ```

   A test that no report measures keeps its duration in the file. A new test
   has none, so give it an estimate, such as
   `--seconds ttrpc::test_ttrpc_uefi_boot=9.4`, which the report of a local
   `test-openvmm --content` run gives. A regeneration from CI's reports
   replaces the estimate once CI has run the test.

3. Commit the file with the change. The planner prints the shards with the
   times that nextest takes to run them, and the tests that changed shard. A
   test stays in its shard unless moving it, or swapping it for a test of
   another shard, shortens the longest shard by a second or more. To change
   the number of shards, pass `--shards whp=COUNT` and change the matrix of
   `openvmm-vmm-tests-windows` in `ci.yml` to list the shards from 1 to COUNT.
   The planner notes reports whose runners have another number of logical
   processors than the file's test threads; pass `--test-threads whp=COUNT` to
   plan for them.

## Change classification

`openvmm-changes` lists the files that changed between the pull request's base
and head, between a push's previous and new commits, or between a merge group's
base and head (see [Merge queue](#merge-queue)), and
`nvx.py classify-ci-changes` selects the suites from their paths. The detector
uses `git diff --name-only --no-renames -z` and the classifier's `--null` option
so both sides of a rename and unquoted filenames, including embedded newlines,
participate in the decision. Documentation, every path under `doc/` or ending
in `.md`, selects no suite, even in a suite's input directory, such as
`scripts/setup/README.md`, because no suite builds, boots, or runs it. Every
other path selects each suite that takes it as an input:

- `run-microvm-tests` enables the NVX microVM tests and their debug-kernel
  jobs. Their inputs are the OpenVMM pin, the guests and every input of their
  cache keys, the `nvx` crate, whose executable their scenarios run and whose
  guest binaries every guest ships, the runtime adapter in
  `tests/runtime-adapter/`, which they test, the workflows,
  actions, and CLI modules that their jobs and the jobs that build their
  artifacts run, the runner setup scripts, the Rust pin, and
  `.gitattributes`, which checks out their scripts on Windows.
- `run-benchmarks` enables the platform jobs, which benchmark each backend and
  package its release, and so the performance regression gate of a pull
  request or a merge group. Their inputs are the OpenVMM pin, the guests and
  every input of their cache keys, the `nvx` crate, which every package ships,
  the performance history and expected hosts under `data/`, `VERSION`,
  `LICENSE`, the workflows, actions, and CLI modules that the platform jobs,
  the gate, and the jobs that build their artifacts run, the runner setup
  scripts, the Rust pin, and `.gitattributes`. A pull request or a merge group
  sets it only when one of these inputs changed, but a push sets it for every
  change but documentation; see
  [Development releases and performance history](#development-releases-and-performance-history).
- `run-openvmm-unit-tests` and `run-openvmm-vmm-tests` enable the OpenVMM
  suites. A push, pull request, or merge group sets each only when one of that
  suite's own inputs changed. The unit tests depend on the OpenVMM pin, the
  Rust pin, the runner setup scripts, `ci.yml`, the actions that their job
  uses, `nvx.py`, and the modules that `ci.py` imports. The VMM tests also
  boot the kernel and Alpine initramfs, so they add every input of those
  artifacts' cache keys and the `build-guest-artifacts` action, and run their
  WHP tests in [shards](#openvmm-vmm-test-shards), so they add the modules of
  the commands that their jobs run and the files that those read, such as the
  partition of the shards. Of `nvx-rs`, those are only the inputs
  of the guest binaries, which `GuestRustBuildConstants.SOURCE_PATTERN`
  matches, so a change to its host side or tests runs the NVX microVM tests
  and the platform jobs but not the VMM tests.
- `run-guest-artifacts` enables `artifacts`, `distro-artifacts`, and
  `debug-kernel`, which build the guest artifacts and the CI debug kernel, and
  `run-openvmm-binaries` the three OpenVMM binary producers. Neither has a
  use of its own, so the classifier sets each exactly when a job that uses its
  artifacts runs: the guest artifacts for the OpenVMM VMM tests, the NVX
  microVM tests, or the platform jobs, and the OpenVMM binaries for the
  microVM tests or the platform jobs. Every input of an artifact is an input
  of each job that uses it. Every input of the VMM tests is also an input of
  the microVM tests, which read the Ubuntu and Azure Linux images of
  `distro-artifacts`, so the three guest artifact jobs share the output. The
  debug kernel is built with the guest artifacts, as before, because the
  development release and performance persistence require its build too.

Every CLI module is an input of the microVM tests, and of the platform jobs,
unless none of that suite's jobs runs it, so a new module selects both until
the classifier excludes it. The always-run quality and CLI jobs catch import
and static-analysis failures, so `nvx.py` importing a module at startup does
not alone make it a suite input. `microvm_tests.py` and the modules that only
it uses, including
`microvm_durations.py`, with the `microvm_test_scripts/` that it runs in guests
and the partition of its [shards](#microvm-test-shards),
`microvm_shards.json`, are inputs of the microVM tests alone.
`openvmm_test_shards.py`, with the partition of the
[VMM test shards](#openvmm-vmm-test-shards), `openvmm_test_shards.json`, and
`shard_plans.py`, which both partitions' planners share, are inputs of the
VMM tests and, as every input of theirs is, of the microVM tests, but not of
the platform jobs. `performance.py`
is an input of the platform jobs alone. `nightly_ci.py`, which only the
nightly report workflow runs, `crate_release.py`, which only the crate
release jobs run, and `merge_queue.py`, which only the job that cancels the
runs of superseded merge groups runs, are inputs of neither. Modules that both kinds
of job run, such as `benchmark.py`, which `microvm_tests.py` imports,
`doctor.py`, which every runner validation runs, and `release.py`, whose
`verify` command the OpenVMM builds run, select both.

A push, pull request, or merge group that changes neither OpenVMM suite's
inputs, such as one that changes only the microVM tests, the benchmarks, the
Ubuntu or Azure Linux guests, or the host side of `nvx-rs`, therefore skips the
jobs of both OpenVMM suites, which occupy the WHP runners longest. A pull
request or a merge group that changes only the microVM tests, such as their
scenarios or the runtime adapter, also skips the platform jobs, which wait for
their pool's single benchmark runner, and one that changes only the
performance tooling, such as `performance.py`, the `run-benchmark` action, or
`data/`, skips the microVM tests. One that changes the input of no suite, such
as the CLI tests, nightly reporting, crate publication, the adversarial
campaigns, or the GitHub-hosted checks, runs only the GitHub-hosted jobs.

The nightly schedule runs at 05:17 UTC, and a manual workflow
dispatch starts the same full run. Both select the full matrix before the
detector examines a comparison base. Each full run has a separate concurrency
key, so it neither cancels nor queues behind a `dev` push. Full runs build,
test, benchmark, and package, but publish no development release, persist
nothing, and save no caches, and they build the OpenVMM VMM tests without
restoring them from the
[OpenVMM VMM test cache](#openvmm-vmm-test-cache). A full run of `dev`
publishes the `nvx` crate to crates.io instead, when the crate changed (see
[crates.io releases](#cratesio-releases)). Only `dev` pushes save the
OpenVMM caches, and only pushes and pull
requests save the guest artifact and pip caches, through the `save-cache`
input of the
[`build-guest-artifacts`](../.github/actions/build-guest-artifacts/action.yml)
and [`check-quality`](../.github/actions/check-quality/action.yml) actions;
merge groups save none (see [Merge queue](#merge-queue)). A
full run still builds the guest artifacts that miss the cache, but installs
the Python tools without the pip cache, which `setup-python` saves whenever it
misses. A failed nightly run opens or updates the
[nightly failure report](#nightly-failure-reports).

For a push, pull request, or merge group, the detector runs the full matrix
instead when the comparison base or a commit is unavailable, when the diff or
the classifier fails, or when the classifier reports an output twice or not at
all, because a missing output would skip its jobs.
`Required status check` passes the same outputs to `check-required-ci`, which
expects every scheduled job to succeed and every other job to be skipped. As
in `ci.yml`, it schedules a job only when the jobs that build the artifacts
that it uses are scheduled too.
Tests in `scripts/test_nvx_tools.py` derive each suite's inputs and fail when
a pattern misses one. The OpenVMM suites' come from the actions that their
jobs use, the modules that `ci.py` imports, and the guest cache keys,
including the files of the guest binaries' source identity, and the VMM
tests' also from the modules of the `nvx.py` commands that their jobs run,
with the files that those read. The microVM
tests' and the platform jobs' come from those jobs and the jobs that build the
artifacts that they use: the reusable workflows and actions that these reach,
the files that the cache keys there hash and the source identity's, the CLI
modules of the `nvx.py` commands that they run, which the tests map from
`nvx.py`'s parser, with every module that those import and every file that
those read beside them, and the `nvx` crate, from which the NVX CLI jobs build
the executable. The tests also check that no other file of `nvx-rs` selects
the VMM tests or changes a guest image key, while every one selects the NVX
microVM tests and the platform jobs.

### Development releases and performance history

A `dev` push publishes its commit's development release and appends the
platform jobs' results to the performance history. `Publish development
release` and `Persist performance baseline` require all three platform jobs to
succeed, so a push that skipped them would publish nothing partial or stale:
it would publish no release and record no history. Its commit would have no
release of its own, though, and [`nvx-rs download`](usage.md#download) would
install an ancestor's, which is older still when that ancestor's release
failed. A push therefore runs the platform jobs, and the jobs that build their
artifacts, for every change but documentation, as every push did before the
classifier selected the platform jobs apart from the tests. Of the 108 merges
to `dev` from October 10 to 21, 2026 that changed more than documentation, 10
changed no input of the platform jobs, and each of their pushes spent about
19 benchmark-runner minutes on them: the KVM, MSHV, and WHP platform jobs take
a median of 5.3, 5.5, and 8.6 minutes. Pull requests and merge groups publish
and persist nothing, so they run the platform jobs only when one of their
inputs changed.
[Development release publication](#development-release-publication) describes
how the release job publishes.

## Development release publication

`Publish development release` runs
[`publish_development_release.py`](../scripts/publish_development_release.py),
which publishes through the REST API with `gh api`. It lists the repository's
releases once to find the release, draft or published, that carries the
commit's tag, `v<VERSION>-dev.<first 12 characters of the commit>`, and
creates a draft when none does. From then on, it reads and changes that
release by its ID alone: it uploads each package that the draft lacks, checks
the size and SHA-256 digest of every asset, and publishes the draft as a
prerelease that is never marked latest, at which GitHub creates the tag. A
rerun resumes a draft that an earlier attempt left, without uploading its
valid assets again, refuses to replace an invalid asset that it did not
upload, and only verifies a release that is published already. Addressing the
release by its tag instead, as `gh release` does, would look the tag up again
on every call, through a lookup that reports any failure to find a draft as
`release not found` (#688).

GitHub keeps tags unique only among published releases, so drafts can share
one. When more than one release carries the tag, publication fails before it
changes anything and names their IDs: delete all but one, and rerun the job.
A request that times out, loses its connection, or fails with HTTP 408, 425,
429, or 5xx is attempted up to three times, with a backoff in between, and so
is an upload that GitHub rejects because it failed to save the asset or
because an asset holds the name. The request that creates the draft is never
repeated; after it fails, publication lists the releases again and resumes a
draft that GitHub created anyway. A 404 for the release means that it is
gone, and any other status fails publication with its message. Every attempt
has a timeout, and the job's timeout outlasts a publication in which every
request takes all of its attempts, as a test checks.

Pull requests and merge groups never publish, so try a change to publication
against a scratch repository, never `microsoft/nvx`: run the script with
`GH_REPO` set to that repository, `GITHUB_SHA` to one of its commits, and
packages in `dist/` that pass the script's checks.

## Nightly failure reports

GitHub notifies only one person when a scheduled run fails: whoever last
changed the workflow's schedule or enabled the workflow again.
[`report-nightly-ci`](usage.md#report-nightly-ci) reports a completed nightly
run in one tracking issue instead, the nightly failure report, which a hidden
marker in its body identifies. Only an issue that GitHub Actions or someone
with a role in the repository opened counts, and the marker must open the
body the way the reporter writes it, so an issue that only copies or
mentions the marker cannot take the report over. A failing run opens the
report, or updates it when one is open, and a passing run closes every open
report. The report shows the latest failing run, and a table of every
failing run attempt since it opened, newest first, which its body also
records in a hidden state comment.
Every later failure also adds a comment with its details, which notifies the
issue's participants, such as its assignees. Reporting an attempt again only
refreshes the body, while a rerun's attempt counts as another failure. The
command reports the latest nightly result, which a later run or attempt than
the one that triggered it may hold: the newest scheduled run's latest
completed attempt, which is the previous attempt while a rerun is in
progress. It skips a result older than the newest run that the report
records. A late result therefore cannot reopen, rewrite, or close the
report. When it finds several open reports, it updates the oldest and
closes the others as duplicates.

The [`nightly-ci-report.yml`](../.github/workflows/nightly-ci-report.yml)
workflow runs the command whenever a CI run on `dev` completes, so a failing
nightly run is reported within minutes of its last job. Its `workflow_run`
trigger reacts to the run rather than to its jobs, so it also reports a run
that was cancelled or whose jobs never started, and it adds nothing to
`ci.yml`. Its job runs only for scheduled runs: a `dev` push's run skips it,
pull request and merge group runs, whose branches the trigger excludes, never
start the workflow, and a manually dispatched run notifies the person who
started it.
The job checks out the default branch, never the run's code. Its token can
read the repository, the run's jobs and logs, and their check annotations,
and write issues, and no other workflow can write issues. The job runs one
report at a time; a newer reporter can replace one that waits for its turn,
but it reports the latest nightly result anyway. Its job summary repeats
the report.

The report links every job that failed, timed out, or was cancelled, names
its failing step, and quotes its evidence: the step's own error annotation,
or else the last error line of its log, or, without the log, the job's most
specific annotation. Up to 15 lines of that log, without the echoed
commands, follow in a collapsed block. The command reads annotations and
logs for at most four minutes, and classifies the remaining jobs from their
steps alone. Each job counts in one group:

- Infrastructure failures: GitHub reports a lost or stopped runner, or a job
  that no runner picked up; a setup step failed or timed out, such as
  `Set up job`, a checkout, `Validate runner`, a download, an upload, a tool
  installation, or a cache step; the benchmark step reports that its runner
  is no longer the host of its series' history (see
  [Expected benchmark hosts](benchmarks.md#expected-benchmark-hosts)); or
  the failing step's log shows a network or download error, such as an
  unresolved name, a reset connection, a rate limit, or a server error, or a
  full disk, other than in a warning or a notice of a retry, which the retry
  may have overcome. Only GitHub's runner errors count for the whole job, so
  that a later step, such as an upload of diagnostics that fails on the
  network, cannot hide a test failure.
- Test and build failures: every other job that failed or timed out. Without
  positive evidence of the infrastructure, a failure counts here, so that the
  report never dismisses a regression; the curl and wget errors of the
  microVM tests' guest network checks count here too.
- Cancelled jobs: jobs that stopped because their run was cancelled.

`Required status check` fails whenever another job fails, so the report
lists it apart, unless it fails alone, for example because a job that
should have run was skipped. A run that fails without a failing job, as when
GitHub cannot start it, is reported with no jobs.

The workflow assigns each new report to its triage owner, the CI maintainer
`ppenna` by default, and labels it `bug`, which the repository defines. The
owner triages a new report within a working day:

1. Open the failed jobs of the latest failing run. For an infrastructure
   failure, remove its cause, then rerun the run's failed jobs; a rerun that
   passes closes the report. A runner that fails `Validate runner` needs the
   [runner setup](../scripts/setup/README.md) again, and one that stays
   offline needs its virtual machine restarted or replaced.
2. For a test or build failure, find the change that caused it, typically
   one that merged since the last passing nightly run without running the
   failing suite, and open an issue or a pull request that links the report.
3. Leave the report open until a nightly run passes, which closes it. Close
   it by hand only when nothing needs a fix, as after a deliberate
   cancellation.

To hand triage over or to rotate it, change the workflow's `--assignee`
options, and reassign the open report.

## crates.io releases

A scheduled or manually dispatched full run of `dev` publishes the
[`nvx` crate](../nvx-rs/README.md) to crates.io once every job of the run has
passed, which `Required status check` confirms. Two jobs on GitHub-hosted
Ubuntu runners, whose checkouts have the LF line endings of the commit, run
[`publish_nvx_crate.py`](../scripts/publish_nvx_crate.py): `crate-package`,
`Package nvx crate`, waits for that check, and `crate-release`,
`Publish nvx crate`, waits for `crate-package`. Pushes, pull requests, and
merge groups never publish the crate. A run publishes a new version only when `nvx-rs/`
changed since the commit from which the newest version on crates.io was
packed, which that version's `.cargo_vcs_info.json` records. A night without
changes to the crate publishes nothing, and neither does a run of a commit
that the newest version's commit already contains, such as a rerun of an
older nightly run. Such a run skips `Publish nvx crate`.

The versions are pre-releases, such as `0.1.0-alpha.4`. A run takes the
number after the highest that crates.io has in the series that
`nvx-rs/Cargo.toml` declares, yanked versions included, since crates.io never
reuses a version, and at least the declared number, so the declared version is
the floor: a pull request changes it only to start another series, as
[The nvx crate](distribution.md#the-nvx-crate) describes. `Package nvx crate`
fails rather than publish a version that would sort below the newest one on
crates.io. When the number differs from the declared one, each job gives only
its own checkout the new version, in `nvx-rs/Cargo.toml` and in `nvx`'s entry
in `nvx-rs/Cargo.lock`. The package's `.cargo_vcs_info.json` then records the
run's commit as dirty, and its `Cargo.toml.orig` and `Cargo.lock` differ from
the commit's files in the version alone.

`Package nvx crate` checks the package that `cargo package --locked` packs:

- Its `.cargo_vcs_info.json` records the run's commit, `nvx-rs` as the path,
  and a dirty checkout exactly when the job rewrote the version.
- Every file is the commit's byte for byte, which rules out CRLF line endings,
  but for the rewritten version and the files that Cargo generates,
  `Cargo.toml` and `.cargo_vcs_info.json`, which must hold no CR either.
- It holds the license, the readme, the build script, and `artifacts.json`.
- Its files build with every feature, as a dependent's build does. `bundled`
  downloads the Linux KVM and MSHV packages of the release that
  `artifacts.json` pins, which must match their digests and declare the
  crate's control contract.

The job outputs the version and the package's SHA-256. Its build runs the
build scripts and procedural macros of the crate's dependencies, so it holds
no right to publish. `Publish nvx crate` builds nothing: it decides again
whether crates.io needs the version, packs the crate again, and checks that
the package holds the commit's files and the very bytes that
`Package nvx crate` built. Only then does it authenticate with crates.io,
publish the package, wait until the crates.io index lists the version, and
download it, which must hold the same bytes. A rerun of either job after the
upload checks the published package instead of publishing again. The job
summaries name the version, or the reason why the run published nothing, and
the run's `crates-io` deployment links the version. One `Publish nvx crate`
job runs at a time. One that finds its version taken by another commit fails
rather than publish the next number, which its package job did not build, and
the next nightly run publishes that number.

crates.io trusted publishing issues the token of the publish step for the
OpenID Connect token of a `ci.yml` job in the `crates-io` environment, which
only `dev` may deploy to, so the repository stores no registry token.
`Publish nvx crate` alone requests an OpenID Connect token and deploys to the
environment, and only its publish step sees the registry token, which the
[authentication action](https://github.com/rust-lang/crates-io-auth-action)
revokes when the job ends. To set publication up, once:

1. A crate owner adds a trusted publisher in the settings of the `nvx` crate
   on crates.io: GitHub, owner `microsoft`, repository `nvx`, workflow
   `ci.yml`, and environment `crates-io`.
2. A repository administrator creates the `crates-io` environment and limits
   its deployment branches to `dev`, with no required reviewers, since
   nightly runs are unattended. GitHub creates a missing environment without
   protection rules when a job first deploys to it, so create it first.

A failure of either job in a nightly run opens the
[nightly failure report](#nightly-failure-reports) like any job's failure. A
declared version behind crates.io needs a pull request that declares a newer
series. A newest version that records no commit, or one that the checkout
lacks, stops publication until a crate owner publishes the next version by
hand, as [The nvx crate](distribution.md#the-nvx-crate) describes. To publish
a change without waiting for the nightly run, dispatch the CI workflow on
`dev`.

## Host qualification

`python3 scripts/nvx.py doctor --backend <kvm|mshv|whp>` qualifies a host for
the [time ABI](design/3.2-clock-and-time.md). It runs checks H1 to H7
in order, prints one `NVX-DOCTOR: check=<id> status=<pass|fail> detail="..."`
line per check, and exits with status 1 if any check fails. `--checks` selects
a subset, and `--summary` appends a Markdown table with the CPU generation,
profile, rates, and skew metrics to a file such as `$GITHUB_STEP_SUMMARY`. H4
and H6 run on long qualification schedules, which take about three
minutes; `--ci-schedule` selects CI's short ones. A failure that matches a time
ABI failure code starts its detail with the code in brackets, for example
`[E_PROFILE_HOST_UNKNOWN]`. A check that passes with a warning starts its
detail with `warning:`, and the summary and, in GitHub Actions, a
`::warning::` annotation repeat the warning. A check fails, and never passes,
when it can't read a fact it gates on: missing or unrecognized tool output
fails it, and an error in one check fails only that check. Only evidence
fields, such as the microcode and the invariant-TSC flags, may read `unknown`.

| Check | Implementation |
| --- | --- |
| H1 | `/dev/kvm` or `/dev/mshv` is readable and writable, and a KVM host has no `/dev/mshv`; on Windows, `WHvGetCapability` reports a hypervisor |
| H2 | Vendor, family, model, stepping, microcode, and OS build from `/proc/cpuinfo` or the Windows registry. Then `openvmm --hypervisor <backend> --cpu-fingerprint <path>` writes the host's CPU fingerprint (by default `nvx-cpu-fingerprint-<backend>.json` in the probe directory; `--cpu-fingerprint` overrides it), checks it against the pinned profile that `auto` selects from OpenVMM's catalog, the latest revision of the generation's profile, which every backend shares, and prints one `NVX-CPU-PROFILE:` line. The check never falls back to a host profile, so a CPU on which `auto` falls back fails H2. H2 reports the generation and the profile from that line, so a profile that an OpenVMM pin adds qualifies its hosts without an NVX change. H2 requires exit status 0, `status=pass`, the same backend, and a catalog profile of the reported generation, reports the profile and surface digests, and otherwise fails with OpenVMM's code, for example `[E_PROFILE_HOST_UNKNOWN]` on a CPU that no profile serves or `[E_PROFILE_UNSUPPORTED]` naming every unsupported CPUID bit. On MSHV and WHP, OpenVMM checks the entries outside the profile on a probe partition configured from the profile, as a cold boot does. With `--no-openvmm`, H2 maps the host with NVX's copy of the catalog instead: `skylake-sp` (6/85, steppings 0 to 4, `intel.skylake-sp.v1`), `icelake-sp` (6/106, `intel.icelake-sp.v1`), `emeraldrapids` (6/207, `intel.emeraldrapids.v1`), `alderlake` (6/151 and 6/154, `intel.alderlake.v1`), `milan` (AMD 25/1, `amd.milan.v1`), `genoa` (AMD 25/17, `amd.genoa.v2`), or `turin` (AMD 26/2, `amd.turin.v1`). Any other CPU, including Cascade Lake and Cooper Lake, fails with `E_PROFILE_HOST_UNKNOWN`. A unit test keeps the copy, which records the latest revision of each generation's profile, equal to the profiles of the OpenVMM submodule's pinned revision, which it reads from the gitlink's commit, so the NVX CLI jobs validate the CLI after they check out OpenVMM. The host OS's invariant-TSC flags (`constant_tsc nonstop_tsc`, or the CPUID bit on Windows) are recorded as evidence and never fail the check, because they don't decide what a guest observes: Azure WHP hosts show the CPUID bit but cannot offer invariant TSC to partitions, and their guests measure tens of nanoseconds of skew |
| H3 | `openvmm --x-time-abi-verify` builds the partition and runs the time ABI preflight without running the guest. Its `NVX-TIME-ABI-VERIFY:` line must report `status=ok` for the backend, plausible declared and native TSC rates, the backend's LAPIC rate, and a `cpu_profile` that is a catalog profile, `<vendor>.<generation>.v<revision>` of any generation but a host profile's `host`, which rejects the host profile that `auto` falls back to on a CPU that no built-in profile serves, and, when H2 runs too, a revision of the profile H2 names. A failed preflight reports OpenVMM's code, for example `[E_TSC_SYNC_UNSUPPORTED]` |
| H4 | Samples of the TSC against the host's monotonic clocks, with sleeps between them so the host's CPUs idle: 13 samples 10 s apart, or 3 samples 1 s apart with `--ci-schedule`. Each sample reads the TSC between two reads of a clock, keeping the tightest of 64 brackets; its uncertainty is half the bracket plus half the clock's resolution. A clock that returns the same value to consecutive reads is coarser than one read, so the probe takes its smallest step as its resolution: Hyper-V's reference TSC page advances the Linux clocks in 100 ns steps although `clock_getres` reports 1 ns. The interval stability is judged against a clock that time synchronization never steers, `CLOCK_MONOTONIC_RAW` on Linux and `QueryPerformanceCounter` on Windows: every interval between consecutive samples must be conclusive within 0.25 ppm, and the interval rates must agree within 1 ppm. chrony's frequency updates move `CLOCK_MONOTONIC`'s rate by up to several ppm between seconds on the Azure runners, which says nothing about the TSC. The rate over the whole window must lie within 250 ppm of the rate H3 reports when H3 runs in the same invocation, and H4 reports their deviation to 0.001 ppm. The limit is the guest's budget for the host's clock correction: the guest's [wall-clock discipline](design/3.2-clock-and-time.md#3210-guest-obligations) follows host UTC, so it applies the host's correction on top of a restore's rate deviation, which the time ABI's restore tolerance bounds at 250 ppm, within Linux's frequency limit of ±500 ppm. Beyond the budget the guest doesn't fail: its offset grows to 128 ms, and the discipline steps its clock and counts a discontinuity. A unit test keeps the limit equal to the difference of nvx-rs's frequency limit and its restore deviation limit, and the latter equal to the restore tolerance of the OpenVMM gitlink's commit. The limit doesn't bound the TSC's error alone, as the time ABI v1's 100 ppm did, because no run can tell that error from the wander of the host's reference clock: on `azure-mshv-1`, chronyd's estimate ranged from +2.5 to +189.3 ppm within a day while the TSC held its rate (#1008). Above that old bound, H4 passes with a warning that says how much of the budget the host's correction uses, and that one run can't tell the TSC's error from the reference's wander. On Linux with a synchronized chronyd, that rate is the rate against `CLOCK_MONOTONIC_RAW` corrected by the frequency that chronyd estimates from its whole sample history, as `chronyc -c tracking` reports it, and every rate within chronyd's skew of that estimate must lie within the limit. The reference's wander can move the estimate and its skew within seconds, so when that range straddles the limit, H4 re-reads the estimate every 2 s for up to 60 s and uses the first whose range lies wholly within or wholly beyond the limit, with the same samples, since only the estimate changes; a range that straddles the limit throughout fails as inconclusive. The rate against `CLOCK_MONOTONIC` is evidence only, because it also carries the slew with which chronyd removes its current offset, which reaches hundreds of ppm for seconds after the host's clock jumps (#768). Without chronyd's estimate, on a Linux host whose kernel PLL another time service drives, such as systemd-timesyncd or ntpd, and which `adjtimex` reports synchronized, the rate is the rate against `CLOCK_MONOTONIC_RAW` corrected by the PLL's frequency, `adjtimex`'s frequency with its tick's, which the PLL keeps apart from the offsets that it slews; H4 never reads it with chronyd, which keeps the PLL off and slews through that frequency. Otherwise the rate is measured against `CLOCK_MONOTONIC` on Linux, and the detail says why neither estimate is available, or against `QueryPerformanceCounter` on Windows. On Linux, H4 also reports as evidence the average correction since boot, the rate of `CLOCK_MONOTONIC_RAW` against `CLOCK_MONOTONIC` over the uptime, and the deviation from the backend's rate that it gives: both clocks exclude suspend, and after a small offset at boot only the time service's gradual adjustments separate them, so the average smooths the reference's wander out without saved state, but slews just after boot or a live migration bias it, so H4 doesn't gate on it. A Linux host's clocksource is recorded as evidence |
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
are evidence only, in the doctor and in CI. In CI, `validate-runner` runs the
cheap host-level checks H1, H2, and H4 in every job that uses it, on the short
`--ci-schedule`. The microVM and platform jobs run it after downloading
OpenVMM and the guest artifacts and add H2's CPU profile check and H3, so that
H4 also compares the measured rate with the backend's native rate; the other
jobs pass `--no-openvmm`. The guest warp probe needs a time ABI boot: the
platform jobs add H6 on CI's schedule before their benchmarks, and the microVM
boot and restore scenarios run CI's warp schedule after every boot and restore
and assert its verdict. H5 and H7 remain available for interactive
qualification.

## Rust toolchain

[`rust-toolchain.toml`](../rust-toolchain.toml) pins the exact Rust release
that builds OpenVMM, its test guests, the `nvx-rs` crate, and the
host tools that NVX compiles, such as the doctor's time probe and the Windows
curl shim. Rustup applies the file to every `cargo` and `rustc` run in the
checkout, including in the `openvmm` submodule, and `test-openvmm` also exports
the release as `RUSTUP_TOOLCHAIN` because flowey can run Cargo outside the
checkout. The pin is an exact release rather than `stable`, so runners
provisioned at different times build with the same compiler (#402). Rust 1.99.0
cannot link OpenVMM's `guest_test_uefi`, failing with `undefined symbol: wcslen`
([rust-lang/rust#163614](https://github.com/rust-lang/rust/issues/163614)).

Self-hosted runners keep the release in a trusted `RUSTUP_HOME` that jobs
cannot modify, so the [runner setup scripts](../scripts/setup/README.md#rust-toolchain)
install it with its OpenVMM targets. `validate-runner` disables rustup's
automatic installation and fails a job before it builds anything when its runner
lacks the release, naming the setup script to rerun. The OpenVMM binary cache
key includes a hash of the file, so a new release rebuilds OpenVMM, and a change
to the file runs the OpenVMM test matrix.

To move the pin, first confirm that `guest_test_uefi` links with the new
release:

```bash
rustup toolchain install RELEASE --profile minimal --target x86_64-unknown-uefi
cd openvmm
cargo +RELEASE build -p guest_test_uefi --target x86_64-unknown-uefi --profile release
```

Then change the release in `rust-toolchain.toml`,
`OpenVMMBuildConstants.RUST_TOOLCHAIN`, and `rust.toolchain` in the
[setup tool manifest](../scripts/setup/README.md#tool-versions), which the
setup scripts and the Specula runner setup read, together; a test in
`scripts/test_nvx_tools.py` fails while they differ. Update every self-hosted
runner as the setup guide describes before the change merges.

## Compilation cache

Every self-hosted runner keeps a persistent
[sccache](https://github.com/mozilla/sccache) cache on its local disk, in
`_work/_sccache`, with the 10-GiB limit that the
[runner setup scripts](../scripts/setup/README.md) configure, and Cargo
compiles through it with incremental compilation disabled. CI builds in clean
target directories, so each runner compiles a crate only when its own cache
lacks it, and loses the cache when its VM is reimaged, as by an image upgrade
of its scale set. The OpenVMM jobs that run on these runners start sccache and
report its statistics at the end of the job through the
[`sccache`](../.github/actions/sccache/action.yml) action.

sccache keys a Rust compilation on its arguments, its `CARGO_*` environment,
and the contents of its sources and of each library that it names with
`--extern`. It cannot cache proc macros, build scripts, or executables, so
every build compiles and links them again. On Linux, the linker reproduces a
proc macro's shared library exactly, but MSVC's `link.exe` stamps every DLL
with the time of the link and a random PDB signature. Every Windows build
therefore missed the cache for each crate that uses a proc macro directly,
several times per job because flowey builds `openvmm` and `pipette` in target
directories of their own: 243 of the 1,421 cacheable compilations of a WHP
VMM-test job, and 286 of the 1,318 of a Windows unit-test job (#693). On
Windows, `build-openvmm-tests`, `test-openvmm`, and `test-openvmm-unit`
therefore append `/Brepro` to `_LINK_` when they build, which makes `link.exe`
derive both from the image's contents.

## Rust crates

The required `nvx-rs` job checks the [`nvx` crate](../nvx-rs/README.md),
which holds the NVX mechanics, on GitHub-hosted Ubuntu and Windows runners
through the [`check-nvx-rs`](../.github/actions/check-nvx-rs/action.yml)
action, on the Rust release that `rust-toolchain.toml` pins. It runs rustfmt,
Clippy with warnings denied for all targets and features and for the library
without default features, the unit, protocol
conformance-vector and property, and fake-OpenVMM instance tests, a test that
the direct children of `nvx::host` depend on one another without cycles, rustdoc
with warnings denied, the minimum Rust version check, and the macOS cross-check.
It also validates bundled artifact staging with a fixture package. The action
sets `NVX_BUNDLE=skip` for `--all-features` builds, so that `bundled` stages
nothing instead of downloading the pinned release.

On Linux, the `nvx-rs` job also lints and tests the
[guest binaries](../nvx-rs/README.md#guest-binaries) for
`x86_64-unknown-linux-musl`, the target that guests run, and builds them with
the `guest` profile. The action delegates the musl lint and tests to
[`nvx.py check-guest-binaries`](usage.md#check-guest-binaries), the fresh,
path-remapped build, static-ELF check, and digest manifest to
[`nvx.py build-guest-binaries`](usage.md#build-guest-binaries),
and the prebuilt-binary integration run to
`nvx.py test-guest-binaries`. The same shared routine supplies every native
and Docker guest build, and Alpine, Ubuntu, and Azure Linux ship its nine
executables.

The `nvx-rs` job also checks the
[runtime adapter](../tests/runtime-adapter/README.md) in
`tests/runtime-adapter/`, an independent adapter that builds on `nvx` with a
lockfile of its own, default features off, and the public API only. For each
of its configurations, protocol-only, host-only, and, on Linux, guest-only for
`x86_64-unknown-linux-musl`, the action runs Clippy with warnings denied, the
build, the tests, rustdoc with warnings denied, and a check with the
minimum Rust version of `nvx`, after rustfmt. The tests check that each
configuration builds as an independent adapter's does: it enables only its
side of `nvx`, the guest-only build pulls in neither Tokio nor a
dependency that only the host side declares, and the guest binary is a
static executable. They also check the adapter's own framing
over the control session between the host and the guest: in the host-only
configuration, on Linux and Windows, that the adapter's host program attaches
to the session and exchanges the framing with the guest agent, and in the
guest-only configuration, that its guest agent serves the session, also on a
tty that it puts in raw mode. Each runs against a test broker that sends it
messages that span records, resets, disconnects, and malformed records. The
host-only tests also check the initramfs that runs the guest agent in place of
NVX's agents, the runtime identity that the host program takes from NVX's
identity of its artifacts, and the plan, OpenVMM arguments, and record of a
launch with a layered root. The guest-only tests also run the guest agent's
worker of a layered root against a fake root: its root configuration, the
failure of each stage of the root, which it reports, and its teardown.

The development release job depends on `nvx-rs` and requires its combined
Linux/Windows results to be successful; failed,
cancelled, or skipped crate checks cannot publish a release. Before it uploads
anything, it checks that each platform's package ships `bin/openvmm`, which
`nvx-rs download` installs, and `bin/nvx-rs`, or their `.exe` forms on Windows.

Each `nvx-microvm-guests-{kvm,mshv,whp}` job builds the runtime adapter's
host-only configuration on its backend and runs its consolidated suite, the
[stage scenarios](../tests/runtime-adapter/README.md#stage-scenarios), with
`scripts/nvx.py test-runtime-adapter`, which names the backend in
`NVX_HYPERVISOR`. The command runs each scenario that
`tests/runtime-adapter/scenarios.txt` names, by name, its VM scenarios
included, which are the adapter's ignored tests, and fails once a scenario is
renamed, added, or gone, or does not run and pass. Through NVX's supervisor,
the adapter's host program
launches a VM of the job's artifacts, whose initramfs runs the adapter's guest
agent in place of NVX's agents, with the runtime identity that the artifacts'
build provenance gives. Later processes reattach to the VM, as after a restart
of the orchestrator, find it mismatched under another runtime identity without
signaling it, and stop it. The adapter's layered scenarios boot the Ubuntu
EROFS image over a copy of the scratch template, from the guest artifacts,
where the guest agent's worker, outside PID 1, assembles the root through
`nvx::guest::root`. They check the root, the mappings that the worker applies
beneath it, through the root and through `nvx::guest::mappings` with reserved
targets of its own, and those that the mapper refuses, and that a failed
assembly, an invalid root configuration, and a failed teardown each reach the
host program with the stage that failed, and still end the VM cleanly. Its
template and clone scenarios capture templates, and start and run their
clones. The
Linux jobs build the guest agent, a static musl executable, first, and the
Windows job takes the one that `distro-artifacts` builds on Linux, as it takes
the Ubuntu and Azure Linux images. The adapter's other tests, including those
of its guest agent, run in the `nvx-rs` job. Changes under
`tests/runtime-adapter/` or `nvx-rs/`, which the adapter builds on, therefore
trigger the NVX microVM test matrix.

## nvx-rs mutation testing

Mutation testing tells whether one selection of the `nvx` crate's tests catches
every mutant of some files that another catches, as when new tests replace
others, which is how
[microsoft/nvx#722](https://github.com/microsoft/nvx/issues/722) retired the
golden tests that compared `nvx-rs` with behavior recorded from Python.
[`nvx-rs-mutants.yml`](../.github/workflows/nvx-rs-mutants.yml) runs
[cargo-mutants](https://mutants.rs/), at the release that it pins, on
GitHub-hosted runners on a manual dispatch. It never runs for pull requests, so
no required check depends on it. Its inputs name the files to mutate, as globs
relative to `nvx-rs`, the arguments of the test binaries that select the tests,
such as a module's filters, the arguments of `cargo test` that select the test
targets, such as `--lib` or `--test cli`, the runner, and the number of jobs,
from 1 to 16, that share the mutants. Each job runs cargo-mutants in place, in
its disposable checkout, because the CLI's tests find the artifacts of the
repository that holds the crate, which the copy of the crate that cargo-mutants
makes by default lacks. Each job uploads its outcomes as
`nvx-rs-mutants-shard-<shard>-<run-id>`, which a rerun of the job replaces, and
the `summarize` job merges the latest outcomes of every shard, also when an
earlier attempt of the run reported them, into `nvx-rs-mutants-<run-id>`, whose
`caught.txt`, `missed.txt`, `timeout.txt`, and `unviable.txt` list the mutants
of each outcome, counts them in the run's summary, and fails if a shard
reported nothing.
To check that new tests catch every mutant that the tests that they replace
catch, dispatch it twice on the same files, once with the filters of the
replaced tests and once with the new tests', and compare the mutants that each
run caught or timed out on:

```bash
sort -u replaced/caught.txt replaced/timeout.txt > replaced.txt
sort -u replacement/caught.txt replacement/timeout.txt > replacement.txt
comm -23 replaced.txt replacement.txt  # The mutants that only the replaced tests catch
```

A push that changes the workflow checks it on `src/gpt/crc.rs` with the
library's `gpt::crc::` tests, in two jobs.

## Module diagrams

[`doc/modules`](modules/README.md) publishes diagrams of the modules of the
[`nvx` crate](../nvx-rs/README.md): its top-level modules, the dependencies
between them, and the modules that each contains, on Linux and Windows.
[`module_diagrams.py`](../scripts/module_diagrams.py) draws them. It exports
the modules of the crate's library, with its `cli`, `testing`, and `guest`
features, for each platform's target with
[cargo-modules](https://github.com/regexident/cargo-modules), merges the
platforms' modules, lays out the diagrams with Graphviz, and writes them with
the page that shows them. Unless it analyzes with `cfg(test)`, cargo-modules
drops as a test module every module whose cfg holds when `test` holds and every
other condition fails, such as one under `cfg(not(windows))`, so the script
also reads its analysis with `cfg(test)` and restores those modules, with their
own uses in place of those that the plain analysis moves to the modules that
contain them, and stops at a cfg that depends on `test` in another way. Nobody
edits the diagrams by hand:
[`module-diagrams.yml`](../.github/workflows/module-diagrams.yml) draws them
again after every push to `dev` that changes their inputs, `nvx-rs/`, the Rust
pin, the script, the build constants that name the crate's targets, or the
workflow, as the merge of such a pull request does, and commits them to `dev`
when they change.

Its `draw` job builds cargo-modules, at the release that the workflow pins,
with the Rust release that `rust-toolchain.toml` pins, and restores the build
from a cache that only `dev` pushes save. It runs on a fixed Ubuntu release,
whose Graphviz release lays out the diagrams, and installs Liberation Sans, the
font that they name first, with which Graphviz measures their labels, so that
their layouts depend neither on the runner image's other fonts nor on its
updates. It checks out, without credentials, the newest commit of the branch
that a push or a dispatch names, rather than the commit that started the run,
or a pull request's merge commit, draws the diagrams into `doc/modules`,
summarizes the files that they change, and uploads them as
`module-diagrams-<run-id>`. For a push to `dev`, or a manual dispatch on it,
the `publish` job then checks out the newest `dev` with
`PERFORMANCE_PERSIST_DEPLOY_KEY`, the deploy key with which the
[performance history](#development-releases-and-performance-history) is
persisted, since deploy keys may push to `dev` past its rules. It replaces
`doc/modules` with the uploaded diagrams and, if that changes anything,
commits it as `doc: update module diagrams (<sha>) [skip ci]`, where `<sha>`
is the commit that the `draw` job drew, and pushes it, retrying up to three
attempts in all on the newest `dev` when a push fails because `dev` moved. The
job builds and runs nothing of the commit, refuses an upload that holds
anything but the page and SVG files, and commits nothing outside
`doc/modules`. `[skip ci]` keeps its commit, which changes only documentation,
from starting the workflows of a push.

Runs on `dev` wait for one another, and a newer push replaces the run that
waits. Each run draws the newest `dev`, so whichever run commits last commits
the diagrams of the newest `dev`, in whatever order the runs start. A rerun of
the `publish` job alone, as of a run's failed jobs, reuses the drawing of its
run, which a newer run may have replaced. So, on each attempt, the job compares
the inputs, the paths that start the workflow, at the drawn commit and on the
newest `dev`, and publishes nothing once `dev` has changed them: the push that
changed them started a run that publishes their diagrams.
A ready pull request that changes the inputs runs the `draw` job alone, so that
a change that breaks the drawing fails before it merges, and its summary and
artifact show the diagrams that its merge will commit. The workflow is not
part of `Required status check`. To draw the diagrams locally, install
cargo-modules and Graphviz, and run `python3 scripts/module_diagrams.py`, which
writes them to `build/modules`, or with `--output` to another directory that
holds nothing but an earlier run's diagrams. Leave `doc/modules` to CI: other
Graphviz releases and fonts lay the diagrams out differently.

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
protocol. Besides the guest and OpenVMM artifacts, the target needs the Rust
toolchain and a prebuilt nvx-rs with its provenance for the scenarios that run
it, as [nvx-rs on the target](adversarial-testing.md#nvx-rs-on-the-target)
explains.
Existing persistent microVM and performance runners are not valid
adversarial targets. Loss of the target heartbeat, a policy oracle, or a
teardown/post-campaign boot failure fails the job and requires quarantine and
reimage.

Normal Actions artifacts contain only the guest-text-free public summary,
catalogued case identifiers, and replay manifest. The external provisioner
must collect controller transcripts and complete target logs into
access-controlled security storage. See
[Copilot-driven adversarial testing](adversarial-testing.md)
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
`rust-toolchain.toml`, the crate manifest, and the setup tool manifest
pin. It restores the shared guest artifacts
through the `restore-only` input of
[`build-guest-artifacts`](../.github/actions/build-guest-artifacts/action.yml)
and the KVM OpenVMM binary that `build-openvmm` caches for the pinned revision,
so agents can run microVM tests without rebuilding either. It doesn't build
`nvx-rs`: the microVM tests build it with Cargo, and record its provenance,
the first time that they need it. The guest artifact
restore computes its keys with the same action that saves them. The binary
restore repeats the key, paths, and gzip compression of the cache entry that
`build-openvmm` saves, and a test in `scripts/test_nvx_tools.py` fails when
they differ, because a mismatch would otherwise only cause a silent cache miss.
Because the agent firewall blocks `cdn.kernel.org` and `cdimage.ubuntu.com`,
even inside containers, the workflow also installs the native guest build
prerequisites, including the pinned Rust toolchain's
`x86_64-unknown-linux-musl` target, allows the unprivileged user namespaces that Alpine's `apk` uses
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
`build-guest-artifacts` action, all `nvx-rs/**` files, the files that pin its Rust, cargo-nextest,
and shell linter versions, `requirements-dev.txt`, `SOURCE-MANIFEST.json`,
`.gitmodules`, the OpenVMM submodule pin, and the NVX CLI and its modules.

Copilot code review uses
[`copilot-code-review.yml`](../.github/workflows/copilot-code-review.yml)
instead. Reviews do not build code, so it only checks out the repository and
does not install the build and test dependencies used by cloud agent sessions.

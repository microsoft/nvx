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
Ubuntu initramfs. Failure logs from the NVX layer are uploaded per backend.
The restore-processor scenario also rejects Linux TSC instability diagnostics,
even if the requested CPUs came online, so clock skew cannot silently pass by
falling back to a different clocksource. After the 1/2/4/8-CPU restores, it
restores the same snapshot once without `--restore-processors`. Every restore
runs with OpenVMM lifecycle profiling and must report exactly one
`startup.vp_thread_bind` record. Its `startup.vp_bind_*` records must show that
an explicit MSHV target binds exactly VPs `0..N-1`, while untargeted MSHV
restores and all KVM and WHP restores bind the full capacity.
On MSHV and WHP, the capture waits until Linux replaces its transitional
`tsc-early` clocksource. A snapshot taken earlier can fail after restore
without any cross-CPU skew, because the clocksource watchdog compares
`tsc-early` with jiffies across the restore downtime, as described in
[the benchmark guide](benchmarks.md).
A restore fails as soon as its guest prints `NVX-RESTORE-PROCESSORS-FAIL`,
rather than waiting for the phase timeout. Restore logs also record OpenVMM's
`adjusted restored vCPU TSC` event for each VP, which includes the applied
snapshot downtime, and its `aligning restored AP TSCs to the BSP` event, which
reports how many created MSHV APs were aligned. When the guest reports
`unstable-tsc`, the harness boots a never-restored eight-vCPU guest with the
same forced warp check and reactivates each AP 20 times. The error then states
whether this control also found TSC instability, which points to host or
hypervisor clock skew rather than restore alignment, and whether the host CPU
exposes an invariant TSC. The control log is kept as
`restore-processors-tsc-control.log`. The control only classifies the
failure; the restore still fails.

Linux runners must expose an invariant TSC, reported as `nonstop_tsc` in
`/proc/cpuinfo`. The `validate-runner` action prints each runner's kernel, CPU
model, clocksource, and TSC flags, and fails the job when `nonstop_tsc` is
missing. On an MSHV runner VM whose Azure host hid the invariant TSC,
never-restored guests also hit cross-vCPU TSC warps during CPU activation, and
keeping every host CPU out of idle removed them (#211). Redeploy such a VM on a
host that exposes an invariant TSC instead of retrying its jobs.

The `restore-tsc-sync` scenario repeats the restore-processor sequence with
the test-only kernel option `clearcpuid=tsc_adjust`. Linux normally skips its
cross-CPU TSC warp test when `IA32_TSC_ADJUST` is available and consistent
within a package. This scenario verifies that the feature is masked, forcing
the live CPU-online check even on those hosts, while retaining the existing
TSC-instability guard. It does not force a fallback clocksource or retry failed
restores. Its logs are kept in a separate `restore-tsc-sync` subdirectory.
Run it alone on Windows with:

```powershell
python scripts\nvx.py test-microvm --backend whp --scenario restore-tsc-sync
```

This regression targets the WHP clock instability tracked in #19; a passing
frozen-counter check is not sufficient to validate a fix.

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
shows that `stop` unmounted the share and overlay first. Linux/KVM runs the
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

The kernel and initramfs cache keys include
[`build_config.py`](../scripts/nvx_tools/build_config.py) and
[`build_constants.py`](../scripts/nvx_tools/build_constants.py), so shared build
configuration or constant changes invalidate cached guest artifacts and their
provenance. The Ubuntu distro layer shares the Ubuntu input hash.

The producer handoff uses one-day workflow artifacts rather than caches. Each
consumer downloads both the normalized executable and its build provenance,
then restores executable permissions on Linux. Once the required artifacts are
ready, benchmarks run in parallel with the NVX test layer and use any available
runner in the matching backend pool. All three use virtual-machine performance
series and the constrained eight-CPU affinity policy. Development releases and
performance baseline updates still require every applicable test and benchmark
lane to pass. The workflow uses the read-only OpenVMM deploy key stored in the
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

## Rust crate

The required `aci-edge-sandboxes` job checks the [`aci_edge_sandboxes` crate](../aci_edge_sandboxes/README.md) on
GitHub-hosted Ubuntu and Windows runners through the
[`check-aci-edge-sandboxes`](../.github/actions/check-aci-edge-sandboxes/action.yml) action. The action
runs rustfmt, Clippy with warnings denied, and rustdoc, all on the Rust
toolchain that MXC pins. It also runs the unit, mock, and fake-OpenVMM
integration tests, checks the declared minimum Rust version, and cross-checks
the macOS build that MXC compiles. The fake-OpenVMM tests drive the real
OpenVMM backend through its control protocol without a hypervisor.
The development release job depends on `aci-edge-sandboxes` and requires its combined
Linux/Windows result to be successful; failed, cancelled, or skipped crate
checks cannot publish a release.

Each `nvx-microvm-tests-{kvm,mshv,whp}` job then runs
`nvx.py test-aci-edge-sandboxes` on its self-hosted runner. This command drives a
complete provision, start, exec, stop, start, and deprovision cycle of the
Alpine guest with the crate's OpenVMM backend. It also checks cancellation,
that guest state lasts only until a stop, that a start terminates the VM of an
interrupted earlier start, host path mappings, and egress rules. On failure
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
so agents can run microVM tests without rebuilding either. Because the agent
firewall blocks `cdn.kernel.org` and `cdimage.ubuntu.com`, even inside
containers, the workflow also installs the native guest build prerequisites,
allows the unprivileged user namespaces that Alpine's `apk` uses for package
triggers, and stages the pinned Linux and Ubuntu Base archives, so agents can
rebuild guest artifacts with `build-guest --native`. When a runner has a
separate `/mnt` disk with more free space than `/`, it mounts the workspace
there. A failed step would make Copilot skip every later setup step, so each
step after checkout continues on error. The last step sets `NVX_COPILOT_SETUP`
to `complete` or `incomplete`, lists failed step IDs in
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

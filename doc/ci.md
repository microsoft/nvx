# Continuous integration

The GitHub Actions workflow has two microVM test layers on Azure-hosted
self-hosted KVM, MSHV, and WHP virtual machines. Each backend has a pool of
three runners labeled by operating system, backend, and `virtual-machine`.
Jobs target the shared backend labels so any available matching runner can
execute them. This allows the backend lanes to execute concurrently without
binding a workload to a specific host. `openvmm-vmm-tests` builds its Xen PVH
probe entirely from the OpenVMM checkout and exercises OpenVMM lifecycle,
TTRPC, and snapshot contracts without restoring NVX guest artifacts.
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
falling back to a different clocksource.

The `restore-tsc-sync` scenario repeats the 1/2/4/8-CPU restore sequence with
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
fresh ext4 scratch copy. Linux/KVM runs the broader Ubuntu SMP, managed
lifecycle, network snapshot, blockless snapshot, and workload-identity set.

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

## Adversarial campaigns

The separate
[`adversarial.yml`](../.github/workflows/adversarial.yml) workflow runs
Copilot-driven campaigns only on trusted manual dispatches or schedules from
`dev`. It is not part of pull-request CI. Each matrix lane uses the matching
persistent KVM, MSHV, or WHP runner and downloads the latest packaged release
before running the controller and executor locally. The runner bootstrap
installs a pinned, SHA-256-verified Copilot CLI; the workflow does not install
the CLI or initiate login.

Copilot authentication is the `COPILOT_GITHUB_TOKEN` secret in the
`adversarial` GitHub Environment, which is restricted to `dev`. The token must
be a personal-account-owned fine-grained token with only the
`Copilot Requests` account permission. It is exposed only to preflight and
campaign steps. The executor's environment allowlist drops Copilot, GitHub,
and SSH credentials. Because controller and executor still share one host and
service account, that allowlist is not a host-escape security boundary.

Local execution is an explicit operational compromise: it validates OpenVMM
process failures, cleanup, canaries, and post-campaign boots, but it cannot
reliably classify a crash of the runner VM itself and does not reimage the
runner after a campaign. Prefix minimization is disabled in CI so an anomalous
target is not reused for additional attempts. A separate disposable executor
remains required for campaigns intended to validate host-crash classification
or contain a suspected guest-to-host escape.

Normal Actions artifacts contain only the guest-text-free public summary,
catalogued case identifiers, and replay manifest. Full controller transcripts
and target logs remain on the runner and must be transferred to
access-controlled security storage before workspace recycling when an anomaly
requires investigation. The local workflow does not automate that transfer.

When any campaign matrix job fails, a GitHub-hosted reporting job creates or
updates one issue for the workflow run. The issue contains the run, commit,
event, failed-job conclusions, and links to the workflow and job pages. It
does not copy guest output, Copilot transcripts, complete target logs, or
potential escape payloads into the issue. Rerunning the same workflow run
updates and reopens the existing issue instead of creating a duplicate.

See
[Copilot-driven adversarial testing](design/copilot-adversarial-testing.md)
for the architecture and operational contract.

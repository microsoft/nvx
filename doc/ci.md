# Continuous integration

The GitHub Actions workflow has two microVM test layers on Azure-hosted
self-hosted KVM, MSHV, and WHP virtual machines. Each backend has a pool of
three runners labeled by operating system, backend, `virtual-machine`, and a
unique host name. Instance 1 runs `openvmm-tests`, instance 2 runs
`nvx-microvm-tests`, and instance 3 runs benchmarks so all three backend lanes
can execute concurrently. `openvmm-tests` builds its Xen PVH probe entirely
from the OpenVMM checkout and exercises OpenVMM lifecycle, TTRPC, and snapshot
contracts without restoring NVX guest artifacts. `nvx-microvm-tests` consumes
the NVX Linux kernel and Alpine initramfs and exercises Linux, SMP, virtio,
sandbox, and snapshot behavior through the public OpenVMM CLI. Failure logs
from the NVX layer are uploaded per backend.
The restore-processor scenario also rejects Linux TSC instability diagnostics,
even if the requested CPUs came online, so clock skew cannot silently pass by
falling back to a different clocksource.

Shared guest artifacts are built with Docker on a GitHub-hosted Ubuntu runner.
Once they are ready, benchmarks run in parallel with the NVX test layer and
remain pinned to instance 3 of each backend so a rolling history does not mix
hosts. All three use virtual-machine performance series and the constrained
eight-CPU affinity policy. Development releases and performance baseline
updates still require every applicable test and benchmark lane to pass. The
workflow uses the read-only OpenVMM deploy key stored in the
`OPENVMM_DEPLOY_KEY` Actions secret to fetch the private submodule at its pinned
commit. Shared guest binaries and development release packages move through
runner-compatible Actions caches; benchmark results use short-lived workflow
artifacts.
Pull requests gate regressions against recent matching-host history, and
successful pushes to `dev` append their p50 values under `data/`. Metadata-only
performance jobs use GitHub-hosted Ubuntu runners. Provisioning instructions
are in the [runner bootstrap guide](../scripts/setup/README.md).

Persistent runners accept pushes and same-repository pull requests only. Fork
pull requests run the GitHub-hosted validation jobs but do not execute code on
the Azure runner fleet. A maintainer must stage an external contribution on a
trusted repository branch before running the backend matrices.

CI caches only the pinned kernel and legacy initramfs. It does not restore or
save a broker initramfs cache, and the benchmark action does not claim to run
the broker profile. Development release jobs build and publish only explicit
`-legacy` packages.

Broker publication fails closed. A future privileged smoke/E2E job must build
the agent image afresh, boot that exact bundle, and supply an independently
authenticated live-gate proof accepted by `verify-broker-live-gate`. The
publish action rejects an unproved broker archive. Pull-request runs are
cancel-in-progress and do not publish releases.

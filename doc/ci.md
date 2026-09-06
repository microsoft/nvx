# Continuous integration

The GitHub Actions workflow has two microVM test layers on self-hosted KVM,
MSHV, and WHP runners. `openvmm-tests` builds its Xen PVH probe entirely from
the OpenVMM checkout and exercises OpenVMM lifecycle, TTRPC, and snapshot
contracts without restoring NVX guest artifacts. `nvx-microvm-tests` consumes
the NVX Linux kernel and Alpine initramfs and exercises Linux, SMP, virtio,
sandbox, and snapshot behavior through the public OpenVMM CLI. Failure logs
from the NVX layer are uploaded per backend.

Benchmarks run only after both test layers pass (or are intentionally skipped)
on bare-metal KVM, MSHV, and WHP hosts and on nested-virtualization MSHV and WHP
virtual machines. Each host type has a distinct result cache and rolling
performance history. The workflow uses the read-only OpenVMM deploy key stored
in the `OPENVMM_DEPLOY_KEY` Actions secret to fetch the private submodule at its
pinned commit. Shared guest binaries, benchmark results, and development
release packages move between jobs through runner-compatible Actions caches.
Pull requests gate regressions against recent matching-host history, and
successful pushes to `dev` append their p50 values under `data/`. The runners
require the [platform prerequisites](setup.md#prerequisites).

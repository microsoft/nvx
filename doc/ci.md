# Continuous integration

The GitHub Actions workflow builds guest artifacts on Linux, then builds and
exercises OpenVMM on self-hosted runners labeled `linux, kvm`, `linux, mshv`,
and `windows, whp`. Benchmarks run on bare-metal KVM, MSHV, and WHP hosts and
on nested-virtualization MSHV and WHP virtual machines. Each host type has a
distinct result cache and rolling performance history. The workflow uses the
read-only OpenVMM deploy key stored in the `OPENVMM_DEPLOY_KEY` Actions secret
to fetch the private submodule at its pinned commit. Every performance series
publishes 23 non-Python metrics. Shared guest binaries, benchmark results, and
development release packages move between jobs through runner-compatible
Actions caches. Pull requests gate regressions against recent matching-host
history, and successful pushes to `dev` append their p50 values under `data/`.
The OpenVMM test matrix is skipped when the push or pull request changes none
of its workflow, harness, submodule, kernel, or Alpine guest inputs; downstream
benchmarks still run after that intentional skip but remain blocked by test
failures. The runners require the [platform prerequisites](setup.md#prerequisites).

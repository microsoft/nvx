# Continuous integration

The GitHub Actions workflow builds guest artifacts on Linux, then builds and
exercises OpenVMM on self-hosted runners labeled `linux, kvm` and
`windows, whp`. It uses the read-only OpenVMM deploy key stored in the
`OPENVMM_DEPLOY_KEY` Actions secret to fetch the private submodule at its
pinned commit. Each platform publishes benchmark tables and JSON/CSV artifacts;
pull requests gate regressions against recent history, and successful pushes to
`main` append their p50 values under `benchmarks/baselines/performance`. The
runners require the [platform prerequisites](setup.md#prerequisites).

# Continuous integration

The GitHub Actions workflow builds guest artifacts on Linux, then builds and
exercises OpenVMM on self-hosted runners labeled `linux, kvm`, `linux, mshv`,
and `windows, whp`. It uses the read-only OpenVMM deploy key stored in the
`OPENVMM_DEPLOY_KEY` Actions secret to fetch the private submodule at its
pinned commit. Each platform publishes the 23-metric non-Python benchmark table
and its JSON/CSV artifacts. Pull requests gate regressions against recent
history, and successful pushes to `dev` append their p50 values under
`benchmarks/baselines/performance`. The runners require the
[platform prerequisites](setup.md#prerequisites).

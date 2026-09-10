# Contributing

## OpenVMM access

OpenVMM is the only private component and is pinned as the `openvmm` Git
submodule.

Access to `https://github.com/nanvix/openvmm-hardfork` is required to initialize or
update the submodule. Cloning this repository does not grant access to it.

## Test ownership

OpenVMM owns checkout-built control-plane coverage. Its microVM tests must not
reference NVX paths, environment variables, kernel binaries, or initramfs
images. Use `scripts/nvx.py test-openvmm --backend BACKEND` to run them.

NVX owns behavior that depends on its patched Linux kernel, Alpine userspace,
guest helpers, SMP behavior, or virtio devices. Add those scenarios to
`scripts/nvx_tools/microvm_tests.py` and run them with
`scripts/nvx.py test-microvm --backend BACKEND`. Keep guest workloads in
`scripts/nvx_tools/microvm_test_scripts` and retain complete failure logs.

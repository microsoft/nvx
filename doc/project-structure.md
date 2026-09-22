# Project structure

This document describes the NVX repository layout. Paths marked as generated
are build products or caches and are not part of the tracked source tree. The
`openvmm/` directory is a Git submodule maintained in a separate repository.

## Repository layout

| Path | Purpose |
| --- | --- |
| `.github/prompts` | Copilot prompts for common development workflows |
| `.github/agents` | Bounded Copilot strategist definitions |
| `.github/specula` | Incremental formal verification adapter and runner setup |
| `kernel` | Reproducible configs and complete Linux patch series |
| `guest` | Common guest sources plus Alpine-control-specific helpers |
| `ubuntu` | Pinned Ubuntu supplemental binary-package lock |
| `openvmm` | Private OpenVMM submodule pinned to `microvm/mshv` |
| `data` | Tracked performance history and generated benchmark data |
| `scripts/nvx_tools` | Retained NVX build and benchmark implementation |
| `scripts/nvx.py` | Canonical build, run, benchmark, and packaging CLI |
| `scripts/nvx_adversarial_executor.py` | Credential-free adversarial executor protocol entry point |
| `.cache/linux` | Generated verified/patched Linux tree; ignored by Git |
| `build/sources` | Generated Linux, Alpine, and Ubuntu release sources; ignored by Git |

## Directory tree

```text
nvx/
|-- .github/                     GitHub automation and Copilot prompts
|   |-- actions/                 Reusable local CI actions
|   |-- agents/                  Bounded Copilot strategist definitions
|   |-- prompts/                 Copilot development workflow prompts
|   |-- specula/                 Incremental formal verification integration
|   |-- workflows/adversarial.yml Trusted scheduled/manual adversarial campaigns
|   `-- workflows/ci.yml         Main build, test, and benchmark workflow
|-- guest/                       Guest-owned scripts and static helpers
|   |-- common/                  Shared init, lifecycle, console, and test helpers
|   |-- alpine/                  Alpine-control container entry helpers
|   `-- ubuntu/                  Ubuntu interactive-shell startup policy
|-- ubuntu/
|   `-- packages.lock.json       Exact supplemental Ubuntu binary package closure
|-- data/                        Benchmark data
|   |-- linux-kvm-virtual-machine*.csv       Rolling Linux/KVM CI histories
|   |-- linux-mshv-virtual-machine*.csv      Rolling Linux/MSHV CI histories
|   `-- windows-whp-virtual-machine*.csv     Rolling Windows/WHP CI histories
|-- build/                       Generated build products (ignored)
|-- dist/                        Generated release packages (ignored)
|-- docker/
|   `-- Dockerfile               Reproducible guest build environment
|-- doc/                         User and contributor documentation
|   |-- benchmarks.md            Benchmark commands and measurement methodology
|   |-- build.md                 Guest and OpenVMM build workflows
|   |-- ci.md                    Continuous integration overview
|   |-- design.md                Design index
|   |-- design/                  MicroVM, sandbox, and snapshot design chapters
|   |-- distribution.md          Packaging and source delivery
|   |-- project-structure.md     This guide
|   |-- run.md                   Guest launch and host mapping
|   `-- setup.md                 Initialization and development prerequisites
|-- kernel/                      Linux configuration and NVX patch set
|   |-- patches/                 Ordered patches applied to Linux
|   |-- COPYING-LINUX            Linux copyright and license notice
|   `-- config-microvm           MicroVM kernel configuration
|-- openvmm/                     Private OpenVMM Git submodule
|-- scripts/                     Build, run, benchmark, and release tooling
|   |-- nvx_tools/               Python implementation behind the NVX CLI
|   |   |-- benchmark.py         OpenVMM benchmark coordinator
|   |   |-- benchmark_scripts/   Shell programs and benchmark templates
|   |   |-- build.py             Artifact build workflows
|   |   |-- build_config.py      Per-invocation build configuration
|   |   |-- build_constants.py   Grouped build pins, paths, and fixed defaults
|   |   |-- performance.py       Performance commands
|   |   |-- adversarial.py       Copilot controller and campaign coordinator
|   |   |-- adversarial_broker.py Typed action catalog and replay journal
|   |   |-- adversarial_executor.py Credential-free target executor
|   |   |-- adversarial_oracles.py Independent canaries and watchdog
|   |   |-- adversarial_cases/   Deterministic campaign catalogs
|   |   |-- collect_alpine_sources.py Alpine source collection
|   |   |-- collect_ubuntu_sources.py Ubuntu source collection
|   |   |-- guests.py           Typed guest descriptors
|   |   |-- ubuntu.py           Verified Ubuntu rootfs and EROFS preparation
|   |   `-- create_linux_source_archive.py Linux source packaging
|   |-- nvx_adversarial_executor.py Restricted adversarial executor entry point
|   |-- nvx.py                   Supported command-line entry point
|   `-- test_*.py                Python tooling tests
|-- .dockerignore                Docker build-context exclusions
|-- .gitattributes               Git path attributes
|-- .gitignore                   Generated-file exclusions
|-- .gitmodules                  OpenVMM submodule definition
|-- LICENSE                      Repository license
|-- pyproject.toml               Pyright and Ruff configuration
|-- README.md                    Project overview and documentation index
|-- requirements-dev.txt         Pinned Python development tools
|-- SOURCE-MANIFEST.json         Pinned source versions, hashes, and outputs
|-- THIRD_PARTY_NOTICES.md       Third-party attribution and notices
`-- VERSION                      NVX release version
```

The ignored `.cache/` directory may also appear at the repository root. It
contains downloaded and prepared upstream source trees, including Linux.

## Source directories

### `.github/`

Repository automation and Copilot customizations live here. `workflows/ci.yml`
defines the main CI pipeline and its job-level orchestration. The `actions/`
directory contains the reusable implementations for validation, artifact
builds, benchmarks, packaging, releases, and performance history management.
The `prompts/` directory defines Copilot prompts for common development
workflows. The `specula/` directory contains the adapter, tests, and dedicated
runner setup for incremental formal verification of the pinned OpenVMM release.

### `guest/` and `ubuntu/`

`guest/common` contains scripts and static helper sources shared by the Alpine
and Ubuntu initramfs builds. `init` controls early boot, emits a stable
distribution marker, and launches either the normal guest shell or the
Alpine-only `nvx-init-agent` sandbox profile. `guest/alpine` contains the
musl-linked container-entry helpers that are not installed in the Ubuntu
initramfs. The sandbox helpers resolve fixed virtio-blk roles through
sysfs, assemble EROFS lower layers over ext4 scratch, place the workload in its
cgroup before release, construct its mount/PID/UTS namespaces, enter its
filesystem root after dropping capabilities, and retain the agent as the outer
PID 1. The remaining common helpers handle shutdown, virtio-fs mounting, and
snapshot preparation. `ubuntu/packages.lock.json` pins the complete
supplemental `.deb` closure installed without maintainer-script execution.

### `data/`

Benchmark data owned by local runs and CI. The platform CSVs at its root are
rolling performance-gate histories maintained by CI. Ignored subdirectories
hold run logs, downloaded artifacts, collected results, and gate inputs.

### `docker/`

The container definition used to build the Linux kernel, Alpine and Ubuntu
initramfs images, and the Ubuntu EROFS distro layer in a reproducible Linux
environment.

### `kernel/`

Inputs owned by NVX for producing the guest kernel. `config-microvm` defines the
kernel build. Files in `patches/` are applied in name order to the pinned Linux
source. `COPYING-LINUX` records the upstream Linux copyright and license
notice. See the [build guide](build.md#building-the-packaged-linux-source) for
kernel-specific details.

### `openvmm/`

A private Git submodule pinned by `.gitmodules` and the parent repository's Git
tree. It contains the VMM implementation and its own source layout,
documentation, tests, and build configuration. Changes to OpenVMM should be
made in that repository and then recorded here by updating the submodule pin.

### `scripts/`

Host-side Python tooling. `nvx.py` is the public entry point; command
implementations live in `nvx_tools/`, and `nvx_tools/build_config.py` carries
the aggregate runtime configuration plus specialized Docker, initramfs,
distro-layer, kernel, and OpenVMM build configurations consumed by each
workflow. Fixed inputs and defaults live in
[`nvx_tools/build_constants.py`](../scripts/nvx_tools/build_constants.py), using
class-qualified constants such as `KernelBuildConstants.VERSION`. The constants
module has no dependencies on the workflow or configuration modules.
Standalone benchmark shell programs and parameterized guest templates
live in `nvx_tools/benchmark_scripts/`.
Source-collection scripts assemble corresponding-source archives for Linux and
Alpine, and Ubuntu. The adversarial controller, typed broker, credential-free
executor, watchdog, and tracked deterministic catalogs also live in
`nvx_tools/`; `nvx_adversarial_executor.py` is the restricted protocol entry
point used by local children and administrator-owned remote wrappers.
Performance scripts analyze benchmark outputs, with adjacent `test_*.py` files
covering those utilities.

## Root files

| Path | Responsibility |
| --- | --- |
| `README.md` | Project overview and documentation index |
| `pyproject.toml` | Strict Pyright policy plus Ruff lint and format settings |
| `requirements-dev.txt` | Pinned Python tools used by contributors and CI |
| `SOURCE-MANIFEST.json` | Exact Linux, Alpine, and Ubuntu source identities and output locations |
| `VERSION` | Distribution version consumed by packaging tools |
| `.gitmodules` | OpenVMM repository URL, path, and tracking branch |
| `.gitignore` | Excludes build products, caches, virtual environments, logs, and platform metadata |
| `.dockerignore` | Limits files sent to the Docker build context |
| `.gitattributes` | Repository-specific Git attributes |
| `LICENSE` | Licensing terms for repository content |
| `THIRD_PARTY_NOTICES.md` | Attribution and redistribution notices for dependencies |

## Generated directories

| Path | Contents |
| --- | --- |
| `.cache/` | Downloaded, verified, and patched upstream source trees |
| `build/` | Kernels, initramfs and EROFS images, package manifests, and collected sources |
| `data/baseline/` | Base-branch histories staged by the performance gate |
| `data/results/` | Collected p50 results for the current commit |
| `data/runs/` | Raw benchmark logs and per-platform artifacts |
| `dist/` | Staged binary and source release archives |
| `openvmm/target/` | Rust build output produced inside the OpenVMM submodule |
| `.ruff_cache/` | Ruff's local lint cache |
| `__pycache__/` | Python bytecode caches that may appear below Python source directories |

Generated paths can be removed and recreated by the build and packaging
commands documented in the other guides in this directory. They should not be
treated as source or edited by hand.

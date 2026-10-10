# Project structure

This document describes the NVX repository layout. Paths marked as generated
are build products or caches and are not part of the tracked source tree. The
`openvmm/` directory is a Git submodule maintained in a separate repository.

## Repository layout

| Path | Purpose |
| --- | --- |
| `.github/workflows` | CI, release verification, adversarial campaign, and Copilot environment workflows |
| `.github/actions` | Reusable local CI actions |
| `.github/skills` | Copilot agent skills for common development workflows |
| `.github/agents` | Bounded Copilot strategist definitions |
| `.github/specula` | Incremental formal verification adapter and runner setup |
| `doc` | User and contributor documentation |
| `docker` | Reproducible guest build environment |
| `kernel` | Reproducible configs and complete Linux patch series |
| `guest` | Common guest sources plus Alpine-control and Ubuntu shell helpers |
| `ubuntu` | Pinned Ubuntu supplemental binary-package lock |
| `azurelinux` | Checksum-pinned Azure Linux supplemental RPM lock |
| `aci_edge_sandboxes` | Rust crate `aci_edge_sandboxes`: state-aware sandbox API with an OpenVMM backend and an opt-in native-agent backend |
| `openvmm` | Public OpenVMM Git submodule from `nanvix/openvmm`, tracking its `main` branch |
| `data` | Tracked performance history and generated benchmark data |
| `scripts/nvx_tools` | Implementation of the `nvx.py` commands |
| `scripts/nvx.py` | Canonical build, run, test, benchmark, and packaging CLI |
| `scripts/setup` | Development host and GitHub Actions runner bootstrap scripts |
| `scripts/nvx_adversarial_executor.py` | Credential-free adversarial executor protocol entry point |
| `.cache/linux` | Generated verified/patched Linux tree; ignored by Git |
| `build/sources` | Generated Linux, Alpine, and Ubuntu release sources; ignored by Git |

## Directory tree

```text
nvx/
|-- .github/                     GitHub automation and Copilot customizations
|   |-- ISSUE_TEMPLATE/          CPU profile request issue form
|   |-- actions/                 Reusable local CI actions
|   |-- agents/                  Bounded Copilot strategist definitions
|   |-- skills/                  Copilot development workflow skills
|   |-- specula/                 Incremental formal verification integration
|   |-- workflows/               GitHub Actions workflows
|   |   |-- adversarial.yml      Trusted scheduled/manual adversarial campaigns
|   |   |-- build-openvmm-binary.yml Reusable OpenVMM binary build
|   |   |-- ci.yml               Main build, test, benchmark, and release workflow
|   |   |-- copilot-code-review.yml Copilot code review environment
|   |   |-- copilot-setup-steps.yml Copilot cloud agent environment
|   |   |-- run-nvx-microvm-tests.yml Reusable NVX microVM test job
|   |   |-- run-platform.yml     Reusable platform benchmark and package job
|   |   `-- specula-release.yml  Specula verification of published releases
|   |-- actionlint.yaml          Self-hosted runner labels for actionlint
|   `-- copilot-instructions.md  Repository-wide Copilot instructions
|-- .vscode/settings.json        Shared VS Code settings
|-- guest/                       Guest-owned scripts and static helpers
|   |-- common/                  Shared init, agent, snapshot, time, and test helpers
|   |-- alpine/                  Alpine-control container entry helpers
|   `-- ubuntu/                  Ubuntu interactive-shell startup policy
|-- ubuntu/
|   `-- packages.lock.json       Exact supplemental Ubuntu binary package closure
|-- azurelinux/
|   `-- packages.lock.json       Checksum-pinned supplemental Azure Linux RPM closure
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
|   |-- contribute.md            Contribution guidelines
|   |-- design.md                Design index
|   |-- design/                  MicroVM, sandbox, and snapshot design chapters
|   |-- distribution.md          Packaging and source delivery
|   |-- guests/                  Guest userland chapters (Ubuntu and Azure Linux)
|   |-- openvmm-upstream-roadmap.md Plan for upstreaming the OpenVMM fork's microVM commits
|   |-- project-structure.md     This guide
|   |-- run.md                   Guest launch and host mapping
|   |-- setup.md                 Initialization and development prerequisites
|   `-- usage.md                 Complete nvx.py command and option reference
|-- kernel/                      Linux configuration and NVX patch set
|   |-- patches/                 Ordered patches applied to Linux
|   |-- COPYING-LINUX            Linux copyright and license notice
|   |-- config-microvm           MicroVM kernel configuration
|   `-- config-microvm-debug     CI debug-kernel fragment (watchdogs on)
|-- aci_edge_sandboxes/          Rust crate `aci_edge_sandboxes` for the state-aware sandbox API
|   |-- model/                   Serializable contract and wire-model crate
|   |-- src/                     Facade and backends
|   |   |-- openvmm/             Default backend that drives the openvmm binary
|   |   |-- agent/               Opt-in backend over a separately supplied native agent library
|   |   `-- bin/                 aci-edge-sandboxes-fake-openvmm test double
|   |-- examples/                Runnable lifecycle examples for the OpenVMM and agent backends
|   |-- tests/                   Mock, fake-OpenVMM, validation, real-hypervisor, and agent-library tests
|   |-- build.rs                 Stages the bundled artifacts (feature `bundled`)
|   `-- artifacts.json           Release package pinned for the bundled artifacts
|-- openvmm/                     OpenVMM Git submodule
|-- scripts/                     Build, run, benchmark, and release tooling
|   |-- nvx_tools/               Python implementation behind the NVX CLI
|   |   |-- aci_edge_sandboxes_tests.py Real-hypervisor aci_edge_sandboxes lifecycle test harness
|   |   |-- adversarial.py       Copilot controller and campaign coordinator
|   |   |-- adversarial_broker.py Typed action catalog and replay journal
|   |   |-- adversarial_cases/   Deterministic campaign catalogs
|   |   |-- adversarial_executor.py Credential-free target executor
|   |   |-- adversarial_oracles.py Independent canaries and watchdog
|   |   |-- archive.py           Reproducible source and release archives
|   |   |-- azurelinux.py        Azure Linux package lock and build-input identity
|   |   |-- benchmark.py         OpenVMM benchmark coordinator
|   |   |-- benchmark_scripts/   Shell programs and benchmark templates
|   |   |-- build.py             Artifact build workflows
|   |   |-- build_config.py      Per-invocation build configuration
|   |   |-- build_constants.py   Grouped build pins, paths, and fixed defaults
|   |   |-- ci.py                CI change classification, required jobs, and OpenVMM tests
|   |   |-- collect_alpine_sources.py Alpine source collection
|   |   |-- collect_ubuntu_sources.py Ubuntu source collection
|   |   |-- common.py            Shared process and configuration helpers
|   |   |-- control_session.py   Control-session client for managed workloads
|   |   |-- create_linux_source_archive.py Linux source packaging
|   |   |-- development_release.py Resumable development release publication
|   |   |-- doctor.py            Time ABI host qualification
|   |   |-- egress_policy.py     Egress policy compiler for OpenVMM's rule grammar
|   |   |-- guests.py            Typed guest descriptors
|   |   |-- host_time_probe.rs   Host TSC probe that doctor builds and runs
|   |   |-- managed_exec_tests.py Managed workload environment, directory, and timeout tests
|   |   |-- microvm_test_scripts/ Guest workloads for the microVM tests
|   |   |-- microvm_tests.py     NVX microVM correctness tests
|   |   |-- openvmm_process.py   Foreground OpenVMM process control for tests
|   |   |-- performance.py       CI performance collection, persistence, and gates
|   |   |-- process_supervisor.py Linux supervisor for one owned process tree
|   |   |-- release.py           Release download, source collection, and packaging
|   |   |-- sandbox.py           Host-side sandbox launch contract
|   |   |-- sandbox_lifecycle.py Managed sandbox lifecycle operations
|   |   |-- sandbox_lifecycle_tests.py Managed sandbox lifecycle and security tests
|   |   |-- time_abi.py          Time ABI harness checks and CPU profile guidance
|   |   `-- ubuntu.py            Verified Ubuntu rootfs and EROFS preparation
|   |-- setup/                   Host and Actions runner bootstrap scripts
|   |-- nvx-hosts.example.json   Template for the ignored .nvx-hosts.json
|   |-- nvx.py                   Supported command-line entry point
|   |-- nvx_adversarial_executor.py Restricted adversarial executor entry point
|   |-- publish_development_release.py Development release publisher run by CI
|   `-- test_*.py                Python tooling unit tests
|-- SECURITY.md                 Microsoft security vulnerability reporting policy
|-- .dockerignore                Docker build-context exclusions
|-- .gitattributes               Git path attributes
|-- .gitignore                   Generated-file exclusions
|-- .gitmodules                  OpenVMM submodule definition
|-- LICENSE                      Repository license
|-- pyproject.toml               Pyright and Ruff configuration
|-- README.md                    Project overview and documentation index
|-- requirements-dev.txt         Pinned Python development tools
|-- rust-toolchain.toml          Pinned Rust release
|-- SOURCE-MANIFEST.json         Pinned source versions, hashes, and outputs
|-- THIRD_PARTY_NOTICES.md       Third-party attribution and notices
`-- VERSION                      NVX release version
```

The ignored `.cache/` directory may also appear at the repository root. It
contains downloaded and prepared upstream source trees, including Linux.

## Source directories

### `.github/`

Repository automation and Copilot customizations live here. `workflows/ci.yml`
defines the main CI pipeline and its job-level orchestration. It calls three
reusable workflows: `build-openvmm-binary.yml` builds OpenVMM for one target,
`run-nvx-microvm-tests.yml` runs the NVX microVM tests on one backend, and
`run-platform.yml` benchmarks one platform and, on `dev` pushes, packages its
release. The other workflows run adversarial campaigns, verify published
releases with Specula, and prepare the Copilot cloud agent and code review
environments. See [Continuous integration](ci.md) for the jobs and gates.

The `actions/` directory contains the reusable implementations for validation,
artifact builds, benchmarks, packaging, releases, and performance history
management. The `skills/` directory defines Copilot agent skills for common
development workflows, and `copilot-instructions.md` holds the repository-wide
Copilot instructions. The `specula/` directory contains the adapter, tests, and
dedicated runner setup for incremental formal verification of the pinned
OpenVMM release.

### `guest/`, `ubuntu/`, and `azurelinux/`

`guest/common` contains scripts and static helper sources shared by the Alpine,
Ubuntu, and Azure Linux initramfs builds, plus the Azure Linux variants of the
container-entry helpers. `init` controls early boot and then starts an
initramfs program named by `nvx_exec=`, the Alpine-only `nvx-init-agent`
sandbox profile, `nvx-managed-agent` for a managed workload, a virtio restore
probe for benchmark profiling, or the interactive shell. On an interactive
boot, `init` prints a stable distribution boot marker for Alpine and Azure
Linux, and `guest/ubuntu/nvx-bashrc` prints Ubuntu's. `guest/alpine` contains
the container launch and entry scripts that only the Alpine initramfs installs;
the entry script stages Alpine's musl loader and `setpriv` inside the
workload's root.

The sandbox helpers resolve fixed virtio-blk roles through
sysfs, assemble EROFS lower layers over ext4 scratch, place the workload in its
cgroup before release, construct its mount/PID/UTS namespaces, enter its
filesystem root after dropping capabilities, and retain the agent as the outer
PID 1. In a managed sandbox, the agent supervises `nvx-managed-agent` and
performs the ordered unmount teardown after a stop request. The remaining
common helpers handle shutdown, virtio-fs mounting, snapshot preparation, and
the guest side of the time ABI, or provide device-access and diagnostic probes.

`ubuntu/packages.lock.json` pins the complete
supplemental `.deb` closure installed without maintainer-script execution.
`azurelinux/packages.lock.json` pins the SHA-256 of every RPM that the Azure
Linux initramfs adds to its digest-pinned base image; the Docker build
downloads exactly those RPMs, verifies their checksums and signatures, and
installs them without resolving packages from a repository.
Update it whenever the base image pin or the added packages change.

### `data/`

Benchmark data owned by local runs and CI. The platform CSVs at its root are
rolling performance-gate histories maintained by CI. Ignored subdirectories
hold run logs, downloaded artifacts, collected results, and gate inputs.

### `docker/`

The container definition used to build the Linux kernel, the Alpine, Ubuntu,
and Azure Linux initramfs images, and the Ubuntu EROFS distro layer in a
reproducible Linux environment.

### `kernel/`

Inputs owned by NVX for producing the guest kernel. `config-microvm` defines the
kernel build. `config-microvm-debug` is a fragment applied on top of it for the
CI debug kernel. Files in `patches/` are applied in name order to the pinned Linux
source. `COPYING-LINUX` records the upstream Linux copyright and license
notice. See the [build guide](build.md#building-the-packaged-linux-source) for
kernel-specific details.

### `aci_edge_sandboxes/`

The Rust crate `aci_edge_sandboxes`, which exposes the five-phase state-aware sandbox lifecycle
(provision, start, exec, stop, and deprovision) to consumers such as MXC. It
defines the contract types, a pluggable backend trait, and the default backend,
which launches the `openvmm` binary and speaks the guest agent's control
protocol. The opt-in `agent` feature adds a second backend, a thin client of a
separately supplied native agent library that owns the whole sandbox lifecycle.
The crate builds independently of the OpenVMM submodule. See its
[README](../aci_edge_sandboxes/README.md) for the API, the policy honor matrix, and tests.

### `openvmm/`

A Git submodule of `nanvix/openvmm`. `.gitmodules` records its URL and tracked
branch, and the parent repository's Git tree pins its exact revision. It
contains the VMM implementation and its own source layout, documentation,
tests, and build configuration. Changes to OpenVMM are made in that repository
and then recorded here by updating the submodule pin; see
[OpenVMM](contribute.md#openvmm).

### `scripts/`

Host-side tooling. `nvx.py` is the public entry point; command
implementations live in `nvx_tools/`, and `nvx_tools/build_config.py` carries
the aggregate runtime configuration plus specialized Docker, initramfs,
distro-layer, kernel, and OpenVMM build configurations consumed by each
workflow. Fixed inputs and defaults live in
[`nvx_tools/build_constants.py`](../scripts/nvx_tools/build_constants.py), using
class-qualified constants such as `KernelBuildConstants.VERSION`. The constants
module has no dependencies on the workflow or configuration modules.

Standalone benchmark shell programs and parameterized guest templates
live in `nvx_tools/benchmark_scripts/`, and the guest workloads of the microVM
tests live in `nvx_tools/microvm_test_scripts/`.
Source-collection scripts assemble corresponding-source archives for Linux,
Alpine, and Ubuntu. The adversarial controller, typed broker, credential-free
executor, watchdog, and tracked deterministic catalogs also live in
`nvx_tools/`; `nvx_adversarial_executor.py` is the restricted protocol entry
point used by local children and administrator-owned remote wrappers.
`performance.py` collects, persists, and gates CI performance results, and the
`test_*.py` files beside `nvx.py` are the tooling's unit tests.

The `setup/` scripts bootstrap development hosts and GitHub Actions runners;
see [Automated environment bootstrap](setup.md#automated-environment-bootstrap).
`nvx-hosts.example.json` is the template for the
[remote agent host](setup.md#remote-agent-hosts) inventory, and CI runs
`publish_development_release.py` to publish development releases.

## Root files

| Path | Responsibility |
| --- | --- |
| `README.md` | Project overview and documentation index |
| `pyproject.toml` | Strict Pyright policy plus Ruff lint and format settings |
| `requirements-dev.txt` | Pinned Python tools used by contributors and CI |
| `rust-toolchain.toml` | Rust release that builds OpenVMM, its test guests, and the repository's Rust code |
| `SOURCE-MANIFEST.json` | Distribution version, OpenVMM microVM ABI and control-protocol versions, and exact Linux, Alpine, Ubuntu, and Azure Linux input identities and output locations |
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
| `aci_edge_sandboxes/target/` | Rust build output of the `aci_edge_sandboxes` crate |
| `.ruff_cache/` | Ruff's local lint cache |
| `__pycache__/` | Python bytecode caches that may appear below Python source directories |

Generated paths can be removed and recreated by the build and packaging
commands documented in the other guides in this directory. They should not be
treated as source or edited by hand.

# Project structure

This document describes the NVX repository layout. Paths marked as generated
are build products or caches and are not part of the tracked source tree. The
`openvmm/` directory is a Git submodule maintained in a separate repository.

## Repository layout

| Path | Purpose |
| --- | --- |
| `.github/workflows` | CI, nightly failure report, module diagram, release verification, adversarial campaign, and Copilot environment workflows |
| `.github/actions` | Reusable local CI actions |
| `.github/skills` | Copilot agent skills for common development workflows |
| `.github/agents` | Copilot custom agents: the bounded adversarial strategist and the issue fixer |
| `.github/specula` | Incremental formal verification adapter and runner setup |
| `doc` | User and contributor documentation |
| `docker` | Reproducible guest build environment |
| `kernel` | Reproducible configs and complete Linux patch series |
| `guest` | Common and distribution-specific guest scripts |
| `ubuntu` | Pinned Ubuntu supplemental binary-package lock |
| `azurelinux` | Checksum-pinned Azure Linux supplemental RPM lock |
| `nvx-rs` | Rust crate `nvx`: the NVX mechanics |
| `tests/runtime-adapter` | Rust crate `runtime-adapter`: an independent adapter that CI builds against `nvx` through its public API |
| `openvmm` | Public OpenVMM Git submodule from `microsoft/openvmm`, tracking its `feature/microvm/dev` branch |
| `data` | Tracked performance history and generated benchmark data |
| `scripts/nvx_tools` | Implementation of the `nvx.py` commands |
| `scripts/nvx.py` | Canonical build, run, test, benchmark, and packaging CLI |
| `scripts/setup` | Development host and GitHub Actions runner bootstrap scripts, and the manifest of the tool versions that they install |
| `scripts/nvx_adversarial_executor.py` | Credential-free adversarial executor protocol entry point |
| `.cache/linux` | Generated verified/patched Linux tree; ignored by Git |
| `build/sources` | Generated Linux, Alpine, and Ubuntu release sources; ignored by Git |

## Directory tree

```text
nvx/
|-- .github/                     GitHub automation and Copilot customizations
|   |-- ISSUE_TEMPLATE/          CPU profile request issue form
|   |-- actions/                 Reusable local CI actions
|   |-- agents/                  Copilot custom agents
|   |-- skills/                  Copilot development workflow skills
|   |-- specula/                 Incremental formal verification integration
|   |-- workflows/               GitHub Actions workflows
|   |   |-- adversarial.yml      Trusted scheduled/manual adversarial campaigns
|   |   |-- build-openvmm-binary.yml Reusable OpenVMM binary build
|   |   |-- ci.yml               Main build, test, benchmark, and release workflow
|   |   |-- copilot-code-review.yml Copilot code review environment
|   |   |-- copilot-setup-steps.yml Copilot cloud agent environment
|   |   |-- module-diagrams.yml  Module diagrams of the nvx crate, drawn after merges
|   |   |-- nightly-ci-report.yml Report of failed nightly CI runs
|   |   |-- nvx-rs-mutants.yml   On-demand mutation testing of nvx-rs
|   |   |-- run-nvx-microvm-tests.yml Reusable NVX microVM test job
|   |   |-- run-platform.yml     Reusable platform benchmark and package job
|   |   `-- specula-release.yml  Specula verification of published releases
|   |-- actionlint.yaml          Self-hosted runner labels for actionlint
|   `-- copilot-instructions.md  Repository-wide Copilot instructions
|-- .vscode/settings.json        Shared VS Code settings
|-- guest/                       Guest-owned scripts
|   |-- common/                  Shared init, agent, snapshot, and probe scripts
|   |-- alpine/                  Alpine-control container entry scripts
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
|   |-- adversarial-testing.md   Copilot-driven adversarial testing
|   |-- benchmarks.md            Benchmark commands and measurement methodology
|   |-- build.md                 Guest and OpenVMM build workflows
|   |-- ci.md                    Continuous integration overview
|   |-- contribute.md            Contribution guidelines
|   |-- design/                  Architecture of the microVM and its guest runtime
|   |-- distribution.md          Packaging and source delivery
|   |-- guests/                  Guest userland chapters (Ubuntu and Azure Linux)
|   |-- modules/                 Module diagrams of the nvx crate
|   |-- openvmm-upstream-roadmap.md Plan for upstreaming the OpenVMM fork's microVM commits
|   |-- project-structure.md     This guide
|   |-- run.md                   Guest launch and host mapping
|   |-- setup.md                 Initialization and development prerequisites
|   `-- usage.md                 Complete nvx-rs and nvx.py command and option reference
|-- kernel/                      Linux configuration and NVX patch set
|   |-- patches/                 Ordered patches applied to Linux
|   |-- COPYING-LINUX            Linux copyright and license notice
|   |-- config-microvm           MicroVM kernel configuration
|   `-- config-microvm-debug     CI debug-kernel fragment (watchdogs on)
|-- nvx-rs/                      Rust crate `nvx` for the NVX mechanics
|   |-- src/
|   |   |-- protocol/            Control-console codec, NVXS session, and the guest targets that live shares may not take, shared by the host and the guest
|   |   |-- activation/          The activation record of a layered root, its canonical encoding, and what Ready reports about it, shared by the host, the guest, and every adapter's framing
|   |   |-- contract.rs          Control-contract versions and release compatibility
|   |   |-- net/                 CIDR parsing and subtraction, and hostname and guest-network forms
|   |   |-- gpt/                 Strict reading of GPT disks whose first partition configures an adapter and whose other partitions hold a root's read-only layers, shared by the host and the guest
|   |   |-- host/                Artifacts, launch, network and egress-policy lowering, sessions, VM instances, sandbox stores, the sandbox launch profile, and the `nvx-rs` CLI (feature `cli`)
|   |   |-- testing/             Fake OpenVMM (feature `testing`)
|   |   |-- guest/               Programs that run in Linux guests, the guest's NVXS connection, the libraries through which any guest init or agent opens the terminal on which it serves its session, starts a program, collects its children's statuses, settles its executions' cgroups, drives each execution, mounts host mappings beneath the descriptors of the roots that it names, assembles and tears down a layered root, and reads a GPT disk's partition table through its block device, and the typed calls through which it runs the time ABI's steps and the reseed (feature `guest`)
|   |   `-- bin/                 nvx-rs CLI, nvx-fake-openvmm test double, and the guest binaries
|   |-- tests/                   Fake-OpenVMM instance lifecycle, sandbox store, CLI scenario, NVXS session, guest-binary, and hypervisor managed-lifecycle tests, the store's record fixtures and sandbox configurations of every format, the cases of the export and root boot tokens, and the conformance vectors of the wire formats
|   |-- build.rs                 Stages the bundled artifacts (feature `bundled`)
|   `-- artifacts.json           Release package pinned for the bundled artifacts
|-- openvmm/                     OpenVMM Git submodule
|-- scripts/                     Build, run, benchmark, and release tooling
|   |-- nvx_tools/               Python implementation behind the NVX CLI
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
|   |   |-- crate_release.py     Checked publication of the nvx crate to crates.io
|   |   |-- create_linux_source_archive.py Linux source packaging
|   |   |-- development_release.py Resumable development release publication
|   |   |-- doctor.py            Time ABI host qualification
|   |   |-- elf.py               Static-executable check of ELF binaries
|   |   |-- guests.py            Typed guest descriptors
|   |   |-- host_telemetry.py    Benchmark host provenance and Windows capture storage telemetry
|   |   |-- host_time_probe.rs   Host TSC probe that doctor builds and runs
|   |   |-- managed_exec_tests.py Managed workload environment, directory, and timeout tests
|   |   |-- merge_queue.py       Cancellation of the CI runs of superseded merge groups
|   |   |-- microvm_durations.py Scenario duration report of the microVM tests
|   |   |-- microvm_test_scripts/ Guest workloads for the microVM tests
|   |   |-- microvm_tests.py     NVX microVM correctness tests
|   |   |-- nightly_ci.py        Nightly CI failure report
|   |   |-- nvx_rs.py            Finds and builds nvx-rs, and runs it for the microVM tests
|   |   |-- openvmm_process.py   Foreground OpenVMM process control for tests
|   |   |-- performance.py       CI performance collection, persistence, and gates
|   |   |-- process_supervisor.py Linux supervisor for one owned process tree
|   |   |-- release.py           Release source collection and packaging
|   |   |-- sandbox_lifecycle_tests.py Managed sandbox lifecycle and security tests
|   |   |-- sandbox_proxy_tests.py Proxy-only sandbox networking tests
|   |   |-- sandbox_ram_upper_tests.py Sandboxes whose overlay upper is in RAM
|   |   |-- storage_events.py    Storage events of a failed Windows benchmark job's window
|   |   |-- time_abi.py          Time ABI harness checks and CPU profile guidance
|   |   `-- ubuntu.py            Verified Ubuntu rootfs and EROFS preparation
|   |-- setup/                   Host and Actions runner bootstrap scripts
|   |   |-- setup-linux.sh        Linux/KVM or Linux/MSHV development-host and Actions runner bootstrap
|   |   |-- setup-windows-whp.ps1 Windows/WHP development-host and Actions runner bootstrap
|   |   `-- tool-versions.conf    Shared tool release and checksum pins
|   |-- check_markdown_links.py  Markdown link, anchor, and doc/ path check run by CI
|   |-- module_diagrams.py       Module diagram generator of the nvx crate
|   |-- nvx-hosts.example.json   Template for the ignored .nvx-hosts.json
|   |-- nvx.py                   Supported command-line entry point
|   |-- nvx_adversarial_executor.py Restricted adversarial executor entry point
|   |-- powershell_testing.py    PowerShell launches of the unit tests, under a shared default time limit
|   |-- publish_development_release.py Development release publisher run by CI
|   |-- publish_nvx_crate.py     nvx crate publisher run by CI's full runs of dev
|   `-- test_*.py                Python tooling unit tests
|-- tests/                       Tests that build on the repository's crates from outside them
|   `-- runtime-adapter/         Rust crate `runtime-adapter`, an independent adapter of nvx
|       |-- src/                 Its framing, its host program, and its guest agent
|       `-- tests/               Its independent build, and its guest agent against a test broker
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
release. The full runs of `ci.yml` on `dev` also publish the `nvx` crate to
crates.io. The other workflows report failed nightly CI runs, draw the module
diagrams of the `nvx` crate after merges, run adversarial campaigns, run the
`nvx` crate's mutation tests, verify published releases with Specula, and
prepare the
Copilot cloud agent and code review environments. See
[Continuous integration](ci.md) for the jobs and gates.

The `actions/` directory contains the reusable implementations for validation,
artifact builds, benchmarks, packaging, releases, and performance history
management. The `skills/` directory defines Copilot agent skills for common
development workflows, and `copilot-instructions.md` holds the repository-wide
Copilot instructions. The `agents/` directory defines Copilot custom agents:
`nvx-adversary`, the bounded strategist of
[adversarial testing](adversarial-testing.md), and `nvx-issue-fixer`, which
takes an issue from its reproduction on the configured hosts to a pull request
with green CI and resolved review threads. The `specula/` directory contains
the adapter, tests, and dedicated runner setup for incremental formal
verification of the pinned OpenVMM release.

### `guest/`, `ubuntu/`, and `azurelinux/`

`guest/common` contains scripts shared by the Alpine, Ubuntu, and Azure Linux
initramfs builds, plus the Azure Linux variants of the container-entry
scripts. The nine compiled `/sbin/nvx-*` programs live under `nvx-rs/src/guest`
and `nvx-rs/src/bin`. `init` controls early boot and then starts an
initramfs program named by `nvx_exec=`, the Alpine-only `nvx-init-agent`
sandbox profile, `nvx-managed-agent` for a managed workload, a virtio restore
probe for benchmark profiling, or the interactive shell. On an interactive
boot, `init` prints a stable distribution boot marker for Alpine and Azure
Linux, and `guest/ubuntu/nvx-bashrc` prints Ubuntu's. `guest/alpine` contains
the container launch and entry scripts that only the Alpine initramfs installs;
the entry script stages Alpine's musl loader and `setpriv` inside the
workload's root.

The sandbox scripts resolve fixed virtio-blk roles through
sysfs, assemble EROFS lower layers over the upper that the host declares, an
ext4 scratch disk or a tmpfs, place the workload in its
cgroup before release, construct its mount/PID/UTS namespaces, enter its
filesystem root after dropping capabilities, and retain the agent as the outer
PID 1. In a managed sandbox, the agent supervises `nvx-managed-agent`, which
enters each execution itself through the library's `nvx::guest::entry`, and
performs the ordered unmount teardown after a stop request. The remaining
common helpers handle shutdown, virtio-fs mounting, snapshot preparation, and
the guest side of the time ABI. The Rust guest binaries provide shutdown,
managed control, restore repair, device access, ingress announcements, and
diagnostic probes.

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

### `nvx-rs/`

The Rust crate `nvx` holds the NVX mechanics: the
authenticated control protocol of the guest agent, artifact discovery and
bundled-release staging, OpenVMM's command line, the lowering of network and
filesystem policies, the compiler of the egress policy files that `nvx-rs run`
and `nvx-rs sandbox` accept, process and platform primitives for Linux and
Windows hosts, the supervision of VM instances, the sandbox launch profile of
`nvx-rs sandbox`, the `nvx-rs` command-line interface that runs NVX's
microVMs and sandboxes, downloads releases, compiles egress policies, and looks
up artifacts, and a fake OpenVMM for tests. Its
`guest` feature builds the nine static executables that every guest image
installs at `/sbin/nvx-*`. It is a standalone crate with its own `Cargo.lock`,
and builds independently of the OpenVMM submodule. See its
[README](../nvx-rs/README.md) and its [module diagrams](modules/README.md).

### `openvmm/`

A Git submodule of `microsoft/openvmm`. `.gitmodules` records its URL and tracked
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
`performance.py` collects, persists, and gates CI performance results.
`host_telemetry.py` records the benchmark host's provenance and, on Windows, the
storage activity during profiled snapshot captures, as
[Snapshot lifecycle profile](benchmarks.md#snapshot-lifecycle-profile) and
[CI collection](benchmarks.md#ci-collection) describe. `storage_events.py` saves
the storage events of a failed Windows benchmark job, as
[CI collection](benchmarks.md#ci-collection) describes. The `test_*.py` files
beside `nvx.py` are the tooling's unit tests. They launch PowerShell through
`powershell_testing.py`, which gives each launch a shared default time limit
and reports what a launch that outlasts its limit printed, at once on stderr
and in the test's failure.

The `setup/` scripts bootstrap development hosts and GitHub Actions runners
using the [tool manifest](../scripts/setup/tool-versions.conf);
see [Automated environment bootstrap](setup.md#automated-environment-bootstrap).
`nvx-hosts.example.json` is the template for the
[remote agent host](setup.md#remote-agent-hosts) inventory, and CI runs
`publish_development_release.py` to publish development releases,
`publish_nvx_crate.py` to publish the `nvx` crate to crates.io (see
[crates.io releases](ci.md#cratesio-releases)), and `module_diagrams.py` to
draw the module diagrams that `doc/modules` publishes (see
[Module diagrams](ci.md#module-diagrams)).

### `tests/`

Tests that build on the repository's crates from outside them.
`runtime-adapter/` is an independent adapter of the NVX runtime: a standalone
crate with its own `Cargo.lock` that depends on `nvx` with default features
off and uses its public API only. Its protocol-only, host-only, and guest-only
configurations build in CI, and its tests check that each one builds as an
independent adapter's does. Its host program and its guest agent exchange the
adapter's own framing over the control session between the host and the
guest, and its tests check each of them against a test broker. Its host
program also runs the guest agent in VMs of NVX's artifacts through NVX's
supervisor, and reattaches to them from other processes. In VMs with a
layered root, the guest agent's worker assembles the root through
`nvx::guest::root`, outside PID 1, and the host program checks it with probes,
in a table of scenarios. Its ignored tests check the VMs on each backend in CI.
See its [README](../tests/runtime-adapter/README.md).

## Root files

| Path | Responsibility |
| --- | --- |
| `README.md` | Project overview and documentation index |
| `pyproject.toml` | Strict Pyright policy plus Ruff lint and format settings |
| `requirements-dev.txt` | Pinned Python tools used by contributors and CI |
| `rust-toolchain.toml` | Rust release that builds OpenVMM, its test guests, and the repository's Rust code |
| `SOURCE-MANIFEST.json` | Distribution version, OpenVMM microVM ABI and control-protocol versions, Rust guest toolchain and source roots, and exact Linux, Alpine, Ubuntu, and Azure Linux input identities and output locations |
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
| `nvx-rs/target/` | Rust build output of the `nvx` crate |
| `tests/runtime-adapter/target/` | Rust build output of the runtime adapter |
| `.ruff_cache/` | Ruff's local lint cache |
| `__pycache__/` | Python bytecode caches that may appear below Python source directories |

Generated paths can be removed and recreated by the build and packaging
commands documented in the other guides in this directory. They should not be
treated as source or edited by hand.

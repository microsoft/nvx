# Project structure

This document describes the NVX repository layout. Paths marked as generated
are build products or caches and are not part of the tracked source tree. The
`openvmm/` directory is a Git submodule maintained in a separate repository.

## Repository layout

| Path | Purpose |
| --- | --- |
| `.github/workflows` | CI, nightly failure report, test change flag, module diagram, release verification, adversarial campaign, and Copilot environment workflows |
| `.github/actions` | Reusable local CI actions |
| `.github/skills` | Copilot agent skills for NVX's own workflows, among them the issue fixer |
| `.github/agents` | Copilot custom agent: the bounded adversarial strategist |
| `.github/specula` | Incremental formal verification adapter and runner setup |
| `doc` | User and contributor documentation |
| `docker` | Reproducible guest build environment |
| `kernel` | Reproducible configs and complete Linux patch series |
| `guest` | Common and distribution-specific guest scripts |
| `ubuntu` | Pinned Ubuntu supplemental binary-package lock |
| `azurelinux` | Checksum-pinned Azure Linux supplemental RPM lock |
| `nvx-rs` | The `nvx` package: the library of NVX's mechanisms, with NVX's own programs as its binaries |
| `openvmm` | Public OpenVMM Git submodule from `microsoft/openvmm`, tracking its `feature/microvm/dev` branch |
| `data` | Tracked performance history and generated benchmark data |
| `scripts/nvx_tools` | Implementation of the `nvx.py` commands |
| `scripts/nvx.py` | Canonical build, run, test, benchmark, and packaging CLI |
| `scripts/setup` | Development host and GitHub Actions runner bootstrap scripts, the manifest of the tool versions that they install, and the Python package that provisions NVX's CI runners on Azure |
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
|   |-- skills/                  Copilot skills for NVX's own workflows
|   |-- specula/                 Incremental formal verification integration
|   |-- workflows/               GitHub Actions workflows
|   |   |-- adversarial.yml      Trusted scheduled/manual adversarial campaigns
|   |   |-- build-openvmm-binary.yml Reusable OpenVMM binary build
|   |   |-- ci.yml               Main build, test, benchmark, and release workflow
|   |   |-- copilot-code-review.yml Copilot code review environment
|   |   |-- copilot-setup-steps.yml Copilot cloud agent environment
|   |   |-- module-diagrams.yml  Module diagrams of the nvx crate, drawn every night
|   |   |-- nightly-ci-report.yml Report of failed nightly CI runs
|   |   |-- nvx-rs-coverage.yml  Coverage of nvx-rs's own tests, for dev and pull requests
|   |   |-- nvx-rs-mutants.yml   On-demand mutation testing of nvx-rs
|   |   |-- nvx-rs-scenario-coverage.yml Coverage of nvx-rs by the KVM microVM scenarios, weekly
|   |   |-- package-platform.yml Reusable platform release package job
|   |   |-- run-nvx-microvm-tests.yml Reusable NVX microVM test job
|   |   |-- run-platform.yml     Reusable platform benchmark job
|   |   |-- scripts-coverage.yml Coverage of the Python tooling's unit tests as dev changes
|   |   |-- specula-release.yml  Specula verification of published releases
|   |   `-- test-changes.yml     Label and comment for pull requests that change existing tests
|   |-- actionlint.yaml          Self-hosted runner labels for actionlint
|   |-- copilot-instructions.md  Repository-wide Copilot instructions
|   `-- pull_request_template.md Sections of a pull request's description
|-- .vscode/settings.json        Shared VS Code settings
|-- guest/                       Guest-owned scripts
|   |-- common/                  Shared init, snapshot, and probe scripts
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
|   |-- acceptance-case-matrix.md The fate of each adapter, CI and docs test, and microVM scenario
|   |-- adversarial-testing.md   Copilot-driven adversarial testing
|   |-- benchmarks.md            Benchmark commands and measurement methodology
|   |-- build.md                 Guest and OpenVMM build workflows
|   |-- ci.md                    Continuous integration overview
|   |-- contribute.md            Contribution guidelines
|   |-- design/                  Architecture of the microVM and its guest runtime
|   |-- distribution.md          Packaging and source delivery
|   |-- guest-agent-guidelines.md Contract of a guest's init agent, with nvx-init as its example
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
|   |-- config-microvm           MicroVM kernel configuration, generated from the feature inventory
|   |-- config-microvm.derived   Rule that sets each symbol of config-microvm beyond the inventory
|   `-- config-microvm-debug     CI debug-kernel fragment (watchdogs on)
|-- nvx-rs/                      The `nvx` package: NVX's library, with NVX's own programs as its binaries
|   |-- src/
|   |   |-- lib.rs               The crate's root, its documentation, and its public modules
|   |   |-- protocol/            Control-console codec, NVXS session, and the guest targets that live shares may not take, shared by the host and the guest
|   |   |-- guest_restore/       The guest-restore record of a layered root, its canonical encoding, and what Ready reports about it, shared by the host, the guest, and every adapter's framing
|   |   |-- contract.rs          Control-contract versions and release compatibility
|   |   |-- net/                 CIDR parsing and subtraction, and hostname and guest-network forms
|   |   |-- gpt/                 Strict reading of GPT disks for NVX's guest programs, private to the crate
|   |   |-- uuid.rs              UUIDs in their canonical text and in GPT's layout, shared by the host and the guest
|   |   |-- error.rs             `nvx::Error` and its `ErrorKind`, and the crate's private `ErrorCode`
|   |   |-- host/                Feature `host`: the runtime and its hypervisor, VMs and their configuration, snapshots, restores, the transport, process references, outcome reports, and the lowering of network postures and host export plans; private to the crate, the platform layer and what only NVX's CLI uses, such as the export planner, the sandbox launch profile, and the egress policy compiler; with feature `cli`, NVX's hidden `nvx-rs` CLI
|   |   |-- testing/             Feature `testing`: NVX's hidden fake OpenVMM
|   |   |-- guest/               Feature `guest`, on Linux: the libraries on which a guest init builds, such as its NVXS session, terminal, host mappings, time ABI steps, reseed, snapshot capture, and guest restore; NVX's hidden guest programs; and NVX's guest space, private to the crate
|   |   `-- bin/                 One file per binary, which calls its program's hidden `main`: nvx-rs, nvx-fake-openvmm, and the ten guest binaries
|   |-- examples/                The external-launch example, and the documented host and guest workflows, which only compile
|   |-- tests/                   Integration tests of the CLI's scenarios, in cli/, the fake OpenVMM, the guest binaries, the NVXS session, in nvxs/, the layering of `nvx::host`, a VM's lifecycle on a hypervisor, the security checklist, and the copies of the OpenVMM submodule's data, with their fixtures and the conformance vectors of the wire formats
|   |-- public-api/              The crate's public API on Linux and on Windows, which CI checks
|   |-- Cargo.toml               Package manifest: features, binaries, examples, and the `guest` profile
|   |-- Cargo.lock               The package's own lockfile
|   |-- LICENSE                  The package's MIT license, which the crate ships
|   `-- README.md                The crate's guide: its modules, the nvx-rs CLI, its features, its guest binaries, and its tests
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
|   |   |-- ci.py                CI change classification, required jobs, and local OpenVMM test runs
|   |   |-- collect_alpine_sources.py Alpine source collection
|   |   |-- collect_ubuntu_sources.py Ubuntu source collection
|   |   |-- common.py            Shared process and configuration helpers
|   |   |-- crate_release.py     Checked publication of the nvx crate to crates.io
|   |   |-- create_linux_source_archive.py Linux source packaging
|   |   |-- development_release.py Resumable development release publication
|   |   |-- doctor.py            Time ABI host qualification
|   |   |-- elf.py               Static-executable check of ELF binaries
|   |   |-- guest_artifact_reuse.py Reuse of a pull request's guest artifacts in its merge group and dev push
|   |   |-- guest_binaries.py    Build, checks, and identity of the nvx package's guest binaries
|   |   |-- guests.py            Typed guest descriptors
|   |   |-- host_telemetry.py    Benchmark host provenance and Windows capture storage telemetry
|   |   |-- host_time_probe.rs   Host TSC probe that doctor builds and runs
|   |   |-- kconfig.py           Kconfig reader that attributes a configuration's symbols to their rules
|   |   |-- kernel_config.py     Generation and checks of the kernel configuration against its inventory
|   |   |-- kernel_features.py   Kernel feature inventory: every setting of the configuration, with its rationale
|   |   |-- managed_exec_tests.py Managed workload environment, directory, and timeout tests
|   |   |-- merge_queue.py       Cancellation of the CI runs of superseded merge groups
|   |   |-- microvm_durations.py Scenario duration report of the microVM tests
|   |   |-- microvm_test_scripts/ Guest workloads for the microVM tests
|   |   |-- microvm_tests.py     NVX microVM correctness tests
|   |   |-- nightly_ci.py        Nightly CI failure report
|   |   |-- nvx_rs.py            Finds and builds nvx-rs, records its provenance, and runs it for the microVM tests
|   |   |-- openvmm_ci.py        Check that OpenVMM's own CI passed the pinned revision
|   |   |-- openvmm_process.py   Foreground OpenVMM process control for tests
|   |   |-- performance.py       CI performance collection, persistence, and gates
|   |   |-- performance_history.py Nightly persistence of the results that dev pushes recorded
|   |   |-- process_supervisor.py Linux supervisor for one owned process tree
|   |   |-- release.py           Release source collection and packaging
|   |   |-- sandbox_lifecycle_tests.py Managed sandbox lifecycle and security tests
|   |   |-- sandbox_proxy_tests.py Proxy-only sandbox networking tests
|   |   |-- sandbox_ram_upper_tests.py Sandboxes whose overlay upper is in RAM
|   |   |-- storage_events.py    Storage events of a failed Windows benchmark job's window
|   |   |-- time_abi.py          Time ABI harness checks and CPU profile guidance
|   |   `-- ubuntu.py            Verified Ubuntu rootfs and EROFS preparation
|   |-- setup/                   Host and Actions runner bootstrap scripts
|   |   |-- azure/                Azure CI runner provisioning of existing scale sets
|   |   |   |-- nvx_azure/        Standard-library Python package that scale-set instances run to become CI runners
|   |   |   |-- linux/            Example Linux scale-set configuration
|   |   |   |-- windows/          Example Windows configuration, Custom Script entry shim, and PowerShell adapters
|   |   |   |-- build_package.py  Offline packager of a scale set's bootstrap payload and model patch
|   |   |   |-- requirements.txt  The packager's Python requirement
|   |   |   `-- README.md         Azure runner provisioning guide
|   |   |-- README.md             Development host and Actions runner bootstrap guide
|   |   |-- setup-linux.sh        Linux/KVM or Linux/MSHV development-host and Actions runner bootstrap
|   |   |-- setup-windows-whp.ps1 Windows/WHP development-host and Actions runner bootstrap
|   |   `-- tool-versions.conf    Shared tool release and checksum pins
|   |-- benchmark_series.py      Benchmark series with CI's validation, for local and remote hosts
|   |-- check_markdown_links.py  Markdown link, anchor, and doc/ path check run by CI
|   |-- check_test_changes.py    Flags the changes of a pull request to existing tests
|   |-- check_test_layout.py     Check that tests live apart from the code that they test
|   |-- ci_triage.py             Classification of a CI run and its first failure
|   |-- host_probe.py            Report of whether a host can run NVX
|   |-- host_worktree.py         Temporary worktrees of an NVX checkout
|   |-- issue_workspace.py       Existing work, topic branch, and cleanup of an issue's fix
|   |-- local_gates.py           The CI gates that a change selects, read from CI, run locally
|   |-- module_diagrams.py       Module diagram generator of the nvx crate
|   |-- nvx-hosts.example.json   Template for the ignored .nvx-hosts.json
|   |-- nvx.py                   Supported command-line entry point
|   |-- nvx_adversarial_executor.py Restricted adversarial executor entry point
|   |-- nvx_hosts.py             Remote host inventory, and commands and scripts on its hosts
|   |-- nvx_rs_coverage.py       Coverage of the nvx crate's own tests, run by CI
|   |-- nvx_rs_scenario_coverage.py Coverage of the nvx crate by the KVM microVM scenarios, run by CI
|   |-- openvmm_git.py           Git and roadmap helpers of the OpenVMM tools
|   |-- openvmm_pin.py           Checks, roadmap facts, and landing of an OpenVMM pin promotion
|   |-- openvmm_release.py       State, checks, publication, and landing of an OpenVMM chain release
|   |-- powershell_testing.py    PowerShell launches of the unit tests, under a shared default time limit
|   |-- pr_threads.py            Pull-request review threads, with checked replies and resolution
|   |-- public_api.py            Public-API snapshot check of the nvx crate run by CI
|   |-- publish_development_release.py Development release publisher run by CI
|   |-- publish_nvx_crate.py     nvx crate publisher run by CI's full runs of dev
|   |-- rebase_check.py          Checks of a branch before and after a rebase
|   |-- scripts_coverage.py      Coverage of the Python tooling's unit tests, run by CI
|   |-- test-layout-allowlist.json The test hooks that production files may hold
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
contains the verified upstream archives and packages that the guest builds
download, under `downloads/`, and the prepared upstream source trees, including
Linux; `NVX_CACHE_DIR` moves the guest builds' cache elsewhere, as
[Build](build.md) describes.

## Source directories

### `.github/`

Repository automation and Copilot customizations live here. `workflows/ci.yml`
defines the main CI pipeline and its job-level orchestration. It calls four
reusable workflows: `build-openvmm-binary.yml` builds OpenVMM for one target,
`package-platform.yml` packages one platform's release,
`run-nvx-microvm-tests.yml` runs the NVX microVM tests on one backend, and
`run-platform.yml` benchmarks one platform. The full runs of `ci.yml` on `dev`
also publish the `nvx` crate to
crates.io. The other workflows report failed nightly CI runs, flag the pull
requests that change existing tests, draw the module
diagrams of the `nvx` crate every night, run adversarial campaigns, measure
the coverage of the `nvx` crate's tests and of the Python tooling's unit tests
as `dev` changes, measure every week which host code of the `nvx` crate the
KVM microVM scenarios reach, run the `nvx` crate's mutation tests, verify
published releases with Specula, and
prepare the
Copilot cloud agent and code review environments. See
[Continuous integration](ci.md) for the jobs and gates.

The `actions/` directory contains the reusable implementations for validation,
artifact builds, benchmarks, packaging, releases, and performance history
management. The `skills/` directory defines Copilot agent skills for NVX's own
workflows, which run their deterministic steps through the tools in `scripts/`:
preparing a host, running and benchmarking microVMs on it, triaging CI runs,
promoting OpenVMM pins, and rebasing the OpenVMM chain. Among them,
`nvx-fix-issue` takes an issue from its reproduction on the configured hosts to
a pull request with green CI and resolved review threads, and runs only when a
user invokes it. Generic workflows, such as commits, pull requests, reviews,
and rebases, are left to the agents' own skills, which follow
`copilot-instructions.md`, the repository-wide Copilot instructions, and
`pull_request_template.md`. The `agents/` directory defines `nvx-adversary`, the
Copilot custom agent that is the bounded strategist of
[adversarial testing](adversarial-testing.md). The `specula/` directory contains
the adapter, tests, and dedicated runner setup for incremental formal
verification of the pinned OpenVMM release.

### `guest/`, `ubuntu/`, and `azurelinux/`

`guest/common` contains scripts shared by the Alpine, Ubuntu, and Azure Linux
initramfs builds. The ten compiled `/sbin/nvx-*` programs live under
`nvx-rs/src/guest` and `nvx-rs/src/bin`. `init` controls early boot and then
starts an initramfs program named by `nvx_exec=`, `nvx-init` for a layered root,
`nvx-managed-agent` for a managed workload, a one-shot
workload named by `nvx_entrypoint=`, a virtio restore probe for benchmark
profiling, or the interactive shell. On an interactive boot, `init` prints a
stable distribution boot marker for Alpine and Azure Linux, and
`guest/ubuntu/nvx-bashrc` prints Ubuntu's.

The other common scripts mount the live host share (`nvx-hostmount`) and
prepare the guest for a snapshot and repair its restores (`nvx-snapshot`); the
microVM tests and benchmarks run the rest as probes (`nvx-identity-probe`,
`nvx-sandbox-smoke`, and `nvx-virtio-restore-probe`). The Rust guest binaries
provide a layered root's PID 1, managed control, the guest side of the time
ABI, restore repair, shutdown, device access, ingress announcements, and
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
kernel build: `nvx.py kernel-config` generates it from the kernel feature
inventory in `scripts/nvx_tools/kernel_features.py`, and records in
`config-microvm.derived` the Kconfig rule that sets each of its other symbols.
`config-microvm-debug` is a fragment applied on top of it for the
CI debug kernel. Files in `patches/` are applied in name order to the pinned Linux
source. `COPYING-LINUX` records the upstream Linux copyright and license
notice. See the [build guide](build.md#kernel-configuration) for the
configuration and [its packaged source](build.md#building-the-packaged-linux-source)
for other kernel-specific details.

### `nvx-rs/`

`nvx-rs/` holds the `nvx` package, the repository's one Cargo package. Its
library, `nvx`, holds the mechanisms of NVX's microVMs: the NVXS session and
the codecs that the host and the guest share, OpenVMM's command line, the
lowering of network postures and host export plans into OpenVMM's options,
verified references to OpenVMM's processes, the creation, spawn, reattachment,
and stop of VMs, their snapshots and restores, and the guest's side of a
snapshot. Its binaries are NVX's own programs: the `nvx-rs` command-line
interface (feature `cli`), which runs NVX's microVMs and sandboxes, downloads
releases, compiles egress policies, and looks up artifacts; a fake OpenVMM for
its tests (feature `testing`); and the ten static executables that every guest
image installs at `/sbin/nvx-*` (feature `guest`). The code that only these
programs use, such as the CLI's stores, export planner, and egress policy
compiler, the platform layer, and the guest's space, is private to the crate,
and each binary calls a `main` that `#[doc(hidden)]` hides, so the snapshots
in `public-api/` record the library's public API alone. The package has its
own `Cargo.lock`, and builds independently of the OpenVMM submodule;
[The nvx package](build.md#the-nvx-package) lists its features, profiles,
binaries, outputs, and license inputs. See its [README](../nvx-rs/README.md)
and its [module diagrams](modules/README.md).

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
`nvx_tools/guest_binaries.py` builds, checks, and identifies the guest binaries
of the `nvx` package, and `nvx_tools/nvx_rs.py` builds `nvx-rs` and records its
provenance; see [The nvx package](build.md#the-nvx-package).
Source-collection scripts assemble corresponding-source archives for Linux,
Alpine, and Ubuntu. The adversarial controller, typed broker, credential-free
executor, watchdog, and tracked deterministic catalogs also live in
`nvx_tools/`; `nvx_adversarial_executor.py` is the restricted protocol entry
point used by local children and administrator-owned remote wrappers.
`performance.py` collects, persists, and gates CI performance results, and
`performance_history.py` persists, in a full run of `dev`, the results that the
`dev` pushes recorded.
`host_telemetry.py` records the benchmark host's provenance and, on Windows, the
storage activity during profiled snapshot captures, as
[Snapshot lifecycle profile](benchmarks.md#snapshot-lifecycle-profile) and
[CI collection](benchmarks.md#ci-collection) describe. `storage_events.py` saves
the storage events of a failed Windows benchmark job, as
[CI collection](benchmarks.md#ci-collection) describes. The `test_*.py` files
beside `nvx.py` are the tooling's unit tests. They launch PowerShell through
`powershell_testing.py`, which gives each launch a shared default time limit
and reports what a launch that outlasts its limit printed, at once on stderr
and in the test's failure. `check_test_layout.py` checks that every test of the
repository lives in a test file, and that production files hold only the test
hooks that `test-layout-allowlist.json` allows them, and
`check_test_changes.py` flags the pull requests that change existing tests; see
[Test layout](contribute.md#test-layout) and
[Changes to existing tests](contribute.md#changes-to-existing-tests).
`public_api.py` checks the public API of `nvx` against its snapshots in
`nvx-rs/public-api/`, and rewrites them; see [Rust crates](ci.md#rust-crates).

The `setup/` scripts bootstrap development hosts and GitHub Actions runners
using the [tool manifest](../scripts/setup/tool-versions.conf);
see [Automated environment bootstrap](setup.md#automated-environment-bootstrap).
On Azure, `setup/azure/nvx_azure` is the standard-library Python package with
which the instances of NVX's CI scale sets provision themselves as runners,
through those scripts, and `setup/azure/build_package.py` packages it into a
scale set's custom data; see
[Azure CI runner provisioning](../scripts/setup/azure/README.md).
`nvx-hosts.example.json` is the template for the
[remote agent host](setup.md#remote-agent-hosts) inventory, and CI runs
`publish_development_release.py` to publish development releases,
`publish_nvx_crate.py` to publish the `nvx` crate to crates.io (see
[crates.io releases](ci.md#cratesio-releases)), `nvx_rs_coverage.py` to
measure which code of the `nvx` crate its tests reach (see
[nvx-rs coverage](ci.md#nvx-rs-coverage)), `nvx_rs_scenario_coverage.py` to
measure which host code of it the KVM microVM scenarios reach (see
[nvx-rs scenario coverage](ci.md#nvx-rs-scenario-coverage)),
`scripts_coverage.py` to measure which code of `scripts/` its unit tests reach
(see [scripts coverage](ci.md#scripts-coverage)), and `module_diagrams.py` to
draw the module diagrams that `doc/modules` publishes (see
[Module diagrams](ci.md#module-diagrams)).

The other standalone tools carry the deterministic steps of the Copilot skills,
and serve contributors as well. `local_gates.py` lists the gates of
[check-quality](../.github/actions/check-quality/action.yml),
[validate-nvx](../.github/actions/validate-nvx/action.yml), and
[check-nvx-rs](../.github/actions/check-nvx-rs/action.yml) that a change
selects, with the commands that those actions define, and runs them.
`pr_threads.py` lists a pull request's review threads, reviews, and
conversation comments, and replies to and resolves threads, checking each
reply and that the pull request's head holds a fix; `ci_triage.py` classifies a
CI run and finds its first failure; and `rebase_check.py` checks a branch
before and after a rebase. `issue_workspace.py` finds the work that may already
cover an issue, and creates and cleans up the topic branch of its fix, in a
worktree that `host_worktree.py` adds when the checkout holds changes.
`nvx_hosts.py` resolves the profiles of the remote host inventory and runs
commands and scripts on those hosts, where `host_probe.py` reports whether a
host can run NVX and `host_worktree.py` adds temporary worktrees.
`benchmark_series.py` runs a benchmark series into a fresh result directory
with the validation of CI's benchmark jobs. `openvmm_pin.py` checks and lands
an OpenVMM pin promotion and derives the
[roadmap](openvmm-upstream-roadmap.md)'s positions from Git, and
`openvmm_release.py` does the same for a release of the OpenVMM chain; both
build on `openvmm_git.py`.

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
| `.cache/` | Downloaded and verified upstream archives and packages, and patched upstream source trees |
| `build/` | Kernels, initramfs and EROFS images, the sandbox scratch template, package manifests, the provenance of OpenVMM, the kernels, the Alpine initramfs, and `nvx-rs`, and collected sources |
| `data/baseline/` | Base-branch histories staged by the performance gate |
| `data/results/` | Collected p50 results for the current commit |
| `data/runs/` | Raw benchmark logs and per-platform artifacts |
| `dist/` | Staged binary and source release archives |
| `openvmm/target/` | Rust build output produced inside the OpenVMM submodule |
| `nvx-rs/target/` | Cargo's build output of the `nvx` package, among it `release/nvx-rs[.exe]` |
| `.ruff_cache/` | Ruff's local lint cache |
| `__pycache__/` | Python bytecode caches that may appear below Python source directories |

Generated paths can be removed and recreated by the build and packaging
commands documented in the other guides in this directory. They should not be
treated as source or edited by hand.

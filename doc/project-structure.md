# Project structure

This document describes the NVX repository layout. Paths marked as generated
are build products or caches and are not part of the tracked source tree. The
`openvmm/` directory is a Git submodule maintained in a separate repository.

## Directory tree

```text
nvx/
|-- .github/                     GitHub Actions and repository automation
|   |-- actions/                 Reusable local CI actions
|   `-- workflows/ci.yml         Main build, test, and benchmark workflow
|-- alpine/                      Files installed in the Alpine guest
|   |-- init                     Guest PID 1 and boot sequence
|   |-- nvx-exit                 Clean guest shutdown helper
|   |-- nvx-hostmount            virtio-fs host mount helper
|   `-- nvx-snapshot             Snapshot preparation helper
|-- benchmarks/                  Benchmark definitions and reference data
|   |-- baselines/               Platform baseline CSV files
|   `-- NVX-METHODOLOGY.md       Workload and measurement methodology
|-- build/                       Generated build products (ignored)
|-- dist/                        Generated release packages (ignored)
|-- docker/
|   `-- Dockerfile               Reproducible guest build environment
|-- doc/
|   `-- project-structure.md     This guide
|-- kernel/                      Linux configuration and NVX patch set
|   |-- patches/                 Ordered patches applied to Linux
|   |-- BUILDING.md              Kernel build notes
|   `-- config-microvm           MicroVM kernel configuration
|-- licenses/                    Vendored third-party license texts
|-- openvmm/                     Private OpenVMM Git submodule
|-- scripts/                     Build, run, benchmark, and release tooling
|   |-- nvx_tools/               Python implementation behind the NVX CLI
|   |   |-- benchmark.py         OpenVMM benchmark coordinator
|   |   |-- performance.py       Performance commands
|   |   |-- collect_alpine_sources.py Alpine source collection
|   |   `-- create_linux_source_archive.py Linux source packaging
|   |-- nvx.py                   Supported command-line entry point
|   `-- test_*.py                Python tooling tests
|-- .dockerignore                Docker build-context exclusions
|-- .gitattributes               Git path attributes
|-- .gitignore                   Generated-file exclusions
|-- .gitmodules                  OpenVMM submodule definition
|-- LICENSE                      Repository license
|-- LICENSE-NVX                  NVX-specific license terms
|-- pyproject.toml               Pyright and Ruff configuration
|-- README.md                    Setup and usage documentation
|-- requirements-dev.txt         Pinned Python development tools
|-- SOURCE-MANIFEST.json         Pinned source versions, hashes, and outputs
|-- THIRD_PARTY_NOTICES.md       Third-party attribution and notices
`-- VERSION                      NVX release version
```

The ignored `.cache/` directory may also appear at the repository root. It
contains downloaded and prepared upstream source trees, including Linux.

## Source directories

### `.github/`

Repository automation lives here. `workflows/ci.yml` defines the main CI
pipeline. The `actions/` directory contains local actions for checking out and
building OpenVMM, running benchmarks, and preparing performance results.

### `alpine/`

Guest-owned scripts copied into the Alpine initramfs. `init` controls early
boot and launches the guest shell. The `nvx-*` helpers handle shutdown,
virtio-fs mounting, and snapshot preparation from inside the guest.

### `benchmarks/`

Benchmark documentation and expected results. `baselines/` stores Linux/KVM
and Windows/WHP CSV reference data. Its `performance/` subdirectory holds the
rolling performance-gate baselines maintained by CI. The supported CLI in
`scripts/nvx.py` exposes the coordinator implemented by
`scripts/nvx_tools/benchmark.py`.

### `docker/`

The container definition used to build the Linux kernel and Alpine initramfs
in a reproducible Linux environment.

### `kernel/`

Inputs owned by NVX for producing the guest kernel. `config-microvm` defines the
kernel build. Files in `patches/` are applied in name order to the pinned Linux
source. See `BUILDING.md` for kernel-specific details.

### `licenses/`

Copies of licenses that must accompany distributed third-party components.
The Linux license text is stored as `COPYING-LINUX`.

### `openvmm/`

A private Git submodule pinned by `.gitmodules` and the parent repository's Git
tree. It contains the VMM implementation and its own source layout,
documentation, tests, and build configuration. Changes to OpenVMM should be
made in that repository and then recorded here by updating the submodule pin.

### `scripts/`

Host-side Python tooling. `nvx.py` is the public entry point; command
implementations live in `nvx_tools/`.
Source-collection scripts assemble corresponding-source archives for Linux and
Alpine. Performance scripts analyze benchmark outputs, with adjacent
`test_*.py` files covering those utilities.

## Root files

| Path | Responsibility |
| --- | --- |
| `README.md` | Installation, build, run, benchmark, and packaging instructions |
| `pyproject.toml` | Strict Pyright policy plus Ruff lint and format settings |
| `requirements-dev.txt` | Pinned Python tools used by contributors and CI |
| `SOURCE-MANIFEST.json` | Exact Linux, Alpine, NVX, and OpenVMM source identities and output locations |
| `VERSION` | Distribution version consumed by packaging tools |
| `.gitmodules` | OpenVMM repository URL, path, and tracking branch |
| `.gitignore` | Excludes build products, caches, virtual environments, logs, and platform metadata |
| `.dockerignore` | Limits files sent to the Docker build context |
| `.gitattributes` | Repository-specific Git attributes |
| `LICENSE` and `LICENSE-NVX` | Licensing terms for repository content |
| `THIRD_PARTY_NOTICES.md` | Attribution and redistribution notices for dependencies |

## Generated directories

| Path | Contents |
| --- | --- |
| `.cache/` | Downloaded, verified, and patched upstream source trees |
| `build/` | Kernels, initramfs images, package manifests, benchmark output, and collected sources |
| `dist/` | Staged binary and source release archives |
| `openvmm/target/` | Rust build output produced inside the OpenVMM submodule |
| `.ruff_cache/` | Ruff's local lint cache |
| `__pycache__/` | Python bytecode caches that may appear below Python source directories |

Generated paths can be removed and recreated by the build and packaging
commands documented in `README.md`. They should not be treated as source or
edited by hand.

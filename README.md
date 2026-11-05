# NVX: An Ultra-Light Micro-VM Sandbox

NVX is an ultra-light micro-VM sandbox for running untrusted workloads with hardware-enforced
isolation. It is built on top of OpenVMM and runs Linux as a guest.

NVX was jointly developed by the [MSR Systems Research Group][msr-systems] and
[Azure Research - Systems][azure-systems], building on research results from the
[Nanvix](https://github.com/nanvix) system.

[msr-systems]: https://www.microsoft.com/en-us/research/group/systems-research-group-redmond/
[azure-systems]: https://www.microsoft.com/en-us/research/group/azure-research-systems/

This repository includes version pins for Linux, OpenVMM, and the Rust guest
toolchain, along with the patches, guest source files, build tools, and
benchmarks needed to use NVX. Alpine, Ubuntu, and Azure Linux ship the ten
static guest binaries from [`nvx-rs`](nvx-rs/README.md#guest-binaries) at
their common `/sbin/nvx-*` paths.

## Quick Start

Git and Rust are required; install Rust with [rustup](https://rustup.rs/), which also installs
Cargo. The commands below clone NVX, build [`nvx-rs`](doc/usage.md#nvx-rs), the NVX command-line
interface, and run a microVM. The first run downloads the prebuilt NVX package of the host's
platform, from the release that CI built from the cloned revision when available, or from its
nearest released first-parent ancestor otherwise, so the guest and OpenVMM need no build. The
download uses the `curl` and `tar` of the host, which Windows 10 and later include. After you pull
newer commits, `nvx-rs download` installs their release; `run` downloads only into a checkout that
holds no artifacts.

A successful boot prints `NVX-GUEST-BOOT-OK: alpine` and opens a root shell. The package also
includes the Ubuntu and Azure Linux userlands: boot either with the same NVX kernel by adding
`--guest ubuntu` or `--guest azurelinux` to `nvx-rs run`. Exit cleanly from the guest with
`/sbin/nvx-exit 0`. See the [setup](doc/setup.md) and [run](doc/run.md) guides for detailed
prerequisites and runtime options. To run without Rust, extract the package of a
[development release](https://github.com/microsoft/nvx/releases) and run its `bin/nvx-rs`. To
build NVX from source instead, first [initialize](doc/setup.md#initialize) the `openvmm`
submodule, which the clone below omits.

> ℹ️ For direct OpenVMM integration without `nvx-rs`, see
[Run OpenVMM directly](doc/run.md#run-openvmm-directly).

### Linux / KVM

Requires [KVM configured with read/write access to `/dev/kvm`](doc/setup.md#linux--kvm).

```bash
git clone https://github.com/microsoft/nvx.git && cd nvx
cargo build --release --locked --features cli \
  --manifest-path nvx-rs/Cargo.toml --target-dir nvx-rs/target
nvx-rs/target/release/nvx-rs run
```

### Linux / MSHV

Requires [MSHV configured with read/write access to `/dev/mshv`](doc/setup.md#linux--mshv).

```bash
git clone https://github.com/microsoft/nvx.git && cd nvx
cargo build --release --locked --features cli \
  --manifest-path nvx-rs/Cargo.toml --target-dir nvx-rs/target
nvx-rs/target/release/nvx-rs run --hypervisor mshv
```

### Windows / WHP

Requires [Windows Hypervisor Platform enabled](doc/setup.md#windows--whp).

```powershell
git clone https://github.com/microsoft/nvx.git; Set-Location nvx
cargo build --release --locked --features cli `
    --manifest-path nvx-rs/Cargo.toml --target-dir nvx-rs/target
nvx-rs\target\release\nvx-rs.exe run
```

## Documentation

### Usage

- [Setup](doc/setup.md) - Instructions for setting up your environment.
- [Build](doc/build.md) - Instructions for building NVX.
- [Run](doc/run.md) - Instructions for running NVX.
- [Benchmark](doc/benchmarks.md) - Instructions for benchmarking NVX.
- [Command-line reference](doc/usage.md) - Complete `nvx-rs` and `scripts/nvx.py` command and option
  reference.
- [NVX Rust crate](nvx-rs/README.md) - The `nvx` crate that holds the NVX mechanics: the OpenVMM
  control protocol and the launch, policy lowering, and supervision of NVX microVMs.

### Development

- [Design](doc/design/README.md) - Architecture of the NVX microVM and its single-workload guest
  runtime.
- [Ubuntu guest support](doc/guests/ubuntu-guest.md) - How NVX runs Ubuntu userland on its own
  kernel.
- [Azure Linux guest support](doc/guests/azurelinux-guest.md) - How NVX runs Azure Linux userland
  on its own kernel.
- [Project structure](doc/project-structure.md) - Overview of the NVX repository layout.
- [Module diagrams](doc/modules/README.md) - Diagrams of the modules of the `nvx` crate and the
  dependencies between them.
- [Continuous integration](doc/ci.md) - Instructions for running and maintaining NVX CI.
- [Copilot-driven adversarial testing](doc/adversarial-testing.md) - Bounded adaptive stress and
  containment campaigns.
- [Package and source delivery](doc/distribution.md) - Instructions for packaging and distributing
  NVX.
- [Contributing](doc/contribute.md) - Guidelines for contributing to NVX.
- [OpenVMM upstream roadmap](doc/openvmm-upstream-roadmap.md) - Plan for upstreaming the OpenVMM
  fork's microVM commits.

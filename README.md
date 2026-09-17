# NVX: An Ultra-Light Micro-VM Sandbox

NVX is an ultra-light micro-VM sandbox for running untrusted workloads with hardware-enforced
isolation. It is built on top of OpenVMM and runs Linux as a guest.

NVX was jointly developed by the [MSR Systems Research Group][msr-systems] and
[Azure Research - Systems][azure-systems], building on research results from the
[Nanvix](https://github.com/nanvix) system.

[msr-systems]: https://www.microsoft.com/en-us/research/group/systems-research-group-redmond/
[azure-systems]: https://www.microsoft.com/en-us/research/group/azure-research-systems/

This repository includes version pins for Linux and OpenVMM, along with the patches, guest source
files, build tools, and benchmarks needed to use NVX.

## Quick Start

Python 3.10 or newer is required. The commands below download the latest NVX release for the
selected platform, so no local build is required. A successful boot prints
`ALPINE-MICROVM-BOOT-OK` and opens a root shell.

Exit cleanly from the guest with `/sbin/nvx-exit 0`. See the [setup](doc/setup.md) and
[run](doc/run.md) guides for detailed prerequisites and runtime options.

> ℹ️ For direct OpenVMM integration without the `scripts/nvx.py` runtime harness,
see [Run OpenVMM directly](doc/run.md#run-openvmm-directly).

### Linux / KVM

Requires [KVM configured with read/write access to `/dev/kvm`](doc/setup.md#linux--kvm).

```bash
git clone https://github.com/nanvix/nvx.git && cd nvx
python3 scripts/nvx.py download
python3 scripts/nvx.py run
```

### Linux / MSHV

Requires [MSHV configured with read/write access to `/dev/mshv`](doc/setup.md#linux--mshv).

```bash
git clone https://github.com/nanvix/nvx.git && cd nvx
python3 scripts/nvx.py download --hypervisor mshv
python3 scripts/nvx.py run --hypervisor mshv
```

### Windows / WHP

Requires [Windows Hypervisor Platform enabled](doc/setup.md#windows--whp).

```powershell
git clone https://github.com/nanvix/nvx.git; Set-Location nvx
python scripts\nvx.py download
python scripts\nvx.py run
```

## Documentation

### Usage

- [Setup](doc/setup.md) - Instructions for setting up your environment.
- [Build](doc/build.md) - Instructions for building NVX.
- [Run](doc/run.md) - Instructions for running NVX.
- [Benchmark](doc/benchmarks.md) - Instructions for benchmarking NVX.
- [Command-line reference](doc/usage.md) - Complete `scripts/nvx.py` command and option reference.

### Development

- [Design](doc/design.md) - Current microVM architecture and ABI.
- [Project structure](doc/project-structure.md) - Overview of the NVX repository layout.
- [Continuous integration](doc/ci.md) - Instructions for running and maintaining NVX CI.
- [Package and source delivery](doc/distribution.md) - Instructions for packaging and distributing
	NVX.
- [Contributing](doc/contribute.md) - Guidelines for contributing to NVX.

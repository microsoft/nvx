# NVX: An Ultra-Light Micro-VM Sandbox

NVX is an ultra-light micro-VM sandbox for running untrusted workloads with hardware-enforced
isolation. It is built on top of OpenVMM and runs Linux as a guest.

NVX was jointly developed by the MSR Systems Research Group and Azure Research - Systems, building
on research results from the [Nanvix](https://github.com/nanvix) system.

This repository includes version pins for Linux and OpenVMM, along with the patches, guest source
files, build tools, and benchmarks needed to use NVX.

## Quick Start

Python 3.10 or newer is required. The commands below download the latest NVX release for the
selected platform, so no local build is required. A successful boot prints
`ALPINE-MICROVM-BOOT-OK` and opens a root shell.

Exit cleanly from the guest with `/sbin/nvx-exit 0`. See the [setup](doc/setup.md) and
[run](doc/run.md) guides for detailed prerequisites and runtime options.

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

- [Setup](doc/setup.md)
- [Contributing](doc/contribute.md)
- [Continuous integration](doc/ci.md)
- [Build](doc/build.md)
- [Run](doc/run.md)
- [Benchmark](doc/benchmarks.md)
- [Package and source delivery](doc/distribution.md)
- [Project structure](doc/project-structure.md)

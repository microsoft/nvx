# NVX Documentation

NVX is a minimal x86-64 VMM that PVH-boots a Linux kernel and RAM initramfs on Linux/KVM and
Windows/WHP. Start with the host and build guides, then use the feature page for the part of the
machine you are changing.

## Getting started

1. [Set up a Linux/KVM or Windows/WHP host](doc/setup.md).
2. [Build the VMM, kernel, and initramfs](doc/build.md).
3. [Boot and operate the guest](doc/usage.md).

## Guides

| Page | Contents |
| --- | --- |
| [Host Setup](doc/setup.md) | Host requirements, virtualization features, packages, and toolchain checks. |
| [Building NVX](doc/build.md) | Native and Docker artifact builds, Make targets, outputs, and overrides. |
| [Running NVX](doc/usage.md) | Workflow CLI, direct VMM options, console behavior, and platform limits. |
| [Project Organization](doc/project-organization.md) | Boot flow, machine model, memory map, kernel configuration, and source ownership. |
| [Networking](doc/networking.md) | KVM TAP, WHP user-mode NAT, external HCN/AF_XDP, and network restore. |
| [Virt-fs](doc/virtfs.md) | Read-only, ephemeral, and persistent host directory exports. |
| [Snapshot and Restore](doc/snapshots.md) | Capture protocol, state formats, lazy RAM restore, and Python workflows. |
| [Profiling and Flamegraphs](doc/profiling-flamegraph.md) | Guest sampling, host traces, symbol resolution, and flamegraph post-processing. |
| [Benchmark Reference](doc/benchmark.md) | Canonical metrics, sample methodology, commands, and CI regression policy. |
| [Contributing](doc/contributing.md) | Development checks, backend validation, documentation ownership, and pull requests. |

Implementation source and generated CLI help remain authoritative when adding a new option or
backend contract; update the corresponding guide in the same change.

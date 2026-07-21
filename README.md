# microvm

[![CI](https://github.com/nanvix/nvx/actions/workflows/ci.yml/badge.svg)](https://github.com/nanvix/nvx/actions/workflows/ci.yml)

A minimal **x86_64 micro-VM** (single processor by default, optional functional **SMP** on the KVM
backend via `--vcpus`) that boots a Linux (Alpine) kernel through the **PVH boot protocol**,
entirely from a **RAM initramfs**. The raw KVM/WHP machine model has no PCI, ACPI, or block device
by default; its tiny always-on PMIO model is a bidirectional "portb" console plus the wall-clock
registers of an RTC. The optional HCS backend supplies the normal Hyper-V platform devices.

It runs on **two hypervisor backends** from the same codebase:

- **Linux / KVM** — the original backend. Additionally, `--mount` can expose a host directory as a
  virt-fs (read-only, or read-write with `--mount-rw`), `--net` attaches a virtio-net NIC bridged
  to a host TAP for real IPv4 networking, `--snapshot`/`--restore` capture and resume the whole
  VM, and `--vcpus N` runs the guest as functional **SMP** across N vCPUs.
- **Windows / WHP** — a backend on the **[Windows Hypervisor Platform](https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/hypervisor-platform)**
  that PVH-boots the *same* kernel + initramfs with the same portb console, and also supports
  `--snapshot`/`--restore` (capture and resume the whole VM) and `--net` (a virtio-net NIC backed
  by a built-in user-mode NAT — no TAP driver or admin rights needed). `--net-config` can instead
  bind AF_XDP to an externally managed interface, including an externally provisioned HCN host
  vNIC.
  See [Running on Windows (WHP backend)](#running-on-windows-whp-backend).

The Linux/KVM backend is a standalone extraction and reworking of the **KVM (Linux) backend of the
[Nanvix Micro-VM (`uservm`)](https://github.com/nanvix/nanvix/tree/dev/src/uservm)**,
stripped of the Nanvix-specific paravirtual ABI (magic control registers, credits,
snapshotting, control plane, profiler, host filesystem, ...) and given the x86_64 Linux boot
support required to run a real Linux kernel. The Windows/WHP backend is the WHP-native counterpart
of that boot path.

This is a **complete, reproducible package** with three components, each a directory in this
repo:

- **the VMM** — `src/` (Rust);
- **the modified kernel** — `kernel/` (a minimal `config` + the `patches/` that add the
  `0xE9` earlycon), built from vanilla LTS source by `python3 scripts/nvx.py build-kernel`;
- **Alpine** — `alpine/` (the RAM `init`), packed onto the official Alpine mini root
  filesystem by `python3 scripts/nvx.py build-initramfs`.

Nothing is prebuilt: `make world` fetches the kernel and Alpine sources and builds all three.

## What it does

```
$ make world                         # build the VMM, the modified kernel, and the initramfs
$ make run                           # boot Alpine to a shell over the portb console
...
[    1.745978] Run /init as init process

==========================================
 ALPINE-MICROVM-BOOT-OK: 3.24.1
 uname: Linux (none) 6.18.38 ... x86_64 Linux
==========================================

/ # cat /etc/alpine-release
3.24.1
/ # id
uid=0(root) gid=0(root)
/ # reboot -f
```

## Design

- **Single core by default, optional KVM SMP.** One vCPU is driven by the main `KVM_RUN` loop. On
  KVM, `--vcpus N` brings up N-1 additional *application processors* — each a vCPU on its own
  host thread — that the guest starts through the normal INIT–SIPI–SIPI path (serviced by the
  in-kernel LAPIC) after finding a VMM-written **Intel MP table**. No PCI or ACPI is involved, and
  no kernel reconfiguration is needed (the config already enables `SMP`/`X86_MPPARSE`).
- **PVH boot** (`XEN_ELFNOTE_PHYS32_ENTRY` + `hvm_start_info`). The VMM loads an uncompressed
  `vmlinux`, locates the PVH 32-bit entry note, and enters the guest in 32-bit protected mode
  with `%ebx` pointing at the boot info. The kernel itself switches to long mode. This avoids
  the real-mode/bzImage setup path entirely.
- **RAM-only root filesystem.** The initramfs is loaded into guest memory and passed as a PVH
  module; the kernel unpacks it and runs `/init`. There is no block device unless the optional
  `--mount` virt-fs is used, and no virtio unless `--net` attaches the virtio-net NIC.
- **Minimal device model.** The primary device is a bidirectional **"portb" console**
  backing the kernel's `hvc0`: output is one `outb` per byte to I/O port `0xE9`, input is polled
  from `0xEA`
  (status) and `0xE9` (data) — no interrupt line. A minimal MC146818 at `0x70`/`0x71` supplies the
  early x86 wall-clock read but no interrupts; without it Linux waits about one second for a
  permanently set update-in-progress bit. Every other port floats. The in-kernel KVM irqchip
  (PIC + IOAPIC) and PIT provide interrupts and the timer; `kvm-clock` provides runtime time.
  `--net` optionally adds one **virtio-net** NIC on a
  **virtio-mmio** window (the only MMIO device and the only interrupt-driven one), pointed at
  through the kernel command line just like the virt-fs.

### Source layout

| Path                                          | Responsibility                                                                                                                                                                                                                                                                                 |
| --------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `src/main.rs`                                 | CLI and entry point; dispatches to the KVM (Linux), WHP (Windows), or experimental HCS (Windows) backend                                                                                                                                                                                       |
| `src/console.rs`                              | *(shared)* Console sink: buffered/quiet output, byte count, cold-start timing                                                                                                                                                                                                                  |
| `src/layout.rs`                               | *(shared)* Guest-physical memory map constants                                                                                                                                                                                                                                                 |
| `src/devices/portb.rs`                        | *(shared)* portb console device: TX `outb` `0xE9`, RX poll `0xEA`/`0xE9`, host-input queue                                                                                                                                                                                                     |
| `src/devices/rtc.rs`                          | *(Linux/KVM)* Minimal MC146818 RTC/CMOS that prevents the early x86 wall-clock timeout                                                                                                                                                                                                         |
| `src/devices/mod.rs`                          | *(shared)* PMIO device bus (portb console `0xE9`/`0xEA`, KVM RTC `0x70`/`0x71`, `0x604` shutdown, `0x605` snapshot)                                                                                                                                                                             |
| `src/boot/mod.rs`                             | *(shared)* PVH boot module + `GuestWrite` trait that decouples the loader from each backend's memory                                                                                                                                                                                           |
| `src/boot/mptable.rs`                         | *(Linux/KVM)* Intel MP table used by an ACPI-less guest to enumerate vCPUs                                                                                                                                                                                                                     |
| `src/boot/pvh.rs`                             | *(shared)* `vmlinux` ELF loader, PVH note parsing, `hvm_start_info` layout                                                                                                                                                                                                                     |
| `src/boot/params.rs`                          | *(shared)* PVH boot-parameter structures                                                                                                                                                                                                                                                       |
| `src/vmm.rs`                                  | *(Linux/KVM)* VM setup, the vCPU run loop, console input thread, TTY handling                                                                                                                                                                                                                  |
| `src/memory.rs`                               | *(Linux/KVM)* Guest RAM as KVM user-memory regions (MMIO-gap aware; snapshot dump / COW restore)                                                                                                                                                                                               |
| `src/vcpu.rs`                                 | *(Linux/KVM)* vCPU creation, CPUID, and the PVH entry register/segment state                                                                                                                                                                                                                   |
| `src/irq.rs`                                  | *(Linux/KVM)* In-kernel irqchip + PIT                                                                                                                                                                                                                                                          |
| `src/snapshot.rs`                             | *(Linux/KVM)* Full VM snapshot / restore (vCPU + devices + VM state)                                                                                                                                                                                                                           |
| `src/virtfs.rs`                               | *(Linux/KVM)* virt-fs: pack a `--mount` host directory into a SquashFS (ro) or ext4 (rw) image, map it into guest memory, and point the guest at it                                                                                                                                            |
| `src/net.rs`                                  | *(Linux/KVM)* virt-net: a virtio-net NIC on a virtio-mmio transport backed by a host TAP (`--net`); parses the endpoint, brings the TAP up, and runs the RX/TX virtqueues                                                                                                                      |
| `src/whp/mod.rs`                              | *(Windows/WHP)* Partition setup, the vCPU run loop, CPUID synth, I/O dispatch, timer/input threads, LAPIC EOI                                                                                                                                                                                  |
| `src/whp/memory.rs`                           | *(Windows/WHP)* Guest RAM via `VirtualAlloc` + `WHvMapGpaRange` (MMIO-gap aware); restore maps `mem.bin` copy-on-write for lazy, RAM-size-independent resume                                                                                                                                   |
| `src/whp/vcpu.rs`                             | *(Windows/WHP)* PVH entry register/segment state via `WHvSetVirtualProcessorRegisters`                                                                                                                                                                                                         |
| `src/whp/pic.rs`                              | *(Windows/WHP)* Minimal i8259 PIC so the kernel wires up IRQ0 (the host-driven PIT tick)                                                                                                                                                                                                       |
| `src/whp/pit.rs`                              | *(Windows/WHP)* Minimal hang-safe i8254 channel-2 PIT counter for guest TSC calibration                                                                                                                                                                                                        |
| `src/whp/rtc.rs`                              | *(Windows/WHP)* Minimal MC146818 RTC/CMOS so the boot-time wall-clock read does not spin                                                                                                                                                                                                       |
| `src/whp/net.rs`                              | *(Windows/WHP)* virtio-net NIC on virtio-mmio (`--net`); MMIO via the WHP instruction emulator, IRQ on a master-PIC line                                                                                                                                                                       |
| `src/whp/slirp.rs`                            | *(Windows/WHP)* user-mode NAT backing the NIC: ARP + ICMP for the gateway, TCP/UDP/DNS out through host sockets                                                                                                                                                                                |
| `src/whp/emulator.rs`                         | *(Windows/WHP)* thin wrapper over WHP's instruction emulator for servicing virtio-mmio memory-access exits                                                                                                                                                                                     |
| `src/whp/snapshot.rs`                         | *(Windows/WHP)* Full VM snapshot / restore (vCPU regs + XSAVE + APIC + emulated devices)                                                                                                                                                                                                       |
| `src/whp/virtfs.rs`                           | *(Windows/WHP)* virt-fs (`--mount`): a FAT image built in pure Rust (`fatfs`), mapped above RAM via `WHvMapGpaRange`; the guest mounts it as `vfat`                                                                                                                                            |
| `src/whp/xdp.rs`                              | *(Windows/WHP)* bounded AF_XDP frame backend, queue discovery, control-pipe handshake, and XDP program lifecycle                                                                                                                                                                             |
| `src/l2bridge.rs`                             | *(shared)* strict versioned `--net-config` schema, guest bootstrap, and snapshot-safe external NIC identity                                                                                                                                                                                  |
| `src/hcs/`                                    | *(Windows/HCS)* HCS compute/snapshot backend and borrowed HCN endpoint validation/attachment                                                                                                                                                                                               |
| `src/windows_terminal.rs`                     | *(Windows)* shared console mode guard for WHP and HCS                                                                                                                                                                                                                                         |
| `docker/Dockerfile`                           | Builds the PVH `vmlinux` + Alpine `initramfs.cpio.gz` in a Linux container (for use from Windows)                                                                                                                                                                                              |
| `kernel/config-microvm`                       | Minimal Linux kernel configuration                                                                                                                                                                                                                                                             |
| `kernel/hvc_xe9.c`                            | The portb `hvc0` console driver (installed into the tree by the Python kernel builder)                                                                                                                                                                                                         |
| `kernel/patches/`                             | Kernel source modifications (the `0xE9` earlycon)                                                                                                                                                                                                                                              |
| `alpine/init`                                 | PID 1 for the RAM initramfs                                                                                                                                                                                                                                                                    |
| `alpine/init.python`                          | PID 1 for the Python initramfs (runs `pyapp=<file>`, default `hello.py`)                                                                                                                                                                                                                       |
| `alpine/hello.py`, `alpine/repl.py`           | Python snapshot apps: pandas/numpy benchmark and interactive REPL                                                                                                                                                                                                                              |
| `alpine/net-hello.py`, `alpine/net-pandas.py` | Networked Python snapshot apps: a bare interpreter and a warmed numpy/pandas app that prove the NIC works after restore                                                                                                                                                                        |
| `scripts/nvx.py`                              | Cross-platform entry point for build, run, smoke-test, snapshot, and benchmark workflows                                                                                                                                                                                                      |
| `scripts/nvx_tools/`                          | Shared Python workflow logic plus explicit Linux/KVM and Windows/WHP host backends                                                                                                                                                                                                             |
| `scripts/performance.py`                      | Performance log collection, history persistence, and regression gating                                                                                                                                                                                                                        |

## The kernel ("modified Alpine kernel")

`kernel/config-microvm` is a minimal x86_64 configuration built from the vanilla LTS source that
Alpine's `linux-lts` tracks. `python3 scripts/nvx.py build-kernel` downloads the matching source,
applies the modification in `kernel/patches/`, drops in this config, and builds an uncompressed
`vmlinux` carrying the PVH entry note.

### Configuration rationale

The guest sees almost no hardware — one or more CPUs, RAM, the in-kernel interrupt controller, and
a single portb console — so the configuration follows one rule: **build in exactly what a
device-less PVH/KVM guest needs to boot, and compile out everything that would probe for hardware
that is not there.**
Probing absent hardware is at best wasted boot time and at worst a multi-second hang (see
`PM_TRACE_RTC` below).

| Config                                                                                                         | Why                                                                                                                                                                                                                                                                                                                                                                                                 |
| -------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CONFIG_PVH=y`                                                                                                 | Enter through the 32-bit PVH entry note, so the VMM loads an uncompressed `vmlinux` and skips the real-mode/bzImage setup and self-decompression path entirely.                                                                                                                                                                                                                                     |
| `CONFIG_HYPERVISOR_GUEST=y`, `CONFIG_PARAVIRT=y`, `CONFIG_KVM_GUEST=y`, `CONFIG_PARAVIRT_CLOCK=y`              | Run as a KVM guest and take time from `kvm-clock`. The VMM also passes KVM's virtual TSC rate as `tsc_early_khz`, avoiding timing-sensitive PIT calibration before `kvm-clock` initializes.                                                                                                                                                                                                         |
| `CONFIG_ACPI=y`, `CONFIG_PCI=y` (and no EFI)                                                                   | HCS kernel-direct guests need the Hyper-V platform discovery paths. The raw KVM/WHP backends still provide no ACPI tables or PCI devices, so these paths remain dormant there.                                                                                                                                                                                                                     |
| `CONFIG_SERIAL_8250=y`, `CONFIG_SERIAL_8250_CONSOLE=y`, `CONFIG_HVC_XE9=y`                                      | Raw KVM/WHP use the portb `hvc0` driver (`kernel/hvc_xe9.c`); HCS uses its emulated COM ports through the built-in 8250 console. `CONFIG_SERIAL_EARLYCON` remains available for `earlycon=xe9`.                                                                                                                                                                                                    |
| `CONFIG_HYPERV=y`, `CONFIG_HYPERV_NET=y`                                                                       | HCS boots on the Hyper-V synthetic platform and attaches its HCN endpoint through NetVSC/VMBus. The drivers are built in because the initramfs has no module tree.                                                                                                                                                                                                                                 |
| `# CONFIG_FB`, `# CONFIG_HID`, `# CONFIG_SOUND`, `# CONFIG_ATA`, `# CONFIG_SCSI`, `# CONFIG_RTC_CLASS`, no USB | None of these devices exist, so their drivers and boot-time probes are removed.                                                                                                                                                                                                                                                                                                                     |
| `CONFIG_BLK_DEV_INITRD=y`, `CONFIG_DEVTMPFS=y`, `CONFIG_TMPFS=y`                                               | The whole userland is the initramfs unpacked into RAM; there is no block device or virtio, hence no storage stack.                                                                                                                                                                                                                                                                                  |
| `CONFIG_NET=y`, `CONFIG_INET=y`, `CONFIG_VIRTIO_MMIO=y` (+ `_CMDLINE_DEVICES`), `CONFIG_VIRTIO_NET=y`          | The minimal networking needed for `--net`: IPv4 over one virtio-net NIC on a virtio-mmio window declared via `virtio_mmio.device=` on the kernel command line. IPv6, wireless, NFS and the rest of the stack stay off. Boots without `--net` pay only a few ms for the dormant stack.                                                                                                               |
| `# CONFIG_MODULES`                                                                                             | Everything required is built in; a single static `vmlinux` needs no module loader.                                                                                                                                                                                                                                                                                                                  |
| `CONFIG_HZ_100=y`, `CONFIG_NO_HZ_IDLE=y`                                                                       | A low 100 Hz tick with tickless idle: fewer timer interrupts, faster boot.                                                                                                                                                                                                                                                                                                                          |
| `CONFIG_SMP=y`, `CONFIG_X86_MPPARSE=y`, `CONFIG_X86_X2APIC=y`, `CONFIG_NR_CPUS=64`                             | Let the guest enumerate and start more than one vCPU from the VMM's Intel MP table (`--vcpus`). The stock config already includes these options, so SMP needs no kernel change.                                                                                                                                                                                                                     |
| `# CONFIG_SUSPEND`, `# CONFIG_HIBERNATION`, `# CONFIG_X86_MCE`, `# CONFIG_NUMA`                                | Power management, machine-check, and NUMA are meaningless for this single-package, device-less VM.                                                                                                                                                                                                                                                                                                  |
| `# CONFIG_FTRACE`, `# CONFIG_KPROBES`, `# CONFIG_PROFILING`, `# CONFIG_DEBUG_KERNEL`                           | Tracing / debug / profiling infrastructure is compiled out to shrink the image and speed boot.                                                                                                                                                                                                                                                                                                      |
| no `PM_TRACE_RTC` (gated off by no suspend)                                                                    | **The load-bearing one.** With suspend/hibernate off there is no `PM_SLEEP`, so the `PM_TRACE` debug feature and its `PM_TRACE_RTC` are never built — which matters: `PM_TRACE_RTC`'s `early_resume_init` initcall reads the RTC via `mc146818_get_time()`, and with no RTC (ports `0x70`/`0x71` float) that read spins to a ~1 s timeout **twice**, most of the old ~1.6 s cold-start (see below). |

The only **source** change is `kernel/patches/0001-microvm-xe9-earlycon.patch`, which adds an
`earlycon=xe9` driver that emits each kernel-log byte with a single `outb` to I/O port `0xE9` —
see [the "portb" strategy](#the-portb-strategy-mirroring-nanvix).

## The initramfs (RAM filesystem)

`python3 scripts/nvx.py build-initramfs` unpacks the official Alpine mini root filesystem, installs
`alpine/init` as PID 1 (it mounts `proc`/`sys`/`dev`/`tmpfs` and execs a shell), and packs it
as a gzipped `newc` cpio archive. The whole userland lives in RAM.

## Building and running

Requirements: Python 3.10+, a Linux host with `/dev/kvm` accessible to your user, a stable Rust toolchain
(edition 2024), and — for building the kernel — `flex`, `bison`, `libelf-dev`, `bc`, `cpio`,
`patch`.

Quick start:

```
make world          # build all three: the VMM + the modified kernel + the Alpine initramfs
make run            # boot Alpine to an interactive shell over the portb console
```

### Make targets

| Target             | What it does                                                                                        |
| ------------------ | --------------------------------------------------------------------------------------------------- |
| `all` *(default)*  | Alias for `release`.                                                                                |
| `world`            | Build all three components: `release` + `kernel` + `initramfs`.                                     |
| `release`          | Build the VMM in release mode → `target/release/microvm`.                                           |
| `build`            | Build the VMM in debug mode.                                                                        |
| `test`             | Run the unit tests (`cargo test --release`; no KVM required).                                       |
| `kernel`           | Download + patch + build the PVH `vmlinux` → `$(KERNEL_IMG)`.                                      |
| `initramfs`        | Build the Alpine RAM rootfs → `$(BUILD_DIR)/initramfs.cpio.gz`.                                    |
| `python-initramfs` | Build a rootfs with CPython + pandas/numpy → `$(PY_INITRD)`.                                       |
| `run`, `boot`      | Boot Alpine to an interactive shell over the portb console.                                       |
| `selftest`         | Run the protected-mode self-test through the real PVH entry path and exit.                          |
| `boot-test`        | End-to-end: boot and assert the guest reaches userspace.                                            |
| `measure`          | Cold-start measurements.                                                                             |
| `bench-virtfs`     | virt-fs throughput + persistent `--mount-image` round-trip.                                         |
| `snapshot-demo`    | pandas/numpy snapshot/restore benchmark.                                                             |
| `snapshot-boot`    | Resume an interactive Python interpreter from a snapshot.                                          |
| `bench-hcs-snapshot-shell` | HCS-native shell snapshot restore across memory sizes (Windows, privileged).              |
| `bench-hcs-snapshot-py` | HCS-native warmed Python snapshot restore (Windows, privileged).                              |
| `bench-hcs-net-snapshot-py` | HCN-backed HCS restore with real HTTP verification (Windows, privileged).               |
| `clean`            | `cargo clean`.                                                                                      |

`make kernel` / `initramfs` / `python-initramfs` always re-run their Python workflow. `snapshot-demo`
and `snapshot-boot` instead depend on the artifacts `$(KERNEL_IMG)` and `$(PY_INITRD)` via file
rules that build **only when the artifact is missing** (and, for the Python initramfs, when its
`alpine/` sources change), so they work from a clean tree without rebuilding what is already there.

Both make and `scripts/nvx.py` read these overridable variables from the environment:

| Variable                               | Default                                 | Used by                                            |
| -------------------------------------- | --------------------------------------- | -------------------------------------------------- |
| `CARGO`                                | `cargo`                                 | the `release` / `build` / `test` / `clean` targets |
| `BUILD_DIR`                            | `$(HOME)/build`                         | where artifacts are written                        |
| `KERNEL_IMG`                           | `$(BUILD_DIR)/vmlinux`                  | kernel artifact path                               |
| `INITRD_IMG`                           | `$(BUILD_DIR)/initramfs.cpio.gz`        | base initramfs artifact path                       |
| `PY_INITRD`                            | `$(BUILD_DIR)/initramfs-python.cpio.gz` | Python initramfs artifact path                     |
| `KVER`                                 | `6.18.38`                               | `build-kernel` (kernel version)                    |
| `AVER` / `ABRANCH`                     | `3.24.1` / `v3.24`                      | the initramfs builders (Alpine version)            |
| `MEM`, `N`, `SNAP`, `KERNEL`, `INITRD` | (see `scripts/nvx.py --help`)           | run, snapshot, and benchmark commands              |
| `NVX_ARTIFACT_DIR`                     | `$HOME/build` (Linux)                   | default Linux snapshot and artifact directory      |

All helper workflows use one cross-platform command surface. Run
`python3 scripts/nvx.py <command> --help` on Linux or `python scripts\nvx.py <command> --help`
on Windows. Shared process, snapshot, parsing, and reporting logic lives in `scripts/nvx_tools/`;
the `LinuxBackend` and `WindowsBackend` classes contain host paths, KVM/WHP capabilities, TAP
cleanup, sparse-file accounting, and Linux-only tool requirements.


Run directly:

```
./target/release/microvm \
    --kernel  $HOME/build/vmlinux \
    --initrd  $HOME/build/initramfs.cpio.gz \
    --mem     512 \
    --cmdline "earlycon=xe9 console=hvc0 reboot=t panic=-1"
```

### CLI

| Flag                    | Default                                       | Meaning                                                                                          |
| ----------------------- | --------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| `--backend <whp|hcs>`   | `whp` (Windows only)                          | Select the Windows backend during the HCS migration                                              |
| `--kernel <path>`       | (required)                                    | Uncompressed `vmlinux` (PVH)                                                                     |
| `--initrd <path>`       | (none)                                        | RAM initramfs image                                                                              |
| `--cmdline <str>`       | backend-specific                              | Kernel command line; an explicit value is always used unchanged                                 |
| `--mem <MiB>`           | `512`                                         | Guest RAM                                                                                        |
| `--vcpus <N>`           | `1`                                           | KVM vCPUs (1-254); snapshots capture and restore the full SMP state                              |
| `--quiet`               |                                               | Fully silent: discard guest console **and** suppress all VMM logging                             |
| `--log-level <lvl>`     | `info` (`off` if `--quiet`)                   | `off`/`error`/`warn`/`info`/`debug`/`trace`; `off` suppresses all logging (`RUST_LOG` overrides) |
| `--exit-on-boot`        |                                               | Stop and report cold-start/restore time when the boot marker appears                             |
| `--boot-marker <s>`     | `ALPINE-MICROVM-BOOT-OK`                      | Console substring that marks boot completion                                                     |
| `--timing-marker <label=text>` | *(repeatable)*                         | Time another console substring from the same first-guest-instruction clock                       |
| `--defer-stdin-until-boot` |                                            | Delay redirected cold-boot input until the boot marker; terminals/restores remain immediate      |
| `--snapshot <dir>`      |                                               | Take a backend-specific snapshot into `<dir>` when the guest requests one, then exit             |
| `--restore <dir>`       |                                               | Restore and resume from a snapshot `<dir>` instead of booting                                    |
| `--net <IP/PREFIX>`     |                                               | Attach standalone networking (KVM TAP, WHP user-mode NAT, or HCS HCN endpoint)                   |
| `--net-config <json>`   |                                               | Attach WHP AF_XDP networking from a strict external or HCN-provisioned manifest                  |
| `--mount <dir>`         |                                               | Export a host directory to the guest as a virt-fs (read-only SquashFS by default)                |
| `--mount-target <path>` | `/mnt/host`                                   | Guest mount point for `--mount`                                                                  |
| `--mount-rw`            |                                               | Mount the `--mount` export read-write (ext4); ephemeral without `--mount-image`                  |
| `--mount-image <file>`  |                                               | Persist a read-write `--mount` to this host image file (implies `--mount-rw`)                    |
| `--mount-size <MiB>`    |                                               | Size of the writable ext4 image (headroom for guest writes; rw only)                             |
| `--selftest`            |                                               | Run the WHP protected-mode self-test or HCS capability preflight and exit                        |

To **suppress all logging**, pass `--log-level off` (mutes the `[… INFO microvm::…]` lines but
still renders the guest console), or `--quiet` for a fully silent run (no guest console and no
logging). The cold-start line printed under `--exit-on-boot` goes to stderr independently of the
log level, so measurements keep working even when logging is off.

## Running on Windows (WHP backend)

On Windows the same VMM PVH-boots the same `vmlinux` + `initramfs.cpio.gz` through the **Windows
Hypervisor Platform** instead of KVM. The kernel and initramfs still have to be *built* on Linux
(they need GCC and a Linux tree), so that step runs in **Docker**; everything else runs natively.

### Requirements

- Windows 10/11 (x64) with the **Windows Hypervisor Platform** feature enabled and virtualization
  enabled in firmware:

  ```powershell
  Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform   # then reboot
  ```
- A stable **Rust** toolchain for the `x86_64-pc-windows-msvc` target (edition 2024).
- **Docker Desktop** (Linux engine) to build the kernel + initramfs.

### Experimental HCS Phase 0

The non-default HCS backend can boot a kernel-direct VM with one vCPU and COM1 console I/O. It
requires Windows 11 Pro/Enterprise or Windows Server 2022 or newer with the full **Hyper-V** feature,
plus an elevated process or membership in **Hyper-V Administrators**. First run the non-destructive
service/schema preflight:

```powershell
.\target\release\microvm.exe --backend hcs --selftest
```

Then boot with the Hyper-V-capable kernel built by the artifact workflow:

```powershell
.\target\release\microvm.exe `
  --backend hcs `
  --kernel build\vmlinux `
  --initrd build\initramfs.cpio.gz `
  --exit-on-boot
```

The HCS default command line uses `ttyS0`; a user-provided `--cmdline` is preserved. During snapshot
capture nvx appends only its reserved `nvx_snapshot_transport=hcs-com2` control token. This Phase 0
path supports console, quiet mode, boot/timing markers, redirected-stdin deferral, clean termination,
resource-handle cleanup, experimental HCS-native snapshot/restore, and a NetVSC adapter backed by an
externally managed HCN endpoint. Mounts, `--net-config`, `--net-tap`, and more than one vCPU are
rejected before HCS creates a compute system. WHP remains the Windows default. NVX opens, queries,
and closes the endpoint handle; it never creates, host-attaches, or deletes persistent HCN objects.

Create an endpoint from an elevated PowerShell session, then pass its descriptor with every HCS
network launch. `--net` is optional when the descriptor is present; when supplied, NVX verifies that
both sources describe the same address:

```powershell
$endpoint = Join-Path $env:TEMP 'nvx-hcs-endpoint.json'
.\scripts\setup-hcn-endpoint.ps1 `
  -OutputPath $endpoint `
  -GuestAddress 192.168.241.2 `
  -Gateway 192.168.241.1

try {
  .\target\release\microvm.exe `
    --backend hcs `
    --kernel build\vmlinux `
    --initrd build\initramfs.cpio.gz `
    --net 192.168.241.2/24 `
    --hcn-endpoint-config $endpoint `
    --exit-on-boot
} finally {
  .\scripts\cleanup-hcn-endpoint.ps1 -DescriptorPath $endpoint
}
```

HCS snapshots are deliberately incompatible with WHP snapshots. The guest helper uses a versioned
COM2 request, HCS writes opaque state to `runtime.vmrs`, and nvx commits an `NVXHCSS1`
`manifest.json` only after save succeeds. Restore requires the exact Windows host build and verifies
the captured kernel and initrd hashes before creating a compute system. A snapshot directory must
not already exist. For example, `shellsnap` requests a snapshot before the normal boot marker:

```powershell
.\target\release\microvm.exe `
  --backend hcs `
  --kernel build\vmlinux `
  --initrd build\initramfs.cpio.gz `
  --cmdline "console=ttyS0,115200 8250_core.skip_txen_test=1 panic=-1 shellsnap" `
  --snapshot hcs-shellsnap

.\target\release\microvm.exe `
  --backend hcs `
  --restore hcs-shellsnap `
  --exit-on-boot
```

Manifest v3 records the external HCN network, endpoint, adapter, MAC, addressing, and DNS identity. Capture
and restore borrow the same externally owned endpoint identity. Keep that endpoint alive across the
capture/restore sequence and pass the descriptor on restore; NVX verifies it against the snapshot
manifest and leaves it intact after the compute system closes.

Run the HCS-native benchmarks from an elevated terminal or as a member of **Hyper-V
Administrators**. Native HCS runs the same 23 collected scenarios as KVM and WHP: five cold-start
modes, five Plan9 file-sharing measurements, two warmed-Python measurements, shell cold/restore at
64/128/256/512 MiB, and three network snapshot measurements. HCS logs retain process-wall and VMRS
details in addition to the shared p50 rows:

```powershell
python scripts\nvx.py bench-hcs-coldstart --runs 10 --mem 512
python scripts\nvx.py bench-hcs-virtfs --runs 5 --mem 512
python scripts\nvx.py bench-hcs-snapshot-shell --runs 10 --memories "64 128 256 512"
python scripts\nvx.py bench-hcs-snapshot-py --runs 8 --mem 512
$endpoint = Join-Path $env:TEMP 'nvx-hcs-benchmark-endpoint.json'
.\scripts\setup-hcn-endpoint.ps1 -OutputPath $endpoint `
  -GuestAddress 192.168.241.2 -Gateway 192.168.241.1
try {
  python scripts\nvx.py bench-hcs-net-snapshot-py --runs 8 --mem 512 `
    --net 192.168.241.2/24 --hcn-endpoint-config $endpoint
} finally {
  .\scripts\cleanup-hcn-endpoint.ps1 -DescriptorPath $endpoint
}
```

To collect a machine-readable baseline without mixing it with WHP history:

```powershell
New-Item -ItemType Directory -Force build\performance-hcs | Out-Null
python scripts\nvx.py bench-hcs-coldstart --runs 10 *>&1 |
  Tee-Object build\performance-hcs\hcs-cold-start.log
python scripts\nvx.py bench-hcs-virtfs --runs 5 *>&1 |
  Tee-Object build\performance-hcs\hcs-virtfs.log
python scripts\nvx.py bench-hcs-snapshot-shell --runs 10 *>&1 |
  Tee-Object build\performance-hcs\hcs-shell-snapshot.log
python scripts\nvx.py bench-hcs-snapshot-py --runs 8 *>&1 |
  Tee-Object build\performance-hcs\hcs-python-snapshot.log
$endpoint = Join-Path $env:TEMP 'nvx-hcs-benchmark-endpoint.json'
.\scripts\setup-hcn-endpoint.ps1 -OutputPath $endpoint `
  -GuestAddress 192.168.241.2 -Gateway 192.168.241.1
try {
  python scripts\nvx.py bench-hcs-net-snapshot-py --runs 8 `
    --net 192.168.241.2/24 --hcn-endpoint-config $endpoint *>&1 |
    Tee-Object build\performance-hcs\hcs-network-snapshot.log
} finally {
  .\scripts\cleanup-hcn-endpoint.ps1 -DescriptorPath $endpoint
}
python scripts\performance.py collect `
  --platform windows-hcs `
  --commit (git rev-parse HEAD) `
  --input-dir build\performance-hcs `
  --output-dir build\performance-results `
  --require-network `
  --require-shared-suite
```

This produces `windows-hcs.csv` with the same 23 metric names as `linux-kvm.csv` and
`windows-whp.csv`. One-off capture and extra process-wall details remain in the human-readable logs.
HCS benchmarks are not part of hosted CI; they require a separately labeled, privileged Hyper-V
runner.

The CI workflow defines opt-in **Windows / HCS** and **Windows / HCN + AF_XDP** hardware jobs. Both
call `setup-hcn-endpoint.ps1` before testing and `cleanup-hcn-endpoint.ps1` in an `always()` step.
The AF_XDP job requests `-AttachToHost`, runs the same 20 non-network WHP scenarios, then uses
`scripts/benchmark-hcn-afxdp-snapshot.ps1` for five external-L2Bridge cold boots, one capture, and
five restores. Every restored sample must rebind all selected AF_XDP queues and ping the HCN gateway
before it contributes to the shared network p50 rows. These jobs are excluded from pull requests
because they create privileged host networking objects.

Runner requirements:

- a dedicated repository runner has labels `windows`, `x64`, and `hcn-afxdp`; do not assign this
  label to a general pull-request runner;
- the dedicated runner service account is an Administrator or Local System;
- Hyper-V, Windows Hypervisor Platform, and Host Network Service are enabled and running;
- signed [XDP-for-Windows Runtime x64 v1.3.0](https://www.nuget.org/packages/Microsoft.XDP-for-Windows.Runtime.x64/1.3.0)
  is installed (`C:\Windows\System32\xdpapi.dll` exists and reports product version `1.3.0`);
- the chosen `/24` does not overlap another HCN or host network.

Enable automatic merged-main runs with repository variable `NVX_HCN_AFXDP_CI=true`. Optional
variables `NVX_HCN_AFXDP_GUEST_ADDRESS` and `NVX_HCN_AFXDP_GATEWAY` select a non-overlapping pair
(defaults: `192.168.240.2` and `192.168.240.1`). Native HCS runs use `NVX_HCS_CI=true` and optional
`NVX_HCS_GUEST_ADDRESS`/`NVX_HCS_GATEWAY` variables (defaults: `192.168.241.2` and
`192.168.241.1`). A manual dispatch can enable either privileged lane independently.

### 1. Build the Linux artifacts (Docker)

`docker/Dockerfile` compiles the PVH `vmlinux` and packs the Alpine `initramfs.cpio.gz` inside a
Linux container, then exports just those two files to the host via `docker build --output`:

```powershell
python scripts\nvx.py build-linux-artifacts    # -> build\vmlinux, build\initramfs.cpio.gz
```

Under the hood this is:

```powershell
docker build -f docker/Dockerfile --target artifacts --output type=local,dest=build .
```

The kernel and initramfs stages build in parallel; the kernel compile is the long pole (several
minutes the first time; downloads and layers are cached afterwards). Override the versions with
`-Kver`/`-Aver`/`-Abranch` (or the `KVER`/`AVER`/`ABRANCH` build args).

The Python snapshot demos and `bench-net-snapshot-py` command additionally need a Python initramfs
(CPython + numpy/pandas). Build it the same way (downloads the packages over the network):

```powershell
python scripts\nvx.py build-python-initramfs   # -> build\initramfs-python.cpio.gz
```

### 2. Build the VMM

```powershell
cargo build --release          # -> target\release\microvm.exe
cargo test --release           # shared + WHP unit tests (no hypervisor required)
.\target\release\microvm.exe --selftest    # validate the WHP protected-mode setup end-to-end
python scripts\nvx.py test-boot              # boot Linux and assert that userspace is reached
```

The shared boot smoke test has a 90-second timeout and succeeds only
after seeing `ALPINE-MICROVM-BOOT-OK` in guest output.

### 3. Boot

```powershell
python scripts\nvx.py run                        # boots build\vmlinux + build\initramfs.cpio.gz
# or, explicitly:
.\target\release\microvm.exe `
    --kernel build\vmlinux --initrd build\initramfs.cpio.gz `
    --mem 512 --cmdline "earlycon=xe9 console=hvc0 reboot=t panic=-1"
```

`--quiet`, `--exit-on-boot`, `--boot-marker`, `--mem`, `--cmdline`, `--log-level`, `--selftest`,
`--snapshot`/`--restore`, `--net` and the virt-fs flags (`--mount`, `--mount-rw`, `--mount-image`,
`--mount-size`, `--mount-target`) all work as on Linux. The TAP-attach option (`--net-tap`, which is
Linux-specific) and multi-vCPU (`--vcpus` greater than 1) are KVM-only and are rejected with a
clear message on Windows. The one
implementation difference is the virt-fs image format: Windows has no `mksquashfs`/`mke2fs`, so the
WHP backend builds a **FAT** image in pure Rust (the guest mounts it as `vfat`) where KVM uses
SquashFS/ext4 — see [Virt-fs](#virt-fs-mount) below.

### Networking (`--net`, user-mode NAT)

Windows has no unprivileged layer-2 TAP, so instead of bridging to a host interface the WHP backend
gives the guest the **same virtio-net NIC** as KVM but backs it with a built-in **user-mode NAT** —
no driver and no administrator rights. The guest gets a static address on a small subnet (from the
kernel command line, exactly as on Linux: `--net 10.0.0.2/24` ⇒ guest `10.0.0.2`, gateway
`10.0.0.1`), and the VMM plays the gateway:

- answers **ARP** for the gateway and **ICMP echo** (so `ping <gateway>` works);
- NATs guest **TCP** out through host `TcpStream`s (speaking TCP to the guest itself);
- NATs guest **UDP** (including DNS) out through host `UdpSocket`s;
- connections to the gateway address are served by the host loopback, so the guest can reach host
  services (e.g. `wget http://10.0.0.1:8080/`).

Two things differ from KVM under the hood because WHP has no in-hypervisor device model: MMIO to the
virtio-mmio window is serviced through WHP's **instruction emulator**
([`WHvEmulatorTryMmioEmulation`](src/whp/emulator.rs)), and the NIC interrupt is delivered as a
**master-PIC line (IRQ5)** injected as a local-APIC vector — the same mechanism as the PIT timer,
which avoids the slave-PIC cascade. See [`src/whp/net.rs`](src/whp/net.rs) and
[`src/whp/slirp.rs`](src/whp/slirp.rs). The NIC also **survives snapshot/restore** (its transport
state and endpoint are captured, and the NAT is rebuilt on resume).

```powershell
.\target\release\microvm.exe --kernel build\vmlinux --initrd build\initramfs.cpio.gz `
    --mem 256 --net 10.0.0.2/24 --cmdline "earlycon=xe9 console=hvc0 reboot=t panic=-1"
# in the guest:  ping 10.0.0.1   /   wget -qO- http://10.0.0.1:8080/
```

### AF_XDP networking (`--net-config`)

WHP can replace user-mode NAT with the bounded AF_XDP L2Bridge backend. Install the signed official
XDP-for-Windows 1.3.0 runtime from NuGet in an elevated PowerShell session:

```powershell
$package = Join-Path $env:TEMP 'xdp-runtime-1.3.0.zip'
$runtime = Join-Path $env:TEMP 'xdp-runtime-1.3.0'
Invoke-WebRequest `
  https://www.nuget.org/api/v2/package/Microsoft.XDP-for-Windows.Runtime.x64/1.3.0 `
  -OutFile $package
Expand-Archive $package $runtime -Force
& "$runtime\runtime\native\xdp-setup.ps1" -Install xdp
```

The `aka.ms/xdp-v1.msi` redirect may install the older 1.1 runtime and is not suitable for this
backend. Run NVX with HCN privileges and provide a version-2 manifest plus an Agent-hosted control
pipe. The Agent reads `DataPlaneReady`, including the selected queues and final interface LUID, then
writes `{"type":"StartVm"}` followed by a newline. NVX never falls back to SLIRP when this path
fails.

Two attachment modes use the same guest/device contract:

- `afxdp-l2bridge` binds an existing interface; `interfaceIndex` and `interfaceLuid` must both be
  nonzero and identify the same interface.
- `hcn-afxdp-l2bridge` binds an externally provisioned HCN host vNIC. The manifest must provide its
  nonzero `interfaceIndex`/`interfaceLuid` and `gatewayMac`; NVX uses the gateway MAC for its local
  ARP proxy and leaves the interface, endpoint, and network intact on shutdown.

Use `queueSelection.mode = "auto"` for HCN host vNICs. RSS can place return traffic on any receive
queue; binding only queue 0 makes connectivity depend on the flow hash. Explicit queue selection is
intended for externally managed interfaces whose steering is controlled by the Agent.

Example HCN-provisioned manifest:

```json
{
  "version": 2,
  "attachment": {
    "backend": "hcn-afxdp-l2bridge",
    "interfaceIndex": 42,
    "interfaceLuid": 1689399632855040,
    "gatewayMac": "00-15-5D-52-CF-2B",
    "queueSelection": { "mode": "auto" }
  },
  "device": { "macAddress": "00-15-5D-01-02-03", "mtu": 1500 },
  "guestBootstrap": {
    "ipv4": {
      "address": "192.168.240.2",
      "prefixLength": 24,
      "gateway": "192.168.240.1"
    },
    "routes": [
      { "destination": "0.0.0.0/0", "nextHop": "192.168.240.1" }
    ],
    "dns": { "servers": ["1.1.1.1"], "search": [] }
  },
  "runtime": { "controlPipe": "\\\\.\\pipe\\nvx-network-agent" }
}
```

The HCN mode requires a usable `/1` through `/30` guest address and the first usable subnet
address as its gateway. The manifest MAC, MTU, routes, and DNS form the snapshot identity; restore
requires the external owner to make the same interface identity available and rejects guest-visible
identity changes.

### Snapshot / restore

The WHP backend implements the same guest-initiated snapshot/restore as KVM (a write to control
port `0x605` captures the VM, then it exits; `--restore <dir>` resumes it). A snapshot directory
holds `mem.bin` (guest RAM, written sparsely) and `state.bin` (the framed processor / APIC / device
state). The captured state is the vCPU register file (GPRs, segments, tables, control/debug and the
model-specific registers a booted Linux keeps), the **full FPU/SSE/AVX/CET register file as one
XSAVE area** (via `WHvGet/SetVirtualProcessorXsaveState`, plus `IA32_XSS` so the guest's supervisor
`XRSTORS` does not fault), the emulated **local-APIC** state (via the interrupt-controller-state
API), and the emulated **8259 PIC / i8253 PIT / MC146818 RTC** and the portb console's pending
input queue. See [`src/whp/snapshot.rs`](src/whp/snapshot.rs).

`mem.bin` is written **sparsely** (the RAM file is marked sparse with `FSCTL_SET_SPARSE`, since —
unlike Unix — seeking over zero runs on Windows otherwise leaves physically-allocated zeros), so a
snapshot occupies only the guest's touched footprint on disk. **Restore is lazy**: rather than
copying the image up front, it maps `mem.bin` copy-on-write (`CreateFileMapping(PAGE_WRITECOPY)` +
`MapViewOfFile(FILE_MAP_COPY)`) and registers the views with `WHvMapGpaRange`, so guest pages fault
in on first access and writes go to private copies — the file is never modified (restores are
replayable). This mirrors the KVM backend's `MAP_PRIVATE` restore and makes resume **sub-100 ms and
independent of the configured RAM size** (e.g. ~95 ms at 256 MiB and ~100 ms at 512 MiB, versus
~200 ms / ~350 ms for an eager copy).

```powershell
# take a snapshot when the guest requests one, then exit:
.\target\release\microvm.exe --kernel build\vmlinux --initrd build\initramfs.cpio.gz `
    --mem 256 --snapshot snap\
# resume from the snapshot (no kernel needed):
.\target\release\microvm.exe --restore snap\ --mem 256
```

<a name="virt-fs-mount"></a>
### Virt-fs (`--mount`)

`--mount <dir>` exports a host directory to the guest as a mountable device — the same mechanism as
KVM: a filesystem image is placed in a guest-physical window above reported RAM, surfaced through
`phram` (MTD) + `mtdblock` as `/dev/mtdblock0`, and mounted by PID 1 at `--mount-target`
(default `/mnt/host`). The location, type and mode travel on the kernel command line
(`phram.phram=virtfs,<base>,<len> virtfs_dir=<path> virtfs_fs=<type> virtfs_mode=<ro|rw>`).

Because Windows has no `mksquashfs`/`mke2fs`, the WHP backend builds the image as a **FAT
filesystem in pure Rust** (the [`fatfs`](https://crates.io/crates/fatfs) crate) and the guest mounts
it as `vfat` (`CONFIG_VFAT_FS`), where the KVM backend uses SquashFS (read-only) / ext4
(read-write). Three modes, matching KVM:

- **read-only** (default): the directory is packed into a FAT image mapped from private host memory
  (`VirtualAlloc`), mounted `-o ro`.
- **read-write ephemeral** (`--mount-rw`): a writable FAT image held in private memory; guest writes
  work but are discarded when the VM stops.
- **read-write persistent** (`--mount-image <file>`): the FAT image is mapped **shared** from the
  host file (`CreateFileMapping(PAGE_READWRITE)` + `MapViewOfFile(FILE_MAP_WRITE)`), so guest writes
  are flushed back and **persist across runs** (the file is created from the directory the first
  time and reused afterwards). `--mount-size <MiB>` sizes the writable image. See
  [`src/whp/virtfs.rs`](src/whp/virtfs.rs).

```powershell
# read-only export:
.\target\release\microvm.exe --kernel build\vmlinux --initrd build\initramfs.cpio.gz `
    --mem 512 --mount C:\some\dir
# persistent read-write image (guest writes to /mnt/host survive across runs):
.\target\release\microvm.exe --kernel build\vmlinux --initrd build\initramfs.cpio.gz `
    --mem 512 --mount C:\some\dir --mount-image host.img --mount-size 192
```

### Benchmarks

Most Python benchmark commands run on both KVM and WHP; the HCS-native snapshot commands are
explicitly named. Shared methodology parses the VMM's `cold-start:` / `restore:` lines, while HCS
also reports full process wall time so service and compute-system lifecycle overhead remains visible:

| Command                    | Measures                                                                                         |
| -------------------------- | ------------------------------------------------------------------------------------------------ |
| `measure-coldstart`     | cold-start (guest start → boot marker) across several console configurations                       |
| `bench-net-snapshot`    | networked cold boot vs. snapshot restore to a live-NIC shell                                       |
| `snapshot-demo`         | pandas/numpy cold boot vs. restore of a warmed interpreter                                          |
| `snapshot-boot`         | resume an interactive Python REPL straight from a snapshot                                          |
| `bench-net-snapshot-py` | networked Python (bare + numpy/pandas) cold vs. restore with a real HTTP round-trip                 |
| `bench-virtfs`          | virt-fs guest I/O throughput + a persistent `--mount-image` round-trip                             |
| `bench-snapshot-shell`  | cold shell boot vs. shell-ready snapshot restore across memory sizes                              |
| `bench-hcs-coldstart`      | native HCS cold-start modes matching the shared five cold-start metrics                         |
| `bench-hcs-virtfs`         | native HCS writable Plan9 throughput and persistence verification                               |
| `bench-hcs-snapshot-shell` | HCS cold shell boot vs. native VMRS restore, including guest and process-wall latency          |
| `bench-hcs-snapshot-py`    | HCS cold Python/pandas startup vs. restore of the warmed interpreter                            |
| `bench-hcs-net-snapshot-py` | HCN-backed HCS restore with a real guest-to-host HTTP request after every restore              |
| `scripts/benchmark-hcn-afxdp-snapshot.ps1` | external HCN/AF_XDP cold boot vs. snapshot restore with a restored gateway probe |

CI records each merged commit's benchmark p50 values in `data/performance/`. On pull requests,
`scripts/performance.py` compares each metric with the arithmetic mean of its latest 10 p50 values
on the PR's base branch. A regression greater than 40% fails the `Performance regression gate`;
lower latency and higher throughput are treated as improvements. Metrics without history are
reported as warmups until a baseline exists. The workflow needs `contents: write` permission (and,
if `main` is protected, a rule allowing `github-actions[bot]`) to persist the baseline commit.
Every backend that runs benchmarks also publishes its collected p50 values as a Markdown table in
its GitHub Actions job summary, including the metric unit and preferred direction.
HCS and HCN/AF_XDP results join that baseline when their opt-in main-branch hardware jobs run;
KVM and WHP results remain mandatory. Privileged jobs do not run on untrusted pull requests, so
their new metrics warm up on main rather than participating in the pull-request gate.
It compiles and unit-tests the shared Python tooling on both hosts, uses the shared boot test, and runs
networked-Python plus interactive snapshot-boot smoke tests on both KVM and WHP.

The Python workflows need the Python initramfs (`build\initramfs-python.cpio.gz`); build it on
Windows with `python scripts\nvx.py build-python-initramfs` (a Docker stage that downloads CPython +
numpy/pandas). `bench-net-snapshot-py` also uses its host Python process for a helper server (the guest
GETs the gateway, which the NAT forwards to `127.0.0.1`).


### How it boots without KVM's device model

WHP virtualizes the CPU and memory and emulates the local APIC, but — unlike KVM — it has no
in-hypervisor PIT/PIC/IOAPIC, no `kvm-clock`, and no legacy chipset. A PVH guest with no ACPI/MP
tables runs its APIC in "virtual wire, no configuration" mode, where Linux does **not** use the
local-APIC timer and instead drives its clock from the **i8253 PIT on IRQ0**. So, to give the
guest a working timer while keeping the device model tiny, the WHP backend (`src/whp/`) adds just
enough emulation:

- **In-hypervisor LAPIC** (`WHvX64LocalApicEmulationModeXApic`): interrupt delivery and `HLT`
  handling.
- **A minimal 8259 PIC** (`src/whp/pic.rs`) so the kernel detects the legacy interrupt controller
  (its probe writes and reads back the mask register) and wires up IRQ0 — otherwise it falls back
  to a "NULL legacy PIC" and `request_irq(0)` fails, leaving the guest with no clock-event device.
- **A host timer thread** that produces the PIT tick: roughly every `CONFIG_HZ` period it flags a
  tick and cancels the vCPU run; the run loop then raises IRQ0 through the PIC and injects it as a
  fixed local-APIC vector with `WHvRequestInterrupt` (which also wakes a parked/idle vCPU). Because
  a virtual-wire guest issues only the *8259* end-of-interrupt and never a *local-APIC* one, the
  VMM completes the LAPIC acknowledge itself — clearing the in-service vector through the
  interrupt-controller-state API, since WHP's XApic registers are not otherwise writable.
- **A CPUID intercept** that hands the guest a **known TSC frequency** (measured from the host TSC
  at start-up, via CPUID leaves `0x15`/`0x16`) so it uses the TSC as a reliable clocksource, and
  forces the "always-running APIC timer" bit (leaf `6`).
- **A minimal RTC/CMOS** (`src/whp/rtc.rs`) so the kernel's boot-time wall-clock read returns a
  sane time instead of spinning forever on a floating `0x70`/`0x71` (the "update-in-progress" bit
  would otherwise read as permanently set), plus a hang-safe calibration **PIT counter**
  (`src/whp/pit.rs`, channel 2 + port `0x61`).
- The shared **portb console** (`0xE9`/`0xEA`) and the `0x604` shutdown port, decoded straight from
  WHP I/O-port exits — the guest uses single-byte `in`/`out`, so no instruction emulator is needed.

The same `kernel/config-microvm` serves both backends: `CONFIG_KVM_GUEST` simply goes dormant when
the KVM CPUID signature is absent, and the kernel falls back to the TSC clocksource + PIT/IRQ0
timer described above. This assumes a host CPU with an invariant TSC (universal on modern x86-64),
which WHP exposes to the guest.

## Cold-start and the portb console

The VMM measures **cold-start** — the time from the first guest instruction to a
boot-completion marker in the console stream — and reports it via `--exit-on-boot`:

```
$ ./target/release/microvm --kernel ~/build/vmlinux --initrd ~/build/initramfs.cpio.gz \
      --exit-on-boot --cmdline "earlycon=xe9 console=hvc0 reboot=t panic=-1"
...
[INFO microvm::console] cold-start: 1895.7 ms to userspace (13305 console bytes emitted)
```

### The "portb" strategy (mirroring Nanvix)

The Nanvix Micro-VM sends guest console output one byte at a time to a dedicated I/O port
(`0xE9`), so each character is a single `outb` — one VM exit — instead of the 8250 UART's
*read line-status + write data* (two exits, plus a poll loop). This VMM makes that its **only**
console: a bidirectional **"portb" console** backed by the in-kernel `hvc0` driver
(`kernel/hvc_xe9.c`, installed by `scripts/nvx.py build-kernel`). Output is one `outb` to `0xE9`;
input is polled from `0xEA` (status) and `0xE9` (data). The `earlycon=xe9` driver
(`kernel/patches/`) provides the earliest boot logs on the same port before `hvc0` takes over:

```
--cmdline "earlycon=xe9 console=hvc0 ..."     # early logs via 0xE9, then the interactive hvc0
```

There is no 16550 UART: it was measured ~65 ms slower to userspace (2 exits/byte vs 1) and
removed. The VMM also has a `0x604` control port (a write requests VM shutdown, Nanvix
`DEFAULT_VMM_PORT`) and a `0x605` port (a write requests a snapshot).

### Measured cold-start (`make measure`)

Median of 10 runs, 512 MiB, 1 vCPU, host with nested KVM. The portb console is the only
transport; rendering it ("loud") vs discarding it (`--quiet`) is the main knob:

| portb console (`0xE9` → `hvc0`) | loud (rendered) | quiet (`--quiet`, discarded) |
| ------------------------------- | --------------: | ---------------------------: |
| to kernel→userspace handoff     |      **265 ms** |                   **194 ms** |

*End-to-end* (to the interactive shell):

| configuration                                |                cold-start |
| -------------------------------------------- | ------------------------: |
| loud, full kernel logs                       |                   ~339 ms |
| silent (`quiet loglevel=0`)                  |                   ~133 ms |
| **fastest** (silent, 128 MiB, tuned cmdline) | **~118 ms** (min ~111 ms) |

### How cold-start went from ~1.6 s to ~0.11 s (~13x)

Profiling with `initcall_debug` found a single dominant cost, then a long tail:

1. **`CONFIG_PM_TRACE_RTC` — the big one (~1.43 s!).** Its `early_resume_init` initcall
   reads the RTC via `mc146818_get_time(..., 1000)`. This device-less VM has **no RTC**
   (ports `0x70/0x71` float), so the read spun to its ~1 s timeout (twice). Disabling the
   `PM_TRACE` debug feature removed the initcall entirely — a 10x win by itself
   (1562 ms -> 158 ms).
2. **Strip every subsystem a device-less VM never uses:** `CONFIG_NET` (and NFS/SUNRPC/
   IPv6), wireless/`CFG80211`, `AUDIT`, `PPS`, `SUSPEND`/`HIBERNATION`, `X86_MCE`, `NUMA`,
   `MICROCODE`, `KPROBES`, `PROFILING`, RTC-CMOS. `vmlinux` shrank 34 MB -> 19 MB and the
   initcall tail dropped (158 ms -> ~135 ms). (A minimal IPv4 + virtio-net stack was later
   built back in for the optional [`--net`](#networking-virt-net) NIC; the rest stay stripped.)
3. **Smaller guest RAM.** The kernel initialises a `struct page` for every page of RAM at
   boot, so 512 MiB costs ~25 ms more than 128 MiB. 128 MiB is plenty for a RAM boot.
4. **Tuned command line** (trusted single-tenant VM):
   `clocksource=kvm-clock tsc=reliable no_timer_check random.trust_cpu=on
   rcupdate.rcu_expedited=1 nokaslr mitigations=off cryptomgr.notests quiet loglevel=0`.

Things that turned out **not** to matter here (already handled by `CONFIG_KVM_GUEST` +
kvm-clock + `RANDOM_TRUST_CPU`): BogoMIPS/`lpj` calibration (already skipped), TSC
calibration, and crng entropy stall (crng init is instant via RDRAND). The initramfs
userland is also negligible (~2 ms to unpack; the ~110 ms is essentially all kernel init).

Reproduce all of the above with `make measure`.

## Sharing a host directory (virt-fs)

`--mount <dir>` exports a host directory to the guest, mounted inside the guest at
`--mount-target` (default `/mnt/host`). By default the export is **read-only**:

```console
$ ./target/release/microvm --kernel ~/build/vmlinux --initrd ~/build/initramfs.cpio.gz \
      --mount ./shared --mount-target /mnt/host
...
virtfs: mounted host directory at /mnt/host (squashfs,ro)
# (inside the guest) ls /mnt/host
```

or `MOUNT=./shared make run`.

This mirrors how the **Nanvix Micro-VM ("uservm")** exposes its RAMFS — the VMM places a
filesystem image in guest memory and hands the guest its base/size, and the guest mounts it —
adapted to a stock Linux guest:

1. The VMM packs the directory into a filesystem image (read-only **SquashFS** via `mksquashfs`,
   or a read-write **ext4** via `mke2fs -d` — see below).
2. It maps the image into a dedicated KVM memory slot placed **above the RAM reported to the
   guest** (just past the 4 GiB MMIO gap and any high RAM), so the kernel never allocates over it.
3. It appends `phram.phram=virtfs,<base>,<len> virtfs_dir=<target> virtfs_fs=<type>
   virtfs_mode=<ro|rw>` to the kernel command line — the "registers" that tell the guest where the
   image is, where to mount it, and how.
4. In the guest, the built-in **`phram`** MTD driver maps that physical window and **`mtdblock`**
   turns it into `/dev/mtdblock0`; PID 1 (`alpine/init`) mounts it at `<target>`.

The guest side needs no custom driver — only stock kernel options
(`CONFIG_MTD`/`MTD_BLOCK`/`MTD_PHRAM`, `CONFIG_SQUASHFS`, `CONFIG_EXT4_FS`, all enabled in
`kernel/config-microvm`).

### Read-write exports

Pass `--mount-rw` to mount the export **read-write** (an ext4 image built from the directory).
Two flavors:

- **Ephemeral** (`--mount-rw` alone): the writable image lives in guest memory, so the guest can
  create and edit files, but the changes are discarded when the VM stops.
- **Persistent** (`--mount-image <file>`, which implies `--mount-rw`): the ext4 image is a host
  file, mapped `MAP_SHARED`, so guest writes are flushed back to it. The file is created from
  `--mount` the first time and **reused** afterwards, so edits survive across runs and are visible
  on the host (e.g. `debugfs -R 'cat /file' <file>`, or a loop mount).

```console
# writable, changes persist to ./disk.img across runs:
$ ./target/release/microvm --kernel ~/build/vmlinux --initrd ~/build/initramfs.cpio.gz \
      --mount ./shared --mount-image ./disk.img
...
virtfs: mounted host directory at /mnt/host (ext4,rw)
```

or `MOUNT=./shared MOUNT_IMAGE=./disk.img make run` (or `MOUNT=./shared MOUNT_RW=1 make run` for
the ephemeral variant). Read-write exports need `mke2fs` (the `e2fsprogs` package) on the host; a
guest `sync` before shutdown ensures writes reach a persistent image. `--mount-size <MiB>` grows
the ext4 image when the guest needs room to write beyond the seeded contents.

### Benchmark (`make bench-virtfs`)

`python3 scripts/nvx.py bench-virtfs` measures guest-observed sequential throughput to the mount (busybox
`dd`, `conv=fsync` writes) for a read-write export that is *ephemeral* (`--mount-rw`) versus
*persistent* (`--mount-image`), and runs a **persistence round-trip** on a persistent image:
create + write a payload, then reuse the image across cold boots and verify (by checksum) that the
guest's writes survived — on the host too (confirmed with `debugfs`). Median of N runs (default
64 MiB payload, 512 MiB guest):

```
== sequential throughput (guest dd, conv=fsync writes) ==
  rw ephemeral (in-memory)    write    336.8 MB/s   read  3100.0 MB/s
  rw persistent (file-backed) write    290.9 MB/s   read  3800.0 MB/s

== persistence round-trip (rw --mount-image) ==
  create image + write 64 MiB       : 1967 ms
  reuse image + verify (cold each)  : 1892 ms  (min 993, max 1916, n=5)
  payload survived across runs      : 5/5 runs (cksum 3975907619)
  host sees /data.bin in image      : 67108864 bytes (debugfs)
```

The persistent image is slower to write than the in-memory one (its dirty pages are backed by a
host file), but reads come from the mapped window at memory speed. Guest `fsync` reaches the
`phram` window (host RAM); the VMM flushes that window to the backing file when the VM stops.
Tune with `N=`, `PAYLOAD_MB=`, `MEM=`, and `IMG_MB=`.

## Networking (virt-net)

`--net <ip>/<prefix>` attaches a NIC and gives the guest a host network endpoint. The value is the
**guest** address and subnet; the **host** side of the point-to-point link takes the first address
of that subnet and becomes the guest's gateway. So `--net 10.0.0.2/24` puts the guest on
`10.0.0.2` and the host on `10.0.0.1`:

```console
$ sudo ./target/release/microvm --kernel ~/build/vmlinux --initrd ~/build/initramfs.cpio.gz \
      --net 10.0.0.2/24
...
virtnet: configured eth0 as 10.0.0.2/255.255.255.0 (gw 10.0.0.1)
/ # ping -c1 10.0.0.1          # guest -> host
64 bytes from 10.0.0.1: seq=0 ttl=64 time=0.20 ms
/ # wget -qO- http://10.0.0.1:8000/   # reach a server on the host
```

and, from the host, `ping 10.0.0.2` reaches the guest. Or `NET=10.0.0.2/24 make run`.

This mirrors, in spirit, how the **Nanvix Micro-VM ("uservm")** exposes the host network to the
guest with `-allow-host-networking` — but where uservm proxies the guest's socket calls to the
host stack, here the guest runs a **stock Linux TCP/IP stack over a real (virtual) NIC**, so no
guest-side custom driver or paravirtual ABI is needed:

1. The VMM models one **virtio-net** device on a **virtio-mmio** (version 2 / VIRTIO 1.0) transport
   in a fixed guest-physical window (`0xd000_0000`, in the MMIO gap that is never reported as RAM),
   with a single legacy IRQ line (10, delivered through the in-kernel 8259 PIC via a KVM `irqfd`).
2. It backs the NIC with a host **TAP** interface: it creates the TAP (persistent, owned by the
   invoking user), gives the host side the gateway address, brings it up, and tears it down when
   the VM stops. Guest transmit frames are written to the TAP; a receive thread `poll`s the TAP and
   feeds inbound frames into the guest's receive queue, raising the NIC's interrupt.
3. It appends `virtio_mmio.device=0x1000@0xd0000000:10 virtnet_ip=<ip> virtnet_mask=<mask>
   virtnet_gw=<host>` to the kernel command line — the "registers" that tell the guest where the
   NIC is and how to address the link.
4. In the guest, the built-in **`virtio_net`** driver binds the device as `eth0`; PID 1
   (`alpine/init`) reads the `virtnet_*` tokens and configures the interface (address via
   `ifconfig`, default route via `ip route`).

The guest side needs only stock kernel options (`CONFIG_NET`, `CONFIG_INET`, `CONFIG_VIRTIO_MMIO`
with `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`, and `CONFIG_VIRTIO_NET`, all enabled in
`kernel/config-microvm`).

### Privileges and scope

Creating and configuring the host TAP needs `CAP_NET_ADMIN`, so run the VMM as **root** or allow
**passwordless `sudo ip`** (the VMM shells out to `ip tuntap`/`ip addr`/`ip link`, escalating with
`sudo -n` when it is not already privileged). The link is **host ⇄ guest** only: the guest can
reach the host (and any service bound on the host, including the `10.0.0.1` gateway address), and
the host can reach the guest. Routing the guest onward to the internet is out of scope — add your
own NAT (`iptables -t nat -A POSTROUTING -s 10.0.0.0/24 -j MASQUERADE` plus
`net.ipv4.ip_forward=1`) if you want it. Networking is IPv4-only.

### Networking across snapshot / restore

The NIC **survives `--snapshot`/`--restore`**. The snapshot captures the device's transport state
(each virtqueue's ready flag, size, ring addresses, and consumer indices) alongside the guest RAM
that holds the rings, so on restore the device resumes in lockstep with the driver instead of
desynchronising. The host side is rebuilt automatically: the restore reads the guest endpoint from
the snapshot, recreates the TAP with the **same MAC** (derived from the gateway IP, so the guest's
ARP entry for the gateway stays valid) and address, re-registers the `irqfd`, and re-arms the
receive thread — no `--net` needs to be given on the restore command line.

`python3 scripts/nvx.py bench-net-snapshot` (`make bench-net-snapshot`) measures this: cold-booting to a
**working-network** shell versus **restoring** one from a snapshot of a warmed, network-configured
guest (the guest re-pings the host over the recreated TAP before the timing marker, so a restore
that reaches the marker has proven the link works). Resuming a networked guest reaches a live link
in tens of milliseconds versus a full cold boot.

#### Reusing a pre-created TAP (`--net-tap`)

By default each run creates and destroys its own TAP, which costs four privileged `ip` calls — the
bulk of a restore's wall-clock (the guest itself resumes in ~15–20 ms). To skip that per-run setup,
pre-create one persistent, user-owned TAP and point runs at it with `--net-tap <name>`; the VMM
then binds to it with a single `TUNSETIFF` (no `sudo`, no `ip`) and leaves it in place on exit:

```console
# one-off: create the TAP with the deterministic gateway MAC/address (needs privileges once)
$ sudo ip tuntap add dev llxnet0 mode tap user "$USER"
$ sudo ip link set dev llxnet0 address 52:54:00:00:00:01   # 52:54:00 + gateway 10.0.0.1
$ sudo ip addr add 10.0.0.1/24 dev llxnet0
$ sudo ip link set dev llxnet0 up

# capture, then restore repeatedly with no per-run TAP setup:
$ ./target/release/microvm --net 10.0.0.2/24 --net-tap llxnet0 --snapshot ./snap ...
$ ./target/release/microvm --restore ./snap --net-tap llxnet0        # ~4x lower wall-clock
```

Because the pre-created MAC matches the one a VMM-managed TAP would derive, snapshots taken either
way restore over either kind of TAP. Measured restore wall-clock (256 MiB, shared host): **~368 ms**
with a per-run TAP versus **~89 ms** attaching to a pre-created one.

#### Networked Python workloads (`make bench-net-snapshot-py`)

`python3 scripts/nvx.py bench-net-snapshot-py` runs the same idea with real workloads on the Python initramfs:
`alpine/net-hello.py` (a bare CPython interpreter) and `alpine/net-pandas.py` (a warmed
numpy/pandas interpreter). Each app configures nothing itself — PID 1 (`alpine/init.python`) brings
the NIC up — then does a real **HTTP GET to the host** (a helper server the script runs) to prove
the link, warms its hot paths, and requests a snapshot; on restore it re-checks the link and prints
its marker, so a restore that reaches the marker has resumed with a **live NIC**. Restoring the
warmed, network-connected interpreter is far faster than cold-booting it (guest resume → marker,
median of 8, 512 MiB, shared host):

| to a working-network Python app   | cold boot |     restore |
| --------------------------------- | --------: | ----------: |
| bare interpreter (`net-hello.py`) |  ~1670 ms |  **~50 ms** |
| numpy + pandas (`net-pandas.py`)  |  ~2580 ms | **~104 ms** |

(These `restore` figures are the guest resume time; add the one-time host-TAP setup for the
wall-clock, or use `--net-tap` to make that negligible.)

## Snapshot / restore and booting from a snapshot

The VMM can capture the **entire VM state** to a directory and later resume from it, skipping
the kernel boot and application startup entirely. A snapshot is **VMM-initiated on a guest
request**: the guest asks for one by writing a byte to I/O port `0x605` (via `/dev/port`), the
VMM intercepts that `outb`, and — because the request happens at a userspace **syscall
boundary** — captures a clean, restorable state:

- guest RAM (`mem.bin`);
- the vCPU register file — GPRs, segments, FPU, XCRs, LAPIC, MP state, pending events, debug
  registers, and the MSRs a booted Linux keeps (syscall entry points, FS/GS bases, SYSENTER,
  PAT, TSC/TSC-deadline, and the KVM paravirtual-clock MSRs);
- the in-kernel interrupt controller (PIC + IOAPIC), the PIT, and the KVM clock;
- the portb console device (its pending host-input queue).

`state.bin` holds all of that; `mem.bin` is the raw RAM, **written sparsely** — runs of zero
pages become file holes, so the image shrinks to the guest's actual footprint (e.g. ~280 MiB
of a 512 MiB guest) rather than the full RAM size. On **restore**, the RAM image is mapped
copy-on-write (`MAP_PRIVATE`), so pages fault in lazily and there is no upfront copy — restore is
dominated by re-applying the vCPU/VM state, not by RAM size.

```
# take a snapshot when the guest requests one, then exit:
microvm --kernel vmlinux --initrd initramfs.cpio.gz --mem 256 --snapshot snap/

# boot from the snapshot (no kernel needed):
microvm --restore snap/ --mem 256
```

The details above (in-kernel PIC/IOAPIC, the KVM clock, `MAP_PRIVATE` COW restore) are the
Linux/KVM implementation. The **Windows/WHP backend supports the same `--snapshot`/`--restore`
flow** with a WHP-native capture — see
[Snapshot / restore](#snapshot--restore) under the WHP section.

### A pandas program from a snapshot (`make snapshot-demo`)

`make python-initramfs` builds an initramfs with a full CPython interpreter and the pandas/numpy
stack, plus `alpine/hello.py`:

```python
import pandas as pd, numpy as np
df = pd.DataFrame({'x': np.arange(5), 'y': np.arange(5) ** 2})
print(df.sum().to_dict())
```

The app imports pandas/numpy and runs the DataFrame computation once to **warm** every code
path, then requests a snapshot at that fully warmed point. On a cold boot it pays for kernel
boot, the Python startup, **and** the whole pandas/numpy import (plus pandas' first-use lazy
init) before it reaches the result; from a snapshot it resumes the warmed interpreter and re-runs
the computation immediately.

Median of 8 runs, 512 MiB, 1 vCPU:

| path                                                      | to `{'x': 10, 'y': 30}` |
| --------------------------------------------------------- | ----------------------: |
| cold boot (kernel + Python startup + pandas/numpy import) |                  ~2.6 s |
| **restore from snapshot**                                 | **~63 ms** (min ~55 ms) |

That is a **~40x** speedup: the snapshot skips the kernel boot and, crucially, the entire
pandas/numpy import, leaving only the (already-warm) DataFrame work on the restored path.
Reproduce with `make python-initramfs && make snapshot-demo`.

Two things keep this fast and compact: the app **warms the computation before snapshotting**, so
the restored path hits warm code/data instead of paying pandas' lazy first-use init; and `mem.bin`
is **written sparsely**, so the snapshot on disk is ~280 MiB rather than the full 512 MiB.

To instead drop into an **interactive Python interpreter resumed from a snapshot**, use
`make snapshot-boot`. It boots `alpine/repl.py`, which warms a full CPython interpreter, requests
a snapshot at the warmed point, and — on restore — resumes straight into a live `>>>` prompt on
the console, skipping the kernel boot and the entire Python startup. The snapshot is
captured once on first use (a one-off cold boot) and reused afterwards, so every later run drops
you at the prompt in milliseconds:

```
$ make snapshot-boot
>> resuming interactive Python interpreter from snapshot ~/build/pyrepl (Ctrl-D or exit() to quit)
Python 3.14.5 on microvm (resumed from snapshot).
Ready: collections, functools, itertools, json, math, os, re, sys. Ctrl-D or exit() to quit.
>>> math.factorial(5)
120
>>> exit()
```

The interpreter and the modules listed in the banner are already imported in the snapshot, so
they are usable by name at the prompt the instant it resumes. `make snapshot-demo` and
`make snapshot-boot` share the one Python initramfs; the initramfs `init` picks the app from a
`pyapp=<file>` token on the kernel command line (default `hello.py`).

## Notes

- **Exiting the VM:** `reboot -f` (a triple-fault reset with `reboot=t`) is caught by the VMM,
  which exits cleanly. `poweroff` has no effect because the kernel has no ACPI/power management
  and simply halts.
- When standard input is a terminal it is put in raw mode for an authentic console;
  when input is piped (scripts, CI) it is left alone.

## Provenance & license

MIT licensed. Derived from the KVM backend of the Nanvix Micro-VM (`uservm`), © The
Maintainers of Nanvix (MIT). See `LICENSE`.

# microvm

A minimal, single-core **x86_64 KVM micro-VM** that boots a Linux (Alpine) kernel through
the **PVH boot protocol**, entirely from a **RAM initramfs** — no PCI, no ACPI, and no block
device by default. The only always-on emulated device is a bidirectional "portb" console (backing
the kernel's `hvc0`); `--mount` can additionally expose a host directory as a virt-fs (read-only,
or read-write with `--mount-rw`).

It is a standalone extraction and reworking of the **KVM (Linux) backend of the
[Nanvix Micro-VM (`uservm`)](https://github.com/nanvix/nanvix/tree/dev/src/uservm)**,
stripped of the Nanvix-specific paravirtual ABI (magic control registers, credits,
snapshotting, control plane, profiler, host filesystem, ...) and given the x86_64 Linux boot
support required to run a real Linux kernel.

This is a **complete, reproducible package** with three components, each a directory in this
repo:

- **the VMM** — `src/` (Rust);
- **the modified kernel** — `kernel/` (a minimal `config` + the `patches/` that add the
  `0xE9` earlycon), built from vanilla LTS source that `scripts/build-kernel.sh` downloads;
- **Alpine** — `alpine/` (the RAM `init`), packed onto the official Alpine mini root
  filesystem that `scripts/build-initramfs.sh` downloads.

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

- **Single core, single thread.** One vCPU driven by a synchronous `KVM_RUN` loop.
- **PVH boot** (`XEN_ELFNOTE_PHYS32_ENTRY` + `hvm_start_info`). The VMM loads an uncompressed
  `vmlinux`, locates the PVH 32-bit entry note, and enters the guest in 32-bit protected mode
  with `%ebx` pointing at the boot info. The kernel itself switches to long mode. This avoids
  the real-mode/bzImage setup path entirely.
- **RAM-only root filesystem.** The initramfs is loaded into guest memory and passed as a PVH
  module; the kernel unpacks it and runs `/init`. There is no virtio, and no block device unless
  the optional `--mount` virt-fs is used.
- **Minimal device model.** The only always-present device is a bidirectional **"portb" console**
  backing the kernel's `hvc0`: output is one `outb` per byte to I/O port `0xE9`, input is polled
  from `0xEA`
  (status) and `0xE9` (data) — no interrupt line. Every other port floats (reads return all-ones,
  writes are dropped), which lets a PCI-less/ACPI-less kernel skip legacy probes (i8042, CMOS/RTC,
  POST codes, ...). The in-kernel KVM irqchip (PIC + IOAPIC) and PIT provide interrupts and the
  timer; `kvm-clock` provides time.

### Source layout

| Path | Responsibility |
|------|----------------|
| `src/main.rs`         | CLI and entry point |
| `src/vmm.rs`          | VM setup, the vCPU run loop, console input thread, TTY handling |
| `src/memory.rs`       | Guest RAM as KVM user-memory regions (MMIO-gap aware; snapshot dump / COW restore) |
| `src/snapshot.rs`     | Full VM snapshot / restore (vCPU + devices + VM state) |
| `src/vcpu.rs`         | vCPU creation, CPUID, and the PVH entry register/segment state |
| `src/virtfs.rs`       | virt-fs: pack a `--mount` host directory into a SquashFS (ro) or ext4 (rw) image, map it into guest memory, and point the guest at it |
| `src/boot/pvh.rs`     | `vmlinux` ELF loader, PVH note parsing, `hvm_start_info` layout |
| `src/boot/params.rs`  | PVH boot-parameter structures |
| `src/devices/portb.rs` | portb console device: TX `outb` `0xE9`, RX poll `0xEA`/`0xE9`, host-input queue |
| `src/devices/mod.rs`  | PMIO device bus (portb console `0xE9`/`0xEA`, `0x604` shutdown, `0x605` snapshot) |
| `src/console.rs`      | Shared console sink: buffered/quiet output, byte count, cold-start timing |
| `src/irq.rs`          | In-kernel irqchip + PIT |
| `src/layout.rs`       | Guest-physical memory map constants |
| `kernel/config-microvm` | Minimal Linux kernel configuration |
| `kernel/hvc_xe9.c`    | The portb `hvc0` console driver (installed into the tree by `build-kernel.sh`) |
| `kernel/patches/`     | Kernel source modifications (the `0xE9` earlycon) |
| `alpine/init`         | PID 1 for the RAM initramfs |
| `alpine/init.python` | PID 1 for the Python initramfs (runs `pyapp=<file>`, default `hello.py`) |
| `alpine/hello.py`, `alpine/repl.py` | Python snapshot apps: pandas/numpy benchmark and interactive REPL |
| `scripts/`            | Build (`build-kernel.sh`, `build-initramfs.sh`, `build-python-initramfs.sh`), `run.sh`, `measure-coldstart.sh`, `snapshot-demo.sh`, `snapshot-boot.sh`, `test-boot.sh` |
| `scripts/`            | Kernel / initramfs build and run helpers |

## The kernel ("modified Alpine kernel")

`kernel/config-microvm` is a minimal x86_64 configuration built from the vanilla LTS source that
Alpine's `linux-lts` tracks. `scripts/build-kernel.sh` downloads the matching kernel source,
applies the modification in `kernel/patches/`, drops in this config, and builds an uncompressed
`vmlinux` carrying the PVH entry note.

### Configuration rationale

The guest sees almost no hardware — one CPU, RAM, the in-kernel interrupt controller, and a single
portb console — so the configuration follows one rule: **build in exactly what a device-less PVH/KVM
guest needs to boot, and compile out everything that would probe for hardware that is not there.**
Probing absent hardware is at best wasted boot time and at worst a multi-second hang (see
`PM_TRACE_RTC` below).

| Config | Why |
|--------|-----|
| `CONFIG_PVH=y` | Enter through the 32-bit PVH entry note, so the VMM loads an uncompressed `vmlinux` and skips the real-mode/bzImage setup and self-decompression path entirely. |
| `CONFIG_HYPERVISOR_GUEST=y`, `CONFIG_PARAVIRT=y`, `CONFIG_KVM_GUEST=y`, `CONFIG_PARAVIRT_CLOCK=y` | Run as a KVM guest and take time from `kvm-clock` — no PIT/HPET/TSC calibration, no RTC read at boot. |
| `# CONFIG_PCI`, `# CONFIG_ACPI` (and no EFI) | The VMM exposes no PCI bus, no ACPI tables, and no EFI/BIOS firmware; unclaimed I/O ports float. Enabling these makes the kernel enumerate buses and firmware that do not exist. |
| `# CONFIG_SERIAL_8250`, `CONFIG_HVC_XE9=y` | The console is the portb `hvc0` driver (`kernel/hvc_xe9.c`), not a 16550 UART — one `outb`/byte out, polled input in. `CONFIG_SERIAL_EARLYCON` stays for `earlycon=xe9`. |
| `# CONFIG_FB`, `# CONFIG_HID`, `# CONFIG_SOUND`, `# CONFIG_ATA`, `# CONFIG_SCSI`, `# CONFIG_RTC_CLASS`, no USB | None of these devices exist, so their drivers and boot-time probes are removed. |
| `CONFIG_BLK_DEV_INITRD=y`, `CONFIG_DEVTMPFS=y`, `CONFIG_TMPFS=y` | The whole userland is the initramfs unpacked into RAM; there is no block device or virtio, hence no storage stack. |
| `# CONFIG_NET` | No NIC and no virtio-net, so the entire network stack (plus NFS / IPv6 / wireless) is dropped. |
| `# CONFIG_MODULES` | Everything required is built in; a single static `vmlinux` needs no module loader. |
| `CONFIG_HZ_100=y`, `CONFIG_NO_HZ_IDLE=y` | A low 100 Hz tick with tickless idle: fewer timer interrupts, faster boot. |
| `# CONFIG_SUSPEND`, `# CONFIG_HIBERNATION`, `# CONFIG_X86_MCE`, `# CONFIG_NUMA` | Power management, machine-check, and NUMA are meaningless for a single-vCPU, device-less VM. |
| `# CONFIG_FTRACE`, `# CONFIG_KPROBES`, `# CONFIG_PROFILING`, `# CONFIG_DEBUG_KERNEL` | Tracing / debug / profiling infrastructure is compiled out to shrink the image and speed boot. |
| no `PM_TRACE_RTC` (gated off by no suspend) | **The load-bearing one.** With suspend/hibernate off there is no `PM_SLEEP`, so the `PM_TRACE` debug feature and its `PM_TRACE_RTC` are never built — which matters: `PM_TRACE_RTC`'s `early_resume_init` initcall reads the RTC via `mc146818_get_time()`, and with no RTC (ports `0x70`/`0x71` float) that read spins to a ~1 s timeout **twice**, most of the old ~1.6 s cold-start (see below). |

The only **source** change is `kernel/patches/0001-microvm-xe9-earlycon.patch`, which adds an
`earlycon=xe9` driver that emits each kernel-log byte with a single `outb` to I/O port `0xE9` —
see [the "portb" strategy](#the-portb-strategy-mirroring-nanvix).

## The initramfs (RAM filesystem)

`scripts/build-initramfs.sh` unpacks the official Alpine mini root filesystem, installs
`alpine/init` as PID 1 (it mounts `proc`/`sys`/`dev`/`tmpfs` and execs a shell), and packs it
as a gzipped `newc` cpio archive. The whole userland lives in RAM.

## Building and running

Requirements: a Linux host with `/dev/kvm` accessible to your user, a stable Rust toolchain
(edition 2024), and — for building the kernel — `flex`, `bison`, `libelf-dev`, `bc`, `cpio`,
`patch`.

Quick start:

```
make world          # build all three: the VMM + the modified kernel + the Alpine initramfs
make run            # boot Alpine to an interactive shell over the portb console
```

### Make targets

| Target | What it does |
|--------|--------------|
| `all` *(default)* | Alias for `release`. |
| `world` | Build all three components: `release` + `kernel` + `initramfs`. |
| `release` | Build the VMM in release mode → `target/release/microvm`. |
| `build` | Build the VMM in debug mode. |
| `test` | Run the unit tests (`cargo test --release`; no KVM required). |
| `kernel` | Download + patch + build the PVH `vmlinux` → `$(KERNEL_IMG)` (`scripts/build-kernel.sh`, ~minutes). |
| `initramfs` | Build the Alpine RAM rootfs → `$(BUILD_DIR)/initramfs.cpio.gz` (`scripts/build-initramfs.sh`). |
| `python-initramfs` | Build a rootfs with CPython + pandas/numpy → `$(PY_INITRD)` (`scripts/build-python-initramfs.sh`). |
| `run`, `boot` | Boot Alpine to an interactive shell over the portb console (`scripts/run.sh`). |
| `selftest` | Run the protected-mode self-test through the real PVH entry path and exit. |
| `boot-test` | End-to-end: boot and assert the guest reaches userspace (`scripts/test-boot.sh`). |
| `measure` | Cold-start measurements (`scripts/measure-coldstart.sh`). |
| `snapshot-demo` | pandas/numpy snapshot/restore benchmark (`scripts/snapshot-demo.sh`). |
| `snapshot-boot` | Resume an interactive Python interpreter from a snapshot (`scripts/snapshot-boot.sh`). |
| `clean` | `cargo clean`. |

`make kernel` / `initramfs` / `python-initramfs` always re-run their build script. `snapshot-demo`
and `snapshot-boot` instead depend on the artifacts `$(KERNEL_IMG)` and `$(PY_INITRD)` via file
rules that build **only when the artifact is missing** (and, for the Python initramfs, when its
`alpine/` sources change), so they work from a clean tree without rebuilding what is already there.

Both make and the scripts read these overridable variables from the environment:

| Variable | Default | Used by |
|----------|---------|---------|
| `CARGO` | `cargo` | the `release` / `build` / `test` / `clean` targets |
| `BUILD_DIR` | `$(HOME)/build` | where artifacts are written |
| `KERNEL_IMG` | `$(BUILD_DIR)/vmlinux` | kernel artifact path |
| `PY_INITRD` | `$(BUILD_DIR)/initramfs-python.cpio.gz` | Python initramfs artifact path |
| `KVER` | `6.18.38` | `build-kernel.sh` (kernel version) |
| `AVER` / `ABRANCH` | `3.24.1` / `v3.24` | the initramfs scripts (Alpine version) |
| `MEM`, `N`, `SNAP`, `KERNEL`, `INITRD` | (see each script) | `run.sh`, `snapshot-*.sh`, `measure-coldstart.sh` |


Run directly:

```
./target/release/microvm \
    --kernel  $HOME/build/vmlinux \
    --initrd  $HOME/build/initramfs.cpio.gz \
    --mem     512 \
    --cmdline "earlycon=xe9 console=hvc0 reboot=t panic=-1"
```

### CLI

| Flag | Default | Meaning |
|------|---------|---------|
| `--kernel <path>`  | (required)                       | Uncompressed `vmlinux` (PVH) |
| `--initrd <path>`  | (none)                           | RAM initramfs image |
| `--cmdline <str>`  | `earlycon=xe9 console=hvc0 reboot=t panic=-1`| Kernel command line |
| `--mem <MiB>`      | `512`                            | Guest RAM |
| `--quiet`          |                                  | Fully silent: discard guest console **and** suppress all VMM logging |
| `--log-level <lvl>`| `info` (`off` if `--quiet`)      | `off`/`error`/`warn`/`info`/`debug`/`trace`; `off` suppresses all logging (`RUST_LOG` overrides) |
| `--exit-on-boot`   |                                  | Stop and report cold-start/restore time when the boot marker appears |
| `--boot-marker <s>`| `ALPINE-MICROVM-BOOT-OK`         | Console substring that marks boot completion |
| `--snapshot <dir>` |                                  | Take a snapshot into `<dir>` when the guest requests one, then exit |
| `--restore <dir>`  |                                  | Restore and resume from a snapshot `<dir>` instead of booting |
| `--mount <dir>`    |                                  | Export a host directory to the guest as a virt-fs (read-only SquashFS by default) |
| `--mount-target <path>` | `/mnt/host`                 | Guest mount point for `--mount` |
| `--mount-rw`       |                                  | Mount the `--mount` export read-write (ext4); ephemeral without `--mount-image` |
| `--mount-image <file>` |                              | Persist a read-write `--mount` to this host image file (implies `--mount-rw`) |
| `--selftest`       |                                  | Run the protected-mode self-test and exit |

To **suppress all logging**, pass `--log-level off` (mutes the `[… INFO microvm::…]` lines but
still renders the guest console), or `--quiet` for a fully silent run (no guest console and no
logging). The cold-start line printed under `--exit-on-boot` goes to stderr independently of the
log level, so measurements keep working even when logging is off.

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
(`kernel/hvc_xe9.c`, installed by `scripts/build-kernel.sh`). Output is one `outb` to `0xE9`;
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
|---------------------------------|----------------:|-----------------------------:|
| to kernel→userspace handoff     | **265 ms**      | **194 ms**                   |

*End-to-end* (to the interactive shell):

| configuration | cold-start |
|---------------|-----------:|
| loud, full kernel logs | ~339 ms |
| silent (`quiet loglevel=0`) | ~133 ms |
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
   initcall tail dropped (158 ms -> ~135 ms).
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
guest `sync` before shutdown ensures writes reach a persistent image.

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

| path | to `{'x': 10, 'y': 30}` |
|------|-----------------------:|
| cold boot (kernel + Python startup + pandas/numpy import) | ~2.6 s |
| **restore from snapshot** | **~63 ms** (min ~55 ms) |

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

# microvm

A minimal, single-core **x86_64 KVM micro-VM** that boots a Linux (Alpine) kernel through
the **PVH boot protocol**, entirely from a **RAM initramfs** — no PCI, no ACPI, no block
device. The only emulated device is a 16550 serial console.

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
$ make run                           # boot Alpine to a shell over the serial console
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
  module; the kernel unpacks it and runs `/init`. There is no virtio and no block device.
- **Minimal device model.** A 16550A UART at I/O port `0x3F8` (`console=ttyS0`, IRQ 4) is the
  only device. Every other port floats (reads return all-ones, writes are dropped), which lets
  a PCI-less/ACPI-less kernel skip legacy probes (i8042, CMOS/RTC, POST codes, ...). The
  in-kernel KVM irqchip (PIC + IOAPIC) and PIT provide interrupts and the timer; `kvm-clock`
  provides time.

### Source layout

| Path | Responsibility |
|------|----------------|
| `src/main.rs`         | CLI and entry point |
| `src/vmm.rs`          | VM setup, the vCPU run loop, console input thread, TTY handling |
| `src/memory.rs`       | Guest RAM as KVM user-memory regions (MMIO-gap aware; snapshot dump / COW restore) |
| `src/snapshot.rs`     | Full VM snapshot / restore (vCPU + devices + VM state) |
| `src/vcpu.rs`         | vCPU creation, CPUID, and the PVH entry register/segment state |
| `src/boot/pvh.rs`     | `vmlinux` ELF loader, PVH note parsing, `hvm_start_info` layout |
| `src/boot/params.rs`  | PVH boot-parameter structures |
| `src/devices/serial.rs` | 16550A UART model |
| `src/devices/mod.rs`  | PMIO device bus (UART, `0xE9` debug port, `0x604` shutdown) |
| `src/console.rs`      | Shared console sink: buffered/quiet output, byte count, cold-start timing |
| `src/irq.rs`          | In-kernel irqchip + PIT |
| `src/layout.rs`       | Guest-physical memory map constants |
| `kernel/config-microvm` | Minimal Linux kernel configuration |
| `kernel/patches/`     | Kernel source modifications (the `0xE9` earlycon) |
| `alpine/init`         | PID 1 for the RAM initramfs |
| `alpine/init.python` | PID 1 for the Python initramfs (runs `pyapp=<file>`, default `hello.py`) |
| `alpine/hello.py`, `alpine/repl.py` | Python snapshot apps: hello-world demo and interactive REPL |
| `scripts/`            | Build (`build-kernel.sh`, `build-initramfs.sh`, `build-python-initramfs.sh`), `run.sh`, `measure-coldstart.sh`, `snapshot-demo.sh`, `snapshot-boot.sh`, `test-boot.sh` |
| `scripts/`            | Kernel / initramfs build and run helpers |

## The kernel ("modified Alpine kernel")

`kernel/config-microvm` is a minimal x86_64 configuration built from the vanilla LTS source
that Alpine's `linux-lts` tracks. Relative to a stock kernel it **removes PCI and PC-like
hardware detection** and everything that a paravirtual micro-VM does not need:

- `# CONFIG_PCI is not set`, `# CONFIG_ACPI is not set`
- no `CONFIG_XEN` (standalone PVH under KVM), no MP table / ISA / legacy platform probing
- no framebuffer/VGA, USB, sound, HID, ATA/SCSI, network drivers, loadable modules
- **no networking at all** (`CONFIG_NET` off), no wireless, no audit, no PPS, no suspend/
  hibernate, no machine-check/NUMA/microcode, no kprobes/profiling
- tuned for fast boot: `CONFIG_HZ=100`, tickless idle, no ftrace/tracing, no kernel debug,
  `CONFIG_RANDOM_TRUST_CPU=y` (instant entropy), and — critically — **`CONFIG_PM_TRACE_RTC`
  off** (see below)

and **enables** exactly what is required to boot:

- `CONFIG_PVH=y` (PVH entry), `CONFIG_KVM_GUEST=y` + paravirt clock
- `CONFIG_SERIAL_8250=y` / `CONFIG_SERIAL_8250_CONSOLE=y` (`ttyS0`)
- `CONFIG_BLK_DEV_INITRD=y`, `CONFIG_DEVTMPFS=y`, `CONFIG_TMPFS=y`

`scripts/build-kernel.sh` downloads the matching kernel source, applies the modifications in
`kernel/patches/` and this config, and builds an uncompressed `vmlinux` with the PVH entry note.

## The initramfs (RAM filesystem)

`scripts/build-initramfs.sh` unpacks the official Alpine mini root filesystem, installs
`alpine/init` as PID 1 (it mounts `proc`/`sys`/`dev`/`tmpfs` and execs a shell), and packs it
as a gzipped `newc` cpio archive. The whole userland lives in RAM.

## Building and running

Requirements: a Linux host with `/dev/kvm` accessible to your user, a stable Rust toolchain
(edition 2024), and — for building the kernel — `flex`, `bison`, `libelf-dev`, `bc`, `cpio`,
`patch`.

```
make world          # build all three: VMM + modified kernel + Alpine initramfs
make run            # boot it

# or individually:
make release        # build the VMM (cargo build --release)
make kernel         # download + patch + build $HOME/build/vmlinux   (~minutes)
make initramfs      # download Alpine + build $HOME/build/initramfs.cpio.gz

make test           # unit tests (no KVM required)
make selftest       # tiny protected-mode program through the real entry path
make boot-test      # end-to-end: boot and assert the guest reaches userspace
make measure        # cold-start measurements
```


Run directly:

```
./target/release/microvm \
    --kernel  $HOME/build/vmlinux \
    --initrd  $HOME/build/initramfs.cpio.gz \
    --mem     512 \
    --cmdline "console=ttyS0 reboot=t panic=-1"
```

### CLI

| Flag | Default | Meaning |
|------|---------|---------|
| `--kernel <path>`  | (required)                       | Uncompressed `vmlinux` (PVH) |
| `--initrd <path>`  | (none)                           | RAM initramfs image |
| `--cmdline <str>`  | `console=ttyS0 reboot=t panic=-1`| Kernel command line |
| `--mem <MiB>`      | `512`                            | Guest RAM |
| `--quiet`          |                                  | Fully silent: discard guest console **and** suppress all VMM logging |
| `--log-level <lvl>`| `info` (`off` if `--quiet`)      | `off`/`error`/`warn`/`info`/`debug`/`trace`; `off` suppresses all logging (`RUST_LOG` overrides) |
| `--exit-on-boot`   |                                  | Stop and report cold-start/restore time when the boot marker appears |
| `--boot-marker <s>`| `ALPINE-MICROVM-BOOT-OK`         | Console substring that marks boot completion |
| `--snapshot <dir>` |                                  | Take a snapshot into `<dir>` when the guest requests one, then exit |
| `--restore <dir>`  |                                  | Restore and resume from a snapshot `<dir>` instead of booting |
| `--selftest`       |                                  | Run the protected-mode self-test and exit |

To **suppress all logging**, pass `--log-level off` (mutes the `[… INFO microvm::…]` lines but
still renders the guest console), or `--quiet` for a fully silent run (no guest console and no
logging). The cold-start line printed under `--exit-on-boot` goes to stderr independently of the
log level, so measurements keep working even when logging is off.

## Cold-start and the `0xE9` debug console

The VMM measures **cold-start** — the time from the first guest instruction to a
boot-completion marker in the console stream — and reports it via `--exit-on-boot`:

```
$ ./target/release/microvm --kernel ~/build/vmlinux --initrd ~/build/initramfs.cpio.gz \
      --exit-on-boot --cmdline "console=ttyS0 reboot=t panic=-1"
...
[INFO microvm::console] cold-start: 1895.7 ms to userspace (13305 console bytes emitted)
```

### The "portb" strategy (mirroring Nanvix)

The Nanvix Micro-VM sends guest console output one byte at a time to a dedicated I/O port
(`0xE9`), so each character is a single `outb` — one VM exit — instead of the 8250 UART's
*read line-status + write data* (two exits, plus a poll loop). This VMM adds the same
**`0xE9` debug port**, and a matching **`earlycon=xe9`** driver in the kernel
(`scripts/build-kernel.sh` patches it in) so kernel logs can be redirected onto that path:

```
--cmdline "earlycon=xe9 keep_bootcon ..."     # kernel logs go out via port 0xE9
```

It also adds a `0x604` control port: a write requests VM shutdown (Nanvix `DEFAULT_VMM_PORT`).

### Measured cold-start (`make measure`)

Median of 10 runs, 512 MiB, 1 vCPU, host with nested KVM.

*Console transport* (to the kernel->userspace handoff, full kernel logs):

| transport | loud (rendered) | quiet (`--quiet`, discarded) |
|-----------|----------------:|-----------------------------:|
| UART `ttyS0` (2 exits/byte) | 337 ms | 256 ms |
| **portb `0xE9`** (1 exit/byte) | **265 ms** | **194 ms** |

*End-to-end* (to the interactive shell):

| configuration | cold-start |
|---------------|-----------:|
| loud, full kernel logs (`console=ttyS0`) | ~339 ms |
| silent (`console=ttyS0 quiet loglevel=0`) | ~133 ms |
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
- the emulated 16550 UART.

`state.bin` holds all of that; `mem.bin` is the raw RAM. On **restore**, the RAM image is
mapped copy-on-write (`MAP_PRIVATE`), so pages fault in lazily and there is no upfront copy —
restore is dominated by re-applying the vCPU/VM state, not by RAM size.

```
# take a snapshot when the guest requests one, then exit:
microvm --kernel vmlinux --initrd initramfs.cpio.gz --mem 256 --snapshot snap/

# boot from the snapshot (no kernel needed):
microvm --restore snap/ --mem 256
```

### Python "hello world" from a snapshot (`make snapshot-demo`)

`make python-initramfs` builds an initramfs with a full CPython interpreter and `alpine/hello.py`,
which warms up the interpreter, requests a snapshot, then prints its greeting. On a cold boot
the app pays for kernel boot **and** the entire Python startup; from a snapshot it resumes the
already-running interpreter and prints immediately.

Median of 8 runs, 256 MiB, 1 vCPU:

| path | to `hello world` |
|------|-----------------:|
| cold boot (kernel + Python startup) | ~380 ms |
| **restore from snapshot** | **~5.3 ms** (min ~4.9 ms) |

That is a **~70x** speedup: booting the Python app from a snapshot takes single-digit
milliseconds because it skips the kernel boot and the interpreter's startup entirely.
Reproduce with `make python-initramfs && make snapshot-demo`.

To instead drop into an **interactive Python interpreter resumed from a snapshot**, use
`make snapshot-boot`. It boots `alpine/repl.py`, which warms a full CPython interpreter, requests
a snapshot at the warmed point, and — on restore — resumes straight into a live `>>>` prompt on
the serial console, skipping the kernel boot and the entire Python startup. The snapshot is
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
- When standard input is a terminal it is put in raw mode for an authentic serial console;
  when input is piped (scripts, CI) it is left alone.

## Provenance & license

MIT licensed. Derived from the KVM backend of the Nanvix Micro-VM (`uservm`), © The
Maintainers of Nanvix (MIT). See `LICENSE`.

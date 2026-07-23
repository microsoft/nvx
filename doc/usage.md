# Running NVX

Use `scripts/nvx.py` for normal development workflows. It selects KVM on Linux and WHP on Windows,
finds artifacts in the platform's default directory, and invokes the correct host executable.
Invoke `microvm` directly when using VMM options that the workflow wrapper does not expose.

## Boot a guest

After following [Building NVX](build.md), boot the base Alpine image:

```console
# Linux
python3 scripts/nvx.py run
```

```powershell
# Windows
python scripts\nvx.py run
```

The defaults are:

| Host | VMM | Kernel and initramfs |
| --- | --- | --- |
| Linux | `target/release/microvm` | `$NVX_ARTIFACT_DIR`, or `$HOME/build` |
| Windows | `target\release\microvm.exe` | Repository `build\` directory |

Override inputs on either host:

```console
python scripts/nvx.py run \
  --kernel build/vmlinux \
  --initrd build/initramfs.cpio.gz \
  --mem 256
```

The guest prints `ALPINE-MICROVM-BOOT-OK` and starts `/bin/sh` on the bidirectional portb console.
Exit with `reboot -f`. The minimal machine has no ACPI power button, so `poweroff` leaves the guest
halted instead of terminating the VMM.

## Direct VMM invocation

Linux:

```console
./target/release/microvm \
  --kernel "$HOME/build/vmlinux" \
  --initrd "$HOME/build/initramfs.cpio.gz" \
  --mem 512 \
  --cmdline "earlycon=xe9 console=hvc0 reboot=t panic=-1"
```

Windows:

```powershell
.\target\release\microvm.exe `
  --kernel build\vmlinux `
  --initrd build\initramfs.cpio.gz `
  --mem 512 `
  --cmdline "earlycon=xe9 console=hvc0 reboot=t panic=-1"
```

When `--cmdline` is omitted, that command line is the backend default. An explicitly supplied
command line is used unchanged; feature setup may append device-specific tokens internally.

## VMM options

This table summarizes the checked-in Rust CLI. `microvm --help` is the final authority for the
built revision.

| Option | Default | Behavior |
| --- | --- | --- |
| `--kernel <path>` | none | Uncompressed PVH `vmlinux`; required for a cold boot. |
| `--initrd <path>` | none | Optional RAM initramfs image. |
| `--cmdline <text>` | portb console command line | Linux kernel command line. |
| `--mem <MiB>` | `512` | Guest RAM size; must be greater than zero. |
| `--vcpus <N>` | `1` | KVM vCPUs, from 1 through 254. WHP rejects values above 1. |
| `--quiet` | off | Discard guest console rendering and, unless overridden, disable VMM logs. |
| `--log-level <level>` | `info` | `off`, `error`, `warn`, `info`, `debug`, or `trace`; `RUST_LOG` overrides it. |
| `--exit-on-boot` | off | Exit and print timing when the boot marker is observed. |
| `--boot-marker <text>` | `ALPINE-MICROVM-BOOT-OK` | Console substring used by `--exit-on-boot`. |
| `--timing-marker <label=text>` | none | Time another substring; repeatable. |
| `--defer-stdin-until-boot` | off | Delay redirected cold-boot input until the boot marker. |
| `--snapshot <dir>` | none | Capture when the guest writes the snapshot control port, then exit. |
| `--restore <dir>` | none | Resume a snapshot instead of cold booting. |
| `--restore-ready-pipe <pipe>` | none | WHP-only named-pipe notification after restore is ready. |
| `--mount <dir>` | none | Export a host directory through virt-fs. |
| `--mount-target <path>` | `/mnt/host` | Guest path for the virt-fs mount. |
| `--mount-rw` | off | Use a writable, in-memory filesystem image. |
| `--mount-image <file>` | none | Use a persistent writable image; implies `--mount-rw`. |
| `--mount-size <MiB>` | computed | Size a new writable image. |
| `--net <IP/PREFIX>` | none | Attach standalone KVM TAP or WHP user-mode NAT networking. |
| `--net-config <json>` | none | WHP-only external AF_XDP L2Bridge manifest; conflicts with `--net`. |
| `--net-tap <name>` | none | KVM-only preconfigured TAP; valid with `--net` or a network snapshot restore. |
| `--selftest` | off | Exercise protected-mode setup without booting Linux. |

See [Networking](networking.md), [Virt-fs](virtfs.md), and
[Snapshot and Restore](snapshots.md) for the grouped feature options and platform differences.

## Console and logging

The guest's `hvc0` console uses I/O port `0xE9` for output and polls `0xEA`/`0xE9` for input. With a
terminal on standard input, NVX enables raw mode and restores it on exit. Redirected input remains
in normal mode; `--defer-stdin-until-boot` prevents scripted commands from reaching the guest before
PID 1 is ready.

`--log-level off` suppresses VMM logs while retaining guest console output. `--quiet` suppresses
both unless an explicit log level or `RUST_LOG` enables VMM logs. Timing output from
`--exit-on-boot` is emitted independently so automated measurements still work when logging is off.

## Workflow commands

Run `python scripts/nvx.py --help` for the complete generated list.

| Command | Purpose |
| --- | --- |
| `build-kernel` | Native Linux PVH kernel build. |
| `build-initramfs` | Native base Alpine initramfs build. |
| `build-python-initramfs` | Native or Docker Python image build. |
| `build-linux-artifacts` | Docker kernel and base initramfs build. |
| `run`, `boot` | Launch a cold guest. |
| `test-boot` | Require the userspace marker within a timeout. |
| `measure-coldstart` | Measure standard cold-start scenarios. |
| `snapshot-demo` | Compare cold and restored warmed Python. |
| `snapshot-boot` | Capture once and resume an interactive Python REPL. |
| `bench-snapshot-shell` | Compare shell cold boot and restore by memory size. |
| `bench-virtfs` | Measure virt-fs throughput and persistence. |
| `bench-net-snapshot` | Compare network-ready cold boot and restore. |
| `bench-net-snapshot-py` | Exercise networked Python snapshot workloads. |

The wrapper accepts `--backend auto|linux|kvm|windows|whp`; `auto` is the normal setting. Backend
selection changes host path and capability handling, not the operating system on which the VMM
binary can execute.

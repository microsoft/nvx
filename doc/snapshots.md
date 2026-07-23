# Snapshot and Restore

NVX can capture a running guest and resume it without repeating kernel boot or application startup.
Snapshots are initiated by the guest: when `--snapshot <dir>` is active, a guest write to control
port `0x605` causes the VMM to save state and exit.

The base and Python initramfs images install `/sbin/nvx-snapshot` to issue that request. Because the
write occurs in a userspace system call and the VMM advances past the I/O instruction before saving,
the restored guest resumes immediately after the request.

## Capture and restore

Capture a base guest when it requests a snapshot:

```console
./target/release/microvm \
  --kernel "$HOME/build/vmlinux" \
  --initrd "$HOME/build/initramfs.cpio.gz" \
  --mem 256 \
  --snapshot ./snap
```

The ordinary interactive initramfs does not request a snapshot unless its command line selects a
snapshot scenario. From a shell, request one explicitly:

```console
/ # /sbin/nvx-snapshot
```

Resume without a kernel or initramfs:

```console
./target/release/microvm --restore ./snap --mem 256
```

Use the same memory size as the captured guest. Processor count comes from a KVM snapshot; WHP is
single-vCPU. Snapshot files are backend-specific and should be treated as tied to the state format
implemented by the VMM revision that created them.

## Directory format

Every snapshot directory contains:

| File | Contents |
| --- | --- |
| `mem.bin` | Raw guest RAM in guest-physical order, stored sparsely where pages are zero. |
| `state.bin` | Framed processor, interrupt-controller, clock, PMIO, and optional network state. |

The logical length of `mem.bin` is the guest RAM size, but its allocated disk footprint tracks
nonzero memory. Restore maps it copy-on-write and faults pages in on demand. The original snapshot
is never modified, so it can be replayed repeatedly.

KVM state files use the current `MVMSNAP2` format; WHP state files use `WHPSNAP1`. They are not
cross-backend compatible.

## Captured state

### KVM

- Every vCPU's GPRs, segments, FPU, XCRs, XSAVE state, LAPIC, multiprocessor state, pending events,
  debug registers, TSC frequency, and Linux-relevant MSRs.
- In-kernel master/slave PIC, IOAPIC, PIT, and KVM clock.
- Shared PMIO device state and pending console input.
- Virtio-net endpoint and queue/transport state when a NIC is present.

For SMP, all vCPUs are brought out of `KVM_RUN` and device threads are stopped before capture. On
restore NVX synchronizes one reference TSC across processors before rearming deadline timers.

### WHP

- The single vCPU's GPRs, segments, tables, control/debug registers, model-specific registers, and
  a complete FPU/SSE/AVX/CET XSAVE area including `IA32_XSS`.
- WHP's emulated local APIC state.
- NVX's 8259 PIC, i8253 PIT, MC146818 RTC, portb console queue, and optional virtio-net state.

WHP writes temporary RAM and state files, publishes `mem.bin`, then publishes `state.bin` last as
the completion marker. It restores RAM with `PAGE_WRITECOPY` and `FILE_MAP_COPY`; KVM uses
`MAP_PRIVATE` for the same lazy, replayable behavior.

## Networking

Standalone KVM TAP and WHP NAT endpoint details are saved with the NIC, so `--net` is not required
on restore. KVM may receive `--net-tap <name>` to attach a preconfigured TAP instead of creating
one.

An external AF_XDP snapshot requires a fresh `--net-config`. NVX allows the host attachment and
queue resources to be reprovisioned but requires the saved guest-visible MAC, MTU, address, routes,
and DNS bootstrap to match. See [Networking](networking.md#networking-across-snapshot-and-restore).

## Python workflows

The Python initramfs provides two ready-made snapshot flows.

Compare a cold pandas/NumPy startup with an already warmed interpreter:

```console
python scripts/nvx.py snapshot-demo
```

The application imports pandas and NumPy, runs its DataFrame calculation once, and requests a
snapshot. Restore resumes after that request and repeats the already-warm calculation.

Capture an interactive Python interpreter once and reuse it:

```console
python scripts/nvx.py snapshot-boot
```

If the configured snapshot does not exist, the command cold-boots `alpine/repl.py` and captures it.
Subsequent runs restore directly to the `>>>` prompt. Use `--smoke-test` to feed `exit()` and verify
the restored banner non-interactively.

Other workflows are:

| Command | Snapshot point and validation |
| --- | --- |
| `bench-snapshot-shell` | Immediately before the base initramfs boot marker, across memory sizes. |
| `bench-net-snapshot` | After standalone network setup; verifies a gateway probe after restore. |
| `bench-net-snapshot-py` | Warm Python applications with a real HTTP round trip before and after restore. |

See [Benchmark Reference](benchmark.md) for canonical metric definitions rather than relying on
historical timing examples.

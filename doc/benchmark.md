# Benchmark Reference

NVX runs the same 23-metric benchmark suite on Linux/KVM, Windows/WHP, and
Windows/HCN + AF_XDP. CI reports the median (p50) for each metric in the job
summary. Latency metrics are lower-is-better; throughput metrics are
higher-is-better.

Use this page for metric names and methodology. Current historical p50 values live in
`data/performance/`; timings copied into old discussions or commit messages are not baselines.

## Running locally

Build the release VMM and required guest artifacts first. See [Building NVX](build.md).

```console
# Linux
python3 scripts/nvx.py measure-coldstart --runs 5
python3 scripts/nvx.py bench-virtfs --runs 3
python3 scripts/nvx.py snapshot-demo --runs 5
python3 scripts/nvx.py bench-snapshot-shell --runs 5 --memories "64 128 256 512"
python3 scripts/nvx.py bench-net-snapshot --runs 5
```

Use `python scripts\nvx.py ...` on Windows. The Python snapshot benchmark requires
`initramfs-python.cpio.gz`. KVM network benchmarks require permission to configure a TAP; the WHP
standalone network uses user-mode NAT.

Useful overrides include `KERNEL`, `INITRD`, `MEM`, `CORES`, `N`, `SNAP`, `NET`, `PAYLOAD_MB`, and
`IMG_MB`. Each subcommand also accepts explicit options; run
`python scripts/nvx.py <command> --help` for the exact surface.

## Benchmark commands

| Benchmark | KVM and WHP | HCN + AF_XDP | Description |
| --- | --- | --- | --- |
| Cold start | `measure-coldstart` | `measure-coldstart` | Measures guest start to kernel/userspace or shell-ready markers under rendered, discarded, silent, and tuned configurations. |
| Virtual file system | `bench-virtfs` | `bench-virtfs` | Measures sequential guest write/read throughput for ephemeral and persistent storage, then verifies data across cold VM launches. |
| Python snapshot | `snapshot-demo` | `snapshot-demo` | Compares a cold Python start, including warmed pandas/numpy work, with restoration of the already-warmed interpreter. |
| Shell snapshot | `bench-snapshot-shell` | `bench-snapshot-shell` | Compares cold boot with shell-ready snapshot restore at 64, 128, 256, and 512 MiB. |
| Network snapshot | `bench-net-snapshot` | `bench-hcn-afxdp-snapshot` | Compares a network-ready cold boot with snapshot restore. HCN + AF_XDP verifies gateway connectivity after every restore. |

## Canonical metrics

### Cold start

These five scenarios normally use five samples. All values are milliseconds from
the first guest instruction to the selected console marker.

| Metric | Description |
| --- | --- |
| `cold_start_kernel_handoff_loud` | Kernel-to-userspace handoff while rendering the full console stream. |
| `cold_start_kernel_handoff_quiet` | Kernel-to-userspace handoff while collecting but discarding console output. |
| `cold_start_shell_loud` | End-to-end boot to the interactive-shell banner with full logs rendered. |
| `cold_start_shell_silent` | End-to-end boot to the shell banner with quiet kernel logging and discarded console output. |
| `cold_start_fast` | Tuned, silent 128 MiB boot to the shell banner using the benchmark's fast-path kernel command line. |

### Virtual file system

Throughput scenarios normally use three samples and a 64 MiB payload. The reuse
scenario measures complete process wall time in milliseconds.

| Metric | Unit | Description |
| --- | --- | --- |
| `virtfs_ephemeral_write` | MB/s | Sequential `dd` write with `fsync` to a fresh ephemeral backing store. |
| `virtfs_ephemeral_read` | MB/s | Sequential read from the ephemeral store after dropping guest page cache. |
| `virtfs_persistent_write` | MB/s | Sequential `dd` write with `fsync` to a host-persistent backing store. |
| `virtfs_persistent_read` | MB/s | Sequential read from the persistent store after dropping guest page cache. |
| `virtfs_reuse` | ms | Cold VM launch, reopen the persistent store, and verify the previously written payload checksum. |

### Python snapshot

These scenarios normally use five samples and a 512 MiB guest.

| Metric | Description |
| --- | --- |
| `python_snapshot_cold` | Guest start to the result marker after CPython starts, imports pandas/numpy, and completes the warmed computation. |
| `python_snapshot_restore` | Snapshot resume to the same result marker with the interpreter and imported modules already warm. |

### Shell snapshot

Each memory size normally uses five cold boots and five restores. Values are
milliseconds to the shell-ready `ALPINE-MICROVM-BOOT-OK` marker.

| Metric | Description |
| --- | --- |
| `shell_snapshot_cold_64_mib` | Cold boot to a shell-ready guest with 64 MiB of memory. |
| `shell_snapshot_restore_64_mib` | Restore a shell-ready 64 MiB snapshot. |
| `shell_snapshot_cold_128_mib` | Cold boot to a shell-ready guest with 128 MiB of memory. |
| `shell_snapshot_restore_128_mib` | Restore a shell-ready 128 MiB snapshot. |
| `shell_snapshot_cold_256_mib` | Cold boot to a shell-ready guest with 256 MiB of memory. |
| `shell_snapshot_restore_256_mib` | Restore a shell-ready 256 MiB snapshot. |
| `shell_snapshot_cold_512_mib` | Cold boot to a shell-ready guest with 512 MiB of memory. |
| `shell_snapshot_restore_512_mib` | Restore a shell-ready 512 MiB snapshot. |

### Network snapshot

These scenarios normally use five samples. A run is accepted only after the
network verification marker is observed. Cold boot and restore both time to one
successful ICMP echo to the configured gateway; the one-second timeout bounds a
failed sample without adding an interval between successful packets.

| Metric | Description |
| --- | --- |
| `network_snapshot_cold` | Guest start to the network-ready marker after interface configuration and a real connectivity probe. |
| `network_snapshot_restore` | Guest resume to the verified restored-network marker. |
| `network_snapshot_restore_wall` | End-to-end host process wall time for restoring the snapshot, rebuilding the network backend, verifying connectivity, and exiting. |

## CI collection

`scripts/performance.py collect --require-shared-suite` rejects a backend result
unless it contains exactly the 23 metrics above. Each backend job publishes its
p50 table to `$GITHUB_STEP_SUMMARY`. Pull-request regression checks compare KVM
and WHP results with the latest base-branch history; privileged HCN/AF_XDP results
are collected when the hardware job is enabled.

The current workflow uses the latest 10 p50 samples on the pull request's base branch. A metric
regresses only when it is more than 50% worse. Lower-is-better millisecond metrics must also be
more than 10 ms slower; higher-is-better metrics use the percentage comparison alone. A missing
history is a warmup, not a failure. Successful main builds append collected results to
`data/performance/`.

## Cold-start methodology

Cold-start timing begins at the first guest instruction and ends when the configured console
substring appears. The standard suite distinguishes the kernel-to-userspace handoff from the
shell-ready marker and separates rendered console output from discarded output. This matters
because each portb byte is a VM exit.

The `earlycon=xe9` and `hvc0` path uses one `outb` per byte. It replaced a 16550-style path that
would normally require a line-status read plus a data write. `--quiet` discards bytes in the VMM,
while `quiet loglevel=0` also suppresses most bytes in the guest; the benchmark records these as
different scenarios.

The fastest standard scenario uses 128 MiB and this class of trusted, single-tenant tuning:

```text
clocksource=<backend> tsc=reliable no_timer_check random.trust_cpu=on
rcupdate.rcu_expedited=1 nokaslr mitigations=off cryptomgr.notests
earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1
```

Major cold-start contributors identified while building the current kernel configuration were:

- `PM_TRACE_RTC` wall-clock probing, which can wait on absent or minimal RTC behavior;
- initialization and probing for hardware subsystems the machine does not expose;
- per-page kernel metadata initialization as guest RAM grows;
- console VM exits and terminal rendering.

The checked-in kernel therefore omits unused PCI, storage, graphics, sound, power-management,
tracing, and debug stacks, keeps a minimal RTC implementation, and uses a low tick rate with
tickless idle. Treat these as design constraints when changing `kernel/config-microvm`; validate
both boot correctness and the relevant cold-start metrics.

# Benchmark Reference

NVX runs the same 23-metric benchmark suite on Linux/KVM, Windows/WHP, and
Windows/HCN + AF_XDP. CI reports the median (p50) for each metric in the job
summary. Latency metrics are lower-is-better; throughput metrics are
higher-is-better.

## Benchmark commands

| Benchmark | KVM and WHP | HCN + AF_XDP | Description |
| --- | --- | --- | --- |
| Cold start | `measure-coldstart` | `measure-coldstart` | Measures guest start to kernel/userspace or shell-ready markers under rendered, discarded, silent, and tuned configurations. |
| Virtual file system | `bench-virtfs` | `bench-virtfs` | Measures sequential guest write/read throughput for ephemeral and persistent storage, then verifies data across cold VM launches. |
| Python snapshot | `snapshot-demo` | `snapshot-demo` | Compares a cold Python start, including warmed pandas/numpy work, with restoration of the already-warmed interpreter. |
| Shell snapshot | `bench-snapshot-shell` | `bench-snapshot-shell` | Compares cold boot with shell-ready snapshot restore at 64, 128, 256, and 512 MiB. |
| Network snapshot | `bench-net-snapshot` | `benchmark-hcn-afxdp-snapshot.ps1` | Compares a network-ready cold boot with snapshot restore. HCN + AF_XDP verifies gateway connectivity after every restore. |

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

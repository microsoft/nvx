#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import contextlib
import copy
import io
import json
import re
import statistics
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import Any, cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import benchmark, benchmark_host, performance  # noqa: E402
from nvx_tools.common import sha256_file  # noqa: E402

COLD_START_LOG = """
    base                     :    101.0 ms  (min 100, max 102, n=5)
    clocksource=tsc          :    102.0 ms  (min 101, max 103, n=5)
    tsc=reliable             :    103.0 ms  (min 102, max 104, n=5)
    no_timer_check           :    104.0 ms  (min 103, max 105, n=5)
    random.trust_cpu=on      :    105.0 ms  (min 104, max 106, n=5)
    rcupdate.rcu_expedited=1 :    106.0 ms  (min 105, max 107, n=5)
    nokaslr                  :    107.0 ms  (min 106, max 108, n=5)
    mitigations=off          :    108.0 ms  (min 107, max 109, n=5)
    cryptomgr.notests        :    109.0 ms  (min 108, max 110, n=5)
"""

VIRTFS_LOG = """
    rw live host directory      write    500.0 MB/s  (min 490, max 510, n=3)
                                                            read   1,200.0 MB/s  (min 1100, max 1300, n=3)
    write samples                     : 490.0, 500.0, 510.0 MB/s
    read samples                      : 1100.0, 1200.0, 1300.0 MB/s
    live exchange (cold each)         :     200 ms  (min 190, max 210, n=3)
    exchange samples                  : 190.000, 200.000, 210.000 ms
"""

DEVICE_IO_LOG = """
NVX_DEVICE_IO_RESULT={"schema_version":1,"attempt_index":0,"device":"virtio-blk","sample_index":null,"status":"success","warmup":true,"results":[{"device":"virtio-blk","operation":"read","operations":900,"bytes_per_operation":4096,"elapsed_ns":1000000000},{"device":"virtio-blk","operation":"write","operations":800,"bytes_per_operation":4096,"elapsed_ns":1000000000}]}
NVX_DEVICE_IO_RESULT={"schema_version":1,"attempt_index":1,"device":"virtio-blk","sample_index":0,"status":"success","warmup":false,"results":[{"device":"virtio-blk","operation":"read","operations":100,"bytes_per_operation":4096,"elapsed_ns":1000000000},{"device":"virtio-blk","operation":"write","operations":80,"bytes_per_operation":4096,"elapsed_ns":1000000000}]}
NVX_DEVICE_IO_RESULT={"schema_version":1,"attempt_index":2,"device":"virtio-blk","sample_index":1,"status":"failure","warmup":false,"results":[],"error":"missing read result"}
NVX_DEVICE_IO_RESULT={"schema_version":1,"attempt_index":3,"device":"virtio-blk","sample_index":2,"status":"success","warmup":false,"results":[{"device":"virtio-blk","operation":"read","operations":600,"bytes_per_operation":4096,"elapsed_ns":2000000000},{"device":"virtio-blk","operation":"write","operations":400,"bytes_per_operation":4096,"elapsed_ns":2000000000}]}
NVX_DEVICE_IO_RESULT={"schema_version":1,"attempt_index":0,"device":"virtio-fs","sample_index":0,"status":"success","warmup":false,"results":[{"device":"virtio-fs","operation":"read","operations":300,"bytes_per_operation":4096,"elapsed_ns":1000000000},{"device":"virtio-fs","operation":"write","operations":200,"bytes_per_operation":4096,"elapsed_ns":1000000000}]}
NVX_DEVICE_IO_RESULT={"schema_version":1,"attempt_index":0,"device":"virtio-net","sample_index":0,"status":"success","warmup":false,"results":[{"device":"virtio-net","operation":"roundtrip","operations":75,"bytes_per_operation":64,"elapsed_ns":500000000}]}
"""


def device_io_log(*, warmups: int = 0, runs: int = 1) -> str:
    lines: list[str] = []
    for device, operations in performance.DEVICE_IO_OPERATIONS.items():
        for attempt_index in range(warmups + runs):
            warmup = attempt_index < warmups
            results: list[dict[str, object]] = []
            for operation in operations:
                results.append(
                    {
                        "device": device,
                        "operation": operation,
                        "operations": 100 + attempt_index,
                        "bytes_per_operation": 64 if device == "virtio-net" else 4096,
                        "elapsed_ns": 1_000_000_000,
                    }
                )
            record: dict[str, object] = {
                "schema_version": 1,
                "attempt_index": attempt_index,
                "device": device,
                "sample_index": None if warmup else attempt_index - warmups,
                "status": "success",
                "warmup": warmup,
                "results": results,
            }
            lines.append(
                performance.DEVICE_IO_RESULT_PREFIX
                + json.dumps(record, separators=(",", ":"))
            )
    return "\n".join(lines) + "\n"


SNAPSHOT_LOG = """
  cold:       300.0 ms  (min 290.0, max 310.0, n=5)
  restore:     30.0 ms  (min 29.0, max 31.0, n=5)
"""

HELLO_SNAPSHOT_LOG = """
    cold:      1,100.0 ms  (min 1,090.0, max 1,110.0, n=5)
    restore:     125.0 ms  (min 120.0, max 130.0, n=5)
"""

NETWORK_LOG = """
  cold  (guest start -> marker):   400.0 ms  (min 390.0, max 410.0, n=5)
  cold wall-clock               :   450.0 ms  (min 440.0, max 460.0, n=5)
  restore (guest resume -> marker):  40.0 ms  (min 39.0, max 41.0, n=5)
  restore wall-clock             :   50.0 ms  (min 49.0, max 51.0, n=5)
"""

SHELL_SNAPSHOT_LOG = """
== 128 MiB ==
    cold boot           : median   520.0 ms   (min 510.0, max 530.0, n=5)
    snapshot restore    : median     5.5 ms   (min 5.3, max 5.7, n=5)
    restore samples     : 5.300, 5.400, 5.500, 5.600, 5.700 ms
    speedup             : 95x (fast-path cold) .. 95x (median cold) faster via snapshot

== 256 MiB ==
    cold boot           : median   540.0 ms   (min 530.0, max 550.0, n=5)
    snapshot restore    : median     6.0 ms   (min 5.8, max 6.2, n=5)
    restore samples     : 5.800, 5.900, 6.000, 6.100, 6.200 ms
    speedup             : 90x (fast-path cold) .. 90x (median cold) faster via snapshot

== 512 MiB ==
    cold boot           : median   580.0 ms   (min 570.0, max 590.0, n=5)
    snapshot restore    : median     7.0 ms   (min 6.8, max 7.2, n=5)
    restore samples     : 6.800, 6.900, 7.000, 7.100, 7.200 ms
    speedup             : 83x (fast-path cold) .. 83x (median cold) faster via snapshot
"""

SHELL_SNAPSHOT_RESTORE_LOG = """
== 512 MiB ==
    snapshot restore    : median     7.0 ms   (p95 7.2, min 6.8, max 7.2, n=5)
    restore samples     : 6.800, 6.900, 7.000, 7.100, 7.200 ms
"""


def lifecycle_document(
    backend: str = "kvm",
    *,
    teardown_mode: str = "guest-exit",
    teardown_timeout_count: int = 0,
) -> dict[str, object]:
    mib = 1024 * 1024

    def samples(minimum: int | float, median: int | float, maximum: int | float):
        return [minimum, *([median] * 8), maximum]

    return {
        "controls": {
            "suite": "e2e",
            "runs": 10,
            "memory_mib": 128,
            "teardown_mode": teardown_mode,
            "marker": "ALPINE-MICROVM-BOOT-OK",
            "restore_marker": "OPENVMM-SNAPSHOT-RESTORE-OK",
            "snapshot_capture_timing": "openvmm-input-gate-to-publication",
            "snapshot_restore_guest_exit_prequeued": True,
        },
        "backends": {
            backend: {
                "samples_ms": samples(190.0, 200.5, 210.0),
                "p50_ms": 200.5,
                "min_ms": 190.0,
                "max_ms": 210.0,
                "peak_rss_samples_bytes": samples(60 * mib, 64 * mib, 70 * mib),
                "peak_rss_p50_bytes": 64 * mib,
                "peak_rss_min_bytes": 60 * mib,
                "peak_rss_max_bytes": 70 * mib,
                "teardown_completed_samples_ms": samples(24.0, 25.25, 27.0),
                "teardown_timeout_count": teardown_timeout_count,
                "teardown_p50_ms": 25.25,
                "teardown_min_ms": 24.0,
                "teardown_max_ms": 27.0,
            }
        },
        "snapshot_capture": {
            backend: {
                "samples_ms": samples(30.0, 31.0, 32.0),
                "p50_ms": 31.0,
                "min_ms": 30.0,
                "max_ms": 32.0,
                "request_to_publication_samples_ms": samples(530.0, 531.0, 532.0),
                "request_to_publication_p50_ms": 531.0,
                "request_to_publication_min_ms": 530.0,
                "request_to_publication_max_ms": 532.0,
                "post_publication_exit_samples_ms": samples(1.0, 1.1, 1.2),
                "post_publication_exit_p50_ms": 1.1,
                "post_publication_exit_min_ms": 1.0,
                "post_publication_exit_max_ms": 1.2,
                "peak_rss_samples_bytes": samples(70 * mib, 72 * mib, 75 * mib),
                "peak_rss_p50_bytes": 72 * mib,
                "peak_rss_min_bytes": 70 * mib,
                "peak_rss_max_bytes": 75 * mib,
            }
        },
        "snapshot_restore": {
            backend: {
                "samples_ms": samples(19.0, 20.25, 21.0),
                "p50_ms": 20.25,
                "min_ms": 19.0,
                "max_ms": 21.0,
                "peak_rss_samples_bytes": samples(30 * mib, 32 * mib, 34 * mib),
                "peak_rss_p50_bytes": 32 * mib,
                "peak_rss_min_bytes": 30 * mib,
                "peak_rss_max_bytes": 34 * mib,
                "teardown_completed_samples_ms": samples(5.0, 5.5, 6.0),
                "teardown_timeout_count": teardown_timeout_count,
                "teardown_p50_ms": 5.5,
                "teardown_min_ms": 5.0,
                "teardown_max_ms": 6.0,
            }
        },
    }


# Snapshot generation from run 35393916034, whose minority fast path the
# stability guard rejects.
MINORITY_FAST_PATH_MS = [
    763.3528,
    1219.9358,
    1242.3189,
    1256.6633,
    1275.0555,
    1274.4436,
    737.5818,
    1257.1264,
    1240.7679,
    1231.9344,
]
BENCHMARK_HOST: dict[str, object] = {
    "schema_version": 1,
    "runner_name": "azure-windows-3",
    "machine_name": "VMSSVAOAN000002",
    "os": "Windows-2022Server-10.0.20348-SP0",
    "cpu_model": "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz",
    "logical_processors": 16,
    "memory_bytes": 64 * 1024**3,
    "azure": {
        "vm_size": "Standard_D16ds_v5",
        "os_disk": {
            "storage_account_type": "Premium_LRS",
            "size_gib": 128,
            "caching": "ReadWrite",
        },
        "data_disks": [],
        "resource_disk_size_kib": 0,
    },
    "volumes": {
        "scratch": {
            "path": "F:\\nvx-benchmark-scratch\\job",
            "volume": "F:",
            "filesystem": "NTFS",
            "size_bytes": 512 * 1024**3,
        }
    },
}
# The provenance that the KVM and WHP benchmark runners recorded in run
# 38377573110, with the CPU signatures and OS builds that doctor H2 reported.
KVM_RUNNER_HOST: dict[str, object] = {
    "schema_version": 1,
    "runner_name": "azure-kvm-3",
    "machine_name": "esaurez-nvx-ci-linux-kvm-vmss000003",
    "os": "Linux-6.6.157.1-1.azl3-x86_64-with-glibc2.38",
    "os_version": {"system": "Linux", "release": "6.6.157.1-1.azl3", "revision": None},
    "cpu_model": "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz",
    "cpu": {
        "vendor": "GenuineIntel",
        "family": 6,
        "model": 106,
        "stepping": 6,
        "microcode": "0xffffffff",
    },
    "logical_processors": 8,
    "memory_bytes": 33605677056,
    "azure": {
        "vm_size": "Standard_D8s_v5",
        "os_disk": {
            "storage_account_type": "Premium_LRS",
            "size_gib": 128,
            "caching": "ReadWrite",
            "write_accelerator_enabled": False,
            "ephemeral_option": None,
        },
        "data_disks": [
            {
                "lun": 0,
                "storage_account_type": "Premium_LRS",
                "size_gib": 512,
                "caching": "None",
                "write_accelerator_enabled": False,
                "ephemeral_option": None,
                "bytes_per_second_throttle": None,
                "ops_per_second_throttle": None,
            }
        ],
        "resource_disk_size_kib": 0,
    },
    "volumes": {
        "workspace": {
            "path": "/opt/nvx-runner/_work/nvx/nvx",
            "volume": "/",
            "filesystem": "ext4",
            "system_volume": True,
            "size_bytes": 134618656768,
            "device": "sda3",
            "disk": "sda",
            "scsi_address": "0:0:0:0",
        },
        "temporary": {
            "path": "/tmp",
            "volume": "/tmp",
            "filesystem": "tmpfs",
            "system_volume": False,
            "size_bytes": 16802840576,
        },
        "scratch": {
            "path": "/tmp",
            "volume": "/tmp",
            "filesystem": "tmpfs",
            "system_volume": False,
            "size_bytes": 16802840576,
        },
    },
}
WHP_RUNNER_HOST: dict[str, object] = {
    **copy.deepcopy(KVM_RUNNER_HOST),
    "runner_name": "azure-windows-5",
    "machine_name": "vmssvaoan000004",
    "os": "Windows-2025Server-10.0.26100-SP0",
    "os_version": {"system": "Windows", "release": "10.0.26100", "revision": 33438},
    "memory_bytes": 34301759488,
    "azure": {
        **copy.deepcopy(cast(dict[str, object], KVM_RUNNER_HOST["azure"])),
        "os_disk": {
            "storage_account_type": "Premium_LRS",
            "size_gib": 128,
            "caching": "None",
            "write_accelerator_enabled": False,
            "ephemeral_option": None,
        },
    },
    "volumes": {
        "workspace": {
            "path": "C:\\actions-runner\\_work\\nvx\\nvx",
            "volume": "C:",
            "filesystem": "NTFS",
            "size_bytes": 135771664384,
            "system_volume": True,
            "disk_number": 0,
            "scsi_address": "0:0:0:0",
        },
        "temporary": {
            "path": "C:\\Windows\\ServiceProfiles\\NetworkService\\AppData\\Local\\Temp",
            "volume": "C:",
            "filesystem": "NTFS",
            "size_bytes": 135771664384,
            "system_volume": True,
            "disk_number": 0,
            "scsi_address": "0:0:0:0",
        },
        "scratch": {
            "path": "F:\\nvx-benchmark-scratch\\windows-whp-virtual-machine-1-1",
            "volume": "F:",
            "filesystem": "NTFS",
            "size_bytes": 549721206784,
            "system_volume": False,
            "disk_number": 1,
            "scsi_address": "1:0:0:0",
        },
    },
}
KVM_SERIES = "linux-kvm-virtual-machine"
WHP_SERIES = "windows-whp-virtual-machine"
MSHV_SERIES = "linux-mshv-virtual-machine"
# azure-mshv-4, the MSHV benchmark runner, on 2026-10-28 (#980): its kernel
# and the Microsoft hypervisor beneath it, which CPUID leaf 0x40000002 reports.
MSHV_RUNNER_HOST: dict[str, object] = {
    **copy.deepcopy(KVM_RUNNER_HOST),
    "runner_name": "azure-mshv-4",
    "machine_name": "esaurez-nvx-ci-linux-vmss000005",
    "os": "Linux-6.18.34.mshv3-1.azl3-x86_64-with-glibc2.38",
    "os_version": {
        "system": "Linux",
        "release": "6.18.34.mshv3-1.azl3",
        "revision": None,
    },
    "hypervisor": {"vendor": "Microsoft Hv", "build": "10.0.26100.9444"},
    "cpu_model": "INTEL(R) XEON(R) PLATINUM 8573C",
    "cpu": {
        "vendor": "GenuineIntel",
        "family": 6,
        "model": 207,
        "stepping": 2,
        "microcode": "0xffffffff",
    },
    "memory_bytes": 33186406400,
}
# The provenance that NVX recorded before it read the hypervisor.
MSHV_RUNNER_HOST_BEFORE_980 = {
    key: value for key, value in MSHV_RUNNER_HOST.items() if key != "hypervisor"
}


def changed_host(
    host: dict[str, object], path: Sequence[str | int], value: object
) -> dict[str, object]:
    """Return a copy of ``host`` with the value at ``path`` replaced."""
    result = copy.deepcopy(host)
    parent: Any = result
    for key in path[:-1]:
        parent = parent[key]
    parent[path[-1]] = value
    return result


def profiled_lifecycle_document(
    samples_ms: list[float] | None = None,
) -> dict[str, object]:
    """Return a Windows lifecycle result with host provenance and storage
    telemetry whose scratch volume writes 160 MiB/s during fast flushes and
    96 MiB/s during slow ones."""
    document = lifecycle_document("whp")
    capture = cast(
        dict[str, object], cast(dict[str, object], document["snapshot_capture"])["whp"]
    )
    if samples_ms is not None:
        capture.update(
            samples_ms=samples_ms,
            p50_ms=statistics.median(samples_ms),
            min_ms=min(samples_ms),
            max_ms=max(samples_ms),
        )
    raw_samples: list[dict[str, object]] = []
    for generation_ms in cast(list[float], capture["samples_ms"]):
        raw_samples.append(
            {
                "generation_duration_ns": round(generation_ms * 1_000_000),
                "records": [
                    {
                        "operation": "capture",
                        "phase": "mapped_memory_flush",
                        "exclusive": True,
                        "duration_ns": round((generation_ms - 20) * 1_000_000),
                        "process_elapsed_ns": 2_000_000_000,
                        "pid": 4242,
                        "source": "openvmm",
                    }
                ],
                "storage_telemetry": {
                    "volumes": {"F:": ["scratch"], "C:": ["workspace", "system"]},
                    "phases": {
                        "capture.mapped_memory_flush": {
                            "volumes": {
                                "F:": {
                                    "write_mib_per_second": (
                                        160.0 if generation_ms < 1000 else 96.0
                                    ),
                                    "average_write_latency_ms": 31.25,
                                    "average_queue_length": 3.94,
                                    "busy_percent": 99.4,
                                },
                                "C:": {"write_mib_per_second": 0.25},
                            },
                            "cpu": {"busy_percent": 6.2},
                            "memory": {
                                "cache_dirty_mib_max": 12.34,
                                "lazy_write_mib_per_second": 0.0,
                            },
                            "defender": {"cpu_percent": 0.04},
                        }
                    },
                },
            }
        )
    capture["profile"] = {"schema_version": 1, "raw_samples": raw_samples}
    document["host"] = BENCHMARK_HOST
    return document


def validate_attempt(source: Path, log: Path) -> int:
    with (
        contextlib.redirect_stdout(io.StringIO()),
        contextlib.redirect_stderr(io.StringIO()),
    ):
        return nvx.main(
            [
                "performance",
                "validate-openvmm",
                "--platform",
                "windows-whp-virtual-machine",
                "--input",
                str(source),
                "--attempt-log",
                str(log),
            ]
        )


# 256 MiB restores of the Linux/MSHV platform job of CI run 37212738981: a host
# stall delayed the warmup and the first six samples and failed the gate (#345).
STALLED_RESTORE_MS = [
    85.689,
    74.955,
    69.875,
    72.078,
    107.193,
    47.428,
    37.112,
    37.751,
    36.336,
    36.276,
]
# 512 MiB restores of CI run 39844925001 on azure-mshv-4: only the last two
# samples escaped the stall, so only the gap limit sees it.
LATE_RECOVERY_RESTORE_MS = [66.6, 68.9, 82.1, 65.9, 66.7, 64.2, 63.8, 62.6, 44.0, 43.2]
# The rerun of CI run 37212738981's 256 MiB restores.
STABLE_RESTORE_MS = [37.2, 36.5, 36.5, 35.6, 36.2, 36.4, 35.7, 35.7, 37.1, 36.6]
# 256 MiB restores of CI run 38296918379 on azure-kvm-2. Both modes are usual
# on the KVM runners.
BIMODAL_KVM_RESTORE_MS = [58.9, 29.1, 59.6, 28.8, 26.9, 54.2, 61.8, 26.1, 59.1, 64.9]


def shell_snapshot_log(restores: dict[int, Sequence[float]]) -> str:
    """Render shell-snapshot.log as the harness writes it: each series' sample
    lines precede the section that summarizes it."""
    output = io.StringIO()
    with contextlib.redirect_stdout(output):
        for memory_mib, restored in restores.items():
            for index, value in enumerate(restored, 1):
                print(f"  sample {index}/{len(restored)}: {value:.3f} ms")
            cold = [300.0 + index for index in range(len(restored))]
            benchmark._print_shell_snapshot_summary(memory_mib, cold, restored)
    return output.getvalue()


def shell_snapshot_restore_log(restores: dict[int, Sequence[float]]) -> str:
    """Render shell-snapshot-restore.log as the restore-only suite writes it."""
    lines: list[str] = []
    for memory_mib, restored in restores.items():
        lines.extend(
            f"  sample {index}/{len(restored)}: {value:.3f} ms"
            for index, value in enumerate(restored, 1)
        )
        lines.extend(
            (
                f"== {memory_mib} MiB ==",
                benchmark._format_shell_snapshot_line("snapshot restore", restored),
                benchmark._format_restore_samples_line(restored),
                "",
            )
        )
    return "\n".join(lines)


def git_repository(path: Path) -> Callable[..., str]:
    """Initialize a git repository at ``path`` and return a git runner for it."""

    def git(*args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(path), *args],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    git("init", "-q")
    git("config", "user.email", "test@example.invalid")
    git("config", "user.name", "test")
    git("config", "commit.gpgsign", "false")
    return git


class PerformanceTests(unittest.TestCase):
    def test_ci_one_vcpu_metric_count_matches_collectors(self):
        expected = len(
            performance.SHARED_METRICS | performance.LIFECYCLE_METRICS
        ) + len(performance.DEVICE_IO_METRIC_NAMES)
        action = (
            Path(__file__).parents[1]
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(expected, 34)
        self.assertIn("-ne 35 ]]", action)
        self.assertIn("Count -ne 34", action)
        self.assertEqual(
            action.count("Expected 34 microVM one-vCPU metrics"),
            2,
        )

    def test_documented_metric_counts_match_collectors(self):
        shared = len(performance.SHARED_METRICS)
        lifecycle = len(performance.SHARED_METRICS | performance.LIFECYCLE_METRICS)
        one_vcpu = len(
            performance.SHARED_METRICS
            | performance.LIFECYCLE_METRICS
            | performance.DEVICE_IO_METRIC_NAMES
        )
        # Above one vCPU, an ABI-2 series records only shell_snapshot_restore_512_mib.
        per_series = one_vcpu + len(performance.MICROVM_PROCESSOR_COUNTS[2] - {1})
        repository = Path(__file__).parents[1]
        series = re.findall(
            r"^[ \t]*benchmark-platform:[ \t]*(\S+)[ \t]*$",
            (repository / ".github" / "workflows" / "ci.yml").read_text(
                encoding="utf-8"
            ),
            re.MULTILINE,
        )
        benchmarks, usage = (
            " ".join((repository / "doc" / name).read_text(encoding="utf-8").split())
            for name in ("benchmarks.md", "usage.md")
        )
        shell_memories = [str(memory) for memory in benchmark.SHELL_SNAPSHOT_MEMORY_MIB]
        profile_memories = [
            str(memory) for memory in benchmark.SNAPSHOT_PROFILE_MEMORY_MIB
        ]

        self.assertTrue(series)
        for name in series:
            self.assertIn(f"| `{name}` |", benchmarks)
        for phrase in (
            f"a {shared}-metric microVM non-Python workload suite",
            f"reports all {one_vcpu} median (p50) values",
            f"{per_series} p50 values per series, {one_vcpu} at one vCPU",
            f"and {per_series * len(series)} values across the",
            f"run all {shared} metrics",
            f"Runs {shared} metrics",
            "snapshot restore at "
            f"{', '.join(shell_memories[:-1])}, and {shell_memories[-1]} MiB",
            f"summarizes {'/'.join(profile_memories)} MiB warm/cold restores",
            f"exactly the {shared} shared metrics",
            f"producing a {lifecycle}-metric one-vCPU result",
            f"the final {one_vcpu}-metric ABI-2 one-vCPU result",
        ):
            self.assertIn(phrase, benchmarks)
        for phrase in (
            f"producing the {lifecycle}-metric microVM CI result",
            f"merges them into a {one_vcpu}-metric one-vCPU result",
            f"`{' '.join(shell_memories)}` (`{' '.join(profile_memories)}` for "
            "`snapshot-profile`)",
        ):
            self.assertIn(phrase, usage)
        documented_counts = re.findall(
            r"\b(\d+)(?:-metric\b| (?:shared )?metrics\b)", f"{benchmarks} {usage}"
        )
        self.assertLessEqual(
            {int(count) for count in documented_counts},
            {shared, lifecycle, one_vcpu},
        )

    def test_documented_metric_tables_match_collectors(self):
        document = (Path(__file__).parents[1] / "doc" / "benchmarks.md").read_text(
            encoding="utf-8"
        )
        _, heading, rest = document.partition("\n## Canonical metrics\n")
        section = rest.split("\n## ", 1)[0]
        documented = re.findall(r"^\| `([a-z0-9_]+)` \|", section, re.MULTILINE)

        self.assertTrue(heading)
        self.assertEqual(len(documented), len(set(documented)))
        self.assertEqual(
            set(documented),
            performance.SHARED_METRICS
            | performance.LIFECYCLE_METRICS
            | performance.DEVICE_IO_METRIC_NAMES,
        )

    def test_documented_gate_thresholds_match_ci_and_cli(self):
        repository = Path(__file__).parents[1]
        workflow = (repository / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        threshold, tolerance = (
            re.findall(rf"^[ \t]*{name}:[ \t]*(\d+)[ \t]*$", workflow, re.MULTILINE)
            for name in (
                "PERFORMANCE_REGRESSION_THRESHOLD",
                "PERFORMANCE_REGRESSION_ABSOLUTE_TOLERANCE_MS",
            )
        )
        benchmarks, usage = (
            " ".join((repository / "doc" / name).read_text(encoding="utf-8").split())
            for name in ("benchmarks.md", "usage.md")
        )
        defaults = nvx.parse_args(
            ["performance", "gate", "--baseline-dir", "base", "--target-dir", "target"]
        )
        threshold_default = f"{defaults.threshold:g}"
        tolerance_default = f"{defaults.absolute_tolerance_ms:g}"

        self.assertEqual(len(threshold), 1)
        self.assertEqual(len(tolerance), 1)
        self.assertIn(f"more than {threshold[0]}% worse", benchmarks)
        self.assertIn(f"more than {tolerance[0]} ms slower", benchmarks)
        self.assertIn(
            f"defaults to {threshold_default}% and {tolerance_default} ms", benchmarks
        )
        self.assertIn(f"`--threshold` to `{threshold_default}`", usage)
        self.assertIn(f"`--absolute-tolerance-ms` to `{tolerance_default}`", usage)

    def test_collect_cli_accepts_lifecycle_input(self):
        args = nvx.parse_args(
            [
                "performance",
                "collect",
                "--platform",
                "linux-kvm",
                "--commit",
                "abc123",
                "--input-dir",
                "logs",
                "--output-dir",
                "results",
                "--lifecycle-input",
                "acceptance.json",
            ]
        )

        self.assertEqual(args.lifecycle_input, Path("acceptance.json"))

    def test_collect_cli_accepts_restore_only_contract(self):
        args = nvx.parse_args(
            [
                "performance",
                "collect",
                "--platform",
                "linux-kvm-baremetal",
                "--commit",
                "abc123",
                "--input-dir",
                "logs",
                "--output-dir",
                "results",
                "--require-shell-snapshot-restore-512",
            ]
        )

        self.assertTrue(args.require_shell_snapshot_restore_512)

    def test_collects_openvmm_mshv_json(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "linux-mshv-virtual-machine.json"
            source.write_text(
                json.dumps(lifecycle_document("mshv")),
                encoding="utf-8",
            )

            result_path = performance.collect_openvmm_results(
                "linux-mshv-virtual-machine", "abc123", source, root / "results"
            )

            results = performance.read_results(result_path)
            self.assertEqual(
                [result.metric for result in results],
                [
                    "openvmm_cold_start",
                    "openvmm_cold_start_guest_exit_teardown",
                    "openvmm_cold_start_peak_rss",
                    "openvmm_snapshot_generation",
                    "openvmm_snapshot_generation_peak_rss",
                    "openvmm_snapshot_restore",
                    "openvmm_snapshot_restore_guest_exit_teardown",
                    "openvmm_snapshot_restore_peak_rss",
                ],
            )
            self.assertEqual(results[0].p50, 200.5)

    def test_collects_restore_rss_after_remeasured_attempt(self):
        mib = 1024 * 1024
        attempts = [
            (20.0, None, 5.0, 25.0),
            *(
                (19.0 + index / 10, (30 + index % 3) * mib, 5.0, 25.0)
                for index in range(10)
            ),
        ]
        with (
            patch.object(benchmark, "measure_once", side_effect=attempts),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            restore = benchmark.benchmark(
                ["openvmm"],
                warmups=0,
                runs=10,
                timeout=1,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
            )
        self.assertEqual(restore["peak_rss_remeasured_count"], 1)

        document = lifecycle_document("mshv")
        cast(dict[str, object], document["snapshot_restore"])["mshv"] = restore
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "acceptance.json"
            source.write_text(json.dumps(document), encoding="utf-8")

            result_path = performance.collect_openvmm_results(
                "linux-mshv-virtual-machine", "abc123", source, root / "results"
            )
            rss = next(
                result
                for result in performance.read_results(result_path)
                if result.metric == "openvmm_snapshot_restore_peak_rss"
            )
            self.assertEqual(rss.p50, 31.0)

            samples = restore["peak_rss_samples_bytes"]
            samples[7] = 0
            restore["peak_rss_p50_bytes"] = int(statistics.median(samples))
            restore["peak_rss_min_bytes"] = 0
            source.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(
                performance.PerformanceError,
                r"snapshot_restore\.mshv\.peak_rss_min_bytes must be positive",
            ):
                performance.collect_openvmm_results(
                    "linux-mshv-virtual-machine", "abc123", source, root / "rejected"
                )

    def test_collects_openvmm_json_and_appends_diagnostics(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "linux-kvm-baremetal.json"
            summary = root / "summary.md"
            source.write_text(
                json.dumps(lifecycle_document()),
                encoding="utf-8",
            )

            result_path = performance.collect_openvmm_results(
                "linux-kvm-baremetal", "abc123", source, root / "results", summary
            )

            results = performance.read_results(result_path)
            self.assertEqual(len(results), 8)
            self.assertEqual(results[0].p50, 200.5)
            self.assertTrue(all(result.direction == "lower" for result in results))
            by_metric = {result.metric: result for result in results}
            self.assertEqual(
                by_metric["openvmm_snapshot_generation_peak_rss"].unit,
                "MiB",
            )
            self.assertEqual(
                by_metric["openvmm_snapshot_generation_peak_rss"].p50,
                72.0,
            )
            markdown = summary.read_text(encoding="utf-8")
            self.assertIn("## Linux / KVM / Bare metal benchmark results", markdown)
            self.assertIn("| `openvmm_snapshot_restore` | 20.25 ms |", markdown)
            self.assertIn("## Linux / KVM / Bare metal lifecycle diagnostics", markdown)
            self.assertIn(
                "| Snapshot generation | 31.00 ms | 30.00 ms | 32.00 ms | 10 |",
                markdown,
            )
            self.assertEqual(by_metric["openvmm_snapshot_generation"].p50, 31.0)
            self.assertIn(
                "| Host snapshot request to publication (diagnostic) | "
                "531.00 ms | 530.00 ms | 532.00 ms | 10 |",
                markdown,
            )
            self.assertIn("| Cold start | 64.00 MiB | 70.00 MiB |", markdown)
            self.assertIn(
                "| Snapshot generation | 72.00 MiB | 75.00 MiB |",
                markdown,
            )
            self.assertIn("| Snapshot-restore speedup | 9.90x |", markdown)

    def test_openvmm_json_rejects_console_contaminated_timing_contracts(self):
        for field in (
            "snapshot_capture_timing",
            "snapshot_restore_guest_exit_prequeued",
        ):
            for value in (None, False, "legacy"):
                with self.subTest(field=field, value=value):
                    document = lifecycle_document()
                    controls = cast(dict[str, object], document["controls"])
                    controls[field] = value
                    with tempfile.TemporaryDirectory() as temporary:
                        source = Path(temporary) / "acceptance.json"
                        source.write_text(json.dumps(document), encoding="utf-8")
                        with self.assertRaisesRegex(
                            performance.PerformanceError,
                            "must use openvmm-input-gate|must prequeue",
                        ):
                            performance.read_lifecycle_data(
                                "linux-kvm-baremetal", source
                            )

    def test_openvmm_json_requires_the_platform_backend(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "windows-whp-virtual-machine.json"
            document = lifecycle_document("whp")
            document["backends"] = {}
            source.write_text(
                json.dumps(document),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                performance.PerformanceError,
                r"backends\.whp",
            ):
                performance.collect_openvmm_results(
                    "windows-whp-virtual-machine",
                    "abc123",
                    source,
                    Path(temporary) / "results",
                )

    def test_openvmm_json_requires_guest_exit_without_timeouts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "linux-mshv.json"
            source.write_text(
                json.dumps(lifecycle_document("mshv", teardown_mode="host-terminate")),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                performance.PerformanceError,
                r"must use guest-exit teardown",
            ):
                performance.collect_openvmm_results(
                    "linux-mshv", "abc123", source, root / "results"
                )

            source.write_text(
                json.dumps(lifecycle_document("mshv", teardown_timeout_count=1)),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                performance.PerformanceError,
                r"guest-exit teardown timeout",
            ):
                performance.collect_openvmm_results(
                    "linux-mshv", "abc123", source, root / "results"
                )

    def test_openvmm_json_rejects_aggregates_that_do_not_match_samples(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "linux-kvm.json"
            document = lifecycle_document()
            backends = cast(dict[str, object], document["backends"])
            kvm = cast(dict[str, object], backends["kvm"])
            kvm["p50_ms"] = 1.0
            kvm["min_ms"] = 1.0
            kvm["max_ms"] = 1.0
            source.write_text(json.dumps(document), encoding="utf-8")

            with self.assertRaisesRegex(
                performance.PerformanceError,
                r"inconsistent statistics.*calculated",
            ):
                performance.collect_openvmm_results(
                    "linux-kvm", "abc123", source, root / "results"
                )

    def test_openvmm_json_rejects_unstable_snapshot_generation(self):
        unstable = [
            739.1318,
            782.923,
            1240.3951,
            1249.8728,
            1268.4783,
            1275.3067,
            1280.3929,
            933.864,
            769.0158,
            744.6426,
        ]
        pooled_runner_stall = [
            736.2791,
            753.5391,
            780.7033,
            753.063,
            1272.4222,
            1260.2828,
            1271.4175,
            1297.1824,
            1233.186,
            747.7988,
        ]
        minority_fast_path = [
            763.3528,
            1219.9358,
            1242.3189,
            1256.6633,
            1275.0555,
            1274.4436,
            737.5818,
            1257.1264,
            1240.7679,
            1231.9344,
        ]
        consecutive_stalls = [
            778.5982,
            770.1922,
            1238.3559,
            1250.9203,
            1238.6946,
            1260.9938,
            1232.4539,
            1251.3472,
            727.1835,
            755.3567,
        ]
        split_regimes = [
            757.897,
            739.761,
            750.972,
            740.019,
            738.420,
            977.413,
            1255.833,
            1271.877,
            1266.763,
            1232.285,
        ]
        uniformly_slow = [1200.0 + index for index in range(10)]
        fast_outliers = [
            3.975,
            3.823,
            3.531,
            4.051,
            3.416,
            3.760,
            2.489,
            2.777,
            3.741,
            3.919,
        ]
        single_slow_outlier = [
            809.1277,
            805.7773,
            1399.4701,
            772.9118,
            784.0111,
            742.691,
            760.8222,
            762.4693,
            746.5348,
            733.4866,
        ]
        for name, samples, error_pattern in (
            ("host-stall", unstable, r"p25.*idle host"),
            ("pooled-runner-stall", pooled_runner_stall, r"p25.*idle host"),
            (
                "minority-fast-path",
                minority_fast_path,
                r"split 2/8.*59\.8% gap.*idle host",
            ),
            ("consecutive-stalls", consecutive_stalls, r"60\.4% above p25.*idle host"),
            ("split-regimes", split_regimes, r"split 5/5.*29\.0% gap.*idle host"),
            ("uniform-slowdown", uniformly_slow, None),
            ("two-fast-outliers", fast_outliers, None),
            ("single-slow-outlier", single_slow_outlier, None),
        ):
            with self.subTest(name=name):
                document = lifecycle_document("whp")
                capture = cast(
                    dict[str, object],
                    cast(dict[str, object], document["snapshot_capture"])["whp"],
                )
                capture.update(
                    samples_ms=samples,
                    p50_ms=statistics.median(samples),
                    min_ms=min(samples),
                    max_ms=max(samples),
                )
                with tempfile.TemporaryDirectory() as temporary:
                    source = Path(temporary) / "acceptance.json"
                    source.write_text(json.dumps(document), encoding="utf-8")
                    if error_pattern is not None:
                        with self.assertRaisesRegex(
                            performance.UnstablePerformanceError,
                            rf"unstable snapshot generation.*{error_pattern}",
                        ):
                            performance.read_lifecycle_data(
                                "windows-whp-virtual-machine", source
                            )
                    else:
                        performance.read_lifecycle_data(
                            "windows-whp-virtual-machine", source
                        )

    def test_unstable_snapshot_generation_uses_temporary_failure_exit_code(self):
        samples = [
            736.2791,
            753.5391,
            780.7033,
            753.063,
            1272.4222,
            1260.2828,
            1271.4175,
            1297.1824,
            1233.186,
            747.7988,
        ]
        document = lifecycle_document("whp")
        capture = cast(
            dict[str, object],
            cast(dict[str, object], document["snapshot_capture"])["whp"],
        )
        capture.update(
            samples_ms=samples,
            p50_ms=statistics.median(samples),
            min_ms=min(samples),
            max_ms=max(samples),
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "acceptance.json"
            source.write_text(json.dumps(document), encoding="utf-8")
            stderr = io.StringIO()

            with contextlib.redirect_stderr(stderr):
                status = nvx.main(
                    [
                        "performance",
                        "validate-openvmm",
                        "--platform",
                        "windows-whp-virtual-machine",
                        "--input",
                        str(source),
                    ]
                )

        self.assertEqual(status, performance.UNSTABLE_EXIT_CODE)
        self.assertEqual(status, 75)
        self.assertIn("unstable snapshot generation", stderr.getvalue())

    def test_invalid_openvmm_result_uses_regular_failure_exit_code(self):
        document = lifecycle_document("whp")
        controls = cast(dict[str, object], document["controls"])
        controls["backend"] = "kvm"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "acceptance.json"
            source.write_text(json.dumps(document), encoding="utf-8")
            stderr = io.StringIO()

            with contextlib.redirect_stderr(stderr):
                status = nvx.main(
                    [
                        "performance",
                        "validate-openvmm",
                        "--platform",
                        "windows-whp-virtual-machine",
                        "--input",
                        str(source),
                    ]
                )

        self.assertEqual(status, 2)
        self.assertIn("does not match platform", stderr.getvalue())

    def test_restore_samples_come_from_the_section_of_their_series(self):
        # Each series' sample lines precede its section, inside the previous
        # one; only the listed samples belong to a section.
        log = shell_snapshot_log(
            {
                128: STABLE_RESTORE_MS,
                256: STALLED_RESTORE_MS,
                512: LATE_RECOVERY_RESTORE_MS,
            }
        )

        self.assertEqual(
            performance._shell_snapshot_restore_samples(log, "shell-snapshot.log"),
            {
                128: STABLE_RESTORE_MS,
                256: STALLED_RESTORE_MS,
                512: LATE_RECOVERY_RESTORE_MS,
            },
        )
        # Logs that passed through Windows tools keep CRLF line ends.
        self.assertEqual(
            performance._shell_snapshot_restore_samples(
                log.replace("\n", "\r\n"), "shell-snapshot.log"
            )[256],
            STALLED_RESTORE_MS,
        )
        self.assertEqual(
            performance._parse_shell_snapshot(log)["shell_snapshot_restore_256_mib"],
            ("ms", "lower", 58.7),
        )
        self.assertEqual(
            performance._shell_snapshot_restore_samples(
                SHELL_SNAPSHOT_RESTORE_LOG, "shell-snapshot-restore.log"
            ),
            {512: [6.8, 6.9, 7.0, 7.1, 7.2]},
        )

    def test_validate_workload_rejects_restore_series_that_a_stall_split(self):
        cases = (
            (
                "stalled-first-half",
                "linux-mshv-virtual-machine",
                "shell-snapshot.log",
                STALLED_RESTORE_MS,
                r"unstable shell-snapshot restore at \S*shell-snapshot\.log "
                r"\(256 MiB\): p50 58\.651 ms is 58\.0% above p25 37\.112 ms "
                r"\(limit 25\.0%\); rerun on an idle host",
            ),
            (
                "late-recovery",
                "windows-whp-virtual-machine",
                "shell-snapshot-restore.log",
                LATE_RECOVERY_RESTORE_MS,
                r"unstable shell-snapshot restore at \S*shell-snapshot-restore\.log "
                r"\(256 MiB\): samples split 2/8 between 44\.000 ms and "
                r"62\.600 ms \(42\.3% gap, limit 25\.0%\)",
            ),
            (
                "bimodal-on-mshv",
                "linux-mshv-baremetal",
                "shell-snapshot-restore.log",
                BIMODAL_KVM_RESTORE_MS,
                r"\(256 MiB\): p50 56\.550 ms is 96\.4% above p25 28\.800 ms",
            ),
            (
                "stable",
                "windows-whp-baremetal",
                "shell-snapshot.log",
                STABLE_RESTORE_MS,
                None,
            ),
            # KVM's restore series aren't guarded, stalled or not.
            (
                "stalled-on-kvm",
                "linux-kvm-virtual-machine",
                "shell-snapshot.log",
                STALLED_RESTORE_MS,
                None,
            ),
            (
                "bimodal-on-kvm",
                "linux-kvm-virtual-machine",
                "shell-snapshot-restore.log",
                BIMODAL_KVM_RESTORE_MS,
                None,
            ),
        )
        for name, platform, filename, restored, error_pattern in cases:
            with (
                self.subTest(name=name),
                tempfile.TemporaryDirectory() as temporary,
                contextlib.redirect_stdout(io.StringIO()) as stdout,
            ):
                logs = Path(temporary)
                render = (
                    shell_snapshot_log
                    if filename == "shell-snapshot.log"
                    else shell_snapshot_restore_log
                )
                (logs / filename).write_text(
                    render({128: STABLE_RESTORE_MS, 256: restored}), encoding="utf-8"
                )
                if error_pattern is None:
                    performance.validate_workload_stability(platform, logs)
                    if platform.startswith("linux-kvm"):
                        self.assertIn("SKIP:", stdout.getvalue())
                    else:
                        self.assertIn(
                            "are stable (2 with at least 10 samples checked)",
                            stdout.getvalue(),
                        )
                else:
                    with self.assertRaisesRegex(
                        performance.UnstablePerformanceError, error_pattern
                    ):
                        performance.validate_workload_stability(platform, logs)

    def test_validate_workload_reports_every_unstable_restore_series(self):
        with tempfile.TemporaryDirectory() as temporary:
            logs = Path(temporary)
            (logs / "shell-snapshot.log").write_text(
                shell_snapshot_log(
                    {
                        128: STABLE_RESTORE_MS,
                        256: STALLED_RESTORE_MS,
                        512: LATE_RECOVERY_RESTORE_MS,
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaises(performance.UnstablePerformanceError) as context:
                performance.validate_workload_stability(
                    "linux-mshv-virtual-machine", logs
                )

        errors = str(context.exception).splitlines()
        self.assertEqual(len(errors), 2)
        self.assertIn("(256 MiB): p50 58.651 ms", errors[0])
        self.assertIn("(512 MiB): samples split 2/8", errors[1])

    def test_restore_guard_rejects_invalid_logs(self):
        stable = shell_snapshot_restore_log({512: STABLE_RESTORE_MS})
        samples_line = benchmark._format_restore_samples_line(STABLE_RESTORE_MS)
        cases = (
            (
                "unlisted",
                stable.replace(samples_line + "\n", ""),
                r"missing restore samples in \S*shell-snapshot-restore\.log "
                r"\(512 MiB\)",
            ),
            # A log cut off before its first section has no series to accept.
            (
                "sectionless",
                stable.split("== 512 MiB ==", maxsplit=1)[0],
                r"missing memory sections in \S*shell-snapshot-restore\.log",
            ),
            (
                "zero",
                shell_snapshot_restore_log({512: [0.0] * 10}),
                r"non-positive restore sample in \S*shell-snapshot-restore\.log "
                r"\(512 MiB\)",
            ),
            (
                "short",
                stable.replace(samples_line, samples_line.replace("37.200, ", "")),
                r"lists 9 restore samples, but its summary counts 10",
            ),
            (
                "other-series",
                stable.replace(
                    samples_line,
                    benchmark._format_restore_samples_line(STALLED_RESTORE_MS),
                ),
                r"lists restore samples with median 58\.651 ms, but its summary "
                r"reports 36\.5 ms",
            ),
            (
                "malformed",
                stable.replace(samples_line, samples_line.replace("37.100", "slow")),
                r"malformed restore samples",
            ),
            (
                "unsummarized",
                stable.replace("snapshot restore    : median", "restore median"),
                r"missing snapshot restore summary",
            ),
        )
        for name, log, error_pattern in cases:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                logs = Path(temporary)
                (logs / "shell-snapshot-restore.log").write_text(log, encoding="utf-8")

                with self.assertRaisesRegex(
                    performance.PerformanceError, error_pattern
                ) as context:
                    performance.validate_workload_stability(
                        "linux-mshv-virtual-machine", logs
                    )
                # Malformed input is invalid, never a reason to remeasure.
                self.assertNotIsInstance(
                    context.exception, performance.UnstablePerformanceError
                )

        with tempfile.TemporaryDirectory() as temporary:
            logs = Path(temporary)
            with self.assertRaisesRegex(
                performance.PerformanceError, r"no shell-snapshot restore log found"
            ):
                performance.validate_workload_stability(
                    "linux-mshv-virtual-machine", logs
                )
            (logs / "shell-snapshot.log").write_text(
                SHELL_SNAPSHOT_LOG, encoding="utf-8"
            )
            with self.assertRaisesRegex(
                performance.PerformanceError, r"unsupported OpenVMM benchmark platform"
            ):
                performance.validate_workload_stability("linux-unknown", logs)
            # Fewer samples than the guard needs may go unlisted.
            unlisted = re.sub(r"(?m)^ *restore samples .*\n", "", SHELL_SNAPSHOT_LOG)
            (logs / "shell-snapshot.log").write_text(unlisted, encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()) as stdout:
                performance.validate_workload_stability(
                    "linux-mshv-virtual-machine", logs
                )
            self.assertIn("(0 with at least 10 samples checked)", stdout.getvalue())

    def test_validate_workload_uses_temporary_failure_exit_code(self):
        unlisted = re.sub(
            r"(?m)^ *restore samples .*\n",
            "",
            shell_snapshot_restore_log({512: STABLE_RESTORE_MS}),
        )
        for name, log, expected_status, expected_output in (
            (
                "stable",
                shell_snapshot_restore_log({512: STABLE_RESTORE_MS}),
                0,
                "are stable",
            ),
            (
                "unstable",
                shell_snapshot_restore_log({512: STALLED_RESTORE_MS}),
                75,
                "ERROR: unstable shell-snapshot restore",
            ),
            ("invalid", unlisted, 2, "ERROR: missing restore samples"),
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                logs = Path(temporary)
                (logs / "shell-snapshot-restore.log").write_text(log, encoding="utf-8")
                stdout, stderr = io.StringIO(), io.StringIO()

                with (
                    contextlib.redirect_stdout(stdout),
                    contextlib.redirect_stderr(stderr),
                ):
                    status = nvx.main(
                        [
                            "performance",
                            "validate-workload",
                            "--platform",
                            "windows-whp-virtual-machine",
                            "--input-dir",
                            str(logs),
                        ]
                    )

                self.assertEqual(status, expected_status)
                self.assertIn(expected_output, stdout.getvalue() + stderr.getvalue())

    def test_collection_rejects_unstable_restore_series_on_guarded_backends(self):
        for platform, guarded in (
            ("linux-mshv-virtual-machine", True),
            ("windows-whp-virtual-machine", True),
            ("linux-kvm-virtual-machine", False),
        ):
            backend = performance.OPENVMM_BACKENDS[platform]
            # The one-vCPU suite and the higher-vCPU restore-only suite.
            for processors in (1, 8):
                with (
                    self.subTest(platform=platform, processors=processors),
                    tempfile.TemporaryDirectory() as temporary,
                    contextlib.redirect_stdout(io.StringIO()),
                ):
                    root = Path(temporary)
                    logs = root / "logs"
                    logs.mkdir()
                    if processors == 1:
                        (logs / "cold-start.log").write_text(
                            COLD_START_LOG, encoding="utf-8"
                        )
                        (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
                        (logs / "shell-snapshot.log").write_text(
                            shell_snapshot_log(
                                {
                                    128: STABLE_RESTORE_MS,
                                    256: STALLED_RESTORE_MS,
                                    512: STABLE_RESTORE_MS,
                                }
                            ),
                            encoding="utf-8",
                        )
                        options: dict[str, Any] = {"require_shell_snapshot": True}
                    else:
                        (logs / "shell-snapshot-restore.log").write_text(
                            shell_snapshot_restore_log({512: STALLED_RESTORE_MS}),
                            encoding="utf-8",
                        )
                        (logs / performance.BENCHMARK_METADATA_FILENAME).write_text(
                            json.dumps(
                                {
                                    "platform": platform,
                                    "backend": backend,
                                    "microvm_abi_version": 2,
                                    "processors": 8,
                                    "warmups": 1,
                                    "measured_runs": 10,
                                    "shell_memories_mib": [512],
                                }
                            ),
                            encoding="utf-8",
                        )
                        options = {"require_shell_snapshot_restore_512": True}
                    results = root / "results"

                    if guarded:
                        with self.assertRaisesRegex(
                            performance.UnstablePerformanceError,
                            r"unstable shell-snapshot restore .* 58\.0% above p25",
                        ):
                            performance.collect_results(
                                platform, "abc123", logs, results, **options
                            )
                        self.assertFalse(results.exists())
                    else:
                        result_path = performance.collect_results(
                            platform, "abc123", logs, results, **options
                        )
                        stalled = (
                            "shell_snapshot_restore_256_mib"
                            if processors == 1
                            else "shell_snapshot_restore_512_mib"
                        )
                        self.assertIn(
                            (stalled, 58.7),
                            [
                                (result.metric, result.p50)
                                for result in performance.read_results(result_path)
                            ],
                        )

    def test_validate_openvmm_records_every_attempt_outcome(self):
        invalid = lifecycle_document("whp")
        cast(dict[str, object], invalid["controls"])["backend"] = "kvm"
        documents = (
            profiled_lifecycle_document(MINORITY_FAST_PATH_MS),
            invalid,
            profiled_lifecycle_document(),
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            log = root / "acceptance-attempts.json"
            statuses: list[int] = []
            digests: list[str] = []
            for index, document in enumerate(documents, 1):
                source = root / f"acceptance-attempt-{index}.json"
                source.write_text(json.dumps(document), encoding="utf-8")
                digests.append(sha256_file(source))
                statuses.append(validate_attempt(source, log))
            recorded = json.loads(log.read_text(encoding="utf-8"))

        self.assertEqual(statuses, [75, 2, 0])
        self.assertEqual(recorded["schema_version"], 1)
        self.assertEqual(recorded["platform"], "windows-whp-virtual-machine")
        attempts = recorded["attempts"]
        self.assertEqual(
            [
                (attempt["attempt"], attempt["file"], attempt["sha256"])
                for attempt in attempts
            ],
            [
                (index, f"acceptance-attempt-{index}.json", digest)
                for index, digest in enumerate(digests, 1)
            ],
        )
        self.assertEqual(
            [(attempt["status"], attempt["exit_status"]) for attempt in attempts],
            [("unstable", 75), ("invalid", 2), ("valid", 0)],
        )
        self.assertRegex(attempts[0]["message"], r"split 2/8.*59\.8% gap")
        self.assertIn("does not match platform", attempts[1]["message"])
        self.assertIsNone(attempts[2]["message"])

    def test_collect_names_the_published_attempt_and_reports_every_attempt(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            # The workload records the host that the lifecycle attempts record.
            (logs / performance.BENCHMARK_METADATA_FILENAME).write_text(
                json.dumps(
                    {
                        "backend": "whp",
                        "memory_mib": {"lifecycle": 128},
                        "host": BENCHMARK_HOST,
                    }
                ),
                encoding="utf-8",
            )
            log = root / "acceptance-attempts.json"
            for index, document in enumerate(
                (
                    profiled_lifecycle_document(MINORITY_FAST_PATH_MS),
                    profiled_lifecycle_document(),
                ),
                1,
            ):
                source = root / f"acceptance-attempt-{index}.json"
                source.write_text(json.dumps(document), encoding="utf-8")
                validate_attempt(source, log)
            published = root / "acceptance.json"
            published.write_bytes((root / "acceptance-attempt-2.json").read_bytes())
            summary = root / "summary.md"
            stdout = io.StringIO()

            with contextlib.redirect_stdout(stdout):
                result_path = performance.collect_results(
                    "windows-whp-virtual-machine",
                    "abc123",
                    logs,
                    root / "results",
                    summary_path=summary,
                    lifecycle_input=published,
                    lifecycle_attempt_log=log,
                )
            markdown = summary.read_text(encoding="utf-8")

            self.assertLessEqual(
                performance.LIFECYCLE_METRICS,
                {result.metric for result in performance.read_results(result_path)},
            )
        self.assertIn(
            "Lifecycle metrics come from attempt 2 of 2: acceptance-attempt-2.json",
            stdout.getvalue(),
        )
        self.assertIn("### Lifecycle attempts", markdown)
        self.assertRegex(
            markdown,
            r"\| 1 \| `acceptance-attempt-1\.json` \| unstable: unstable snapshot "
            r"generation at .*split 2/8.* \| 1241\.54 ms \| no \|",
        )
        self.assertIn(
            "| 2 | `acceptance-attempt-2.json` | valid | 31.00 ms | yes |", markdown
        )
        self.assertIn("### Benchmark host", markdown)
        self.assertIn("| Runner | azure-windows-3 |", markdown)
        self.assertIn("| Azure VM size | Standard_D16ds_v5 |", markdown)
        self.assertIn(
            "| Azure OS disk | Premium_LRS, 128 GiB, ReadWrite caching |", markdown
        )
        self.assertIn("### Snapshot generation storage telemetry", markdown)
        self.assertIn(
            "#### Attempt 1: unstable (`acceptance-attempt-1.json`)", markdown
        )
        self.assertIn(
            "#### Attempt 2: valid, published (`acceptance-attempt-2.json`)", markdown
        )
        self.assertIn("Scratch volume: `F:`.", markdown)
        self.assertIn(
            "| 1 | 763.35 ms | 743.35 ms | 160.0 MiB/s | 31.25 ms | 3.9 | 99% | "
            "0.2 MiB/s | 6% | 12.3 MiB | 0.0 MiB/s | 0.0% |",
            markdown,
        )
        self.assertIn(
            "| 2 | 1219.94 ms | 1199.94 ms | 96.0 MiB/s | 31.25 ms | 3.9 | 99% | ",
            markdown,
        )
        self.assertEqual(markdown.count("| Sample | Generation | Flush |"), 2)

    def test_collect_rejects_a_lifecycle_input_that_no_valid_attempt_supplied(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            log = root / "acceptance-attempts.json"
            unstable = root / "acceptance-attempt-1.json"
            document = profiled_lifecycle_document(MINORITY_FAST_PATH_MS)
            unstable.write_text(json.dumps(document), encoding="utf-8")
            validate_attempt(unstable, log)
            published = root / "acceptance.json"
            published.write_text(
                json.dumps(profiled_lifecycle_document()), encoding="utf-8"
            )
            results = root / "results"

            with self.assertRaisesRegex(
                performance.PerformanceError, "matches 0 valid attempts"
            ):
                performance.collect_results(
                    "windows-whp-virtual-machine",
                    "abc123",
                    logs,
                    results,
                    lifecycle_input=published,
                    lifecycle_attempt_log=log,
                )
            self.assertFalse(results.exists())
            with self.assertRaisesRegex(
                performance.PerformanceError, "requires a lifecycle input"
            ):
                performance.collect_results(
                    "windows-whp-virtual-machine",
                    "abc123",
                    logs,
                    results,
                    lifecycle_attempt_log=log,
                )

    def test_attempt_log_rejects_malformed_entries(self):
        valid = {
            "attempt": 1,
            "file": "acceptance-attempt-1.json",
            "sha256": "a" * 64,
            "status": "valid",
            "exit_status": 0,
            "message": None,
        }
        cases: dict[str, tuple[dict[str, object], str]] = {
            "platform": (
                {"schema_version": 1, "platform": "linux-kvm", "attempts": [valid]},
                "records platform 'linux-kvm'",
            ),
            "schema": (
                {
                    "schema_version": 2,
                    "platform": "windows-whp-virtual-machine",
                    "attempts": [valid],
                },
                "unsupported schema_version 2",
            ),
            "status": (
                {
                    "schema_version": 1,
                    "platform": "windows-whp-virtual-machine",
                    "attempts": [{**valid, "exit_status": 75}],
                },
                r"invalid lifecycle attempt at .*attempts\[0\]",
            ),
            "order": (
                {
                    "schema_version": 1,
                    "platform": "windows-whp-virtual-machine",
                    "attempts": [{**valid, "attempt": 2}],
                },
                r"invalid lifecycle attempt at .*attempts\[0\]",
            ),
            "digest": (
                {
                    "schema_version": 1,
                    "platform": "windows-whp-virtual-machine",
                    "attempts": [{**valid, "sha256": "not-a-digest"}],
                },
                r"invalid lifecycle attempt at .*attempts\[0\]",
            ),
        }
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "acceptance-attempts.json"
            for name, (document, error) in cases.items():
                with self.subTest(name=name):
                    log.write_text(json.dumps(document), encoding="utf-8")
                    with self.assertRaisesRegex(performance.PerformanceError, error):
                        performance.read_lifecycle_attempt_log(
                            log, "windows-whp-virtual-machine"
                        )

    def test_lifecycle_diagnostics_report_host_and_telemetry_without_a_log(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "acceptance.json"
            source.write_text(
                json.dumps(profiled_lifecycle_document()), encoding="utf-8"
            )
            summary = root / "summary.md"

            performance.collect_openvmm_results(
                "windows-whp-virtual-machine",
                "abc123",
                source,
                root / "results",
                summary,
            )
            markdown = summary.read_text(encoding="utf-8")

        self.assertNotIn("### Lifecycle attempts", markdown)
        self.assertIn("| Machine | VMSSVAOAN000002 |", markdown)
        self.assertIn("### Snapshot generation storage telemetry", markdown)
        self.assertNotIn("#### Attempt", markdown)
        self.assertEqual(markdown.count("| 160.0 MiB/s |"), 10)

    def test_collect_cli_accepts_lifecycle_attempt_logs(self):
        collect = nvx.parse_args(
            [
                "performance",
                "collect",
                "--platform",
                "windows-whp-virtual-machine",
                "--commit",
                "abc123",
                "--input-dir",
                "logs",
                "--output-dir",
                "results",
                "--lifecycle-input",
                "acceptance.json",
                "--lifecycle-attempt-log",
                "acceptance-attempts.json",
            ]
        )
        validate = nvx.parse_args(
            [
                "performance",
                "validate-openvmm",
                "--platform",
                "windows-whp-virtual-machine",
                "--input",
                "acceptance-attempt-1.json",
                "--attempt-log",
                "acceptance-attempts.json",
            ]
        )

        self.assertEqual(
            collect.lifecycle_attempt_log, Path("acceptance-attempts.json")
        )
        self.assertEqual(validate.attempt_log, Path("acceptance-attempts.json"))

    def test_collect_appends_ci_benchmark_table(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            summary = root / "summary.md"
            logs.mkdir()
            summary.write_text("Existing summary", encoding="utf-8")
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "shell-snapshot.log").write_text(
                SHELL_SNAPSHOT_LOG, encoding="utf-8"
            )
            (logs / "network.log").write_text(NETWORK_LOG, encoding="utf-8")

            performance.collect_results(
                "windows-whp",
                "abc123",
                logs,
                root / "results",
                require_network=True,
                require_shell_snapshot=True,
                require_shared_suite=True,
                summary_path=summary,
            )

            markdown = summary.read_text(encoding="utf-8")
            self.assertTrue(markdown.startswith("Existing summary\n##"))
            self.assertIn("## Windows / WHP benchmark results", markdown)
            self.assertIn("| Metric | p50 | Preferred direction |", markdown)
            self.assertIn(
                "| `network_snapshot_restore_wall` | 50.00 ms | Lower is better |",
                markdown,
            )
            self.assertIn("Commit: `abc123`", markdown)

    def test_live_virtfs_roundtrip_uses_shared_metric_name(self):
        self.assertEqual(
            performance._platform_metric_name("windows-whp", "virtfs_live_roundtrip"),
            "virtfs_live_roundtrip",
        )
        self.assertEqual(
            performance._platform_metric_name("linux-kvm", "virtfs_live_roundtrip"),
            "virtfs_live_roundtrip",
        )

    def test_device_io_parser_filters_warmups_and_retains_failures(self):
        metrics = performance._parse_device_io(DEVICE_IO_LOG)

        self.assertEqual(
            metrics,
            {
                "virtio_blk_random_read_iops": ("ops/s", "higher", 200.0),
                "virtio_blk_random_write_iops": ("ops/s", "higher", 140.0),
                "virtio_fs_random_read_iops": ("ops/s", "higher", 300.0),
                "virtio_fs_random_write_iops": ("ops/s", "higher", 200.0),
                "virtio_net_udp_roundtrip_ops": ("ops/s", "higher", 150.0),
            },
        )

    def test_device_io_parser_rejects_duplicate_attempts(self):
        duplicate = DEVICE_IO_LOG + DEVICE_IO_LOG.splitlines()[2] + "\n"

        with self.assertRaisesRegex(
            performance.PerformanceError,
            r"duplicate retained virtio-blk sample 0",
        ):
            performance._parse_device_io(duplicate)

    def test_device_io_parser_rejects_missing_and_zero_results(self):
        missing = device_io_log().replace(
            ',{"device":"virtio-blk","operation":"write","operations":100,"bytes_per_operation":4096,"elapsed_ns":1000000000}',
            "",
            1,
        )
        with self.assertRaisesRegex(
            performance.PerformanceError, r"missing virtio-blk"
        ):
            performance._parse_device_io(missing)

        zero = device_io_log().replace('"operations":100', '"operations":0', 1)
        with self.assertRaisesRegex(performance.PerformanceError, r"nonzero work"):
            performance._parse_device_io(zero)

    def test_device_io_parser_requires_every_configured_attempt(self):
        incomplete = "\n".join(device_io_log(warmups=1, runs=2).splitlines()[:-1])

        with self.assertRaisesRegex(
            performance.PerformanceError,
            r"attempt coverage mismatch.*virtio-net",
        ):
            performance._parse_device_io(
                incomplete,
                expected_warmups=1,
                expected_runs=2,
            )

    def test_collect_skips_stale_device_io_log_for_other_suites(self):
        # Regression test for https://github.com/microsoft/nvx/issues/499:
        # a device-io.log left over from an earlier device-io run must not
        # contaminate collection of a different suite sharing the output dir.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "device-io.log").write_text(
                device_io_log(warmups=1, runs=2), encoding="utf-8"
            )
            (logs / performance.BENCHMARK_METADATA_FILENAME).write_text(
                json.dumps(
                    {
                        "suite": "performance",
                        "platform": "linux-mshv-baremetal",
                        "backend": "mshv",
                        "microvm_abi_version": 1,
                        "processors": 1,
                        "warmups": 1,
                        "measured_runs": 2,
                    }
                ),
                encoding="utf-8",
            )

            result_path = performance.collect_results(
                "linux-mshv-baremetal",
                "abc123",
                logs,
                root / "results",
            )
            results = performance.read_results(result_path)

            self.assertFalse(
                {result.metric for result in results}
                & set(performance.DEVICE_IO_METRIC_NAMES),
                "stale device-io metrics leaked into the performance suite results",
            )

    def test_collects_device_io_metrics_with_abi_dimension(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "device-io.log").write_text(
                device_io_log(warmups=1, runs=2), encoding="utf-8"
            )
            (logs / performance.BENCHMARK_METADATA_FILENAME).write_text(
                json.dumps(
                    {
                        "suite": "device-io",
                        "platform": "linux-mshv-baremetal",
                        "backend": "mshv",
                        "microvm_abi_version": 1,
                        "processors": 1,
                        "warmups": 1,
                        "measured_runs": 2,
                    }
                ),
                encoding="utf-8",
            )

            result_path = performance.collect_results(
                "linux-mshv-baremetal",
                "abc123",
                logs,
                root / "results",
            )
            results = performance.read_results(result_path)

            self.assertEqual(result_path.name, "linux-mshv-baremetal.csv")
            self.assertEqual(
                {result.metric for result in results},
                set(performance.DEVICE_IO_METRIC_NAMES),
            )
            self.assertEqual(len(results), 5)
            self.assertTrue(all(result.unit == "ops/s" for result in results))
            self.assertTrue(all(result.direction == "higher" for result in results))

    def test_collects_linux_metrics_from_utf8_and_utf16_logs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            output = root / "results"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "network.log").write_text(NETWORK_LOG, encoding="utf-16")
            (logs / "shell-snapshot.log").write_text(
                SHELL_SNAPSHOT_LOG, encoding="utf-8"
            )
            lifecycle = root / "acceptance.json"
            lifecycle.write_text(
                json.dumps(lifecycle_document()),
                encoding="utf-8",
            )

            result_path = performance.collect_results(
                "linux-kvm",
                "abc123",
                logs,
                output,
                require_network=True,
                require_shell_snapshot=True,
                require_shared_suite=True,
                summary_path=root / "summary.md",
                lifecycle_input=lifecycle,
            )
            results = performance.read_results(result_path)

            self.assertEqual(len(results), 29)
            by_metric = {result.metric: result for result in results}
            self.assertEqual(by_metric["cold_start_base"].p50, 101.0)
            self.assertEqual(by_metric["cold_start_cryptomgr_notests"].p50, 109.0)
            self.assertEqual(by_metric["virtfs_live_read"].p50, 1200.0)
            self.assertEqual(by_metric["virtfs_live_read"].direction, "higher")
            self.assertEqual(by_metric["network_snapshot_restore"].p50, 40.0)
            self.assertEqual(by_metric["shell_snapshot_cold_128_mib"].p50, 520.0)
            self.assertEqual(
                by_metric["shell_snapshot_cold_128_mib"].direction,
                "lower",
            )
            self.assertEqual(by_metric["shell_snapshot_restore_512_mib"].p50, 7.0)
            self.assertEqual(by_metric["openvmm_snapshot_generation"].p50, 31.0)
            self.assertEqual(
                by_metric["openvmm_snapshot_restore_peak_rss"].p50,
                32.0,
            )
            markdown = (root / "summary.md").read_text(encoding="utf-8")
            self.assertIn("## Linux / KVM benchmark results", markdown)
            self.assertEqual(markdown.count("\n| `"), 29)
            self.assertIn(
                "| `virtfs_live_read` | 1200.00 MB/s | Higher is better |", markdown
            )
            self.assertIn(
                "| `network_snapshot_restore` | 40.00 ms | Lower is better |", markdown
            )
            self.assertIn("## Linux / KVM lifecycle diagnostics", markdown)

    def test_collects_mshv_shared_suite_with_network(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "shell-snapshot.log").write_text(
                SHELL_SNAPSHOT_LOG, encoding="utf-8"
            )
            (logs / "network.log").write_text(NETWORK_LOG, encoding="utf-8")

            result_path = performance.collect_results(
                "linux-mshv",
                "abc123",
                logs,
                root / "results",
                require_network=True,
                require_shell_snapshot=True,
                require_shared_suite=True,
            )
            results = performance.read_results(result_path)

            self.assertEqual(len(results), 21)
            self.assertIn(
                "network_snapshot_restore",
                {result.metric for result in results},
            )

    def test_shared_suite_rejects_missing_scenarios(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "shell-snapshot.log").write_text(
                SHELL_SNAPSHOT_LOG, encoding="utf-8"
            )

            with self.assertRaisesRegex(
                performance.PerformanceError,
                r"exactly 21 metrics \(missing: network_snapshot_cold",
            ):
                performance.collect_results(
                    "linux-kvm",
                    "abc123",
                    logs,
                    root / "results",
                    require_shell_snapshot=True,
                    require_shared_suite=True,
                )

    def test_shell_snapshot_requires_every_memory_size(self):
        incomplete_log = SHELL_SNAPSHOT_LOG.split("== 512 MiB ==", maxsplit=1)[0]
        with self.assertRaisesRegex(
            performance.PerformanceError,
            r"missing memory section\(s\).*512 MiB",
        ):
            performance._parse_shell_snapshot(incomplete_log)

    def test_collects_only_canonical_higher_vcpu_shell_restore(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "shell-snapshot-restore.log").write_text(
                SHELL_SNAPSHOT_RESTORE_LOG, encoding="utf-8"
            )
            (logs / performance.BENCHMARK_METADATA_FILENAME).write_text(
                json.dumps(
                    {
                        "platform": "linux-kvm-baremetal",
                        "backend": "kvm",
                        "microvm_abi_version": 2,
                        "processors": 8,
                        "warmups": 1,
                        "measured_runs": 10,
                        "shell_memories_mib": [512],
                    }
                ),
                encoding="utf-8",
            )

            result_path = performance.collect_results(
                "linux-kvm-baremetal",
                "abc123",
                logs,
                root / "results",
                require_shell_snapshot_restore_512=True,
            )

            self.assertEqual(
                performance.read_results(result_path),
                [
                    performance.Result(
                        "abc123",
                        "shell_snapshot_restore_512_mib",
                        "ms",
                        "lower",
                        7.0,
                        "linux-kvm-baremetal",
                        2,
                        8,
                    )
                ],
            )

    def test_restore_only_collection_rejects_other_metrics(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "shell-snapshot-restore.log").write_text(
                SHELL_SNAPSHOT_RESTORE_LOG, encoding="utf-8"
            )
            (logs / performance.BENCHMARK_METADATA_FILENAME).write_text(
                json.dumps(
                    {
                        "platform": "linux-kvm-baremetal",
                        "backend": "kvm",
                        "microvm_abi_version": 2,
                        "processors": 8,
                        "warmups": 1,
                        "measured_runs": 10,
                        "shell_memories_mib": [512],
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                performance.PerformanceError,
                r"unexpected: cold_start_base",
            ):
                performance.collect_results(
                    "linux-kvm-baremetal",
                    "abc123",
                    logs,
                    root / "results",
                    require_shell_snapshot_restore_512=True,
                )

    def test_collect_requires_shell_snapshot_log_when_requested(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")

            with self.assertRaisesRegex(
                performance.PerformanceError,
                r"required benchmark log not found: .*shell-snapshot\.log",
            ):
                performance.collect_results(
                    "linux-kvm",
                    "abc123",
                    logs,
                    root / "results",
                    require_shell_snapshot=True,
                )

    def test_collect_does_not_require_python_snapshot_logs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")

            result_path = performance.collect_results(
                "linux-kvm", "abc123", logs, root / "results"
            )

            self.assertEqual(len(performance.read_results(result_path)), 12)

    def test_gate_uses_latest_ten_p50_values_and_both_directions(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline"
            target = root / "target"
            summary = root / "summary.md"
            old = [
                performance.Result("old", "latency", "ms", "lower", 1000.0),
                performance.Result("old", "throughput", "MB/s", "higher", 1.0),
            ]
            recent: list[performance.Result] = []
            for index in range(10):
                recent.extend(
                    [
                        performance.Result(
                            f"base-{index}", "latency", "ms", "lower", 100.0
                        ),
                        performance.Result(
                            f"base-{index}",
                            "throughput",
                            "MB/s",
                            "higher",
                            100.0,
                        ),
                    ]
                )
            performance.write_results(baseline / "linux-kvm.csv", [*old, *recent])
            performance.write_results(
                target / "linux-kvm.csv",
                [
                    performance.Result("pr", "latency", "ms", "lower", 140.0),
                    performance.Result("pr", "throughput", "MB/s", "higher", 60.0),
                ],
            )

            self.assertEqual(
                performance.gate_results(baseline, target, 10, 40.0, summary), 0
            )
            self.assertIn("(10/10)", summary.read_text(encoding="utf-8"))

            performance.write_results(
                target / "linux-kvm.csv",
                [
                    performance.Result("pr", "latency", "ms", "lower", 141.0),
                    performance.Result("pr", "throughput", "MB/s", "higher", 59.0),
                ],
            )
            self.assertEqual(performance.gate_results(baseline, target, 10, 40.0), 1)

    def test_gate_uses_baseline_median_for_multimodal_history(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline"
            target = root / "target"
            performance.write_results(
                baseline / "linux-kvm.csv",
                [
                    performance.Result(
                        f"base-{index}",
                        "latency",
                        "ms",
                        "lower",
                        value,
                    )
                    for index, value in enumerate(
                        (1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 100.0, 100.0, 100.0, 100.0)
                    )
                ],
            )
            performance.write_results(
                target / "linux-kvm.csv",
                [performance.Result("pr", "latency", "ms", "lower", 10.0)],
            )

            self.assertEqual(
                performance.gate_results(baseline, target, 10, 40.0),
                1,
            )

    def test_gate_warms_up_until_minimum_history(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline"
            target = root / "target"
            summary = root / "summary.md"
            performance.write_results(
                baseline / "linux-kvm.csv",
                [
                    performance.Result(
                        f"base-{index}",
                        "latency",
                        "ms",
                        "lower",
                        10.0,
                    )
                    for index in range(9)
                ],
            )
            performance.write_results(
                target / "linux-kvm.csv",
                [performance.Result("pr", "latency", "ms", "lower", 100.0)],
            )

            self.assertEqual(
                performance.gate_results(baseline, target, 10, 40.0, summary),
                0,
            )
            self.assertIn(
                "Warmup (9/10)",
                summary.read_text(encoding="utf-8"),
            )

    def test_gate_requires_absolute_latency_regression_beyond_tolerance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline"
            target = root / "target"
            summary = root / "summary.md"
            performance.write_results(
                baseline / "linux-kvm.csv",
                [
                    performance.Result("base", "restore_latency", "ms", "lower", 7.0),
                    performance.Result("base", "throughput", "MB/s", "higher", 100.0),
                ],
            )
            performance.write_results(
                target / "linux-kvm.csv",
                [
                    performance.Result("pr", "restore_latency", "ms", "lower", 12.0),
                    performance.Result("pr", "throughput", "MB/s", "higher", 100.0),
                ],
            )

            self.assertEqual(
                performance.gate_results(
                    baseline,
                    target,
                    10,
                    40.0,
                    summary,
                    5.0,
                    minimum_history=1,
                ),
                0,
            )
            self.assertIn("+71.4%, +5.00 ms", summary.read_text(encoding="utf-8"))

            performance.write_results(
                target / "linux-kvm.csv",
                [
                    performance.Result("pr", "restore_latency", "ms", "lower", 12.1),
                    performance.Result("pr", "throughput", "MB/s", "higher", 100.0),
                ],
            )
            self.assertEqual(
                performance.gate_results(
                    baseline,
                    target,
                    10,
                    40.0,
                    absolute_tolerance_ms=5.0,
                    minimum_history=1,
                ),
                1,
            )

    def test_gate_does_not_apply_latency_tolerance_to_throughput(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline"
            target = root / "target"
            performance.write_results(
                baseline / "linux-kvm.csv",
                [performance.Result("base", "throughput", "MB/s", "higher", 10.0)],
            )
            performance.write_results(
                target / "linux-kvm.csv",
                [performance.Result("pr", "throughput", "MB/s", "higher", 5.0)],
            )

            self.assertEqual(
                performance.gate_results(
                    baseline,
                    target,
                    10,
                    40.0,
                    minimum_history=1,
                ),
                1,
            )

    def test_gate_applies_relative_threshold_to_peak_rss(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline"
            target = root / "target"
            performance.write_results(
                baseline / "linux-kvm.csv",
                [
                    performance.Result(
                        "base",
                        "openvmm_cold_start_peak_rss",
                        "MiB",
                        "lower",
                        100.0,
                    )
                ],
            )
            performance.write_results(
                target / "linux-kvm.csv",
                [
                    performance.Result(
                        "pr",
                        "openvmm_cold_start_peak_rss",
                        "MiB",
                        "lower",
                        151.0,
                    )
                ],
            )

            self.assertEqual(
                performance.gate_results(
                    baseline,
                    target,
                    10,
                    50.0,
                    absolute_tolerance_ms=1000.0,
                    minimum_history=1,
                ),
                1,
            )

    def test_gate_warms_up_metric_without_history(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline"
            target = root / "target"
            summary = root / "summary.md"
            performance.write_results(
                baseline / "linux-kvm.csv",
                [performance.Result("base", "other", "ms", "lower", 10.0)],
            )
            performance.write_results(
                target / "linux-kvm.csv",
                [
                    performance.Result(
                        "pr", "python_pandas_restore", "ms", "lower", 800.0
                    )
                ],
            )

            self.assertEqual(
                performance.gate_results(
                    baseline,
                    target,
                    10,
                    40.0,
                    summary,
                    minimum_history=4,
                ),
                0,
            )
            self.assertIn(
                "| linux-kvm | `python_pandas_restore` | 800.00 ms | - | - | Warmup |",
                summary.read_text(encoding="utf-8"),
            )

    def test_gate_honors_selective_history_reset_without_disabling_other_metrics(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline"
            target = root / "target"
            candidate = root / "candidate"
            summary = root / "summary.md"
            filename = "linux-mshv-baremetal-microvm-v2-1vcpu.csv"
            reset_metrics = (
                "openvmm_snapshot_generation",
                "openvmm_snapshot_restore_guest_exit_teardown",
            )
            metrics = (*reset_metrics, "openvmm_snapshot_restore")

            def result(commit: str, metric: str, p50: float) -> performance.Result:
                return performance.Result(
                    commit, metric, "ms", "lower", p50, "linux-mshv-baremetal", 2, 1
                )

            history = [
                result(f"base-{index}", metric, 10.0)
                for index in range(10)
                for metric in metrics
            ]
            performance.write_results(baseline / filename, history)
            performance.write_results(
                candidate / filename,
                [row for row in history if row.metric not in reset_metrics],
            )
            performance.write_results(
                target / filename, [result("pr", metric, 100.0) for metric in metrics]
            )

            self.assertEqual(
                nvx.main(
                    [
                        "performance",
                        "gate",
                        "--baseline-dir",
                        str(baseline),
                        "--target-dir",
                        str(target),
                        "--history-reset-dir",
                        str(candidate),
                        "--summary",
                        str(summary),
                    ]
                ),
                1,
            )
            markdown = summary.read_text(encoding="utf-8")
            self.assertEqual(markdown.count("Warmup (history reset)"), 2)
            self.assertIn("Checked 1 metric(s); found 1 regression(s).", markdown)

            performance.persist_results(target, candidate)
            self.assertEqual(
                performance.gate_results(
                    candidate, target, 10, 40, history_reset_dir=candidate
                ),
                1,
            )
            new_history = performance.read_results(candidate / filename)
            for metric in reset_metrics:
                self.assertEqual(
                    [row.commit for row in new_history if row.metric == metric], ["pr"]
                )

    def test_gate_missing_candidate_history_does_not_reset_baseline(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            performance.write_results(
                root / "baseline" / "linux-kvm.csv",
                [performance.Result("base", "latency", "ms", "lower", 10.0)],
            )
            performance.write_results(
                root / "target" / "linux-kvm.csv",
                [performance.Result("pr", "latency", "ms", "lower", 100.0)],
            )
            self.assertEqual(
                performance.gate_results(
                    root / "baseline",
                    root / "target",
                    10,
                    40,
                    minimum_history=1,
                    history_reset_dir=root / "missing",
                ),
                1,
            )

    def test_gate_uses_verified_windows_virtfs_reuse_history(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline"
            target = root / "target"
            summary = root / "summary.md"
            performance.write_results(
                baseline / "windows-whp-baremetal.csv",
                [
                    *[
                        performance.Result(
                            f"legacy-{index}",
                            "virtfs_reuse",
                            "ms",
                            "lower",
                            500.0,
                        )
                        for index in range(5)
                    ],
                    *[
                        performance.Result(
                            f"verified-{index}",
                            "virtfs_verified_reuse",
                            "ms",
                            "lower",
                            2250.0,
                        )
                        for index in range(3)
                    ],
                    performance.Result(
                        "canonical-after-transition",
                        "virtfs_reuse",
                        "ms",
                        "lower",
                        2280.0,
                    ),
                ],
            )
            performance.write_results(
                target / "windows-whp-baremetal.csv",
                [performance.Result("pr", "virtfs_reuse", "ms", "lower", 2270.0)],
            )

            self.assertEqual(
                performance.gate_results(
                    baseline,
                    target,
                    10,
                    40.0,
                    summary,
                    minimum_history=4,
                ),
                0,
            )
            markdown = summary.read_text(encoding="utf-8")
            self.assertIn("2250.00 ms (4/10)", markdown)
            self.assertNotIn("500.00 ms", markdown)

    def test_persist_is_idempotent_per_commit_and_metric(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            history = root / "history"
            first = [
                performance.Result("commit-1", "latency", "ms", "lower", 10.0),
                performance.Result("commit-1", "throughput", "MB/s", "higher", 20.0),
            ]
            performance.write_results(source / "linux-kvm.csv", first)
            performance.persist_results(source, history)

            performance.write_results(
                source / "linux-kvm.csv",
                [
                    performance.Result("commit-1", "latency", "ms", "lower", 999.0),
                    performance.Result(
                        "commit-1", "throughput", "MB/s", "higher", 999.0
                    ),
                ],
            )
            performance.persist_results(source, history)
            self.assertEqual(performance.read_results(history / "linux-kvm.csv"), first)

            performance.write_results(
                source / "linux-kvm.csv",
                [
                    performance.Result("commit-2", "latency", "ms", "lower", 11.0),
                    performance.Result(
                        "commit-2", "throughput", "MB/s", "higher", 21.0
                    ),
                ],
            )
            performance.persist_results(source, history)
            self.assertEqual(
                len(performance.read_results(history / "linux-kvm.csv")), 4
            )

    def test_persist_merges_distinct_metrics_for_same_abi_commit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            history = root / "history"
            filename = "linux-kvm-baremetal-microvm-v2-1vcpu.csv"
            lifecycle = performance.Result(
                "commit-1",
                "cold_start_base",
                "ms",
                "lower",
                10.0,
                "linux-kvm-baremetal",
                2,
                1,
            )
            device = performance.Result(
                "commit-1",
                "virtio_blk_random_read_iops",
                "ops/s",
                "higher",
                20.0,
                "linux-kvm-baremetal",
                2,
                1,
            )

            performance.write_results(source / filename, [lifecycle])
            performance.persist_results(source, history)
            performance.write_results(source / filename, [device])
            performance.persist_results(source, history)

            self.assertEqual(
                performance.read_results(history / filename),
                [lifecycle, device],
            )

    def test_persist_writes_all_platform_results(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            history = root / "history"
            platforms = (
                "linux-kvm-baremetal",
                "linux-kvm-virtual-machine",
                "linux-mshv-baremetal",
                "linux-mshv-virtual-machine",
                "windows-whp-baremetal",
                "windows-whp-virtual-machine",
            )
            results = [performance.Result("commit", "latency", "ms", "lower", 10.0)]
            for platform in platforms:
                performance.write_results(source / f"{platform}.csv", results)

            performance.persist_results(source, history)

            for platform in platforms:
                self.assertEqual(
                    performance.read_results(history / f"{platform}.csv"), results
                )

    def test_persist_excludes_selected_metrics(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            history = root / "history"
            performance.write_results(
                source / "linux-kvm.csv",
                [
                    performance.Result("commit", "keep", "ms", "lower", 10.0),
                    performance.Result("commit", "reset", "ms", "lower", 20.0),
                ],
            )

            self.assertEqual(
                nvx.main(
                    [
                        "performance",
                        "persist",
                        "--source-dir",
                        str(source),
                        "--history-dir",
                        str(history),
                        "--exclude-metric",
                        "reset",
                    ]
                ),
                0,
            )

            self.assertEqual(
                performance.read_results(history / "linux-kvm.csv"),
                [performance.Result("commit", "keep", "ms", "lower", 10.0)],
            )

    def test_reads_legacy_history_as_microvm_v1_one_vcpu(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "linux-kvm-baremetal.csv"
            path.write_text(
                "commit,metric,unit,direction,p50\nlegacy,latency,ms,lower,10\n",
                encoding="utf-8",
            )

            result = performance.read_results(path)[0]
            self.assertEqual(result.platform, "linux-kvm-baremetal")
            self.assertEqual(result.microvm_abi_version, 1)
            self.assertEqual(result.processors, 1)

    def test_collects_dimensioned_microvm_v2_results(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / performance.BENCHMARK_METADATA_FILENAME).write_text(
                json.dumps(
                    {
                        "platform": "linux-kvm-baremetal",
                        "backend": "kvm",
                        "microvm_abi_version": 2,
                        "processors": 4,
                    }
                ),
                encoding="utf-8",
            )

            path = performance.collect_results(
                "linux-kvm-baremetal", "commit", logs, root / "results"
            )
            self.assertEqual(path.name, "linux-kvm-baremetal-microvm-v2-4vcpu.csv")
            results = performance.read_results(path)
            self.assertTrue(results)
            self.assertTrue(
                all(
                    result.platform == "linux-kvm-baremetal"
                    and result.microvm_abi_version == 2
                    and result.processors == 4
                    for result in results
                )
            )

    def test_rejects_unreadable_benchmark_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            logs = Path(temporary)
            (logs / performance.BENCHMARK_METADATA_FILENAME).mkdir()

            with self.assertRaisesRegex(
                performance.PerformanceError, "cannot read benchmark metadata"
            ):
                performance.read_workload_dimensions(logs, "linux-kvm-virtual-machine")

    def test_rejects_microvm_v3_dimensions(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / performance.BENCHMARK_METADATA_FILENAME).write_text(
                json.dumps(
                    {
                        "platform": "linux-kvm-baremetal",
                        "backend": "kvm",
                        "microvm_abi_version": 3,
                        "processors": 1,
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                performance.PerformanceError,
                "unsupported microVM ABI/processor dimensions",
            ):
                performance.collect_results(
                    "linux-kvm-baremetal", "commit", logs, root / "results"
                )

    def test_rejects_lifecycle_workload_processor_mismatch(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / performance.BENCHMARK_METADATA_FILENAME).write_text(
                json.dumps(
                    {
                        "platform": "linux-kvm-baremetal",
                        "backend": "kvm",
                        "microvm_abi_version": 2,
                        "processors": 4,
                    }
                ),
                encoding="utf-8",
            )
            lifecycle = lifecycle_document()
            controls = cast(dict[str, object], lifecycle["controls"])
            controls.update(
                {
                    "platform": "linux-kvm-baremetal",
                    "microvm_abi_version": 2,
                    "processors": 2,
                }
            )
            lifecycle_path = root / "lifecycle.json"
            lifecycle_path.write_text(json.dumps(lifecycle), encoding="utf-8")

            with self.assertRaisesRegex(
                performance.PerformanceError, "metadata mismatch"
            ):
                performance.collect_results(
                    "linux-kvm-baremetal",
                    "commit",
                    logs,
                    root / "results",
                    lifecycle_input=lifecycle_path,
                )

    def test_reset_check_ignores_side_branch_history_that_never_reset_a_row(self):
        # A side branch forked before the measurement can hold fewer rows than the
        # measurement without resetting them. Comparing it with the previous commit
        # in log order would flag the throughput row, which it never removed.
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)

            def git(*args: str) -> str:
                completed = subprocess.run(
                    ["git", "-C", str(repo), *args],
                    check=True,
                    capture_output=True,
                    text=True,
                )
                return completed.stdout.strip()

            git("init", "-q")
            git("config", "user.email", "test@example.invalid")
            git("config", "user.name", "test")
            history_file = repo / "data" / "linux-kvm.csv"
            performance.write_results(
                history_file,
                [performance.Result("s0", "latency", "ms", "lower", 10.0)],
            )
            git("add", "data")
            git("commit", "-q", "-m", "seed")
            main = git("rev-parse", "--abbrev-ref", "HEAD")
            git("checkout", "-q", "-b", "side")
            performance.write_results(
                history_file,
                [performance.Result("x", "latency", "ms", "lower", 11.0)],
            )
            git("commit", "-q", "-am", "side change")
            git("checkout", "-q", main)
            performance.write_results(
                history_file,
                [
                    performance.Result("s0", "latency", "ms", "lower", 10.0),
                    performance.Result("A", "throughput", "MB/s", "higher", 20.0),
                ],
            )
            git("commit", "-q", "-am", "measured")
            measured = git("rev-parse", "HEAD")
            # The merge keeps the side branch's latency change and A's throughput row.
            git("merge", "-q", "--no-commit", "--no-ff", "-X", "ours", "side")
            performance.write_results(
                history_file,
                [
                    performance.Result("x", "latency", "ms", "lower", 11.0),
                    performance.Result("A", "throughput", "MB/s", "higher", 20.0),
                ],
            )
            git("commit", "-q", "-am", "merge side")

            self.assertEqual(
                performance._reset_dimensions_since(repo, history_file, measured),
                set(),
            )

    def test_reset_check_ignores_a_merged_branch_that_predates_an_earlier_reset(self):
        # Dev resets latency before measurement A. A branch forked before that reset
        # keeps the old latency rows and adds throughput; merging it into dev keeps the
        # reset, so the merge drops latency relative to that branch. The reset preceded A,
        # so A's latency row must not be discarded.
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)

            def git(*args: str) -> str:
                completed = subprocess.run(
                    ["git", "-C", str(repo), *args],
                    check=True,
                    capture_output=True,
                    text=True,
                )
                return completed.stdout.strip()

            git("init", "-q")
            git("config", "user.email", "test@example.invalid")
            git("config", "user.name", "test")
            history_file = repo / "data" / "linux-kvm.csv"
            performance.write_results(
                history_file,
                [performance.Result("s0", "latency", "ms", "lower", 10.0)],
            )
            git("add", "data")
            git("commit", "-q", "-m", "seed")
            main = git("rev-parse", "--abbrev-ref", "HEAD")
            git("checkout", "-q", "-b", "branch")
            performance.write_results(
                history_file,
                [
                    performance.Result("s0", "latency", "ms", "lower", 12.0),
                    performance.Result("b", "throughput", "MB/s", "higher", 5.0),
                ],
            )
            git("commit", "-q", "-am", "branch work")
            git("checkout", "-q", main)
            performance.write_results(history_file, [])
            git("commit", "-q", "-am", "reset latency before the measurement")
            git("commit", "-q", "--allow-empty", "-m", "measured")
            measured = git("rev-parse", "HEAD")
            git("merge", "-q", "--no-commit", "--no-ff", "-X", "ours", "branch")
            performance.write_results(
                history_file,
                [performance.Result("b", "throughput", "MB/s", "higher", 5.0)],
            )
            git("commit", "-q", "-am", "merge branch")

            self.assertEqual(
                performance._reset_dimensions_since(repo, history_file, measured),
                set(),
            )

    def test_persist_drops_rows_measured_before_a_later_history_reset(self):
        # A row measured before a commit that reset its metric must not refill the reset
        # series, or nine post-reset rows would already gate the metric. Unaffected metrics
        # measured at the same commit still persist.
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)

            def git(*args: str) -> str:
                completed = subprocess.run(
                    ["git", "-C", str(repo), *args],
                    check=True,
                    capture_output=True,
                    text=True,
                )
                return completed.stdout.strip()

            def commit(message: str) -> str:
                git("add", "data")
                git("commit", "-q", "--allow-empty", "-m", message)
                return git("rev-parse", "HEAD")

            git("init", "-q")
            git("config", "user.email", "test@example.invalid")
            git("config", "user.name", "test")
            history = repo / "data"
            source = repo / "source"
            history_file = history / "linux-kvm.csv"
            source_file = source / "linux-kvm.csv"
            performance.write_results(
                history_file,
                [
                    performance.Result("seed", "latency", "ms", "lower", 10.0),
                    performance.Result("seed", "throughput", "MB/s", "higher", 20.0),
                ],
            )
            measured = commit("seed")

            # Commit B resets only the latency metric; throughput keeps its history.
            performance.write_results(
                history_file,
                [performance.Result("seed", "throughput", "MB/s", "higher", 20.0)],
            )
            commit("reset latency")

            for number in range(9):
                sha = commit(f"post-reset {number}")
                performance.write_results(
                    source_file,
                    [performance.Result(sha, "latency", "ms", "lower", 100.0)],
                )
                performance.persist_results(source, history, git_checkout=repo)

            # The measurement of A arrives after the reset. It must not refill latency.
            performance.write_results(
                source_file,
                [
                    performance.Result(measured, "latency", "ms", "lower", 10.0),
                    performance.Result(measured, "throughput", "MB/s", "higher", 20.0),
                ],
            )
            performance.persist_results(source, history, git_checkout=repo)

            rows = performance.read_results(history_file)
            latency = [row for row in rows if row.metric == "latency"]
            throughput = [row for row in rows if row.metric == "throughput"]
            self.assertEqual(len(latency), 9)
            self.assertNotIn(measured, {row.commit for row in latency})
            self.assertEqual({row.commit for row in throughput}, {"seed", measured})

    def test_persist_detects_merged_reset_and_ignores_reverted_side_reset(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)

            def git(*args: str) -> str:
                return subprocess.run(
                    ["git", "-C", str(repo), *args],
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout.strip()

            def commit(message: str) -> str:
                git("add", "data")
                git("commit", "-q", "--allow-empty", "-m", message)
                return git("rev-parse", "HEAD")

            git("init", "-q")
            git("config", "user.email", "test@example.invalid")
            git("config", "user.name", "test")
            history_file = repo / "data" / "linux-kvm.csv"
            original = [performance.Result("seed", "latency", "ms", "lower", 10.0)]
            performance.write_results(history_file, original)
            commit("seed")
            main = git("rev-parse", "--abbrev-ref", "HEAD")

            # A side branch forked before the measurement resets and restores
            # rows; its temporary reset is outside the measured ancestry.
            git("checkout", "-q", "-b", "reverted")
            unrelated = repo / "data" / "unrelated.txt"
            unrelated.write_text("x", encoding="utf-8")
            git("add", "data/unrelated.txt")
            git("commit", "-q", "-m", "unrelated")
            performance.write_results(history_file, [])
            commit("temporary reset")
            performance.write_results(history_file, original)
            commit("restore rows")
            git("checkout", "-q", main)
            measured = commit("measurement")
            git("merge", "-q", "--no-ff", "reverted", "-m", "merge restored branch")

            source = repo / "source"
            performance.write_results(
                source / "linux-kvm.csv",
                [performance.Result(measured, "latency", "ms", "lower", 11.0)],
            )
            performance.persist_results(source, repo / "data", git_checkout=repo)
            rows = performance.read_results(history_file)
            self.assertIn(measured, {row.commit for row in rows})

            # A second branch performs a multi-commit reset. The integration
            # merge must expose the loss even when git's path simplification
            # would otherwise omit an intermediate commit.
            git("checkout", "-q", "-b", "reset-branch")
            performance.write_results(history_file, [])
            commit("begin reset")
            commit("continue reset")
            git("checkout", "-q", main)
            git("merge", "-q", "--no-ff", "reset-branch", "-m", "merge reset")
            performance.write_results(
                source / "linux-kvm.csv",
                [performance.Result(measured, "latency", "ms", "lower", 12.0)],
            )
            performance.persist_results(source, repo / "data", git_checkout=repo)
            self.assertEqual(performance.read_results(history_file), [])

    def test_persist_filters_stale_metadata_before_validation(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)

            def git(*args: str) -> str:
                return subprocess.run(
                    ["git", "-C", str(repo), *args],
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout.strip()

            def commit(message: str) -> str:
                git("add", "data")
                git("commit", "-q", "-m", message)
                return git("rev-parse", "HEAD")

            git("init", "-q")
            git("config", "user.email", "test@example.invalid")
            git("config", "user.name", "test")
            history_file = repo / "data" / "linux-kvm.csv"
            performance.write_results(
                history_file,
                [
                    performance.Result("seed", "latency", "ms", "lower", 10.0),
                    performance.Result("seed", "throughput", "MB/s", "higher", 20.0),
                ],
            )
            measured = commit("measurement")
            performance.write_results(
                history_file,
                [
                    performance.Result("seed", "throughput", "MB/s", "higher", 20.0),
                ],
            )
            commit("reset latency")
            source = repo / "source"
            performance.write_results(
                source / "linux-kvm.csv",
                [
                    performance.Result(measured, "latency", "seconds", "higher", 1.0),
                    performance.Result(measured, "throughput", "MB/s", "higher", 21.0),
                ],
            )
            performance.persist_results(source, repo / "data", git_checkout=repo)
            rows = performance.read_results(history_file)
            self.assertEqual(
                [(row.metric, row.commit) for row in rows],
                [("throughput", "seed"), ("throughput", measured)],
            )

    def test_resolve_commit_id_records_the_commit_that_a_revision_names(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            git = git_repository(repo)
            git("commit", "-q", "--allow-empty", "-m", "measured")
            head = git("rev-parse", "HEAD")

            self.assertEqual(performance._resolve_commit_id("HEAD", repo), head)
            self.assertEqual(performance._resolve_commit_id(head[:12], repo), head)
            # A full commit ID is already immutable, so it needs no checkout.
            self.assertEqual(performance._resolve_commit_id(head, repo / "none"), head)
            for revision in ("missing", "--verify"):
                with (
                    self.subTest(revision=revision),
                    self.assertRaisesRegex(
                        performance.PerformanceError, "does not name a commit"
                    ),
                ):
                    performance._resolve_commit_id(revision, repo)

    def test_collect_clis_record_the_commit_id_of_a_symbolic_revision(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            git = git_repository(repo)
            git("commit", "-q", "--allow-empty", "-m", "measured")
            head = git("rev-parse", "HEAD")

            for command, collector, inputs in (
                ("collect", "collect_results", ["--input-dir", "logs"]),
                ("collect-openvmm", "collect_openvmm_results", ["--input", "e2e"]),
            ):
                args = nvx.parse_args(
                    [
                        "performance",
                        command,
                        "--platform",
                        "linux-kvm",
                        "--commit",
                        "HEAD",
                        *inputs,
                        "--output-dir",
                        "results",
                    ]
                )
                with (
                    self.subTest(command=command),
                    patch.object(performance, "ROOT", repo),
                    patch.object(performance, collector) as collect,
                ):
                    self.assertEqual(args.handler(args), 0)
                    self.assertEqual(collect.call_args.args[1], head)

    def test_persist_rejects_a_result_commit_that_is_not_a_commit_id(self):
        # Results collected with --commit HEAD would compare the reset history with
        # itself, find no reset, and refill the reset series.
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            git = git_repository(repo)
            history_file = repo / "data" / "linux-kvm.csv"
            performance.write_results(
                history_file,
                [performance.Result("seed", "latency", "ms", "lower", 10.0)],
            )
            git("add", "data")
            git("commit", "-q", "-m", "measured")
            performance.write_results(history_file, [])
            git("commit", "-q", "-am", "reset latency")
            source = repo / "source"
            performance.write_results(
                source / "linux-kvm.csv",
                [performance.Result("HEAD", "latency", "ms", "lower", 11.0)],
            )

            with self.assertRaisesRegex(
                performance.PerformanceError, "HEAD is not a full commit ID"
            ):
                performance.persist_results(source, repo / "data", git_checkout=repo)
            self.assertEqual(performance.read_results(history_file), [])

    def test_git_checkout_of_probes_the_nearest_existing_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary) / "repo"
            outside = Path(temporary) / "outside"
            repo.mkdir()
            outside.mkdir()
            git_repository(repo)

            self.assertEqual(
                performance._git_checkout_of(repo / "data" / "new"), repo.resolve()
            )
            self.assertIsNone(performance._git_checkout_of(outside / "data"))

    def test_persist_cli_checks_a_history_directory_that_does_not_exist(self):
        # A reset can remove the history directory, and a new series has none yet;
        # neither may skip the reset and ancestry checks.
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            git = git_repository(repo)
            history = repo / "data"
            performance.write_results(
                history / "linux-kvm.csv",
                [performance.Result("seed", "latency", "ms", "lower", 10.0)],
            )
            git("add", "data")
            git("commit", "-q", "-m", "measured")
            measured = git("rev-parse", "HEAD")
            git("rm", "-q", "-r", "data")
            git("commit", "-q", "-m", "reset the series")
            source = repo / "source"

            def persist(commit: str, history_dir: Path) -> int:
                performance.write_results(
                    source / "linux-kvm.csv",
                    [performance.Result(commit, "latency", "ms", "lower", 11.0)],
                )
                with (
                    contextlib.redirect_stdout(io.StringIO()),
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    return nvx.main(
                        [
                            "performance",
                            "persist",
                            "--source-dir",
                            str(source),
                            "--history-dir",
                            str(history_dir),
                        ]
                    )

            self.assertEqual(persist(measured, history), 0)
            self.assertFalse((history / "linux-kvm.csv").exists())
            self.assertEqual(persist("f" * 40, repo / "new"), 2)
            self.assertFalse((repo / "new" / "linux-kvm.csv").exists())

    def test_persist_and_gate_isolate_processor_counts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            history = root / "history"
            baseline = root / "baseline"
            target = root / "target"
            filename = "linux-kvm-baremetal-microvm-v2.csv"
            one_vcpu = performance.Result(
                "commit",
                "latency",
                "ms",
                "lower",
                10.0,
                "linux-kvm-baremetal",
                2,
                1,
            )
            two_vcpu = performance.Result(
                "commit",
                "latency",
                "ms",
                "lower",
                20.0,
                "linux-kvm-baremetal",
                2,
                2,
            )
            performance.write_results(source / filename, [one_vcpu, two_vcpu])
            performance.persist_results(source, history)
            self.assertEqual(len(performance.read_results(history / filename)), 2)

            performance.write_results(baseline / filename, [one_vcpu])
            performance.write_results(target / filename, [two_vcpu])
            summary = root / "summary.md"
            self.assertEqual(
                performance.gate_results(baseline, target, 10, 0, summary), 0
            )
            self.assertIn(
                "linux-kvm-baremetal/microvm-v2/2vcpu",
                summary.read_text(encoding="utf-8"),
            )

    def test_gate_never_uses_legacy_abi_v1_as_abi_v2_baseline(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline"
            target = root / "target"
            filename = "linux-kvm-baremetal-microvm-v2-1vcpu.csv"
            legacy = performance.Result(
                "base",
                "latency",
                "ms",
                "lower",
                1.0,
                "linux-kvm-baremetal",
                1,
                1,
            )
            current = performance.Result(
                "target",
                "latency",
                "ms",
                "lower",
                100.0,
                "linux-kvm-baremetal",
                2,
                1,
            )
            performance.write_results(baseline / filename, [legacy])
            performance.write_results(target / filename, [current])
            summary = root / "summary.md"

            self.assertEqual(
                performance.gate_results(
                    baseline,
                    target,
                    window=1,
                    threshold=0,
                    minimum_history=1,
                    summary_path=summary,
                ),
                0,
            )
            self.assertIn("Warmup", summary.read_text(encoding="utf-8"))

    def test_rejects_duplicate_dimensional_rows_across_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            result = performance.Result(
                "commit",
                "latency",
                "ms",
                "lower",
                10.0,
                "linux-kvm-baremetal",
                2,
                4,
            )
            performance.write_results(source / "first.csv", [result])
            performance.write_results(source / "second.csv", [result])

            with self.assertRaisesRegex(
                performance.PerformanceError, "duplicate dimensional row"
            ):
                performance.persist_results(source, root / "history")
            with self.assertRaisesRegex(
                performance.PerformanceError, "duplicate dimensional row"
            ):
                performance.gate_results(root / "baseline", source, 10, 40)

    def test_matching_explicit_network_metadata_is_accepted(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "shell-snapshot.log").write_text(
                SHELL_SNAPSHOT_LOG, encoding="utf-8"
            )
            (logs / "network.log").write_text(NETWORK_LOG, encoding="utf-8")
            metadata = {
                "platform": "linux-kvm-baremetal",
                "backend": "kvm",
                "microvm_abi_version": 2,
                "processors": 4,
                "network": "10.0.0.2/24",
                "lifecycle_network": "10.0.0.2/24",
                "host_affinity_set": "0-5",
                "host_cpu_reserve": 2,
                "memory_mib": {"lifecycle": 128},
                "artifact_revisions": {"nvx": "a", "openvmm": "b"},
                "warmups": 1,
                "measured_runs": 10,
                "virtfs_measured_runs": 10,
                "payload_mib": 64,
                "virtfs_memory_mib": 512,
                "shell_memories_mib": [128, 256, 512],
                "network_memory_mib": 256,
            }
            (logs / performance.BENCHMARK_METADATA_FILENAME).write_text(
                json.dumps(metadata), encoding="utf-8"
            )
            lifecycle = lifecycle_document()
            controls = cast(dict[str, object], lifecycle["controls"])
            controls.update(
                {
                    "platform": "linux-kvm-baremetal",
                    "backend": "kvm",
                    "microvm_abi_version": 2,
                    "processors": 4,
                    "network": "10.0.0.2/24",
                    "cpus": "0-5",
                    "host_cpu_reserve": 2,
                    "artifact_revisions": {"nvx": "a", "openvmm": "b"},
                    "warmups": 1,
                }
            )
            lifecycle_path = root / "lifecycle.json"
            lifecycle_path.write_text(json.dumps(lifecycle), encoding="utf-8")

            result = performance.collect_results(
                "linux-kvm-baremetal",
                "commit",
                logs,
                root / "results",
                require_network=True,
                require_shell_snapshot=True,
                require_shared_suite=True,
                lifecycle_input=lifecycle_path,
            )
            self.assertTrue(result.is_file())


class BenchmarkHostTests(unittest.TestCase):
    def identity(
        self, host: dict[str, object], platform: str = KVM_SERIES
    ) -> dict[str, object]:
        return benchmark_host.host_identity(platform, host)

    def compare(
        self,
        expected: dict[str, object],
        actual: dict[str, object],
        platform: str = KVM_SERIES,
    ) -> list[benchmark_host.HostField]:
        return benchmark_host.compare_hosts(
            self.identity(expected, platform), self.identity(actual, platform)
        )

    def test_identity_keeps_only_what_identifies_a_benchmark_host(self):
        kvm = self.identity(KVM_RUNNER_HOST)

        self.assertEqual(kvm["platform"], KVM_SERIES)
        self.assertEqual(kvm["runner_name"], "azure-kvm-3")
        self.assertEqual(
            kvm["cpu"],
            {
                "vendor": "GenuineIntel",
                "family": 6,
                "model": 106,
                "stepping": 6,
                "brand": "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz",
                "microcode": "0xffffffff",
            },
        )
        self.assertEqual(kvm["logical_processors"], 8)
        self.assertEqual(kvm["memory_gib"], 31.3)
        self.assertEqual(
            kvm["os"],
            {"system": "Linux", "release": "6.6.157.1-1.azl3", "revision": None},
        )
        # Paths, device names, SCSI addresses, and sizes differ between
        # identical VMs.
        tmpfs = {"volume": "/tmp", "filesystem": "tmpfs", "system_volume": False}
        self.assertEqual(
            kvm["volumes"],
            {
                "workspace": {
                    "volume": "/",
                    "filesystem": "ext4",
                    "system_volume": True,
                },
                "temporary": tmpfs,
                "scratch": tmpfs,
            },
        )
        # A Linux runner's volumes are on the OS disk and tmpfs, so its data
        # disk is not part of its identity; WHP scratch is on a data disk.
        self.assertEqual(
            kvm["azure"],
            {
                "vm_size": "Standard_D8s_v5",
                "os_disk": {
                    "storage_account_type": "Premium_LRS",
                    "size_gib": 128,
                    "caching": "ReadWrite",
                    "write_accelerator_enabled": False,
                    "ephemeral_option": None,
                },
            },
        )
        whp = self.identity(WHP_RUNNER_HOST, WHP_SERIES)
        self.assertEqual(
            set(cast(dict[str, object], whp["azure"])),
            {"vm_size", "os_disk", "data_disks", "resource_disk_size_kib"},
        )
        self.assertEqual(cast(dict[str, object], whp["os"])["revision"], 33438)
        with tempfile.TemporaryDirectory() as temporary:
            path = benchmark_host.host_file(Path(temporary), WHP_SERIES)
            benchmark_host.write_host(path, whp)
            self.assertEqual(benchmark_host.read_host(path, WHP_SERIES), whp)

    def test_tracked_expected_hosts_describe_each_ci_series(self):
        repository = Path(__file__).parents[1]
        series = re.findall(
            r"^[ \t]*benchmark-platform:[ \t]*(\S+)[ \t]*$",
            (repository / ".github" / "workflows" / "ci.yml").read_text(
                encoding="utf-8"
            ),
            re.MULTILINE,
        )

        self.assertTrue(series)
        for platform in series:
            with self.subTest(platform=platform):
                path = benchmark_host.host_file(repository / "data", platform)
                host = benchmark_host.read_host(path, platform)
                text = path.read_bytes().decode("utf-8").replace("\r\n", "\n")
                self.assertEqual(text, benchmark_host.format_host(host))
                cpu = cast(dict[str, object], host["cpu"])
                system = cast(dict[str, object], host["os"])
                azure = cast(dict[str, object], host["azure"])
                required = {
                    "cpu.vendor": cpu["vendor"],
                    "cpu.family": cpu["family"],
                    "cpu.model": cpu["model"],
                    "cpu.stepping": cpu["stepping"],
                    "cpu.brand": cpu["brand"],
                    "logical_processors": host["logical_processors"],
                    "memory_gib": host["memory_gib"],
                    "os.system": system["system"],
                    "os.release": system["release"],
                    "azure.vm_size": azure["vm_size"],
                }
                for field, value in required.items():
                    self.assertIsNotNone(value, field)
                self.assertEqual(
                    tuple(cast(dict[str, object], host["volumes"])),
                    benchmark_host.VOLUME_ROLES,
                )
                if system["system"] == "Windows":
                    self.assertIsNotNone(system["revision"])
                # Each Linux/MSHV series records its hypervisor's build.
                if benchmark_host.records_hypervisor_build(platform):
                    self.assertIsNotNone(host["hypervisor_build"])

    def test_identical_hosts_match_within_the_memory_tolerance(self):
        # Kernels reserve different amounts of a VM's memory: the same VM
        # size reported 30.9 and 31.3 GiB on the MSHV and KVM runners.
        fields = self.compare(
            KVM_RUNNER_HOST,
            changed_host(KVM_RUNNER_HOST, ("memory_bytes",), 33186406400),
        )

        self.assertEqual(
            [field.label for field in fields],
            [
                "Runner",
                "Machine",
                "CPU signature",
                "CPU model",
                "CPU microcode",
                "Logical processors",
                "Memory",
                "Operating system",
                "Linux kernel",
                "Azure VM size",
                "Azure OS disk",
                "Workspace volume",
                "Temporary-directory volume",
                "Benchmark scratch volume",
            ],
        )
        self.assertEqual({field.result for field in fields}, {benchmark_host.MATCH})
        memory = next(field for field in fields if field.label == "Memory")
        self.assertEqual((memory.expected, memory.actual), ("31.3 GiB", "30.9 GiB"))

    def test_each_required_difference_is_a_mismatch(self):
        windows_scratch = {
            "path": "C:\\Windows\\Temp",
            "volume": "C:",
            "filesystem": "NTFS",
            "system_volume": True,
            "disk_number": 0,
        }
        cases: tuple[
            tuple[dict[str, object], Sequence[str | int], object, set[str]], ...
        ] = (
            # An Emerald Rapids host of the same VM size (#383, #426).
            (KVM_RUNNER_HOST, ("cpu", "model"), 207, {"CPU signature"}),
            (KVM_RUNNER_HOST, ("cpu", "stepping"), 7, {"CPU signature"}),
            (
                KVM_RUNNER_HOST,
                ("cpu_model",),
                "INTEL(R) XEON(R) PLATINUM 8573C",
                {"CPU model"},
            ),
            (KVM_RUNNER_HOST, ("logical_processors",), 16, {"Logical processors"}),
            (KVM_RUNNER_HOST, ("memory_bytes",), 2 * 33605677056, {"Memory"}),
            # Another kernel version, or the flavor of #415's runners.
            (
                KVM_RUNNER_HOST,
                ("os_version", "release"),
                "6.12.40.1-1.azl3",
                {"Linux kernel"},
            ),
            (
                KVM_RUNNER_HOST,
                ("os_version", "release"),
                "6.6.137.mshv2-2.azl3",
                {"Linux kernel"},
            ),
            (
                WHP_RUNNER_HOST,
                ("os_version", "release"),
                "10.0.26200",
                {"Windows build"},
            ),
            (
                KVM_RUNNER_HOST,
                ("azure", "vm_size"),
                "Standard_D8ds_v5",
                {"Azure VM size"},
            ),
            (
                KVM_RUNNER_HOST,
                ("azure", "os_disk", "caching"),
                "None",
                {"Azure OS disk"},
            ),
            (
                KVM_RUNNER_HOST,
                ("azure", "os_disk", "storage_account_type"),
                "StandardSSD_LRS",
                {"Azure OS disk"},
            ),
            (
                WHP_RUNNER_HOST,
                ("azure", "data_disks", 0, "size_gib"),
                1024,
                {"Azure data disks"},
            ),
            (
                WHP_RUNNER_HOST,
                ("azure", "data_disks", 0, "caching"),
                "ReadOnly",
                {"Azure data disks"},
            ),
            (
                KVM_RUNNER_HOST,
                ("volumes", "scratch", "filesystem"),
                "ext4",
                {"Benchmark scratch volume"},
            ),
            # Scratch on the system disk leaves the data disks unused.
            (
                WHP_RUNNER_HOST,
                ("volumes", "scratch"),
                windows_scratch,
                {
                    "Benchmark scratch volume",
                    "Azure data disks",
                    "Azure temporary disk",
                },
            ),
            # Values that cannot be read fail closed.
            (KVM_RUNNER_HOST, ("cpu", "vendor"), None, {"CPU signature"}),
            (
                KVM_RUNNER_HOST,
                ("azure",),
                {"error": "Azure instance metadata is unavailable: timed out"},
                {"Azure VM size", "Azure OS disk"},
            ),
            (KVM_RUNNER_HOST, ("azure",), None, {"Azure VM size"}),
        )
        for host, path, value, labels in cases:
            platform = WHP_SERIES if host is WHP_RUNNER_HOST else KVM_SERIES
            with self.subTest(path=path, value=value):
                fields = self.compare(host, changed_host(host, path, value), platform)
                self.assertEqual(
                    {field.label for field in benchmark_host.mismatches(fields)},
                    labels,
                )
                self.assertEqual(benchmark_host.drift(fields), [])

    def test_advisory_differences_only_drift(self):
        cases: tuple[tuple[dict[str, object], Sequence[str], object, str], ...] = (
            (KVM_RUNNER_HOST, ("runner_name",), "azure-kvm-1", "Runner"),
            (
                KVM_RUNNER_HOST,
                ("machine_name",),
                "esaurez-nvx-ci-linux-kvm-vmss000001",
                "Machine",
            ),
            (KVM_RUNNER_HOST, ("cpu", "microcode"), "0xd0003e7", "CPU microcode"),
            (
                KVM_RUNNER_HOST,
                ("os_version", "release"),
                "6.6.160.1-2.azl3",
                "Linux kernel",
            ),
            (WHP_RUNNER_HOST, ("os_version", "revision"), 33500, "Windows build"),
        )
        for host, path, value, label in cases:
            platform = WHP_SERIES if host is WHP_RUNNER_HOST else KVM_SERIES
            with self.subTest(path=path):
                fields = self.compare(host, changed_host(host, path, value), platform)
                self.assertEqual(benchmark_host.mismatches(fields), [])
                self.assertEqual(
                    [field.label for field in benchmark_host.drift(fields)], [label]
                )

    def test_kernel_series_keeps_the_version_and_flavor(self):
        for release, series in (
            ("6.6.157.1-1.azl3", "6.6 azl3"),
            ("6.6.148-200.azl3", "6.6 azl3"),
            ("6.6.137.mshv2-2.azl3", "6.6 mshv2 azl3"),
            ("6.18.34.mshv3-1.azl3", "6.18 mshv3 azl3"),
            ("6.8.0-1015-azure", "6.8 azure"),
        ):
            with self.subTest(release=release):
                self.assertEqual(benchmark_host.kernel_series(release), series)

    def test_mshv_hosts_record_the_hypervisor_build(self):
        mshv = self.identity(MSHV_RUNNER_HOST, MSHV_SERIES)

        self.assertEqual(mshv["hypervisor_build"], "10.0.26100.9444")
        # The host file lists it after the operating system.
        keys = list(mshv)
        self.assertEqual(keys[keys.index("os") + 1], "hypervisor_build")
        with tempfile.TemporaryDirectory() as temporary:
            path = benchmark_host.host_file(Path(temporary), MSHV_SERIES)
            benchmark_host.write_host(path, mshv)
            self.assertEqual(benchmark_host.read_host(path, MSHV_SERIES), mshv)
        # A hypervisor that couldn't be read is unknown, while provenance that
        # predates the build records none.
        unreadable = changed_host(
            MSHV_RUNNER_HOST, ("hypervisor",), {"error": "CPUID is unavailable: x"}
        )
        self.assertIsNone(self.identity(unreadable, MSHV_SERIES)["hypervisor_build"])
        self.assertNotIn(
            "hypervisor_build", self.identity(MSHV_RUNNER_HOST_BEFORE_980, MSHV_SERIES)
        )
        # A KVM VM's hypervisor is its cloud host's, and a Windows build and
        # UBR identify Windows' own.
        for platform in (KVM_SERIES, "linux-kvm-baremetal", WHP_SERIES):
            with self.subTest(platform=platform):
                self.assertNotIn(
                    "hypervisor_build", self.identity(MSHV_RUNNER_HOST, platform)
                )
        for platform in ("linux-mshv", "linux-mshv-baremetal"):
            with self.subTest(platform=platform):
                self.assertTrue(benchmark_host.records_hypervisor_build(platform))

    def test_another_hypervisor_build_is_another_host(self):
        # The whole version counts: prometheus30's 10.0.26100.30000 is build
        # 26100 too.
        for build, result in (
            ("10.0.26100.9444", benchmark_host.MATCH),
            ("10.0.26100.30000", benchmark_host.MISMATCH),
            ("10.0.26100.9445", benchmark_host.MISMATCH),
            (None, benchmark_host.MISMATCH),
        ):
            with self.subTest(build=build):
                fields = self.compare(
                    MSHV_RUNNER_HOST,
                    changed_host(MSHV_RUNNER_HOST, ("hypervisor", "build"), build),
                    MSHV_SERIES,
                )
                labels = [field.label for field in fields]
                self.assertEqual(
                    labels[labels.index("Linux kernel") + 1], "Hypervisor build"
                )
                field = fields[labels.index("Hypervisor build")]
                self.assertEqual(field.result, result)
                self.assertEqual(
                    benchmark_host.mismatches(fields),
                    [] if result == benchmark_host.MATCH else [field],
                )
                self.assertEqual(benchmark_host.drift(fields), [])
        unknown = self.compare(
            MSHV_RUNNER_HOST,
            changed_host(MSHV_RUNNER_HOST, ("hypervisor",), {"error": "x"}),
            MSHV_SERIES,
        )
        self.assertEqual(
            [field.describe() for field in benchmark_host.mismatches(unknown)],
            ["Hypervisor build: expected 10.0.26100.9444, found unknown"],
        )
        # An unknown build matches none, not even another unknown one, so an
        # expected host whose hypervisor couldn't be read never passes.
        unreadable = changed_host(MSHV_RUNNER_HOST, ("hypervisor",), {"error": "x"})
        self.assertEqual(
            [
                field.describe()
                for field in benchmark_host.mismatches(
                    self.compare(unreadable, unreadable, MSHV_SERIES)
                )
            ],
            ["Hypervisor build: expected unknown, found unknown"],
        )

    def test_hosts_that_predate_the_hypervisor_build_only_drift(self):
        # Adding the build to an expected host, or comparing it with results
        # that a dev run in flight recorded before it, needs no history reset.
        legacy = self.identity(MSHV_RUNNER_HOST_BEFORE_980, MSHV_SERIES)
        current = self.identity(MSHV_RUNNER_HOST, MSHV_SERIES)
        for expected, actual, description in (
            (legacy, current, "expected not recorded, found 10.0.26100.9444"),
            (current, legacy, "expected 10.0.26100.9444, found not recorded"),
        ):
            with self.subTest(description=description):
                fields = benchmark_host.compare_hosts(expected, actual)
                self.assertEqual(benchmark_host.mismatches(fields), [])
                self.assertEqual(
                    [field.describe() for field in benchmark_host.drift(fields)],
                    [f"Hypervisor build: {description}"],
                )
        with tempfile.TemporaryDirectory() as temporary:
            path = benchmark_host.host_file(Path(temporary), MSHV_SERIES)
            benchmark_host.write_host(path, legacy)
            self.assertEqual(benchmark_host.read_host(path, MSHV_SERIES), legacy)

    def test_host_files_reject_unknown_and_malformed_fields(self):
        host = self.identity(KVM_RUNNER_HOST)
        cpu = cast(dict[str, object], host["cpu"])
        cases: tuple[tuple[object, str], ...] = (
            ({**host, "surface_digest": "sha256:0"}, "surface_digest is missing"),
            ({**host, "cpu": {**cpu, "family": "6"}}, "cpu.family is missing"),
            ({**host, "memory_gib": True}, "memory_gib is missing"),
            (
                {key: value for key, value in host.items() if key != "volumes"},
                "volumes is missing",
            ),
            ({**host, "platform": WHP_SERIES}, "describes platform"),
            # Only Linux/MSHV hosts record the hypervisor build.
            (
                {**host, "hypervisor_build": "10.0.26102.3669"},
                "hypervisor_build is missing",
            ),
            ([host], "the document is missing"),
        )
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "host.json"
            for document, message in cases:
                with self.subTest(message=message):
                    path.write_text(json.dumps(document), encoding="utf-8")
                    with self.assertRaisesRegex(
                        benchmark_host.HostFileError, re.escape(message)
                    ):
                        benchmark_host.read_host(path, KVM_SERIES)
            mshv = self.identity(MSHV_RUNNER_HOST, MSHV_SERIES)
            for build in (26100, ""):
                with self.subTest(build=build):
                    path.write_text(
                        json.dumps({**mshv, "hypervisor_build": build}),
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(
                        benchmark_host.HostFileError, "hypervisor_build is missing"
                    ):
                        benchmark_host.read_host(path, MSHV_SERIES)
            path.write_text("{", encoding="utf-8")
            with self.assertRaisesRegex(benchmark_host.HostFileError, "cannot read"):
                benchmark_host.read_host(path, KVM_SERIES)
            with self.assertRaisesRegex(benchmark_host.HostFileError, "does not exist"):
                benchmark_host.read_host(path.with_name("missing.json"), KVM_SERIES)

    def check_host(
        self,
        root: Path,
        provenance: dict[str, object],
        *,
        expected: bool = True,
        platform: str = KVM_SERIES,
        expected_host: dict[str, object] = KVM_RUNNER_HOST,
    ) -> tuple[int, str, str, dict[str, object]]:
        history = root / "data"
        if expected:
            benchmark_host.write_host(
                benchmark_host.host_file(history, platform),
                self.identity(expected_host, platform),
            )
        scratch = root / "scratch"
        scratch.mkdir()
        output = root / "runs" / f"{platform}-host.json"
        summary = root / "summary.md"
        stdout = io.StringIO()
        with (
            patch.object(
                performance.host_telemetry,
                "host_provenance",
                return_value=copy.deepcopy(provenance),
            ) as collect,
            contextlib.redirect_stdout(stdout),
        ):
            status = performance.check_host(
                platform,
                history,
                scratch_dir=scratch,
                output=output,
                summary_path=summary,
            )
        collect.assert_called_once_with(benchmark_host.benchmark_volumes(scratch))
        return (
            status,
            stdout.getvalue(),
            summary.read_text(encoding="utf-8"),
            json.loads(output.read_text(encoding="utf-8")),
        )

    def test_check_host_fails_and_lists_every_required_difference(self):
        # azure-kvm-3 back on an Emerald Rapids host with #415's kernel.
        provenance = changed_host(KVM_RUNNER_HOST, ("cpu", "model"), 207)
        provenance = changed_host(provenance, ("cpu", "stepping"), 2)
        provenance = changed_host(
            provenance, ("cpu_model",), "INTEL(R) XEON(R) PLATINUM 8573C"
        )
        provenance = changed_host(
            provenance, ("os_version", "release"), "6.6.137.mshv2-2.azl3"
        )
        provenance = changed_host(
            provenance, ("machine_name",), "esaurez-nvx-ci-linux-kvm-vmss000009"
        )
        with tempfile.TemporaryDirectory() as temporary:
            status, output, summary, actual = self.check_host(
                Path(temporary), provenance
            )

        self.assertEqual(status, 1)
        errors = [line for line in output.splitlines() if line.startswith("::error")]
        self.assertEqual(len(errors), 1)
        self.assertTrue(errors[0].startswith("::error title=Benchmark host mismatch::"))
        for difference in (
            "CPU signature: expected GenuineIntel 6/106/6, found GenuineIntel 6/207/2",
            "CPU model: expected Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz, "
            "found INTEL(R) XEON(R) PLATINUM 8573C",
            "Linux kernel: expected 6.6.157.1-1.azl3, found 6.6.137.mshv2-2.azl3",
        ):
            self.assertIn(difference, errors[0])
        self.assertIn("in 3 required field(s)", errors[0])
        self.assertNotIn("Machine", errors[0])
        warnings = [
            line for line in output.splitlines() if line.startswith("::warning")
        ]
        self.assertEqual(len(warnings), 1)
        self.assertIn(
            "Machine: expected esaurez-nvx-ci-linux-kvm-vmss000003, "
            "found esaurez-nvx-ci-linux-kvm-vmss000009",
            warnings[0],
        )
        self.assertEqual(actual, self.identity(provenance))
        self.assertIn("**mismatch** (3 required field(s) differ)", summary)
        self.assertIn(
            "| CPU signature | GenuineIntel 6/106/6 | GenuineIntel 6/207/2 | "
            "**mismatch** |",
            summary,
        )
        self.assertIn("| Machine | esaurez-nvx-ci-linux-kvm-vmss000003 |", summary)
        self.assertIn("accept the host", summary)
        self.assertIn(benchmark_host.format_host(actual), summary)

    def test_check_host_warns_about_advisory_drift(self):
        provenance = changed_host(KVM_RUNNER_HOST, ("cpu", "microcode"), "0xd0003e7")
        with tempfile.TemporaryDirectory() as temporary:
            status, output, summary, _ = self.check_host(Path(temporary), provenance)

        self.assertEqual(status, 0)
        self.assertNotIn("::error", output)
        self.assertIn(
            "::warning title=Benchmark host drift::Linux / KVM / Virtual machine: "
            "this runner differs from ",
            output,
        )
        self.assertIn(
            "CPU microcode: expected 0xffffffff, found 0xd0003e7. Update the file",
            output,
        )
        self.assertIn("match with drift (1 advisory field(s) differ)", summary)
        self.assertIn("needs no history reset", summary)

    def test_check_host_passes_on_the_expected_host(self):
        with tempfile.TemporaryDirectory() as temporary:
            status, output, summary, actual = self.check_host(
                Path(temporary), KVM_RUNNER_HOST
            )

        self.assertEqual(status, 0)
        self.assertNotIn("::", output)
        self.assertIn(
            "Benchmark host check for linux-kvm-virtual-machine: match", output
        )
        self.assertIn("`: match.", summary)
        self.assertEqual(actual, self.identity(KVM_RUNNER_HOST))

    def test_check_host_requires_the_mshv_hypervisor_build(self):
        # A reimage of azure-mshv-4 that brings another hypervisor but keeps
        # the kernel (#980).
        provenance = changed_host(
            MSHV_RUNNER_HOST, ("hypervisor", "build"), "10.0.26100.30000"
        )
        with tempfile.TemporaryDirectory() as temporary:
            status, output, summary, actual = self.check_host(
                Path(temporary),
                provenance,
                platform=MSHV_SERIES,
                expected_host=MSHV_RUNNER_HOST,
            )

        self.assertEqual(status, 1)
        errors = [line for line in output.splitlines() if line.startswith("::error")]
        self.assertEqual(len(errors), 1)
        self.assertIn(
            "in 1 required field(s): Hypervisor build: expected 10.0.26100.9444, "
            "found 10.0.26100.30000.",
            errors[0],
        )
        self.assertEqual(actual["hypervisor_build"], "10.0.26100.30000")
        self.assertIn(
            "| Hypervisor build | 10.0.26100.9444 | 10.0.26100.30000 | **mismatch** |",
            summary,
        )

        with tempfile.TemporaryDirectory() as temporary:
            status, output, _, actual = self.check_host(
                Path(temporary),
                MSHV_RUNNER_HOST,
                platform=MSHV_SERIES,
                expected_host=MSHV_RUNNER_HOST,
            )

        self.assertEqual(status, 0)
        self.assertNotIn("::", output)
        self.assertIn(
            "MATCH: Hypervisor build: expected 10.0.26100.9444, found 10.0.26100.9444",
            output,
        )
        self.assertEqual(actual, self.identity(MSHV_RUNNER_HOST, MSHV_SERIES))

    def test_check_host_warns_about_an_expected_host_without_the_build(self):
        with tempfile.TemporaryDirectory() as temporary:
            status, output, summary, actual = self.check_host(
                Path(temporary),
                MSHV_RUNNER_HOST,
                platform=MSHV_SERIES,
                expected_host=MSHV_RUNNER_HOST_BEFORE_980,
            )

        self.assertEqual(status, 0)
        self.assertNotIn("::error", output)
        self.assertIn(
            "Hypervisor build: expected not recorded, found 10.0.26100.9444. "
            "Update the file",
            output,
        )
        self.assertIn("needs no history reset", summary)
        self.assertEqual(actual["hypervisor_build"], "10.0.26100.9444")

    def test_check_host_requires_an_expected_host(self):
        with tempfile.TemporaryDirectory() as temporary:
            status, output, summary, actual = self.check_host(
                Path(temporary), KVM_RUNNER_HOST, expected=False
            )

        self.assertEqual(status, 2)
        self.assertIn("::error title=Benchmark host::", output)
        self.assertIn("linux-kvm-virtual-machine-host.json does not exist", output)
        self.assertIn("**No valid expected host**", summary)
        self.assertIn(benchmark_host.format_host(actual), summary)

    def test_check_host_compares_before_writing_over_the_expected_host(self):
        # --output may name the expected host file itself.
        provenance = changed_host(KVM_RUNNER_HOST, ("cpu", "model"), 207)
        with tempfile.TemporaryDirectory() as temporary:
            history = Path(temporary)
            expected = benchmark_host.host_file(history, KVM_SERIES)
            benchmark_host.write_host(expected, self.identity(KVM_RUNNER_HOST))
            stdout = io.StringIO()
            with (
                patch.object(
                    performance.host_telemetry,
                    "host_provenance",
                    return_value=provenance,
                ),
                contextlib.redirect_stdout(stdout),
            ):
                status = performance.check_host(KVM_SERIES, history, output=expected)

            self.assertEqual(status, 1)
            self.assertIn("::error title=Benchmark host mismatch::", stdout.getvalue())
            self.assertEqual(
                benchmark_host.read_host(expected, KVM_SERIES),
                self.identity(provenance),
            )

    def test_check_host_cli(self):
        args = nvx.parse_args(
            [
                "performance",
                "check-host",
                "--platform",
                KVM_SERIES,
                "--history-dir",
                "data",
                "--scratch-dir",
                "scratch",
                "--output",
                "host.json",
                "--summary",
                "summary.md",
            ]
        )
        with patch.object(performance, "check_host", return_value=1) as check:
            self.assertEqual(args.handler(args), 1)

        check.assert_called_once_with(
            KVM_SERIES,
            Path("data"),
            scratch_dir=Path("scratch"),
            output=Path("host.json"),
            summary_path=Path("summary.md"),
        )
        self.assertTrue(
            nvx.parse_args(
                [
                    "performance",
                    "persist",
                    "--source-dir",
                    "results",
                    "--history-dir",
                    "data",
                    "--require-host",
                ]
            ).require_host
        )

    def test_annotations_escape_workflow_command_data(self):
        self.assertEqual(
            performance._annotation("error", "Host: a, b", "100%\nfound"),
            "::error title=Host%3A a%2C b::100%25%0Afound",
        )

    def test_collect_records_the_host_of_its_results(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "acceptance.json"
            document = lifecycle_document()
            document["host"] = KVM_RUNNER_HOST
            source.write_text(json.dumps(document), encoding="utf-8")
            results = root / "results"

            performance.collect_openvmm_results(KVM_SERIES, "first", source, results)

            path = benchmark_host.host_file(results, KVM_SERIES)
            self.assertEqual(
                benchmark_host.read_host(path, KVM_SERIES),
                self.identity(KVM_RUNNER_HOST),
            )
            # Another host's results cannot join them.
            document["host"] = changed_host(
                KVM_RUNNER_HOST, ("machine_name",), "esaurez-nvx-ci-linux-kvm-vmss1"
            )
            source.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(
                performance.PerformanceError,
                "records another host than .*Machine: expected",
            ):
                performance.collect_openvmm_results(
                    KVM_SERIES, "second", source, results
                )
            self.assertEqual(
                {
                    result.commit
                    for result in performance.read_results(
                        results / f"{KVM_SERIES}.csv"
                    )
                },
                {"first"},
            )
            # Results that record no host leave no host file.
            del document["host"]
            source.write_text(json.dumps(document), encoding="utf-8")
            performance.collect_openvmm_results(
                KVM_SERIES, "third", source, root / "other"
            )
            self.assertFalse(
                benchmark_host.host_file(root / "other", KVM_SERIES).exists()
            )
            # Results with and without a recorded host never mix, in either
            # order.
            with self.assertRaisesRegex(
                performance.PerformanceError,
                "the linux-kvm-virtual-machine results record no host, but "
                ".* records the host",
            ):
                performance.collect_openvmm_results(
                    KVM_SERIES, "fourth", source, results
                )
            document["host"] = KVM_RUNNER_HOST
            controls = cast(dict[str, object], document["controls"])
            controls.update({"microvm_abi_version": 2, "processors": 1})
            source.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(
                performance.PerformanceError,
                "holds linux-kvm-virtual-machine results that record no host",
            ):
                performance.collect_openvmm_results(
                    KVM_SERIES, "fifth", source, root / "other"
                )
            self.assertFalse(
                benchmark_host.host_file(root / "other", KVM_SERIES).exists()
            )

    def test_collect_rejects_lifecycle_and_workload_results_of_two_hosts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / performance.BENCHMARK_METADATA_FILENAME).write_text(
                json.dumps(
                    {
                        "backend": "kvm",
                        "memory_mib": {"lifecycle": 128},
                        "host": KVM_RUNNER_HOST,
                    }
                ),
                encoding="utf-8",
            )
            lifecycle = lifecycle_document()
            lifecycle["host"] = KVM_RUNNER_HOST
            lifecycle_path = root / "acceptance.json"
            lifecycle_path.write_text(json.dumps(lifecycle), encoding="utf-8")

            performance.collect_results(
                KVM_SERIES,
                "commit",
                logs,
                root / "results",
                lifecycle_input=lifecycle_path,
            )
            self.assertEqual(
                benchmark_host.read_host(
                    benchmark_host.host_file(root / "results", KVM_SERIES), KVM_SERIES
                ),
                self.identity(KVM_RUNNER_HOST),
            )

            lifecycle["host"] = changed_host(
                KVM_RUNNER_HOST, ("cpu_model",), "INTEL(R) XEON(R) PLATINUM 8573C"
            )
            lifecycle_path.write_text(json.dumps(lifecycle), encoding="utf-8")
            with self.assertRaisesRegex(
                performance.PerformanceError, "record different hosts: CPU model"
            ):
                performance.collect_results(
                    KVM_SERIES,
                    "commit",
                    logs,
                    root / "other",
                    lifecycle_input=lifecycle_path,
                )
            self.assertFalse((root / "other").exists())
            # A host that only the lifecycle records doesn't vouch for the
            # workload metrics.
            metadata = logs / performance.BENCHMARK_METADATA_FILENAME
            metadata.write_text(
                json.dumps({"backend": "kvm", "memory_mib": {"lifecycle": 128}}),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                performance.PerformanceError,
                "acceptance.json records a host, but .*benchmark-metadata.json "
                "records none",
            ):
                performance.collect_results(
                    KVM_SERIES,
                    "commit",
                    logs,
                    root / "other",
                    lifecycle_input=lifecycle_path,
                )
            self.assertFalse((root / "other").exists())

    def write_series(
        self,
        directory: Path,
        commits: Sequence[str],
        host: dict[str, object] | None,
        p50: float = 10.0,
        platform: str = KVM_SERIES,
    ) -> Path:
        path = directory / f"{platform}-microvm-v2-1vcpu.csv"
        performance.write_results(
            path,
            [
                performance.Result(
                    commit, "openvmm_cold_start", "ms", "lower", p50, platform, 2, 1
                )
                for commit in commits
            ],
        )
        if host is not None:
            benchmark_host.write_host(
                benchmark_host.host_file(directory, platform),
                self.identity(host, platform),
            )
        return path

    def test_persist_refuses_results_of_another_host(self):
        new_host = changed_host(KVM_RUNNER_HOST, ("cpu", "model"), 207)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            history = self.write_series(root / "data", ["base"], KVM_RUNNER_HOST)
            self.write_series(root / "results", ["new"], new_host)

            for require_host in (True, False):
                with self.subTest(require_host=require_host):
                    with self.assertRaisesRegex(
                        performance.PerformanceError,
                        "refusing to persist the linux-kvm-virtual-machine results: "
                        "their host differs from .* in required fields: "
                        "CPU signature: expected GenuineIntel 6/106/6, "
                        "found GenuineIntel 6/207/6",
                    ):
                        performance.persist_results(
                            root / "results", root / "data", require_host=require_host
                        )
                    self.assertEqual(
                        [row.commit for row in performance.read_results(history)],
                        ["base"],
                    )

    def test_persist_warns_about_advisory_drift(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            history = self.write_series(root / "data", ["base"], KVM_RUNNER_HOST)
            self.write_series(
                root / "results",
                ["new"],
                changed_host(
                    KVM_RUNNER_HOST, ("os_version", "release"), "6.6.160.1-1.azl3"
                ),
            )
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                performance.persist_results(
                    root / "results", root / "data", require_host=True
                )

            self.assertIn(
                "::warning title=Benchmark host drift::The host of the "
                "linux-kvm-virtual-machine results differs from ",
                stdout.getvalue(),
            )
            self.assertEqual(
                [row.commit for row in performance.read_results(history)],
                ["base", "new"],
            )

    def test_persist_accepts_hosts_that_predate_the_hypervisor_build(self):
        # A history whose expected host predates the build accepts results
        # that record it, and a history whose host records it accepts the
        # results of a dev run that was in flight when the build was added.
        for history_host, result_host in (
            (MSHV_RUNNER_HOST_BEFORE_980, MSHV_RUNNER_HOST),
            (MSHV_RUNNER_HOST, MSHV_RUNNER_HOST_BEFORE_980),
        ):
            with (
                self.subTest(history_records="hypervisor" in history_host),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                history = self.write_series(
                    root / "data", ["base"], history_host, platform=MSHV_SERIES
                )
                self.write_series(
                    root / "results", ["new"], result_host, platform=MSHV_SERIES
                )
                stdout = io.StringIO()
                with contextlib.redirect_stdout(stdout):
                    performance.persist_results(
                        root / "results", root / "data", require_host=True
                    )

                self.assertIn(
                    "::warning title=Benchmark host drift::", stdout.getvalue()
                )
                self.assertIn("Hypervisor build: expected ", stdout.getvalue())
                self.assertEqual(
                    [row.commit for row in performance.read_results(history)],
                    ["base", "new"],
                )

    def test_persist_refuses_results_of_another_hypervisor_build(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            history = self.write_series(
                root / "data", ["base"], MSHV_RUNNER_HOST, platform=MSHV_SERIES
            )
            self.write_series(
                root / "results",
                ["new"],
                changed_host(
                    MSHV_RUNNER_HOST, ("hypervisor", "build"), "10.0.26100.30000"
                ),
                platform=MSHV_SERIES,
            )
            with self.assertRaisesRegex(
                performance.PerformanceError,
                "refusing to persist the linux-mshv-virtual-machine results: "
                "their host differs from .* in required fields: Hypervisor build: "
                "expected 10.0.26100.9444, found 10.0.26100.30000$",
            ):
                performance.persist_results(
                    root / "results", root / "data", require_host=True
                )
            self.assertEqual(
                [row.commit for row in performance.read_results(history)], ["base"]
            )

    def test_persist_requires_both_hosts_when_asked(self):
        for recorded, expected in ((False, True), (True, False)):
            with (
                self.subTest(recorded=recorded, expected=expected),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                history = self.write_series(
                    root / "data", ["base"], KVM_RUNNER_HOST if expected else None
                )
                self.write_series(
                    root / "results", ["new"], KVM_RUNNER_HOST if recorded else None
                )
                missing = root / ("results" if not recorded else "data")
                with self.assertRaisesRegex(
                    performance.PerformanceError,
                    "cannot verify the host of the linux-kvm-virtual-machine "
                    f"results: {re.escape(str(missing))}",
                ):
                    performance.persist_results(
                        root / "results", root / "data", require_host=True
                    )
                # Without --require-host, a history that records no host
                # accepts any results, but results that record none never
                # join a history that records one.
                if recorded:
                    performance.persist_results(root / "results", root / "data")
                    self.assertEqual(
                        [row.commit for row in performance.read_results(history)],
                        ["base", "new"],
                    )
                else:
                    with self.assertRaisesRegex(
                        performance.PerformanceError, "cannot verify the host"
                    ):
                        performance.persist_results(root / "results", root / "data")
                    self.assertEqual(
                        [row.commit for row in performance.read_results(history)],
                        ["base"],
                    )

    def gate(
        self,
        root: Path,
        candidate_host: dict[str, object],
        *,
        reset: bool,
        platform: str = KVM_SERIES,
        base_host: dict[str, object] = KVM_RUNNER_HOST,
    ) -> tuple[int, str, str]:
        commits = [f"base-{index}" for index in range(10)]
        self.write_series(root / "baseline", commits, base_host, platform=platform)
        self.write_series(
            root / "data", [] if reset else commits, candidate_host, platform=platform
        )
        self.write_series(root / "results", ["pr"], candidate_host, platform=platform)
        summary = root / "summary.md"
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            status = performance.gate_results(
                root / "baseline",
                root / "results",
                10,
                50,
                summary,
                history_reset_dir=root / "data",
            )
        return status, stdout.getvalue(), summary.read_text(encoding="utf-8")

    def test_gate_requires_a_new_host_to_reset_its_series(self):
        new_host = changed_host(KVM_RUNNER_HOST, ("cpu", "model"), 207)
        with tempfile.TemporaryDirectory() as temporary:
            status, output, summary = self.gate(Path(temporary), new_host, reset=False)

        self.assertEqual(status, 1)
        self.assertIn(
            "This change accepts a new expected host for linux-kvm-virtual-machine, "
            "so each of its metrics must restart its warmup: CPU signature: "
            "expected GenuineIntel 6/106/6, found GenuineIntel 6/207/6.",
            summary,
        )
        self.assertIn("**Host changed without a history reset**", summary)
        kept = "linux-kvm-virtual-machine-microvm-v2-1vcpu.csv (10 row(s))"
        self.assertIn(
            "**The linux-kvm-virtual-machine series keeps the previous host's "
            f"history**: {kept}.",
            summary,
        )
        self.assertIn(
            "::error title=Benchmark host changed without a history reset::", output
        )
        self.assertIn(f"keeps the previous host's history: {kept}.", output)

    def test_gate_requires_a_new_host_to_reset_metrics_it_no_longer_measures(self):
        # The change stops measuring a metric whose previous-host rows remain;
        # a later change could measure it again on top of them.
        new_host = changed_host(KVM_RUNNER_HOST, ("cpu", "model"), 207)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.write_series(root / "baseline", ["base"], KVM_RUNNER_HOST)
            history = self.write_series(root / "data", [], new_host)
            performance.write_results(
                history,
                [
                    performance.Result(
                        "base", "retired_metric", "ms", "lower", 5.0, KVM_SERIES, 2, 1
                    )
                ],
            )
            self.write_series(root / "results", ["pr"], new_host)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                status = performance.gate_results(
                    root / "baseline",
                    root / "results",
                    10,
                    50,
                    history_reset_dir=root / "data",
                )

        self.assertEqual(status, 1)
        self.assertIn(
            "keeps the previous host's history: "
            "linux-kvm-virtual-machine-microvm-v2-1vcpu.csv (1 row(s))",
            stdout.getvalue(),
        )

    def test_gate_warms_up_a_series_that_accepts_a_new_host(self):
        new_host = changed_host(KVM_RUNNER_HOST, ("cpu", "model"), 207)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            status, output, summary = self.gate(root, new_host, reset=True)

            self.assertEqual(status, 0)
            self.assertNotIn("::error", output)
            self.assertIn("Warmup (history reset)", summary)
            # After the merge, dev appends the new host's results, and only
            # them, to the reset history.
            performance.persist_results(
                root / "results", root / "data", require_host=True
            )
            self.assertEqual(
                [
                    row.commit
                    for row in performance.read_results(
                        root / "data" / f"{KVM_SERIES}-microvm-v2-1vcpu.csv"
                    )
                ],
                ["pr"],
            )
            self.write_series(root / "results", ["old"], KVM_RUNNER_HOST)
            with self.assertRaisesRegex(
                performance.PerformanceError, "refusing to persist"
            ):
                performance.persist_results(
                    root / "results", root / "data", require_host=True
                )

    def test_gate_needs_no_reset_for_advisory_host_edits(self):
        acknowledged = changed_host(KVM_RUNNER_HOST, ("cpu", "microcode"), "0xd0003e7")
        with tempfile.TemporaryDirectory() as temporary:
            status, output, summary = self.gate(
                Path(temporary), acknowledged, reset=False
            )

        self.assertEqual(status, 0)
        self.assertNotIn("::error", output)
        self.assertNotIn("accepts a new expected host", summary)
        self.assertIn("| OK |", summary)

    def test_gate_needs_no_reset_to_add_the_hypervisor_build(self):
        # The base branch's expected host predates the build, and the pull
        # request's records the runner's (#980).
        with tempfile.TemporaryDirectory() as temporary:
            status, output, summary = self.gate(
                Path(temporary),
                MSHV_RUNNER_HOST,
                reset=False,
                platform=MSHV_SERIES,
                base_host=MSHV_RUNNER_HOST_BEFORE_980,
            )

        self.assertEqual(status, 0)
        self.assertNotIn("::error", output)
        self.assertNotIn("accepts a new expected host", summary)
        self.assertIn("| OK |", summary)

    def test_gate_requires_a_new_hypervisor_build_to_reset_its_series(self):
        new_build = changed_host(
            MSHV_RUNNER_HOST, ("hypervisor", "build"), "10.0.26100.30000"
        )
        with tempfile.TemporaryDirectory() as temporary:
            status, output, summary = self.gate(
                Path(temporary),
                new_build,
                reset=False,
                platform=MSHV_SERIES,
                base_host=MSHV_RUNNER_HOST,
            )

        self.assertEqual(status, 1)
        self.assertIn(
            "This change accepts a new expected host for linux-mshv-virtual-machine, "
            "so each of its metrics must restart its warmup: Hypervisor build: "
            "expected 10.0.26100.9444, found 10.0.26100.30000.",
            summary,
        )
        self.assertIn(
            "::error title=Benchmark host changed without a history reset::", output
        )


if __name__ == "__main__":
    unittest.main()

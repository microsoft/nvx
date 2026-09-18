#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import contextlib
import io
import json
import statistics
import sys
import tempfile
import unittest
from pathlib import Path
from typing import cast

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import performance  # noqa: E402

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
    live exchange (cold each)         :     200 ms  (min 190, max 210, n=3)
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
== 64 MiB ==
    cold boot           : median   510.0 ms   (min 500.0, max 1,510.0, n=5)
             fast path   505.0 ms (n=4)  |  slow path  1510.0 ms (n=1, +~1005 ms TSC PIT-calib)
    snapshot restore    : median     5.0 ms   (min 4.8, max 5.2, n=5)
    speedup             : 101x (fast-path cold) .. 102x (median cold) faster via snapshot

== 128 MiB ==
    cold boot           : median   520.0 ms   (min 510.0, max 530.0, n=5)
    snapshot restore    : median     5.5 ms   (min 5.3, max 5.7, n=5)
    speedup             : 95x (fast-path cold) .. 95x (median cold) faster via snapshot

== 256 MiB ==
    cold boot           : median   540.0 ms   (min 530.0, max 550.0, n=5)
    snapshot restore    : median     6.0 ms   (min 5.8, max 6.2, n=5)
    speedup             : 90x (fast-path cold) .. 90x (median cold) faster via snapshot

== 512 MiB ==
    cold boot           : median   580.0 ms   (min 570.0, max 590.0, n=5)
    snapshot restore    : median     7.0 ms   (min 6.8, max 7.2, n=5)
    speedup             : 83x (fast-path cold) .. 83x (median cold) faster via snapshot
"""

SHELL_SNAPSHOT_RESTORE_LOG = """
== 512 MiB ==
    snapshot restore    : median     7.0 ms   (p95 7.2, min 6.8, max 7.2, n=5)
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


class PerformanceTests(unittest.TestCase):
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

        self.assertEqual(status, performance.UNSTABLE_LIFECYCLE_EXIT_CODE)
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

            self.assertEqual(len(results), 31)
            by_metric = {result.metric: result for result in results}
            self.assertEqual(by_metric["cold_start_base"].p50, 101.0)
            self.assertEqual(by_metric["cold_start_cryptomgr_notests"].p50, 109.0)
            self.assertEqual(by_metric["virtfs_live_read"].p50, 1200.0)
            self.assertEqual(by_metric["virtfs_live_read"].direction, "higher")
            self.assertEqual(by_metric["network_snapshot_restore"].p50, 40.0)
            self.assertEqual(by_metric["shell_snapshot_cold_64_mib"].p50, 510.0)
            self.assertEqual(by_metric["shell_snapshot_cold_64_mib"].direction, "lower")
            self.assertEqual(by_metric["shell_snapshot_restore_512_mib"].p50, 7.0)
            self.assertEqual(by_metric["openvmm_snapshot_generation"].p50, 31.0)
            self.assertEqual(
                by_metric["openvmm_snapshot_restore_peak_rss"].p50,
                32.0,
            )
            markdown = (root / "summary.md").read_text(encoding="utf-8")
            self.assertIn("## Linux / KVM benchmark results", markdown)
            self.assertEqual(markdown.count("\n| `"), 31)
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

            self.assertEqual(len(results), 23)
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
                r"exactly 23 metrics \(missing: network_snapshot_cold",
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
                "shell_memories_mib": [64, 128, 256, 512],
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


if __name__ == "__main__":
    unittest.main()

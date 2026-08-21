#!/usr/bin/env python3

import json
import sys
import tempfile
import unittest
from pathlib import Path

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


class PerformanceTests(unittest.TestCase):
    def test_collects_openvmm_mshv_json(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "linux-mshv.json"
            source.write_text(
                json.dumps(
                    {
                        "controls": {"suite": "e2e"},
                        "backends": {
                            "mshv": {"p50_ms": 150.0, "teardown_p50_ms": 20.0}
                        },
                        "snapshot_restore": {
                            "mshv": {"p50_ms": 15.0, "teardown_p50_ms": 4.0}
                        },
                    }
                ),
                encoding="utf-8",
            )

            result_path = performance.collect_openvmm_results(
                "linux-mshv", "abc123", source, root / "results"
            )

            results = performance.read_results(result_path)
            self.assertEqual(
                [result.metric for result in results],
                [
                    "openvmm_cold_start",
                    "openvmm_snapshot_restore",
                    "openvmm_cold_start_teardown",
                    "openvmm_snapshot_restore_teardown",
                ],
            )
            self.assertEqual(results[0].p50, 150.0)

    def test_collects_openvmm_json_and_appends_diagnostics(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "linux-kvm.json"
            summary = root / "summary.md"
            source.write_text(
                json.dumps(
                    {
                        "controls": {"suite": "e2e"},
                        "backends": {
                            "kvm": {
                                "p50_ms": 200.5,
                                "peak_rss_p50_bytes": 64 * 1024 * 1024,
                                "teardown_p50_ms": 25.25,
                            }
                        },
                        "snapshot_restore": {
                            "kvm": {
                                "p50_ms": 20.25,
                                "peak_rss_p50_bytes": 32 * 1024 * 1024,
                                "teardown_p50_ms": 5.5,
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )

            result_path = performance.collect_openvmm_results(
                "linux-kvm", "abc123", source, root / "results", summary
            )

            results = performance.read_results(result_path)
            self.assertEqual(
                [result.metric for result in results],
                [
                    "openvmm_cold_start",
                    "openvmm_snapshot_restore",
                    "openvmm_cold_start_teardown",
                    "openvmm_snapshot_restore_teardown",
                ],
            )
            self.assertEqual(results[0].p50, 200.5)
            self.assertTrue(all(result.direction == "lower" for result in results))
            markdown = summary.read_text(encoding="utf-8")
            self.assertIn("## Linux / KVM benchmark results", markdown)
            self.assertIn("| `openvmm_snapshot_restore` | 20.25 ms |", markdown)
            self.assertIn("## Linux / KVM benchmark diagnostics", markdown)
            self.assertIn("| Cold-start peak RSS p50 | 64.00 MiB |", markdown)
            self.assertIn("| Snapshot-restore speedup | 9.90x |", markdown)

    def test_openvmm_json_requires_the_platform_backend(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "windows-whp.json"
            source.write_text(
                json.dumps(
                    {
                        "controls": {"suite": "e2e"},
                        "backends": {},
                        "snapshot_restore": {},
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                performance.PerformanceError,
                r"backends\.whp",
            ):
                performance.collect_openvmm_results(
                    "windows-whp",
                    "abc123",
                    source,
                    Path(temporary) / "results",
                )

    def test_collect_appends_ci_benchmark_table(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            summary = root / "summary.md"
            logs.mkdir()
            summary.write_text("Existing summary", encoding="utf-8")
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "snapshot.log").write_text(SNAPSHOT_LOG, encoding="utf-8")
            (logs / "snapshot-hello.log").write_text(
                HELLO_SNAPSHOT_LOG, encoding="utf-8"
            )
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

    def test_collects_linux_metrics_from_utf8_and_utf16_logs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            output = root / "results"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "snapshot.log").write_text(SNAPSHOT_LOG, encoding="utf-8")
            (logs / "snapshot-hello.log").write_text(
                HELLO_SNAPSHOT_LOG, encoding="utf-8"
            )
            (logs / "network.log").write_text(NETWORK_LOG, encoding="utf-16")
            (logs / "shell-snapshot.log").write_text(
                SHELL_SNAPSHOT_LOG, encoding="utf-8"
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
            )
            results = performance.read_results(result_path)

            self.assertEqual(len(results), 27)
            by_metric = {result.metric: result for result in results}
            self.assertEqual(by_metric["cold_start_base"].p50, 101.0)
            self.assertEqual(by_metric["cold_start_cryptomgr_notests"].p50, 109.0)
            self.assertEqual(by_metric["virtfs_live_read"].p50, 1200.0)
            self.assertEqual(by_metric["virtfs_live_read"].direction, "higher")
            self.assertEqual(by_metric["network_snapshot_restore"].p50, 40.0)
            self.assertEqual(by_metric["shell_snapshot_cold_64_mib"].p50, 510.0)
            self.assertEqual(by_metric["shell_snapshot_cold_64_mib"].direction, "lower")
            self.assertEqual(by_metric["shell_snapshot_restore_512_mib"].p50, 7.0)
            self.assertEqual(by_metric["python_hello_cold"].p50, 1100.0)
            self.assertEqual(by_metric["python_hello_restore"].p50, 125.0)
            markdown = (root / "summary.md").read_text(encoding="utf-8")
            self.assertIn("## Linux / KVM benchmark results", markdown)
            self.assertEqual(markdown.count("\n| `"), 27)
            self.assertIn(
                "| `virtfs_live_read` | 1200.00 MB/s | Higher is better |", markdown
            )
            self.assertIn(
                "| `network_snapshot_restore` | 40.00 ms | Lower is better |", markdown
            )

    def test_shared_suite_rejects_missing_scenarios(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "snapshot.log").write_text(SNAPSHOT_LOG, encoding="utf-8")
            (logs / "snapshot-hello.log").write_text(
                HELLO_SNAPSHOT_LOG, encoding="utf-8"
            )
            (logs / "shell-snapshot.log").write_text(
                SHELL_SNAPSHOT_LOG, encoding="utf-8"
            )

            with self.assertRaisesRegex(
                performance.PerformanceError,
                r"exactly 27 metrics \(missing: network_snapshot_cold",
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

    def test_collect_requires_shell_snapshot_log_when_requested(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "snapshot.log").write_text(SNAPSHOT_LOG, encoding="utf-8")
            (logs / "snapshot-hello.log").write_text(
                HELLO_SNAPSHOT_LOG, encoding="utf-8"
            )

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

    def test_collect_requires_hello_snapshot_log(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "snapshot.log").write_text(SNAPSHOT_LOG, encoding="utf-8")

            with self.assertRaisesRegex(
                performance.PerformanceError,
                r"required benchmark log not found: .*snapshot-hello\.log",
            ):
                performance.collect_results(
                    "linux-kvm", "abc123", logs, root / "results"
                )

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
            recent = []
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
                performance.gate_results(baseline, target, 10, 40.0, summary, 5.0),
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
                    baseline, target, 10, 40.0, absolute_tolerance_ms=5.0
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

            self.assertEqual(performance.gate_results(baseline, target, 10, 40.0), 1)

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
                performance.gate_results(baseline, target, 10, 40.0, summary),
                0,
            )
            self.assertIn(
                "| linux-kvm | `python_pandas_restore` | 800.00 ms | - | - | Warmup |",
                summary.read_text(encoding="utf-8"),
            )

    def test_gate_uses_verified_windows_virtfs_reuse_history(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline"
            target = root / "target"
            summary = root / "summary.md"
            performance.write_results(
                baseline / "windows-whp.csv",
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
                target / "windows-whp.csv",
                [performance.Result("pr", "virtfs_reuse", "ms", "lower", 2270.0)],
            )

            self.assertEqual(
                performance.gate_results(baseline, target, 10, 40.0, summary),
                0,
            )
            markdown = summary.read_text(encoding="utf-8")
            self.assertIn("2257.50 ms (4/10)", markdown)
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


if __name__ == "__main__":
    unittest.main()

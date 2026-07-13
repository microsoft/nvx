#!/usr/bin/env python3

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import performance  # noqa: E402


COLD_START_LOG = """
  loud (rendered)    :    101.0 ms  (min 100, max 102, n=5)
  quiet (discarded)  :    102.0 ms  (min 101, max 103, n=5)
  loud full logs     :    103.0 ms  (min 102, max 104, n=5)
  silent (quiet klog):    104.0 ms  (min 103, max 105, n=5)
  fast               :    105.0 ms  (min 104, max 106, n=5)
"""

VIRTFS_LOG = """
  rw ephemeral (in-memory)    write    500.0 MB/s  (min 490, max 510, n=3)
                              read   1,200.0 MB/s  (min 1100, max 1300, n=3)
  rw persistent (file-backed) write    400.0 MB/s  (min 390, max 410, n=3)
                              read     900.0 MB/s  (min 850, max 950, n=3)
  reuse image + verify (cold each)  :     200 ms  (min 190, max 210, n=3)
"""

SNAPSHOT_LOG = """
  cold:       300.0 ms  (min 290.0, max 310.0, n=5)
  restore:     30.0 ms  (min 29.0, max 31.0, n=5)
"""

NETWORK_LOG = """
  cold  (guest start -> marker):   400.0 ms  (min 390.0, max 410.0, n=5)
  restore (guest resume -> marker):  40.0 ms  (min 39.0, max 41.0, n=5)
  restore wall-clock             :   50.0 ms  (min 49.0, max 51.0, n=5)
"""


class PerformanceTests(unittest.TestCase):
    def test_collects_linux_and_utf16_windows_logs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            logs = root / "logs"
            output = root / "results"
            logs.mkdir()
            (logs / "cold-start.log").write_text(COLD_START_LOG, encoding="utf-8")
            (logs / "virtfs.log").write_text(VIRTFS_LOG, encoding="utf-8")
            (logs / "snapshot.log").write_text(SNAPSHOT_LOG, encoding="utf-8")
            (logs / "network.log").write_text(NETWORK_LOG, encoding="utf-16")

            result_path = performance.collect_results(
                "windows-whp", "abc123", logs, output, require_network=True
            )
            results = performance.read_results(result_path)

            self.assertEqual(len(results), 15)
            by_metric = {result.metric: result for result in results}
            self.assertEqual(by_metric["virtfs_ephemeral_read"].p50, 1200.0)
            self.assertEqual(by_metric["virtfs_ephemeral_read"].direction, "higher")
            self.assertEqual(by_metric["network_snapshot_restore"].p50, 40.0)

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
                    performance.Result(
                        "pr", "throughput", "MB/s", "higher", 60.0
                    ),
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
                    performance.Result(
                        "pr", "throughput", "MB/s", "higher", 59.0
                    ),
                ],
            )
            self.assertEqual(
                performance.gate_results(baseline, target, 10, 40.0), 1
            )

    def test_persist_is_idempotent_per_commit_and_metric(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            history = root / "history"
            first = [
                performance.Result("commit-1", "latency", "ms", "lower", 10.0),
                performance.Result(
                    "commit-1", "throughput", "MB/s", "higher", 20.0
                ),
            ]
            performance.write_results(source / "linux-kvm.csv", first)
            performance.persist_results(source, history)

            performance.write_results(
                source / "linux-kvm.csv",
                [
                    performance.Result(
                        "commit-1", "latency", "ms", "lower", 999.0
                    ),
                    performance.Result(
                        "commit-1", "throughput", "MB/s", "higher", 999.0
                    ),
                ],
            )
            performance.persist_results(source, history)
            self.assertEqual(
                performance.read_results(history / "linux-kvm.csv"), first
            )

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


if __name__ == "__main__":
    unittest.main()

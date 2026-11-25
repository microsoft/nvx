#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import io
import json
import queue
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    benchmark,
)
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
)


class BenchmarkTests(unittest.TestCase):
    def test_snapshot_profile_records_are_parsed_and_summarized(self):
        record = benchmark.parse_snapshot_profile_line(
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=artifact_open exclusive=1 duration_ns=1500000 "
            b"process_elapsed_ns=2000000 pid=42 logical_bytes=1024 "
            b"allocated_bytes=512\r"
        )

        self.assertIsNotNone(record)
        assert record is not None
        self.assertEqual(record["operation"], "restore")
        self.assertEqual(record["duration_ns"], 1_500_000)
        self.assertTrue(record["exclusive"])
        summary = benchmark.summarize_lifecycle_profiles(
            [{"records": [record, {**record, "duration_ns": 2_500_000}]}]
        )
        metric = summary["phases"]["restore.artifact_open"]
        self.assertEqual(metric["samples_ms"], [1.5, 2.5])
        self.assertEqual(metric["p50_ms"], 2.0)
        self.assertEqual(metric["p95_ms"], 2.5)
        self.assertTrue(metric["exclusive"])

    def test_snapshot_profile_rejects_incomplete_records(self):
        with self.assertRaisesRegex(ValueError, "missing"):
            benchmark.parse_snapshot_profile_line(
                b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore phase=open"
            )

    def test_snapshot_profile_records_console_input_dispatch(self):
        collector = benchmark.SnapshotProfileCollector(42, 100)

        with patch.object(benchmark, "process_resource_counters", return_value={}):
            collector.feed(
                b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                b"phase=input_gate exclusive=1 duration_ns=20 "
                b"process_elapsed_ns=120 pid=42\n"
                b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                b"phase=publication_commit exclusive=1 duration_ns=10 "
                b"process_elapsed_ns=310 pid=42\n"
            )
            sample = collector.finish_capture(200, 260, 300, 400, 450, 4096)

        records = cast(list[dict[str, object]], sample["records"])
        by_phase = {str(record["phase"]): record for record in records}
        generation = by_phase["snapshot_generation"]
        self.assertEqual(generation["duration_ns"], 210)
        self.assertEqual(generation["source"], "openvmm_clock")
        self.assertEqual(sample["generation_duration_ns"], 210)
        dispatch = by_phase["console_input_dispatch"]
        self.assertEqual(dispatch["operation"], "capture")
        self.assertEqual(dispatch["phase"], "console_input_dispatch")
        self.assertEqual(dispatch["duration_ns"], 60)
        self.assertEqual(dispatch["observer_elapsed_ns"], 160)
        self.assertEqual(dispatch["logical_bytes"], 4096)
        self.assertTrue(dispatch["exclusive"])
        round_trip = by_phase["console_command_round_trip"]
        self.assertEqual(round_trip["phase"], "console_command_round_trip")
        self.assertEqual(round_trip["duration_ns"], 100)
        guest_to_publication = by_phase["guest_dispatch_to_publication"]
        self.assertEqual(guest_to_publication["phase"], "guest_dispatch_to_publication")
        self.assertEqual(guest_to_publication["duration_ns"], 100)
        self.assertEqual(by_phase["request_to_publication"]["duration_ns"], 200)

    def test_snapshot_generation_requires_valid_process_clock_boundaries(self):
        gate: dict[str, object] = {
            "operation": "capture",
            "phase": "input_gate",
            "duration_ns": 20,
            "process_elapsed_ns": 120,
            "pid": 42,
        }
        publication: dict[str, object] = {
            "operation": "capture",
            "phase": "publication_commit",
            "duration_ns": 10,
            "process_elapsed_ns": 310,
            "pid": 42,
        }
        self.assertEqual(
            benchmark.snapshot_generation_duration_ns([gate, publication], 42), 210
        )
        for records, error in (
            ([publication], "exactly one 'input_gate'"),
            ([gate], "exactly one 'publication_commit'"),
            ([gate, gate, publication], "exactly one 'input_gate'"),
            ([gate, publication, publication], "exactly one 'publication_commit'"),
            ([gate, {**publication, "pid": 43}], "different process"),
            ([{**gate, "duration_ns": -1}, publication], "not monotonic"),
            ([{**gate, "process_elapsed_ns": 10}, publication], "not monotonic"),
            ([gate, {**publication, "process_elapsed_ns": 125}], "not monotonic"),
        ):
            with self.subTest(records=records):
                with self.assertRaisesRegex(ValueError, error):
                    benchmark.snapshot_generation_duration_ns(records, 42)

    def test_capture_excludes_polling_input_and_matches_profile_generation(self):
        for delay_ms in (0, 500, 2000):
            for profiled in (False, True):
                with self.subTest(delay_ms=delay_ms, profiled=profiled):
                    self.check_capture_timing(delay_ms, profiled)

    def check_capture_timing(self, delay_ms: int, profiled: bool) -> None:
        clock_ns = 0
        writes: list[bytes] = []

        class FakeProcess:
            pid = 42
            returncode = 0

            def poll(self) -> int:
                return 0

            def wait(self) -> int:
                nonlocal clock_ns
                clock_ns += 1_000_000
                return 0

        class FakeInteraction:
            process = FakeProcess()

            def read_output(self, _chunks: object) -> None:
                pass

            def read_stderr(self, _chunks: object) -> None:
                pass

            def write_input(self, data: bytes) -> None:
                nonlocal clock_ns
                writes.append(data)
                if data == b"nvx-snapshot\n":
                    clock_ns += 20_000_000

            def close(self) -> None:
                pass

        chunks = iter(
            (
                ("console", benchmark.BOOT_MARKER + b"\n", 0, False),
                ("console", benchmark.SMP_PROBE_COMPLETION_MARKER + b"\n", 1, False),
                (
                    "console",
                    benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER + b"\n",
                    delay_ms,
                    False,
                ),
                (
                    "stderr",
                    b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                    b"phase=input_gate exclusive=1 duration_ns=500000 "
                    b"process_elapsed_ns=500000000 pid=42\n"
                    b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                    b"phase=publication_commit exclusive=1 duration_ns=100000 "
                    b"process_elapsed_ns=502500000 pid=42\n",
                    3,
                    True,
                ),
                ("console", None, 0, False),
                ("stderr", None, 0, False),
            )
        )
        profiles: list[dict[str, object]] = []
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "snapshot"

            def read_chunk(*, timeout: float) -> tuple[str, bytes | None]:
                nonlocal clock_ns
                del timeout
                stream, chunk, advance_ms, publish = next(chunks)
                clock_ns += advance_ms * 1_000_000
                if publish:
                    snapshot.mkdir()
                return stream, chunk

            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=FakeInteraction()
                ) as interaction,
                patch.object(benchmark.threading, "Thread"),
                patch.object(benchmark.queue, "Queue") as queues,
                patch.object(
                    benchmark.time, "perf_counter_ns", side_effect=lambda: clock_ns
                ),
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(
                    benchmark, "process_resource_counters", return_value={}
                ) as counters,
            ):
                queues.return_value.get.side_effect = read_chunk
                result = benchmark.capture_snapshot(
                    ["openvmm"],
                    snapshot,
                    processors=1,
                    timeout=5,
                    snapshot_profile=profiled,
                    profile_sink=profiles,
                )

        self.assertEqual(result, (3.0, delay_ms + 23.0, 1.0, 1024))
        self.assertEqual(interaction.call_args.args[0], ["openvmm"])
        self.assertEqual(
            interaction.call_args.args[1][benchmark.SNAPSHOT_PROFILE_ENV], "1"
        )
        self.assertIs(interaction.call_args.kwargs["separate_stderr"], True)
        self.assertEqual(writes[1:], [b"nvx-snapshot\n"])
        self.assertIn(
            b"echo OPENVMM-SNAPSHOT-RESTORE-OK\nnvx-exit 0\n",
            writes[0],
        )
        if profiled:
            phases = benchmark.summarize_lifecycle_profiles(profiles)["phases"]
            self.assertEqual(phases["capture.snapshot_generation"]["p50_ms"], result[0])
            self.assertEqual(
                phases["capture.console_command_round_trip"]["p50_ms"],
                delay_ms + 20.0,
            )
        else:
            counters.assert_not_called()
            self.assertEqual(profiles, [])

    # OpenVMM's capture profile records from a Linux/MSHV acceptance run, in
    # which the records reached the shared terminal in the middle of the
    # guest's dispatch marker line.
    INCIDENT_PID = 621736
    INCIDENT_CAPTURE_PROFILE = (
        ("input_gate", 1, 263787, 1422094434, ""),
        ("vp_stop_at_io_boundary", 1, 120994, 1422235827, ""),
        ("quiesce", 1, 393680, 1422716902, ""),
        ("save_state", 1, 647567, 1423376968, ""),
        ("mapped_memory_flush", 1, 2700, 1423391067, ""),
        ("memory_handle_flush", 1, 1400, 1423507161, " logical_bytes=134217728"),
        (
            "publication_state",
            1,
            21499,
            1423577158,
            " logical_bytes=2455 allocated_bytes=4096",
        ),
        (
            "publication_manifest",
            1,
            8899,
            1423643854,
            " logical_bytes=1454 allocated_bytes=4096",
        ),
        (
            "publication_memory",
            1,
            39798,
            1423694652,
            " logical_bytes=134217728 allocated_bytes=65011712",
        ),
        ("publication_staging_sync", 1, 3000, 1423709951, ""),
        ("publication_commit", 1, 12700, 1423734150, ""),
        ("publication_parent_sync", 1, 2700, 1423748449, ""),
        ("publication", 0, 235088, 1423760848, " logical_bytes=134217728"),
    )

    @classmethod
    def incident_capture_records(cls) -> bytes:
        return b"".join(
            (
                f"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture phase={phase} "
                f"exclusive={exclusive} duration_ns={duration} "
                f"process_elapsed_ns={elapsed} pid={cls.INCIDENT_PID}{extra}\n"
            ).encode()
            for phase, exclusive, duration, elapsed, extra in (
                cls.INCIDENT_CAPTURE_PROFILE
            )
        )

    def test_capture_observes_dispatch_marker_split_by_profile_records(self):
        records = self.incident_capture_records()
        committed = b"INFO microVM snapshot committed; terminating source process\n"
        snapshot_requested = threading.Event()
        console_split = threading.Event()
        stderr_written = threading.Event()

        class FakeProcess:
            pid = BenchmarkTests.INCIDENT_PID
            returncode = 0

            def poll(self) -> None:
                return None

            def wait(self) -> int:
                return 0

        class FakeInteraction:
            def __init__(self, snapshot: Path) -> None:
                self.process = FakeProcess()
                self.snapshot = snapshot
                self.writes: list[bytes] = []

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.BOOT_MARKER + b"\r\n")
                chunks.put(benchmark.SMP_PROBE_COMPLETION_MARKER + b"\r\n")
                if snapshot_requested.wait(5):
                    # The console relay delivers the dispatch line in two
                    # writes, around OpenVMM's capture records on stderr.
                    chunks.put(b"nvx-snapshot\r\nNVX-S")
                    console_split.set()
                    if stderr_written.wait(5):
                        chunks.put(b"NAPSHOT-DISPATCHED\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if console_split.wait(5):
                    chunks.put(records)
                    self.snapshot.mkdir()
                    chunks.put(committed)
                    stderr_written.set()
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                self.writes.append(data)
                if data == b"nvx-snapshot\n":
                    snapshot_requested.set()

            def close(self) -> None:
                pass

        profiles: list[dict[str, object]] = []
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "snapshot"
            log_path = Path(temporary) / "capture.log"
            interaction = FakeInteraction(snapshot)
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=interaction
                ) as spawn,
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(benchmark, "process_resource_counters", return_value={}),
                patch.object(benchmark, "terminate"),
            ):
                result = benchmark.capture_snapshot(
                    ["openvmm"],
                    snapshot,
                    processors=1,
                    timeout=10,
                    snapshot_profile=True,
                    profile_sink=profiles,
                    log_path=log_path,
                )
            log = log_path.read_bytes()

        self.assertIs(spawn.call_args.kwargs["separate_stderr"], True)
        self.assertEqual(result[0], 1_903_503 / 1_000_000)
        self.assertEqual(result[3], 1024)
        self.assertEqual(interaction.writes[1:], [b"nvx-snapshot\n"])
        sample = cast(list[dict[str, object]], profiles[0]["records"])
        self.assertEqual(
            [record["phase"] for record in sample if record["source"] == "openvmm"],
            [phase for phase, *_ in self.INCIDENT_CAPTURE_PROFILE],
        )
        self.assertTrue(
            benchmark.contains_output_line(
                log, benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER
            )
        )
        for line in [*records.splitlines(), committed.removesuffix(b"\n")]:
            self.assertTrue(benchmark.contains_output_line(log, line), line)

    def test_capture_samples_storage_from_the_request_through_source_exit(self):
        records = self.incident_capture_records()
        snapshot_requested = threading.Event()

        class FakeProcess:
            pid = BenchmarkTests.INCIDENT_PID
            returncode = 0

            def poll(self) -> None:
                return None

            def wait(self) -> int:
                return 0

        class FakeInteraction:
            def __init__(self, snapshot: Path) -> None:
                self.process = FakeProcess()
                self.snapshot = snapshot
                self.writes: list[bytes] = []

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.BOOT_MARKER + b"\r\n")
                chunks.put(benchmark.SMP_PROBE_COMPLETION_MARKER + b"\r\n")
                if snapshot_requested.wait(5):
                    chunks.put(
                        b"nvx-snapshot\r\n"
                        + benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER
                        + b"\r\n"
                    )
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if snapshot_requested.wait(5):
                    chunks.put(records)
                    self.snapshot.mkdir()
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                self.writes.append(data)
                if data == b"nvx-snapshot\n":
                    snapshot_requested.set()

            def close(self) -> None:
                pass

        class FakeTelemetry:
            def __init__(self, interaction: FakeInteraction) -> None:
                self.interaction = interaction
                self.events: list[str] = []
                self.windows: dict[str, tuple[int, int]] = {}

            def start(self) -> None:
                self.events.append(f"start after {len(self.interaction.writes)}")

            def stop(self) -> None:
                self.events.append("stop")

            def result(self, windows: dict[str, tuple[int, int]]) -> dict[str, object]:
                self.events.append("result")
                self.windows = windows
                return {"phases": {}}

        for retained in (True, False):
            with (
                self.subTest(retained=retained),
                tempfile.TemporaryDirectory() as temporary,
            ):
                snapshot_requested.clear()
                snapshot = Path(temporary) / "snapshot"
                interaction = FakeInteraction(snapshot)
                telemetry = FakeTelemetry(interaction)
                profiles: list[dict[str, object]] = []
                with (
                    patch.object(
                        benchmark, "InteractiveProcess", return_value=interaction
                    ),
                    patch.object(benchmark, "_try_peak_rss", return_value=1024),
                    patch.object(
                        benchmark, "process_resource_counters", return_value={}
                    ),
                    patch.object(benchmark, "terminate"),
                    patch.object(
                        benchmark.host_telemetry,
                        "create_storage_telemetry",
                        return_value=telemetry,
                    ) as create,
                ):
                    benchmark.capture_snapshot(
                        ["openvmm"],
                        snapshot,
                        processors=1,
                        timeout=10,
                        snapshot_profile=True,
                        profile_sink=profiles if retained else None,
                    )

                if not retained:
                    # Warmups keep no profile, so they sample nothing.
                    create.assert_not_called()
                    continue
                volumes, origin = create.call_args.args
                self.assertEqual(volumes["scratch"], Path(temporary))
                self.assertEqual(volumes["workspace"], BuildConstants.REPO_ROOT)
                self.assertIsInstance(origin, int)
                # Sampling starts after the SMP probe script, before the request.
                self.assertEqual(
                    telemetry.events, ["start after 1", "stop", "result", "stop"]
                )
                self.assertEqual(interaction.writes[1:], [b"nvx-snapshot\n"])
                flush_start, flush_end = telemetry.windows[
                    "capture.mapped_memory_flush"
                ]
                self.assertEqual(flush_end - flush_start, 2700)
                generation_start, generation_end = telemetry.windows[
                    "capture.snapshot_generation"
                ]
                self.assertEqual(generation_end - generation_start, 1_903_503)
                self.assertEqual(profiles[0]["storage_telemetry"], {"phases": {}})

    def test_profile_phase_windows_use_the_tightest_clock_offset(self):
        records: list[dict[str, object]] = [
            {
                "operation": "capture",
                "phase": "save_state",
                "duration_ns": 1_000,
                "process_elapsed_ns": 5_000,
                "observer_elapsed_ns": 20_500,
                "source": "openvmm",
            },
            {
                "operation": "capture",
                "phase": "mapped_memory_flush",
                "duration_ns": 900_000,
                "process_elapsed_ns": 905_000,
                "observer_elapsed_ns": 920_000,
                "source": "openvmm",
            },
            {
                "operation": "capture",
                "phase": "snapshot_generation",
                "duration_ns": 950_000,
                "process_elapsed_ns": 955_000,
                "source": "openvmm_clock",
            },
            {
                "operation": "capture",
                "phase": "source_teardown",
                "duration_ns": 9_000,
                "observer_elapsed_ns": 990_000,
                "source": "benchmark_observer",
            },
        ]

        # The save_state record arrived 0.5 us late, so the flush record's
        # 15 us offset bounds OpenVMM's clock.
        self.assertEqual(
            benchmark.snapshot_profile_phase_windows(records),
            {
                "capture.save_state": (19_000, 20_000),
                "capture.mapped_memory_flush": (20_000, 920_000),
                "capture.snapshot_generation": (20_000, 970_000),
                "capture.source_teardown": (981_000, 990_000),
            },
        )

    def test_capture_telemetry_line_describes_the_ram_flush_window(self):
        records = [
            {
                "operation": "capture",
                "phase": "mapped_memory_flush",
                "duration_ns": 1_165_314_800,
            }
        ]
        sample: dict[str, object] = {
            "records": records,
            "storage_telemetry": {
                "volumes": {"F:": ["scratch"]},
                "phases": {
                    "capture.mapped_memory_flush": {
                        "volumes": {
                            "F:": {
                                "write_mib_per_second": 128.44,
                                "average_write_latency_ms": 31.2,
                                "average_queue_length": 3.94,
                                "busy_percent": 99.4,
                            }
                        },
                        "cpu": {"busy_percent": 6.2},
                    }
                },
            },
        }

        self.assertEqual(
            benchmark.describe_capture_telemetry(sample),
            "mapped_memory_flush 1165.315 ms: scratch F: write 128.4 MiB/s at "
            "31.2 ms, queue 3.9, 99% busy; CPU 6% busy",
        )
        self.assertIsNone(benchmark.describe_capture_telemetry({"records": records}))

    def test_measure_once_observes_restore_marker_split_by_profile_record(self):
        record = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=guest_repair_gate exclusive=0 duration_ns=1500 "
            b"process_elapsed_ns=90000000 pid=4321\n"
        )
        console_split = threading.Event()
        stderr_written = threading.Event()

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(b"OPENVMM-SNAP")
                console_split.set()
                if stderr_written.wait(5):
                    chunks.put(b"SHOT-RESTORE-OK\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if console_split.wait(5):
                    chunks.put(record)
                    stderr_written.set()
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        profiles: list[dict[str, object]] = []
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "restore.log"
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=FakeInteraction()
                ) as spawn,
                patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
                patch.object(benchmark, "wait_for_process_exit", return_value=0),
                patch.object(benchmark, "process_resource_counters", return_value={}),
            ):
                result = benchmark.measure_once(
                    ["openvmm"],
                    environment={},
                    timeout=10,
                    marker=benchmark.RESTORE_MARKER,
                    marker_must_be_line=True,
                    guest_exit_prequeued=True,
                    snapshot_profile=True,
                    profile_sink=profiles,
                    log_path=log_path,
                )
            log = log_path.read_bytes()

        self.assertIs(spawn.call_args.kwargs["separate_stderr"], True)
        self.assertEqual(result[1], 1024)
        records = cast(list[dict[str, object]], profiles[0]["records"])
        self.assertIn("guest_repair_gate", [item["phase"] for item in records])
        self.assertTrue(benchmark.contains_output_line(log, benchmark.RESTORE_MARKER))
        self.assertTrue(benchmark.contains_output_line(log, record.removesuffix(b"\n")))

    def test_measure_once_keeps_profile_records_observed_after_the_marker(self):
        records = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=startup "
            b"phase=worker_launch exclusive=0 duration_ns=2000000 "
            b"process_elapsed_ns=5000000 pid=4321\n"
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=device_start exclusive=1 duration_ns=300000 "
            b"process_elapsed_ns=80000000 pid=4321\n"
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=guest_repair_gate exclusive=0 duration_ns=1500 "
            b"process_elapsed_ns=90000000 pid=4321\n"
        )
        torn_down = threading.Event()

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.RESTORE_MARKER + b"\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                # The stderr reader falls behind: OpenVMM's records arrive
                # only once the guest marker has been handled.
                if torn_down.wait(5):
                    chunks.put(records[:100])
                    chunks.put(records[100:])
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        def counters(_pid: int) -> dict[str, int]:
            return {} if torn_down.is_set() else {"rss_bytes": 7}

        def wait_for_exit(_process: object, _timeout: float) -> int:
            torn_down.set()
            return 0

        profiles: list[dict[str, object]] = []
        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            patch.object(benchmark, "wait_for_process_exit", side_effect=wait_for_exit),
            patch.object(benchmark, "process_resource_counters", side_effect=counters),
        ):
            benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=10,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
                snapshot_profile=True,
                profile_sink=profiles,
            )

        by_phase = {
            str(record["phase"]): record
            for record in cast(list[dict[str, object]], profiles[0]["records"])
        }
        self.assertLessEqual(
            {"process_startup", "worker_launch", "device_start", "guest_repair_gate"},
            set(by_phase),
        )
        self.assertEqual(by_phase["process_startup"]["duration_ns"], 3_000_000)
        # The device_start observation followed the marker, so it cannot bound
        # the interval to readiness.
        self.assertNotIn("resume_to_readiness", by_phase)
        readiness = by_phase["process_launch_to_readiness"]
        self.assertEqual(readiness["host_counters"], {"rss_bytes": 7})

    def test_warm_snapshot_cache_reads_every_artifact(self):
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary)
            for index, name in enumerate(benchmark.SNAPSHOT_FILENAMES):
                (snapshot / name).write_bytes(bytes([index]) * 32)

            benchmark.warm_snapshot_artifacts(snapshot)

    def test_snapshot_artifact_validation_preserves_error_prefix(self):
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary)
            for prefix in ("snapshot", "console/active snapshot"):
                with self.subTest(prefix=prefix):
                    with self.assertRaises(RuntimeError) as context:
                        benchmark._require_snapshot_artifacts(snapshot, prefix)

                    self.assertEqual(
                        str(context.exception),
                        f"{prefix} did not publish {snapshot / 'manifest.bin'}",
                    )

    def test_network_snapshot_restore_selects_portable_profile(self):
        command = benchmark.snapshot_restore_command(
            Path("openvmm"),
            "mshv",
            Path("snapshot"),
            processors=4,
            network_profile="portable",
        )

        self.assertEqual(
            command[2:8],
            ["--machine", "microvm", "--processors", "4", "--hypervisor", "mshv"],
        )

        self.assertEqual(
            command[-4:],
            [
                "--network-profile",
                "portable",
                "--virtio-console",
                "console",
            ],
        )

    def test_snapshot_restore_command_separates_capacity_from_online_target(self):
        command = benchmark.snapshot_restore_command(
            Path("openvmm"),
            "kvm",
            Path("snapshot"),
            processors=8,
            restore_processors=4,
        )

        self.assertEqual(command[command.index("--processors") + 1], "8")
        self.assertEqual(command[command.index("--restore-processors") + 1], "4")
        # The restore packet gives every restore fresh entropy.
        self.assertNotIn("--restore-entropy", command)

    def test_snapshot_restore_command_sets_memory_target_separately(self):
        command = benchmark.snapshot_restore_command(
            Path("openvmm"),
            "whp",
            Path("snapshot"),
            restore_memory_mib=2048,
        )

        self.assertNotIn("--memory", command)
        self.assertEqual(command[command.index("--restore-memory") + 1], "2048M")

    def test_memory_online_latency_parser_validates_added_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "restore.log"
            log.write_bytes(
                b"NVX-MEMORY-ONLINE-OK: added_bytes=536870912 "
                b"memtotal_kib=1000000 elapsed_us=12345\n"
            )
            self.assertEqual(
                benchmark._memory_online_elapsed_ms(log, 536870912),
                12.345,
            )
            with self.assertRaisesRegex(RuntimeError, "expected 1"):
                benchmark._memory_online_elapsed_ms(log, 1)

    def test_benchmark_snapshot_restore_propagates_processors(self):
        args = argparse.Namespace(
            processors=4,
            network_profile=None,
            warmups=1,
            runs=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        result = cast(benchmark.BenchmarkResult, {"p50_ms": 10.0})
        with patch.object(benchmark, "benchmark", return_value=result) as run:
            self.assertIs(
                benchmark.benchmark_snapshot_restore(
                    args,
                    Path("openvmm"),
                    "kvm",
                    ["unused-cold-command"],
                    snapshot_path=Path("snapshot"),
                ),
                result,
            )

        command = run.call_args.args[0]
        self.assertEqual(
            command[2:8],
            ["--machine", "microvm", "--processors", "4", "--hypervisor", "kvm"],
        )
        self.assertTrue(run.call_args.kwargs["marker_must_be_line"])
        self.assertTrue(run.call_args.kwargs["guest_exit_prequeued"])

    def test_snapshot_request_triggers_waiting_capture_controller(self):
        script = benchmark.snapshot_request_script(
            4,
            teardown_mode="guest-exit",
        )
        self.assertEqual(script, "nvx-snapshot\n")

        no_controller = benchmark.snapshot_request_script(
            None,
            teardown_mode="guest-exit",
        )
        self.assertEqual(
            no_controller,
            "echo NVX-SNAPSHOT-DISPATCHED\n"
            "nvx-snapshot\n"
            "echo OPENVMM-SNAPSHOT-RESTORE-OK\n"
            "nvx-exit 0\n",
        )
        self.assertNotIn(
            "nvx-exit",
            benchmark.snapshot_request_script(None, teardown_mode="host-terminate"),
        )

    def test_prepare_snapshot_capture_stages_waiting_controller(self):
        script = benchmark.prepare_snapshot_capture_script(
            4,
            teardown_mode="guest-exit",
            network_gateway="10.0.0.1",
            ioapic_irq=16,
        )

        self.assertTrue(
            script.startswith(
                f"cat >{benchmark.SMP_PROBE_PATH} <<'NVX_SMP_PROBE_SCRIPT'\n"
            )
        )
        self.assertIn('ping -c 2 -W 1 "10.0.0.1"', script)
        self.assertIn("NVX-SMP-PROBE-OK", script)
        self.assertIn(
            f"cat >{benchmark.SNAPSHOT_CAPTURE_PATH} <<'NVX_SNAPSHOT_CAPTURE_SCRIPT'\n",
            script,
        )
        self.assertIn("IFS= read -r trigger\n", script)
        self.assertIn("echo NVX-SNAPSHOT-DISPATCHED\n", script)
        self.assertIn(
            "/sbin/nvx-snapshot\necho OPENVMM-SNAPSHOT-RESTORE-OK\nnvx-exit 0\n",
            script,
        )
        host_terminated = benchmark.prepare_snapshot_capture_script(
            4, teardown_mode="host-terminate"
        )
        self.assertNotIn("nvx-exit 0", host_terminated)
        self.assertTrue(
            script.endswith(
                "NVX_SNAPSHOT_CAPTURE_SCRIPT\n"
                f"chmod +x {benchmark.SMP_PROBE_PATH} "
                f"{benchmark.SNAPSHOT_CAPTURE_PATH}\n"
                f"{benchmark.SNAPSHOT_CAPTURE_PATH}\n"
            )
        )

    def test_prepare_snapshot_capture_runs_a_pre_capture_script_after_the_trigger(
        self,
    ):
        script = benchmark.prepare_snapshot_capture_script(
            1,
            teardown_mode="guest-exit",
            pre_capture_script="echo prepare\n",
            post_restore_script="echo repair\n",
        )

        controller = script.split("<<'NVX_SNAPSHOT_CAPTURE_SCRIPT'\n", 1)[1]
        controller = controller.split("NVX_SNAPSHOT_CAPTURE_SCRIPT\n", 1)[0]
        self.assertIn(
            "IFS= read -r trigger\n"
            'if [ "$trigger" != nvx-snapshot ]; then\n'
            "  /sbin/nvx-exit 90\n"
            "  exit 90\n"
            "fi\n"
            f"{benchmark.SNAPSHOT_PRE_CAPTURE_PATH}\n"
            "echo NVX-SNAPSHOT-DISPATCHED\n"
            "/sbin/nvx-snapshot\n"
            f"{benchmark.SNAPSHOT_POST_RESTORE_PATH}\n"
            "echo OPENVMM-SNAPSHOT-RESTORE-OK\n",
            controller,
        )
        self.assertTrue(
            script.endswith(
                f"cat >{benchmark.SNAPSHOT_PRE_CAPTURE_PATH} "
                "<<'NVX_PRE_CAPTURE_SCRIPT'\n"
                "echo prepare\nNVX_PRE_CAPTURE_SCRIPT\n"
                f"chmod +x {benchmark.SNAPSHOT_PRE_CAPTURE_PATH}\n"
                f"cat >{benchmark.SNAPSHOT_POST_RESTORE_PATH} "
                "<<'NVX_POST_RESTORE_SCRIPT'\n"
                "echo repair\nNVX_POST_RESTORE_SCRIPT\n"
                f"chmod +x {benchmark.SNAPSHOT_POST_RESTORE_PATH}\n"
                f"{benchmark.SNAPSHOT_CAPTURE_PATH}\n"
            ),
            script,
        )

    def test_capture_does_not_wait_for_a_clocksource(self):
        # The time ABI registers the tsc clocksource at device_initcall on
        # every backend, so no capture waits for Linux to leave tsc-early.
        script = benchmark.prepare_snapshot_capture_script(
            1, teardown_mode="guest-exit"
        )
        probe = script.split("<<'NVX_SMP_PROBE_SCRIPT'\n", 1)[1]
        probe = probe.split("NVX_SMP_PROBE_SCRIPT\n", 1)[0]
        self.assertEqual(probe, benchmark.smp_probe_script(1, exit_guest=False))
        for text in ("clocksource", "tsc-early", "clock_tries"):
            self.assertNotIn(text, script)
        self.assertFalse(hasattr(benchmark, "stable_clocksource_wait_script"))

    def test_device_restore_marker_and_trace_parsers(self):
        marker = benchmark.parse_device_restore_marker(
            "NVX-VIRTIO-RESTORE-PROBE phase=io-success device=net mode=deferred\r"
        )
        self.assertEqual(
            marker,
            {"phase": "io-success", "device": "net", "mode": "deferred"},
        )
        self.assertIsNone(benchmark.parse_device_restore_marker("unrelated"))

        event = benchmark.parse_virtio_restore_event(
            "DEBUG virtio_restore: virtio restore lifecycle "
            'event: "queue_start", device_type: 0x1a, trigger: "driver-ok", '
            "queue_index: 0, restored_progress: true, success: true"
        )
        self.assertEqual(
            event,
            {
                "event": "queue_start",
                "device_type": 0x1A,
                "trigger": "driver-ok",
                "queue_index": 0,
                "restored_progress": True,
                "success": True,
            },
        )
        self.assertIsNone(benchmark.parse_virtio_restore_event("unrelated"))

    def test_device_restore_commands_use_fixed_attachments(self):
        common = (
            Path("openvmm"),
            "whp",
            Path("vmlinux"),
            Path("initrd"),
            256,
        )
        console_boot, console_restore = benchmark.device_restore_commands(
            *common, "console", "deferred"
        )
        self.assertIn(
            "nvx_virtio_restore_probe=console,deferred",
            console_boot[console_boot.index("--cmdline") + 1],
        )
        self.assertEqual(console_boot[-2:], ["--virtio-console", "console"])
        self.assertEqual(console_restore[-2:], ["--virtio-console", "console"])

        net_boot, net_restore = benchmark.device_restore_commands(
            *common, "net", "active"
        )
        self.assertIn("--network-profile", net_boot)
        self.assertIn("--network-profile", net_restore)

        fs_boot, fs_restore = benchmark.device_restore_commands(
            *common,
            "virtiofs",
            "deferred",
            host_directory=Path("host-share"),
        )
        self.assertIn("--mount", fs_boot)
        self.assertIn("--mount", fs_restore)

    def test_device_restore_sample_rejects_premature_activation(self):
        markers = [
            {
                "phase": "restore-ready",
                "device": "net",
                "mode": "deferred",
                "observer_elapsed_ms": "8.0",
            },
            {
                "phase": "trigger",
                "device": "net",
                "mode": "deferred",
                "observer_elapsed_ms": "10.0",
            },
        ]
        events: list[dict[str, object]] = [
            {
                "event": "restore_staged",
                "device_type": 1,
                "trigger": "inactive-start",
                "queue_index": -1,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 5.0,
            },
            {
                "event": "private_state_apply",
                "device_type": 1,
                "trigger": "kick",
                "queue_index": -1,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 9.0,
            },
            {
                "event": "kick_staged",
                "device_type": 1,
                "trigger": "kick",
                "queue_index": 0,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 10.5,
            },
        ]
        for queue_index in range(2):
            events.append(
                {
                    "event": "queue_start",
                    "device_type": 1,
                    "trigger": "driver-ok",
                    "queue_index": queue_index,
                    "restored_progress": False,
                    "success": True,
                    "observer_elapsed_ms": 11.0,
                }
            )
        events.append(
            {
                "event": "kick_dispatch",
                "device_type": 1,
                "trigger": "driver-ok",
                "queue_index": 0,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 12.0,
            }
        )
        sample = cast(
            benchmark.DeviceRestoreSample,
            {
                "process_launch_to_ready_ms": 8.0,
                "trigger_to_first_successful_io_ms": 2.0,
                "peak_rss_bytes": 1,
                "guest_markers": markers,
                "events": events,
                "queue_start_count": 0,
                "staged_kick_dispatch_count": 0,
                "stale_premature_callback_count": 0,
                "log": "sample.log",
            },
        )
        with self.assertRaisesRegex(RuntimeError, "premature"):
            benchmark.validate_device_restore_sample(sample, "net", "deferred")
        events[1]["observer_elapsed_ms"] = 10.25
        benchmark.validate_device_restore_sample(sample, "net", "deferred")
        self.assertEqual(sample["queue_start_count"], 2)
        self.assertEqual(sample["staged_kick_dispatch_count"], 1)
        self.assertEqual(sample["stale_premature_callback_count"], 0)

    def test_device_restore_profile_writes_standalone_result(self):
        args = argparse.Namespace(
            processors=1,
            restore_devices=["console"],
            restore_modes=["active"],
            net=None,
            memory_mib=128,
            timeout=1.0,
            warmups=1,
            runs=2,
            platform="windows-whp-baremetal",
        )

        def sample() -> benchmark.DeviceRestoreSample:
            return {
                "process_launch_to_ready_ms": 10.0,
                "trigger_to_first_successful_io_ms": 2.0,
                "peak_rss_bytes": 1024,
                "guest_markers": [
                    {
                        "phase": "restore-ready",
                        "device": "console",
                        "mode": "active",
                        "observer_elapsed_ms": "10.0",
                    },
                    {
                        "phase": "trigger",
                        "device": "console",
                        "mode": "active",
                        "observer_elapsed_ms": "11.0",
                    },
                ],
                "events": [
                    {
                        "event": "restore_staged",
                        "device_type": 3,
                        "trigger": "active-start",
                        "queue_index": -1,
                        "restored_progress": False,
                        "success": True,
                    },
                    {
                        "event": "private_state_apply",
                        "device_type": 3,
                        "trigger": "active-start",
                        "queue_index": -1,
                        "restored_progress": False,
                        "success": True,
                    },
                    {
                        "event": "kick_dispatch",
                        "device_type": 3,
                        "trigger": "kick",
                        "queue_index": 1,
                        "restored_progress": False,
                        "success": True,
                    },
                    *[
                        {
                            "event": "queue_start",
                            "device_type": 3,
                            "trigger": "active-start",
                            "queue_index": queue_index,
                            "restored_progress": True,
                            "success": True,
                        }
                        for queue_index in range(2)
                    ],
                ],
                "queue_start_count": 0,
                "staged_kick_dispatch_count": 0,
                "stale_premature_callback_count": 0,
                "log": "",
            }

        def restore_sample(
            *_args: object, **_kwargs: object
        ) -> benchmark.DeviceRestoreSample:
            return sample()

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            with (
                patch.object(
                    benchmark,
                    "capture_device_restore_snapshot",
                    return_value=[
                        {
                            "phase": "capture-ready",
                            "device": "console",
                            "mode": "active",
                        }
                    ],
                ) as capture,
                patch.object(
                    benchmark,
                    "run_device_restore_sample",
                    side_effect=restore_sample,
                ) as restore,
            ):
                path = benchmark.benchmark_device_restore_profile(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "whp",
                    output_dir,
                )

            self.assertEqual(capture.call_count, 1)
            self.assertEqual(restore.call_count, 3)
            document = json.loads(path.read_text(encoding="utf-8"))
            self.assertTrue(document["non_canonical"])
            self.assertTrue(document["assertions"]["zero_stale_premature_callbacks"])
            scenario = document["scenarios"]["console-active"]
            self.assertEqual(
                scenario["process_launch_to_ready_ms"]["samples"], [10.0, 10.0]
            )
            self.assertEqual(scenario["process_launch_to_ready_ms"]["p50"], 10.0)
            self.assertEqual(scenario["queue_start_counts"], [2, 2])

    def test_device_restore_profile_writes_lf_only_virtiofs_seed(self):
        args = argparse.Namespace(
            processors=1,
            restore_devices=["virtiofs"],
            restore_modes=["active"],
            net=None,
            memory_mib=128,
            timeout=1.0,
            warmups=1,
            runs=1,
            platform="windows-whp-baremetal",
        )
        observed_seeds: list[bytes] = []
        host_directories: list[Path] = []

        def commands(*_args: object, **kwargs: object) -> tuple[list[str], list[str]]:
            host_directory = cast(Path, kwargs["host_directory"])
            host_directories.append(host_directory)
            observed_seeds.append(
                (host_directory / ".nvx-virtio-restore-host-seed").read_bytes()
            )
            return ["boot"], ["restore"]

        sample = cast(
            benchmark.DeviceRestoreSample,
            {
                "process_launch_to_ready_ms": 1.0,
                "trigger_to_first_successful_io_ms": 1.0,
                "peak_rss_bytes": 1,
                "guest_markers": [],
                "events": [],
                "queue_start_count": 0,
                "staged_kick_dispatch_count": 0,
                "stale_premature_callback_count": 0,
                "log": "sample.log",
            },
        )

        def run_sample(
            *_args: object, **_kwargs: object
        ) -> benchmark.DeviceRestoreSample:
            (host_directories[-1] / ".nvx-virtio-restore-guest-result").write_bytes(
                benchmark.VIRTFS_GUEST_TO_HOST
            )
            return sample

        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(
                    benchmark, "device_restore_commands", side_effect=commands
                ),
                patch.object(
                    benchmark,
                    "capture_device_restore_snapshot",
                    return_value=[],
                ),
                patch.object(
                    benchmark,
                    "run_device_restore_sample",
                    side_effect=run_sample,
                ),
                patch.object(
                    benchmark,
                    "summarize_device_restore_samples",
                    return_value={"stale_premature_callback_count": 0},
                ),
            ):
                benchmark.benchmark_device_restore_profile(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "whp",
                    Path(temporary),
                )

        self.assertEqual(observed_seeds, [benchmark.VIRTFS_HOST_TO_GUEST])

    def test_device_restore_profile_rejects_reused_output(self):
        args = argparse.Namespace(
            suite="device-restore-profile",
            output=None,
            output_dir=None,
            platform="linux-kvm-baremetal",
        )
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            (output_dir / "stale.log").write_text("stale", encoding="ascii")
            args.output_dir = output_dir
            with self.assertRaisesRegex(FileExistsError, "empty output directory"):
                benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "kvm",
                )

    def test_snapshot_capture_runs_smp_probe_for_selected_count(self):
        args = argparse.Namespace(
            warmups=0,
            runs=1,
            timeout=1.0,
            processors=8,
            teardown_mode="guest-exit",
        )
        with patch.object(
            benchmark,
            "capture_snapshot",
            return_value=(1.0, 1.0, 1.0, 1024),
        ) as capture:
            benchmark.benchmark_snapshot_capture(args, ["openvmm"])

        self.assertNotIn("backend", capture.call_args.kwargs)
        self.assertEqual(capture.call_args.kwargs["processors"], 8)
        self.assertEqual(capture.call_args.kwargs["teardown_mode"], "guest-exit")

    def test_shell_snapshot_restore_suite_only_measures_restore(self):
        args = argparse.Namespace(
            processors=8,
            shell_memories=[512],
            warmups=1,
            runs=5,
            timeout=40.0,
            teardown_mode="guest-exit",
            network_profile=None,
            snapshot_profile=True,
        )
        result = cast(
            benchmark.BenchmarkResult,
            {
                "samples_ms": [10.0, 12.0],
                "profile": {
                    "schema_version": 1,
                    "raw_samples": [],
                    "phases": {
                        "startup.partition_build": {
                            "exclusive": True,
                            "source": "openvmm",
                            "samples_ms": [1.0],
                            "p50_ms": 1.0,
                            "p95_ms": 1.0,
                            "min_ms": 1.0,
                            "max_ms": 1.0,
                        }
                    },
                },
            },
        )
        with (
            patch.object(benchmark, "capture_snapshot") as capture,
            patch.object(benchmark, "benchmark", return_value=result) as run,
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            benchmark.benchmark_shell_snapshot_restore_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
            )

        capture.assert_called_once()
        capture_command = capture.call_args.args[0]
        self.assertNotIn("shellsnap", capture_command)
        self.assertEqual(capture.call_args.kwargs["processors"], 8)
        self.assertEqual(capture.call_args.kwargs["teardown_mode"], "guest-exit")
        run.assert_called_once()
        restore_command = run.call_args.args[0]
        self.assertIn("--restore-snapshot", restore_command)
        self.assertEqual(
            restore_command[restore_command.index("--processors") + 1], "8"
        )
        self.assertIn("== 512 MiB ==", output.getvalue())
        self.assertIn("snapshot restore", output.getvalue())
        self.assertIn("  restore samples     : 10.000, 12.000 ms\n", output.getvalue())
        self.assertNotIn("cold boot", output.getvalue())
        self.assertIn('marker : "OPENVMM-SNAPSHOT-RESTORE-OK"', output.getvalue())
        self.assertTrue(run.call_args.kwargs["snapshot_profile"])
        self.assertEqual(run.call_args.kwargs["marker"], benchmark.RESTORE_MARKER)
        self.assertTrue(run.call_args.kwargs["marker_must_be_line"])
        self.assertTrue(run.call_args.kwargs["guest_exit_prequeued"])
        self.assertIn(
            "shell-snapshot-restore/whp/8vcpu/512-mib lifecycle phases:",
            output.getvalue(),
        )

    def test_restore_vcpu_suite_reuses_one_capacity_snapshot_for_matrix(self):
        args = argparse.Namespace(
            processors=8,
            memory_mib=512,
            warmups=1,
            runs=5,
            timeout=40.0,
            teardown_mode="guest-exit",
            snapshot_profile=True,
        )
        result = cast(
            benchmark.BenchmarkResult,
            {
                "samples_ms": [10.0, 12.0],
                "peak_rss_samples_bytes": [1024, 2048],
                "profile": {
                    "schema_version": 1,
                    "raw_samples": [],
                    "phases": {
                        "restore.saved_state_restore": {
                            "exclusive": True,
                            "source": "openvmm",
                            "samples_ms": [2.0],
                            "p50_ms": 2.0,
                            "p95_ms": 2.0,
                            "min_ms": 2.0,
                            "max_ms": 2.0,
                        }
                    },
                },
            },
        )
        with (
            patch.object(benchmark, "capture_automatic_snapshot") as capture,
            patch.object(benchmark, "benchmark", return_value=result) as run,
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            benchmark.benchmark_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "mshv",
            )

        capture.assert_called_once()
        self.assertEqual(capture.call_args.kwargs["timeout"], args.timeout)
        capture_command = capture.call_args.args[0]
        self.assertEqual(
            capture_command[capture_command.index("--processors") + 1], "8"
        )
        self.assertTrue(any("maxcpus=1" in argument for argument in capture_command))
        self.assertEqual(run.call_count, 4)
        targets: list[int] = []
        for invocation in run.call_args_list:
            command = invocation.args[0]
            self.assertEqual(invocation.kwargs["timeout"], args.timeout)
            self.assertTrue(invocation.kwargs["snapshot_profile"])
            self.assertEqual(command[command.index("--processors") + 1], "8")
            targets.append(int(command[command.index("--restore-processors") + 1]))
        self.assertEqual(targets, [1, 2, 4, 8])
        self.assertIn("restore-online 8 vCPU", output.getvalue())
        self.assertIn(
            "snapshot-restore-vcpu/mshv/capacity-8/online-1 lifecycle phases:",
            output.getvalue(),
        )

        args.processors = 4
        with self.assertRaisesRegex(ValueError, "requires --processors 8"):
            benchmark.benchmark_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
            )

    def test_platform_restore_vcpu_suite_restores_one_platform_capture(self):
        args = argparse.Namespace(
            processors=8,
            memory_mib=512,
            warmups=1,
            runs=5,
            timeout=40.0,
            teardown_mode="guest-exit",
            snapshot_profile=True,
        )
        result = cast(
            benchmark.BenchmarkResult,
            {
                "samples_ms": [10.0, 12.0],
                "peak_rss_samples_bytes": [1024, 2048],
                "profile": {
                    "schema_version": 1,
                    "raw_samples": [],
                    "phases": {
                        "restore.guest_repair_gate": {
                            "exclusive": False,
                            "source": "openvmm",
                            "samples_ms": [2.0],
                            "p50_ms": 2.0,
                            "p95_ms": 2.0,
                            "min_ms": 2.0,
                            "max_ms": 2.0,
                        }
                    },
                },
            },
        )

        def blocks(command: list[str]) -> list[str]:
            return [
                command[index + 1]
                for index, argument in enumerate(command)
                if argument == "--microvm-sandbox-block"
            ]

        prefix = ("taskset", "-c", "2")
        with (
            patch.object(benchmark, "capture_snapshot") as capture,
            patch.object(benchmark, "benchmark", return_value=result) as run,
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            benchmark.benchmark_platform_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "mshv",
                command_prefix=prefix,
            )

        capture.assert_called_once()
        self.assertEqual(capture.call_args.kwargs["timeout"], args.timeout)
        self.assertEqual(capture.call_args.kwargs["processors"], 1)
        # The restored guest waits for the host after the marker instead of
        # running a prequeued exit, so its peak RSS can be sampled.
        self.assertNotEqual(capture.call_args.kwargs["teardown_mode"], "guest-exit")
        capture_command = capture.call_args.args[0]
        self.assertEqual(tuple(capture_command[:3]), prefix)
        self.assertEqual(
            capture_command[capture_command.index("--processors") + 1], "8"
        )
        # The platform command line takes no tenant tokens, such as quiet or
        # the pre-banner capture's shellsnap.
        self.assertEqual(
            capture_command[capture_command.index("--cmdline") + 1], "maxcpus=1"
        )
        self.assertEqual(
            capture_command[capture_command.index("--snapshot-tier") + 1], "platform"
        )
        capture_blocks = blocks(capture_command)
        self.assertEqual(len(capture_blocks), 2)
        self.assertRegex(capture_blocks[0], r"^distro:file:.+,ro$")
        self.assertRegex(capture_blocks[1], r"^scratch:file:.+(?<!,ro)$")
        self.assertEqual(run.call_count, 4)
        targets: list[int] = []
        for invocation in run.call_args_list:
            command = invocation.args[0]
            self.assertEqual(tuple(command[:3]), prefix)
            self.assertEqual(command[command.index("--processors") + 1], "8")
            self.assertEqual(blocks(command), capture_blocks)
            self.assertEqual(invocation.kwargs["timeout"], args.timeout)
            self.assertEqual(invocation.kwargs["marker"], benchmark.RESTORE_MARKER)
            self.assertTrue(invocation.kwargs["marker_must_be_line"])
            self.assertEqual(invocation.kwargs["teardown_mode"], "guest-exit")
            self.assertFalse(invocation.kwargs.get("guest_exit_prequeued", False))
            self.assertTrue(invocation.kwargs["snapshot_profile"])
            targets.append(int(command[command.index("--restore-processors") + 1]))
        self.assertEqual(targets, [1, 2, 4, 8])
        self.assertIn("platform snapshot restore vCPU activation", output.getvalue())
        self.assertIn("restore-online 8 vCPU", output.getvalue())
        self.assertIn(
            "platform-snapshot-restore-vcpu/mshv/capacity-8/online-1 lifecycle phases:",
            output.getvalue(),
        )

        args.teardown_mode = "host-terminate"
        with (
            patch.object(benchmark, "capture_snapshot"),
            patch.object(benchmark, "benchmark", return_value=result) as run,
            patch("sys.stdout", new_callable=io.StringIO),
        ):
            benchmark.benchmark_platform_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
            )
        self.assertTrue(
            all(
                entry.kwargs["teardown_mode"] == "host-terminate"
                for entry in run.call_args_list
            )
        )

        args.processors = 4
        with self.assertRaisesRegex(ValueError, "requires --processors 8"):
            benchmark.benchmark_platform_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
            )

    def test_platform_restore_vcpu_metadata_records_the_targets(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "platform-snapshot-restore-vcpu",
                "--backend",
                "kvm",
                "--processors",
                "8",
            ]
        )
        benchmark.apply_benchmark_suite_defaults(args)
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(
                benchmark,
                "benchmark_host_provenance",
                return_value={"machine_name": "host"},
            ),
            patch.object(benchmark, "sha256_file", return_value="0" * 64),
            patch.object(benchmark, "_git_revision", return_value="revision"),
            patch.object(benchmark, "_git_status", return_value=[]),
        ):
            path = benchmark.write_benchmark_metadata(
                args,
                Path(temporary),
                Path("openvmm"),
                Path("vmlinux"),
                Path("initramfs.cpio.gz"),
                "kvm",
            )
            document = json.loads(path.read_text(encoding="utf-8"))

        self.assertEqual(document["suite"], "platform-snapshot-restore-vcpu")
        self.assertEqual(document["processors"], 8)
        self.assertEqual(document["restore_processor_targets"], [1, 2, 4, 8])

    def test_snapshot_capture_excludes_warmup_and_retains_last_snapshot(self):
        with tempfile.TemporaryDirectory() as temporary:
            retained = Path(temporary) / "retained"
            observed_paths: list[Path] = []

            def capture(
                _command: object,
                snapshot_path: Path,
                **_kwargs: object,
            ) -> tuple[float, float, float, int]:
                snapshot_path.mkdir()
                observed_paths.append(snapshot_path)
                sample = float(len(observed_paths))
                return sample, sample, sample / 10, len(observed_paths) * 1024

            args = argparse.Namespace(
                warmups=1,
                runs=2,
                timeout=1.0,
                processors=1,
                teardown_mode="guest-exit",
            )
            with patch.object(benchmark, "capture_snapshot", side_effect=capture):
                result = benchmark.benchmark_snapshot_capture(
                    args,
                    ["openvmm"],
                    retained_snapshot_path=retained,
                )

            self.assertEqual(result["samples_ms"], [2.0, 3.0])
            self.assertEqual(result["peak_rss_samples_bytes"], [2048, 3072])
            self.assertEqual(result["peak_rss_p50_bytes"], 2560)
            self.assertEqual(result["peak_rss_max_bytes"], 3072)
            self.assertEqual(observed_paths[-1], retained)
            self.assertTrue(retained.is_dir())

    def test_snapshot_restore_reuses_provided_snapshot(self):
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "snapshot"
            snapshot.mkdir()
            args = argparse.Namespace(
                warmups=1,
                runs=2,
                timeout=3.0,
                teardown_mode="guest-exit",
                network_profile=None,
                processors=1,
            )
            expected = {"p50_ms": 1.0}
            with (
                patch.object(benchmark, "capture_snapshot") as capture,
                patch.object(benchmark, "benchmark", return_value=expected) as run,
            ):
                result = benchmark.benchmark_snapshot_restore(
                    args,
                    Path("openvmm"),
                    "whp",
                    ["openvmm", "--kernel", "vmlinux"],
                    snapshot_path=snapshot,
                )

            self.assertIs(result, expected)
            capture.assert_not_called()
            self.assertIn(str(snapshot), run.call_args.args[0])
            self.assertEqual(run.call_args.kwargs["marker"], benchmark.RESTORE_MARKER)
            self.assertTrue(run.call_args.kwargs["marker_must_be_line"])
            self.assertEqual(run.call_args.kwargs["teardown_mode"], "guest-exit")
            self.assertTrue(run.call_args.kwargs["guest_exit_prequeued"])

    def test_native_e2e_measures_and_reuses_snapshot_capture(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm = root / "openvmm"
            executable = openvmm / "target" / "release" / "openvmm"
            build_dir = root / "build"
            executable.parent.mkdir(parents=True)
            build_dir.mkdir()
            (openvmm / "Cargo.toml").touch()
            executable.touch()
            (build_dir / "vmlinux").touch()
            (build_dir / "initramfs.cpio.gz").touch()
            output = root / "e2e.json"
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "e2e",
                    "--backend",
                    "kvm",
                    "--openvmm-dir",
                    str(openvmm),
                    "--nvx-dir",
                    str(root),
                    "--cpus",
                    "0-2",
                    "--warmups",
                    "1",
                    "--runs",
                    "1",
                    "--skip-build",
                    "--output",
                    str(output),
                ]
            )
            run_result: benchmark.BenchmarkResult = {
                "samples_ms": [10.0],
                "p50_ms": 10.0,
                "p95_ms": 10.0,
                "min_ms": 10.0,
                "max_ms": 10.0,
                "wall_samples_ms": [12.0],
                "wall_p50_ms": 12.0,
                "wall_p95_ms": 12.0,
                "wall_min_ms": 12.0,
                "wall_max_ms": 12.0,
                "peak_rss_samples_bytes": [1024],
                "peak_rss_p50_bytes": 1024,
                "peak_rss_min_bytes": 1024,
                "peak_rss_max_bytes": 1024,
                "peak_rss_remeasured_count": 0,
                "teardown_samples_ms": [2.0],
                "teardown_completed_samples_ms": [2.0],
                "teardown_timeout_count": 0,
                "teardown_timeout_seconds": 5.0,
                "teardown_p50_ms": 2.0,
                "teardown_p95_ms": 2.0,
                "teardown_min_ms": 2.0,
                "teardown_max_ms": 2.0,
            }
            capture_result: benchmark.SnapshotCaptureResult = {
                "samples_ms": [3.0],
                "p50_ms": 3.0,
                "p95_ms": 3.0,
                "min_ms": 3.0,
                "max_ms": 3.0,
                "request_to_publication_samples_ms": [3.0],
                "request_to_publication_p50_ms": 3.0,
                "request_to_publication_p95_ms": 3.0,
                "request_to_publication_min_ms": 3.0,
                "request_to_publication_max_ms": 3.0,
                "post_publication_exit_samples_ms": [1.0],
                "post_publication_exit_p50_ms": 1.0,
                "post_publication_exit_p95_ms": 1.0,
                "post_publication_exit_min_ms": 1.0,
                "post_publication_exit_max_ms": 1.0,
                "peak_rss_samples_bytes": [2048],
                "peak_rss_p50_bytes": 2048,
                "peak_rss_min_bytes": 2048,
                "peak_rss_max_bytes": 2048,
            }
            with (
                patch.object(benchmark, "benchmark", return_value=run_result),
                patch.object(
                    benchmark,
                    "benchmark_snapshot_capture",
                    return_value=capture_result,
                ) as capture,
                patch.object(
                    benchmark,
                    "benchmark_snapshot_restore",
                    return_value=run_result,
                ) as restore,
            ):
                self.assertEqual(benchmark.run_native_linux(args), 0)

            retained = capture.call_args.kwargs["retained_snapshot_path"]
            self.assertIsInstance(retained, Path)
            self.assertEqual(restore.call_args.kwargs["snapshot_path"], retained)
            document = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(
                document["snapshot_capture"]["kvm"]["peak_rss_p50_bytes"],
                2048,
            )

    def test_kvm_worker_e2e_restores_the_measured_snapshot(self):
        args = argparse.Namespace(
            _stage_dir="/tmp/stage",
            cpus="0",
            memory_mib=128,
            processors=4,
            net=None,
            suite="e2e",
            warmups=1,
            runs=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        run_result = cast(benchmark.BenchmarkResult, {"p50_ms": 10.0})
        capture_result = cast(benchmark.SnapshotCaptureResult, {"p50_ms": 3.0})
        with (
            patch.object(benchmark, "benchmark", return_value=run_result),
            patch.object(
                benchmark,
                "benchmark_snapshot_capture",
                return_value=capture_result,
            ) as capture,
            patch.object(
                benchmark,
                "benchmark_snapshot_restore",
                return_value=run_result,
            ) as restore,
            patch.object(benchmark, "print_summary"),
            patch.object(benchmark, "print_snapshot_summary"),
        ):
            self.assertEqual(benchmark.run_kvm_worker(args), 0)

        self.assertEqual(
            capture.call_args.args[1][-2:], ["--virtio-console", "console"]
        )
        retained = capture.call_args.kwargs["retained_snapshot_path"]
        self.assertIsInstance(retained, Path)
        self.assertEqual(restore.call_args.kwargs["snapshot_path"], retained)


if __name__ == "__main__":
    unittest.main()

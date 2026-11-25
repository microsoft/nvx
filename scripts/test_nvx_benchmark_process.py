#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import contextlib
import io
import os
import queue
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from collections.abc import Sequence
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, call, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import (  # noqa: E402
    benchmark,
)


class _TwoStreamInteraction:
    """An OpenVMM that prints the boot marker, waits for the guest's script,
    then writes each of ``steps``, a chunk or the end of its stream, to its
    console or to its stderr, and exits with ``status``.

    Its stderr shares the console stream, as InteractiveProcess's does by
    default, unless ``separate_stderr``, so that the same steps show what
    reaches the harness either way.
    """

    containment = None

    def __init__(
        self,
        steps: Sequence[tuple[benchmark.OutputStream, bytes | None]],
        status: int = 0,
        *,
        separate_stderr: bool,
    ) -> None:
        self.process = MagicMock(pid=123)
        self.process.poll.return_value = status
        self.process.wait.return_value = status
        self._steps = steps
        self._separate_stderr = separate_stderr
        self._stderr: benchmark.ChunkSink | None = None
        self._stderr_ready = threading.Event()
        self._typed = threading.Event()

    def read_stderr(self, chunks: benchmark.ChunkSink) -> None:
        self._stderr = chunks
        self._stderr_ready.set()

    def read_output(self, chunks: benchmark.ChunkSink) -> None:
        stderr = chunks
        if self._separate_stderr:
            self._stderr_ready.wait(5)
            assert self._stderr is not None
            stderr = self._stderr
        chunks.put(benchmark.BOOT_MARKER + b"\r\n")
        self._typed.wait(5)
        for stream, chunk in self._steps:
            # The shared stream ends once, after both of its writers.
            if chunk is not None or stderr is not chunks:
                (chunks if stream == "console" else stderr).put(chunk)
        if stderr is chunks:
            chunks.put(None)

    def write_input(self, _data: bytes) -> None:
        self._typed.set()

    def close(self) -> None:
        pass


class BenchmarkTests(unittest.TestCase):
    def test_require_file_preserves_resolved_path_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            missing = root / "nested" / ".." / "missing"
            resolved_missing = missing.resolve()

            with self.assertRaises(FileNotFoundError) as raised:
                benchmark.require_file(missing, "benchmark artifact")

            self.assertEqual(
                str(raised.exception),
                f"benchmark artifact not found: {resolved_missing}",
            )

    def test_measure_once_reads_output_to_its_end_before_finishing(self):
        record = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=guest_repair_gate exclusive=0 duration_ns=1500 "
            b"process_elapsed_ns=90000000 pid=4321\n"
        )
        split = record.index(b"duration_ns=") + len(b"duration_ns=")

        class FakeInteraction:
            def __init__(self, exited: threading.Event) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None
                self.exited = exited

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.RESTORE_MARKER + b"\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if self.exited.wait(5):
                    chunks.put(record[:split])
                    # The rest of the record outlasts the bounded drain that
                    # error paths use.
                    time.sleep(0.2)
                    chunks.put(record[split:])
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        for profiled in (False, True):
            with self.subTest(profiled=profiled):
                exited = threading.Event()

                def wait_for_exit(
                    _process: object, _timeout: float, exited: threading.Event = exited
                ) -> int:
                    exited.set()
                    return 0

                profiles: list[dict[str, object]] = []
                with tempfile.TemporaryDirectory() as temporary:
                    log_path = Path(temporary) / "restore.log"
                    with (
                        patch.object(
                            benchmark,
                            "InteractiveProcess",
                            return_value=FakeInteraction(exited),
                        ),
                        patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
                        patch.object(
                            benchmark, "live_peak_rss_bytes", return_value=1024
                        ),
                        patch.object(
                            benchmark,
                            "wait_for_process_exit",
                            side_effect=wait_for_exit,
                        ),
                        patch.object(
                            benchmark, "process_resource_counters", return_value={}
                        ),
                    ):
                        benchmark.measure_once(
                            ["openvmm"],
                            environment={},
                            timeout=10,
                            marker=benchmark.RESTORE_MARKER,
                            marker_must_be_line=True,
                            guest_exit_prequeued=True,
                            snapshot_profile=profiled,
                            profile_sink=profiles if profiled else None,
                            log_path=log_path,
                        )
                    log = log_path.read_bytes()

                self.assertTrue(
                    benchmark.contains_output_line(log, record.removesuffix(b"\n"))
                )
                if profiled:
                    records = cast(list[dict[str, object]], profiles[0]["records"])
                    repair = next(
                        item for item in records if item["phase"] == "guest_repair_gate"
                    )
                    self.assertEqual(repair["duration_ns"], 1500)
                    self.assertEqual(repair["process_elapsed_ns"], 90_000_000)
                else:
                    self.assertEqual(profiles, [])

    def test_measure_once_reports_output_that_does_not_reach_eof(self):
        release = threading.Event()
        self.addCleanup(release.set)

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.RESTORE_MARKER + b"\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(
                    b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
                    b"phase=guest_repair_gate exclusive=0 duration_ns="
                )
                release.wait(10)
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        profiles: list[dict[str, object]] = []
        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            patch.object(benchmark, "wait_for_process_exit", return_value=0),
            patch.object(benchmark, "process_resource_counters", return_value={}),
            patch.object(benchmark, "terminate") as terminate,
            self.assertRaisesRegex(
                RuntimeError, r"did not reach EOF within 0\.5s"
            ) as raised,
        ):
            benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=0.5,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
                snapshot_profile=True,
                profile_sink=profiles,
            )

        self.assertEqual(profiles, [])
        terminate.assert_called_once()
        self.assertIn("phase=guest_repair_gate", str(raised.exception))

    def test_measure_once_returns_when_a_descendant_keeps_the_output_open(self):
        # The descendant inherits the console output and stderr and outlives
        # the launched process, so neither stream reaches EOF.
        child = (
            "import subprocess, sys\n"
            "holder = subprocess.Popen(\n"
            "    [sys.executable, '-c', 'import time; time.sleep(60)'],\n"
            "    stdin=subprocess.DEVNULL,\n"
            ")\n"
            "with open(sys.argv[1], 'w') as stream:\n"
            "    stream.write(str(holder.pid))\n"
            "print('NVX-OUTPUT-HOLDER-READY', flush=True)\n"
        )
        with tempfile.TemporaryDirectory() as temporary:
            holder_pid = Path(temporary) / "holder.pid"

            def kill_holder() -> None:
                try:
                    os.kill(int(holder_pid.read_text()), signal.SIGTERM)
                except (OSError, ValueError):
                    pass

            started = time.monotonic()
            try:
                with (
                    patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
                    self.assertRaisesRegex(
                        RuntimeError, r"did not reach EOF within 3s of its exit"
                    ),
                ):
                    benchmark.measure_once(
                        [sys.executable, "-I", "-c", child, str(holder_pid)],
                        environment=dict(os.environ),
                        timeout=3,
                        marker=b"NVX-OUTPUT-HOLDER-READY",
                        marker_must_be_line=True,
                        guest_exit_prequeued=True,
                        log_path=Path(temporary) / "launch.log",
                    )
                elapsed = time.monotonic() - started
            finally:
                kill_holder()

        # Closing a pipe that a reader is blocked on would wait for the
        # descendant to exit.
        self.assertLess(elapsed, 20)

    def test_output_reader_reads_and_stops_above_fd_setsize(self):
        if sys.platform != "linux":
            self.skipTest("select() limits descriptors to FD_SETSIZE on POSIX")
        import fcntl
        import resource

        floor = 1100
        limit = floor + 64
        soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        if soft < limit:
            if hard != resource.RLIM_INFINITY and hard < limit:
                self.skipTest(f"the RLIMIT_NOFILE hard limit {hard} is too low")
            resource.setrlimit(resource.RLIMIT_NOFILE, (limit, hard))
            self.addCleanup(resource.setrlimit, resource.RLIMIT_NOFILE, (soft, hard))
        read_fd, write_fd = os.pipe()
        self.addCleanup(os.close, write_fd)
        # F_DUPFD takes the lowest free descriptor at or above the floor, so it
        # cannot replace a descriptor that something else owns.
        high = fcntl.fcntl(read_fd, fcntl.F_DUPFD, floor)
        os.close(read_fd)
        os.set_inheritable(high, False)
        self.addCleanup(os.close, high)
        self.assertGreaterEqual(high, floor)

        reader = benchmark._OutputReader(high)
        chunks: queue.Queue[bytes | None] = queue.Queue()
        thread = threading.Thread(target=reader.run, args=(chunks,), daemon=True)
        thread.start()
        os.write(write_fd, b"console output")
        self.assertEqual(chunks.get(timeout=5), b"console output")
        # The write end stays open, so only stop() can end the read.
        self.assertTrue(reader.stop(5))
        self.assertIsNone(chunks.get(timeout=5))
        thread.join(5)
        self.assertFalse(thread.is_alive())

    def test_guest_failure_report_includes_stderr_written_after_the_marker(self):
        terminated = threading.Event()

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(b"NVX-RESTORE-PROCESSORS-FAIL unstable-tsc\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if terminated.wait(5):
                    chunks.put(b"WARN host diagnostics after the guest failure\n")
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        def terminate_openvmm(_process: object) -> None:
            terminated.set()

        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(
                benchmark, "terminate", side_effect=terminate_openvmm
            ) as terminate,
            self.assertRaises(benchmark.GuestFailureReported) as raised,
        ):
            benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=10,
                marker=b"NVX-RESTORE-PROCESSORS-OK count=8",
                marker_must_be_line=True,
                guest_exit_prequeued=True,
                failure_marker=b"NVX-RESTORE-PROCESSORS-FAIL",
            )

        terminate.assert_called_once()
        self.assertEqual(
            raised.exception.line, "NVX-RESTORE-PROCESSORS-FAIL unstable-tsc"
        )
        self.assertIn("host diagnostics after the guest failure", str(raised.exception))

    def test_separated_output_keeps_process_output_lines_whole(self):
        record = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture phase=input_gate "
            b"exclusive=1 duration_ns=263787 process_elapsed_ns=1422094434 pid=1\n"
        )
        child = (
            "import os, time; "
            "os.write(1, b'NVX-S'); time.sleep(0.05); "
            f"os.write(2, {record!r}); time.sleep(0.05); "
            "os.write(1, b'NAPSHOT-DISPATCHED\\n~ # ')"
        )
        interaction = benchmark.InteractiveProcess(
            [sys.executable, "-I", "-c", child],
            dict(os.environ),
            separate_stderr=True,
        )
        stderr = bytearray()
        try:
            output = benchmark.SeparatedOutput(interaction)
            deadline = time.monotonic() + 30
            while not output.closed:
                stream, chunk = output.get(max(0.0, deadline - time.monotonic()))
                if stream == "stderr" and chunk is not None:
                    stderr.extend(chunk)
            self.assertEqual(interaction.process.wait(timeout=30), 0)
        finally:
            benchmark.terminate(interaction.process)
            interaction.close()

        marker = benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER
        self.assertTrue(benchmark.contains_output_line(output.console, marker))
        self.assertNotIn(b"OPENVMM_SNAPSHOT_PROFILE_V1", output.console)
        self.assertIn(record, bytes(stderr))
        contents = output.contents()
        self.assertTrue(benchmark.contains_output_line(contents, marker))
        self.assertTrue(
            benchmark.contains_output_line(contents, record.removesuffix(b"\n"))
        )
        self.assertTrue(contents.endswith(b"~ # "))

    def test_interactive_process_gives_stderr_its_own_pipe_on_request(self):
        for platform in ("linux", "win32"):
            for separate_stderr in (False, True):
                with self.subTest(platform=platform, separate=separate_stderr):
                    terminal: list[int] = []

                    def open_terminal(
                        terminal: list[int] = terminal,
                    ) -> tuple[int, int]:
                        terminal.extend(os.pipe())
                        return terminal[0], terminal[1]

                    process = MagicMock(pid=4321)
                    if not separate_stderr:
                        process.stderr = None
                    with (
                        patch.object(benchmark.sys, "platform", platform),
                        patch.object(
                            benchmark.os,
                            "openpty",
                            side_effect=open_terminal,
                            create=True,
                        ),
                        patch.object(
                            benchmark.subprocess, "Popen", return_value=process
                        ) as popen,
                    ):
                        interaction = benchmark.InteractiveProcess(
                            ["openvmm"], {}, separate_stderr=separate_stderr
                        )
                    try:
                        stderr = popen.call_args.kwargs["stderr"]
                        if separate_stderr:
                            self.assertEqual(stderr, subprocess.PIPE)
                        elif platform == "linux":
                            self.assertEqual(stderr, terminal[1])
                        else:
                            self.assertEqual(stderr, subprocess.STDOUT)
                        if not separate_stderr:
                            chunks: queue.Queue[bytes | None] = queue.Queue()
                            with self.assertRaisesRegex(
                                RuntimeError, "shares the console stream"
                            ):
                                interaction.read_stderr(chunks)
                            self.assertIsNone(chunks.get_nowait())
                    finally:
                        interaction.close()
                    if separate_stderr:
                        process.stderr.close.assert_called_once_with()

    def test_measure_once_does_not_resend_prequeued_guest_exit(self):
        class FakeProcess:
            pid = 123
            returncode = 0

            def poll(self):
                return None

            def terminate(self):
                raise AssertionError("guest-exit teardown terminated the host process")

        class FakeInteraction:
            def __init__(self):
                self.process = FakeProcess()
                self.writes: list[bytes] = []

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(b"/ # echo OPENVMM-SNAPSHOT-RESTORE-OK\r\n")
                chunks.put(b"OPENVMM-SNAPSHOT-RESTORE-OK\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]):
                chunks.put(None)

            def write_input(self, data: bytes):
                self.writes.append(data)

            def close(self):
                pass

        interaction = FakeInteraction()
        with (
            patch.object(benchmark, "InteractiveProcess", return_value=interaction),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            patch.object(benchmark, "wait_for_process_exit", return_value=0),
            patch.object(
                benchmark.time,
                "perf_counter_ns",
                side_effect=[0, 10_000_000, 17_000_000],
            ),
        ):
            result = benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=1,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
            )

        self.assertEqual(result, (10.0, 1024, 7.0, 17.0))
        self.assertEqual(interaction.writes, [])

    def test_measure_once_reports_missing_rss_when_openvmm_exits_first(self):
        class FakeProcess:
            pid = 123

            def __init__(self, status: int):
                self.status = status
                self.exited = False
                self.returncode: int | None = None

            def poll(self):
                if self.exited:
                    self.returncode = self.status
                return self.returncode

            def wait(self, timeout: float | None = None) -> int:
                del timeout
                return self.status

            def terminate(self):
                raise AssertionError("unexpected process termination")

        class FakeInteraction:
            def __init__(self, status: int):
                self.process = FakeProcess(status)

            def read_output(self, chunks: queue.Queue[bytes | None]):
                self.process.exited = True
                chunks.put(benchmark.RESTORE_MARKER + b"\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]):
                chunks.put(None)

            def write_input(self, data: bytes):
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self):
                pass

        for status in (0, 1):
            with self.subTest(status=status):
                interaction = FakeInteraction(status)

                def linux_peak_rss_bytes(
                    pid: int, interaction: FakeInteraction = interaction
                ) -> int | None:
                    self.assertEqual(pid, 123)
                    # A zombie's /proc status no longer reports VmHWM.
                    return None if interaction.process.exited else 1024

                with (
                    patch.object(
                        benchmark, "InteractiveProcess", return_value=interaction
                    ),
                    patch.object(
                        benchmark,
                        "_linux_live_peak_rss_bytes",
                        side_effect=linux_peak_rss_bytes,
                    ),
                    patch.object(benchmark, "windows_peak_rss_bytes", return_value=1),
                    patch.object(
                        benchmark, "wait_for_process_exit", return_value=status
                    ) as wait,
                    patch.object(
                        benchmark.time,
                        "perf_counter_ns",
                        side_effect=[0, 10_000_000, 17_000_000],
                    ),
                ):
                    if status == 0:
                        result = benchmark.measure_once(
                            ["openvmm"],
                            environment={},
                            timeout=1,
                            marker=benchmark.RESTORE_MARKER,
                            marker_must_be_line=True,
                            guest_exit_prequeued=True,
                        )
                        self.assertEqual(result, (10.0, None, 7.0, 17.0))
                    else:
                        with self.assertRaisesRegex(
                            RuntimeError, "status 1 during teardown"
                        ):
                            benchmark.measure_once(
                                ["openvmm"],
                                environment={},
                                timeout=1,
                                marker=benchmark.RESTORE_MARKER,
                                marker_must_be_line=True,
                                guest_exit_prequeued=True,
                            )

                wait.assert_called_once_with(
                    interaction.process, benchmark.TEARDOWN_TIMEOUT_SECONDS
                )

    def test_completed_output_line_with_prefix_requires_a_whole_line(self):
        prefix = b"NVX-RESTORE-PROCESSORS-FAIL"
        self.assertIsNone(
            benchmark.completed_output_line_with_prefix(
                b"NVX-RESTORE-PROCESSORS-FAIL unst", prefix
            )
        )
        self.assertIsNone(
            benchmark.completed_output_line_with_prefix(
                b'> echo "NVX-RESTORE-PROCESSORS-FAIL count=$n"\r\n', prefix
            )
        )
        self.assertEqual(
            benchmark.completed_output_line_with_prefix(
                b"guest\r\nNVX-RESTORE-PROCESSORS-FAIL unstable-tsc\r\nmore", prefix
            ),
            b"NVX-RESTORE-PROCESSORS-FAIL unstable-tsc",
        )

    def test_measure_once_stops_at_a_completed_guest_failure_marker(self):
        class FakeInteraction:
            def __init__(self):
                self.process = MagicMock(pid=123)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(b'> echo "NVX-RESTORE-PROCESSORS-FAIL count=$n"\r\n')
                chunks.put(b"Measured 6 cycles TSC warp between CPUs\r\n")
                chunks.put(b"NVX-RESTORE-PROCESSORS-FAIL unst")
                chunks.put(b"able-tsc\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]):
                chunks.put(None)

            def write_input(self, data: bytes):
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self):
                pass

        interaction = FakeInteraction()
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "restore.log"
            with (
                patch.object(benchmark, "InteractiveProcess", return_value=interaction),
                patch.object(benchmark, "terminate") as terminate,
                patch.object(benchmark, "wait_for_process_exit") as wait,
            ):
                with self.assertRaises(benchmark.GuestFailureReported) as raised:
                    benchmark.measure_once(
                        ["openvmm"],
                        environment={},
                        timeout=60,
                        marker=b"NVX-RESTORE-PROCESSORS-OK count=8",
                        marker_must_be_line=True,
                        guest_exit_prequeued=True,
                        log_path=log_path,
                        failure_marker=b"NVX-RESTORE-PROCESSORS-FAIL",
                    )
            log = log_path.read_bytes()

        self.assertEqual(
            raised.exception.line, "NVX-RESTORE-PROCESSORS-FAIL unstable-tsc"
        )
        self.assertIn("TSC warp", raised.exception.output_tail)
        self.assertIn("--- OpenVMM output ---", str(raised.exception))
        terminate.assert_called_once_with(interaction.process)
        wait.assert_not_called()
        self.assertTrue(log.endswith(b"NVX-RESTORE-PROCESSORS-FAIL unstable-tsc\r\n"))

    def test_terminate_waits_for_a_killed_process_to_exit(self):
        process = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"]
        )
        try:
            benchmark.terminate(process)
            exited = process.returncode is not None
        finally:
            process.kill()
            process.wait()

        self.assertTrue(exited)
        # An exited process is not killed again.
        with patch.object(process, "kill") as kill:
            benchmark.terminate(process)
        kill.assert_not_called()

    def test_terminate_fails_when_a_killed_process_keeps_running(self):
        process = MagicMock(pid=4321)
        process.poll.return_value = None
        with (
            patch.object(benchmark, "_wait_for_exit", return_value=False) as wait,
            self.assertRaisesRegex(
                benchmark.ProcessExitTimeout,
                "process 4321 did not exit within 30s of being killed",
            ),
        ):
            benchmark.terminate(process)

        process.kill.assert_called_once_with()
        wait.assert_called_once_with(process, benchmark.TERMINATION_TIMEOUT_SECONDS)

    def test_terminate_warns_only_about_a_slow_exit(self):
        for times, warning in (
            ((10.0, 12.5), "process 4321 took 2.500s to exit after it was killed"),
            ((10.0, 10.05), None),
        ):
            process = MagicMock(pid=4321)
            process.poll.return_value = None
            stderr = io.StringIO()
            with (
                patch.object(benchmark, "_wait_for_exit", return_value=True),
                patch.object(benchmark.time, "perf_counter", side_effect=times),
                contextlib.redirect_stderr(stderr),
            ):
                benchmark.terminate(process)

            if warning is None:
                self.assertEqual(stderr.getvalue(), "")
            else:
                self.assertIn(warning, stderr.getvalue())

    def test_exit_wait_ignores_the_returncode_that_a_windows_kill_stores(self):
        # Popen.kill() of a terminating Windows process stores its exit code,
        # although the process still runs.
        process = MagicMock(pid=4321, returncode=1)
        process.poll.return_value = 1
        with (
            patch.object(benchmark.os, "name", "nt"),
            patch.object(
                benchmark, "_wait_for_windows_process", side_effect=[False, True]
            ) as wait,
        ):
            self.assertFalse(benchmark._wait_for_exit(process, 2.0))
            self.assertTrue(benchmark._wait_for_exit(process, 2.0))

        self.assertEqual(wait.call_args_list, [call(4321, 2.0)] * 2)
        process.wait.assert_not_called()

    def test_exit_wait_reports_a_posix_process_that_keeps_running(self):
        process = MagicMock(pid=4321)
        process.wait.side_effect = [subprocess.TimeoutExpired(["openvmm"], 2.0), 0]
        with patch.object(benchmark.os, "name", "posix"):
            self.assertFalse(benchmark._wait_for_exit(process, 2.0))
            self.assertTrue(benchmark._wait_for_exit(process, 2.0))

        self.assertEqual(process.wait.call_args_list, [call(timeout=2.0)] * 2)

    @unittest.skipUnless(sys.platform == "win32", "requires Windows")
    def test_windows_exit_wait_waits_for_the_process_object(self):
        process = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"]
        )
        try:
            self.assertFalse(benchmark._wait_for_windows_process(process.pid, 0.05))
            process.kill()
            self.assertTrue(benchmark._wait_for_windows_process(process.pid, 30))
        finally:
            process.kill()
            process.wait()

    def test_guest_runner_preserves_primary_and_close_failures_with_bounded_tail(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return 7

            def wait(self):
                return 7

        class FakeInteraction:
            process = FakeProcess()
            containment = None

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\n" + b"x" * 5000 + b"\nDONE\n")
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                raise OSError("close failed")

        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "guest.log"
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=FakeInteraction()
                ),
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(benchmark, "terminate") as terminate,
            ):
                with self.assertRaisesRegex(
                    RuntimeError, "OpenVMM exited with status 7"
                ) as raised:
                    benchmark.run_guest_script(
                        ["openvmm"],
                        "guest command\n",
                        b"DONE",
                        timeout=1,
                        log_path=log_path,
                    )

            error = raised.exception
            cleanup_errors = cast(
                tuple[BaseException, ...],
                error.cleanup_errors,  # type: ignore[attr-defined]
            )
            self.assertEqual([str(item) for item in cleanup_errors], ["close failed"])
            self.assertIn("--- OpenVMM output ---", str(error))
            self.assertIn("--- cleanup failures ---", str(error))
            self.assertNotIn(benchmark.BOOT_MARKER.decode(), str(error))
            self.assertGreater(len(log_path.read_bytes()), 4096)
            self.assertTrue(log_path.read_bytes().endswith(b"\nDONE\n"))
            terminate.assert_called_once()

    def test_guest_runner_propagates_success_only_close_failure(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return 0

            def wait(self):
                return 0

        class FakeInteraction:
            process = FakeProcess()
            containment = None

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\nDONE\n")
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                raise OSError("close after success")

        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(benchmark, "_try_peak_rss", return_value=1024),
        ):
            with self.assertRaisesRegex(OSError, "close after success"):
                benchmark.run_guest_script(
                    ["openvmm"],
                    "guest command\n",
                    b"DONE",
                    timeout=1,
                )

    def test_guest_runner_describes_the_host_before_killing_a_hung_guest(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return None

        process = FakeProcess()
        events: list[str] = []

        class HungInteraction:
            containment = None

            def __init__(self):
                self.process = process
                self.closed = threading.Event()

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\n/ # esac\n")
                self.closed.wait(5)
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                self.closed.set()

        def describe(observed: object) -> str:
            events.append("described")
            self.assertIs(observed, process)
            return "files: shared/device-io.bin 4096 bytes"

        def fail(_observed: object) -> str:
            raise OSError("volume stalled")

        def kill(_process: object) -> None:
            events.append("killed")

        for describer, state in (
            (describe, "files: shared/device-io.bin 4096 bytes"),
            (fail, "could not be described: volume stalled"),
        ):
            events.clear()
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=HungInteraction()
                ),
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(benchmark, "terminate", side_effect=kill),
            ):
                with self.assertRaises(RuntimeError) as raised:
                    benchmark.run_guest_script(
                        ["openvmm"],
                        "guest command\n",
                        b"DONE",
                        timeout=0.2,
                        host_state_on_timeout=describer,
                    )

            message = str(raised.exception)
            self.assertTrue(
                message.startswith(
                    "guest workload did not finish within 0.2s\n"
                    f"--- host state at the timeout ---\n{state}\n"
                    "--- OpenVMM output ---\n"
                ),
                message,
            )
            self.assertTrue(message.endswith("/ # esac\n"), message)
            if describer is describe:
                # The host is described while OpenVMM still runs.
                self.assertEqual(events, ["described", "killed"])

    def test_guest_runner_describes_the_host_only_for_a_timeout(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return 7

            def wait(self):
                return 7

        class ExitingInteraction:
            process = FakeProcess()
            containment = None

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\n")
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                pass

        describe = MagicMock(return_value="files: none")
        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=ExitingInteraction()
            ),
            patch.object(benchmark, "_try_peak_rss", return_value=1024),
            patch.object(benchmark, "terminate"),
        ):
            with self.assertRaisesRegex(RuntimeError, "OpenVMM exited with status 7"):
                benchmark.run_guest_script(
                    ["openvmm"],
                    "guest command\n",
                    b"DONE",
                    timeout=1,
                    host_state_on_timeout=describe,
                )

        describe.assert_not_called()

    def test_guest_runner_ends_a_hung_guest_when_its_description_is_interrupted(
        self,
    ):
        class FakeProcess:
            pid = 123

            def poll(self):
                return None

        events: list[str] = []

        class HungInteraction:
            containment = None

            def __init__(self):
                self.process = FakeProcess()
                self.closed = threading.Event()

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\n/ # esac\n")
                self.closed.wait(5)
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                events.append("closed")
                self.closed.set()

        def interrupt(_observed: object) -> str:
            raise KeyboardInterrupt

        def kill(_process: object) -> None:
            events.append("killed")

        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=HungInteraction()
            ),
            patch.object(benchmark, "_try_peak_rss", return_value=1024),
            patch.object(benchmark, "terminate", side_effect=kill),
        ):
            with self.assertRaises(KeyboardInterrupt) as raised:
                benchmark.run_guest_script(
                    ["openvmm"],
                    "guest command\n",
                    b"DONE",
                    timeout=0.2,
                    host_state_on_timeout=interrupt,
                )

        # The interruption ends the run once OpenVMM is ended and its console closed.
        self.assertEqual(events, ["killed", "closed"])
        self.assertIsInstance(raised.exception.__context__, TimeoutError)

    def test_guest_runner_keeps_stderr_out_of_console_lines_on_request(self):
        # In CI run 41973357177, OpenVMM's stderr delivered the byte channel's
        # bytes around the console's marker, whose line they split on the
        # shared stream (#1290).
        steps: list[tuple[benchmark.OutputStream, bytes | None]] = [
            ("console", b"NVX-GUEST-SCRIPT-START\r\n"),
            ("stderr", b"\x00\r\n\x7f\xff"),
            ("console", b"NVX-LIFECYCLE-OK\r\n"),
            ("stderr", b"\n"),
            ("console", None),
            ("stderr", None),
        ]
        boot = benchmark.BOOT_MARKER + b"\r\n"
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "guest.log"
            shared = _TwoStreamInteraction(steps, separate_stderr=False)
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=shared
                ) as spawn,
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(benchmark, "terminate"),
                self.assertRaisesRegex(
                    RuntimeError,
                    "^guest exited without completion marker 'NVX-LIFECYCLE-OK'",
                ),
            ):
                benchmark.run_guest_script(
                    ["openvmm"],
                    "lifecycle\n",
                    b"NVX-LIFECYCLE-OK",
                    timeout=5,
                    log_path=log_path,
                )
            self.assertIs(spawn.call_args.kwargs["separate_stderr"], False)
            self.assertTrue(
                log_path.read_bytes().endswith(b"\x7f\xffNVX-LIFECYCLE-OK\r\n\n")
            )

            separate = _TwoStreamInteraction(steps, separate_stderr=True)
            stderr = bytearray()
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=separate
                ) as spawn,
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
            ):
                result = benchmark.run_guest_script(
                    ["openvmm"],
                    "lifecycle\n",
                    b"NVX-LIFECYCLE-OK",
                    timeout=5,
                    log_path=log_path,
                    on_stderr=stderr.extend,
                )
            self.assertIs(spawn.call_args.kwargs["separate_stderr"], True)
            self.assertEqual(
                result["text"],
                (boot + b"NVX-GUEST-SCRIPT-START\r\nNVX-LIFECYCLE-OK\r\n").decode(),
            )
            self.assertEqual(bytes(stderr), b"\x00\r\n\x7f\xff\n")
            # The log keeps both streams, interleaved by whole lines.
            self.assertEqual(
                log_path.read_bytes(),
                boot
                + b"NVX-GUEST-SCRIPT-START\r\n\x00\r\nNVX-LIFECYCLE-OK\r\n\x7f\xff\n",
            )

    def test_guest_runner_reads_a_separate_stderr_to_its_end(self):
        # OpenVMM writes a fatal error to stderr after its console has ended.
        fatal = b"fatal error: failed to launch vm worker\n"
        steps: list[tuple[benchmark.OutputStream, bytes | None]] = [
            ("console", None),
            ("stderr", fatal),
            ("stderr", None),
        ]
        interaction = _TwoStreamInteraction(steps, 1, separate_stderr=True)
        stderr = bytearray()
        with (
            patch.object(benchmark, "InteractiveProcess", return_value=interaction),
            patch.object(benchmark, "_try_peak_rss", return_value=1024),
            patch.object(benchmark, "terminate"),
            self.assertRaisesRegex(
                RuntimeError, "^OpenVMM exited with status 1"
            ) as raised,
        ):
            benchmark.run_guest_script(
                ["openvmm"], "true\n", b"DONE", timeout=5, on_stderr=stderr.extend
            )

        self.assertEqual(bytes(stderr), fatal)
        # The error report keeps both streams.
        self.assertTrue(
            str(raised.exception).endswith(
                "\n--- OpenVMM output ---\n"
                f"{benchmark.BOOT_MARKER.decode()}\r\n{fatal.decode()}"
            ),
            str(raised.exception),
        )

    def test_device_io_host_state_describes_files_processes_and_volume(self):
        from datetime import datetime, timezone

        whole_seconds = datetime(2026, 10, 7, 15, 40, 38, tzinfo=timezone.utc)
        modified_ns = int(whole_seconds.timestamp()) * 1_000_000_000 + 123_000_000
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "block.raw").write_bytes(b"")
            (root / "shared").mkdir()
            (root / "shared" / "device-io.bin").write_bytes(bytes(4096))
            for path in (root / "block.raw", root / "shared" / "device-io.bin"):
                os.utime(path, ns=(modified_ns, modified_ns))
            with (
                patch.object(benchmark, "_openvmm_pids", return_value=[77, 123]),
                patch.object(
                    benchmark.host_telemetry,
                    "describe_volume_activity",
                    return_value="scratch F: write 0.0 MiB/s at n/a ms, queue 32.0",
                ) as activity,
            ):
                state = benchmark._device_io_host_state(root, MagicMock(pid=123))

        activity.assert_called_once_with(
            {"scratch": root}, benchmark.DEVICE_IO_HOST_STATE_SAMPLE_SECONDS
        )
        self.assertEqual(
            state,
            "files: block.raw 0 bytes, modified 2026-10-07T15:40:38.123+00:00; "
            "shared/device-io.bin 4096 bytes, "
            "modified 2026-10-07T15:40:38.123+00:00\n"
            "OpenVMM processes: 77, 123 (this attempt)\n"
            "scratch volume: scratch F: write 0.0 MiB/s at n/a ms, queue 32.0",
        )

    def test_device_io_host_state_bounds_each_description(self):
        release = threading.Event()

        def stalled(_root: Path) -> str:
            release.wait(10)
            return "late"

        try:
            with (
                patch.object(benchmark, "DEVICE_IO_HOST_STATE_TIMEOUT_SECONDS", 0.2),
                patch.object(
                    benchmark, "_describe_volume_activity", side_effect=stalled
                ),
                patch.object(
                    benchmark, "_describe_files", side_effect=OSError("no listing")
                ),
                patch.object(benchmark, "_openvmm_pids", return_value=[]),
            ):
                started = time.monotonic()
                state = benchmark._device_io_host_state(Path("root"), MagicMock(pid=1))
                elapsed = time.monotonic() - started
        finally:
            release.set()

        self.assertLess(elapsed, 5)
        self.assertEqual(
            state,
            "files: failed: no listing\n"
            "OpenVMM processes: none\n"
            "scratch volume: did not finish within 0.2s",
        )

    def test_device_io_host_state_reports_a_directory_that_cannot_be_listed(self):
        real_scandir = os.scandir

        def scandir(path: str):
            if os.path.basename(path) == "shared":
                raise OSError("Input/output error")
            return real_scandir(path)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "block.raw").write_bytes(b"")
            (root / "shared").mkdir()
            (root / "shared" / "device-io.bin").write_bytes(b"")
            # The listing fails rather than leave out the files that it cannot list.
            with patch.object(benchmark.os, "scandir", side_effect=scandir):
                with self.assertRaisesRegex(OSError, "Input/output error"):
                    benchmark._describe_files(root)
            with self.assertRaises(FileNotFoundError):
                benchmark._describe_files(root / "missing")
            with (
                patch.object(benchmark, "_openvmm_pids", return_value=[]),
                patch.object(
                    benchmark, "_describe_volume_activity", return_value="idle"
                ),
            ):
                state = benchmark._device_io_host_state(
                    root / "missing", MagicMock(pid=1)
                )

        self.assertRegex(state.splitlines()[0], r"^files: failed: .*missing")

    def test_openvmm_pids_reads_the_windows_task_list(self):
        listing = (
            '"openvmm.exe","4321","Services","0","82,412 K"\r\n'
            '"openvmm.exe","17","Services","0","1,024 K"\r\n'
        )
        with (
            patch.object(benchmark.os, "name", "nt"),
            patch.object(
                benchmark.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0, listing, ""),
            ) as run,
        ):
            self.assertEqual(benchmark._openvmm_pids(), [17, 4321])
            run.return_value = subprocess.CompletedProcess(
                [], 0, "INFO: No tasks are running which match the criteria.\r\n", ""
            )
            self.assertEqual(benchmark._openvmm_pids(), [])

        self.assertEqual(
            run.call_args.args[0][:3],
            ["tasklist.exe", "/FI", "IMAGENAME eq openvmm.exe"],
        )

    def test_openvmm_pids_reads_linux_process_names(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            for pid, name in ((12, "openvmm"), (7, "python3"), (40, "openvmm")):
                (proc / str(pid)).mkdir()
                (proc / str(pid) / "comm").write_text(f"{name}\n", encoding="utf-8")
            # A process that exited before its name was read, and a link.
            (proc / "99").mkdir()
            (proc / "self").mkdir()
            with patch.object(benchmark.os, "name", "posix"):
                self.assertEqual(benchmark._openvmm_pids(proc), [12, 40])

    def test_live_peak_rss_samples_linux_process_without_reaping(self):
        process = MagicMock(pid=123)
        with (
            patch.object(benchmark.os, "name", "posix"),
            patch.object(benchmark.sys, "platform", "linux"),
            patch.object(
                benchmark, "_linux_live_peak_rss_bytes", side_effect=[4096, None, 0]
            ) as read,
        ):
            self.assertEqual(benchmark.live_peak_rss_bytes(process), 4096)
            self.assertIsNone(benchmark.live_peak_rss_bytes(process))
            with self.assertRaisesRegex(RuntimeError, "peak RSS 0 bytes"):
                benchmark.live_peak_rss_bytes(process)

        self.assertEqual(read.call_args_list, [call(123)] * 3)
        process.poll.assert_not_called()

    def test_live_peak_rss_accepts_only_running_windows_samples(self):
        running = MagicMock(pid=7)
        running.poll.return_value = None
        exited = MagicMock(pid=8)
        exited.poll.return_value = 0
        racing = MagicMock(pid=9)
        racing.poll.return_value = None

        def read_counters(pid: int) -> int:
            if pid == racing.pid:
                racing.poll.return_value = 0
            return 8192

        with (
            patch.object(benchmark.os, "name", "nt"),
            patch.object(
                benchmark, "windows_peak_rss_bytes", side_effect=read_counters
            ) as read,
        ):
            self.assertEqual(benchmark.live_peak_rss_bytes(running), 8192)
            # A retained handle still reports counters, including teardown.
            self.assertIsNone(benchmark.live_peak_rss_bytes(exited))
            self.assertIsNone(benchmark.live_peak_rss_bytes(racing))

        self.assertEqual(read.call_args_list, [call(7), call(8), call(9)])
        with (
            patch.object(benchmark.os, "name", "nt"),
            patch.object(
                benchmark, "windows_peak_rss_bytes", side_effect=OSError("closed")
            ),
        ):
            self.assertIsNone(benchmark.live_peak_rss_bytes(exited))
            with self.assertRaisesRegex(OSError, "closed"):
                benchmark.live_peak_rss_bytes(running)

    def test_linux_live_peak_rss_requires_process_address_space(self):
        running = "Name:\topenvmm\nState:\tS (sleeping)\nVmHWM:\t   59008 kB\n"
        zombie = "Name:\topenvmm\nState:\tZ (zombie)\nThreads:\t1\n"
        with patch.object(
            Path,
            "read_text",
            autospec=True,
            side_effect=[running, zombie, FileNotFoundError(), ProcessLookupError()],
        ) as read:
            self.assertEqual(benchmark._linux_live_peak_rss_bytes(123), 59008 * 1024)
            for _ in range(3):
                self.assertIsNone(benchmark._linux_live_peak_rss_bytes(123))

        self.assertEqual(read.call_args.args[0], Path("/proc/123/status"))
        with (
            patch.object(Path, "read_text", side_effect=PermissionError()),
            self.assertRaises(PermissionError),
        ):
            benchmark._linux_live_peak_rss_bytes(123)

    def test_linux_process_counters_read_only_kernel_counters(self):
        # Profiles sample these counters inside measured intervals, where
        # reading smaps_rollup slowed MSHV snapshot generation (#383).
        files = {
            Path("/proc/123/status"): (
                "Name:\topenvmm\nVmHWM:\t   59008 kB\nVmRSS:\t   57344 kB\n"
            ),
            Path("/proc/123/stat"): (
                "123 (openvmm) S 1 123 123 0 -1 4194560 812 0 3 0 5 2 0 0 20 0 9\n"
            ),
        }

        def read_text(path: Path, encoding: str) -> str:
            self.assertEqual(encoding, "ascii")
            return files[path]

        with patch.object(
            Path, "read_text", autospec=True, side_effect=read_text
        ) as read:
            counters = benchmark._linux_process_resource_counters(123)

        self.assertEqual(
            counters,
            {
                "rss_bytes": 57344 * 1024,
                "peak_rss_bytes": 59008 * 1024,
                "minor_faults": 812,
                "major_faults": 3,
                "page_faults": 815,
            },
        )
        self.assertEqual(
            [entry.args[0] for entry in read.call_args_list],
            [Path("/proc/123/status"), Path("/proc/123/stat")],
        )

    def test_process_counters_use_linux_counters_and_tolerate_exit(self):
        with (
            patch.object(benchmark.os, "name", "posix"),
            patch.object(benchmark.sys, "platform", "linux"),
            patch.object(
                benchmark,
                "_linux_process_resource_counters",
                side_effect=[
                    {"rss_bytes": 4096},
                    FileNotFoundError(),
                    ProcessLookupError(),
                ],
            ) as read,
        ):
            self.assertEqual(
                benchmark.process_resource_counters(123), {"rss_bytes": 4096}
            )
            # OpenVMM can exit between a profile record and the read.
            self.assertEqual(benchmark.process_resource_counters(123), {})
            self.assertEqual(benchmark.process_resource_counters(123), {})

        self.assertEqual(read.call_args_list, [call(123)] * 3)

    def test_output_marker_must_be_a_complete_line(self):
        marker = benchmark.RESTORE_MARKER
        self.assertFalse(
            benchmark.contains_output_line(
                b"/ # echo OPENVMM-SNAPSHOT-RESTORE-OK\r\n",
                marker,
            )
        )
        self.assertTrue(
            benchmark.contains_output_line(
                b"/ # echo OPENVMM-SNAPSHOT-RESTORE-OK\r\n"
                b"OPENVMM-SNAPSHOT-RESTORE-OK\r\n",
                marker,
            )
        )


if __name__ == "__main__":
    unittest.main()

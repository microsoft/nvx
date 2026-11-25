#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import errno
import io
import queue
import re
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Callable
from pathlib import Path
from typing import Any, cast
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import (  # noqa: E402
    benchmark,
    openvmm_process,
)


class CleanupAfterKillTests(unittest.TestCase):
    """A failure path keeps its error when OpenVMM outlives its kill."""

    class StuckInteraction:
        """An OpenVMM that printed ``console`` and outlives its kill, whose
        output reader, with ``reader_stuck``, doesn't stop either."""

        def __init__(self, console: bytes, reader_stuck: bool = False) -> None:
            self.process = MagicMock(pid=4321)
            self.process.poll.return_value = None
            self.console = console
            self.reader_stuck = reader_stuck
            self.closed = False
            self.closes = 0

        def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
            chunks.put(self.console)

        def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
            del chunks

        def write_input(self, data: bytes) -> None:
            raise AssertionError(f"unexpected input: {data!r}")

        def close(self) -> None:
            self.closed = True
            self.closes += 1
            if self.reader_stuck:
                raise RuntimeError("OpenVMM output reader did not stop")

    EXIT_TIMEOUT = "process 4321 did not exit within 30s of being killed"
    READER_STUCK = "RuntimeError: OpenVMM output reader did not stop"

    def outliving_kill(self) -> Any:
        def outlive(*_args: object, **_kwargs: object) -> None:
            raise benchmark.ProcessExitTimeout(self.EXIT_TIMEOUT)

        return patch.object(benchmark, "terminate", side_effect=outlive)

    def test_a_launch_failure_keeps_its_error_when_openvmm_outlives_the_kill(self):
        with (
            patch.object(
                benchmark,
                "InteractiveProcess",
                return_value=self.StuckInteraction(b"booting\n"),
            ),
            patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
            self.outliving_kill() as terminate,
            self.assertRaises(RuntimeError) as raised,
        ):
            benchmark.measure_once(["openvmm"], environment={}, timeout=0.2)

        terminate.assert_called_once()
        self.assertEqual(
            str(raised.exception),
            "guest marker was not observed within 0.2s\n"
            "--- OpenVMM output ---\nbooting\n\n"
            f"--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}",
        )
        self.assertIsInstance(raised.exception.__cause__, TimeoutError)

    def test_a_guest_failure_report_survives_an_openvmm_that_outlives_the_kill(self):
        with (
            patch.object(
                benchmark,
                "InteractiveProcess",
                return_value=self.StuckInteraction(b"NVX-TEST-FAIL broken\n"),
            ),
            patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
            self.outliving_kill() as terminate,
            self.assertRaises(benchmark.GuestFailureReported) as reported,
        ):
            benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=5,
                failure_marker=b"NVX-TEST-FAIL",
            )

        terminate.assert_called_once()
        self.assertEqual(reported.exception.line, "NVX-TEST-FAIL broken")
        self.assertTrue(
            str(reported.exception).endswith(
                f"\n--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}"
            ),
            str(reported.exception),
        )

    def test_a_teardown_timeout_kills_an_openvmm_that_outlives_the_kill_once(self):
        with (
            patch.object(
                benchmark,
                "InteractiveProcess",
                return_value=self.StuckInteraction(benchmark.BOOT_MARKER + b"\n"),
            ),
            patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            patch.object(
                benchmark,
                "wait_for_process_exit",
                side_effect=subprocess.TimeoutExpired("openvmm", 15),
            ),
            self.outliving_kill() as terminate,
            self.assertRaises(RuntimeError) as raised,
        ):
            benchmark.measure_once(
                ["openvmm"], environment={}, timeout=5, guest_exit_prequeued=True
            )

        terminate.assert_called_once()
        self.assertTrue(
            str(raised.exception).startswith(
                f"{self.EXIT_TIMEOUT}\n--- OpenVMM output ---\n"
            ),
            str(raised.exception),
        )

    def test_cleanup_runs_every_step_and_keeps_the_error_that_it_follows(self):
        calls: list[str] = []

        def step(name: str, error: Exception | None = None) -> Callable[[], None]:
            def run() -> None:
                calls.append(name)
                if error is not None:
                    raise error

            return run

        failure = ValueError("scenario failed")
        benchmark.append_cleanup_failures(
            failure, [benchmark.ProcessExitTimeout(self.EXIT_TIMEOUT)]
        )
        benchmark.finish_cleanup(
            failure,
            step("log", OSError("disk full")),
            None,
            step("close", RuntimeError("OpenVMM output reader did not stop")),
        )
        self.assertEqual(calls, ["log", "close"])
        # The failures join the kill's under one heading.
        self.assertEqual(
            str(failure),
            "scenario failed\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n- OSError: disk full\n"
            f"- {self.READER_STUCK}",
        )
        # Without an error to follow, the first failure surfaces with the others.
        with self.assertRaises(OSError) as raised:
            benchmark.finish_cleanup(
                None,
                step("log", OSError("disk full")),
                step("close", RuntimeError("OpenVMM output reader did not stop")),
            )
        self.assertEqual(
            str(raised.exception),
            f"disk full\n--- cleanup failures ---\n- {self.READER_STUCK}",
        )
        benchmark.finish_cleanup(None, step("close"))

    def test_cleanup_failures_reach_an_error_whose_message_has_an_errno(self):
        # An OSError with an errno takes its message from its errno, not its
        # arguments, so its failures go into an OSError that it caused.
        def full() -> OSError:
            return OSError(errno.ENOSPC, "No space left on device", "source.log")

        def fail(error: BaseException) -> Callable[[], None]:
            def run() -> None:
                raise error

            return run

        original = full()
        combined = benchmark.append_cleanup_failures(
            original, [RuntimeError("OpenVMM output reader did not stop")]
        )
        expected = f"{full()}\n--- cleanup failures ---\n- {self.READER_STUCK}"
        self.assertIs(type(combined), OSError)
        self.assertIs(combined.__cause__, original)
        self.assertEqual(str(combined), expected)
        self.assertEqual(str(original), str(full()))
        # A cleanup whose first failure is such an error raises the combined
        # one, and so does a cleanup that follows such an error.
        reader = RuntimeError("OpenVMM output reader did not stop")
        with self.assertRaises(OSError) as raised:
            benchmark.finish_cleanup(None, fail(full()), fail(reader))
        self.assertEqual(str(raised.exception), expected)
        with self.assertRaises(OSError) as raised:
            try:
                raise full()
            except OSError as failure:
                reader = RuntimeError("OpenVMM output reader did not stop")
                benchmark.finish_cleanup(failure, fail(reader))
                raise
        self.assertEqual(str(raised.exception), expected)
        self.assertIsInstance(raised.exception.__cause__, OSError)

    def test_a_launch_failure_keeps_its_error_when_the_reader_does_not_stop_either(
        self,
    ):
        with (
            patch.object(
                benchmark,
                "InteractiveProcess",
                return_value=self.StuckInteraction(b"booting\n", reader_stuck=True),
            ),
            patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
            self.outliving_kill() as terminate,
            self.assertRaises(RuntimeError) as raised,
        ):
            benchmark.measure_once(["openvmm"], environment={}, timeout=0.2)

        terminate.assert_called_once()
        self.assertEqual(
            str(raised.exception),
            "guest marker was not observed within 0.2s\n"
            "--- OpenVMM output ---\nbooting\n\n"
            f"--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n"
            f"- {self.READER_STUCK}",
        )
        self.assertIsInstance(raised.exception.__cause__, TimeoutError)

    def test_a_snapshot_capture_keeps_its_error_and_log_past_a_stuck_reader(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "source.log"
            with (
                patch.object(
                    benchmark,
                    "InteractiveProcess",
                    return_value=self.StuckInteraction(b"booting\n", reader_stuck=True),
                ),
                self.outliving_kill() as terminate,
                self.assertRaises(RuntimeError) as raised,
            ):
                benchmark.capture_device_restore_snapshot(
                    ["openvmm"],
                    Path(temporary) / "snapshot",
                    "blk",
                    "cold",
                    timeout=0.2,
                    log_path=log_path,
                )

            terminate.assert_called_once()
            # The log is written before the interaction's shutdown fails.
            self.assertEqual(log_path.read_bytes(), b"booting\n")
        self.assertEqual(
            str(raised.exception),
            "blk/cold source snapshot did not finish within 0.2s\n"
            "--- OpenVMM output ---\nbooting\n\n"
            f"--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n"
            f"- {self.READER_STUCK}",
        )
        self.assertIsInstance(raised.exception.__cause__, TimeoutError)

    def test_an_openvmm_process_closes_and_keeps_its_error_past_a_failed_kill(self):
        interaction = self.StuckInteraction(b"booting\n")
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "process.log"
            with (
                patch.object(
                    openvmm_process, "InteractiveProcess", return_value=interaction
                ),
                self.outliving_kill() as terminate,
                self.assertRaises(RuntimeError) as raised,
                openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process,
            ):
                process.wait_for(b"NEVER", 0.2)

            terminate.assert_called_once()
            self.assertTrue(interaction.closed)
            self.assertEqual(log_path.read_bytes(), b"booting\n")
        self.assertEqual(
            str(raised.exception),
            "marker b'NEVER' was not observed within 0.2s\n"
            "--- OpenVMM output ---\nbooting\n\n"
            f"--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}",
        )

    def test_an_openvmm_process_reports_a_failed_kill_after_its_block(self):
        interaction = self.StuckInteraction(b"booting\n")
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "process.log"
            with (
                patch.object(
                    openvmm_process, "InteractiveProcess", return_value=interaction
                ),
                self.outliving_kill(),
            ):
                # A block that leaves OpenVMM running fails only on the kill.
                with self.assertRaisesRegex(
                    benchmark.ProcessExitTimeout, f"^{re.escape(self.EXIT_TIMEOUT)}$"
                ):
                    with openvmm_process.OpenvmmProcess(["openvmm"], log_path):
                        pass
                # The error that leaves a block stays the one that surfaces.
                with self.assertRaises(ValueError) as raised:
                    with openvmm_process.OpenvmmProcess(["openvmm"], log_path):
                        raise ValueError("scenario failed")
        self.assertEqual(
            str(raised.exception),
            "scenario failed\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}",
        )

    def test_an_openvmm_process_keeps_its_error_past_a_failed_kill_and_shutdown(self):
        # An OpenVMM that outlives its kill can keep its output open, so its
        # reader doesn't stop either, and neither failure hides the other.
        interaction = self.StuckInteraction(b"booting\n", reader_stuck=True)
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "process.log"
            with (
                patch.object(
                    openvmm_process, "InteractiveProcess", return_value=interaction
                ),
                self.outliving_kill() as terminate,
                self.assertRaises(RuntimeError) as raised,
                openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process,
            ):
                process.wait_for(b"NEVER", 0.2)

            terminate.assert_called_once()
            # The failure path releases the process, and the block's exit
            # doesn't release it again.
            self.assertEqual(interaction.closes, 1)
            self.assertEqual(log_path.read_bytes(), b"booting\n")
        self.assertEqual(
            str(raised.exception),
            "marker b'NEVER' was not observed within 0.2s\n"
            "--- OpenVMM output ---\nbooting\n\n"
            f"--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n"
            f"- {self.READER_STUCK}",
        )

    def test_an_openvmm_process_reports_a_failed_kill_and_shutdown_after_its_block(
        self,
    ):
        def stuck(*_arguments: object) -> CleanupAfterKillTests.StuckInteraction:
            return self.StuckInteraction(b"", reader_stuck=True)

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(openvmm_process, "InteractiveProcess", side_effect=stuck),
            self.outliving_kill(),
        ):
            log_path = Path(temporary) / "process.log"
            with self.assertRaises(benchmark.ProcessExitTimeout) as closed:
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path):
                    pass
            with self.assertRaises(ValueError) as raised:
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path):
                    raise ValueError("scenario failed")
        self.assertEqual(
            str(closed.exception),
            f"{self.EXIT_TIMEOUT}\n--- cleanup failures ---\n- {self.READER_STUCK}",
        )
        self.assertEqual(
            str(raised.exception),
            "scenario failed\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n- {self.READER_STUCK}",
        )

    def test_an_openvmm_process_reports_its_cleanup_after_an_errno_error(self):
        def stuck(*_arguments: object) -> CleanupAfterKillTests.StuckInteraction:
            return self.StuckInteraction(b"", reader_stuck=True)

        full = OSError(errno.ENOSPC, "No space left on device", "source.log")
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(openvmm_process, "InteractiveProcess", side_effect=stuck),
            self.outliving_kill(),
            self.assertRaises(OSError) as raised,
            openvmm_process.OpenvmmProcess(["openvmm"], Path(temporary) / "log"),
        ):
            raise full
        # The block's error can't show the failures, so the error that it
        # caused, which does, surfaces in its place.
        self.assertIs(raised.exception.__cause__, full)
        self.assertEqual(
            str(raised.exception),
            f"{full}\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n- {self.READER_STUCK}",
        )

    def test_an_openvmm_process_keeps_its_error_past_a_failed_transcript_close(
        self,
    ):
        class FullLog(io.BytesIO):
            """A transcript whose close fails to flush it."""

            def close(self) -> None:
                super().close()
                raise OSError(errno.ENOSPC, "No space left on device", "log")

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(
                openvmm_process,
                "InteractiveProcess",
                return_value=self.StuckInteraction(b"booting\n"),
            ),
            self.outliving_kill(),
            self.assertRaises(ValueError) as raised,
            openvmm_process.OpenvmmProcess(
                ["openvmm"], Path(temporary) / "log"
            ) as process,
        ):
            process._log.close()
            cast(Any, process)._log = FullLog()
            raise ValueError("scenario failed")
        full = OSError(errno.ENOSPC, "No space left on device", "log")
        self.assertEqual(
            str(raised.exception),
            "scenario failed\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n- OSError: {full}",
        )

    def test_a_failed_openvmm_launch_keeps_its_error_past_a_failed_kill_and_shutdown(
        self,
    ):
        interaction = self.StuckInteraction(b"", reader_stuck=True)
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(
                openvmm_process, "InteractiveProcess", return_value=interaction
            ),
            patch.object(
                openvmm_process.time, "sleep", side_effect=RuntimeError("interrupted")
            ),
            self.outliving_kill() as terminate,
            self.assertRaises(RuntimeError) as raised,
        ):
            log_path = Path(temporary) / "process.log"
            openvmm_process.OpenvmmProcess(["openvmm"], log_path, output_read_delay=1)

        terminate.assert_called_once()
        self.assertEqual(interaction.closes, 1)
        self.assertEqual(
            str(raised.exception),
            "interrupted\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n- {self.READER_STUCK}",
        )


if __name__ == "__main__":
    unittest.main()

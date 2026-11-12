"""The scenario duration report of `nvx.py test-microvm`.

doc/usage.md#scenario-duration-report describes the format.
"""

from __future__ import annotations

import ctypes
import json
import os
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from datetime import datetime, timezone
from pathlib import Path
from types import TracebackType
from typing import Literal

from .common import github_job

# The report's name in test-microvm's output directory.
REPORT_NAME = "scenario-durations.jsonl"
SCHEMA = "nvx-microvm-durations"
# Changes only when a reader of the previous version would misread a report;
# added fields keep it.
SCHEMA_VERSION = 1
# A record keeps this much of a failure's message; the logs keep all of it.
ERROR_CHARACTERS = 1000
# Seconds from the FILETIME epoch, 1601-01-01, to the Unix epoch.
FILETIME_UNIX_EPOCH_SECONDS = 11_644_473_600


def proc_start_seconds(stat: str, ticks_per_second: int) -> float:
    """Return when a Linux process started, in seconds after boot, from its
    /proc/PID/stat."""
    # The fields after the command name, which may hold spaces and
    # parentheses, start with the 3rd; starttime is the 22nd.
    return int(stat.rpartition(")")[2].split()[19]) / ticks_per_second


def process_age_seconds() -> float | None:
    """Return how long ago this process started, or None if the OS doesn't
    tell."""
    try:
        if sys.platform == "linux":
            started = proc_start_seconds(
                Path("/proc/self/stat").read_text(encoding="utf-8", errors="replace"),
                os.sysconf("SC_CLK_TCK"),
            )
            return time.clock_gettime(time.CLOCK_BOOTTIME) - started
        if sys.platform == "win32":
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.GetCurrentProcess.restype = ctypes.c_void_p
            filetime = ctypes.POINTER(ctypes.c_uint64)
            kernel32.GetProcessTimes.argtypes = (
                ctypes.c_void_p,
                filetime,
                filetime,
                filetime,
                filetime,
            )
            kernel32.GetProcessTimes.restype = ctypes.c_int
            created, exited, kernel, user = (ctypes.c_uint64() for _ in range(4))
            if not kernel32.GetProcessTimes(
                kernel32.GetCurrentProcess(),
                ctypes.byref(created),
                ctypes.byref(exited),
                ctypes.byref(kernel),
                ctypes.byref(user),
            ):
                raise ctypes.WinError(ctypes.get_last_error())
            # FILETIME counts 100 ns intervals.
            return time.time() - (
                created.value / 10_000_000 - FILETIME_UNIX_EPOCH_SECONDS
            )
    except (OSError, ValueError, IndexError):
        pass
    return None


def _outcome(error: BaseException | None) -> str:
    if error is None:
        return "passed"
    return "interrupted" if isinstance(error, KeyboardInterrupt) else "failed"


def _error(error: BaseException | None) -> str | None:
    if error is None or isinstance(error, KeyboardInterrupt):
        return None
    message = str(error)
    name = type(error).__name__
    return (f"{name}: {message}" if message else name)[:ERROR_CHARACTERS]


class DurationReport:
    """The scenario duration report of one test-microvm run.

    The report is JSON Lines. A header comes first, and then one record for
    each span of the run, which the report appends as soon as the span ends,
    so that a run that fails or is interrupted keeps every span that it
    completed. The spans follow one another without gaps from the start of
    the process: the setup, one span for each scenario execution, which ends
    when the next one starts, and the teardown, which finish() starts and
    which ends with the with block, with the run's outcome.
    """

    def __init__(
        self,
        path: Path,
        *,
        backend: str,
        guest: str,
        kernel: str,
        processors: Sequence[int],
        scenarios: Sequence[str],
        architecture: str = "x86_64",
        scope: str = "full",
        clock: Callable[[], float] = time.monotonic,
        wall_clock: Callable[[], float] = time.time,
        started: float | None = None,
    ) -> None:
        """Write the header of a report on a run that started at `started`
        on `clock`, by default when this process started."""
        self.path = path
        self._clock = clock
        now = clock()
        # Every timestamp is a reading of `clock` plus this one offset, so
        # that each span starts exactly where the previous one ended.
        self._offset = wall_clock() - now
        if started is None:
            started = now - max(0.0, process_age_seconds() or 0.0)
        self._dimensions = {
            "guest": guest,
            "backend": backend,
            "kernel": kernel,
            "architecture": architecture,
            "scope": scope,
        }
        self._span: dict[str, object] = {"record": "setup"}
        self._span_started = started
        self._write(
            {
                "record": "run",
                "schema": SCHEMA,
                "schema_version": SCHEMA_VERSION,
                "started_at": self._timestamp(started),
                **self._dimensions,
                "processors": [*dict.fromkeys(processors)],
                "scenarios": list(scenarios),
                "github": github_job(os.environ),
            },
            mode="w",
        )

    def start(
        self,
        scenario: str,
        message: str,
        *,
        processors: Sequence[int] | None = None,
        variant: str | None = None,
    ) -> None:
        """End the open span, print `message` at once, and time `scenario`,
        which runs on `processors` if it takes processor counts."""
        ended = self._end_span(None)
        self._span = {
            "record": "scenario",
            "scenario": scenario,
            **self._dimensions,
            "processors": None if processors is None else [*dict.fromkeys(processors)],
        }
        if variant is not None:
            self._span["variant"] = variant
        self._write(ended)
        print(message, flush=True)

    def finish(self) -> None:
        """End the last scenario, or the setup if none ran, and start the
        teardown."""
        ended = self._end_span(None)
        self._span = {"record": "teardown"}
        self._write(ended)

    def __enter__(self) -> DurationReport:
        return self

    def __exit__(
        self,
        kind: type[BaseException] | None,
        error: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        """End the open span, and then the teardown, with the run's outcome."""
        try:
            if self._span["record"] != "teardown":
                # The span that raised `error`, or the last scenario if
                # finish() was not called.
                ended = self._end_span(error)
                self._span = {"record": "teardown"}
                self._write(ended)
            self._write(self._end_span(error))
        except OSError as failure:
            if error is None:
                raise
            # Keep the run's own error; the report then lacks its end.
            print(
                f"warning: could not complete {self.path}: {failure}",
                file=sys.stderr,
                flush=True,
            )

    def _end_span(self, error: BaseException | None) -> dict[str, object]:
        now = self._clock()
        record = {
            **self._span,
            "started_at": self._timestamp(self._span_started),
            "seconds": round(now - self._span_started, 3),
            "outcome": _outcome(error),
            "error": _error(error),
        }
        self._span_started = now
        return record

    def _timestamp(self, value: float) -> str:
        moment = datetime.fromtimestamp(self._offset + value, timezone.utc)
        return moment.isoformat(timespec="milliseconds").replace("+00:00", "Z")

    def _write(
        self, record: Mapping[str, object], mode: Literal["a", "w"] = "a"
    ) -> None:
        with self.path.open(mode, encoding="utf-8", newline="\n") as stream:
            stream.write(json.dumps(record, separators=(",", ":")) + "\n")

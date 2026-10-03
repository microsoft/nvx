"""Binary-safe foreground process control for OpenVMM integration tests."""

from __future__ import annotations

import os
import queue
import socket
import subprocess
import threading
import time
from collections.abc import Mapping, Sequence
from pathlib import Path
from types import TracebackType
from typing import NamedTuple

from .benchmark import InteractiveProcess, terminate
from .common import remaining_timeout
from .time_abi import (
    GUEST_BOOT_MARKERS,
    STATUS_TIMEOUT_SECONDS,
    TimeAbiFailure,
    TimeAbiMonitor,
    status_script,
)

_GUEST_BOOT_MARKERS = tuple(marker.encode() for marker in GUEST_BOOT_MARKERS)


def _line_marker_end(
    output: bytearray,
    marker: bytes,
    offset: int,
) -> int | None:
    if offset > 0 and output[offset - 1] != ord("\n"):
        newline = output.find(b"\n", offset)
        if newline < 0:
            return None
        offset = newline + 1
    while True:
        newline = output.find(b"\n", offset)
        if newline < 0:
            return None
        if bytes(output[offset:newline]).removesuffix(b"\r") == marker:
            return newline + 1
        offset = newline + 1


class OpenvmmProcessResult(NamedTuple):
    returncode: int
    output: bytes


class OpenvmmProcess:
    def __init__(
        self,
        command: Sequence[str],
        log_path: Path,
        *,
        environment: Mapping[str, str] | None = None,
        output_read_delay: float = 0.0,
        time_abi_status: bool = True,
    ) -> None:
        """Start OpenVMM and scan its console output for the time ABI.

        With ``time_abi_status``, a cold-booted guest whose boot marker
        appeared on this console answers a time ABI status query before its
        first other input, which waits until the query exits so the guest's
        tty cannot echo that input into the status lines. The query fails
        unless the guest reported a valid boot line.
        """
        if output_read_delay < 0:
            raise ValueError("OpenVMM output read delay cannot be negative")
        process_environment = os.environ.copy()
        process_environment["OPENVMM_LOG"] = "off"
        if environment is not None:
            process_environment.update(environment)
        self._interaction = InteractiveProcess(command, process_environment)
        self._chunks: queue.Queue[bytes | None] = queue.Queue()
        self._reader = threading.Thread(
            target=self._interaction.read_output,
            args=(self._chunks,),
            daemon=True,
        )
        try:
            if output_read_delay:
                time.sleep(output_read_delay)
            self._reader.start()
        except BaseException:
            terminate(self._interaction.process)
            self._interaction.close()
            raise
        self._output = bytearray()
        self._search_offset = 0
        self._log_path = log_path
        self._finished = False
        self._monitor = TimeAbiMonitor(command)
        self._query_status = time_abi_status and self._monitor.cold_boot
        self._status_sent = False

    @property
    def process(self):
        return self._interaction.process

    @property
    def output(self) -> bytes:
        return bytes(self._output)

    @property
    def time_abi(self) -> TimeAbiMonitor:
        return self._monitor

    def send_bytes(self, data: bytes) -> None:
        self._query_status_first()
        self._interaction.write_input(data)

    def send_line(self, line: str) -> None:
        self.send_bytes(f"{line}\n".encode())

    def _query_status_first(self) -> None:
        if (
            not self._query_status
            or self._status_sent
            # The guest's shell is not on this console, or has not booted.
            or not any(marker in self._output for marker in _GUEST_BOOT_MARKERS)
        ):
            return
        self._status_sent = True
        self._interaction.write_input(status_script().encode())
        deadline = time.monotonic() + STATUS_TIMEOUT_SECONDS
        while self._monitor.status_queries == 0:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self._fail(
                    TimeoutError(
                        "nvx-time status did not exit within "
                        f"{STATUS_TIMEOUT_SECONDS:g}s"
                    )
                )
            try:
                chunk = self._chunks.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                continue
            if chunk is None:
                self._fail(self._exited("nvx-time status exited"))
            self._consume(chunk)

    def _consume(self, chunk: bytes) -> None:
        self._output.extend(chunk)
        try:
            self._monitor.feed(chunk)
        except TimeAbiFailure as error:
            self._fail(error)

    def _exited(self, before: str) -> Exception:
        try:
            returncode: int | None = self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            returncode = None
        try:
            self._monitor.finish()
            self._monitor.check_exit(returncode)
        except TimeAbiFailure as error:
            return error
        return self._monitor.exit_error(returncode, when=f"before {before}")

    def wait_for_time_abi(self, phase: str, timeout: float) -> dict[str, str]:
        """Wait for a passing ``NVX-TIME-ABI`` marker of a conformance phase."""
        deadline = time.monotonic() + timeout
        while True:
            if phase == "boot" and self._monitor.boot is not None:
                return self._monitor.boot
            if phase == "restore" and self._monitor.restores:
                return self._monitor.restores[-1]
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self._fail(
                    TimeoutError(
                        f"time ABI {phase} marker was not observed within {timeout:g}s"
                    )
                )
            try:
                chunk = self._chunks.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                continue
            if chunk is None:
                self._fail(self._exited(f"the time ABI {phase} marker"))
            self._consume(chunk)

    def wait_for(self, marker: bytes, timeout: float) -> None:
        if not marker:
            raise ValueError("OpenVMM process marker cannot be empty")
        deadline = time.monotonic() + timeout
        while True:
            index = self._output.find(marker, self._search_offset)
            if index >= 0:
                self._search_offset = index + len(marker)
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self._fail(
                    TimeoutError(
                        f"marker {marker!r} was not observed within {timeout:g}s"
                    )
                )
            try:
                chunk = self._chunks.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                continue
            if chunk is None:
                self._fail(self._exited(repr(marker)))
            self._consume(chunk)

    def wait_for_line(self, marker: bytes, timeout: float) -> None:
        if not marker or b"\n" in marker or b"\r" in marker:
            raise ValueError("OpenVMM process line marker must be one non-empty line")
        deadline = time.monotonic() + timeout
        while True:
            marker_end = _line_marker_end(self._output, marker, self._search_offset)
            if marker_end is not None:
                self._search_offset = marker_end
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self._fail(
                    TimeoutError(
                        f"line marker {marker!r} was not observed within {timeout:g}s"
                    )
                )
            try:
                chunk = self._chunks.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                continue
            if chunk is None:
                self._fail(self._exited(f"line marker {marker!r}"))
            self._consume(chunk)

    def wait(self, timeout: float) -> OpenvmmProcessResult:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self._fail(
                    TimeoutError(
                        f"OpenVMM output did not reach EOF within {timeout:g}s"
                    )
                )
            try:
                chunk = self._chunks.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                continue
            if chunk is None:
                break
            self._consume(chunk)
        remaining = remaining_timeout(deadline)
        try:
            returncode = self.process.wait(timeout=remaining)
        except subprocess.TimeoutExpired:
            self._fail(TimeoutError(f"OpenVMM did not exit within {timeout:g}s"))
        try:
            self._monitor.finish()
            self._monitor.check_exit(returncode)
        except TimeAbiFailure as error:
            self._fail(error)
        self._finished = True
        self._write_log()
        return OpenvmmProcessResult(returncode, bytes(self._output))

    def close(self) -> None:
        """Stop OpenVMM if it still runs, and keep its output in the log.

        It never raises a time ABI failure, so the error that led here on a
        failure path surfaces unmasked. Every scenario's success path ends in
        ``wait``, which scans the output to EOF and checks the exit status
        (``test_every_openvmm_process_success_path_waits``).
        """
        if not self._finished and self.process.poll() is None:
            terminate(self.process)
        self._drain_available()
        self._write_log()
        self._interaction.close()
        self._finished = True

    def _drain_available(self) -> None:
        """Keep the output still queued; failure paths only, so no scan."""
        while True:
            try:
                chunk = self._chunks.get_nowait()
            except queue.Empty:
                return
            if chunk is not None:
                self._output.extend(chunk)

    def _write_log(self) -> None:
        self._log_path.parent.mkdir(parents=True, exist_ok=True)
        self._log_path.write_bytes(self._output)

    def _fail(self, error: Exception):
        self.close()
        tail = self._output[-4096:].decode("utf-8", "replace")
        if tail:
            raise RuntimeError(f"{error}\n--- OpenVMM output ---\n{tail}") from error
        raise error

    def __enter__(self) -> OpenvmmProcess:
        return self

    def __exit__(
        self,
        _exc_type: type[BaseException] | None,
        _exc_value: BaseException | None,
        _traceback: TracebackType | None,
    ) -> None:
        self.close()


class TcpConsole:
    def __init__(
        self,
        connection: socket.socket,
        monitor: TimeAbiMonitor | None = None,
        *,
        time_abi_status: bool = False,
    ) -> None:
        self._connection = connection
        self._output = bytearray()
        self._search_offset = 0
        self._monitor = monitor
        self._query_status = (
            time_abi_status and monitor is not None and monitor.cold_boot
        )
        self._status_sent = False
        self._closed = False
        self._failure: TimeAbiFailure | None = None

    @classmethod
    def connect(
        cls,
        address: tuple[str, int],
        timeout: float,
        *,
        monitor: TimeAbiMonitor | None = None,
        time_abi_status: bool = False,
    ) -> TcpConsole:
        """Connect to a virtio console.

        ``monitor`` scans the console's output for the time ABI, as
        OpenvmmProcess scans OpenVMM's. With ``time_abi_status``, a
        cold-booted guest whose shell is on this console answers a time ABI
        status query before the scenario's first input, the same contract as
        OpenvmmProcess's ``time_abi_status``.
        """
        deadline = time.monotonic() + timeout
        while True:
            try:
                connection = socket.create_connection(address, timeout=0.25)
                try:
                    connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                except BaseException:
                    connection.close()
                    raise
                return cls(connection, monitor, time_abi_status=time_abi_status)
            except OSError as error:
                if time.monotonic() >= deadline:
                    raise TimeoutError(
                        f"failed to connect to virtio console at {address}"
                    ) from error

    def _consume(self, chunk: bytes) -> None:
        self._output.extend(chunk)
        if self._monitor is not None:
            self._monitor.feed(chunk)

    def send_bytes(self, data: bytes) -> None:
        self._query_status_first()
        self._connection.sendall(data)

    def _query_status_first(self) -> None:
        if (
            not self._query_status
            or self._status_sent
            # The guest's shell is not on this console, or has not booted.
            or not any(marker in self._output for marker in _GUEST_BOOT_MARKERS)
        ):
            return
        assert self._monitor is not None
        self._status_sent = True
        self._connection.sendall(status_script().encode())
        deadline = time.monotonic() + STATUS_TIMEOUT_SECONDS
        while self._monitor.status_queries == 0:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    "nvx-time status did not exit within "
                    f"{STATUS_TIMEOUT_SECONDS:g}s on the TCP console"
                )
            self._connection.settimeout(min(remaining, 0.25))
            try:
                chunk = self._connection.recv(4096)
            except TimeoutError:
                continue
            if not chunk:
                raise RuntimeError("TCP console closed before nvx-time status exited")
            self._consume(chunk)

    @property
    def output(self) -> bytes:
        return bytes(self._output)

    def send_line(self, line: str) -> None:
        self.send_bytes(f"{line}\n".encode())

    def wait_for(self, marker: bytes, timeout: float) -> None:
        if not marker:
            raise ValueError("TCP console marker cannot be empty")
        deadline = time.monotonic() + timeout
        while True:
            index = self._output.find(marker, self._search_offset)
            if index >= 0:
                self._search_offset = index + len(marker)
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"TCP console marker {marker!r} was not observed")
            self._connection.settimeout(min(remaining, 0.25))
            try:
                chunk = self._connection.recv(4096)
            except TimeoutError:
                continue
            if not chunk:
                raise RuntimeError(f"TCP console closed before marker {marker!r}")
            self._consume(chunk)

    def wait_for_line(self, marker: bytes, timeout: float) -> None:
        if not marker or b"\n" in marker or b"\r" in marker:
            raise ValueError("TCP console line marker must be one non-empty line")
        deadline = time.monotonic() + timeout
        while True:
            marker_end = _line_marker_end(self._output, marker, self._search_offset)
            if marker_end is not None:
                self._search_offset = marker_end
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    f"TCP console line marker {marker!r} was not observed"
                )
            self._connection.settimeout(min(remaining, 0.25))
            try:
                chunk = self._connection.recv(4096)
            except TimeoutError:
                continue
            if not chunk:
                raise RuntimeError(f"TCP console closed before line marker {marker!r}")
            self._consume(chunk)

    def finish(self, *, check: bool = True) -> bytes:
        """Drain the console until it goes quiet, close it, and return its output.

        The drained bytes reach the monitor too, so a violation or failed check
        that arrived after the last awaited marker still fails the scenario.
        Error paths pass ``check=False``: the monitor still scans the bytes for
        the log, but nothing is raised over the error being handled.
        """
        if not self._closed:
            self._closed = True
            self._connection.settimeout(0.1)
            try:
                while chunk := self._connection.recv(4096):
                    try:
                        self._consume(chunk)
                    except TimeAbiFailure as error:
                        # Keep draining, so the log holds everything.
                        self._failure = self._failure or error
            except (TimeoutError, ConnectionError, OSError):
                pass
            finally:
                self._connection.close()
            if self._monitor is not None:
                try:
                    self._monitor.finish()
                except TimeAbiFailure as error:
                    self._failure = self._failure or error
        if check and self._failure is not None:
            raise self._failure
        return bytes(self._output)

    def close(self) -> None:
        self._closed = True
        self._connection.close()

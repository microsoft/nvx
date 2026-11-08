"""Binary-safe foreground process control for OpenVMM integration tests."""

from __future__ import annotations

import os
import queue
import socket
import struct
import subprocess
import sys
import threading
import time
from collections.abc import Callable, Mapping, Sequence
from contextlib import suppress
from pathlib import Path
from types import TracebackType
from typing import NamedTuple, Protocol

from .benchmark import (
    InteractiveProcess,
    append_cleanup_failures,
    combined_failure,
    raise_with_output,
    run_cleanup,
    terminate_after_failure,
)
from .common import remaining_timeout
from .time_abi import (
    GUEST_BOOT_MARKERS,
    STATUS_TIMEOUT_SECONDS,
    TimeAbiFailure,
    TimeAbiMonitor,
    status_script,
)

_GUEST_BOOT_MARKERS = tuple(marker.encode() for marker in GUEST_BOOT_MARKERS)
# How long a read of a named pipe waits before it looks for a shutdown.
_PIPE_READ_WAIT_SECONDS = 0.1
# How long the console's reader may take to stop once its stream is shut down.
_CONSOLE_READER_STOP_SECONDS = 5.0
# How long a connection to a boot console that does not listen yet waits
# before it tries again.
_CONSOLE_CONNECT_RETRY_SECONDS = 0.001
# How long a write to a boot console waits for the guest to take its input.
_CONSOLE_WRITE_TIMEOUT_SECONDS = 60.0
# CreateFile's SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION: a pipe's
# server may identify the harness's account but not impersonate it.
_PIPE_IDENTIFICATION_ONLY = 0x00100000 | 0x00010000


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


def _substring_marker_end(
    output: bytearray,
    marker: bytes,
    offset: int,
) -> int | None:
    index = output.find(marker, offset)
    return None if index < 0 else index + len(marker)


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
        # The monitor exists before OpenVMM does, so that nothing that it
        # raises leaves OpenVMM running outside the cleanup below (#720).
        self._monitor = TimeAbiMonitor(command)
        self._query_status = time_abi_status and self._monitor.cold_boot
        process_environment = os.environ.copy()
        process_environment["OPENVMM_LOG"] = "off"
        if environment is not None:
            process_environment.update(environment)
        # The transcript is open before OpenVMM starts, so a failure to create it
        # stops the run here, and every consumed byte reaches it as it is read.
        log_path.parent.mkdir(parents=True, exist_ok=True)
        self._log = log_path.open("wb")
        try:
            self._interaction = InteractiveProcess(command, process_environment)
        except BaseException:
            self._log.close()
            raise
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
        except BaseException as error:
            cleanup_errors = terminate_after_failure(self._interaction.process)
            cleanup_errors += run_cleanup(self._interaction.close, self._log.close)
            if not cleanup_errors:
                raise
            combined = append_cleanup_failures(error, cleanup_errors)
            if combined is error:
                raise
            raise combined from error
        self._output = bytearray()
        self._search_offset = 0
        self._log_path = log_path
        self._finished = False
        self._released = False
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
        self._record(chunk)
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
        self._wait_for_marker(marker, timeout, _substring_marker_end, "marker", "")

    def wait_for_line(self, marker: bytes, timeout: float) -> None:
        if not marker or b"\n" in marker or b"\r" in marker:
            raise ValueError("OpenVMM process line marker must be one non-empty line")
        self._wait_for_marker(
            marker, timeout, _line_marker_end, "line marker", "line marker"
        )

    def _wait_for_marker(
        self,
        marker: bytes,
        timeout: float,
        finder: Callable[[bytearray, bytes, int], int | None],
        label: str,
        exit_label: str,
    ) -> None:
        deadline = time.monotonic() + timeout
        while True:
            marker_end = finder(self._output, marker, self._search_offset)
            if marker_end is not None:
                self._search_offset = marker_end
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self._fail(
                    TimeoutError(
                        f"{label} {marker!r} was not observed within {timeout:g}s"
                    )
                )
            try:
                chunk = self._chunks.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                continue
            if chunk is None:
                self._fail(
                    self._exited(f"{exit_label + ' ' if exit_label else ''}{marker!r}")
                )
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
        self._close_log()
        return OpenvmmProcessResult(returncode, bytes(self._output))

    def close(self) -> None:
        """Stop OpenVMM if it still runs, and keep its output in the log.

        It never raises a time ABI failure, so the error that led here on a
        failure path surfaces unmasked. Every scenario's success path ends in
        ``wait``, which scans the output to EOF and checks the exit status
        (``test_every_openvmm_process_success_path_waits``). It releases the
        interaction and the log even if OpenVMM outlives its kill, and then
        raises the first failure, the kill's ``ProcessExitTimeout`` before
        those of the interaction's shutdown and of the log, with the others
        appended.
        """
        cleanup_errors = self._release()
        if cleanup_errors:
            raise combined_failure(cleanup_errors)

    def _release(self) -> list[BaseException]:
        """Stop OpenVMM if it still runs, release the interaction and the log
        once, and return the failures of the kill, of the shutdown, of the
        drain, and of the log's close.

        An OpenVMM that outlives its kill can keep its output open, so its
        reader may not stop either, and every failure is kept.
        """
        if self._released:
            return []
        self._released = True
        cleanup_errors: list[BaseException] = []
        if not self._finished and self.process.poll() is None:
            cleanup_errors = terminate_after_failure(self.process)

        def drain() -> None:
            if not self._log.closed:
                self._drain_available()

        try:
            # The reader queues its last chunks while the interaction shuts down,
            # so drain only after it has stopped. Drain even if that shutdown
            # fails, and release the transcript either way.
            cleanup_errors += run_cleanup(
                self._interaction.close, drain, self._close_log
            )
        finally:
            self._finished = True
        return cleanup_errors

    def _drain_available(self) -> None:
        """Keep the output still queued; failure paths only, so no scan.

        A transcript write error does not stop the drain: every queued chunk is
        kept in memory, and the first error is re-raised once the queue is empty.
        """
        error: Exception | None = None
        while True:
            try:
                chunk = self._chunks.get_nowait()
            except queue.Empty:
                break
            if chunk is None:
                continue
            self._output.extend(chunk)
            if error is None:
                try:
                    self._log.write(chunk)
                    self._log.flush()
                except Exception as write_error:
                    error = write_error
        if error is not None:
            raise error

    def _record(self, chunk: bytes) -> None:
        """Keep a consumed chunk in memory and in the transcript, flushed for readers."""
        self._output.extend(chunk)
        self._log.write(chunk)
        self._log.flush()

    def _close_log(self) -> None:
        if not self._log.closed:
            self._log.close()

    def _fail(self, error: Exception):
        cleanup_errors = self._release()
        tail = self._output[-4096:].decode("utf-8", "replace")
        raise_with_output(error, tail, cleanup_errors)

    def __enter__(self) -> OpenvmmProcess:
        return self

    def __exit__(
        self,
        _exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        _traceback: TracebackType | None,
    ) -> None:
        if exc_value is None:
            self.close()
            return
        # The error that left the block stays the one that surfaces, unless
        # its message can't show the cleanup failures, which the error that it
        # caused then shows.
        cleanup_errors = self._release()
        if cleanup_errors:
            combined = append_cleanup_failures(exc_value, cleanup_errors)
            if combined is not exc_value:
                raise combined


class ConsoleConnection(Protocol):
    """The stream of a boot console: the part of a socket's interface that
    BootConsole uses. A read blocks until data arrives, and returns nothing
    once the stream ends or ``shutdown`` stops it."""

    def recv(self, bufsize: int, /) -> bytes: ...

    def sendall(self, data: bytes, /) -> None: ...

    def shutdown(self, how: int, /) -> None: ...

    def close(self) -> None: ...


def _write_timeout() -> TimeoutError:
    """The failure of a write that the guest did not take in time."""
    return TimeoutError(
        "the guest took no input on the boot console within "
        f"{_CONSOLE_WRITE_TIMEOUT_SECONDS:g}s"
    )


class NamedPipeConnection:
    """A client of a Windows named pipe, as a ConsoleConnection.

    The handle is overlapped, so that a read that waits for data never holds
    a write back, as Windows holds every operation on a synchronous handle
    behind the one in progress.
    """

    def __init__(self, endpoint: str) -> None:
        self._stopped = threading.Event()
        if sys.platform != "win32":
            raise OSError("named pipes are available only on Windows")
        import _winapi

        # Fails with an OSError until OpenVMM creates the pipe, and while
        # another client holds its instance. A process that took the pipe's
        # name may learn who connected, but cannot act as them.
        self._handle = _winapi.CreateFile(
            endpoint.replace("/", "\\"),
            _winapi.GENERIC_READ | _winapi.GENERIC_WRITE,
            0,
            _winapi.NULL,
            _winapi.OPEN_EXISTING,
            _winapi.FILE_FLAG_OVERLAPPED | _PIPE_IDENTIFICATION_ONLY,
            _winapi.NULL,
        )

    def recv(self, bufsize: int, /) -> bytes:
        if sys.platform != "win32":
            raise OSError("named pipes are available only on Windows")
        import _winapi

        while not self._stopped.is_set():
            try:
                overlapped, _ = _winapi.ReadFile(self._handle, bufsize, overlapped=True)
                # A read that waits too long is canceled, so that shutdown
                # stops the reader; it keeps whatever arrived before.
                if (
                    _winapi.WaitForSingleObject(
                        overlapped.event, int(_PIPE_READ_WAIT_SECONDS * 1000)
                    )
                    != _winapi.WAIT_OBJECT_0
                ):
                    overlapped.cancel()
                overlapped.GetOverlappedResult(True)
            except BrokenPipeError:
                return b""
            data = overlapped.getbuffer()
            if data:
                return data
        return b""

    def sendall(self, data: bytes, /) -> None:
        if sys.platform != "win32":
            raise OSError("named pipes are available only on Windows")
        import _winapi

        view = memoryview(data)
        while view:
            overlapped, _ = _winapi.WriteFile(self._handle, view, overlapped=True)
            # A write that the guest does not take in time is canceled.
            timed_out = (
                _winapi.WaitForSingleObject(
                    overlapped.event, int(_CONSOLE_WRITE_TIMEOUT_SECONDS * 1000)
                )
                != _winapi.WAIT_OBJECT_0
            )
            if timed_out:
                overlapped.cancel()
            written, _ = overlapped.GetOverlappedResult(True)
            if timed_out and written < len(view):
                raise _write_timeout()
            if not written:
                raise BrokenPipeError("the named pipe took no data")
            view = view[written:]

    def shutdown(self, how: int, /) -> None:
        self._stopped.set()

    def close(self) -> None:
        if sys.platform != "win32":
            raise OSError("named pipes are available only on Windows")
        import _winapi

        self._stopped.set()
        _winapi.CloseHandle(self._handle)


def _connect_console(endpoint: str) -> ConsoleConnection:
    """Connect to the boot console that listens at ``endpoint``: a named pipe
    on Windows, and a Unix socket elsewhere. Fails with an OSError while the
    endpoint does not take the connection, at once, so that the caller's
    deadline bounds the wait."""
    if sys.platform == "win32":
        return NamedPipeConnection(endpoint)
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        # A blocking connection to a listener whose backlog is full would
        # wait until the listener accepts; this one fails instead.
        connection.setblocking(False)
        connection.connect(endpoint)
        connection.setblocking(True)
        # A write that the guest does not take in time fails with
        # BlockingIOError, while reads still wait for the guest.
        seconds, fraction = divmod(_CONSOLE_WRITE_TIMEOUT_SECONDS, 1)
        connection.setsockopt(
            socket.SOL_SOCKET,
            socket.SO_SNDTIMEO,
            struct.pack("ll", int(seconds), int(fraction * 1_000_000)),
        )
    except BaseException:
        connection.close()
        raise
    return connection


class BootConsole:
    """A client of a microVM's boot console, the virtio console ``hvc1``, which
    OpenVMM serves on a Unix socket or a Windows named pipe.

    Boot consoles never listen on TCP: a guest's gateway maps the guest's TCP
    flows onto host loopback, so any guest could reach a loopback listener
    (microsoft/nvx#551).

    A thread reads the console as the guest prints, whatever the scenario
    waits for. A socket or a pipe holds much less unread output than TCP
    did, and a guest whose output nobody reads stops at its next write. A
    write of the harness waits for the guest to take its input only so long,
    so a guest that stops reading fails the scenario instead of holding it.
    """

    def __init__(
        self,
        connection: ConsoleConnection,
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
        self._stopped = False
        self._failure: TimeAbiFailure | None = None
        # Each chunk that the console printed, then None once it ended.
        self._chunks: queue.Queue[bytes | None] = queue.Queue()
        self._reader = threading.Thread(
            target=self._read, name="nvx-boot-console", daemon=True
        )
        self._reader.start()

    @classmethod
    def connect(
        cls,
        endpoint: str,
        timeout: float,
        *,
        monitor: TimeAbiMonitor | None = None,
        time_abi_status: bool = False,
    ) -> BootConsole:
        """Connect to the boot console that listens at ``endpoint``, waiting
        until OpenVMM listens there.

        ``monitor`` scans the console's output for the time ABI, as
        OpenvmmProcess scans OpenVMM's. With ``time_abi_status``, a
        cold-booted guest whose shell is on this console answers a time ABI
        status query before the scenario's first input, the same contract as
        OpenvmmProcess's ``time_abi_status``.
        """
        deadline = time.monotonic() + timeout
        while True:
            try:
                connection = _connect_console(endpoint)
                return cls(connection, monitor, time_abi_status=time_abi_status)
            except OSError as error:
                if time.monotonic() >= deadline:
                    raise TimeoutError(
                        f"failed to connect to the boot console at {endpoint}"
                    ) from error
                # A restored guest may exit soon after OpenVMM listens, and
                # OpenVMM then drops what the guest printed, so the retries
                # follow one another closely.
                time.sleep(_CONSOLE_CONNECT_RETRY_SECONDS)

    def _read(self) -> None:
        """Queue what the console prints until it ends or is shut down."""
        try:
            while chunk := self._connection.recv(65536):
                self._chunks.put(chunk)
        except OSError:
            # The console was reset, or closed under the read.
            pass
        finally:
            self._chunks.put(None)

    def _next(self, timeout: float) -> bytes | None:
        """Return the next chunk that the console printed within ``timeout``,
        or None once it ended, which later calls return too. Raises
        queue.Empty when nothing arrives in time."""
        chunk = self._chunks.get(timeout=timeout)
        if chunk is None:
            self._chunks.put(None)
        return chunk

    def _stop(self) -> None:
        """Stop the reader and close the stream, once."""
        if self._stopped:
            return
        self._stopped = True
        # A shutdown wakes a Linux socket's read, and a close a Windows one's.
        with suppress(OSError):
            self._connection.shutdown(socket.SHUT_RDWR)
        self._connection.close()
        self._reader.join(_CONSOLE_READER_STOP_SECONDS)

    def _consume(self, chunk: bytes) -> None:
        self._output.extend(chunk)
        if self._monitor is not None:
            self._monitor.feed(chunk)

    def send_bytes(self, data: bytes) -> None:
        self._query_status_first()
        self._send(data)

    def _send(self, data: bytes) -> None:
        try:
            self._connection.sendall(data)
        except BlockingIOError as error:
            # A Unix socket's send timeout expired.
            raise _write_timeout() from error

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
        self._send(status_script().encode())
        deadline = time.monotonic() + STATUS_TIMEOUT_SECONDS
        while self._monitor.status_queries == 0:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    "nvx-time status did not exit within "
                    f"{STATUS_TIMEOUT_SECONDS:g}s on the boot console"
                )
            try:
                chunk = self._next(min(remaining, 0.25))
            except queue.Empty:
                continue
            if chunk is None:
                raise RuntimeError("boot console closed before nvx-time status exited")
            self._consume(chunk)

    @property
    def output(self) -> bytes:
        return bytes(self._output)

    def send_line(self, line: str) -> None:
        self.send_bytes(f"{line}\n".encode())

    def wait_for(self, marker: bytes, timeout: float) -> None:
        if not marker:
            raise ValueError("boot console marker cannot be empty")
        deadline = time.monotonic() + timeout
        while True:
            index = self._output.find(marker, self._search_offset)
            if index >= 0:
                self._search_offset = index + len(marker)
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"boot console marker {marker!r} was not observed")
            try:
                chunk = self._next(min(remaining, 0.25))
            except queue.Empty:
                continue
            if chunk is None:
                raise RuntimeError(f"boot console closed before marker {marker!r}")
            self._consume(chunk)

    def wait_for_line(self, marker: bytes, timeout: float) -> None:
        if not marker or b"\n" in marker or b"\r" in marker:
            raise ValueError("boot console line marker must be one non-empty line")
        deadline = time.monotonic() + timeout
        while True:
            marker_end = _line_marker_end(self._output, marker, self._search_offset)
            if marker_end is not None:
                self._search_offset = marker_end
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    f"boot console line marker {marker!r} was not observed"
                )
            try:
                chunk = self._next(min(remaining, 0.25))
            except queue.Empty:
                continue
            if chunk is None:
                raise RuntimeError(f"boot console closed before line marker {marker!r}")
            self._consume(chunk)

    @property
    def status_queries(self) -> int:
        """How many ``nvx-time status`` queries the monitor has observed so far."""
        return 0 if self._monitor is None else self._monitor.status_queries

    def wait_for_time_abi_status(
        self, timeout: float, *, since: int | None = None
    ) -> None:
        """Wait until a ``nvx-time status`` query exits after ``since``.

        ``since`` is a ``status_queries`` count captured before the caller's
        earlier reads, so a query that one of those reads already consumed still
        counts. By default the wait is for the next query after this call.

        The monitor validates what the query printed when its exit line
        arrives, so a failed or pending check raises here.
        """
        if self._monitor is None:
            raise ValueError("boot console has no time ABI monitor")
        baseline = self._monitor.status_queries if since is None else since
        deadline = time.monotonic() + timeout
        while self._monitor.status_queries == baseline:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    f"nvx-time status did not exit within {timeout:g}s on the boot console"
                )
            try:
                chunk = self._next(min(remaining, 0.25))
            except queue.Empty:
                continue
            if chunk is None:
                raise RuntimeError("boot console closed before nvx-time status exited")
            self._consume(chunk)

    def _consume_to_the_end(self, chunk: bytes) -> None:
        try:
            self._consume(chunk)
        except TimeAbiFailure as error:
            # Keep draining, so the log holds everything.
            self._failure = self._failure or error

    def finish(self, *, check: bool = True) -> bytes:
        """Drain the console until it goes quiet, close it, and return its output.

        The drained bytes reach the monitor too, so a violation or failed check
        that arrived after the last awaited marker still fails the scenario.
        Error paths pass ``check=False``: the monitor still scans the bytes for
        the log, but nothing is raised over the error being handled.
        """
        if not self._closed:
            self._closed = True
            try:
                while True:
                    try:
                        chunk = self._next(0.1)
                    except queue.Empty:
                        break
                    if chunk is None:
                        break
                    self._consume_to_the_end(chunk)
            finally:
                self._stop()
            # What the reader queued before it stopped.
            while True:
                try:
                    chunk = self._chunks.get_nowait()
                except queue.Empty:
                    break
                if chunk is not None:
                    self._consume_to_the_end(chunk)
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
        self._stop()

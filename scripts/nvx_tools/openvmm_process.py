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
    ) -> None:
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
        self._reader.start()
        self._output = bytearray()
        self._search_offset = 0
        self._log_path = log_path
        self._finished = False

    @property
    def process(self):
        return self._interaction.process

    @property
    def output(self) -> bytes:
        return bytes(self._output)

    def send_bytes(self, data: bytes) -> None:
        self._interaction.write_input(data)

    def send_line(self, line: str) -> None:
        self.send_bytes(f"{line}\n".encode())

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
                if self.process.poll() is not None:
                    self._drain_available()
                    index = self._output.find(marker, self._search_offset)
                    if index >= 0:
                        self._search_offset = index + len(marker)
                        return
                    self._fail(
                        RuntimeError(
                            f"OpenVMM exited with status {self.process.returncode} before {marker!r}"
                        )
                    )
                continue
            if chunk is None:
                self._fail(
                    RuntimeError(
                        f"OpenVMM exited with status {self.process.poll()} before {marker!r}"
                    )
                )
            self._output.extend(chunk)

    def wait(self, timeout: float) -> OpenvmmProcessResult:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self._fail(TimeoutError(f"OpenVMM did not exit within {timeout:g}s"))
            try:
                chunk = self._chunks.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                if self.process.poll() is not None:
                    self._drain_available()
                    break
                continue
            if chunk is None:
                break
            self._output.extend(chunk)
        remaining = max(0.0, deadline - time.monotonic())
        try:
            returncode = self.process.wait(timeout=remaining)
        except subprocess.TimeoutExpired:
            self._fail(TimeoutError(f"OpenVMM did not exit within {timeout:g}s"))
        self._finished = True
        self._write_log()
        return OpenvmmProcessResult(returncode, bytes(self._output))

    def close(self) -> None:
        if not self._finished and self.process.poll() is None:
            terminate(self.process)
        self._drain_available()
        self._write_log()
        self._interaction.close()
        self._finished = True

    def _drain_available(self) -> None:
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
    def __init__(self, connection: socket.socket) -> None:
        self._connection = connection
        self._output = bytearray()
        self._search_offset = 0

    @classmethod
    def connect(cls, address: tuple[str, int], timeout: float) -> TcpConsole:
        deadline = time.monotonic() + timeout
        while True:
            try:
                connection = socket.create_connection(address, timeout=0.25)
                connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                return cls(connection)
            except OSError as error:
                if time.monotonic() >= deadline:
                    raise TimeoutError(
                        f"failed to connect to virtio console at {address}"
                    ) from error

    def send_bytes(self, data: bytes) -> None:
        self._connection.sendall(data)

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
            self._output.extend(chunk)

    def finish(self) -> bytes:
        self._connection.settimeout(0.1)
        try:
            while chunk := self._connection.recv(4096):
                self._output.extend(chunk)
        except (TimeoutError, ConnectionError, OSError):
            pass
        finally:
            self._connection.close()
        return bytes(self._output)

    def close(self) -> None:
        self._connection.close()

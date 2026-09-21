"""Authenticated control-session client for managed NVX microVM workloads."""

from __future__ import annotations

import errno
import os
import secrets
import socket
import struct
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast

from .common import ScriptError

OUTER_HEADER = struct.Struct("<4sHBB16sQQI")
APP_HEADER = struct.Struct("<4sBBHQiI")
OUTER_MAX_PAYLOAD = 65_536
APP_MAX_ARGUMENTS = 64
APP_MAX_ARGUMENT_BYTES = 4096

OUTER_HOST_ATTACH = 2
OUTER_RESET = 3
OUTER_DATA = 5
OUTER_WAIT = 6
OUTER_READY = 7
OUTER_ERROR = 8

APP_PING = 1
APP_EXEC = 2
APP_STOP = 3
APP_READY = 0x81
APP_STDOUT = 0x82
APP_STDERR = 0x83
APP_EXIT = 0x84
APP_STOPPED = 0x85
APP_ERROR = 0xFF
MANAGED_EXIT_CATEGORIES = frozenset(
    {"exit", "timeout", "output-limit", "signal", "failed"}
)


@dataclass(frozen=True)
class ManagedExecResult:
    returncode: int
    category: str
    stdout: bytes
    stderr: bytes


class _SocketStream:
    def __init__(self, connection: socket.socket) -> None:
        self._connection = connection

    @classmethod
    def connect(cls, path: Path, timeout: float) -> _SocketStream:
        deadline = time.monotonic() + timeout
        family = getattr(socket, "AF_UNIX", None)
        if family is None:
            raise RuntimeError("Unix-domain sockets are unavailable")
        while True:
            connection = socket.socket(cast(int, family), socket.SOCK_STREAM)
            try:
                connection.settimeout(min(0.25, max(0.01, deadline - time.monotonic())))
                connection.connect(os.fspath(path))
                return cls(connection)
            except OSError as error:
                connection.close()
                if time.monotonic() >= deadline:
                    raise TimeoutError(
                        f"managed control endpoint did not become available: {path}"
                    ) from error
                time.sleep(0.025)

    def read_exact(self, length: int, deadline: float) -> bytes:
        output = bytearray()
        while len(output) != length:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("managed control response timed out")
            self._connection.settimeout(min(remaining, 0.25))
            try:
                chunk = self._connection.recv(length - len(output))
            except TimeoutError:
                continue
            if not chunk:
                raise ConnectionError("managed control endpoint closed")
            output.extend(chunk)
        return bytes(output)

    def write_all(self, data: bytes) -> None:
        self._connection.sendall(data)

    def close(self) -> None:
        self._connection.close()


if os.name == "nt":
    import ctypes
    import msvcrt

    _kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _peek_named_pipe = _kernel32.PeekNamedPipe
    _peek_named_pipe.argtypes = [
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.c_uint32,
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_uint32),
        ctypes.c_void_p,
    ]
    _peek_named_pipe.restype = ctypes.c_int
else:
    ctypes = cast(Any, None)
    msvcrt = cast(Any, None)
    _peek_named_pipe = cast(Any, None)


class _NamedPipeStream:
    def __init__(self, fd: int) -> None:
        self._fd = fd

    @classmethod
    def connect(cls, path: Path, timeout: float) -> _NamedPipeStream:
        normalized = os.fspath(path).replace("/", "\\")
        deadline = time.monotonic() + timeout
        flags = os.O_RDWR | getattr(os, "O_BINARY", 0)
        while True:
            try:
                return cls(os.open(normalized, flags))
            except OSError as error:
                if time.monotonic() >= deadline:
                    raise TimeoutError(
                        f"managed control endpoint did not become available: {path}"
                    ) from error
                if error.errno not in (
                    errno.ENOENT,
                    errno.EACCES,
                    errno.EBUSY,
                    errno.EAGAIN,
                    # The Windows CRT reports the reconnect transition as EINVAL.
                    errno.EINVAL,
                ):
                    raise
                time.sleep(0.025)

    def _available(self) -> int:
        available = ctypes.c_uint32()
        handle = msvcrt.get_osfhandle(self._fd)
        if not _peek_named_pipe(
            handle,
            None,
            0,
            None,
            ctypes.byref(available),
            None,
        ):
            error = ctypes.get_last_error()
            if error in (109, 233):
                raise ConnectionError("managed control endpoint closed")
            raise OSError(error, "PeekNamedPipe failed")
        return int(available.value)

    def read_exact(self, length: int, deadline: float) -> bytes:
        output = bytearray()
        while len(output) != length:
            if time.monotonic() >= deadline:
                raise TimeoutError("managed control response timed out")
            available = self._available()
            if available == 0:
                time.sleep(0.01)
                continue
            chunk = os.read(self._fd, min(length - len(output), available))
            if not chunk:
                raise ConnectionError("managed control endpoint closed")
            output.extend(chunk)
        return bytes(output)

    def write_all(self, data: bytes) -> None:
        remaining = memoryview(data)
        while remaining:
            count = os.write(self._fd, remaining)
            if count <= 0:
                raise ConnectionError("managed control endpoint closed")
            remaining = remaining[count:]

    def close(self) -> None:
        os.close(self._fd)


class ControlSession:
    def __init__(self, stream: _SocketStream | _NamedPipeStream) -> None:
        self._stream = stream
        self._instance_id = bytes(16)
        self._epoch = 0
        self._send_sequence = 0
        self._receive_sequence = 0

    @classmethod
    def connect(
        cls,
        endpoint: Path,
        capability: bytes,
        timeout: float,
    ) -> ControlSession:
        if len(capability) != 32 or capability == bytes(32):
            raise ValueError("control capability must be 32 nonzero bytes")
        stream = (
            _NamedPipeStream.connect(endpoint, timeout)
            if os.name == "nt"
            else _SocketStream.connect(endpoint, timeout)
        )
        session = cls(stream)
        try:
            session._write_outer(
                OUTER_HOST_ATTACH,
                bytes(16),
                0,
                0,
                capability,
            )
            deadline = time.monotonic() + timeout
            while True:
                record_type, instance_id, epoch, sequence, payload = (
                    session._read_outer(deadline)
                )
                if payload:
                    raise ScriptError(
                        "control attach response carried an invalid payload"
                    )
                if record_type == OUTER_WAIT:
                    continue
                if record_type == OUTER_ERROR:
                    raise ScriptError("control capability authentication failed")
                if record_type != OUTER_READY or instance_id == bytes(16) or epoch == 0:
                    raise ScriptError(
                        "control endpoint returned an invalid attach response"
                    )
                session._instance_id = instance_id
                session._epoch = epoch
                session._receive_sequence = sequence + 1
                return session
        except BaseException:
            stream.close()
            raise

    def _write_outer(
        self,
        record_type: int,
        instance_id: bytes,
        epoch: int,
        sequence: int,
        payload: bytes,
    ) -> None:
        if len(payload) > OUTER_MAX_PAYLOAD:
            raise ValueError("control payload exceeds the outer protocol limit")
        header = OUTER_HEADER.pack(
            b"NVXS",
            1,
            record_type,
            0,
            instance_id,
            epoch,
            sequence,
            len(payload),
        )
        self._stream.write_all(header + payload)

    def _read_outer(
        self,
        deadline: float,
    ) -> tuple[int, bytes, int, int, bytes]:
        header = self._stream.read_exact(OUTER_HEADER.size, deadline)
        magic, version, record_type, flags, instance_id, epoch, sequence, length = (
            OUTER_HEADER.unpack(header)
        )
        if magic != b"NVXS" or version != 1 or flags != 0 or length > OUTER_MAX_PAYLOAD:
            raise ScriptError("control endpoint returned an invalid outer record")
        payload = self._stream.read_exact(length, deadline) if length else b""
        return record_type, instance_id, epoch, sequence, payload

    def _send_app(
        self,
        kind: int,
        request_id: int,
        payload: bytes = b"",
    ) -> None:
        frame = (
            APP_HEADER.pack(
                b"NVXC",
                1,
                kind,
                0,
                request_id,
                0,
                len(payload),
            )
            + payload
        )
        self._write_outer(
            OUTER_DATA,
            self._instance_id,
            self._epoch,
            self._send_sequence,
            frame,
        )
        self._send_sequence += 1

    def _read_app(
        self,
        deadline: float,
    ) -> tuple[int, int, int, bytes]:
        record_type, instance_id, epoch, sequence, frame = self._read_outer(deadline)
        if record_type == OUTER_RESET:
            raise ConnectionError("managed control session was reset")
        if (
            record_type != OUTER_DATA
            or instance_id != self._instance_id
            or epoch != self._epoch
            or sequence != self._receive_sequence
            or len(frame) < APP_HEADER.size
        ):
            raise ScriptError("control endpoint returned an invalid data record")
        self._receive_sequence += 1
        magic, version, kind, flags, request_id, status, length = APP_HEADER.unpack(
            frame[: APP_HEADER.size]
        )
        payload = frame[APP_HEADER.size :]
        if magic != b"NVXC" or version != 1 or flags != 0 or length != len(payload):
            raise ScriptError("control endpoint returned an invalid application frame")
        return kind, request_id, status, payload

    @staticmethod
    def _request_id() -> int:
        return secrets.randbits(64) or 1

    def ping(self, timeout: float) -> None:
        request_id = self._request_id()
        self._send_app(APP_PING, request_id)
        kind, response_id, status, payload = self._read_app(time.monotonic() + timeout)
        if kind != APP_READY or response_id != request_id or status != 0 or payload:
            raise ScriptError("managed guest did not acknowledge readiness")

    def exec(
        self,
        arguments: tuple[str, ...],
        *,
        timeout_ms: int,
        response_timeout: float,
    ) -> ManagedExecResult:
        if not response_timeout > 0:
            raise ValueError("managed exec response timeout must be positive")
        if not 1 <= len(arguments) <= APP_MAX_ARGUMENTS:
            raise ValueError("managed exec requires 1 through 64 arguments")
        encoded: list[bytes] = []
        for argument in arguments:
            value = argument.encode("utf-8")
            if not value or len(value) > APP_MAX_ARGUMENT_BYTES or b"\0" in value:
                raise ValueError("managed exec argument is empty or exceeds 4096 bytes")
            encoded.append(struct.pack("<I", len(value)) + value)
        if not arguments[0].startswith("/"):
            raise ValueError("managed exec entrypoint must be absolute")
        if not 0 <= timeout_ms <= 3_600_000:
            raise ValueError("managed exec timeout must be 0 through 3600000 ms")
        payload = struct.pack("<IHH", timeout_ms, len(arguments), 0) + b"".join(encoded)
        if len(payload) + APP_HEADER.size > OUTER_MAX_PAYLOAD:
            raise ValueError("managed exec request exceeds the protocol limit")

        request_id = self._request_id()
        self._send_app(APP_EXEC, request_id, payload)
        stdout = bytearray()
        stderr = bytearray()
        deadline = time.monotonic() + response_timeout
        while True:
            kind, response_id, status, response = self._read_app(deadline)
            if response_id != request_id:
                raise ScriptError("managed guest returned a mismatched request ID")
            if kind == APP_STDOUT:
                stdout.extend(response)
            elif kind == APP_STDERR:
                stderr.extend(response)
            elif kind == APP_EXIT:
                try:
                    category = response.decode("ascii")
                except UnicodeDecodeError as error:
                    raise ScriptError(
                        "managed guest returned an invalid exit category"
                    ) from error
                if category not in MANAGED_EXIT_CATEGORIES:
                    raise ScriptError(
                        "managed guest returned an unsupported exit category"
                    )
                return ManagedExecResult(status, category, bytes(stdout), bytes(stderr))
            elif kind == APP_ERROR:
                raise ScriptError(
                    "managed guest rejected exec "
                    f"(status={status}, category={response.decode('ascii', 'replace')})"
                )
            else:
                raise ScriptError("managed guest returned an invalid exec response")

    def stop(self, timeout: float) -> None:
        request_id = self._request_id()
        self._send_app(APP_STOP, request_id)
        kind, response_id, status, payload = self._read_app(time.monotonic() + timeout)
        if kind != APP_STOPPED or response_id != request_id or status != 0 or payload:
            raise ScriptError("managed guest did not acknowledge stop")

    def close(self) -> None:
        self._stream.close()

    def __enter__(self) -> ControlSession:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import json
import os
import socket
import struct
import sys
import threading
import time
import unittest
import uuid
from collections.abc import Callable
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import control_session, host_control  # noqa: E402
from nvx_tools.common import ScriptError  # noqa: E402


def _read_like_openvmm(descriptor: int) -> bytes | None:
    """Reads a capability pipe as OpenVMM reads --microvm-control-auth-stdin.

    OpenVMM switches the pipe to non-blocking mode and reads it at once, so it
    gets the capability only if the capability and the end of file are already
    there. Returns what the pipe held, or None if a read would block because the
    pipe's write end is still open.
    """
    if sys.platform == "win32":
        import _winapi
        import msvcrt

        handle = msvcrt.get_osfhandle(descriptor)
        pipe_nowait = 1
        _winapi.SetNamedPipeHandleState(handle, pipe_nowait, None, None)

        def read() -> bytes | None:
            try:
                data, _ = _winapi.ReadFile(handle, 64)
            except OSError as error:
                if error.winerror == _winapi.ERROR_BROKEN_PIPE:
                    return b""
                if error.winerror == _winapi.ERROR_NO_DATA:
                    return None
                raise
            return data

    else:
        os.set_blocking(descriptor, False)

        def read() -> bytes | None:
            try:
                return os.read(descriptor, 64)
            except BlockingIOError:
                return None

    data = b""
    while True:
        chunk = read()
        if chunk is None:
            return None
        if not chunk:
            return data
        data += chunk


def _read_exact(connection: socket.socket, length: int) -> bytes:
    output = bytearray()
    while len(output) != length:
        chunk = connection.recv(length - len(output))
        if not chunk:
            raise RuntimeError("test control connection closed")
        output.extend(chunk)
    return bytes(output)


def _read_outer(connection: socket.socket):
    header = _read_exact(connection, control_session.OUTER_HEADER.size)
    values = control_session.OUTER_HEADER.unpack(header)
    payload = _read_exact(connection, values[-1]) if values[-1] else b""
    return (*values[:-1], payload)


def _write_app(
    connection: socket.socket,
    *,
    instance_id: bytes,
    sequence: int,
    kind: int,
    request_id: int,
    status: int,
    payload: bytes,
) -> None:
    app = (
        control_session.APP_HEADER.pack(
            b"NVXC",
            1,
            kind,
            0,
            request_id,
            status,
            len(payload),
        )
        + payload
    )
    connection.sendall(
        control_session.OUTER_HEADER.pack(
            b"NVXS",
            1,
            control_session.OUTER_DATA,
            0,
            instance_id,
            1,
            sequence,
            len(app),
        )
        + app
    )


class ControlSessionTests(unittest.TestCase):
    def test_exec_streams_output_and_returns_bounded_status(self):
        client, server = socket.socketpair()
        instance = bytes.fromhex("11" * 16)
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = instance
        session._epoch = 1

        def serve() -> None:
            (
                magic,
                version,
                record_type,
                flags,
                actual_instance,
                epoch,
                sequence,
                frame,
            ) = _read_outer(server)
            self.assertEqual(
                (
                    magic,
                    version,
                    record_type,
                    flags,
                    actual_instance,
                    epoch,
                    sequence,
                ),
                (b"NVXS", 1, control_session.OUTER_DATA, 0, instance, 1, 0),
            )
            (
                app_magic,
                app_version,
                kind,
                app_flags,
                request_id,
                status,
                payload_length,
            ) = control_session.APP_HEADER.unpack(
                frame[: control_session.APP_HEADER.size]
            )
            self.assertEqual(
                (app_magic, app_version, kind, app_flags, status),
                (b"NVXC", 1, control_session.APP_EXEC, 0, 0),
            )
            payload = frame[control_session.APP_HEADER.size :]
            self.assertEqual(payload_length, len(payload))
            timeout_ms, argc, reserved = struct.unpack("<IHH", payload[:8])
            self.assertEqual((timeout_ms, argc, reserved), (5000, 3, 0))

            _write_app(
                server,
                instance_id=instance,
                sequence=0,
                kind=control_session.APP_STDOUT,
                request_id=request_id,
                status=0,
                payload=b"hello",
            )
            _write_app(
                server,
                instance_id=instance,
                sequence=1,
                kind=control_session.APP_STDERR,
                request_id=request_id,
                status=0,
                payload=b"warning",
            )
            _write_app(
                server,
                instance_id=instance,
                sequence=2,
                kind=control_session.APP_EXIT,
                request_id=request_id,
                status=7,
                payload=b"exit",
            )
            server.close()

        worker = threading.Thread(target=serve)
        worker.start()
        result = session.exec(
            ("/bin/sh", "-c", "echo hello"),
            timeout_ms=5000,
            response_timeout=5,
        )
        worker.join(timeout=5)
        session.close()

        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.category, "exit")
        self.assertEqual(result.stdout, b"hello")
        self.assertEqual(result.stderr, b"warning")

    def test_exec_rejects_unbounded_or_relative_arguments(self):
        client, server = socket.socketpair()
        session = control_session.ControlSession(control_session._SocketStream(client))
        with self.assertRaisesRegex(ValueError, "absolute"):
            session.exec(("relative",), timeout_ms=0, response_timeout=1)
        with self.assertRaisesRegex(ValueError, "64"):
            session.exec(
                tuple("/bin/true" for _ in range(65)),
                timeout_ms=0,
                response_timeout=1,
            )
        session.close()
        server.close()

    def test_exec_encodes_extended_cwd_and_environment_exactly(self):
        client, server = socket.socketpair()
        instance = bytes.fromhex("33" * 16)
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = instance
        session._epoch = 1
        captured: dict[str, object] = {}

        def serve() -> None:
            *_, frame = _read_outer(server)
            header = control_session.APP_HEADER.unpack(
                frame[: control_session.APP_HEADER.size]
            )
            request_id = header[4]
            payload = frame[control_session.APP_HEADER.size :]
            (
                timeout_ms,
                argc,
                extension,
                flags,
                envc,
                cwd_len,
            ) = struct.unpack("<IHHHHI", payload[:16])
            offset = 16
            values: list[bytes] = []
            for _ in range(argc):
                length = struct.unpack("<I", payload[offset : offset + 4])[0]
                offset += 4
                values.append(payload[offset : offset + length])
                offset += length
            cwd = payload[offset : offset + cwd_len]
            offset += cwd_len
            environment: list[bytes] = []
            for _ in range(envc):
                length = struct.unpack("<I", payload[offset : offset + 4])[0]
                offset += 4
                environment.append(payload[offset : offset + length])
                offset += length
            captured.update(
                timeout_ms=timeout_ms,
                extension=extension,
                flags=flags,
                values=values,
                cwd=cwd,
                environment=environment,
                offset=offset,
                payload_len=len(payload),
            )
            _write_app(
                server,
                instance_id=instance,
                sequence=0,
                kind=control_session.APP_EXIT,
                request_id=request_id,
                status=0,
                payload=b"exit",
            )
            server.close()

        worker = threading.Thread(target=serve)
        worker.start()
        session.exec(
            ("/bin/sh", "-c", "printf exact"),
            timeout_ms=0xFFFFFFFF,
            response_timeout=5,
            cwd="/tmp/space \N{SNOWMAN}",
            environment=("EMPTY=", "SPACED=a b", "EQUALS=a=b", "UTF8=\N{SNOWMAN}"),
        )
        worker.join(timeout=5)
        session.close()

        self.assertEqual(captured["timeout_ms"], 0xFFFFFFFF)
        self.assertEqual(captured["extension"], control_session.APP_EXEC_EXTENDED)
        self.assertEqual(
            captured["flags"],
            control_session.APP_EXEC_CWD_PRESENT
            | control_session.APP_EXEC_ENVIRONMENT_PRESENT,
        )
        self.assertEqual(captured["values"], [b"/bin/sh", b"-c", b"printf exact"])
        self.assertEqual(captured["cwd"], "/tmp/space \N{SNOWMAN}".encode())
        self.assertEqual(
            captured["environment"],
            [
                b"EMPTY=",
                b"SPACED=a b",
                b"EQUALS=a=b",
                "UTF8=\N{SNOWMAN}".encode(),
            ],
        )
        self.assertEqual(captured["offset"], captured["payload_len"])

    def _exec_payload(
        self,
        environment: tuple[str, ...] | None,
        *,
        inherit_default_environment: bool = False,
    ) -> bytes:
        client, server = socket.socketpair()
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = bytes.fromhex("44" * 16)
        session._epoch = 1
        captured = bytearray()

        def serve() -> None:
            *_, frame = _read_outer(server)
            header = control_session.APP_HEADER.unpack(
                frame[: control_session.APP_HEADER.size]
            )
            captured.extend(frame[control_session.APP_HEADER.size :])
            _write_app(
                server,
                instance_id=session._instance_id,
                sequence=0,
                kind=control_session.APP_EXIT,
                request_id=header[4],
                status=0,
                payload=b"exit",
            )
            server.close()

        worker = threading.Thread(target=serve)
        worker.start()
        session.exec(
            ("/bin/true",),
            timeout_ms=0,
            response_timeout=5,
            environment=environment,
            inherit_default_environment=inherit_default_environment,
        )
        worker.join(timeout=5)
        session.close()
        return bytes(captured)

    def test_exec_distinguishes_omitted_and_empty_environment(self):
        omitted = self._exec_payload(None)
        empty = self._exec_payload(())
        self.assertEqual(struct.unpack("<H", omitted[6:8])[0], 0)
        self.assertEqual(struct.unpack("<H", empty[6:8])[0], 1)
        self.assertEqual(
            struct.unpack("<H", empty[8:10])[0],
            control_session.APP_EXEC_ENVIRONMENT_PRESENT,
        )
        self.assertEqual(struct.unpack("<H", empty[10:12])[0], 0)

    def test_exec_layers_only_a_supplied_environment_over_the_defaults(self):
        layered_flags = (
            control_session.APP_EXEC_ENVIRONMENT_PRESENT
            | control_session.APP_EXEC_INHERIT_DEFAULT_ENV
        )
        layered = self._exec_payload(("A=1",), inherit_default_environment=True)
        self.assertEqual(
            struct.unpack("<HHHI", layered[6:16]),
            (control_session.APP_EXEC_EXTENDED, layered_flags, 1, 0),
        )
        self.assertTrue(layered.endswith(struct.pack("<I", 3) + b"A=1"))
        empty = self._exec_payload((), inherit_default_environment=True)
        self.assertEqual(
            struct.unpack("<HHHI", empty[6:16]),
            (control_session.APP_EXEC_EXTENDED, layered_flags, 0, 0),
        )
        replaced = self._exec_payload(("A=1",))
        self.assertEqual(
            struct.unpack("<H", replaced[8:10])[0],
            control_session.APP_EXEC_ENVIRONMENT_PRESENT,
        )
        # An omitted environment already is the default one, so the request stays the
        # legacy one, which the guest agent accepts without an environment.
        self.assertEqual(
            self._exec_payload(None, inherit_default_environment=True),
            self._exec_payload(None),
        )

    def test_exec_accepts_full_uint32_timeout_range_without_waiting(self):
        for timeout in (0, 3_600_001, 86_400_000, 0xFFFFFFFF):
            with self.subTest(timeout=timeout):
                client, server = socket.socketpair()
                instance = bytes.fromhex("55" * 16)
                session = control_session.ControlSession(
                    control_session._SocketStream(client)
                )
                session._instance_id = instance
                session._epoch = 1

                def serve(
                    server_socket: socket.socket = server,
                    expected_timeout: int = timeout,
                    expected_instance: bytes = instance,
                ) -> None:
                    *_, frame = _read_outer(server_socket)
                    header = control_session.APP_HEADER.unpack(
                        frame[: control_session.APP_HEADER.size]
                    )
                    payload = frame[control_session.APP_HEADER.size :]
                    self.assertEqual(
                        struct.unpack("<I", payload[:4])[0],
                        expected_timeout,
                    )
                    _write_app(
                        server_socket,
                        instance_id=expected_instance,
                        sequence=0,
                        kind=control_session.APP_EXIT,
                        request_id=header[4],
                        status=0,
                        payload=b"exit",
                    )
                    server_socket.close()

                worker = threading.Thread(target=serve)
                worker.start()
                session.exec(
                    ("/bin/true",),
                    timeout_ms=timeout,
                    response_timeout=5,
                )
                worker.join(timeout=5)
                session.close()

    def test_exec_rejects_invalid_timeout_and_execution_fields_before_sending(self):
        client, server = socket.socketpair()
        session = control_session.ControlSession(control_session._SocketStream(client))
        invalid = (-1, 0x100000000, True, 1.5, "1")
        for timeout in invalid:
            with (
                self.subTest(timeout=timeout),
                self.assertRaises((TypeError, ValueError)),
            ):
                session.exec(
                    ("/bin/true",),
                    timeout_ms=timeout,  # type: ignore[arg-type]
                    response_timeout=1,
                )
        for kwargs in (
            {"cwd": ""},
            {"cwd": "relative"},
            {"cwd": "/bad\0path"},
            {"environment": ("MISSING_EQUALS",)},
            {"environment": ("=empty-name",)},
            {"environment": ("DUP=1", "DUP=2")},
        ):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                session.exec(
                    ("/bin/true",),
                    timeout_ms=0,
                    response_timeout=1,
                    **kwargs,  # type: ignore[arg-type]
                )
        for inherit in (1, "true", None):
            with (
                self.subTest(inherit=inherit),
                self.assertRaisesRegex(TypeError, "inheritance must be a boolean"),
            ):
                session.exec(
                    ("/bin/true",),
                    timeout_ms=0,
                    response_timeout=1,
                    environment=("A=1",),
                    inherit_default_environment=inherit,  # type: ignore[arg-type]
                )
        server.setblocking(False)
        with self.assertRaises(BlockingIOError):
            server.recv(1)
        session.close()
        server.close()

    def test_exec_rejects_invalid_response_timeout_before_sending(self):
        client, server = socket.socketpair()
        session = control_session.ControlSession(control_session._SocketStream(client))

        for timeout in (0.0, -1.0, float("nan"), float("inf")):
            with (
                self.subTest(timeout=timeout),
                self.assertRaisesRegex(ValueError, "response timeout"),
            ):
                session.exec(
                    ("/bin/true",),
                    timeout_ms=0,
                    response_timeout=timeout,
                )

        server.setblocking(False)
        with self.assertRaises(BlockingIOError):
            server.recv(1)
        session.close()
        server.close()

    def test_exec_rejects_unknown_exit_category(self):
        client, server = socket.socketpair()
        instance = bytes.fromhex("22" * 16)
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = instance
        session._epoch = 1

        def serve() -> None:
            *_, frame = _read_outer(server)
            request_id = control_session.APP_HEADER.unpack(
                frame[: control_session.APP_HEADER.size]
            )[4]
            _write_app(
                server,
                instance_id=instance,
                sequence=0,
                kind=control_session.APP_EXIT,
                request_id=request_id,
                status=125,
                payload=b"guest-provided-detail",
            )
            server.close()

        worker = threading.Thread(target=serve)
        worker.start()
        with self.assertRaisesRegex(
            control_session.ScriptError, "unsupported exit category"
        ):
            session.exec(("/bin/true",), timeout_ms=0, response_timeout=5)
        worker.join(timeout=5)
        session.close()

    def test_exec_refusal_keeps_status_category_and_diagnostic(self):
        client, server = socket.socketpair()
        instance = bytes.fromhex("33" * 16)
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = instance
        session._epoch = 1
        diagnostic = (
            b"nvx-managed-agent: cannot enter working directory /missing: "
            b"No such file or directory\n"
        )

        def serve() -> None:
            *_, frame = _read_outer(server)
            request_id = control_session.APP_HEADER.unpack(
                frame[: control_session.APP_HEADER.size]
            )[4]
            _write_app(
                server,
                instance_id=instance,
                sequence=0,
                kind=control_session.APP_STDERR,
                request_id=request_id,
                status=0,
                payload=diagnostic,
            )
            _write_app(
                server,
                instance_id=instance,
                sequence=1,
                kind=control_session.APP_ERROR,
                request_id=request_id,
                status=2,
                payload=b"cwd-failed",
            )
            server.close()

        worker = threading.Thread(target=serve)
        worker.start()
        with self.assertRaises(control_session.ManagedExecRefused) as raised:
            session.exec(
                ("/bin/true",), timeout_ms=0, response_timeout=5, cwd="/missing"
            )
        worker.join(timeout=5)
        session.close()

        refusal = raised.exception
        self.assertIsInstance(refusal, control_session.ScriptError)
        self.assertEqual(
            (refusal.status, refusal.category, refusal.stdout, refusal.stderr),
            (2, "cwd-failed", b"", diagnostic),
        )
        self.assertEqual(
            str(refusal),
            "managed guest rejected exec (status=2, category=cwd-failed)",
        )

    def test_session_reset_is_not_a_closed_endpoint(self):
        # A reset comes from an OpenVMM that still serves the endpoint, so it must
        # not look like the closure with which OpenVMM shuts down.
        client, server = socket.socketpair()
        instance = bytes.fromhex("11" * 16)
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = instance
        session._epoch = 1
        server.sendall(
            control_session.OUTER_HEADER.pack(
                b"NVXS", 1, control_session.OUTER_RESET, 0, instance, 1, 0, 0
            )
        )
        try:
            with self.assertRaisesRegex(
                ConnectionError, "^managed control session was reset$"
            ) as raised:
                session.ping(5)
        finally:
            session.close()
            server.close()

        self.assertNotIsInstance(
            raised.exception, control_session.ControlEndpointClosed
        )


class CapabilityPipeTests(unittest.TestCase):
    CAPABILITY = bytes(range(1, 33))

    def test_pipe_holds_the_capability_and_then_end_of_file(self):
        descriptor = control_session.capability_pipe(self.CAPABILITY)
        try:
            self.assertEqual(_read_like_openvmm(descriptor), self.CAPABILITY)
        finally:
            os.close(descriptor)

    def test_openvmm_refuses_a_pipe_whose_writer_is_open(self):
        # A launcher that writes the capability only after it starts OpenVMM
        # leaves the writer open when OpenVMM reads, as before issue #440.
        read, write = os.pipe()
        try:
            os.write(write, self.CAPABILITY)
            self.assertIsNone(_read_like_openvmm(read))
        finally:
            os.close(read)
            os.close(write)

    def test_invalid_capabilities_are_rejected(self):
        for capability in (b"", bytes(range(1, 32)), bytes(32), bytes(range(1, 34))):
            with (
                self.subTest(capability=capability.hex()),
                self.assertRaisesRegex(ValueError, "32 nonzero bytes"),
            ):
                control_session.capability_pipe(capability)


class SocketStreamTests(unittest.TestCase):
    def test_read_reports_a_closed_endpoint(self):
        client, server = socket.socketpair()
        stream = control_session._SocketStream(client)
        server.close()
        try:
            with self.assertRaisesRegex(
                control_session.ControlEndpointClosed,
                "^managed control endpoint closed$",
            ):
                stream.read_exact(1, time.monotonic() + 5)
        finally:
            stream.close()

    def test_reset_and_broken_pipe_report_a_closed_endpoint(self):
        # OpenVMM may close the socket before it reads everything that the client
        # sent, which resets the client's reads, and a later write fails with a
        # broken pipe.
        if sys.platform != "linux":
            self.skipTest("Unix-domain socket closure semantics require Linux")
        client, server = socket.socketpair()
        stream = control_session._SocketStream(client)
        stream.write_all(b"unread request")
        server.close()
        try:
            with self.assertRaises(control_session.ControlEndpointClosed) as raised:
                stream.read_exact(1, time.monotonic() + 5)
            self.assertIsInstance(raised.exception.__cause__, ConnectionResetError)
            with self.assertRaises(control_session.ControlEndpointClosed) as raised:
                stream.write_all(b"record")
            self.assertIsInstance(raised.exception.__cause__, BrokenPipeError)
        finally:
            stream.close()


class NamedPipeStreamTests(unittest.TestCase):
    def test_write_reports_a_closed_endpoint(self):
        # OpenVMM closes the pipe as it tears the VM down, and the CRT reports a
        # later write to it as EINVAL rather than as a broken pipe.
        if sys.platform != "win32":
            self.skipTest("named pipes require a Windows host")
        import _winapi

        name = rf"\\.\pipe\nvx-control-session-test-{uuid.uuid4().hex}"
        server = _winapi.CreateNamedPipe(
            name,
            _winapi.PIPE_ACCESS_DUPLEX,
            _winapi.PIPE_WAIT,
            1,
            65536,
            65536,
            0,
            _winapi.NULL,
        )
        try:
            stream = control_session._NamedPipeStream.connect(Path(name), 5)
        finally:
            _winapi.CloseHandle(server)
        try:
            with self.assertRaisesRegex(
                control_session.ControlEndpointClosed,
                "^managed control endpoint closed$",
            ):
                stream.write_all(b"record")
        finally:
            stream.close()


def _host_control_frame(value: object) -> bytes:
    payload = value if isinstance(value, bytes) else json.dumps(value).encode()
    return host_control.FRAME_HEADER.pack(len(payload)) + payload


class HostControlTests(unittest.TestCase):
    CAPABILITY = bytes(range(1, 33))

    def _exchange(
        self,
        response: bytes,
        operation: Callable[[host_control.HostControl], object],
    ) -> tuple[object, dict[str, object]]:
        """Runs `operation` against a peer that answers one request with
        `response`, and returns its result and the request it received."""
        client, server = socket.socketpair()
        received: list[dict[str, object]] = []

        def serve() -> None:
            with server:
                if _read_exact(server, 32) != self.CAPABILITY:
                    raise RuntimeError("host-control client sent a wrong capability")
                (length,) = host_control.FRAME_HEADER.unpack(_read_exact(server, 4))
                received.append(json.loads(_read_exact(server, length)))
                server.sendall(response)

        thread = threading.Thread(target=serve)
        thread.start()
        try:
            with (
                patch.object(
                    host_control,
                    "connect_local_stream",
                    return_value=control_session._SocketStream(client),
                ),
                host_control.HostControl(
                    Path("endpoint"), self.CAPABILITY, 5
                ) as control,
            ):
                result = operation(control)
        finally:
            thread.join(5)
        return result, received[0]

    def test_query_sends_a_versioned_request_and_returns_the_slots(self):
        slots = [{"slot": 0, "state": "empty"}]
        result, request = self._exchange(
            _host_control_frame(
                {
                    "version": 1,
                    "request_id": 1,
                    "status": "ok",
                    "result": {"slots": slots},
                }
            ),
            lambda control: control.query_image_slots(),
        )
        self.assertEqual(result, slots)
        self.assertEqual(
            request, {"version": 1, "request_id": 1, "operation": "query_image_slots"}
        )

    def test_bind_sends_the_path_and_identity_and_reports_error_codes(self):
        bound = {
            "version": 1,
            "request_id": 1,
            "status": "ok",
            "result": {"slot": 2, "capacity_sectors": 8},
        }
        _, request = self._exchange(
            _host_control_frame(bound),
            lambda control: control.bind_image_slot(2, Path("image.raw"), "id"),
        )
        self.assertEqual(
            request,
            {
                "version": 1,
                "request_id": 1,
                "operation": "bind_image_slot",
                "slot": 2,
                "path": str(Path("image.raw")),
                "identity": "id",
            },
        )
        response = {
            "version": 1,
            "request_id": 1,
            "status": "error",
            "code": "already_bound",
            "message": "image slot 2 is already bound to a different identity",
        }
        with self.assertRaisesRegex(ScriptError, r"\(already_bound\): image slot 2"):
            self._exchange(
                _host_control_frame(response),
                lambda control: control.bind_image_slot(2, Path("image.raw"), "id"),
            )

    def test_invalid_responses_are_rejected(self):
        ok = {"version": 1, "request_id": 1, "status": "ok"}
        for response, message in (
            (
                host_control.FRAME_HEADER.pack(host_control.MAX_FRAME_SIZE + 1),
                "exceeds the protocol limit",
            ),
            (_host_control_frame(b"{not json"), "invalid JSON"),
            (_host_control_frame([1]), "non-object response"),
            (_host_control_frame({**ok, "request_id": 2, "result": {}}), "mismatched"),
            (_host_control_frame({**ok, "version": 2, "result": {}}), "mismatched"),
            (_host_control_frame({**ok, "result": []}), "invalid success response"),
            (_host_control_frame({**ok, "result": {"slots": {}}}), "image-slot table"),
            (_host_control_frame({**ok, "result": {"slots": [1]}}), "image-slot table"),
        ):
            with self.subTest(message=message, response=response[:40]):
                with self.assertRaisesRegex(ScriptError, message):
                    self._exchange(
                        response, lambda control: control.query_image_slots()
                    )

    def test_oversized_requests_and_invalid_capabilities_fail_before_sending(self):
        with self.assertRaisesRegex(ValueError, "32 nonzero bytes"):
            host_control.HostControl(Path("endpoint"), bytes(32), 5)
        client, server = socket.socketpair()
        with (
            server,
            patch.object(
                host_control,
                "connect_local_stream",
                return_value=control_session._SocketStream(client),
            ),
            host_control.HostControl(Path("endpoint"), self.CAPABILITY, 5) as control,
        ):
            with self.assertRaisesRegex(ValueError, "exceeds the protocol limit"):
                control.bind_image_slot(
                    0, Path("x" * host_control.MAX_FRAME_SIZE), "identity"
                )
            server.settimeout(5)
            # Only the capability reached the peer.
            self.assertEqual(_read_exact(server, 32), self.CAPABILITY)
            server.setblocking(False)
            with self.assertRaises(BlockingIOError):
                server.recv(1)


if __name__ == "__main__":
    unittest.main()

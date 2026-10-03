#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import socket
import struct
import sys
import threading
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import control_session  # noqa: E402


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

    def test_exec_distinguishes_omitted_and_empty_environment(self):
        def payload_for(environment: tuple[str, ...] | None) -> bytes:
            client, server = socket.socketpair()
            session = control_session.ControlSession(
                control_session._SocketStream(client)
            )
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
            )
            worker.join(timeout=5)
            session.close()
            return bytes(captured)

        omitted = payload_for(None)
        empty = payload_for(())
        self.assertEqual(struct.unpack("<H", omitted[6:8])[0], 0)
        self.assertEqual(struct.unpack("<H", empty[6:8])[0], 1)
        self.assertEqual(
            struct.unpack("<H", empty[8:10])[0],
            control_session.APP_EXEC_ENVIRONMENT_PRESENT,
        )
        self.assertEqual(struct.unpack("<H", empty[10:12])[0], 0)

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


if __name__ == "__main__":
    unittest.main()

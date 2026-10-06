#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import socket
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import control_session, state_control  # noqa: E402
from nvx_tools.common import ScriptError  # noqa: E402

INSTANCE = bytes(range(16))
CAPABILITY = bytes([0x5A]) * 32


def _response(
    operation: int,
    sequence: int,
    *,
    status: int = 0,
    state: int = 1,
    transitions: int = 0,
    detail: bytes = b"",
    magic: bytes = b"NVXV",
    version: int = 1,
    kind: int | None = None,
    flags: int = 0,
    reserved: bytes = bytes(7),
    record_bytes: int = 32,
) -> bytes:
    record = state_control.STATE_RECORD.pack(state, reserved, transitions, INSTANCE)
    payload = record[:record_bytes] + detail
    return (
        state_control.HEADER.pack(
            magic,
            version,
            operation | 0x80 if kind is None else kind,
            flags,
            sequence,
            status,
            len(payload),
        )
        + payload
    )


def _read_exact(connection: socket.socket, length: int) -> bytes:
    output = bytearray()
    while len(output) != length:
        chunk = connection.recv(length - len(output))
        if not chunk:
            raise RuntimeError("test state-control connection closed")
        output.extend(chunk)
    return bytes(output)


class StateControlSessionTests(unittest.TestCase):
    def _session(self) -> tuple[state_control.StateControlSession, socket.socket]:
        host, server = socket.socketpair()
        server.settimeout(5)
        session = state_control.StateControlSession(control_session._SocketStream(host))
        self.addCleanup(session.close)
        self.addCleanup(server.close)
        return session, server

    def test_requests_have_the_documented_layout(self) -> None:
        self.assertEqual(state_control.HEADER.size, 24)
        self.assertEqual(state_control.STATE_RECORD.size, 32)
        session, server = self._session()
        server.sendall(_response(state_control.AUTHENTICATE, 1))
        session.request(state_control.AUTHENTICATE, 5, CAPABILITY)
        header = _read_exact(server, state_control.HEADER.size)
        self.assertEqual(
            header,
            b"NVXV\x01\x00\x01\x00"
            + (1).to_bytes(8, "little")
            + bytes(4)
            + (32).to_bytes(4, "little"),
        )
        self.assertEqual(_read_exact(server, 32), CAPABILITY)

        server.sendall(_response(state_control.PAUSE, 2))
        session.request(state_control.PAUSE, 5)
        self.assertEqual(
            _read_exact(server, state_control.HEADER.size),
            b"NVXV\x01\x00\x03\x00" + (2).to_bytes(8, "little") + bytes(8),
        )

    def test_responses_decode_status_state_transitions_and_detail(self) -> None:
        session, server = self._session()
        server.sendall(
            _response(
                state_control.RESUME,
                1,
                status=2,
                state=2,
                transitions=3,
                detail="restore failed \N{LATIN SMALL LETTER E WITH ACUTE}".encode(),
            )
        )
        self.assertEqual(
            session.request(state_control.RESUME, 5),
            state_control.StateResponse(
                "rejected",
                "paused",
                3,
                INSTANCE,
                "restore failed \N{LATIN SMALL LETTER E WITH ACUTE}",
            ),
        )
        for sequence, (status, state, expected) in enumerate(
            (
                (0, 0, ("ok", "unknown")),
                (1, 4, ("busy", "busy")),
                (3, 3, ("failed", "stopped")),
            ),
            start=2,
        ):
            with self.subTest(status=status, state=state):
                server.sendall(
                    _response(state_control.QUERY, sequence, status=status, state=state)
                )
                response = session.request(state_control.QUERY, 5)
                self.assertEqual((response.status, response.state), expected)

    def test_malformed_responses_are_rejected(self) -> None:
        cases = {
            "wrong magic": _response(state_control.PAUSE, 1, magic=b"NVXW"),
            "wrong version": _response(state_control.PAUSE, 1, version=2),
            "nonzero flags": _response(state_control.PAUSE, 1, flags=1),
            "short record": _response(state_control.PAUSE, 1, record_bytes=31),
            "wrong operation": _response(
                state_control.PAUSE, 1, kind=state_control.QUERY | 0x80
            ),
            "wrong sequence": _response(state_control.PAUSE, 7),
            "unknown status": _response(state_control.PAUSE, 1, status=4),
            "unknown state": _response(state_control.PAUSE, 1, state=5),
            "reserved bytes": _response(
                state_control.PAUSE, 1, reserved=b"\x01" + bytes(6)
            ),
            "invalid detail": _response(state_control.PAUSE, 1, detail=b"\xff"),
            "oversized detail": state_control.HEADER.pack(
                b"NVXV", 1, state_control.PAUSE | 0x80, 0, 1, 0, 545
            )
            + bytes(545),
        }
        for name, response in cases.items():
            with self.subTest(name):
                session, server = self._session()
                server.sendall(response)
                with self.assertRaises(ScriptError):
                    session.request(state_control.PAUSE, 5)

    def test_closed_without_response_distinguishes_close_reply_and_silence(
        self,
    ) -> None:
        session, server = self._session()
        server.sendall(b"N")
        self.assertFalse(session.closed_without_response(5))
        started = time.monotonic()
        self.assertFalse(session.closed_without_response(0.05))
        self.assertLess(time.monotonic() - started, 5)
        server.close()
        self.assertTrue(session.closed_without_response(5))

    def test_authentication_requires_a_nonzero_capability(self) -> None:
        for capability in (bytes(32), CAPABILITY[:31]):
            with (
                self.subTest(length=len(capability)),
                self.assertRaisesRegex(ValueError, "32 nonzero bytes"),
            ):
                state_control.StateControlSession.authenticate(
                    Path("unused"), capability, 1
                )


if __name__ == "__main__":
    unittest.main()

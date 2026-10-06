"""Client for OpenVMM's authenticated microVM state-control endpoint.

``--microvm-state-control listen=<endpoint>`` serves host pause, resume, and
run-state requests for a microVM that has a live authenticated control
console. The host authenticates with the control-console capability. Protocol
version 1 is documented in OpenVMM's Guide, in
``reference/openvmm/management/state_control_protocol.md``.
"""

from __future__ import annotations

import struct
import time
from dataclasses import dataclass
from pathlib import Path

from .common import ScriptError
from .control_session import EndpointStream, connect_endpoint

HEADER = struct.Struct("<4sHBBQII")
STATE_RECORD = struct.Struct("<B7sQ16s")
MAGIC = b"NVXV"
VERSION = 1
CAPABILITY_BYTES = 32
MAX_DETAIL_BYTES = 512
RESPONSE_BIT = 0x80

AUTHENTICATE = 1
QUERY = 2
PAUSE = 3
RESUME = 4

STATUSES = ("ok", "busy", "rejected", "failed")
STATES = ("unknown", "running", "paused", "stopped", "busy")


@dataclass(frozen=True)
class StateResponse:
    """A state-control response: the status and the resulting run state."""

    status: str
    state: str
    transitions: int
    instance_id: bytes
    detail: str


class StateControlSession:
    """One host connection to the state-control endpoint."""

    def __init__(self, stream: EndpointStream) -> None:
        self._stream = stream
        self._sequence = 0

    @classmethod
    def connect(cls, endpoint: Path, timeout: float) -> StateControlSession:
        """Connects without authenticating."""
        return cls(connect_endpoint(endpoint, timeout))

    @classmethod
    def authenticate(
        cls, endpoint: Path, capability: bytes, timeout: float
    ) -> tuple[StateControlSession, StateResponse]:
        """Connects, authenticates, and returns the session and run state."""
        if len(capability) != CAPABILITY_BYTES or capability == bytes(CAPABILITY_BYTES):
            raise ValueError("control capability must be 32 nonzero bytes")
        session = cls.connect(endpoint, timeout)
        try:
            response = session.request(AUTHENTICATE, timeout, capability)
            if response.status != "ok":
                raise ScriptError(f"state-control authentication failed: {response}")
            return session, response
        except BaseException:
            session.close()
            raise

    def send(self, operation: int, payload: bytes = b"") -> None:
        """Sends one request without waiting for its response."""
        self._sequence += 1
        self._stream.write_all(
            HEADER.pack(MAGIC, VERSION, operation, 0, self._sequence, 0, len(payload))
            + payload
        )

    def request(
        self, operation: int, timeout: float, payload: bytes = b""
    ) -> StateResponse:
        """Sends one request and returns its validated response."""
        self.send(operation, payload)
        deadline = time.monotonic() + timeout
        header: tuple[bytes, int, int, int, int, int, int] = HEADER.unpack(
            self._stream.read_exact(HEADER.size, deadline)
        )
        magic, version, kind, flags, sequence, status, length = header
        if (
            magic != MAGIC
            or version != VERSION
            or kind != operation | RESPONSE_BIT
            or flags != 0
            or sequence != self._sequence
            or status >= len(STATUSES)
            or not STATE_RECORD.size <= length <= STATE_RECORD.size + MAX_DETAIL_BYTES
        ):
            raise ScriptError(
                "state-control endpoint returned an invalid response header: "
                f"kind={kind:#x} flags={flags} sequence={sequence} "
                f"status={status} length={length}"
            )
        payload = self._stream.read_exact(length, deadline)
        record: tuple[int, bytes, int, bytes] = STATE_RECORD.unpack_from(payload)
        state, reserved, transitions, instance_id = record
        if state >= len(STATES) or reserved != bytes(7):
            raise ScriptError("state-control endpoint returned an invalid state record")
        try:
            detail = payload[STATE_RECORD.size :].decode("utf-8")
        except UnicodeDecodeError as error:
            raise ScriptError("state-control detail is not UTF-8") from error
        return StateResponse(
            STATUSES[status], STATES[state], transitions, instance_id, detail
        )

    def closed_without_response(self, timeout: float) -> bool:
        """Returns whether the endpoint closes the connection before sending
        anything."""
        try:
            self._stream.read_exact(1, time.monotonic() + timeout)
        except TimeoutError:
            return False
        except ConnectionError:
            return True
        return False

    def close(self) -> None:
        self._stream.close()

    def __enter__(self) -> StateControlSession:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

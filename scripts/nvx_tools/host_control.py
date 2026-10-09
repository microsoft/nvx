"""Client for OpenVMM's authenticated local host-control protocol."""

from __future__ import annotations

import json
import struct
import time
from pathlib import Path
from typing import Any, cast

from .common import ScriptError
from .control_session import connect_local_stream

FRAME_HEADER = struct.Struct("<I")
MAX_FRAME_SIZE = 64 * 1024


class HostControl:
    def __init__(self, endpoint: Path, capability: bytes, timeout: float) -> None:
        if len(capability) != 32 or capability == bytes(32):
            raise ValueError("control capability must be 32 nonzero bytes")
        self._stream = connect_local_stream(endpoint, timeout)
        try:
            self._stream.write_all(capability)
        except BaseException:
            self._stream.close()
            raise
        self._timeout = timeout
        self._next_request_id = 1

    def __enter__(self) -> HostControl:
        return self

    def __exit__(self, *_: object) -> None:
        self._stream.close()

    @classmethod
    def connect(cls, endpoint: Path, capability: bytes, timeout: float) -> HostControl:
        return cls(endpoint, capability, timeout)

    def request(self, operation: str, **arguments: object) -> dict[str, Any]:
        request_id = self._next_request_id
        self._next_request_id += 1
        request = {
            "version": 1,
            "request_id": request_id,
            "operation": operation,
            **arguments,
        }
        payload = json.dumps(request, separators=(",", ":")).encode("utf-8")
        if len(payload) > MAX_FRAME_SIZE:
            raise ValueError("host-control request exceeds the protocol limit")
        self._stream.write_all(FRAME_HEADER.pack(len(payload)) + payload)
        deadline = time.monotonic() + self._timeout
        (length,) = FRAME_HEADER.unpack(
            self._stream.read_exact(FRAME_HEADER.size, deadline)
        )
        if length > MAX_FRAME_SIZE:
            raise ScriptError("host-control response exceeds the protocol limit")
        raw = self._stream.read_exact(length, deadline)
        try:
            value: object = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ScriptError("host-control returned invalid JSON") from error
        if not isinstance(value, dict):
            raise ScriptError("host-control returned a non-object response")
        response = cast(dict[str, Any], value)
        if response.get("version") != 1 or response.get("request_id") != request_id:
            raise ScriptError("host-control returned a mismatched response")
        if response.get("status") != "ok":
            code = response.get("code", "unknown")
            message = response.get("message", "host-control operation failed")
            raise ScriptError(f"host-control {operation} failed ({code}): {message}")
        result = response.get("result")
        if not isinstance(result, dict):
            raise ScriptError("host-control returned an invalid success response")
        return cast(dict[str, Any], result)

    def query_image_slots(self) -> list[dict[str, Any]]:
        response = self.request("query_image_slots")
        slots = response.get("slots")
        if not isinstance(slots, list):
            raise ScriptError("host-control returned an invalid image-slot table")
        typed_slots = cast(list[object], slots)
        result: list[dict[str, Any]] = []
        for slot in typed_slots:
            if not isinstance(slot, dict):
                raise ScriptError("host-control returned an invalid image-slot table")
            result.append(cast(dict[str, Any], slot))
        return result

    def bind_image_slot(self, slot: int, path: Path, identity: str) -> None:
        self.request(
            "bind_image_slot",
            slot=slot,
            path=str(path),
            identity=identity,
        )

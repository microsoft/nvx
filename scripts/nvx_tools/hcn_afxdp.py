"""Externally managed HCN vNIC and AF_XDP workflows."""

from __future__ import annotations

import ctypes
import http.server
import json
import os
import re
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Mapping, Sequence

from .common import (
    CommandResult,
    ScriptError,
    remove_tree,
    require_file,
    require_success,
    run_capture,
)
from .vm import METRIC_PATTERN, format_median
from .windows_hcn import normalize_mac


DEFAULT_GUEST_ADDRESS = "192.168.240.2"
DEFAULT_GATEWAY = "192.168.240.1"
DEFAULT_CMDLINE = "earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1"


@dataclass(frozen=True)
class AfxdpConfig:
    microvm: Path
    kernel: Path
    initrd: Path
    endpoint_config: Path
    guest_address: str = DEFAULT_GUEST_ADDRESS
    prefix_length: int = 24
    gateway: str = DEFAULT_GATEWAY
    mtu: int = 1500
    timeout: int = 300


@dataclass(frozen=True)
class AfxdpVmResult:
    metric: float | None
    wall_ms: float
    stdout: str
    stderr: str
    ready: Mapping[str, Any]

    @property
    def text(self) -> str:
        return f"{self.stdout}\n{self.stderr}"


class _Overlapped(ctypes.Structure):
    _fields_ = [
        ("internal", ctypes.c_size_t),
        ("internal_high", ctypes.c_size_t),
        ("offset", ctypes.c_uint32),
        ("offset_high", ctypes.c_uint32),
        ("event", ctypes.c_void_p),
    ]


class WindowsNamedPipeServer:
    """Raw duplex named pipe compatible with the Rust control-pipe protocol."""

    ERROR_IO_PENDING = 997
    ERROR_OPERATION_ABORTED = 995
    ERROR_NOT_FOUND = 1168
    ERROR_PIPE_CONNECTED = 535
    WAIT_OBJECT_0 = 0
    WAIT_TIMEOUT = 258
    WAIT_FAILED = 0xFFFFFFFF
    INFINITE = 0xFFFFFFFF
    FILE_FLAG_OVERLAPPED = 0x40000000
    PIPE_ACCESS_DUPLEX = 0x00000003
    PIPE_TYPE_BYTE = 0x00000000
    PIPE_READMODE_BYTE = 0x00000000
    PIPE_WAIT = 0x00000000

    def __init__(self, path: str) -> None:
        if os.name != "nt":
            raise ScriptError("AF_XDP control pipes require Windows")
        self.kernel32 = ctypes.WinDLL("kernel32.dll", use_last_error=True)
        self._bind()
        self.handle = self.kernel32.CreateNamedPipeW(
            path,
            self.PIPE_ACCESS_DUPLEX | self.FILE_FLAG_OVERLAPPED,
            self.PIPE_TYPE_BYTE | self.PIPE_READMODE_BYTE | self.PIPE_WAIT,
            1,
            4096,
            4096,
            0,
            None,
        )
        if self.handle == ctypes.c_void_p(-1).value:
            raise ctypes.WinError(ctypes.get_last_error())

    def _bind(self) -> None:
        self.kernel32.CreateNamedPipeW.argtypes = [
            ctypes.c_wchar_p,
            ctypes.c_uint32,
            ctypes.c_uint32,
            ctypes.c_uint32,
            ctypes.c_uint32,
            ctypes.c_uint32,
            ctypes.c_uint32,
            ctypes.c_void_p,
        ]
        self.kernel32.CreateNamedPipeW.restype = ctypes.c_void_p
        self.kernel32.CreateEventW.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_wchar_p,
        ]
        self.kernel32.CreateEventW.restype = ctypes.c_void_p
        self.kernel32.ConnectNamedPipe.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(_Overlapped),
        ]
        self.kernel32.ConnectNamedPipe.restype = ctypes.c_int
        self.kernel32.ReadFile.argtypes = [
            ctypes.c_void_p,
            ctypes.c_void_p,
            ctypes.c_uint32,
            ctypes.POINTER(ctypes.c_uint32),
            ctypes.POINTER(_Overlapped),
        ]
        self.kernel32.ReadFile.restype = ctypes.c_int
        self.kernel32.WriteFile.argtypes = self.kernel32.ReadFile.argtypes
        self.kernel32.WriteFile.restype = ctypes.c_int
        self.kernel32.GetOverlappedResult.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(_Overlapped),
            ctypes.POINTER(ctypes.c_uint32),
            ctypes.c_int,
        ]
        self.kernel32.GetOverlappedResult.restype = ctypes.c_int
        self.kernel32.WaitForSingleObject.argtypes = [
            ctypes.c_void_p,
            ctypes.c_uint32,
        ]
        self.kernel32.WaitForSingleObject.restype = ctypes.c_uint32
        self.kernel32.CancelIoEx.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(_Overlapped),
        ]
        self.kernel32.DisconnectNamedPipe.argtypes = [ctypes.c_void_p]
        self.kernel32.CloseHandle.argtypes = [ctypes.c_void_p]

    @staticmethod
    def _remaining_ms(deadline: float) -> int:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError
        return min(0xFFFFFFFE, max(1, int(remaining * 1000)))

    def _overlapped(self) -> tuple[_Overlapped, int]:
        event = self.kernel32.CreateEventW(None, True, False, None)
        if not event:
            raise ctypes.WinError(ctypes.get_last_error())
        operation = _Overlapped()
        operation.event = event
        return operation, event

    def _cancel_and_drain(self, operation: _Overlapped, event: int) -> None:
        if not self.kernel32.CancelIoEx(self.handle, ctypes.byref(operation)):
            error = ctypes.get_last_error()
            if error != self.ERROR_NOT_FOUND:
                raise ctypes.WinError(error)
        wait = self.kernel32.WaitForSingleObject(event, self.INFINITE)
        if wait != self.WAIT_OBJECT_0:
            raise ctypes.WinError(ctypes.get_last_error())
        transferred = ctypes.c_uint32()
        if not self.kernel32.GetOverlappedResult(
            self.handle,
            ctypes.byref(operation),
            ctypes.byref(transferred),
            False,
        ):
            error = ctypes.get_last_error()
            if error != self.ERROR_OPERATION_ABORTED:
                raise ctypes.WinError(error)

    def _finish(self, operation: _Overlapped, event: int, deadline: float) -> int:
        transferred = ctypes.c_uint32()
        try:
            try:
                remaining = self._remaining_ms(deadline)
            except TimeoutError:
                self._cancel_and_drain(operation, event)
                raise
            wait = self.kernel32.WaitForSingleObject(event, remaining)
            if wait == self.WAIT_TIMEOUT:
                self._cancel_and_drain(operation, event)
                raise TimeoutError
            if wait == self.WAIT_FAILED:
                error = ctypes.get_last_error()
                self._cancel_and_drain(operation, event)
                raise ctypes.WinError(error)
            if wait != self.WAIT_OBJECT_0:
                self._cancel_and_drain(operation, event)
                raise ctypes.WinError(ctypes.get_last_error())
            if not self.kernel32.GetOverlappedResult(
                self.handle,
                ctypes.byref(operation),
                ctypes.byref(transferred),
                False,
            ):
                raise ctypes.WinError(ctypes.get_last_error())
            return int(transferred.value)
        finally:
            self.kernel32.CloseHandle(event)

    def connect(self, deadline: float) -> None:
        operation, event = self._overlapped()
        connected = self.kernel32.ConnectNamedPipe(
            self.handle, ctypes.byref(operation)
        )
        if connected:
            self.kernel32.CloseHandle(event)
            return
        error = ctypes.get_last_error()
        if error == self.ERROR_PIPE_CONNECTED:
            self.kernel32.CloseHandle(event)
            return
        if error != self.ERROR_IO_PENDING:
            self.kernel32.CloseHandle(event)
            raise ctypes.WinError(error)
        self._finish(operation, event, deadline)

    def _read_byte(self, deadline: float) -> bytes:
        buffer = ctypes.create_string_buffer(1)
        transferred = ctypes.c_uint32()
        operation, event = self._overlapped()
        complete = self.kernel32.ReadFile(
            self.handle,
            buffer,
            1,
            ctypes.byref(transferred),
            ctypes.byref(operation),
        )
        if complete:
            self.kernel32.CloseHandle(event)
            count = int(transferred.value)
        else:
            error = ctypes.get_last_error()
            if error != self.ERROR_IO_PENDING:
                self.kernel32.CloseHandle(event)
                raise ctypes.WinError(error)
            count = self._finish(operation, event, deadline)
        return buffer.raw[:count]

    def read_line(self, deadline: float, limit: int = 16 * 1024) -> str:
        data = bytearray()
        while len(data) <= limit:
            value = self._read_byte(deadline)
            if not value:
                raise ScriptError("control pipe closed before AF_XDP readiness")
            if value == b"\n":
                return data.decode("utf-8")
            data.extend(value)
        raise ScriptError(f"control-pipe message exceeds {limit} bytes")

    def write_line(self, document: Mapping[str, Any], deadline: float) -> None:
        data = json.dumps(document, separators=(",", ":")).encode("utf-8") + b"\n"
        buffer = ctypes.create_string_buffer(data)
        transferred = ctypes.c_uint32()
        operation, event = self._overlapped()
        complete = self.kernel32.WriteFile(
            self.handle,
            buffer,
            len(data),
            ctypes.byref(transferred),
            ctypes.byref(operation),
        )
        if complete:
            self.kernel32.CloseHandle(event)
            count = int(transferred.value)
        else:
            error = ctypes.get_last_error()
            if error != self.ERROR_IO_PENDING:
                self.kernel32.CloseHandle(event)
                raise ctypes.WinError(error)
            count = self._finish(operation, event, deadline)
        if count != len(data):
            raise ScriptError(f"short control-pipe write: {count}/{len(data)} bytes")

    def close(self) -> None:
        if self.handle:
            self.kernel32.DisconnectNamedPipe(self.handle)
            self.kernel32.CloseHandle(self.handle)
            self.handle = None


def load_hcn_endpoint(config: AfxdpConfig) -> dict[str, Any]:
    require_file(
        config.endpoint_config,
        f"missing HCN endpoint descriptor: {config.endpoint_config}",
    )
    try:
        document = json.loads(config.endpoint_config.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ScriptError(f"invalid HCN endpoint descriptor: {error}") from error
    if not isinstance(document, dict) or document.get("version") != 1:
        raise ScriptError("HCN endpoint descriptor must use version 1")
    try:
        interface_index = int(document["interfaceIndex"])
        interface_luid = int(document["interfaceLuid"])
        gateway_mac = normalize_mac(str(document["gatewayMac"]))
        normalize_mac(str(document["macAddress"]))
    except (KeyError, TypeError, ValueError) as error:
        raise ScriptError(f"incomplete HCN endpoint descriptor: {error}") from error
    if not document.get("hostAttached") or interface_index == 0 or interface_luid == 0:
        raise ScriptError(
            "HCN endpoint descriptor does not contain a host-attached vNIC: "
            f"{config.endpoint_config}"
        )
    if not gateway_mac:
        raise ScriptError("HCN endpoint descriptor has no gateway MAC")
    if (
        document.get("guestAddress") != config.guest_address
        or int(document.get("prefixLength", -1)) != config.prefix_length
        or document.get("gateway") != config.gateway
    ):
        raise ScriptError(
            "HCN endpoint descriptor addressing does not match the AF_XDP parameters"
        )
    return document


def build_manifest(
    endpoint: Mapping[str, Any],
    config: AfxdpConfig,
    control_pipe: str,
) -> dict[str, Any]:
    return {
        "version": 2,
        "attachment": {
            "backend": "hcn-afxdp-l2bridge",
            "interfaceIndex": int(endpoint["interfaceIndex"]),
            "interfaceLuid": int(endpoint["interfaceLuid"]),
            "gatewayMac": str(endpoint["gatewayMac"]),
            "queueSelection": {"mode": "auto"},
        },
        "device": {
            "macAddress": str(endpoint["macAddress"]),
            "mtu": config.mtu,
        },
        "guestBootstrap": {
            "ipv4": {
                "address": config.guest_address,
                "prefixLength": config.prefix_length,
                "gateway": config.gateway,
            },
            "routes": [
                {"destination": "0.0.0.0/0", "nextHop": config.gateway}
            ],
            "dns": {"servers": ["1.1.1.1"], "search": []},
        },
        "runtime": {"controlPipe": control_pipe},
    }


def validate_ready(document: Mapping[str, Any]) -> None:
    if document.get("type") == "DataPlaneError":
        raise ScriptError(
            f"NVX reported data-plane failure: {document.get('message', '<none>')}"
        )
    queues = document.get("queues")
    if (
        document.get("type") != "DataPlaneReady"
        or int(document.get("interfaceLuid", 0)) == 0
        or not isinstance(queues, list)
        or 0 not in queues
    ):
        raise ScriptError(f"invalid DataPlaneReady message: {document}")


def run_afxdp_vm(
    config: AfxdpConfig,
    endpoint: Mapping[str, Any],
    arguments: Sequence[str | Path],
    *,
    required_marker: str = "",
    require_metric: bool = False,
) -> AfxdpVmResult:
    pipe_name = f"nvx-hcn-afxdp-{uuid.uuid4().hex}"
    pipe_path = rf"\\.\pipe\{pipe_name}"
    manifest_path = Path(tempfile.gettempdir()) / f"{pipe_name}.json"
    manifest_path.write_text(
        json.dumps(build_manifest(endpoint, config, pipe_path), separators=(",", ":")),
        encoding="utf-8",
        newline="\n",
    )
    pipe: WindowsNamedPipeServer | None = None
    process: subprocess.Popen[bytes] | None = None
    stdout = b""
    stderr = b""
    ready: dict[str, Any] = {}
    failure: Exception | None = None
    started_at = 0.0
    ended_at = 0.0
    try:
        pipe = WindowsNamedPipeServer(pipe_path)
        command = [os.fspath(config.microvm), *map(os.fspath, arguments), "--net-config", os.fspath(manifest_path)]
        started_at = time.perf_counter()
        process = subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        deadline = time.monotonic() + config.timeout
        with ThreadPoolExecutor(max_workers=2) as pool:
            stdout_task = pool.submit(process.stdout.read)  # type: ignore[union-attr]
            stderr_task = pool.submit(process.stderr.read)  # type: ignore[union-attr]
            try:
                pipe.connect(deadline)
                line = pipe.read_line(deadline)
                parsed = json.loads(line)
                if not isinstance(parsed, dict):
                    raise ScriptError(f"invalid DataPlaneReady message: {line}")
                ready = parsed
                validate_ready(ready)
                pipe.write_line({"type": "StartVm"}, deadline)
                process.wait(timeout=max(0.001, deadline - time.monotonic()))
                ended_at = time.perf_counter()
            except (TimeoutError, subprocess.TimeoutExpired) as error:
                failure = ScriptError(
                    f"AF_XDP VM run exceeded {config.timeout}s"
                )
                if process.poll() is None:
                    process.kill()
            except Exception as error:
                failure = error
                if process.poll() is None:
                    process.kill()
            finally:
                process.wait()
                if ended_at == 0:
                    ended_at = time.perf_counter()
                stdout = stdout_task.result()
                stderr = stderr_task.result()
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
        if pipe is not None:
            pipe.close()
        manifest_path.unlink(missing_ok=True)

    stdout_text = stdout.decode("utf-8", errors="replace")
    stderr_text = stderr.decode("utf-8", errors="replace")
    text = f"{stdout_text}\n{stderr_text}"
    if failure is not None:
        raise ScriptError(f"{failure}\n{text}") from failure
    if process is None or process.returncode != 0:
        status = process.returncode if process is not None else "unknown"
        raise ScriptError(f"microvm.exe exited with code {status}\n{text}")
    if required_marker and required_marker not in text:
        raise ScriptError(f"AF_XDP VM run did not emit {required_marker!r}\n{text}")
    metric: float | None = None
    if require_metric:
        match = METRIC_PATTERN.search(text)
        if match is None:
            raise ScriptError(f"AF_XDP VM run did not report a timing marker\n{text}")
        metric = float(match.group(1))
    return AfxdpVmResult(
        metric,
        (ended_at - started_at) * 1000,
        stdout_text,
        stderr_text,
        ready,
    )


class _HelloHandler(http.server.BaseHTTPRequestHandler):
    logs: list[str]

    def do_GET(self) -> None:
        body = b"HELLO-HOST"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:
        self.logs.append(format % args)


class HostHttpServer:
    def __init__(self, port: int) -> None:
        self.logs: list[str] = []
        handler = type("HelloHandler", (_HelloHandler,), {"logs": self.logs})
        self.server = http.server.ThreadingHTTPServer(("0.0.0.0", port), handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self) -> "HostHttpServer":
        self.thread.start()
        return self

    def __exit__(self, *args: object) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


class _ServiceStatusProcess(ctypes.Structure):
    _fields_ = [(name, ctypes.c_uint32) for name in (
        "service_type",
        "current_state",
        "controls_accepted",
        "win32_exit_code",
        "service_specific_exit_code",
        "check_point",
        "wait_hint",
        "process_id",
        "service_flags",
    )]


class _FixedFileInfo(ctypes.Structure):
    _fields_ = [(name, ctypes.c_uint32) for name in (
        "signature",
        "structure_version",
        "file_version_ms",
        "file_version_ls",
        "product_version_ms",
        "product_version_ls",
        "file_flags_mask",
        "file_flags",
        "file_os",
        "file_type",
        "file_subtype",
        "file_date_ms",
        "file_date_ls",
    )]


def _file_product_version(path: Path) -> str:
    version = ctypes.WinDLL("version.dll", use_last_error=True)
    version.GetFileVersionInfoSizeW.argtypes = [
        ctypes.c_wchar_p,
        ctypes.POINTER(ctypes.c_uint32),
    ]
    version.GetFileVersionInfoSizeW.restype = ctypes.c_uint32
    version.GetFileVersionInfoW.argtypes = [
        ctypes.c_wchar_p,
        ctypes.c_uint32,
        ctypes.c_uint32,
        ctypes.c_void_p,
    ]
    version.GetFileVersionInfoW.restype = ctypes.c_int
    version.VerQueryValueW.argtypes = [
        ctypes.c_void_p,
        ctypes.c_wchar_p,
        ctypes.POINTER(ctypes.c_void_p),
        ctypes.POINTER(ctypes.c_uint32),
    ]
    version.VerQueryValueW.restype = ctypes.c_int
    size = version.GetFileVersionInfoSizeW(str(path), None)
    if size == 0:
        raise ctypes.WinError(ctypes.get_last_error())
    data = ctypes.create_string_buffer(size)
    if not version.GetFileVersionInfoW(str(path), 0, size, data):
        raise ctypes.WinError(ctypes.get_last_error())
    pointer = ctypes.c_void_p()
    length = ctypes.c_uint32()
    if not version.VerQueryValueW(data, "\\", ctypes.byref(pointer), ctypes.byref(length)):
        raise ctypes.WinError(ctypes.get_last_error())
    info = ctypes.cast(pointer, ctypes.POINTER(_FixedFileInfo)).contents
    fixed_version = (
        info.product_version_ms >> 16,
        info.product_version_ms & 0xFFFF,
        info.product_version_ls >> 16,
        info.product_version_ls & 0xFFFF,
    )

    translations = ctypes.c_void_p()
    translation_size = ctypes.c_uint32()
    if version.VerQueryValueW(
        data,
        "\\VarFileInfo\\Translation",
        ctypes.byref(translations),
        ctypes.byref(translation_size),
    ):
        values = ctypes.cast(translations, ctypes.POINTER(ctypes.c_uint16))
        for index in range(0, translation_size.value // 2, 2):
            language = values[index]
            code_page = values[index + 1]
            product = ctypes.c_void_p()
            product_size = ctypes.c_uint32()
            block = (
                f"\\StringFileInfo\\{language:04x}{code_page:04x}"
                "\\ProductVersion"
            )
            if version.VerQueryValueW(
                data,
                block,
                ctypes.byref(product),
                ctypes.byref(product_size),
            ) and product.value:
                return ctypes.wstring_at(product.value)
    return ".".join(str(value) for value in fixed_version)


def _require_hns_running() -> None:
    advapi = ctypes.WinDLL("advapi32.dll", use_last_error=True)
    advapi.OpenSCManagerW.argtypes = [
        ctypes.c_wchar_p,
        ctypes.c_wchar_p,
        ctypes.c_uint32,
    ]
    advapi.OpenSCManagerW.restype = ctypes.c_void_p
    advapi.OpenServiceW.argtypes = [
        ctypes.c_void_p,
        ctypes.c_wchar_p,
        ctypes.c_uint32,
    ]
    advapi.OpenServiceW.restype = ctypes.c_void_p
    advapi.QueryServiceStatusEx.argtypes = [
        ctypes.c_void_p,
        ctypes.c_int,
        ctypes.c_void_p,
        ctypes.c_uint32,
        ctypes.POINTER(ctypes.c_uint32),
    ]
    advapi.QueryServiceStatusEx.restype = ctypes.c_int
    advapi.CloseServiceHandle.argtypes = [ctypes.c_void_p]
    advapi.CloseServiceHandle.restype = ctypes.c_int
    manager = advapi.OpenSCManagerW(None, None, 1)
    if not manager:
        raise ctypes.WinError(ctypes.get_last_error())
    service = None
    try:
        service = advapi.OpenServiceW(manager, "hns", 4)
        if not service:
            raise ctypes.WinError(ctypes.get_last_error())
        status = _ServiceStatusProcess()
        needed = ctypes.c_uint32()
        if not advapi.QueryServiceStatusEx(
            service,
            0,
            ctypes.byref(status),
            ctypes.sizeof(status),
            ctypes.byref(needed),
        ):
            raise ctypes.WinError(ctypes.get_last_error())
        if status.current_state != 4:
            raise ScriptError(
                f"Host Network Service is not running (state: {status.current_state})"
            )
    finally:
        if service:
            advapi.CloseServiceHandle(service)
        advapi.CloseServiceHandle(manager)


def prepare_afxdp(config: AfxdpConfig) -> dict[str, Any]:
    if os.name != "nt":
        raise ScriptError("HCN AF_XDP workflows require Windows")
    require_file(config.microvm, f"missing VMM: {config.microvm}")
    require_file(config.kernel, f"missing kernel: {config.kernel}")
    require_file(config.initrd, f"missing initrd: {config.initrd}")
    endpoint = load_hcn_endpoint(config)

    xdp_api = Path(os.environ.get("WINDIR", r"C:\Windows")) / "System32" / "xdpapi.dll"
    require_file(xdp_api, "xdpapi.dll is missing; install signed XDP-for-Windows v1.3.0")
    observed = _file_product_version(xdp_api)
    if not observed.startswith("1.3.0"):
        raise ScriptError(
            f"XDP-for-Windows v1.3.0 is required, found {observed!r}"
        )
    _require_hns_running()
    selftest = run_capture([config.microvm, "--selftest", "--log-level", "warn"])
    require_success(selftest, "WHP self-test")
    if selftest.text.strip():
        print(selftest.text.strip())
    return endpoint


def _guest_http_probe(config: AfxdpConfig, web_port: int) -> str:
    return f"""marker=NVX-HCN-AFXDP
/bin/busybox timeout 20 /bin/busybox wget -qO /tmp/hcn-afxdp-http.log http://{config.gateway}:{web_port}/
probe_status=$?
read guest_tx_packets < /sys/class/net/eth0/statistics/tx_packets
read guest_tx_errors < /sys/class/net/eth0/statistics/tx_errors
read guest_carrier < /sys/class/net/eth0/carrier
read guest_operstate < /sys/class/net/eth0/operstate
read host_response < /tmp/hcn-afxdp-http.log
echo "${{marker}}-GUEST-TX=${{guest_tx_packets}} ERRORS=${{guest_tx_errors}} CARRIER=${{guest_carrier}} OPERSTATE=${{guest_operstate}}"
echo "${{marker}}-HTTP=${{host_response}}"
if [ "${{probe_status:-1}}" -eq 0 ] && [ "${{host_response}}" = HELLO-HOST ]; then
    echo "${{marker}}-SMOKE-OK"
    exit 0
fi
echo "${{marker}}-SMOKE-FAIL"
exit 42
"""


def test_hcn_afxdp(
    config: AfxdpConfig,
    *,
    web_port: int = 8099,
    log_path: Path = Path("build/performance/hcn-afxdp-smoke.log"),
    endpoint: Mapping[str, Any] | None = None,
    invoker: Callable[..., AfxdpVmResult] = run_afxdp_vm,
) -> AfxdpVmResult:
    endpoint = endpoint or prepare_afxdp(config)
    log_path = log_path.expanduser().resolve()
    log_path.parent.mkdir(parents=True, exist_ok=True)
    result: AfxdpVmResult | None = None
    with tempfile.TemporaryDirectory(prefix="nvx-hcn-afxdp-work-") as temporary:
        work_root = Path(temporary)
        (work_root / "hcn-afxdp-smoke.sh").write_text(
            _guest_http_probe(config, web_port), encoding="utf-8", newline="\n"
        )
        with HostHttpServer(web_port) as server:
            try:
                result = invoker(
                    config,
                    endpoint,
                    [
                        "--kernel",
                        config.kernel,
                        "--initrd",
                        config.initrd,
                        "--mem",
                        "512",
                        "--cmdline",
                        DEFAULT_CMDLINE,
                        "--mount",
                        work_root,
                        "--exec",
                        "/mnt/host/hcn-afxdp-smoke.sh",
                        "--log-level",
                        "info",
                    ],
                    required_marker="NVX-HCN-AFXDP-SMOKE-OK",
                )
            except Exception as error:
                log_path.write_text(
                    f"failure: {error}\n--- HTTP helper ---\n"
                    + "\n".join(server.logs),
                    encoding="utf-8",
                    newline="\n",
                )
                raise
            log_path.write_text(
                "control: "
                + json.dumps(result.ready, separators=(",", ":"))
                + "\n--- stderr ---\n"
                + result.stderr
                + "\n--- stdout ---\n"
                + result.stdout
                + "\n--- HTTP helper ---\n"
                + "\n".join(server.logs),
                encoding="utf-8",
                newline="\n",
            )

    assert result is not None
    if result.stderr:
        print(result.stderr, file=sys.stderr, end="" if result.stderr.endswith("\n") else "\n")
    if result.stdout:
        print(result.stdout, end="" if result.stdout.endswith("\n") else "\n")
    queues = ",".join(str(value) for value in result.ready["queues"])
    print(
        "PASS: externally managed HCN vNIC attached to AF_XDP "
        f"(LUID {result.ready['interfaceLuid']}, queues {queues})"
    )
    return result


def benchmark_hcn_afxdp_smoke(
    config: AfxdpConfig,
    *,
    runs: int = 5,
    web_port: int = 8099,
    log_directory: Path = Path("build/performance/hcn-afxdp-runs"),
) -> None:
    if runs < 1:
        raise ScriptError("number of runs must be at least 1")
    log_directory = log_directory.expanduser().resolve()
    log_directory.mkdir(parents=True, exist_ok=True)
    samples: list[float] = []
    print(f"HCN AF_XDP verified network benchmark, median of {runs} runs")
    for run in range(1, runs + 1):
        started = time.perf_counter()
        test_hcn_afxdp(
            config,
            web_port=web_port,
            log_path=log_directory / f"run-{run}.log",
        )
        elapsed = (time.perf_counter() - started) * 1000
        samples.append(elapsed)
        print(
            f"  verified network: run {run}/{runs} complete "
            f"(wall {elapsed:.1f} ms)"
        )
    print(f"  verified network wall : {format_median(samples, width=0)}")
    print("  verified marker       : NVX-HCN-AFXDP-SMOKE-OK")


def benchmark_hcn_afxdp_snapshot(
    config: AfxdpConfig,
    snapshot_path: Path,
    *,
    runs: int = 5,
    endpoint: Mapping[str, Any] | None = None,
    invoker: Callable[..., AfxdpVmResult] = run_afxdp_vm,
) -> None:
    if runs < 1:
        raise ScriptError("number of runs must be at least 1")
    endpoint = endpoint or prepare_afxdp(config)
    snapshot_path = snapshot_path.expanduser().resolve()
    cmdline = f"{DEFAULT_CMDLINE} virtnet_probe={config.gateway}"
    cold_marker = f"VIRTNET-PROBE-OK: {config.gateway}"
    restore_marker = f"NETSNAP-RESTORE-PROBE-OK: {config.gateway}"
    cold: list[float] = []
    cold_wall: list[float] = []
    restored: list[float] = []
    restore_wall: list[float] = []

    print(
        f"HCN AF_XDP networking + snapshot benchmark, median of {runs}, "
        "256 MiB, 1 vCPU"
    )
    try:
        for run in range(1, runs + 1):
            result = invoker(
                config,
                endpoint,
                [
                    "--kernel",
                    config.kernel,
                    "--initrd",
                    config.initrd,
                    "--mem",
                    "256",
                    "--cmdline",
                    cmdline,
                    "--exit-on-boot",
                    "--boot-marker",
                    cold_marker,
                ],
                required_marker=cold_marker,
                require_metric=True,
            )
            assert result.metric is not None
            cold.append(result.metric)
            cold_wall.append(result.wall_ms)
            print(
                f"  AF_XDP cold boot: run {run}/{runs} complete "
                f"(guest {result.metric:.1f} ms, wall {result.wall_ms:.1f} ms)"
            )

        if snapshot_path.exists():
            remove_tree(snapshot_path, label="AF_XDP snapshot")
        capture = invoker(
            config,
            endpoint,
            [
                "--kernel",
                config.kernel,
                "--initrd",
                config.initrd,
                "--mem",
                "256",
                "--cmdline",
                f"{cmdline} netsnap",
                "--snapshot",
                snapshot_path,
            ],
            required_marker="netsnap: pre-snapshot link OK",
        )
        for filename in ("state.bin", "mem.bin"):
            require_file(
                snapshot_path / filename,
                f"AF_XDP snapshot capture is incomplete: {snapshot_path}",
            )
        print(f"  AF_XDP snapshot capture complete ({capture.wall_ms:.1f} ms)")

        for run in range(1, runs + 1):
            result = invoker(
                config,
                endpoint,
                [
                    "--restore",
                    snapshot_path,
                    "--mem",
                    "256",
                    "--exit-on-boot",
                    "--boot-marker",
                    restore_marker,
                ],
                required_marker=restore_marker,
                require_metric=True,
            )
            assert result.metric is not None
            restored.append(result.metric)
            restore_wall.append(result.wall_ms)
            print(
                f"  AF_XDP restore: run {run}/{runs} complete "
                f"(guest {result.metric:.1f} ms, wall {result.wall_ms:.1f} ms)"
            )

        print(f"  cold  (guest start -> marker): {format_median(cold, width=0)}")
        print(f"  cold wall-clock               : {format_median(cold_wall, width=0)}")
        print(f"  restore (guest resume -> marker): {format_median(restored, width=0)}")
        print(f"  restore wall-clock             : {format_median(restore_wall, width=0)}")
        print("  verified marker                : NETSNAP-RESTORE-OK")
    finally:
        if snapshot_path.exists():
            remove_tree(snapshot_path, label="AF_XDP snapshot")
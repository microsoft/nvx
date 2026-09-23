"""Persistent lifecycle operations for managed NVX microVM sandboxes."""

from __future__ import annotations

import json
import os
import secrets
import subprocess
import time
import uuid
from pathlib import Path
from typing import Any, cast

from .build_constants import (
    AlpineBuildConstants,
    KernelBuildConstants,
)
from .common import (
    ScriptError,
    artifact_path,
    openvmm_binary_path,
    require_file,
)
from .control_session import (
    MANAGED_EXIT_CATEGORIES,
    ControlSession,
    ManagedExecResult,
)
from .sandbox import SandboxLaunch, SandboxLayer

CONFIG_NAME = "config.json"
RUNTIME_NAME = "runtime.json"
CAPABILITY_NAME = "control.capability"
LOG_NAME = "openvmm.log"
CONTROL_SOCKET_NAME = "control.sock"
OUTCOME_NAME = "outcome.json"
STATE_FORMAT = 1
OUTCOME_SCHEMA_VERSION = 1


def _write_json(path: Path, value: dict[str, Any], mode: int = 0o600) -> None:
    temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
    try:
        temporary.write_text(
            json.dumps(value, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        os.chmod(temporary, mode)
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def _read_json(
    path: Path,
    description: str,
    *,
    version_field: str = "format",
    version: int = STATE_FORMAT,
) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ScriptError(f"failed to read {description}: {path}") from error
    if not isinstance(value, dict):
        raise ScriptError(f"{description} has an unsupported format: {path}")
    typed = cast(dict[str, Any], value)
    if typed.get(version_field) != version:
        raise ScriptError(f"{description} has an unsupported format: {path}")
    return typed


def _outcome_destination(path: Path) -> Path:
    candidate = path if path.is_absolute() else Path.cwd() / path
    parent = candidate.parent
    if parent.is_symlink() or not parent.is_dir():
        raise ScriptError(f"outcome report parent is not a plain directory: {parent}")
    if not candidate.name:
        raise ScriptError("outcome report path has no filename")
    resolved = parent.resolve() / candidate.name
    if os.path.lexists(resolved):
        raise ScriptError(f"outcome report already exists: {resolved}")
    return resolved


def validate_outcome_destination(path: Path) -> None:
    _outcome_destination(path)


def _write_new_json(path: Path, value: dict[str, Any]) -> None:
    resolved = _outcome_destination(path)
    temporary = resolved.with_name(f".{resolved.name}.{uuid.uuid4().hex}.tmp")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_BINARY", 0)
    try:
        descriptor = os.open(temporary, flags, 0o600)
    except FileExistsError as error:
        raise ScriptError("failed to reserve an outcome report staging file") from error
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as output:
            json.dump(value, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        try:
            os.link(temporary, resolved)
        except FileExistsError as error:
            raise ScriptError(f"outcome report already exists: {resolved}") from error
        except OSError as error:
            raise ScriptError(
                f"failed to publish outcome report: {resolved}"
            ) from error
    finally:
        temporary.unlink(missing_ok=True)


def _read_openvmm_outcome(path: Path) -> dict[str, Any]:
    typed = _read_json(
        path,
        "OpenVMM outcome report",
        version_field="schema_version",
        version=OUTCOME_SCHEMA_VERSION,
    )
    for name in ("outcome", "network_policy", "teardown"):
        if not isinstance(typed.get(name), dict):
            raise ScriptError(
                f"OpenVMM outcome report has an invalid {name} section: {path}"
            )
    return typed


def write_exec_outcome(path: Path, result: ManagedExecResult) -> None:
    if result.category not in MANAGED_EXIT_CATEGORIES:
        raise ScriptError("managed workload returned an unsupported outcome category")
    if not -(2**31) <= result.returncode < 2**31:
        raise ScriptError("managed workload returned an out-of-range status")
    _write_new_json(
        path,
        {
            "schema_version": OUTCOME_SCHEMA_VERSION,
            "operation_id": secrets.token_hex(16),
            "outcome": {
                "operation": "exec",
                "category": result.category,
                "status_code": result.returncode,
            },
        },
    )


def _prepare_state_directory(path: Path, *, create: bool) -> Path:
    resolved = path.resolve()
    if create:
        resolved.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(resolved, 0o700)
    if resolved.is_symlink() or not resolved.is_dir():
        raise ScriptError(f"sandbox state path is not a plain directory: {resolved}")
    return resolved


def _serialize_launch(
    launch: SandboxLaunch,
    *,
    hypervisor: str,
    memory_mib: int,
    net: str | None,
    network_profile: str | None,
    network_egress: str | None,
    network_ingress: str | None,
    network_egress_allow: tuple[str, ...],
    network_egress_deny: tuple[str, ...],
    host_loopback: str | None,
    network_proxy: str | None,
    host_loopback_forward: tuple[str, ...],
    cmdline: str,
) -> dict[str, Any]:
    return {
        "format": STATE_FORMAT,
        "layers": [
            {
                "role": layer.role,
                "path": os.fspath(layer.path.resolve()),
                "uuid": layer.uuid,
            }
            for layer in launch.ordered_layers()
        ],
        "scratch": os.fspath(launch.scratch.resolve()),
        "hostname": launch.hostname,
        "workload_uid": launch.workload_identity[0],
        "workload_gid": launch.workload_identity[1],
        "memory_max": launch.memory_max,
        "pids_max": launch.pids_max,
        "hypervisor": hypervisor,
        "memory_mib": memory_mib,
        "net": net,
        "network_profile": network_profile,
        "network_egress": network_egress,
        "network_ingress": network_ingress,
        "network_egress_allow": list(network_egress_allow),
        "network_egress_deny": list(network_egress_deny),
        "host_loopback": host_loopback,
        "network_proxy": network_proxy,
        "host_loopback_forward": list(host_loopback_forward),
        "cmdline": cmdline,
    }


def _deserialize_launch(config: dict[str, Any]) -> SandboxLaunch:
    try:
        layers = tuple(
            SandboxLayer(
                role=str(layer["role"]),
                path=Path(str(layer["path"])),
                uuid=str(layer["uuid"]),
            )
            for layer in config["layers"]
        )
        identity = (int(config["workload_uid"]), int(config["workload_gid"]))
        launch = SandboxLaunch(
            layers=layers,
            scratch=Path(str(config["scratch"])),
            hostname=str(config["hostname"]),
            workload_identity=identity,
            memory_max=(
                None if config["memory_max"] is None else int(config["memory_max"])
            ),
            pids_max=None if config["pids_max"] is None else int(config["pids_max"]),
        )
    except (KeyError, TypeError, ValueError) as error:
        raise ScriptError("sandbox configuration is malformed") from error
    return launch.validated()


def _process_running(pid: int) -> bool:
    if pid <= 0:
        return False
    if os.name != "nt":
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False
        except PermissionError:
            return True

    import ctypes

    process_query_limited_information = 0x1000
    still_active = 259
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    handle = kernel32.OpenProcess(process_query_limited_information, False, pid)
    if not handle:
        return False
    try:
        exit_code = ctypes.c_uint32()
        if not kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code)):
            raise OSError(ctypes.get_last_error(), "GetExitCodeProcess failed")
        return exit_code.value == still_active
    finally:
        kernel32.CloseHandle(handle)


def _load_running(state_dir: Path) -> tuple[dict[str, Any], bytes]:
    runtime_path = state_dir / RUNTIME_NAME
    if not runtime_path.is_file():
        raise ScriptError("sandbox is not running")
    runtime = _read_json(runtime_path, "sandbox runtime state")
    try:
        pid = int(runtime["pid"])
    except (KeyError, TypeError, ValueError) as error:
        raise ScriptError("sandbox runtime state has an invalid process ID") from error
    if not _process_running(pid):
        raise ScriptError(
            "sandbox runtime state is stale because the OpenVMM process is not running"
        )
    capability = require_file(
        state_dir / CAPABILITY_NAME, "sandbox control capability"
    ).read_bytes()
    if len(capability) != 32 or capability == bytes(32):
        raise ScriptError("sandbox control capability is invalid")
    return runtime, capability


def _endpoint(runtime: dict[str, Any]) -> Path:
    try:
        return Path(str(runtime["control_endpoint"]))
    except KeyError as error:
        raise ScriptError("sandbox runtime state has no control endpoint") from error


def provision(
    state_path: Path,
    launch: SandboxLaunch,
    *,
    hypervisor: str,
    memory_mib: int,
    net: str | None,
    network_profile: str | None,
    network_egress: str | None,
    network_ingress: str | None,
    network_egress_allow: tuple[str, ...],
    network_egress_deny: tuple[str, ...],
    host_loopback: str | None,
    network_proxy: str | None,
    host_loopback_forward: tuple[str, ...],
    cmdline: str,
) -> None:
    state_dir = _prepare_state_directory(state_path, create=True)
    config_path = state_dir / CONFIG_NAME
    runtime_path = state_dir / RUNTIME_NAME
    if config_path.exists() or runtime_path.exists():
        raise ScriptError("sandbox is already provisioned")
    _write_json(
        config_path,
        _serialize_launch(
            launch.validated(),
            hypervisor=hypervisor,
            memory_mib=memory_mib,
            net=net,
            network_profile=network_profile,
            network_egress=network_egress,
            network_ingress=network_ingress,
            network_egress_allow=network_egress_allow,
            network_egress_deny=network_egress_deny,
            host_loopback=host_loopback,
            network_proxy=network_proxy,
            host_loopback_forward=host_loopback_forward,
            cmdline=cmdline,
        ),
    )


def start(state_path: Path, timeout: float) -> None:
    state_dir = _prepare_state_directory(state_path, create=False)
    config = _read_json(
        require_file(state_dir / CONFIG_NAME, "sandbox configuration"),
        "sandbox configuration",
    )
    if (state_dir / RUNTIME_NAME).exists():
        raise ScriptError("sandbox is already running or has stale runtime state")
    outcome_path = state_dir / OUTCOME_NAME
    outcome_path.unlink(missing_ok=True)
    launch = _deserialize_launch(config)
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
    kernel = require_file(
        artifact_path(KernelBuildConstants.BINARY_NAME), "Linux direct kernel"
    )
    initrd = require_file(
        artifact_path(AlpineBuildConstants.INITRAMFS_NAME), "initramfs"
    )
    capability = secrets.token_bytes(32)
    if capability == bytes(32):
        raise AssertionError("secrets.token_bytes returned an all-zero capability")
    endpoint_value = (
        f"//./pipe/openvmm-microvm-{uuid.uuid4().hex}"
        if os.name == "nt"
        else os.fspath(state_dir / CONTROL_SOCKET_NAME)
    )
    command = [
        os.fspath(executable),
        *launch.openvmm_arguments(),
        "--microvm-lifecycle",
        "managed",
        "--single-process",
        "--hypervisor",
        str(config["hypervisor"]),
        "--memory",
        f"{int(config['memory_mib'])}M",
        "--kernel",
        os.fspath(kernel),
        "--initrd",
        os.fspath(initrd),
        "--cmdline",
        launch.kernel_command_line(str(config["cmdline"])),
        "--virtio-console",
        "none",
        "--microvm-control-console",
        f"listen={endpoint_value}",
        "--microvm-control-auth-stdin",
        "--microvm-report",
        os.fspath(outcome_path),
    ]
    net = config.get("net")
    network_profile = config.get("network_profile")
    if net is not None:
        command.extend(["--net", str(net), "--network-profile", str(network_profile)])
    for name in ("network_egress", "network_ingress", "host_loopback"):
        value = config.get(name)
        if value is not None:
            command.extend([f"--{name.replace('_', '-')}", str(value)])
    for name in (
        "network_egress_allow",
        "network_egress_deny",
        "host_loopback_forward",
    ):
        for value in config.get(name, []):
            command.extend([f"--{name.replace('_', '-')}", str(value)])
    network_proxy = config.get("network_proxy")
    if network_proxy is not None:
        command.extend(["--network-proxy", str(network_proxy)])

    capability_path = state_dir / CAPABILITY_NAME
    capability_path.write_bytes(capability)
    os.chmod(capability_path, 0o600)
    log_path = state_dir / LOG_NAME
    log = log_path.open("ab", buffering=0)
    creationflags = (
        getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0) if os.name == "nt" else 0
    )
    process: subprocess.Popen[bytes] | None = None
    try:
        process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=os.name != "nt",
            creationflags=creationflags,
        )
        if process.stdin is None:
            raise ScriptError("failed to create the OpenVMM capability pipe")
        process.stdin.write(capability)
        process.stdin.close()
        _write_json(
            state_dir / RUNTIME_NAME,
            {
                "format": STATE_FORMAT,
                "pid": process.pid,
                "control_endpoint": endpoint_value,
            },
        )
        with ControlSession.connect(
            Path(endpoint_value), capability, timeout
        ) as session:
            session.ping(timeout)
    except BaseException:
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        (state_dir / RUNTIME_NAME).unlink(missing_ok=True)
        capability_path.unlink(missing_ok=True)
        (state_dir / CONTROL_SOCKET_NAME).unlink(missing_ok=True)
        raise
    finally:
        log.close()


def exec_workload(
    state_path: Path,
    arguments: tuple[str, ...],
    *,
    timeout_ms: int,
    response_timeout: float,
) -> ManagedExecResult:
    state_dir = _prepare_state_directory(state_path, create=False)
    runtime, capability = _load_running(state_dir)
    with ControlSession.connect(
        _endpoint(runtime), capability, response_timeout
    ) as session:
        return session.exec(
            arguments,
            timeout_ms=timeout_ms,
            response_timeout=response_timeout,
        )


def stop(state_path: Path, timeout: float) -> dict[str, Any]:
    state_dir = _prepare_state_directory(state_path, create=False)
    runtime, capability = _load_running(state_dir)
    with ControlSession.connect(_endpoint(runtime), capability, timeout) as session:
        session.stop(timeout)
    pid = int(runtime["pid"])
    deadline = time.monotonic() + timeout
    while _process_running(pid):
        if time.monotonic() >= deadline:
            raise TimeoutError("OpenVMM did not terminate after managed stop")
        time.sleep(0.025)
    try:
        outcome = _read_openvmm_outcome(state_dir / OUTCOME_NAME)
    finally:
        (state_dir / RUNTIME_NAME).unlink(missing_ok=True)
        (state_dir / CAPABILITY_NAME).unlink(missing_ok=True)
        (state_dir / CONTROL_SOCKET_NAME).unlink(missing_ok=True)
    return outcome


def deprovision(state_path: Path) -> None:
    state_dir = _prepare_state_directory(state_path, create=False)
    runtime_path = state_dir / RUNTIME_NAME
    if runtime_path.is_file():
        runtime = _read_json(runtime_path, "sandbox runtime state")
        try:
            running = _process_running(int(runtime["pid"]))
        except (KeyError, TypeError, ValueError) as error:
            raise ScriptError(
                "sandbox runtime state has an invalid process ID"
            ) from error
        if running:
            raise ScriptError("sandbox must be stopped before deprovision")
    for name in (
        RUNTIME_NAME,
        CAPABILITY_NAME,
        CONTROL_SOCKET_NAME,
        OUTCOME_NAME,
        LOG_NAME,
        CONFIG_NAME,
    ):
        (state_dir / name).unlink(missing_ok=True)
    unknown = tuple(state_dir.iterdir())
    if unknown:
        raise ScriptError(
            "sandbox state directory contains files not owned by NVX: "
            + ", ".join(path.name for path in unknown)
        )
    state_dir.rmdir()

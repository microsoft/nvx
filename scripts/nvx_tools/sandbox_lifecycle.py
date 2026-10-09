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
    path_exists,
    require_file,
)
from .control_session import (
    MANAGED_EXIT_CATEGORIES,
    ControlEndpointClosed,
    ControlSession,
    ManagedExecResult,
    capability_pipe,
)
from .host_control import HostControl
from .sandbox import SandboxLaunch, SandboxLayer, SandboxMount

CONFIG_NAME = "config.json"
RUNTIME_NAME = "runtime.json"
CAPABILITY_NAME = "control.capability"
LOG_NAME = "openvmm.log"
CONTROL_SOCKET_NAME = "control.sock"
HOST_CONTROL_SOCKET_NAME = "host-control.sock"
OUTCOME_NAME = "outcome.json"
STATE_FORMAT = 1
CONFIG_FORMAT = 1
# Format-1 readers ignore unknown fields, so a configuration with a live share
# uses a format that older NVX releases reject instead of starting without it.
MOUNT_CONFIG_FORMAT = 2
# Likewise, format-2 readers would start a caller-owned share as the VMM.
OWNER_CONFIG_FORMAT = 3
# And earlier readers know only the single `mount` share, so a configuration
# with several shares lists them under `mounts` in a format they reject.
MULTI_MOUNT_CONFIG_FORMAT = 4
# Format-4 readers would ignore allowed and writable paths and start a share
# without them, writable everywhere or with its allowed paths hidden, so a
# configuration with either lists every share under `mounts` in a format that
# they reject.
POLICY_CONFIG_FORMAT = 5
# Earlier readers would start a sandbox without its image slots, so a
# configuration that declares them lists every share under `mounts`, with its
# policy, in a format that they reject.
IMAGE_SLOT_CONFIG_FORMAT = 6
CONFIG_FORMATS = (
    CONFIG_FORMAT,
    MOUNT_CONFIG_FORMAT,
    OWNER_CONFIG_FORMAT,
    MULTI_MOUNT_CONFIG_FORMAT,
    POLICY_CONFIG_FORMAT,
    IMAGE_SLOT_CONFIG_FORMAT,
)
# These formats list every share under `mounts`.
MOUNTS_CONFIG_FORMATS = (
    MULTI_MOUNT_CONFIG_FORMAT,
    POLICY_CONFIG_FORMAT,
    IMAGE_SLOT_CONFIG_FORMAT,
)
# These formats also carry each share's allowed and writable paths.
MOUNT_POLICY_CONFIG_FORMATS = (POLICY_CONFIG_FORMAT, IMAGE_SLOT_CONFIG_FORMAT)
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
    version: int | tuple[int, ...] = STATE_FORMAT,
) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ScriptError(f"failed to read {description}: {path}") from error
    if not isinstance(value, dict):
        raise ScriptError(f"{description} has an unsupported format: {path}")
    typed = cast(dict[str, Any], value)
    accepted = (version,) if isinstance(version, int) else version
    if typed.get(version_field) not in accepted:
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
    if path_exists(resolved):
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
    config: dict[str, Any] = {
        "format": _config_format(launch.mounts, launch.image_slot_boot_count),
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
    config_format = config["format"]
    if config_format in MOUNTS_CONFIG_FORMATS:
        config["mounts"] = [
            _serialize_mount(mount, policy=config_format in MOUNT_POLICY_CONFIG_FORMATS)
            for mount in launch.mounts
        ]
    else:
        config["mount"] = _serialize_mount(launch.mounts[0]) if launch.mounts else None
    if config_format == IMAGE_SLOT_CONFIG_FORMAT:
        config["image_slot_boot_count"] = launch.image_slot_boot_count
    return config


def _config_format(
    mounts: tuple[SandboxMount, ...], image_slot_boot_count: int | None = None
) -> int:
    if image_slot_boot_count is not None:
        return IMAGE_SLOT_CONFIG_FORMAT
    if any(mount.allowed_paths or mount.writable_paths for mount in mounts):
        return POLICY_CONFIG_FORMAT
    if len(mounts) > 1:
        return MULTI_MOUNT_CONFIG_FORMAT
    if len(mounts) == 1 and mounts[0].owner == "caller":
        return OWNER_CONFIG_FORMAT
    return MOUNT_CONFIG_FORMAT if mounts else CONFIG_FORMAT


def _serialize_mount(mount: SandboxMount, *, policy: bool = False) -> dict[str, Any]:
    absolute = mount.absolute()
    serialized: dict[str, Any] = {
        "guest_target": absolute.guest_target,
        "host_path": os.fspath(absolute.host_path),
        "access": absolute.access,
        "denied_paths": list(absolute.denied_paths),
        "owner": absolute.owner,
    }
    if policy:
        serialized["allowed_paths"] = list(absolute.allowed_paths)
        serialized["writable_paths"] = list(absolute.writable_paths)
    return serialized


def _deserialize_paths(mount: dict[str, Any], key: str) -> tuple[str, ...]:
    paths = mount[key]
    if not isinstance(paths, list):
        raise TypeError(f"sandbox mount {key.replace('_', ' ')} must be a list")
    return tuple(str(path) for path in cast(list[object], paths))


def _deserialize_mount(value: object, *, policy: bool = False) -> SandboxMount:
    if not isinstance(value, dict):
        raise TypeError("sandbox mount configuration must be an object")
    mount = cast(dict[str, Any], value)
    return SandboxMount(
        guest_target=str(mount["guest_target"]),
        host_path=Path(str(mount["host_path"])),
        access=str(mount["access"]),
        denied_paths=_deserialize_paths(mount, "denied_paths"),
        # Configurations written before ownership modes ran shares as the VMM.
        owner=str(mount.get("owner", "vmm")),
        allowed_paths=_deserialize_paths(mount, "allowed_paths") if policy else (),
        writable_paths=_deserialize_paths(mount, "writable_paths") if policy else (),
    )


def _deserialize_mounts(config: dict[str, Any]) -> tuple[SandboxMount, ...]:
    if config.get("format") in MOUNTS_CONFIG_FORMATS:
        mounts = config["mounts"]
        if not isinstance(mounts, list):
            raise TypeError("sandbox mounts configuration must be a list")
        policy = config.get("format") in MOUNT_POLICY_CONFIG_FORMATS
        return tuple(
            _deserialize_mount(mount, policy=policy)
            for mount in cast(list[object], mounts)
        )
    mount = config.get("mount")
    return () if mount is None else (_deserialize_mount(mount),)


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
            mounts=_deserialize_mounts(config),
            # The format check below rejects a count in any other format.
            image_slot_boot_count=(
                None
                if config.get("image_slot_boot_count") is None
                else int(config["image_slot_boot_count"])
            ),
        )
    except (KeyError, TypeError, ValueError) as error:
        raise ScriptError("sandbox configuration is malformed") from error
    if config.get("format") != _config_format(
        launch.mounts, launch.image_slot_boot_count
    ):
        raise ScriptError(
            "sandbox configuration format does not match its mounts or image slots"
        )
    return launch.validated()


def _linux_start_time(stat: bytes) -> int | None:
    # The command name may contain spaces and parentheses, so the fields after it
    # start at its last ")", with the state as field 3 and starttime as field 22.
    end = stat.rfind(b")")
    fields = stat[end + 1 :].split() if end >= 0 else []
    if len(fields) < 20 or not fields[19].isdigit():
        raise ValueError("process status has an unexpected format")
    if fields[0] in (b"Z", b"X"):
        return None
    return int(fields[19])


def _process_start_time(pid: int) -> int | None:
    """Returns the start time of a live process, or None if no live process has the ID.

    A zombie counts as exited. On Windows, so does a process that the caller cannot
    open: OpenVMM runs as the caller, so such a process cannot be its VM. Raises
    OSError or ValueError when the state cannot be determined.
    """
    if os.name != "nt":
        try:
            stat = Path(f"/proc/{pid}/stat").read_bytes()
        except (FileNotFoundError, ProcessLookupError):
            return None
        return _linux_start_time(stat)

    import ctypes
    from ctypes import wintypes

    synchronize = 0x00100000
    process_query_limited_information = 0x1000
    error_access_denied = 5
    error_invalid_parameter = 87
    wait_object_0 = 0
    wait_timeout = 0x102
    if pid > 0xFFFFFFFF:
        return None
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.OpenProcess.argtypes = (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)
    kernel32.OpenProcess.restype = wintypes.HANDLE
    kernel32.WaitForSingleObject.argtypes = (wintypes.HANDLE, wintypes.DWORD)
    kernel32.WaitForSingleObject.restype = wintypes.DWORD
    filetime = ctypes.POINTER(wintypes.FILETIME)
    kernel32.GetProcessTimes.argtypes = (
        wintypes.HANDLE,
        filetime,
        filetime,
        filetime,
        filetime,
    )
    kernel32.GetProcessTimes.restype = wintypes.BOOL
    kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
    kernel32.CloseHandle.restype = wintypes.BOOL
    handle = kernel32.OpenProcess(
        synchronize | process_query_limited_information, False, pid
    )
    if not handle:
        error = ctypes.get_last_error()
        if error in (error_access_denied, error_invalid_parameter):
            return None
        raise ctypes.WinError(error)
    try:
        wait = kernel32.WaitForSingleObject(handle, 0)
        if wait == wait_object_0:
            return None
        if wait != wait_timeout:
            raise ctypes.WinError(ctypes.get_last_error())
        created = wintypes.FILETIME()
        unused = wintypes.FILETIME()
        if not kernel32.GetProcessTimes(
            handle,
            ctypes.byref(created),
            ctypes.byref(unused),
            ctypes.byref(unused),
            ctypes.byref(unused),
        ):
            raise ctypes.WinError(ctypes.get_last_error())
        return (created.dwHighDateTime << 32) | created.dwLowDateTime
    finally:
        kernel32.CloseHandle(handle)


def _process_running(pid: int, start_time: int | None) -> bool:
    """Returns whether the recorded OpenVMM process still runs.

    A record without a start time, written by an earlier NVX version, identifies
    OpenVMM by its process ID alone.
    """
    if pid <= 0:
        return False
    try:
        current = _process_start_time(pid)
    except (OSError, ValueError) as error:
        raise ScriptError(
            f"cannot determine whether OpenVMM process {pid} is running: {error}"
        ) from error
    if start_time is None:
        return current is not None
    return current == start_time


def _runtime_process(runtime: dict[str, Any]) -> tuple[int, int | None]:
    try:
        pid = int(runtime["pid"])
    except (KeyError, TypeError, ValueError) as error:
        raise ScriptError("sandbox runtime state has an invalid process ID") from error
    start_time = runtime.get("start_time")
    if start_time is not None and (
        not isinstance(start_time, int)
        or isinstance(start_time, bool)
        or start_time < 0
    ):
        raise ScriptError("sandbox runtime state has an invalid process start time")
    return pid, start_time


def runtime_process_running(runtime: dict[str, Any]) -> bool:
    """Returns whether the OpenVMM process of a runtime record still runs."""
    return _process_running(*_runtime_process(runtime))


def _load_running(state_dir: Path) -> tuple[dict[str, Any], bytes]:
    runtime_path = state_dir / RUNTIME_NAME
    if not runtime_path.is_file():
        raise ScriptError("sandbox is not running")
    runtime = _read_json(runtime_path, "sandbox runtime state")
    if not _process_running(*_runtime_process(runtime)):
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


def _end_failed_start(process: subprocess.Popen[bytes], grace: float) -> int | None:
    """Ends the OpenVMM process of a failed start.

    Returns OpenVMM's exit status if it exits on its own within `grace` seconds,
    or terminates it and returns None. Terminating OpenVMM while it publishes its
    outcome report would leave the report's staging file in the state directory,
    which `deprovision` refuses to remove.
    """
    try:
        if grace > 0:
            process.wait(timeout=grace)
    except subprocess.TimeoutExpired:
        pass
    finally:
        status = process.poll()
        if status is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
    return status


def _startup_exit_message(outcome_path: Path, status: int) -> str:
    """Describes how OpenVMM ended a start, preferring its outcome report.

    The report keeps the guest's own status, such as the one with which it
    refuses an unavailable workload identity.
    """
    try:
        outcome: dict[str, Any] = _read_openvmm_outcome(outcome_path)["outcome"]
    except ScriptError:
        outcome = {}
    category = outcome.get("category")
    status_code = outcome.get("status_code")
    if isinstance(category, str) and type(status_code) is int:
        return f"OpenVMM exited during startup: {category} status {status_code}"
    return f"OpenVMM exited during startup with status {status}"


def _host_endpoint(runtime: dict[str, Any]) -> Path:
    endpoint = runtime.get("host_control_endpoint")
    if endpoint is None:
        raise ScriptError("sandbox was not provisioned with image slots")
    return Path(str(endpoint))


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
        version=CONFIG_FORMATS,
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
    # Only a slot-declaring launch has a host-control endpoint.
    host_endpoint_value: str | None = None
    if launch.image_slot_boot_count is not None:
        host_endpoint_value = (
            f"//./pipe/openvmm-microvm-host-{uuid.uuid4().hex}"
            if os.name == "nt"
            else os.fspath(state_dir / HOST_CONTROL_SOCKET_NAME)
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
    if host_endpoint_value is not None:
        command.extend(("--microvm-host-control", f"listen={host_endpoint_value}"))
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
        values = config.get(name, [])
        if not isinstance(values, list):
            raise ScriptError("sandbox configuration is malformed")
        for value in cast(list[object], values):
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
        # OpenVMM reads its capability as soon as it starts, so the capability is
        # in the pipe, and the pipe's write end closed, before OpenVMM starts.
        capability_input = capability_pipe(capability)
        try:
            process = subprocess.Popen(
                command,
                stdin=capability_input,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=os.name != "nt",
                creationflags=creationflags,
            )
        finally:
            os.close(capability_input)
        # Until this process reaps the child or closes its handle, no other process
        # can reuse its ID, so this identifies OpenVMM itself.
        try:
            start_time = _process_start_time(process.pid)
        except (OSError, ValueError) as error:
            raise ScriptError(
                f"cannot identify the OpenVMM process: {error}"
            ) from error
        if start_time is None:
            raise ScriptError(
                "OpenVMM exited during startup"
                if process.poll() is not None
                else "cannot identify the OpenVMM process"
            )
        runtime: dict[str, Any] = {
            "format": STATE_FORMAT,
            "pid": process.pid,
            "start_time": start_time,
            "control_endpoint": endpoint_value,
        }
        if host_endpoint_value is not None:
            runtime["host_control_endpoint"] = host_endpoint_value
        _write_json(state_dir / RUNTIME_NAME, runtime)
        with ControlSession.connect(
            Path(endpoint_value), capability, timeout
        ) as session:
            session.ping(timeout)
    except BaseException as error:
        status: int | None = None
        try:
            if process is not None:
                # OpenVMM closes the control endpoint as it tears the VM down,
                # before it publishes its outcome report and exits, so only a
                # closed endpoint warrants waiting for it.
                status = _end_failed_start(
                    process, timeout if isinstance(error, ControlEndpointClosed) else 0
                )
        finally:
            (state_dir / RUNTIME_NAME).unlink(missing_ok=True)
            capability_path.unlink(missing_ok=True)
            (state_dir / CONTROL_SOCKET_NAME).unlink(missing_ok=True)
            (state_dir / HOST_CONTROL_SOCKET_NAME).unlink(missing_ok=True)
        if status is not None and isinstance(error, Exception):
            raise ScriptError(_startup_exit_message(outcome_path, status)) from error
        raise
    finally:
        log.close()


def exec_workload(
    state_path: Path,
    arguments: tuple[str, ...],
    *,
    timeout_ms: int,
    response_timeout: float,
    cwd: str | None = None,
    environment: tuple[str, ...] | None = None,
    inherit_default_environment: bool = False,
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
            cwd=cwd,
            environment=environment,
            inherit_default_environment=inherit_default_environment,
        )


def query_image_slots(state_path: Path, timeout: float) -> list[dict[str, Any]]:
    state_dir = _prepare_state_directory(state_path, create=False)
    runtime, capability = _load_running(state_dir)
    with HostControl.connect(_host_endpoint(runtime), capability, timeout) as control:
        return control.query_image_slots()


def bind_image_slot(
    state_path: Path,
    slot: int,
    image: Path,
    identity: str,
    timeout: float,
) -> None:
    state_dir = _prepare_state_directory(state_path, create=False)
    runtime, capability = _load_running(state_dir)
    # OpenVMM records at most 1024 identity bytes per slot.
    if not identity or len(identity.encode("utf-8")) > 1024:
        raise ScriptError("image slot identity must be 1..1024 UTF-8 bytes")
    # OpenVMM opens the media itself, as a read-only regular file that is not a
    # symbolic link, and only when the slot is empty. A retry with the bound
    # identity therefore succeeds even after the media's path is gone.
    with HostControl.connect(_host_endpoint(runtime), capability, timeout) as control:
        control.bind_image_slot(slot, image.absolute(), identity)


def stop(state_path: Path, timeout: float) -> dict[str, Any]:
    state_dir = _prepare_state_directory(state_path, create=False)
    runtime, capability = _load_running(state_dir)
    pid, start_time = _runtime_process(runtime)
    with ControlSession.connect(_endpoint(runtime), capability, timeout) as session:
        session.stop(timeout)
    deadline = time.monotonic() + timeout
    while _process_running(pid, start_time):
        if time.monotonic() >= deadline:
            raise TimeoutError("OpenVMM did not terminate after managed stop")
        time.sleep(0.025)
    try:
        outcome = _read_openvmm_outcome(state_dir / OUTCOME_NAME)
    finally:
        (state_dir / RUNTIME_NAME).unlink(missing_ok=True)
        (state_dir / CAPABILITY_NAME).unlink(missing_ok=True)
        (state_dir / CONTROL_SOCKET_NAME).unlink(missing_ok=True)
        (state_dir / HOST_CONTROL_SOCKET_NAME).unlink(missing_ok=True)
    return outcome


def deprovision(state_path: Path) -> None:
    state_dir = _prepare_state_directory(state_path, create=False)
    runtime_path = state_dir / RUNTIME_NAME
    if runtime_path.is_file():
        runtime = _read_json(runtime_path, "sandbox runtime state")
        if _process_running(*_runtime_process(runtime)):
            raise ScriptError("sandbox must be stopped before deprovision")
    for name in (
        RUNTIME_NAME,
        CAPABILITY_NAME,
        CONTROL_SOCKET_NAME,
        HOST_CONTROL_SOCKET_NAME,
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

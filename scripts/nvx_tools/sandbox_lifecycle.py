"""Persistent lifecycle operations for managed NVX microVM sandboxes."""

from __future__ import annotations

import contextlib
import errno
import json
import math
import os
import secrets
import stat
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
from .sandbox import SandboxLaunch, SandboxLayer, SandboxMount

CONFIG_NAME = "config.json"
RUNTIME_NAME = "runtime.json"
CAPABILITY_NAME = "control.capability"
LOG_NAME = "openvmm.log"
CONTROL_SOCKET_NAME = "control.sock"
OUTCOME_NAME = "outcome.json"
LOCK_NAME = "lifecycle.lock"
# Files that exist only from the start that launches an OpenVMM process until the
# stop or failed start that ends it, whether or not the runtime record names it.
RUNTIME_FILE_NAMES = (RUNTIME_NAME, CAPABILITY_NAME, CONTROL_SOCKET_NAME)
# How often a caller retries a lifecycle lock that another caller holds, and the
# removal of a state directory that a removed lock file still keeps.
LOCK_RETRY_INTERVAL = 0.025
# Deprovision writes this into the lock file before it removes the file, so that a
# waiter that then acquires the removed file knows the directory is going away.
LOCK_TOMBSTONE = b"deprovisioned\n"
# The errors with which rmdir() reports a directory that is not empty.
DIRECTORY_NOT_EMPTY = (errno.ENOTEMPTY, errno.EEXIST)
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
CONFIG_FORMATS = (
    CONFIG_FORMAT,
    MOUNT_CONFIG_FORMAT,
    OWNER_CONFIG_FORMAT,
    MULTI_MOUNT_CONFIG_FORMAT,
    POLICY_CONFIG_FORMAT,
)
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


def _open_lock_file(path: Path) -> int:
    """Opens the lifecycle lock file of a state directory, creating it if needed.

    Deprovision writes to the file, so it must be a regular file that no other
    name links, rather than a link to a file elsewhere.
    """
    try:
        descriptor = _open_lock_path(path)
    except OSError as error:
        if error.errno != errno.ELOOP:
            raise
        raise ScriptError(
            f"sandbox lifecycle lock is not a standalone regular file: {path}"
        ) from error
    try:
        status = os.fstat(descriptor)
        unsafe = not stat.S_ISREG(status.st_mode) or status.st_nlink != 1
        if os.name == "nt":
            # The handle refers to a symbolic link or junction, not its target.
            attributes = status.st_file_attributes
            unsafe = unsafe or bool(attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT)
        if unsafe:
            raise ScriptError(
                f"sandbox lifecycle lock is not a standalone regular file: {path}"
            )
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor


def _open_lock_path(path: Path) -> int:
    """Opens or creates path, without following a link that it names.

    On Windows, the file is shared for deletion, so that deprovision can remove it
    while it holds the lock and other callers wait for it.
    """
    if os.name != "nt":
        return os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)

    import ctypes
    import msvcrt
    from ctypes import wintypes

    generic_read = 0x80000000
    generic_write = 0x40000000
    file_share_read_write_delete = 0x7
    open_always = 4
    file_attribute_normal = 0x80
    file_flag_open_reparse_point = 0x00200000
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateFileW.argtypes = (
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.HANDLE,
    )
    kernel32.CreateFileW.restype = wintypes.HANDLE
    kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
    kernel32.CloseHandle.restype = wintypes.BOOL
    handle = kernel32.CreateFileW(
        os.fspath(path),
        generic_read | generic_write,
        file_share_read_write_delete,
        None,
        open_always,
        file_attribute_normal | file_flag_open_reparse_point,
        None,
    )
    if handle is None or handle == wintypes.HANDLE(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        return msvcrt.open_osfhandle(handle, os.O_RDWR)
    except BaseException:
        kernel32.CloseHandle(handle)
        raise


def _try_lock_file(descriptor: int) -> bool:
    """Takes the exclusive lock of an open lock file unless another caller holds it."""
    if os.name != "nt":
        import fcntl

        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
        return True

    import msvcrt

    try:
        # A new descriptor is at offset 0, so this locks the file's first byte.
        msvcrt.locking(descriptor, msvcrt.LK_NBLCK, 1)
    except OSError as error:
        if error.errno in (errno.EACCES, errno.EDEADLOCK):
            return False
        raise
    return True


def _unlock_file(descriptor: int) -> None:
    if os.name != "nt":
        import fcntl

        fcntl.flock(descriptor, fcntl.LOCK_UN)
        return

    import msvcrt

    msvcrt.locking(descriptor, msvcrt.LK_UNLCK, 1)


def _names_open_file(path: Path, descriptor: int) -> bool:
    """Returns whether path still names the file open as descriptor.

    Without POSIX deletion semantics, Windows keeps a removed file in its
    directory until its last handle closes, and denies access to it meanwhile.
    """
    try:
        current = os.stat(path)
    except FileNotFoundError:
        return False
    except PermissionError:
        if os.name == "nt":
            return False
        raise
    return os.path.samestat(current, os.fstat(descriptor))


def _read_lock_file(descriptor: int, size: int) -> bytes:
    os.lseek(descriptor, 0, os.SEEK_SET)
    try:
        data = b""
        while len(data) < size:
            chunk = os.read(descriptor, size - len(data))
            if not chunk:
                break
            data += chunk
        return data
    finally:
        os.lseek(descriptor, 0, os.SEEK_SET)


def _write_lock_file(descriptor: int, data: bytes) -> None:
    os.lseek(descriptor, 0, os.SEEK_SET)
    try:
        remaining = memoryview(data)
        while remaining:
            written = os.write(descriptor, remaining)
            if not written:
                raise OSError(
                    errno.EIO, "sandbox lifecycle lock write made no progress"
                )
            remaining = remaining[written:]
    finally:
        # msvcrt.locking() unlocks from the file position, as it locked.
        os.lseek(descriptor, 0, os.SEEK_SET)


def _pause(deadline: float) -> bool:
    """Waits for the next retry of a step that deadline bounds.

    Returns False once the deadline has passed, so that no retry starts after it.
    """
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        return False
    time.sleep(min(LOCK_RETRY_INTERVAL, remaining))
    return time.monotonic() < deadline


def _transition_in_progress() -> TimeoutError:
    return TimeoutError(
        "another lifecycle transition of the sandbox is still in progress"
    )


def _directory_status(path: Path) -> os.stat_result | None:
    try:
        return os.lstat(path)
    except OSError:
        return None


def _hold_directory(path: Path) -> tuple[os.stat_result, int | None]:
    """Returns the identity of a directory, and a descriptor that keeps it if any.

    On POSIX systems, the open descriptor keeps a directory that replaces this one
    from reusing its inode number. NTFS and ReFS do not reuse file IDs.
    """
    if os.name == "nt":
        return os.lstat(path), None
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        return os.fstat(descriptor), descriptor
    except BaseException:
        os.close(descriptor)
        raise


def _wait_for_handoff(
    state_dir: Path, removing: os.stat_result, deadline: float
) -> None:
    """Waits while lingering lock files keep the directory that deprovision removes.

    The wait ends once the path names no directory or another one, or the
    directory holds anything else, such as a provisioned sandbox, which makes
    deprovision fail. Once nothing keeps the directory, the caller removes it
    itself, in case deprovision has given up, as it provisions the path again.
    """
    while True:
        current = _directory_status(state_dir)
        if current is None or not os.path.samestat(current, removing):
            return
        try:
            entries = tuple(state_dir.iterdir())
        except OSError:
            return
        if not all(_lingers(entry) for entry in entries):
            return
        if not entries:
            try:
                state_dir.rmdir()
            except OSError as error:
                if error.errno not in DIRECTORY_NOT_EMPTY:
                    return
            else:
                return
        if not _pause(deadline):
            raise _transition_in_progress()


def _lingers(path: Path) -> bool:
    """Returns whether path is a removed lock file that a handle still keeps.

    NFS renames a removed file that is still open to `.nfs*`, and Windows file
    systems without POSIX deletion keep it under its own name. A transition that
    starts after deprovision removed the lock file also holds a lock file there
    until it finds the sandbox gone.
    """
    return path.name == LOCK_NAME or path.name.startswith(".nfs")


class _LifecycleLock:
    """Exclusive lock that serializes the lifecycle transitions of a state directory.

    `provision`, `start`, `stop`, and `deprovision` hold it from their first look
    at the state until they return, so overlapping transitions take effect one at
    a time, and each waits at most its timeout for another to finish. The one
    exception is deprovision, which releases it before it retries removing a
    directory that a removed lock file still keeps. `exec` does not take it, so a
    long workload cannot delay `stop`.

    A provisioned state directory keeps the lock file. A release removes it from
    any other directory, such as one that deprovision emptied or that never held a
    sandbox. A caller that acquires a lock file that its holder removed meanwhile
    holds no lock. Unless it provisions the sandbox, it then retries only if the
    sandbox is still provisioned, so that it creates no lock file in a directory
    that deprovision is removing. A provision that finds that deprovision removed
    the lock file, or the whole state directory, provisions a new directory once
    the old one is gone, unless another provision has provisioned it first.
    """

    def __init__(self, state_dir: Path, timeout: float, *, provision: bool = False):
        if not 0 < timeout < math.inf:
            raise ValueError("lifecycle timeout must be finite and greater than 0")
        self._state_dir = state_dir
        self._path = state_dir / LOCK_NAME
        self._timeout = timeout
        self._provision = provision
        self._descriptor: int | None = None

    def __enter__(self) -> _LifecycleLock:
        deadline = time.monotonic() + self._timeout
        while True:
            try:
                descriptor = _open_lock_file(self._path)
            except FileNotFoundError as error:
                # Deprovision removed the state directory meanwhile, which only a
                # provision recreates.
                if not self._provision:
                    raise ScriptError(
                        "sandbox state path is not a plain directory: "
                        f"{self._state_dir}"
                    ) from error
                if time.monotonic() >= deadline:
                    raise _transition_in_progress() from error
                _prepare_state_directory(self._state_dir, create=True)
                continue
            except PermissionError:
                # Without POSIX deletion semantics, Windows denies opening a removed
                # lock file until its last handle closes.
                if not self._provision or os.name != "nt" or not _pause(deadline):
                    raise
                continue
            try:
                while not _try_lock_file(descriptor):
                    if not _pause(deadline):
                        raise _transition_in_progress()
                if _names_open_file(self._path, descriptor):
                    self._descriptor = descriptor
                    return self
                tombstone = _read_lock_file(descriptor, len(LOCK_TOMBSTONE))
                # Deprovision leaves the mark only if lingering lock files, such as
                # the one that this handle may keep, kept it from removing the
                # directory, which the path then still names.
                removing = (
                    _directory_status(self._state_dir)
                    if self._provision and tombstone == LOCK_TOMBSTONE
                    else None
                )
            except BaseException:
                os.close(descriptor)
                raise
            os.close(descriptor)
            if not self._provision:
                if not path_exists(self._state_dir / CONFIG_NAME):
                    raise ScriptError("sandbox is not provisioned")
            elif removing is not None:
                _wait_for_handoff(self._state_dir, removing, deadline)
            if time.monotonic() >= deadline:
                raise _transition_in_progress()

    def remove(self) -> None:
        """Removes the held lock file, marking it so that waiters know why.

        If the removal fails, the mark goes again, as deprovision then keeps the
        directory.
        """
        assert self._descriptor is not None
        try:
            _write_lock_file(self._descriptor, LOCK_TOMBSTONE)
            self._path.unlink()
        except BaseException:
            self.retract()
            raise

    def retract(self) -> None:
        """Clears the mark of the removed lock file, so that waiters retry at once.

        Deprovision retracts it before it releases the lock, unless lingering lock
        files keep the directory, since its waiters then have no removal left to
        wait for.
        """
        assert self._descriptor is not None
        with contextlib.suppress(OSError):
            _write_lock_file(self._descriptor, bytes(len(LOCK_TOMBSTONE)))

    def __exit__(self, *_exception: object) -> None:
        self.release()

    def release(self) -> None:
        descriptor = self._descriptor
        if descriptor is None:
            return
        self._descriptor = None
        try:
            if not path_exists(self._state_dir / CONFIG_NAME):
                with contextlib.suppress(OSError):
                    if _names_open_file(self._path, descriptor):
                        self._path.unlink()
        finally:
            # Closing the descriptor releases the lock even if the unlock fails.
            with contextlib.suppress(OSError):
                _unlock_file(descriptor)
            os.close(descriptor)


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
        "format": _config_format(launch.mounts),
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
    config_format = _config_format(launch.mounts)
    if config_format in (MULTI_MOUNT_CONFIG_FORMAT, POLICY_CONFIG_FORMAT):
        config["mounts"] = [
            _serialize_mount(mount, policy=config_format == POLICY_CONFIG_FORMAT)
            for mount in launch.mounts
        ]
    else:
        config["mount"] = _serialize_mount(launch.mounts[0]) if launch.mounts else None
    return config


def _config_format(mounts: tuple[SandboxMount, ...]) -> int:
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
    if config.get("format") in (MULTI_MOUNT_CONFIG_FORMAT, POLICY_CONFIG_FORMAT):
        mounts = config["mounts"]
        if not isinstance(mounts, list):
            raise TypeError("sandbox mounts configuration must be a list")
        policy = config.get("format") == POLICY_CONFIG_FORMAT
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
        )
    except (KeyError, TypeError, ValueError) as error:
        raise ScriptError("sandbox configuration is malformed") from error
    if config.get("format") != _config_format(launch.mounts):
        raise ScriptError("sandbox configuration format does not match its mounts")
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
    timeout: float,
) -> None:
    state_dir = _prepare_state_directory(state_path, create=True)
    config_path = state_dir / CONFIG_NAME
    runtime_path = state_dir / RUNTIME_NAME
    with _LifecycleLock(state_dir, timeout, provision=True):
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
    with _LifecycleLock(state_dir, timeout):
        _start(state_dir, timeout)


def _create_capability(path: Path, capability: bytes) -> None:
    """Creates the control capability file of a start, which only it may hold."""
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_BINARY", 0)
    try:
        descriptor = os.open(path, flags, 0o600)
    except FileExistsError as error:
        raise ScriptError(
            "sandbox is already running or has stale runtime state"
        ) from error
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(capability)
    except BaseException:
        path.unlink(missing_ok=True)
        raise


def _start(state_dir: Path, timeout: float) -> None:
    config = _read_json(
        require_file(state_dir / CONFIG_NAME, "sandbox configuration"),
        "sandbox configuration",
        version=CONFIG_FORMATS,
    )
    # Any runtime file belongs to an OpenVMM process that may still run, even
    # without a runtime record, as after a start that was killed. Without them,
    # every runtime file that appears while this start holds the lifecycle lock
    # is its own.
    if any(path_exists(state_dir / name) for name in RUNTIME_FILE_NAMES):
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
        values = config.get(name, [])
        if not isinstance(values, list):
            raise ScriptError("sandbox configuration is malformed")
        for value in cast(list[object], values):
            command.extend([f"--{name.replace('_', '-')}", str(value)])
    network_proxy = config.get("network_proxy")
    if network_proxy is not None:
        command.extend(["--network-proxy", str(network_proxy)])

    capability_path = state_dir / CAPABILITY_NAME
    _create_capability(capability_path, capability)
    log_path = state_dir / LOG_NAME
    try:
        log = log_path.open("ab", buffering=0)
    except BaseException:
        capability_path.unlink(missing_ok=True)
        raise
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
        _write_json(
            state_dir / RUNTIME_NAME,
            {
                "format": STATE_FORMAT,
                "pid": process.pid,
                "start_time": start_time,
                "control_endpoint": endpoint_value,
            },
        )
        with ControlSession.connect(
            Path(endpoint_value), capability, timeout
        ) as session:
            session.ping(timeout)
    except BaseException as error:
        status: int | None = None
        ended = process is None
        try:
            if process is not None:
                # OpenVMM closes the control endpoint as it tears the VM down,
                # before it publishes its outcome report and exits, so only a
                # closed endpoint warrants waiting for it.
                status = _end_failed_start(
                    process, timeout if isinstance(error, ControlEndpointClosed) else 0
                )
                ended = True
        finally:
            # This start found none of these files and has held the lifecycle
            # lock since, so they are its own. They stay while its OpenVMM may
            # still run, so that the sandbox does not look stopped. The runtime
            # record goes last, so that an interrupted cleanup leaves a record
            # that shows OpenVMM gone rather than runtime files that deprovision
            # cannot vouch for.
            if ended:
                (state_dir / CONTROL_SOCKET_NAME).unlink(missing_ok=True)
                capability_path.unlink(missing_ok=True)
                (state_dir / RUNTIME_NAME).unlink(missing_ok=True)
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


def stop(state_path: Path, timeout: float) -> dict[str, Any]:
    state_dir = _prepare_state_directory(state_path, create=False)
    with _LifecycleLock(state_dir, timeout):
        return _stop(state_dir, timeout)


def _stop(state_dir: Path, timeout: float) -> dict[str, Any]:
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
        # As in a failed start, the runtime record goes last.
        (state_dir / CONTROL_SOCKET_NAME).unlink(missing_ok=True)
        (state_dir / CAPABILITY_NAME).unlink(missing_ok=True)
        (state_dir / RUNTIME_NAME).unlink(missing_ok=True)
    return outcome


def deprovision(state_path: Path, timeout: float) -> None:
    state_dir = _prepare_state_directory(state_path, create=False)
    with _LifecycleLock(state_dir, timeout) as lock:
        runtime_path = state_dir / RUNTIME_NAME
        if runtime_path.is_file():
            runtime = _read_json(runtime_path, "sandbox runtime state")
            if _process_running(*_runtime_process(runtime)):
                raise ScriptError("sandbox must be stopped before deprovision")
        elif any(path_exists(state_dir / name) for name in RUNTIME_FILE_NAMES):
            # As start does, refuse runtime files that no record vouches for.
            raise ScriptError(
                "sandbox has runtime files but no runtime record, so its OpenVMM "
                "process may still run: end any OpenVMM process whose arguments "
                f"name the state directory, then remove {CAPABILITY_NAME} and "
                f"{CONTROL_SOCKET_NAME}"
            )
        for name in (
            RUNTIME_NAME,
            CAPABILITY_NAME,
            CONTROL_SOCKET_NAME,
            OUTCOME_NAME,
            LOG_NAME,
            CONFIG_NAME,
        ):
            (state_dir / name).unlink(missing_ok=True)
        unknown = tuple(path for path in state_dir.iterdir() if path.name != LOCK_NAME)
        if unknown:
            raise ScriptError(
                "sandbox state directory contains files not owned by NVX: "
                + ", ".join(path.name for path in unknown)
            )
        # Holding the lock until the directory is gone keeps a waiting transition
        # from creating a new lock file in it.
        lock.remove()
        lingering = False
        try:
            state_dir.rmdir()
        except OSError as error:
            if error.errno not in DIRECTORY_NOT_EMPTY:
                raise
            lingering = True
        finally:
            if not lingering:
                lock.retract()
        if lingering:
            # A removed lock file that a handle still keeps stays in the directory,
            # as do the lock files of transitions that start meanwhile. Each
            # waiter closes its handle once it finds the sandbox gone, so the
            # directory empties after this call releases the lock.
            removing, held = _hold_directory(state_dir)
            try:
                lock.release()
                _remove_emptied_state_directory(state_dir, removing, timeout)
            finally:
                if held is not None:
                    os.close(held)


def _remove_emptied_state_directory(
    state_dir: Path, removing: os.stat_result, timeout: float
) -> None:
    """Removes a state directory once no lingering lock file keeps it.

    A provision that waits for the removal may complete it and provision the path
    again, which ends this attempt too. Any failure other than the directory not
    being empty ends the attempt at once.
    """
    deadline = time.monotonic() + timeout
    retried_at_once = False
    while True:
        try:
            state_dir.rmdir()
            return
        except FileNotFoundError:
            return
        except OSError as error:
            current = _directory_status(state_dir)
            if current is None or not os.path.samestat(current, removing):
                return
            try:
                remaining = tuple(state_dir.iterdir())
            except FileNotFoundError:
                return
            unexpected = sorted(path.name for path in remaining if not _lingers(path))
            # A provision that starts after this deprovision removed the lock
            # file, and before it removed the directory, provisions it again.
            if CONFIG_NAME in unexpected:
                raise ScriptError(
                    "sandbox was provisioned again while deprovision removed it"
                ) from error
            if unexpected:
                raise ScriptError(
                    "sandbox state directory contains files not owned by NVX: "
                    + ", ".join(unexpected)
                ) from error
            if error.errno not in DIRECTORY_NOT_EMPTY:
                raise
            # Without remaining entries, the last lingering lock file went away
            # after rmdir failed, so the next attempt follows at once. Another
            # such failure right after it pauses first, so that a file system that
            # does not list what keeps the directory cannot make this loop spin.
            if not remaining and not retried_at_once:
                retried_at_once = True
                if time.monotonic() >= deadline:
                    raise
                continue
            retried_at_once = False
            if not _pause(deadline):
                raise

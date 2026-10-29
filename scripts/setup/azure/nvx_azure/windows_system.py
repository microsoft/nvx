"""Windows operations that the Windows bootstrap commands share.

Python decides what to change. The adapters next to this package, small
PowerShell scripts with a JSON request on standard input and a JSON result on
a marked line of standard output, run only the cmdlets that have no
standard-library equivalent: storage, firewall, scheduled tasks, optional
features, Appx, and ACLs. The registry, processes, files, and HTTP stay in
Python.
"""

import json
import os
import sys
from collections.abc import Generator
from contextlib import contextmanager
from pathlib import Path, PureWindowsPath

from . import common
from .common import BootstrapError, JsonObject, Log

ROOT = Path(r"C:\ProgramData\NvxBootstrap")
CONTINUATION_TASK = "Nvx-Setup-GitHub-Runner"
# The runner bootstrap resumes through its task after reboots, so a reboot is
# requested only while that task exists.
REBOOT_DELAY_SECONDS = 60
# Windows PowerShell also writes warnings and host messages to standard
# output, so each adapter marks the line that holds its result.
RESULT_PREFIX = b"NVX-ADAPTER-RESULT "


def powershell() -> str:
    system_root = os.environ.get("SystemRoot", r"C:\Windows")
    return str(
        PureWindowsPath(system_root) / "System32/WindowsPowerShell/v1.0/powershell.exe"
    )


def powershell_environment() -> dict[str, str]:
    """Return this process's environment for a Windows PowerShell child.

    PowerShell 7 puts its own module directories first on PSModulePath, where
    Windows PowerShell would load incompatible modules; without the variable,
    Windows PowerShell uses its default module path.
    """
    return {
        name: value
        for name, value in os.environ.items()
        if name.upper() != "PSMODULEPATH"
    }


def adapter(
    script: str, operation: str, request: JsonObject | None = None, root: Path = ROOT
) -> object:
    """Run one adapter operation and return its decoded JSON result."""
    result = common.run_command(
        [
            powershell(),
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            str(root / script),
            "-Operation",
            operation,
        ],
        stdin=json.dumps(request or {}).encode(),
        check=False,
        env=powershell_environment(),
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).decode("utf-8", "replace").strip()
        raise BootstrapError(f"{script} {operation} failed: {detail[-1000:]}")
    results = [
        line[len(RESULT_PREFIX) :]
        for line in result.stdout.splitlines()
        if line.startswith(RESULT_PREFIX)
    ]
    try:
        if len(results) != 1:
            raise ValueError("no single result line")
        return json.loads(results[0])
    except ValueError as error:
        raise BootstrapError(f"{script} {operation} returned no JSON result") from error


def registry_value(key: str, name: str) -> object | None:
    """Read a value below HKEY_LOCAL_MACHINE, or None when it is absent."""
    if sys.platform != "win32":
        raise BootstrapError("The Windows registry requires Windows")
    import winreg

    try:
        with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, key) as handle:
            value, _ = winreg.QueryValueEx(handle, name)
    except FileNotFoundError:
        return None
    return value


def set_registry_value(key: str, name: str, value: int | str | list[str]) -> None:
    if sys.platform != "win32":
        raise BootstrapError("The Windows registry requires Windows")
    import winreg

    kind = (
        winreg.REG_DWORD
        if isinstance(value, int)
        else (winreg.REG_MULTI_SZ if isinstance(value, list) else winreg.REG_SZ)
    )
    with winreg.CreateKeyEx(
        winreg.HKEY_LOCAL_MACHINE, key, 0, winreg.KEY_SET_VALUE
    ) as handle:
        winreg.SetValueEx(handle, name, 0, kind, value)


def delete_registry_value(key: str, name: str) -> None:
    if sys.platform != "win32":
        raise BootstrapError("The Windows registry requires Windows")
    import winreg

    try:
        with winreg.OpenKey(
            winreg.HKEY_LOCAL_MACHINE, key, 0, winreg.KEY_SET_VALUE
        ) as handle:
            winreg.DeleteValue(handle, name)
    except FileNotFoundError:
        pass


def task_exists(name: str = CONTINUATION_TASK) -> bool:
    return adapter("system.ps1", "TaskExists", {"name": name}) is True


def boot_time() -> str:
    value = adapter("system.ps1", "BootTime")
    if not isinstance(value, str) or not value:
        raise BootstrapError("Windows reported no boot time")
    return value


def request_reboot(reason: str, log: Log) -> None:
    """Schedule a reboot that the continuation task resumes from."""
    if not task_exists():
        raise BootstrapError("Runner continuation task is missing; refusing to reboot.")
    common.run_command(
        ["shutdown.exe", "/r", "/t", str(REBOOT_DELAY_SECONDS), "/c", reason]
    )
    log.marker("NVX_CLOUD_RUNNER_REBOOT_SCHEDULED")


def is_reparse_point(path: Path) -> bool:
    if sys.platform != "win32":
        return path.is_symlink()
    import stat

    return bool(os.lstat(path).st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT)


def require_regular_directory(path: Path) -> None:
    """Create a directory or require the existing one to be a plain directory."""
    if not path.exists() and not path.is_symlink():
        path.mkdir()
    if not path.is_dir() or is_reparse_point(path):
        raise BootstrapError(f"{path} is not a regular directory")


def require_regular_file(path: Path) -> None:
    """Allow a missing file, or require the existing one to be a plain file."""
    if (path.exists() or path.is_symlink()) and (
        not path.is_file() or is_reparse_point(path)
    ):
        raise BootstrapError(f"{path} is not a regular file")


@contextmanager
def exclusive(path: Path) -> Generator[None, None, None]:
    """Hold an exclusive lock on a file for the duration of the block."""
    if sys.platform != "win32":
        raise BootstrapError("The bootstrap lock requires Windows")
    import msvcrt

    descriptor = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        try:
            msvcrt.locking(descriptor, msvcrt.LK_NBLCK, 1)
        except OSError as busy:
            raise BootstrapError("another Windows bootstrap is running") from busy
        try:
            yield
        finally:
            os.lseek(descriptor, 0, os.SEEK_SET)
            msvcrt.locking(descriptor, msvcrt.LK_UNLCK, 1)
    finally:
        os.close(descriptor)


@contextmanager
def named_mutex(name: str, timeout_seconds: float) -> Generator[None, None, None]:
    """Hold a named mutex, waiting up to the timeout for its holder to release it.

    A mutex that its holder abandoned by exiting counts as acquired.
    """
    if sys.platform != "win32":
        raise BootstrapError("Named mutexes require Windows")
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateMutexW.argtypes = (ctypes.c_void_p, wintypes.BOOL, wintypes.LPCWSTR)
    kernel32.CreateMutexW.restype = wintypes.HANDLE
    kernel32.WaitForSingleObject.argtypes = (wintypes.HANDLE, wintypes.DWORD)
    kernel32.WaitForSingleObject.restype = wintypes.DWORD
    kernel32.ReleaseMutex.argtypes = (wintypes.HANDLE,)
    kernel32.ReleaseMutex.restype = wintypes.BOOL
    kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
    kernel32.CloseHandle.restype = wintypes.BOOL
    handle = kernel32.CreateMutexW(None, False, name)
    if not handle:
        raise BootstrapError(
            f"Could not open mutex {name}: {ctypes.WinError(ctypes.get_last_error())}"
        )
    try:
        status = kernel32.WaitForSingleObject(handle, int(timeout_seconds * 1000))
        if status == 0x102:  # WAIT_TIMEOUT
            raise BootstrapError(f"Timed out waiting for mutex {name}")
        # WAIT_OBJECT_0 or WAIT_ABANDONED
        if status not in (0x0, 0x80):
            raise BootstrapError(
                f"Could not wait for mutex {name}: "
                f"{ctypes.WinError(ctypes.get_last_error())}"
            )
        try:
            yield
        finally:
            kernel32.ReleaseMutex(handle)
    finally:
        kernel32.CloseHandle(handle)

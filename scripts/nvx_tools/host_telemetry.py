#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Record safe benchmark host provenance and bounded storage telemetry.

Provenance names the runner and machine, the CPU and memory, the Azure VM
size and managed-disk configuration, and the volumes behind the benchmark's
workspace, temporary, and scratch directories. It never records credentials,
network addresses, Azure resource identifiers, names, or tags.

Storage telemetry samples cumulative operating-system counters while a
snapshot capture runs, so that each profiled capture phase can be compared
with the disk, CPU, writeback, and Microsoft Defender activity that overlapped
it. Windows CI jobs run as Network Service, which cannot read performance
counters, so the sampler uses only interfaces that any account can query:
volume performance IOCTLs, whose on-demand counters an open disk performance
WMI data block enables, GetSystemTimes, and NtQuerySystemInformation.
"""

from __future__ import annotations

import copy
import ctypes
import functools
import http.client
import json
import mmap
import os
import platform
import re
import shutil
import struct
import sys
import threading
import time
import urllib.request
import uuid
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import NamedTuple, Protocol, cast

HOST_PROVENANCE_SCHEMA_VERSION = 1
STORAGE_TELEMETRY_SCHEMA_VERSION = 1
STORAGE_TELEMETRY_INTERVAL_SECONDS = 0.05
STORAGE_TELEMETRY_PROCESS_INTERVAL_SECONDS = 0.5
STORAGE_TELEMETRY_MAX_SAMPLES = 600
STORAGE_TELEMETRY_PHASES = (
    "capture.snapshot_generation",
    "capture.mapped_memory_flush",
)
AZURE_IMDS_URL = (
    "http://169.254.169.254/metadata/instance/compute?api-version=2021-05-01"
)
AZURE_IMDS_TIMEOUT_SECONDS = 2.0
AZURE_CHASSIS_ASSET_TAG = "7783-7084-3265-9085-8269-3286-77"
DEFENDER_PROCESS_NAMES = frozenset({"msmpeng.exe", "mpdefendercoreservice.exe"})
MIB = 1024 * 1024
GIB = 1024 * MIB
TICKS_PER_SECOND = 10_000_000
"""Windows reports disk, CPU, and process times in 100 ns ticks."""

_WINDOWS_PROCESSOR_KEY = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0"
_WINDOWS_BIOS_KEY = r"HARDWARE\DESCRIPTION\System\BIOS"
_SCSI_ADDRESS = re.compile(r"\d+:\d+:\d+:\d+")


def host_provenance(volumes: Mapping[str, Path]) -> dict[str, object]:
    """Return safe provenance for this host and the volumes of ``volumes``.

    ``volumes`` maps a role, such as ``scratch``, to a path on the volume.
    """
    return {
        "schema_version": HOST_PROVENANCE_SCHEMA_VERSION,
        "runner_name": os.environ.get("RUNNER_NAME") or None,
        "machine_name": platform.node() or None,
        "os": platform.platform(),
        "cpu_model": cpu_model(),
        "logical_processors": os.cpu_count(),
        "memory_bytes": physical_memory_bytes(),
        "azure": azure_vm_provenance(),
        "volumes": {role: volume_provenance(path) for role, path in volumes.items()},
    }


def cpu_model() -> str | None:
    """Return the host CPU's brand string as the host OS reports it."""
    try:
        if sys.platform == "win32":
            import winreg

            with winreg.OpenKey(
                winreg.HKEY_LOCAL_MACHINE, _WINDOWS_PROCESSOR_KEY
            ) as key:
                value, _ = winreg.QueryValueEx(key, "ProcessorNameString")
            return str(value).strip() or None
        for line in Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
            name, separator, value = line.partition(":")
            if separator and name.strip() == "model name":
                return value.strip() or None
    except (OSError, UnicodeError):
        pass
    return platform.processor() or None


class _MemoryStatus(ctypes.Structure):
    _fields_ = [
        ("length", ctypes.c_uint32),
        ("memory_load", ctypes.c_uint32),
        ("total_physical", ctypes.c_uint64),
        ("available_physical", ctypes.c_uint64),
        ("total_page_file", ctypes.c_uint64),
        ("available_page_file", ctypes.c_uint64),
        ("total_virtual", ctypes.c_uint64),
        ("available_virtual", ctypes.c_uint64),
        ("available_extended_virtual", ctypes.c_uint64),
    ]


def physical_memory_bytes() -> int | None:
    """Return the host's installed physical memory in bytes."""
    if sys.platform == "win32":
        status = _MemoryStatus()
        status.length = ctypes.sizeof(status)
        try:
            if not _kernel32().GlobalMemoryStatusEx(ctypes.byref(status)):
                return None
        except OSError:
            return None
        return int(status.total_physical)
    try:
        return os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
    except (OSError, ValueError):
        return None


def azure_vm_provenance() -> dict[str, object] | None:
    """Return the Azure VM size and disks, or None off Azure.

    The instance metadata service is queried at most once per process.
    """
    return copy.deepcopy(_azure_vm_provenance())


@functools.cache
def _azure_vm_provenance() -> dict[str, object] | None:
    if not may_be_azure_vm():
        return None
    try:
        return azure_compute_provenance(read_azure_compute_metadata())
    except (OSError, ValueError, http.client.HTTPException) as error:
        return {"error": f"Azure instance metadata is unavailable: {error}"}


def may_be_azure_vm() -> bool:
    """Return whether the firmware identifies a possible Azure VM.

    Linux reads Azure's chassis asset tag. Windows exposes only the Hyper-V
    manufacturer and product to unprivileged accounts, so on another Hyper-V
    guest the metadata query fails after its timeout and records the error.
    """
    try:
        if sys.platform == "win32":
            import winreg

            with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, _WINDOWS_BIOS_KEY) as key:
                manufacturer, _ = winreg.QueryValueEx(key, "SystemManufacturer")
                product, _ = winreg.QueryValueEx(key, "SystemProductName")
            return (manufacturer, product) == (
                "Microsoft Corporation",
                "Virtual Machine",
            )
        tag = Path("/sys/class/dmi/id/chassis_asset_tag").read_text(encoding="ascii")
        return tag.strip() == AZURE_CHASSIS_ASSET_TAG
    except (OSError, UnicodeError):
        return False


def read_azure_compute_metadata(
    timeout: float = AZURE_IMDS_TIMEOUT_SECONDS,
) -> object:
    request = urllib.request.Request(AZURE_IMDS_URL, headers={"Metadata": "true"})
    # The link-local metadata service must never be reached through a proxy.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(request, timeout=timeout) as response:
        return json.loads(response.read(MIB).decode("utf-8"))


def azure_compute_provenance(compute: object) -> dict[str, object]:
    """Keep only the VM size and disk performance settings of IMDS compute
    metadata. Names, resource IDs, the subscription, tags, and network data
    are dropped."""
    document = _string_keys(compute)
    vm_size = _text(document.get("vmSize"))
    if vm_size is None:
        raise ValueError("compute metadata has no vmSize")
    storage = _string_keys(document.get("storageProfile"))
    os_disk = _string_keys(storage.get("osDisk"))
    data_disks = storage.get("dataDisks")
    disks = cast(list[object], data_disks) if isinstance(data_disks, list) else []
    return {
        "vm_size": vm_size,
        "os_disk": _azure_disk(os_disk) if os_disk else None,
        "data_disks": [
            _azure_disk(disk, data_disk=True)
            for disk in map(_string_keys, disks)
            if disk
        ],
        "resource_disk_size_kib": _integer(
            _string_keys(storage.get("resourceDisk")).get("size")
        ),
    }


def _azure_disk(
    disk: Mapping[str, object], *, data_disk: bool = False
) -> dict[str, object]:
    result: dict[str, object] = {}
    if data_disk:
        result["lun"] = _integer(disk.get("lun"))
    result.update(
        {
            "storage_account_type": _text(
                _string_keys(disk.get("managedDisk")).get("storageAccountType")
            ),
            "size_gib": _integer(disk.get("diskSizeGB")),
            "caching": _text(disk.get("caching")),
            "write_accelerator_enabled": _boolean(disk.get("writeAcceleratorEnabled")),
            "ephemeral_option": _text(
                _string_keys(disk.get("diffDiskSettings")).get("option")
            ),
        }
    )
    if data_disk:
        # IMDS populates the provisioned limits only for Ultra Disks.
        result["bytes_per_second_throttle"] = _integer(
            disk.get("bytesPerSecondThrottle")
        )
        result["ops_per_second_throttle"] = _integer(disk.get("opsPerSecondThrottle"))
    return result


def _string_keys(value: object) -> dict[str, object]:
    if not isinstance(value, dict):
        return {}
    return {str(key): item for key, item in cast(dict[object, object], value).items()}


def _text(value: object) -> str | None:
    return value if isinstance(value, str) and value else None


def _integer(value: object) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.isdecimal():
        return int(value)
    return None


def _boolean(value: object) -> bool | None:
    if isinstance(value, bool):
        return value
    if isinstance(value, str) and value.lower() in ("true", "false"):
        return value.lower() == "true"
    return None


_VOLUME_ROLE_LABELS = (
    ("workspace", "Workspace volume"),
    ("temporary", "Temporary-directory volume"),
    ("scratch", "Benchmark scratch volume"),
)


def host_provenance_rows(host: Mapping[str, object]) -> list[tuple[str, str]]:
    """Return labeled, human-readable values of recorded host provenance."""
    rows = [
        ("Runner", _text(host.get("runner_name")) or "not recorded"),
        ("Machine", _text(host.get("machine_name")) or "unknown"),
        ("Operating system", _text(host.get("os")) or "unknown"),
    ]
    cpu = _text(host.get("cpu_model")) or "unknown CPU"
    processors = _integer(host.get("logical_processors"))
    if processors is not None:
        cpu = f"{cpu}, {processors} logical processors"
    rows.append(("CPU", cpu))
    memory = _integer(host.get("memory_bytes"))
    if memory is not None:
        rows.append(("Memory", f"{memory / GIB:.1f} GiB"))
    azure = _string_keys(host.get("azure"))
    if azure:
        rows.extend(_azure_rows(azure))
    volumes = _string_keys(host.get("volumes"))
    for role, label in _VOLUME_ROLE_LABELS:
        if role in volumes:
            rows.append((label, _describe_volume(_string_keys(volumes[role]))))
    return rows


def describe_host_provenance(host: Mapping[str, object]) -> str:
    """Return recorded host provenance as one line."""
    return "; ".join(f"{label}: {value}" for label, value in host_provenance_rows(host))


def _azure_rows(azure: Mapping[str, object]) -> list[tuple[str, str]]:
    error = _text(azure.get("error"))
    if error is not None:
        return [("Azure metadata", error)]
    rows = [("Azure VM size", _text(azure.get("vm_size")) or "unknown")]
    os_disk = _string_keys(azure.get("os_disk"))
    if os_disk:
        rows.append(("Azure OS disk", _describe_azure_disk(os_disk)))
    data_disks = azure.get("data_disks")
    disks = cast(list[object], data_disks) if isinstance(data_disks, list) else []
    for disk in map(_string_keys, disks):
        lun = _integer(disk.get("lun"))
        label = "Azure data disk" if lun is None else f"Azure data disk LUN {lun}"
        rows.append((label, _describe_azure_disk(disk)))
    temporary = _integer(azure.get("resource_disk_size_kib"))
    if temporary:
        rows.append(("Azure temporary disk", f"{temporary / MIB:.1f} GiB"))
    return rows


def _describe_azure_disk(disk: Mapping[str, object]) -> str:
    parts = [_text(disk.get("storage_account_type")) or "unknown SKU"]
    size = _integer(disk.get("size_gib"))
    if size is not None:
        parts.append(f"{size} GiB")
    caching = _text(disk.get("caching"))
    if caching is not None:
        parts.append(f"{caching} caching")
    if disk.get("write_accelerator_enabled") is True:
        parts.append("write accelerator")
    ephemeral = _text(disk.get("ephemeral_option"))
    if ephemeral is not None:
        parts.append(f"ephemeral ({ephemeral})")
    throughput = _integer(disk.get("bytes_per_second_throttle"))
    if throughput is not None:
        parts.append(f"{throughput / MIB:.0f} MiB/s limit")
    operations = _integer(disk.get("ops_per_second_throttle"))
    if operations is not None:
        parts.append(f"{operations} IOPS limit")
    return ", ".join(parts)


def _describe_volume(volume: Mapping[str, object]) -> str:
    error = _text(volume.get("error"))
    if error is not None:
        return f"unavailable ({error})"
    name = _text(volume.get("volume")) or "unknown volume"
    filesystem = _text(volume.get("filesystem"))
    parts = [name if filesystem is None else f"{name} {filesystem}"]
    size = _integer(volume.get("size_bytes"))
    if size is not None:
        parts.append(f"{size / GIB:.1f} GiB")
    if volume.get("system_volume") is True:
        parts.append("system volume")
    disk = _integer(volume.get("disk_number"))
    if disk is not None:
        parts.append(f"disk {disk}")
    device = _text(volume.get("device"))
    if device is not None:
        parts.append(f"device {device}")
    address = _text(volume.get("scsi_address"))
    if address is not None:
        parts.append(f"SCSI {address}")
    path = _text(volume.get("path"))
    if path is not None:
        parts.append(f"at {path}")
    return ", ".join(parts)


def volume_provenance(path: Path) -> dict[str, object]:
    """Describe the volume that holds ``path``."""
    try:
        if sys.platform == "win32":
            return _windows_volume_provenance(path)
        return _linux_volume_provenance(path)
    except (OSError, ValueError) as error:
        return {"path": str(path), "error": str(error)}


def _linux_volume_provenance(path: Path) -> dict[str, object]:
    if sys.platform == "win32":
        raise OSError("Linux volume provenance is unavailable on Windows")
    device = os.stat(path).st_dev
    major, minor = os.major(device), os.minor(device)
    result: dict[str, object] = {"path": str(path)}
    mountinfo = Path("/proc/self/mountinfo").read_text(encoding="utf-8")
    mount = linux_mount(str(path.resolve()), f"{major}:{minor}", mountinfo)
    if mount is not None:
        result["volume"], result["filesystem"] = mount
        result["system_volume"] = mount[0] == "/"
    result["size_bytes"] = shutil.disk_usage(path).total
    block = Path(f"/sys/dev/block/{major}:{minor}")
    if block.exists():
        resolved = block.resolve()
        disk = resolved.parent if (resolved / "partition").exists() else resolved
        result["device"] = resolved.name
        result["disk"] = disk.name
        address = (disk / "device").resolve().name
        if _SCSI_ADDRESS.fullmatch(address):
            result["scsi_address"] = address
    return result


def linux_mount(target: str, device: str, mountinfo: str) -> tuple[str, str] | None:
    """Return the mount point and file system of the deepest mount of
    ``device`` in ``mountinfo`` that contains the resolved path ``target``."""
    deepest: tuple[str, str] | None = None
    for line in mountinfo.splitlines():
        fields = line.split()
        if len(fields) < 10 or fields[2] != device or "-" not in fields[6:]:
            continue
        mount_point = re.sub(
            r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), fields[4]
        )
        filesystem = fields[fields.index("-", 6) + 1]
        contains = target == mount_point or target.startswith(
            mount_point.rstrip("/") + "/"
        )
        if contains and (deepest is None or len(mount_point) > len(deepest[0])):
            deepest = (mount_point, filesystem)
    return deepest


_FILE_SHARE_READ_WRITE = 0x3
_OPEN_EXISTING = 3
_INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value
_IOCTL_DISK_PERFORMANCE = 0x00070020
_IOCTL_STORAGE_GET_DEVICE_NUMBER = 0x002D1080
_IOCTL_SCSI_GET_ADDRESS = 0x00041018
_SYSTEM_PERFORMANCE_INFORMATION = 2
_SYSTEM_PROCESS_INFORMATION = 5
_STATUS_INFO_LENGTH_MISMATCH = 0xC0000004
_DISK_PERFORMANCE_WMI_GUID = uuid.UUID("bdd865d1-d7c1-11d0-a501-00a0c9062910")
_WMIGUID_QUERY = 0x0001


class _Guid(ctypes.Structure):
    _fields_ = [
        ("data1", ctypes.c_uint32),
        ("data2", ctypes.c_uint16),
        ("data3", ctypes.c_uint16),
        ("data4", ctypes.c_uint8 * 8),
    ]


class _DiskPerformance(ctypes.Structure):
    _fields_ = [
        ("bytes_read", ctypes.c_int64),
        ("bytes_written", ctypes.c_int64),
        ("read_time", ctypes.c_int64),
        ("write_time", ctypes.c_int64),
        ("idle_time", ctypes.c_int64),
        ("read_count", ctypes.c_uint32),
        ("write_count", ctypes.c_uint32),
        ("queue_depth", ctypes.c_uint32),
        ("split_count", ctypes.c_uint32),
        ("query_time", ctypes.c_int64),
        ("storage_device_number", ctypes.c_uint32),
        ("storage_manager_name", ctypes.c_uint16 * 8),
    ]


class _StorageDeviceNumber(ctypes.Structure):
    _fields_ = [
        ("device_type", ctypes.c_uint32),
        ("device_number", ctypes.c_uint32),
        ("partition_number", ctypes.c_uint32),
    ]


class _ScsiAddress(ctypes.Structure):
    _fields_ = [
        ("length", ctypes.c_uint32),
        ("port_number", ctypes.c_uint8),
        ("path_id", ctypes.c_uint8),
        ("target_id", ctypes.c_uint8),
        ("lun", ctypes.c_uint8),
    ]


@functools.cache
def _kernel32() -> ctypes.CDLL:
    if sys.platform != "win32":
        raise OSError("the Windows API is unavailable")
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateFileW.argtypes = (
        ctypes.c_wchar_p,
        ctypes.c_uint32,
        ctypes.c_uint32,
        ctypes.c_void_p,
        ctypes.c_uint32,
        ctypes.c_uint32,
        ctypes.c_void_p,
    )
    kernel32.CreateFileW.restype = ctypes.c_void_p
    kernel32.DeviceIoControl.argtypes = (
        ctypes.c_void_p,
        ctypes.c_uint32,
        ctypes.c_void_p,
        ctypes.c_uint32,
        ctypes.c_void_p,
        ctypes.c_uint32,
        ctypes.POINTER(ctypes.c_uint32),
        ctypes.c_void_p,
    )
    kernel32.DeviceIoControl.restype = ctypes.c_int
    kernel32.CloseHandle.argtypes = (ctypes.c_void_p,)
    kernel32.CloseHandle.restype = ctypes.c_int
    kernel32.GetSystemTimes.argtypes = (ctypes.POINTER(ctypes.c_uint64),) * 3
    kernel32.GetSystemTimes.restype = ctypes.c_int
    kernel32.GetVolumePathNameW.argtypes = (
        ctypes.c_wchar_p,
        ctypes.c_wchar_p,
        ctypes.c_uint32,
    )
    kernel32.GetVolumePathNameW.restype = ctypes.c_int
    kernel32.GetVolumeNameForVolumeMountPointW.argtypes = (
        ctypes.c_wchar_p,
        ctypes.c_wchar_p,
        ctypes.c_uint32,
    )
    kernel32.GetVolumeNameForVolumeMountPointW.restype = ctypes.c_int
    kernel32.GetVolumeInformationW.argtypes = (
        ctypes.c_wchar_p,
        ctypes.c_wchar_p,
        ctypes.c_uint32,
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.c_wchar_p,
        ctypes.c_uint32,
    )
    kernel32.GetVolumeInformationW.restype = ctypes.c_int
    kernel32.GlobalMemoryStatusEx.argtypes = (ctypes.c_void_p,)
    kernel32.GlobalMemoryStatusEx.restype = ctypes.c_int
    return kernel32


@functools.cache
def _ntdll() -> ctypes.CDLL:
    if sys.platform != "win32":
        raise OSError("the Windows API is unavailable")
    ntdll = ctypes.WinDLL("ntdll")
    ntdll.NtQuerySystemInformation.argtypes = (
        ctypes.c_uint32,
        ctypes.c_void_p,
        ctypes.c_uint32,
        ctypes.POINTER(ctypes.c_uint32),
    )
    ntdll.NtQuerySystemInformation.restype = ctypes.c_int32
    return ntdll


@functools.cache
def _advapi32() -> ctypes.CDLL:
    if sys.platform != "win32":
        raise OSError("the Windows API is unavailable")
    advapi32 = ctypes.WinDLL("advapi32")
    advapi32.WmiOpenBlock.argtypes = (
        ctypes.POINTER(_Guid),
        ctypes.c_uint32,
        ctypes.POINTER(ctypes.c_void_p),
    )
    advapi32.WmiOpenBlock.restype = ctypes.c_uint32
    advapi32.WmiCloseBlock.argtypes = (ctypes.c_void_p,)
    advapi32.WmiCloseBlock.restype = ctypes.c_uint32
    return advapi32


def _windows_error(code: int | None = None) -> OSError:
    if sys.platform == "win32":
        return ctypes.WinError(ctypes.get_last_error() if code is None else code)
    return OSError("the Windows API is unavailable")


def _enable_disk_performance() -> int:
    """Open the disk performance WMI data block and return its handle.

    Windows collects disk counters only on demand, and an open block is
    such a demand: until it closes, IOCTL_DISK_PERFORMANCE returns counters
    instead of failing. The block grants query access to every account.
    """
    guid = _Guid.from_buffer_copy(_DISK_PERFORMANCE_WMI_GUID.bytes_le)
    handle = ctypes.c_void_p()
    status = _advapi32().WmiOpenBlock(
        ctypes.byref(guid), _WMIGUID_QUERY, ctypes.byref(handle)
    )
    if status != 0 or not handle.value:
        raise _windows_error(status)
    return handle.value


def _open_device(path: str) -> int:
    # Zero access opens a volume or disk for query IOCTLs without privileges.
    handle = _kernel32().CreateFileW(
        path, 0, _FILE_SHARE_READ_WRITE, None, _OPEN_EXISTING, 0, None
    )
    if handle is None or handle == _INVALID_HANDLE_VALUE:
        raise _windows_error()
    return int(handle)


def _close_device(handle: int) -> None:
    _kernel32().CloseHandle(handle)


def _device_io_control(handle: int, code: int, output: ctypes.Structure) -> None:
    returned = ctypes.c_uint32()
    if not _kernel32().DeviceIoControl(
        handle,
        code,
        None,
        0,
        ctypes.byref(output),
        ctypes.sizeof(output),
        ctypes.byref(returned),
        None,
    ):
        raise _windows_error()


def _windows_mount_point(path: Path) -> str:
    buffer = ctypes.create_unicode_buffer(32768)
    if not _kernel32().GetVolumePathNameW(str(path), buffer, len(buffer)):
        raise _windows_error()
    return buffer.value


def _windows_volume_device(mount_point: str) -> str:
    buffer = ctypes.create_unicode_buffer(64)
    if not _kernel32().GetVolumeNameForVolumeMountPointW(
        mount_point, buffer, len(buffer)
    ):
        raise _windows_error()
    # Without its trailing separator, the GUID path opens the volume device
    # rather than its root directory.
    return buffer.value.rstrip("\\")


def _volume_key(mount_point: str) -> str:
    key = mount_point.rstrip("\\/") or mount_point
    # Paths keep the drive-letter case of their spelling.
    return key.upper() if re.fullmatch(r"[A-Za-z]:", key) else key


def _windows_volume_provenance(path: Path) -> dict[str, object]:
    mount_point = _windows_mount_point(path)
    key = _volume_key(mount_point)
    result: dict[str, object] = {"path": str(path), "volume": key}
    filesystem = ctypes.create_unicode_buffer(64)
    if _kernel32().GetVolumeInformationW(
        mount_point, None, 0, None, None, None, filesystem, len(filesystem)
    ):
        result["filesystem"] = filesystem.value
    result["size_bytes"] = shutil.disk_usage(mount_point).total
    system_root = os.environ.get("SystemRoot")
    if system_root:
        system_key = _volume_key(_windows_mount_point(Path(system_root)))
        result["system_volume"] = system_key.casefold() == key.casefold()
    try:
        result.update(_windows_disk_identity(_windows_volume_device(mount_point)))
    except OSError:
        pass
    return result


def _windows_disk_identity(volume_device: str) -> dict[str, object]:
    """Return the disk number and SCSI address behind a single-disk volume,
    which identify the Azure OS disk, data-disk LUN, or temporary disk."""
    number = _StorageDeviceNumber()
    handle = _open_device(volume_device)
    try:
        _device_io_control(handle, _IOCTL_STORAGE_GET_DEVICE_NUMBER, number)
    finally:
        _close_device(handle)
    identity: dict[str, object] = {"disk_number": int(number.device_number)}
    address = _ScsiAddress()
    try:
        handle = _open_device(rf"\\.\PhysicalDrive{number.device_number}")
        try:
            _device_io_control(handle, _IOCTL_SCSI_GET_ADDRESS, address)
        finally:
            _close_device(handle)
    except OSError:
        return identity
    identity["scsi_address"] = (
        f"{address.port_number}:{address.path_id}:{address.target_id}:{address.lun}"
    )
    return identity


# SYSTEM_PERFORMANCE_INFORMATION offsets. winternl.h documents the structure
# only as a 312-byte reserved block; these counters keep the NT kernel's
# layout, which Windows 10 extended with the system cache's dirty pages.
_SYSTEM_PERFORMANCE_COUNTERS = (
    ("available_pages", 44),
    ("modified_pages_written", 96),
    ("modified_page_writes", 100),
    ("mapped_pages_written", 104),
    ("mapped_page_writes", 108),
    ("lazy_write_ios", 280),
    ("lazy_write_pages", 284),
    ("cache_flushes", 288),
    ("cache_flush_pages", 292),
)
_SYSTEM_PERFORMANCE_BASE_SIZE = 312
_SYSTEM_PERFORMANCE_CACHE_COUNTERS = (
    ("cache_dirty_pages", 312),
    ("cache_dirty_page_threshold", 320),
)
_SYSTEM_PERFORMANCE_SIZE = 344

# x64 SYSTEM_PROCESS_INFORMATION offsets from winternl.h. Its first reserved
# block holds the user and kernel times, and its last one the I/O operation
# and transfer counts.
_PROCESS_ENTRY_SIZE = 256
_PROCESS_TIMES_OFFSET = 40
_PROCESS_IMAGE_NAME_OFFSET = 56
_PROCESS_TRANSFERS_OFFSET = 232
_DEFENDER_NAME_LENGTHS = frozenset(len(name) * 2 for name in DEFENDER_PROCESS_NAMES)
_MAX_PROCESS_INFORMATION_BYTES = 64 * MIB


def parse_system_performance(data: bytes) -> dict[str, int]:
    """Return the writeback and memory counters of SystemPerformanceInformation."""
    if len(data) < _SYSTEM_PERFORMANCE_BASE_SIZE:
        raise ValueError(f"system performance information has only {len(data)} bytes")
    counters: dict[str, int] = {
        name: struct.unpack_from("<I", data, offset)[0]
        for name, offset in _SYSTEM_PERFORMANCE_COUNTERS
    }
    if len(data) >= _SYSTEM_PERFORMANCE_SIZE:
        counters.update(
            (name, struct.unpack_from("<Q", data, offset)[0])
            for name, offset in _SYSTEM_PERFORMANCE_CACHE_COUNTERS
        )
    return counters


def parse_defender_processes(data: bytes, base_address: int) -> dict[str, int]:
    """Sum the CPU time and I/O transfers of Microsoft Defender processes in
    an x64 SystemProcessInformation buffer that was filled at
    ``base_address``."""
    totals: dict[str, int] = {
        "processes": 0,
        "cpu_ticks": 0,
        "read_bytes": 0,
        "write_bytes": 0,
        "other_bytes": 0,
    }
    offset = 0
    while True:
        if offset + _PROCESS_ENTRY_SIZE > len(data):
            raise ValueError("process information entry is truncated")
        (next_entry,) = struct.unpack_from("<I", data, offset)
        length, address = struct.unpack_from(
            "<H6xQ", data, offset + _PROCESS_IMAGE_NAME_OFFSET
        )
        if length in _DEFENDER_NAME_LENGTHS and address:
            start = address - base_address
            if start < 0 or start + length > len(data):
                raise ValueError("process image name lies outside the buffer")
            name = data[start : start + length].decode("utf-16-le").casefold()
            if name in DEFENDER_PROCESS_NAMES:
                user, kernel = struct.unpack_from(
                    "<qq", data, offset + _PROCESS_TIMES_OFFSET
                )
                read, write, other = struct.unpack_from(
                    "<3Q", data, offset + _PROCESS_TRANSFERS_OFFSET
                )
                totals["processes"] += 1
                totals["cpu_ticks"] += user + kernel
                totals["read_bytes"] += read
                totals["write_bytes"] += write
                totals["other_bytes"] += other
        if next_entry == 0:
            return totals
        offset += next_entry


class _BufferTooSmallError(OSError):
    def __init__(self, required: int) -> None:
        super().__init__(f"system information requires {required} bytes")
        self.required = required


def _query_system_information(
    information_class: int, buffer: ctypes.Array[ctypes.c_char]
) -> bytes:
    returned = ctypes.c_uint32()
    status = _ntdll().NtQuerySystemInformation(
        information_class, buffer, len(buffer), ctypes.byref(returned)
    )
    if status & 0xFFFFFFFF == _STATUS_INFO_LENGTH_MISMATCH:
        raise _BufferTooSmallError(returned.value)
    if status != 0:
        raise OSError(
            f"NtQuerySystemInformation({information_class}) failed with status "
            f"0x{status & 0xFFFFFFFF:08X}"
        )
    return ctypes.string_at(buffer, returned.value)


def _volume_counters(handle: int) -> dict[str, int]:
    counters = _DiskPerformance()
    _device_io_control(handle, _IOCTL_DISK_PERFORMANCE, counters)
    return {
        "bytes_read": counters.bytes_read,
        "bytes_written": counters.bytes_written,
        "read_ticks": counters.read_time,
        "write_ticks": counters.write_time,
        "idle_ticks": counters.idle_time,
        "reads": counters.read_count,
        "writes": counters.write_count,
        "queue_depth": counters.queue_depth,
        "query_ticks": counters.query_time,
    }


def _cpu_counters() -> dict[str, int]:
    idle, kernel, user = ctypes.c_uint64(), ctypes.c_uint64(), ctypes.c_uint64()
    if not _kernel32().GetSystemTimes(
        ctypes.byref(idle), ctypes.byref(kernel), ctypes.byref(user)
    ):
        raise _windows_error()
    return {
        "idle_ticks": idle.value,
        "kernel_ticks": kernel.value,
        "user_ticks": user.value,
    }


class CounterReader(Protocol):
    def description(self) -> dict[str, object]:
        """Return the reader's source, units, and monitored volumes."""
        ...

    def read(self, *, processes: bool) -> tuple[dict[str, object], list[str]]:
        """Return one sample of cumulative counters and any read errors.

        ``processes`` adds the costlier per-process counters.
        """
        ...

    def close(self) -> None: ...


class WindowsCounterReader:
    """Read volume, CPU, writeback, and Defender counters without privileges."""

    def __init__(self, volumes: Mapping[str, Path]) -> None:
        self._handles: dict[str, int] = {}
        self._roles: dict[str, list[str]] = {}
        self._errors: list[str] = []
        self._system_buffer = ctypes.create_string_buffer(_SYSTEM_PERFORMANCE_SIZE)
        self._process_buffer = ctypes.create_string_buffer(MIB)
        self._disk_performance: int | None = None
        try:
            self._disk_performance = _enable_disk_performance()
        except OSError as error:
            self._errors.append(f"disk counters could not be enabled: {error}")
        keys: dict[str, str] = {}
        for role, path in volumes.items():
            try:
                mount_point = _windows_mount_point(path)
                device = _windows_volume_device(mount_point)
                if device not in keys:
                    handle = _open_device(device)
                    keys[device] = _volume_key(mount_point)
                    self._handles[keys[device]] = handle
            except OSError as error:
                self._errors.append(f"{role} volume counters are unavailable: {error}")
                continue
            self._roles.setdefault(keys[device], []).append(role)

    def description(self) -> dict[str, object]:
        return {
            "source": "windows",
            "tick_ns": 100,
            "page_size_bytes": mmap.PAGESIZE,
            "volumes": {key: list(roles) for key, roles in self._roles.items()},
        }

    def read(self, *, processes: bool) -> tuple[dict[str, object], list[str]]:
        errors = list(self._errors)
        volumes: dict[str, object] = {}
        for key, handle in self._handles.items():
            try:
                volumes[key] = _volume_counters(handle)
            except OSError as error:
                errors.append(f"{key} volume counters failed: {error}")
        sample: dict[str, object] = {"volumes": volumes}
        readers = [("cpu", _cpu_counters), ("memory", self._memory_counters)]
        if processes:
            readers.append(("defender", self._defender_counters))
        for name, read in readers:
            try:
                sample[name] = read()
            except (OSError, ValueError, struct.error) as error:
                errors.append(f"{name} counters failed: {error}")
        return sample, errors

    def close(self) -> None:
        for handle in self._handles.values():
            _close_device(handle)
        self._handles.clear()
        if self._disk_performance is not None:
            _advapi32().WmiCloseBlock(self._disk_performance)
            self._disk_performance = None

    def _memory_counters(self) -> dict[str, int]:
        try:
            data = _query_system_information(
                _SYSTEM_PERFORMANCE_INFORMATION, self._system_buffer
            )
        except _BufferTooSmallError as error:
            # Windows before 10 accepts only its shorter structure.
            self._system_buffer = ctypes.create_string_buffer(error.required)
            data = _query_system_information(
                _SYSTEM_PERFORMANCE_INFORMATION, self._system_buffer
            )
        return parse_system_performance(data)

    def _defender_counters(self) -> dict[str, int]:
        while True:
            try:
                data = _query_system_information(
                    _SYSTEM_PROCESS_INFORMATION, self._process_buffer
                )
            except _BufferTooSmallError as error:
                if error.required > _MAX_PROCESS_INFORMATION_BYTES:
                    raise
                # Leave room for processes that start before the next query.
                size = error.required + MIB // 4
                self._process_buffer = ctypes.create_string_buffer(size)
                continue
            return parse_defender_processes(
                data, ctypes.addressof(self._process_buffer)
            )


class StorageTelemetry:
    """Sample host counters on a fixed cadence between start() and stop().

    Sample times are nanoseconds after ``origin_ns`` on the
    ``time.perf_counter_ns`` clock, the base of the snapshot profile's
    ``observer_elapsed_ns``. Sampling never raises; failures are recorded.
    """

    def __init__(
        self,
        reader: CounterReader,
        origin_ns: int,
        *,
        interval_seconds: float = STORAGE_TELEMETRY_INTERVAL_SECONDS,
        process_interval_seconds: float = STORAGE_TELEMETRY_PROCESS_INTERVAL_SECONDS,
        max_samples: int = STORAGE_TELEMETRY_MAX_SAMPLES,
        clock: Callable[[], int] = time.perf_counter_ns,
    ) -> None:
        self._reader = reader
        self._origin_ns = origin_ns
        self._clock = clock
        self._interval_seconds = interval_seconds
        self._process_interval_ns = int(process_interval_seconds * 1e9)
        self._max_samples = max_samples
        self._samples: list[dict[str, object]] = []
        self._errors: list[str] = []
        self._truncated = False
        self._last_process_sample_ns: int | None = None
        self._stop_requested = threading.Event()
        self._thread: threading.Thread | None = None
        self._stopped = False

    def start(self) -> None:
        if self._thread is not None or self._stopped:
            return
        self._sample(final=False)
        self._thread = threading.Thread(
            target=self._run, name="nvx-storage-telemetry", daemon=True
        )
        self._thread.start()

    def stop(self) -> None:
        if self._stopped:
            return
        self._stopped = True
        if self._thread is not None:
            self._stop_requested.set()
            self._thread.join()
            self._sample(final=True)
        self._reader.close()

    def result(self, windows: Mapping[str, tuple[int, int]]) -> dict[str, object]:
        """Stop sampling and summarize each telemetry phase in ``windows``.

        ``windows`` maps a profile phase to its start and end on the sample
        clock.
        """
        self.stop()
        description = self._reader.description()
        page_size = description.get("page_size_bytes")
        phases: dict[str, object] = {}
        for phase in STORAGE_TELEMETRY_PHASES:
            if phase not in windows:
                continue
            summary = summarize_storage_window(
                self._samples,
                *windows[phase],
                page_size=page_size if isinstance(page_size, int) else mmap.PAGESIZE,
            )
            if summary is not None:
                phases[phase] = summary
        return {
            "schema_version": STORAGE_TELEMETRY_SCHEMA_VERSION,
            **description,
            "interval_ms": self._interval_seconds * 1000,
            "process_interval_ms": self._process_interval_ns / 1e6,
            "samples": self._samples,
            "truncated": self._truncated,
            "errors": self._errors,
            "phases": phases,
        }

    def _run(self) -> None:
        while not self._stop_requested.wait(self._interval_seconds):
            if not self._sample(final=False):
                return

    def _sample(self, *, final: bool) -> bool:
        # The final sample always lands, so phases that end before the source
        # exits stay bracketed after a long capture truncates sampling.
        if not final and len(self._samples) >= self._max_samples:
            self._truncated = True
            return False
        elapsed_ns = self._clock() - self._origin_ns
        processes = (
            final
            or self._last_process_sample_ns is None
            or elapsed_ns - self._last_process_sample_ns >= self._process_interval_ns
        )
        if processes:
            self._last_process_sample_ns = elapsed_ns
        try:
            counters, errors = self._reader.read(processes=processes)
        except Exception as error:
            counters, errors = {}, [f"counter sampling failed: {error}"]
        for error in errors:
            if error not in self._errors:
                self._errors.append(error)
        self._samples.append({"elapsed_ns": elapsed_ns, **counters})
        return True


def create_storage_telemetry(
    volumes: Mapping[str, Path], origin_ns: int
) -> StorageTelemetry | None:
    """Return a sampler for ``volumes``, or None where it is unsupported.

    Only Windows captures flush a file-backed guest RAM image, so other hosts
    record no storage telemetry.
    """
    if sys.platform != "win32" or ctypes.sizeof(ctypes.c_void_p) != 8:
        return None
    return StorageTelemetry(WindowsCounterReader(volumes), origin_ns)


class _Endpoints(NamedTuple):
    """The samples nearest to a phase's boundaries that carry one counter
    group, and every sample of the group between them."""

    start_ns: int
    end_ns: int
    before: dict[str, object]
    after: dict[str, object]
    covered: list[dict[str, object]]

    @property
    def seconds(self) -> float:
        return (self.end_ns - self.start_ns) / 1e9


def summarize_storage_window(
    samples: Sequence[Mapping[str, object]],
    start_ns: int,
    end_ns: int,
    *,
    page_size: int = mmap.PAGESIZE,
) -> dict[str, object] | None:
    """Summarize the counters over the phase from ``start_ns`` to ``end_ns``.

    Each counter group uses the samples nearest to the phase's start and end
    that carry it, so a failed read does not hide what neighboring samples
    recorded. Rates and averages span those samples, and gauges report their
    extreme sample between them. The summary reports the interval between the
    samples nearest to the phase, and a group whose samples differ, such as
    Defender's sparser process counters, reports its own. Returns None unless
    two distinct samples bound the phase.
    """
    timed = [
        (elapsed, _string_keys(sample))
        for sample in samples
        if (elapsed := _counter(sample, "elapsed_ns")) is not None
    ]
    overall = _endpoints(timed, start_ns, end_ns, lambda _: True)
    if overall is None:
        return None

    def bounded(
        summary: dict[str, object] | None, endpoints: _Endpoints
    ) -> dict[str, object] | None:
        if summary is not None and (endpoints.start_ns, endpoints.end_ns) != (
            overall.start_ns,
            overall.end_ns,
        ):
            summary["sampled_start_ns"] = endpoints.start_ns
            summary["sampled_end_ns"] = endpoints.end_ns
        return summary

    def memory(endpoints: _Endpoints) -> dict[str, object] | None:
        return _memory_window(endpoints, page_size)

    volumes: dict[str, object] = {}
    keys = dict.fromkeys(
        key for _, sample in timed for key in _group(sample, "volumes")
    )
    for key in keys:
        endpoints = _endpoints(
            timed,
            start_ns,
            end_ns,
            lambda sample, key=key: key in _group(sample, "volumes"),
        )
        if endpoints is not None:
            summary = bounded(_volume_window(key, endpoints), endpoints)
            if summary is not None:
                volumes[key] = summary
    summarizers: tuple[
        tuple[str, Callable[[_Endpoints], dict[str, object] | None]], ...
    ] = (("cpu", _cpu_window), ("memory", memory), ("defender", _defender_window))
    groups: dict[str, object] = {}
    for name, summarize in summarizers:
        endpoints = _endpoints(
            timed, start_ns, end_ns, lambda sample, name=name: name in sample
        )
        groups[name] = (
            None if endpoints is None else bounded(summarize(endpoints), endpoints)
        )
    return {
        "start_ns": start_ns,
        "end_ns": end_ns,
        "sampled_start_ns": overall.start_ns,
        "sampled_end_ns": overall.end_ns,
        "volumes": volumes,
        **groups,
    }


def _endpoints(
    timed: Sequence[tuple[int, dict[str, object]]],
    start_ns: int,
    end_ns: int,
    carries: Callable[[dict[str, object]], bool],
) -> _Endpoints | None:
    candidates = [(elapsed, sample) for elapsed, sample in timed if carries(sample)]
    bounds = _nearest_samples([elapsed for elapsed, _ in candidates], start_ns, end_ns)
    if bounds is None:
        return None
    first, last = bounds
    return _Endpoints(
        candidates[first][0],
        candidates[last][0],
        candidates[first][1],
        candidates[last][1],
        [sample for _, sample in candidates[first : last + 1]],
    )


def _nearest_samples(
    times: Sequence[int], start_ns: int, end_ns: int
) -> tuple[int, int] | None:
    if not times:
        return None
    first = min(range(len(times)), key=lambda index: abs(times[index] - start_ns))
    last = min(range(len(times)), key=lambda index: abs(times[index] - end_ns))
    if times[last] <= times[first]:
        return None
    return first, last


def _group(sample: Mapping[str, object], name: str) -> dict[str, object]:
    return _string_keys(sample.get(name))


def _counter(group: Mapping[str, object], name: str) -> int | None:
    value = group.get(name)
    if isinstance(value, bool) or not isinstance(value, int):
        return None
    return value


def _increases(
    before: Mapping[str, object],
    after: Mapping[str, object],
    names: Sequence[str],
    *,
    modulus: int | None = None,
) -> tuple[int, ...] | None:
    """Return each counter's increase, or None if any is missing or fell."""
    increases: list[int] = []
    for name in names:
        start, end = _counter(before, name), _counter(after, name)
        if start is None or end is None:
            return None
        increase = end - start if modulus is None else (end - start) % modulus
        if increase < 0:
            return None
        increases.append(increase)
    return tuple(increases)


def _rounded(value: float) -> float:
    return round(value, 3)


def _percent(value: float) -> float:
    return _rounded(min(100.0, max(0.0, value)))


def _volume_window(key: str, endpoints: _Endpoints) -> dict[str, object] | None:
    first = _string_keys(_group(endpoints.before, "volumes").get(key))
    last = _string_keys(_group(endpoints.after, "volumes").get(key))
    totals = _increases(
        first,
        last,
        (
            "bytes_read",
            "bytes_written",
            "read_ticks",
            "write_ticks",
            "idle_ticks",
            "query_ticks",
        ),
    )
    # DISK_PERFORMANCE counts requests in 32-bit fields that wrap.
    counts = _increases(first, last, ("reads", "writes"), modulus=1 << 32)
    if totals is None or counts is None or totals[5] <= 0:
        return None
    read_bytes, write_bytes, read_ticks, write_ticks, idle_ticks, query = totals
    reads, writes = counts
    seconds = endpoints.seconds
    depths = [
        depth
        for sample in endpoints.covered
        if (
            depth := _counter(
                _string_keys(_group(sample, "volumes").get(key)), "queue_depth"
            )
        )
        is not None
    ]
    return {
        "read_mib_per_second": _rounded(read_bytes / MIB / seconds),
        "write_mib_per_second": _rounded(write_bytes / MIB / seconds),
        "read_iops": _rounded(reads / seconds),
        "write_iops": _rounded(writes / seconds),
        "average_read_latency_ms": (
            _rounded(read_ticks / reads / 10_000) if reads else None
        ),
        "average_write_latency_ms": (
            _rounded(write_ticks / writes / 10_000) if writes else None
        ),
        "average_queue_length": _rounded((read_ticks + write_ticks) / query),
        "busy_percent": _percent(100 * (1 - idle_ticks / query)),
        "max_queue_depth": max(depths, default=None),
    }


def _cpu_window(endpoints: _Endpoints) -> dict[str, object] | None:
    times = _increases(
        _group(endpoints.before, "cpu"),
        _group(endpoints.after, "cpu"),
        ("idle_ticks", "kernel_ticks", "user_ticks"),
    )
    if times is None:
        return None
    idle, kernel, user = times
    # Windows includes idle time in kernel time.
    if kernel + user <= 0:
        return None
    return {"busy_percent": _percent(100 * (1 - idle / (kernel + user)))}


def _memory_window(endpoints: _Endpoints, page_size: int) -> dict[str, object] | None:
    start, end = _group(endpoints.before, "memory"), _group(endpoints.after, "memory")
    seconds = endpoints.seconds

    def mib(pages: int) -> float:
        return _rounded(pages * page_size / MIB)

    def rate(name: str) -> float | None:
        pages = _increases(start, end, (name,), modulus=1 << 32)
        return None if pages is None else _rounded(mib(pages[0]) / seconds)

    def extreme(name: str, largest: bool) -> float | None:
        values = [
            value
            for sample in endpoints.covered
            if (value := _counter(_group(sample, "memory"), name)) is not None
        ]
        if not values:
            return None
        return mib(max(values) if largest else min(values))

    threshold = _counter(end, "cache_dirty_page_threshold")
    return {
        "available_mib_min": extreme("available_pages", largest=False),
        "cache_dirty_mib_max": extreme("cache_dirty_pages", largest=True),
        "cache_dirty_threshold_mib": None if threshold is None else mib(threshold),
        "lazy_write_mib_per_second": rate("lazy_write_pages"),
        "cache_flush_mib_per_second": rate("cache_flush_pages"),
        "modified_write_mib_per_second": rate("modified_pages_written"),
        "mapped_write_mib_per_second": rate("mapped_pages_written"),
    }


def _defender_window(endpoints: _Endpoints) -> dict[str, object] | None:
    start = _group(endpoints.before, "defender")
    end = _group(endpoints.after, "defender")
    processes = _counter(end, "processes")
    # A Defender process that started or exited would skew the summed totals.
    if processes is None or processes != _counter(start, "processes"):
        return None
    totals = _increases(
        start, end, ("cpu_ticks", "read_bytes", "write_bytes", "other_bytes")
    )
    if totals is None:
        return None
    cpu_ticks, read_bytes, write_bytes, other_bytes = totals
    seconds = endpoints.seconds
    return {
        "processes": processes,
        "cpu_percent": _rounded(100 * cpu_ticks / TICKS_PER_SECOND / seconds),
        "read_mib_per_second": _rounded(read_bytes / MIB / seconds),
        "write_mib_per_second": _rounded(write_bytes / MIB / seconds),
        "other_mib_per_second": _rounded(other_bytes / MIB / seconds),
    }


def describe_storage_window(
    window: Mapping[str, object], volume_roles: Mapping[str, object]
) -> str:
    """Return a one-line description of a summarized telemetry window."""

    def number(value: object, unit: str, digits: int = 1) -> str:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return "n/a"
        return f"{value:.{digits}f}{unit}"

    def roles(key: str) -> list[str]:
        value = volume_roles.get(key)
        if not isinstance(value, list):
            return []
        return [str(role) for role in cast(list[object], value)]

    parts: list[str] = []
    volumes = _group(window, "volumes")
    for key in sorted(volumes, key=lambda key: "scratch" not in roles(key)):
        stats = _string_keys(volumes[key])
        label = "/".join(roles(key)) or "volume"
        parts.append(
            f"{label} {key} write "
            f"{number(stats.get('write_mib_per_second'), ' MiB/s')} at "
            f"{number(stats.get('average_write_latency_ms'), ' ms')}, queue "
            f"{number(stats.get('average_queue_length'), '')}, "
            f"{number(stats.get('busy_percent'), '%', 0)} busy"
        )
    parts.append(
        f"CPU {number(_group(window, 'cpu').get('busy_percent'), '%', 0)} busy"
    )
    memory = _group(window, "memory")
    if memory:
        parts.append(
            f"cache dirty {number(memory.get('cache_dirty_mib_max'), ' MiB')}, "
            f"lazy writes {number(memory.get('lazy_write_mib_per_second'), ' MiB/s')}"
        )
    defender = _group(window, "defender")
    if defender:
        parts.append(f"Defender {number(defender.get('cpu_percent'), '%')} CPU")
    return "; ".join(parts)

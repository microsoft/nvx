"""Prepare the Azure managed data disk of a Windows runner.

Existing filesystems are never formatted. A RAW disk must pass a full-device
zero scan before initialization, and only a partition that this helper's
on-disk marker owns is formatted. When the Azure temporary disk holds the
requested letter and the pagefile, setup moves the pagefile, records its
progress, and reboots once; the runner bootstrap task resumes it at startup.
"""

import re
from pathlib import Path
from typing import cast

from . import common, windows_system
from .common import BootstrapError, JsonObject, Log

INIT_MARKER_TYPE = "{8f8f0f40-5c84-4a2a-8c22-8fbff2e8bc91}"
BASIC_DATA_TYPE = "{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}"
MARKER_SIZE = 16 * 1024 * 1024
PENDING_KEY = r"SOFTWARE\NvxRunner\DataDiskSetup"
MEMORY_KEY = r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management"
LOG_PATH = Path(r"C:\WindowsAzure\Logs\setup-datadisk.log")
DISCOVERY_ATTEMPTS = 12
DISCOVERY_DELAY_SECONDS = 5
FALLBACK_LETTERS = ("T", "U", "V", "W", "X", "Y", "Z")
MAXIMUM_DISK_BYTES = 2**63 - 1
# The mutex of the earlier PowerShell disk setup, and its wait for it.
LEGACY_MUTEX = r"Global\NvxRunner-SetupDataDisk"
SETUP_TIMEOUT_SECONDS = 4 * 60 * 60


def storage(operation: str, request: JsonObject | None = None) -> object:
    return windows_system.adapter("storage.ps1", operation, request)


def required_result(value: object) -> JsonObject:
    return common.json_object(value, "The storage result")


def optional_result(value: object) -> JsonObject | None:
    return None if value is None else common.json_object(value, "The storage result")


def results(value: object) -> list[JsonObject]:
    if not isinstance(value, list):
        raise BootstrapError("The storage adapter returned no list")
    return [
        common.json_object(item, "The storage result")
        for item in cast(list[object], value)
    ]


def number(disk: JsonObject) -> int:
    return common.integer(disk, "number", "disk description")


def disk_address(disk: JsonObject) -> tuple[int, int, int] | None:
    """Return the adapter, target, and LUN in a SCSI disk's location."""
    match = re.search(
        r"Adapter\s*(\d+).*Target\s*(\d+)\s*:\s*LUN\s*(\d+)",
        str(disk.get("location") or ""),
        re.I,
    )
    if match is None:
        return None
    return int(match[1]), int(match[2]), int(match[3])


def _names(disk: JsonObject) -> str:
    return f"{disk.get('friendlyName') or ''} {disk.get('model') or ''}"


def is_local_nvme(disk: JsonObject) -> bool:
    return re.search("Microsoft NVMe Direct Disk", _names(disk), re.I) is not None


def is_managed_nvme(disk: JsonObject) -> bool:
    pattern = "MSFT NVMe Accelerator|Virtual_Disk NVMe Premium"
    return re.search(pattern, _names(disk), re.I) is not None


def managed_nvme_lun(disk: JsonObject, guest_disks: list[JsonObject]) -> int | None:
    """Map a managed NVMe disk to its Azure LUN through its namespace.

    Azure exposes data-disk LUN n as NVMe namespace, or SCSI logical unit,
    n + 1. Unknown or ambiguous mappings select nothing.
    """
    device = f"\\\\.\\PHYSICALDRIVE{number(disk)}".lower()
    matching = [
        guest
        for guest in guest_disks
        if guest.get("index") == number(disk)
        and str(guest.get("deviceId") or "").lower() == device
    ]
    if len(matching) != 1:
        return None
    namespace = matching[0].get("scsiLogicalUnit")
    if not isinstance(namespace, int) or isinstance(namespace, bool):
        return None
    if not 1 <= namespace <= 64:
        return None
    return namespace - 1


def select_managed_disk(
    disks: list[JsonObject], guest_disks: list[JsonObject], lun: int
) -> JsonObject | None:
    """Return the only data disk at the LUN, or None when that is not certain."""
    candidates = [
        disk for disk in disks if not disk.get("isBoot") and not disk.get("isSystem")
    ]
    scsi: list[JsonObject] = []
    nvme: list[JsonObject] = []
    for disk in candidates:
        if is_managed_nvme(disk):
            if managed_nvme_lun(disk, guest_disks) == lun:
                nvme.append(disk)
            continue
        address = disk_address(disk)
        if address is not None and address[0] >= 1 and address[1:] == (0, lun):
            scsi.append(disk)
    if len(scsi) == 1 and not nvme:
        return scsi[0]
    if not scsi and len(nvme) == 1:
        return nvme[0]
    return None


def free_letter(preferred: str, used: list[str], data_letter: str) -> str:
    for letter in (preferred, *FALLBACK_LETTERS):
        if letter != data_letter and letter not in used:
            return letter
    raise BootstrapError(
        "No free fallback drive letter is available for the Azure temporary disk."
    )


def configured_pagefile(letter: str) -> bool:
    value = windows_system.registry_value(MEMORY_KEY, "PagingFiles")
    entries = (
        cast(list[object], value)
        if isinstance(value, list)
        else ([value] if isinstance(value, str) else [])
    )
    return any(
        isinstance(entry, str) and re.match(rf"{letter}:\\", entry, re.I)
        for entry in entries
    )


def active_pagefile(letter: str) -> bool:
    names = storage("ActivePagefiles")
    if not isinstance(names, list):
        raise BootstrapError("Windows reported no pagefile list")
    prefix = f"{letter}:\\".lower()
    return any(
        str(name).lower().startswith(prefix) for name in cast(list[object], names)
    )


def physical_drive(disk_number: int) -> str:
    return f"\\\\.\\PhysicalDrive{disk_number}"


def disk_is_zero(disk: JsonObject) -> bool:
    """Scan the whole raw disk in bounded chunks."""
    size = disk.get("size")
    if (
        not isinstance(size, int)
        or isinstance(size, bool)
        or not 0 < size <= MAXIMUM_DISK_BYTES
    ):
        raise BootstrapError(
            f"Invalid disk size for disk {disk.get('number')}; refusing to initialize it."
        )
    disk_number = number(disk)
    drive = physical_drive(disk_number)
    try:
        with open(drive, "rb", buffering=0) as stream:
            return common.zero_filled(stream, size)
    except OSError as error:
        raise BootstrapError(
            f"could not read managed data disk {disk_number} at {drive}: {error}"
        ) from error


def discover(lun: int, log: Log) -> JsonObject:
    disks: list[JsonObject] = []
    guest_disks: list[JsonObject] = []
    selected: JsonObject | None = None
    for attempt in range(DISCOVERY_ATTEMPTS):
        found = common.json_object(storage("Disks"), "The disk inventory")
        disks = results(found.get("disks"))
        guest_disks = results(found.get("guestDisks"))
        selected = select_managed_disk(disks, guest_disks, lun)
        if selected is not None:
            break
        if attempt + 1 < DISCOVERY_ATTEMPTS:
            common.sleep(DISCOVERY_DELAY_SECONDS)
    for disk in disks:
        nvme_lun = (
            managed_nvme_lun(disk, guest_disks) if is_managed_nvme(disk) else None
        )
        log(
            f"Disk {disk.get('number')}: Boot={disk.get('isBoot')} "
            f"System={disk.get('isSystem')} Style={disk.get('partitionStyle')} "
            f"Bus={disk.get('busType')} FriendlyName='{disk.get('friendlyName')}' "
            f"Model='{disk.get('model')}' Location='{disk.get('location')}' "
            f"Address={disk_address(disk)} AzureNvmeLun={nvme_lun}"
        )
    if selected is None:
        raise BootstrapError(
            f"could not uniquely identify managed data disk LUN {lun}."
        )
    log(f"Selected managed data disk {number(selected)}.")
    return selected


def is_temporary_disk(disk: JsonObject, partition: JsonObject) -> bool:
    if is_local_nvme(disk) or disk_address(disk) == (0, 0, 1):
        return True
    volume = optional_result(
        storage(
            "Volume",
            {
                "diskNumber": partition.get("diskNumber"),
                "partitionNumber": partition.get("partitionNumber"),
            },
        )
    )
    return volume is not None and volume.get("fileSystemLabel") == "Temporary Storage"


def release_letter(data_disk: JsonObject, letter: str, fallback: str, log: Log) -> bool:
    """Move the Azure temporary disk off the data letter.

    Returns False when a pagefile on it requires the scheduled reboot first.
    """
    conflict = optional_result(storage("PartitionAtLetter", {"letter": letter}))
    if conflict is None or conflict.get("diskNumber") == number(data_disk):
        return True
    conflict_disk = required_result(
        storage("Disk", {"number": conflict.get("diskNumber")})
    )
    if not is_temporary_disk(conflict_disk, conflict):
        raise BootstrapError(
            f"{letter}: belongs to disk {number(conflict_disk)}, which is not "
            "positively identified as Azure temporary storage."
        )
    recorded = windows_system.registry_value(PENDING_KEY, "TempDriveLetter")
    if isinstance(recorded, str) and recorded.strip():
        target = recorded.strip()
    else:
        used = storage("UsedLetters")
        target = free_letter(
            fallback,
            [str(item) for item in cast(list[object], used)]
            if isinstance(used, list)
            else [],
            letter,
        )
    if active_pagefile(letter) or configured_pagefile(letter):
        log(
            f"Azure temporary disk at {letter}: hosts the active pagefile. Moving the "
            "pagefile to C: and scheduling post-reboot continuation."
        )
        # Invalidate completion first: a later failure must not leave a stale
        # marker that lets the runner bootstrap continue.
        windows_system.set_registry_value(PENDING_KEY, "Completed", 0)
        if not windows_system.task_exists():
            raise BootstrapError(
                "Runner continuation task is missing; refusing to move the pagefile."
            )
        current_boot = windows_system.boot_time()
        requested = windows_system.registry_value(
            PENDING_KEY, "RebootRequestedFromBoot"
        )
        if requested and requested != current_boot:
            raise BootstrapError(
                f"the {letter}: pagefile conflict persists after the continuation reboot."
            )
        windows_system.set_registry_value(
            PENDING_KEY, "TempDiskNumber", number(conflict_disk)
        )
        windows_system.set_registry_value(PENDING_KEY, "TempDriveLetter", target)
        windows_system.set_registry_value(
            PENDING_KEY, "RebootRequestedFromBoot", current_boot
        )
        windows_system.set_registry_value(
            MEMORY_KEY, "PagingFiles", [r"C:\pagefile.sys 0 0"]
        )
        windows_system.request_reboot(
            "Completing persistent Azure data-disk setup after moving the pagefile.",
            log,
        )
        log("Reboot scheduled; setup will continue automatically at startup.")
        return False
    log(f"Moving Azure temporary disk from {letter}: to {target}:.")
    storage(
        "SetPartitionLetter",
        {
            "diskNumber": conflict.get("diskNumber"),
            "partitionNumber": conflict.get("partitionNumber"),
            "letter": target,
        },
    )
    for attempt in range(20):
        if storage("PartitionAtLetter", {"letter": letter}) is None:
            break
        if attempt + 1 < 20:
            common.sleep(0.5)
    else:
        raise BootstrapError(f"{letter}: was not released by the temporary disk.")
    moved = optional_result(storage("PartitionAtLetter", {"letter": target}))
    if moved is None or moved.get("diskNumber") != number(conflict_disk):
        raise BootstrapError(f"temporary disk was not verified at {target}:.")
    return True


def is_unformatted(volume: JsonObject | None) -> bool:
    return (
        volume is None
        or volume.get("fileSystemType") == "Unknown"
        or not str(volume.get("fileSystem") or "").strip()
        or volume.get("fileSystem") == "RAW"
    )


def prepare_volume(
    disk: JsonObject, log: Log
) -> tuple[JsonObject, JsonObject, list[JsonObject]]:
    """Return the data partition, its NTFS volume, and the owned marker partitions.

    Only a verified-zero RAW disk is initialized, a marker is created only on
    a disk that this invocation initialized, and only a marked partition is
    formatted.
    """
    disk_number = number(disk)
    partitions = results(storage("Partitions", {"diskNumber": disk_number}))
    markers = [
        partition
        for partition in partitions
        if str(partition.get("gptType")).lower() == INIT_MARKER_TYPE
    ]
    data = [
        partition
        for partition in partitions
        if partition.get("type") != "Reserved"
        and str(partition.get("gptType")).lower() != INIT_MARKER_TYPE
    ]
    if len(markers) > 1 or len(data) > 1:
        raise BootstrapError(
            "unexpected partition layout on the managed data disk; refusing to "
            "choose or modify a partition."
        )
    initialized = False
    if disk.get("partitionStyle") == "RAW":
        log(
            f"Verifying the full contents of RAW disk {disk_number} before initialization."
        )
        if not disk_is_zero(disk):
            raise BootstrapError(
                "RAW disk contains nonzero data; refusing to initialize it."
            )
        log("Initializing truly RAW managed data disk as GPT.")
        disk = required_result(storage("InitializeGpt", {"number": disk_number}))
        initialized = True
        markers = []
        data = []
    if disk.get("partitionStyle") != "GPT":
        raise BootstrapError(
            f"existing disk uses partition style '{disk.get('partitionStyle')}', "
            "not GPT; preserving it."
        )
    if not data:
        if not markers:
            if not initialized:
                raise BootstrapError(
                    "existing GPT disk has no owned initialization marker; preserving "
                    "it without creating partitions."
                )
            log("Creating persistent on-disk initialization marker.")
            markers = [
                required_result(
                    storage(
                        "NewPartition",
                        {
                            "diskNumber": disk_number,
                            "size": MARKER_SIZE,
                            "gptType": INIT_MARKER_TYPE,
                        },
                    )
                )
            ]
        log("Creating the data partition.")
        data = [
            required_result(
                storage(
                    "NewPartition",
                    {"diskNumber": disk_number, "gptType": BASIC_DATA_TYPE},
                )
            )
        ]
    partition = data[0]
    request: JsonObject = {
        "diskNumber": disk_number,
        "partitionNumber": partition.get("partitionNumber"),
    }
    volume = optional_result(storage("Volume", request))
    if is_unformatted(volume) or markers:
        if len(markers) != 1:
            raise BootstrapError(
                "filesystem-less partition lacks the on-disk initialization marker; "
                "refusing to format it."
            )
        log(
            f"Formatting script-created partition {request['partitionNumber']} as NTFS."
        )
        volume = optional_result(storage("FormatNtfs", request))
    else:
        assert volume is not None
        log(
            f"Existing filesystem '{volume.get('fileSystem')}' found; preserving all data."
        )
    if volume is None or volume.get("fileSystem") != "NTFS":
        found = None if volume is None else volume.get("fileSystem")
        raise BootstrapError(
            f"existing filesystem '{found}' is not NTFS; preserving it but refusing "
            "to use it as the Windows data volume."
        )
    return partition, volume, markers


def setup(lun: int, letter: str, fallback: str, log: Log) -> bool:
    """Prepare and mount the data disk; False means that a reboot is pending."""
    letter = letter.rstrip(":").upper()
    fallback = fallback.rstrip(":").upper()
    if not 0 <= lun <= 63:
        raise BootstrapError("Data disk LUN must be between 0 and 63.")
    if not re.fullmatch("[A-Z]", letter) or not re.fullmatch("[A-Z]", fallback):
        raise BootstrapError("Drive-letter parameters must each contain one letter.")
    log(f"=== setup-datadisk starting (lun={lun}, letter={letter}) ===")
    # Earlier bootstraps prepared the disk from a task of their own, which
    # holds this mutex while it runs: unregistering that task does not stop
    # a run that already started, so wait for it to finish.
    with windows_system.named_mutex(LEGACY_MUTEX, SETUP_TIMEOUT_SECONDS):
        return _setup(lun, letter, fallback, log)


def _setup(lun: int, letter: str, fallback: str, log: Log) -> bool:
    disk = discover(lun, log)
    if not release_letter(disk, letter, fallback, log):
        return False
    if disk.get("isOffline") or disk.get("isReadOnly"):
        disk = required_result(
            storage(
                "SetDisk",
                {
                    "number": number(disk),
                    "online": bool(disk.get("isOffline")),
                    "writable": bool(disk.get("isReadOnly")),
                },
            )
        )
    partition, _, markers = prepare_volume(disk, log)
    if markers:
        log("Initialization completed; removing the on-disk marker partition.")
        offset = partition.get("offset")
        storage(
            "RemovePartition",
            {
                "diskNumber": number(disk),
                "partitionNumber": markers[0].get("partitionNumber"),
            },
        )
        remaining = results(storage("Partitions", {"diskNumber": number(disk)}))
        matching = [item for item in remaining if item.get("offset") == offset]
        if not matching:
            raise BootstrapError(
                "data partition could not be found after removing the initialization marker."
            )
        partition = matching[0]
    if storage("VolumeAtLetter", {"letter": letter}) is not None:
        holder = optional_result(storage("PartitionAtLetter", {"letter": letter}))
        if (
            holder is None
            or holder.get("diskNumber") != number(disk)
            or holder.get("partitionNumber") != partition.get("partitionNumber")
        ):
            raise BootstrapError(f"{letter}: remains occupied by an unexpected volume.")
    if partition.get("driveLetter") != letter:
        try:
            storage(
                "SetPartitionLetter",
                {
                    "diskNumber": number(disk),
                    "partitionNumber": partition.get("partitionNumber"),
                    "letter": letter,
                },
            )
        except BootstrapError as error:
            raise BootstrapError(
                f"failed to assign {letter}: to the managed data disk: {error}"
            ) from error
    verified = optional_result(storage("PartitionAtLetter", {"letter": letter}))
    volume = optional_result(storage("VolumeAtLetter", {"letter": letter}))
    if (
        verified is None
        or verified.get("diskNumber") != number(disk)
        or volume is None
        or volume.get("fileSystem") != "NTFS"
    ):
        raise BootstrapError(
            f"final verification failed; {letter}: is not the selected managed NTFS "
            "data disk."
        )
    windows_system.set_registry_value(PENDING_KEY, "Completed", 1)
    windows_system.delete_registry_value(PENDING_KEY, "RebootRequestedFromBoot")
    log(f"=== setup-datadisk finished OK; managed disk is mounted at {letter}: ===")
    return True

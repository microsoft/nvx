"""Prepare the Azure managed data disk of a Linux runner.

The disk is found by its Azure LUN. Recognized ext4 data is preserved; only a
disk that a full-device scan proves zero-filled, or that bears this helper's
interrupted-initialization marker, is ever partitioned or formatted. The
selected device is verified before fstab changes and again after mounting.
"""

import os
import re
import stat
import sys
import tempfile
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from . import common
from .common import BootstrapError, Log

# GPT type that marks a partition whose initialization has not completed.
INIT_PART_TYPE = "8f8f0f40-5c84-4a2a-8c22-8fbff2e8bc91"
DATA_PART_TYPE = "0fc63daf-8483-4772-8e79-3d69d8477de4"
# Disk identifier of the GPT table that initialization creates.
INIT_TABLE_ID = "6f746dc1-31d4-4b7c-b2ee-9cf6075517a1"
MOUNT_OPTIONS = "defaults,nofail,x-systemd.device-timeout=30s"
DISCOVERY_SECONDS = 30


@dataclass(frozen=True)
class DiskPaths:
    log: Path = Path("/var/log/setup-datadisk.log")
    fsck_log: Path = Path("/var/log/setup-datadisk-e2fsck.log")
    fstab: Path = Path("/etc/fstab")
    sysfs_block: Path = Path("/sys/class/block")
    # Azure exposes managed data disks differently on SCSI and NVMe/Boost
    # controllers. Both paths identify the managed disk by Azure LUN.
    lun_links: tuple[str, ...] = (
        "/dev/disk/azure/data/by-lun/{lun}",
        "/dev/disk/azure/scsi1/lun{lun}",
    )
    # Owner of a rewritten fstab; None leaves the current user's.
    owner: tuple[int, int] | None = (0, 0)


def validate(lun: int, mount_point: str) -> None:
    if not 0 <= lun <= 63:
        raise BootstrapError("data disk LUN must be an integer from 0 to 63")
    if not mount_point.startswith(("/mnt/", "/srv/")):
        raise BootstrapError("data disk mount point must be below /mnt or /srv")
    if (
        not re.fullmatch(r"/[A-Za-z0-9_./-]+", mount_point)
        or any(part in (".", "..") for part in mount_point.split("/"))
        or mount_point.endswith("/")
        or "//" in mount_point
    ):
        raise BootstrapError("invalid data disk mount point")


def ensure_partitioning_tool() -> None:
    """Install the distribution's sfdisk package when it is missing."""
    if common.which("sfdisk") is not None:
        return
    if common.which("apt-get") is not None:
        common.run_command(
            ["apt-get", "install", "-y", "fdisk"],
            capture=False,
            env={**os.environ, "DEBIAN_FRONTEND": "noninteractive"},
        )
    elif common.which("tdnf") is not None:
        common.run_command(["tdnf", "install", "-y", "util-linux"], capture=False)
    elif common.which("dnf") is not None:
        common.run_command(["dnf", "install", "-y", "util-linux"], capture=False)
    else:
        raise BootstrapError(
            "sfdisk is unavailable and no supported package manager was found"
        )
    if common.which("sfdisk") is None:
        raise BootstrapError(
            "sfdisk is unavailable after installing the partitioning tools"
        )


def find_disk(lun: int, paths: DiskPaths) -> Path:
    for attempt in range(DISCOVERY_SECONDS):
        for template in paths.lun_links:
            link = Path(template.format(lun=lun))
            if link.exists():
                device = Path(os.path.realpath(link))
                if not stat.S_ISBLK(os.stat(device).st_mode):
                    raise BootstrapError(f"{link} did not resolve to a block device")
                return device
        if attempt + 1 < DISCOVERY_SECONDS:
            common.sleep(1)
    raise BootstrapError(
        f"managed data disk LUN {lun} was not found after {DISCOVERY_SECONDS} seconds"
    )


def list_partitions(disk: Path) -> list[Path]:
    output = common.run_command(["lsblk", "-lnpo", "NAME,TYPE", disk.as_posix()]).text()
    partitions: list[Path] = []
    for line in output.splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[1] == "part":
            partitions.append(Path(fields[0]))
    return partitions


def filesystem_type(device: Path) -> str:
    """Return the filesystem that low-level probing finds, or an empty string."""
    result = common.run_command(
        ["blkid", "-p", "-s", "TYPE", "-o", "value", device.as_posix()], check=False
    )
    if result.returncode == 2:
        return ""
    if result.returncode != 0:
        raise BootstrapError(
            f"filesystem probe of {device} failed (rc={result.returncode}); "
            "refusing to guess"
        )
    return result.text().strip()


def partition_type(device: Path) -> str:
    output = common.run_command(["lsblk", "-dno", "PARTTYPE", device.as_posix()]).text()
    return "".join(output.split()).lower()


def has_signatures(device: Path) -> bool:
    result = common.run_command(["wipefs", "-n", device.as_posix()], check=False)
    if result.returncode != 0:
        raise BootstrapError(
            f"signature probe of {device} failed (rc={result.returncode}); "
            "refusing to modify it"
        )
    return bool(result.text().strip())


def owns_empty_table(disk: Path) -> bool:
    """Return whether the disk holds only the empty table that setup creates."""
    result = common.run_command(["sfdisk", "--dump", disk.as_posix()], check=False)
    if result.returncode != 0:
        return False
    table_ids: list[str] = []
    entries = 0
    for line in result.text().splitlines():
        key, separator, value = line.partition(":")
        if separator and key == "label-id":
            table_ids.append("".join(value.split()).lower())
        if not line.startswith("#") and " : " in line:
            entries += 1
    return table_ids == [INIT_TABLE_ID] and entries == 0


def verify_blank_device(device: Path) -> None:
    """Fail unless every byte of the device is zero."""
    result = common.run_command(
        ["blockdev", "--getsize64", device.as_posix()], check=False
    )
    size = result.text().strip()
    if result.returncode != 0:
        raise BootstrapError(
            f"could not read the size of {device}; refusing to initialize it"
        )
    if not re.fullmatch(r"[0-9]+", size) or int(size) <= 0:
        raise BootstrapError(
            f"invalid device size for {device}; refusing to initialize it"
        )
    try:
        with open(device, "rb", buffering=0) as stream:
            blank = common.zero_filled(stream, int(size))
    except (OSError, BootstrapError) as error:
        raise BootstrapError(
            f"could not verify blankness of {device} ({error}); "
            "refusing to initialize it"
        ) from error
    if not blank:
        raise BootstrapError(
            f"{device} is not entirely zero-filled; preserving unrecognized data"
        )


def logged(command: Sequence[str], log: Log, stdin: bytes | None = None) -> None:
    """Run a command that changes the disk, logging all of its output."""
    result = common.run_command(command, stdin=stdin, check=False)
    output = (result.stdout + result.stderr).decode("utf-8", "replace").strip()
    if output:
        log(output)
    if result.returncode != 0:
        raise BootstrapError(
            f"{Path(command[0]).name} failed with exit code {result.returncode}"
        )


def verify_new_filesystem(device: Path, paths: DiskPaths) -> None:
    if sys.platform != "win32":
        os.sync()
    result = common.run_command(["e2fsck", "-fn", device.as_posix()], check=False)
    paths.fsck_log.write_bytes(result.stdout + result.stderr)
    if result.returncode != 0:
        raise BootstrapError(
            f"validation of newly created filesystem {device} failed "
            f"(rc={result.returncode}); see {paths.fsck_log}"
        )


def set_partition_type(
    disk: Path, partition: Path, kind: str, paths: DiskPaths
) -> None:
    number_path = paths.sysfs_block / partition.name / "partition"
    try:
        number = number_path.read_text(encoding="ascii").strip()
    except OSError:
        number = ""
    if not re.fullmatch(r"[0-9]+", number):
        raise BootstrapError(
            f"could not determine the partition number for {partition}"
        )
    common.run_command(["sfdisk", "--part-type", disk.as_posix(), number, kind])
    common.run_command(["udevadm", "settle"])


def format_partition(
    disk: Path, partition: Path, paths: DiskPaths, log: Log, *, force: bool
) -> None:
    logged(["mkfs.ext4", *(["-F"] if force else []), partition.as_posix()], log)
    verify_new_filesystem(partition, paths)
    set_partition_type(disk, partition, DATA_PART_TYPE, paths)


def require_blank_or_owned(disk: Path, log: Log) -> None:
    """Fail unless the disk is zero-filled or holds only the owned empty table.

    A missing signature does not establish blankness. Only zero-filled disks
    or this helper's interrupted initialization may be repartitioned.
    """
    if has_signatures(disk):
        if not owns_empty_table(disk):
            raise BootstrapError(
                f"{disk} contains signatures not belonging to an interrupted runner "
                "disk initialization; refusing to repartition it"
            )
        log(f"Recovering an interrupted empty GPT-table creation on {disk}")
    else:
        verify_blank_device(disk)


def initialize_blank_disk(disk: Path, paths: DiskPaths, log: Log) -> Path:
    """Partition and format a disk that is blank or bears the owned empty table."""
    require_blank_or_owned(disk, log)
    log(f"Atomically creating a GPT partition on blank disk {disk}")
    script = f"label: gpt\nlabel-id: {INIT_TABLE_ID}\n,,{INIT_PART_TYPE}\n"
    logged(["sfdisk", disk.as_posix()], log, stdin=script.encode())
    common.run_command(["udevadm", "settle"])
    partitions: list[Path] = []
    for attempt in range(DISCOVERY_SECONDS):
        partitions = list_partitions(disk)
        if len(partitions) == 1 and _is_block_device(partitions[0]):
            break
        if attempt + 1 < DISCOVERY_SECONDS:
            common.sleep(1)
    else:
        raise BootstrapError(
            f"exactly one partition did not appear on {disk} after "
            f"{DISCOVERY_SECONDS} seconds"
        )
    partition = partitions[0]
    if has_signatures(partition):
        raise BootstrapError(
            f"new partition {partition} unexpectedly contains a signature; "
            "refusing to format it"
        )
    format_partition(disk, partition, paths, log, force=False)
    return partition


def select_target(disk: Path, paths: DiskPaths, log: Log) -> Path:
    """Return the ext4 device to mount, finishing only owned initialization."""
    whole_filesystem = filesystem_type(disk)
    partitions = list_partitions(disk)
    if whole_filesystem and partitions:
        raise BootstrapError(
            f"{disk} reports both a whole-disk filesystem and partitions; "
            "refusing to guess"
        )
    if whole_filesystem:
        target = disk
    elif not partitions:
        target = initialize_blank_disk(disk, paths, log)
    elif len(partitions) == 1:
        target = partitions[0]
        kind = partition_type(target)
        if not filesystem_type(target):
            # The recoverable state of a crash after partitioning and before
            # formatting. Never format if any unknown signature is present.
            if kind != INIT_PART_TYPE:
                raise BootstrapError(
                    f"blank partition {target} lacks the runner disk initialization "
                    "marker; refusing to format it"
                )
            if has_signatures(target):
                raise BootstrapError(
                    f"unformatted partition {target} contains a signature; "
                    "refusing to overwrite it"
                )
            log(
                "Completing interrupted initialization by formatting the sole "
                f"blank partition {target}"
            )
            format_partition(disk, target, paths, log, force=False)
        elif kind == INIT_PART_TYPE:
            # A crash during or immediately after formatting leaves an ext4
            # signature while the marker remains. Such a partition has never
            # been mounted, so finishing initialization overwrites no data.
            log(f"Recreating the incomplete marked filesystem on {target}")
            format_partition(disk, target, paths, log, force=True)
    else:
        raise BootstrapError(
            f"{disk} has {len(partitions)} partitions; refusing to select or modify one"
        )
    found = filesystem_type(target)
    if found != "ext4":
        raise BootstrapError(
            f"{target} contains '{found}', not ext4; preserving it but refusing to "
            "mount it as ext4"
        )
    return target


def _is_block_device(path: Path) -> bool:
    try:
        return stat.S_ISBLK(os.stat(path).st_mode)
    except OSError:
        return False


def is_mounted(mount_point: str) -> bool:
    return (
        common.run_command(["mountpoint", "-q", mount_point], check=False).returncode
        == 0
    )


def verify_mount_identity(mount_point: str, target: Path) -> None:
    mounted = common.run_command(
        ["findmnt", "-rn", "-o", "MAJ:MIN", "--target", mount_point], check=False
    )
    if mounted.returncode != 0:
        raise BootstrapError(f"could not identify the mounted device at {mount_point}")
    selected = common.run_command(
        ["lsblk", "-dno", "MAJ:MIN", target.as_posix()], check=False
    )
    if selected.returncode != 0:
        raise BootstrapError(f"could not identify the selected device {target}")
    mounted_device = mounted.text().strip()
    selected_device = "".join(selected.text().split())
    if not mounted_device or not selected_device or mounted_device != selected_device:
        raise BootstrapError(f"{mount_point} is not mounted from {target}")


def fstab_spec(target: Path) -> str:
    for tag in ("PARTUUID", "UUID"):
        result = common.run_command(
            ["blkid", "-s", tag, "-o", "value", target.as_posix()], check=False
        )
        value = result.text().strip() if result.returncode == 0 else ""
        if value:
            return f"{tag}={value}"
    raise BootstrapError(f"could not read PARTUUID or UUID for {target}")


def update_fstab(spec: str, mount_point: str, paths: DiskPaths, log: Log) -> None:
    """Make the mount point's only fstab entry the expected one."""
    expected = f"{spec} {mount_point} ext4 {MOUNT_OPTIONS} 1 2"
    current = paths.fstab.read_text(encoding="utf-8") if paths.fstab.exists() else ""
    lines = current.split("\n")
    if lines and lines[-1] == "":
        lines.pop()

    def entry(line: str) -> bool:
        fields = line.split()
        return (
            len(fields) >= 2
            and not fields[0].startswith("#")
            and fields[1] == mount_point
        )

    entries = [line for line in lines if entry(line)]
    if entries == [expected]:
        log(f"fstab already contains exactly the expected {mount_point} entry")
        return
    content = "".join(line + "\n" for line in lines if not entry(line))
    content += expected + "\n"
    descriptor, temporary = tempfile.mkstemp(
        prefix=paths.fstab.name + ".setup-datadisk.", dir=paths.fstab.parent
    )
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            stream.write(content)
        if paths.owner is not None and sys.platform != "win32":
            os.chown(temporary, *paths.owner)
        os.chmod(temporary, 0o644)
        os.replace(temporary, paths.fstab)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise
    if common.which("restorecon") is not None:
        common.run_command(["restorecon", paths.fstab.as_posix()], check=False)
    log(f"Set fstab entry: {expected}")


def mount(mount_point: str, target: Path, paths: DiskPaths, log: Log) -> None:
    if is_mounted(mount_point):
        verify_mount_identity(mount_point, target)
    elif common.run_command(
        ["findmnt", "-rn", "-S", target.as_posix()], check=False
    ).returncode:
        result = common.run_command(["e2fsck", "-n", target.as_posix()], check=False)
        paths.fsck_log.write_bytes(result.stdout + result.stderr)
        if result.returncode != 0:
            # Status 1 means errors were found and would be corrected. Any
            # status is operator-visible, but bootstrap never repairs or
            # reformats an existing filesystem automatically.
            log(
                f"WARNING: e2fsck -n returned {result.returncode} for {target}; "
                f"see {paths.fsck_log}"
            )
    spec = fstab_spec(target)
    os.makedirs(mount_point, exist_ok=True)
    update_fstab(spec, mount_point, paths, log)
    common.run_command(["systemctl", "daemon-reload"])
    if not is_mounted(mount_point):
        result = common.run_command(["mount", mount_point], check=False)
        if result.returncode != 0 and not is_mounted(mount_point):
            raise BootstrapError(
                f"could not mount {mount_point}: {result.stderr.decode(errors='replace').strip()}"
            )
    if not is_mounted(mount_point):
        raise BootstrapError(f"{mount_point} is not mounted after setup")
    verify_mount_identity(mount_point, target)


def setup(
    lun: int,
    mount_point: str,
    paths: DiskPaths | None = None,
    prepare: Callable[[], None] = ensure_partitioning_tool,
) -> None:
    paths = paths or DiskPaths()
    validate(lun, mount_point)
    log = Log(paths.log)
    log(f"=== setup-datadisk run at {datetime.now(timezone.utc).isoformat()} ===")
    try:
        prepare()
        disk = find_disk(lun, paths)
        log(f"Using managed data disk LUN {lun}: {disk}")
        target = select_target(disk, paths, log)
        mount(mount_point, target, paths, log)
    except BootstrapError as error:
        log(f"ERROR: {error}")
        raise
    log("=== setup-datadisk finished OK ===")

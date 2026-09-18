"""Verified Ubuntu Base preparation, package provenance, and EROFS helpers."""

from __future__ import annotations

import gzip
import hashlib
import io
import json
import lzma
import os
import re
import shutil
import stat
import subprocess
import tarfile
import uuid
from collections.abc import Iterable, Sequence
from pathlib import Path, PurePosixPath
from typing import TypedDict, cast

from .common import (
    REPO_ROOT,
    ScriptError,
    cache_root,
    download_verified,
    require_tool,
    sha256_file,
)

DEFAULT_UBUNTU_VERSION = "26.04.1"
DEFAULT_UBUNTU_CODENAME = "resolute"
DEFAULT_UBUNTU_ARCHITECTURE = "amd64"
DEFAULT_UBUNTU_BASE_URL = (
    "https://cdimage.ubuntu.com/ubuntu-base/releases/26.04/release/"
    "ubuntu-base-26.04.1-base-amd64.tar.gz"
)
DEFAULT_UBUNTU_BASE_SHA256 = (
    "a496a960472ce474a59590b8987d3a1135d3cbef1991f3b1abe8cacfea8bf85a"
)
UBUNTU_PACKAGE_LOCK = REPO_ROOT / "ubuntu" / "packages.lock.json"
UBUNTU_EROFS_FORMAT = 1
_SHA256 = re.compile(r"[0-9a-f]{64}")
_PACKAGE_NAME = re.compile(r"[a-z0-9][a-z0-9+.-]*")
_KNOWN_PACKAGE_CONTROL_ACTIONS = {
    "iputils-ping": frozenset({"postinst"}),
    "libcap2": frozenset({"triggers"}),
    "libidn2-0": frozenset({"triggers"}),
    "libunistring5": frozenset({"triggers"}),
    "netcat-openbsd": frozenset({"postinst", "prerm"}),
}


class UbuntuLockedPackage(TypedDict):
    name: str
    source_name: str
    source_version: str
    version: str
    architecture: str
    url: str
    sha256: str
    license_path: str
    required_by: list[str]
    depends: str
    pre_depends: str
    component: str
    suite: str


class UbuntuPackageRecord(TypedDict):
    name: str
    version: str
    architecture: str
    source_name: str
    source_version: str
    installed: bool
    deb_sha256: str | None
    license_path: str
    origin: str


HelperProvenance = dict[str, dict[str, str]]


def parse_deb822(text: str) -> list[dict[str, str]]:
    records: list[dict[str, str]] = []
    record: dict[str, str] = {}
    current_field: str | None = None
    for line in text.splitlines():
        if not line:
            if record:
                records.append(record)
                record = {}
                current_field = None
            continue
        if line[0].isspace():
            if current_field is None:
                raise ScriptError("Debian control continuation has no field")
            record[current_field] += "\n" + line[1:]
            continue
        current_field, separator, value = line.partition(":")
        if not separator or not current_field:
            raise ScriptError(f"malformed Debian control line: {line}")
        if current_field in record:
            raise ScriptError(f"duplicate Debian control field: {current_field}")
        record[current_field] = value.lstrip()
    if record:
        records.append(record)
    return records


def _required_string(
    document: dict[str, object],
    field: str,
    *,
    label: str,
) -> str:
    value = document.get(field)
    if not isinstance(value, str) or not value:
        raise ScriptError(f"{label} has no valid {field}")
    return value


def load_package_lock(
    path: Path = UBUNTU_PACKAGE_LOCK,
) -> tuple[UbuntuLockedPackage, ...]:
    raw_document = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(raw_document, dict):
        raise ScriptError(f"{path} must contain a JSON object")
    document = cast(dict[str, object], raw_document)
    expected_header = {
        "format": 1,
        "release": DEFAULT_UBUNTU_VERSION,
        "codename": DEFAULT_UBUNTU_CODENAME,
        "architecture": DEFAULT_UBUNTU_ARCHITECTURE,
    }
    for field, expected in expected_header.items():
        if document.get(field) != expected:
            raise ScriptError(f"{path} {field} must be {expected!r}")
    raw_packages = document.get("packages")
    if not isinstance(raw_packages, list) or not raw_packages:
        raise ScriptError(f"{path} must contain a nonempty packages array")
    packages: list[UbuntuLockedPackage] = []
    names: set[str] = set()
    for raw_package in cast(list[object], raw_packages):
        if not isinstance(raw_package, dict):
            raise ScriptError(f"{path} contains a non-object package record")
        package_document = cast(dict[str, object], raw_package)
        name = _required_string(package_document, "name", label=str(path))
        if _PACKAGE_NAME.fullmatch(name) is None:
            raise ScriptError(f"{path} contains invalid package name {name!r}")
        if name in names:
            raise ScriptError(f"{path} contains duplicate package {name}")
        names.add(name)
        required_by = package_document.get("required_by")
        if (
            not isinstance(required_by, list)
            or not required_by
            or not all(
                isinstance(item, str) and item
                for item in cast(list[object], required_by)
            )
        ):
            raise ScriptError(f"{path} package {name} has invalid required_by")
        package = cast(
            UbuntuLockedPackage,
            {
                field: _required_string(
                    package_document,
                    field,
                    label=f"{path} package {name}",
                )
                for field in (
                    "name",
                    "source_name",
                    "source_version",
                    "version",
                    "architecture",
                    "url",
                    "sha256",
                    "license_path",
                    "component",
                    "suite",
                )
            }
            | {
                "required_by": [
                    cast(str, item) for item in cast(list[object], required_by)
                ],
                "depends": str(package_document.get("depends", "")),
                "pre_depends": str(package_document.get("pre_depends", "")),
            },
        )
        if package["architecture"] != DEFAULT_UBUNTU_ARCHITECTURE:
            raise ScriptError(
                f"{path} package {name} has unsupported architecture "
                f"{package['architecture']}"
            )
        if not package["url"].startswith("https://") or "/pool/" not in package["url"]:
            raise ScriptError(f"{path} package {name} has a non-immutable URL")
        if _SHA256.fullmatch(package["sha256"]) is None:
            raise ScriptError(f"{path} package {name} has an invalid SHA-256")
        if not package["license_path"].startswith("/usr/share/doc/"):
            raise ScriptError(f"{path} package {name} has an invalid license path")
        packages.append(package)
    if [package["name"] for package in packages] != sorted(names):
        raise ScriptError(f"{path} package records must be sorted by name")
    return tuple(packages)


def _normalize_archive_path(raw_name: str, label: str) -> PurePosixPath:
    path = PurePosixPath(raw_name)
    if path.is_absolute():
        raise ScriptError(f"{label} contains absolute path {raw_name!r}")
    parts: list[str] = []
    for part in path.parts:
        if part in ("", "."):
            continue
        if part == "..":
            raise ScriptError(f"{label} contains path traversal {raw_name!r}")
        parts.append(part)
    return PurePosixPath(*parts)


def _virtual_symlink_target(path: PurePosixPath, linkname: str, label: str) -> None:
    target = PurePosixPath(linkname)
    parts = [] if target.is_absolute() else list(path.parent.parts)
    for part in target.parts:
        if part in ("", ".", "/"):
            continue
        if part == "..":
            if not parts:
                raise ScriptError(f"{label} symlink {path} escapes the extraction root")
            parts.pop()
        else:
            parts.append(part)


def _member_kind(member: tarfile.TarInfo) -> str:
    if member.isdir():
        return "directory"
    if member.isfile():
        return "file"
    if member.issym():
        return "symlink"
    if member.islnk():
        return "hardlink"
    if member.ischr() or member.isblk():
        return "device"
    if member.isfifo():
        return "fifo"
    return "socket-or-unknown"


def _path_exists(path: Path) -> bool:
    return os.path.lexists(path)


def _destination_path(root: Path, relative: PurePosixPath, label: str) -> Path:
    current = root
    for part in relative.parts[:-1]:
        current /= part
        if _path_exists(current) and current.is_symlink():
            raise ScriptError(
                f"{label} would write through archive-controlled symlink {current}"
            )
    return root.joinpath(*relative.parts)


def _safe_extract_open_tar(
    archive: tarfile.TarFile,
    destination: Path,
    *,
    label: str,
) -> tuple[PurePosixPath, ...]:
    members: dict[PurePosixPath, tarfile.TarInfo] = {}
    member_kinds: dict[PurePosixPath, str] = {}
    for member in archive.getmembers():
        path = _normalize_archive_path(member.name, label)
        if not path.parts:
            continue
        kind = _member_kind(member)
        if kind in ("device", "fifo", "socket-or-unknown"):
            raise ScriptError(f"{label} contains unsupported {kind} {path}")
        previous = member_kinds.get(path)
        if previous is not None:
            if previous != kind or kind != "directory":
                raise ScriptError(
                    f"{label} contains duplicate path {path} as {previous} and {kind}"
                )
            continue
        members[path] = member
        member_kinds[path] = kind
        if kind == "symlink":
            _virtual_symlink_target(path, member.linkname, label)
        elif kind == "hardlink":
            _normalize_archive_path(member.linkname, label)

    for path in members:
        for parent in path.parents:
            if not parent.parts:
                break
            parent_kind = member_kinds.get(parent)
            if parent_kind is not None and parent_kind != "directory":
                raise ScriptError(
                    f"{label} places {path} below non-directory archive member {parent}"
                )

    destination.mkdir(parents=True, exist_ok=True)
    directory_members = sorted(
        (
            (path, member)
            for path, member in members.items()
            if member_kinds[path] == "directory"
        ),
        key=lambda item: (len(item[0].parts), item[0].as_posix()),
    )
    for path, _member in directory_members:
        target = _destination_path(destination, path, label)
        if _path_exists(target):
            if target.is_symlink() or not target.is_dir():
                raise ScriptError(f"{label} directory conflicts with {target}")
        else:
            target.mkdir(mode=0o755)

    for path, member in sorted(members.items(), key=lambda item: item[0].as_posix()):
        kind = member_kinds[path]
        if kind not in ("file", "symlink"):
            continue
        target = _destination_path(destination, path, label)
        if _path_exists(target):
            raise ScriptError(f"{label} member conflicts with existing path {target}")
        target.parent.mkdir(parents=True, exist_ok=True)
        if kind == "symlink":
            os.symlink(member.linkname, target)
            continue
        source = archive.extractfile(member)
        if source is None:
            raise ScriptError(f"{label} could not read regular file {path}")
        with source, target.open("xb") as output:
            shutil.copyfileobj(source, output)
        target.chmod(stat.S_IMODE(member.mode))

    pending_hardlinks = {
        path: member
        for path, member in members.items()
        if member_kinds[path] == "hardlink"
    }
    while pending_hardlinks:
        progress = False
        for path, member in tuple(pending_hardlinks.items()):
            target = _destination_path(destination, path, label)
            if _path_exists(target):
                raise ScriptError(
                    f"{label} hard link conflicts with existing path {target}"
                )
            link_path = _normalize_archive_path(member.linkname, label)
            source = _destination_path(destination, link_path, label)
            if not _path_exists(source):
                continue
            if source.is_symlink() or not source.is_file():
                raise ScriptError(
                    f"{label} hard link {path} targets non-file {link_path}"
                )
            target.parent.mkdir(parents=True, exist_ok=True)
            os.link(source, target)
            del pending_hardlinks[path]
            progress = True
        if not progress:
            unresolved = ", ".join(
                f"{path}->{member.linkname}"
                for path, member in sorted(
                    pending_hardlinks.items(),
                    key=lambda item: item[0].as_posix(),
                )
            )
            raise ScriptError(f"{label} has unresolved hard links: {unresolved}")

    for path, member in reversed(directory_members):
        target = _destination_path(destination, path, label)
        target.chmod(stat.S_IMODE(member.mode))
    return tuple(sorted(members, key=PurePosixPath.as_posix))


def safe_extract_tar(path: Path, destination: Path, *, label: str) -> None:
    try:
        with tarfile.open(path, "r:*") as archive:
            _safe_extract_open_tar(archive, destination, label=label)
    except tarfile.TarError as error:
        raise ScriptError(f"invalid {label}: {error}") from error


def _decompress_tar_member(name: str, data: bytes) -> bytes:
    if name.endswith(".gz"):
        return gzip.decompress(data)
    if name.endswith(".xz"):
        return lzma.decompress(data)
    if name.endswith(".zst"):
        zstd = require_tool(
            "zstd",
            "zstd was not found on PATH; install it to extract Ubuntu packages",
        )
        result = subprocess.run(
            [zstd, "--decompress", "--stdout", "--quiet"],
            input=data,
            capture_output=True,
        )
        if result.returncode != 0:
            diagnostic = result.stderr.decode("utf-8", errors="replace").strip()
            raise ScriptError(
                f"zstd could not decompress {name}"
                + (f": {diagnostic}" if diagnostic else "")
            )
        return result.stdout
    if name.endswith(".tar"):
        return data
    raise ScriptError(f"unsupported Debian package member compression: {name}")


def _read_ar_members(path: Path) -> dict[str, bytes]:
    data = path.read_bytes()
    if not data.startswith(b"!<arch>\n"):
        raise ScriptError(f"{path} is not a Debian ar archive")
    members: dict[str, bytes] = {}
    offset = 8
    while offset < len(data):
        header = data[offset : offset + 60]
        if len(header) != 60 or header[58:60] != b"`\n":
            raise ScriptError(f"{path} has an invalid ar header at offset {offset}")
        raw_name = header[:16].decode("ascii", errors="strict").strip()
        if raw_name.startswith(("/", "#1/")):
            raise ScriptError(f"{path} uses an unsupported extended ar filename")
        name = raw_name.removesuffix("/")
        try:
            size = int(header[48:58].decode("ascii").strip(), 10)
        except ValueError as error:
            raise ScriptError(f"{path} has an invalid ar member size") from error
        start = offset + 60
        end = start + size
        if end > len(data):
            raise ScriptError(f"{path} has a truncated ar member {name}")
        if name in members:
            raise ScriptError(f"{path} contains duplicate ar member {name}")
        members[name] = data[start:end]
        offset = end + (size % 2)
    if members.get("debian-binary") != b"2.0\n":
        raise ScriptError(f"{path} is not Debian package format 2.0")
    return members


def _control_files(
    package_path: Path,
    name: str,
    data: bytes,
) -> tuple[dict[str, tuple[bytes, int]], dict[str, str]]:
    files: dict[str, tuple[bytes, int]] = {}
    try:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as archive:
            for member in archive.getmembers():
                relative = _normalize_archive_path(
                    member.name,
                    f"{package_path.name} control archive",
                )
                if not relative.parts or member.isdir():
                    continue
                if len(relative.parts) != 1 or not member.isfile():
                    raise ScriptError(
                        f"{package_path.name} has unsupported control member "
                        f"{member.name}"
                    )
                source = archive.extractfile(member)
                if source is None:
                    raise ScriptError(
                        f"{package_path.name} could not read control member "
                        f"{member.name}"
                    )
                files[relative.name] = (source.read(), stat.S_IMODE(member.mode))
    except tarfile.TarError as error:
        raise ScriptError(
            f"invalid control archive in {package_path}: {error}"
        ) from error
    control_entry = files.get("control")
    if control_entry is None:
        raise ScriptError(f"{package_path.name} has no control metadata")
    records = parse_deb822(control_entry[0].decode("utf-8"))
    if len(records) != 1:
        raise ScriptError(f"{package_path.name} has invalid control metadata")
    control = records[0]
    if control.get("Package") != name:
        raise ScriptError(
            f"{package_path.name} identifies package {control.get('Package')!r}, "
            f"expected {name!r}"
        )
    return files, control


def _source_identity(record: dict[str, str]) -> tuple[str, str]:
    binary_name = record.get("Package")
    binary_version = record.get("Version")
    if not binary_name or not binary_version:
        raise ScriptError("installed Ubuntu package has no name or version")
    source = record.get("Source")
    if source is None:
        return binary_name, binary_version
    match = re.fullmatch(r"([^ ]+)(?: \((.+)\))?", source)
    if match is None:
        raise ScriptError(
            f"{binary_name}-{binary_version} has invalid Source field {source!r}"
        )
    return match.group(1), match.group(2) or binary_version


def _dependency_alternatives(value: str) -> Iterable[tuple[str, ...]]:
    for dependency in value.replace("\n", " ").split(","):
        alternatives: list[str] = []
        for raw_alternative in dependency.split("|"):
            name = re.split(
                r"\s*(?:\(|\[|<)",
                raw_alternative.strip(),
                maxsplit=1,
            )[0].split(":", maxsplit=1)[0]
            if name:
                alternatives.append(name)
        if alternatives:
            yield tuple(alternatives)


def _validate_package_closure(
    base_packages: set[str],
    packages: Sequence[UbuntuLockedPackage],
) -> None:
    available = base_packages | {package["name"] for package in packages}
    for package in packages:
        for field in ("pre_depends", "depends"):
            for alternatives in _dependency_alternatives(package[field]):
                if not any(alternative in available for alternative in alternatives):
                    raise ScriptError(
                        f"Ubuntu package lock cannot satisfy {package['name']} "
                        f"dependency {' | '.join(alternatives)}"
                    )


def _append_status_record(root: Path, control_text: str) -> None:
    lines = control_text.strip().splitlines()
    if not lines or not lines[0].startswith("Package: "):
        raise ScriptError("supplemental Ubuntu package has malformed control metadata")
    if any(line.startswith("Status: ") for line in lines):
        raise ScriptError("supplemental Ubuntu package unexpectedly contains Status")
    lines.insert(1, "Status: install ok installed")
    status = root / "var" / "lib" / "dpkg" / "status"
    with status.open("a", encoding="utf-8", newline="\n") as output:
        if status.stat().st_size:
            output.write("\n")
        output.write("\n".join(lines) + "\n")


def _install_deb(
    root: Path,
    package: UbuntuLockedPackage,
    package_path: Path,
) -> None:
    members = _read_ar_members(package_path)
    control_name = next(
        (name for name in members if name.startswith("control.tar")),
        None,
    )
    data_name = next(
        (name for name in members if name.startswith("data.tar")),
        None,
    )
    if control_name is None or data_name is None:
        raise ScriptError(f"{package_path.name} has no control or data archive")
    control_files, control = _control_files(
        package_path,
        package["name"],
        _decompress_tar_member(control_name, members[control_name]),
    )
    for field, expected in (
        ("Version", package["version"]),
        ("Architecture", package["architecture"]),
        ("Depends", package["depends"]),
        ("Pre-Depends", package["pre_depends"]),
    ):
        if control.get(field, "") != expected:
            raise ScriptError(
                f"{package_path.name} {field} is {control.get(field, '')!r}, "
                f"expected {expected!r}"
            )
    source_name, source_version = _source_identity(control)
    if (source_name, source_version) != (
        package["source_name"],
        package["source_version"],
    ):
        raise ScriptError(
            f"{package_path.name} source is {source_name} {source_version}, "
            f"expected {package['source_name']} {package['source_version']}"
        )
    actions = frozenset(control_files) & frozenset(
        {"preinst", "postinst", "prerm", "postrm", "triggers"}
    )
    allowed_actions = _KNOWN_PACKAGE_CONTROL_ACTIONS.get(
        package["name"],
        frozenset(),
    )
    if actions != allowed_actions:
        raise ScriptError(
            f"{package_path.name} has unsupported maintainer actions: "
            f"{', '.join(sorted(actions ^ allowed_actions))}"
        )

    try:
        with tarfile.open(
            fileobj=io.BytesIO(_decompress_tar_member(data_name, members[data_name])),
            mode="r:",
        ) as archive:
            installed_paths = _safe_extract_open_tar(
                archive,
                root,
                label=f"{package_path.name} data archive",
            )
    except tarfile.TarError as error:
        raise ScriptError(f"invalid data archive in {package_path}: {error}") from error

    info = root / "var" / "lib" / "dpkg" / "info"
    info.mkdir(parents=True, exist_ok=True)
    for control_file, (data, mode) in control_files.items():
        if control_file == "control":
            continue
        destination = info / f"{package['name']}.{control_file}"
        destination.write_bytes(data)
        destination.chmod(mode)
    (info / f"{package['name']}.list").write_text(
        "".join(f"/{path.as_posix()}\n" for path in installed_paths),
        encoding="utf-8",
    )
    _append_status_record(root, control_files["control"][0].decode("utf-8"))


def _ensure_symlink(root: Path, relative: str, target: str) -> None:
    path = root / relative
    if _path_exists(path):
        if path.is_symlink() and os.readlink(path) == target:
            return
        raise ScriptError(f"Ubuntu customization path already exists: /{relative}")
    path.parent.mkdir(parents=True, exist_ok=True)
    os.symlink(target, path)


def _clear_directory(path: Path) -> None:
    if not path.is_dir() or path.is_symlink():
        return
    for child in path.iterdir():
        if child.is_dir() and not child.is_symlink():
            shutil.rmtree(child)
        else:
            child.unlink()


def _validate_accounts(root: Path) -> None:
    passwd = [
        line.split(":")
        for line in (root / "etc" / "passwd").read_text(encoding="utf-8").splitlines()
        if line
    ]
    groups = [
        line.split(":")
        for line in (root / "etc" / "group").read_text(encoding="utf-8").splitlines()
        if line
    ]
    roots = [record for record in passwd if len(record) >= 7 and record[2] == "0"]
    nobody = [record for record in passwd if len(record) >= 7 and record[2] == "65534"]
    nogroup = [record for record in groups if len(record) >= 3 and record[2] == "65534"]
    if len(roots) != 1 or roots[0][0] != "root":
        raise ScriptError("Ubuntu Base must contain one root account")
    if (
        len(nobody) != 1
        or nobody[0][0] != "nobody"
        or nobody[0][3] != "65534"
        or nobody[0][5] != "/nonexistent"
    ):
        raise ScriptError("Ubuntu Base must contain the expected nobody account")
    if len(nogroup) != 1 or nogroup[0][0] != "nogroup":
        raise ScriptError("Ubuntu Base must contain the expected nogroup")


def _validate_usr_merge(root: Path) -> None:
    expected = {
        "bin": "usr/bin",
        "sbin": "usr/sbin",
        "lib": "usr/lib",
        "lib64": "usr/lib64",
    }
    for name, target in expected.items():
        path = root / name
        if not path.is_symlink() or os.readlink(path) != target:
            raise ScriptError(
                f"Ubuntu Base usr-merge path /{name} must point to {target}"
            )
        resolved = path.resolve()
        root_resolved = root.resolve()
        if root_resolved not in resolved.parents or not resolved.is_dir():
            raise ScriptError(f"Ubuntu Base usr-merge target is invalid: /{name}")


def _customize_root(root: Path) -> None:
    for relative, mode in (
        ("root", 0o700),
        ("tmp", 0o1777),
        ("run", 0o755),
        ("nonexistent", 0o755),
    ):
        path = root / relative
        path.mkdir(parents=True, exist_ok=True)
        path.chmod(mode)
    resolver = root / "etc" / "resolv.conf"
    if _path_exists(resolver):
        resolver.unlink()
    resolver.touch(mode=0o644)
    for relative in (
        "var/cache/apt/archives",
        "var/lib/apt/lists",
        "var/log",
        "tmp",
        "var/tmp",
    ):
        _clear_directory(root / relative)
    for relative in (
        "etc/machine-id",
        "var/lib/dbus/machine-id",
        "var/lib/systemd/random-seed",
    ):
        path = root / relative
        if _path_exists(path):
            path.unlink()
    ssh = root / "etc" / "ssh"
    if ssh.is_dir():
        for host_key in ssh.glob("ssh_host_*"):
            host_key.unlink()
    _ensure_symlink(root, "usr/bin/nc", "nc.openbsd")
    _ensure_symlink(root, "usr/bin/netcat", "nc.openbsd")
    _ensure_symlink(root, "usr/sbin/mdev", "../bin/busybox")
    _validate_accounts(root)
    _validate_usr_merge(root)


def _validate_ubuntu_identity(root: Path) -> None:
    records = parse_deb822(
        (root / "var" / "lib" / "dpkg" / "status").read_text(encoding="utf-8")
    )
    if not records:
        raise ScriptError("Ubuntu Base package database is empty")
    os_release: dict[str, str] = {}
    for line in (root / "etc" / "os-release").read_text(encoding="utf-8").splitlines():
        key, separator, value = line.partition("=")
        if separator:
            os_release[key] = value.strip('"')
    if (
        os_release.get("ID") != "ubuntu"
        or DEFAULT_UBUNTU_VERSION not in os_release.get("VERSION", "")
        or os_release.get("VERSION_CODENAME") != DEFAULT_UBUNTU_CODENAME
    ):
        raise ScriptError(
            "Ubuntu Base /etc/os-release does not match the pinned release"
        )


def prepare_root(work: Path) -> Path:
    work.mkdir(parents=True, exist_ok=True)
    downloads = cache_root() / "downloads"
    downloads.mkdir(parents=True, exist_ok=True)
    archive = downloads / f"ubuntu-base-{DEFAULT_UBUNTU_VERSION}-base-amd64.tar.gz"
    download_verified(
        DEFAULT_UBUNTU_BASE_URL,
        archive,
        DEFAULT_UBUNTU_BASE_SHA256,
    )
    root = work / "root"
    if root.exists():
        shutil.rmtree(root)
    root.mkdir()
    safe_extract_tar(archive, root, label="Ubuntu Base archive")
    _validate_ubuntu_identity(root)

    packages = load_package_lock()
    base_records = parse_deb822(
        (root / "var" / "lib" / "dpkg" / "status").read_text(encoding="utf-8")
    )
    base_packages = {
        record["Package"]
        for record in base_records
        if record.get("Status") == "install ok installed" and "Package" in record
    }
    duplicate = sorted(base_packages & {package["name"] for package in packages})
    if duplicate:
        raise ScriptError(
            "Ubuntu supplemental package lock duplicates Ubuntu Base: "
            + ", ".join(duplicate)
        )
    _validate_package_closure(base_packages, packages)

    package_downloads = downloads / "ubuntu-packages"
    package_downloads.mkdir(exist_ok=True)
    for package in packages:
        filename = package["url"].rsplit("/", maxsplit=1)[-1]
        package_path = package_downloads / filename
        download_verified(package["url"], package_path, package["sha256"])
        _install_deb(root, package, package_path)
    _customize_root(root)
    return root


def apply_metadata_policy(root: Path) -> None:
    listxattr = getattr(os, "listxattr", None)
    for path in (*root.rglob("*"), root):
        metadata = path.lstat()
        mode = metadata.st_mode
        if not (stat.S_ISREG(mode) or stat.S_ISDIR(mode) or stat.S_ISLNK(mode)):
            raise ScriptError(f"Ubuntu rootfs contains unsupported inode {path}")
        if not stat.S_ISLNK(mode) and mode & (stat.S_ISUID | stat.S_ISGID):
            path.chmod(stat.S_IMODE(mode) & ~(stat.S_ISUID | stat.S_ISGID))
        if listxattr is None:
            continue
        try:
            attributes = cast(list[str], listxattr(path, follow_symlinks=False))
        except OSError as error:
            if path.is_symlink():
                continue
            raise ScriptError(
                f"failed to inspect xattrs for {path}: {error}"
            ) from error
        if attributes:
            raise ScriptError(
                f"Ubuntu rootfs metadata policy rejects xattrs on {path}: "
                + ", ".join(sorted(attributes))
            )


def rootfs_sha256(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(
        root.rglob("*"), key=lambda item: item.relative_to(root).as_posix()
    ):
        relative = path.relative_to(root).as_posix()
        metadata = path.lstat()
        mode = stat.S_IMODE(metadata.st_mode)
        if path.is_symlink():
            kind = "symlink"
            payload = os.readlink(path).encode("utf-8")
        elif path.is_dir():
            kind = "directory"
            payload = b""
        elif path.is_file():
            kind = "file"
            file_digest = hashlib.sha256()
            with path.open("rb") as source:
                while chunk := source.read(1024 * 1024):
                    file_digest.update(chunk)
            payload = file_digest.digest()
        else:
            raise ScriptError(f"cannot fingerprint unsupported inode {path}")
        digest.update(f"{kind}\0{mode:o}\0{relative}\0".encode() + payload + b"\0")
    return digest.hexdigest()


def rootfs_inventory(root: Path) -> dict[str, dict[str, object]]:
    inventory: dict[str, dict[str, object]] = {}
    for path in sorted(
        root.rglob("*"), key=lambda item: item.relative_to(root).as_posix()
    ):
        relative = path.relative_to(root).as_posix()
        metadata = path.lstat()
        record: dict[str, object] = {
            "mode": stat.S_IMODE(metadata.st_mode),
            "uid": metadata.st_uid,
            "gid": metadata.st_gid,
            "mtime_ns": metadata.st_mtime_ns,
        }
        if path.is_symlink():
            record["type"] = "symlink"
            record["target"] = os.readlink(path)
        elif path.is_dir():
            record["type"] = "directory"
        elif path.is_file():
            record["type"] = "file"
            record["size"] = metadata.st_size
            record["sha256"] = sha256_file(path)
        else:
            record["type"] = "unsupported"
        inventory[relative] = record
    return inventory


def _license_path(root: Path, package_name: str, declared: str | None) -> str:
    relative = (declared or f"/usr/share/doc/{package_name}/copyright").lstrip("/")
    path = root / relative
    if not path.exists():
        raise ScriptError(
            f"Ubuntu package {package_name} license metadata is missing: /{relative}"
        )
    resolved = path.resolve()
    root_resolved = root.resolve()
    if root_resolved not in resolved.parents or not resolved.is_file():
        raise ScriptError(
            f"Ubuntu package {package_name} license metadata escapes the rootfs"
        )
    return f"/{relative}"


def package_records(root: Path) -> list[UbuntuPackageRecord]:
    locked = {package["name"]: package for package in load_package_lock()}
    records = parse_deb822(
        (root / "var" / "lib" / "dpkg" / "status").read_text(encoding="utf-8")
    )
    packages: list[UbuntuPackageRecord] = []
    identities: set[tuple[str, str]] = set()
    for record in records:
        if record.get("Status") != "install ok installed":
            continue
        name = record.get("Package")
        version = record.get("Version")
        architecture = record.get("Architecture")
        if (
            not name
            or not version
            or architecture
            not in (
                DEFAULT_UBUNTU_ARCHITECTURE,
                "all",
            )
        ):
            raise ScriptError("Ubuntu package database contains an invalid record")
        identity = (name, version)
        if identity in identities:
            raise ScriptError(f"Ubuntu package database duplicates {name}-{version}")
        identities.add(identity)
        source_name, source_version = _source_identity(record)
        supplemental = locked.get(name)
        if supplemental is not None and (
            version != supplemental["version"]
            or source_name != supplemental["source_name"]
            or source_version != supplemental["source_version"]
        ):
            raise ScriptError(f"installed supplemental package is stale: {name}")
        packages.append(
            {
                "name": name,
                "version": version,
                "architecture": architecture,
                "source_name": source_name,
                "source_version": source_version,
                "installed": True,
                "deb_sha256": (
                    supplemental["sha256"] if supplemental is not None else None
                ),
                "license_path": _license_path(
                    root,
                    name,
                    supplemental["license_path"] if supplemental is not None else None,
                ),
                "origin": (
                    "supplemental-lock" if supplemental is not None else "ubuntu-base"
                ),
            }
        )
    packages.sort(key=lambda package: package["name"])
    if set(locked) - {package["name"] for package in packages}:
        raise ScriptError("Ubuntu package database is missing supplemental packages")
    return packages


def package_manifest(
    root: Path,
    helpers: HelperProvenance,
) -> dict[str, object]:
    return {
        "format": 1,
        "guest": "ubuntu",
        "release": DEFAULT_UBUNTU_VERSION,
        "architecture": DEFAULT_UBUNTU_ARCHITECTURE,
        "rootfs_sha256": rootfs_sha256(root),
        "packages": package_records(root),
        "helpers": helpers,
    }


def converter_input_sha256(customization_files: Sequence[Path]) -> str:
    lock = load_package_lock()
    document = {
        "domain": "nvx-ubuntu-erofs",
        "format": UBUNTU_EROFS_FORMAT,
        "release": DEFAULT_UBUNTU_VERSION,
        "architecture": DEFAULT_UBUNTU_ARCHITECTURE,
        "base_sha256": DEFAULT_UBUNTU_BASE_SHA256,
        "supplemental_packages": [
            {"name": package["name"], "sha256": package["sha256"]} for package in lock
        ],
        "customization_files": [
            {
                "path": path.relative_to(REPO_ROOT).as_posix(),
                "sha256": sha256_file(path),
            }
            for path in sorted(customization_files)
        ],
    }
    encoded = json.dumps(
        document,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def erofs_uuid(input_sha256: str) -> str:
    if _SHA256.fullmatch(input_sha256) is None:
        raise ScriptError("EROFS input digest must be a lowercase SHA-256")
    return str(uuid.UUID(bytes=bytes.fromhex(input_sha256)[:16]))

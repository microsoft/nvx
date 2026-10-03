"""Package lock and build-input identity for the Docker-built Azure Linux guest."""

from __future__ import annotations

import hashlib
import json
import re
from pathlib import Path
from typing import TypedDict, cast

from .build_constants import (
    AzureLinuxBuildConstants,
    BuildConstants,
    DockerBuildConstants,
)
from .common import ScriptError, download_verified, sha256_file

_RPM_FIELD = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+~^-]*")
_SHA256 = re.compile(r"[0-9a-f]{64}")
_LOCK_FIELDS = ("name", "version", "release", "architecture", "url", "sha256")


class AzureLinuxLockedPackage(TypedDict):
    name: str
    version: str
    release: str
    architecture: str
    url: str
    sha256: str


def package_lock_path() -> Path:
    return (
        BuildConstants.REPO_ROOT / AzureLinuxBuildConstants.PACKAGE_LOCK_RELATIVE_PATH
    )


def load_package_lock(path: Path | None = None) -> tuple[AzureLinuxLockedPackage, ...]:
    """Return the checksum-pinned RPMs that the guest adds to the base image."""
    path = path or package_lock_path()
    try:
        raw_document: object = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ScriptError(f"failed to read Azure Linux package lock: {path}") from error
    if not isinstance(raw_document, dict):
        raise ScriptError(f"{path} must contain a JSON object")
    document = cast(dict[str, object], raw_document)
    expected_header = {
        "format": AzureLinuxBuildConstants.PACKAGE_LOCK_FORMAT,
        "release": AzureLinuxBuildConstants.VERSION,
        "architecture": AzureLinuxBuildConstants.ARCHITECTURE,
        "image": AzureLinuxBuildConstants.IMAGE,
    }
    for field, expected in expected_header.items():
        if document.get(field) != expected:
            raise ScriptError(f"{path} {field} must be {expected!r}")
    raw_packages = document.get("packages")
    if not isinstance(raw_packages, list) or not raw_packages:
        raise ScriptError(f"{path} must contain a nonempty packages array")
    packages: list[AzureLinuxLockedPackage] = []
    for raw_package in cast(list[object], raw_packages):
        if not isinstance(raw_package, dict):
            raise ScriptError(f"{path} contains a non-object package record")
        record = cast(dict[str, object], raw_package)
        fields: dict[str, str] = {}
        for field in _LOCK_FIELDS:
            value = record.get(field)
            if not isinstance(value, str) or not value:
                raise ScriptError(f"{path} package record has no valid {field}")
            fields[field] = value
        package = cast(AzureLinuxLockedPackage, fields)
        name = package["name"]
        for field in ("name", "version", "release", "architecture"):
            if _RPM_FIELD.fullmatch(package[field]) is None:
                raise ScriptError(f"{path} package {name} has an invalid {field}")
        if package["architecture"] not in (
            AzureLinuxBuildConstants.ARCHITECTURE,
            "noarch",
        ):
            raise ScriptError(f"{path} package {name} has an unsupported architecture")
        filename = (
            f"{name}-{package['version']}-{package['release']}."
            f"{package['architecture']}.rpm"
        )
        expected_url = (
            f"{AzureLinuxBuildConstants.REPOSITORY_URL}/Packages/"
            f"{name[0].lower()}/{filename}"
        )
        if package["url"] != expected_url:
            raise ScriptError(
                f"{path} package {name} must be fetched from {expected_url}"
            )
        if _SHA256.fullmatch(package["sha256"]) is None:
            raise ScriptError(f"{path} package {name} has an invalid SHA-256")
        packages.append(package)
    names = [package["name"] for package in packages]
    if names != sorted(set(names)):
        raise ScriptError(f"{path} package records must be unique and sorted by name")
    return tuple(packages)


def package_lock_sha256(path: Path | None = None) -> str:
    path = path or package_lock_path()
    contents = path.read_bytes().replace(b"\r\n", b"\n")
    if b"\r" in contents:
        raise ScriptError(f"{path} contains unsupported carriage returns")
    return hashlib.sha256(contents).hexdigest()


def download_packages(destination: Path) -> tuple[Path, ...]:
    """Download every locked RPM into destination and verify its SHA-256."""
    downloaded: list[Path] = []
    for package in load_package_lock():
        path = destination / package["url"].rsplit("/", maxsplit=1)[-1]
        download_verified(package["url"], path, package["sha256"])
        downloaded.append(path)
    return tuple(downloaded)


def input_files() -> tuple[Path, ...]:
    """Return the checkout files that define the Azure Linux initramfs."""
    root = BuildConstants.REPO_ROOT
    files = [
        root / DockerBuildConstants.DOCKERFILE,
        package_lock_path(),
        # The Docker build runs these modules to download the RPMs and record
        # this digest, so changes to them must also rebuild the artifact.
        *(
            root / "scripts" / "nvx_tools" / name
            for name in ("azurelinux.py", "build_constants.py", "common.py")
        ),
    ]
    for directory in AzureLinuxBuildConstants.GUEST_SOURCE_DIRECTORIES:
        files.extend(path for path in (root / directory).rglob("*") if path.is_file())
    return tuple(sorted(files, key=lambda path: path.relative_to(root).as_posix()))


def input_sha256(
    *,
    image: str = AzureLinuxBuildConstants.IMAGE,
    version: str = AzureLinuxBuildConstants.VERSION,
) -> str:
    """Return the aggregate digest of the Azure Linux initramfs build inputs."""
    root = BuildConstants.REPO_ROOT
    document = {
        "domain": AzureLinuxBuildConstants.INPUT_DIGEST_DOMAIN,
        "format": AzureLinuxBuildConstants.INPUT_DIGEST_FORMAT,
        "version": version,
        "architecture": AzureLinuxBuildConstants.ARCHITECTURE,
        "image": image,
        "files": [
            {
                "path": path.relative_to(root).as_posix(),
                "sha256": sha256_file(path),
            }
            for path in input_files()
        ],
    }
    encoded = json.dumps(document, sort_keys=True, separators=(",", ":")).encode(
        "utf-8"
    )
    return hashlib.sha256(encoded).hexdigest()

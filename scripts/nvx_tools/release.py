"""Source collection, verification, and release packaging workflows."""

from __future__ import annotations

import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import cast

from .archive import create_reproducible_tar_gz
from .build import (
    DEFAULT_ALPINE_BRANCH,
    DEFAULT_ALPINE_MINIROOTFS_SHA256,
    DEFAULT_ALPINE_VERSION,
    DEFAULT_KERNEL_SHA256,
    DEFAULT_KERNEL_URL,
    DEFAULT_KERNEL_VERSION,
    GUEST_AGENT_ARTIFACT_NAME,
    GUEST_AGENT_SHA256_NAME,
    GUEST_AGENT_TARGET,
    OPENVMM_PROVENANCE_NAME,
    REQUIRED_SANDBOX_KERNEL_CONFIG,
    DockerBuildConfig,
    build_docker_linux_source,
    validate_static_x86_64_elf,
)
from .collect_alpine_sources import collect_alpine_sources
from .common import (
    OPENVMM_DIR,
    REPO_ROOT,
    SOURCE_DIR,
    ScriptError,
    artifact_path,
    download,
    openvmm_binary_path,
    require_file,
    sha256_file,
    verify_sha256_sums,
    write_sha256_sums,
)

PROJECT_SOURCE_PATHS = (
    "alpine",
    "data/linux-kvm-baremetal.csv",
    "data/linux-kvm-virtual-machine.csv",
    "data/linux-mshv-baremetal.csv",
    "data/linux-mshv-virtual-machine.csv",
    "data/windows-whp-baremetal.csv",
    "data/windows-whp-virtual-machine.csv",
    "docker",
    "kernel",
    "licenses",
    "scripts",
    ".dockerignore",
    ".gitattributes",
    ".gitmodules",
    ".gitignore",
    "LICENSE",
    "README.md",
    "SOURCE-MANIFEST.json",
    "THIRD_PARTY_NOTICES.md",
    "VERSION",
    "pyproject.toml",
    "requirements-dev.txt",
)

GUEST_RELEASE_NAMES = (
    "vmlinux",
    "vmlinux.config",
    "initramfs.cpio.gz",
    "initramfs.cpio.gz.packages.json",
)
GITHUB_API_VERSION = "2022-11-28"


@dataclass(frozen=True)
class _ReleaseAsset:
    tag: str
    name: str
    url: str
    size: int


def _github_headers(token: str | None, accept: str) -> dict[str, str]:
    headers = {
        "Accept": accept,
        "X-GitHub-Api-Version": GITHUB_API_VERSION,
        "User-Agent": "nvx-release-downloader",
    }
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def _latest_release_asset(
    repository: str,
    platform: str,
    token: str | None,
) -> _ReleaseAsset:
    repository_parts = repository.split("/")
    if len(repository_parts) != 2 or not all(repository_parts):
        raise ScriptError("GitHub repository must be OWNER/REPOSITORY")
    encoded_repository = "/".join(
        urllib.parse.quote(part, safe="") for part in repository_parts
    )
    url = f"https://api.github.com/repos/{encoded_repository}/releases?per_page=100"
    request = urllib.request.Request(
        url,
        headers=_github_headers(token, "application/vnd.github+json"),
    )
    try:
        with urllib.request.urlopen(request) as response:
            releases: object = json.load(response)
    except urllib.error.HTTPError as error:
        hint = ""
        if token is None and error.code in (401, 403, 404):
            hint = "; set GH_TOKEN to access private releases"
        raise ScriptError(
            f"GitHub release query failed with HTTP {error.code}{hint}"
        ) from error
    except (OSError, urllib.error.URLError) as error:
        raise ScriptError(f"GitHub release query failed: {error}") from error
    if not isinstance(releases, list):
        raise ScriptError("GitHub release query returned an invalid response")

    extension = ".zip" if platform.startswith("windows-") else ".tar.gz"
    asset_pattern = re.compile(rf"^nvx-.+-{re.escape(platform)}{re.escape(extension)}$")
    for release_value in cast(list[object], releases):
        if not isinstance(release_value, dict):
            continue
        release = cast(dict[str, object], release_value)
        if release.get("draft") is True:
            continue
        tag = release.get("tag_name")
        assets = release.get("assets")
        if not isinstance(tag, str) or not isinstance(assets, list):
            continue
        for asset_value in cast(list[object], assets):
            if not isinstance(asset_value, dict):
                continue
            asset = cast(dict[str, object], asset_value)
            name = asset.get("name")
            asset_url = asset.get("url")
            size = asset.get("size")
            if (
                isinstance(name, str)
                and asset_pattern.fullmatch(name) is not None
                and isinstance(asset_url, str)
                and isinstance(size, int)
            ):
                return _ReleaseAsset(tag, name, asset_url, size)
    raise ScriptError(f"no GitHub release contains an NVX package for {platform}")


def _validate_archive_member(name: str) -> None:
    path = PurePosixPath(name)
    if (
        not name
        or "\\" in name
        or path.is_absolute()
        or ".." in path.parts
        or (path.parts and path.parts[0].endswith(":"))
    ):
        raise ScriptError(f"unsafe path in release archive: {name}")


def _extract_release_archive(archive_path: Path, destination: Path) -> None:
    try:
        if archive_path.name.endswith(".tar.gz"):
            with tarfile.open(archive_path, "r:gz") as archive:
                members = archive.getmembers()
                for member in members:
                    _validate_archive_member(member.name)
                    if not (member.isfile() or member.isdir()):
                        raise ScriptError(
                            f"unsupported entry in release archive: {member.name}"
                        )
                archive.extractall(destination)
            return
        if archive_path.suffix == ".zip":
            with zipfile.ZipFile(archive_path) as archive:
                for member in archive.infolist():
                    _validate_archive_member(member.filename)
                    file_type = (member.external_attr >> 16) & 0o170000
                    if file_type not in (0, stat.S_IFDIR, stat.S_IFREG):
                        raise ScriptError(
                            f"unsupported entry in release archive: {member.filename}"
                        )
                archive.extractall(destination)
            return
    except (tarfile.TarError, zipfile.BadZipFile) as error:
        raise ScriptError(
            f"invalid release archive {archive_path.name}: {error}"
        ) from error
    raise ScriptError(f"unsupported release archive: {archive_path.name}")


def _replace_runtime_file(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f"{destination.name}.part")
    temporary.unlink(missing_ok=True)
    try:
        shutil.copy2(source, temporary)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)


def _control_contract(manifest: object, description: str) -> tuple[int, int, str]:
    if not isinstance(manifest, dict):
        raise ScriptError(f"{description} source manifest is invalid")
    document = cast(dict[str, object], manifest)
    openvmm_value = document.get("openvmm")
    if not isinstance(openvmm_value, dict):
        raise ScriptError(f"{description} source manifest has no OpenVMM contract")
    openvmm = cast(dict[str, object], openvmm_value)
    abi_version = openvmm.get("microvm_abi_version")
    protocol_version = openvmm.get("control_session_protocol_version")
    contract_revision = openvmm.get("control_contract_revision")
    if (
        not isinstance(abi_version, int)
        or isinstance(abi_version, bool)
        or not isinstance(protocol_version, int)
        or isinstance(protocol_version, bool)
        or not isinstance(contract_revision, str)
        or not contract_revision
    ):
        raise ScriptError(f"{description} OpenVMM control contract is invalid")
    return abi_version, protocol_version, contract_revision


def _install_release_archive(archive_path: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="nvx-release-") as temporary:
        extraction_root = Path(temporary)
        _extract_release_archive(archive_path, extraction_root)
        checksum_files = list(extraction_root.glob("*/SHA256SUMS"))
        if len(checksum_files) != 1:
            raise ScriptError(
                "release archive must contain one package root with SHA256SUMS"
            )
        package_root = checksum_files[0].parent
        verify_sha256_sums(package_root)

        binary_destination = openvmm_binary_path()
        binary_source = require_file(
            package_root / "bin" / binary_destination.name,
            "packaged OpenVMM binary",
        )
        guest_sources = {
            name: require_file(
                package_root / "guest" / name,
                f"packaged guest artifact {name}",
            )
            for name in GUEST_RELEASE_NAMES
        }
        runtime_manifest = json.loads(
            require_file(
                package_root / "SOURCE-MANIFEST.json",
                "packaged source manifest",
            ).read_text(encoding="utf-8")
        )
        packaged_contract = _control_contract(runtime_manifest, "packaged")
        local_manifest = json.loads(
            require_file(
                REPO_ROOT / "SOURCE-MANIFEST.json",
                "local source manifest",
            ).read_text(encoding="utf-8")
        )
        if packaged_contract != _control_contract(local_manifest, "local"):
            raise ScriptError(
                "packaged OpenVMM control contract does not match the local checkout"
            )
        try:
            openvmm_manifest = runtime_manifest["openvmm"]
            linux_manifest = runtime_manifest["linux"]
            alpine_manifest = runtime_manifest["alpine"]
            agent_manifest = runtime_manifest["guest_agent"]
            source_revision = openvmm_manifest["source_revision"]
            expected_agent_sha256 = agent_manifest["sha256"]
            expected_agent_size = agent_manifest["size"]
        except (KeyError, TypeError) as error:
            raise ScriptError(
                "packaged source manifest is missing required runtime identity fields"
            ) from error
        if (
            not isinstance(source_revision, str)
            or len(source_revision) != 40
            or re.fullmatch(r"[0-9a-f]{40}", source_revision) is None
        ):
            raise ScriptError("packaged OpenVMM source revision is invalid")
        expected_runtime_hashes = {
            binary_source: openvmm_manifest.get("executable_sha256"),
            guest_sources["vmlinux"]: linux_manifest.get("kernel_sha256"),
            guest_sources["initramfs.cpio.gz"]: alpine_manifest.get("initramfs_sha256"),
        }
        for source, expected in expected_runtime_hashes.items():
            actual = sha256_file(source)
            if expected != actual:
                raise ScriptError(
                    f"packaged identity for {source.name} is {expected}, actual {actual}"
                )
        agent_source = package_root / "guest" / GUEST_AGENT_ARTIFACT_NAME
        agent_sha256: str | None = None
        agent_present = agent_source.is_file()
        agent_declared = expected_agent_sha256 is not None
        if agent_present != agent_declared:
            raise ScriptError(
                "packaged NVX guest-agent presence does not match SOURCE-MANIFEST.json"
            )
        if agent_source.is_file():
            agent_sha256 = sha256_file(agent_source)
            if expected_agent_sha256 != agent_sha256:
                raise ScriptError(
                    "packaged NVX guest-agent identity does not match "
                    "SOURCE-MANIFEST.json"
                )
            if expected_agent_size != agent_source.stat().st_size:
                raise ScriptError(
                    "packaged NVX guest-agent size does not match SOURCE-MANIFEST.json"
                )
            validate_static_x86_64_elf(agent_source)
        elif expected_agent_size is not None:
            raise ScriptError(
                "packaged NVX guest-agent size is present without an artifact"
            )
        provenance = {
            "format": 1,
            "source_revision": source_revision,
            "source_clean": True,
            "executable_sha256": sha256_file(binary_source),
            "origin": "release",
        }

        _replace_runtime_file(binary_source, binary_destination)
        for name, source in guest_sources.items():
            _replace_runtime_file(source, artifact_path(name))
        agent_destination = artifact_path(GUEST_AGENT_ARTIFACT_NAME)
        agent_pin = artifact_path(GUEST_AGENT_SHA256_NAME)
        if agent_sha256 is not None:
            _replace_runtime_file(agent_source, agent_destination)
            agent_pin.write_text(f"{agent_sha256}\n", encoding="ascii")
        else:
            agent_destination.unlink(missing_ok=True)
            agent_pin.unlink(missing_ok=True)
        provenance_path = artifact_path(OPENVMM_PROVENANCE_NAME)
        provenance_path.write_text(
            json.dumps(provenance, indent=2) + "\n",
            encoding="utf-8",
        )


def download_latest_release(repository: str, platform: str) -> None:
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    asset = _latest_release_asset(repository, platform, token)
    print(f">> downloading {asset.name} from {asset.tag}")
    with tempfile.TemporaryDirectory(prefix="nvx-download-") as temporary:
        archive_path = Path(temporary) / asset.name
        download(
            asset.url,
            archive_path,
            headers=_github_headers(token, "application/octet-stream"),
        )
        actual_size = archive_path.stat().st_size
        if actual_size != asset.size:
            raise ScriptError(
                f"downloaded {asset.name} is {actual_size} bytes, expected {asset.size}"
            )
        _install_release_archive(archive_path)
    print(f">> installed {asset.tag} for {platform}")


def _copy_release_file(source: Path, destination: Path) -> None:
    require_file(source, source.name)
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)


def _publish_release_directory(staged: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    if not destination.exists():
        staged.replace(destination)
        return

    backup = destination.with_name(f".{destination.name}.previous")
    if backup.exists():
        raise ScriptError(f"stale release backup blocks publication: {backup}")
    destination.replace(backup)
    try:
        staged.replace(destination)
    except OSError:
        backup.replace(destination)
        raise
    shutil.rmtree(backup)


def _validate_alpine_sources(package_manifests: list[Path]) -> None:
    source_root = SOURCE_DIR / "alpine"
    source_manifest_path = require_file(
        source_root / "manifest.json",
        "collected Alpine source manifest",
    )
    source_manifest = json.loads(source_manifest_path.read_text(encoding="utf-8"))
    collected = {
        (package["package"], package["version"], package["commit"]): package
        for package in source_manifest["packages"]
    }
    missing: list[str] = []
    for manifest_path in package_manifests:
        package_manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        for package in package_manifest["packages"]:
            key = (
                package["name"],
                package["version"],
                package["aports_commit"],
            )
            if key not in collected:
                missing.append(f"{key[0]}-{key[1]} ({key[2]})")
    if missing:
        raise ScriptError(
            "collected Alpine sources do not cover packaged APKs: "
            + ", ".join(sorted(missing))
        )
    for package in collected.values():
        for field in ("recipe", "source_directory"):
            path = source_root / package[field]
            if not path.exists():
                raise ScriptError(
                    f"collected Alpine {field} is missing for "
                    f"{package['package']}: {path}"
                )
    verify_sha256_sums(source_root)


def _create_source_archive(
    output: Path,
    inputs: list[tuple[Path, str]],
) -> None:
    print(f">> creating source archive {output}")
    create_reproducible_tar_gz(output, inputs)


def _project_source_archive(
    output: Path,
    version: str,
    package_manifests: list[Path],
) -> None:
    root = f"nvx-project-source-{version}"
    inputs = [
        (REPO_ROOT / relative, f"{root}/{relative}")
        for relative in PROJECT_SOURCE_PATHS
    ]
    inputs.extend(
        (
            manifest,
            f"{root}/build/{manifest.name}",
        )
        for manifest in package_manifests
    )
    inputs.append((artifact_path("vmlinux.config"), f"{root}/build/vmlinux.config"))
    _create_source_archive(output, inputs)


def _alpine_source_archive(
    output: Path,
    version: str,
    package_manifests: list[Path],
) -> None:
    root = f"nvx-alpine-source-{version}"
    inputs = [(SOURCE_DIR / "alpine", f"{root}/sources")]
    inputs.extend(
        (
            manifest,
            f"{root}/manifests/{manifest.name}",
        )
        for manifest in package_manifests
    )
    _create_source_archive(output, inputs)


def _validate_linux_source_archive(path: Path) -> None:
    expected_members = {
        "vmlinux.config": artifact_path("vmlinux.config").read_bytes(),
        "SOURCE-MANIFEST.json": (REPO_ROOT / "SOURCE-MANIFEST.json").read_bytes(),
    }
    manifest = json.loads(expected_members["SOURCE-MANIFEST.json"])
    expected_members.update(
        {
            patch: (REPO_ROOT / patch).read_bytes()
            for patch in manifest["linux"]["patches"]
        }
    )
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        names = [member.name for member in members]
        if not any(
            name.endswith(f"linux-{DEFAULT_KERNEL_VERSION}/drivers/tty/hvc/hvc_xe9.c")
            for name in names
        ):
            raise ScriptError(f"{path} does not contain the patched xe9 HVC driver")
        for suffix, expected in expected_members.items():
            matches = [member for member in members if member.name.endswith(suffix)]
            if len(matches) != 1:
                raise ScriptError(
                    f"{path} contains {len(matches)} members ending in {suffix}"
                )
            extracted = archive.extractfile(matches[0])
            if extracted is None or extracted.read() != expected:
                raise ScriptError(f"{path} has stale contents for {suffix}")


def _guest_release_inputs() -> tuple[list[str], list[Path]]:
    for name in GUEST_RELEASE_NAMES:
        require_file(artifact_path(name), f"required guest artifact {name}")
    guest_names: list[str] = list(GUEST_RELEASE_NAMES)
    agent = artifact_path(GUEST_AGENT_ARTIFACT_NAME)
    pin = artifact_path(GUEST_AGENT_SHA256_NAME)
    if agent.exists() or pin.exists():
        require_file(agent, "staged NVX guest agent")
        expected = (
            require_file(pin, "staged NVX guest-agent SHA-256 pin")
            .read_text(encoding="ascii")
            .strip()
        )
        actual = sha256_file(agent)
        if actual != expected:
            raise ScriptError(
                f"staged NVX guest-agent SHA-256 is {actual}, expected {expected}"
            )
        guest_names.append(GUEST_AGENT_ARTIFACT_NAME)
    package_manifests = [artifact_path("initramfs.cpio.gz.packages.json")]
    return guest_names, package_manifests


def _runtime_source_manifest(
    release_destination: Path,
    binary_name: str,
    guest_names: list[str],
) -> bytes:
    manifest = json.loads(
        (REPO_ROOT / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
    )
    manifest["openvmm"]["executable_sha256"] = sha256_file(
        release_destination / "bin" / binary_name
    )
    manifest["linux"]["kernel_sha256"] = sha256_file(
        release_destination / "guest" / "vmlinux"
    )
    manifest["alpine"]["initramfs_sha256"] = sha256_file(
        release_destination / "guest" / "initramfs.cpio.gz"
    )
    if GUEST_AGENT_ARTIFACT_NAME in guest_names:
        agent = release_destination / "guest" / GUEST_AGENT_ARTIFACT_NAME
        manifest["guest_agent"]["sha256"] = sha256_file(agent)
        manifest["guest_agent"]["size"] = agent.stat().st_size
    else:
        manifest["guest_agent"]["sha256"] = None
        manifest["guest_agent"]["size"] = None
    return (json.dumps(manifest, indent=2) + "\n").encode("utf-8")


def collect_release_sources() -> None:
    _guest_names, package_manifests = _guest_release_inputs()
    collect_alpine_sources(
        package_manifests,
        SOURCE_DIR / "alpine",
        REPO_ROOT / ".cache" / "aports",
    )
    build_docker_linux_source(DockerBuildConfig(destination=SOURCE_DIR / "linux"))
    print(f">> collected release sources under {SOURCE_DIR}")


def package_release(
    *,
    version: str | None,
    destination: Path | None,
    include_source: bool,
    force: bool,
) -> None:
    guest_names, package_manifests = _guest_release_inputs()
    linux_source_archive = (
        SOURCE_DIR / "linux" / f"nvx-linux-source-{DEFAULT_KERNEL_VERSION}.tar.gz"
    )
    if include_source:
        _validate_alpine_sources(package_manifests)
        require_file(linux_source_archive, "Linux corresponding-source archive")
        _validate_linux_source_archive(linux_source_archive)
    else:
        print(
            "!! binary-only package: publish matching Linux and Alpine "
            "corresponding source separately",
            file=sys.stderr,
        )

    release_version = (
        version or (REPO_ROOT / "VERSION").read_text(encoding="ascii").strip()
    )
    release_destination = (
        destination or REPO_ROOT / "dist" / release_version
    ).resolve()
    if release_destination.exists():
        if not force:
            raise ScriptError(
                f"release directory already exists: {release_destination}; "
                "pass --force to replace it"
            )
        release_root = (REPO_ROOT / "dist").resolve()
        if (
            release_destination == release_root
            or release_root not in release_destination.parents
        ):
            raise ScriptError(
                "--force may only replace a version directory below dist/"
            )
    binary = openvmm_binary_path()
    provenance_path = require_file(
        artifact_path(OPENVMM_PROVENANCE_NAME),
        "OpenVMM build provenance",
    )
    provenance = json.loads(provenance_path.read_text(encoding="utf-8"))
    source_manifest = json.loads(
        (REPO_ROOT / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
    )
    expected_revision = source_manifest["openvmm"]["source_revision"]
    actual_binary_sha256 = sha256_file(binary)
    if (
        provenance.get("format") != 1
        or provenance.get("source_revision") != expected_revision
        or provenance.get("source_clean") is not True
        or provenance.get("executable_sha256") != actual_binary_sha256
    ):
        raise ScriptError(
            "OpenVMM build provenance does not match the clean pinned source and binary"
        )
    release_destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix=f".{release_destination.name}.staging-",
        dir=release_destination.parent,
    ) as temporary:
        staged_destination = Path(temporary) / release_destination.name
        _copy_release_file(binary, staged_destination / "bin" / binary.name)
        packaged_binary = staged_destination / "bin" / binary.name
        if sha256_file(packaged_binary) != provenance["executable_sha256"]:
            raise ScriptError(
                "packaged OpenVMM binary does not match its build provenance"
            )
        for name in guest_names:
            _copy_release_file(artifact_path(name), staged_destination / "guest" / name)
        if GUEST_AGENT_ARTIFACT_NAME in guest_names:
            packaged_agent = staged_destination / "guest" / GUEST_AGENT_ARTIFACT_NAME
            expected_agent_sha256 = (
                require_file(
                    artifact_path(GUEST_AGENT_SHA256_NAME),
                    "staged NVX guest-agent SHA-256 pin",
                )
                .read_text(encoding="ascii")
                .strip()
            )
            if sha256_file(packaged_agent) != expected_agent_sha256:
                raise ScriptError(
                    "packaged NVX guest agent does not match its staged pin"
                )
        for name in ("LICENSE", "README.md", "THIRD_PARTY_NOTICES.md"):
            _copy_release_file(REPO_ROOT / name, staged_destination / name)
        (staged_destination / "SOURCE-MANIFEST.json").write_bytes(
            _runtime_source_manifest(staged_destination, binary.name, guest_names)
        )
        _copy_release_file(
            OPENVMM_DIR / "LICENSE",
            staged_destination / "licenses" / "LICENSE-OPENVMM",
        )
        _copy_release_file(
            REPO_ROOT / "kernel" / "COPYING-LINUX",
            staged_destination / "licenses" / "COPYING-LINUX",
        )
        if include_source:
            source_destination = staged_destination / "source"
            _copy_release_file(
                linux_source_archive,
                source_destination / linux_source_archive.name,
            )
            _project_source_archive(
                source_destination / f"nvx-project-source-{release_version}.tar.gz",
                release_version,
                package_manifests,
            )
            _alpine_source_archive(
                source_destination / f"nvx-alpine-source-{release_version}.tar.gz",
                release_version,
                package_manifests,
            )
        write_sha256_sums(staged_destination)
        _publish_release_directory(staged_destination, release_destination)
    print(f">> packaged {release_destination}")


def verify_source_tree() -> None:
    manifest = json.loads(
        (REPO_ROOT / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
    )
    required = {
        REPO_ROOT / "kernel" / "config-microvm": "kernel build configuration",
        REPO_ROOT
        / "kernel"
        / "patches"
        / "0001-microvm-xe9-earlycon.patch": "xe9 early console patch",
        REPO_ROOT
        / "kernel"
        / "patches"
        / "0002-microvm-hvc-xe9.patch": "xe9 HVC patch",
        REPO_ROOT / "kernel" / "COPYING-LINUX": "Linux copyright notice",
        OPENVMM_DIR / "Cargo.toml": "initialized OpenVMM submodule",
    }
    required.update(
        {
            REPO_ROOT / patch: f"kernel patch {patch}"
            for patch in manifest["linux"]["patches"]
        }
    )
    for path, description in required.items():
        require_file(path, description)
    forbidden = (
        REPO_ROOT / "third_party" / "linux",
        REPO_ROOT / "third_party" / "alpine-sources",
    )
    present = [str(path) for path in forbidden if path.exists()]
    if present:
        raise ScriptError(
            "generated third-party sources must not be checked out here: "
            + ", ".join(present)
        )
    linux_manifest = manifest["linux"]
    expected_linux = {
        "version": DEFAULT_KERNEL_VERSION,
        "upstream_url": DEFAULT_KERNEL_URL,
        "upstream_archive_sha256": DEFAULT_KERNEL_SHA256,
    }
    for field, expected in expected_linux.items():
        if linux_manifest.get(field) != expected:
            raise ScriptError(
                f"SOURCE-MANIFEST.json Linux {field} does not match the build pin"
            )
    alpine_manifest = manifest["alpine"]
    expected_alpine = {
        "version": DEFAULT_ALPINE_VERSION,
        "branch": DEFAULT_ALPINE_BRANCH,
        "minirootfs_sha256": DEFAULT_ALPINE_MINIROOTFS_SHA256,
    }
    for field, expected in expected_alpine.items():
        if alpine_manifest.get(field) != expected:
            raise ScriptError(
                f"SOURCE-MANIFEST.json Alpine {field} does not match the build pin"
            )
    config_path = REPO_ROOT / "kernel" / "config-microvm"
    config = config_path.read_text(encoding="utf-8")
    for setting in (
        "CONFIG_PVH=y",
        "CONFIG_HVC_XE9=y",
        "CONFIG_VIRTIO_FS=y",
        "CONFIG_FUSE_FS=y",
    ):
        if setting not in config.splitlines():
            raise ScriptError(f"{config_path} is missing {setting}")
    hvc_patch_text = (
        REPO_ROOT / "kernel" / "patches" / "0002-microvm-hvc-xe9.patch"
    ).read_text(encoding="utf-8")
    for marker in ("config HVC_XE9", "hvc_xe9.o", "hvc_xe9.c"):
        if marker not in hvc_patch_text:
            raise ScriptError(f"xe9 HVC patch is missing {marker}")
    generated_config = artifact_path("vmlinux.config")
    if generated_config.is_file():
        generated = generated_config.read_text(encoding="utf-8").splitlines()
        for setting in (
            "CONFIG_PVH=y",
            "CONFIG_HVC_XE9=y",
            *REQUIRED_SANDBOX_KERNEL_CONFIG,
        ):
            if setting not in generated:
                raise ScriptError(f"{generated_config} is missing {setting}")
    head = subprocess.run(
        ["git", "-C", OPENVMM_DIR, "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    expected = subprocess.run(
        ["git", "-C", REPO_ROOT, "rev-parse", ":openvmm"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    if head != expected:
        raise ScriptError(f"OpenVMM submodule is at {head}, expected {expected}")
    openvmm_manifest = manifest["openvmm"]
    if openvmm_manifest.get("source_revision") != expected:
        raise ScriptError(
            "SOURCE-MANIFEST.json OpenVMM source revision does not match the submodule"
        )
    expected_agent = {
        "artifact": f"guest/{GUEST_AGENT_ARTIFACT_NAME}",
        "target": GUEST_AGENT_TARGET,
        "optional": True,
        "installed_in_initramfs": False,
        "protocol_schema_version": 1,
    }
    agent_manifest = manifest["guest_agent"]
    for field, value in expected_agent.items():
        if agent_manifest.get(field) != value:
            raise ScriptError(
                f"SOURCE-MANIFEST.json guest-agent {field} does not match the build contract"
            )
    print(">> source tree and submodule metadata are consistent")

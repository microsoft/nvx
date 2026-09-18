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
import uuid
import zipfile
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import cast

from .archive import create_reproducible_release_archive, create_reproducible_tar_gz
from .build import (
    CONTROL_CONTRACT_REVISION,
    CONTROL_SESSION_PROTOCOL_VERSION,
    DEFAULT_ALPINE_BRANCH,
    DEFAULT_ALPINE_MINIROOTFS_SHA256,
    DEFAULT_ALPINE_VERSION,
    DEFAULT_KERNEL_SHA256,
    DEFAULT_KERNEL_URL,
    DEFAULT_KERNEL_VERSION,
    KERNEL_PROVENANCE_NAME,
    MICROVM_ABI_VERSION,
    OPENVMM_PROVENANCE_NAME,
    REQUIRED_SANDBOX_KERNEL_CONFIG,
    DockerBuildConfig,
    assert_required_kernel_config,
    build_docker_linux_source,
    kernel_provenance_inputs,
)
from .collect_alpine_sources import collect_alpine_sources
from .common import (
    OPENVMM_DIR,
    REPO_ROOT,
    SOURCE_DIR,
    ScriptError,
    artifact_path,
    credential_safe_opener,
    download,
    openvmm_binary_path,
    require_file,
    require_success,
    run_capture,
    sha256_file,
    verify_sha256_sums,
    write_sha256_sums,
)

PROJECT_SOURCE_PATHS = (
    "alpine",
    "data/linux-kvm-virtual-machine.csv",
    "data/linux-mshv-virtual-machine.csv",
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


class _GitHubReleaseQueryError(ScriptError):
    def __init__(self, status: int, message: str) -> None:
        self.status = status
        super().__init__(message)


class _ReleaseRestoreError(ScriptError):
    """Raised when release publication and restoration both fail."""


def create_release_archive(source: Path, destination: Path) -> None:
    source = source.resolve()
    destination = destination.resolve()
    if destination == source or source in destination.parents:
        raise ScriptError("release archive destination must be outside its source")
    verify_sha256_sums(source)
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = destination.with_name(f".staging-{uuid.uuid4().hex}-{destination.name}")
    try:
        create_reproducible_release_archive(source, staging)
        verify_sha256_sums(source)
        staging.replace(destination)
    finally:
        staging.unlink(missing_ok=True)
    print(f">> archived {source} as {destination}")


def _github_headers(token: str | None, accept: str) -> dict[str, str]:
    headers = {
        "Accept": accept,
        "X-GitHub-Api-Version": GITHUB_API_VERSION,
        "User-Agent": "nvx-release-downloader",
    }
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def _github_error_message(error: urllib.error.HTTPError) -> str:
    try:
        payload = error.read().decode("utf-8", "replace").strip()
    except (AttributeError, OSError, ValueError):
        return ""
    if not payload:
        return ""
    try:
        document: object = json.loads(payload)
    except json.JSONDecodeError:
        return " ".join(payload.split())[:200]
    if isinstance(document, dict):
        message = cast(dict[str, object], document).get("message")
        if isinstance(message, str):
            return message
    return ""


def _github_error_hint(error: urllib.error.HTTPError, token: str | None) -> str:
    if token is None and error.code in (401, 403, 404):
        return "set GH_TOKEN to a token that can read the repository"
    if error.code == 401:
        return "the configured GitHub token was rejected; refresh or replace it"
    if error.code == 403:
        if error.headers.get("x-ratelimit-remaining") == "0":
            return "the GitHub API rate limit is exhausted; retry later"
        return (
            "the configured GitHub token lacks access; grant it read access to "
            "the repository contents and authorize it for organization single sign-on"
        )
    if error.code == 404:
        return (
            "verify --repository and that the configured GitHub token can read "
            "that repository"
        )
    return ""


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
        message = _github_error_message(error)
        hint = _github_error_hint(error, token)
        detail = f": {message.rstrip('.')}" if message else ""
        advice = f"; {hint}" if hint else ""
        raise _GitHubReleaseQueryError(
            error.code,
            f"GitHub release query failed with HTTP {error.code}{detail}{advice}",
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


def _latest_release_asset_with_fallback(
    repository: str,
    platform: str,
    token: str | None,
) -> tuple[_ReleaseAsset, str | None]:
    if token is None:
        return _latest_release_asset(repository, platform, None), None

    try:
        return _latest_release_asset(repository, platform, token), token
    except _GitHubReleaseQueryError as authenticated_error:
        if authenticated_error.status not in (401, 403):
            raise
        print(
            f">> {authenticated_error}; retrying without credentials",
            file=sys.stderr,
        )
        try:
            return _latest_release_asset(repository, platform, None), None
        except _GitHubReleaseQueryError as public_error:
            raise authenticated_error from public_error


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
        provenance_sources = {
            name: require_file(
                package_root / "provenance" / name,
                f"packaged provenance artifact {name}",
            )
            for name in (OPENVMM_PROVENANCE_NAME, KERNEL_PROVENANCE_NAME)
        }
        _replace_runtime_file(binary_source, binary_destination)
        for name, source in guest_sources.items():
            _replace_runtime_file(source, artifact_path(name))
        for name, source in provenance_sources.items():
            _replace_runtime_file(source, artifact_path(name))


def download_latest_release(repository: str, platform: str) -> None:
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    asset, download_token = _latest_release_asset_with_fallback(
        repository,
        platform,
        token,
    )
    print(f">> downloading {asset.name} from {asset.tag}")
    with tempfile.TemporaryDirectory(prefix="nvx-download-") as temporary:
        archive_path = Path(temporary) / asset.name
        download(
            asset.url,
            archive_path,
            headers=_github_headers(download_token, "application/octet-stream"),
            opener=credential_safe_opener(),
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
    package_manifests = [artifact_path("initramfs.cpio.gz.packages.json")]
    return guest_names, package_manifests


def _read_json_object(path: Path, description: str) -> dict[str, object]:
    try:
        value: object = json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as error:
        raise ScriptError(f"invalid {description}: {error}") from error
    if not isinstance(value, dict):
        raise ScriptError(f"invalid {description}: expected a JSON object")
    return cast(dict[str, object], value)


def _openvmm_git_state() -> tuple[str, bool]:
    head = run_capture(["git", "-C", OPENVMM_DIR, "rev-parse", "HEAD"])
    require_success(head, "OpenVMM revision query")
    gitlink = run_capture(["git", "-C", REPO_ROOT, "rev-parse", ":openvmm"])
    require_success(gitlink, "OpenVMM gitlink query")
    status = run_capture(["git", "-C", OPENVMM_DIR, "status", "--porcelain"])
    require_success(status, "OpenVMM status query")
    revision = head.stdout.decode("ascii").strip()
    expected_revision = gitlink.stdout.decode("ascii").strip()
    if revision != expected_revision:
        raise ScriptError(
            f"OpenVMM submodule is at {revision}, expected {expected_revision}"
        )
    return revision, not status.stdout.strip()


def _validate_openvmm_provenance(
    binary: Path,
    provenance_path: Path,
) -> dict[str, object]:
    provenance = _read_json_object(provenance_path, "OpenVMM build provenance")
    revision, source_clean = _openvmm_git_state()
    if (
        provenance.get("format") != 1
        or provenance.get("source_revision") != revision
        or provenance.get("source_clean") is not True
        or not source_clean
        or provenance.get("executable_sha256") != sha256_file(binary)
    ):
        raise ScriptError(
            "OpenVMM build provenance does not match the current clean pinned "
            "source and executable"
        )
    return provenance


def _validate_kernel_provenance(
    kernel: Path,
    kernel_config: Path,
    provenance_path: Path,
) -> dict[str, object]:
    provenance = _read_json_object(provenance_path, "kernel build provenance")
    expected_inputs = kernel_provenance_inputs()
    if (
        provenance.get("format") != 1
        or provenance.get("source") != expected_inputs["source"]
        or provenance.get("input_config") != expected_inputs["input_config"]
        or provenance.get("kernel_sha256") != sha256_file(kernel)
        or provenance.get("config_sha256") != sha256_file(kernel_config)
    ):
        raise ScriptError(
            "kernel build provenance does not match the current source, config, "
            "and vmlinux"
        )
    assert_required_kernel_config(kernel_config)
    return provenance


def _packaged_source_manifest(
    root_manifest: dict[str, object],
    release_version: str,
    release_root: Path,
    openvmm_provenance: dict[str, object],
    binary_name: str,
) -> bytes:
    distribution = root_manifest.get("distribution")
    openvmm = root_manifest.get("openvmm")
    linux = root_manifest.get("linux")
    alpine = root_manifest.get("alpine")
    if not all(
        isinstance(section, dict) for section in (distribution, openvmm, linux, alpine)
    ):
        raise ScriptError("SOURCE-MANIFEST.json is missing a required object")
    distribution_section = cast(dict[str, object], distribution)
    openvmm_section = cast(dict[str, object], openvmm)
    linux_section = cast(dict[str, object], linux)
    alpine_section = cast(dict[str, object], alpine)
    distribution_section["version"] = release_version
    openvmm_section["source_revision"] = openvmm_provenance["source_revision"]
    openvmm_section["executable_sha256"] = sha256_file(
        release_root / "bin" / binary_name
    )
    linux_section["kernel_sha256"] = sha256_file(release_root / "guest" / "vmlinux")
    linux_section["config_sha256"] = sha256_file(
        release_root / "guest" / "vmlinux.config"
    )
    alpine_section["initramfs_sha256"] = sha256_file(
        release_root / "guest" / "initramfs.cpio.gz"
    )
    alpine_section["initramfs_package_manifest_sha256"] = sha256_file(
        release_root / "guest" / "initramfs.cpio.gz.packages.json"
    )
    return (json.dumps(root_manifest, indent=2, sort_keys=True) + "\n").encode("utf-8")


def _validate_root_manifest_contract(manifest: dict[str, object]) -> None:
    distribution = manifest.get("distribution")
    openvmm = manifest.get("openvmm")
    if not isinstance(distribution, dict):
        raise ScriptError("SOURCE-MANIFEST.json distribution must identify nvx")
    distribution_section = cast(dict[str, object], distribution)
    if distribution_section.get("name") != "nvx":
        raise ScriptError("SOURCE-MANIFEST.json distribution must identify nvx")
    expected_openvmm = {
        "microvm_abi_version": MICROVM_ABI_VERSION,
        "control_session_protocol_version": CONTROL_SESSION_PROTOCOL_VERSION,
        "control_contract_revision": CONTROL_CONTRACT_REVISION,
    }
    if not isinstance(openvmm, dict):
        raise ScriptError(
            "SOURCE-MANIFEST.json OpenVMM contract does not match the build contract"
        )
    openvmm_section = cast(dict[str, object], openvmm)
    for field, expected in expected_openvmm.items():
        if openvmm_section.get(field) != expected:
            raise ScriptError(
                "SOURCE-MANIFEST.json OpenVMM contract does not match the "
                "build contract"
            )
    if "source_revision" in openvmm_section:
        raise ScriptError(
            "root SOURCE-MANIFEST.json must not duplicate the OpenVMM gitlink revision"
        )
    if "guest_agent" in manifest:
        raise ScriptError(
            "root SOURCE-MANIFEST.json must not contain product guest-agent metadata"
        )


def _validate_source_manifest_metadata(
    manifest: dict[str, object],
    kernel_inputs: dict[str, object],
) -> list[str]:
    _validate_root_manifest_contract(manifest)
    if manifest.get("format") != 1:
        raise ScriptError("SOURCE-MANIFEST.json format must be 1")
    linux_value = manifest.get("linux")
    alpine_value = manifest.get("alpine")
    source_value = kernel_inputs.get("source")
    config_value = kernel_inputs.get("input_config")
    if not all(
        isinstance(section, dict)
        for section in (linux_value, alpine_value, source_value, config_value)
    ):
        raise ScriptError("SOURCE-MANIFEST.json is missing source metadata")
    linux = cast(dict[str, object], linux_value)
    alpine = cast(dict[str, object], alpine_value)
    source = cast(dict[str, object], source_value)
    input_config = cast(dict[str, object], config_value)
    source_patches = source.get("patches")
    if not isinstance(source_patches, list):
        raise ScriptError("kernel provenance source patches must be a list")
    patch_paths: list[str] = []
    for patch_value in cast(list[object], source_patches):
        if not isinstance(patch_value, dict):
            raise ScriptError("kernel provenance source patch must be an object")
        patch = cast(dict[str, object], patch_value)
        path = patch.get("path")
        if not isinstance(path, str):
            raise ScriptError("kernel provenance source patch path must be a string")
        patch_paths.append(path)
    expected_linux: dict[str, object] = {
        "version": DEFAULT_KERNEL_VERSION,
        "upstream_url": DEFAULT_KERNEL_URL,
        "upstream_archive_sha256": DEFAULT_KERNEL_SHA256,
        "source_cache": f".cache/linux/linux-{DEFAULT_KERNEL_VERSION}",
        "source_archive": (
            f"build/sources/linux/nvx-linux-source-{DEFAULT_KERNEL_VERSION}.tar.gz"
        ),
        "generated_final_config": "build/vmlinux.config",
        "input_config": input_config.get("path"),
        "patches": patch_paths,
    }
    for field, expected in expected_linux.items():
        if linux.get(field) != expected:
            raise ScriptError(
                f"SOURCE-MANIFEST.json Linux {field} does not match the build pin"
            )
    expected_alpine: dict[str, object] = {
        "version": DEFAULT_ALPINE_VERSION,
        "branch": DEFAULT_ALPINE_BRANCH,
        "architecture": "x86_64",
        "minirootfs_url": (
            "https://dl-cdn.alpinelinux.org/alpine/"
            f"{DEFAULT_ALPINE_BRANCH}/releases/x86_64/"
            f"alpine-minirootfs-{DEFAULT_ALPINE_VERSION}-x86_64.tar.gz"
        ),
        "minirootfs_sha256": DEFAULT_ALPINE_MINIROOTFS_SHA256,
        "guest_sources": "alpine",
        "package_manifests": "build/*.packages.json",
        "source_output": "build/sources/alpine",
    }
    for field, expected in expected_alpine.items():
        if alpine.get(field) != expected:
            raise ScriptError(
                f"SOURCE-MANIFEST.json Alpine {field} does not match the build pin"
            )
    return patch_paths


def _validate_release_replacement(destination: Path, force: bool) -> None:
    if not destination.exists():
        return
    if not force:
        raise ScriptError(
            f"release directory already exists: {destination}; "
            "pass --force to replace it"
        )
    release_root = (REPO_ROOT / "dist").resolve()
    if destination == release_root or release_root not in destination.parents:
        raise ScriptError("--force may only replace a version directory below dist/")


def _publish_release_directory(
    staging: Path,
    destination: Path,
    *,
    force: bool,
) -> None:
    backup = destination.with_name(f".{destination.name}.backup-{uuid.uuid4().hex}")
    moved_prior = False
    try:
        _validate_release_replacement(destination, force)
        if destination.exists():
            destination.replace(backup)
            moved_prior = True
        staging.replace(destination)
    except ScriptError:
        shutil.rmtree(staging)
        raise
    except OSError as publish_error:
        if moved_prior:
            try:
                backup.replace(destination)
            except OSError as restore_error:
                raise _ReleaseRestoreError(
                    "failed to publish the staged release and restore the prior "
                    f"release; prior release remains at {backup} and staged "
                    f"release remains at {staging}: {restore_error}"
                ) from publish_error
        shutil.rmtree(staging, ignore_errors=True)
        raise ScriptError(f"failed to publish staged release: {publish_error}") from (
            publish_error
        )
    if moved_prior:
        shutil.rmtree(backup)


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
    _validate_release_replacement(release_destination, force)
    binary = require_file(openvmm_binary_path(), "OpenVMM release binary")
    kernel = require_file(artifact_path("vmlinux"), "required guest artifact vmlinux")
    kernel_config = require_file(
        artifact_path("vmlinux.config"),
        "required guest artifact vmlinux.config",
    )
    openvmm_provenance_path = require_file(
        artifact_path(OPENVMM_PROVENANCE_NAME),
        "OpenVMM build provenance",
    )
    kernel_provenance_path = require_file(
        artifact_path(KERNEL_PROVENANCE_NAME),
        "kernel build provenance",
    )
    openvmm_provenance = _validate_openvmm_provenance(
        binary,
        openvmm_provenance_path,
    )
    kernel_provenance = _validate_kernel_provenance(
        kernel,
        kernel_config,
        kernel_provenance_path,
    )
    root_manifest_path = require_file(
        REPO_ROOT / "SOURCE-MANIFEST.json",
        "source manifest",
    )
    root_manifest = _read_json_object(root_manifest_path, "source manifest")
    _validate_source_manifest_metadata(root_manifest, kernel_provenance)
    for name in ("LICENSE", "README.md", "THIRD_PARTY_NOTICES.md"):
        require_file(REPO_ROOT / name, name)
    require_file(OPENVMM_DIR / "LICENSE", "OpenVMM license")
    require_file(REPO_ROOT / "kernel" / "COPYING-LINUX", "Linux copyright notice")

    release_destination.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(
        tempfile.mkdtemp(
            prefix=f".{release_destination.name}.staging-",
            dir=release_destination.parent,
        )
    )
    preserve_staging = False
    try:
        _copy_release_file(binary, staging / "bin" / binary.name)
        for name in guest_names:
            _copy_release_file(artifact_path(name), staging / "guest" / name)
        _copy_release_file(
            openvmm_provenance_path,
            staging / "provenance" / OPENVMM_PROVENANCE_NAME,
        )
        _copy_release_file(
            kernel_provenance_path,
            staging / "provenance" / KERNEL_PROVENANCE_NAME,
        )
        for name in ("LICENSE", "README.md", "THIRD_PARTY_NOTICES.md"):
            _copy_release_file(REPO_ROOT / name, staging / name)
        _copy_release_file(
            OPENVMM_DIR / "LICENSE",
            staging / "licenses" / "LICENSE-OPENVMM",
        )
        _copy_release_file(
            REPO_ROOT / "kernel" / "COPYING-LINUX",
            staging / "licenses" / "COPYING-LINUX",
        )
        if include_source:
            source_destination = staging / "source"
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
        packaged_binary = staging / "bin" / binary.name
        packaged_kernel = staging / "guest" / "vmlinux"
        packaged_config = staging / "guest" / "vmlinux.config"
        packaged_openvmm_provenance = staging / "provenance" / OPENVMM_PROVENANCE_NAME
        packaged_kernel_provenance = staging / "provenance" / KERNEL_PROVENANCE_NAME
        if sha256_file(packaged_binary) != openvmm_provenance["executable_sha256"]:
            raise ScriptError(
                "packaged OpenVMM executable does not match its build provenance"
            )
        if sha256_file(packaged_kernel) != kernel_provenance["kernel_sha256"]:
            raise ScriptError("packaged kernel does not match its build provenance")
        if sha256_file(packaged_config) != kernel_provenance["config_sha256"]:
            raise ScriptError(
                "packaged kernel config does not match its build provenance"
            )
        if (
            _read_json_object(
                packaged_openvmm_provenance,
                "packaged OpenVMM build provenance",
            )
            != openvmm_provenance
        ):
            raise ScriptError("packaged OpenVMM provenance changed while staging")
        if (
            _read_json_object(
                packaged_kernel_provenance,
                "packaged kernel build provenance",
            )
            != kernel_provenance
        ):
            raise ScriptError("packaged kernel provenance changed while staging")
        (staging / "SOURCE-MANIFEST.json").write_bytes(
            _packaged_source_manifest(
                root_manifest,
                release_version,
                staging,
                openvmm_provenance,
                binary.name,
            )
        )
        write_sha256_sums(staging)
        verify_sha256_sums(staging)
        _publish_release_directory(staging, release_destination, force=force)
    except _ReleaseRestoreError:
        preserve_staging = True
        raise
    finally:
        if staging.exists() and not preserve_staging:
            shutil.rmtree(staging)
    print(f">> packaged {release_destination}")


def verify_source_tree() -> None:
    manifest = _read_json_object(
        REPO_ROOT / "SOURCE-MANIFEST.json",
        "source manifest",
    )
    patch_paths = _validate_source_manifest_metadata(
        manifest,
        kernel_provenance_inputs(),
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
        {REPO_ROOT / patch: f"kernel patch {patch}" for patch in patch_paths}
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
    config_path = REPO_ROOT / "kernel" / "config-microvm"
    config = config_path.read_text(encoding="utf-8")
    for setting in (
        "CONFIG_PVH=y",
        "CONFIG_HVC_XE9=y",
        "CONFIG_VIRTIO_FS=y",
        "CONFIG_FUSE_FS=y",
        *REQUIRED_SANDBOX_KERNEL_CONFIG,
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
    print(">> source tree and submodule metadata are consistent")

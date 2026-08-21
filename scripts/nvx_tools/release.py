"""Source collection, verification, and release packaging workflows."""

from __future__ import annotations

import json
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile

from .archive import create_reproducible_tar_gz
from .build import (
    DEFAULT_ALPINE_BRANCH,
    DEFAULT_ALPINE_MINIROOTFS_SHA256,
    DEFAULT_ALPINE_VERSION,
    DEFAULT_KERNEL_SHA256,
    DEFAULT_KERNEL_VERSION,
    DEFAULT_KERNEL_URL,
    DockerBuildConfig,
    build_docker_linux_source,
)
from .collect_alpine_sources import collect_alpine_sources
from .common import (
    OPENVMM_DIR,
    REPO_ROOT,
    SOURCE_DIR,
    ScriptError,
    artifact_path,
    openvmm_binary_path,
    require_file,
    verify_sha256_sums,
    write_sha256_sums,
)


PROJECT_SOURCE_PATHS = (
    "alpine",
    "benchmarks",
    "docker",
    "kernel",
    "licenses",
    "scripts",
    ".dockerignore",
    ".gitattributes",
    ".gitmodules",
    ".gitignore",
    "LICENSE",
    "LICENSE-NVX",
    "Makefile",
    "README.md",
    "SOURCE-MANIFEST.json",
    "THIRD_PARTY_NOTICES.md",
    "VERSION",
)


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
        "kernel/patches/0001-microvm-xe9-earlycon.patch": (
            REPO_ROOT / "kernel" / "patches" / "0001-microvm-xe9-earlycon.patch"
        ).read_bytes(),
        "kernel/patches/0002-microvm-hvc-xe9.patch": (
            REPO_ROOT / "kernel" / "patches" / "0002-microvm-hvc-xe9.patch"
        ).read_bytes(),
    }
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        names = [member.name for member in members]
        if not any(
            name.endswith(
                f"linux-{DEFAULT_KERNEL_VERSION}/drivers/tty/hvc/hvc_xe9.c"
            )
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
    required_guest_names = (
        "vmlinux",
        "vmlinux.config",
        "initramfs.cpio.gz",
        "initramfs.cpio.gz.packages.json",
    )
    for name in required_guest_names:
        require_file(artifact_path(name), f"required guest artifact {name}")
    guest_names = list(required_guest_names)
    package_manifests = [artifact_path("initramfs.cpio.gz.packages.json")]
    return guest_names, package_manifests


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
        SOURCE_DIR
        / "linux"
        / f"nvx-linux-source-{DEFAULT_KERNEL_VERSION}.tar.gz"
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

    release_version = version or (REPO_ROOT / "VERSION").read_text(
        encoding="ascii"
    ).strip()
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
        shutil.rmtree(release_destination)
    binary = openvmm_binary_path()
    _copy_release_file(binary, release_destination / "bin" / binary.name)
    for name in guest_names:
        _copy_release_file(artifact_path(name), release_destination / "guest" / name)
    for name in (
        "LICENSE",
        "LICENSE-NVX",
        "README.md",
        "SOURCE-MANIFEST.json",
        "THIRD_PARTY_NOTICES.md",
    ):
        _copy_release_file(REPO_ROOT / name, release_destination / name)
    _copy_release_file(
        OPENVMM_DIR / "LICENSE",
        release_destination / "licenses" / "LICENSE-OPENVMM",
    )
    _copy_release_file(
        REPO_ROOT / "licenses" / "COPYING-LINUX",
        release_destination / "licenses" / "COPYING-LINUX",
    )
    if include_source:
        source_destination = release_destination / "source"
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
    write_sha256_sums(release_destination)
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
        REPO_ROOT / "licenses" / "COPYING-LINUX": "Linux copyright notice",
        OPENVMM_DIR / "Cargo.toml": "initialized OpenVMM submodule",
    }
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
        for setting in ("CONFIG_PVH=y", "CONFIG_HVC_XE9=y"):
            if setting not in generated:
                raise ScriptError(f"{generated_config} is missing {setting}")
    head = subprocess.run(
        ["git", "-C", OPENVMM_DIR, "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    expected = manifest["openvmm"]["commit"]
    if head != expected:
        raise ScriptError(f"OpenVMM submodule is at {head}, expected {expected}")
    print(">> source tree and submodule metadata are consistent")

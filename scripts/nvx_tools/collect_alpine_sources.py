"""Collect exact aports recipes and upstream sources for initramfs APKs."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import tarfile
from pathlib import Path

from .build_constants import (
    AlpineBuildConstants,
    BuildConstants,
)
from .common import write_sha256_sums


class SourceError(RuntimeError):
    """An actionable source-collection failure."""


def _package_metadata(
    package: dict[str, object],
    branch: str,
    architecture: str,
) -> dict[str, str]:
    name = str(package["name"])
    expected_version = str(package["version"])
    commit = package.get("aports_commit")
    if not isinstance(commit, str) or re.fullmatch(r"[0-9a-f]{40}", commit) is None:
        raise SourceError(
            f"{name}-{expected_version} has no exact 40-character aports commit"
        )
    origin = package.get("origin")
    if not isinstance(origin, str) or not origin:
        raise SourceError(f"{name}-{expected_version} has no aports origin")
    return {
        "package": name,
        "version": expected_version,
        "origin": origin,
        "repository": "main",
        "license": str(package.get("license") or "unknown"),
        "commit": commit,
        "package_url": (
            f"{AlpineBuildConstants.PACKAGE_INDEX_URL}"
            f"?name={name}&branch={branch}&arch={architecture}"
        ),
    }


def _load_packages(paths: list[Path]) -> tuple[str, str, list[dict[str, object]]]:
    branch = None
    architecture = None
    packages: dict[tuple[str, str], dict[str, object]] = {}
    for path in paths:
        document = json.loads(path.read_text(encoding="utf-8"))
        guest = document.get("guest", AlpineBuildConstants.GUEST_NAME)
        if guest != AlpineBuildConstants.GUEST_NAME:
            raise SourceError(
                f"{path} is a {guest!r} guest package manifest; "
                "Alpine source collection only supports Alpine manifests"
            )
        current_branch = document.get("alpine_branch", AlpineBuildConstants.BRANCH)
        current_architecture = document.get(
            "architecture", AlpineBuildConstants.ARCHITECTURE
        )
        if branch not in (None, current_branch):
            raise SourceError("package manifests use different Alpine branches")
        if architecture not in (None, current_architecture):
            raise SourceError("package manifests use different architectures")
        branch = str(current_branch)
        architecture = str(current_architecture)
        for package in document["packages"]:
            key = (str(package["name"]), str(package["version"]))
            packages[key] = package
    if not packages:
        raise SourceError("package manifests contain no packages")
    return (
        branch or AlpineBuildConstants.BRANCH,
        architecture or AlpineBuildConstants.ARCHITECTURE,
        list(packages.values()),
    )


def _run(
    command: list[str | os.PathLike[str]],
    *,
    cwd: Path | None = None,
    capture: bool = False,
) -> subprocess.CompletedProcess[bytes]:
    display = " ".join(os.fspath(argument) for argument in command)
    print(f">> {display}")
    return subprocess.run(
        [os.fspath(argument) for argument in command],
        cwd=cwd,
        check=True,
        capture_output=capture,
    )


def _prepare_aports(cache: Path, branch: str) -> None:
    git_dir = cache / ".git"
    stable_branch = f"{branch.removeprefix('v')}-stable"
    if not git_dir.is_dir():
        cache.parent.mkdir(parents=True, exist_ok=True)
        _run(
            [
                "git",
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                "--single-branch",
                "--branch",
                stable_branch,
                AlpineBuildConstants.APORTS_URL,
                cache,
            ]
        )
    else:
        _run(
            ["git", "-C", cache, "fetch", "--filter=blob:none", "origin", stable_branch]
        )


def _safe_extract(data: bytes, destination: Path) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    archive_path = destination.resolve()
    from io import BytesIO

    with tarfile.open(fileobj=BytesIO(data), mode="r:") as archive:
        for member in archive.getmembers():
            target = (destination / member.name).resolve()
            if archive_path != target and archive_path not in target.parents:
                raise SourceError(f"unsafe path in aports archive: {member.name}")
        archive.extractall(destination)


def _extract_recipe(cache: Path, output: Path, metadata: dict[str, str]) -> str:
    repositories = (
        metadata["repository"],
        *(
            repository
            for repository in AlpineBuildConstants.REPOSITORIES
            if repository != metadata["repository"]
        ),
    )
    for repository in repositories:
        recipe = f"{repository}/{metadata['origin']}"
        probe = subprocess.run(
            [
                "git",
                "-C",
                cache,
                "cat-file",
                "-e",
                f"{metadata['commit']}:{recipe}/APKBUILD",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if probe.returncode == 0:
            metadata["repository"] = repository
            break
    else:
        raise SourceError(
            f"cannot find {metadata['origin']} at aports commit {metadata['commit']}"
        )
    destination = output / "recipes" / metadata["commit"]
    apkbuild = destination / recipe / "APKBUILD"
    metadata["source_directory"] = (
        f"upstream/{metadata['commit']}/{metadata['repository']}/{metadata['origin']}"
    )
    if apkbuild.is_file():
        return apkbuild.relative_to(output).as_posix()
    result = _run(
        [
            "git",
            "-C",
            cache,
            "archive",
            "--format=tar",
            metadata["commit"],
            recipe,
        ],
        capture=True,
    )
    _safe_extract(result.stdout, destination)
    if not apkbuild.is_file():
        raise SourceError(f"aports recipe was not extracted: {recipe}")
    return apkbuild.relative_to(output).as_posix()


def _fetch_upstream_sources(output: Path, alpine_version: str) -> None:
    script = r"""
set -eu
apk add --no-cache alpine-sdk
mkdir -p /bundle/upstream
find /bundle/recipes -name APKBUILD -type f | while IFS= read -r apkbuild; do
    recipe=${apkbuild%/APKBUILD}
    relative=${recipe#/bundle/recipes/}
    source_directory=/bundle/upstream/$relative
    mkdir -p "$source_directory"
    echo ">> fetching $recipe"
    work=$(mktemp -d)
    cp -a "$recipe/." "$work/"
    (
        cd "$work"
        attempt=1
        while ! SRCDEST="$source_directory" abuild -F fetch; do
            if [ "$attempt" -ge 3 ]; then
                echo "source fetch failed after $attempt attempts: $recipe" >&2
                exit 1
            fi
            attempt=$((attempt + 1))
            sleep 5
        done
        SRCDEST="$source_directory" abuild -F verify
    )
    rm -rf "$work"
done
"""
    _run(
        [
            "docker",
            "run",
            "--rm",
            "--volume",
            f"{output.resolve()}:/bundle",
            f"alpine:{alpine_version}",
            "sh",
            "-c",
            script,
        ]
    )


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("manifests", nargs="+", type=Path)
    parser.add_argument(
        "--output",
        type=Path,
        default=BuildConstants.SOURCE_DIR / AlpineBuildConstants.GUEST_NAME,
    )
    parser.add_argument(
        "--cache",
        type=Path,
        default=(
            BuildConstants.REPO_ROOT
            / BuildConstants.CACHE_DIRECTORY_NAME
            / AlpineBuildConstants.APORTS_CACHE_DIRECTORY_NAME
        ),
    )
    parser.add_argument(
        "--skip-upstream",
        action="store_true",
        help="collect exact aports recipes without running abuild fetch",
    )
    parser.set_defaults(handler=command_collect_alpine_sources)


def collect_alpine_sources(
    manifests: list[Path],
    output: Path,
    cache: Path,
    *,
    skip_upstream: bool = False,
) -> None:
    branch, architecture, packages = _load_packages(manifests)
    metadata = [
        _package_metadata(package, branch, architecture) for package in packages
    ]
    _prepare_aports(cache, branch)
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    for item in metadata:
        item["recipe"] = _extract_recipe(cache, output, item)
    manifest: dict[str, object] = {
        "format": AlpineBuildConstants.SOURCE_MANIFEST_FORMAT,
        "alpine_branch": branch,
        "architecture": architecture,
        "packages": metadata,
    }
    (output / "manifest.json").write_text(
        json.dumps(manifest, indent=2) + "\n",
        encoding="utf-8",
    )
    if not skip_upstream:
        alpine_version = branch.removeprefix("v")
        _fetch_upstream_sources(output, alpine_version)
    write_sha256_sums(output)
    print(f">> collected Alpine sources in {output}")


def command_collect_alpine_sources(args: argparse.Namespace) -> None:
    try:
        collect_alpine_sources(
            args.manifests,
            args.output,
            args.cache,
            skip_upstream=args.skip_upstream,
        )
    except KeyError as error:
        raise SourceError(f"missing package manifest field: {error}") from error

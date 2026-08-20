#!/usr/bin/env python3
"""Collect exact aports recipes and upstream sources for initramfs APKs."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile


REPO_ROOT = Path(__file__).resolve().parents[1]
APORTS_URL = "https://gitlab.alpinelinux.org/alpine/aports.git"
REPOSITORIES = ("main", "community", "testing")


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
            "https://pkgs.alpinelinux.org/packages"
            f"?name={name}&branch={branch}&arch={architecture}"
        ),
    }


def _load_packages(paths: list[Path]) -> tuple[str, str, list[dict[str, object]]]:
    branch = None
    architecture = None
    packages: dict[tuple[str, str], dict[str, object]] = {}
    for path in paths:
        document = json.loads(path.read_text(encoding="utf-8"))
        current_branch = document.get("alpine_branch", "v3.24")
        current_architecture = document.get("architecture", "x86_64")
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
    return branch or "v3.24", architecture or "x86_64", list(packages.values())


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
                APORTS_URL,
                cache,
            ]
        )
    else:
        _run(["git", "-C", cache, "fetch", "--filter=blob:none", "origin", stable_branch])


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
        *(repository for repository in REPOSITORIES if repository != metadata["repository"]),
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


def _write_checksums(output: Path) -> None:
    lines = []
    for path in sorted(output.rglob("*")):
        if not path.is_file() or path.name == "SHA256SUMS":
            continue
        digest = hashlib.sha256()
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
        lines.append(f"{digest.hexdigest()}  {path.relative_to(output).as_posix()}")
    (output / "SHA256SUMS").write_text("\n".join(lines) + "\n", encoding="ascii")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifests", nargs="+", type=Path)
    parser.add_argument(
        "--output",
        type=Path,
        default=REPO_ROOT / "build" / "sources" / "alpine",
    )
    parser.add_argument(
        "--cache",
        type=Path,
        default=REPO_ROOT / ".cache" / "aports",
    )
    parser.add_argument(
        "--skip-upstream",
        action="store_true",
        help="collect exact aports recipes without running abuild fetch",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        branch, architecture, packages = _load_packages(args.manifests)
        metadata = [
            _package_metadata(package, branch, architecture) for package in packages
        ]
        _prepare_aports(args.cache, branch)
        output = args.output.resolve()
        output.mkdir(parents=True, exist_ok=True)
        for item in metadata:
            item["recipe"] = _extract_recipe(args.cache, output, item)
        manifest = {
            "format": 1,
            "alpine_branch": branch,
            "architecture": architecture,
            "packages": metadata,
        }
        (output / "manifest.json").write_text(
            json.dumps(manifest, indent=2) + "\n",
            encoding="utf-8",
        )
        if not args.skip_upstream:
            alpine_version = branch.removeprefix("v")
            _fetch_upstream_sources(output, alpine_version)
        _write_checksums(output)
        print(f">> collected Alpine sources in {output}")
    except (
        SourceError,
        OSError,
        KeyError,
        ValueError,
        subprocess.CalledProcessError,
    ) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

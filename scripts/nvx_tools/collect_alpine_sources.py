"""Collect exact aports recipes and upstream sources for initramfs APKs."""

from __future__ import annotations

import argparse
import io
import json
import os
import re
import shutil
import subprocess
import tarfile
from pathlib import Path, PurePosixPath
from typing import cast

from .build_constants import (
    AlpineBuildConstants,
    BuildConstants,
)
from .common import is_link_or_junction, write_sha256_sums


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
        try:
            document = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise SourceError(f"cannot read Alpine package manifest {path}") from error
        if not isinstance(document, dict):
            raise SourceError(
                f"Alpine package manifest {path} must contain a JSON object"
            )
        document = cast(dict[str, object], document)
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
        for raw_package in cast(list[object], document["packages"]):
            package = cast(dict[str, object], raw_package)
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


def _recipe_symlink_source(
    path: PurePosixPath,
    members: dict[PurePosixPath, tarfile.TarInfo],
    directories: set[PurePosixPath],
    recipe: PurePosixPath,
) -> tarfile.TarInfo:
    # Resolve within the archive so a recipe link becomes a copy of its target.
    message = (
        f"aports recipe symlink {path} -> {members[path].linkname} does not "
        f"resolve to a regular file inside {recipe}"
    )
    current = path
    member = members[path]
    visited: set[PurePosixPath] = set()
    while member.issym():
        link = PurePosixPath(member.linkname)
        if current in visited or link.is_absolute():
            raise SourceError(message)
        visited.add(current)
        parts: list[str] = list(current.parent.parts)
        for index, part in enumerate(link.parts):
            if part == "..":
                if not parts:
                    raise SourceError(message)
                parts.pop()
                continue
            parts.append(part)
            if index + 1 < len(link.parts) and PurePosixPath(*parts) not in directories:
                raise SourceError(message)
        current = PurePosixPath(*parts)
        target = members.get(current)
        if target is None or recipe not in current.parents:
            raise SourceError(message)
        member = target
    if not member.isfile():
        raise SourceError(message)
    return member


def _safe_extract(data: bytes, destination: Path, recipe: str) -> list[str]:
    recipe_path = PurePosixPath(recipe)
    destination.mkdir(parents=True, exist_ok=True)
    root = destination.resolve()
    try:
        archive = tarfile.open(fileobj=io.BytesIO(data), mode="r:")
    except tarfile.TarError as error:
        raise SourceError(f"invalid aports archive for {recipe}: {error}") from error
    with archive:
        members: dict[PurePosixPath, tarfile.TarInfo] = {}
        for member in archive.getmembers():
            path = PurePosixPath(member.name)
            if not path.parts or path.is_absolute() or ".." in path.parts:
                raise SourceError(f"unsafe path in aports archive: {member.name}")
            target = destination.joinpath(*path.parts).resolve()
            if target != root and root not in target.parents:
                raise SourceError(f"unsafe path in aports archive: {member.name}")
            if not (member.isdir() or member.isfile() or member.issym()):
                raise SourceError(
                    "aports archive member is not a file, directory, or symlink: "
                    f"{member.name}"
                )
            if path in members:
                raise SourceError(f"duplicate path in aports archive: {member.name}")
            if not (
                path == recipe_path
                or recipe_path in path.parents
                or path in recipe_path.parents
            ):
                raise SourceError(
                    f"aports archive member is outside {recipe}: {member.name}"
                )
            members[path] = member
        # As tar extraction does, treat every parent of a member as a directory.
        directories = {path for path, member in members.items() if member.isdir()}
        for path in members:
            for parent in path.parents:
                if not parent.parts:
                    break
                listed = members.get(parent)
                if listed is not None and not listed.isdir():
                    raise SourceError(
                        f"aports archive places {path} below non-directory {parent}"
                    )
                directories.add(parent)
        files = {
            path: (
                _recipe_symlink_source(path, members, directories, recipe_path)
                if member.issym()
                else member
            )
            for path, member in members.items()
            if not member.isdir()
        }
        for directory in sorted(directories):
            target = destination.joinpath(*directory.parts)
            target.mkdir(exist_ok=True)
            target.chmod(0o755)
        executables: list[str] = []
        for path, source in sorted(files.items(), key=lambda item: item[0]):
            contents = archive.extractfile(source)
            if contents is None:
                raise SourceError(f"cannot read aports archive member: {source.name}")
            target = destination.joinpath(*path.parts)
            with contents, target.open("xb") as output:
                shutil.copyfileobj(contents, output)
            executable = bool(source.mode & 0o111)
            target.chmod(0o755 if executable else 0o644)
            if executable:
                executables.append(path.as_posix())
        return executables


def _extract_recipe(
    cache: Path,
    output: Path,
    metadata: dict[str, str],
    executables: set[str],
) -> str:
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
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.eol=lf",
            "-C",
            cache,
            "archive",
            "--format=tar",
            metadata["commit"],
            recipe,
        ],
        capture=True,
    )
    executables.update(
        f"{destination.relative_to(output).as_posix()}/{path}"
        for path in _safe_extract(result.stdout, destination, recipe)
    )
    if not apkbuild.is_file():
        raise SourceError(f"aports recipe was not extracted: {recipe}")
    return apkbuild.relative_to(output).as_posix()


def _fetch_upstream_sources(output: Path, alpine_version: str) -> None:
    script = r"""
set -eu
owner=$(stat -c '%u:%g' /bundle)
restore_owner() {
    if [ -e /bundle/upstream ]; then
        chown -R "$owner" /bundle/upstream ||
            echo "warning: cannot restore ownership of /bundle/upstream" >&2
    fi
}
trap restore_owner EXIT
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
            "--env",
            (
                "DISTFILES_MIRROR=https://distfiles.alpinelinux.org/"
                f"distfiles/v{alpine_version}"
            ),
            f"alpine:{alpine_version}",
            "sh",
            "-c",
            script,
        ]
    )


def _resolve_source_output(output: Path) -> Path:
    # Check the path as given, so a link there is refused instead of followed.
    if is_link_or_junction(output) or (output.exists() and not output.is_dir()):
        raise SourceError(f"Alpine source output is not a directory: {output}")
    if output.exists():
        unexpected = sorted(
            path.name
            for path in output.iterdir()
            if path.name not in AlpineBuildConstants.SOURCE_OUTPUT_ENTRIES
        )
        if unexpected:
            raise SourceError(
                "Alpine source output contains unexpected entries: "
                + ", ".join(unexpected)
            )
    return output.resolve()


def _reset_source_output(output: Path) -> None:
    for generated in (output / "recipes", output / "upstream"):
        if is_link_or_junction(generated) or (
            generated.exists() and not generated.is_dir()
        ):
            raise SourceError(
                f"Alpine source generated path is not a directory: {generated}"
            )
        if generated.exists():
            try:
                shutil.rmtree(generated)
            except OSError as error:
                raise SourceError(
                    f"cannot remove previous Alpine source output {generated}: "
                    f"{error}; an earlier Docker source fetch may have left "
                    "root-owned files, so remove it manually"
                ) from error
    for generated in (output / "manifest.json", output / "SHA256SUMS"):
        if is_link_or_junction(generated) or (
            generated.exists() and not generated.is_file()
        ):
            raise SourceError(
                f"Alpine source generated path is not a file: {generated}"
            )
        generated.unlink(missing_ok=True)


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
    output = _resolve_source_output(output)
    _prepare_aports(cache, branch)
    _reset_source_output(output)
    output.mkdir(parents=True, exist_ok=True)
    executables: set[str] = set()
    for item in metadata:
        item["recipe"] = _extract_recipe(cache, output, item, executables)
    manifest: dict[str, object] = {
        "format": AlpineBuildConstants.SOURCE_MANIFEST_FORMAT,
        "alpine_branch": branch,
        "architecture": architecture,
        "packages": metadata,
        # Hosts such as Windows cannot store file modes, so record them here.
        "executables": sorted(executables),
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

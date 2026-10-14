"""Check, build, identify, and test the Rust executables for Linux guests."""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from .build_constants import BuildConstants, GuestRustBuildConstants
from .common import (
    ScriptError,
    normalized_sha256_file,
    require_file,
    require_tool,
    run_checked,
    sha256_file,
)
from .elf import check_static_executables


@dataclass(frozen=True)
class GuestBinaryBuildResult:
    """Source identity and executable identities of one fresh guest build."""

    source_sha256: str
    binaries: dict[str, str]


def guest_binary_source_files(root: Path | None = None) -> tuple[Path, ...]:
    """Return the complete ordered input set of the PR 1 guest executables."""
    root = Path(root or BuildConstants.REPO_ROOT)
    crate = root / "nvx-rs"
    files = {
        crate / "Cargo.toml",
        crate / "Cargo.lock",
        crate / "build.rs",
        root / "rust-toolchain.toml",
        root / "scripts" / "nvx_tools" / "build_constants.py",
        root / "scripts" / "nvx_tools" / "guest_binaries.py",
    }
    source = crate / "src"
    if not source.is_dir():
        raise ScriptError(f"nvx-rs source directory not found: {source}")
    files.update(
        path
        for path in source.rglob("*")
        if path.is_file() and "target" not in path.relative_to(source).parts
    )
    return tuple(
        sorted(
            (require_file(path, "Rust guest binary source input") for path in files),
            key=lambda path: path.relative_to(root).as_posix(),
        )
    )


def guest_binary_source_sha256(root: Path | None = None) -> str:
    """Hash the paths and contents that determine every Rust guest executable."""
    root = Path(root or BuildConstants.REPO_ROOT)
    inventory = {
        "domain": GuestRustBuildConstants.SOURCE_DIGEST_DOMAIN,
        "format": GuestRustBuildConstants.SOURCE_DIGEST_FORMAT,
        "files": [
            {
                "path": path.relative_to(root).as_posix(),
                "sha256": normalized_sha256_file(path),
            }
            for path in guest_binary_source_files(root)
        ],
    }
    encoded = json.dumps(inventory, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def pinned_guest_rust_toolchain(root: Path | None = None) -> str:
    """Read and validate the repository's exact Rust release pin."""
    toolchain_file = (root or BuildConstants.REPO_ROOT) / "rust-toolchain.toml"
    contents = require_file(toolchain_file, "Rust toolchain pin").read_text(
        encoding="utf-8"
    )
    matches = re.findall(r'^channel = "([^"]+)"$', contents, re.MULTILINE)
    if len(matches) != 1:
        raise ScriptError(f"{toolchain_file} must define one toolchain channel")
    [toolchain] = matches
    if toolchain != GuestRustBuildConstants.TOOLCHAIN:
        raise ScriptError(
            "Rust guest build toolchain mismatch: "
            f"build constants select {GuestRustBuildConstants.TOOLCHAIN}, "
            f"but rust-toolchain.toml selects {toolchain}"
        )
    if re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", toolchain) is None:
        raise ScriptError(f"Rust guest build toolchain is not exact: {toolchain}")
    return toolchain


def _cargo_environment(target_directory: Path) -> dict[str, str]:
    """Return a deterministic Cargo environment for one fresh target tree."""
    environment = os.environ.copy()
    cargo_home = Path(
        environment.get("CARGO_HOME", str(Path.home() / ".cargo"))
    ).expanduser()
    if not cargo_home.is_absolute():
        cargo_home = (GuestRustBuildConstants.DIRECTORY / cargo_home).resolve()
    else:
        cargo_home = cargo_home.resolve()
    remap_flags = (
        f"--remap-path-prefix={BuildConstants.REPO_ROOT.resolve()}=/nvx-source",
        f"--remap-path-prefix={cargo_home}=/cargo-home",
        "-C",
        "link-arg=-Wl,--build-id=none",
    )
    environment.pop("RUSTFLAGS", None)
    for name in tuple(environment):
        if name.startswith("CARGO_PROFILE_"):
            environment.pop(name)
    environment["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(remap_flags)
    environment["CARGO_INCREMENTAL"] = "0"
    environment["CARGO_TARGET_DIR"] = str(target_directory)
    environment["SOURCE_DATE_EPOCH"] = "0"
    return environment


def _guest_cargo_arguments() -> list[str]:
    """Return the Cargo arguments that select the canonical guest build."""
    return [
        "--locked",
        "--profile",
        GuestRustBuildConstants.PROFILE,
        "--target",
        GuestRustBuildConstants.TARGET,
        "--no-default-features",
        "--features",
        GuestRustBuildConstants.FEATURE,
    ]


def _manifest(result: GuestBinaryBuildResult) -> dict[str, object]:
    """Return the stable on-disk manifest for a completed build."""
    return {
        "domain": GuestRustBuildConstants.BUILD_MANIFEST_DOMAIN,
        "format": GuestRustBuildConstants.BUILD_MANIFEST_FORMAT,
        "toolchain": GuestRustBuildConstants.TOOLCHAIN,
        "target": GuestRustBuildConstants.TARGET,
        "profile": GuestRustBuildConstants.PROFILE,
        "source_sha256": result.source_sha256,
        "binaries": result.binaries,
    }


def _validate_output_path(output: Path) -> None:
    """Reject output paths that overlap source or hold unrelated files."""
    crate = GuestRustBuildConstants.DIRECTORY.resolve()
    toolchain = GuestRustBuildConstants.TOOLCHAIN_FILE.resolve()
    if (
        output == crate
        or output.is_relative_to(crate)
        or crate.is_relative_to(output)
        or toolchain == output
        or toolchain.is_relative_to(output)
    ):
        raise ScriptError(f"guest binary output overlaps source inputs: {output}")
    if output.exists():
        _validate_owned_output(output)


def _validate_owned_output(output: Path) -> tuple[str, ...]:
    """Require an existing output to be a complete build owned by this tool.

    Return the retired executables that the output holds, which its build manifest
    must record. The output may lack added executables, which its build manifest
    must not record.
    """
    if not output.is_dir():
        raise ScriptError(
            f"guest binary output exists and is not a directory: {output}"
        )
    actual = {entry.name for entry in output.iterdir()}
    retired = tuple(
        binary
        for binary in GuestRustBuildConstants.RETIRED_BINARIES
        if binary in actual
    )
    missing = tuple(
        binary
        for binary in GuestRustBuildConstants.ADDED_BINARIES
        if binary not in actual
    )
    present = tuple(
        binary for binary in GuestRustBuildConstants.BINARIES if binary not in missing
    )
    expected = {
        GuestRustBuildConstants.BUILD_MANIFEST_NAME,
        *present,
        *retired,
    }
    if actual != expected:
        raise ScriptError(f"guest binary output is not owned by this builder: {output}")
    manifest_path = output / GuestRustBuildConstants.BUILD_MANIFEST_NAME
    try:
        loaded: object = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise ScriptError(
            f"guest binary output has an invalid build manifest: {manifest_path}"
        ) from error
    if not isinstance(loaded, dict):
        raise ScriptError(
            f"guest binary output has an invalid build manifest: {manifest_path}"
        )
    manifest = cast(dict[str, object], loaded)
    if (
        manifest.get("domain") != GuestRustBuildConstants.BUILD_MANIFEST_DOMAIN
        or manifest.get("format") != GuestRustBuildConstants.BUILD_MANIFEST_FORMAT
    ):
        raise ScriptError(
            f"guest binary output has an unrecognized build manifest: {manifest_path}"
        )
    if retired or missing:
        recorded = manifest.get("binaries")
        if (
            not isinstance(recorded, dict)
            or not all(
                binary in cast(dict[str, object], recorded) for binary in retired
            )
            or any(binary in cast(dict[str, object], recorded) for binary in missing)
        ):
            raise ScriptError(
                f"guest binary output is not owned by this builder: {output}"
            )
    for binary in (*present, *retired):
        path = output / binary
        if not path.is_file() or path.is_symlink():
            raise ScriptError(f"guest binary output has an invalid executable: {path}")
    return retired


def _install_output(staging: Path, output: Path) -> None:
    """Install staging without recursively deleting an existing directory."""
    if not output.exists():
        staging.replace(output)
        return
    retired = _validate_owned_output(output)
    for binary in GuestRustBuildConstants.BINARIES:
        (staging / binary).replace(output / binary)
    # Remove retired executables while the previous manifest still records them, so
    # that an interrupted installation leaves an output that the next build accepts.
    for binary in retired:
        (output / binary).unlink()
    (staging / GuestRustBuildConstants.BUILD_MANIFEST_NAME).replace(
        output / GuestRustBuildConstants.BUILD_MANIFEST_NAME
    )
    staging.rmdir()


def check_guest_binaries(toolchain: str | None = None) -> None:
    """Lint and test the guest binaries for musl.

    ``toolchain`` names the Rust toolchain, by default the exact repository pin.
    The tests run static musl executables on the host.
    """
    if sys.platform != "linux":
        raise ScriptError("Rust guest binaries can be checked only on Linux")
    selected = toolchain or pinned_guest_rust_toolchain()
    rustup = require_tool("rustup")
    cargo = require_tool("cargo")
    target = GuestRustBuildConstants.TARGET
    guest = (
        "--locked",
        "--target",
        target,
        "--no-default-features",
        "--features",
        GuestRustBuildConstants.FEATURE,
    )
    commands = (
        ("clippy", *guest, "--all-targets", "--", "-D", "warnings"),
        ("test", *guest),
    )
    # The integration tests must run the binaries that Cargo builds from this
    # checkout rather than a directory that the caller selected.
    environment = os.environ.copy()
    environment.pop(GuestRustBuildConstants.BINARIES_VARIABLE, None)
    run_checked(
        [rustup, "target", "add", "--toolchain", selected, target],
        cwd=GuestRustBuildConstants.DIRECTORY,
        env=environment,
    )
    for command in commands:
        run_checked(
            [cargo, f"+{selected}", *command],
            cwd=GuestRustBuildConstants.DIRECTORY,
            env=environment,
        )


def inspect_guest_binaries(directory: Path) -> GuestBinaryBuildResult:
    """Validate and identify a current, previously staged guest build."""
    directory = directory.resolve()
    retired = _validate_owned_output(directory)
    missing = [
        binary
        for binary in GuestRustBuildConstants.BINARIES
        if not (directory / binary).is_file()
    ]
    if retired or missing:
        raise ScriptError(
            f"guest binary output is not a current build of every executable: "
            f"{directory}"
        )
    paths = [directory / binary for binary in GuestRustBuildConstants.BINARIES]
    check_static_executables(paths)
    result = GuestBinaryBuildResult(
        source_sha256=guest_binary_source_sha256(),
        binaries={
            binary: sha256_file(path)
            for binary, path in zip(
                GuestRustBuildConstants.BINARIES, paths, strict=True
            )
        },
    )
    manifest_path = directory / GuestRustBuildConstants.BUILD_MANIFEST_NAME
    manifest = cast(
        dict[str, object],
        json.loads(manifest_path.read_text(encoding="utf-8")),
    )
    if manifest != _manifest(result):
        raise ScriptError(
            "Rust guest binary build manifest does not match the current "
            "sources, build pins, and executables"
        )
    return result


def build_guest_binaries(output: Path) -> GuestBinaryBuildResult:
    """Build, statically check, digest, and stage a fresh set of executables."""
    if sys.platform != "linux":
        raise ScriptError("Rust guest binaries can be built only on Linux")
    if not output.name:
        raise ScriptError("guest binary output must name a directory")
    output = output.resolve()
    _validate_output_path(output)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix=f".{output.name}.work-",
        dir=output.parent,
        ignore_cleanup_errors=True,
    ) as temporary:
        work = Path(temporary)
        target = work / "target"
        staging = work / "output"
        target.mkdir(parents=True)
        staging.mkdir()

        source_sha256 = guest_binary_source_sha256()
        toolchain = pinned_guest_rust_toolchain()
        cargo = require_tool("cargo")
        environment = _cargo_environment(target)
        command: list[str | os.PathLike[str]] = [
            cargo,
            f"+{toolchain}",
            "build",
            *_guest_cargo_arguments(),
        ]
        for binary in GuestRustBuildConstants.BINARIES:
            command.extend(("--bin", binary))
        run_checked(
            command,
            cwd=GuestRustBuildConstants.DIRECTORY,
            env=environment,
        )

        built_directory = (
            target / GuestRustBuildConstants.TARGET / GuestRustBuildConstants.PROFILE
        )
        staged_paths: list[Path] = []
        for binary in GuestRustBuildConstants.BINARIES:
            source = require_file(
                built_directory / binary, f"built Rust guest binary {binary}"
            )
            destination = staging / binary
            shutil.copyfile(source, destination)
            destination.chmod(0o755)
            staged_paths.append(destination)
        check_static_executables(staged_paths)
        if guest_binary_source_sha256() != source_sha256:
            raise ScriptError(
                "Rust guest binary source inputs changed during the build"
            )
        result = GuestBinaryBuildResult(
            source_sha256=source_sha256,
            binaries={
                binary: sha256_file(staging / binary)
                for binary in GuestRustBuildConstants.BINARIES
            },
        )
        (staging / GuestRustBuildConstants.BUILD_MANIFEST_NAME).write_text(
            json.dumps(_manifest(result), indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
            newline="\n",
        )
        _install_output(staging, output)
        return result


def test_guest_binaries(binaries: Path) -> None:
    """Run the musl integration tests against a staged guest-profile build."""
    if sys.platform != "linux":
        raise ScriptError("Rust guest binaries can be tested only on Linux")
    binaries = binaries.resolve()
    if not binaries.is_dir():
        raise ScriptError(f"guest binary directory not found: {binaries}")
    for binary in GuestRustBuildConstants.BINARIES:
        require_file(binaries / binary, f"Rust guest binary {binary}")
    require_file(
        binaries / GuestRustBuildConstants.BUILD_MANIFEST_NAME,
        "Rust guest binary build manifest",
    )
    environment = os.environ.copy()
    environment[GuestRustBuildConstants.BINARIES_VARIABLE] = str(binaries)
    run_checked(
        [
            require_tool("cargo"),
            f"+{pinned_guest_rust_toolchain()}",
            "test",
            "--locked",
            "--target",
            GuestRustBuildConstants.TARGET,
            "--no-default-features",
            "--features",
            GuestRustBuildConstants.FEATURE,
            "--test",
            "guest_tools",
        ],
        cwd=GuestRustBuildConstants.DIRECTORY,
        env=environment,
    )

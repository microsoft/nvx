"""Build the test-only Rust programs that microVM scenarios share into a guest.

Unlike the guest binaries, they never ship in an initramfs: a scenario that tests guest
library code shares one into the guest that it boots, and runs it there.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Final, cast

from .build_constants import BuildConstants, GuestRustBuildConstants
from .common import ScriptError, require_tool
from .elf import check_static_executables
from .guest_binaries import pinned_guest_rust_toolchain

# Each program and the Cargo integration test of `nvx-rs` that builds it.
GUEST_TEST_PROGRAMS: Final = {"gpt-device": "gpt_device"}
OUTPUT_DIRECTORY: Final = BuildConstants.BUILD_DIR / "guest-tests"


def guest_test_program_path(program: str) -> Path:
    """Return where ``build-guest-tests`` puts ``program`` by default."""
    return OUTPUT_DIRECTORY / program


def _test_executables(messages: str) -> dict[str, Path]:
    """Return the executable of each integration test in Cargo's JSON ``messages``."""
    executables: dict[str, Path] = {}
    for line in messages.splitlines():
        if not line.startswith("{"):
            continue
        message = cast(dict[str, object], json.loads(line))
        target = message.get("target")
        executable = message.get("executable")
        if (
            message.get("reason") == "compiler-artifact"
            and isinstance(target, dict)
            and cast(dict[str, object], target).get("kind") == ["test"]
            and isinstance(executable, str)
        ):
            name = cast(dict[str, object], target).get("name")
            if isinstance(name, str):
                executables[name] = Path(executable)
    return executables


def build_guest_test_programs(output: Path) -> dict[str, Path]:
    """Build every test-only guest program for musl into ``output``.

    Each is the static executable of an integration test that Cargo builds with the
    ``guest`` feature, and runs its ignored tests when a scenario passes ``--ignored``.
    The build keeps the test profile's debug assertions, but strips the debug
    information, which would make the programs ten times as large, from the
    executables that the guests' memory holds. Return the path of each program.
    """
    if sys.platform != "linux":
        raise ScriptError("Rust guest test programs can be built only on Linux")
    command = [
        require_tool("cargo"),
        f"+{pinned_guest_rust_toolchain()}",
        "test",
        "--locked",
        "--no-run",
        "--message-format=json-render-diagnostics",
        "--config",
        'profile.test.strip="debuginfo"',
        "--target",
        GuestRustBuildConstants.TARGET,
        "--no-default-features",
        "--features",
        GuestRustBuildConstants.FEATURE,
    ]
    for test in GUEST_TEST_PROGRAMS.values():
        command.extend(("--test", test))
    try:
        built = subprocess.run(
            command,
            cwd=GuestRustBuildConstants.DIRECTORY,
            stdout=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            check=False,
        )
    except OSError as error:
        raise ScriptError(f"cannot run cargo: {error}") from error
    if built.returncode != 0:
        raise ScriptError(
            f"command failed with exit {built.returncode}: {' '.join(command)}"
        )
    executables = _test_executables(built.stdout)
    output.mkdir(parents=True, exist_ok=True)
    programs: dict[str, Path] = {}
    for program, test in GUEST_TEST_PROGRAMS.items():
        source = executables.get(test)
        if source is None:
            raise ScriptError(f"Cargo built no executable for the {test} test")
        destination = output / program
        shutil.copyfile(source, destination)
        os.chmod(destination, 0o755)
        programs[program] = destination
    check_static_executables(list(programs.values()))
    return programs

"""The runtime adapter's consolidated suite, run by name on a real hypervisor.

`tests/runtime-adapter/scenarios.txt` names the suite's scenarios, each a test of one of
the adapter's test targets. The command checks that the scenarios name exactly the tests
of their targets, and that neither the library nor another target has an ignored test,
as the scenarios that boot VMs are, so that a scenario that is renamed, added, or gone
fails the command rather than drop out of it. It then runs each target's scenarios by
name, the ignored ones with the others, and checks that each of them ran and passed.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast

from .build_constants import BuildConstants, GuestRustBuildConstants
from .ci import OPENVMM_TEST_BACKENDS, validate_openvmm_test_backend
from .common import ScriptError, host_architecture, require_file
from .elf import check_static_executables

ADAPTER_DIRECTORY = BuildConstants.REPO_ROOT / "tests" / "runtime-adapter"
PACKAGE = "runtime-adapter"
SCENARIOS = ADAPTER_DIRECTORY / "scenarios.txt"
GUEST_AGENTS = {
    architecture: ADAPTER_DIRECTORY
    / "target"
    / target
    / "release"
    / "runtime-adapter-guest"
    for architecture, target in GuestRustBuildConstants.TARGETS.items()
}
GUEST_AGENT = GUEST_AGENTS["x86_64"]
SCENARIO_SCOPES = {
    "template_capture": "portable",
    "template_operations": "portable",
    "clone_start": "portable",
    "host_session": "portable",
    "staged_images": "portable",
    "layered_launch": "portable",
    "vm_restart": "cold",
    "vm_steps": "cold",
    "vm_layered": "cold",
    "vm_template": "snapshot",
    "vm_clone": "snapshot",
}
# libtest's line for a test that ended, and the summary of a test target's run.
_TEST_LINE = re.compile(r"^test (\S+) \.\.\. (\S+)")
_RESULT_LINE = re.compile(r"^test result: \S+\. (\d+) passed; ")


@dataclass(frozen=True)
class Target:
    """A test target of the adapter, with the tests of it that the scenarios name."""

    name: str
    tests: tuple[str, ...]


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.description = (
        "Run the runtime adapter's consolidated suite on a real hypervisor: each "
        "scenario that tests/runtime-adapter/scenarios.txt names, by its name."
    )
    parser.add_argument("--backend", choices=OPENVMM_TEST_BACKENDS, required=True)
    parser.add_argument(
        "--architecture",
        choices=GuestRustBuildConstants.ARCHITECTURES,
        default="x86_64",
    )
    parser.add_argument(
        "--cold-only",
        action="store_true",
        help="run the explicitly classified portable/cold targets, retaining the "
        "exhaustive full-suite naming check; report snapshot targets as deferred",
    )
    parser.add_argument(
        "--guest-agent",
        type=Path,
        help="the adapter's guest agent, a static musl executable of --architecture "
        "(default: its ISA-specific release build in tests/runtime-adapter)",
    )
    parser.add_argument(
        "--check-only",
        action="store_true",
        help="check that the scenarios name exactly the tests of their targets, and "
        "run none",
    )
    parser.add_argument(
        "--cargo",
        default="cargo",
        help="cargo executable used to build and run the tests (default: cargo)",
    )
    parser.set_defaults(handler=command_test_runtime_adapter)


def read_scenarios(path: Path = SCENARIOS) -> list[Target]:
    """Returns the targets that the scenarios of `path` name, in the order of their
    first lines, each with its tests in the order of their lines.

    A line names a target, then one of its tests; `#` starts a comment. Raises
    `ScriptError` for another line, and a scenario that two lines name.
    """
    tests: dict[str, list[str]] = {}
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        fields = line.split("#", 1)[0].split()
        if not fields:
            continue
        where = f"{path}:{number}"
        if len(fields) != 2:
            raise ScriptError(f"{where}: expected a target and a test, not {line!r}")
        target, test = fields
        named = tests.setdefault(target, [])
        if test in named:
            raise ScriptError(f"{where}: {target} {test} is named twice")
        named.append(test)
    if not tests:
        raise ScriptError(f"{path} names no scenario")
    return [Target(name, tuple(named)) for name, named in tests.items()]


def cargo_test_targets(cargo: str, environment: dict[str, str]) -> list[str]:
    """Returns the names of the adapter's test targets as Cargo finds them, those of
    the directories of its `tests` directory that hold a `main.rs` included."""
    command = [cargo, "metadata", "--locked", "--no-deps", "--format-version", "1"]
    found = listing(command, environment)
    if found.returncode != 0:
        raise ScriptError(f"cannot read the adapter's targets:\n{found.stderr}")
    metadata = cast(dict[str, Any], json.loads(found.stdout))
    for package in cast(list[dict[str, Any]], metadata["packages"]):
        if package["name"] == PACKAGE:
            return sorted(
                cast(str, target["name"])
                for target in cast(list[dict[str, Any]], package["targets"])
                if "test" in target["kind"]
            )
    raise ScriptError(f"Cargo's metadata of the adapter has no package {PACKAGE}")


def cargo_test(
    cargo: str, target: str | None, *arguments: str, host_target: str | None = None
) -> list[str]:
    """Returns the command that runs the test target `target` of the adapter's
    host-only configuration, or the unit tests of its library for `None`, with
    libtest's `arguments`."""
    selection = ["--lib"] if target is None else ["--test", target]
    return [
        cargo,
        "test",
        "--locked",
        "--features",
        "host",
        *(["--target", host_target] if host_target is not None else []),
        *selection,
        "--",
        *arguments,
    ]


def listed_tests(output: str) -> list[str]:
    """Returns the tests that libtest's terse list `output` names."""
    suffix = ": test"
    return [
        line.removesuffix(suffix)
        for line in output.splitlines()
        if line.endswith(suffix)
    ]


def naming_problems(target: Target, tests: Sequence[str]) -> list[str]:
    """Returns why the scenarios of `target` do not name exactly `tests`, the tests
    that the target has."""
    present = set(tests)
    named = set(target.tests)
    problems = [
        f"{target.name} has no test {test}"
        for test in target.tests
        if test not in present
    ]
    problems.extend(
        f"{target.name} has the test {test}, which no scenario names"
        for test in tests
        if test not in named
    )
    return problems


def run_problems(target: Target, output: str, status: int) -> list[str]:
    """Returns why the run of the scenarios of `target`, which printed `output` and
    exited with `status`, does not show that each of them ran and passed."""
    outcomes: dict[str, str] = {}
    passed = 0
    for line in output.splitlines():
        if (ended := _TEST_LINE.match(line)) is not None:
            outcomes[ended.group(1)] = ended.group(2)
        elif (result := _RESULT_LINE.match(line)) is not None:
            passed += int(result.group(1))
    problems: list[str] = []
    for test in target.tests:
        outcome = outcomes.get(test)
        if outcome is None:
            problems.append(f"{target.name} {test} did not run")
        elif outcome != "ok":
            problems.append(f"{target.name} {test}: {outcome}")
    if passed != len(target.tests):
        problems.append(f"{target.name} passed {passed} tests, not {len(target.tests)}")
    if status != 0 and not problems:
        problems.append(f"the tests of {target.name} exited with status {status}")
    return problems


def listing(
    command: list[str], environment: dict[str, str]
) -> subprocess.CompletedProcess[str]:
    """Runs `command`, which lists tests, in the adapter's directory, and returns what
    it printed."""
    return subprocess.run(
        command,
        cwd=ADAPTER_DIRECTORY,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )


def streamed(command: list[str], environment: dict[str, str]) -> tuple[str, int]:
    """Runs `command` in the adapter's directory, printing its output as it comes,
    and returns that output and the command's status."""
    print(f">> {shlex.join(command)}", flush=True)
    lines: list[str] = []
    with subprocess.Popen(
        command,
        cwd=ADAPTER_DIRECTORY,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
    ) as process:
        assert process.stdout is not None
        for line in process.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
            lines.append(line)
        status = process.wait()
    return "".join(lines), status


def suite_problems(
    targets: Sequence[Target],
    cargo: str,
    environment: dict[str, str],
    *,
    host_target: str | None = None,
) -> list[str]:
    """Returns why `targets` are not the adapter's suite: each target that is not one
    of the adapter's test targets, each scenario that names a test that its target does
    not have, each test of a named target that no scenario names, and each ignored test
    of the library or of a test target that no scenario names."""
    available = cargo_test_targets(cargo, environment)
    problems: list[str] = []
    for target in targets:
        if target.name not in available:
            problems.append(f"{target.name} is not a test target of the adapter")
            continue
        listed = listing(
            cargo_test(
                cargo,
                target.name,
                "--list",
                "--format",
                "terse",
                host_target=host_target,
            ),
            environment,
        )
        if listed.returncode != 0:
            problems.append(f"cannot list the tests of {target.name}:\n{listed.stderr}")
            continue
        problems.extend(naming_problems(target, listed_tests(listed.stdout)))
    named = {target.name for target in targets}
    others = [name for name in available if name not in named]
    for name in [None, *others]:
        command = cargo_test(
            cargo,
            name,
            "--ignored",
            "--list",
            "--format",
            "terse",
            host_target=host_target,
        )
        listed = listing(command, environment)
        what = "the library" if name is None else name
        if listed.returncode != 0:
            problems.append(f"cannot list the tests of {what}:\n{listed.stderr}")
            continue
        problems.extend(
            f"{what} has the ignored test {test}, which no scenario names"
            for test in listed_tests(listed.stdout)
        )
    return problems


def command_test_runtime_adapter(args: argparse.Namespace) -> int:
    backend = cast(str, args.backend)
    cargo = cast(str, args.cargo)
    check_only = cast(bool, args.check_only)
    architecture = getattr(args, "architecture", "x86_64")
    cold_only = getattr(args, "cold_only", False)
    if architecture != host_architecture():
        raise ScriptError("runtime adapter validation requires its native host ISA")
    if architecture == "aarch64" and (backend != "whp" or sys.platform != "win32"):
        raise ScriptError(
            "ARM64 runtime adapter validation requires native Windows/WHP"
        )
    if architecture == "aarch64" and not check_only and not cold_only:
        raise ScriptError("ARM64 adapter runs require explicit --cold-only selection")
    all_targets = read_scenarios()
    targets = all_targets
    if cold_only:
        named = {target.name for target in all_targets}
        if named != SCENARIO_SCOPES.keys():
            raise ScriptError(
                "runtime adapter scope inventory differs from its canonical targets: "
                f"unclassified={sorted(named - SCENARIO_SCOPES.keys())}; "
                f"stale={sorted(SCENARIO_SCOPES.keys() - named)}"
            )
        if any(
            scope not in ("portable", "cold", "snapshot")
            for scope in SCENARIO_SCOPES.values()
        ):
            raise ScriptError("runtime adapter inventory has an unknown scenario scope")
        targets = [
            target
            for target in all_targets
            if SCENARIO_SCOPES[target.name] in ("portable", "cold")
        ]
        if not any(
            SCENARIO_SCOPES[target.name] == "cold" and target.tests
            for target in targets
        ):
            raise ScriptError("runtime adapter cold scope selects no cold cases")
        deferred = [
            target
            for target in all_targets
            if SCENARIO_SCOPES[target.name] == "snapshot"
        ]
        print(
            f"runtime adapter cold scope on {architecture}: "
            f"{sum(len(target.tests) for target in targets)} selected tests; "
            f"{sum(len(target.tests) for target in deferred)} deferred snapshot tests "
            f"({', '.join(target.name for target in deferred)})"
        )
    host_target = "aarch64-pc-windows-msvc" if architecture == "aarch64" else None
    environment = os.environ.copy()
    if not check_only:
        validate_openvmm_test_backend(backend)
        guest = require_file(
            cast(Path | None, args.guest_agent) or GUEST_AGENTS[architecture],
            "runtime adapter guest agent",
        )
        try:
            check_static_executables([guest], architecture=architecture)
        except ValueError as error:
            raise ScriptError(
                f"invalid runtime adapter guest agent: {error}"
            ) from error
        environment["NVX_HYPERVISOR"] = backend
        # Cargo runs the tests in the adapter's directory, so a relative directory of
        # artifacts is the caller's, and NVX takes an empty one for none.
        artifacts = environment.get("NVX_ARTIFACTS_DIR")
        environment["NVX_ARTIFACTS_DIR"] = os.fspath(
            Path(artifacts).resolve() if artifacts else BuildConstants.REPO_ROOT
        )
        environment["RUNTIME_ADAPTER_GUEST"] = os.fspath(guest.resolve())
    problems = suite_problems(all_targets, cargo, environment, host_target=host_target)
    if not problems and not check_only:
        for target in targets:
            command = cargo_test(
                cargo,
                target.name,
                "--include-ignored",
                "--exact",
                *target.tests,
                host_target=host_target,
            )
            output, status = streamed(command, environment)
            problems.extend(run_problems(target, output, status))
    count = sum(len(target.tests) for target in targets)
    if problems:
        print(
            f"the runtime adapter's suite of {count} scenarios failed:", file=sys.stderr
        )
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    if check_only:
        print(
            f"{sum(len(target.tests) for target in all_targets)} scenarios name exactly "
            f"the tests of {len(all_targets)} targets, and "
            "neither the library nor another target has an ignored test"
        )
    else:
        print(f"the {count} scenarios of {len(targets)} targets passed on {backend}")
    return 0

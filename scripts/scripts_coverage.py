#!/usr/bin/env python3
"""Measure which statements and branches of the Python host tooling in scripts/
its unit tests reach.

The script runs the unit tests that the validate-nvx action runs, with the
commands of its Linux step, under coverage.py with branch coverage, measuring
the Python processes that the tests start too, and writes what they reached on
this host to an output directory:

- host-tooling/ and scenario-drivers/: the JSON, LCOV, and HTML reports of the
  host tooling and of the end-to-end scenario drivers, with paths relative to
  the repository;
- tests.log: the output of the tests;
- measurement.json: the commit, the tools, and how each command of the tests
  went;
- summary.md: the totals of the host tooling and of the scenario drivers, how
  the tests went, and the coverage of each module.

The measurement leaves out the unit tests, the test_*.py modules, and their
PowerShell test support, as check_test_layout.py names them. The end-to-end
scenario drivers, the *_tests.py modules of scripts/nvx_tools/, are measured,
but only a hypervisor runs them end to end, so the reports count them apart
from the host tooling, every other module of scripts/. A command whose tests
fail fails the measurement, after it writes the reports. The "scripts
coverage" section of doc/ci.md describes the measurement and the workflow that
runs it.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast

import yaml
from check_test_layout import (
    PYTHON_END_TO_END_TESTS,
    PYTHON_TEST_SUPPORT,
    PYTHON_UNIT_TESTS,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT = REPO_ROOT / "build" / "scripts-coverage"
# The action whose Linux step runs the unit tests that the measurement runs.
VALIDATE_NVX = ".github/actions/validate-nvx/action.yml"
VALIDATE_NVX_STEP = "Validate NVX CLI on Linux"
MEASUREMENT_FILE = "measurement.json"
MEASUREMENT_FORMAT = 1
SUMMARY_FILE = "summary.md"
LOG_FILE = "tests.log"
COVERAGE_FILE = "coverage.json"
LCOV_FILE = "lcov.info"
HTML_DIRECTORY = "html"
# The groups of the measured modules, whose reports go to directories of their
# own, so that the totals of each count only its modules.
HOST_TOOLING = "host-tooling"
SCENARIO_DRIVERS = "scenario-drivers"
GROUPS = {HOST_TOOLING: "Host tooling", SCENARIO_DRIVERS: "Scenario drivers"}
OUTPUTS = frozenset({MEASUREMENT_FILE, SUMMARY_FILE, LOG_FILE, *GROUPS})
# The variables with which coverage.py starts a measurement in a process. A
# measurement that runs under another, as when a test of this script runs
# under one, must not hand the other's on to the commands that it runs.
COVERAGE_VARIABLES = (
    "COVERAGE_FILE",
    "COVERAGE_PROCESS_CONFIG",
    "COVERAGE_PROCESS_START",
    "COVERAGE_RCFILE",
)
# A command of the Linux step that runs unit tests, with the arguments that
# follow `python3`: unittest's or a module's own runner, with -v.
_TEST_COMMAND = re.compile(r"python3 ((?:-m unittest )?(?:\S+\.py )+-v)")
# How unittest ends its report: the number of tests that ran, then the outcome.
_RAN = re.compile(r"Ran (\d+) tests? in [\d.]+s")
_OUTCOME = re.compile(r"(OK|FAILED)(?: \(([^)]*)\))?")
# The seconds between two copies of the output of a running command.
_POLL_SECONDS = 0.2


class MeasurementError(Exception):
    """A measurement that cannot run, or a report that cannot be read."""


Runner = Callable[[Sequence[str], Path, bool], subprocess.CompletedProcess[str]]


def environment(base: Mapping[str, str] | None = None) -> dict[str, str]:
    """Returns the environment of the measurement's commands: this process's,
    or `base`, without the variables with which coverage.py starts another
    measurement."""
    result = dict(os.environ if base is None else base)
    for name in COVERAGE_VARIABLES:
        result.pop(name, None)
    return result


def run_command(
    command: Sequence[str], cwd: Path, echo: bool
) -> subprocess.CompletedProcess[str]:
    """Runs `command` in `cwd` with the measurement's environment, and returns
    its output and errors together.

    The output goes to a file rather than to a pipe, so that a process that a
    test leaves running, holding what it inherited, cannot keep the command
    from ending. With `echo`, the file's bytes are copied to standard output
    as they come. A command that cannot start fails with status 127, as in a
    shell.
    """
    print("+ " + shlex.join(command), file=sys.stderr, flush=True)
    sys.stdout.flush()
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as directory:
        path = Path(directory) / "output"
        with path.open("wb") as output, path.open("rb") as reader:
            try:
                process = subprocess.Popen(
                    list(command),
                    cwd=cwd,
                    env=environment(),
                    stdin=subprocess.DEVNULL,
                    stdout=output,
                    stderr=subprocess.STDOUT,
                )
            except OSError as error:
                return subprocess.CompletedProcess(list(command), 127, f"{error}\n")
            with process:
                while True:
                    status = process.poll()
                    chunk = reader.read()
                    if chunk and echo:
                        sys.stdout.buffer.write(chunk)
                        sys.stdout.buffer.flush()
                    if status is not None:
                        break
                    time.sleep(_POLL_SECONDS)
        text = path.read_bytes().decode("utf-8", errors="replace")
    return subprocess.CompletedProcess(list(command), status, text)


def _require(result: subprocess.CompletedProcess[str], what: str) -> None:
    if result.returncode != 0:
        raise MeasurementError(
            f"{what} failed with status {result.returncode}: {shlex.join(result.args)}"
        )


def test_commands(root: Path = REPO_ROOT) -> list[tuple[str, ...]]:
    """Returns the commands with which the Linux step of validate-nvx runs unit
    tests, as the arguments that follow `python3`, in their order.

    A command runs unit tests if every module that it names is one, as
    check_test_layout.py places them: the step's other commands run the CLI.
    """
    path = root / VALIDATE_NVX
    try:
        action = cast(dict[str, Any], yaml.safe_load(path.read_text(encoding="utf-8")))
        steps = cast(list[dict[str, Any]], action["runs"]["steps"])
        step = next(step for step in steps if step.get("name") == VALIDATE_NVX_STEP)
        script = cast(str, step["run"])
    except (OSError, yaml.YAMLError, KeyError, TypeError, StopIteration) as error:
        raise MeasurementError(
            f"cannot read the step {VALIDATE_NVX_STEP!r} of {path}: {error!r}"
        ) from error
    commands: list[tuple[str, ...]] = []
    for line in script.splitlines():
        match = _TEST_COMMAND.fullmatch(line.strip())
        if match is None:
            continue
        arguments = tuple(match.group(1).split())
        modules = [argument for argument in arguments if argument.endswith(".py")]
        if not all(PYTHON_UNIT_TESTS.fullmatch(module) for module in modules):
            continue
        missing = [module for module in modules if not (root / module).is_file()]
        if missing:
            raise MeasurementError(
                f"{VALIDATE_NVX} runs test modules that do not exist: "
                + ", ".join(missing)
            )
        commands.append(arguments)
    if not commands:
        raise MeasurementError(
            f"the step {VALIDATE_NVX_STEP!r} of {VALIDATE_NVX} runs no unit tests"
        )
    return commands


def _relative(path: Path, root: Path) -> str:
    return path.relative_to(root).as_posix()


def unmeasured_modules(root: Path = REPO_ROOT) -> list[str]:
    """Returns the modules of scripts/ that the measurement leaves out, the
    unit tests and their test support, relative to `root`."""
    return sorted(
        relative
        for relative in (
            _relative(path, root) for path in (root / "scripts").rglob("*.py")
        )
        if PYTHON_UNIT_TESTS.fullmatch(relative) or relative in PYTHON_TEST_SUPPORT
    )


def _setting(value: str) -> str:
    # coverage.py substitutes environment variables for `$NAME` in settings.
    return value.replace("$", "$$")


def configuration(root: Path, data: Path) -> str:
    """Returns the coverage.py configuration of a measurement of the checkout
    at `root` whose data goes to the directory `data`.

    Its paths are absolute, since the processes that the tests start read it
    in their own working directories. It measures branches too, and the Python
    processes that the tests start, in which a measurement of nothing from
    scripts/ is no reason to warn. Its reports include the modules that no
    test imported, also those of directories without __init__.py, such as
    scripts/ itself.
    """
    lines = [
        "[run]",
        "branch = true",
        "patch = subprocess",
        f"source = {_setting(str(root / 'scripts'))}",
        f"data_file = {_setting(str(data / 'coverage'))}",
        "disable_warnings = no-data-collected",
        "omit =",
        *(f"    {_setting(str(root / module))}" for module in unmeasured_modules(root)),
        "",
        "[report]",
        "include_namespace_packages = true",
        "",
    ]
    return "\n".join(lines)


@dataclass(frozen=True)
class Count:
    covered: int
    count: int


@dataclass(frozen=True)
class Totals:
    statements: Count
    branches: Count

    def __add__(self, other: Totals) -> Totals:
        return Totals(
            Count(
                self.statements.covered + other.statements.covered,
                self.statements.count + other.statements.count,
            ),
            Count(
                self.branches.covered + other.branches.covered,
                self.branches.count + other.branches.count,
            ),
        )


NO_CODE = Totals(Count(0, 0), Count(0, 0))


def _integer(value: Any, what: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise MeasurementError(f"{what} is not a count: {value!r}")
    return value


def group_of(path: str) -> str:
    """Returns the group of a measured module, by its path relative to the
    repository."""
    if PYTHON_UNIT_TESTS.fullmatch(path) or path in PYTHON_TEST_SUPPORT:
        raise MeasurementError(f"the measurement counts test code: {path}")
    if PYTHON_END_TO_END_TESTS.fullmatch(path):
        return SCENARIO_DRIVERS
    return HOST_TOOLING


def _object(value: Any, what: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise MeasurementError(f"{what} is not an object")
    return cast(dict[str, Any], value)


def analyze(report: Any) -> dict[str, dict[str, Totals]]:
    """Returns the totals of each module of a coverage.py JSON report, by
    group and by the module's path relative to the repository."""
    files = _object(_object(report, "the report").get("files"), "the report's files")
    if not files:
        raise MeasurementError("the report holds no module")
    groups: dict[str, dict[str, Totals]] = {group: {} for group in GROUPS}
    for name, data in files.items():
        path = name.replace("\\", "/")
        counts = _object(
            _object(data, f"the report of {path}").get("summary"),
            f"the summary of {path}",
        )
        groups[group_of(path)][path] = Totals(
            Count(
                _integer(counts.get("covered_lines"), f"covered statements of {path}"),
                _integer(counts.get("num_statements"), f"statements of {path}"),
            ),
            Count(
                _integer(counts.get("covered_branches"), f"covered branches of {path}"),
                _integer(counts.get("num_branches"), f"branches of {path}"),
            ),
        )
    return groups


def total(modules: Mapping[str, Totals]) -> Totals:
    result = NO_CODE
    for totals in modules.values():
        result += totals
    return result


def outcome(output: str) -> str | None:
    """Returns how the tests whose unittest output is `output` went, as in
    "84 tests: OK", or None if the output doesn't tell."""
    lines = output.splitlines()
    for index in range(len(lines) - 1, -1, -1):
        ran = _RAN.fullmatch(lines[index].strip())
        if ran is None:
            continue
        for line in lines[index + 1 :]:
            result = _OUTCOME.fullmatch(line.strip())
            if result is not None:
                tests = int(ran.group(1))
                noun = "test" if tests == 1 else "tests"
                return f"{tests:,} {noun}: {result.group(0)}"
        return None
    return None


def _share(count: Count) -> str:
    """Returns the share of `count` that the tests reached, rounded down, so
    that only code that they reached entirely shows 100%."""
    if count.count == 0:
        return "none"
    share = math.floor(1000 * count.covered / count.count) / 10
    return f"{share:.1f}% of {count.count:,}"


def _table(header: str, rows: Iterable[tuple[str, Totals]]) -> list[str]:
    lines = [f"| {header} | Statements | Branches |", "| --- | ---: | ---: |"]
    for name, totals in rows:
        lines.append(
            f"| {name} | {_share(totals.statements)} | {_share(totals.branches)} |"
        )
    return lines


def _command(arguments: Sequence[str]) -> str:
    """Returns how the summary names a command of the tests."""
    if tuple(arguments[:2]) == ("-m", "unittest"):
        modules = [argument for argument in arguments if argument.endswith(".py")]
        noun = "module" if len(modules) == 1 else "modules"
        return f"`python3 -m unittest` with {len(modules)} {noun}"
    return f"`{shlex.join(['python3', *arguments])}`"


def _runs(measurement: Mapping[str, Any]) -> list[dict[str, Any]]:
    runs = measurement.get("runs")
    if not isinstance(runs, list):
        raise MeasurementError("the measurement records no runs")
    return cast(list[dict[str, Any]], runs)


def render(
    measurement: Mapping[str, Any], groups: Mapping[str, Mapping[str, Totals]]
) -> str:
    """Returns the Markdown summary of a measurement and of its coverage."""
    commit = str(measurement.get("commit", "unknown"))
    lines = [
        f"## scripts coverage on {measurement.get('host', 'unknown')}",
        "",
        "What the unit tests that validate-nvx runs reached of the Python modules "
        f"of `scripts/` at `{commit[:12]}`, with the Python processes that they "
        f"started, on Python {measurement.get('python', 'unknown')}, measured by "
        f"coverage.py {measurement.get('coverage', 'unknown')}. The unit tests, "
        "the `test_*.py` modules, and their test support go unmeasured, and the "
        "end-to-end scenario drivers, the `*_tests.py` modules of "
        "`scripts/nvx_tools/`, which only a hypervisor runs end to end, count "
        "apart.",
        "",
        *_table(
            "Code",
            ((GROUPS[group], total(groups.get(group, {}))) for group in GROUPS),
        ),
        "",
        "| Tests | Status | Result | Seconds |",
        "| --- | ---: | --- | ---: |",
    ]
    failed: list[str] = []
    for run in _runs(measurement):
        arguments = [str(argument) for argument in run.get("arguments", [])]
        status = run.get("status")
        result = run.get("outcome") or "unknown"
        lines.append(
            f"| {_command(arguments)} | {status} | {result} | {run.get('seconds')} |"
        )
        if status != 0:
            failed.append(_command(arguments))
    lines.append("")
    if failed:
        lines += [
            "The tests of "
            + ", ".join(failed)
            + " failed, so the measurement misses what they would have reached.",
            "",
        ]
    for group, title in GROUPS.items():
        modules = groups.get(group, {})
        lines += [f"### {title} by module", ""]
        if modules:
            lines += _table(
                "Module",
                ((f"`{path}`", totals) for path, totals in sorted(modules.items())),
            )
        else:
            lines.append("No module.")
        lines.append("")
    return "\n".join(lines)


def prepare(output: Path) -> None:
    """Makes `output` an empty directory. It refuses a directory that holds
    anything but the reports of an earlier run, which it removes."""
    if output.exists():
        if not output.is_dir():
            raise MeasurementError(f"{output} is not a directory")
        foreign = sorted(
            entry.name for entry in output.iterdir() if entry.name not in OUTPUTS
        )
        if foreign:
            raise MeasurementError(
                f"{output} holds more than the reports of an earlier run: "
                f"{', '.join(foreign)}; remove it or choose another --output"
            )
        for entry in output.iterdir():
            if entry.is_dir() and not entry.is_symlink():
                shutil.rmtree(entry)
            else:
                entry.unlink()
    output.mkdir(parents=True, exist_ok=True)


def _first_line(result: subprocess.CompletedProcess[str]) -> str:
    output = result.stdout.strip() if result.returncode == 0 else ""
    return output.splitlines()[0] if output else "unknown"


def measure(
    output: Path,
    root: Path = REPO_ROOT,
    run: Runner = run_command,
    python: str = sys.executable,
) -> int:
    """Measures the coverage of the unit tests of the checkout at `root` into
    `output`, and returns 0, or 1 if a command of the tests failed. It writes
    the reports either way."""
    root = root.resolve()
    output = output.resolve()
    commands = test_commands(root)
    coverage = [python, "-m", "coverage"]
    version = run([*coverage, "--version"], root, False)
    if version.returncode != 0:
        raise MeasurementError(
            "coverage.py is not installed: install requirements-dev.txt, with "
            "Python 3.11 or later"
        )
    found = re.search(r"version (\S+)", version.stdout)
    prepare(output)
    with tempfile.TemporaryDirectory(
        prefix="scripts-coverage-", ignore_cleanup_errors=True
    ) as directory:
        temporary = Path(directory)
        data = temporary / "data"
        data.mkdir()
        rcfile = temporary / "coveragerc"
        rcfile.write_text(configuration(root, data), encoding="utf-8")
        settings = f"--rcfile={rcfile}"
        transcript: list[str] = []
        runs: list[dict[str, Any]] = []
        started = time.monotonic()
        for arguments in commands:
            command = [*coverage, "run", settings, *arguments]
            began = time.monotonic()
            result = run(command, root, True)
            transcript.append(f"+ {shlex.join(command)}\n{result.stdout}")
            (output / LOG_FILE).write_text("".join(transcript), encoding="utf-8")
            runs.append(
                {
                    "arguments": list(arguments),
                    "status": result.returncode,
                    "outcome": outcome(result.stdout),
                    "seconds": round(time.monotonic() - began, 1),
                }
            )
        seconds = round(time.monotonic() - started, 1)
        _require(
            run([*coverage, "combine", settings, "--quiet"], root, True),
            "combining the data",
        )
        # A report command first combines every data file beside the one that
        # it reads, so the reports read a copy of the combined data, apart
        # from the files that a process that a test left running may write.
        combined = temporary / "combined" / "coverage"
        combined.parent.mkdir()
        try:
            shutil.copyfile(data / "coverage", combined)
        except OSError as error:
            raise MeasurementError(f"the combined data is missing: {error}") from error
        reading = [settings, f"--data-file={combined}", "--quiet"]
        report = temporary / COVERAGE_FILE
        _require(
            run([*coverage, "json", *reading, "-o", str(report)], root, True),
            "reporting",
        )
        try:
            groups = analyze(json.loads(report.read_text(encoding="utf-8")))
        except (OSError, ValueError) as error:
            raise MeasurementError(f"cannot read {report}: {error}") from error
        for group, title in GROUPS.items():
            modules = groups[group]
            if not modules:
                continue
            reports = output / group
            include = "--include=" + ",".join(
                str(root / module) for module in sorted(modules)
            )
            for options in (
                ["json", "-o", str(reports / COVERAGE_FILE)],
                ["lcov", "-o", str(reports / LCOV_FILE)],
                [
                    "html",
                    "-d",
                    str(reports / HTML_DIRECTORY),
                    f"--title=scripts coverage: {title.lower()}",
                ],
            ):
                _require(
                    run([*coverage, *options, *reading, include], root, True),
                    f"reporting the {title.lower()}",
                )
    commit = run(["git", "-C", str(root), "rev-parse", "HEAD"], root, False)
    totals = {group: total(modules) for group, modules in groups.items()}
    measurement: dict[str, Any] = {
        "format": MEASUREMENT_FORMAT,
        "host": platform.system(),
        "commit": commit.stdout.strip() if commit.returncode == 0 else "unknown",
        "python": platform.python_version(),
        "coverage": found.group(1) if found else _first_line(version),
        "seconds": seconds,
        "runs": runs,
        "totals": {
            group: {
                "statements": [counts.statements.covered, counts.statements.count],
                "branches": [counts.branches.covered, counts.branches.count],
            }
            for group, counts in totals.items()
        },
    }
    (output / MEASUREMENT_FILE).write_text(
        json.dumps(measurement, indent=2) + "\n", encoding="utf-8"
    )
    (output / SUMMARY_FILE).write_text(render(measurement, groups), encoding="utf-8")
    failed = [entry for entry in runs if entry["status"] != 0]
    for entry in failed:
        print(
            f"error: the tests of {_command(entry['arguments'])} failed with "
            f"status {entry['status']}",
            file=sys.stderr,
        )
    return 1 if failed else 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Measures which statements and branches of the Python modules of"
            " scripts/ the unit tests that validate-nvx runs reach, with"
            " coverage.py."
        )
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=DEFAULT_OUTPUT,
        help=(
            "the directory of the reports, which holds nothing but the reports"
            " of an earlier run (default: build/scripts-coverage)"
        ),
    )
    arguments = parser.parse_args(argv)
    try:
        return measure(cast(Path, arguments.output))
    except MeasurementError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())

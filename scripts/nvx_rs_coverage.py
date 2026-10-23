#!/usr/bin/env python3
"""Measure what each family of the nvx crate's golden tests alone reaches.

microsoft/nvx#722 retires, family by family, the golden tests that compare
nvx-rs with behavior recorded from Python, and each retirement has to show that
the tests that remain reach every line and branch that only the retired family
reached (#726). The measure command runs the crate's tests under cargo-llvm-cov,
with branch coverage, once for each selection of tests:

- all: every test;
- only-FAMILY: the tests of one family;
- without-FAMILY: every test but those of one family;
- without-families: every test outside the families.

It writes the LCOV report of each selection to the output directory, as
SELECTION.lcov with paths relative to the crate, the names of the crate's tests
to tests.txt, and what it measured to measurement.json. The summarize command
reads one or more such directories, such as those of a Linux and a Windows
host, and prints a Markdown summary: for each family, the lines and branches of
production code that only it reaches, which all.lcov covers and
without-FAMILY.lcov doesn't, and those that it reaches beyond the tests outside
the families, which only-FAMILY.lcov covers and without-families.lcov doesn't.
Production code is every source file of the crate but those of its test-only
modules, which a cfg(test) module declaration names; test items inside other
files count as production code. The "nvx-rs coverage" section of doc/ci.md
describes the workflow that runs both commands.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import stat
import subprocess
import sys
import time
from collections import Counter
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, cast

REPO_ROOT = Path(__file__).resolve().parents[1]
CRATE_DIR = REPO_ROOT / "nvx-rs"
MEASUREMENT_FILE = "measurement.json"
MEASUREMENT_FORMAT = 1
TESTS_FILE = "tests.txt"
# Every run builds and reports with the same options, so that one build serves
# every selection and every report instruments the same lines and branches.
# Branch coverage needs a nightly compiler. The remapped paths are relative to
# the crate, so that the reports of different hosts compare, and the reports
# leave out the crate's tests directories and tests.rs files, as cargo-llvm-cov
# does for remapped paths.
RUN_OPTIONS = ("--branch", "--remap-path-prefix", "--all-features", "--locked")
REPORT_OPTIONS = ("--remap-path-prefix", "--locked")
# The most runs of a selection, whose tests may fail only sometimes.
RUNS = 2


class MeasurementError(Exception):
    """A measurement that cannot run, or a report that cannot be read."""


@dataclass(frozen=True)
class Family:
    """A family of golden tests, which microsoft/nvx#722 retires as a whole.

    Its tests are those whose names contain one of its libtest filters, and its
    slug names its selections, such as only-SLUG.
    """

    slug: str
    title: str
    filters: tuple[str, ...]


FAMILIES = (
    Family("python-semantics", "Python semantics", ("host::python::golden::",)),
    Family(
        "launch-profile", "Launch profile and options", ("host::sandbox::golden::",)
    ),
    Family(
        "cli-scenarios", "CLI scenarios", ("nvx_rs_reproduces_the_recorded_scenarios",)
    ),
)


@dataclass(frozen=True)
class Selection:
    """The tests of one run, which these arguments of the test binaries select."""

    name: str
    arguments: tuple[str, ...]


def _skipping(filters: Iterable[str]) -> tuple[str, ...]:
    return tuple(argument for name in filters for argument in ("--skip", name))


def selections(families: Sequence[Family]) -> list[Selection]:
    """Returns the selections to measure, in the order in which to measure them."""
    return [
        Selection("all", ()),
        *(Selection(f"only-{family.slug}", family.filters) for family in families),
        *(
            Selection(f"without-{family.slug}", _skipping(family.filters))
            for family in families
        ),
        Selection(
            "without-families",
            _skipping(name for family in families for name in family.filters),
        ),
    ]


_LISTED_TEST = re.compile(r"^(\S.*): test$", re.MULTILINE)


def listed_tests(listing: str) -> list[str]:
    """Returns the tests that the test binaries' --list output names, sorted."""
    return sorted(set(_LISTED_TEST.findall(listing)))


def check_families(families: Sequence[Family], tests: Sequence[str]) -> None:
    """Fails unless each filter of each family selects one of the tests."""
    unmatched = [
        f"{name} ({family.title})"
        for family in families
        for name in family.filters
        if not any(name in test for test in tests)
    ]
    if unmatched:
        raise MeasurementError(
            "these family filters select no test: " + ", ".join(unmatched)
        )


# The attributes of a test-only module, and its out-of-line declaration.
_TEST_CFG = re.compile(r"#\[cfg\((?:test|all\(\s*test\s*,.*)\)\]")
_MODULE = re.compile(r"(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;")
_ATTRIBUTE_OR_COMMENT = re.compile(r"#\[.*|//.*")


def test_modules(crate: Path) -> list[str]:
    """Returns the files of the crate's test-only modules, relative to the crate.

    A test-only module is one that a cfg(test) declaration names: its file and,
    with a trailing slash, the directory that holds its submodules.
    """
    found: set[str] = set()
    for path in sorted((crate / "src").rglob("*.rs")):
        attributes: list[str] = []
        for line in path.read_text(encoding="utf-8").splitlines():
            text = line.strip()
            if not text or _ATTRIBUTE_OR_COMMENT.fullmatch(text):
                attributes.append(text)
                continue
            declaration = _MODULE.fullmatch(text)
            if declaration and any(map(_TEST_CFG.fullmatch, attributes)):
                found.update(_module_files(crate, path, declaration.group(1)))
            attributes = []
    return sorted(found)


def _module_files(crate: Path, parent: Path, name: str) -> list[str]:
    """Returns the file and directory of the module `name` that `parent` declares."""
    if parent.name in ("lib.rs", "main.rs", "mod.rs"):
        directory = parent.parent / name
    else:
        directory = parent.parent / parent.stem / name
    file = directory.with_name(f"{name}.rs")
    if not file.is_file():
        file = directory / "mod.rs"
    if not file.is_file():
        raise MeasurementError(
            f"{parent.relative_to(crate).as_posix()}: "
            f"the test module {name} has no file"
        )
    return [
        file.relative_to(crate).as_posix(),
        directory.relative_to(crate).as_posix() + "/",
    ]


def is_production(path: str, modules: Sequence[str]) -> bool:
    """Returns whether `path`, relative to the crate, holds production code."""
    if path.startswith("tests/"):
        return False
    return not any(
        path.startswith(module) if module.endswith("/") else path == module
        for module in modules
    )


Line = tuple[str, int]
Branch = tuple[str, int, int, int]


@dataclass(frozen=True)
class Report:
    """The lines and branches of an LCOV report, and whether its run reached each.

    A line is a source path and a line number, and a branch also has its block
    and branch numbers, as LCOV's DA and BRDA records give them.
    """

    lines: dict[Line, bool]
    branches: dict[Branch, bool]


def _source(path: str) -> str:
    source = path.replace("\\", "/").removeprefix("./")
    if source.startswith("/") or re.match(r"[A-Za-z]:/", source):
        raise MeasurementError(
            f"{path} is not relative to the crate; run cargo llvm-cov with "
            "--remap-path-prefix"
        )
    return source


def read_lcov(text: str) -> Report:
    """Reads an LCOV report, merging the records of a line or branch."""
    lines: dict[Line, bool] = {}
    branches: dict[Branch, bool] = {}
    source: str | None = None
    for number, record in enumerate(text.splitlines(), 1):
        kind, _, value = record.partition(":")
        try:
            if kind == "SF":
                source = _source(value)
            elif kind == "end_of_record":
                source = None
            elif source is not None and kind == "DA":
                line, count = value.split(",")[:2]
                key = (source, int(line))
                lines[key] = lines.get(key, False) or int(count) > 0
            elif source is not None and kind == "BRDA":
                line, block, branch, taken = value.split(",")
                branch_key = (source, int(line), int(block), int(branch))
                reached = taken != "-" and int(taken) > 0
                branches[branch_key] = branches.get(branch_key, False) or reached
        except ValueError as error:
            raise MeasurementError(
                f"line {number}: malformed {kind} record: {record!r}"
            ) from error
    return Report(lines, branches)


@dataclass(frozen=True)
class Reach:
    """Lines and branches of production code."""

    lines: frozenset[Line]
    branches: frozenset[Branch]

    def __sub__(self, other: Reach) -> Reach:
        return Reach(self.lines - other.lines, self.branches - other.branches)


@dataclass(frozen=True)
class FamilyReach:
    """What a family reaches that other tests don't.

    `only` holds what only the family reaches: what every test reaches and the
    tests outside the family don't. `beyond` holds what the family reaches and
    the tests outside every family don't.
    """

    family: Family
    only: Reach
    beyond: Reach


@dataclass(frozen=True)
class Analysis:
    """The summary of one measurement.

    `together` holds what the families reach and the tests outside every family
    don't, `failed` the selections whose tests failed in every run, and
    `retried` those whose tests failed and then passed.
    """

    label: str
    commit: str
    toolchain: str
    failed: tuple[str, ...]
    retried: tuple[str, ...]
    instrumented: Reach
    reached: Reach
    families: tuple[FamilyReach, ...]
    together: Reach


def _object(value: Any, what: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise MeasurementError(f"{what} is not a JSON object")
    return cast(dict[str, Any], value)


def _array(value: Any, what: str) -> list[Any]:
    if not isinstance(value, list):
        raise MeasurementError(f"{what} is not a JSON array")
    return cast(list[Any], value)


def _string(value: Any, what: str) -> str:
    if not isinstance(value, str):
        raise MeasurementError(f"{what} is not a JSON string")
    return value


def _strings(value: Any, what: str) -> tuple[str, ...]:
    return tuple(_string(item, what) for item in _array(value, what))


def analyze(directory: Path) -> Analysis:
    """Reads the measurement in `directory` and computes what each family reaches."""
    path = directory / MEASUREMENT_FILE
    try:
        document = _object(json.loads(path.read_text(encoding="utf-8")), str(path))
    except (OSError, ValueError) as error:
        raise MeasurementError(f"{path}: {error}") from error
    if document.get("format") != MEASUREMENT_FORMAT:
        raise MeasurementError(f"{path}: not a measurement of format 1")
    families = tuple(
        Family(
            _string(family.get("slug"), "a family's slug"),
            _string(family.get("title"), "a family's title"),
            _strings(family.get("filters"), "a family's filters"),
        )
        for family in (
            _object(item, "a family")
            for item in _array(document.get("families"), "families")
        )
    )
    modules = _strings(document.get("test_modules"), "test_modules")
    reports: dict[str, Report] = {}
    failed: list[str] = []
    retried: list[str] = []
    for item in _array(document.get("selections"), "selections"):
        selection = _object(item, "a selection")
        name = _string(selection.get("name"), "a selection's name")
        report = directory / _string(selection.get("report"), "a selection's report")
        try:
            reports[name] = read_lcov(report.read_text(encoding="utf-8"))
        except (OSError, MeasurementError) as error:
            raise MeasurementError(f"{report}: {error}") from error
        statuses = _array(selection.get("statuses"), "a selection's statuses")
        if not statuses or not all(isinstance(status, int) for status in statuses):
            raise MeasurementError(f"{path}: {name} has no statuses of its runs")
        if statuses[-1] != 0:
            failed.append(name)
        elif len(statuses) > 1:
            retried.append(name)
    expected = [selection.name for selection in selections(families)]
    if sorted(reports) != sorted(expected):
        raise MeasurementError(f"{path}: the selections are not {', '.join(expected)}")
    first = reports["all"]
    for name, report in reports.items():
        if report.lines.keys() != first.lines.keys() or (
            report.branches.keys() != first.branches.keys()
        ):
            raise MeasurementError(
                f"{directory}: {name} and all instrument different lines or "
                "branches; measure them with one build"
            )

    def reach(name: str) -> Reach:
        report = reports[name]
        return Reach(
            frozenset(
                line
                for line, hit in report.lines.items()
                if hit and is_production(line[0], modules)
            ),
            frozenset(
                branch
                for branch, hit in report.branches.items()
                if hit and is_production(branch[0], modules)
            ),
        )

    every = reach("all")
    outside = reach("without-families")
    return Analysis(
        label=_string(document.get("host"), "host"),
        commit=_string(document.get("commit"), "commit"),
        toolchain=_string(document.get("toolchain"), "toolchain"),
        failed=tuple(failed),
        retried=tuple(retried),
        instrumented=Reach(
            frozenset(line for line in first.lines if is_production(line[0], modules)),
            frozenset(
                branch for branch in first.branches if is_production(branch[0], modules)
            ),
        ),
        reached=every,
        families=tuple(
            FamilyReach(
                family,
                only=every - reach(f"without-{family.slug}"),
                beyond=reach(f"only-{family.slug}") - outside,
            )
            for family in families
        ),
        together=every - outside,
    )


def _amount(count: int, singular: str, plural: str) -> str:
    return f"{count:,} {singular if count == 1 else plural}"


def _ranges(numbers: Iterable[int]) -> str:
    """Returns sorted numbers as ranges, such as 1-3, 7."""
    parts: list[tuple[int, int]] = []
    for number in sorted(set(numbers)):
        if parts and parts[-1][1] == number - 1:
            parts[-1] = (parts[-1][0], number)
        else:
            parts.append((number, number))
    return ", ".join(
        str(first) if first == last else f"{first}-{last}" for first, last in parts
    )


def _locations(reach: Reach) -> list[str]:
    """Returns a Markdown list item for each file that `reach` holds lines of."""
    items: list[str] = []
    for source in sorted({line[0] for line in reach.lines | reach.branches}):
        parts: list[str] = []
        numbers = [line[1] for line in reach.lines if line[0] == source]
        if numbers:
            parts.append("lines " + _ranges(numbers))
        outcomes = Counter(
            branch[1] for branch in reach.branches if branch[0] == source
        )
        if outcomes:
            parts.append(
                "branches on lines "
                + ", ".join(
                    str(line) if count == 1 else f"{line} ({count})"
                    for line, count in sorted(outcomes.items())
                )
            )
        items.append(f"- `{source}`: " + "; ".join(parts))
    return items


def _table(
    analyses: Sequence[Analysis], select: Callable[[FamilyReach], Reach]
) -> list[str]:
    header = ["Family"]
    for analysis in analyses:
        header += [f"{analysis.label} lines", f"{analysis.label} branches"]
    rows = [
        "| " + " | ".join(header) + " |",
        "| --- |" + " ---: |" * (len(header) - 1),
    ]
    for index, family in enumerate(analyses[0].families):
        cells = [family.family.title]
        for analysis in analyses:
            reach = select(analysis.families[index])
            cells += [f"{len(reach.lines):,}", f"{len(reach.branches):,}"]
        rows.append("| " + " | ".join(cells) + " |")
    cells = ["All families"]
    for analysis in analyses:
        cells += [
            f"{len(analysis.together.lines):,}",
            f"{len(analysis.together.branches):,}",
        ]
    rows.append("| " + " | ".join(cells) + " |")
    return rows


def render(analyses: Sequence[Analysis]) -> str:
    """Returns the Markdown summary of one or more measurements."""
    titles = [[family.family for family in analysis.families] for analysis in analyses]
    if any(families != titles[0] for families in titles):
        raise MeasurementError("the measurements have different families")
    lines = ["## nvx-rs coverage", ""]
    for analysis in analyses:
        lines.append(
            f"- {analysis.label}, commit `{analysis.commit}`, `{analysis.toolchain}`: "
            f"the tests reach {len(analysis.reached.lines):,} of "
            f"{len(analysis.instrumented.lines):,} lines and "
            f"{len(analysis.reached.branches):,} of "
            f"{len(analysis.instrumented.branches):,} branches of production code."
        )
        if analysis.failed:
            lines.append(
                "  Tests failed in " + ", ".join(analysis.failed) + ", whose "
                "reports lack what the failed tests would have reached."
            )
        if analysis.retried:
            lines.append(
                "  Tests failed in " + ", ".join(analysis.retried) + ", and "
                "passed when they ran again."
            )
    lines += [
        "",
        "### What only each family reaches",
        "",
        "The lines and branches of production code that all tests reach and the "
        "tests outside the family don't. Retiring the family loses them, unless "
        "other tests reach them.",
        "",
        *_table(analyses, lambda family: family.only),
        "",
        "### What each family reaches beyond the tests outside the families",
        "",
        "The lines and branches of production code that the family reaches and "
        "the tests outside every family don't. In both tables, All families "
        "counts those that all tests reach and the tests outside every family "
        "don't.",
        "",
        *_table(analyses, lambda family: family.beyond),
        "",
        "### Where",
        "",
        "Each list names the lines, and the lines of the branch outcomes, with "
        "their number in parentheses when a line has several, that only the "
        "family reaches.",
    ]
    for analysis in analyses:
        named = [(family.family.title, family.only) for family in analysis.families]
        for title, reach in [*named, ("all families", analysis.together)]:
            if not reach.lines and not reach.branches:
                continue
            lines += [
                "",
                "<details>",
                f"<summary>{analysis.label}: what only {title} reach "
                f"({_amount(len(reach.lines), 'line', 'lines')}, "
                f"{_amount(len(reach.branches), 'branch', 'branches')})</summary>",
                "",
                *_locations(reach),
                "",
                "</details>",
            ]
    return "\n".join(lines) + "\n"


def _reach_json(reach: Reach) -> dict[str, Any]:
    return {
        "lines": [list(line) for line in sorted(reach.lines)],
        "branches": [list(branch) for branch in sorted(reach.branches)],
    }


def to_json(analyses: Sequence[Analysis]) -> dict[str, Any]:
    """Returns the lines and branches of the summary as a JSON document."""
    return {
        "format": 1,
        "measurements": [
            {
                "host": analysis.label,
                "commit": analysis.commit,
                "toolchain": analysis.toolchain,
                "failed": list(analysis.failed),
                "retried": list(analysis.retried),
                "instrumented": {
                    "lines": len(analysis.instrumented.lines),
                    "branches": len(analysis.instrumented.branches),
                },
                "reached": {
                    "lines": len(analysis.reached.lines),
                    "branches": len(analysis.reached.branches),
                },
                "families": [
                    {
                        "slug": family.family.slug,
                        "title": family.family.title,
                        "only": _reach_json(family.only),
                        "beyond": _reach_json(family.beyond),
                    }
                    for family in analysis.families
                ],
                "together": _reach_json(analysis.together),
            }
            for analysis in analyses
        ],
    }


Runner = Callable[[Sequence[str], bool], subprocess.CompletedProcess[str]]


def run_command(
    command: Sequence[str], capture: bool
) -> subprocess.CompletedProcess[str]:
    """Runs `command` in the crate, capturing its standard output if `capture`."""
    print("+ " + subprocess.list2cmdline(command), file=sys.stderr, flush=True)
    return subprocess.run(
        list(command),
        cwd=CRATE_DIR,
        stdout=subprocess.PIPE if capture else None,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )


def _require(result: subprocess.CompletedProcess[str], what: str) -> None:
    if result.returncode != 0:
        raise MeasurementError(f"{what} failed with status {result.returncode}")


def _version(run: Runner, command: Sequence[str]) -> str:
    result = run(command, True)
    output = result.stdout.strip() if result.returncode == 0 else ""
    return output.splitlines()[0] if output else "unknown"


def profile_directory(run: Runner, toolchain: str, crate: Path = CRATE_DIR) -> Path:
    """Returns the directory to which cargo-llvm-cov's runs write their profiles.

    That is its target directory: CARGO_LLVM_COV_TARGET_DIR, if set, which a
    relative path names in the crate, where cargo-llvm-cov runs, or else the
    llvm-cov-target directory in Cargo's target directory.
    """
    override = os.environ.get("CARGO_LLVM_COV_TARGET_DIR")
    if override:
        return crate / override
    result = run(
        ["cargo", f"+{toolchain}", "metadata", "--format-version", "1", "--no-deps"],
        True,
    )
    _require(result, "reading the crate's metadata")
    try:
        metadata = _object(json.loads(result.stdout), "the crate's metadata")
    except ValueError as error:
        raise MeasurementError(f"the crate's metadata: {error}") from error
    target = _string(metadata.get("target_directory"), "target_directory")
    return Path(target) / "llvm-cov-target"


def make_readable(directory: Path) -> int:
    """Lets the current user read and write each profile in `directory`, and
    returns how many profiles needed it.

    A process writes its profile under its umask, so a process whose umask
    clears the owner's permissions, as some store tests set, leaves a profile
    that llvm-profdata can't read when it merges the profiles, and it crashes.
    """
    owner = stat.S_IRUSR | stat.S_IWUSR
    changed = 0
    for path in sorted(directory.glob("*.profraw")):
        mode = path.stat().st_mode
        if mode & owner != owner:
            path.chmod(stat.S_IMODE(mode) | owner)
            changed += 1
    return changed


def measure(
    output: Path,
    toolchain: str,
    run: Runner = run_command,
    crate: Path = CRATE_DIR,
    families: Sequence[Family] = FAMILIES,
) -> int:
    """Measures every selection into `output` and returns the exit status.

    The tests of a selection may fail, even only sometimes, so a selection whose
    tests fail runs once more, and its report holds what its last run reached.
    measurement.json records the statuses of each run, and the status is 1 if
    the tests of a selection failed in both.
    """
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    cargo = ["cargo", f"+{toolchain}", "llvm-cov"]
    _require(run([*cargo, "clean", "--workspace"], False), "cleaning")
    profiles = profile_directory(run, toolchain, crate)
    # One build serves every selection. Listing the tests also checks that each
    # family's filters still select tests.
    listing = run([*cargo, "--no-report", *RUN_OPTIONS, "--", "--list"], True)
    _require(listing, "building and listing the tests")
    tests = listed_tests(listing.stdout)
    (output / TESTS_FILE).write_text("".join(f"{test}\n" for test in tests), "utf-8")
    check_families(families, tests)
    records: list[dict[str, Any]] = []
    for selection in selections(families):
        statuses: list[int] = []
        started = time.monotonic()
        while not statuses or (statuses[-1] != 0 and len(statuses) < RUNS):
            if statuses:
                print(
                    f"Running the tests of {selection.name} again",
                    file=sys.stderr,
                    flush=True,
                )
            _require(run([*cargo, "clean", "--profraw-only"], False), "cleaning")
            command = [*cargo, "--no-report", *RUN_OPTIONS, "--no-fail-fast", "--"]
            statuses.append(run(command + list(selection.arguments), False).returncode)
        seconds = round(time.monotonic() - started, 1)
        repaired = make_readable(profiles)
        if repaired:
            print(
                "Let the owner read and write "
                f"{_amount(repaired, 'profile', 'profiles')} of {selection.name}",
                file=sys.stderr,
                flush=True,
            )
        report = f"{selection.name}.lcov"
        _require(
            run(
                [*cargo, "report", *REPORT_OPTIONS, "--lcov", "--output-path"]
                + [str(output / report)],
                False,
            ),
            f"reporting {selection.name}",
        )
        records.append(
            {
                "name": selection.name,
                "arguments": list(selection.arguments),
                "statuses": statuses,
                "seconds": seconds,
                "report": report,
            }
        )
    commit = run(["git", "-C", str(REPO_ROOT), "rev-parse", "HEAD"], True)
    measurement = {
        "format": MEASUREMENT_FORMAT,
        "host": platform.system(),
        "commit": commit.stdout.strip() if commit.returncode == 0 else "unknown",
        "toolchain": toolchain,
        "rustc": _version(run, ["rustc", f"+{toolchain}", "--version"]),
        "cargo_llvm_cov": _version(run, [*cargo, "--version"]),
        "families": [
            {
                "slug": family.slug,
                "title": family.title,
                "filters": list(family.filters),
            }
            for family in families
        ],
        "test_modules": test_modules(crate),
        "selections": records,
    }
    (output / MEASUREMENT_FILE).write_text(
        json.dumps(measurement, indent=2) + "\n", "utf-8"
    )
    failed = [record["name"] for record in records if record["statuses"][-1] != 0]
    if failed:
        print(f"error: tests failed in {', '.join(failed)}", file=sys.stderr)
        return 1
    return 0


def summarize(directories: Sequence[Path], json_path: Path | None) -> str:
    """Returns the Markdown summary of the measurements in `directories`.

    Writes the lines and branches as JSON to `json_path` too, if given.
    """
    analyses = [analyze(directory) for directory in directories]
    labels = Counter(analysis.label for analysis in analyses)
    analyses = [
        analysis
        if labels[analysis.label] == 1
        else replace(analysis, label=f"{analysis.label} ({directory.name})")
        for analysis, directory in zip(analyses, directories, strict=True)
    ]
    if json_path is not None:
        json_path.write_text(json.dumps(to_json(analyses)) + "\n", "utf-8")
    return render(analyses)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Measure what each family of the nvx crate's golden tests "
        "alone reaches, on this host."
    )
    commands = parser.add_subparsers(dest="command", required=True)
    measuring = commands.add_parser(
        "measure", help="run the tests of every selection and write their reports"
    )
    measuring.add_argument(
        "--output", type=Path, required=True, help="the directory of the reports"
    )
    measuring.add_argument(
        "--toolchain",
        default="nightly",
        help="the rustup toolchain, a nightly one with the llvm-tools component "
        "(default: nightly)",
    )
    summarizing = commands.add_parser(
        "summarize", help="print the Markdown summary of measurements"
    )
    summarizing.add_argument(
        "directories", nargs="+", type=Path, help="the directories of measurements"
    )
    summarizing.add_argument(
        "--json", type=Path, help="also write the lines and branches to this file"
    )
    arguments = parser.parse_args(argv)
    try:
        if arguments.command == "measure":
            return measure(arguments.output, arguments.toolchain)
        sys.stdout.write(summarize(arguments.directories, arguments.json))
        return 0
    except MeasurementError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

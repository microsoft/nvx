#!/usr/bin/env python3
"""Measure which code of the nvx crate its own tests reach.

The script runs the crate's tests, with every feature, under cargo-llvm-cov, with
the compiler that rust-toolchain.toml pins and its llvm-tools component, and
writes what they reached on this host to an output directory:

- lcov.info and html/: the LCOV and HTML reports of the crate's production code,
  with paths relative to the crate;
- coverage.json: llvm-cov's summary of each file of production code and of the
  test doubles;
- tests.log: the output of the tests;
- measurement.json: the commit, the tools, and how the tests went;
- summary.md: the totals of the production code and of the test doubles, the
  coverage of each module, and what the measurement misses.

Production code is every source file of the crate but its test code and test
data, its tests directories and tests.rs files, and its test doubles, the fake
OpenVMM of src/testing/ and its binary, which the summary counts apart. The
tests that fail run once more, and the summary names those that passed only
then.

With --base REVISION, the summary also lists, by file, the lines of production
code that the working tree adds or changes since the merge base of REVISION and
HEAD and that no test reaches, with the share of those of them that the LCOV
report instruments that the tests reach, and, when the compiler measures
branches, the branches on the changed lines that no test takes. The "nvx-rs
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
import stat
import subprocess
import sys
import time
from collections.abc import Callable, Collection, Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast

REPO_ROOT = Path(__file__).resolve().parents[1]
CRATE_DIR = REPO_ROOT / "nvx-rs"
DEFAULT_OUTPUT = REPO_ROOT / "build" / "nvx-rs-coverage"
MEASUREMENT_FILE = "measurement.json"
MEASUREMENT_FORMAT = 1
COVERAGE_FILE = "coverage.json"
LCOV_FILE = "lcov.info"
HTML_DIRECTORY = "html"
LOG_FILE = "tests.log"
SUMMARY_FILE = "summary.md"
# The runs remap the crate's paths to paths relative to it, which the reports
# keep, so that the reports of different hosts compare and the patterns below
# match wherever the crate is. The reports need the option too. Cargo prints
# its diagnostics, which failed_targets() reads, in plain text whatever
# CARGO_TERM_COLOR says.
RUN_OPTIONS = (
    "--remap-path-prefix",
    "--all-features",
    "--locked",
    "--no-fail-fast",
    "--color",
    "never",
)
REPORT_OPTIONS = ("--remap-path-prefix", "--locked")
# Test code and test data: the crate's tests directories and tests.rs files.
# cargo-llvm-cov leaves them out only where a path has the host's separators,
# but a unit test includes a test vector through a path that keeps `/` on
# Windows.
TEST_CODE = r"(^|[/\\])tests([/\\]|\.rs$)"
# The test doubles, which the summary counts apart from production code.
TEST_DOUBLES = r"^src[/\\](testing([/\\]|\.rs$)|bin[/\\]nvx-fake-openvmm\.rs$)"
# The binaries whose processes the tests start on every host.
TESTED_BINARIES = ("nvx-rs", "nvx-fake-openvmm")
METRICS = ("lines", "functions", "regions", "branches")
# The failed tests of a target run again by name while their names take no
# more characters than this, which keeps the command within Windows' limit;
# otherwise the whole target runs again.
RETRY_NAMES_LIMIT = 8192
# How --base compares the working tree with the merge base: without context,
# with Git's rename detection, and with the colors, prefixes, external diff
# drivers, text conversions, and relative paths that the parser expects,
# whatever the Git configuration says.
DIFF_OPTIONS = (
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--find-renames",
    "--unified=0",
    "--src-prefix=a/",
    "--dst-prefix=b/",
)


class MeasurementError(Exception):
    """A measurement that cannot run, or a report that cannot be read."""


Runner = Callable[[Sequence[str], bool], subprocess.CompletedProcess[str]]


def environment(base: Mapping[str, str] | None = None) -> dict[str, str]:
    """Returns the environment of the measurement's commands: this process's,
    or `base`, without NVX_GUEST_BINARIES.

    The guest binaries' tests run the binaries that NVX_GUEST_BINARIES names,
    which are not instrumented, instead of Cargo's.
    """
    result = dict(os.environ if base is None else base)
    result.pop("NVX_GUEST_BINARIES", None)
    return result


def run_command(command: Sequence[str], echo: bool) -> subprocess.CompletedProcess[str]:
    """Runs `command` in the crate with the measurement's environment.

    With `echo`, it copies the bytes of the command's output and errors to
    standard output as they come, whatever their encoding, and returns them
    together; otherwise it returns the output alone and lets the errors
    through.
    """
    print("+ " + shlex.join(command), file=sys.stderr, flush=True)
    if not echo:
        return subprocess.run(
            list(command),
            cwd=CRATE_DIR,
            env=environment(),
            stdout=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
    chunks: list[bytes] = []
    sys.stdout.flush()
    with subprocess.Popen(
        list(command),
        cwd=CRATE_DIR,
        env=environment(),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    ) as process:
        assert process.stdout is not None
        for line in process.stdout:
            sys.stdout.buffer.write(line)
            chunks.append(line)
        sys.stdout.buffer.flush()
        status = process.wait()
    output = b"".join(chunks).decode("utf-8", errors="replace")
    return subprocess.CompletedProcess(list(command), status, output)


def _require(result: subprocess.CompletedProcess[str], what: str) -> None:
    if result.returncode != 0:
        raise MeasurementError(f"{what} failed with status {result.returncode}")


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


def _integer(value: Any, what: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool):
        raise MeasurementError(f"{what} is not a JSON integer")
    return value


def profile_directory(run: Runner, crate: Path = CRATE_DIR) -> Path:
    """Returns the directory to which cargo-llvm-cov's runs write their profiles.

    That is its target directory: CARGO_LLVM_COV_TARGET_DIR, if set, which a
    relative path names in the crate, where cargo-llvm-cov runs, or else the
    llvm-cov-target directory in Cargo's target directory.
    """
    override = os.environ.get("CARGO_LLVM_COV_TARGET_DIR")
    if override:
        return crate / override
    result = run(["cargo", "metadata", "--format-version", "1", "--no-deps"], False)
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


def stray_profiles(crate: Path) -> set[str]:
    """Returns the names of the profiles in the crate's directory.

    A process that a test starts without LLVM_PROFILE_FILE writes its profile
    under the instrumented build's default name into its working directory,
    which is the crate's for the processes that start there, rather than where
    cargo-llvm-cov merges the profiles from.
    """
    return {path.name for path in crate.glob("default_*.profraw")}


@dataclass(frozen=True)
class FailedTarget:
    """A test target whose tests failed: the arguments of cargo test that
    select it, as Cargo gives them, such as --lib, and the tests that its
    harness listed as failed, or none if it listed none, as when it crashed."""

    arguments: str
    tests: tuple[str, ...]


_STARTED_TARGET = re.compile(r"\s+(Running|Doc-tests) .*")
_FAILED_TARGET = re.compile(r"error: test failed, to rerun pass `(.+)`")
_FAILED_TEST = re.compile(r"    (\S+)")
_RESULT = re.compile(r"test result: \w+\. (\d+) passed; \d+ failed;.*")


def failed_targets(output: str) -> list[FailedTarget]:
    """Returns the targets whose tests failed in `output`, the output of cargo
    test, in their order.

    The tests of a target are those that the target's harness listed as
    failed last before Cargo reported the target, since the processes that
    tests start can print listings of their own before the harness's.
    """
    targets: list[FailedTarget] = []
    failed: tuple[str, ...] = ()
    lines = output.splitlines()
    for index, line in enumerate(lines):
        if _STARTED_TARGET.fullmatch(line):
            failed = ()
        elif line == "failures:":
            names: list[str] = []
            for following in lines[index + 1 :]:
                listed = _FAILED_TEST.fullmatch(following)
                if listed is None:
                    break
                names.append(listed.group(1))
            if names:
                failed = tuple(names)
        elif reported := _FAILED_TARGET.fullmatch(line):
            targets.append(FailedTarget(reported.group(1), failed))
            failed = ()
    return targets


def passed_tests(output: str) -> int | None:
    """Returns how many tests passed by the last result in `output`, which is
    the harness's own after those of the processes that its tests started, or
    None if there is none."""
    passed = [
        int(result.group(1))
        for line in output.splitlines()
        if (result := _RESULT.fullmatch(line))
    ]
    return passed[-1] if passed else None


@dataclass(frozen=True)
class Retry:
    """A run again of the failed tests of a target: the target, the tests that
    ran again, or none if every test of the target did, those of them that
    failed again, and the status of the run."""

    target: str
    tests: tuple[str, ...]
    failed: tuple[str, ...]
    status: int

    def passed(self) -> bool:
        """Returns whether every test that ran again passed."""
        return not self.failed and self.status == 0

    def to_json(self) -> dict[str, Any]:
        return {
            "target": self.target,
            "tests": list(self.tests),
            "failed": list(self.failed),
            "status": self.status,
        }


def retry(cargo: Sequence[str], target: FailedTarget, run: Runner) -> Retry:
    """Runs the failed tests of `target` once more, by name, or the whole
    target if its harness named none or their names take too much room."""
    selection = shlex.split(target.arguments)
    tests = target.tests if sum(map(len, target.tests)) <= RETRY_NAMES_LIMIT else ()
    command = [*cargo, "--no-report", *RUN_OPTIONS, *selection]
    if tests:
        command += ["--", "--exact", *tests]
    result = run(command, True)
    if not tests:
        return Retry(target.arguments, (), (), result.returncode)
    listed = next(
        (
            failed.tests
            for failed in failed_targets(result.stdout)
            if failed.arguments == target.arguments
        ),
        (),
    )
    if listed:
        failed = set(listed)
    elif result.returncode == 0:
        failed = set[str]()
    else:
        failed = set(tests)
    # The tests that the harness did not list as failed passed only if it ran
    # each of them, which it doesn't for a name that no test has.
    if passed_tests(result.stdout) != sum(test not in failed for test in tests):
        failed = set(tests)
    failed_again = tuple(test for test in tests if test in failed)
    return Retry(target.arguments, tests, failed_again, result.returncode)


def _retries(measurement: Mapping[str, Any]) -> list[Retry]:
    retries: list[Retry] = []
    for item in _array(measurement.get("retries"), "retries"):
        retried = _object(item, "a retry")
        retries.append(
            Retry(
                _string(retried.get("target"), "a retry's target"),
                _strings(retried.get("tests"), "a retry's tests"),
                _strings(retried.get("failed"), "a retry's failed tests"),
                _integer(retried.get("status"), "a retry's status"),
            )
        )
    return retries


@dataclass(frozen=True)
class Count:
    """How many lines, functions, regions, or branches some code has, and how
    many of them the tests reached."""

    covered: int
    count: int

    def __add__(self, other: Count) -> Count:
        return Count(self.covered + other.covered, self.count + other.count)


Counts = dict[str, Count]


def _counts(summary: Any, what: str) -> Counts:
    entries = _object(summary, what)
    counts: Counts = {}
    for metric in METRICS:
        entry = _object(entries.get(metric, {"covered": 0, "count": 0}), metric)
        counts[metric] = Count(
            _integer(entry.get("covered"), f"{what}: the covered {metric}"),
            _integer(entry.get("count"), f"{what}: the {metric}"),
        )
    return counts


def _add(total: Counts, counts: Counts) -> None:
    for metric, count in counts.items():
        total[metric] = total.get(metric, Count(0, 0)) + count


def source_path(filename: str) -> str:
    """Returns a report's path of a source file with `/` as its separators."""
    return filename.replace("\\", "/").removeprefix("./")


def is_test_code(path: str) -> bool:
    """Returns whether `path`, relative to the crate, holds test code or data."""
    return re.search(TEST_CODE, path) is not None


def is_test_double(path: str) -> bool:
    """Returns whether `path`, relative to the crate, holds a test double."""
    return re.search(TEST_DOUBLES, path) is not None


def parent_modules(paths: Iterable[str]) -> set[str]:
    """Returns the submodules of the crate's top-level modules that have
    submodules of their own, as directories of the source files `paths`."""
    return {
        "/".join(parts[:3])
        for parts in (path.split("/") for path in paths)
        if len(parts) > 3 and parts[0] == "src" and parts[1] != "bin"
    }


def module_of(path: str, parents: Collection[str]) -> str:
    """Returns the row of the module table that counts the source file `path`.

    A row counts the files of a top-level module of the crate, or of one of
    its submodules in `parents`, those that have submodules of their own,
    that no other row counts. The binaries' files share a row.
    """
    parts = path.split("/")
    if len(parts) < 2 or parts[0] != "src":
        raise MeasurementError(f"{path} is not a source file of the crate")
    if parts[1] == "bin" or len(parts) == 2:
        return "src/" + parts[1].removesuffix(".rs")
    submodule = "/".join(parts[:3]).removesuffix(".rs")
    return submodule if submodule in parents else "src/" + parts[1]


@dataclass(frozen=True)
class Coverage:
    """What the tests reached of the production code, of the test doubles,
    and of each row of the module table, and whether processes of each binary
    wrote profiles."""

    production: Counts
    test_doubles: Counts
    modules: dict[str, Counts]
    binaries: dict[str, bool]


def analyze(report: Any) -> Coverage:
    """Reads llvm-cov's JSON summary of the production code and the test
    doubles, whose paths are relative to the crate."""
    data = _array(_object(report, "the coverage report").get("data"), "data")
    if len(data) != 1:
        raise MeasurementError("the coverage report does not hold one export")
    files: dict[str, Counts] = {}
    for entry in _array(_object(data[0], "the export").get("files"), "files"):
        file = _object(entry, "a file")
        path = source_path(_string(file.get("filename"), "a file's name"))
        if not is_test_code(path):
            files[path] = _counts(file.get("summary"), path)
    production: Counts = {metric: Count(0, 0) for metric in METRICS}
    test_doubles: Counts = {metric: Count(0, 0) for metric in METRICS}
    modules: dict[str, Counts] = {}
    parents = parent_modules(path for path in files if not is_test_double(path))
    binaries: dict[str, bool] = {}
    for path, counts in sorted(files.items()):
        directory, _, name = path.rpartition("/")
        if directory == "src/bin" and name.endswith(".rs"):
            binaries[name.removesuffix(".rs")] = counts["lines"].covered > 0
        if is_test_double(path):
            _add(test_doubles, counts)
        else:
            _add(production, counts)
            _add(modules.setdefault(module_of(path, parents), {}), counts)
    return Coverage(production, test_doubles, modules, binaries)


def git(repository: Path, *arguments: str) -> bytes:
    """Runs git with `arguments` in `repository` and returns its output."""
    result = subprocess.run(
        ["git", "-C", str(repository), *arguments],
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        errors = result.stderr.decode("utf-8", errors="replace").strip()
        failed = f"git {shlex.join(arguments)} failed with status {result.returncode}"
        raise MeasurementError(f"{failed}: {errors}" if errors else failed)
    return result.stdout


# The escapes with which Git quotes the bytes of unusual file names, besides
# the octal ones.
_GIT_ESCAPES = {
    "a": 0x07,
    "b": 0x08,
    "t": 0x09,
    "n": 0x0A,
    "v": 0x0B,
    "f": 0x0C,
    "r": 0x0D,
    '"': 0x22,
    "\\": 0x5C,
}
_OCTAL = re.compile(r"[0-3][0-7]{2}")
_HUNK = re.compile(r"@@ -\d+(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")


def _unquote(name: str) -> str:
    """Returns the file name that `name` gives in a diff's header: `name`
    itself, or, if Git quoted it, what the C-style string between the quotes
    says, whose escapes stand for bytes of the name's UTF-8."""
    if len(name) < 2 or not (name.startswith('"') and name.endswith('"')):
        return name
    text = name[1:-1]
    data = bytearray()
    index = 0
    while index < len(text):
        character = text[index]
        escape = text[index + 1 : index + 2]
        if character != "\\":
            data += character.encode("utf-8")
            index += 1
        elif escape in _GIT_ESCAPES:
            data.append(_GIT_ESCAPES[escape])
            index += 2
        elif _OCTAL.fullmatch(text, index + 1, index + 4):
            data.append(int(text[index + 1 : index + 4], 8))
            index += 4
        else:
            raise MeasurementError(f"the diff quotes the file name {name} oddly")
    return data.decode("utf-8", errors="replace")


def _new_path(name: str) -> str | None:
    """Returns the path that the name of a diff's +++ line gives the file in
    the new tree, or None if the line names none, as for a deleted file."""
    # Git ends a name that holds a space with a tab, as patch expects.
    name = _unquote(name.removesuffix("\t"))
    if name == "/dev/null":
        return None
    if not name.startswith("b/"):
        raise MeasurementError(f"the diff names the file {name} without its prefix")
    return name.removeprefix("b/")


def added_lines(diff: str) -> dict[str, set[int]]:
    """Returns the lines that `diff`, a unified diff of Git, adds or changes in
    each file, by the file's path in the new tree. A file that it deletes, or
    only renames or changes the mode of, has none."""
    added: dict[str, set[int]] = {}
    path: str | None = None
    lines = diff.split("\n")
    index = 0
    while index < len(lines):
        line = lines[index]
        index += 1
        if line.startswith("diff --git "):
            path = None
        elif line.startswith("+++ "):
            path = _new_path(line[4:])
        elif hunk := _HUNK.match(line):
            old = 1 if hunk.group(1) is None else int(hunk.group(1))
            number = int(hunk.group(2))
            new = 1 if hunk.group(3) is None else int(hunk.group(3))
            # A hunk holds as many lines of each side as its header says,
            # whatever they hold, such as an added line that begins with "++".
            while old or new:
                if index == len(lines):
                    raise MeasurementError("the diff ends within a hunk")
                body = lines[index]
                index += 1
                if body.startswith("+") and new:
                    if path is None:
                        raise MeasurementError("the diff adds lines to no file")
                    added.setdefault(path, set()).add(number)
                    number += 1
                    new -= 1
                elif body.startswith("-") and old:
                    old -= 1
                elif body.startswith(" ") and old and new:
                    number += 1
                    old -= 1
                    new -= 1
                elif not body.startswith("\\"):
                    raise MeasurementError(
                        f"the diff holds a line that its hunk lacks: {body!r}"
                    )
    return added


@dataclass(frozen=True)
class Changes:
    """The lines that the working tree adds or changes in each file of the
    crate since a merge base, by the file's path relative to the crate."""

    merge_base: str
    lines: dict[str, frozenset[int]]


def changes_since(
    base: str, repository: Path = REPO_ROOT, crate: Path = CRATE_DIR
) -> Changes:
    """Returns the lines of the crate's files that the working tree of
    `repository` adds or changes since the merge base of `base` and HEAD.

    The measurement builds the working tree, so the changes include those that
    no commit holds yet, but not untracked files, which no diff holds.
    """
    merge_base = git(repository, "merge-base", base, "HEAD").decode().strip()
    prefix = crate.relative_to(repository).as_posix() + "/"
    diff = git(repository, "diff", *DIFF_OPTIONS, merge_base, "--", prefix)
    lines = added_lines(diff.decode("utf-8", errors="replace"))
    return Changes(
        merge_base,
        {
            path.removeprefix(prefix): frozenset(numbers)
            for path, numbers in lines.items()
            if path.startswith(prefix)
        },
    )


@dataclass(frozen=True)
class FileLines:
    """What an LCOV report says of a file: how many times the tests ran each
    line that it instruments, and, if the compiler measured branches, whether
    they took each branch of a line, by the branch's block and number."""

    hits: dict[int, int]
    branches: dict[int, dict[tuple[str, str], bool]]


def read_lcov(text: str) -> dict[str, FileLines]:
    """Reads an LCOV report: what it says of each of its files, by the file's
    path with `/` as its separators."""
    files: dict[str, FileLines] = {}
    file: FileLines | None = None
    for number, line in enumerate(text.splitlines(), 1):
        record, _, value = line.partition(":")
        try:
            if record == "SF":
                file = files.setdefault(source_path(value), FileLines({}, {}))
            elif record == "end_of_record":
                file = None
            elif record in ("DA", "BRDA"):
                if file is None:
                    raise ValueError(f"{record} outside of a file's record")
                fields = value.split(",")
                if record == "DA":
                    hit, count = int(fields[0]), int(fields[1])
                    file.hits[hit] = file.hits.get(hit, 0) + count
                else:
                    branched, block, branch, taken = fields
                    flags = file.branches.setdefault(int(branched), {})
                    # A branch whose block never ran was not taken: "-".
                    flags[(block, branch)] = flags.get((block, branch), False) or (
                        taken != "-" and int(taken) > 0
                    )
        except (ValueError, IndexError) as error:
            raise MeasurementError(
                f"line {number} of the LCOV report: {error}"
            ) from error
    return files


@dataclass(frozen=True)
class ChangedFile:
    """What the tests reached of the lines that changes add or change in a
    file of production code: how many of those that the report instruments
    they reached, the others, and the changed lines with branches that they
    did not all take, with how many of the line's branches they took."""

    path: str
    lines: Count
    missed: tuple[int, ...]
    branches: tuple[tuple[int, Count], ...]


@dataclass(frozen=True)
class ChangedCoverage:
    """What the tests reached of the lines that the changes since a merge base
    add or change, by file, and whether the report measures branches, which
    only some compilers do."""

    merge_base: str
    files: tuple[ChangedFile, ...]
    branches: bool

    def lines(self) -> Count:
        """Returns how many of the changed lines the report instruments, and
        how many of them the tests reached."""
        total = Count(0, 0)
        for file in self.files:
            total += file.lines
        return total


def changed_coverage(
    changes: Changes, report: Mapping[str, FileLines]
) -> ChangedCoverage:
    """Returns what the tests reached of the lines that `changes` add or
    change in the files of `report`, an LCOV report of production code. A line
    that the report does not instrument counts for nothing: a comment, a blank
    line, a declaration, or a line of a file that the report lacks, such as a
    test, or code that this host doesn't compile."""
    files: list[ChangedFile] = []
    for path, numbers in sorted(changes.lines.items()):
        found = report.get(path)
        if found is None:
            continue
        instrumented = sorted(number for number in numbers if number in found.hits)
        if not instrumented:
            continue
        missed = tuple(number for number in instrumented if found.hits[number] <= 0)
        branches: list[tuple[int, Count]] = []
        for number in instrumented:
            taken = found.branches.get(number, {})
            if not all(taken.values()):
                branches.append((number, Count(sum(taken.values()), len(taken))))
        reached = Count(len(instrumented) - len(missed), len(instrumented))
        files.append(ChangedFile(path, reached, missed, tuple(branches)))
    measured = any(found.branches for found in report.values())
    return ChangedCoverage(changes.merge_base, tuple(files), measured)


def share(count: Count) -> str:
    """Returns the share of `count` that the tests reached, rounded down, so
    that only code that they reached entirely shows 100%."""
    if count.count == 0:
        return "none"
    percent = math.floor(1000 * count.covered / count.count) / 10
    return f"{percent:.1f}% of {count.count:,}"


def _table(
    header: str, rows: Iterable[tuple[str, Counts]], metrics: Sequence[str]
) -> list[str]:
    lines = [
        f"| {header} | " + " | ".join(metric.capitalize() for metric in metrics) + " |",
        "| --- |" + " ---: |" * len(metrics),
    ]
    for name, counts in rows:
        cells = (share(counts.get(metric, Count(0, 0))) for metric in metrics)
        lines.append(f"| {name} | " + " | ".join(cells) + " |")
    return lines


def join_names(names: Iterable[str]) -> str:
    """Returns `names` in code spans, as a list in a sentence: `a`, `a` and
    `b`, or `a`, `b`, and `c`."""
    quoted = [f"`{name}`" for name in names]
    if len(quoted) < 3:
        return " and ".join(quoted)
    return ", ".join(quoted[:-1]) + ", and " + quoted[-1]


def problems(measurement: Mapping[str, Any], coverage: Coverage) -> list[str]:
    """Returns, in Markdown, what the measurement misses: what the tests that
    failed again would have reached, and what processes that the tests started
    reached but wrote where cargo-llvm-cov doesn't find it, or didn't write."""
    found: list[str] = []
    for retried in _retries(measurement):
        if retried.failed:
            found.append(
                f"The tests {join_names(retried.failed)} of `{retried.target}` failed "
                "when they ran again."
            )
        elif retried.status != 0:
            found.append(f"The tests of `{retried.target}` failed when they ran again.")
    missing = [name for name in TESTED_BINARIES if not coverage.binaries.get(name)]
    if missing:
        found.append(
            f"No process of {join_names(missing)} wrote a profile, though the tests "
            "start them."
        )
    strays = _strings(measurement.get("stray_profiles"), "stray_profiles")
    if strays:
        found.append(
            f"{len(strays):,} processes wrote their profiles into the crate's "
            "directory, as processes that a test starts there without "
            f"`LLVM_PROFILE_FILE` do: {join_names(strays)}."
        )
    return found


def _ranges(numbers: Iterable[int]) -> str:
    """Returns line numbers as sorted ranges, such as `3-5, 9`."""
    ranges: list[tuple[int, int]] = []
    for number in sorted(numbers):
        if ranges and number == ranges[-1][1] + 1:
            ranges[-1] = (ranges[-1][0], number)
        else:
            ranges.append((number, number))
    return ", ".join(
        str(first) if first == last else f"{first}-{last}" for first, last in ranges
    )


def render_changes(changed: ChangedCoverage) -> list[str]:
    """Returns the lines of the summary's Markdown section on the lines that
    the changes since a merge base add or change and that no test reaches."""
    since = f"since `{changed.merge_base[:12]}`"
    lines = ["### Changed lines that no test reaches", ""]
    total = changed.lines()
    if total.count == 0:
        return [
            *lines,
            f"The changes {since} add or change no instrumented line of "
            "production code.",
            "",
        ]
    lines += [
        f"Tests reach {share(total)} instrumented lines of production code that "
        f"the changes {since} add or change.",
        "",
    ]
    rows = [file for file in changed.files if file.missed or file.branches]
    if not rows:
        return lines
    if changed.branches:
        lines += [
            "By file, the lines that no test reaches, and the changed lines with "
            "branches that no test takes, with how many of the line's branches no "
            "test takes:",
            "",
            "| File | Lines that no test reaches | Branches that no test takes |",
            "| --- | --- | --- |",
        ]
        for file in rows:
            branches = ", ".join(
                f"{number} ({count.count - count.covered} of {count.count})"
                for number, count in file.branches
            )
            missed = _ranges(file.missed)
            lines.append(
                f"| `{file.path}` | {missed or 'none'} | {branches or 'none'} |"
            )
    else:
        lines += [
            "By file, the lines that no test reaches:",
            "",
            "| File | Lines |",
            "| --- | --- |",
        ]
        lines += [f"| `{file.path}` | {_ranges(file.missed)} |" for file in rows]
    lines.append("")
    return lines


def render(
    measurement: Mapping[str, Any],
    coverage: Coverage,
    changed: ChangedCoverage | None = None,
) -> str:
    """Returns the Markdown summary of a measurement and of its coverage, and
    of what the tests reached of the changes in `changed`, if any."""
    host = _string(measurement.get("host"), "host")
    commit = _string(measurement.get("commit"), "commit")
    rustc = _string(measurement.get("rustc"), "rustc")
    tool = _string(measurement.get("cargo_llvm_cov"), "cargo_llvm_cov")
    # Branches need a nightly compiler, which the measurement doesn't use.
    metrics = [metric for metric in METRICS if coverage.production[metric].count]
    lines = [
        f"## nvx-rs coverage on {host}",
        "",
        f"What the tests of the `nvx` crate reached at `{commit[:12]}`, with every "
        f"feature, built by {rustc} and measured by {tool}. Production code "
        "leaves out test code, test data, and the test doubles, the fake OpenVMM "
        "of `src/testing/` and its `nvx-fake-openvmm` binary, which count apart.",
        "",
        *_table(
            "Code",
            (
                ("Production", coverage.production),
                ("Test doubles", coverage.test_doubles),
            ),
            metrics,
        ),
        "",
    ]
    binaries = sorted(coverage.binaries.items())
    wrote = [name for name, profiles in binaries if profiles]
    other = [name for name, profiles in binaries if not profiles]
    sentence = f"The profiles include processes of {join_names(wrote) or 'no binary'}."
    if other:
        sentence += f" No process of {join_names(other)} wrote one."
    lines += [sentence, ""]
    for retried in _retries(measurement):
        passed = [test for test in retried.tests if test not in retried.failed]
        if passed:
            lines.append(
                f"The tests {join_names(passed)} of `{retried.target}` passed only when "
                "they ran again."
            )
        elif not retried.tests and retried.passed():
            lines.append(
                f"The tests of `{retried.target}` passed only when they ran again."
            )
        else:
            continue
        lines.append("")
    found = problems(measurement, coverage)
    if found:
        lines += ["The measurement misses what these reached:", ""]
        lines += [f"- {problem}" for problem in found]
        lines.append("")
    if changed is not None:
        lines += render_changes(changed)
    lines += [
        "### Production code by module",
        "",
        "A row counts the files of a top-level module, or of one of its "
        "submodules that has submodules of its own, that no other row counts.",
        "",
        *_table(
            "Module",
            (
                (f"`{name}`", counts)
                for name, counts in sorted(coverage.modules.items())
            ),
            metrics,
        ),
    ]
    return "\n".join(lines) + "\n"


def version(run: Runner, command: Sequence[str]) -> str:
    """Returns the first line that `command` prints, or "unknown" if it fails
    or prints nothing."""
    result = run(command, False)
    output = result.stdout.strip() if result.returncode == 0 else ""
    return output.splitlines()[0] if output else "unknown"


@dataclass(frozen=True)
class TestRun:
    """A run of the crate's tests under cargo-llvm-cov: the directory of its
    profiles, the status of its first run, how its failed tests ran again,
    the profiles that it left in the crate's directory, and how long it
    took, in seconds."""

    profiles: Path
    status: int
    retries: tuple[Retry, ...]
    strays: tuple[str, ...]
    seconds: float


def run_tests(log: Path, run: Runner = run_command, crate: Path = CRATE_DIR) -> TestRun:
    """Runs the crate's tests under cargo-llvm-cov, without a report, after it
    removes what earlier runs built and measured, runs the tests that fail
    once more, and lets the owner read and write every profile. It writes the
    output of the tests to `log` as they run."""
    cargo = ["cargo", "llvm-cov"]
    _require(run([*cargo, "clean", "--workspace"], True), "cleaning")
    profiles = profile_directory(run, crate)
    earlier_strays = stray_profiles(crate)
    transcript: list[str] = []

    def run_logged(
        command: Sequence[str], echo: bool
    ) -> subprocess.CompletedProcess[str]:
        result = run(command, echo)
        transcript.append(f"+ {shlex.join(command)}\n{result.stdout}")
        log.write_text("".join(transcript), encoding="utf-8")
        return result

    started = time.monotonic()
    result = run_logged([*cargo, "--no-report", *RUN_OPTIONS], True)
    failed = failed_targets(result.stdout)
    if result.returncode != 0 and not failed:
        raise MeasurementError(
            f"the tests did not run: cargo llvm-cov failed with status "
            f"{result.returncode}"
        )
    retries = tuple(retry(cargo, target, run_logged) for target in failed)
    seconds = round(time.monotonic() - started, 1)
    strays = tuple(sorted(stray_profiles(crate) - earlier_strays))
    repaired = make_readable(profiles)
    if repaired:
        print(
            f"Let the owner read and write {repaired:,} profiles",
            file=sys.stderr,
            flush=True,
        )
    return TestRun(profiles, result.returncode, retries, strays, seconds)


def measure(
    output: Path,
    run: Runner = run_command,
    crate: Path = CRATE_DIR,
    changes: Changes | None = None,
) -> int:
    """Measures the coverage of the crate's tests into `output`, and returns
    0, or 1 if the measurement misses something, as problems() tells. It
    writes the reports either way, and the summary lists the lines of
    `changes`, if any, that no test reaches."""
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    tests = run_tests(output / LOG_FILE, run, crate)
    cargo = ["cargo", "llvm-cov"]
    report = [*cargo, "report", *REPORT_OPTIONS]
    without_tests = ["--ignore-filename-regex", TEST_CODE]
    production = ["--ignore-filename-regex", f"{TEST_CODE}|{TEST_DOUBLES}"]
    for options, what in (
        (
            [*without_tests, "--json", "--summary-only", "--output-path"]
            + [str(output / COVERAGE_FILE)],
            "the summary",
        ),
        ([*production, "--lcov", "--output-path", str(output / LCOV_FILE)], "LCOV"),
        ([*production, "--html", "--output-dir", str(output)], "HTML"),
    ):
        _require(run([*report, *options], True), f"reporting {what}")
    commit = run(["git", "-C", str(REPO_ROOT), "rev-parse", "HEAD"], False)
    measurement: dict[str, Any] = {
        "format": MEASUREMENT_FORMAT,
        "host": platform.system(),
        "commit": commit.stdout.strip() if commit.returncode == 0 else "unknown",
        "rustc": version(run, ["rustc", "--version"]),
        "cargo_llvm_cov": version(run, [*cargo, "--version"]),
        "seconds": tests.seconds,
        "status": tests.status,
        "retries": [retried.to_json() for retried in tests.retries],
        "stray_profiles": list(tests.strays),
    }
    (output / MEASUREMENT_FILE).write_text(
        json.dumps(measurement, indent=2) + "\n", encoding="utf-8"
    )
    try:
        report_data = json.loads((output / COVERAGE_FILE).read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise MeasurementError(f"{output / COVERAGE_FILE}: {error}") from error
    coverage = analyze(report_data)
    changed = None
    if changes is not None:
        try:
            lcov = (output / LCOV_FILE).read_text(encoding="utf-8")
        except OSError as error:
            raise MeasurementError(f"{output / LCOV_FILE}: {error}") from error
        changed = changed_coverage(changes, read_lcov(lcov))
    (output / SUMMARY_FILE).write_text(
        render(measurement, coverage, changed), encoding="utf-8"
    )
    found = problems(measurement, coverage)
    for problem in found:
        print(f"error: {problem}", file=sys.stderr)
    return 1 if found else 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Measure which code of the nvx crate its own tests reach."
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=DEFAULT_OUTPUT,
        help="directory of the reports (default: build/nvx-rs-coverage)",
    )
    parser.add_argument(
        "--base",
        metavar="REVISION",
        help="also list the lines of production code that the working tree adds "
        "or changes since the merge base of REVISION and HEAD and that no test "
        "reaches",
    )
    arguments = parser.parse_args(argv)
    try:
        base = cast(str | None, arguments.base)
        changes = None if base is None else changes_since(base)
        status = measure(cast(Path, arguments.output), changes=changes)
    except (MeasurementError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"Wrote the reports to {arguments.output}", file=sys.stderr)
    return status


if __name__ == "__main__":
    sys.exit(main())

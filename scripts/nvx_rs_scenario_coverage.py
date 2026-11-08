#!/usr/bin/env python3
"""Measure which host code of the nvx crate the KVM microVM scenarios reach.

On a Linux host with KVM, the script builds an instrumented nvx-rs, in release
mode with the `cli` feature, as CI builds the one that the scenarios run, in
the environment that `cargo llvm-cov show-env --no-rustc-wrapper
--remap-path-prefix` prints. With that environment, and with an
LLVM_PROFILE_FILE that gives each process a profile of its own, it runs:

- the microVM scenarios of `nvx.py test-microvm --backend kvm`, whose NVX_RS
  names a wrapper that records the start of each nvx-rs process and then
  becomes the instrumented nvx-rs;
- the runtime adapter's suite of `nvx.py test-runtime-adapter --backend kvm`,
  whose host program and tests Cargo builds on the crate with the same
  instrumentation.

It then runs the crate's own tests as scripts/nvx_rs_coverage.py does, merges
the profiles of the scenarios, of the crate's tests, and of both, and writes
to an output directory what each reached of the crate's production code,
which leaves out test code, test data, and the test doubles, as
nvx_rs_coverage.py counts it:

- summary.md: the lines and functions that each reached, in all, and the
  lines by module, with those that only the scenarios reach; what the
  scenarios reach of the lines that their builds compile; the nvx-rs
  processes that wrote no profile; and what the measurement misses;
- measurement.json: the commit, the tools, and how the runs went;
- processes.json: each nvx-rs process that the scenarios started, with the
  span of test-microvm in which it started, its arguments, and whether it
  wrote a profile;
- crate-tests/, scenarios/, and together/: the LCOV report of the production
  code that each reached, lcov.info, and llvm-cov's summary of each file,
  coverage.json; scenarios/ and together/ also hold an HTML report, html/;
- microvm/: the output directory of test-microvm, with the logs of the
  scenarios and their duration report;
- build.log, microvm.log, runtime-adapter.log, and crate-tests.log: what the
  build of nvx-rs and the runs printed.

The "nvx-rs scenario coverage" section of doc/ci.md describes the measurement
and the workflow that runs it.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import time
from collections.abc import Callable, Collection, Iterable, Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Protocol, cast

import nvx_rs_coverage as coverage
from nvx_rs_coverage import Count, Counts, MeasurementError
from nvx_tools.adversarial_oracles import terminate_process_tree
from nvx_tools.runtime_adapter_tests import ADAPTER_DIRECTORY, GUEST_AGENT

REPO_ROOT = coverage.REPO_ROOT
CRATE_DIR = coverage.CRATE_DIR
NVX_PY = REPO_ROOT / "scripts" / "nvx.py"
ADAPTER_DIR = ADAPTER_DIRECTORY
DEFAULT_OUTPUT = REPO_ROOT / "build" / "nvx-rs-scenario-coverage"
# The guest agent that test-runtime-adapter takes by default: its release
# build in tests/runtime-adapter.
DEFAULT_GUEST_AGENT = GUEST_AGENT
# What a measurement builds and records, which the next one removes first. It
# lies beneath nvx-rs/target, where an nvx-rs finds the checkout's artifacts
# without --root, as one that `cargo build` makes does.
WORK_DIR = CRATE_DIR / "target" / "scenario-coverage"
BACKEND = "kvm"
MEASUREMENT_FORMAT = 1
SUMMARY_FILE = "summary.md"
MEASUREMENT_FILE = "measurement.json"
PROCESSES_FILE = "processes.json"
LCOV_FILE = "lcov.info"
COVERAGE_FILE = "coverage.json"
HTML_DIRECTORY = "html"
MICROVM_DIRECTORY = "microvm"
BUILD_LOG = "build.log"
MICROVM_LOG = "microvm.log"
ADAPTER_LOG = "runtime-adapter.log"
CRATE_TESTS_LOG = "crate-tests.log"
# test-microvm's scenario duration report, in its output directory.
DURATIONS_FILE = "scenario-durations.jsonl"
# The runs whose profiles the reports merge, and those whose reports include
# HTML.
RUNS = ("crate-tests", "scenarios", "together")
RUN_NAMES = {
    "crate-tests": "The crate's tests",
    "scenarios": "The scenarios",
    "together": "Together",
}
HTML_RUNS = ("scenarios", "together")
METRICS = ("lines", "functions")
# The environment that instruments every crate that Cargo builds, with the
# crate's paths relative to it, as nvx_rs_coverage.py's reports have them,
# and every other path absolute. cargo-llvm-cov's rustc wrapper would
# instrument only the crates of nvx-rs's workspace, so the runtime adapter's
# binaries, which link the crate, would link no profiler runtime and write no
# profile.
SHOW_ENV = (
    "cargo",
    "llvm-cov",
    "show-env",
    "--no-rustc-wrapper",
    "--remap-path-prefix",
)
# The variables of show-env that instrument the builds, of which Cargo takes
# CARGO_ENCODED_RUSTFLAGS first. The measurement names the profiles and the
# target directories itself.
FLAG_VARIABLES = ("CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS")
# The nvx-rs of the scenarios: a release build with the `cli` feature, as CI's
# nvx-rs build jobs make it, for the host's own target.
BUILD = (
    "cargo",
    "build",
    "--manifest-path",
    str(CRATE_DIR / "Cargo.toml"),
    "--release",
    "--locked",
    "--features",
    "cli",
    "--bin",
    "nvx-rs",
)
# Each process writes a profile of its own, named by its ID and by the
# signature of its binary, %m, with which processes that reuse an ID merge
# their profiles rather than replace one another's.
PROFILE_PATTERN = "%p-%m.profraw"
# The directories, beneath a measurement's profiles, of the nvx-rs processes,
# which the wrapper names; of the other processes of the build of nvx-rs and
# of test-microvm, such as the managed-lifecycle scenario's test; and of the
# processes of test-runtime-adapter. llvm-cov counts what a profile reached of
# a function for every executable that holds the function under the same
# symbol, so whether a group of executables wrote profiles shows only in the
# profiles of its own directory.
NVX_RS_PROFILES = "nvx-rs"
MICROVM_PROFILES = "microvm"
ADAPTER_PROFILES = "runtime-adapter"
# The reports leave out each file outside the crate, whose path the
# instrumentation leaves absolute, and test code, and the LCOV reports the
# test doubles too, as nvx_rs_coverage.py's do.
OUTSIDE_CRATE = "^/"
WITHOUT_TESTS = f"{OUTSIDE_CRATE}|{coverage.TEST_CODE}"
PRODUCTION = f"{WITHOUT_TESTS}|{coverage.TEST_DOUBLES}"
# The commands of nvx-rs that take an operation, such as `sandbox start`.
COMMAND_GROUPS = ("egress", "sandbox")
# How long after the end of test-microvm's last span a process still counts
# as one of that span's, for the rounding of the report's times.
SPAN_SLACK_SECONDS = 1.0
# The variable whose value, unique to a measurement, every process of its
# runs inherits, by which an interrupted measurement ends those that left its
# process tree, such as the VMs that the runtime adapter's host program starts
# in sessions of their own, for its later processes to reattach to.
RUN_VARIABLE = "NVX_SCENARIO_COVERAGE_RUN"
# How long an interrupted measurement kills such processes before it gives up.
END_SECONDS = 10.0


class Streamer(Protocol):
    """Runs a command of the measurement as stream() does."""

    def __call__(
        self,
        command: Sequence[str],
        environment: Mapping[str, str],
        log: Path,
        *,
        forward_interrupts: bool = False,
    ) -> int: ...


def stream(
    command: Sequence[str],
    environment: Mapping[str, str],
    log: Path,
    *,
    forward_interrupts: bool = False,
) -> int:
    """Runs `command` in the repository with `environment`, copying what it
    prints to standard output and to `log` as it comes, and returns its
    status.

    An interrupt of the measurement, such as the SIGINT and then the SIGTERM
    of a cancelled CI step, which reach the measurement alone, ends the
    command and every process that it started, which a signal that reached
    the command alone could leave running, as Cargo leaves the tests that it
    runs. With `forward_interrupts`, the first two interrupts reach the
    command instead, as nvx.py test-microvm takes them to end what it
    started, and the measurement waits for it to end; a third interrupt ends
    it and every process that it started.
    """
    print("+ " + shlex.join(command), file=sys.stderr, flush=True)
    sys.stdout.flush()
    signals = [signal.SIGINT, signal.SIGTERM] if forward_interrupts else []
    interrupted = False
    with (
        log.open("wb") as written,
        subprocess.Popen(
            list(command),
            cwd=REPO_ROOT,
            env=dict(environment),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        ) as process,
    ):
        assert process.stdout is not None
        try:
            while True:
                try:
                    for line in process.stdout:
                        sys.stdout.buffer.write(line)
                        sys.stdout.buffer.flush()
                        written.write(line)
                    status = process.wait()
                    break
                except KeyboardInterrupt:
                    if not signals:
                        raise
                    interrupted = True
                    process.send_signal(signals.pop(0))
        except BaseException:
            terminate_process_tree(process, process_group=False)
            raise
    if interrupted:
        raise KeyboardInterrupt
    return status


def run_command(command: Sequence[str], echo: bool) -> subprocess.CompletedProcess[str]:
    """Runs `command` as nvx_rs_coverage.run_command() does, except that,
    with `echo`, as for the crate's tests, an interrupt of the measurement
    ends the command and every process that it started before it ends the
    measurement, since Cargo would leave the tests that it runs running."""
    if not echo:
        return coverage.run_command(command, echo)
    print("+ " + shlex.join(command), file=sys.stderr, flush=True)
    chunks: list[bytes] = []
    sys.stdout.flush()
    with subprocess.Popen(
        list(command),
        cwd=CRATE_DIR,
        env=coverage.environment(),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    ) as process:
        assert process.stdout is not None
        try:
            for line in process.stdout:
                sys.stdout.buffer.write(line)
                chunks.append(line)
            sys.stdout.buffer.flush()
            status = process.wait()
        except BaseException:
            terminate_process_tree(process, process_group=False)
            raise
    output = b"".join(chunks).decode("utf-8", errors="replace")
    return subprocess.CompletedProcess(list(command), status, output)


def _require(result: subprocess.CompletedProcess[str], what: str) -> None:
    if result.returncode != 0:
        raise MeasurementError(f"{what} failed with status {result.returncode}")


def _display(path: Path) -> str:
    """Returns `path` relative to the repository if it lies there."""
    try:
        return path.relative_to(REPO_ROOT).as_posix()
    except ValueError:
        return str(path)


def llvm_tools(
    run: coverage.Runner, environment: Mapping[str, str]
) -> tuple[Path, Path]:
    """Returns llvm-profdata and llvm-cov: those that LLVM_PROFDATA and
    LLVM_COV name, as cargo-llvm-cov takes them, or else those of the
    llvm-tools component of the Rust release that builds the crate."""
    profdata = environment.get("LLVM_PROFDATA")
    llvm_cov = environment.get("LLVM_COV")
    if profdata and llvm_cov:
        return Path(profdata), Path(llvm_cov)
    result = run(["rustc", "--print", "target-libdir"], False)
    _require(result, "finding the libraries of the Rust release")
    tools = Path(result.stdout.strip()).parent / "bin"
    found = (tools / "llvm-profdata", tools / "llvm-cov")
    for tool in found:
        if not tool.is_file():
            raise MeasurementError(
                f"{tool} is missing: add the llvm-tools component to the Rust "
                "release that rust-toolchain.toml pins, as the setup scripts do"
            )
    return found


def read_show_env(text: str) -> dict[str, str]:
    """Reads the variables that `cargo llvm-cov show-env` prints, a line
    `NAME=VALUE` each, whose values it quotes as a POSIX shell reads them."""
    variables: dict[str, str] = {}
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            words = shlex.split(line)
        except ValueError as error:
            raise MeasurementError(
                f"cargo llvm-cov show-env printed {line!r}: {error}"
            ) from error
        name, separator, value = words[0].partition("=") if words else ("", "", "")
        if len(words) != 1 or not separator or not name:
            raise MeasurementError(f"cargo llvm-cov show-env printed {line!r}")
        variables[name] = value
    return variables


def instrumentation(run: coverage.Runner) -> dict[str, str]:
    """Returns the variables that instrument the builds of the scenarios, as
    `cargo llvm-cov show-env` sets them."""
    result = run(SHOW_ENV, False)
    _require(result, "cargo llvm-cov show-env")
    variables = read_show_env(result.stdout)
    flags = {name: variables[name] for name in FLAG_VARIABLES if name in variables}
    if not flags:
        raise MeasurementError(
            "cargo llvm-cov show-env set neither CARGO_ENCODED_RUSTFLAGS nor RUSTFLAGS"
        )
    return flags


def scenario_environment(
    base: Mapping[str, str],
    flags: Mapping[str, str],
    target: Path,
    profiles: Path,
    wrapper: Path,
) -> dict[str, str]:
    """Returns the environment of a run of the scenarios, or of the build of
    their nvx-rs: `base`, whose instrumentation flags `flags` replace, with
    the target directory `target`, the profiles in `profiles`, and the nvx-rs
    `wrapper`."""
    environment = {
        name: value for name, value in base.items() if name not in FLAG_VARIABLES
    }
    environment.update(flags)
    environment["CARGO_TARGET_DIR"] = str(target)
    environment["LLVM_PROFILE_FILE"] = str(profiles / PROFILE_PATTERN)
    environment["NVX_RS"] = str(wrapper)
    return environment


def wrapper_script(binary: Path, starts: Path, profiles: Path) -> str:
    """Returns the nvx-rs that NVX_RS names for the scenarios: a Bash script
    that appends a record of its start to the file of `starts` named by its
    process ID, and then becomes `binary`, with the same ID, which writes its
    profile to `profiles` even if its caller left LLVM_PROFILE_FILE out of
    its environment.

    A record is the start's time, the number of arguments, and the
    arguments, each ended by a NUL, so that every argument fits, and the
    records of processes that reuse an ID follow one another in its file.
    The profile's name begins with the process's ID and the digits of that
    time, its stamp, so that it names the start that wrote it even when
    processes reuse an ID. $EPOCHREALTIME, the time, needs Bash 5 or later.
    """
    starts_prefix = shlex.quote(f"{starts}/")
    profiles_prefix = shlex.quote(f"{profiles}/")
    return (
        "#!/bin/bash\n"
        "# Written by scripts/nvx_rs_scenario_coverage.py: records the start of\n"
        "# an nvx-rs process of the scenarios, then runs the instrumented nvx-rs.\n"
        "started=$EPOCHREALTIME\n"
        'printf \'%s\\0\' "$started" "$#" "$@" '
        f'>>{starts_prefix}"$$" || exit 125\n'
        f"export LLVM_PROFILE_FILE={profiles_prefix}"
        '"%p-${started//[!0-9]/}-%m.profraw"\n'
        f'exec {shlex.quote(str(binary))} "$@"\n'
    )


@dataclass(frozen=True)
class Start:
    """The start of an nvx-rs process of the scenarios, as the wrapper
    recorded it: the process's ID, when it started, in seconds since the
    epoch, the digits of that time as the wrapper wrote it, which name its
    profile, and its arguments."""

    pid: int
    time: float
    stamp: str
    arguments: tuple[str, ...]


def read_starts(directory: Path) -> list[Start]:
    """Returns the starts that the wrapper recorded in `directory`, in the
    order of their times."""
    starts: list[Start] = []
    for path in sorted(directory.iterdir()):
        if not path.name.isdigit():
            raise MeasurementError(f"{path} names no process")
        fields = path.read_bytes().split(b"\0")
        # A NUL ends each field, so the last part of the split is empty.
        if fields.pop() != b"":
            raise MeasurementError(f"{path} ends within a record")
        index = 0
        while index < len(fields):
            try:
                text = fields[index].decode("ascii")
                # Bash writes $EPOCHREALTIME with the locale's decimal point.
                started = float(text.replace(",", "."))
                count = int(fields[index + 1].decode("ascii"))
            except (IndexError, UnicodeDecodeError, ValueError) as error:
                raise MeasurementError(
                    f"{path} holds a malformed record: {error}"
                ) from error
            arguments = fields[index + 2 : index + 2 + count]
            if count < 0 or len(arguments) != count:
                raise MeasurementError(f"{path} ends within a record")
            starts.append(
                Start(
                    int(path.name),
                    started,
                    "".join(character for character in text if character.isdigit()),
                    tuple(os.fsdecode(argument) for argument in arguments),
                )
            )
            index += 2 + count
    return sorted(starts, key=lambda start: (start.time, start.pid))


def profiled_processes(directory: Path) -> set[tuple[int, str]]:
    """Returns the starts whose processes wrote profiles to `directory`, by
    process ID and stamp, with which the wrapper begins their names."""
    found: set[tuple[int, str]] = set()
    for path in directory.glob("*.profraw"):
        parts = path.name.split("-", 2)
        if len(parts) == 3 and parts[0].isdigit() and parts[1].isdigit():
            found.add((int(parts[0]), parts[1]))
    return found


def still_runs(pid: int, binary: Path) -> bool:
    """Returns whether the process `pid` still runs `binary`."""
    try:
        return Path(os.readlink(f"/proc/{pid}/exe")) == binary.resolve()
    except OSError:
        return False


def marked_processes(marker: str) -> list[int]:
    """Returns the IDs of this user's other processes whose environment gives
    RUN_VARIABLE the value `marker`."""
    entry = f"{RUN_VARIABLE}={marker}".encode()
    found: list[int] = []
    for path in Path("/proc").iterdir():
        if not path.name.isdigit() or int(path.name) == os.getpid():
            continue
        try:
            environment = (path / "environ").read_bytes()
        except OSError:
            continue
        if entry in environment.split(b"\0"):
            found.append(int(path.name))
    return found


def end_marked(marker: str) -> None:
    """Kills each process of this user whose environment gives RUN_VARIABLE
    the value `marker`, and those that they start meanwhile, as the runner's
    orphan cleanup kills those of a job by RUNNER_TRACKING_ID."""
    if sys.platform == "win32":
        return
    deadline = time.monotonic() + END_SECONDS
    while found := marked_processes(marker):
        if time.monotonic() > deadline:
            raise MeasurementError(
                f"processes of the runs outlived {END_SECONDS:g} s of kills: "
                + ", ".join(str(pid) for pid in found)
            )
        for pid in found:
            try:
                os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                continue
        time.sleep(0.05)


@dataclass(frozen=True)
class Span:
    """A span of a run of test-microvm, from its scenario duration report: a
    scenario's, or the setup or the teardown, and when it started and ended,
    in seconds since the epoch."""

    name: str
    start: float
    end: float


def _epoch_seconds(text: str) -> float:
    # Python 3.10 reads no `Z` for UTC.
    moment = datetime.fromisoformat(text.replace("Z", "+00:00"))
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=timezone.utc)
    return moment.timestamp()


def read_spans(report: Path) -> list[Span]:
    """Returns the spans of the scenario duration report `report` in the order
    of their starts, or none if test-microvm wrote none, as when it failed
    before it started."""
    try:
        text = report.read_text(encoding="utf-8")
    except FileNotFoundError:
        return []
    spans: list[Span] = []
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        try:
            record = cast(dict[str, Any], json.loads(line))
            kind = cast(str, record["record"])
            if kind == "run":
                continue
            started = _epoch_seconds(cast(str, record["started_at"]))
            seconds = float(cast(float, record["seconds"]))
            name = cast(str, record["scenario"]) if kind == "scenario" else kind
        except (KeyError, TypeError, ValueError) as error:
            raise MeasurementError(f"{report}:{number}: {error!r}") from error
        spans.append(Span(name, started, started + seconds))
    return sorted(spans, key=lambda span: span.start)


def span_of(started: float, spans: Sequence[Span]) -> str | None:
    """Returns the name of the span in which a process that started at
    `started` started: the last one that started before it, unless it started
    after that span ended, or None."""
    found = None
    for span in spans:
        if span.start <= started:
            found = span
    if found is None or started > found.end + SPAN_SLACK_SECONDS:
        return None
    return found.name


def command_of(arguments: Sequence[str]) -> str:
    """Returns the command of nvx-rs that `arguments` run, such as `nvx-rs
    run` or `nvx-rs sandbox start`, after its global options."""
    words = list(arguments)
    while words and words[0].startswith("-"):
        option = words.pop(0)
        if option == "--root" and words:
            words.pop(0)
    command = words[:1]
    if (
        command
        and command[0] in COMMAND_GROUPS
        and len(words) > 1
        and not words[1].startswith("-")
    ):
        command.append(words[1])
    return " ".join(["nvx-rs", *command])


@dataclass(frozen=True)
class Process:
    """An nvx-rs process of the scenarios: its start, the span of test-microvm
    in which it started, if any, whether it wrote a profile, and, if it
    didn't, whether it still ran when the measurement looked."""

    start: Start
    span: str | None
    profile: bool
    running: bool

    def to_json(self) -> dict[str, Any]:
        moment = datetime.fromtimestamp(self.start.time, timezone.utc)
        started = moment.isoformat(timespec="milliseconds").replace("+00:00", "Z")
        return {
            "pid": self.start.pid,
            "started_at": started,
            "span": self.span,
            "command": command_of(self.start.arguments),
            "arguments": list(self.start.arguments),
            "profile": self.profile,
            "running": self.running,
        }


def processes(
    starts: Sequence[Start],
    spans: Sequence[Span],
    profiled: Collection[tuple[int, str]],
    runs: Callable[[int], bool],
) -> list[Process]:
    """Returns the nvx-rs processes of `starts`, in the order of their times,
    which wrote a profile if their IDs and stamps are among `profiled`; `runs`
    tells whether the last process with an ID still runs, if it wrote no
    profile."""
    last = {start.pid: start for start in starts}
    found: list[Process] = []
    for start in starts:
        wrote = (start.pid, start.stamp) in profiled
        running = not wrote and last[start.pid] is start and runs(start.pid)
        found.append(Process(start, span_of(start.time, spans), wrote, running))
    return found


def executables(directory: Path) -> list[Path]:
    """Returns the executables that Cargo built in `directory`, a deps
    directory of a target directory's profile, which holds the binaries that
    Cargo copies, but neither build scripts, which Cargo builds elsewhere,
    nor shared libraries, such as procedural macros."""
    if not directory.is_dir():
        return []
    found: list[Path] = []
    for path in sorted(directory.iterdir()):
        mode = path.lstat().st_mode
        if path.suffix == "" and stat.S_ISREG(mode) and mode & 0o111:
            found.append(path)
    return found


def objects(target: Path) -> list[Path]:
    """Returns the executables that Cargo built in the target directory
    `target`, for each of its profiles."""
    found: list[Path] = []
    for deps in sorted(target.glob("*/deps")):
        found += executables(deps)
    return found


def _object_options(paths: Iterable[Path]) -> list[str]:
    options: list[str] = []
    for path in paths:
        options += ["-object", str(path)]
    return options


def merge(
    run: coverage.Runner, llvm_profdata: Path, inputs: Sequence[Path], output: Path
) -> None:
    """Merges the profiles `inputs` into the indexed profile `output`."""
    if not inputs:
        raise MeasurementError(f"no profile to merge into {output.name}")
    listing = output.with_suffix(".inputs")
    listing.write_text("".join(f"{path}\n" for path in inputs), encoding="utf-8")
    command = [str(llvm_profdata), "merge", "-sparse", "-f", str(listing)]
    _require(run([*command, "-o", str(output)], True), f"merging {output.name}")


def export(
    run: coverage.Runner,
    llvm_cov: Path,
    profile: Path,
    paths: Sequence[Path],
    options: Sequence[str],
) -> str:
    """Returns what `llvm-cov export` with `options` prints of the profile
    `profile` of the executables `paths`."""
    command = [str(llvm_cov), "export", *options, "-instr-profile", str(profile)]
    result = run([*command, *_object_options(paths)], False)
    _require(result, f"exporting {profile.name}")
    return result.stdout


def _summary(text: str, what: str) -> Any:
    try:
        return json.loads(text)
    except ValueError as error:
        raise MeasurementError(f"llvm-cov's summary of {what}: {error}") from error


def production_file(path: str) -> bool:
    """Returns whether `path`, relative to the crate, holds production code."""
    return (
        path.startswith("src/")
        and not coverage.is_test_code(path)
        and not coverage.is_test_double(path)
    )


@dataclass(frozen=True)
class Reached:
    """What a run reached of the crate's production code: its lines and
    functions in all, and in each row of the module table."""

    total: Counts
    modules: dict[str, Counts]


def reached(lcov: str, summary: Any) -> Reached:
    """Returns what a run reached, from its LCOV report of the production
    code and llvm-cov's JSON summary of each file of the crate.

    The lines come from the LCOV report's records, which count each line
    once, as reached once any build of the crate ran it. llvm-cov's summary
    counts a line once for each function whose regions hold it, and, of the
    builds of a function, only the one that reaches the most of it, as it
    does regions. The functions come from the summary, which counts a
    function once, as reached once any build of it ran.
    """
    files = {
        path: lines
        for path, lines in coverage.read_lcov(lcov).items()
        if production_file(path)
    }
    parents = coverage.parent_modules(files)
    modules: dict[str, Counts] = {}
    lines_reached = Count(0, 0)
    for path, lines in sorted(files.items()):
        hits = lines.hits.values()
        counted = Count(sum(hit > 0 for hit in hits), len(hits))
        row = modules.setdefault(coverage.module_of(path, parents), {})
        row["lines"] = row.get("lines", Count(0, 0)) + counted
        lines_reached += counted
    analyzed = coverage.analyze(summary)
    for module, counts in analyzed.modules.items():
        modules.setdefault(module, {})["functions"] = counts["functions"]
    total = {"lines": lines_reached, "functions": analyzed.production["functions"]}
    return Reached(total, modules)


@dataclass(frozen=True)
class Group:
    """Instrumented executables that the scenarios ran: what they are, as a
    phrase in Markdown, their paths, and the directory to which their
    processes write their profiles."""

    name: str
    paths: tuple[Path, ...]
    profiles: Path


def group_reaches(
    run: coverage.Runner,
    llvm_profdata: Path,
    llvm_cov: Path,
    group: Group,
    profdata: Path,
) -> bool:
    """Returns whether the processes of `group` wrote profiles that reach the
    crate's production code in its executables, from the profiles of its
    directory alone, which it merges into the directory `profdata`.

    llvm-cov counts what a profile reached of a function for every executable
    that holds the function under the same symbol, so the profiles of other
    executables could reach code of the group's executables, though no
    process of theirs wrote a profile.
    """
    inputs = sorted(group.profiles.glob("*.profraw"))
    if not inputs:
        return False
    merged = profdata / f"{group.profiles.name}.profdata"
    merge(run, llvm_profdata, inputs, merged)
    options = ["-format=text", "-summary-only", "-ignore-filename-regex", PRODUCTION]
    summary = export(run, llvm_cov, merged, group.paths, options)
    production = coverage.analyze(_summary(summary, group.name)).production
    return production["lines"].covered > 0


def _listing(items: Sequence[str]) -> str:
    """Returns `items` as a list in a sentence: `a`, `a and b`, or `a, b, and
    c`."""
    if len(items) < 3:
        return " and ".join(items)
    return ", ".join(items[:-1]) + ", and " + items[-1]


def _plural(count: int, singular: str, plural: str) -> str:
    """Returns `count` with the noun that it takes."""
    return f"{count:,} {singular if count == 1 else plural}"


def lines_of(lcov: str) -> Count:
    """Returns how many lines of production code an LCOV report instruments,
    and how many of them it reached, each counted once."""
    total = Count(0, 0)
    for path, lines in coverage.read_lcov(lcov).items():
        if production_file(path):
            hits = lines.hits.values()
            total += Count(sum(hit > 0 for hit in hits), len(hits))
    return total


def _remove(path: Path) -> None:
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    elif path.exists() or path.is_symlink():
        path.unlink()


def problems(
    measurement: Mapping[str, Any],
    found: Sequence[Process],
    silent: Sequence[str],
) -> list[str]:
    """Returns, in Markdown, what the measurement misses: what the runs that
    failed would have reached, what processes reached but wrote where the
    measurement doesn't merge it, or didn't write at all, and what the
    groups of executables in `silent`, whose processes wrote no profile that
    reaches the crate's production code, reached."""
    issues: list[str] = []
    runs = cast(dict[str, Any], measurement["runs"])
    for name, what in (
        ("microvm", "`nvx.py test-microvm`"),
        ("runtime_adapter", "`nvx.py test-runtime-adapter`"),
    ):
        status = cast(int, runs[name]["status"])
        if status != 0:
            issues.append(f"{what} failed with status {status}.")
    tests = cast(dict[str, Any], measurement["crate_tests"])
    for retried in cast(list[dict[str, Any]], tests["retries"]):
        target = cast(str, retried["target"])
        failed = cast(list[str], retried["failed"])
        if failed:
            issues.append(
                f"The crate's tests {coverage.join_names(failed)} of `{target}` "
                "failed when they ran again."
            )
        elif cast(int, retried["status"]) != 0:
            issues.append(
                f"The crate's tests of `{target}` failed when they ran again."
            )
    strays = [
        *cast(list[str], tests["stray_profiles"]),
        *cast(list[str], measurement["stray_profiles"]),
    ]
    if strays:
        issues.append(
            _plural(len(strays), "process", "processes")
            + " wrote profiles where the measurement doesn't merge them, as "
            "processes that start without `LLVM_PROFILE_FILE` do: "
            f"{coverage.join_names(strays)}."
        )
    if found and not any(process.profile for process in found):
        issues.append(
            "No `nvx-rs` process wrote a profile, though the scenarios started "
            f"{len(found):,}."
        )
    issues += [
        f"No process of {group} wrote a profile that reaches the crate's "
        "production code."
        for group in silent
    ]
    return issues


def render_processes(found: Sequence[Process]) -> list[str]:
    """Returns the lines of the summary's Markdown section on the nvx-rs
    processes of the scenarios, which lists by span and command those that
    wrote no profile."""
    lines = ["### `nvx-rs` processes", ""]
    if not found:
        return [*lines, "The scenarios started no `nvx-rs` process.", ""]
    missing = [process for process in found if not process.profile]
    started = "The scenarios started " + _plural(
        len(found), "`nvx-rs` process", "`nvx-rs` processes"
    )
    if not missing:
        each = (
            ", which wrote its profile"
            if len(found) == 1
            else ", and each wrote its profile"
        )
        return [*lines, f"{started}{each}.", ""]
    lines += [
        f"{started}, of which {len(missing):,} wrote no profile, so what they "
        "reached is missing: a process that a signal ends writes none, and one that "
        "still runs hasn't written it yet.",
        "",
        "| Span of test-microvm | Command | Processes | Still running |",
        "| --- | --- | ---: | ---: |",
    ]
    rows: dict[tuple[str, str], list[Process]] = {}
    for process in missing:
        span = f"`{process.span}`" if process.span else "none"
        command = command_of(process.start.arguments)
        rows.setdefault((span, command), []).append(process)
    for (span, command), members in sorted(rows.items()):
        running = sum(process.running for process in members)
        lines.append(f"| {span} | `{command}` | {len(members):,} | {running:,} |")
    lines.append("")
    return lines


def render(
    measurement: Mapping[str, Any],
    runs: Mapping[str, Reached],
    found: Sequence[Process],
    issues: Sequence[str],
) -> str:
    """Returns the Markdown summary of a measurement: what its runs reached,
    the nvx-rs processes of its scenarios, and `issues`, what it misses."""
    commit = cast(str, measurement["commit"])
    rustc = cast(str, measurement["rustc"])
    tool = cast(str, measurement["cargo_llvm_cov"])
    groups = cast(list[str], measurement["profiled_groups"])
    lines = [
        "## nvx-rs scenario coverage on Linux/KVM",
        "",
        "What the KVM microVM scenarios and the runtime adapter's suite reached of "
        f"the production code of the `nvx` crate at `{commit[:12]}`, alone and "
        f"together with the crate's own tests, built by {rustc} and measured by "
        f"{tool}. Production code leaves out test code, test data, and the test "
        "doubles, as nvx-rs coverage does. A line or a function counts once, as "
        "reached once any build of the crate ran it.",
        "",
        "| Run | " + " | ".join(metric.capitalize() for metric in METRICS) + " |",
        "| --- |" + " ---: |" * len(METRICS),
    ]
    for name in RUNS:
        cells = (coverage.share(runs[name].total[metric]) for metric in METRICS)
        lines.append(f"| {RUN_NAMES[name]} | " + " | ".join(cells) + " |")
    alone = runs["crate-tests"].total["lines"].covered
    gained = runs["together"].total["lines"].covered - alone
    sources = _listing(groups) if groups else "no executable"
    compiled = cast(dict[str, int], measurement["scenario_builds"])
    reachable = Count(compiled["reached"], compiled["lines"])
    lines += [
        "",
        f"The scenarios reach {gained:,} lines that the crate's tests don't. "
        f"Their profiles include processes of {sources}. Of the lines of "
        "production code that the builds of the scenarios compile, they reach "
        f"{coverage.share(reachable)}. These builds leave out the other lines, "
        "among them the guest code, which runs only in guests.",
        "",
    ]
    retries = cast(list[dict[str, Any]], measurement["crate_tests"]["retries"])
    for retried in retries:
        failed = cast(list[str], retried["failed"])
        tests = [
            test for test in cast(list[str], retried["tests"]) if test not in failed
        ]
        if tests:
            lines += [
                f"The crate's tests {coverage.join_names(tests)} of "
                f"`{retried['target']}` passed only when they ran again.",
                "",
            ]
    lines += render_processes(found)
    if issues:
        lines += ["The measurement misses what these reached:", ""]
        lines += [f"- {issue}" for issue in issues]
        lines.append("")
    lines += [
        "### Production lines by module",
        "",
        "A row counts the files of a top-level module, or of one of its submodules "
        "that has submodules of its own, that no other row counts. The last column "
        "counts the lines that the scenarios reach and the crate's tests don't.",
        "",
        "| Module | "
        + " | ".join(RUN_NAMES[name] for name in RUNS)
        + " | Only the scenarios |",
        "| --- |" + " ---: |" * (len(RUNS) + 1),
    ]
    for module in sorted({module for name in RUNS for module in runs[name].modules}):
        counts = [
            runs[name].modules.get(module, {}).get("lines", Count(0, 0))
            for name in RUNS
        ]
        alone_lines, together = counts[0], counts[RUNS.index("together")]
        only = together.covered - alone_lines.covered
        cells = [coverage.share(count) for count in counts]
        lines.append(f"| `{module}` | " + " | ".join(cells) + f" | {only:,} |")
    return "\n".join(lines) + "\n"


def _prepare_output(output: Path) -> Path:
    """Creates the output directory, without what an earlier measurement
    wrote there, and returns its resolved path."""
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    for name in (
        SUMMARY_FILE,
        MEASUREMENT_FILE,
        PROCESSES_FILE,
        MICROVM_DIRECTORY,
        BUILD_LOG,
        MICROVM_LOG,
        ADAPTER_LOG,
        CRATE_TESTS_LOG,
        *RUNS,
    ):
        _remove(output / name)
    return output


def _report(
    run: coverage.Runner,
    llvm_cov: Path,
    name: str,
    profile: Path,
    paths: Sequence[Path],
    output: Path,
) -> Reached:
    """Writes the reports of the run `name` into its directory of `output`,
    from its profile `profile` of the executables `paths`, and returns what it
    reached."""
    directory = output / name
    directory.mkdir()
    production = ["-ignore-filename-regex", PRODUCTION]
    lcov = export(run, llvm_cov, profile, paths, ["-format=lcov", *production])
    (directory / LCOV_FILE).write_text(lcov, encoding="utf-8")
    options = ["-format=text", "-summary-only", "-ignore-filename-regex", WITHOUT_TESTS]
    summary = export(run, llvm_cov, profile, paths, options)
    (directory / COVERAGE_FILE).write_text(summary, encoding="utf-8")
    if name in HTML_RUNS:
        command = [str(llvm_cov), "show", "-format=html", "-show-instantiations=false"]
        command += ["-output-dir", str(directory / HTML_DIRECTORY), *production]
        command += ["-instr-profile", str(profile), *_object_options(paths)]
        _require(run(command, True), f"the HTML report of {name}")
    return reached(lcov, _summary(summary, name))


def measure(
    output: Path,
    *,
    guest_agent: Path,
    scenarios: Sequence[str] = (),
    work: Path = WORK_DIR,
    run: coverage.Runner = run_command,
    run_streamed: Streamer = stream,
    environment: Mapping[str, str] | None = None,
) -> int:
    """Measures which code of the crate the scenarios reach into `output`,
    and returns 0, or 1 if the measurement misses something, as problems()
    tells. It writes the reports either way.

    `scenarios` selects scenarios of test-microvm, which runs its default
    suite without them. `work` holds what the measurement builds and records,
    and `environment`, by default this process's, is that of the runs.
    """
    output = _prepare_output(output)
    base = dict(os.environ if environment is None else environment)
    marker = f"{os.getpid()}-{time.time_ns()}"
    base[RUN_VARIABLE] = marker
    llvm_profdata, llvm_cov = llvm_tools(run, base)
    flags = instrumentation(run)
    _remove(work)
    target = work / "target"
    adapter_target = work / "adapter-target"
    profiles = work / "profiles"
    nvx_rs_profiles = profiles / NVX_RS_PROFILES
    microvm_profiles = profiles / MICROVM_PROFILES
    adapter_profiles = profiles / ADAPTER_PROFILES
    profile_directories = (nvx_rs_profiles, microvm_profiles, adapter_profiles)
    starts = work / "starts"
    profdata = work / "profdata"
    for directory in (*profile_directories, starts, profdata):
        directory.mkdir(parents=True)
    wrapper = work / "bin" / "nvx-rs"
    binary = target / "release" / "nvx-rs"
    earlier_strays = {
        directory: coverage.stray_profiles(directory)
        for directory in (REPO_ROOT, CRATE_DIR, ADAPTER_DIR)
    }
    microvm = scenario_environment(base, flags, target, microvm_profiles, wrapper)
    adapter = scenario_environment(
        base, flags, adapter_target, adapter_profiles, wrapper
    )

    try:
        if run_streamed(BUILD, microvm, output / BUILD_LOG) != 0:
            raise MeasurementError("the build of the instrumented nvx-rs failed")
        if not binary.is_file():
            raise MeasurementError(f"the build left no nvx-rs at {binary}")
        wrapper.parent.mkdir(parents=True)
        wrapper.write_text(
            wrapper_script(binary, starts, nvx_rs_profiles), encoding="utf-8"
        )
        wrapper.chmod(0o755)
        microvm_output = output / MICROVM_DIRECTORY
        command = [sys.executable, str(NVX_PY), "test-microvm", "--backend", BACKEND]
        command += ["--output-dir", str(microvm_output)]
        for scenario in scenarios:
            command += ["--scenario", scenario]
        started = time.monotonic()
        microvm_status = run_streamed(
            command, microvm, output / MICROVM_LOG, forward_interrupts=True
        )
        microvm_seconds = round(time.monotonic() - started, 1)
        command = [sys.executable, str(NVX_PY), "test-runtime-adapter"]
        command += ["--backend", BACKEND, "--guest-agent", str(guest_agent)]
        started = time.monotonic()
        adapter_status = run_streamed(command, adapter, output / ADAPTER_LOG)
        adapter_seconds = round(time.monotonic() - started, 1)
        tests = coverage.run_tests(output / CRATE_TESTS_LOG, run, CRATE_DIR)
    except BaseException:
        # Each run ended its process tree, but not the processes that left it.
        end_marked(marker)
        raise

    found = processes(
        read_starts(starts),
        read_spans(microvm_output / DURATIONS_FILE),
        profiled_processes(nvx_rs_profiles),
        lambda pid: still_runs(pid, binary),
    )
    repaired = sum(
        coverage.make_readable(directory) for directory in profile_directories
    )
    if repaired:
        print(
            f"Let the owner read and write {repaired:,} profiles",
            file=sys.stderr,
            flush=True,
        )
    groups = [
        Group(
            "`nvx-rs`",
            tuple(executables(target / "release" / "deps")),
            nvx_rs_profiles,
        ),
        Group(
            "the crate's tests that the scenarios run",
            tuple(executables(target / "debug" / "deps")),
            microvm_profiles,
        ),
        Group(
            "the runtime adapter's tests and host program",
            tuple(objects(adapter_target)),
            adapter_profiles,
        ),
    ]
    groups = [group for group in groups if group.paths]
    scenario_objects = [path for group in groups for path in group.paths]
    every_object = [*scenario_objects, *objects(tests.profiles)]
    scenarios_profile = profdata / "scenarios.profdata"
    tests_profile = profdata / "crate-tests.profdata"
    merge(run, llvm_profdata, sorted(profiles.glob("*/*.profraw")), scenarios_profile)
    merge(run, llvm_profdata, sorted(tests.profiles.glob("*.profraw")), tests_profile)
    merge(
        run,
        llvm_profdata,
        [scenarios_profile, tests_profile],
        profdata / "together.profdata",
    )
    runs = {
        name: _report(
            run, llvm_cov, name, profdata / f"{name}.profdata", every_object, output
        )
        for name in RUNS
    }
    profiled_groups: list[str] = []
    silent: list[str] = []
    for group in groups:
        reaches = group_reaches(run, llvm_profdata, llvm_cov, group, profdata)
        (profiled_groups if reaches else silent).append(group.name)
    # The lines that the builds of the scenarios compile, which leave out code
    # that only other builds compile, such as the guest code.
    options = ["-format=lcov", "-ignore-filename-regex", PRODUCTION]
    builds = lines_of(
        export(run, llvm_cov, scenarios_profile, scenario_objects, options)
    )
    # The crate's tests report the profiles that they left in the crate's
    # directory themselves.
    strays = sorted(
        _display(directory / name)
        for directory, earlier in earlier_strays.items()
        for name in coverage.stray_profiles(directory) - earlier
        if directory != CRATE_DIR or name not in tests.strays
    )
    commit = run(["git", "-C", str(REPO_ROOT), "rev-parse", "HEAD"], False)
    measurement: dict[str, Any] = {
        "format": MEASUREMENT_FORMAT,
        "host": platform.system(),
        "backend": BACKEND,
        "commit": commit.stdout.strip() if commit.returncode == 0 else "unknown",
        "rustc": coverage.version(run, ["rustc", "--version"]),
        "cargo_llvm_cov": coverage.version(run, ["cargo", "llvm-cov", "--version"]),
        "instrumentation": flags,
        "scenarios": list(scenarios),
        "runs": {
            "microvm": {"status": microvm_status, "seconds": microvm_seconds},
            "runtime_adapter": {"status": adapter_status, "seconds": adapter_seconds},
        },
        "crate_tests": {
            "status": tests.status,
            "seconds": tests.seconds,
            "retries": [retried.to_json() for retried in tests.retries],
            "stray_profiles": list(tests.strays),
        },
        "stray_profiles": strays,
        "processes": {
            "started": len(found),
            "profiled": sum(process.profile for process in found),
        },
        "profiled_groups": profiled_groups,
        "scenario_builds": {"lines": builds.count, "reached": builds.covered},
        "objects": [_display(path) for path in every_object],
    }
    (output / MEASUREMENT_FILE).write_text(
        json.dumps(measurement, indent=2) + "\n", encoding="utf-8"
    )
    (output / PROCESSES_FILE).write_text(
        json.dumps([process.to_json() for process in found], indent=2) + "\n",
        encoding="utf-8",
    )
    issues = problems(measurement, found, silent)
    (output / SUMMARY_FILE).write_text(
        render(measurement, runs, found, issues), encoding="utf-8"
    )
    for issue in issues:
        print(f"error: {issue}", file=sys.stderr)
    return 1 if issues else 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Measure which host code of the nvx crate the KVM microVM "
        "scenarios and the runtime adapter's suite reach, alone and together "
        "with the crate's own tests."
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=DEFAULT_OUTPUT,
        help="directory of the reports (default: build/nvx-rs-scenario-coverage)",
    )
    parser.add_argument(
        "--guest-agent",
        type=Path,
        default=DEFAULT_GUEST_AGENT,
        help="the runtime adapter's guest agent, a static musl executable that "
        "the measurement doesn't instrument (default: its release build in "
        "tests/runtime-adapter)",
    )
    parser.add_argument(
        "--scenario",
        action="append",
        default=[],
        metavar="NAME",
        help="run only this scenario of test-microvm; repeat to select more "
        "(default: its default suite)",
    )
    arguments = parser.parse_args(argv)
    if sys.platform != "linux":
        print(
            "error: the measurement runs the KVM microVM scenarios, on Linux only",
            file=sys.stderr,
        )
        return 1
    guest_agent = cast(Path, arguments.guest_agent).resolve()
    if not guest_agent.is_file():
        print(
            f"error: the runtime adapter's guest agent is missing: {guest_agent}; "
            "build it in tests/runtime-adapter with `cargo build --locked --release "
            "--features guest --target x86_64-unknown-linux-musl --bin "
            "runtime-adapter-guest`",
            file=sys.stderr,
        )
        return 1
    # A cancelled CI step gets SIGINT, and SIGTERM 7.5 s later, which then
    # interrupts the measurement as SIGINT does, so that its runs end too.
    if signal.getsignal(signal.SIGTERM) == signal.SIG_DFL:
        signal.signal(signal.SIGTERM, signal.default_int_handler)
    try:
        status = measure(
            cast(Path, arguments.output),
            guest_agent=guest_agent,
            scenarios=cast(list[str], arguments.scenario),
        )
    except KeyboardInterrupt:
        print("Interrupted", file=sys.stderr)
        return 130
    except (MeasurementError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"Wrote the reports to {arguments.output}", file=sys.stderr)
    return status


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Tests for the nvx-rs scenario coverage workflow and
scripts/nvx_rs_scenario_coverage.py.

They check the workflow's triggers, its runner, the guest artifacts that it
hands to the runner, and the tools that it requires, and the script's parts:
how it reads cargo-llvm-cov's environment and makes those of its runs, the
nvx-rs wrapper and its records, how it attributes processes to the spans of
test-microvm, which executables it measures, how it counts what each run
reached, its summary, and, with stubs of the host's commands, the whole
measurement.
"""

from __future__ import annotations

import io
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from collections.abc import Collection, Mapping, Sequence
from contextlib import redirect_stderr, redirect_stdout
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, cast
from unittest.mock import patch

import yaml

sys.path.insert(0, str(Path(__file__).parent))
import nvx_rs_coverage as coverage  # noqa: E402
import nvx_rs_scenario_coverage as scenario  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = "nvx-rs-scenario-coverage.yml"
MANIFEST = REPO_ROOT / "scripts" / "setup" / "tool-versions.conf"


def _workflow(name: str = WORKFLOW) -> dict[Any, Any]:
    path = REPO_ROOT / ".github" / "workflows" / name
    return cast(dict[Any, Any], yaml.safe_load(path.read_text(encoding="utf-8")))


def _steps(workflow: Mapping[Any, Any], job: str) -> list[dict[str, Any]]:
    return cast(list[dict[str, Any]], workflow["jobs"][job]["steps"])


def _step(workflow: Mapping[Any, Any], job: str, name: str) -> dict[str, Any]:
    return next(step for step in _steps(workflow, job) if step.get("name") == name)


def _transfers(workflow: Mapping[Any, Any], job: str, action: str) -> dict[str, Any]:
    """Returns the paths of the artifacts that the steps of `job` upload or
    download with `action`, by name."""
    return {
        step["with"]["name"]: step["with"]["path"]
        for step in _steps(workflow, job)
        if str(step.get("uses", "")).startswith(f"actions/{action}@")
    }


def _manifest_value(key: str) -> str:
    for line in MANIFEST.read_text(encoding="utf-8").splitlines():
        name, separator, value = line.partition("=")
        if separator and name == key:
            return value
    raise AssertionError(f"the tool manifest pins no {key}")


def _completed(
    command: Sequence[str], output: str = "", status: int = 0
) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(list(command), status, output)


def _bash5() -> bool:
    """Returns whether /bin/bash, which runs the wrapper, has $EPOCHREALTIME."""
    if os.name != "posix" or not Path("/bin/bash").is_file():
        return False
    result = subprocess.run(
        ["/bin/bash", "-c", 'echo "${BASH_VERSINFO[0]}"'],
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode == 0 and int(result.stdout.strip() or 0) >= 5


class WorkflowTests(unittest.TestCase):
    def test_the_workflow_runs_every_week_and_on_demand_only(self):
        workflow = _workflow()
        # YAML 1.1 reads the key `on` as true.
        triggers = cast(dict[str, Any], workflow.get("on", workflow.get(True)))
        # Never for pull requests, pushes, or merge groups, so that no check
        # that they need waits for it.
        self.assertEqual(set(triggers), {"schedule", "workflow_dispatch"})
        self.assertEqual(triggers["schedule"], [{"cron": "41 7 * * 0"}])
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        self.assertEqual(
            workflow["concurrency"],
            {
                "group": "${{ github.workflow }}-${{ github.ref }}",
                "cancel-in-progress": False,
            },
        )
        # dev requires only ci.yml's check, which GitHub finds by its name.
        ci = _workflow("ci.yml")
        self.assertEqual(
            ci["jobs"]["required-status-check"]["name"], "Required status check"
        )
        for name, job in workflow["jobs"].items():
            with self.subTest(job=name):
                self.assertNotIn("Required status check", job["name"])
        # nvx-rs never replaces a missing guest artifact with a release's.
        self.assertEqual(workflow["env"], {"NVX_AUTO_DOWNLOAD": "0"})

    def test_the_scenarios_run_on_ci_s_kvm_runners_with_its_openvmm(self):
        workflow = _workflow()
        ci = _workflow("ci.yml")
        job = workflow["jobs"]["measure"]
        kvm = ci["jobs"]["nvx-microvm-tests-kvm"]["with"]
        self.assertEqual(job["runs-on"], json.loads(kvm["runner"]))
        self.assertEqual(job["needs"], "artifacts")
        # Only microsoft/nvx has the self-hosted runners.
        self.assertEqual(
            workflow["jobs"]["artifacts"]["if"], "github.repository == 'microsoft/nvx'"
        )
        build = _step(workflow, "measure", "Build OpenVMM")
        openvmm = ci["jobs"]["build-openvmm-linux-gnu"]["with"]
        self.assertEqual(build["uses"], "./.github/actions/build-openvmm")
        self.assertEqual(
            (build["with"]["backend"], build["with"]["binary-path"]),
            (openvmm["backend"], openvmm["binary"]),
        )
        validate = _step(workflow, "measure", "Validate runner")
        self.assertEqual(validate["uses"], "./.github/actions/validate-runner")
        self.assertEqual(validate["with"], {"backend": "kvm", "verify-openvmm": "true"})

    def test_the_runner_gets_the_guest_artifacts_that_the_scenarios_read(self):
        workflow = _workflow()
        ci = _workflow("ci.yml")
        build = _step(workflow, "artifacts", "Build guest artifacts")
        self.assertEqual(build["uses"], "./.github/actions/build-guest-artifacts")
        # The kernel and the Alpine initramfs, which every scenario boots, and
        # the Ubuntu distro layer and the sandbox scratch template, which the
        # public sandbox scenarios read.
        self.assertEqual(
            build["with"], {"components": "kernel alpine ubuntu", "save-cache": "false"}
        )
        uploads = _transfers(workflow, "artifacts", "upload-artifact")
        self.assertEqual(
            set(uploads), {"guest-kernel", "guest-alpine", "guest-ubuntu-distro"}
        )
        ci_uploads = {
            name: path
            for job in ("artifacts", "distro-artifacts")
            for name, path in _transfers(ci, job, "upload-artifact").items()
        }
        for name, path in uploads.items():
            with self.subTest(artifact=name):
                self.assertEqual(path, ci_uploads[name])
        self.assertEqual(
            _transfers(workflow, "measure", "download-artifact"),
            dict.fromkeys(uploads, "build"),
        )

    def test_the_measurement_execs_the_script_with_its_output_directory(self):
        workflow = _workflow()
        # The step execs the measurement, which a cancellation then signals.
        words = shlex.split(_step(workflow, "measure", "Measure")["run"])
        self.assertEqual(
            words[:3], ["exec", "python3", "scripts/nvx_rs_scenario_coverage.py"]
        )
        output = words[words.index("--output") + 1]
        self.assertEqual(REPO_ROOT / output, scenario.DEFAULT_OUTPUT)

    def test_reruns_replace_the_reports(self):
        workflow = _workflow()
        upload = _step(workflow, "measure", "Upload the reports")["with"]
        # The name holds no attempt, so that a rerun of the job replaces the
        # reports of its earlier attempt.
        self.assertEqual(
            upload["name"], "nvx-rs-scenario-coverage-${{ github.run_id }}"
        )
        self.assertEqual(upload["path"], "build/nvx-rs-scenario-coverage")
        self.assertIs(upload["overwrite"], True)
        # The reports of a measurement that misses something upload too.
        for name in ("Write the summary", "Upload the reports"):
            with self.subTest(step=name):
                self.assertIn("!cancelled()", _step(workflow, "measure", name)["if"])

    def test_the_runner_must_have_the_pinned_coverage_tools(self):
        check = _step(_workflow(), "measure", "Check the coverage tools")["run"]
        bash = shutil.which("bash")
        if os.name != "posix" or bash is None:
            self.skipTest("runs the workflow's bash step on POSIX hosts")
        pinned = _manifest_value("cargo_llvm_cov.version")
        llvm_tools = "llvm-tools-x86_64-unknown-linux-gnu"
        cases = {
            "pinned": (f"cargo-llvm-cov {pinned}", llvm_tools, 0),
            # The whole line, so that 0.9.10 doesn't pass for 0.9.1.
            "another release": (f"cargo-llvm-cov {pinned}0", llvm_tools, 1),
            "no llvm-tools": (f"cargo-llvm-cov {pinned}", "rustc-x86_64", 1),
        }
        for case, (version, component, status) in cases.items():
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                programs = Path(directory)
                for name, output in (("cargo", version), ("rustup", component)):
                    program = programs / name
                    program.write_text(
                        f"#!/bin/sh\nprintf '%s\\n' {shlex.quote(output)}\n",
                        encoding="utf-8",
                    )
                    program.chmod(0o755)
                result = subprocess.run(
                    [bash, "--noprofile", "--norc", "-c", check],
                    cwd=REPO_ROOT,
                    env={
                        **os.environ,
                        "PATH": f"{programs}{os.pathsep}{os.environ['PATH']}",
                    },
                    capture_output=True,
                    text=True,
                    timeout=60,
                    check=False,
                )
                self.assertEqual(result.returncode, status, result.stderr)
                if status:
                    self.assertIn(
                        "rerun scripts/setup/setup-linux.sh --runner-only",
                        result.stderr,
                    )


class ShowEnvTests(unittest.TestCase):
    def test_variables_read_as_a_shell_reads_them(self):
        text = (
            "LLVM_PROFILE_FILE='/a b/nvx-rs-%p-%8m.profraw'\n"
            "RUSTFLAGS='-C instrument-coverage --remap-path-prefix /a b/='\n"
            "CARGO_LLVM_COV=1\n"
            "\n"
            "QUOTED='it'\\''s'\n"
        )
        self.assertEqual(
            scenario.read_show_env(text),
            {
                "LLVM_PROFILE_FILE": "/a b/nvx-rs-%p-%8m.profraw",
                "RUSTFLAGS": "-C instrument-coverage --remap-path-prefix /a b/=",
                "CARGO_LLVM_COV": "1",
                "QUOTED": "it's",
            },
        )

    def test_malformed_lines_fail(self):
        for line in ("RUSTFLAGS", "=1", "A=1 B=2", "A='open"):
            with self.subTest(line=line), self.assertRaises(coverage.MeasurementError):
                scenario.read_show_env(line)

    def test_the_builds_take_the_flags_that_show_env_sets(self):
        commands: list[tuple[str, ...]] = []

        def run(command: Sequence[str], echo: bool) -> subprocess.CompletedProcess[str]:
            commands.append(tuple(command))
            output = "LLVM_PROFILE_FILE=/p\nRUSTFLAGS='-C instrument-coverage'\n"
            return _completed(command, output)

        self.assertEqual(
            scenario.instrumentation(run), {"RUSTFLAGS": "-C instrument-coverage"}
        )
        self.assertEqual(commands, [scenario.SHOW_ENV])
        # The scenarios start Cargo themselves, so the flags must instrument
        # every build that they start; the crate's paths keep their prefix
        # without the remap.
        self.assertIn("--no-rustc-wrapper", scenario.SHOW_ENV)
        self.assertIn("--remap-path-prefix", scenario.SHOW_ENV)

    def test_an_environment_without_flags_fails(self):
        def run(command: Sequence[str], echo: bool) -> subprocess.CompletedProcess[str]:
            return _completed(command, "CARGO_LLVM_COV=1\n")

        with self.assertRaises(coverage.MeasurementError):
            scenario.instrumentation(run)

    def test_runs_build_into_their_own_target_with_the_flags(self):
        base = {
            "PATH": "/bin",
            "RUSTFLAGS": "-C opt-level=1",
            "CARGO_ENCODED_RUSTFLAGS": "-Cdebuginfo=0",
            "CARGO_TARGET_DIR": "/elsewhere",
        }
        target, profiles, wrapper = Path("/w/t"), Path("/w/p"), Path("/w/bin/nvx-rs")
        environment = scenario.scenario_environment(
            base, {"RUSTFLAGS": "-C instrument-coverage"}, target, profiles, wrapper
        )
        # Cargo would take CARGO_ENCODED_RUSTFLAGS before RUSTFLAGS.
        self.assertEqual(
            environment,
            {
                "PATH": "/bin",
                "RUSTFLAGS": "-C instrument-coverage",
                "CARGO_TARGET_DIR": str(target),
                "LLVM_PROFILE_FILE": str(profiles / "%p-%m.profraw"),
                "NVX_RS": str(wrapper),
            },
        )


class ToolTests(unittest.TestCase):
    def test_tools_that_the_environment_names_come_first(self):
        def run(command: Sequence[str], echo: bool) -> subprocess.CompletedProcess[str]:
            raise AssertionError(f"ran {command}")

        environment = {"LLVM_PROFDATA": "/t/profdata", "LLVM_COV": "/t/cov"}
        self.assertEqual(
            scenario.llvm_tools(run, environment),
            (Path("/t/profdata"), Path("/t/cov")),
        )

    def test_the_rust_release_s_llvm_tools_come_next(self):
        with tempfile.TemporaryDirectory() as directory:
            libraries = Path(directory) / "lib" / "rustlib" / "host" / "lib"
            tools = libraries.parent / "bin"
            libraries.mkdir(parents=True)

            def run(
                command: Sequence[str], echo: bool
            ) -> subprocess.CompletedProcess[str]:
                self.assertEqual(list(command), ["rustc", "--print", "target-libdir"])
                return _completed(command, f"{libraries}\n")

            # Without them, the measurement asks for the component.
            with self.assertRaisesRegex(coverage.MeasurementError, "llvm-tools"):
                scenario.llvm_tools(run, {"LLVM_COV": "/t/cov"})
            tools.mkdir()
            for name in ("llvm-profdata", "llvm-cov"):
                (tools / name).write_bytes(b"")
            self.assertEqual(
                scenario.llvm_tools(run, {}),
                (tools / "llvm-profdata", tools / "llvm-cov"),
            )


class WrapperTests(unittest.TestCase):
    def test_the_wrapper_records_its_start_and_becomes_the_instrumented_nvx_rs(self):
        binary, starts, profiles = Path("/w/nvx-rs"), Path("/w/s"), Path("/w/p")
        script = scenario.wrapper_script(binary, starts, profiles)
        self.assertTrue(script.startswith("#!/bin/bash\n"))
        self.assertIn(
            "started=$EPOCHREALTIME\n"
            'printf \'%s\\0\' "$started" "$#" "$@" '
            f'>>{shlex.quote(f"{starts}/")}"$$" || exit 125\n',
            script,
        )
        self.assertIn(
            f"export LLVM_PROFILE_FILE={shlex.quote(f'{profiles}/')}"
            '"%p-${started//[!0-9]/}-%m.profraw"\n',
            script,
        )
        self.assertTrue(script.endswith(f'exec {shlex.quote(str(binary))} "$@"\n'))

    @unittest.skipUnless(_bash5(), "runs the wrapper with Bash 5 or later")
    def test_a_wrapped_process_keeps_its_id_its_arguments_and_its_status(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            starts, profiles = root / "starts", root / "profiles"
            starts.mkdir()
            profiles.mkdir()
            binary = root / "nvx-rs"
            binary.write_text(
                '#!/bin/sh\nprintf "%s\\n%s\\n" "$$" "$LLVM_PROFILE_FILE" >"$OUT"\n'
                "exit 7\n",
                encoding="utf-8",
            )
            binary.chmod(0o755)
            wrapper = root / "wrapper"
            wrapper.write_text(
                scenario.wrapper_script(binary, starts, profiles), encoding="utf-8"
            )
            wrapper.chmod(0o755)
            arguments = ["--root", "/r", "sandbox", "start", "a b", "", "line\nbreak"]
            # The caller left LLVM_PROFILE_FILE out of the environment.
            environment = {
                **{
                    name: value
                    for name, value in os.environ.items()
                    if name != "LLVM_PROFILE_FILE"
                },
                "OUT": str(root / "out"),
            }
            before = time.time()
            result = subprocess.run(
                [str(wrapper), *arguments], env=environment, timeout=60, check=False
            )
            pid, pattern = (root / "out").read_text(encoding="utf-8").splitlines()
            starts_read = scenario.read_starts(starts)
        self.assertEqual(result.returncode, 7)
        self.assertEqual(len(starts_read), 1)
        start = starts_read[0]
        self.assertEqual(start.pid, int(pid))
        self.assertEqual(start.arguments, tuple(arguments))
        self.assertGreaterEqual(start.time, before - 1)
        # The profile's name holds the digits of the start's time, which
        # $EPOCHREALTIME writes with six of them after the point.
        self.assertEqual(pattern, f"{profiles}/%p-{start.stamp}-%m.profraw")
        self.assertTrue(start.stamp.isdigit())
        self.assertEqual(int(start.stamp) // 1_000_000, int(start.time))


class StartTests(unittest.TestCase):
    def test_records_hold_each_start_of_each_process(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            # Two processes with the same ID, and a locale whose decimal point
            # is a comma.
            (root / "41").write_bytes(
                b"1700000002.5\x002\x00run\x00\x00" + b"1700000001.25\x000\x00"
            )
            (root / "7").write_bytes(b"1700000003,75\x001\x00a\nb\x00")
            starts = scenario.read_starts(root)
        self.assertEqual(
            starts,
            [
                scenario.Start(41, 1700000001.25, "170000000125", ()),
                scenario.Start(41, 1700000002.5, "17000000025", ("run", "")),
                scenario.Start(7, 1700000003.75, "170000000375", ("a\nb",)),
            ],
        )

    def test_malformed_records_fail(self):
        cases = {
            "x": b"1\x000\x00",
            "1": b"1\x000",
            "2": b"time\x000\x00",
            "3": b"1\x002\x00a\x00",
            "4": b"1\x00-1\x00",
        }
        for name, data in cases.items():
            with self.subTest(file=name), tempfile.TemporaryDirectory() as directory:
                (Path(directory) / name).write_bytes(data)
                with self.assertRaises(coverage.MeasurementError):
                    scenario.read_starts(Path(directory))

    def test_profiles_name_their_starts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in (
                "41-1700000001250000-123_0.profraw",
                "7-17-9_0.profraw",
                # Processes that the wrapper didn't start name theirs by ID
                # alone, and processes without LLVM_PROFILE_FILE by default.
                "8-9_0.profraw",
                "default_1_2.profraw",
                "x-1-2.profraw",
                "9-1-2.txt",
            ):
                (root / name).write_bytes(b"")
            self.assertEqual(
                scenario.profiled_processes(root),
                {(41, "1700000001250000"), (7, "17")},
            )


def _timestamp(seconds: float) -> str:
    """Returns `seconds` since the epoch as the duration report writes it."""
    moment = datetime.fromtimestamp(seconds, timezone.utc)
    return moment.isoformat(timespec="milliseconds").replace("+00:00", "Z")


def _durations(start: float, spans: Sequence[tuple[str, float]]) -> str:
    """Returns a scenario duration report of a run that started at `start`,
    with `spans`, each a scenario or `setup` or `teardown` and its seconds."""
    records: list[dict[str, Any]] = [
        {"record": "run", "started_at": _timestamp(start), "scenarios": []}
    ]
    for name, seconds in spans:
        record: dict[str, Any] = {
            "record": name if name in ("setup", "teardown") else "scenario",
            "started_at": _timestamp(start),
            "seconds": seconds,
            "outcome": "passed",
        }
        if record["record"] == "scenario":
            record["scenario"] = name
        records.append(record)
        start += seconds
    return "".join(json.dumps(record) + "\n" for record in records)


class SpanTests(unittest.TestCase):
    def test_processes_belong_to_the_span_in_which_they_started(self):
        start = 1_800_000_000.0
        report = _durations(
            start,
            (("setup", 1.0), ("lifecycle", 10.0), ("smp", 5.0), ("teardown", 0.5)),
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "scenario-durations.jsonl"
            path.write_text(report, encoding="utf-8")
            spans = scenario.read_spans(path)
            # A run that failed before its report has no spans.
            self.assertEqual(scenario.read_spans(Path(directory) / "none"), [])
        self.assertEqual(
            [(span.name, span.end - span.start) for span in spans],
            [("setup", 1.0), ("lifecycle", 10.0), ("smp", 5.0), ("teardown", 0.5)],
        )
        for offset, expected in (
            (-0.1, None),
            (0.5, "setup"),
            (1.0, "lifecycle"),
            (10.9, "lifecycle"),
            (11.0, "smp"),
            (16.4, "teardown"),
            # The report rounds its times to the millisecond.
            (16.6, "teardown"),
            (18.0, None),
        ):
            with self.subTest(offset=offset):
                self.assertEqual(scenario.span_of(start + offset, spans), expected)

    def test_malformed_reports_fail(self):
        for line in ('{"record": "setup"}', '{"record": "smp", "seconds": 1}', "["):
            with self.subTest(line=line), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "scenario-durations.jsonl"
                path.write_text(line + "\n", encoding="utf-8")
                with self.assertRaises(coverage.MeasurementError):
                    scenario.read_spans(path)


class ProcessTests(unittest.TestCase):
    def test_commands_name_what_nvx_rs_runs(self):
        for arguments, expected in (
            (["--root", "/r", "run", "--hypervisor", "kvm"], "nvx-rs run"),
            (
                ["--root", "/r", "sandbox", "start", "--state-dir", "s"],
                "nvx-rs sandbox start",
            ),
            (
                ["--root=/r", "egress", "compile", "--", "policy"],
                "nvx-rs egress compile",
            ),
            (["--root", "/r", "sandbox", "--help"], "nvx-rs sandbox"),
            (["artifacts"], "nvx-rs artifacts"),
            (["-h"], "nvx-rs"),
        ):
            with self.subTest(arguments=arguments):
                self.assertEqual(scenario.command_of(arguments), expected)

    def test_processes_tell_their_spans_profiles_and_states(self):
        starts = [
            scenario.Start(
                41, 1_800_000_001.5, "1800000001500000", ("--root", "/r", "run")
            ),
            scenario.Start(
                42,
                1_800_000_002.0,
                "1800000002000000",
                ("--root", "/r", "sandbox", "stop"),
            ),
            scenario.Start(43, 1_800_000_100.0, "1800000100000000", ("artifacts",)),
        ]
        spans = [scenario.Span("lifecycle", 1_800_000_001.0, 1_800_000_010.0)]
        found = scenario.processes(
            starts, spans, {(41, "1800000001500000")}, lambda pid: pid == 42
        )
        self.assertEqual(
            [(process.span, process.profile, process.running) for process in found],
            [
                ("lifecycle", True, False),
                ("lifecycle", False, True),
                (None, False, False),
            ],
        )
        self.assertEqual(
            found[1].to_json(),
            {
                "pid": 42,
                "started_at": "2027-01-15T08:00:02.000Z",
                "span": "lifecycle",
                "command": "nvx-rs sandbox stop",
                "arguments": ["--root", "/r", "sandbox", "stop"],
                "profile": False,
                "running": True,
            },
        )

    def test_processes_that_reuse_an_id_tell_their_own_profiles(self):
        # The second process with ID 7 was killed, and the first wrote its
        # profile; the other way around, the first can't still run.
        starts = [
            scenario.Start(7, 1_800_000_001.0, "1800000001000000", ("run",)),
            scenario.Start(7, 1_800_000_002.0, "1800000002000000", ("run",)),
        ]
        for profiled, expected in (
            ("1800000001000000", [(True, False), (False, True)]),
            ("1800000002000000", [(False, False), (True, False)]),
        ):
            with self.subTest(profiled=profiled):
                found = scenario.processes(
                    starts, [], {(7, profiled)}, lambda pid: pid == 7
                )
                self.assertEqual(
                    [(process.profile, process.running) for process in found],
                    expected,
                )

    @unittest.skipIf(os.name == "nt", "Windows has no execute permission bits")
    def test_the_measurement_reads_the_executables_that_cargo_built(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            for name, mode in (
                ("debug/deps/vm_restart-0123", 0o755),
                ("debug/deps/libserde_derive-4567.so", 0o755),
                ("debug/deps/vm_restart-0123.d", 0o644),
                ("debug/deps/data-89ab", 0o644),
                ("debug/build/nvx-cdef/build-script-build", 0o755),
                ("debug/top-level-host", 0o755),
                ("release/deps/nvx_rs-0123", 0o755),
            ):
                path = target / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"")
                path.chmod(mode)
            (target / "debug" / "deps" / "directory-0123").mkdir()
            self.assertEqual(
                scenario.executables(target / "debug" / "deps"),
                [target / "debug/deps/vm_restart-0123"],
            )
            self.assertEqual(
                scenario.objects(target),
                [
                    target / "debug/deps/vm_restart-0123",
                    target / "release/deps/nvx_rs-0123",
                ],
            )
            self.assertEqual(scenario.executables(target / "missing"), [])


def _lcov(files: Mapping[str, Mapping[int, int]]) -> str:
    """Returns an LCOV report of `files`, each with the hits of its lines."""
    records = ""
    for name, hits in files.items():
        lines = "".join(f"DA:{line},{count}\n" for line, count in hits.items())
        records += f"SF:{name}\n{lines}end_of_record\n"
    return records


def _summary(files: Mapping[str, tuple[int, int]]) -> dict[str, Any]:
    """Returns llvm-cov's JSON summary of `files`, each with how many of its
    functions, and as many lines and regions, were reached and counted."""
    return {
        "data": [
            {
                "files": [
                    {
                        "filename": name,
                        "summary": {
                            metric: {"covered": covered, "count": count}
                            for metric in ("lines", "functions", "regions")
                        },
                    }
                    for name, (covered, count) in files.items()
                ]
            }
        ]
    }


class ReachedTests(unittest.TestCase):
    def test_lines_count_once_as_reached_by_any_build(self):
        # Two builds of one file: llvm-cov's summary would count the larger
        # share of them, 1 of 3 lines, rather than both lines that they reach.
        lcov = _lcov(
            {
                "src/host/cli/run.rs": {1: 3, 2: 0, 3: 0},
                "src/lib.rs": {1: 1},
                "src/testing/mod.rs": {1: 1},
            }
        ) + _lcov({"src/host/cli/run.rs": {2: 1, 4: 0}})
        summary = _summary(
            {
                "src/host/cli/run.rs": (1, 3),
                "src/lib.rs": (1, 1),
                "src/testing/mod.rs": (5, 5),
                "tests/cli.rs": (9, 9),
            }
        )
        reached = scenario.reached(lcov, summary)
        count = coverage.Count
        self.assertEqual(
            reached.total, {"lines": count(3, 5), "functions": count(2, 4)}
        )
        self.assertEqual(
            reached.modules,
            {
                "src/host/cli": {"lines": count(2, 4), "functions": count(1, 3)},
                "src/lib": {"lines": count(1, 1), "functions": count(1, 1)},
            },
        )
        self.assertEqual(scenario.lines_of(lcov), count(3, 5))


def _measurement(**changes: Any) -> dict[str, Any]:
    measurement: dict[str, Any] = {
        "format": scenario.MEASUREMENT_FORMAT,
        "host": "Linux",
        "backend": "kvm",
        "commit": "0123456789abcdef0123456789abcdef01234567",
        "rustc": "rustc 1.0.0 (0 1970-01-01)",
        "cargo_llvm_cov": "cargo-llvm-cov 1.0.0",
        "runs": {
            "microvm": {"status": 0, "seconds": 1.0},
        },
        "crate_tests": {
            "status": 0,
            "seconds": 1.0,
            "retries": [],
            "stray_profiles": [],
        },
        "stray_profiles": [],
        "profiled_groups": ["`nvx-rs`", "the crate's tests that the scenarios run"],
        "scenario_builds": {"lines": 40, "reached": 30},
    }
    measurement.update(changes)
    return measurement


def _reached(lines: Mapping[str, tuple[int, int]]) -> scenario.Reached:
    """Returns what a run reached: the lines of each module, and as many
    functions."""
    modules = {
        module: {
            "lines": coverage.Count(*count),
            "functions": coverage.Count(*count),
        }
        for module, count in lines.items()
    }
    total = coverage.Count(0, 0)
    for counts in modules.values():
        total += counts["lines"]
    return scenario.Reached({"lines": total, "functions": total}, modules)


RUNS = {
    "crate-tests": _reached({"src/host/cli": (80, 100), "src/guest": (9, 10)}),
    "scenarios": _reached({"src/host/cli": (30, 100), "src/guest": (0, 10)}),
    "together": _reached({"src/host/cli": (90, 100), "src/guest": (9, 10)}),
}


def _process(
    pid: int, span: str | None, profile: bool, *arguments: str
) -> scenario.Process:
    start = scenario.Start(
        pid, 1_800_000_000.0, "1800000000000000", ("--root", "/r", *arguments)
    )
    return scenario.Process(start, span, profile, running=pid == 44)


class SummaryTests(unittest.TestCase):
    def test_summaries_give_each_run_and_the_lines_that_only_the_scenarios_reach(self):
        found = [_process(41, "lifecycle", True, "run")]
        summary = scenario.render(_measurement(), RUNS, found, [])
        self.assertTrue(
            summary.startswith("## nvx-rs scenario coverage on Linux/KVM\n")
        )
        for text in (
            "`0123456789ab`",
            "rustc 1.0.0 (0 1970-01-01)",
            "cargo-llvm-cov 1.0.0",
        ):
            self.assertIn(text, summary)
        self.assertIn(
            "| Run | Lines | Functions |\n"
            "| --- | ---: | ---: |\n"
            "| The crate's tests | 80.9% of 110 | 80.9% of 110 |\n"
            "| The scenarios | 27.2% of 110 | 27.2% of 110 |\n"
            "| Together | 90.0% of 110 | 90.0% of 110 |\n",
            summary,
        )
        self.assertIn(
            "The scenarios reach 10 lines that the crate's tests don't. Their "
            "profiles include processes of `nvx-rs` and the crate's tests that "
            "the scenarios run. Of the lines of production code that the builds of the "
            "scenarios compile, they reach 75.0% of 40. These builds leave out "
            "the other lines, among them the guest code, which runs only in "
            "guests.",
            summary,
        )
        self.assertIn(
            "The scenarios started 1 `nvx-rs` process, which wrote its profile.",
            summary,
        )
        self.assertIn(
            "| Module | The crate's tests | The scenarios | Together "
            "| Only the scenarios |\n"
            "| --- | ---: | ---: | ---: | ---: |\n"
            "| `src/guest` | 90.0% of 10 | 0.0% of 10 | 90.0% of 10 | 0 |\n"
            "| `src/host/cli` | 80.0% of 100 | 30.0% of 100 | 90.0% of 100 | 10 |\n",
            summary,
        )
        self.assertNotIn("misses", summary)

    def test_summaries_list_the_processes_that_wrote_no_profile(self):
        found = [
            _process(41, "lifecycle", True, "run"),
            _process(42, "run-console-restore", False, "run", "--hypervisor", "kvm"),
            _process(43, "run-console-restore", False, "run"),
            _process(44, "sandbox-lifecycle", False, "sandbox", "start"),
            _process(45, None, False, "artifacts"),
        ]
        summary = scenario.render(_measurement(), RUNS, found, [])
        self.assertIn(
            "The scenarios started 5 `nvx-rs` processes, of which 4 wrote no "
            "profile, so what they reached is missing",
            summary,
        )
        self.assertIn(
            "| Span of test-microvm | Command | Processes | Still running |\n"
            "| --- | --- | ---: | ---: |\n"
            "| `run-console-restore` | `nvx-rs run` | 2 | 0 |\n"
            "| `sandbox-lifecycle` | `nvx-rs sandbox start` | 1 | 1 |\n"
            "| none | `nvx-rs artifacts` | 1 | 0 |\n",
            summary,
        )

    def test_summaries_name_the_crate_s_tests_that_passed_only_when_they_ran_again(
        self,
    ):
        retries = [
            coverage.Retry("--lib", ("a", "b"), ("b",), 101).to_json(),
            coverage.Retry("--test cli", ("c",), (), 0).to_json(),
        ]
        crate_tests = {"status": 101, "seconds": 1.0, "retries": retries}
        measurement = _measurement(crate_tests={**crate_tests, "stray_profiles": []})
        summary = scenario.render(measurement, RUNS, [], [])
        self.assertIn(
            "The crate's tests `a` of `--lib` passed only when they ran again.", summary
        )
        self.assertIn(
            "The crate's tests `c` of `--test cli` passed only when they ran again.",
            summary,
        )
        self.assertIn("The scenarios started no `nvx-rs` process.", summary)


class ProblemTests(unittest.TestCase):
    def test_measurements_without_problems_miss_nothing(self):
        found = [
            _process(41, "lifecycle", True, "run"),
            _process(42, None, False, "run"),
        ]
        self.assertEqual(scenario.problems(_measurement(), found, []), [])

    def test_problems_name_what_the_measurement_misses(self):
        retries = [
            coverage.Retry("--lib", ("a", "b"), ("b",), 101).to_json(),
            coverage.Retry("--test store", (), (), 101).to_json(),
        ]
        measurement = _measurement(
            runs={
                "microvm": {"status": 1, "seconds": 1.0},
            },
            crate_tests={
                "status": 101,
                "seconds": 1.0,
                "retries": retries,
                "stray_profiles": ["default_1_2.profraw"],
            },
            stray_profiles=["build/default_3_4.profraw"],
        )
        found = [_process(41, "lifecycle", False, "run")]
        issues = scenario.problems(
            measurement, found, ["the crate's tests that the scenarios run"]
        )
        self.assertEqual(
            issues,
            [
                "`nvx.py test-microvm` failed with status 1.",
                "The crate's tests `b` of `--lib` failed when they ran again.",
                "The crate's tests of `--test store` failed when they ran again.",
                "2 processes wrote profiles where the measurement doesn't merge "
                "them, as processes that start without `LLVM_PROFILE_FILE` do: "
                "`default_1_2.profraw` and "
                "`build/default_3_4.profraw`.",
                "No `nvx-rs` process wrote a profile, though the scenarios started 1.",
                "No process of the crate's tests that the scenarios run wrote a "
                "profile that reaches the crate's production code.",
            ],
        )
        summary = scenario.render(measurement, RUNS, found, issues)
        self.assertIn(
            "The measurement misses what these reached:\n\n"
            "- `nvx.py test-microvm` failed with status 1.\n",
            summary,
        )


class FakePopen:
    """A run that each of the first `interrupts` reads of its output
    interrupts, as a SIGINT of the measurement would, and that then prints a
    line and ends."""

    def __init__(self, interrupts: int):
        self.interrupts = interrupts
        self.signals: list[int] = []
        self.stdout = self
        self.lines = [b"stopped\n"]

    def __call__(self, *arguments: Any, **options: Any) -> FakePopen:
        return self

    def __iter__(self) -> FakePopen:
        return self

    def __next__(self) -> bytes:
        if self.interrupts:
            self.interrupts -= 1
            raise KeyboardInterrupt
        if self.lines:
            return self.lines.pop(0)
        raise StopIteration

    def send_signal(self, number: int) -> None:
        self.signals.append(number)

    def wait(self) -> int:
        return -2

    def __enter__(self) -> FakePopen:
        return self

    def __exit__(self, *exception: object) -> None:
        return None


class EndedTrees:
    """Stands for terminate_process_tree(), recording the processes whose
    trees it ended instead of signalling any."""

    def __init__(self, test: unittest.TestCase):
        self.test = test
        self.processes: list[object] = []

    def __call__(self, process: object, *, process_group: bool) -> None:
        # The processes that a run started may have left its process group.
        self.test.assertFalse(process_group)
        self.processes.append(process)


def _stdout() -> io.TextIOWrapper[io.BytesIO]:
    """Returns a standard output that keeps the bytes written to its buffer."""
    return io.TextIOWrapper(io.BytesIO(), encoding="utf-8")


class StreamTests(unittest.TestCase):
    def _stream(
        self, command: Sequence[str], log: Path, *, forward_interrupts: bool = False
    ) -> tuple[int, bytes]:
        stdout = _stdout()
        with redirect_stdout(stdout), redirect_stderr(io.StringIO()):
            status = scenario.stream(
                command, os.environ, log, forward_interrupts=forward_interrupts
            )
            stdout.flush()
        return status, stdout.buffer.getvalue()

    def test_runs_print_and_log_what_they_print_and_return_their_status(self):
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "run.log"
            script = "import sys; print('a'); print('b', file=sys.stderr); sys.exit(3)"
            status, printed = self._stream([sys.executable, "-c", script], log)
            logged = log.read_bytes()
        self.assertEqual(status, 3)
        self.assertEqual(printed, logged)
        # The run's output and errors share the pipe, in whatever order their
        # buffers flush.
        self.assertEqual(sorted(logged.split()), [b"a", b"b"])

    def test_interrupts_reach_a_run_that_takes_them_and_end_it_at_the_third(self):
        for interrupts, signals, ended, logged in (
            (1, [signal.SIGINT], False, b"stopped\n"),
            (2, [signal.SIGINT, signal.SIGTERM], False, b"stopped\n"),
            (3, [signal.SIGINT, signal.SIGTERM], True, b""),
        ):
            with (
                self.subTest(interrupts=interrupts),
                tempfile.TemporaryDirectory() as directory,
            ):
                process = FakePopen(interrupts)
                trees = EndedTrees(self)
                log = Path(directory) / "run.log"
                with (
                    patch.object(scenario.subprocess, "Popen", process),
                    patch.object(scenario, "terminate_process_tree", trees),
                    self.assertRaises(KeyboardInterrupt),
                ):
                    self._stream(["run"], log, forward_interrupts=True)
                self.assertEqual(process.signals, signals)
                self.assertEqual(trees.processes, [process] if ended else [])
                # The measurement kept what the run printed as it ended.
                self.assertEqual(log.read_bytes(), logged)

    def test_interrupts_end_other_runs_with_every_process_that_they_started(self):
        with tempfile.TemporaryDirectory() as directory:
            process = FakePopen(1)
            trees = EndedTrees(self)
            with (
                patch.object(scenario.subprocess, "Popen", process),
                patch.object(scenario, "terminate_process_tree", trees),
                self.assertRaises(KeyboardInterrupt),
            ):
                self._stream(["run"], Path(directory) / "run.log")
        self.assertEqual(process.signals, [])
        self.assertEqual(trees.processes, [process])


class RunCommandTests(unittest.TestCase):
    def test_commands_without_echo_run_as_nvx_rs_coverage_runs_them(self):
        calls: list[tuple[tuple[str, ...], bool]] = []

        def run(command: Sequence[str], echo: bool) -> subprocess.CompletedProcess[str]:
            calls.append((tuple(command), echo))
            return _completed(command, "output\n")

        with patch.object(scenario.coverage, "run_command", run):
            result = scenario.run_command(["cargo", "metadata"], False)
        self.assertEqual(calls, [(("cargo", "metadata"), False)])
        self.assertEqual(result.stdout, "output\n")

    def test_echoed_commands_print_and_return_their_output_and_errors(self):
        stdout = _stdout()
        script = "import sys; print('a'); print('b', file=sys.stderr); sys.exit(3)"
        with redirect_stdout(stdout), redirect_stderr(io.StringIO()):
            result = scenario.run_command([sys.executable, "-c", script], True)
            stdout.flush()
        self.assertEqual(result.returncode, 3)
        self.assertEqual(sorted(result.stdout.split()), ["a", "b"])
        self.assertEqual(sorted(stdout.buffer.getvalue().split()), [b"a", b"b"])

    def test_interrupts_end_echoed_commands_with_every_process_that_they_started(
        self,
    ):
        # Cargo would leave the tests that it runs running.
        process = FakePopen(1)
        trees = EndedTrees(self)
        with (
            patch.object(scenario.subprocess, "Popen", process),
            patch.object(scenario, "terminate_process_tree", trees),
            redirect_stdout(_stdout()),
            redirect_stderr(io.StringIO()),
            self.assertRaises(KeyboardInterrupt),
        ):
            scenario.run_command(["cargo", "llvm-cov", "--no-report"], True)
        self.assertEqual(trees.processes, [process])


@unittest.skipUnless(sys.platform == "linux", "reads the environments in /proc")
class MarkedProcessTests(unittest.TestCase):
    def test_measurements_end_the_processes_that_hold_their_value(self):
        sigkill = cast(int, getattr(signal, "SIGKILL"))  # noqa: B009
        started = [
            subprocess.Popen(
                ["sleep", "60"],
                env={**os.environ, scenario.RUN_VARIABLE: marker},
                # As a process may leave a run's process tree.
                start_new_session=True,
            )
            for marker in ("1-2", "1-23")
        ]
        try:
            self.assertEqual(scenario.marked_processes("1-2"), [started[0].pid])
            scenario.end_marked("1-2")
            self.assertEqual(started[0].wait(timeout=10), -sigkill)
            # Another measurement's process runs on.
            self.assertIsNone(started[1].poll())
        finally:
            for process in started:
                process.kill()
                process.wait()


# What nvx-rs is in a fake measurement: a script that writes a profile where
# LLVM_PROFILE_FILE names it, as the instrumented nvx-rs does when it exits,
# unless an argument asks it to end as a process that a signal kills does.
FAKE_NVX_RS = """#!/bin/bash
for argument in "$@"; do
    [ "$argument" = --killed ] && exit 0
done
profile=${LLVM_PROFILE_FILE//%p/$$}
: >"${profile//%m/1234_0}"
"""
# The profiles of a fake measurement, named as PROFILE_PATTERN names them, by
# a process's ID and its executable's signature: the end of the names of
# nvx-rs's, and those of a build script, of the crate's test that a scenario
# runs, and of the crate's tests.
NVX_RS_PROFILE = "-1234_0.profraw"
BUILD_SCRIPT_PROFILE = "13579-9999_0.profraw"
LIFECYCLE_PROFILE = "24680-4567_0.profraw"
CRATE_TESTS_PROFILE = "nvx-rs-1-2.profraw"
CRATE_TESTS_OUTPUT = (
    "     Running unittests src/lib.rs (deps/nvx-0123)\n"
    "test result: ok. 3 passed; 0 failed; 0 ignored\n"
)


def _executable(path: Path, text: str = "") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    path.chmod(0o755)


def _profile(environment: Mapping[str, str], name: str) -> None:
    """Writes the profile `name` to the directory of the profiles of
    `environment`'s processes."""
    (Path(environment["LLVM_PROFILE_FILE"]).parent / name).write_bytes(b"")


class FakeHost:
    """Runs the commands of a measurement as the host would, without building
    or booting anything, in `root`.

    The build leaves FAKE_NVX_RS as nvx-rs, and a build script's profile.
    test-microvm runs nvx-rs through the wrapper three times, the last time as
    a killed process, writes the duration report of their scenarios, and
    builds and runs the crate's test that a scenario runs. The crate's tests
    leave a profile in the crate's directory, `crate_dir`, with `crate_stray`.
    The processes of the executables in `silent` write no profile.
    llvm-profdata merges by writing the names of the raw profiles that its
    output holds, and llvm-cov reports what reached() says that they reached
    of some executables.
    """

    def __init__(
        self,
        root: Path,
        *,
        build_status: int = 0,
        microvm_status: int = 0,
        silent: Collection[str] = (),
        crate_stray: bool = False,
        interrupt_microvm: bool = False,
    ):
        self.root = root
        self.llvm_profdata = root / "llvm-profdata"
        self.llvm_cov = root / "llvm-cov"
        self.crate_target = root / "crate-target"
        self.crate_dir = root / "crate"
        self.crate_dir.mkdir()
        self.build_status = build_status
        self.microvm_status = microvm_status
        self.silent = silent
        self.crate_stray = crate_stray
        self.interrupt_microvm = interrupt_microvm
        self.detached: subprocess.Popen[bytes] | None = None
        self.commands: list[tuple[str, ...]] = []
        self.streamed: list[tuple[tuple[str, ...], dict[str, str]]] = []
        self.forwarded: list[bool] = []
        self.merged: dict[str, list[str]] = {}
        self.exports: list[tuple[str, tuple[str, ...], str]] = []
        self.environment = {
            "PATH": os.environ["PATH"],
            "LLVM_PROFDATA": str(self.llvm_profdata),
            "LLVM_COV": str(self.llvm_cov),
        }

    def stream(
        self,
        command: Sequence[str],
        environment: Mapping[str, str],
        log: Path,
        *,
        forward_interrupts: bool = False,
    ) -> int:
        self.streamed.append((tuple(command), dict(environment)))
        self.forwarded.append(forward_interrupts)
        log.write_text("ran\n", encoding="utf-8")
        target = Path(environment["CARGO_TARGET_DIR"])
        if list(command[:2]) == ["cargo", "build"]:
            _executable(target / "release" / "nvx-rs", FAKE_NVX_RS)
            _executable(target / "release" / "deps" / "nvx_rs-0123", FAKE_NVX_RS)
            _profile(environment, BUILD_SCRIPT_PROFILE)
            return self.build_status
        if "test-microvm" in command:
            output = Path(command[list(command).index("--output-dir") + 1])
            output.mkdir(parents=True)
            began = time.time()
            self._nvx_rs(environment, "run", "--hypervisor", "kvm")
            self._nvx_rs(environment, "egress", "compile", "policy")
            time.sleep(0.05)
            middle = time.time()
            self._nvx_rs(environment, "run", "--killed")
            ended = time.time()
            spans = (
                ("setup", 1.0),
                ("lifecycle", middle - began),
                ("run-console-restore", ended - middle + 0.05),
                ("teardown", 0.01),
            )
            (output / "scenario-durations.jsonl").write_text(
                _durations(began - 1.0, spans), encoding="utf-8"
            )
            _executable(target / "debug" / "deps" / "managed_lifecycle-4567")
            if "managed_lifecycle" not in self.silent:
                _profile(environment, LIFECYCLE_PROFILE)
            if self.interrupt_microvm:
                # A process that outlives the run's process tree.
                self.detached = subprocess.Popen(
                    ["sleep", "60"], env=dict(environment), start_new_session=True
                )
                raise KeyboardInterrupt
            return self.microvm_status
        raise AssertionError(f"streamed {command}")

    def _nvx_rs(self, environment: Mapping[str, str], *arguments: str) -> None:
        command = [environment["NVX_RS"], "--root", "/r", *arguments]
        subprocess.run(command, env=dict(environment), timeout=60, check=True)

    def reached(
        self, profiles: Collection[str], objects: Sequence[str]
    ) -> dict[str, dict[int, int]]:
        """Returns what the raw profiles `profiles` reached of `objects`: the
        crate's tests, lines 1 and 2 of a host file and the guest code; nvx-rs,
        lines 2 and 3; and the crate's test that a scenario runs, line 4."""
        built = {name.split("-")[0] for name in objects}
        tests = CRATE_TESTS_PROFILE in profiles
        crate = "nvx" in built and tests
        nvx_rs = "nvx_rs" in built and any(
            name.endswith(NVX_RS_PROFILE) for name in profiles
        )
        lifecycle = LIFECYCLE_PROFILE in profiles
        hits = {
            1: crate,
            2: crate or nvx_rs,
            3: nvx_rs,
            4: "managed_lifecycle" in built and lifecycle,
        }
        files = {"src/host/cli/run.rs": {line: int(hit) for line, hit in hits.items()}}
        if "nvx" in built:
            files["src/guest/time.rs"] = {1: int(tests)}
        return files

    def run(
        self, command: Sequence[str], echo: bool
    ) -> subprocess.CompletedProcess[str]:
        self.commands.append(tuple(command))
        words = list(command)
        if words[:3] == ["cargo", "llvm-cov", "show-env"]:
            return _completed(command, "RUSTFLAGS='-C instrument-coverage'\n")
        if words[:2] == ["cargo", "metadata"]:
            target = {"target_directory": str(self.crate_target)}
            return _completed(command, json.dumps(target))
        if words[:3] == ["cargo", "llvm-cov", "--no-report"]:
            profiles = self.crate_target / "llvm-cov-target"
            _executable(profiles / "debug" / "deps" / "nvx-0123")
            (profiles / CRATE_TESTS_PROFILE).write_bytes(b"")
            if self.crate_stray:
                (self.crate_dir / "default_1_2.profraw").write_bytes(b"")
            return _completed(command, CRATE_TESTS_OUTPUT)
        if words[:3] == ["cargo", "llvm-cov", "--version"]:
            return _completed(command, "cargo-llvm-cov 1.0.0\n")
        if words[:3] == ["cargo", "llvm-cov", "clean"]:
            return _completed(command)
        if words[0] == "rustc":
            return _completed(command, "rustc 1.0.0 (0 1970-01-01)\n")
        if words[0] == "git":
            return _completed(command, "0123456789abcdef0123456789abcdef01234567\n")
        if words[0] == str(self.llvm_profdata):
            listing = Path(words[words.index("-f") + 1]).read_text(encoding="utf-8")
            output = Path(words[words.index("-o") + 1])
            inputs = [Path(line) for line in listing.splitlines()]
            self.merged[output.stem] = sorted(path.name for path in inputs)
            held: list[str] = []
            for path in inputs:
                if path.suffix == ".profdata":
                    held += json.loads(path.read_text(encoding="utf-8"))
                else:
                    held.append(path.name)
            output.write_text(json.dumps(sorted(held)), encoding="utf-8")
            return _completed(command)
        if words[0] == str(self.llvm_cov):
            indexed = Path(words[words.index("-instr-profile") + 1])
            profile = indexed.stem
            if words[1] == "show":
                html = Path(words[words.index("-output-dir") + 1])
                html.mkdir(parents=True)
                (html / "index.html").write_text("<html/>", encoding="utf-8")
                return _completed(command)
            objects = tuple(
                Path(words[index + 1]).name
                for index, word in enumerate(words)
                if word == "-object"
            )
            form = "lcov" if "-format=lcov" in words else "summary"
            self.exports.append((profile, objects, form))
            held = cast(list[str], json.loads(indexed.read_text(encoding="utf-8")))
            files = self.reached(held, objects)
            if form == "lcov":
                return _completed(command, _lcov(files))
            counts = {
                path: (sum(1 for hit in hits.values() if hit), len(hits))
                for path, hits in files.items()
            }
            return _completed(command, json.dumps(_summary(counts)))
        raise AssertionError(f"ran {command}")


@unittest.skipUnless(_bash5(), "runs the nvx-rs wrapper with Bash 5 or later")
class MeasureTests(unittest.TestCase):
    def _measure(self, host: FakeHost) -> tuple[int, str]:
        """Measures with `host` into its root's out/, and returns the status
        and what the measurement printed on standard error."""
        stderr = io.StringIO()
        with (
            redirect_stderr(stderr),
            redirect_stdout(io.StringIO()),
            patch.dict("os.environ", {"CARGO_LLVM_COV_TARGET_DIR": ""}),
            patch.object(scenario, "CRATE_DIR", host.crate_dir),
        ):
            status = scenario.measure(
                host.root / "out",
                scenarios=["lifecycle", "run-console-restore"],
                work=host.root / "work",
                run=host.run,
                run_streamed=host.stream,
                environment=host.environment,
            )
        return status, stderr.getvalue()

    def test_measurements_run_the_scenarios_and_the_crate_s_tests(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(Path(directory).resolve())
            self.assertEqual(self._measure(host), (0, ""))
            output = host.root / "out"
            work = host.root / "work"
            summary = (output / "summary.md").read_text(encoding="utf-8")
            measurement = json.loads((output / "measurement.json").read_text("utf-8"))
            found = json.loads((output / "processes.json").read_text("utf-8"))
            reports = {
                name: sorted(path.name for path in (output / name).iterdir())
                for name in scenario.RUNS
            }
            logs = [(output / name).is_file() for name in ("build.log", "microvm.log")]
        # The build and the runs, with the instrumentation of show-env, of
        # which test-microvm alone takes the interrupts of the measurement to
        # end what it started.
        build, microvm = host.streamed
        self.assertEqual(host.forwarded, [False, True])
        self.assertEqual(build[0], scenario.BUILD)
        python = [sys.executable, str(scenario.NVX_PY)]
        self.assertEqual(
            list(microvm[0]),
            [*python, "test-microvm", "--backend", "kvm", "--output-dir"]
            + [str(output / "microvm"), "--scenario", "lifecycle"]
            + ["--scenario", "run-console-restore"],
        )
        for _, environment in host.streamed:
            self.assertEqual(environment["RUSTFLAGS"], "-C instrument-coverage")
            self.assertEqual(environment["CARGO_TARGET_DIR"], str(work / "target"))
            self.assertEqual(
                environment["LLVM_PROFILE_FILE"],
                str(work / "profiles" / "microvm" / "%p-%m.profraw"),
            )
            self.assertEqual(environment["NVX_RS"], str(work / "bin" / "nvx-rs"))
        # The processes of both streamed commands, and theirs alone, share a value.
        self.assertEqual(
            len(
                {environment[scenario.RUN_VARIABLE] for _, environment in host.streamed}
            ),
            1,
        )
        self.assertNotIn(scenario.RUN_VARIABLE, host.environment)
        self.assertEqual(logs, [True, True])
        # The crate's tests ran as nvx_rs_coverage.py runs them.
        self.assertIn(
            ("cargo", "llvm-cov", "--no-report", *coverage.RUN_OPTIONS), host.commands
        )
        # The scenarios' profiles are those of the nvx-rs processes that ended,
        # of the build, and of the executables that the runs built, and the
        # profile of both, those of each.
        nvx_rs_profiles = host.merged["nvx-rs"]
        self.assertEqual(len(nvx_rs_profiles), 2)
        self.assertTrue(all(name.endswith(NVX_RS_PROFILE) for name in nvx_rs_profiles))
        self.assertEqual(
            host.merged["scenarios"],
            sorted([BUILD_SCRIPT_PROFILE, LIFECYCLE_PROFILE] + nvx_rs_profiles),
        )
        self.assertEqual(host.merged["crate-tests"], [CRATE_TESTS_PROFILE])
        self.assertEqual(
            host.merged["together"], ["crate-tests.profdata", "scenarios.profdata"]
        )
        # Whether the processes of each group of executables wrote profiles
        # shows in the profiles of their run's directory alone.
        self.assertEqual(
            host.merged["microvm"], [BUILD_SCRIPT_PROFILE, LIFECYCLE_PROFILE]
        )
        for group in (
            ("nvx-rs", ("nvx_rs-0123",)),
            ("microvm", ("managed_lifecycle-4567",)),
        ):
            self.assertIn((*group, "summary"), host.exports)
        # Each run's reports read every executable that the runs built.
        every = (
            "nvx_rs-0123",
            "managed_lifecycle-4567",
            "nvx-0123",
        )
        for name in scenario.RUNS:
            for form in ("lcov", "summary"):
                self.assertIn((name, every, form), host.exports)
        self.assertEqual(
            reports,
            {
                "crate-tests": ["coverage.json", "lcov.info"],
                "scenarios": ["coverage.json", "html", "lcov.info"],
                "together": ["coverage.json", "html", "lcov.info"],
            },
        )
        self.assertEqual(measurement["processes"], {"started": 3, "profiled": 2})
        self.assertEqual(
            measurement["profiled_groups"],
            [
                "`nvx-rs`",
                "the crate's tests that the scenarios run",
            ],
        )
        self.assertEqual(measurement["scenario_builds"], {"lines": 4, "reached": 3})
        self.assertEqual(
            [(entry["span"], entry["command"], entry["profile"]) for entry in found],
            [
                ("lifecycle", "nvx-rs run", True),
                ("lifecycle", "nvx-rs egress compile", True),
                ("run-console-restore", "nvx-rs run", False),
            ],
        )
        self.assertIn(
            "| The crate's tests | 60.0% of 5 | 60.0% of 5 |\n"
            "| The scenarios | 60.0% of 5 | 60.0% of 5 |\n"
            "| Together | 100.0% of 5 | 100.0% of 5 |\n",
            summary,
        )
        self.assertIn(
            "The scenarios reach 2 lines that the crate's tests don't.", summary
        )
        self.assertIn("| `run-console-restore` | `nvx-rs run` | 1 | 0 |\n", summary)
        self.assertNotIn("misses", summary)

    def test_failed_runs_fail_the_measurement_after_its_reports(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(Path(directory).resolve(), microvm_status=1)
            status, stderr = self._measure(host)
            summary = (host.root / "out" / "summary.md").read_text(encoding="utf-8")
        self.assertEqual(status, 1)
        self.assertIn("error: `nvx.py test-microvm` failed with status 1.", stderr)
        self.assertIn("- `nvx.py test-microvm` failed with status 1.\n", summary)

    def test_the_profiles_that_the_crate_s_tests_leave_count_once(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(Path(directory).resolve(), crate_stray=True)
            status, stderr = self._measure(host)
            measurement = json.loads(
                (host.root / "out" / "measurement.json").read_text("utf-8")
            )
        self.assertEqual(status, 1)
        self.assertIn(
            "error: 1 process wrote profiles where the measurement doesn't merge",
            stderr,
        )
        self.assertEqual(stderr.count("default_1_2.profraw"), 1)
        self.assertEqual(
            measurement["crate_tests"]["stray_profiles"], ["default_1_2.profraw"]
        )
        self.assertEqual(measurement["stray_profiles"], [])

    def test_executables_that_write_no_profile_fail_the_measurement(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(Path(directory).resolve(), silent={"managed_lifecycle"})
            status, stderr = self._measure(host)
        self.assertEqual(status, 1)
        self.assertIn(
            "No process of the crate's tests that the scenarios run wrote a "
            "profile that reaches the crate's production code.",
            stderr,
        )
        # The other group's processes wrote theirs.
        self.assertEqual(stderr.count("No process of"), 1)
        # The build's profile, which the crate's test's directory holds too,
        # reaches nothing of the test.
        self.assertEqual(host.merged["microvm"], [BUILD_SCRIPT_PROFILE])

    def test_a_failed_build_ends_the_measurement(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(Path(directory).resolve(), build_status=101)
            with self.assertRaisesRegex(coverage.MeasurementError, "build"):
                self._measure(host)
        self.assertEqual(len(host.streamed), 1)

    def test_interrupts_end_the_processes_that_left_the_runs(self):
        sigkill = cast(int, getattr(signal, "SIGKILL"))  # noqa: B009
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(Path(directory).resolve(), interrupt_microvm=True)
            try:
                with self.assertRaises(KeyboardInterrupt):
                    self._measure(host)
                assert host.detached is not None
                self.assertEqual(host.detached.wait(timeout=10), -sigkill)
                # The crate's tests didn't run.
                self.assertNotIn(
                    ("cargo", "llvm-cov", "--no-report", *coverage.RUN_OPTIONS),
                    host.commands,
                )
            finally:
                if host.detached is not None and host.detached.poll() is None:
                    host.detached.kill()
                    host.detached.wait()


class CommandTests(unittest.TestCase):
    def _main(
        self,
        arguments: Sequence[str],
        system: str = "linux",
        raised: BaseException | None = None,
    ) -> tuple[int, list[dict[str, Any]], str]:
        calls: list[dict[str, Any]] = []

        def measure(output: Path, **options: Any) -> int:
            calls.append({"output": output, **options})
            if raised is not None:
                raise raised
            return 0

        stderr = io.StringIO()
        with (
            patch.object(scenario.sys, "platform", system),
            patch.object(scenario, "measure", measure),
            patch.object(scenario.signal, "signal"),
            redirect_stderr(stderr),
        ):
            status = scenario.main(arguments)
        return status, calls, stderr.getvalue()

    def test_the_measurement_writes_to_the_build_directory_by_default(self):
        status, calls, _ = self._main(["--scenario", "smp"])
        self.assertEqual(status, 0)
        self.assertEqual(
            calls,
            [{"output": scenario.DEFAULT_OUTPUT, "scenarios": ["smp"]}],
        )

    def test_measurements_run_on_linux_only(self):
        status, calls, stderr = self._main([], system="win32")
        self.assertEqual((status, calls), (1, []))
        self.assertIn("on Linux only", stderr)

    def test_errors_and_interrupts_end_the_measurement(self):
        status, _, stderr = self._main(
            [], raised=coverage.MeasurementError("no profile")
        )
        self.assertEqual((status, stderr), (1, "error: no profile\n"))
        status, _, stderr = self._main([], raised=KeyboardInterrupt())
        self.assertEqual((status, stderr), (130, "Interrupted\n"))


if __name__ == "__main__":
    unittest.main()

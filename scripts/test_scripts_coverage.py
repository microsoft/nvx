#!/usr/bin/env python3
"""Tests for the scripts coverage workflow and scripts_coverage.py, which
measure which statements and branches of the Python host tooling in scripts/
the unit tests that validate-nvx runs reach.

They check the workflow's triggers, that its job prepares the tests as the NVX
CLI / Linux job of ci.yml does, the coverage.py release that it installs, and
the names of its artifacts; that the script runs the unit tests that
validate-nvx runs, with a configuration that leaves the test code out; and,
with a stub of coverage.py, how it measures, reports, and fails. Where
coverage.py is installed, a test also measures a sample checkout with it.
"""

from __future__ import annotations

import configparser
import contextlib
import importlib.util
import io
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from collections.abc import Generator, Mapping, Sequence
from pathlib import Path
from typing import Any, cast
from unittest.mock import patch

import yaml

sys.path.insert(0, str(Path(__file__).parent))
import scripts_coverage  # noqa: E402
from scripts_coverage import Count, Totals  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = "scripts-coverage.yml"
BASH = shutil.which("bash")


def _write(root: Path, files: Mapping[str, str]) -> None:
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")


def _workflow(name: str) -> dict[Any, Any]:
    path = REPO_ROOT / ".github" / "workflows" / name
    return cast(dict[Any, Any], yaml.safe_load(path.read_text(encoding="utf-8")))


def _step(workflow: Mapping[Any, Any], job: str, name: str) -> dict[str, Any]:
    steps = cast(list[dict[str, Any]], workflow["jobs"][job]["steps"])
    return next(step for step in steps if step.get("name") == name)


def _action(linux: str) -> str:
    """Returns a validate-nvx action whose Linux step runs `linux`."""
    return yaml.safe_dump(
        {
            "runs": {
                "using": "composite",
                "steps": [
                    {
                        "name": scripts_coverage.VALIDATE_NVX_STEP,
                        "shell": "bash",
                        "run": linux,
                    }
                ],
            }
        }
    )


def _totals(statements: tuple[int, int], branches: tuple[int, int]) -> Totals:
    return Totals(Count(*statements), Count(*branches))


def _summary(statements: tuple[int, int], branches: tuple[int, int]) -> dict[str, int]:
    return {
        "covered_lines": statements[0],
        "num_statements": statements[1],
        "covered_branches": branches[0],
        "num_branches": branches[1],
    }


class WorkflowTests(unittest.TestCase):
    def test_the_workflow_never_runs_for_pull_requests(self) -> None:
        workflow = _workflow(WORKFLOW)
        # YAML 1.1 reads the key `on` as true.
        triggers = cast(dict[str, Any], workflow.get("on", workflow.get(True)))
        self.assertEqual(set(triggers), {"push", "workflow_dispatch"})
        # A push to dev measures again when the tooling, its tests, or how they
        # are measured change.
        self.assertEqual(
            triggers["push"],
            {
                "branches": ["dev"],
                "paths": [
                    "scripts/**",
                    "requirements-dev.txt",
                    scripts_coverage.VALIDATE_NVX,
                    f".github/workflows/{WORKFLOW}",
                ],
            },
        )
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        self.assertIs(workflow["concurrency"]["cancel-in-progress"], True)

    def test_the_job_prepares_the_tests_as_the_nvx_cli_job_does(self) -> None:
        workflow = _workflow(WORKFLOW)
        ci = _workflow("ci.yml")
        job = workflow["jobs"]["measure"]
        cli = ci["jobs"]["nvx-cli-linux"]
        self.assertEqual(job["runs-on"], cli["runs-on"])
        # The tests that run nvx-rs run the build that NVX_RS names.
        self.assertEqual(job["env"], cli["env"])
        for name in ("Checkout", "Checkout OpenVMM", "Set up Python"):
            with self.subTest(step=name):
                self.assertEqual(
                    _step(workflow, "measure", name), _step(ci, "nvx-cli-linux", name)
                )
        for name in ("Build nvx-rs", "Record nvx-rs provenance"):
            with self.subTest(step=name):
                self.assertEqual(
                    _step(workflow, "measure", name),
                    _step(ci, "build-nvx-rs-linux-musl", name),
                )

    def test_the_job_installs_the_coverage_release_that_requirements_pin(self) -> None:
        requirements = (REPO_ROOT / "requirements-dev.txt").read_text(encoding="utf-8")
        pins = [line for line in requirements.splitlines() if line.startswith("cov")]
        # coverage.py 7.17 needs Python 3.11, and the quality checks, which
        # install the file too, run on 3.10.
        self.assertEqual(len(pins), 1)
        self.assertRegex(
            pins[0], r'^coverage==\d+\.\d+\.\d+; python_version >= "3\.11"$'
        )
        run = _step(_workflow(WORKFLOW), "measure", "Install coverage.py")["run"]
        self.assertIn("--requirement scripts/setup/azure/requirements.txt", run)
        action = (REPO_ROOT / scripts_coverage.VALIDATE_NVX).read_text(encoding="utf-8")
        self.assertIn(
            "python3 -m pip install --requirement scripts/setup/azure/requirements.txt",
            action,
        )

    @unittest.skipIf(
        os.name == "nt" or BASH is None, "runs the workflow's bash step on POSIX"
    )
    def test_the_install_step_passes_the_pin_to_pip(self) -> None:
        run = _step(_workflow(WORKFLOW), "measure", "Install coverage.py")["run"]
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            arguments = directory / "arguments"
            stub = directory / "python3"
            stub.write_text(
                f'#!/bin/sh\nprintf "%s\\n" "$@" > "{arguments}"\n', encoding="utf-8"
            )
            stub.chmod(0o755)
            assert BASH is not None
            subprocess.run(
                [BASH, "-c", run],
                cwd=REPO_ROOT,
                env={
                    **os.environ,
                    "PATH": f"{directory}{os.pathsep}{os.environ['PATH']}",
                },
                check=True,
            )
            passed = arguments.read_text(encoding="utf-8").splitlines()
        requirements = (REPO_ROOT / "requirements-dev.txt").read_text(encoding="utf-8")
        pin = next(line for line in requirements.splitlines() if line.startswith("cov"))
        self.assertEqual(
            passed,
            [
                "-m",
                "pip",
                "install",
                "--requirement",
                "scripts/setup/azure/requirements.txt",
                pin,
            ],
        )

    def test_the_job_reports_whether_the_tests_pass(self) -> None:
        workflow = _workflow(WORKFLOW)
        output = "build/scripts-coverage"
        self.assertEqual(
            scripts_coverage.DEFAULT_OUTPUT, REPO_ROOT / Path(*output.split("/"))
        )
        self.assertEqual(
            _step(workflow, "measure", "Measure")["run"],
            f"python3 scripts/scripts_coverage.py --output {output}",
        )
        summary = _step(workflow, "measure", "Write the summary")
        self.assertEqual(
            summary["run"], f'cat {output}/summary.md >> "${{GITHUB_STEP_SUMMARY}}"'
        )
        upload = _step(workflow, "measure", "Upload the reports")
        # The name holds no attempt, so that a rerun replaces the reports of
        # its earlier attempt.
        self.assertEqual(
            upload["with"],
            {
                "name": "scripts-coverage-${{ github.run_id }}",
                "path": output,
                "if-no-files-found": "error",
                "overwrite": True,
                "retention-days": 30,
            },
        )
        for step in (summary, upload):
            self.assertIn("!cancelled()", step["if"])


class CommandTests(unittest.TestCase):
    def test_the_measurement_runs_the_unit_tests_that_validate_nvx_runs(self) -> None:
        commands = scripts_coverage.test_commands()
        action = cast(
            dict[str, Any],
            yaml.safe_load(
                (REPO_ROOT / scripts_coverage.VALIDATE_NVX).read_text(encoding="utf-8")
            ),
        )
        steps = {
            cast(str, step["name"]): cast(str, step["run"])
            for step in cast(list[dict[str, Any]], action["runs"]["steps"])
        }
        linux = [
            line.strip()
            for line in steps[scripts_coverage.VALIDATE_NVX_STEP].splitlines()
        ]
        for arguments in commands:
            self.assertIn("python3 " + " ".join(arguments), linux)
        # The Windows step runs the same tests.
        windows = [
            tuple(argument.replace("\\", "/") for argument in line.split()[1:])
            for line in map(
                str.strip, steps["Validate NVX CLI on Windows"].splitlines()
            )
            if re.fullmatch(r"python (-m unittest )?(\S+\.py )+-v", line)
        ]
        self.assertEqual(windows, commands)
        # Every unit test of scripts/ runs, and this module's too.
        modules = {argument for command in commands for argument in command}
        unit_tests = {
            f"scripts/{path.name}" for path in (REPO_ROOT / "scripts").glob("test_*.py")
        }
        self.assertLessEqual(unit_tests, modules)
        self.assertIn("scripts/test_scripts_coverage.py", modules)

    def test_only_the_commands_that_run_unit_tests_count(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            _write(
                root,
                {
                    scripts_coverage.VALIDATE_NVX: _action(
                        "set -euo pipefail\n"
                        "python3 -m compileall -q scripts\n"
                        "python3 -m unittest scripts/test_a.py scripts/test_b.py -v\n"
                        "python3 scripts/test_c.py -v\n"
                        "python3 .github/skills/demo/scripts/test_d.py -v\n"
                        "python3 scripts/nvx.py -v\n"
                        "python3 scripts/nvx.py --help\n"
                    ),
                    "scripts/test_a.py": "",
                    "scripts/test_b.py": "",
                    "scripts/test_c.py": "",
                    "scripts/nvx.py": "",
                    ".github/skills/demo/scripts/test_d.py": "",
                },
            )
            self.assertEqual(
                scripts_coverage.test_commands(root),
                [
                    ("-m", "unittest", "scripts/test_a.py", "scripts/test_b.py", "-v"),
                    ("scripts/test_c.py", "-v"),
                    (".github/skills/demo/scripts/test_d.py", "-v"),
                ],
            )
            (root / "scripts" / "test_b.py").unlink()
            with self.assertRaisesRegex(
                scripts_coverage.MeasurementError, "do not exist: scripts/test_b.py$"
            ):
                scripts_coverage.test_commands(root)
            _write(
                root, {scripts_coverage.VALIDATE_NVX: _action("python3 x.py --help\n")}
            )
            with self.assertRaisesRegex(
                scripts_coverage.MeasurementError, "runs no unit tests"
            ):
                scripts_coverage.test_commands(root)
            _write(root, {scripts_coverage.VALIDATE_NVX: "runs: []\n"})
            with self.assertRaisesRegex(
                scripts_coverage.MeasurementError, "cannot read"
            ):
                scripts_coverage.test_commands(root)


class ConfigurationTests(unittest.TestCase):
    def test_the_measurement_leaves_out_the_unit_tests_and_their_support(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            # coverage.py reads `$NAME` in a setting as an environment variable.
            root = Path(temporary) / "check$out"
            data = Path(temporary) / "data"
            _write(
                root,
                {
                    "scripts/tool.py": "",
                    "scripts/test_tool.py": "",
                    "scripts/powershell_testing.py": "",
                    "scripts/nvx_tools/__init__.py": "",
                    "scripts/nvx_tools/demo_tests.py": "",
                },
            )
            self.assertEqual(
                scripts_coverage.unmeasured_modules(root),
                ["scripts/powershell_testing.py", "scripts/test_tool.py"],
            )
            parser = configparser.RawConfigParser()
            parser.read_string(scripts_coverage.configuration(root, data))

            def setting(path: Path) -> str:
                return str(path).replace("$", "$$")

            run = parser["run"]
            self.assertEqual(run["branch"], "true")
            self.assertEqual(run["patch"], "subprocess")
            self.assertEqual(run["source"], setting(root / "scripts"))
            self.assertEqual(run["data_file"], setting(data / "coverage"))
            self.assertEqual(
                [line.strip() for line in run["omit"].splitlines() if line.strip()],
                [
                    setting(root / "scripts" / "powershell_testing.py"),
                    setting(root / "scripts" / "test_tool.py"),
                ],
            )
            self.assertEqual(parser["report"]["include_namespace_packages"], "true")

    def test_the_unmeasured_modules_of_the_repository_are_test_code(self) -> None:
        unmeasured = scripts_coverage.unmeasured_modules()
        self.assertIn("scripts/test_nvx_tools.py", unmeasured)
        self.assertIn("scripts/powershell_testing.py", unmeasured)
        self.assertNotIn("scripts/nvx.py", unmeasured)
        self.assertNotIn("scripts/nvx_tools/microvm_tests.py", unmeasured)

    def test_the_commands_inherit_no_other_measurement(self) -> None:
        base = {
            "PATH": "/bin",
            "COVERAGE_PROCESS_CONFIG": ":data:{}",
            "COVERAGE_PROCESS_START": "/tmp/.coveragerc",
            "COVERAGE_RCFILE": "/tmp/.coveragerc",
            "COVERAGE_FILE": "/tmp/.coverage",
            "COVERAGE_CORE": "ctrace",
        }
        self.assertEqual(
            scripts_coverage.environment(base),
            {"PATH": "/bin", "COVERAGE_CORE": "ctrace"},
        )


class ReportTests(unittest.TestCase):
    def test_the_scenario_drivers_count_apart(self) -> None:
        groups = scripts_coverage.analyze(
            {
                "files": {
                    "scripts/nvx.py": {"summary": _summary((3, 4), (1, 2))},
                    "scripts\\nvx_tools\\benchmark.py": {
                        "summary": _summary((5, 10), (0, 0))
                    },
                    "scripts/nvx_tools/microvm_tests.py": {
                        "summary": _summary((1, 9), (2, 4))
                    },
                }
            }
        )
        self.assertEqual(
            groups,
            {
                scripts_coverage.HOST_TOOLING: {
                    "scripts/nvx.py": _totals((3, 4), (1, 2)),
                    "scripts/nvx_tools/benchmark.py": _totals((5, 10), (0, 0)),
                },
                scripts_coverage.SCENARIO_DRIVERS: {
                    "scripts/nvx_tools/microvm_tests.py": _totals((1, 9), (2, 4)),
                },
            },
        )
        self.assertEqual(
            scripts_coverage.total(groups[scripts_coverage.HOST_TOOLING]),
            _totals((8, 14), (1, 2)),
        )

    def test_a_report_that_counts_test_code_is_refused(self) -> None:
        for path in ("scripts/test_nvx_tools.py", "scripts/powershell_testing.py"):
            with self.subTest(path=path):
                with self.assertRaisesRegex(
                    scripts_coverage.MeasurementError, f"counts test code: {path}$"
                ):
                    scripts_coverage.analyze(
                        {"files": {path: {"summary": _summary((1, 1), (0, 0))}}}
                    )
        malformed: tuple[object, ...] = (
            {},
            {"files": {}},
            {"files": {"scripts/a.py": {}}},
            [],
        )
        for report in malformed:
            with self.subTest(report=report):
                with self.assertRaises(scripts_coverage.MeasurementError):
                    scripts_coverage.analyze(report)
        with self.assertRaisesRegex(scripts_coverage.MeasurementError, "not a count"):
            scripts_coverage.analyze(
                {"files": {"scripts/a.py": {"summary": {"covered_lines": True}}}}
            )

    def test_the_outcome_is_the_end_of_unittest_s_report(self) -> None:
        for output, expected in (
            ("\n---\nRan 84 tests in 12.476s\n\nOK\n", "84 tests: OK"),
            ("Ran 1 test in 0.001s\n\nOK (skipped=1)\n", "1 test: OK (skipped=1)"),
            (
                "Ran 2 tests in 1s\nFAILED\nRan 1831 tests in 221.238s\n\n"
                "FAILED (errors=1, skipped=35)\n",
                "1,831 tests: FAILED (errors=1, skipped=35)",
            ),
            ("Traceback (most recent call last):\n", None),
            ("Ran 3 tests in 1.0s\n", None),
        ):
            with self.subTest(output=output):
                self.assertEqual(scripts_coverage.outcome(output), expected)

    def test_the_summary_shows_the_totals_the_tests_and_each_module(self) -> None:
        measurement = {
            "host": "Linux",
            "commit": "0123456789abcdef",
            "python": "3.12.16",
            "coverage": "7.17.0",
            "runs": [
                {
                    "arguments": ["-m", "unittest", "scripts/test_a.py", "-v"],
                    "status": 0,
                    "outcome": "2 tests: OK",
                    "seconds": 1.5,
                },
                {
                    "arguments": ["scripts/test_b.py", "-v"],
                    "status": 1,
                    "outcome": None,
                    "seconds": 0.5,
                },
            ],
        }
        groups = {
            scripts_coverage.HOST_TOOLING: {
                "scripts/b.py": _totals((999, 1000), (0, 0)),
                "scripts/a.py": _totals((1, 3), (2, 2)),
            },
            scripts_coverage.SCENARIO_DRIVERS: {},
        }
        summary = scripts_coverage.render(measurement, groups)
        lines = summary.splitlines()
        self.assertEqual(lines[0], "## scripts coverage on Linux")
        self.assertIn("at `0123456789ab`", summary)
        self.assertIn("on Python 3.12.16, measured by coverage.py 7.17.0", summary)
        # Shares round down, so that only what the tests reached entirely shows
        # 100%.
        self.assertIn("| Host tooling | 99.7% of 1,003 | 100.0% of 2 |", lines)
        self.assertIn("| Scenario drivers | none | none |", lines)
        self.assertIn(
            "| `python3 -m unittest` with 1 module | 0 | 2 tests: OK | 1.5 |", lines
        )
        self.assertIn("| `python3 scripts/test_b.py -v` | 1 | unknown | 0.5 |", lines)
        self.assertIn(
            "The tests of `python3 scripts/test_b.py -v` failed, so the measurement "
            "misses what they would have reached.",
            lines,
        )
        modules = lines.index("### Host tooling by module")
        self.assertEqual(
            lines[modules + 4 : modules + 6],
            [
                "| `scripts/a.py` | 33.3% of 3 | 100.0% of 2 |",
                "| `scripts/b.py` | 99.9% of 1,000 | none |",
            ],
        )
        drivers = lines.index("### Scenario drivers by module")
        self.assertEqual(lines[drivers + 2], "No module.")


class _StubCoverage:
    """Stands in for coverage.py and Git: records the commands that a
    measurement runs, and answers them as they would."""

    def __init__(self, statuses: Sequence[int], report: Mapping[str, Any]) -> None:
        self.statuses = list(statuses)
        self.report = report
        self.commands: list[list[str]] = []
        self.settings: list[str] = []

    def __call__(
        self, command: Sequence[str], cwd: Path, echo: bool
    ) -> subprocess.CompletedProcess[str]:
        command = list(command)
        self.commands.append(command)
        if command[0] == "git":
            return subprocess.CompletedProcess(command, 0, "0123456789abcdef\n")
        action = command[3]
        output = ""
        status = 0
        if action == "--version":
            output = "Coverage.py, version 7.17.0 with C extension\nFull documentation"
        elif action == "run":
            self.settings.append(
                Path(command[4].removeprefix("--rcfile=")).read_text(encoding="utf-8")
            )
            status = self.statuses.pop(0)
            output = f"Ran 2 tests in 0.1s\n\n{'OK' if status == 0 else 'FAILED'}\n"
        elif action == "combine":
            settings = Path(command[4].removeprefix("--rcfile=")).read_text(
                encoding="utf-8"
            )
            found = re.search(r"^data_file = (.+)$", settings, re.MULTILINE)
            assert found is not None
            Path(found.group(1).replace("$$", "$")).write_bytes(b"combined")
        elif action == "json":
            path = Path(command[command.index("-o") + 1])
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(self.report), encoding="utf-8")
        elif action == "lcov":
            path = Path(command[command.index("-o") + 1])
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("TN:\n", encoding="utf-8")
        elif action == "html":
            path = Path(command[command.index("-d") + 1])
            path.mkdir(parents=True, exist_ok=True)
            (path / "index.html").write_text("<html></html>", encoding="utf-8")
        return subprocess.CompletedProcess(command, status, output)


REPORT = {
    "files": {
        "scripts/tool.py": {"summary": _summary((3, 4), (1, 2))},
        "scripts/nvx_tools/demo_tests.py": {"summary": _summary((1, 2), (0, 0))},
    }
}


@contextlib.contextmanager
def _checkout() -> Generator[Path]:
    """Yields a sample checkout whose validate-nvx runs two commands of unit
    tests."""
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary) / "checkout"
        _write(
            root,
            {
                scripts_coverage.VALIDATE_NVX: _action(
                    "python3 -m unittest scripts/test_a.py -v\n"
                    "python3 scripts/test_b.py -v\n"
                ),
                "scripts/test_a.py": "",
                "scripts/test_b.py": "",
                "scripts/tool.py": "",
            },
        )
        yield root


class MeasureTests(unittest.TestCase):
    def test_a_measurement_runs_the_tests_under_coverage_and_reports(self) -> None:
        stub = _StubCoverage([0, 0], REPORT)
        with _checkout() as root:
            output = root.parent / "reports"
            with contextlib.redirect_stderr(io.StringIO()):
                status = scripts_coverage.measure(
                    output, root=root, run=stub, python="python"
                )
            self.assertEqual(status, 0)
            coverage = ["python", "-m", "coverage"]
            rcfile = stub.commands[1][4]
            self.assertTrue(rcfile.startswith("--rcfile="))
            self.assertEqual(
                stub.commands[:4],
                [
                    [*coverage, "--version"],
                    [
                        *coverage,
                        "run",
                        rcfile,
                        "-m",
                        "unittest",
                        "scripts/test_a.py",
                        "-v",
                    ],
                    [*coverage, "run", rcfile, "scripts/test_b.py", "-v"],
                    [*coverage, "combine", rcfile, "--quiet"],
                ],
            )
            self.assertIn(f"source = {root.resolve() / 'scripts'}", stub.settings[0])
            found = re.search(r"^data_file = (.+)$", stub.settings[0], re.MULTILINE)
            assert found is not None
            data = Path(found.group(1)).parent
            # Every report reads a copy of the combined data, apart from the
            # data files that the processes write, which a report command
            # would combine first.
            combined = stub.commands[4]
            self.assertEqual(combined[3:5], ["json", rcfile])
            reading = combined[5]
            self.assertTrue(reading.startswith("--data-file="))
            self.assertNotEqual(Path(reading.removeprefix("--data-file=")).parent, data)
            self.assertNotEqual(Path(combined[combined.index("-o") + 1]).parent, data)
            for command in stub.commands[4:-1]:
                self.assertIn(reading, command)
            # Each group's reports count its modules alone.
            reports = {
                (command[3], argument)
                for command in stub.commands[5:-1]
                for argument in command
                if argument.startswith("--include=")
            }
            for group, path in (
                (scripts_coverage.HOST_TOOLING, "scripts/tool.py"),
                (scripts_coverage.SCENARIO_DRIVERS, "scripts/nvx_tools/demo_tests.py"),
            ):
                include = f"--include={root.resolve() / path}"
                with self.subTest(group=group):
                    for action in ("json", "lcov", "html"):
                        self.assertIn((action, include), reports)
                    for name in ("coverage.json", "lcov.info", "html/index.html"):
                        self.assertTrue((output / group / name).is_file(), name)
            self.assertEqual(stub.commands[-1][0], "git")
            measurement = json.loads(
                (output / "measurement.json").read_text(encoding="utf-8")
            )
            self.assertEqual(measurement["commit"], "0123456789abcdef")
            self.assertEqual(measurement["coverage"], "7.17.0")
            self.assertEqual(
                [
                    (run["arguments"], run["status"], run["outcome"])
                    for run in measurement["runs"]
                ],
                [
                    (["-m", "unittest", "scripts/test_a.py", "-v"], 0, "2 tests: OK"),
                    (["scripts/test_b.py", "-v"], 0, "2 tests: OK"),
                ],
            )
            self.assertEqual(
                measurement["totals"],
                {
                    scripts_coverage.HOST_TOOLING: {
                        "statements": [3, 4],
                        "branches": [1, 2],
                    },
                    scripts_coverage.SCENARIO_DRIVERS: {
                        "statements": [1, 2],
                        "branches": [0, 0],
                    },
                },
            )
            log = (output / "tests.log").read_text(encoding="utf-8")
            self.assertIn("scripts/test_b.py -v\nRan 2 tests in 0.1s\n\nOK\n", log)
            summary = (output / "summary.md").read_text(encoding="utf-8")
            self.assertIn("| Host tooling | 75.0% of 4 | 50.0% of 2 |", summary)
            self.assertNotIn("failed", summary)

    def test_failed_tests_fail_the_measurement_after_its_reports(self) -> None:
        stub = _StubCoverage([0, 1], REPORT)
        with _checkout() as root:
            output = root.parent / "reports"
            errors = io.StringIO()
            with contextlib.redirect_stderr(errors):
                status = scripts_coverage.measure(output, root=root, run=stub)
            self.assertEqual(status, 1)
            self.assertIn(
                "error: the tests of `python3 scripts/test_b.py -v` failed with status 1",
                errors.getvalue(),
            )
            summary = (output / "summary.md").read_text(encoding="utf-8")
            self.assertIn("The tests of `python3 scripts/test_b.py -v` failed", summary)
            self.assertTrue((output / "host-tooling" / "coverage.json").is_file())

    def test_a_measurement_needs_coverage_py(self) -> None:
        def missing(
            command: Sequence[str], cwd: Path, echo: bool
        ) -> subprocess.CompletedProcess[str]:
            return subprocess.CompletedProcess(
                list(command), 1, "No module named coverage"
            )

        with _checkout() as root:
            output = root.parent / "reports"
            with self.assertRaisesRegex(
                scripts_coverage.MeasurementError, "coverage.py is not installed"
            ):
                scripts_coverage.measure(output, root=root, run=missing)
            self.assertFalse(output.exists())

    def test_the_output_holds_only_the_reports_of_an_earlier_run(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "reports"
            _write(
                output,
                {
                    "summary.md": "",
                    "tests.log": "",
                    "measurement.json": "",
                    "host-tooling/html/index.html": "",
                },
            )
            scripts_coverage.prepare(output)
            self.assertEqual(list(output.iterdir()), [])
            _write(output, {"notes.txt": "", "summary.md": ""})
            with self.assertRaisesRegex(
                scripts_coverage.MeasurementError,
                "holds more than the reports of an earlier run: notes.txt;",
            ):
                scripts_coverage.prepare(output)
            self.assertTrue((output / "summary.md").exists())
            scripts_coverage.prepare(output / "new" / "reports")
            self.assertTrue((output / "new" / "reports").is_dir())

    def test_main_reports_what_keeps_the_measurement_from_running(self) -> None:
        errors = io.StringIO()
        with (
            patch.object(
                scripts_coverage,
                "measure",
                side_effect=scripts_coverage.MeasurementError("no tests"),
            ) as measure,
            contextlib.redirect_stderr(errors),
        ):
            self.assertEqual(scripts_coverage.main(["--output", "reports"]), 2)
        measure.assert_called_once_with(Path("reports"))
        self.assertEqual(errors.getvalue(), "error: no tests\n")


class RunCommandTests(unittest.TestCase):
    def test_a_process_that_a_command_leaves_running_does_not_hold_it(self) -> None:
        script = (
            "import subprocess, sys\n"
            "child = subprocess.Popen([sys.executable, '-c', "
            "'import time; time.sleep(60)'])\n"
            "print(child.pid, flush=True)\n"
            "print('to stderr', file=sys.stderr, flush=True)\n"
            "sys.exit(3)\n"
        )
        stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8")
        started = time.monotonic()
        with (
            patch.object(sys, "stdout", stdout),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            result = scripts_coverage.run_command(
                [sys.executable, "-c", script], Path.cwd(), True
            )
        elapsed = time.monotonic() - started
        child = int(result.stdout.splitlines()[0])
        try:
            self.assertEqual(result.returncode, 3)
            self.assertIn("to stderr", result.stdout)
            self.assertLess(elapsed, 30)
            echoed = stdout.buffer.getvalue().decode("utf-8")
            self.assertEqual(echoed, result.stdout)
        finally:
            with contextlib.suppress(OSError):
                os.kill(child, signal.SIGTERM)

    def test_a_command_that_cannot_start_fails_as_in_a_shell(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            missing = str(Path(temporary) / "missing")
            with contextlib.redirect_stderr(io.StringIO()):
                result = scripts_coverage.run_command([missing], Path.cwd(), False)
        self.assertEqual(result.returncode, 127)
        self.assertNotEqual(result.stdout, "")


@unittest.skipUnless(
    importlib.util.find_spec("coverage"), "measures with coverage.py, if installed"
)
class MeasurementTests(unittest.TestCase):
    def test_a_measurement_of_a_sample_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "checkout"
            _write(
                root,
                {
                    scripts_coverage.VALIDATE_NVX: _action(
                        "python3 -m unittest scripts/test_tool.py -v\n"
                    ),
                    "scripts/tool.py": textwrap.dedent(
                        """\
                        def choose(flag):
                            if flag:
                                return "yes"
                            return "no"


                        def child():
                            return "child"


                        if __name__ == "__main__":
                            print(child())
                        """
                    ),
                    "scripts/powershell_testing.py": "SUPPORT = True\n",
                    "scripts/nvx_tools/__init__.py": "",
                    "scripts/nvx_tools/demo_tests.py": textwrap.dedent(
                        """\
                        def scenario():
                            return 1
                        """
                    ),
                    "scripts/test_tool.py": textwrap.dedent(
                        """\
                        import subprocess
                        import sys
                        import unittest
                        from pathlib import Path

                        sys.path.insert(0, str(Path(__file__).parent))
                        import powershell_testing
                        import tool
                        from nvx_tools import demo_tests


                        class ToolTests(unittest.TestCase):
                            def test_choose(self):
                                self.assertTrue(powershell_testing.SUPPORT)
                                self.assertTrue(callable(demo_tests.scenario))
                                self.assertEqual(tool.choose(True), "yes")

                            def test_child(self):
                                completed = subprocess.run(
                                    [sys.executable, str(Path(tool.__file__))],
                                    capture_output=True,
                                    text=True,
                                    check=True,
                                )
                                self.assertEqual(completed.stdout, "child\\n")
                                self.assertEqual(completed.stderr, "")
                        """
                    ),
                },
            )
            output = Path(temporary) / "reports"
            stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8")
            with (
                patch.object(sys, "stdout", stdout),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                status = scripts_coverage.measure(output, root=root)
            echoed = stdout.buffer.getvalue().decode("utf-8")
            self.assertEqual(status, 0, echoed)
            self.assertNotIn("Couldn't use data file", echoed)
            # coverage.py names the modules with the host's separators.
            reports = {
                group: {
                    name.replace("\\", "/"): data
                    for name, data in cast(
                        dict[str, dict[str, Any]],
                        json.loads(
                            (output / group / "coverage.json").read_text(
                                encoding="utf-8"
                            )
                        )["files"],
                    ).items()
                }
                for group in scripts_coverage.GROUPS
            }
            for group in scripts_coverage.GROUPS:
                for name in ("lcov.info", "html/index.html"):
                    self.assertTrue((output / group / name).is_file(), name)
            tool = reports[scripts_coverage.HOST_TOOLING]["scripts/tool.py"]
            # The test reached one branch of choose(), and the process that it
            # started reached child().
            self.assertEqual(tool["missing_lines"], [4])
            self.assertIn(8, tool["executed_lines"])
            self.assertEqual(tool["summary"]["num_branches"], 4)
            self.assertNotIn(
                "scripts/test_tool.py", reports[scripts_coverage.HOST_TOOLING]
            )
            self.assertNotIn(
                "scripts/powershell_testing.py", reports[scripts_coverage.HOST_TOOLING]
            )
            self.assertEqual(
                list(reports[scripts_coverage.SCENARIO_DRIVERS]),
                ["scripts/nvx_tools/demo_tests.py"],
            )
            measurement = json.loads(
                (output / "measurement.json").read_text(encoding="utf-8")
            )
            self.assertEqual(measurement["runs"][0]["outcome"], "2 tests: OK")
            summary = (output / "summary.md").read_text(encoding="utf-8")
            self.assertIn("| `scripts/nvx_tools/demo_tests.py` |", summary)


if __name__ == "__main__":
    unittest.main()

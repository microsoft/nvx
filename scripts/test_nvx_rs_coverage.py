#!/usr/bin/env python3
"""Tests for the nvx-rs coverage workflow and scripts/nvx_rs_coverage.py.

They check the workflow's triggers, the tools that it pins, and the names of its
artifacts, and the script's parts: how it finds the failed tests in Cargo's
output and runs them again, which files it counts as production code, test code,
or test doubles, the module table and the summary, the lines that changes add or
change, from Git's diffs in fixture repositories, and what LCOV reports say of
them, and, with a stub of Cargo, the whole measurement.
"""

from __future__ import annotations

import io
import json
import os
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Mapping, Sequence
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path, PurePosixPath
from typing import Any, cast
from unittest.mock import patch

import yaml

sys.path.insert(0, str(Path(__file__).parent))
import nvx_rs_coverage as coverage  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = "nvx-rs-coverage.yml"
MANIFEST = REPO_ROOT / "scripts" / "setup" / "tool-versions.conf"


def _workflow() -> dict[Any, Any]:
    path = REPO_ROOT / ".github" / "workflows" / WORKFLOW
    return cast(dict[Any, Any], yaml.safe_load(path.read_text(encoding="utf-8")))


def _step(workflow: Mapping[Any, Any], name: str) -> dict[str, Any]:
    steps = cast(list[dict[str, Any]], workflow["jobs"]["measure"]["steps"])
    return next(step for step in steps if step.get("name") == name)


def _manifest_value(key: str) -> str:
    for line in MANIFEST.read_text(encoding="utf-8").splitlines():
        name, separator, value = line.partition("=")
        if separator and name == key:
            return value
    raise AssertionError(f"the tool manifest pins no {key}")


class WorkflowTests(unittest.TestCase):
    def test_pushes_to_dev_that_change_the_measurement_run_it(self):
        workflow = _workflow()
        # YAML 1.1 reads the key `on` as true.
        triggers = cast(dict[str, Any], workflow.get("on", workflow.get(True)))
        # Never for merge groups, so that the merge queue waits for none of
        # its jobs.
        self.assertEqual(set(triggers), {"push", "pull_request", "workflow_dispatch"})
        push = triggers["push"]
        self.assertEqual(push["branches"], ["dev"])
        self.assertEqual(
            push["paths"],
            [
                "nvx-rs/**",
                "rust-toolchain.toml",
                "scripts/nvx_rs_coverage.py",
                "scripts/setup/tool-versions.conf",
                f".github/workflows/{WORKFLOW}",
            ],
        )
        for path in push["paths"]:
            with self.subTest(path=path):
                self.assertTrue((REPO_ROOT / path.removesuffix("/**")).exists())
        self.assertEqual(workflow["permissions"], {"contents": "read"})

    def test_the_workflow_measures_linux_and_windows(self):
        job = _workflow()["jobs"]["measure"]
        self.assertEqual(
            job["strategy"]["matrix"]["os"], ["ubuntu-latest", "windows-latest"]
        )
        self.assertIs(job["strategy"]["fail-fast"], False)
        run = _step(_workflow(), "Measure")["run"]
        self.assertIn("options=(--output build/nvx-rs-coverage)", run)
        self.assertIn('python scripts/nvx_rs_coverage.py "${options[@]}"', run)

    def test_ready_pull_requests_that_change_the_measurement_run_it(self):
        workflow = _workflow()
        triggers = cast(dict[str, Any], workflow.get("on", workflow.get(True)))
        ci = yaml.safe_load(
            (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
        )
        ci_triggers = cast(dict[str, Any], ci.get("on", ci.get(True)))
        pull_request = triggers["pull_request"]
        # As in ci.yml, for any base, so that a stacked pull request lists the
        # lines of its own changes, and once a draft is marked ready.
        self.assertEqual(pull_request["types"], ci_triggers["pull_request"]["types"])
        self.assertIn("ready_for_review", pull_request["types"])
        self.assertNotIn("branches", pull_request)
        self.assertEqual(pull_request["paths"], triggers["push"]["paths"])
        # Drafts skip the measurement, as they skip CI's other jobs.
        job = workflow["jobs"]["measure"]
        self.assertEqual(
            job["if"],
            "github.event_name != 'pull_request' || "
            "github.event.pull_request.draft == false",
        )
        self.assertEqual(job["if"], ci["jobs"]["nvx-rs"]["if"])
        # dev requires only ci.yml's check, which GitHub finds by its name.
        self.assertEqual(
            ci["jobs"]["required-status-check"]["name"], "Required status check"
        )
        self.assertNotIn("Required status check", job["name"])
        # A push to a pull request replaces the measurement of the one before.
        self.assertEqual(
            workflow["concurrency"],
            {
                "group": "${{ github.workflow }}-${{ github.ref }}",
                "cancel-in-progress": True,
            },
        )

    def test_pull_requests_measure_their_head_and_their_changes(self):
        workflow = _workflow()
        checkout = _step(workflow, "Checkout")["with"]
        # A pull request's head, whose lines the summary names, with the
        # history of its merge base; a push's commit, without that history.
        self.assertEqual(checkout["ref"], "${{ github.event.pull_request.head.sha }}")
        self.assertEqual(
            checkout["fetch-depth"],
            "${{ github.event_name == 'pull_request' && '0' || '1' }}",
        )
        self.assertIs(checkout["persist-credentials"], False)
        measure = _step(workflow, "Measure")
        self.assertEqual(
            measure["env"], {"BASE": "${{ github.event.pull_request.base.sha }}"}
        )
        bash = shutil.which("bash")
        if os.name != "posix" or bash is None:
            self.skipTest("runs the workflow's bash step on POSIX hosts")
        with tempfile.TemporaryDirectory() as directory:
            programs = Path(directory)
            arguments = programs / "arguments"
            python = programs / "python"
            python.write_text(
                f'#!/bin/sh\nprintf "%s\\n" "$@" > {shlex.quote(str(arguments))}\n',
                encoding="utf-8",
            )
            python.chmod(0o755)
            command = [
                "scripts/nvx_rs_coverage.py",
                "--output",
                "build/nvx-rs-coverage",
            ]
            base = "0123456789abcdef0123456789abcdef01234567"
            # A push or a dispatch has no base.
            for value, expected in (("", command), (base, [*command, "--base", base])):
                with self.subTest(base=value):
                    result = subprocess.run(
                        [bash, "--noprofile", "--norc", "-eo", "pipefail", "-c"]
                        + [measure["run"]],
                        cwd=REPO_ROOT,
                        env={
                            **os.environ,
                            "BASE": value,
                            "PATH": f"{programs}{os.pathsep}{os.environ['PATH']}",
                        },
                        capture_output=True,
                        text=True,
                        timeout=60,
                        check=False,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(
                        arguments.read_text(encoding="utf-8").splitlines(), expected
                    )

    def test_reruns_replace_the_reports_of_their_system(self):
        workflow = _workflow()
        upload = _step(workflow, "Upload the reports")["with"]
        # Names hold no attempt, so that a rerun of a job replaces the reports
        # of its earlier attempt, and the system tells the jobs apart.
        self.assertEqual(
            upload["name"], "nvx-rs-coverage-${{ runner.os }}-${{ github.run_id }}"
        )
        self.assertIs(upload["overwrite"], True)
        self.assertEqual(upload["path"], "build/nvx-rs-coverage")
        # The reports of a measurement that misses something upload too.
        for name in ("Write the summary", "Upload the reports"):
            with self.subTest(step=name):
                self.assertIn("!cancelled()", _step(workflow, name)["if"])

    def test_the_workflow_installs_the_pinned_tools(self):
        workflow = _workflow()
        install = _step(workflow, "Install Rust, its llvm-tools, and cargo-llvm-cov")
        self.assertIn("--component llvm-tools", install["run"])
        self.assertIn(
            'install --locked cargo-llvm-cov --version "${CARGO_LLVM_COV}"',
            install["run"],
        )
        resolve = _step(workflow, "Resolve the pinned tools")["run"]
        bash = shutil.which("bash")
        if os.name != "posix" or bash is None:
            self.skipTest("runs the workflow's bash step on POSIX hosts")
        with tempfile.TemporaryDirectory() as directory:
            outputs = Path(directory) / "outputs"
            result = subprocess.run(
                [bash, "--noprofile", "--norc", "-eo", "pipefail", "-c", resolve],
                cwd=REPO_ROOT,
                env={**os.environ, "GITHUB_OUTPUT": str(outputs)},
                capture_output=True,
                text=True,
                timeout=60,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            values = outputs.read_text(encoding="utf-8").splitlines()
        toolchain = re.search(
            r'^channel = "([^"]+)"$',
            (REPO_ROOT / "rust-toolchain.toml").read_text(encoding="utf-8"),
            re.MULTILINE,
        )
        assert toolchain is not None
        self.assertEqual(
            values,
            [
                f"toolchain={toolchain.group(1)}",
                f"cargo_llvm_cov={_manifest_value('cargo_llvm_cov.version')}",
            ],
        )


# The output of a cargo test whose library tests failed: a process that a test
# started printed a listing of its own before the harness's.
FAILED_OUTPUT = """\
     Running unittests src/lib.rs (target/llvm-cov-target/debug/deps/nvx-0123)

running 3 tests
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out
test a::first ... ok
test a::second ... FAILED
test a::third ... FAILED

failures:

---- a::second stdout ----
failures:
    child::test

thread 'a::second' panicked at src/a.rs:1:1:

failures:
    a::second
    a::third

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out

error: test failed, to rerun pass `--lib`
     Running tests/store.rs (target/llvm-cov-target/debug/deps/store-4567)

running 2 tests
test kept ... ok
error: test failed, to rerun pass `--test store`

Caused by:
  process didn't exit successfully: `store-4567` (signal: 6, SIGABRT)
     Running tests/cli.rs (target/llvm-cov-target/debug/deps/cli-89ab)

running 1 test
test scenario ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

error: 2 targets failed:
    `--lib`
    `--test store`
"""


def _result(passed: int, failed: Sequence[str] = ()) -> str:
    """Returns the output of a run of one target whose harness passed `passed`
    tests and failed those in `failed`."""
    if not failed:
        return f"test result: ok. {passed} passed; 0 failed; 0 ignored\n"
    names = "".join(f"    {name}\n" for name in failed)
    return (
        f"failures:\n{names}\ntest result: FAILED. {passed} passed; "
        f"{len(failed)} failed; 0 ignored\n\n"
        "error: test failed, to rerun pass `--lib`\n"
    )


class FailedTargetTests(unittest.TestCase):
    def test_the_harness_lists_the_failed_tests_of_each_target(self):
        self.assertEqual(
            coverage.failed_targets(FAILED_OUTPUT),
            [
                coverage.FailedTarget("--lib", ("a::second", "a::third")),
                # A harness that crashed listed none.
                coverage.FailedTarget("--test store", ()),
            ],
        )
        crlf = FAILED_OUTPUT.replace("\n", "\r\n")
        self.assertEqual(
            coverage.failed_targets(crlf), coverage.failed_targets(FAILED_OUTPUT)
        )

    def test_targets_whose_tests_passed_are_not_failed(self):
        self.assertEqual(coverage.failed_targets(_result(3)), [])
        self.assertEqual(coverage.failed_targets(""), [])

    def test_cargo_prints_its_diagnostics_in_plain_text(self):
        # CARGO_TERM_COLOR=always colors them even in a pipe, and then no
        # failed target is found.
        colored = "\x1b[1m\x1b[91merror\x1b[0m: test failed, to rerun pass `--lib`\n"
        self.assertEqual(coverage.failed_targets(colored), [])
        options = list(coverage.RUN_OPTIONS)
        self.assertEqual(options[options.index("--color") + 1], "never")

    def test_the_last_result_is_the_harness_s(self):
        self.assertEqual(coverage.passed_tests(FAILED_OUTPUT), 1)
        self.assertEqual(coverage.passed_tests(_result(9) + _result(2, ["a"])), 2)
        self.assertIsNone(coverage.passed_tests("running 1 test\n"))


class FakeRunner:
    """Answers each command with the next of `results`, (status, output)
    pairs, and records the commands."""

    def __init__(self, *results: tuple[int, str]):
        self.results = list(results)
        self.commands: list[tuple[str, ...]] = []

    def __call__(
        self, command: Sequence[str], echo: bool
    ) -> subprocess.CompletedProcess[str]:
        self.commands.append(tuple(command))
        status, output = self.results.pop(0)
        return subprocess.CompletedProcess(list(command), status, output)


class RetryTests(unittest.TestCase):
    CARGO = ("cargo", "llvm-cov")
    RUN = (*CARGO, "--no-report", *coverage.RUN_OPTIONS)

    def test_failed_tests_run_again_by_name(self):
        target = coverage.FailedTarget("--test cli", ("a", "b"))
        runner = FakeRunner((0, _result(2)))
        retried = coverage.retry(self.CARGO, target, runner)
        self.assertEqual(
            runner.commands,
            [(*self.RUN, "--test", "cli", "--", "--exact", "a", "b")],
        )
        self.assertEqual(retried, coverage.Retry("--test cli", ("a", "b"), (), 0))
        self.assertTrue(retried.passed())

    def test_tests_that_fail_again_are_told_apart(self):
        target = coverage.FailedTarget("--lib", ("a", "b"))
        retried = coverage.retry(
            self.CARGO, target, FakeRunner((101, _result(1, ["b"])))
        )
        self.assertEqual(retried, coverage.Retry("--lib", ("a", "b"), ("b",), 101))
        self.assertFalse(retried.passed())

    def test_tests_that_the_harness_did_not_run_did_not_pass(self):
        target = coverage.FailedTarget("--lib", ("a", "gone"))
        for result in (
            (0, _result(1)),
            (101, "error: test failed, to rerun pass `--lib`\n"),
        ):
            with self.subTest(result=result):
                retried = coverage.retry(self.CARGO, target, FakeRunner(result))
                self.assertEqual(retried.failed, ("a", "gone"))

    def test_targets_without_names_run_again_whole(self):
        long = coverage.FailedTarget("--lib", ("x" * coverage.RETRY_NAMES_LIMIT, "y"))
        for target in (coverage.FailedTarget("--test store", ()), long):
            for status in (0, 101):
                with self.subTest(target=target.arguments, status=status):
                    runner = FakeRunner((status, ""))
                    retried = coverage.retry(self.CARGO, target, runner)
                    self.assertEqual(
                        runner.commands, [(*self.RUN, *target.arguments.split())]
                    )
                    self.assertEqual(
                        retried, coverage.Retry(target.arguments, (), (), status)
                    )
                    self.assertEqual(retried.passed(), status == 0)


class ClassificationTests(unittest.TestCase):
    def test_tests_directories_and_files_hold_test_code(self):
        for path, expected in (
            ("tests/cli.rs", True),
            ("tests/nvxs/main.rs", True),
            ("src/host/sandbox/tests.rs", True),
            ("src/host/process/tests/linux.rs", True),
            ("src\\host\\process\\tests\\linux.rs", True),
            # A unit test includes a vector through a path that keeps `/` on
            # Windows, which cargo-llvm-cov counted as production code there.
            (
                "src\\host\\template\\plan\\../../../../tests/vectors/openvmm/"
                "platform_command_line_token_is_invariant.rs",
                True,
            ),
            ("src/guest/time/test_mode.rs", False),
            ("src/host/contests.rs", False),
            ("src/testing/mod.rs", False),
            ("src/lib.rs", False),
        ):
            with self.subTest(path=path):
                self.assertIs(coverage.is_test_code(path), expected)

    def test_the_fake_openvmm_is_the_test_double(self):
        for path, expected in (
            ("src/testing/mod.rs", True),
            ("src\\testing\\fake_openvmm.rs", True),
            ("src/testing.rs", True),
            ("src/bin/nvx-fake-openvmm.rs", True),
            ("src/bin/nvx-rs.rs", False),
            ("src/host/testing.rs", False),
            ("src/testing_helpers.rs", False),
        ):
            with self.subTest(path=path):
                self.assertIs(coverage.is_test_double(path), expected)

    def test_rows_count_top_level_modules_and_submodules_with_submodules(self):
        paths = [
            "src/lib.rs",
            "src/uuid.rs",
            "src/bin/nvx-rs.rs",
            "src/activation/mod.rs",
            "src/activation/codec.rs",
            "src/host/mod.rs",
            "src/host/seal.rs",
            "src/host/cli.rs",
            "src/host/cli/run.rs",
            "src/host/cli/sandbox/mod.rs",
            "src/guest/time/daemon.rs",
        ]
        parents = coverage.parent_modules(paths)
        self.assertEqual(parents, {"src/host/cli", "src/guest/time"})
        self.assertEqual(
            [coverage.module_of(path, parents) for path in paths],
            [
                "src/lib",
                "src/uuid",
                "src/bin",
                "src/activation",
                "src/activation",
                "src/host",
                "src/host",
                "src/host/cli",
                "src/host/cli",
                "src/host/cli",
                "src/guest/time",
            ],
        )
        with self.assertRaises(coverage.MeasurementError):
            coverage.module_of("/home/u/.cargo/registry/x.rs", parents)


def _summary(
    covered: int, count: int, branches: tuple[int, int] = (0, 0)
) -> dict[str, Any]:
    """Returns llvm-cov's summary of a file whose lines, functions, and regions
    are `count`, `covered` of which the tests reached, with `branches`."""
    entry = {"count": count, "covered": covered}
    return {
        "lines": entry,
        "functions": entry,
        "regions": {**entry, "notcovered": count - covered},
        "branches": {"covered": branches[0], "count": branches[1]},
        "instantiations": entry,
    }


def _report(files: Mapping[str, dict[str, Any]]) -> dict[str, Any]:
    """Returns llvm-cov's JSON summary export of `files`."""
    return {
        "type": "llvm.coverage.json.export",
        "version": "2.0.1",
        "data": [
            {
                "files": [
                    {"filename": name, "summary": summary}
                    for name, summary in files.items()
                ]
            }
        ],
    }


# A Windows report, with the test vector that a unit test includes.
REPORT = _report(
    {
        "src\\lib.rs": _summary(1, 1),
        "src\\host\\cli.rs": _summary(5, 10),
        "src\\host\\cli\\run.rs": _summary(10, 10),
        "src\\host\\seal.rs": _summary(3, 4),
        "src\\bin\\nvx-rs.rs": _summary(5, 6),
        "src\\bin\\nvx-exit.rs": _summary(0, 3),
        "src\\bin\\nvx-fake-openvmm.rs": _summary(3, 3),
        "src\\testing\\fake_openvmm.rs": _summary(90, 100),
        "src\\host\\template\\plan\\../../../../tests/vectors/openvmm/x.rs": _summary(
            27, 27
        ),
    }
)


class AnalysisTests(unittest.TestCase):
    def test_production_code_leaves_out_tests_and_test_doubles(self):
        analysis = coverage.analyze(REPORT)
        lines = {
            "production": analysis.production["lines"],
            "test doubles": analysis.test_doubles["lines"],
        }
        self.assertEqual(
            lines,
            {
                "production": coverage.Count(24, 34),
                "test doubles": coverage.Count(93, 103),
            },
        )
        self.assertEqual(
            {name: counts["lines"] for name, counts in analysis.modules.items()},
            {
                "src/lib": coverage.Count(1, 1),
                "src/host": coverage.Count(3, 4),
                "src/host/cli": coverage.Count(15, 20),
                "src/bin": coverage.Count(5, 9),
            },
        )
        # A binary whose file the tests reached had processes that wrote
        # profiles, as the fake OpenVMM's did.
        self.assertEqual(
            analysis.binaries,
            {"nvx-exit": False, "nvx-fake-openvmm": True, "nvx-rs": True},
        )

    def test_malformed_reports_fail(self):
        reports: list[dict[str, Any]] = [
            {},
            {"data": []},
            {"data": [{"files": [{"filename": 1, "summary": {}}]}]},
            _report({"src/lib.rs": {"lines": {"count": "1", "covered": 1}}}),
        ]
        for report in reports:
            with self.subTest(report=report):
                with self.assertRaises(coverage.MeasurementError):
                    coverage.analyze(report)


def _measurement(**changes: Any) -> dict[str, Any]:
    return {
        "format": coverage.MEASUREMENT_FORMAT,
        "host": "Windows",
        "commit": "0123456789abcdef0123456789abcdef01234567",
        "rustc": "rustc 1.0.0 (0 1970-01-01)",
        "cargo_llvm_cov": "cargo-llvm-cov 1.0.0",
        "seconds": 1.0,
        "status": 0,
        "retries": [],
        "stray_profiles": [],
        **changes,
    }


class SummaryTests(unittest.TestCase):
    def test_summaries_give_totals_and_a_row_for_each_module(self):
        summary = coverage.render(_measurement(), coverage.analyze(REPORT))
        self.assertTrue(summary.startswith("## nvx-rs coverage on Windows\n"))
        self.assertIn("`0123456789ab`", summary)
        self.assertIn("rustc 1.0.0 (0 1970-01-01)", summary)
        self.assertIn("cargo-llvm-cov 1.0.0", summary)
        # The stable compiler measures no branches.
        self.assertIn(
            "| Code | Lines | Functions | Regions |\n"
            "| --- | ---: | ---: | ---: |\n"
            "| Production | 70.5% of 34 | 70.5% of 34 | 70.5% of 34 |\n"
            "| Test doubles | 90.2% of 103 | 90.2% of 103 | 90.2% of 103 |\n",
            summary,
        )
        self.assertIn(
            "The profiles include processes of `nvx-fake-openvmm` and `nvx-rs`. "
            "No process of `nvx-exit` wrote one.\n",
            summary,
        )
        self.assertIn(
            "| `src/bin` | 55.5% of 9 | 55.5% of 9 | 55.5% of 9 |\n"
            "| `src/host` | 75.0% of 4 | 75.0% of 4 | 75.0% of 4 |\n"
            "| `src/host/cli` | 75.0% of 20 | 75.0% of 20 | 75.0% of 20 |\n"
            "| `src/lib` | 100.0% of 1 | 100.0% of 1 | 100.0% of 1 |\n",
            summary,
        )
        self.assertNotIn("misses", summary)

    def test_shares_round_down_and_branches_show_when_measured(self):
        report = _report({"src/lib.rs": _summary(1999, 2000, (1, 3))})
        summary = coverage.render(_measurement(), coverage.analyze(report))
        self.assertIn("| Code | Lines | Functions | Regions | Branches |\n", summary)
        self.assertIn(
            "| Production | 99.9% of 2,000 | 99.9% of 2,000 | 99.9% of 2,000 "
            "| 33.3% of 3 |\n",
            summary,
        )
        self.assertIn("| Test doubles | none | none | none | none |\n", summary)

    def test_summaries_name_the_tests_that_passed_only_when_they_ran_again(self):
        measurement = _measurement(
            retries=[
                coverage.Retry("--lib", ("a", "b"), (), 0).to_json(),
                coverage.Retry("--test cli", ("c",), (), 0).to_json(),
                coverage.Retry("--test store", (), (), 0).to_json(),
            ]
        )
        summary = coverage.render(measurement, coverage.analyze(REPORT))
        self.assertIn(
            "The tests `a` and `b` of `--lib` passed only when they ran again.\n\n"
            "The tests `c` of `--test cli` passed only when they ran again.\n\n"
            "The tests of `--test store` passed only when they ran again.\n",
            summary,
        )
        self.assertEqual(coverage.problems(measurement, coverage.analyze(REPORT)), [])

    def test_summaries_name_what_the_measurement_misses(self):
        measurement = _measurement(
            retries=[
                coverage.Retry("--lib", ("a", "b", "c"), ("b", "c"), 101).to_json(),
                coverage.Retry("--test store", (), (), 101).to_json(),
            ],
            stray_profiles=["default_1_0_2.profraw"],
        )
        report = _report({"src/bin/nvx-rs.rs": _summary(0, 6)})
        found = coverage.problems(measurement, coverage.analyze(report))
        self.assertEqual(
            found,
            [
                "The tests `b` and `c` of `--lib` failed when they ran again.",
                "The tests of `--test store` failed when they ran again.",
                "No process of `nvx-rs` and `nvx-fake-openvmm` wrote a profile, "
                "though the tests start them.",
                "1 processes wrote their profiles into the crate's directory, as "
                "processes that a test starts there without `LLVM_PROFILE_FILE` "
                "do: `default_1_0_2.profraw`.",
            ],
        )
        summary = coverage.render(measurement, coverage.analyze(report))
        self.assertIn(
            "The tests `a` of `--lib` passed only when they ran again.\n", summary
        )
        self.assertIn(
            "The measurement misses what these reached:\n\n"
            + "".join(f"- {problem}\n" for problem in found),
            summary,
        )


class DiffTests(unittest.TestCase):
    def test_hunks_hold_the_lines_that_their_headers_count(self):
        diff = (
            "diff --git a/nvx-rs/src/a.rs b/nvx-rs/src/a.rs\n"
            "index 0123456..89abcde 100644\n"
            "--- a/nvx-rs/src/a.rs\n"
            "+++ b/nvx-rs/src/a.rs\n"
            "@@ -1,0 +2,2 @@ fn a() {\n"
            # Added lines that look like a header and a hunk.
            "+++ b/nvx-rs/src/b.rs\n"
            "+@@ -1 +1 @@\n"
            "@@ -5 +7 @@ fn b() {\n"
            "-old\n"
            "\\ No newline at end of file\n"
            "+new\n"
            "\\ No newline at end of file\n"
            "diff --git a/nvx-rs/src/gone.rs b/nvx-rs/src/gone.rs\n"
            "deleted file mode 100644\n"
            "--- a/nvx-rs/src/gone.rs\n"
            "+++ /dev/null\n"
            "@@ -1,2 +0,0 @@\n"
            "-a\n"
            "-b\n"
        )
        self.assertEqual(coverage.added_lines(diff), {"nvx-rs/src/a.rs": {2, 3, 7}})
        self.assertEqual(coverage.added_lines(""), {})

    def test_headers_name_files_as_git_quotes_them(self):
        for name, path in (
            ('"b/src/caf\\303\\251.rs"', "src/caf\u00e9.rs"),
            ('"b/src/a\\tb\\\\c\\"d.rs"', 'src/a\tb\\c"d.rs'),
            # Git ends a name that holds a space with a tab.
            ("b/src/a b.rs\t", "src/a b.rs"),
            ("b/src/caf\u00e9.rs", "src/caf\u00e9.rs"),
        ):
            with self.subTest(name=name):
                diff = f"+++ {name}\n@@ -0,0 +1 @@\n+a\n"
                self.assertEqual(coverage.added_lines(diff), {path: {1}})

    def test_malformed_diffs_fail(self):
        for diff in (
            "+++ b/a.rs\n@@ -0,0 +1,2 @@\n+a\n",
            "+++ b/a.rs\n@@ -0,0 +1 @@\n a\n",
            "+++ b/a.rs\n@@ -1 +0,0 @@\n+a\n",
            "@@ -0,0 +1 @@\n+a\n",
            "+++ a.rs\n",
            '+++ "b/a\\q.rs"\n',
            '+++ "b/a\\8.rs"\n',
        ):
            with self.subTest(diff=diff):
                with self.assertRaises(coverage.MeasurementError):
                    coverage.added_lines(diff)


def _numbered(name: str, count: int) -> str:
    return "".join(f"{name} {number}\n" for number in range(1, count + 1))


class Repository:
    """A Git repository in a temporary directory, whose Git commands read the
    configuration in `configuration`, empty at first, rather than the
    developer's, such as commit signing."""

    def __init__(self, test: unittest.TestCase):
        temporary = tempfile.TemporaryDirectory()
        test.addCleanup(temporary.cleanup)
        directory = Path(temporary.name)
        self.root = directory / "repository"
        self.root.mkdir()
        self.crate = self.root / "nvx-rs"
        self.configuration = directory / "gitconfig"
        self.configuration.write_bytes(b"")
        environment = patch.dict(
            os.environ,
            {
                "GIT_CEILING_DIRECTORIES": str(directory),
                "GIT_CONFIG_GLOBAL": str(self.configuration),
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_AUTHOR_NAME": "NVX",
                "GIT_AUTHOR_EMAIL": "nvx@example.com",
                "GIT_COMMITTER_NAME": "NVX",
                "GIT_COMMITTER_EMAIL": "nvx@example.com",
            },
        )
        environment.start()
        test.addCleanup(environment.stop)
        self.git("init", "--quiet", "--initial-branch=dev")

    def git(self, *arguments: str) -> str:
        return subprocess.run(
            ["git", *arguments],
            cwd=self.root,
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=True,
        ).stdout

    def write(self, files: Mapping[str, str | None]) -> None:
        """Writes `files` with LF line endings, or deletes those without
        text."""
        for name, text in files.items():
            path = self.root / name
            if text is None:
                path.unlink()
            else:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(text.encode("utf-8"))

    def commit(self, files: Mapping[str, str | None]) -> str:
        """Commits `files`, as write() writes them, and returns the commit."""
        self.write(files)
        self.git("add", "--all")
        self.git("commit", "--quiet", "--message", "change")
        return self.git("rev-parse", "HEAD").strip()


@unittest.skipIf(shutil.which("git") is None, "requires git")
class ChangesTests(unittest.TestCase):
    def test_changes_hold_the_lines_that_the_working_tree_adds_or_changes(self):
        repository = Repository(self)
        lib = _numbered("line", 10)
        base = repository.commit(
            {
                "nvx-rs/src/lib.rs": lib,
                "nvx-rs/src/old.rs": _numbered("old", 12),
                "nvx-rs/src/same.rs": _numbered("same", 12),
                "nvx-rs/src/gone.rs": _numbered("gone", 5),
                "nvx-rs/src/a b.rs": "a\nb\n",
                "nvx-rs/src/caf\u00e9.rs": "a\nb\n",
                "nvx-rs/tests/cli.rs": "a\n",
                "scripts/tool.py": "a\n",
            }
        )
        repository.git("switch", "--quiet", "--create", "topic")
        repository.commit(
            {
                # Changes line 2, deletes line 5, and adds two after line 8.
                "nvx-rs/src/lib.rs": lib.replace("line 2\n", "line two\n")
                .replace("line 5\n", "")
                .replace("line 8\n", "line 8\nnew a\nnew b\n"),
                # A renamed file has only the lines that its changes add.
                "nvx-rs/src/old.rs": None,
                "nvx-rs/src/new.rs": _numbered("old", 12).replace("old 7", "old 07"),
                "nvx-rs/src/same.rs": None,
                "nvx-rs/src/moved.rs": _numbered("same", 12),
                "nvx-rs/src/gone.rs": None,
                "nvx-rs/src/added.rs": "a\nb\n",
                "nvx-rs/src/a b.rs": "a\nB\n",
                "nvx-rs/src/caf\u00e9.rs": "a\nB\n",
                "nvx-rs/tests/cli.rs": "A\n",
                "scripts/tool.py": "A\n",
            }
        )
        # What dev changes after the merge base is none of the changes.
        repository.git("switch", "--quiet", "dev")
        repository.commit({"nvx-rs/src/lib.rs": lib.replace("line 1\n", "line one\n")})
        repository.git("switch", "--quiet", "topic")
        # The working tree's changes count, but untracked files don't.
        repository.write(
            {"nvx-rs/src/added.rs": "a\nb\nc\n", "nvx-rs/src/untracked.rs": "a\n"}
        )
        expected = coverage.Changes(
            base,
            {
                "src/lib.rs": frozenset({2, 8, 9}),
                "src/new.rs": frozenset({7}),
                "src/added.rs": frozenset({1, 2, 3}),
                "src/a b.rs": frozenset({2}),
                "src/caf\u00e9.rs": frozenset({2}),
                "tests/cli.rs": frozenset({1}),
            },
        )
        self.assertEqual(
            coverage.changes_since("dev", repository.root, repository.crate), expected
        )
        # Whatever the developer's configuration says of diffs.
        repository.configuration.write_text(
            "[diff]\n"
            "\tnoprefix = true\n"
            "\tmnemonicPrefix = true\n"
            "\trenames = false\n"
            "\tcontext = 5\n"
            "\texternal = nvx-missing-diff-driver\n"
            "[color]\n"
            "\tui = always\n"
            "[core]\n"
            "\tquotePath = false\n",
            encoding="utf-8",
        )
        self.assertEqual(
            coverage.changes_since("dev", repository.root, repository.crate), expected
        )

    def test_bases_without_a_merge_base_fail(self):
        repository = Repository(self)
        repository.commit({"nvx-rs/src/lib.rs": "a\n"})
        repository.git("switch", "--quiet", "--orphan", "other")
        repository.commit({"nvx-rs/src/lib.rs": "b\n"})
        for base, message in (
            ("dev", "git merge-base dev HEAD failed with status 1"),
            ("missing", "git merge-base missing HEAD failed with status 128: .+"),
        ):
            with self.subTest(base=base):
                with self.assertRaisesRegex(coverage.MeasurementError, message):
                    coverage.changes_since(base, repository.root, repository.crate)

    def test_changes_find_the_lines_that_they_add_and_no_test_reaches(self):
        repository = Repository(self)
        repository.commit({"nvx-rs/src/lib.rs": "fn a() {\n    one();\n}\n"})
        repository.git("switch", "--quiet", "--create", "topic")
        repository.commit(
            {"nvx-rs/src/lib.rs": "fn a() {\n    one();\n    // Two.\n    two();\n}\n"}
        )
        changes = coverage.changes_since("dev", repository.root, repository.crate)
        self.assertEqual(changes.lines, {"src/lib.rs": frozenset({3, 4})})
        # llvm-cov instruments no comment.
        for hits, share, listed in (
            (0, "0.0% of 1", True),
            (2, "100.0% of 1", False),
        ):
            with self.subTest(hits=hits):
                report = coverage.read_lcov(
                    f"SF:src/lib.rs\nDA:1,1\nDA:2,1\nDA:4,{hits}\nDA:5,1\n"
                    "end_of_record\n"
                )
                summary = "\n".join(
                    coverage.render_changes(coverage.changed_coverage(changes, report))
                )
                self.assertIn(f"Tests reach {share} instrumented lines", summary)
                self.assertEqual("| `src/lib.rs` | 4 |" in summary, listed)


class LcovTests(unittest.TestCase):
    def test_reports_give_the_hits_and_branches_of_each_line(self):
        report = coverage.read_lcov(
            "TN:\n"
            "SF:src\\host\\cli.rs\n"
            "FN:3,_RNvNtCs0_3nvx3cli\n"
            "FNDA:1,_RNvNtCs0_3nvx3cli\n"
            "DA:3,1\n"
            "DA:4,0\n"
            "DA:7,12,c2hlY2tzdW0\n"
            "BRDA:4,0,0,-\n"
            "BRDA:4,0,1,-\n"
            "BRDA:7,0,0,12\n"
            "BRDA:7,0,1,0\n"
            "BRF:4\n"
            "BRH:1\n"
            "LF:3\n"
            "LH:2\n"
            "end_of_record\n"
            "SF:src/lib.rs\n"
            "DA:1,0\n"
            "end_of_record\n"
            # Another record of a file adds to the first.
            "SF:src/lib.rs\n"
            "DA:1,2\n"
            "end_of_record\n"
        )
        self.assertEqual(
            report,
            {
                "src/host/cli.rs": coverage.FileLines(
                    {3: 1, 4: 0, 7: 12},
                    {
                        4: {("0", "0"): False, ("0", "1"): False},
                        7: {("0", "0"): True, ("0", "1"): False},
                    },
                ),
                "src/lib.rs": coverage.FileLines({1: 2}, {}),
            },
        )
        self.assertEqual(coverage.read_lcov(""), {})

    def test_malformed_reports_fail(self):
        for text in (
            "DA:1,1\n",
            "SF:src/lib.rs\nend_of_record\nBRDA:1,0,0,1\n",
            "SF:src/lib.rs\nDA:x,1\n",
            "SF:src/lib.rs\nDA:1\n",
            "SF:src/lib.rs\nBRDA:1,0,0\n",
            "SF:src/lib.rs\nBRDA:1,0,0,x\n",
        ):
            with self.subTest(text=text):
                with self.assertRaises(coverage.MeasurementError):
                    coverage.read_lcov(text)


MERGE_BASE = "0123456789abcdef0123456789abcdef01234567"


class ChangedCoverageTests(unittest.TestCase):
    def test_changes_count_the_lines_that_the_report_instruments(self):
        report = coverage.read_lcov(
            "SF:src/lib.rs\nDA:2,1\nDA:3,0\nDA:4,0\nDA:9,5\nend_of_record\n"
            "SF:src/host/windows.rs\nDA:1,0\nend_of_record\n"
        )
        changes = coverage.Changes(
            MERGE_BASE,
            {
                # Line 1 is a comment.
                "src/lib.rs": frozenset({1, 2, 3, 4}),
                # Code that this host doesn't compile, and test code.
                "src/host/linux.rs": frozenset({5}),
                "tests/cli.rs": frozenset({1}),
                # Lines of code that tests did not reach, but none changed.
                "src/host/windows.rs": frozenset({2}),
            },
        )
        changed = coverage.changed_coverage(changes, report)
        self.assertEqual(
            changed,
            coverage.ChangedCoverage(
                MERGE_BASE,
                (coverage.ChangedFile("src/lib.rs", coverage.Count(1, 3), (3, 4), ()),),
                False,
            ),
        )
        self.assertEqual(changed.lines(), coverage.Count(1, 3))

    def test_branches_that_no_test_takes_count_on_every_changed_line(self):
        report = coverage.read_lcov(
            "SF:src/lib.rs\n"
            "DA:1,1\nBRDA:1,0,0,1\nBRDA:1,0,1,0\n"
            "DA:2,0\nBRDA:2,0,0,-\nBRDA:2,0,1,-\n"
            "DA:3,4\nBRDA:3,0,0,2\nBRDA:3,0,1,2\n"
            "DA:4,1\n"
            "DA:5,0\nBRDA:5,0,0,-\nBRDA:5,0,1,-\n"
            "end_of_record\n"
        )
        changes = coverage.Changes(MERGE_BASE, {"src/lib.rs": frozenset({1, 2, 3, 4})})
        changed = coverage.changed_coverage(changes, report)
        self.assertEqual(
            changed.files,
            (
                coverage.ChangedFile(
                    "src/lib.rs",
                    coverage.Count(3, 4),
                    (2,),
                    ((1, coverage.Count(1, 2)), (2, coverage.Count(0, 2))),
                ),
            ),
        )
        self.assertIs(changed.branches, True)


class ChangedSummaryTests(unittest.TestCase):
    def test_summaries_list_by_file_the_changed_lines_that_no_test_reaches(self):
        changed = coverage.ChangedCoverage(
            MERGE_BASE,
            (
                coverage.ChangedFile("src/a.rs", coverage.Count(2, 2), (), ()),
                coverage.ChangedFile(
                    "src/b.rs", coverage.Count(1, 7), (3, 4, 5, 9, 11, 12), ()
                ),
            ),
            False,
        )
        self.assertEqual(
            "\n".join(coverage.render_changes(changed)),
            "### Changed lines that no test reaches\n"
            "\n"
            "Tests reach 33.3% of 9 instrumented lines of production code that the "
            "changes since `0123456789ab` add or change.\n"
            "\n"
            "By file, the lines that no test reaches:\n"
            "\n"
            "| File | Lines |\n"
            "| --- | --- |\n"
            "| `src/b.rs` | 3-5, 9, 11-12 |\n",
        )

    def test_changes_that_tests_reach_entirely_list_no_line(self):
        changed = coverage.ChangedCoverage(
            MERGE_BASE,
            (coverage.ChangedFile("src/a.rs", coverage.Count(2, 2), (), ()),),
            False,
        )
        self.assertEqual(
            coverage.render_changes(changed),
            [
                "### Changed lines that no test reaches",
                "",
                "Tests reach 100.0% of 2 instrumented lines of production code "
                "that the changes since `0123456789ab` add or change.",
                "",
            ],
        )

    def test_changes_without_instrumented_lines_say_so(self):
        changed = coverage.ChangedCoverage(MERGE_BASE, (), True)
        self.assertEqual(
            coverage.render_changes(changed),
            [
                "### Changed lines that no test reaches",
                "",
                "The changes since `0123456789ab` add or change no instrumented "
                "line of production code.",
                "",
            ],
        )

    def test_branches_that_no_test_takes_show_when_measured(self):
        changed = coverage.ChangedCoverage(
            MERGE_BASE,
            (
                coverage.ChangedFile(
                    "src/a.rs",
                    coverage.Count(3, 4),
                    (2,),
                    (
                        (1, coverage.Count(1, 2)),
                        (2, coverage.Count(0, 2)),
                        (5, coverage.Count(1, 4)),
                    ),
                ),
                coverage.ChangedFile(
                    "src/b.rs",
                    coverage.Count(1, 1),
                    (),
                    ((3, coverage.Count(1, 2)),),
                ),
            ),
            True,
        )
        summary = "\n".join(coverage.render_changes(changed))
        self.assertIn(
            "By file, the lines that no test reaches, and the changed lines with "
            "branches that no test takes, with how many of the line's branches no "
            "test takes:\n"
            "\n"
            "| File | Lines that no test reaches | Branches that no test takes |\n"
            "| --- | --- | --- |\n"
            "| `src/a.rs` | 2 | 1 (1 of 2), 2 (2 of 2), 5 (3 of 4) |\n"
            "| `src/b.rs` | none | 3 (1 of 2) |\n",
            summary,
        )

    def test_the_changes_follow_the_totals_and_precede_the_modules(self):
        changed = coverage.ChangedCoverage(MERGE_BASE, (), False)
        summary = coverage.render(_measurement(), coverage.analyze(REPORT), changed)
        section = summary.index("### Changed lines that no test reaches\n")
        self.assertLess(summary.index("| Production |"), section)
        self.assertLess(section, summary.index("### Production code by module\n"))
        self.assertNotIn(
            "### Changed lines",
            coverage.render(_measurement(), coverage.analyze(REPORT)),
        )


class ProfileTests(unittest.TestCase):
    def test_the_measurement_runs_with_cargo_s_instrumented_guest_binaries(self):
        result = coverage.environment({"PATH": "/bin", "NVX_GUEST_BINARIES": "/b"})
        self.assertEqual(result, {"PATH": "/bin"})

    @unittest.skipIf(os.name == "nt", "Windows has no owner permission bits")
    def test_profiles_that_tests_leave_unreadable_become_readable(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, mode in (
                ("a.profraw", 0o200),
                ("b.profraw", 0o644),
                ("c.txt", 0),
            ):
                (root / name).write_bytes(b"")
                (root / name).chmod(mode)
            self.assertEqual(coverage.make_readable(root), 1)
            modes = {
                path.name: stat.S_IMODE(path.stat().st_mode) for path in root.iterdir()
            }
            (root / "c.txt").chmod(0o600)
        self.assertEqual(modes, {"a.profraw": 0o600, "b.profraw": 0o644, "c.txt": 0})

    def test_default_profiles_in_the_crate_are_strays(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("default_1_0_2.profraw", "nvx-1-2.profraw", "default.txt"):
                (root / name).write_bytes(b"")
            self.assertEqual(coverage.stray_profiles(root), {"default_1_0_2.profraw"})


PASSED_OUTPUT = (
    "     Running unittests src/lib.rs (deps/nvx-0123)\n" + _result(3) + "\n"
)


class FakeCargo:
    """Runs the commands of a measurement as cargo-llvm-cov would, without
    building, in `root`, which holds the crate and its target directory.

    Each run of tests answers with the next of `runs`, (status, output)
    pairs, and leaves a profile, which only its owner can write if
    `unreadable`; a report fails while such a profile is there, as
    llvm-profdata does, and writes `report` as its JSON summary. The first run
    also leaves the profiles named in `strays` in the crate.
    """

    def __init__(
        self,
        root: Path,
        *runs: tuple[int, str],
        report: dict[str, Any] | None = None,
        unreadable: bool = False,
        strays: Sequence[str] = (),
    ):
        self.root = root
        self.crate = root / "crate"
        self.crate.mkdir()
        self.profiles = root / "target" / "llvm-cov-target"
        self.runs = list(runs)
        self.report = REPORT if report is None else report
        self.unreadable = unreadable
        self.strays = strays
        self.commands: list[tuple[str, ...]] = []

    def __call__(
        self, command: Sequence[str], echo: bool
    ) -> subprocess.CompletedProcess[str]:
        self.commands.append(tuple(command))
        status, output = 0, ""
        if command[0] == "git":
            output = "0123456789abcdef0123456789abcdef01234567\n"
        elif command[0] == "rustc":
            output = "rustc 1.0.0 (0 1970-01-01)\n"
        elif "metadata" in command:
            output = json.dumps({"target_directory": str(self.root / "target")})
        elif "--version" in command:
            output = "cargo-llvm-cov 1.0.0\n"
        elif "report" in command:
            if any(
                path.stat().st_mode & 0o600 != 0o600
                for path in self.profiles.glob("*.profraw")
            ):
                status = 1
            elif "--html" in command:
                html = Path(command[command.index("--output-dir") + 1]) / "html"
                html.mkdir(parents=True, exist_ok=True)
                (html / "index.html").write_text("<html/>", encoding="utf-8")
            else:
                text = (
                    json.dumps(self.report)
                    if "--json" in command
                    else "end_of_record\n"
                )
                path = Path(command[command.index("--output-path") + 1])
                path.write_text(text, encoding="utf-8")
        elif "--no-report" in command:
            status, output = self.runs.pop(0)
            self.profiles.mkdir(parents=True, exist_ok=True)
            profile = self.profiles / f"nvx-{len(self.commands)}.profraw"
            profile.write_bytes(b"profile")
            if self.unreadable:
                profile.chmod(0o200)
            for stray in self.strays:
                (self.crate / stray).write_bytes(b"profile")
            self.strays = ()
        return subprocess.CompletedProcess(list(command), status, output)


class LcovCargo(FakeCargo):
    """A FakeCargo whose LCOV reports hold `lcov`."""

    def __init__(self, root: Path, *runs: tuple[int, str], lcov: str):
        super().__init__(root, *runs)
        self.lcov = lcov

    def __call__(
        self, command: Sequence[str], echo: bool
    ) -> subprocess.CompletedProcess[str]:
        result = super().__call__(command, echo)
        if "report" in command and "--lcov" in command:
            path = Path(command[command.index("--output-path") + 1])
            path.write_text(self.lcov, encoding="utf-8")
        return result


class MeasureTests(unittest.TestCase):
    def _measure(self, cargo: FakeCargo) -> tuple[int, str]:
        """Measures with `cargo` into its root's out/, and returns the status
        and what the measurement printed on standard error."""
        stderr = io.StringIO()
        with (
            redirect_stderr(stderr),
            redirect_stdout(io.StringIO()),
            patch.dict("os.environ", {"CARGO_LLVM_COV_TARGET_DIR": ""}),
        ):
            status = coverage.measure(cargo.root / "out", run=cargo, crate=cargo.crate)
        return status, stderr.getvalue()

    def _outputs(self, cargo: FakeCargo) -> tuple[dict[str, Any], str, str]:
        output = cargo.root / "out"
        measurement = json.loads(
            (output / coverage.MEASUREMENT_FILE).read_text(encoding="utf-8")
        )
        return (
            measurement,
            (output / coverage.SUMMARY_FILE).read_text(encoding="utf-8"),
            (output / coverage.LOG_FILE).read_text(encoding="utf-8"),
        )

    def test_measurements_run_the_tests_once_and_report_them(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(Path(directory), (0, PASSED_OUTPUT))
            self.assertEqual(self._measure(cargo), (0, ""))
            measurement, summary, log = self._outputs(cargo)
            # The measurement resolves its output directory, as it does a
            # temporary directory's short name on Windows.
            output = (cargo.root / "out").resolve()
            self.assertTrue((output / coverage.LCOV_FILE).is_file())
            self.assertTrue((output / coverage.HTML_DIRECTORY / "index.html").is_file())
        prefix = ("cargo", "llvm-cov")
        report = (
            *prefix,
            "report",
            *coverage.REPORT_OPTIONS,
            "--ignore-filename-regex",
        )
        production = f"{coverage.TEST_CODE}|{coverage.TEST_DOUBLES}"
        self.assertEqual(
            cargo.commands[:7],
            [
                (*prefix, "clean", "--workspace"),
                ("cargo", "metadata", "--format-version", "1", "--no-deps"),
                (*prefix, "--no-report", *coverage.RUN_OPTIONS),
                (
                    *report,
                    coverage.TEST_CODE,
                    "--json",
                    "--summary-only",
                    "--output-path",
                )
                + (str(output / coverage.COVERAGE_FILE),),
                (*report, production, "--lcov", "--output-path")
                + (str(output / coverage.LCOV_FILE),),
                (*report, production, "--html", "--output-dir", str(output)),
                ("git", "-C", str(coverage.REPO_ROOT), "rev-parse", "HEAD"),
            ],
        )
        self.assertEqual(
            {key: measurement[key] for key in ("commit", "rustc", "cargo_llvm_cov")},
            {
                "commit": "0123456789abcdef0123456789abcdef01234567",
                "rustc": "rustc 1.0.0 (0 1970-01-01)",
                "cargo_llvm_cov": "cargo-llvm-cov 1.0.0",
            },
        )
        self.assertEqual((measurement["status"], measurement["retries"]), (0, []))
        self.assertIn("| Production | 70.5% of 34 |", summary)
        self.assertEqual(
            log,
            f"+ cargo llvm-cov --no-report {' '.join(coverage.RUN_OPTIONS)}\n"
            + PASSED_OUTPUT,
        )

    def test_tests_that_fail_once_pass_when_they_run_again(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(
                Path(directory),
                (101, "     Running unittests src/lib.rs (x)\n" + _result(1, ["a"])),
                (0, _result(1)),
            )
            self.assertEqual(self._measure(cargo), (0, ""))
            measurement, summary, log = self._outputs(cargo)
        self.assertEqual(
            cargo.commands[3],
            ("cargo", "llvm-cov", "--no-report", *coverage.RUN_OPTIONS)
            + ("--lib", "--", "--exact", "a"),
        )
        self.assertEqual(
            measurement["retries"],
            [{"target": "--lib", "tests": ["a"], "failed": [], "status": 0}],
        )
        self.assertIn(
            "The tests `a` of `--lib` passed only when they ran again.", summary
        )
        self.assertEqual(log.count("+ cargo llvm-cov --no-report"), 2)

    def test_tests_that_fail_twice_fail_the_measurement_after_its_reports(self):
        failed = "     Running unittests src/lib.rs (x)\n" + _result(1, ["a"])
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(Path(directory), (101, failed), (101, _result(0, ["a"])))
            status, messages = self._measure(cargo)
            measurement, summary, _ = self._outputs(cargo)
            self.assertTrue((cargo.root / "out" / coverage.LCOV_FILE).is_file())
        self.assertEqual(status, 1)
        self.assertEqual(
            messages, "error: The tests `a` of `--lib` failed when they ran again.\n"
        )
        self.assertEqual(measurement["retries"][0]["failed"], ["a"])
        self.assertIn(
            "- The tests `a` of `--lib` failed when they ran again.\n", summary
        )

    def test_tests_that_do_not_run_fail_the_measurement(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(
                Path(directory), (101, "error: could not compile `nvx`\n")
            )
            with self.assertRaisesRegex(coverage.MeasurementError, "status 101"):
                self._measure(cargo)
            log = (cargo.root / "out" / coverage.LOG_FILE).read_text(encoding="utf-8")
        self.assertIn("error: could not compile `nvx`\n", log)
        self.assertFalse(any("report" in command for command in cargo.commands))

    @unittest.skipIf(os.name == "nt", "Windows has no owner permission bits")
    def test_profiles_that_tests_leave_unreadable_are_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(Path(directory), (0, PASSED_OUTPUT), unreadable=True)
            status, messages = self._measure(cargo)
        self.assertEqual(status, 0)
        self.assertEqual(messages, "Let the owner read and write 1 profiles\n")

    def test_profiles_in_the_crate_s_directory_fail_the_measurement(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(
                Path(directory), (0, PASSED_OUTPUT), strays=["default_7_0_8.profraw"]
            )
            # A profile of an earlier measurement is none of this one's.
            (cargo.crate / "default_1_0_2.profraw").write_bytes(b"")
            status, messages = self._measure(cargo)
            measurement, _, _ = self._outputs(cargo)
        self.assertEqual(status, 1)
        self.assertEqual(measurement["stray_profiles"], ["default_7_0_8.profraw"])
        self.assertIn("`default_7_0_8.profraw`", messages)

    def test_tested_binaries_without_profiles_fail_the_measurement(self):
        report = _report({"src/bin/nvx-rs.rs": _summary(5, 6)})
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(Path(directory), (0, PASSED_OUTPUT), report=report)
            status, messages = self._measure(cargo)
        self.assertEqual(status, 1)
        self.assertEqual(
            messages,
            "error: No process of `nvx-fake-openvmm` wrote a profile, though the "
            "tests start them.\n",
        )

    def test_cargo_llvm_cov_target_dir_holds_the_profiles_if_set(self):
        runner = FakeRunner()
        with patch.dict("os.environ", {"CARGO_LLVM_COV_TARGET_DIR": "coverage"}):
            directory = coverage.profile_directory(runner, Path("crate"))
        self.assertEqual((directory, runner.commands), (Path("crate") / "coverage", []))

    def test_measurements_of_changes_list_the_lines_that_no_test_reaches(self):
        lcov = "SF:src\\host\\cli.rs\nDA:3,0\nDA:4,2\nend_of_record\n"
        changes = coverage.Changes(
            MERGE_BASE, {"src/host/cli.rs": frozenset({2, 3, 4})}
        )
        with tempfile.TemporaryDirectory() as directory:
            cargo = LcovCargo(Path(directory), (0, PASSED_OUTPUT), lcov=lcov)
            with (
                redirect_stderr(io.StringIO()),
                redirect_stdout(io.StringIO()),
                patch.dict("os.environ", {"CARGO_LLVM_COV_TARGET_DIR": ""}),
            ):
                status = coverage.measure(
                    cargo.root / "out", run=cargo, crate=cargo.crate, changes=changes
                )
            _, summary, _ = self._outputs(cargo)
        self.assertEqual(status, 0)
        self.assertIn(
            "Tests reach 50.0% of 2 instrumented lines of production code that the "
            "changes since `0123456789ab` add or change.\n",
            summary,
        )
        self.assertIn("| `src/host/cli.rs` | 3 |\n", summary)


class CommandTests(unittest.TestCase):
    def test_commands_echo_their_output_and_errors_together(self):
        # Rust's programs write UTF-8 whatever the console's encoding.
        script = (
            "import sys; sys.stdout.write('out\\n'); sys.stdout.flush(); "
            "sys.stderr.buffer.write('err \\u00e9\\n'.encode('utf-8'))"
        )
        buffer = io.BytesIO()
        stdout = io.TextIOWrapper(buffer, encoding="ascii")
        with redirect_stdout(stdout), redirect_stderr(io.StringIO()):
            echoed = coverage.run_command([sys.executable, "-c", script], True)
            quiet = coverage.run_command([sys.executable, "-c", "print('out')"], False)
        stdout.flush()
        self.assertEqual(echoed.returncode, 0)
        self.assertEqual(echoed.stdout.splitlines(), ["out", "err \u00e9"])
        # The bytes reach the console whatever its encoding.
        self.assertEqual(
            buffer.getvalue().decode("utf-8").splitlines(), ["out", "err \u00e9"]
        )
        self.assertEqual((quiet.returncode, quiet.stdout.splitlines()), (0, ["out"]))

    def test_errors_print_their_message(self):
        stderr = io.StringIO()
        with (
            redirect_stderr(stderr),
            patch.object(
                coverage, "measure", side_effect=coverage.MeasurementError("x")
            ),
        ):
            status = coverage.main(["--output", "out"])
        self.assertEqual((status, stderr.getvalue()), (1, "error: x\n"))

    def test_the_output_defaults_to_the_build_directory(self):
        with (
            redirect_stderr(io.StringIO()),
            patch.object(coverage, "measure", return_value=0) as measure,
        ):
            self.assertEqual(coverage.main([]), 0)
        measure.assert_called_once_with(
            coverage.REPO_ROOT / "build" / "nvx-rs-coverage", changes=None
        )
        self.assertEqual(
            PurePosixPath(
                coverage.DEFAULT_OUTPUT.relative_to(coverage.REPO_ROOT).as_posix()
            ),
            PurePosixPath("build/nvx-rs-coverage"),
        )

    def test_a_base_lists_the_changes_since_its_merge_base(self):
        changes = coverage.Changes(MERGE_BASE, {})
        with (
            redirect_stderr(io.StringIO()),
            patch.object(coverage, "changes_since", return_value=changes) as since,
            patch.object(coverage, "measure", return_value=0) as measure,
        ):
            self.assertEqual(
                coverage.main(["--output", "out", "--base", "origin/dev"]), 0
            )
        since.assert_called_once_with("origin/dev")
        measure.assert_called_once_with(Path("out"), changes=changes)

    def test_bases_that_git_cannot_resolve_fail_before_the_measurement(self):
        stderr = io.StringIO()
        with (
            redirect_stderr(stderr),
            patch.object(
                coverage,
                "changes_since",
                side_effect=coverage.MeasurementError("no merge base"),
            ),
            patch.object(coverage, "measure", return_value=0) as measure,
        ):
            self.assertEqual(coverage.main(["--base", "missing"]), 1)
        measure.assert_not_called()
        self.assertEqual(stderr.getvalue(), "error: no merge base\n")


if __name__ == "__main__":
    unittest.main()

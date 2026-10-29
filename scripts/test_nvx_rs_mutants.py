#!/usr/bin/env python3
"""Tests for the nvx-rs mutants workflow, which runs cargo-mutants on files of the
nvx crate against the tests that a dispatch selects.

They check the workflow's triggers, its pinned cargo-mutants release, and the
names of its artifacts, and, on POSIX hosts, run its steps with a stub of Cargo:
the plan of the shards, the run of cargo-mutants in each shard, and the merge of
their outcomes.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest
from collections.abc import Mapping
from pathlib import Path, PurePosixPath
from typing import Any, cast

import yaml

REPO_ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = "nvx-rs-mutants.yml"


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


def _artifact(name: str) -> str:
    """Returns an artifact name or pattern of a workflow, with sample values in
    place of the expressions that it holds."""
    for expression, value in (
        ("${{ github.run_id }}", "1234"),
        ("${{ matrix.index }}", "0"),
    ):
        name = name.replace(expression, value)
    if "${{" in name:
        raise AssertionError(f"{name} holds an expression without a sample value")
    return name


class WorkflowTests(unittest.TestCase):
    def test_reruns_summarize_the_latest_artifact_of_every_job(self):
        workflow = _workflow(WORKFLOW)
        job, summary = (
            _step(workflow, *upload)["with"]
            for upload in (
                ("mutants", "Upload the outcomes"),
                ("summarize", "Upload the merged outcomes"),
            )
        )
        pattern = _artifact(
            _step(workflow, "summarize", "Download the outcomes")["with"]["pattern"]
        )
        # Names hold no attempt, so that a rerun of a job replaces the artifact
        # of its earlier attempt, and the summary downloads the latest artifact
        # of every job, but not its own.
        for artifact in (job, summary):
            self.assertNotIn("run_attempt", artifact["name"])
            self.assertIs(artifact["overwrite"], True)
        self.assertTrue(PurePosixPath(_artifact(job["name"])).match(pattern))
        self.assertFalse(PurePosixPath(_artifact(summary["name"])).match(pattern))

    def test_the_workflow_never_runs_for_pull_requests(self):
        workflow = _workflow(WORKFLOW)
        # YAML 1.1 reads the key `on` as true.
        triggers = cast(dict[str, Any], workflow.get("on", workflow.get(True)))
        self.assertNotIn("pull_request", triggers)
        self.assertNotIn("pull_request_target", triggers)
        self.assertIn("workflow_dispatch", triggers)
        # A push revalidates the workflow when it changes.
        self.assertEqual(triggers["push"], {"paths": [f".github/workflows/{WORKFLOW}"]})
        self.assertEqual(workflow["permissions"], {"contents": "read"})

    def test_the_workflow_pins_cargo_mutants(self):
        workflow = _workflow(WORKFLOW)
        self.assertRegex(workflow["env"]["CARGO_MUTANTS_VERSION"], r"^\d+\.\d+\.\d+$")


BASH = shutil.which("bash")
# The inputs of a dispatch of the mutants workflow, as its plan step reads them.
DISPATCH = {
    "EVENT_NAME": "workflow_dispatch",
    "FILES": "src/host/a.rs  src/host/b/*.rs",
    "TESTS": "host::a:: --skip slow",
    "TARGETS": "--lib",
    "OS": "windows-latest",
    "SHARDS": "4",
}


@unittest.skipIf(
    os.name == "nt" or BASH is None, "runs the workflow's bash steps on POSIX hosts"
)
class MutantsWorkflowStepTests(unittest.TestCase):
    """Runs the steps of the mutants workflow, with a stub of Cargo."""

    def _run(
        self, job: str, step: str, environment: Mapping[str, str], directory: Path
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, str], str]:
        """Runs the step as GitHub runs a bash step, in `directory`, and returns
        its result, its outputs, and the summary that it wrote."""
        script = _step(_workflow(WORKFLOW), job, step)["run"]
        outputs = directory / "github-output"
        summary = directory / "github-summary"
        outputs.write_text("", encoding="utf-8")
        summary.write_text("", encoding="utf-8")
        result = subprocess.run(
            [cast(str, BASH), "--noprofile", "--norc", "-eo", "pipefail", "-c", script],
            cwd=directory,
            env={
                **os.environ,
                **environment,
                "GITHUB_OUTPUT": str(outputs),
                "GITHUB_STEP_SUMMARY": str(summary),
            },
            capture_output=True,
            text=True,
            check=False,
        )
        values = dict(
            line.split("=", 1)
            for line in outputs.read_text(encoding="utf-8").splitlines()
        )
        return result, values, summary.read_text(encoding="utf-8")

    def _plan(
        self, environment: Mapping[str, str]
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, str]]:
        with tempfile.TemporaryDirectory() as directory:
            result, outputs, _ = self._run(
                "plan",
                "Check the inputs and plan the shards",
                environment,
                Path(directory),
            )
        return result, outputs

    def test_a_push_checks_the_workflow_on_a_small_file(self):
        result, outputs = self._plan({"EVENT_NAME": "push"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            outputs,
            {
                "files": "src/gpt/crc.rs",
                "tests": "gpt::crc::",
                "targets": "--lib",
                "os": "ubuntu-latest",
                "shards": "2",
                "indexes": "[0,1]",
            },
        )

    def test_dispatches_plan_one_job_for_each_shard(self):
        for shards, indexes in (("1", "[0]"), ("4", "[0,1,2,3]"), ("16", None)):
            with self.subTest(shards=shards):
                result, outputs = self._plan(DISPATCH | {"SHARDS": shards})
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(
                    outputs["indexes"],
                    indexes or "[" + ",".join(map(str, range(16))) + "]",
                )
                self.assertEqual(outputs["files"], DISPATCH["FILES"])
                self.assertEqual(outputs["tests"], DISPATCH["TESTS"])
                self.assertEqual(outputs["os"], "windows-latest")
        result, outputs = self._plan(DISPATCH | {"TESTS": "", "TARGETS": ""})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((outputs["tests"], outputs["targets"]), ("", ""))

    def test_dispatches_with_invalid_inputs_fail_before_any_job(self):
        for change in (
            {"SHARDS": "0"},
            {"SHARDS": "17"},
            {"SHARDS": "04"},
            {"SHARDS": "2 "},
            {"FILES": " \t"},
            {"FILES": "src/a.rs\nsrc/b.rs"},
            {"TESTS": "a\r"},
            {"TARGETS": "--lib\n--test cli"},
            {"OS": "macos-latest"},
        ):
            with self.subTest(change=change):
                result, outputs = self._plan(DISPATCH | change)
                self.assertEqual(result.returncode, 1)
                self.assertIn("::error::", result.stdout)
                self.assertEqual(outputs, {})

    def _mutate(
        self, environment: Mapping[str, str], status: int
    ) -> tuple[subprocess.CompletedProcess[str], list[str]]:
        """Runs the cargo-mutants step in a checkout's nvx-rs directory, as the
        workflow does, with a Cargo that exits with `status`, and returns its
        result and Cargo's arguments. That Cargo exits with 9 instead if the
        parent of its output directory is missing, as cargo-mutants fails."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            crate = root / "nvx-rs"
            stubs = root / "bin"
            stubs.mkdir()
            cargo = stubs / "cargo"
            cargo.write_text(
                '#!/bin/sh\nprintf "%s\\n" "$@" > "$CARGO_ARGUMENTS"\n'
                '[ -d ../build/mutants ] || exit 9\nexit "$CARGO_STATUS"\n',
                encoding="utf-8",
            )
            cargo.chmod(0o755)
            # A file that a glob matches, which the step leaves to cargo-mutants.
            (crate / "src" / "host" / "b").mkdir(parents=True)
            (crate / "src" / "host" / "b" / "c.rs").write_text("", encoding="utf-8")
            result, _, _ = self._run(
                "mutants",
                "Run cargo-mutants",
                {
                    "PATH": f"{stubs}{os.pathsep}{os.environ['PATH']}",
                    "CARGO_ARGUMENTS": str(root / "arguments"),
                    "CARGO_STATUS": str(status),
                    "SHARD": "1/4",
                    **environment,
                },
                crate,
            )
            arguments = (root / "arguments").read_text(encoding="utf-8").splitlines()
        return result, arguments

    def test_shards_pass_the_inputs_to_cargo_mutants(self):
        # In place, since the CLI's tests need the checkout that holds the
        # crate, which a copy lacks.
        common = ["mutants", "--all-features", "--in-place", "--shard", "1/4"]
        common += ["--output", "../build/mutants"]
        environment = {key: DISPATCH[key] for key in ("FILES", "TESTS", "TARGETS")}
        result, arguments = self._mutate(environment, 0)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            arguments,
            [
                *common,
                "--file",
                "src/host/a.rs",
                "--file",
                "src/host/b/*.rs",
                "--",
                "--lib",
                "--",
                "host::a::",
                "--skip",
                "slow",
            ],
        )
        # Without test or target arguments, every test of every target runs.
        result, arguments = self._mutate(
            {"FILES": "src/a.rs", "TESTS": "", "TARGETS": ""}, 0
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(arguments, [*common, "--file", "src/a.rs", "--", "--"])

    def test_missed_mutants_and_timeouts_are_outcomes_and_failures_fail(self):
        environment = {"FILES": "src/a.rs", "TESTS": "", "TARGETS": ""}
        for status, expected in ((0, 0), (1, 1), (2, 0), (3, 0), (4, 4), (70, 70)):
            with self.subTest(status=status):
                result, _ = self._mutate(environment, status)
                self.assertEqual(result.returncode, expected)

    def _merge(
        self,
        shards: Mapping[str, Mapping[str, str]],
        count: int,
        mutants_result: str = "success",
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, str], str]:
        """Merges the outcome files of `shards`, which maps the index of each
        shard to its files, for `count` shards whose mutants job ended with
        `mutants_result`, and returns the result, the merged lists, and the
        summary.

        The download merges the artifacts of the shards into shards/, where
        each artifact holds its shard's outcomes under the shard's index."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for index, files in shards.items():
                _write(root / "shards" / index, files)
            result, _, summary = self._run(
                "summarize",
                "Merge the outcomes of the shards",
                {
                    "FILES": "src/a.rs",
                    "TESTS": "a::",
                    "TARGETS": "",
                    "OS": "ubuntu-latest",
                    "SHARDS": str(count),
                    "MUTANTS_RESULT": mutants_result,
                },
                root,
            )
            merged = {
                path.name: path.read_text(encoding="utf-8")
                for path in sorted((root / "mutants").glob("*.txt"))
            }
        return result, merged, summary

    def test_the_outcomes_of_every_shard_merge(self):
        result, merged, summary = self._merge(
            {
                "0": {
                    "outcomes.json": "{}\n",
                    "caught.txt": "src/a.rs:2:1: b\nsrc/a.rs:1:1: a\n",
                    "missed.txt": "src/a.rs:9:1: m\n",
                    "timeout.txt": "",
                    "unviable.txt": "",
                },
                "1": {"outcomes.json": "{}\n", "caught.txt": "src/a.rs:3:1: c\n"},
            },
            2,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            merged,
            {
                "caught.txt": "src/a.rs:1:1: a\nsrc/a.rs:2:1: b\nsrc/a.rs:3:1: c\n",
                "missed.txt": "src/a.rs:9:1: m\n",
                "timeout.txt": "",
                "unviable.txt": "",
            },
        )
        self.assertIn("on ubuntu-latest: 2 of 2 shards reported.", summary)
        self.assertIn("| caught | 3 |\n| missed | 1 |\n| timeout | 0 |\n", summary)
        self.assertIn("<summary>Missed mutants</summary>", summary)
        self.assertIn("src/a.rs:9:1: m\n", summary)

    def test_shards_that_report_nothing_fail_the_merge(self):
        result, merged, summary = self._merge(
            {
                "0": {"outcomes.json": "{}\n", "caught.txt": "src/a.rs:1:1: a\n"},
                # A shard without outcomes.json reported nothing, and the merge
                # leaves its lists out.
                "1": {"caught.txt": "src/a.rs:2:1: b\n"},
            },
            3,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("::error::Only 1 of 3 shards reported", result.stdout)
        self.assertEqual(merged["caught.txt"], "src/a.rs:1:1: a\n")
        self.assertIn("on ubuntu-latest: 1 of 3 shards reported.", summary)
        self.assertNotIn("Missed mutants", summary)

    def test_a_mutants_job_that_did_not_succeed_fails_the_merge(self):
        shards = {"0": {"outcomes.json": "{}\n", "caught.txt": "src/a.rs:1:1: a\n"}}
        for mutants_result in ("failure", "cancelled", "skipped"):
            with self.subTest(mutants_result=mutants_result):
                result, merged, summary = self._merge(shards, 1, mutants_result)
                self.assertEqual(result.returncode, 1)
                self.assertIn(
                    f"::error::The mutants job did not succeed ({mutants_result}).",
                    result.stdout,
                )
                # The merge stops before it merges or summarizes anything.
                self.assertEqual((merged, summary), ({}, ""))


if __name__ == "__main__":
    unittest.main()

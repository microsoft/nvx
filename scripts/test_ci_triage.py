#!/usr/bin/env python3
"""Unit tests for deterministic NVX CI run triage."""

from __future__ import annotations

import base64
import contextlib
import io
import json
import subprocess
import sys
import unittest
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import cast

sys.path.insert(0, str(Path(__file__).parent))

import ci_triage

SCRIPT = Path(__file__).with_name("ci_triage.py")
ROOT = Path(__file__).resolve().parents[1]
NOW = datetime(2026, 11, 6, tzinfo=timezone.utc)
OLD_HEAD = "a" * 40
NEW_HEAD = "b" * 40
# The benchmark host check's mismatch diagnostic, as performance.py writes it.
HOST_CHANGED = (
    "Linux / KVM: this runner differs from "
    "data/linux-kvm-virtual-machine-host.json in 2 required field(s): "
    "cpu_model, vm_size. The series' history comes from the expected host; to "
    "measure it here, accept this host and reset the history "
    '(doc/benchmarks.md, "Expected benchmark hosts").'
)


@dataclass(frozen=True)
class Response:
    stdout: str = ""
    stderr: str = ""
    returncode: int = 0


class FakeRunner:
    def __init__(self, responses: Sequence[Response]) -> None:
        self.responses = list(responses)
        self.calls: list[tuple[str, ...]] = []

    def __call__(self, arguments: Sequence[str]) -> subprocess.CompletedProcess[str]:
        self.calls.append(tuple(arguments))
        if not self.responses:
            raise AssertionError(f"unexpected command: {arguments}")
        response = self.responses.pop(0)
        return subprocess.CompletedProcess(
            list(arguments),
            response.returncode,
            response.stdout,
            response.stderr,
        )

    def assert_finished(self) -> None:
        if self.responses:
            raise AssertionError(f"{len(self.responses)} response(s) were not used")


def encoded(value: object) -> str:
    return json.dumps(value)


def step(
    name: str,
    conclusion: str = "failure",
    *,
    completed_at: str = "2026-11-05T00:02:00Z",
) -> dict[str, object]:
    return {
        "name": name,
        "status": "completed",
        "conclusion": conclusion,
        "number": 2,
        "startedAt": "2026-11-05T00:01:00Z",
        "completedAt": completed_at,
    }


def job(
    name: str,
    *,
    job_id: int = 501,
    conclusion: str = "failure",
    failed_step: str = "Run tests",
    completed_at: str = "2026-11-05T00:03:00Z",
) -> dict[str, object]:
    return {
        "databaseId": job_id,
        "name": name,
        "status": "completed",
        "conclusion": conclusion,
        "startedAt": "2026-11-05T00:00:00Z",
        "completedAt": completed_at,
        "runnerName": "runner-1",
        "labels": ["self-hosted", "Linux"],
        "steps": [step("Checkout", "success"), step(failed_step)],
    }


def run_document(
    *,
    run_id: int = 100,
    event: str = "push",
    status: str = "completed",
    conclusion: str | None = "success",
    head: str = OLD_HEAD,
    branch: str = "feature/test",
    jobs: Sequence[dict[str, object]] = (),
    attempt: int = 1,
    created_at: str = "2026-11-05T00:00:00Z",
    workflow_id: int = 7,
    workflow_name: str = "CI",
) -> str:
    return encoded(
        {
            "databaseId": run_id,
            "attempt": attempt,
            "event": event,
            "status": status,
            "conclusion": conclusion,
            "headSha": head,
            "headBranch": branch,
            "url": f"https://github.com/microsoft/nvx/actions/runs/{run_id}",
            "workflowDatabaseId": workflow_id,
            "workflowName": workflow_name,
            "createdAt": created_at,
            "updatedAt": "2026-11-05T00:04:00Z",
            "jobs": list(jobs),
        }
    )


def workflow_document(
    path: str = ".github/workflows/ci.yml", workflow_id: int = 7
) -> str:
    return encoded({"id": workflow_id, "name": "CI", "path": path})


def pull_request(
    *,
    head: str,
    draft: bool = False,
    number: int = 42,
    with_branch: bool = False,
) -> dict[str, object]:
    value: dict[str, object] = {
        "number": number,
        "headRefOid": head,
        "isDraft": draft,
        "url": f"https://github.com/microsoft/nvx/pull/{number}",
        "state": "OPEN",
    }
    if with_branch:
        value["headRefName"] = "feature/test"
    return value


def listed_run(
    run_id: int,
    head: str,
    *,
    conclusion: str | None = "success",
    created_at: str = "2026-11-05T01:00:00Z",
    event: str = "pull_request",
    attempt: int = 1,
) -> dict[str, object]:
    return {
        "databaseId": run_id,
        "attempt": attempt,
        "status": "completed",
        "conclusion": conclusion,
        "event": event,
        "headSha": head,
        "createdAt": created_at,
        "url": f"https://github.com/microsoft/nvx/actions/runs/{run_id}",
    }


def workflow_content(queue: str) -> str:
    workflow = (
        "name: CI\n"
        "concurrency:\n"
        "  group: ci-${{ github.ref }}\n"
        "  cancel-in-progress: false\n"
        f"  queue: {queue}\n"
        "jobs: {}\n"
    )
    return encoded(
        {
            "encoding": "base64",
            "content": base64.b64encode(workflow.encode()).decode(),
        }
    )


class CiTriageTests(unittest.TestCase):
    def run_cli(
        self, arguments: Sequence[str], runner: FakeRunner
    ) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            result = ci_triage.run_cli(arguments, runner, now=NOW)
        return result, stdout.getvalue(), stderr.getvalue()

    def json_classification(
        self, responses: Sequence[Response], target: str = "100"
    ) -> tuple[str, dict[str, object], FakeRunner]:
        runner = FakeRunner(responses)
        result, stdout, stderr = self.run_cli(
            [target, "--no-history", "--json"], runner
        )
        self.assertEqual(result, 0, stderr)
        document = cast(dict[str, object], json.loads(stdout))
        classification = cast(dict[str, object], document["classification"])
        return cast(str, classification["name"]), document, runner

    def test_help_runs_with_the_selected_python(self) -> None:
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--help"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("CI workflow run", result.stdout)

    def test_target_parser_accepts_run_and_pull_request_forms(self) -> None:
        cases = {
            "123": ci_triage.Target("run", 123),
            "https://github.com/microsoft/nvx/actions/runs/123": (
                ci_triage.Target("run", 123)
            ),
            "https://github.com/microsoft/nvx/actions/runs/123/attempts/2": (
                ci_triage.Target("run", 123, attempt=2)
            ),
            "https://github.com/microsoft/nvx/actions/runs/123/job/456": (
                ci_triage.Target("run", 123, job_id=456)
            ),
            "https://github.com/microsoft/nvx/pull/42": (
                ci_triage.Target("pull_request", 42)
            ),
            "#42": ci_triage.Target("pull_request", 42),
            "pr:42": ci_triage.Target("pull_request", 42),
        }
        for value, expected in cases.items():
            with self.subTest(value=value):
                self.assertEqual(ci_triage.parse_target(value), expected)

    def test_target_parser_rejects_another_repository(self) -> None:
        with self.assertRaisesRegex(ci_triage.TriageError, "belongs to"):
            ci_triage.parse_target(
                "https://github.com/example/project/actions/runs/123"
            )

    def test_pull_request_target_uses_latest_run_of_current_head(self) -> None:
        runner = FakeRunner(
            [
                Response(encoded(pull_request(head=NEW_HEAD, with_branch=True))),
                Response(
                    encoded(
                        [
                            listed_run(
                                200,
                                NEW_HEAD,
                                created_at="2026-11-05T00:00:00Z",
                            ),
                            listed_run(
                                201,
                                NEW_HEAD,
                                created_at="2026-11-05T01:00:00Z",
                                attempt=3,
                            ),
                        ]
                    )
                ),
                Response(run_document(run_id=201, head=NEW_HEAD, attempt=3)),
            ]
        )
        result, stdout, stderr = self.run_cli(["#42", "--no-history", "--json"], runner)
        self.assertEqual(result, 0, stderr)
        document = cast(dict[str, object], json.loads(stdout))
        run = cast(dict[str, object], document["run"])
        self.assertEqual(run["databaseId"], 201)
        self.assertIn("--attempt", runner.calls[2])
        self.assertIn("3", runner.calls[2])
        runner.assert_finished()

    def test_passed_and_in_progress_classifications(self) -> None:
        name, _, runner = self.json_classification(
            [Response(run_document(conclusion="success"))]
        )
        self.assertEqual(name, "passed")
        runner.assert_finished()

        name, _, runner = self.json_classification(
            [Response(run_document(status="in_progress", conclusion=None))]
        )
        self.assertEqual(name, "in-progress")
        runner.assert_finished()

    def test_cancelled_pull_request_is_superseded_by_current_head_run(self) -> None:
        name, document, runner = self.json_classification(
            [
                Response(
                    run_document(
                        event="pull_request",
                        conclusion="cancelled",
                        branch="feature/test",
                    )
                ),
                Response(encoded([pull_request(head=NEW_HEAD)])),
                Response(encoded([listed_run(101, NEW_HEAD)])),
            ]
        )
        self.assertEqual(name, "superseded")
        replacement = cast(dict[str, object], document["replacementRun"])
        self.assertEqual(replacement["databaseId"], 101)
        self.assertEqual(document["logs"], [])
        runner.assert_finished()

    def test_cancelled_current_head_is_investigated_and_draft_is_noted(self) -> None:
        name, document, runner = self.json_classification(
            [
                Response(
                    run_document(
                        event="pull_request",
                        conclusion="cancelled",
                        branch="feature/test",
                    )
                ),
                Response(encoded([pull_request(head=OLD_HEAD, draft=True)])),
            ]
        )
        self.assertEqual(name, "cancelled-current-head")
        notes = cast(list[str], document["notes"])
        self.assertTrue(any("now draft" in note for note in notes))
        runner.assert_finished()

    def test_cancelled_dev_push_with_queue_max_needs_investigation(self) -> None:
        name, document, runner = self.json_classification(
            [
                Response(
                    run_document(event="push", conclusion="cancelled", branch="dev")
                ),
                Response(workflow_document()),
                Response(workflow_content("max")),
            ]
        )
        self.assertEqual(name, "dev-push-cancelled")
        workflow = cast(dict[str, object], document["workflowAtHead"])
        self.assertEqual(workflow["name"], "CI")
        self.assertIs(workflow["queueMax"], True)
        runner.assert_finished()

    def test_cancelled_dev_push_without_queue_max_can_be_superseded(self) -> None:
        name, document, runner = self.json_classification(
            [
                Response(
                    run_document(event="push", conclusion="cancelled", branch="dev")
                ),
                Response(workflow_document()),
                Response(workflow_content("single")),
                Response(
                    encoded(
                        [
                            listed_run(
                                102,
                                NEW_HEAD,
                                event="push",
                                created_at="2026-11-05T02:00:00Z",
                            )
                        ]
                    )
                ),
            ]
        )
        self.assertEqual(name, "superseded")
        self.assertIsNotNone(document["replacementRun"])
        runner.assert_finished()

    def test_a_cancelled_push_of_another_workflow_reads_that_workflow(self) -> None:
        mutants = ".github/workflows/nvx-rs-mutants.yml"
        name, document, runner = self.json_classification(
            [
                Response(
                    run_document(
                        event="push",
                        conclusion="cancelled",
                        branch="dev",
                        workflow_id=9,
                        workflow_name="nvx-rs mutants",
                    )
                ),
                Response(workflow_document(mutants, 9)),
                Response(workflow_content("single")),
                Response(
                    encoded(
                        [
                            listed_run(
                                102,
                                NEW_HEAD,
                                event="push",
                                created_at="2026-11-05T02:00:00Z",
                            )
                        ]
                    )
                ),
            ]
        )
        self.assertEqual(name, "superseded")
        self.assertIsNotNone(document["replacementRun"])
        self.assertEqual(
            runner.calls[1], ("gh", "api", "repos/microsoft/nvx/actions/workflows/9")
        )
        self.assertEqual(
            runner.calls[2],
            ("gh", "api", f"repos/microsoft/nvx/contents/{mutants}?ref={OLD_HEAD}"),
        )
        listing = runner.calls[3]
        self.assertEqual(listing[listing.index("--workflow") + 1], "9")
        runner.assert_finished()

    def test_a_cancelled_pull_request_of_another_workflow_finds_its_runs(
        self,
    ) -> None:
        name, _, runner = self.json_classification(
            [
                Response(
                    run_document(
                        event="pull_request",
                        conclusion="cancelled",
                        branch="feature/test",
                        workflow_id=9,
                        workflow_name="nvx-rs mutants",
                    )
                ),
                Response(encoded([pull_request(head=NEW_HEAD)])),
                Response(encoded([listed_run(101, NEW_HEAD)])),
            ]
        )
        self.assertEqual(name, "superseded")
        listing = runner.calls[2]
        self.assertEqual(listing[listing.index("--workflow") + 1], "9")
        runner.assert_finished()

    def test_only_the_workflows_top_level_concurrency_counts(self) -> None:
        workflow = ci_triage.parse_workflow(
            "name: CI\n"
            "concurrency:\n"
            "  group: ci-${{ github.ref }}\n"
            "  cancel-in-progress: false\n"
            "  queue: max\n"
            "jobs:\n"
            "  release:\n"
            "    concurrency:\n"
            "      group: crate-release\n"
            "      cancel-in-progress: true\n",
            "test",
        )
        self.assertEqual(workflow.group, "ci-${{ github.ref }}")
        self.assertEqual(workflow.cancel_in_progress, "false")
        self.assertTrue(workflow.queue_max)
        ci = ci_triage.parse_workflow(
            (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8"),
            "ci.yml",
        )
        self.assertTrue(cast(str, ci.group).startswith("ci-"), ci.group)

    def test_cancelled_merge_group_checks_its_pull_request(self) -> None:
        name, document, runner = self.json_classification(
            [
                Response(
                    run_document(
                        event="merge_group",
                        conclusion="cancelled",
                        branch="gh-readonly-queue/dev/pr-42-deadbeef",
                    )
                ),
                Response(encoded(pull_request(head=NEW_HEAD, with_branch=True))),
            ]
        )
        self.assertEqual(name, "merge-group-cancelled")
        pull = cast(dict[str, object], document["pullRequest"])
        self.assertEqual(pull["number"], 42)
        runner.assert_finished()

    def test_benchmark_host_failure_is_infrastructure(self) -> None:
        # The host check is a step of the run-benchmark composite action, so
        # the job reports it as its Run benchmark step, which a benchmark's own
        # failure also fails; only the failed log tells them apart.
        failed_job = job("Linux benchmark (kvm, 1)", failed_step="Run benchmark")
        name, document, runner = self.json_classification(
            [
                Response(run_document(conclusion="failure", jobs=[failed_job])),
                Response(
                    "job\tRun benchmark\t2026-11-05T00:00:00Z MISMATCH: cpu_model\n"
                    f"job\tRun benchmark\t2026-11-05T00:00:01Z ##[error]{HOST_CHANGED}\n"
                    "job\tRun benchmark\t2026-11-05T00:00:02Z "
                    "##[error]Process completed with exit code 1.\n"
                ),
            ]
        )
        self.assertEqual(name, "benchmark-host-changed")
        first = cast(dict[str, object], document["firstFailure"])
        self.assertEqual(first["step"], "Run benchmark")
        runner.assert_finished()

    def test_a_host_change_in_any_failed_benchmark_step_counts(self) -> None:
        # Excerpts come from the first five failed jobs, but the log of every
        # failed benchmark step counts.
        others = [job(f"Build {index}", job_id=510 + index) for index in range(1, 6)]
        benchmark = job(
            "Linux benchmark (kvm, 1)", job_id=520, failed_step="Run benchmark"
        )
        name, _document, runner = self.json_classification(
            [
                Response(run_document(conclusion="failure", jobs=[*others, benchmark])),
                *(
                    Response("job\tRun tests\t2026-11-05T00:00:00Z Error: broken\n")
                    for _ in others
                ),
                Response(
                    f"job\tRun benchmark\t2026-11-05T00:00:00Z ##[error]{HOST_CHANGED}\n"
                ),
            ]
        )
        self.assertEqual(name, "benchmark-host-changed")
        log = runner.calls[-1]
        self.assertEqual(log[log.index("--job") + 1], "520")
        runner.assert_finished()

    def test_a_host_diagnostic_outside_a_benchmark_step_is_failed(self) -> None:
        # A failing test of the host check can quote its diagnostic.
        quoted = f"AssertionError: '{HOST_CHANGED}' != ''"
        name, _document, runner = self.json_classification(
            [
                Response(
                    run_document(
                        conclusion="failure",
                        jobs=[job("NVX CLI / Linux", failed_step="Run NVX CLI tests")],
                    )
                ),
                Response(f"job\tRun NVX CLI tests\t2026-11-05T00:00:00Z {quoted}\n"),
            ]
        )
        self.assertEqual(name, "failed")
        runner.assert_finished()

    def test_other_benchmark_failures_are_failed(self) -> None:
        # Advisory host drift warns without failing the step.
        drift = (
            "Linux / KVM: this runner differs from "
            "data/linux-kvm-virtual-machine-host.json in advisory field(s): "
            "kernel. Update the file to acknowledge them; that needs no history "
            "reset."
        )
        missing = "Expected 34 microVM one-vCPU metrics in build/benchmark/result.json"
        name, _document, runner = self.json_classification(
            [
                Response(
                    run_document(
                        conclusion="failure",
                        jobs=[
                            job("Linux benchmark (kvm, 1)", failed_step="Run benchmark")
                        ],
                    )
                ),
                Response(
                    f"job\tRun benchmark\t2026-11-05T00:00:00Z ##[warning]{drift}\n"
                    f"job\tRun benchmark\t2026-11-05T00:09:00Z ##[error]{missing}\n"
                ),
            ]
        )
        self.assertEqual(name, "failed")
        runner.assert_finished()

    def test_other_failure_is_failed_and_text_starts_with_classification(self) -> None:
        runner = FakeRunner(
            [
                Response(
                    run_document(
                        conclusion="failure", jobs=[job("Quality (linux, debug)")]
                    )
                ),
                Response("job\tstep\t2026-11-05T00:00:00Z Error: broken\n"),
            ]
        )
        result, stdout, stderr = self.run_cli(
            ["100", "--no-history", "--log-lines", "4"], runner
        )
        self.assertEqual(result, 0, stderr)
        self.assertTrue(stdout.startswith("Classification: failed"))
        self.assertIn("First failure: Quality (linux, debug) / Run tests", stdout)
        self.assertIn("Error: broken", stdout)
        runner.assert_finished()

    def test_log_excerpt_strips_prefixes_and_follows_the_first_marker(self) -> None:
        log = "\n".join(
            [
                "job\tstep\t2026-11-05T00:00:00Z setup",
                "job\tstep\t2026-11-05T00:00:01Z before",
                "job\tstep\t2026-11-05T00:00:02Z FAILED first cause",
                "job\tstep\t2026-11-05T00:00:03Z after one",
                "job\tstep\t2026-11-05T00:00:04Z after two",
                "job\tstep\t2026-11-05T00:00:05Z after three",
                "job\tstep\t2026-11-05T00:00:06Z after four",
                "job\tstep\t2026-11-05T00:00:07Z unrelated tail",
            ]
        )
        excerpt = ci_triage.extract_log_excerpt(log, 5)
        self.assertEqual(
            excerpt,
            ["setup", "before", "FAILED first cause", "after one", "after two"],
        )
        self.assertTrue(all("job\tstep" not in line for line in excerpt))
        self.assertEqual(
            ci_triage.extract_log_excerpt("one\ntwo\nthree", 2),
            ["two", "three"],
        )
        markers = (
            "##[error]bad",
            "error: bad",
            "Error: bad",
            "FAILED bad",
            "Traceback (most recent call last):",
            "panicked at source.rs:1",
            "AssertionError: bad",
            "FAIL: test_bad (module.Tests.test_bad)",
            "        FAIL [   0.012s] crate::module::test_bad",
            "---- module::test_bad stdout ----",
        )
        for marker in markers:
            with self.subTest(marker=marker):
                self.assertIn(
                    marker,
                    ci_triage.extract_log_excerpt(
                        f"job\tstep\t2026-11-05T00:00:00Z {marker}", 1
                    ),
                )

    def test_log_excerpt_prefers_a_test_failure_to_a_passing_tests_output(
        self,
    ) -> None:
        # A passing test that checks a CLI's error prints `error:` long before
        # unittest reports the failure, as in run 42340111506.
        log = "\n".join(
            [
                "test_cli_rejects_unknown_backends ... prog: error: invalid choice",
                "ok",
                "test_other ... ok",
                "======================================================",
                "FAIL: test_measurements (module.MeasureTests.test_measurements)",
                "------------------------------------------------------",
                "Traceback (most recent call last):",
                "AssertionError: Lists differ",
                "Ran 1867 tests in 339.744s",
                "FAILED (failures=1, skipped=71)",
                "##[error]Process completed with exit code 1.",
            ]
        )
        excerpt = ci_triage.extract_log_excerpt(log, 7)
        self.assertEqual(
            excerpt,
            [
                "ok",
                "test_other ... ok",
                "======================================================",
                "FAIL: test_measurements (module.MeasureTests.test_measurements)",
                "------------------------------------------------------",
                "Traceback (most recent call last):",
                "AssertionError: Lists differ",
            ],
        )
        # The generic annotation of a failed step is the last resort.
        self.assertEqual(
            ci_triage.extract_log_excerpt(
                "build output\n##[error]Process completed with exit code 2.", 2
            ),
            ["build output", "##[error]Process completed with exit code 2."],
        )

    def test_history_matches_job_and_step_and_reports_issue_searches(self) -> None:
        current_job = job("Platform (linux, kvm)", failed_step="Run microVM tests")
        historical_job = job(
            "Platform (linux, kvm)",
            job_id=601,
            failed_step="Run microVM tests",
        )
        runner = FakeRunner(
            [
                Response(run_document(conclusion="failure", jobs=[current_job])),
                Response(
                    "job\tstep\t2026-11-05T00:00:00Z Traceback "
                    "(most recent call last):\n"
                ),
                Response(
                    encoded(
                        [
                            {
                                "databaseId": 90,
                                "conclusion": "failure",
                                "createdAt": "2026-11-01T00:00:00Z",
                                "headSha": "c" * 40,
                                "url": "https://example.test/run/90",
                                "event": "push",
                            },
                            {
                                "databaseId": 89,
                                "conclusion": "failure",
                                "createdAt": "2026-10-01T00:00:00Z",
                                "headSha": "d" * 40,
                                "url": "https://example.test/run/89",
                                "event": "push",
                            },
                        ]
                    )
                ),
                Response(encoded({"jobs": [historical_job]})),
                Response(
                    encoded(
                        [
                            {
                                "number": 77,
                                "title": "Platform is flaky",
                                "url": "https://example.test/issues/77",
                            }
                        ]
                    )
                ),
                Response(encoded([])),
            ]
        )
        result, stdout, stderr = self.run_cli(["100", "--json"], runner)
        self.assertEqual(result, 0, stderr)
        document = cast(dict[str, object], json.loads(stdout))
        history = cast(dict[str, object], document["history"])
        matches = cast(list[dict[str, object]], history["runs"])
        issues = cast(list[dict[str, object]], history["issues"])
        self.assertEqual(len(matches), 1)
        self.assertEqual(matches[0]["databaseId"], 90)
        self.assertEqual(matches[0]["failedStep"], "Run microVM tests")
        self.assertEqual(issues[0]["number"], 77)
        run = cast(dict[str, object], document["run"])
        jobs = cast(list[dict[str, object]], run["jobs"])
        self.assertEqual(jobs[0]["matrix"], ["linux", "kvm"])
        self.assertEqual(jobs[0]["runnerName"], "runner-1")
        searches = [call for call in runner.calls if "issue" in call]
        self.assertEqual(len(searches), 2)
        self.assertTrue(
            any('"Platform (linux, kvm)" in:title,body' in call for call in searches)
        )
        self.assertTrue(
            any('"Run microVM tests" in:title,body' in call for call in searches)
        )
        runner.assert_finished()

    def test_unresolvable_target_returns_one_without_traceback(self) -> None:
        runner = FakeRunner([Response(stderr="not found", returncode=1)])
        result, _, stderr = self.run_cli(["999", "--no-history"], runner)
        self.assertEqual(result, 1)
        self.assertTrue(stderr.startswith("error:"))
        self.assertNotIn("Traceback", stderr)
        runner.assert_finished()


if __name__ == "__main__":
    unittest.main()

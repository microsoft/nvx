#!/usr/bin/env python3
# pyright: reportPrivateUsage=false
"""Tests for the nightly CI failure report (nvx.py report-nightly-ci)."""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import re
import shlex
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Mapping, Sequence
from email.message import Message
from pathlib import Path
from typing import cast
from unittest.mock import patch

import yaml

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import nightly_ci  # noqa: E402
from nvx_tools.common import ScriptError  # noqa: E402
from nvx_tools.nightly_ci import (  # noqa: E402
    FailureKind,
    GitHubApiError,
    Job,
    Step,
)

REPOSITORY = "microsoft/nvx"
REPOSITORY_URL = "https://github.com/microsoft/nvx"
WORKFLOW_ID = 348569047
WORKFLOWS = Path(__file__).resolve().parent.parent / ".github" / "workflows"


def _run(
    run_id: int = 5001,
    *,
    number: int = 50,
    attempt: int = 1,
    conclusion: str | None = "failure",
    event: str = "schedule",
    status: str = "completed",
    sha: str = "0123456789abcdef0123456789abcdef01234567",
) -> dict[str, object]:
    return {
        "id": run_id,
        "run_number": number,
        "run_attempt": attempt,
        "workflow_id": WORKFLOW_ID,
        "name": "CI",
        "path": ".github/workflows/ci.yml",
        "event": event,
        "status": status,
        "conclusion": conclusion,
        "head_branch": "dev",
        "head_sha": sha,
        "updated_at": "2026-10-22T05:58:12Z",
        "repository": {"html_url": REPOSITORY_URL},
    }


def _step(
    number: int,
    name: str,
    conclusion: str | None = "success",
    started_at: str = "2026-10-22T05:20:00Z",
    completed_at: str = "2026-10-22T05:20:00Z",
) -> dict[str, object]:
    return {
        "number": number,
        "name": name,
        "conclusion": conclusion,
        "started_at": started_at,
        "completed_at": completed_at,
    }


def _job(
    job_id: int,
    name: str,
    conclusion: str | None,
    steps: Sequence[dict[str, object]] = (),
    runner: str | None = "azure-kvm-1",
) -> dict[str, object]:
    return {
        "id": job_id,
        "name": name,
        "html_url": f"{REPOSITORY_URL}/actions/runs/5001/job/{job_id}",
        "conclusion": conclusion,
        "runner_name": runner,
        "steps": list(steps),
    }


def _parsed_job(
    name: str = "NVX microVM tests / Linux / KVM",
    conclusion: str | None = "failure",
    steps: Sequence[tuple[str, str | None]] = (),
    runner: str | None = "azure-kvm-1",
) -> Job:
    return Job(
        id=1,
        name=name,
        url=f"{REPOSITORY_URL}/actions/runs/5001/job/1",
        conclusion=conclusion,
        runner_name=runner,
        steps=tuple(
            Step(index + 1, step_name, step_conclusion, None, None)
            for index, (step_name, step_conclusion) in enumerate(steps)
        ),
    )


def _annotation(message: str, level: str = "failure") -> dict[str, object]:
    return {"annotation_level": level, "message": message}


class FakeGitHub:
    """An in-memory GitHub that serves the REST paths that the report uses."""

    def __init__(self) -> None:
        self.runs: dict[int, dict[str, object]] = {}
        self.attempts: dict[tuple[int, int], dict[str, object]] = {}
        self.jobs: dict[tuple[int, int], list[dict[str, object]]] = {}
        self.annotations: dict[int, list[dict[str, object]]] = {}
        self.logs: dict[int, str] = {}
        self.log_errors: set[int] = set()
        self.log_requests: list[int] = []
        self.annotation_requests: list[int] = []
        # The time that passes while GitHub sends a log.
        self.now = 0.0
        self.log_seconds = 0.0
        self.labels = {"bug"}
        self.rejected_assignees: set[str] = set()
        self.issues: dict[int, dict[str, object]] = {}
        self.comments: dict[int, list[str]] = {}
        self.writes: list[tuple[str, str, dict[str, object]]] = []
        self.listed_runs: dict[str, str] = {}
        self.next_number = 700
        # A pull request also appears in the issue list.
        self.issues[1] = {
            "number": 1,
            "state": "open",
            "body": nightly_ci.REPORT_MARKER,
            "pull_request": {},
            "user": {"login": "ppenna", "type": "User"},
            "author_association": "OWNER",
        }

    def add_run(
        self, run: dict[str, object], jobs: Sequence[dict[str, object]] = ()
    ) -> None:
        run_id = cast(int, run["id"])
        attempt = cast(int, run["run_attempt"])
        self.runs[run_id] = run
        self.attempts[(run_id, attempt)] = dict(run)
        self.jobs[(run_id, attempt)] = list(jobs)

    def add_issue(
        self,
        number: int,
        body: str = nightly_ci.REPORT_MARKER,
        *,
        login: str = "ppenna",
        association: str = "MEMBER",
    ) -> None:
        self.issues[number] = {
            "number": number,
            "state": "open",
            "body": body,
            "user": {"login": login, "type": "User"},
            "author_association": association,
        }
        self.comments[number] = []

    def open_reports(self) -> list[dict[str, object]]:
        return [
            issue
            for number, issue in sorted(self.issues.items())
            if number != 1 and issue["state"] == "open"
        ]

    def _route(self, path: str, pattern: str) -> re.Match[str] | None:
        return re.fullmatch(f"/repos/{REPOSITORY}{pattern}", path)

    def get(self, path: str, params: Mapping[str, str] | None = None) -> object:
        if match := self._route(path, r"/actions/runs/(\d+)"):
            return self.runs[int(match.group(1))]
        if match := self._route(path, r"/actions/runs/(\d+)/attempts/(\d+)"):
            return self.attempts[(int(match.group(1)), int(match.group(2)))]
        if match := self._route(path, r"/actions/workflows/(\d+)/runs"):
            self.listed_runs = dict(params or {})
            scheduled = sorted(
                (run for run in self.runs.values() if run["event"] == "schedule"),
                key=lambda run: cast(int, run["run_number"]),
                reverse=True,
            )
            count = int(self.listed_runs.get("per_page", "30"))
            return {"total_count": len(scheduled), "workflow_runs": scheduled[:count]}
        if match := self._route(path, r"/labels/(.+)"):
            if urllib.parse.unquote(match.group(1)) in self.labels:
                return {"name": urllib.parse.unquote(match.group(1))}
            raise GitHubApiError(404, "Not Found")
        raise AssertionError(f"unexpected GET {path}")

    def get_all(
        self,
        path: str,
        params: Mapping[str, str] | None = None,
        *,
        key: str | None = None,
        deadline: float | None = None,
    ) -> list[object]:
        if match := self._route(path, r"/actions/runs/(\d+)/attempts/(\d+)/jobs"):
            assert key == "jobs"
            return list(self.jobs[(int(match.group(1)), int(match.group(2)))])
        if match := self._route(path, r"/check-runs/(\d+)/annotations"):
            assert deadline is not None
            self.annotation_requests.append(int(match.group(1)))
            return list(self.annotations.get(int(match.group(1)), []))
        if self._route(path, r"/issues"):
            assert params == {"state": "open"}
            return [issue for issue in self.issues.values() if issue["state"] == "open"]
        raise AssertionError(f"unexpected GET {path}")

    def get_log(self, path: str, *, deadline: float | None = None) -> str | None:
        match = self._route(path, r"/actions/jobs/(\d+)/logs")
        assert match is not None, path
        assert deadline is not None
        job_id = int(match.group(1))
        self.log_requests.append(job_id)
        self.now += self.log_seconds
        if job_id in self.log_errors:
            raise ScriptError("GitHub GET logs failed: connection reset")
        return self.logs.get(job_id)

    def exists(self, path: str) -> bool:
        try:
            self.get(path)
        except GitHubApiError as error:
            if error.status == 404:
                return False
            raise
        return True

    def post(self, path: str, body: Mapping[str, object]) -> object:
        self.writes.append(("POST", path, dict(body)))
        if self._route(path, r"/issues"):
            assignees = cast(list[str], body.get("assignees", []))
            if set(assignees) & self.rejected_assignees:
                raise GitHubApiError(422, "Validation Failed")
            number = self.next_number
            self.next_number += 1
            self.issues[number] = {
                "number": number,
                "state": "open",
                "title": body["title"],
                "body": body["body"],
                "labels": list(cast(list[str], body.get("labels", []))),
                "assignees": list(assignees),
                "user": {"login": nightly_ci.REPORT_BOT, "type": "Bot"},
                "author_association": "NONE",
            }
            self.comments[number] = []
            return {"number": number}
        if match := self._route(path, r"/issues/(\d+)/comments"):
            self.comments[int(match.group(1))].append(cast(str, body["body"]))
            return {"id": 1}
        raise AssertionError(f"unexpected POST {path}")

    def patch(self, path: str, body: Mapping[str, object]) -> object:
        self.writes.append(("PATCH", path, dict(body)))
        match = self._route(path, r"/issues/(\d+)")
        assert match is not None, path
        issue = self.issues[int(match.group(1))]
        issue.update(body)
        if body.get("state") == "closed":
            issue["state"] = "closed"
        return issue


def _report(
    github: FakeGitHub,
    run_id: int,
    *,
    attempt: int | None = None,
    labels: Sequence[str] = ("bug",),
    assignees: Sequence[str] = ("ppenna",),
    dry_run: bool = False,
) -> nightly_ci.Plan:
    with contextlib.redirect_stdout(io.StringIO()):
        plan = nightly_ci.plan_report(
            github,
            nightly_ci.ReportOptions(
                repository=REPOSITORY,
                run_id=run_id,
                run_attempt=attempt,
                assignees=tuple(assignees),
                labels=tuple(labels),
                dry_run=dry_run,
            ),
        )
        if not dry_run:
            nightly_ci.apply_plan(github, REPOSITORY, plan)
    return plan


def _failing_jobs() -> list[dict[str, object]]:
    return [
        _job(
            11,
            "OpenVMM vmm-tests / Windows / WHP",
            "failure",
            [_step(1, "Set up job"), _step(2, "Checkout", "failure")],
            runner="azure-windows-3",
        ),
        _job(
            12,
            "NVX microVM tests / Linux / KVM",
            "failure",
            [
                _step(1, "Set up job"),
                _step(
                    2,
                    "Run NVX microVM tests on Linux",
                    "failure",
                    "2026-10-22T05:30:00Z",
                    "2026-10-22T05:40:00Z",
                ),
            ],
        ),
        _job(
            13,
            "Required status check",
            "failure",
            [_step(1, "Verify required jobs", "failure")],
            runner="GitHub Actions 1",
        ),
        _job(14, "Lint, typing, and formatting", "success", [_step(1, "Checkout")]),
        _job(15, "Publish development release", "skipped"),
    ]


def _add_failing_run(
    github: FakeGitHub, run_id: int, number: int, attempt: int = 1
) -> None:
    github.add_run(_run(run_id, number=number, attempt=attempt), _failing_jobs())
    github.annotations[11] = [
        _annotation(
            "fatal: unable to access 'https://github.com/microsoft/nvx/': "
            "Could not resolve host: github.com"
        )
    ]
    github.annotations[12] = [_annotation("Process completed with exit code 1.")]
    github.annotations[13] = [
        _annotation("nvx-microvm-tests-kvm: expected success, got failure")
    ]
    github.logs[12] = "\n".join(
        (
            "2026-10-22T05:29:59.1000000Z ##[group]Run python3 scripts/nvx.py "
            "test-microvm --backend kvm",
            "2026-10-22T05:29:59.2000000Z python3 scripts/nvx.py test-microvm",
            "2026-10-22T05:29:59.3000000Z ##[endgroup]",
            "2026-10-22T05:31:00.0000000Z error: live filesystem snapshot "
            "capture failed",
            "2026-10-22T05:31:00.1000000Z Running microVM console exit correctness",
            "2026-10-22T05:40:00.2000000Z ##[error]Process completed with exit code 1.",
            "2026-10-22T05:40:00.3000000Z ##[group]Run actions/upload-artifact@v7",
        )
    )


class ClassificationTests(unittest.TestCase):
    def classify(
        self,
        messages: Sequence[str] = (),
        tail: Sequence[str] = (),
        *,
        conclusion: str = "failure",
        steps: Sequence[tuple[str, str | None]] = (
            ("Set up job", "success"),
            ("Run NVX microVM tests on Linux", "failure"),
        ),
        runner: str | None = "azure-kvm-1",
    ) -> nightly_ci.JobFailure:
        failure = nightly_ci.classify_job(
            _parsed_job(conclusion=conclusion, steps=steps, runner=runner),
            messages,
            tail,
        )
        assert failure is not None
        return failure

    def test_lost_and_stopped_runners_are_infrastructure_failures(self):
        for message in (
            "The self-hosted runner: azure-kvm-2 lost communication with the "
            "server. Verify the machine is running and has a healthy network "
            "connection.",
            "The runner has received a shutdown signal. This can happen when the "
            "runner service is stopped, or a manually started runner is canceled.",
            "The hosted runner encountered an error while running your job. "
            "(Error Type: Disconnect).",
            "Received request to deprovision: The request was cancelled by the "
            "remote provider.",
        ):
            with self.subTest(message=message):
                failure = self.classify(
                    [message, "Process completed with exit code 1."],
                    ["error: live filesystem snapshot capture failed"],
                )
                self.assertIs(failure.kind, FailureKind.INFRASTRUCTURE)
                self.assertEqual(failure.cause, "runner lost")
                self.assertEqual(failure.evidence, message)
                self.assertEqual(failure.step, "Run NVX microVM tests on Linux")

    def test_jobs_that_no_runner_picked_up_are_infrastructure_failures(self):
        for message, conclusion in (
            (
                "The job was not acquired by Runner of type hosted even after "
                "multiple attempts",
                "cancelled",
            ),
            (
                "This request was automatically failed because there were no "
                "enabled runners online to process the request for more than 1 "
                "days.",
                "failure",
            ),
        ):
            with self.subTest(message=message):
                failure = self.classify(
                    [message], conclusion=conclusion, steps=(), runner=None
                )
                self.assertIs(failure.kind, FailureKind.INFRASTRUCTURE)
                self.assertEqual(failure.cause, "no runner")
                self.assertIsNone(failure.step)

    def test_only_setup_timeouts_are_infrastructure_failures(self):
        timeout = (
            "The job running on runner azure-windows-4 has exceeded the maximum "
            "execution time of 60 minutes."
        )
        for step, kind, cause in (
            ("Download guest artifacts", FailureKind.INFRASTRUCTURE, "setup timed out"),
            ("Validate runner", FailureKind.INFRASTRUCTURE, "setup timed out"),
            ("Run OpenVMM tests on Windows", FailureKind.TEST, "timed out"),
        ):
            with self.subTest(step=step):
                failure = self.classify(
                    [timeout, "The operation was canceled."],
                    steps=(("Set up job", "success"), (step, "cancelled")),
                )
                self.assertIs(failure.kind, kind)
                self.assertEqual(failure.cause, cause)
                self.assertEqual(failure.step, step)
                self.assertEqual(failure.evidence, timeout)
        failure = self.classify(conclusion="timed_out")
        self.assertIs(failure.kind, FailureKind.TEST)
        self.assertEqual(failure.cause, "timed out")

    def test_setup_step_failures_are_infrastructure_failures(self):
        for step in (
            "Set up job",
            "Checkout",
            "Checkout OpenVMM",
            "Validate runner",
            "Download OpenVMM executable",
            "Install the pinned Rust release",
            "Set up Python",
            "Restore OpenVMM inputs on Linux",
            "Prepare sccache",
            "Make OpenVMM executable",
            "Post Checkout",
        ):
            with self.subTest(step=step):
                # Validate runner's error when a runner lacks the pinned Rust.
                failure = self.classify(
                    [
                        "Rust 1.0.0, which rust-toolchain.toml pins, is not "
                        "installed; rerun scripts/setup/setup-linux.sh --runner-only",
                        "Process completed with exit code 1.",
                    ],
                    steps=(("Set up job", "success"), (step, "failure")),
                )
                self.assertIs(failure.kind, FailureKind.INFRASTRUCTURE)
                self.assertEqual(failure.cause, "setup failed")
                self.assertEqual(failure.step, step)
                self.assertIn("is not installed", failure.evidence or "")

    def test_build_and_test_steps_are_not_setup_steps(self):
        for step in (
            "Validate NVX CLI",
            "Build OpenVMM",
            "Build guest artifacts",
            "Check nvx-rs",
            "Run benchmark",
            "Verify required jobs",
            "Detect relevant changes",
        ):
            with self.subTest(step=step):
                failure = self.classify(
                    ["Process completed with exit code 1."],
                    steps=(("Set up job", "success"), (step, "failure")),
                )
                self.assertIs(failure.kind, FailureKind.TEST)
                self.assertEqual(failure.cause, "failed")

    def test_network_download_and_disk_errors_are_infrastructure_failures(self):
        for line, cause in (
            (
                "error: failed to download from `https://static.crates.io/crates/"
                "libc/libc-0.2.170.crate`",
                "network or download error",
            ),
            (
                "ERROR: Could not install packages due to an OSError: "
                "HTTPSConnectionPool(host='files.pythonhosted.org', port=443): Max "
                "retries exceeded with url: /packages/ruff.whl (Caused by "
                "ReadTimeoutError())",
                "network or download error",
            ),
            (
                "error: failed to download https://cdn.kernel.org/pub/linux/kernel/"
                "linux-6.18.tar.xz: <urlopen error [Errno -3] Temporary failure in "
                "name resolution>",
                "network or download error",
            ),
            (
                "fatal: unable to access 'https://github.com/microsoft/openvmm/': "
                "Could not resolve host: github.com",
                "network or download error",
            ),
            (
                "Error: Unable to download artifact(s): Unable to download and "
                "extract artifact: ECONNRESET",
                "network or download error",
            ),
            (
                "toomanyrequests: You have reached your unauthenticated pull rate "
                "limit.",
                "network or download error",
            ),
            (
                "urllib.error.HTTPError: HTTP Error 503: Service Unavailable",
                "network or download error",
            ),
            ("OSError: [Errno 28] No space left on device", "disk full"),
            ("There is not enough space on the disk.", "disk full"),
        ):
            with self.subTest(line=line):
                failure = self.classify(
                    ["Process completed with exit code 1."],
                    [line, "##[error]Process completed with exit code 1."],
                    steps=(("Set up job", "success"), ("Build OpenVMM", "failure")),
                )
                self.assertIs(failure.kind, FailureKind.INFRASTRUCTURE)
                self.assertEqual(failure.cause, cause)
                self.assertEqual(failure.evidence, line)

    def test_guest_network_errors_remain_test_failures(self):
        # The guest network checks of the microVM tests print curl and wget
        # errors when a policy misbehaves; those are the change's failures.
        for line in (
            "curl: (7) Failed to connect to 10.0.2.2 port 8080: Connection refused",
            "curl: (6) Could not resolve host: denied.example",
            "wget: bad address 'denied.example'",
            "ConnectionResetError: [Errno 104] Connection reset by peer",
            "error: Test timed out",
        ):
            with self.subTest(line=line):
                failure = self.classify(
                    ["Process completed with exit code 1."],
                    [line, "##[error]Process completed with exit code 1."],
                )
                self.assertIs(failure.kind, FailureKind.TEST)
                self.assertEqual(failure.cause, "failed")

    def test_retries_that_recovered_are_no_evidence(self):
        # A warning or a notice of a retry shows a transient error that the
        # retry may have overcome; what failed the step came later.
        for retry in (
            "warning: spurious network error (2 tries remaining): [28] Timeout "
            "was reached",
            "WARNING: Retrying (Retry(total=4, connect=None, read=None)) after "
            "connection broken by 'ReadTimeoutError()': /simple/ruff/",
            ">> download failed (1/3); retrying: <urlopen error [Errno -3] "
            "Temporary failure in name resolution>",
            "Warning: Transient problem: timeout Will retry in 1 seconds. 3 "
            "retries left.",
        ):
            with self.subTest(retry=retry):
                failure = self.classify(
                    ["Process completed with exit code 101."],
                    [
                        retry,
                        "  Downloaded libc v0.2.170",
                        "error[E0425]: cannot find value `pending` in this scope",
                        "error: could not compile `nvx` (lib) due to 1 previous error",
                        "##[error]Process completed with exit code 101.",
                    ],
                    steps=(("Set up job", "success"), ("Check nvx-rs", "failure")),
                )
                self.assertIs(failure.kind, FailureKind.TEST)
                self.assertEqual(
                    failure.evidence,
                    "error: could not compile `nvx` (lib) due to 1 previous error",
                )
        # Retries that never recovered end in an error of their own.
        failure = self.classify(
            ["Process completed with exit code 101."],
            [
                "warning: spurious network error (1 tries remaining): [28] Timeout",
                "error: failed to download from `https://static.crates.io/crates/"
                "libc/libc-0.2.170.crate`",
                "Caused by:",
                "  [28] Timeout was reached",
                "##[error]Process completed with exit code 101.",
            ],
            steps=(("Set up job", "success"), ("Check nvx-rs", "failure")),
        )
        self.assertIs(failure.kind, FailureKind.INFRASTRUCTURE)
        self.assertEqual(failure.cause, "network or download error")
        self.assertTrue(
            (failure.evidence or "").startswith("error: failed to download")
        )

    def test_a_benchmark_runner_that_changed_hosts_is_infrastructure(self):
        # The platform jobs check their runner's identity inside the benchmark
        # step, which a real benchmark failure also fails.
        steps = (("Validate runner", "success"), ("Run benchmark", "failure"))
        changed = (
            "Linux / KVM: this runner differs from "
            "data/linux-kvm-virtual-machine-host.json in 2 required field(s): "
            "cpu_model, vm_size. The series' history comes from the expected host."
        )
        failure = self.classify(
            [changed, "Process completed with exit code 1."],
            [f"##[error]{changed}", "##[error]Process completed with exit code 1."],
            steps=steps,
        )
        self.assertIs(failure.kind, FailureKind.INFRASTRUCTURE)
        self.assertEqual(failure.cause, "benchmark host changed")
        self.assertEqual(failure.evidence, changed)

        missing = "Expected 34 microVM one-vCPU metrics in build/benchmark/result.json"
        failure = self.classify(
            [missing, "Process completed with exit code 1."],
            [f"##[error]{missing}", "##[error]Process completed with exit code 1."],
            steps=steps,
        )
        self.assertIs(failure.kind, FailureKind.TEST)
        self.assertEqual(failure.evidence, missing)

    def test_cancelled_jobs_are_reported_apart(self):
        failure = self.classify(
            ["The operation was canceled."],
            conclusion="cancelled",
            steps=(("Set up job", "success"), ("Build OpenVMM", "cancelled")),
        )
        self.assertIs(failure.kind, FailureKind.CANCELLED)
        self.assertEqual(failure.step, "Build OpenVMM")
        # A job that a cancelled run stopped in the middle of a step fails
        # with that step cancelled and no error of its own.
        failure = self.classify(
            ["The operation was canceled."],
            steps=(
                ("Set up job", "success"),
                ("Run OpenVMM tests on MSHV", "cancelled"),
            ),
        )
        self.assertIs(failure.kind, FailureKind.CANCELLED)
        self.assertEqual(failure.cause, "cancelled")

    def test_successful_and_skipped_jobs_did_not_fail(self):
        for conclusion in ("success", "skipped", "neutral", None):
            with self.subTest(conclusion=conclusion):
                self.assertIsNone(
                    nightly_ci.classify_job(_parsed_job(conclusion=conclusion), [])
                )

    def test_failures_cite_their_most_specific_evidence(self):
        failure = self.classify(
            ["Process completed with exit code 1."],
            [
                "error: live filesystem snapshot capture failed",
                "Running microVM console exit correctness (1 vCPU) on OpenVMM/kvm",
                "##[error]Process completed with exit code 1.",
            ],
        )
        self.assertIs(failure.kind, FailureKind.TEST)
        self.assertEqual(
            failure.evidence, "error: live filesystem snapshot capture failed"
        )
        # The step's own error annotations, which its log repeats, come first.
        failure = self.classify(
            [
                "nvx-microvm-tests-kvm: expected success, got skipped",
                "Process completed with exit code 1.",
            ],
            [
                "error: unrelated",
                "##[error]nvx-microvm-tests-kvm: expected success, got skipped",
                "##[error]Process completed with exit code 1.",
            ],
        )
        self.assertEqual(
            failure.evidence, "nvx-microvm-tests-kvm: expected success, got skipped"
        )
        # Without the step's log, the job's annotations stand in for it.
        failure = self.classify(
            [
                "Process completed with exit code 1.",
                "nvx-microvm-tests-kvm: expected success, got skipped",
            ]
        )
        self.assertEqual(
            failure.evidence, "nvx-microvm-tests-kvm: expected success, got skipped"
        )
        failure = self.classify(["Process completed with exit code 2."])
        self.assertEqual(failure.evidence, "Process completed with exit code 2.")

    def test_errors_of_later_steps_do_not_hide_a_test_failure(self):
        # The microVM and OpenVMM jobs upload their diagnostics after a
        # failure, so an upload that fails on the network or a full disk
        # annotates the job too; only the failing step's log classifies it.
        for message in (
            "Failed to upload artifact: read ECONNRESET",
            "Unable to upload artifact: HTTP Error 503: Service Unavailable",
            "No space left on device",
        ):
            with self.subTest(message=message):
                failure = self.classify(
                    ["Process completed with exit code 1.", message],
                    [
                        "error: live filesystem snapshot capture failed",
                        "##[error]Process completed with exit code 1.",
                    ],
                )
                self.assertIs(failure.kind, FailureKind.TEST)
                self.assertEqual(failure.cause, "failed")
                self.assertEqual(
                    failure.evidence, "error: live filesystem snapshot capture failed"
                )

    def test_a_job_that_never_started_without_runner_errors_is_not_dismissed(self):
        failure = self.classify(
            ["Error when evaluating 'runs-on' for job 'platform-kvm'."],
            steps=(),
            runner=None,
        )
        self.assertIs(failure.kind, FailureKind.TEST)
        self.assertEqual(failure.cause, "never started")


class StepLogTailTests(unittest.TestCase):
    def test_tail_is_the_failing_step_up_to_its_first_error(self):
        log = "\n".join(
            (
                "2026-10-22T05:00:00.1Z ##[group]Run make before",
                "2026-10-22T05:00:00.2Z make before",
                "2026-10-22T05:00:00.3Z ##[endgroup]",
                "2026-10-22T05:00:09.0Z output of the step before",
                "2026-10-22T05:00:10.1Z ##[group]Run make test",
                "2026-10-22T05:00:10.2Z make test",
                "2026-10-22T05:00:10.3Z shell: /usr/bin/bash -e {0}",
                "2026-10-22T05:00:10.4Z ##[endgroup]",
                "2026-10-22T05:00:11.0Z \x1b[31mFAILED\x1b[0m test_boot",
                "2026-10-22T05:00:12.0Z ##[error]boot failed",
                "2026-10-22T05:00:12.1Z ##[error]Process completed with exit code 1.",
                "2026-10-22T05:00:12.2Z ##[group]Run actions/upload-artifact@v7",
                "2026-10-22T05:00:12.3Z with:",
                "2026-10-22T05:00:12.4Z ##[endgroup]",
                "2026-10-22T05:00:12.5Z ##[error]upload failed",
            )
        )
        step = Step(
            2, "Run tests", "failure", "2026-10-22T05:00:10Z", "2026-10-22T05:00:12Z"
        )
        self.assertEqual(
            nightly_ci.step_log_tail(log, step),
            (
                "FAILED test_boot",
                "##[error]boot failed",
                "##[error]Process completed with exit code 1.",
            ),
        )

    def test_tail_without_step_times_reads_the_whole_log(self):
        log = "\n".join(
            (
                "2026-10-22T05:00:09.0Z first",
                "2026-10-22T05:00:10.0Z second",
                "2026-10-22T05:00:11.0Z ##[error]third",
                "2026-10-22T05:00:12.0Z after",
            )
        )
        expected = ("first", "second", "##[error]third")
        self.assertEqual(nightly_ci.step_log_tail(log, None), expected)
        untimed = Step(1, "Build", "failure", None, None)
        self.assertEqual(nightly_ci.step_log_tail(log, untimed), expected)
        # Without timestamps in the log, the whole log stands in for the step.
        timed = Step(
            1, "Build", "failure", "2026-10-21T00:00:00Z", "2026-10-21T00:00:01Z"
        )
        self.assertEqual(
            nightly_ci.step_log_tail("first\nsecond\n##[error]third\nafter", timed),
            expected,
        )

    def test_a_log_without_the_steps_lines_has_no_tail(self):
        # A long log that lost its start keeps only later steps, such as a
        # failed upload of diagnostics, which must not stand in for the step.
        log = "\n".join(
            (
                "2026-10-22T05:41:00.0Z ##[group]Run actions/upload-artifact@v7",
                "2026-10-22T05:41:00.1Z ##[endgroup]",
                "2026-10-22T05:41:30.0Z ##[error]Failed to upload artifact: read "
                "ECONNRESET",
            )
        )
        step = Step(
            2,
            "Run NVX microVM tests on Linux",
            "failure",
            "2026-10-22T05:30:00Z",
            "2026-10-22T05:40:00Z",
        )
        self.assertEqual(nightly_ci.step_log_tail(log, step), ())

    def test_tail_keeps_the_last_lines_and_shortens_long_ones(self):
        lines = [f"2026-10-22T05:00:10.0Z line {index}" for index in range(40)]
        lines.append("2026-10-22T05:00:10.0Z " + "x" * 500)
        tail = nightly_ci.step_log_tail("\n".join(lines), None)
        self.assertEqual(len(tail), nightly_ci.EXCERPT_LINES)
        self.assertEqual(tail[0], f"line {41 - nightly_ci.EXCERPT_LINES}")
        self.assertEqual(len(tail[-1]), nightly_ci.EXCERPT_LINE_LENGTH)
        self.assertTrue(tail[-1].endswith("..."))


class ReportTests(unittest.TestCase):
    def test_failing_run_opens_one_report(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)

        plan = _report(github, 5001)

        self.assertEqual(plan.outcome, "run #50 failed; opening a report")
        [issue] = github.open_reports()
        self.assertEqual(issue["title"], nightly_ci.REPORT_TITLE)
        self.assertEqual(issue["labels"], ["bug"])
        self.assertEqual(issue["assignees"], ["ppenna"])
        body = cast(str, issue["body"])
        self.assertTrue(body.startswith(nightly_ci.REPORT_MARKER))
        self.assertIn("Triage owner: @ppenna", body)
        self.assertIn(
            f"[Run #50]({REPOSITORY_URL}/actions/runs/5001/attempts/1), attempt 1, "
            f"of `dev` at [`0123456789`]({REPOSITORY_URL}/commit/"
            "0123456789abcdef0123456789abcdef01234567) finished on 2026-10-22 "
            "05:58 UTC with conclusion `failure`: 1 infrastructure failure, 1 test "
            "or build failure, and 0 cancelled jobs.",
            body,
        )
        infrastructure = body.split("### Infrastructure failures", 1)[1].split(
            "###", 1
        )[0]
        self.assertIn(
            f"| [OpenVMM vmm-tests / Windows / WHP]({REPOSITORY_URL}/actions/runs/"
            "5001/job/11) | Checkout | setup failed | `fatal: unable to access "
            "'https://github.com/microsoft/nvx/': Could not resolve host: "
            "github.com` |",
            infrastructure,
        )
        tests = body.split("### Test and build failures", 1)[1]
        self.assertIn(
            "| Run NVX microVM tests on Linux | failed | `error: live filesystem "
            "snapshot capture failed` |",
            tests,
        )
        self.assertIn(
            f"[Required status check]({REPOSITORY_URL}/actions/runs/5001/job/13), "
            "which checks the other jobs' results, also failed.",
            body,
        )
        self.assertNotIn("Lint, typing, and formatting", body)
        self.assertIn(
            "<summary>Last log lines of Run NVX microVM tests on Linux in NVX "
            "microVM tests / Linux / KVM</summary>",
            body,
        )
        self.assertNotIn("python3 scripts/nvx.py test-microvm", body)
        self.assertNotIn("upload-artifact", body)
        self.assertIn(
            f"| [#50, attempt 1]({REPOSITORY_URL}/actions/runs/5001/attempts/1) "
            f"| [`0123456789`]({REPOSITORY_URL}/commit/"
            "0123456789abcdef0123456789abcdef01234567) | 2026-10-22 05:58 UTC "
            "| `failure` | 1 | 1 | 0 |",
            body,
        )
        self.assertEqual(github.comments[700], [])
        # Cancelled jobs, jobs that never ran, and the summary job explain
        # themselves without a log.
        self.assertEqual(github.log_requests, [12, 11])

    def test_a_missing_label_is_left_out(self):
        github = FakeGitHub()
        github.labels = set()
        _add_failing_run(github, 5001, 50)
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            plan = nightly_ci.plan_report(
                github,
                nightly_ci.ReportOptions(REPOSITORY, 5001, labels=("bug",)),
            )
        [action] = plan.actions
        assert isinstance(action, nightly_ci.CreateIssue)
        self.assertEqual(action.labels, ())
        self.assertIn("label 'bug' does not exist", stdout.getvalue())
        # Creating an issue with a missing label would create the label.
        self.assertFalse(any(path.endswith("/labels") for _, path, _ in github.writes))

    def test_a_label_that_cannot_be_looked_up_is_left_out(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        stdout = io.StringIO()
        with (
            patch.object(
                github, "exists", side_effect=GitHubApiError(403, "Forbidden")
            ),
            contextlib.redirect_stdout(stdout),
        ):
            plan = nightly_ci.plan_report(
                github,
                nightly_ci.ReportOptions(REPOSITORY, 5001, labels=("bug",)),
            )
        [action] = plan.actions
        assert isinstance(action, nightly_ci.CreateIssue)
        self.assertEqual(action.labels, ())
        self.assertIn(
            "cannot look up label 'bug'; omitting it: Forbidden", stdout.getvalue()
        )

    def test_a_rejected_assignee_does_not_block_the_report(self):
        github = FakeGitHub()
        github.rejected_assignees = {"ppenna"}
        _add_failing_run(github, 5001, 50)

        _report(github, 5001)

        [issue] = github.open_reports()
        self.assertEqual(issue["assignees"], [])
        self.assertIn("Triage owner: @ppenna", cast(str, issue["body"]))

    def test_repeated_failures_update_the_same_report(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        _report(github, 5001)
        _add_failing_run(github, 5002, 51)

        plan = _report(github, 5002)

        self.assertEqual(plan.outcome, "run #51 failed; updating #700")
        [issue] = github.open_reports()
        body = cast(str, issue["body"])
        self.assertIn("[Run #51]", body.split("## Failing runs", 1)[0])
        history = body.split("## Failing runs", 1)[1]
        self.assertLess(
            history.index("[#51, attempt 1]"), history.index("[#50, attempt 1]")
        )
        [comment] = github.comments[700]
        self.assertTrue(
            comment.startswith("The nightly full run failed again.\n\n[Run #51]")
        )
        self.assertNotIn(nightly_ci.REPORT_MARKER, comment)
        history_entries = nightly_ci.parse_history(body)
        assert history_entries is not None
        self.assertEqual([entry.run_id for entry in history_entries], [5001, 5002])

    def test_reporting_the_same_attempt_again_adds_nothing(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        _report(github, 5001)
        _add_failing_run(github, 5002, 51)
        _report(github, 5002)
        writes = len(github.writes)

        plan = _report(github, 5002)

        self.assertEqual(plan.outcome, "run #51 attempt 1 is already in #700")
        self.assertEqual(len(github.writes), writes)
        [issue] = github.open_reports()
        self.assertEqual(len(github.comments[700]), 1)
        history = nightly_ci.parse_history(cast(str, issue["body"]))
        assert history is not None
        self.assertEqual([entry.run_id for entry in history], [5001, 5002])

        # A report that someone edited is restored without another comment.
        issue["body"] = cast(str, issue["body"]).replace("Triage owner", "Owner")

        plan = _report(github, 5002)

        self.assertEqual(
            plan.outcome, "run #51 attempt 1 is already in #700; refreshing it"
        )
        self.assertIn("Triage owner", cast(str, github.issues[700]["body"]))
        self.assertEqual(len(github.comments[700]), 1)

    def test_a_failed_rerun_is_a_new_failure(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        _report(github, 5001)
        _add_failing_run(github, 5001, 50, attempt=2)

        _report(github, 5001, attempt=2)

        [issue] = github.open_reports()
        history = nightly_ci.parse_history(cast(str, issue["body"]))
        assert history is not None
        self.assertEqual(
            [(entry.run_id, entry.attempt) for entry in history], [(5001, 1), (5001, 2)]
        )
        self.assertEqual(len(github.comments[700]), 1)

    def test_a_passing_run_closes_the_report(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        _report(github, 5001)
        github.add_run(_run(5002, number=51, conclusion="success"))

        plan = _report(github, 5002)

        self.assertEqual(plan.outcome, "run #51 passed; closing #700")
        self.assertEqual(github.open_reports(), [])
        self.assertEqual(github.issues[700]["state_reason"], "completed")
        [comment] = github.comments[700]
        self.assertIn(
            f"[Run #51]({REPOSITORY_URL}/actions/runs/5002/attempts/1), attempt 1, "
            "passed",
            comment,
        )
        self.assertIn("after 1 failing run attempt.", comment)
        # The issue closes before the comment, so a retry cannot comment twice.
        self.assertEqual(
            [(method, path) for method, path, _ in github.writes[-2:]],
            [
                ("PATCH", f"/repos/{REPOSITORY}/issues/700"),
                ("POST", f"/repos/{REPOSITORY}/issues/700/comments"),
            ],
        )

        # The next failure opens a new report.
        _add_failing_run(github, 5003, 52)
        _report(github, 5003)
        [issue] = github.open_reports()
        self.assertEqual(issue["number"], 701)

    def test_a_passing_run_without_a_report_changes_nothing(self):
        github = FakeGitHub()
        github.add_run(_run(5001, conclusion="success"))

        plan = _report(github, 5001)

        self.assertEqual(plan.outcome, "run #50 passed; no report is open")
        self.assertEqual(github.writes, [])

    def test_runs_that_neither_pass_nor_fail_change_nothing(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        _report(github, 5001)
        writes = len(github.writes)
        for conclusion in ("neutral", "skipped", "stale"):
            with self.subTest(conclusion=conclusion):
                github.add_run(_run(5002, number=51, conclusion=conclusion))
                plan = _report(github, 5002)
                self.assertEqual(plan.actions, ())
                self.assertEqual(len(github.writes), writes)
        self.assertEqual(len(github.open_reports()), 1)

    def test_cancelled_and_timed_out_runs_are_failures(self):
        for conclusion in ("cancelled", "timed_out", "startup_failure"):
            with self.subTest(conclusion=conclusion):
                github = FakeGitHub()
                github.add_run(_run(5001, conclusion=conclusion))
                plan = _report(github, 5001)
                self.assertEqual(plan.outcome, "run #50 failed; opening a report")
                [issue] = github.open_reports()
                self.assertIn("No job failed.", cast(str, issue["body"]))

    def test_duplicate_reports_converge_on_the_oldest(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        _report(github, 5001)
        # A maintainer may open a report too, for example with a token of
        # their own.
        github.add_issue(650)
        _add_failing_run(github, 5002, 51)

        plan = _report(github, 5002)

        self.assertEqual(plan.outcome, "run #51 failed; updating #650")
        self.assertEqual([issue["number"] for issue in github.open_reports()], [650])
        self.assertEqual(github.issues[700]["state_reason"], "not_planned")
        self.assertEqual(
            github.comments[700],
            ["#650 tracks this failure; closing this duplicate.\n"],
        )
        # #650 had no history, so it starts a new one with this run.
        history = nightly_ci.parse_history(cast(str, github.issues[650]["body"]))
        assert history is not None
        self.assertEqual([entry.run_id for entry in history], [5002])

    def test_issues_that_copy_the_marker_are_not_reports(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        _report(github, 5001)
        for number, association in ((650, "NONE"), (651, "CONTRIBUTOR")):
            github.add_issue(number, login="mallory", association=association)
        _add_failing_run(github, 5002, 51)

        plan = _report(github, 5002)

        self.assertEqual(plan.outcome, "run #51 failed; updating #700")
        self.assertEqual(
            [issue["number"] for issue in github.open_reports()], [650, 651, 700]
        )
        self.assertEqual(github.issues[650]["body"], nightly_ci.REPORT_MARKER)
        self.assertEqual(github.comments[650], [])

        github.add_run(_run(5003, number=52, conclusion="success"))
        _report(github, 5003)
        self.assertEqual(
            [issue["number"] for issue in github.open_reports()], [650, 651]
        )

    def test_an_issue_that_quotes_the_marker_is_not_a_report(self):
        quoting = (
            "Design note: the reporter finds its tracking issue by the hidden "
            f"marker `{nightly_ci.REPORT_MARKER}`. Please keep this issue open."
        )
        # A green night does not close the quoting issue.
        green = FakeGitHub()
        green.add_issue(650, quoting, association="MEMBER")
        green.add_run(_run(5001, number=50, conclusion="success"))

        plan = _report(green, 5001)

        self.assertEqual(plan.outcome, "run #50 passed; no report is open")
        self.assertEqual(green.issues[650]["state"], "open")
        self.assertEqual(green.comments[650], [])

        # A red night opens a new report instead of taking the quoting issue
        # over, even when the quoting issue is older than any report.
        red = FakeGitHub()
        red.add_issue(650, quoting, association="MEMBER")
        _add_failing_run(red, 5001, 50)

        plan = _report(red, 5001)

        self.assertEqual(plan.outcome, "run #50 failed; opening a report")
        self.assertEqual(red.issues[650]["state"], "open")
        self.assertEqual(red.issues[650]["body"], quoting)
        self.assertEqual([issue["number"] for issue in red.open_reports()], [650, 700])
        self.assertEqual(red.issues[700]["title"], nightly_ci.REPORT_TITLE)

        # The next red night updates the real report and does not close the
        # quoting issue as a duplicate.
        red.add_issue(800, quoting, association="MEMBER")
        _add_failing_run(red, 5002, 51)

        plan = _report(red, 5002)

        self.assertEqual(plan.outcome, "run #51 failed; updating #700")
        self.assertEqual(red.issues[800]["state"], "open")
        self.assertEqual(red.issues[800]["body"], quoting)
        self.assertEqual(red.comments[800], [])

    def test_a_passing_run_closes_every_open_report(self):
        github = FakeGitHub()
        for number in (650, 651):
            github.add_issue(number, association="OWNER")
        github.add_run(_run(5001, conclusion="success"))

        _report(github, 5001)

        self.assertEqual(github.open_reports(), [])

    def test_history_is_bounded(self):
        github = FakeGitHub()
        for index in range(nightly_ci.MAX_HISTORY + 3):
            _add_failing_run(github, 6000 + index, 100 + index)
            _report(github, 6000 + index)
        [issue] = github.open_reports()
        history = nightly_ci.parse_history(cast(str, issue["body"]))
        assert history is not None
        self.assertEqual(len(history), nightly_ci.MAX_HISTORY)
        self.assertEqual(history[0].run_id, 6003)
        self.assertEqual(history[-1].run_id, 6000 + nightly_ci.MAX_HISTORY + 2)

    def test_the_report_follows_the_latest_nightly_result(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        _report(github, 5001)
        # Run #51 passed, but its reporter waited behind another reporter and
        # a rerun of #50 replaced it, as a concurrency group does; the
        # reporter of #50's rerun reports #51's result instead of its own.
        github.add_run(_run(5002, number=51, conclusion="success"))
        _add_failing_run(github, 5001, 50, attempt=2)

        plan = _report(github, 5001, attempt=2)

        self.assertEqual(plan.outcome, "run #51 passed; closing #700")
        self.assertEqual(
            github.listed_runs,
            {"event": "schedule", "branch": "dev", "per_page": "20"},
        )
        self.assertEqual(github.issues[700]["state"], "closed")

    def test_the_report_follows_the_latest_attempt(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        github.add_run(_run(5001, number=50, attempt=2, conclusion="success"))

        plan = _report(github, 5001, attempt=1)

        self.assertEqual(plan.outcome, "run #50 passed; no report is open")
        with self.assertRaisesRegex(ScriptError, "has no attempt 3"):
            _report(github, 5001, attempt=3)
        self.assertEqual(github.writes, [])

    def test_older_results_leave_the_report_while_a_newer_run_is_rerun(self):
        # Run #51 failed and is being rerun, so it is in progress again, but
        # its first attempt is still the latest result: an older run's result
        # neither closes nor rewrites the report.
        for conclusion in ("success", "failure"):
            with self.subTest(conclusion=conclusion):
                github = FakeGitHub()
                _add_failing_run(github, 5002, 51)
                _report(github, 5002)
                body = github.issues[700]["body"]
                writes = len(github.writes)
                github.runs[5002].update(
                    status="in_progress", conclusion=None, run_attempt=2
                )
                github.add_run(_run(5001, number=50, conclusion=conclusion))

                plan = _report(github, 5001)

                self.assertEqual(plan.outcome, "run #51 attempt 1 is already in #700")
                self.assertEqual(github.issues[700]["state"], "open")
                self.assertEqual(github.issues[700]["body"], body)
                self.assertEqual(len(github.writes), writes)

    def test_older_results_are_stale_when_the_report_records_a_newer_run(self):
        for conclusion in ("success", "failure"):
            with self.subTest(conclusion=conclusion):
                github = FakeGitHub()
                _add_failing_run(github, 5002, 51)
                _report(github, 5002)
                # GitHub no longer lists #51, for example because someone
                # deleted it.
                del github.runs[5002]
                github.add_run(_run(5001, number=50, conclusion=conclusion))

                plan = _report(github, 5001)

                self.assertEqual(
                    plan.outcome,
                    "skipped: run #50 is older than run #51, which the report records",
                )
                self.assertEqual(github.issues[700]["state"], "open")
                self.assertEqual(github.comments[700], [])

    def test_a_newer_run_without_a_result_does_not_supersede(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        github.add_run(_run(5002, number=51, status="in_progress", conclusion=None))

        plan = _report(github, 5001)

        self.assertEqual(plan.outcome, "run #50 failed; opening a report")

    def test_only_completed_scheduled_runs_are_reported(self):
        github = FakeGitHub()
        github.add_run(_run(5001, event="pull_request"), _failing_jobs())
        with self.assertRaisesRegex(ScriptError, "is a pull_request run"):
            _report(github, 5001)
        # A dry run may preview the report of any run.
        plan = _report(github, 5001, dry_run=True)
        self.assertEqual(plan.outcome, "run #50 failed; opening a report")
        github.add_run(_run(5002, number=51, status="in_progress", conclusion=None))
        with self.assertRaisesRegex(ScriptError, "has not completed"):
            _report(github, 5002)
        self.assertEqual(github.writes, [])

    def test_job_errors_degrade_to_what_the_report_can_show(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        github.log_errors = {12}

        _report(github, 5001)

        [issue] = github.open_reports()
        body = cast(str, issue["body"])
        self.assertIn(
            "| Run NVX microVM tests on Linux | failed | `Process completed with "
            "exit code 1.` |",
            body,
        )
        self.assertNotIn("<details>", body.split("Required status check", 1)[1])

    def test_another_steps_log_does_not_classify_the_failure(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        # The retained log holds only the diagnostics upload that followed
        # the failing test step.
        github.logs[12] = "\n".join(
            (
                "2026-10-22T05:41:00.0Z ##[group]Run actions/upload-artifact@v7",
                "2026-10-22T05:41:00.1Z ##[endgroup]",
                "2026-10-22T05:41:30.0Z ##[error]Failed to upload artifact: read "
                "ECONNRESET",
            )
        )
        github.annotations[12] = [
            _annotation("Process completed with exit code 1."),
            _annotation("Failed to upload artifact: read ECONNRESET"),
        ]

        _report(github, 5001)

        [issue] = github.open_reports()
        body = cast(str, issue["body"])
        self.assertIn(
            "| Run NVX microVM tests on Linux | failed | `Failed to upload artifact: "
            "read ECONNRESET` |",
            body.split("### Test and build failures", 1)[1],
        )

    def test_a_partial_rerun_reports_the_failures_that_it_did_not_repeat(self):
        # A rerun of one job lists the run's other jobs in its attempt too,
        # with their earlier results, as run 39524517026's second attempt
        # does, so a failure that was not rerun stays in the report.
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        _report(github, 5001)
        jobs = _failing_jobs()
        jobs[1].update(
            id=22,
            conclusion="success",
            steps=[_step(1, "Set up job"), _step(2, "Run NVX microVM tests on Linux")],
        )
        github.add_run(_run(5001, number=50, attempt=2), jobs)

        _report(github, 5001, attempt=2)

        [issue] = github.open_reports()
        body = cast(str, issue["body"])
        latest = body.split("## Latest failing run", 1)[1].split("## Failing runs")[0]
        self.assertIn("OpenVMM vmm-tests / Windows / WHP", latest)
        self.assertNotIn("NVX microVM tests / Linux / KVM", latest)
        self.assertIn("1 infrastructure failure, 0 test or build failures", latest)

    def test_logs_are_read_for_a_bounded_number_of_jobs(self):
        github = FakeGitHub()
        jobs = [
            _job(
                100 + index,
                f"Job {index:02}",
                "failure",
                [_step(1, "Run tests", "failure")],
            )
            for index in range(nightly_ci.MAX_LOGGED_JOBS + 2)
        ]
        github.add_run(_run(5001), jobs)

        _report(github, 5001)

        self.assertEqual(len(github.log_requests), nightly_ci.MAX_LOGGED_JOBS)

    def test_annotations_and_logs_share_a_time_budget(self):
        github = FakeGitHub()
        jobs = [
            _job(
                100 + index,
                f"Job {index}",
                "failure",
                [_step(1, "Run tests", "failure")],
            )
            for index in range(4)
        ]
        github.add_run(_run(5001), jobs)
        github.log_seconds = 100.0
        stdout = io.StringIO()

        with contextlib.redirect_stdout(stdout):
            report = nightly_ci.build_run_report(
                github,
                REPOSITORY,
                nightly_ci._parse_run(github.runs[5001]),
                budget=240.0,
                clock=lambda: github.now,
            )

        # Each log takes 100 s, so the fourth job starts after the budget and
        # is classified from its steps alone.
        self.assertEqual(github.annotation_requests, [100, 101, 102])
        self.assertEqual(github.log_requests, [100, 101, 102])
        self.assertEqual(
            [(failure.name, failure.kind) for failure in report.failures],
            [(f"Job {index}", FailureKind.TEST) for index in range(4)],
        )
        self.assertIn("took over 240 s", stdout.getvalue())

    def test_a_summary_job_that_fails_alone_is_the_failure(self):
        github = FakeGitHub()
        github.add_run(
            _run(5001),
            [
                _job(
                    13,
                    "Required status check",
                    "failure",
                    [_step(1, "Verify required jobs", "failure")],
                )
            ],
        )
        github.annotations[13] = [
            _annotation("nvx-microvm-debug-kvm: expected success, got skipped")
        ]

        _report(github, 5001)

        [issue] = github.open_reports()
        body = cast(str, issue["body"])
        self.assertIn(
            "| Verify required jobs | failed | `nvx-microvm-debug-kvm: expected "
            "success, got skipped` |",
            body,
        )
        self.assertNotIn("also failed.", body)

    def test_rendering_quotes_untrusted_text(self):
        report = nightly_ci.RunReport(
            nightly_ci._parse_run(_run()),
            (
                nightly_ci.JobFailure(
                    name="Job [x] | y",
                    url=f"{REPOSITORY_URL}/actions/runs/5001/job/1",
                    kind=FailureKind.TEST,
                    cause="failed",
                    step="Run | tests",
                    evidence="@octocat broke `main` | #12",
                    excerpt=("``` @octocat", "<script>"),
                ),
            ),
        )

        details = nightly_ci.render_run_details(report)

        self.assertIn(
            f"| [Job \\[x\\] \\| y]({REPOSITORY_URL}/actions/runs/5001/job/1) "
            "| Run \\| tests | failed | ``@octocat broke `main` \\| #12`` |",
            details,
        )
        self.assertIn("````text\n``` @octocat\n<script>\n````", details)
        self.assertIn(
            "<summary>Last log lines of Run | tests in Job [x] | y</summary>", details
        )

    def test_reports_fit_within_github_body_limits(self):
        failures = tuple(
            nightly_ci.JobFailure(
                name=f"Job {index}",
                url=f"{REPOSITORY_URL}/actions/runs/5001/job/{index}",
                kind=FailureKind.TEST,
                cause="failed",
                step="Run tests",
                evidence="e" * 1000,
                excerpt=tuple("x" * 200 for _ in range(15)),
            )
            for index in range(200)
        )
        report = nightly_ci.RunReport(nightly_ci._parse_run(_run()), failures)
        entry = nightly_ci.HistoryEntry.from_report(report)

        body = nightly_ci.render_issue_body(report, [entry] * 30, ["ppenna"])
        comment = nightly_ci.render_repeat_comment(report)

        for text in (body, comment):
            self.assertLessEqual(len(text), nightly_ci.MAX_BODY_LENGTH)
            self.assertNotIn("<details>", text)
            self.assertIn("\n| and 180 more | | | |\n", text)
        self.assertEqual(nightly_ci.parse_history(body), [entry] * 30)

    def test_history_survives_its_round_trip(self):
        entry = nightly_ci.HistoryEntry(
            run_id=1,
            number=2,
            attempt=3,
            sha="a" * 40,
            finished_at="2026-10-22T05:58:12Z",
            conclusion="failure-->",
            infrastructure=4,
            test=5,
            cancelled=6,
        )
        comment = nightly_ci._state_comment([entry])
        self.assertEqual(comment.count("-->"), 1)
        self.assertEqual(nightly_ci.parse_history(comment), [entry])
        for body in (
            "no state",
            "<!-- nvx-nightly-ci-state: {not json} -->",
            '<!-- nvx-nightly-ci-state: {"version":2,"runs":[]} -->',
            '<!-- nvx-nightly-ci-state: {"version":1,"runs":[{"run_id":1}]} -->',
        ):
            with self.subTest(body=body):
                self.assertIsNone(nightly_ci.parse_history(body))


class _FakeResponse(io.BytesIO):
    def __init__(self, payload: bytes, link: str | None = None) -> None:
        super().__init__(payload)
        self.headers = Message()
        if link is not None:
            self.headers["Link"] = link

    def __enter__(self) -> _FakeResponse:
        return self


class _FakeOpener:
    """Answers requests in order and records them."""

    def __init__(self, *responses: object) -> None:
        self.responses = list(responses)
        self.requests: list[urllib.request.Request] = []
        self.timeouts: list[float | None] = []

    def open(
        self, request: urllib.request.Request, timeout: float | None = None
    ) -> _FakeResponse:
        self.requests.append(request)
        self.timeouts.append(timeout)
        response = self.responses.pop(0)
        if isinstance(response, BaseException):
            raise response
        return cast(_FakeResponse, response)


class _SlowResponse(_FakeResponse):
    """Sends four bytes at a time, each ten seconds after the last."""

    def __init__(self, payload: bytes, now: list[float]) -> None:
        super().__init__(payload)
        self.now = now

    def read1(self, size: int | None = -1, /) -> bytes:
        self.now[0] += 10.0
        return super().read1(4)


class _TricklingResponse(_FakeResponse):
    """Holds a read until released, as a server that trickles data can."""

    def __init__(self) -> None:
        super().__init__(b"")
        self.released = threading.Event()

    def read(self, size: int | None = -1, /) -> bytes:
        return b"" if self.released.wait(10.0) else b"x"

    def read1(self, size: int | None = -1, /) -> bytes:
        return self.read(size)


def _http_error(status: int, message: str = "") -> urllib.error.HTTPError:
    payload = json.dumps({"message": message}).encode() if message else b""
    return urllib.error.HTTPError(
        "https://api.github.com/x", status, message, Message(), io.BytesIO(payload)
    )


class GitHubClientTests(unittest.TestCase):
    def client(
        self, opener: _FakeOpener, token: str | None = "secret"
    ) -> tuple[nightly_ci.GitHubClient, list[float]]:
        sleeps: list[float] = []
        client = nightly_ci.GitHubClient(
            token,
            "https://api.example.test/",
            opener=cast(urllib.request.OpenerDirector, opener),
            sleep=sleeps.append,
            log_limit=8,
        )
        return client, sleeps

    def timed_client(
        self, opener: _FakeOpener, now: list[float]
    ) -> nightly_ci.GitHubClient:
        def sleep(seconds: float) -> None:
            now[0] += seconds

        return nightly_ci.GitHubClient(
            "secret",
            "https://api.example.test/",
            opener=cast(urllib.request.OpenerDirector, opener),
            sleep=sleep,
            clock=lambda: now[0],
        )

    def test_reads_finish_by_their_deadline(self):
        now = [100.0]
        opener = _FakeOpener(_http_error(502), _http_error(502), _FakeResponse(b"[]"))
        client = self.timed_client(opener, now)

        # A request waits no longer than the time left, and a retry whose
        # backoff would end after the deadline is not made.
        with self.assertRaisesRegex(GitHubApiError, "HTTP 502"):
            client.get_all("/annotations", deadline=101.5)
        self.assertEqual(opener.timeouts, [1.5])
        self.assertEqual(now, [100.0])
        self.assertEqual(client.get_all("/annotations"), [])
        self.assertEqual(opener.timeouts[1:], [60.0, 60.0])
        self.assertEqual(now, [102.0])

        # Without time left, nothing is sent.
        with self.assertRaisesRegex(ScriptError, "GET /log ran out of time"):
            client.get_log("/log", deadline=now[0])
        self.assertEqual(len(opener.requests), 3)

    def test_a_log_that_arrives_too_slowly_is_abandoned(self):
        now = [0.0]
        opener = _FakeOpener(_SlowResponse(b"0123456789", now))
        client = self.timed_client(opener, now)

        with self.assertRaisesRegex(ScriptError, "GET /log ran out of time"):
            client.get_log("/log", deadline=25.0)
        self.assertEqual(len(opener.requests), 1)

    def test_a_trickling_log_ends_at_the_deadline(self):
        # A socket timeout bounds each wait for data, so a server that keeps
        # sending a little at a time can hold one read for a long time; only
        # the deadline ends the download.
        response = _TricklingResponse()
        client, _ = self.client(_FakeOpener(response))
        started = time.monotonic()
        try:
            with self.assertRaisesRegex(
                ScriptError, "GitHub GET /log failed: GitHub took too long"
            ):
                client.get_log("/log", deadline=started + 0.5)
        finally:
            response.released.set()
        self.assertLess(time.monotonic() - started, 5.0)

    def test_trickling_annotations_and_errors_end_at_the_deadline(self):
        # Annotations and an error's body are bounded the same way as logs.
        def annotations(trickle: _TricklingResponse) -> object:
            return trickle

        def error_body(trickle: _TricklingResponse) -> object:
            return urllib.error.HTTPError(
                "https://api.example.test/x", 403, "Forbidden", Message(), trickle
            )

        for make in (annotations, error_body):
            with self.subTest(response=make.__name__):
                trickle = _TricklingResponse()
                client, _ = self.client(_FakeOpener(make(trickle)))
                started = time.monotonic()
                try:
                    with self.assertRaisesRegex(
                        ScriptError,
                        "GitHub GET /annotations failed: GitHub took too long",
                    ):
                        client.get_all("/annotations", deadline=started + 0.5)
                finally:
                    trickle.released.set()
                self.assertLess(time.monotonic() - started, 5.0)

    def test_requests_name_the_api_version_and_carry_the_token(self):
        opener = _FakeOpener(_FakeResponse(b'{"id": 1}'), _FakeResponse(b"{}"))
        client, _ = self.client(opener)

        self.assertEqual(
            client.get("/repos/o/r/actions/runs/1", {"a": "b c"}), {"id": 1}
        )
        client.post("/repos/o/r/issues", {"title": "t"})

        get, post = opener.requests
        self.assertEqual(
            get.full_url, "https://api.example.test/repos/o/r/actions/runs/1?a=b+c"
        )
        self.assertEqual(get.get_method(), "GET")
        self.assertEqual(get.get_header("Authorization"), "Bearer secret")
        self.assertEqual(get.get_header("X-github-api-version"), "2022-11-28")
        self.assertEqual(post.get_method(), "POST")
        self.assertEqual(json.loads(cast(bytes, post.data)), {"title": "t"})
        self.assertEqual(post.get_header("Content-type"), "application/json")

        anonymous, _ = self.client(_FakeOpener(_FakeResponse(b"{}")), token=None)
        anonymous.get("/x")

    def test_pages_follow_the_next_link(self):
        opener = _FakeOpener(
            _FakeResponse(
                b'{"jobs": [1, 2]}',
                '<https://api.example.test/next?page=2>; rel="next", '
                '<https://api.example.test/next?page=2>; rel="last"',
            ),
            _FakeResponse(b'{"jobs": [3]}'),
        )
        client, _ = self.client(opener)

        self.assertEqual(
            client.get_all("/jobs", {"filter": "all"}, key="jobs"), [1, 2, 3]
        )
        self.assertEqual(
            [request.full_url for request in opener.requests],
            [
                "https://api.example.test/jobs?per_page=100&filter=all",
                "https://api.example.test/next?page=2",
            ],
        )

    def test_reads_retry_transient_failures_and_writes_do_not(self):
        opener = _FakeOpener(
            _http_error(502),
            urllib.error.URLError("reset"),
            _FakeResponse(b"[]"),
        )
        client, sleeps = self.client(opener)
        self.assertEqual(client.get("/x"), [])
        self.assertEqual(sleeps, [2.0, 4.0])

        opener = _FakeOpener(_http_error(502), _FakeResponse(b"{}"))
        client, sleeps = self.client(opener)
        with self.assertRaisesRegex(GitHubApiError, "POST /x failed with HTTP 502"):
            client.post("/x", {})
        self.assertEqual(sleeps, [])

        opener = _FakeOpener(*(urllib.error.URLError("down") for _ in range(3)))
        client, _ = self.client(opener)
        with self.assertRaisesRegex(ScriptError, "GitHub GET /x failed: .*down"):
            client.get("/x")

    def test_errors_carry_the_status_and_githubs_message(self):
        opener = _FakeOpener(_http_error(422, "Validation Failed"))
        client, _ = self.client(opener)
        with self.assertRaises(GitHubApiError) as raised:
            client.patch("/repos/o/r/issues/1", {"body": ""})
        self.assertEqual(raised.exception.status, 422)
        self.assertIn("HTTP 422: Validation Failed", str(raised.exception))

        client, _ = self.client(_FakeOpener(_http_error(404), _http_error(500)))
        self.assertFalse(client.exists("/repos/o/r/labels/bug"))
        client, _ = self.client(_FakeOpener(*(_http_error(500) for _ in range(3))))
        with self.assertRaises(GitHubApiError):
            client.exists("/repos/o/r/labels/bug")

    def test_logs_are_bounded_and_missing_logs_are_none(self):
        opener = _FakeOpener(
            _FakeResponse(b"\xef\xbb\xbf0123456789"),
            _http_error(404),
            _http_error(410),
            _http_error(403, "Must have admin rights"),
        )
        client, _ = self.client(opener)

        self.assertEqual(client.get_log("/log"), "23456789")
        self.assertIsNone(client.get_log("/log"))
        self.assertIsNone(client.get_log("/log"))
        with self.assertRaisesRegex(GitHubApiError, "HTTP 403"):
            client.get_log("/log")

        opener = _FakeOpener(_FakeResponse(b"\xef\xbb\xbfshort"))
        client, _ = self.client(opener)
        self.assertEqual(client.get_log("/log"), "short")

    def test_the_default_opener_drops_the_token_on_redirects(self):
        with patch.object(nightly_ci, "credential_safe_opener") as opener:
            nightly_ci.GitHubClient("secret")
        opener.assert_called_once_with()


class CommandTests(unittest.TestCase):
    def run_command(
        self, github: FakeGitHub, *arguments: str, token: str | None = "secret"
    ) -> str:
        args = nvx.parse_args(["report-nightly-ci", *arguments])
        environment = {"GH_TOKEN": token} if token else {}
        stdout = io.StringIO()
        with (
            patch.dict("os.environ", environment, clear=True),
            patch.object(nightly_ci, "GitHubClient", return_value=github) as client,
            contextlib.redirect_stdout(stdout),
        ):
            args.handler(args)
        client.assert_called_once_with(token, nightly_ci.GITHUB_API_URL)
        return stdout.getvalue()

    def test_the_command_reports_and_summarizes_a_run(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        with tempfile.TemporaryDirectory() as directory:
            summary = Path(directory) / "summary.md"
            output = self.run_command(
                github,
                "--repository",
                REPOSITORY,
                "--run-id",
                "5001",
                "--run-attempt",
                "1",
                "--assignee",
                "ppenna",
                "--label",
                "bug",
                "--summary",
                str(summary),
            )
            text = summary.read_text(encoding="utf-8")
        self.assertEqual(
            output.splitlines()[-2:],
            [
                "nightly CI report: run #50 failed; opening a report",
                "nightly CI report: opened #700",
            ],
        )
        self.assertTrue(
            text.startswith(
                "## Nightly CI report\n\nRun #50 failed; opening a report.\n\n[Run #50]"
            )
        )
        self.assertEqual(len(github.open_reports()), 1)

    def test_a_dry_run_changes_nothing(self):
        github = FakeGitHub()
        _add_failing_run(github, 5001, 50)
        output = self.run_command(
            github,
            "--repository",
            REPOSITORY,
            "--run-id",
            "5001",
            "--assignee",
            "ppenna",
            "--dry-run",
            token=None,
        )
        self.assertIn(
            f"would open {nightly_ci.REPORT_TITLE!r} (labels: none; assignees: ppenna):",
            output,
        )
        self.assertIn(nightly_ci.REPORT_MARKER, output)
        self.assertEqual(github.writes, [])

    def test_the_command_needs_a_token_and_a_repository(self):
        github = FakeGitHub()
        with self.assertRaisesRegex(ScriptError, "set GH_TOKEN"):
            self.run_command(
                github, "--repository", REPOSITORY, "--run-id", "1", token=None
            )
        with self.assertRaisesRegex(ScriptError, "OWNER/REPOSITORY"):
            self.run_command(github, "--repository", "nvx", "--run-id", "1")
        for value in ("0", "-1", "x"):
            with self.subTest(value=value):
                with (
                    contextlib.redirect_stderr(io.StringIO()),
                    self.assertRaises(SystemExit),
                ):
                    nvx.parse_args(
                        [
                            "report-nightly-ci",
                            "--repository",
                            REPOSITORY,
                            "--run-id",
                            value,
                        ]
                    )

    def test_describe_action_names_every_change(self):
        self.assertEqual(
            nightly_ci.describe_action(nightly_ci.EditIssue(5, "body")),
            "would update #5:\nbody",
        )
        self.assertEqual(
            nightly_ci.describe_action(nightly_ci.CommentOnIssue(5, "body")),
            "would comment on #5:\nbody",
        )
        self.assertEqual(
            nightly_ci.describe_action(nightly_ci.CloseIssue(5, "completed")),
            "would close #5 as completed",
        )


def _workflow(name: str) -> dict[object, object]:
    document = yaml.safe_load((WORKFLOWS / name).read_text(encoding="utf-8"))
    assert isinstance(document, dict)
    return cast(dict[object, object], document)


def _triggers(workflow: Mapping[object, object]) -> dict[str, object]:
    # YAML 1.1 reads the bare key "on" as true.
    triggers = workflow.get("on", workflow.get(True))
    assert isinstance(triggers, dict)
    return cast(dict[str, object], triggers)


def _jobs(workflow: Mapping[object, object]) -> dict[str, dict[str, object]]:
    return cast(dict[str, dict[str, object]], workflow["jobs"])


class WorkflowTests(unittest.TestCase):
    def setUp(self) -> None:
        self.workflow = _workflow("nightly-ci-report.yml")
        jobs = _jobs(self.workflow)
        self.assertEqual(list(jobs), ["report"])
        self.job = jobs["report"]

    def step(self, name: str) -> dict[str, object]:
        steps = cast(list[dict[str, object]], self.job["steps"])
        [step] = [step for step in steps if step.get("name") == name]
        return step

    def test_the_report_follows_completed_ci_runs_on_dev(self):
        ci = _workflow("ci.yml")
        self.assertEqual(
            _triggers(self.workflow),
            {
                "workflow_run": {
                    "workflows": [ci["name"]],
                    "types": ["completed"],
                    "branches": ["dev"],
                }
            },
        )
        self.assertIn("schedule", _triggers(ci))

    def test_only_scheduled_runs_are_reported(self):
        self.assertEqual(
            self.job["if"],
            f"github.event.workflow_run.event == '{nightly_ci.NIGHTLY_EVENT}'",
        )
        self.assertEqual(nightly_ci.NIGHTLY_EVENT, "schedule")

    def test_only_the_report_job_writes_and_only_issues(self):
        self.assertEqual(self.workflow["permissions"], {})
        self.assertEqual(
            self.job["permissions"],
            {
                "actions": "read",
                "checks": "read",
                "contents": "read",
                "issues": "write",
            },
        )
        # Pull requests and pushes keep the permissions that they had: no other
        # workflow or job can write issues.
        writers: list[str] = []
        for path in sorted(WORKFLOWS.glob("*.yml")):
            workflow = _workflow(path.name)
            scopes = [("", workflow.get("permissions"))] + [
                (name, job.get("permissions")) for name, job in _jobs(workflow).items()
            ]
            for name, permissions in scopes:
                if permissions == "write-all" or (
                    isinstance(permissions, dict)
                    and cast(dict[str, object], permissions).get("issues") == "write"
                ):
                    writers.append(f"{path.name}:{name}")
        self.assertEqual(writers, ["nightly-ci-report.yml:report"])

    def test_the_report_runs_the_default_branch_without_credentials(self):
        self.assertEqual(self.job["runs-on"], "ubuntu-latest")
        checkout = self.step("Checkout")
        self.assertEqual(checkout["uses"], "actions/checkout@v5")
        # Without a ref, the checkout is the default branch, not the run's code.
        self.assertEqual(checkout["with"], {"persist-credentials": False})
        # The triggering run reaches the command only through the environment.
        self.assertNotIn(
            "${{", cast(str, self.step("Report the nightly CI run")["run"])
        )

    def test_the_report_names_the_triggering_run_attempt(self):
        report = self.step("Report the nightly CI run")
        self.assertEqual(
            report["env"],
            {
                "GH_TOKEN": "${{ github.token }}",
                "RUN_ID": "${{ github.event.workflow_run.id }}",
                "RUN_ATTEMPT": "${{ github.event.workflow_run.run_attempt }}",
            },
        )
        args = self.report_arguments()
        self.assertIs(args.handler, nightly_ci.command_report_nightly_ci)
        self.assertEqual(
            (args.repository, args.run_id, args.run_attempt), (REPOSITORY, 5001, 2)
        )
        self.assertEqual(args.assignee, ["ppenna"])
        self.assertEqual(args.label, ["bug"])
        self.assertEqual(args.summary, Path("summary.md"))
        self.assertFalse(args.dry_run)

    def report_arguments(self) -> argparse.Namespace:
        environment = {
            "GITHUB_REPOSITORY": REPOSITORY,
            "RUN_ID": "5001",
            "RUN_ATTEMPT": "2",
            "GITHUB_STEP_SUMMARY": "summary.md",
        }
        command = re.sub(
            r"\$\{(\w+)\}",
            lambda match: environment[match.group(1)],
            cast(str, self.step("Report the nightly CI run")["run"]),
        )
        argv = shlex.split(command)
        self.assertEqual(argv[:2], ["python3", "scripts/nvx.py"])
        return nvx.parse_args(argv[2:])

    def test_the_documentation_names_the_owner_and_the_label(self):
        document = (WORKFLOWS.parent.parent / "doc" / "ci.md").read_text(
            encoding="utf-8"
        )
        section = document.split("\n## Nightly failure reports\n", 1)[1]
        section = section.split("\n## ", 1)[0]
        args = self.report_arguments()
        for name in (*args.assignee, *args.label):
            with self.subTest(name=name):
                self.assertIn(f"`{name}`", section)

    def test_reports_never_run_concurrently(self):
        # The group keeps at most one reporter waiting, so a newer reporter
        # can replace a waiting one; plan_report follows the latest nightly
        # result, whichever run triggered it, so no result goes unreported
        # (ReportTests.test_the_report_follows_the_latest_nightly_result).
        self.assertEqual(
            self.job["concurrency"],
            {"group": "nightly-ci-report", "cancel-in-progress": False},
        )

    def test_the_summary_job_is_the_required_status_check(self):
        jobs = _jobs(_workflow("ci.yml"))
        self.assertIn(
            jobs["required-status-check"]["name"], nightly_ci.SUMMARY_JOB_NAMES
        )


if __name__ == "__main__":
    unittest.main()

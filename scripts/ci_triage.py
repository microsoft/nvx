#!/usr/bin/env python3
"""Classify one NVX CI workflow run before detailed investigation.

The command resolves workflow-run and pull-request selectors, records the run's
jobs and first failing steps, extracts concise failure evidence, and classifies
cancellations using the workflow and current pull-request head, and benchmark
host changes using the failed logs. Optional history finds matching failures on
``dev`` and open issues. GitHub access is exclusively through ``gh`` and uses
an injectable subprocess runner.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import json
import re
import subprocess
import sys
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import cast
from urllib.parse import urlparse

DEFAULT_REPOSITORY = "microsoft/nvx"
CI_WORKFLOW = "ci.yml"
RUN_FIELDS = (
    "databaseId,attempt,event,status,conclusion,headSha,headBranch,url,"
    "workflowDatabaseId,workflowName,createdAt,updatedAt,jobs"
)
RUN_LIST_FIELDS = "databaseId,attempt,status,conclusion,event,headSha,createdAt,url"
HISTORY_FIELDS = "databaseId,conclusion,createdAt,headSha,url,event"
PR_FIELDS = "number,headRefOid,isDraft,url,state"
PR_VIEW_FIELDS = f"{PR_FIELDS},headRefName,mergedAt,mergeStateStatus"
_REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
_MERGE_GROUP_PR = re.compile(r"(?:^|/)pr-(\d+)-")
# Markers of a failure, most decisive first: a test framework's record of a
# failed test, an uncaught exception, a compiler's or a tool's error, a step's
# own error annotation, and generic words last. A log of passing tests often
# holds generic words, such as the `error:` that a test expects a CLI to print.
_LOG_MARKERS = tuple(
    re.compile(pattern)
    for pattern in (
        r"^(?:FAIL|ERROR): \S",
        r"^\s*(?:FAIL|SIGSEGV|SIGABRT|TIMEOUT) \[\s*[\d.]+s\]",
        r"^---- \S+ stdout ----$|^test \S+ \.\.\. FAILED$",
        r"panicked at",
        r"^Traceback \(most recent call last\):",
        r"^error(?:\[E\d+\])?: ",
        r"##\[error\](?!Process completed with exit code)",
        r"AssertionError|\bFAILED\b|\berror:|\bError:",
        r"##\[error\]",
    )
)
# The lines before a marker that an excerpt keeps; the cause follows it.
_LOG_CONTEXT_BEFORE = 3
_TIMESTAMP_PREFIX = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z ?")
# The benchmark host check, a step of the run-benchmark composite action that a
# platform job reports as its Run benchmark step, fails that step with this
# diagnostic when its runner is no longer the host of the series' history, as
# nightly_ci.py detects (doc/benchmarks.md, "Expected benchmark hosts").
_BENCHMARK_STEP = "Run benchmark"
_BENCHMARK_HOST_CHANGED = re.compile(
    r"this runner differs from \S+ in \d+ required field"
)

JsonObject = dict[str, object]
Runner = Callable[[Sequence[str]], subprocess.CompletedProcess[str]]


class TriageError(RuntimeError):
    """The target or a required GitHub response could not be resolved."""


def _run(arguments: Sequence[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        list(arguments),
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )


def _json(text: str, context: str) -> object:
    try:
        return cast(object, json.loads(text))
    except json.JSONDecodeError as error:
        raise TriageError(f"{context} returned invalid JSON: {error.msg}") from error


def _object(value: object, context: str) -> JsonObject:
    if not isinstance(value, dict):
        raise TriageError(f"{context} is not a JSON object")
    return cast(JsonObject, value)


def _array(value: object, context: str) -> list[object]:
    if not isinstance(value, list):
        raise TriageError(f"{context} is not a JSON array")
    return cast(list[object], value)


def _string(value: object, context: str, *, empty: bool = True) -> str:
    if not isinstance(value, str) or (not empty and not value):
        raise TriageError(f"{context} is not a string")
    return value


def _optional_string(value: object, context: str) -> str | None:
    if value is None:
        return None
    return _string(value, context)


def _integer(value: object, context: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool):
        raise TriageError(f"{context} is not an integer")
    return value


def _optional_integer(value: object, context: str) -> int | None:
    if value is None:
        return None
    return _integer(value, context)


def _boolean(value: object, context: str) -> bool:
    if not isinstance(value, bool):
        raise TriageError(f"{context} is not a boolean")
    return value


def _datetime(value: str) -> datetime:
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as error:
        raise TriageError(f"invalid GitHub timestamp {value!r}") from error
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def _repository(repository: str) -> str:
    if _REPOSITORY.fullmatch(repository) is None:
        raise TriageError("--repo must be OWNER/NAME")
    return repository


def _json_objects(text: str, context: str) -> list[JsonObject]:
    return [
        _object(value, f"{context} item")
        for value in _array(_json(text, context), context)
    ]


@dataclass(frozen=True)
class Target:
    kind: str
    number: int
    attempt: int | None = None
    job_id: int | None = None

    def as_json(self) -> JsonObject:
        return {
            "kind": self.kind,
            "number": self.number,
            "attempt": self.attempt,
            "jobId": self.job_id,
        }


def parse_target(value: str, repository: str = DEFAULT_REPOSITORY) -> Target:
    """Parse a run or pull-request selector without performing I/O."""

    repository = _repository(repository)
    if value.isdigit():
        return Target("run", int(value))
    pull_match = re.fullmatch(r"#(\d+)|pr:(\d+)", value, re.IGNORECASE)
    if pull_match is not None:
        number = pull_match.group(1) or pull_match.group(2)
        return Target("pull_request", int(number))

    parsed = urlparse(value)
    if parsed.scheme not in {"http", "https"} or parsed.netloc.lower() != "github.com":
        raise TriageError(
            "TARGET must be a run ID, run URL, pull-request URL, #N, or pr:N"
        )
    parts = [part for part in parsed.path.split("/") if part]
    if len(parts) < 4:
        raise TriageError("the GitHub URL does not identify a run or pull request")
    url_repository = f"{parts[0]}/{parts[1]}"
    if url_repository.lower() != repository.lower():
        raise TriageError(f"the target belongs to {url_repository}, not {repository}")
    if parts[2:4] == ["actions", "runs"] and len(parts) >= 5:
        if not parts[4].isdigit():
            raise TriageError("the workflow run URL has an invalid run ID")
        run_id = int(parts[4])
        if len(parts) == 5:
            return Target("run", run_id)
        if len(parts) == 7 and parts[5] == "attempts" and parts[6].isdigit():
            return Target("run", run_id, attempt=int(parts[6]))
        if len(parts) == 7 and parts[5] == "job" and parts[6].isdigit():
            return Target("run", run_id, job_id=int(parts[6]))
        raise TriageError("the workflow run URL has an unsupported suffix")
    if parts[2] == "pull" and len(parts) == 4 and parts[3].isdigit():
        return Target("pull_request", int(parts[3]))
    raise TriageError("the GitHub URL does not identify a run or pull request")


@dataclass(frozen=True)
class Step:
    name: str
    status: str
    conclusion: str | None
    number: int | None
    started_at: str | None
    completed_at: str | None

    def as_json(self) -> JsonObject:
        return {
            "name": self.name,
            "status": self.status,
            "conclusion": self.conclusion,
            "number": self.number,
            "startedAt": self.started_at,
            "completedAt": self.completed_at,
        }


@dataclass(frozen=True)
class Job:
    database_id: int
    name: str
    status: str
    conclusion: str | None
    started_at: str | None
    completed_at: str | None
    runner_name: str | None
    runner_labels: tuple[str, ...]
    matrix: tuple[str, ...]
    steps: tuple[Step, ...]

    @property
    def first_failed_step(self) -> Step | None:
        return next((step for step in self.steps if step.conclusion == "failure"), None)

    @property
    def failed(self) -> bool:
        return self.conclusion in {"failure", "timed_out"}

    def as_json(self) -> JsonObject:
        failed_step = self.first_failed_step
        return {
            "databaseId": self.database_id,
            "name": self.name,
            "status": self.status,
            "conclusion": self.conclusion,
            "startedAt": self.started_at,
            "completedAt": self.completed_at,
            "runnerName": self.runner_name,
            "runnerLabels": list(self.runner_labels),
            "matrix": list(self.matrix),
            "firstFailedStep": (
                failed_step.as_json() if failed_step is not None else None
            ),
            "steps": [step.as_json() for step in self.steps],
        }


@dataclass(frozen=True)
class Run:
    database_id: int
    attempt: int
    event: str
    status: str
    conclusion: str | None
    head_sha: str
    head_branch: str
    url: str
    workflow_id: int
    workflow_name: str
    created_at: str
    updated_at: str
    jobs: tuple[Job, ...]

    def as_json(self) -> JsonObject:
        return {
            "databaseId": self.database_id,
            "attempt": self.attempt,
            "event": self.event,
            "status": self.status,
            "conclusion": self.conclusion,
            "headSha": self.head_sha,
            "headBranch": self.head_branch,
            "url": self.url,
            "workflowDatabaseId": self.workflow_id,
            "workflowName": self.workflow_name,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "jobs": [job.as_json() for job in self.jobs],
        }


@dataclass(frozen=True)
class PullRequest:
    number: int
    head_sha: str
    is_draft: bool
    url: str
    state: str
    head_branch: str | None = None
    merged_at: str | None = None
    merge_state_status: str | None = None

    def as_json(self) -> JsonObject:
        return {
            "number": self.number,
            "headRefOid": self.head_sha,
            "isDraft": self.is_draft,
            "url": self.url,
            "state": self.state,
            "headRefName": self.head_branch,
            "mergedAt": self.merged_at,
            "mergeStateStatus": self.merge_state_status,
        }


@dataclass(frozen=True)
class WorkflowConcurrency:
    name: str | None
    group: str | None
    cancel_in_progress: str | None
    queue: str | None
    queue_max: bool
    source: str

    def as_json(self) -> JsonObject:
        return {
            "name": self.name,
            "group": self.group,
            "cancelInProgress": self.cancel_in_progress,
            "queue": self.queue,
            "queueMax": self.queue_max,
            "source": self.source,
        }


@dataclass(frozen=True)
class Classification:
    name: str
    meaning: str
    next_action: str

    def as_json(self) -> JsonObject:
        return {
            "name": self.name,
            "meaning": self.meaning,
            "nextAction": self.next_action,
        }


CLASSIFICATIONS = {
    "passed": Classification(
        "passed", "The CI run completed successfully.", "No CI debugging is needed."
    ),
    "in-progress": Classification(
        "in-progress",
        "The CI run has not completed yet.",
        "Wait for this exact run attempt before drawing a conclusion.",
    ),
    "superseded": Classification(
        "superseded",
        "A newer run replaced this obsolete head or queued dev push.",
        "Use the reported replacement run and do not debug this cancellation.",
    ),
    "dev-push-cancelled": Classification(
        "dev-push-cancelled",
        "A queued dev push was cancelled although dev pushes do not replace one another.",
        "Investigate a timeout, runner loss, or explicit cancellation.",
    ),
    "merge-group-cancelled": Classification(
        "merge-group-cancelled",
        "The merge queue replaced this group or its pull request left the queue.",
        "Check the pull request's queue outcome before treating this as a defect.",
    ),
    "cancelled-current-head": Classification(
        "cancelled-current-head",
        "The current run has no qualifying replacement.",
        "Investigate timeout, runner loss, or cancellation cleanup.",
    ),
    "benchmark-host-changed": Classification(
        "benchmark-host-changed",
        "The benchmark runner no longer matches the host of its series history.",
        "Treat this as infrastructure; accept the host and reset history only on request.",
    ),
    "failed": Classification(
        "failed",
        "At least one CI job failed for this run's exact head.",
        "Investigate the first failed step and decisive log excerpt.",
    ),
}


class GitHub:
    """GitHub and local immutable reads through an injectable command runner."""

    def __init__(self, repository: str, runner: Runner = _run) -> None:
        self.repository = _repository(repository)
        self.runner = runner

    def raw(self, arguments: Sequence[str]) -> subprocess.CompletedProcess[str]:
        return self.runner(arguments)

    def command(self, arguments: Sequence[str], context: str) -> str:
        result = self.raw(["gh", *arguments])
        if result.returncode != 0:
            detail = result.stderr.strip() or result.stdout.strip()
            raise TriageError(
                f"{context} failed: {detail or 'gh exited unsuccessfully'}"
            )
        return result.stdout

    def try_command(
        self, arguments: Sequence[str], context: str
    ) -> tuple[str | None, str | None]:
        result = self.raw(["gh", *arguments])
        if result.returncode == 0:
            return result.stdout, None
        detail = result.stderr.strip() or result.stdout.strip()
        return None, f"{context} failed: {detail or 'gh exited unsuccessfully'}"

    def run(self, run_id: int, attempt: int | None) -> Run:
        arguments = [
            "run",
            "view",
            str(run_id),
            "--repo",
            self.repository,
            "--json",
            RUN_FIELDS,
        ]
        if attempt is not None:
            arguments.extend(("--attempt", str(attempt)))
        output = self.command(arguments, f"reading workflow run {run_id}")
        return parse_run(_object(_json(output, "workflow run"), "workflow run"))

    def pull_request(self, number: int) -> PullRequest:
        output = self.command(
            [
                "pr",
                "view",
                str(number),
                "--repo",
                self.repository,
                "--json",
                PR_VIEW_FIELDS,
            ],
            f"reading pull request {number}",
        )
        return parse_pull_request(
            _object(_json(output, "pull request"), "pull request"),
            with_branch=True,
        )

    def pull_requests_for_branch(self, branch: str) -> list[PullRequest]:
        output = self.command(
            [
                "pr",
                "list",
                "--repo",
                self.repository,
                "--head",
                branch,
                "--state",
                "all",
                "--json",
                PR_FIELDS,
            ],
            f"finding the pull request for branch {branch}",
        )
        return [
            parse_pull_request(item)
            for item in _json_objects(output, "pull request list")
        ]

    def runs_for_commit(
        self, head_sha: str, workflow: str = CI_WORKFLOW
    ) -> list[JsonObject]:
        output = self.command(
            [
                "run",
                "list",
                "--repo",
                self.repository,
                "--workflow",
                workflow,
                "--commit",
                head_sha,
                "--json",
                RUN_LIST_FIELDS,
            ],
            f"finding {workflow} runs for {head_sha}",
        )
        return _json_objects(output, "workflow run list")

    def dev_runs(
        self, limit: int, fields: str, workflow: str = CI_WORKFLOW
    ) -> list[JsonObject]:
        output = self.command(
            [
                "run",
                "list",
                "--repo",
                self.repository,
                "--workflow",
                workflow,
                "--branch",
                "dev",
                "--limit",
                str(limit),
                "--json",
                fields,
            ],
            f"listing recent dev runs of {workflow}",
        )
        return _json_objects(output, "dev workflow runs")

    def workflow_path(self, workflow_id: int) -> tuple[str | None, str | None]:
        """Return the file of the workflow that a run's workflowDatabaseId
        names, such as .github/workflows/ci.yml, or why it is unknown."""
        output, error = self.try_command(
            ["api", f"repos/{self.repository}/actions/workflows/{workflow_id}"],
            f"reading workflow {workflow_id}",
        )
        if output is None:
            return None, error
        try:
            item = _object(_json(output, "workflow"), "workflow")
            return _string(item.get("path"), "workflow path", empty=False), None
        except TriageError as decode_error:
            return None, str(decode_error)

    def workflow_at_head(
        self, head_sha: str, path: str
    ) -> tuple[WorkflowConcurrency | None, str | None]:
        endpoint = f"repos/{self.repository}/contents/{path}?ref={head_sha}"
        output, error = self.try_command(
            ["api", endpoint], f"reading {path} at {head_sha}"
        )
        if output is not None:
            try:
                item = _object(_json(output, "workflow content"), "workflow content")
                encoding = _string(item.get("encoding"), "workflow encoding")
                content = _string(item.get("content"), "workflow content body")
                if encoding != "base64":
                    raise TriageError(
                        f"workflow content uses unsupported encoding {encoding}"
                    )
                decoded = base64.b64decode(content, validate=False).decode("utf-8")
                return parse_workflow(decoded, "gh api"), None
            except (UnicodeError, binascii.Error, TriageError) as decode_error:
                error = str(decode_error)

        if re.fullmatch(r"[0-9A-Fa-f]{7,64}", head_sha) is not None:
            result = self.raw(
                [
                    "git",
                    "--no-pager",
                    "show",
                    f"{head_sha}:{path}",
                ]
            )
            if result.returncode == 0:
                return parse_workflow(result.stdout, "git show"), None
            local_error = result.stderr.strip() or result.stdout.strip()
            if local_error:
                error = f"{error or 'gh api failed'}; git show failed: {local_error}"
        return None, error


def _matrix(name: str) -> tuple[str, ...]:
    match = re.search(r"\(([^()]*)\)\s*$", name)
    if match is None:
        return ()
    return tuple(value.strip() for value in match.group(1).split(",") if value.strip())


def parse_step(item: JsonObject) -> Step:
    return Step(
        name=_string(item.get("name"), "step name"),
        status=_string(item.get("status"), "step status"),
        conclusion=_optional_string(item.get("conclusion"), "step conclusion"),
        number=_optional_integer(item.get("number"), "step number"),
        started_at=_optional_string(item.get("startedAt"), "step startedAt"),
        completed_at=_optional_string(item.get("completedAt"), "step completedAt"),
    )


def _labels(value: object) -> tuple[str, ...]:
    if value is None:
        return ()
    return tuple(
        _string(label, "runner label") for label in _array(value, "runner labels")
    )


def parse_job(item: JsonObject) -> Job:
    steps_value = item.get("steps", [])
    steps = tuple(
        parse_step(_object(step, "job step"))
        for step in _array(steps_value, "job steps")
    )
    name = _string(item.get("name"), "job name")
    runner_name = item.get("runnerName", item.get("runner_name"))
    labels = item.get("labels", item.get("runnerLabels"))
    return Job(
        database_id=_integer(item.get("databaseId"), "job databaseId"),
        name=name,
        status=_string(item.get("status"), "job status"),
        conclusion=_optional_string(item.get("conclusion"), "job conclusion"),
        started_at=_optional_string(item.get("startedAt"), "job startedAt"),
        completed_at=_optional_string(item.get("completedAt"), "job completedAt"),
        runner_name=_optional_string(runner_name, "job runnerName"),
        runner_labels=_labels(labels),
        matrix=_matrix(name),
        steps=steps,
    )


def parse_run(item: JsonObject) -> Run:
    jobs = tuple(
        parse_job(_object(job, "workflow job"))
        for job in _array(item.get("jobs"), "workflow jobs")
    )
    return Run(
        database_id=_integer(item.get("databaseId"), "run databaseId"),
        attempt=_integer(item.get("attempt"), "run attempt"),
        event=_string(item.get("event"), "run event"),
        status=_string(item.get("status"), "run status"),
        conclusion=_optional_string(item.get("conclusion"), "run conclusion"),
        head_sha=_string(item.get("headSha"), "run headSha", empty=False),
        head_branch=_string(item.get("headBranch"), "run headBranch"),
        url=_string(item.get("url"), "run url", empty=False),
        workflow_id=_integer(item.get("workflowDatabaseId"), "run workflowDatabaseId"),
        workflow_name=_string(item.get("workflowName"), "run workflowName"),
        created_at=_string(item.get("createdAt"), "run createdAt"),
        updated_at=_string(item.get("updatedAt"), "run updatedAt"),
        jobs=jobs,
    )


def parse_pull_request(item: JsonObject, *, with_branch: bool = False) -> PullRequest:
    branch = None
    if with_branch:
        branch = _optional_string(item.get("headRefName"), "PR headRefName")
    return PullRequest(
        number=_integer(item.get("number"), "PR number"),
        head_sha=_string(item.get("headRefOid"), "PR headRefOid", empty=False),
        is_draft=_boolean(item.get("isDraft"), "PR isDraft"),
        url=_string(item.get("url"), "PR url", empty=False),
        state=_string(item.get("state"), "PR state", empty=False),
        head_branch=branch,
        merged_at=_optional_string(item.get("mergedAt"), "PR mergedAt"),
        merge_state_status=_optional_string(
            item.get("mergeStateStatus"), "PR mergeStateStatus"
        ),
    )


def parse_workflow(text: str, source: str) -> WorkflowConcurrency:
    name: str | None = None
    concurrency: dict[str, str] = {}
    in_concurrency = False
    block_indent = 0
    for raw_line in text.splitlines():
        line = raw_line.split("#", 1)[0].rstrip()
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip())
        stripped = line.strip()
        if name is None and indent == 0 and stripped.startswith("name:"):
            name = stripped.split(":", 1)[1].strip().strip("\"'") or None
        # A job's own concurrency block, indented under it, does not set the
        # workflow's.
        if indent == 0 and stripped == "concurrency:":
            in_concurrency = True
            block_indent = indent
            continue
        if in_concurrency and indent <= block_indent:
            in_concurrency = False
        if in_concurrency and ":" in stripped:
            key, value = stripped.split(":", 1)
            concurrency[key.strip()] = value.strip()
    queue = concurrency.get("queue")
    queue_max = (
        queue is not None
        and re.search(r"(?:^|[\s'\"])(?:max)(?:$|[\s'\"}])", queue) is not None
    )
    return WorkflowConcurrency(
        name=name,
        group=concurrency.get("group"),
        cancel_in_progress=concurrency.get("cancel-in-progress"),
        queue=queue,
        queue_max=queue_max,
        source=source,
    )


def _run_list_created_at(item: JsonObject) -> datetime:
    return _datetime(_string(item.get("createdAt"), "listed run createdAt"))


def _listed_run_json(item: JsonObject) -> JsonObject:
    return {
        "databaseId": _integer(item.get("databaseId"), "listed run databaseId"),
        "attempt": _optional_integer(item.get("attempt"), "listed run attempt"),
        "status": _optional_string(item.get("status"), "listed run status"),
        "conclusion": _optional_string(item.get("conclusion"), "listed run conclusion"),
        "event": _string(item.get("event"), "listed run event"),
        "headSha": _string(item.get("headSha"), "listed run headSha"),
        "createdAt": _string(item.get("createdAt"), "listed run createdAt"),
        "url": _string(item.get("url"), "listed run url"),
    }


def _select_latest(items: Sequence[JsonObject]) -> JsonObject:
    if not items:
        raise TriageError("no CI workflow run exists for the pull request's head")
    return max(items, key=_run_list_created_at)


@dataclass(frozen=True)
class ResolvedTarget:
    target: Target
    run_id: int
    attempt: int | None
    job_id: int | None
    pull_request: PullRequest | None


def resolve_target(github: GitHub, target: Target) -> ResolvedTarget:
    if target.kind == "run":
        return ResolvedTarget(
            target, target.number, target.attempt, target.job_id, None
        )
    pull_request = github.pull_request(target.number)
    runs = github.runs_for_commit(pull_request.head_sha)
    latest = _select_latest(runs)
    return ResolvedTarget(
        target=target,
        run_id=_integer(latest.get("databaseId"), "listed run databaseId"),
        attempt=_optional_integer(latest.get("attempt"), "listed run attempt"),
        job_id=None,
        pull_request=pull_request,
    )


def _select_pull_request(
    run: Run, candidates: Sequence[PullRequest]
) -> PullRequest | None:
    exact = next(
        (candidate for candidate in candidates if candidate.head_sha == run.head_sha),
        None,
    )
    return exact or (candidates[0] if candidates else None)


def _failure_time(job: Job) -> str:
    step = job.first_failed_step
    if step is not None:
        return step.completed_at or step.started_at or job.completed_at or ""
    return job.completed_at or job.started_at or ""


def failed_jobs(run: Run, requested_job: int | None) -> list[Job]:
    jobs = [job for job in run.jobs if job.failed]
    jobs.sort(
        key=lambda job: (
            _failure_time(job) or "\uffff",
            0 if requested_job is not None and job.database_id == requested_job else 1,
            job.name,
        )
    )
    return jobs


def strip_log_prefix(line: str) -> str:
    """Remove gh's job, step, and timestamp columns from one log line."""

    parts = line.rstrip("\r\n").split("\t")
    timestamp_index = next(
        (index for index, part in enumerate(parts) if _TIMESTAMP_PREFIX.match(part)),
        None,
    )
    if timestamp_index is not None:
        payload = "\t".join(parts[timestamp_index:])
        return _TIMESTAMP_PREFIX.sub("", payload)
    return _TIMESTAMP_PREFIX.sub("", line.rstrip("\r\n"))


def extract_log_excerpt(text: str, limit: int) -> list[str]:
    """Return up to limit lines from shortly before the most decisive failure
    marker onwards, or the log's last lines when it holds no marker."""

    if limit <= 0:
        return []
    lines = [strip_log_prefix(line) for line in text.splitlines()]
    for pattern in _LOG_MARKERS:
        marker = next(
            (index for index, line in enumerate(lines) if pattern.search(line)),
            None,
        )
        if marker is not None:
            start = max(0, marker - _LOG_CONTEXT_BEFORE)
            return lines[start : start + limit]
    return lines[-limit:]


def _replacement_for_head(
    github: GitHub, run: Run, pull_request: PullRequest
) -> JsonObject | None:
    if pull_request.head_sha == run.head_sha:
        return None
    candidates = github.runs_for_commit(pull_request.head_sha, str(run.workflow_id))
    newer = [
        candidate
        for candidate in candidates
        if _run_list_created_at(candidate) > _datetime(run.created_at)
        and _integer(candidate.get("databaseId"), "listed run databaseId")
        != run.database_id
    ]
    if not newer:
        return None
    return _listed_run_json(_select_latest(newer))


def _newer_dev_run(github: GitHub, run: Run, limit: int) -> JsonObject | None:
    candidates = github.dev_runs(limit, RUN_LIST_FIELDS, str(run.workflow_id))
    newer = [
        candidate
        for candidate in candidates
        if _string(candidate.get("event"), "listed run event") == "push"
        and _run_list_created_at(candidate) > _datetime(run.created_at)
        and _integer(candidate.get("databaseId"), "listed run databaseId")
        != run.database_id
    ]
    return _listed_run_json(_select_latest(newer)) if newer else None


@dataclass
class ClassificationResult:
    classification: Classification
    replacement: JsonObject | None = None
    workflow: WorkflowConcurrency | None = None


def classify(
    github: GitHub,
    run: Run,
    pull_request: PullRequest | None,
    notes: list[str],
    history_runs: int,
) -> ClassificationResult:
    if run.status != "completed" or run.conclusion is None:
        return ClassificationResult(CLASSIFICATIONS["in-progress"])
    if run.conclusion == "success":
        return ClassificationResult(CLASSIFICATIONS["passed"])

    if run.conclusion == "cancelled":
        if run.event == "pull_request" and pull_request is not None:
            try:
                replacement = _replacement_for_head(github, run, pull_request)
            except TriageError as error:
                notes.append(str(error))
                replacement = None
            if replacement is not None:
                return ClassificationResult(
                    CLASSIFICATIONS["superseded"], replacement=replacement
                )
            return ClassificationResult(CLASSIFICATIONS["cancelled-current-head"])
        if run.event == "push" and run.head_branch == "dev":
            # Each workflow queues or replaces its runs by its own
            # concurrency, so a cancellation is judged by the run's workflow.
            path, workflow_error = github.workflow_path(run.workflow_id)
            workflow: WorkflowConcurrency | None = None
            if path is not None:
                workflow, workflow_error = github.workflow_at_head(run.head_sha, path)
            if workflow_error is not None:
                notes.append(workflow_error)
            if workflow is not None and workflow.queue_max:
                return ClassificationResult(
                    CLASSIFICATIONS["dev-push-cancelled"], workflow=workflow
                )
            try:
                replacement = _newer_dev_run(github, run, history_runs)
            except TriageError as error:
                notes.append(str(error))
                replacement = None
            if replacement is not None:
                return ClassificationResult(
                    CLASSIFICATIONS["superseded"],
                    replacement=replacement,
                    workflow=workflow,
                )
            return ClassificationResult(
                CLASSIFICATIONS["cancelled-current-head"], workflow=workflow
            )
        if run.event == "merge_group":
            return ClassificationResult(CLASSIFICATIONS["merge-group-cancelled"])
        return ClassificationResult(CLASSIFICATIONS["cancelled-current-head"])

    return ClassificationResult(CLASSIFICATIONS["failed"])


def _failed_log(github: GitHub, run: Run, job: Job) -> tuple[str | None, str | None]:
    return github.try_command(
        [
            "run",
            "view",
            str(run.database_id),
            "--repo",
            github.repository,
            "--job",
            str(job.database_id),
            "--log-failed",
        ],
        f"reading failed log for {job.name}",
    )


def _log_records(
    github: GitHub,
    run: Run,
    jobs: Sequence[Job],
    log_lines: int,
    notes: list[str],
) -> tuple[list[JsonObject], dict[int, str | None]]:
    """Return an excerpt of the failed log of each of the first five failed
    jobs, and the logs that it read, by job."""
    records: list[JsonObject] = []
    outputs: dict[int, str | None] = {}
    for job in jobs[:5]:
        output, error = _failed_log(github, run, job)
        outputs[job.database_id] = output
        if output is None:
            notes.append(error or f"could not read failed log for {job.name}")
            excerpt: list[str] = []
        else:
            excerpt = extract_log_excerpt(output, log_lines)
        records.append(
            {
                "jobId": job.database_id,
                "job": job.name,
                "step": (
                    job.first_failed_step.name
                    if job.first_failed_step is not None
                    else None
                ),
                "excerpt": excerpt,
                "error": error,
            }
        )
    return records, outputs


def _benchmark_host_changed(
    github: GitHub,
    run: Run,
    jobs: Sequence[Job],
    outputs: dict[int, str | None],
    notes: list[str],
) -> bool:
    """Return whether the host check failed the benchmark step of a failed job,
    reading the failed logs of the benchmark jobs that outputs lacks."""
    for job in jobs:
        step = job.first_failed_step
        if step is None or step.name != _BENCHMARK_STEP:
            continue
        if job.database_id in outputs:
            output = outputs[job.database_id]
        else:
            output, error = _failed_log(github, run, job)
            if output is None:
                notes.append(error or f"could not read failed log for {job.name}")
        if output is not None and _BENCHMARK_HOST_CHANGED.search(output):
            return True
    return False


def _history_run_jobs(github: GitHub, run_id: int) -> list[Job]:
    output = github.command(
        [
            "run",
            "view",
            str(run_id),
            "--repo",
            github.repository,
            "--json",
            "jobs",
        ],
        f"reading jobs of historical run {run_id}",
    )
    item = _object(_json(output, "historical run"), "historical run")
    return [
        parse_job(_object(job, "historical job"))
        for job in _array(item.get("jobs"), "historical jobs")
    ]


def _issue_records(
    github: GitHub, terms: Sequence[str], notes: list[str]
) -> list[JsonObject]:
    records: list[JsonObject] = []
    for term in dict.fromkeys(terms):
        escaped = term.replace('"', '\\"')
        output, error = github.try_command(
            [
                "issue",
                "list",
                "--repo",
                github.repository,
                "--state",
                "open",
                "--search",
                f'"{escaped}" in:title,body',
                "--json",
                "number,title,url",
                "--limit",
                "5",
            ],
            f"searching issues for {term}",
        )
        if output is None:
            notes.append(error or f"could not search issues for {term}")
            continue
        try:
            matches = _json_objects(output, f"issues matching {term}")
            for match in matches:
                records.append(
                    {
                        "match": term,
                        "number": _integer(match.get("number"), "issue number"),
                        "title": _string(match.get("title"), "issue title"),
                        "url": _string(match.get("url"), "issue url"),
                    }
                )
        except TriageError as parse_error:
            notes.append(str(parse_error))
    return records


def collect_history(
    github: GitHub,
    run: Run,
    current_failed_jobs: Sequence[Job],
    history_days: int,
    history_runs: int,
    now: datetime,
    notes: list[str],
) -> JsonObject:
    matches: list[JsonObject] = []
    try:
        listed_runs = github.dev_runs(
            history_runs, HISTORY_FIELDS, str(run.workflow_id)
        )
    except TriageError as error:
        notes.append(str(error))
        listed_runs = []
    cutoff = now.astimezone(timezone.utc) - timedelta(days=history_days)
    names = {job.name for job in current_failed_jobs}
    for item in listed_runs:
        try:
            run_id = _integer(item.get("databaseId"), "historical run databaseId")
            if run_id == run.database_id:
                continue
            conclusion = _optional_string(
                item.get("conclusion"), "historical run conclusion"
            )
            created_at = _string(item.get("createdAt"), "historical run createdAt")
            if conclusion != "failure" or _datetime(created_at) < cutoff:
                continue
            jobs = _history_run_jobs(github, run_id)
            for job in jobs:
                if job.name not in names or not job.failed:
                    continue
                step = job.first_failed_step
                matches.append(
                    {
                        "databaseId": run_id,
                        "date": created_at,
                        "url": _string(item.get("url"), "historical run url"),
                        "job": job.name,
                        "failedStep": step.name if step is not None else None,
                    }
                )
        except TriageError as error:
            notes.append(str(error))
    terms: list[str] = []
    for job in current_failed_jobs:
        terms.append(job.name)
        step = job.first_failed_step
        if step is not None:
            terms.append(step.name)
    issues = _issue_records(github, terms, notes)
    return {"enabled": True, "runs": matches, "issues": issues}


def _merge_group_pull_request(
    github: GitHub, run: Run, notes: list[str]
) -> PullRequest | None:
    match = _MERGE_GROUP_PR.search(run.head_branch)
    if match is None:
        notes.append("the merge-group ref does not identify its pull request")
        return None
    try:
        pull_request = github.pull_request(int(match.group(1)))
        outcome = pull_request.state
        if pull_request.merged_at is not None:
            outcome += f" at {pull_request.merged_at}"
        elif pull_request.merge_state_status is not None:
            outcome += f" ({pull_request.merge_state_status})"
        notes.append(f"merge-queue pull request #{pull_request.number} is {outcome}")
        return pull_request
    except TriageError as error:
        notes.append(str(error))
        return None


def _first_failure(jobs: Sequence[Job]) -> JsonObject | None:
    if not jobs:
        return None
    job = jobs[0]
    step = job.first_failed_step
    return {
        "job": job.name,
        "jobId": job.database_id,
        "step": step.name if step is not None else None,
        "at": _failure_time(job) or None,
    }


def triage(
    github: GitHub,
    target: Target,
    *,
    history_days: int,
    history_runs: int,
    no_history: bool,
    log_lines: int,
    now: datetime,
) -> JsonObject:
    resolved = resolve_target(github, target)
    run = github.run(resolved.run_id, resolved.attempt)
    notes: list[str] = []
    pull_request = resolved.pull_request
    if run.event == "pull_request" and pull_request is None:
        try:
            candidates = github.pull_requests_for_branch(run.head_branch)
            pull_request = _select_pull_request(run, candidates)
            if pull_request is None:
                notes.append("no pull request matches the run's head branch")
        except TriageError as error:
            notes.append(str(error))
    elif run.event == "merge_group" and pull_request is None:
        pull_request = _merge_group_pull_request(github, run, notes)

    if run.event == "pull_request" and pull_request is not None:
        if pull_request.is_draft:
            notes.append(
                "the pull request is now draft; converting a ready pull request "
                "to draft does not cancel its existing run"
            )

    result = classify(github, run, pull_request, notes, history_runs)
    current_failed_jobs = failed_jobs(run, resolved.job_id)
    stop_for_supersession = result.classification.name == "superseded"
    logs: list[JsonObject] = []
    if not stop_for_supersession:
        logs, outputs = _log_records(github, run, current_failed_jobs, log_lines, notes)
        if result.classification.name == "failed" and _benchmark_host_changed(
            github, run, current_failed_jobs, outputs, notes
        ):
            result = ClassificationResult(CLASSIFICATIONS["benchmark-host-changed"])
    if no_history or stop_for_supersession or not current_failed_jobs:
        history: JsonObject = {
            "enabled": not no_history,
            "runs": [],
            "issues": [],
        }
    else:
        history = collect_history(
            github,
            run,
            current_failed_jobs,
            history_days,
            history_runs,
            now,
            notes,
        )
    return {
        "classification": result.classification.as_json(),
        "target": target.as_json(),
        "run": run.as_json(),
        "pullRequest": (pull_request.as_json() if pull_request is not None else None),
        "replacementRun": result.replacement,
        "workflowAtHead": (
            result.workflow.as_json() if result.workflow is not None else None
        ),
        "firstFailure": _first_failure(current_failed_jobs),
        "logs": logs,
        "history": history,
        "notes": notes,
    }


def render(document: JsonObject) -> str:
    classification = _object(document.get("classification"), "classification")
    name = _string(classification.get("name"), "classification name")
    meaning = _string(classification.get("meaning"), "classification meaning")
    action = _string(classification.get("nextAction"), "classification action")
    run = _object(document.get("run"), "run")
    lines = [
        f"Classification: {name} — {meaning}",
        f"Next action: {action}",
        (
            f"Run: {_string(run.get('url'), 'run url')} "
            f"(ID {_integer(run.get('databaseId'), 'run id')}, "
            f"attempt {_integer(run.get('attempt'), 'run attempt')})"
        ),
        (
            f"Workflow: {_string(run.get('workflowName'), 'workflow name')}; "
            f"event: {_string(run.get('event'), 'run event')}; "
            f"status: {_string(run.get('status'), 'run status')}/"
            f"{_optional_string(run.get('conclusion'), 'run conclusion') or '-'}"
        ),
        (
            f"Head: {_string(run.get('headSha'), 'run head')} "
            f"({_string(run.get('headBranch'), 'run branch')})"
        ),
    ]
    pull_value = document.get("pullRequest")
    if pull_value is not None:
        pull_request = _object(pull_value, "pull request")
        draft = (
            "draft" if _boolean(pull_request.get("isDraft"), "PR draft") else "ready"
        )
        lines.append(
            f"Pull request: #{_integer(pull_request.get('number'), 'PR number')} "
            f"{_string(pull_request.get('state'), 'PR state')} ({draft}), "
            f"head {_string(pull_request.get('headRefOid'), 'PR head')}"
        )
    replacement_value = document.get("replacementRun")
    if replacement_value is not None:
        replacement = _object(replacement_value, "replacement run")
        lines.append(
            "Replacement: "
            f"{_string(replacement.get('url'), 'replacement URL')} "
            f"at {_string(replacement.get('headSha'), 'replacement head')}"
        )
    workflow_value = document.get("workflowAtHead")
    if workflow_value is not None:
        workflow = _object(workflow_value, "workflow at head")
        lines.append(
            f"Workflow concurrency: queue={workflow.get('queue')}, "
            f"cancel-in-progress={workflow.get('cancelInProgress')}"
        )
    failure_value = document.get("firstFailure")
    if failure_value is not None:
        failure = _object(failure_value, "first failure")
        lines.append(
            f"First failure: {_string(failure.get('job'), 'failure job')} / "
            f"{_optional_string(failure.get('step'), 'failure step') or 'unknown step'}"
        )
    logs = _array(document.get("logs"), "logs")
    for raw_log in logs:
        log = _object(raw_log, "log")
        lines.append(f"Log excerpt — {_string(log.get('job'), 'log job')}:")
        excerpt = _array(log.get("excerpt"), "log excerpt")
        if excerpt:
            lines.extend(f"  {_string(line, 'log line')}" for line in excerpt)
        else:
            lines.append("  (unavailable)")
    history = _object(document.get("history"), "history")
    history_runs = _array(history.get("runs"), "history runs")
    lines.append(f"Matching dev failures: {len(history_runs)}")
    for raw_match in history_runs:
        match = _object(raw_match, "history match")
        lines.append(
            f"- {_string(match.get('date'), 'history date')}: "
            f"{_string(match.get('job'), 'history job')} / "
            f"{_optional_string(match.get('failedStep'), 'history step') or '-'} "
            f"({_string(match.get('url'), 'history URL')})"
        )
    issues = _array(history.get("issues"), "history issues")
    lines.append(f"Matching open issues: {len(issues)}")
    for raw_issue in issues:
        issue = _object(raw_issue, "history issue")
        lines.append(
            f"- #{_integer(issue.get('number'), 'issue number')} "
            f"{_string(issue.get('title'), 'issue title')} "
            f"({_string(issue.get('url'), 'issue URL')})"
        )
    for raw_note in _array(document.get("notes"), "notes"):
        lines.append(f"Note: {_string(raw_note, 'note')}")
    return "\n".join(lines)


def _positive(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Classify one NVX CI workflow run before debugging it."
    )
    parser.add_argument("target", metavar="TARGET")
    parser.add_argument("--repo", default=DEFAULT_REPOSITORY, metavar="OWNER/NAME")
    parser.add_argument("--history-days", type=_positive, default=14)
    parser.add_argument("--history-runs", type=_positive, default=20)
    parser.add_argument("--no-history", action="store_true")
    parser.add_argument("--log-lines", type=_positive, default=40)
    parser.add_argument("--json", action="store_true", dest="as_json")
    return parser


def run_cli(
    argv: Sequence[str] | None,
    runner: Runner = _run,
    *,
    now: datetime | None = None,
) -> int:
    args = _parser().parse_args(argv)
    try:
        repository = _repository(cast(str, args.repo))
        target = parse_target(cast(str, args.target), repository)
        document = triage(
            GitHub(repository, runner),
            target,
            history_days=cast(int, args.history_days),
            history_runs=cast(int, args.history_runs),
            no_history=cast(bool, args.no_history),
            log_lines=cast(int, args.log_lines),
            now=now or datetime.now(timezone.utc),
        )
        if cast(bool, args.as_json):
            print(json.dumps(document, indent=2, sort_keys=True))
        else:
            print(render(document))
    except (OSError, TriageError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    return run_cli(argv)


if __name__ == "__main__":
    raise SystemExit(main())

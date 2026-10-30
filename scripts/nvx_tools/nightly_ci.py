"""Report failed nightly CI runs in one GitHub tracking issue.

GitHub notifies only one person when a scheduled run fails, so
``nvx.py report-nightly-ci`` reports every completed scheduled CI run in one
tracking issue instead: a failing run opens the issue, or updates it while
failures repeat, and the next passing run closes it. The report links every
failing job and separates infrastructure failures, such as a lost runner or a
network error, from test and build failures. See doc/ci.md, "Nightly failure
reports".
"""

from __future__ import annotations

import argparse
import functools
import html
import http.client
import json
import os
import re
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import deque
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Protocol, TypeVar, cast

from .common import ScriptError, credential_safe_opener, positive_int

NIGHTLY_EVENT = "schedule"
REPORT_TITLE = "ci: the nightly full run is failing"
REPORT_MARKER = "<!-- nvx-nightly-ci-report -->"
# Anyone can open an issue with the marker, so only issues that the workflow's
# token or someone with a role in the repository opened are reports.
REPORT_BOT = "github-actions[bot]"
_TRUSTED_ASSOCIATIONS = frozenset({"COLLABORATOR", "MEMBER", "OWNER"})
# The report keeps its history in a hidden comment, so that the command
# updates it without parsing the tables that it renders.
_STATE_PREFIX = "<!-- nvx-nightly-ci-state: "
_STATE_PATTERN = re.compile(r"<!-- nvx-nightly-ci-state: (.*?) -->")
_STATE_VERSION = 1
DOCUMENTATION_PATH = "doc/ci.md"
DOCUMENTATION_ANCHOR = "nightly-failure-reports"

# The job that checks the other jobs' results fails whenever one of them
# does, so the report lists it apart from the jobs that failed on their own.
SUMMARY_JOB_NAMES = frozenset({"Required status check"})

RUN_PASSED_CONCLUSIONS = frozenset({"success"})
RUN_FAILED_CONCLUSIONS = frozenset(
    {"action_required", "cancelled", "failure", "startup_failure", "timed_out"}
)
_JOB_FAILED_CONCLUSIONS = frozenset({"cancelled", "failure", "timed_out"})
_STEP_FAILED_CONCLUSIONS = frozenset({"cancelled", "failure", "timed_out"})

MAX_HISTORY = 30
# Scheduled runs are a day apart, so the latest result is among the latest 20.
_RECENT_RUNS = 20
MAX_LOGGED_JOBS = 10
# Annotations and logs only add detail, so the report stops reading them after
# this long and reports without the rest, well within its job's timeout.
DIAGNOSTICS_BUDGET_SECONDS = 240.0
EXCERPT_LINES = 15
EXCERPT_LINE_LENGTH = 200
EVIDENCE_LENGTH = 300
# GitHub rejects issue and comment bodies longer than 65,536 characters.
MAX_BODY_LENGTH = 60_000
LOG_LIMIT_BYTES = 16 * 1024 * 1024

GITHUB_API_URL = "https://api.github.com"
_REQUEST_TIMEOUT_SECONDS = 60.0
_GET_ATTEMPTS = 3
_RETRY_BACKOFF_SECONDS = 2.0
_REPOSITORY_PATTERN = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
_NEXT_PAGE = re.compile(r'<([^>]+)>\s*;\s*rel="next"')

# GitHub reports these when a runner disappears in the middle of a job.
_RUNNER_LOST = re.compile(
    r"lost communication with the server|"
    r"runner has received a shutdown signal|"
    r"hosted runner encountered an error|"
    r"received request to deprovision",
    re.IGNORECASE,
)
# GitHub reports these when no runner ever picked a job up.
_NO_RUNNER = re.compile(
    r"was not acquired by runner|"
    r"no (?:enabled )?runners? (?:were |was )?online|"
    r"no runner matching the specified labels",
    re.IGNORECASE,
)
_TIMED_OUT = re.compile(r"exceeded the maximum execution time", re.IGNORECASE)
_OPERATION_CANCELLED = re.compile(r"^the operation was canceled\.?$", re.IGNORECASE)
_EXIT_CODE = re.compile(r"^process completed with exit code \d+\.?$", re.IGNORECASE)
_DISK_FULL = re.compile(
    r"no space left on device|\bENOSPC\b|not enough space on the disk",
    re.IGNORECASE,
)
# Errors that Git, Cargo, pip, apt, Docker, urllib, and the Actions toolkit
# report when a download or a connection to a service fails. The guest network
# checks of the microVM tests report curl and wget errors, such as an
# unresolved or refused host, so this leaves those out.
_NETWORK_ERROR = re.compile(
    r"temporary failure (?:in name resolution|resolving)|"
    r"\b(?:EAI_AGAIN|ECONNRESET|ENOTFOUND|ETIMEDOUT)\b|socket hang up|"
    r"urlopen error|HTTP Error (?:429|5\d\d)|"
    r"\b(?:429 Too Many Requests|502 Bad Gateway|503 Service Unavailable|"
    r"504 Gateway Time-?out)\b|"
    r"TLS handshake timeout|i/o timeout|toomanyrequests|failed to download|"
    r"unable to access '?https?://|the remote end hung up unexpectedly|"
    r"\bRPC failed\b|Could not fetch URL|Max retries exceeded with url|"
    r"(?:Connect|Read)TimeoutError|"
    r"unable to download artifact|failed to (?:download|upload) artifact",
    re.IGNORECASE,
)
# A warning or a notice of a retry shows a transient error that the retry may
# have overcome, such as Cargo's "spurious network error", so it is no
# evidence of what failed the step.
_RETRY_NOTICE = re.compile(
    r"^\s*(?:##\[warning\])?warning\b|\bretrying\b|\bwill retry\b|"
    r"tries remaining|retries left|\bRetry\(total=",
    re.IGNORECASE,
)
# The benchmark host check fails a platform job's benchmark step when its
# runner is no longer the host that the series' history comes from
# (doc/benchmarks.md, "Expected benchmark hosts").
_BENCHMARK_HOST_CHANGED = re.compile(
    r"this runner differs from \S+ in \d+ required field"
)
# Steps that prepare a job rather than build or test the change: checkout,
# runner validation, tool setup, downloads, caches, and runner housekeeping.
_SETUP_STEP = re.compile(
    r"^(?:Set up job|Complete job|Initialize containers|Stop containers|"
    r"Post .+|Checkout(?: .+)?|Validate runner|"
    r"(?:Download|Upload|Restore|Save|Install|Prepare|Set up|Setup) .+|"
    r"Make .+ executable|Report sccache)$"
)
_ERROR_LINE = re.compile(
    r"\b(?:error|errors|fail|failed|failure|failures|panicked|fatal|"
    r"exception|traceback|timed out)\b",
    re.IGNORECASE,
)
_LOG_LINE = re.compile(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.\d+)?Z ?(.*)$")
_LOG_MARKER = re.compile(r"^##\[(?:group|endgroup|start-action|end-action|debug)\b")
_ANSI_ESCAPE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


class FailureKind(Enum):
    INFRASTRUCTURE = "infrastructure"
    TEST = "test"
    CANCELLED = "cancelled"


_KIND_HEADINGS = {
    FailureKind.INFRASTRUCTURE: "Infrastructure failures",
    FailureKind.TEST: "Test and build failures",
    FailureKind.CANCELLED: "Cancelled jobs",
}


class GitHubApiError(ScriptError):
    """A GitHub REST request that GitHub answered with an error status."""

    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status


class GitHubApi(Protocol):
    """The GitHub REST operations that the report uses.

    ``deadline`` is a ``time.monotonic()`` value by which a read must finish,
    including its retries.
    """

    def get(self, path: str, params: Mapping[str, str] | None = None) -> object: ...

    def get_all(
        self,
        path: str,
        params: Mapping[str, str] | None = None,
        *,
        key: str | None = None,
        deadline: float | None = None,
    ) -> list[object]: ...

    def get_log(self, path: str, *, deadline: float | None = None) -> str | None: ...

    def exists(self, path: str) -> bool: ...

    def post(self, path: str, body: Mapping[str, object]) -> object: ...

    def patch(self, path: str, body: Mapping[str, object]) -> object: ...


_Reader = Callable[[urllib.request.Request, float], tuple[bytes, str | None]]
_T = TypeVar("_T")


class _HttpFailure(Exception):
    """An HTTP error status, with the message that GitHub's body gave."""

    def __init__(self, status: int, message: str) -> None:
        super().__init__(status, message)
        self.status = status
        self.message = message


def _finish_by(seconds: float, work: Callable[[], _T]) -> _T:
    """Return what ``work`` returns, or raise TimeoutError after ``seconds``.

    A socket timeout bounds each wait for data, not a whole read, so a server
    that keeps sending a little at a time could hold a read for as long as it
    likes. ``work`` therefore runs in a daemon thread, which the command
    abandons when the time runs out and which ends with the process.
    """
    outcome: list[_T] = []
    failure: list[Exception] = []

    def run() -> None:
        try:
            outcome.append(work())
        except Exception as error:
            failure.append(error)

    thread = threading.Thread(target=run, name="nvx-github-read", daemon=True)
    thread.start()
    thread.join(max(seconds, 0.0))
    if thread.is_alive():
        raise TimeoutError("GitHub took too long to send the response")
    if failure:
        raise failure[0]
    return outcome[0]


class GitHubClient:
    """A GitHub REST client that never forwards its token to another origin.

    Job logs redirect to storage on another host, so requests go through the
    credential-safe opener, which drops the token on such a redirect. Reads
    retry transient failures; writes never retry, so that a lost response
    cannot post a comment twice.
    """

    def __init__(
        self,
        token: str | None,
        api_url: str = GITHUB_API_URL,
        *,
        opener: urllib.request.OpenerDirector | None = None,
        sleep: Callable[[float], None] = time.sleep,
        clock: Callable[[], float] = time.monotonic,
        log_limit: int = LOG_LIMIT_BYTES,
    ) -> None:
        self._token = token
        self._api_url = api_url.rstrip("/")
        self._opener = opener if opener is not None else credential_safe_opener()
        self._sleep = sleep
        self._clock = clock
        self._log_limit = log_limit

    def _url(self, path: str, params: Mapping[str, str] | None) -> str:
        url = path if path.startswith("https://") else f"{self._api_url}{path}"
        if params:
            url = f"{url}?{urllib.parse.urlencode(params)}"
        return url

    def _request(
        self, method: str, url: str, body: Mapping[str, object] | None
    ) -> urllib.request.Request:
        headers = {
            "Accept": "application/vnd.github+json",
            "User-Agent": "nvx-report-nightly-ci",
            "X-GitHub-Api-Version": "2022-11-28",
        }
        data = None
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if self._token:
            headers["Authorization"] = f"Bearer {self._token}"
        return urllib.request.Request(url, data=data, headers=headers, method=method)

    def _timeout(self, deadline: float | None) -> float:
        if deadline is None:
            return _REQUEST_TIMEOUT_SECONDS
        return min(_REQUEST_TIMEOUT_SECONDS, deadline - self._clock())

    def _attempt(
        self,
        method: str,
        url: str,
        body: Mapping[str, object] | None,
        read: _Reader,
        timeout: float,
    ) -> tuple[bytes, str | None]:
        """Send one request and read its response, or GitHub's error message."""
        try:
            return read(self._request(method, url, body), timeout)
        except urllib.error.HTTPError as error:
            raise _HttpFailure(error.code, _github_error_message(error)) from error

    def _open(
        self,
        method: str,
        url: str,
        body: Mapping[str, object] | None,
        read: _Reader,
        deadline: float | None = None,
    ) -> tuple[bytes, str | None]:
        attempts = _GET_ATTEMPTS if method == "GET" else 1
        target = urllib.parse.urlsplit(url).path
        for attempt in range(1, attempts + 1):
            timeout = self._timeout(deadline)
            if timeout <= 0:
                raise ScriptError(f"GitHub {method} {target} ran out of time")
            backoff = _RETRY_BACKOFF_SECONDS * attempt
            retry = attempt < attempts and (
                deadline is None or self._clock() + backoff < deadline
            )
            work = functools.partial(self._attempt, method, url, body, read, timeout)
            try:
                if deadline is None:
                    return work()
                # The whole attempt, including the response and an error's
                # body, ends by the deadline, however slowly they arrive.
                return _finish_by(deadline - self._clock(), work)
            except _HttpFailure as failure:
                if failure.status >= 500 and retry:
                    self._sleep(backoff)
                    continue
                suffix = f": {failure.message}" if failure.message else ""
                raise GitHubApiError(
                    failure.status,
                    f"GitHub {method} {target} failed with HTTP {failure.status}"
                    f"{suffix}",
                ) from failure
            except (OSError, http.client.HTTPException) as error:
                if retry:
                    self._sleep(backoff)
                    continue
                raise ScriptError(
                    f"GitHub {method} {target} failed: {error}"
                ) from error
        raise AssertionError("unreachable")

    def _read_json(
        self, request: urllib.request.Request, timeout: float
    ) -> tuple[bytes, str | None]:
        with self._opener.open(request, timeout=timeout) as response:
            return response.read(), response.headers.get("Link")

    def _send(
        self,
        method: str,
        url: str,
        body: Mapping[str, object] | None = None,
        deadline: float | None = None,
    ) -> tuple[object, str | None]:
        payload, link = self._open(method, url, body, self._read_json, deadline)
        if not payload:
            return None, link
        try:
            return json.loads(payload), link
        except json.JSONDecodeError as error:
            path = urllib.parse.urlsplit(url).path
            raise ScriptError(
                f"GitHub {method} {path} returned invalid JSON"
            ) from error

    def get(self, path: str, params: Mapping[str, str] | None = None) -> object:
        return self._send("GET", self._url(path, params))[0]

    def get_all(
        self,
        path: str,
        params: Mapping[str, str] | None = None,
        *,
        key: str | None = None,
        deadline: float | None = None,
        until: Callable[[Sequence[object]], bool] | None = None,
    ) -> list[object]:
        """Return the items of every page of ``path``, or of the pages up to
        the first for which ``until`` returns true."""
        items: list[object] = []
        url: str | None = self._url(path, {"per_page": "100", **(params or {})})
        while url is not None:
            document, link = self._send("GET", url, deadline=deadline)
            page = _mapping(document, path).get(key) if key is not None else document
            if not isinstance(page, list):
                raise ScriptError(f"GitHub GET {path} returned an invalid page")
            items.extend(cast(list[object], page))
            if until is not None and until(cast(list[object], page)):
                break
            match = _NEXT_PAGE.search(link or "")
            url = match.group(1) if match else None
        return items

    def get_log(self, path: str, *, deadline: float | None = None) -> str | None:
        def download(request: urllib.request.Request, timeout: float) -> bytes:
            chunks: deque[bytes] = deque()
            size = 0
            with self._opener.open(request, timeout=timeout) as response:
                # read1 returns what has arrived, so the deadline is checked
                # while a slow server is still sending.
                while chunk := response.read1(1024 * 1024):
                    if deadline is not None and self._clock() >= deadline:
                        raise TimeoutError("the log took too long to read")
                    chunks.append(chunk)
                    size += len(chunk)
                    while size - len(chunks[0]) >= self._log_limit:
                        size -= len(chunks.popleft())
            return b"".join(chunks)[-self._log_limit :]

        def read(
            request: urllib.request.Request, timeout: float
        ) -> tuple[bytes, str | None]:
            return download(request, timeout), None

        try:
            payload = self._open("GET", self._url(path, None), None, read, deadline)[0]
        except GitHubApiError as error:
            # GitHub deletes logs after their retention period and has none
            # for a job that never started.
            if error.status in (404, 410):
                return None
            raise
        # GitHub starts job logs with a byte order mark.
        return payload.decode("utf-8-sig", errors="replace")

    def download(
        self,
        path: str,
        destination: Path,
        *,
        limit: int,
        deadline: float | None = None,
    ) -> None:
        """Write the content at ``path``, such as an artifact's archive, to
        ``destination``, and refuse content longer than ``limit`` bytes."""

        def read(
            request: urllib.request.Request, timeout: float
        ) -> tuple[bytes, str | None]:
            size = 0
            # A retry writes the file anew.
            with (
                self._opener.open(request, timeout=timeout) as response,
                destination.open("wb") as output,
            ):
                while chunk := response.read1(1024 * 1024):
                    if deadline is not None and self._clock() >= deadline:
                        raise TimeoutError("the download took too long")
                    size += len(chunk)
                    if size > limit:
                        raise ScriptError(
                            f"GitHub GET {path} sent more than {limit} bytes"
                        )
                    output.write(chunk)
                # read1 ends quietly when the connection closes before the
                # content that the response announced, so a retry fetches it
                # whole.
                announced = response.headers.get("Content-Length")
                if announced is not None and announced.isdigit():
                    if size < int(announced):
                        raise http.client.HTTPException(
                            f"the response ended after {size} of its {announced} bytes"
                        )
            return b"", None

        self._open("GET", self._url(path, None), None, read, deadline)

    def exists(self, path: str) -> bool:
        try:
            self.get(path)
        except GitHubApiError as error:
            if error.status == 404:
                return False
            raise
        return True

    def post(self, path: str, body: Mapping[str, object]) -> object:
        return self._send("POST", self._url(path, None), body)[0]

    def patch(self, path: str, body: Mapping[str, object]) -> object:
        return self._send("PATCH", self._url(path, None), body)[0]


def _github_error_message(error: urllib.error.HTTPError) -> str:
    try:
        payload = error.read().decode("utf-8", "replace").strip()
    except (AttributeError, OSError, ValueError):
        return ""
    try:
        document: object = json.loads(payload)
    except json.JSONDecodeError:
        return " ".join(payload.split())[:200]
    if isinstance(document, dict):
        message = cast(dict[str, object], document).get("message")
        if isinstance(message, str):
            return message
    return ""


def _mapping(value: object, what: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ScriptError(f"GitHub returned an invalid {what}")
    return cast(dict[str, object], value)


def _text(document: Mapping[str, object], key: str, what: str) -> str:
    value = document.get(key)
    if not isinstance(value, str):
        raise ScriptError(f"GitHub returned a {what} without {key}")
    return value


def _optional_text(document: Mapping[str, object], key: str) -> str | None:
    value = document.get(key)
    return value if isinstance(value, str) else None


def _integer(document: Mapping[str, object], key: str, what: str) -> int:
    value = document.get(key)
    if not isinstance(value, int) or isinstance(value, bool):
        raise ScriptError(f"GitHub returned a {what} without {key}")
    return value


@dataclass(frozen=True)
class WorkflowRun:
    """One attempt of a workflow run."""

    id: int
    number: int
    attempt: int
    workflow_id: int
    workflow_name: str
    workflow_file: str
    event: str
    status: str
    conclusion: str | None
    branch: str
    sha: str
    finished_at: str
    repository_url: str

    @property
    def url(self) -> str:
        return f"{self.repository_url}/actions/runs/{self.id}/attempts/{self.attempt}"

    @property
    def commit_url(self) -> str:
        return f"{self.repository_url}/commit/{self.sha}"


def _parse_run(value: object) -> WorkflowRun:
    run = _mapping(value, "workflow run")
    repository = _mapping(run.get("repository"), "workflow run repository")
    return WorkflowRun(
        id=_integer(run, "id", "workflow run"),
        number=_integer(run, "run_number", "workflow run"),
        attempt=_integer(run, "run_attempt", "workflow run"),
        workflow_id=_integer(run, "workflow_id", "workflow run"),
        workflow_name=_text(run, "name", "workflow run"),
        workflow_file=_text(run, "path", "workflow run").rsplit("/", 1)[-1],
        event=_text(run, "event", "workflow run"),
        status=_text(run, "status", "workflow run"),
        conclusion=_optional_text(run, "conclusion"),
        branch=_text(run, "head_branch", "workflow run"),
        sha=_text(run, "head_sha", "workflow run"),
        finished_at=_text(run, "updated_at", "workflow run"),
        repository_url=_text(repository, "html_url", "workflow run repository"),
    )


@dataclass(frozen=True)
class Step:
    number: int
    name: str
    conclusion: str | None
    started_at: str | None
    completed_at: str | None


@dataclass(frozen=True)
class Job:
    id: int
    name: str
    url: str
    conclusion: str | None
    runner_name: str | None
    steps: tuple[Step, ...]

    @property
    def failing_step(self) -> Step | None:
        """The first step that failed, timed out, or was cancelled."""
        return next(
            (
                step
                for step in sorted(self.steps, key=lambda step: step.number)
                if step.conclusion in _STEP_FAILED_CONCLUSIONS
            ),
            None,
        )


def _parse_job(value: object) -> Job:
    job = _mapping(value, "job")
    steps: list[Step] = []
    steps_value = job.get("steps")
    for step_value in cast(list[object], steps_value or []):
        step = _mapping(step_value, "job step")
        steps.append(
            Step(
                number=_integer(step, "number", "job step"),
                name=_text(step, "name", "job step"),
                conclusion=_optional_text(step, "conclusion"),
                started_at=_optional_text(step, "started_at"),
                completed_at=_optional_text(step, "completed_at"),
            )
        )
    return Job(
        id=_integer(job, "id", "job"),
        name=_text(job, "name", "job"),
        url=_text(job, "html_url", "job"),
        conclusion=_optional_text(job, "conclusion"),
        runner_name=_optional_text(job, "runner_name") or None,
        steps=tuple(steps),
    )


@dataclass(frozen=True)
class JobFailure:
    """A job that did not succeed, with the evidence of why."""

    name: str
    url: str
    kind: FailureKind
    cause: str
    step: str | None = None
    evidence: str | None = None
    excerpt: tuple[str, ...] = ()


def _clean_log_line(text: str) -> str:
    return _ANSI_ESCAPE.sub("", text).rstrip()


def step_log_tail(log: str, step: Step | None) -> tuple[str, ...]:
    """Return the last lines that a step logged, up to its first error.

    GitHub timestamps every log line, and the jobs API gives each step's start
    and end to the second, so the step's lines are those stamped within that
    window. The window may overlap the next step by a fraction of a second,
    so the tail ends with the step's first block of error lines. Without the
    step's times or the log's timestamps, the whole log stands in for the
    step. A log that has timestamps but none in the step's window, such as a
    long log whose start was cut, has no tail: another step's lines, such as
    a failed upload of diagnostics, must not stand in for the failing step.
    """
    window: tuple[str, str] | None = None
    if step is not None and step.started_at and step.completed_at:
        window = (step.started_at[:19], step.completed_at[:19])
    lines: list[str] = []
    inside = window is None
    echo = failed = stamped = False
    for raw in log.splitlines():
        match = _LOG_LINE.match(raw)
        if match is not None:
            stamp, text = match.groups()
            stamped = True
            inside = window is None or window[0] <= stamp <= window[1]
        else:
            text = raw
        # The runner echoes each command, its shell, and its environment in a
        # "Run" group, which only repeats the workflow; after an error, the
        # next one starts another step.
        if echo:
            echo = not text.startswith("##[endgroup]")
            continue
        if text.startswith("##[group]Run "):
            if inside and failed:
                break
            echo = True
            continue
        if inside:
            lines.append(_clean_log_line(text))
            failed = failed or text.startswith("##[error]")
    if window is not None and not stamped:
        return step_log_tail(log, None)

    first_error = next(
        (index for index, line in enumerate(lines) if line.startswith("##[error]")),
        None,
    )
    if first_error is not None:
        end = first_error
        while end < len(lines) and lines[end].startswith("##[error]"):
            end += 1
        lines = lines[:end]
    kept = [line for line in lines if line.strip() and not _LOG_MARKER.match(line)]
    return tuple(_truncate(line, EXCERPT_LINE_LENGTH) for line in kept[-EXCERPT_LINES:])


def _truncate(text: str, length: int) -> str:
    return text if len(text) <= length else f"{text[: length - 3]}..."


def _specific(lines: Sequence[str]) -> list[str]:
    return [
        line
        for line in lines
        if not _EXIT_CODE.match(line) and not _OPERATION_CANCELLED.match(line)
    ]


def _evidence(messages: Sequence[str], tail: Sequence[str]) -> str | None:
    """Pick the line that best explains a failure.

    The failing step's own error annotations, which its log repeats, explain a
    failure best, then the last error line of its log, and the exit status
    least. Without the step's log, the job's annotations stand in for it,
    although a later step, such as a diagnostics upload, may have made them.
    """
    own = _specific(
        [
            line.removeprefix("##[error]")
            for line in tail
            if line.startswith("##[error]")
        ]
    )
    if own:
        return own[0]
    for line in reversed(tail):
        if _ERROR_LINE.search(line) and not line.startswith("##[error]"):
            return line
    specific = _specific(messages)
    if specific:
        return specific[0]
    return messages[0] if messages else None


def classify_job(
    job: Job,
    messages: Sequence[str],
    tail: Sequence[str] = (),
) -> JobFailure | None:
    """Classify a job's failure as infrastructure, test or build, or cancelled.

    ``messages`` are the job's failure annotations, which hold GitHub's own
    runner errors and the errors that every step reported, and ``tail`` is the
    end of its failing step's log. A failure counts as infrastructure only on
    positive evidence: a lost or missing runner, a failure or timeout in a
    setup step, a benchmark runner that is no longer its series' host, or a
    network, download, or disk error that the failing step logged, other than
    in a warning or a notice of a retry. The runner's errors concern the whole
    job, but a step's error counts only from that step's log, so that a later
    step, such as an upload of diagnostics that fails on the network, cannot
    hide a test failure. Anything else is a test or build failure, so that a
    real regression is never dismissed.
    """
    if job.conclusion not in _JOB_FAILED_CONCLUSIONS:
        return None
    step = job.failing_step
    step_name = step.name if step is not None else None

    def failure(
        kind: FailureKind, cause: str, evidence: str | None = None
    ) -> JobFailure:
        return JobFailure(
            name=job.name,
            url=job.url,
            kind=kind,
            cause=cause,
            step=step_name,
            evidence=evidence,
            excerpt=tuple(tail),
        )

    lost = next((message for message in messages if _RUNNER_LOST.search(message)), None)
    if lost is not None:
        return failure(FailureKind.INFRASTRUCTURE, "runner lost", lost)
    missing = next(
        (message for message in messages if _NO_RUNNER.search(message)), None
    )
    if missing is not None:
        return failure(FailureKind.INFRASTRUCTURE, "no runner", missing)

    timeout = next(
        (message for message in messages if _TIMED_OUT.search(message)), None
    )
    if timeout is not None or job.conclusion == "timed_out":
        if step_name is None or _SETUP_STEP.match(step_name):
            return failure(FailureKind.INFRASTRUCTURE, "setup timed out", timeout)
        return failure(FailureKind.TEST, "timed out", timeout)

    if job.conclusion == "cancelled" or (
        step is not None
        and step.conclusion == "cancelled"
        and not any(not _OPERATION_CANCELLED.match(message) for message in messages)
    ):
        return failure(
            FailureKind.CANCELLED, "cancelled", messages[0] if messages else None
        )

    evidence = _evidence(messages, tail)
    if step_name is not None and _SETUP_STEP.match(step_name):
        return failure(FailureKind.INFRASTRUCTURE, "setup failed", evidence)
    lines = [
        line.removeprefix("##[error]")
        for line in tail
        if not _RETRY_NOTICE.search(line)
    ]
    host = next((line for line in lines if _BENCHMARK_HOST_CHANGED.search(line)), None)
    if host is not None:
        return failure(FailureKind.INFRASTRUCTURE, "benchmark host changed", host)
    disk = next((line for line in lines if _DISK_FULL.search(line)), None)
    if disk is not None:
        return failure(FailureKind.INFRASTRUCTURE, "disk full", disk)
    network = next((line for line in lines if _NETWORK_ERROR.search(line)), None)
    if network is not None:
        return failure(FailureKind.INFRASTRUCTURE, "network or download error", network)
    if step is None and job.runner_name is None:
        return failure(FailureKind.TEST, "never started", evidence)
    return failure(FailureKind.TEST, "failed", evidence)


@dataclass(frozen=True)
class RunReport:
    """The classified failures of one workflow run attempt."""

    run: WorkflowRun
    failures: tuple[JobFailure, ...]
    summary_failures: tuple[JobFailure, ...] = ()

    def count(self, kind: FailureKind) -> int:
        return sum(failure.kind is kind for failure in self.failures)


def _annotations(
    api: GitHubApi, repository: str, job: Job, deadline: float
) -> list[str]:
    try:
        values = api.get_all(
            f"/repos/{repository}/check-runs/{job.id}/annotations", deadline=deadline
        )
    except ScriptError as error:
        print(f"warning: no annotations for job {job.id}: {error}")
        return []
    messages: list[str] = []
    for value in values:
        annotation = _mapping(value, "annotation")
        message = _optional_text(annotation, "message")
        if annotation.get("annotation_level") == "failure" and message:
            messages.append(" ".join(message.split()))
    return messages


def _needs_log(job: Job, messages: Sequence[str]) -> bool:
    # A cancelled job's log shows only the cancellation, a job that no runner
    # picked up has none, and a summary job's annotations name its findings.
    failure = classify_job(job, messages)
    return (
        failure is not None
        and failure.kind is not FailureKind.CANCELLED
        and failure.cause != "no runner"
        and job.name not in SUMMARY_JOB_NAMES
    )


def build_run_report(
    api: GitHubApi,
    repository: str,
    run: WorkflowRun,
    *,
    budget: float = DIAGNOSTICS_BUDGET_SECONDS,
    clock: Callable[[], float] = time.monotonic,
) -> RunReport:
    """Fetch and classify every job of a run attempt that did not succeed.

    The annotations and logs share ``budget`` seconds: once it runs out, the
    remaining jobs are classified from their steps alone.
    """
    # An attempt lists every job of the run: when a rerun repeats only some
    # jobs, the others appear in its list too, with their earlier results,
    # steps, and logs, so the list is the run's state after the rerun.
    values = api.get_all(
        f"/repos/{repository}/actions/runs/{run.id}/attempts/{run.attempt}/jobs",
        key="jobs",
    )
    deadline = clock() + budget
    failures: list[JobFailure] = []
    summary_failures: list[JobFailure] = []
    logged = 0
    for job in sorted((_parse_job(value) for value in values), key=lambda j: j.name):
        if job.conclusion not in _JOB_FAILED_CONCLUSIONS:
            continue
        messages: list[str] = []
        tail: tuple[str, ...] = ()
        if clock() < deadline:
            messages = _annotations(api, repository, job, deadline)
        if (
            _needs_log(job, messages)
            and logged < MAX_LOGGED_JOBS
            and clock() < deadline
        ):
            logged += 1
            try:
                log = api.get_log(
                    f"/repos/{repository}/actions/jobs/{job.id}/logs",
                    deadline=deadline,
                )
            except ScriptError as error:
                print(f"warning: no log for job {job.id}: {error}")
                log = None
            if log:
                tail = step_log_tail(log, job.failing_step)
        failure = classify_job(job, messages, tail)
        if failure is None:
            continue
        if job.name in SUMMARY_JOB_NAMES:
            summary_failures.append(failure)
        else:
            failures.append(failure)
    if clock() >= deadline:
        print(
            f"warning: reading annotations and logs took over {budget:.0f} s; "
            "some jobs are classified from their steps alone"
        )
    if not failures:
        # A summary job that fails alone found a job that ran when it should
        # not have, or did not run when it should have.
        failures, summary_failures = summary_failures, []
    return RunReport(run, tuple(failures), tuple(summary_failures))


@dataclass(frozen=True)
class HistoryEntry:
    """One failing run attempt, as the report's history records it."""

    run_id: int
    number: int
    attempt: int
    sha: str
    finished_at: str
    conclusion: str
    infrastructure: int
    test: int
    cancelled: int

    @classmethod
    def from_report(cls, report: RunReport) -> HistoryEntry:
        run = report.run
        return cls(
            run_id=run.id,
            number=run.number,
            attempt=run.attempt,
            sha=run.sha,
            finished_at=run.finished_at,
            conclusion=run.conclusion or "unknown",
            infrastructure=report.count(FailureKind.INFRASTRUCTURE),
            test=report.count(FailureKind.TEST),
            cancelled=report.count(FailureKind.CANCELLED),
        )

    def same_attempt(self, other: HistoryEntry) -> bool:
        return (self.run_id, self.attempt) == (other.run_id, other.attempt)


def _state_comment(history: Sequence[HistoryEntry]) -> str:
    state = {
        "version": _STATE_VERSION,
        "runs": [
            {
                "run_id": entry.run_id,
                "number": entry.number,
                "attempt": entry.attempt,
                "sha": entry.sha,
                "finished_at": entry.finished_at,
                "conclusion": entry.conclusion,
                "infrastructure": entry.infrastructure,
                "test": entry.test,
                "cancelled": entry.cancelled,
            }
            for entry in history
        ],
    }
    # Escaping ">" keeps "-->" out of the comment.
    encoded = json.dumps(state, separators=(",", ":")).replace(">", "\\u003e")
    return f"{_STATE_PREFIX}{encoded} -->"


def parse_history(body: str) -> list[HistoryEntry] | None:
    """Return the history that a report body records, or None if it has none."""
    match = _STATE_PATTERN.search(body)
    if match is None:
        return None
    try:
        state = _mapping(json.loads(match.group(1)), "report state")
        runs = state.get("runs")
        if state.get("version") != _STATE_VERSION or not isinstance(runs, list):
            return None
        history: list[HistoryEntry] = []
        for value in cast(list[object], runs):
            entry = _mapping(value, "report state entry")
            history.append(
                HistoryEntry(
                    run_id=_integer(entry, "run_id", "report state entry"),
                    number=_integer(entry, "number", "report state entry"),
                    attempt=_integer(entry, "attempt", "report state entry"),
                    sha=_text(entry, "sha", "report state entry"),
                    finished_at=_text(entry, "finished_at", "report state entry"),
                    conclusion=_text(entry, "conclusion", "report state entry"),
                    infrastructure=_integer(
                        entry, "infrastructure", "report state entry"
                    ),
                    test=_integer(entry, "test", "report state entry"),
                    cancelled=_integer(entry, "cancelled", "report state entry"),
                )
            )
    except (json.JSONDecodeError, ScriptError):
        return None
    return history


def _display_time(timestamp: str) -> str:
    match = re.match(r"^(\d{4}-\d\d-\d\d)T(\d\d:\d\d)", timestamp)
    return f"{match.group(1)} {match.group(2)} UTC" if match else timestamp


def _plural(count: int, singular: str, plural: str) -> str:
    return f"{count} {singular if count == 1 else plural}"


def _code_span(text: str) -> str:
    """Quote text as inline code, which neither mentions nor links anything."""
    text = " ".join(text.split())
    longest = max((len(run) for run in re.findall(r"`+", text)), default=0)
    fence = "`" * (longest + 1)
    padding = " " if text.startswith("`") or text.endswith("`") else ""
    return f"{fence}{padding}{text}{padding}{fence}"


def _cell(text: str) -> str:
    return " ".join(text.split()).replace("|", "\\|")


def _link_text(text: str) -> str:
    return re.sub(r"([\[\]|\\])", r"\\\1", " ".join(text.split()))


def _failure_table(failures: Sequence[JobFailure], row_limit: int | None) -> list[str]:
    lines = ["| Job | Step | Cause | Evidence |", "| --- | --- | --- | --- |"]
    shown = failures if row_limit is None else failures[:row_limit]
    for failure in shown:
        evidence = failure.evidence and _code_span(
            _truncate(failure.evidence, EVIDENCE_LENGTH)
        )
        lines.append(
            f"| [{_link_text(failure.name)}]({failure.url}) "
            f"| {_cell(failure.step or '-')} | {failure.cause} "
            f"| {_cell(evidence or '-')} |"
        )
    if len(shown) < len(failures):
        lines.append(f"| and {len(failures) - len(shown)} more | | | |")
    return lines


def _excerpt(failure: JobFailure) -> list[str]:
    longest = max(
        (len(run) for line in failure.excerpt for run in re.findall(r"`+", line)),
        default=0,
    )
    fence = "`" * max(3, longest + 1)
    step = f"{failure.step} in " if failure.step else ""
    return [
        "<details>",
        f"<summary>Last log lines of {html.escape(step + failure.name)}</summary>",
        "",
        f"{fence}text",
        *failure.excerpt,
        fence,
        "",
        "</details>",
    ]


def render_run_details(
    report: RunReport, *, excerpts: bool = True, row_limit: int | None = None
) -> str:
    """Render a run attempt's failures as Markdown."""
    run = report.run
    infrastructure = _plural(
        report.count(FailureKind.INFRASTRUCTURE),
        "infrastructure failure",
        "infrastructure failures",
    )
    test = _plural(
        report.count(FailureKind.TEST),
        "test or build failure",
        "test or build failures",
    )
    cancelled = _plural(
        report.count(FailureKind.CANCELLED), "cancelled job", "cancelled jobs"
    )
    lines = [
        f"[Run #{run.number}]({run.url}), attempt {run.attempt}, of `{run.branch}` "
        f"at [`{run.sha[:10]}`]({run.commit_url}) finished on "
        f"{_display_time(run.finished_at)} with conclusion "
        f"`{run.conclusion or 'unknown'}`: {infrastructure}, {test}, and "
        f"{cancelled}.",
    ]
    if not report.failures:
        lines += [
            "",
            "No job failed. GitHub fails a run without failing a job when it "
            "cannot start the run, for example because the workflow file is "
            "invalid; the run page names the cause.",
        ]
    for kind in FailureKind:
        failures = [failure for failure in report.failures if failure.kind is kind]
        if failures:
            lines += ["", f"### {_KIND_HEADINGS[kind]}", ""]
            lines += _failure_table(failures, row_limit)
    for failure in report.summary_failures:
        lines += [
            "",
            f"[{_link_text(failure.name)}]({failure.url}), which checks the other "
            "jobs' results, also failed.",
        ]
    if excerpts:
        for failure in report.failures:
            if failure.excerpt and failure.kind is not FailureKind.CANCELLED:
                lines += ["", *_excerpt(failure)]
    return "\n".join(lines)


def _fit(render: Callable[[bool, int | None], str]) -> str:
    """Render a body within GitHub's size limit, dropping detail as needed."""
    body = ""
    for excerpts, row_limit in ((True, None), (False, None), (False, 20), (False, 5)):
        body = render(excerpts, row_limit)
        if len(body) <= MAX_BODY_LENGTH:
            return body
    return body[:MAX_BODY_LENGTH]


def _documentation_url(run: WorkflowRun) -> str:
    return (
        f"{run.repository_url}/blob/{run.branch}/{DOCUMENTATION_PATH}"
        f"#{DOCUMENTATION_ANCHOR}"
    )


def _history_table(run: WorkflowRun, history: Sequence[HistoryEntry]) -> list[str]:
    lines = [
        "| Run | Commit | Finished | Conclusion | Infrastructure | Test or build "
        "| Cancelled |",
        "| --- | --- | --- | --- | ---: | ---: | ---: |",
    ]
    for entry in reversed(history):
        run_url = (
            f"{run.repository_url}/actions/runs/{entry.run_id}/attempts/{entry.attempt}"
        )
        commit_url = f"{run.repository_url}/commit/{entry.sha}"
        lines.append(
            f"| [#{entry.number}, attempt {entry.attempt}]({run_url}) "
            f"| [`{entry.sha[:10]}`]({commit_url}) "
            f"| {_display_time(entry.finished_at)} "
            f"| {_cell(_code_span(entry.conclusion))} "
            f"| {entry.infrastructure} | {entry.test} | {entry.cancelled} |"
        )
    return lines


def render_issue_body(
    report: RunReport,
    history: Sequence[HistoryEntry],
    owners: Sequence[str],
) -> str:
    """Render the tracking issue: the latest failing run and every earlier one."""
    run = report.run
    workflow_url = (
        f"{run.repository_url}/actions/workflows/{run.workflow_file}"
        "?query=event%3Aschedule"
    )

    def render(excerpts: bool, row_limit: int | None) -> str:
        lines = [
            REPORT_MARKER,
            _state_comment(history),
            "",
            f"The nightly full run of [{_link_text(run.workflow_name)}]({workflow_url}) "
            f"on `{run.branch}` is failing. This report follows every failing "
            "nightly run and closes when one passes; "
            f"[Nightly failure reports]({_documentation_url(run)}) describes "
            "the triage.",
        ]
        if owners:
            lines += ["", "Triage owner: " + ", ".join(f"@{owner}" for owner in owners)]
        lines += [
            "",
            "## Latest failing run",
            "",
            render_run_details(report, excerpts=excerpts, row_limit=row_limit),
            "",
            "## Failing runs",
            "",
            *_history_table(run, history),
        ]
        return "\n".join(lines) + "\n"

    return _fit(render)


def render_repeat_comment(report: RunReport) -> str:
    def render(excerpts: bool, row_limit: int | None) -> str:
        details = render_run_details(report, excerpts=excerpts, row_limit=row_limit)
        return f"The nightly full run failed again.\n\n{details}\n"

    return _fit(render)


def render_passed_comment(run: WorkflowRun, history: Sequence[HistoryEntry]) -> str:
    failing = (
        f" after {_plural(len(history), 'failing run attempt', 'failing run attempts')}"
        if history
        else ""
    )
    return (
        f"[Run #{run.number}]({run.url}), attempt {run.attempt}, passed at "
        f"[`{run.sha[:10]}`]({run.commit_url}), so the nightly full run is green "
        f"again{failing}. Closing this report.\n"
    )


@dataclass(frozen=True)
class OpenReport:
    number: int
    body: str


@dataclass(frozen=True)
class CreateIssue:
    title: str
    body: str
    labels: tuple[str, ...]
    assignees: tuple[str, ...]


@dataclass(frozen=True)
class EditIssue:
    number: int
    body: str


@dataclass(frozen=True)
class CommentOnIssue:
    number: int
    body: str


@dataclass(frozen=True)
class CloseIssue:
    number: int
    reason: str


Action = CreateIssue | EditIssue | CommentOnIssue | CloseIssue


@dataclass(frozen=True)
class ReportOptions:
    repository: str
    run_id: int
    run_attempt: int | None = None
    assignees: tuple[str, ...] = ()
    labels: tuple[str, ...] = ()
    dry_run: bool = False


@dataclass(frozen=True)
class Plan:
    """What the report does for one run attempt."""

    outcome: str
    actions: tuple[Action, ...] = ()
    report: RunReport | None = None


def _trusted_author(issue: Mapping[str, object]) -> bool:
    if issue.get("author_association") in _TRUSTED_ASSOCIATIONS:
        return True
    user = issue.get("user")
    return (
        isinstance(user, dict)
        and cast(dict[str, object], user).get("login") == REPORT_BOT
    )


def find_open_reports(api: GitHubApi, repository: str) -> list[OpenReport]:
    """Return the open tracking issues, oldest first."""
    reports: list[OpenReport] = []
    for value in api.get_all(f"/repos/{repository}/issues", {"state": "open"}):
        issue = _mapping(value, "issue")
        body = _optional_text(issue, "body") or ""
        # A report carries the marker as the first line of its body, the form
        # the reporter writes it in; a body that only quotes the marker is not
        # a report.
        if (
            "pull_request" in issue
            or not body.lstrip().startswith(REPORT_MARKER)
            or not _trusted_author(issue)
        ):
            continue
        reports.append(OpenReport(_integer(issue, "number", "issue"), body))
    return sorted(reports, key=lambda report: report.number)


def _latest_result(
    api: GitHubApi, repository: str, run: WorkflowRun
) -> tuple[int, int] | None:
    """Return the run ID and attempt of the latest nightly result, if any.

    The latest result is the newest scheduled run's latest completed attempt:
    a run that completed has its current attempt's result, and a run that is
    being rerun still has its previous attempt's. A run on its first attempt
    has no result yet.
    """
    document = _mapping(
        api.get(
            f"/repos/{repository}/actions/workflows/{run.workflow_id}/runs",
            {
                "event": NIGHTLY_EVENT,
                "branch": run.branch,
                "per_page": str(_RECENT_RUNS),
            },
        ),
        "workflow run list",
    )
    runs = document.get("workflow_runs")
    for value in cast(list[object], runs) if isinstance(runs, list) else []:
        candidate = _parse_run(value)
        if candidate.status == "completed":
            return candidate.id, candidate.attempt
        if candidate.attempt > 1:
            return candidate.id, candidate.attempt - 1
    return None


def _existing_labels(
    api: GitHubApi, repository: str, labels: Sequence[str]
) -> list[str]:
    existing: list[str] = []
    for label in labels:
        path = f"/repos/{repository}/labels/{urllib.parse.quote(label, safe='')}"
        try:
            found = api.exists(path)
        except ScriptError as error:
            print(f"warning: cannot look up label {label!r}; omitting it: {error}")
            continue
        if found:
            existing.append(label)
        else:
            print(f"warning: label {label!r} does not exist; the report omits it")
    return existing


def _close(number: int, comment: str, reason: str) -> list[Action]:
    # Closing first keeps a rerun from commenting twice if the comment fails.
    return [CloseIssue(number, reason), CommentOnIssue(number, comment)]


def plan_report(api: GitHubApi, options: ReportOptions) -> Plan:
    """Decide how the tracking issue reflects the latest nightly result.

    A completed run triggers the report, but the report follows the latest
    nightly result, which a later run or attempt may already hold: reporters
    of runs that complete close together may run in either order, and one may
    replace another that waits for its turn.
    """
    repository = options.repository
    trigger = _parse_run(api.get(f"/repos/{repository}/actions/runs/{options.run_id}"))
    attempt = options.run_attempt or trigger.attempt
    if attempt > trigger.attempt:
        raise ScriptError(f"run {trigger.id} has no attempt {attempt}")
    target = (trigger.id, attempt)
    if trigger.event != NIGHTLY_EVENT:
        if not options.dry_run:
            raise ScriptError(
                f"run {trigger.id} is a {trigger.event} run; only scheduled runs "
                "are reported"
            )
        print(
            f"note: run {trigger.id} is a {trigger.event} run; the report treats "
            "it as a scheduled run"
        )
    else:
        latest = _latest_result(api, repository, trigger)
        if latest is not None and latest != target:
            print(
                f"note: reporting run {latest[0]} attempt {latest[1]}, the latest "
                f"nightly result, instead of run {trigger.id} attempt {attempt}"
            )
            target = latest
    run = _parse_run(
        api.get(f"/repos/{repository}/actions/runs/{target[0]}/attempts/{target[1]}")
    )
    if run.status != "completed":
        raise ScriptError(f"run {run.id} attempt {run.attempt} has not completed")

    reports = find_open_reports(api, repository)
    histories = {report.number: parse_history(report.body) for report in reports}
    recorded = max(
        (entry.number for history in histories.values() for entry in history or ()),
        default=0,
    )
    if run.number < recorded:
        # A report's history only holds results that were current when they
        # were reported, so an older run's result is stale.
        return Plan(
            f"skipped: run #{run.number} is older than run #{recorded}, which "
            "the report records"
        )
    if run.conclusion in RUN_PASSED_CONCLUSIONS:
        if not reports:
            return Plan(f"run #{run.number} passed; no report is open")
        actions: list[Action] = []
        for report in reports:
            actions += _close(
                report.number,
                render_passed_comment(run, histories[report.number] or []),
                "completed",
            )
        numbers = ", ".join(f"#{report.number}" for report in reports)
        return Plan(f"run #{run.number} passed; closing {numbers}", tuple(actions))
    if run.conclusion not in RUN_FAILED_CONCLUSIONS:
        return Plan(f"run #{run.number} concluded {run.conclusion}; nothing to report")

    report = build_run_report(api, repository, run)
    entry = HistoryEntry.from_report(report)
    if not reports:
        labels = _existing_labels(api, repository, options.labels)
        body = render_issue_body(report, [entry], options.assignees)
        return Plan(
            f"run #{run.number} failed; opening a report",
            (CreateIssue(REPORT_TITLE, body, tuple(labels), options.assignees),),
            report,
        )

    primary, *duplicates = reports
    history = histories[primary.number]
    if history is None:
        print(f"warning: #{primary.number} records no history; starting a new one")
        history = []
    reported = any(entry.same_attempt(previous) for previous in history)
    history = [previous for previous in history if not entry.same_attempt(previous)]
    history = [*history, entry][-MAX_HISTORY:]
    body = render_issue_body(report, history, options.assignees)
    actions = [] if body == primary.body else [EditIssue(primary.number, body)]
    if not reported:
        actions.append(CommentOnIssue(primary.number, render_repeat_comment(report)))
    for duplicate in duplicates:
        actions += _close(
            duplicate.number,
            f"#{primary.number} tracks this failure; closing this duplicate.\n",
            "not_planned",
        )
    if not reported:
        outcome = f"run #{run.number} failed; updating #{primary.number}"
    else:
        outcome = (
            f"run #{run.number} attempt {run.attempt} is already in "
            f"#{primary.number}" + ("; refreshing it" if body != primary.body else "")
        )
    return Plan(outcome, tuple(actions), report)


def _create_issue(api: GitHubApi, repository: str, action: CreateIssue) -> int:
    request: dict[str, object] = {"title": action.title, "body": action.body}
    if action.labels:
        request["labels"] = list(action.labels)
    if action.assignees:
        request["assignees"] = list(action.assignees)
    try:
        created = api.post(f"/repos/{repository}/issues", request)
    except GitHubApiError as error:
        # GitHub rejects an assignee who cannot be assigned; the body still
        # names the owner.
        if error.status != 422 or not action.assignees:
            raise
        print(f"warning: GitHub rejected the assignees ({error}); retrying without")
        del request["assignees"]
        created = api.post(f"/repos/{repository}/issues", request)
    return _integer(_mapping(created, "issue"), "number", "issue")


def apply_plan(api: GitHubApi, repository: str, plan: Plan) -> list[str]:
    """Carry out a plan's actions in order and describe each one."""
    done: list[str] = []
    for action in plan.actions:
        if isinstance(action, CreateIssue):
            number = _create_issue(api, repository, action)
            done.append(f"opened #{number}")
        elif isinstance(action, EditIssue):
            api.patch(
                f"/repos/{repository}/issues/{action.number}", {"body": action.body}
            )
            done.append(f"updated #{action.number}")
        elif isinstance(action, CommentOnIssue):
            api.post(
                f"/repos/{repository}/issues/{action.number}/comments",
                {"body": action.body},
            )
            done.append(f"commented on #{action.number}")
        else:
            api.patch(
                f"/repos/{repository}/issues/{action.number}",
                {"state": "closed", "state_reason": action.reason},
            )
            done.append(f"closed #{action.number} as {action.reason}")
    return done


def describe_action(action: Action) -> str:
    """Describe an action that a dry run would take, with its Markdown."""
    if isinstance(action, CreateIssue):
        labels = ", ".join(action.labels) or "none"
        assignees = ", ".join(action.assignees) or "none"
        return (
            f"would open {action.title!r} (labels: {labels}; assignees: "
            f"{assignees}):\n{action.body}"
        )
    if isinstance(action, EditIssue):
        return f"would update #{action.number}:\n{action.body}"
    if isinstance(action, CommentOnIssue):
        return f"would comment on #{action.number}:\n{action.body}"
    return f"would close #{action.number} as {action.reason}"


def _summary(plan: Plan) -> str:
    outcome = plan.outcome[:1].upper() + plan.outcome[1:]
    lines = ["## Nightly CI report", "", f"{outcome}."]
    if plan.report is not None:
        lines += ["", render_run_details(plan.report)]
    return "\n".join(lines) + "\n"


def command_report_nightly_ci(args: argparse.Namespace) -> None:
    if not _REPOSITORY_PATTERN.fullmatch(args.repository):
        raise ScriptError("--repository must be OWNER/REPOSITORY")
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not token and not args.dry_run:
        raise ScriptError(
            "set GH_TOKEN to a token that can write the repository's issues"
        )
    api = GitHubClient(token, os.environ.get("GITHUB_API_URL") or GITHUB_API_URL)
    plan = plan_report(
        api,
        ReportOptions(
            repository=args.repository,
            run_id=args.run_id,
            run_attempt=args.run_attempt,
            assignees=tuple(args.assignee or ()),
            labels=tuple(args.label or ()),
            dry_run=args.dry_run,
        ),
    )
    print(f"nightly CI report: {plan.outcome}")
    # The job summary shows the report even if GitHub then rejects a change.
    if args.summary is not None:
        with args.summary.open("a", encoding="utf-8", newline="") as summary:
            summary.write(_summary(plan))
    if args.dry_run:
        for action in plan.actions:
            print(describe_action(action))
    else:
        for done in apply_plan(api, args.repository, plan):
            print(f"nightly CI report: {done}")


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.description = (
        "Open, update, or close the tracking issue that reports failed nightly "
        "CI runs (doc/ci.md, 'Nightly failure reports')."
    )
    parser.add_argument(
        "--repository",
        required=True,
        metavar="OWNER/REPOSITORY",
        help="the GitHub repository of the CI run and the report",
    )
    parser.add_argument(
        "--run-id",
        type=positive_int,
        required=True,
        metavar="ID",
        help="the CI workflow run ID",
    )
    parser.add_argument(
        "--run-attempt",
        type=positive_int,
        metavar="N",
        help="the run attempt to report (default: the run's latest attempt)",
    )
    parser.add_argument(
        "--assignee",
        action="append",
        metavar="LOGIN",
        help="triage owner to assign a new report to and name in it; repeatable",
    )
    parser.add_argument(
        "--label",
        action="append",
        metavar="NAME",
        help="label for a new report, applied only if it exists; repeatable",
    )
    parser.add_argument(
        "--summary",
        type=Path,
        metavar="PATH",
        help="Markdown file, such as $GITHUB_STEP_SUMMARY, to append the report to",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print what the report would change without changing anything",
    )
    parser.set_defaults(handler=command_report_nightly_ci)

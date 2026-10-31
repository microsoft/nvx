"""Evidence that OpenVMM's own CI passed the OpenVMM revision that NVX pins.

NVX pins a revision of `feature/microvm/dev` in microsoft/openvmm, whose
checkin gates test it: the OpenVMM PR workflow on each pull request into the
branch that is ready for review, and the OpenVMM CI workflow on each push to
it. ``nvx.py check-openvmm-ci`` requires the jobs of those gates that cover
NVX's use of OpenVMM, the x86-64 Linux and Windows unit tests and the VMM tests
on KVM, MSHV, and Windows, to have passed for the pinned revision in either
workflow, and waits while they run. The gates' other jobs, such as the VMM tests
on TDX and SEV-SNP hosts, test OpenHCL and confidential VMs, which NVX does not
use.
"""

from __future__ import annotations

import argparse
import os
import re
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol, cast

from .common import ScriptError, git_output
from .nightly_ci import GITHUB_API_URL, GitHubClient

OPENVMM_REPOSITORY = "microsoft/openvmm"
# The workflows that run OpenVMM's checkin gates, by their paths.
OPENVMM_CI_WORKFLOWS = (
    ".github/workflows/openvmm-pr.yaml",
    ".github/workflows/openvmm-ci.yaml",
)
# The jobs of the checkin gates that must pass, each as one of the
# comma-separated parts of a job's name: the OpenVMM PR workflow names its
# Linux unit test job "clippy [macos], unit tests [x64-linux]", and the OpenVMM
# CI workflow "clippy [x64-linux, macos], unit tests [x64-linux]".
OPENVMM_CI_REQUIRED_JOBS = (
    "unit tests [x64-linux]",
    "unit tests [x64-windows]",
    "run vmm-tests [x64-linux-amd-kvm]",
    "run vmm-tests [x64-linux-intel-mshv]",
    "run vmm-tests [x64-windows-intel]",
    "run vmm-tests [x64-windows-amd]",
)
# How long the check waits between two looks at runs that have not finished.
# OpenVMM's gates take one and a half to two hours, so a look every five
# minutes adds little to a wait, and keeps a wait's requests, three a look,
# well below the rate limit that every job of a repository's runs shares.
POLL_SECONDS = 300
# A look that fails, as when GitHub's API is briefly unavailable, is retried
# at the next poll, at most this many times in a row, while the check may
# still wait; a failure that persists ends the check.
FAILED_LOOK_RETRIES = 2
_REVISION = re.compile(r"[0-9a-f]{40}")


class OpenVMMCiApi(Protocol):
    """The GitHub REST operation that the check uses."""

    def get_all(
        self,
        path: str,
        params: Mapping[str, str] | None = None,
        *,
        key: str | None = None,
        deadline: float | None = None,
    ) -> list[object]: ...


@dataclass(frozen=True)
class OpenVMMJob:
    """The latest attempt of a job of a run of OpenVMM's checkin gates."""

    name: str
    status: str
    conclusion: str | None
    url: str

    @property
    def passed(self) -> bool:
        return self.status == "completed" and self.conclusion == "success"


@dataclass(frozen=True)
class OpenVMMRun:
    """A run of OpenVMM's checkin gates for a revision, with its jobs."""

    url: str
    finished: bool
    jobs: tuple[OpenVMMJob, ...]


@dataclass(frozen=True)
class Verdict:
    """Whether a required job passed for the revision: `passed`, `pending`
    while a run may still pass it, `failed` when every run that ran it
    finished it without passing, or `missing` when no run will run it."""

    job: str
    state: str
    detail: str


def covers(name: str, job: str) -> bool:
    """Return whether a job of the checkin gates named `name` runs `job`."""
    return job in (part.strip() for part in name.split(","))


def _text(document: Mapping[str, object], field: str) -> str:
    value = document.get(field)
    if not isinstance(value, str):
        raise ScriptError(f"GitHub returned a workflow run or job without {field}")
    return value


def openvmm_ci_runs(api: OpenVMMCiApi, revision: str) -> list[OpenVMMRun]:
    """Return the runs of OpenVMM's checkin gates whose head is `revision`
    in OpenVMM's repository, with the latest attempt of each of their jobs."""
    runs: list[OpenVMMRun] = []
    for document in api.get_all(
        f"/repos/{OPENVMM_REPOSITORY}/actions/runs",
        {"head_sha": revision},
        key="workflow_runs",
    ):
        if not isinstance(document, Mapping):
            raise ScriptError("GitHub returned an invalid workflow run")
        run = cast(Mapping[str, object], document)
        head_repository = run.get("head_repository")
        # A pull request from a fork runs the fork's copy of the workflow.
        if (
            str(run.get("path", "")).partition("@")[0] not in OPENVMM_CI_WORKFLOWS
            or run.get("head_sha") != revision
            or not isinstance(head_repository, Mapping)
            or cast(Mapping[str, object], head_repository).get("full_name")
            != OPENVMM_REPOSITORY
        ):
            continue
        identifier = run.get("id")
        if not isinstance(identifier, int) or isinstance(identifier, bool):
            raise ScriptError("GitHub returned a workflow run without an id")
        jobs: list[OpenVMMJob] = []
        for item in api.get_all(
            f"/repos/{OPENVMM_REPOSITORY}/actions/runs/{identifier}/jobs",
            {"filter": "latest"},
            key="jobs",
        ):
            if not isinstance(item, Mapping):
                raise ScriptError("GitHub returned an invalid job")
            job = cast(Mapping[str, object], item)
            conclusion = job.get("conclusion")
            jobs.append(
                OpenVMMJob(
                    name=_text(job, "name"),
                    status=_text(job, "status"),
                    conclusion=conclusion if isinstance(conclusion, str) else None,
                    url=str(job.get("html_url", "")),
                )
            )
        runs.append(
            OpenVMMRun(
                url=str(run.get("html_url", "")),
                finished=run.get("status") == "completed",
                jobs=tuple(jobs),
            )
        )
    return runs


def assess_openvmm_ci(
    runs: Sequence[OpenVMMRun],
    required: Sequence[str] = OPENVMM_CI_REQUIRED_JOBS,
) -> list[Verdict]:
    """Return the verdict on each required job: passed if any run passed it.
    A run creates a job only when the jobs that it needs finish, so a run that
    has not finished may still run a job that it does not list yet."""
    verdicts: list[Verdict] = []
    for job in required:
        matches = [
            found for run in runs for found in run.jobs if covers(found.name, job)
        ]
        passed = [found for found in matches if found.passed]
        if passed:
            verdicts.append(Verdict(job, "passed", passed[0].url))
            continue
        unfinished = [found for found in matches if found.status != "completed"]
        unlisted = [
            run
            for run in runs
            if not run.finished
            and not any(covers(found.name, job) for found in run.jobs)
        ]
        if unfinished or unlisted:
            verdicts.append(
                Verdict(
                    job,
                    "pending",
                    unfinished[0].url if unfinished else unlisted[0].url,
                )
            )
        elif matches:
            verdicts.append(
                Verdict(
                    job,
                    "failed",
                    ", ".join(
                        f"{found.conclusion or found.status} {found.url}"
                        for found in matches
                    ),
                )
            )
        else:
            verdicts.append(Verdict(job, "missing", "no run of the gates ran it"))
    return verdicts


def _annotation(text: str) -> str:
    """Escape text for the message of a GitHub Actions workflow command."""
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def check_openvmm_ci(
    api: OpenVMMCiApi,
    revision: str,
    *,
    wait_seconds: float,
    sleep: Callable[[float], None] = time.sleep,
    clock: Callable[[], float] = time.monotonic,
) -> list[Verdict]:
    """Return the verdict on each required job for `revision`, looking again
    while any is pending and none failed or is missing, or after a look that
    failed, until `wait_seconds` run out."""
    deadline = clock() + wait_seconds
    failures = 0
    while True:
        try:
            verdicts = assess_openvmm_ci(openvmm_ci_runs(api, revision))
        except (ScriptError, OSError) as error:
            failures += 1
            if failures > FAILED_LOOK_RETRIES or clock() + POLL_SECONDS > deadline:
                raise
            print(
                "::warning::"
                + _annotation(
                    f"could not read OpenVMM's CI of {revision}, so the check "
                    f"looks again: {error}"
                ),
                flush=True,
            )
            sleep(POLL_SECONDS)
            continue
        failures = 0
        pending = [verdict.job for verdict in verdicts if verdict.state == "pending"]
        # A job that failed, or that no run will run, fails the check whatever
        # the pending jobs do, so the check does not wait for them.
        if (
            not pending
            or any(verdict.state in ("failed", "missing") for verdict in verdicts)
            or clock() + POLL_SECONDS > deadline
        ):
            return verdicts
        print(
            f">> waiting for OpenVMM's CI of {revision}: {', '.join(pending)}",
            flush=True,
        )
        sleep(POLL_SECONDS)


def _pinned_revision() -> str:
    """Return the OpenVMM revision that the checkout's index pins."""
    return git_output("rev-parse", ":openvmm")


def _summary(revision: str, verdicts: Sequence[Verdict]) -> str:
    rows = "".join(
        f"| `{verdict.job}` | {verdict.state} | {verdict.detail} |\n"
        for verdict in verdicts
    )
    return (
        f"### OpenVMM CI of `{revision}`\n"
        "| Job | Result | Evidence |\n"
        "| --- | --- | --- |\n"
        f"{rows}"
    )


def command_check_openvmm_ci(args: argparse.Namespace) -> None:
    revision = args.revision or _pinned_revision()
    if not _REVISION.fullmatch(revision):
        raise ScriptError(f"{revision!r} is no full OpenVMM commit SHA")
    # The repository is public, so any token reads its runs, and none does
    # too, under a lower rate limit.
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN") or None
    api = GitHubClient(token, os.environ.get("GITHUB_API_URL") or GITHUB_API_URL)
    verdicts = check_openvmm_ci(api, revision, wait_seconds=args.wait_minutes * 60)
    for verdict in verdicts:
        print(f"{verdict.state}: {verdict.job}: {verdict.detail}")
    if args.summary is not None:
        with args.summary.open("a", encoding="utf-8", newline="") as summary:
            summary.write(_summary(revision, verdicts))
    failures = [verdict for verdict in verdicts if verdict.state != "passed"]
    if failures:
        for verdict in failures:
            print(
                "::error::"
                + _annotation(
                    f"OpenVMM's CI has not passed {verdict.job} "
                    f"({verdict.state}: {verdict.detail})"
                )
            )
        raise ScriptError(
            f"OpenVMM's CI has not passed {len(failures)} required job(s) for "
            f"{revision}"
        )


def _minutes(value: str) -> int:
    if not value.isdigit():
        raise argparse.ArgumentTypeError("must be a whole number of minutes")
    return int(value)


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.description = (
        "Check that OpenVMM's checkin gates passed the jobs that cover NVX's use "
        "of OpenVMM for the pinned OpenVMM revision."
    )
    parser.add_argument(
        "--revision",
        metavar="SHA",
        help="the full OpenVMM commit to check (default: the revision that the "
        "checkout's index pins)",
    )
    parser.add_argument(
        "--wait-minutes",
        type=_minutes,
        default=0,
        metavar="MINUTES",
        help="how long to wait for runs that may still pass a job (default: 0)",
    )
    parser.add_argument(
        "--summary",
        type=Path,
        metavar="PATH",
        help="Markdown file, such as $GITHUB_STEP_SUMMARY, to append the verdicts to",
    )
    parser.set_defaults(handler=command_check_openvmm_ci)

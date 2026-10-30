"""Cancellation of the CI runs of superseded merge groups.

GitHub's merge queue tests each queued pull request in a merge group, on a
branch ``gh-readonly-queue/BASE/pr-NUMBER-PARENT``, and builds the pull request
a new group, on another parent, whenever the queue ahead of it changes: when a
group ahead fails or leaves the queue, an entry jumps the queue, or the base
branch changes outside the queue. It deletes the branch of the group that it
replaces but leaves the group's workflow runs running, so they hold their
runners to their end, although the queue never reads their checks again.

``nvx.py cancel-superseded-merge-groups`` therefore runs in each merge group's
CI run and cancels the unfinished runs of the workflow for the pull request's
earlier groups. It cancels only the runs whose groups' branches are gone or
hold another head: the queue builds a pull request's groups one after another,
so the group whose branch holds a run's head is the live one, even when a late
event started its run before the run that looks. A pull request that returns
to the queue on the same parent gets its earlier group's branch again, with a
new head, so a branch that exists may still belong to a replaced run. The run
that looks passes the same test, and cancels itself last when it fails it: a
late event can start a replaced group's run after the run of the group that
replaced it looked. See doc/ci.md, "Superseded merge groups".
"""

from __future__ import annotations

import argparse
import os
import re
import urllib.parse
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Protocol, cast

from .common import ScriptError, positive_int, workflow_path
from .nightly_ci import GITHUB_API_URL, GitHubApiError, GitHubClient

MERGE_GROUP_BRANCH = re.compile(
    r"(?:refs/heads/)?gh-readonly-queue/(?P<base>.+)/"
    r"pr-(?P<number>[1-9][0-9]*)-(?P<parent>[0-9a-f]{40})"
)
# The statuses of a run that hasn't completed, which may hold runners or wait
# for them.
UNFINISHED_RUN_STATUSES = ("queued", "in_progress", "waiting", "requested", "pending")
_REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")


class MergeQueueApi(Protocol):
    """The GitHub REST operations that the cancellation uses."""

    def get(self, path: str, params: Mapping[str, str] | None = None) -> object: ...

    def get_all(
        self,
        path: str,
        params: Mapping[str, str] | None = None,
        *,
        key: str | None = None,
        deadline: float | None = None,
    ) -> list[object]: ...

    def post(self, path: str, body: Mapping[str, object]) -> object: ...


@dataclass(frozen=True)
class MergeGroup:
    """A merge group: its base branch, its pull request, and the commit on
    which the queue built it."""

    base: str
    number: int
    parent: str

    @property
    def prefix(self) -> str:
        """The start of the branch of each group of the pull request."""
        return f"gh-readonly-queue/{self.base}/pr-{self.number}-"

    @property
    def branch(self) -> str:
        return f"{self.prefix}{self.parent}"


@dataclass(frozen=True)
class SupersededRun:
    """An unfinished workflow run of a merge group that the queue replaced."""

    id: int
    url: str
    branch: str


def merge_group(ref: str) -> MergeGroup | None:
    """Return the merge group whose branch or ref `ref` names, if it names one."""
    match = MERGE_GROUP_BRANCH.fullmatch(ref)
    if match is None:
        return None
    return MergeGroup(match["base"], int(match["number"]), match["parent"])


def superseded_runs(
    runs: Sequence[object],
    branches: Mapping[str, str],
    *,
    group: MergeGroup,
    workflow: str,
    run_id: int,
) -> list[SupersededRun]:
    """Return the unfinished runs of `workflow` among `runs` for the merge
    groups of `group`'s pull request and base whose branches are gone from
    `branches`, which maps each branch to its head, or hold another head than
    the run's, with the run `run_id`, the caller's, last."""
    superseded: dict[int, SupersededRun] = {}
    for document in runs:
        if not isinstance(document, Mapping):
            raise ScriptError("GitHub returned an invalid workflow run")
        run = cast(Mapping[str, object], document)
        identifier = run.get("id")
        branch = run.get("head_branch")
        if (
            not isinstance(identifier, int)
            or isinstance(identifier, bool)
            or run.get("event") != "merge_group"
            or run.get("status") == "completed"
            or str(run.get("path", "")).partition("@")[0] != workflow
            or not isinstance(branch, str)
        ):
            continue
        other = merge_group(branch)
        if other is None or (other.base, other.number) != (group.base, group.number):
            continue
        # A run whose head the branch holds is the live group's, and so is one
        # whose head GitHub didn't report while the branch exists.
        head = run.get("head_sha")
        if branch in branches and (
            not isinstance(head, str) or branches[branch] == head
        ):
            continue
        superseded[identifier] = SupersededRun(
            identifier, str(run.get("html_url", "")), branch
        )
    # Cancelling the caller's own run ends it, so that comes after the others.
    order = sorted(superseded)
    return [superseded[identifier] for identifier in order if identifier != run_id] + [
        superseded[identifier] for identifier in order if identifier == run_id
    ]


def _annotation(text: str) -> str:
    """Escape text for the message of a GitHub Actions workflow command."""
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def _branch_heads(document: object) -> dict[str, str]:
    """Return the head of each branch of a list of references."""
    if not isinstance(document, list):
        raise ScriptError("GitHub returned an invalid list of references")
    heads: dict[str, str] = {}
    for item in cast(list[object], document):
        reference: Mapping[str, object] = (
            cast(Mapping[str, object], item) if isinstance(item, Mapping) else {}
        )
        name = reference.get("ref")
        target = reference.get("object")
        head = (
            cast(Mapping[str, object], target).get("sha")
            if isinstance(target, Mapping)
            else None
        )
        if not isinstance(name, str) or not isinstance(head, str):
            raise ScriptError("GitHub returned an invalid reference")
        heads[name.removeprefix("refs/heads/")] = head
    return heads


def cancel_superseded_merge_groups(
    api: MergeQueueApi,
    *,
    repository: str,
    workflow_ref: str,
    run_id: int,
    ref: str,
) -> list[str]:
    """Cancel the unfinished runs of the workflow for the earlier merge groups
    of the pull request of the group that `ref` names, and the run `run_id`
    last if its group is one of them, and return a line for each run that it
    cancelled or could not cancel."""
    group = merge_group(ref)
    if group is None:
        raise ScriptError(f"{ref!r} names no merge group")
    workflow = workflow_path(workflow_ref, repository)
    name = urllib.parse.quote(PurePosixPath(workflow).name, safe="")
    runs: list[object] = []
    for status in UNFINISHED_RUN_STATUSES:
        runs += api.get_all(
            f"/repos/{repository}/actions/workflows/{name}/runs",
            {"event": "merge_group", "status": status},
            key="workflow_runs",
        )
    # The branches are read after the runs, so that a run that the list shows
    # for a live group finds that group's branch.
    branches = _branch_heads(
        api.get(
            f"/repos/{repository}/git/matching-refs/heads/"
            + urllib.parse.quote(group.prefix, safe="/")
        )
    )
    report: list[str] = []
    for run in superseded_runs(
        runs, branches, group=group, workflow=workflow, run_id=run_id
    ):
        # One failed cancellation never stops the others.
        try:
            api.post(f"/repos/{repository}/actions/runs/{run.id}/cancel", {})
        except GitHubApiError as error:
            # GitHub refuses to cancel a run that completed meanwhile.
            if error.status == 409:
                report.append(f"run {run.id} of `{run.branch}` had completed")
            else:
                report.append(
                    f"could not cancel run {run.id} of `{run.branch}`: {error}"
                )
            continue
        except (ScriptError, OSError) as error:
            report.append(f"could not cancel run {run.id} of `{run.branch}`: {error}")
            continue
        caller = " (this run)" if run.id == run_id else ""
        report.append(f"cancelled run {run.id} of `{run.branch}`{caller}: {run.url}")
    return report


def command_cancel_superseded_merge_groups(args: argparse.Namespace) -> None:
    if not _REPOSITORY.fullmatch(args.repository):
        raise ScriptError("--repository must be OWNER/REPOSITORY")
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not token:
        raise ScriptError(
            "set GH_TOKEN to a token that can cancel the repository's workflow runs"
        )
    group = merge_group(args.ref)
    if group is None:
        raise ScriptError(f"--ref {args.ref!r} names no merge group")
    api = GitHubClient(token, os.environ.get("GITHUB_API_URL") or GITHUB_API_URL)
    try:
        report = cancel_superseded_merge_groups(
            api,
            repository=args.repository,
            workflow_ref=args.workflow_ref,
            run_id=args.run_id,
            ref=args.ref,
        )
    except (ScriptError, OSError) as error:
        # A cancellation only frees runners, so a failure warns but never
        # fails the run.
        report = [f"could not look for superseded runs: {error}"]
    for line in report:
        # GitHub's error text may hold a newline or a percent sign, which would
        # end or alter the warning's message.
        print(
            f"::warning::{_annotation(line)}" if line.startswith("could not") else line
        )
    if not report:
        print(f"no earlier group of pull request #{group.number} has a run left")
    if args.summary is not None:
        with args.summary.open("a", encoding="utf-8", newline="") as summary:
            summary.write(f"### Superseded merge groups of #{group.number}\n")
            summary.write(
                "".join(f"- {line}\n" for line in report)
                or "No earlier group of the pull request has a run left.\n"
            )


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.description = (
        "Cancel the unfinished CI runs of the earlier merge groups of a merge "
        "group's pull request, which the merge queue replaced (doc/ci.md, "
        "'Superseded merge groups')."
    )
    parser.add_argument(
        "--repository",
        required=True,
        metavar="OWNER/REPOSITORY",
        help="the GitHub repository of the merge queue",
    )
    parser.add_argument(
        "--workflow-ref",
        required=True,
        metavar="REF",
        help="the run's GITHUB_WORKFLOW_REF, which names its workflow",
    )
    parser.add_argument(
        "--run-id",
        type=positive_int,
        required=True,
        metavar="ID",
        help="the merge group's own run, which is cancelled last, if its group "
        "is superseded too",
    )
    parser.add_argument(
        "--ref",
        required=True,
        metavar="REF",
        help="the merge group's ref, refs/heads/gh-readonly-queue/BASE/pr-NUMBER-SHA",
    )
    parser.add_argument(
        "--summary",
        type=Path,
        metavar="PATH",
        help="Markdown file, such as $GITHUB_STEP_SUMMARY, to append the outcome to",
    )
    parser.set_defaults(handler=command_cancel_superseded_merge_groups)

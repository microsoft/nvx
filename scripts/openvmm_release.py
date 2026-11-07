#!/usr/bin/env python3
"""Prepare, validate, publish, and land an OpenVMM microVM release.

The tool records the current microVM chain and its pending pull requests,
identifies commits that upstream may have made redundant, checks a replayed
chain commit by commit, and performs the two guarded pushes that land a
release. Networked GitHub queries and long-running validation commands are
kept behind a runner so callers can make planning and testing hermetic.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Literal, Protocol, TextIO, cast

import openvmm_git
from openvmm_git import (
    DEV_BRANCH,
    OPENVMM_DIR,
    OPENVMM_REPOSITORY,
    RELEASE_PREFIX,
    REPO_ROOT,
    ROADMAP,
    GitError,
    chain,
    count,
    git,
    git_succeeds,
    nvx_pin,
    position,
    read_scope,
    require_complete_history,
    require_remote,
    resolve_commit,
    scope_section,
)

NVX_REPOSITORY = openvmm_git.NVX_REPOSITORY
_RELEASE = re.compile(r"^v(?P<major>\d+)\.(?P<minor>\d+)$")
_RANGE_DIFF = re.compile(
    r"^\s*(?:\d+|-):\s+\S+\s+(?P<kind>[=!<>])\s+(?:\d+|-):",
    re.MULTILINE,
)
_OPEN_UPSTREAM = "Open upstream pull requests"
_GH_POLL_SECONDS = 30.0

ReleaseKind = Literal["rebase", "reorganization"]
Clock = Callable[[], float]
Sleeper = Callable[[float], None]


class ReleaseError(RuntimeError):
    """An expected release operation failed."""

    def __init__(self, message: str, exit_code: int = 1) -> None:
        super().__init__(message)
        self.exit_code = exit_code


class CommandRunner(Protocol):
    """The subprocess interface used for GitHub and validation commands."""

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        capture_output: bool,
        stream: TextIO | None = None,
    ) -> subprocess.CompletedProcess[str]: ...


class SubprocessRunner:
    """Run a command, either capturing it or streaming it to one destination."""

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        capture_output: bool,
        stream: TextIO | None = None,
    ) -> subprocess.CompletedProcess[str]:
        if capture_output:
            return subprocess.run(
                list(args),
                cwd=cwd,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
            )
        return subprocess.run(
            list(args),
            cwd=cwd,
            stdout=stream,
            stderr=stream,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )


DEFAULT_RUNNER = SubprocessRunner()


@dataclass(frozen=True)
class PullRequest:
    """An open OpenVMM pull request and the branch relation that stacks it."""

    number: int
    title: str
    head: str
    base: str
    draft: bool
    url: str

    def as_json(self) -> dict[str, object]:
        return {
            "number": self.number,
            "title": self.title,
            "headRefName": self.head,
            "baseRefName": self.base,
            "isDraft": self.draft,
            "url": self.url,
        }


@dataclass(frozen=True)
class PullRequestNode:
    """One OpenVMM pull request and pull requests stacked directly on it."""

    pull_request: PullRequest
    children: tuple[PullRequestNode, ...]

    def as_json(self) -> dict[str, object]:
        data = self.pull_request.as_json()
        data["children"] = [child.as_json() for child in self.children]
        return data


@dataclass(frozen=True)
class PinPromotion:
    """An open NVX pull request that promotes the OpenVMM pin."""

    number: int
    title: str
    url: str
    head: str

    def as_json(self) -> dict[str, object]:
        return {
            "number": self.number,
            "title": self.title,
            "url": self.url,
            "headRefName": self.head,
        }


@dataclass(frozen=True)
class Signature:
    """A commit's subject, git's %G? letter for it, and whether its object
    carries a signature, which git cannot always verify."""

    commit: str
    status: str
    subject: str
    signed: bool = True

    @property
    def state(self) -> str:
        return openvmm_git.Signature(self.commit, self.status, self.signed).state

    def as_json(self) -> dict[str, object]:
        return {
            "commit": self.commit,
            "status": self.status,
            "state": self.state,
            "subject": self.subject,
        }


SignatureQuery = Callable[[Path, str, str], Sequence[Signature]]


def _git_result(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )


def _detail(result: subprocess.CompletedProcess[str]) -> str:
    return result.stderr.strip() or result.stdout.strip() or "no diagnostic"


def _remote_ref(repo: Path, remote: str, ref: str) -> str | None:
    result = _git_result(repo, "ls-remote", "--exit-code", remote, ref)
    if result.returncode == 2:
        return None
    if result.returncode != 0:
        raise GitError(
            f"git ls-remote --exit-code {remote} {ref} failed in {repo}: "
            f"{_detail(result)}"
        )
    matches: list[str] = []
    for line in result.stdout.splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[1] == ref:
            matches.append(fields[0])
    if len(matches) != 1:
        raise GitError(
            f"git ls-remote {remote} did not return exactly one value for {ref}"
        )
    return matches[0]


def _read_roadmap(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as error:
        raise ReleaseError(f"cannot read roadmap {path}: {error}") from error


def _json_value(text: str, context: str) -> object:
    try:
        return cast(object, json.loads(text))
    except json.JSONDecodeError as error:
        raise ReleaseError(f"{context} returned invalid JSON: {error}") from error


def _record(value: object, context: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ReleaseError(f"{context} returned a non-object JSON value")
    unknown = cast(dict[object, object], value)
    if not all(isinstance(key, str) for key in unknown):
        raise ReleaseError(f"{context} returned an object with a non-string key")
    return cast(dict[str, object], unknown)


def _records(value: object, context: str) -> list[dict[str, object]]:
    if not isinstance(value, list):
        raise ReleaseError(f"{context} returned a non-array JSON value")
    return [_record(item, context) for item in cast(list[object], value)]


def _string(record: dict[str, object], field: str, context: str) -> str:
    value = record.get(field)
    if not isinstance(value, str):
        raise ReleaseError(f"{context} has no string {field} field")
    return value


def _integer(record: dict[str, object], field: str, context: str) -> int:
    value = record.get(field)
    if not isinstance(value, int) or isinstance(value, bool):
        raise ReleaseError(f"{context} has no integer {field} field")
    return value


def _boolean(record: dict[str, object], field: str, context: str) -> bool:
    value = record.get(field)
    if not isinstance(value, bool):
        raise ReleaseError(f"{context} has no boolean {field} field")
    return value


def _run_gh(
    runner: CommandRunner,
    cwd: Path,
    args: Sequence[str],
) -> object:
    command = ["gh", *args]
    result = runner.run(command, cwd=cwd, capture_output=True)
    if result.returncode != 0:
        raise ReleaseError(
            f"{' '.join(command)} failed with status {result.returncode}: "
            f"{_detail(result)}"
        )
    return _json_value(result.stdout, " ".join(command))


def _open_pull_requests(
    base: str,
    *,
    runner: CommandRunner,
    cwd: Path,
) -> list[PullRequest]:
    context = f"open OpenVMM pull requests based on {base}"
    value = _run_gh(
        runner,
        cwd,
        (
            "pr",
            "list",
            "--repo",
            OPENVMM_REPOSITORY,
            "--base",
            base,
            "--state",
            "open",
            "--json",
            "number,title,headRefName,baseRefName,isDraft,url",
        ),
    )
    pull_requests: list[PullRequest] = []
    for item in _records(value, context):
        number = _integer(item, "number", context)
        pull_requests.append(
            PullRequest(
                number=number,
                title=_string(item, "title", f"pull request #{number}"),
                head=_string(item, "headRefName", f"pull request #{number}"),
                base=_string(item, "baseRefName", f"pull request #{number}"),
                draft=_boolean(item, "isDraft", f"pull request #{number}"),
                url=_string(item, "url", f"pull request #{number}"),
            )
        )
    return pull_requests


def _pull_request_tree(
    *,
    runner: CommandRunner,
    cwd: Path,
    base: str = DEV_BRANCH,
) -> tuple[PullRequestNode, ...]:
    visited: set[str] = set()

    def visit(branch: str) -> tuple[PullRequestNode, ...]:
        if branch in visited:
            return ()
        visited.add(branch)
        nodes: list[PullRequestNode] = []
        for pull_request in _open_pull_requests(
            branch,
            runner=runner,
            cwd=cwd,
        ):
            children = visit(pull_request.head)
            nodes.append(PullRequestNode(pull_request, children))
        return tuple(nodes)

    return visit(base)


def _pin_promotions(
    *,
    runner: CommandRunner,
    cwd: Path,
) -> list[PinPromotion]:
    context = "open NVX OpenVMM pin promotions"
    value = _run_gh(
        runner,
        cwd,
        (
            "pr",
            "list",
            "--repo",
            NVX_REPOSITORY,
            "--state",
            "open",
            "--search",
            "openvmm: pin in:title",
            "--json",
            "number,title,url,headRefName",
        ),
    )
    promotions: list[PinPromotion] = []
    for item in _records(value, context):
        number = _integer(item, "number", context)
        promotions.append(
            PinPromotion(
                number=number,
                title=_string(item, "title", f"NVX pull request #{number}"),
                url=_string(item, "url", f"NVX pull request #{number}"),
                head=_string(item, "headRefName", f"NVX pull request #{number}"),
            )
        )
    return promotions


def _release_branches(
    repo: Path,
    remote: str,
) -> list[tuple[tuple[int, int], str, str]]:
    ref_prefix = f"refs/remotes/{remote}/{RELEASE_PREFIX}"
    output = git(
        repo,
        "for-each-ref",
        "--format=%(refname)%09%(objectname)",
        f"{ref_prefix}*",
    )
    releases: list[tuple[tuple[int, int], str, str]] = []
    for line in output.splitlines():
        fields = line.split("\t", 1)
        if len(fields) != 2 or not fields[0].startswith(ref_prefix):
            continue
        version = fields[0][len(ref_prefix) :]
        match = _RELEASE.fullmatch(version)
        if match is None:
            continue
        branch = f"{RELEASE_PREFIX}{version}"
        releases.append(
            (
                (int(match["major"]), int(match["minor"])),
                branch,
                fields[1],
            )
        )
    return sorted(releases, reverse=True)


def _previous_release(repo: Path, remote: str, old_head: str) -> tuple[str, str]:
    for _, branch, head in _release_branches(repo, remote):
        if git_succeeds(repo, "merge-base", "--is-ancestor", head, old_head):
            return branch, head
    raise ReleaseError(
        f"no {remote}/{RELEASE_PREFIX}vX.Y branch has a head that is an "
        f"ancestor of {old_head}"
    )


def next_version(previous_branch: str, kind: ReleaseKind) -> str:
    version = previous_branch.removeprefix(RELEASE_PREFIX)
    match = _RELEASE.fullmatch(version)
    if match is None:
        raise ReleaseError(f"previous release {previous_branch} has no vX.Y version")
    major = int(match["major"])
    minor = int(match["minor"])
    if kind == "rebase":
        return f"v{major}.{minor + 1}"
    return f"v{major + 1}.0"


def _emit(
    data: dict[str, object],
    lines: Sequence[str],
    json_output: bool,
    out: TextIO,
) -> None:
    if json_output:
        json.dump(data, out, indent=2, sort_keys=True)
        out.write("\n")
    else:
        for line in lines:
            print(line, file=out)


def _tree_lines(nodes: Sequence[PullRequestNode], indent: str = "") -> list[str]:
    lines: list[str] = []
    for node in nodes:
        pull_request = node.pull_request
        draft = " (draft)" if pull_request.draft else ""
        lines.append(
            f"{indent}- #{pull_request.number} {pull_request.title}{draft} "
            f"[{pull_request.base} <- {pull_request.head}] {pull_request.url}"
        )
        lines.extend(_tree_lines(node.children, f"{indent}  "))
    return lines


def state_command(
    *,
    openvmm: Path,
    roadmap: Path,
    nvx: Path,
    remote: str,
    kind: ReleaseKind,
    target: str,
    no_fetch: bool,
    json_output: bool,
    runner: CommandRunner = DEFAULT_RUNNER,
    out: TextIO = sys.stdout,
) -> int:
    """Record the old chain, the next release, and work that must land first."""
    require_remote(openvmm, remote, OPENVMM_REPOSITORY)
    if not no_fetch:
        git(
            openvmm,
            "fetch",
            remote,
            "main",
            DEV_BRANCH,
            f"+refs/heads/{RELEASE_PREFIX}*:refs/remotes/{remote}/{RELEASE_PREFIX}*",
        )
    require_complete_history(openvmm)

    old_head = resolve_commit(openvmm, f"{remote}/{DEV_BRANCH}")
    new_base = resolve_commit(openvmm, f"{remote}/main")
    old_base = git(openvmm, "merge-base", old_head, new_base)
    roadmap_text = _read_roadmap(roadmap)
    # The first call validates the section boundary; the second parses its fields.
    scope_section(roadmap_text)
    scope = read_scope(roadmap_text)
    if old_base != scope.fork_base:
        raise ReleaseError(
            f"roadmap fork base {scope.fork_base} does not match the chain's "
            f"merge base {old_base}; the roadmap is out of date or the chain moved"
        )

    previous_branch, previous_head = _previous_release(openvmm, remote, old_head)
    pin = nvx_pin(target, nvx)
    if not git_succeeds(openvmm, "merge-base", "--is-ancestor", pin, old_head):
        raise ReleaseError(
            f"NVX target {target} pins {pin}, which is not an ancestor of "
            f"the old development head {old_head}"
        )

    new_base_date = git(openvmm, "show", "-s", "--format=%cI", new_base)
    version = next_version(previous_branch, kind)
    release_branch = f"{RELEASE_PREFIX}{version}"
    release_ref = f"refs/heads/{release_branch}"
    if _remote_ref(openvmm, remote, release_ref) is not None:
        raise ReleaseError(f"remote release branch {release_branch} already exists")

    pull_requests = _pull_request_tree(runner=runner, cwd=nvx)
    promotions = _pin_promotions(runner=runner, cwd=nvx)
    next_steps = [
        "Land or carry every open OpenVMM pull request in dependency order.",
        (
            f"After each parent lands, retarget its stacked pull requests to "
            f"{DEV_BRANCH}."
        ),
        "Merge or close every open NVX OpenVMM pin promotion.",
        f"Replay the old chain onto {new_base} and publish {release_branch}.",
    ]
    data: dict[str, object] = {
        "command": "state",
        "kind": kind,
        "old_head": old_head,
        "old_base": old_base,
        "previous_release": {
            "branch": previous_branch,
            "head": previous_head,
        },
        "nvx_target": target,
        "nvx_pin": pin,
        "new_base": new_base,
        "new_base_committer_date": new_base_date,
        "next_version": version,
        "next_release": release_branch,
        "open_openvmm_pull_requests": [node.as_json() for node in pull_requests],
        "open_nvx_pin_promotions": [item.as_json() for item in promotions],
        "next_steps": next_steps,
    }
    lines = [
        f"Old development head: {old_head}",
        f"Old fork base: {old_base}",
        f"Previous release: {previous_branch} ({previous_head})",
        f"NVX {target} pin: {pin}",
        f"New upstream base: {new_base} ({new_base_date})",
        f"Next {kind}: {release_branch}",
        "Open OpenVMM pull requests:",
    ]
    lines.extend(_tree_lines(pull_requests) or ["- none"])
    lines.append("Open NVX pin promotions:")
    lines.extend(
        [
            f"- #{item.number} {item.title} [{item.head}] {item.url}"
            for item in promotions
        ]
        or ["- none"]
    )
    lines.append("Next steps:")
    lines.extend(f"{index}. {step}" for index, step in enumerate(next_steps, 1))
    _emit(data, lines, json_output, out)
    return 0


def _roadmap_section(text: str, heading: str) -> str:
    match = re.search(
        rf"^### {re.escape(heading)}\n(.*?)(?=^### |^## |\Z)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise ReleaseError(f"the roadmap has no '### {heading}' section")
    return match.group(1)


def _upstream_pull_request_numbers(text: str) -> list[int]:
    section = _roadmap_section(text, _OPEN_UPSTREAM)
    numbers: list[int] = []
    seen: set[int] = set()
    for match in re.finditer(r"\bmicrosoft/openvmm#(?P<number>\d+)\b", section):
        number = int(match["number"])
        if number not in seen:
            seen.add(number)
            numbers.append(number)
    return numbers


def _upstream_pull_request(
    number: int,
    *,
    runner: CommandRunner,
    cwd: Path,
) -> dict[str, object]:
    context = f"OpenVMM pull request #{number}"
    value = _run_gh(
        runner,
        cwd,
        (
            "pr",
            "view",
            str(number),
            "--repo",
            OPENVMM_REPOSITORY,
            "--json",
            "number,state,mergedAt,mergeCommit,title,url",
        ),
    )
    item = _record(value, context)
    actual_number = _integer(item, "number", context)
    state = _string(item, "state", context)
    title = _string(item, "title", context)
    url = _string(item, "url", context)
    merged_at_value = item.get("mergedAt")
    if merged_at_value is not None and not isinstance(merged_at_value, str):
        raise ReleaseError(f"{context} has an invalid mergedAt field")
    merge_commit_value = item.get("mergeCommit")
    merge_commit: str | None = None
    if merge_commit_value is not None:
        merge_commit_record = _record(merge_commit_value, f"{context} mergeCommit")
        merge_commit = _string(merge_commit_record, "oid", f"{context} mergeCommit")
    return {
        "number": actual_number,
        "state": state,
        "mergedAt": merged_at_value,
        "mergeCommit": merge_commit,
        "title": title,
        "url": url,
        "drop_candidate": state.upper() == "MERGED" or merged_at_value is not None,
    }


def drops_command(
    *,
    openvmm: Path,
    roadmap: Path,
    nvx: Path,
    old_base: str,
    old_head: str,
    new_base: str,
    json_output: bool,
    runner: CommandRunner = DEFAULT_RUNNER,
    out: TextIO = sys.stdout,
) -> int:
    """List upstream changes, patch-equivalent chain commits, and merged PRs."""
    base = resolve_commit(openvmm, old_base)
    head = resolve_commit(openvmm, old_head)
    upstream = resolve_commit(openvmm, new_base)
    upstream_count = count(openvmm, f"{base}..{upstream}")
    log_output = git(
        openvmm,
        "log",
        "--format=%h %s",
        "--max-count=200",
        f"{base}..{upstream}",
    )
    upstream_commits = log_output.splitlines()

    drop_commits: list[dict[str, object]] = []
    cherry_output = git(openvmm, "cherry", upstream, head, base)
    for line in cherry_output.splitlines():
        fields = line.split()
        if len(fields) != 2 or fields[0] not in {"+", "-"}:
            raise ReleaseError(f"git cherry returned an unexpected line: {line}")
        if fields[0] == "-":
            commit = resolve_commit(openvmm, fields[1])
            drop_commits.append(
                {
                    "position": position(openvmm, base, commit),
                    "commit": commit[:9],
                    "full_commit": commit,
                    "subject": git(openvmm, "show", "-s", "--format=%s", commit),
                }
            )

    roadmap_text = _read_roadmap(roadmap)
    pull_requests = [
        _upstream_pull_request(number, runner=runner, cwd=nvx)
        for number in _upstream_pull_request_numbers(roadmap_text)
    ]
    merged = [item for item in pull_requests if item["drop_candidate"] is True]
    data: dict[str, object] = {
        "command": "drops",
        "old_base": base,
        "old_head": head,
        "new_base": upstream,
        "upstream_commits": {
            "count": upstream_count,
            "shown": upstream_commits,
            "limit": 200,
            "truncated": upstream_count > len(upstream_commits),
        },
        "patch_equivalent_chain_commits": drop_commits,
        "roadmap_open_upstream_pull_requests": pull_requests,
        "merged_pull_request_drop_candidates": merged,
    }
    lines = [
        f"Upstream commits since {base}: {upstream_count} "
        f"({len(upstream_commits)} shown, limit 200)",
    ]
    lines.extend(f"- {item}" for item in upstream_commits)
    lines.append("Chain commits whose patches upstream has:")
    lines.extend(
        f"- {cast(int, item['position']):03d} {item['commit']} {item['subject']}"
        for item in drop_commits
    )
    if not drop_commits:
        lines.append("- none")
    lines.append("Roadmap open upstream pull requests:")
    for item in pull_requests:
        marker = "; examine as a drop candidate" if item["drop_candidate"] else ""
        lines.append(
            f"- #{item['number']} {item['state']} {item['title']} {item['url']}{marker}"
        )
    if not pull_requests:
        lines.append("- none")
    _emit(data, lines, json_output, out)
    return 0


def query_signatures(repo: Path, base: str, head: str) -> list[Signature]:
    """Read the signature of every commit in base..head, oldest first."""
    output = git(repo, "log", "--reverse", "--format=%H%x00%s", f"{base}..{head}")
    subjects: dict[str, str] = {}
    for line in output.splitlines():
        fields = line.split("\0", 1)
        if len(fields) != 2:
            raise ReleaseError("git log returned an invalid commit record")
        subjects[fields[0]] = fields[1]
    return [
        Signature(item.commit, item.code, subjects[item.commit], item.signed)
        for item in openvmm_git.signatures(repo, list(subjects))
    ]


def _range_diff_counts(output: str) -> dict[str, int]:
    counts = {"=": 0, "!": 0, "<": 0, ">": 0}
    for match in _RANGE_DIFF.finditer(output):
        counts[match["kind"]] += 1
    return counts


def check_chain_command(
    *,
    openvmm: Path,
    worktree: Path | None,
    new_base: str,
    head: str,
    old_base: str | None,
    old_head: str | None,
    json_output: bool,
    signature_query: SignatureQuery = query_signatures,
    out: TextIO = sys.stdout,
) -> int:
    """Check a replayed chain's topology, signatures, and equivalence evidence."""
    repo = worktree or openvmm
    base = resolve_commit(repo, new_base)
    tip = resolve_commit(repo, head)
    commits = chain(repo, base, tip)
    merge_output = git(
        repo,
        "log",
        "--reverse",
        "--format=%H %s",
        "--merges",
        f"{base}..{tip}",
    )
    merges = merge_output.splitlines()

    queried = list(signature_query(repo, base, tip))
    by_commit: dict[str, Signature] = {}
    for signature in queried:
        if signature.commit in by_commit:
            raise ReleaseError(
                f"signature query returned {signature.commit} more than once"
            )
        by_commit[signature.commit] = signature
    signatures: list[Signature] = []
    for commit in commits:
        signature = by_commit.get(commit)
        if signature is None:
            signature = Signature(
                commit,
                "?",
                git(repo, "show", "-s", "--format=%s", commit),
            )
        signatures.append(signature)
    # A signature that this host cannot verify is still one; only an unsigned
    # commit, or a bad or expired signature, fails the chain.
    unsigned = [
        item for item in signatures if item.state in ("unsigned", "bad", "expired")
    ]
    unverified = [item for item in signatures if item.state == "unverified"]

    range_diff_data: dict[str, object] | None = None
    tree_identical: bool | None = None
    if (old_base is None) != (old_head is None):
        raise ReleaseError("--old-base and --old-head must be supplied together")
    if old_base is not None and old_head is not None:
        previous_base = resolve_commit(repo, old_base)
        previous_head = resolve_commit(repo, old_head)
        range_diff = git(
            repo,
            "range-diff",
            f"{previous_base}..{previous_head}",
            f"{base}..{tip}",
        )
        range_diff_data = {
            "counts": _range_diff_counts(range_diff),
            "output": range_diff,
        }
        tree_identical = git(repo, "rev-parse", f"{previous_head}^{{tree}}") == git(
            repo,
            "rev-parse",
            f"{tip}^{{tree}}",
        )

    data: dict[str, object] = {
        "command": "check-chain",
        "new_base": base,
        "head": tip,
        "commit_count": len(commits),
        "merges": merges,
        "signatures": [item.as_json() for item in signatures],
        "invalid_signatures": [item.as_json() for item in unsigned],
        "unverified_signatures": [item.as_json() for item in unverified],
        "verification_hint": (
            openvmm_git.verification_hint(repo) if unverified else None
        ),
        "range_diff": range_diff_data,
        "tree_identical": tree_identical,
        "passed": not merges and not unsigned,
    }
    lines = [
        f"Chain {base}..{tip}: {len(commits)} commits",
        f"Merges: {len(merges)}",
    ]
    lines.extend(f"- {item}" for item in merges)
    lines.append(f"Unsigned commits, or bad or expired signatures: {len(unsigned)}")
    lines.extend(
        f"- {item.commit[:9]} {item.state} {item.subject}" for item in unsigned
    )
    good = len(signatures) - len(unsigned) - len(unverified)
    lines.append(f"Good signatures: {good}")
    if unverified:
        lines.append(
            f"Signed commits that this host cannot verify: {len(unverified)}; "
            + openvmm_git.verification_hint(repo)
        )
    if range_diff_data is not None:
        counts_data = cast(dict[str, int], range_diff_data["counts"])
        lines.append(
            "Range diff: "
            + ", ".join(f"{kind}={counts_data[kind]}" for kind in ("=", "!", "<", ">"))
        )
        lines.append(f"Tree identical: {'yes' if tree_identical else 'no'}")
    _emit(data, lines, json_output, out)
    return 1 if merges or unsigned else 0


def _rust_files(repo: Path, commit: str) -> list[str]:
    output = git(
        repo,
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        "--diff-filter=AM",
        commit,
        "--",
        "*.rs",
    )
    return output.splitlines()


def _selected_commits(
    repo: Path,
    base: str,
    head: str,
    selected: Sequence[str],
) -> list[str]:
    commits = chain(repo, base, head)
    if selected == ["all"] or selected == ("all",):
        return commits
    if "all" in selected:
        raise ReleaseError("--commits all cannot be combined with commit names")
    wanted: set[str] = set()
    available = set(commits)
    for revision in selected:
        commit = resolve_commit(repo, revision)
        if commit not in available:
            raise ReleaseError(f"selected commit {revision} is not in {base}..{head}")
        wanted.add(commit)
    return [commit for commit in commits if commit in wanted]


def _commit_commands(files: Sequence[str]) -> list[list[str]]:
    commands: list[list[str]] = []
    if files:
        commands.append(["rustfmt", "--check", "--edition", "2024", *files])
    commands.extend(
        (
            ["cargo", "check", "--locked", "--workspace", "--all-targets"],
            ["cargo", "xtask", "fmt", "--pass", "lints"],
        )
    )
    return commands


def check_commits_command(
    *,
    openvmm: Path,
    worktree: Path | None,
    new_base: str,
    head: str,
    commits: Sequence[str],
    dry_run: bool,
    keep_going: bool,
    json_output: bool,
    runner: CommandRunner = DEFAULT_RUNNER,
    out: TextIO = sys.stdout,
    err: TextIO = sys.stderr,
) -> int:
    """Run formatting and Cargo checks in a detached worktree per commit."""
    repo = worktree or openvmm
    base = resolve_commit(repo, new_base)
    tip = resolve_commit(repo, head)
    selected = _selected_commits(repo, base, tip, commits)
    plans: list[dict[str, object]] = []
    plan_commands: list[list[list[str]]] = []
    for commit in selected:
        files = _rust_files(repo, commit)
        commands = _commit_commands(files)
        plan_commands.append(commands)
        plans.append(
            {
                "commit": commit,
                "subject": git(repo, "show", "-s", "--format=%s", commit),
                "rust_files": files,
                "commands": commands,
            }
        )

    if dry_run:
        data: dict[str, object] = {
            "command": "check-commits",
            "new_base": base,
            "head": tip,
            "dry_run": True,
            "commits": plans,
            "passed": None,
        }
        lines = [f"Plan for {len(plans)} commits in {base}..{tip}:"]
        for plan in plans:
            lines.append(f"- {plan['commit']} {plan['subject']}")
            for command in cast(list[list[str]], plan["commands"]):
                lines.append(f"  {' '.join(command)}")
        _emit(data, lines, json_output, out)
        return 0

    results: list[dict[str, object]] = []
    failed = False
    stream = err if json_output else out
    scratch_parent = repo.parent
    try:
        scratch = tempfile.TemporaryDirectory(
            prefix=".openvmm-release-check-",
            dir=scratch_parent,
        )
    except OSError as error:
        raise ReleaseError(
            f"cannot create a scratch worktree directory beside {repo}: {error}"
        ) from error
    with scratch as scratch_name:
        pairs = zip(plans, plan_commands, strict=True)
        for index, (plan, commands) in enumerate(pairs):
            commit = cast(str, plan["commit"])
            directory = Path(scratch_name) / f"commit-{index + 1}"
            added = False
            failure: list[str] | None = None
            status = 0
            try:
                git(repo, "worktree", "add", "--detach", str(directory), commit)
                added = True
                for command in commands:
                    print(f"[{commit[:9]}] {' '.join(command)}", file=stream)
                    try:
                        completed = runner.run(
                            command,
                            cwd=directory,
                            capture_output=False,
                            stream=stream,
                        )
                    except OSError as error:
                        raise ReleaseError(
                            f"cannot run {' '.join(command)} at {commit}: {error}"
                        ) from error
                    if completed.returncode != 0:
                        failure = command
                        status = completed.returncode
                        break
            finally:
                if added:
                    git(repo, "worktree", "remove", "--force", str(directory))
            passed = failure is None
            failed = failed or not passed
            result_item: dict[str, object] = {
                **plan,
                "passed": passed,
                "failed_command": failure,
                "returncode": status,
            }
            results.append(result_item)
            print(
                f"[{commit[:9]}] {'PASS' if passed else 'FAIL'}",
                file=stream,
            )
            if not passed and not keep_going:
                break

    data = {
        "command": "check-commits",
        "new_base": base,
        "head": tip,
        "dry_run": False,
        "commits": results,
        "passed": not failed,
    }
    lines = [
        f"Checked {len(results)} of {len(plans)} selected commits: "
        f"{'PASS' if not failed else 'FAIL'}"
    ]
    _emit(data, lines, json_output, out)
    return 1 if failed else 0


def _workflow_runs(
    revision: str,
    *,
    runner: CommandRunner,
    cwd: Path,
) -> list[dict[str, object]]:
    value = _run_gh(
        runner,
        cwd,
        (
            "run",
            "list",
            "--repo",
            OPENVMM_REPOSITORY,
            "--commit",
            revision,
            "--json",
            "databaseId,status,conclusion,workflowName,url",
        ),
    )
    return _records(value, f"OpenVMM workflow runs for {revision}")


def _version(value: str) -> str:
    if _RELEASE.fullmatch(value) is None:
        raise ReleaseError(f"release version {value!r} is not vX.Y")
    return value


def publish_command(
    *,
    directory: Path,
    nvx: Path,
    remote: str,
    version: str,
    push: bool,
    wait_minutes: int,
    appear_timeout: int,
    json_output: bool,
    runner: CommandRunner = DEFAULT_RUNNER,
    clock: Clock = time.monotonic,
    sleeper: Sleeper = time.sleep,
    out: TextIO = sys.stdout,
    err: TextIO = sys.stderr,
) -> int:
    """Publish a release branch and wait for its exact-head OpenVMM CI."""
    require_remote(directory, remote, OPENVMM_REPOSITORY)
    checked_version = _version(version)
    head = resolve_commit(directory, "HEAD")
    branch = f"{RELEASE_PREFIX}{checked_version}"
    ref = f"refs/heads/{branch}"
    if _remote_ref(directory, remote, ref) is not None:
        raise ReleaseError(f"remote release branch {branch} already exists")
    push_command = [
        "git",
        "-C",
        str(directory),
        "push",
        remote,
        f"HEAD:{ref}",
    ]
    wait_command = [
        sys.executable,
        str(Path("scripts") / "nvx.py"),
        "check-openvmm-ci",
        "--revision",
        head,
        "--wait-minutes",
        str(wait_minutes),
    ]
    if not push:
        data: dict[str, object] = {
            "command": "publish",
            "dry_run": True,
            "head": head,
            "branch": branch,
            "push_command": push_command,
            "ci_command": wait_command,
            "appear_timeout": appear_timeout,
        }
        lines = [
            "Dry run; no branch was pushed.",
            f"Push: {' '.join(push_command)}",
            (
                f"Wait up to {appear_timeout} seconds for an OpenVMM workflow "
                f"run on {head}."
            ),
            f"Check CI: {' '.join(wait_command)}",
        ]
        _emit(data, lines, json_output, out)
        return 0

    git(directory, "push", remote, f"HEAD:{ref}")
    started = clock()
    runs: list[dict[str, object]] = []
    while True:
        runs = _workflow_runs(head, runner=runner, cwd=nvx)
        if runs:
            break
        elapsed = clock() - started
        if elapsed >= appear_timeout:
            raise ReleaseError(
                f"no OpenVMM workflow run appeared for {head} within "
                f"{appear_timeout} seconds"
            )
        sleeper(min(_GH_POLL_SECONDS, float(appear_timeout) - elapsed))

    stream = err if json_output else out
    try:
        completed = runner.run(
            wait_command,
            cwd=nvx,
            capture_output=False,
            stream=stream,
        )
    except OSError as error:
        raise ReleaseError(f"cannot run {' '.join(wait_command)}: {error}") from error
    data = {
        "command": "publish",
        "dry_run": False,
        "head": head,
        "branch": branch,
        "workflow_runs": runs,
        "ci_command": wait_command,
        "ci_returncode": completed.returncode,
        "passed": completed.returncode == 0,
    }
    lines = [
        f"Published {branch} at {head}.",
        f"OpenVMM workflow runs appeared: {len(runs)}.",
        f"NVX OpenVMM CI check exited {completed.returncode}.",
    ]
    _emit(data, lines, json_output, out)
    return completed.returncode


def _land_commands(
    openvmm: Path,
    remote: str,
    old_head: str,
    new_head: str,
    previous_release: str,
) -> tuple[list[str], list[str]]:
    previous_ref = f"refs/heads/{previous_release}"
    dev_ref = f"refs/heads/{DEV_BRANCH}"
    return (
        [
            "git",
            "-C",
            str(openvmm),
            "push",
            remote,
            f"{old_head}:{previous_ref}",
        ],
        [
            "git",
            "-C",
            str(openvmm),
            "push",
            f"--force-with-lease={dev_ref}:{old_head}",
            remote,
            f"{new_head}:{dev_ref}",
        ],
    )


def land_command(
    *,
    openvmm: Path,
    nvx: Path,
    remote: str,
    old_head: str,
    new_head: str,
    previous_release: str,
    push: bool,
    json_output: bool,
    runner: CommandRunner = DEFAULT_RUNNER,
    out: TextIO = sys.stdout,
) -> int:
    """Fast-forward the previous release, then move dev with an exact lease."""
    require_remote(openvmm, remote, OPENVMM_REPOSITORY)
    if not previous_release.startswith(RELEASE_PREFIX):
        raise ReleaseError(
            f"previous release {previous_release} must begin with {RELEASE_PREFIX}"
        )
    old = resolve_commit(openvmm, old_head)
    new = resolve_commit(openvmm, new_head)
    git(openvmm, "fetch", remote, DEV_BRANCH)
    current = resolve_commit(openvmm, f"{remote}/{DEV_BRANCH}")
    if current != old:
        if git_succeeds(openvmm, "merge-base", "--is-ancestor", old, current):
            additions = git(openvmm, "log", "--oneline", f"{old}..{current}")
            message = (
                f"development branch moved from {old} to {current}. Carry these "
                "commits onto the release, publish again, rerun OpenVMM CI and "
                "the NVX pin, and land again with the new heads:"
            )
            if additions:
                message = f"{message}\n{additions}"
            raise ReleaseError(message, exit_code=3)
        raise ReleaseError(
            f"development branch was rewritten: expected {old}, found {current}; "
            "stop and ask before proceeding",
            exit_code=2,
        )

    previous_ref = f"refs/heads/{previous_release}"
    previous_head = _remote_ref(openvmm, remote, previous_ref)
    if previous_head is None:
        raise ReleaseError(
            f"previous release branch {previous_release} does not exist on {remote}",
            exit_code=2,
        )
    if not git_succeeds(
        openvmm,
        "merge-base",
        "--is-ancestor",
        previous_head,
        old,
    ):
        raise ReleaseError(
            f"previous release {previous_release} at {previous_head} is not an "
            f"ancestor of old development head {old}; never force it",
            exit_code=2,
        )

    previous_push, dev_push = _land_commands(
        openvmm,
        remote,
        old,
        new,
        previous_release,
    )
    ci_command = [
        sys.executable,
        str(Path("scripts") / "nvx.py"),
        "check-openvmm-ci",
        "--revision",
        new,
    ]
    if not push:
        data: dict[str, object] = {
            "command": "land",
            "dry_run": True,
            "old_head": old,
            "new_head": new,
            "previous_release": previous_release,
            "previous_release_head": previous_head,
            "ci_command": ci_command,
            "push_commands": [previous_push, dev_push],
        }
        lines = [
            "Dry run; no branch was pushed.",
            f"First require OpenVMM CI: {' '.join(ci_command)}",
            f"Fast-forward previous release: {' '.join(previous_push)}",
            f"Verify {previous_ref} is {old} with git ls-remote.",
            f"Move development branch: {' '.join(dev_push)}",
            f"Verify refs/heads/{DEV_BRANCH} is {new} with git ls-remote.",
        ]
        _emit(data, lines, json_output, out)
        return 0

    git(openvmm, "push", remote, f"{old}:{previous_ref}")
    if _remote_ref(openvmm, remote, previous_ref) != old:
        raise ReleaseError(
            f"{remote}/{previous_release} did not resolve to {old} after its push"
        )
    dev_ref = f"refs/heads/{DEV_BRANCH}"
    git(
        openvmm,
        "push",
        f"--force-with-lease={dev_ref}:{old}",
        remote,
        f"{new}:{dev_ref}",
    )
    if _remote_ref(openvmm, remote, dev_ref) != new:
        raise ReleaseError(
            f"{remote}/{DEV_BRANCH} did not resolve to {new} after its push"
        )
    pull_requests = _open_pull_requests(DEV_BRANCH, runner=runner, cwd=nvx)
    data = {
        "command": "land",
        "dry_run": False,
        "old_head": old,
        "new_head": new,
        "previous_release": previous_release,
        "open_pull_requests_needing_rebase": [item.as_json() for item in pull_requests],
        "passed": True,
    }
    lines = [
        f"Fast-forwarded {previous_release} to {old}.",
        f"Moved {DEV_BRANCH} to {new} with a lease on {old}.",
        "Open pull requests that now need rebases:",
    ]
    lines.extend(
        f"- #{item.number} {item.title} [{item.head}] {item.url}"
        for item in pull_requests
    )
    if not pull_requests:
        lines.append("- none")
    _emit(data, lines, json_output, out)
    return 0


def _common_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument(
        "--openvmm",
        type=Path,
        default=OPENVMM_DIR,
        help=f"OpenVMM checkout (default: {OPENVMM_DIR})",
    )
    parser.add_argument(
        "--roadmap",
        type=Path,
        default=ROADMAP,
        help=f"OpenVMM roadmap (default: {ROADMAP})",
    )
    parser.add_argument(
        "--nvx",
        type=Path,
        default=REPO_ROOT,
        help=f"NVX checkout (default: {REPO_ROOT})",
    )
    parser.add_argument("--json", action="store_true", help="print JSON")
    return parser


def build_parser() -> argparse.ArgumentParser:
    """Build the command-line parser."""
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    common = _common_parser()

    state = subparsers.add_parser("state", parents=[common])
    state.add_argument("--remote", required=True)
    state.add_argument("--kind", choices=("rebase", "reorganization"), required=True)
    state.add_argument("--target", default="origin/dev")
    state.add_argument("--no-fetch", action="store_true")

    drops = subparsers.add_parser("drops", parents=[common])
    drops.add_argument("--old-base", required=True)
    drops.add_argument("--old-head", required=True)
    drops.add_argument("--new-base", required=True)

    check_chain = subparsers.add_parser("check-chain", parents=[common])
    check_chain.add_argument("--new-base", required=True)
    check_chain.add_argument("--dir", dest="worktree", type=Path)
    check_chain.add_argument("--head", default="HEAD")
    check_chain.add_argument("--old-base")
    check_chain.add_argument("--old-head")

    check_commits = subparsers.add_parser("check-commits", parents=[common])
    check_commits.add_argument("--new-base", required=True)
    check_commits.add_argument("--dir", dest="worktree", type=Path)
    check_commits.add_argument("--head", default="HEAD")
    check_commits.add_argument("--commits", nargs="+", default=["all"])
    check_commits.add_argument("--dry-run", action="store_true")
    check_commits.add_argument("--keep-going", action="store_true")

    publish = subparsers.add_parser("publish", parents=[common])
    publish.add_argument("--dir", dest="directory", type=Path, required=True)
    publish.add_argument("--version", required=True)
    publish.add_argument("--remote", required=True)
    publish.add_argument("--push", action="store_true")
    publish.add_argument("--wait-minutes", type=int, default=300)
    publish.add_argument("--appear-timeout", type=int, default=1800)

    land = subparsers.add_parser("land", parents=[common])
    land.add_argument("--remote", required=True)
    land.add_argument("--old-head", required=True)
    land.add_argument("--new-head", required=True)
    land.add_argument("--prev-release", required=True)
    land.add_argument("--push", action="store_true")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    """Run an OpenVMM release subcommand."""
    args = build_parser().parse_args(argv)
    command = cast(str, args.command)
    openvmm = cast(Path, args.openvmm)
    roadmap = cast(Path, args.roadmap)
    nvx = cast(Path, args.nvx)
    json_output = cast(bool, args.json)
    try:
        if command == "state":
            return state_command(
                openvmm=openvmm,
                roadmap=roadmap,
                nvx=nvx,
                remote=cast(str, args.remote),
                kind=cast(ReleaseKind, args.kind),
                target=cast(str, args.target),
                no_fetch=cast(bool, args.no_fetch),
                json_output=json_output,
            )
        if command == "drops":
            return drops_command(
                openvmm=openvmm,
                roadmap=roadmap,
                nvx=nvx,
                old_base=cast(str, args.old_base),
                old_head=cast(str, args.old_head),
                new_base=cast(str, args.new_base),
                json_output=json_output,
            )
        if command == "check-chain":
            return check_chain_command(
                openvmm=openvmm,
                worktree=cast(Path | None, args.worktree),
                new_base=cast(str, args.new_base),
                head=cast(str, args.head),
                old_base=cast(str | None, args.old_base),
                old_head=cast(str | None, args.old_head),
                json_output=json_output,
            )
        if command == "check-commits":
            return check_commits_command(
                openvmm=openvmm,
                worktree=cast(Path | None, args.worktree),
                new_base=cast(str, args.new_base),
                head=cast(str, args.head),
                commits=cast(list[str], args.commits),
                dry_run=cast(bool, args.dry_run),
                keep_going=cast(bool, args.keep_going),
                json_output=json_output,
            )
        if command == "publish":
            wait_minutes = cast(int, args.wait_minutes)
            appear_timeout = cast(int, args.appear_timeout)
            if wait_minutes < 0:
                raise ReleaseError("--wait-minutes cannot be negative")
            if appear_timeout < 0:
                raise ReleaseError("--appear-timeout cannot be negative")
            return publish_command(
                directory=cast(Path, args.directory),
                nvx=nvx,
                remote=cast(str, args.remote),
                version=cast(str, args.version),
                push=cast(bool, args.push),
                wait_minutes=wait_minutes,
                appear_timeout=appear_timeout,
                json_output=json_output,
            )
        if command == "land":
            return land_command(
                openvmm=openvmm,
                nvx=nvx,
                remote=cast(str, args.remote),
                old_head=cast(str, args.old_head),
                new_head=cast(str, args.new_head),
                previous_release=cast(str, args.prev_release),
                push=cast(bool, args.push),
                json_output=json_output,
            )
        raise ReleaseError(f"unknown command {command}")
    except (GitError, ReleaseError, OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return error.exit_code if isinstance(error, ReleaseError) else 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Find, create, and safely clean up workspaces for one NVX issue.

The tool performs the deterministic repository preparation used before fixing a
microsoft/nvx issue. It discovers existing work, creates a nontracking topic
branch either in place or in an isolated worktree, records exactly what it did,
and later restores only unchanged state.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Protocol, cast

import host_worktree
from host_worktree import GitRunner
from openvmm_git import (
    NVX_REPOSITORY,
    OPENVMM_REPOSITORY,
    REPO_ROOT,
    GitError,
    git,
    git_succeeds,
    github_repository,
    require_remote,
)

_GROUP_ORDER = (
    "pull requests",
    "linked branches",
    "branches",
    "worktrees",
    "commits not on origin/dev",
    "commits on origin/dev",
)
_SLUG = re.compile(r"[a-z0-9-]+")
# --checkout overrides a submodule.openvmm.update setting, such as none, that
# would leave OpenVMM uninitialized or away from the branch's pin.
_UPDATE_OPENVMM = ("submodule", "update", "--init", "--checkout", "openvmm")


class WorkspaceError(RuntimeError):
    """The requested issue workspace operation is unsafe or incomplete."""


@dataclass(frozen=True)
class CommandResult:
    """Captured output from a GitHub CLI or verification command."""

    returncode: int
    stdout: str = ""
    stderr: str = ""


class GhRunner(Protocol):
    """A GitHub CLI implementation used by issue discovery."""

    def __call__(self, args: Sequence[str]) -> CommandResult: ...


class VerifyRunner(Protocol):
    """A command implementation used for nvx.py verify."""

    def __call__(self, args: Sequence[str], cwd: Path) -> CommandResult: ...


def _empty_details() -> dict[str, str | int | None]:
    return {}


@dataclass
class Candidate:
    """One item that may already cover the issue."""

    group: str
    name: str
    evidence: list[str]
    url: str | None = None
    details: dict[str, str | int | None] = field(default_factory=_empty_details)

    def as_dict(self) -> dict[str, object]:
        value: dict[str, object] = {
            "group": self.group,
            "name": self.name,
            "evidence": self.evidence,
        }
        if self.url is not None:
            value["url"] = self.url
        if self.details:
            value["details"] = self.details
        return value


@dataclass(frozen=True)
class FindResult:
    """Issue discovery candidates and any unsupported-query notes."""

    candidates: list[Candidate]
    notes: list[str]

    def as_dict(self) -> dict[str, object]:
        return {
            "candidates": [candidate.as_dict() for candidate in self.candidates],
            "notes": self.notes,
        }


@dataclass(frozen=True)
class CreateResult:
    """A prepared issue branch and its verification state."""

    branch: str
    path: Path
    mode: str
    base_commit: str
    openvmm_commit: str
    verify: str
    state_file: Path

    def as_dict(self) -> dict[str, str]:
        return {
            "branch": self.branch,
            "path": str(self.path),
            "mode": self.mode,
            "base_commit": self.base_commit,
            "openvmm_commit": self.openvmm_commit,
            "verify": self.verify,
            "state_file": str(self.state_file),
        }


@dataclass(frozen=True)
class CleanupResult:
    """Actions taken, and state retained, while cleaning up an issue."""

    restored: bool
    actions: list[str]
    kept: list[str]
    state_file: Path

    def as_dict(self) -> dict[str, object]:
        return {
            "restored": self.restored,
            "actions": self.actions,
            "kept": self.kept,
            "state_file": str(self.state_file),
        }


class VerificationError(WorkspaceError):
    """nvx.py verify failed after the workspace was created."""

    def __init__(self, workspace: Path, result: CommandResult) -> None:
        detail = result.stderr.strip() or result.stdout.strip()
        super().__init__(
            f"verification failed in {workspace} with exit code "
            f"{result.returncode}: {detail or 'no output'}; the workspace was kept"
        )
        self.workspace = workspace
        self.result = result


def _default_gh_runner(args: Sequence[str]) -> CommandResult:
    result = subprocess.run(
        ["gh", *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    return CommandResult(result.returncode, result.stdout, result.stderr)


def _default_verify_runner(args: Sequence[str], cwd: Path) -> CommandResult:
    result = subprocess.run(
        list(args),
        cwd=cwd,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    return CommandResult(result.returncode, result.stdout, result.stderr)


def _json_object(text: str, description: str) -> dict[str, object]:
    try:
        value: object = json.loads(text)
    except json.JSONDecodeError as error:
        raise WorkspaceError(f"{description} returned invalid JSON: {error}") from error
    if not isinstance(value, dict):
        raise WorkspaceError(f"{description} did not return a JSON object")
    return cast(dict[str, object], value)


def _json_array(text: str, description: str) -> list[object]:
    try:
        value: object = json.loads(text)
    except json.JSONDecodeError as error:
        raise WorkspaceError(f"{description} returned invalid JSON: {error}") from error
    if not isinstance(value, list):
        raise WorkspaceError(f"{description} did not return a JSON array")
    return cast(list[object], value)


def _string(value: object, default: str = "") -> str:
    return value if isinstance(value, str) else default


def _integer(value: object) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def _reference_pattern(number: int) -> re.Pattern[str]:
    return re.compile(rf"(?:#{number}|issues/{number})(?![0-9])", re.IGNORECASE)


def _number_pattern(number: int) -> re.Pattern[str]:
    return re.compile(rf"(?<![0-9]){number}(?![0-9])")


def _gh_json(runner: GhRunner, args: Sequence[str], description: str) -> CommandResult:
    result = runner(args)
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise WorkspaceError(f"{description} failed: {detail or 'no output'}")
    return result


def _add_pull_request(
    candidates: list[Candidate],
    by_number: dict[int, Candidate],
    value: object,
    evidence: str,
) -> None:
    if not isinstance(value, dict):
        return
    item = cast(dict[str, object], value)
    number = _integer(item.get("number"))
    if number is None:
        return
    existing = by_number.get(number)
    if existing is not None:
        if evidence not in existing.evidence:
            existing.evidence.append(evidence)
        return
    title = _string(item.get("title"), "(untitled)")
    candidate = Candidate(
        group="pull requests",
        name=f"#{number} {title}",
        evidence=[evidence],
        url=_string(item.get("url")) or None,
        details={
            "number": number,
            "state": _string(item.get("state")) or None,
            "branch": _string(item.get("headRefName")) or None,
        },
    )
    candidates.append(candidate)
    by_number[number] = candidate


def _append_commit_candidates(
    candidates: list[Candidate], output: str, group: str, evidence: str
) -> None:
    for line in output.splitlines():
        parts = line.split("\t", 2)
        if len(parts) != 3:
            continue
        commit, source, subject = parts
        candidates.append(
            Candidate(
                group=group,
                name=f"{commit[:12]} {subject}",
                evidence=[evidence],
                details={"commit": commit, "source": source, "subject": subject},
            )
        )


def find_issue(
    checkout: Path,
    number: int,
    *,
    github_repo: str = NVX_REPOSITORY,
    fetch: bool = True,
    gh_runner: GhRunner = _default_gh_runner,
    git_runner: GitRunner = git,
) -> FindResult:
    """Find pull requests, branches, worktrees, and commits for an issue."""
    repo = Path(git_runner(checkout, "rev-parse", "--show-toplevel")).resolve()
    if fetch:
        git_runner(repo, "fetch", "--all", "--prune")
    candidates: list[Candidate] = []
    notes: list[str] = []
    pull_requests: dict[int, Candidate] = {}
    reference = _reference_pattern(number)

    pr_result = _gh_json(
        gh_runner,
        (
            "pr",
            "list",
            "--repo",
            github_repo,
            "--state",
            "all",
            "--search",
            str(number),
            "--json",
            "number,title,state,headRefName,url,body",
            "--limit",
            "50",
        ),
        "pull request search",
    )
    for value in _json_array(pr_result.stdout, "pull request search"):
        if not isinstance(value, dict):
            continue
        item = cast(dict[str, object], value)
        if reference.search(
            f"{_string(item.get('title'))}\n{_string(item.get('body'))}"
        ):
            _add_pull_request(
                candidates,
                pull_requests,
                item,
                f"title or body references issue #{number}",
            )

    issue_result = gh_runner(
        (
            "issue",
            "view",
            str(number),
            "--repo",
            github_repo,
            "--json",
            "closedByPullRequestsReferences,title,state,url",
        )
    )
    if issue_result.returncode == 0:
        issue = _json_object(issue_result.stdout, "issue view")
        linked = issue.get("closedByPullRequestsReferences", [])
        if isinstance(linked, list):
            for value in cast(list[object], linked):
                _add_pull_request(
                    candidates,
                    pull_requests,
                    value,
                    f"GitHub links the pull request to issue #{number}",
                )
    else:
        detail = issue_result.stderr.strip() or issue_result.stdout.strip()
        lowered = detail.lower()
        if "closedbypullrequestsreferences" in lowered and (
            "unknown" in lowered or "unsupported" in lowered or "invalid" in lowered
        ):
            notes.append(
                "GitHub does not support closedByPullRequestsReferences here; "
                "linked pull requests were skipped"
            )
        else:
            raise WorkspaceError(f"issue view failed: {detail or 'no output'}")

    develop_result = gh_runner(
        ("issue", "develop", "--list", str(number), "--repo", github_repo)
    )
    if develop_result.returncode == 0:
        for line in develop_result.stdout.splitlines():
            value = line.strip()
            if value:
                candidates.append(
                    Candidate(
                        group="linked branches",
                        name=value,
                        evidence=[f"GitHub links the branch to issue #{number}"],
                    )
                )
    else:
        detail = develop_result.stderr.strip() or develop_result.stdout.strip()
        notes.append(
            "GitHub linked-branch query was skipped: "
            f"{detail or 'gh issue develop failed'}"
        )

    separate_number = _number_pattern(number)
    refs = git_runner(
        repo,
        "for-each-ref",
        "--format=%(refname:short)",
        "refs/heads",
        "refs/remotes",
    )
    for name in refs.splitlines():
        if separate_number.search(name):
            candidates.append(
                Candidate(
                    group="branches",
                    name=name,
                    evidence=[f"branch name contains issue number {number}"],
                )
            )

    for record in host_worktree.worktree_records(repo, git_runner):
        path = record.get("worktree", "")
        branch = record.get("branch", "").removeprefix("refs/heads/")
        evidence: list[str] = []
        if separate_number.search(path):
            evidence.append(f"worktree path contains issue number {number}")
        if branch and separate_number.search(branch):
            evidence.append(f"worktree branch contains issue number {number}")
        if evidence:
            candidates.append(
                Candidate(
                    group="worktrees",
                    name=path,
                    evidence=evidence,
                    details={"branch": branch or None},
                )
            )

    grep_arguments = (
        "-E",
        "--grep",
        rf"#{number}([^0-9]|$)",
        "--grep",
        rf"issues/{number}([^0-9]|$)",
    )
    outside = git_runner(
        repo,
        "log",
        "--branches",
        "--remotes",
        "--not",
        "origin/dev",
        "--source",
        "--format=%H%x09%S%x09%s",
        *grep_arguments,
    )
    _append_commit_candidates(
        candidates,
        outside,
        "commits not on origin/dev",
        f"commit outside origin/dev mentions issue #{number}",
    )
    on_dev = git_runner(
        repo,
        "log",
        "origin/dev",
        "--source",
        "--format=%H%x09%S%x09%s",
        *grep_arguments,
    )
    _append_commit_candidates(
        candidates,
        on_dev,
        "commits on origin/dev",
        f"commit on origin/dev mentions issue #{number}",
    )
    candidates.sort(key=lambda candidate: _GROUP_ORDER.index(candidate.group))
    return FindResult(candidates, notes)


def _optional_git(repo: Path, runner: GitRunner, *args: str) -> str | None:
    try:
        value = runner(repo, *args).strip()
    except GitError:
        return None
    return value or None


def _require_signing(repo: Path, label: str, runner: GitRunner) -> None:
    setting = _optional_git(repo, runner, "config", "--bool", "--get", "commit.gpgsign")
    if setting != "true":
        raise WorkspaceError(
            f"commit.gpgsign must be true in {label}; ask the user to enable "
            "commit signing before creating the workspace"
        )


def _submodule_initialized(repo: Path) -> bool:
    return (repo / ".git").exists() and git_succeeds(
        repo, "rev-parse", "--is-inside-work-tree"
    )


def _submodule_url(repo: Path, runner: GitRunner) -> str:
    configured = _optional_git(repo, runner, "config", "--get", "submodule.openvmm.url")
    if configured is not None:
        return configured
    recorded = _optional_git(
        repo,
        runner,
        "config",
        "-f",
        str(repo / ".gitmodules"),
        "--get",
        "submodule.openvmm.url",
    )
    return recorded or ""


def _require_workspace_configuration(repo: Path, runner: GitRunner) -> None:
    try:
        require_remote(repo, "origin", NVX_REPOSITORY)
    except GitError as error:
        raise WorkspaceError(
            f"{error}; ask the user to correct both origin fetch and push URLs"
        ) from error
    _require_signing(repo, "NVX", runner)
    openvmm = repo / "openvmm"
    if _submodule_initialized(openvmm):
        _require_signing(openvmm, "openvmm", runner)
    url = _submodule_url(repo, runner)
    cloned = host_worktree.clone_url(repo, url, runner)
    if github_repository(cloned) != OPENVMM_REPOSITORY:
        raise WorkspaceError(
            f"the OpenVMM submodule URL must resolve to {OPENVMM_REPOSITORY}, "
            f"but it is {host_worktree.describe_url(url, cloned)}; ask the user "
            "to approve correcting it, including git submodule sync -- openvmm "
            "when needed"
        )


def _state_path(repo: Path, number: int, runner: GitRunner) -> Path:
    common = Path(
        runner(repo, "rev-parse", "--path-format=absolute", "--git-common-dir")
    )
    return common / "nvx-issue-workspaces" / f"{number}.json"


def _current_branch(repo: Path, runner: GitRunner) -> str | None:
    return _optional_git(repo, runner, "symbolic-ref", "--quiet", "--short", "HEAD")


def _changes(repo: Path, runner: GitRunner) -> str:
    # Name untracked files and changed submodules explicitly, since
    # status.showUntrackedFiles and submodule.openvmm.ignore can hide them from
    # the checks that guard switching the checkout and removing a worktree.
    return runner(
        repo,
        "status",
        "--porcelain",
        "--untracked-files=normal",
        "--ignore-submodules=none",
    )


def _openvmm_clean(repo: Path, runner: GitRunner) -> bool:
    openvmm = repo / "openvmm"
    return not _submodule_initialized(openvmm) or not _changes(openvmm, runner)


def _checkout_clean(repo: Path, runner: GitRunner) -> bool:
    return not _changes(repo, runner) and _openvmm_clean(repo, runner)


def _restorable_checkout(repo: Path, runner: GitRunner) -> bool:
    changes = _changes(repo, runner).splitlines()
    return _openvmm_clean(repo, runner) and all(
        change == " M openvmm" for change in changes
    )


def _main_worktree(repo: Path, runner: GitRunner) -> Path:
    records = host_worktree.worktree_records(repo, runner)
    if not records or "worktree" not in records[0]:
        raise WorkspaceError("git did not report a main worktree")
    return Path(records[0]["worktree"]).resolve()


def _fetch_base(repo: Path, base: str, runner: GitRunner) -> None:
    remote, separator, branch = base.partition("/")
    if separator and remote and branch:
        runner(repo, "fetch", remote, branch)


def _write_state(path: Path, state: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        f"{json.dumps(state, indent=2, sort_keys=True)}\n",
        encoding="utf-8",
    )


def create_workspace(
    checkout: Path,
    number: int,
    slug: str,
    *,
    base: str = "origin/dev",
    path: Path | None = None,
    verify: bool = True,
    git_runner: GitRunner = git,
    verify_runner: VerifyRunner = _default_verify_runner,
) -> CreateResult:
    """Create a nontracking issue branch in place or in a linked worktree."""
    if _SLUG.fullmatch(slug) is None:
        raise WorkspaceError(
            "SLUG must contain only lowercase letters, digits, and hyphens"
        )
    repo = Path(git_runner(checkout, "rev-parse", "--show-toplevel")).resolve()
    branch = f"agents/issue-{number}-{slug}"
    _require_workspace_configuration(repo, git_runner)
    state_file = _state_path(repo, number, git_runner)
    if state_file.exists():
        raise WorkspaceError(
            f"state already exists at {state_file}; ask the user whether to run cleanup"
        )
    _fetch_base(repo, base, git_runner)
    if git_succeeds(repo, "show-ref", "--verify", "--quiet", f"refs/heads/{branch}"):
        raise WorkspaceError(
            f"local branch {branch} already exists; ask the user whether to reuse or remove it"
        )
    if git_succeeds(
        repo,
        "show-ref",
        "--verify",
        "--quiet",
        f"refs/remotes/origin/{branch}",
    ):
        raise WorkspaceError(
            f"branch {branch} already exists on origin; ask the user whether it is existing work"
        )
    base_commit = git_runner(repo, "rev-parse", "--verify", f"{base}^{{commit}}")
    head = git_runner(repo, "rev-parse", "HEAD")
    starting_branch = _current_branch(repo, git_runner)
    clean = _checkout_clean(repo, git_runner)
    own_commits = int(git_runner(repo, "rev-list", "--count", f"{base}..HEAD"))
    in_place = clean and own_commits == 0
    workspace = (
        repo
        if in_place
        else (
            path.expanduser().resolve()
            if path is not None
            else repo.with_name(f"{repo.name}-issue-{number}")
        )
    )
    mode = "in-place" if in_place else "worktree"
    state: dict[str, object] = {
        "issue": number,
        "starting_branch": starting_branch,
        "starting_detached_sha": head if starting_branch is None else None,
        "starting_head": head,
        "main_worktree": str(_main_worktree(repo, git_runner)),
        "checkout_clean": clean,
        "branch": branch,
        "workspace": str(workspace),
        "mode": mode,
        "base_commit": base_commit,
    }
    _write_state(state_file, state)
    if in_place:
        git_runner(repo, "switch", "--no-track", "-c", branch, base_commit)
        git_runner(repo, *_UPDATE_OPENVMM)
        openvmm_commit = git_runner(repo / "openvmm", "rev-parse", "HEAD")
    else:
        added = host_worktree.add_worktree(
            repo,
            base_commit,
            path=workspace,
            branch=branch,
            git_runner=git_runner,
        )
        openvmm_commit = added.openvmm_commit
    verify_state = "skipped"
    if verify:
        result = verify_runner((sys.executable, "scripts/nvx.py", "verify"), workspace)
        if result.returncode != 0:
            raise VerificationError(workspace, result)
        verify_state = "passed"
    return CreateResult(
        branch,
        workspace,
        mode,
        base_commit,
        openvmm_commit,
        verify_state,
        state_file,
    )


def _read_state(path: Path) -> dict[str, object]:
    try:
        value: object = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise WorkspaceError(f"no issue workspace state exists at {path}") from error
    except json.JSONDecodeError as error:
        raise WorkspaceError(
            f"invalid issue workspace state at {path}: {error}"
        ) from error
    if not isinstance(value, dict):
        raise WorkspaceError(f"invalid issue workspace state at {path}")
    return cast(dict[str, object], value)


def _required_state_string(state: dict[str, object], key: str) -> str:
    value = state.get(key)
    if not isinstance(value, str) or not value:
        raise WorkspaceError(f"issue workspace state has no valid {key}")
    return value


def _optional_state_string(state: dict[str, object], key: str) -> str | None:
    value = state.get(key)
    if value is None or isinstance(value, str):
        return value
    raise WorkspaceError(f"issue workspace state has no valid {key}")


def _branch_is_checked_out(repo: Path, branch: str, runner: GitRunner) -> bool:
    expected = f"refs/heads/{branch}"
    return any(
        record.get("branch") == expected
        for record in host_worktree.worktree_records(repo, runner)
    )


def cleanup_workspace(
    checkout: Path,
    number: int,
    *,
    git_runner: GitRunner = git,
) -> CleanupResult:
    """Restore only unchanged state created for an issue workspace."""
    repo = Path(git_runner(checkout, "rev-parse", "--show-toplevel")).resolve()
    state_file = _state_path(repo, number, git_runner)
    state = _read_state(state_file)
    branch = _required_state_string(state, "branch")
    workspace = Path(_required_state_string(state, "workspace")).resolve()
    mode = _required_state_string(state, "mode")
    main = Path(_required_state_string(state, "main_worktree")).resolve()
    starting_branch = _optional_state_string(state, "starting_branch")
    starting_head = _required_state_string(state, "starting_head")
    actions: list[str] = []
    kept: list[str] = []
    workspace_restored = False

    if mode == "worktree":
        linked = any(
            Path(record.get("worktree", "")).resolve() == workspace
            for record in host_worktree.worktree_records(main, git_runner)
            if record.get("worktree")
        )
        if not linked:
            workspace_restored = True
            actions.append(f"worktree {workspace} was already absent")
        elif not _checkout_clean(workspace, git_runner):
            kept.append(f"dirty worktree {workspace}")
        elif host_worktree.unpreserved_commits(workspace, git_runner):
            kept.append(
                f"worktree {workspace}, whose HEAD reaches commits that no branch holds"
            )
        elif host_worktree.unpreserved_openvmm_commits(workspace, git_runner):
            kept.append(
                f"worktree {workspace}, whose OpenVMM repository holds commits "
                "that no remote branch holds"
            )
        else:
            host_worktree.remove_worktree(
                main,
                workspace,
                discard=True,
                git_runner=git_runner,
            )
            workspace_restored = True
            actions.append(f"removed clean worktree {workspace}")
    elif mode == "in-place":
        # create switched the checkout where it ran, which can be a linked
        # worktree rather than the main one.
        current = _current_branch(workspace, git_runner)
        already_restored = current == starting_branch and (
            starting_branch is not None
            or git_runner(workspace, "rev-parse", "HEAD") == starting_head
        )
        if already_restored:
            if not _restorable_checkout(workspace, git_runner):
                kept.append(f"dirty restored checkout {workspace}")
            else:
                git_runner(workspace, *_UPDATE_OPENVMM)
                workspace_restored = True
                actions.append(f"checkout {workspace} was already restored")
        elif current != branch:
            kept.append(
                f"checkout {workspace} is on "
                f"{current or 'a different detached commit'}, not {branch}"
            )
        elif not _checkout_clean(workspace, git_runner):
            kept.append(f"dirty in-place workspace {workspace}")
        else:
            if starting_branch is None:
                git_runner(workspace, "switch", "--detach", starting_head)
            else:
                git_runner(workspace, "switch", starting_branch)
            if _openvmm_clean(workspace, git_runner):
                git_runner(workspace, *_UPDATE_OPENVMM)
                workspace_restored = True
                actions.append(f"restored checkout {workspace}")
            else:
                kept.append(f"OpenVMM became dirty while restoring {workspace}")
    else:
        raise WorkspaceError(f"unknown workspace mode {mode}")

    branch_removed = not git_succeeds(
        main, "show-ref", "--verify", "--quiet", f"refs/heads/{branch}"
    )
    if not branch_removed and workspace_restored:
        if _branch_is_checked_out(main, branch, git_runner):
            kept.append(f"branch {branch} is still checked out")
        else:
            own_commits = int(
                git_runner(main, "rev-list", "--count", f"origin/dev..{branch}")
            )
            if own_commits:
                kept.append(
                    f"branch {branch} has {own_commits} commit(s) not on origin/dev"
                )
            else:
                git_runner(main, "branch", "-D", branch)
                branch_removed = True
                actions.append(f"deleted unchanged branch {branch}")

    restored = workspace_restored and branch_removed
    if restored:
        state_file.unlink()
        try:
            state_file.parent.rmdir()
        except OSError:
            pass
        actions.append(f"removed state {state_file}")
    return CleanupResult(restored, actions, kept, state_file)


def _print_find(result: FindResult) -> None:
    if not result.candidates:
        print("no existing work found")
    for group in _GROUP_ORDER:
        grouped = [
            candidate for candidate in result.candidates if candidate.group == group
        ]
        if not grouped:
            continue
        print(f"{group}:")
        for candidate in grouped:
            suffix = f" ({candidate.url})" if candidate.url else ""
            print(f"  - {candidate.name}{suffix}")
            for evidence in candidate.evidence:
                print(f"    evidence: {evidence}")
    for note in result.notes:
        print(f"note: {note}")


def _print_create(result: CreateResult) -> None:
    print(f"branch: {result.branch}")
    print(f"path: {result.path}")
    print(f"mode: {result.mode}")
    print(f"base commit: {result.base_commit}")
    print(f"openvmm: {result.openvmm_commit}")
    print(f"verify: {result.verify}")


def _print_cleanup(result: CleanupResult) -> None:
    for action in result.actions:
        print(f"restored: {action}")
    for kept in result.kept:
        print(f"kept: {kept}")


def _positive_issue(value: str) -> int:
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("issue number must be positive")
    return number


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo",
        default=NVX_REPOSITORY,
        help="GitHub OWNER/NAME for queries (default: microsoft/nvx)",
    )
    commands = parser.add_subparsers(dest="command", required=True)

    find = commands.add_parser("find", help="find work that may cover an issue")
    find.add_argument("number", type=_positive_issue)
    find.add_argument("--no-fetch", action="store_true")
    find.add_argument("--json", action="store_true")

    create = commands.add_parser("create", help="create an issue workspace")
    create.add_argument("number", type=_positive_issue)
    create.add_argument("slug")
    create.add_argument("--base", default="origin/dev")
    create.add_argument("--path", type=Path)
    create.add_argument("--no-verify", action="store_true")
    create.add_argument("--json", action="store_true")

    cleanup = commands.add_parser("cleanup", help="restore an unchanged workspace")
    cleanup.add_argument("number", type=_positive_issue)
    cleanup.add_argument("--json", action="store_true")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "find":
            found = find_issue(
                REPO_ROOT,
                args.number,
                github_repo=args.repo,
                fetch=not args.no_fetch,
            )
            if args.json:
                print(json.dumps(found.as_dict(), sort_keys=True))
            else:
                _print_find(found)
        elif args.command == "create":
            created = create_workspace(
                REPO_ROOT,
                args.number,
                args.slug,
                base=args.base,
                path=args.path,
                verify=not args.no_verify,
            )
            if args.json:
                print(json.dumps(created.as_dict(), sort_keys=True))
            else:
                _print_create(created)
        else:
            cleaned = cleanup_workspace(REPO_ROOT, args.number)
            if args.json:
                print(json.dumps(cleaned.as_dict(), sort_keys=True))
            else:
                _print_cleanup(cleaned)
    except VerificationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 3
    except (GitError, WorkspaceError, OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

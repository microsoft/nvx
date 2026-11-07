#!/usr/bin/env python3
"""Create and remove isolated NVX worktrees without changing their checkout.

Test-host workflows use this tool to materialize an exact NVX revision beside an
existing checkout. It validates repository URLs, initializes the pinned OpenVMM
submodule while borrowing local objects when possible, and removes only clean
linked worktrees unless discarding changes was explicitly requested.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from openvmm_git import (
    NVX_REPOSITORY,
    OPENVMM_REPOSITORY,
    REPO_ROOT,
    GitError,
    git,
    git_succeeds,
    github_repository,
)


class GitRunner(Protocol):
    """A git command implementation used by worktree operations."""

    def __call__(self, repo: Path, *args: str) -> str: ...


@dataclass(frozen=True)
class WorktreeInfo:
    """The revision and OpenVMM checkout in a newly added worktree."""

    path: Path
    commit: str
    branch: str | None
    openvmm_commit: str

    def as_dict(self) -> dict[str, str | None]:
        return {
            "path": str(self.path),
            "commit": self.commit,
            "branch": self.branch,
            "openvmm_commit": self.openvmm_commit,
        }


@dataclass(frozen=True)
class RemovalInfo:
    """A linked worktree that was removed."""

    path: Path

    def as_dict(self) -> dict[str, str | bool]:
        return {"path": str(self.path), "removed": True}


def _checkout(repo: Path, runner: GitRunner) -> Path:
    return Path(runner(repo, "rev-parse", "--show-toplevel")).resolve()


def _optional_config(repo: Path, key: str, runner: GitRunner) -> str | None:
    try:
        value = runner(repo, "config", "--get", key).strip()
    except GitError:
        return None
    return value or None


def _require_safe_urls(repo: Path, remote: str, runner: GitRunner) -> None:
    remote_url = runner(repo, "remote", "get-url", remote)
    if github_repository(remote_url) != NVX_REPOSITORY:
        raise GitError(
            f"remote {remote} must fetch from {NVX_REPOSITORY}, but it uses "
            f"{remote_url}; ask the user to correct it and approve that change"
        )
    submodule_url = _optional_config(repo, "submodule.openvmm.url", runner)
    if (
        submodule_url is not None
        and github_repository(submodule_url) != OPENVMM_REPOSITORY
    ):
        raise GitError(
            "submodule.openvmm.url must resolve to "
            f"{OPENVMM_REPOSITORY}, but it is {submodule_url}; ask the user to "
            "approve correcting or synchronizing the submodule URL"
        )


def add_worktree(
    repo: Path,
    revision: str,
    *,
    path: Path | None = None,
    fetch: str | None = None,
    remote: str = "origin",
    branch: str | None = None,
    git_runner: GitRunner = git,
) -> WorktreeInfo:
    """Add a detached or topic-branch worktree and initialize OpenVMM."""
    checkout = _checkout(repo.resolve(), git_runner)
    _require_safe_urls(checkout, remote, git_runner)
    if fetch is not None:
        git_runner(checkout, "fetch", remote, fetch)
    commit = git_runner(checkout, "rev-parse", "--verify", f"{revision}^{{commit}}")
    if branch is not None and git_succeeds(
        checkout, "show-ref", "--verify", "--quiet", f"refs/heads/{branch}"
    ):
        raise GitError(f"local branch {branch} already exists")
    short_commit = git_runner(checkout, "rev-parse", "--short", commit)
    worktree_path = (
        path.expanduser().resolve()
        if path is not None
        else checkout.with_name(f"{checkout.name}-wt-{short_commit}")
    )
    if worktree_path.exists():
        raise GitError(f"worktree path {worktree_path} already exists")
    if branch is None:
        git_runner(
            checkout,
            "worktree",
            "add",
            "--detach",
            str(worktree_path),
            commit,
        )
    else:
        git_runner(
            checkout,
            "worktree",
            "add",
            "--no-track",
            "-b",
            branch,
            str(worktree_path),
            commit,
        )
    common_dir = Path(
        git_runner(
            checkout,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        )
    )
    module_store = common_dir / "modules" / "openvmm"
    update = ["submodule", "update", "--init"]
    if module_store.exists():
        update.extend(("--reference", str(module_store)))
    update.append("openvmm")
    try:
        git_runner(worktree_path, *update)
        openvmm_commit = git_runner(worktree_path / "openvmm", "rev-parse", "HEAD")
    except GitError as error:
        raise GitError(
            f"OpenVMM initialization failed in {worktree_path}; the worktree was "
            f"kept for inspection: {error}"
        ) from error
    return WorktreeInfo(worktree_path, commit, branch, openvmm_commit)


def _worktrees(repo: Path, runner: GitRunner) -> list[tuple[Path, str | None]]:
    entries: list[tuple[Path, str | None]] = []
    path: Path | None = None
    branch: str | None = None
    for line in f"{runner(repo, 'worktree', 'list', '--porcelain')}\n\n".splitlines():
        if line.startswith("worktree "):
            path = Path(line.removeprefix("worktree ")).resolve()
        elif line.startswith("branch "):
            branch = line.removeprefix("branch ")
        elif not line and path is not None:
            entries.append((path, branch))
            path = None
            branch = None
    return entries


def _submodule_is_initialized(path: Path) -> bool:
    return path.is_dir() and git_succeeds(path, "rev-parse", "--is-inside-work-tree")


def remove_worktree(
    repo: Path,
    path: Path,
    *,
    discard: bool = False,
    git_runner: GitRunner = git,
) -> RemovalInfo:
    """Remove a linked worktree, preserving tracked and untracked changes."""
    checkout = _checkout(repo.resolve(), git_runner)
    entries = _worktrees(checkout, git_runner)
    target = path.expanduser().resolve()
    if (
        not entries
        or target == entries[0][0]
        or target not in {entry_path for entry_path, _branch in entries[1:]}
    ):
        raise GitError(
            f"{target} is not a linked worktree of {checkout}; the main worktree "
            "cannot be removed"
        )
    checkout_changes = git_runner(target, "status", "--porcelain")
    openvmm_path = target / "openvmm"
    openvmm_changes = (
        git_runner(openvmm_path, "status", "--porcelain")
        if _submodule_is_initialized(openvmm_path)
        else ""
    )
    if not discard and (checkout_changes or openvmm_changes):
        locations: list[str] = []
        if checkout_changes:
            locations.append(str(target))
        if openvmm_changes:
            locations.append(str(openvmm_path))
        raise GitError(
            "refusing to remove a dirty worktree; preserve the changes in "
            f"{', '.join(locations)}, or pass --discard"
        )
    git_runner(checkout, "worktree", "remove", "--force", str(target))
    git_runner(checkout, "worktree", "prune")
    return RemovalInfo(target)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo",
        type=Path,
        default=REPO_ROOT,
        help="NVX checkout (default: checkout containing this script)",
    )
    commands = parser.add_subparsers(dest="command", required=True)

    add = commands.add_parser("add", help="create and initialize a worktree")
    add.add_argument("revision")
    add.add_argument("--path", type=Path)
    add.add_argument("--fetch", metavar="REF")
    add.add_argument("--remote", default="origin")
    add.add_argument("--branch")
    add.add_argument("--json", action="store_true")

    remove = commands.add_parser("remove", help="remove a linked worktree")
    remove.add_argument("path", type=Path)
    remove.add_argument("--discard", action="store_true")
    remove.add_argument("--json", action="store_true")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "add":
            info = add_worktree(
                args.repo,
                args.revision,
                path=args.path,
                fetch=args.fetch,
                remote=args.remote,
                branch=args.branch,
            )
            if args.json:
                print(json.dumps(info.as_dict(), sort_keys=True))
            else:
                print(f"path: {info.path}")
                print(f"commit: {info.commit}")
                print(f"branch: {info.branch or 'detached'}")
                print(f"openvmm: {info.openvmm_commit}")
        else:
            removed = remove_worktree(
                args.repo,
                args.path,
                discard=args.discard,
            )
            if args.json:
                print(json.dumps(removed.as_dict(), sort_keys=True))
            else:
                print(f"removed: {removed.path}")
    except (GitError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

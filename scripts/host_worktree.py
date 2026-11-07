#!/usr/bin/env python3
"""Create and remove isolated NVX worktrees without changing their checkout.

Test-host workflows use this tool to materialize an exact NVX revision beside an
existing checkout. It validates repository URLs, initializes the pinned OpenVMM
submodule while borrowing local objects when possible, and removes only clean
linked worktrees whose commits other references keep, unless discarding them
was explicitly requested.
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
    OPENVMM_URL,
    REPO_ROOT,
    GitError,
    git,
    git_succeeds,
    github_repository,
    redact_url,
)


class GitRunner(Protocol):
    """A git command implementation used by worktree operations."""

    def __call__(self, repo: Path, *args: str) -> str: ...


# The file in a worktree's private Git directory that records the commit that
# add checked out, which git worktree remove deletes with that directory.
START_FILE = "nvx-host-worktree-start"


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


def _require_safe_remote(repo: Path, remote: str, runner: GitRunner) -> None:
    # git remote get-url applies any url.<base>.insteadOf rewrite.
    remote_url = runner(repo, "remote", "get-url", remote)
    if github_repository(remote_url) != NVX_REPOSITORY:
        raise GitError(
            f"remote {remote} must fetch from {NVX_REPOSITORY}, but it uses "
            f"{redact_url(remote_url)}; ask the user to correct it and approve "
            "that change"
        )


def _submodule_name(repo: Path, gitmodules: str, runner: GitRunner) -> str:
    """Return the name that a .gitmodules blob gives the submodule at openvmm."""
    try:
        listing = runner(
            repo,
            "config",
            "--blob",
            gitmodules,
            "--null",
            "--get-regexp",
            r"^submodule\..*\.path$",
        )
    except GitError:
        listing = ""
    names = [
        key.removeprefix("submodule.").removesuffix(".path")
        for key, _separator, path in (
            entry.partition("\n") for entry in listing.split("\0") if entry
        )
        if path == "openvmm"
    ]
    if len(names) != 1:
        raise GitError(
            f"{gitmodules} must name exactly one submodule at openvmm, not {len(names)}"
        )
    return names[0]


def clone_url(repo: Path, url: str, runner: GitRunner) -> str:
    """Return the URL that git fetches for url, after any url.<base>.insteadOf
    rewrite, which git ls-remote --get-url applies without contacting it."""
    return runner(repo, "ls-remote", "--get-url", "--", url).strip() if url else ""


def describe_url(url: str, cloned: str) -> str:
    """Describe url, and the URL that git clones for it, without credentials."""
    if not url:
        return "(none)"
    if cloned == url:
        return redact_url(url)
    return (
        f"{redact_url(url)}, which url.<base>.insteadOf rewrites to "
        f"{redact_url(cloned)}"
    )


def _require_safe_submodule_url(repo: Path, commit: str, runner: GitRunner) -> None:
    """Fail unless initializing OpenVMM at commit would clone OpenVMM.

    git submodule update --init finds the submodule at openvmm by the name that
    commit's .gitmodules gives it, and clones the URL that the configuration
    sets for that name, or else the one that .gitmodules records, after any
    url.<base>.insteadOf rewrite.
    """
    gitmodules = f"{commit}:.gitmodules"
    key = f"submodule.{_submodule_name(repo, gitmodules, runner)}.url"
    configured = _optional_config(repo, key, runner)
    if configured is not None:
        cloned = clone_url(repo, configured, runner)
        if github_repository(cloned) != OPENVMM_REPOSITORY:
            raise GitError(
                f"{key} must resolve to {OPENVMM_REPOSITORY}, but it is "
                f"{describe_url(configured, cloned)}; ask the user to approve "
                "correcting or synchronizing the submodule URL"
            )
        return
    try:
        recorded = runner(repo, "config", "--blob", gitmodules, "--get", key).strip()
    except GitError:
        recorded = ""
    cloned = clone_url(repo, recorded, runner)
    if github_repository(cloned) != OPENVMM_REPOSITORY:
        raise GitError(
            f"{key} is unset, so initializing OpenVMM would clone the URL that "
            f"{gitmodules} records, {describe_url(recorded, cloned)}, which does "
            f"not resolve to {OPENVMM_REPOSITORY}; ask the user to approve setting "
            f"{key} to {OPENVMM_URL}"
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
    _require_safe_remote(checkout, remote, git_runner)
    if fetch is not None:
        git_runner(checkout, "fetch", remote, fetch)
    commit = git_runner(checkout, "rev-parse", "--verify", f"{revision}^{{commit}}")
    _require_safe_submodule_url(checkout, commit, git_runner)
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
    (_git_dir(worktree_path, git_runner) / START_FILE).write_text(
        f"{commit}\n", encoding="utf-8"
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
    # --checkout overrides a submodule.openvmm.update setting, such as none,
    # that would leave OpenVMM uninitialized.
    update = ["submodule", "update", "--init", "--checkout"]
    if module_store.exists():
        update.extend(("--reference", str(module_store)))
    update.append("openvmm")
    openvmm_path = worktree_path / "openvmm"
    try:
        git_runner(worktree_path, *update)
        if not _submodule_is_initialized(openvmm_path):
            raise GitError(f"git submodule update left {openvmm_path} uninitialized")
        openvmm_commit = git_runner(openvmm_path, "rev-parse", "HEAD")
    except GitError as error:
        raise GitError(
            f"OpenVMM initialization failed in {worktree_path}; the worktree was "
            f"kept for inspection: {error}"
        ) from error
    return WorktreeInfo(worktree_path, commit, branch, openvmm_commit)


def worktree_records(repo: Path, runner: GitRunner) -> list[dict[str, str]]:
    """Return the records of git worktree list --porcelain as attribute maps.

    -z ends each line with NUL, so that a path can hold a newline; Git before
    2.36 lacks it, and ends each line with a newline.
    """
    try:
        text = runner(repo, "worktree", "list", "--porcelain", "-z")
        terminator = "\0"
    except GitError:
        text = runner(repo, "worktree", "list", "--porcelain")
        terminator = "\n"
    records: list[dict[str, str]] = []
    record: dict[str, str] = {}
    for line in [*text.split(terminator), ""]:
        if not line:
            if record:
                records.append(record)
                record = {}
            continue
        key, separator, value = line.partition(" ")
        record[key] = value if separator else ""
    return records


def _worktrees(repo: Path, runner: GitRunner) -> list[tuple[Path, str | None]]:
    return [
        (Path(record["worktree"]).resolve(), record.get("branch"))
        for record in worktree_records(repo, runner)
        if "worktree" in record
    ]


def _submodule_is_initialized(path: Path) -> bool:
    # An uninitialized submodule's directory lies in the superproject's
    # worktree, where git commands run on the superproject.
    return (path / ".git").exists() and git_succeeds(
        path, "rev-parse", "--is-inside-work-tree"
    )


def _git_dir(worktree: Path, runner: GitRunner) -> Path:
    return Path(runner(worktree, "rev-parse", "--path-format=absolute", "--git-dir"))


def unpreserved_commits(worktree: Path, runner: GitRunner) -> int:
    """Count the commits that only the worktree's HEAD reaches, which removing
    it, with its HEAD and reflog, would leave unreachable.

    A branch, a tag, a remote branch, or the commit that add checked out, such
    as a pull request's head fetched by name, preserves the commits that it
    reaches.
    """
    try:
        start = (_git_dir(worktree, runner) / START_FILE).read_text(encoding="utf-8")
    except OSError:
        start = ""
    preserved = ["--branches", "--tags", "--remotes", *start.split()]
    return int(runner(worktree, "rev-list", "--count", "HEAD", "--not", *preserved))


def remove_worktree(
    repo: Path,
    path: Path,
    *,
    discard: bool = False,
    git_runner: GitRunner = git,
) -> RemovalInfo:
    """Remove a linked worktree, preserving its changes and the commits that
    only its HEAD reaches, unless discard is set."""
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
    # Name untracked files and changed submodules explicitly, since
    # status.showUntrackedFiles and submodule.openvmm.ignore can hide them, and
    # worktree remove --force deletes them with the submodule's repository.
    status = (
        "status",
        "--porcelain",
        "--untracked-files=normal",
        "--ignore-submodules=none",
    )
    checkout_changes = git_runner(target, *status)
    openvmm_path = target / "openvmm"
    openvmm_changes = (
        git_runner(openvmm_path, *status)
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
    if not discard:
        unpreserved = unpreserved_commits(target, git_runner)
        if unpreserved:
            raise GitError(
                f"refusing to remove {target}, whose HEAD reaches {unpreserved} "
                "commit(s) that no branch, tag, or remote branch holds; create "
                "a branch for them, or pass --discard"
            )
    # worktree remove deletes the target's records itself; git worktree prune
    # would also delete those of every other worktree that looks missing, such
    # as one on a drive that is not mounted.
    git_runner(checkout, "worktree", "remove", "--force", str(target))
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

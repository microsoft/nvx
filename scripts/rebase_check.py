#!/usr/bin/env python3
"""Record and verify the invariants of an NVX branch rebase.

Before a rebase, this tool records commit identities, stable patch IDs,
signatures, merge commits, and OpenVMM state. Afterward it detects incomplete
rebases, conflicts, dropped or unexpected commits, lost signatures, an incorrect
base, and a submodule checkout that no longer matches the branch's gitlink.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from openvmm_git import (
    REPO_ROOT,
    GitError,
    git,
    git_succeeds,
    signatures,
    verification_hint,
)

PUBLISH_NOTE = (
    "Publishing the rewritten branch needs git push --force-with-lease, which "
    "restarts CI for a Ready pull request and triggers a new Copilot review."
)


class RebaseCheckError(RuntimeError):
    """The rebase state cannot be recorded or verified safely."""


@dataclass(frozen=True)
class CommitRecord:
    """A branch commit's identity across history rewriting."""

    sha: str
    subject: str
    patch_id: str | None
    signature: str
    signed: bool
    signature_state: str

    def as_dict(self) -> dict[str, str | bool | None]:
        return {
            "sha": self.sha,
            "subject": self.subject,
            "patch_id": self.patch_id,
            "signature": self.signature,
            "signed": self.signed,
            "signature_state": self.signature_state,
        }


@dataclass(frozen=True)
class RecordResult:
    """The recorded state file and warnings that need a decision."""

    state_file: Path
    branch: str
    head: str
    target: str
    target_commit: str
    commits: list[CommitRecord]
    commit_gpgsign: bool
    warnings: list[str]

    def as_dict(self) -> dict[str, object]:
        return {
            "state_file": str(self.state_file),
            "branch": self.branch,
            "head": self.head,
            "target": self.target,
            "target_commit": self.target_commit,
            "commits": [commit.as_dict() for commit in self.commits],
            "commit_gpgsign": self.commit_gpgsign,
            "warnings": self.warnings,
        }


@dataclass(frozen=True)
class VerifyResult:
    """The result of comparing the current branch with recorded state."""

    ok: bool
    problems: list[str]
    dropped_commits: list[CommitRecord]
    new_commits: list[CommitRecord]
    warnings: list[str]
    note: str = PUBLISH_NOTE

    def as_dict(self) -> dict[str, object]:
        return {
            "ok": self.ok,
            "problems": self.problems,
            "dropped_commits": [commit.as_dict() for commit in self.dropped_commits],
            "new_commits": [commit.as_dict() for commit in self.new_commits],
            "warnings": self.warnings,
            "note": self.note,
        }


def _git_path(repo: Path, name: str) -> Path:
    path = Path(git(repo, "rev-parse", "--git-path", name))
    return path if path.is_absolute() else repo / path


def _default_state_path(repo: Path) -> Path:
    return _git_path(repo, "nvx-rebase-check.json").resolve()


def _rebase_paths(repo: Path) -> tuple[Path, Path]:
    return (_git_path(repo, "rebase-merge"), _git_path(repo, "rebase-apply"))


def _rebase_in_progress(repo: Path) -> bool:
    return any(path.exists() for path in _rebase_paths(repo))


def _patch_id(repo: Path, commit: str) -> str | None:
    shown = subprocess.run(
        ["git", "-C", str(repo), "show", commit],
        capture_output=True,
        check=False,
    )
    if shown.returncode != 0:
        detail = shown.stderr.decode("utf-8", errors="replace").strip()
        raise GitError(f"git show {commit} failed in {repo}: {detail}")
    patched = subprocess.run(
        ["git", "-C", str(repo), "patch-id", "--stable"],
        input=shown.stdout,
        capture_output=True,
        check=False,
    )
    if patched.returncode != 0:
        detail = patched.stderr.decode("utf-8", errors="replace").strip()
        raise GitError(f"git patch-id --stable failed in {repo}: {detail}")
    output = patched.stdout.decode("ascii", errors="replace").strip()
    return output.split(maxsplit=1)[0] if output else None


def _range_commits(repo: Path, target: str) -> list[CommitRecord]:
    commits = git(repo, "rev-list", "--reverse", f"{target}..HEAD").splitlines()
    commit_signatures = signatures(repo, commits)
    return [
        CommitRecord(
            sha=commit,
            subject=git(repo, "show", "-s", "--format=%s", commit),
            patch_id=_patch_id(repo, commit),
            signature=signature.code,
            signed=signature.signed,
            signature_state=signature.state,
        )
        for commit, signature in zip(commits, commit_signatures, strict=True)
    ]


def _commit_signing_enabled(repo: Path) -> bool:
    if not git_succeeds(repo, "config", "--bool", "--get", "commit.gpgsign"):
        return False
    return git(repo, "config", "--bool", "--get", "commit.gpgsign") == "true"


def _openvmm_state(repo: Path, gitlink: str) -> tuple[str | None, bool, bool]:
    openvmm = repo / "openvmm"
    initialized = (openvmm / ".git").exists() and git_succeeds(
        openvmm, "rev-parse", "--is-inside-work-tree"
    )
    if not initialized:
        return None, False, True
    checked_out = git(openvmm, "rev-parse", "HEAD")
    dirty = bool(git(openvmm, "status", "--porcelain"))
    return checked_out, dirty, checked_out != gitlink


def record_rebase(
    checkout: Path,
    *,
    target: str = "origin/dev",
    state_file: Path | None = None,
) -> RecordResult:
    """Record the current branch's commits and rebase-sensitive state."""
    repo = Path(git(checkout, "rev-parse", "--show-toplevel")).resolve()
    branch = git(repo, "symbolic-ref", "--quiet", "--short", "HEAD")
    head = git(repo, "rev-parse", "HEAD")
    target_commit = git(repo, "rev-parse", "--verify", f"{target}^{{commit}}")
    commits = _range_commits(repo, target_commit)
    signing_required = _commit_signing_enabled(repo)
    merges = git(
        repo, "rev-list", "--reverse", "--merges", f"{target_commit}..HEAD"
    ).splitlines()
    gitlink = git(repo, "rev-parse", "HEAD:openvmm")
    checked_out, openvmm_dirty, openvmm_off_pin = _openvmm_state(repo, gitlink)
    rebase_active = _rebase_in_progress(repo)
    warnings: list[str] = []
    if merges:
        warnings.append(
            "the range contains merge commits; a plain rebase drops them, so ask "
            "before flattening the history"
        )
    if rebase_active:
        warnings.append("a rebase is already in progress")
    unverified = [
        commit for commit in commits if commit.signature_state == "unverified"
    ]
    if unverified:
        warnings.append(
            f"{len(unverified)} commit signature(s) are unverified; "
            f"{verification_hint(repo)}"
        )
    destination = (
        state_file.expanduser().resolve()
        if state_file is not None
        else _default_state_path(repo)
    )
    state: dict[str, object] = {
        "version": 2,
        "branch": branch,
        "head": head,
        "target": target,
        "target_commit": target_commit,
        "commits": [commit.as_dict() for commit in commits],
        "commit_gpgsign": signing_required,
        "merge_commits": merges,
        "gitlink": gitlink,
        "openvmm_checked_out": checked_out,
        "openvmm_dirty": openvmm_dirty,
        "openvmm_off_pin": openvmm_off_pin,
        "rebase_in_progress": rebase_active,
    }
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(
        f"{json.dumps(state, indent=2, sort_keys=True)}\n", encoding="utf-8"
    )
    return RecordResult(
        destination,
        branch,
        head,
        target,
        target_commit,
        commits,
        signing_required,
        warnings,
    )


def _read_state(path: Path) -> dict[str, object]:
    try:
        value: object = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise RebaseCheckError(f"rebase state does not exist at {path}") from error
    except json.JSONDecodeError as error:
        raise RebaseCheckError(f"invalid rebase state at {path}: {error}") from error
    if not isinstance(value, dict):
        raise RebaseCheckError(f"invalid rebase state at {path}")
    return cast(dict[str, object], value)


def _state_string(state: dict[str, object], key: str) -> str:
    value = state.get(key)
    if not isinstance(value, str) or not value:
        raise RebaseCheckError(f"rebase state has no valid {key}")
    return value


def _state_bool(state: dict[str, object], key: str) -> bool:
    value = state.get(key)
    if not isinstance(value, bool):
        raise RebaseCheckError(f"rebase state has no valid {key}")
    return value


def _state_commits(state: dict[str, object]) -> list[CommitRecord]:
    values = state.get("commits")
    if not isinstance(values, list):
        raise RebaseCheckError("rebase state has no valid commits")
    commits: list[CommitRecord] = []
    for value in cast(list[object], values):
        if not isinstance(value, dict):
            raise RebaseCheckError("rebase state contains an invalid commit")
        item = cast(dict[str, object], value)
        sha = item.get("sha")
        subject = item.get("subject")
        patch_id = item.get("patch_id")
        signature = item.get("signature")
        signed = item.get("signed")
        signature_state = item.get("signature_state")
        if (
            not isinstance(sha, str)
            or not isinstance(subject, str)
            or (patch_id is not None and not isinstance(patch_id, str))
            or not isinstance(signature, str)
            or not isinstance(signed, bool)
            or not isinstance(signature_state, str)
        ):
            raise RebaseCheckError("rebase state contains an invalid commit")
        commits.append(
            CommitRecord(sha, subject, patch_id, signature, signed, signature_state)
        )
    return commits


def _candidate_paths(repo: Path, target: str) -> set[str]:
    commands = (
        ("diff", "--name-only", f"{target}..HEAD", "--"),
        ("diff", "--name-only", "--"),
        ("diff", "--cached", "--name-only", "--"),
        ("ls-files", "--others", "--exclude-standard"),
    )
    paths: set[str] = set()
    for command in commands:
        paths.update(path for path in git(repo, *command).splitlines() if path)
    return paths


def _conflict_markers(repo: Path, target: str) -> list[str]:
    markers: list[str] = []
    for relative in sorted(_candidate_paths(repo, target)):
        path = repo / relative
        if not path.is_file():
            continue
        try:
            lines = path.read_bytes().splitlines()
        except OSError as error:
            raise RebaseCheckError(f"cannot inspect {path}: {error}") from error
        for number, line in enumerate(lines, start=1):
            if (
                line.startswith(b"<<<<<<< ")
                or line.startswith(b">>>>>>> ")
                or line == b"======="
            ):
                markers.append(f"{relative}:{number}")
    return markers


def _match_commits(
    old: list[CommitRecord], new: list[CommitRecord]
) -> tuple[dict[int, int], list[CommitRecord], list[CommitRecord]]:
    matches: dict[int, int] = {}
    available = set(range(len(new)))
    for old_index, old_commit in enumerate(old):
        if old_commit.patch_id is None:
            continue
        match = next(
            (
                new_index
                for new_index in sorted(available)
                if new[new_index].patch_id == old_commit.patch_id
            ),
            None,
        )
        if match is not None:
            matches[old_index] = match
            available.remove(match)
    for old_index, old_commit in enumerate(old):
        if old_index in matches:
            continue
        match = next(
            (
                new_index
                for new_index in sorted(available)
                if new[new_index].subject == old_commit.subject
            ),
            None,
        )
        if match is not None:
            matches[old_index] = match
            available.remove(match)
    dropped = [commit for index, commit in enumerate(old) if index not in matches]
    added = [commit for index, commit in enumerate(new) if index in available]
    return matches, dropped, added


def verify_rebase(
    checkout: Path,
    *,
    state_file: Path | None = None,
) -> VerifyResult:
    """Verify the recorded branch after its rebase."""
    repo = Path(git(checkout, "rev-parse", "--show-toplevel")).resolve()
    source = (
        state_file.expanduser().resolve()
        if state_file is not None
        else _default_state_path(repo)
    )
    state = _read_state(source)
    target_commit = _state_string(state, "target_commit")
    old_commits = _state_commits(state)
    signing_required = _state_bool(state, "commit_gpgsign")
    problems: list[str] = []
    warnings: list[str] = []

    if _rebase_in_progress(repo):
        problems.append("a rebase is still in progress")
    unmerged = git(repo, "diff", "--name-only", "--diff-filter=U").splitlines()
    if unmerged:
        problems.append(f"unmerged paths remain: {', '.join(unmerged)}")
    markers = _conflict_markers(repo, target_commit)
    if markers:
        problems.append(f"conflict markers remain: {', '.join(markers)}")
    if not git_succeeds(repo, "merge-base", "--is-ancestor", target_commit, "HEAD"):
        problems.append(f"recorded target {target_commit} is not an ancestor of HEAD")

    new_commits = _range_commits(repo, target_commit)
    matches, dropped, added = _match_commits(old_commits, new_commits)
    if dropped:
        problems.append(
            "recorded commits were dropped: "
            + ", ".join(f"{commit.sha[:12]} {commit.subject}" for commit in dropped)
        )
    if added:
        problems.append(
            "new commits appeared: "
            + ", ".join(f"{commit.sha[:12]} {commit.subject}" for commit in added)
        )
    for old_index, new_index in matches.items():
        old_commit = old_commits[old_index]
        new_commit = new_commits[new_index]
        if old_commit.signed and not new_commit.signed:
            problems.append(
                f"signed commit {old_commit.sha[:12]} {old_commit.subject!r} maps "
                f"to unsigned commit {new_commit.sha[:12]}"
            )

    for commit in new_commits:
        if commit.signature_state == "bad":
            problems.append(
                f"rebased commit {commit.sha[:12]} {commit.subject!r} has a bad "
                f"signature ({commit.signature})"
            )
        if signing_required and not commit.signed:
            problems.append(
                f"rebased commit {commit.sha[:12]} {commit.subject!r} is unsigned "
                "although commit.gpgsign was true before the rebase"
            )
    unverified = [
        commit for commit in new_commits if commit.signature_state == "unverified"
    ]
    if unverified:
        warnings.append(
            f"{len(unverified)} rebased commit signature(s) are unverified; "
            f"{verification_hint(repo)}"
        )

    gitlink = git(repo, "rev-parse", "HEAD:openvmm")
    checked_out, openvmm_dirty, _off_pin = _openvmm_state(repo, gitlink)
    if checked_out != gitlink:
        dirty = " and openvmm has changes" if openvmm_dirty else ""
        problems.append(
            f"OpenVMM gitlink {gitlink} does not match the checked-out commit "
            f"{checked_out or '(uninitialized)'}{dirty}; nvx.py verify fails until "
            "they match, and updating a clean submodule needs the user's approval"
        )
    return VerifyResult(not problems, problems, dropped, added, warnings)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    record = commands.add_parser("record", help="record state before rebasing")
    record.add_argument("--target", default="origin/dev")
    record.add_argument("--state", type=Path)
    record.add_argument("--json", action="store_true")

    verify = commands.add_parser("verify", help="verify state after rebasing")
    verify.add_argument("--state", type=Path)
    verify.add_argument("--json", action="store_true")
    return parser


def _print_record(result: RecordResult) -> None:
    print(f"state: {result.state_file}")
    print(f"branch: {result.branch}")
    print(f"HEAD: {result.head}")
    print(f"target: {result.target} ({result.target_commit})")
    print(f"commits: {len(result.commits)}")
    print(f"commit.gpgsign: {'true' if result.commit_gpgsign else 'false'}")
    for warning in result.warnings:
        print(f"warning: {warning}")


def _print_verify(result: VerifyResult) -> None:
    stream = sys.stdout if result.ok else sys.stderr
    print(
        "rebase verification: passed"
        if result.ok
        else "error: rebase verification failed",
        file=stream,
    )
    for commit in result.dropped_commits:
        print(f"dropped: {commit.sha} {commit.subject}", file=stream)
    for commit in result.new_commits:
        print(f"new: {commit.sha} {commit.subject}", file=stream)
    for problem in result.problems:
        print(f"error: {problem}", file=stream)
    for warning in result.warnings:
        print(f"warning: {warning}", file=stream)
    print(result.note, file=stream)


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    verifying = args.command == "verify"
    try:
        if args.command == "record":
            result = record_rebase(
                REPO_ROOT,
                target=args.target,
                state_file=args.state,
            )
            if args.json:
                print(json.dumps(result.as_dict(), sort_keys=True))
            else:
                _print_record(result)
            return 0
        result = verify_rebase(REPO_ROOT, state_file=args.state)
        if args.json:
            print(json.dumps(result.as_dict(), sort_keys=True))
            if not result.ok:
                print("error: rebase verification failed", file=sys.stderr)
        else:
            _print_verify(result)
        return 0 if result.ok else 1
    except (GitError, RebaseCheckError, OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        if verifying:
            print(PUBLISH_NOTE, file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Inspect, validate, and land an OpenVMM revision promoted into NVX.

The tool records a source revision and its pull request, compares replacement
revisions, derives the facts needed to update the OpenVMM upstream roadmap,
checks the two NVX promotion commits and that roadmap, and fast-forwards
OpenVMM's development branch after review.  It derives commit identities and
positions from Git rather than accepting hand-counted values.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import NoReturn, Protocol, TextIO, cast

import openvmm_git
from openvmm_git import (
    DEV_BRANCH,
    OPENVMM_REPOSITORY,
    REPO_ROOT,
    ROADMAP,
    GitError,
    RoadmapScope,
    Signature,
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

_GH_FIELDS = (
    "number,title,state,isDraft,baseRefName,headRefName,headRefOid,"
    "reviewDecision,statusCheckRollup,url"
)
_RANGE_DIFF_LINE = re.compile(
    r"^\s*(?P<old_index>-|\d+):\s+(?P<old_sha>-+|[0-9a-f]+)\s+"
    r"(?P<kind>[=!<>])\s+(?P<new_index>-|\d+):\s+"
    r"(?P<new_sha>-+|[0-9a-f]+)(?:\s+(?P<subject>.*))?$"
)
_TABLE_ROW = re.compile(
    r"^\|\s*(?P<position>\d{3})\s*\|\s*`(?P<sha>[0-9a-fA-F]{9})`\s*\|"
)
_MAPPING_ENTRY = re.compile(r"^-\s+(?P<position>\d{3})\s+`(?P<sha>[0-9a-fA-F]{9})`\s+-")
_DAG_MARKER = re.compile(
    r"(?P<sha>[0-9a-fA-F]{9})\s+\((?P<marker>mapped tip|NVX pin)\)$"
)


class PinError(RuntimeError):
    """An expected command failure with the exit status the caller needs."""

    def __init__(self, message: str, exit_code: int = 1) -> None:
        super().__init__(message)
        self.exit_code = exit_code


class PinArgumentParser(argparse.ArgumentParser):
    """Turn argument errors into the tool's normal error form."""

    def error(self, message: str) -> NoReturn:
        raise PinError(message, 2)


class CommandRunner(Protocol):
    """Run the GitHub CLI for source pull-request inspection."""

    def run(
        self, args: Sequence[str], *, cwd: Path
    ) -> subprocess.CompletedProcess[str]: ...


class SubprocessRunner:
    """Run an external command without exposing a shell."""

    def run(
        self, args: Sequence[str], *, cwd: Path
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            list(args),
            cwd=cwd,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )


class SignatureQueries(Protocol):
    """Queries whose answers depend on the caller's signing configuration."""

    def inspect(self, repo: Path, commits: list[str]) -> list[Signature]: ...

    def verify_commit(
        self, repo: Path, commit: str
    ) -> subprocess.CompletedProcess[str]: ...

    def verification_hint(self, repo: Path) -> str: ...


class GitSignatureQueries:
    """Read commit signatures from Git."""

    def inspect(self, repo: Path, commits: list[str]) -> list[Signature]:
        return openvmm_git.signatures(repo, commits)

    def verify_commit(
        self, repo: Path, commit: str
    ) -> subprocess.CompletedProcess[str]:
        return _run_git_result(repo, "verify-commit", commit)

    def verification_hint(self, repo: Path) -> str:
        return openvmm_git.verification_hint(repo)


@dataclass(frozen=True)
class Report:
    """A command's machine-readable and human-readable forms."""

    data: dict[str, object]
    text: str
    warnings: tuple[str, ...] = ()


def _run_git_result(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )


def _result_output(result: subprocess.CompletedProcess[str]) -> str:
    return "\n".join(
        part.strip() for part in (result.stdout, result.stderr) if part.strip()
    )


def _tree(repo: Path, commit: str) -> str:
    return git(repo, "rev-parse", f"{commit}^{{tree}}")


def _scope(text: str) -> RoadmapScope:
    scope_section(text)
    return read_scope(text)


def _scope_data(scope: RoadmapScope) -> dict[str, object]:
    return {
        "fork_base": scope.fork_base,
        "mapped_tip": scope.mapped_tip,
        "mapped_tip_position": scope.mapped_tip_position,
        "pin": scope.pin,
        "pin_position": scope.pin_position,
        "upstream_main": scope.upstream_main,
    }


def _json_string(document: dict[str, object], field: str) -> str:
    value = document.get(field)
    if not isinstance(value, str):
        raise PinError(f"gh returned no string {field} field")
    return value


def _json_integer(document: dict[str, object], field: str) -> int:
    value = document.get(field)
    if isinstance(value, bool) or not isinstance(value, int):
        raise PinError(f"gh returned no integer {field} field")
    return value


def _signature_verification(
    repo: Path, commit: str, queries: SignatureQueries
) -> dict[str, object]:
    inspected = queries.inspect(repo, [commit])
    if len(inspected) != 1 or inspected[0].commit != commit:
        raise GitError(f"signature query returned no result for {commit}")
    signature = inspected[0]
    output = ""
    if signature.state in {"good", "bad"}:
        output = _result_output(queries.verify_commit(repo, commit))
    hint = queries.verification_hint(repo) if signature.state == "unverified" else ""
    return {
        "state": signature.state,
        "status": signature.state,
        "code": signature.code,
        "signed": signature.signed,
        "output": output,
        "verification_hint": hint,
    }


def _pull_request(
    runner: CommandRunner, openvmm: Path, pr: str, commit: str
) -> dict[str, object]:
    result = runner.run(
        (
            "gh",
            "pr",
            "view",
            pr,
            "--repo",
            OPENVMM_REPOSITORY,
            "--json",
            _GH_FIELDS,
        ),
        cwd=openvmm,
    )
    if result.returncode != 0:
        detail = _result_output(result) or "gh pr view failed"
        raise PinError(detail)
    try:
        raw: object = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise PinError(f"gh returned invalid JSON: {error}") from error
    if not isinstance(raw, dict):
        raise PinError("gh returned a pull request that is not a JSON object")
    document = cast(dict[str, object], raw)
    head = _json_string(document, "headRefOid")
    if head.lower() != commit.lower():
        raise PinError(f"the pull request's head is {head}, not {commit}")
    draft = document.get("isDraft")
    if not isinstance(draft, bool):
        raise PinError("gh returned no boolean isDraft field")
    review_value = document.get("reviewDecision")
    if review_value is not None and not isinstance(review_value, str):
        raise PinError("gh returned an invalid reviewDecision field")
    rollup = document.get("statusCheckRollup")
    if not isinstance(rollup, list):
        raise PinError("gh returned no statusCheckRollup array")
    check_counts: dict[str, int] = {}
    for raw_check in cast(list[object], rollup):
        if not isinstance(raw_check, dict):
            raise PinError("gh returned a non-object status check")
        check = cast(dict[str, object], raw_check)
        conclusion = check.get("conclusion")
        if isinstance(conclusion, str) and conclusion:
            key = conclusion
        else:
            status = check.get("status")
            key = status if isinstance(status, str) and status else "PENDING"
        check_counts[key] = check_counts.get(key, 0) + 1
    return {
        "number": _json_integer(document, "number"),
        "title": _json_string(document, "title"),
        "state": _json_string(document, "state"),
        "is_draft": draft,
        "base": _json_string(document, "baseRefName"),
        "head": _json_string(document, "headRefName"),
        "head_oid": head,
        "review_decision": review_value or "",
        "checks_by_conclusion": dict(sorted(check_counts.items())),
        "url": _json_string(document, "url"),
    }


def source_report(
    openvmm: Path,
    revision: str,
    pr: str | None,
    runner: CommandRunner,
    signatures: SignatureQueries,
) -> Report:
    """Describe one exact source revision and optional pull request."""
    commit = resolve_commit(openvmm, revision)
    fuller = git(openvmm, "show", "--no-patch", "--format=fuller", commit)
    record = git(
        openvmm,
        "show",
        "--no-patch",
        "--format=%H%x00%an <%ae>%x00%aI%x00%cn <%ce>%x00%cI%x00%s",
        commit,
    )
    fields = record.split("\x00")
    if len(fields) != 6:
        raise GitError(f"git show returned malformed metadata for {commit}")
    full_commit, author, author_date, committer, commit_date, subject = fields
    signature = _signature_verification(openvmm, commit, signatures)
    pull_request = _pull_request(runner, openvmm, pr, commit) if pr else None
    tree_id = _tree(openvmm, commit)
    data: dict[str, object] = {
        "command": "source",
        "revision": revision,
        "commit": full_commit,
        "author": author,
        "author_date": author_date,
        "committer": committer,
        "commit_date": commit_date,
        "subject": subject,
        "fuller": fuller,
        "signature": signature,
        "tree": tree_id,
        "pull_request": pull_request,
    }
    signature_status = cast(str, signature["state"])
    signature_output = cast(str, signature["output"])
    signature_hint = cast(str, signature["verification_hint"])
    lines = [
        f"commit: {full_commit}",
        f"author: {author}",
        f"author date: {author_date}",
        f"committer: {committer}",
        f"commit date: {commit_date}",
        f"subject: {subject}",
        f"signature: {signature_status}",
    ]
    if signature_output:
        lines.extend(["signature output:", signature_output])
    if signature_hint:
        lines.append(f"verification hint: {signature_hint}")
    lines.append(f"tree: {tree_id}")
    if pull_request is not None:
        checks = cast(dict[str, int], pull_request["checks_by_conclusion"])
        rendered_checks = ", ".join(
            f"{conclusion}={number}" for conclusion, number in checks.items()
        )
        lines.extend(
            [
                f"pull request: {pull_request['url']}",
                f"base: {pull_request['base']}",
                f"state: {pull_request['state']}",
                f"draft: {str(pull_request['is_draft']).lower()}",
                f"review decision: {pull_request['review_decision'] or 'none'}",
                f"checks: {rendered_checks or 'none'}",
            ]
        )
    return Report(data, "\n".join(lines))


def _parse_range_diff(text: str) -> tuple[dict[str, int], list[dict[str, str]]]:
    counts = {"identical": 0, "changed": 0, "removed": 0, "added": 0}
    labels = {"=": "identical", "!": "changed", "<": "removed", ">": "added"}
    changed_pairs: list[dict[str, str]] = []
    for line in text.splitlines():
        match = _RANGE_DIFF_LINE.match(line)
        if match is None:
            continue
        kind = match["kind"]
        counts[labels[kind]] += 1
        if kind == "!":
            changed_pairs.append(
                {
                    "old_index": match["old_index"],
                    "old_sha": match["old_sha"],
                    "new_index": match["new_index"],
                    "new_sha": match["new_sha"],
                    "subject": match["subject"] or "",
                    "line": line,
                }
            )
    return counts, changed_pairs


def equivalence_report(
    openvmm: Path,
    old_revision: str,
    new_revision: str,
    old_base: str | None,
    new_base: str | None,
) -> Report:
    """Compare exact trees, or complete patch series when bases are supplied."""
    if (old_base is None) != (new_base is None):
        raise PinError("--old-base and --new-base must be passed together")
    old_commit = resolve_commit(openvmm, old_revision)
    new_commit = resolve_commit(openvmm, new_revision)
    old_tree = _tree(openvmm, old_commit)
    new_tree = _tree(openvmm, new_commit)
    data: dict[str, object] = {
        "command": "equivalence",
        "old": old_commit,
        "new": new_commit,
        "old_tree": old_tree,
        "new_tree": new_tree,
        "status": "tree-identical" if old_tree == new_tree else "differs",
        "counts": None,
        "changed_pairs": [],
        "range_diff": None,
    }
    if old_tree == new_tree:
        return Report(data, f"tree-identical\ntree: {old_tree}")
    if old_base is None or new_base is None:
        reason = "trees differ; pass both bases to compare patches"
        data["reason"] = reason
        return Report(data, f"differs ({reason})")
    old_base_commit = resolve_commit(openvmm, old_base)
    new_base_commit = resolve_commit(openvmm, new_base)
    range_diff = git(
        openvmm,
        "range-diff",
        "--no-color",
        f"{old_base_commit}..{old_commit}",
        f"{new_base_commit}..{new_commit}",
    )
    counts, changed_pairs = _parse_range_diff(range_diff)
    patch_equivalent = not any(counts[name] for name in ("changed", "removed", "added"))
    status = "patch-equivalent" if patch_equivalent else "differs"
    data.update(
        {
            "status": status,
            "old_base": old_base_commit,
            "new_base": new_base_commit,
            "counts": counts,
            "changed_pairs": changed_pairs,
            "range_diff": range_diff,
        }
    )
    lines = [
        status,
        "range-diff counts: "
        + ", ".join(f"{name}={value}" for name, value in counts.items()),
    ]
    if changed_pairs:
        lines.append("changed pairs:")
        lines.extend(f"  {pair['line']}" for pair in changed_pairs)
    return Report(data, "\n".join(lines))


def _log_commits(repo: Path, revision_range: str) -> list[dict[str, object]]:
    output = git(
        repo,
        "log",
        "--reverse",
        "--format=%H%x00%s",
        revision_range,
    )
    commits: list[dict[str, object]] = []
    for line in output.splitlines():
        commit, separator, subject = line.partition("\x00")
        if not separator:
            raise GitError(f"git log returned malformed metadata in {revision_range}")
        commits.append({"commit": commit, "subject": subject})
    return commits


def _section_bounds(lines: list[str], heading: str) -> tuple[int, int]:
    try:
        start = lines.index(heading)
    except ValueError as error:
        raise GitError(f"the roadmap has no {heading} section") from error
    end = len(lines)
    for index in range(start + 1, len(lines)):
        if lines[index].startswith("## "):
            end = index
            break
    return start, end


def _pipe_cells(line: str) -> list[str]:
    stripped = line.strip()
    if not stripped.startswith("|") or not stripped.endswith("|"):
        return []
    return [cell.strip() for cell in stripped[1:-1].split("|")]


def _commits_table_header(text: str) -> list[str]:
    lines = text.splitlines()
    start, end = _section_bounds(lines, "## Commits after the mapped tip")
    for line in lines[start + 1 : end]:
        cells = _pipe_cells(line)
        if len(cells) >= 3 and cells[0] == "Position" and cells[1] == "Commit":
            return cells
    raise GitError(
        "the roadmap's Commits after the mapped tip section has no commit table"
    )


def _markdown_rows(
    commits: list[dict[str, object]], header: list[str], pr: int | None
) -> str:
    if len(header) < 3:
        raise GitError("the roadmap's commit table must have at least three columns")
    rows: list[str] = []
    middle_columns = len(header) - 3
    for commit in commits:
        commit_id = cast(str, commit["commit"])
        subject = cast(str, commit["subject"])
        if pr is not None:
            subject += f" (microsoft/openvmm#{pr})"
        cells = [
            f"{cast(int, commit['position']):03d}",
            f"`{commit_id[:9]}`",
            *(["-"] * middle_columns),
            subject,
        ]
        rows.append("| " + " | ".join(cells) + " |")
    return "\n".join(rows)


def _non_ancestor_error(
    openvmm: Path, previous_pin: str, revision: str, fork_base: str
) -> PinError:
    missing = git(
        openvmm,
        "log",
        "--format=%h %s",
        f"{revision}..{previous_pin}",
    )
    merge_base_result = _run_git_result(openvmm, "merge-base", previous_pin, revision)
    merge_base = (
        merge_base_result.stdout.strip() if merge_base_result.returncode == 0 else ""
    )
    branched_from_old_head = bool(
        merge_base
        and merge_base != fork_base
        and git_succeeds(openvmm, "merge-base", "--is-ancestor", fork_base, merge_base)
    )
    if branched_from_old_head:
        classification = (
            "the pull request branched from an older head of "
            "feature/microvm/dev: rebase it onto the pin and rerun OpenVMM CI "
            "before promoting"
        )
    else:
        classification = (
            "the chain itself was rebased: use the nvx-rebase-openvmm-chain skill"
        )
    listed = missing or "(no commits listed)"
    return PinError(
        f"the previous pin {previous_pin} is not an ancestor of {revision}\n"
        f"commits in the previous pin that the revision lacks:\n{listed}\n"
        f"{classification}",
        2,
    )


def _merge_tree(
    openvmm: Path,
    previous_pin: str,
    upstream_main: str,
    revision: str,
) -> dict[str, object]:
    result = _run_git_result(
        openvmm,
        "merge-tree",
        "--write-tree",
        "--name-only",
        "--merge-base",
        previous_pin,
        upstream_main,
        revision,
    )
    output = _result_output(result)
    stdout_lines = result.stdout.splitlines()
    tree_id = stdout_lines[0].strip() if stdout_lines else ""
    if result.returncode == 0:
        if not re.fullmatch(r"[0-9a-f]{40,64}", tree_id):
            raise GitError(f"git merge-tree returned no merged tree ID: {output}")
        return {
            "status": "applies cleanly",
            "tree": tree_id,
            "conflicting_files": [],
            "output": output,
        }
    if result.returncode == 1:
        remaining = stdout_lines[1:]
        while remaining and not remaining[0].strip():
            remaining.pop(0)
        conflicts: list[str] = []
        for line in remaining:
            if not line.strip():
                break
            conflicts.append(line.strip())
        return {
            "status": "conflicts",
            "tree": tree_id or None,
            "conflicting_files": conflicts,
            "output": output,
        }
    raise GitError(f"git merge-tree failed: {output or f'exit {result.returncode}'}")


def roadmap_report(
    openvmm: Path,
    roadmap: Path,
    nvx: Path,
    revision: str,
    target: str,
    upstream_main: str | None,
    pr: int | None,
) -> Report:
    """Derive the Git facts and ready-to-fill rows for a roadmap update."""
    require_complete_history(openvmm)
    text = roadmap.read_text(encoding="utf-8")
    roadmap_scope = _scope(text)
    commit = resolve_commit(openvmm, revision)
    previous_pin = nvx_pin(target, nvx)
    warnings: list[str] = []
    if previous_pin != roadmap_scope.pin:
        warnings.append(
            f"the target pin {previous_pin} differs from the roadmap Scope pin "
            f"{roadmap_scope.pin}"
        )
    if not git_succeeds(openvmm, "merge-base", "--is-ancestor", previous_pin, commit):
        raise _non_ancestor_error(
            openvmm, previous_pin, commit, roadmap_scope.fork_base
        )
    merge_count = int(
        git(
            openvmm,
            "rev-list",
            "--count",
            "--merges",
            f"{roadmap_scope.fork_base}..{commit}",
        )
    )
    if merge_count:
        raise PinError(
            f"{roadmap_scope.fork_base}..{commit} contains {merge_count} merge "
            "commit(s); the OpenVMM chain must not contain merges",
            2,
        )
    pin_position = position(openvmm, roadmap_scope.fork_base, commit)
    mapped_distance = count(openvmm, f"{roadmap_scope.mapped_tip}..{commit}")
    new_commits = _log_commits(openvmm, f"{previous_pin}..{commit}")
    for item in new_commits:
        item["position"] = position(
            openvmm, roadmap_scope.fork_base, cast(str, item["commit"])
        )
        item["short_commit"] = cast(str, item["commit"])[:9]
    changed_output = git(openvmm, "diff", "--name-only", previous_pin, commit)
    changed_files = changed_output.splitlines() if changed_output else []
    earlier: dict[str, object] = {}
    for path in changed_files:
        history = git(
            openvmm,
            "log",
            "--reverse",
            "--format=%H%x00%s",
            f"{roadmap_scope.fork_base}..{previous_pin}",
            "--",
            path,
        )
        commits: list[dict[str, object]] = []
        for line in history.splitlines():
            earlier_commit, separator, subject = line.partition("\x00")
            if not separator:
                raise GitError(f"git log returned malformed metadata for {path}")
            commits.append(
                {
                    "position": position(
                        openvmm, roadmap_scope.fork_base, earlier_commit
                    ),
                    "commit": earlier_commit,
                    "short_commit": earlier_commit[:9],
                    "subject": subject,
                }
            )
        earlier[path] = commits
    selected_upstream = upstream_main or roadmap_scope.upstream_main
    if selected_upstream is None:
        raise PinError("the roadmap Scope has no upstream main; pass --upstream-main")
    upstream_commit = resolve_commit(openvmm, selected_upstream)
    applicability = _merge_tree(openvmm, previous_pin, upstream_commit, commit)
    header = _commits_table_header(text)
    rows = _markdown_rows(new_commits, header, pr)
    fill_note = (
        "Fill every generated '-' column according to the existing table header "
        f"({', '.join(header)}) before committing the roadmap."
    )
    type_check_note = (
        "A clean merge only shows that the commits apply. Before saying they can "
        "go upstream on their own, type-check each affected package on the merged "
        "tree: git -C openvmm worktree add --detach DIR UPSTREAM; "
        "git -C DIR read-tree -u --reset TREE; cargo check -p PACKAGE; "
        "git -C openvmm worktree remove --force DIR."
    )
    data: dict[str, object] = {
        "command": "roadmap",
        "revision": commit,
        "target": target,
        "scope": _scope_data(roadmap_scope),
        "previous_pin": previous_pin,
        "warnings": warnings,
        "pin_position": pin_position,
        "distance_from_mapped_tip": mapped_distance,
        "new_commits": new_commits,
        "changed_files": changed_files,
        "earlier_commits_by_file": earlier,
        "upstream_main": upstream_commit,
        "upstream_applicability": applicability,
        "table_header": header,
        "markdown_rows": rows,
        "fill_note": fill_note,
        "type_check_note": type_check_note,
    }
    lines = [
        f"previous pin: {previous_pin}",
        f"new pin: {commit}",
        f"pin position: {pin_position}",
        f"distance from mapped tip: {mapped_distance}",
        "new commits:",
    ]
    if new_commits:
        lines.extend(
            "  {position:03d} {short_commit} {subject}".format(**item)
            for item in new_commits
        )
    else:
        lines.append("  (none)")
    lines.append("changed files and earlier chain commits:")
    for path in changed_files:
        lines.append(f"  {path}")
        path_commits = cast(list[dict[str, object]], earlier[path])
        if path_commits:
            lines.extend(
                "    {position:03d} {short_commit} {subject}".format(**item)
                for item in path_commits
            )
        else:
            lines.append("    (none)")
    if not changed_files:
        lines.append("  (none)")
    lines.append(
        f"upstream applicability: {applicability['status']}"
        + (
            f"; merged tree {applicability['tree']}"
            if applicability["status"] == "applies cleanly"
            else ""
        )
    )
    conflicting_files = cast(list[str], applicability["conflicting_files"])
    if conflicting_files:
        lines.append("conflicting files: " + ", ".join(conflicting_files))
    lines.extend(["markdown rows:", rows or "(none)", fill_note, type_check_note])
    return Report(data, "\n".join(lines), tuple(warnings))


def _roadmap_git_path(roadmap: Path, nvx: Path) -> str:
    try:
        return roadmap.resolve().relative_to(nvx.resolve()).as_posix()
    except ValueError as error:
        raise PinError(
            f"the roadmap {roadmap} is not inside the NVX repository"
        ) from error


def _parents(repo: Path, commit: str) -> list[str]:
    return git(repo, "rev-list", "--parents", "-n1", commit).split()


def _commit_paths(repo: Path, commit: str) -> list[str]:
    parents = _parents(repo, commit)
    if len(parents) == 1:
        output = git(
            repo,
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-r",
            commit,
        )
    else:
        output = git(repo, "diff", "--name-only", parents[1], commit)
    return output.splitlines() if output else []


def _is_gitlink(repo: Path, revision: str) -> bool:
    entry = git(repo, "ls-tree", revision, "--", "openvmm")
    fields = entry.split(None, 3)
    return len(fields) >= 3 and fields[0] == "160000" and fields[1] == "commit"


def _coordinated_path(path: str) -> bool:
    return (
        path.startswith("nvx-rs/")
        or path
        in {
            "scripts/nvx_tools/time_abi.py",
            "scripts/test_time_abi.py",
            "SOURCE-MANIFEST.json",
        }
        or path.startswith("guest/")
    )


def _diff_check(repo: Path, revision_range: str) -> list[str]:
    result = _run_git_result(repo, "diff", "--check", revision_range)
    if result.returncode == 0:
        return []
    output = _result_output(result)
    if not output:
        return [f"git diff --check failed with exit {result.returncode}"]
    return [f"git diff --check: {line}" for line in output.splitlines()]


def _scope_at_head(nvx: Path, roadmap_path: str) -> RoadmapScope:
    return _scope(git(nvx, "show", f"HEAD:{roadmap_path}"))


def check_branch_report(
    nvx: Path,
    roadmap: Path,
    target: str,
    topic: bool,
    signatures: SignatureQueries,
) -> Report:
    """Verify a standalone two-commit promotion or a promotion topic."""
    target_commit = resolve_commit(nvx, target)
    head = resolve_commit(nvx, "HEAD")
    roadmap_path = _roadmap_git_path(roadmap, nvx)
    commits_output = git(nvx, "rev-list", "--reverse", f"{target_commit}..{head}")
    commits = commits_output.splitlines() if commits_output else []
    failures: list[str] = []
    if not git_succeeds(nvx, "merge-base", "--is-ancestor", target_commit, head):
        failures.append(f"HEAD does not descend from target {target_commit}")
    inspected_signatures = signatures.inspect(nvx, commits)
    if [item.commit for item in inspected_signatures] != commits:
        raise GitError("signature query did not preserve the requested commit order")
    signature_statuses: dict[str, object] = {}
    verified_commits: list[str] = []
    unverified_commits: list[str] = []
    for signature in inspected_signatures:
        signature_statuses[signature.commit] = {
            "state": signature.state,
            "code": signature.code,
            "signed": signature.signed,
        }
        if signature.state == "good":
            verified_commits.append(signature.commit)
        elif signature.state == "unverified":
            unverified_commits.append(signature.commit)
        elif signature.state == "unsigned":
            failures.append(f"commit {signature.commit} is unsigned")
        else:
            article = "an" if signature.state == "expired" else "a"
            failures.append(
                f"commit {signature.commit} has {article} {signature.state} "
                f"signature (git status code {signature.code})"
            )
    signature_hint = signatures.verification_hint(nvx) if unverified_commits else ""
    coordinated_files: list[str] = []
    gitlink_commit: str | None = None
    roadmap_commits: list[str] = []
    if topic:
        gitlink_output = git(
            nvx,
            "rev-list",
            "--reverse",
            f"{target_commit}..{head}",
            "--",
            "openvmm",
        )
        gitlink_commits = gitlink_output.splitlines() if gitlink_output else []
        if not gitlink_commits:
            failures.append("no commit in the topic changes the openvmm gitlink")
        else:
            gitlink_commit = gitlink_commits[-1]
            paths = _commit_paths(nvx, gitlink_commit)
            coordinated_files = [path for path in paths if path != "openvmm"]
            if "openvmm" not in paths:
                failures.append(
                    f"the selected gitlink commit {gitlink_commit} does not change openvmm"
                )
            elif not _is_gitlink(nvx, gitlink_commit):
                failures.append(
                    f"the selected commit {gitlink_commit} does not record openvmm "
                    "as a gitlink"
                )
            outside = [
                path for path in coordinated_files if not _coordinated_path(path)
            ]
            if outside:
                failures.append(
                    f"gitlink commit {gitlink_commit} changes paths outside the "
                    f"coordinated set: {', '.join(outside)}"
                )
        roadmap_output = git(
            nvx,
            "rev-list",
            "--reverse",
            f"{target_commit}..{head}",
            "--",
            roadmap_path,
        )
        roadmap_commits = roadmap_output.splitlines() if roadmap_output else []
        if not roadmap_commits:
            failures.append(f"no commit in the topic changes {roadmap_path}")
        for roadmap_commit in roadmap_commits:
            paths = _commit_paths(nvx, roadmap_commit)
            if paths != [roadmap_path]:
                failures.append(
                    f"roadmap commit {roadmap_commit} changes "
                    f"{', '.join(paths) or 'no paths'}, not only {roadmap_path}"
                )
    else:
        if len(commits) != 2:
            failures.append(
                f"standalone promotion has {len(commits)} commits in "
                f"{target_commit}..HEAD, not exactly 2"
            )
        if len(commits) == 2:
            gitlink_commit, roadmap_commit = commits
            roadmap_commits = [roadmap_commit]
            first_parents = _parents(nvx, gitlink_commit)
            if first_parents != [gitlink_commit, target_commit]:
                failures.append(
                    f"gitlink commit {gitlink_commit} does not have only target "
                    f"{target_commit} as its parent"
                )
            second_parents = _parents(nvx, roadmap_commit)
            if second_parents != [roadmap_commit, gitlink_commit]:
                failures.append(
                    f"roadmap commit {roadmap_commit} does not have only gitlink "
                    f"commit {gitlink_commit} as its parent"
                )
            first_paths = _commit_paths(nvx, gitlink_commit)
            coordinated_files = [path for path in first_paths if path != "openvmm"]
            if "openvmm" not in first_paths:
                failures.append(
                    f"first commit {gitlink_commit} does not change openvmm"
                )
            elif not _is_gitlink(nvx, gitlink_commit):
                failures.append(
                    f"first commit {gitlink_commit} does not record openvmm as a gitlink"
                )
            outside = [
                path for path in coordinated_files if not _coordinated_path(path)
            ]
            if outside:
                failures.append(
                    f"first commit {gitlink_commit} changes paths outside the "
                    f"coordinated set: {', '.join(outside)}"
                )
            second_paths = _commit_paths(nvx, roadmap_commit)
            if second_paths != [roadmap_path]:
                failures.append(
                    f"second commit {roadmap_commit} changes "
                    f"{', '.join(second_paths) or 'no paths'}, not only {roadmap_path}"
                )
    try:
        roadmap_scope = _scope_at_head(nvx, roadmap_path)
        head_pin = nvx_pin("HEAD", nvx)
        if roadmap_scope.pin != head_pin:
            failures.append(
                f"the roadmap Scope pin {roadmap_scope.pin} does not match "
                f"HEAD:openvmm {head_pin}"
            )
    except GitError as error:
        failures.append(str(error))
    failures.extend(_diff_check(nvx, f"{target_commit}..{head}"))
    if failures:
        detail = "branch check failed:\n" + "\n".join(
            f"- {failure}" for failure in failures
        )
        if unverified_commits:
            detail += (
                "\nunverified signed commits (not failures): "
                + ", ".join(unverified_commits)
                + f"\nverification hint: {signature_hint}"
            )
        raise PinError(detail)
    data: dict[str, object] = {
        "command": "check-branch",
        "mode": "topic" if topic else "standalone",
        "target": target_commit,
        "head": head,
        "commits": commits,
        "gitlink_commit": gitlink_commit,
        "coordinated_files": coordinated_files,
        "roadmap_commits": roadmap_commits,
        "signatures": signature_statuses,
        "verified_commits": verified_commits,
        "unverified_commits": unverified_commits,
        "verification_hint": signature_hint,
        "failures": [],
    }
    files = ", ".join(coordinated_files) if coordinated_files else "none"
    text = (
        f"branch check passed ({data['mode']})\n"
        f"target: {target_commit}\nhead: {head}\n"
        f"coordinated files: {files}\n"
        f"verified commits: {', '.join(verified_commits) or 'none'}\n"
        f"unverified signed commits: {', '.join(unverified_commits) or 'none'}"
    )
    if signature_hint:
        text += f"\nverification hint: {signature_hint}"
    return Report(data, text)


def _line_number(lines: list[str], prefix: str) -> int:
    for number, line in enumerate(lines, start=1):
        if line.startswith(prefix):
            return number
    return 1


def _expected_chain_commit(commit_chain: list[str], commit_position: int) -> str | None:
    if commit_position < 1 or commit_position > len(commit_chain):
        return None
    return commit_chain[commit_position - 1]


def _worktree_pin(nvx: Path) -> tuple[str, str]:
    head_pin = nvx_pin("HEAD", nvx)
    result = _run_git_result(nvx, "rev-parse", ":openvmm")
    if result.returncode == 0:
        index_pin = result.stdout.strip()
        if index_pin and index_pin != head_pin:
            return index_pin, "index (:openvmm), which differs from HEAD:openvmm"
    return head_pin, "HEAD:openvmm"


def check_roadmap_report(
    openvmm: Path,
    roadmap: Path,
    nvx: Path,
    revision: str,
    worktree: bool,
) -> Report:
    """Check only the roadmap sections that describe the current chain."""
    require_complete_history(openvmm)
    if worktree:
        text = roadmap.read_text(encoding="utf-8")
        pin, pin_source = _worktree_pin(nvx)
        roadmap_source = f"working tree {roadmap}"
    else:
        roadmap_path = _roadmap_git_path(roadmap, nvx)
        text = git(nvx, "show", f"{revision}:{roadmap_path}")
        pin = nvx_pin(revision, nvx)
        pin_source = f"{revision}:openvmm"
        roadmap_source = f"{revision}:{roadmap_path}"
    lines = text.splitlines()
    roadmap_scope = _scope(text)
    pin_line = _line_number(lines, "- NVX's pinned OpenVMM revision:")
    mapped_line = _line_number(lines, "- Mapped fork tip:")
    fork_line = _line_number(lines, "- OpenVMM fork base:")
    mismatches: list[dict[str, object]] = []

    def mismatch(line: int, message: str) -> None:
        mismatches.append({"line": line, "message": message})

    if pin != roadmap_scope.pin:
        mismatch(
            pin_line,
            f"Scope pin {roadmap_scope.pin} does not match {pin_source} {pin}",
        )
    if not git_succeeds(
        openvmm, "merge-base", "--is-ancestor", roadmap_scope.fork_base, pin
    ):
        mismatch(
            fork_line,
            f"pin {pin} does not descend from fork base {roadmap_scope.fork_base}",
        )
        commit_chain: list[str] = []
    else:
        commit_chain = chain(openvmm, roadmap_scope.fork_base, pin)
    if roadmap_scope.pin_position != len(commit_chain):
        mismatch(
            pin_line,
            f"Scope pin position {roadmap_scope.pin_position} does not equal "
            f"the chain length {len(commit_chain)}",
        )
    try:
        mapped_position = position(
            openvmm, roadmap_scope.fork_base, roadmap_scope.mapped_tip
        )
    except GitError as error:
        mismatch(mapped_line, str(error))
    else:
        if roadmap_scope.mapped_tip_position != mapped_position:
            mismatch(
                mapped_line,
                f"Scope mapped-tip position {roadmap_scope.mapped_tip_position} "
                f"does not equal Git position {mapped_position}",
            )
    expected_mapped = _expected_chain_commit(
        commit_chain, roadmap_scope.mapped_tip_position
    )
    if expected_mapped != roadmap_scope.mapped_tip:
        mismatch(
            mapped_line,
            f"chain position {roadmap_scope.mapped_tip_position:03d} is "
            f"{expected_mapped or 'missing'}, not mapped tip "
            f"{roadmap_scope.mapped_tip}",
        )
    checked_references = 0
    dag_marker_lines: dict[str, list[int]] = {"mapped tip": [], "NVX pin": []}
    dag_heading_line = 1
    section_headings = (
        "## Actual branch DAG",
        "## Commits after the mapped tip",
        "## Exact commit-to-PR mapping",
    )
    for heading in section_headings:
        try:
            start, end = _section_bounds(lines, heading)
        except GitError as error:
            mismatch(1, str(error))
            continue
        if heading == "## Actual branch DAG":
            dag_heading_line = start + 1
        for index in range(start + 1, end):
            line = lines[index]
            line_number = index + 1
            if heading == "## Actual branch DAG":
                stripped = line.strip()
                if not (
                    stripped.endswith("(mapped tip)") or stripped.endswith("(NVX pin)")
                ):
                    continue
                marker_match = _DAG_MARKER.search(stripped)
                if marker_match is None:
                    mismatch(
                        line_number,
                        "DAG marker has no 9-character hexadecimal commit",
                    )
                    continue
                checked_references += 1
                marker = marker_match["marker"]
                dag_marker_lines[marker].append(line_number)
                expected = (
                    roadmap_scope.mapped_tip
                    if marker == "mapped tip"
                    else roadmap_scope.pin
                )
                named = marker_match["sha"].lower()
                if not expected.startswith(named):
                    mismatch(
                        line_number,
                        f"DAG {marker} names {named}, expected {expected[:9]}",
                    )
                continue
            if "~~" in line:
                continue
            row_match = _TABLE_ROW.match(line)
            entry_match = _MAPPING_ENTRY.match(line)
            reference = row_match or entry_match
            if reference is None:
                continue
            checked_references += 1
            named_position = int(reference["position"])
            named_sha = reference["sha"].lower()
            expected = _expected_chain_commit(commit_chain, named_position)
            if expected is None:
                mismatch(
                    line_number,
                    f"position {named_position:03d} is outside the "
                    f"{len(commit_chain)}-commit chain",
                )
            elif not expected.startswith(named_sha):
                mismatch(
                    line_number,
                    f"position {named_position:03d} names {named_sha}, "
                    f"expected {expected[:9]}",
                )
    for marker, marker_lines in dag_marker_lines.items():
        if not marker_lines:
            mismatch(dag_heading_line, f"Actual branch DAG has no ({marker}) line")
        elif len(marker_lines) > 1:
            mismatch(
                marker_lines[1],
                f"Actual branch DAG has more than one ({marker}) line",
            )
    if mismatches:
        details = "\n".join(
            f"- line {item['line']}: {item['message']}" for item in mismatches
        )
        raise PinError(f"roadmap check failed:\n{details}")
    data: dict[str, object] = {
        "command": "check-roadmap",
        "roadmap_source": roadmap_source,
        "pin_source": pin_source,
        "pin": pin,
        "scope": _scope_data(roadmap_scope),
        "chain_length": len(commit_chain),
        "references_checked": checked_references,
        "mismatches": [],
    }
    text = (
        f"roadmap check passed\nroadmap: {roadmap_source}\n"
        f"pin: {pin} (from {pin_source})\n"
        f"chain positions: {len(commit_chain)}\n"
        f"references checked: {checked_references}"
    )
    return Report(data, text)


def _remote_revision(output: str, reference: str) -> str | None:
    for line in output.splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[1] == reference:
            return fields[0]
    return None


def land_report(
    openvmm: Path,
    revision: str,
    remote: str,
    push: bool,
) -> Report:
    """Fast-forward feature/microvm/dev to an exact reviewed revision."""
    require_remote(openvmm, remote, OPENVMM_REPOSITORY)
    commit = resolve_commit(openvmm, revision)
    git(openvmm, "fetch", remote, DEV_BRANCH)
    current = resolve_commit(openvmm, "FETCH_HEAD")
    ref = f"refs/heads/{DEV_BRANCH}"
    refspec = f"{commit}:{ref}"
    command = ["git", "-C", str(openvmm), "push", remote, refspec]
    command_text = shlex.join(command)
    if current == commit:
        data: dict[str, object] = {
            "command": "land",
            "status": "already landed",
            "revision": commit,
            "remote": remote,
            "branch": DEV_BRANCH,
            "pushed": False,
        }
        return Report(data, f"already landed: {DEV_BRANCH} is {commit}")
    if not git_succeeds(openvmm, "merge-base", "--is-ancestor", current, commit):
        raise PinError(
            "the branch moved past the pull request's base: rebase the pull "
            "request, rerun OpenVMM CI, and promote again",
            2,
        )
    if not push:
        data = {
            "command": "land",
            "status": "dry run",
            "revision": commit,
            "current": current,
            "remote": remote,
            "branch": DEV_BRANCH,
            "push_command": command,
            "push_command_text": command_text,
            "pushed": False,
        }
        return Report(data, f"dry run; push with:\n{command_text}")
    git(openvmm, "push", remote, refspec)
    observed_output = git(openvmm, "ls-remote", remote, ref)
    observed = _remote_revision(observed_output, ref)
    if observed != commit:
        raise PinError(
            f"the push completed, but {remote} {ref} is "
            f"{observed or 'missing'}, not {commit}"
        )
    data = {
        "command": "land",
        "status": "landed",
        "revision": commit,
        "previous": current,
        "remote": remote,
        "branch": DEV_BRANCH,
        "push_command": command,
        "push_command_text": command_text,
        "pushed": True,
        "verified_remote_revision": observed,
    }
    return Report(data, f"landed: {DEV_BRANCH} is now {commit}")


def _common_parser(*, suppress_defaults: bool) -> PinArgumentParser:
    parser = PinArgumentParser(add_help=False)
    default_openvmm: Path | str = (
        argparse.SUPPRESS if suppress_defaults else openvmm_git.OPENVMM_DIR
    )
    default_roadmap: Path | str = argparse.SUPPRESS if suppress_defaults else ROADMAP
    default_nvx: Path | str = argparse.SUPPRESS if suppress_defaults else REPO_ROOT
    default_json: bool | str = argparse.SUPPRESS if suppress_defaults else False
    parser.add_argument(
        "--openvmm",
        type=Path,
        default=default_openvmm,
        help="OpenVMM Git repository (default: openvmm)",
    )
    parser.add_argument(
        "--roadmap",
        type=Path,
        default=default_roadmap,
        help="OpenVMM roadmap path (default: doc/openvmm-upstream-roadmap.md)",
    )
    parser.add_argument(
        "--nvx",
        type=Path,
        default=default_nvx,
        help="NVX Git repository (default: repository root)",
    )
    parser.add_argument(
        "--json",
        dest="as_json",
        action="store_true",
        default=default_json,
        help="emit a JSON report",
    )
    return parser


def parse_arguments(argv: Sequence[str] | None = None) -> argparse.Namespace:
    common = _common_parser(suppress_defaults=False)
    command_common = _common_parser(suppress_defaults=True)
    parser = PinArgumentParser(
        description="Inspect, validate, and land an OpenVMM pin promotion.",
        parents=[common],
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    source = subparsers.add_parser(
        "source", parents=[command_common], help="inspect a source revision"
    )
    source.add_argument("revision")
    source.add_argument("--pr", help="OpenVMM pull-request number or URL")

    equivalence = subparsers.add_parser(
        "equivalence",
        parents=[command_common],
        help="compare source trees or patch series",
    )
    equivalence.add_argument("old")
    equivalence.add_argument("new")
    equivalence.add_argument("--old-base")
    equivalence.add_argument("--new-base")

    roadmap_parser = subparsers.add_parser(
        "roadmap",
        parents=[command_common],
        help="derive facts for a roadmap update",
    )
    roadmap_parser.add_argument("revision")
    roadmap_parser.add_argument("--target", default="origin/dev")
    roadmap_parser.add_argument("--upstream-main")
    roadmap_parser.add_argument(
        "--pr", type=int, help="OpenVMM pull-request number for generated rows"
    )

    branch = subparsers.add_parser(
        "check-branch",
        parents=[command_common],
        help="validate the NVX promotion commits",
    )
    branch.add_argument("--target", default="origin/dev")
    branch.add_argument(
        "--topic",
        action="store_true",
        help="allow other signed commits around the promotion commits",
    )

    roadmap_check = subparsers.add_parser(
        "check-roadmap",
        parents=[command_common],
        help="validate positions and commit IDs in the roadmap",
    )
    roadmap_check.add_argument("--revision", default="HEAD")
    roadmap_check.add_argument(
        "--worktree",
        action="store_true",
        help="read the roadmap from the working tree and the pin from the index",
    )

    land = subparsers.add_parser(
        "land",
        parents=[command_common],
        help=f"fast-forward {DEV_BRANCH}",
    )
    land.add_argument("revision")
    land.add_argument("--remote", required=True)
    land.add_argument("--push", action="store_true")
    return parser.parse_args(argv)


def _dispatch(
    arguments: argparse.Namespace,
    runner: CommandRunner,
    signatures: SignatureQueries,
) -> Report:
    command = cast(str, arguments.command)
    openvmm = cast(Path, arguments.openvmm)
    roadmap = cast(Path, arguments.roadmap)
    nvx = cast(Path, arguments.nvx)
    if command == "source":
        return source_report(
            openvmm,
            cast(str, arguments.revision),
            cast(str | None, arguments.pr),
            runner,
            signatures,
        )
    if command == "equivalence":
        return equivalence_report(
            openvmm,
            cast(str, arguments.old),
            cast(str, arguments.new),
            cast(str | None, arguments.old_base),
            cast(str | None, arguments.new_base),
        )
    if command == "roadmap":
        return roadmap_report(
            openvmm,
            roadmap,
            nvx,
            cast(str, arguments.revision),
            cast(str, arguments.target),
            cast(str | None, arguments.upstream_main),
            cast(int | None, arguments.pr),
        )
    if command == "check-branch":
        return check_branch_report(
            nvx,
            roadmap,
            cast(str, arguments.target),
            cast(bool, arguments.topic),
            signatures,
        )
    if command == "check-roadmap":
        return check_roadmap_report(
            openvmm,
            roadmap,
            nvx,
            cast(str, arguments.revision),
            cast(bool, arguments.worktree),
        )
    if command == "land":
        return land_report(
            openvmm,
            cast(str, arguments.revision),
            cast(str, arguments.remote),
            cast(bool, arguments.push),
        )
    raise PinError(f"unknown command {command}", 2)


def run(
    argv: Sequence[str] | None = None,
    *,
    runner: CommandRunner | None = None,
    signatures: SignatureQueries | None = None,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
) -> int:
    """Run the CLI with injectable GitHub and signature queries."""
    output = stdout or sys.stdout
    errors = stderr or sys.stderr
    try:
        arguments = parse_arguments(argv)
        report = _dispatch(
            arguments,
            runner or SubprocessRunner(),
            signatures or GitSignatureQueries(),
        )
        for warning in report.warnings:
            print(f"warning: {warning}", file=errors)
        if cast(bool, arguments.as_json):
            print(json.dumps(report.data, indent=2, sort_keys=True), file=output)
        else:
            print(report.text, file=output)
        return 0
    except PinError as error:
        print(f"error: {error}", file=errors)
        return error.exit_code
    except (GitError, OSError) as error:
        print(f"error: {error}", file=errors)
        return 1


def main(argv: Sequence[str] | None = None) -> int:
    return run(argv)


if __name__ == "__main__":
    raise SystemExit(main())

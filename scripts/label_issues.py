"""Initial issue-label suggestions from title keywords, not confirmed impact.

Only titles are considered. Matching is case-insensitive and allows multiple
labels; operating-system names do not imply a hypervisor backend.

Usage:
    python scripts/label_issues.py --repository microsoft/nvx backfill
    python scripts/label_issues.py --repository microsoft/nvx backfill --apply

Backfill previews without writing unless --apply is supplied. The opened mode
applies labels and requires an issues.opened event, and is intended to be applied
automatically when an issue is created. Neither mode removes labels.
Run backfill apply only once: rerunning can re-add manually removed labels.
Repository labels are cached per run and writes are spaced one second apart.
Explicit rate-limit rejections retry twice with one-minute waits. Other write
failures are not retried, since the write may have succeeded. The shared client
does not expose response headers, so retry timing cannot follow server headers.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.parse
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Final, TypeVar, cast

from nvx_tools.common import ScriptError
from nvx_tools.nightly_ci import (
    GITHUB_API_URL,
    GitHubApi,
    GitHubApiError,
    GitHubClient,
)

KEYWORD_CONFIG: Final = Path(__file__).with_name("issue-labels.json")
_T = TypeVar("_T")


def retry_rate_limit(operation: Callable[[], _T]) -> _T:
    """Retry explicit throttling only; leave uncertain writes and other errors alone."""
    for attempt in range(3):
        try:
            return operation()
        except GitHubApiError as error:
            limited = error.status == 429 or (
                error.status == 403 and "rate limit" in str(error).lower()
            )
            if not limited or attempt == 2:
                raise
            print("GitHub rate limit reached; retrying in 60 seconds", file=sys.stderr)
            time.sleep(60.0)
    raise AssertionError("unreachable")


def labels_for_title(title: str, rules: Mapping[str, Sequence[str]]) -> list[str]:
    """Return matching labels once, in configuration order.

    Hyphens and word characters belong to a token, so ``nvx-rs`` does not
    match ``nvx-rs-extra``. Phrase keywords allow whitespace between words.
    """
    labels: list[str] = []
    for label, keywords in rules.items():
        for keyword in keywords:
            phrase = r"\s+".join(re.escape(word) for word in keyword.split())
            if re.search(rf"(?<![\w-]){phrase}(?![\w-])", title, re.IGNORECASE):
                labels.append(label)
                break
    return labels


class LabelError(RuntimeError):
    """An invalid input or failed GitHub operation."""


def github_token() -> str:
    """Prefer workflow/environment credentials, then the local gh login."""
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        return token
    result = subprocess.run(
        ["gh", "auth", "token"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )
    if result.returncode or not result.stdout.strip():
        raise LabelError(
            "GitHub authentication required: run gh auth login or set GH_TOKEN"
        )
    return result.stdout.strip()


def object_fields(value: object) -> Mapping[str, object]:
    if not isinstance(value, dict):
        raise LabelError("Expected a JSON object")
    result: dict[str, object] = {}
    for key, item in cast(dict[object, object], value).items():
        if not isinstance(key, str):
            raise LabelError("Expected string JSON keys")
        result[key] = item
    return result


def json_items(value: object) -> list[object]:
    if not isinstance(value, list):
        raise LabelError("Expected a JSON array")
    return cast(list[object], value)


def load_keyword_labels(path: Path = KEYWORD_CONFIG) -> dict[str, tuple[str, ...]]:
    """Read label-to-keyword rules, rejecting empty or malformed entries."""
    fields = object_fields(json.loads(path.read_text(encoding="utf-8")))
    rules: dict[str, tuple[str, ...]] = {}
    for label, value in fields.items():
        if not label.strip():
            raise LabelError(f"{path}: label names must not be empty")
        keywords: list[str] = []
        for keyword in json_items(value):
            if not isinstance(keyword, str) or not keyword.strip():
                raise LabelError(
                    f"{path}: keywords for {label!r} must be nonempty strings"
                )
            keywords.append(keyword)
        if not keywords:
            raise LabelError(f"{path}: {label!r} needs at least one keyword")
        rules[label] = tuple(keywords)
    if not rules:
        raise LabelError(f"{path}: at least one label rule is required")
    return rules


@dataclass(frozen=True)
class Issue:
    number: int
    title: str
    labels: tuple[str, ...]
    state: str
    is_pull_request: bool

    @classmethod
    def parse(cls, value: object) -> Issue:
        fields = object_fields(value)
        number, title, state = (
            fields.get("number"),
            fields.get("title"),
            fields.get("state"),
        )
        if (
            not isinstance(number, int)
            or isinstance(number, bool)
            or number <= 0
            or not isinstance(title, str)
            or not isinstance(state, str)
            or state not in ("open", "closed")
        ):
            raise LabelError("Invalid issue number, title, or state")
        labels: list[str] = []
        for item in json_items(fields.get("labels")):
            name = object_fields(item).get("name")
            if not isinstance(name, str):
                raise LabelError("Invalid issue label name")
            labels.append(name)
        return cls(number, title, tuple(labels), state, "pull_request" in fields)


class GitHub:
    """Additive label operations using the nightly reporter's REST client."""

    def __init__(self, repository: str) -> None:
        if not re.fullmatch(r"[\w.-]+/[\w.-]+", repository):
            raise LabelError("Repository must have the form owner/repo")
        self.repository = repository
        self.root = f"/repos/{repository}"
        self.api: GitHubApi = GitHubClient(
            github_token(), os.environ.get("GITHUB_API_URL") or GITHUB_API_URL
        )
        self._label_names: set[str] | None = None
        self._last_write: float | None = None

    def _post(self, path: str, body: Mapping[str, object]) -> None:
        if self._last_write is not None:
            time.sleep(max(0.0, 1.0 - (time.monotonic() - self._last_write)))
        try:
            retry_rate_limit(lambda: self.api.post(path, body))
        finally:
            self._last_write = time.monotonic()

    def open_issues(self) -> list[Issue]:
        listing = retry_rate_limit(
            lambda: self.api.get_all(f"{self.root}/issues", {"state": "open"})
        )
        return [
            issue
            for item in listing
            if not (issue := Issue.parse(item)).is_pull_request
            and issue.state == "open"
        ]

    def issue(self, number: int) -> Issue:
        return Issue.parse(
            retry_rate_limit(lambda: self.api.get(f"{self.root}/issues/{number}"))
        )

    def ensure_labels(self, labels: Sequence[str]) -> None:
        if self._label_names is None:
            names: set[str] = set()
            for item in retry_rate_limit(
                lambda: self.api.get_all(f"{self.root}/labels")
            ):
                name = object_fields(item).get("name")
                if not isinstance(name, str):
                    raise LabelError("Invalid repository label name")
                names.add(name)
            self._label_names = names
        names = self._label_names
        for label in labels:
            if label not in names:
                try:
                    self._post(
                        f"{self.root}/labels",
                        {
                            "name": label,
                            "color": "c5def5"
                            if label.startswith("area:")
                            else "bfdadc",
                            "description": "Initial title-keyword triage suggestion",
                        },
                    )
                except GitHubApiError as error:
                    if error.status != 422:
                        raise
                    # Another job may have created the label after our listing.
                    name = urllib.parse.quote(label, safe="")
                    existing_label = retry_rate_limit(
                        lambda name=name: self.api.get(f"{self.root}/labels/{name}")
                    )
                    if object_fields(existing_label).get("name") != label:
                        raise
                names.add(label)

    def add_labels(self, number: int, labels: Sequence[str]) -> None:
        self._post(
            f"{self.root}/issues/{number}/labels",
            {"labels": list(labels)},
        )


def label_issue(
    github: GitHub, issue: Issue, rules: Mapping[str, Sequence[str]], *, apply: bool
) -> None:
    if apply:
        issue = github.issue(issue.number)
    if issue.is_pull_request or issue.state != "open":
        print(f"#{issue.number}: skipped (closed issue or pull request)")
        return
    suggested = labels_for_title(issue.title, rules)
    if not suggested and not issue.labels:
        suggested = ["needs triage"]
    missing = [label for label in suggested if label not in issue.labels]
    print(
        f"#{issue.number} {issue.title}: "
        + (", ".join(missing) if missing else "no labels to add"),
        flush=True,
    )
    if apply and missing:
        github.ensure_labels(missing)
        github.add_labels(issue.number, missing)
        print(f"#{issue.number}: labels added", flush=True)


def opened_issue(path: Path, repository: str) -> Issue:
    event = object_fields(json.loads(path.read_text(encoding="utf-8")))
    if event.get("action") != "opened":
        raise LabelError("Only issues.opened events are accepted")
    if object_fields(event.get("repository")).get("full_name") != repository:
        raise LabelError("Event repository does not match --repository")
    issue = Issue.parse(event.get("issue"))
    if issue.is_pull_request:
        raise LabelError("Pull request events are not accepted")
    return issue


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    commands = parser.add_subparsers(dest="command", required=True)
    backfill = commands.add_parser("backfill", help="Preview open issues by default")
    backfill.add_argument("--apply", action="store_true", help="Actually add labels")
    opened = commands.add_parser("opened", help="Label one newly opened issue")
    opened.add_argument("--event", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        rules = load_keyword_labels()
        github = GitHub(args.repository)
        if args.command == "opened":
            label_issue(
                github, opened_issue(args.event, args.repository), rules, apply=True
            )
        else:
            print("Applying backfill" if args.apply else "Preview only; no writes")
            issues = github.open_issues()
            for issue in issues:
                label_issue(github, issue, rules, apply=args.apply)
            print(f"{len(issues)} open issues processed")
    except (LabelError, ScriptError, OSError, ValueError) as error:
        print(f"Error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

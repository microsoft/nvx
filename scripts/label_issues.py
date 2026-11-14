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
Explicit rate-limit rejections retry with one-minute waits within a three-minute
budget per operation. In-progress requests are not interrupted by that budget.
Other write failures are not retried, since the write may have succeeded. The
shared client does not expose response headers, so retry timing cannot follow
server headers.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
import urllib.parse
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from http import HTTPStatus
from pathlib import Path
from typing import Final, TypeVar

from nvx_tools.common import (
    ScriptError,
    github_token,
    require_array,
    require_object,
    require_repository,
    strict_json_object,
)
from nvx_tools.nightly_ci import (
    GITHUB_API_URL,
    GitHubApi,
    GitHubApiError,
    GitHubClient,
)

KEYWORD_CONFIG: Final = Path(__file__).with_name("issue-labels.json")
# Allow one-minute cooldowns and request time without waiting indefinitely.
RATE_LIMIT_RETRY_BUDGET_SECONDS: Final = 180.0
# Give explicit throttling rejections a minute to clear before retrying.
RATE_LIMIT_RETRY_WAIT_SECONDS: Final = 60.0
# Space writes to reduce secondary rate limiting during backfill.
WRITE_INTERVAL_SECONDS: Final = 1.0
AREA_LABEL_COLOR: Final = "c5def5"
DEFAULT_LABEL_COLOR: Final = "bfdadc"
LABEL_DESCRIPTION: Final = "Initial title-keyword triage suggestion"
_T = TypeVar("_T")


def retry_rate_limit(operation: Callable[[], _T]) -> _T:
    """Retry explicit throttling only; leave uncertain writes and other errors alone.

    Keep labeler throttling retries local for now: the shared client's other
    callers retain their existing policy of never retrying writes.
    """
    deadline = time.monotonic() + RATE_LIMIT_RETRY_BUDGET_SECONDS
    while True:
        try:
            return operation()
        except GitHubApiError as error:
            limited = error.status == HTTPStatus.TOO_MANY_REQUESTS or (
                error.status == HTTPStatus.FORBIDDEN
                and "rate limit" in str(error).lower()
            )
            if (
                not limited
                or time.monotonic() + RATE_LIMIT_RETRY_WAIT_SECONDS >= deadline
            ):
                raise
            print(
                "GitHub rate limit reached; retrying in "
                f"{RATE_LIMIT_RETRY_WAIT_SECONDS:g} seconds",
                file=sys.stderr,
            )
            time.sleep(RATE_LIMIT_RETRY_WAIT_SECONDS)
            if time.monotonic() >= deadline:
                raise


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


@dataclass(frozen=True)
class LabelConfig:
    rules: dict[str, tuple[str, ...]]
    fallback_label: str


def load_label_config(path: Path = KEYWORD_CONFIG) -> LabelConfig:
    """Read label-to-keyword rules, rejecting duplicate, empty, or malformed entries."""
    fields = require_object(
        json.loads(
            path.read_text(encoding="utf-8"), object_pairs_hook=strict_json_object
        ),
        str(path),
    )
    fallback_label = fields.get("fallback_label")
    if not isinstance(fallback_label, str) or not fallback_label.strip():
        raise ScriptError(f"{path}: fallback_label must be a nonempty string")
    keyword_labels = require_object(fields.get("rules"), f"{path}: rules")
    rules: dict[str, tuple[str, ...]] = {}
    for label, value in keyword_labels.items():
        if not label.strip():
            raise ScriptError(f"{path}: label names must not be empty")
        keywords: list[str] = []
        for keyword in require_array(value, f"{path}: keywords for {label!r}"):
            if not isinstance(keyword, str) or not keyword.strip():
                raise ScriptError(
                    f"{path}: keywords for {label!r} must be nonempty strings"
                )
            keywords.append(keyword)
        if not keywords:
            raise ScriptError(f"{path}: {label!r} needs at least one keyword")
        rules[label] = tuple(keywords)
    if not rules:
        raise ScriptError(f"{path}: at least one label rule is required")
    return LabelConfig(rules, fallback_label)


@dataclass(frozen=True)
class Issue:
    number: int
    title: str
    labels: tuple[str, ...]
    state: str
    is_pull_request: bool

    @classmethod
    def parse(cls, value: object) -> Issue:
        fields = require_object(value, "Issue")
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
            raise ScriptError("Invalid issue number, title, or state")
        labels: list[str] = []
        for item in require_array(fields.get("labels"), "Issue labels"):
            name = require_object(item, "Issue label").get("name")
            if not isinstance(name, str):
                raise ScriptError("Invalid issue label name")
            labels.append(name)
        return cls(number, title, tuple(labels), state, "pull_request" in fields)


class GitHub:
    """Additive label operations using the nightly reporter's REST client."""

    def __init__(self, repository: str) -> None:
        self.repository = require_repository(repository)
        self.root = f"/repos/{self.repository}"
        self.api: GitHubApi = GitHubClient(
            github_token(use_gh_login=True),
            os.environ.get("GITHUB_API_URL") or GITHUB_API_URL,
        )
        self._label_names: set[str] | None = None
        self._last_write: float | None = None

    def _post(self, path: str, body: Mapping[str, object]) -> None:
        if self._last_write is not None:
            time.sleep(
                max(0.0, WRITE_INTERVAL_SECONDS - (time.monotonic() - self._last_write))
            )
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
                name = require_object(item, "Repository label").get("name")
                if not isinstance(name, str):
                    raise ScriptError("Invalid repository label name")
                names.add(name.casefold())
            self._label_names = names
        names = self._label_names
        for label in labels:
            if label.casefold() not in names:
                try:
                    self._post(
                        f"{self.root}/labels",
                        {
                            "name": label,
                            "color": AREA_LABEL_COLOR
                            if label.startswith("area:")
                            else DEFAULT_LABEL_COLOR,
                            "description": LABEL_DESCRIPTION,
                        },
                    )
                except GitHubApiError as error:
                    if error.status != HTTPStatus.UNPROCESSABLE_ENTITY:
                        raise
                    # Another job may have created the label after our listing.
                    name = urllib.parse.quote(label, safe="")
                    existing_label = retry_rate_limit(
                        lambda name=name: self.api.get(f"{self.root}/labels/{name}")
                    )
                    existing_name = require_object(
                        existing_label, "Repository label"
                    ).get("name")
                    if (
                        not isinstance(existing_name, str)
                        or existing_name.casefold() != label.casefold()
                    ):
                        raise
                names.add(label.casefold())

    def add_labels(self, number: int, labels: Sequence[str]) -> None:
        self._post(
            f"{self.root}/issues/{number}/labels",
            {"labels": list(labels)},
        )


def label_issue(
    github: GitHub, issue: Issue, config: LabelConfig, *, apply: bool
) -> None:
    if apply:
        issue = github.issue(issue.number)
    if issue.is_pull_request or issue.state != "open":
        print(f"#{issue.number}: skipped (closed issue or pull request)")
        return
    suggested = labels_for_title(issue.title, config.rules)
    if not suggested and not issue.labels:
        suggested = [config.fallback_label]
    existing = {label.casefold() for label in issue.labels}
    missing = [label for label in suggested if label.casefold() not in existing]
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
    event = require_object(json.loads(path.read_text(encoding="utf-8")), str(path))
    if event.get("action") != "opened":
        raise ScriptError("Only issues.opened events are accepted")
    if (
        require_object(event.get("repository"), "Event repository").get("full_name")
        != repository
    ):
        raise ScriptError("Event repository does not match --repository")
    issue = Issue.parse(event.get("issue"))
    if issue.is_pull_request:
        raise ScriptError("Pull request events are not accepted")
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
        config = load_label_config()
        github = GitHub(args.repository)
        if args.command == "opened":
            label_issue(
                github, opened_issue(args.event, args.repository), config, apply=True
            )
        else:
            print("Applying backfill" if args.apply else "Preview only; no writes")
            issues = github.open_issues()
            for issue in issues:
                label_issue(github, issue, config, apply=args.apply)
            print(f"{len(issues)} open issues processed")
    except (ScriptError, OSError, ValueError) as error:
        print(f"Error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

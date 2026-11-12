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
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Final, cast

"""
Mapping of labels to title keywords. 
Each label is associated with a tuple of keywords that, 
if found in the title, suggest the label should be applied.
"""
KEYWORD_LABELS: Final[tuple[tuple[str, tuple[str, ...]], ...]] = (
    ("area: openvmm", ("openvmm", "microvm")),
    ("area: guest", ("guest", "nvx-init")),
    ("area: kernel", ("kernel",)),
    ("area: nvx-rs", ("nvx-rs",)),
    ("area: userland", ("userland", "alpine", "ubuntu", "azure linux", "azurelinux")),
    ("area: networking", ("network", "networking")),
    ("area: snapshot", ("snapshot", "snapshots")),
    ("area: ci", ("ci",)),
    ("area: docs", ("docs", "documentation")),
    ("area: security", ("security",)),
    ("platform: kvm", ("kvm",)),
    ("platform: mshv", ("mshv",)),
    ("platform: whp", ("whp",)),
)


def labels_for_title(title: str) -> list[str]:
    """Return matching labels once, in table order.

    Hyphens and word characters belong to a token, so ``nvx-rs`` does not
    match ``nvx-rs-extra``. Phrase keywords allow whitespace between words.
    """
    labels: list[str] = []
    for label, keywords in KEYWORD_LABELS:
        for keyword in keywords:
            phrase = r"\s+".join(re.escape(word) for word in keyword.split())
            if re.search(rf"(?<![\w-]){phrase}(?![\w-])", title, re.IGNORECASE):
                labels.append(label)
                break
    return labels


class LabelError(RuntimeError):
    """An invalid input or failed GitHub operation."""


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
    """Small additive label operations through the authenticated gh CLI."""

    def __init__(self, repository: str) -> None:
        if not re.fullmatch(r"[\w.-]+/[\w.-]+", repository):
            raise LabelError("Repository must have the form owner/repo")
        self.repository = repository
        self.root = f"repos/{repository}"

    def api(
        self,
        endpoint: str,
        *,
        method: str = "GET",
        body: Mapping[str, object] | None = None,
        options: Sequence[str] = (),
    ) -> object:
        arguments = ["gh", "api", "--method", method, *options, endpoint]
        if body is not None:
            arguments.extend(("--input", "-"))
        result = subprocess.run(
            arguments,
            input=json.dumps(body) if body is not None else None,
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
        )
        if result.returncode:
            raise LabelError(
                f"gh api {method} {endpoint} failed: {result.stderr.strip()}"
            )
        return json.loads(result.stdout)

    def open_issues(self) -> list[Issue]:
        listing = self.api(
            f"{self.root}/issues?state=open&per_page=100",
            options=(
                "--paginate",
                "--slurp",
                "--jq",
                '[.[][] | select(has("pull_request") | not) '
                "| {number, title, state, labels}]",
            ),
        )
        return [
            issue
            for item in json_items(listing)
            if not (issue := Issue.parse(item)).is_pull_request
            and issue.state == "open"
        ]

    def issue(self, number: int) -> Issue:
        return Issue.parse(self.api(f"{self.root}/issues/{number}"))

    def ensure_labels(self, labels: Sequence[str]) -> None:
        existing = self.api(
            f"{self.root}/labels?per_page=100",
            options=("--paginate", "--slurp", "--jq", "[.[][] | .name]"),
        )
        names = json_items(existing)
        if not all(isinstance(name, str) for name in names):
            raise LabelError("Invalid repository label names")
        for label in labels:
            if label not in names:
                self.api(
                    f"{self.root}/labels",
                    method="POST",
                    body={
                        "name": label,
                        "color": "c5def5" if label.startswith("area:") else "bfdadc",
                        "description": "Initial title-keyword triage suggestion",
                    },
                )
                names.append(label)

    def add_labels(self, number: int, labels: Sequence[str]) -> None:
        self.api(
            f"{self.root}/issues/{number}/labels",
            method="POST",
            body={"labels": list(labels)},
        )


def label_issue(github: GitHub, issue: Issue, *, apply: bool) -> None:
    if apply:
        issue = github.issue(issue.number)
    if issue.is_pull_request or issue.state != "open":
        print(f"#{issue.number}: skipped (closed issue or pull request)")
        return
    missing = [
        label for label in labels_for_title(issue.title) if label not in issue.labels
    ]
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
        github = GitHub(args.repository)
        if args.command == "opened":
            label_issue(github, opened_issue(args.event, args.repository), apply=True)
        else:
            print("Applying backfill" if args.apply else "Preview only; no writes")
            issues = github.open_issues()
            for issue in issues:
                label_issue(github, issue, apply=args.apply)
            print(f"{len(issues)} open issues processed")
    except (LabelError, OSError, ValueError) as error:
        print(f"Error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

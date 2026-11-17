#!/usr/bin/env python3
"""Check that a pull request references an issue of its repository.

A pull request is linked when GitHub links it to an issue of the repository,
as a closing keyword in the description of a pull request to the default
branch, or the Development sidebar, does, or when its description holds
`KEYWORD TARGET`:

- KEYWORD is close, closes, closed, fix, fixes, fixed, resolve, resolves,
  resolved, Part of, or Refs, in any case, with an optional colon;
- TARGET is #N, OWNER/REPO#N, or https://github.com/OWNER/REPO/issues/N, where
  OWNER/REPO is the repository and N one of its issues, open or closed. A pull
  request's number, a missing number, and a bare #N do not count.

The script reads the description as GitHub renders it, through GitHub's
Markdown API, and only the text that GitHub shows counts: not an HTML comment,
an attribute such as a link's title, or code, inline, fenced, indented, or in
<pre>, nor a keyword that code or the end of a block, such as a paragraph, a
list item, a heading, or a table cell, separates from its target. A link
counts by where it goes, and of its text only a keyword that starts it: a link
to an issue or a pull request, https://github.com/OWNER/REPO/issues/N or
.../pull/N, such as the link that GitHub makes of #N, as that keyword and
OWNER/REPO#N, and any other link as code does. Its scheme and host may take
any case or be left out, and the text after it does not extend N. Nothing may
follow N, in such a link or in an issue's URL that GitHub shows as text, other
than a slash, a query, or a fragment, or, in text, the punctuation that ends a
sentence. It looks up at most the first LOOKUPS numbers of the description. A
pull request by dependabot[bot] or github-actions[bot], or labelled no-issue,
is exempt, and a closed or merged one is left alone.

The script reads the pull request through the API, never from an event, and
never runs its code or prints its description, so a workflow that
pull_request_target triggers can run it, from the default branch, on any pull
request. A linked or exempt pull request loses the needs-issue label, the
marker comment, if there is one, says so, and the script exits 0. An unlinked
one gets the label and one marker comment, and the script exits 1. A GitHub
API error before the first change changes nothing, and the script exits 2.
"""

from __future__ import annotations

import argparse
import html.parser
import json
import re
import subprocess
import sys
import urllib.parse
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast

NEEDS_ISSUE = "needs-issue"
NO_ISSUE = "no-issue"
# The labels that a flagged pull request needs, with their colors and
# descriptions, created when the repository lacks them.
LABELS = {
    NEEDS_ISSUE: ("d93f0b", "The description references no issue; see Issue link"),
    NO_ISSUE: ("c2e0c6", "Needs no issue reference; applied by a maintainer"),
}
COMMENT_MARKER = "<!-- nvx-issue-link -->"
COMMENTER = "github-actions[bot]"
EXEMPT_AUTHORS = frozenset({"dependabot[bot]", "github-actions[bot]"})
GUIDE = "doc/contribute.md#commits-and-pull-requests"

# Exit codes.
PASSED = 0
FLAGGED = 1
FAILED = 2

_KEYWORD = r"(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?|part\s+of|refs)"
_NAME = r"[A-Za-z0-9_.-]+"
# GitHub's numbers fit in 32 bits, so a longer number, which int() might not
# even convert, matches nothing.
_NUMBER = r"[1-9][0-9]{0,9}"
_REFERENCE = re.compile(
    rf"(?<![\w-]){_KEYWORD}:?\s+"
    rf"(?:#(?P<local>{_NUMBER})"
    rf"|(?P<owner>{_NAME})/(?P<name>{_NAME})#(?P<qualified>{_NUMBER})"
    rf"|https://github\.com/(?P<url_owner>{_NAME})/(?P<url_name>{_NAME})"
    rf"/issues/(?P<url>{_NUMBER})"
    # As in a link, nothing but a slash, a query, or a fragment may follow the
    # number of a URL, but for the punctuation that ends a sentence.
    r"(?=/?(?:[?#][^\s\x00]*)?[.,;:!)\]]*(?![^\s\x00])))"
    r"(?![\w-])",
    re.IGNORECASE,
)
# The most numbers of a description that the check looks up, so that a
# description cannot make it call the API without bound.
LOOKUPS = 10

# A link to an issue or a pull request on GitHub, such as the link that GitHub
# makes of #N, which stands for OWNER/REPO#N whatever its text. Its scheme and
# host may take any case, as a browser reads them, or be left out, but nothing
# may follow the number other than a slash, a query, or a fragment, so that it
# goes where it names.
_ISSUE_LINK = re.compile(
    rf"(?:(?i:https?:)?//(?i:github\.com))?/(?P<owner>{_NAME})/(?P<name>{_NAME})"
    rf"/(?:issues|pull)/(?P<number>{_NUMBER})/?(?:[?#].*)?",
    re.DOTALL,
)
# A keyword that starts the text of a link to an issue, which the link keeps.
_LEADING_KEYWORD = re.compile(rf"\s*{_KEYWORD}:?(?=\s|\Z)", re.IGNORECASE)
# A NUL stands for code, for a link elsewhere, and for the start and the end of
# each block, which \s does not match, so that no reference spans them, and
# follows the reference that a link to an issue stands for, so that no text
# after the link extends its number.
_BOUNDARY = "\0"
# The elements whose text GitHub links no reference in.
_CODE = frozenset({"code", "pre"})
# The elements that GitHub renders within a line of text. Any other element,
# a block or a ruby annotation (rt, rp) alike, separates its text from the
# text around it.
_INLINE = frozenset(
    {
        "a",
        "abbr",
        "b",
        "bdi",
        "bdo",
        "br",
        "cite",
        "del",
        "dfn",
        "em",
        "g-emoji",
        "i",
        "img",
        "ins",
        "kbd",
        "mark",
        "picture",
        "q",
        "ruby",
        "s",
        "samp",
        "small",
        "source",
        "span",
        "strike",
        "strong",
        "sub",
        "sup",
        "themed-picture",
        "tt",
        "u",
        "var",
        "wbr",
    }
)


class _Text(html.parser.HTMLParser):
    """Collects the text of GitHub's HTML but for code, with a boundary at
    code and at the start and the end of each block. Attributes, such as a
    link's title, are not text, and a link stands for where it goes: a link
    to an issue or a pull request for the keyword that starts its text, if
    any, and OWNER/REPO#N, and any other link for a boundary."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.code = 0
        # Whether each open link hides its text, and how many do.
        self.links: list[bool] = []
        self.hidden = 0
        self.parts: list[str] = []
        # The reference that the open link to an issue stands for, and its text.
        self.target: str | None = None
        self.label: list[str] = []

    def _add(self, part: str) -> None:
        (self.parts if self.target is None else self.label).append(part)

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag == "a":
            href = dict(attrs).get("href")
            if href is not None and not self.code and not self.hidden:
                target = _ISSUE_LINK.fullmatch(href)
                if target:
                    self.target = (
                        f"{target['owner']}/{target['name']}#{target['number']}"
                    )
                    self.label = []
                else:
                    self.parts.append(_BOUNDARY)
            self.links.append(href is not None)
            self.hidden += href is not None
        elif tag == "br":
            self._add("\n")
        elif tag not in _INLINE:
            self._add(_BOUNDARY)
        if tag in _CODE:
            self.code += 1

    def handle_endtag(self, tag: str) -> None:
        if tag == "a" and self.links:
            self.hidden -= self.links.pop()
            if not self.hidden and self.target is not None:
                keyword = _LEADING_KEYWORD.match("".join(self.label))
                if keyword:
                    self.parts.append(f"{keyword[0]} ")
                self.parts += (self.target, _BOUNDARY)
                self.target = None
        if tag in _CODE:
            self.code = max(0, self.code - 1)
        if tag not in _INLINE:
            self._add(_BOUNDARY)

    def handle_data(self, data: str) -> None:
        if self.code:
            return
        if self.target is not None:
            self.label.append(data)
        elif not self.hidden:
            self.parts.append(data)


def visible_text(rendered: str) -> str:
    """Returns the text of the HTML that GitHub renders for a description,
    with a boundary for code, for a link elsewhere than an issue or a pull
    request, and at the start and the end of each block, and a link to an
    issue or a pull request as the keyword that starts its text, if any, and
    OWNER/REPO#N, then a boundary."""
    text = _Text()
    text.feed(rendered)
    text.close()
    return "".join(text.parts)


def referenced_numbers(rendered: str, repository: str) -> list[int]:
    """Returns the numbers that a description references with a keyword in
    the repository, in order and once each, from the HTML that GitHub renders
    for it."""
    numbers: list[int] = []
    for match in _REFERENCE.finditer(visible_text(rendered)):
        if match["local"]:
            number = match["local"]
        elif match["qualified"]:
            if f"{match['owner']}/{match['name']}".casefold() != repository.casefold():
                continue
            number = match["qualified"]
        else:
            if (
                f"{match['url_owner']}/{match['url_name']}".casefold()
                != repository.casefold()
            ):
                continue
            number = match["url"]
        if int(number) not in numbers:
            numbers.append(int(number))
    return numbers


# GitHub.


class LinkError(RuntimeError):
    """A call to GitHub failed, or its answer was not what the API documents."""


Runner = Callable[[Sequence[str], str | None], subprocess.CompletedProcess[str]]


def _run(
    arguments: Sequence[str], stdin: str | None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        list(arguments),
        input=stdin,
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )


def _object(value: object, what: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise LinkError(f"GitHub returned no object for {what}")
    return cast(dict[str, object], value)


def _list(value: object, what: str) -> list[object]:
    if not isinstance(value, list):
        raise LinkError(f"GitHub returned no list for {what}")
    return cast(list[object], value)


def _string(value: object, what: str) -> str:
    if not isinstance(value, str):
        raise LinkError(f"GitHub returned no string for {what}")
    return value


def _number(value: object, what: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool):
        raise LinkError(f"GitHub returned no number for {what}")
    return value


def _json(text: str | None, what: str) -> object:
    try:
        return json.loads(text or "")
    except ValueError as error:
        raise LinkError(f"GitHub returned no JSON for {what}: {error}") from error


@dataclass(frozen=True)
class PullRequest:
    number: int
    open: bool
    author: str
    body: str
    labels: tuple[str, ...]
    default_branch: str

    def labelled(self, label: str) -> bool:
        # GitHub compares label names without case.
        return label.casefold() in (name.casefold() for name in self.labels)


@dataclass(frozen=True)
class Comment:
    id: int
    body: str


class GitHub:
    """The calls to GitHub that check a pull request, through the gh CLI."""

    GRAPHQL_LINKS = """
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      closingIssuesReferences(first: 100) {
        nodes { number repository { nameWithOwner } }
      }
    }
  }
}
"""

    def __init__(self, repository: str, run: Runner = _run) -> None:
        self.repository = repository
        self.run = run

    def api(
        self,
        endpoint: str,
        *,
        method: str = "GET",
        body: Mapping[str, object] | None = None,
        options: Sequence[str] = (),
        tolerate: Sequence[int] = (),
    ) -> str | None:
        """Calls the API; returns None for an HTTP status that tolerate lists,
        and raises LinkError for any other failure."""
        arguments = ["gh", "api", "--method", method, *options, endpoint]
        stdin = None
        if body is not None:
            arguments += ["--input", "-"]
            stdin = json.dumps(body)
        try:
            result = self.run(arguments, stdin)
        except OSError as error:
            raise LinkError(f"gh api {method} {endpoint} failed: {error}") from error
        if result.returncode != 0:
            if any(f"HTTP {status}" in result.stderr for status in tolerate):
                return None
            raise LinkError(
                f"gh api {method} {endpoint} failed: {result.stderr.strip()}"
            )
        return result.stdout

    def pull_request(self, number: int) -> PullRequest:
        what = f"pull request #{number}"
        fields = _object(
            _json(self.api(f"repos/{self.repository}/pulls/{number}"), what), what
        )
        base = _object(fields.get("base"), f"{what}'s base")
        repository = _object(base.get("repo"), f"{what}'s repository")
        body = fields.get("body")
        return PullRequest(
            number=_number(fields.get("number"), what),
            open=_string(fields.get("state"), f"{what}'s state") == "open",
            author=_string(
                _object(fields.get("user"), f"{what}'s author").get("login"),
                f"{what}'s author",
            ),
            body=_string(body, f"{what}'s description") if body is not None else "",
            labels=tuple(
                _string(_object(label, f"{what}'s label").get("name"), "a label")
                for label in _list(fields.get("labels"), f"{what}'s labels")
            ),
            default_branch=_string(
                repository.get("default_branch"), "the default branch"
            ),
        )

    def render(self, text: str) -> str:
        """Returns the HTML that GitHub renders for Markdown text of the
        repository, as a pull request's page shows it."""
        return (
            self.api(
                "markdown",
                method="POST",
                body={"text": text, "mode": "gfm", "context": self.repository},
            )
            or ""
        )

    def linked_issues(self, number: int) -> list[int]:
        """Returns the issues of the repository that GitHub links to the pull
        request: those that a closing keyword links, for a pull request to the
        default branch, and those linked from the Development sidebar."""
        owner, name = self.repository.split("/")
        what = f"the issues linked to #{number}"
        answer = _object(
            _json(
                self.api(
                    "graphql",
                    method="POST",
                    options=(
                        "-f",
                        f"query={self.GRAPHQL_LINKS}",
                        "-f",
                        f"owner={owner}",
                        "-f",
                        f"name={name}",
                        "-F",
                        f"number={number}",
                    ),
                ),
                what,
            ),
            what,
        )
        if answer.get("errors"):
            raise LinkError(f"GitHub could not list {what}: {answer['errors']}")
        node: object = answer
        for key in ("data", "repository", "pullRequest", "closingIssuesReferences"):
            node = _object(node, what).get(key)
        issues: list[int] = []
        for item in _list(_object(node, what).get("nodes"), what):
            issue = _object(item, what)
            source = _object(issue.get("repository"), what).get("nameWithOwner")
            if _string(source, what).casefold() == self.repository.casefold():
                issues.append(_number(issue.get("number"), what))
        return issues

    def kind(self, number: int) -> str:
        """Returns what #number of the repository is: an issue, a pull
        request, missing, or an issue that moved to another repository."""
        what = f"#{number}"
        text = self.api(f"repos/{self.repository}/issues/{number}", tolerate=(404, 410))
        if text is None:
            return "missing"
        fields = _object(_json(text, what), what)
        if "pull_request" in fields:
            return "pull request"
        source = _string(fields.get("repository_url"), f"{what}'s repository")
        if not source.casefold().endswith(f"/repos/{self.repository}".casefold()):
            return "another repository"
        return "issue"

    def label_exists(self, label: str) -> bool:
        name = urllib.parse.quote(label, safe="")
        return (
            self.api(f"repos/{self.repository}/labels/{name}", tolerate=(404,))
            is not None
        )

    def comments(self, number: int) -> list[Comment]:
        """Returns COMMENTER's comments on #number."""
        what = f"the comments of #{number}"
        listing = self.api(
            f"repos/{self.repository}/issues/{number}/comments",
            options=(
                "--paginate",
                "--jq",
                ".[] | {id, author: .user.login, body} | @json",
            ),
        )
        comments: list[Comment] = []
        for line in (listing or "").splitlines():
            if line.strip():
                fields = _object(_json(line, what), what)
                # Anyone else's comments do not matter, and a deleted account's
                # have no author.
                if fields.get("author") != COMMENTER:
                    continue
                comments.append(
                    Comment(
                        _number(fields.get("id"), what),
                        _string(fields.get("body"), what),
                    )
                )
        return comments

    def create_label(self, label: str) -> bool:
        """Creates the label, and returns False when another run created it
        first."""
        color, description = LABELS[label]
        endpoint = f"repos/{self.repository}/labels"
        body = {"name": label, "color": color, "description": description}
        if self.api(endpoint, method="POST", body=body, tolerate=(422,)) is not None:
            return True
        # GitHub answers 422 both when the label exists and when it rejects it.
        if not self.label_exists(label):
            raise LinkError(f"GitHub rejected the `{label}` label (HTTP 422)")
        return False

    def add_label(self, number: int, label: str) -> None:
        self.api(
            f"repos/{self.repository}/issues/{number}/labels",
            method="POST",
            body={"labels": [label]},
        )

    def remove_label(self, number: int, label: str) -> None:
        name = urllib.parse.quote(label, safe="")
        self.api(
            f"repos/{self.repository}/issues/{number}/labels/{name}",
            method="DELETE",
            tolerate=(404,),
        )

    def create_comment(self, number: int, body: str) -> None:
        self.api(
            f"repos/{self.repository}/issues/{number}/comments",
            method="POST",
            body={"body": body},
        )

    def update_comment(self, comment: int, body: str) -> None:
        self.api(
            f"repos/{self.repository}/issues/comments/{comment}",
            method="PATCH",
            body={"body": body},
        )


# The verdict.


@dataclass(frozen=True)
class Verdict:
    state: str  # closed, exempt, linked, or unlinked
    reason: str
    linked: tuple[int, ...] = ()
    # The numbers of the description, with what each is, in order; None for
    # one that the check did not need to look up.
    referenced: tuple[tuple[int, str | None], ...] = ()

    @property
    def code(self) -> int:
        return FLAGGED if self.state == "unlinked" else PASSED


def evaluate(github: GitHub, pull: PullRequest) -> Verdict:
    """Reads what the verdict needs and changes nothing."""
    if not pull.open:
        return Verdict("closed", "the pull request is closed")
    if pull.author in EXEMPT_AUTHORS:
        return Verdict("exempt", f"its author is `{pull.author}`")
    if pull.labelled(NO_ISSUE):
        return Verdict("exempt", f"it is labelled `{NO_ISSUE}`")
    numbers = (
        referenced_numbers(github.render(pull.body), github.repository)
        if pull.body.strip()
        else []
    )
    linked = tuple(github.linked_issues(pull.number))
    if linked:
        return Verdict(
            "linked",
            f"GitHub links it to #{linked[0]}",
            linked,
            tuple((number, None) for number in numbers),
        )
    referenced: list[tuple[int, str | None]] = []
    found: int | None = None
    for index, number in enumerate(numbers):
        if found is not None or index >= LOOKUPS:
            referenced.append((number, None))
            continue
        kind = github.kind(number)
        referenced.append((number, kind))
        if kind == "issue":
            found = number
    if found is not None:
        return Verdict(
            "linked", f"its description references #{found}", (), tuple(referenced)
        )
    reason = "its description references no issue"
    if len(numbers) > LOOKUPS:
        reason = f"none of the first {LOOKUPS} numbers of its description is an issue"
    return Verdict("unlinked", reason, (), tuple(referenced))


def comment_body(verdict: Verdict, repository: str, default_branch: str) -> str:
    guide = f"https://github.com/{repository}/blob/{default_branch}/{GUIDE}"
    if verdict.state != "unlinked":
        return "\n".join(
            [
                COMMENT_MARKER,
                f"**This pull request passes the issue link check**: {verdict.reason}.",
                "",
            ]
        )
    headline = "**This pull request references no issue.**"
    if len(verdict.referenced) > LOOKUPS:
        headline = (
            f"**None of the first {LOOKUPS} numbers that this pull request's "
            "description references is an issue**, and the check looks up no "
            "more."
        )
    return "\n".join(
        [
            COMMENT_MARKER,
            f"{headline} Reference an issue of "
            "this repository on the first line of the description, as "
            f"[Commits and pull requests]({guide}) asks:",
            "",
            "- `Fixes #N` or `Closes #N` when the pull request completes the issue;",
            "- `Part of #N` or `Refs #N` otherwise.",
            "",
            f"`#N` may also be `{repository}#N` or the issue's URL. When no issue "
            "covers the change, open one first. Editing the description runs "
            f"this check again, which then removes the `{NEEDS_ISSUE}` label.",
            "",
            f"A maintainer can label a pull request that completes no issue, such "
            f"as a release pin of the OpenVMM chain, `{NO_ISSUE}`.",
            "",
        ]
    )


def publish(github: GitHub, pull: PullRequest, verdict: Verdict) -> list[str]:
    """Labels and comments as the verdict says, and returns what changed. It
    reads everything first, so that an error changes nothing."""
    flagged = verdict.state == "unlinked"
    body = comment_body(verdict, github.repository, pull.default_branch)
    comments = [
        comment
        for comment in github.comments(pull.number)
        # Other workflows comment as COMMENTER too.
        if comment.body.startswith(COMMENT_MARKER)
    ]
    add = flagged and not pull.labelled(NEEDS_ISSUE)
    # A label that the pull request holds exists; no-issue, which a flagged
    # pull request cannot hold, has to exist for a maintainer to apply it.
    missing = [
        label
        for label in LABELS
        if flagged and not pull.labelled(label) and not github.label_exists(label)
    ]
    changes: list[str] = []
    for label in missing:
        if github.create_label(label):
            changes.append(f"created the `{label}` label")
    if add:
        github.add_label(pull.number, NEEDS_ISSUE)
        changes.append(f"added `{NEEDS_ISSUE}`")
    elif not flagged and pull.labelled(NEEDS_ISSUE):
        github.remove_label(pull.number, NEEDS_ISSUE)
        changes.append(f"removed `{NEEDS_ISSUE}`")
    if comments:
        if comments[0].body != body:
            github.update_comment(comments[0].id, body)
            changes.append("updated the comment")
    elif flagged:
        github.create_comment(pull.number, body)
        changes.append("added the comment")
    return changes


def summary(number: int, verdict: Verdict, changes: Sequence[str] | None) -> str:
    """Renders the verdict and the numbers found, never the description."""

    def numbers(found: Sequence[tuple[int, str | None]]) -> str:
        return (
            ", ".join(
                f"#{number} ({kind or 'not looked up'})" for number, kind in found
            )
            or "none"
        )

    lines = [
        f"### Issue link: {verdict.state}",
        "",
        f"Pull request #{number}: {verdict.reason}.",
        "",
    ]
    if verdict.state in ("linked", "unlinked"):
        lines += [
            f"- GitHub's links: "
            f"{', '.join(f'#{issue}' for issue in verdict.linked) or 'none'}",
            f"- The description's references: {numbers(verdict.referenced)}",
        ]
    if changes is None:
        lines.append("- Changes: none, a dry run")
    else:
        lines.append(f"- Changes: {'; '.join(changes) or 'none'}")
    return "\n".join(lines) + "\n"


def _repository(value: str) -> str:
    if not re.fullmatch(rf"{_NAME}/{_NAME}", value):
        raise argparse.ArgumentTypeError("must have the form OWNER/REPO")
    return value


def _pull_request(value: str) -> int:
    if not re.fullmatch(_NUMBER, value):
        raise argparse.ArgumentTypeError("must be a pull request's number")
    return int(value)


def main(argv: Sequence[str] | None = None, run: Runner = _run) -> int:
    parser = argparse.ArgumentParser(
        description="Check that a pull request references an issue."
    )
    parser.add_argument("number", type=_pull_request, help="the pull request")
    parser.add_argument(
        "--repository", type=_repository, required=True, help="OWNER/REPO"
    )
    parser.add_argument("--summary", type=Path, help="append the verdict here")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="report the verdict without labelling or commenting",
    )
    args = parser.parse_args(argv)
    github = GitHub(args.repository, run)
    try:
        pull = github.pull_request(args.number)
        verdict = evaluate(github, pull)
        changes = (
            None
            if args.dry_run
            else []
            if verdict.state == "closed"
            else publish(github, pull, verdict)
        )
        report = summary(args.number, verdict, changes)
    except LinkError as error:
        report = f"### Issue link: error\n\n{error}\n"
        print(f"error: {error}", file=sys.stderr)
        code = FAILED
    else:
        print(report, end="")
        code = verdict.code
    if args.summary is not None:
        try:
            with args.summary.open("a", encoding="utf-8") as output:
                output.write(report)
        except OSError as error:
            print(f"error: {error}", file=sys.stderr)
            return FAILED
    return code


if __name__ == "__main__":
    sys.exit(main())

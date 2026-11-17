# pyright: reportPrivateUsage=false
"""Tests of check_issue_link.py, which checks that a pull request references an
issue."""

from __future__ import annotations

import contextlib
import html
import io
import json
import re
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path
from typing import cast

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_issue_link as link  # noqa: E402

REPOSITORY = "microsoft/nvx"
WORKFLOWS = Path(__file__).resolve().parents[1] / ".github" / "workflows"


def numbers(rendered: str) -> list[int]:
    return link.referenced_numbers(rendered, REPOSITORY)


def paragraph(text: str) -> str:
    return f'<p dir="auto">{text}</p>'


def issue_link(number: int, text: str = "") -> str:
    """Returns GitHub's link for a reference to #number of the repository."""
    return (
        '<a class="issue-link js-issue-link" '
        f'href="https://github.com/{REPOSITORY}/issues/{number}">'
        f"{text or f'#{number}'}</a>"
    )


class TextTests(unittest.TestCase):
    """The numbers of descriptions as GitHub renders them: the HTML here is
    what gh api markdown returns in the repository's context, but for the data
    attributes of its links."""

    KEYWORDS = (
        "close",
        "closes",
        "closed",
        "fix",
        "fixes",
        "fixed",
        "resolve",
        "resolves",
        "resolved",
        "part of",
        "refs",
    )

    def test_each_keyword_counts_in_any_case_with_an_optional_colon(self) -> None:
        for keyword in self.KEYWORDS:
            for spelling in (keyword, keyword.upper(), keyword.title()):
                for colon in ("", ":"):
                    with self.subTest(keyword=spelling, colon=colon):
                        rendered = paragraph(f"{spelling}{colon} {issue_link(12)}")
                        self.assertEqual(numbers(rendered), [12])
        self.assertEqual(numbers(paragraph(f"Part  of\t{issue_link(12)}")), [12])

    def test_each_target_form_of_the_repository_counts(self) -> None:
        # GitHub shows #N, OWNER/REPO#N, and the URL of an issue of the
        # repository as #N, and a URL that it does not resolve as it is.
        for target in (
            issue_link(12),
            issue_link(12, "#12 (comment)"),
            "microsoft/nvx#12",
            "Microsoft/NVX#12",
            '<a href="https://github.com/microsoft/nvx/issues/12">'
            "https://github.com/microsoft/nvx/issues/12</a>",
        ):
            with self.subTest(target=target):
                self.assertEqual(numbers(paragraph(f"Fixes {target}.")), [12])

    def test_other_repositories_and_pull_request_urls_do_not_count(self) -> None:
        for target in (
            '<a class="issue-link js-issue-link" '
            'href="https://github.com/microsoft/openvmm/issues/12">'
            "microsoft/openvmm#12</a>",
            "https://github.com/microsoft/openvmm/issues/12",
            # GitHub shows the URL of a pull request that it resolves as #N,
            # which the check looks up.
            "https://github.com/microsoft/nvx/pull/12",
        ):
            with self.subTest(target=target):
                self.assertEqual(numbers(paragraph(f"Fixes {target}")), [])

    def test_a_bare_or_missing_number_does_not_count(self) -> None:
        for text in (
            issue_link(12),
            f"Stacked on {issue_link(12)}.",
            "Fixes #",
            "Fixes #N",
            "Fixes 12",
            "Fixes #0",
            "Fixes #12abc",
            f"Prefixes {issue_link(12)}",
        ):
            with self.subTest(text=text):
                self.assertEqual(numbers(paragraph(text)), [])

    def test_a_number_too_long_to_be_an_issue_does_not_count(self) -> None:
        # Python converts no number of more than 4300 digits.
        long = "9" * 5000
        for target in (
            f"#{long}",
            f"https://github.com/microsoft/nvx/issues/{long}",
            "#12345678901",
        ):
            with self.subTest(target=target[:20]):
                rendered = paragraph(f"Refs {target}, fixes {issue_link(12)}")
                self.assertEqual(numbers(rendered), [12])

    def test_code_does_not_count(self) -> None:
        for rendered in (
            paragraph('Fixes <code class="notranslate">#12</code>'),
            '<pre class="notranslate"><code class="notranslate">Fixes #12\n'
            "</code></pre>",
            # GitHub links no reference in raw <pre> either.
            '<pre class="notranslate">Fixes #12</pre>',
            # Backticks pair across what a table's cells would be.
            paragraph('| <code class="notranslate">x | Refs #12 </code> |'),
            # Nor does a link in code.
            paragraph(
                'Fixes <code class="notranslate">'
                f'<a href="https://github.com/{REPOSITORY}/issues/12">#12</a></code>'
            ),
        ):
            with self.subTest(rendered=rendered):
                self.assertEqual(numbers(rendered), [])

    def test_text_after_code_counts(self) -> None:
        for rendered in (
            '<pre class="notranslate"><code class="notranslate">Fixes #1\n'
            f"</code></pre>\n{paragraph(f'Refs {issue_link(12)}')}",
            paragraph(
                f'<code class="notranslate">Fixes #1</code> and refs {issue_link(12)}'
            ),
        ):
            with self.subTest(rendered=rendered):
                self.assertEqual(numbers(rendered), [12])

    def test_attributes_do_not_count(self) -> None:
        # A link's title, an element's title, and an image's text.
        rendered = paragraph(
            '<a href="https://x.y" title="Fixes #12" rel="nofollow">t</a> '
            '<span title="Fixes #13">x</span> '
            '<img src="https://x.y/a.png" alt="Fixes #14">'
        )
        self.assertEqual(numbers(rendered), [])
        # Nor do they open a comment or code.
        rendered = paragraph(
            f'<span title="&lt;!--">Refs</span> {issue_link(12)} '
            '<code class="notranslate">x</code> --&gt;'
        )
        self.assertEqual(numbers(rendered), [12])

    def test_a_link_counts_by_where_it_goes(self) -> None:
        # Whatever its text, but for a keyword that starts it: a link to an
        # issue or a pull request as OWNER/REPO#N, which the check looks up,
        # and any other link as code.
        issues = f"https://github.com/{REPOSITORY}/issues/12"
        for rendered, expected in (
            (paragraph(f'Part of <a href="{issues}">the issue</a>'), [12]),
            (
                paragraph(
                    f'Fixes <a href="{issues}"><code class="notranslate">#12</code></a>'
                ),
                [12],
            ),
            (paragraph(f'Fixes <a href="{issues}#issuecomment-1">t</a>'), [12]),
            (
                paragraph(
                    f'Refs <a href="https://github.com/{REPOSITORY}/pull/12">#12</a>'
                ),
                [12],
            ),
            (
                paragraph(
                    'Refs <a href="https://github.com/microsoft/openvmm/issues/12">#12</a>'
                ),
                [],
            ),
            (
                paragraph('Fixes <a href="https://example.com" rel="nofollow">#12</a>'),
                [],
            ),
            (
                paragraph(
                    '<a href="https://example.com" rel="nofollow">Fixes</a> '
                    f"{issue_link(12)}"
                ),
                [],
            ),
        ):
            with self.subTest(rendered=rendered):
                self.assertEqual(numbers(rendered), expected)

    def test_a_link_to_an_issue_goes_where_its_href_names(self) -> None:
        # GitHub keeps a link's href as written. The browser reads the scheme
        # and the host in any case, and resolves the rest against the page.
        for href, expected in (
            (f"HTTPS://GitHub.com/{REPOSITORY}/issues/12", [12]),
            (f"http://github.com/{REPOSITORY}/issues/12/?q=1#x", [12]),
            (f"//github.com/{REPOSITORY}/issues/12", [12]),
            (f"/{REPOSITORY}/issues/12", [12]),
            (
                f"https://github.com/{REPOSITORY}/issues/12/../../../openvmm/issues/12",
                [],
            ),
            (f"https://github.com/{REPOSITORY}/issues/12/x", []),
            (f"https://github.com.example.com/{REPOSITORY}/issues/12", []),
            # The browser takes microsoft for the host of this one, and resolves
            # the next two against the page.
            (f"///{REPOSITORY}/issues/12", []),
            (f"{REPOSITORY}/issues/12", []),
            ("../issues/12", []),
        ):
            with self.subTest(href=href):
                rendered = paragraph(f'Refs <a href="{href}">#12</a>')
                self.assertEqual(numbers(rendered), expected)

    def test_a_link_to_an_issue_keeps_the_keyword_that_starts_its_text(
        self,
    ) -> None:
        issues = f"https://github.com/{REPOSITORY}/issues/12"
        for text, expected in (
            ("Part of #12", [12]),
            ("fixes: the crash", [12]),
            (" Refs", [12]),
            ("<strong>Fixes</strong> #12", [12]),
            ("See part of #12", []),
            ("Fixes#12", []),
            ('<code class="notranslate">Fixes</code> #12', []),
        ):
            with self.subTest(text=text):
                self.assertEqual(
                    numbers(paragraph(f'<a href="{issues}">{text}</a>')), expected
                )
        # The href, not the text, names the issue.
        rendered = paragraph(
            '<a href="https://github.com/microsoft/openvmm/issues/12">Part of #12</a>'
        )
        self.assertEqual(numbers(rendered), [])

    def test_text_after_a_link_to_an_issue_does_not_extend_its_number(
        self,
    ) -> None:
        # GitHub links #12 in "Refs #12-related", and shows the 3 after a link
        # to #12 apart from it.
        pulls = f"https://github.com/{REPOSITORY}/pull/12"
        for rendered in (
            paragraph(f"Refs {issue_link(12)}-related"),
            paragraph(f'Refs <a href="{pulls}">#12</a>3'),
            paragraph(f'<a href="{pulls}">Fixes #12</a>3'),
        ):
            with self.subTest(rendered=rendered):
                self.assertEqual(numbers(rendered), [12])

    def test_an_issues_url_shown_as_text_goes_where_its_path_names(self) -> None:
        # As in a link, nothing but a slash, a query, or a fragment may follow
        # the number, but for the punctuation that ends a sentence.
        url = f"https://github.com/{REPOSITORY}/issues/12"
        for suffix, expected in (
            ("", [12]),
            ("/", [12]),
            ("/?q=1#x", [12]),
            ("#x).", [12]),
            ("),", [12]),
            ("/../../../openvmm/issues/123", []),
            ("/x", []),
            ("./../../openvmm/issues/123", []),
            ("\\..\\..\\..\\openvmm\\issues\\123", []),
        ):
            for after in ("", " and more"):
                with self.subTest(suffix=suffix, after=after):
                    rendered = paragraph(f"Refs {url}{suffix}{after}")
                    self.assertEqual(numbers(rendered), expected)

    def test_no_reference_spans_code_or_blocks(self) -> None:
        for rendered in (
            paragraph(f'Fixes <code class="notranslate">#8</code> {issue_link(12)}'),
            f'<h1 dir="auto">Fixes</h1>\n{paragraph(issue_link(12))}',
            f'<ol dir="auto">\n<li>Refs</li>\n<li>{issue_link(12)}</li>\n</ol>',
            f"<blockquote>\n{paragraph('Part of')}\n</blockquote>\n"
            f"<blockquote>\n{paragraph(issue_link(12))}\n</blockquote>",
            '<markdown-accessiblity-table><table role="table">\n<tbody>\n<tr>\n'
            f"<td>Fixes</td>\n<td>{issue_link(12)}</td>\n</tr>\n</tbody>\n"
            "</table></markdown-accessiblity-table>",
            f"<details><summary>Fixes</summary>\n{paragraph(issue_link(12))}\n"
            "</details>",
        ):
            with self.subTest(rendered=rendered):
                self.assertEqual(numbers(rendered), [])

    def test_a_reference_within_a_line_of_text_counts(self) -> None:
        # Emphasis, line breaks, pictures, ruby, and a task's checkbox end no
        # line, and GitHub drops comments.
        for rendered in (
            paragraph(f"<strong>Refs</strong> {issue_link(12)}"),
            paragraph(f"Fixes<br>{issue_link(12)}"),
            paragraph(
                'Fixes <themed-picture data-catalyst-inline="true"><picture>'
                '<source srcset="https://x.y/a.png"><img src="https://x.y/a.png">'
                f"</picture></themed-picture> {issue_link(12)}"
            ),
            paragraph(f"Part of <ruby>{issue_link(12)}<rt>x</rt></ruby>"),
            '<ul class="contains-task-list">\n<li class="task-list-item">'
            '<input type="checkbox" id="" disabled="" '
            'class="task-list-item-checkbox" aria-label="Incomplete task"> '
            f"Fixes {issue_link(12)}</li>\n</ul>",
            paragraph(f"Fixes  {issue_link(12)}"),
        ):
            with self.subTest(rendered=rendered):
                self.assertEqual(numbers(rendered), [12])

    def test_numbers_come_in_order_once_each(self) -> None:
        rendered = paragraph(
            f"Part of {issue_link(3)}<br>\n"
            f"Refs {issue_link(2)}, and fixes {issue_link(3)}."
        )
        self.assertEqual(numbers(rendered), [3, 2])


Call = tuple[str, str]


class FakeGitHub:
    """A repository as gh api reports it, which the calls change."""

    def __init__(self) -> None:
        self.pulls: dict[int, dict[str, object]] = {}
        self.issues: set[int] = set()
        self.links: dict[int, list[tuple[int, str]]] = {}
        self.labels: set[str] = set()
        self.comments: list[dict[str, object]] = []
        self.failures: dict[Call, str] = {}
        # GitHub's HTML for a description; any other renders as one paragraph.
        self.rendered: dict[str, str] = {}
        self.renders: list[object] = []
        # Labels that another run creates just before this one tries to.
        self.racing: set[str] = set()
        self.calls: list[Call] = []

    def pull(
        self,
        number: int = 7,
        *,
        body: str | None = "",
        author: str = "someone",
        state: str = "open",
        labels: Sequence[str] = (),
    ) -> None:
        self.pulls[number] = {
            "number": number,
            "state": state,
            "user": {"login": author},
            "body": body,
            "labels": [{"name": label} for label in labels],
            "base": {"ref": "feature", "repo": {"default_branch": "dev"}},
        }

    def comment(self, author: str | None, body: str) -> int:
        identifier = 100 + len(self.comments)
        self.comments.append({"id": identifier, "author": author, "body": body})
        return identifier

    def writes(self) -> list[Call]:
        return [
            (method, endpoint)
            for method, endpoint in self.calls
            if method != "GET" and endpoint not in ("graphql", "markdown")
        ]

    def labelled(self, number: int = 7) -> list[str]:
        labels = cast(list[dict[str, str]], self.pulls[number]["labels"])
        return [label["name"] for label in labels]

    def __call__(
        self, arguments: Sequence[str], stdin: str | None
    ) -> subprocess.CompletedProcess[str]:
        method = arguments[arguments.index("--method") + 1]
        endpoint = next(
            argument
            for argument in arguments[2:]
            if argument.startswith("repos/") or argument in ("graphql", "markdown")
        )
        self.calls.append((method, endpoint))
        if (method, endpoint) in self.failures:
            return self.answer(arguments, 1, "", self.failures[(method, endpoint)])
        if endpoint == "markdown":
            request = cast(dict[str, str], json.loads(stdin or "{}"))
            self.renders.append(request)
            text = request["text"]
            rendered = self.rendered.get(text, paragraph(html.escape(text)))
            return self.answer(arguments, 0, rendered)
        if endpoint == "graphql":
            [field] = [
                argument for argument in arguments if argument.startswith("number=")
            ]
            nodes = [
                {"number": issue, "repository": {"nameWithOwner": source}}
                for issue, source in self.links.get(int(field.split("=")[1]), [])
            ]
            data = {"pullRequest": {"closingIssuesReferences": {"nodes": nodes}}}
            return self.answer(arguments, 0, json.dumps({"data": {"repository": data}}))
        path = endpoint.removeprefix(f"repos/{REPOSITORY}/").split("/")
        payload = cast(dict[str, object], json.loads(stdin or "{}"))
        match method, path:
            case "GET", ["pulls", number]:
                return self.answer(arguments, 0, json.dumps(self.pulls[int(number)]))
            case "GET", ["issues", number] if int(number) in self.issues:
                source = f"https://api.github.com/repos/{REPOSITORY}"
                issue = {"number": int(number), "repository_url": source}
                return self.answer(arguments, 0, json.dumps(issue))
            case "GET", ["issues", number] if int(number) in self.pulls:
                pull: dict[str, object] = {"number": int(number), "pull_request": {}}
                return self.answer(arguments, 0, json.dumps(pull))
            case "GET", ["labels", name] if name in self.labels:
                return self.answer(arguments, 0, json.dumps({"name": name}))
            case "POST", ["labels"]:
                if payload["name"] in self.labels | self.racing:
                    self.labels.add(cast(str, payload["name"]))
                    return self.answer(arguments, 1, "", "HTTP 422: already_exists")
                self.labels.add(cast(str, payload["name"]))
            case "POST", ["issues", number, "labels"]:
                for name in cast(list[str], payload["labels"]):
                    if name not in self.labelled(int(number)):
                        labels = self.pulls[int(number)]["labels"]
                        cast(list[object], labels).append({"name": name})
            case "DELETE", ["issues", number, "labels", name]:
                if name not in self.labelled(int(number)):
                    return self.answer(arguments, 1, "", "gh: Not Found (HTTP 404)")
                self.pulls[int(number)]["labels"] = [
                    {"name": label}
                    for label in self.labelled(int(number))
                    if label != name
                ]
            case "GET", ["issues", _, "comments"]:
                # What the --jq filter prints: one JSON object a line.
                listing = "".join(f"{json.dumps(item)}\n" for item in self.comments)
                return self.answer(arguments, 0, listing)
            case "POST", ["issues", _, "comments"]:
                self.comment(link.COMMENTER, cast(str, payload["body"]))
            case "PATCH", ["issues", "comments", identifier]:
                for item in self.comments:
                    if item["id"] == int(identifier):
                        item["body"] = payload["body"]
            case _:
                return self.answer(arguments, 1, "", "gh: Not Found (HTTP 404)")
        return self.answer(arguments, 0, "{}")

    @staticmethod
    def answer(
        arguments: Sequence[str], status: int, stdout: str, stderr: str = ""
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.CompletedProcess(list(arguments), status, stdout, stderr)


class CheckTests(unittest.TestCase):
    def setUp(self) -> None:
        self.github = FakeGitHub()
        self.github.issues.add(12)
        self.github.pull(8)
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.summary = Path(directory.name) / "summary.md"
        self.output = ""

    def check(self, *options: str) -> int:
        self.github.calls.clear()
        self.summary.write_text("", encoding="utf-8")
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stdout):
            code = link.main(
                [
                    "--repository",
                    REPOSITORY,
                    "--summary",
                    str(self.summary),
                    *options,
                    "7",
                ],
                run=self.github,
            )
        self.output = stdout.getvalue()
        return code

    def ours(self) -> list[str]:
        return [
            cast(str, item["body"])
            for item in self.github.comments
            if item["author"] == link.COMMENTER
        ]

    def test_githubs_link_alone_passes(self) -> None:
        self.github.pull(body=None)
        self.github.links[7] = [(12, REPOSITORY)]
        self.assertEqual(self.check(), link.PASSED)
        self.assertNotIn(("GET", f"repos/{REPOSITORY}/issues/12"), self.github.calls)
        self.assertEqual(self.github.renders, [])
        self.assertEqual(self.github.writes(), [])
        self.assertIn("GitHub links it to #12", self.summary.read_text("utf-8"))

    def test_the_description_counts_as_github_renders_it(self) -> None:
        # The Markdown references #8 too, which GitHub shows as code.
        body = "`Fixes #8` and refs #12"
        self.github.rendered[body] = paragraph(
            f'<code class="notranslate">Fixes #8</code> and refs {issue_link(12)}'
        )
        self.github.pull(body=body)
        self.assertEqual(self.check(), link.PASSED)
        self.assertEqual(
            self.github.renders, [{"text": body, "mode": "gfm", "context": REPOSITORY}]
        )
        self.assertNotIn(("GET", f"repos/{REPOSITORY}/issues/8"), self.github.calls)
        self.assertIn("references: #12 (issue)", self.summary.read_text("utf-8"))

    def test_a_link_to_another_repositorys_issue_does_not_count(self) -> None:
        self.github.pull()
        self.github.links[7] = [(12, "microsoft/openvmm")]
        self.assertEqual(self.check(), link.FLAGGED)

    def test_the_description_alone_passes_whatever_the_base(self) -> None:
        # GitHub links no issue to a pull request whose base is not the
        # default branch, nor for Part of or Refs.
        self.github.pull(body="Fixes #12")
        self.assertEqual(self.check(), link.PASSED)
        self.assertIn(("GET", f"repos/{REPOSITORY}/issues/12"), self.github.calls)
        self.assertEqual(self.github.writes(), [])

    def test_a_pull_request_or_a_missing_number_does_not_count(self) -> None:
        for body in ("Refs #8", "Fixes #99"):
            with self.subTest(body=body):
                self.github.pull(body=body)
                self.assertEqual(self.check("--dry-run"), link.FLAGGED)
        self.assertIn("#99 (missing)", self.summary.read_text("utf-8"))

    def test_an_issue_after_a_pull_request_counts(self) -> None:
        self.github.pull(body="Stacked on #8.\nRefs #8\nPart of #12")
        self.assertEqual(self.check(), link.PASSED)
        self.assertIn("#8 (pull request), #12 (issue)", self.summary.read_text("utf-8"))

    def test_a_description_makes_a_bounded_number_of_lookups(self) -> None:
        missing = range(100, 100 + link.LOOKUPS + 2)
        self.github.pull(
            body="".join(f"Refs #{number}\n" for number in missing) + "Part of #12"
        )
        self.assertEqual(self.check(), link.FLAGGED)
        lookups = [
            endpoint
            for method, endpoint in self.github.calls
            if method == "GET" and re.fullmatch(r".*/issues/[0-9]+", endpoint)
        ]
        self.assertEqual(len(lookups), link.LOOKUPS)
        report = self.summary.read_text("utf-8")
        self.assertIn(f"none of the first {link.LOOKUPS} numbers", report)
        self.assertIn("#12 (not looked up)", report)
        [comment] = self.ours()
        self.assertIn(f"None of the first {link.LOOKUPS} numbers", comment)

    def test_exempt_pull_requests_pass_without_a_label_or_a_comment(self) -> None:
        for author, labels in (
            ("dependabot[bot]", ()),
            ("github-actions[bot]", ()),
            ("someone", ("No-Issue",)),
        ):
            with self.subTest(author=author, labels=labels):
                self.github.pull(author=author, labels=labels)
                self.assertEqual(self.check(), link.PASSED)
                self.assertNotIn(("POST", "graphql"), self.github.calls)
                self.assertEqual(self.github.writes(), [])

    def test_a_closed_pull_request_is_left_alone(self) -> None:
        self.github.pull(state="closed", labels=[link.NEEDS_ISSUE])
        self.github.comment(link.COMMENTER, f"{link.COMMENT_MARKER}\nold")
        self.assertEqual(self.check(), link.PASSED)
        self.assertEqual(self.github.calls, [("GET", f"repos/{REPOSITORY}/pulls/7")])

    def test_a_flagged_pull_request_passes_once_its_description_is_fixed(
        self,
    ) -> None:
        self.github.pull(body="Stacked on #8.")
        self.assertEqual(self.check(), link.FLAGGED)
        self.assertEqual(self.github.labels, {link.NEEDS_ISSUE, link.NO_ISSUE})
        self.assertEqual(self.github.labelled(), [link.NEEDS_ISSUE])
        [flagged] = self.ours()
        self.assertTrue(flagged.startswith(link.COMMENT_MARKER))
        # Another run of the same description changes nothing.
        self.assertEqual(self.check(), link.FLAGGED)
        self.assertEqual(self.github.writes(), [])
        # An edit of the description passes, with no push.
        self.github.pull(
            body="Part of #12\n\nStacked on #8.", labels=[link.NEEDS_ISSUE]
        )
        self.assertEqual(self.check(), link.PASSED)
        self.assertEqual(self.github.labelled(), [])
        [passed] = self.ours()
        self.assertTrue(passed.startswith(link.COMMENT_MARKER))
        self.assertIn("references #12", passed)
        # So does an exemption.
        self.github.pull(labels=[link.NEEDS_ISSUE, link.NO_ISSUE])
        self.assertEqual(self.check(), link.PASSED)
        self.assertEqual(self.github.labelled(), [link.NO_ISSUE])
        self.assertIn("labelled `no-issue`", self.ours()[0])
        self.assertEqual(len(self.github.comments), 1)

    def test_a_flagged_pull_request_recreates_a_deleted_label(self) -> None:
        self.github.labels = {link.NEEDS_ISSUE}
        self.github.pull(labels=[link.NEEDS_ISSUE])
        self.github.comment(
            link.COMMENTER,
            link.comment_body(
                link.Verdict("unlinked", "its description references no issue"),
                REPOSITORY,
                "dev",
            ),
        )
        self.assertEqual(self.check(), link.FLAGGED)
        self.assertEqual(self.github.labels, {link.NEEDS_ISSUE, link.NO_ISSUE})
        self.assertEqual(self.github.writes(), [("POST", f"repos/{REPOSITORY}/labels")])

    def test_a_label_that_another_run_creates_first_is_no_error(self) -> None:
        self.github.pull()
        self.github.racing = {link.NO_ISSUE}
        self.assertEqual(self.check(), link.FLAGGED)
        self.assertEqual(self.github.labels, {link.NEEDS_ISSUE, link.NO_ISSUE})
        self.assertIn(link.NEEDS_ISSUE, self.github.labelled())
        report = self.summary.read_text("utf-8")
        self.assertIn("created the `needs-issue` label", report)
        self.assertNotIn("created the `no-issue` label", report)

    def test_a_label_that_github_rejects_fails_the_check(self) -> None:
        self.github.labels = {link.NEEDS_ISSUE}
        self.github.pull()
        self.github.failures = {
            ("POST", f"repos/{REPOSITORY}/labels"): "gh: Validation Failed (HTTP 422)"
        }
        self.assertEqual(self.check(), link.FAILED)
        self.assertEqual(self.github.writes(), [("POST", f"repos/{REPOSITORY}/labels")])
        self.assertIn("rejected the `no-issue` label", self.summary.read_text("utf-8"))

    def test_the_comment_tells_the_author_what_to_do(self) -> None:
        self.github.pull()
        self.check()
        [comment] = self.ours()
        for text in (
            "`Fixes #N`",
            "`Closes #N`",
            "`Part of #N`",
            "`Refs #N`",
            "first line of the description",
            "open one first",
            "Editing the description runs this check again",
            "`no-issue`",
            "https://github.com/microsoft/nvx/blob/dev/doc/contribute.md"
            "#commits-and-pull-requests",
        ):
            self.assertIn(text, comment)

    def test_a_comment_that_quotes_the_marker_is_never_edited(self) -> None:
        quote = f"{link.COMMENT_MARKER} quoted by someone"
        self.github.comment("someone", quote)
        self.github.comment(link.COMMENTER, "a comment of another workflow")
        self.github.pull()
        self.assertEqual(self.check(), link.FLAGGED)
        self.assertEqual(self.github.comments[0]["body"], quote)
        self.assertEqual(len(self.ours()), 2)
        self.github.pull(body="Fixes #12")
        self.assertEqual(self.check(), link.PASSED)
        self.assertEqual(self.github.comments[0]["body"], quote)
        self.assertEqual(self.ours()[0], "a comment of another workflow")
        self.assertIn("references #12", self.ours()[1])

    def test_a_comment_without_an_author_is_no_error(self) -> None:
        # GitHub reports no author for the comments of a deleted account.
        ghost = f"{link.COMMENT_MARKER} from a deleted account"
        self.github.comment(None, ghost)
        self.github.pull()
        self.assertEqual(self.check(), link.FLAGGED)
        self.assertEqual(len(self.ours()), 1)
        self.github.pull(body="Fixes #12")
        self.assertEqual(self.check(), link.PASSED)
        self.assertEqual(self.github.comments[0]["body"], ghost)
        self.assertIn("references #12", self.ours()[0])

    def test_an_api_error_changes_nothing(self) -> None:
        self.github.pull(body="Refs #12", labels=[link.NEEDS_ISSUE])
        self.github.comment(link.COMMENTER, f"{link.COMMENT_MARKER}\nold")
        reads = [
            ("GET", f"repos/{REPOSITORY}/pulls/7"),
            ("POST", "markdown"),
            ("POST", "graphql"),
            ("GET", f"repos/{REPOSITORY}/issues/12"),
            ("GET", f"repos/{REPOSITORY}/issues/7/comments"),
        ]
        for read in reads:
            with self.subTest(read=read):
                self.github.failures = {read: "gh: Bad Gateway (HTTP 502)"}
                self.assertEqual(self.check(), link.FAILED)
                self.assertIn(read, self.github.calls)
                self.assertEqual(self.github.writes(), [])
                self.assertIn("HTTP 502", self.summary.read_text("utf-8"))
        self.github.pull()
        self.github.failures = {
            ("GET", f"repos/{REPOSITORY}/labels/needs-issue"): "HTTP 403"
        }
        self.assertEqual(self.check(), link.FAILED)
        self.assertEqual(self.github.writes(), [])

    def test_the_report_never_echoes_the_description(self) -> None:
        self.github.pull(body="A SENTINEL description. Refs #8")
        self.assertEqual(self.check(), link.FLAGGED)
        report = self.summary.read_text("utf-8")
        self.assertIn("Issue link: unlinked", report)
        self.assertIn("#8 (pull request)", report)
        for text in (report, self.output, *self.ours()):
            self.assertNotIn("SENTINEL", text)

    def test_a_dry_run_changes_nothing(self) -> None:
        self.github.pull()
        self.assertEqual(self.check("--dry-run"), link.FLAGGED)
        self.assertEqual(self.github.writes(), [])
        self.assertNotIn(
            ("GET", f"repos/{REPOSITORY}/issues/7/comments"), self.github.calls
        )

    def test_gh_reads_and_writes_utf_8_whatever_the_locale(self) -> None:
        sent = link._run(
            [sys.executable, "-c", "import sys; print(sys.stdin.buffer.read().hex())"],
            "\u00e9",
        )
        self.assertEqual(sent.stdout.strip(), "c3a9")
        received = link._run(
            [
                sys.executable,
                "-c",
                "import sys; sys.stdout.buffer.write(b'\\xc3\\xa9')",
            ],
            None,
        )
        self.assertEqual(received.stdout, "\u00e9")


class WorkflowTests(unittest.TestCase):
    def setUp(self) -> None:
        self.text = (WORKFLOWS / "issue-link.yml").read_text(encoding="utf-8")
        self.workflow = cast(dict[object, object], yaml.safe_load(self.text))
        jobs = cast(dict[str, dict[str, object]], self.workflow["jobs"])
        self.assertEqual(list(jobs), ["check"])
        self.job = jobs["check"]
        self.steps = cast(list[dict[str, object]], self.job["steps"])

    def test_every_event_that_can_change_the_verdict_checks_again(self) -> None:
        self.assertEqual(self.workflow["name"], "Issue link")
        self.assertEqual(
            self.job["name"], "Check that the pull request references an issue"
        )
        # YAML 1.1 reads the `on` key as True.
        self.assertEqual(
            self.workflow[True],
            {
                "pull_request_target": {
                    "types": [
                        "opened",
                        "reopened",
                        "synchronize",
                        "edited",
                        "labeled",
                        "unlabeled",
                    ]
                }
            },
        )
        # A run whose job is skipped replaces the result of the run before it.
        self.assertNotIn("if", self.job)
        self.assertTrue(all("if" not in step for step in self.steps))

    def test_the_default_branchs_script_runs_with_a_token_that_writes_no_issue(
        self,
    ) -> None:
        self.assertEqual(self.workflow["permissions"], {})
        self.assertEqual(
            self.job["permissions"], {"contents": "read", "pull-requests": "write"}
        )
        checkout, check = self.steps
        test_changes = yaml.safe_load(
            (WORKFLOWS / "test-changes.yml").read_text(encoding="utf-8")
        )
        [pinned] = [
            step["uses"]
            for step in test_changes["jobs"]["flag"]["steps"]
            if str(step.get("uses", "")).startswith("actions/checkout@")
        ]
        self.assertEqual(checkout["uses"], pinned)
        self.assertEqual(
            checkout["with"],
            {
                "ref": "${{ github.event.repository.default_branch }}",
                "persist-credentials": False,
            },
        )
        self.assertEqual(
            str(check["run"]).split(),
            [
                "python3",
                "scripts/check_issue_link.py",
                "--repository",
                '"${GITHUB_REPOSITORY}"',
                "--summary",
                '"${GITHUB_STEP_SUMMARY}"',
                '"${NUMBER}"',
            ],
        )

    def test_only_the_pull_requests_number_reaches_the_job(self) -> None:
        check = self.steps[1]
        self.assertEqual(
            check["env"],
            {
                "GH_TOKEN": "${{ github.token }}",
                "NUMBER": "${{ github.event.pull_request.number }}",
            },
        )
        for step in self.steps:
            self.assertNotIn("${{", str(step.get("run", "")))
        self.assertEqual(
            sorted(set(self._event_fields())), ["github.event.pull_request.number"]
        )

    def test_runs_of_one_pull_request_go_one_at_a_time(self) -> None:
        self.assertNotIn("concurrency", self.workflow)
        self.assertEqual(
            self.job["concurrency"],
            {
                "group": "issue-link-${{ github.event.pull_request.number }}",
                "cancel-in-progress": False,
            },
        )
        self.assertEqual(self.job["timeout-minutes"], 5)

    def _event_fields(self) -> list[str]:
        """The fields of the pull request that the workflow reads."""
        fields: list[str] = []
        for line in self.text.splitlines():
            if not line.lstrip().startswith("#"):
                start = line.find("github.event.pull_request.")
                while start != -1:
                    end = start
                    while end < len(line) and (
                        line[end].isalnum() or line[end] in "._"
                    ):
                        end += 1
                    fields.append(line[start:end])
                    start = line.find("github.event.pull_request.", end)
        return fields


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Unit tests for the pull-request review-thread command."""

from __future__ import annotations

import contextlib
import io
import json
import subprocess
import sys
import unittest
import uuid
from collections.abc import Generator, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast

sys.path.insert(0, str(Path(__file__).parent))

import pr_threads

SCRIPT = Path(__file__).with_name("pr_threads.py")


@dataclass(frozen=True)
class Response:
    stdout: str = ""
    stderr: str = ""
    returncode: int = 0


class FakeRunner:
    def __init__(self, responses: Sequence[Response]) -> None:
        self.responses = list(responses)
        self.calls: list[tuple[str, ...]] = []

    def __call__(self, arguments: Sequence[str]) -> subprocess.CompletedProcess[str]:
        self.calls.append(tuple(arguments))
        if not self.responses:
            raise AssertionError(f"unexpected command: {arguments}")
        response = self.responses.pop(0)
        return subprocess.CompletedProcess(
            list(arguments),
            response.returncode,
            response.stdout,
            response.stderr,
        )

    def assert_finished(self) -> None:
        if self.responses:
            raise AssertionError(f"{len(self.responses)} response(s) were not used")


def encoded(value: object) -> str:
    return json.dumps(value)


def comment(number: int, body: str = "body") -> dict[str, object]:
    return {
        "databaseId": number,
        "author": {"login": "reviewer"},
        "body": body,
        "createdAt": "2026-11-01T00:00:00Z",
        "url": f"https://example.test/comments/{number}",
        "commit": {"oid": "a" * 40},
    }


def thread(
    thread_id: str,
    *,
    resolved: bool,
    comments: Sequence[dict[str, object]],
    comments_next: bool = False,
    comments_cursor: str | None = None,
) -> dict[str, object]:
    return {
        "id": thread_id,
        "isResolved": resolved,
        "isOutdated": False,
        "path": "scripts/example.py",
        "line": 12,
        "originalLine": 11,
        "comments": {
            "nodes": list(comments),
            "pageInfo": {
                "hasNextPage": comments_next,
                "endCursor": comments_cursor,
            },
        },
    }


def thread_page(
    threads: Sequence[dict[str, object]],
    *,
    has_next: bool = False,
    cursor: str | None = None,
) -> str:
    return encoded(
        {
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": list(threads),
                            "pageInfo": {
                                "hasNextPage": has_next,
                                "endCursor": cursor,
                            },
                        }
                    }
                }
            }
        }
    )


def comment_page(
    comments: Sequence[dict[str, object]],
    *,
    has_next: bool = False,
    cursor: str | None = None,
) -> str:
    return encoded(
        {
            "data": {
                "node": {
                    "comments": {
                        "nodes": list(comments),
                        "pageInfo": {
                            "hasNextPage": has_next,
                            "endCursor": cursor,
                        },
                    }
                }
            }
        }
    )


def pull_request() -> str:
    return encoded(
        {
            "headRefOid": "b" * 40,
            "state": "OPEN",
            "isDraft": False,
            "mergeable": "MERGEABLE",
            "url": "https://github.com/microsoft/nvx/pull/42",
        }
    )


def review(review_id: int, body: str) -> dict[str, object]:
    return {
        "id": review_id,
        "user": {"login": "maintainer"},
        "state": "CHANGES_REQUESTED",
        "commit_id": "c" * 40,
        "body": body,
        "submitted_at": "2026-11-01T01:00:00Z",
    }


def conversation(comment_id: int, body: str) -> dict[str, object]:
    return {
        "id": comment_id,
        "user": {"login": "maintainer"},
        "body": body,
        "created_at": "2026-11-01T02:00:00Z",
        "html_url": f"https://example.test/issues/comments/{comment_id}",
    }


class PrThreadsTests(unittest.TestCase):
    @contextlib.contextmanager
    def body_file(self, contents: str) -> Generator[Path, None, None]:
        path = Path(__file__).parent / f".pr-threads-{uuid.uuid4().hex}.txt"
        path.write_text(contents, encoding="utf-8")
        try:
            yield path
        finally:
            path.unlink(missing_ok=True)

    def run_cli(
        self, arguments: Sequence[str], runner: FakeRunner
    ) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            result = pr_threads.run_cli(arguments, runner)
        return result, stdout.getvalue(), stderr.getvalue()

    def test_help_runs_with_the_selected_python(self) -> None:
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--help"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("review threads", result.stdout)

    def test_concatenated_page_arrays_are_flattened(self) -> None:
        parsed = pr_threads.parse_concatenated_arrays(
            '[{"id": 1}]\n[{"id": 2}]', "reviews"
        )
        slurped = pr_threads.parse_concatenated_arrays(
            '[[{"id": 1}], [{"id": 2}]]', "reviews"
        )
        self.assertEqual([item["id"] for item in parsed], [1, 2])
        self.assertEqual(slurped, parsed)

    def test_json_list_paginates_threads_comments_and_rest_feedback(self) -> None:
        first_comments = [comment(number) for number in range(1, 101)]
        runner = FakeRunner(
            [
                Response(pull_request()),
                Response(
                    thread_page(
                        [
                            thread(
                                "thread-unresolved",
                                resolved=False,
                                comments=first_comments,
                                comments_next=True,
                                comments_cursor="comments-100",
                            )
                        ],
                        has_next=True,
                        cursor="threads-100",
                    )
                ),
                Response(comment_page([comment(101, "page two")])),
                Response(
                    thread_page(
                        [
                            thread(
                                "thread-resolved",
                                resolved=True,
                                comments=[comment(200)],
                            )
                        ]
                    )
                ),
                Response("gh api help with --slurp"),
                Response(encoded([[review(7, "Please update this")]])),
                Response(encoded([[conversation(8, "Question in conversation")]])),
            ]
        )
        result, stdout, stderr = self.run_cli(
            ["list", "42", "--unresolved", "--json"], runner
        )
        self.assertEqual(result, 0, stderr)
        document = cast(dict[str, object], json.loads(stdout))
        threads = cast(list[dict[str, object]], document["threads"])
        self.assertEqual([item["id"] for item in threads], ["thread-unresolved"])
        comments = cast(list[dict[str, object]], threads[0]["comments"])
        self.assertEqual(len(comments), 101)
        self.assertEqual(comments[-1]["body"], "page two")
        reviews = cast(list[dict[str, object]], document["reviews"])
        issue_comments = cast(list[dict[str, object]], document["conversationComments"])
        self.assertEqual(reviews[0]["body"], "Please update this")
        self.assertEqual(issue_comments[0]["id"], 8)
        graphql_calls = [call for call in runner.calls if "graphql" in call]
        self.assertEqual(len(graphql_calls), 3)
        self.assertIn("cursor=comments-100", graphql_calls[1])
        self.assertIn("cursor=threads-100", graphql_calls[2])
        self.assertIn("--slurp", runner.calls[-2])
        runner.assert_finished()

    def test_text_list_orders_unresolved_and_shows_all_feedback(self) -> None:
        runner = FakeRunner(
            [
                Response(pull_request()),
                Response(
                    thread_page(
                        [
                            thread("resolved", resolved=True, comments=[comment(2)]),
                            thread(
                                "unresolved",
                                resolved=False,
                                comments=[comment(1, "first line\nsecond line")],
                            ),
                        ]
                    )
                ),
                Response("gh api help without the option"),
                Response(encoded([review(7, "review body"), review(9, "")])),
                Response(encoded([conversation(8, "conversation body")])),
            ]
        )
        result, stdout, stderr = self.run_cli(["list", "42"], runner)
        self.assertEqual(result, 0, stderr)
        self.assertLess(stdout.index("- unresolved"), stdout.index("- resolved"))
        self.assertIn("reviewer #1: first line", stdout)
        self.assertIn("second line", stdout)
        self.assertIn("Reviews with bodies:", stdout)
        self.assertIn("review body", stdout)
        self.assertNotIn("#9", stdout)
        self.assertIn("Conversation comments:", stdout)
        self.assertIn("conversation body", stdout)
        self.assertNotIn("--slurp", runner.calls[-2])
        runner.assert_finished()

    def test_reply_posts_body_file_and_verifies_read_back(self) -> None:
        with self.body_file("Fixed in https://example.test/commit. Done.\r\n") as body:
            runner = FakeRunner(
                [
                    Response(encoded({"id": 91})),
                    Response(
                        encoded(
                            {
                                "id": 91,
                                "body": (
                                    "Fixed in https://example.test/commit. Done.\n\n"
                                ),
                                "html_url": "https://example.test/reply/91",
                            }
                        )
                    ),
                ]
            )
            result, stdout, stderr = self.run_cli(
                ["reply", "42", "17", "--body-file", str(body)], runner
            )
        self.assertEqual(result, 0, stderr)
        self.assertEqual(stdout.strip(), "https://example.test/reply/91")
        self.assertIn(f"body=@{body}", runner.calls[0])
        self.assertIn(
            "repos/microsoft/nvx/pulls/42/comments/17/replies", runner.calls[0]
        )
        self.assertIn("repos/microsoft/nvx/pulls/comments/91", runner.calls[1])
        runner.assert_finished()

    def test_reply_fails_when_read_back_body_differs(self) -> None:
        with self.body_file("expected") as body:
            runner = FakeRunner(
                [
                    Response(encoded({"id": 91})),
                    Response(
                        encoded(
                            {
                                "id": 91,
                                "body": "different",
                                "html_url": "https://example.test/reply/91",
                            }
                        )
                    ),
                ]
            )
            result, _, stderr = self.run_cli(
                ["reply", "42", "17", "--body-file", str(body)], runner
            )
        self.assertEqual(result, 1)
        self.assertIn("does not match", stderr)
        runner.assert_finished()

    def test_empty_body_is_refused_before_github_is_called(self) -> None:
        with self.body_file(" \r\n\t") as body:
            runner = FakeRunner([])
            result, _, stderr = self.run_cli(
                ["comment", "42", "--body-file", str(body)], runner
            )
        self.assertEqual(result, 1)
        self.assertIn("body file is empty", stderr)
        self.assertEqual(runner.calls, [])

    def test_comment_posts_body_file_and_verifies_read_back(self) -> None:
        with self.body_file("No change needed here. Evidence.\n") as body:
            runner = FakeRunner(
                [
                    Response(encoded({"id": 92})),
                    Response(
                        encoded(
                            {
                                "id": 92,
                                "body": "No change needed here. Evidence.",
                                "html_url": "https://example.test/comment/92",
                            }
                        )
                    ),
                ]
            )
            result, stdout, stderr = self.run_cli(
                ["comment", "42", "--body-file", str(body)], runner
            )
        self.assertEqual(result, 0, stderr)
        self.assertEqual(stdout.strip(), "https://example.test/comment/92")
        self.assertIn("repos/microsoft/nvx/issues/42/comments", runner.calls[0])
        self.assertIn("repos/microsoft/nvx/issues/comments/92", runner.calls[1])
        runner.assert_finished()

    def test_resolve_refuses_heads_that_do_not_contain_commit(self) -> None:
        for status in ("behind", "diverged"):
            with self.subTest(status=status):
                runner = FakeRunner(
                    [Response(pull_request()), Response(encoded({"status": status}))]
                )
                result, _, stderr = self.run_cli(
                    ["resolve", "thread", "--pr", "42", "--contains", "a" * 40],
                    runner,
                )
                self.assertEqual(result, 1)
                self.assertIn(
                    "the pull request's head does not contain SHA; push it first",
                    stderr,
                )
                self.assertFalse(
                    any("resolveReviewThread" in call for call in runner.calls)
                )
                runner.assert_finished()

    def test_resolve_accepts_ahead_and_identical_heads(self) -> None:
        for status in ("ahead", "identical"):
            with self.subTest(status=status):
                runner = FakeRunner(
                    [
                        Response(pull_request()),
                        Response(encoded({"status": status})),
                        Response(
                            encoded(
                                {
                                    "data": {
                                        "resolveReviewThread": {
                                            "thread": {"isResolved": True}
                                        }
                                    }
                                }
                            )
                        ),
                    ]
                )
                result, stdout, stderr = self.run_cli(
                    ["resolve", "thread", "--pr", "42", "--contains", "a" * 40],
                    runner,
                )
                self.assertEqual(result, 0, stderr)
                self.assertEqual(stdout.strip(), "resolved thread")
                mutation = runner.calls[-1]
                self.assertIn("id=thread", mutation)
                runner.assert_finished()

    def test_resolve_requires_mutation_to_report_resolved(self) -> None:
        runner = FakeRunner(
            [
                Response(
                    encoded(
                        {
                            "data": {
                                "resolveReviewThread": {"thread": {"isResolved": False}}
                            }
                        }
                    )
                )
            ]
        )
        result, _, stderr = self.run_cli(["resolve", "thread"], runner)
        self.assertEqual(result, 1)
        self.assertIn("did not report", stderr)
        runner.assert_finished()


if __name__ == "__main__":
    unittest.main()

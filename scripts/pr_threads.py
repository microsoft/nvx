#!/usr/bin/env python3
"""List and update GitHub pull-request review conversations safely.

The tool reads every review thread and comment, together with reviews and issue
comments that GitHub keeps outside review threads.  It also posts verified
replies and conversation comments, and resolves a review thread only after an
optional commit-containment guard.  Every GitHub operation goes through the
``gh`` CLI so callers can inject a hermetic command runner in unit tests.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast

DEFAULT_REPOSITORY = "microsoft/nvx"
_REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
_SHA = re.compile(r"[0-9A-Fa-f]{7,64}")

THREADS_QUERY = """
query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          originalLine
          comments(first: 100) {
            nodes {
              databaseId
              author { login }
              body
              createdAt
              url
              commit { oid }
            }
            pageInfo { hasNextPage endCursor }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
""".strip()

THREAD_COMMENTS_QUERY = """
query($id: ID!, $cursor: String) {
  node(id: $id) {
    ... on PullRequestReviewThread {
      comments(first: 100, after: $cursor) {
        nodes {
          databaseId
          author { login }
          body
          createdAt
          url
          commit { oid }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
""".strip()

RESOLVE_QUERY = """
mutation($id: ID!) {
  resolveReviewThread(input: {threadId: $id}) {
    thread { isResolved }
  }
}
""".strip()

JsonObject = dict[str, object]
Runner = Callable[[Sequence[str]], subprocess.CompletedProcess[str]]


class ThreadError(RuntimeError):
    """A GitHub response or requested review-thread operation is invalid."""


def _run(arguments: Sequence[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        list(arguments),
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )


def _json(text: str, context: str) -> object:
    try:
        return cast(object, json.loads(text))
    except json.JSONDecodeError as error:
        raise ThreadError(f"{context} returned invalid JSON: {error.msg}") from error


def _object(value: object, context: str) -> JsonObject:
    if not isinstance(value, dict):
        raise ThreadError(f"{context} is not a JSON object")
    return cast(JsonObject, value)


def _array(value: object, context: str) -> list[object]:
    if not isinstance(value, list):
        raise ThreadError(f"{context} is not a JSON array")
    return cast(list[object], value)


def _field_object(value: JsonObject, field: str, context: str) -> JsonObject:
    return _object(value.get(field), f"{context}.{field}")


def _field_array(value: JsonObject, field: str, context: str) -> list[object]:
    return _array(value.get(field), f"{context}.{field}")


def _string(value: object, context: str, *, empty: bool = True) -> str:
    if not isinstance(value, str) or (not empty and not value):
        raise ThreadError(f"{context} is not a string")
    return value


def _optional_string(value: object, context: str) -> str | None:
    if value is None:
        return None
    return _string(value, context)


def _integer(value: object, context: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool):
        raise ThreadError(f"{context} is not an integer")
    return value


def _optional_integer(value: object, context: str) -> int | None:
    if value is None:
        return None
    return _integer(value, context)


def _boolean(value: object, context: str) -> bool:
    if not isinstance(value, bool):
        raise ThreadError(f"{context} is not a boolean")
    return value


def _page(connection: JsonObject, context: str) -> tuple[bool, str | None]:
    page_info = _field_object(connection, "pageInfo", context)
    has_next = _boolean(page_info.get("hasNextPage"), f"{context}.hasNextPage")
    cursor = _optional_string(page_info.get("endCursor"), f"{context}.endCursor")
    if has_next and cursor is None:
        raise ThreadError(f"{context} has another page but no end cursor")
    return has_next, cursor


def _repository_parts(repository: str) -> tuple[str, str]:
    if _REPOSITORY.fullmatch(repository) is None:
        raise ThreadError("--repo must be OWNER/NAME")
    owner, name = repository.split("/", 1)
    return owner, name


def parse_concatenated_arrays(text: str, context: str) -> list[JsonObject]:
    """Parse arrays printed one after another by ``gh api --paginate``.

    ``--slurp`` wraps those page arrays in one outer array.  This accepts both
    forms so old and new gh releases produce the same result.
    """

    decoder = json.JSONDecoder()
    position = 0
    documents: list[object] = []
    while position < len(text):
        while position < len(text) and text[position].isspace():
            position += 1
        if position == len(text):
            break
        try:
            value, position = decoder.raw_decode(text, position)
        except json.JSONDecodeError as error:
            raise ThreadError(
                f"{context} returned invalid JSON: {error.msg}"
            ) from error
        documents.append(cast(object, value))

    rows: list[JsonObject] = []
    for document in documents:
        page = _array(document, context)
        nested = bool(page) and all(isinstance(item, list) for item in page)
        pages = cast(list[list[object]], page) if nested else [page]
        for items in pages:
            for item in items:
                rows.append(_object(item, f"{context} item"))
    return rows


@dataclass(frozen=True)
class ReviewComment:
    database_id: int | None
    author: str | None
    body: str
    created_at: str
    url: str
    commit_oid: str | None

    def as_json(self) -> JsonObject:
        return {
            "databaseId": self.database_id,
            "author": self.author,
            "body": self.body,
            "createdAt": self.created_at,
            "url": self.url,
            "commitOid": self.commit_oid,
        }


@dataclass(frozen=True)
class ReviewThread:
    thread_id: str
    is_resolved: bool
    is_outdated: bool
    path: str
    line: int | None
    original_line: int | None
    comments: tuple[ReviewComment, ...]

    def as_json(self) -> JsonObject:
        return {
            "id": self.thread_id,
            "isResolved": self.is_resolved,
            "isOutdated": self.is_outdated,
            "path": self.path,
            "line": self.line,
            "originalLine": self.original_line,
            "comments": [comment.as_json() for comment in self.comments],
        }


@dataclass(frozen=True)
class PullRequest:
    head_sha: str
    state: str
    is_draft: bool
    mergeable: str
    url: str

    def as_json(self) -> JsonObject:
        return {
            "headRefOid": self.head_sha,
            "state": self.state,
            "isDraft": self.is_draft,
            "mergeable": self.mergeable,
            "url": self.url,
        }


@dataclass(frozen=True)
class Review:
    review_id: int
    author: str | None
    state: str
    commit_id: str | None
    body: str
    submitted_at: str | None

    def as_json(self) -> JsonObject:
        return {
            "id": self.review_id,
            "author": self.author,
            "state": self.state,
            "commitId": self.commit_id,
            "body": self.body,
            "submittedAt": self.submitted_at,
        }


@dataclass(frozen=True)
class ConversationComment:
    comment_id: int
    author: str | None
    body: str
    created_at: str
    url: str

    def as_json(self) -> JsonObject:
        return {
            "id": self.comment_id,
            "author": self.author,
            "body": self.body,
            "createdAt": self.created_at,
            "url": self.url,
        }


@dataclass(frozen=True)
class Listing:
    pull_request: PullRequest
    threads: tuple[ReviewThread, ...]
    reviews: tuple[Review, ...]
    conversation_comments: tuple[ConversationComment, ...]

    def as_json(self) -> JsonObject:
        return {
            "pullRequest": self.pull_request.as_json(),
            "threads": [thread.as_json() for thread in self.threads],
            "reviews": [review.as_json() for review in self.reviews],
            "conversationComments": [
                comment.as_json() for comment in self.conversation_comments
            ],
        }


class GitHub:
    """Typed operations against GitHub through an injectable gh runner."""

    def __init__(self, repository: str, runner: Runner = _run) -> None:
        self.repository = repository
        self.owner, self.name = _repository_parts(repository)
        self.runner = runner
        self._slurp: bool | None = None

    def command(self, arguments: Sequence[str], context: str) -> str:
        result = self.runner(["gh", *arguments])
        if result.returncode != 0:
            detail = result.stderr.strip() or result.stdout.strip()
            raise ThreadError(
                f"{context} failed: {detail or 'gh exited unsuccessfully'}"
            )
        return result.stdout

    def graphql(
        self, query: str, variables: Mapping[str, str | int], context: str
    ) -> JsonObject:
        arguments = ["api", "graphql", "-f", f"query={query}"]
        for name, value in variables.items():
            arguments.extend(("-F", f"{name}={value}"))
        return _object(_json(self.command(arguments, context), context), context)

    def supports_slurp(self) -> bool:
        if self._slurp is None:
            result = self.runner(["gh", "api", "--help"])
            self._slurp = result.returncode == 0 and "--slurp" in result.stdout
        return self._slurp

    def paginated(self, endpoint: str, context: str) -> list[JsonObject]:
        arguments = ["api", "--paginate"]
        if self.supports_slurp():
            arguments.append("--slurp")
        arguments.append(endpoint)
        return parse_concatenated_arrays(self.command(arguments, context), context)

    def pull_request(self, number: int) -> PullRequest:
        fields = "headRefOid,state,isDraft,mergeable,url"
        output = self.command(
            [
                "pr",
                "view",
                str(number),
                "--repo",
                self.repository,
                "--json",
                fields,
            ],
            f"reading pull request {number}",
        )
        item = _object(_json(output, "pull request"), "pull request")
        return PullRequest(
            head_sha=_string(item.get("headRefOid"), "headRefOid", empty=False),
            state=_string(item.get("state"), "state", empty=False),
            is_draft=_boolean(item.get("isDraft"), "isDraft"),
            mergeable=_string(item.get("mergeable"), "mergeable", empty=False),
            url=_string(item.get("url"), "url", empty=False),
        )

    def review_comments(self, connection: JsonObject) -> list[ReviewComment]:
        comments: list[ReviewComment] = []
        for raw in _field_array(connection, "nodes", "comments"):
            item = _object(raw, "review comment")
            author_value = item.get("author")
            author = None
            if author_value is not None:
                author = _string(
                    _object(author_value, "comment author").get("login"),
                    "comment author login",
                )
            commit_value = item.get("commit")
            commit = None
            if commit_value is not None:
                commit = _string(
                    _object(commit_value, "comment commit").get("oid"),
                    "comment commit oid",
                )
            comments.append(
                ReviewComment(
                    database_id=_optional_integer(
                        item.get("databaseId"), "comment databaseId"
                    ),
                    author=author,
                    body=_string(item.get("body"), "comment body"),
                    created_at=_string(item.get("createdAt"), "comment createdAt"),
                    url=_string(item.get("url"), "comment url"),
                    commit_oid=commit,
                )
            )
        return comments

    def thread_comment_pages(self, thread_id: str, cursor: str) -> list[ReviewComment]:
        comments: list[ReviewComment] = []
        next_cursor: str | None = cursor
        while next_cursor is not None:
            document = self.graphql(
                THREAD_COMMENTS_QUERY,
                {"id": thread_id, "cursor": next_cursor},
                f"reading comments of thread {thread_id}",
            )
            data = _field_object(document, "data", "comment response")
            node = _field_object(data, "node", "comment response data")
            connection = _field_object(node, "comments", "review thread")
            comments.extend(self.review_comments(connection))
            has_next, next_cursor = _page(connection, "thread comments")
            if not has_next:
                next_cursor = None
        return comments

    def threads(self, number: int) -> list[ReviewThread]:
        threads: list[ReviewThread] = []
        cursor: str | None = None
        while True:
            variables: dict[str, str | int] = {
                "owner": self.owner,
                "name": self.name,
                "number": number,
            }
            if cursor is not None:
                variables["cursor"] = cursor
            document = self.graphql(
                THREADS_QUERY, variables, f"reading review threads of PR {number}"
            )
            data = _field_object(document, "data", "thread response")
            repository = _field_object(data, "repository", "thread response data")
            pull_request = _field_object(repository, "pullRequest", "repository")
            connection = _field_object(pull_request, "reviewThreads", "pull request")
            for raw in _field_array(connection, "nodes", "reviewThreads"):
                item = _object(raw, "review thread")
                comment_connection = _field_object(item, "comments", "review thread")
                comments = self.review_comments(comment_connection)
                has_more_comments, comment_cursor = _page(
                    comment_connection, "thread comments"
                )
                thread_id = _string(item.get("id"), "thread id", empty=False)
                if has_more_comments:
                    if comment_cursor is None:
                        raise ThreadError("thread comments have no end cursor")
                    comments.extend(
                        self.thread_comment_pages(thread_id, comment_cursor)
                    )
                threads.append(
                    ReviewThread(
                        thread_id=thread_id,
                        is_resolved=_boolean(
                            item.get("isResolved"), "thread isResolved"
                        ),
                        is_outdated=_boolean(
                            item.get("isOutdated"), "thread isOutdated"
                        ),
                        path=_string(item.get("path"), "thread path"),
                        line=_optional_integer(item.get("line"), "thread line"),
                        original_line=_optional_integer(
                            item.get("originalLine"), "thread originalLine"
                        ),
                        comments=tuple(comments),
                    )
                )
            has_next, cursor = _page(connection, "reviewThreads")
            if not has_next:
                return threads

    def reviews(self, number: int) -> list[Review]:
        rows = self.paginated(
            f"repos/{self.repository}/pulls/{number}/reviews",
            f"reading reviews of PR {number}",
        )
        reviews: list[Review] = []
        for item in rows:
            user_value = item.get("user")
            author = None
            if user_value is not None:
                author = _string(
                    _object(user_value, "review user").get("login"),
                    "review user login",
                )
            reviews.append(
                Review(
                    review_id=_integer(item.get("id"), "review id"),
                    author=author,
                    state=_string(item.get("state"), "review state"),
                    commit_id=_optional_string(
                        item.get("commit_id"), "review commit_id"
                    ),
                    body=_string(item.get("body"), "review body"),
                    submitted_at=_optional_string(
                        item.get("submitted_at"), "review submitted_at"
                    ),
                )
            )
        return reviews

    def conversation_comments(self, number: int) -> list[ConversationComment]:
        rows = self.paginated(
            f"repos/{self.repository}/issues/{number}/comments",
            f"reading conversation comments of PR {number}",
        )
        comments: list[ConversationComment] = []
        for item in rows:
            user_value = item.get("user")
            author = None
            if user_value is not None:
                author = _string(
                    _object(user_value, "comment user").get("login"),
                    "comment user login",
                )
            comments.append(
                ConversationComment(
                    comment_id=_integer(item.get("id"), "conversation comment id"),
                    author=author,
                    body=_string(item.get("body"), "conversation comment body"),
                    created_at=_string(
                        item.get("created_at"), "conversation comment created_at"
                    ),
                    url=_string(item.get("html_url"), "conversation comment url"),
                )
            )
        return comments

    def listing(self, number: int) -> Listing:
        return Listing(
            pull_request=self.pull_request(number),
            threads=tuple(self.threads(number)),
            reviews=tuple(self.reviews(number)),
            conversation_comments=tuple(self.conversation_comments(number)),
        )

    def post_file(self, endpoint: str, body_file: Path, context: str) -> JsonObject:
        output = self.command(
            [
                "api",
                "--method",
                "POST",
                "-F",
                f"body=@{body_file}",
                endpoint,
            ],
            context,
        )
        return _object(_json(output, context), context)

    def get_object(self, endpoint: str, context: str) -> JsonObject:
        output = self.command(["api", endpoint], context)
        return _object(_json(output, context), context)

    def compare_contains(self, contained: str, head: str) -> bool:
        output = self.command(
            [
                "api",
                f"repos/{self.repository}/compare/{contained}...{head}",
            ],
            f"comparing {contained} with pull-request head {head}",
        )
        item = _object(_json(output, "comparison"), "comparison")
        return _string(item.get("status"), "comparison status") in {
            "ahead",
            "identical",
        }

    def resolve(self, thread_id: str) -> None:
        document = self.graphql(
            RESOLVE_QUERY, {"id": thread_id}, f"resolving thread {thread_id}"
        )
        data = _field_object(document, "data", "resolve response")
        resolution = _field_object(data, "resolveReviewThread", "resolve data")
        thread = _field_object(resolution, "thread", "resolveReviewThread")
        if not _boolean(thread.get("isResolved"), "resolved thread isResolved"):
            raise ThreadError("GitHub did not report the review thread as resolved")


def normalize_body(body: str) -> str:
    """Normalize line endings and insignificant whitespace at the body end."""

    return body.replace("\r\n", "\n").replace("\r", "\n").rstrip()


def read_body(path: Path) -> str:
    try:
        body = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise ThreadError(f"cannot read {path}: {error}") from error
    if not body.strip():
        raise ThreadError("the body file is empty")
    return body


def _verify_posted_body(
    expected: str, item: JsonObject, context: str
) -> tuple[int, str]:
    body = _string(item.get("body"), f"{context} body")
    if normalize_body(body) != normalize_body(expected):
        raise ThreadError(f"{context} body does not match the body file")
    item_id = _integer(item.get("id"), f"{context} id")
    url = _string(item.get("html_url"), f"{context} html_url", empty=False)
    return item_id, url


def post_and_verify(
    github: GitHub,
    body_file: Path,
    post_endpoint: str,
    read_endpoint: Callable[[int], str],
    noun: str,
) -> str:
    expected = read_body(body_file)
    created = github.post_file(post_endpoint, body_file, f"posting {noun}")
    created_id = _integer(created.get("id"), f"created {noun} id")
    posted = github.get_object(
        read_endpoint(created_id), f"reading created {noun} {created_id}"
    )
    _, url = _verify_posted_body(expected, posted, f"created {noun}")
    return url


def _body_lines(body: str, limit: int = 2) -> list[str]:
    lines = body.splitlines() or [""]
    shown = lines[:limit]
    if len(lines) > limit:
        shown[-1] += " …"
    return shown


def render_listing(listing: Listing, unresolved_only: bool) -> str:
    pull_request = listing.pull_request
    state_flags = [pull_request.state]
    if pull_request.is_draft:
        state_flags.append("draft")
    state_flags.append(f"mergeable: {pull_request.mergeable}")
    lines = [
        f"Pull request: {pull_request.url}",
        f"Head: {pull_request.head_sha}",
        f"State: {', '.join(state_flags)}",
        "Threads:",
    ]
    threads = sorted(listing.threads, key=lambda item: item.is_resolved)
    if unresolved_only:
        threads = [thread for thread in threads if not thread.is_resolved]
    if not threads:
        lines.append("  (none)")
    for thread in threads:
        line = thread.line if thread.line is not None else thread.original_line
        location = f"{thread.path}:{line}" if line is not None else thread.path
        flags = ["resolved" if thread.is_resolved else "unresolved"]
        if thread.is_outdated:
            flags.append("outdated")
        lines.append(f"- {thread.thread_id} {location} [{', '.join(flags)}]")
        for comment in thread.comments:
            author = comment.author or "ghost"
            number = (
                f"#{comment.database_id}"
                if comment.database_id is not None
                else "#unknown"
            )
            body_lines = _body_lines(comment.body)
            lines.append(f"  {author} {number}: {body_lines[0]}")
            lines.extend(f"    {body_line}" for body_line in body_lines[1:])

    lines.append("Reviews with bodies:")
    reviews = [review for review in listing.reviews if review.body.strip()]
    if not reviews:
        lines.append("  (none)")
    for review in reviews:
        lines.append(
            f"- {review.author or 'ghost'} #{review.review_id} [{review.state}]"
        )
        lines.extend(f"  {body_line}" for body_line in _body_lines(review.body))

    lines.append("Conversation comments:")
    if not listing.conversation_comments:
        lines.append("  (none)")
    for comment in listing.conversation_comments:
        lines.append(f"- {comment.author or 'ghost'} #{comment.comment_id}")
        lines.extend(f"  {body_line}" for body_line in _body_lines(comment.body))
    return "\n".join(lines)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="List, reply to, and resolve pull-request review threads."
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    listing = subparsers.add_parser("list", help="list all pull-request feedback")
    listing.add_argument("pull_request", type=int, metavar="PR")
    listing.add_argument("--unresolved", action="store_true")
    listing.add_argument("--json", action="store_true", dest="as_json")
    listing.add_argument("--repo", default=DEFAULT_REPOSITORY, metavar="OWNER/NAME")

    reply = subparsers.add_parser("reply", help="reply to a review comment")
    reply.add_argument("pull_request", type=int, metavar="PR")
    reply.add_argument("comment_id", type=int, metavar="COMMENT_ID")
    reply.add_argument("--body-file", type=Path, required=True, metavar="FILE")
    reply.add_argument("--repo", default=DEFAULT_REPOSITORY, metavar="OWNER/NAME")

    resolve = subparsers.add_parser("resolve", help="resolve a review thread")
    resolve.add_argument("thread_id", metavar="THREAD_ID")
    resolve.add_argument("--pr", type=int, metavar="PR")
    resolve.add_argument("--contains", metavar="SHA")
    resolve.add_argument("--repo", default=DEFAULT_REPOSITORY, metavar="OWNER/NAME")

    comment = subparsers.add_parser("comment", help="post a conversation comment")
    comment.add_argument("pull_request", type=int, metavar="PR")
    comment.add_argument("--body-file", type=Path, required=True, metavar="FILE")
    comment.add_argument("--repo", default=DEFAULT_REPOSITORY, metavar="OWNER/NAME")
    return parser


def run_cli(argv: Sequence[str] | None, runner: Runner = _run) -> int:
    args = _parser().parse_args(argv)
    try:
        repository = cast(str, args.repo)
        github = GitHub(repository, runner)
        command = cast(str, args.command)
        if command == "list":
            number = cast(int, args.pull_request)
            listing = github.listing(number)
            if cast(bool, args.as_json):
                document = listing.as_json()
                if cast(bool, args.unresolved):
                    document["threads"] = [
                        thread.as_json()
                        for thread in listing.threads
                        if not thread.is_resolved
                    ]
                print(json.dumps(document, indent=2, sort_keys=True))
            else:
                print(render_listing(listing, cast(bool, args.unresolved)))
        elif command == "reply":
            number = cast(int, args.pull_request)
            comment_id = cast(int, args.comment_id)
            url = post_and_verify(
                github,
                cast(Path, args.body_file),
                f"repos/{repository}/pulls/{number}/comments/{comment_id}/replies",
                lambda created_id: f"repos/{repository}/pulls/comments/{created_id}",
                "review reply",
            )
            print(url)
        elif command == "comment":
            number = cast(int, args.pull_request)
            url = post_and_verify(
                github,
                cast(Path, args.body_file),
                f"repos/{repository}/issues/{number}/comments",
                lambda created_id: f"repos/{repository}/issues/comments/{created_id}",
                "conversation comment",
            )
            print(url)
        elif command == "resolve":
            pull_request = cast(int | None, args.pr)
            contained = cast(str | None, args.contains)
            if (pull_request is None) != (contained is None):
                raise ThreadError("--pr and --contains must be used together")
            if pull_request is not None and contained is not None:
                if _SHA.fullmatch(contained) is None:
                    raise ThreadError("--contains must be a hexadecimal commit SHA")
                head = github.pull_request(pull_request).head_sha
                if not github.compare_contains(contained, head):
                    raise ThreadError(
                        "the pull request's head does not contain SHA; push it first"
                    )
            github.resolve(cast(str, args.thread_id))
            print(f"resolved {cast(str, args.thread_id)}")
        else:
            raise ThreadError(f"unsupported command {command}")
    except (OSError, ThreadError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    return run_cli(argv)


if __name__ == "__main__":
    raise SystemExit(main())

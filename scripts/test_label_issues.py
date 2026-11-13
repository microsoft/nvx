"""Tests for title-based issue-label suggestions."""

from __future__ import annotations

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import create_autospec, patch

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))

from label_issues import (  # noqa: E402
    GitHub,
    Issue,
    LabelError,
    github_token,
    label_issue,
    labels_for_title,
    load_keyword_labels,
    main,
    opened_issue,
)
from nvx_tools.nightly_ci import GitHubApiError, GitHubClient  # noqa: E402


class TitleLabelsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.rules = load_keyword_labels()

    def test_keywords_are_loaded_from_json(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rules.json"
            path.write_text('{"area: custom": ["custom phrase"]}', encoding="utf-8")
            rules = load_keyword_labels(path)
        self.assertEqual(labels_for_title("CUSTOM phrase", rules), ["area: custom"])

    def test_invalid_keyword_configuration_is_rejected(self) -> None:
        invalid = (
            "not JSON",
            "[]",
            "{}",
            '{"": ["guest"]}',
            '{"area: guest": []}',
            '{"area: guest": "guest"}',
            '{"area: guest": [" "]}',
            '{"area: guest": [1]}',
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rules.json"
            for content in invalid:
                with self.subTest(content=content):
                    path.write_text(content, encoding="utf-8")
                    with self.assertRaises((LabelError, ValueError)):
                        load_keyword_labels(path)

    def test_multiple_labels_are_case_insensitive_and_deduplicated(self) -> None:
        self.assertCountEqual(
            labels_for_title(
                "OpenVMM/microVM: SNAPSHOTS snapshot on WHP and KVM", self.rules
            ),
            ["area: openvmm", "area: snapshot", "platform: kvm", "platform: whp"],
        )

    def test_aliases_and_phrase_whitespace(self) -> None:
        cases = (
            ("nvx-init: boot", ["area: guest"]),
            ("nvx-rs: guest boot", ["area: guest", "area: nvx-rs"]),
            ("Azure \t Linux: network", ["area: userland", "area: networking"]),
            ("azurelinux: documentation", ["area: userland", "area: docs"]),
            (
                "Alpine/Ubuntu: kernel security CI MSHV",
                [
                    "area: kernel",
                    "area: userland",
                    "area: ci",
                    "area: security",
                    "platform: mshv",
                ],
            ),
        )
        for title, expected in cases:
            with self.subTest(title=title):
                self.assertCountEqual(labels_for_title(title, self.rules), expected)

    def test_keywords_do_not_match_within_longer_tokens(self) -> None:
        self.assertEqual(
            labels_for_title(
                "special networkingish snapshots2 nvx-rs-extra nvx-initrd "
                "my_openvmm whpx",
                self.rules,
            ),
            [],
        )
        self.assertCountEqual(
            labels_for_title("`nvx-rs`: (guest), [whp]", self.rules),
            [
                "area: guest",
                "area: nvx-rs",
                "platform: whp",
            ],
        )

    def test_unmatched_titles_do_not_imply_backends(self) -> None:
        for title in ("", "Windows and Linux: pipe timeout", "fix startup"):
            with self.subTest(title=title):
                self.assertEqual(labels_for_title(title, self.rules), [])


class AuthenticationTests(unittest.TestCase):
    def test_environment_token_does_not_invoke_gh(self) -> None:
        for variable in ("GH_TOKEN", "GITHUB_TOKEN"):
            with (
                self.subTest(variable=variable),
                patch.dict(os.environ, {variable: "test-token"}, clear=True),
                patch("label_issues.subprocess.run") as run,
            ):
                self.assertEqual(github_token(), "test-token")
                run.assert_not_called()

    def test_local_gh_login_and_missing_authentication(self) -> None:
        for status, output in ((0, "test-token\n"), (1, ""), (0, "")):
            with (
                self.subTest(status=status, output=output),
                patch.dict(os.environ, {}, clear=True),
                patch(
                    "label_issues.subprocess.run",
                    return_value=subprocess.CompletedProcess([], status, output, ""),
                ) as run,
            ):
                if status or not output:
                    with self.assertRaisesRegex(LabelError, "authentication required"):
                        github_token()
                else:
                    self.assertEqual(github_token(), "test-token")
                self.assertEqual(run.call_args.args[0], ["gh", "auth", "token"])


class GitHubLabelsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.rules = load_keyword_labels()
        sleep = patch("label_issues.time.sleep")
        self.sleep = sleep.start()
        self.addCleanup(sleep.stop)
        self.api = create_autospec(GitHubClient, instance=True)
        client = patch("label_issues.GitHubClient", return_value=self.api)
        client.start()
        self.addCleanup(client.stop)
        token = patch("label_issues.github_token", return_value="test-token")
        token.start()
        self.addCleanup(token.stop)

    def test_labels_are_cached_and_writes_are_spaced(self) -> None:
        self.api.get_all.return_value = [{"name": "area: guest"}]
        github = GitHub("microsoft/nvx")
        with patch("label_issues.time.monotonic", return_value=100.0):
            github.ensure_labels(["area: guest", "area: snapshot"])
            github.ensure_labels(["area: snapshot"])
            github.add_labels(1, ["area: guest"])
        self.api.get_all.assert_called_once()
        self.assertEqual(self.api.post.call_count, 2)
        self.sleep.assert_called_once_with(1.0)

    def test_reads_and_writes_retry_explicit_rate_limit_errors(self) -> None:
        for operation in ("listing", "read", "write"):
            for status in (403, 429):
                with self.subTest(operation=operation, status=status):
                    self.api.reset_mock()
                    self.sleep.reset_mock()
                    github = GitHub("microsoft/nvx")
                    limited = GitHubApiError(status, "secondary rate limit")
                    with contextlib.redirect_stderr(io.StringIO()) as output:
                        if operation == "listing":
                            self.api.get_all.side_effect = [limited, []]
                            self.assertEqual(github.open_issues(), [])
                            self.assertEqual(self.api.get_all.call_count, 2)
                        elif operation == "read":
                            self.api.get.side_effect = [
                                limited,
                                {
                                    "number": 1,
                                    "title": "guest",
                                    "state": "open",
                                    "labels": [],
                                },
                            ]
                            self.assertEqual(github.issue(1).number, 1)
                            self.assertEqual(self.api.get.call_count, 2)
                        else:
                            self.api.post.side_effect = [limited, []]
                            github.add_labels(1, ["area: guest"])
                            self.assertEqual(self.api.post.call_count, 2)
                    self.sleep.assert_called_once_with(60.0)
                    self.assertIn("rate limit", output.getvalue())

    def test_repeated_rate_limits_fail_after_two_retries(self) -> None:
        self.api.post.side_effect = GitHubApiError(429, "rate limited")
        with (
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(GitHubApiError),
        ):
            GitHub("microsoft/nvx").add_labels(1, ["area: guest"])
        self.assertEqual(self.api.post.call_count, 3)
        self.assertEqual(
            [call.args for call in self.sleep.call_args_list], [(60.0,), (60.0,)]
        )

    def test_other_write_errors_are_not_retried(self) -> None:
        for failure in (
            GitHubApiError(403, "permission denied"),
            GitHubApiError(500, "server error"),
            OSError("connection lost"),
        ):
            with self.subTest(failure=str(failure)):
                self.api.reset_mock()
                self.sleep.reset_mock()
                self.api.post.side_effect = failure
                with self.assertRaises(type(failure)):
                    GitHub("microsoft/nvx").add_labels(1, ["area: guest"])
                self.api.post.assert_called_once()
                self.sleep.assert_not_called()

    def test_backfill_loads_configuration_once_for_multiple_issues(self) -> None:
        with (
            patch("label_issues.load_keyword_labels", return_value=self.rules) as load,
            patch.object(
                GitHub,
                "open_issues",
                return_value=[
                    Issue(1, "guest", (), "open", False),
                    Issue(2, "snapshot", (), "open", False),
                ],
            ),
            contextlib.redirect_stdout(io.StringIO()) as output,
        ):
            self.assertEqual(main(["--repository", "microsoft/nvx", "backfill"]), 0)
        load.assert_called_once_with()
        self.assertIn("area: guest", output.getvalue())
        self.assertIn("area: snapshot", output.getvalue())

    def test_backfill_apply_is_explicit(self) -> None:
        with (
            patch.object(
                GitHub,
                "open_issues",
                return_value=[Issue(1, "snapshot", (), "open", False)],
            ),
            patch("label_issues.label_issue") as label,
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(
                main(["--repository", "microsoft/nvx", "backfill", "--apply"]), 0
            )
        self.assertTrue(label.call_args.kwargs["apply"])

    def test_backfill_preview_performs_only_paginated_read(self) -> None:
        listing: list[dict[str, object]] = [
            {"number": 1, "title": "snapshot", "state": "open", "labels": []},
            {
                "number": 2,
                "title": "snapshot",
                "state": "open",
                "labels": [],
                "pull_request": {},
            },
            {"number": 3, "title": "snapshot", "state": "closed", "labels": []},
            {"number": 4, "title": "guest", "state": "open", "labels": []},
        ]
        self.api.get_all.return_value = listing
        with contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(main(["--repository", "microsoft/nvx", "backfill"]), 0)
        self.api.get_all.assert_called_once_with(
            "/repos/microsoft/nvx/issues", {"state": "open"}
        )
        self.api.post.assert_not_called()
        self.api.patch.assert_not_called()
        self.assertIn("#1 snapshot", output.getvalue())
        self.assertNotIn("#2", output.getvalue())
        self.assertNotIn("#3", output.getvalue())
        self.assertIn("#4 guest", output.getvalue())

    def test_apply_preserves_labels_and_existing_definitions(self) -> None:
        github = GitHub("microsoft/nvx")
        stale = Issue(1, "snapshot", (), "open", False)
        current = {
            "number": 1,
            "title": "nvx-rs snapshot",
            "state": "open",
            "labels": [{"name": "bug"}, {"name": "area: snapshot"}],
        }
        self.api.get.return_value = current
        self.api.get_all.return_value = [{"name": "area: nvx-rs"}]
        with contextlib.redirect_stdout(io.StringIO()):
            label_issue(github, stale, self.rules, apply=True)
        self.api.post.assert_called_once_with(
            "/repos/microsoft/nvx/issues/1/labels", {"labels": ["area: nvx-rs"]}
        )

    def test_needs_triage_is_only_a_fallback_for_unlabeled_issues(self) -> None:
        cases: tuple[tuple[str, tuple[str, ...], list[str]], ...] = (
            ("fix startup", (), ["needs triage"]),
            ("fix startup", ("bug",), []),
            ("fix startup", ("needs triage",), []),
            ("guest boot", (), ["area: guest"]),
            ("guest boot", ("area: guest",), []),
        )
        for title, existing, expected in cases:
            with self.subTest(title=title, existing=existing):
                self.api.reset_mock()
                self.api.get.return_value = {
                    "number": 1,
                    "title": title,
                    "state": "open",
                    "labels": [{"name": name} for name in existing],
                }
                self.api.get_all.return_value = [
                    {"name": "needs triage"},
                    {"name": "area: guest"},
                ]
                with contextlib.redirect_stdout(io.StringIO()):
                    label_issue(
                        GitHub("microsoft/nvx"),
                        Issue(1, title, existing, "open", False),
                        self.rules,
                        apply=True,
                    )
                if expected:
                    self.api.post.assert_called_once_with(
                        "/repos/microsoft/nvx/issues/1/labels", {"labels": expected}
                    )
                else:
                    self.api.post.assert_not_called()

    def test_needs_triage_preview_does_not_write(self) -> None:
        with contextlib.redirect_stdout(io.StringIO()) as output:
            label_issue(
                GitHub("microsoft/nvx"),
                Issue(1, "fix startup", (), "open", False),
                self.rules,
                apply=False,
            )
        self.assertIn("needs triage", output.getvalue())
        self.api.get_all.assert_not_called()
        self.api.post.assert_not_called()

    def test_missing_label_is_created_without_editing_existing_metadata(self) -> None:
        github = GitHub("microsoft/nvx")
        self.api.get_all.return_value = [
            {"name": "area: guest"},
            {"name": "platform: whp"},
        ]
        github.ensure_labels(["area: guest", "platform: whp", "area: ci"])
        self.api.get_all.assert_called_once_with("/repos/microsoft/nvx/labels")
        self.api.post.assert_called_once()
        self.assertEqual(self.api.post.call_args.args[1]["name"], "area: ci")
        self.api.patch.assert_not_called()

    def test_apply_rechecks_closed_issues_and_pull_requests(self) -> None:
        for state, is_pr in (("closed", False), ("open", True)):
            github = GitHub("microsoft/nvx")
            with (
                patch.object(
                    github,
                    "issue",
                    return_value=Issue(1, "snapshot", (), state, is_pr),
                ),
                patch.object(github, "ensure_labels") as ensure,
                patch.object(github, "add_labels") as add,
                contextlib.redirect_stdout(io.StringIO()),
            ):
                label_issue(
                    github,
                    Issue(1, "snapshot", (), "open", False),
                    self.rules,
                    apply=True,
                )
            ensure.assert_not_called()
            add.assert_not_called()

    def test_concurrent_label_creation_still_labels_the_issue(self) -> None:
        issue: dict[str, object] = {
            "number": 1,
            "title": "guest",
            "state": "open",
            "labels": [],
        }
        self.api.get.side_effect = [issue, {"name": "area: guest"}]
        self.api.get_all.return_value = []
        self.api.post.side_effect = [GitHubApiError(422, "already exists"), []]
        with contextlib.redirect_stdout(io.StringIO()):
            label_issue(
                GitHub("microsoft/nvx"),
                Issue.parse(issue),
                self.rules,
                apply=True,
            )
        self.api.get.assert_called_with("/repos/microsoft/nvx/labels/area%3A%20guest")
        self.api.post.assert_called_with(
            "/repos/microsoft/nvx/issues/1/labels", {"labels": ["area: guest"]}
        )

    def test_label_creation_failures_are_not_silently_ignored(self) -> None:
        cases = (
            (403, None),
            (422, {"name": "different label"}),
            (422, None),
        )
        for status, confirmation in cases:
            self.api.reset_mock()
            self.api.get_all.return_value = []
            self.api.post.side_effect = GitHubApiError(status, "creation failed")
            self.api.get.side_effect = (
                None if confirmation else GitHubApiError(404, "not found")
            )
            self.api.get.return_value = confirmation
            with (
                self.subTest(status=status, confirmation=confirmation),
                self.assertRaises(GitHubApiError),
            ):
                GitHub("microsoft/nvx").ensure_labels(["area: guest"])
            self.assertEqual(self.api.get.call_count, 0 if status == 403 else 1)

    def test_api_failure_is_visible_and_nonzero(self) -> None:
        self.api.get_all.side_effect = GitHubApiError(403, "permission denied")
        with (
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()) as error,
        ):
            self.assertEqual(main(["--repository", "microsoft/nvx", "backfill"]), 1)
        self.assertIn("permission denied", error.getvalue())

    def test_opened_event_is_validated_and_applied(self) -> None:
        event: dict[str, object] = {
            "action": "opened",
            "repository": {"full_name": "microsoft/nvx"},
            "issue": {"number": 1, "title": "guest", "state": "open", "labels": []},
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "event.json"
            path.write_text(json.dumps(event), encoding="utf-8")
            with patch("label_issues.label_issue") as label:
                self.assertEqual(
                    main(
                        [
                            "--repository",
                            "microsoft/nvx",
                            "opened",
                            "--event",
                            str(path),
                        ]
                    ),
                    0,
                )
            self.assertTrue(label.call_args.kwargs["apply"])
            with self.assertRaisesRegex(LabelError, "repository"):
                opened_issue(path, "other/repo")
            event["action"] = "edited"
            path.write_text(json.dumps(event), encoding="utf-8")
            with self.assertRaisesRegex(LabelError, "opened"):
                opened_issue(path, "microsoft/nvx")


class WorkflowTests(unittest.TestCase):
    def test_only_opened_issues_run_trusted_code_with_limited_permissions(self) -> None:
        path = (
            Path(__file__).resolve().parents[1]
            / ".github"
            / "workflows"
            / "label-issues.yml"
        )
        workflow = yaml.load(path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)
        self.assertEqual(workflow["on"], {"issues": {"types": ["opened"]}})
        self.assertEqual(workflow["permissions"], {})
        self.assertEqual(
            workflow["concurrency"],
            {
                "group": "label-issues",
                "cancel-in-progress": "false",
                "queue": "max",
            },
        )
        job = workflow["jobs"]["label"]
        self.assertEqual(job["permissions"], {"contents": "read", "issues": "write"})
        checkout, label = job["steps"]
        self.assertEqual(checkout["uses"], "actions/checkout@v5")
        self.assertEqual(
            checkout["with"],
            {
                "ref": "${{ github.event.repository.default_branch }}",
                "persist-credentials": "false",
            },
        )
        self.assertEqual(label["env"], {"GH_TOKEN": "${{ github.token }}"})
        self.assertEqual(
            label["run"].split(),
            [
                "python3",
                "scripts/label_issues.py",
                "--repository",
                '"${GITHUB_REPOSITORY}"',
                "opened",
                "--event",
                '"${GITHUB_EVENT_PATH}"',
            ],
        )


if __name__ == "__main__":
    unittest.main()

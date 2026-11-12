"""Tests for title-based issue-label suggestions."""

from __future__ import annotations

import contextlib
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))

from label_issues import (  # noqa: E402
    GitHub,
    Issue,
    LabelError,
    label_issue,
    labels_for_title,
    main,
    opened_issue,
)


class TitleLabelsTests(unittest.TestCase):
    def test_multiple_labels_are_case_insensitive_and_deduplicated(self) -> None:
        self.assertCountEqual(
            labels_for_title("OpenVMM/microVM: SNAPSHOTS snapshot on WHP and KVM"),
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
                self.assertCountEqual(labels_for_title(title), expected)

    def test_keywords_do_not_match_within_longer_tokens(self) -> None:
        self.assertEqual(
            labels_for_title(
                "special networkingish snapshots2 nvx-rs-extra nvx-initrd "
                "my_openvmm whpx"
            ),
            [],
        )
        self.assertCountEqual(
            labels_for_title("`nvx-rs`: (guest), [whp]"),
            [
                "area: guest",
                "area: nvx-rs",
                "platform: whp",
            ],
        )

    def test_unmatched_titles_do_not_imply_backends(self) -> None:
        for title in ("", "Windows and Linux: pipe timeout", "fix startup"):
            with self.subTest(title=title):
                self.assertEqual(labels_for_title(title), [])


class GitHubLabelsTests(unittest.TestCase):
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
        ]
        with (
            patch(
                "label_issues.subprocess.run",
                return_value=subprocess.CompletedProcess(
                    [], 0, json.dumps(listing), ""
                ),
            ) as run,
            contextlib.redirect_stdout(io.StringIO()) as output,
        ):
            self.assertEqual(main(["--repository", "microsoft/nvx", "backfill"]), 0)
        run.assert_called_once()
        arguments = run.call_args.args[0]
        self.assertIn("--paginate", arguments)
        self.assertEqual(arguments[arguments.index("--method") + 1], "GET")
        self.assertIn("#1 snapshot", output.getvalue())
        self.assertNotIn("#2", output.getvalue())
        self.assertNotIn("#3", output.getvalue())

    def test_apply_preserves_labels_and_existing_definitions(self) -> None:
        github = GitHub("microsoft/nvx")
        stale = Issue(1, "snapshot", (), "open", False)
        current = {
            "number": 1,
            "title": "nvx-rs snapshot",
            "state": "open",
            "labels": [{"name": "bug"}, {"name": "area: snapshot"}],
        }
        with (
            patch.object(
                github, "api", side_effect=[current, ["area: nvx-rs"], []]
            ) as api,
            contextlib.redirect_stdout(io.StringIO()),
        ):
            label_issue(github, stale, apply=True)
        self.assertEqual(api.call_count, 3)
        self.assertEqual(
            api.call_args.kwargs,
            {"method": "POST", "body": {"labels": ["area: nvx-rs"]}},
        )
        self.assertTrue(api.call_args.args[0].endswith("/issues/1/labels"))

    def test_missing_label_is_created_without_editing_existing_metadata(self) -> None:
        github = GitHub("microsoft/nvx")
        with patch.object(github, "api", side_effect=[["area: guest"], {}, {}]) as api:
            github.ensure_labels(["area: guest", "platform: whp", "area: ci"])
        self.assertEqual(api.call_count, 3)
        for call in api.call_args_list[1:]:
            self.assertEqual(call.kwargs["method"], "POST")
        self.assertEqual(api.call_args_list[1].kwargs["body"]["name"], "platform: whp")

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
                label_issue(github, Issue(1, "snapshot", (), "open", False), apply=True)
            ensure.assert_not_called()
            add.assert_not_called()

    def test_api_failure_is_visible_and_nonzero(self) -> None:
        with (
            patch(
                "label_issues.subprocess.run",
                return_value=subprocess.CompletedProcess(
                    [], 1, "", "permission denied"
                ),
            ),
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

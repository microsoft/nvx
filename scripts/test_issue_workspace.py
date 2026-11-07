#!/usr/bin/env python3
"""Tests for deterministic NVX issue discovery and workspace handling."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import issue_workspace  # noqa: E402
from openvmm_git import git  # noqa: E402


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=True,
    ).stdout.strip()


def _configure(repo: Path) -> None:
    for key, value in (
        ("user.name", "NVX Test"),
        ("user.email", "nvx@example.com"),
        ("commit.gpgsign", "false"),
    ):
        _git(repo, "config", key, value)


def _commit(repo: Path, name: str, message: str) -> str:
    (repo / name).write_text(f"{message}\n", encoding="utf-8")
    _git(repo, "add", name)
    _git(repo, "commit", "--quiet", "-m", message)
    return _git(repo, "rev-parse", "HEAD")


class IssueRepository:
    """A scratch NVX clone with a real OpenVMM submodule and dev ref."""

    def __init__(self, root: Path) -> None:
        self.openvmm_source = root / "openvmm-source"
        self.openvmm_source.mkdir()
        _git(self.openvmm_source, "init", "--quiet")
        _configure(self.openvmm_source)
        self.openvmm_pin = _commit(self.openvmm_source, "pin", "initial OpenVMM")
        self.openvmm_later = _commit(self.openvmm_source, "later", "later OpenVMM")
        _git(self.openvmm_source, "checkout", "--quiet", self.openvmm_pin)

        self.source = root / "nvx-source"
        self.source.mkdir()
        _git(self.source, "init", "--quiet")
        _configure(self.source)
        _commit(self.source, "README", "initial NVX")
        _git(
            self.source,
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            str(self.openvmm_source),
            "openvmm",
        )
        _git(
            self.source,
            "config",
            "-f",
            ".gitmodules",
            "submodule.openvmm.url",
            "https://github.com/microsoft/openvmm.git",
        )
        _git(self.source, "add", ".gitmodules", "openvmm")
        _git(self.source, "commit", "--quiet", "-m", "add OpenVMM")
        _git(self.source, "branch", "-M", "dev")

        self.path = root / "nvx"
        subprocess.run(
            [
                "git",
                "clone",
                "--quiet",
                "--branch",
                "dev",
                str(self.source),
                str(self.path),
            ],
            capture_output=True,
            check=True,
        )
        _configure(self.path)
        _git(
            self.path,
            "-c",
            "protocol.file.allow=always",
            "-c",
            f"submodule.openvmm.url={self.openvmm_source}",
            "submodule",
            "update",
            "--init",
            "--quiet",
            "openvmm",
        )
        _configure(self.path / "openvmm")
        _git(
            self.path,
            "remote",
            "set-url",
            "origin",
            "https://github.com/microsoft/nvx.git",
        )
        _git(
            self.path,
            "remote",
            "set-url",
            "--push",
            "origin",
            "https://github.com/microsoft/nvx.git",
        )

    def runner(self, repo: Path, *args: str) -> str:
        if args == ("config", "--bool", "--get", "commit.gpgsign"):
            return "true"
        if args == ("fetch", "origin", "dev"):
            return git(
                repo,
                "fetch",
                str(self.source),
                "dev:refs/remotes/origin/dev",
            )
        if args[:3] == ("submodule", "update", "--init"):
            return git(
                repo,
                "-c",
                "protocol.file.allow=always",
                "-c",
                f"submodule.openvmm.url={self.openvmm_source}",
                *args,
            )
        return git(repo, *args)


class FakeGh:
    """A deterministic fake for the three GitHub queries used by find."""

    def __init__(self, *, unsupported_links: bool = False) -> None:
        self.calls: list[tuple[str, ...]] = []
        self.unsupported_links = unsupported_links

    def __call__(self, args: Sequence[str]) -> issue_workspace.CommandResult:
        call = tuple(args)
        self.calls.append(call)
        if call[:2] == ("pr", "list"):
            return issue_workspace.CommandResult(
                0,
                json.dumps(
                    [
                        {
                            "number": 10,
                            "title": "Fix #57",
                            "body": "",
                            "state": "OPEN",
                            "headRefName": "fix-57",
                            "url": "https://example.test/pr/10",
                        },
                        {
                            "number": 11,
                            "title": "Fix #570",
                            "body": "unrelated",
                            "state": "OPEN",
                            "headRefName": "fix-570",
                            "url": "https://example.test/pr/11",
                        },
                    ]
                ),
            )
        if call[:2] == ("issue", "view"):
            if self.unsupported_links:
                return issue_workspace.CommandResult(
                    1,
                    stderr="unknown JSON field: closedByPullRequestsReferences",
                )
            return issue_workspace.CommandResult(
                0,
                json.dumps(
                    {
                        "title": "Example",
                        "state": "OPEN",
                        "url": "https://example.test/issue/57",
                        "closedByPullRequestsReferences": [
                            {
                                "number": 10,
                                "title": "Fix #57",
                                "state": "OPEN",
                                "url": "https://example.test/pr/10",
                            },
                            {
                                "number": 12,
                                "title": "Linked fix",
                                "state": "MERGED",
                                "url": "https://example.test/pr/12",
                            },
                        ],
                    }
                ),
            )
        if call[:3] == ("issue", "develop", "--list"):
            return issue_workspace.CommandResult(
                0, "microsoft/nvx agents/issue-57-linked\n"
            )
        raise AssertionError(f"unexpected gh call: {call}")


class IssueWorkspaceTests(unittest.TestCase):
    root: Path
    repository: IssueRepository

    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.repository = IssueRepository(self.root)

    def test_find_groups_exact_issue_matches_from_every_source(self) -> None:
        repo = self.repository.path
        dev_commit = _commit(repo, "dev-issue", "Fix #57 on dev")
        _git(repo, "update-ref", "refs/remotes/origin/dev", dev_commit)
        _git(repo, "switch", "--quiet", "-c", "agents/issue-57-local")
        topic_commit = _commit(repo, "topic", "Work on issues/57")
        _git(repo, "switch", "--quiet", "dev")
        _git(repo, "branch", "issue-570-unrelated")
        _git(repo, "update-ref", "refs/remotes/origin/feature/57", topic_commit)
        worktree = self.root / "workspace-57"
        _git(repo, "worktree", "add", "--quiet", "--detach", str(worktree), "HEAD")
        self.addCleanup(
            lambda: subprocess.run(
                [
                    "git",
                    "-C",
                    str(repo),
                    "worktree",
                    "remove",
                    "--force",
                    str(worktree),
                ],
                capture_output=True,
                check=False,
            )
        )
        fake = FakeGh()
        result = issue_workspace.find_issue(
            repo,
            57,
            fetch=False,
            gh_runner=fake,
        )
        groups = {candidate.group for candidate in result.candidates}
        self.assertEqual(
            groups,
            {
                "pull requests",
                "linked branches",
                "branches",
                "worktrees",
                "commits not on origin/dev",
                "commits on origin/dev",
            },
        )
        names = [candidate.name for candidate in result.candidates]
        self.assertTrue(any(name.startswith("#10 ") for name in names))
        self.assertTrue(any(name.startswith("#12 ") for name in names))
        self.assertFalse(any("#11 " in name for name in names))
        self.assertFalse(any("570" in name for name in names))
        pull = next(
            candidate
            for candidate in result.candidates
            if candidate.name.startswith("#10 ")
        )
        self.assertEqual(len(pull.evidence), 2)
        self.assertEqual(len(fake.calls), 3)

    def test_find_notes_an_unsupported_linked_pull_request_field(self) -> None:
        result = issue_workspace.find_issue(
            self.repository.path,
            57,
            fetch=False,
            gh_runner=FakeGh(unsupported_links=True),
        )
        self.assertTrue(
            any(
                "unsupported" in note or "does not support" in note
                for note in result.notes
            )
        )

    def test_create_uses_the_clean_checkout_and_records_then_restores_it(self) -> None:
        repo = self.repository.path
        starting_head = _git(repo, "rev-parse", "HEAD")
        created = issue_workspace.create_workspace(
            repo,
            57,
            "example",
            verify=False,
            git_runner=self.repository.runner,
        )
        self.assertEqual(created.mode, "in-place")
        self.assertEqual(created.path, repo)
        self.assertEqual(_git(repo, "branch", "--show-current"), created.branch)
        state = json.loads(created.state_file.read_text(encoding="utf-8"))
        self.assertEqual(state["starting_branch"], "dev")
        self.assertEqual(state["starting_head"], starting_head)
        self.assertEqual(state["mode"], "in-place")
        self.assertEqual(state["base_commit"], created.base_commit)
        cleaned = issue_workspace.cleanup_workspace(
            repo, 57, git_runner=self.repository.runner
        )
        self.assertTrue(cleaned.restored)
        self.assertEqual(_git(repo, "branch", "--show-current"), "dev")
        self.assertFalse(_git(repo, "branch", "--list", created.branch))
        self.assertFalse(created.state_file.exists())

    def test_cleanup_keeps_a_manually_restored_checkout_with_changes(self) -> None:
        repo = self.repository.path
        created = issue_workspace.create_workspace(
            repo,
            65,
            "keep-changes",
            verify=False,
            git_runner=self.repository.runner,
        )
        _git(repo, "switch", "--quiet", "dev")
        (repo / "user-file").write_text("preserve\n", encoding="utf-8")
        cleaned = issue_workspace.cleanup_workspace(
            repo, 65, git_runner=self.repository.runner
        )
        self.assertFalse(cleaned.restored)
        self.assertTrue(created.state_file.exists())
        self.assertTrue(_git(repo, "branch", "--list", created.branch))

    def test_create_uses_a_worktree_when_the_checkout_is_dirty(self) -> None:
        repo = self.repository.path
        dirty = repo / "local-change"
        dirty.write_text("preserve\n", encoding="utf-8")
        path = self.root / "issue-worktree"
        created = issue_workspace.create_workspace(
            repo,
            58,
            "dirty",
            path=path,
            verify=False,
            git_runner=self.repository.runner,
        )
        self.assertEqual(created.mode, "worktree")
        self.assertEqual(_git(repo, "branch", "--show-current"), "dev")
        self.assertTrue(dirty.exists())
        dirty.unlink()
        cleaned = issue_workspace.cleanup_workspace(
            repo, 58, git_runner=self.repository.runner
        )
        self.assertTrue(cleaned.restored)
        self.assertFalse(path.exists())

    def test_create_uses_a_worktree_when_openvmm_is_off_its_pin(self) -> None:
        repo = self.repository.path
        _git(repo / "openvmm", "checkout", "--quiet", self.repository.openvmm_later)
        path = self.root / "off-pin-worktree"
        created = issue_workspace.create_workspace(
            repo,
            59,
            "off-pin",
            path=path,
            verify=False,
            git_runner=self.repository.runner,
        )
        self.assertEqual(created.mode, "worktree")
        self.assertEqual(
            _git(repo / "openvmm", "rev-parse", "HEAD"),
            self.repository.openvmm_later,
        )

    def test_create_refuses_a_wrong_origin_fetch_url(self) -> None:
        _git(
            self.repository.path,
            "remote",
            "set-url",
            "origin",
            "https://github.com/example/nvx.git",
        )
        with self.assertRaisesRegex(issue_workspace.WorkspaceError, "ask the user"):
            issue_workspace.create_workspace(
                self.repository.path,
                60,
                "wrong-origin",
                verify=False,
                git_runner=self.repository.runner,
            )

    def test_create_refuses_an_origin_push_url_to_a_fork(self) -> None:
        _git(
            self.repository.path,
            "remote",
            "set-url",
            "--push",
            "origin",
            "https://github.com/example/nvx.git",
        )
        with self.assertRaisesRegex(issue_workspace.WorkspaceError, "push URLs"):
            issue_workspace.create_workspace(
                self.repository.path,
                61,
                "wrong-push",
                verify=False,
                git_runner=self.repository.runner,
            )

    def test_create_refuses_when_commit_signing_is_disabled(self) -> None:
        with self.assertRaisesRegex(issue_workspace.WorkspaceError, "commit.gpgsign"):
            issue_workspace.create_workspace(
                self.repository.path,
                62,
                "unsigned",
                verify=False,
            )

    def test_create_refuses_the_legacy_nanvix_submodule_url(self) -> None:
        _git(
            self.repository.path,
            "config",
            "submodule.openvmm.url",
            "https://github.com/nanvix/openvmm.git",
        )
        with self.assertRaisesRegex(issue_workspace.WorkspaceError, "submodule sync"):
            issue_workspace.create_workspace(
                self.repository.path,
                63,
                "old-url",
                verify=False,
                git_runner=self.repository.runner,
            )

    def test_create_refuses_an_existing_topic_branch(self) -> None:
        branch = "agents/issue-64-existing"
        _git(self.repository.path, "branch", branch)
        with self.assertRaisesRegex(issue_workspace.WorkspaceError, "already exists"):
            issue_workspace.create_workspace(
                self.repository.path,
                64,
                "existing",
                verify=False,
                git_runner=self.repository.runner,
            )


if __name__ == "__main__":
    unittest.main()

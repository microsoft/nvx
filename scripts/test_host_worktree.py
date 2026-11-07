#!/usr/bin/env python3
"""Tests for creating and removing isolated NVX worktrees."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import host_worktree  # noqa: E402
from openvmm_git import GitError, git  # noqa: E402


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


class WorktreeRepository:
    """A scratch NVX repository with a real local OpenVMM submodule."""

    def __init__(self, root: Path) -> None:
        self.openvmm = root / "openvmm-source"
        self.openvmm.mkdir()
        _git(self.openvmm, "init", "--quiet")
        _configure(self.openvmm)
        (self.openvmm / "openvmm.txt").write_text("openvmm\n", encoding="utf-8")
        _git(self.openvmm, "add", "openvmm.txt")
        _git(self.openvmm, "commit", "--quiet", "-m", "initial OpenVMM")
        self.openvmm_commit = _git(self.openvmm, "rev-parse", "HEAD")

        self.path = root / "nvx"
        self.path.mkdir()
        _git(self.path, "init", "--quiet")
        _configure(self.path)
        (self.path / "README").write_text("nvx\n", encoding="utf-8")
        _git(self.path, "add", "README")
        _git(self.path, "commit", "--quiet", "-m", "initial NVX")
        _git(
            self.path,
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            str(self.openvmm),
            "openvmm",
        )
        _git(self.path, "commit", "--quiet", "-am", "add OpenVMM")
        _git(
            self.path,
            "remote",
            "add",
            "origin",
            "https://github.com/microsoft/nvx.git",
        )
        _git(
            self.path,
            "config",
            "submodule.openvmm.url",
            "https://github.com/microsoft/openvmm.git",
        )

    def runner(self, repo: Path, *args: str) -> str:
        if args[:3] == ("submodule", "update", "--init"):
            return git(
                repo,
                "-c",
                "protocol.file.allow=always",
                "-c",
                f"submodule.openvmm.url={self.openvmm}",
                *args,
            )
        return git(repo, *args)


class HostWorktreeTests(unittest.TestCase):
    root: Path
    repository: WorktreeRepository

    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.repository = WorktreeRepository(self.root)

    def test_add_creates_a_detached_worktree_at_the_requested_revision(self) -> None:
        path = self.root / "detached"
        expected = _git(self.repository.path, "rev-parse", "HEAD")
        result = host_worktree.add_worktree(
            self.repository.path,
            "HEAD",
            path=path,
            git_runner=self.repository.runner,
        )
        self.assertEqual(result.path, path)
        self.assertEqual(result.commit, expected)
        self.assertIsNone(result.branch)
        self.assertEqual(result.openvmm_commit, self.repository.openvmm_commit)
        self.assertEqual(_git(path, "rev-parse", "--abbrev-ref", "HEAD"), "HEAD")
        host_worktree.remove_worktree(self.repository.path, path, discard=True)

    def test_add_creates_a_nontracking_topic_branch(self) -> None:
        path = self.root / "branch"
        branch = "agents/issue-57-example"
        result = host_worktree.add_worktree(
            self.repository.path,
            "HEAD",
            path=path,
            branch=branch,
            git_runner=self.repository.runner,
        )
        self.assertEqual(result.branch, branch)
        self.assertEqual(_git(path, "branch", "--show-current"), branch)
        self.assertEqual(
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(path),
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
                capture_output=True,
                check=False,
            ).returncode,
            128,
        )
        host_worktree.remove_worktree(self.repository.path, path, discard=True)

    def test_remove_refuses_changes_unless_discarding_them(self) -> None:
        path = self.root / "dirty"
        host_worktree.add_worktree(
            self.repository.path,
            "HEAD",
            path=path,
            git_runner=self.repository.runner,
        )
        (path / "untracked").write_text("keep me\n", encoding="utf-8")
        with self.assertRaisesRegex(GitError, "dirty worktree"):
            host_worktree.remove_worktree(self.repository.path, path)
        self.assertTrue(path.exists())
        result = host_worktree.remove_worktree(self.repository.path, path, discard=True)
        self.assertEqual(result.path, path)
        self.assertFalse(path.exists())

    def test_remove_refuses_changes_inside_openvmm(self) -> None:
        path = self.root / "dirty-submodule"
        host_worktree.add_worktree(
            self.repository.path,
            "HEAD",
            path=path,
            git_runner=self.repository.runner,
        )
        (path / "openvmm" / "local").write_text("keep me\n", encoding="utf-8")
        with self.assertRaisesRegex(GitError, "openvmm"):
            host_worktree.remove_worktree(self.repository.path, path)
        host_worktree.remove_worktree(self.repository.path, path, discard=True)

    def test_add_refuses_a_remote_that_is_not_microsoft_nvx(self) -> None:
        _git(
            self.repository.path,
            "remote",
            "set-url",
            "origin",
            "https://github.com/example/nvx.git",
        )
        with self.assertRaisesRegex(GitError, "user.*approve|approve.*user"):
            host_worktree.add_worktree(
                self.repository.path,
                "HEAD",
                path=self.root / "wrong-origin",
                git_runner=self.repository.runner,
            )

    def test_add_refuses_the_legacy_nanvix_submodule_url(self) -> None:
        _git(
            self.repository.path,
            "config",
            "submodule.openvmm.url",
            "https://github.com/nanvix/openvmm.git",
        )
        with self.assertRaisesRegex(GitError, "approve.*submodule URL"):
            host_worktree.add_worktree(
                self.repository.path,
                "HEAD",
                path=self.root / "wrong-submodule",
                git_runner=self.repository.runner,
            )


if __name__ == "__main__":
    unittest.main()

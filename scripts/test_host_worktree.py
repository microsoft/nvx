#!/usr/bin/env python3
"""Tests for creating and removing isolated NVX worktrees."""

from __future__ import annotations

import shutil
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

    def refusing_runner(self, repo: Path, *args: str) -> str:
        """Run git like runner, but fail instead of adding a worktree, for
        checks that must refuse before changing anything."""
        if args[:2] == ("worktree", "add"):
            raise AssertionError(f"git {' '.join(args)} ran before a refusal")
        return self.runner(repo, *args)


class HostWorktreeTests(unittest.TestCase):
    root: Path
    repository: WorktreeRepository

    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        # The tool resolves the paths that it reports, which expands an 8.3
        # name such as the RUNNER~1 of a Windows runner's temporary directory.
        self.root = Path(directory.name).resolve()
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

    def test_remove_refuses_untracked_files_that_status_would_hide(self) -> None:
        # status.showUntrackedFiles=no hides untracked files from git status,
        # whether NVX's configuration or OpenVMM's sets it.
        for name, directory in (("nvx", "."), ("submodule", "openvmm")):
            with self.subTest(location=name):
                path = self.root / f"hidden-{name}"
                host_worktree.add_worktree(
                    self.repository.path,
                    "HEAD",
                    path=path,
                    git_runner=self.repository.runner,
                )
                checkout = path / directory
                _git(checkout, "config", "status.showUntrackedFiles", "no")
                untracked = checkout / "untracked"
                untracked.write_text("keep me\n", encoding="utf-8")
                with self.assertRaisesRegex(GitError, "dirty worktree"):
                    host_worktree.remove_worktree(self.repository.path, path)
                self.assertTrue(untracked.exists())
                host_worktree.remove_worktree(self.repository.path, path, discard=True)

    def test_remove_refuses_an_openvmm_commit_that_submodule_ignore_hides(
        self,
    ) -> None:
        path = self.root / "ignored-commit"
        host_worktree.add_worktree(
            self.repository.path,
            "HEAD",
            path=path,
            git_runner=self.repository.runner,
        )
        # submodule.openvmm.ignore=all hides a new OpenVMM commit from NVX's
        # status, and the commit leaves OpenVMM's own status clean.
        _git(path, "config", "submodule.openvmm.ignore", "all")
        openvmm = path / "openvmm"
        _configure(openvmm)
        (openvmm / "local").write_text("keep me\n", encoding="utf-8")
        _git(openvmm, "add", "local")
        _git(openvmm, "commit", "--quiet", "-m", "local OpenVMM change")
        with self.assertRaisesRegex(GitError, "dirty worktree"):
            host_worktree.remove_worktree(self.repository.path, path)
        self.assertTrue((openvmm / "local").exists())
        host_worktree.remove_worktree(self.repository.path, path, discard=True)

    def test_add_checks_out_openvmm_whatever_its_update_setting(self) -> None:
        repo = self.repository.path
        # submodule.openvmm.update=none, from the configuration or from
        # .gitmodules, makes git submodule update skip OpenVMM.
        for source in ("configuration", ".gitmodules"):
            with self.subTest(source=source):
                if source == "configuration":
                    _git(repo, "config", "submodule.openvmm.update", "none")
                else:
                    _git(repo, "config", "--unset", "submodule.openvmm.update")
                    _git(
                        repo,
                        "config",
                        "-f",
                        ".gitmodules",
                        "submodule.openvmm.update",
                        "none",
                    )
                    _git(repo, "commit", "--quiet", "-am", "skip OpenVMM updates")
                path = self.root / f"update-none-{source.strip('.')}"
                result = host_worktree.add_worktree(
                    repo,
                    "HEAD",
                    path=path,
                    git_runner=self.repository.runner,
                )
                self.assertEqual(result.openvmm_commit, self.repository.openvmm_commit)
                self.assertTrue((path / "openvmm" / ".git").exists())
                host_worktree.remove_worktree(repo, path, discard=True)

    def test_add_fails_when_openvmm_stays_uninitialized(self) -> None:
        def skipping_runner(repo: Path, *args: str) -> str:
            if args[:2] == ("submodule", "update"):
                return ""
            return self.repository.runner(repo, *args)

        path = self.root / "uninitialized"
        with self.assertRaisesRegex(GitError, "uninitialized"):
            host_worktree.add_worktree(
                self.repository.path,
                "HEAD",
                path=path,
                git_runner=skipping_runner,
            )
        self.assertTrue(path.exists())

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

    def test_rejected_urls_hide_their_credentials(self) -> None:
        repo = self.repository.path
        url = "https://user:TOKEN@github.com/example/{}.git"
        _git(repo, "remote", "set-url", "origin", url.format("nvx"))
        self.assert_refused_without_credentials("https://github.com/example/nvx.git")
        _git(
            repo, "remote", "set-url", "origin", "https://github.com/microsoft/nvx.git"
        )
        _git(repo, "config", "submodule.openvmm.url", url.format("openvmm"))
        self.assert_refused_without_credentials(
            "https://github.com/example/openvmm.git"
        )
        _git(repo, "config", "--unset", "submodule.openvmm.url")
        _git(
            repo,
            "config",
            "-f",
            ".gitmodules",
            "submodule.openvmm.url",
            url.format("openvmm"),
        )
        _git(repo, "commit", "--quiet", "-am", "record a URL with credentials")
        self.assert_refused_without_credentials(
            "https://github.com/example/openvmm.git"
        )

    def assert_refused_without_credentials(self, redacted: str) -> None:
        with self.assertRaises(GitError) as raised:
            host_worktree.add_worktree(
                self.repository.path,
                "HEAD",
                path=self.root / "refused",
                git_runner=self.repository.refusing_runner,
            )
        self.assertIn(redacted, str(raised.exception))
        self.assertNotIn("TOKEN", str(raised.exception))

    def test_add_checks_the_url_that_insteadof_rewrites_the_submodule_url_to(
        self,
    ) -> None:
        repo = self.repository.path
        # git clones the URL that a url.<base>.insteadOf setting rewrites a
        # submodule's URL to, whether the configuration sets that URL or
        # .gitmodules records it.
        _git(
            repo,
            "config",
            "url.https://github.com/example/openvmm.git.insteadOf",
            "https://github.com/microsoft/openvmm.git",
        )
        rewritten = "insteadOf rewrites to https://github.com/example/openvmm.git"
        with self.assertRaisesRegex(GitError, rewritten):
            host_worktree.add_worktree(
                repo,
                "HEAD",
                path=self.root / "configured",
                git_runner=self.repository.refusing_runner,
            )
        _git(repo, "config", "--unset", "submodule.openvmm.url")
        _git(
            repo,
            "config",
            "-f",
            ".gitmodules",
            "submodule.openvmm.url",
            "https://github.com/microsoft/openvmm.git",
        )
        _git(repo, "commit", "--quiet", "-am", "record OpenVMM's URL")
        with self.assertRaisesRegex(GitError, rewritten):
            host_worktree.add_worktree(
                repo,
                "HEAD",
                path=self.root / "recorded",
                git_runner=self.repository.refusing_runner,
            )

    def test_remove_keeps_the_records_of_other_missing_worktrees(self) -> None:
        repo = self.repository.path
        # A worktree on a drive that is not mounted looks missing, and
        # git worktree prune would delete its records.
        missing = self.root / "unmounted"
        _git(repo, "worktree", "add", "--quiet", "--detach", str(missing), "HEAD")
        shutil.rmtree(missing)
        path = self.root / "removed"
        host_worktree.add_worktree(
            repo, "HEAD", path=path, git_runner=self.repository.runner
        )
        host_worktree.remove_worktree(repo, path)
        listed = {
            Path(line.removeprefix("worktree ")).resolve()
            for line in _git(repo, "worktree", "list", "--porcelain").splitlines()
            if line.startswith("worktree ")
        }
        self.assertIn(missing.resolve(), listed)
        self.assertNotIn(path.resolve(), listed)

    def test_worktree_records_keep_a_newline_in_a_path_and_read_older_git(
        self,
    ) -> None:
        nul_terminated = (
            "worktree /srv/a\nb\0HEAD 1234\0detached\0\0"
            "worktree /srv/c\0HEAD 5678\0branch refs/heads/topic\0\0"
        )
        line_terminated = "worktree /srv/c\nHEAD 5678\nbranch refs/heads/topic"

        def current(repo: Path, *args: str) -> str:
            del repo
            if args == ("worktree", "list", "--porcelain", "-z"):
                return nul_terminated
            raise AssertionError(args)

        def before_2_36(repo: Path, *args: str) -> str:
            del repo
            if args == ("worktree", "list", "--porcelain", "-z"):
                raise GitError("error: unknown switch `z'")
            if args == ("worktree", "list", "--porcelain"):
                return line_terminated
            raise AssertionError(args)

        topic = {"worktree": "/srv/c", "HEAD": "5678", "branch": "refs/heads/topic"}
        self.assertEqual(
            host_worktree.worktree_records(Path("."), current),
            [{"worktree": "/srv/a\nb", "HEAD": "1234", "detached": ""}, topic],
        )
        self.assertEqual(
            host_worktree.worktree_records(Path("."), before_2_36), [topic]
        )

    def test_a_worktree_at_a_non_ascii_path_with_a_space_round_trips(self) -> None:
        path = self.root / "caf\u00e9 wt"
        host_worktree.add_worktree(
            self.repository.path,
            "HEAD",
            path=path,
            git_runner=self.repository.runner,
        )
        host_worktree.remove_worktree(self.repository.path, path, discard=True)
        self.assertFalse(path.exists())

    def test_remove_refuses_commits_that_only_a_detached_worktree_holds(
        self,
    ) -> None:
        path = self.root / "advanced"
        host_worktree.add_worktree(
            self.repository.path,
            "HEAD",
            path=path,
            git_runner=self.repository.runner,
        )
        # A commit leaves the status clean, and removing the worktree would
        # delete the HEAD and reflog that alone reach it.
        (path / "work").write_text("keep me\n", encoding="utf-8")
        _git(path, "add", "work")
        _git(path, "commit", "--quiet", "-m", "local work")
        with self.assertRaisesRegex(GitError, "1 commit"):
            host_worktree.remove_worktree(self.repository.path, path)
        self.assertTrue((path / "work").exists())
        host_worktree.remove_worktree(self.repository.path, path, discard=True)

    def test_remove_accepts_a_start_that_no_ref_holds(self) -> None:
        repo = self.repository.path
        # A revision fetched by object name, such as a pull request's head,
        # need not be on a branch; only the commits after it count.
        start = _git(repo, "commit-tree", "-p", "HEAD", "-m", "fetched", "HEAD^{tree}")
        path = self.root / "fetched"
        host_worktree.add_worktree(
            repo, start, path=path, git_runner=self.repository.runner
        )
        host_worktree.remove_worktree(repo, path)
        self.assertFalse(path.exists())

    def test_add_checks_the_gitmodules_url_of_the_requested_revision(self) -> None:
        repo = self.repository.path
        revisions: dict[str, str] = {}
        for owner in ("nanvix", "example", "microsoft"):
            _git(
                repo,
                "config",
                "-f",
                ".gitmodules",
                "submodule.openvmm.url",
                f"https://github.com/{owner}/openvmm.git",
            )
            _git(repo, "commit", "--quiet", "-am", f"use {owner}/openvmm")
            revisions[owner] = _git(repo, "rev-parse", "HEAD")
        # Without a configured URL, OpenVMM's initialization clones the URL
        # that the requested revision's .gitmodules records.
        _git(repo, "config", "--unset", "submodule.openvmm.url")
        for owner in ("nanvix", "example"):
            with self.subTest(owner=owner):
                path = self.root / f"revision-{owner}"
                with self.assertRaisesRegex(GitError, f"{owner}/openvmm.*approve"):
                    host_worktree.add_worktree(
                        repo,
                        revisions[owner],
                        path=path,
                        git_runner=self.repository.refusing_runner,
                    )
                self.assertFalse(path.exists())
        path = self.root / "revision-microsoft"
        result = host_worktree.add_worktree(
            repo,
            revisions["microsoft"],
            path=path,
            git_runner=self.repository.runner,
        )
        self.assertEqual(result.openvmm_commit, self.repository.openvmm_commit)
        host_worktree.remove_worktree(repo, path, discard=True)

    def test_add_checks_the_url_of_the_name_that_gitmodules_gives_openvmm(
        self,
    ) -> None:
        # git finds the URL by the submodule's name in .gitmodules, so the
        # configured submodule.openvmm.url does not cover another name.
        repo = self.repository.path
        (repo / ".gitmodules").write_text(
            '[submodule "fork"]\n'
            "\tpath = openvmm\n"
            "\turl = https://github.com/example/openvmm.git\n",
            encoding="utf-8",
        )
        _git(repo, "commit", "--quiet", "-am", "rename the OpenVMM submodule")
        path = self.root / "renamed"
        with self.assertRaisesRegex(
            GitError, r"submodule\.fork\.url.*example/openvmm.*approve"
        ):
            host_worktree.add_worktree(
                repo,
                "HEAD",
                path=path,
                git_runner=self.repository.refusing_runner,
            )
        self.assertFalse(path.exists())


if __name__ == "__main__":
    unittest.main()

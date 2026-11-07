#!/usr/bin/env python3
"""Tests for recording and verifying NVX rebase invariants."""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import rebase_check  # noqa: E402


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


def _commit(repo: Path, name: str, contents: str, message: str) -> str:
    (repo / name).write_text(contents, encoding="utf-8")
    _git(repo, "add", name)
    _git(repo, "commit", "--quiet", "-m", message)
    return _git(repo, "rev-parse", "HEAD")


class RebaseRepository:
    """A scratch branch over origin/dev with a real OpenVMM submodule."""

    def __init__(self, root: Path) -> None:
        self.openvmm_source = root / "openvmm-source"
        self.openvmm_source.mkdir()
        _git(self.openvmm_source, "init", "--quiet")
        _configure(self.openvmm_source)
        self.openvmm_pin = _commit(
            self.openvmm_source,
            "pin",
            "pin\n",
            "initial OpenVMM",
        )
        self.openvmm_later = _commit(
            self.openvmm_source,
            "later",
            "later\n",
            "later OpenVMM",
        )
        _git(self.openvmm_source, "checkout", "--quiet", self.openvmm_pin)

        self.path = root / "nvx"
        self.path.mkdir()
        _git(self.path, "init", "--quiet")
        _configure(self.path)
        (self.path / "README").write_text("nvx\n", encoding="utf-8")
        (self.path / "conflict.txt").write_text("base\n", encoding="utf-8")
        _git(self.path, "add", "README", "conflict.txt")
        _git(self.path, "commit", "--quiet", "-m", "initial NVX")
        _git(
            self.path,
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            str(self.openvmm_source),
            "openvmm",
        )
        _git(self.path, "commit", "--quiet", "-am", "add OpenVMM")
        _git(self.path, "branch", "-M", "dev")
        self.target = _git(self.path, "rev-parse", "HEAD")
        _git(self.path, "update-ref", "refs/remotes/origin/dev", self.target)
        _git(self.path, "switch", "--quiet", "-c", "agents/rebase-test")
        (self.path / "conflict.txt").write_text("topic\n", encoding="utf-8")
        (self.path / "topic.txt").write_text("topic\n", encoding="utf-8")
        _git(self.path, "add", "conflict.txt", "topic.txt")
        _git(self.path, "commit", "--quiet", "-m", "topic change")
        self.topic = _git(self.path, "rev-parse", "HEAD")


class RebaseCheckTests(unittest.TestCase):
    root: Path
    repository: RebaseRepository
    state: Path

    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.repository = RebaseRepository(self.root)
        self.state = self.root / "rebase-state.json"

    def record(self) -> rebase_check.RecordResult:
        return rebase_check.record_rebase(
            self.repository.path,
            state_file=self.state,
        )

    def verify(self) -> rebase_check.VerifyResult:
        return rebase_check.verify_rebase(
            self.repository.path,
            state_file=self.state,
        )

    def configure_ssh_signing(self, *, allow_signer: bool = False) -> None:
        ssh_keygen = shutil.which("ssh-keygen")
        if ssh_keygen is None:
            self.skipTest("ssh-keygen is unavailable")
        key = self.root / "test-signing-key"
        subprocess.run(
            [
                ssh_keygen,
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "test",
                "-f",
                str(key),
            ],
            capture_output=True,
            check=True,
        )
        public_key = Path(f"{key}.pub")
        repo = self.repository.path
        _git(repo, "config", "gpg.format", "ssh")
        _git(repo, "config", "user.signingkey", str(public_key))
        _git(repo, "config", "user.email", "t@example.com")
        if allow_signer:
            allowed = self.root / "allowed-signers"
            allowed.write_text(
                f"t@example.com {public_key.read_text(encoding='utf-8').strip()}\n",
                encoding="utf-8",
            )
            _git(repo, "config", "gpg.ssh.allowedSignersFile", str(allowed))

    def prepare_signed_rebase(self) -> None:
        self.configure_ssh_signing()
        repo = self.repository.path
        _git(repo, "commit", "--quiet", "--amend", "--no-edit", "-S")
        _git(repo, "switch", "--quiet", "dev")
        upstream = _commit(repo, "upstream", "upstream\n", "upstream change")
        _git(repo, "update-ref", "refs/remotes/origin/dev", upstream)
        _git(repo, "switch", "--quiet", "agents/rebase-test")

    def test_record_saves_patch_signatures_gitlink_and_submodule_state(self) -> None:
        result = self.record()
        self.assertEqual(result.branch, "agents/rebase-test")
        self.assertEqual(result.target_commit, self.repository.target)
        self.assertEqual(len(result.commits), 1)
        self.assertIsNotNone(result.commits[0].patch_id)
        state = json.loads(self.state.read_text(encoding="utf-8"))
        self.assertEqual(state["commits"][0]["signature"], "N")
        self.assertFalse(state["commits"][0]["signed"])
        self.assertEqual(state["commits"][0]["signature_state"], "unsigned")
        self.assertFalse(state["commit_gpgsign"])
        self.assertEqual(state["gitlink"], self.repository.openvmm_pin)
        self.assertFalse(state["openvmm_dirty"])
        self.assertFalse(state["openvmm_off_pin"])
        self.assertFalse(state["rebase_in_progress"])

    def test_verify_accepts_an_unchanged_equivalent_branch(self) -> None:
        self.record()
        result = self.verify()
        self.assertTrue(result.ok)
        self.assertEqual(result.problems, [])
        self.assertIn("--force-with-lease", result.note)

    def test_verify_detects_conflict_markers_in_changed_files(self) -> None:
        self.record()
        (self.repository.path / "topic.txt").write_text(
            "before\n<<<<<<< ours\n=======\n>>>>>>> theirs\n",
            encoding="utf-8",
        )
        result = self.verify()
        self.assertFalse(result.ok)
        self.assertTrue(
            any("conflict markers" in problem for problem in result.problems)
        )

    def test_verify_detects_unmerged_paths(self) -> None:
        self.record()
        repo = self.repository.path
        _git(repo, "switch", "--quiet", "-c", "other", "origin/dev")
        _commit(repo, "conflict.txt", "other\n", "other change")
        _git(repo, "switch", "--quiet", "agents/rebase-test")
        merge = subprocess.run(
            ["git", "-C", str(repo), "merge", "other"],
            capture_output=True,
            check=False,
        )
        self.assertNotEqual(merge.returncode, 0)
        result = self.verify()
        self.assertFalse(result.ok)
        self.assertTrue(any("unmerged paths" in problem for problem in result.problems))

    def test_verify_detects_when_the_target_is_not_an_ancestor(self) -> None:
        self.record()
        repo = self.repository.path
        tree = _git(repo, "rev-parse", "origin/dev^{tree}")
        parent = _git(repo, "rev-parse", "origin/dev^")
        created = subprocess.run(
            ["git", "-C", str(repo), "commit-tree", tree, "-p", parent],
            input="divergent history\n",
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=True,
        ).stdout.strip()
        _git(repo, "reset", "--hard", created)
        result = self.verify()
        self.assertFalse(result.ok)
        self.assertTrue(
            any("not an ancestor" in problem for problem in result.problems)
        )

    def test_verify_detects_a_lost_commit_signature(self) -> None:
        self.prepare_signed_rebase()
        recorded = self.record()
        self.assertTrue(recorded.commits[0].signed)
        self.assertEqual(recorded.commits[0].signature_state, "unverified")
        _git(
            self.repository.path,
            "-c",
            "commit.gpgsign=false",
            "rebase",
            "origin/dev",
        )
        result = self.verify()
        self.assertFalse(result.ok)
        self.assertTrue(
            any("unsigned commit" in problem for problem in result.problems)
        )

    def test_verify_accepts_a_rebase_that_preserves_ssh_signatures(self) -> None:
        self.prepare_signed_rebase()
        self.record()
        _git(
            self.repository.path,
            "-c",
            "commit.gpgsign=true",
            "rebase",
            "origin/dev",
        )
        result = self.verify()
        self.assertTrue(result.ok)
        self.assertTrue(
            any("allowedSignersFile" in warning for warning in result.warnings)
        )

    def test_record_recognizes_a_good_ssh_signature(self) -> None:
        self.configure_ssh_signing(allow_signer=True)
        _git(
            self.repository.path,
            "commit",
            "--quiet",
            "--amend",
            "--no-edit",
            "-S",
        )
        result = self.record()
        self.assertTrue(result.commits[0].signed)
        self.assertEqual(result.commits[0].signature_state, "good")
        self.assertEqual(result.commits[0].signature, "G")

    def test_verify_requires_signatures_when_commit_signing_was_enabled(self) -> None:
        _git(self.repository.path, "config", "commit.gpgsign", "true")
        recorded = self.record()
        self.assertTrue(recorded.commit_gpgsign)
        result = self.verify()
        self.assertFalse(result.ok)
        self.assertTrue(
            any("commit.gpgsign was true" in problem for problem in result.problems)
        )

    def test_verify_detects_an_openvmm_gitlink_mismatch(self) -> None:
        self.record()
        _git(
            self.repository.path / "openvmm",
            "checkout",
            "--quiet",
            self.repository.openvmm_later,
        )
        result = self.verify()
        self.assertFalse(result.ok)
        mismatch = next(
            problem for problem in result.problems if "OpenVMM gitlink" in problem
        )
        self.assertIn("nvx.py verify fails", mismatch)
        self.assertIn("user's approval", mismatch)


if __name__ == "__main__":
    unittest.main()

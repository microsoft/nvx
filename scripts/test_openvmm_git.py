#!/usr/bin/env python3
"""Tests of the Git and roadmap helpers of the OpenVMM tools."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import openvmm_git  # noqa: E402
from openvmm_git import GitError  # noqa: E402

SCOPE = """# OpenVMM Micro-VM PR Strategy

## Scope

- OpenVMM fork base: `{base}`, upstream main on 2026-11-05.
- Mapped fork tip: `{tip}`, position 2. The graph covers positions 001-002.
- NVX's pinned OpenVMM revision: `{pin}`, position 3.
- Upstream main: `{base}` (2026-11-05), the fork base itself.

## Upstream status

- OpenVMM fork base: `{other}`, which a later section quotes.
"""


def _git(repo: Path, *args: str, check: bool = True) -> str:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        check=check,
    ).stdout.strip()


def _ssh_key(path: Path) -> Path:
    """Generate an Ed25519 key pair at path, and return its public key."""
    subprocess.run(
        ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "t", "-f", str(path)],
        capture_output=True,
        check=True,
    )
    return path.with_suffix(".pub")


class Repository:
    """A scratch repository whose commits are unsigned and need no identity."""

    def __init__(self, path: Path) -> None:
        self.path = path
        path.mkdir()
        _git(path, "init", "--quiet")
        for key, value in (
            ("user.name", "NVX Test"),
            ("user.email", "nvx@example.com"),
            ("commit.gpgsign", "false"),
        ):
            _git(path, "config", key, value)

    def commit(self, name: str) -> str:
        (self.path / name).write_text(name, encoding="utf-8")
        _git(self.path, "add", name)
        _git(self.path, "commit", "--quiet", "-m", f"add {name}")
        return _git(self.path, "rev-parse", "HEAD")


class GitHubRepositoryTests(unittest.TestCase):
    def test_remote_urls_name_their_repository(self):
        for url, repository in (
            ("https://github.com/microsoft/openvmm.git", "microsoft/openvmm"),
            ("https://github.com/microsoft/openvmm", "microsoft/openvmm"),
            ("https://token@github.com/Microsoft/OpenVMM.git/", "microsoft/openvmm"),
            ("git@github.com:microsoft/nvx", "microsoft/nvx"),
            ("git@github.com:nanvix/openvmm.git", "nanvix/openvmm"),
            ("ssh://git@github.com/microsoft/nvx.git", "microsoft/nvx"),
            ("ssh://git@github.com:22/microsoft/nvx.git", "microsoft/nvx"),
            ("work-github:microsoft/nvx", None),
            ("https://example.com/microsoft/nvx.git", None),
            ("/srv/mirrors/openvmm.git", None),
        ):
            with self.subTest(url=url):
                self.assertEqual(openvmm_git.github_repository(url), repository)

    def test_redacting_a_url_drops_the_credentials_before_its_host(self):
        for url, redacted in (
            (
                "https://TOKEN@github.com/example/openvmm.git",
                "https://github.com/example/openvmm.git",
            ),
            (
                "https://user:p@ss@github.com/example/nvx",
                "https://github.com/example/nvx",
            ),
            (
                "ssh://git@github.com:22/microsoft/nvx.git",
                "ssh://github.com:22/microsoft/nvx.git",
            ),
            ("https://github.com/example/a@b", "https://github.com/example/a@b"),
            # The scp-like syntax can name a user, but never a password.
            ("git@github.com:microsoft/nvx", "git@github.com:microsoft/nvx"),
            ("/srv/mirrors/openvmm.git", "/srv/mirrors/openvmm.git"),
        ):
            with self.subTest(url=url):
                self.assertEqual(openvmm_git.redact_url(url), redacted)


class ScopeTests(unittest.TestCase):
    def test_the_scope_names_the_fork_base_mapped_tip_and_pin(self):
        base, tip, pin, other = ("a" * 40, "b" * 40, "c" * 40, "d" * 40)
        scope = openvmm_git.read_scope(
            SCOPE.format(base=base, tip=tip, pin=pin, other=other)
        )
        self.assertEqual(
            scope,
            openvmm_git.RoadmapScope(
                fork_base=base,
                mapped_tip=tip,
                mapped_tip_position=2,
                pin=pin,
                pin_position=3,
                upstream_main=base,
            ),
        )

    def test_a_scope_without_a_field_fails(self):
        text = SCOPE.format(base="a" * 40, tip="b" * 40, pin="c" * 40, other="d" * 40)
        for line, field in (
            ("- Mapped fork tip:", "mapped_tip"),
            ("- NVX's pinned OpenVMM revision:", "pin"),
        ):
            with self.subTest(field=field):
                broken = text.replace(line, "- Former line:")
                with self.assertRaisesRegex(GitError, field):
                    openvmm_git.read_scope(broken)
        with self.assertRaisesRegex(GitError, "no Scope section"):
            openvmm_git.read_scope("# Roadmap\n\n## Upstream status\n")

    def test_the_repository_roadmap_has_a_complete_scope(self):
        scope = openvmm_git.read_scope(openvmm_git.ROADMAP.read_text(encoding="utf-8"))
        self.assertLess(scope.mapped_tip_position, scope.pin_position)


class HistoryTests(unittest.TestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.repository = Repository(self.root / "openvmm")

    def test_positions_count_from_the_fork_base(self):
        base = self.repository.commit("base")
        first = self.repository.commit("first")
        second = self.repository.commit("second")
        path = self.repository.path
        self.assertEqual(openvmm_git.position(path, base, second), 2)
        self.assertEqual(openvmm_git.chain(path, base, second), [first, second])
        self.assertEqual(openvmm_git.count(path, f"{first}..{second}"), 1)
        self.assertEqual(openvmm_git.resolve_commit(path, "HEAD~1"), first)
        with self.assertRaisesRegex(GitError, "not a commit"):
            openvmm_git.resolve_commit(path, "no-such-branch")
        self.assertTrue(
            openvmm_git.git_succeeds(path, "merge-base", "--is-ancestor", base, second)
        )
        self.assertFalse(
            openvmm_git.git_succeeds(path, "merge-base", "--is-ancestor", second, base)
        )

    def test_a_chain_must_descend_from_its_fork_base(self):
        base = self.repository.commit("base")
        first = self.repository.commit("first")
        path = self.repository.path
        _git(path, "switch", "--quiet", "-c", "upstream", base)
        upstream = self.repository.commit("upstream")
        # fork_base..head lists the chain's commits for an upstream main that
        # the chain was never replayed onto, and nothing for a fork base past
        # the head.
        for case, fork_base, head in (
            ("not replayed", upstream, first),
            ("past the head", first, base),
        ):
            with self.subTest(case=case):
                with self.assertRaisesRegex(GitError, "does not descend"):
                    openvmm_git.chain(path, fork_base, head)
                with self.assertRaisesRegex(GitError, "does not descend"):
                    openvmm_git.position(path, fork_base, head)
        self.assertEqual(openvmm_git.chain(path, first, first), [])

    def test_a_shallow_clone_cannot_count_the_chain(self):
        for name in ("base", "first", "second"):
            self.repository.commit(name)
        shallow = self.root / "shallow"
        subprocess.run(
            [
                "git",
                "clone",
                "--quiet",
                "--depth",
                "1",
                self.repository.path.resolve().as_uri(),
                str(shallow),
            ],
            capture_output=True,
            check=True,
        )
        openvmm_git.require_complete_history(self.repository.path)
        with self.assertRaisesRegex(GitError, "fetch --unshallow"):
            openvmm_git.require_complete_history(shallow)

    def test_a_remote_must_fetch_from_and_push_to_the_repository(self):
        path = self.repository.path
        _git(path, "remote", "add", "origin", "https://github.com/microsoft/openvmm")
        openvmm_git.require_remote(path, "origin", "microsoft/openvmm")
        with self.assertRaisesRegex(GitError, "microsoft/nvx"):
            openvmm_git.require_remote(path, "origin", "microsoft/nvx")
        _git(path, "remote", "set-url", "--push", "origin", "git@github.com:me/fork")
        with self.assertRaisesRegex(GitError, "git@github.com:me/fork"):
            openvmm_git.require_remote(path, "origin", "microsoft/openvmm")
        with self.assertRaises(GitError):
            openvmm_git.require_remote(path, "upstream", "microsoft/openvmm")

    def test_a_rejected_remote_url_hides_its_credentials(self):
        path = self.repository.path
        _git(path, "remote", "add", "origin", "https://TOKEN@github.com/me/openvmm")
        with self.assertRaises(GitError) as raised:
            openvmm_git.require_remote(path, "origin", "microsoft/openvmm")
        self.assertIn("https://github.com/me/openvmm", str(raised.exception))
        self.assertNotIn("TOKEN", str(raised.exception))


class SignatureTests(unittest.TestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        configuration = self.root / "gitconfig"
        configuration.write_bytes(b"")
        # Hide the host's Git configuration, such as a global
        # gpg.ssh.allowedSignersFile, which decides what git reports for the
        # signatures of the fixtures.
        environment = patch.dict(
            os.environ,
            {"GIT_CONFIG_GLOBAL": str(configuration), "GIT_CONFIG_NOSYSTEM": "1"},
        )
        environment.start()
        self.addCleanup(environment.stop)
        self.repository = Repository(self.root / "openvmm")

    def test_an_unsigned_commit_reads_unsigned(self):
        commit = self.repository.commit("plain")
        [signature] = openvmm_git.signatures(self.repository.path, [commit])
        self.assertEqual(signature, openvmm_git.Signature(commit, "N", False))
        self.assertEqual(signature.state, "unsigned")
        with self.assertRaisesRegex(GitError, "not a commit"):
            openvmm_git.signatures(self.repository.path, ["0" * 40])

    def test_states_follow_the_letters_of_signed_commits(self):
        for code, state in (
            ("G", "good"),
            ("U", "unverified"),
            ("B", "bad"),
            ("R", "bad"),
            ("X", "expired"),
            ("Y", "expired"),
            ("E", "unverified"),
            ("N", "unverified"),
        ):
            with self.subTest(code=code):
                self.assertEqual(openvmm_git.Signature("a", code, True).state, state)

    @unittest.skipUnless(shutil.which("ssh-keygen"), "requires ssh-keygen")
    def test_a_signature_that_git_cannot_verify_is_still_a_signature(self):
        # Without gpg.ssh.allowedSignersFile, git reports N for an SSH-signed
        # commit, as it did for every commit of the OpenVMM chain.
        public_key = _ssh_key(self.root / "key")
        path = self.repository.path
        _git(path, "config", "gpg.format", "ssh")
        _git(path, "config", "user.signingkey", str(public_key))
        plain = self.repository.commit("plain")
        (path / "signed").write_text("signed", encoding="utf-8")
        _git(path, "add", "signed")
        _git(path, "commit", "--quiet", "-S", "-m", "add signed")
        signed = _git(path, "rev-parse", "HEAD")

        states = [
            signature.state
            for signature in openvmm_git.signatures(path, [signed, plain])
        ]
        self.assertEqual(states, ["unverified", "unsigned"])
        self.assertIn("allowedSignersFile", openvmm_git.verification_hint(path))

        signers = self.root / "allowed_signers"
        signers.write_text(
            "nvx@example.com " + public_key.read_text(encoding="utf-8"),
            encoding="utf-8",
        )
        _git(path, "config", "gpg.ssh.allowedSignersFile", str(signers))
        [signature] = openvmm_git.signatures(path, [signed])
        self.assertEqual(signature.state, "good")

    @unittest.skipUnless(shutil.which("ssh-keygen"), "requires ssh-keygen")
    def test_a_key_that_no_allowed_signer_lists_is_unverified(self):
        # git reports U when gpg.ssh.allowedSignersFile lists none of the
        # signing key's principals, which a signature by any key earns.
        public_key = _ssh_key(self.root / "key")
        signers = self.root / "allowed_signers"
        signers.write_text(
            "nvx@example.com "
            + _ssh_key(self.root / "other").read_text(encoding="utf-8"),
            encoding="utf-8",
        )
        path = self.repository.path
        _git(path, "config", "gpg.format", "ssh")
        _git(path, "config", "user.signingkey", str(public_key))
        _git(path, "config", "gpg.ssh.allowedSignersFile", str(signers))
        (path / "signed").write_text("signed", encoding="utf-8")
        _git(path, "add", "signed")
        _git(path, "commit", "--quiet", "-S", "-m", "add signed")
        signed = _git(path, "rev-parse", "HEAD")

        [signature] = openvmm_git.signatures(path, [signed])
        self.assertEqual(signature, openvmm_git.Signature(signed, "U", True))
        self.assertEqual(signature.state, "unverified")
        self.assertIn("gpg.ssh.allowedSignersFile", openvmm_git.verification_hint(path))

    @unittest.skipUnless(shutil.which("ssh-keygen"), "requires ssh-keygen")
    def test_the_hint_follows_the_signature_rather_than_gpg_format(self):
        # Git verifies a signature by its own type, and gpg.format only says
        # how this host signs, so an SSH signature needs
        # gpg.ssh.allowedSignersFile on a host that signs with OpenPGP too.
        path = self.repository.path
        _git(path, "config", "gpg.format", "ssh")
        _git(path, "config", "user.signingkey", str(_ssh_key(self.root / "key")))
        (path / "signed").write_text("signed", encoding="utf-8")
        _git(path, "add", "signed")
        _git(path, "commit", "--quiet", "-S", "-m", "add signed")
        signed = _git(path, "rev-parse", "HEAD")
        _git(path, "config", "--unset", "gpg.format")

        [signature] = openvmm_git.signatures(path, [signed])
        self.assertEqual(signature.state, "unverified")
        self.assertIn("gpg.ssh.allowedSignersFile", openvmm_git.verification_hint(path))


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Tests for the OpenVMM pin inspection and validation tool."""

from __future__ import annotations

import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import openvmm_pin  # noqa: E402


def _git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if result.returncode != 0:
        raise AssertionError(
            f"git {' '.join(args)} failed in {repo}: "
            f"{result.stderr.strip() or result.stdout.strip()}"
        )
    return result.stdout.strip()


class Repository:
    """A temporary Git repository with deterministic unsigned commits."""

    def __init__(self, path: Path, *, bare: bool = False) -> None:
        self.path = path
        path.mkdir()
        arguments = ["git", "-C", str(path), "init", "--quiet"]
        if bare:
            arguments.append("--bare")
        subprocess.run(arguments, capture_output=True, check=True)
        if not bare:
            for key, value in (
                ("user.name", "NVX Test"),
                ("user.email", "nvx@example.com"),
                ("commit.gpgsign", "false"),
            ):
                _git(path, "config", key, value)

    @classmethod
    def existing(cls, path: Path) -> Repository:
        repository = cls.__new__(cls)
        repository.path = path
        return repository

    def write(self, name: str, content: str) -> None:
        destination = self.path / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(content, encoding="utf-8")

    def commit(self, message: str, files: dict[str, str]) -> str:
        for name, content in files.items():
            self.write(name, content)
            _git(self.path, "add", name)
        _git(self.path, "commit", "--quiet", "-m", message)
        return _git(self.path, "rev-parse", "HEAD")

    def commit_index(self, message: str) -> str:
        _git(self.path, "commit", "--quiet", "-m", message)
        return _git(self.path, "rev-parse", "HEAD")

    def detach(self, commit: str) -> None:
        _git(self.path, "switch", "--quiet", "--detach", commit)

    def branch(self, name: str, commit: str) -> None:
        _git(self.path, "branch", "-f", name, commit)

    def set_gitlink(self, commit: str) -> None:
        _git(
            self.path,
            "update-index",
            "--add",
            "--cacheinfo",
            f"160000,{commit},openvmm",
        )


@dataclass(frozen=True)
class Invocation:
    code: int
    stdout: str
    stderr: str

    def json(self) -> dict[str, object]:
        raw: object = json.loads(self.stdout)
        if not isinstance(raw, dict):
            raise AssertionError("CLI output is not a JSON object")
        return cast(dict[str, object], raw)


class FakeRunner:
    """A gh-only runner whose response is supplied by a test."""

    def __init__(
        self,
        payload: dict[str, object] | None = None,
        *,
        returncode: int = 0,
        stderr: str = "",
    ) -> None:
        self.payload = payload or {}
        self.returncode = returncode
        self.stderr = stderr
        self.calls: list[tuple[list[str], Path]] = []

    def run(
        self, args: Sequence[str], *, cwd: Path
    ) -> subprocess.CompletedProcess[str]:
        command = list(args)
        if command[:3] != ["gh", "pr", "view"]:
            raise AssertionError(f"unexpected external command: {command}")
        self.calls.append((command, cwd))
        return subprocess.CompletedProcess(
            command,
            self.returncode,
            json.dumps(self.payload),
            self.stderr,
        )


class FakeSignatures:
    """Fake shared signature facts for unsigned temporary commits."""

    def __init__(
        self,
        *,
        verify_returncode: int = 0,
        verify_stdout: str = "verified",
        verify_stderr: str = "",
        default_code: str = "G",
        default_signed: bool = True,
    ) -> None:
        self.verify_returncode = verify_returncode
        self.verify_stdout = verify_stdout
        self.verify_stderr = verify_stderr
        self.default_code = default_code
        self.default_signed = default_signed
        self.states: dict[str, tuple[str, bool]] = {}
        self.verify_calls: list[str] = []

    def inspect(self, repo: Path, commits: list[str]) -> list[openvmm_pin.Signature]:
        del repo
        return [
            openvmm_pin.Signature(
                commit,
                self.states.get(commit, (self.default_code, self.default_signed))[0],
                self.states.get(commit, (self.default_code, self.default_signed))[1],
            )
            for commit in commits
        ]

    def verify_commit(
        self, repo: Path, commit: str
    ) -> subprocess.CompletedProcess[str]:
        self.verify_calls.append(commit)
        return subprocess.CompletedProcess(
            ["git", "-C", str(repo), "verify-commit", commit],
            self.verify_returncode,
            self.verify_stdout,
            self.verify_stderr,
        )

    def verification_hint(self, repo: Path) -> str:
        del repo
        return "configure the test signer trust"


def _invoke(
    arguments: Sequence[str],
    *,
    runner: openvmm_pin.CommandRunner | None = None,
    signatures: openvmm_pin.SignatureQueries | None = None,
) -> Invocation:
    stdout = io.StringIO()
    stderr = io.StringIO()
    code = openvmm_pin.run(
        arguments,
        runner=runner or FakeRunner(),
        signatures=signatures or FakeSignatures(),
        stdout=stdout,
        stderr=stderr,
    )
    return Invocation(code, stdout.getvalue(), stderr.getvalue())


class PromotionFixture:
    """OpenVMM chain, upstream branches, NVX superproject, and bare remotes."""

    root: Path
    openvmm: Repository
    nvx: Repository
    base: str
    mapped: str
    previous: str
    first_new: str
    revision: str
    upstream: str
    conflicting_upstream: str
    conflicting_revision: str
    stale_revision: str
    rebased_revision: str
    merge_revision: str
    identical_tree: str
    old_patch: str
    new_patch: str
    changed_patch: str
    target: str
    roadmap: Path

    def __init__(self, root: Path) -> None:
        self.root = root
        self.openvmm = Repository(root / "openvmm")
        self.base = self.openvmm.commit(
            "base",
            {
                "base.txt": "base\n",
                "conflict.txt": "base\n",
                "equiv.txt": "one\ntwo\nthree\nfour\n",
            },
        )
        self.mapped = self.openvmm.commit("mapped change", {"history.txt": "mapped\n"})
        self.previous = self.openvmm.commit(
            "previous pin", {"previous.txt": "previous\n"}
        )
        self.first_new = self.openvmm.commit(
            "revise shared history", {"history.txt": "candidate\n"}
        )
        self.revision = self.openvmm.commit(
            "add final feature", {"feature.txt": "feature\n"}
        )
        self.openvmm.branch("candidate", self.revision)

        self.openvmm.detach(self.base)
        self.upstream = self.openvmm.commit(
            "upstream main",
            {
                "history.txt": "mapped\n",
                "upstream.txt": "upstream\n",
            },
        )
        self.openvmm.branch("upstream-main", self.upstream)
        self.conflicting_upstream = self.openvmm.commit(
            "upstream conflict", {"conflict.txt": "upstream\n"}
        )
        self.openvmm.branch("conflicting-upstream", self.conflicting_upstream)

        self.openvmm.detach(self.previous)
        self.conflicting_revision = self.openvmm.commit(
            "candidate conflict", {"conflict.txt": "candidate\n"}
        )
        self.openvmm.branch("conflicting-revision", self.conflicting_revision)

        self.openvmm.detach(self.mapped)
        self.stale_revision = self.openvmm.commit(
            "stale topic", {"stale.txt": "stale\n"}
        )
        self.openvmm.branch("stale", self.stale_revision)

        self.openvmm.detach(self.base)
        self.rebased_revision = self.openvmm.commit(
            "rebased chain", {"rebased.txt": "rebased\n"}
        )
        self.openvmm.branch("rebased", self.rebased_revision)

        self.openvmm.detach(self.first_new)
        side = self.openvmm.commit("side", {"side.txt": "side\n"})
        self.openvmm.branch("side", side)
        self.openvmm.detach(self.revision)
        _git(
            self.openvmm.path,
            "merge",
            "--quiet",
            "--no-ff",
            "side",
            "-m",
            "merge side",
        )
        self.merge_revision = _git(self.openvmm.path, "rev-parse", "HEAD")
        self.openvmm.branch("merge-candidate", self.merge_revision)

        self.openvmm.detach(self.revision)
        _git(self.openvmm.path, "commit", "--quiet", "--allow-empty", "-m", "same")
        self.identical_tree = _git(self.openvmm.path, "rev-parse", "HEAD")

        self.openvmm.detach(self.base)
        self.old_patch = self.openvmm.commit(
            "equivalent patch", {"equiv.txt": "one\ntwo same\nthree\nfour\n"}
        )
        self.openvmm.detach(self.upstream)
        self.new_patch = self.openvmm.commit(
            "equivalent patch", {"equiv.txt": "one\ntwo same\nthree\nfour\n"}
        )
        self.openvmm.detach(self.upstream)
        self.changed_patch = self.openvmm.commit(
            "equivalent patch", {"equiv.txt": "one\ntwo changed\nthree\nfour\n"}
        )
        self.openvmm.detach(self.revision)

        self.nvx = Repository(root / "nvx")
        self.nvx.write(
            ".gitmodules",
            '[submodule "openvmm"]\n'
            "\tpath = openvmm\n"
            "\turl = https://github.com/microsoft/openvmm.git\n",
        )
        _git(self.nvx.path, "add", ".gitmodules")
        self.nvx.set_gitlink(self.previous)
        self.roadmap = self.nvx.path / "doc" / "openvmm-upstream-roadmap.md"
        self.nvx.write(
            "doc/openvmm-upstream-roadmap.md",
            self.roadmap_text(self.previous),
        )
        _git(self.nvx.path, "add", "doc/openvmm-upstream-roadmap.md")
        self.target = self.nvx.commit_index("target")
        self.nvx.branch("target", self.target)

    def clone(self, root: Path) -> PromotionFixture:
        clone = self.__class__.__new__(self.__class__)
        clone.root = root
        clone.openvmm = self.openvmm
        clone.base = self.base
        clone.mapped = self.mapped
        clone.previous = self.previous
        clone.first_new = self.first_new
        clone.revision = self.revision
        clone.upstream = self.upstream
        clone.conflicting_upstream = self.conflicting_upstream
        clone.conflicting_revision = self.conflicting_revision
        clone.stale_revision = self.stale_revision
        clone.rebased_revision = self.rebased_revision
        clone.merge_revision = self.merge_revision
        clone.identical_tree = self.identical_tree
        clone.old_patch = self.old_patch
        clone.new_patch = self.new_patch
        clone.changed_patch = self.changed_patch
        clone.target = self.target
        nvx_path = root / "nvx"
        subprocess.run(
            ["git", "clone", "--quiet", str(self.nvx.path), str(nvx_path)],
            capture_output=True,
            check=True,
        )
        clone.nvx = Repository.existing(nvx_path)
        for key, value in (
            ("user.name", "NVX Test"),
            ("user.email", "nvx@example.com"),
            ("commit.gpgsign", "false"),
        ):
            _git(nvx_path, "config", key, value)
        if not _git(nvx_path, "branch", "--list", "target"):
            _git(nvx_path, "branch", "target", clone.target)
        clone.roadmap = nvx_path / "doc" / "openvmm-upstream-roadmap.md"
        return clone

    @property
    def chain(self) -> list[str]:
        return [self.mapped, self.previous, self.first_new, self.revision]

    def roadmap_text(self, pin: str, *, bad_row: bool = False) -> str:
        pin_position = self.chain.index(pin) + 1
        mapped_short = self.mapped[:9]
        pin_short = pin[:9]
        table_rows: list[str] = []
        for commit_position, commit in enumerate(self.chain[1:pin_position], start=2):
            sha = "deadbeef0" if bad_row and commit_position == 2 else commit[:9]
            subject = _git(self.openvmm.path, "show", "-s", "--format=%s", commit)
            table_rows.append(
                f"| {commit_position:03d} | `{sha}` | - | - | - | {subject} |"
            )
        return (
            "# Roadmap\n\n"
            "## Scope\n\n"
            f"- OpenVMM fork base: `{self.base}`, upstream main.\n"
            f"- Mapped fork tip: `{self.mapped}`, position 1.\n"
            f"- NVX's pinned OpenVMM revision: `{pin}`, position {pin_position}.\n"
            f"- Upstream main: `{self.upstream}`.\n\n"
            "## Upstream status\n\n"
            "| 999 | `deadbeef0` | an older release quotation |\n\n"
            "## Actual branch DAG\n\n"
            "```text\n"
            f"  -> {mapped_short} (mapped tip)\n"
            f"  -> {pin_short} (NVX pin)\n"
            "```\n\n"
            "## Commits after the mapped tip\n\n"
            "| Position | Commit | v4.0 position | v3.0 position | "
            "v2.4 positions | Topic |\n"
            "| ---: | --- | ---: | ---: | --- | --- |\n"
            + "\n".join(table_rows)
            + "\n\n## Exact commit-to-PR mapping\n\n"
            f"- 001 `{self.mapped[:9]}` - mapped change\n"
            "- ~~002 `deadbeef0` - landed entry~~\n"
        )

    def promote(
        self,
        pin: str | None = None,
        *,
        coordinated: dict[str, str] | None = None,
        extra_roadmap_file: dict[str, str] | None = None,
    ) -> tuple[str, str]:
        selected = pin or self.revision
        self.nvx.set_gitlink(selected)
        for path, content in (coordinated or {}).items():
            self.nvx.write(path, content)
            _git(self.nvx.path, "add", path)
        gitlink_commit = self.nvx.commit_index("pin openvmm")
        self.nvx.write("doc/openvmm-upstream-roadmap.md", self.roadmap_text(selected))
        _git(self.nvx.path, "add", "doc/openvmm-upstream-roadmap.md")
        for path, content in (extra_roadmap_file or {}).items():
            self.nvx.write(path, content)
            _git(self.nvx.path, "add", path)
        roadmap_commit = self.nvx.commit_index("map roadmap")
        return gitlink_commit, roadmap_commit

    def remote(self, name: str, head: str) -> tuple[str, Path]:
        bare = Repository(self.root / name, bare=True)
        self.openvmm.branch(f"remote-{name}", head)
        _git(self.openvmm.path, "remote", "add", name, str(bare.path))
        _git(
            self.openvmm.path,
            "push",
            "--quiet",
            name,
            f"{head}:refs/heads/{openvmm_pin.DEV_BRANCH}",
        )
        return name, bare.path


class RealSignatureTests(unittest.TestCase):
    def test_source_distinguishes_unverified_and_trusted_ssh_signatures(self) -> None:
        ssh_keygen = shutil.which("ssh-keygen")
        if ssh_keygen is None:
            self.skipTest("ssh-keygen is unavailable")
        directory = tempfile.TemporaryDirectory(dir=Path(__file__).parent)
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        configuration = root / "gitconfig"
        configuration.write_bytes(b"")
        # Hide the host's Git configuration, such as a global
        # gpg.ssh.allowedSignersFile, which decides what git reports for the
        # signature of the fixture.
        environment = patch.dict(
            os.environ,
            {"GIT_CONFIG_GLOBAL": str(configuration), "GIT_CONFIG_NOSYSTEM": "1"},
        )
        environment.start()
        self.addCleanup(environment.stop)
        repository = Repository(root / "signed")
        key = root / "signing-key"
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
        _git(repository.path, "config", "gpg.format", "ssh")
        _git(repository.path, "config", "user.signingkey", f"{key}.pub")
        repository.write("signed.txt", "signed\n")
        _git(repository.path, "add", "signed.txt")
        _git(repository.path, "commit", "--quiet", "-S", "-m", "signed")
        commit = _git(repository.path, "rev-parse", "HEAD")

        unverified = _invoke(
            ["source", "--openvmm", str(repository.path), commit, "--json"],
            signatures=openvmm_pin.GitSignatureQueries(),
        )
        self.assertEqual(unverified.code, 0, unverified.stderr)
        unverified_signature = cast(dict[str, object], unverified.json()["signature"])
        self.assertEqual(unverified_signature["state"], "unverified")
        self.assertEqual(unverified_signature["output"], "")
        self.assertIn(
            "gpg.ssh.allowedSignersFile",
            cast(str, unverified_signature["verification_hint"]),
        )

        allowed_signers = root / "allowed-signers"
        public_key = Path(f"{key}.pub").read_text(encoding="utf-8").strip()
        allowed_signers.write_text(f"nvx@example.com {public_key}\n", encoding="utf-8")
        _git(
            repository.path,
            "config",
            "gpg.ssh.allowedSignersFile",
            str(allowed_signers),
        )
        verified = _invoke(
            ["source", "--openvmm", str(repository.path), commit, "--json"],
            signatures=openvmm_pin.GitSignatureQueries(),
        )
        self.assertEqual(verified.code, 0, verified.stderr)
        verified_signature = cast(dict[str, object], verified.json()["signature"])
        self.assertEqual(verified_signature["state"], "good")
        self.assertTrue(cast(str, verified_signature["output"]))


class PinCommandTests(unittest.TestCase):
    fixture: PromotionFixture
    template: PromotionFixture

    @classmethod
    def setUpClass(cls) -> None:
        directory = tempfile.TemporaryDirectory(dir=Path(__file__).parent)
        cls.addClassCleanup(directory.cleanup)
        cls.template = PromotionFixture(Path(directory.name))

    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory(dir=Path(__file__).parent)
        self.addCleanup(directory.cleanup)
        self.fixture = self.template.clone(Path(directory.name))

    def common(self, command: str) -> list[str]:
        return [
            command,
            "--openvmm",
            str(self.fixture.openvmm.path),
            "--roadmap",
            str(self.fixture.roadmap),
            "--nvx",
            str(self.fixture.nvx.path),
        ]

    def test_source_reports_metadata_signature_and_pull_request(self) -> None:
        payload: dict[str, object] = {
            "number": 4970,
            "title": "storage identity",
            "state": "OPEN",
            "isDraft": False,
            "baseRefName": openvmm_pin.DEV_BRANCH,
            "headRefName": "users/topic",
            "headRefOid": self.fixture.revision,
            "reviewDecision": "APPROVED",
            "statusCheckRollup": [
                {"conclusion": "SUCCESS", "status": "COMPLETED"},
                {"conclusion": "SUCCESS", "status": "COMPLETED"},
                {"conclusion": "", "status": "IN_PROGRESS"},
            ],
            "url": "https://github.com/microsoft/openvmm/pull/4970",
        }
        runner = FakeRunner(payload)
        result = _invoke(
            [*self.common("source"), self.fixture.revision, "--pr", "4970", "--json"],
            runner=runner,
        )
        self.assertEqual(result.code, 0, result.stderr)
        report = result.json()
        self.assertEqual(report["commit"], self.fixture.revision)
        self.assertEqual(cast(dict[str, object], report["signature"])["status"], "good")
        pull_request = cast(dict[str, object], report["pull_request"])
        self.assertEqual(pull_request["base"], openvmm_pin.DEV_BRANCH)
        self.assertEqual(
            pull_request["checks_by_conclusion"],
            {"IN_PROGRESS": 1, "SUCCESS": 2},
        )
        self.assertEqual(len(runner.calls), 1)

    def test_source_reports_each_non_good_signature_without_failing(self) -> None:
        cases = (
            ("N", False, "unsigned", False),
            ("B", True, "bad", True),
            ("N", True, "unverified", False),
        )
        for code, signed, expected, verifies in cases:
            with self.subTest(expected=expected):
                signatures = FakeSignatures(
                    verify_returncode=1,
                    verify_stdout="BAD signature",
                    default_code=code,
                    default_signed=signed,
                )
                result = _invoke(
                    [*self.common("source"), self.fixture.revision, "--json"],
                    signatures=signatures,
                )
                self.assertEqual(result.code, 0, result.stderr)
                signature = cast(dict[str, object], result.json()["signature"])
                self.assertEqual(signature["state"], expected)
                self.assertEqual(bool(signatures.verify_calls), verifies)
                if expected == "unverified":
                    self.assertIn(
                        "configure the test signer trust",
                        cast(str, signature["verification_hint"]),
                    )

    def test_source_rejects_a_pull_request_at_another_head(self) -> None:
        payload: dict[str, object] = {
            "number": 1,
            "title": "wrong",
            "state": "OPEN",
            "isDraft": True,
            "baseRefName": openvmm_pin.DEV_BRANCH,
            "headRefName": "wrong",
            "headRefOid": self.fixture.previous,
            "reviewDecision": "",
            "statusCheckRollup": [],
            "url": "https://example.invalid/pr/1",
        }
        result = _invoke(
            [*self.common("source"), self.fixture.revision, "--pr", "1"],
            runner=FakeRunner(payload),
        )
        self.assertEqual(result.code, 1)
        self.assertIn("the pull request's head is", result.stderr)
        self.assertIn(self.fixture.revision, result.stderr)
        self.assertEqual(result.stdout, "")

    def test_source_rejects_a_missing_revision_without_a_traceback(self) -> None:
        result = _invoke([*self.common("source"), "not-a-revision"])
        self.assertEqual(result.code, 1)
        self.assertTrue(result.stderr.startswith("error: "))
        self.assertNotIn("Traceback", result.stderr)

    def test_equivalence_distinguishes_trees_patches_and_changed_pairs(self) -> None:
        identical = _invoke(
            [
                *self.common("equivalence"),
                self.fixture.revision,
                self.fixture.identical_tree,
                "--json",
            ]
        )
        self.assertEqual(identical.code, 0, identical.stderr)
        self.assertEqual(identical.json()["status"], "tree-identical")

        no_bases = _invoke(
            [
                *self.common("equivalence"),
                self.fixture.old_patch,
                self.fixture.new_patch,
            ]
        )
        self.assertEqual(no_bases.code, 0, no_bases.stderr)
        self.assertIn("pass both bases", no_bases.stdout)

        equivalent = _invoke(
            [
                *self.common("equivalence"),
                self.fixture.old_patch,
                self.fixture.new_patch,
                "--old-base",
                self.fixture.base,
                "--new-base",
                self.fixture.upstream,
                "--json",
            ]
        )
        self.assertEqual(equivalent.code, 0, equivalent.stderr)
        self.assertEqual(equivalent.json()["status"], "patch-equivalent")
        counts = cast(dict[str, object], equivalent.json()["counts"])
        self.assertEqual(counts["identical"], 1)

        changed = _invoke(
            [
                *self.common("equivalence"),
                self.fixture.old_patch,
                self.fixture.changed_patch,
                "--old-base",
                self.fixture.base,
                "--new-base",
                self.fixture.upstream,
                "--json",
            ]
        )
        self.assertEqual(changed.code, 0, changed.stderr)
        changed_report = changed.json()
        self.assertEqual(changed_report["status"], "differs")
        self.assertEqual(
            cast(dict[str, object], changed_report["counts"])["changed"], 1
        )
        self.assertEqual(len(cast(list[object], changed_report["changed_pairs"])), 1)

    def test_equivalence_requires_both_bases(self) -> None:
        result = _invoke(
            [
                *self.common("equivalence"),
                self.fixture.old_patch,
                self.fixture.new_patch,
                "--old-base",
                self.fixture.base,
            ]
        )
        self.assertEqual(result.code, 1)
        self.assertIn("must be passed together", result.stderr)

    def test_roadmap_reports_positions_files_clean_merge_and_dynamic_rows(self) -> None:
        result = _invoke(
            [
                *self.common("roadmap"),
                self.fixture.revision,
                "--target",
                "target",
                "--pr",
                "4970",
                "--json",
            ]
        )
        self.assertEqual(result.code, 0, result.stderr)
        report = result.json()
        self.assertEqual(report["pin_position"], 4)
        self.assertEqual(report["distance_from_mapped_tip"], 3)
        self.assertEqual(
            cast(dict[str, object], report["upstream_applicability"])["status"],
            "applies cleanly",
        )
        earlier = cast(dict[str, object], report["earlier_commits_by_file"])
        history = cast(list[dict[str, object]], earlier["history.txt"])
        self.assertEqual(history[0]["commit"], self.fixture.mapped)
        rows = cast(str, report["markdown_rows"])
        self.assertIn(
            f"| 003 | `{self.fixture.first_new[:9]}` | - | - | - | "
            "revise shared history (microsoft/openvmm#4970) |",
            rows,
        )
        self.assertIn(
            f"| 004 | `{self.fixture.revision[:9]}` | - | - | - |",
            rows,
        )
        self.assertIn("cargo check -p PACKAGE", cast(str, report["type_check_note"]))

    def test_roadmap_reports_conflicting_merge_tree_files(self) -> None:
        result = _invoke(
            [
                *self.common("roadmap"),
                self.fixture.conflicting_revision,
                "--target",
                "target",
                "--upstream-main",
                self.fixture.conflicting_upstream,
                "--json",
            ]
        )
        self.assertEqual(result.code, 0, result.stderr)
        applicability = cast(dict[str, object], result.json()["upstream_applicability"])
        self.assertEqual(applicability["status"], "conflicts")
        self.assertIn(
            "conflict.txt", cast(list[str], applicability["conflicting_files"])
        )

    def test_roadmap_classifies_a_topic_from_an_older_chain_head(self) -> None:
        result = _invoke(
            [
                *self.common("roadmap"),
                self.fixture.stale_revision,
                "--target",
                "target",
            ]
        )
        self.assertEqual(result.code, 2)
        self.assertIn("branched from an older head", result.stderr)
        self.assertIn("previous pin", result.stderr)

    def test_roadmap_classifies_a_rebased_chain(self) -> None:
        result = _invoke(
            [
                *self.common("roadmap"),
                self.fixture.rebased_revision,
                "--target",
                "target",
            ]
        )
        self.assertEqual(result.code, 2)
        self.assertIn("chain itself was rebased", result.stderr)
        self.assertIn("nvx-rebase-openvmm-chain", result.stderr)

    def test_roadmap_rejects_merges_in_the_chain(self) -> None:
        result = _invoke(
            [
                *self.common("roadmap"),
                self.fixture.merge_revision,
                "--target",
                "target",
            ]
        )
        self.assertEqual(result.code, 2)
        self.assertIn("merge commit", result.stderr)

    def test_check_branch_accepts_a_signed_two_commit_promotion(self) -> None:
        _, roadmap_commit = self.fixture.promote(
            coordinated={"nvx-rs/allowed.txt": "allowed\n"}
        )
        result = _invoke([*self.common("check-branch"), "--target", "target", "--json"])
        self.assertEqual(result.code, 0, result.stderr)
        report = result.json()
        self.assertEqual(report["head"], roadmap_commit)
        self.assertEqual(report["mode"], "standalone")
        self.assertEqual(report["coordinated_files"], ["nvx-rs/allowed.txt"])

    def test_check_branch_allows_signed_but_unverified_commits(self) -> None:
        self.fixture.promote()
        signatures = FakeSignatures(default_code="N", default_signed=True)
        result = _invoke(
            [*self.common("check-branch"), "--target", "target", "--json"],
            signatures=signatures,
        )
        self.assertEqual(result.code, 0, result.stderr)
        report = result.json()
        self.assertEqual(
            cast(list[str], report["unverified_commits"]),
            cast(list[str], report["commits"]),
        )
        self.assertEqual(report["verified_commits"], [])
        self.assertIn("test signer trust", cast(str, report["verification_hint"]))

    def test_check_branch_rejects_unsigned_bad_and_expired_signatures(self) -> None:
        self.fixture.promote()
        cases = (
            ("N", False, "is unsigned"),
            ("B", True, "has a bad signature"),
            ("X", True, "has an expired signature"),
        )
        for code, signed, expected in cases:
            with self.subTest(expected=expected):
                result = _invoke(
                    [*self.common("check-branch"), "--target", "target"],
                    signatures=FakeSignatures(default_code=code, default_signed=signed),
                )
                self.assertEqual(result.code, 1)
                self.assertIn(expected, result.stderr)

    def test_check_branch_lists_all_structure_signature_and_diff_failures(self) -> None:
        self.fixture.nvx.set_gitlink(self.fixture.revision)
        self.fixture.nvx.write("outside.txt", "bad trailing space   \n")
        _git(self.fixture.nvx.path, "add", "outside.txt")
        self.fixture.nvx.commit_index("bad pin")
        self.fixture.nvx.write(
            "doc/openvmm-upstream-roadmap.md",
            self.fixture.roadmap_text(self.previous_pin_for_mismatch()),
        )
        self.fixture.nvx.write("also.txt", "also\n")
        _git(
            self.fixture.nvx.path,
            "add",
            "doc/openvmm-upstream-roadmap.md",
            "also.txt",
        )
        self.fixture.nvx.commit_index("bad roadmap")
        signatures = FakeSignatures(default_code="N", default_signed=False)
        result = _invoke(
            [*self.common("check-branch"), "--target", "target"],
            signatures=signatures,
        )
        self.assertEqual(result.code, 1)
        self.assertIn("outside the coordinated set: outside.txt", result.stderr)
        self.assertIn("not only doc/openvmm-upstream-roadmap.md", result.stderr)
        self.assertIn("is unsigned", result.stderr)
        self.assertIn("does not match HEAD:openvmm", result.stderr)
        self.assertIn("git diff --check", result.stderr)

    def previous_pin_for_mismatch(self) -> str:
        return self.fixture.previous

    def test_check_branch_topic_allows_other_signed_commits(self) -> None:
        self.fixture.nvx.commit("topic preparation", {"notes.txt": "notes\n"})
        self.fixture.nvx.set_gitlink(self.fixture.revision)
        self.fixture.nvx.commit_index("pin openvmm")
        self.fixture.nvx.write(
            "doc/openvmm-upstream-roadmap.md",
            self.fixture.roadmap_text(self.fixture.revision),
        )
        _git(self.fixture.nvx.path, "add", "doc/openvmm-upstream-roadmap.md")
        self.fixture.nvx.commit_index("map roadmap")
        self.fixture.nvx.commit("topic tail", {"tail.txt": "tail\n"})
        result = _invoke(
            [
                *self.common("check-branch"),
                "--target",
                "target",
                "--topic",
                "--json",
            ]
        )
        self.assertEqual(result.code, 0, result.stderr)
        self.assertEqual(result.json()["mode"], "topic")

    def test_check_roadmap_accepts_only_current_sections_at_a_revision(self) -> None:
        result = _invoke(
            [
                *self.common("check-roadmap"),
                "--revision",
                "target",
                "--json",
            ]
        )
        self.assertEqual(result.code, 0, result.stderr)
        report = result.json()
        self.assertEqual(report["pin"], self.fixture.previous)
        self.assertEqual(report["pin_source"], "target:openvmm")
        self.assertGreater(cast(int, report["references_checked"]), 0)

    def test_check_roadmap_reports_mismatches_with_line_numbers(self) -> None:
        self.fixture.roadmap.write_text(
            self.fixture.roadmap_text(self.fixture.previous, bad_row=True),
            encoding="utf-8",
        )
        result = _invoke([*self.common("check-roadmap"), "--worktree"])
        self.assertEqual(result.code, 1)
        self.assertRegex(result.stderr, r"line \d+: position 002")
        self.assertIn("expected", result.stderr)

    def test_check_roadmap_worktree_prefers_a_staged_gitlink(self) -> None:
        self.fixture.nvx.set_gitlink(self.fixture.first_new)
        self.fixture.roadmap.write_text(
            self.fixture.roadmap_text(self.fixture.first_new), encoding="utf-8"
        )
        result = _invoke([*self.common("check-roadmap"), "--worktree", "--json"])
        self.assertEqual(result.code, 0, result.stderr)
        report = result.json()
        self.assertEqual(report["pin"], self.fixture.first_new)
        self.assertIn("index (:openvmm)", cast(str, report["pin_source"]))

    def test_check_roadmap_ignores_wrong_rows_outside_checked_sections(self) -> None:
        text = self.fixture.roadmap.read_text(encoding="utf-8")
        self.assertIn("| 999 | `deadbeef0`", text)
        result = _invoke([*self.common("check-roadmap"), "--worktree"])
        self.assertEqual(result.code, 0, result.stderr)

    def test_land_reports_an_already_landed_revision(self) -> None:
        remote, _ = self.fixture.remote("already", self.fixture.revision)
        with patch.object(openvmm_pin, "require_remote", return_value=None):
            result = _invoke(
                [
                    *self.common("land"),
                    self.fixture.revision,
                    "--remote",
                    remote,
                    "--json",
                ]
            )
        self.assertEqual(result.code, 0, result.stderr)
        self.assertEqual(result.json()["status"], "already landed")

    def test_land_dry_run_prints_an_exact_non_force_push(self) -> None:
        remote, _ = self.fixture.remote("dry", self.fixture.previous)
        with patch.object(openvmm_pin, "require_remote", return_value=None):
            result = _invoke(
                [
                    *self.common("land"),
                    self.fixture.revision,
                    "--remote",
                    remote,
                ]
            )
        self.assertEqual(result.code, 0, result.stderr)
        self.assertIn(
            f"{self.fixture.revision}:refs/heads/{openvmm_pin.DEV_BRANCH}",
            result.stdout,
        )
        self.assertNotIn("--force", result.stdout)
        observed = _git(
            self.fixture.openvmm.path,
            "ls-remote",
            remote,
            f"refs/heads/{openvmm_pin.DEV_BRANCH}",
        )
        self.assertTrue(observed.startswith(self.fixture.previous))

    def test_land_pushes_and_verifies_the_remote_revision(self) -> None:
        remote, _ = self.fixture.remote("push", self.fixture.previous)
        with patch.object(openvmm_pin, "require_remote", return_value=None):
            result = _invoke(
                [
                    *self.common("land"),
                    self.fixture.revision,
                    "--remote",
                    remote,
                    "--push",
                    "--json",
                ]
            )
        self.assertEqual(result.code, 0, result.stderr)
        report = result.json()
        self.assertEqual(report["status"], "landed")
        self.assertEqual(report["verified_remote_revision"], self.fixture.revision)
        command = cast(list[str], report["push_command"])
        self.assertNotIn("--force", command)

    def test_land_rejects_a_branch_that_is_not_an_ancestor(self) -> None:
        remote, _ = self.fixture.remote("moved", self.fixture.stale_revision)
        with patch.object(openvmm_pin, "require_remote", return_value=None):
            result = _invoke(
                [
                    *self.common("land"),
                    self.fixture.revision,
                    "--remote",
                    remote,
                ]
            )
        self.assertEqual(result.code, 2)
        self.assertIn("branch moved past the pull request's base", result.stderr)
        self.assertIn("rerun OpenVMM CI", result.stderr)


if __name__ == "__main__":
    unittest.main()

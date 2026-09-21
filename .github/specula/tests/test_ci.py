import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import ci  # noqa: E402


class CITests(unittest.TestCase):
    def setUp(self):
        self.config = {
            "specula_binary": "/opt/specula-venv/bin/specula",
            "state_root": "/data/specula",
            "agent_config": "agents.json",
            "target": "snapshot|owner/repo|Rust|scope",
        }
        self.source = Path("/data/specula/source/openvmm")
        self.revision = "a" * 40

    def test_initialize_command_uses_source_guidance_and_target(self):
        command = ci.specula_command(
            self.config, "initialize", self.source, self.revision, None
        )
        self.assertIn("--ci-init", command)
        self.assertIn(f"--artifact={self.source}", command)
        self.assertIn(f"--revision={self.revision}", command)
        self.assertIn(f"--agent-config={ROOT / 'agents.json'}", command)
        self.assertTrue(any(arg.startswith("--guidance=") for arg in command))
        self.assertEqual(command[-1], self.config["target"])

    def test_bare_repository_is_addressed_explicitly(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            (repo / "HEAD").write_text("ref: refs/heads/main\n")
            (repo / "objects").mkdir()
            self.assertEqual(ci.git_repo_args(repo), [f"--git-dir={repo}"])

    def test_dot_path_components_are_not_valid_ids(self):
        self.assertFalse(ci.valid_id("."))
        self.assertFalse(ci.valid_id(".."))
        self.assertTrue(ci.valid_id("run-1"))

    def test_tag_names_must_be_exact_git_refs(self):
        self.assertTrue(ci.valid_tag("v1.2.3"))
        self.assertFalse(ci.valid_tag(""))
        self.assertFalse(ci.valid_tag("release:refs/tags/other"))
        self.assertFalse(ci.valid_tag("../release"))

    def test_state_lock_rejects_a_second_owner(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = {"state_root": temporary}
            with ci.state_lock(config):
                with self.assertRaises(ci.CIError):
                    with ci.state_lock(config):
                        pass

    def test_specula_source_rejects_staged_and_untracked_changes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            subprocess.run(["git", "init", "-q", source], check=True)
            subprocess.run(
                ["git", "-C", source, "config", "user.email", "test@example.com"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", source, "config", "user.name", "Test"], check=True
            )
            tracked = source / "tracked.txt"
            tracked.write_text("clean\n")
            subprocess.run(["git", "-C", source, "add", "tracked.txt"], check=True)
            subprocess.run(
                ["git", "-C", source, "commit", "-qm", "initial"], check=True
            )
            minimum = ci.git(source, "rev-parse", "HEAD")
            subprocess.run(
                [
                    "git",
                    "-C",
                    source,
                    "remote",
                    "add",
                    "origin",
                    "https://example.test/specula.git",
                ],
                check=True,
            )
            binary = root / "venv/bin/specula"
            binary.parent.mkdir(parents=True)
            binary.write_text("#!/bin/sh\nprintf 'specula 1.2.0\\n'\n")
            binary.chmod(0o755)
            config = {
                "specula_source": str(source),
                "specula_repository": "https://example.test/specula.git",
                "specula_min_commit": minimum,
                "specula_min_version": "1.2.0",
                "specula_max_version_exclusive": "2.0.0",
                "specula_binary": str(binary),
            }
            tracked.write_text("descendant\n")
            subprocess.run(["git", "-C", source, "add", "tracked.txt"], check=True)
            subprocess.run(
                ["git", "-C", source, "commit", "-qm", "descendant"], check=True
            )
            identity = ci.check_specula_source(config)
            self.assertEqual(identity["commit"], ci.git(source, "rev-parse", "HEAD"))
            self.assertEqual(identity["version"], "1.2.0")
            tracked.write_text("staged\n")
            subprocess.run(["git", "-C", source, "add", "tracked.txt"], check=True)
            with self.assertRaises(ci.CIError):
                ci.check_specula_source(config)

    def test_specula_source_rejects_incompatible_version(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            subprocess.run(["git", "init", "-q", source], check=True)
            subprocess.run(
                ["git", "-C", source, "config", "user.email", "test@example.com"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", source, "config", "user.name", "Test"], check=True
            )
            (source / "tracked.txt").write_text("clean\n")
            subprocess.run(["git", "-C", source, "add", "tracked.txt"], check=True)
            subprocess.run(
                ["git", "-C", source, "commit", "-qm", "initial"], check=True
            )
            subprocess.run(
                [
                    "git",
                    "-C",
                    source,
                    "remote",
                    "add",
                    "origin",
                    "https://example.test/specula.git",
                ],
                check=True,
            )
            binary = root / "venv/bin/specula"
            binary.parent.mkdir(parents=True)
            binary.write_text("#!/bin/sh\nprintf 'specula 2.0.0\\n'\n")
            binary.chmod(0o755)
            config = {
                "specula_source": str(source),
                "specula_repository": "https://example.test/specula.git",
                "specula_min_commit": ci.git(source, "rev-parse", "HEAD"),
                "specula_min_version": "1.2.0",
                "specula_max_version_exclusive": "2.0.0",
                "specula_binary": str(binary),
            }
            with self.assertRaisesRegex(ci.CIError, "compatibility range"):
                ci.check_specula_source(config)

    def test_specula_source_rejects_revision_before_minimum(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            subprocess.run(["git", "init", "-q", source], check=True)
            subprocess.run(
                ["git", "-C", source, "config", "user.email", "test@example.com"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", source, "config", "user.name", "Test"], check=True
            )
            tracked = source / "tracked.txt"
            tracked.write_text("old\n")
            subprocess.run(["git", "-C", source, "add", "tracked.txt"], check=True)
            subprocess.run(["git", "-C", source, "commit", "-qm", "old"], check=True)
            old = ci.git(source, "rev-parse", "HEAD")
            tracked.write_text("minimum\n")
            subprocess.run(["git", "-C", source, "add", "tracked.txt"], check=True)
            subprocess.run(
                ["git", "-C", source, "commit", "-qm", "minimum"], check=True
            )
            minimum = ci.git(source, "rev-parse", "HEAD")
            subprocess.run(
                [
                    "git",
                    "-C",
                    source,
                    "remote",
                    "add",
                    "origin",
                    "https://example.test/specula.git",
                ],
                check=True,
            )
            subprocess.run(["git", "-C", source, "checkout", "-q", old], check=True)
            binary = root / "venv/bin/specula"
            binary.parent.mkdir(parents=True)
            binary.write_text("#!/bin/sh\nprintf 'specula 1.2.0\\n'\n")
            binary.chmod(0o755)
            config = {
                "specula_source": str(source),
                "specula_repository": "https://example.test/specula.git",
                "specula_min_commit": minimum,
                "specula_min_version": "1.2.0",
                "specula_max_version_exclusive": "2.0.0",
                "specula_binary": str(binary),
            }
            with self.assertRaisesRegex(ci.CIError, "predates or diverges"):
                ci.check_specula_source(config)
            subprocess.run(["git", "-C", source, "reset", "--hard", "-q"], check=True)
            (source / "untracked.py").write_text("raise SystemExit\n")
            with self.assertRaises(ci.CIError):
                ci.check_specula_source(config)

    def test_publish_report_replaces_previous_attempt(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            report = root / "reports/request-1"
            report.mkdir(parents=True)
            (report / "stale-verdict.json").write_text("{}\n")
            result = ci.publish_report(
                {"state_root": str(root)},
                "request-1",
                "preflight",
                self.revision,
                None,
                0,
                set(),
            )
            self.assertFalse((result / "stale-verdict.json").exists())
            self.assertTrue((result / "result.json").is_file())

    def test_preflight_report_does_not_select_an_old_run(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            old = (
                root
                / "state/openvmm-snapshot-restore/runs/old/pedro-microvm/.specula-output"
            )
            old.mkdir(parents=True)
            (old / "summary.md").write_text("stale verification\n")
            report = ci.publish_report(
                {"state_root": str(root)},
                "preflight-1",
                "preflight",
                self.revision,
                None,
                0,
                set(),
            )
            self.assertNotIn("stale verification", (report / "summary.md").read_text())

    def test_publish_report_rejects_symlinked_run_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runs = root / "state/openvmm-snapshot-restore/runs"
            outside = root / "outside"
            outside.mkdir()
            runs.mkdir(parents=True)
            (runs / "run-1").symlink_to(outside, target_is_directory=True)
            with self.assertRaisesRegex(ci.CIError, "not a real directory"):
                ci.publish_report(
                    {"state_root": str(root)},
                    "request-1",
                    "resume",
                    self.revision,
                    "run-1",
                    0,
                    set(),
                )

    def test_publish_report_rejects_symlinked_curated_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = (
                root
                / "state/openvmm-snapshot-restore/runs/run-1/target/.specula-output"
            )
            output.mkdir(parents=True)
            secret = root / "secret"
            secret.write_text("do not upload\n")
            (output / "summary.md").symlink_to(secret)
            with self.assertRaisesRegex(ci.CIError, "unsafe curated report file"):
                ci.publish_report(
                    {"state_root": str(root)},
                    "request-1",
                    "resume",
                    self.revision,
                    "run-1",
                    0,
                    set(),
                )

    def test_incremental_command_reuses_native_ci_state(self):
        command = ci.specula_command(
            self.config, "incremental", self.source, self.revision, None
        )
        self.assertIn("--incremental", command)
        self.assertNotIn("--ci-init", command)
        self.assertFalse(any(arg.startswith("--guidance=") for arg in command))
        self.assertNotIn(self.config["target"], command)

    def test_resume_only_uses_saved_run_configuration(self):
        command = ci.specula_command(
            self.config, "resume", self.source, self.revision, "run-1"
        )
        self.assertEqual(
            command,
            [
                self.config["specula_binary"],
                "run",
                "--ci-dir=/data/specula/state/openvmm-snapshot-restore",
                "--run-id=run-1",
                f"--revision={self.revision}",
            ],
        )
        with self.assertRaises(ci.CIError):
            ci.specula_command(
                self.config, "resume", self.source, self.revision, "../bad"
            )

    def test_release_event_defaults_to_incremental(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "event.json"
            path.write_text(
                json.dumps(
                    {
                        "repository": {"full_name": "microsoft/nvx"},
                        "action": "published",
                        "release": {"tag_name": "v1", "draft": False},
                    }
                )
            )
            self.assertEqual(
                ci.event_request(path, "release"), ("incremental", "v1", None)
            )

    def test_wrong_repository_event_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "event.json"
            path.write_text(json.dumps({"repository": {"full_name": "other/repo"}}))
            with self.assertRaises(ci.CIError):
                ci.event_request(path, "workflow_dispatch")


if __name__ == "__main__":
    unittest.main()

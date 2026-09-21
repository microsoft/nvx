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

    def test_state_lock_rejects_a_second_owner(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = {"state_root": temporary}
            with ci.state_lock(config):
                with self.assertRaises(ci.CIError):
                    with ci.state_lock(config):
                        pass

    def test_specula_source_rejects_staged_and_untracked_changes(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
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
            config = {
                "specula_source": str(source),
                "specula_commit": ci.git(source, "rev-parse", "HEAD"),
                "specula_binary": str(source / "venv/bin/specula"),
            }
            ci.check_specula_source(config)
            tracked.write_text("staged\n")
            subprocess.run(["git", "-C", source, "add", "tracked.txt"], check=True)
            with self.assertRaises(ci.CIError):
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

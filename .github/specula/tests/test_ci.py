import json
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
            "agent": "copilot-cli",
            "model": "gpt-5.6-sol-fast",
            "effort": "xhigh",
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
        self.assertTrue(any(arg.startswith("--guidance=") for arg in command))
        self.assertEqual(command[-1], self.config["target"])

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

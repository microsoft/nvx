import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

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

    def test_semver_rejects_prereleases(self):
        self.assertEqual(ci.parse_semver("specula 1.2.0+local"), (1, 2, 0))
        with self.assertRaises(ci.CIError):
            ci.parse_semver("specula 1.2.0-alpha")

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
                reset_report=True,
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

    def test_publish_report_rejects_symlinked_runs_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            state = root / "state/openvmm-snapshot-restore"
            outside = root / "outside"
            outside.mkdir()
            state.mkdir(parents=True)
            (state / "runs").symlink_to(outside, target_is_directory=True)
            with self.assertRaisesRegex(
                ci.CIError, "runs path is not a real directory"
            ):
                ci.publish_report(
                    {"state_root": str(root)},
                    "request-1",
                    "resume",
                    self.revision,
                    "run-1",
                    0,
                    set(),
                )

    def test_incomplete_report_can_precede_native_run_discovery(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            report = ci.publish_report(
                {"state_root": str(root)},
                "request-1",
                "initialize",
                self.revision,
                None,
                130,
                set(),
            )
            result = json.loads((report / "result.json").read_text())
            self.assertIsNone(result["run_id"])
            self.assertFalse(result["complete"])

    def test_native_run_discovery_accepts_latest_link(self):
        with tempfile.TemporaryDirectory() as temporary:
            runs = Path(temporary) / "runs"
            selected = runs / "run-1"
            selected.mkdir(parents=True)
            (runs / "latest").symlink_to("run-1", target_is_directory=True)
            self.assertEqual(ci.discover_new_run(runs, set()), "run-1")

    def test_native_run_discovery_tolerates_existing_latest_during_replacement(self):
        with tempfile.TemporaryDirectory() as temporary:
            runs = Path(temporary) / "runs"
            (runs / "old").mkdir(parents=True)
            (runs / "run-1").mkdir()
            (runs / "latest").symlink_to("old", target_is_directory=True)
            self.assertEqual(ci.discover_new_run(runs, {"old", "latest"}), "run-1")

    def test_run_request_publishes_native_run_id_after_launch(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = {**self.config, "state_root": temporary}
            args = SimpleNamespace(
                mode="incremental",
                tag=None,
                revision=self.revision,
                run_id=None,
                event_file=None,
                event_name=None,
                request_id="request-1",
            )

            testcase = self

            class Process:
                returncode = 0

                def __init__(self, command):
                    self.command = command
                    self.polls = 0

                def poll(self):
                    self.polls += 1
                    if self.polls == 1:
                        result_path = Path(temporary) / "reports/request-1/result.json"
                        result = json.loads(result_path.read_text())
                        testcase.assertIsNone(result["run_id"])
                        testcase.assertFalse(result["complete"])
                        run = (
                            Path(temporary)
                            / "state/openvmm-snapshot-restore/runs/run-1"
                        )
                        run.mkdir(parents=True)
                        return None
                    return self.returncode

                def wait(self):
                    return self.returncode

            def launch(command):
                result_path = Path(temporary) / "reports/request-1/result.json"
                self.assertTrue(result_path.exists())
                self.assertNotIn("--run-id=run-1", command)
                return Process(command)

            with (
                mock.patch.object(ci, "prepare_source", return_value=self.source),
                mock.patch.object(
                    ci,
                    "check_runner",
                    return_value={"commit": self.revision, "version": "1.2.0"},
                ),
                mock.patch.object(ci.subprocess, "Popen", side_effect=launch) as popen,
                mock.patch.object(ci.time, "sleep"),
            ):
                self.assertEqual(ci.run_request(config, args), 0)
            popen.assert_called_once()
            result = json.loads(
                (Path(temporary) / "reports/request-1/result.json").read_text()
            )
            self.assertEqual(result["run_id"], "run-1")
            self.assertTrue(result["complete"])

    def test_run_id_survives_curated_report_copy_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = (
                root
                / "state/openvmm-snapshot-restore/runs/run-1/target/.specula-output"
            )
            output.mkdir(parents=True)
            (output / "summary.md").write_text("completed summary\n")
            report = root / "reports/request-1"
            report.mkdir(parents=True)
            (report / "summary.md").write_text("incomplete summary\n")
            (report / "result.json").write_text('{"run_id": null}\n')

            with (
                mock.patch.object(
                    ci, "atomic_copy", side_effect=OSError("interrupted")
                ),
                self.assertRaisesRegex(OSError, "interrupted"),
            ):
                ci.publish_report(
                    {"state_root": str(root)},
                    "request-1",
                    "incremental",
                    self.revision,
                    "run-1",
                    130,
                    set(),
                )

            result = json.loads((report / "result.json").read_text())
            self.assertEqual(result["run_id"], "run-1")
            self.assertFalse(result["complete"])
            self.assertEqual(
                (report / "summary.md").read_text(), "incomplete summary\n"
            )

    def test_callback_failure_is_raised_only_after_specula_is_reaped(self):
        class Process:
            returncode = 0

            def __init__(self):
                self.polls = 0
                self.waited = False

            def poll(self):
                self.polls += 1
                return None if self.polls == 1 else self.returncode

            def wait(self):
                self.waited = True
                return self.returncode

        process = Process()

        def fail(_run_id):
            raise OSError("publication failed")

        with (
            mock.patch.object(ci.subprocess, "Popen", return_value=process),
            mock.patch.object(ci, "discover_new_run", return_value="run-1"),
            mock.patch.object(ci.time, "sleep"),
            self.assertRaisesRegex(OSError, "publication failed"),
        ):
            ci.run_specula(["specula"], Path("/runs"), set(), fail)

        self.assertTrue(process.waited)

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

    def test_new_run_command_rejects_preassigned_id(self):
        with self.assertRaisesRegex(ci.CIError, "only in resume"):
            ci.specula_command(
                self.config, "incremental", self.source, self.revision, "run-1"
            )

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

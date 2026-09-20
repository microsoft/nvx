"""Host contracts for asset reuse and scoped operator consent; no model calls."""

from pathlib import Path
import unittest
from unittest.mock import patch

import release
import test_release


class SeededInitializationTests(unittest.TestCase):
    def setUp(self):
        self.fixture = test_release.ReleaseTests("test_missing_baseline_never_implicitly_initializes")
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.config = self.fixture.config
        self.seed = self.fixture.root / "retained-assets"
        (self.seed / "spec").mkdir(parents=True)
        (self.seed / "harness").mkdir()
        (self.seed / "spec/base.tla").write_text("SYNTHETIC retained model, not verification\n")
        (self.seed / "harness/run.sh").write_text("# SYNTHETIC retained harness, not executable\n")
        manifest = self.fixture.root / "retained-manifest.json"
        release.runtime.write_json(manifest, {"files_sha256": release.bundle_files(self.seed)})
        self.binding = {
            "path": str(self.seed), "manifest": str(manifest),
            "manifest_sha256": release.digest(manifest),
        }
        self.config["initialization_seed"] = self.binding

    def test_seeded_initialization_binds_native_byom_and_exact_mount(self):
        code, _, record = self.fixture.run_request(self.fixture.request("initialize", "v1", "seeded"))
        self.assertEqual(code, 0)
        arguments = next(args for name, args in self.config["_calls"] if name == "native")
        self.assertIn("--ci-init", arguments)
        self.assertIn("--byom=/seed", arguments)
        managed = release.read_json(release.managed_record(Path(self.config["work"]), record["native_run"]))
        self.assertEqual(managed["initialization_seed"], self.binding)
        receipt = release.read_json(Path(managed["runtime_receipt"]))
        self.assertEqual(receipt["seed"], str(self.seed))
        inputs = release.read_json(Path(record["native_work"]) / "ci/runs" / record["native_run"] / "ci-input.json")
        self.assertIn("fresh current-source", inputs["guidance"])

    def test_resume_restores_saved_seed_without_injecting_new_native_options(self):
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        _, _, first = self.fixture.run_request(self.fixture.request("initialize", "v1", "seeded"))
        self.config.pop("initialization_seed")
        self.config.pop("_runtime_status")
        self.config.pop("_exit_code")
        code, _, resumed = self.fixture.run_request(
            self.fixture.request("resume", "v1", "resume-seeded", run_id=first["native_run"]),
        )
        self.assertEqual(code, 0)
        self.assertEqual(resumed["initialization_seed"], self.binding)
        arguments = [args for name, args in self.config["_calls"] if name == "native"][-1]
        self.assertNotIn("--byom=/seed", arguments)
        self.assertIn("--run-id=" + first["native_run"], arguments)
        self.assertEqual(release.read_json(Path(resumed["runtime_receipt"]))["seed"], str(self.seed))

    def test_changed_or_linked_seed_cannot_launch(self):
        model = self.seed / "spec/base.tla"
        model.write_text("changed synthetic seed\n")
        code, _, record = self.fixture.run_request(self.fixture.request("initialize", "v1", "changed"))
        self.assertEqual(code, 1)
        self.assertEqual(record["status"], "invalid_seed")
        self.assertFalse(any(name == "native" for name, _ in self.config["_calls"]))
        model.unlink()
        model.symlink_to(self.seed / "harness/run.sh")
        with self.assertRaises(release.ReleaseError):
            release.initialization_seed(self.config)

    def test_resume_cannot_substitute_seed_and_runtime_mount_must_match(self):
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        _, _, first = self.fixture.run_request(self.fixture.request("initialize", "v1", "seeded"))
        self.config["initialization_seed"] = {**self.binding, "path": str(self.seed / "different")}
        code, _, record = self.fixture.run_request(
            self.fixture.request("resume", "v1", "changed-resume", run_id=first["native_run"]),
        )
        self.assertEqual(code, 1)
        self.assertEqual(record["status"], "resume_identity_changed")
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)

    def test_execution_approval_is_operator_consent_not_a_provider_clearance_claim(self):
        approval = {
            "version": 2, "execution_authorized": True,
            "repository": self.config["repository"], "target": self.config["target"],
            "acknowledged_incident": "synthetic",
            "approval_reference": "SYNTHETIC USER CONSENT FIXTURE",
            "approved_at": "2026-09-20T00:00:00Z", "approved_by": "synthetic operator",
        }
        path = Path(self.config["authorization"])
        release.runtime.write_json(path, approval)
        path.chmod(0o600)
        self.assertIsNone(release.authorization_blocker(self.config))
        self.assertNotIn("provider_authorized", approval)
        hold = Path(self.config["work"]) / "releases/provider-hold.json"
        hold.parent.mkdir(parents=True)
        release.runtime.write_json(hold, {"incident": "a-new-stop"})
        self.assertIsNotNone(release.authorization_blocker(self.config))
        approval["acknowledged_incident"] = "a-new-stop"
        approval["target"] = "another-target"
        release.runtime.write_json(path, approval)
        path.chmod(0o600)
        self.assertIsNotNone(release.authorization_blocker(self.config))

    def test_wrong_seed_mount_is_rejected_during_launch_reconciliation(self):
        original = test_release.FakeBackend.invoke

        def wrong_mount(backend, *args, **kwargs):
            code, record, path = original(backend, *args, **kwargs)
            record["seed"] = "/wrong-seed"
            release.runtime.write_json(Path(path), record)
            return code, record, path

        with patch.object(test_release.FakeBackend, "invoke", wrong_mount):
            code, _, record = self.fixture.run_request(self.fixture.request("initialize", "v1", "wrong-mount"))
        self.assertEqual(code, 1)
        self.assertEqual(record["status"], "invalid_state")
        self.assertFalse((Path(self.config["work"]) / "releases/active-work.json").exists())


if __name__ == "__main__":
    unittest.main()

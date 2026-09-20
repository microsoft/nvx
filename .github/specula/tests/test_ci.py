"""Synthetic CI orchestration only; no live model, VM or TLC execution."""

from pathlib import Path
import unittest
from unittest.mock import patch

import ci
import release
import test_release
import test_seeded_initialization


class CITests(unittest.TestCase):
    def setUp(self):
        self.seeded = test_seeded_initialization.SeededInitializationTests(
            "test_seeded_initialization_binds_native_byom_and_exact_mount",
        )
        self.seeded.setUp()
        self.addCleanup(self.seeded.doCleanups)
        self.fixture = self.seeded.fixture
        self.config = self.fixture.config
        self.config.update(bootstrap_revision=self.fixture.a, timeout_seconds=21600,
                           _calls=[], _currents={}, _publications={})

    def run_ci(self, request=None, blockers=()):
        with patch.object(release, "prerequisites", side_effect=lambda _: list(blockers)):
            return ci.dispatch(self.config, request or self.fixture.request(),
                               backend_factory=test_release.FakeBackend,
                               bundle_directory=self.fixture.control)

    def test_cold_ci_prepares_retained_model_then_runs_requested_incremental(self):
        code, _, record = self.run_ci()
        self.assertEqual(code, 0)
        self.assertTrue(record["complete"])
        self.assertEqual(record["revision"], self.fixture.b)
        self.assertEqual(record["bootstrap_source"], self.fixture.a)
        calls = [args for name, args in self.config["_calls"] if name == "native"]
        self.assertEqual(len(calls), 2)
        self.assertIn("--ci-init", calls[0])
        self.assertIn("--byom=/seed", calls[0])
        self.assertIn("--incremental", calls[1])
        self.assertNotIn("--byom=/seed", calls[1])

    def test_existing_baseline_does_not_initialize_again(self):
        self.fixture.baseline()
        code, _, _ = self.run_ci()
        self.assertEqual(code, 0)
        calls = [args for name, args in self.config["_calls"] if name == "native"]
        self.assertEqual(len(calls), 1)
        self.assertIn("--incremental", calls[0])

    def test_new_dispatch_cannot_replace_incomplete_target_run(self):
        self.fixture.baseline()
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        _, public, original = self.run_ci(self.fixture.request(request_id="gh-first"))
        self.config.pop("_runtime_status")
        self.config.pop("_exit_code")
        code, _, blocked = self.run_ci(self.fixture.request(request_id="gh-second"))
        self.assertNotEqual(code, 0)
        self.assertEqual(blocked["status"], "resume_required")
        self.assertIn(original["native_run"], blocked["error"])
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)
        self.assertEqual(release.read_json(public / "result.json"), original)

    def test_preflight_and_authorization_blockers_do_not_start_bootstrap(self):
        for request, blockers in (
            (self.fixture.request("preflight"), []),
            (self.fixture.request(preflight=True), []),
            (self.fixture.request(), ["operator approval missing"]),
        ):
            with self.subTest(request=request):
                code, _, _ = self.run_ci(request, blockers)
                self.assertNotEqual(code, 0)
                self.assertFalse(any(name == "native" for name, _ in self.config["_calls"]))

    def test_bootstrap_failure_is_not_a_release_verdict_and_same_run_is_resumed(self):
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        code, _, failed = self.run_ci()
        self.assertEqual(code, 1)
        self.assertFalse(failed["requested_release_verified"])
        self.assertEqual(failed["requested_release_revision"], self.fixture.b)
        original_run = failed["native_run"]
        self.config.pop("_runtime_status")
        self.config.pop("_exit_code")
        code, _, completed = self.run_ci()
        self.assertEqual(code, 0)
        self.assertEqual(completed["revision"], self.fixture.b)
        calls = [args for name, args in self.config["_calls"] if name == "native"]
        self.assertIn("--run-id=" + original_run, calls[1])
        self.assertIn("--incremental", calls[2])

    def test_completed_failing_bootstrap_still_allows_incremental_but_preserves_failure(self):
        self.config["_verdict"] = "FAIL"
        code, _, result = self.run_ci()
        self.assertEqual(code, 2)
        self.assertEqual(result["status"], "complete_fail")
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 2)

    def test_future_bootstrap_or_incompatible_existing_base_is_not_forced(self):
        self.config["bootstrap_revision"] = self.fixture.b
        with self.assertRaisesRegex(release.ReleaseError, "not an ancestor"):
            self.run_ci(self.fixture.request(tag="v1"))
        self.fixture.baseline(self.fixture.b)
        code, _, report = self.run_ci(self.fixture.request(tag="v1"))
        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "unsupported_ancestry")
        self.assertFalse(any(name == "native" for name, _ in self.config["_calls"]))

    def test_missing_bootstrap_configuration_preserves_explicit_initialization_contract(self):
        self.config.pop("bootstrap_revision")
        code, _, report = self.run_ci()
        self.assertEqual(code, 3)
        self.assertEqual(report["status"], "needs_initialization")
        self.assertFalse(any(name == "native" for name, _ in self.config["_calls"]))


if __name__ == "__main__":
    unittest.main()

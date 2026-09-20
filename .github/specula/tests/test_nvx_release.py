"""NVX release-to-OpenVMM wiring with disposable local Git repositories."""

import argparse
from pathlib import Path
import unittest
from unittest.mock import patch

import ci
import release
import test_release
import test_seeded_initialization


class NVXReleaseTests(unittest.TestCase):
    def setUp(self):
        self.seeded = test_seeded_initialization.SeededInitializationTests()
        self.seeded.setUp()
        self.addCleanup(self.seeded.doCleanups)
        self.fixture = self.seeded.fixture
        self.config = self.fixture.config
        self.nvx = self.fixture.root / "nvx"
        release.git(None, "init", "--quiet", "--initial-branch=dev", str(self.nvx))
        release.git(self.nvx, "config", "user.name", "Synthetic fixture")
        release.git(self.nvx, "config", "user.email", "fixture@example.invalid")
        self.modules = self.nvx / ".gitmodules"
        self.modules.write_text(
            '[submodule "openvmm"]\n\tpath = openvmm\n'
            f'\turl = {self.config["source_url"]}\n\tbranch = main\n'
        )
        self.nvx_a = self.pin(self.fixture.a, "nvx-v1")
        self.nvx_b = self.pin(self.fixture.b, "nvx-v2")
        self.config.update(
            release_source={"url": str(self.nvx), "trusted_branch": "dev", "submodule": "openvmm"},
            bootstrap_revision=self.fixture.a, timeout_seconds=21600,
            _calls=[], _currents={}, _publications={},
        )

    def pin(self, revision, tag):
        release.git(self.nvx, "add", ".gitmodules")
        release.git(self.nvx, "update-index", "--add", "--cacheinfo", f"160000,{revision},openvmm")
        release.git(self.nvx, "commit", "--quiet", "--allow-empty", "-m", f"Synthetic NVX pin {tag}")
        release.git(self.nvx, "tag", tag)
        return release.git(self.nvx, "rev-parse", "HEAD").stdout.strip()

    def run_ci(self, request):
        with patch.object(release, "prerequisites", side_effect=lambda _: []):
            return ci.dispatch(self.config, request, backend_factory=test_release.FakeBackend,
                               bundle_directory=self.fixture.control)

    def test_nvx_tag_resolves_exact_gitlink_not_openvmm_head(self):
        source, revision, _, selected = release.source_for(
            self.config, self.fixture.request(tag="nvx-v1"))
        self.assertEqual(revision, self.fixture.a)
        self.assertNotEqual(revision, self.fixture.b)
        self.assertEqual(release.git(source, "rev-parse", "HEAD").stdout.strip(), revision)
        self.assertFalse((source / ".gitmodules").exists())
        self.assertEqual(selected, {
            "repository": "microsoft/nvx", "tag": "nvx-v1",
            "revision": self.nvx_a, "submodule": "openvmm",
        })

    def test_cold_ci_bootstraps_openvmm_then_verifies_nvx_pin(self):
        code, public, record = self.run_ci(self.fixture.request(tag="nvx-v2"))
        self.assertEqual(code, 0)
        self.assertTrue(record["complete"])
        self.assertEqual(record["revision"], self.fixture.b)
        self.assertEqual(record["release"]["revision"], self.nvx_b)
        self.assertIn(self.nvx_b, (public / "summary.md").read_text())
        calls = [args for name, args in self.config["_calls"] if name == "native"]
        self.assertEqual(len(calls), 2)
        self.assertIn("--revision=" + self.fixture.a, calls[0])
        self.assertIn("--byom=/seed", calls[0])
        self.assertIn("--revision=" + self.fixture.b, calls[1])
        self.assertIn("--incremental", calls[1])

    def test_bootstrap_failure_keeps_nvx_provenance_and_can_resume(self):
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        _, public, failed = self.run_ci(self.fixture.request(tag="nvx-v2"))
        self.assertFalse(failed["requested_release_verified"])
        self.assertEqual(failed["requested_release"]["revision"], self.nvx_b)
        self.assertIn(self.nvx_b, (public / "summary.md").read_text())
        self.config.pop("_runtime_status")
        self.config.pop("_exit_code")
        code, _, completed = self.run_ci(self.fixture.request(tag="nvx-v2"))
        self.assertEqual(code, 0)
        self.assertEqual(completed["revision"], self.fixture.b)

    def test_moved_nvx_tag_is_rejected(self):
        request = self.fixture.request(tag="nvx-v1")
        release.source_for(self.config, request)
        release.git(self.nvx, "tag", "-f", "nvx-v1", self.nvx_b)
        with self.assertRaises(release.ReleaseError):
            release.source_for(self.config, request)
        cache = self.fixture.root / "repos/nvx-release-cache.git"
        self.assertEqual(release.git(cache, "rev-parse", "refs/tags/nvx-v1").stdout.strip(), self.nvx_a)

    def test_non_gitlink_and_unapproved_url_are_rejected(self):
        release.git(self.nvx, "update-index", "--force-remove", "openvmm")
        (self.nvx / "openvmm").write_text("not a submodule\n")
        release.git(self.nvx, "add", "openvmm")
        release.git(self.nvx, "commit", "--quiet", "-m", "Synthetic invalid pin")
        release.git(self.nvx, "tag", "not-a-gitlink")
        with self.assertRaisesRegex(release.ReleaseError, "does not pin"):
            release.source_for(self.config, self.fixture.request(tag="not-a-gitlink"))
        self.modules.write_text('[submodule "openvmm"]\n\tpath = openvmm\n\turl = unapproved\n')
        self.pin(self.fixture.b, "wrong-origin")
        with self.assertRaisesRegex(release.ReleaseError, "URL differs"):
            release.source_for(self.config, self.fixture.request(tag="wrong-origin"))

    def test_nvx_release_must_be_on_dev_ancestry(self):
        release.git(self.nvx, "switch", "--quiet", "-c", "side", self.nvx_a)
        self.pin(self.fixture.b, "side-release")
        release.git(self.nvx, "switch", "--quiet", "dev")
        with self.assertRaisesRegex(release.ReleaseError, "trusted branch"):
            release.source_for(self.config, self.fixture.request(tag="side-release"))

    def test_same_openvmm_pin_cannot_rebind_request_to_another_nvx_release(self):
        self.fixture.baseline()
        request = self.fixture.request(tag="nvx-v2")
        _, public, original = self.fixture.run_request(request)
        self.pin(self.fixture.b, "nvx-v3")
        code, _, rejected = self.fixture.run_request(self.fixture.request(tag="nvx-v3"))
        self.assertNotEqual(code, 0)
        self.assertEqual(rejected["status"], "request_identity_changed")
        self.assertEqual(release.read_json(public / "result.json"), original)
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)

    def test_incomplete_target_requires_resume_with_original_openvmm_identity(self):
        self.fixture.baseline()
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        _, _, original = self.fixture.run_request(self.fixture.request(tag="nvx-v2"))
        self.config.pop("_runtime_status")
        self.config.pop("_exit_code")
        code, _, blocked = self.run_ci(self.fixture.request(tag="nvx-v2", request_id="another-dispatch"))
        self.assertNotEqual(code, 0)
        self.assertEqual(blocked["status"], "resume_required")
        code, _, resumed = self.fixture.run_request(self.fixture.request(
            "resume", "nvx-v2", "resume-pin", run_id=original["native_run"]))
        self.assertEqual(code, 0)
        self.assertEqual(resumed["revision"], self.fixture.b)
        self.assertEqual(resumed["native_run"], original["native_run"])

    def test_event_repository_is_nvx_not_openvmm(self):
        path = self.fixture.root / "nvx-event.json"
        args = argparse.Namespace(event_file=str(path), event_name="release", mode=None,
                                  tag=None, revision=None, run_id=None, request_id="nvx-gh-1",
                                  preflight_only=False)
        event = {"repository": {"full_name": "microsoft/nvx"}, "action": "published",
                 "release": {"tag_name": "nvx-v2", "draft": False}}
        release.runtime.write_json(path, event)
        self.assertEqual(release.request_from_args(args, self.config).tag, "nvx-v2")
        event["repository"]["full_name"] = "nanvix/openvmm"
        release.runtime.write_json(path, event)
        with self.assertRaisesRegex(release.ReleaseError, "workflow repository"):
            release.request_from_args(args, self.config)

    def test_checked_in_configuration_selects_nvx_releases(self):
        config = release.load_config(Path(release.__file__).parent / "config.json")
        self.assertEqual(config["repository"], "microsoft/nvx")
        self.assertEqual(config["release_source"]["trusted_branch"], "dev")
        self.assertEqual(config["source_url"], "https://github.com/nanvix/openvmm.git")


if __name__ == "__main__":
    unittest.main()

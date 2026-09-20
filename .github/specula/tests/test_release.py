"""Wiring tests only: real disposable Git histories, no Specula/model/VM calls."""

import argparse
import json
import os
from pathlib import Path
import tempfile
import subprocess
import unittest
from unittest.mock import patch

import release


class FakeBackend:
    """Synthetic native receipts confined to each test's disposable data root."""

    def __init__(self, config, bundle):
        self.config, self.bundle = config, bundle
        self.work = Path(config.get("_native_work", config["work"]))
        self.calls = config.setdefault("_calls", [])

    def store(self, operation, **values):
        self.calls.append((operation, values))
        if operation in {"promote", "verify-initialization"}:
            result = self.config["_candidate"]
            if result["snapshot"] != values["token"]:
                raise AssertionError("wrong candidate")
            self.config["_current"] = {
                "token": result["snapshot"], "source_commit": values["revision"],
                "target": values["target"], "verdict": result["verdict"], "run_id": result["run_id"],
            }
            self.config.setdefault("_currents", {})[str(self.work)] = self.config["_current"]
        if operation == "inspect" and values.get("token"):
            return {"current": self.config["_publications"][values["token"]]}
        return {"current": self.config.get("_currents", {}).get(str(self.work))}

    def invoke(self, mode, arguments, **_kwargs):
        if mode != "native":
            raise AssertionError("unexpected fake invocation")
        self.calls.append(("native", arguments))
        resumed = next((arg.split("=", 1)[1] for arg in arguments if arg.startswith("--run-id=")), None)
        self.config["_serial"] = self.config.get("_serial", 0) + 1
        run_id = resumed or f"synthetic-{self.config['_serial']}"
        directory = self.work / "ci/runs" / run_id
        directory.mkdir(parents=True, exist_ok=True)
        revision = next((arg.split("=", 1)[1] for arg in arguments if arg.startswith("--revision=")),
                        self.config.get("_last_revision"))
        self.config["_last_revision"] = revision
        if not resumed:
            artifact = next(arg.split("=", 1)[1] for arg in arguments if arg.startswith("--artifact="))
            guidance = next(arg.split("=", 1)[1] for arg in arguments if arg.startswith("--guidance="))
            current = self.config.get("_currents", {}).get(str(self.work))
            release.runtime.write_json(directory / "ci-input.json", {
                "source_commit": revision, "target": self.config["target"], "artifact": artifact,
                "dirty": False, "previous": current["token"] if current else None,
                "guidance": (self.work / Path(guidance).relative_to("/work")).read_text(),
            })
        verdict = self.config.get("_verdict", "PASS")
        result = {"run_id": run_id, "complete": True, "candidate": "--ci-candidate" in arguments,
                  "verdict": verdict, "snapshot": f"runs/{run_id}/ci-published/synthetic-fixture"}
        self.config["_candidate"] = result
        self.config.setdefault("_publications", {})[result["snapshot"]] = {
            "token": result["snapshot"], "source_commit": revision, "target": self.config["target"],
            "verdict": verdict, "run_id": run_id,
        }
        status = self.config.get("_runtime_status", {
            "PASS": "native_complete_pass", "WARNING": "native_complete_warning",
            "FAIL": "native_complete_bug_fail",
        }[verdict])
        code = self.config.get("_exit_code", 2 if verdict == "FAIL" else 0)
        receipt = {"status": status, "exit_code": code, "native_results": [result],
                   "mode": "native", "work": str(self.work), "image_id": self.config["image"],
                   "environment_id": self.config["environment_id"],
                   "launch_id": self.config["_launch_id"], "arguments": arguments,
                   "requested_image": self.config["image"], "execution_started": True,
                   "requested_seed": str(_kwargs["seed"]) if _kwargs.get("seed") else None,
                   "seed": str(_kwargs["seed"]) if _kwargs.get("seed") else None,
                   "requested_work": str(self.work), "container_exit_code": code}
        if not result["candidate"]:
            self.config.setdefault("_currents", {})[str(self.work)] = {
                "token": result["snapshot"], "source_commit": revision,
                "target": self.config["target"], "verdict": verdict, "run_id": run_id,
            }
        path = self.work / "runtime" / f"synthetic-attempt-{self.config['_serial']}" / "status.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        release.runtime.write_json(path.with_name("command.json"), {"mode": "native", "argv": arguments})
        release.runtime.write_json(path, receipt)
        return code, receipt, str(path)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="specula-release-wiring-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.donor = self.root / "donor"
        release.git(None, "init", "--quiet", "--initial-branch=main", str(self.donor))
        release.git(self.donor, "config", "user.name", "Synthetic fixture")
        release.git(self.donor, "config", "user.email", "fixture@example.invalid")
        self.a = self.commit("A")
        release.git(self.donor, "tag", "v1")
        self.b = self.commit("B")
        release.git(self.donor, "tag", "v2")
        self.control = self.root / "control"
        (self.control / "harness").mkdir(parents=True)
        (self.control / "guidance.md").write_text("Synthetic wiring fixture. Not a model result.\n")
        (self.control / "harness/run.sh").write_text("# Synthetic fixture; not executable.\n")
        self.config = {
            "version": 1, "root": str(self.root), "work": str(self.root / "work"),
            "source_url": str(self.donor), "repository": "microsoft/nvx",
            "trusted_branch": "main", "target": "synthetic-wiring-fixture",
            "image": "sha256:" + "a" * 64, "environment_id": "synthetic-only",
            "authorization": str(self.root / "approval.json"), "policy_incident": "synthetic",
        }

    def commit(self, value):
        (self.donor / "source.txt").write_text(value + "\n")
        release.git(self.donor, "add", "source.txt")
        release.git(self.donor, "commit", "--quiet", "-m",
                    f"Synthetic fixture {value}\n\nCo-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>")
        return release.git(self.donor, "rev-parse", "HEAD").stdout.strip()

    def request(self, mode="incremental", tag="v2", request_id="example", **kwargs):
        return release.Request(mode, tag, None, kwargs.get("run_id"), request_id,
                               kwargs.get("preflight", False)).validate()

    def run_request(self, request, blockers=(), *, provider_gate=False):
        def requirements(config):
            problem = release.authorization_blocker(config) if provider_gate else None
            return list(blockers) + ([problem] if problem else [])
        with patch.object(release, "prerequisites", side_effect=requirements):
            return release.run_request(self.config, request, backend_factory=FakeBackend,
                                       bundle_directory=self.control)

    def synthetic_approval(self):
        path = Path(self.config["authorization"])
        release.runtime.write_json(path, {
            "version": 1, "provider_authorized": True, "policy_incident": "synthetic",
            "resolution_reference": "SYNTHETIC FIXTURE ONLY, NOT PROVIDER AUTHORIZATION",
            "approved_at": "2026-09-16T00:00:00Z", "approved_by": "synthetic fixture",
        })
        path.chmod(0o600)

    def baseline(self, revision=None):
        self.config["_current"] = {
            "token": "runs/synthetic-old/ci-published/fixture", "source_commit": revision or self.a,
            "target": self.config["target"], "verdict": "PASS",
        }
        self.config.setdefault("_currents", {})[self.config["work"]] = self.config["_current"]

    def test_real_tag_resolution_and_complete_clean_clone(self):
        source, revision, _, selection = release.source_for(self.config, self.request(tag="v1"))
        self.assertEqual(revision, self.a)
        self.assertEqual(selection, {})
        self.assertEqual(release.git(source, "status", "--porcelain").stdout, "")
        self.assertFalse(list((source / ".git").rglob("*.promisor")))
        self.assertFalse((source / ".git/objects/info/alternates").exists())

    def test_moved_tag_is_rejected_and_old_resolution_retained(self):
        request = self.request(tag="v1")
        release.source_for(self.config, request)
        release.git(self.donor, "tag", "-f", "v1", self.b)
        with self.assertRaises(release.ReleaseError):
            release.source_for(self.config, request)
        cache = self.root / "repos/release-cache.git"
        self.assertEqual(release.git(cache, "rev-parse", "refs/tags/v1").stdout.strip(), self.a)

    def test_side_branch_tag_is_not_trusted(self):
        release.git(self.donor, "switch", "--quiet", "-c", "side", self.a)
        self.commit("side")
        release.git(self.donor, "tag", "side-release")
        release.git(self.donor, "switch", "--quiet", "main")
        with self.assertRaisesRegex(release.ReleaseError, "trusted branch"):
            release.source_for(self.config, self.request(tag="side-release"))

    def test_missing_baseline_never_implicitly_initializes(self):
        code, public, record = self.run_request(self.request(), blockers=["provider approval missing"])
        self.assertEqual(code, 3)
        self.assertEqual(record["status"], "needs_initialization")
        self.assertEqual(len(record["blockers"]), 2)
        self.assertTrue((public / "summary.md").is_file())
        self.assertFalse(any(name == "native" for name, _ in self.config["_calls"]))

    def test_explicit_initialization_then_real_commit_incremental_wiring(self):
        code, _, initial = self.run_request(self.request("initialize", "v1", "initial"))
        self.assertEqual(code, 0)
        first = next(args for name, args in self.config["_calls"] if name == "native")
        self.assertIn("--ci-init", first)
        self.assertNotIn("--ci-candidate", first)
        self.assertIn("--revision=" + self.a, first)
        code, _, updated = self.run_request(self.request(request_id="update"))
        self.assertEqual(code, 0)
        self.assertEqual(updated["previous"], initial["snapshot"])
        self.assertEqual(updated["revision"], self.b)
        calls = [args for name, args in self.config["_calls"] if name == "native"]
        self.assertEqual(len(calls), 2)
        self.assertIn("--incremental", calls[-1])
        self.assertIn("--ci-candidate", calls[-1])
        self.assertNotIn("--ci-init", calls[-1])
        self.assertIn("--revision=" + self.b, calls[-1])

    def test_complete_bug_fail_preserves_failure_and_can_promote(self):
        self.baseline()
        self.config["_verdict"] = "FAIL"
        code, _, record = self.run_request(self.request())
        self.assertEqual(code, 2)
        self.assertTrue(record["complete"])
        self.assertEqual(self.config["_current"]["verdict"], "FAIL")

    def test_failed_initialization_cannot_become_active_baseline(self):
        self.config.update(_runtime_status="wrapper_oom", _exit_code=137)
        code, _, record = self.run_request(self.request("initialize", "v1", "failed-init"))
        self.assertEqual(code, 137)
        self.assertFalse(record["complete"])
        work = Path(self.config["work"])
        self.assertEqual(release.active_work(work), work)
        self.assertFalse((work / "releases/active-work.json").exists())
        self.assertFalse(any(name == "verify-initialization" for name, _ in self.config["_calls"]))

    def test_initialization_resume_keeps_private_native_work_directory(self):
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        _, _, original = self.run_request(self.request("initialize", "v1", "init-resume"))
        self.config.pop("_runtime_status")
        self.config.pop("_exit_code")
        code, _, resumed = self.run_request(
            self.request("resume", "v1", "init-resume", run_id=original["native_run"]))
        self.assertEqual(code, 0)
        self.assertEqual(resumed["native_work"], original["native_work"])
        self.assertEqual(str(release.active_work(Path(self.config["work"]))), original["native_work"])
        args = [args for name, args in self.config["_calls"] if name == "native"][-1]
        self.assertNotIn("--ci-candidate", args)

    def test_new_request_cannot_duplicate_unselected_initialization(self):
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        self.run_request(self.request("initialize", "v1", "first-init"))
        code, _, rejected = self.run_request(self.request("initialize", "v1", "second-init"))
        self.assertNotEqual(code, 0)
        self.assertEqual(rejected["status"], "initialization_incomplete")
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)

    def test_incomplete_oom_timeout_and_policy_stop_never_promote(self):
        for status, exit_code in (("native_incomplete", 1), ("wrapper_oom", 137),
                                  ("wrapper_timeout", 124), ("policy_stop", 76)):
            with self.subTest(status=status):
                self.baseline()
                previous = dict(self.config["_current"])
                self.config.update(_runtime_status=status, _exit_code=exit_code, _calls=[])
                code, _, record = self.run_request(self.request(request_id=status))
                self.assertEqual(code, exit_code)
                self.assertFalse(record["complete"])
                self.assertEqual(self.config["_current"], previous)
                self.assertFalse(any(name == "promote" for name, _ in self.config["_calls"]))
                self.config.pop("_runtime_status")
                self.config.pop("_exit_code")
                code, _, _ = self.run_request(self.request(
                    "resume", request_id=status + "-resume", run_id=record["native_run"]))
                self.assertEqual(code, 0)

    def test_pending_target_blocks_new_requests_until_matching_resume(self):
        for status, exit_code in (("native_incomplete", 1), ("wrapper_oom", 137),
                                  ("wrapper_timeout", 124), ("wrapper_interrupted", 130)):
            with self.subTest(status=status):
                self.baseline()
                self.config.update(_runtime_status=status, _exit_code=exit_code, _calls=[])
                _, public, original = self.run_request(
                    self.request(tag="v1", request_id=status))
                self.config.pop("_runtime_status")
                self.config.pop("_exit_code")
                for mode in ("incremental", "preflight"):
                    code, _, blocked = self.run_request(
                        self.request(mode, request_id=status + "-" + mode))
                    self.assertNotEqual(code, 0)
                    self.assertEqual(blocked["status"], "resume_required")
                    self.assertIn(original["native_run"], blocked["error"])
                    self.assertIn(self.a, blocked["error"])
                self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)
                self.assertEqual(release.read_json(public / "result.json"), original)
                code, _, completed = self.run_request(self.request(
                    "resume", "v1", status + "-resume", run_id=original["native_run"]))
                self.assertEqual(code, 0)
                self.assertEqual(completed["native_run"], original["native_run"])
                code, _, _ = self.run_request(self.request(request_id=status + "-next"))
                self.assertEqual(code, 0)
                calls = [args for name, args in self.config["_calls"] if name == "native"]
                self.assertEqual(len(calls), 3)
                self.assertIn("--run-id=" + original["native_run"], calls[1])
                self.assertIn("--incremental", calls[2])

    def test_explicit_resume_preserves_native_identity(self):
        self.baseline()
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        _, _, original = self.run_request(self.request())
        self.config.pop("_runtime_status")
        self.config.pop("_exit_code")
        code, _, resumed = self.run_request(self.request("resume", run_id=original["native_run"]))
        self.assertEqual(code, 0)
        self.assertEqual(resumed["native_run"], original["native_run"])
        args = [args for name, args in self.config["_calls"] if name == "native"][-1]
        self.assertIn("--run-id=" + original["native_run"], args)
        self.assertNotIn("--ci-init", args)

    def test_resume_uses_retained_bundle_after_control_checkout_changes(self):
        self.baseline()
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        _, _, original = self.run_request(self.request())
        self.config.pop("_runtime_status")
        self.config.pop("_exit_code")
        (self.control / "guidance.md").write_text("Changed control checkout, not changed resume input.\n")
        code, _, resumed = self.run_request(self.request("resume", run_id=original["native_run"]))
        self.assertEqual(code, 0)
        self.assertEqual(resumed["control_bundle"], original["control_bundle"])

    def test_outer_crash_recovers_binding_from_explicit_launch_metadata(self):
        class SimulatedProcessDeath(BaseException):
            pass

        original_invoke = FakeBackend.invoke

        def interrupted(backend, *args, **kwargs):
            result = original_invoke(backend, *args, **kwargs)
            self.config["_interrupted_run"] = result[1]["native_results"][0]["run_id"]
            raise SimulatedProcessDeath()

        self.baseline()
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        with patch.object(FakeBackend, "invoke", interrupted), self.assertRaises(SimulatedProcessDeath):
            self.run_request(self.request())
        run_id = self.config["_interrupted_run"]
        self.assertFalse(release.managed_record(Path(self.config["work"]), run_id).exists())
        code, _, blocked = self.run_request(self.request(request_id="other-dispatch"))
        self.assertNotEqual(code, 0)
        self.assertEqual(blocked["status"], "resume_required")
        self.assertIn(run_id, blocked["error"])
        self.config.pop("_runtime_status")
        self.config.pop("_exit_code")
        code, _, recovered = self.run_request(self.request("resume", request_id="explicit-recovery", run_id=run_id))
        self.assertEqual(code, 0)
        self.assertEqual(recovered["native_run"], run_id)
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 2)

    def test_old_preflight_report_does_not_hide_an_interrupted_launch(self):
        class SimulatedProcessDeath(BaseException):
            pass

        self.baseline()
        self.run_request(self.request(preflight=True))
        original_invoke = FakeBackend.invoke

        def interrupted(backend, *args, **kwargs):
            original_invoke(backend, *args, **kwargs)
            raise SimulatedProcessDeath()

        with patch.object(FakeBackend, "invoke", interrupted), self.assertRaises(SimulatedProcessDeath):
            self.run_request(self.request())
        code, _, rejected = self.run_request(self.request())
        self.assertNotEqual(code, 0)
        self.assertEqual(rejected["status"], "resume_required")
        self.assertTrue(rejected.get("native_run"))
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)

    def test_interrupted_resume_refusal_is_reconciled_before_another_resume(self):
        class SimulatedProcessDeath(BaseException):
            pass

        self.baseline()
        self.synthetic_approval()
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        _, _, first = self.run_request(self.request(), provider_gate=True)
        self.config.update(_runtime_status="policy_stop", _exit_code=76)
        original_invoke = FakeBackend.invoke

        def interrupted(backend, *args, **kwargs):
            original_invoke(backend, *args, **kwargs)
            raise SimulatedProcessDeath()

        with patch.object(FakeBackend, "invoke", interrupted), self.assertRaises(SimulatedProcessDeath):
            self.run_request(self.request("resume", request_id="resumed-crash",
                                          run_id=first["native_run"]), provider_gate=True)
        self.assertIsNone(release.authorization_blocker(self.config))
        code, _, blocked = self.run_request(self.request("resume", request_id="after-resumed-refusal",
                                                        run_id=first["native_run"]), provider_gate=True)
        self.assertEqual(code, 3)
        self.assertEqual(blocked["status"], "provider_approval_required")
        self.assertIsNotNone(release.authorization_blocker(self.config))
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 2)

    def test_other_request_id_cannot_hide_an_interrupted_refusal(self):
        class SimulatedProcessDeath(BaseException):
            pass

        self.baseline()
        self.synthetic_approval()
        self.config.update(_runtime_status="policy_stop", _exit_code=76)
        original_invoke = FakeBackend.invoke

        def interrupted(backend, *args, **kwargs):
            original_invoke(backend, *args, **kwargs)
            raise SimulatedProcessDeath()

        with patch.object(FakeBackend, "invoke", interrupted), self.assertRaises(SimulatedProcessDeath):
            self.run_request(self.request(request_id="refused-crash"), provider_gate=True)
        code, _, blocked = self.run_request(self.request(request_id="different-release"), provider_gate=True)
        self.assertEqual(code, 3)
        self.assertEqual(blocked["status"], "provider_approval_required")
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)

    def test_unfinished_observer_blocks_other_requests_even_with_updated_approval(self):
        class SimulatedProcessDeath(BaseException):
            pass

        self.baseline()
        self.synthetic_approval()
        self.config.update(_runtime_status="starting", _exit_code=1)
        original_invoke = FakeBackend.invoke

        def interrupted(backend, *args, **kwargs):
            original_invoke(backend, *args, **kwargs)
            raise SimulatedProcessDeath()

        with patch.object(FakeBackend, "invoke", interrupted), self.assertRaises(SimulatedProcessDeath):
            self.run_request(self.request(request_id="observer-crash"), provider_gate=True)
        code, _, blocked = self.run_request(self.request(request_id="other-request"), provider_gate=True)
        self.assertNotEqual(code, 0)
        self.assertEqual(blocked["status"], "runtime_unresolved")
        hold = release.read_json(Path(self.config["work"]) / "releases/provider-hold.json")
        approval_path = Path(self.config["authorization"])
        approval = release.read_json(approval_path)
        approval["policy_incident"] = hold["incident"]
        release.runtime.write_json(approval_path, approval)
        code, _, blocked = self.run_request(self.request(request_id="still-unobserved"), provider_gate=True)
        self.assertNotEqual(code, 0)
        self.assertEqual(blocked["status"], "runtime_unresolved")
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)

    def test_newer_completed_resume_receipt_is_recovered_without_another_native_call(self):
        class SimulatedProcessDeath(BaseException):
            pass

        self.baseline()
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        _, _, first = self.run_request(self.request())
        self.config.pop("_runtime_status")
        self.config.pop("_exit_code")
        original_invoke = FakeBackend.invoke

        def interrupted(backend, *args, **kwargs):
            original_invoke(backend, *args, **kwargs)
            raise SimulatedProcessDeath()

        with patch.object(FakeBackend, "invoke", interrupted), self.assertRaises(SimulatedProcessDeath):
            self.run_request(self.request("resume", request_id="completion-crash", run_id=first["native_run"]))
        code, _, blocked = self.run_request(self.request(request_id="new-after-completion-crash"))
        self.assertNotEqual(code, 0)
        self.assertEqual(blocked["status"], "resume_required")
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 2)
        code, _, completed = self.run_request(self.request("resume", request_id="recover-completion",
                                                           run_id=first["native_run"]))
        self.assertEqual(code, 0)
        self.assertTrue(completed["complete"])
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 2)

    def test_resume_interrupted_before_runtime_start_can_be_retried_explicitly(self):
        class SimulatedProcessDeath(BaseException):
            pass

        self.baseline()
        self.config.update(_runtime_status="native_incomplete", _exit_code=1)
        _, _, first = self.run_request(self.request())
        with patch.object(FakeBackend, "invoke", side_effect=SimulatedProcessDeath()), self.assertRaises(SimulatedProcessDeath):
            self.run_request(self.request("resume", request_id="before-start", run_id=first["native_run"]))
        self.config.pop("_runtime_status")
        self.config.pop("_exit_code")
        code, _, completed = self.run_request(self.request("resume", request_id="retry-before-start",
                                                           run_id=first["native_run"]))
        self.assertEqual(code, 0)
        self.assertTrue(completed["complete"])
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 2)

    def test_coherent_public_metadata_cannot_accept_an_oom_initialization(self):
        self.config.update(_runtime_status="wrapper_oom", _exit_code=137)
        request = self.request("initialize", "v1", "oom-cache")
        _, public, original = self.run_request(request)
        altered = {**original, "complete": True, "status": "complete_pass", "exit_code": 0,
                   "verdict": "PASS", "snapshot": self.config["_candidate"]["snapshot"]}
        release.runtime.write_json(public / "result.json", altered)
        code, _, refused = self.run_request(request)
        self.assertNotEqual(code, 0)
        self.assertEqual(refused["status"], "invalid_result")
        self.assertFalse((Path(self.config["work"]) / "releases/active-work.json").exists())
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)

    def test_actual_launcher_image_inspection_failure_does_not_poison_future_requests(self):
        class MissingImageBackend(FakeBackend):
            def invoke(backend, mode, arguments, **kwargs):
                if backend.config.get("_image_missing"):
                    root = Path(backend.config["root"])
                    with patch.object(release.runtime, "ROOT", root.parent), \
                         patch.object(release.runtime, "NATIVE", root), \
                         patch.object(release.runtime, "docker", side_effect=subprocess.CalledProcessError(
                             1, ["docker", "image", "inspect", "SYNTHETIC-MISSING-IMAGE"])):
                        return release.Backend.invoke(backend, mode, arguments, **kwargs)
                return super().invoke(mode, arguments, **kwargs)

        self.baseline()
        (self.control / "controller").mkdir()
        (self.control / "controller/store.py").write_text("# Synthetic fixture; never executed.\n")
        fixture = self.root / "fixture"
        fixture.write_text("not a guest fixture\n")
        self.config.update(kernel=str(fixture), initramfs=str(fixture), timeout_seconds=60,
                           _image_missing=True)
        with patch.object(release, "prerequisites", return_value=[]):
            code, _, failed = release.run_request(
                self.config, self.request(request_id="missing-image"), backend_factory=MissingImageBackend,
                bundle_directory=self.control)
        self.assertNotEqual(code, 0)
        receipt = release.read_json(Path(failed["runtime_receipt"]))
        self.assertEqual(receipt["requested_image"], self.config["image"])
        self.assertEqual(receipt["requested_work"], self.config["work"])
        self.assertFalse(receipt["execution_started"])
        self.assertNotIn("image_id", receipt)
        self.config["_image_missing"] = False
        with patch.object(release, "prerequisites", return_value=[]):
            code, _, completed = release.run_request(
                self.config, self.request(request_id="restored-image"), backend_factory=MissingImageBackend,
                bundle_directory=self.control)
        self.assertEqual(code, 0)
        self.assertTrue(completed["complete"])

    def test_deferred_promotion_cannot_relabel_environment_or_target(self):
        self.baseline()
        original_store = FakeBackend.store

        def interrupted(backend, operation, **values):
            if operation == "promote":
                raise release.ReleaseError("synthetic_promotion_interruption", "fixture only")
            return original_store(backend, operation, **values)

        with patch.object(FakeBackend, "store", interrupted):
            _, _, failed = self.run_request(self.request())
        for key in ("environment_id", "target"):
            old = self.config[key]
            self.config[key] = "different-fixture-identity"
            code, _, refused = self.run_request(self.request("resume", request_id="changed-" + key,
                                                             run_id=failed["native_run"]))
            self.assertNotEqual(code, 0)
            self.assertEqual(refused["status"], "resume_identity_changed")
            self.config[key] = old
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)

    def test_report_rejects_symlinked_summary_without_modifying_target(self):
        self.baseline()
        public = Path(self.config["work"]) / "releases/requests/example/public"
        public.mkdir(parents=True)
        unrelated = self.root / "unrelated.txt"
        unrelated.write_text("untouched\n")
        (public / "summary.md").symlink_to(unrelated)
        code, rejected, record = self.run_request(self.request(preflight=True))
        self.assertNotEqual(code, 0)
        self.assertEqual(record["status"], "unsafe_path")
        self.assertNotEqual(public, rejected)
        self.assertEqual(unrelated.read_text(), "untouched\n")

    def test_cached_summary_symlink_and_changed_verdict_are_rejected(self):
        self.baseline()
        _, public, result = self.run_request(self.request())
        target = self.root / "unrelated.txt"
        target.write_text("untouched\n")
        (public / "summary.md").unlink()
        (public / "summary.md").symlink_to(target)
        code, _, failed = self.run_request(self.request())
        self.assertNotEqual(code, 0)
        self.assertEqual(failed["status"], "unsafe_path")
        self.assertEqual(target.read_text(), "untouched\n")
        (public / "summary.md").unlink()
        altered = {**result, "verdict": "FAIL", "status": "complete_fail", "exit_code": 2}
        release.runtime.write_json(public / "result.json", altered)
        code, _, failed = self.run_request(self.request())
        self.assertNotEqual(code, 0)
        self.assertEqual(failed["status"], "invalid_result")
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)

    def test_rerun_incomplete_requires_resume_without_overwriting_original(self):
        self.baseline()
        self.config.update(_runtime_status="policy_stop", _exit_code=76)
        _, public, old = self.run_request(self.request())
        code, rejection, new = self.run_request(self.request())
        self.assertNotEqual(code, 0)
        self.assertEqual(new["status"], "resume_required")
        self.assertNotEqual(public, rejection)
        self.assertEqual(release.read_json(public / "result.json")["native_run"], old["native_run"])
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)

    def test_duplicate_complete_request_reuses_receipt_not_new_model_call(self):
        self.baseline()
        self.run_request(self.request())
        code, _, record = self.run_request(self.request())
        self.assertEqual(code, 0)
        self.assertTrue(record["complete"])
        self.assertEqual(sum(name == "native" for name, _ in self.config["_calls"]), 1)
        self.assertEqual(self.config["_calls"][-1][0], "inspect")
        self.assertIn("token", self.config["_calls"][-1][1])

    def test_request_id_cannot_be_rebound_to_another_commit(self):
        self.baseline()
        _, public, old = self.run_request(self.request(tag="v1"))
        code, rejected, record = self.run_request(self.request(tag="v2"))
        self.assertNotEqual(code, 0)
        self.assertEqual(record["status"], "request_identity_changed")
        self.assertNotEqual(public, rejected)
        self.assertEqual(release.read_json(public / "result.json")["revision"], old["revision"])

    def test_backward_release_rejected(self):
        self.baseline(self.b)
        code, _, record = self.run_request(self.request(tag="v1"))
        self.assertNotEqual(code, 0)
        self.assertEqual(record["status"], "unsupported_ancestry")

    def test_preflight_and_provider_gate_do_not_invoke_model(self):
        self.baseline()
        code, _, record = self.run_request(self.request(preflight=True))
        self.assertEqual((code, record["status"]), (0, "preflight_ready"))
        code, _, record = self.run_request(self.request(request_id="blocked"), blockers=["No authorization"])
        self.assertEqual((code, record["status"]), (3, "provider_approval_required"))
        self.assertFalse(any(name == "native" for name, _ in self.config["_calls"]))

    def test_existing_initialization_and_legacy_resume_are_not_replaced(self):
        path = Path(self.config["work"]) / "ci/runs/legacy/ci-input.json"
        path.parent.mkdir(parents=True)
        path.write_text("{}")
        _, _, record = self.run_request(self.request("initialize"))
        self.assertEqual(record["status"], "initialization_incomplete")
        _, _, record = self.run_request(self.request("resume", request_id="legacy-resume", run_id="legacy"))
        self.assertEqual(record["status"], "unmanaged_resume")
        self.assertFalse(any(name == "native" for name, _ in self.config["_calls"]))

    def test_lock_conflict_does_not_overwrite_completed_request(self):
        self.baseline()
        _, public, old = self.run_request(self.request())
        with release.lock(self.root / ".release.lock"):
            code, rejected, record = self.run_request(self.request())
        self.assertNotEqual(code, 0)
        self.assertEqual(record["status"], "busy")
        self.assertNotEqual(public, rejected)
        self.assertEqual(release.read_json(public / "result.json"), old)

    def test_published_release_and_manual_dispatch_select_same_interface(self):
        path = self.root / "event.json"
        base = dict(event_file=str(path), mode=None, tag=None, revision=None, run_id=None,
                    request_id="gh-123", preflight_only=False)
        release.runtime.write_json(path, {
            "repository": {"full_name": "microsoft/nvx"}, "action": "published",
            "release": {"tag_name": "v2", "draft": False},
        })
        event = release.request_from_args(argparse.Namespace(**base, event_name="release"), self.config)
        self.assertEqual(event, self.request(request_id="gh-123"))
        release.runtime.write_json(path, {
            "repository": {"full_name": "microsoft/nvx"},
            "inputs": {"mode": "resume", "release_tag": "v2", "run_id": "native-id"},
        })
        event = release.request_from_args(argparse.Namespace(**base, event_name="workflow_dispatch"), self.config)
        self.assertEqual(event.mode, "resume")
        self.assertEqual(event.run_id, "native-id")
        with self.assertRaises(release.ReleaseError):
            release.request_from_args(argparse.Namespace(**base, event_name="pull_request"), self.config)

    def test_ref_and_path_injection_rejected(self):
        for tag in ("../bad", "a\nb", "--upload-pack=x", "a b", "refs/../x"):
            with self.subTest(tag=tag), self.assertRaises(release.ReleaseError):
                self.request(tag=tag)
        with self.assertRaises(release.ReleaseError):
            self.request(request_id="../escape")

    def test_approval_is_explicit_private_and_incident_bound(self):
        self.assertIsNotNone(release.authorization_blocker(self.config))
        path = Path(self.config["authorization"])
        release.runtime.write_json(path, {"version": 1, "provider_authorized": True,
            "policy_incident": "synthetic", "resolution_reference": "SYNTHETIC TEST ONLY",
            "approved_at": "2026-09-16T00:00:00Z", "approved_by": "synthetic test"})
        path.chmod(0o600)
        self.assertIsNone(release.authorization_blocker(self.config))
        hold = Path(self.config["work"]) / "releases/provider-hold.json"
        hold.parent.mkdir(parents=True)
        release.runtime.write_json(hold, {"incident": "new-synthetic-refusal"})
        self.assertIsNotNone(release.authorization_blocker(self.config))
        path.chmod(0o644)
        self.assertIsNotNone(release.authorization_blocker(self.config))

    def test_symlinked_data_root_child_is_rejected_without_writes(self):
        outside = self.root / "unrelated"
        outside.mkdir()
        (self.root / "repos").symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(release.ReleaseError, "symlink"):
            release.source_for(self.config, self.request())
        self.assertEqual(list(outside.iterdir()), [])


if __name__ == "__main__":
    unittest.main()

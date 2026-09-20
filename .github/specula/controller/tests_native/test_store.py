"""Native storage API fixtures only. No verified model, agent, VM or TLC run."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from store import operate
from specula.ci_store import CIError, CIStore, write_json


def git(path, *args):
    return subprocess.check_output(
        ["git", "-c", "core.hooksPath=/dev/null", "-C", str(path), *args],
        text=True, stderr=subprocess.PIPE,
    ).strip()


class NativeStoreTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="SYNTHETIC-native-store-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "ci"
        self.target = "SYNTHETIC_STORAGE_TEST_ONLY"

    def publish(self, name, *, previous=None, parent_source=None, candidate=False):
        run = self.root / "runs" / name
        source = run / "ci-source"
        run.mkdir(parents=True)
        if parent_source:
            subprocess.run(["git", "clone", "--quiet", "--no-local", str(parent_source), str(source)], check=True)
        else:
            source.mkdir()
            git(source, "init", "--quiet", "--initial-branch=main")
        git(source, "config", "user.name", "Synthetic storage fixture")
        git(source, "config", "user.email", "fixture@example.invalid")
        (source / "fixture.txt").write_text(name + "\n")
        git(source, "add", "fixture.txt")
        git(source, "commit", "--quiet", "-m",
            f"Synthetic fixture {name}\n\nCo-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>")
        revision = git(source, "rev-parse", "HEAD")
        assets = run / "synthetic-assets"
        (assets / "spec").mkdir(parents=True)
        (assets / "harness").mkdir()
        (assets / "spec/base.tla").write_text("SYNTHETIC STORAGE FIXTURE; NOT A VERIFIED MODEL\n")
        (assets / "harness/run.sh").write_text("# SYNTHETIC STORAGE FIXTURE; NOT AN EXECUTABLE HARNESS\n")
        write_json(assets / "ci-verdict.json", {"version": 1, "run_id": name, "findings": []})
        inputs = {
            "target": self.target, "artifact": "synthetic-storage-test", "source": f"runs/{name}/ci-source",
            "source_commit": revision, "snapshot_commit": revision, "dirty": False,
            "guidance": "Synthetic storage fixture only.", "previous": previous,
            "verdict": "PASS", "check_key": "synthetic-storage-key",
        }
        store = CIStore(self.root)
        store.acquire()
        try:
            destination = store.publish(run, assets, inputs, advance=False)
            token = destination.relative_to(store.root).as_posix()
            write_json(run / "ci-result.json", {
                "run_id": name, "complete": True, "verdict": "PASS",
                "candidate": candidate, "snapshot": token,
            })
            if not candidate:
                store.advance(token)
        finally:
            store.close()
        return token, revision, source

    def test_empty_store_inspection_does_not_invent_a_baseline(self):
        self.assertEqual(operate(self.root, "inspect"), {"current": None})

    def test_initialization_verification_is_not_candidate_promotion(self):
        token, revision, _ = self.publish("init")
        result = operate(self.root, "verify-initialization", token=token, revision=revision,
                         target=self.target, verdict="PASS")
        self.assertEqual(result["current"]["token"], token)
        with self.assertRaises(CIError):
            operate(self.root, "promote", token=token, revision=revision, target=self.target, verdict="PASS")

    def test_candidate_keeps_previous_until_explicit_idempotent_promotion(self):
        old, _, source = self.publish("a")
        new, revision, _ = self.publish("b", previous=old, parent_source=source, candidate=True)
        self.assertEqual(operate(self.root, "inspect")["current"]["token"], old)
        for _ in range(2):
            self.assertEqual(operate(self.root, "promote", token=new, revision=revision,
                                    target=self.target, verdict="PASS")["current"]["token"], new)

    def test_tampered_model_cannot_be_promoted(self):
        old, _, source = self.publish("a")
        new, revision, _ = self.publish("b", previous=old, parent_source=source, candidate=True)
        (self.root / new / "model/spec/base.tla").write_text("tampered fixture\n")
        with self.assertRaises(CIError):
            operate(self.root, "promote", token=new, revision=revision, target=self.target, verdict="PASS")
        self.assertEqual(operate(self.root, "inspect")["current"]["token"], old)

    def test_source_verdict_and_completion_mismatches_are_rejected(self):
        old, _, source = self.publish("a")
        new, revision, _ = self.publish("b", previous=old, parent_source=source, candidate=True)
        for expected_revision, expected_verdict in (("0" * 40, "PASS"), (revision, "FAIL")):
            with self.assertRaises(CIError):
                operate(self.root, "promote", token=new, revision=expected_revision,
                        target=self.target, verdict=expected_verdict)
        receipt = self.root / "runs/b/ci-result.json"
        record = json.loads(receipt.read_text())
        record["complete"] = False
        write_json(receipt, record)
        with self.assertRaises(CIError):
            operate(self.root, "promote", token=new, revision=revision, target=self.target, verdict="PASS")
        self.assertEqual(operate(self.root, "inspect")["current"]["token"], old)

    def test_stale_predecessor_cannot_replace_new_current(self):
        old, _, source = self.publish("a")
        b, revision_b, _ = self.publish("b", previous=old, parent_source=source, candidate=True)
        c, revision_c, _ = self.publish("c", previous=old, parent_source=source, candidate=True)
        operate(self.root, "promote", token=c, revision=revision_c, target=self.target, verdict="PASS")
        with self.assertRaises(CIError):
            operate(self.root, "promote", token=b, revision=revision_b, target=self.target, verdict="PASS")
        self.assertEqual(operate(self.root, "inspect")["current"]["token"], c)

    def test_inspection_requires_native_verification_and_completion(self):
        token, _, _ = self.publish("init")
        state_path = self.root / token / "state.json"
        receipt_path = self.root / "runs/init/ci-result.json"
        state = json.loads(state_path.read_text())
        receipt = json.loads(receipt_path.read_text())
        for changes, receipt_changes in (
            ({"verdict": "UNVERIFIED"}, {"verdict": "UNVERIFIED"}),
            ({"verification": "SYNTHETIC unverified input"}, {}),
            ({}, {"complete": False}),
        ):
            with self.subTest(changes=changes, receipt_changes=receipt_changes):
                write_json(state_path, state | changes)
                write_json(receipt_path, receipt | receipt_changes)
                with self.assertRaises(CIError):
                    operate(self.root, "inspect")
        write_json(state_path, state)
        write_json(receipt_path, receipt)
        self.assertEqual(operate(self.root, "inspect")["current"]["token"], token)


if __name__ == "__main__":
    if not os.environ.get("TMPDIR", "").startswith("/work/"):
        raise SystemExit("Run this synthetic storage suite only in the bounded runtime with /work scratch.")
    unittest.main()

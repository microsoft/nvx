"""Pinned native CLI/recovery contracts; no provider, VM, or model invocation."""

import contextlib
import io
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "runtime"))
from entrypoint import POLICY, native_args
from specula import phaselib
from specula.ci_workflow import CIPipeline


class NativeRuntimeContractTests(unittest.TestCase):
    def parse(self, arguments):
        pipeline = CIPipeline()
        self.assertIsNone(pipeline.parse_args(native_args(arguments)[2:]))
        return pipeline

    def test_new_run_arguments_match_the_native_mode_contract(self):
        for mode in ("--ci-init", "--incremental"):
            with self.subTest(mode=mode):
                args = [mode, "SYNTHETIC"] if mode == "--ci-init" else [mode]
                pipeline = self.parse(args)
                self.assertEqual(pipeline.policy_retries, 2)
                self.assertEqual(pipeline.transient_resumes, 3)
                self.assertEqual(pipeline.max_parallel, "1" if mode == "--ci-init" else None)

    def test_resume_restores_saved_budgets_instead_of_new_defaults(self):
        for mode in ("--ci-init", "--incremental"):
            for policy_budget, transient_budget in ((0, 0), (0, 3), (2, 3)):
                with self.subTest(mode=mode, policy=policy_budget, transient=transient_budget):
                    args = [mode, "SYNTHETIC"] if mode == "--ci-init" else [mode]
                    saved = self.parse(args)._resume_configuration_document()
                    saved["policy_retries"] = policy_budget
                    saved["transient_resumes"] = transient_budget
                    resumed = self.parse(["--run-id=synthetic-existing"])
                    resumed._restore_resume_configuration(saved)
                    self.assertEqual(resumed.policy_retries, policy_budget)
                    self.assertEqual(resumed.transient_resumes, transient_budget)
                    self.assertEqual(resumed.incremental, mode == "--incremental")

    def test_native_recovery_has_independent_bounded_budgets(self):
        for codes, expected, calls, delays in (
            ([74, 74, 0], 0, 3, 2),
            ([74, 74, 74, 74], 74, 4, 3),
            ([76, 76, 0], 0, 3, 0),
            ([76, 76, 76], 76, 3, 0),
            ([74, 76, 0], 0, 3, 1),
            ([74, 76, 74, 76, 74, 76], 76, 6, 3),
            ([76, 74, 76, 74, 74, 74], 74, 6, 3),
        ):
            with self.subTest(codes=codes), tempfile.TemporaryDirectory(
                prefix="SYNTHETIC-recovery-"
            ) as directory:
                root = Path(directory)
                with patch("specula.phaselib.subprocess.run", side_effect=[
                    SimpleNamespace(returncode=code) for code in codes
                ]) as invoke, patch("specula.phaselib.time.sleep") as sleep, \
                     patch("specula.phaselib.tlc_tasks.prepare_environment"), \
                     patch("specula.phaselib.tlc_tasks.stop_owned_tasks"), \
                     contextlib.redirect_stdout(io.StringIO()):
                    code, _ = phaselib.run_agent_blocking(
                        root / "synthetic-adapter.sh",
                        "SYNTHETIC TRANSPORT TEST ONLY",
                        root / "prompt.md", root / "turn.log",
                        phase_key="synthetic", work_dir=root, claude_alias="unused",
                        policy_retries=int(POLICY["policy-retries"]),
                        transient_resumes=int(POLICY["transient-resumes"]),
                    )
                self.assertEqual(code, expected)
                self.assertEqual(invoke.call_count, calls)
                self.assertEqual(sleep.call_count, delays)
                self.assertEqual([call.args[0] for call in sleep.call_args_list], [4, 8, 16][:delays])


if __name__ == "__main__":
    if not os.environ.get("TMPDIR", "").startswith("/work/"):
        raise SystemExit("Run these synthetic contracts only in the bounded runtime with /work scratch.")
    unittest.main()

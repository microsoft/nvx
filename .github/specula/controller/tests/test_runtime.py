from pathlib import Path
import contextlib
import io
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT), str(ROOT / "runtime")]

import run
from copilot_wrapper import SAFETY, safe_args
from common import SECRET_NAMES, clean_env
from entrypoint import auth_probe, native_args
from git_guard import checked_args


class RuntimeTests(unittest.TestCase):
    def test_auth_probe_requires_exact_response_without_tools(self):
        for response, expected in (("AUTH_OK\n", 0), ("unexpected response", 1)):
            with self.subTest(response=response), \
                patch.dict("os.environ", {"NATIVE_ATTEMPT_DIR": "/work/mock"}), \
                patch("entrypoint.subprocess.run", return_value=SimpleNamespace(
                    returncode=0, stdout=response, stderr="",
                )) as process, patch("entrypoint.write_json"), patch.object(Path, "write_text"), \
                contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(auth_probe(), expected)
                self.assertIn("--available-tools=", process.call_args.args[0])
                self.assertNotIn("--deny-tool=*", process.call_args.args[0])
                self.assertIn("gpt-5.6-sol-fast", process.call_args.args[0])

    def test_shell_environment_removes_credential_names(self):
        with patch.dict("os.environ", {key: "synthetic-test-value" for key in SECRET_NAMES}):
            self.assertTrue(all(key not in clean_env() for key in SECRET_NAMES))

    def test_native_modes_and_fixed_policy(self):
        for mode in ("--ci-init", "--incremental", "--run-id=original"):
            command = native_args([mode, "--revision=" + "a" * 40])
            self.assertIn("--agent=copilot-cli", command)
            self.assertIn("--model=gpt-5.6-sol-fast", command)
            self.assertIn("--effort=xhigh", command)
            self.assertIn("--max-turns=0", command)
            if mode.startswith("--run-id="):
                self.assertNotIn("--artifact=/source", command)
                self.assertFalse(any(arg.startswith("--policy-retries=") for arg in command))
                self.assertFalse(any(arg.startswith("--transient-resumes=") for arg in command))
            else:
                self.assertIn("--artifact=/source", command)
                self.assertIn("--policy-retries=2", command)
                self.assertIn("--transient-resumes=3", command)
            if mode == "--ci-init":
                self.assertIn("--max-parallel=1", command)
            else:
                self.assertFalse(any(arg.startswith("--max-parallel=") for arg in command))
            self.assertIn("--ci-dir=/work/ci", command)
        command = native_args(["--ci-init", "--artifact=/sources/source-a"])
        self.assertIn("--artifact=/sources/source-a", command)
        self.assertNotIn("--artifact=/source", command)

    def test_skips_and_provider_changes_rejected(self):
        for option in ("--skip-confirmation", "--skip-classification", "--skip-repair-loop",
                       "--agent=codex", "--model=other", "--ci-dir=/elsewhere",
                       "--policy-retries=1", "--transient-resumes=20", "--artifact=/work/source"):
            with self.subTest(option=option), self.assertRaises(ValueError):
                native_args(["--incremental", option])
        with self.assertRaises(ValueError):
            native_args(["--ci-init", "--artifact=/sources/clean", "--artifact=/sources/dirty"])

    def test_incremental_and_resume_reject_phase_parallelism(self):
        for mode in ("--incremental", "--run-id=existing"):
            with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, "parallelism"):
                native_args([mode, "--max-parallel=1"])

    def test_resume_does_not_override_saved_recovery_budget(self):
        for option in ("policy-retries", "transient-resumes"):
            for budget in ("0", "2", "3"):
                with self.subTest(option=option, budget=budget), self.assertRaisesRegex(ValueError, "saved"):
                    native_args(["--run-id=existing", f"--{option}={budget}"])

    def test_created_or_running_container_does_not_imply_command_success(self):
        for status in ("created", "running", "restarting", "dead"):
            with self.subTest(status=status), self.assertRaises(ValueError):
                run.completed_exit_code({
                    "Status": status, "Running": status == "running", "ExitCode": 0,
                    "StartedAt": "2026-09-16T05:00:00Z",
                })
        with self.assertRaises(ValueError):
            run.completed_exit_code({
                "Status": "exited", "Running": False, "ExitCode": 0,
                "StartedAt": "0001-01-01T00:00:00Z",
            })
        for code in (0, 7, 137):
            self.assertEqual(run.completed_exit_code({
                "Status": "exited", "Running": False, "Restarting": False,
                "ExitCode": code, "StartedAt": "2026-09-16T05:00:00Z",
            }), code)

    def test_copilot_server_and_prompt_receive_safety_flags(self):
        for command in (["-p", "test"], ["--headless", "--stdio"]):
            args, probe = safe_args(command)
            self.assertFalse(probe)
            self.assertIn("--no-remote-export", args)
            self.assertIn("--disable-builtin-mcps", args)
            self.assertTrue(any(arg.startswith("--secret-env-vars=COPILOT_GITHUB_TOKEN") for arg in args))
        self.assertTrue(safe_args(["--version"])[1])
        for option in ("login", "--remote-export", "--share-gist=report"):
            with self.assertRaises(ValueError):
                safe_args([option])

    def test_git_local_read_allowed_publication_denied(self):
        self.assertIn("clone", checked_args(["clone", "/source", "/work/source"]))
        for args in (["push", "origin", "main"], ["-C", "/work", "push"],
                     ["send-pack", "host:repo"], ["-c", "alias.publish=!git push", "publish"]):
            with self.assertRaises(ValueError):
                checked_args(args)

    def test_docker_limits_and_readonly_mounts(self):
        args = run.mounts_and_limits(
            "/data/work", "/data/cache", "/data/source", "/data/seed",
            sources="/data/sources", harness="/data/harness",
            vmlinux="/guest/kernel", initramfs="/guest/initramfs",
        )
        for flag in ("--memory=26g", "--memory-swap=26g", "--cpus=6",
                     "--pids-limit=1024", "--user=1001:1003", "--group-add=998",
                     "--read-only", "--device=/dev/mshv:/dev/mshv"):
            self.assertIn(flag, args)
        self.assertIn("type=bind,src=/data/source,dst=/source,readonly", args)
        self.assertIn("type=bind,src=/data/seed,dst=/seed,readonly", args)
        for mount in (
            "type=bind,src=/data/sources,dst=/sources,readonly",
            "type=bind,src=/data/harness,dst=/harness,readonly",
            "type=bind,src=/guest/kernel,dst=/fixtures/vmlinux,readonly",
            "type=bind,src=/guest/initramfs,dst=/fixtures/initramfs.cpio.gz,readonly",
        ):
            self.assertIn(mount, args)

    def test_completion_not_inferred_from_exit_code(self):
        self.assertEqual(run.outcome("native", 0, []), "native_incomplete")
        self.assertEqual(run.outcome("native", 2, []), "native_incomplete")
        self.assertEqual(run.outcome("native", 2, [{"verdict": "FAIL"}]), "native_complete_bug_fail")
        self.assertEqual(run.outcome("native", 0, [{"verdict": "WARNING"}]), "native_complete_warning")
        self.assertEqual(run.outcome("native", 76, []), "policy_stop")
        self.assertEqual(run.outcome("native", 0, [{"verdict": "FAIL"}]), "completed_receipt_exit_mismatch")
        self.assertEqual(run.outcome("native", 0, [{"verdict": "PASS"}], timed_out=True), "wrapper_timeout")
        self.assertEqual(run.outcome("native", 137, [], oom=True), "wrapper_oom")
        self.assertEqual(run.outcome("exec", 0, []), "command_success")


if __name__ == "__main__":
    unittest.main()

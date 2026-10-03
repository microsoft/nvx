#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import json
import queue
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Callable
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    benchmark,
    common,
    control_session,
    managed_exec_tests,
    microvm_tests,
    openvmm_process,
)
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
)


def _posix_shell() -> str | None:
    shell = shutil.which("sh")
    if shell is None:
        git = shutil.which("git")
        git_shell = Path(git).parent.parent / "bin" / "sh.exe" if git else None
        if git_shell is not None and git_shell.is_file():
            shell = str(git_shell)
    return shell


def _vp_binding_profile(vp_indices: list[int]) -> bytes:
    fields = "duration_ns=1 process_elapsed_ns=2 pid=3"
    phases = [
        "vp_bind_bsp" if index == 0 else f"vp_bind_ap_{index}" for index in vp_indices
    ]
    lines = [
        f"OPENVMM_SNAPSHOT_PROFILE_V1 operation=startup phase={phase} "
        f"exclusive=0 {fields}"
        for phase in phases
    ]
    lines.append(
        "OPENVMM_SNAPSHOT_PROFILE_V1 operation=startup phase=vp_thread_bind "
        f"exclusive=1 {fields}"
    )
    return "".join(f"{line}\r\n" for line in lines).encode()


def _restore_target(command: list[str]) -> int | None:
    if "--restore-processors" not in command:
        return None
    return int(command[command.index("--restore-processors") + 1])


def _restore_processors_measure(
    backend: str,
    *,
    mshv_prefix: bool = True,
) -> MagicMock:
    def measure(command: list[str], **kwargs: object) -> None:
        target = _restore_target(command)
        vp_count = 8
        if mshv_prefix and backend == "mshv" and target is not None:
            vp_count = target
        log_path = cast(Path, kwargs["log_path"])
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_path.write_bytes(_vp_binding_profile(list(range(vp_count))))

    return MagicMock(side_effect=measure)


class MicrovmTestParserTests(unittest.TestCase):
    def test_parser_defaults_to_all_correctness_scenarios(self):
        args = nvx.parse_args(["test-microvm", "--backend", "mshv"])

        self.assertIsNone(args.scenario)
        self.assertEqual(args.processors, [1, 2, 4, 8])
        self.assertEqual(args.guest, "alpine")
        self.assertIsNone(args.memory_mib)
        self.assertEqual(args.timeout, 60.0)
        self.assertIs(args.handler, microvm_tests.run)

    def test_parser_accepts_selected_scenarios_and_processors(self):
        args = nvx.parse_args(
            [
                "test-microvm",
                "--backend",
                "whp",
                "--scenario",
                "smp",
                "--processors",
                "2",
                "8",
                "--output-dir",
                "results",
            ]
        )

        self.assertEqual(args.scenario, ["smp"])
        self.assertEqual(args.processors, [2, 8])
        self.assertEqual(args.output_dir, Path("results"))

    def test_parser_accepts_ubuntu_guest_profile(self):
        args = nvx.parse_args(
            [
                "test-microvm",
                "--backend",
                "whp",
                "--guest",
                "ubuntu",
                "--scenario",
                "guest-boot",
            ]
        )

        self.assertEqual(args.guest, "ubuntu")
        self.assertIsNone(args.memory_mib)


class PublicManagedExecAcceptanceTests(unittest.TestCase):
    def _run_acceptance(
        self,
        root: Path,
        *,
        bad_pwd_output: bool = False,
        malformed_outcome: bool = False,
        outcome_mutation: str | None = None,
        start_returncode: int = 0,
        stop_returncode: int = 0,
        default_environment: bytes = (
            b"PATH=/usr/sbin:/usr/bin:/sbin:/bin\n"
            b"TERM=linux\nHOME=/nonexistent\nUSER=nobody\nLOGNAME=nobody\n"
            b"SHLVL=1\nnvx_workload_uid=65534\nnvx_hostname=nvx\n"
        ),
        evidence_failure: bool = False,
        command_log: list[list[str]] | None = None,
    ) -> tuple[list[list[str]], list[dict[str, object]]]:
        distro = root / "ubuntu-distro.erofs"
        distro.write_bytes(b"distro")
        distro.with_name("ubuntu-distro.erofs.manifest.json").write_text(
            json.dumps({"uuid": "12345678-1234-1234-1234-123456789abc"}),
            encoding="utf-8",
        )
        scratch = root / "ubuntu-smoke-scratch.ext4"
        scratch.write_bytes(b"scratch")
        output_dir = root / "results"
        commands: list[list[str]] = command_log if command_log is not None else []
        options: list[dict[str, object]] = []
        real_copyfile = shutil.copyfile

        def artifact(name: str) -> Path:
            return {
                "ubuntu-distro.erofs": distro,
                "ubuntu-smoke-scratch.ext4": scratch,
            }[name]

        def require(path: Path, _description: str) -> Path:
            return path

        def copyfile(source: Path | str, destination: Path | str) -> Path | str:
            if (
                evidence_failure
                and Path(destination) == output_dir / "public-exec-openvmm.log"
            ):
                raise OSError("injected evidence failure")
            return real_copyfile(source, destination)

        def run(
            command: list[str], **kwargs: object
        ) -> subprocess.CompletedProcess[bytes]:
            commands.append(command)
            options.append(kwargs)
            operation = command[3]
            state = Path(command[command.index("--state-dir") + 1])
            if operation == "provision":
                state.mkdir(parents=True, exist_ok=True)
            if operation == "start":
                state.mkdir(parents=True, exist_ok=True)
                (state / "openvmm.log").write_text("bounded log\n", encoding="utf-8")
                if start_returncode:
                    return subprocess.CompletedProcess(
                        command, start_returncode, b"", b"start failed"
                    )
            if operation == "stop":
                return subprocess.CompletedProcess(
                    command, stop_returncode, b"", b"stop failed"
                )
            if operation == "deprovision":
                if stop_returncode:
                    return subprocess.CompletedProcess(
                        command,
                        1,
                        b"",
                        b"sandbox must be stopped before deprovision",
                    )
                shutil.rmtree(state)
            if operation != "exec":
                return subprocess.CompletedProcess(command, 0, b"", b"")

            cwd = command[command.index("--cwd") + 1] if "--cwd" in command else "/"
            timeout_ms = (
                command[command.index("--exec-timeout-ms") + 1]
                if "--exec-timeout-ms" in command
                else "0"
            )
            if not cwd.startswith("/"):
                return subprocess.CompletedProcess(
                    command,
                    1,
                    b"",
                    b"managed exec working directory must be an absolute path\n",
                )
            if int(timeout_ms) < 0 or int(timeout_ms) > 0xFFFFFFFF:
                return subprocess.CompletedProcess(
                    command,
                    1,
                    b"",
                    b"managed exec timeout must be 0 through 4294967295 ms\n",
                )

            entrypoint = command[command.index("--entrypoint") + 1]
            stdout = b""
            stderr = b""
            returncode = 0
            category = "exit"
            if entrypoint == "/bin/pwd":
                stdout = (
                    b"x" * (managed_exec_tests.DIAGNOSTIC_LIMIT + 10)
                    if bad_pwd_output
                    else f"{cwd}\n".encode()
                )
            elif entrypoint == "/usr/bin/env":
                if "--environment-file" in command:
                    environment_file = Path(
                        command[command.index("--environment-file") + 1]
                    )
                    entries = json.loads(environment_file.read_text(encoding="utf-8"))
                    stdout = (
                        b"" if not entries else ("\n".join(entries) + "\n").encode()
                    )
                elif "--environment" in command:
                    entries = [
                        command[index + 1]
                        for index, value in enumerate(command)
                        if value == "--environment"
                    ]
                    stdout = ("\n".join(entries) + "\n").encode()
                else:
                    stdout = default_environment
            elif entrypoint == "/usr/bin/getent":
                stdout = b"nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n"
            elif entrypoint == "/bin/sleep":
                returncode = 124
                category = "timeout"
            elif cwd in ("/does-not-exist", "/etc/passwd", "/root"):
                returncode = 125
                stderr = b"cannot use working directory\n"
            elif entrypoint == "/bin/sh":
                stdout = b"public stdout"
                stderr = b"public stderr"
                returncode = 7

            if "--outcome-report" in command:
                report = Path(command[command.index("--outcome-report") + 1])
                payload: dict[str, object] = {
                    "schema_version": 1,
                    "operation_id": "1" * 32,
                    "outcome": {
                        "operation": "exec",
                        "category": category,
                        "status_code": True if malformed_outcome else returncode,
                    },
                }
                if outcome_mutation == "extra-top-level":
                    payload["extra"] = "unexpected"
                elif outcome_mutation == "missing-top-level":
                    del payload["operation_id"]
                elif outcome_mutation == "extra-outcome":
                    cast(dict[str, object], payload["outcome"])["extra"] = "unexpected"
                elif outcome_mutation == "missing-outcome":
                    del cast(dict[str, object], payload["outcome"])["category"]
                elif outcome_mutation == "float-schema":
                    payload["schema_version"] = 1.0
                elif outcome_mutation == "float-status":
                    cast(dict[str, object], payload["outcome"])["status_code"] = float(
                        returncode
                    )
                report.write_text(
                    json.dumps(payload),
                    encoding="utf-8",
                )
            return subprocess.CompletedProcess(command, returncode, stdout, stderr)

        with (
            patch.object(managed_exec_tests, "artifact_path", side_effect=artifact),
            patch.object(managed_exec_tests, "require_file", side_effect=require),
            patch.object(managed_exec_tests.subprocess, "run", side_effect=run),
            patch.object(managed_exec_tests.shutil, "copyfile", side_effect=copyfile),
        ):
            managed_exec_tests.run_managed_exec_configuration(
                "whp", timeout=2, output_dir=output_dir
            )
        return commands, options

    def test_public_acceptance_observes_full_cli_contract(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            commands, options = self._run_acceptance(root)
            records = json.loads(
                (root / "results" / "public-exec-checks.json").read_text(
                    encoding="utf-8"
                )
            )
            exit_outcome = json.loads(
                (root / "results" / "public-exec-exit-outcome.json").read_text(
                    encoding="utf-8"
                )
            )
            timeout_outcome = json.loads(
                (root / "results" / "public-exec-timeout-outcome.json").read_text(
                    encoding="utf-8"
                )
            )

        exec_commands = [command for command in commands if command[3] == "exec"]
        self.assertTrue(
            all(
                command[:3]
                == [
                    sys.executable,
                    str(BuildConstants.REPO_ROOT / "scripts" / "nvx.py"),
                    "sandbox",
                ]
                for command in commands
            )
        )
        self.assertTrue(
            all(
                option
                == {
                    "cwd": BuildConstants.REPO_ROOT,
                    "capture_output": True,
                    "timeout": 12,
                }
                for option in options
            )
        )
        state_directories = {
            command[command.index("--state-dir") + 1] for command in commands
        }
        self.assertEqual(len(state_directories), 1)
        self.assertTrue(
            any(
                "--cwd" in command and command[command.index("--cwd") + 1] == "relative"
                for command in exec_commands
            )
        )
        for value in ("-1", str(0x100000000)):
            self.assertTrue(
                any(
                    "--exec-timeout-ms" in command
                    and command[command.index("--exec-timeout-ms") + 1] == value
                    for command in exec_commands
                )
            )
        self.assertTrue(any("--outcome-report" in command for command in exec_commands))
        self.assertEqual(
            [record["operation"] for record in records[:2]], ["provision", "start"]
        )
        self.assertEqual(records[-1]["operation"], "deprovision")
        self.assertTrue(all(record["state_exists"] for record in records[:-1]))
        self.assertFalse(records[-1]["state_exists"])
        self.assertTrue(all(isinstance(record.get("argv"), list) for record in records))
        recorded_argv = [value for record in records for value in record["argv"]]
        self.assertIn("<redacted>", recorded_argv)
        self.assertNotIn("SECOND=inline value", recorded_argv)
        self.assertEqual(exit_outcome["outcome"]["category"], "exit")
        self.assertEqual(exit_outcome["outcome"]["status_code"], 7)
        self.assertEqual(timeout_outcome["outcome"]["category"], "timeout")
        self.assertEqual(timeout_outcome["outcome"]["status_code"], 124)
        fixture_root = Path(state_directories.pop()).parent
        self.assertFalse(fixture_root.exists())

    def test_public_acceptance_preserves_test_and_cleanup_failures(self):
        commands: list[list[str]] = []
        fixture_root: Path | None = None
        try:
            with tempfile.TemporaryDirectory() as temporary:
                with self.assertRaisesRegex(
                    RuntimeError,
                    "unexpected output.*cleanup.*stop failed",
                ) as raised:
                    self._run_acceptance(
                        Path(temporary),
                        bad_pwd_output=True,
                        stop_returncode=9,
                        command_log=commands,
                    )
            fixture_root = Path(
                commands[0][commands[0].index("--state-dir") + 1]
            ).parent
            self.assertIn("10 bytes omitted", str(raised.exception))
            self.assertLess(
                len(str(raised.exception)), managed_exec_tests.DIAGNOSTIC_LIMIT + 512
            )
        finally:
            if fixture_root is None and commands:
                fixture_root = Path(
                    commands[0][commands[0].index("--state-dir") + 1]
                ).parent
            if fixture_root is not None and fixture_root.exists():
                shutil.rmtree(fixture_root)

    def test_public_acceptance_rejects_malformed_outcome_packet(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(
                RuntimeError, "outcome has unexpected typed fields"
            ):
                self._run_acceptance(Path(temporary), malformed_outcome=True)

    def test_public_acceptance_rejects_outcome_shape_mutations(self):
        for mutation in (
            "extra-top-level",
            "missing-top-level",
            "extra-outcome",
            "missing-outcome",
            "float-schema",
            "float-status",
        ):
            with (
                self.subTest(mutation=mutation),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError, "outcome has unexpected typed fields"
                ):
                    self._run_acceptance(Path(temporary), outcome_mutation=mutation)

    def test_public_acceptance_rejects_default_environment_mutations(self):
        valid = {
            "PATH": "/usr/sbin:/usr/bin:/sbin:/bin",
            "TERM": "linux",
            "HOME": "/nonexistent",
            "USER": "nobody",
            "LOGNAME": "nobody",
            "SHLVL": "1",
            "nvx_workload_uid": "65534",
        }
        required_names = ("PATH", "TERM", "HOME", "USER", "LOGNAME")
        mutations = [
            *[
                (f"wrong-{name}", {**valid, name: f"wrong-{value}"})
                for name, value in valid.items()
                if name in required_names
            ],
            *[
                (
                    f"missing-{missing}",
                    {name: value for name, value in valid.items() if name != missing},
                )
                for missing in required_names
            ],
            *[
                (f"leaked-{name}", {**valid, name: "leaked"})
                for name in (
                    "EMPTY",
                    "COMPLEX",
                    "SECOND",
                    "ORDER",
                    "NVX_EXEC_CONFIG_FD",
                )
            ],
        ]
        for mutation, environment in mutations:
            output = "".join(
                f"{name}={value}\n" for name, value in environment.items()
            ).encode()
            with (
                self.subTest(mutation=mutation),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError, "did not match workload defaults"
                ):
                    self._run_acceptance(Path(temporary), default_environment=output)

    def test_public_acceptance_allows_unrelated_bootstrap_environment(self):
        environment = (
            b"PATH=/usr/sbin:/usr/bin:/sbin:/bin\n"
            b"TERM=linux\nHOME=/nonexistent\nUSER=nobody\nLOGNAME=nobody\n"
            b"SHLVL=1\nnvx_layer=distro\nnvx_workload_uid=65534\n"
            b"nvx_workload_gid=65534\nnvx_hostname=nvx\n"
        )
        with tempfile.TemporaryDirectory() as temporary:
            self._run_acceptance(Path(temporary), default_environment=environment)

    def test_evidence_failure_still_deprovisions_stopped_sandbox(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            commands: list[list[str]] = []
            with self.assertRaisesRegex(RuntimeError, "injected evidence failure"):
                self._run_acceptance(
                    root,
                    evidence_failure=True,
                    command_log=commands,
                )
            records = json.loads(
                (root / "results" / "public-exec-checks.json").read_text(
                    encoding="utf-8"
                )
            )

        self.assertIn("deprovision", [command[3] for command in commands])
        self.assertEqual(records[-1]["operation"], "deprovision")
        fixture_root = Path(commands[0][commands[0].index("--state-dir") + 1]).parent
        self.assertFalse(fixture_root.exists())

    def test_failed_start_deprovisions_safely_stopped_sandbox(self):
        with tempfile.TemporaryDirectory() as temporary:
            commands: list[list[str]] = []
            with self.assertRaisesRegex(RuntimeError, "start failed"):
                self._run_acceptance(
                    Path(temporary),
                    start_returncode=9,
                    command_log=commands,
                )

        self.assertIn("deprovision", [command[3] for command in commands])
        fixture_root = Path(commands[0][commands[0].index("--state-dir") + 1]).parent
        self.assertFalse(fixture_root.exists())

    def test_failed_stop_attempts_guarded_deprovision(self):
        with tempfile.TemporaryDirectory() as temporary:
            commands: list[list[str]] = []
            with self.assertRaisesRegex(
                RuntimeError,
                "stop failed.*sandbox must be stopped before deprovision.*"
                "managed fixture preserved for recovery",
            ) as raised:
                self._run_acceptance(
                    Path(temporary),
                    stop_returncode=9,
                    command_log=commands,
                )

        self.assertIn("deprovision", [command[3] for command in commands])
        fixture_root = Path(commands[0][commands[0].index("--state-dir") + 1]).parent
        self.assertIn(str(fixture_root), str(raised.exception))
        self.assertTrue((fixture_root / "state" / "openvmm.log").is_file())
        self.assertTrue((fixture_root / "scratch.ext4").is_file())
        shutil.rmtree(fixture_root)
        self.assertFalse(fixture_root.exists())


class GuestIdentityScriptTests(unittest.TestCase):
    def test_guest_identity_checks_fail_before_success_markers(self):
        descriptor = microvm_tests.guest_descriptor("ubuntu")
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm"],
            ),
            patch.object(microvm_tests, "run_guest_script") as run_guest_script,
        ):
            for runner in (
                microvm_tests.run_guest_boot,
                microvm_tests.run_guest_identity,
            ):
                with self.subTest(runner=runner.__name__):
                    runner(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        descriptor,
                        memory_mib=256,
                        timeout=60,
                        log_path=Path("guest.log"),
                    )
                    self.assertTrue(
                        run_guest_script.call_args.args[1].startswith("set -e\n")
                    )
                    run_guest_script.reset_mock()


def _create_wsl_symlink(path: Path, target: str) -> None:
    """Create the WSL-style link that OpenVMM stores for a guest on Windows."""
    if sys.platform != "win32":
        raise unittest.SkipTest("WSL-style links exist only on Windows")
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateFileW.restype = wintypes.HANDLE
    kernel32.CreateFileW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.HANDLE,
    ]
    kernel32.DeviceIoControl.restype = wintypes.BOOL
    kernel32.DeviceIoControl.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
        wintypes.LPVOID,
    ]
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    generic_write = 0x40000000
    create_new = 1
    open_reparse_point = 0x00200000
    fsctl_set_reparse_point = 0x000900A4
    handle = kernel32.CreateFileW(
        str(path), generic_write, 0, None, create_new, open_reparse_point, None
    )
    if handle in (None, wintypes.HANDLE(-1).value):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        # Version 2 of the LX link layout stores the UTF-8 target after a
        # 32-bit version field.
        data = (2).to_bytes(4, "little") + target.encode()
        reparse = (
            microvm_tests.IO_REPARSE_TAG_LX_SYMLINK.to_bytes(4, "little")
            + len(data).to_bytes(2, "little")
            + bytes(2)
            + data
        )
        buffer = ctypes.create_string_buffer(reparse, len(reparse))
        returned = wintypes.DWORD()
        if not kernel32.DeviceIoControl(
            handle,
            fsctl_set_reparse_point,
            buffer,
            len(reparse),
            None,
            0,
            ctypes.byref(returned),
            None,
        ):
            raise ctypes.WinError(ctypes.get_last_error())
    finally:
        kernel32.CloseHandle(handle)


class GuestSymlinkTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.target = self.root / "target"
        self.target.write_text("host data\n", encoding="utf-8")

    @unittest.skipIf(sys.platform == "win32", "Windows hosts store WSL-style links")
    def test_posix_link_must_keep_the_exact_target(self):
        link = self.root / "link"
        link.symlink_to("../target")
        microvm_tests.assert_guest_symlink(link, "../target")

        with self.assertRaisesRegex(RuntimeError, "does not point to"):
            microvm_tests.assert_guest_symlink(link, "target")
        with self.assertRaisesRegex(RuntimeError, "does not point to"):
            microvm_tests.assert_guest_symlink(self.target, "target")

    @unittest.skipUnless(sys.platform == "win32", "WSL-style links exist on Windows")
    def test_windows_link_must_be_an_inert_wsl_link(self):
        link = self.root / "link"
        _create_wsl_symlink(link, "../target")
        self.assertEqual(microvm_tests.read_wsl_symlink(link), b"../target")
        microvm_tests.assert_guest_symlink(link, "../target")

        with self.assertRaisesRegex(RuntimeError, "does not point to"):
            microvm_tests.assert_guest_symlink(link, "target")
        with self.assertRaisesRegex(RuntimeError, "not a WSL-style link"):
            microvm_tests.assert_guest_symlink(self.target, "target")
        followable = self.root / "followable"
        try:
            followable.symlink_to(self.target)
        except OSError as error:
            self.skipTest(f"NT symbolic links are unavailable: {error}")
        with self.assertRaisesRegex(RuntimeError, "not a WSL-style link"):
            microvm_tests.assert_guest_symlink(followable, str(self.target))


class ControlSessionTests(unittest.TestCase):
    def test_named_pipe_connect_retries_transient_invalid_argument(self):
        error = OSError(control_session.errno.EINVAL, "Invalid argument")
        with (
            patch.object(
                control_session.os, "open", side_effect=[error, 123]
            ) as open_pipe,
            patch.object(
                control_session.time,
                "monotonic",
                side_effect=[0.0, 0.0],
            ),
            patch.object(control_session.time, "sleep") as sleep,
        ):
            stream = control_session._NamedPipeStream.connect(
                Path(r"\\.\pipe\nvx-test"),
                1.0,
            )

        self.assertEqual(stream._fd, 123)
        self.assertEqual(open_pipe.call_count, 2)
        sleep.assert_called_once_with(0.025)


class MicrovmTests(unittest.TestCase):
    def test_host_loopback_listener_pair_retries_protocol_port_conflict(self):
        first_udp = MagicMock()
        first_udp.getsockname.return_value = ("127.0.0.1", 50000)
        first_tcp = MagicMock()
        first_tcp.bind.side_effect = PermissionError("TCP port is excluded")
        second_udp = MagicMock()
        second_udp.getsockname.return_value = ("127.0.0.1", 50001)
        second_tcp = MagicMock()

        with patch.object(
            microvm_tests.socket,
            "socket",
            side_effect=[first_udp, first_tcp, second_udp, second_tcp],
        ) as create_socket:
            tcp_listener, udp_listener = microvm_tests._bind_tcp_udp_listener_pair(
                5.0, microvm_tests.NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS
            )

        self.assertIs(tcp_listener, second_tcp)
        self.assertIs(udp_listener, second_udp)
        self.assertEqual(create_socket.call_count, 4)
        first_udp.bind.assert_called_once_with(("127.0.0.1", 0))
        first_tcp.bind.assert_called_once_with(("0.0.0.0", 50000))
        first_tcp.close.assert_called_once_with()
        first_udp.close.assert_called_once_with()
        second_udp.bind.assert_called_once_with(("127.0.0.1", 0))
        second_tcp.bind.assert_called_once_with(("0.0.0.0", 50001))
        second_tcp.listen.assert_called_once_with(1)
        second_tcp.settimeout.assert_called_once_with(5.0)
        second_udp.settimeout.assert_called_once_with(
            microvm_tests.NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS
        )

    def test_host_loopback_rejections_cover_generic_allow_and_explicit_denial(self):
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(microvm_tests, "OpenvmmProcess") as process:
                process.return_value.__enter__.return_value.wait.side_effect = [
                    openvmm_process.OpenvmmProcessResult(2, message)
                    for message in (
                        b"does not support generic host-loopback connectivity",
                        b"does not support generic host-loopback connectivity",
                        b"--host-loopback-forward requires explicit --host-loopback allow",
                        b"must match the guest gateway",
                    )
                ]
                microvm_tests.run_host_loopback_rejections(
                    Path("openvmm"),
                    Path("kernel"),
                    Path("initrd"),
                    "whp",
                    memory_mib=128,
                    timeout=40,
                    output_dir=Path(temporary),
                )
        commands = [call.args[0] for call in process.call_args_list]
        self.assertEqual(len(commands), 4)
        self.assertEqual(
            [command[command.index("--host-loopback") + 1] for command in commands],
            ["allow", "allow", "deny", "deny"],
        )
        for command in commands[:2]:
            self.assertNotIn("--host-loopback-forward", command)
            self.assertIn("--pidfile", command)
        self.assertIn("--network-proxy", commands[1])
        self.assertIn("--host-loopback-forward", commands[2])

    def test_host_loopback_rejection_requires_diagnostic_and_no_boot(self):
        diagnostic = b"does not support generic host-loopback connectivity"
        for result in (
            openvmm_process.OpenvmmProcessResult(0, diagnostic),
            openvmm_process.OpenvmmProcessResult(2, b"failed to create pidfile"),
            openvmm_process.OpenvmmProcessResult(
                2, diagnostic + b"\n" + microvm_tests.BOOT_MARKER
            ),
        ):
            with self.subTest(result=result):
                with tempfile.TemporaryDirectory() as temporary:
                    with patch.object(microvm_tests, "OpenvmmProcess") as process:
                        process.return_value.__enter__.return_value.wait.return_value = result
                        with self.assertRaisesRegex(RuntimeError, "before boot"):
                            microvm_tests.run_host_loopback_rejections(
                                Path("openvmm"),
                                Path("kernel"),
                                Path("initrd"),
                                "whp",
                                memory_mib=128,
                                timeout=40,
                                output_dir=Path(temporary),
                            )

    def test_host_loopback_scenario_detects_udp_proxy_leak(self):
        def send_forbidden_udp(
            command: list[str], script: str, *_args: object, **_kwargs: object
        ):
            if "--host-loopback" not in command:
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
                    ports = {
                        int(line.split()[5])
                        for line in script.splitlines()
                        if line.strip().startswith("nc -u")
                    }
                    for port in ports:
                        sender.sendto(
                            b"NVX-HOST-LOOPBACK-UDP-CONTROL",
                            ("127.0.0.1", port),
                        )
                return
            self.assertEqual(command[command.index("--network-egress") + 1], "allow")
            self.assertEqual(command[command.index("--host-loopback") + 1], "deny")
            proxy = command[command.index("--network-proxy") + 1]
            port = int(proxy.rsplit(":", 1)[1])
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
                sender.sendto(b"forbidden-proxy-udp", ("127.0.0.1", port))

        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "_http_server"),
                patch.object(
                    microvm_tests, "run_guest_script", side_effect=send_forbidden_udp
                ),
            ):
                with self.assertRaisesRegex(RuntimeError, "reached a host UDP service"):
                    microvm_tests.run_host_loopback_policy(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        memory_mib=128,
                        timeout=40,
                        output_dir=Path(temporary),
                    )

    def test_host_loopback_scenario_requires_observed_udp_control(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "_http_server"),
                patch.object(microvm_tests, "run_guest_script") as guest,
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(microvm_tests.socket, "create_connection"),
                patch.object(microvm_tests.time, "sleep"),
                patch.object(microvm_tests, "run_host_loopback_rejections"),
            ):
                process.return_value.__enter__.return_value.wait.return_value = (
                    openvmm_process.OpenvmmProcessResult(0, b"")
                )
                with self.assertRaisesRegex(RuntimeError, "UDP positive control"):
                    microvm_tests.run_host_loopback_policy(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        memory_mib=128,
                        timeout=5,
                        output_dir=Path(temporary),
                    )
                self.assertEqual(guest.call_count, 1)
                self.assertNotIn("--host-loopback", guest.call_args.args[0])
                process.assert_not_called()

    def test_host_loopback_udp_probes_reject_failed_sender(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        for mode, status, expected in (
            ("udp-control", 1, 99),
            ("udp-control", 127, 99),
            ("deny", 126, 99),
            ("deny", 127, 99),
            ("deny", 1, 0),
        ):
            with self.subTest(mode=mode, status=status):
                script = microvm_tests._render_script(
                    "host-loopback-policy.sh.in",
                    MODE=mode,
                    GATEWAY_IPV4="10.0.0.1",
                    GENERAL_PORT="8444",
                    PROXY_PORT="8443",
                    GUEST_PORT="0",
                ).replace("nvx-exit", "nvx_exit")
                result = subprocess.run(
                    [shell, "-s"],
                    input=(
                        f"nc() {{ return {status}; }}\n"
                        'wget() { case "$*" in\n'
                        "*/general) return 1 ;;\n"
                        "*/proxy) echo NVX-HOST-LOOPBACK-PROXY ;;\n"
                        "esac; }\n"
                        'nvx_exit() { exit "$1"; }\n' + script
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )
                self.assertEqual(
                    result.returncode, expected, result.stdout + result.stderr
                )
                self.assertNotIn("UDP-CONTROL-OK", result.stdout)
                if expected:
                    self.assertNotIn("DENY-OK", result.stdout)

    def test_host_loopback_script_probes_udp_on_proxy_and_general_ports(self):
        script = microvm_tests._render_script(
            "host-loopback-policy.sh.in",
            MODE="deny",
            GATEWAY_IPV4="10.0.0.1",
            GENERAL_PORT="8444",
            PROXY_PORT="8443",
            GUEST_PORT="0",
        )
        self.assertIn("nc -u -w 1 10.0.0.1 8443", script)
        self.assertIn("nc -u -w 1 10.0.0.1 8444", script)
        self.assertIn("http://10.0.0.1:8443/proxy", script)

    @staticmethod
    def _outcome_report(
        backend: str,
        *,
        outcome: dict[str, object],
        policy: dict[str, object],
    ) -> dict[str, object]:
        return {
            "schema_version": 1,
            "instance_id": "11" * 16,
            "backend": backend,
            "outcome": outcome,
            "network_policy": policy,
            "teardown": {name: True for name in microvm_tests.OUTCOME_TEARDOWN_FIELDS},
        }

    def test_workload_identity_scenario_checks_enforcement_and_rejection(self):
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(microvm_tests, "OpenvmmProcess") as process:
                process.return_value.__enter__.return_value.wait.side_effect = [
                    openvmm_process.OpenvmmProcessResult(
                        0, microvm_tests.WORKLOAD_IDENTITY_MARKER + b"\n"
                    ),
                    openvmm_process.OpenvmmProcessResult(
                        125, b"configured workload UID is unavailable\n"
                    ),
                    openvmm_process.OpenvmmProcessResult(
                        2, b"microVM workload UID must be nonzero\n"
                    ),
                ]
                microvm_tests.run_workload_identity(
                    Path("openvmm"),
                    Path("kernel"),
                    Path("initrd"),
                    "whp",
                    memory_mib=128,
                    timeout=40,
                    output_dir=Path(temporary),
                )

        self.assertEqual(process.call_count, 3)
        commands = [call.args[0] for call in process.call_args_list]
        self.assertEqual(
            [
                command[command.index("--microvm-workload-identity") + 1]
                for command in commands
            ],
            ["65534:65534", "12345:12345", "0:0"],
        )
        self.assertTrue(
            all(
                "nvx_exec=/sbin/nvx-identity-probe"
                in command[command.index("--cmdline") + 1]
                for command in commands
            )
        )

    def test_structured_outcome_scenario_covers_exit_policy_and_rejection(self):
        applied = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "guest-exit",
                "status_code": 37,
            },
            policy={
                "status": "applied",
                "status_code": 0,
                "mode": "rules",
                "allow_rule_count": 2,
                "deny_rule_count": 1,
                "host_loopback": "deny",
            },
        )
        rejected = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "vmm-failure",
                "status_code": 1,
            },
            policy={
                "status": "failed",
                "status_code": 1,
                "mode": "rules",
                "allow_rule_count": 1,
                "deny_rule_count": 0,
                "host_loopback": "allow",
            },
        )
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(
                    microvm_tests,
                    "_read_outcome_report",
                    side_effect=(applied, rejected),
                ),
            ):
                active = process.return_value.__enter__.return_value
                active.wait.side_effect = (
                    openvmm_process.OpenvmmProcessResult(
                        37, b"sensitive-output-value\n"
                    ),
                    openvmm_process.OpenvmmProcessResult(
                        2, b"--network-egress is required\n"
                    ),
                )
                microvm_tests.run_structured_outcome(
                    Path("openvmm"),
                    Path("kernel"),
                    Path("initrd"),
                    "whp",
                    memory_mib=128,
                    timeout=40,
                    output_dir=Path(temporary),
                )

        self.assertEqual(process.call_count, 2)
        applied_command = process.call_args_list[0].args[0]
        self.assertIn("--microvm-report", applied_command)
        self.assertEqual(
            applied_command.count("--network-egress-allow"),
            2,
        )
        self.assertEqual(
            applied_command.count("--network-egress-deny"),
            1,
        )
        self.assertEqual(
            applied_command[applied_command.index("--host-loopback") + 1],
            "deny",
        )
        rejected_command = process.call_args_list[1].args[0]
        self.assertNotIn("--network-egress", rejected_command)
        self.assertIn("--network-egress-allow", rejected_command)
        active.send_line.assert_called_once_with(
            "printf 'sensitive-output-value\\n'; /sbin/nvx-exit 37"
        )

    def test_structured_outcome_rejects_boolean_schema_version(self):
        report = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "guest-exit",
                "status_code": 0,
            },
            policy={
                "status": "applied",
                "status_code": 0,
                "mode": "rules",
                "allow_rule_count": 0,
                "deny_rule_count": 0,
                "host_loopback": "deny",
            },
        )
        report["schema_version"] = True
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "outcome.json"
            path.write_text(json.dumps(report), encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "unsupported version"):
                microvm_tests._read_outcome_report(path)

    def test_structured_outcome_preserves_invalid_primary_report(self):
        report = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "guest-exit",
                "status_code": 37,
            },
            policy={
                "status": "applied",
                "status_code": 0,
                "mode": "rules",
                "allow_rule_count": 2,
                "deny_rule_count": 1,
                "host_loopback": "deny",
            },
        )
        teardown = cast(dict[str, object], report["teardown"])
        teardown[next(iter(teardown))] = False
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            with (
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(
                    microvm_tests,
                    "_read_outcome_report",
                    return_value=report,
                ),
            ):
                active = process.return_value.__enter__.return_value
                active.wait.return_value = openvmm_process.OpenvmmProcessResult(
                    37, b"sensitive-output-value\n"
                )
                with self.assertRaisesRegex(RuntimeError, "incomplete teardown"):
                    microvm_tests.run_structured_outcome(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        memory_mib=128,
                        timeout=40,
                        output_dir=output,
                    )
            preserved = json.loads(
                (output / "structured-outcome.json").read_text(encoding="utf-8")
            )
        self.assertEqual(preserved, report)

    def test_structured_outcome_preserves_invalid_rejection_report(self):
        applied = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "guest-exit",
                "status_code": 37,
            },
            policy={
                "status": "applied",
                "status_code": 0,
                "mode": "rules",
                "allow_rule_count": 2,
                "deny_rule_count": 1,
                "host_loopback": "deny",
            },
        )
        rejected = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "vmm-failure",
                "status_code": 1,
            },
            policy={
                "status": "failed",
                "status_code": 1,
                "mode": "rules",
                "allow_rule_count": 1,
                "deny_rule_count": 0,
                "host_loopback": "allow",
            },
        )
        teardown = cast(dict[str, object], rejected["teardown"])
        teardown[next(iter(teardown))] = False
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            with (
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(
                    microvm_tests,
                    "_read_outcome_report",
                    side_effect=(applied, rejected),
                ),
            ):
                active = process.return_value.__enter__.return_value
                active.wait.side_effect = (
                    openvmm_process.OpenvmmProcessResult(
                        37, b"sensitive-output-value\n"
                    ),
                    openvmm_process.OpenvmmProcessResult(
                        2, b"--network-egress is required\n"
                    ),
                )
                with self.assertRaisesRegex(RuntimeError, "leaked host resources"):
                    microvm_tests.run_structured_outcome(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        memory_mib=128,
                        timeout=40,
                        output_dir=output,
                    )
            preserved = json.loads(
                (output / "structured-outcome-rejected.json").read_text(
                    encoding="utf-8"
                )
            )
        self.assertEqual(preserved, rejected)

    def test_console_exit_preserves_full_output_and_guest_status(self):
        expected = (
            b"x" * microvm_tests.CONSOLE_EXIT_PAYLOAD_BYTES
            + b"\n"
            + microvm_tests.CONSOLE_EXIT_COMPLETION_MARKER
            + b"\n"
        )
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "capture_snapshot") as capture,
                patch.object(microvm_tests, "OpenvmmProcess") as process,
            ):
                process.return_value.__enter__.return_value.wait.side_effect = [
                    openvmm_process.OpenvmmProcessResult(
                        code, expected.replace(b"\n", b"\r\n")
                    )
                    for code in (0, 37)
                ]
                microvm_tests.run_console_exit(
                    Path("openvmm"),
                    Path("kernel"),
                    Path("initrd"),
                    "mshv",
                    2,
                    memory_mib=512,
                    timeout=40,
                    output_dir=Path(temporary),
                )

        self.assertEqual(capture.call_count, 2)
        self.assertEqual(process.call_count, 2)
        for call, code in zip(capture.call_args_list, (0, 37), strict=True):
            self.assertEqual(call.kwargs["processors"], 2)
            self.assertIn("head -c 65536", call.kwargs["post_restore_script"])
            self.assertIn(
                f"/sbin/nvx-exit {code}\n", call.kwargs["post_restore_script"]
            )
        for call in process.call_args_list:
            self.assertEqual(call.kwargs["output_read_delay"], 2.0)
            self.assertIn("--restore-snapshot", call.args[0])
            self.assertEqual(call.args[0][call.args[0].index("--processors") + 1], "2")

    def test_console_exit_rejects_truncation_even_if_marker_survives(self):
        marker = b"\n" + microvm_tests.CONSOLE_EXIT_COMPLETION_MARKER + b"\n"
        for output in (
            marker,
            b"x" * (microvm_tests.CONSOLE_EXIT_PAYLOAD_BYTES - 1) + marker,
            b"y" * microvm_tests.CONSOLE_EXIT_PAYLOAD_BYTES + marker,
            b"x" * microvm_tests.CONSOLE_EXIT_PAYLOAD_BYTES,
        ):
            with self.subTest(length=len(output)):
                with tempfile.TemporaryDirectory() as temporary:
                    with (
                        patch.object(microvm_tests, "capture_snapshot"),
                        patch.object(microvm_tests, "OpenvmmProcess") as process,
                    ):
                        process.return_value.__enter__.return_value.wait.return_value = openvmm_process.OpenvmmProcessResult(
                            0, output
                        )
                        with self.assertRaisesRegex(
                            RuntimeError, "truncated or corrupt console output"
                        ):
                            microvm_tests.run_console_exit(
                                Path("openvmm"),
                                Path("kernel"),
                                Path("initrd"),
                                "kvm",
                                2,
                                memory_mib=128,
                                timeout=40,
                                output_dir=Path(temporary),
                            )

    def test_console_exit_rejects_wrong_guest_status(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "capture_snapshot"),
                patch.object(microvm_tests, "OpenvmmProcess") as process,
            ):
                process.return_value.__enter__.return_value.wait.return_value = (
                    openvmm_process.OpenvmmProcessResult(1, b"")
                )
                with self.assertRaisesRegex(
                    RuntimeError, "expected exit status 0, got 1"
                ):
                    microvm_tests.run_console_exit(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        2,
                        memory_mib=128,
                        timeout=40,
                        output_dir=Path(temporary),
                    )

    def test_output_read_delay_precedes_reader_start(self):
        events: list[tuple[str, float | None]] = []

        def record_delay(delay: float) -> None:
            events.append(("delay", delay))

        def record_reader_start() -> None:
            events.append(("reader", None))

        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread") as thread,
                patch.object(
                    openvmm_process.time,
                    "sleep",
                    side_effect=record_delay,
                ),
            ):
                interaction.return_value.process.poll.return_value = 0
                thread.return_value.start.side_effect = record_reader_start
                with openvmm_process.OpenvmmProcess(
                    ["openvmm"],
                    Path(temporary) / "output.log",
                    output_read_delay=2,
                ):
                    pass
        self.assertEqual(events, [("delay", 2), ("reader", None)])

    def test_negative_output_read_delay_does_not_start_process(self):
        with patch.object(openvmm_process, "InteractiveProcess") as interaction:
            with self.assertRaisesRegex(ValueError, "cannot be negative"):
                openvmm_process.OpenvmmProcess(
                    ["openvmm"], Path("unused.log"), output_read_delay=-1
                )
        interaction.assert_not_called()

    def test_snapshot_restore_uses_batched_port_io_and_zero_expansion_path(self):
        snapshot = (
            Path(__file__).parents[1] / "guest" / "common" / "nvx-snapshot"
        ).read_text(encoding="utf-8")

        self.assertIn(
            '/sbin/nvx-port-io read-restore-packet 233 234 "$restore_packet"',
            snapshot,
        )
        self.assertIn(
            "generation_id=$(/sbin/nvx-port-io read-generation-id 233 234)",
            snapshot,
        )
        self.assertIn(
            'generation_id=$(/sbin/nvx-reseed "$entropy" "$generation_id")',
            snapshot,
        )
        self.assertIn(
            '/sbin/nvx-reseed --generation-only "$entropy" "$generation_id"',
            snapshot,
        )
        self.assertIn('export NVX_VM_GENERATION_ID="$generation_id"', snapshot)
        self.assertNotIn("dd if=/dev/port", snapshot)
        self.assertNotIn("dd of=/dev/port", snapshot)
        self.assertIn('[ "$range_count" -eq 0 ]', snapshot)
        self.assertIn("RESTORE_MEMORY_EXPANSION_AVAILABLE=16", snapshot)
        self.assertIn('console_status "NVX-SNAPSHOT-ERROR: $*"', snapshot)
        for stage in ("packet", "entropy", "identity", "runtime-hook", "acknowledge"):
            self.assertIn(
                f'console_status "NVX-POST-RESTORE-STAGE: {stage}"',
                snapshot,
            )
        self.assertIn(
            '"NVX-MEMORY-ONLINE-OK: added_bytes=0 '
            'memtotal_kib=$memtotal_kib elapsed_us=0"',
            snapshot,
        )
        zero_expansion_fast_path = snapshot.index(
            "[ $((restore_status & RESTORE_MEMORY_EXPANSION_AVAILABLE)) -eq 0 ]"
        )
        packet_restore = snapshot.index("    post_restore\n")
        self.assertLess(zero_expansion_fast_path, packet_restore)

    def test_snapshot_console_diagnostics_are_nonfatal_and_ordered(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        snapshot = (
            Path(__file__).parents[1] / "guest" / "common" / "nvx-snapshot"
        ).read_text(encoding="utf-8")

        markers = [
            'console_status "NVX-POST-RESTORE-STAGE: packet"',
            'console_status "NVX-POST-RESTORE-STAGE: entropy"',
            'console_status "NVX-POST-RESTORE-STAGE: identity"',
            'console_status "NVX-POST-RESTORE-STAGE: runtime-hook"',
            'console_status "NVX-POST-RESTORE-STAGE: acknowledge"',
        ]
        positions = [snapshot.index(marker) for marker in markers]
        self.assertEqual(positions, sorted(positions))

        functions_start = snapshot.index("console_status() {")
        functions_end = snapshot.index("\n}\n\ncleanup()", functions_start) + 3
        functions = snapshot[functions_start:functions_end]
        functions = functions.replace(">/dev/console", '>"$console_target"')
        functions = functions.replace("/sbin/nvx-exit", "nvx_exit")

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            console_target = root / "console-directory"
            console_target.mkdir()
            exit_record = root / "exit-record"
            result = subprocess.run(
                [shell, "-s", "--", str(console_target), str(exit_record)],
                input=(
                    "set -eu\n"
                    "console_target=$1\n"
                    "exit_record=$2\n"
                    "post_restore_pending=false\n"
                    'nvx_exit() { printf "%s\\n" "$1" >"$exit_record"; }\n'
                    f"{functions}\n"
                    'console_status "unavailable console is non-fatal"\n'
                    'fail_closed "synthetic restore failure"\n'
                ),
                text=True,
                capture_output=True,
                timeout=5,
                check=False,
            )

            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertEqual(exit_record.read_text(encoding="ascii"), "1\n")
            self.assertEqual(
                result.stderr,
                "nvx-snapshot: synthetic restore failure; terminating the VM\n",
            )

    def test_console_log_persists_buffered_and_completed_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            connection, peer = socket.socketpair()
            console = openvmm_process.TcpConsole(connection)
            peer.sendall(b"failure diagnostic\n")
            peer.close()

            failure_log = root / "failure.log"
            output = microvm_tests._persist_console_log(console, b"", failure_log)
            self.assertEqual(output, b"failure diagnostic\n")
            self.assertEqual(failure_log.read_bytes(), output)

            success_log = root / "success.log"
            output = microvm_tests._persist_console_log(
                None,
                b"completed output\n",
                success_log,
            )
            self.assertEqual(output, b"completed output\n")
            self.assertEqual(success_log.read_bytes(), output)

            connection_failure_log = root / "connection-failure.log"
            output = microvm_tests._persist_console_log(
                None,
                b"",
                connection_failure_log,
            )
            self.assertEqual(output, b"")
            self.assertEqual(connection_failure_log.read_bytes(), b"")

    def test_process_wait_reads_final_chunks_after_process_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = 0
                interaction.return_value.process.wait.return_value = 0
                queues.return_value.get.side_effect = [
                    b"BEGIN-",
                    queue.Empty,
                    b"END\n",
                    None,
                ]
                queues.return_value.get_nowait.side_effect = queue.Empty
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process:
                    result = process.wait(1)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.output, b"BEGIN-END\n")
                self.assertEqual(log_path.read_bytes(), result.output)
                self.assertEqual(queues.return_value.get.call_count, 4)

    def test_process_wait_clamps_elapsed_process_timeout(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
                patch.object(
                    openvmm_process.time,
                    "monotonic",
                    side_effect=[0.0, 0.25, 2.0],
                ),
            ):
                interaction.return_value.process.poll.return_value = 0
                interaction.return_value.process.wait.return_value = 0
                queues.return_value.get.return_value = None
                queues.return_value.get_nowait.side_effect = queue.Empty
                with openvmm_process.OpenvmmProcess(
                    ["openvmm"], Path(temporary) / "output.log"
                ) as process:
                    process.wait(1.0)
                interaction.return_value.process.wait.assert_called_once_with(
                    timeout=0.0
                )

    def test_process_wait_for_accepts_marker_after_process_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = 0
                queues.return_value.get.side_effect = [
                    b"MAR",
                    queue.Empty,
                    b"KER\n",
                ]
                queues.return_value.get_nowait.side_effect = queue.Empty
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process:
                    process.wait_for(b"MARKER", 1)
                self.assertEqual(log_path.read_bytes(), b"MARKER\n")

    def test_process_wait_for_line_ignores_marker_inside_echoed_script(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = None
                queues.return_value.get.side_effect = [
                    b"echo NVX-READY\n",
                    b"NVX-READY\n",
                ]
                queues.return_value.get_nowait.side_effect = queue.Empty
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process:
                    process.wait_for_line(b"NVX-READY", 1)
                self.assertEqual(
                    log_path.read_bytes(),
                    b"echo NVX-READY\nNVX-READY\n",
                )

    def test_process_wait_bounds_missing_output_eof_after_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
                patch.object(
                    openvmm_process.time,
                    "monotonic",
                    side_effect=[0.0, 0.0, 1.0],
                ),
            ):
                interaction.return_value.process.poll.return_value = 0
                interaction.return_value.process.wait.return_value = 0
                queues.return_value.get.side_effect = queue.Empty
                queues.return_value.get_nowait.side_effect = queue.Empty
                with openvmm_process.OpenvmmProcess(
                    ["openvmm"], Path(temporary) / "output.log"
                ) as process:
                    with self.assertRaisesRegex(TimeoutError, "did not reach EOF"):
                        process.wait(0.5)

    def test_openvmm_process_preserves_buffered_sequential_markers(self):
        class FakeProcess:
            pid = 123
            returncode = 0

            def poll(self):
                return 0

            def wait(self, timeout: float | None = None):
                del timeout
                return 0

        class FakeInteraction:
            def __init__(self):
                self.process = FakeProcess()

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(b"FIRST\nSECOND\n")
                chunks.put(None)

            def write_input(self, _data: bytes) -> None:
                pass

            def close(self) -> None:
                pass

        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "process.log"
            with patch.object(
                openvmm_process,
                "InteractiveProcess",
                return_value=FakeInteraction(),
            ):
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process:
                    process.wait_for(b"FIRST", 1)
                    process.wait_for(b"SECOND", 1)
                    result = process.wait(1)

            self.assertEqual(result.returncode, 0)
            self.assertEqual(log_path.read_bytes(), b"FIRST\nSECOND\n")

    def test_tcp_console_line_marker_ignores_echoed_command(self):
        connection, peer = socket.socketpair()
        console = openvmm_process.TcpConsole(connection)
        marker = b"NVX-CONSOLE-RX-READY"
        output = b"> echo " + marker + b"\r\n" + marker + b"\r\n"
        peer.sendall(output)

        console.wait_for_line(marker, 1.0)

        self.assertEqual(console.output, output)
        console.close()
        peer.close()

    def test_tcp_console_connect_closes_failed_connection(self):
        failed = MagicMock()
        failed.setsockopt.side_effect = OSError("configuration failed")
        connected = MagicMock()
        with patch.object(
            openvmm_process.socket,
            "create_connection",
            side_effect=(failed, connected),
        ):
            console = openvmm_process.TcpConsole.connect(("127.0.0.1", 1), 1.0)

        failed.close.assert_called_once_with()
        console.close()
        connected.close.assert_called_once_with()

    def test_snapshot_core_script_selects_backend_clocksource(self):
        kvm = microvm_tests._snapshot_core_script("kvm")
        whp = microvm_tests._snapshot_core_script("whp")
        mshv = microvm_tests._snapshot_core_script("mshv")

        self.assertIn("echo kvm-clock", kvm)
        self.assertIn('current_clocksource)" != tsc-early', whp)
        self.assertNotIn("@SELECT_CLOCKSOURCE@", mshv)
        self.assertIn("/sbin/nvx-reseed", mshv)
        self.assertIn("/sbin/nvx-reseed --sample", mshv)
        self.assertIn("NVX-SNAPSHOT-GENERATION-ID-", mshv)
        self.assertIn("NVX-SNAPSHOT-UUID-", mshv)
        self.assertIn("NVX-SNAPSHOT-TEMP-ID-", mshv)

    def test_smp_worker_requires_bounded_local_timer_progress(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        worker = (
            benchmark.smp_probe_script(2)
            .split("<<'NVX_SMP_WORKER'\n", 1)[1]
            .split("\nNVX_SMP_WORKER", 1)[0]
        )
        worker = worker.replace("/proc/interrupts", "interrupts").replace(
            "timer_attempts=10000", "timer_attempts=8"
        )
        for name, initial, advanced, advance_read, actual, status, reads in (
            ("frozen", "LOC: 100 100", "LOC: 100 100", 3, 1, 88, 9),
            ("other-cpu", "LOC: 100 100", "LOC: 101 100", 3, 1, 88, 9),
            ("delayed", "LOC: 100 100", "LOC: 100 101", 4, 1, 0, 4),
            ("last-attempt", "LOC: 100 100", "LOC: 100 101", 9, 1, 0, 9),
            ("too-late", "LOC: 100 100", "LOC: 100 101", 10, 1, 88, 9),
            ("backwards", "LOC: 100 100", "LOC: 100 99", 3, 1, 88, 9),
            ("missing", "RES: 1 1", "RES: 1 1", 0, 1, 87, 2),
            ("wrong-cpu", "LOC: 100 100", "LOC: 100 101", 3, 0, 87, 0),
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "interrupts").write_text(initial + "\n", encoding="ascii")
                result = subprocess.run(
                    [shell, "-s", "--", "1", "result", "1"],
                    cwd=root,
                    input=(
                        "loc_reads=0\n"
                        "trap 'echo NVX-LAPIC-READS-$loc_reads' EXIT\n"
                        f"awk() {{ echo {actual}; }}\n"
                        "read() {\n"
                        "    loc_reads=$((loc_reads + 1))\n"
                        f'    if [ "$loc_reads" -eq {advance_read} ]; then\n'
                        f"        printf '%s\\n' '{advanced}' >interrupts\n"
                        "    fi\n"
                        '    command read "$@"\n'
                        "}\n" + worker + "\n"
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )

                self.assertEqual(
                    result.returncode, status, result.stdout + result.stderr
                )
                self.assertIn(f"NVX-LAPIC-READS-{reads}\n", result.stdout)
                if status:
                    self.assertFalse((root / "result").exists())
                    self.assertIn(
                        "SMP-WORKER-FAIL" if name == "wrong-cpu" else "SMP-LAPIC-FAIL",
                        result.stdout,
                    )
                else:
                    self.assertEqual(
                        (root / "result").read_text(encoding="ascii").split(),
                        ["1", "101", "1"],
                    )

    def test_snapshot_core_whp_waits_for_stable_clocksource(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        clocksource_script = microvm_tests._snapshot_core_script("whp").split(
            "generation_id_before=", 1
        )[0]
        for ready_after, stable_source, expected_returncode, expected_waits in (
            (0, "tsc", 0, 0),
            (2, "refined-jiffies", 0, 2),
            (101, "tsc", 46, 100),
        ):
            with self.subTest(
                ready_after=ready_after,
                stable_source=stable_source,
            ):
                result = subprocess.run(
                    [shell, "-s"],
                    input=(
                        "clock_waits=0\n"
                        "cat() {\n"
                        f'    if [ "$clock_waits" -ge {ready_after} ]; then\n'
                        f"        echo {stable_source}\n"
                        "    else\n"
                        "        echo tsc-early\n"
                        "    fi\n"
                        "}\n"
                        "sleep() { clock_waits=$((clock_waits + 1)); }\n"
                        "nvx_exit() {\n"
                        '    echo "NVX-CLOCKSOURCE-WAITS-$clock_waits"\n'
                        '    exit "$1"\n'
                        "}\n"
                        + clocksource_script.replace("nvx-exit", "nvx_exit")
                        + 'echo "NVX-CLOCKSOURCE-WAITS-$clock_waits"\n'
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )

                self.assertEqual(
                    result.returncode,
                    expected_returncode,
                    result.stdout + result.stderr,
                )
                self.assertIn(
                    f"NVX-CLOCKSOURCE-WAITS-{expected_waits}\n", result.stdout
                )
                if expected_returncode:
                    self.assertIn("NVX-SNAPSHOT-CORE-FAIL code=46", result.stdout)

    def test_snapshot_core_waits_for_no_destination_marker_line_before_exit(self):
        events: list[tuple[str, bytes | str | None]] = []
        marker = b"NVX-SNAPSHOT-NO-DESTINATION-OK"

        class StopAfterNoDestination(Exception):
            pass

        class FakeProcess:
            def __enter__(self):
                return self

            def __exit__(
                self,
                _exception_type: type[BaseException] | None,
                _exception: BaseException | None,
                _traceback: object | None,
            ) -> None:
                return None

            def wait_for(self, expected: bytes, _timeout: float) -> None:
                events.append(("wait_for", expected))

            def wait_for_line(self, expected: bytes, _timeout: float) -> None:
                events.append(("wait_for_line", expected))

            def send_line(self, line: str) -> None:
                events.append(("send_line", line))

            def wait(self, _timeout: float) -> openvmm_process.OpenvmmProcessResult:
                events.append(("wait", None))
                return openvmm_process.OpenvmmProcessResult(0, marker + b"\n")

        with (
            patch.object(
                microvm_tests, "workload_boot_command", return_value=["openvmm"]
            ),
            patch.object(microvm_tests, "OpenvmmProcess", return_value=FakeProcess()),
            patch.object(
                microvm_tests.tempfile,
                "TemporaryDirectory",
                side_effect=StopAfterNoDestination,
            ),
            self.assertRaises(StopAfterNoDestination),
        ):
            microvm_tests.run_snapshot_core(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initramfs"),
                "whp",
                memory_mib=128,
                timeout=1,
                output_dir=Path("logs"),
            )

        self.assertEqual(
            events,
            [
                ("wait_for", microvm_tests.BOOT_MARKER),
                ("send_line", "nvx-snapshot; echo NVX-SNAPSHOT-NO-DESTINATION-OK"),
                ("wait_for_line", marker),
                ("send_line", "nvx-exit 0"),
                ("wait", None),
            ],
        )

    def test_snapshot_marker_parsers_require_single_well_formed_values(self):
        output = b"PREFIX-12\r\nPAIR-4-5\n"
        self.assertEqual(
            microvm_tests._single_marker_value(output, b"PREFIX-"),
            b"12",
        )
        self.assertEqual(
            microvm_tests._single_framed_marker_value(
                b"FRAME-17-END[kernel output]\n",
                b"FRAME-",
                b"-END",
            ),
            b"17",
        )
        self.assertEqual(
            microvm_tests._parse_marker_pair(output, b"PAIR-"),
            (4, 5),
        )
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            microvm_tests._single_marker_value(b"X-1\nX-2\n", b"X-")
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            microvm_tests._single_framed_marker_value(
                b"FRAME-17-ENDFRAME-34-END",
                b"FRAME-",
                b"-END",
            )
        with self.assertRaisesRegex(RuntimeError, "malformed"):
            microvm_tests._single_framed_marker_value(
                b"FRAME-17",
                b"FRAME-",
                b"-END",
            )
        with self.assertRaisesRegex(RuntimeError, "malformed"):
            microvm_tests._parse_marker_pair(b"PAIR-4\n", b"PAIR-")

    def test_console_snapshot_script_preserves_backend_specific_rx_and_tx(self):
        kvm, kvm_count, kvm_rx = microvm_tests._console_snapshot_script("kvm")
        mshv, mshv_count, mshv_rx = microvm_tests._console_snapshot_script("mshv")
        whp, whp_count, whp_rx = microvm_tests._console_snapshot_script("whp")

        self.assertEqual((kvm_count, mshv_count, whp_count), (10_000, 100, 1_000))
        self.assertEqual(kvm_rx, bytes((0, 1, 2, 127, 255)))
        self.assertEqual(whp_rx, kvm_rx)
        self.assertEqual(mshv_rx, b"NVX-CONSOLE-RX\n")
        self.assertIn("NVX-CONSOLE-RX-RESTORED", kvm)
        self.assertNotIn("NVX-CONSOLE-RX-RESTORED", mshv)
        self.assertIn("NVX-CONSOLE-TX-DONE", whp)
        self.assertIn("stty -F /dev/hvc1 raw -echo", whp)
        self.assertIn("nvx-console-pending /dev/hvc1", whp)
        self.assertIn(f'[ "$pending" -lt {len(whp_rx)} ]', whp)
        self.assertIn(f'[ "$pending" -lt {len(mshv_rx)} ]', mshv)
        self.assertIn('while [ "$snapshot_now" = 0 ]; do', whp)
        self.assertNotIn("sleep 1", whp)

    def test_console_snapshot_waits_until_rx_is_queued_before_snapshot(self):
        events: list[tuple[str, bytes] | tuple[str, bytes, float]] = []

        class RecordingConsole:
            def send_bytes(self, data: bytes) -> None:
                events.append(("send", data))

            def wait_for_line(self, marker: bytes, timeout: float) -> None:
                events.append(("wait", marker, timeout))

        console = cast(openvmm_process.TcpConsole, RecordingConsole())
        queued_rx = bytes((0, 1, 2, 127, 255))
        microvm_tests._send_console_rx_and_wait_until_queued(
            console,
            queued_rx,
            3.0,
        )

        self.assertEqual(
            events,
            [
                ("send", queued_rx),
                ("wait", microvm_tests.CONSOLE_RX_QUEUED_MARKER, 3.0),
            ],
        )

    def test_endpoint_policy_arguments_are_repeatable_and_ordered(self):
        command = ["openvmm"]
        microvm_tests._append_endpoint_policy(command, microvm_tests.ENDPOINT_POLICY)

        self.assertEqual(
            command,
            [
                "openvmm",
                "--allow-endpoint",
                "10.0.0.9:8443",
                "--allow-endpoint",
                "192.0.2.7:443",
                "--allow-endpoint",
                "10.0.0.9:443",
            ],
        )

    def test_network_snapshot_script_renders_host_ports(self):
        script = microvm_tests._render_script(
            "network-snapshot.sh.in",
            HTTP_PORT="1234",
            UDP_PORT="5678",
        )

        self.assertIn("10.0.0.1:1234/hold", script)
        self.assertIn("10.0.0.1 5678", script)
        self.assertNotIn("@HTTP_PORT@", script)
        self.assertNotIn("@UDP_PORT@", script)

    def test_snapshot_tier_scripts_preserve_tier_specific_policy(self):
        platform = microvm_tests._snapshot_tier_script("platform")
        workload = microvm_tests._snapshot_tier_script("workload-start")
        checkpoint = microvm_tests._snapshot_tier_script("instance-checkpoint")

        self.assertIn("/sbin/nvx-snapshot --tier platform", platform)
        self.assertIn("date -u -s 200001010000.00", platform)
        self.assertNotIn("mkfs.ext4", platform)
        self.assertIn("/sbin/nvx-snapshot --tier workload-start", workload)
        self.assertIn("mkfs.ext4 -F /dev/vdb", workload)
        self.assertIn("runtime-post-restore", workload)
        self.assertIn(": >/run/nvx/workload-ran", workload)
        self.assertIn("[ -e /run/nvx/workload-ran ]", workload)
        self.assertIn("/sbin/nvx-snapshot\n", checkpoint)
        self.assertIn("captured-workload-id", checkpoint)
        self.assertNotIn("@CAPTURE_ACTION@", checkpoint)

    def test_snapshot_tier_entry_points_reject_unsupported_tiers(self):
        with self.assertRaisesRegex(ValueError, "unsupported snapshot tier 'invalid'"):
            microvm_tests._snapshot_tier_script("invalid")
        with self.assertRaisesRegex(ValueError, "unsupported snapshot tier 'invalid'"):
            microvm_tests._run_snapshot_tier(
                "invalid",
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                memory_mib=128,
                timeout=60,
                output_dir=Path("logs"),
            )

    def test_snapshot_tier_runner_dispatches_all_tiers(self):
        with patch.object(microvm_tests, "_run_snapshot_tier") as run_tier:
            microvm_tests.run_snapshot_tiers(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                memory_mib=128,
                timeout=60,
                output_dir=Path("logs"),
            )

        self.assertEqual(
            [entry.args[0] for entry in run_tier.call_args_list],
            ["platform", "workload-start", "instance-checkpoint"],
        )

    def test_lifecycle_uses_one_vcpu_linux_guest(self):
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ) as boot_command,
            patch.object(microvm_tests, "run_guest_script") as run_guest_script,
        ):
            microvm_tests.run_lifecycle(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                memory_mib=128,
                timeout=45,
                log_path=Path("lifecycle.log"),
            )

        boot_command.assert_called_once_with(
            Path("openvmm"),
            "kvm",
            Path("vmlinux"),
            Path("initrd"),
            128,
            "quiet loglevel=0",
        )
        script = run_guest_script.call_args.args[1]
        self.assertIn("NVX-LIFECYCLE-OK", script)
        self.assertIn("/^LOC:/", script)
        self.assertEqual(
            run_guest_script.call_args.args[2],
            microvm_tests.LIFECYCLE_COMPLETION_MARKER,
        )
        self.assertEqual(run_guest_script.call_args.kwargs["timeout"], 45)
        self.assertEqual(
            run_guest_script.call_args.kwargs["log_path"],
            Path("lifecycle.log"),
        )

    def test_lifecycle_script_exits_on_unexpected_command_failure(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")

        script = microvm_tests._read_script("lifecycle.sh")
        prologue, separator, _ = script.partition("\nprintf '\\013' | dd")
        self.assertTrue(separator)
        prologue = prologue.replace("nvx-exit", "record_exit")
        result = subprocess.run(
            [shell],
            input=(
                "record_exit() { printf 'NVX-EXIT %s\\n' \"$1\"; }\n"
                f"{prologue}\n"
                "false\n"
            ),
            text=True,
            capture_output=True,
            timeout=5,
            check=False,
        )

        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stdout.splitlines(),
            ["NVX-LIFECYCLE-FAIL code=1", "NVX-EXIT 1"],
        )

    def test_smp_uses_requested_processor_count(self):
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ) as boot_command,
            patch.object(
                microvm_tests,
                "smp_probe_script",
                return_value="probe\n",
            ) as smp_probe_script,
            patch.object(microvm_tests, "run_guest_script") as run_guest_script,
        ):
            microvm_tests.run_smp(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "mshv",
                4,
                memory_mib=256,
                timeout=90,
                log_path=Path("smp-4.log"),
            )

        boot_command.assert_called_once_with(
            Path("openvmm"),
            "mshv",
            Path("vmlinux"),
            Path("initrd"),
            256,
            "quiet loglevel=0",
            processors=4,
        )
        smp_probe_script.assert_called_once_with(4)
        run_guest_script.assert_called_once_with(
            ["openvmm", "boot"],
            "probe\n",
            benchmark.SMP_PROBE_COMPLETION_MARKER,
            timeout=90,
            log_path=Path("smp-4.log"),
        )

    def test_smp_lapic_exercises_counting_timer_without_weakening_probe(self):
        with patch.object(microvm_tests, "run_guest_script") as run:
            microvm_tests.run_smp(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                4,
                memory_mib=128,
                timeout=60,
                log_path=Path("smp-lapic-4.log"),
                force_lapic_timer=True,
            )
        command, script, marker = run.call_args.args
        self.assertEqual(
            command[command.index("--cmdline") + 1],
            "quiet loglevel=0 lapic=notscdeadline",
        )
        self.assertEqual(script, benchmark.smp_probe_script(4))
        self.assertEqual(marker, benchmark.SMP_PROBE_COMPLETION_MARKER)

    def test_virtio_net_uses_portable_endpoint_policy(self):
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ) as boot_command,
            patch.object(microvm_tests, "run_guest_script") as run_guest_script,
        ):
            microvm_tests.run_virtio_net(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                memory_mib=128,
                timeout=60,
                log_path=Path("virtio-net.log"),
            )

        boot_command.assert_called_once_with(
            Path("openvmm"),
            "whp",
            Path("vmlinux"),
            Path("initrd"),
            128,
            "quiet loglevel=0",
            network="10.0.0.2/24",
        )
        command = run_guest_script.call_args.args[0]
        self.assertEqual(command.count("--allow-endpoint"), 3)
        self.assertIn("192.0.2.7:443", command)
        script = run_guest_script.call_args.args[1]
        self.assertIn("virtnet_ip=10.0.0.2", script)
        self.assertIn("10.0.0.10", script)
        self.assertEqual(
            run_guest_script.call_args.args[2],
            microvm_tests.VIRTIO_NET_COMPLETION_MARKER,
        )

    def test_directional_network_commands_map_generic_default_actions(self):
        with patch.object(
            microvm_tests,
            "workload_boot_command",
            side_effect=[["openvmm", "boot"], ["openvmm", "boot"]],
        ) as boot_command:
            allow = microvm_tests._directional_network_command(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                128,
                egress="allow",
                ingress="deny",
            )
            deny = microvm_tests._directional_network_command(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                128,
                egress="deny",
                ingress="deny",
            )

        self.assertEqual(boot_command.call_count, 2)
        self.assertEqual(
            allow[-4:],
            ["--network-egress", "allow", "--network-ingress", "deny"],
        )
        self.assertEqual(
            deny[-4:],
            ["--network-egress", "deny", "--network-ingress", "deny"],
        )
        with self.assertRaisesRegex(ValueError, "actions must be"):
            microvm_tests._directional_network_command(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                128,
                egress="block",
                ingress="deny",
            )

    def test_egress_port_reservation_closes_tcp_when_udp_fails(self):
        endpoints = [MagicMock(spec=socket.socket) for _ in range(5)]
        with (
            patch.object(
                microvm_tests,
                "_bind_consecutive_ports",
                side_effect=[endpoints, RuntimeError("UDP unavailable")],
            ),
            self.assertRaisesRegex(RuntimeError, "UDP unavailable"),
        ):
            microvm_tests.run_l3_l4_egress_policy(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initramfs"),
                "whp",
                memory_mib=128,
                timeout=1,
                output_dir=Path("."),
            )
        for endpoint in endpoints:
            endpoint.close.assert_called_once_with()

    def test_bounded_egress_acceptance_policy_lowers_ranges_and_exclusions(self):
        policy = microvm_tests._bounded_egress_policy(
            "192.0.2.1",
            (21001, 21002, 21003),
            (22001, 22002, 22003),
        )

        self.assertIn("192.0.2.0/24:tcp:21001", policy.allow)
        self.assertIn("192.0.2.0/24:tcp:21003", policy.allow)
        self.assertIn("192.0.2.0/24:udp:22001", policy.allow)
        self.assertIn("192.0.2.0/24:udp:22003", policy.allow)
        self.assertIn("192.0.2.0/24:tcp:21002", policy.deny)
        self.assertIn("192.0.2.0/24:udp:22002", policy.deny)
        self.assertNotIn("192.0.2.0/24:tcp:21000", policy.allow)
        self.assertNotIn("192.0.2.0/24:tcp:21004", policy.allow)

    def test_l3_l4_egress_acceptance_invokes_public_nvx_policy_file(self):
        class ImmediateThread:
            def __init__(
                self, *, target: Callable[[], None], **_kwargs: object
            ) -> None:
                self.target = target

            def start(self) -> None:
                self.target()

            def join(self, _timeout: float | None = None, **_kwargs: object) -> None:
                pass

            def is_alive(self) -> bool:
                return False

        for guest, memory_mib in (("alpine", 128), ("ubuntu", 256)):
            with self.subTest(guest=guest):
                tcp = [MagicMock(spec=socket.socket) for _ in range(5)]
                udp = [MagicMock(spec=socket.socket) for _ in range(5)]
                for index, endpoint in enumerate(tcp):
                    endpoint.getsockname.return_value = ("0.0.0.0", 21000 + index)
                for index, endpoint in enumerate(udp):
                    endpoint.getsockname.return_value = ("0.0.0.0", 22000 + index)
                for endpoint in (tcp[0], tcp[2], tcp[4]):
                    endpoint.accept.side_effect = TimeoutError
                for endpoint in (udp[0], udp[2], udp[4]):
                    endpoint.recvfrom.side_effect = TimeoutError
                for endpoint in (tcp[1], tcp[3]):
                    connection = MagicMock(spec=socket.socket)
                    connection.recv.return_value = b"GET /allowed HTTP/1.1\r\n\r\n"
                    endpoint.accept.return_value = (connection, ("127.0.0.1", 1))
                udp[1].recvfrom.return_value = (
                    b"NVX-L3-L4-UDP-ALLOW-START",
                    ("127.0.0.1", 1),
                )
                udp[3].recvfrom.return_value = (
                    b"NVX-L3-L4-UDP-ALLOW-END",
                    ("127.0.0.1", 1),
                )

                with (
                    tempfile.TemporaryDirectory() as temporary,
                    patch.object(
                        microvm_tests,
                        "_bind_egress_ports",
                        return_value=(tcp, udp),
                    ),
                    patch.object(microvm_tests, "run_guest_script") as run_guest_script,
                    patch.object(microvm_tests, "OpenvmmProcess") as openvmm_process,
                    patch.object(
                        microvm_tests.threading,
                        "Thread",
                        side_effect=ImmediateThread,
                    ),
                ):
                    wait = openvmm_process.return_value.__enter__.return_value.wait
                    wait.side_effect = (
                        MagicMock(
                            returncode=1,
                            output=b"--network-egress is required",
                        ),
                        MagicMock(
                            returncode=1,
                            output=b"invalid egress transport",
                        ),
                    )
                    output_dir = Path(temporary)
                    microvm_tests.run_l3_l4_egress_policy(
                        Path("openvmm"),
                        Path("vmlinux"),
                        Path("initramfs"),
                        "whp",
                        memory_mib=memory_mib,
                        timeout=1,
                        output_dir=output_dir,
                        guest=guest,
                    )

                    policy_path = output_dir / "l3-l4-requested-policy.json"
                    nvx_path = str(Path(microvm_tests.__file__).parents[1] / "nvx.py")
                    self.assertEqual(
                        run_guest_script.call_args.args[0],
                        [
                            sys.executable,
                            nvx_path,
                            "run",
                            "--guest",
                            guest,
                            "--hypervisor",
                            "whp",
                            "--memory-mib",
                            str(memory_mib),
                            "--net",
                            microvm_tests.DIRECTIONAL_NETWORK_CIDR,
                            "--network-profile",
                            "portable",
                            "--network-egress",
                            "deny",
                            "--network-ingress",
                            "deny",
                            "--network-egress-policy-file",
                            str(policy_path),
                            "--cmdline",
                            "quiet loglevel=0",
                        ],
                    )
                    self.assertIs(
                        run_guest_script.call_args.kwargs["contain_process_tree"],
                        True,
                    )
                    requested = json.loads(policy_path.read_text(encoding="utf-8"))
                    self.assertEqual(requested["allow"][0]["port"], 21001)
                    self.assertEqual(requested["allow"][0]["endPort"], 21003)
                    self.assertEqual(requested["deny"][1]["port"], 21002)
                    results = json.loads(
                        (output_dir / "l3-l4-egress-policy-results.json").read_text(
                            encoding="utf-8"
                        )
                    )
                    self.assertEqual(results["interface"], "nvx.py run")
                    self.assertEqual(
                        results["observed"]["allowed"],
                        ["tcp:start", "tcp:end", "udp:start", "udp:end"],
                    )
                    self.assertEqual(
                        results["observed"]["blocked"],
                        [
                            "tcp:adjacent-low",
                            "tcp:interior",
                            "tcp:adjacent-high",
                            "udp:adjacent-low",
                            "udp:interior",
                            "udp:adjacent-high",
                        ],
                    )

    def test_l3_l4_egress_does_not_report_unobserved_success(self):
        class ControlledThread:
            run_target = False

            def __init__(
                self, *, target: Callable[[], None], **_kwargs: object
            ) -> None:
                self.target = target

            def start(self) -> None:
                if self.run_target:
                    self.target()

            def join(self, _timeout: float | None = None, **_kwargs: object) -> None:
                pass

            def is_alive(self) -> bool:
                return False

        for failure in ("absent-positive", "unexpected-connection"):
            with self.subTest(failure=failure):
                tcp = [MagicMock(spec=socket.socket) for _ in range(5)]
                udp = [MagicMock(spec=socket.socket) for _ in range(5)]
                for index, endpoint in enumerate(tcp):
                    endpoint.getsockname.return_value = ("0.0.0.0", 21000 + index)
                for index, endpoint in enumerate(udp):
                    endpoint.getsockname.return_value = ("0.0.0.0", 22000 + index)
                for endpoint in (tcp[0], tcp[2], tcp[4]):
                    endpoint.accept.side_effect = TimeoutError
                for endpoint in (udp[0], udp[2], udp[4]):
                    endpoint.recvfrom.side_effect = TimeoutError

                ControlledThread.run_target = failure == "unexpected-connection"
                if ControlledThread.run_target:
                    for endpoint in (tcp[1], tcp[3]):
                        connection = MagicMock(spec=socket.socket)
                        connection.recv.return_value = b"GET /allowed HTTP/1.1\r\n\r\n"
                        endpoint.accept.return_value = (
                            connection,
                            ("127.0.0.1", 1),
                        )
                    udp[1].recvfrom.return_value = (
                        b"NVX-L3-L4-UDP-ALLOW-START",
                        ("127.0.0.1", 1),
                    )
                    udp[3].recvfrom.return_value = (
                        b"NVX-L3-L4-UDP-ALLOW-END",
                        ("127.0.0.1", 1),
                    )
                    tcp[0].accept.side_effect = None
                    tcp[0].accept.return_value = (
                        MagicMock(spec=socket.socket),
                        ("127.0.0.1", 1),
                    )

                with tempfile.TemporaryDirectory() as temporary:
                    output_dir = Path(temporary)
                    with (
                        patch.object(
                            microvm_tests,
                            "_bind_egress_ports",
                            return_value=(tcp, udp),
                        ),
                        patch.object(microvm_tests, "run_guest_script"),
                        patch.object(
                            microvm_tests.threading,
                            "Thread",
                            side_effect=ControlledThread,
                        ),
                        self.assertRaises(RuntimeError),
                    ):
                        microvm_tests.run_l3_l4_egress_policy(
                            Path("openvmm"),
                            Path("vmlinux"),
                            Path("initramfs"),
                            "whp",
                            memory_mib=128,
                            timeout=1,
                            output_dir=output_dir,
                        )
                    self.assertFalse(
                        (output_dir / "l3-l4-egress-policy-results.json").exists()
                    )

    def test_runner_dispatches_public_l3_l4_egress_acceptance(self):
        def require(path: Path, _description: str) -> Path:
            return path

        for guest, memory_mib in (("alpine", 128), ("ubuntu", 256)):
            with (
                self.subTest(guest=guest),
                tempfile.TemporaryDirectory() as temporary,
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(
                    microvm_tests, "run_l3_l4_egress_policy"
                ) as run_l3_l4_egress_policy,
            ):
                args = nvx.parse_args(
                    [
                        "test-microvm",
                        "--backend",
                        "whp",
                        "--guest",
                        guest,
                        "--scenario",
                        "l3-l4-egress-policy",
                        "--output-dir",
                        temporary,
                    ]
                )
                self.assertEqual(microvm_tests.run(args), 0)

                run_l3_l4_egress_policy.assert_called_once_with(
                    microvm_tests.openvmm_binary_path(),
                    microvm_tests.artifact_path(
                        microvm_tests.KernelBuildConstants.BINARY_NAME
                    ),
                    microvm_tests.artifact_path(
                        microvm_tests.guest_descriptor(guest).initramfs_name
                    ),
                    "whp",
                    memory_mib=memory_mib,
                    timeout=60.0,
                    output_dir=Path(temporary),
                    guest=guest,
                )

    def test_sandbox_blocks_use_fixed_roles_and_access(self):
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ),
            patch.object(microvm_tests, "run_guest_script") as run_guest_script,
        ):
            microvm_tests.run_sandbox_blocks(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                memory_mib=128,
                timeout=60,
                log_path=Path("sandbox-blocks.log"),
            )

        command = run_guest_script.call_args.args[0]
        values = [
            command[index + 1]
            for index, value in enumerate(command)
            if value == "--microvm-sandbox-block"
        ]
        self.assertEqual(len(values), 4)
        self.assertTrue(values[0].startswith("distro:file:"))
        self.assertTrue(values[1].startswith("runtime:file:"))
        self.assertTrue(values[2].startswith("custom:file:"))
        self.assertTrue(values[3].startswith("scratch:file:"))
        self.assertTrue(all(value.endswith(",ro") for value in values[:3]))
        self.assertFalse(values[3].endswith(",ro"))
        self.assertEqual(
            run_guest_script.call_args.args[2],
            microvm_tests.SANDBOX_BLOCKS_COMPLETION_MARKER,
        )

    def test_smp_snapshot_reruns_probe_and_checks_two_restores(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            with (
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    return_value=["openvmm", "boot"],
                ),
                patch.object(
                    microvm_tests,
                    "smp_probe_script",
                    return_value="probe\n",
                ),
                patch.object(microvm_tests, "capture_snapshot") as capture_snapshot,
                patch.object(microvm_tests, "measure_once") as measure_once,
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    return_value=("manifest", "state", "memory"),
                ),
            ):
                microvm_tests.run_smp_snapshot(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "whp",
                    memory_mib=128,
                    timeout=60,
                    output_dir=output_dir,
                )

        self.assertEqual(capture_snapshot.call_args.kwargs["processors"], 2)
        self.assertEqual(
            capture_snapshot.call_args.kwargs["post_restore_script"],
            "probe\n",
        )
        self.assertEqual(measure_once.call_count, 2)
        self.assertTrue(
            all(
                entry.kwargs["guest_exit_prequeued"]
                for entry in measure_once.call_args_list
            )
        )
        self.assertTrue(
            all(
                entry.kwargs["marker"] == benchmark.RESTORE_MARKER
                for entry in measure_once.call_args_list
            )
        )

    def test_restore_processors_uses_capacity_eight_and_each_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            measure_once = _restore_processors_measure("mshv")
            with (
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    return_value=["openvmm", "boot"],
                ) as workload_boot_command,
                patch.object(microvm_tests, "capture_snapshot") as capture_snapshot,
                patch.object(microvm_tests, "measure_once", measure_once),
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    return_value=("manifest", "state", "memory"),
                ),
            ):
                microvm_tests.run_restore_processors(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "mshv",
                    [1, 2, 4, 8],
                    memory_mib=128,
                    timeout=60,
                    output_dir=output_dir,
                )
            logs = sorted(path.name for path in output_dir.iterdir())

        self.assertEqual(workload_boot_command.call_args.kwargs["processors"], 8)
        self.assertEqual(
            workload_boot_command.call_args.args[5], "quiet loglevel=0 maxcpus=1"
        )
        self.assertEqual(capture_snapshot.call_args.kwargs["processors"], 1)
        self.assertEqual(measure_once.call_count, 5)
        self.assertTrue(
            all(
                entry.kwargs["guest_exit_prequeued"]
                for entry in measure_once.call_args_list
            )
        )
        self.assertTrue(
            all(
                entry.kwargs["environment"][benchmark.SNAPSHOT_PROFILE_ENV] == "1"
                for entry in measure_once.call_args_list
            )
        )
        self.assertEqual(
            [_restore_target(entry.args[0]) for entry in measure_once.call_args_list],
            [1, 2, 4, 8, None],
        )
        self.assertEqual(
            [entry.kwargs["marker"] for entry in measure_once.call_args_list],
            [
                b"NVX-RESTORE-PROCESSORS-OK count=1",
                b"NVX-RESTORE-PROCESSORS-OK count=2",
                b"NVX-RESTORE-PROCESSORS-OK count=4",
                b"NVX-RESTORE-PROCESSORS-OK count=8",
                b"NVX-RESTORE-PROCESSORS-OK count=1",
            ],
        )
        self.assertEqual(
            logs,
            [
                "restore-processors-1.log",
                "restore-processors-2.log",
                "restore-processors-4.log",
                "restore-processors-8.log",
                "restore-processors-untargeted.log",
            ],
        )

    def test_restore_processors_rejects_full_capacity_mshv_prefix(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "capture_snapshot"),
                patch.object(
                    microvm_tests,
                    "measure_once",
                    _restore_processors_measure("mshv", mshv_prefix=False),
                ),
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    return_value=("manifest", "state", "memory"),
                ),
            ):
                with self.assertRaisesRegex(
                    RuntimeError,
                    r"restore target 2 bound VPs \[0, 1, 2, 3, 4, 5, 6, 7\] on mshv; "
                    r"expected exactly VPs 0\.\.1 of capacity 8",
                ):
                    microvm_tests.run_restore_processors(
                        Path("openvmm"),
                        Path("vmlinux"),
                        Path("initrd"),
                        "mshv",
                        [2],
                        memory_mib=128,
                        timeout=60,
                        output_dir=Path(temporary),
                    )

    def test_restore_processors_requires_full_capacity_off_mshv(self):
        for backend in ("kvm", "whp"):
            with self.subTest(backend=backend):
                with tempfile.TemporaryDirectory() as temporary:

                    def measure(command: list[str], **kwargs: object) -> None:
                        log_path = cast(Path, kwargs["log_path"])
                        log_path.write_bytes(_vp_binding_profile([0, 1]))

                    with (
                        patch.object(microvm_tests, "capture_snapshot"),
                        patch.object(
                            microvm_tests, "measure_once", side_effect=measure
                        ),
                        patch.object(
                            microvm_tests,
                            "_snapshot_fingerprint",
                            return_value=("manifest", "state", "memory"),
                        ),
                    ):
                        with self.assertRaisesRegex(
                            RuntimeError,
                            rf"restore target 2 bound VPs \[0, 1\] on {backend}; "
                            r"expected exactly VPs 0\.\.7 of capacity 8",
                        ):
                            microvm_tests.run_restore_processors(
                                Path("openvmm"),
                                Path("vmlinux"),
                                Path("initrd"),
                                backend,
                                [2],
                                memory_mib=128,
                                timeout=60,
                                output_dir=Path(temporary),
                            )

    def test_restore_vp_bindings_require_one_complete_record_set(self):
        profile = _vp_binding_profile([0, 1])
        self.assertEqual(microvm_tests._restore_vp_bindings(profile), [0, 1])
        self.assertEqual(
            microvm_tests._restore_vp_bindings(b"guest output\n" + profile),
            [0, 1],
        )
        self.assertEqual(
            microvm_tests._restore_vp_bindings(
                profile.replace(
                    b"operation=startup phase=vp_thread_bind",
                    b"phase=vp_thread_bind operation=startup",
                )
            ),
            [0, 1],
        )
        self.assertEqual(
            microvm_tests._restore_vp_bindings(
                profile + profile.replace(b"operation=startup", b"operation=restore")
            ),
            [0, 1],
        )
        microvm_tests._check_restore_vp_bindings(profile, "mshv", target=2, capacity=8)
        with self.assertRaisesRegex(RuntimeError, r"bound VPs \[0, 1, 1\]"):
            microvm_tests._check_restore_vp_bindings(
                _vp_binding_profile([0, 1, 1]), "mshv", target=2, capacity=8
            )
        with self.assertRaisesRegex(RuntimeError, r"untargeted restore bound VPs"):
            microvm_tests._check_restore_vp_bindings(
                profile, "mshv", target=None, capacity=8
            )
        with self.assertRaisesRegex(RuntimeError, r"found 0"):
            microvm_tests._restore_vp_bindings(b"")
        with self.assertRaisesRegex(RuntimeError, r"found 2"):
            microvm_tests._restore_vp_bindings(profile + profile)
        with self.assertRaisesRegex(RuntimeError, r"malformed VP binding"):
            microvm_tests._restore_vp_bindings(
                profile.replace(b"vp_bind_ap_1", b"vp_bind_ap_x")
            )
        with self.assertRaises(ValueError):
            microvm_tests._restore_vp_bindings(
                profile.replace(b"exclusive=0", b"exclusive=2")
            )

    def test_restore_tsc_sync_forces_linux_warp_check_and_keeps_failure_guard(self):
        for backend in ("kvm", "mshv", "whp"):
            with self.subTest(backend=backend):
                measure = _restore_processors_measure(backend)
                with tempfile.TemporaryDirectory() as temporary:
                    with (
                        patch.object(microvm_tests, "capture_snapshot") as capture,
                        patch.object(microvm_tests, "measure_once", measure),
                        patch.object(
                            microvm_tests,
                            "_snapshot_fingerprint",
                            return_value=("manifest", "state", "memory"),
                        ),
                    ):
                        microvm_tests.run_restore_processors(
                            Path("openvmm"),
                            Path("vmlinux"),
                            Path("initrd"),
                            backend,
                            [1, 2, 4, 8],
                            memory_mib=128,
                            timeout=60,
                            output_dir=Path(temporary),
                            check_tsc_sync=True,
                        )
                command = capture.call_args.args[0]
                self.assertEqual(
                    command[command.index("--cmdline") + 1],
                    "quiet loglevel=0 maxcpus=1 clearcpuid=tsc_adjust",
                )
                script = capture.call_args.kwargs["post_restore_script"]
                self.assertTrue(
                    script.startswith(microvm_tests._read_script("restore-tsc-sync.sh"))
                )
                self.assertTrue(
                    script.endswith(microvm_tests._read_script("restore-processors.sh"))
                )
                self.assertEqual(capture.call_args.kwargs["processors"], 1)
                self.assertEqual(measure.call_count, 5)
                self.assertEqual(
                    [_restore_target(call.args[0]) for call in measure.call_args_list],
                    [1, 2, 4, 8, None],
                )

    def test_restore_tsc_sync_rejects_an_ineffective_cpu_feature_mask(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        script = microvm_tests._read_script("restore-tsc-sync.sh")
        for cpuinfo, status, expected in (
            ("flags : tsc constant_tsc rdtscp", 0, 0),
            ("flags : tsc tsc_adjust constant_tsc", 0, 96),
            ("flags : tsc tsc_adjust", 0, 96),
            ("", 1, 1),
        ):
            with self.subTest(cpuinfo=cpuinfo, status=status):
                result = subprocess.run(
                    [shell],
                    input=(
                        f"cat() {{ printf '%s\\n' '{cpuinfo}'; return {status}; }}\n"
                        + script
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(
                    "NVX-RESTORE-TSC-SYNC-CHECK-ENABLED" in result.stdout,
                    expected == 0,
                )

    def test_restore_processors_rejects_unstable_tsc_after_cpu_activation(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")

        script = microvm_tests._read_script("restore-processors.sh")
        for kernel_log, dmesg_status, expected_status in (
            ("clocksource: Switched to clocksource tsc", 0, 0),
            ("Measured 10992 cycles TSC warp between CPUs", 0, 95),
            ("tsc: Marking TSC unstable due to check_tsc_sync_source failed", 0, 95),
            ("TSC found unstable after boot", 0, 95),
            ("", 1, 1),
        ):
            with self.subTest(kernel_log=kernel_log, dmesg_status=dmesg_status):
                result = subprocess.run(
                    [shell],
                    input=(
                        "getconf() { printf '4\\n'; }\n"
                        "cat() {\n"
                        '  case "$1" in\n'
                        "    */cpu/online) printf '0-3\\n' ;;\n"
                        "    */current_clocksource) printf 'tsc\\n' ;;\n"
                        "    *) return 99 ;;\n"
                        "  esac\n"
                        "}\n"
                        'taskset() { printf "%s\\n" "$2"; }\n'
                        f"dmesg() {{ printf '%s\\n' '{kernel_log}'; "
                        f"return {dmesg_status}; }}\n" + script
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )

                self.assertEqual(result.returncode, expected_status, result.stderr)
                self.assertIn("NVX-RESTORE-PROCESSOR-OK count=4 cpu=3", result.stdout)
                if expected_status == 0:
                    self.assertIn(
                        "NVX-RESTORE-CLOCKSOURCE-OK source=tsc", result.stdout
                    )
                    self.assertIn("NVX-RESTORE-PROCESSORS-OK count=4", result.stdout)
                else:
                    self.assertNotIn("NVX-RESTORE-PROCESSORS-OK", result.stdout)
                if expected_status == 95:
                    self.assertIn(kernel_log, result.stdout)
                    self.assertIn(
                        "NVX-RESTORE-PROCESSORS-FAIL unstable-tsc", result.stdout
                    )

    def test_restore_processors_fail_fast_and_record_restored_tsc_logs(self):
        for check_tsc_sync in (False, True):
            with self.subTest(check_tsc_sync=check_tsc_sync):
                measure = _restore_processors_measure("mshv")
                with tempfile.TemporaryDirectory() as temporary:
                    with (
                        patch.object(microvm_tests, "capture_snapshot"),
                        patch.object(microvm_tests, "measure_once", measure),
                        patch.object(
                            microvm_tests,
                            "_snapshot_fingerprint",
                            return_value=("manifest", "state", "memory"),
                        ),
                    ):
                        microvm_tests.run_restore_processors(
                            Path("openvmm"),
                            Path("vmlinux"),
                            Path("initrd"),
                            "mshv",
                            [2, 8],
                            memory_mib=128,
                            timeout=60,
                            output_dir=Path(temporary),
                            check_tsc_sync=check_tsc_sync,
                        )

                self.assertEqual(measure.call_count, 3)
                for entry in measure.call_args_list:
                    self.assertEqual(
                        entry.kwargs["failure_marker"],
                        b"NVX-RESTORE-PROCESSORS-FAIL",
                    )
                    environment = entry.kwargs["environment"]
                    self.assertEqual(
                        environment["OPENVMM_LOG"],
                        "off,vmm_core::partition_unit::vp_set::tsc=debug,"
                        "virt_mshv::x86_64::tsc=info",
                    )
                    self.assertEqual(environment[benchmark.SNAPSHOT_PROFILE_ENV], "1")

    def test_restore_processors_classifies_unstable_tsc_with_fresh_boot_control(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            passing = _restore_processors_measure("mshv")

            def measure(command: list[str], **kwargs: object) -> None:
                if _restore_target(command) == 4:
                    raise benchmark.GuestFailureReported(
                        "NVX-RESTORE-PROCESSORS-FAIL unstable-tsc",
                        "Measured 45 cycles TSC warp between CPUs\r\n",
                    )
                passing(command, **kwargs)

            with (
                patch.object(microvm_tests, "capture_snapshot"),
                patch.object(
                    microvm_tests, "measure_once", side_effect=measure
                ) as measure_once,
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    return_value=("manifest", "state", "memory"),
                ),
                patch.object(
                    microvm_tests,
                    "run_fresh_boot_tsc_control",
                    return_value="fresh-boot TSC control: verdict",
                ) as control,
                patch.object(
                    microvm_tests,
                    "_host_invariant_tsc_note",
                    return_value="host clock: note\n",
                ),
            ):
                with self.assertRaises(RuntimeError) as raised:
                    microvm_tests.run_restore_processors(
                        Path("openvmm"),
                        Path("vmlinux"),
                        Path("initrd"),
                        "mshv",
                        [1, 2, 4, 8],
                        memory_mib=192,
                        timeout=30,
                        output_dir=output_dir,
                        check_tsc_sync=True,
                    )

        self.assertEqual(
            str(raised.exception),
            "restore target 4: guest reported "
            "NVX-RESTORE-PROCESSORS-FAIL unstable-tsc\n"
            "fresh-boot TSC control: verdict\n"
            "host clock: note\n"
            "--- OpenVMM output ---\n"
            "Measured 45 cycles TSC warp between CPUs\r\n",
        )
        self.assertIsInstance(
            raised.exception.__cause__, benchmark.GuestFailureReported
        )
        self.assertEqual(measure_once.call_count, 3)
        control.assert_called_once_with(
            Path("openvmm"),
            Path("vmlinux"),
            Path("initrd"),
            "mshv",
            memory_mib=192,
            timeout=30,
            log_path=output_dir / "restore-processors-tsc-control.log",
        )

    def test_host_invariant_tsc_note_reads_the_cpu_flags(self):
        with tempfile.TemporaryDirectory() as temporary:
            cpuinfo = Path(temporary) / "cpuinfo"
            cases = (
                (
                    "processor\t: 0\nflags\t\t: fpu tsc constant_tsc nonstop_tsc\n",
                    "host CPU exposes an invariant TSC (nonstop_tsc)\n",
                ),
                (
                    "processor\t: 0\nflags\t\t: fpu tsc constant_tsc tsc_known_freq\n",
                    "host CPU does not expose an invariant TSC (nonstop_tsc); "
                    "guests on this host intermittently see cross-vCPU TSC warps\n",
                ),
                ("processor\t: 0\n", ""),
            )
            for text, expected in cases:
                with self.subTest(text=text):
                    cpuinfo.write_text(text, encoding="utf-8")
                    self.assertEqual(
                        microvm_tests._host_invariant_tsc_note(cpuinfo), expected
                    )
            self.assertEqual(
                microvm_tests._host_invariant_tsc_note(Path(temporary) / "missing"),
                "",
            )

    def test_restore_processors_reports_other_guest_failures_without_control(self):
        failure = benchmark.GuestFailureReported(
            "NVX-RESTORE-PROCESSORS-FAIL expected=0-3 actual=0-2", "tail"
        )
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "capture_snapshot"),
                patch.object(microvm_tests, "measure_once", side_effect=failure),
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    return_value=("manifest", "state", "memory"),
                ),
                patch.object(microvm_tests, "run_fresh_boot_tsc_control") as control,
            ):
                with self.assertRaises(RuntimeError) as raised:
                    microvm_tests.run_restore_processors(
                        Path("openvmm"),
                        Path("vmlinux"),
                        Path("initrd"),
                        "kvm",
                        [4],
                        memory_mib=128,
                        timeout=60,
                        output_dir=Path(temporary),
                    )

        self.assertEqual(
            str(raised.exception),
            "restore target 4: guest reported "
            "NVX-RESTORE-PROCESSORS-FAIL expected=0-3 actual=0-2\n"
            "--- OpenVMM output ---\ntail",
        )
        self.assertIs(raised.exception.__cause__, failure)
        control.assert_not_called()

    def test_fresh_boot_tsc_control_forces_the_warp_check_on_every_processor(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "control.log"
            result: benchmark.GuestCommandResult = {
                "text": (
                    '> echo "NVX-TSC-CONTROL-RESULT unstable activations=0"\r\n'
                    "NVX-TSC-CONTROL-RESULT stable activations=147\r\n"
                ),
                "wall_ms": 1.0,
                "peak_rss_bytes": 1,
            }
            with (
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    return_value=["openvmm", "boot"],
                ) as workload_boot_command,
                patch.object(
                    microvm_tests, "run_guest_script", return_value=result
                ) as run_guest_script,
            ):
                verdict = microvm_tests.run_fresh_boot_tsc_control(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "kvm",
                    memory_mib=128,
                    timeout=45,
                    log_path=log_path,
                )

        self.assertEqual(
            verdict,
            "fresh-boot TSC control: no TSC instability across 147 CPU "
            "activations without snapshot restore",
        )
        self.assertEqual(
            workload_boot_command.call_args.args,
            (
                Path("openvmm"),
                "kvm",
                Path("vmlinux"),
                Path("initrd"),
                128,
                "quiet loglevel=0 clearcpuid=tsc_adjust",
            ),
        )
        self.assertEqual(workload_boot_command.call_args.kwargs, {"processors": 8})
        command, script, marker = run_guest_script.call_args.args
        self.assertEqual(command, ["openvmm", "boot"])
        self.assertEqual(
            script,
            microvm_tests._render_script(
                "tsc-sync-control.sh.in", PROCESSORS="8", ROUNDS="20"
            ),
        )
        self.assertNotIn("@", script)
        self.assertEqual(marker, b"NVX-TSC-CONTROL-DONE")
        self.assertEqual(
            run_guest_script.call_args.kwargs, {"timeout": 45, "log_path": log_path}
        )

    def test_fresh_boot_tsc_control_reports_its_own_failure(self):
        with patch.object(
            microvm_tests,
            "run_guest_script",
            side_effect=TimeoutError("guest workload did not finish within 45s\ntail"),
        ):
            verdict = microvm_tests.run_fresh_boot_tsc_control(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "mshv",
                memory_mib=128,
                timeout=45,
                log_path=Path("control.log"),
            )

        self.assertEqual(
            verdict,
            "fresh-boot TSC control did not complete: "
            "guest workload did not finish within 45s",
        )

    def test_tsc_control_verdict_accepts_only_guest_result_lines(self):
        for text, expected in (
            (
                "NVX-TSC-CONTROL-RESULT unstable activations=12\r\n",
                "fresh-boot TSC control: Linux also found TSC instability without "
                "snapshot restore after 12 CPU activations",
            ),
            (
                '> echo "NVX-TSC-CONTROL-RESULT stable activations=$activations"\n',
                "fresh-boot TSC control did not report a result",
            ),
            (
                "NVX-TSC-CONTROL-RESULT stable activations=many\n",
                "fresh-boot TSC control did not report a result",
            ),
            ("", "fresh-boot TSC control did not report a result"),
        ):
            with self.subTest(text=text):
                self.assertEqual(microvm_tests._tsc_control_verdict(text), expected)

    def test_tsc_sync_control_script_reactivates_aps_until_tsc_instability(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        for flags, online, unstable, expected_status, expected_output in (
            ("tsc rdtscp", 4, False, 0, "NVX-TSC-CONTROL-RESULT stable activations=9"),
            (
                "tsc rdtscp",
                4,
                True,
                0,
                "NVX-TSC-CONTROL-RESULT unstable activations=3",
            ),
            ("tsc tsc_adjust", 4, False, 61, "NVX-TSC-CONTROL-FAIL code=61"),
            ("tsc rdtscp", 2, False, 60, "NVX-TSC-CONTROL-FAIL code=60"),
        ):
            with self.subTest(flags=flags, online=online, unstable=unstable):
                with tempfile.TemporaryDirectory() as temporary:
                    cpu_root = Path(temporary)
                    for cpu in range(1, 4):
                        (cpu_root / f"cpu{cpu}").mkdir()
                    kernel_log = (
                        "Measured 45 cycles TSC warp between CPUs"
                        if unstable
                        else "clocksource: Switched to clocksource tsc"
                    )
                    script = (
                        microvm_tests._render_script(
                            "tsc-sync-control.sh.in", PROCESSORS="4", ROUNDS="2"
                        )
                        .replace("/sys/devices/system/cpu", cpu_root.as_posix())
                        .replace("nvx-exit", "nvx_exit")
                    )
                    result = subprocess.run(
                        [shell, "-s"],
                        input=(
                            f"getconf() {{ printf '{online}\\n'; }}\n"
                            f"cat() {{ printf 'flags : {flags}\\n'; }}\n"
                            f"dmesg() {{ printf '%s\\n' '{kernel_log}'; }}\n"
                            'nvx_exit() { exit "$1"; }\n' + script
                        ),
                        text=True,
                        capture_output=True,
                        timeout=5,
                        check=False,
                    )
                    reactivated = all(
                        (cpu_root / f"cpu{cpu}" / "online").is_file()
                        and (cpu_root / f"cpu{cpu}" / "online").read_text() == "1\n"
                        for cpu in range(1, 4)
                    )

                self.assertEqual(
                    result.returncode, expected_status, result.stdout + result.stderr
                )
                self.assertIn(expected_output, result.stdout)
                self.assertEqual(
                    "NVX-TSC-CONTROL-DONE" in result.stdout, expected_status == 0
                )
                self.assertEqual(reactivated, expected_output.endswith("=9"))
                if unstable:
                    self.assertIn(kernel_log, result.stdout)

    def test_tsc_sync_control_script_fails_when_dmesg_fails(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        script = microvm_tests._render_script(
            "tsc-sync-control.sh.in", PROCESSORS="4", ROUNDS="2"
        ).replace("nvx-exit", "nvx_exit")

        result = subprocess.run(
            [shell, "-s"],
            input=(
                "getconf() { printf '4\\n'; }\n"
                "cat() { printf 'flags : tsc rdtscp\\n'; }\n"
                "dmesg() { return 71; }\n"
                'nvx_exit() { exit "$1"; }\n' + script
            ),
            text=True,
            capture_output=True,
            timeout=5,
            check=False,
        )

        self.assertEqual(result.returncode, 71, result.stdout + result.stderr)
        self.assertIn("NVX-TSC-CONTROL-FAIL code=71", result.stdout)
        self.assertNotIn("NVX-TSC-CONTROL-RESULT", result.stdout)
        self.assertNotIn("NVX-TSC-CONTROL-DONE", result.stdout)

    def test_restore_memory_reuses_one_base_snapshot_for_all_targets(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output_dir = root / "logs"
            snapshot_memory = root / "snapshot" / "memory.bin"
            snapshot_memory.parent.mkdir()
            with snapshot_memory.open("wb") as memory:
                memory.truncate(512 * 1024 * 1024)

            def fingerprint(snapshot_path: Path) -> tuple[str, str, str]:
                return ("manifest", "state", str(snapshot_path / "memory.bin"))

            def measure(command: list[str], **kwargs: object) -> None:
                target = int(
                    command[command.index("--restore-memory") + 1].removesuffix("M")
                )
                added = (target - 512) * 1024 * 1024
                log_path = cast(Path, kwargs["log_path"])
                log_path.parent.mkdir(parents=True, exist_ok=True)
                log_path.write_bytes(
                    f"NVX-MEMORY-ONLINE-OK: added_bytes={added} "
                    "memtotal_kib=1 elapsed_us=1\n"
                    "NVX-RESTORE-MEMORY-WORKLOAD-OK\n".encode()
                )

            with (
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    return_value=["openvmm", "boot"],
                ),
                patch.object(microvm_tests, "capture_snapshot") as capture_snapshot,
                patch.object(
                    microvm_tests, "measure_once", side_effect=measure
                ) as measure_once,
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    side_effect=fingerprint,
                ),
                patch.object(
                    microvm_tests,
                    "require_file",
                    return_value=snapshot_memory,
                ),
            ):
                microvm_tests.run_restore_memory(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "whp",
                    timeout=60,
                    output_dir=output_dir,
                )

        capture_command = capture_snapshot.call_args.args[0]
        self.assertEqual(
            capture_command[capture_command.index("--memory-capacity") + 1],
            "2048M",
        )
        self.assertEqual(measure_once.call_count, 3)
        self.assertTrue(
            all(
                entry.kwargs["guest_exit_prequeued"]
                for entry in measure_once.call_args_list
            )
        )
        self.assertEqual(
            [
                entry.args[0][entry.args[0].index("--restore-memory") + 1]
                for entry in measure_once.call_args_list
            ],
            ["512M", "1024M", "2048M"],
        )

    def test_runner_dispatches_selected_scenarios_once(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            args = argparse.Namespace(
                backend="whp",
                guest="alpine",
                scenario=["smp", "smp", "smp-lapic", "smp-lapic"],
                processors=[2, 2, 8],
                memory_mib=128,
                timeout=60.0,
                output_dir=output_dir,
            )

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(
                    microvm_tests,
                    "require_file",
                    side_effect=require,
                ),
                patch.object(microvm_tests, "run_lifecycle") as run_lifecycle,
                patch.object(microvm_tests, "run_smp") as run_smp,
            ):
                self.assertEqual(microvm_tests.run(args), 0)

        run_lifecycle.assert_not_called()
        self.assertEqual(
            [entry.args[4] for entry in run_smp.call_args_list],
            [2, 8, 2, 8],
        )
        self.assertEqual(
            [entry.kwargs["log_path"].name for entry in run_smp.call_args_list],
            ["smp-2.log", "smp-8.log", "smp-lapic-2.log", "smp-lapic-8.log"],
        )
        self.assertEqual(
            [entry.kwargs["force_lapic_timer"] for entry in run_smp.call_args_list],
            [False, False, True, True],
        )

    def test_runner_dispatches_public_managed_exec_configuration(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    "--scenario",
                    "managed-exec-config",
                    "--output-dir",
                    str(output_dir),
                ]
            )

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_managed_exec_configuration") as run,
            ):
                self.assertEqual(microvm_tests.run(args), 0)
            run.assert_called_once_with(
                "whp", timeout=args.timeout, output_dir=output_dir
            )

    def test_managed_container_launch_prepares_identity_and_static_helper(self):
        root = Path(__file__).resolve().parent.parent
        bootstrap = (root / "guest" / "common" / "nvx-init-agent").read_text()
        launcher = (root / "guest" / "alpine" / "nvx-container-enter").read_text()
        self.assertLess(
            bootstrap.index('>"$runtime/workload-machine-id"'),
            bootstrap.index("    /sbin/nvx-managed-agent \\"),
        )
        self.assertIn(
            "set -- /.nvx-agent/nvx-managed-agent \\\n"
            '        --exec-config-fd "$NVX_EXEC_CONFIG_FD" -- "$@"',
            launcher,
        )

    def test_runner_uses_ubuntu_artifact_and_default_memory(self):
        requested: list[Path] = []

        def require(path: Path, _description: str) -> Path:
            requested.append(path)
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    "--guest",
                    "ubuntu",
                    "--scenario",
                    "guest-boot",
                    "--output-dir",
                    temporary,
                ]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_guest_boot") as run_guest_boot,
            ):
                self.assertEqual(microvm_tests.run(args), 0)

        self.assertIn(
            BuildConstants.BUILD_DIR / "initramfs-ubuntu.cpio.gz",
            requested,
        )
        self.assertEqual(run_guest_boot.call_args.kwargs["memory_mib"], 256)

    def test_runner_omits_console_snapshot_from_ubuntu_defaults(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    "--guest",
                    "ubuntu",
                    "--output-dir",
                    temporary,
                ]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(
                    microvm_tests,
                    "MICROVM_TEST_SCENARIOS",
                    ("console-snapshot", "guest-boot"),
                ),
                patch.object(
                    microvm_tests,
                    "require_file",
                    side_effect=require,
                ),
                patch.object(microvm_tests, "run_guest_boot") as guest_boot,
                patch.object(
                    microvm_tests,
                    "run_console_snapshot",
                ) as console_snapshot,
            ):
                self.assertEqual(microvm_tests.run(args), 0)

        guest_boot.assert_called_once()
        console_snapshot.assert_not_called()

    def test_runner_rejects_ubuntu_unsupported_scenarios(self):
        def require(path: Path, _description: str) -> Path:
            return path

        for scenario in (
            "console-snapshot",
            "sandbox-blocks",
            "scratch-snapshot",
            "snapshot-tiers",
        ):
            with self.subTest(scenario=scenario):
                args = nvx.parse_args(
                    [
                        "test-microvm",
                        "--backend",
                        "whp",
                        "--guest",
                        "ubuntu",
                        "--scenario",
                        scenario,
                    ]
                )
                with (
                    patch.object(
                        microvm_tests,
                        "validate_openvmm_test_backend",
                    ),
                    patch.object(
                        microvm_tests,
                        "require_file",
                        side_effect=require,
                    ),
                    self.assertRaisesRegex(
                        common.ScriptError,
                        "Ubuntu guest does not support",
                    ),
                ):
                    microvm_tests.run(args)

    def test_runner_excludes_sandbox_scenarios_without_sandbox_control(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "kvm",
                    "--guest",
                    "azurelinux",
                    "--output-dir",
                    temporary,
                ]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(
                    microvm_tests,
                    "MICROVM_TEST_SCENARIOS",
                    ("sandbox-blocks", "guest-boot"),
                ),
                patch.object(
                    microvm_tests,
                    "require_file",
                    side_effect=require,
                ),
                patch.object(microvm_tests, "run_guest_boot") as guest_boot,
                patch.object(microvm_tests, "run_sandbox_blocks") as sandbox_blocks,
            ):
                self.assertEqual(microvm_tests.run(args), 0)

        guest_boot.assert_called_once()
        sandbox_blocks.assert_not_called()

    def test_runner_rejects_sandbox_scenarios_without_sandbox_control(self):
        def require(path: Path, _description: str) -> Path:
            return path

        args = nvx.parse_args(
            [
                "test-microvm",
                "--backend",
                "kvm",
                "--guest",
                "azurelinux",
                "--scenario",
                "sandbox-blocks",
            ]
        )
        with (
            patch.object(microvm_tests, "validate_openvmm_test_backend"),
            patch.object(microvm_tests, "require_file", side_effect=require),
            self.assertRaisesRegex(
                common.ScriptError,
                "Azure Linux guest does not support",
            ),
        ):
            microvm_tests.run(args)

    def test_runner_keeps_restore_tsc_logs_separate_from_processor_restore(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    "--scenario",
                    "restore-processors",
                    "--scenario",
                    "restore-tsc-sync",
                    "--scenario",
                    "restore-tsc-sync",
                    "--output-dir",
                    str(output_dir),
                ]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_restore_processors") as run,
            ):
                self.assertEqual(microvm_tests.run(args), 0)
            self.assertTrue((output_dir / "restore-tsc-sync").is_dir())
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args_list[0].kwargs["output_dir"], output_dir)
        self.assertNotIn("check_tsc_sync", run.call_args_list[0].kwargs)
        self.assertEqual(
            run.call_args_list[1].kwargs["output_dir"],
            output_dir / "restore-tsc-sync",
        )
        self.assertTrue(run.call_args_list[1].kwargs["check_tsc_sync"])

    def test_runner_dispatches_console_exit_for_each_requested_cpu_count(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = argparse.Namespace(
                backend="kvm",
                guest="alpine",
                scenario=["console-exit", "console-exit"],
                processors=[1, 2, 2, 4, 8],
                memory_mib=128,
                timeout=40.0,
                output_dir=Path(temporary),
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_console_exit") as run,
            ):
                self.assertEqual(microvm_tests.run(args), 0)
        self.assertEqual([call.args[4] for call in run.call_args_list], [1, 2, 4, 8])

    def test_guest_runner_persists_full_output_on_failure(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return 0

            def wait(self):
                return 0

        class FakeInteraction:
            def __init__(self):
                self.process = FakeProcess()

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(b"complete raw output\n")
                chunks.put(None)

            def write_input(self, _data: bytes) -> None:
                raise AssertionError("input should not be sent without a boot marker")

            def close(self) -> None:
                pass

        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "failure.log"
            with (
                patch.object(
                    benchmark,
                    "InteractiveProcess",
                    return_value=FakeInteraction(),
                ),
                patch.object(benchmark, "terminate"),
            ):
                with self.assertRaisesRegex(RuntimeError, "boot marker"):
                    benchmark.run_guest_script(
                        ["openvmm"],
                        "echo test\n",
                        b"DONE",
                        timeout=1,
                        log_path=log_path,
                    )

            self.assertEqual(log_path.read_bytes(), b"complete raw output\n")


if __name__ == "__main__":
    unittest.main()

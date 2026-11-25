#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import contextlib
import io
import json
import os
import re
import signal
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    benchmark,
    build,
    build_config,
    ci,
    common,
    guests,
    openvmm_ci,
)
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
    UbuntuBuildConstants,
)
from nvx_tools.time_abi import (  # noqa: E402
    CPU_GENERATIONS,
    OPENVMM_CPU_PROFILES,
    CpuGeneration,
    CpuModel,
    HostCpu,
    cpu_profile_request_url,
)


class CliTests(unittest.TestCase):
    def test_usage_documents_every_command_and_option(self):
        # doc/usage.md is the CLI reference: every command has a row in its
        # command table and a section that names exactly the options of the
        # command's --help usage.
        option = re.compile(r"(?<![\w-])--[a-z0-9]+(?:-[a-z0-9]+)*(?![\w*-])")

        def usage_of(*command: str) -> str:
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout), self.assertRaises(SystemExit):
                nvx.parse_args([*command, "--help"])
            return stdout.getvalue().split("\n\n", 1)[0]

        def subcommands(usage: str) -> list[str]:
            # A subcommand list reads "{a,b} ...", which argparse may wrap
            # before the dots; an option's repeated choices read
            # "{a,b} [{a,b} ...]".
            match = re.search(r"(?<!\[)\{([a-z0-9,-]+)\}\s+\.\.\.(?!\])", usage)
            return match.group(1).split(",") if match else []

        document = (BuildConstants.REPO_ROOT / "doc" / "usage.md").read_text(
            encoding="utf-8"
        )

        def section(title: str, level: int) -> str:
            match = re.search(
                rf"^{'#' * level} `{re.escape(title)}`\n(.*?)(?=^#{{2,{level}}} |\Z)",
                document,
                re.MULTILINE | re.DOTALL,
            )
            self.assertIsNotNone(match, f"doc/usage.md has no section for {title}")
            return match.group(1) if match else ""

        table = document.split("## Commands\n", 1)[1].split("\n## ", 1)[0]
        commands = subcommands(usage_of())
        self.assertEqual(
            set(re.findall(r"^\| `([a-z0-9-]+)` \|", table, re.MULTILINE)),
            set(commands),
        )
        for name in commands:
            usage = usage_of(name)
            nested = subcommands(usage)
            section(name, 3)
            targets = (
                [((name, child), 4) for child in nested] if nested else [((name,), 3)]
            )
            for command, level in targets:
                title = " ".join(command)
                with self.subTest(command=title):
                    self.assertEqual(
                        set(option.findall(section(title, level))),
                        set(option.findall(usage_of(*command))),
                    )

    @unittest.skipUnless(os.name == "posix", "SIGTERM is a POSIX signal")
    def test_sigterm_interrupts_a_command_as_sigint_does(self):
        # A cancelled CI step gets SIGINT and then SIGTERM (#946). Either one
        # unwinds the command, so that its cleanup stops OpenVMM and removes
        # its scratch directories, and nvx.py then exits as after Ctrl-C.
        previous = signal.signal(signal.SIGTERM, signal.SIG_DFL)
        self.addCleanup(signal.signal, signal.SIGTERM, previous)
        cleaned: list[bool] = []

        def interrupted(_args: argparse.Namespace) -> int:
            try:
                # Without its handler, SIGTERM would end this test process.
                self.assertIs(
                    signal.getsignal(signal.SIGTERM), signal.default_int_handler
                )
                os.kill(os.getpid(), signal.SIGTERM)
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    time.sleep(0.01)
                self.fail("SIGTERM did not interrupt the command")
            finally:
                cleaned.append(True)

        stderr = io.StringIO()
        with (
            patch.object(
                nvx, "parse_args", return_value=argparse.Namespace(handler=interrupted)
            ),
            contextlib.redirect_stderr(stderr),
        ):
            self.assertEqual(nvx.main([]), 130)
        self.assertEqual(cleaned, [True])
        self.assertEqual(stderr.getvalue(), "Interrupted\n")
        self.assertEqual(signal.getsignal(signal.SIGTERM), signal.SIG_DFL)

        # As Python does for SIGINT, nvx.py leaves an ignored SIGTERM ignored.
        signal.signal(signal.SIGTERM, signal.SIG_IGN)

        def ignored(_args: argparse.Namespace) -> int:
            self.assertEqual(signal.getsignal(signal.SIGTERM), signal.SIG_IGN)
            os.kill(os.getpid(), signal.SIGTERM)
            time.sleep(0.05)
            return 0

        with patch.object(
            nvx, "parse_args", return_value=argparse.Namespace(handler=ignored)
        ):
            self.assertEqual(nvx.main([]), 0)
        self.assertEqual(signal.getsignal(signal.SIGTERM), signal.SIG_IGN)

    def test_check_required_ci_rejects_malformed_run_flags(self):
        # microsoft/nvx#496: an empty or malformed --run-* value must not be
        # silently accepted as false, which would let required-status checking
        # pass with required jobs skipped. Every output of the change detector
        # is a required flag.
        flags = tuple(f"--{output}" for output in ci.CI_CHANGE_OUTPUTS)
        base = [
            "check-required-ci",
            "--event-name",
            "pull_request",
            "--same-repository",
            "true",
            *(value for flag in flags for value in (flag, "true")),
        ]
        for flag in flags:
            for bad in ("", "tru", "TRUE", "1", "yes", None):
                argv = list(base)
                index = argv.index(flag)
                if bad is None:
                    del argv[index : index + 2]
                else:
                    argv[index + 1] = bad
                with self.subTest(flag=flag, value=bad):
                    with (
                        patch("sys.stderr", new_callable=io.StringIO),
                        self.assertRaises(SystemExit) as exit_context,
                    ):
                        nvx.parse_args(argv)
                    self.assertEqual(exit_context.exception.code, 2)
        for value in ("true", "false"):
            argv = list(base)
            for flag in flags:
                argv[argv.index(flag) + 1] = value
            args = nvx.parse_args(argv)
            for flag in flags:
                self.assertEqual(getattr(args, flag[2:].replace("-", "_")), value)

    def test_check_required_ci_accepts_every_ci_workflow_event(self):
        base = [
            "check-required-ci",
            "--event-name",
            "",
            "--same-repository",
            "true",
            *(
                value
                for output in ci.CI_CHANGE_OUTPUTS
                for value in (f"--{output}", "true")
            ),
        ]
        events = (
            "pull_request",
            "merge_group",
            "push",
            "schedule",
            "workflow_dispatch",
        )
        self.assertEqual(ci.CI_EVENTS, events)
        for event_name in events:
            argv = list(base)
            argv[argv.index("--event-name") + 1] = event_name
            with self.subTest(event_name=event_name):
                self.assertEqual(nvx.parse_args(argv).event_name, event_name)
        argv = list(base)
        argv[argv.index("--event-name") + 1] = "pull_request_target"
        with (
            patch("sys.stderr", new_callable=io.StringIO),
            self.assertRaises(SystemExit) as exit_context,
        ):
            nvx.parse_args(argv)
        self.assertEqual(exit_context.exception.code, 2)

    def test_classify_ci_changes_accepts_the_classified_events(self):
        # Full runs select every suite without classifying a change.
        self.assertEqual(
            ci.CI_CLASSIFIED_EVENTS, ("pull_request", "merge_group", "push")
        )
        for event_name in ci.CI_EVENTS:
            argv = [
                "classify-ci-changes",
                "--event-name",
                event_name,
                "--changed-files",
                "changed-files",
            ]
            with self.subTest(event_name=event_name):
                if event_name in ci.CI_CLASSIFIED_EVENTS:
                    self.assertEqual(nvx.parse_args(argv).event_name, event_name)
                    continue
                with (
                    patch("sys.stderr", new_callable=io.StringIO),
                    self.assertRaises(SystemExit) as exit_context,
                ):
                    nvx.parse_args(argv)
                self.assertEqual(exit_context.exception.code, 2)

    def test_check_required_ci_passes_each_flag_to_its_suite(self):
        # With the results that a selection expects, the command passes; with
        # any one flag flipped, the expected results differ, so it fails. A
        # flag passed to the wrong suite would fail the first or pass the
        # second.
        for changes in (
            ci.CiChanges(True, True, True, True, True),
            ci.CiChanges(True, True, False, True, False),
            ci.CiChanges(True, True, True, False, True),
        ):
            results = ci.required_ci_expected_results(
                "pull_request", same_repository=True, changes=changes
            )
            environment = {
                ci.REQUIRED_CI_RESULT_ENVIRONMENTS[job]: result
                for job, result in results.items()
            }
            for flipped in (None, *ci.CI_CHANGE_OUTPUTS):
                argv = [
                    "check-required-ci",
                    "--event-name",
                    "pull_request",
                    "--same-repository",
                    "true",
                ]
                for output, selected in changes.outputs().items():
                    argv += [
                        f"--{output}",
                        str(selected != (output == flipped)).lower(),
                    ]
                args = nvx.parse_args(argv)
                with (
                    self.subTest(changes=changes, flipped=flipped),
                    patch.dict(os.environ, environment),
                    patch("sys.stdout", new_callable=io.StringIO),
                ):
                    if flipped is None:
                        args.handler(args)
                    else:
                        with self.assertRaisesRegex(
                            common.ScriptError, "required CI job result"
                        ):
                            args.handler(args)

    def test_guest_selection_includes_azure_linux(self):
        args = nvx.parse_args(["build-initramfs", "--guest", "azurelinux"])
        self.assertEqual(args.guest, "azurelinux")
        self.assertEqual(
            guests.guest_descriptor(args.guest).initramfs_name,
            "initramfs-azurelinux.cpio.gz",
        )

    def test_usage_states_each_guest_memory_default(self):
        usage = (BuildConstants.REPO_ROOT / "doc" / "usage.md").read_text(
            encoding="utf-8"
        )
        row = next(
            line
            for line in usage.splitlines()
            if line.startswith("| `--memory-mib MIB`")
        )
        labels = {"alpine": "Alpine", "ubuntu": "Ubuntu", "azurelinux": "Azure Linux"}
        for name in guests.GUEST_NAMES:
            default = guests.guest_descriptor(name).default_memory_mib
            with self.subTest(guest=name):
                self.assertIn(f"{default} for {labels[name]}", row)

    def test_benchmark_exposes_device_restore_profile(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "device-restore-profile",
                "--restore-devices",
                "console",
                "virtiofs",
                "--restore-modes",
                "deferred",
                "--output-dir",
                "results",
            ]
        )

        benchmark.apply_benchmark_suite_defaults(args)
        self.assertEqual(args.restore_devices, ["console", "virtiofs"])
        self.assertEqual(args.restore_modes, ["deferred"])
        self.assertEqual(args.warmups, 1)
        self.assertEqual(args.runs, 5)
        self.assertEqual(args.output_dir, Path("results"))
        self.assertIs(args.handler, nvx.command_benchmark)

    def test_benchmark_exposes_non_python_performance_suite(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "performance",
                "--shell-memories",
                "64",
                "256",
                "--output-dir",
                "results",
            ]
        )

        self.assertEqual(args.suite, "performance")
        self.assertEqual(args.shell_memories, [64, 256])
        self.assertEqual(args.virtfs_runs, 3)
        self.assertEqual(args.payload_mib, 64)
        self.assertEqual(args.network_memory_mib, 256)
        self.assertEqual(args.host_cpu_reserve, 2)
        self.assertEqual(args.output_dir, Path("results"))
        self.assertIs(args.handler, nvx.command_benchmark)

    def test_device_io_uses_canonical_sampling_defaults(self):
        args = nvx.parse_args(["benchmark", "--suite", "device-io"])

        benchmark.apply_benchmark_suite_defaults(args)

        self.assertEqual(args.warmups, 5)
        self.assertEqual(args.runs, 30)
        self.assertEqual(args.device_io_duration_seconds, 10.0)
        self.assertEqual(args.device_io_size_mib, 512)
        self.assertEqual(args.device_io_port, 5201)
        self.assertFalse(hasattr(args, "device_io_abi"))

        with self.assertRaises(SystemExit):
            nvx.parse_args(
                ["benchmark", "--suite", "device-io", "--device-io-abi", "1"]
            )

    def test_benchmark_exposes_restore_only_shell_suite(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "shell-snapshot-restore",
                "--shell-memories",
                "512",
                "--processors",
                "8",
                "--output-dir",
                "results",
            ]
        )

        self.assertEqual(args.suite, "shell-snapshot-restore")
        self.assertEqual(args.shell_memories, [512])
        self.assertEqual(args.processors, 8)
        self.assertIs(args.handler, nvx.command_benchmark)

    def test_benchmark_exposes_restore_vcpu_matrix(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "snapshot-restore-vcpu",
                "--processors",
                "8",
                "--memory-mib",
                "512",
                "--output-dir",
                "results",
            ]
        )

        self.assertEqual(args.suite, "snapshot-restore-vcpu")
        self.assertEqual(args.processors, 8)
        self.assertEqual(args.memory_mib, 512)
        self.assertIs(args.handler, nvx.command_benchmark)

    def test_benchmark_exposes_platform_restore_vcpu_matrix(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "platform-snapshot-restore-vcpu",
                "--processors",
                "8",
                "--output-dir",
                "results",
            ]
        )

        self.assertEqual(args.suite, "platform-snapshot-restore-vcpu")
        self.assertIn(args.suite, benchmark.WORKLOAD_SUITES)
        self.assertIs(args.handler, nvx.command_benchmark)

    def test_benchmark_exposes_snapshot_profile_matrix(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "snapshot-profile",
                "--backend",
                "mshv",
                "--cache-state",
                "cold",
            ]
        )

        self.assertEqual(args.suite, "snapshot-profile")
        self.assertIsNone(args.shell_memories)
        benchmark.apply_benchmark_suite_defaults(args)
        self.assertEqual(args.shell_memories, [128, 256, 512, 1024])
        self.assertEqual(args.cache_state, "cold")
        self.assertFalse(args.snapshot_profile)
        self.assertIs(args.handler, nvx.command_benchmark)

    def test_benchmark_preserves_canonical_shell_memory_defaults(self):
        args = nvx.parse_args(["benchmark", "--suite", "performance"])

        benchmark.apply_benchmark_suite_defaults(args)

        self.assertEqual(args.shell_memories, [128, 256, 512])

    def test_benchmark_positive_integer_diagnostics_are_stable(self):
        for value, message in (
            ("not-an-int", "argument --runs: invalid positive_int value: 'not-an-int'"),
            ("0", "argument --runs: must be at least 1"),
        ):
            with self.subTest(value=value):
                with (
                    patch("sys.stderr", new_callable=io.StringIO) as errors,
                    self.assertRaises(SystemExit) as exit_context,
                ):
                    nvx.parse_args(["benchmark", "--runs", value])

                self.assertEqual(exit_context.exception.code, 2)
                self.assertIn(message, errors.getvalue())

    def test_release_commands_keep_their_cli_contract(self):
        # nvx download replaced the download command.
        with (
            patch("sys.stderr", new_callable=io.StringIO) as errors,
            self.assertRaises(SystemExit) as exit_context,
        ):
            nvx.parse_args(["download"])
        self.assertEqual(exit_context.exception.code, 2)
        self.assertIn("invalid choice: 'download'", errors.getvalue())

        collect = nvx.parse_args(["collect-sources"])
        self.assertEqual(collect.command, "collect-sources")
        self.assertIs(collect.handler, nvx.command_collect_sources)

        package = nvx.parse_args(
            [
                "package",
                "--version",
                "1.2.3",
                "--destination",
                "output",
                "--include-source",
                "--force",
            ]
        )
        self.assertEqual(package.command, "package")
        self.assertEqual(package.version, "1.2.3")
        self.assertEqual(package.destination, Path("output"))
        self.assertTrue(package.include_source)
        self.assertFalse(package.binary_only)
        self.assertTrue(package.force)
        self.assertIs(package.handler, nvx.command_package)

        verify = nvx.parse_args(["verify"])
        self.assertEqual(verify.command, "verify")
        self.assertIs(verify.handler, nvx.command_verify)

        openvmm_unit_tests = nvx.parse_args(["test-openvmm-unit"])
        self.assertEqual(openvmm_unit_tests.command, "test-openvmm-unit")
        self.assertIs(openvmm_unit_tests.handler, nvx.command_test_openvmm_unit)

        openvmm_tests = nvx.parse_args(["test-openvmm", "--backend", "mshv"])
        self.assertEqual(openvmm_tests.backend, "mshv")
        self.assertIs(openvmm_tests.handler, nvx.command_test_openvmm)

        # CI checks OpenVMM's own CI of the pin instead of building and running
        # OpenVMM's tests, so the commands that handed its test builds and
        # shards between jobs are gone (doc/ci.md#openvmm-ci).
        for arguments in (
            ["test-openvmm", "--backend", "kvm", "--content", "content"],
            ["test-openvmm", "--backend", "whp", "--shard", "2/2"],
            ["build-openvmm-tests", "--backend", "kvm", "--output", "content"],
            ["openvmm-test-cache-key", "--backend", "kvm"],
            ["check-openvmm-tests", "--backend", "whp", "--content", "content"],
            ["reuse-openvmm-tests", "--backend", "whp", "--content", "content"],
            ["plan-openvmm-test-shards", "--report", "reports"],
        ):
            with (
                self.subTest(arguments=arguments),
                patch("sys.stderr", new_callable=io.StringIO),
                self.assertRaises(SystemExit) as exit_context,
            ):
                nvx.parse_args(arguments)
            self.assertEqual(exit_context.exception.code, 2)

        openvmm_ci_check = nvx.parse_args(["check-openvmm-ci"])
        self.assertIsNone(openvmm_ci_check.revision)
        self.assertEqual(openvmm_ci_check.wait_minutes, 0)
        self.assertIsNone(openvmm_ci_check.summary)
        self.assertIs(openvmm_ci_check.handler, openvmm_ci.command_check_openvmm_ci)
        openvmm_ci_check = nvx.parse_args(
            [
                "check-openvmm-ci",
                "--revision",
                "a" * 40,
                "--wait-minutes",
                "300",
                "--summary",
                "summary.md",
            ]
        )
        self.assertEqual(openvmm_ci_check.revision, "a" * 40)
        self.assertEqual(openvmm_ci_check.wait_minutes, 300)
        self.assertEqual(openvmm_ci_check.summary, Path("summary.md"))
        for value in ("-1", "1.5", "an hour", ""):
            with (
                self.subTest(wait_minutes=value),
                patch("sys.stderr", new_callable=io.StringIO),
                self.assertRaises(SystemExit),
            ):
                nvx.parse_args(["check-openvmm-ci", "--wait-minutes", value])

    def test_openvmm_help_describes_petri_vmm_tests(self):
        with (
            patch("sys.stdout", new_callable=io.StringIO) as output,
            self.assertRaises(SystemExit) as exit_context,
        ):
            nvx.parse_args(["--help"])

        self.assertEqual(exit_context.exception.code, 0)
        help_text = output.getvalue()
        self.assertIn("run OpenVMM Petri VMM tests", help_text)
        self.assertNotIn("run OpenVMM microVM integration tests", help_text)

    def test_guest_selection_cli_contract(self):
        default_build = nvx.parse_args(["build-guest"])
        self.assertEqual(default_build.guest, "alpine")
        self.assertFalse(default_build.debug_kernel)

        debug_guest = nvx.parse_args(["build-guest", "--debug-kernel"])
        self.assertTrue(debug_guest.debug_kernel)
        self.assertTrue(nvx._build_config(debug_guest).debug_kernel)
        self.assertTrue(nvx.parse_args(["build", "--debug-kernel"]).debug_kernel)

        self.assertFalse(nvx.parse_args(["build-kernel"]).debug)
        self.assertTrue(nvx.parse_args(["build-kernel", "--debug"]).debug)

        all_guests = nvx.parse_args(["build-guest", "--guest", "all"])
        self.assertEqual(all_guests.guest, "all")

        ubuntu_initramfs = nvx.parse_args(["build-initramfs", "--guest", "ubuntu"])
        self.assertEqual(ubuntu_initramfs.guest, "ubuntu")

        azurelinux_initramfs = nvx.parse_args(
            ["build-initramfs", "--guest", "azurelinux"]
        )
        with patch("nvx.build_docker_initramfs") as build_docker_initramfs:
            nvx.command_build_initramfs(azurelinux_initramfs)
        build_docker_initramfs.assert_called_once_with(
            build.DockerBuildConfig(artifact_destination=BuildConstants.BUILD_DIR),
            "azurelinux",
        )

        distro = nvx.parse_args(
            [
                "build-distro-layer",
                "--guest",
                "ubuntu",
                "--output",
                "ubuntu.erofs",
                "--replace",
            ]
        )
        self.assertEqual(distro.guest, "ubuntu")
        self.assertEqual(distro.output, Path("ubuntu.erofs"))
        self.assertTrue(distro.replace)

        scratch = nvx.parse_args(["build-scratch-template"])
        self.assertEqual(
            scratch.output,
            common.artifact_path(UbuntuBuildConstants.SCRATCH_TEMPLATE_NAME),
        )
        self.assertFalse(scratch.replace)
        with patch("nvx.build_scratch_template") as build_scratch_template:
            nvx.command_build_scratch_template(
                nvx.parse_args(
                    [
                        "build-scratch-template",
                        "--output",
                        "scratch.ext4",
                        "--replace",
                    ]
                )
            )
        build_scratch_template.assert_called_once_with(
            build_config.ScratchTemplateBuildConfig(
                output=Path("scratch.ext4"), replace=True
            )
        )

    def test_docker_initramfs_rejects_native_guest(self):
        with self.assertRaisesRegex(
            common.ScriptError, "Alpine Linux.*do not require Docker"
        ):
            build.build_docker_initramfs(build.DockerBuildConfig(), "alpine")

    def test_docker_initramfs_requires_expected_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = build.DockerBuildConfig(artifact_destination=Path(temporary))
            with (
                patch.object(build, "require_tool"),
                patch.object(build, "run_checked") as run_checked,
                self.assertRaisesRegex(
                    common.ScriptError,
                    "initramfs-azurelinux.cpio.gz",
                ),
            ):
                build.build_docker_initramfs(config, "azurelinux")
        command = run_checked.call_args.args[0]
        self.assertEqual(
            command[command.index("--target") + 1],
            "azurelinux-initramfs-artifacts",
        )

    def test_docker_artifacts_require_expected_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = build.DockerBuildConfig(artifact_destination=Path(temporary))
            with (
                patch.object(build, "require_tool"),
                patch.object(build, "run_checked"),
                self.assertRaisesRegex(common.ScriptError, "vmlinux"),
            ):
                build.build_docker_artifacts(config, "azurelinux")

    def test_benchmark_refuses_a_cpu_that_no_built_in_profile_serves(self):
        # The command guards the benchmark coordinator, which it runs after.
        self.assertIs(nvx.run_benchmark, benchmark.run)
        sapphire_rapids = HostCpu(
            "GenuineIntel", 6, 143, 8, "Intel(R) Xeon(R) Platinum 8480C"
        )
        icelake = HostCpu("GenuineIntel", 6, 106, 6)
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)

        def checkout(name: str, *generations: CpuGeneration) -> str:
            # An OpenVMM checkout that pins profiles of these generations, with
            # the fields that NVX reads.
            directory = Path(temporary.name) / name
            profiles = directory / OPENVMM_CPU_PROFILES
            profiles.mkdir(parents=True)
            for generation in generations:
                document = {
                    "vendor": generation.vendor,
                    "generation": {
                        "name": generation.name,
                        "cpus": [
                            {
                                "family": cpu.family,
                                "model": cpu.model,
                                "steppings": [
                                    cpu.steppings.start,
                                    cpu.steppings.stop - 1,
                                ],
                            }
                            for cpu in generation.cpus
                        ],
                    },
                }
                (profiles / f"{generation.name}.json").write_text(
                    json.dumps(document), encoding="utf-8"
                )
            return str(directory)

        pinned = checkout("pinned", *CPU_GENERATIONS)

        def run_command(
            host: HostCpu | None, *arguments: str, openvmm_dir: str = pinned
        ) -> tuple[int, str, list[argparse.Namespace]]:
            stderr = io.StringIO()
            launched: list[argparse.Namespace] = []

            def run(args: argparse.Namespace) -> int:
                launched.append(args)
                return 0

            with (
                patch.object(nvx, "host_cpu_signature", return_value=host),
                patch.object(nvx, "run_benchmark", side_effect=run),
                contextlib.redirect_stderr(stderr),
            ):
                status = nvx.main(
                    ["benchmark", "--openvmm-dir", openvmm_dir, *arguments]
                )
            return status, stderr.getvalue(), launched

        # The refusal comes before the coordinator builds or measures anything.
        status, output, launched = run_command(
            sapphire_rapids, "--suite", "e2e", "--backend", "kvm"
        )
        self.assertEqual((status, launched), (1, []))
        self.assertTrue(
            output.startswith(
                "error: benchmark refuses this host: no built-in CPU profile serves "
                "its CPU, GenuineIntel 6/143/8 (Intel(R) Xeon(R) Platinum 8480C)"
            ),
            output,
        )
        self.assertIn("openvmm --hypervisor kvm --cpu-fingerprint", output)
        self.assertIn(cpu_profile_request_url(sapphire_rapids), output)
        # On Windows, both backends name WHP's fingerprint.
        status, output, launched = run_command(sapphire_rapids, "--backend", "both")
        self.assertEqual((status, launched), (1, []))
        self.assertIn("openvmm --hypervisor whp --cpu-fingerprint", output)
        # A CPU that cannot be identified is refused too.
        status, output, launched = run_command(None)
        self.assertEqual((status, launched), (1, []))
        self.assertIn("it cannot identify the host's CPU", output)
        # The phase 2 suite boots no microVM, so it runs on any CPU, and a CPU
        # that a built-in profile serves runs every suite.
        for host, arguments in (
            (sapphire_rapids, ("--suite", "phase2")),
            (None, ("--suite", "phase2")),
            (icelake, ("--suite", "e2e")),
            (icelake, ()),
        ):
            with self.subTest(host=host, arguments=arguments):
                status, output, launched = run_command(host, *arguments)
                self.assertEqual((status, output), (0, ""))
                self.assertEqual(len(launched), 1)
        # The profiles that decide are those of the OpenVMM checkout that
        # benchmark builds: one that adds a profile for a CPU benchmarks it, and
        # one that lacks a CPU's profile refuses it.
        sapphirerapids = CpuGeneration(
            "sapphirerapids", "GenuineIntel", (CpuModel(6, 143),)
        )
        candidate = checkout("candidate", *CPU_GENERATIONS, sapphirerapids)
        status, output, launched = run_command(
            sapphire_rapids, "--suite", "e2e", openvmm_dir=candidate
        )
        self.assertEqual((status, output), (0, ""))
        self.assertEqual([args.openvmm_dir for args in launched], [Path(candidate)])
        lacking = checkout(
            "lacking",
            *(
                generation
                for generation in CPU_GENERATIONS
                if generation.name != "icelake-sp"
            ),
        )
        status, output, launched = run_command(icelake, openvmm_dir=lacking)
        self.assertEqual((status, launched), (1, []))
        self.assertIn(
            "no built-in CPU profile serves its CPU, GenuineIntel 6/106/6", output
        )
        # A checkout whose generations overlap on a CPU is refused there, since
        # every cold boot fails on it.
        ambiguous = checkout(
            "ambiguous",
            *CPU_GENERATIONS,
            CpuGeneration("icelake-x", "GenuineIntel", (CpuModel(6, 106),)),
        )
        status, output, launched = run_command(icelake, openvmm_dir=ambiguous)
        self.assertEqual((status, launched), (1, []))
        self.assertIn(
            "is in more than one generation of the built-in CPU profiles "
            "(icelake-sp, icelake-x)",
            output,
        )
        # A checkout whose profiles cannot be read is refused, except for the
        # phase 2 suite.
        missing = str(Path(temporary.name) / "missing")
        status, output, launched = run_command(icelake, openvmm_dir=missing)
        self.assertEqual((status, launched), (1, []))
        self.assertTrue(
            output.startswith(
                "error: benchmark cannot confirm that a built-in CPU profile serves "
                f"this host's CPU: the OpenVMM checkout {missing} pins no CPU profile"
            ),
            output,
        )
        status, output, launched = run_command(
            icelake, "--suite", "phase2", openvmm_dir=missing
        )
        self.assertEqual((status, output, len(launched)), (0, "", 1))
        # A KVM worker, which the Windows coordinator reinvokes inside WSL
        # without its --openvmm-dir, runs the binaries that the coordinator
        # staged after the check, so it skips the check, whatever its checkout.
        status, output, launched = run_command(
            sapphire_rapids, "--_kvm-worker", openvmm_dir=missing
        )
        self.assertEqual((status, output, len(launched)), (0, "", 1))

    def test_openvmm_command_passes_restore_choice_in_build_config(self):
        with patch.object(nvx, "build_openvmm") as build_openvmm:
            nvx.command_build_openvmm(argparse.Namespace(skip_restore=True))

        config = build_openvmm.call_args.args[0]
        self.assertIsInstance(config, build_config.OpenVmmBuildConfig)
        self.assertTrue(config.skip_restore)
        self.assertIsNone(config.backend)

    def test_build_commands_pass_backend_choice_in_config(self):
        for command in ("build-openvmm", "build"):
            for backend in (None, "kvm", "mshv", "whp"):
                with self.subTest(command=command, backend=backend):
                    arguments = [command, "--skip-restore"]
                    if backend is not None:
                        arguments.extend(["--backend", backend])
                    if command == "build":
                        arguments.append("--native")
                    args = nvx.parse_args(arguments)
                    with (
                        patch.object(nvx, "build_openvmm") as openvmm,
                        patch.object(nvx, "build_all") as combined,
                    ):
                        args.handler(args)

                    if command == "build":
                        aggregate = combined.call_args.args[0]
                        self.assertIsInstance(aggregate, build_config.BuildConfig)
                        self.assertTrue(aggregate.native_guest)
                        config = aggregate.openvmm
                        openvmm.assert_not_called()
                    else:
                        config = openvmm.call_args.args[0]
                        combined.assert_not_called()
                    self.assertIsInstance(config, build_config.OpenVmmBuildConfig)
                    self.assertEqual(config.backend, backend)
                    self.assertTrue(config.skip_restore)

    def test_build_commands_reject_unknown_backends(self):
        for command in ("build-openvmm", "build"):
            with self.subTest(command=command), self.assertRaises(SystemExit):
                nvx.parse_args([command, "--backend", "unknown"])

    def test_build_commands_pass_specialized_configs_to_consumers(self):
        with (
            patch.object(nvx, "build_kernel") as build_kernel,
            patch.object(nvx, "build_initramfs") as build_initramfs,
            patch.object(nvx, "record_openvmm_provenance") as provenance,
            patch.object(nvx, "collect_release_sources") as collect_sources,
        ):
            nvx.command_build_kernel(argparse.Namespace())
            nvx.command_build_initramfs(argparse.Namespace(guest="alpine"))
            nvx.command_record_openvmm_provenance(argparse.Namespace())
            nvx.command_collect_sources(argparse.Namespace())

        self.assertIsInstance(
            build_kernel.call_args.args[0],
            build_config.KernelBuildConfig,
        )
        self.assertFalse(build_kernel.call_args.args[0].debug)
        with patch.object(nvx, "build_kernel") as build_debug_kernel:
            nvx.command_build_kernel(argparse.Namespace(debug=True))
        self.assertEqual(
            build_debug_kernel.call_args.args[0],
            build_config.KernelBuildConfig.debug_variant(),
        )
        self.assertIsInstance(
            build_initramfs.call_args.args[0],
            build_config.InitramfsBuildConfig,
        )
        self.assertIsInstance(
            provenance.call_args.args[0],
            build_config.OpenVmmBuildConfig,
        )
        self.assertIsInstance(
            collect_sources.call_args.args[0],
            build_config.DockerBuildConfig,
        )

    def test_record_openvmm_provenance_command_is_exposed(self):
        args = nvx.parse_args(["record-openvmm-provenance"])
        self.assertIs(args.handler, nvx.command_record_openvmm_provenance)

    def test_package_command_forwards_parsed_options(self):
        args = nvx.parse_args(
            [
                "package",
                "--version",
                "2.0.0",
                "--destination",
                "dist/custom",
                "--binary-only",
                "--force",
            ]
        )

        with patch.object(nvx, "package_release") as package_release:
            args.handler(args)

        package_release.assert_called_once_with(
            version="2.0.0",
            destination=Path("dist/custom"),
            include_source=False,
            force=True,
        )

    def test_archive_release_command_forwards_paths(self):
        args = nvx.parse_args(
            [
                "archive-release",
                "--source",
                "dist/package",
                "--destination",
                "dist/package.zip",
            ]
        )

        with patch.object(nvx, "create_release_archive") as create_archive:
            args.handler(args)

        create_archive.assert_called_once_with(
            Path("dist/package"),
            Path("dist/package.zip"),
        )


if __name__ == "__main__":
    unittest.main()

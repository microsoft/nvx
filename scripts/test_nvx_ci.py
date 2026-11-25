#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import os
import re
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import (  # noqa: E402
    ci,
    common,
)
from nvx_tools.build_constants import (  # noqa: E402
    OpenVMMBuildConstants,
)


class CiTests(unittest.TestCase):
    def test_openvmm_unit_tests_run_nextest_and_doctests(self):
        with tempfile.TemporaryDirectory() as temporary:
            openvmm = Path(temporary) / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            fuzz_crates = common.CommandResult(
                args=("cargo", "xtask", "fuzz", "list", "--crates"),
                returncode=0,
                stdout=b"fuzz_alpha\nfuzz_beta\n",
                stderr=b"",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(ci, "require_tool", return_value="cargo"),
                patch.object(ci, "run_capture", return_value=fuzz_crates) as capture,
                patch.object(ci, "run_checked") as run_checked,
                patch.dict(os.environ, {"NVX_TEST_VARIABLE": "1"}, clear=True),
            ):
                ci.run_openvmm_unit_tests()

            environment = capture.call_args.kwargs["env"]
            expected_environment = {"NVX_TEST_VARIABLE": "1"}
            if os.name == "nt":
                expected_environment["_LINK_"] = ci.MSVC_REPRODUCIBLE_LINK_OPTION
            self.assertEqual(environment, expected_environment)
            capture.assert_called_once_with(
                ["cargo", "xtask", "fuzz", "list", "--crates"],
                cwd=openvmm,
                env=environment,
            )
            self.assertEqual(run_checked.call_count, 3)
            for command in run_checked.call_args_list:
                self.assertIs(command.kwargs["env"], environment)
            self.assertEqual(
                run_checked.call_args_list[0],
                call(
                    [
                        "cargo",
                        "xflowey",
                        "restore-packages",
                        "--no-compat-igvm",
                    ],
                    cwd=openvmm,
                    env=environment,
                ),
            )
            command = run_checked.call_args_list[1].args[0]
            self.assertEqual(
                command[:10],
                [
                    "cargo",
                    "nextest",
                    "run",
                    "--profile",
                    "agent",
                    "--workspace",
                    "--tests",
                    "--bins",
                    "--features",
                    "ci",
                ],
            )
            excluded_packages = [
                command[index + 1]
                for index, argument in enumerate(command)
                if argument == "--exclude"
            ]
            self.assertEqual(
                excluded_packages,
                [
                    *ci.OPENVMM_UNIT_TEST_EXCLUDED_PACKAGES,
                    "fuzz_alpha",
                    "fuzz_beta",
                ],
            )
            self.assertEqual(run_checked.call_args_list[1].kwargs["cwd"], openvmm)
            self.assertEqual(
                run_checked.call_args_list[2],
                call(
                    [
                        "cargo",
                        "test",
                        "--locked",
                        "--doc",
                        "--workspace",
                        "--no-fail-fast",
                    ],
                    cwd=openvmm,
                    env=environment,
                ),
            )

    def test_openvmm_windows_builds_link_reproducibly(self):
        for options, expected in (
            (None, "/Brepro"),
            ("", "/Brepro"),
            ("/DEBUG:FULL", "/DEBUG:FULL /Brepro"),
            ("/brepro", "/brepro"),
            ("-BREPRO /DEBUG:FULL", "-BREPRO /DEBUG:FULL"),
        ):
            with self.subTest(options=options):
                environment = {} if options is None else {"_LINK_": options}
                ci._link_reproducibly_with_msvc(environment)
                self.assertEqual(environment, {"_LINK_": expected})

    def test_openvmm_unit_tests_stop_when_fuzz_crate_query_fails(self):
        with tempfile.TemporaryDirectory() as temporary:
            openvmm = Path(temporary) / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            failed_query = common.CommandResult(
                args=("cargo", "xtask", "fuzz", "list", "--crates"),
                returncode=2,
                stdout=b"",
                stderr=b"query failed",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(ci, "require_tool", return_value="cargo"),
                patch.object(ci, "run_capture", return_value=failed_query),
                patch.object(ci, "run_checked") as run_checked,
                self.assertRaisesRegex(
                    common.ScriptError,
                    "OpenVMM fuzz crate query exited 2",
                ),
            ):
                ci.run_openvmm_unit_tests()

            run_checked.assert_not_called()

    def test_openvmm_tests_use_nvx_guest_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm = root / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            kernel = root / "vmlinux"
            initrd = root / "initramfs.cpio.gz"
            kernel.touch()
            initrd.touch()
            backend = "whp" if os.name == "nt" else "kvm"
            installed_targets = common.CommandResult(
                args=("rustup", "target", "list"),
                returncode=0,
                stdout=(
                    "\n".join(OpenVMMBuildConstants.TEST_RUST_TARGETS[backend]) + "\n"
                ).encode(),
                stderr=b"",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(
                    ci,
                    "artifact_path",
                    side_effect=[kernel, initrd],
                ),
                patch.object(ci.os, "access", return_value=True),
                patch.object(ci.Path, "exists", return_value=False),
                patch.object(ci, "require_tool", side_effect=["cargo", "rustup"]),
                patch.object(
                    ci,
                    "run_capture",
                    return_value=installed_targets,
                ) as run_capture,
                patch.object(ci, "run_checked") as run_checked,
                patch.dict(
                    os.environ,
                    {
                        "PETRI_CAPABILITIES": "vpci",
                        "RUNNER_TEMP": os.fspath(root),
                        "_LINK_": "",
                    },
                ),
            ):
                ci.run_openvmm_tests(backend)

            run_capture.assert_called_once_with(
                [
                    "rustup",
                    "target",
                    "list",
                    "--installed",
                    "--toolchain",
                    OpenVMMBuildConstants.RUST_TOOLCHAIN,
                ]
            )
            self.assertEqual(run_checked.call_count, 2)
            restore, tests = run_checked.call_args_list
            self.assertEqual(
                restore.args[0],
                ["cargo", "xflowey", "restore-packages", "--no-compat-igvm"],
            )
            environment = restore.kwargs["env"]
            self.assertEqual(
                environment["RUSTUP_TOOLCHAIN"],
                OpenVMMBuildConstants.RUST_TOOLCHAIN,
            )
            if os.name == "nt":
                self.assertNotIn("XDG_CACHE_HOME", environment)
            else:
                self.assertEqual(
                    environment["XDG_CACHE_HOME"],
                    os.fspath(root / "openvmm-cache"),
                )
            command = tests.args[0]
            self.assertEqual(command[:3], ["cargo", "xflowey", "vmm-tests-run"])
            filter_index = command.index("--filter")
            self.assertEqual(
                command[filter_index + 1],
                ci.OPENVMM_TEST_FILTERS[backend],
            )
            self.assertEqual(tests.kwargs["cwd"], openvmm)
            self.assertIs(tests.kwargs["env"], environment)
            self.assertEqual(
                environment["OPENVMM_MICROVM_TEST_KERNEL"],
                os.fspath(kernel.resolve()),
            )
            self.assertEqual(
                environment["OPENVMM_MICROVM_TEST_INITRD"],
                os.fspath(initrd.resolve()),
            )
            self.assertEqual(environment["PETRI_CAPABILITIES"], "vpci")
            self.assertEqual(
                environment["_LINK_"],
                ci.MSVC_REPRODUCIBLE_LINK_OPTION if os.name == "nt" else "",
            )
            if os.name == "nt":
                self.assertEqual(
                    command[command.index("--dir") + 1],
                    os.fspath(root / backend),
                )

    def test_openvmm_tests_pin_toolchain_without_runner_temp(self):
        installed_targets = common.CommandResult(
            args=("rustup", "target", "list"),
            returncode=0,
            stdout=(
                "\n".join(OpenVMMBuildConstants.TEST_RUST_TARGETS["whp"]) + "\n"
            ).encode(),
            stderr=b"",
        )

        with (
            patch.object(ci, "run_capture", return_value=installed_targets),
            patch.object(ci, "run_checked") as run_checked,
            patch.dict(os.environ, {}, clear=True),
        ):
            environment = ci._prepare_openvmm_test_environment(("whp",), "rustup")

        expected_environment = {
            "RUSTUP_TOOLCHAIN": OpenVMMBuildConstants.RUST_TOOLCHAIN,
        }
        if os.name == "nt":
            expected_environment["_LINK_"] = ci.MSVC_REPRODUCIBLE_LINK_OPTION
        self.assertEqual(environment, expected_environment)
        run_checked.assert_not_called()

    def test_openvmm_tests_prepare_job_local_toolchain_for_missing_targets(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm = root / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            kernel = root / "vmlinux"
            initrd = root / "initramfs.cpio.gz"
            kernel.touch()
            initrd.touch()
            backend = "whp" if os.name == "nt" else "kvm"
            installed_targets = common.CommandResult(
                args=("rustup", "target", "list"),
                returncode=0,
                stdout=f"{OpenVMMBuildConstants.GUEST_RUST_TARGET}\n".encode(),
                stderr=b"",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(
                    ci,
                    "artifact_path",
                    side_effect=[kernel, initrd],
                ),
                patch.object(ci.os, "access", return_value=True),
                patch.object(ci.Path, "exists", return_value=False),
                patch.object(ci, "require_tool", side_effect=["cargo", "rustup"]),
                patch.object(ci, "run_capture", return_value=installed_targets),
                patch.object(ci, "run_checked") as run_checked,
                patch.dict(os.environ, {"RUNNER_TEMP": os.fspath(root)}),
            ):
                ci.run_openvmm_tests(backend)

            self.assertEqual(run_checked.call_count, 4)
            install_toolchain, install_targets, restore, tests = (
                run_checked.call_args_list
            )
            environment = install_toolchain.kwargs["env"]
            self.assertEqual(
                environment["RUSTUP_HOME"],
                os.fspath(root / "openvmm-rustup"),
            )
            self.assertEqual(
                environment["RUSTUP_TOOLCHAIN"],
                OpenVMMBuildConstants.RUST_TOOLCHAIN,
            )
            if os.name != "nt":
                self.assertEqual(
                    environment["XDG_CACHE_HOME"],
                    os.fspath(root / "openvmm-cache"),
                )
            self.assertEqual(
                install_toolchain.args[0],
                [
                    "rustup",
                    "toolchain",
                    "install",
                    OpenVMMBuildConstants.RUST_TOOLCHAIN,
                    "--profile",
                    "minimal",
                ],
            )
            self.assertEqual(
                install_targets.args[0],
                [
                    "rustup",
                    "target",
                    "add",
                    *OpenVMMBuildConstants.TEST_RUST_TARGETS[backend],
                    "--toolchain",
                    OpenVMMBuildConstants.RUST_TOOLCHAIN,
                ],
            )
            for command in (install_targets, restore, tests):
                self.assertIs(command.kwargs["env"], environment)

    def test_openvmm_tests_install_missing_pinned_toolchain(self):
        missing_toolchain = common.CommandResult(
            args=("rustup", "target", "list"),
            returncode=1,
            stdout=b"",
            stderr=(
                f"error: toolchain '{OpenVMMBuildConstants.RUST_TOOLCHAIN}' "
                "is not installed\n"
            ).encode(),
        )
        install_toolchain = [
            "rustup",
            "toolchain",
            "install",
            OpenVMMBuildConstants.RUST_TOOLCHAIN,
            "--profile",
            "minimal",
        ]
        add_targets = [
            "rustup",
            "target",
            "add",
            *OpenVMMBuildConstants.TEST_RUST_TARGETS["kvm"],
            "--toolchain",
            OpenVMMBuildConstants.RUST_TOOLCHAIN,
        ]

        with tempfile.TemporaryDirectory() as temporary:
            for runner_temp in (None, temporary):
                with (
                    self.subTest(runner_temp=runner_temp),
                    patch.object(ci, "run_capture", return_value=missing_toolchain),
                    patch.object(ci, "run_checked") as run_checked,
                    patch.dict(
                        os.environ,
                        {} if runner_temp is None else {"RUNNER_TEMP": runner_temp},
                        clear=True,
                    ),
                ):
                    environment = ci._prepare_openvmm_test_environment(
                        ("kvm",), "rustup"
                    )

                    self.assertEqual(
                        [command.args[0] for command in run_checked.call_args_list],
                        [install_toolchain, add_targets],
                    )
                    for command in run_checked.call_args_list:
                        self.assertIs(command.kwargs["env"], environment)
                    self.assertEqual(
                        environment["RUSTUP_TOOLCHAIN"],
                        OpenVMMBuildConstants.RUST_TOOLCHAIN,
                    )
                    self.assertEqual("_LINK_" in environment, os.name == "nt")
                    if runner_temp is None:
                        self.assertNotIn("RUSTUP_HOME", environment)
                    else:
                        self.assertEqual(
                            environment["RUSTUP_HOME"],
                            os.fspath(
                                Path(runner_temp)
                                / OpenVMMBuildConstants.RUSTUP_DIRECTORY_NAME
                            ),
                        )

    def test_openvmm_tests_define_each_backend_filter(self):
        self.assertEqual(
            set(ci.OPENVMM_TEST_FILTERS),
            set(ci.OPENVMM_TEST_BACKENDS),
        )
        self.assertEqual(
            set(OpenVMMBuildConstants.TEST_RUST_TARGETS),
            set(ci.OPENVMM_TEST_BACKENDS),
        )
        self.assertEqual(
            ci.OPENVMM_TEST_FILTERS["kvm"],
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertIn(
            "!test(no_vmbus_prepped_boot_no_vmbus_windows)",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertIn(
            "!test(windows_datacenter_core)",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertIn("!test(virtio_net_windows)", ci.OPENVMM_KVM_TEST_FILTER)
        self.assertEqual(
            set(ci.OPENVMM_KVM_EXCLUDED_TESTS),
            {
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
                "multiarch::openvmm_pcat_x64_ubuntu_2404_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot_heavy",
            },
        )
        for excluded_test in ci.OPENVMM_KVM_EXCLUDED_TESTS:
            self.assertIn(
                f"!{ci._exact_openvmm_test(excluded_test)}",
                ci.OPENVMM_KVM_TEST_FILTER,
            )
        self.assertNotIn("!test(openvmm_pcat_x64)", ci.OPENVMM_KVM_TEST_FILTER)
        self.assertNotIn(
            "openvmm_linux_x64_apicid_offset",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertNotIn(
            "openvmm_linux_x64_legacy_xapic",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertEqual(
            ci.OPENVMM_TEST_FILTERS["mshv"],
            ci.OPENVMM_MSHV_TEST_FILTER,
        )
        self.assertIn(
            "!test(windows_datacenter_core)",
            ci.OPENVMM_MSHV_TEST_FILTER,
        )
        self.assertEqual(
            set(ci.OPENVMM_MSHV_EXCLUDED_TESTS),
            {
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
                "multiarch::openvmm_pcat_x64_ubuntu_2404_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot_heavy",
                "multiarch::pcie::openvmm_linux_x64_pcie_save_restore",
            },
        )
        self.assertNotIn(
            "x86_64::openvmm_linux_x64_virtio_blk_device",
            ci.OPENVMM_MSHV_EXCLUDED_TESTS,
        )
        for excluded_test in ci.OPENVMM_MSHV_EXCLUDED_TESTS:
            self.assertIn(
                f"!{ci._exact_openvmm_test(excluded_test)}",
                ci.OPENVMM_MSHV_TEST_FILTER,
            )
        self.assertNotIn("!test(openvmm_pcat_x64)", ci.OPENVMM_MSHV_TEST_FILTER)
        for backend, test_filter in ci.OPENVMM_TEST_FILTERS.items():
            for required_test in ci.OPENVMM_REQUIRED_MICROVM_TESTS:
                with self.subTest(backend=backend, required_test=required_test):
                    self.assertIn(
                        ci._exact_openvmm_test(required_test),
                        test_filter,
                    )
        self.assertEqual(len(ci.OPENVMM_WHP_TESTS), 30)
        self.assertEqual(
            len(set(ci.OPENVMM_WHP_TESTS)),
            len(ci.OPENVMM_WHP_TESTS),
        )
        for required_test in ci.OPENVMM_REQUIRED_MICROVM_TESTS:
            self.assertIn(required_test, ci.OPENVMM_WHP_TESTS)
        # vmm-tests-run checks for Hyper-V when a selected test needs it, as
        # flowey reads from "hyperv" in a test's name, so no WHP test may need
        # Hyper-V, which WHP hosts need not run (#690).
        for test in ci.OPENVMM_WHP_TESTS:
            with self.subTest(whp_test=test):
                self.assertNotIn("hyperv", test)
        # Only Linux hosts can build the Linux pipette this TTRPC test boots.
        self.assertNotIn("test_ttrpc_interface", ci.OPENVMM_TEST_FILTERS["whp"])
        for backend in ("kvm", "mshv"):
            with self.subTest(backend=backend):
                self.assertIn("test(ttrpc)", ci.OPENVMM_TEST_FILTERS[backend])
                self.assertNotIn(
                    "test_ttrpc_interface",
                    ci.OPENVMM_TEST_FILTERS[backend],
                )
                self.assertIn("!test(vm_host)", ci.OPENVMM_TEST_FILTERS[backend])
        self.assertNotIn("vm_host", ci.OPENVMM_TEST_FILTERS["whp"])
        self.assertEqual(ci.OPENVMM_WHP_EXCLUDED_TESTS, ())
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_windows_datacenter_core_2022_x64_boot_heavy",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_windows_datacenter_core_2022_x64_boot",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::pcie::openvmm_uefi_x64_windows_datacenter_core_2022_x64_pcie_nvme_boot",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertTrue(
            set(ci.OPENVMM_WHP_EXCLUDED_TESTS).issubset(ci.OPENVMM_WHP_TESTS)
        )
        self.assertEqual(
            ci.OPENVMM_TEST_FILTERS["whp"],
            ci._join_openvmm_tests(
                ci.OPENVMM_WHP_TESTS,
                ci.OPENVMM_WHP_EXCLUDED_TESTS,
            ),
        )
        self.assertEqual(
            ci._join_openvmm_tests(
                ("suite::boot", "suite::boot_heavy"),
                ("suite::boot_heavy",),
            ),
            (
                "(test(/^suite::boot$/) | test(/^suite::boot_heavy$/))"
                " & !test(/^suite::boot_heavy$/)"
            ),
        )

    def test_linux_filters_leave_out_every_windows_server_test(self):
        # The Linux legs run Linux guests. A test's name holds the year of its
        # Windows Server image, so a test that moves to another image, as the
        # KVP IC test moved from 2022 to 2025 (#1035), must stay out of them.
        windows_tests = [
            test for test in ci.OPENVMM_WHP_TESTS if "windows_datacenter_core" in test
        ]
        self.assertIn(
            "multiarch::ic::openvmm_uefi_x64_windows_datacenter_core_2025_x64_kvp_ic",
            windows_tests,
        )
        for backend in ("kvm", "mshv"):
            test_filter = ci.OPENVMM_TEST_FILTERS[backend]
            for test in windows_tests:
                with self.subTest(backend=backend, test=test):
                    self.assertFalse(_nextest_filter_matches(test_filter, test))
            for test in (
                "multiarch::ic::openvmm_uefi_x64_ubuntu_2504_server_x64_timesync_ic",
                *ci.OPENVMM_REQUIRED_MICROVM_TESTS,
            ):
                with self.subTest(backend=backend, test=test):
                    self.assertTrue(_nextest_filter_matches(test_filter, test))

    def test_openvmm_tests_reject_unknown_backend(self):
        with self.assertRaisesRegex(common.ScriptError, "unsupported.*backend"):
            ci.run_openvmm_tests("unknown")


def _nextest_filter_matches(expression: str, test: str) -> bool:
    """Return whether a nextest filter that OpenVMM's test filters spell,
    with test(/REGEX/), test(SUBSTRING), !, &, |, and parentheses, selects a
    test's name."""
    tokens = re.findall(
        r"test\(/(?:[^/\\]|\\.)*/\)|test\([^/)][^)]*\)|[!&|()]", expression
    )
    if "".join(tokens) != "".join(expression.split()):
        raise AssertionError(f"unsupported filter {expression!r}")
    position = 0

    def take() -> str:
        nonlocal position
        position += 1
        return tokens[position - 1]

    def peek() -> str | None:
        return tokens[position] if position < len(tokens) else None

    def primary() -> bool:
        token = take()
        if token == "!":
            return not primary()
        if token == "(":
            value = union()
            if take() != ")":
                raise AssertionError(f"unbalanced filter {expression!r}")
            return value
        if token.startswith("test(/"):
            return re.search(token[6:-2], test) is not None
        # Without slashes, test() matches a substring of the name.
        return token[5:-1] in test

    def intersection() -> bool:
        value = primary()
        while peek() == "&":
            take()
            value = primary() and value
        return value

    def union() -> bool:
        value = intersection()
        while peek() == "|":
            take()
            value = intersection() or value
        return value

    selected = union()
    if position != len(tokens):
        raise AssertionError(f"unbalanced filter {expression!r}")
    return selected


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import build, build_config, ci, common, release  # noqa: E402
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
    GuestRustBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
)


class KernelBuildTests(unittest.TestCase):
    def test_kernel_build_rejects_input_config_mutation_during_build(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            input_config = root / "kernel" / "config-microvm"
            input_config.parent.mkdir(parents=True)
            input_config.write_text(
                "\n".join(
                    (
                        *KernelBuildConstants.REQUIRED_DIRECT_BOOT_CONFIG,
                        *KernelBuildConstants.REQUIRED_VIRTIO_CONSOLE_CONFIG,
                        *KernelBuildConstants.REQUIRED_SHARED_STATUS_CONFIG,
                        *KernelBuildConstants.REQUIRED_SANDBOX_CONFIG,
                        *KernelBuildConstants.REQUIRED_NETWORK_CONFIG,
                        *KernelBuildConstants.REQUIRED_TIME_ABI_CONFIG,
                        *KernelBuildConstants.REQUIRED_HARDENING_CONFIG,
                    )
                )
                + "\n",
                encoding="utf-8",
            )
            patch_path = root / "kernel" / "patches" / "example.patch"
            patch_path.parent.mkdir()
            patch_path.write_text("patch", encoding="utf-8")
            source = root / "source"
            source.mkdir()
            work = root / "work"
            output = root / "output" / "vmlinux"
            prior_provenance = output.with_name(KernelBuildConstants.PROVENANCE_NAME)
            prior_provenance.parent.mkdir()
            prior_provenance.write_text("stale", encoding="utf-8")

            with patch.object(BuildConstants, "REPO_ROOT", root):
                source_fingerprint = build._kernel_source_fingerprint()

            def run_build(command: object, **_kwargs: object) -> None:
                if isinstance(command, list) and command[-1] == "vmlinux":
                    (work / "vmlinux").write_bytes(b"kernel")
                    input_config.write_text("CONFIG_CHANGED=y\n", encoding="utf-8")

            with (
                patch.object(BuildConstants, "REPO_ROOT", root),
                patch.object(build, "_require_linux"),
                patch.object(build, "require_tool", return_value="tool"),
                patch.object(
                    build,
                    "prepare_kernel_source",
                    return_value=(source, source_fingerprint),
                ),
                patch.object(build, "run_checked", side_effect=run_build),
                patch.object(build, "check_build_configuration"),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "inputs changed during the build",
                ),
            ):
                build.build_kernel(
                    build_config.KernelBuildConfig(
                        work=work,
                        output=output,
                    )
                )

            self.assertFalse(output.exists())
            self.assertFalse(output.with_name("vmlinux.config").exists())
            self.assertFalse(prior_provenance.exists())

    def test_materialize_kernel_provenance_inputs_bypasses_mutable_index(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            git_environment = {
                key: value
                for key, value in os.environ.items()
                if not key.startswith("GIT_CONFIG_")
            }
            config = root / "kernel" / "config-microvm"
            patch_path = root / "kernel" / "patches" / "example.patch"
            stale_patch = root / "kernel" / "patches" / "stale.patch"
            config.parent.mkdir(parents=True)
            patch_path.parent.mkdir()
            config.write_bytes(b"CONFIG_EXAMPLE=y\n")
            patch_path.write_bytes(b"patch\n")
            (root / ".gitattributes").write_text(
                "\n".join(
                    (
                        "kernel/config-microvm text eol=lf",
                        "kernel/patches/*.patch text eol=lf",
                    )
                )
                + "\n",
                encoding="ascii",
            )
            subprocess.run(
                ["git", "init", "-q"],
                cwd=root,
                env=git_environment,
                check=True,
            )
            subprocess.run(
                ["git", "config", "user.name", "NVX Tests"],
                cwd=root,
                env=git_environment,
                check=True,
            )
            subprocess.run(
                ["git", "config", "user.email", "nvx-tests@example.com"],
                cwd=root,
                env=git_environment,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "add",
                    ".gitattributes",
                    "kernel/config-microvm",
                    "kernel/patches/example.patch",
                ],
                cwd=root,
                env=git_environment,
                check=True,
            )
            subprocess.run(
                ["git", "commit", "-qm", "kernel provenance fixture"],
                cwd=root,
                env=git_environment,
                check=True,
            )
            git_dir = Path(
                subprocess.run(
                    ["git", "rev-parse", "--git-dir"],
                    cwd=root,
                    env=git_environment,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout.strip()
            )
            (root / git_dir / "info" / "attributes").write_text(
                "\n".join(
                    (
                        "kernel/config-microvm -text",
                        "kernel/patches/*.patch -text",
                    )
                )
                + "\n",
                encoding="ascii",
            )
            config.write_bytes(b"CONFIG_EXAMPLE=y\r\n")
            patch_path.write_bytes(b"patch\r\n")
            stale_patch.write_bytes(b"stale\r\n")
            subprocess.run(
                [
                    "git",
                    "add",
                    "kernel/config-microvm",
                    "kernel/patches/example.patch",
                    "kernel/patches/stale.patch",
                ],
                cwd=root,
                env=git_environment,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "checkout-index",
                    "--force",
                    "--",
                    "kernel/config-microvm",
                    "kernel/patches/example.patch",
                    "kernel/patches/stale.patch",
                ],
                cwd=root,
                env=git_environment,
                check=True,
            )
            self.assertEqual(config.read_bytes(), b"CONFIG_EXAMPLE=y\r\n")
            self.assertEqual(patch_path.read_bytes(), b"patch\r\n")
            self.assertEqual(stale_patch.read_bytes(), b"stale\r\n")

            with (
                patch.dict(os.environ, git_environment, clear=True),
                patch.object(BuildConstants, "REPO_ROOT", root),
            ):
                build.materialize_kernel_provenance_inputs()

            self.assertEqual(config.read_bytes(), b"CONFIG_EXAMPLE=y\n")
            self.assertEqual(patch_path.read_bytes(), b"patch\n")
            self.assertFalse(stale_patch.exists())

    def test_manifest_tracks_every_kernel_patch(self):
        manifest = json.loads(
            (BuildConstants.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(
            manifest["linux"]["patches"],
            [
                path.relative_to(BuildConstants.REPO_ROOT).as_posix()
                for path in build._kernel_patch_files()
            ],
        )
        self.assertNotIn("guest_agent", manifest)
        self.assertEqual(
            manifest["openvmm"],
            {
                "microvm_abi_version": OpenVMMBuildConstants.MICROVM_ABI_VERSION,
                "time_abi_version": 1,
                "restore_packet_version": 1,
                "control_session_protocol_version": 1,
                "control_contract_revision": "nvx-microvm-v2-control-v2",
            },
        )
        self.assertEqual(
            manifest["guest_rust"],
            {
                "toolchain": GuestRustBuildConstants.TOOLCHAIN,
                "target": GuestRustBuildConstants.TARGET,
            },
        )

    def test_kernel_patch_identity_follows_patch_paths_and_order(self):
        # A content-only hash ignores names, so a rename reused a kernel whose
        # provenance named the old patch. The identity must see paths and order.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "0001-a.patch").write_bytes(b"same bytes\n")
            (root / "0002-b.patch").write_bytes(b"other bytes\n")
            original = ci.kernel_patch_identity(root)
            self.assertEqual(original, ci.kernel_patch_identity(root))

            (root / "0001-a.patch").rename(root / "0001-renamed.patch")
            self.assertNotEqual(original, ci.kernel_patch_identity(root))

            renamed = ci.kernel_patch_identity(root)
            (root / "0001-renamed.patch").rename(root / "0003-renamed.patch")
            self.assertNotEqual(renamed, ci.kernel_patch_identity(root))

    def test_kernel_patch_identity_rejects_missing_or_empty_patch_directories(self):
        # An empty digest would stop following patch renames without an error.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with self.assertRaisesRegex(common.ScriptError, "no kernel patches"):
                ci.kernel_patch_identity(root / "missing")
            with self.assertRaisesRegex(common.ScriptError, "no kernel patches"):
                ci.kernel_patch_identity(root)

    def test_kernel_patch_identity_ignores_files_the_kernel_build_does_not_read(self):
        # Provenance hashes only top-level *.patch files, so the key must match.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "0001-a.patch").write_bytes(b"patch\n")
            original = ci.kernel_patch_identity(root)
            (root / "README.md").write_bytes(b"notes\n")
            (root / "nested").mkdir()
            (root / "nested" / "0002-b.patch").write_bytes(b"nested\n")
            self.assertEqual(original, ci.kernel_patch_identity(root))

    def test_kernel_patch_identity_changes_when_provenance_would_reject_the_kernel(
        self,
    ):
        # Renaming a patch changes the provenance source, so the identity must
        # change with it and the stale provenance must fail validation.
        with tempfile.TemporaryDirectory() as temporary:
            tree = Path(temporary)
            patches = tree / KernelBuildConstants.PATCH_DIRECTORY
            patches.mkdir(parents=True)
            (tree / KernelBuildConstants.INPUT_CONFIG).parent.mkdir(
                parents=True, exist_ok=True
            )
            (tree / KernelBuildConstants.INPUT_CONFIG).write_bytes(b"CONFIG_X=y\n")
            (patches / "0001-a.patch").write_bytes(b"same bytes\n")
            (patches / "0002-b.patch").write_bytes(b"other bytes\n")
            kernel = tree / "vmlinux"
            kernel.write_bytes(b"kernel\n")
            kernel_config = tree / "kernel.config"
            kernel_config.write_bytes(b"CONFIG_X=y\n")
            provenance_path = tree / "provenance.json"

            with (
                patch.object(BuildConstants, "REPO_ROOT", tree),
                patch.object(release, "assert_required_kernel_config"),
            ):
                stale_identity = ci.kernel_patch_identity(patches)
                provenance_path.write_text(
                    json.dumps(
                        {
                            "format": KernelBuildConstants.PROVENANCE_FORMAT,
                            **build.kernel_provenance_inputs(),
                            "kernel_sha256": release.sha256_file(kernel),
                            "config_sha256": release.sha256_file(kernel_config),
                        }
                    ),
                    encoding="utf-8",
                )
                release._validate_kernel_provenance(
                    kernel, kernel_config, provenance_path
                )

                (patches / "0001-a.patch").rename(patches / "0003-a.patch")

                self.assertNotEqual(stale_identity, ci.kernel_patch_identity(patches))
                with self.assertRaisesRegex(common.ScriptError, "does not match"):
                    release._validate_kernel_provenance(
                        kernel, kernel_config, provenance_path
                    )

    def test_kernel_input_config_uses_canonical_lf_line_endings(self):
        attributes = (BuildConstants.REPO_ROOT / ".gitattributes").read_text(
            encoding="utf-8"
        )
        for path in (
            KernelBuildConstants.INPUT_CONFIG,
            KernelBuildConstants.DEBUG_CONFIG_FRAGMENT,
        ):
            with self.subTest(path=path):
                self.assertIn(
                    f"{path.as_posix()} text eol=lf",
                    attributes.splitlines(),
                )

    def test_sandbox_kernel_config_requires_every_feature(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / ".config"
            config.write_text(
                "\n".join(KernelBuildConstants.REQUIRED_SANDBOX_CONFIG) + "\n",
                encoding="utf-8",
            )
            build._assert_sandbox_kernel_config(config)

            for missing in KernelBuildConstants.REQUIRED_SANDBOX_CONFIG:
                with self.subTest(missing=missing):
                    config.write_text(
                        "\n".join(
                            setting
                            for setting in KernelBuildConstants.REQUIRED_SANDBOX_CONFIG
                            if setting != missing
                        )
                        + "\n",
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(common.ScriptError, missing):
                        build._assert_sandbox_kernel_config(config)

    def test_network_kernel_config_requires_every_feature(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / ".config"
            config.write_text(
                "\n".join(KernelBuildConstants.REQUIRED_NETWORK_CONFIG) + "\n",
                encoding="utf-8",
            )
            build._assert_network_kernel_config(config)

            for missing in KernelBuildConstants.REQUIRED_NETWORK_CONFIG:
                with self.subTest(missing=missing):
                    config.write_text(
                        "\n".join(
                            setting
                            for setting in KernelBuildConstants.REQUIRED_NETWORK_CONFIG
                            if setting != missing
                        )
                        + "\n",
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(
                        common.ScriptError, "portable network profile"
                    ) as raised:
                        build._assert_network_kernel_config(config)
                    self.assertIn(missing, str(raised.exception))

            # An enabled SIT device leaves the "is not set" line out.
            config.write_text(
                "\n".join(
                    "CONFIG_IPV6_SIT=y"
                    if setting == "# CONFIG_IPV6_SIT is not set"
                    else setting
                    for setting in KernelBuildConstants.REQUIRED_NETWORK_CONFIG
                )
                + "\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(common.ScriptError, "CONFIG_IPV6_SIT"):
                build._assert_network_kernel_config(config)

    def test_checked_in_config_provides_the_portable_network_profile(self):
        config = BuildConstants.REPO_ROOT / KernelBuildConstants.INPUT_CONFIG
        build._assert_network_kernel_config(config)

    def test_checked_in_config_preserves_generic_sandbox_capabilities(self):
        config = BuildConstants.REPO_ROOT / "kernel" / "config-microvm"
        build._assert_sandbox_kernel_config(config)
        configured = config.read_text(encoding="utf-8").splitlines()
        for setting in (
            "CONFIG_SECCOMP_FILTER=y",
            "CONFIG_UNIX=y",
            "# CONFIG_OVERLAY_FS_REDIRECT_ALWAYS_FOLLOW is not set",
        ):
            self.assertIn(setting, configured)

    def test_shared_status_kernel_config_is_required(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / ".config"
            config.write_text(
                "\n".join(KernelBuildConstants.REQUIRED_SHARED_STATUS_CONFIG) + "\n",
                encoding="utf-8",
            )
            build._assert_shared_status_kernel_config(config)

            config.write_text("", encoding="utf-8")
            with self.assertRaisesRegex(
                common.ScriptError,
                KernelBuildConstants.REQUIRED_SHARED_STATUS_CONFIG[0],
            ):
                build._assert_shared_status_kernel_config(config)

    def test_checked_in_config_meets_time_abi_kernel_contract(self):
        config = BuildConstants.REPO_ROOT / KernelBuildConstants.INPUT_CONFIG
        build._assert_time_abi_kernel_config(config)
        build._assert_hardening_kernel_config(config)
        build._assert_watchdog_kernel_config(config, debug=False)
        configured = config.read_text(encoding="utf-8").splitlines()
        for removed in (
            "CONFIG_CPU_FREQ=y",
            "CONFIG_X86_INTEL_PSTATE=y",
            "CONFIG_SCHED_MC_PRIO=y",
            "CONFIG_HYPERV=y",
            "CONFIG_KVM_GUEST=y",
            "CONFIG_PARAVIRT_CLOCK=y",
            "CONFIG_HALTPOLL_CPUIDLE=y",
            "CONFIG_ARCH_CPUIDLE_HALTPOLL=y",
        ):
            with self.subTest(removed=removed):
                self.assertNotIn(removed, configured)
        self.assertFalse(
            any(line.startswith("CONFIG_CPU_FREQ_") for line in configured)
        )

    def test_time_abi_kernel_config_requires_every_setting(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / ".config"
            for missing in KernelBuildConstants.REQUIRED_TIME_ABI_CONFIG:
                with self.subTest(missing=missing):
                    config.write_text(
                        "\n".join(
                            setting
                            for setting in KernelBuildConstants.REQUIRED_TIME_ABI_CONFIG
                            if setting != missing
                        )
                        + "\n",
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(common.ScriptError, missing):
                        build._assert_time_abi_kernel_config(config)

    def test_hardening_kernel_config_requires_every_setting(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / ".config"
            for missing in KernelBuildConstants.REQUIRED_HARDENING_CONFIG:
                with self.subTest(missing=missing):
                    config.write_text(
                        "\n".join(
                            setting
                            for setting in KernelBuildConstants.REQUIRED_HARDENING_CONFIG
                            if setting != missing
                        )
                        + "\n",
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(common.ScriptError, re.escape(missing)):
                        build._assert_hardening_kernel_config(config)

    def test_hardening_kernel_config_rejects_loadable_modules(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / ".config"
            config.write_text(
                "\n".join(
                    (*KernelBuildConstants.REQUIRED_HARDENING_CONFIG, "CONFIG_FOO=m")
                )
                + "\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(common.ScriptError, "CONFIG_FOO=m"):
                build._assert_hardening_kernel_config(config)

    def test_watchdog_kernel_config_separates_production_and_debug(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / ".config"
            config.write_text(
                "\n".join(KernelBuildConstants.REQUIRED_DEBUG_CONFIG) + "\n",
                encoding="utf-8",
            )
            build._assert_watchdog_kernel_config(config, debug=True)
            with self.assertRaisesRegex(common.ScriptError, "debug-only watchdogs"):
                build._assert_watchdog_kernel_config(config, debug=False)

            config.write_text("CONFIG_DEBUG_KERNEL=y\n", encoding="utf-8")
            build._assert_watchdog_kernel_config(config, debug=False)
            with self.assertRaisesRegex(
                common.ScriptError, "CONFIG_SOFTLOCKUP_DETECTOR=y"
            ):
                build._assert_watchdog_kernel_config(config, debug=True)

    def test_kernel_config_fragment_replaces_base_assignments(self):
        base = "\n".join(
            (
                "#",
                "# Kernel hacking",
                "#",
                "# CONFIG_DEBUG_KERNEL is not set",
                "CONFIG_RCU_CPU_STALL_TIMEOUT=21",
                "CONFIG_UNRELATED=y",
            )
        )
        fragment = "\n".join(
            (
                "# Comment lines and blank lines are ignored.",
                "",
                "CONFIG_DEBUG_KERNEL=y",
                "# CONFIG_DEBUG_MISC is not set",
                "CONFIG_RCU_CPU_STALL_TIMEOUT=21",
            )
        )
        self.assertEqual(
            build.merge_kernel_config_fragment(base, fragment).splitlines(),
            [
                "#",
                "# Kernel hacking",
                "#",
                "CONFIG_UNRELATED=y",
                "CONFIG_DEBUG_KERNEL=y",
                "# CONFIG_DEBUG_MISC is not set",
                "CONFIG_RCU_CPU_STALL_TIMEOUT=21",
            ],
        )
        for invalid, message in (
            ("DEBUG_KERNEL=y\n", "line 1 is invalid"),
            ("# only a comment\n", "assigns no symbols"),
            (
                "CONFIG_DEBUG_KERNEL=y\n# CONFIG_DEBUG_KERNEL is not set\n",
                "more than once",
            ),
        ):
            with self.subTest(fragment=invalid):
                with self.assertRaisesRegex(common.ScriptError, message):
                    build.merge_kernel_config_fragment(base, invalid)

    def test_checked_in_debug_fragment_only_adds_watchdogs(self):
        base = (BuildConstants.REPO_ROOT / KernelBuildConstants.INPUT_CONFIG).read_text(
            encoding="utf-8"
        )
        fragment = (
            BuildConstants.REPO_ROOT / KernelBuildConstants.DEBUG_CONFIG_FRAGMENT
        ).read_text(encoding="utf-8")
        merged = build.merge_kernel_config_fragment(base, fragment).splitlines()
        for setting in KernelBuildConstants.REQUIRED_DEBUG_CONFIG:
            with self.subTest(setting=setting):
                self.assertIn(setting, merged)
        for setting in (
            "# CONFIG_DEBUG_MISC is not set",
            "# CONFIG_RCU_TRACE is not set",
            "# CONFIG_X86_DEBUG_FPU is not set",
            "CONFIG_RCU_CPU_STALL_TIMEOUT=21",
        ):
            with self.subTest(setting=setting):
                self.assertIn(setting, merged)
        self.assertNotIn("# CONFIG_DEBUG_KERNEL is not set", merged)
        for setting in KernelBuildConstants.REQUIRED_TIME_ABI_CONFIG:
            with self.subTest(time_abi=setting):
                self.assertIn(setting, merged)

    def test_debug_kernel_build_applies_fragment_and_records_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            input_config = root / KernelBuildConstants.INPUT_CONFIG
            input_config.parent.mkdir(parents=True)
            input_config.write_text(
                "\n".join(
                    (
                        *KernelBuildConstants.REQUIRED_DIRECT_BOOT_CONFIG,
                        *KernelBuildConstants.REQUIRED_VIRTIO_CONSOLE_CONFIG,
                        *KernelBuildConstants.REQUIRED_SHARED_STATUS_CONFIG,
                        *KernelBuildConstants.REQUIRED_SANDBOX_CONFIG,
                        *KernelBuildConstants.REQUIRED_NETWORK_CONFIG,
                        *KernelBuildConstants.REQUIRED_TIME_ABI_CONFIG,
                        *KernelBuildConstants.REQUIRED_HARDENING_CONFIG,
                        "# CONFIG_DEBUG_KERNEL is not set",
                    )
                )
                + "\n",
                encoding="utf-8",
            )
            fragment = root / KernelBuildConstants.DEBUG_CONFIG_FRAGMENT
            fragment.write_text(
                "# debug fixture\n"
                + "\n".join(KernelBuildConstants.REQUIRED_DEBUG_CONFIG)
                + "\n",
                encoding="utf-8",
            )
            patch_path = root / KernelBuildConstants.PATCH_DIRECTORY / "example.patch"
            patch_path.parent.mkdir()
            patch_path.write_text("patch", encoding="utf-8")
            source = root / "source"
            source.mkdir()
            output_directory = root / "output"
            output_directory.mkdir()
            production_provenance = (
                output_directory / KernelBuildConstants.PROVENANCE_NAME
            )
            production_provenance.write_text("production", encoding="utf-8")
            config = build_config.KernelBuildConfig(
                work=root / "work-debug",
                output=output_directory / KernelBuildConstants.DEBUG_BINARY_NAME,
                debug=True,
            )

            with patch.object(BuildConstants, "REPO_ROOT", root):
                source_fingerprint = build._kernel_source_fingerprint()

            def run_build(command: object, **_kwargs: object) -> None:
                if isinstance(command, list) and command[-1] == "vmlinux":
                    (config.work / "vmlinux").write_bytes(b"debug kernel")

            with (
                patch.object(BuildConstants, "REPO_ROOT", root),
                patch.object(build, "_require_linux"),
                patch.object(build, "require_tool", return_value="tool"),
                patch.object(
                    build,
                    "prepare_kernel_source",
                    return_value=(source, source_fingerprint),
                ),
                patch.object(build, "run_checked", side_effect=run_build),
            ):
                build.build_kernel(config)

            generated = (
                output_directory / KernelBuildConstants.DEBUG_CONFIG_NAME
            ).read_text(encoding="utf-8")
            self.assertIn("CONFIG_DEBUG_KERNEL=y", generated.splitlines())
            self.assertNotIn("# CONFIG_DEBUG_KERNEL is not set", generated)
            provenance = json.loads(
                (
                    output_directory / KernelBuildConstants.DEBUG_PROVENANCE_NAME
                ).read_text(encoding="utf-8")
            )
            self.assertEqual(
                provenance["debug_config_fragment"],
                {
                    "path": KernelBuildConstants.DEBUG_CONFIG_FRAGMENT.as_posix(),
                    "sha256": common.sha256_file(fragment),
                },
            )
            self.assertEqual(
                provenance["input_config"]["sha256"], common.sha256_file(input_config)
            )
            self.assertEqual(
                provenance["kernel_sha256"],
                hashlib.sha256(b"debug kernel").hexdigest(),
            )
            self.assertEqual(
                production_provenance.read_text(encoding="utf-8"), "production"
            )
            self.assertIn(
                "debug_config_fragment_sha256",
                (config.work / KernelBuildConstants.BUILD_STAMP_NAME).read_text(
                    encoding="utf-8"
                ),
            )
            self.assertEqual(
                config.provenance_name, KernelBuildConstants.DEBUG_PROVENANCE_NAME
            )
            self.assertEqual(
                build_config.KernelBuildConfig().provenance_name,
                KernelBuildConstants.PROVENANCE_NAME,
            )
            self.assertNotIn("debug_config_fragment", build.kernel_provenance_inputs())


if __name__ == "__main__":
    unittest.main()

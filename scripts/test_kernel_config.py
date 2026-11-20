#!/usr/bin/env python3
"""Tests for the kernel feature inventory and the checks of the configuration.

The kernel feature inventory, scripts/nvx_tools/kernel_features.py, names
every setting of kernel/config-microvm that NVX makes on purpose, and
kernel/config-microvm.derived records every other symbol that the
configuration sets with the Kconfig rule that sets it (#344). These tests
check the attribution of derived symbols, on test_kconfig.py's small tree,
the inventory itself, the source check that CI runs on every change, which
must refuse a newly enabled symbol that neither the inventory nor the record
accounts for, and the check that the kernel build runs against a fresh
generation.
"""

from __future__ import annotations

import argparse
import json
import shutil
import sys
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import build, build_config, common, kconfig  # noqa: E402
from nvx_tools import create_linux_source_archive as source_archive  # noqa: E402
from nvx_tools import kernel_config as kc  # noqa: E402
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
    KernelBuildConstants,
)
from nvx_tools.kernel_features import (  # noqa: E402
    KERNEL_CONFIG_CHECKS,
    KERNEL_FEATURES,
    KernelFeature,
    features_for_architecture,
)
from test_kconfig import read_tree, write_tree  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[1]
CONFIG = REPO_ROOT / KernelBuildConstants.INPUT_CONFIG
DERIVED = REPO_ROOT / KernelBuildConstants.DERIVED_CONFIG


def features(
    *settings: str, expects: tuple[str, ...] = ()
) -> tuple[KernelFeature, ...]:
    return (
        KernelFeature(
            name="test",
            summary="A test feature",
            rationale="Tests need it.",
            consumers=("tests",),
            validation=("this test",),
            settings=settings,
            expects=expects,
        ),
    )


class AttributionTests(unittest.TestCase):
    def test_attributes_each_symbol_to_the_rule_that_sets_it(self):
        tree = read_tree()
        values = {
            "X86": "y",
            "ARCH_THING": "y",
            "BASE": "y",
            "HELPER": "y",
            "EXPERT": "n",
            "HIDDEN": "y",
            "SOFT": "y",
            "PROBED": "y",
            "COUNT": "8",
            "MODE_FAST": "y",
        }
        settings = kc.inventory_settings(features("CONFIG_BASE=y"))
        entries = kc.derive(tree, values, settings)
        reasons = {symbol: reason for symbol, (_, reason) in entries.items()}
        self.assertEqual(
            reasons,
            {
                "X86": kc.NO_PROMPT,
                "ARCH_THING": kc.NO_PROMPT,
                "HELPER": kc.NO_PROMPT,
                "PROBED": kc.NO_PROMPT,
                "HIDDEN": "default, prompt hidden by EXPERT",
                "SOFT": "implied by BASE",
                "COUNT": "default",
                "MODE_FAST": "choice default",
            },
        )

    def test_names_every_selector_that_applies(self):
        tree = read_tree()
        values = {"EXPERT": "y", "DEBUG_KERNEL": "y"}
        entries = kc.derive(
            tree, values, kc.inventory_settings(features("CONFIG_EXPERT=y"))
        )
        self.assertEqual(entries["DEBUG_KERNEL"][1], "selected by EXPERT")

    def test_refuses_a_symbol_that_a_user_set_beyond_the_inventory(self):
        tree = read_tree()
        values = {"BASE": "y", "OPTION": "y"}
        with self.assertRaisesRegex(common.ScriptError, "CONFIG_OPTION=y"):
            kc.derive(tree, values, kc.inventory_settings(features("CONFIG_BASE=y")))


class InventoryTests(unittest.TestCase):
    def test_the_inventory_is_valid(self):
        self.assertEqual(kc.inventory_problems(), [])

    def test_arm_binding_keeps_shared_owners_without_x86_ports_or_growth(self):
        self.assertIs(features_for_architecture("x86_64"), KERNEL_FEATURES)
        arm = features_for_architecture("aarch64")
        self.assertEqual(kc.inventory_problems(arm), [])
        self.assertEqual(
            [feature.name for feature in arm],
            [feature.name for feature in KERNEL_FEATURES],
        )
        shared = {"sandbox-filesystems", "sandbox-isolation", "network", "guest-init"}
        originals = {feature.name: feature for feature in KERNEL_FEATURES}
        for feature in arm:
            if feature.name in shared:
                self.assertIs(feature, originals[feature.name])
        settings = kc.inventory_settings(arm)
        for symbol in ("DEVMEM", "DEVPORT", "HOTPLUG_CPU", "MEMORY_HOTPLUG", "COMPAT"):
            self.assertEqual(settings[symbol][1].value, "n", symbol)
        self.assertEqual(settings["HVC_NVX_MMIO"][1].value, "y")
        self.assertNotIn("X86_X2APIC", settings)
        with self.assertRaisesRegex(ValueError, "unsupported kernel architecture"):
            features_for_architecture("arm64")

    def test_every_feature_records_why_and_for_whom(self):
        for feature in KERNEL_FEATURES:
            with self.subTest(feature=feature.name):
                self.assertTrue(feature.rationale.endswith("."), feature.name)
                self.assertTrue(feature.consumers)
                self.assertTrue(feature.validation)
                self.assertTrue(feature.settings)

    def test_the_networking_of_309_is_required(self):
        sandbox = KernelBuildConstants.REQUIRED_SANDBOX_CONFIG
        self.assertIn("CONFIG_NET_NS=y", sandbox)
        self.assertIn("CONFIG_VETH=y", sandbox)
        owner = kc.inventory_settings()["NET_NS"][0]
        self.assertEqual(owner.name, "guest-internal-networking")
        self.assertIn("#309", owner.summary)

    def test_build_checks_are_their_features_settings_and_expectations(self):
        groups = {
            "direct-boot": KernelBuildConstants.REQUIRED_DIRECT_BOOT_CONFIG,
            "virtio-console": KernelBuildConstants.REQUIRED_VIRTIO_CONSOLE_CONFIG,
            "shared-status": KernelBuildConstants.REQUIRED_SHARED_STATUS_CONFIG,
            "sandbox": KernelBuildConstants.REQUIRED_SANDBOX_CONFIG,
            "network": KernelBuildConstants.REQUIRED_NETWORK_CONFIG,
            "time-abi": KernelBuildConstants.REQUIRED_TIME_ABI_CONFIG,
            "hardening": KernelBuildConstants.REQUIRED_HARDENING_CONFIG,
        }
        self.assertEqual(set(groups), set(KERNEL_CONFIG_CHECKS))
        by_name = {feature.name: feature for feature in KERNEL_FEATURES}
        for check, group in groups.items():
            with self.subTest(check=check):
                expected = {
                    assignment
                    for name in KERNEL_CONFIG_CHECKS[check]
                    for assignment in (*by_name[name].settings, *by_name[name].expects)
                }
                self.assertEqual(set(group), expected)
                self.assertEqual(len(group), len(set(group)))

    def test_refuses_an_invalid_inventory(self):
        valid = features("CONFIG_A=y")[0]
        cases = {
            "not kebab-case": (replace(valid, name="Not_Kebab"),),
            "has no rationale": (replace(valid, rationale=" "),),
            "names no consumer": (replace(valid, consumers=()),),
            "names no validation": (replace(valid, validation=("",)),),
            "makes no setting": (replace(valid, settings=()),),
            "sets 'CONFIG_A'": (replace(valid, settings=("CONFIG_A",)),),
            "is set by both": (valid, replace(valid, name="other")),
            "defined twice": (valid, replace(valid, settings=("CONFIG_B=y",))),
            "but test sets": (replace(valid, expects=("# CONFIG_A is not set",)),),
        }
        for message, inventory in cases.items():
            with self.subTest(message=message):
                problems = kc.inventory_problems(inventory, {})
                self.assertTrue(
                    any(message in problem for problem in problems), problems
                )
        self.assertIn(
            "check missing names unknown feature nope",
            kc.inventory_problems((valid,), {"missing": ("nope",)}),
        )

    def test_the_fragment_holds_every_setting(self):
        fragment = kc.render_fragment().splitlines()
        for feature in KERNEL_FEATURES:
            for setting in feature.settings:
                with self.subTest(setting=setting):
                    self.assertIn(setting, fragment)


class DerivedRecordTests(unittest.TestCase):
    def test_arm_record_names_its_own_generator_and_input_without_changing_x86(self):
        record = kc.DerivedRecord(
            "gcc 1.0", {"A": (kc.Assignment("A", "y"), "no prompt")}
        )
        original = record.render()
        self.assertEqual(original, record.render(architecture="x86_64"))
        arm = record.render(architecture="aarch64")
        self.assertIn("--architecture aarch64 --native", arm)
        self.assertIn("kernel/config-microvm-arm64 sets", arm)
        self.assertEqual(kc.DerivedRecord.parse(arm), record)
        self.assertEqual(kc.DerivedRecord.parse(original), record)
        with self.assertRaisesRegex(
            common.ScriptError, "unsupported kernel architecture"
        ):
            record.render(architecture="arm64")

    def test_round_trips(self):
        record = kc.DerivedRecord(
            "gcc 1.0",
            {
                "B": (kc.Assignment("B", '"two  # words"'), "default"),
                "A": (kc.Assignment("A", "y"), "selected by C, D"),
            },
        )
        text = record.render()
        self.assertIn("# Toolchain: gcc 1.0\n", text)
        self.assertLess(text.index("CONFIG_A=y"), text.index("CONFIG_B="))
        self.assertEqual(kc.DerivedRecord.parse(text), record)

    def test_refuses_an_invalid_line(self):
        for line in ("CONFIG_A=y", "CONFIG_A=y  # ", "A=y  # default"):
            with self.subTest(line=line), self.assertRaises(common.ScriptError):
                kc.DerivedRecord.parse(line + "\n")
        with self.assertRaisesRegex(common.ScriptError, "twice"):
            kc.DerivedRecord.parse("CONFIG_A=y  # no prompt\n" * 2)


class CheckedInConfigurationTests(unittest.TestCase):
    """The source check that CI runs on every change (doc/build.md)."""

    def setUp(self):
        self.config = CONFIG.read_text(encoding="utf-8")
        self.derived = DERIVED.read_text(encoding="utf-8")

    def problems(
        self, config: str | None = None, derived: str | None = None
    ) -> list[str]:
        return kc.configuration_problems(
            self.config if config is None else config,
            self.derived if derived is None else derived,
        )

    def test_the_checked_in_configuration_matches_its_inventory(self):
        self.assertEqual(self.problems(), [])

    def test_refuses_a_newly_enabled_untracked_symbol(self):
        for line in (
            "CONFIG_INPUT_UINPUT=y",
            "CONFIG_NVX_UNTRACKED_FEATURE=y",
            'CONFIG_NVX_UNTRACKED_VALUE="x"',
        ):
            with self.subTest(line=line):
                self.assertNotIn(line.split("=")[0] + "=", self.config)
                problems = self.problems(config=self.config + line + "\n")
                self.assertEqual(len(problems), 1, problems)
                self.assertIn(line, problems[0])
                self.assertIn("neither the kernel feature inventory", problems[0])

    def test_refuses_an_enabled_symbol_that_replaces_its_not_set_line(self):
        for symbol in ("INPUT", "VT", "HUGETLBFS", "CRYPTO"):
            with self.subTest(symbol=symbol):
                not_set = f"# CONFIG_{symbol} is not set\n"
                self.assertIn(not_set, self.config)
                problems = self.problems(
                    config=self.config.replace(not_set, f"CONFIG_{symbol}=y\n")
                )
                self.assertEqual(len(problems), 1, problems)
                self.assertIn(f"CONFIG_{symbol}=y", problems[0])

    def test_refuses_a_dropped_inventory_setting(self):
        config = self.config.replace("CONFIG_VETH=y\n", "# CONFIG_VETH is not set\n")
        self.assertEqual(
            self.problems(config=config),
            [
                "kernel/config-microvm does not hold CONFIG_VETH=y, which the "
                "inventory's guest-internal-networking feature sets"
            ],
        )

    def test_refuses_a_changed_derived_value(self):
        config = self.config.replace("CONFIG_NR_CPUS=64\n", "CONFIG_NR_CPUS=8192\n")
        problems = self.problems(config=config)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("CONFIG_NR_CPUS=64", problems[0])

    def test_refuses_a_stale_or_misplaced_record_entry(self):
        stale = self.problems(derived=self.derived + "CONFIG_GONE=y  # no prompt\n")
        self.assertEqual(len(stale), 1, stale)
        self.assertIn("does not set", stale[0])
        misplaced = self.problems(
            derived=self.derived + "CONFIG_VETH=y  # selected by NOTHING\n"
        )
        self.assertTrue(any("which the inventory sets" in p for p in misplaced))

    def test_refuses_a_record_of_another_toolchain(self):
        derived = "\n".join(
            "# Toolchain: another" if line.startswith("# Toolchain: ") else line
            for line in self.derived.splitlines()
        )
        problems = self.problems(derived=derived + "\n")
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("names the toolchain 'another'", problems[0])

    def test_the_record_names_the_compiler_and_its_tools(self):
        values = kc.parse_config(self.config)
        toolchain = kc.toolchain_identity(values)
        assert toolchain is not None
        self.assertEqual(kc.DerivedRecord.parse(self.derived).toolchain, toolchain)
        self.assertTrue(toolchain.startswith(values["CC_VERSION_TEXT"].strip('"')))
        self.assertIn(f"; LD_VERSION={values['LD_VERSION']}", toolchain)

    def test_a_tool_s_version_is_part_of_the_toolchain(self):
        values = {
            "CC_VERSION_TEXT": '"gcc 1.0"',
            "AS_VERSION": "24000",
            "LD_VERSION": "24000",
            "LLD_VERSION": "0",
            "PAHOLE_VERSION": "0",
        }
        self.assertEqual(
            kc.toolchain_identity(values),
            "gcc 1.0; AS_VERSION=24000; LD_VERSION=24000",
        )
        # The same compiler with another linker or with pahole is another
        # toolchain, whose probes may decide other symbols.
        for symbol, value in (("LD_VERSION", "24200"), ("PAHOLE_VERSION", "127")):
            with self.subTest(symbol=symbol):
                self.assertNotEqual(
                    kc.toolchain_identity({**values, symbol: value}),
                    kc.toolchain_identity(values),
                )
        self.assertIsNone(kc.toolchain_identity({"AS_VERSION": "24000"}))

    def test_the_cli_check_reports_the_problems(self):
        import nvx  # noqa: PLC0415

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "kernel").mkdir()
            (root / KernelBuildConstants.INPUT_CONFIG).write_text(
                self.config + "CONFIG_INPUT_UINPUT=y\n", encoding="utf-8"
            )
            (root / KernelBuildConstants.DERIVED_CONFIG).write_text(
                self.derived, encoding="utf-8"
            )
            with (
                patch.object(BuildConstants, "REPO_ROOT", root),
                self.assertRaisesRegex(common.ScriptError, "CONFIG_INPUT_UINPUT=y"),
            ):
                nvx.command_check_kernel_config(argparse.Namespace())


class BuildCheckTests(unittest.TestCase):
    """The check that the kernel build runs against a fresh generation."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.source = self.root / "source"
        write_tree(self.source)
        self.values = {
            "X86": "y",
            "ARCH_THING": "y",
            "BASE": "y",
            "HELPER": "y",
            "HIDDEN": "y",
            "SOFT": "y",
            "COUNT": "8",
            "MODE_FAST": "y",
            "EXPERT": "n",
            "CC_VERSION_TEXT": '"gcc 1.0"',
        }
        self.inventory = features("CONFIG_BASE=y", "# CONFIG_EXPERT is not set")
        tree = kconfig.read_kconfig(self.source)
        self.record = kc.DerivedRecord(
            "gcc 1.0",
            kc.derive(tree, self.values, kc.inventory_settings(self.inventory)),
        )

    def check(self, effective: dict[str, str], record: kc.DerivedRecord) -> None:
        text = "\n".join(kc.Assignment(s, v).line for s, v in effective.items())
        (self.root / "effective").write_text(text + "\n", encoding="utf-8")
        (self.root / "derived").write_text(record.render(), encoding="utf-8")
        generated = kc.GeneratedConfig(text, dict(self.values), self.record)
        with patch.object(kc, "generate", return_value=generated):
            kc.check_build_configuration(
                self.source,
                self.root / "work",
                self.root / "effective",
                self.root / "derived",
                self.inventory,
            )

    def test_accepts_the_generated_configuration(self):
        self.check(self.values, self.record)

    def test_arm_generation_selects_arm_make_and_the_arm_kconfig_tree(self):
        work = self.root / "arm-work"
        selected_tree = read_tree()

        def allnoconfig(command: list[object]) -> None:
            self.assertIn("ARCH=arm64", command)
            self.assertEqual(command[-1], "allnoconfig")
            text = "\n".join(kc.Assignment(s, v).line for s, v in self.values.items())
            (work / ".config").write_text(text + "\n", encoding="utf-8")

        with (
            patch.object(kc, "inventory_problems", return_value=[]),
            patch.object(kc, "run_checked", side_effect=allnoconfig),
            patch.object(
                kc.kconfig, "read_kconfig", return_value=selected_tree
            ) as read,
        ):
            kc.generate(self.source, work, self.inventory, architecture="aarch64")
        read.assert_called_once_with(self.source, srcarch="arm64", arch="arm64")

    def test_refuses_a_symbol_that_the_inventory_does_not_generate(self):
        with self.assertRaisesRegex(common.ScriptError, "CONFIG_OPTION=y"):
            self.check({**self.values, "OPTION": "y"}, self.record)

    def test_refuses_a_record_that_misattributes_a_symbol(self):
        entries = dict(self.record.entries)
        entries["HIDDEN"] = (kc.Assignment("HIDDEN", "y"), "selected by BASE")
        with self.assertRaisesRegex(common.ScriptError, "CONFIG_HIDDEN"):
            self.check(self.values, kc.DerivedRecord("gcc 1.0", entries))

    def test_another_toolchain_compares_only_prompted_entries(self):
        # Another compiler, or the same one with another linker.
        for toolchain in ("gcc 2.0", "gcc 1.0; LD_VERSION=24200"):
            with self.subTest(toolchain=toolchain):
                entries = dict(self.record.entries)
                del entries["ARCH_THING"]
                entries["PROBED"] = (kc.Assignment("PROBED", "y"), kc.NO_PROMPT)
                self.check(self.values, kc.DerivedRecord(toolchain, entries))
                entries["SOFT"] = (kc.Assignment("SOFT", "y"), "default")
                with self.assertRaisesRegex(common.ScriptError, "CONFIG_SOFT"):
                    self.check(self.values, kc.DerivedRecord(toolchain, entries))
                del entries["SOFT"]
                entries["ARCH_THING"] = (
                    kc.Assignment("ARCH_THING", "y"),
                    "selected by X86",
                )
                with self.assertRaisesRegex(common.ScriptError, "without a prompt"):
                    self.check(self.values, kc.DerivedRecord(toolchain, entries))

    def test_the_production_build_runs_the_check_and_the_debug_build_does_not(self):
        class Stop(Exception):
            pass

        def stop_after_olddefconfig(command: list[object], **_: object) -> None:
            if command[-1] != "olddefconfig":
                raise Stop

        for debug in (False, True):
            with self.subTest(debug=debug):
                config = build_config.KernelBuildConfig(
                    work=self.root / "work", output=self.root / "vmlinux", debug=debug
                )
                with (
                    patch.object(build, "_require_linux"),
                    patch.object(build, "require_tool"),
                    patch.object(
                        build, "prepare_kernel_source", return_value=(self.source, "{}")
                    ),
                    patch.object(
                        build, "run_checked", side_effect=stop_after_olddefconfig
                    ),
                    patch.object(build, "assert_required_kernel_config"),
                    patch.object(build, "check_build_configuration") as check,
                    self.assertRaises(Stop),
                ):
                    build.build_kernel(config)
                self.assertEqual(check.called, not debug)
                if not debug:
                    self.assertEqual(
                        check.call_args.args[1:],
                        (
                            self.root
                            / KernelBuildConstants.INVENTORY_WORK_DIRECTORY_NAME,
                            self.root / "work" / KernelBuildConstants.BUILD_CONFIG_NAME,
                            REPO_ROOT / KernelBuildConstants.DERIVED_CONFIG,
                        ),
                    )


class ArmKernelProducerTests(unittest.TestCase):
    def test_selected_patch_order_and_config_do_not_replace_x86_inputs(self):
        manifest = json.loads((REPO_ROOT / "SOURCE-MANIFEST.json").read_text())
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in (
                *manifest["linux"]["patches"],
                *KernelBuildConstants.ARM_PATCHES,
                KernelBuildConstants.INPUT_CONFIG,
            ):
                path = Path(name)
                target = root / path
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(REPO_ROOT / path, target)
            (root / KernelBuildConstants.ARM_INPUT_CONFIG).write_text(
                "CONFIG_ARM64=y\n"
            )
            with patch.object(BuildConstants, "REPO_ROOT", root):
                wire = json.loads(
                    json.dumps(
                        {
                            "x86": build.kernel_provenance_inputs(),
                            "arm": build.kernel_provenance_inputs("aarch64"),
                        }
                    )
                )
        original = wire["x86"]["source"]
        selected = wire["arm"]["source"]
        self.assertNotIn("architecture", original)
        self.assertEqual(
            [row["path"] for row in original["patches"]],
            manifest["linux"]["patches"],
        )
        self.assertEqual(selected["architecture"], "aarch64")
        self.assertEqual(
            [row["path"] for row in selected["patches"]],
            [
                *manifest["linux"]["patches"],
                *(path.as_posix() for path in KernelBuildConstants.ARM_PATCHES),
            ],
        )
        self.assertEqual(
            manifest["linux"]["architectures"]["aarch64"]["patches"],
            [row["path"] for row in selected["patches"]],
        )
        with self.assertRaisesRegex(common.ScriptError, "must not overwrite x86"):
            build_config.KernelBuildConfig(architecture="aarch64")

    def test_arm_make_and_provenance_use_image_arm_inputs_and_the_real_jobs_limit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            config = replace(
                build_config.KernelBuildConfig.for_architecture("aarch64", jobs=1),
                work=root / "work",
                output=root / "products" / "Image",
            )
            input_path = root / config.input_config
            input_path.parent.mkdir()
            values: dict[str, str] = {}
            for feature in features_for_architecture("aarch64"):
                for setting in (*feature.settings, *feature.expects):
                    assignment = kc.Assignment.parse(setting)
                    assert assignment is not None
                    values[assignment.symbol] = assignment.value
            text = "\n".join(kc.Assignment(s, v).line for s, v in values.items()) + "\n"
            input_path.write_text(text, encoding="utf-8")
            commands: list[list[object]] = []

            def make(command: list[object]) -> None:
                commands.append(command)
                self.assertIn("ARCH=arm64", command)
                if command[-1] == "Image":
                    self.assertIn("-j1", command)
                    built = config.work / "arch" / "arm64" / "boot" / "Image"
                    built.parent.mkdir(parents=True)
                    built.write_bytes(b"mock producer output")

            fingerprint = json.dumps({"architecture": "aarch64", "patches": []})
            with (
                patch.object(BuildConstants, "REPO_ROOT", root),
                patch.object(build, "_require_linux"),
                patch.object(build, "require_tool"),
                patch.object(
                    build, "prepare_kernel_source", return_value=(source, fingerprint)
                ),
                patch.object(
                    build, "_kernel_source_fingerprint", return_value=fingerprint
                ),
                patch.object(build, "run_checked", side_effect=make),
                patch.object(build, "check_build_configuration") as check,
            ):
                build.build_kernel(config)
            self.assertEqual(
                [command[-1] for command in commands], ["olddefconfig", "Image"]
            )
            self.assertEqual(check.call_args.kwargs["architecture"], "aarch64")
            self.assertEqual(check.call_args.args[3], root / config.derived_config)
            self.assertEqual(config.output.read_bytes(), b"mock producer output")
            receipt = json.loads(
                config.output.with_name(config.provenance_name).read_text()
            )
            self.assertEqual(
                receipt["input_config"]["path"], config.input_config.as_posix()
            )
            self.assertEqual(receipt["source"]["architecture"], "aarch64")
            self.assertFalse((root / KernelBuildConstants.INPUT_CONFIG).exists())

    def test_current_cli_selects_architecture_and_refuses_unwired_docker_generation(
        self,
    ):
        import nvx  # noqa: PLC0415

        args = nvx.parse_args(["build-kernel", "--architecture=aarch64", "--jobs", "1"])
        with patch.object(nvx, "build_kernel") as build_selected:
            nvx.command_build_kernel(args)
        selected = build_selected.call_args.args[0]
        self.assertEqual(selected.architecture, "aarch64")
        self.assertEqual(selected.jobs, 1)
        self.assertEqual(selected.output.name, "Image")
        args = nvx.parse_args(["kernel-config", "--architecture", "aarch64"])
        with (
            patch.object(nvx, "generate_docker_kernel_config") as docker,
            self.assertRaisesRegex(common.ScriptError, "requires --native"),
        ):
            nvx.command_kernel_config(args)
        docker.assert_not_called()

    def test_arm_corresponding_source_uses_arm_patch_context_and_final_config_member(
        self,
    ):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            generated = root / "Image.config"
            generated.write_text("CONFIG_ARM64=y\n# CONFIG_X86_64 is not set\n")
            args = argparse.Namespace(
                architecture="aarch64", config=generated, output=root / "source.tar.gz"
            )
            with (
                patch.object(
                    source_archive,
                    "prepare_kernel_source",
                    return_value=(root / "patched-linux", "{}"),
                ) as prepare,
                patch.object(source_archive, "create_reproducible_tar_gz") as archive,
            ):
                source_archive.command_create_linux_source_archive(args)
            prepare.assert_called_once_with(
                build_config.KernelBuildConfig.for_architecture("aarch64")
            )
            members = archive.call_args.args[1]
            config_member = next(
                member for member in members if member[0] == generated.resolve()
            )
            self.assertEqual(
                config_member[1],
                f"{KernelBuildConstants.SOURCE_ARCHIVE_ROOT}-aarch64/Image.config",
            )

    def test_arm_source_archive_refuses_x86_config_before_any_source_preparation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            generated = root / "Image.config"
            generated.write_text("CONFIG_X86_64=y\n")
            args = argparse.Namespace(
                architecture="aarch64", config=generated, output=root / "source.tar.gz"
            )
            with (
                patch.object(source_archive, "prepare_kernel_source") as prepare,
                self.assertRaisesRegex(common.ScriptError, "generated ARM64"),
            ):
                source_archive.command_create_linux_source_archive(args)
            prepare.assert_not_called()

    def test_legacy_source_archive_defaults_keep_x86_context_and_vmlinux_member(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            generated = root / "vmlinux.config"
            generated.write_text("CONFIG_X86_64=y\n")
            args = argparse.Namespace(config=generated, output=root / "source.tar.gz")
            with (
                patch.object(
                    source_archive,
                    "prepare_kernel_source",
                    return_value=(root / "patched-linux", "{}"),
                ) as prepare,
                patch.object(source_archive, "create_reproducible_tar_gz") as archive,
            ):
                source_archive.command_create_linux_source_archive(args)
            prepare.assert_called_once_with(build_config.KernelBuildConfig())
            members = archive.call_args.args[1]
            self.assertIn(
                (
                    generated.resolve(),
                    f"{KernelBuildConstants.SOURCE_ARCHIVE_ROOT}/vmlinux.config",
                ),
                members,
            )


if __name__ == "__main__":
    unittest.main()

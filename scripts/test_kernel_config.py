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
import struct
import sys
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from typing import cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import build, build_config, common, elf, kconfig  # noqa: E402
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

    def test_strict_attribution_cannot_treat_an_unknown_symbol_as_hidden(self):
        tree = read_tree()
        settings = kc.inventory_settings(features("CONFIG_BASE=y"))
        with self.assertRaisesRegex(
            common.ScriptError, "unknown Kconfig symbol CONFIG_UNKNOWN=y"
        ):
            kc.derive(
                tree,
                {"BASE": "y", "UNKNOWN": "y"},
                settings,
                require_known_symbols=True,
            )
        self.assertEqual(
            kc.derive(
                tree, {"BASE": "y", "HELPER": "y"}, settings, require_known_symbols=True
            )["HELPER"][1],
            kc.NO_PROMPT,
        )


class InventoryTests(unittest.TestCase):
    def test_the_inventory_is_valid(self):
        self.assertEqual(kc.inventory_problems(), [])

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


class ArmKernelBindingTests(unittest.TestCase):
    def test_actual_arm_config_rejects_x86_leakage_even_with_forged_attribution(self):
        config = (REPO_ROOT / KernelBuildConstants.ARM_INPUT_CONFIG).read_text(
            encoding="utf-8"
        )
        record = kc.DerivedRecord.parse(
            (REPO_ROOT / KernelBuildConstants.ARM_DERIVED_CONFIG).read_text(
                encoding="utf-8"
            )
        )
        arm = features_for_architecture("aarch64")
        for symbol in (
            "X86_64",
            "HVC_XE9",
            "SERIAL_8250",
            "ACPI",
            "COMPAT",
            "HOTPLUG_CPU",
            "HAS_IOPORT_MAP",
        ):
            lines = [
                line
                for line in config.splitlines()
                if (assignment := kc.Assignment.parse(line)) is None
                or assignment.symbol != symbol
            ]
            lines.append(f"CONFIG_{symbol}=y")
            entries = dict(record.entries)
            entries[symbol] = (kc.Assignment(symbol, "y"), kc.NO_PROMPT)
            forged = replace(record, entries=entries).render()
            problems = kc.arm_configuration_problems(
                "\n".join(lines) + "\n", forged, arm
            )
            self.assertTrue(
                any(
                    f"excluded option CONFIG_{symbol}=y" in problem
                    for problem in problems
                ),
                problems,
            )

    def test_actual_arm_config_rejects_unnecessary_explicit_extension(self):
        config = (REPO_ROOT / KernelBuildConstants.ARM_INPUT_CONFIG).read_text(
            encoding="utf-8"
        )
        record = (REPO_ROOT / KernelBuildConstants.ARM_DERIVED_CONFIG).read_text(
            encoding="utf-8"
        )
        arm = features_for_architecture("aarch64")
        extension = KernelFeature(
            name="unnecessary-arm-extension",
            summary="Test extension",
            rationale="Negative fixture",
            consumers=("test",),
            validation=("test",),
            settings=("CONFIG_ARM64_SVE=y",),
        )
        lines = [line for line in config.splitlines() if "CONFIG_ARM64_SVE" not in line]
        problems = kc.arm_configuration_problems(
            "\n".join(lines) + "\nCONFIG_ARM64_SVE=y\n",
            record,
            (*arm, extension),
        )
        self.assertTrue(
            any(
                "unnecessary or unreviewed CONFIG_ARM64_SVE=y" in problem
                for problem in problems
            ),
            problems,
        )

    def test_arm_audit_still_requires_common_product_capabilities(self):
        config = (REPO_ROOT / KernelBuildConstants.ARM_INPUT_CONFIG).read_text(
            encoding="utf-8"
        )
        record = (REPO_ROOT / KernelBuildConstants.ARM_DERIVED_CONFIG).read_text(
            encoding="utf-8"
        )
        self.assertIn("CONFIG_CGROUPS=y", config)
        problems = kc.arm_configuration_problems(
            config.replace("CONFIG_CGROUPS=y", "# CONFIG_CGROUPS is not set"),
            record,
            features_for_architecture("aarch64"),
        )
        self.assertTrue(
            any("CONFIG_CGROUPS=y" in problem for problem in problems), problems
        )

    def test_arm_audit_keeps_gpt_and_unavoidable_arm_capabilities(self):
        config = (REPO_ROOT / KernelBuildConstants.ARM_INPUT_CONFIG).read_text(
            encoding="utf-8"
        )
        record = (REPO_ROOT / KernelBuildConstants.ARM_DERIVED_CONFIG).read_text(
            encoding="utf-8"
        )
        values = kc.parse_config(config)
        self.assertEqual(values["EFI_PARTITION"], "y")
        self.assertEqual(values["HAS_IOPORT"], "y")
        self.assertEqual(values["NO_IOPORT_MAP"], "y")
        self.assertEqual(values.get("HAS_IOPORT_MAP", "n"), "n")
        self.assertEqual(
            kc.arm_configuration_problems(
                config, record, features_for_architecture("aarch64")
            ),
            [],
        )

    def test_arm_inventory_is_coherent_and_keeps_x86_inventory_identity(self):
        self.assertIs(features_for_architecture("x86_64"), KERNEL_FEATURES)
        arm = features_for_architecture("aarch64")
        self.assertEqual(kc.inventory_problems(arm), [])
        settings = kc.inventory_settings(arm)
        for symbol, value in {
            "ARM64_4K_PAGES": "y",
            "ARM_GIC_V3": "y",
            "OF_RESERVED_MEM": "y",
            "HVC_NVX_MMIO": "y",
            "VIRTIO_MMIO_CMDLINE_DEVICES": "n",
            "DEVMEM": "n",
            "DEVPORT": "n",
            "COMPAT": "n",
            "MEMORY_HOTPLUG": "n",
            "PT_RECLAIM": "n",
            "ARM64_PTR_AUTH": "y",
            "ARM64_PTR_AUTH_KERNEL": "n",
        }.items():
            self.assertEqual(settings[symbol][1].value, value)
        with self.assertRaises(ValueError):
            features_for_architecture("arm64")

    def test_arm_pac_uses_existing_user_keys_without_enabling_kernel_signing(self):
        config = (REPO_ROOT / KernelBuildConstants.ARM_INPUT_CONFIG).read_text(
            encoding="utf-8"
        )
        values = kc.parse_config(config)
        for symbol, value in {
            "ARM64_PTR_AUTH": "y",
            "ARM64_PTR_AUTH_KERNEL": "n",
            "STACKPROTECTOR": "y",
            "STACKPROTECTOR_STRONG": "y",
        }.items():
            self.assertEqual(values.get(symbol, "n"), value)
        x86 = kc.inventory_settings(features_for_architecture("x86_64"))
        self.assertNotIn("ARM64_PTR_AUTH", x86)
        self.assertNotIn("ARM64_PTR_AUTH_KERNEL", x86)

    def test_kernel_paths_and_patch_sets_keep_architectures_separate(self):
        x86 = build_config.KernelBuildConfig.for_architecture("x86_64")
        self.assertEqual(x86, build_config.KernelBuildConfig())
        arm = build_config.KernelBuildConfig.for_architecture("aarch64")
        self.assertEqual(arm.output.name, "Image")
        self.assertEqual(arm.input_config, KernelBuildConstants.ARM_INPUT_CONFIG)
        self.assertEqual(arm.make_architecture, ("ARCH=arm64",))
        self.assertNotEqual(arm.work.parent, x86.work.parent)

        def patches(architecture: str) -> list[str]:
            source = cast(
                dict[str, object],
                build.kernel_provenance_inputs(architecture)["source"],
            )
            records = cast(list[dict[str, str]], source["patches"])
            return [record["path"] for record in records]

        x86_patches = patches("x86_64")
        arm_patches = patches("aarch64")
        self.assertEqual(arm_patches[: len(x86_patches)], x86_patches)
        self.assertTrue(
            all(
                path.startswith("kernel/patches/arm64/")
                for path in arm_patches[len(x86_patches) :]
            )
        )
        with self.assertRaises(common.ScriptError):
            build_config.KernelBuildConfig(architecture="arm64")

    def test_image_header_checks_exact_flags_bounds_and_overflow(self):
        def header(
            offset: int = 0, size: int = 64, flags: int = 10, magic: int = 0x644D5241
        ) -> bytes:
            return struct.pack(
                "<IIQQQQQQII", 0, 0, offset, size, flags, 0, 0, 0, magic, 0
            )

        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "Image"
            path.write_bytes(header())
            self.assertEqual(elf.read_arm64_image(path)["image_size"], 64)
            for data in [
                b"ELF",
                header(magic=0),
                header(flags=11),
                header(flags=8),
                header(flags=12),
                header(flags=14),
                header(flags=2),
                header(flags=26),
                header(size=63),
                header(offset=1),
                header(offset=0xFFFFFFFFFFFFF000, size=4096),
            ]:
                path.write_bytes(data)
                with self.assertRaises(common.ScriptError):
                    elf.read_arm64_image(path)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Tests for the Kconfig reader of the kernel configuration checks.

scripts/nvx_tools/kconfig.py reads the Kconfig tree of the pinned kernel, so
that the checks of kernel/config-microvm can name the Kconfig rule that sets
each symbol (#344). These tests read a small tree that uses the constructs of
the pinned kernel's tree, and the pinned tree itself where its source is
prepared.
"""

from __future__ import annotations

import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import common, kconfig  # noqa: E402
from nvx_tools.build_constants import KernelBuildConstants  # noqa: E402

TREE = {
    "Kconfig": """\
        mainmenu "Test/$(ARCH) Kernel Configuration"
        cc-option = $(success,true)
        $(error-if,$(failure,true),never)

        source "arch/$(SRCARCH)/Kconfig"

        menuconfig EXPERT
        \tbool "Expert"
        \tselect DEBUG_KERNEL

        config DEBUG_KERNEL
        \tbool "Debug"
        \thelp
        \t  A help text that mentions
        \t  config NOT_A_SYMBOL
        \t  and keeps going.

        \t  After a blank line too.

        config BASE
        \tbool "Base feature"
        \tselect HELPER if !EXPERT
        \timply SOFT

        config HELPER
        \tbool

        config TWICE
        \tbool "Twice"
        \tselect ARCH_THING

        if EXPERT
        config TWICE
        \tbool
        \tselect HELPER
        \timply SOFT
        endif

        config SOFT
        \tbool "Soft" if EXPERT
        \tdefault n

        config HIDDEN
        \tbool "Hidden unless expert" if EXPERT
        \tdefault y

        config PROBED
        \tdef_bool $(cc-option,-mfoo)

        config COUNT
        \tint "Count"
        \trange 1 \\
        \t  64
        \tdefault 8 if BASE
        \tdefault 4

        if BASE
        menu "Options"
        \tvisible if !EXPERT

        config OPTION
        \tbool "Option"
        \tmodules
        \ttransitional

        endmenu
        endif

        choice
        \tprompt "Mode"
        \tdefault MODE_FAST

        config MODE_FAST
        \tbool "Fast"

        config MODE_SLOW
        \tbool "Slow"

        endchoice
        """,
    "arch/x86/Kconfig": """\
        config X86
        \tdef_bool y
        \tselect ARCH_THING

        config ARCH_THING
        \tbool
        """,
}


def write_tree(root: Path) -> None:
    for name, text in TREE.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(text), encoding="utf-8")


def read_tree() -> kconfig.KconfigTree:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        write_tree(root)
        return kconfig.read_kconfig(root)


class KconfigReaderTests(unittest.TestCase):
    def test_reads_symbols_prompts_and_blocks(self):
        tree = read_tree()
        self.assertNotIn("NOT_A_SYMBOL", tree.symbols)
        self.assertTrue(tree.symbols["BASE"].prompted)
        self.assertFalse(tree.symbols["HELPER"].prompted)
        self.assertFalse(tree.symbols["PROBED"].prompted)
        self.assertFalse(tree.symbols["ARCH_THING"].prompted)
        self.assertEqual(tree.symbols["COUNT"].type, "int")
        option = tree.symbols["OPTION"].definitions[0]
        assert option.dependency is not None and option.visibility is not None
        self.assertEqual(kconfig.format_expression(option.dependency), "BASE")
        self.assertEqual(kconfig.format_expression(option.visibility), "!EXPERT")
        self.assertEqual(
            [
                (name, kconfig.format_expression(condition))
                for name, condition in tree.selectors("HELPER")
                if condition
            ],
            [("BASE", "!EXPERT"), ("TWICE", "EXPERT")],
        )
        self.assertEqual(
            [name for name, _ in tree.selectors("ARCH_THING")], ["X86", "TWICE"]
        )
        self.assertEqual(
            [
                (name, condition and kconfig.format_expression(condition))
                for name, condition in tree.implicators("SOFT")
            ],
            [("BASE", None), ("TWICE", "EXPERT")],
        )
        choice = tree.choice_of("MODE_SLOW")
        assert choice is not None
        self.assertEqual(choice.members, ["MODE_FAST", "MODE_SLOW"])
        self.assertEqual(choice.defaults[0][0], "MODE_FAST")

    def test_an_entry_s_select_and_imply_apply_only_under_its_dependencies(self):
        tree = read_tree()
        self.assertEqual(len(tree.symbols["TWICE"].definitions), 2)
        # TWICE's second entry, inside `if EXPERT`, selects HELPER and implies
        # SOFT; its first entry selects ARCH_THING alone.
        plain = kconfig.ConfigValues(tree, {"TWICE": "y", "EXPERT": "n"})
        self.assertEqual(kconfig.active_selectors(tree, plain, "ARCH_THING"), ["TWICE"])
        self.assertEqual(kconfig.active_selectors(tree, plain, "HELPER"), [])
        self.assertEqual(kconfig.active_implicators(tree, plain, "SOFT"), [])
        expert = kconfig.ConfigValues(tree, {"TWICE": "y", "EXPERT": "y"})
        self.assertEqual(kconfig.active_selectors(tree, expert, "HELPER"), ["TWICE"])
        self.assertEqual(kconfig.active_implicators(tree, expert, "SOFT"), ["TWICE"])

    def test_reports_the_line_that_it_cannot_read(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Kconfig").write_text("config A\n\tbool\nfrobnicate A\n")
            with self.assertRaisesRegex(common.ScriptError, r"Kconfig:3"):
                kconfig.read_kconfig(root)

    def test_refuses_an_unclosed_block(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Kconfig").write_text("if A\nconfig B\n\tbool\n")
            with self.assertRaisesRegex(common.ScriptError, "unclosed if"):
                kconfig.read_kconfig(root)

    def test_reads_the_pinned_kernel_where_it_is_prepared(self):
        source = (
            common.cache_root()
            / KernelBuildConstants.SOURCE_DIRECTORY_NAME
            / KernelBuildConstants.SOURCE_NAME
        )
        if not (source / "Kconfig").is_file():
            self.skipTest("the pinned kernel source is not prepared")
        tree = kconfig.read_kconfig(source)
        self.assertTrue(tree.symbols["EXPERT"].prompted)
        self.assertFalse(tree.symbols["X86_64"].prompted)
        self.assertIn("X86", [name for name, _ in tree.selectors("PERF_EVENTS")])


class KconfigExpressionTests(unittest.TestCase):
    def test_parses_and_formats_with_precedence(self):
        for text in (
            "A",
            "!A",
            "A && B || C",
            "A && (B || C)",
            "!(A || B)",
            'A="y" && B!=4',
            "$(cc-option,-mfoo) && X",
        ):
            with self.subTest(text=text):
                expression = kconfig.parse_expression(text)
                self.assertEqual(
                    kconfig.format_expression(
                        kconfig.parse_expression(kconfig.format_expression(expression))
                    ),
                    kconfig.format_expression(expression),
                )

    def test_evaluates_tristate_logic_and_comparisons(self):
        tree = read_tree()
        values = kconfig.ConfigValues(tree, {"BASE": "y", "EXPERT": "n", "COUNT": "8"})
        cases = {
            "BASE": kconfig.YES,
            "EXPERT": kconfig.NO,
            "!EXPERT": kconfig.YES,
            "BASE && EXPERT": kconfig.NO,
            "BASE || EXPERT": kconfig.YES,
            "COUNT = 8": kconfig.YES,
            "COUNT > 10": kconfig.NO,
            "COUNT >= 0x8": kconfig.YES,
            "m": kconfig.MOD,
            "BASE && m": kconfig.MOD,
            "UNDEFINED": kconfig.NO,
        }
        for text, expected in cases.items():
            with self.subTest(text=text):
                self.assertEqual(
                    values.evaluate(kconfig.parse_expression(text)), expected
                )

    def test_a_toolchain_probe_is_unknown_unless_decided(self):
        tree = read_tree()
        values = kconfig.ConfigValues(tree, {"BASE": "y"})
        probe = "$(cc-option,-mfoo)"
        self.assertIsNone(values.evaluate(kconfig.parse_expression(probe)))
        self.assertEqual(
            values.evaluate(kconfig.parse_expression(f"{probe} || BASE")),
            kconfig.YES,
        )
        self.assertEqual(
            values.evaluate(kconfig.parse_expression(f"{probe} && !BASE")),
            kconfig.NO,
        )


if __name__ == "__main__":
    unittest.main()

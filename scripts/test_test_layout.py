"""Tests of check_test_layout.py, the check of where tests live."""

from __future__ import annotations

import contextlib
import io
import os
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Mapping
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_test_layout as layout  # noqa: E402

CARGO = '[package]\nname = "demo"\nversion = "0.1.0"\n'


class FakeTree:
    """The tracked files of a tree, in memory."""

    def __init__(self, files: Mapping[str, str]) -> None:
        self._files = {path: text.encode("utf-8") for path, text in files.items()}

    @property
    def files(self) -> frozenset[str]:
        return frozenset(self._files)

    def read(self, path: str) -> bytes:
        return self._files[path]


def crate(files: Mapping[str, str], allowlist: str | None = None) -> FakeTree:
    """A tree with one crate, demo/, that holds files, by their paths in it."""
    tree = {"demo/Cargo.toml": CARGO}
    tree.update({f"demo/{path}": text for path, text in files.items()})
    if allowlist is not None:
        tree[layout.ALLOWLIST] = allowlist
    return FakeTree(tree)


def allowlist(
    baseline: Mapping[str, tuple[int, int]] | None = None,
    seams: Mapping[str, int] | None = None,
) -> str:
    return layout.render_allowlist(
        layout.Allowlist(dict(baseline or {}), dict(seams or {}))
    )


def rules(tree: layout.Snapshot) -> list[tuple[str, int, str]]:
    return [
        (problem.path, problem.line, problem.rule)
        for problem in layout.check_snapshot(tree).problems
    ]


LIBRARY = "pub mod a;\n"
TESTED = "pub fn f() -> u8 {\n    1\n}\n\n#[cfg(test)]\nmod tests;\n"
TESTS = "use super::*;\n\n#[test]\nfn works() {\n    assert_eq!(f(), 1);\n}\n"


class TokenizeTests(unittest.TestCase):
    def texts(self, source: str) -> list[str]:
        return [token.text for token in layout.tokenize_rust(source)]

    def test_comments_are_no_tokens(self) -> None:
        source = "a // b #[cfg(test)]\n/* c /* d */ e */ f\n/// g\n//! h\n/** i */ j"
        self.assertEqual(self.texts(source), ["a", "f", "j"])

    def test_literals_hide_what_looks_like_code(self) -> None:
        source = (
            'x("// #[cfg(test)] /* \\" ", r#"a " b"#, br##"#"##, b"\\\\", c"z",'
            " '\"', '{', '\\'', '\\\\', '\\u{1F600}', b'\\'')"
        )
        tokens = layout.tokenize_rust(source)
        self.assertEqual(
            [token.text for token in tokens if token.kind == layout.LITERAL],
            [
                '"// #[cfg(test)] /* \\" "',
                'r#"a " b"#',
                'br##"#"##',
                'b"\\\\"',
                'c"z"',
                "'\"'",
                "'{'",
                "'\\''",
                "'\\\\'",
                "'\\u{1F600}'",
                "b'\\''",
            ],
        )
        self.assertEqual(layout.match_brackets(tokens)[1], len(tokens) - 1)

    def test_lifetimes_and_labels_are_no_char_literals(self) -> None:
        source = (
            "fn f<'a>(x: &'a str) -> &'static str { 'outer: loop { break 'outer; } }"
        )
        tokens = layout.tokenize_rust(source)
        self.assertEqual(
            [token.text for token in tokens if token.kind == layout.LIFETIME],
            ["'a", "'a", "'static", "'outer", "'outer"],
        )
        self.assertEqual(
            [token.text for token in layout.tokenize_rust("['a', 'b']")],
            ["[", "'a'", ",", "'b'", "]"],
        )

    def test_raw_identifiers_and_numbers(self) -> None:
        self.assertEqual(self.texts("r#type 0..10 1.5e3f64 x.0"), [
            "r#type", "0", ".", ".", "10", "1.5e3f64", "x", ".", "0",
        ])  # fmt: skip
        self.assertEqual(layout.unraw("r#type"), "type")

    def test_tokens_know_their_lines(self) -> None:
        tokens = layout.tokenize_rust('a\n"b\nc"\n\nd')
        self.assertEqual([token.line for token in tokens], [1, 2, 5])

    def test_unterminated_literals_and_comments_fail(self) -> None:
        for source, line in (('a\n"b', 2), ("/* a\n", 1), ('r#"a"', 1)):
            with self.subTest(source=source):
                with self.assertRaises(layout.RustSyntaxError) as raised:
                    layout.tokenize_rust(source)
                self.assertEqual(raised.exception.line, line)

    def test_unbalanced_brackets_fail(self) -> None:
        for source in ("(]", "{", "}"):
            with self.subTest(source=source):
                with self.assertRaises(layout.RustSyntaxError):
                    layout.match_brackets(layout.tokenize_rust(source))


class CfgTests(unittest.TestCase):
    def cfg(self, source: str) -> layout.Cfg:
        tokens = layout.tokenize_rust(source)
        partner = layout.match_brackets(tokens)
        return layout.parse_cfg(tokens, partner, 0, len(tokens))

    def test_requires_forbids_and_mentions_test(self) -> None:
        for source, requires, forbids, mentions in (
            ("test", True, False, True),
            ("not(test)", False, True, True),
            ("all(test, unix)", True, False, True),
            ("all(unix, not(test))", False, True, True),
            ('any(test, feature = "cli")', False, False, True),
            ("any(test, all(test, unix))", True, False, True),
            ("not(not(test))", True, False, True),
            ("any()", False, False, False),
            ('feature = "test"', False, False, False),
            ("unix", False, False, False),
            ("$test", False, False, False),
            ("all(test, $rest)", True, False, True),
        ):
            with self.subTest(source=source):
                cfg = self.cfg(source)
                self.assertEqual(layout.requires_test(cfg), requires)
                self.assertEqual(layout.forbids_test(cfg), forbids)
                self.assertEqual(layout.mentions_test(cfg), mentions)

    def test_attributes(self) -> None:
        source = (
            "#[cfg(test)] #[cfg_attr(test, derive(Debug))] #[cfg_attr(unix, cfg(test))] "
            '#[cfg_attr(unix, path = "x.rs")] #[tokio::test] #![allow(dead_code)]'
        )
        tokens = layout.tokenize_rust(source)
        partner = layout.match_brackets(tokens)
        attributes = [
            attribute
            for index in range(len(tokens))
            if (attribute := layout.attribute_at(tokens, partner, index)) is not None
        ]
        self.assertEqual(
            [(attribute.path, attribute.inner) for attribute in attributes],
            [
                ("cfg", False),
                ("cfg_attr", False),
                ("cfg_attr", False),
                ("cfg_attr", False),
                ("tokio::test", False),
                ("allow", True),
            ],
        )
        self.assertEqual(
            [
                layout.attribute_mentions_test(tokens, partner, attribute)
                for attribute in attributes
            ],
            [True, True, True, False, False, False],
        )
        self.assertEqual(
            [
                layout.attribute_requires_test(tokens, partner, attribute)
                for attribute in attributes
            ],
            [True, False, False, False, False, False],
        )
        self.assertIn("path", layout.attribute_paths(tokens, partner, attributes[3]))
        self.assertTrue(layout.is_test_attribute("tokio::test"))
        self.assertTrue(layout.is_test_attribute("should_panic"))
        self.assertFalse(layout.is_test_function_attribute("ignore"))


class ItemTests(unittest.TestCase):
    def test_items_end_where_rust_ends_them(self) -> None:
        source = """
#![allow(unused)]
use std::{collections::HashMap, fmt};
extern crate alloc as heap;
pub(crate) const LIMIT: Limit = Limit { value: { 3 } };
const _: () = assert!(true);
static mut COUNT: u8 = 0;
pub const fn twice(x: u8) -> u8 { x * 2 }
pub unsafe extern "C" fn raw() {}
unsafe extern "C" { fn ffi(); static OUTSIDE: u8; }
struct Unit;
struct Pair(u8, u8);
struct Named<T> where T: Copy { field: T }
pub enum Kind { A = { 1 }, B }
union Bits { a: u8 }
trait Shape: Send { const SIDES: u8; fn area(&self) -> u8; type Unit; }
unsafe impl<T> Send for Named<T> where T: Copy {}
impl Pair { pub fn sum(&self) -> u8 { self.0 + self.1 } #[cfg(test)] fn probe() {} }
type Alias = Pair;
macro_rules! square { ($x:expr) => { $x * $x }; }
macro_rules! cube ( ($x:expr) => { $x * $x * $x } );
thread_local! { static SEEN: u8 = const { 0 }; }
std::println!("{}", 1);
mod inline { fn inner() {} }
mod declared;
"""
        parsed = layout.parse_rust(source)
        self.assertEqual(
            [attribute.path for attribute in parsed.inner_attributes], ["allow"]
        )
        self.assertEqual(
            [(item.kind, item.name) for item in parsed.items],
            [
                ("use", ""),
                ("extern crate", "alloc"),
                ("const", "LIMIT"),
                ("const", "_"),
                ("static", "COUNT"),
                ("fn", "twice"),
                ("fn", "raw"),
                ("extern", ""),
                ("struct", "Unit"),
                ("struct", "Pair"),
                ("struct", "Named"),
                ("enum", "Kind"),
                ("union", "Bits"),
                ("trait", "Shape"),
                ("impl", "impl < T > Send for Named < T > where T : Copy"),
                ("impl", "impl Pair"),
                ("type", "Alias"),
                ("macro_rules", "square"),
                ("macro_rules", "cube"),
                ("macro", "thread_local"),
                ("macro", "std::println"),
                ("mod", "inline"),
                ("mod", "declared"),
            ],
        )
        children = {item.name: item.children for item in parsed.items}
        self.assertEqual(
            [(child.kind, child.name) for child in children["impl Pair"]],
            [("fn", "sum"), ("fn", "probe")],
        )
        self.assertEqual(
            [(child.kind, child.name) for child in children["Shape"]],
            [("const", "SIDES"), ("fn", "area"), ("type", "Unit")],
        )
        self.assertEqual(
            [(child.kind, child.name) for child in children[""]],
            [("fn", "ffi"), ("static", "OUTSIDE")],
        )
        self.assertEqual([child.name for child in children["inline"]], ["inner"])
        self.assertIsNone(parsed.items[-1].body)

    def test_braces_in_generic_arguments_open_no_body(self) -> None:
        parsed = layout.parse_rust(
            "impl S<{ 1 }> {\n    fn a() {}\n}\n"
            "fn g<F: Fn() -> u8>(f: F) -> Option<u8> where F: Copy {\n    None\n}\n"
            "fn compare() -> bool { 1 < 2 }\n"
            "fn after() {}\n"
        )
        self.assertEqual(
            [(item.kind, item.name) for item in parsed.items],
            [
                ("impl", "impl S < { 1 } >"),
                ("fn", "g"),
                ("fn", "compare"),
                ("fn", "after"),
            ],
        )
        self.assertEqual([child.name for child in parsed.items[0].children], ["a"])


class ModulePathTests(unittest.TestCase):
    def test_roles_and_module_files(self) -> None:
        for path, role in (
            ("demo/src/a.rs", "production"),
            ("demo/src/a/tests.rs", "test"),
            ("demo/src/a/tests/b.rs", "test"),
            ("demo/src/tests/mod.rs", "test"),
            ("demo/tests/cli.rs", "test"),
            ("demo/build.rs", ""),
            ("demo/src/a.txt", ""),
        ):
            with self.subTest(path=path):
                self.assertEqual(layout.rust_role(path, "demo"), role)
        self.assertEqual(
            layout.module_files("demo", "a/b.rs", ("inner",), "tests"),
            ("demo/src/a/b/inner/tests.rs", "demo/src/a/b/inner/tests/mod.rs"),
        )
        self.assertEqual(
            layout.module_files("demo", "a/mod.rs", (), "tests")[0],
            "demo/src/a/tests.rs",
        )
        self.assertEqual(
            layout.module_files("demo", "lib.rs", (), "tests")[0], "demo/src/tests.rs"
        )
        self.assertEqual(
            layout.module_files("demo", "bin/tool.rs", (), "x")[0],
            "demo/src/bin/x.rs",
        )
        self.assertEqual(layout.module_path("a/b/mod.rs"), ("a", "b"))
        self.assertEqual(layout.module_path("lib.rs"), ())
        self.assertEqual(layout.module_path("bin/tool.rs"), ("bin:tool",))
        self.assertEqual(layout.crate_of("x/demo/src/a.rs", ("", "x/demo")), "x/demo")


class RustRuleTests(unittest.TestCase):
    def test_tests_in_their_own_files_pass(self) -> None:
        tree = crate(
            {"src/lib.rs": LIBRARY, "src/a.rs": TESTED, "src/a/tests.rs": TESTS}
        )
        self.assertEqual(rules(tree), [])

    def test_test_code_in_production_files_fails(self) -> None:
        for source, rule in (
            (
                "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
                "inline-test-module",
            ),
            ("#[cfg(all(test, unix))]\nmod checks {}\n", "inline-test-module"),
            ("mod checks {\n    #![cfg(test)]\n}\n", "inline-test-module"),
            ("#[cfg(test)]\nmod support;\n", "test-module-name"),
            ("#[cfg(test)]\nfn helper() {}\n", "test-only-item"),
            ("#[cfg_attr(all(), cfg(test))]\nfn helper() {}\n", "test-only-item"),
            (
                "struct S<const N: u8>;\nimpl S<{ 1 }> {\n    #[cfg(test)]\n"
                "    fn helper() {}\n}\n",
                "test-only-item",
            ),
            ("#[cfg(test)]\nuse std::fmt;\n", "test-only-item"),
            (
                "struct S;\nimpl S {\n    #[cfg(test)]\n    fn probe() {}\n}\n",
                "test-only-item",
            ),
            ("#[test]\nfn stray() {}\n", "test-harness"),
            ("#[cfg_attr(test, test)]\nfn applied() {}\n", "test-harness"),
            ("proptest! {\n    #[test]\n    fn p(x in 0..1u8) {}\n}\n", "test-harness"),
            ("#![cfg(test)]\nfn f() {}\n", "test-only-file"),
        ):
            with self.subTest(source=source):
                tree = crate(
                    {
                        "src/lib.rs": LIBRARY,
                        "src/a.rs": source,
                        "src/a/support.rs": "",
                    }
                )
                found = rules(tree)
                self.assertIn(rule, {found_rule for _, _, found_rule in found})
                self.assertTrue(all(path == "demo/src/a.rs" for path, _, _ in found))

    def test_tests_modules_require_test_and_their_files(self) -> None:
        for files, rule in (
            ({"src/a.rs": "mod tests;\n", "src/a/tests.rs": ""}, "tests-module"),
            ({"src/a.rs": "mod tests {}\n"}, "tests-module"),
            ({"src/a.rs": "#[cfg(test)]\nmod tests;\n"}, "tests-module"),
            (
                {
                    "src/a.rs": "#[cfg(test)]\nmod tests;\n",
                    "src/a/tests.rs": "",
                    "src/a/tests/mod.rs": "",
                },
                "tests-module",
            ),
            ({"src/a.rs": "", "src/a/tests.rs": ""}, "orphan-test-file"),
            (
                {"src/a.rs": '#[path = "b.rs"]\nmod b;\n', "src/b.rs": ""},
                "path-attribute",
            ),
        ):
            with self.subTest(files=files):
                found = rules(crate({"src/lib.rs": LIBRARY, **files}))
                self.assertIn(rule, {found_rule for _, _, found_rule in found})

    def test_platform_only_tests_modules_need_their_files_too(self) -> None:
        tree = crate(
            {
                "src/lib.rs": LIBRARY,
                "src/a.rs": "#[cfg(all(test, windows))]\nmod tests;\n",
            }
        )
        self.assertEqual(rules(tree), [("demo/src/a.rs", 2, "tests-module")])

    def test_test_files_declared_in_test_code_belong_to_a_module(self) -> None:
        tree = crate(
            {
                "src/lib.rs": LIBRARY,
                "src/a.rs": "#[cfg(test)]\nmod tests;\n",
                "src/a/tests.rs": "mod vectors;\nmod inner {\n    mod deep;\n}\n",
                "src/a/tests/vectors.rs": "",
                "src/a/tests/inner/deep.rs": "",
            }
        )
        self.assertEqual(rules(tree), [])

    def test_test_files_that_no_target_reaches_never_compile(self) -> None:
        files = {"src/a.rs": TESTED, "src/a/tests.rs": TESTS}
        self.assertEqual(rules(crate({"src/lib.rs": LIBRARY, **files})), [])
        self.assertEqual(
            rules(crate({"src/lib.rs": "", **files})),
            [("demo/src/a/tests.rs", 0, "orphan-test-file")],
        )
        binary = {
            "src/lib.rs": "",
            "src/bin/tool/main.rs": "mod a;\n\nfn main() {}\n",
            "src/bin/tool/a.rs": TESTED,
            "src/bin/tool/a/tests.rs": TESTS,
        }
        self.assertEqual(rules(crate(binary)), [])
        # Cargo runs the unit tests of no target whose `test` is false.
        manifest = (
            CARGO
            + '\n[[bin]]\nname = "tool"\npath = "src/bin/tool/main.rs"\ntest = false\n'
        )
        self.assertEqual(
            rules(crate({**binary, "Cargo.toml": manifest})),
            [("demo/src/bin/tool/a/tests.rs", 0, "orphan-test-file")],
        )

    def test_integration_tests_reach_their_modules(self) -> None:
        files = {
            "src/lib.rs": "",
            "tests/cli.rs": '#[path = "cli/harness.rs"]\nmod harness;\nmod common;\n',
            "tests/cli/harness.rs": TESTS,
            "tests/common/mod.rs": "pub mod gpt;\n",
            "tests/common/gpt.rs": "",
            "tests/suite/main.rs": 'mod inner {\n    #[path = "cases.rs"]\n    mod cases;\n}\n',
            "tests/suite/inner/cases.rs": TESTS,
            # Data, which no test declares.
            "tests/fixtures/generate.rs": TESTS,
            "tests/vectors/openvmm/a.rs": TESTS,
        }
        self.assertEqual(rules(crate(files)), [])
        unused = {**files, "tests/support/unused.rs": TESTS}
        self.assertEqual(
            rules(crate(unused)),
            [("demo/tests/support/unused.rs", 0, "orphan-test-file")],
        )
        manifest = CARGO + '\n[[test]]\nname = "suite"\ntest = false\n'
        self.assertEqual(
            rules(crate({**files, "Cargo.toml": manifest})),
            [
                ("demo/tests/suite/inner/cases.rs", 0, "orphan-test-file"),
                ("demo/tests/suite/main.rs", 0, "orphan-test-file"),
            ],
        )

    def test_declarations_that_no_test_build_compiles_reach_nothing(self) -> None:
        files = {
            "src/lib.rs": (
                "#[cfg(any())]\nmod a;\n#[cfg(windows)]\nmod b;\n"
                "#[cfg_attr(test, cfg(any()))]\nmod c;\nmod d;\n"
                "#[cfg(all(unix, not(all())))]\nmod e {\n    mod f;\n}\n"
            ),
            "src/a.rs": TESTED,
            "src/a/tests.rs": TESTS,
            "src/b.rs": TESTED,
            "src/b/tests.rs": TESTS,
            "src/c.rs": TESTED,
            "src/c/tests.rs": TESTS,
            "src/d.rs": "#![cfg(any())]\n" + TESTED,
            "src/d/tests.rs": TESTS,
            "src/e/f.rs": TESTED,
            "src/e/f/tests.rs": TESTS,
        }
        # A platform's module compiles where its predicate holds.
        self.assertEqual(
            rules(crate(files, allowlist(seams={"demo/src/lib.rs": 1}))),
            [
                ("demo/src/a/tests.rs", 0, "orphan-test-file"),
                ("demo/src/c/tests.rs", 0, "orphan-test-file"),
                ("demo/src/d/tests.rs", 0, "orphan-test-file"),
                ("demo/src/e/f/tests.rs", 0, "orphan-test-file"),
            ],
        )

    def test_manifests_name_the_targets_whose_tests_run(self) -> None:
        found = {
            "src/lib.rs",
            "src/main.rs",
            "src/bin/tool.rs",
            "src/bin/other/main.rs",
            "tests/cli.rs",
            "tests/suite/main.rs",
            "benches/speed.rs",
            "examples/demo.rs",
        }
        self.assertEqual(
            layout.target_roots(CARGO, found),
            {
                "src/lib.rs": True,
                "src/main.rs": True,
                "src/bin/tool.rs": True,
                "src/bin/other/main.rs": True,
                "tests/cli.rs": True,
                "tests/suite/main.rs": True,
                "benches/speed.rs": False,
                "examples/demo.rs": False,
            },
        )
        manifest = (
            CARGO
            + "autobins = false\nautotests = false\n\n"
            + '[lib]\npath = "src/core.rs"\ntest = false\n\n'
            + '[[bin]]\nname = "tool"\n\n'
            + '[[test]]\nname = "suite"\n\n'
            + '[[example]]\nname = "demo"\ntest = true\n'
        )
        self.assertEqual(
            layout.target_roots(manifest, found),
            {
                "src/core.rs": False,
                "src/bin/tool.rs": True,
                "tests/suite/main.rs": True,
                "benches/speed.rs": False,
                "examples/demo.rs": True,
            },
        )
        # A library that the manifest roots elsewhere no longer compiles the
        # modules that src/lib.rs declares.
        files = {
            "src/lib.rs": "mod a;\n",
            "src/a.rs": TESTED,
            "src/a/tests.rs": TESTS,
            "src/core.rs": "mod b;\n",
            "src/b.rs": TESTED,
            "src/b/tests.rs": TESTS,
        }
        self.assertEqual(
            rules(crate(files)), [("demo/src/b/tests.rs", 0, "orphan-test-file")]
        )
        rooted = CARGO + '\n[lib]\npath = "src/core.rs"\n'
        self.assertEqual(
            rules(crate({**files, "Cargo.toml": rooted})),
            [("demo/src/a/tests.rs", 0, "orphan-test-file")],
        )
        tests = {"src/lib.rs": "", "tests/auto.rs": TESTS, "tests/named.rs": TESTS}
        self.assertEqual(rules(crate(tests)), [])
        named = CARGO + 'autotests = false\n\n[[test]]\nname = "named"\n'
        self.assertEqual(
            rules(crate({**tests, "Cargo.toml": named})),
            [("demo/tests/auto.rs", 0, "orphan-test-file")],
        )
        # The root of a target whose tests never run still compiles as a module
        # that another target declares.
        shared = {"src/lib.rs": "", "tests/suite.rs": "mod common;\n"}
        unrun = CARGO + '\n[[test]]\nname = "common"\ntest = false\n'
        self.assertEqual(
            rules(crate({**shared, "tests/common.rs": TESTS, "Cargo.toml": unrun})), []
        )
        # TOML's literal and basic strings both name a file, and a declaration
        # of a target that the check cannot read is a problem.
        for spelled in ("'src/core.rs'", '"src\\u002fcore.rs"'):
            with self.subTest(spelled=spelled):
                self.assertEqual(
                    layout.target_roots(
                        CARGO + f"\n[lib]\npath = {spelled}\n", {"src/lib.rs"}
                    ),
                    {"src/core.rs": True},
                )
        for manifest, line in (
            ('lib = { path = "src/core.rs" }\n' + CARGO, 1),
            ('[lib]\npath = """src/core.rs"""\n' + CARGO, 2),
        ):
            with self.subTest(manifest=manifest):
                unread: list[tuple[int, str]] = []
                layout.target_roots(manifest, {"src/lib.rs"}, unread)
                self.assertEqual([found for found, _ in unread], [line])
                self.assertIn(
                    ("demo/Cargo.toml", line, "manifest"),
                    rules(crate({"src/lib.rs": "", "Cargo.toml": manifest})),
                )

    def test_test_hooks_need_seams(self) -> None:
        source = (
            "#[cfg_attr(test, derive(Debug))]\n"
            "pub struct S {\n"
            "    #[cfg(test)]\n"
            "    seen: u8,\n"
            "}\n"
            "pub fn f() -> bool {\n"
            "    #[cfg(test)]\n"
            "    pause();\n"
            "    cfg!(test)\n"
            "}\n"
            '#[cfg(any(test, feature = "cli"))]\n'
            "pub fn g() {}\n"
            "/// #[cfg(test)] in a doc comment\n"
            'pub const DOC: &str = "#[cfg(test)]";\n'
            "#[cfg(test)]\n"
            "mod tests;\n"
        )
        files = {
            "src/lib.rs": LIBRARY,
            "src/a.rs": source,
            "src/a/tests.rs": "",
        }
        self.assertEqual(rules(crate(files)), [("demo/src/a.rs", 0, "test-hooks")])
        seams = {"demo/src/a.rs": 5}
        self.assertEqual(rules(crate(files, allowlist(seams=seams))), [])
        self.assertEqual(
            rules(crate(files, allowlist(seams={"demo/src/a.rs": 6}))),
            [("demo/src/a.rs", 0, "allowlist")],
        )
        self.assertEqual(
            rules(crate(files, allowlist(seams={"demo/src/a.rs": 4}))),
            [("demo/src/a.rs", 0, "test-hooks")],
        )
        analysis = layout.check_snapshot(crate(files)).analyses["demo/src/a.rs"]
        self.assertEqual(analysis.hooks, [1, 3, 7, 9, 11])

    def test_seams_allow_hooks_but_no_test_code(self) -> None:
        files = {
            "src/lib.rs": LIBRARY,
            "src/a.rs": "#[cfg(test)]\nfn helper() {}\n",
        }
        found = rules(crate(files, allowlist(seams={"demo/src/a.rs": 1})))
        self.assertEqual(found, [("demo/src/a.rs", 2, "test-only-item")])

    def test_baseline_files_keep_their_test_code_but_no_more(self) -> None:
        legacy = (
            "pub fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n"
        )
        files = {"src/lib.rs": LIBRARY, "src/a.rs": legacy}
        path = "demo/src/a.rs"
        self.assertEqual(rules(crate(files, allowlist({path: (1, 1)}))), [])
        grown = legacy.replace("fn t() {}", "fn t() {}\n    #[test]\n    fn u() {}")
        self.assertEqual(
            rules(crate({**files, "src/a.rs": grown}, allowlist({path: (1, 1)}))),
            [(path, 0, "test-code")],
        )
        helper = legacy + "#[cfg(test)]\nfn helper() {}\n"
        self.assertEqual(
            rules(crate({**files, "src/a.rs": helper}, allowlist({path: (1, 1)}))),
            [(path, 0, "test-code")],
        )
        self.assertEqual(
            rules(crate(files, allowlist({path: (2, 1)}))), [(path, 0, "allowlist")]
        )
        moved = {
            "src/lib.rs": LIBRARY,
            "src/a.rs": "pub fn f() {}\n#[cfg(test)]\nmod tests;\n",
            "src/a/tests.rs": "#[test]\nfn t() {}\n",
        }
        self.assertEqual(
            rules(crate(moved, allowlist({path: (1, 1)}))), [(path, 0, "allowlist")]
        )

    def test_unreadable_files_fail(self) -> None:
        tree = crate({"src/lib.rs": "fn f() {\n"})
        self.assertEqual(rules(tree), [("demo/src/lib.rs", 1, "rust-syntax")])


class AllowlistTests(unittest.TestCase):
    def test_entries_must_name_production_files(self) -> None:
        files = {"src/lib.rs": LIBRARY, "src/a.rs": "#[cfg(test)]\nfn f() {}\n"}
        both = layout.render_allowlist(
            layout.Allowlist({"demo/src/a.rs": (1, 0)}, {"demo/src/a.rs": 1})
        )
        found = rules(crate(files, both))
        self.assertIn((layout.ALLOWLIST, 0, "allowlist"), found)
        stale = allowlist(seams={"demo/src/gone.rs": 1})
        self.assertEqual(
            rules(crate({"src/lib.rs": LIBRARY}, stale)),
            [(layout.ALLOWLIST, 0, "allowlist")],
        )

    def test_the_form_is_checked(self) -> None:
        for text in (
            "[]",
            "{",
            '{"schema": "other", "schema_version": 1, "baseline": {}, "seams": {}}\n',
            '{"schema": "nvx-test-layout-allowlist", "schema_version": 1, '
            '"baseline": {}, "seams": {"a": 0}}\n',
            allowlist().replace("\n", "\r\n").replace("  ", "    "),
        ):
            with self.subTest(text=text):
                errors = layout.parse_allowlist(text)[1]
                self.assertNotEqual(errors, [])
        self.assertEqual(layout.parse_allowlist(allowlist())[1], [])

    def test_tighten_only_lowers(self) -> None:
        analyses = {
            "demo/src/lowered.rs": layout.RustAnalysis(
                "demo/src/lowered.rs", "demo", True, hooks=[3], tests=1,
                legacy=[layout.Problem("demo/src/lowered.rs", 3, "inline-test-module", "")],
            ),
            "demo/src/moved.rs": layout.RustAnalysis(
                "demo/src/moved.rs", "demo", True, hooks=[4, 9]
            ),
            "demo/src/clean.rs": layout.RustAnalysis("demo/src/clean.rs", "demo", True),
            "demo/src/grown.rs": layout.RustAnalysis(
                "demo/src/grown.rs", "demo", True, hooks=[1, 2, 3]
            ),
            "demo/src/fewer.rs": layout.RustAnalysis(
                "demo/src/fewer.rs", "demo", True, hooks=[1]
            ),
            "demo/src/new.rs": layout.RustAnalysis(
                "demo/src/new.rs", "demo", True, hooks=[1]
            ),
        }  # fmt: skip
        before = layout.Allowlist(
            {
                "demo/src/lowered.rs": (2, 3),
                "demo/src/moved.rs": (5, 4),
                "demo/src/clean.rs": (1, 1),
                "demo/src/gone.rs": (1, 1),
            },
            {"demo/src/grown.rs": 2, "demo/src/fewer.rs": 2},
        )
        after = layout.tighten(before, analyses)
        self.assertEqual(after.baseline, {"demo/src/lowered.rs": (1, 1)})
        self.assertEqual(
            after.seams,
            {"demo/src/moved.rs": 2, "demo/src/grown.rs": 2, "demo/src/fewer.rs": 1},
        )


class PythonRuleTests(unittest.TestCase):
    def test_tests_live_where_the_test_jobs_find_them(self) -> None:
        for path, allowed in (
            ("scripts/test_a.py", True),
            (".github/specula/tests/test_a.py", True),
            (".github/skills/demo/scripts/test_a.py", True),
            ("scripts/nvx_tools/test_a.py", False),
            ("tools/test_a.py", False),
            ("scripts/nvx_tools/scenario_tests.py", True),
            ("scripts/scenario_tests.py", False),
        ):
            with self.subTest(path=path):
                problems = layout.python_problems(path, "")
                self.assertEqual(problems == [], allowed)

    def test_production_modules_import_no_test_code(self) -> None:
        for source, allowed in (
            ("import json\nfrom pathlib import Path\n", True),
            ("import unittest\n", False),
            ("from unittest.mock import patch\n", False),
            ("import pytest\n", False),
            ("import doctest\n", False),
            ("import test_nvx_tools\n", False),
            ("from powershell_testing import run_powershell\n", False),
            ("from nvx_tools.guest_binaries import test_guest_binaries\n", True),
            ("def f(:\n", False),
        ):
            with self.subTest(source=source):
                problems = layout.python_problems("scripts/tool.py", source)
                self.assertEqual(problems == [], allowed)
        self.assertEqual(
            layout.python_problems(
                "scripts/powershell_testing.py", "import unittest\n"
            ),
            [],
        )
        self.assertEqual(
            layout.python_problems("scripts/test_a.py", "import unittest\n"), []
        )

    def test_imports_resolve_to_the_tracked_test_modules(self) -> None:
        files = frozenset(
            {
                "scripts/tool.py",
                "scripts/test_tool.py",
                "scripts/nvx_tools/__init__.py",
                "scripts/nvx_tools/helper.py",
                "scripts/nvx_tools/test_local.py",
                "scripts/powershell_testing.py",
            }
        )
        for path, source, allowed in (
            ("scripts/tool.py", "from scripts import test_tool\n", False),
            ("scripts/tool.py", "from scripts import test_tool as alias\n", False),
            ("scripts/nvx_tools/helper.py", "from . import test_local\n", False),
            ("scripts/nvx_tools/helper.py", "from .. import test_tool\n", False),
            ("scripts/nvx_tools/helper.py", "from .test_local import x\n", False),
            ("scripts/tool.py", "from scripts import powershell_testing\n", False),
            ("scripts/nvx_tools/helper.py", "from . import helper\n", True),
            ("scripts/tool.py", "from nvx_tools.helper import test_helper\n", True),
            ("scripts/nvx_tools/helper.py", "from .unittest import x\n", True),
        ):
            with self.subTest(source=source):
                problems = layout.python_problems(path, source, files)
                self.assertEqual(problems == [], allowed, problems)


class RepositoryTests(unittest.TestCase):
    def test_the_repository_follows_the_layout(self) -> None:
        layout_found = layout.check_snapshot(layout.WorkTree(layout.REPO_ROOT))
        self.assertEqual([str(problem) for problem in layout_found.problems], [])

    def test_main_tightens_the_allowlist_and_reports_problems(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            files = {
                "demo/Cargo.toml": CARGO,
                "demo/src/lib.rs": LIBRARY,
                "demo/src/a.rs": "#[cfg(test)]\nfn helper() {}\n",
                layout.ALLOWLIST: allowlist(seams={"demo/src/a.rs": 1}),
            }
            for path, text in files.items():
                (root / path).parent.mkdir(parents=True, exist_ok=True)
                (root / path).write_text(text, encoding="utf-8", newline="\n")
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(
                ["git", "-c", "core.autocrlf=false", "add", "-A"],
                cwd=root,
                check=True,
                capture_output=True,
            )

            def run(*arguments: str) -> tuple[int, str]:
                output = io.StringIO()
                with (
                    contextlib.redirect_stdout(output),
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    status = layout.main(["--root", directory, *arguments])
                return status, output.getvalue()

            status, output = run()
            self.assertEqual(status, 1)
            self.assertIn("demo/src/a.rs:2: test-only-item:", output)
            with patch.dict(os.environ, {"GITHUB_ACTIONS": "true"}):
                self.assertIn("::error file=demo/src/a.rs,line=2::", run()[1])
            listed = (root / layout.ALLOWLIST).read_text(encoding="utf-8")
            (root / "demo/src/a.rs").write_text(
                "#[cfg(test)]\nfn helper( {}\n", encoding="utf-8"
            )
            status, output = run("--tighten")
            self.assertEqual(status, 1)
            self.assertIn("demo/src/a.rs:2: rust-syntax:", output)
            self.assertEqual(
                (root / layout.ALLOWLIST).read_text(encoding="utf-8"), listed
            )
            (root / "demo/src/a.rs").write_text("pub fn f() {}\n", encoding="utf-8")
            (root / layout.ALLOWLIST).write_text("{", encoding="utf-8")
            status, output = run("--tighten")
            self.assertEqual(status, 1)
            self.assertIn(f"{layout.ALLOWLIST}: ", output)
            self.assertEqual((root / layout.ALLOWLIST).read_text(encoding="utf-8"), "{")
            (root / layout.ALLOWLIST).write_text(listed, encoding="utf-8")
            self.assertEqual(run()[0], 1)
            self.assertEqual(run("--tighten"), (0, ""))
            self.assertEqual(
                (root / layout.ALLOWLIST).read_text(encoding="utf-8"), allowlist()
            )


if __name__ == "__main__":
    unittest.main()

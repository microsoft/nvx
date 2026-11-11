"""Tests of check_test_changes.py, which flags the changes to existing tests."""

from __future__ import annotations

import contextlib
import io
import json
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_test_changes as changes  # noqa: E402
import check_test_layout as layout  # noqa: E402

CARGO = '[package]\nname = "demo"\nversion = "0.1.0"\n'
WORKFLOW = layout.REPO_ROOT / ".github" / "workflows" / "test-changes.yml"


def flagged(found: Sequence[changes.Change]) -> list[tuple[str, str, str]]:
    return sorted(
        (change.category, change.action, change.kind)
        for change in found
        if change.flags
    )


def ways(*predicates: set[str]) -> changes.Conditions:
    """Returns the ways that code compiles, one for each set of predicates."""
    return frozenset(frozenset(way) for way in predicates)


ANYWHERE = ways(set())


def actions(found: Sequence[changes.Change]) -> list[str]:
    return sorted(change.action for change in found)


def rust(
    base: Mapping[str, str], head: Mapping[str, str], crate: str = "demo"
) -> list[changes.Change]:
    """Compares the test entries of Rust files of a crate, whose modules
    inherit the cfg predicates of their declarations among the files."""

    def entries(files: Mapping[str, str]) -> list[changes.Entry]:
        full = {f"{crate}/{path}": text for path, text in files.items()}
        # The crate's root declares the top-level modules, unless given.
        root = f"{crate}/src/lib.rs"
        names = sorted(
            {
                path.split("/")[2].removesuffix(".rs")
                for path in full
                if path.startswith(f"{crate}/src/") and path != root
            }
        )
        tree = {root: "".join(f"mod {name};\n" for name in names), **full}
        inherited = changes.module_conditions(
            tree.keys(),
            lambda paths: {path: tree[path] for path in paths},
            (crate,),
            full,
        )
        found: list[changes.Entry] = []
        for path, text in full.items():
            role = layout.rust_role(path, crate)
            found.extend(
                changes.rust_entries(
                    path,
                    crate,
                    text,
                    changes.TEST_CODE,
                    role == "test",
                    # Files that no root among these reaches still compare.
                    inherited[path] or changes.ANYWHERE,
                )
            )
        return found

    return changes.compare(entries(base), entries(head))


def python(base: Mapping[str, str], head: Mapping[str, str]) -> list[changes.Change]:
    def entries(files: Mapping[str, str]) -> list[changes.Entry]:
        found: list[changes.Entry] = []
        for path, text in files.items():
            found.extend(changes.python_entries(path, text, changes.TEST_CODE))
        return found

    return changes.compare(entries(base), entries(head))


INLINE = """pub fn f() -> u8 {
    1
}

/// The tests of `f`.
#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED: u8 = 1;

    #[test]
    fn works() {
        assert_eq!(f(), EXPECTED);
    }
}
"""
DECLARED = """pub fn f() -> u8 {
    1
}

/// The tests of `f`.
#[cfg(test)]
mod tests;
"""
MOVED = """//! The tests of `f`.

use super::*;

const EXPECTED: u8 = 1;

#[test]
fn works() {
    // The value that `f` documents.
    assert_eq!(f(), EXPECTED);
}
"""


class CompareTests(unittest.TestCase):
    def entry(
        self,
        key: str,
        content: str,
        moved: str | None = None,
        name: str = "",
        kind: str = "test",
        path: str = "a.rs",
        monotone: bool = False,
    ) -> changes.Entry:
        return changes.Entry(
            ("rust", key),
            ("rust", "fn", name or key),
            ("rust", "scope"),
            name or key,
            content,
            content if moved is None else moved,
            changes.TEST_CODE,
            kind,
            f"`{key}`",
            changes.Location(path, 1),
            monotone,
        )

    def test_entries_change_by_identity_then_by_content(self) -> None:
        base = [
            self.entry("same", "a b c"),
            self.entry("edited", "a b c"),
            self.entry("moved", "x y z", path="old.rs"),
            self.entry("relocated", "p q r", moved="P"),
            self.entry("gone", "1 2 3"),
            self.entry("imports", "a\nb", kind="imports", monotone=True),
            self.entry("pruned", "a\nb", kind="imports", monotone=True),
        ]
        head = [
            self.entry("same", "a b c"),
            self.entry("edited", "a b d"),
            self.entry("moved", "x y z", path="new.rs"),
            self.entry("elsewhere", "other", moved="P", name="relocated"),
            self.entry("imports", "a\nb\nc", kind="imports", monotone=True),
            self.entry("pruned", "a", kind="imports", monotone=True),
            self.entry("new", "n"),
        ]
        found = {change.what: change.action for change in changes.compare(base, head)}
        self.assertEqual(
            found,
            {
                "`edited`": "modified",
                "`moved`": "relocated",
                "`relocated`": "relocated",
                "`gone`": "removed",
                "`imports`": "extended",
                "`pruned`": "modified",
                "`new`": "added",
            },
        )

    def test_a_removed_entry_pairs_with_a_new_one_of_its_name_or_content(self) -> None:
        base = [
            self.entry("old", "assert eq f 1"),
            self.entry("renamed", "alpha beta gamma delta epsilon zeta"),
        ]
        head = [
            self.entry("other", "assert eq f 2", name="old"),
            self.entry("rename", "alpha beta gamma delta epsilon theta"),
        ]
        found = {
            change.what: (change.action, change.detail)
            for change in changes.compare(base, head)
        }
        self.assertEqual(
            found,
            {
                "`old`": ("modified", "moved"),
                "`renamed`": ("modified", "renamed to `rename`"),
            },
        )


class RustEntryTests(unittest.TestCase):
    def test_an_inline_tests_module_moves_to_its_own_file_unchanged(self) -> None:
        found = rust(
            {"src/a.rs": INLINE},
            {"src/a.rs": DECLARED, "src/a/tests.rs": MOVED},
        )
        self.assertEqual(flagged(found), [])
        self.assertEqual(actions(found), ["relocated"] * 4)

    def test_changes_to_what_tests_do_count(self) -> None:
        for edited, count in (
            (MOVED.replace("#[test]", "#[test]\n#[ignore]"), 1),
            (MOVED.replace("EXPECTED);", "EXPECTED + 0);"), 1),
            (MOVED.replace("const EXPECTED: u8 = 1;", "const EXPECTED: u8 = 2;"), 1),
            (MOVED.replace("use super::*;", "use super::f;"), 1),
            # The attribute, and where each item compiles.
            ("#![cfg(unix)]\n" + MOVED, 4),
        ):
            with self.subTest(edited=edited):
                found = rust(
                    {"src/a.rs": DECLARED, "src/a/tests.rs": MOVED},
                    {"src/a.rs": DECLARED, "src/a/tests.rs": edited},
                )
                self.assertEqual(len(flagged(found)), count)

    def test_formatting_comments_lints_and_visibility_do_not_count(self) -> None:
        edited = (
            MOVED.replace("const EXPECTED", "pub(crate) const EXPECTED")
            .replace("#[test]", "#[test]\n#[allow(clippy::unit_cmp)]")
            .replace(
                "assert_eq!(f(), EXPECTED);",
                "assert_eq!(\n    f(),\n    EXPECTED, // f\n);",
            )
            .replace("use super::*;", "#![allow(dead_code)]\nuse super::*;")
        )
        found = rust(
            {"src/a.rs": DECLARED, "src/a/tests.rs": MOVED},
            {"src/a.rs": DECLARED, "src/a/tests.rs": edited},
        )
        self.assertEqual(found, [])

    def test_commas_count_only_where_they_mean_something(self) -> None:
        def test(body: str) -> str:
            return f"#[test]\nfn t() {{\n    {body}\n}}\n"

        for before, after, counts in (
            ("g(a, b);", "g(\n        a,\n        b,\n    );", False),
            ("let v = [1, 2];", "let v = [1, 2,];", False),
            ("match x { A => { 1 }, B => 2 }", "match x { A => { 1 } B => 2, }", False),
            (
                "match x { A => { f(1) } B => 2 }",
                "match x { A => f(1), B => 2 }",
                False,
            ),
            ("match x { A => { f(1) } }", "match x { A => { let y = 1; f(y) } }", True),
            ("match x { A => f(1), B => 2 }", "match x { A => f(2), B => 2 }", True),
            ("v.map(|x| { x + 1 });", "v.map(|x| x + 1);", False),
            ("v.map(|x| { x + 1 });", "v.map(|x| x + 2);", True),
            ("v.map(|x| -> u8 { x });", "v.map(|x| -> u8 { x + 1 });", True),
            ("let t = (x,);", "let t = (x);", True),
            ("let t = ({ 1 },);", "let t = ({ 1 });", True),
            ("let t = (x, y);", "let t = (x, y,);", False),
            ("return (x,);", "return (x);", True),
            ("let v = [{ 1 }, -2];", "let v = [{ 1 } -2];", True),
            # A macro may read a comma or a brace as it chooses.
            ("check!(1);", "check!(1,);", True),
            ("check!(g(a, b));", "check!(g(a, b,));", True),
            ("check! { a => { 1 } }", "check! { a => 1 }", True),
            (
                "macro_rules! m { ($a:expr) => { $a }; }",
                "macro_rules! m { ($a:expr,) => { $a }; }",
                True,
            ),
            (
                "check!(match x { A => { 1 } B => 2 });",
                "check!(match x { A => 1, B => 2, });",
                False,
            ),
            # The standard library's take expressions and a trailing comma.
            (
                "assert_eq!(g(a, b), c);",
                "assert_eq!(\n        g(a, b,),\n        c,\n    );",
                False,
            ),
            ("let v = vec![1, 2];", "let v = vec![1, 2,];", False),
        ):
            with self.subTest(before=before):
                found = rust(
                    {"src/a/tests.rs": test(before)}, {"src/a/tests.rs": test(after)}
                )
                self.assertEqual(bool(flagged(found)), counts)

    def test_test_helpers_move_into_the_tests_module(self) -> None:
        base = {
            "src/a.rs": "pub struct S {\n    x: u8,\n}\n\nimpl S {\n"
            "    #[cfg(test)]\n    pub(crate) fn x(&self) -> u8 {\n        self.x\n    }\n}\n"
            "\n#[cfg(all(test, unix))]\npub(crate) fn probe() -> u8 {\n    1\n}\n",
        }
        head = {
            "src/a.rs": "pub struct S {\n    x: u8,\n}\n\n#[cfg(test)]\nmod tests;\n",
            "src/a/tests.rs": "use super::*;\n\nimpl S {\n    fn x(&self) -> u8 {\n"
            "        self.x\n    }\n}\n\n#[cfg(unix)]\nfn probe() -> u8 {\n    1\n}\n",
        }
        found = rust(base, head)
        self.assertEqual(flagged(found), [])
        self.assertIn("relocated", actions(found))

    def test_includes_follow_the_file_that_moves(self) -> None:
        inline = INLINE.replace(
            "const EXPECTED: u8 = 1;",
            'const EXPECTED: &str = include_str!("../README.md");',
        )
        moved = MOVED.replace(
            "const EXPECTED: u8 = 1;",
            'const EXPECTED: &str = include_str!("../../README.md");',
        )
        found = rust(
            {"src/a.rs": inline},
            {"src/a.rs": DECLARED, "src/a/tests.rs": moved},
        )
        self.assertEqual(flagged(found), [])
        renamed = moved.replace("../../README.md", "../../CHANGES.md")
        found = rust(
            {"src/a.rs": inline},
            {"src/a.rs": DECLARED, "src/a/tests.rs": renamed},
        )
        self.assertEqual(flagged(found), [(changes.TEST_CODE, "modified", "helper")])

    def test_test_hooks_count_but_production_code_does_not(self) -> None:
        base = (
            "pub fn f(x: u8) -> u8 {\n"
            "    #[cfg(test)]\n"
            "    if let Some(value) = take() {\n"
            "        return value;\n"
            "    }\n"
            "    x + 1\n"
            "}\n"
        )
        hook = base.replace("return value;", "return value + 1;")
        production = base.replace("x + 1", "x + 2")
        self.assertEqual(
            flagged(rust({"src/a.rs": base}, {"src/a.rs": hook})),
            [(changes.TEST_CODE, "modified", "hook")],
        )
        self.assertEqual(rust({"src/a.rs": base}, {"src/a.rs": production}), [])

    def test_hooks_of_conditions_end_with_their_bodies(self) -> None:
        base = (
            "pub fn f() -> u8 {\n"
            "    #[cfg(test)]\n"
            "    if let Some(value) = { take() } {\n"
            "        return value;\n"
            "    }\n"
            "    #[cfg(test)]\n"
            "    for S { a } in { items() } {\n"
            "        record(a);\n"
            "    }\n"
            "    #[cfg(test)]\n"
            "    while let 1..=5 = { next() } {\n"
            "        pause();\n"
            "    }\n"
            "    1\n"
            "}\n"
        )
        for old, new in (
            ("return value;", "return value + 1;"),
            ("record(a);", "record(a + 1);"),
            ("pause();", "pause_more();"),
        ):
            with self.subTest(old=old):
                self.assertEqual(
                    flagged(
                        rust({"src/a.rs": base}, {"src/a.rs": base.replace(old, new)})
                    ),
                    [(changes.TEST_CODE, "modified", "hook")],
                )

    def test_hooks_end_with_their_construct(self) -> None:
        source = (
            "fn f(kind: Kind) -> u8 {\n"
            "    #[cfg(test)]\n"
            "    record!(kind);\n"
            "    #[cfg(test)]\n"
            "    let seen: HashMap<u8, u16> = HashMap::new();\n"
            "    #[cfg(test)]\n"
            "    let add = |a, b| a + b;\n"
            "    match kind {\n"
            "        #[cfg(test)]\n"
            "        Kind::Fake => { 1 }\n"
            "        #[cfg(test)]\n"
            "        Kind::Probe => convert::<u8, u16>(1),\n"
            "        Kind::Real => 2,\n"
            "    }\n"
            "}\n"
            "fn g(a: u8, b: u8) -> S {\n"
            "    S {\n"
            "        #[cfg(test)]\n"
            "        check: |a, b| a < b,\n"
            "        #[cfg(test)]\n"
            "        less: a < b,\n"
            "        more: a > b,\n"
            "    }\n"
            "}\n"
            "struct S {\n"
            "    #[cfg(test)]\n"
            "    seen: u8,\n"
            "    #[cfg(test)]\n"
            "    probe: Result<u8, u16>,\n"
            "    other: u8,\n"
            "}\n"
        )
        entries = changes.rust_entries(
            "demo/src/a.rs", "demo", source, changes.TEST_CODE, False
        )
        hooks = {entry.name: entry.content for entry in entries}
        self.assertEqual(
            hooks,
            {
                "fn f": "# [ cfg ( test ) ] record ! ( kind ) ; | "
                "# [ cfg ( test ) ] let seen : HashMap < u8 , u16 > = HashMap :: new "
                "( ) ; | # [ cfg ( test ) ] let add = | a , b | a + b ; | "
                "# [ cfg ( test ) ] Kind :: Fake => 1 | "
                "# [ cfg ( test ) ] Kind :: Probe => convert :: < u8 , u16 > ( 1 ) ,",
                "fn g": "# [ cfg ( test ) ] check : | a , b | a < b , | "
                "# [ cfg ( test ) ] less : a < b ,",
                "struct S": "# [ cfg ( test ) ] seen : u8 , | "
                "# [ cfg ( test ) ] probe : Result < u8 , u16 > ,",
            },
        )

    def test_operators_keep_their_punctuation_together(self) -> None:
        files = {"src/a.rs": DECLARED}
        base = "#[test]\nfn t() {\n    assert!(ready() && check());\n}\n"
        split = base.replace("ready() && check()", "ready() & &check()")
        self.assertEqual(
            flagged(
                rust(
                    {**files, "src/a/tests.rs": base},
                    {**files, "src/a/tests.rs": split},
                )
            ),
            [(changes.TEST_CODE, "modified", "test")],
        )
        # rustfmt breaks a chain of calls that grows too wide, which changes
        # nothing.
        chained = (
            "#[test]\nfn t() -> Result<(), E> {\n    let v = make()?.value();\n"
            "    Ok(())\n}\n"
        )
        wrapped = chained.replace("make()?.value()", "make()?\n        .value()")
        self.assertEqual(
            flagged(
                rust(
                    {**files, "src/a/tests.rs": chained},
                    {**files, "src/a/tests.rs": wrapped},
                )
            ),
            [],
        )

    def test_commas_and_braces_that_carry_meaning_count(self) -> None:
        files = {"src/a.rs": DECLARED}

        def compare(base: str, head: str) -> list[tuple[str, str, str]]:
            return flagged(
                rust(
                    {**files, "src/a/tests.rs": base}, {**files, "src/a/tests.rs": head}
                )
            )

        # After `->` or `=>`, a comma makes a tuple of one.
        returns = "fn accepts(_: impl Fn() -> (u8,)) {}\n"
        self.assertEqual(
            compare(returns, returns.replace("(u8,)", "(u8)")),
            [(changes.TEST_CODE, "modified", "helper")],
        )
        arm = "fn pick(x: u8) -> (u8,) {\n    match x {\n        _ => (x,),\n    }\n}\n"
        self.assertEqual(
            compare(arm, arm.replace("_ => (x,)", "_ => (x)")),
            [(changes.TEST_CODE, "modified", "helper")],
        )
        # A block after an operator `|` keeps its braces, and a closure's body
        # does not.
        operand = "#[test]\nfn t() {\n    assert!(true | { true == false });\n}\n"
        self.assertEqual(
            compare(operand, operand.replace("{ true == false }", "true == false")),
            [(changes.TEST_CODE, "modified", "test")],
        )
        closure = "#[test]\nfn t() {\n    assert!(v.iter().all(|x| { *x > 0 }));\n}\n"
        self.assertEqual(compare(closure, closure.replace("{ *x > 0 }", "*x > 0")), [])

    def test_only_paths_changed(self) -> None:
        edited = MOVED.replace("assert_eq!(f()", "assert_eq!(super::f()")
        found = rust(
            {"src/a.rs": DECLARED, "src/a/tests.rs": MOVED},
            {"src/a.rs": DECLARED, "src/a/tests.rs": edited},
        )
        self.assertEqual(
            [change.detail for change in found if change.flags],
            ["only the paths that it names changed"],
        )

    def test_proptest_functions_and_platform_twins_are_tests(self) -> None:
        source = (
            "proptest! {\n"
            "    #![proptest_config(ProptestConfig::with_cases(8))]\n"
            "    #[test]\n"
            "    fn holds(x in 0..10u8) {\n"
            "        prop_assert!(x < 10);\n"
            "    }\n"
            "}\n"
            "#[cfg(unix)]\n#[test]\nfn twin() {}\n"
            "#[cfg(windows)]\n#[test]\nfn twin() {}\n"
            "#[cfg_attr(unix, test)]\nfn applied() {}\n"
        )
        entries = changes.rust_entries(
            "demo/src/a/tests.rs", "demo", source, changes.TEST_CODE, True
        )
        self.assertEqual(
            sorted((entry.name, entry.kind) for entry in entries),
            [
                ("", "attributes"),
                ("applied", "test"),
                ("holds", "test"),
                ("proptest", "helper"),
                ("twin", "test"),
                ("twin", "test"),
            ],
        )
        self.assertEqual(len({entry.key for entry in entries}), 6)

    def test_crate_tests_are_modules_of_their_own(self) -> None:
        self.assertEqual(
            changes.rust_module("demo/tests/cli/harness.rs", "demo"),
            ("[tests]", "cli", "harness"),
        )
        self.assertEqual(
            changes.rust_module("demo/src/a/tests.rs", "demo"), ("a", "tests")
        )

    def test_a_move_keeps_where_what_moves_compiles(self) -> None:
        test = (
            "#[test]\nfn works() {\n    // The value that `f` documents.\n"
            "    assert_eq!(f(), EXPECTED);\n}\n"
        )
        wrapped = MOVED.replace(
            test,
            "#[cfg(any())]\nmod moved {\n    use super::*;\n\n"
            "    #[test]\n    fn works() {\n        assert_eq!(f(), EXPECTED);\n"
            "    }\n}\n",
        )
        found = rust(
            {"src/a.rs": DECLARED, "src/a/tests.rs": MOVED},
            {"src/a.rs": DECLARED, "src/a/tests.rs": wrapped},
        )
        self.assertEqual(
            [
                (change.action, change.detail)
                for change in found
                if change.kind == "test"
            ],
            [
                (
                    "modified",
                    "moved, and it now compiles where `any ( )` and `test` "
                    "hold, not where `test` did",
                )
            ],
        )
        base = {
            "src/a.rs": DECLARED,
            "src/a/tests.rs": MOVED,
            "src/b.rs": "pub fn g() {}\n",
        }
        for declaration, flags in (
            ("#[cfg(test)]\nmod tests;\n", False),
            ("#[cfg(all(test, windows))]\nmod tests;\n", True),
        ):
            with self.subTest(declaration=declaration):
                head = {
                    "src/a.rs": "pub fn f() -> u8 {\n    1\n}\n",
                    "src/b.rs": "pub fn g() {}\n\n" + declaration,
                    "src/b/tests.rs": MOVED,
                }
                found = rust(base, head)
                self.assertEqual(
                    (changes.TEST_CODE, "modified", "test") in flagged(found), flags
                )

    def test_modules_inherit_the_conditions_of_their_declarations(self) -> None:
        files = {
            "demo/src/lib.rs": "#[cfg(unix)]\nmod a;\n"
            "#[cfg_attr(windows, cfg_attr(test, cfg(any())))]\nmod c;\n",
            "demo/src/a.rs": '#![cfg(feature = "x")]\nmod inner {\n'
            '    #![cfg(target_os = "linux")]\n    mod b;\n}\n'
            "#[cfg(test)]\nmod tests;\n",
            "demo/src/a/inner/b.rs": "",
            "demo/src/a/tests.rs": "",
            "demo/src/c.rs": "",
            "demo/src/orphan.rs": "#[cfg(test)]\nmod tests;\n",
            "demo/src/orphan/tests.rs": "",
            "demo/tests/cli.rs": '#[cfg(windows)]\n#[path = "cli/harness.rs"]\n'
            "mod harness;\nmod common;\n",
            "demo/tests/cli/harness.rs": "",
            "demo/tests/common/mod.rs": "",
        }
        found = changes.module_conditions(
            files.keys(),
            lambda paths: {path: files[path] for path in paths},
            ("demo",),
            files,
        )
        self.assertEqual(
            found,
            {
                "demo/src/lib.rs": ANYWHERE,
                "demo/src/a.rs": ways({"unix"}),
                "demo/src/a/inner/b.rs": ways(
                    {"unix", 'feature = "x"', 'target_os = "linux"'}
                ),
                "demo/src/a/tests.rs": ways({"unix", 'feature = "x"', "test"}),
                "demo/src/c.rs": ways(
                    {"any ( not ( all ( windows , test ) ) , any ( ) )"}
                ),
                "demo/src/orphan.rs": None,
                "demo/src/orphan/tests.rs": None,
                "demo/tests/cli.rs": ANYWHERE,
                "demo/tests/cli/harness.rs": ways({"windows"}),
                "demo/tests/common/mod.rs": ANYWHERE,
            },
        )

    def test_targets_whose_tests_never_run_reach_no_module(self) -> None:
        files = {
            "demo/Cargo.toml": '[package]\nname = "demo"\n\n[[bin]]\nname = "tool"\n'
            "test = false\n",
            "demo/src/lib.rs": "",
            "demo/src/bin/tool/main.rs": "mod a;\n",
            "demo/src/bin/tool/a.rs": "",
            "demo/benches/speed.rs": "mod b;\n",
            "demo/benches/b.rs": "",
        }
        found = changes.module_conditions(
            files.keys(),
            lambda paths: {path: files[path] for path in paths},
            ("demo",),
            [path for path in files if path.endswith(".rs")],
        )
        self.assertEqual(
            found,
            {
                "demo/src/lib.rs": ANYWHERE,
                "demo/src/bin/tool/main.rs": None,
                "demo/src/bin/tool/a.rs": None,
                "demo/benches/speed.rs": None,
                "demo/benches/b.rs": None,
            },
        )

    def test_modules_reach_through_path_attributes_and_other_targets(self) -> None:
        files = {
            "demo/Cargo.toml": CARGO + '\n[[test]]\nname = "common"\ntest = false\n',
            "demo/src/lib.rs": "",
            "demo/tests/cli.rs": "mod suite;\nmod common;\n",
            "demo/tests/suite/mod.rs": '#[path = "../shared/cases.rs"]\nmod cases;\n',
            "demo/tests/shared/cases.rs": "",
            # A root of a target whose tests never run, which cli.rs compiles.
            "demo/tests/common.rs": "",
        }
        found = changes.module_conditions(
            files.keys(),
            lambda paths: {path: files[path] for path in paths},
            ("demo",),
            ["demo/tests/shared/cases.rs", "demo/tests/common.rs"],
        )
        self.assertEqual(
            found,
            {
                "demo/tests/shared/cases.rs": ANYWHERE,
                "demo/tests/common.rs": ANYWHERE,
            },
        )

    def test_modules_that_inner_attributes_guard_hold_test_code(self) -> None:
        base = (
            "pub fn f() {}\n\nmod checks {\n    #![cfg(test)]\n\n"
            "    #[test]\n    fn works() {\n        assert!(true);\n    }\n}\n"
        )
        edited = base.replace("assert!(true);", "assert!(false);")
        self.assertEqual(
            flagged(rust({"src/a.rs": base}, {"src/a.rs": edited})),
            [(changes.TEST_CODE, "modified", "test")],
        )
        whole = "#![cfg(test)]\n\n#[test]\nfn works() {\n    assert!(true);\n}\n"
        self.assertEqual(
            flagged(
                rust({"src/b.rs": whole}, {"src/b.rs": whole.replace("true", "false")})
            ),
            [(changes.TEST_CODE, "modified", "test")],
        )

    def test_tests_and_helpers_that_no_cfg_test_guards_count(self) -> None:
        body = "\nfn works() {\n    assert!(true);\n}\n"
        for attribute in (
            "#[test]",
            "#[cfg_attr(unix, test)]",
            "#[cfg_attr(all(), cfg(test))]",
        ):
            with self.subTest(attribute=attribute):
                source = f"pub fn f() {{}}\n\n{attribute}{body}"
                edited = source.replace("assert!(true);", "assert!(false);")
                self.assertEqual(
                    len(flagged(rust({"src/a.rs": source}, {"src/a.rs": edited}))), 1
                )

    def test_the_attributes_of_an_import_count(self) -> None:
        base = MOVED.replace(
            "use super::*;\n",
            "use super::*;\n#[cfg(unix)]\nuse std::os::unix::fs::PermissionsExt;\n",
        )
        for edited, expected in (
            (base.replace("#[cfg(unix)]", "#[cfg(windows)]"), ["modified"]),
            (
                base.replace("#[cfg(unix)]", "#[allow(unused_imports)]\n#[cfg(unix)]"),
                [],
            ),
            (base.replace("#[cfg(unix)]\n", ""), ["modified"]),
        ):
            with self.subTest(edited=edited):
                found = rust(
                    {"src/a.rs": DECLARED, "src/a/tests.rs": base},
                    {"src/a.rs": DECLARED, "src/a/tests.rs": edited},
                )
                self.assertEqual(
                    [change.action for change in found if change.flags], expected
                )


class PythonEntryTests(unittest.TestCase):
    BASE = '''"""Tests."""

import unittest
from pathlib import Path


class ATests(unittest.TestCase):
    def test_a(self):
        """Checks a."""
        self.assertEqual(1, 1)

    def helper(self):
        return Path(".")


if __name__ == "__main__":
    unittest.main()
'''

    def test_methods_imports_and_moves(self) -> None:
        path = "scripts/test_a.py"
        docstring = self.BASE.replace("Checks a.", "Checks that a holds.")
        self.assertEqual(python({path: self.BASE}, {path: docstring}), [])
        edited = self.BASE.replace("assertEqual(1, 1)", "assertTrue(True)")
        self.assertEqual(
            flagged(python({path: self.BASE}, {path: edited})),
            [(changes.TEST_CODE, "modified", "test")],
        )
        extended = self.BASE.replace(
            "import unittest\n", "import json\nimport unittest\n"
        )
        self.assertEqual(
            actions(python({path: self.BASE}, {path: extended})), ["extended"]
        )
        reduced = self.BASE.replace("from pathlib import Path\n", "")
        self.assertEqual(
            flagged(python({path: self.BASE}, {path: reduced})),
            [(changes.TEST_CODE, "modified", "imports")],
        )
        moved = self.BASE.replace("class ATests", "class BTests")
        found = python({path: self.BASE}, {"scripts/test_b.py": moved})
        self.assertEqual(flagged(found), [(changes.TEST_CODE, "removed", "helper")])
        self.assertEqual(actions(found).count("relocated"), 4)


class WiringTests(unittest.TestCase):
    def test_changed_lines_of_test_actions(self) -> None:
        base = (
            "steps:\n  - name: Test\n    run: |\n"
            "      cargo test --locked\n      cargo test --lib\n# note\n"
        )
        for head, expected in (
            (base.replace("# note", "# other note"), []),
            (base.replace("      cargo test --lib\n", ""), ["removed"]),
            (base.replace("--locked", "--locked --exact"), ["modified"]),
            (base + "      cargo test --doc\n", []),
            (
                base.replace("    run: |", "    if: false\n    run: |"),
                ["added a line that may skip"],
            ),
            (base + "    continue-on-error: true\n", ["added a line that may skip"]),
            # A conditional that wraps a test command keeps the command's line.
            (
                base.replace(
                    "      cargo test --locked\n",
                    "      if false; then\n      cargo test --locked\n      fi\n",
                ),
                ["added a line that may skip", "added a line that may skip"],
            ),
            (
                base.replace(
                    "      cargo test --lib\n",
                    "      If ($false) {\n      cargo test --lib\n      }\n",
                ),
                ["added a line that may skip"],
            ),
        ):
            with self.subTest(head=head):
                found = changes.line_changes("action.yml", base, head)
                self.assertEqual([change.action for change in found], expected)
        nested = base.replace("    run: |", "    env:\n      if: 'false'\n    run: |")
        dedented = nested.replace("      if: 'false'", "    if: 'false'")
        self.assertEqual(
            [
                change.action
                for change in changes.line_changes("action.yml", nested, dedented)
            ],
            ["modified", "added a line that may skip"],
        )
        self.assertEqual(changes.line_changes("action.yml", None, base), [])
        self.assertEqual(
            [
                change.action
                for change in changes.line_changes("action.yml", base, None)
            ],
            ["removed"],
        )

    def test_tokens_that_ci_names(self) -> None:
        base = (
            "      - uses: ./.github/actions/validate-nvx\n"
            "        run: >-\n"
            "          python3 scripts/nvx.py test-microvm --scenario guest-boot\n"
            '          cargo "+${{ steps.t.outputs.toolchain }}" test --locked \\\n'
            "            --all-features\n"
            "          python -m unittest scripts\\test_a.py\n"
        )
        tokens = changes.wiring_tokens(base)
        self.assertEqual(
            sorted(tokens),
            [
                "0:./.github/actions/validate-nvx",
                "1:test-microvm",
                "2:guest-boot",
                "3:test_a.py",
                "4:cargo test --locked --all-features",
            ],
        )
        self.assertEqual(tokens["4:cargo test --locked --all-features"], [4])
        head = base.replace("--scenario guest-boot", "").replace(
            "    --all-features", "    --no-default-features"
        )
        self.assertEqual(
            [change.what for change in changes.token_changes("ci.yml", base, head)],
            ["`guest-boot`", "`cargo test --locked --all-features`"],
        )

    def test_a_token_counts_each_time_that_ci_names_it(self) -> None:
        job = "      - uses: ./.github/actions/validate-nvx\n"
        both = f"linux:\n{job}windows:\n{job}"
        self.assertEqual(
            changes.wiring_tokens(both)["0:./.github/actions/validate-nvx"], [2, 4]
        )
        found = changes.token_changes("ci.yml", both, f"linux:\n{job}")
        self.assertEqual(
            [
                (change.action, cast(changes.Location, change.base).line)
                for change in found
            ],
            [("removed", 4)],
        )
        commented = f"linux:\n{job}windows:\n      # {job.strip()}\n"
        self.assertEqual(
            [
                change.action
                for change in changes.token_changes("ci.yml", both, commented)
            ],
            ["removed"],
        )

    def test_jobs_that_run_tests(self) -> None:
        base = (
            "on: push\n"
            "jobs:\n"
            "  build:\n"
            "    runs-on: ubuntu-latest\n"
            "    steps:\n"
            "      - run: cargo build\n"
            "  test:\n"
            "    runs-on: ${{ matrix.os }}\n"
            "    strategy:\n"
            "      matrix:\n"
            "        os: [ubuntu-latest, windows-latest]\n"
            "    steps:\n"
            "      - name: Prepare\n"
            "        run: cargo fetch\n"
            "      - uses: ./.github/actions/validate-nvx\n"
            "      - run: cargo test --locked  # every crate\n"
        )
        uses = "      - uses: ./.github/actions/validate-nvx\n"
        for head, expected in (
            (base.replace("cargo build", "cargo build --release"), []),
            (base.replace("cargo fetch", "cargo fetch --locked"), []),
            (base.replace("# every crate", "# all crates"), []),
            (base.replace(uses, f"      # {uses.strip()}\n"), ["removed", "removed"]),
            (
                base.replace(
                    "      - run: cargo test",
                    "      - if: false\n        run: cargo test",
                ),
                ["added a line that may skip", "modified"],
            ),
            (base.replace(", windows-latest", ""), ["modified"]),
            (
                base.replace(
                    "    strategy:", "    continue-on-error: true\n    strategy:"
                ),
                ["added a line that may skip"],
            ),
            (
                base + "        continue-on-error: true\n",
                ["added a line that may skip"],
            ),
            (base.replace("  test:\n", "  tests:\n"), ["removed"]),
            (base.replace(uses, uses + "      - run: cargo test --doc\n"), []),
        ):
            with self.subTest(head=head):
                found = changes.token_changes(
                    "ci.yml", base, head
                ) + changes.job_changes("ci.yml", base, head)
                self.assertEqual(sorted(change.action for change in found), expected)
        nested = base.replace(
            "      - run: cargo test --locked  # every crate\n",
            "      - run: cargo test --locked\n        env:\n          if: 'false'\n",
        )
        dedented = nested.replace("          if: 'false'", "        if: 'false'")
        self.assertEqual(
            sorted(
                change.action
                for change in changes.job_changes("ci.yml", nested, dedented)
            ),
            ["added a line that may skip", "modified"],
        )
        jobs = changes.workflow_jobs(base)
        self.assertEqual(
            {
                name: (
                    job.line,
                    [text for _, text in job.keys],
                    [[text for _, text in step] for step in job.steps],
                    job.runs_tests(),
                )
                for name, job in jobs.items()
            },
            {
                "build": (
                    3,
                    ["    runs-on: ubuntu-latest"],
                    [["      - run: cargo build"]],
                    False,
                ),
                "test": (
                    7,
                    [
                        "    runs-on: ${{ matrix.os }}",
                        "    strategy:",
                        "      matrix:",
                        "        os: [ubuntu-latest, windows-latest]",
                    ],
                    [
                        ["      - name: Prepare", "        run: cargo fetch"],
                        ["      - uses: ./.github/actions/validate-nvx"],
                        ["      - run: cargo test --locked"],
                    ],
                    True,
                ),
            },
        )
        self.assertEqual(changes.job_changes("ci.yml", None, base), [])

    def test_scenarios_that_leave_the_shards(self) -> None:
        def shards(*scenarios: str) -> str:
            return json.dumps(
                {"backends": {"kvm": {"shards": [{"scenarios": list(scenarios)}]}}}
            )

        self.assertEqual(
            changes.shard_changes(shards("a", "b"), shards("b", "a", "c")), []
        )
        self.assertEqual(
            [
                change.what
                for change in changes.shard_changes(shards("a", "b"), shards("a"))
            ],
            ["`b` on kvm"],
        )
        self.assertEqual(
            [change.action for change in changes.shard_changes(shards("a"), "{")],
            ["modified"],
        )

    def test_allowlist_entries_that_allow_more(self) -> None:
        def allowlist(seams: Mapping[str, int]) -> str:
            return layout.render_allowlist(layout.Allowlist(dict(seams)))

        base = allowlist({"a.rs": 2, "b.rs": 1})
        self.assertEqual(changes.allowlist_changes(base, allowlist({"a.rs": 1})), [])
        raised = allowlist({"a.rs": 3, "b.rs": 1, "c.rs": 1})
        self.assertEqual(
            [change.detail for change in changes.allowlist_changes(base, raised)],
            ["the seams allow 3 test hooks, up from 2", "the seams allow 1 test hooks"],
        )
        self.assertEqual(
            [change.action for change in changes.allowlist_changes(base, "[]")],
            ["modified"],
        )
        self.assertEqual(changes.allowlist_changes(None, raised), [])

    def test_allowlists_of_the_first_schema_version(self) -> None:
        # Merge bases from before the baseline went hold the first version.
        def first(
            baseline: Mapping[str, tuple[int, int]], seams: Mapping[str, int]
        ) -> str:
            document = {
                "schema": layout.ALLOWLIST_SCHEMA,
                "schema_version": 1,
                "baseline": {
                    path: {"hooks": hooks, "tests": tests}
                    for path, (hooks, tests) in sorted(baseline.items())
                },
                "seams": dict(sorted(seams.items())),
            }
            return json.dumps(document, indent=2) + "\n"

        base = first({"a.rs": (2, 3), "b.rs": (1, 1)}, {"c.rs": 2})
        lowered = first({"a.rs": (2, 1)}, {"b.rs": 1, "c.rs": 1})
        self.assertEqual(changes.allowlist_changes(base, lowered), [])
        raised = first({"a.rs": (2, 4), "b.rs": (1, 1)}, {"c.rs": 2, "d.rs": 1})
        self.assertEqual(
            [change.what for change in changes.allowlist_changes(base, raised)],
            ["`a.rs`", "`d.rs`"],
        )
        # Fewer hooks do not pay for more tests, and a file of the seams that
        # moves to the baseline may hold test code again.
        traded = first({"a.rs": (1, 4), "b.rs": (1, 1)}, {"c.rs": 2})
        self.assertEqual(
            [change.what for change in changes.allowlist_changes(base, traded)],
            ["`a.rs`"],
        )
        back = first({"a.rs": (2, 3), "b.rs": (1, 1), "c.rs": (1, 0)}, {})
        self.assertEqual(
            [change.detail for change in changes.allowlist_changes(base, back)],
            ["moves from the seams to the baseline, which allows tests"],
        )

        # The seams of the current version allow a baseline file its hooks.
        def current(seams: Mapping[str, int]) -> str:
            return layout.render_allowlist(layout.Allowlist(dict(seams)))

        self.assertEqual(
            changes.allowlist_changes(base, current({"a.rs": 2, "c.rs": 2})), []
        )
        self.assertEqual(
            [
                change.detail
                for change in changes.allowlist_changes(
                    base, current({"a.rs": 3, "c.rs": 2})
                )
            ],
            ["the seams allow 3 test hooks, up from 2"],
        )
        renamed = base.replace('"tests"', '"cases"')
        self.assertEqual(
            [change.action for change in changes.allowlist_changes(base, renamed)],
            ["modified"],
        )


class ClassifyTests(unittest.TestCase):
    def test_paths_and_their_categories(self) -> None:
        crates = ("nvx-rs",)
        for path, expected in (
            ("scripts/test-layout-allowlist.json", (changes.TEST_POLICY, "allowlist")),
            ("scripts/check_test_layout.py", (changes.TEST_POLICY, "file")),
            (".github/actions/validate-nvx/action.yml", (changes.CI_WIRING, "lines")),
            (".github/workflows/ci.yml", (changes.CI_WIRING, "tokens")),
            ("scripts/nvx_tools/microvm_shards.json", (changes.CI_WIRING, "shards")),
            ("nvx-rs/src/testing/fake_openvmm.rs", (changes.TEST_DOUBLES, "rust")),
            ("nvx-rs/src/bin/nvx-fake-openvmm.rs", (changes.TEST_DOUBLES, "rust")),
            (
                "scripts/nvx_tools/microvm_test_scripts/a.sh",
                (changes.END_TO_END, "file"),
            ),
            ("guest/common/nvx-sandbox-smoke", (changes.END_TO_END, "file")),
            ("scripts/nvx_tools/microvm_tests.py", (changes.END_TO_END, "python")),
            ("scripts/test_nvx_tools.py", (changes.TEST_CODE, "python")),
            ("scripts/powershell_testing.py", (changes.TEST_CODE, "python")),
            ("nvx-rs/tests/fixtures/store/runtime.json", (changes.TEST_DATA, "file")),
            ("nvx-rs/tests/vectors/openvmm/a.rs", (changes.TEST_DATA, "file")),
            ("nvx-rs/tests/cli.rs", (changes.TEST_CODE, "rust")),
            ("nvx-rs/src/a/tests.rs", (changes.TEST_CODE, "rust")),
            ("nvx-rs/src/a.rs", (changes.TEST_CODE, "rust-production")),
            ("nvx-rs/tests/vectors/README.md", None),
            ("nvx-rs/README.md", None),
            ("scripts/nvx_tools/benchmark.py", None),
            ("doc/ci.md", None),
        ):
            with self.subTest(path=path):
                self.assertEqual(changes.classify(path, crates), expected)


class Repository:
    """A Git repository in a temporary directory."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.git("init", "-q", "-b", "dev")

    def git(self, *arguments: str) -> str:
        return subprocess.run(
            [
                "git",
                "-c",
                "core.autocrlf=false",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                *arguments,
            ],
            cwd=self.root,
            check=True,
            capture_output=True,
            text=True,
        ).stdout

    def commit(self, files: Mapping[str, str | None]) -> str:
        for path, text in files.items():
            target = self.root / path
            if text is None:
                target.unlink()
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text, encoding="utf-8", newline="\n")
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", "change")
        return self.git("rev-parse", "HEAD").strip()


class CommitTests(unittest.TestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.repository = Repository(Path(directory.name))
        self.base = self.repository.commit(
            {
                "demo/Cargo.toml": CARGO,
                "demo/src/lib.rs": "pub mod a;\n",
                "demo/src/a.rs": INLINE,
                "demo/tests/fixtures/expected.json": '{"value": 1}\n',
                "demo/tests/fixtures/old.json": "{}\n",
                "scripts/nvx_tools/microvm_test_scripts/boot.sh": "echo boot\n",
                "scripts/test_tool.py": PythonEntryTests.BASE,
                "notes/guide.md": "# Guide\n",
            }
        )

    def compare(self, head: str) -> changes.Comparison:
        return changes.compare_commits(self.repository.root, self.base, head)

    def test_a_declaration_that_changes_where_its_module_compiles_counts(self) -> None:
        unchanged = self.repository.commit(
            {"demo/src/lib.rs": "pub mod a;\npub mod b;\n"}
        )
        self.assertEqual(self.compare(unchanged).changes, [])
        head = self.repository.commit(
            {"demo/src/lib.rs": "#[cfg(windows)]\npub mod a;\n"}
        )
        self.assertIn(
            (
                "test",
                "it now compiles where `test` and `windows` hold, not where `test` did",
            ),
            [(change.kind, change.detail) for change in self.compare(head).flagged],
        )
        head = self.repository.commit(
            {"demo/src/lib.rs": "#[cfg_attr(unix, cfg(any()))]\npub mod a;\n"}
        )
        self.assertIn(
            (
                "test",
                "it now compiles where `any ( not ( unix ) , any ( ) )` and `test` "
                "hold, not where `test` did",
            ),
            [(change.kind, change.detail) for change in self.compare(head).flagged],
        )
        # A module that no longer compiles takes its tests with it.
        head = self.repository.commit({"demo/src/lib.rs": "pub mod b;\n"})
        self.assertIn(
            ("removed", "test", "`fn works` in `a::tests`"),
            [
                (change.action, change.kind, change.what)
                for change in self.compare(head).flagged
            ],
        )

    def test_every_target_that_compiles_a_module_counts(self) -> None:
        both = self.repository.commit(
            {
                "demo/src/lib.rs": "#[cfg(windows)]\nmod a;\n",
                "demo/src/main.rs": "#[cfg(unix)]\nmod a;\n\nfn main() {}\n",
            }
        )
        # The binary compiles the module still, but no longer where the library
        # did.
        binary = self.repository.commit({"demo/src/lib.rs": ""})
        comparison = changes.compare_commits(self.repository.root, both, binary)
        self.assertIn(
            (
                "test",
                "it now compiles where `test` and `unix` hold, not where "
                "(`test` and `unix`) or (`test` and `windows`) did",
            ),
            [(change.kind, change.detail) for change in comparison.flagged],
        )

    def test_a_declaration_in_another_directory_counts(self) -> None:
        cases = "#[test]\nfn case() {}\n"
        reached = self.repository.commit(
            {
                "demo/tests/cli.rs": "mod suite;\n",
                "demo/tests/suite/mod.rs": '#[path = "../shared/cases.rs"]\nmod cases;\n',
                "demo/tests/shared/cases.rs": cases,
            }
        )
        # Only the declarer changes, in a directory apart from its module's.
        guarded = self.repository.commit(
            {
                "demo/tests/suite/mod.rs": '#[cfg(windows)]\n#[path = "../shared/cases.rs"]\n'
                "mod cases;\n"
            }
        )
        comparison = changes.compare_commits(self.repository.root, reached, guarded)
        self.assertIn(
            ("modified", "test", "it now compiles only where `windows` hold"),
            [
                (change.action, change.kind, change.detail)
                for change in comparison.flagged
            ],
        )
        dropped = self.repository.commit({"demo/tests/suite/mod.rs": ""})
        comparison = changes.compare_commits(self.repository.root, reached, dropped)
        self.assertIn(
            ("removed", "test", "demo/tests/shared/cases.rs"),
            [
                (change.action, change.kind, cast(changes.Location, change.base).path)
                for change in comparison.flagged
            ],
        )

    def test_a_manifest_that_stops_testing_a_target_removes_its_tests(self) -> None:
        unchanged = self.repository.commit(
            {"demo/Cargo.toml": CARGO + '\n[dependencies]\nlibc = "0.2"\n'}
        )
        self.assertEqual(self.compare(unchanged).changes, [])
        head = self.repository.commit(
            {"demo/Cargo.toml": CARGO + "\n[lib]\ntest = false\n"}
        )
        self.assertIn(
            ("removed", "test", "`fn works` in `a::tests`"),
            [
                (change.action, change.kind, change.what)
                for change in self.compare(head).flagged
            ],
        )
        restored = self.repository.commit({"demo/Cargo.toml": CARGO})
        comparison = changes.compare_commits(self.repository.root, head, restored)
        self.assertEqual(comparison.flagged, [])
        self.assertIn(
            ("added", "test"),
            [(change.action, change.kind) for change in comparison.changes],
        )
        # A library that the manifest roots elsewhere no longer compiles the
        # modules of src/lib.rs, which stays.
        moved = self.repository.commit(
            {
                "demo/Cargo.toml": CARGO + '\n[lib]\npath = "src/core.rs"\n',
                "demo/src/core.rs": "",
            }
        )
        self.assertIn(
            ("removed", "test", "`fn works` in `a::tests`"),
            [
                (change.action, change.kind, change.what)
                for change in changes.compare_commits(
                    self.repository.root, restored, moved
                ).flagged
            ],
        )

    def test_moves_and_new_tests_flag_nothing(self) -> None:
        head = self.repository.commit(
            {
                "demo/src/a.rs": DECLARED,
                "demo/src/a/tests.rs": MOVED + "\n#[test]\nfn more() {}\n",
                "demo/tests/fixtures/old.json": None,
                "demo/tests/fixtures/renamed.json": "{}\n",
                "notes/guide.md": "# Guide\n\nMore.\n",
            }
        )
        comparison = self.compare(head)
        self.assertEqual(comparison.flagged, [])
        self.assertEqual(comparison.count("relocated"), 5)
        self.assertEqual(comparison.count("added"), 1)
        self.assertIn("changes no existing test", changes.render(comparison, None))

    def test_changes_to_existing_tests_flag(self) -> None:
        head = self.repository.commit(
            {
                "demo/src/a.rs": INLINE.replace("EXPECTED);", "EXPECTED + 0);"),
                "demo/tests/fixtures/expected.json": '{"value": 2}\n',
                "scripts/nvx_tools/microvm_test_scripts/boot.sh": None,
                "scripts/test_tool.py": PythonEntryTests.BASE.replace(
                    "(1, 1)", "(1, 2)"
                ),
            }
        )
        comparison = self.compare(head)
        self.assertEqual(
            flagged(comparison.flagged),
            [
                (changes.END_TO_END, "removed", "file"),
                (changes.TEST_CODE, "modified", "test"),
                (changes.TEST_CODE, "modified", "test"),
                (changes.TEST_DATA, "modified", "file"),
            ],
        )
        report = changes.render(comparison, "owner/repo")
        self.assertTrue(report.startswith(changes.COMMENT_MARKER))
        self.assertIn("modifies or removes 4 existing test asset(s)", report)
        self.assertIn(
            f"(https://github.com/owner/repo/blob/{comparison.head}/demo/src/a.rs#L",
            report,
        )
        self.assertIn("#### Test data", report)

    def test_the_merge_base_counts(self) -> None:
        topic = self.repository.commit({"demo/tests/fixtures/new.json": "{}\n"})
        self.repository.git("checkout", "-q", "-b", "other", self.base)
        moved_on = self.repository.commit(
            {"demo/tests/fixtures/expected.json": '{"value": 3}\n'}
        )
        comparison = changes.compare_commits(self.repository.root, moved_on, topic)
        self.assertEqual(comparison.base, self.base)
        self.assertEqual(comparison.flagged, [])

    def test_modules_that_only_tests_compile_are_test_code(self) -> None:
        support = "pub(crate) fn fixture() -> u8 {\n    1\n}\n"
        base = self.repository.commit(
            {
                "demo/src/lib.rs": "pub mod a;\n#[cfg(test)]\nmod support;\n",
                "demo/src/support.rs": support,
                "demo/src/a.rs": "mod inner {\n    #[cfg(test)]\n    mod model;\n}\n",
                "demo/src/a/inner/model.rs": support,
            }
        )
        head = self.repository.commit(
            {
                "demo/src/support.rs": support.replace("1", "2"),
                "demo/src/a/inner/model.rs": support.replace("1", "3"),
            }
        )
        comparison = changes.compare_commits(self.repository.root, base, head)
        self.assertEqual(
            sorted(
                (change.action, change.kind, cast(changes.Location, change.base).path)
                for change in comparison.flagged
            ),
            [
                ("modified", "helper", "demo/src/a/inner/model.rs"),
                ("modified", "helper", "demo/src/support.rs"),
            ],
        )
        # So does a module that a root that the manifest names declares.
        rooted = self.repository.commit(
            {
                "demo/Cargo.toml": CARGO + '\n[lib]\npath = "src/core.rs"\n',
                "demo/src/core.rs": "pub mod a;\n#[cfg(test)]\nmod helpers;\n",
                "demo/src/helpers.rs": support,
            }
        )
        edited = self.repository.commit(
            {"demo/src/helpers.rs": support.replace("1", "4")}
        )
        comparison = changes.compare_commits(self.repository.root, rooted, edited)
        self.assertEqual(
            [
                (change.action, change.kind, cast(changes.Location, change.base).path)
                for change in comparison.flagged
            ],
            [("modified", "helper", "demo/src/helpers.rs")],
        )

    def test_main_reports_and_writes_outputs(self) -> None:
        head = self.repository.commit(
            {"demo/tests/fixtures/expected.json": '{"value": 2}\n'}
        )
        root = self.repository.root
        report, output = root / "report.md", root / "output.txt"
        printed = io.StringIO()
        with contextlib.redirect_stdout(printed):
            status = changes.main(
                [
                    "--root",
                    str(root),
                    "--base",
                    self.base,
                    "--head",
                    head,
                    "--report",
                    str(report),
                    "--github-output",
                    str(output),
                ]
            )
        self.assertEqual(status, 0)
        self.assertIn(
            "test data: modified file demo/tests/fixtures/expected.json",
            printed.getvalue(),
        )
        self.assertEqual(output.read_text(encoding="utf-8"), "flagged=true\ncount=1\n")
        self.assertTrue(
            report.read_text(encoding="utf-8").startswith(changes.COMMENT_MARKER)
        )
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(
                changes.main(["--root", str(root), "--base", "missing"]), 2
            )
            with self.assertRaises(SystemExit):
                changes.main(["--root", str(root), "--publish", "1"])


class RenderTests(unittest.TestCase):
    def test_comments_fit_and_mention_no_one(self) -> None:
        comparison = changes.Comparison("a" * 40, "b" * 40)
        comparison.changes = [
            changes.Change(
                changes.TEST_CODE,
                "removed",
                "test",
                f"`fn test_{index}` of @someone | x",
                changes.Location("src/a.rs", index + 1),
            )
            for index in range(3000)
        ]
        body = changes.comment_body(comparison, "owner/repo")
        self.assertLessEqual(len(body), changes.COMMENT_LIMIT)
        self.assertIn("more, which the job's summary lists", body)
        self.assertNotIn("@someone", body)
        self.assertIn("\\|", body)


class FakeGh:
    """Answers gh's calls from a script of responses."""

    def __init__(
        self, responses: Mapping[tuple[str, str], tuple[int, str, str]]
    ) -> None:
        self.responses = responses
        self.calls: list[tuple[str, str, str | None]] = []

    def __call__(
        self, arguments: Sequence[str], stdin: str | None
    ) -> subprocess.CompletedProcess[str]:
        method = arguments[arguments.index("--method") + 1]
        endpoint = next(
            argument for argument in arguments[2:] if argument.startswith("repos/")
        )
        self.calls.append((method, endpoint, stdin))
        status, stdout, stderr = self.responses.get((method, endpoint), (0, "", ""))
        return subprocess.CompletedProcess(list(arguments), status, stdout, stderr)


class PublishTests(unittest.TestCase):
    LABEL = "repos/o/r/labels/tests-modified"
    ISSUE_LABELS = "repos/o/r/issues/7/labels"
    COMMENTS = "repos/o/r/issues/7/comments"

    def publish(
        self,
        flagged: bool,
        responses: Mapping[tuple[str, str], tuple[int, str, str]],
    ) -> list[tuple[str, str, str | None]]:
        gh = FakeGh(responses)
        changes.GitHub("o/r", gh).publish(7, "body", flagged)
        return gh.calls

    def test_a_flagged_pull_request_gets_the_label_and_a_comment(self) -> None:
        calls = self.publish(
            flagged=True, responses={("GET", self.LABEL): (1, "", "HTTP 404")}
        )
        self.assertEqual(
            [(method, endpoint) for method, endpoint, _ in calls],
            [
                ("GET", self.LABEL),
                ("POST", "repos/o/r/labels"),
                ("POST", self.ISSUE_LABELS),
                ("GET", self.COMMENTS),
                ("POST", self.COMMENTS),
            ],
        )
        self.assertEqual(
            json.loads(cast(str, calls[2][2])), {"labels": ["tests-modified"]}
        )
        self.assertEqual(json.loads(cast(str, calls[4][2])), {"body": "body"})

    def test_the_comment_is_updated_and_the_label_removed(self) -> None:
        calls = self.publish(
            flagged=False,
            responses={
                ("DELETE", f"{self.ISSUE_LABELS}/tests-modified"): (1, "", "HTTP 404"),
                ("GET", self.COMMENTS): (0, "11\n12\n", ""),
            },
        )
        self.assertEqual(
            [(method, endpoint) for method, endpoint, _ in calls],
            [
                ("DELETE", f"{self.ISSUE_LABELS}/tests-modified"),
                ("GET", self.COMMENTS),
                ("PATCH", "repos/o/r/issues/comments/11"),
            ],
        )
        self.assertEqual(
            [
                endpoint
                for method, endpoint, _ in self.publish(False, {})
                if method != "GET"
            ],
            [f"{self.ISSUE_LABELS}/tests-modified"],
        )

    def test_failures_raise(self) -> None:
        with self.assertRaises(changes.FlagError):
            self.publish(True, {("POST", self.ISSUE_LABELS): (1, "", "HTTP 403")})

    def test_a_label_that_another_run_created_first_is_no_failure(self) -> None:
        calls = self.publish(
            True,
            {
                ("GET", self.LABEL): (1, "", "HTTP 404"),
                ("POST", "repos/o/r/labels"): (1, "", "HTTP 422: already_exists"),
            },
        )
        self.assertIn(("POST", self.ISSUE_LABELS), [call[:2] for call in calls])


class WorkflowTests(unittest.TestCase):
    def setUp(self) -> None:
        self.workflow = cast(
            dict[object, object], yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
        )

    def test_it_flags_pull_requests_without_running_their_code(self) -> None:
        # YAML 1.1 reads the `on` key as True.
        triggers = cast(dict[str, object], self.workflow[True])
        self.assertEqual(list(triggers), ["pull_request_target"])
        self.assertEqual(self.workflow["permissions"], {})
        jobs = cast(dict[str, dict[str, object]], self.workflow["jobs"])
        [job] = jobs.values()
        self.assertEqual(
            job["permissions"], {"contents": "read", "pull-requests": "write"}
        )
        steps = cast(list[dict[str, object]], job["steps"])
        checkouts = [
            step
            for step in steps
            if str(step.get("uses", "")).startswith("actions/checkout@")
        ]
        self.assertEqual(len(checkouts), 1)
        options = cast(dict[str, object], checkouts[0]["with"])
        # The default branch's script runs, never the pull request's.
        self.assertEqual(
            options["ref"], "${{ github.event.repository.default_branch }}"
        )
        self.assertNotIn("repository", options)
        self.assertIs(options["persist-credentials"], False)
        # A skipped job, as an edit of the title skips, holds no concurrency
        # group, so it cancels no comparison.
        self.assertNotIn("concurrency", self.workflow)
        self.assertEqual(
            job["concurrency"],
            {
                "group": "test-changes-${{ github.event.pull_request.number }}",
                "cancel-in-progress": True,
            },
        )
        scripts = [str(step["run"]) for step in steps if "run" in step]
        self.assertTrue(scripts)
        # The run flags the head of its event only while it is the pull
        # request's head.
        self.assertIn(
            '"$(git rev-parse refs/remotes/pull/head)" == "${HEAD_SHA}"', scripts[0]
        )
        for script in scripts:
            self.assertNotIn("${{", script)
            for forbidden in ("pip ", "cargo ", "npm ", "make ", "bash ./", "sh ./"):
                self.assertNotIn(forbidden, script)
        self.assertIn("scripts/check_test_changes.py", "\n".join(scripts))
        self.assertIn("--publish", "\n".join(scripts))


if __name__ == "__main__":
    unittest.main()

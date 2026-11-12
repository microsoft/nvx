"""Tests of public_api.py, which checks the public API of nvx against snapshots.

The tests walk small rustdoc JSON documents, shaped as the pinned Rust release
writes them, and check the snapshots against listings, without building any.
"""

from __future__ import annotations

import contextlib
import io
import sys
import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import public_api  # noqa: E402
from nvx_tools.common import ScriptError  # noqa: E402

# How rustdoc's JSON gives the cfg of an item that a feature gates.
GATED = {
    "other": '#[attr = CfgTrace([NameValue { name: "feature", value: Some("cli"), '
    "span: src/lib.rs:2:7: 2:22 (#0) }])]"
}


class Crate:
    """The rustdoc JSON of a crate, with what the walk reads of each item."""

    def __init__(self) -> None:
        self.index: dict[str, Any] = {}

    def add(
        self,
        kind: str,
        name: str | None = None,
        inner: dict[str, Any] | None = None,
        *,
        hidden: bool = False,
        attrs: Sequence[object] = (),
    ) -> int:
        item_id = len(self.index)
        self.index[str(item_id)] = {
            "id": item_id,
            "name": name,
            "attrs": [*attrs, *([public_api.HIDDEN] if hidden else [])],
            "inner": {kind: inner or {}},
        }
        return item_id

    def module(
        self,
        name: str,
        *items: int,
        hidden: bool = False,
        attrs: Sequence[object] = (),
    ) -> int:
        inner = {"is_crate": False, "items": list(items), "is_stripped": False}
        return self.add("module", name, inner, hidden=hidden, attrs=attrs)

    def use(
        self,
        source: str,
        target: int | None,
        name: str | None = None,
        *,
        glob: bool = False,
        hidden: bool = False,
    ) -> int:
        inner = {
            "source": source,
            "name": name or source.rsplit("::", 1)[-1],
            "id": target,
            "is_glob": glob,
        }
        return self.add("use", None, inner, hidden=hidden)

    def struct(
        self, name: str, shape: object = "unit", impls: Sequence[int] = ()
    ) -> int:
        return self.add("struct", name, {"kind": shape, "impls": list(impls)})

    def impl(self, *items: int, trait: object = None, hidden: bool = False) -> int:
        inner = {"trait": trait, "items": list(items), "is_synthetic": False}
        return self.add("impl", None, inner, hidden=hidden)

    def document(
        self, root: int, version: int = public_api.FORMAT_VERSION
    ) -> dict[str, Any]:
        return {"format_version": version, "root": root, "index": self.index}


class ListingTests(unittest.TestCase):
    def test_lists_each_kind_of_item_by_its_path(self) -> None:
        crate = Crate()
        clone = crate.add("function", "clone")
        plain = crate.struct(
            "Plain",
            {"plain": {"fields": [crate.add("struct_field", "count")]}},
            [
                crate.impl(
                    crate.add("function", "new"), crate.add("assoc_const", "MAX")
                ),
                crate.impl(clone, trait={"path": "Clone", "args": None}),
            ],
        )
        # The second field of the tuple struct is private.
        pair = crate.struct("Pair", {"tuple": [crate.add("struct_field", "0"), None]})
        empty = crate.add("variant", "Empty", {"kind": "plain"})
        one_field = crate.add("struct_field", "0")
        one = crate.add("variant", "One", {"kind": {"tuple": [one_field]}})
        code = crate.add("struct_field", "code")
        named = crate.add("variant", "Named", {"kind": {"struct": {"fields": [code]}}})
        shape = crate.add(
            "enum", "Shape", {"variants": [empty, one, named], "impls": []}
        )
        raw = crate.add("struct_field", "raw")
        bits = crate.add("union", "Bits", {"fields": [raw], "impls": []})
        run = crate.add(
            "trait",
            "Run",
            {
                "items": [
                    crate.add("function", "run"),
                    crate.add("assoc_const", "SIZE"),
                    crate.add("assoc_type", "Output"),
                ]
            },
        )
        kinds = crate.module(
            "kinds",
            plain,
            pair,
            shape,
            bits,
            run,
            crate.add("function", "start"),
            crate.add("constant", "LIMIT"),
            crate.add("static", "COUNTER"),
            crate.add("type_alias", "Alias"),
        )
        root = crate.module("demo", kinds, crate.add("macro", "probe"))

        self.assertEqual(
            public_api.api_lines(crate.document(root)),
            [
                "mod demo",
                "mod demo::kinds",
                "type demo::kinds::Alias",
                "union demo::kinds::Bits",
                "field demo::kinds::Bits::raw",
                "static demo::kinds::COUNTER",
                "const demo::kinds::LIMIT",
                "struct demo::kinds::Pair",
                "field demo::kinds::Pair::0",
                "struct demo::kinds::Plain",
                "const demo::kinds::Plain::MAX",
                "field demo::kinds::Plain::count",
                "method demo::kinds::Plain::new",
                "trait demo::kinds::Run",
                "type demo::kinds::Run::Output",
                "const demo::kinds::Run::SIZE",
                "method demo::kinds::Run::run",
                "enum demo::kinds::Shape",
                "variant demo::kinds::Shape::Empty",
                "variant demo::kinds::Shape::Named",
                "field demo::kinds::Shape::Named::code",
                "variant demo::kinds::Shape::One",
                "field demo::kinds::Shape::One::0",
                "fn demo::kinds::start",
                "macro demo::probe",
            ],
        )

    def test_follows_re_exports(self) -> None:
        crate = Crate()
        # The items of the private module `private` are public only through
        # the re-exports, which the crate root holds.
        item = crate.struct("Item")
        helper = crate.add("function", "helper")
        nested = crate.module("nested", crate.add("function", "inner"))
        ready = crate.add(
            "variant", "Ready", {"kind": {"tuple": [crate.add("struct_field", "0")]}}
        )
        gone = crate.add("variant", "Gone", {"kind": "plain"}, hidden=True)
        open_ = crate.add("function", "open")
        shown = crate.module("shown", open_)
        root = crate.module(
            "demo",
            shown,
            crate.use("private::Item", item),
            crate.use("private::helper", helper, "assist"),
            crate.use("private::nested", nested),
            crate.use("private::State::Ready", ready, "Alias"),
            crate.use("private::State::Gone", gone),
            crate.use("shown::open", open_),
        )

        self.assertEqual(
            public_api.api_lines(crate.document(root)),
            [
                "mod demo",
                "variant demo::Alias",
                "field demo::Alias::0",
                "variant demo::Gone (hidden)",
                "struct demo::Item",
                "fn demo::assist",
                "mod demo::nested",
                "fn demo::nested::inner",
                "fn demo::open",
                "mod demo::shown",
                "fn demo::shown::open",
            ],
        )

    def test_marks_hidden_items_and_what_hidden_items_contain(self) -> None:
        crate = Crate()
        program = crate.module(
            "program",
            crate.add("function", "main"),
            crate.struct("Deeper"),
            hidden=True,
        )
        shown = crate.struct(
            "Shown",
            impls=[
                crate.impl(crate.add("function", "open")),
                crate.impl(crate.add("function", "internal"), hidden=True),
            ],
        )
        library = crate.module(
            "library",
            shown,
            crate.add("function", "entry", hidden=True),
            crate.use("library::Shown", shown, "Alias", hidden=True),
        )
        root = crate.module("demo", library, program)

        self.assertEqual(
            public_api.api_lines(crate.document(root)),
            [
                "mod demo",
                "mod demo::library",
                "struct demo::library::Alias (hidden)",
                "method demo::library::Alias::internal (hidden)",
                "method demo::library::Alias::open (hidden)",
                "struct demo::library::Shown",
                "method demo::library::Shown::internal (hidden)",
                "method demo::library::Shown::open",
                "fn demo::library::entry (hidden)",
                "mod demo::program (hidden)",
                "struct demo::program::Deeper (hidden)",
                "fn demo::program::main (hidden)",
            ],
        )

    def test_lists_feature_gated_items_as_any_other(self) -> None:
        crate = Crate()
        main = crate.add("function", "main", attrs=[GATED])
        root = crate.module("demo", crate.module("cli", main, attrs=[GATED]))

        self.assertEqual(
            public_api.api_lines(crate.document(root)),
            ["mod demo", "mod demo::cli", "fn demo::cli::main"],
        )
        # The listing documents every feature, each by name rather than
        # through `default`.
        self.assertEqual(
            public_api.feature_arguments(
                {
                    "default": ["host"],
                    "host": ["serde"],
                    "testing": [],
                    "cli": ["host"],
                    "serde": [],
                    "guest": [],
                }
            ),
            ["--no-default-features", "--features", "cli,guest,host,serde,testing"],
        )

    def test_refuses_another_format_version(self) -> None:
        crate = Crate()
        root = crate.module("demo")
        version = public_api.FORMAT_VERSION + 1

        with self.assertRaisesRegex(
            ScriptError,
            f"format version {version}, but this script reads only version "
            f"{public_api.FORMAT_VERSION}",
        ):
            public_api.api_lines(crate.document(root, version))

    def test_refuses_what_it_cannot_list(self) -> None:
        crate = Crate()
        shown = crate.module("shown", crate.add("function", "open"))
        glob = crate.module("glob", crate.use("shown", shown, glob=True))
        foreign = crate.module("foreign", crate.use("std::collections::HashMap", 999))
        cycle = crate.add("module", "cycle")
        crate.index[str(cycle)]["inner"]["module"] = {
            "items": [crate.use("cycle", cycle, "again")]
        }
        external = crate.module(
            "external", crate.add("extern_crate", "serde", {"name": "serde"})
        )
        for module, error in (
            (glob, "demo::glob re-exports everything of shown"),
            (foreign, "re-exports std::collections::HashMap, which is not an item"),
            (cycle, "demo::cycle::again re-exports a module that contains it"),
            (external, "demo::external::serde is of kind extern_crate"),
        ):
            with self.subTest(error=error):
                root = crate.module("demo", shown, module)
                with self.assertRaisesRegex(ScriptError, error):
                    public_api.api_lines(crate.document(root))


class SnapshotTests(unittest.TestCase):
    def test_check_prints_how_the_api_differs_and_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            linux = "fn demo::kept\nfn demo::removed\n"
            (directory / "linux.txt").write_text(linux, encoding="utf-8")
            (directory / "windows.txt").write_text("fn demo::kept\n", encoding="utf-8")
            output, errors = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                status = public_api.compare(
                    {
                        "linux": ["fn demo::added", "fn demo::kept"],
                        "windows": ["fn demo::kept"],
                    },
                    directory,
                    update=False,
                )

            self.assertEqual(status, 1)
            self.assertIn("\n-fn demo::removed\n", output.getvalue())
            self.assertIn("\n+fn demo::added\n", output.getvalue())
            self.assertIn("linux.txt", errors.getvalue())
            self.assertNotIn("windows.txt", output.getvalue() + errors.getvalue())
            self.assertIn(public_api.UPDATE_COMMAND, errors.getvalue())
            self.assertEqual(
                (directory / "linux.txt").read_text(encoding="utf-8"), linux
            )

    def test_update_rewrites_the_snapshots(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary) / "public-api"
            listings = {
                "linux": ["mod demo", "fn demo::main (hidden)"],
                "windows": ["mod demo"],
            }
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(public_api.compare(listings, directory, True), 0)

            self.assertEqual(
                (directory / "linux.txt").read_bytes(),
                b"mod demo\nfn demo::main (hidden)\n",
            )
            self.assertEqual((directory / "windows.txt").read_bytes(), b"mod demo\n")
            self.assertEqual(public_api.compare(listings, directory, False), 0)


if __name__ == "__main__":
    unittest.main()

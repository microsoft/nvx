#!/usr/bin/env python3
"""Tests for module_diagrams.py, which draws the module diagrams of the nvx
crate that doc/modules publishes, and for the workflow that draws them after
every merge to dev and commits them.

They check how the script reads and checks cargo-modules' exports, which are
written here in its DOT form, how it merges the platforms' modules, the graphs
that it draws, the page that shows them, how it replaces an earlier run's
output, and what it runs, with a stand-in for cargo-modules and Graphviz. They
also check that doc/modules holds a page and the diagrams that it shows, the
workflow's triggers, credentials, and pins, and, on POSIX hosts, run its
publication step against a local repository.
"""

from __future__ import annotations

import contextlib
import io
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path
from typing import Any, cast
from unittest.mock import patch

import yaml

sys.path.insert(0, str(Path(__file__).parent))
import module_diagrams as diagrams  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "module-diagrams.yml"
PUBLISHED = REPO_ROOT / "doc" / "modules"
LINUX, WINDOWS = diagrams.TARGETS


def owns(owner: str, path: str) -> str:
    """The statement of a cargo-modules export in which a module contains
    another."""
    return (
        f'"{owner}" -> "{path}" [label="owns", color="#000000", style="solid"]'
        ' [constraint=true]; // "owns" edge'
    )


def export(
    modules: Mapping[str, str],
    uses: Iterable[tuple[str, str]] = (),
    *,
    extra: Iterable[str] = (),
) -> str:
    """A cargo-modules export in its DOT form: the modules with their
    visibility, the crate first, the containment that their paths imply, the
    uses, and extra statements."""
    lines = [
        "digraph {",
        "",
        "    graph [",
        '        label="nvx",',
        "    ];",
        "",
    ]
    for path, visibility in modules.items():
        if path == diagrams.CRATE:
            lines.append(
                f'    "{path}" [label="crate|{path}", fillcolor="#5397c8"];'
                ' // "crate" node'
            )
        else:
            name = path.removeprefix(f"{diagrams.CRATE}::")
            lines.append(
                f'    "{path}" [label="{visibility} mod|{name}",'
                ' fillcolor="#81c169"]; // "mod" node'
            )
    lines.extend(
        f"    {owns(path.rpartition('::')[0], path)}"
        for path in modules
        if path != diagrams.CRATE
    )
    for user, used in uses:
        lines.append(
            f'    "{user}" -> "{used}" [label="uses", color="#7f7f7f",'
            ' style="dashed"] [constraint=false]; // "uses" edge'
        )
    lines.extend(f"    {statement}" for statement in extra)
    lines += ["", "}", ""]
    return "\n".join(lines)


def structure(found: Mapping[str, Sequence[str]]) -> str:
    """cargo-modules' structure of the modules of found, with their cfg
    conditions: the crate, then each module below the module that contains it,
    in the order of their names."""
    children: dict[str, list[str]] = {}
    for path in sorted(found):
        if path != diagrams.CRATE:
            children.setdefault(path.rpartition("::")[0], []).append(path)
    lines = ["", f"crate {diagrams.CRATE}"]

    def walk(path: str, prefix: str) -> None:
        below = children.get(path, [])
        for index, child in enumerate(below):
            last = index == len(below) - 1
            listed = ", ".join(f"#[cfg({condition})]" for condition in found[child])
            name = child.rpartition("::")[2]
            line = f"{prefix}{'└── ' if last else '├── '}mod {name}: pub"
            lines.append(f"{line} {listed}" if listed else line)
            walk(child, prefix + ("    " if last else "│   "))

    walk(diagrams.CRATE, "")
    return "\n".join(lines) + "\n"


# The modules of a small crate on each platform: host::sys and the guest are
# Linux's alone, and host::windows is Windows'.
LINUX_MODULES = {
    "nvx": "crate",
    "nvx::error": "pub(crate)",
    "nvx::guest": "pub",
    "nvx::guest::time": "pub",
    "nvx::host": "pub",
    "nvx::host::cli": "pub",
    "nvx::host::sys": "pub(self)",
    "nvx::host::sys::socket": "pub(self)",
    "nvx::protocol": "pub",
    "nvx::protocol::nvxs": "pub(self)",
}
WINDOWS_MODULES = {
    "nvx": "crate",
    "nvx::error": "pub(crate)",
    "nvx::host": "pub",
    "nvx::host::cli": "pub",
    "nvx::host::windows": "pub(self)",
    "nvx::protocol": "pub",
    "nvx::protocol::nvxs": "pub(self)",
}
LINUX_USES = (
    ("nvx", "nvx::error"),
    ("nvx::host", "nvx::host::cli"),
    ("nvx::host::cli", "nvx::protocol"),
    ("nvx::host::cli", "nvx::protocol::nvxs"),
    ("nvx::host::sys::socket", "nvx::error"),
    ("nvx::guest::time", "nvx::protocol::nvxs"),
)
WINDOWS_USES = (
    ("nvx", "nvx::error"),
    ("nvx::host::cli", "nvx::protocol::nvxs"),
    ("nvx::host::windows", "nvx::error"),
)
EXPORTS = {
    LINUX: export(LINUX_MODULES, LINUX_USES),
    WINDOWS: export(WINDOWS_MODULES, WINDOWS_USES),
}


def crate() -> diagrams.Crate:
    return diagrams.merge(
        {
            platform: diagrams.parse(EXPORTS[platform], target)
            for platform, target in diagrams.TARGETS.items()
        }
    )


_STATEMENT = re.compile(
    r'^"(?P<tail>[^"]+)"(?: -> "(?P<head>[^"]+)")?(?: \[(?P<attributes>.*)\])?;$'
)
_ATTRIBUTE = re.compile(r'(\w+)=(?:"((?:[^"\\]|\\.)*)"|([^,\s\]]+))')


def statements(source: str) -> list[tuple[str, str | None, dict[str, str]]]:
    """The node and edge statements of a graph that the script drew, one per
    line, with their attributes, unquoted."""
    result: list[tuple[str, str | None, dict[str, str]]] = []
    for line in source.splitlines():
        match = _STATEMENT.match(line)
        if match is None:
            continue
        attributes = {
            key: (quoted.replace('\\"', '"').replace("\\\\", "\\") if quoted else bare)
            for key, quoted, bare in _ATTRIBUTE.findall(match["attributes"] or "")
        }
        result.append((match["tail"], match["head"], attributes))
    return result


def nodes(source: str) -> dict[str, dict[str, str]]:
    return {
        tail: attributes for tail, head, attributes in statements(source) if not head
    }


def edges(source: str) -> dict[tuple[str, str], dict[str, str]]:
    return {
        (tail, head): attributes
        for tail, head, attributes in statements(source)
        if head is not None and attributes.get("style") != "invis"
    }


def label(attributes: Mapping[str, str]) -> list[str]:
    return attributes["label"].split("\\n")


class ParseTests(unittest.TestCase):
    def test_exports_give_each_module_its_visibility_owner_and_uses(self):
        parsed = diagrams.parse(EXPORTS[LINUX], "linux")
        self.assertEqual(parsed.visibility, LINUX_MODULES)
        self.assertEqual(
            parsed.owner,
            {
                path: path.rpartition("::")[0]
                for path in LINUX_MODULES
                if path != diagrams.CRATE
            },
        )
        self.assertEqual(parsed.uses, frozenset(LINUX_USES))

    def test_exports_of_anything_but_modules_and_their_uses_fail(self):
        modules = {"nvx": "crate", "nvx::host": "pub"}
        cases = {
            "a type": export(
                modules,
                extra=['"nvx::host::Spec" [label="pub struct|Spec"]; // "type" node'],
            ),
            "an edge of another kind": export(
                modules,
                extra=['"nvx" -> "nvx::host" [label="x"]; // "calls" edge'],
            ),
            "a module twice": export(
                modules,
                extra=['"nvx::host" [label="pub mod|host"]; // "mod" node'],
            ),
            "no crate": export({"nvx::host": "pub"}),
            "another crate": export(
                modules, extra=['"other" [label="crate|other"]; // "crate" node']
            ),
            "a module named as the crate": export(modules).replace(
                '"nvx" [label="crate|nvx", fillcolor="#5397c8"]; // "crate" node',
                '"nvx" [label="pub mod|nvx"]; // "mod" node',
            ),
            "an edge to no module": export(modules, [("nvx", "nvx::missing")]),
            "a module without an owner": export(modules).replace(
                owns("nvx", "nvx::host"), ""
            ),
            "a module that contains another's": export(
                {"nvx": "crate", "nvx::a": "pub", "nvx::a::b": "pub"}
            ).replace(owns("nvx::a", "nvx::a::b"), owns("nvx", "nvx::a::b")),
            "a module with two owners": export(
                modules, extra=[owns("nvx", "nvx::host")]
            ),
        }
        for case, text in cases.items():
            with self.subTest(case=case):
                self.assertNotEqual(text, export(modules))
                with self.assertRaisesRegex(
                    diagrams.DiagramError, "unexpected cargo-modules export of linux"
                ):
                    diagrams.parse(text, "linux")


STRUCTURE = """
crate nvx
├── mod guest: pub #[cfg(feature = "guest")], #[cfg(target_os = "linux")]
│   └── mod tests: pub(self) #[cfg(test)], #[cfg(any(target_os = "linux", windows))]
├── mod host: pub
│   ├── mod imp: pub(self) #[cfg(not(windows))]
│   └── mod spawn: pub(in crate::host)
└── mod uuid: pub
    └── mod tests: pub(self) #[cfg(test)]
"""


class ConditionsTests(unittest.TestCase):
    def test_the_structure_gives_each_module_its_cfg_conditions(self):
        expected = {
            "nvx": (),
            "nvx::guest": ('feature = "guest"', 'target_os = "linux"'),
            "nvx::guest::tests": ("test", 'any(target_os = "linux", windows)'),
            "nvx::host": (),
            "nvx::host::imp": ("not(windows)",),
            "nvx::host::spawn": (),
            "nvx::uuid": (),
            "nvx::uuid::tests": ("test",),
        }
        self.assertEqual(diagrams.conditions(STRUCTURE, "linux"), expected)
        self.assertEqual(diagrams.conditions(structure(expected), "linux"), expected)

    def test_structures_that_cannot_be_read_fail(self):
        cases = {
            "nothing": "",
            "no crate": "├── mod host: pub\n",
            "another crate": "crate other\n",
            "a function": "crate nvx\n└── fn main: pub\n",
            "a skipped level": "crate nvx\n│   └── mod deep: pub\n",
            "a module twice": "crate nvx\n├── mod a: pub\n└── mod a: pub\n",
            "an unreadable condition": "crate nvx\n└── mod a: pub #[cfg(test)] x\n",
        }
        for case, text in cases.items():
            with self.subTest(case=case):
                with self.assertRaisesRegex(
                    diagrams.DiagramError, "unexpected cargo-modules structure"
                ):
                    diagrams.conditions(text, "linux")


# A plain analysis that lacks nvx::host::imp, which builds under cfg(not(windows)),
# and an analysis with cfg(test) that holds it and the test modules.
PLAIN = {"nvx": "crate", "nvx::error": "pub(crate)", "nvx::host": "pub"}
TESTED = {
    **PLAIN,
    "nvx::host::imp": "pub(self)",
    "nvx::host::tests": "pub(self)",
    "nvx::host::tests::cases": "pub(self)",
}
TESTED_USES = (
    ("nvx::host", "nvx::error"),
    ("nvx::host", "nvx::host::imp"),
    ("nvx::host::imp", "nvx::error"),
    ("nvx::host::tests", "nvx::host::imp"),
    ("nvx::host::tests::cases", "nvx::error"),
)


def found(**changes: tuple[str, ...]) -> dict[str, tuple[str, ...]]:
    """The cfg conditions of TESTED's modules, with the changes of the modules
    named by their last part."""
    result: dict[str, tuple[str, ...]] = {path: () for path in TESTED}
    result["nvx::host::imp"] = ("not(windows)",)
    result["nvx::host::tests"] = ("test",)
    for name, listed in changes.items():
        [path] = [path for path in result if path.rpartition("::")[2] == name]
        result[path] = listed
    return result


class RestoreTests(unittest.TestCase):
    def restore(
        self,
        conditions: Mapping[str, Sequence[str]],
        plain: Mapping[str, str] = PLAIN,
        plain_uses: Iterable[tuple[str, str]] = (("nvx::host", "nvx::error"),),
        tested: Mapping[str, str] = TESTED,
        tested_uses: Iterable[tuple[str, str]] = TESTED_USES,
    ) -> diagrams.Export:
        return diagrams.restore(
            diagrams.parse(export(plain, plain_uses), "linux"),
            diagrams.parse(export(tested, tested_uses), "linux"),
            conditions,
            "linux",
        )

    def test_modules_that_cargo_modules_drops_as_tests_return_with_their_uses(self):
        restored = self.restore(found())
        self.assertEqual(restored.visibility, {**PLAIN, "nvx::host::imp": "pub(self)"})
        self.assertEqual(
            restored.owner,
            {
                "nvx::error": "nvx",
                "nvx::host": "nvx",
                "nvx::host::imp": "nvx::host",
            },
        )
        # The uses of the test modules stay out with them.
        self.assertEqual(
            restored.uses,
            {
                ("nvx::host", "nvx::error"),
                ("nvx::host", "nvx::host::imp"),
                ("nvx::host::imp", "nvx::error"),
            },
        )

    def test_a_test_in_a_string_is_no_condition(self):
        restored = self.restore(found(imp=('feature = "test"',)))
        self.assertIn("nvx::host::imp", restored.visibility)

    def test_a_use_that_only_a_restored_module_makes_counts_once(self):
        # cargo-modules moves the uses of a module that it drops to the module
        # that contains it, so the plain analysis has host use protocol, which
        # only imp does.
        protocol = {"nvx::protocol": "pub"}
        restored = self.restore(
            {**found(), "nvx::protocol": ()},
            plain={**PLAIN, **protocol},
            plain_uses=(
                ("nvx::host", "nvx::error"),
                ("nvx::host", "nvx::protocol"),
            ),
            tested={**TESTED, **protocol},
            tested_uses=(*TESTED_USES, ("nvx::host::imp", "nvx::protocol")),
        )
        self.assertIn(("nvx::host::imp", "nvx::protocol"), restored.uses)
        self.assertNotIn(("nvx::host", "nvx::protocol"), restored.uses)
        dependencies = edges(diagrams.dependencies(diagrams.merge({"Linux": restored})))
        self.assertEqual(dependencies[("nvx::host", "nvx::protocol")]["label"], "1")
        # host uses error itself, as imp does: both uses count.
        self.assertEqual(dependencies[("nvx::host", "nvx::error")]["label"], "2")

    def test_uses_that_only_the_plain_analysis_holds_stay(self):
        # Such as those of an item under cfg(not(test)).
        restored = self.restore(
            found(),
            plain_uses=(("nvx::host", "nvx::error"), ("nvx::error", "nvx::host")),
        )
        self.assertIn(("nvx::error", "nvx::host"), restored.uses)

    def test_uses_that_only_tests_make_stay_out(self):
        restored = self.restore(
            found(), tested_uses=(*TESTED_USES, ("nvx::error", "nvx::host"))
        )
        self.assertNotIn(("nvx::error", "nvx::host"), restored.uses)

    def test_conditions_that_depend_on_test_otherwise_fail(self):
        for condition in ("any(test, unix)", "not(test)", "any(all(test, unix), x)"):
            with self.subTest(condition=condition):
                with self.assertRaisesRegex(
                    diagrams.DiagramError,
                    re.escape(f"nvx::host::imp builds under cfg({condition})"),
                ):
                    self.restore(found(imp=(condition,)))

    def test_modules_that_only_the_plain_analysis_holds_fail(self):
        # Such as one under cfg(not(test)).
        plain = {**PLAIN, "nvx::real": "pub"}
        with self.assertRaisesRegex(
            diagrams.DiagramError, "nvx::real build outside tests only"
        ):
            self.restore(found(), plain=plain)

    def test_the_structure_and_the_dependencies_name_the_same_modules(self):
        conditions = found()
        del conditions["nvx::host::tests::cases"]
        with self.assertRaisesRegex(diagrams.DiagramError, "different modules"):
            self.restore(conditions)


class MergeTests(unittest.TestCase):
    def test_the_platforms_share_their_modules_and_uses(self):
        merged = crate()
        self.assertEqual(merged.platforms, ("Linux", "Windows"))
        self.assertEqual(
            sorted(merged.modules), sorted({**LINUX_MODULES, **WINDOWS_MODULES})
        )
        both = frozenset({"Linux", "Windows"})
        self.assertEqual(merged.modules["nvx::host"].platforms, both)
        self.assertEqual(
            merged.modules["nvx::host::sys"].platforms, frozenset({"Linux"})
        )
        self.assertEqual(
            merged.modules["nvx::host::windows"].platforms, frozenset({"Windows"})
        )
        self.assertEqual(merged.uses[("nvx", "nvx::error")], both)
        self.assertEqual(
            merged.uses[("nvx::host::cli", "nvx::protocol")], frozenset({"Linux"})
        )
        self.assertEqual(merged.owner["nvx::host::windows"], "nvx::host")
        self.assertEqual(
            merged.roots(),
            ["nvx::error", "nvx::guest", "nvx::host", "nvx::protocol"],
        )
        self.assertEqual(
            merged.subtree("nvx::host"),
            [
                "nvx::host",
                "nvx::host::cli",
                "nvx::host::sys",
                "nvx::host::sys::socket",
                "nvx::host::windows",
            ],
        )

    def test_only_the_outermost_module_of_a_platform_names_it(self):
        merged = crate()
        expected = {
            "nvx": None,
            "nvx::guest": "Linux",
            "nvx::guest::time": None,
            "nvx::host": None,
            "nvx::host::sys": "Linux",
            "nvx::host::sys::socket": None,
            "nvx::host::windows": "Windows",
        }
        for path, platforms in expected.items():
            with self.subTest(path=path):
                self.assertEqual(merged.only(path), platforms)

    def test_a_visibility_that_differs_names_each_platform(self):
        merged = diagrams.merge(
            {
                "Linux": diagrams.parse(
                    export({"nvx": "crate", "nvx::a": "pub"}), "linux"
                ),
                "Windows": diagrams.parse(
                    export({"nvx": "crate", "nvx::a": "pub(crate)"}), "windows"
                ),
            }
        )
        self.assertEqual(
            merged.modules["nvx::a"].visibility, "pub on Linux, pub(crate) on Windows"
        )
        drawn = nodes(diagrams.dependencies(merged))
        self.assertIn("dashed", drawn["nvx::a"]["style"])
        self.assertEqual(
            label(drawn["nvx::a"]), ["a", "pub on Linux, pub(crate) on Windows"]
        )

    def test_labels_are_quoted_for_graphviz(self):
        self.assertEqual(diagrams.quote('a"b\\c\nd'), '"a\\"b\\\\c\\nd"')


class GraphTests(unittest.TestCase):
    def test_boxes_show_their_visibility_and_platforms(self):
        source = diagrams.hierarchy(crate(), "nvx::host")
        drawn = nodes(source)
        # Public modules are solid, private ones dashed and unnamed.
        self.assertEqual(label(drawn["nvx::host"]), ["nvx::host"])
        self.assertNotIn("dashed", drawn["nvx::host::cli"]["style"])
        self.assertEqual(label(drawn["nvx::host::sys"]), ["sys", "Linux only"])
        self.assertIn("dashed", drawn["nvx::host::sys"]["style"])
        self.assertEqual(label(drawn["nvx::host::sys::socket"]), ["socket"])
        self.assertEqual(
            label(drawn["nvx::host::windows"]), ["windows", "Windows only"]
        )
        # Any other visibility is named.
        error = nodes(diagrams.dependencies(crate()))["nvx::error"]
        self.assertEqual(label(error), ["error", "pub(crate)"])
        self.assertIn("dashed", error["style"])
        for path, attributes in drawn.items():
            with self.subTest(path=path):
                self.assertIn(path, attributes["tooltip"])
                self.assertEqual(
                    (attributes["fillcolor"], attributes["color"]),
                    diagrams.COLORS["host"],
                )

    def test_hierarchies_draw_the_containment_in_a_top_level_module(self):
        merged = crate()
        source = diagrams.hierarchy(merged, "nvx::host")
        self.assertEqual(set(nodes(source)), set(merged.subtree("nvx::host")))
        self.assertEqual(
            set(edges(source)),
            {
                (merged.owner[path], path)
                for path in merged.subtree("nvx::host")
                if path != "nvx::host"
            },
        )
        # Children keep the order of their paths.
        self.assertIn("ordering=out", source)

    def test_the_overview_shows_every_top_level_module_in_the_crate(self):
        merged = crate()
        source = diagrams.overview(merged)
        drawn = nodes(source)
        self.assertEqual(set(drawn), set(merged.roots()))
        self.assertEqual(
            label(drawn["nvx::host"]),
            ["host", diagrams.DESCRIPTIONS["host"], "4 nested modules"],
        )
        self.assertEqual(
            label(drawn["nvx::guest"]),
            ["guest", diagrams.DESCRIPTIONS["guest"], "1 nested module", "Linux only"],
        )
        self.assertEqual(
            label(drawn["nvx::error"]),
            ["error", diagrams.DESCRIPTIONS["error"], "pub(crate)"],
        )
        self.assertIn('subgraph "cluster_nvx"', source)
        self.assertEqual(edges(source), {})

    def test_the_dependencies_count_the_uses_between_top_level_modules(self):
        source = diagrams.dependencies(crate())
        counts = {
            pair: attributes["label"] for pair, attributes in edges(source).items()
        }
        self.assertEqual(
            counts,
            {
                ("nvx", "nvx::error"): "1",
                ("nvx::guest", "nvx::protocol"): "1",
                # host::sys on Linux and host::windows on Windows.
                ("nvx::host", "nvx::error"): "2",
                ("nvx::host", "nvx::protocol"): "2",
            },
        )
        self.assertEqual(
            edges(source)[("nvx::guest", "nvx::protocol")]["color"],
            diagrams.COLORS["guest"][1],
        )
        self.assertEqual(set(nodes(source)), {"nvx", *crate().roots()})

    def test_the_page_shows_the_overview_the_dependencies_and_each_hierarchy(self):
        drawn = diagrams.diagrams(crate())
        # error contains no module, so it has no hierarchy.
        self.assertEqual(
            [diagram.name for diagram in drawn],
            [
                "overview.svg",
                "dependencies.svg",
                "nvx-guest.svg",
                "nvx-host.svg",
                "nvx-protocol.svg",
            ],
        )
        self.assertEqual(
            [diagram.root for diagram in drawn],
            [None, None, "nvx::guest", "nvx::host", "nvx::protocol"],
        )


class PageTests(unittest.TestCase):
    def page(
        self, output: Path = diagrams.DEFAULT_OUTPUT, tools: str = "the tools"
    ) -> str:
        merged = crate()
        return diagrams.page(merged, diagrams.diagrams(merged), output, tools)

    def links(self, output: Path) -> dict[str, str]:
        """The links of the page in the output directory, by their text."""
        text = " ".join(self.page(output).split())
        return dict(re.findall(r"(?<!!)\[([^\]]+)\]\(([^)]+)\)", text))

    def test_the_page_shows_every_diagram_in_order(self):
        text = self.page()
        self.assertTrue(text.startswith(diagrams.MARKER + "\n\n# Module diagrams\n"))
        self.assertEqual(
            re.findall(r"!\[[^\]]*\]\(([^)]+)\)", text),
            [diagram.name for diagram in diagrams.diagrams(crate())],
        )
        self.assertEqual(
            re.findall(r"^#+ .*$", text, re.M),
            [
                "# Module diagrams",
                "## Overview",
                "## Dependencies",
                "## Hierarchies",
                "### `nvx::guest`",
                "### `nvx::host`",
                "### `nvx::protocol`",
            ],
        )

    def test_the_page_says_what_the_diagrams_cover(self):
        text = " ".join(self.page(tools="TOOLS").split())
        self.assertIn("as TOOLS.", text)
        for feature in diagrams.FEATURES:
            self.assertIn(f"`{feature}`", text)
        for platform, target in diagrams.TARGETS.items():
            self.assertIn(f"{platform} (`{target}`)", text)
        self.assertIn("`nvx::host`, in", text)
        self.assertIn("contains 4 modules.", text)

    def test_the_page_links_lead_from_its_directory_to_the_files(self):
        nested = REPO_ROOT / "build" / "nested" / "modules"
        for output, up in ((diagrams.DEFAULT_OUTPUT, "../.."), (nested, "../../..")):
            with self.subTest(output=output):
                links = self.links(output)
                self.assertEqual(links["`nvx` crate"], f"{up}/nvx-rs/README.md")
                self.assertEqual(
                    links["`nvx-rs/src/host/mod.rs`"], f"{up}/nvx-rs/src/host/mod.rs"
                )
                for target in links.values():
                    path = (output / target.partition("#")[0]).resolve()
                    self.assertTrue(path.is_file(), path)

    def test_the_page_leads_to_how_ci_draws_the_diagrams(self):
        text = " ".join(self.page().split())
        self.assertIn("CI draws them again after every merge to `dev`", text)
        for output, target in (
            (diagrams.DEFAULT_OUTPUT, "../../doc/ci.md#module-diagrams"),
            (PUBLISHED, "../ci.md#module-diagrams"),
        ):
            with self.subTest(output=output):
                self.assertEqual(self.links(output)["Module diagrams"], target)

    def test_an_inline_module_has_no_source_link(self):
        merged = diagrams.merge(
            {
                "Linux": diagrams.parse(
                    export(
                        {"nvx": "crate", "nvx::inline": "pub", "nvx::inline::a": "pub"}
                    ),
                    "linux",
                )
            }
        )
        output = diagrams.DEFAULT_OUTPUT
        text = " ".join(
            diagrams.page(merged, diagrams.diagrams(merged), output, "x").split()
        )
        self.assertIn("`nvx::inline` contains 1 module.", text)


def _svg(title: str) -> str:
    return f'<svg xmlns="http://www.w3.org/2000/svg"><title>{title}</title></svg>\n'


class FakeTools:
    """cargo-modules and Graphviz. Each export command prints the export of its
    target, plain or with cfg(test), which defaults to the plain one; each
    structure command prints the modules of the export with cfg(test), with the
    cfg conditions that found names; and dot draws an SVG titled with its graph's
    name."""

    def __init__(
        self,
        exports: Mapping[str, str] = EXPORTS,
        tested: Mapping[str, str] | None = None,
        found: Mapping[str, Mapping[str, Sequence[str]]] | None = None,
    ) -> None:
        self.outputs: dict[tuple[str, ...], str] = {}
        for platform, target in diagrams.TARGETS.items():
            with_tests = (tested or exports)[platform]
            listed = (found or {}).get(platform, {})
            modules = {
                match["path"]: listed.get(match["path"], ())
                for match in diagrams.NODE.finditer(with_tests)
            }
            self.outputs[tuple(diagrams.export_command(target))] = exports[platform]
            command = diagrams.export_command(target, cfg_test=True)
            self.outputs[tuple(command)] = with_tests
            self.outputs[tuple(diagrams.structure_command(target))] = structure(modules)
        self.commands: list[list[str]] = []
        self.sources: list[str] = []
        self.versions = True
        self.svg = True

    def __call__(self, command: Sequence[str], stdin: str | None) -> diagrams.Output:
        self.commands.append(list(command))
        if tuple(command) in self.outputs:
            return diagrams.Output(self.outputs[tuple(command)], "")
        if command[1:] == ["-Tsvg"] and stdin is not None:
            self.sources.append(stdin)
            name = re.match(r'digraph "([^"]+)"', stdin)
            assert name is not None
            return diagrams.Output(
                _svg(name[1]) if self.svg else "<html/>", "warning: none\n"
            )
        if not self.versions:
            raise diagrams.DiagramError("no version")
        if command == ["cargo", "modules", "--version"]:
            return diagrams.Output("cargo-modules 9.9.9\n", "")
        if command[1:] == ["-V"]:
            return diagrams.Output("", "dot - graphviz version 1.2.3 (0)\n")
        raise AssertionError(f"unexpected command {command}")


class DrawTests(unittest.TestCase):
    def draw(self, output: Path, tools: FakeTools) -> list[str]:
        with contextlib.redirect_stderr(io.StringIO()):
            return diagrams.draw(output, "dot", tools)

    def test_draw_exports_each_platform_and_writes_the_page_and_diagrams(self):
        tools = FakeTools()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "modules"
            names = self.draw(output, tools)
            drawn = diagrams.diagrams(crate())
            self.assertEqual(
                names, sorted([diagrams.PAGE, *(diagram.name for diagram in drawn)])
            )
            self.assertEqual(sorted(path.name for path in output.iterdir()), names)
            for diagram in drawn:
                with self.subTest(diagram=diagram.name):
                    root = ET.parse(output / diagram.name).getroot()
                    self.assertEqual(root.tag, "{http://www.w3.org/2000/svg}svg")
            page = (output / diagrams.PAGE).read_text(encoding="utf-8")
        self.assertEqual(
            tools.commands[: 3 * len(diagrams.TARGETS)],
            [
                command
                for target in diagrams.TARGETS.values()
                for command in (
                    diagrams.export_command(target),
                    diagrams.export_command(target, cfg_test=True),
                    diagrams.structure_command(target),
                )
            ],
        )
        self.assertEqual(tools.sources, [diagram.source for diagram in drawn])
        flat = " ".join(page.split())
        self.assertIn("cargo-modules) 9.9.9 finds them and Graphviz 1.2.3 draws", flat)

    def test_draw_restores_the_modules_that_cargo_modules_drops_as_tests(self):
        # As nvx::host::sandbox::scratch::imp: each platform builds a module of
        # the same path under its own condition, but cargo-modules drops Linux's,
        # under cfg(not(windows)), unless it analyzes with cfg(test).
        linux = {
            **LINUX_MODULES,
            "nvx::host::imp": "pub(self)",
            "nvx::host::tests": "pub(self)",
        }
        windows = {**WINDOWS_MODULES, "nvx::host::imp": "pub(self)"}
        imports = (*LINUX_USES, ("nvx::host::imp", "nvx::error"))
        tools = FakeTools(
            {LINUX: EXPORTS[LINUX], WINDOWS: export(windows, WINDOWS_USES)},
            {LINUX: export(linux, imports), WINDOWS: export(windows, WINDOWS_USES)},
            {
                LINUX: {
                    "nvx::host::imp": ("not(windows)",),
                    "nvx::host::tests": ("test",),
                },
                WINDOWS: {"nvx::host::imp": ("windows",)},
            },
        )
        with tempfile.TemporaryDirectory() as directory:
            self.draw(Path(directory), tools)
        [host] = [
            source
            for source in tools.sources
            if source.startswith('digraph "nvx::host"')
        ]
        drawn = nodes(host)
        # Both platforms build it, so it names neither.
        self.assertEqual(label(drawn["nvx::host::imp"]), ["imp"])
        self.assertNotIn("nvx::host::tests", drawn)

    def test_the_page_names_no_version_that_a_tool_does_not_report(self):
        tools = FakeTools()
        tools.versions = False
        with tempfile.TemporaryDirectory() as directory:
            self.draw(Path(directory), tools)
            page = (Path(directory) / diagrams.PAGE).read_text(encoding="utf-8")
        self.assertIn("finds them and Graphviz draws them", " ".join(page.split()))

    def test_a_failed_drawing_leaves_the_output_unchanged(self):
        tools = FakeTools()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            self.draw(output, tools)
            before = {path.name: path.read_bytes() for path in output.iterdir()}
            failing = FakeTools(
                {LINUX: export({"nvx": "crate"}), WINDOWS: EXPORTS[WINDOWS]}
            )
            failing.svg = False
            with self.assertRaisesRegex(diagrams.DiagramError, "no SVG"):
                self.draw(output, failing)
            broken = FakeTools({LINUX: "digraph {}", WINDOWS: EXPORTS[WINDOWS]})
            with self.assertRaisesRegex(diagrams.DiagramError, "export"):
                self.draw(output, broken)
            after = {path.name: path.read_bytes() for path in output.iterdir()}
        self.assertEqual(after, before)

    def test_the_commands_cover_the_library_with_the_features_of_the_page(self):
        for target in diagrams.TARGETS.values():
            commands = {
                "plain": diagrams.export_command(target),
                "tested": diagrams.export_command(target, cfg_test=True),
                "structure": diagrams.structure_command(target),
            }
            for kind, command in commands.items():
                with self.subTest(target=target, kind=kind):
                    subcommand = "structure" if kind == "structure" else "dependencies"
                    self.assertEqual(
                        command[:4], ["cargo", "modules", subcommand, "--lib"]
                    )
                    options = dict(zip(command[4:], command[5:], strict=False))
                    self.assertEqual(options["--manifest-path"], str(diagrams.MANIFEST))
                    self.assertEqual(options["--features"], ",".join(diagrams.FEATURES))
                    self.assertEqual(options["--target"], target)
                    for excluded in ("fns", "traits", "types"):
                        self.assertIn(f"--no-{excluded}", command)
                    self.assertEqual("--cfg-test" in command, kind != "plain")
            for kind in ("plain", "tested"):
                with self.subTest(target=target, kind=kind):
                    for excluded in ("externs", "sysroot"):
                        self.assertIn(f"--no-{excluded}", commands[kind])
                    self.assertNotIn("--no-uses", commands[kind])
                    self.assertNotIn("--no-owns", commands[kind])

    def test_a_missing_cargo_modules_names_the_release_to_install(self):
        stderr = io.StringIO()
        with (
            patch.object(diagrams.shutil, "which", return_value=None),
            contextlib.redirect_stderr(stderr),
        ):
            status = diagrams.main(["--output", str(REPO_ROOT / "build" / "unused")])
        self.assertEqual(status, 1)
        self.assertIn(
            f"cargo-modules --version {diagrams.CARGO_MODULES_VERSION}",
            stderr.getvalue(),
        )


class ReplaceTests(unittest.TestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.files = {diagrams.PAGE: diagrams.MARKER + "\npage\n", "a.svg": "<svg/>\n"}

    def contents(self, output: Path) -> dict[str, bytes]:
        return {path.name: path.read_bytes() for path in output.iterdir()}

    def test_an_earlier_run_is_replaced_with_lf_line_endings(self):
        output = self.root / "new" / "modules"
        diagrams.replace(output, self.files)
        (output / "stale.svg").write_text("<svg/>\n", encoding="utf-8")
        diagrams.replace(output, self.files | {"b.svg": "<svg>\n</svg>\n"})
        self.assertEqual(
            self.contents(output),
            {
                diagrams.PAGE: (diagrams.MARKER + "\npage\n").encode(),
                "a.svg": b"<svg/>\n",
                "b.svg": b"<svg>\n</svg>\n",
            },
        )
        empty = self.root / "empty"
        empty.mkdir()
        diagrams.replace(empty, self.files)
        self.assertEqual(sorted(self.contents(empty)), ["README.md", "a.svg"])

    def test_directories_that_hold_more_than_an_earlier_run_are_refused(self):
        cases: dict[str, dict[str, str]] = {
            "another file": {diagrams.PAGE: diagrams.MARKER + "\n", "notes.txt": ""},
            "another page": {diagrams.PAGE: "# Notes\n", "a.svg": ""},
            "no page": {"a.svg": "<svg/>\n"},
        }
        for case, files in cases.items():
            with self.subTest(case=case):
                output = self.root / case
                output.mkdir()
                for name, text in files.items():
                    (output / name).write_text(text, encoding="utf-8")
                before = self.contents(output)
                with self.assertRaisesRegex(diagrams.DiagramError, "--output"):
                    diagrams.replace(output, self.files)
                self.assertEqual(self.contents(output), before)
        nested = self.root / "nested"
        (nested / "sub").mkdir(parents=True)
        with self.assertRaisesRegex(diagrams.DiagramError, "sub"):
            diagrams.replace(nested, self.files)
        regular = self.root / "file"
        regular.write_text("", encoding="utf-8")
        with self.assertRaisesRegex(diagrams.DiagramError, "not a directory"):
            diagrams.replace(regular, self.files)

    def test_a_symbolic_link_is_refused_rather_than_written_through(self):
        output = self.root / "linked"
        diagrams.replace(output, self.files)
        target = self.root / "outside.txt"
        target.write_text("outside\n", encoding="utf-8")
        try:
            (output / "b.svg").symlink_to(target)
        except OSError:
            self.skipTest("this host cannot create symbolic links")
        with self.assertRaisesRegex(diagrams.DiagramError, "b.svg"):
            diagrams.replace(output, self.files | {"b.svg": "<svg/>\n"})
        self.assertEqual(target.read_text(encoding="utf-8"), "outside\n")


class PublishedTests(unittest.TestCase):
    def test_doc_modules_holds_the_page_and_the_diagrams_that_it_shows(self):
        page = (PUBLISHED / diagrams.PAGE).read_text(encoding="utf-8")
        self.assertTrue(page.startswith(diagrams.MARKER + "\n"))
        images = re.findall(r"!\[[^\]]*\]\(([^)]+)\)", page)
        self.assertEqual(images[:2], ["overview.svg", "dependencies.svg"])
        self.assertEqual(
            sorted(path.name for path in PUBLISHED.iterdir()),
            sorted([diagrams.PAGE, *images]),
        )
        for image in images:
            with self.subTest(image=image):
                root = ET.parse(PUBLISHED / image).getroot()
                self.assertEqual(root.tag, "{http://www.w3.org/2000/svg}svg")


def _workflow() -> dict[Any, Any]:
    return cast(dict[Any, Any], yaml.safe_load(WORKFLOW.read_text(encoding="utf-8")))


def _job(name: str) -> dict[str, Any]:
    return cast(dict[str, Any], _workflow()["jobs"][name])


def _step(job: str, name: str) -> dict[str, Any]:
    steps = cast(list[dict[str, Any]], _job(job)["steps"])
    [step] = [step for step in steps if step.get("name") == name]
    return step


class WorkflowTests(unittest.TestCase):
    def test_nights_and_pull_requests_that_change_the_inputs_draw(self):
        workflow = _workflow()
        # YAML 1.1 reads the key `on` as true.
        triggers = cast(dict[str, Any], workflow.get("on", workflow.get(True)))
        # dev takes the diagrams once a night, with the nightly full run of CI,
        # rather than with each merge, since every push to dev outside the
        # merge queue rebuilds every queued merge group.
        self.assertEqual(
            set(triggers), {"pull_request", "schedule", "workflow_dispatch"}
        )
        ci = yaml.safe_load(
            (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
        )
        ci_triggers = cast(dict[str, Any], ci.get("on", ci.get(True)))
        self.assertEqual(triggers["schedule"], ci_triggers["schedule"])
        constants = sys.modules[diagrams.GuestRustBuildConstants.__module__].__file__
        inputs = [
            f"{diagrams.MANIFEST.parent.relative_to(REPO_ROOT).as_posix()}/**",
            "rust-toolchain.toml",
            Path(diagrams.__file__).resolve().relative_to(REPO_ROOT).as_posix(),
            # The script takes the crate and its targets from the build constants.
            Path(cast(str, constants)).resolve().relative_to(REPO_ROOT).as_posix(),
            WORKFLOW.relative_to(REPO_ROOT).as_posix(),
        ]
        self.assertEqual(triggers["pull_request"]["branches"], ["dev"])
        self.assertEqual(triggers["pull_request"]["paths"], inputs)
        self.assertIn("ready_for_review", triggers["pull_request"]["types"])
        self.assertEqual(
            _job("draw")["if"],
            "github.event_name != 'pull_request' || "
            "github.event.pull_request.draft == false",
        )

    def test_only_the_publication_of_dev_holds_a_credential(self):
        workflow = _workflow()
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        for name, job in workflow["jobs"].items():
            with self.subTest(job=name):
                self.assertNotIn("permissions", job)
        text = WORKFLOW.read_text(encoding="utf-8")
        self.assertEqual(
            re.findall(r"secrets\.\w+", text),
            ["secrets.PERFORMANCE_PERSIST_DEPLOY_KEY"],
        )
        self.assertEqual(
            _step("publish", "Checkout dev")["with"]["ssh-key"],
            "${{ secrets.PERFORMANCE_PERSIST_DEPLOY_KEY }}",
        )
        self.assertIs(_step("draw", "Checkout")["with"]["persist-credentials"], False)
        publish = _job("publish")
        self.assertEqual(publish["needs"], "draw")
        self.assertEqual(
            publish["if"],
            "github.event_name != 'pull_request' && github.ref == 'refs/heads/dev'",
        )
        # The publication runs nothing of the commit but Git.
        for step in publish["steps"]:
            with self.subTest(step=step["name"]):
                self.assertFalse(step.get("uses", "").startswith("./"))
                self.assertNotIn("${{", step.get("run", ""))
                self.assertNotIn("python", step.get("run", ""))

    def test_the_publication_commits_what_the_draw_job_drew(self):
        upload = _step("draw", "Upload the diagrams")["with"]
        download = _step("publish", "Download the diagrams")["with"]
        self.assertEqual(upload["path"], PUBLISHED.relative_to(REPO_ROOT).as_posix())
        self.assertEqual(upload["name"], download["name"])
        self.assertIn("github.run_id", upload["name"])
        self.assertEqual(
            _step("publish", "Commit the diagrams to dev")["env"]["DIAGRAMS"],
            download["path"],
        )
        draw = _step("draw", "Draw the diagrams")["run"]
        self.assertIn("python3 scripts/module_diagrams.py --output doc/modules", draw)
        self.assertIn('"cargo-modules ${CARGO_MODULES_VERSION}"', draw)

    def test_dev_draws_its_newest_commit_and_names_it_in_the_commit(self):
        # Whichever run commits last commits the diagrams of the newest dev,
        # in whatever order the runs start.
        self.assertEqual(
            _step("draw", "Checkout")["with"]["ref"],
            "${{ github.event_name != 'pull_request' && github.ref || '' }}",
        )
        self.assertEqual(
            _job("draw")["outputs"], {"sha": "${{ steps.draw.outputs.sha }}"}
        )
        self.assertIn(
            'echo "sha=$(git rev-parse HEAD)" >> "${GITHUB_OUTPUT}"',
            _step("draw", "Draw the diagrams")["run"],
        )
        commit = _step("publish", "Commit the diagrams to dev")
        self.assertEqual(commit["env"]["DRAWN"], "${{ needs.draw.outputs.sha }}")
        self.assertIn(
            '"doc: update module diagrams (${DRAWN}) [skip ci]"', commit["run"]
        )

    def test_the_workflow_pins_its_tools(self):
        workflow = _workflow()
        self.assertEqual(
            workflow["env"]["CARGO_MODULES_VERSION"], diagrams.CARGO_MODULES_VERSION
        )
        # A fixed Ubuntu release keeps the Graphviz release that lays out the
        # diagrams, and Liberation Sans, the first font that they name.
        self.assertRegex(_job("draw")["runs-on"], r"^ubuntu-\d\d\.\d\d$")
        self.assertTrue(diagrams.FONT.startswith("Liberation Sans,"))
        install = _step("draw", "Install Graphviz")["run"]
        self.assertIn("graphviz fonts-liberation", install)
        rust = _step("draw", "Install the pinned Rust release")["run"]
        self.assertIn("rust-toolchain.toml", rust)
        restore = _step("draw", "Restore cargo-modules")
        save = _step("draw", "Save cargo-modules")
        self.assertEqual(restore["with"], save["with"])
        self.assertIn("env.CARGO_MODULES_VERSION", restore["with"]["key"])
        self.assertIn("env.RUST_TOOLCHAIN", restore["with"]["key"])
        self.assertIn("github.ref == 'refs/heads/dev'", save["if"])

    def test_runs_of_dev_wait_and_pull_requests_cancel(self):
        concurrency = _workflow()["concurrency"]
        self.assertEqual(
            concurrency["cancel-in-progress"],
            "${{ github.event_name == 'pull_request' }}",
        )
        self.assertIn(
            "github.event.pull_request.number || github.ref", concurrency["group"]
        )

    def test_the_publication_compares_the_paths_that_start_the_workflow(self):
        # A change of any other path starts no pull request run that would
        # draw it.
        workflow = _workflow()
        triggers = cast(dict[str, Any], workflow.get("on", workflow.get(True)))
        commit = _step("publish", "Commit the diagrams to dev")
        inputs = cast(str, commit["env"]["INPUTS"]).split()
        paths = cast(list[str], triggers["pull_request"]["paths"])
        self.assertEqual(inputs, [path.removesuffix("/**") for path in paths])
        self.assertIn(
            'git diff --quiet "${DRAWN}" HEAD -- "${inputs[@]}"', commit["run"]
        )


BASH = shutil.which("bash")
GIT = shutil.which("git")
# A commit that the local repository does not hold.
UNKNOWN = "0123456789abcdef0123456789abcdef01234567"


@unittest.skipIf(
    os.name == "nt" or BASH is None or GIT is None,
    "runs the workflow's bash step with Git on POSIX hosts",
)
class PublishStepTests(unittest.TestCase):
    """Runs the publication step in a clone of a local repository."""

    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        config = self.root / "gitconfig"
        config.write_text("", encoding="utf-8")
        # Git reads neither the host's configuration nor an identity from the
        # environment, as on a runner, where the step sets its own.
        self.environment = {
            **{
                name: value
                for name, value in os.environ.items()
                if not name.startswith(("GIT_AUTHOR_", "GIT_COMMITTER_"))
            },
            "GIT_CONFIG_GLOBAL": str(config),
            "GIT_CONFIG_NOSYSTEM": "1",
        }
        self.origin = self.root / "origin.git"
        self.git("init", "--bare", "--initial-branch=dev", str(self.origin))
        self.seed = self.root / "seed"
        self.git("clone", str(self.origin), str(self.seed))
        # The drawn commit: the diagrams of an input, nvx-rs/src/lib.rs.
        self.drawn = self.advance(
            {
                "doc/modules/README.md": "old page\n",
                "doc/modules/overview.svg": "<svg/>\n",
                "doc/ci.md": "# CI\n",
                "nvx-rs/src/lib.rs": "// The crate.\n",
            }
        )
        self.checkout = self.root / "checkout"
        self.git("clone", "--branch", "dev", str(self.origin), str(self.checkout))
        # The diagrams replace the page and remove overview.svg.
        self.diagrams = self.root / "diagrams"
        self.diagrams.mkdir()
        (self.diagrams / "README.md").write_text("new page\n", encoding="utf-8")
        (self.diagrams / "dependencies.svg").write_text("<svg></svg>\n", "utf-8")

    def git(self, *arguments: str) -> str:
        return subprocess.run(
            [cast(str, GIT), *arguments],
            env=self.environment,
            capture_output=True,
            text=True,
            check=True,
        ).stdout

    def advance(self, files: Mapping[str, str]) -> str:
        """Pushes a commit of the files to dev, after dev's newest commit, and
        returns it."""
        if self.git("-C", str(self.origin), "branch", "--list", "dev").strip():
            self.git("-C", str(self.seed), "pull", "--ff-only", "origin", "dev")
        for name, text in files.items():
            (self.seed / name).parent.mkdir(parents=True, exist_ok=True)
            (self.seed / name).write_text(text, encoding="utf-8")
        self.git("-C", str(self.seed), "add", "--all")
        self.git(
            "-C",
            str(self.seed),
            "-c",
            "user.name=Seed",
            "-c",
            "user.email=seed@example.com",
            "commit",
            "-m",
            "advance",
        )
        self.git("-C", str(self.seed), "push", "origin", "HEAD:dev")
        return self.git("-C", str(self.seed), "rev-parse", "HEAD").strip()

    def publish(self, drawn: str | None = None) -> subprocess.CompletedProcess[str]:
        step = _step("publish", "Commit the diagrams to dev")
        return subprocess.run(
            [cast(str, BASH), "--noprofile", "--norc", "-eo", "pipefail", "-c"]
            + [step["run"]],
            cwd=self.checkout,
            env={
                **self.environment,
                "DIAGRAMS": str(self.diagrams),
                "DRAWN": self.drawn if drawn is None else drawn,
                "INPUTS": step["env"]["INPUTS"],
            },
            capture_output=True,
            text=True,
            check=False,
        )

    def head(self) -> str:
        return self.git("-C", str(self.origin), "rev-parse", "dev").strip()

    def dev(self) -> tuple[str, list[str], list[str]]:
        """The subject and author of dev's newest commit, and its files."""
        subject, author = self.git(
            "-C", str(self.origin), "log", "-1", "--format=%s%n%an", "dev"
        ).splitlines()
        files = self.git(
            "-C", str(self.origin), "ls-tree", "-r", "--name-only", "dev"
        ).split()
        return subject, [author], files

    def test_changed_diagrams_are_committed_to_dev_without_ci(self):
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        subject, author, files = self.dev()
        self.assertEqual(
            subject, f"doc: update module diagrams ({self.drawn}) [skip ci]"
        )
        self.assertEqual(author, ["github-actions[bot]"])
        self.assertEqual(
            files,
            [
                "doc/ci.md",
                "doc/modules/README.md",
                "doc/modules/dependencies.svg",
                "nvx-rs/src/lib.rs",
            ],
        )
        changed = self.git(
            "-C", str(self.origin), "diff", "--name-only", "dev~1", "dev"
        ).split()
        self.assertTrue(all(path.startswith("doc/modules/") for path in changed))
        # The same diagrams again change nothing.
        head = self.head()
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("up to date", result.stdout)
        self.assertEqual(self.head(), head)

    def test_a_rerun_after_dev_changed_the_inputs_publishes_nothing(self):
        # A newer run drew and published the diagrams of newer inputs. A rerun of
        # the older publication alone reuses the older drawing, and must not
        # replace the newer diagrams with it.
        newer = self.advance(
            {"nvx-rs/src/lib.rs": "// A newer crate.\n", "doc/modules/README.md": "x\n"}
        )
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("::notice::", result.stdout)
        self.assertEqual(self.head(), newer)

    def test_newer_commits_that_leave_the_inputs_alone_hold_nothing_back(self):
        newer = self.advance({"doc/ci.md": "# CI, revised\n"})
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        subject, _, files = self.dev()
        self.assertEqual(
            subject, f"doc: update module diagrams ({self.drawn}) [skip ci]"
        )
        self.assertEqual(
            self.git("-C", str(self.origin), "rev-parse", "dev~1").strip(), newer
        )
        self.assertIn("doc/modules/dependencies.svg", files)

    def test_a_rejected_push_is_retried_on_the_newest_dev(self):
        hook = self.origin / "hooks" / "pre-receive"
        hook.write_text(
            '#!/bin/sh\nmarker="$(dirname "$0")/rejected"\n'
            '[ -e "$marker" ] && exit 0\n: > "$marker"\nexit 1\n',
            encoding="utf-8",
        )
        hook.chmod(0o755)
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((self.origin / "hooks" / "rejected").exists())
        subject, _, files = self.dev()
        self.assertIn("[skip ci]", subject)
        self.assertIn("doc/modules/dependencies.svg", files)

    def test_diagrams_with_other_files_or_no_page_are_refused(self):
        cases = {
            "a script": lambda: (self.diagrams / "run.sh").write_text("", "utf-8"),
            "a directory": lambda: (self.diagrams / "sub").mkdir(),
            "no page": lambda: (self.diagrams / "README.md").unlink(),
        }
        head = self.head()
        # A drawn commit that the repository lacks fails the comparison of the
        # inputs.
        for drawn in (
            "",
            "0123abcd",
            UNKNOWN.upper(),
            f"{UNKNOWN}\n{UNKNOWN}",
            UNKNOWN,
        ):
            with self.subTest(drawn=drawn):
                result = self.publish(drawn)
                self.assertEqual(result.returncode, 1)
                self.assertIn("::error::", result.stdout)
                self.assertEqual(self.head(), head)
        for case, change in cases.items():
            with self.subTest(case=case):
                change()
                result = self.publish()
                self.assertEqual(result.returncode, 1)
                self.assertIn("::error::", result.stdout)
                self.assertEqual(self.head(), head)
                shutil.rmtree(self.diagrams)
                self.diagrams.mkdir()
                (self.diagrams / "README.md").write_text("new page\n", "utf-8")


if __name__ == "__main__":
    unittest.main()

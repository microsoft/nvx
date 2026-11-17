#!/usr/bin/env python3
"""Tests for check_markdown_links.py, the link check that check-quality runs.

The check resolves the relative links and anchors of the tracked Markdown
files, and the doc/ paths that tracked files mention, as GitHub does (#563).
These tests cover its heading slugs, which Markdown holds link targets, how it
resolves targets in a work tree and a submodule, its allowlist of test
fixtures, and its command line.
"""

from __future__ import annotations

import contextlib
import io
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Collection, Mapping
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import check_markdown_links as checker  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[1]
NO_FIXTURES: Mapping[str, Collection[str]] = {}


def _anchors(text: str) -> frozenset[str]:
    return checker.parse_markdown(text).anchors


def _targets(text: str) -> list[tuple[int, str]]:
    return [(link.line, link.target) for link in checker.parse_markdown(text).links]


class SlugTests(unittest.TestCase):
    def test_slugs_keep_words_and_hyphenate_spaces(self):
        cases = {
            "3.2.10 Guest obligations": "3210-guest-obligations",
            "Debug kernel (opt-in)": "debug-kernel-opt-in",
            "Linux / KVM": "linux--kvm",
            "NVX: An Ultra-Light Micro-VM": "nvx-an-ultra-light-micro-vm",
            "snake_case & C#": "snake_case--c",
            "Ünïcode Größe": "ünïcode-größe",
        }
        for text, slug in cases.items():
            with self.subTest(text=text):
                self.assertEqual(checker.github_slug(text), slug)

    def test_heading_anchors_use_the_rendered_text(self):
        headings = (
            "# NVX",
            "## `nvx_tools/`",
            "### `guest/`, `ubuntu/`, and `azurelinux/`",
            "## [Run](run.md) the _fast_ __path__",
            "## Escaped \\_name\\_ &amp; more",
            "## Closed heading ##",
            "## <code>Tagged</code> ![logo](logo.png) heading",
            "## Code `` a`b `` span",
            "## _`--backend`_ option",
            "## [`a]b`](link.md)",
            "> ## Quoted",
        )
        self.assertEqual(
            _anchors("\n\n".join(headings)),
            {
                "nvx",
                "nvx_tools",
                "guest-ubuntu-and-azurelinux",
                "run-the-fast-path",
                "escaped-_name_--more",
                "closed-heading",
                "tagged--heading",
                "code-ab-span",
                "--backend-option",
                "ab",
                "quoted",
            },
        )

    def test_repeated_headings_get_numbered_anchors(self):
        self.assertEqual(
            _anchors("# Usage\n\n## Usage\n\n## Usage-1\n\n### Usage\n"),
            {"usage", "usage-1", "usage-1-1", "usage-2"},
        )

    def test_setext_headings_have_anchors_but_thematic_breaks_do_not(self):
        self.assertEqual(
            _anchors("Title\n=====\n\nSection\n---\n\n- Item\n---\n\nText\n\n---\n"),
            {"title", "section"},
        )

    def test_setext_lines_keep_their_final_spaces_in_code_spans(self):
        # Elsewhere, the spaces around a line ending render nothing.
        cases = {
            "`foo  \nbar`\n===\n": {"foo---bar"},
            "> `foo  \n> bar`\n> ===\n": {"foo---bar"},
            "a  \nb\n===\n": {"ab"},
            "`foo\n  bar`\n===\n": {"foo-bar"},
        }
        for document, anchors in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_anchors(document), anchors)

    def test_heading_text_drops_link_destinations_with_parentheses(self):
        self.assertEqual(
            _anchors("## [Run](a_(b).md)\n\n## ![Logo](c_(d).png) Logo\n"),
            {"run", "-logo"},
        )

    def test_heading_autolinks_keep_their_address(self):
        self.assertEqual(
            _anchors("# Contact <help@example.com>\n\n# See <https://example.com/x>\n"),
            {"contact-helpexamplecom", "see-httpsexamplecomx"},
        )

    def test_heading_references_decode_in_place_and_need_semicolons(self):
        self.assertEqual(
            _anchors(
                "# &copy\n\n# &<b>copy;</b> &#95;x&#95; \\&amp;\n\n"
                "# <https://a.com/?a&amp;b_c_>\n"
            ),
            {"copy", "copy-_x_-amp", "httpsacomab_c_"},
        )

    def test_html_headings_have_anchors_numbered_with_the_others(self):
        # A heading's start tag closes the heading that it starts in, and
        # inline, an HTML heading's Markdown renders first.
        document = (
            '<h1 align="center">NVX</h1>\n\n'
            "<h2>Hello <b>World</b> &amp; more &copy</h2>\n\n"
            '<h2 id="x">Same</h2>\n\n## Same\n\n'
            "Text <h4>**Bold** _x_</h4> end\n\n"
            "<h2>A <h3>B</h3> C</h2>\n\n"
            "<h2>A <!-- B --> \\_c\\_</h2>\n"
        )
        self.assertEqual(
            _anchors(document),
            {
                "nvx",
                "hello-world--more-copy",
                "x",
                "same",
                "same-1",
                "bold-x",
                "a-",
                "b",
                "a--_c_",
            },
        )

    def test_html_headings_lose_only_the_markers_of_their_containers(self):
        # While the indentation after the markers stays in the text, where
        # the columns of a tab that a marker consumes in part are spaces.
        cases = {
            "> <h2>\n> Usage</h2>\n": {"usage"},
            "- <h2>\n  Usage</h2>\n": {"usage"},
            "> <h2>\n>   Usage</h2>\n": {"--usage"},
            "> <h2>\n>\tUsage</h2>\n": {"--usage"},
            "- <h2>\n \tUsage</h2>\n": {"--usage"},
            "> <h2>\n> \tUsage</h2>\n": {"usage"},
        }
        for document, anchors in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_anchors(document), anchors)

    def test_html_headings_run_on_until_their_elements_close(self):
        # GitHub slugs the text of each heading in the tree that libxml2
        # builds from its HTML, where few tags close a heading, such as the
        # start tag of a paragraph, so one runs on across blocks.
        cases = {
            "<h2>First\n\n<span>\nSecond\n</span>\n</h2>\n": {"firstsecond"},
            "- <h2>First\n\n  <span>\n  Second\n  </span>\n": {"firstsecond"},
            "> <h2>First\n\n<span>\nSecond\n</span>\n": {"first"},
            (
                "<h2>First\n\n    code\n\n***\n\n- item\n\n> quote\n\n"
                "<span>\nLast\n</span>\n"
            ): {"firstcodeitemquotelast"},
            "<h2>First\n\nPara\n": {"first"},
            "<h2>A <span>B\n\nPara\n": {"a-bpara"},
            "<h2>A <span>\n\n| b |\n| - |\n| c |\n": {"a-bc"},
            "## Title <div>\n\nPara\n": {"title-para"},
            "x <h2>A\n\n- item\n": {"aitem"},
            "<h2>First <!-- open\n\n<div>\n-->Second</h2>\n</div>\n": {"first-second"},
        }
        for document, anchors in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_anchors(document), anchors)

    def test_tags_close_headings_as_libxml2_closes_their_elements(self):
        # A start tag closes the current element only if libxml2 lists it for
        # that element, and an end tag closes no element whose priority, such
        # as a div's, is higher than its own.
        cases: dict[str, set[str]] = {
            "<h2>A <p>x</p> y</h2>\n": {"a-"},
            "<h2>B <table><tr><td>x</td></tr></table> y</h2>\n": {"b-"},
            "<h2>C <li>x</li> y</h2>\n": {"c-"},
            "<h2>D <ul><li>x</li></ul> y</h2>\n": {"d-x-y"},
            "<h2>E <span>x <p>y</p> z</span></h2>\n": {"e-x-y-z"},
            "<h2>F <b>x <p>y</p> z</b></h2>\n": {"f-x-"},
            "<h2>G <div>x</h2> y</div> z</h2>\n": {"g-x-y-z"},
            "<div><h2>H</div> x</h2>\n": {"h"},
            "<h2/>M</h2>\n": set(),
            "## I <p>x</p> y\n": {"i-"},
            "## J </h3> x\n": {"j--x"},
            "## [K <p>x</p> y](README.md) z\n": {"k-x-y-z"},
            "## *L <span>x* <p>y</p>\n": {"l-x-"},
        }
        for document, anchors in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_anchors(document), anchors)

    def test_elements_that_earlier_blocks_leave_open_hold_later_headings(self):
        # Neither a blank line nor the end tag of a block quote, list item, or
        # paragraph closes a div, whose end tag then closes a heading in it.
        cases = {
            "<div>\n\n<h2>H</div> x</h2>\n": {"h"},
            "<div>\n\n## H </div> x\n": {"h-"},
            "> <div>\n\n<h2>H</div> x</h2>\n": {"h"},
            "- <div>\n- <h2>H</div> x</h2>\n": {"h"},
            "Text <div> more\n\n<h2>H</div> x</h2>\n": {"h"},
            "<div>\n\nText[^1]\n\n[^1]: <h2>N</div> x</h2>\n": {
                "n",
                "fn-1",
                "fnref-1",
            },
            "<div>\n\n</div>\n\n<h2>H</div> x</h2>\n": {"h-x"},
        }
        for document, anchors in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_anchors(document), anchors)

    def test_tight_lists_render_their_paragraphs_without_tags(self):
        # So a heading inside an item runs on through the item's next
        # paragraphs, unless a blank line separates two items of its list or
        # two blocks of one item. Blank lines in fenced code, or that end the
        # list, separate nothing, and neither does a paragraph that holds only
        # definitions, unless a block of the list's parent interrupts it.
        cases = {
            "- text <h2>A\n  ***\n  tail\n": {"atail"},
            "1. text <h2>A\n   ***\n   tail\n": {"atail"},
            "- text <h2>A\n\n  ***\n\n  tail\n": {"a"},
            "- x <h2>A\n  ***\n  tail\n- next\n": {"atail"},
            "- x <h2>A\n  ***\n  tail\n\n- next\n": {"a"},
            "- x <h2>A\n  ***\n  tail\n\nafter\n": {"atail"},
            "- x <h2>A\n  ***\n  tail\n-\n\n- c\n": {"a"},
            "- a <h2>A\n  - b\n\n  - c\n": {"abc"},
            "- > q <h2>A\n  > ***\n  > tail\n": {"a"},
            "- x <h2>A\n  ```\n  code\n\n  more\n  ```\n  tail\n": {"acodemoretail"},
            "[d]\n\n- x <h2>A\n  ***\n  tail\n\n  [d]: d.md\n": {"atail"},
            "- x <h2>A\n  ***\n  tail\n\n  [d]: d.md\n# H\n\n[d]\n": {"a", "h"},
        }
        for document, anchors in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_anchors(document), anchors)

    def test_ordered_list_items_render_in_ol_elements(self):
        # Whose end tags close only the elements of ordered lists.
        cases = {
            "1. <h2>Head</ol> tail</h2>\n": {"head"},
            "3. <h2>Head</ol> tail</h2>\n": {"head"},
            "1) <h2>Head</ol> tail</h2>\n": {"head"},
            "1. - <h2>Head</ol> tail</h2>\n": {"head"},
            "1. <h2>Head</ul> tail</h2>\n": {"head-tail"},
            "- <h2>Head</ul> tail</h2>\n": {"head"},
            "- <h2>Head</ol> tail</h2>\n": {"head-tail"},
        }
        for document, anchors in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_anchors(document), anchors)

    def test_inline_raw_html_renders_as_an_html_parser_reads_it(self):
        # Markup other than a tag or a comment ends at its first ">", a
        # comment at "--!>" too, and markup in the attributes of the tags that
        # GitHub filters is live, so the rest renders, headings included.
        cases = {
            "# A <![CDATA[ x > B ]]> C\n": {"a--b--c"},
            "# A <?php x > B ?> C\n": {"a--b--c"},
            "# A <!-- x --!> B --> C\n": {"a--b----c"},
            "# A <!DOCTYPE x> C\n": {"a--c"},
            '# A <script title="<b>x</b>"> C\n': {"a-script-titlex-c"},
            "Text <![CDATA[ x > <h2>B</h2> ]]> end\n": {"b"},
            'Text <script title="<h2>Z</h2>"> end\n': {"z"},
            "Text <!-- x --!> <h2>C</h2> --> end\n": {"c"},
        }
        for document, anchors in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_anchors(document), anchors)

    def test_headings_without_text_have_no_anchors(self):
        # A heading whose slug is empty still counts toward the numbers.
        cases = {
            "#\n\n#\n\n# -\n": {"-"},
            "<h2></h2>\n\n<h2> </h2>\n\n<h2>-</h2>\n": {"-", "--1"},
            "# !\n\n# ?\n\n# !-\n": {"-", "-1"},
        }
        for document, anchors in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_anchors(document), anchors)

    def test_heading_delimiters_match_as_cmark_gfm_matches_them(self):
        # The text of each link matches its delimiters alone, and a match,
        # even of strikethrough runs of different lengths, drops the
        # delimiters between its own. A closer that matches nothing bounds
        # the search of each later closer of its character and length modulo
        # three, whether or not either can open.
        cases = {
            "# a[_b_](README.md)c\n": "abc",
            "# _foo__bar_\n": "foo__bar",
            "# foo_bar_baz\n": "foo_bar_baz",
            "# *a _b* c_\n": "a-_b-c_",
            "# _a *b_ c*\n": "a-b-c",
            "# ~a _b~~ c_\n": "a-_b-c_",
            "# _a__b_\n": "a__b",
            "# __a_b__\n": "a_b",
            "# __a_\n": "_a",
            "# _a [b_](x.md) c_\n": "a-b_-c",
            "# _a [_b](y.md) c_\n": "a-_b-c",
            "# &#95;a_ b_\n": "_a_-b_",
            "# __a *!_!b* c_\n": "__a-_b-c_",
        }
        for document, anchor in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_anchors(document), {anchor})

    def test_reference_labels_end_past_their_escaped_brackets(self):
        self.assertEqual(
            _anchors("# [Shown][key\\]]\n\n[key\\]]: README.md\n"), {"shown"}
        )

    def test_bare_url_autolinks_keep_their_text_like_angle_bracket_ones(self):
        # Unless brackets hold one or its domain is not one that GitHub links,
        # while GitHub links a bare email address only after emphasis.
        cases = {
            "# https://example.com/_foo_/bar\n": "httpsexamplecom_foo_bar",
            "# <https://example.com/_foo_/bar>\n": "httpsexamplecom_foo_bar",
            "# www.example.com/_foo_\n": "wwwexamplecom_foo_",
            "# ftp://example.com/_a_\n": "ftpexamplecom_a_",
            "# (see https://example.com/_a_)\n": "see-httpsexamplecom_a_",
            "# https://example.com/a&amp;_b_ c\n": "httpsexamplecomaamp_b_-c",
            "# [see https://example.com/_a_] x\n": "see-httpsexamplecoma-x",
            "# https://exa_mple.com/_a_ b_\n": "httpsexa_mplecoma-b_",
            "# xhttps://example.com/_a_\n": "xhttpsexamplecoma",
            "# Contact _help_@example.com\n": "contact-helpexamplecom",
            "# Contact <_help_@example.com>\n": "contact-_help_examplecom",
        }
        for document, anchor in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_anchors(document), {anchor})
        self.assertEqual(_targets("https://example.com/[a](b.md) end\n"), [])

    def test_heading_citations_render_the_numbers_of_their_footnotes(self):
        self.assertEqual(
            _anchors("# Title[^note]\n\n[^note]: Body.\n"),
            {"title1", "fn-note", "fnref-note"},
        )
        self.assertEqual(
            _anchors("Text[^a]\n\n# Title[^b] and[^a]\n\n[^a]: A\n[^b]: B\n"),
            {"title2-and1", "fn-a", "fn-b", "fnref-a", "fnref-a-2", "fnref-b"},
        )

    def test_headings_keep_reference_links_whose_labels_are_undefined(self):
        self.assertEqual(
            _anchors(
                "# [Overview][undefined]\n\n[defined]: d.md\n\n"
                "# [Shown][defined]\n\n# [Collapsed][]\n\n# [Defined][]\n"
            ),
            {"overviewundefined", "shown", "collapsed", "defined"},
        )

    def test_headings_drop_defined_shortcut_references(self):
        self.assertEqual(
            _anchors(
                "# ![badge] Title\n\n## [label] Text\n\n### ![badge][] Collapsed\n\n"
                "#### [label][undefined] Full\n\n"
                "[badge]: https://example.com/logo.png\n[label]: label.md\n"
            ),
            {"-title", "label-text", "-collapsed", "labelundefined-full"},
        )

    def test_cited_footnotes_have_anchors(self):
        self.assertEqual(
            _anchors(
                "Text[^note] and[^Note].\n\n[^note]: Cited.\n\n[^unused]: Not cited.\n"
            ),
            {"fn-note", "fnref-note", "fnref-note-2"},
        )

    def test_headings_in_list_items_and_block_quotes_have_anchors(self):
        self.assertEqual(
            _anchors(
                "- ## Listed heading\n\n> ## Quoted heading\n\n"
                "> Quoted\n> ===\n\n- Listed\n  ---\n\n> Lazy\n===\n"
            ),
            {"listed-heading", "quoted-heading", "quoted", "listed"},
        )

    def test_html_ids_and_names_are_anchors(self):
        # GitHub's script finds a name on any element that it keeps.
        self.assertEqual(
            _anchors(
                '<a id="custom"></a>\n\n<a name="legacy"></a>\n\n'
                '<div name="named"></div>\n\n`<a id="x">`\n'
            ),
            {"custom", "legacy", "named"},
        )

    def test_front_matter_code_and_comments_hold_no_headings(self):
        document = (
            "---\n# front matter\n---\n"
            "# Title\n\n"
            "```bash\n# Linux/KVM\n```\n\n"
            "1. Step\n\n    ```text\n    # indented\n    ```\n\n"
            "- ```text\n  # listed\n  ```\n\n"
            "~~~~\n```\n# inner\n~~~~\n\n"
            "<!--\n# Commented\n-->\n\n"
            "    # Indented code\n\n"
            "<div>\n# Raw HTML\n</div>\n"
        )
        self.assertEqual(_anchors(document), {"title"})


class MarkdownLinkTests(unittest.TestCase):
    def test_links_images_definitions_and_html_attributes_are_targets(self):
        document = (
            'See [a](guide.md#usage "Guide") and ![b](<images/a b.png>).\n'
            "[Wrapped\ntext](run.md) and [c](a_(b).md) and [d](a\\_b.md).\n"
            "[![badge](badge.svg)](status.md)\n"
            "<a href=\"setup.md?x=1&amp;y=2\">e</a> <img src='diagram.svg'>\n"
            "\n"
            '[reference]: <ref target.md> "Title"\n'
            "[external]: https://example.com\n"
        )
        self.assertEqual(
            _targets(document),
            [
                (1, "guide.md#usage"),
                (1, "images/a b.png"),
                (3, "run.md"),
                (3, "a_(b).md"),
                (3, "a_b.md"),
                (4, "badge.svg"),
                (4, "status.md"),
                (5, "setup.md?x=1&y=2"),
                (5, "diagram.svg"),
                (7, "ref target.md"),
                (8, "https://example.com"),
            ],
        )

    def test_destinations_decode_references_before_escapes(self):
        # Like cmark, so an escaped ampersand still starts a reference.
        document = (
            "[a](c&copy;.md) [b](c\\&copy;.md) [c](a&copy.md) [d](&#92;*.md)\n\n"
            "[e]: c\\&copy;.md\n\n[e]\n"
        )
        self.assertEqual(
            _targets(document),
            [
                (1, "c©.md"),
                (1, "c\\©.md"),
                (1, "a&copy.md"),
                (1, "*.md"),
                (3, "c\\©.md"),
            ],
        )

    def test_html_attributes_keep_backslashes_and_bare_named_references(self):
        document = (
            '<a href="a\\_b.md">1</a> <a href="c&copy;.md">2</a> '
            '<a href="x.md?a=1&copy=2">3</a> <a href="&#99;.md">4</a>\n'
            '<a id="k&copy"></a><a id="m&#99"></a>\n'
        )
        self.assertEqual(
            _targets(document),
            [(1, "a\\_b.md"), (1, "c©.md"), (1, "x.md?a=1&copy=2"), (1, "c.md")],
        )
        self.assertEqual(_anchors(document), {"k&copy", "mc"})

    def test_angle_bracket_destinations_end_at_their_first_unescaped_bracket(self):
        # As in cmark, a backslash skips whatever follows it, even a line end.
        document = (
            "[r]: <missing\\>file.md>\n[s]: <a\\<b.md>\n\n"
            "[r] [s] [a](<b\\>c.md>) [d](<e\\<f.md>) [g](<h\\\ni.md>)\n"
        )
        self.assertEqual(
            _targets(document),
            [
                (1, "missing>file.md"),
                (2, "a<b.md"),
                (4, "b>c.md"),
                (4, "e<f.md"),
                (4, "h\\\ni.md"),
            ],
        )

    def test_destinations_drop_the_whitespace_that_github_drops(self):
        # cmark trims a Markdown destination, while GitHub drops only the
        # leading whitespace of an HTML one and so breaks trail.md's link.
        document = (
            "[Self](< README.md >) ![i](< logo.png >) [a](< >)\n\n"
            "[r]: < ref.md >\n\n[r]\n\n"
            '<a href=" lead.md">x</a> <img src="\tlogo.png"> <a href="trail.md ">y</a>\n'
        )
        self.assertEqual(
            _targets(document),
            [
                (1, "README.md"),
                (1, "logo.png"),
                (1, ""),
                (3, "ref.md"),
                (7, "lead.md"),
                (7, "logo.png"),
                (7, "trail.md "),
            ],
        )

    def test_the_first_attribute_of_each_name_hides_the_others(self):
        # As in HTML, whatever their case, even when the first has no value.
        document = (
            '<a href="README.md" href="missing.md" id="real" id="phantom">Guide</a>\n'
            '<a HREF="a.md" href="b.md" ID="c" id="d">x</a>\n'
            '<a href href="c.md" id ID="e">x</a>\n'
        )
        self.assertEqual(_targets(document), [(1, "README.md"), (2, "a.md"), (3, "")])
        self.assertEqual(_anchors(document), {"real", "c"})

    def test_attribute_values_lose_the_markers_of_their_containers(self):
        document = '> <a href="\n> README.md">Guide</a>\n\n> <img src="\n> logo.png">\n'
        self.assertEqual(_targets(document), [(1, "README.md"), (4, "logo.png")])

    def test_html_blocks_tokenize_tags_as_html_does(self):
        # Unlike inline raw HTML, which follows CommonMark's stricter rules.
        document = (
            '<div>\n<a href="missing.md" @click="noop">Go</a>\n</div>\n\n'
            '<div>\n<a href="nested.md" @click="<a href=\'ghost.md\'>">Go</a>\n</div>\n\n'
            '<div>\n<a href=README.md data-x=">" title=\'<a href="no.md">\'>Go</a>\n'
            "<img src=logo.png alt=x/>\n</div>\n\n"
            'Text <a href="inline.md" @click="noop">Go</a>\n\n'
            '<h2 @click="x">Clicked</h2>\n'
        )
        self.assertEqual(
            _targets(document),
            [(2, "missing.md"), (6, "nested.md"), (10, "README.md"), (11, "logo.png")],
        )
        self.assertEqual(_anchors(document), {"clicked"})

    def test_ordered_list_markers_need_ascii_digits(self):
        for digit in ("１", "١"):
            with self.subTest(digit=digit):
                self.assertEqual(
                    _targets(f"{digit}. item\n\n    [example](missing.md)\n"), []
                )

    def test_filtered_tags_render_as_text(self):
        # Without its tag, an attribute's markup renders.
        document = (
            '<iframe src="missing.html"></iframe>\n\n'
            'Text <iframe src="inline.html" id="f"></iframe> '
            '<SCRIPT src="s.js"></SCRIPT> end\n\n'
            "<iframe title=\"<a href='nested.md'>\">\n\n"
            '# A <script title="&amp;_x_">B</script> C\n'
        )
        self.assertEqual(_targets(document), [(5, "nested.md")])
        self.assertEqual(_anchors(document), {"a-script-title_x_bscript-c"})

    def test_only_the_html_that_github_keeps_holds_targets_and_anchors(self):
        document = (
            '<div href="div.md" id="kept">a</div> <source src="s.mp4"> '
            '<video src="v.mp4" id="gone"></video> <a src="a-src.png" href="a.md">a</a> '
            '<img href="img-href.md" src="j.png"> <custom id="c-id">c</custom> '
            '<small name="sm">s</small>\n'
        )
        self.assertEqual(_targets(document), [(1, "a.md"), (1, "j.png")])
        self.assertEqual(_anchors(document), {"kept"})

    def test_code_comments_and_escapes_hold_no_targets(self):
        document = (
            "`[a](a.md)` and ``[b `tick`](b.md)`` and \\[c](c.md).\n"
            "A `multi-line\n[d](d.md)` span.\n"
            "\n"
            "```\n[e](e.md)\n```\n"
            "\n"
            "<!-- [f](f.md) -->\n"
            "\n"
            "Text <!-- [g](g.md) --> and \\`[h](h.md)\\` and [i]().\n"
            "\n"
            "- ```text\n  [j](j.md)\n  ```\n"
            "1. > ~~~\n   > [k](k.md)\n   > ~~~\n"
        )
        self.assertEqual(_targets(document), [(11, "h.md"), (11, "")])

    def test_only_complete_definitions_that_start_a_block_are_targets(self):
        document = (
            "[a]: a.md trailing words\n"
            "\n"
            "Text\n"
            "[b]: b.md\n"
            "\n"
            '[c]: c.md "Title"\n'
            "[d]:\n"
            "  <d d.md>\n"
            "  'Title'\n"
        )
        self.assertEqual(_targets(document), [(6, "c.md"), (8, "d d.md")])

    def test_definitions_end_with_their_titles(self):
        # GitHub keeps a destination whose parentheses do not balance.
        document = (
            '[guide]: README.md "[literal](ghost.md)"\n\n'
            '[a]: existing.md\n  "Title"\n[b]: missing.md\n\n'
            '[c]: c.md\n"Title" text [d](inline.md)\n\n'
            "[bad]: ghost(.md\n\n[guide] [a] [b] [c] [bad]\n"
        )
        self.assertEqual(
            _targets(document),
            [
                (1, "README.md"),
                (3, "existing.md"),
                (5, "missing.md"),
                (7, "c.md"),
                (8, "inline.md"),
                (10, "ghost(.md"),
            ],
        )

    def test_titles_in_parentheses_hold_no_unescaped_parentheses(self):
        # Neither an inline link's nor a definition's, whose destination must
        # then end its line.
        document = (
            "[a](missing.md (title (nested))) [b](ok.md (t\\(x\\)))\n\n"
            "[c]: ghost.md (title (nested)\n\n[d]: own.md\n(title (nested)\n\n"
            "[c] [d]\n"
        )
        self.assertEqual(_targets(document), [(1, "ok.md"), (5, "own.md")])

    def test_container_markers_do_not_interrupt_inline_content(self):
        document = (
            "> [quoted](\n> quoted.md\n> )\n\n"
            "> [q]:\n> definition.md\n\n"
            "- [listed](\n  listed.md)\n\n[q]\n"
        )
        self.assertEqual(
            _targets(document),
            [(2, "quoted.md"), (6, "definition.md"), (9, "listed.md")],
        )

    def test_escaped_and_commented_tags_are_not_targets(self):
        document = (
            '\\<a href="escaped.md">x</a> and \\\\<a href="tag.md">y</a>\n\n'
            '<div>\n<!-- <a href="commented.md"> -->\n<a href="real.md">x</a>\n</div>\n'
        )
        self.assertEqual(_targets(document), [(1, "tag.md"), (5, "real.md")])

    def test_tag_attributes_and_titles_hold_no_targets(self):
        document = (
            'Text <a href="README.md" title="[example](missing.md)">Guide</a>\n\n'
            "[Guide](README.md \"<img src='missing.png'>\")\n\n"
            "[g]: README.md \"<img src='missing.png'>\"\n\n[g]\n"
        )
        self.assertEqual(
            _targets(document), [(1, "README.md"), (3, "README.md"), (5, "README.md")]
        )

    def test_definitions_end_with_their_paragraph(self):
        self.assertEqual(
            _targets("[a]:\n> quoted.md\n\n[b]:\n- listed.md\n\n[a] [b]\n"), []
        )

    def test_footnotes_render_their_contents_only_when_cited(self):
        document = (
            "Text[^note].\n\n"
            "[^note]: [Guide](footnote.md)\n\n"
            "    Continued [more](footnote-more.md).\n\n"
            "[^unused]: [x](unreferenced.md)\n"
        )
        self.assertEqual(
            _targets(document), [(3, "footnote.md"), (5, "footnote-more.md")]
        )

    def test_only_rendered_citations_cite_footnotes(self):
        document = (
            'Text \\[^a]. [Guide](README.md "[^b]") '
            '<a href="README.md" title="[^c]">x</a> `[^d]`\n\n'
            "[^a]: [x](a.md)\n[^b]: [x](b.md)\n[^c]: [x](c.md)\n[^d]: [x](d.md)\n"
        )
        self.assertEqual(_targets(document), [(1, "README.md"), (1, "README.md")])
        self.assertEqual(_anchors(document), set())

    def test_citations_in_footnotes_that_do_not_render_have_no_anchors(self):
        document = "Text[^a]\n\n[^a]: A [x](a.md)\n\n[^b]: B cites[^a] [y](b.md)\n"
        self.assertEqual(_targets(document), [(3, "a.md")])
        self.assertEqual(_anchors(document), {"fn-a", "fnref-a"})

    def test_citations_in_footnotes_that_do_not_render_still_count(self):
        # As in cmark-gfm, they render what they cite and advance its numbers.
        document = (
            "[^unused]: See[^nested].\n\n[^nested]: [x](missing.md)\n\n"
            "[^c]: C[^d]\n\n[^d]: D[^c] [y](cycle.md)\n"
        )
        self.assertEqual(_targets(document), [(3, "missing.md"), (7, "cycle.md")])
        self.assertEqual(
            _anchors(document), {"fn-nested", "fn-c", "fn-d", "fnref-c", "fnref-d"}
        )
        self.assertEqual(
            _anchors("[^unused]: Ref[^note]\n\nText[^note].\n\n[^note]: Body.\n"),
            {"fn-note", "fnref-note-2"},
        )

    def test_definitions_in_hidden_footnotes_serve_rendered_references(self):
        # Definitions belong to the whole document, wherever they are.
        document = (
            "[Guide][r] ![Logo][i]\n\n[^unused]: [r]: missing.md\n    [i]: logo.png\n\n"
            "[^also]: [u]: unused.md\n"
        )
        self.assertEqual(_targets(document), [(3, "missing.md"), (4, "logo.png")])

    def test_code_spans_start_only_where_inline_parsing_reaches(self):
        document = (
            '[Guide](README.md "Use `") and `[example](missing.md)`\n\n'
            'Text <a href="README.md" title="`">Guide</a> and `[example](missing.md)`\n\n'
            '## [Guide](README.md "Use `") and `x`\n'
        )
        self.assertEqual(
            _targets(document), [(1, "README.md"), (3, "README.md"), (5, "README.md")]
        )
        self.assertEqual(_anchors(document), {"guide-and-x"})

    def test_raw_html_blocks_end_with_their_containers(self):
        document = (
            "> <pre>\n> content\n\n# Outside\n\n[Outside](quoted.md)\n\n"
            "- <pre>\n  content\n\n# Listed\n\n[Listed](listed.md)\n"
        )
        self.assertEqual(_targets(document), [(6, "quoted.md"), (13, "listed.md")])
        self.assertEqual(_anchors(document), {"outside", "listed"})

    def test_only_delimited_tables_split_paragraphs(self):
        document = (
            "A `multiline\n| [example](missing.md)\ncode` span.\n\n"
            "| a | b |\n| --- | --- |\n| `x | [c](cell.md) |\n"
        )
        self.assertEqual(_targets(document), [(7, "cell.md")])

    def test_table_cells_parse_alone_up_to_the_header_count(self):
        document = (
            "| a | b |\n| --- | --- |\n"
            "| [x | ](missing.md) |\n"
            "| `literal | [broken](missing.md) ` |\n"
            "| x | y | [unused](missing.md) |\n"
            "| [x \\| y](escaped.md) | z |\n"
        )
        self.assertEqual(_targets(document), [(4, "missing.md"), (6, "escaped.md")])

    def test_tables_end_where_cmark_gfm_ends_them(self):
        # A table continues with each line that starts no other block, so no
        # setext underline or lazy line follows one, and a paragraph's lines
        # before its header hold no definitions.
        cases = {
            "| A | B |\n| --- | --- |\n| X | Y |\n---\n": [],
            "| A |\n| - |\n-\n": [],
            "| A |\n| - |\n===\n": [],
            "| A |\n| - |\n2. [x](x.md) | [y](y.md)\n": [(3, "x.md"), (3, "y.md")],
            "| A |\n| - |\n    [x](x.md) | [y](y.md)\n": [],
            "| A |\n| - |\n<span>\n[x](x.md) | [y](y.md)\n": [],
            "| A |\n| - |\n|\n[x](x.md) | [y](y.md)\n": [(4, "x.md"), (4, "y.md")],
            "> | a |\n> | - |\n| [b](b.md) | [c](c.md) |\n": [(3, "b.md"), (3, "c.md")],
            "[a]: x.md\n| h |\n| - |\n\n[a]\n": [],
            "a\n:-:\n[x](x.md) | [y](y.md)\n": [(3, "x.md")],
            (
                "| [a](a.md) | [b](b.md) |\n| - |\n| [c](c.md) |\n| - |\n"
                "| [d](d.md) | [e](e.md) |\n"
            ): [(1, "a.md"), (1, "b.md"), (3, "c.md"), (5, "d.md"), (5, "e.md")],
            "| H |\n| - |\n \n[a](a.md) | [b](b.md)\n": [(4, "a.md"), (4, "b.md")],
            "> | H |\n> | - |\n>  \n> [a](a.md) | [b](b.md)\n": [
                (4, "a.md"),
                (4, "b.md"),
            ],
        }
        for document, targets in cases.items():
            with self.subTest(document=document):
                self.assertEqual(_targets(document), targets)
                self.assertEqual(_anchors(document), set())
        # A line of spaces, which has no cells, ends a table too.
        self.assertEqual(_anchors("| H |\n| - |\n \nSection\n---\n"), {"section"})

    def test_links_do_not_nest(self):
        document = (
            "[outer [inner](README.md)](missing.md)\n\n"
            "[outer [inner][r]](missing.md)\n\n[r]: ref.md\n\n"
            "## [outer [inner](README.md)](missing.md)\n"
        )
        self.assertEqual(
            _targets(document), [(1, "README.md"), (5, "ref.md"), (7, "README.md")]
        )
        self.assertEqual(_anchors(document), {"outer-innermissingmd"})

    def test_image_descriptions_hold_no_targets(self):
        document = (
            "![Screenshot [details](missing.md)](logo.png)\n\n"
            "![a ![b](c.png)](d.png)\n\n"
            "[![alt](img.png)](page.md)\n\n"
            "## ![a [b](c.md)](d.png) Text\n"
        )
        self.assertEqual(
            _targets(document),
            [
                (1, "logo.png"),
                (3, "d.png"),
                (5, "img.png"),
                (5, "page.md"),
                (7, "d.png"),
            ],
        )
        self.assertEqual(_anchors(document), {"-text"})

    def test_definitions_in_block_quotes_and_list_items_are_targets(self):
        document = (
            "> [a]: quoted.md\n>\n> [a]\n\n"
            "- [b]: listed-b.md\n- [c]: listed-c.md\n\n"
            "Text\n> [d]: quote-start.md\n\n"
            "Text\n- [e]: item-start.md\n\n"
            "> text\n[f]: lazy.md\n\n"
            "> text\n> [g]: continued.md\n\n"
            "[a] [b] [c] [d] [e] [f] [g]\n"
        )
        self.assertEqual(
            _targets(document),
            [
                (1, "quoted.md"),
                (5, "listed-b.md"),
                (6, "listed-c.md"),
                (9, "quote-start.md"),
                (12, "item-start.md"),
            ],
        )

    def test_indented_code_holds_no_targets_unlike_list_item_content(self):
        document = (
            "Text\n\n    [a](code.md)\n\n"
            "\t[b](tab.md)\n\n"
            "- Item\n\n    [c](item.md)\n\n"
            "      [d](item-code.md)\n\n"
            "Text\n    [e](continued.md)\n\n"
            "# Title\n    [f](heading-code.md)\n\n"
            ">     [g](quoted-code.md)\n"
        )
        self.assertEqual(_targets(document), [(9, "item.md"), (14, "continued.md")])

    def test_tabs_indent_like_the_spaces_that_they_expand_to(self):
        tabbed = (
            "-\titem\n\n    # Inside\n\n    [a](a.md)\n\n"
            "1.\titem\n\n    [b](b.md)\n\n"
            "- \titem\n\n    [c](c.md)\n\n"
            "- item\n\n  \t# Tabbed\n\n"
            "> \t- item\n>\n>     [d](d.md)\n\n"
            "- ```\n  \t```\n  [e](e.md)\n"
        )
        spaced = (
            "-   item\n\n    # Inside\n\n    [a](a.md)\n\n"
            "1.  item\n\n    [b](b.md)\n\n"
            "-   item\n\n    [c](c.md)\n\n"
            "- item\n\n    # Tabbed\n\n"
            ">   - item\n>\n>     [d](d.md)\n\n"
            "- ```\n    ```\n  [e](e.md)\n"
        )
        for document in (tabbed, spaced):
            with self.subTest(document=document):
                self.assertEqual(
                    _targets(document),
                    [
                        (5, "a.md"),
                        (9, "b.md"),
                        (13, "c.md"),
                        (21, "d.md"),
                        (25, "e.md"),
                    ],
                )
                self.assertEqual(_anchors(document), {"inside", "tabbed"})

    def test_paragraph_continuations_hold_definitions_whatever_their_indent(self):
        self.assertEqual(
            _targets("[a]: a.md\n    [b]: b.md\n\n[a] [b]\n"),
            [(1, "a.md"), (2, "b.md")],
        )

    def test_a_blank_line_ends_a_list_item_that_holds_nothing(self):
        self.assertEqual(
            _targets("-\n\n    [a](code.md)\n\n- \n  [b](item.md)\n"), [(6, "item.md")]
        )

    def test_lazy_lines_start_ordered_lists_at_any_number(self):
        # Unlike a line within the block quote, which continues its paragraph.
        document = (
            "> A `multiline\n2. [example](missing.md)\ncode` span.\n\n"
            "> B `multiline\n> 2. [quoted](missing.md)\n> code` span.\n"
        )
        self.assertEqual(_targets(document), [(2, "missing.md")])

    def test_fences_close_only_within_their_container(self):
        document = (
            "```\n- ```\n[a](fence.md)\n```\n\n"
            "```\n> ```\n    ```\n[b](fence.md)\n```\n\n"
            "- ```\n  [c](item.md)\n  ```\n\n"
            "> ```\n> [d](quote.md)\n\n"
            "- ```\n  [e](item.md)\n\n"
            "[f](after.md)\n"
        )
        self.assertEqual(_targets(document), [(22, "after.md")])

    def test_html_blocks_hold_no_markdown_targets(self):
        document = (
            "<div>\n[a](raw.md)\n</div>\n\n"
            "<details>\n<summary>S</summary>\n\n[b](details.md)\n\n</details>\n\n"
            "<pre>\n[c](pre.md)\n\n[d](pre.md)\n</pre>\n\n"
            '<p align="center"><img src="logo.png"></p>\n\n'
            'Text\n<img src="inline.png">\n[e](paragraph.md)\n'
        )
        self.assertEqual(
            _targets(document),
            [
                (8, "details.md"),
                (18, "logo.png"),
                (21, "inline.png"),
                (22, "paragraph.md"),
            ],
        )

    def test_comments_hide_only_what_they_enclose(self):
        document = (
            '<!-- note --><a id="custom"></a>\n\n[Jump](#custom)\n\n'
            '<!--\nnote\n--><a href="after.md">x</a>\n\n'
            '<!-- a --><a href="x.md"><!-- b --></a>\n\n'
            '<div>\n<!-- a --!> <a href="bang.md">x</a> -->\n</div>\n'
        )
        self.assertEqual(
            _targets(document),
            [(3, "#custom"), (7, "after.md"), (9, "x.md"), (12, "bang.md")],
        )
        self.assertEqual(_anchors(document), {"custom"})

    def test_unclosed_comments_hide_what_renders_up_to_their_ends(self):
        # Only raw HTML ends a comment, and one still open at the end hides
        # the footnotes too.
        document = (
            "<div>\n<!-- open\n\n# Shown\n\n[a](hidden.md)\n\n-->\n\n"
            "<p>\n-->\n</p>\n\n# Shown\n\n[b](shown.md)\n\n"
            "> <!--\n> [c](quoted.md)\n\n[d](after.md)[^1]\n\n[^1]: [e](note.md)\n"
        )
        self.assertEqual(_targets(document), [(16, "shown.md")])
        self.assertEqual(_anchors(document), {"shown"})

    def test_comments_hide_the_definitions_that_no_reference_uses(self):
        document = (
            "[Go][used]\n\n<div>\n<!-- open\n\n[used]: used.md\n[unused]: missing.md\n"
        )
        self.assertEqual(_targets(document), [(6, "used.md")])

    def test_comment_blocks_run_to_the_line_that_holds_their_end(self):
        # A block that starts with "<!--" ends only at a line that holds "-->",
        # so after a comment that ends at "--!>", its Markdown renders as text
        # up to there, while its tags render.
        document = (
            "<!-- a --!>\n[a](block.md)\n\n# Hidden\n-->\n\n[b](after.md)\n\n"
            '<!-- c --!> <a href="tag.md">x</a>\n[d](text.md)\n-->\n'
        )
        self.assertEqual(_targets(document), [(7, "after.md"), (9, "tag.md")])
        self.assertEqual(_anchors(document), set())

    def test_cdata_instruction_and_declaration_blocks_run_to_their_ends(self):
        document = (
            "<![CDATA[\n\n[a](cdata.md)\n\n]]>\n\n"
            "<?php\n\n[b](pi.md)\n\n?>\n\n"
            "<!DOCTYPE html\n\n[c](declaration.md)\n\n>\n\n"
            "Text\n<![CDATA[\n[d](interrupted.md)\n]]>\n\n"
            "> <![CDATA[\n> [e](quoted.md)\n\n[f](after.md)\n\n"
            "<!doctype html\n\n[g](lowercase.md)\n"
        )
        self.assertEqual(_targets(document), [(27, "after.md"), (31, "lowercase.md")])

    def test_block_quote_markers_end_no_declaration_block(self):
        # A block ends at what its container holds, so only the ">" after a
        # quote's markers ends a declaration in it.
        document = "> <!DOCTYPE html\n> [a](hidden.md)\n> >\n\n[b](after.md)\n"
        self.assertEqual(_targets(document), [(5, "after.md")])

    def test_markup_other_than_comments_ends_at_the_first_angle_bracket(self):
        document = (
            '<div>\n<![CDATA[<a href="hidden.md">]]>\n'
            '<![CDATA[ a > b <a href="cdata.md">x</a> ]]>\n'
            '<?pi a > b <a href="pi.md">y</a> ?>\n</div>\n\n'
            'Text <![CDATA[ a > b <a href="inline.md">x</a> ]]> end\n\n'
            "A <!x [a](lowercase.md)> <!X[b](unspaced.md)> <!X [c](declared.md)>\n"
        )
        self.assertEqual(
            _targets(document),
            [
                (3, "cdata.md"),
                (4, "pi.md"),
                (7, "inline.md"),
                (9, "lowercase.md"),
                (9, "unspaced.md"),
            ],
        )

    def test_brackets_without_a_destination_are_not_links(self):
        self.assertEqual(
            _targets("- [ ] Task (later)\n[note] (aside) [x]: y z\n[^1]: Footnote\n"),
            [],
        )


class ProblemTests(unittest.TestCase):
    def test_annotations_escape_their_properties_and_message(self):
        self.assertEqual(
            checker.Problem("a,b:c.md", 3, "50% done\nnext").annotation(),
            "::error file=a%2Cb%3Ac.md,line=3::a,b:c.md:3: 50%25 done%0Anext",
        )
        self.assertEqual(
            checker.Problem("a.md", 0, "stale").annotation(),
            "::error file=a.md::a.md: stale",
        )


@unittest.skipIf(shutil.which("git") is None, "requires git")
class RepositoryTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.root = self.directory / "repository"
        self.root.mkdir()
        configuration = self.directory / "gitconfig"
        configuration.write_bytes(b"")
        # Keep the developer's Git configuration, such as commit signing or a
        # file system monitor, away from the fixture repositories.
        environment = patch.dict(
            os.environ,
            {
                "GIT_CEILING_DIRECTORIES": str(self.directory),
                "GIT_CONFIG_GLOBAL": str(configuration),
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_AUTHOR_NAME": "NVX",
                "GIT_AUTHOR_EMAIL": "nvx@example.com",
                "GIT_COMMITTER_NAME": "NVX",
                "GIT_COMMITTER_EMAIL": "nvx@example.com",
            },
        )
        environment.start()
        self.addCleanup(environment.stop)
        self.git(self.root, "init", "--quiet")

    def git(self, directory: Path, *arguments: str) -> str:
        return subprocess.run(
            ["git", *arguments],
            cwd=directory,
            capture_output=True,
            text=True,
            check=True,
        ).stdout

    def write(self, files: Mapping[str, str], *, track: bool = True) -> None:
        for name, text in files.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(text.encode())
        if track:
            self.git(self.root, "add", "--", *files)

    def check(
        self,
        files: Mapping[str, str],
        fixtures: Mapping[str, Collection[str]] = NO_FIXTURES,
    ) -> list[str]:
        self.write(files)
        problems = checker.check_repository(self.root, fixtures)
        return [str(problem) for problem in problems]

    def main(self, *, github_actions: bool = False) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        environment = {"GITHUB_ACTIONS": "true" if github_actions else "false"}
        with (
            patch.dict(os.environ, environment),
            patch.object(checker, "DOC_PATH_FIXTURES", NO_FIXTURES),
            contextlib.redirect_stdout(stdout),
            contextlib.redirect_stderr(stderr),
        ):
            status = checker.main(["--root", str(self.root)])
        return status, stdout.getvalue(), stderr.getvalue()

    def test_targets_resolve_to_tracked_files_directories_and_anchors(self):
        problems = self.check(
            {
                "README.md": (
                    "# NVX\n\n"
                    "[a](manual/guide.md#usage) [b](manual/design/) "
                    "[c](/manual/guide.md) [d](#nvx) [e](#top) [f](#)\n"
                    "[g](manual/a%20b.md) [h](manual/a\\_b.md) "
                    "[i](manual/guide.md#caf%C3%A9) [j](manual/design#design)\n"
                    "[k](tool.py#L2) [l](tool.py#L1-L2) "
                    "[m](manual/guide.md?plain=1#L3)\n"
                    "[n](https://example.com/x.md) [o](mailto:nvx@example.com) "
                    "[p](//example.com/x.md)\n"
                    '[q]() [r](<>) <a href="">s</a>\n'
                ),
                "manual/guide.md": (
                    "# Guide\n\n## Usage\n\n## Café\n\n[Back](../README.md#nvx)\n"
                ),
                "manual/design/README.md": "# Design\n",
                "manual/a b.md": "# Spaced\n",
                "manual/a_b.md": "# Escaped\n",
                "tool.py": "print(1)\nprint(2)\n",
            }
        )
        self.assertEqual(problems, [])

    def test_broken_targets_name_what_is_missing(self):
        self.write({"build/output.md": "# Output\n"}, track=False)
        problems = self.check(
            {
                "README.md": "# NVX\n",
                "tool.py": "pass\n",
                "manual/guide.md": (
                    "# Guide\n\n"
                    "[a](missing.md) [b](../build/output.md) [c](../../outside.md)\n"
                    "[d](guide.md/) [e](#usage) [f](../README.md#install)\n"
                    "[g](../tool.py#main) [h](../tool.py#L3) [j](../manual#guide)\n"
                ),
            }
        )
        self.assertEqual(
            problems,
            [
                "manual/guide.md:3: ../../outside.md leaves the repository",
                "manual/guide.md:3: ../build/output.md: build/output.md is not tracked",
                "manual/guide.md:3: missing.md: manual/missing.md does not exist",
                "manual/guide.md:4: #usage: manual/guide.md has no heading or anchor "
                "#usage",
                "manual/guide.md:4: ../README.md#install: README.md has no heading or "
                "anchor #install",
                "manual/guide.md:4: guide.md/: manual/guide.md is not a directory",
                "manual/guide.md:5: ../manual#guide: manual has no README",
                "manual/guide.md:5: ../tool.py#L3: tool.py ends at line 1",
                "manual/guide.md:5: ../tool.py#main: tool.py has only line anchors "
                "such as #L1",
            ],
        )

    def test_image_anchors_select_nothing_to_check(self):
        # As GitHub's #gh-dark-mode-only and #gh-light-mode-only do, unless a
        # rendered link uses the image's definition too.
        problems = self.check(
            {
                "dark.png": "png\n",
                "tool.py": "pass\n",
                "README.md": (
                    "![Light](dark.png#gh-light-mode-only) "
                    '<img src="dark.png#gh-dark-mode-only">\n'
                    "![Logo][logo] ![Missing](missing.png#gh-light-mode-only)\n"
                    "[Source](tool.py#gh-dark-mode-only) ![x][both] [y][both]\n\n"
                    "[logo]: dark.png#gh-dark-mode-only\n"
                    "[both]: dark.png#gh-dark-mode-only\n\n"
                    "![Themed][themed]\n\n[themed]: dark.png#gh-dark-mode-only\n\n"
                    "[^unused]: [Source][themed]\n"
                ),
            }
        )
        self.assertEqual(
            problems,
            [
                "README.md:2: missing.png#gh-light-mode-only: missing.png does not "
                "exist",
                "README.md:3: tool.py#gh-dark-mode-only: tool.py has only line "
                "anchors such as #L1",
                "README.md:6: dark.png#gh-dark-mode-only: dark.png has only line "
                "anchors such as #L1",
            ],
        )

    def test_anchors_match_fragments_in_lowercase(self):
        # As GitHub's script looks them up, so an uppercase id is out of reach.
        problems = self.check(
            {
                "README.md": (
                    '## Usage\n\n<a id="Custom"></a>\n\n'
                    "[a](#Usage) [b](#Custom) [c](#custom)\n"
                ),
            }
        )
        self.assertEqual(
            problems,
            [
                "README.md:5: #Custom: README.md has no heading or anchor #Custom",
                "README.md:5: #custom: README.md has no heading or anchor #custom",
            ],
        )

    def test_fragments_may_hold_the_prefix_of_the_ids_that_github_renders(self):
        # GitHub's script prefixes a fragment with user-content- only when it
        # lacks the prefix, which GitHub gives every id that lacks it too.
        problems = self.check(
            {
                "README.md": (
                    "## Usage\n\n## User Content Foo\n\n"
                    '<a id="user-content-custom"></a>\n\n'
                    "[a](#user-content-usage) [b](#custom) [c](#user-content-custom)\n"
                    "[d](#user-content-foo) [e](#user-content-user-content-foo)\n"
                ),
            }
        )
        self.assertEqual(
            problems,
            [
                "README.md:8: #user-content-foo: README.md has no heading or anchor "
                "#user-content-foo",
            ],
        )

    def test_line_anchors_need_ascii_digits(self):
        problems = self.check(
            {"tool.py": "pass\n", "README.md": "[a](tool.py#L1) [b](tool.py#L١)\n"}
        )
        self.assertEqual(
            problems,
            ["README.md:1: tool.py#L١: tool.py has only line anchors such as #L1"],
        )

    def test_case_mismatches_name_the_tracked_path(self):
        problems = self.check(
            {
                "README.md": "[a](readme.md) [b](Manual/Guide.md)\n",
                "manual/guide.md": "# Guide\n",
            }
        )
        self.assertEqual(
            problems,
            [
                "README.md:1: Manual/Guide.md: Manual/Guide.md differs in case from "
                "manual/guide.md",
                "README.md:1: readme.md: readme.md differs in case from README.md",
            ],
        )

    def test_directory_anchors_resolve_in_the_readme_whatever_its_case(self):
        problems = self.check(
            {
                "README.md": "[a](manual/#usage) [b](manual#usage) [c](manual/#gone)\n",
                "manual/readme.md": "# Usage\n",
            }
        )
        self.assertEqual(
            problems,
            [
                "README.md:1: manual/#gone: manual/readme.md has no heading or anchor "
                "#gone"
            ],
        )

    def test_submodule_targets_resolve_at_the_pinned_revision(self):
        upstream = self.directory / "upstream"
        upstream.mkdir()
        self.git(upstream, "init", "--quiet")
        (upstream / "Guide.md").write_bytes(b"# Guide\n\n## Build\n")
        self.git(upstream, "add", "Guide.md")
        self.git(upstream, "commit", "--quiet", "--no-verify", "--message", "Guide")
        commit = self.git(upstream, "rev-parse", "HEAD").strip()
        self.git(
            self.root, "update-index", "--add", "--cacheinfo", f"160000,{commit},sub"
        )
        (self.root / "sub").mkdir()
        files = {"README.md": "[a](sub) [b](sub/Guide.md#build) [c](sub/Missing.md)\n"}
        with self.subTest(checked_out=False):
            self.assertEqual(self.check(files), [])
        self.git(self.root, "clone", "--quiet", str(upstream), "sub")
        # The check reads the pinned revision, not the submodule's work tree.
        (self.root / "sub" / "Guide.md").write_bytes(b"# Renamed\n")
        with self.subTest(checked_out=True):
            self.assertEqual(
                self.check(files),
                ["README.md:1: sub/Missing.md: sub/Missing.md does not exist"],
            )

    def test_doc_paths_that_files_mention_resolve_from_the_root(self):
        problems = self.check(
            {
                "doc/guide.md": "# Guide\n\n## Usage\n",
                "doc/c++.md": "# C++\n",
                "tool.py": (
                    "# See doc/guide.md#usage, doc/guide.md., and doc/.\n"
                    "# Read doc/missing.md and doc/guide.md#install.\n"
                    "SHARED = '/usr/share/doc/missing.md'  # or rustdoc/missing.md\n"
                    "PATTERNS = ('doc/{name}.md', 'doc/guide-%s.md', 'doc/*.md')\n"
                    "OTHERS = ('doc/c++.md', 'doc/missing.md@HEAD', 'doc/a(b).md')\n"
                ),
                "manual/README.md": (
                    "[Guide](doc/guide.md) is relative, unlike doc/guide.md#usage.\n"
                ),
            }
        )
        self.assertEqual(
            problems,
            [
                "manual/README.md:1: doc/guide.md: manual/doc/guide.md does not exist",
                "tool.py:2: doc/guide.md#install: doc/guide.md has no heading or "
                "anchor #install",
                "tool.py:2: doc/missing.md: doc/missing.md does not exist",
            ],
        )

    def test_fixtures_hold_missing_doc_paths_until_no_longer_needed(self):
        problems = self.check(
            {
                "doc/guide.md": "# Guide\n",
                "test_tool.py": "CHANGED = ('doc/missing.md', 'doc/guide.md')\n",
            },
            fixtures={
                "test_tool.py": {"doc/guide.md", "doc/missing.md"},
                "test_gone.py": {"doc/missing.md"},
            },
        )
        self.assertEqual(
            problems,
            [
                "test_gone.py: DOC_PATH_FIXTURES lists this file, which is not tracked",
                "test_tool.py: DOC_PATH_FIXTURES lists doc/guide.md, which this file "
                "no longer mentions as a missing document",
            ],
        )

    def test_the_check_may_mention_the_fixtures_that_it_lists(self):
        files = {
            "scripts/check_markdown_links.py": "FIXTURES = ('doc/missing.md',)\n",
            "test_tool.py": "CHANGED = 'doc/missing.md'\n",
        }
        fixtures = {"test_tool.py": {"doc/missing.md"}}
        own = self.root / "scripts" / "check_markdown_links.py"
        with self.subTest(own=False):
            self.assertEqual(
                self.check(files, fixtures),
                [
                    "scripts/check_markdown_links.py:1: doc/missing.md: "
                    "doc/missing.md does not exist"
                ],
            )
        with self.subTest(own=True), patch.object(checker, "__file__", str(own)):
            self.assertEqual(self.check(files, fixtures), [])

    def test_command_passes_silently_when_every_target_resolves(self):
        self.write({"README.md": "# NVX\n\n[NVX](#nvx)\n"})
        self.assertEqual(self.main(), (0, "", ""))

    def test_command_reports_each_problem_and_fails(self):
        self.write({"README.md": "[a](a.md)\n"})
        self.assertEqual(
            self.main(),
            (
                1,
                "README.md:1: a.md: a.md does not exist\n",
                "error: 1 broken link(s) or doc/ path(s)\n",
            ),
        )

    def test_command_annotates_problems_in_github_actions(self):
        self.write({"README.md": "[a](a.md)\n"})
        status, stdout, _ = self.main(github_actions=True)
        self.assertEqual(status, 1)
        self.assertEqual(
            stdout,
            "::error file=README.md,line=1::README.md:1: a.md: a.md does not exist\n",
        )

    def test_command_fails_outside_a_git_work_tree(self):
        self.root = self.directory / "plain"
        self.root.mkdir()
        status, stdout, stderr = self.main()
        self.assertEqual((status, stdout), (2, ""))
        self.assertIn("git ls-files failed", stderr)


class QualityGateTests(unittest.TestCase):
    def test_check_quality_runs_the_link_check(self):
        action = (
            REPO_ROOT / ".github" / "actions" / "check-quality" / "action.yml"
        ).read_text(encoding="utf-8")
        self.assertIn("run: python scripts/check_markdown_links.py\n", action)


if __name__ == "__main__":
    unittest.main()

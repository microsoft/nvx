#!/usr/bin/env python3
"""Check the relative links and anchors of the tracked Markdown files.

Every relative link and image target of a tracked Markdown file outside the
submodules, whether an inline link, a reference definition, or the href of an
HTML link or src of an image, must name a tracked file or directory, and the
anchor of every link must match, in lowercase as GitHub looks it up, a heading
slug that GitHub generates, or the id or name of an HTML element that GitHub
keeps, in the target document; other files have only line anchors such as
#L10. An image's anchor, such as GitHub's #gh-dark-mode-only, selects nothing
to check, and an empty target links to its own document. As on GitHub, code
blocks, code spans, HTML blocks, and front matter hold no Markdown links, an
HTML comment hides what renders up to its end, the tags that GitHub filters
render as text, and block quotes and list items decide where the blocks inside
them end. A link into a submodule resolves against its pinned revision when the
submodule is checked out and is skipped otherwise.

Every doc/ path, with any anchor, that a tracked text file mentions must
resolve the same way from the repository root, except the test fixtures that
DOC_PATH_FIXTURES lists.
"""

from __future__ import annotations

import argparse
import bisect
import html.entities
import os
import posixpath
import re
import subprocess
import sys
import unicodedata
import urllib.parse
from collections.abc import Callable, Collection, Iterable, Iterator, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import NamedTuple

REPO_ROOT = Path(__file__).resolve().parents[1]

# Test fixtures that mention doc/ paths that do not exist, by the file that
# mentions them. An entry that its file no longer needs fails the check, and
# this file may mention every path that the entries list.
DOC_PATH_FIXTURES: Mapping[str, Collection[str]] = {
    "scripts/test_markdown_links.py": frozenset({"doc/guide.md", "doc/missing.md"}),
    "scripts/test_nvx_tools.py": frozenset({"doc/input.patch", "doc/notes.md"}),
}

_ASCII_PUNCTUATION = frozenset("!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~")
_ESCAPED = re.compile(r"\\([!-/:-@\[-`{-~])")
# Character references, as cmark decodes them in Markdown, and as GitHub
# decodes them in HTML attributes, where a number needs no semicolon.
_ENTITY = re.compile(
    r"&(?:#[0-9]{1,7}|#[xX][0-9A-Fa-f]{1,6}|[A-Za-z][A-Za-z0-9]{1,30});"
)
_HTML_ENTITY = re.compile(r"&(?:#[0-9]+;?|#[xX][0-9A-Fa-f]+;?|[A-Za-z][A-Za-z0-9]*;)")
_EXTERNAL = re.compile(r"[A-Za-z][A-Za-z0-9+.-]*:|//")
_SPACE = re.compile(r"[ \t]*(?:\n[ \t]*)?")
_LINE_BREAK = re.compile(r"\n[ \t]*")
_BLANK_LINE = re.compile(r"\n[ \t]*(?:\n|$)")
_BACKTICKS = re.compile(r"`+")
# Block syntax, which starts at a line's first character that is not a space
# or tab, fewer than four columns into its container.
_LIST_MARKER = re.compile(r"(?:[-+*]|([0-9]{1,9})[.)])(?=[ \t]|$)")
_FENCE_OPENER = re.compile(r"(`{3,}|~{3,})(.*)")
_FENCE_CLOSER = re.compile(r"(`{3,}|~{3,})[ \t]*")
_THEMATIC_BREAK = re.compile(r"(?:\*[ \t]*){3,}|(?:-[ \t]*){3,}|(?:_[ \t]*){3,}")
_ATX_HEADING = re.compile(r"#{1,6}(?:[ \t]+(.*))?")
_CLOSING_SEQUENCE = re.compile(r"(?:^|[ \t]+)#+[ \t]*$")
_SETEXT_UNDERLINE = re.compile(r"(?:=+|-+)[ \t]*")
# A table needs a delimiter row whose cells match its header row's.
_TABLE_DELIMITER = re.compile(
    r"[ \t]*\|?(?:[ \t]*:?-+:?[ \t]*\|)*[ \t]*:?-+:?[ \t]*\|?[ \t]*"
)
_FOOTNOTE = re.compile(r"\[\^([^\]\s]+)\]:")
_FOOTNOTE_LABEL = re.compile(r"\^([^\]\s]+)")
# Raw HTML, after CommonMark: tags are atomic inline, and their attribute
# values are literal.
_TAG_ATTRIBUTE = re.compile(
    r"""\s+([A-Za-z_:][A-Za-z0-9_.:-]*)"""
    r"""(?:\s*=\s*(?:([^\s"'=<>`]+)|'([^']*)'|"([^"]*)"))?"""
)
_OPEN_TAG = re.compile(
    r"<[A-Za-z][A-Za-z0-9-]*(?:" + _TAG_ATTRIBUTE.pattern + r")*\s*/?>"
)
_CLOSE_TAG = re.compile(r"</[A-Za-z][A-Za-z0-9-]*\s*>")
# The start conditions of the HTML blocks of types 1 to 6, whose lines GitHub
# passes through without parsing Markdown, as cmark finds them, and the end
# conditions of types 1 to 5, which, unlike the others, no blank line ends.
_HTML_STARTS = (
    re.compile(r"<(?:pre|script|style|textarea)(?:[ \t>]|$)", re.I),
    re.compile("<!--"),
    re.compile(r"<\?"),
    re.compile("<![A-Z]"),
    re.compile(r"<!\[CDATA\["),
    re.compile(
        r"</?(?:address|article|aside|base|basefont|blockquote|body|caption"
        r"|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset"
        r"|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr"
        r"|html|iframe|legend|li|link|main|menu|menuitem|nav|noframes|ol"
        r"|optgroup|option|p|param|search|section|summary|table|tbody|td"
        r"|tfoot|th|thead|title|tr|track|ul)(?:[ \t>]|/>|$)",
        re.I,
    ),
)
_HTML_ENDS = (
    re.compile(r"</(?:pre|script|style|textarea)>", re.I),
    # A comment block ends only at "-->", though the comment in it can end
    # earlier, at "--!>", as _COMMENT_END finds.
    re.compile("-->"),
    re.compile(r"\?>"),
    re.compile(">"),
    re.compile(r"\]\]>"),
)
_HTML_TAG_LINE = re.compile(
    "(?:" + _OPEN_TAG.pattern + "|" + _CLOSE_TAG.pattern + r")[ \t]*"
)
# What an HTML parser finds in raw HTML: a comment ends at "-->" or "--!>", or
# at once as "<!-->" or "<!--->", while other markup that starts with "<!",
# "<?", or "</" but is no tag, such as a CDATA section, ends at the first ">".
_MARKUP = re.compile(r"<(?:!--|[!?/A-Za-z])")
_COMMENT_END = re.compile("--!?>")
_ABRUPT_COMMENT_END = re.compile("-?>")
# What the marks of marked text, as parse_markdown makes it, render.
_UNMARK = str.maketrans({"\x00": "", "\x01": " ", "\x02": "  ", "\x03": "   "})
# GitHub escapes the "<" of these tags, which then render as text.
_FILTERED_TAG = re.compile(
    r"</?(?:iframe|noembed|noframes|plaintext|script|style|textarea|title|xmp)"
    r"(?=[ \t\n\v\f\r>]|/>)",
    re.I,
)
# What GitHub's sanitizer keeps: the targets of links and images, and the
# elements whose id and name attributes are anchors.
_TARGET_ATTRIBUTES = frozenset((("a", "href"), ("img", "src")))
_ANCHOR_ELEMENTS = frozenset(
    "a b blockquote br code dd del details div dl dt em h1 h2 h3 h4 h5 h6 hr i"
    " img ins kbd li mark ol p picture pre q rp rt ruby s samp section source"
    " span strike strong sub summary sup table tbody td tfoot th thead tr tt"
    " ul var".split()
)
# The HTML headings, whose text GitHub slugs too.
_HEADINGS = frozenset(("h1", "h2", "h3", "h4", "h5", "h6"))
# GitHub slugs the text of each heading in the tree that libxml2's HTML parser
# builds from its HTML, as an HTML5 parser rebuilds it. libxml2 closes the
# current element while a start tag that this table lists for it follows, as
# its htmlStartClose table does for each element that can stay open here.
_CLOSED_BY: Mapping[str, frozenset[str]] = {
    old: frozenset(new.split())
    for old, new in {
        "a": "a fieldset table td th",
        "address": "dd dl dt form li ul",
        "b": "center p td th",
        "big": "p",
        "caption": "col colgroup tbody tfoot thead tr",
        "colgroup": "colgroup tbody tfoot thead tr",
        "dd": "dt",
        "dir": "dd dl dt form ul",
        "dl": "form li",
        "dt": "dd dl",
        "font": "center td th",
        "form": "form",
        **dict.fromkeys(_HEADINGS, "fieldset form li p table"),
        "i": "center p td th",
        "legend": "fieldset",
        "li": "li",
        "listing": "dd dl dt fieldset form li table ul",
        "menu": "dd dl dt form ul",
        "ol": "form",
        "option": "optgroup option",
        "p": "address blockquote body caption center col colgroup dd dir div dl dt"
        " fieldset form frameset h1 h2 h3 h4 h5 h6 head hr li listing menu ol p pre"
        " table tbody td tfoot th tr ul",
        "pre": "dd dl dt fieldset form li table ul",
        "s": "p",
        "small": "p",
        "span": "td th",
        "strike": "p",
        "tbody": "tbody tfoot",
        "td": "tbody td tfoot th tr",
        "tfoot": "tbody",
        "th": "tbody td tfoot th tr",
        "thead": "tbody tfoot",
        "tr": "tbody tfoot tr",
        "tt": "p",
        "u": "p td th",
        "ul": "address form menu pre",
    }.items()
}
# An end tag closes the elements after its own only if none of them has a
# higher priority than its own, of 100 unless listed here; libxml2 ignores it
# otherwise, as it does an end tag of no open element.
_END_PRIORITY: Mapping[str, int] = {
    "div": 150,
    "td": 160,
    "th": 160,
    "tr": 170,
    "thead": 180,
    "tbody": 180,
    "tfoot": 180,
    "table": 190,
}
# The elements that libxml2 never leaves open, and those whose start tags it
# discards after they close others.
_VOID_ELEMENTS = frozenset(
    "area base basefont br col frame hr img input isindex link meta param".split()
)
_DISCARDED_ELEMENTS = frozenset(("body", "head", "html"))
# The elements that GitHub renders for block quotes, the items of bullet and
# ordered lists, and footnote definitions, around what they hold.
_CONTAINER_ELEMENTS: Mapping[str, tuple[str, ...]] = {
    "quote": ("blockquote",),
    "item": ("ul", "li"),
    "ordered": ("ol", "li"),
    "footnote": ("section", "ol", "li"),
}
_TAG_START = re.compile("</?[A-Za-z]")
# The prefix that GitHub gives each id and name that lacks it, and that its
# script adds to each fragment that lacks it before it looks one up.
_PREFIX = "user-content-"
_LABEL = re.compile(r"\[(?!\^)((?:[^\[\]\\]|\\.){1,999})\]:")
_LINE_END = re.compile(r"[ \t]*(?=\n|\Z)")
_README_NAMES = frozenset(("readme.md", "readme.markdown"))
_AUTOLINK = re.compile(r"<[A-Za-z][A-Za-z0-9+.-]{1,31}:[^<>\s]*>")
_EMAIL_AUTOLINK = re.compile(
    r"<[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?"
    r"(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*>"
)
# Comments, processing instructions, declarations, and CDATA sections.
_RAW_HTML = re.compile(
    r"<!-{2,3}>|<!--.*?-->|<\?.*?\?>|<![A-Z]+[ \t\n\v\f\r][^>]*>"
    r"|<!\[CDATA\[.*?\]\]>",
    re.S,
)
# GitHub keeps letters, marks, digits, connector punctuation such as "_",
# hyphens, and spaces, which become hyphens.
_SLUG_CATEGORIES = frozenset(
    ("Ll", "Lm", "Lo", "Lt", "Lu", "Mc", "Me", "Mn", "Nd", "Nl", "Pc")
)
_LINE_ANCHOR = re.compile(r"L([0-9]+)(?:C[0-9]+)?(?:-L([0-9]+)(?:C[0-9]+)?)?")
_DOC_PATH = re.compile(r"(?<![\w./\\-])doc/[\w./-]*(?:#[\w.-]*)?")
# Whitespace or one of these ends a doc/ path. Any other character, such as the
# "*" of a pattern, the "{" of a template, or the "+" of a file name that the
# expression does not match, leaves the path ambiguous, so it is not checked.
_DOC_PATH_ENDS = frozenset("\"'`,;:!?)]}>|\\")


class CheckError(RuntimeError):
    """Raised when the tracked files of the work tree cannot be listed."""


@dataclass(frozen=True, order=True)
class Problem:
    """A link target or a doc/ path that does not resolve."""

    path: str
    line: int
    message: str

    def __str__(self) -> str:
        location = f"{self.path}:{self.line}" if self.line else self.path
        return f"{location}: {self.message}"

    def annotation(self) -> str:
        """Formats the problem as a GitHub Actions error annotation."""
        properties = f"file={_escape_property(self.path)}"
        if self.line:
            properties += f",line={self.line}"
        return f"::error {properties}::{_escape_data(str(self))}"


@dataclass(frozen=True)
class Link:
    """A link or image target, as a browser receives it, and its span in the
    text."""

    line: int
    start: int
    end: int
    target: str
    image: bool = False


@dataclass(frozen=True)
class MarkdownDocument:
    """The link targets and anchors of a Markdown document, each anchor being
    the id of an element that GitHub renders, without its user-content-
    prefix."""

    links: tuple[Link, ...]
    anchors: frozenset[str]


def _escape_data(value: str) -> str:
    return value.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def _escape_property(value: str) -> str:
    return _escape_data(value).replace(":", "%3A").replace(",", "%2C")


def normalize_newlines(text: str) -> str:
    return text.replace("\r\n", "\n").replace("\r", "\n")


def github_slug(text: str) -> str:
    """Returns the anchor that GitHub derives from a heading's text."""
    return "".join(
        "-" if char == " " else char
        for char in text.lower()
        if char in " -" or unicodedata.category(char) in _SLUG_CATEGORIES
    )


def parse_markdown(text: str) -> MarkdownDocument:
    """Finds the link targets and anchors of Markdown as GitHub renders it.

    Offsets and line numbers refer to the text after normalize_newlines.
    """
    text = normalize_newlines(text)
    lines = text.split("\n")
    starts = [0, *(match.end() for match in re.finditer("\n", text))]
    layout = _layout(lines)
    # Blank what renders no Markdown and, as CommonMark strips them before
    # parsing inline content, the block quote and list item markers. The
    # text of HTML headings keeps the indentation after those markers, so
    # marked holds each marker, and each control character that renders
    # nothing in a slug, as "\x00", and each tab that a container consumed in
    # part as the control character whose code counts the spaces it renders.
    chars = list(text)
    marks = ["\x00" if char < "\x04" else char for char in text]
    for number, line in enumerate(lines):
        end = len(line) if layout.hidden[number] else layout.content[number]
        _blank(chars, starts[number], starts[number] + end)
        prefix = layout.prefixes[number]
        marks[starts[number] : starts[number] + prefix] = ["\x00"] * prefix
        if layout.pads[number] and not layout.hidden[number]:
            marks[starts[number] + prefix] = chr(layout.pads[number])
    blanked = "".join(chars)
    marked = "".join(marks)
    definitions, firsts = _definitions(blanked, lines, layout, starts)
    labels = {definition[0] for definition in definitions}
    footnotes: dict[str, str] = {}
    for label, _, _ in layout.footnotes:
        footnotes.setdefault(label.casefold(), label)
    # The raw HTML, by the index of each HTML block where it ends, the inline
    # links, each as the offset where it renders and the span of its
    # destination, the footnote citations, and the block and the span of the
    # inline content that holds each item of inline raw HTML, by where the
    # item ends.
    regions = {
        starts[end - 1] + len(lines[end - 1]): index
        for index, (kind, _, end) in enumerate(layout.blocks)
        if kind == "html"
    }
    raw = [(starts[layout.blocks[index][1]], end) for end, index in regions.items()]
    inline: list[tuple[int, int, int, bool]] = []
    citations: list[tuple[int, str]] = []
    spans: dict[int, tuple[int, int, int]] = {}
    # Each use of a reference, as its offset, whether an image uses it, and
    # its label, since a target that only rendered images use holds no anchor.
    uses: list[tuple[int, bool, str]] = []
    for index, start, end in _inline_ranges(lines, layout, starts, firsts):
        content = blanked[start:end]
        items = _inline(content, labels, footnotes)
        for item in items:
            if item.kind in ("tag", "html"):
                raw.append((start + item.start, start + item.end))
                spans[start + item.end] = (index, start, end)
            elif item.kind == "footnote":
                label = blanked[start + item.start + 2 : start + item.close]
                citations.append((start + item.start, label.casefold()))
            elif item.destination is not None:
                first, last = item.destination
                inline.append(
                    (
                        start + item.start,
                        start + first,
                        start + last,
                        item.kind == "image",
                    )
                )
            elif item.label is not None:
                uses.append((start + item.start, item.kind == "image", item.label))
    citations.sort()
    # GitHub renders the cited footnotes after the rest of the document, in
    # the order of their first citations, and the others not at all.
    ranks: dict[str, int] = {}
    for _, key in citations:
        ranks.setdefault(key, len(ranks) + 1)
    rank = [0] * len(lines)
    for label, first, end in layout.footnotes:
        rank[first:end] = [ranks.get(label.casefold(), -1)] * (end - first)

    def place(offset: int) -> tuple[int, int]:
        return rank[bisect.bisect_right(starts, offset) - 1], offset

    tags, comments = _raw_html(
        blanked,
        sorted(
            (span for span in raw if place(span[0])[0] >= 0),
            key=lambda span: place(span[0]),
        ),
    )
    hidden = [
        (place(start), (len(ranks) + 1, 0) if end is None else place(end))
        for start, end in comments
    ]
    openings = [start for start, _ in hidden]

    def shown(offset: int) -> bool:
        """Whether GitHub renders what is at offset, outside any comment."""
        at = place(offset)
        index = bisect.bisect_right(openings, at) - 1
        return at[0] >= 0 and (index < 0 or hidden[index][1] <= at)

    attributes = [
        (tag.name, key, start, end)
        for tag in tags
        for key, start, end in tag.attributes
    ]
    references: dict[bool, set[str]] = {True: set(), False: set()}
    for offset, image, label in uses:
        if shown(offset):
            references[image].add(label)
    # A definition counts where it renders, as do the definitions that the
    # rendered references use, wherever they are, such as in hidden footnotes.
    # The text of each target is blanked, without the markers of the
    # containers that its lines continue.
    targets = [
        (
            start,
            end,
            _destination(blanked[start:end]),
            label in references[True] and label not in references[False],
        )
        for label, start, end, _ in definitions
        if shown(start) or label in references[True] | references[False]
    ]
    targets.extend(
        (start, end, _destination(blanked[start:end]), image)
        for offset, start, end, image in inline
        if shown(offset)
    )
    targets.extend(
        (start, end, _url(blanked[start:end]), element == "img")
        for element, name, start, end in attributes
        if (element, name) in _TARGET_ATTRIBUTES
    )
    links = tuple(
        Link(bisect.bisect_right(starts, start), start, end, target, image)
        for start, end, target, image in sorted(targets)
    )
    anchors = {
        _attribute(blanked[start:end]).removeprefix(_PREFIX)
        for element, name, start, end in attributes
        if element in _ANCHOR_ELEMENTS and name in ("id", "name")
    }
    counts: dict[str, int] = {}
    for offset, key in citations:
        counts[key] = counts.get(key, 0) + 1
        if shown(offset):
            suffix = "" if counts[key] == 1 else f"-{counts[key]}"
            anchors.add(f"fnref-{footnotes[key]}{suffix}")
    anchors.update(
        f"fn-{footnotes[label.casefold()]}"
        for label, first, _ in layout.footnotes
        if shown(starts[first])
    )
    # GitHub numbers the repeated slugs of Markdown and HTML headings alike,
    # in the order that it renders them.
    texts = _Texts(
        lines,
        starts,
        layout,
        blanked,
        marked,
        firsts,
        labels,
        ranks,
        regions,
        spans,
        place,
        shown,
        _tight(layout, firsts),
    )
    headings: list[tuple[tuple[int, int], str]] = []
    for index, (_, first, _) in enumerate(layout.blocks):
        offset = starts[first] + layout.content[first]
        if shown(offset) and (heading := texts.markdown(index)) is not None:
            headings.append((place(offset), heading))
    for tag in tags:
        if tag.name in _HEADINGS and (heading := texts.html(tag)) is not None:
            headings.append((place(tag.start), heading))
    anchors.update(_slugs(heading for _, heading in sorted(headings)))
    anchors.discard("")
    return MarkdownDocument(links, frozenset(anchors))


def _blank(chars: list[str], start: int, end: int) -> None:
    """Replaces the characters in a span with spaces, keeping line endings."""
    chars[start:end] = (" " if char != "\n" else char for char in chars[start:end])


def _definitions(
    blanked: str, lines: Sequence[str], layout: _Layout, starts: Sequence[int]
) -> tuple[list[tuple[str, int, int, int]], list[int]]:
    """Parses the link reference definitions at the start of each paragraph,
    within that paragraph.

    Returns the definitions, and the first line of each block after them,
    where its inline content starts.
    """
    definitions: list[tuple[str, int, int, int]] = []
    firsts: list[int] = []
    for kind, first, end in layout.blocks:
        number = first
        if kind in ("paragraph", "setext"):
            base = starts[first]
            paragraph = blanked[base : starts[end - 1] + len(lines[end - 1])]
            while number < end:
                offset = starts[number] + layout.content[number] - base
                definition = _definition(paragraph, offset)
                if definition is None:
                    break
                label, start, stop, last = definition
                definitions.append((label, base + start, base + stop, base + last))
                number = bisect.bisect_right(starts, base + last)
        firsts.append(number)
    return definitions, firsts


def _definition(text: str, index: int) -> tuple[str, int, int, int] | None:
    """Parses a link reference definition at index of text whose container
    markers are blank.

    Returns its normalized label, the span of its destination, and the end of
    its last line, or None when the text there holds no definition. Like
    GitHub, it accepts any destination without spaces, even one whose
    parentheses do not balance.
    """
    label = _LABEL.match(text, index)
    if label is None or not label[1].strip():
        return None
    start = _skip_space(text, label.end())
    if text.startswith("<", start):
        end = _pointy(text, start)
        if end is None:
            return None
        destination = (start + 1, end)
        end += 1
    else:
        end = start
        while end < len(text) and text[end] > " ":
            end += 1
        if end == start:
            return None
        destination = (start, end)
    name = _label(label[1])
    title = _skip_space(text, end)
    if title > end and text[title : title + 1] in ('"', "'", "("):
        # A title in parentheses holds no other unescaped parenthesis.
        stops = "()" if text[title] == "(" else text[title]
        close = title + 1
        while close < len(text) and text[close] not in stops:
            if _BLANK_LINE.match(text, close):
                break
            close += 2 if text[close] == "\\" else 1
        after = _LINE_END.match(text, close + 1)
        if text.startswith(stops[-1], close) and after is not None:
            return name, destination[0], destination[1], after.end()
    # A title that does not end its line leaves the definition before it.
    after = _LINE_END.match(text, end)
    if after is None:
        return None
    return name, destination[0], destination[1], after.end()


def _raw_html(
    text: str, regions: Iterable[tuple[int, int]]
) -> tuple[list[_Tag], list[tuple[int, int | None]]]:
    """Tokenizes raw HTML as an HTML parser does, given its spans of text in
    the order that GitHub renders them.

    Returns the open tags, and the span of each comment, where a comment that
    one span leaves open hides everything that renders after it up to the end
    of the comment in a later span, or with no end, None.
    """
    tags: list[_Tag] = []
    comments: list[tuple[int, int | None]] = []
    opened: int | None = None
    for start, end in regions:
        index = start
        while index < end:
            if opened is not None:
                close = _COMMENT_END.search(text, index, end)
                if close is None:
                    break
                comments.append((opened, close.end()))
                opened, index = None, close.end()
                continue
            markup = _MARKUP.search(text, index, end)
            if markup is None:
                break
            index = markup.start()
            if _FILTERED_TAG.match(text, index, end):
                index += 1
            elif markup[0] == "<!--":
                abrupt = _ABRUPT_COMMENT_END.match(text, markup.end(), end)
                if abrupt is None:
                    opened, index = index, markup.end()
                else:
                    comments.append((index, abrupt.end()))
                    index = abrupt.end()
            elif _TAG_START.match(text, index, end):
                tag = _html_tag(text, index, end)
                if tag is None:
                    index = markup.end()
                    continue
                if text[index + 1] != "/":
                    tags.append(tag)
                index = tag.end
            elif markup[0][1] in "!?/":
                close = text.find(">", markup.end(), end)
                comments.append((index, end if close < 0 else close + 1))
                index = end if close < 0 else close + 1
            else:
                index = markup.end()
    if opened is not None:
        comments.append((opened, None))
    return tags, comments


@dataclass(frozen=True)
class _Tag:
    """A start or end tag of raw HTML: the offsets where it starts and ends,
    the end of the raw HTML that holds it, its lowercase name, the lowercase
    name and value span of each of its attributes, and whether it closes
    itself, with "/>"."""

    start: int
    end: int
    region: int
    name: str
    attributes: tuple[tuple[str, int, int], ...]
    closed: bool = False


def _html_tag(text: str, start: int, end: int) -> _Tag | None:
    """Tokenizes the tag at start as an HTML parser does, or returns None
    when it does not end before end. As in HTML, the first attribute of each
    name, in any case, hides the others, and one without a value has an
    empty one."""
    index = start + (2 if text[start + 1] == "/" else 1)
    first = index
    while index < end and text[index] not in "\t\n\f\r />":
        index += 1
    name = text[first:index].lower()
    attributes: list[tuple[str, int, int]] = []
    seen: set[str] = set()
    last = index  # where the name or the last attribute ends
    while index < end:
        if text[index] in "\t\n\f\r /":
            index += 1
            continue
        if text[index] == ">":
            closed = text[index - 1] == "/" and index > last
            return _Tag(start, index + 1, end, name, tuple(attributes), closed)
        first = index
        index += 1
        while index < end and text[index] not in "\t\n\f\r />=":
            index += 1
        key = text[first:index].lower()
        while index < end and text[index] in "\t\n\f\r ":
            index += 1
        value = (index, index)
        if index < end and text[index] == "=":
            index += 1
            while index < end and text[index] in "\t\n\f\r ":
                index += 1
            if index < end and text[index] in "\"'":
                close = text.find(text[index], index + 1, end)
                if close < 0:
                    return None
                value, index = (index + 1, close), close + 1
            else:
                first = index
                while index < end and text[index] not in "\t\n\f\r >":
                    index += 1
                value = (first, index)
        if key not in seen:
            seen.add(key)
            attributes.append((key, *value))
        last = index
    return None


class _Event(NamedTuple):
    """A start or end tag that GitHub's HTML holds: the name of its element,
    whether it ends the element or, as "/>" does, starts and ends it, and the
    offset of raw HTML in the text that holds it, or -1."""

    name: str
    end: bool = False
    closed: bool = False
    offset: int = -1


def _event(text: str, tag: _Tag) -> _Event:
    return _Event(tag.name, text[tag.start + 1] == "/", tag.closed, tag.start)


class _Elements:
    """The elements that GitHub's HTML holds open, as libxml2's HTML parser
    nests them and an HTML5 parser rebuilds them, where a heading closes the
    heading that it starts in. It collects the text of one element, which a
    start tag opens before mark is called."""

    def __init__(self, ancestors: Iterable[str]) -> None:
        self.stack = list(ancestors)
        self.depth: int | None = None
        self.opened = False
        self.closed = False
        self.pieces: list[str] = []

    @property
    def text(self) -> str:
        return "".join(self.pieces)

    def mark(self) -> None:
        """Collects the text of the element that the last start tag opened."""
        self.depth = len(self.stack) - 1
        self.closed = not self.opened

    def feed(self, token: str | _Event) -> None:
        if isinstance(token, str):
            if self.depth is not None and not self.closed:
                self.pieces.append(token)
        elif token.end:
            self._end(token.name)
        else:
            self._start(token.name, token.closed)

    def _start(self, name: str, closed: bool) -> None:
        while self.stack and name in _CLOSED_BY.get(self.stack[-1], ()):
            self._pop(len(self.stack) - 1)
        if name in _HEADINGS and self.stack and self.stack[-1] in _HEADINGS:
            self._pop(len(self.stack) - 1)
        self.opened = not (
            closed or name in _VOID_ELEMENTS or name in _DISCARDED_ELEMENTS
        )
        if self.opened:
            self.stack.append(name)

    def _end(self, name: str) -> None:
        priority = _END_PRIORITY.get(name, 100)
        for index in range(len(self.stack) - 1, -1, -1):
            if self.stack[index] == name:
                self._pop(index)
                return
            if _END_PRIORITY.get(self.stack[index], 100) > priority:
                return

    def _pop(self, index: int) -> None:
        """Closes the elements from index of the stack."""
        del self.stack[index:]
        if self.depth is not None and index <= self.depth:
            self.closed = True


def _raw_tokens(
    text: str, index: int, end: int, unmark: bool = True
) -> Iterator[tuple[int, str | _Event]]:
    """Yields the text, with its character references decoded, and the tags
    of raw HTML from index to end as an HTML parser tokenizes it, each with
    its offset. The text is marked as parse_markdown marks it, unless unmark
    is false. A comment renders nothing, and one that does not end there
    hides the rest."""
    table = _UNMARK if unmark else {}
    while index < end:
        markup = _MARKUP.search(text, index, end)
        stop = end if markup is None else markup.start()
        if stop > index:
            yield index, _attribute(text[index:stop].translate(table))
        if markup is None:
            return
        index = stop
        if _FILTERED_TAG.match(text, index, end):
            yield index, "<"
            index += 1
        elif markup[0] == "<!--":
            close = _ABRUPT_COMMENT_END.match(
                text, markup.end(), end
            ) or _COMMENT_END.search(text, markup.end(), end)
            if close is None:
                return
            index = close.end()
        elif _TAG_START.match(text, index, end):
            tag = _html_tag(text, index, end)
            if tag is None:
                return
            yield index, _event(text, tag)
            index = tag.end
        elif markup[0][1] in "!?/":
            close = text.find(">", markup.end(), end)
            if close < 0:
                return
            index = close + 1
        else:
            yield index, markup[0]
            index = markup.end()


def _character(reference: str) -> str | None:
    """Decodes a character reference such as "&amp;", "&#38;", or "&#x26;",
    or returns None for an unknown name."""
    name = reference[1:].rstrip(";")
    if not name.startswith("#"):
        return html.entities.html5.get(f"{name};")
    code = int(name[2:], 16) if name[1] in "xX" else int(name[1:])
    if 0 < code < 0x110000 and not 0xD800 <= code < 0xE000:
        return chr(code)
    return "\ufffd"


def _decode(match: re.Match[str]) -> str:
    return _character(match[0]) or match[0]


def _destination(text: str) -> str:
    """Decodes a Markdown link destination as cmark does: it trims the
    destination's whitespace, and then decodes its character references and
    then its backslash escapes."""
    return _ESCAPED.sub(r"\1", _ENTITY.sub(_decode, text.strip(" \t\n\v\f\r")))


def _attribute(text: str) -> str:
    """Decodes an HTML attribute value as GitHub does, which keeps its
    backslashes, and its named character references without semicolons,
    literal."""
    return _HTML_ENTITY.sub(_decode, text)


def _url(text: str) -> str:
    """Decodes the URL of an HTML attribute as GitHub does, which drops its
    leading whitespace and control characters but keeps its trailing ones."""
    return _attribute(text).lstrip("".join(map(chr, range(0x21))))


@dataclass(eq=False)
class _Node:
    """A block of the tree that cmark builds, as far as the tightness of lists
    needs it: its kind, its parent and children, and whether cmark marks its
    last line as blank. A list has the marker that its items share, an item
    the line where it starts, and a paragraph the index of its block. A list
    that a new block of its parent ends while a paragraph inside it is still
    open, which cmark finalizes only after the list, holds that paragraph."""

    kind: str
    parent: _Node | None = None
    children: list[_Node] = field(default_factory=list["_Node"])
    blank: bool = False
    key: str = ""
    line: int = -1
    index: int = -1
    pending: _Node | None = None

    def add(self, child: _Node) -> _Node:
        child.parent = self
        self.children.append(child)
        return child


@dataclass(frozen=True)
class _Layout:
    """The blocks of the lines of a Markdown document.

    hidden marks the lines that render no Markdown: front matter and code.
    prefixes holds the offset where each line's container markers end, and
    content the offset where its content starts, after its indentation too,
    which a line of code keeps beyond that of its block. pads holds the
    columns of the tab at the prefix of each line, or at the content of a
    line of code, that renders as spaces since a container or the code's
    indentation consumed only part of it. scopes identifies the containers
    that hold each line's content, whose kinds, "quote", "item", "ordered"
    for the item of an ordered list, or "footnote", kinds holds, and whose
    nodes in cmark's tree nodes holds.
    blocks lists the paragraphs, tables, headings, HTML blocks, code blocks,
    and thematic breaks in order, each as a kind, its first line, and its
    end line, which is the underline of a setext heading or the closing
    fence of code; a paragraph of the kind "text", before a table's header,
    holds no definitions. footnotes lists each footnote definition as its
    label, its first line, and its end line.
    """

    hidden: list[bool]
    prefixes: list[int]
    content: list[int]
    pads: list[int]
    scopes: list[tuple[int, ...]]
    kinds: list[str]
    nodes: list[_Node]
    blocks: list[tuple[str, int, int]]
    footnotes: list[tuple[str, int, int]]


def _layout(lines: Sequence[str]) -> _Layout:
    """Finds the blocks of Markdown lines as cmark nests them.

    Block quotes, list items, and GitHub's footnote definitions contain the
    other blocks. A line continues each container whose marker or
    indentation it repeats, or a paragraph as a lazy continuation, and every
    other container ends with the code block, paragraph, table, or HTML
    block that it holds. Indentation counts columns, which a tab advances to
    the next multiple of four.
    """
    hidden = [False] * len(lines)
    prefixes = [0] * len(lines)
    content = [0] * len(lines)
    pads = [0] * len(lines)
    scopes: list[tuple[int, ...]] = [()] * len(lines)
    kinds: list[str] = []
    blocks: list[tuple[str, int, int]] = []
    footnotes: list[tuple[str, int, int]] = []
    # 0 for a block quote, else the content width of a list item or footnote,
    # whose index in footnotes owners holds, and whether each container is a
    # list item that holds nothing yet, which a blank line ends. Each
    # container's index in kinds is in serials.
    containers: list[int] = []
    owners: list[int | None] = []
    empty: list[bool] = []
    serials: list[int] = []
    # cmark's tree of blocks: the node of each container, by its serial, the
    # node of the open leaf, or of the last one, and the paragraph that the
    # line interrupts until the line opens its first block.
    document = _Node("document")
    nodes: list[_Node] = []
    block = document
    interrupted: _Node | None = None
    leaf = ""  # the open paragraph, table, fence, code, or html block
    fence = ""
    indent = 0  # the columns of a fence's indentation, which its code drops
    ending: re.Pattern[str] | None = None  # how the html block ends, if not blank
    tried = False  # whether the paragraph failed to start a table
    start = 0

    def close(end: int) -> None:
        nonlocal leaf
        if leaf == "paragraph":
            block.index = len(blocks)
        if leaf in ("paragraph", "table", "html"):
            blocks.append((leaf, start, end))
        elif leaf in ("fence", "code") and start < end:
            blocks.append(("code", start, end))
        leaf = ""

    def holder() -> _Node:
        return nodes[serials[-1]] if serials else document

    def adopt(child: _Node) -> _Node:
        # A block other than a paragraph ends the list before it at once, and
        # so before the paragraph that its line interrupts.
        nonlocal interrupted
        parent = holder()
        if (
            parent.children
            and parent.children[-1].kind == "list"
            and child.kind != "paragraph"
        ):
            parent.children[-1].pending = interrupted
        interrupted = None
        return parent.add(child)

    def mark(node: _Node, blank: bool, last: bool) -> None:
        # As cmark's last_line_blank: a line marks the deepest block that it
        # continues as last, a blank line marks that block's last child too,
        # and neither marks the blocks around them.
        if blank and node.children:
            node.children[-1].blank = True
        node.blank = last
        while node.parent is not None:
            node = node.parent
            node.blank = False

    def push(width: int, owner: int | None, vacant: bool, line: int, key: str) -> None:
        if not width or owner is not None:
            nodes.append(adopt(_Node("quote" if not width else "footnote")))
        else:
            # An item continues the list before it that has the same marker.
            items = holder().children[-1] if holder().children else None
            if items is None or items.kind != "list" or items.key != key:
                items = adopt(_Node("list", key=key))
            nodes.append(items.add(_Node("item", line=line)))
        empty[:] = [False] * len(empty)
        containers.append(width)
        owners.append(owner)
        empty.append(vacant)
        serials.append(len(kinds))
        if not width:
            kinds.append("quote")
        elif owner is not None:
            kinds.append("footnote")
        else:
            kinds.append("ordered" if key in (".", ")") else "item")

    def pop(count: int, end: int) -> None:
        for owner in owners[count:]:
            if owner is not None:
                label, first, _ = footnotes[owner]
                footnotes[owner] = (label, first, end)
        del containers[count:]
        del owners[count:]
        del empty[count:]
        del serials[count:]

    def code(number: int, index: int, column: int, columns: int) -> None:
        # A line of code drops up to columns of its indentation.
        hidden[number] = True
        content[number], at = _advance(lines[number], index, column, columns)
        pads[number] = _pad(lines[number], content[number], at)

    first = _front_matter(lines)
    hidden[:first] = [True] * first
    for number in range(first, len(lines)):
        line = lines[number]
        index = column = matched = 0
        for width in containers:
            nonspace, at = _indent(line, index, column)
            if not width:
                if at - column >= 4 or not line.startswith(">", nonspace):
                    break
                index, column = _quote(line, nonspace, at)
            elif at - column >= width:
                index, column = _advance(line, index, column, width)
            elif nonspace < len(line) or empty[matched]:
                break
            matched += 1
        nonspace, at = _indent(line, index, column)
        prefixes[number], content[number] = index, nonspace
        pads[number] = _pad(line, index, column)
        scopes[number] = tuple(serials)
        blank = nonspace == len(line)
        if matched == len(containers):
            if leaf == "html" and ending is not None:
                mark(block, blank, blank)
                if ending.search(line, nonspace):
                    close(number + 1)
                continue
            if leaf == "fence":
                # A blank line in fenced code marks nothing.
                mark(block, blank, False)
                hidden[number] = True
                closer = _FENCE_CLOSER.fullmatch(line, nonspace)
                if (
                    at - column < 4
                    and closer is not None
                    and closer[1][0] == fence[0]
                    and len(closer[1]) >= len(fence)
                ):
                    if start < number:
                        blocks.append(("code", start, number))
                    leaf = ""
                else:
                    code(number, index, column, indent)
                continue
            if leaf == "code" and (blank or at - column >= 4):
                mark(block, blank, blank)
                code(number, index, column, 4)
                continue
            if leaf == "html" and not blank:
                mark(block, False, False)
                continue
            # A table continues with each line that has a cell and starts no
            # other block, where only a paragraph's rules would be laxer.
            if (
                leaf == "table"
                and not blank
                and at - column < 4
                and _cells(line, nonspace)
                and not _starts_block(line, nonspace, at - column)
            ):
                mark(block, False, False)
                continue
        elif (
            leaf == "paragraph"
            and not blank
            and not _starts_block(line, nonspace, at - column)
        ):
            mark(block, False, False)
            continue
        interrupted = block if leaf == "paragraph" else None
        if leaf != "paragraph" or matched < len(containers):
            close(number)
        pop(matched, number)
        while True:
            nonspace, at = _indent(line, index, column)
            if at - column >= 4:
                break
            if line.startswith(">", nonspace):
                close(number)
                push(0, None, False, number, "")
                index, column = _quote(line, nonspace, at)
                continue
            if _THEMATIC_BREAK.fullmatch(line, nonspace):
                break
            footnote = _FOOTNOTE.match(line, nonspace)
            if footnote is not None:
                close(number)
                push(4, len(footnotes), False, number, "")
                footnotes.append((footnote[1], number, len(lines)))
                index, column = _indent(line, footnote.end(), at + len(footnote[0]))
                continue
            item = _LIST_MARKER.match(line, nonspace)
            if item is None:
                break
            marker = at + len(item[0])
            after, spaces = _indent(line, item.end(), marker)
            vacant = after == len(line)
            # Only a bullet or a 1, with content, can interrupt a paragraph.
            if leaf == "paragraph" and (vacant or int(item[1] or 1) != 1):
                break
            close(number)
            # The content starts one to four columns after the marker, or one
            # when more, or nothing but the end of the line, follow it. Items
            # with the same bullet, or the same delimiter after their number,
            # form a list.
            key = item[0][-1]
            if vacant or spaces - marker > 4:
                push(marker + 1 - column, None, vacant, number, key)
                index, column = _advance(line, item.end(), marker, 1)
            else:
                push(spaces - column, None, False, number, key)
                index, column = after, spaces
        nonspace, at = _indent(line, index, column)
        prefixes[number], content[number] = index, nonspace
        pads[number] = _pad(line, index, column)
        scopes[number] = tuple(serials)
        if nonspace == len(line):
            close(number)
            # The line is blank in the deepest container that it continues,
            # or in the list that ends that container, but not in a block
            # quote or an item that starts on it.
            last = holder()
            if last.children and last.children[-1].kind == "list":
                last = last.children[-1]
            mark(
                last,
                True,
                last.kind != "quote"
                and not (
                    last.kind == "item" and not last.children and last.line == number
                ),
            )
            continue
        empty[:] = [False] * len(empty)
        if at - column >= 4:
            if leaf != "paragraph":
                code(number, index, column, 4)
                leaf, start, block = "code", number, adopt(_Node("code"))
        elif (opener := _fence(line, nonspace)) is not None:
            close(number)
            hidden[number] = True
            leaf, fence, start, indent = "fence", opener, number + 1, at - column
            block = adopt(_Node("fence"))
        elif kind := _html_block(line, nonspace, leaf == "paragraph"):
            close(number)
            leaf, start, block = "html", number, adopt(_Node("html"))
            ending = _HTML_ENDS[kind - 1] if kind <= len(_HTML_ENDS) else None
            if ending is not None and ending.search(line, nonspace):
                close(number + 1)
        elif _ATX_HEADING.fullmatch(line, nonspace):
            close(number)
            adopt(_Node("heading"))
            blocks.append(("heading", number, number + 1))
        elif leaf == "paragraph" and _SETEXT_UNDERLINE.fullmatch(line, nonspace):
            blocks.append(("setext", start, number))
            leaf = ""
        elif _THEMATIC_BREAK.fullmatch(line, nonspace):
            close(number)
            adopt(_Node("break"))
            blocks.append(("break", number, number + 1))
        elif (
            leaf == "paragraph"
            and not tried
            and _TABLE_DELIMITER.fullmatch(line, nonspace)
        ):
            # The paragraph's last line is the header of a table with as many
            # cells, and a paragraph's lines before it hold no definitions. A
            # paragraph that fails this once never starts a table.
            header = number - 1
            if len(_cells(line, nonspace)) == len(
                _cells(lines[header], content[header])
            ):
                if start < header:
                    blocks.append(("text", start, header))
                leaf, start = "table", header
            else:
                tried = True
        elif leaf != "paragraph":
            leaf, start, tried = "paragraph", number, False
            block = adopt(_Node("paragraph"))
        mark(block if leaf else holder(), False, False)
    close(len(lines))
    pop(0, len(lines))
    return _Layout(
        hidden, prefixes, content, pads, scopes, kinds, nodes, blocks, footnotes
    )


def _front_matter(lines: Sequence[str]) -> int:
    """Returns the number of lines of the front matter, which GitHub renders
    as a table, or zero."""
    if lines and lines[0].rstrip() == "---":
        for number in range(1, len(lines)):
            if lines[number].rstrip() in ("---", "..."):
                return number + 1
    return 0


def _tight(layout: _Layout, firsts: Sequence[int]) -> set[int]:
    """Returns the serials of the items of tight lists, whose paragraphs
    render without tags, as cmark finds them: a list is loose when a blank
    line ends one of its items before another, or one block of an item
    before another, where a list or item ends as its last block does.

    cmark drops each paragraph that holds only definitions, which start at
    firsts, when it finalizes it, so such a paragraph separates no blocks,
    unless it was still open when cmark finalized the list.
    """
    hollow = {
        index
        for index, ((kind, _, end), inline) in enumerate(
            zip(layout.blocks, firsts, strict=True)
        )
        if kind == "paragraph" and inline >= end
    }
    loose: dict[_Node, bool] = {}

    def kept(node: _Node, items: _Node) -> list[_Node]:
        return [
            child
            for child in node.children
            if child.index not in hollow or child is items.pending
        ]

    def ends_blank(node: _Node | None, items: _Node) -> bool:
        while node is not None:
            if node.blank:
                return True
            children = kept(node, items) if node.kind in ("list", "item") else []
            node = children[-1] if children else None
        return False

    def is_loose(items: _Node) -> bool:
        for position, item in enumerate(items.children):
            following = position + 1 < len(items.children)
            if item.blank and following:
                return True
            children = kept(item, items)
            for place, child in enumerate(children):
                if (following or place + 1 < len(children)) and ends_blank(
                    child, items
                ):
                    return True
        return False

    tight: set[int] = set()
    for serial, node in enumerate(layout.nodes):
        if node.kind == "item" and node.parent is not None:
            if node.parent not in loose:
                loose[node.parent] = is_loose(node.parent)
            if not loose[node.parent]:
                tight.add(serial)
    return tight


def _starts_block(line: str, index: int, indent: int) -> bool:
    """Whether a line that does not continue its containers, and whose
    content starts at index, indent columns in, starts a block rather than
    continuing a paragraph lazily. As in cmark, the rules that keep a list
    item from interrupting a paragraph do not apply."""
    return indent < 4 and bool(
        line.startswith(">", index)
        or _LIST_MARKER.match(line, index)
        or _FOOTNOTE.match(line, index)
        or _fence(line, index) is not None
        or _ATX_HEADING.fullmatch(line, index)
        or _THEMATIC_BREAK.fullmatch(line, index)
        or _html_block(line, index, False)
    )


def _fence(line: str, index: int) -> str | None:
    """Returns the fence that opens a code block at index of a line, or
    None."""
    opener = _FENCE_OPENER.fullmatch(line, index)
    if opener is None or (opener[1][0] == "`" and "`" in opener[2]):
        return None
    return opener[1]


def _html_block(line: str, index: int, interrupting: bool) -> int:
    """Returns the type, from 1 to 7, of the HTML block that starts at index
    of a line, or 0 for none. A block of type 7, a lone complete tag, cannot
    interrupt a paragraph."""
    for kind, opener in enumerate(_HTML_STARTS, 1):
        if opener.match(line, index):
            return kind
    if not interrupting and _HTML_TAG_LINE.fullmatch(line, index):
        return len(_HTML_STARTS) + 1
    return 0


def _indent(text: str, index: int, column: int) -> tuple[int, int]:
    """Skips the spaces and tabs at index, which is at column, and returns
    the offset and the column after them. A tab advances to the next
    multiple of four columns."""
    while index < len(text) and text[index] in " \t":
        column += 4 - column % 4 if text[index] == "\t" else 1
        index += 1
    return index, column


def _advance(text: str, index: int, column: int, columns: int) -> tuple[int, int]:
    """Skips up to columns of the spaces and tabs at index, which is at
    column, and returns the offset and the column after them. A tab that it
    skips only in part stays at the offset."""
    end = column + columns
    while column < end and index < len(text) and text[index] in " \t":
        step = 4 - column % 4 if text[index] == "\t" else 1
        if column + step > end:
            return index, end
        index, column = index + 1, column + step
    return index, column


def _quote(line: str, index: int, column: int) -> tuple[int, int]:
    """Skips the block quote marker at index of a line, which is at column,
    and the column of space after it."""
    return _advance(line, index + 1, column + 1, 1)


def _pad(line: str, index: int, column: int) -> int:
    """Returns the columns that remain at column of the tab at index of a
    line, which cmark renders as spaces, when a container or the indentation
    of code consumed part of it, or 0."""
    if index >= len(line) or line[index] != "\t":
        return 0
    start = 0
    for char in line[:index]:
        start += 4 - start % 4 if char == "\t" else 1
    return 4 - column % 4 if column > start else 0


def _inline_ranges(
    lines: Sequence[str],
    layout: _Layout,
    starts: Sequence[int],
    firsts: Sequence[int],
) -> Iterator[tuple[int, int, int]]:
    """Yields the index of the block and the span of each part of it whose
    inline content GitHub parses as a unit: each heading, each paragraph
    after its definitions, which start at firsts, and each cell of a table's
    rows."""

    def span(first: int, end: int) -> tuple[int, int]:
        return starts[first] + layout.content[first], starts[end - 1] + len(
            lines[end - 1]
        )

    for index, ((kind, first, end), inline) in enumerate(
        zip(layout.blocks, firsts, strict=True)
    ):
        if kind == "heading":
            yield index, *span(first, end)
        elif kind == "table":
            for number, cells in _rows(lines, layout, first, end):
                for first_cell, last_cell in cells:
                    yield index, starts[number] + first_cell, starts[number] + last_cell
        elif kind in ("paragraph", "text", "setext") and inline < end:
            yield index, *span(inline, end)


def _rows(
    lines: Sequence[str], layout: _Layout, first: int, end: int
) -> Iterator[tuple[int, list[tuple[int, int]]]]:
    """Yields the line and the cells of each row of a table, where GitHub
    parses each cell alone and drops those past the header's."""
    columns = len(_cells(lines[first], layout.content[first]))
    for number in range(first, end):
        if number != first + 1:
            yield number, _cells(lines[number], layout.content[number])[:columns]


def _cells(line: str, start: int) -> list[tuple[int, int]]:
    """Returns the span of each cell of the table row whose content starts at
    start, between its unescaped pipes, or none for a blank line."""
    end = len(line.rstrip(" \t"))
    while start < end and line[start] in " \t":
        start += 1
    if start >= end:
        return []
    pipes: list[int] = []
    index = start
    while index < end:
        if line[index] == "\\":
            index += 2
            continue
        if line[index] == "|":
            pipes.append(index)
        index += 1
    bounds = pipes if pipes and pipes[0] == start else [start - 1, *pipes]
    if bounds[-1] != end - 1:
        bounds.append(end)
    return [(bounds[cell] + 1, bounds[cell + 1]) for cell in range(len(bounds) - 1)]


def _label(text: str) -> str:
    """Normalizes a link label as CommonMark matches them."""
    return " ".join(text.split()).casefold()


@dataclass(frozen=True)
class _Inline:
    """A link, image, footnote citation, code span, autolink, open tag, or
    other raw HTML of inline content.

    start is the offset where it starts, at the "[" of a link, image, or
    citation; close is the offset of the "]" that ends their text, or of a
    code span's closing backticks; end is the offset after it; destination
    is the span of an inline link's or image's destination; and label is the
    normalized label of a reference link's or image's definition.
    """

    kind: str
    start: int
    close: int
    end: int
    destination: tuple[int, int] | None = None
    label: str | None = None


def _inline(
    text: str, labels: Collection[str], footnotes: Collection[str]
) -> list[_Inline]:
    """Parses inline content left to right as CommonMark does, where
    references resolve against labels and citations against the casefolded
    labels of footnotes.

    Code spans, autolinks, and raw HTML are atomic, so they hold no links,
    and so are GitHub's bare URL autolinks, outside brackets. Each
    destination is consumed with its title. A link deactivates the link
    openers around it, since links do not nest, and an image's description
    renders as its alt text, so nothing inside it counts.
    """
    found: list[_Inline] = []
    # Each opener is the offset of its "[", whether it opens an image, whether
    # it is still active, and how many items precede it.
    openers: list[tuple[int, bool, bool, int]] = []
    index = 0
    while index < len(text):
        char = text[index]
        if char == "\\" and text[index + 1 : index + 2] in _ASCII_PUNCTUATION:
            index += 2
            continue
        if char in ":w" and not openers:
            url = (
                _url_autolink(text, index)
                if char == ":"
                else _www_autolink(text, index)
            )
            if url is not None:
                found.append(_Inline("url", url[0], url[1], url[1]))
                index = url[1]
                continue
        if char == "`":
            run = index
            while run < len(text) and text[run] == "`":
                run += 1
            closing = next(
                (
                    other
                    for other in _BACKTICKS.finditer(text, run)
                    if len(other[0]) == run - index
                ),
                None,
            )
            if closing is not None:
                found.append(_Inline("code", index, closing.start(), closing.end()))
            index = closing.end() if closing is not None else run
            continue
        if char == "<":
            match = _AUTOLINK.match(text, index) or _EMAIL_AUTOLINK.match(text, index)
            kind = "autolink"
            if match is None:
                match = _OPEN_TAG.match(text, index)
                kind = "tag"
            if match is None:
                match = _CLOSE_TAG.match(text, index) or _RAW_HTML.match(text, index)
                kind = "html"
            if match is not None:
                found.append(_Inline(kind, index, match.end(), match.end()))
                index = match.end()
                continue
        elif char == "!" and text.startswith("[", index + 1):
            openers.append((index + 1, True, True, len(found)))
            index += 2
            continue
        elif char == "[":
            openers.append((index, False, True, len(found)))
        elif char == "]" and openers:
            start, image, active, count = openers.pop()
            link = _link_end(text, start, index, labels) if active else None
            citation = _FOOTNOTE_LABEL.fullmatch(text, start + 1, index)
            if link is None and not image and citation is not None:
                if citation[1].casefold() in footnotes:
                    found.append(_Inline("footnote", start, index, index + 1))
            if link is not None:
                end, destination, label = link
                if image:
                    del found[count:]
                else:
                    openers = [
                        (opener, is_image, is_active and is_image, before)
                        for opener, is_image, is_active, before in openers
                    ]
                kind = "image" if image else "link"
                found.append(_Inline(kind, start, index, end, destination, label))
                index = end
                continue
        index += 1
    return found


def _url_autolink(text: str, colon: int) -> tuple[int, int] | None:
    """Returns the span of the bare URL autolink, such as one that starts
    with https://, whose scheme's colon is at colon, as cmark-gfm's autolink
    extension finds it, or None."""
    if len(text) - colon < 4 or text[colon + 1 : colon + 3] != "//":
        return None
    start = colon
    while start and text[start - 1].isascii() and text[start - 1].isalpha():
        start -= 1
    after = text[colon + 3]
    if text[start : colon + 3].lower() not in ("http://", "https://", "ftp://") or not (
        after.isascii() and after.isalnum()
    ):
        return None
    domain = _domain(text, colon + 3)
    if not domain:
        return None
    end = _autolink_end(text, colon, colon + 3 + domain)
    return (start, end) if end > colon else None


def _www_autolink(text: str, index: int) -> tuple[int, int] | None:
    """Returns the span of the bare www. autolink at index, as cmark-gfm's
    autolink extension finds it, or None."""
    if index and text[index - 1] not in "*_~( \t\n\v\f\r":
        return None
    if not text.startswith("www.", index):
        return None
    domain = _domain(text, index)
    if not domain:
        return None
    end = _autolink_end(text, index, index + domain)
    return (index, end) if end > index else None


def _domain(text: str, start: int) -> int:
    """Returns the length of the domain at start as cmark-gfm's check_domain
    measures it, or 0 for one without a period or with an underscore in its
    last two segments."""
    size = len(text) - start
    periods = before = current = 0  # underscores before and after the last "."
    index = 1
    while index < size - 1:
        if text[start + index] == "\\" and index < size - 2:
            index += 1
        char = text[start + index]
        if char == "_":
            current += 1
        elif char == ".":
            before, current = current, 0
            periods += 1
        elif char != "-" and (_is_space(char) or _is_punctuation(char)):
            break
        index += 1
        if not char.isascii():
            # It scans bytes, so a character of several ends the domain.
            break
    if (before or current) and periods <= 10:
        return 0
    return index if periods else 0


def _autolink_end(text: str, start: int, index: int) -> int:
    """Returns the end of the bare autolink that starts at start and whose
    domain ends at index, as cmark-gfm's autolink_delim trims its trailing
    punctuation, unmatched parentheses, and character reference."""
    end = index
    while end < len(text) and text[end] not in " \t\n\v\f\r<":
        end += 1
    opening = text.count("(", start, end)
    closing = text.count(")", start, end)
    while end > start:
        char = text[end - 1]
        if char == ")":
            if closing <= opening:
                break
            closing -= 1
            end -= 1
        elif char in "?!.,:*_~'\"":
            end -= 1
        elif char == ";":
            letter = end - 2
            while letter > start and text[letter].isascii() and text[letter].isalpha():
                letter -= 1
            end = letter if letter < end - 2 and text[letter] == "&" else end - 1
        else:
            break
    return end


def _link_end(
    text: str, opening: int, closing: int, labels: Collection[str]
) -> tuple[int, tuple[int, int] | None, str | None] | None:
    """Matches what follows the text of a link or image, from the bracket at
    opening to the one at closing: an inline destination, or a reference
    whose label is defined.

    Returns the offset after the link, and the span of its inline destination
    or the normalized label of its reference, or None when the brackets form
    no link.
    """
    after = closing + 1
    if text.startswith("(", after):
        destination = _inline_destination(text, after + 1)
        if destination is not None:
            return destination[2] + 1, (destination[0], destination[1]), None
    if text.startswith("[", after):
        label = _label_end(text, after)
        if label is not None:
            name = _label(text[after + 1 : label] or text[opening + 1 : closing])
            # A following label, even an undefined one, leaves no shortcut.
            return (label + 1, None, name) if name in labels else None
    name = _label(text[opening + 1 : closing])
    return (after, None, name) if name in labels else None


def _label_end(text: str, start: int) -> int | None:
    """Returns the offset of the "]" that closes the link label whose "[" is
    at start, past its escapes, or None when a "[" comes first or the label
    runs past 999 characters."""
    index = start + 1
    while index < len(text) and index - start <= 1000:
        if text[index] == "\\" and text[index + 1 : index + 2] in _ASCII_PUNCTUATION:
            index += 2
        elif text[index] == "[":
            return None
        elif text[index] == "]":
            return index
        else:
            index += 1
    return None


def _pointy(text: str, start: int) -> int | None:
    """Returns the offset of the ">" that closes the destination whose "<" is
    at start, or None when a line ending or "<" comes first. As in cmark, a
    backslash keeps the character after it, whatever it is, from either."""
    index = start + 1
    while index < len(text):
        if text[index] == "\\":
            index += 2
        elif text[index] == ">":
            return index
        elif text[index] in "<\n":
            return None
        else:
            index += 1
    return None


def _inline_destination(text: str, index: int) -> tuple[int, int, int] | None:
    """Parses the destination and title of an inline link from index.

    Returns the span of the destination and the offset of the closing
    parenthesis, or None when the text is not an inline link.
    """
    start = _skip_space(text, index)
    if text.startswith("<", start):
        end = _pointy(text, start)
        if end is None:
            return None
        span, index = (start + 1, end), end + 1
    else:
        end, depth = start, 0
        while end < len(text) and text[end] > " ":
            if text[end] == "\\" and text[end + 1 : end + 2] in _ASCII_PUNCTUATION:
                end += 2
                continue
            if text[end] == "(":
                depth += 1
            elif text[end] == ")":
                if not depth:
                    break
                depth -= 1
            end += 1
        if depth:
            return None
        span, index = (start, end), end
    title = _skip_space(text, index)
    if title > index and text[title : title + 1] in ('"', "'", "("):
        # A title in parentheses holds no other unescaped parenthesis.
        stops = "()" if text[title] == "(" else text[title]
        end = title + 1
        while end < len(text) and text[end] not in stops:
            end += 2 if text[end] == "\\" else 1
        if not text.startswith(stops[-1], end):
            return None
        index = end + 1
    close = _skip_space(text, index)
    if not text.startswith(")", close):
        return None
    return span[0], span[1], close


def _skip_space(text: str, index: int) -> int:
    """Skips spaces and tabs with up to one line ending."""
    match = _SPACE.match(text, index)
    return index if match is None else match.end()


@dataclass
class _Texts:
    """Finds the text that GitHub slugs for each heading of a Markdown
    document, from the heading's start tag up to where the elements of the
    HTML that it renders close the heading, which may be after its block.

    lines, starts, layout, blanked, and marked are what parse_markdown finds
    of the document. Inline content starts at firsts after definitions,
    references resolve against labels, and citations render as the numbers
    of their footnotes. regions holds the index of each HTML block by where
    it ends, and spans the index of the block and the span of the inline
    content that holds each item of inline raw HTML by where the item ends.
    place and shown tell where and whether what is at an offset renders, and
    tight holds the serials of the items of tight lists. stacks holds the
    elements open where each rendered block starts, such as a div that an
    earlier block leaves open.
    """

    lines: Sequence[str]
    starts: Sequence[int]
    layout: _Layout
    blanked: str
    marked: str
    firsts: Sequence[int]
    labels: Collection[str]
    numbers: Mapping[str, int]
    regions: Mapping[int, int]
    spans: Mapping[int, tuple[int, int, int]]
    place: Callable[[int], tuple[int, int]]
    shown: Callable[[int], bool]
    tight: Collection[int]
    stacks: list[list[str] | None] = field(init=False)

    def __post_init__(self) -> None:
        # GitHub renders the footnotes after the rest, in order of rank.
        self.stacks = [None] * len(self.layout.blocks)
        elements = _Elements(())
        scope: tuple[int, ...] = ()
        for rank, index in sorted(
            (self.place(self._offset(index))[0], index)
            for index in range(len(self.layout.blocks))
        ):
            if rank < 0:
                continue
            if self.layout.blocks[index][0] == "html" or self.shown(
                self._offset(index)
            ):
                scope = self._enter(elements, scope, index)
                self.stacks[index] = list(elements.stack)
                for token in self._tokens(index, markup=True):
                    elements.feed(token)
            else:
                self.stacks[index] = list(elements.stack)

    def markdown(self, index: int) -> str | None:
        """Returns the text of the Markdown heading that is the block at
        index, or None when the block is no heading."""
        heading = _heading(self.lines, self.layout, index, self.firsts[index])
        if heading is None:
            return None
        name, content = heading
        elements = _Elements(self._stack(index))
        elements.feed(_Event(name))
        elements.mark()
        for token in _inline_tokens(content, self.labels, self.numbers):
            elements.feed(token)
        elements.feed(_Event(name, end=True))
        self._follow(elements, index)
        return elements.text

    def html(self, tag: _Tag) -> str | None:
        """Returns the text of the HTML heading that tag starts, or None when
        no rendered block holds it. Inline content is inside the paragraph,
        heading, or table cell that holds it."""
        tokens: Iterable[tuple[int, str | _Event]]
        span = self.spans.get(tag.region)
        if span is not None:
            index, start, end = span
            parents = self._parents(index)
            # The final whitespace of inline content renders nothing.
            tokens = (
                (
                    start + token.offset
                    if isinstance(token, _Event) and token.offset >= 0
                    else -1,
                    token,
                )
                for token in _inline_tokens(
                    self.blanked[start:end].rstrip(" \t"), self.labels, self.numbers
                )
            )
        elif tag.region in self.regions:
            index, parents = self.regions[tag.region], []
            first = self.layout.blocks[index][1]
            tokens = self._raw(self.starts[first], tag.region)
        else:
            return None
        elements = _Elements(self._stack(index) + parents)
        for offset, token in tokens:
            elements.feed(token)
            if offset == tag.start and isinstance(token, _Event):
                elements.mark()
            elif elements.closed:
                break
        if elements.depth is None:
            return None
        for name in reversed(parents):
            elements.feed(_Event(name, end=True))
        self._follow(elements, index)
        return elements.text

    def _offset(self, index: int) -> int:
        """Returns the offset where the content of the block at index starts."""
        first = self.layout.blocks[index][1]
        return self.starts[first] + self.layout.content[first]

    def _raw(self, start: int, end: int) -> Iterator[tuple[int, str | _Event]]:
        """Yields the text and the tags of the raw HTML of a block from start
        to end, each with its offset, after the end of a comment that an
        earlier block leaves open."""
        if not self.shown(start):
            close = _COMMENT_END.search(self.marked, start, end)
            if close is None:
                return
            start = close.end()
        yield from _raw_tokens(self.marked, start, end)

    def _stack(self, index: int) -> list[str]:
        """Returns the elements open where the block at index starts, or the
        elements of its containers when it does not render."""
        stack = self.stacks[index]
        if stack is not None:
            return list(stack)
        first = self.layout.blocks[index][1]
        return [
            name
            for serial in self.layout.scopes[first]
            for name in _CONTAINER_ELEMENTS[self.layout.kinds[serial]]
        ]

    def _parents(self, index: int) -> list[str]:
        """Returns the elements that hold the inline content of the block at
        index, inside its containers, where the paragraphs of a tight list's
        items have none."""
        kind, first, _ = self.layout.blocks[index]
        if kind == "table":
            return ["table", "tr", "td"]
        heading = _heading(self.lines, self.layout, index, self.firsts[index])
        if heading is not None:
            return [heading[0]]
        scope = self.layout.scopes[first]
        return [] if scope and scope[-1] in self.tight else ["p"]

    def _enter(
        self, elements: _Elements, scope: tuple[int, ...], index: int
    ) -> tuple[int, ...]:
        """Feeds elements the tags of the containers that close and open from
        scope to the block at index, and returns the block's scope."""
        new = self.layout.scopes[self.layout.blocks[index][1]]
        same = 0
        while same < min(len(scope), len(new)) and scope[same] == new[same]:
            same += 1
        for serial in reversed(scope[same:]):
            for name in reversed(_CONTAINER_ELEMENTS[self.layout.kinds[serial]]):
                elements.feed(_Event(name, end=True))
        for serial in new[same:]:
            for name in _CONTAINER_ELEMENTS[self.layout.kinds[serial]]:
                elements.feed(_Event(name))
        return new

    def _follow(self, elements: _Elements, index: int) -> None:
        """Feeds elements what renders after the block at index, until the
        element whose text it collects closes. Footnotes render elsewhere."""
        scope = self.layout.scopes[self.layout.blocks[index][1]]
        rank = self.place(self._offset(index))[0]
        for later in range(index + 1, len(self.layout.blocks)):
            if elements.closed:
                return
            offset = self._offset(later)
            if self.place(offset)[0] != rank or (
                self.layout.blocks[later][0] != "html" and not self.shown(offset)
            ):
                continue
            scope = self._enter(elements, scope, later)
            for token in self._tokens(later):
                if elements.closed:
                    return
                elements.feed(token)

    def _tokens(self, index: int, markup: bool = False) -> Iterator[str | _Event]:
        """Yields the text and the tags that GitHub renders for the block at
        index, without its containers, or with markup, only what may change
        the elements that it leaves open."""

        def inline(content: str) -> list[str | _Event]:
            # Inline content without raw HTML leaves no element open.
            if markup and "<" not in content:
                return []
            return _inline_tokens(content, self.labels, self.numbers)

        kind, first, end = self.layout.blocks[index]
        last = self.starts[end - 1] + len(self.lines[end - 1])
        if kind == "html":
            yield from (token for _, token in self._raw(self.starts[first], last))
        elif kind == "code":
            yield from (_Event("pre"), _Event("code"))
            for number in range(first, end):
                line, start = self.lines[number], self.layout.content[number]
                pad = self.layout.pads[number]
                yield " " * pad + line[start + 1 if pad else start :] + "\n"
            yield from (_Event("code", end=True), _Event("pre", end=True))
        elif kind == "break":
            yield _Event("hr")
        elif kind == "table":
            yield _Event("table")
            for number, cells in _rows(self.lines, self.layout, first, end):
                yield _Event("tr")
                for start, stop in cells:
                    # GitHub trims each cell's content.
                    cell = self.blanked[
                        self.starts[number] + start : self.starts[number] + stop
                    ]
                    yield _Event("td")
                    yield from inline(cell.strip(" \t"))
                    yield _Event("td", end=True)
                yield _Event("tr", end=True)
            yield _Event("table", end=True)
        elif (
            heading := _heading(self.lines, self.layout, index, self.firsts[index])
        ) is not None:
            name, content = heading
            yield _Event(name)
            yield from inline(content)
            yield _Event(name, end=True)
        elif kind in ("paragraph", "text") and self.firsts[index] < end:
            inline_first = self.firsts[index]
            start = self.starts[inline_first] + self.layout.content[inline_first]
            paragraph = self._parents(index)
            yield from (_Event(name) for name in paragraph)
            yield from inline(self.blanked[start:last].rstrip(" \t"))
            yield from (_Event(name, end=True) for name in paragraph)


def _heading(
    lines: Sequence[str], layout: _Layout, index: int, inline: int
) -> tuple[str, str] | None:
    """Returns the element and the inline content of the Markdown heading
    that is the block at index of a layout, or None when it is no heading.

    A setext heading holds the lines of its paragraph after the definitions
    that start at inline.
    """
    kind, first, end = layout.blocks[index]
    if kind == "heading":
        line = lines[first][layout.content[first] :]
        heading = _ATX_HEADING.fullmatch(line)
        text = (heading[1] or "") if heading else ""
        level = len(line) - len(line.lstrip("#"))
        return f"h{level}", _CLOSING_SEQUENCE.sub("", text.strip(" \t"))
    if kind == "setext" and inline < end:
        # Only the final whitespace of a paragraph renders nothing, while
        # that of each line may render in a code span.
        content = "\n".join(
            lines[number][layout.content[number] :] for number in range(inline, end)
        ).rstrip(" \t")
        level = 1 if lines[end][layout.content[end]] == "=" else 2
        return f"h{level}", content
    return None


def _slugs(headings: Iterable[str]) -> Iterator[str]:
    """Yields the anchors that GitHub generates for the texts of headings in
    order, whose repeated slugs it numbers. A heading without text has none,
    while an empty slug, which is no anchor, still counts."""
    occurrences: dict[str, int] = {}
    for heading in headings:
        if not heading:
            continue
        slug = github_slug(heading)
        anchor = slug
        while anchor in occurrences:
            occurrences[slug] += 1
            anchor = f"{slug}-{occurrences[slug]}"
        occurrences[anchor] = 0
        yield anchor


def _inline_tokens(
    content: str, labels: Collection[str], numbers: Mapping[str, int]
) -> list[str | _Event]:
    """Returns the text and the tags that GitHub renders for inline content:
    links keep their text, autolinks their address, code spans their code,
    citations the numbers of their footnotes in numbers, and the tags that
    GitHub filters their own text, while images, whose descriptions are only
    alt text, render none, and the delimiters of emphasis and strikethrough
    render only when unmatched."""
    items = sorted(_inline(content, labels, numbers), key=lambda item: item.start)
    return _render(content, items, 0, len(content), numbers)


def _render(
    content: str,
    items: Sequence[_Inline],
    start: int,
    end: int,
    numbers: Mapping[str, int],
) -> list[str | _Event]:
    """Renders the text and the tags of content from start to end, whose
    delimiters match only one another, as in the whole of a heading or the
    text of a link."""
    pieces: list[str | _Event | _Delimiter] = []
    index = start
    for item in items:
        if item.start < index or item.end > end:
            continue
        begin = item.start - 1 if item.kind == "image" else item.start
        _plain(content, index, begin, pieces)
        if item.kind == "link":
            pieces.append(_Event("a"))
            pieces.extend(_render(content, items, item.start + 1, item.close, numbers))
            pieces.append(_Event("a", end=True))
        elif item.kind == "code":
            # A line ending in a code span renders as a space, without the
            # indentation of the next line, which blanked text holds.
            ticks = item.end - item.close
            code = _LINE_BREAK.sub(" ", content[item.start + ticks : item.close])
            if code.startswith(" ") and code.endswith(" ") and code.strip(" "):
                code = code[1:-1]
            pieces.append(code)
        elif item.kind == "autolink":
            pieces.append(_ENTITY.sub(_decode, content[item.start + 1 : item.end - 1]))
        elif item.kind == "url":
            pieces.append(content[item.start : item.end])
        elif item.kind == "footnote":
            label = content[item.start + 2 : item.close].casefold()
            pieces.append(str(numbers[label]))
        elif item.kind in ("tag", "html"):
            # As an HTML parser tokenizes it, so the text of a CDATA section,
            # say, after its first ">" renders.
            pieces.extend(
                token for _, token in _raw_tokens(content, item.start, item.end, False)
            )
        index = item.end
    _plain(content, index, end, pieces)
    _emphasize([piece for piece in pieces if isinstance(piece, _Delimiter)])
    tokens: list[str | _Event] = []
    for piece in pieces:
        if isinstance(piece, _Delimiter):
            tokens.extend(_Event(name, end=True) for name in piece.ends)
            tokens.append(piece.char * piece.count)
            tokens.extend(_Event(name) for name in reversed(piece.starts))
        else:
            tokens.append(piece)
    return tokens


@dataclass
class _Delimiter:
    """A run of emphasis or strikethrough delimiters, its length, the
    characters of it that no match uses, whether it can open or close, and
    the elements that its matches start and end, innermost first."""

    char: str
    length: int
    count: int
    opens: bool
    closes: bool
    starts: list[str] = field(default_factory=list[str])
    ends: list[str] = field(default_factory=list[str])


def _plain(
    content: str, start: int, end: int, pieces: list[str | _Event | _Delimiter]
) -> None:
    """Splits the plain text of content from start to end into the text of
    its characters, escapes, and character references, and its delimiters.
    A line ending drops the spaces and tabs around it."""
    index = start
    while index < end:
        char = content[index]
        if char == "\\" and content[index + 1 : index + 2] in _ASCII_PUNCTUATION:
            pieces.append(content[index + 1])
            index += 2
        elif (
            char == "&"
            and (reference := _ENTITY.match(content, index, end)) is not None
            and (literal := _character(reference[0])) is not None
        ):
            pieces.append(literal)
            index = reference.end()
        elif char in "*_~":
            run = index
            while run < end and content[run] == char:
                run += 1
            delimiter = _delimiter(content, index, run)
            pieces.append(content[index:run] if delimiter is None else delimiter)
            index = run
        elif char == "\n":
            while pieces and pieces[-1] in (" ", "\t"):
                pieces.pop()
            pieces.append(char)
            index += 1
            while index < end and content[index] in " \t":
                index += 1
        else:
            pieces.append(char)
            index += 1


def _delimiter(content: str, start: int, end: int) -> _Delimiter | None:
    """Returns the delimiter that the run of "*", "_", or "~" from start to
    end of content forms as cmark-gfm scans it, or None when it forms none."""
    char = content[start]
    before = content[start - 1] if start else "\n"
    after = content[end] if end < len(content) else "\n"
    left = not _is_space(after) and (
        not _is_punctuation(after) or _is_space(before) or _is_punctuation(before)
    )
    right = not _is_space(before) and (
        not _is_punctuation(before) or _is_space(after) or _is_punctuation(after)
    )
    opens, closes = left, right
    if char == "_":
        opens = left and (not right or _is_punctuation(before))
        closes = right and (not left or _is_punctuation(after))
    elif char == "~" and end - start > 2:
        return None
    if not (opens or closes):
        return None
    return _Delimiter(char, end - start, end - start, opens, closes)


def _is_space(char: str) -> bool:
    return char in "\t\n\f\r " or unicodedata.category(char) == "Zs"


def _is_punctuation(char: str) -> bool:
    return char in _ASCII_PUNCTUATION or unicodedata.category(char).startswith("P")


def _emphasize(delimiters: Sequence[_Delimiter]) -> None:
    """Matches delimiters as cmark-gfm's process_emphasis does, leaving in
    each one's count the characters of it that render, and in its starts and
    ends the elements of its matches. A match removes the delimiters between
    its own, as strikethrough does even between runs of different lengths,
    which it leaves literal."""
    stack = list(range(len(delimiters)))
    bottoms: dict[tuple[str, int], int] = {}
    current = 0
    while current < len(stack):
        closer = delimiters[stack[current]]
        if not closer.closes:
            current += 1
            continue
        key = (closer.char, closer.length % 3)
        found = _opener(delimiters, stack, current, bottoms.get(key, 0))
        if found is None:
            bottoms[key] = stack[current]
            if closer.opens:
                current += 1
            else:
                del stack[current]
            continue
        opener = delimiters[stack[found]]
        del stack[found + 1 : current]
        current = found + 1
        if closer.char == "~":
            if opener.count == closer.count:
                opener.count = closer.count = 0
                opener.starts.append("del")
                closer.ends.append("del")
            del stack[found : current + 1]
            current = found
            continue
        used = 2 if opener.count >= 2 and closer.count >= 2 else 1
        opener.count -= used
        closer.count -= used
        opener.starts.append("strong" if used == 2 else "em")
        closer.ends.append("strong" if used == 2 else "em")
        if opener.count == 0:
            del stack[found]
            current -= 1
        if closer.count == 0:
            del stack[current]


def _opener(
    delimiters: Sequence[_Delimiter], stack: Sequence[int], current: int, bottom: int
) -> int | None:
    """Returns the index in stack of the nearest delimiter, from bottom, that
    the one at current closes, by the rule of three, or None."""
    closer = delimiters[stack[current]]
    for index in range(current - 1, -1, -1):
        if stack[index] < bottom:
            break
        opener = delimiters[stack[index]]
        if (
            opener.opens
            and opener.char == closer.char
            and (
                not (closer.opens or opener.closes)
                or closer.length % 3 == 0
                or (opener.length + closer.length) % 3 != 0
            )
        ):
            return index
    return None


def _is_markdown(path: str) -> bool:
    return path.lower().endswith((".md", ".markdown"))


def _git(directory: Path, *arguments: str) -> bytes:
    try:
        result = subprocess.run(
            ["git", *arguments], cwd=directory, capture_output=True, check=False
        )
    except OSError as error:
        raise CheckError(f"cannot run git: {error}") from error
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", "replace").strip()
        raise CheckError(f"git {arguments[0]} failed in {directory}: {detail}")
    return result.stdout


class _Tree:
    """The files that links resolve against and the directories that hold them."""

    def __init__(self, files: Iterable[str], read: Callable[[str], bytes]) -> None:
        self.files = frozenset(files)
        directories = {""}
        for path in self.files:
            parent = posixpath.dirname(path)
            while parent not in directories:
                directories.add(parent)
                parent = posixpath.dirname(parent)
        self.directories = frozenset(directories)
        self.casefolded = {path.casefold(): path for path in self.files | directories}
        # GitHub renders a directory's README, whatever its case, below the
        # directory's listing.
        self.readmes: dict[str, str] = {}
        for path in sorted(self.files):
            directory, name = posixpath.split(path)
            if name.casefold() in _README_NAMES and (
                directory not in self.readmes or name == "README.md"
            ):
                self.readmes[directory] = path
        self._read = read
        self._texts: dict[str, str | None] = {}
        self._documents: dict[str, MarkdownDocument] = {}

    def text(self, path: str) -> str | None:
        """Returns a file's text, or None for a binary or unreadable file."""
        if path not in self._texts:
            try:
                data = self._read(path)
            except OSError:
                data = None
            self._texts[path] = (
                None
                if data is None or b"\0" in data
                else normalize_newlines(data.decode("utf-8-sig", "replace"))
            )
        return self._texts[path]

    def document(self, path: str) -> MarkdownDocument:
        if path not in self._documents:
            self._documents[path] = parse_markdown(self.text(path) or "")
        return self._documents[path]


class Repository:
    """The tracked files of a Git work tree and its pinned submodules."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.submodules: dict[str, str] = {}
        files: list[str] = []
        for entry in _git(root, "ls-files", "--stage", "-z").split(b"\0"):
            if not entry:
                continue
            metadata, _, name = entry.partition(b"\t")
            mode, commit = metadata.decode("ascii").split()[:2]
            path = os.fsdecode(name)
            if mode == "160000":
                self.submodules[path] = commit
            elif os.path.lexists(root / path):
                files.append(path)
        self.tree = _Tree(files, lambda path: (root / path).read_bytes())
        self._submodule_trees: dict[str, _Tree | None] = {}

    def submodule_tree(self, path: str) -> _Tree | None:
        """Returns the pinned files of a submodule, or None when they are not
        available because the submodule is not checked out."""
        if path not in self._submodule_trees:
            self._submodule_trees[path] = self._load_submodule(path)
        return self._submodule_trees[path]

    def _load_submodule(self, path: str) -> _Tree | None:
        directory = self.root / path
        commit = self.submodules[path]
        if not (directory / ".git").exists():
            return None
        try:
            listing = _git(directory, "ls-tree", "-r", "-z", "--name-only", commit)
        except CheckError:
            return None
        return _Tree(
            (os.fsdecode(name) for name in listing.split(b"\0") if name),
            lambda name: _git(directory, "cat-file", "blob", f"{commit}:{name}"),
        )


class _Checker:
    def __init__(self, repository: Repository) -> None:
        self.repository = repository

    def resolve(
        self, source: str, base: str, target: str, image: bool = False
    ) -> str | None:
        """Returns why a link target of source, relative to the base
        directory, does not resolve, or None when it does. An empty target
        links to source itself, and the anchor of an image, such as GitHub's
        #gh-dark-mode-only, selects nothing to check."""
        if _EXTERNAL.match(target):
            return None
        reference, has_fragment, fragment = target.partition("#")
        reference, _, query = reference.partition("?")
        path = urllib.parse.unquote(reference)
        fragment = urllib.parse.unquote(fragment)
        resolved = source
        if path:
            # GitHub resolves a path that starts with / from the repository root.
            joined = path[1:] if path.startswith("/") else posixpath.join(base, path)
            resolved = posixpath.normpath(joined or ".")
            if resolved == ".." or resolved.startswith("../"):
                return f"{target} leaves the repository"
            resolved = "" if resolved == "." else resolved
        location = self._locate(resolved)
        if location is None:
            return None
        tree, inner = location
        anchored = not image and has_fragment and fragment.lower() not in ("", "top")
        if inner in tree.directories:
            if not anchored:
                return None
            readme = tree.readmes.get(inner)
            if readme is None:
                return f"{target}: {resolved or '.'} has no README"
            resolved = posixpath.join(resolved[: len(resolved) - len(inner)], readme)
            inner = readme
        elif path.endswith("/") and inner in tree.files:
            return f"{target}: {resolved} is not a directory"
        if inner not in tree.files:
            tracked = tree.casefolded.get(inner.casefold())
            if tracked is not None:
                prefix = resolved[: len(resolved) - len(inner)]
                reason = f"differs in case from {prefix}{tracked}"
            elif tree is self.repository.tree and os.path.lexists(
                self.repository.root / resolved
            ):
                reason = "is not tracked"
            else:
                reason = "does not exist"
            return f"{target}: {resolved} {reason}"
        if not anchored:
            return None
        if _is_markdown(inner) and "plain=1" not in query.split("&"):
            # GitHub's script looks the fragment up in lowercase, so an id or
            # name with an uppercase letter is out of its reach, and with the
            # prefix of the ids that GitHub renders, which it may already hold.
            if fragment.lower().removeprefix(_PREFIX) in tree.document(inner).anchors:
                return None
            return f"{target}: {resolved} has no heading or anchor #{fragment}"
        anchor = _LINE_ANCHOR.fullmatch(fragment)
        if anchor is None:
            return f"{target}: {resolved} has only line anchors such as #L1"
        text = tree.text(inner) or ""
        lines = text.count("\n") + (0 if text.endswith("\n") or not text else 1)
        if any(not 1 <= int(line) <= lines for line in anchor.groups() if line):
            return f"{target}: {resolved} ends at line {lines}"
        return None

    def _locate(self, path: str) -> tuple[_Tree, str] | None:
        """Returns the tree that holds a path and the path within it, or None
        for a path in a submodule that is not checked out."""
        for submodule in self.repository.submodules:
            if path == submodule or path.startswith(f"{submodule}/"):
                tree = self.repository.submodule_tree(submodule)
                return None if tree is None else (tree, path[len(submodule) + 1 :])
        return self.repository.tree, path

    def check(
        self, source: str, fixtures: Collection[str], needed: set[str]
    ) -> Iterator[Problem]:
        """Checks the links and doc/ paths of a tracked file, adding the
        fixtures that it needs to needed."""
        tree = self.repository.tree
        text = tree.text(source)
        if text is None:
            return
        links: tuple[Link, ...] = ()
        if _is_markdown(source):
            links = tree.document(source).links
            for link in links:
                message = self.resolve(
                    source, posixpath.dirname(source), link.target, link.image
                )
                if message is not None:
                    yield Problem(source, link.line, message)
        for match in _DOC_PATH.finditer(text):
            following = text[match.end() : match.end() + 1]
            if (
                following
                and not following.isspace()
                and following not in _DOC_PATH_ENDS
            ) or any(link.start <= match.start() < link.end for link in links):
                continue
            mention = match[0].rstrip(".")
            message = self.resolve(source, "", mention)
            if message is None:
                continue
            path = mention.partition("#")[0]
            if path in fixtures:
                needed.add(path)
            else:
                yield Problem(source, text.count("\n", 0, match.start()) + 1, message)


def check_repository(
    root: Path, fixtures: Mapping[str, Collection[str]] | None = None
) -> list[Problem]:
    """Returns the broken links and doc/ paths of the Git work tree at root.

    The fixtures default to DOC_PATH_FIXTURES.
    """
    if fixtures is None:
        fixtures = DOC_PATH_FIXTURES
    repository = Repository(root)
    checker = _Checker(repository)
    problems: list[Problem] = []
    try:
        own = Path(__file__).resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        own = None
    for source in sorted(repository.tree.files):
        listed = fixtures.get(source, ())
        # This file lists the fixtures, so it mentions every one of them.
        allowed = (
            {path for paths in fixtures.values() for path in paths}
            if source == own
            else listed
        )
        needed: set[str] = set()
        problems.extend(checker.check(source, allowed, needed))
        problems.extend(
            Problem(
                source,
                0,
                f"DOC_PATH_FIXTURES lists {path}, which this file no longer "
                "mentions as a missing document",
            )
            for path in set(listed) - needed
        )
    problems.extend(
        Problem(source, 0, "DOC_PATH_FIXTURES lists this file, which is not tracked")
        for source in set(fixtures) - repository.tree.files
    )
    return sorted(problems)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Check the relative links and anchors of the tracked "
        "Markdown files and the doc/ paths that tracked files mention."
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=REPO_ROOT,
        help="Git work tree to check (default: this repository)",
    )
    args = parser.parse_args(argv)
    try:
        problems = check_repository(args.root)
    except CheckError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    annotate = os.environ.get("GITHUB_ACTIONS") == "true"
    for problem in problems:
        print(problem.annotation() if annotate else problem)
    if problems:
        print(f"error: {len(problems)} broken link(s) or doc/ path(s)", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

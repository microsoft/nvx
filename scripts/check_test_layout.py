#!/usr/bin/env python3
"""Check that tests live apart from the code that they test.

A Rust crate of the repository, a directory with a tracked Cargo.toml, keeps
its tests in test files: the files beneath the crate's `tests` directory, and
in `src/`, the `tests.rs` files and the files beneath a `tests` directory. The
crate's other files in `src/` are production files. A production file
declares its unit tests only as an out-of-line module named `tests` whose cfg
requires `test`, and whose file holds them:

    #[cfg(test)]
    mod tests;

A test file belongs to a module that a target of its crate whose tests
`cargo test` runs, its library, a binary, or an integration test, as the
crate's manifest names it or Cargo discovers it, reaches through `mod`
declarations from its root that builds of tests compile: no cfg that no build
of tests satisfies, such as `any()` or `not(test)`, guards one. The Rust
files of `tests/fixtures/` and `tests/vectors/` are data, not modules.

A production file holds no other test code: outside function bodies, no
inline module, module of another name, or item that a cfg requiring `test`
guards, directly or through cfg_attr; no test attribute, such as #[test],
#[should_panic], or #[ignore]; no proptest! block; and no #![cfg(test)] that
makes the whole file test-only. What it may hold are test hooks, the
attributes and cfg! invocations whose predicates mention `test`, such as a
field, statement, or match arm that only tests compile, or a derive that only
tests need: exactly as many as the seams of scripts/test-layout-allowlist.json
allow the file. A hook is test code that cannot move out of the code that it
hooks. `--tighten` lowers the seams to the hooks that the files hold; it never
raises an entry or adds a file.

Every module named `tests` is out of line and requires `test`, and its file
exists. No file in `src/` sets a module's file with #[path].

Python unit tests are the `test_*.py` modules of scripts/,
.github/specula/tests/, and .github/skills/*/scripts/, and the end-to-end
scenario drivers are the `*_tests.py` modules of scripts/nvx_tools/. No other
module imports unittest, pytest, doctest, a `test_*` module, or the
PowerShell test support of powershell_testing.py.

The checks read the files that Git tracks, so stage a new file before you run
them.
"""

from __future__ import annotations

import argparse
import ast
import bisect
import json
import os
import posixpath
import re
import subprocess
import sys
from collections.abc import Collection, Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Protocol, cast

REPO_ROOT = Path(__file__).resolve().parents[1]
ALLOWLIST = "scripts/test-layout-allowlist.json"
ALLOWLIST_SCHEMA = "nvx-test-layout-allowlist"
ALLOWLIST_SCHEMA_VERSION = 2
TIGHTEN_COMMAND = "python3 scripts/check_test_layout.py --tighten"

# The directories, relative to a crate, of the data of its tests, such as
# fixtures and conformance vectors, whose Rust files are no modules.
TEST_DATA_DIRECTORIES = ("tests/fixtures/", "tests/vectors/")

# The places of the Python unit tests and of the end-to-end scenario drivers,
# and the modules that only the unit tests import.
PYTHON_UNIT_TESTS = re.compile(
    r"(?:scripts|\.github/specula/tests|\.github/skills/[^/]+/scripts)"
    r"/test_[^/]*\.py"
)
PYTHON_END_TO_END_TESTS = re.compile(r"scripts/nvx_tools/[^/]*_tests\.py")
PYTHON_TEST_SUPPORT = frozenset({"scripts/powershell_testing.py"})
PYTHON_TEST_FRAMEWORKS = frozenset({"doctest", "pytest", "unittest"})

# Attributes that only tests take, and those of them that make a function a
# test.
TEST_ATTRIBUTES = frozenset({"bench", "ignore", "should_panic", "test"})
TEST_FUNCTION_ATTRIBUTES = frozenset({"bench", "test"})

IDENT = "ident"
LIFETIME = "lifetime"
LITERAL = "literal"
PUNCT = "punct"


class CheckError(RuntimeError):
    """Raised when the files of the repository cannot be listed or read."""


@dataclass(frozen=True, order=True)
class Problem:
    """A place where test code is not where the layout puts it."""

    path: str
    line: int
    rule: str
    message: str

    def __str__(self) -> str:
        location = f"{self.path}:{self.line}" if self.line else self.path
        return f"{location}: {self.rule}: {self.message}"

    def annotation(self) -> str:
        """Formats the problem as a GitHub Actions error annotation."""
        properties = f"file={_escape_property(self.path)}"
        if self.line:
            properties += f",line={self.line}"
        return f"::error {properties}::{_escape_data(str(self))}"


def _escape_data(value: str) -> str:
    return value.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def _escape_property(value: str) -> str:
    return _escape_data(value).replace(":", "%3A").replace(",", "%2C")


# Rust tokens.


class RustSyntaxError(ValueError):
    """Raised when a Rust file does not split into tokens whose brackets
    balance."""

    def __init__(self, line: int, message: str) -> None:
        super().__init__(f"line {line}: {message}")
        self.line = line


@dataclass(frozen=True)
class Token:
    """A token of Rust source: an identifier, which includes keywords and raw
    identifiers; a lifetime or label; a literal; or one punctuation
    character. Comments, doc comments included, are no tokens."""

    kind: str
    text: str
    start: int
    end: int
    line: int


_SPACE_OR_LINE_COMMENT = re.compile(r"(?:\s+|//[^\n]*)+")
_BLOCK_COMMENT_DELIMITER = re.compile(r"/\*|\*/")
_IDENTIFIER = re.compile(r"[^\W\d]\w*")
_NUMBER = re.compile(r"[0-9]\w*(?:\.[0-9]\w*)?")
_STRING_REST = re.compile(r'[^"\\]*(?:\\.[^"\\]*)*"', re.DOTALL)
_CHAR = re.compile(
    r"'(?:[^'\\\n]|\\(?:u\{[0-9A-Fa-f_]*\}|x[0-9A-Fa-f]{2}|.))'", re.DOTALL
)
_LIFETIME = re.compile(r"'(?:r#)?[^\W\d]\w*")


def tokenize_rust(text: str) -> list[Token]:
    """Splits Rust source into tokens, without its comments."""
    newlines = [match.start() for match in re.finditer("\n", text)]

    def line_of(offset: int) -> int:
        return bisect.bisect_right(newlines, offset) + 1

    tokens: list[Token] = []

    def add(kind: str, start: int, end: int) -> int:
        tokens.append(Token(kind, text[start:end], start, end, line_of(start)))
        return end

    position = 1 if text.startswith("\ufeff") else 0
    size = len(text)
    while position < size:
        skipped = _SPACE_OR_LINE_COMMENT.match(text, position)
        if skipped is not None:
            position = skipped.end()
            continue
        if text.startswith("/*", position):
            position = _block_comment_end(text, position, line_of(position))
            continue
        char = text[position]
        if char == '"':
            position = add(
                LITERAL, position, _string_end(text, position, position + 1, line_of)
            )
            continue
        if char == "'":
            literal = _CHAR.match(text, position)
            if literal is not None:
                position = add(LITERAL, position, literal.end())
                continue
            lifetime = _LIFETIME.match(text, position)
            if lifetime is not None:
                position = add(LIFETIME, position, lifetime.end())
                continue
            position = add(PUNCT, position, position + 1)
            continue
        if "0" <= char <= "9":
            number = _NUMBER.match(text, position)
            assert number is not None
            position = add(LITERAL, position, number.end())
            continue
        word = _IDENTIFIER.match(text, position)
        if word is None:
            position = add(PUNCT, position, position + 1)
            continue
        end = word.end()
        prefix = word.group()
        following = text[end] if end < size else ""
        if prefix in ("r", "br", "cr") and following in ('"', "#"):
            hashes = end
            while hashes < size and text[hashes] == "#":
                hashes += 1
            if hashes < size and text[hashes] == '"':
                delimiter = '"' + "#" * (hashes - end)
                close = text.find(delimiter, hashes + 1)
                if close < 0:
                    raise RustSyntaxError(line_of(position), "unterminated raw string")
                position = add(LITERAL, position, close + len(delimiter))
                continue
            if prefix == "r" and hashes == end + 1:
                raw = _IDENTIFIER.match(text, hashes)
                if raw is not None:
                    position = add(IDENT, position, raw.end())
                    continue
        elif prefix in ("b", "c") and following == '"':
            position = add(
                LITERAL, position, _string_end(text, position, end + 1, line_of)
            )
            continue
        elif prefix == "b" and following == "'":
            literal = _CHAR.match(text, end)
            if literal is not None:
                position = add(LITERAL, position, literal.end())
                continue
        position = add(IDENT, position, end)
    return tokens


def _block_comment_end(text: str, start: int, line: int) -> int:
    depth = 0
    position = start
    while True:
        delimiter = _BLOCK_COMMENT_DELIMITER.search(text, position)
        if delimiter is None:
            raise RustSyntaxError(line, "unterminated block comment")
        depth += 1 if delimiter.group() == "/*" else -1
        position = delimiter.end()
        if depth == 0:
            return position


def _string_end(text: str, start: int, body: int, line_of: _LineOf) -> int:
    rest = _STRING_REST.match(text, body)
    if rest is None:
        raise RustSyntaxError(line_of(start), "unterminated string")
    return rest.end()


class _LineOf(Protocol):
    def __call__(self, offset: int, /) -> int: ...


_CLOSERS = {")": "(", "]": "[", "}": "{"}


def match_brackets(tokens: Sequence[Token]) -> list[int]:
    """Returns, for each token, the index of the bracket that it opens or
    closes, or -1."""
    partner = [-1] * len(tokens)
    stack: list[int] = []
    for index, token in enumerate(tokens):
        if token.kind != PUNCT:
            continue
        if token.text in "([{":
            stack.append(index)
        elif token.text in _CLOSERS:
            if not stack or tokens[stack[-1]].text != _CLOSERS[token.text]:
                raise RustSyntaxError(token.line, f"unbalanced {token.text!r}")
            opener = stack.pop()
            partner[opener] = index
            partner[index] = opener
    if stack:
        raise RustSyntaxError(tokens[stack[-1]].line, "unclosed bracket")
    return partner


def unraw(name: str) -> str:
    """Returns an identifier without its raw prefix, `r#`."""
    return name[2:] if name.startswith("r#") else name


def is_token(tokens: Sequence[Token], index: int, kind: str, *texts: str) -> bool:
    """Whether the token at index is of kind and, if any texts are given, one
    of them."""
    if index >= len(tokens):
        return False
    token = tokens[index]
    return token.kind == kind and (not texts or token.text in texts)


def _path(tokens: Sequence[Token], start: int, end: int) -> tuple[str, int]:
    """Reads a path of identifiers, such as `tokio::test`, from start; returns
    it, empty when none starts there, and the index after it."""
    index = start
    if is_token(tokens, index, PUNCT, ":") and is_token(tokens, index + 1, PUNCT, ":"):
        index += 2
    parts: list[str] = []
    while index < end and is_token(tokens, index, IDENT):
        parts.append(unraw(tokens[index].text))
        index += 1
        if (
            index + 2 < end
            and is_token(tokens, index, PUNCT, ":")
            and is_token(tokens, index + 1, PUNCT, ":")
            and is_token(tokens, index + 2, IDENT)
        ):
            index += 2
            continue
        break
    if not parts:
        return "", start
    return "::".join(parts), index


def split_commas(
    tokens: Sequence[Token], partner: Sequence[int], start: int, end: int
) -> list[tuple[int, int]]:
    """Splits the tokens from start to end at the commas outside brackets."""
    pieces: list[tuple[int, int]] = []
    piece = start
    index = start
    while index < end:
        token = tokens[index]
        if token.kind == PUNCT:
            if token.text in "([{":
                index = partner[index] + 1
                continue
            if token.text == ",":
                pieces.append((piece, index))
                piece = index + 1
        index += 1
    pieces.append((piece, end))
    return [(first, last) for first, last in pieces if first < last]


# cfg predicates and attributes.


@dataclass(frozen=True)
class Cfg:
    """A cfg predicate: `all`, `any`, or `not` of operands; an `option`, with
    the literal of its value if it has one; or `other`, whatever does not
    parse, with its tokens."""

    operator: str
    name: str = ""
    value: str | None = None
    operands: tuple[Cfg, ...] = ()
    words: tuple[str, ...] = ()


def parse_cfg(
    tokens: Sequence[Token], partner: Sequence[int], start: int, end: int
) -> Cfg:
    """Parses the cfg predicate from start to end."""
    if start < end and tokens[start].kind == IDENT:
        name = unraw(tokens[start].text)
        if start + 1 == end:
            return Cfg("option", name)
        if (
            name in ("all", "any", "not")
            and is_token(tokens, start + 1, PUNCT, "(")
            and partner[start + 1] == end - 1
        ):
            operands = tuple(
                parse_cfg(tokens, partner, first, last)
                for first, last in split_commas(tokens, partner, start + 2, end - 1)
            )
            return Cfg(name, operands=operands)
        if (
            start + 3 == end
            and is_token(tokens, start + 1, PUNCT, "=")
            and is_token(tokens, start + 2, LITERAL)
        ):
            return Cfg("option", name, tokens[start + 2].text)
    words = tuple(
        token.text
        for index, token in enumerate(tokens[start:end], start)
        if token.kind == IDENT
        and unraw(token.text) == "test"
        and not is_token(tokens, index - 1, PUNCT, "$")
        and not is_token(tokens, index + 1, PUNCT, "=")
    )
    return Cfg("other", words=words)


def mentions_test(cfg: Cfg) -> bool:
    """Whether the predicate depends on the `test` option."""
    if cfg.operator == "option":
        return cfg.name == "test" and cfg.value is None
    if cfg.operator == "other":
        return bool(cfg.words)
    return any(mentions_test(operand) for operand in cfg.operands)


def requires_test(cfg: Cfg) -> bool:
    """Whether the predicate holds only when `test` does."""
    if cfg.operator == "option":
        return cfg.name == "test" and cfg.value is None
    if cfg.operator == "all":
        return any(requires_test(operand) for operand in cfg.operands)
    if cfg.operator == "any":
        live = [operand for operand in cfg.operands if not _never(operand)]
        return bool(live) and all(requires_test(operand) for operand in live)
    if cfg.operator == "not":
        return len(cfg.operands) == 1 and forbids_test(cfg.operands[0])
    return False


def forbids_test(cfg: Cfg) -> bool:
    """Whether the predicate holds only when `test` does not."""
    if cfg.operator == "all":
        return any(forbids_test(operand) for operand in cfg.operands)
    if cfg.operator == "any":
        live = [operand for operand in cfg.operands if not _never(operand)]
        return bool(live) and all(forbids_test(operand) for operand in live)
    if cfg.operator == "not":
        return len(cfg.operands) == 1 and requires_test(cfg.operands[0])
    return False


def _never(cfg: Cfg) -> bool:
    """Whether the predicate holds for no configuration, as `any()` does."""
    if cfg.operator == "any":
        return all(_never(operand) for operand in cfg.operands)
    if cfg.operator == "all":
        return any(_never(operand) for operand in cfg.operands)
    if cfg.operator == "not":
        return len(cfg.operands) == 1 and _always(cfg.operands[0])
    return False


def _always(cfg: Cfg) -> bool:
    """Whether the predicate holds for every configuration, as `all()` does."""
    if cfg.operator == "all":
        return all(_always(operand) for operand in cfg.operands)
    if cfg.operator == "any":
        return any(_always(operand) for operand in cfg.operands)
    if cfg.operator == "not":
        return len(cfg.operands) == 1 and _never(cfg.operands[0])
    return False


def excludes_tests(cfg: Cfg) -> bool:
    """Whether the predicate holds in no build of tests: for no configuration,
    as `any()`, or only where `test` does not, as `not(test)`."""
    return _never(cfg) or forbids_test(cfg)


@dataclass(frozen=True)
class Attribute:
    """An attribute: the indices of its `#` and of its closing `]`, its path,
    such as `cfg` or `tokio::test`, empty when it starts with none, and the
    range of the tokens between the delimiters after its path, if any."""

    start: int
    end: int
    inner: bool
    path: str
    arguments: tuple[int, int] | None
    line: int


def attribute_at(
    tokens: Sequence[Token], partner: Sequence[int], index: int
) -> Attribute | None:
    """Returns the attribute whose `#` is at index, if one is."""
    if not is_token(tokens, index, PUNCT, "#"):
        return None
    bracket = index + 1
    inner = is_token(tokens, bracket, PUNCT, "!")
    if inner:
        bracket += 1
    if not is_token(tokens, bracket, PUNCT, "["):
        return None
    close = partner[bracket]
    path, after = _path(tokens, bracket + 1, close)
    arguments = None
    if path and after < close and is_token(tokens, after, PUNCT, "(", "[", "{"):
        arguments = (after + 1, partner[after])
    return Attribute(index, close, inner, path, arguments, tokens[index].line)


def attribute_cfg(
    tokens: Sequence[Token], partner: Sequence[int], attribute: Attribute
) -> Cfg | None:
    """Returns the predicate of a #[cfg(...)] attribute."""
    if attribute.path != "cfg" or attribute.arguments is None:
        return None
    return parse_cfg(tokens, partner, *attribute.arguments)


def attribute_requires_test(
    tokens: Sequence[Token], partner: Sequence[int], attribute: Attribute
) -> bool:
    """Whether the attribute is a cfg that only lets tests compile what it
    guards, or a cfg_attr that applies such a cfg wherever its predicate
    holds: `cfg_attr(P, cfg(X))` guards with `any(not(P), X)`."""
    if attribute.arguments is None:
        return False
    if attribute.path == "cfg":
        return requires_test(parse_cfg(tokens, partner, *attribute.arguments))
    if attribute.path == "cfg_attr":
        return any(
            requires_test(cfg)
            for cfg in _cfg_attr_guards(tokens, partner, *attribute.arguments, ())
        )
    return False


def attribute_excludes_tests(
    tokens: Sequence[Token], partner: Sequence[int], attribute: Attribute
) -> bool:
    """Whether the attribute is a cfg that no build of tests satisfies, or a
    cfg_attr that applies one wherever tests build, so that what it is on
    never compiles for tests, as with #[cfg(any())] or
    #[cfg_attr(test, cfg(not(test)))]."""
    if attribute.arguments is None:
        return False
    if attribute.path == "cfg":
        return excludes_tests(parse_cfg(tokens, partner, *attribute.arguments))
    if attribute.path == "cfg_attr":
        return any(
            excludes_tests(cfg)
            for cfg in _cfg_attr_guards(tokens, partner, *attribute.arguments, ())
        )
    return False


def _cfg_attr_guards(
    tokens: Sequence[Token],
    partner: Sequence[int],
    start: int,
    end: int,
    held: tuple[Cfg, ...],
) -> list[Cfg]:
    """Returns the predicates with which the cfgs that the cfg_attr from start
    to end applies, where held holds too, guard what it is on."""
    pieces = split_commas(tokens, partner, start, end)
    if not pieces:
        return []
    held = (*held, parse_cfg(tokens, partner, *pieces[0]))
    where = held[0] if len(held) == 1 else Cfg("all", operands=held)
    guards: list[Cfg] = []
    for first, last in pieces[1:]:
        path, after = _path(tokens, first, last)
        if not is_token(tokens, after, PUNCT, "(") or partner[after] != last - 1:
            continue
        if path == "cfg":
            applied = parse_cfg(tokens, partner, after + 1, partner[after])
            guards.append(Cfg("any", operands=(Cfg("not", operands=(where,)), applied)))
        elif path == "cfg_attr":
            guards.extend(
                _cfg_attr_guards(tokens, partner, after + 1, partner[after], held)
            )
    return guards


def attribute_mentions_test(
    tokens: Sequence[Token], partner: Sequence[int], attribute: Attribute
) -> bool:
    """Whether the attribute is a cfg or cfg_attr whose predicate, or that of
    an attribute that it applies, depends on `test`."""
    if attribute.arguments is None:
        return False
    if attribute.path == "cfg":
        return mentions_test(parse_cfg(tokens, partner, *attribute.arguments))
    if attribute.path == "cfg_attr":
        return _cfg_attr_mentions_test(tokens, partner, *attribute.arguments)
    return False


def _cfg_attr_mentions_test(
    tokens: Sequence[Token], partner: Sequence[int], start: int, end: int
) -> bool:
    pieces = split_commas(tokens, partner, start, end)
    if not pieces:
        return False
    if mentions_test(parse_cfg(tokens, partner, *pieces[0])):
        return True
    for first, last in pieces[1:]:
        path, after = _path(tokens, first, last)
        if not is_token(tokens, after, PUNCT, "(") or partner[after] != last - 1:
            continue
        if path == "cfg" and mentions_test(
            parse_cfg(tokens, partner, after + 1, last - 1)
        ):
            return True
        if path == "cfg_attr" and _cfg_attr_mentions_test(
            tokens, partner, after + 1, last - 1
        ):
            return True
    return False


def attribute_paths(
    tokens: Sequence[Token], partner: Sequence[int], attribute: Attribute
) -> list[str]:
    """Returns the paths of the attribute and of those that it applies through
    cfg_attr."""
    paths = [attribute.path]
    if attribute.path == "cfg_attr" and attribute.arguments is not None:
        paths.extend(_cfg_attr_paths(tokens, partner, *attribute.arguments))
    return paths


def _cfg_attr_paths(
    tokens: Sequence[Token], partner: Sequence[int], start: int, end: int
) -> list[str]:
    paths: list[str] = []
    for first, last in split_commas(tokens, partner, start, end)[1:]:
        path, after = _path(tokens, first, last)
        paths.append(path)
        if path == "cfg_attr" and is_token(tokens, after, PUNCT, "("):
            paths.extend(_cfg_attr_paths(tokens, partner, after + 1, partner[after]))
    return paths


def is_test_attribute(path: str) -> bool:
    """Whether an attribute's path makes a test, such as #[test] or
    #[tokio::test], or applies only to tests, such as #[ignore]."""
    return path in TEST_ATTRIBUTES or path.endswith("::test")


def is_test_function_attribute(path: str) -> bool:
    """Whether an attribute's path makes its function a test or benchmark."""
    return path in TEST_FUNCTION_ATTRIBUTES or path.endswith("::test")


# Rust items.


@dataclass
class Item:
    """An item: a module, function, type, impl, use, macro invocation, or
    another. Its tokens run from start, its first attribute, through end,
    exclusive; head is the first token after its attributes. A module, impl,
    trait, extern block, or proptest! block holds the items between the braces
    of its body."""

    kind: str
    name: str
    start: int
    head: int
    end: int
    line: int
    attributes: tuple[Attribute, ...]
    body: tuple[int, int] | None = None
    inner_attributes: tuple[Attribute, ...] = ()
    children: list[Item] = field(default_factory=list["Item"])


_QUALIFIERS = frozenset({"async", "auto", "default", "gen", "safe", "unsafe"})
_FUNCTION_QUALIFIERS = frozenset({"async", "extern", "fn", "safe", "unsafe"})


class _ItemParser:
    def __init__(self, tokens: Sequence[Token], partner: Sequence[int]) -> None:
        self.tokens = tokens
        self.partner = partner

    def block(self, start: int, end: int) -> tuple[tuple[Attribute, ...], list[Item]]:
        """Parses the inner attributes and the items from start to end."""
        inner: list[Attribute] = []
        index = start
        while index < end:
            attribute = attribute_at(self.tokens, self.partner, index)
            if attribute is None or not attribute.inner:
                break
            inner.append(attribute)
            index = attribute.end + 1
        items: list[Item] = []
        while index < end:
            if is_token(self.tokens, index, PUNCT, ";"):
                index += 1
                continue
            item = self.item(index, end)
            items.append(item)
            index = item.end
        return tuple(inner), items

    def item(self, start: int, end: int) -> Item:
        tokens = self.tokens
        index = start
        attributes: list[Attribute] = []
        while index < end:
            attribute = attribute_at(tokens, self.partner, index)
            if attribute is None:
                break
            attributes.append(attribute)
            index = attribute.end + 1
        head = index
        line = tokens[head].line if head < end else tokens[start].line

        def make(
            kind: str,
            name: str,
            stop: int,
            body: tuple[int, int] | None = None,
            children: bool = False,
        ) -> Item:
            item = Item(
                kind, name, start, head, max(stop, head + 1), line, tuple(attributes)
            )
            if body is not None:
                item.body = body
                if children:
                    item.inner_attributes, item.children = self.block(
                        body[0] + 1, body[1]
                    )
            return item

        if head >= end:
            return Item("other", "", start, head, end, line, tuple(attributes))
        if is_token(tokens, index, IDENT, "pub"):
            index += 1
            if is_token(tokens, index, PUNCT, "("):
                index = self.partner[index] + 1
        keyword = index
        while keyword < end and is_token(tokens, keyword, IDENT):
            word = tokens[keyword].text
            if word in _QUALIFIERS and is_token(tokens, keyword + 1, IDENT):
                keyword += 1
            elif word == "const" and is_token(
                tokens, keyword + 1, IDENT, *_FUNCTION_QUALIFIERS
            ):
                keyword += 1
            elif word == "extern" and not is_token(tokens, keyword + 1, IDENT, "crate"):
                after = keyword + 1
                if is_token(tokens, after, LITERAL):
                    after += 1
                if is_token(tokens, after, PUNCT, "{"):
                    return make(
                        "extern",
                        "",
                        self.partner[after] + 1,
                        (after, self.partner[after]),
                        True,
                    )
                keyword = after
            else:
                break
        word = tokens[keyword].text if is_token(tokens, keyword, IDENT) else ""
        name = (
            unraw(tokens[keyword + 1].text)
            if is_token(tokens, keyword + 1, IDENT)
            else ""
        )
        if word == "extern" and is_token(tokens, keyword + 1, IDENT, "crate"):
            name = (
                unraw(tokens[keyword + 2].text)
                if is_token(tokens, keyword + 2, IDENT)
                else ""
            )
            return make("extern crate", name, self._scan(keyword, end, False)[0])
        if word == "mod" and name:
            if is_token(tokens, keyword + 2, PUNCT, ";"):
                return make("mod", name, keyword + 3)
            if is_token(tokens, keyword + 2, PUNCT, "{"):
                close = self.partner[keyword + 2]
                return make("mod", name, close + 1, (keyword + 2, close), True)
        if word in ("fn", "struct", "enum", "trait") or (word == "union" and name):
            stop, brace = self._scan(keyword + 1, end, True, True)
            if word == "trait" and brace is not None:
                return make("trait", name, stop, (brace, stop - 1), True)
            return make(word, name, stop, None if brace is None else (brace, stop - 1))
        if word == "impl":
            stop, brace = self._scan(keyword + 1, end, True, True)
            header = " ".join(
                token.text
                for token in tokens[keyword : (brace if brace is not None else stop)]
            )
            if brace is None:
                return make("impl", header, stop)
            return make("impl", header, stop, (brace, stop - 1), True)
        if word in ("type", "const", "static", "use"):
            if word == "static" and is_token(tokens, keyword + 1, IDENT, "mut"):
                name = (
                    unraw(tokens[keyword + 2].text)
                    if is_token(tokens, keyword + 2, IDENT)
                    else ""
                )
            return make(
                word, "" if word == "use" else name, self._scan(keyword, end, False)[0]
            )
        path, after = _path(tokens, keyword, end)
        if path and is_token(tokens, after, PUNCT, "!"):
            group = after + 1
            if path == "macro_rules" and is_token(tokens, group, IDENT):
                name = unraw(tokens[group].text)
                group += 1
            else:
                name = path
            if is_token(tokens, group, PUNCT, "(", "[", "{"):
                close = self.partner[group]
                stop = close + 1
                brace = tokens[group].text == "{"
                if not brace and is_token(tokens, stop, PUNCT, ";"):
                    stop += 1
                kind = "macro_rules" if path == "macro_rules" else "macro"
                if brace and path.split("::")[-1] == "proptest":
                    return make(kind, name, stop, (group, close), True)
                return make(kind, name, stop, (group, close) if brace else None)
        return make("other", "", self._scan(head, end, True)[0])

    def _scan(
        self, index: int, end: int, braces: bool, generics: bool = False
    ) -> tuple[int, int | None]:
        """Returns the index after the item that runs on from index, and the
        index of the brace that opens its body if it has one: the item ends
        with the first `;` outside brackets or, if braces, with the first
        `{...}` group outside them. In the header of a generic item, the
        braces of a const argument inside `<...>`, as in `S<{ 1 }>`, open no
        body."""
        tokens = self.tokens
        start = index
        angles = 0
        while index < end:
            token = tokens[index]
            if token.kind == PUNCT:
                if token.text == ";" and not angles:
                    return index + 1, None
                if token.text == "{" and braces and not angles:
                    return self.partner[index] + 1, index
                if token.text in "([{":
                    index = self.partner[index] + 1
                    continue
                if generics and token.text == "<":
                    angles += 1
                elif (
                    generics
                    and token.text == ">"
                    and angles
                    and not is_token(tokens, index - 1, PUNCT, "-", "=")
                ):
                    angles -= 1
            index += 1
        if angles:
            # A `<` that no `>` closes is no generic argument list.
            return self._scan(start, end, braces)
        return end, None


@dataclass
class RustFile:
    """A parsed Rust file: its tokens, the partners of its brackets, its inner
    attributes and items, and every attribute and cfg! invocation in it, with
    the index of the invocation's `cfg` and its predicate."""

    tokens: list[Token]
    partner: list[int]
    inner_attributes: tuple[Attribute, ...]
    items: list[Item]
    attributes: list[Attribute]
    cfg_macros: list[tuple[int, Cfg]]


def parse_rust(text: str) -> RustFile:
    """Parses Rust source; raises RustSyntaxError when it does not tokenize or
    its brackets do not balance."""
    tokens = tokenize_rust(text)
    partner = match_brackets(tokens)
    inner, items = _ItemParser(tokens, partner).block(0, len(tokens))
    attributes: list[Attribute] = []
    cfg_macros: list[tuple[int, Cfg]] = []
    for index, token in enumerate(tokens):
        if token.kind == PUNCT and token.text == "#":
            attribute = attribute_at(tokens, partner, index)
            if attribute is not None:
                attributes.append(attribute)
        elif (
            token.kind == IDENT
            and token.text == "cfg"
            and is_token(tokens, index + 1, PUNCT, "!")
            and is_token(tokens, index + 2, PUNCT, "(", "[", "{")
        ):
            group = index + 2
            cfg_macros.append(
                (index, parse_cfg(tokens, partner, group + 1, partner[group]))
            )
    return RustFile(tokens, partner, inner, items, attributes, cfg_macros)


# The files of a repository.


class Snapshot(Protocol):
    """The tracked files of a tree, which may be a work tree or a commit."""

    @property
    def files(self) -> frozenset[str]: ...

    def read(self, path: str) -> bytes: ...


def git(root: Path, *arguments: str, stdin: bytes | None = None) -> bytes:
    """Runs git in root and returns what it printed; raises CheckError when it
    fails."""
    try:
        result = subprocess.run(
            ["git", *arguments],
            cwd=root,
            input=stdin,
            capture_output=True,
            check=False,
        )
    except OSError as error:
        raise CheckError(f"cannot run git: {error}") from error
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", "replace").strip()
        raise CheckError(f"git {arguments[0]} failed in {root}: {detail}")
    return result.stdout


class WorkTree:
    """The tracked files of a Git work tree, outside its submodules."""

    def __init__(self, root: Path) -> None:
        self.root = root
        files: set[str] = set()
        for entry in git(root, "ls-files", "--stage", "-z").split(b"\0"):
            if not entry:
                continue
            metadata, _, name = entry.partition(b"\t")
            path = os.fsdecode(name)
            if metadata.split()[0] != b"160000" and os.path.lexists(root / path):
                files.add(path)
        self._files = frozenset(files)

    @property
    def files(self) -> frozenset[str]:
        return self._files

    def read(self, path: str) -> bytes:
        try:
            return (self.root / path).read_bytes()
        except OSError as error:
            raise CheckError(f"cannot read {path}: {error}") from error


def decode(data: bytes) -> str:
    """Decodes a source file's bytes, with its newlines normalized."""
    return data.decode("utf-8-sig", "replace").replace("\r\n", "\n").replace("\r", "\n")


def rust_crates(files: Iterable[str]) -> tuple[str, ...]:
    """Returns the directories of the tracked Cargo.toml files, the crates."""
    return tuple(
        sorted(
            {
                posixpath.dirname(path)
                for path in files
                if posixpath.basename(path) == "Cargo.toml"
            }
        )
    )


_TOML_TABLE = re.compile(r"^\s*(\[\[?)\s*([^\]]*?)\s*\]\]?\s*(?:#.*)?$")
_TOML_KEY = re.compile(r"""^\s*([A-Za-z0-9_.-]+|"[^"]*"|'[^']*')\s*=\s*(.*?)\s*$""")
_TOML_VALUE = re.compile(
    r"""(?:"((?:[^"\\]|\\.)*)"|'([^']*)'|(true|false))\s*(?:#.*)?"""
)
# The directories where Cargo discovers each kind of target but the library,
# and the package settings that turn its discovery of each kind off.
_TARGET_DIRECTORIES = {
    "bin": "src/bin",
    "test": "tests",
    "bench": "benches",
    "example": "examples",
}
_DISCOVERY = {
    "lib": "autolib",
    "bin": "autobins",
    "test": "autotests",
    "bench": "autobenches",
    "example": "autoexamples",
}
# The keys of the package and target tables that target_roots reads.
_TARGET_KEYS = frozenset({"name", "path", "test", *_DISCOVERY.values()})


def _unquoted(key: str) -> str:
    """Returns a TOML key or table name of one part without its quotes."""
    if len(key) >= 2 and key[0] == key[-1] and key[0] in "\"'":
        return key[1:-1]
    return key


def _toml_value(text: str) -> str | bool | None:
    """Returns the string or boolean that a TOML value on one line spells, or
    None for any other value."""
    found = _TOML_VALUE.fullmatch(text)
    if found is None:
        return None
    basic, literal, boolean = found.groups()
    if literal is not None:
        return literal
    if boolean is not None:
        return boolean == "true"
    try:
        # A basic string's escapes are JSON's, but for \U and \e.
        return cast(str, json.loads(f'"{basic}"'))
    except json.JSONDecodeError:
        return None


def target_roots(
    manifest: str, files: Collection[str], unread: list[tuple[int, str]] | None = None
) -> dict[str, bool]:
    """Returns the root files, relative to the crate, of the targets of a
    Cargo manifest, each with whether `cargo test` runs its tests: the
    library, the binaries, the integration tests, the benchmarks, and the
    examples that its tables name, at their `path` or where Cargo infers it,
    and that Cargo discovers among files, the crate's files relative to it,
    unless the package turns that off. `cargo test` runs the tests of the
    library, the binaries, and the integration tests whose `test` key is not
    false, and of the benchmarks and examples whose `test` key is true. Reads
    the keys of the package and target tables that Cargo.toml files spell one
    per line, with a string or a boolean, and appends to unread the line and
    the problem of each declaration of a target that it cannot read, such as
    an inline table, which it ignores."""
    package: dict[str, str | bool] = {}
    tables: list[tuple[str, dict[str, str | bool]]] = []
    current: dict[str, str | bool] | None = None
    section: str | None = None
    for number, line in enumerate(manifest.splitlines(), 1):
        table = _TOML_TABLE.match(line)
        if table:
            array, section = table.group(1) == "[[", _unquoted(table.group(2))
            current = None
            if not array and section == "package":
                current = package
            elif (array and section in _TARGET_DIRECTORIES) or (
                not array and section == "lib"
            ):
                current = {}
                tables.append((section, current))
            continue
        assignment = _TOML_KEY.match(line)
        if assignment is None:
            continue
        key, value = _unquoted(assignment.group(1)), assignment.group(2)
        if section is None and key.split(".")[0].strip("\"'") in (
            "package",
            "lib",
            *_TARGET_DIRECTORIES,
        ):
            if unread is not None:
                unread.append((number, f"`{key}` declares targets outside a table"))
            continue
        if current is None or key not in _TARGET_KEYS:
            continue
        read = _toml_value(value)
        if read is None:
            if unread is not None:
                unread.append(
                    (number, f"`{key}` holds no string or boolean on its line")
                )
            continue
        current[key] = read
    name = package.get("name")
    package_name = name if isinstance(name, str) else ""
    # Each target, by its kind and name: its root, and whether `cargo test`
    # runs its tests. A table that names a target that Cargo discovers
    # configures that target.
    targets: dict[tuple[str, str], tuple[str, bool]] = {}
    if package.get(_DISCOVERY["lib"]) is not False and "src/lib.rs" in files:
        targets["lib", ""] = ("src/lib.rs", True)
    for kind, directory in _TARGET_DIRECTORIES.items():
        if package.get(_DISCOVERY[kind]) is False:
            continue
        tested = kind in ("bin", "test")
        for path in sorted(files):
            parent, file = posixpath.split(path)
            if parent == directory and file.endswith(".rs"):
                targets[kind, file[: -len(".rs")]] = (path, tested)
            elif posixpath.dirname(parent) == directory and file == "main.rs":
                targets.setdefault((kind, posixpath.basename(parent)), (path, tested))
        if kind == "bin" and package_name and "src/main.rs" in files:
            targets["bin", package_name] = ("src/main.rs", True)
    for kind, table in tables:
        target = table.get("name")
        target_name = target if isinstance(target, str) else ""
        path = table.get("path")
        if isinstance(path, str):
            root = posixpath.normpath(path)
        elif kind == "lib":
            root = "src/lib.rs"
        elif target_name:
            directory = _TARGET_DIRECTORIES[kind]
            inferred = [f"{directory}/{target_name}.rs"]
            inferred.append(f"{directory}/{target_name}/main.rs")
            if kind == "bin" and target_name == package_name:
                inferred.insert(0, "src/main.rs")
            root = next((found for found in inferred if found in files), inferred[0])
        else:
            continue
        test = table.get("test")
        tested = test if isinstance(test, bool) else kind in ("lib", "bin", "test")
        targets[kind, "" if kind == "lib" else target_name] = (root, tested)
    roots: dict[str, bool] = {}
    for root, tested in targets.values():
        roots[root] = roots.get(root, False) or tested
    return roots


def crate_of(path: str, crates: Sequence[str]) -> str | None:
    """Returns the innermost crate that holds path."""
    found: str | None = None
    for crate in crates:
        if (not crate or path.startswith(crate + "/")) and (
            found is None or len(crate) > len(found)
        ):
            found = crate
    return found


def crate_relative(path: str, crate: str) -> str:
    """Returns a path of a crate relative to the crate's directory."""
    return path[len(crate) + 1 :] if crate else path


def rust_role(path: str, crate: str) -> str:
    """Returns "production" for a production file of the crate, "test" for a
    test file, and "" for any other file."""
    if not path.endswith(".rs"):
        return ""
    relative = crate_relative(path, crate)
    if relative.startswith("tests/"):
        return "test"
    if not relative.startswith("src/"):
        return ""
    parts = relative.split("/")[1:]
    if parts[-1] == "tests.rs" or "tests" in parts[:-1]:
        return "test"
    return "production"


def _join(*parts: str) -> str:
    return "/".join(part for part in parts if part)


def is_crate_root(source: str) -> bool:
    """Whether a file, relative to its crate's src/, roots a crate: the
    library, the main binary, or another binary."""
    directory, name = posixpath.split(source)
    return (
        (not directory and name in ("lib.rs", "main.rs"))
        or directory == "bin"
        or (posixpath.dirname(directory) == "bin" and name == "main.rs")
    )


def is_target_root(source: str, base: str = "src") -> bool:
    """Whether a file, relative to its crate's base directory, src or tests,
    roots one of the crate's targets: in src/, as is_crate_root says, and in
    tests/, an integration test, `NAME.rs` or `NAME/main.rs`."""
    if base != "tests":
        return is_crate_root(source)
    directory, name = posixpath.split(source)
    return not directory or ("/" not in directory and name == "main.rs")


def module_directory(source: str, base: str = "src", root: bool | None = None) -> str:
    """Returns the directory, relative to the crate's base directory, of the
    modules that a file of that directory declares: the file's own for a
    target's root or a mod.rs file, and the one named after the file for any
    other. root says whether the file roots a target, which by default the
    places where Cargo discovers targets say."""
    directory, name = posixpath.split(source)
    if root is None:
        root = is_target_root(source, base)
    if name == "mod.rs" or root:
        return directory
    return _join(directory, name[: -len(".rs")])


def module_files(
    crate: str,
    source: str,
    inline: Sequence[str],
    name: str,
    base: str = "src",
    root: bool | None = None,
) -> tuple[str, str]:
    """Returns the two files that may hold the module that `mod NAME;`
    declares in a file of the crate's base directory, src or tests, inside the
    inline modules inline; root says whether the file roots a target."""
    directory = _join(crate, base, module_directory(source, base, root), *inline)
    return _join(directory, f"{name}.rs"), _join(directory, name, "mod.rs")


def declared_files(
    crate: str,
    base: str,
    source: str,
    declaration: Declaration,
    root: bool | None = None,
) -> tuple[str, ...]:
    """Returns the files that may hold the module that a declaration of a file
    of the crate's base directory declares; root says whether the file roots a
    target. An integration test may set a module's file with #[path],
    relative to the declaring file's directory or, inside inline modules, to
    their directory."""
    if declaration.path is None or base != "tests":
        return module_files(
            crate, source, declaration.inline, declaration.name, base, root
        )
    if declaration.inline:
        directory = _join(crate, base, module_directory(source, base, root))
        directory = _join(directory, *declaration.inline)
    else:
        directory = _join(crate, base, posixpath.dirname(source))
    return (posixpath.normpath(_join(directory, declaration.path)),)


def module_path(source: str) -> tuple[str, ...]:
    """Returns the module path, from its crate's root, of a file of src/, such
    as ("host", "store") for host/store/mod.rs; a binary's root starts with
    "bin:NAME"."""
    parts = source[: -len(".rs")].split("/")
    if parts[0] == "bin":
        binary = parts[1]
        rest = parts[2:]
        if rest == ["main"]:
            rest = []
        return (f"bin:{binary}", *rest)
    if parts in (["lib"], ["main"]):
        return ()
    if parts[-1] == "mod":
        parts = parts[:-1]
    return tuple(parts)


# The test code of the production files.


@dataclass(frozen=True)
class Declaration:
    """A `mod NAME;` declaration, inside the inline modules inline, and whether
    a cfg that requires `test` guards it; the file that its #[path] sets, if
    it has one; and whether a cfg that no build of tests satisfies guards it
    or a module that holds it, so that tests never compile its module."""

    inline: tuple[str, ...]
    name: str
    line: int
    test_only: bool
    path: str | None = None
    excluded: bool = False


@dataclass
class RustAnalysis:
    """What a Rust file of a crate's src/ or tests/ declares, and whether its
    inner attributes keep tests from compiling it; for a production file, the
    lines of the test hooks that it holds; and its problems, such as the test
    code that a production file holds besides them."""

    path: str
    crate: str
    production: bool
    declarations: list[Declaration] = field(default_factory=list[Declaration])
    hooks: list[int] = field(default_factory=list[int])
    problems: list[Problem] = field(default_factory=list[Problem])
    excluded: bool = False


def _describe(item: Item) -> str:
    if item.kind == "impl":
        return f"`{item.name}` block"
    if item.kind in ("macro", "macro_rules"):
        return f"`{item.name}!`" if item.kind == "macro" else f"macro `{item.name}`"
    if item.kind == "use":
        return "`use`"
    if item.name:
        return f"`{item.kind} {item.name}`"
    return f"`{item.kind}` item"


def _path_value(tokens: Sequence[Token], attributes: Iterable[Attribute]) -> str | None:
    """Returns the file that a `#[path = "FILE"]` among attributes sets."""
    for attribute in attributes:
        if attribute.path == "path":
            for token in tokens[attribute.start : attribute.end]:
                if token.kind == LITERAL and token.text.startswith('"'):
                    return token.text[1:-1]
    return None


def analyze_rust(path: str, crate: str, text: str) -> RustAnalysis:
    """Finds the module declarations of a Rust file of a crate's src/ or
    tests/ and, if it is a production file, its test code."""
    production = rust_role(path, crate) == "production"
    analysis = RustAnalysis(path, crate, production)
    try:
        parsed = parse_rust(text)
    except RustSyntaxError as error:
        analysis.problems.append(
            Problem(path, error.line, "rust-syntax", f"cannot read the file: {error}")
        )
        return analysis
    tokens, partner = parsed.tokens, parsed.partner
    base, _, source = crate_relative(path, crate).partition("/")

    def tests_file(inline: Sequence[str]) -> str:
        return module_files(crate, source, inline, "tests", base)[0]

    def excludes(attributes: Iterable[Attribute]) -> bool:
        return any(
            attribute_excludes_tests(tokens, partner, attribute)
            for attribute in attributes
        )

    exempt: set[int] = set()
    test_code_ranges: list[tuple[int, int]] = []

    def test_code(line: int, rule: str, message: str) -> None:
        analysis.problems.append(Problem(path, line, rule, message))

    def walk(
        items: Sequence[Item], inline: tuple[str, ...], in_tests: bool, excluded: bool
    ) -> None:
        """Walks items outside function bodies. In test code, which a test file
        or a module that only tests compile holds, it only collects the module
        declarations. excluded says whether the inline modules that hold items
        compile in no build of tests."""
        for item in items:
            only_tests = any(
                attribute_requires_test(tokens, partner, attribute)
                for attribute in item.attributes
            )
            gone = excluded or excludes(item.attributes)
            for attribute in item.attributes:
                if base == "src" and "path" in attribute_paths(
                    tokens, partner, attribute
                ):
                    analysis.problems.append(
                        Problem(
                            path,
                            attribute.line,
                            "path-attribute",
                            "#[path] sets the file of a module, which the test "
                            "layout does not follow; name the file after its "
                            "module",
                        )
                    )
            if item.kind == "mod" and item.body is None:
                analysis.declarations.append(
                    Declaration(
                        inline,
                        item.name,
                        item.line,
                        only_tests or in_tests,
                        _path_value(tokens, item.attributes),
                        gone,
                    )
                )
                if in_tests:
                    continue
                if only_tests and item.name == "tests":
                    exempt.update(attribute.start for attribute in item.attributes)
                elif only_tests:
                    test_code(
                        item.line,
                        "test-module-name",
                        f"module `{item.name}` compiles only for tests, but only "
                        "the `tests` module may; move it into the `tests` module, "
                        "as "
                        f"{module_files(crate, source, (*inline, 'tests'), item.name, base)[0]}",
                    )
                elif item.name == "tests":
                    analysis.problems.append(
                        Problem(
                            path,
                            item.line,
                            "tests-module",
                            "module `tests` compiles outside tests; guard it with "
                            "#[cfg(test)]",
                        )
                    )
                continue
            inner = gone or excludes(item.inner_attributes)
            if in_tests:
                if item.kind == "mod":
                    walk(item.children, (*inline, item.name), True, inner)
                continue
            if item.kind == "mod":
                only_tests = only_tests or any(
                    attribute_requires_test(tokens, partner, attribute)
                    for attribute in item.inner_attributes
                )
                if only_tests:
                    walk(item.children, (*inline, item.name), True, inner)
                    test_code_ranges.append((item.start, item.end))
                    if item.name == "tests":
                        message = (
                            f"inline module `tests`; move its contents to "
                            f"{tests_file(inline)} and declare it as "
                            "`#[cfg(test)] mod tests;`"
                        )
                    else:
                        message = (
                            f"inline module `{item.name}` that compiles only for "
                            "tests; move it into the `tests` module, as "
                            f"{module_files(crate, source, (*inline, 'tests'), item.name, base)[0]}"
                        )
                    test_code(item.line, "inline-test-module", message)
                    continue
                if item.name == "tests":
                    analysis.problems.append(
                        Problem(
                            path,
                            item.line,
                            "tests-module",
                            "inline module `tests` compiles outside tests; guard "
                            f"it with #[cfg(test)] and move it to {tests_file(inline)}",
                        )
                    )
                walk(item.children, (*inline, item.name), False, inner)
                continue
            if only_tests:
                test_code_ranges.append((item.start, item.end))
                test_code(
                    item.line,
                    "test-only-item",
                    f"{_describe(item)} compiles only for tests; move it into the "
                    f"`tests` module, {tests_file(inline)}",
                )
                continue
            if item.kind == "macro" and item.name.split("::")[-1] == "proptest":
                test_code_ranges.append((item.start, item.end))
                test_code(
                    item.line,
                    "test-harness",
                    "proptest! block; tests belong in the `tests` module, "
                    f"{tests_file(inline)}",
                )
                continue
            walk(item.children, inline, False, gone)

    analysis.excluded = excludes(parsed.inner_attributes)
    walk(parsed.items, (), not production, False)
    for attribute in parsed.inner_attributes:
        if base == "src" and "path" in attribute_paths(tokens, partner, attribute):
            analysis.problems.append(
                Problem(
                    path,
                    attribute.line,
                    "path-attribute",
                    "#![path] sets the file of a module, which the test layout "
                    "does not follow",
                )
            )
        if production and attribute_requires_test(tokens, partner, attribute):
            test_code(
                attribute.line,
                "test-only-file",
                "#![cfg(test)] makes this production file compile only for tests; "
                "move its code into a `tests` module",
            )
    if not production:
        return analysis

    def in_test_code(index: int) -> bool:
        return any(start <= index < end for start, end in test_code_ranges)

    for attribute in parsed.attributes:
        if attribute.start not in exempt and attribute_mentions_test(
            tokens, partner, attribute
        ):
            analysis.hooks.append(attribute.line)
        # cfg_attr may apply a test attribute, as #[cfg_attr(test, test)] does.
        paths = attribute_paths(tokens, partner, attribute)
        harness = [path for path in paths if is_test_attribute(path)]
        if harness and not in_test_code(attribute.start):
            test_code(
                attribute.line,
                "test-harness",
                f"#[{harness[0]}] in a production file; tests belong in the "
                f"`tests` module",
            )
    for index, cfg in parsed.cfg_macros:
        if mentions_test(cfg):
            analysis.hooks.append(tokens[index].line)
    analysis.hooks.sort()
    return analysis


# The allowlist.


@dataclass
class Allowlist:
    """The test hooks that production files may hold: the seams, each with the
    number of its file's hooks."""

    seams: dict[str, int] = field(default_factory=dict[str, int])


def parse_allowlist(text: str) -> tuple[Allowlist, list[str]]:
    """Parses the allowlist; returns it and what is wrong with its form."""
    errors: list[str] = []
    allowlist = Allowlist()
    try:
        data: object = json.loads(text)
    except json.JSONDecodeError as error:
        return allowlist, [f"is not JSON: {error}"]
    if not isinstance(data, dict):
        return allowlist, ["is not a JSON object"]
    document = cast(dict[str, object], data)
    unknown = sorted(set(document) - {"schema", "schema_version", "seams"})
    if unknown:
        errors.append(f"has unknown keys {', '.join(unknown)}")
    if document.get("schema") != ALLOWLIST_SCHEMA:
        errors.append(f'schema is not "{ALLOWLIST_SCHEMA}"')
    if document.get("schema_version") != ALLOWLIST_SCHEMA_VERSION:
        errors.append(f"schema_version is not {ALLOWLIST_SCHEMA_VERSION}")
    seams = document.get("seams", {})
    if not isinstance(seams, dict):
        errors.append("seams is not an object")
    else:
        for path, count in cast(dict[str, object], seams).items():
            if _is_count(count) and cast(int, count) > 0:
                allowlist.seams[path] = cast(int, count)
            else:
                errors.append(f"seams entry {path} is not a positive count")
    if not errors and text != render_allowlist(allowlist):
        errors.append(
            "is not in its canonical form, with sorted entries, two-space "
            f"indentation, and a final newline; run `{TIGHTEN_COMMAND}`"
        )
    return allowlist, errors


def _is_count(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def render_allowlist(allowlist: Allowlist) -> str:
    """Returns the canonical text of the allowlist."""
    document = {
        "schema": ALLOWLIST_SCHEMA,
        "schema_version": ALLOWLIST_SCHEMA_VERSION,
        "seams": dict(sorted(allowlist.seams.items())),
    }
    return json.dumps(document, indent=2) + "\n"


def tighten(allowlist: Allowlist, analyses: Mapping[str, RustAnalysis]) -> Allowlist:
    """Lowers the seams to the test hooks that the production files hold. An
    entry for a file that is no production file, or holds no hook, goes. No
    entry rises, and no file that the seams do not list joins them."""
    tightened = Allowlist()
    for path, count in allowlist.seams.items():
        analysis = analyses.get(path)
        if analysis is None or not analysis.production:
            continue
        found = len(analysis.hooks)
        if found:
            tightened.seams[path] = min(found, count)
    return tightened


# The checks.


def _lines(lines: Sequence[int]) -> str:
    shown = ", ".join(str(line) for line in lines[:12])
    return shown + (", ..." if len(lines) > 12 else "")


def _rust_count_problems(analysis: RustAnalysis, allowlist: Allowlist) -> list[Problem]:
    path = analysis.path
    hooks = len(analysis.hooks)
    allowed = allowlist.seams.get(path, 0)
    if hooks > allowed:
        return [
            Problem(
                path,
                0,
                "test-hooks",
                f"holds {hooks} test hook(s), at lines {_lines(analysis.hooks)}, but "
                f"the allowlist allows {allowed}: move test code into the `tests` "
                "module, and allow only a hook that cannot move in the seams of "
                f"{ALLOWLIST}",
            )
        ]
    if hooks < allowed:
        return [
            Problem(
                path,
                0,
                "allowlist",
                f"holds {hooks} test hook(s), fewer than the {allowed} that the "
                f"seams allow; run `{TIGHTEN_COMMAND}`",
            )
        ]
    return []


def analyze_crates(snapshot: Snapshot) -> tuple[dict[str, RustAnalysis], list[Problem]]:
    """Analyzes every Rust file of the crates' src/ directories and of their
    integration tests but the data of TEST_DATA_DIRECTORIES; returns the
    analyses and the problems of the module tree: `tests` modules whose files
    are missing, and test files that no target of their crate whose tests
    `cargo test` runs, as target_roots finds them, compiles, since no module
    that such a target's root reaches declares them, through declarations
    that builds of tests compile."""
    files = snapshot.files
    crates = rust_crates(files)
    members: dict[str, set[str]] = {crate: set() for crate in crates}
    for path in files:
        crate = crate_of(path, crates)
        if crate is not None:
            members[crate].add(crate_relative(path, crate))
    targets: dict[str, dict[str, bool]] = {}
    problems: list[Problem] = []
    for crate in crates:
        manifest = _join(crate, "Cargo.toml")
        unread: list[tuple[int, str]] = []
        targets[crate] = target_roots(
            decode(snapshot.read(manifest)), members[crate], unread
        )
        problems.extend(
            Problem(manifest, line, "manifest", f"cannot read its targets: {message}")
            for line, message in unread
        )
    analyses: dict[str, RustAnalysis] = {}
    for path in sorted(files):
        crate = crate_of(path, crates)
        if crate is None or not path.endswith(".rs"):
            continue
        relative = crate_relative(path, crate)
        if relative.startswith("src/") or (
            relative.startswith("tests/")
            and not relative.startswith(TEST_DATA_DIRECTORIES)
        ):
            analyses[path] = analyze_rust(path, crate, decode(snapshot.read(path)))
    declares: dict[str, list[str]] = {}
    for path, analysis in analyses.items():
        relative = crate_relative(path, analysis.crate)
        base, _, source = relative.partition("/")
        # A target's root declares the modules of its directory only where
        # its own target compiles it, which for tests is where it runs them.
        root = targets[analysis.crate].get(relative, False)
        for declaration in analysis.declarations:
            candidates = declared_files(analysis.crate, base, source, declaration, root)
            present = [candidate for candidate in candidates if candidate in files]
            if not declaration.excluded:
                declares.setdefault(path, []).extend(present)
            if (
                analysis.production
                and declaration.test_only
                and declaration.name == "tests"
                and len(present) != 1
            ):
                problems.append(
                    Problem(
                        path,
                        declaration.line,
                        "tests-module",
                        f"`mod tests;` names {' and '.join(candidates)}, of which "
                        "exactly one must be a tracked file"
                        if present
                        else f"`mod tests;` names neither {candidates[0]} nor "
                        f"{candidates[1]}; stage the file of the module",
                    )
                )
    compiled = {
        path
        for path, analysis in analyses.items()
        if targets[analysis.crate].get(crate_relative(path, analysis.crate))
        and not analysis.excluded
    }
    pending = sorted(compiled)
    while pending:
        for declared in declares.get(pending.pop(), ()):
            target = analyses.get(declared)
            if declared not in compiled and not (target and target.excluded):
                compiled.add(declared)
                pending.append(declared)
    for path, analysis in analyses.items():
        if analysis.production or path in compiled:
            continue
        problems.append(
            Problem(
                path,
                0,
                "orphan-test-file",
                "no target of the crate whose tests run compiles this test file, "
                "so its tests never run; declare it with `mod NAME;` in the file "
                "of its parent module, which a target of its crate whose tests "
                "`cargo test` runs, its library, a binary, or an integration test, "
                "must reach, through declarations that builds of tests compile",
            )
        )
    return analyses, problems


def python_problems(
    path: str, text: str, files: frozenset[str] = frozenset()
) -> list[Problem]:
    """Checks where a tracked Python module lives and what it imports. files,
    the tracked files, name the test modules that an import may resolve to."""
    name = posixpath.basename(path)
    if name.startswith("test_"):
        if PYTHON_UNIT_TESTS.fullmatch(path) is None:
            return [
                Problem(
                    path,
                    0,
                    "python-test-location",
                    "Python unit tests live in scripts/, .github/specula/tests/, "
                    "or .github/skills/*/scripts/",
                )
            ]
        return []
    problems: list[Problem] = []
    if name.endswith("_tests.py") and PYTHON_END_TO_END_TESTS.fullmatch(path) is None:
        problems.append(
            Problem(
                path,
                0,
                "python-test-location",
                "the end-to-end scenario drivers, the `*_tests.py` modules, live "
                "in scripts/nvx_tools/",
            )
        )
    if path in PYTHON_TEST_SUPPORT:
        return problems
    try:
        tree = ast.parse(text, path)
    except (SyntaxError, ValueError, RecursionError, MemoryError) as error:
        line = getattr(error, "lineno", None)
        problems.append(
            Problem(
                path,
                line if isinstance(line, int) else 0,
                "python-syntax",
                f"cannot read the module: {error}",
            )
        )
        return problems
    support = {
        posixpath.basename(module)[: -len(".py")] for module in PYTHON_TEST_SUPPORT
    }
    tests = PYTHON_TEST_SUPPORT | {
        file
        for file in files
        if file.endswith(".py") and posixpath.basename(file).startswith("test_")
    }

    def test_module(parts: Sequence[str], directory: str | None) -> bool:
        """Whether a module's path, from directory or, if None, from any import
        root, names a tracked test module."""
        suffix = "/".join(parts) + ".py"
        if directory is not None:
            return _join(directory, suffix) in tests
        return any(test == suffix or test.endswith("/" + suffix) for test in tests)

    for node in ast.walk(tree):
        found: list[str] = []
        line = 0
        if isinstance(node, ast.Import):
            line = node.lineno
            for alias in node.names:
                parts = alias.name.split(".")
                if (
                    parts[0] in PYTHON_TEST_FRAMEWORKS
                    or any(part.startswith("test_") for part in parts)
                    or parts[-1] in support
                ):
                    found.append(alias.name)
        elif isinstance(node, ast.ImportFrom):
            line = node.lineno
            parts = node.module.split(".") if node.module else []
            directory: str | None = None
            if node.level:
                directory = posixpath.dirname(path)
                for _ in range(node.level - 1):
                    directory = posixpath.dirname(directory)
            source = "." * node.level + (node.module or "")
            if parts and (
                (not node.level and parts[0] in PYTHON_TEST_FRAMEWORKS)
                or any(part.startswith("test_") for part in parts)
                or parts[-1] in support
                or test_module(parts, directory)
            ):
                found.append(source)
            # A name may be a module of the package, such as `from scripts
            # import test_a`, or an item of the module, such as a function
            # named test_a, which no test module's path matches.
            found.extend(
                f"{source}.{alias.name}"
                for alias in node.names
                if alias.name != "*"
                and (
                    alias.name in support
                    or test_module([*parts, alias.name], directory)
                )
            )
        for module in found:
            problems.append(
                Problem(
                    path,
                    line,
                    "python-test-import",
                    f"imports {module}, which only tests may; move the code "
                    "that needs it into a test module",
                )
            )
    return problems


@dataclass
class Layout:
    """The checked layout: the analyses of the Rust files, the allowlist, and
    the problems."""

    analyses: dict[str, RustAnalysis]
    allowlist: Allowlist
    problems: list[Problem]


def check_snapshot(snapshot: Snapshot) -> Layout:
    """Checks the test layout of a tree."""
    analyses, problems = analyze_crates(snapshot)
    allowlist = Allowlist()
    if ALLOWLIST in snapshot.files:
        allowlist, errors = parse_allowlist(decode(snapshot.read(ALLOWLIST)))
        problems.extend(Problem(ALLOWLIST, 0, "allowlist", error) for error in errors)
    for path in sorted(allowlist.seams):
        analysis = analyses.get(path)
        if analysis is None or not analysis.production:
            problems.append(
                Problem(
                    ALLOWLIST,
                    0,
                    "allowlist",
                    f"lists {path}, which is no tracked production file of a "
                    f"crate's src/; run `{TIGHTEN_COMMAND}`",
                )
            )
    for analysis in analyses.values():
        problems.extend(analysis.problems)
        if analysis.production:
            problems.extend(_rust_count_problems(analysis, allowlist))
    for path in sorted(snapshot.files):
        if path.endswith(".py"):
            problems.extend(
                python_problems(path, decode(snapshot.read(path)), snapshot.files)
            )
    return Layout(analyses, allowlist, sorted(set(problems)))


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Check that tests live apart from the code that they test."
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=REPO_ROOT,
        help="Git work tree to check (default: this repository)",
    )
    parser.add_argument(
        "--tighten",
        action="store_true",
        help=f"lower {ALLOWLIST} to what the production files hold, then check",
    )
    args = parser.parse_args(argv)
    root: Path = args.root
    try:
        tree = WorkTree(root)
        layout = check_snapshot(tree)
        if args.tighten:
            # A file that does not parse counts no test hooks, and an allowlist
            # that does not parse lacks its broken entries, so tightening then
            # would drop what they allow.
            unread = [
                str(problem)
                for problem in layout.problems
                if problem.rule == "rust-syntax"
            ]
            if ALLOWLIST in tree.files:
                errors = parse_allowlist(decode(tree.read(ALLOWLIST)))[1]
                unread.extend(f"{ALLOWLIST}: {error}" for error in errors)
            if unread:
                for line in unread:
                    print(line)
                print(
                    f"error: {ALLOWLIST} is not tightened while the files above "
                    "do not parse",
                    file=sys.stderr,
                )
                return 1
            text = render_allowlist(tighten(layout.allowlist, layout.analyses))
            (root / ALLOWLIST).write_text(text, encoding="utf-8", newline="\n")
            layout = check_snapshot(WorkTree(root))
    except CheckError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    annotate = os.environ.get("GITHUB_ACTIONS") == "true"
    for problem in layout.problems:
        print(problem.annotation() if annotate else problem)
    if layout.problems:
        print(
            f"error: {len(layout.problems)} problem(s) with the test layout; "
            "see doc/contribute.md#test-layout",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

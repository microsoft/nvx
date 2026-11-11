#!/usr/bin/env python3
"""Flag the changes of a pull request to its repository's existing tests.

Compares the head of a pull request with its merge base, and reports every
existing test asset that the head modifies or removes, in six categories:

- test code: the items of the Rust test files and of the code that only tests
  compile in production files, such as an inline `tests` module, a test
  helper, or a test hook; the functions, classes, and other statements of the
  Python unit tests and of their PowerShell test support;
- test data: fixtures, conformance vectors, and other data files of the tests,
  and the scenario and adversarial case lists;
- end-to-end tests: the scenario drivers, the `*_tests.py` modules of
  scripts/nvx_tools/, the guest scripts of the microVM tests, and the guest
  probes;
- test doubles: the fake OpenVMM of `nvx::testing` and its binary;
- test policy: an allowlist entry of scripts/test-layout-allowlist.json that
  is new or allows more, and any change to the layout check, to this script,
  or to its workflow;
- CI test wiring: a removed or changed line of the actions and workflow that
  run the tests, and of the ci.yml jobs that run them, or an added line there
  that may skip them, such as an `if:`, a `continue-on-error`, or a
  conditional that wraps a test command; a test job
  that ci.yml no longer has, and a test action, workflow, command, module, or
  scenario that it no longer names; and a scenario that leaves a backend's
  shards. Comments count for none of these.

It compares items, not lines: a Rust item is the same item where it keeps its
module path, kind, name, and cfg, so an inline `tests` module that moves to
its own file keeps its items. Formatting, comments, lint attributes, imports'
order, and visibility do not count, nor does an item that moves unchanged
under the same cfg predicates, its own and those that it inherits from its
module and file, in each way that a target compiles it, such as a test helper
that moves into a `tests` module with its cfg(test) dropped. A new test flags
nothing, and a test whose module no
target of its crate compiles any more, as when its crate's manifest sets the
`test` key of its target to false or roots the target elsewhere, counts as
removed.

The script reads the commits' blobs and never runs their code, so a
workflow that pull_request_target triggers can run it, from the default
branch, on the commits of any pull request.
"""

from __future__ import annotations

import argparse
import ast
import copy
import difflib
import json
import os
import posixpath
import re
import subprocess
import sys
import urllib.parse
from collections import Counter, defaultdict
from collections.abc import Callable, Collection, Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import cast

import check_test_layout as layout
from check_test_layout import IDENT, LITERAL, PUNCT, Item, RustFile, Token

REPO_ROOT = layout.REPO_ROOT
DEFAULT_BASE = "origin/dev"
LABEL = "tests-modified"
LABEL_COLOR = "d93f0b"
LABEL_DESCRIPTION = "Changes existing tests, test data, or test wiring"
COMMENT_MARKER = "<!-- nvx-test-changes -->"
# GitHub refuses comments longer than 65536 characters.
COMMENT_LIMIT = 60000
ROWS_PER_SECTION = 50

TEST_CODE = "test code"
TEST_DATA = "test data"
END_TO_END = "end-to-end tests"
TEST_DOUBLES = "test doubles"
TEST_POLICY = "test policy"
CI_WIRING = "CI test wiring"
CATEGORIES = (TEST_CODE, TEST_DATA, END_TO_END, TEST_DOUBLES, TEST_POLICY, CI_WIRING)

# The actions that flag a pull request.
FLAGGING = frozenset({"modified", "removed", "loosened", "added a line that may skip"})

POLICY_FILES = frozenset(
    {
        "scripts/check_test_changes.py",
        "scripts/check_test_layout.py",
        ".github/workflows/test-changes.yml",
    }
)
# The actions and workflow that run tests and nothing else, whose every line is
# test wiring, and the workflow that names them, whose test actions,
# workflows, commands, modules, and scenarios are.
WIRING_FILES = frozenset(
    {
        ".github/actions/check-nvx-rs/action.yml",
        ".github/actions/check-quality/action.yml",
        ".github/actions/validate-nvx/action.yml",
        ".github/workflows/run-nvx-microvm-tests.yml",
    }
)
WIRING_TOKEN_FILES = frozenset({".github/workflows/ci.yml"})
SHARDS_FILE = "scripts/nvx_tools/microvm_shards.json"
DOUBLES = re.compile(r"nvx-rs/src/testing/.+\.rs|nvx-rs/src/bin/nvx-fake-openvmm\.rs")
END_TO_END_FILES = re.compile(
    r"scripts/nvx_tools/microvm_test_scripts/.+"
    r"|guest/common/nvx-(?:identity-probe|sandbox-smoke|virtio-restore-probe)"
)
TEST_DATA_FILES = re.compile(r"scripts/nvx_tools/adversarial_cases/.+")
# Lines that may keep tests from running when a test action or workflow adds
# them: a step's condition or its tolerance of failure, a command that ends the
# script early or skips tests, and a shell or PowerShell conditional or loop,
# which may wrap a test command without changing its line.
SKIPPING = re.compile(
    r"continue-on-error|(?:^|\s)if:|\|\|\s*(?:true|:)(?:\s|$)|\bexit\s+0\b"
    r"|--skip\b|--exclude\b"
    r"|(?:^\s*|[;&|:]\s*)(?i:if|elif|elseif|else|then|fi|case|esac|switch|while"
    r"|until|for|foreach|do|done|return)\b(?!:)"
)
# A Python test module that a test command names, such as scripts/test_a.py.
TEST_MODULE = re.compile(r"(?:\.[/\\])?(?:[\w.-]+[/\\])*test_\w+\.py")
WIRING_TOKENS = (
    re.compile(
        r"uses:\s*(\./\.github/(?:actions/(?:check-nvx-rs|check-quality|validate-nvx)"
        r"|workflows/run-nvx-microvm-tests\.yml))"
    ),
    re.compile(r"\bnvx\.py\s+((?:test|check)-[\w-]+)"),
    re.compile(r"--scenario\s+([\w.-]+)"),
    re.compile(r"scripts[/\\](test_\w+\.py)"),
    re.compile(r"(\bcargo\b[^\n]*\b(?:test|nextest)\b[^\n]*)"),
)
LINT_ATTRIBUTES = frozenset({"allow", "deny", "expect", "forbid", "warn"})


class FlagError(RuntimeError):
    """Raised when the commits cannot be compared or the pull request cannot
    be flagged."""


# What changed.


@dataclass(frozen=True)
class Location:
    path: str
    line: int


@dataclass(frozen=True)
class Change:
    """A change to a test asset: modified, removed, relocated, added, or one
    of the actions of the test policy and CI wiring."""

    category: str
    action: str
    kind: str
    what: str
    base: Location | None = None
    head: Location | None = None
    detail: str = ""

    @property
    def flags(self) -> bool:
        return self.action in FLAGGING

    def order(self) -> tuple[int, str, str, str, int, str, int, str]:
        """Sorts changes by category, action, kind, and place."""
        base = self.base or Location("", 0)
        head = self.head or Location("", 0)
        return (
            CATEGORIES.index(self.category),
            self.action,
            self.kind,
            base.path,
            base.line,
            head.path,
            head.line,
            self.what,
        )


# The ways that code compiles: for each target that compiles it, through each
# chain of declarations, the cfg predicates that must all hold. The
# conditions of code that compiles where nothing needs to hold, or nowhere,
# are ANYWHERE and empty.
Conditions = frozenset[frozenset[str]]
ANYWHERE: Conditions = frozenset({frozenset()})


def _reduced(ways: Iterable[frozenset[str]]) -> Conditions:
    """Returns ways without those that another, which needs fewer predicates
    to hold, makes moot."""
    unique = set(ways)
    return frozenset(way for way in unique if not any(other < way for other in unique))


def _within(conditions: Conditions, predicates: Iterable[str]) -> Conditions:
    """Returns the ways that code compiles where conditions say that the code
    that holds it does, and predicates hold too."""
    added = frozenset(predicates)
    return _reduced(way | added for way in conditions)


@dataclass(frozen=True)
class Entry:
    """A test asset that may change: an item, a pseudo-item such as a module's
    imports, or a file. Its key identifies it across commits; loose is its
    kind and name, and scope the module or file and container that hold it.
    content is what counts as a change, and moved what an unchanged copy
    elsewhere holds. The content of a monotone entry is a set of lines, which
    may grow without changing what it was. conditions are the ways that a
    Rust item compiles: for each, the cfg predicates that must hold, its own
    and those that its module and the items that hold it inherit, which a move
    must keep."""

    key: tuple[str, ...]
    loose: tuple[str, ...]
    scope: tuple[str, ...]
    name: str
    content: str
    moved: str
    category: str
    kind: str
    what: str
    location: Location
    monotone: bool = False
    conditions: Conditions = ANYWHERE


# How alike the content of a removed entry and of a new one in the same scope
# must be for the new one to count as the old one renamed.
RENAMED_SIMILARITY = 0.6
_WORDS = re.compile(r"\w+|[^\w\s]")


def compare(base: Sequence[Entry], head: Sequence[Entry]) -> list[Change]:
    """Compares the entries of two commits: an entry of both is modified when
    its content or its conditions changed; one of the base only is relocated
    when the head holds its content elsewhere under its conditions, modified
    when the head holds its content under others, or an entry of its kind and
    name or, in its scope, of content like its own, and removed otherwise;
    and one of the head only is added."""
    before = {entry.key: entry for entry in base}
    after = {entry.key: entry for entry in head}
    changes: list[Change] = []
    for key in sorted(before.keys() & after.keys()):
        old, new = before[key], after[key]
        same_conditions = old.conditions == new.conditions
        if old.content == new.content and same_conditions:
            if old.location.path != new.location.path:
                changes.append(_change(old, new, "relocated"))
            continue
        if (
            old.monotone
            and same_conditions
            and set(old.content.split("\n")) <= set(new.content.split("\n"))
        ):
            changes.append(_change(old, new, "extended"))
            continue
        details = [] if old.content == new.content else [_detail(old, new)]
        if not same_conditions:
            details.append(_conditions_detail(old, new))
        changes.append(_change(old, new, "modified", _join(details)))
    removed = [before[key] for key in sorted(before.keys() - after.keys())]
    unmatched = {key: after[key] for key in sorted(after.keys() - before.keys())}
    by_moved: dict[str, list[Entry]] = defaultdict(list)
    by_loose: dict[tuple[str, ...], list[Entry]] = defaultdict(list)
    by_scope: dict[tuple[str, ...], list[Entry]] = defaultdict(list)
    for entry in unmatched.values():
        by_moved[entry.moved].append(entry)
        by_loose[entry.loose].append(entry)
        by_scope[entry.scope].append(entry)

    def take(candidates: Sequence[Entry], entry: Entry) -> Entry | None:
        """Takes the unmatched candidate most like entry: of its conditions,
        then of its kind and name."""
        left = [candidate for candidate in candidates if candidate.key in unmatched]
        if not left:
            return None
        match = min(
            left,
            key=lambda candidate: (
                candidate.conditions != entry.conditions,
                candidate.loose != entry.loose,
            ),
        )
        del unmatched[match.key]
        return match

    left: list[Entry] = []
    for entry in removed:
        match = take(by_moved.get(entry.moved, []), entry)
        if match is None:
            left.append(entry)
        elif match.conditions == entry.conditions:
            changes.append(_change(entry, match, "relocated"))
        else:
            detail = _join(["moved", _conditions_detail(entry, match)])
            changes.append(_change(entry, match, "modified", detail))
    for entry in left:
        match = take(by_loose.get(entry.loose, []), entry) if entry.name else None
        if match is not None:
            details = [_detail(entry, match, moved=True)]
            if match.conditions != entry.conditions:
                details.append(_conditions_detail(entry, match))
            changes.append(_change(entry, match, "modified", _join(details)))
            continue
        match = _renamed(entry, by_scope.get(entry.scope, []), unmatched)
        if match is not None:
            del unmatched[match.key]
            details = [f"renamed to `{match.name}`"]
            if match.conditions != entry.conditions:
                details.append(_conditions_detail(entry, match))
            changes.append(_change(entry, match, "modified", _join(details)))
            continue
        changes.append(_change(entry, None, "removed"))
    changes.extend(_change(None, entry, "added") for entry in unmatched.values())
    return changes


def _join(details: Sequence[str]) -> str:
    return ", and ".join(detail for detail in details if detail)


def _conditions_detail(old: Entry, new: Entry) -> str:
    """Says how the conditions under which an entry compiles changed."""

    def listed(conditions: Conditions) -> str:
        ways = sorted(
            " and ".join(f"`{predicate}`" for predicate in sorted(way))
            for way in conditions
        )
        if len(ways) == 1:
            return ways[0]
        return " or ".join(f"({way})" if " and " in way else way for way in ways)

    if new.conditions == ANYWHERE:
        return f"it compiles without {listed(old.conditions)}"
    if old.conditions == ANYWHERE:
        return f"it now compiles only where {listed(new.conditions)} hold"
    return (
        f"it now compiles where {listed(new.conditions)} hold, "
        f"not where {listed(old.conditions)} did"
    )


def _renamed(
    entry: Entry,
    candidates: Sequence[Entry],
    unmatched: Mapping[tuple[str, ...], Entry],
) -> Entry | None:
    """Returns the new entry in entry's scope whose content is most like its
    own, if alike enough."""
    if entry.kind not in ("test", "helper"):
        return None
    words = _WORDS.findall(entry.content)
    best: tuple[float, Entry] | None = None
    for candidate in candidates:
        if candidate.key not in unmatched or candidate.kind != entry.kind:
            continue
        matcher = difflib.SequenceMatcher(
            None, words, _WORDS.findall(candidate.content), autojunk=False
        )
        if matcher.quick_ratio() < RENAMED_SIMILARITY:
            continue
        ratio = matcher.ratio()
        if ratio >= RENAMED_SIMILARITY and (best is None or ratio > best[0]):
            best = (ratio, candidate)
    return None if best is None else best[1]


def _change(
    old: Entry | None, new: Entry | None, action: str, detail: str = ""
) -> Change:
    entry = old if old is not None else new
    assert entry is not None
    return Change(
        entry.category,
        action,
        entry.kind,
        entry.what,
        None if old is None else old.location,
        None if new is None else new.location,
        detail,
    )


_PATH_PREFIX = re.compile(r"(?:(?<![\w#])[\w#]+ :: )+")


def _detail(old: Entry, new: Entry, moved: bool = False) -> str:
    first, second = (old.moved, new.moved) if moved else (old.content, new.content)
    paths = old.key[0] == "rust" and _PATH_PREFIX.sub("", first) == _PATH_PREFIX.sub(
        "", second
    )
    if moved:
        return "moved, and only the paths that it names changed" if paths else "moved"
    return "only the paths that it names changed" if paths else ""


# Rust items.


def _tokens(tokens: Sequence[Token], start: int, end: int) -> str:
    return _joined(tokens, range(start, end))


# The pairs of punctuation that Rust reads as one operator when nothing
# separates them, which rustfmt keeps together: `a && b` and `a & &b` differ.
_OPERATOR_PAIRS = frozenset(
    {"!=", "%=", "&&", "&=", "*=", "+=", "-=", "->", "..", "/=", "::", "<<"}
    | {"<=", "==", "=>", ">=", ">>", "^=", "|=", "||"}
)


def _joined(tokens: Sequence[Token], indices: Iterable[int]) -> str:
    """Joins the texts of the tokens at indices with spaces, but for the
    punctuation that forms an operator with the punctuation right before it
    in the source, as the second `&` of `&&` does, which joins it."""
    words: list[str] = []
    previous: Token | None = None
    for index in indices:
        token = tokens[index]
        if (
            previous is not None
            and previous.kind == PUNCT
            and token.kind == PUNCT
            and previous.end == token.start
            and previous.text + token.text in _OPERATOR_PAIRS
        ):
            words[-1] += token.text
        else:
            words.append(token.text)
        previous = token
    return " ".join(words)


# Keywords after which a parenthesized group is an expression or a pattern,
# not the arguments of a call.
_EXPRESSION_KEYWORDS = frozenset(
    {"box", "break", "else", "if", "in", "let", "match", "move", "mut", "ref"}
    | {"return", "while", "yield"}
)


def _one_tuple(parsed: RustFile, comma: int) -> bool:
    """Whether the comma before a `)` makes its group a tuple of one element,
    whose comma rustfmt never drops."""
    tokens, partner = parsed.tokens, parsed.partner
    opener = partner[comma + 1]
    if tokens[opener].text != "(":
        return False
    pieces = layout.split_commas(tokens, partner, opener + 1, comma + 1)
    if len(pieces) != 1:
        return False
    if opener == 0:
        return True
    before = tokens[opener - 1]
    if before.kind == IDENT:
        return before.text in _EXPRESSION_KEYWORDS
    if (
        before.kind == PUNCT
        and before.text == ">"
        and layout.is_token(tokens, opener - 2, PUNCT, "-", "=")
        and tokens[opener - 2].end == before.start
    ):
        # After `->` or `=>`, a group is a type or an expression, not the
        # arguments of a call: `-> (u8,)` returns a tuple.
        return True
    return not (before.kind == PUNCT and before.text in "!>)]?")


def _statements(parsed: RustFile, opener: int) -> bool:
    """Whether the block that opener opens holds a statement: a `;` outside
    its inner brackets, or nothing at all."""
    tokens, partner = parsed.tokens, parsed.partner
    close = partner[opener]
    if close == opener + 1:
        return True
    index = opener + 1
    while index < close:
        token = tokens[index]
        if token.kind == PUNCT:
            if token.text == ";":
                return True
            if token.text in "([{":
                index = partner[index] + 1
                continue
        index += 1
    return False


def _match_arms(parsed: RustFile, keyword: int, end: int) -> int | None:
    """Returns the brace that opens the arms of the match at keyword."""
    tokens, partner = parsed.tokens, parsed.partner
    index = keyword + 1
    while index < end:
        token = tokens[index]
        if token.kind == PUNCT and token.text in "([":
            index = partner[index] + 1
            continue
        if token.kind == PUNCT and token.text == "{":
            # A block may be the scrutinee, as in `match { x } { ... }`.
            after = partner[index] + 1
            if index == keyword + 1 and layout.is_token(tokens, after, PUNCT, "{"):
                return after
            return index
        index += 1
    return None


# The macros of the standard library whose arguments are expressions, or a
# pattern, and that accept a trailing comma, in which commas count as they do
# outside macros.
_EXPRESSION_MACROS = frozenset(
    {"assert", "assert_eq", "assert_ne", "debug_assert", "debug_assert_eq"}
    | {"debug_assert_ne", "dbg", "eprint", "eprintln", "format", "format_args"}
    | {"matches", "panic", "print", "println", "todo", "unimplemented"}
    | {"unreachable", "vec", "write", "writeln"}
)


def _opaque_trees(parsed: RustFile, start: int, end: int) -> list[int]:
    """Returns the openers of the token trees from start to end that macros
    read as they choose: those of the invocations of the macros but those of
    _EXPRESSION_MACROS, and of the macro_rules! definitions."""
    tokens, partner = parsed.tokens, parsed.partner
    trees: list[int] = []
    for index in range(max(start, 1), end):
        before = tokens[index - 1]
        if (
            not layout.is_token(tokens, index, PUNCT, "!")
            or before.kind != IDENT
            or before.text in _EXPRESSION_KEYWORDS
            or before.text in _EXPRESSION_MACROS
        ):
            continue
        opener = index + 1
        if before.text == "macro_rules" and opener < end:
            opener += 1
        if (
            opener < end
            and layout.is_token(tokens, opener, PUNCT, "(", "[", "{")
            and partner[opener] < end
        ):
            trees.append(opener)
    return trees


def _formatting(parsed: RustFile, start: int, end: int) -> set[int]:
    """Returns the tokens from start to end whose presence rustfmt chooses by
    the width that the code has: the commas between the arms of a match, the
    braces of a match arm's or closure's body that holds one expression, and
    the comma before a closing bracket. In the token tree of
    a macro, which rustfmt leaves as it finds it but for its expressions'
    match arms and blocks, and whose commas may choose the macro's rule, only
    the commas between the arms of a match and the braces of their bodies and
    of closures' bodies count as formatting, unless the macro is one of the
    standard library's that take expressions and a trailing comma."""
    tokens, partner = parsed.tokens, parsed.partner
    opaque: set[int] = set()
    for opener in _opaque_trees(parsed, start, end):
        opaque.update(range(opener + 1, partner[opener]))
    arms_of_matches: set[int] = set()
    skipped: set[int] = set()
    for index in range(start, end):
        token = tokens[index]
        if token.kind == IDENT and token.text == "match":
            arms = _match_arms(parsed, index, end)
            if arms is None:
                continue
            arms_of_matches.add(arms)
            position = arms + 1
            while position < partner[arms]:
                if layout.is_token(tokens, position, PUNCT, "(", "[", "{"):
                    position = partner[position] + 1
                    continue
                if layout.is_token(tokens, position, PUNCT, ","):
                    skipped.add(position)
                position += 1
        elif token.kind == PUNCT and token.text == "{" and index > 0:
            arm = index >= 2 and (tokens[index - 2].text, tokens[index - 1].text) == (
                "=",
                ">",
            )
            if arm and index in opaque:
                # In a macro, `=>` may be the macro's own, as in its rules.
                arm = _innermost(parsed, start, index) in arms_of_matches
            closure = _closure_body(parsed, index, start)
            if (arm or closure) and not _statements(parsed, index):
                skipped.update((index, partner[index]))
        elif token.kind == PUNCT and token.text == "," and index not in opaque:
            closer = layout.is_token(tokens, index + 1, PUNCT, ")", "]", "}")
            one_tuple = layout.is_token(tokens, index + 1, PUNCT, ")") and _one_tuple(
                parsed, index
            )
            if closer and not one_tuple:
                skipped.add(index)
    return skipped


def _closure_body(parsed: RustFile, brace: int, start: int) -> bool:
    """Whether the brace at index brace opens the body of a closure, right
    after the `|` that ends its parameters, rather than a block that follows
    an operator `|`, as in `a | { b }`: the `|` that starts the parameters,
    after start, starts an operand."""
    tokens, partner = parsed.tokens, parsed.partner
    if not layout.is_token(tokens, brace - 1, PUNCT, "|"):
        return False
    index = brace - 2
    while index >= start:
        token = tokens[index]
        if token.kind == PUNCT and token.text in ")]}":
            index = partner[index] - 1
            continue
        if token.kind == PUNCT and token.text in "([{;":
            return False
        if token.kind == PUNCT and token.text == "|":
            return _operand(tokens, index, start)
        index -= 1
    return False


def _innermost(parsed: RustFile, start: int, index: int) -> int | None:
    """Returns the opener of the innermost bracket group from start that
    holds index."""
    tokens, partner = parsed.tokens, parsed.partner
    found: int | None = None
    position = start
    while position < index:
        if layout.is_token(tokens, position, PUNCT, "(", "[", "{"):
            if partner[position] > index:
                found = position
                position += 1
                continue
            position = partner[position] + 1
            continue
        position += 1
    return found


def _render(parsed: RustFile, start: int, end: int) -> str:
    """Renders tokens as they count for a change: without those whose presence
    rustfmt chooses by the width that the code has, so that code that moves to
    another indentation renders the same."""
    skipped = _formatting(parsed, start, end)
    return _joined(
        parsed.tokens, (index for index in range(start, end) if index not in skipped)
    )


_INCLUDE = re.compile(r'\b(include_str|include_bytes|include) ! \( "([^"\\]*)" \)')


def _resolve_includes(content: str, path: str) -> str:
    """Names the file of each include_str!, include_bytes!, or include! from
    the repository's root rather than from the file that includes it, so that
    the include stays the same when its file moves."""

    def resolve(match: re.Match[str]) -> str:
        relative = match.group(2)
        resolved = posixpath.normpath(posixpath.join(posixpath.dirname(path), relative))
        return f'{match.group(1)} ! ( "/{resolved}" )'

    return _INCLUDE.sub(resolve, content)


def _after_visibility(parsed: RustFile, index: int) -> int:
    if layout.is_token(parsed.tokens, index, IDENT, "pub"):
        index += 1
        if layout.is_token(parsed.tokens, index, PUNCT, "("):
            index = parsed.partner[index] + 1
    return index


def _render_cfg(cfg: layout.Cfg) -> str | None:
    if cfg.operator == "option":
        return cfg.name if cfg.value is None else f"{cfg.name} = {cfg.value}"
    if cfg.operator in ("all", "any", "not"):
        operands = [_render_cfg(operand) for operand in cfg.operands]
        if any(operand is None for operand in operands):
            return None
        return f"{cfg.operator} ( {' , '.join(cast(list[str], operands))} )"
    return None


def _cfg_text(cfg: layout.Cfg) -> str:
    """Renders a predicate, with the tokens of what does not parse."""
    if cfg.operator in ("all", "any", "not"):
        operands = " , ".join(_cfg_text(operand) for operand in cfg.operands)
        return f"{cfg.operator} ( {operands} )" if operands else f"{cfg.operator} ( )"
    return _render_cfg(cfg) or " ".join(cfg.words)


def _conjuncts(
    parsed: RustFile, attributes: Iterable[layout.Attribute]
) -> frozenset[str]:
    """Returns the predicates that the cfg attributes among attributes all
    require: the operands of an `all`, whose own operands an `all` among them
    extends, and any other predicate, each rendered; and for a cfg that
    cfg_attr applies where P holds, which nested cfg_attrs extend, `any(not(P),
    X)`, where X is what the cfg requires."""
    tokens, partner = parsed.tokens, parsed.partner
    found: set[str] = set()

    def add(cfg: layout.Cfg) -> None:
        if cfg.operator == "all":
            for operand in cfg.operands:
                add(operand)
            return
        found.add(_cfg_text(cfg))

    def conditional(start: int, end: int, held: tuple[layout.Cfg, ...]) -> None:
        """Adds what the cfg attributes that the cfg_attr from start to end
        applies require where held holds too."""
        pieces = layout.split_commas(tokens, partner, start, end)
        if not pieces:
            return
        held = (*held, layout.parse_cfg(tokens, partner, *pieces[0]))
        where = held[0] if len(held) == 1 else layout.Cfg("all", operands=held)
        for begin, finish in pieces[1:]:
            if finish - begin < 3 or not layout.is_token(tokens, begin + 1, PUNCT, "("):
                continue
            inner = (begin + 2, partner[begin + 1])
            if layout.is_token(tokens, begin, IDENT, "cfg"):
                required = _cfg_text(layout.parse_cfg(tokens, partner, *inner))
                found.add(f"any ( not ( {_cfg_text(where)} ) , {required} )")
            elif layout.is_token(tokens, begin, IDENT, "cfg_attr"):
                conditional(*inner, held)

    for attribute in attributes:
        if attribute.arguments is None:
            continue
        if attribute.path == "cfg":
            add(layout.parse_cfg(tokens, partner, *attribute.arguments))
        elif attribute.path == "cfg_attr":
            conditional(*attribute.arguments, ())
    return frozenset(found)


def _assume_test(cfg: layout.Cfg) -> layout.Cfg | None:
    """Returns what a predicate requires besides `test`, or None if nothing."""
    if cfg.operator == "option" and cfg.name == "test" and cfg.value is None:
        return None
    if cfg.operator == "all":
        rest = [
            residue
            for residue in (_assume_test(operand) for operand in cfg.operands)
            if residue is not None
        ]
        if not rest:
            return None
        return rest[0] if len(rest) == 1 else layout.Cfg("all", operands=tuple(rest))
    return cfg


def _attribute_text(parsed: RustFile, attribute: layout.Attribute, moved: bool) -> str:
    """Renders an attribute as it counts for a change: nothing for a lint
    attribute, and for moved content, a cfg without its `test`."""
    tokens, partner = parsed.tokens, parsed.partner
    paths = layout.attribute_paths(tokens, partner, attribute)
    if paths[0] in LINT_ATTRIBUTES or (
        paths[0] == "cfg_attr" and all(path in LINT_ATTRIBUTES for path in paths[1:])
    ):
        return ""
    if moved and attribute.path == "cfg" and attribute.arguments is not None:
        cfg = layout.parse_cfg(tokens, partner, *attribute.arguments)
        if layout.requires_test(cfg):
            residue = _assume_test(cfg)
            if residue is None:
                return ""
            rendered = _render_cfg(residue)
            if rendered is not None:
                return f"# [ cfg ( {rendered} ) ]"
    return _tokens(tokens, attribute.start, attribute.end + 1)


def _item_text(
    parsed: RustFile, item: Item, moved: bool, stop: int | None = None
) -> str:
    parts = [_attribute_text(parsed, attribute, moved) for attribute in item.attributes]
    start = _after_visibility(parsed, item.head)
    parts.append(_render(parsed, start, item.end if stop is None else stop))
    return " ".join(part for part in parts if part)


def _inner_text(parsed: RustFile, attributes: Iterable[layout.Attribute]) -> str:
    return " ".join(
        text
        for text in (
            _attribute_text(parsed, attribute, False) for attribute in attributes
        )
        if text
    )


def _use_paths(parsed: RustFile, item: Item) -> list[str]:
    """Expands a `use` item into the paths that it imports."""
    tokens, partner = parsed.tokens, parsed.partner
    start = _after_visibility(parsed, item.head) + 1
    end = (
        item.end - 1 if layout.is_token(tokens, item.end - 1, PUNCT, ";") else item.end
    )

    def tree(first: int, last: int, prefix: Sequence[str]) -> list[str]:
        parts = list(prefix)
        index = first
        while index < last:
            token = tokens[index]
            if token.kind == PUNCT and token.text == "{":
                found: list[str] = []
                for begin, finish in layout.split_commas(
                    tokens, partner, index + 1, partner[index]
                ):
                    found.extend(tree(begin, finish, parts))
                return found
            if token.kind == PUNCT and token.text == "*":
                return ["::".join([*parts, "*"])]
            if token.kind == IDENT and token.text == "as" and index + 1 < last:
                return ["::".join(parts) + " as " + tokens[index + 1].text]
            if token.kind == IDENT:
                parts.append(layout.unraw(token.text))
            index += 1
        if parts and parts[-1] == "self":
            parts.pop()
        return ["::".join(parts)]

    return tree(start, end, ())


_BLOCK_STARTS = frozenset(
    {"async", "const", "enum", "fn", "for", "if", "impl", "loop", "match", "mod"}
    | {"static", "struct", "trait", "type", "unsafe", "use", "while"}
)


def _context(parsed: RustFile, item: Item, limit: int) -> str:
    """Returns what the bracket group of item that closes at limit holds:
    "statements", those of a block; "types", the fields or variants of a type
    or the parameters of a function; or "expressions", such as match arms or
    the fields of a struct expression."""
    tokens, partner = parsed.tokens, parsed.partner
    if limit >= item.end or not layout.is_token(tokens, limit, PUNCT, ")", "]", "}"):
        return "expressions"
    opener = partner[limit]
    if tokens[opener].text == "{" and _statements(parsed, opener):
        return "statements"
    if item.kind in ("struct", "union", "enum"):
        return "types"
    if item.kind == "fn" and (item.body is None or opener < item.body[0]):
        return "types"
    return "expressions"


# The keywords after which an operand starts.
_OPERAND_KEYWORDS = frozenset({"break", "in", "move", "return", "yield"})


def _operand(tokens: Sequence[Token], index: int, first: int) -> bool:
    """Whether the token at index starts an operand, as the `|` of a closure
    or the `<` of a qualified path do, rather than following one, as a binary
    operator does."""
    if index == first:
        return True
    before = tokens[index - 1]
    if before.kind == IDENT:
        return before.text in _OPERAND_KEYWORDS
    if before.kind != PUNCT:
        return False
    if before.text == ">":
        return layout.is_token(tokens, index - 2, PUNCT, "=")
    return before.text in "([{,=:;"


# The constructs whose condition, scrutinee, or iterator precedes their body,
# and the keywords after which a `{` opens a block operand, not a body.
_CONDITIONS = frozenset({"for", "if", "match", "while"})
_CONDITION_KEYWORDS = frozenset(
    {"break", "if", "in", "let", "match", "return", "while", "yield"}
)


def _ends_operand(tokens: Sequence[Token], index: int) -> bool:
    """Whether the token before index ends an operand, as an identifier, a
    literal, or a closing bracket does, so that the `{` at index ends a
    condition and opens the body after it."""
    if index == 0:
        return False
    before = tokens[index - 1]
    if before.kind == LITERAL:
        return True
    if before.kind == IDENT:
        return before.text not in _CONDITION_KEYWORDS
    return before.kind == PUNCT and before.text in ")]}?"


def _lone_equals(tokens: Sequence[Token], index: int) -> bool:
    """Whether the `=` at index stands alone, as that of a `let` does, rather
    than in an operator such as `==`, `<=`, `=>`, or `..=`."""
    equals = tokens[index]
    before = tokens[index - 1] if index > 0 else None
    after = tokens[index + 1] if index + 1 < len(tokens) else None
    joined_before = (
        before is not None
        and before.kind == PUNCT
        and before.end == equals.start
        and before.text in "=!<>.+-*/%^&|"
    )
    joined_after = (
        after is not None
        and after.kind == PUNCT
        and equals.end == after.start
        and after.text in "=>"
    )
    return not (joined_before or joined_after)


def _construct_end(
    parsed: RustFile, start: int, limit: int, context: str = "expressions"
) -> int:
    """Returns the index after the field, variant, statement, match arm, or
    expression that starts at start, after its attributes, within limit, in a
    group of the context that _context names. A `;` ends it, and so does a
    `,`, but for one in a block of statements, which only generic arguments
    and closures' parameters hold, and one in the generic arguments of a type,
    a turbofish, or a qualified path, or in a closure's parameters."""
    tokens, partner = parsed.tokens, parsed.partner
    index = start
    while index < limit:
        attribute = layout.attribute_at(tokens, partner, index)
        if attribute is None:
            break
        index = attribute.end + 1
    first = index
    first_text = tokens[first].text if first < limit else ""
    conditional = first_text in _CONDITIONS

    def scan(generics: bool) -> tuple[int, bool]:
        """Returns the end, and whether a `<` that it took to open generic
        arguments stayed open."""
        index = first
        angles = 0
        # Whether a pattern runs at index, from `let` to its `=` or from `for`
        # to `in`, whose braces open no body.
        pattern = first_text == "for"
        while index < limit:
            token = tokens[index]
            if token.kind == IDENT and conditional:
                if token.text == "let":
                    pattern = True
                elif token.text == "in" and first_text == "for":
                    pattern = False
            if token.kind != PUNCT:
                index += 1
                continue
            text = token.text
            if pattern and text == "=" and _lone_equals(tokens, index):
                pattern = False
            if text == ";" or (text == "," and not angles and context != "statements"):
                return index + 1, False
            if text in ")]}":
                return index, angles > 0
            if text in "([":
                index = partner[index] + 1
                continue
            if text == "{":
                close = partner[index]
                after = close + 1
                if angles or (
                    conditional and (pattern or not _ends_operand(tokens, index))
                ):
                    # The braces of a const argument, as in `S<{ 1 }>`, of a
                    # pattern, or of a block in a condition, as in
                    # `if let Some(value) = { take() } { ... }`.
                    index = after
                    continue
                if layout.is_token(tokens, after, IDENT, "else"):
                    index = after + 1
                    continue
                arm = (
                    index >= 2
                    and tokens[index - 1].text == ">"
                    and tokens[index - 2].text == "="
                )
                invocation = index >= 1 and tokens[index - 1].text == "!"
                if (
                    arm
                    or invocation
                    or first_text in _BLOCK_STARTS
                    or first_text == "{"
                ):
                    if layout.is_token(tokens, after, PUNCT, ";", ","):
                        return after + 1, False
                    return after, False
                index = after
                continue
            if (
                generics
                and text == "<"
                and (context == "types" or angles or _operand(tokens, index, first))
            ):
                angles += 1
            elif (
                generics
                and text == ">"
                and angles
                and not layout.is_token(tokens, index - 1, PUNCT, "-", "=")
            ):
                angles -= 1
            elif (
                text == "|"
                and context == "expressions"
                and not angles
                and _operand(tokens, index, first)
            ):
                # A closure's parameters, which end with the next `|`.
                bar = index + 1
                while bar < limit and not layout.is_token(tokens, bar, PUNCT, "|"):
                    opens = layout.is_token(tokens, bar, PUNCT, "(", "[", "{")
                    bar = partner[bar] + 1 if opens else bar + 1
                if bar < limit:
                    index = bar + 1
                    continue
            index += 1
        return limit, angles > 0

    end, open_angles = scan(context != "statements")
    if open_angles:
        # A `<` that no `>` closes is no generic argument list.
        end = scan(False)[0]
    return end


class _RustEntries:
    """Collects the test entries of a Rust file, whose module compiles in the
    ways that inherited says."""

    def __init__(
        self,
        parsed: RustFile,
        crate: str,
        path: str,
        category: str,
        inherited: Conditions = ANYWHERE,
    ) -> None:
        self.parsed = parsed
        self.crate = crate
        self.path = path
        self.category = category
        self.ways = inherited
        # The predicates that the file's inner attributes add to every way.
        self.inherited = _conjuncts(parsed, parsed.inner_attributes)
        self.entries: list[Entry] = []
        # The imports of each module, as they count for a change and as they
        # count for a move, with the line of the first and the module's
        # conditions.
        self.imports: dict[
            tuple[str, ...], tuple[set[str], set[str], int, frozenset[str]]
        ] = {}
        self.seen: dict[tuple[str, ...], int] = defaultdict(int)

    def add(
        self,
        module: tuple[str, ...],
        container: str,
        kind: str,
        name: str,
        cfg: str,
        content: str,
        moved: str,
        test: bool,
        line: int,
        what: str,
        conditions: frozenset[str],
    ) -> None:
        base = ("rust", self.crate, "::".join(module), container, kind, name, cfg)
        self.seen[base] += 1
        key = (*base, str(self.seen[base]))
        self.entries.append(
            Entry(
                key,
                ("rust", kind, name),
                base[:5],
                name,
                content,
                moved,
                self.category,
                "test" if test else _KINDS.get(kind, "helper"),
                what,
                Location(self.path, line),
                kind == "use",
                _within(self.ways, conditions),
            )
        )

    def item(
        self,
        module: tuple[str, ...],
        container: str,
        item: Item,
        test: bool,
        conditions: frozenset[str],
        stop: int | None = None,
        extra: str = "",
    ) -> None:
        parsed = self.parsed
        cfg = " & ".join(
            sorted(
                _tokens(parsed.tokens, attribute.start, attribute.end + 1)
                for attribute in item.attributes
                if attribute.path == "cfg"
            )
        )
        content = _resolve_includes(_item_text(parsed, item, False, stop), self.path)
        moved = _resolve_includes(_item_text(parsed, item, True, stop), self.path)
        content += extra
        moved += extra
        name = item.name or _tokens(
            parsed.tokens, item.head, min(item.head + 6, item.end)
        )
        self.add(
            module,
            container,
            item.kind,
            name,
            cfg,
            content,
            moved,
            test,
            item.line,
            _what(module, container, item.kind, name),
            conditions | _conjuncts(parsed, item.attributes),
        )

    def inner(
        self,
        module: tuple[str, ...],
        attributes: Sequence[layout.Attribute],
        line: int,
        conditions: frozenset[str],
    ) -> None:
        """Adds the inner attributes of a module of test code, even none, so
        that one which a change adds, such as a cfg, modifies them."""
        text = _inner_text(self.parsed, attributes)
        self.add(
            module,
            "",
            "inner",
            "",
            "",
            text,
            text,
            False,
            line,
            f"the inner attributes of `{_module_name(module)}`",
            conditions,
        )

    def use(
        self, module: tuple[str, ...], item: Item, conditions: frozenset[str]
    ) -> None:
        """Adds the paths that a `use` item imports to its module's imports,
        each with the attributes of the item but its lint attributes, and, as
        they count for a move, without `test` in its cfg."""
        parsed = self.parsed
        content, moved, line, _ = self.imports.get(
            module, (set[str](), set[str](), item.line, conditions)
        )
        attributes, moved_attributes = (
            " ".join(
                text
                for text in (
                    _attribute_text(parsed, attribute, for_move)
                    for attribute in item.attributes
                )
                if text
            )
            for for_move in (False, True)
        )
        for path in _use_paths(parsed, item):
            content.add(f"{attributes} {path}".strip())
            moved.add(f"{moved_attributes} {path}".strip())
        self.imports[module] = (content, moved, line, conditions)

    def finish(self) -> list[Entry]:
        for module, (content, moved, line, conditions) in sorted(self.imports.items()):
            self.add(
                module,
                "",
                "use",
                "",
                "",
                "\n".join(sorted(content)),
                "\n".join(sorted(moved)),
                False,
                line,
                f"the imports of `{_module_name(module)}`",
                conditions,
            )
        return self.entries

    def walk(
        self,
        items: Sequence[Item],
        module: tuple[str, ...],
        container: str,
        in_test: bool,
        conditions: frozenset[str],
    ) -> None:
        parsed = self.parsed
        tokens, partner = parsed.tokens, parsed.partner
        for item in items:
            # A test function compiles only for tests, whatever guards it.
            gated = (
                in_test
                or any(
                    layout.attribute_requires_test(tokens, partner, attribute)
                    for attribute in item.attributes
                )
                or _test_function(parsed, item)
            )
            held = conditions | _conjuncts(parsed, item.attributes)
            if item.kind == "mod":
                inner = (*module, item.name)
                if item.body is not None:
                    # A module's inner attributes guard it as its outer ones do.
                    gated = gated or any(
                        layout.attribute_requires_test(tokens, partner, attribute)
                        for attribute in item.inner_attributes
                    )
                    held |= _conjuncts(parsed, item.inner_attributes)
                if gated:
                    keyword = _after_visibility(parsed, item.head)
                    self.item(module, container, item, False, conditions, keyword + 2)
                    if item.body is not None:
                        self.inner(inner, item.inner_attributes, item.line, held)
                        self.walk(item.children, inner, "", True, held)
                elif item.body is not None:
                    self.walk(item.children, inner, "", False, held)
                continue
            if not gated:
                self.hooks(module, container, item, conditions)
                if item.children:
                    self.walk(item.children, module, _container(item), False, held)
                continue
            if item.kind == "use":
                self.use(module, item, conditions)
                continue
            if item.body is not None and (
                item.kind in ("impl", "trait", "extern")
                or (item.kind == "macro" and item.children)
            ):
                inner_text = _inner_text(parsed, item.inner_attributes)
                self.item(
                    module,
                    container,
                    item,
                    False,
                    conditions,
                    item.body[0],
                    f" {inner_text}" if inner_text else "",
                )
                self.walk(item.children, module, _container(item), True, held)
                continue
            test = _test_function(parsed, item) or (
                item.kind == "fn" and container == "proptest!"
            )
            self.item(module, container, item, test, conditions)

    def hooks(
        self,
        module: tuple[str, ...],
        container: str,
        item: Item,
        conditions: frozenset[str],
    ) -> None:
        """Adds the test hooks of a production item, but those of the items
        that it holds."""
        parsed = self.parsed
        tokens, partner = parsed.tokens, parsed.partner
        held = [(child.start, child.end) for child in item.children]

        def outside(index: int) -> bool:
            return item.start <= index < item.end and not any(
                start <= index < end for start, end in held
            )

        pieces: list[tuple[int, str]] = []
        for attribute in parsed.attributes:
            if not outside(attribute.start) or not layout.attribute_mentions_test(
                tokens, partner, attribute
            ):
                continue
            end = attribute.end + 1
            if attribute.start >= item.head and layout.attribute_requires_test(
                tokens, partner, attribute
            ):
                limit = _limit(parsed, item, attribute.start)
                end = _construct_end(
                    parsed, attribute.start, limit, _context(parsed, item, limit)
                )
            pieces.append((attribute.start, _render(parsed, attribute.start, end)))
        for index, cfg in parsed.cfg_macros:
            if outside(index) and layout.mentions_test(cfg):
                pieces.append((index, _tokens(tokens, index, partner[index + 2] + 1)))
        if not pieces:
            return
        text = _resolve_includes(
            " | ".join(piece for _, piece in sorted(pieces)), self.path
        )
        name = f"{item.kind} {item.name}".strip()
        self.add(
            module,
            container,
            "hooks",
            name,
            "",
            text,
            text,
            False,
            item.line,
            f"the test hooks of `{name}` in `{_module_name(module)}`"
            + (f" (`{container}`)" if container else ""),
            conditions | _conjuncts(parsed, item.attributes),
        )


_KINDS = {"hooks": "hook", "inner": "attributes", "mod": "module", "use": "imports"}


def _test_function(parsed: RustFile, item: Item) -> bool:
    """Whether an attribute of item, or one that a cfg_attr of it applies,
    makes it a test, as #[test] does."""
    return any(
        layout.is_test_function_attribute(path)
        for attribute in item.attributes
        for path in layout.attribute_paths(parsed.tokens, parsed.partner, attribute)
    )


def _limit(parsed: RustFile, item: Item, index: int) -> int:
    """Returns the end of the innermost bracket group of item that holds
    index."""
    tokens, partner = parsed.tokens, parsed.partner
    limit = item.end
    position = item.start
    while position < limit:
        if (
            tokens[position].kind == PUNCT
            and tokens[position].text in "([{"
            and position < index < partner[position]
        ):
            limit = partner[position]
            position += 1
            continue
        if tokens[position].kind == PUNCT and tokens[position].text in "([{":
            position = partner[position] + 1
            continue
        position += 1
    return limit


def _container(item: Item) -> str:
    if item.kind == "impl":
        return item.name
    if item.kind == "trait":
        return f"trait {item.name}"
    if item.kind == "extern":
        return "extern"
    return f"{item.name}!"


def _module_name(module: tuple[str, ...]) -> str:
    if module and module[0] == "[tests]":
        return "/".join(module[1:]) + " (tests)"
    return "::".join(module) or "crate"


def _what(module: tuple[str, ...], container: str, kind: str, name: str) -> str:
    where = _module_name(module) + (f", `{container}`" if container else "")
    # An impl's name is its header, which starts with its kind.
    item = name if kind == "impl" else f"{kind} {name}"
    return f"`{item}` in `{where}`"


def rust_module(path: str, crate: str) -> tuple[str, ...]:
    """Returns the module path of a Rust file of a crate, from its root; a file
    of the crate's tests directory has one of its own."""
    relative = layout.crate_relative(path, crate)
    if relative.startswith("src/"):
        return layout.module_path(relative[len("src/") :])
    return ("[tests]", *relative[len("tests/") : -len(".rs")].split("/"))


def rust_entries(
    path: str,
    crate: str,
    text: str,
    category: str,
    whole: bool,
    inherited: Conditions = ANYWHERE,
) -> list[Entry]:
    """Returns the test entries of a Rust file, whose module compiles in the
    ways that inherited says: all of its items when whole, as for a test file
    or a file whose inner attributes require `test`, and otherwise its test
    code and test hooks. Raises RustSyntaxError when the file does not parse."""
    parsed = layout.parse_rust(text)
    module = rust_module(path, crate)
    collector = _RustEntries(parsed, crate, path, category, inherited)
    whole = whole or any(
        layout.attribute_requires_test(parsed.tokens, parsed.partner, attribute)
        for attribute in parsed.inner_attributes
    )
    if whole:
        collector.inner(module, parsed.inner_attributes, 1, collector.inherited)
    else:
        pieces = [
            _tokens(parsed.tokens, attribute.start, attribute.end + 1)
            for attribute in parsed.inner_attributes
            if layout.attribute_mentions_test(parsed.tokens, parsed.partner, attribute)
        ]
        if pieces:
            text = " | ".join(pieces)
            collector.add(
                module,
                "",
                "hooks",
                "",
                "",
                text,
                text,
                False,
                1,
                f"the test hooks of `{_module_name(module)}`",
                collector.inherited,
            )
    collector.walk(parsed.items, module, "", whole, collector.inherited)
    return collector.finish()


# Python items.


def _without_docstring(body: list[ast.stmt]) -> list[ast.stmt]:
    if (
        body
        and isinstance(body[0], ast.Expr)
        and isinstance(body[0].value, ast.Constant)
        and isinstance(body[0].value.value, str)
    ):
        return body[1:]
    return body


def python_entries(path: str, text: str, category: str) -> list[Entry]:
    """Returns the entries of a Python module: its functions, classes and
    their methods, assignments, imports, and other statements. Raises
    SyntaxError or ValueError when it does not parse."""
    tree = ast.parse(text, path)
    entries: list[Entry] = []
    seen: dict[tuple[str, ...], int] = defaultdict(int)
    imports: set[str] = set()
    others: list[str] = []
    first_other = 0

    def add(
        container: str, kind: str, name: str, content: str, line: int, label: str
    ) -> None:
        base = ("python", path, container, kind, name)
        seen[base] += 1
        where = f"{container}." if container else ""
        what = (
            f"`{kind} {where}{name}` in `{path}`" if name else f"the {kind} of `{path}`"
        )
        entries.append(
            Entry(
                (*base, str(seen[base])),
                ("python", kind, name),
                base[:4],
                name,
                content,
                content,
                category,
                label,
                what,
                Location(path, line),
                kind == "imports",
            )
        )

    def visit(node: ast.stmt, container: str) -> None:
        nonlocal first_other
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            function = copy.copy(node)
            function.body = _without_docstring(node.body)
            test = (
                node.name.startswith("test")
                if container
                else node.name.startswith("test_")
            )
            add(
                container,
                "def",
                node.name,
                ast.dump(function),
                node.lineno,
                "test" if test else "helper",
            )
        elif isinstance(node, ast.ClassDef):
            header = copy.copy(node)
            header.body = [
                statement
                for statement in _without_docstring(node.body)
                if not isinstance(
                    statement, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
                )
            ]
            add(container, "class", node.name, ast.dump(header), node.lineno, "helper")
            inner = f"{container}.{node.name}" if container else node.name
            for statement in node.body:
                if isinstance(
                    statement, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
                ):
                    visit(statement, inner)
        elif isinstance(node, ast.Import):
            imports.update(
                f"import {alias.name} as {alias.asname}" for alias in node.names
            )
        elif isinstance(node, ast.ImportFrom):
            source = "." * node.level + (node.module or "")
            imports.update(
                f"from {source} import {alias.name} as {alias.asname}"
                for alias in node.names
            )
        elif isinstance(node, (ast.Assign, ast.AnnAssign)) and not container:
            targets = node.targets if isinstance(node, ast.Assign) else [node.target]
            name = ", ".join(ast.unparse(target) for target in targets)
            add("", "assignment", name, ast.dump(node), node.lineno, "helper")
        else:
            if not others:
                first_other = node.lineno
            others.append(ast.dump(node))

    for statement in _without_docstring(tree.body):
        visit(statement, "")
    if imports:
        add("", "imports", "", "\n".join(sorted(imports)), 1, "imports")
    if others:
        add("", "statements", "", "\n".join(others), first_other, "statements")
    return entries


def file_entry(path: str, blob: str, category: str) -> Entry:
    """Returns a file as an entry, whose content is its blob."""
    directory, _, name = path.rpartition("/")
    return Entry(
        ("file", path),
        ("file", name),
        ("file", directory),
        name,
        blob,
        blob,
        category,
        "file",
        f"`{path}`",
        Location(path, 0),
    )


# Test policy and CI wiring.


@dataclass
class _Allowances:
    """What an allowlist allows the production files: the test hooks of the
    seams and, in the first schema version, the test hooks and tests of the
    baseline, which listed the files that predated the test layout."""

    seams: dict[str, int] = field(default_factory=dict[str, int])
    baseline: dict[str, tuple[int, int]] = field(
        default_factory=dict[str, tuple[int, int]]
    )


def _is_count(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def _first_version(text: str) -> _Allowances | None:
    """Reads an allowlist of the first schema version, or returns None if the
    text is no such allowlist."""
    try:
        data: object = json.loads(text)
    except json.JSONDecodeError:
        return None
    if not isinstance(data, dict):
        return None
    document = cast(dict[str, object], data)
    baseline = document.get("baseline", {})
    seams = document.get("seams", {})
    if (
        set(document) - {"schema", "schema_version", "baseline", "seams"}
        or document.get("schema") != layout.ALLOWLIST_SCHEMA
        or document.get("schema_version") != 1
        or not isinstance(baseline, dict)
        or not isinstance(seams, dict)
    ):
        return None
    allowances = _Allowances()
    for path, counts in cast(dict[str, object], baseline).items():
        if not isinstance(counts, dict):
            return None
        entry = cast(dict[str, object], counts)
        hooks, tests = entry.get("hooks"), entry.get("tests")
        if set(entry) != {"hooks", "tests"} or not (
            _is_count(hooks) and _is_count(tests)
        ):
            return None
        allowances.baseline[path] = (cast(int, hooks), cast(int, tests))
    for path, count in cast(dict[str, object], seams).items():
        if not _is_count(count):
            return None
        allowances.seams[path] = cast(int, count)
    return allowances


def _allowances(text: str) -> tuple[_Allowances, list[str]]:
    """Reads an allowlist of either schema version; returns what it allows and
    what is wrong with its form. Merge bases from before the baseline went, and
    branches that start from them, hold the first."""
    allowlist, errors = layout.parse_allowlist(text)
    if errors:
        first = _first_version(text)
        if first is not None:
            return first, []
    return _Allowances(dict(allowlist.seams)), errors


def allowlist_changes(base: str | None, head: str | None) -> list[Change]:
    """Returns the allowlist entries that are new or allow more. The first
    allowlist loosens nothing that a test policy held before."""
    if base is None or head is None:
        return []
    before = _allowances(base)[0]
    after, errors = _allowances(head)
    location = Location(layout.ALLOWLIST, 0)
    if errors:
        return [
            Change(
                TEST_POLICY,
                "modified",
                "file",
                f"`{layout.ALLOWLIST}`",
                location,
                location,
                "it does not parse: " + "; ".join(errors),
            )
        ]

    changes: list[Change] = []

    def loosened(path: str, detail: str) -> None:
        changes.append(
            Change(
                TEST_POLICY,
                "loosened",
                "allowlist entry",
                f"`{path}`",
                None,
                location,
                detail,
            )
        )

    # A baseline, which only the first schema version has, allows test code
    # besides hooks, and its two numbers bound different things, so each
    # counts on its own.
    for path, (hooks, tests) in sorted(after.baseline.items()):
        if path in before.baseline:
            old_hooks, old_tests = before.baseline[path]
            if hooks > old_hooks or tests > old_tests:
                loosened(
                    path,
                    f"the baseline allows {hooks} test hooks and {tests} tests, "
                    f"up from {old_hooks} and {old_tests}",
                )
        elif path in before.seams:
            loosened(path, "moves from the seams to the baseline, which allows tests")
        else:
            loosened(path, f"the baseline allows {hooks} test hooks and {tests} tests")
    for path, count in sorted(after.seams.items()):
        if path in before.seams:
            old = before.seams[path]
        elif path in before.baseline:
            old = before.baseline[path][0]
        else:
            loosened(path, f"the seams allow {count} test hooks")
            continue
        if count > old:
            loosened(path, f"the seams allow {count} test hooks, up from {old}")
    return changes


def _without_comment(line: str) -> str:
    """Returns a line of YAML, or of the shell or PowerShell script of a step,
    without its comment: what follows a `#` that starts the line or a word
    outside quotes."""
    quote = ""
    for index, character in enumerate(line):
        if quote:
            if character == quote:
                quote = ""
            continue
        if character in "'\"":
            quote = character
        elif character == "#" and (index == 0 or line[index - 1] in " \t"):
            return line[:index]
    return line


def _normalized(line: str) -> str:
    """Returns a line with its indentation, which nests a YAML key, and its
    other runs of whitespace as single spaces."""
    text = _without_comment(line).rstrip()
    words = " ".join(text.split())
    return " " * (len(text) - len(text.lstrip(" "))) + words if words else ""


def _normalized_lines(text: str) -> list[tuple[int, str]]:
    lines: list[tuple[int, str]] = []
    for number, line in enumerate(text.split("\n"), 1):
        normalized = _normalized(line)
        if normalized:
            lines.append((number, normalized))
    return lines


def _extends(old: str, new: str) -> bool:
    """Whether a line only gains test modules, as a test command that runs
    another one does, at the same indentation."""
    if len(old) - len(old.lstrip(" ")) != len(new) - len(new.lstrip(" ")):
        return False
    before, after = Counter(old.split()), Counter(new.split())
    if before - after:
        return False
    return all(TEST_MODULE.fullmatch(word) for word in (after - before).elements())


def line_changes(path: str, base: str | None, head: str | None) -> list[Change]:
    """Returns the removed and changed lines of a test action or workflow, but
    those that only gain test modules, and its added lines that may keep tests
    from running."""
    if base is None:
        return []
    if head is None:
        return [Change(CI_WIRING, "removed", "file", f"`{path}`", Location(path, 0))]
    return _line_diff(path, _normalized_lines(base), _normalized_lines(head))


def _line_diff(
    path: str, old: Sequence[tuple[int, str]], new: Sequence[tuple[int, str]]
) -> list[Change]:
    """Returns the removed and changed lines of old in new, but those that
    only gain test modules, and the added lines that may keep tests from
    running."""
    matcher = difflib.SequenceMatcher(
        None, [line for _, line in old], [line for _, line in new], autojunk=False
    )
    changes: list[Change] = []
    for tag, first, last, begin, end in matcher.get_opcodes():
        if (
            tag == "replace"
            and last - first == end - begin
            and all(
                _extends(before[1], after[1])
                for before, after in zip(old[first:last], new[begin:end], strict=True)
            )
        ):
            continue
        if tag in ("replace", "delete"):
            for number, line in old[first:last]:
                changes.append(
                    Change(
                        CI_WIRING,
                        "modified" if tag == "replace" else "removed",
                        "line",
                        f"`{_shorten(line.strip())}`",
                        Location(path, number),
                        Location(path, new[begin][0]) if begin < end else None,
                    )
                )
        if tag in ("replace", "insert"):
            for number, line in new[begin:end]:
                if SKIPPING.search(line):
                    changes.append(
                        Change(
                            CI_WIRING,
                            "added a line that may skip",
                            "line",
                            f"`{_shorten(line.strip())}`",
                            None,
                            Location(path, number),
                        )
                    )
    return changes


def _shorten(text: str, limit: int = 120) -> str:
    text = text.replace("`", "'")
    return text if len(text) <= limit else text[: limit - 3] + "..."


def wiring_tokens(text: str) -> dict[str, list[int]]:
    """Returns the test actions, workflows, commands, modules, and scenarios
    that a workflow names, with the line of each time that it names one, as
    ci.yml names validate-nvx once for each operating system. A line that ends
    with a backslash continues on the next."""
    logical: list[tuple[int, str]] = []
    pending: list[str] = []
    start = 1
    for number, line in enumerate(text.split("\n"), 1):
        line = _without_comment(line)
        if not pending:
            start = number
        if line.rstrip().endswith("\\"):
            pending.append(line.rstrip()[:-1])
            continue
        logical.append((start, " ".join([*pending, line])))
        pending = []
    if pending:
        logical.append((start, " ".join(pending)))
    tokens: dict[str, list[int]] = defaultdict(list)
    for number, line in logical:
        for kind, pattern in enumerate(WIRING_TOKENS):
            for match in pattern.finditer(line):
                value = re.sub(r'"\+\$\{\{[^}]*\}\}"', "", match.group(1))
                value = " ".join(value.replace("\\", "/").split())
                tokens[f"{kind}:{value}"].append(number)
    return dict(tokens)


def token_changes(path: str, base: str | None, head: str | None) -> list[Change]:
    """Returns the test actions, workflows, commands, modules, and scenarios
    that a workflow names fewer times, once for each time that it names one
    no more."""
    if base is None:
        return []
    old = wiring_tokens(base)
    new = wiring_tokens(head or "")
    changes: list[Change] = []
    for token, lines in sorted(old.items()):
        kept = len(new.get(token, []))
        changes.extend(
            Change(
                CI_WIRING,
                "removed",
                "test wiring",
                f"`{_shorten(token.split(':', 1)[1])}`",
                Location(path, line),
                detail=f"named {len(lines)} times before, {kept} now" if kept else "",
            )
            for line in lines[kept:]
        )
    return changes


@dataclass
class WorkflowJob:
    """A job of a workflow: the line of its ID, the lines of its keys but its
    steps, and the lines of each step, without comments, as _normalized_lines
    returns them."""

    line: int
    keys: list[tuple[int, str]] = field(default_factory=list[tuple[int, str]])
    steps: list[list[tuple[int, str]]] = field(
        default_factory=list[list[tuple[int, str]]]
    )

    def runs_tests(self) -> bool:
        return any(map(_names_tests, [self.keys, *self.steps]))

    def test_lines(self) -> list[tuple[int, str]]:
        """Returns the lines of its keys and of its steps that run tests."""
        lines = list(self.keys)
        for step in self.steps:
            if _names_tests(step):
                lines.extend(step)
        return lines


def _names_tests(lines: Sequence[tuple[int, str]]) -> bool:
    return bool(wiring_tokens("\n".join(text for _, text in lines)))


def workflow_jobs(text: str) -> dict[str, WorkflowJob]:
    """Returns the jobs of a workflow by their IDs."""
    jobs: dict[str, WorkflowJob] = {}
    job: WorkflowJob | None = None
    in_jobs = False
    job_indent: int | None = None
    key_indent: int | None = None
    step_indent: int | None = None
    in_steps = False
    for number, raw in enumerate(text.split("\n"), 1):
        line = _without_comment(raw).rstrip()
        stripped = line.strip()
        if not stripped:
            continue
        depth = len(line) - len(line.lstrip(" "))
        if depth == 0:
            in_jobs = line == "jobs:"
            job = None
            continue
        if not in_jobs:
            continue
        if job_indent is None:
            job_indent = depth
        if depth <= job_indent:
            job = jobs[stripped.removesuffix(":").strip("'\"")] = WorkflowJob(number)
            key_indent = step_indent = None
            in_steps = False
            continue
        if job is None:
            continue
        if key_indent is None:
            key_indent = depth
        normalized = (number, _normalized(raw))
        if in_steps:
            item = stripped.startswith("- ") or stripped == "-"
            if item and depth >= key_indent and depth == (step_indent or depth):
                step_indent = depth
                job.steps.append([normalized])
                continue
            if depth > (key_indent if step_indent is None else step_indent):
                if job.steps:
                    job.steps[-1].append(normalized)
                continue
            in_steps = False
        if depth == key_indent and stripped == "steps:":
            in_steps = True
            step_indent = None
            continue
        job.keys.append(normalized)
    return jobs


def job_changes(path: str, base: str | None, head: str | None) -> list[Change]:
    """Returns, for the jobs of a workflow that run tests, the removed and
    changed lines of their keys and of their steps that run tests, but those
    that only gain test modules, and the lines added there that may keep tests
    from running, such as an `if:`, a `continue-on-error`, or a conditional
    that wraps a command; and the jobs that run tests that the workflow no
    longer has."""
    if base is None:
        return []
    old, new = workflow_jobs(base), workflow_jobs(head or "")
    changes: list[Change] = []
    for name, job in sorted(old.items()):
        if not job.runs_tests():
            continue
        if name not in new:
            changes.append(
                Change(
                    CI_WIRING,
                    "removed",
                    "test job",
                    f"`{name}`",
                    Location(path, job.line),
                )
            )
            continue
        changes.extend(_line_diff(path, job.test_lines(), new[name].test_lines()))
    return changes


def shard_scenarios(text: str) -> set[tuple[str, str]] | None:
    """Returns the backends and scenarios of the microVM test shards."""
    try:
        data: object = json.loads(text)
    except json.JSONDecodeError:
        return None
    scenarios: set[tuple[str, str]] = set()
    if not isinstance(data, dict):
        return None
    backends = cast(dict[str, object], data).get("backends")
    if not isinstance(backends, dict):
        return None
    for backend, plan in cast(dict[str, object], backends).items():
        shards = (
            cast(dict[str, object], plan).get("shards")
            if isinstance(plan, dict)
            else None
        )
        if not isinstance(shards, list):
            return None
        for shard in cast(list[object], shards):
            listed = (
                cast(dict[str, object], shard).get("scenarios")
                if isinstance(shard, dict)
                else None
            )
            if not isinstance(listed, list):
                return None
            scenarios.update(
                (backend, str(scenario)) for scenario in cast(list[object], listed)
            )
    return scenarios


def shard_changes(base: str | None, head: str | None) -> list[Change]:
    """Returns the scenarios that leave a backend's shards."""
    if base is None:
        return []
    old = shard_scenarios(base)
    new = shard_scenarios(head or "{}") if head is not None else set[tuple[str, str]]()
    location = Location(SHARDS_FILE, 0)
    if old is None or new is None:
        return [
            Change(
                CI_WIRING,
                "modified",
                "file",
                f"`{SHARDS_FILE}`",
                location,
                location,
                "it does not parse",
            )
        ]
    return [
        Change(
            CI_WIRING,
            "removed",
            "scenario",
            f"`{scenario}` on {backend}",
            location,
        )
        for backend, scenario in sorted(old - new)
    ]


# Commits.


class Tree:
    """The blobs of a commit, by path."""

    def __init__(self, root: Path, commit: str) -> None:
        self.commit = commit
        self.blobs: dict[str, str] = {}
        listing = layout.git(root, "ls-tree", "-r", "-z", "--full-tree", commit)
        for entry in listing.split(b"\0"):
            if not entry:
                continue
            metadata, _, name = entry.partition(b"\t")
            fields = metadata.decode("ascii").split()
            if fields[1] == "blob":
                self.blobs[os.fsdecode(name)] = fields[2]


def read_blobs(root: Path, blobs: Iterable[str]) -> dict[str, bytes]:
    """Reads blobs by their object names, with one git process."""
    names = sorted(set(blobs))
    if not names:
        return {}
    output = layout.git(
        root, "cat-file", "--batch", stdin=("\n".join(names) + "\n").encode("ascii")
    )
    found: dict[str, bytes] = {}
    position = 0
    for name in names:
        end = output.index(b"\n", position)
        header = output[position:end].decode("ascii").split()
        if len(header) != 3:
            raise FlagError(f"git cat-file cannot read {name}")
        size = int(header[2])
        found[name] = output[end + 1 : end + 1 + size]
        position = end + 1 + size + 1
    return found


def resolve(root: Path, revision: str) -> str:
    """Returns the commit that a revision names."""
    output = layout.git(root, "rev-parse", "--verify", f"{revision}^{{commit}}")
    return output.decode("ascii").strip()


@dataclass
class Comparison:
    """The changes to the test assets between a merge base and a head."""

    base: str
    head: str
    changes: list[Change] = field(default_factory=list[Change])

    @property
    def flagged(self) -> list[Change]:
        return sorted(
            (change for change in self.changes if change.flags), key=Change.order
        )

    def count(self, action: str) -> int:
        return sum(1 for change in self.changes if change.action == action)


def classify(path: str, crates: Sequence[str]) -> tuple[str, str] | None:
    """Returns the category of a test asset and how to compare it: by its
    "rust" items, its "rust-production" test code, its "python" items, its
    "file", its "lines", its "tokens", its "shards", or its "allowlist"
    entries; or None for a file that is no test asset."""
    if path == layout.ALLOWLIST:
        return TEST_POLICY, "allowlist"
    if path in POLICY_FILES:
        return TEST_POLICY, "file"
    if path in WIRING_FILES:
        return CI_WIRING, "lines"
    if path in WIRING_TOKEN_FILES:
        return CI_WIRING, "tokens"
    if path == SHARDS_FILE:
        return CI_WIRING, "shards"
    if DOUBLES.fullmatch(path):
        return TEST_DOUBLES, "rust"
    if END_TO_END_FILES.fullmatch(path):
        return END_TO_END, "file"
    if layout.PYTHON_END_TO_END_TESTS.fullmatch(path):
        return END_TO_END, "python"
    if layout.PYTHON_UNIT_TESTS.fullmatch(path) or path in layout.PYTHON_TEST_SUPPORT:
        return TEST_CODE, "python"
    if TEST_DATA_FILES.fullmatch(path):
        return TEST_DATA, "file"
    crate = layout.crate_of(path, crates)
    if crate is None:
        return None
    relative = layout.crate_relative(path, crate)
    if path.endswith(".md"):
        return None
    if relative.startswith(layout.TEST_DATA_DIRECTORIES):
        return TEST_DATA, "file"
    role = layout.rust_role(path, crate)
    if role == "test":
        return TEST_CODE, "rust"
    if role == "production":
        return TEST_CODE, "rust-production"
    if relative.startswith("tests/") or "/tests/" in relative:
        return TEST_DATA, "file"
    return None


def _crate_path(crate: str, relative: str) -> str:
    return f"{crate}/{relative}" if crate else relative


def _target_root(relative: str) -> bool:
    """Whether a file, relative to its crate, roots one of the crate's
    targets: the library, a binary, a test, a benchmark, an example, or the
    build script."""
    directory, name = posixpath.split(relative)
    if relative in ("src/lib.rs", "src/main.rs", "build.rs"):
        return True
    return any(
        directory == targets
        or (posixpath.dirname(directory) == targets and name == "main.rs")
        for targets in ("src/bin", "tests", "benches", "examples")
    )


def _module_directory(relative: str, root: bool | None = None) -> str:
    """Returns the directory of the modules that a file, relative to its
    crate, declares: its own for a target's root or a mod.rs file, and the one
    named after it for any other. root says whether the file roots a target,
    which by default the places where Cargo discovers targets say."""
    directory, name = posixpath.split(relative)
    if root is None:
        root = _target_root(relative)
    if name == "mod.rs" or root:
        return directory
    return posixpath.join(directory, name[: -len(".rs")])


def _declarer_candidates(relative: str) -> list[str]:
    """Returns the files, relative to the crate, that may declare the module
    of the file relative, nearest first: for each directory that holds it, the
    file named after the directory and its mod.rs, main.rs, and lib.rs."""
    directory = posixpath.dirname(relative)
    if posixpath.basename(relative) == "mod.rs":
        directory = posixpath.dirname(directory)
    parts = directory.split("/") if directory else []
    found: list[str] = []
    for cut in range(len(parts), 0, -1):
        prefix = "/".join(parts[:cut])
        found += [
            f"{prefix}{suffix}" for suffix in (".rs", "/mod.rs", "/main.rs", "/lib.rs")
        ]
    return found


def _declared(
    declarer: str,
    inline: Sequence[str],
    name: str,
    path_attribute: str | None,
    root: bool | None = None,
) -> tuple[str, ...]:
    """Returns the files, relative to the crate, that may hold the module that
    `mod NAME;` declares in declarer, inside the inline modules inline, with
    the #[path] path_attribute if it has one; root says whether declarer roots
    a target."""
    directory = posixpath.join(_module_directory(declarer, root), *inline)
    if path_attribute is not None:
        if not inline:
            directory = posixpath.dirname(declarer)
        return (posixpath.normpath(posixpath.join(directory, path_attribute)),)
    return (
        posixpath.join(directory, f"{name}.rs"),
        posixpath.join(directory, name, "mod.rs"),
    )


def _path_attribute(parsed: RustFile, item: Item) -> str | None:
    for attribute in item.attributes:
        if attribute.path == "path":
            for token in parsed.tokens[attribute.start : attribute.end]:
                if token.kind == LITERAL and token.text.startswith('"'):
                    return token.text[1:-1]
    return None


def _module_declarations(
    parsed: RustFile,
) -> list[tuple[tuple[str, ...], str, str | None, frozenset[str]]]:
    """Returns the `mod NAME;` declarations of a file: the inline modules that
    hold each, its name, its #[path], and the predicates that it and those
    modules require."""
    found: list[tuple[tuple[str, ...], str, str | None, frozenset[str]]] = []

    def walk(
        items: Sequence[Item], inline: tuple[str, ...], held: frozenset[str]
    ) -> None:
        for item in items:
            if item.kind != "mod":
                continue
            conditions = held | _conjuncts(parsed, item.attributes)
            if item.body is None:
                path = _path_attribute(parsed, item)
                found.append((inline, item.name, path, conditions))
            else:
                inner = conditions | _conjuncts(parsed, item.inner_attributes)
                walk(item.children, (*inline, item.name), inner)

    walk(parsed.items, (), frozenset())
    return found


# A #[path] attribute, which may set the file of a module anywhere.
_PATH_ATTRIBUTE = re.compile(r"#\s*\[\s*path\s*=")


def crate_targets(
    available: Collection[str],
    read: Callable[[Sequence[str]], Mapping[str, str]],
    crates: Sequence[str],
) -> dict[str, dict[str, bool]]:
    """Returns, for each crate, the roots of its targets, relative to it, each
    with whether `cargo test` runs its tests, as target_roots finds them in its
    manifest and among available, the paths of a commit's files. read returns
    the text of each of the files that it is given."""
    manifests = read(
        sorted(
            manifest
            for crate in crates
            if (manifest := _crate_path(crate, "Cargo.toml")) in available
        )
    )
    members: dict[str, set[str]] = {crate: set() for crate in crates}
    for path in available:
        crate = layout.crate_of(path, crates)
        if crate is not None:
            members[crate].add(layout.crate_relative(path, crate))
    return {
        crate: layout.target_roots(
            manifests.get(_crate_path(crate, "Cargo.toml"), ""), members[crate]
        )
        for crate in crates
    }


def module_conditions(
    available: Collection[str],
    read: Callable[[Sequence[str]], Mapping[str, str]],
    crates: Sequence[str],
    paths: Iterable[str],
) -> dict[str, Conditions | None]:
    """Returns, for each Rust file among paths, the ways that its module
    compiles: for each declaration of it, through each chain of declarations
    of its ancestors, the cfg predicates that those declarations, the inline
    modules that hold them, and their files' inner attributes require; or
    None for a file that no target of its crate whose tests `cargo test` runs
    compiles, since no module that such a target's root, as crate_targets
    finds it, reaches declares it. available are the paths of a commit's
    files, and read returns the text of each of the files that it is given."""
    paths = list(paths)
    targets = crate_targets(available, read, crates)
    wanted = {
        crate for path in paths if (crate := layout.crate_of(path, crates)) is not None
    }
    texts = read(
        sorted(
            path
            for path in available
            if path.endswith(".rs") and layout.crate_of(path, crates) in wanted
        )
    )
    # The files that may declare a module in another directory with #[path].
    redirecting: dict[str, list[str]] = {crate: [] for crate in wanted}
    for path, text in sorted(texts.items()):
        crate = layout.crate_of(path, crates)
        if crate is not None and _PATH_ATTRIBUTE.search(text):
            redirecting[crate].append(layout.crate_relative(path, crate))

    def candidates(crate: str, relative: str) -> list[str]:
        """The files that may declare the module of relative: those that its
        directories name, the roots of the targets whose modules lie in one of
        its directories, and those with a #[path], which may name it from
        anywhere."""
        directory = posixpath.dirname(relative)
        return [
            *_declarer_candidates(relative),
            *(
                root
                for root, tested in sorted(targets[crate].items())
                if (directory + "/").startswith(_module_directory(root, tested) + "/")
            ),
            *redirecting[crate],
        ]

    read_declarations: dict[
        str,
        tuple[
            list[tuple[tuple[str, ...], str, str | None, frozenset[str]]],
            frozenset[str],
        ],
    ] = {}

    def declarations(
        path: str,
    ) -> tuple[
        list[tuple[tuple[str, ...], str, str | None, frozenset[str]]], frozenset[str]
    ]:
        if path not in read_declarations:
            try:
                parsed = layout.parse_rust(texts[path])
            except layout.RustSyntaxError:
                read_declarations[path] = ([], frozenset())
            else:
                read_declarations[path] = (
                    _module_declarations(parsed),
                    _conjuncts(parsed, parsed.inner_attributes),
                )
        return read_declarations[path]

    memo: dict[str, Conditions | None] = {}

    def inherited(path: str) -> Conditions | None:
        if path in memo:
            return memo[path]
        # A module that declares itself, through its ancestors, is reached by
        # none of them.
        memo[path] = None
        crate = layout.crate_of(path, crates)
        if crate is None:
            return memo[path]
        relative = layout.crate_relative(path, crate)
        # The root of a target whose tests run compiles; that of another may
        # still compile as a module that such a target declares.
        if targets[crate].get(relative):
            memo[path] = ANYWHERE
            return memo[path]
        # Every declaration that reaches the module is a way that it compiles.
        ways: set[frozenset[str]] = set()
        for candidate in candidates(crate, relative):
            full = _crate_path(crate, candidate)
            if full not in texts or full == path:
                continue
            found, own = declarations(full)
            # A root declares the modules of its directory where its target
            # compiles it, which for tests is where it runs them.
            root = targets[crate].get(candidate, False)
            for inline, name, path_attribute, conditions in found:
                if relative in _declared(candidate, inline, name, path_attribute, root):
                    parent = inherited(full)
                    if parent is not None:
                        ways.update(way | own | conditions for way in parent)
        memo[path] = _reduced(ways) if ways else None
        return memo[path]

    return {path: inherited(path) for path in paths}


def _requires_test(conditions: Conditions | None) -> bool:
    """Whether code that compiles in the ways that conditions say compiles only
    where `test` holds: each way has a predicate that requires it, as the
    `test` of a `#[cfg(test)] mod` declaration of its module or of an ancestor
    does."""

    def requires(predicate: str) -> bool:
        try:
            tokens = layout.tokenize_rust(predicate)
            partner = layout.match_brackets(tokens)
        except layout.RustSyntaxError:
            return False
        return layout.requires_test(layout.parse_cfg(tokens, partner, 0, len(tokens)))

    return bool(conditions) and all(
        any(requires(predicate) for predicate in way) for way in conditions or ()
    )


def _entries(
    path: str,
    blob: str,
    data: bytes,
    crates: Sequence[str],
    kind: tuple[str, str],
    inherited: Conditions = ANYWHERE,
) -> list[Entry]:
    category, mode = kind
    if mode in ("rust", "rust-production"):
        crate = layout.crate_of(path, crates)
        if crate is not None:
            try:
                return rust_entries(
                    path,
                    crate,
                    layout.decode(data),
                    category,
                    mode == "rust",
                    inherited,
                )
            except layout.RustSyntaxError:
                pass
    elif mode == "python":
        try:
            return python_entries(path, layout.decode(data), category)
        except (SyntaxError, ValueError, RecursionError, MemoryError):
            pass
    return [file_entry(path, blob, category)]


def _affected(
    changed: Collection[str], sides: Sequence[tuple[Tree, Sequence[str]]]
) -> set[str]:
    """Returns the Rust files of either commit in the crates whose Rust files
    or manifest changed, but the changed files: any of them may compile in
    other ways, since a changed declaration reaches the modules beneath it,
    and through a #[path] modules anywhere in its crate, and a changed
    manifest every target of its crate."""
    touched: set[str] = set()
    for path in changed:
        for _, crates in sides:
            crate = layout.crate_of(path, crates)
            if crate is not None and (
                path.endswith(".rs") or path == _crate_path(crate, "Cargo.toml")
            ):
                touched.add(crate)
    return {
        path
        for tree, crates in sides
        for path in tree.blobs
        if path.endswith(".rs")
        and path not in changed
        and layout.crate_of(path, crates) in touched
    }


def compare_commits(root: Path, base: str, head: str) -> Comparison:
    """Compares the test assets of the merge base of base and head with those
    of head."""
    head_commit = resolve(root, head)
    base_commit = (
        layout.git(root, "merge-base", resolve(root, base), head_commit)
        .decode("ascii")
        .strip()
    )
    before, after = Tree(root, base_commit), Tree(root, head_commit)
    changed = sorted(
        path
        for path in before.blobs.keys() | after.blobs.keys()
        if before.blobs.get(path) != after.blobs.get(path)
    )
    sides = (
        (before, layout.rust_crates(before.blobs)),
        (after, layout.rust_crates(after.blobs)),
    )

    def reader(tree: Tree) -> Callable[[Sequence[str]], Mapping[str, str]]:
        def read(paths: Sequence[str]) -> Mapping[str, str]:
            blobs = read_blobs(root, (tree.blobs[path] for path in paths))
            return {path: layout.decode(blobs[tree.blobs[path]]) for path in paths}

        return read

    # The ways that the modules of the Rust files compile, which a move must
    # keep, and which a changed declaration or manifest may change for any
    # unchanged file of its crate.
    affected = _affected(set(changed), sides)
    inherited = [
        module_conditions(
            tree.blobs.keys(),
            reader(tree),
            crates,
            (
                path
                for path in (*changed, *affected)
                if path in tree.blobs
                and (kind := classify(path, crates)) is not None
                and kind[1] in ("rust", "rust-production")
            ),
        )
        for tree, crates in sides
    ]
    changed = sorted(
        {
            *changed,
            *(
                path
                for path in affected
                if path in inherited[0]
                and path in inherited[1]
                and inherited[0][path] != inherited[1][path]
            ),
        }
    )
    kinds = [
        {path: kind for path in changed if (kind := classify(path, crates)) is not None}
        for _, crates in sides
    ]
    # A production file whose module only tests compile holds test code alone.
    for (tree, _), found, conditions in zip(sides, kinds, inherited, strict=True):
        test_only = [
            path
            for path, (_, mode) in found.items()
            if mode == "rust-production"
            and path in tree.blobs
            and _requires_test(conditions.get(path))
        ]
        for path in test_only:
            found[path] = (TEST_CODE, "rust")
    data = read_blobs(
        root,
        (
            tree.blobs[path]
            for (tree, _), found in zip(sides, kinds, strict=True)
            for path in found
            if path in tree.blobs
        ),
    )

    def text(side: int, path: str) -> str | None:
        tree = sides[side][0]
        if path not in tree.blobs:
            return None
        return layout.decode(data[tree.blobs[path]])

    comparison = Comparison(base_commit, head_commit)
    entries: list[list[Entry]] = [[], []]
    for path in changed:
        kind = kinds[1].get(path) or kinds[0].get(path)
        if kind is None:
            continue
        mode = kind[1]
        if mode == "allowlist":
            comparison.changes.extend(allowlist_changes(text(0, path), text(1, path)))
        elif mode == "lines":
            comparison.changes.extend(line_changes(path, text(0, path), text(1, path)))
        elif mode == "tokens":
            comparison.changes.extend(token_changes(path, text(0, path), text(1, path)))
            comparison.changes.extend(job_changes(path, text(0, path), text(1, path)))
        elif mode == "shards":
            comparison.changes.extend(shard_changes(text(0, path), text(1, path)))
        elif mode == "file" and kind[0] == TEST_POLICY:
            if path in before.blobs:
                comparison.changes.append(
                    Change(
                        TEST_POLICY,
                        "modified" if path in after.blobs else "removed",
                        "file",
                        f"`{path}`",
                        Location(path, 0),
                        Location(path, 0) if path in after.blobs else None,
                    )
                )
        else:
            for side, (tree, crates) in enumerate(sides):
                found = kinds[side].get(path)
                if found is None or path not in tree.blobs:
                    continue
                conditions = inherited[side].get(path, ANYWHERE)
                if conditions is None:
                    # No target compiles the file, so its tests never run.
                    continue
                blob = tree.blobs[path]
                entries[side].extend(
                    _entries(path, blob, data[blob], crates, found, conditions)
                )
    comparison.changes.extend(compare(entries[0], entries[1]))
    comparison.changes.sort(key=Change.order)
    return comparison


# Reports.


def _link(repository: str | None, commit: str, location: Location) -> str:
    label = f"{location.path}:{location.line}" if location.line else location.path
    label = label.replace("`", "'").replace("|", "\\|")
    if repository is None:
        return f"`{label}`"
    url = (
        f"https://github.com/{repository}/blob/{commit}/"
        f"{urllib.parse.quote(location.path)}"
    )
    if location.line:
        url += f"#L{location.line}"
    return f"[`{label}`]({url})"


def _cell(text: str) -> str:
    return text.replace("|", "\\|").replace("\n", " ").replace("@", "@\u200b")


def render(
    comparison: Comparison, repository: str | None, rows: int = ROWS_PER_SECTION
) -> str:
    """Renders the comparison as the Markdown of the pull request's comment."""
    flagged = comparison.flagged
    lines = [COMMENT_MARKER]
    guide = "doc/contribute.md#changes-to-existing-tests"
    if repository is not None:
        guide = f"[{guide}](https://github.com/{repository}/blob/dev/{guide})"
    if flagged:
        lines += [
            "### This pull request changes existing tests",
            "",
            f"It modifies or removes {len(flagged)} existing test asset(s), "
            "listed below. Review each with care: a change to a test can hide "
            f"a regression that the test caught. See {guide}.",
        ]
    else:
        lines += ["### This pull request changes no existing test"]
    for category in CATEGORIES:
        found = [change for change in flagged if change.category == category]
        if not found:
            continue
        lines += ["", f"#### {category[0].upper()}{category[1:]}", ""]
        lines += ["| Change | What | Before | After |", "| --- | --- | --- | --- |"]
        for change in found[:rows]:
            action = f"{change.action} {change.kind}"
            what = change.what + (f" ({change.detail})" if change.detail else "")
            before = (
                _link(repository, comparison.base, change.base) if change.base else ""
            )
            after = (
                _link(repository, comparison.head, change.head) if change.head else ""
            )
            lines.append(f"| {_cell(action)} | {_cell(what)} | {before} | {after} |")
        if len(found) > rows:
            lines += [
                "",
                f"... and {len(found) - rows} more, which the job's summary lists.",
            ]
    lines += [
        "",
        f"Relocated unchanged: {comparison.count('relocated')}; added: "
        f"{comparison.count('added')}. Compared the merge base "
        f"`{comparison.base[:12]}` with `{comparison.head[:12]}`.",
    ]
    return "\n".join(lines) + "\n"


def comment_body(comparison: Comparison, repository: str | None) -> str:
    """Renders the comment, with fewer rows per section until it fits."""
    rows = ROWS_PER_SECTION
    while True:
        body = render(comparison, repository, rows)
        if len(body) <= COMMENT_LIMIT or rows <= 1:
            return body[:COMMENT_LIMIT]
        rows //= 2


def summary_lines(comparison: Comparison) -> list[str]:
    """Renders the flagged changes as plain text, one per line."""
    lines: list[str] = []
    for change in comparison.flagged:
        where = " -> ".join(
            f"{location.path}:{location.line}" if location.line else location.path
            for location in (change.base, change.head)
            if location is not None
        )
        detail = f" ({change.detail})" if change.detail else ""
        lines.append(
            f"{change.category}: {change.action} {change.kind} "
            f"{change.what.replace('`', '')}{detail} [{where}]"
        )
    return lines


# Publication.

Runner = Callable[[Sequence[str], str | None], subprocess.CompletedProcess[str]]


def _run(
    arguments: Sequence[str], stdin: str | None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        list(arguments), input=stdin, capture_output=True, text=True, check=False
    )


class GitHub:
    """The calls to GitHub that flag a pull request, through the gh CLI."""

    def __init__(self, repository: str, run: Runner = _run) -> None:
        self.repository = repository
        self.run = run

    def api(
        self,
        endpoint: str,
        *,
        method: str = "GET",
        body: Mapping[str, object] | None = None,
        options: Sequence[str] = (),
        tolerate: Sequence[int] = (),
    ) -> str | None:
        """Calls the REST API; returns None for an HTTP status that tolerate
        lists, and raises FlagError for any other failure."""
        arguments = ["gh", "api", "--method", method, *options, endpoint]
        stdin = None
        if body is not None:
            arguments += ["--input", "-"]
            stdin = json.dumps(body)
        result = self.run(arguments, stdin)
        if result.returncode != 0:
            if any(f"HTTP {status}" in result.stderr for status in tolerate):
                return None
            raise FlagError(
                f"gh api {method} {endpoint} failed: {result.stderr.strip()}"
            )
        return result.stdout

    def publish(
        self, number: int, body: str, flagged: bool, label: str = LABEL
    ) -> None:
        """Adds or removes the label, and writes or resolves the comment."""
        repository = self.repository
        name = urllib.parse.quote(label, safe="")
        if flagged:
            if self.api(f"repos/{repository}/labels/{name}", tolerate=(404,)) is None:
                # Another run may create the label first.
                self.api(
                    f"repos/{repository}/labels",
                    method="POST",
                    body={
                        "name": label,
                        "color": LABEL_COLOR,
                        "description": LABEL_DESCRIPTION,
                    },
                    tolerate=(422,),
                )
            self.api(
                f"repos/{repository}/issues/{number}/labels",
                method="POST",
                body={"labels": [label]},
            )
        else:
            self.api(
                f"repos/{repository}/issues/{number}/labels/{name}",
                method="DELETE",
                tolerate=(404,),
            )
        listing = self.api(
            f"repos/{repository}/issues/{number}/comments",
            options=(
                "--paginate",
                "--jq",
                '.[] | select(.user.login == "github-actions[bot]" and '
                f'(.body | startswith("{COMMENT_MARKER}"))) | .id',
            ),
        )
        comments = [line for line in (listing or "").split() if line.isdigit()]
        if comments:
            self.api(
                f"repos/{repository}/issues/comments/{comments[0]}",
                method="PATCH",
                body={"body": body},
            )
        elif flagged:
            self.api(
                f"repos/{repository}/issues/{number}/comments",
                method="POST",
                body={"body": body},
            )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Flag the changes of a pull request to existing tests."
    )
    parser.add_argument("--root", type=Path, default=REPO_ROOT, help="Git repository")
    parser.add_argument(
        "--base",
        default=DEFAULT_BASE,
        help=f"the pull request's base; its merge base with head counts (default: {DEFAULT_BASE})",
    )
    parser.add_argument("--head", default="HEAD", help="the pull request's head")
    parser.add_argument("--repository", help="OWNER/REPO, for links and --publish")
    parser.add_argument("--report", type=Path, help="write the comment's Markdown here")
    parser.add_argument("--summary", type=Path, help="append the full report here")
    parser.add_argument(
        "--github-output", type=Path, help="append flagged= and count= here"
    )
    parser.add_argument(
        "--publish",
        type=int,
        metavar="NUMBER",
        help="label and comment on this pull request with gh; needs --repository",
    )
    parser.add_argument("--label", default=LABEL, help=f"the label (default: {LABEL})")
    args = parser.parse_args(argv)
    if args.publish is not None and not args.repository:
        parser.error("--publish needs --repository")
    try:
        comparison = compare_commits(args.root, args.base, args.head)
        flagged = comparison.flagged
        body = comment_body(comparison, args.repository)
        if args.report is not None:
            args.report.write_text(body, encoding="utf-8")
        if args.summary is not None:
            with args.summary.open("a", encoding="utf-8") as summary:
                summary.write(render(comparison, args.repository, len(flagged) or 1))
        if args.github_output is not None:
            with args.github_output.open("a", encoding="utf-8") as output:
                output.write(f"flagged={'true' if flagged else 'false'}\n")
                output.write(f"count={len(flagged)}\n")
        if args.publish is not None:
            GitHub(args.repository).publish(
                args.publish, body, bool(flagged), args.label
            )
    except (layout.CheckError, FlagError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    for line in summary_lines(comparison):
        print(line)
    print(
        f"{len(flagged)} change(s) to existing tests; relocated unchanged: "
        f"{comparison.count('relocated')}; added: {comparison.count('added')}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

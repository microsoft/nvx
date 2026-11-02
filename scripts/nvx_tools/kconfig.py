"""Read a Linux Kconfig tree well enough to explain a configuration.

The kernel feature inventory names the settings that the microVM kernel makes
on purpose, and Kconfig derives every other symbol of the configuration from
them. This module reads the declarations of the Kconfig language (types,
prompts, dependencies, defaults, `select`, and `imply`) and evaluates their
conditions against a configuration, so that NVX can tell, for each symbol that
a configuration sets, whether a user can set it at all and which rule set it.

It does not resolve configurations, which `make allnoconfig` and
`make olddefconfig` do, and it does not run the Kconfig preprocessor's
functions, such as `$(cc-option,...)`, which probe the toolchain. A condition
that calls one is unknown, and the symbols that such probes define, such as
`CC_HAS_*`, carry the values that the configuration records.
"""

from __future__ import annotations

import re
from collections.abc import Iterator, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

from .common import ScriptError


@dataclass(frozen=True, slots=True)
class Ref:
    """A symbol, or an unquoted constant such as `y` or `0x1000`."""

    name: str


@dataclass(frozen=True, slots=True)
class Text:
    """A quoted string constant."""

    value: str


@dataclass(frozen=True, slots=True)
class Macro:
    """A preprocessor call such as `$(cc-option,-mfoo)`, whose value is
    unknown."""

    text: str


@dataclass(frozen=True, slots=True)
class Not:
    operand: Expr


@dataclass(frozen=True, slots=True)
class And:
    left: Expr
    right: Expr


@dataclass(frozen=True, slots=True)
class Or:
    left: Expr
    right: Expr


@dataclass(frozen=True, slots=True)
class Compare:
    operator: str
    left: Expr
    right: Expr


Expr = Ref | Text | Macro | Not | And | Or | Compare

# Tristate values, ordered as Kconfig orders them, and None for unknown.
Tristate = Literal[0, 1, 2]
NO: Tristate = 0
MOD: Tristate = 1
YES: Tristate = 2
_TRISTATE_CONSTANTS: dict[str, Tristate] = {"n": NO, "m": MOD, "y": YES}

SymbolType = Literal["bool", "tristate", "string", "int", "hex"]
# The keywords that give an entry its type, and the ones of them that also
# give it a default rather than a prompt.
_TYPE_KEYWORDS: dict[str, SymbolType] = {
    "bool": "bool",
    "tristate": "tristate",
    "string": "string",
    "int": "int",
    "hex": "hex",
    "def_bool": "bool",
    "def_tristate": "tristate",
}
_DEFAULT_TYPE_KEYWORDS = ("def_bool", "def_tristate")
_TOKEN = re.compile(
    r"\s*(?:"
    r"(?P<macro>\$\()"
    r"|(?P<string>\"(?:[^\"\\]|\\.)*\"|'(?:[^'\\]|\\.)*')"
    r"|(?P<operator>&&|\|\||!=|<=|>=|[=<>!()])"
    r"|(?P<word>[^\s=<>!()&|\"']+)"
    r")"
)
_MACRO_ASSIGNMENT = re.compile(r"^[A-Za-z0-9_-]+\s*(?::=|\+=|=)")


def _tokens(text: str) -> list[str]:
    tokens: list[str] = []
    position = 0
    while position < len(text):
        if text[position:].strip() == "":
            break
        match = _TOKEN.match(text, position)
        if match is None:
            raise ValueError(f"cannot tokenize {text[position:]!r}")
        if match.group("macro") is not None:
            start = match.start("macro")
            depth = 0
            end = start
            while end < len(text):
                if text[end] == "(":
                    depth += 1
                elif text[end] == ")":
                    depth -= 1
                    if depth == 0:
                        break
                end += 1
            if depth != 0:
                raise ValueError(f"unterminated macro in {text!r}")
            tokens.append(text[start : end + 1])
            position = end + 1
            continue
        tokens.append(match.group(match.lastgroup or "word"))
        position = match.end()
    return tokens


class _ExpressionParser:
    def __init__(self, tokens: list[str]) -> None:
        self._tokens = tokens
        self._position = 0

    def parse(self) -> Expr:
        expression = self._or()
        if self._position != len(self._tokens):
            raise ValueError(f"unexpected {self._tokens[self._position]!r}")
        return expression

    def _peek(self) -> str | None:
        if self._position < len(self._tokens):
            return self._tokens[self._position]
        return None

    def _take(self) -> str:
        token = self._peek()
        if token is None:
            raise ValueError("unexpected end of expression")
        self._position += 1
        return token

    def _or(self) -> Expr:
        expression = self._and()
        while self._peek() == "||":
            self._take()
            expression = Or(expression, self._and())
        return expression

    def _and(self) -> Expr:
        expression = self._not()
        while self._peek() == "&&":
            self._take()
            expression = And(expression, self._not())
        return expression

    def _not(self) -> Expr:
        if self._peek() == "!":
            self._take()
            return Not(self._not())
        return self._comparison()

    def _comparison(self) -> Expr:
        expression = self._atom()
        if self._peek() in ("=", "!=", "<", "<=", ">", ">="):
            operator = self._take()
            expression = Compare(operator, expression, self._atom())
        return expression

    def _atom(self) -> Expr:
        token = self._take()
        if token == "(":
            expression = self._or()
            if self._take() != ")":
                raise ValueError("missing )")
            return expression
        if token.startswith("$("):
            return Macro(token)
        if token[0] in "\"'":
            return Text(token[1:-1])
        if token in ("&&", "||", "=", "!=", "<", "<=", ">", ">=", ")"):
            raise ValueError(f"unexpected {token!r}")
        return Ref(token)


def parse_expression(text: str) -> Expr:
    """Parse a Kconfig expression, such as `X86_64 && !(A || B = "y")`."""
    return _ExpressionParser(_tokens(text)).parse()


def format_expression(expression: Expr) -> str:
    """Render an expression as Kconfig source, with the parentheses that its
    precedence needs."""
    return _format(expression, 0)


def _format(expression: Expr, context: int) -> str:
    match expression:
        case Ref(name):
            return name
        case Text(value):
            return f'"{value}"'
        case Macro(text):
            return text
        case Not(operand):
            return "!" + _format(operand, 3)
        case Compare(operator, left, right):
            return f"{_format(left, 4)}{operator}{_format(right, 4)}"
        case And(left, right):
            text = f"{_format(left, 2)} && {_format(right, 2)}"
            return f"({text})" if context > 2 else text
        case Or(left, right):
            text = f"{_format(left, 1)} || {_format(right, 1)}"
            return f"({text})" if context > 1 else text


def _and(left: Expr | None, right: Expr | None) -> Expr | None:
    if left is None:
        return right
    if right is None:
        return left
    return And(left, right)


@dataclass(slots=True)
class Definition:
    """One `config` or `menuconfig` entry of a symbol, with the conditions
    that its enclosing `if`, `menu`, and `choice` blocks add."""

    path: str
    line: int
    prompt: str | None = None
    # The `if` of the prompt.
    prompt_condition: Expr | None = None
    # The entry's `depends on`, and its blocks' conditions and dependencies.
    dependency: Expr | None = None
    # The `visible if` of the enclosing menus.
    visibility: Expr | None = None
    defaults: list[tuple[Expr, Expr | None]] = field(
        default_factory=lambda: list[tuple[Expr, Expr | None]]()
    )
    selects: list[tuple[str, Expr | None]] = field(
        default_factory=lambda: list[tuple[str, Expr | None]]()
    )
    implies: list[tuple[str, Expr | None]] = field(
        default_factory=lambda: list[tuple[str, Expr | None]]()
    )
    choice: int | None = None


@dataclass(slots=True)
class KconfigSymbol:
    name: str
    type: SymbolType | None = None
    definitions: list[Definition] = field(default_factory=lambda: list[Definition]())

    @property
    def prompted(self) -> bool:
        """Whether any entry of the symbol has a prompt, through which a
        user can set it when the prompt is visible."""
        return any(definition.prompt is not None for definition in self.definitions)


@dataclass(slots=True)
class Choice:
    index: int
    path: str
    line: int
    prompt: str | None = None
    dependency: Expr | None = None
    defaults: list[tuple[str, Expr | None]] = field(
        default_factory=lambda: list[tuple[str, Expr | None]]()
    )
    members: list[str] = field(default_factory=lambda: list[str]())


@dataclass(slots=True)
class _Block:
    kind: Literal["if", "menu", "choice"]
    dependency: Expr | None
    visibility: Expr | None = None
    choice: Choice | None = None


@dataclass(slots=True)
class _Entry:
    kind: Literal["config", "choice", "menu", "comment"]
    definition: Definition | None = None
    symbol: KconfigSymbol | None = None
    block: _Block | None = None


class KconfigTree:
    """The symbols and choices that a Kconfig tree declares."""

    def __init__(
        self,
        symbols: dict[str, KconfigSymbol],
        choices: list[Choice],
    ) -> None:
        self.symbols = symbols
        self.choices = choices
        self._selectors: dict[str, list[tuple[str, Expr | None]]] = {}
        self._implicators: dict[str, list[tuple[str, Expr | None]]] = {}
        for symbol in symbols.values():
            for definition in symbol.definitions:
                # As in Kconfig, an entry's select or imply applies only where
                # the entry's own dependencies, its blocks' included, hold, so
                # that another entry of the same symbol does not apply it.
                for target, condition in definition.selects:
                    self._selectors.setdefault(target, []).append(
                        (symbol.name, _and(definition.dependency, condition))
                    )
                for target, condition in definition.implies:
                    self._implicators.setdefault(target, []).append(
                        (symbol.name, _and(definition.dependency, condition))
                    )

    def selectors(self, name: str) -> list[tuple[str, Expr | None]]:
        """The symbols that `select` a symbol, each with the condition under
        which its select applies: the select's `if` and the dependencies of
        the entry that holds it."""
        return self._selectors.get(name, [])

    def implicators(self, name: str) -> list[tuple[str, Expr | None]]:
        """The symbols that `imply` a symbol, each with the condition under
        which its imply applies, as for `selectors`."""
        return self._implicators.get(name, [])

    def choice_of(self, name: str) -> Choice | None:
        symbol = self.symbols.get(name)
        if symbol is None:
            return None
        for definition in symbol.definitions:
            if definition.choice is not None:
                return self.choices[definition.choice]
        return None


def _logical_lines(path: Path) -> Iterator[tuple[int, str]]:
    """Yield each line with its continuations joined, numbered from 1."""
    pending: list[str] = []
    start = 0
    for number, raw in enumerate(
        path.read_text(encoding="utf-8", errors="replace").splitlines(), start=1
    ):
        if not pending:
            start = number
        if raw.endswith("\\"):
            pending.append(raw[:-1])
            continue
        pending.append(raw)
        yield start, " ".join(pending) if len(pending) > 1 else pending[0]
        pending = []
    if pending:
        yield start, " ".join(pending)


def _strip_comment(line: str) -> str:
    quote: str | None = None
    escaped = False
    for index, character in enumerate(line):
        if escaped:
            escaped = False
        elif character == "\\":
            escaped = True
        elif quote is not None:
            if character == quote:
                quote = None
        elif character in "\"'":
            quote = character
        elif character == "#":
            return line[:index]
    return line


def _indentation(line: str) -> int:
    width = 0
    for character in line:
        if character == " ":
            width += 1
        elif character == "\t":
            width = (width // 8 + 1) * 8
        else:
            break
    return width


def _split_condition(text: str) -> tuple[str, Expr | None]:
    """Split `value if condition`, at an `if` outside quotes, parentheses, and
    macros."""
    tokens = _tokens(text)
    depth = 0
    for index, token in enumerate(tokens):
        if token == "(":
            depth += 1
        elif token == ")":
            depth -= 1
        elif token == "if" and depth == 0:
            return (
                " ".join(tokens[:index]),
                _ExpressionParser(tokens[index + 1 :]).parse(),
            )
    return " ".join(tokens), None


def _prompt_text(text: str) -> str:
    text = text.strip()
    if len(text) >= 2 and text[0] in "\"'" and text[-1] == text[0]:
        return text[1:-1]
    return text


class _Reader:
    def __init__(self, source: Path, variables: Mapping[str, str]) -> None:
        self._source = source
        self._variables = dict(variables)
        self._symbols: dict[str, KconfigSymbol] = {}
        self._choices: list[Choice] = []
        self._blocks: list[_Block] = []
        self._entry: _Entry | None = None

    def read(self, kconfig: str) -> KconfigTree:
        self._read_file(kconfig)
        if self._blocks:
            raise ScriptError(
                f"Kconfig ends inside an unclosed {self._blocks[-1].kind}"
            )
        return KconfigTree(self._symbols, self._choices)

    def _expand(self, text: str) -> str:
        def substitute(match: re.Match[str]) -> str:
            name = match.group(1)
            if name not in self._variables:
                raise ScriptError(f"Kconfig source path uses unknown $({name})")
            return self._variables[name]

        return re.sub(r"\$\(([A-Za-z0-9_]+)\)", substitute, text)

    def _expand_known(self, text: str) -> str:
        return re.sub(
            r"\$\(([A-Za-z0-9_]+)\)",
            lambda match: self._variables.get(match.group(1), match.group(0)),
            text,
        )

    def _block_dependency(self) -> Expr | None:
        dependency: Expr | None = None
        for block in self._blocks:
            dependency = _and(dependency, block.dependency)
        return dependency

    def _block_visibility(self) -> Expr | None:
        visibility: Expr | None = None
        for block in self._blocks:
            visibility = _and(visibility, block.visibility)
        return visibility

    def _enclosing_choice(self) -> Choice | None:
        for block in reversed(self._blocks):
            if block.kind == "choice":
                return block.choice
        return None

    def _read_file(self, relative: str) -> None:
        path = self._source / relative
        if not path.is_file():
            raise ScriptError(f"Kconfig file {relative} is missing")
        lines = list(_logical_lines(path))
        index = 0
        while index < len(lines):
            number, raw = lines[index]
            index += 1
            line = _strip_comment(raw).strip()
            if not line:
                continue
            # Expand the variables that the reader knows, such as $(SRCARCH),
            # wherever they appear, and leave other preprocessor calls alone.
            line = self._expand_known(line)
            parts = line.split(None, 1)
            keyword = parts[0]
            rest = parts[1].strip() if len(parts) > 1 else ""
            if keyword in ("help", "---help---"):
                index = self._skip_help(lines, index)
                continue
            try:
                self._statement(relative, number, keyword, rest, line)
            except ValueError as error:
                raise ScriptError(
                    f"{relative}:{number}: cannot read {line!r}: {error}"
                ) from error

    @staticmethod
    def _skip_help(lines: list[tuple[int, str]], index: int) -> int:
        indent: int | None = None
        while index < len(lines):
            raw = lines[index][1]
            if raw.strip() == "":
                index += 1
                continue
            width = _indentation(raw)
            if indent is None:
                if width == 0:
                    return index
                indent = width
            elif width < indent:
                return index
            index += 1
        return index

    def _statement(
        self, path: str, number: int, keyword: str, rest: str, line: str
    ) -> None:
        if keyword in ("config", "menuconfig"):
            self._start_config(path, number, rest)
        elif keyword == "choice":
            self._start_choice(path, number)
        elif keyword == "endchoice":
            self._close("choice")
        elif keyword == "menu":
            self._entry = None
            block = _Block("menu", None)
            self._blocks.append(block)
            self._entry = _Entry("menu", block=block)
        elif keyword == "endmenu":
            self._close("menu")
        elif keyword == "if":
            self._entry = None
            self._blocks.append(_Block("if", parse_expression(rest)))
        elif keyword == "endif":
            self._close("if")
        elif keyword == "comment":
            self._entry = _Entry("comment")
        elif keyword in ("source", "rsource", "osource", "orsource"):
            self._entry = None
            included = self._expand(_prompt_text(rest))
            if keyword in ("rsource", "orsource"):
                included = str(Path(path).parent / included)
            if keyword.startswith("o") and not (self._source / included).is_file():
                return
            self._read_file(included)
        elif keyword == "mainmenu":
            self._entry = None
        elif _MACRO_ASSIGNMENT.match(line) or line.startswith("$("):
            # A preprocessor variable, or a call such as $(error-if,...) that
            # expands to nothing.
            self._entry = None
        else:
            self._attribute(keyword, rest)

    def _close(self, kind: str) -> None:
        self._entry = None
        if not self._blocks or self._blocks[-1].kind != kind:
            raise ValueError(f"end{kind} without {kind}")
        self._blocks.pop()

    def _start_config(self, path: str, number: int, name: str) -> None:
        symbol = self._symbols.setdefault(name, KconfigSymbol(name))
        choice = self._enclosing_choice()
        definition = Definition(
            path=path,
            line=number,
            dependency=self._block_dependency(),
            visibility=self._block_visibility(),
            choice=choice.index if choice is not None else None,
        )
        if choice is not None and name not in choice.members:
            choice.members.append(name)
        symbol.definitions.append(definition)
        self._entry = _Entry("config", definition=definition, symbol=symbol)

    def _start_choice(self, path: str, number: int) -> None:
        choice = Choice(
            index=len(self._choices),
            path=path,
            line=number,
            dependency=self._block_dependency(),
        )
        self._choices.append(choice)
        block = _Block("choice", None, choice=choice)
        self._blocks.append(block)
        self._entry = _Entry("choice", block=block)

    def _attribute(self, keyword: str, rest: str) -> None:
        entry = self._entry
        if entry is None:
            raise ValueError(f"{keyword} outside an entry")
        if keyword == "depends":
            if not rest.startswith("on"):
                raise ValueError("depends without on")
            self._depends(entry, parse_expression(rest[2:]))
        elif keyword == "visible":
            if not rest.startswith("if") or entry.block is None:
                raise ValueError("visible without if, or outside a menu")
            entry.block.visibility = _and(
                entry.block.visibility, parse_expression(rest[2:])
            )
        elif entry.kind == "config":
            self._config_attribute(entry, keyword, rest)
        elif entry.kind == "choice":
            self._choice_attribute(entry, keyword, rest)
        elif keyword not in ("optional",):
            raise ValueError(f"unexpected {keyword} in a {entry.kind}")

    def _depends(self, entry: _Entry, dependency: Expr) -> None:
        if entry.definition is not None:
            entry.definition.dependency = _and(entry.definition.dependency, dependency)
        elif entry.block is not None:
            entry.block.dependency = _and(entry.block.dependency, dependency)
            if entry.block.choice is not None:
                entry.block.choice.dependency = _and(
                    entry.block.choice.dependency, dependency
                )

    def _config_attribute(self, entry: _Entry, keyword: str, rest: str) -> None:
        definition = entry.definition
        symbol = entry.symbol
        assert definition is not None and symbol is not None
        if keyword in _TYPE_KEYWORDS:
            symbol.type = _TYPE_KEYWORDS[keyword]
            if not rest:
                return
            value, condition = _split_condition(rest)
            if keyword in _DEFAULT_TYPE_KEYWORDS:
                definition.defaults.append((parse_expression(value), condition))
            else:
                definition.prompt = _prompt_text(value)
                definition.prompt_condition = condition
        elif keyword == "prompt":
            value, condition = _split_condition(rest)
            definition.prompt = _prompt_text(value)
            definition.prompt_condition = condition
        elif keyword == "default":
            value, condition = _split_condition(rest)
            definition.defaults.append((parse_expression(value), condition))
        elif keyword in ("select", "imply"):
            value, condition = _split_condition(rest)
            target = definition.selects if keyword == "select" else definition.implies
            target.append((value, condition))
        elif keyword in ("range", "option", "modules", "transitional"):
            return
        else:
            raise ValueError(f"unknown attribute {keyword}")

    @staticmethod
    def _choice_attribute(entry: _Entry, keyword: str, rest: str) -> None:
        assert entry.block is not None and entry.block.choice is not None
        choice = entry.block.choice
        if keyword in ("bool", "tristate", "prompt"):
            if rest:
                choice.prompt = _prompt_text(_split_condition(rest)[0])
        elif keyword == "default":
            value, condition = _split_condition(rest)
            choice.defaults.append((value, condition))
        elif keyword not in ("optional",):
            raise ValueError(f"unknown choice attribute {keyword}")


def read_kconfig(
    source: Path, *, srcarch: str = "x86", arch: str = "x86"
) -> KconfigTree:
    """Read the Kconfig tree of a kernel source directory."""
    return _Reader(
        source,
        {"SRCARCH": srcarch, "ARCH": arch, "HEADER_ARCH": srcarch},
    ).read("Kconfig")


class ConfigValues:
    """The values that a configuration gives its symbols, for evaluating
    Kconfig conditions."""

    def __init__(self, tree: KconfigTree, values: Mapping[str, str]) -> None:
        self._tree = tree
        self._values = dict(values)

    def raw(self, name: str) -> str | None:
        return self._values.get(name)

    def tristate(self, name: str) -> Tristate:
        value = self._values.get(name)
        symbol = self._tree.symbols.get(name)
        if symbol is None or symbol.type not in ("bool", "tristate"):
            return NO
        if value == "y":
            return YES
        if value == "m":
            return MOD
        return NO

    def evaluate(self, expression: Expr | None) -> Tristate | None:
        """Evaluate a condition: YES, MOD, or NO, or None when it calls a
        preprocessor function. A missing condition is YES."""
        if expression is None:
            return YES
        match expression:
            case Ref(name):
                if name in _TRISTATE_CONSTANTS:
                    return _TRISTATE_CONSTANTS[name]
                return self.tristate(name)
            case Text(value):
                return _TRISTATE_CONSTANTS.get(value, NO)
            case Macro():
                return None
            case Not(operand):
                value = self.evaluate(operand)
                return None if value is None else _tristate(2 - value)
            case And(left, right):
                return _combine(self.evaluate(left), self.evaluate(right), "&&")
            case Or(left, right):
                return _combine(self.evaluate(left), self.evaluate(right), "||")
            case Compare(operator, left, right):
                return self._compare(operator, left, right)

    def _string(self, expression: Expr) -> str | None:
        match expression:
            case Ref(name):
                symbol = self._tree.symbols.get(name)
                if symbol is None:
                    return name
                if symbol.type in ("bool", "tristate"):
                    return "nmy"[self.tristate(name)]
                return self._values.get(name, "")
            case Text(value):
                return value
            case _:
                return None

    def _compare(self, operator: str, left: Expr, right: Expr) -> Tristate | None:
        first = self._string(left)
        second = self._string(right)
        if first is None or second is None:
            return None
        try:
            order = _order(int(first, 0), int(second, 0))
        except ValueError:
            order = _order(first, second)
        result = {
            "=": order == 0,
            "!=": order != 0,
            "<": order < 0,
            "<=": order <= 0,
            ">": order > 0,
            ">=": order >= 0,
        }[operator]
        return YES if result else NO


def _order(first: int | str, second: int | str) -> int:
    if first == second:
        return 0
    if isinstance(first, int) and isinstance(second, int):
        return -1 if first < second else 1
    return -1 if str(first) < str(second) else 1


def _tristate(value: int) -> Tristate:
    if value <= 0:
        return NO
    if value == 1:
        return MOD
    return YES


def _combine(
    left: Tristate | None, right: Tristate | None, operation: str
) -> Tristate | None:
    """Combine two conditions with `&&` or `||`, where an unknown operand
    leaves the result unknown unless the other decides it."""
    deciding = NO if operation == "&&" else YES
    if left == deciding or right == deciding:
        return deciding
    if left is None or right is None:
        return None
    return _tristate(min(left, right) if operation == "&&" else max(left, right))


def prompt_visibility(
    tree: KconfigTree, values: ConfigValues, name: str
) -> Tristate | None:
    """Whether a user can set a symbol in a configuration: the visibility of
    its most visible prompt, NO when it has none, or None when a toolchain
    probe decides it."""
    symbol = tree.symbols.get(name)
    if symbol is None:
        return NO
    visibility: Tristate | None = NO
    for definition in symbol.definitions:
        if definition.prompt is None:
            continue
        condition = _and(
            _and(definition.prompt_condition, definition.dependency),
            definition.visibility,
        )
        visibility = _combine(visibility, values.evaluate(condition), "||")
    return visibility


def active_selectors(tree: KconfigTree, values: ConfigValues, name: str) -> list[str]:
    """The enabled symbols whose `select` of a symbol applies in a
    configuration, including those whose condition a toolchain probe
    decides."""
    return sorted(
        {
            selector
            for selector, condition in tree.selectors(name)
            if values.tristate(selector) != NO and values.evaluate(condition) != NO
        }
    )


def active_implicators(tree: KconfigTree, values: ConfigValues, name: str) -> list[str]:
    """The enabled symbols whose `imply` of a symbol applies in a
    configuration."""
    return sorted(
        {
            implicator
            for implicator, condition in tree.implicators(name)
            if values.tristate(implicator) != NO and values.evaluate(condition) != NO
        }
    )


def hidden_prompt_conditions(
    tree: KconfigTree, values: ConfigValues, name: str
) -> list[str]:
    """The conditions that hide a symbol's prompts in a configuration: each
    prompt's `if`, or the `visible if` of its menus."""
    symbol = tree.symbols.get(name)
    if symbol is None:
        return []
    conditions: list[str] = []
    for definition in symbol.definitions:
        if definition.prompt is None:
            continue
        for condition in (
            definition.prompt_condition,
            definition.visibility,
            definition.dependency,
        ):
            if condition is not None and values.evaluate(condition) != YES:
                text = format_expression(condition)
                if text not in conditions:
                    conditions.append(text)
                break
    return conditions

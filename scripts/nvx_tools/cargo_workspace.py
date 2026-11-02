"""Cargo's manifests and lockfiles, and the packages and workspaces of a
checkout that they describe.

Python 3.10, which the scripts support, has no TOML reader, so read_toml()
reads the TOML of these files, which Cargo reads or writes, as the pinned
Cargo does: tables, arrays of tables, bare, quoted, and dotted keys, strings,
integers, floats, booleans, dates and times, arrays, and inline tables. That
is TOML 1.1: beyond the TOML 1.0 of Python 3.11's tomllib, an inline table may
hold line breaks and comments, which Cargo allows even around the = of a pair,
and end with a comma, a string may escape the escape character and two hex
digits, and a time may omit its seconds. It refuses the syntax errors and the
keys and tables defined twice that it meets, and reads a time in a leap second,
which Python's times cannot hold, as a LeapSecond.
"""

from __future__ import annotations

import datetime
import os
import re
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from .common import ScriptError

# A table of a TOML document, or the document itself.
Table = dict[str, object]

MANIFEST = "Cargo.toml"
LOCKFILE = "Cargo.lock"
# The Cargo configuration of a directory, which Cargo reads when it runs there
# or in a directory beneath it: the first of these files that the directory
# holds, since Cargo prefers the name without the extension.
CONFIGURATION = (Path(".cargo") / "config", Path(".cargo") / "config.toml")
# The tables of a manifest, and of its `target.<platform>` tables, that hold the
# dependencies of a package's build. Its dev-dependencies build only its tests,
# examples, and benchmarks.
BUILD_DEPENDENCIES = ("dependencies", "build-dependencies", "build_dependencies")
DEPENDENCIES = (*BUILD_DEPENDENCIES, "dev-dependencies", "dev_dependencies")

_BARE_KEY = re.compile(r"[A-Za-z0-9_-]+")
_INTEGER = re.compile(
    r"0x[0-9A-Fa-f](?:_?[0-9A-Fa-f])*|0o[0-7](?:_?[0-7])*|0b[01](?:_?[01])*"
    r"|[+-]?(?:0|[1-9](?:_?[0-9])*)"
)
_DIGITS = r"[0-9](?:_?[0-9])*"
_EXPONENT = rf"[eE][+-]?{_DIGITS}"
_FLOAT = re.compile(
    rf"[+-]?(?:inf|nan|(?:0|[1-9](?:_?[0-9])*)(?:\.{_DIGITS}(?:{_EXPONENT})?|{_EXPONENT}))"
)
# A float that is not a number equals none, not even itself, so the reader
# reads each sign of one as a single float: documents that hold the same ones
# in the same places are then equal, for Python's containers take an object
# for equal to itself.
_NOT_A_NUMBER = float("nan")
_NOT_NUMBERS = {"nan": _NOT_A_NUMBER, "+nan": _NOT_A_NUMBER, "-nan": -_NOT_A_NUMBER}
# A time, with its seconds and their fraction if any.
_TIME = r"([0-9]{2}):([0-9]{2})(?::([0-9]{2})(?:\.([0-9]+))?)?"
# A date, and its time and offset if any, or a time alone. datetime checks the
# ranges of a date and a time, but not those of an offset.
_DATE_TIME = re.compile(
    rf"([0-9]{{4}})-([0-9]{{2}})-([0-9]{{2}})"
    rf"(?:[Tt ]{_TIME}(?:([Zz])|([+-])([01][0-9]|2[0-3]):([0-5][0-9]))?)?|{_TIME}"
)
_HEX_DIGITS = re.compile(r"[0-9A-Fa-f]+")
_LINE_CONTINUATION = re.compile(r"\\[ \t]*\n[ \t\n]*")
_ESCAPES = {
    "b": "\b",
    "t": "\t",
    "n": "\n",
    "f": "\f",
    "r": "\r",
    "e": "\x1b",
    '"': '"',
    "\\": "\\",
}
# What may follow a value: a space, a comment, a separator, the end of its
# container or of its line, or the end of the document.
_VALUE_ENDS = frozenset(" \t\n#,]}")


class _Reader:
    """A TOML document, read from its start."""

    def __init__(self, text: str, source: str) -> None:
        self.text = text.replace("\r\n", "\n")
        self.source = source
        self.index = 0
        # Inline tables and arrays are complete where they end, a header or a
        # dotted key defines a table once, and only an array of tables that
        # headers made takes more tables. Dotted keys extend no table that a
        # header defined, and define each table that they extend, even one
        # that a header only implied.
        self.closed: set[int] = set()
        self.defined: set[int] = set()
        self.headed: set[int] = set()
        self.table_arrays: set[int] = set()

    def error(self, message: str) -> ScriptError:
        line = self.text.count("\n", 0, self.index) + 1
        return ScriptError(f"{self.source}, line {line}: {message}")

    def at(self, token: str) -> bool:
        return self.text.startswith(token, self.index)

    def skip(self, *, newlines: bool) -> None:
        """Skip spaces and comments, and line ends too when `newlines`."""
        spaces = " \t\n" if newlines else " \t"
        while self.index < len(self.text):
            if self.text[self.index] in spaces:
                self.index += 1
            elif self.at("#"):
                end = self.text.find("\n", self.index)
                self.index = len(self.text) if end < 0 else end
            else:
                return

    def expect(self, token: str) -> None:
        self.skip(newlines=False)
        if not self.at(token):
            raise self.error(f"expected {token!r}")
        self.index += len(token)

    def document(self) -> Table:
        # TOML allows no control character but tabs and line ends, even in a
        # string or a comment.
        control = re.search(r"[\x00-\x08\x0b-\x1f\x7f]", self.text)
        if control is not None:
            self.index = control.start()
            raise self.error(f"control character {control[0]!r}")
        root: Table = {}
        table = root
        while True:
            self.skip(newlines=True)
            if self.index == len(self.text):
                return root
            if self.at("["):
                table = self.header(root)
            else:
                self.pair(table)
            self.skip(newlines=False)
            if self.index < len(self.text) and not self.at("\n"):
                raise self.error("expected the end of the line")

    def header(self, root: Table) -> Table:
        """Read a [table] or [[array of tables]] header, and return the table
        that the following pairs define."""
        array = self.at("[[")
        self.index += 2 if array else 1
        self.skip(newlines=False)
        keys = self.keys()
        self.expect("]]" if array else "]")
        parent = root
        for key in keys[:-1]:
            parent = self.descend(parent, key)
        name, last = ".".join(keys), keys[-1]
        entry: Table = {}
        if array:
            if last not in parent:
                tables: list[object] = []
                parent[last] = tables
                self.table_arrays.add(id(tables))
            value = parent[last]
            if id(value) not in self.table_arrays:
                raise self.error(f"{name} is not an array of tables")
            cast(list[object], value).append(entry)
            return entry
        value = parent.setdefault(last, entry)
        identity = id(value)
        if (
            identity in self.defined
            or identity in self.closed
            or not isinstance(value, dict)
        ):
            raise self.error(f"table {name} is defined twice")
        self.defined.add(identity)
        self.headed.add(identity)
        return cast(Table, value)

    def descend(self, table: Table, key: str) -> Table:
        """Return the table `key` of `table`, or the last table of the array
        of tables `key`, in which a header defines a table."""
        new: Table = {}
        value = table.setdefault(key, new)
        if id(value) in self.table_arrays:
            value = cast(list[object], value)[-1]
        if id(value) in self.closed or not isinstance(value, dict):
            raise self.error(f"{key} is not a table")
        return cast(Table, value)

    def pair(self, table: Table, *, newlines: bool = False) -> None:
        """Read a pair, with line breaks around its = when `newlines`."""
        keys = self.keys()
        self.skip(newlines=newlines)
        self.expect("=")
        self.skip(newlines=newlines)
        value = self.value()
        for key in keys[:-1]:
            child = table.setdefault(key, {})
            if id(child) in self.headed:
                raise self.error(f"table {key} is defined twice")
            if id(child) in self.closed or not isinstance(child, dict):
                raise self.error(f"{key} is not a table")
            table = cast(Table, child)
            self.defined.add(id(table))
        if keys[-1] in table:
            raise self.error(f"{'.'.join(keys)} is defined twice")
        table[keys[-1]] = value

    def keys(self) -> list[str]:
        """Read a key, dotted or not, and the spaces after it."""
        keys = [self.key()]
        self.skip(newlines=False)
        while self.at("."):
            self.index += 1
            self.skip(newlines=False)
            keys.append(self.key())
            self.skip(newlines=False)
        return keys

    def key(self) -> str:
        if self.at('"""') or self.at("'''"):
            raise self.error("a key cannot be a multi-line string")
        if self.at('"') or self.at("'"):
            return self.string()
        match = _BARE_KEY.match(self.text, self.index)
        if match is None:
            raise self.error("expected a key")
        self.index = match.end()
        return match.group()

    def value(self) -> object:
        if self.at('"') or self.at("'"):
            return self.string()
        if self.at("["):
            return self.array()
        if self.at("{"):
            return self.inline_table()
        value: object
        if self.at("true") or self.at("false"):
            value = self.at("true")
            self.index += 4 if value else 5
        elif moment := _DATE_TIME.match(self.text, self.index):
            self.index = moment.end()
            try:
                value = _moment(moment)
            except ValueError as error:
                raise self.error(f"invalid date or time: {error}") from error
        elif number := _FLOAT.match(self.text, self.index):
            self.index = number.end()
            text = number.group()
            value = _NOT_NUMBERS.get(text) or float(text.replace("_", ""))
        else:
            match = _INTEGER.match(self.text, self.index)
            if match is None:
                raise self.error("expected a value")
            self.index = match.end()
            value = int(match.group().replace("_", ""), 0)
        if self.index < len(self.text) and self.text[self.index] not in _VALUE_ENDS:
            raise self.error("invalid value")
        return value

    def string(self) -> str:
        quote = self.text[self.index]
        multiline = self.at(quote * 3)
        delimiter = quote * 3 if multiline else quote
        self.index += len(delimiter)
        if multiline and self.at("\n"):
            self.index += 1
        parts: list[str] = []
        while not self.at(delimiter):
            if self.index == len(self.text) or (self.at("\n") and not multiline):
                raise self.error("unterminated string")
            if quote == '"' and self.at("\\"):
                parts.append(self.escape(multiline))
            else:
                parts.append(self.text[self.index])
                self.index += 1
        end = self.index + len(delimiter)
        if multiline:
            # Up to two quotes before the delimiter end the string's text.
            while end < len(self.text) and self.text[end] == quote:
                end += 1
            if end - self.index > 5:
                raise self.error("unterminated string")
            parts.append(quote * (end - self.index - 3))
        self.index = end
        return "".join(parts)

    def escape(self, multiline: bool) -> str:
        if multiline:
            match = _LINE_CONTINUATION.match(self.text, self.index)
            if match is not None:
                self.index = match.end()
                return ""
        code = self.text[self.index + 1 : self.index + 2]
        if code in _ESCAPES:
            self.index += 2
            return _ESCAPES[code]
        width = {"x": 2, "u": 4, "U": 8}.get(code, 0)
        digits = self.text[self.index + 2 : self.index + 2 + width]
        if width and len(digits) == width and _HEX_DIGITS.fullmatch(digits):
            point = int(digits, 16)
            if point <= 0x10FFFF and not 0xD800 <= point <= 0xDFFF:
                self.index += 2 + width
                return chr(point)
        raise self.error("invalid escape sequence")

    def array(self) -> list[object]:
        self.index += 1
        values: list[object] = []
        while True:
            self.skip(newlines=True)
            if self.at("]"):
                self.index += 1
                self.closed.add(id(values))
                return values
            values.append(self.value())
            self.skip(newlines=True)
            if self.at(","):
                self.index += 1
            elif not self.at("]"):
                raise self.error("expected ',' or ']'")

    def inline_table(self) -> Table:
        # Line breaks and comments may come between its pairs, and a comma
        # after the last one.
        self.index += 1
        table: Table = {}
        while True:
            self.skip(newlines=True)
            if self.at("}"):
                self.index += 1
                self.closed.add(id(table))
                return table
            self.pair(table, newlines=True)
            self.skip(newlines=True)
            if self.at(","):
                self.index += 1
            elif not self.at("}"):
                raise self.error("expected ',' or '}'")


@dataclass(frozen=True)
class LeapSecond:
    """A date and time, or a time, in a leap second, which TOML allows and
    Python's times cannot hold: `moment` is the one with second 59 in place of
    60, with the same fraction and offset. It equals only the same leap second,
    and has no order or arithmetic, which would treat it as second 59."""

    moment: datetime.datetime | datetime.time


def _moment(match: re.Match[str]) -> datetime.date | datetime.time | LeapSecond:
    """Return the date, date and time, or time that a match of _DATE_TIME
    holds, with at most microseconds and its offset if it has one, or the
    LeapSecond of one in a leap second."""
    year, month, day, *clock, utc, sign, hours, minutes = match.groups()[:11]
    hour, minute, second, fraction = clock if year else match.groups()[11:]
    seconds = 59 if second == "60" else int(second or 0)
    microsecond = int((fraction or "0")[:6].ljust(6, "0"))
    zone = datetime.timezone.utc if utc else None
    if sign:
        offset = datetime.timedelta(hours=int(hours), minutes=int(minutes))
        zone = datetime.timezone(-offset if sign == "-" else offset)
    moment: datetime.datetime | datetime.time
    if not year:
        moment = datetime.time(int(hour), int(minute), seconds, microsecond)
    else:
        date = datetime.date(int(year), int(month), int(day))
        if not hour:
            return date
        time = datetime.time(int(hour), int(minute), seconds, microsecond, zone)
        moment = datetime.datetime.combine(date, time)
    return LeapSecond(moment) if second == "60" else moment


def read_toml(data: bytes | str, source: str) -> Table:
    """Return the TOML document `data`, which errors name as `source`. A byte
    order mark may begin it, as Cargo allows."""
    if isinstance(data, bytes):
        try:
            data = data.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ScriptError(f"{source} is not UTF-8: {error}") from error
    return _Reader(data.removeprefix("\ufeff"), source).document()


def read_toml_file(path: Path) -> Table:
    try:
        data = path.read_bytes()
    except OSError as error:
        raise ScriptError(f"cannot read {path}: {error}") from error
    return read_toml(data, str(path))


def table(value: object) -> Table:
    """Return `value` if it is a table, and an empty table otherwise."""
    return cast(Table, value) if isinstance(value, dict) else {}


def _normalized(path: Path, checkout: Path) -> Path:
    """Return `path` without `..` components, which must lie in `checkout`."""
    normalized = Path(os.path.normpath(path))
    if normalized != checkout and checkout not in normalized.parents:
        raise ScriptError(f"{normalized} is outside the checkout {checkout}")
    return normalized


def _checkout(path: Path) -> Path:
    return Path(os.path.normpath(path))


def _ancestors(directory: Path, checkout: Path) -> list[Path]:
    """Return the directories above `directory` up to the checkout's root,
    the nearest first."""
    parts = directory.relative_to(checkout).parts
    return [checkout.joinpath(*parts[:depth]) for depth in reversed(range(len(parts)))]


def workspace_root(package: Path, checkout: Path) -> Path:
    """Return the root of the workspace of the package at `package`, a
    directory of the checkout at `checkout`, where Cargo keeps the workspace's
    lockfile: the package itself when its manifest declares a workspace, the
    root that its manifest names, or the nearest directory above it in the
    checkout whose manifest declares a workspace that does not exclude it; and
    otherwise the package itself, a package alone in its workspace."""
    checkout = _checkout(checkout)
    package = _normalized(package, checkout)
    manifest = read_toml_file(package / MANIFEST)
    if "workspace" in manifest:
        return package
    named = table(manifest.get("package")).get("workspace")
    if isinstance(named, str):
        return _normalized(package / named, checkout)
    for directory in _ancestors(package, checkout):
        path = directory / MANIFEST
        workspace = read_toml_file(path).get("workspace") if path.is_file() else None
        if not isinstance(workspace, dict):
            continue
        relative = package.relative_to(directory)
        excluded = cast(Table, workspace).get("exclude")
        entries = cast(list[object], excluded) if isinstance(excluded, list) else []
        if any(
            Path(entry) == relative or Path(entry) in relative.parents
            for entry in entries
            if isinstance(entry, str)
        ):
            return package
        return directory
    return package


def configuration_files(directory: Path, checkout: Path) -> tuple[Path, ...]:
    """Return the Cargo configuration files of the checkout at `checkout` that
    Cargo reads when it runs in `directory`, a directory of the checkout: the
    configuration of `directory` and of each directory above it in the
    checkout, the nearest first."""
    checkout = _checkout(checkout)
    directory = _normalized(directory, checkout)
    files: list[Path] = []
    for parent in (directory, *_ancestors(directory, checkout)):
        present = [parent / name for name in CONFIGURATION if (parent / name).is_file()]
        files += present[:1]
    return tuple(files)


def dependency_tables(
    manifest: Table, keys: tuple[str, ...] = DEPENDENCIES
) -> Iterator[Table]:
    """Yield the dependency tables `keys` of `manifest` and of its
    `target.<platform>` tables, an empty table for each that it lacks."""
    for platform in (manifest, *map(table, table(manifest.get("target")).values())):
        for key in keys:
            yield table(platform.get(key))


def local_packages(package: Path, checkout: Path) -> tuple[Path, ...]:
    """Return the package at `package`, a directory of the checkout at
    `checkout`, and the directory of each package in the checkout that its
    build uses: its path dependencies, including those that it inherits from
    the workspace that holds it, the path patches and replacements of its
    workspace, and the path patches and overrides of the Cargo configuration
    that its build reads when Cargo runs in the checkout's root, and theirs."""
    checkout = _checkout(checkout)
    package = _normalized(package, checkout)
    root = workspace_root(package, checkout)
    pending = [package]
    # A configuration file's paths are relative to the directory that holds
    # its .cargo directory.
    configurations = [
        (path.parent.parent, read_toml_file(path))
        for path in configuration_files(checkout, checkout)
    ]
    for base, document in configurations:
        overrides = document.get("paths")
        overrides = cast(list[object], overrides) if isinstance(overrides, list) else []
        pending += [base / path for path in overrides if isinstance(path, str)]
    # A patch of the configuration replaces the workspace's of the same source
    # and name, and one of a nearer configuration that of a farther one.
    patched: dict[tuple[str, str], Path | None] = {}
    manifest = (root, read_toml_file(root / MANIFEST))
    for base, document in (manifest, *reversed(configurations)):
        for source, patches in table(document.get("patch")).items():
            for name, patch in table(patches).items():
                path = table(patch).get("path")
                patched[source, name] = base / path if isinstance(path, str) else None
    pending += [path for path in patched.values() if path is not None]
    for replacement in table(manifest[1].get("replace")).values():
        path = table(replacement).get("path")
        if isinstance(path, str):
            pending.append(root / path)
    found: dict[Path, None] = {}
    while pending:
        current = _normalized(pending.pop(), checkout)
        if current in found:
            continue
        found[current] = None
        own = workspace_root(current, checkout)
        workspace = table(read_toml_file(own / MANIFEST).get("workspace"))
        inherited = table(workspace.get("dependencies"))
        for dependencies in dependency_tables(
            read_toml_file(current / MANIFEST), BUILD_DEPENDENCIES
        ):
            for name, value in dependencies.items():
                base, specification = current, table(value)
                if specification.get("workspace") is True:
                    base, specification = own, table(inherited.get(name))
                path = specification.get("path")
                if isinstance(path, str):
                    pending.append(base / path)
    return tuple(found)


def build_script(package: Path, checkout: Path) -> Path | None:
    """Return the build script of the package at `package`: the one that its
    manifest names, or else its build.rs, unless its manifest disables it,
    without `..` components, which must lie in the checkout at `checkout`;
    None when it has none."""
    manifest = read_toml_file(package / MANIFEST)
    name = table(manifest.get("package")).get("build", True)
    if name is True:
        name = "build.rs"
    if isinstance(name, str) and (package / name).is_file():
        return _normalized(package / name, _checkout(checkout))
    return None


def root_files(package: Path) -> tuple[Path, ...]:
    """Return the root files that the manifest of the package at `package`
    names for its library and binaries, without `..` components. Cargo finds
    the others in src/."""
    manifest = read_toml_file(package / MANIFEST)
    binaries = manifest.get("bin")
    targets = [manifest.get("lib")]
    targets += cast(list[object], binaries) if isinstance(binaries, list) else []
    paths = [table(target).get("path") for target in targets]
    return tuple(
        Path(os.path.normpath(package / path))
        for path in paths
        if isinstance(path, str)
    )

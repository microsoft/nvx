#!/usr/bin/env python3
"""Check the public API of the nvx crate against its snapshots.

The snapshots, nvx-rs/public-api/linux.txt and windows.txt, list the items that
nvx exposes on Linux and on Windows, one a line: the item's kind, its path, and
`(hidden)` if `#[doc(hidden)]` hides it or an item that contains it. The lines
are sorted by path, so that a change to a snapshot reads like the change to the
API. A pull request that changes the public API updates the snapshots:

    python3 scripts/public_api.py --update

Without `--update`, the script prints how the API differs from each snapshot,
and fails if it does.

The script builds the library's rustdoc JSON with every feature but `bundled`,
whose build script downloads a release, for x86_64-unknown-linux-musl, the
guests' target, and for x86_64-pc-windows-msvc, on the Rust release that
rust-toolchain.toml pins. It first adds both targets to that release with
rustup: rustdoc needs only a target's standard library, not its linker, so any
host lists both platforms, and lists them alike. The JSON output is unstable,
so the script enables it with RUSTC_BOOTSTRAP=1, and reads only the format
version of the pinned release.

It walks the public tree from the crate root, following re-exports, and lists
the modules, structs, enums, unions, traits and their items, functions,
constants, statics, type aliases, macros, public fields, variants, and the
methods, constants, and types of inherent impls. It leaves trait
implementations out. It refuses what it cannot list: a glob re-export, a
re-export of an item of another crate, and a re-export of a module that
contains it.
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

from nvx_tools.build_constants import (
    BuildConstants,
    GuestRustBuildConstants,
    OpenVMMBuildConstants,
)
from nvx_tools.common import ScriptError, require_success, run_capture, run_checked
from nvx_tools.guest_binaries import pinned_guest_rust_toolchain

CRATE = "nvx"
CRATE_DIR = GuestRustBuildConstants.DIRECTORY
SNAPSHOTS = CRATE_DIR / "public-api"
UPDATE_COMMAND = "python3 scripts/public_api.py --update"

# The rustdoc JSON format of the Rust release that rust-toolchain.toml pins.
FORMAT_VERSION = 57

# The snapshot of each platform, and the target that rustdoc documents for it:
# on Linux, the guests' target, for which the guest module builds.
TARGETS = {
    "linux": GuestRustBuildConstants.TARGET,
    "windows": OpenVMMBuildConstants.WINDOWS_RUST_TARGET,
}

# `default` names other features, and the build script of `bundled` downloads
# a release.
EXCLUDED_FEATURES = frozenset({"bundled", "default"})

# How rustdoc's JSON gives the attribute `#[doc(hidden)]`.
HIDDEN = {"other": "#[doc(hidden)]"}

# The kind that a line names for each kind of item in a module, and for each
# kind of item in a trait or an inherent impl.
MODULE_ITEMS = {
    "module": "mod",
    "struct": "struct",
    "enum": "enum",
    "union": "union",
    "trait": "trait",
    "function": "fn",
    "constant": "const",
    "static": "static",
    "type_alias": "type",
    "macro": "macro",
}
ASSOCIATED_ITEMS = {
    "function": "method",
    "assoc_const": "const",
    "assoc_type": "type",
}


class Lister:
    """Lists the public items of a crate's rustdoc JSON."""

    def __init__(self, document: Any) -> None:
        version = document.get("format_version")
        if version != FORMAT_VERSION:
            raise ScriptError(
                f"the rustdoc JSON has format version {version}, but this script "
                f"reads only version {FORMAT_VERSION}, that of the Rust release "
                "that rust-toolchain.toml pins"
            )
        self.index: dict[str, Any] = document["index"]
        self.items: set[tuple[str, str, bool]] = set()

    def get(self, item_id: Any) -> Any:
        item = self.index.get(str(item_id))
        if item is None:
            raise ScriptError(f"the rustdoc JSON has no item {item_id}")
        return item

    def add(self, path: str, kind: str, hidden: bool) -> None:
        self.items.add((path, kind, hidden))

    def item(
        self,
        item: Any,
        parent: str,
        name: str | None,
        hidden: bool,
        modules: frozenset[int],
    ) -> None:
        """Lists `item`, and what it holds, as `parent`'s item `name`.

        `name` is that of a re-export, or `None` for the item's own. `hidden`
        tells whether an item that contains it is hidden, and `modules` holds
        the modules that contain it.
        """
        [(kind, inner)] = item["inner"].items()
        hidden = hidden or HIDDEN in item["attrs"]
        if kind == "use":
            path = f"{parent}::{inner['name']}"
            if inner["is_glob"]:
                raise ScriptError(
                    f"{parent} re-exports everything of {inner['source']}, which "
                    "this script cannot list"
                )
            if str(inner["id"]) not in self.index:
                raise ScriptError(
                    f"{path} re-exports {inner['source']}, which is not an item "
                    f"of {CRATE}"
                )
            self.item(self.get(inner["id"]), parent, inner["name"], hidden, modules)
            return
        own = name or item["name"]
        path = f"{parent}::{own}" if parent else own
        if kind == "module":
            if item["id"] in modules:
                raise ScriptError(f"{path} re-exports a module that contains it")
            inside = modules | {item["id"]}
            self.add(path, "mod", hidden)
            for child in inner["items"]:
                self.item(self.get(child), path, None, hidden, inside)
        elif kind in ("struct", "union"):
            self.add(path, kind, hidden)
            self.fields(inner, path, hidden)
            self.impls(inner["impls"], path, hidden)
        elif kind == "enum":
            self.add(path, kind, hidden)
            for variant_id in inner["variants"]:
                variant = self.get(variant_id)
                variant_path = f"{path}::{variant['name']}"
                variant_hidden = hidden or HIDDEN in variant["attrs"]
                self.add(variant_path, "variant", variant_hidden)
                self.fields(variant["inner"]["variant"], variant_path, variant_hidden)
            self.impls(inner["impls"], path, hidden)
        elif kind == "trait":
            self.add(path, kind, hidden)
            self.associated(inner["items"], path, hidden)
        elif kind in MODULE_ITEMS:
            self.add(path, MODULE_ITEMS[kind], hidden)
        else:
            raise ScriptError(
                f"{path} is of kind {kind}, which this script cannot list"
            )

    def fields(self, inner: Any, path: str, hidden: bool) -> None:
        """Lists the fields of a struct, a union, or a variant.

        rustdoc gives only the public fields of a struct, and a null in place
        of each other field of a tuple struct.
        """
        ids: Sequence[Any] = []
        if "fields" in inner:
            ids = inner["fields"]
        elif not isinstance(inner["kind"], str):
            [(form, layout)] = inner["kind"].items()
            ids = layout if form == "tuple" else layout["fields"]
        for field_id in ids:
            if field_id is not None:
                field = self.get(field_id)
                field_hidden = hidden or HIDDEN in field["attrs"]
                self.add(f"{path}::{field['name']}", "field", field_hidden)

    def impls(self, ids: Sequence[Any], path: str, hidden: bool) -> None:
        """Lists the items of the inherent impls among the impls `ids`."""
        for impl_id in ids:
            impl = self.get(impl_id)
            inner = impl["inner"]["impl"]
            if inner["trait"] is None:
                impl_hidden = hidden or HIDDEN in impl["attrs"]
                self.associated(inner["items"], path, impl_hidden)

    def associated(self, ids: Sequence[Any], path: str, hidden: bool) -> None:
        """Lists the items of a trait or an inherent impl."""
        for item_id in ids:
            item = self.get(item_id)
            [kind] = item["inner"]
            item_path = f"{path}::{item['name']}"
            if kind not in ASSOCIATED_ITEMS:
                raise ScriptError(
                    f"{item_path} is of kind {kind}, which this script cannot list"
                )
            item_hidden = hidden or HIDDEN in item["attrs"]
            self.add(item_path, ASSOCIATED_ITEMS[kind], item_hidden)


def api_lines(document: Any) -> list[str]:
    """Returns the lines that list the public API of a crate's rustdoc JSON."""
    lister = Lister(document)
    lister.item(lister.get(document["root"]), "", None, False, frozenset())
    return [
        f"{kind} {path}{' (hidden)' if hidden else ''}"
        for path, kind, hidden in sorted(
            lister.items, key=lambda item: (item[0].split("::"), item[1], item[2])
        )
    ]


def selected_features(features: Mapping[str, object]) -> list[str]:
    """Returns the features to document: every one but `bundled`."""
    return sorted(set(features) - EXCLUDED_FEATURES)


def platform_lines() -> dict[str, list[str]]:
    """Builds the rustdoc JSON of each platform, and lists its public API."""
    toolchain = pinned_guest_rust_toolchain()
    run_checked(
        ["rustup", "target", "add", "--toolchain", toolchain, *TARGETS.values()],
        cwd=CRATE_DIR,
    )
    cargo = ["cargo", f"+{toolchain}"]
    result = run_capture(
        [*cargo, "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=CRATE_DIR,
    )
    require_success(result, "cargo metadata")
    metadata = json.loads(result.stdout)
    [package] = metadata["packages"]
    features = ",".join(selected_features(package["features"]))
    environment = {**os.environ, "RUSTC_BOOTSTRAP": "1"}
    listings: dict[str, list[str]] = {}
    for platform, target in TARGETS.items():
        run_checked(
            [
                *cargo,
                "rustdoc",
                "--locked",
                "--lib",
                "--features",
                features,
                "--target",
                target,
                "--",
                "-Z",
                "unstable-options",
                "--output-format",
                "json",
                "--document-hidden-items",
            ],
            cwd=CRATE_DIR,
            env=environment,
        )
        output = Path(metadata["target_directory"]) / target / "doc" / f"{CRATE}.json"
        listings[platform] = api_lines(json.loads(output.read_text(encoding="utf-8")))
    return listings


def shown(path: Path) -> str:
    """Returns `path` relative to the repository, if it is in it."""
    try:
        return path.relative_to(BuildConstants.REPO_ROOT).as_posix()
    except ValueError:
        return str(path)


def compare(listings: Mapping[str, list[str]], directory: Path, update: bool) -> int:
    """Checks the snapshots in `directory` against `listings`, or rewrites them.

    Prints how each snapshot differs, and returns the exit status: 1 if one
    differs and `update` is false, 0 otherwise.
    """
    stale: list[str] = []
    for platform, lines in listings.items():
        path = directory / f"{platform}.txt"
        text = "".join(f"{line}\n" for line in lines)
        current = path.read_text(encoding="utf-8") if path.is_file() else ""
        if current == text:
            continue
        if update:
            directory.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8", newline="\n")
            print(f"updated {shown(path)}")
            continue
        stale.append(shown(path))
        sys.stdout.writelines(
            difflib.unified_diff(
                current.splitlines(keepends=True),
                text.splitlines(keepends=True),
                shown(path),
                f"the public API on {platform}",
            )
        )
    if stale:
        print(
            f"error: the public API differs from {' and '.join(stale)}; if the "
            f"change is intended, run `{UPDATE_COMMAND}`, and commit the snapshots",
            file=sys.stderr,
        )
        return 1
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=f"Check the public API of {CRATE} against its snapshots."
    )
    parser.add_argument(
        "--update",
        action="store_true",
        help=f"rewrite {shown(SNAPSHOTS)}/ to list the current public API",
    )
    args = parser.parse_args(argv)
    try:
        listings = platform_lines()
    except ScriptError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    return compare(listings, SNAPSHOTS, args.update)


if __name__ == "__main__":
    sys.exit(main())

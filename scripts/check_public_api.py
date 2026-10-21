#!/usr/bin/env python3
"""Check the public API of nvx that adapters build on against its snapshots.

An adapter builds on nvx in one of three configurations: protocol-only,
without features, host-only, with `host`, and guest-only, with `guest`.
tests/public-api records the public API of each configuration on each of its
targets, as the pinned cargo-public-api lists it
from the rustdoc JSON of the pinned nightly toolchain, followed by the defaults
of generic parameters, which cargo-public-api leaves out and the check reads
from the same rustdoc JSON. NAME.txt holds the items
that every target of configuration NAME lists, and NAME.TARGET.txt, present
when TARGET lists items that another target doesn't, those items. The check
fails when the snapshots differ from the listings, which `--update` writes to
them instead, and in GitHub Actions it reports the size of each listing in the
job summary.

`--install-tools` installs the pinned nightly toolchain, with the targets of
the configurations, and the pinned cargo-public-api.
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
import shlex
import subprocess
import sys
from collections import Counter
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast

from nvx_tools.build_constants import GuestRustBuildConstants, OpenVMMBuildConstants

REPO_ROOT = Path(__file__).resolve().parents[1]
MANIFEST = REPO_ROOT / "nvx-rs" / "Cargo.toml"
SNAPSHOTS = REPO_ROOT / "tests" / "public-api"
COMMAND = "python3 scripts/check_public_api.py"
# The release of cargo-public-api, and the nightly toolchain that produces the
# rustdoc JSON that it reads: the oldest one that the release's own tests run
# with. Move them together, and update the snapshots, whose format may change,
# in the same commit.
CARGO_PUBLIC_API_VERSION = "0.52.0"
NIGHTLY_TOOLCHAIN = "nightly-2026-02-16"
LINUX_TARGET = OpenVMMBuildConstants.GNU_RUST_TARGET
WINDOWS_TARGET = OpenVMMBuildConstants.WINDOWS_RUST_TARGET
# The target of the guest binaries, which guests run.
GUEST_TARGET = GuestRustBuildConstants.TARGET


class CheckError(Exception):
    """A failure to list the public API, rather than an API that changed."""


@dataclass(frozen=True)
class Configuration:
    """An adapter-facing configuration of nvx and the targets that list it."""

    name: str
    features: tuple[str, ...]
    targets: tuple[str, ...]


@dataclass(frozen=True)
class Snapshot:
    """The text that a snapshot must hold, and the part of the API it records."""

    path: Path
    text: str
    subject: str


@dataclass(frozen=True)
class Surface:
    """The number of items that a configuration lists on a target."""

    configuration: Configuration
    target: str
    items: int


# Adapters run the host side on Linux and Windows hosts, and the guest side in
# Linux guests.
CONFIGURATIONS = (
    Configuration("protocol", (), (LINUX_TARGET, WINDOWS_TARGET)),
    Configuration("host", ("host",), (LINUX_TARGET, WINDOWS_TARGET)),
    Configuration("guest", ("guest",), (GUEST_TARGET,)),
)


def relative(path: Path) -> str:
    """Return `path` relative to the repository, with forward slashes."""
    return path.relative_to(REPO_ROOT).as_posix()


def install_hint() -> str:
    return f"install them with `{COMMAND} --install-tools`"


def targets() -> list[str]:
    """Return every target of a configuration, in a stable order."""
    return sorted({target for item in CONFIGURATIONS for target in item.targets})


def listing_command(configuration: Configuration, target: str) -> list[str]:
    """Return the command that lists the API of `configuration` for `target`."""
    command = [
        "cargo",
        f"+{NIGHTLY_TOOLCHAIN}",
        "public-api",
        "--manifest-path",
        os.fspath(MANIFEST),
        "--color",
        "never",
        # Blanket implementations, such as `impl<T, U> Into<U> for T`, come
        # from the standard library. Auto-trait and derived implementations
        # stay, because adapters rely on them.
        "--omit",
        "blanket-impls",
        "--no-default-features",
        "--target",
        target,
    ]
    if configuration.features:
        command.extend(("--features", ",".join(configuration.features)))
    return command


def run(command: Sequence[str]) -> str:
    """Run `command` in the repository and return its standard output."""
    try:
        result = subprocess.run(
            command,
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
        )
    except OSError as error:
        raise CheckError(f"cannot run {command[0]}: {error}") from error
    if result.returncode != 0:
        raise CheckError(
            f"{shlex.join(command)} failed with exit status {result.returncode}; "
            f"if a pinned tool or target is missing, {install_hint()}:\n"
            f"{result.stderr.rstrip()}"
        )
    return result.stdout


def require_tools() -> None:
    """Fail unless Cargo runs the pinned cargo-public-api."""
    expected = f"cargo-public-api {CARGO_PUBLIC_API_VERSION}"
    try:
        version = run(["cargo", "public-api", "--version"]).strip()
    except CheckError as error:
        raise CheckError(f"{expected} is not installed: {error}") from error
    if version != expected:
        raise CheckError(f"found {version} instead of {expected}; {install_hint()}")


def install_tools(root: Path | None) -> None:
    """Install the pinned nightly toolchain, its targets, and cargo-public-api."""
    install = [
        "cargo",
        "install",
        "--locked",
        "cargo-public-api",
        "--version",
        CARGO_PUBLIC_API_VERSION,
    ]
    if root is not None:
        install.extend(("--root", os.fspath(root)))
    for command in (
        ["rustup", "toolchain", "install", NIGHTLY_TOOLCHAIN, "--profile", "minimal"],
        ["rustup", "target", "add", "--toolchain", NIGHTLY_TOOLCHAIN, *targets()],
        install,
    ):
        print(f">> {shlex.join(command)}", flush=True)
        try:
            returncode = subprocess.run(command, cwd=REPO_ROOT, check=False).returncode
        except OSError as error:
            raise CheckError(f"cannot run {command[0]}: {error}") from error
        if returncode != 0:
            raise CheckError(
                f"{shlex.join(command)} failed with exit status {returncode}"
            )


def partition(
    items: Sequence[str], shared: Counter[str]
) -> tuple[list[str], list[str]]:
    """Split `items`, in order, into those that `shared` counts and the others."""
    remaining = Counter(shared)
    common: list[str] = []
    specific: list[str] = []
    for item in items:
        if remaining[item] > 0:
            remaining[item] -= 1
            common.append(item)
        else:
            specific.append(item)
    return common, specific


def lines(items: Iterable[str]) -> str:
    return "".join(f"{item}\n" for item in items)


def snapshots(
    configuration: Configuration, listings: Mapping[str, str]
) -> list[Snapshot]:
    """Return the snapshots of `configuration`, given the listing of each of its
    targets: the items that every target lists, in the order of the first
    target's listing, then, for each target that lists more, those items."""
    items = {target: listings[target].splitlines() for target in configuration.targets}
    first, *others = configuration.targets
    shared = Counter(items[first])
    for target in others:
        shared &= Counter(items[target])
    subject = f"the {configuration.name} API of nvx"
    common, _ = partition(items[first], shared)
    result = [Snapshot(SNAPSHOTS / f"{configuration.name}.txt", lines(common), subject)]
    for target in configuration.targets:
        _, specific = partition(items[target], shared)
        if specific:
            result.append(
                Snapshot(
                    SNAPSHOTS / f"{configuration.name}.{target}.txt",
                    lines(specific),
                    f"{subject} specific to {target}",
                )
            )
    return result


def unified_diff(old: str, new: str, old_name: str, new_name: str) -> Iterable[str]:
    return difflib.unified_diff(
        old.splitlines(keepends=True),
        new.splitlines(keepends=True),
        fromfile=old_name,
        tofile=new_name,
    )


def rustdoc_document(target: str) -> dict[str, Any]:
    """Return the rustdoc JSON that the last listing for `target` produced."""
    try:
        metadata = json.loads(
            run(
                [
                    "cargo",
                    "metadata",
                    "--format-version",
                    "1",
                    "--no-deps",
                    "--manifest-path",
                    os.fspath(MANIFEST),
                ]
            )
        )
        package = next(
            item
            for item in metadata["packages"]
            if Path(item["manifest_path"]).resolve() == MANIFEST.resolve()
        )
        library = next(
            item["name"].replace("-", "_")
            for item in package["targets"]
            if "lib" in item["kind"]
        )
        path = Path(metadata["target_directory"]) / target / "doc" / f"{library}.json"
        return cast(dict[str, Any], json.loads(path.read_text(encoding="utf-8")))
    except (OSError, ValueError, KeyError, StopIteration) as error:
        raise CheckError(
            f"cannot read the rustdoc JSON for {target}: {error}"
        ) from error


def public_paths(document: Mapping[str, Any]) -> dict[str, list[str]]:
    """Return the public paths of each item that the crate's root reaches
    through its public modules and re-exports, by item ID."""
    index: Mapping[str, Any] = document["index"]
    paths: dict[str, list[str]] = {}

    def visit(module: str, prefix: list[str], seen: frozenset[str]) -> None:
        if module in seen:
            return
        seen = seen | {module}
        for child in index[module]["inner"]["module"]["items"]:
            item = index.get(str(child))
            if item is None:
                continue
            inner = item["inner"]
            if "use" in inner:
                target = inner["use"].get("id")
                if target is None or str(target) not in index:
                    continue
                target = str(target)
                is_module = "module" in index[target]["inner"]
                if inner["use"]["is_glob"]:
                    if is_module:
                        visit(target, prefix, seen)
                    continue
                path = [*prefix, inner["use"]["name"]]
            else:
                target = str(child)
                is_module = "module" in inner
                path = [*prefix, item["name"]]
            paths.setdefault(target, []).append("::".join(path))
            if is_module:
                visit(target, path, seen)

    root = str(document["root"])
    name = index[root]["name"]
    paths[root] = [name]
    visit(root, [name], frozenset())
    return paths


def resolve_ids(value: Any, names: Mapping[str, str]) -> Any:
    """Return `value` with each item ID, which changes between builds, replaced
    by the qualified path of the item, in `names`, or `null` if it has none."""
    if isinstance(value, dict):
        entries = cast(dict[str, Any], value)
        return {
            key: names.get(str(item)) if key == "id" else resolve_ids(item, names)
            for key, item in entries.items()
        }
    if isinstance(value, list):
        return [resolve_ids(item, names) for item in cast(list[Any], value)]
    return value


def item_names(document: Mapping[str, Any]) -> dict[str, str]:
    """Return the qualified path of each item that the rustdoc JSON names, by
    ID: the first public path of an item of the crate, or the path that rustdoc
    records for any other."""
    names = {
        identifier: "::".join(summary["path"])
        for identifier, summary in cast(
            Mapping[str, Any], document.get("paths", {})
        ).items()
    }
    names.update(
        (identifier, min(paths)) for identifier, paths in public_paths(document).items()
    )
    return names


def as_text(value: Any, names: Mapping[str, str]) -> str:
    """Return `value`, from the rustdoc JSON, as compact JSON with its item IDs
    resolved, which change between builds."""
    return json.dumps(resolve_ids(value, names), sort_keys=True, separators=(",", ":"))


def generic_defaults(document: Mapping[str, Any]) -> list[str]:
    """Return a line for each default of a generic parameter of a public item.

    cargo-public-api leaves these defaults out, as in `pub type nvx::Result<T,
    E>` for `Result<T, E = Error>`, so the check records them from the rustdoc
    JSON, with each default type as its JSON in which each item ID is replaced
    by the qualified path of the item."""
    index: Mapping[str, Any] = document["index"]
    names = item_names(document)
    found: set[str] = set()
    for identifier, paths in public_paths(document).items():
        inner: Mapping[str, Any] = index[identifier]["inner"]
        for body in inner.values():
            if not isinstance(body, dict):
                continue
            generics = cast(dict[str, Any], body).get("generics")
            if not generics:
                continue
            parameters: list[dict[str, Any]] = generics["params"]
            for parameter in parameters:
                kind: dict[str, Any] = parameter["kind"]
                default: Any = None
                for key in ("type", "const"):
                    if key in kind and kind[key].get("default") is not None:
                        default = kind[key]["default"]
                if default is None:
                    continue
                text = as_text(default, names)
                found.update(
                    f"generic default {path}<{parameter['name']}> = {text}"
                    for path in paths
                )
    return sorted(found)


def trait_item_defaults(document: Mapping[str, Any]) -> list[str]:
    """Return a line for each item of a public trait that says whether the
    trait provides it: a method with a body, an associated constant with a
    value, or an associated type with a default.

    cargo-public-api renders the same line for a required and a provided item,
    but an implementation that leaves out an item stops compiling when the
    trait no longer provides it."""
    index: Mapping[str, Any] = document["index"]
    names = item_names(document)
    found: set[str] = set()
    for identifier, paths in public_paths(document).items():
        trait = index[identifier]["inner"].get("trait")
        if not isinstance(trait, dict):
            continue
        members: list[Any] = cast(dict[str, Any], trait)["items"]
        for member in members:
            item = index.get(str(member))
            if item is None:
                continue
            inner: Mapping[str, Any] = item["inner"]
            if "function" in inner:
                kind = "method"
                provided = bool(inner["function"].get("has_body"))
                default = None
            elif "assoc_const" in inner:
                kind = "constant"
                default = inner["assoc_const"].get("value")
                provided = default is not None
            elif "assoc_type" in inner:
                kind = "type"
                default = inner["assoc_type"].get("type")
                provided = default is not None
            else:
                continue
            status = "provided" if provided else "required"
            if default is not None:
                status += f" = {as_text(default, names)}"
            found.update(
                f"trait {kind} {path}::{item['name']} is {status}" for path in paths
            )
    return sorted(found)


def listing(configuration: Configuration, target: str) -> str:
    """Return the public API of `configuration` on `target`: the items that
    cargo-public-api lists, then what it leaves out, the defaults of generic
    parameters and which trait items their traits provide."""
    items = run(listing_command(configuration, target))
    document = rustdoc_document(target)
    # The rustdoc JSON is the one that cargo-public-api just read only if it
    # reaches the modules that the listing names.
    listed = {
        line.removeprefix("pub mod ")
        for line in items.splitlines()
        if line.startswith("pub mod ")
    }
    reached = {
        path
        for identifier, paths in public_paths(document).items()
        if "module" in document["index"][identifier]["inner"]
        for path in paths
    }
    if listed != reached:
        raise CheckError(
            f"the rustdoc JSON for {target} does not match the {configuration.name} "
            f"listing: its modules differ by {sorted(listed ^ reached)}"
        )
    return items + lines(generic_defaults(document) + trait_item_defaults(document))


def check(update: bool) -> tuple[list[Surface], list[str]]:
    """List each configuration on each of its targets, then compare the
    snapshots with the listings, or update them. Return the size of each
    listing, and the problems, each a path and a message."""
    require_tools()
    surfaces: list[Surface] = []
    expected: list[Snapshot] = []
    for configuration in CONFIGURATIONS:
        listings = {
            target: listing(configuration, target) for target in configuration.targets
        }
        surfaces.extend(
            Surface(configuration, target, len(listing.splitlines()))
            for target, listing in listings.items()
        )
        expected.extend(snapshots(configuration, listings))
    problems: list[str] = []
    for snapshot in expected:
        path = snapshot.path
        recorded = path.read_text(encoding="utf-8") if path.is_file() else None
        if recorded == snapshot.text:
            continue
        if update:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(snapshot.text, encoding="utf-8", newline="\n")
            print(f"updated {relative(path)}")
            continue
        sys.stdout.writelines(
            unified_diff(
                recorded or "", snapshot.text, relative(path), snapshot.subject
            )
        )
        problems.append(
            f"{relative(path)}: {snapshot.subject} "
            + ("has no snapshot" if recorded is None else "differs from this snapshot")
        )
    paths = {snapshot.path for snapshot in expected}
    for path in sorted(SNAPSHOTS.glob("*.txt")):
        if path in paths:
            continue
        if update:
            path.unlink()
            print(f"removed {relative(path)}")
        else:
            problems.append(f"{relative(path)}: no listing has items for this snapshot")
    return surfaces, problems


def summary(surfaces: Sequence[Surface], problems: Sequence[str]) -> str:
    """Return the Markdown job summary of the listings and the check."""
    rows = [
        "### Public API of nvx",
        "",
        "| Configuration | Features | Target | Items |",
        "| --- | --- | --- | --- |",
    ]
    for surface in surfaces:
        configuration = surface.configuration
        features = ", ".join(f"`{name}`" for name in configuration.features)
        rows.append(
            f"| {configuration.name} | {features or 'none'} | `{surface.target}` "
            f"| {surface.items} |"
        )
    rows.append("")
    rows.append(
        "The snapshots in `tests/public-api` differ from these listings; the log "
        "shows how."
        if problems
        else "The snapshots in `tests/public-api` match these listings."
    )
    return "\n".join(rows) + "\n"


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Check the public API of nvx that adapters build on "
        "against the snapshots in tests/public-api."
    )
    action = parser.add_mutually_exclusive_group()
    action.add_argument(
        "--update",
        action="store_true",
        help="write the listings to the snapshots and remove obsolete snapshots, "
        "to commit with the change",
    )
    action.add_argument(
        "--install-tools",
        action="store_true",
        help=f"install {NIGHTLY_TOOLCHAIN} with the targets of the configurations, "
        f"and cargo-public-api {CARGO_PUBLIC_API_VERSION}",
    )
    parser.add_argument(
        "--root",
        type=Path,
        help="with --install-tools, the installation root of cargo-public-api "
        "(default: Cargo's)",
    )
    args = parser.parse_args(argv)
    if args.root is not None and not args.install_tools:
        parser.error("--root requires --install-tools")
    try:
        if args.install_tools:
            install_tools(args.root)
            return 0
        surfaces, problems = check(args.update)
    except CheckError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    for surface in surfaces:
        print(
            f"the {surface.configuration.name} API of nvx lists "
            f"{surface.items} items on {surface.target}"
        )
    job_summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if job_summary:
        with open(job_summary, "a", encoding="utf-8") as stream:
            stream.write(summary(surfaces, problems))
    annotate = os.environ.get("GITHUB_ACTIONS") == "true"
    for problem in problems:
        path, message = problem.split(": ", 1)
        print(f"::error file={path}::{message}" if annotate else problem)
    sys.stdout.flush()
    if problems:
        print(
            "error: the public API of nvx changed. If the change is intended, run "
            f"`{COMMAND} --update`, commit the snapshots, and answer the "
            "adapter-contract question of the pull request template: a change to "
            "the adapter contract carries a migration note and conformance cases.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

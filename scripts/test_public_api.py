#!/usr/bin/env python3
"""Tests for check_public_api.py, the public-API gate that check-nvx-rs runs.

The gate lists the public API of nvx in each configuration that adapters
build on, with pinned tools, and compares it with the snapshots in
tests/public-api (#620). These tests cover the configurations and the listing
command, how the listings of a configuration's targets split into snapshots,
the check and the update, the tool pins and their installation, the exit
statuses and reports, the committed snapshots, and the pull request template
that asks about the adapter contract.
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Generator, Mapping, Sequence
from pathlib import Path
from typing import Any, cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import check_public_api as gate  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[1]
LINUX = gate.LINUX_TARGET
WINDOWS = gate.WINDOWS_TARGET
MUSL = gate.GUEST_TARGET
PROTOCOL = "pub mod nvx\npub mod nvx::protocol\n"
HOST = PROTOCOL + "pub mod nvx::host\n"
GUEST = PROTOCOL + "pub mod nvx::guest\n"
CONTRACT_QUESTION = (
    "Does this change the adapter contract (public API, behavior, persisted "
    "formats or wire)? If so, add a migration note and conformance cases."
)


def _option(command: Sequence[str], option: str) -> str | None:
    return next(
        (
            command[index + 1]
            for index in range(len(command) - 1)
            if command[index] == option
        ),
        None,
    )


def _document(
    modules: Sequence[str], defaults: Sequence[tuple[str, str, object]] = ()
) -> dict[str, object]:
    """Return a rustdoc JSON document that reaches `modules`, each a path from
    `nvx`, and a struct at each path of `defaults` whose generic parameter has
    the default type."""
    index: dict[str, dict[str, object]] = {}
    ids: dict[str, int] = {}

    def add(path: str, inner: dict[str, object]) -> None:
        identifier = len(index)
        ids[path] = identifier
        index[str(identifier)] = {"name": path.rsplit("::", 1)[-1], "inner": inner}
        parent = path.rpartition("::")[0]
        if parent:
            items = index[str(ids[parent])]["inner"]
            cast(
                list[int], cast(dict[str, dict[str, object]], items)["module"]["items"]
            ).append(identifier)

    for path in sorted({"nvx", *modules}, key=lambda path: path.count("::")):
        add(path, {"module": {"items": []}})
    for path, name, default in defaults:
        parameter = {"name": name, "kind": {"type": {"default": default}}}
        add(path, {"struct": {"generics": {"params": [parameter]}}})
    return {"root": ids["nvx"], "index": index}


class FakeCargo:
    """Answers the commands of the gate as the pinned tools would."""

    def __init__(self, version: str = gate.CARGO_PUBLIC_API_VERSION) -> None:
        self.version = f"cargo-public-api {version}\n"
        self.listings: dict[tuple[str, str], str | Exception] = {
            ("protocol", LINUX): PROTOCOL,
            ("protocol", WINDOWS): PROTOCOL,
            ("host", LINUX): HOST,
            ("host", WINDOWS): HOST,
            ("guest", MUSL): GUEST,
        }
        # The generic defaults of each listing's rustdoc JSON.
        self.defaults: dict[tuple[str, str], list[tuple[str, str, object]]] = {}
        self.listed: dict[str, tuple[str, str]] = {}
        self.commands: list[list[str]] = []

    def __call__(self, command: Sequence[str]) -> str:
        self.commands.append(list(command))
        if list(command) == ["cargo", "public-api", "--version"]:
            return self.version
        name = {None: "protocol", "host": "host", "guest": "guest"}[
            _option(command, "--features")
        ]
        target = _option(command, "--target")
        assert target is not None
        listing = self.listings[(name, target)]
        if isinstance(listing, Exception):
            raise listing
        self.listed[target] = (name, listing)
        return listing

    def document(self, target: str) -> dict[str, object]:
        """Return the rustdoc JSON of the last listing for `target`."""
        name, listing = self.listed[target]
        modules = [
            line.removeprefix("pub mod ")
            for line in listing.splitlines()
            if line.startswith("pub mod ")
        ]
        return _document(modules, self.defaults.get((name, target), []))


@contextlib.contextmanager
def _repository() -> Generator[Path, None, None]:
    """Point the gate at an empty repository and return its snapshot directory."""
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        snapshots = root / "tests" / "public-api"
        snapshots.mkdir(parents=True)
        with (
            patch.object(gate, "REPO_ROOT", root),
            patch.object(gate, "SNAPSHOTS", snapshots),
        ):
            yield snapshots


def _main(
    cargo: FakeCargo, *argv: str, environment: Mapping[str, str] | None = None
) -> tuple[int, str, str]:
    stdout = io.StringIO()
    stderr = io.StringIO()
    with patch.dict(os.environ):
        for name in ("GITHUB_ACTIONS", "GITHUB_STEP_SUMMARY"):
            os.environ.pop(name, None)
        os.environ.update(environment or {})
        with (
            patch.object(gate, "run", cargo),
            patch.object(gate, "rustdoc_document", cargo.document),
            contextlib.redirect_stdout(stdout),
            contextlib.redirect_stderr(stderr),
        ):
            status = gate.main(list(argv))
    return status, stdout.getvalue(), stderr.getvalue()


def _files(directory: Path) -> dict[str, bytes]:
    return {path.name: path.read_bytes() for path in sorted(directory.iterdir())}


class ConfigurationTests(unittest.TestCase):
    def test_configurations_are_those_of_the_runtime_adapter(self):
        # The runtime adapter builds on nvx without features, and with each
        # side that one of its features enables.
        manifest = (REPO_ROOT / "tests" / "runtime-adapter" / "Cargo.toml").read_text(
            encoding="utf-8"
        )
        sides = re.findall(r'^(\w+) = \["nvx/(\w+)"\]$', manifest, re.MULTILINE)
        self.assertEqual(sides, [("host", "host"), ("guest", "guest")])
        self.assertIn("\ndefault = []\n", manifest)
        self.assertEqual(
            [(item.name, item.features) for item in gate.CONFIGURATIONS],
            [("protocol", ()), ("host", ("host",)), ("guest", ("guest",))],
        )

    def test_hosts_list_the_host_side_and_guests_the_guest_side(self):
        targets = {item.name: item.targets for item in gate.CONFIGURATIONS}
        self.assertEqual(targets["protocol"], (LINUX, WINDOWS))
        self.assertEqual(targets["host"], (LINUX, WINDOWS))
        self.assertEqual(targets["guest"], (MUSL,))
        self.assertEqual(gate.targets(), sorted({LINUX, WINDOWS, MUSL}))

    def test_listing_command_pins_the_tools_and_the_configuration(self):
        protocol, host, _ = gate.CONFIGURATIONS
        common = [
            "cargo",
            f"+{gate.NIGHTLY_TOOLCHAIN}",
            "public-api",
            "--manifest-path",
            os.fspath(REPO_ROOT / "nvx-rs" / "Cargo.toml"),
            "--color",
            "never",
            "--omit",
            "blanket-impls",
            "--no-default-features",
        ]
        self.assertEqual(
            gate.listing_command(protocol, LINUX), [*common, "--target", LINUX]
        )
        self.assertEqual(
            gate.listing_command(host, WINDOWS),
            [*common, "--target", WINDOWS, "--features", "host"],
        )

    def test_pins_are_exact_releases(self):
        self.assertRegex(gate.CARGO_PUBLIC_API_VERSION, r"^[0-9]+\.[0-9]+\.[0-9]+$")
        self.assertRegex(
            gate.NIGHTLY_TOOLCHAIN, r"^nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}$"
        )


class SnapshotTests(unittest.TestCase):
    def _snapshots(
        self, name: str, listings: Mapping[str, str]
    ) -> list[tuple[str, str, str]]:
        configuration = next(item for item in gate.CONFIGURATIONS if item.name == name)
        return [
            (snapshot.path.name, snapshot.text, snapshot.subject)
            for snapshot in gate.snapshots(configuration, listings)
        ]

    def test_targets_that_list_one_api_share_one_snapshot(self):
        with _repository():
            self.assertEqual(
                self._snapshots("host", {LINUX: HOST, WINDOWS: HOST}),
                [("host.txt", HOST, "the host API of nvx")],
            )
            self.assertEqual(
                self._snapshots("guest", {MUSL: GUEST}),
                [("guest.txt", GUEST, "the guest API of nvx")],
            )

    def test_items_specific_to_a_target_get_its_own_snapshot(self):
        with _repository():
            self.assertEqual(
                self._snapshots("host", {LINUX: "a\nb\nc\n", WINDOWS: "a\nc\nd\n"}),
                [
                    ("host.txt", "a\nc\n", "the host API of nvx"),
                    (
                        f"host.{LINUX}.txt",
                        "b\n",
                        f"the host API of nvx specific to {LINUX}",
                    ),
                    (
                        f"host.{WINDOWS}.txt",
                        "d\n",
                        f"the host API of nvx specific to {WINDOWS}",
                    ),
                ],
            )

    def test_repeated_items_are_counted(self):
        with _repository():
            self.assertEqual(
                self._snapshots("protocol", {LINUX: "a\na\nb\n", WINDOWS: "b\na"}),
                [
                    ("protocol.txt", "a\nb\n", "the protocol API of nvx"),
                    (
                        f"protocol.{LINUX}.txt",
                        "a\n",
                        f"the protocol API of nvx specific to {LINUX}",
                    ),
                ],
            )


class CheckTests(unittest.TestCase):
    def test_update_writes_the_snapshots_that_the_check_then_accepts(self):
        cargo = FakeCargo()
        with _repository() as snapshots:
            status, stdout, _ = _main(cargo, "--update")
            self.assertEqual(status, 0)
            self.assertEqual(
                _files(snapshots),
                {
                    "guest.txt": GUEST.encode(),
                    "host.txt": HOST.encode(),
                    "protocol.txt": PROTOCOL.encode(),
                },
            )
            self.assertIn("updated tests/public-api/host.txt\n", stdout)
            status, stdout, stderr = _main(cargo)
        self.assertEqual((status, stderr), (0, ""))
        self.assertIn(f"the host API of nvx lists 3 items on {WINDOWS}\n", stdout)
        self.assertNotIn("updated", stdout)
        # Each check first requires the pinned cargo-public-api, then lists
        # each configuration on each of its targets.
        listed = [
            command
            for command in cargo.commands
            if command != ["cargo", "public-api", "--version"]
        ]
        self.assertEqual(len(listed), 2 * 5)
        self.assertEqual(cargo.commands[0], ["cargo", "public-api", "--version"])

    def test_check_reports_a_changed_api_and_leaves_its_snapshot(self):
        cargo = FakeCargo()
        with _repository() as snapshots:
            _main(cargo, "--update")
            for target in (LINUX, WINDOWS):
                cargo.listings[("host", target)] = HOST + "pub fn nvx::host::new()\n"
            status, stdout, stderr = _main(cargo)
            self.assertEqual((snapshots / "host.txt").read_text(encoding="utf-8"), HOST)
        self.assertEqual(status, 1)
        self.assertIn("--- tests/public-api/host.txt\n", stdout)
        self.assertIn("+++ the host API of nvx\n", stdout)
        self.assertIn("+pub fn nvx::host::new()\n", stdout)
        self.assertIn(
            "tests/public-api/host.txt: the host API of nvx differs from this "
            "snapshot\n",
            stdout,
        )
        self.assertNotIn("protocol.txt:", stdout)
        self.assertIn("`python3 scripts/check_public_api.py --update`", stderr)
        self.assertIn("pull request template", stderr)
        self.assertIn("migration note and conformance cases", stderr)

    def test_check_reports_a_missing_snapshot(self):
        cargo = FakeCargo()
        with _repository() as snapshots:
            _main(cargo, "--update")
            (snapshots / "guest.txt").unlink()
            status, stdout, _ = _main(cargo)
        self.assertEqual(status, 1)
        self.assertIn(
            "tests/public-api/guest.txt: the guest API of nvx has no snapshot\n",
            stdout,
        )

    def test_items_specific_to_a_target_are_checked_and_updated(self):
        cargo = FakeCargo()
        windows = f"host.{WINDOWS}.txt"
        with _repository() as snapshots:
            _main(cargo, "--update")
            cargo.listings[("host", WINDOWS)] = HOST + "pub fn nvx::host::handle()\n"
            status, stdout, _ = _main(cargo)
            self.assertEqual(status, 1)
            self.assertIn(
                f"tests/public-api/{windows}: the host API of nvx specific to "
                f"{WINDOWS} has no snapshot\n",
                stdout,
            )
            self.assertEqual(_main(cargo, "--update")[0], 0)
            self.assertEqual(
                (snapshots / windows).read_text(encoding="utf-8"),
                "pub fn nvx::host::handle()\n",
            )
            self.assertEqual((snapshots / "host.txt").read_text(encoding="utf-8"), HOST)
            self.assertEqual(_main(cargo)[0], 0)
            # Once every target lists the same API again, the target's
            # snapshot is obsolete.
            cargo.listings[("host", WINDOWS)] = HOST
            status, stdout, _ = _main(cargo)
            self.assertEqual(status, 1)
            self.assertIn(
                f"tests/public-api/{windows}: no listing has items for this snapshot\n",
                stdout,
            )
            status, stdout, _ = _main(cargo, "--update")
            self.assertEqual(status, 0)
            self.assertIn(f"removed tests/public-api/{windows}\n", stdout)
            self.assertEqual(
                sorted(_files(snapshots)), ["guest.txt", "host.txt", "protocol.txt"]
            )

    def test_check_reports_a_snapshot_of_no_configuration(self):
        cargo = FakeCargo()
        with _repository() as snapshots:
            _main(cargo, "--update")
            (snapshots / "previous.txt").write_text("pub mod nvx\n", encoding="utf-8")
            (snapshots / "README.md").write_text("# Snapshots\n", encoding="utf-8")
            status, stdout, _ = _main(cargo)
            self.assertEqual(status, 1)
            self.assertIn(
                "tests/public-api/previous.txt: no listing has items for this "
                "snapshot\n",
                stdout,
            )
            self.assertNotIn("README.md", stdout)
            self.assertEqual(_main(cargo, "--update")[0], 0)
            self.assertFalse((snapshots / "previous.txt").exists())
            self.assertTrue((snapshots / "README.md").exists())

    def test_snapshots_checked_out_with_crlf_line_endings_match(self):
        cargo = FakeCargo()
        with _repository() as snapshots:
            _main(cargo, "--update")
            for path in snapshots.iterdir():
                path.write_bytes(path.read_bytes().replace(b"\n", b"\r\n"))
            self.assertEqual(_main(cargo)[0], 0)

    def test_github_actions_annotations_and_job_summary(self):
        cargo = FakeCargo()
        with _repository() as snapshots:
            _main(cargo, "--update")
            summary = snapshots.parent / "summary.md"
            environment = {
                "GITHUB_ACTIONS": "true",
                "GITHUB_STEP_SUMMARY": str(summary),
            }
            status, _, _ = _main(cargo, environment=environment)
            self.assertEqual(status, 0)
            passed = summary.read_text(encoding="utf-8")
            summary.unlink()
            cargo.listings[("guest", MUSL)] = GUEST + "pub fn nvx::guest::new()\n"
            status, stdout, _ = _main(cargo, environment=environment)
            failed = summary.read_text(encoding="utf-8")
        self.assertEqual(status, 1)
        self.assertIn(
            "::error file=tests/public-api/guest.txt::the guest API of nvx differs "
            "from this snapshot\n",
            stdout,
        )
        self.assertIn("| Configuration | Features | Target | Items |\n", passed)
        self.assertIn(f"| protocol | none | `{LINUX}` | 2 |\n", passed)
        self.assertIn(f"| host | `host` | `{WINDOWS}` | 3 |\n", passed)
        self.assertIn(f"| guest | `guest` | `{MUSL}` | 3 |\n", passed)
        self.assertIn("The snapshots in `tests/public-api` match", passed)
        self.assertIn(f"| guest | `guest` | `{MUSL}` | 4 |\n", failed)
        self.assertIn("The snapshots in `tests/public-api` differ", failed)


class GenericDefaultTests(unittest.TestCase):
    def test_defaults_are_recorded_at_their_public_paths(self):
        # `Result<T, E = Error, const N: usize = 4>` lives in a private module
        # and is re-exported at the root; `Wide<T = u8>` comes from a glob
        # re-export; `Hidden<T = u8>` is reachable from nowhere.
        error = {"resolved_path": {"path": "Error", "id": 9, "args": None}}
        result = {
            "name": "Result",
            "inner": {
                "type_alias": {
                    "generics": {
                        "params": [
                            {"name": "T", "kind": {"type": {"default": None}}},
                            {"name": "E", "kind": {"type": {"default": error}}},
                            {"name": "N", "kind": {"const": {"default": "4"}}},
                        ]
                    }
                }
            },
        }
        byte = {"type": {"default": {"primitive": "u8"}}}
        struct = {"generics": {"params": [{"name": "T", "kind": byte}]}}
        document = {
            "root": 0,
            "index": {
                "0": {"name": "nvx", "inner": {"module": {"items": [1, 3]}}},
                "1": {
                    "name": None,
                    "inner": {"use": {"id": 2, "name": "Result", "is_glob": False}},
                },
                "2": result,
                "3": {
                    "name": None,
                    "inner": {"use": {"id": 4, "name": "wide", "is_glob": True}},
                },
                "4": {"name": "wide", "inner": {"module": {"items": [5]}}},
                "5": {"name": "Wide", "inner": {"struct": struct}},
                "6": {"name": "Hidden", "inner": {"struct": struct}},
            },
            "paths": {"9": {"crate_id": 0, "path": ["nvx", "error", "Error"]}},
        }
        self.assertEqual(
            gate.generic_defaults(document),
            [
                'generic default nvx::Result<E> = {"resolved_path":{"args":null,'
                '"id":"nvx::error::Error","path":"Error"}}',
                'generic default nvx::Result<N> = "4"',
                'generic default nvx::Wide<T> = {"primitive":"u8"}',
            ],
        )
        paths = gate.public_paths(document)
        self.assertEqual(paths["0"], ["nvx"])
        self.assertNotIn("4", paths)
        self.assertNotIn("6", paths)

    def test_defaults_record_the_item_that_their_name_resolves_to(self):
        # A private import can make the same spelling name another type.
        def default(identifier: int) -> list[str]:
            reference = {"resolved_path": {"path": "Error", "id": identifier}}
            kind = {"type": {"default": reference}}
            struct = {"generics": {"params": [{"name": "T", "kind": kind}]}}
            document: dict[str, object] = {
                "root": 0,
                "index": {
                    "0": {"name": "nvx", "inner": {"module": {"items": [1, 2]}}},
                    "1": {"name": "Pool", "inner": {"struct": struct}},
                    "2": {"name": "Error", "inner": {"struct": {}}},
                },
                "paths": {
                    "2": {"crate_id": 0, "path": ["nvx", "left", "Error"]},
                    "7": {"crate_id": 3, "path": ["std", "io", "error", "Error"]},
                },
            }
            return gate.generic_defaults(document)

        self.assertEqual(
            default(2),
            [
                'generic default nvx::Pool<T> = {"resolved_path":'
                '{"id":"nvx::Error","path":"Error"}}'
            ],
        )
        self.assertEqual(
            default(7),
            [
                'generic default nvx::Pool<T> = {"resolved_path":'
                '{"id":"std::io::error::Error","path":"Error"}}'
            ],
        )
        self.assertEqual(
            default(8),
            [
                'generic default nvx::Pool<T> = {"resolved_path":{"id":null,"path":"Error"}}'
            ],
        )

    def test_trait_items_record_whether_the_trait_provides_them(self):
        error = {"resolved_path": {"path": "Error", "id": 9, "args": None}}
        document = {
            "root": 0,
            "index": {
                "0": {"name": "nvx", "inner": {"module": {"items": [1]}}},
                "1": {"name": "Sink", "inner": {"trait": {"items": [2, 3, 4, 5, 6]}}},
                "2": {"name": "write", "inner": {"function": {"has_body": False}}},
                "3": {"name": "flush", "inner": {"function": {"has_body": True}}},
                "4": {"name": "LIMIT", "inner": {"assoc_const": {"value": "4"}}},
                "5": {"name": "Error", "inner": {"assoc_type": {"type": error}}},
                "6": {"name": "Item", "inner": {"assoc_type": {"type": None}}},
            },
            "paths": {"9": {"crate_id": 3, "path": ["std", "io", "error", "Error"]}},
        }
        self.assertEqual(
            gate.trait_item_defaults(document),
            [
                'trait constant nvx::Sink::LIMIT is provided = "4"',
                "trait method nvx::Sink::flush is provided",
                "trait method nvx::Sink::write is required",
                "trait type nvx::Sink::Error is provided = "
                '{"resolved_path":{"args":null,"id":"std::io::error::Error",'
                '"path":"Error"}}',
                "trait type nvx::Sink::Item is required",
            ],
        )

    def test_a_change_of_a_provided_trait_method_fails_the_check(self):
        cargo = FakeCargo()
        provided = [False]
        generate = cargo.document

        def document(target: str) -> dict[str, object]:
            result = generate(target)
            index = cast(dict[str, dict[str, Any]], result["index"])
            root = index[str(result["root"])]
            trait, method = len(index), len(index) + 1
            index[str(trait)] = {
                "name": "OutputSink",
                "inner": {"trait": {"items": [method]}},
            }
            index[str(method)] = {
                "name": "write",
                "inner": {"function": {"has_body": provided[0]}},
            }
            root["inner"]["module"]["items"].append(trait)
            return result

        cargo.document = document
        with _repository():
            self.assertEqual(_main(cargo, "--update")[0], 0)
            self.assertEqual(_main(cargo)[0], 0)
            provided[0] = True
            status, stdout, _ = _main(cargo)
        self.assertEqual(status, 1)
        self.assertIn("-trait method nvx::OutputSink::write is required\n", stdout)
        self.assertIn("+trait method nvx::OutputSink::write is provided\n", stdout)

    def test_a_default_only_change_fails_the_check(self):
        cargo = FakeCargo()
        for target in (LINUX, WINDOWS):
            cargo.defaults[("host", target)] = [
                ("nvx::host::Pool", "T", {"primitive": "u8"})
            ]
        with _repository() as snapshots:
            self.assertEqual(_main(cargo, "--update")[0], 0)
            self.assertIn(
                'generic default nvx::host::Pool<T> = {"primitive":"u8"}\n',
                (snapshots / "host.txt").read_text(encoding="utf-8"),
            )
            self.assertEqual(_main(cargo)[0], 0)
            for target in (LINUX, WINDOWS):
                cargo.defaults[("host", target)] = [
                    ("nvx::host::Pool", "T", {"primitive": "u16"})
                ]
            status, stdout, _ = _main(cargo)
        self.assertEqual(status, 1)
        self.assertIn(
            '-generic default nvx::host::Pool<T> = {"primitive":"u8"}\n', stdout
        )
        self.assertIn(
            '+generic default nvx::host::Pool<T> = {"primitive":"u16"}\n', stdout
        )
        self.assertIn("tests/public-api/host.txt: the host API of nvx differs", stdout)

    def test_rustdoc_json_of_other_modules_is_a_tooling_error(self):
        # A stale rustdoc JSON, such as one of another configuration, reaches
        # other modules than the listing names.
        cargo = FakeCargo()
        cargo.document = lambda target: _document(["nvx::protocol", "nvx::guest"])
        with _repository() as snapshots:
            status, _, stderr = _main(cargo, "--update")
            self.assertEqual(_files(snapshots), {})
        self.assertEqual(status, 2)
        self.assertIn(
            f"the rustdoc JSON for {LINUX} does not match the protocol listing: "
            "its modules differ by ['nvx::guest']",
            stderr,
        )

    def test_rustdoc_document_reads_the_json_of_the_library_for_the_target(self):
        with tempfile.TemporaryDirectory() as directory:
            target_directory = Path(directory)
            metadata = {
                "target_directory": os.fspath(target_directory),
                "packages": [
                    {
                        "manifest_path": os.fspath(gate.MANIFEST),
                        "targets": [
                            {"name": "nvx-fake-openvmm", "kind": ["bin"]},
                            {"name": "nvx", "kind": ["lib"]},
                        ],
                    }
                ],
            }
            commands: list[list[str]] = []

            def run(command: Sequence[str]) -> str:
                commands.append(list(command))
                return json.dumps(metadata)

            with patch.object(gate, "run", run):
                with self.assertRaises(gate.CheckError) as raised:
                    gate.rustdoc_document(MUSL)
                self.assertIn(
                    f"cannot read the rustdoc JSON for {MUSL}", str(raised.exception)
                )
                path = target_directory / MUSL / "doc" / "nvx.json"
                path.parent.mkdir(parents=True)
                path.write_text('{"root": 0}', encoding="utf-8")
                self.assertEqual(gate.rustdoc_document(MUSL), {"root": 0})
        self.assertEqual(
            commands[0],
            [
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--manifest-path",
                os.fspath(gate.MANIFEST),
            ],
        )


class ToolTests(unittest.TestCase):
    def test_another_cargo_public_api_release_is_a_tooling_error(self):
        with _repository() as snapshots:
            status, _, stderr = _main(FakeCargo("0.51.0"), "--update")
            self.assertEqual(_files(snapshots), {})
        self.assertEqual(status, 2)
        self.assertIn(
            f"found cargo-public-api 0.51.0 instead of cargo-public-api "
            f"{gate.CARGO_PUBLIC_API_VERSION}",
            stderr,
        )
        self.assertIn("python3 scripts/check_public_api.py --install-tools", stderr)

    def test_a_failed_listing_is_a_tooling_error_that_writes_nothing(self):
        cargo = FakeCargo()
        cargo.listings[("guest", MUSL)] = gate.CheckError("rustdoc failed")
        with _repository() as snapshots:
            status, _, stderr = _main(cargo, "--update")
            self.assertEqual(_files(snapshots), {})
        self.assertEqual((status, stderr), (2, "error: rustdoc failed\n"))

    def test_run_reports_the_command_and_its_error_output(self):
        command = [
            sys.executable,
            "-c",
            "import sys; sys.stderr.write('no rustdoc'); sys.exit(3)",
        ]
        with self.assertRaises(gate.CheckError) as raised:
            gate.run(command)
        message = str(raised.exception)
        self.assertIn("failed with exit status 3", message)
        self.assertIn("python3 scripts/check_public_api.py --install-tools", message)
        self.assertTrue(message.endswith("\nno rustdoc"))
        with self.assertRaises(gate.CheckError) as raised:
            gate.run(["nvx-public-api-test-missing-program"])
        self.assertIn(
            "cannot run nvx-public-api-test-missing-program", str(raised.exception)
        )
        self.assertEqual(gate.run([sys.executable, "-c", "print('items')"]), "items\n")

    def test_missing_cargo_public_api_is_a_tooling_error(self):
        def cargo(command: Sequence[str]) -> str:
            raise gate.CheckError("no such command: `public-api`")

        with _repository(), patch.object(gate, "run", cargo):
            with self.assertRaises(gate.CheckError) as raised:
                gate.check(update=False)
        self.assertEqual(
            str(raised.exception),
            f"cargo-public-api {gate.CARGO_PUBLIC_API_VERSION} is not installed: "
            "no such command: `public-api`",
        )

    def test_install_tools_installs_the_pins(self):
        commands: list[list[str]] = []

        def run(
            command: Sequence[str], **_: object
        ) -> subprocess.CompletedProcess[str]:
            commands.append(list(command))
            return subprocess.CompletedProcess(command, 0)

        stdout = io.StringIO()
        with (
            patch.object(gate.subprocess, "run", run),
            contextlib.redirect_stdout(stdout),
        ):
            self.assertEqual(gate.main(["--install-tools", "--root", "tools"]), 0)
        nightly = gate.NIGHTLY_TOOLCHAIN
        self.assertEqual(
            commands,
            [
                ["rustup", "toolchain", "install", nightly, "--profile", "minimal"],
                ["rustup", "target", "add", "--toolchain", nightly, *gate.targets()],
                [
                    "cargo",
                    "install",
                    "--locked",
                    "cargo-public-api",
                    "--version",
                    gate.CARGO_PUBLIC_API_VERSION,
                    "--root",
                    "tools",
                ],
            ],
        )
        self.assertIn(
            f">> rustup toolchain install {nightly} --profile minimal\n",
            stdout.getvalue(),
        )

    def test_install_tools_stops_at_the_first_failure(self):
        commands: list[list[str]] = []

        def run(
            command: Sequence[str], **_: object
        ) -> subprocess.CompletedProcess[str]:
            commands.append(list(command))
            return subprocess.CompletedProcess(command, 1)

        stderr = io.StringIO()
        with (
            patch.object(gate.subprocess, "run", run),
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(stderr),
        ):
            self.assertEqual(gate.main(["--install-tools"]), 2)
        self.assertEqual(len(commands), 1)
        self.assertNotIn("--root", commands[0])
        self.assertIn("failed with exit status 1", stderr.getvalue())

    def test_options_that_do_not_combine_are_rejected(self):
        for argv in (["--root", "tools"], ["--update", "--install-tools"]):
            with self.subTest(argv=argv):
                with (
                    contextlib.redirect_stderr(io.StringIO()),
                    self.assertRaises(SystemExit) as raised,
                ):
                    gate.main(argv)
                self.assertEqual(raised.exception.code, 2)


class RepositoryTests(unittest.TestCase):
    def test_committed_snapshots_are_those_of_the_configurations(self):
        names = {path.name for path in gate.SNAPSHOTS.glob("*.txt")}
        shared = {f"{item.name}.txt" for item in gate.CONFIGURATIONS}
        specific = {
            f"{item.name}.{target}.txt"
            for item in gate.CONFIGURATIONS
            for target in item.targets
        }
        self.assertLessEqual(shared, names)
        self.assertLessEqual(names, shared | specific)
        for name in sorted(names):
            # A checkout may convert the snapshots to CRLF, which the check accepts.
            data = (gate.SNAPSHOTS / name).read_bytes().replace(b"\r\n", b"\n")
            with self.subTest(snapshot=name):
                self.assertTrue(data.endswith(b"\n"))
                self.assertNotIn(b"\r", data)
                self.assertNotIn(b"\n\n", data)

    def test_committed_snapshots_keep_each_side_to_its_configuration(self):
        def modules(name: str) -> set[str]:
            text = (gate.SNAPSHOTS / f"{name}.txt").read_text(encoding="utf-8")
            return set(re.findall(r"^pub mod (\S+)$", text, re.MULTILINE))

        protocol, host, guest = modules("protocol"), modules("host"), modules("guest")
        self.assertIn("nvx::protocol", protocol)
        self.assertIn("nvx::host", host)
        self.assertIn("nvx::guest", guest)
        self.assertLess(protocol, host)
        self.assertLess(protocol, guest)
        for name, side in (
            ("protocol", "host"),
            ("protocol", "guest"),
            ("host", "guest"),
            ("guest", "host"),
        ):
            text = (gate.SNAPSHOTS / f"{name}.txt").read_text(encoding="utf-8")
            with self.subTest(snapshot=name, side=side):
                self.assertNotRegex(text, rf"\bnvx::{side}\b")

    def test_pull_request_template_asks_about_the_adapter_contract(self):
        template = (REPO_ROOT / ".github" / "pull_request_template.md").read_text(
            encoding="utf-8"
        )
        self.assertIn(f"\n{CONTRACT_QUESTION}\n", template)


if __name__ == "__main__":
    unittest.main()

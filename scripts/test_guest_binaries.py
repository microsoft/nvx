#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from collections.abc import Generator
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import guest_binaries  # noqa: E402
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
    GuestRustBuildConstants,
)


def _write_build_output(
    output: Path, binaries: tuple[str, ...], *, recorded: tuple[str, ...]
) -> None:
    """Write the output of an earlier build whose manifest records ``recorded``."""
    output.mkdir()
    for binary in binaries:
        (output / binary).write_bytes(f"previous {binary}".encode())
    manifest = {
        "domain": GuestRustBuildConstants.BUILD_MANIFEST_DOMAIN,
        "format": GuestRustBuildConstants.BUILD_MANIFEST_FORMAT,
        "binaries": {binary: "0" * 64 for binary in recorded},
    }
    (output / GuestRustBuildConstants.BUILD_MANIFEST_NAME).write_text(
        json.dumps(manifest), encoding="utf-8"
    )


def _contents(directory: Path) -> dict[str, bytes]:
    """Return the name and contents of every file in a directory."""
    return {path.name: path.read_bytes() for path in directory.iterdir()}


@contextlib.contextmanager
def _fake_guest_build() -> Generator[MagicMock]:
    """Stand for Cargo, which writes each binary's name as its contents."""

    def run_cargo(
        command: list[str | os.PathLike[str]],
        *,
        cwd: Path,
        env: dict[str, str],
    ) -> None:
        rendered = [os.fspath(part) for part in command]
        destination = (
            Path(env["CARGO_TARGET_DIR"])
            / GuestRustBuildConstants.TARGET
            / GuestRustBuildConstants.PROFILE
        )
        destination.mkdir(parents=True, exist_ok=True)
        for index, value in enumerate(rendered):
            if value == "--bin":
                name = rendered[index + 1]
                (destination / name).write_bytes(name.encode())

    cargo = MagicMock(side_effect=run_cargo)
    with (
        patch.object(guest_binaries.sys, "platform", "linux"),
        patch.object(
            guest_binaries, "guest_binary_source_sha256", return_value="a" * 64
        ),
        patch.object(guest_binaries, "require_tool", return_value="cargo"),
        patch.object(guest_binaries, "run_checked", cargo),
        patch.object(guest_binaries, "check_static_executables"),
    ):
        yield cargo


class GuestBinaryBuildTests(unittest.TestCase):
    def test_source_identity_covers_the_crate_and_the_toolchain(self):
        relative = {
            path.relative_to(BuildConstants.REPO_ROOT).as_posix()
            for path in guest_binaries.guest_binary_source_files()
        }
        self.assertIn("rust-toolchain.toml", relative)
        self.assertIn("nvx-rs/Cargo.toml", relative)
        self.assertIn("nvx-rs/Cargo.lock", relative)
        self.assertIn("nvx-rs/build.rs", relative)
        self.assertIn("scripts/nvx_tools/build_constants.py", relative)
        self.assertIn("scripts/nvx_tools/guest_binaries.py", relative)
        self.assertIn("nvx-rs/src/guest/exit.rs", relative)
        self.assertIn("nvx-rs/src/guest/device_io.rs", relative)
        self.assertFalse(any("/target/" in f"/{path}/" for path in relative))
        self.assertRegex(guest_binaries.guest_binary_source_sha256(), r"^[0-9a-f]{64}$")
        self.assertEqual(
            guest_binaries.pinned_guest_rust_toolchain(),
            GuestRustBuildConstants.TOOLCHAIN,
        )

    def test_build_uses_fresh_remapped_target_and_writes_manifest(self):
        with tempfile.TemporaryDirectory(
            prefix=".test-guest-binaries-",
            dir=BuildConstants.REPO_ROOT,
        ) as temporary:
            root = Path(temporary)
            crate = root / "nvx-rs"
            source = crate / "src"
            tools = root / "scripts" / "nvx_tools"
            source.mkdir(parents=True)
            tools.mkdir(parents=True)
            recipe_constants = tools / "build_constants.py"
            recipe_builder = tools / "guest_binaries.py"
            files = {
                crate / "Cargo.toml": '[package]\nname = "fixture"\n',
                crate / "Cargo.lock": "# lock\n",
                crate / "build.rs": "fn main() {}\n",
                source / "lib.rs": "pub fn fixture() {}\n",
                recipe_constants: "PROFILE = 'guest'\n",
                recipe_builder: "def build(): pass\n",
                root / "rust-toolchain.toml": (
                    f'[toolchain]\nchannel = "{GuestRustBuildConstants.TOOLCHAIN}"\n'
                ),
            }
            for path, contents in files.items():
                path.write_text(contents, encoding="utf-8")
            output = root / "artifacts"
            stale = root / ".artifacts.work" / "stale"
            stale.parent.mkdir()
            stale.write_text("stale", encoding="utf-8")
            commands: list[list[str]] = []
            environments: list[dict[str, str]] = []

            def run_cargo(
                command: list[str | os.PathLike[str]],
                *,
                cwd: Path,
                env: dict[str, str],
            ) -> None:
                rendered = [os.fspath(part) for part in command]
                commands.append(rendered)
                environments.append(dict(env))
                self.assertEqual(cwd, crate)
                self.assertTrue(stale.exists())
                destination = (
                    Path(env["CARGO_TARGET_DIR"])
                    / GuestRustBuildConstants.TARGET
                    / GuestRustBuildConstants.PROFILE
                )
                destination.mkdir(parents=True, exist_ok=True)
                names = [
                    rendered[index + 1]
                    for index, value in enumerate(rendered)
                    if value == "--bin"
                ]
                for name in names:
                    (destination / name).write_bytes(name.encode())

            cargo_home = root / "cargo-home"
            with (
                patch.object(guest_binaries.sys, "platform", "linux"),
                patch.object(BuildConstants, "REPO_ROOT", root),
                patch.object(GuestRustBuildConstants, "DIRECTORY", crate),
                patch.object(GuestRustBuildConstants, "MANIFEST", crate / "Cargo.toml"),
                patch.object(GuestRustBuildConstants, "LOCKFILE", crate / "Cargo.lock"),
                patch.object(
                    GuestRustBuildConstants, "BUILD_SCRIPT", crate / "build.rs"
                ),
                patch.object(
                    GuestRustBuildConstants,
                    "SOURCE_DIRECTORIES",
                    (source,),
                ),
                patch.object(
                    GuestRustBuildConstants,
                    "BUILD_RECIPE_FILES",
                    (recipe_constants, recipe_builder),
                ),
                patch.object(
                    GuestRustBuildConstants,
                    "TOOLCHAIN_FILE",
                    root / "rust-toolchain.toml",
                ),
                patch.object(guest_binaries, "require_tool", return_value="cargo"),
                patch.object(guest_binaries, "run_checked", side_effect=run_cargo),
                patch.object(
                    guest_binaries, "check_static_executables"
                ) as check_static,
                patch.dict(
                    os.environ,
                    {
                        "CARGO_HOME": str(cargo_home),
                        "RUSTFLAGS": "host flags",
                        "CARGO_PROFILE_GUEST_OPT_LEVEL": "0",
                        "CARGO_PROFILE_GUEST_STRIP": "none",
                        "CARGO_PROFILE_RELEASE_LTO": "false",
                    },
                ),
            ):
                first_source = guest_binaries.guest_binary_source_sha256()
                recipe_constants.write_text(
                    "PROFILE = 'guest'\nLTO = True\n", encoding="utf-8"
                )
                self.assertNotEqual(
                    guest_binaries.guest_binary_source_sha256(), first_source
                )
                result = guest_binaries.build_guest_binaries(output)
                inspected = guest_binaries.inspect_guest_binaries(output)
                (source / "lib.rs").write_bytes(b"pub fn fixture() {}\r\n")
                self.assertEqual(
                    guest_binaries.guest_binary_source_sha256(),
                    result.source_sha256,
                )

            # One Cargo build produces every binary from the canonical recipe.
            self.assertEqual(len(commands), 1)
            self.assertEqual(inspected, result)
            [command] = commands
            self.assertEqual(command[:3], ["cargo", "+1.95.0", "build"])
            self.assertEqual(
                [
                    command[index + 1]
                    for index, value in enumerate(command)
                    if value == "--bin"
                ],
                list(GuestRustBuildConstants.BINARIES),
            )
            self.assertIn("--locked", command)
            self.assertIn(GuestRustBuildConstants.TARGET, command)
            self.assertIn(GuestRustBuildConstants.PROFILE, command)
            self.assertIn("--no-default-features", command)
            self.assertIn(GuestRustBuildConstants.FEATURE, command)
            self.assertNotIn("--", command)
            for environment in environments:
                self.assertEqual(environment["CARGO_INCREMENTAL"], "0")
                self.assertEqual(environment["SOURCE_DATE_EPOCH"], "0")
                self.assertNotIn("RUSTFLAGS", environment)
                self.assertFalse(
                    any(name.startswith("CARGO_PROFILE_") for name in environment)
                )
                encoded = environment["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
                self.assertIn(
                    f"--remap-path-prefix={root.resolve()}=/nvx-source", encoded
                )
                self.assertIn(
                    f"--remap-path-prefix={cargo_home.resolve()}=/cargo-home",
                    encoded,
                )
                self.assertIn("link-arg=-Wl,--build-id=none", encoded)
            self.assertEqual(
                [path.name for path in check_static.call_args.args[0]],
                list(GuestRustBuildConstants.BINARIES),
            )
            self.assertEqual(check_static.call_count, 2)
            self.assertEqual(
                set(result.binaries), set(GuestRustBuildConstants.BINARIES)
            )
            manifest = json.loads(
                (output / GuestRustBuildConstants.BUILD_MANIFEST_NAME).read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(manifest["source_sha256"], result.source_sha256)
            self.assertEqual(manifest["binaries"], result.binaries)
            self.assertEqual(
                manifest["domain"],
                GuestRustBuildConstants.BUILD_MANIFEST_DOMAIN,
            )
            self.assertTrue(stale.exists())
            self.assertFalse(any(root.glob(".artifacts.work-*")))

    def test_output_must_not_overlap_sources_or_their_ancestors(self):
        with tempfile.TemporaryDirectory(
            prefix=".test-guest-binary-source-",
            dir=BuildConstants.REPO_ROOT,
        ) as temporary:
            root = Path(temporary)
            crate = root / "nvx-rs"
            source = crate / "src"
            source.mkdir(parents=True)
            sentinel = source / "uncommitted.rs"
            sentinel.write_text("preserve", encoding="utf-8")
            toolchain = root / "rust-toolchain.toml"
            toolchain.write_text('[toolchain]\nchannel = "1.95.0"\n', encoding="utf-8")
            with (
                patch.object(GuestRustBuildConstants, "DIRECTORY", crate),
                patch.object(GuestRustBuildConstants, "TOOLCHAIN_FILE", toolchain),
            ):
                for output in (root, crate, source / "output"):
                    with (
                        self.subTest(output=output),
                        self.assertRaisesRegex(
                            guest_binaries.ScriptError, "overlaps source"
                        ),
                    ):
                        guest_binaries._validate_output_path(output.resolve())
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "preserve")

    def test_unrelated_existing_output_is_never_removed(self):
        with tempfile.TemporaryDirectory(
            prefix=".test-guest-binary-unrelated-",
            dir=BuildConstants.REPO_ROOT,
        ) as temporary:
            root = Path(temporary)
            crate = root / "nvx-rs"
            crate.mkdir()
            toolchain = root / "rust-toolchain.toml"
            toolchain.write_text('[toolchain]\nchannel = "1.95.0"\n', encoding="utf-8")
            output = root / "output"
            output.mkdir()
            sentinel = output / "uncommitted.txt"
            sentinel.write_text("preserve", encoding="utf-8")
            with (
                patch.object(GuestRustBuildConstants, "DIRECTORY", crate),
                patch.object(GuestRustBuildConstants, "TOOLCHAIN_FILE", toolchain),
                self.assertRaisesRegex(
                    guest_binaries.ScriptError, "not owned by this builder"
                ),
            ):
                guest_binaries._validate_output_path(output.resolve())
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "preserve")

    def test_rebuild_removes_a_retired_executable_that_the_manifest_records(self):
        # Otherwise, a build would remove an executable that it has just installed.
        self.assertTrue(
            set(GuestRustBuildConstants.RETIRED_BINARIES).isdisjoint(
                GuestRustBuildConstants.BINARIES
            )
        )
        with tempfile.TemporaryDirectory(
            prefix=".test-guest-binary-retired-",
            dir=BuildConstants.REPO_ROOT,
        ) as temporary:
            root = Path(temporary)
            output = root / "output"
            # The output of a build from before nvx-port-io retired.
            previous = (*GuestRustBuildConstants.BINARIES, "nvx-port-io")
            _write_build_output(output, previous, recorded=previous)
            with _fake_guest_build():
                result = guest_binaries.build_guest_binaries(output)
            self.assertEqual(
                {path.name for path in output.iterdir()},
                {
                    GuestRustBuildConstants.BUILD_MANIFEST_NAME,
                    *GuestRustBuildConstants.BINARIES,
                },
            )
            for binary in GuestRustBuildConstants.BINARIES:
                self.assertEqual((output / binary).read_bytes(), binary.encode())
            manifest = json.loads(
                (output / GuestRustBuildConstants.BUILD_MANIFEST_NAME).read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(manifest["binaries"], result.binaries)
            self.assertFalse(any(root.glob(".output.work-*")))

    def test_rebuild_adds_an_executable_that_an_earlier_build_lacked(self):
        # Otherwise, an output could lack an executable that no build installs.
        self.assertTrue(
            set(GuestRustBuildConstants.ADDED_BINARIES).issubset(
                GuestRustBuildConstants.BINARIES
            )
        )
        earlier = tuple(
            binary
            for binary in GuestRustBuildConstants.BINARIES
            if binary not in GuestRustBuildConstants.ADDED_BINARIES
        )
        latest = GuestRustBuildConstants.ADDED_BINARIES[-2:]
        # The time tools arrived together, so the immediate predecessor lacks both.
        self.assertEqual(latest, ("nvx-time", "nvx-time-probe"))
        predecessor = tuple(
            binary
            for binary in GuestRustBuildConstants.BINARIES
            if binary not in latest
        )
        for case, previous in (
            ("build without the added executables", earlier),
            ("build without the latest added executables", predecessor),
            ("build from before nvx-port-io retired", (*earlier, "nvx-port-io")),
        ):
            with (
                self.subTest(case),
                tempfile.TemporaryDirectory(
                    prefix=".test-guest-binary-added-",
                    dir=BuildConstants.REPO_ROOT,
                ) as temporary,
            ):
                root = Path(temporary)
                output = root / "output"
                _write_build_output(output, previous, recorded=previous)
                with _fake_guest_build():
                    result = guest_binaries.build_guest_binaries(output)
                self.assertEqual(
                    {path.name for path in output.iterdir()},
                    {
                        GuestRustBuildConstants.BUILD_MANIFEST_NAME,
                        *GuestRustBuildConstants.BINARIES,
                    },
                )
                for binary in GuestRustBuildConstants.BINARIES:
                    self.assertEqual((output / binary).read_bytes(), binary.encode())
                manifest = json.loads(
                    (output / GuestRustBuildConstants.BUILD_MANIFEST_NAME).read_text(
                        encoding="utf-8"
                    )
                )
                self.assertEqual(manifest["binaries"], result.binaries)
                self.assertFalse(any(root.glob(".output.work-*")))

    def test_rebuild_refuses_unrecorded_or_unrelated_files(self):
        current = GuestRustBuildConstants.BINARIES
        previous = (*current, "nvx-port-io")
        unrelated = (*previous, "uncommitted.txt")
        earlier = tuple(
            binary
            for binary in current
            if binary not in GuestRustBuildConstants.ADDED_BINARIES
        )
        for case, binaries, recorded in (
            ("unrecorded retired executable", previous, current),
            ("recorded unrelated file", unrelated, unrelated),
            ("recorded missing added executable", earlier, current),
        ):
            with (
                self.subTest(case),
                tempfile.TemporaryDirectory(
                    prefix=".test-guest-binary-unrecorded-",
                    dir=BuildConstants.REPO_ROOT,
                ) as temporary,
            ):
                output = Path(temporary) / "output"
                _write_build_output(output, binaries, recorded=recorded)
                before = _contents(output)
                with (
                    _fake_guest_build() as cargo,
                    self.assertRaisesRegex(
                        guest_binaries.ScriptError, "not owned by this builder"
                    ),
                ):
                    guest_binaries.build_guest_binaries(output)
                cargo.assert_not_called()
                self.assertEqual(_contents(output), before)

    def test_inspection_refuses_an_earlier_build(self):
        # A build upgrades these outputs in place, but none is a current build.
        earlier = tuple(
            binary
            for binary in GuestRustBuildConstants.BINARIES
            if binary not in GuestRustBuildConstants.ADDED_BINARIES
        )
        retired = (*GuestRustBuildConstants.BINARIES, "nvx-port-io")
        for case, binaries in (
            ("build without the added executables", earlier),
            ("build from before nvx-port-io retired", retired),
        ):
            with (
                self.subTest(case),
                tempfile.TemporaryDirectory(
                    prefix=".test-guest-binary-inspect-",
                    dir=BuildConstants.REPO_ROOT,
                ) as temporary,
            ):
                output = Path(temporary) / "output"
                _write_build_output(output, binaries, recorded=binaries)
                before = _contents(output)
                with (
                    _fake_guest_build(),
                    self.assertRaisesRegex(
                        guest_binaries.ScriptError, "not a current build"
                    ),
                ):
                    guest_binaries.inspect_guest_binaries(output)
                self.assertEqual(_contents(output), before)

    def test_initialization_failure_removes_allocated_work_directory(self):
        with tempfile.TemporaryDirectory(
            prefix=".test-guest-binary-initialization-",
            dir=BuildConstants.REPO_ROOT,
        ) as temporary:
            root = Path(temporary)
            output = root / "output"
            with (
                patch.object(guest_binaries.sys, "platform", "linux"),
                patch.object(
                    guest_binaries,
                    "guest_binary_source_sha256",
                    side_effect=guest_binaries.ScriptError("source failure"),
                ),
                self.assertRaisesRegex(guest_binaries.ScriptError, "source failure"),
            ):
                guest_binaries.build_guest_binaries(output)
            self.assertFalse(any(root.glob(".output.work-*")))

    def test_test_command_uses_the_pinned_musl_configuration(self):
        with tempfile.TemporaryDirectory(
            prefix=".test-guest-binary-run-",
            dir=BuildConstants.REPO_ROOT,
        ) as temporary:
            directory = Path(temporary)
            for binary in GuestRustBuildConstants.BINARIES:
                (directory / binary).write_bytes(b"binary")
            (directory / GuestRustBuildConstants.BUILD_MANIFEST_NAME).write_text(
                "{}\n", encoding="utf-8"
            )
            with (
                patch.object(guest_binaries.sys, "platform", "linux"),
                patch.object(guest_binaries, "require_tool", return_value="cargo"),
                patch.object(guest_binaries, "run_checked") as run,
            ):
                guest_binaries.test_guest_binaries(directory)
            raw_command = cast(list[str | os.PathLike[str]], run.call_args.args[0])
            command = [os.fspath(part) for part in raw_command]
            self.assertEqual(command[:3], ["cargo", "+1.95.0", "test"])
            self.assertIn("--locked", command)
            self.assertIn(GuestRustBuildConstants.TARGET, command)
            self.assertIn("--no-default-features", command)
            self.assertIn(GuestRustBuildConstants.FEATURE, command)
            self.assertEqual(
                run.call_args.kwargs["env"]["NVX_GUEST_BINARIES"],
                str(directory.resolve()),
            )


def _tool(name: str) -> str:
    """Stand for require_tool by returning the bare tool name."""
    return name


class GuestBinaryCheckTests(unittest.TestCase):
    def _run_check(
        self, toolchain: str | None = None
    ) -> list[tuple[list[str], Path, dict[str, str]]]:
        """Run the check with an inherited binaries directory, without Cargo."""
        with (
            patch.object(guest_binaries.sys, "platform", "linux"),
            patch.object(guest_binaries, "require_tool", side_effect=_tool),
            patch.object(guest_binaries, "run_checked") as run,
            patch.dict(
                os.environ,
                {GuestRustBuildConstants.BINARIES_VARIABLE: "/elsewhere"},
            ),
        ):
            guest_binaries.check_guest_binaries(toolchain)
        calls: list[tuple[list[str], Path, dict[str, str]]] = []
        for call in run.call_args_list:
            command = cast(list[str | os.PathLike[str]], call.args[0])
            calls.append(
                (
                    [os.fspath(part) for part in command],
                    cast(Path, call.kwargs["cwd"]),
                    cast(dict[str, str], call.kwargs["env"]),
                )
            )
        return calls

    def test_check_lints_and_tests_the_guest_feature_for_musl(self):
        calls = self._run_check()
        toolchain = GuestRustBuildConstants.TOOLCHAIN
        target = GuestRustBuildConstants.TARGET
        cargo = ["cargo", f"+{toolchain}"]
        guest = [
            "--locked",
            "--target",
            target,
            "--no-default-features",
            "--features",
            "guest",
        ]
        self.assertEqual(
            [command for command, _, _ in calls],
            [
                ["rustup", "target", "add", "--toolchain", toolchain, target],
                [*cargo, "clippy", *guest, "--all-targets", "--", "-D", "warnings"],
                [*cargo, "test", *guest],
            ],
        )
        for _, cwd, environment in calls:
            self.assertEqual(cwd, GuestRustBuildConstants.DIRECTORY)
            # The integration tests run the binaries built from the checkout.
            self.assertNotIn(GuestRustBuildConstants.BINARIES_VARIABLE, environment)

    def test_check_uses_the_selected_toolchain_or_the_pin(self):
        pin = GuestRustBuildConstants.TOOLCHAIN
        for requested, expected in ((None, pin), ("", pin), ("1.96.0", "1.96.0")):
            with self.subTest(requested=requested):
                commands = [command for command, _, _ in self._run_check(requested)]
                self.assertEqual(commands[0][3:5], ["--toolchain", expected])
                self.assertEqual(
                    {tuple(command[:2]) for command in commands[1:]},
                    {("cargo", f"+{expected}")},
                )

    def test_check_runs_only_on_linux(self):
        with (
            patch.object(guest_binaries.sys, "platform", "win32"),
            patch.object(guest_binaries, "run_checked") as run,
            self.assertRaisesRegex(guest_binaries.ScriptError, "only on Linux"),
        ):
            guest_binaries.check_guest_binaries()
        run.assert_not_called()


class GuestBinaryCliTests(unittest.TestCase):
    def test_check_command_passes_the_selected_toolchain(self):
        for argv, toolchain in (
            (["check-guest-binaries"], None),
            (["check-guest-binaries", "--toolchain", "1.96.0"], "1.96.0"),
        ):
            with self.subTest(argv=argv):
                args = nvx.parse_args(argv)
                with patch.object(nvx, "check_guest_binaries") as check:
                    args.handler(args)
                check.assert_called_once_with(toolchain)

    def test_build_command_reports_digests(self):
        args = nvx.parse_args(
            ["build-guest-binaries", "--output", "build/guest-binaries"]
        )
        result = guest_binaries.GuestBinaryBuildResult(
            source_sha256="a" * 64,
            binaries={"nvx-exit": "b" * 64},
        )
        stdout = io.StringIO()
        with (
            patch.object(nvx, "build_guest_binaries", return_value=result) as build,
            contextlib.redirect_stdout(stdout),
        ):
            args.handler(args)
        build.assert_called_once_with(Path("build/guest-binaries"))
        self.assertEqual(
            json.loads(stdout.getvalue()),
            {
                "source_sha256": result.source_sha256,
                "binaries": result.binaries,
            },
        )

    def test_test_command_uses_the_selected_directory(self):
        args = nvx.parse_args(
            ["test-guest-binaries", "--binaries", "build/guest-binaries"]
        )
        with patch.object(nvx, "test_guest_binaries") as test:
            args.handler(args)
        test.assert_called_once_with(Path("build/guest-binaries"))


if __name__ == "__main__":
    unittest.main()

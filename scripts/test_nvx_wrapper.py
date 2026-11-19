#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import contextlib
import io
import itertools
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
import check_markdown_links  # noqa: E402
import nvx  # noqa: E402
from nvx_tools import common, guests, nvx_cli  # noqa: E402
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
    GuestRustBuildConstants,
    KernelBuildConstants,
)
from nvx_tools_testing import elf_file as _elf_file  # noqa: E402
from nvx_tools_testing import (
    without_nvx_variable as _without_nvx_variable,  # noqa: E402
)


def _completed(
    returncode: int, stdout: bytes = b"", stderr: bytes = b""
) -> subprocess.CompletedProcess[bytes]:
    return subprocess.CompletedProcess(["nvx"], returncode, stdout, stderr)


class NvxTests(unittest.TestCase):
    def test_nvx_variable_names_the_executable(self):
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "nvx"
            binary.write_bytes(b"")
            with (
                patch.dict(os.environ, {"NVX": str(binary)}),
                patch.object(nvx_cli, "build_binary") as build,
            ):
                self.assertEqual(nvx_cli.require_binary(), binary.absolute())
            build.assert_not_called()
            missing = Path(temporary) / "missing"
            with (
                patch.dict(os.environ, {"NVX": str(missing)}),
                self.assertRaisesRegex(
                    common.ScriptError, "NVX does not name an nvx executable"
                ),
            ):
                nvx_cli.require_binary()

    def test_the_checkout_binary_runs_without_a_build(self):
        self.assertEqual(
            nvx_cli.binary_path(),
            BuildConstants.REPO_ROOT
            / "nvx"
            / "target"
            / "release"
            / ("nvx.exe" if os.name == "nt" else "nvx"),
        )
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "nvx"
            binary.write_bytes(b"")
            with (
                _without_nvx_variable(),
                patch.object(nvx_cli, "binary_path", return_value=binary),
                patch("subprocess.run") as run,
            ):
                self.assertEqual(nvx_cli.require_binary(), binary)
            run.assert_not_called()

    def test_cargo_builds_a_missing_binary_onto_standard_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "nvx"

            def build(
                command: tuple[str, ...], **_: object
            ) -> subprocess.CompletedProcess[bytes]:
                binary.write_bytes(b"")
                return _completed(0)

            stdout = io.StringIO()
            stderr = io.StringIO()
            with (
                _without_nvx_variable(),
                patch.object(nvx_cli, "binary_path", return_value=binary),
                patch("shutil.which", return_value="cargo"),
                patch("subprocess.run", side_effect=build) as run,
                patch.object(nvx_cli, "_stderr_target", return_value=2),
                patch.object(nvx_cli, "record_provenance") as record,
                contextlib.redirect_stdout(stdout),
                contextlib.redirect_stderr(stderr),
            ):
                self.assertEqual(nvx_cli.require_binary(), binary)
        run.assert_called_once()
        # The build records the provenance of what it built, for the host.
        record.assert_called_once_with()
        self.assertEqual(
            run.call_args.args[0],
            (
                "cargo",
                "build",
                "--release",
                "--locked",
                "--features",
                "cli",
                "--manifest-path",
                "nvx/Cargo.toml",
                "--target-dir",
                "nvx/target",
            ),
        )
        # From the repository root, so that rust-toolchain.toml applies, and
        # with Cargo's output on standard error, away from the command's.
        self.assertEqual(run.call_args.kwargs["cwd"], BuildConstants.REPO_ROOT)
        self.assertEqual(run.call_args.kwargs["stdout"], 2)
        self.assertEqual(stdout.getvalue(), "")
        self.assertEqual(
            stderr.getvalue(),
            ">> cargo build --release --locked --features cli --manifest-path "
            "nvx/Cargo.toml --target-dir nvx/target\n",
        )

    def test_build_output_goes_to_the_descriptor_of_standard_error(self):
        stream = MagicMock()
        stream.fileno.return_value = 2
        with patch("sys.stderr", stream):
            self.assertEqual(nvx_cli._stderr_target(), 2)
        stream.flush.assert_called_once_with()
        with patch("sys.stderr", new_callable=io.StringIO):
            self.assertEqual(nvx_cli._stderr_target(), subprocess.DEVNULL)

    def test_missing_cargo_names_every_way_to_get_nvx(self):
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "nvx"
            with (
                _without_nvx_variable(),
                patch.object(nvx_cli, "binary_path", return_value=binary),
                patch("shutil.which", return_value=None),
                patch("subprocess.run") as run,
                self.assertRaises(common.ScriptError) as raised,
            ):
                nvx_cli.require_binary()
        run.assert_not_called()
        message = str(raised.exception)
        for expected in (
            f"nvx is not built at {binary}, and Cargo is not on PATH",
            "set NVX to an nvx executable",
            "https://rustup.rs",
            "cargo build --release --locked --features cli --manifest-path "
            "nvx/Cargo.toml --target-dir nvx/target",
        ):
            self.assertIn(expected, message)
        self.assertNotIn("download", message)

    def test_release_targets_follow_the_operating_system(self):
        self.assertEqual(
            nvx_cli.release_target(),
            "x86_64-pc-windows-msvc"
            if os.name == "nt"
            else "x86_64-unknown-linux-musl",
        )
        self.assertEqual(nvx_cli.FEATURES, ("cli",))
        # BUILD_COMMAND builds the features that releases ship.
        features = nvx_cli.BUILD_COMMAND.index("--features") + 1
        self.assertEqual(nvx_cli.BUILD_COMMAND[features].split(","), ["cli"])

    def test_source_digest_covers_the_build_inputs_in_any_line_endings(self):
        relative = [
            path.relative_to(BuildConstants.REPO_ROOT).as_posix()
            for path in nvx_cli.source_files()
        ]
        self.assertEqual(relative, sorted(relative))
        for name in (
            "nvx/Cargo.toml",
            "nvx/Cargo.lock",
            "nvx/src/bin/nvx.rs",
            "rust-toolchain.toml",
        ):
            self.assertIn(name, relative)
        # Tests and documentation build no part of the executable.
        for name in relative:
            self.assertNotRegex(name, r"^nvx/(tests/|README\.md$)")
        self.assertEqual(nvx_cli.source_sha256(), nvx_cli.source_sha256())

        with tempfile.TemporaryDirectory() as temporary:
            checkouts = [Path(temporary) / "lf", Path(temporary) / "crlf"]
            for checkout, newline in zip(checkouts, (b"\n", b"\r\n"), strict=True):
                for name in (
                    "nvx/Cargo.toml",
                    "nvx/Cargo.lock",
                    "nvx/src/lib.rs",
                    "nvx/src/bin/nvx.rs",
                    "rust-toolchain.toml",
                ):
                    path = checkout / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(newline.join((name.encode(), b"line", b"")))
            digest = nvx_cli.source_sha256(checkouts[0])
            # Checkouts differ only in line endings, as on Windows.
            self.assertEqual(nvx_cli.source_sha256(checkouts[1]), digest)
            source = checkouts[0] / "nvx" / "src" / "lib.rs"
            source.write_bytes(b"changed\n")
            self.assertNotEqual(nvx_cli.source_sha256(checkouts[0]), digest)
            # A new source file changes the digest too.
            (checkouts[1] / "nvx" / "src" / "new.rs").write_bytes(b"")
            self.assertNotEqual(nvx_cli.source_sha256(checkouts[1]), digest)
            shutil.rmtree(checkouts[1] / "nvx" / "src")
            with self.assertRaisesRegex(
                common.ScriptError, "nvx source directory not found"
            ):
                nvx_cli.source_sha256(checkouts[1])

    def test_host_target_is_the_host_of_the_pinned_rust(self):
        version = (
            "rustc 1.95.0 (abc 2026-01-01)\nbinary: rustc\n"
            "host: x86_64-unknown-linux-gnu\nrelease: 1.95.0\n"
        )
        with patch(
            "subprocess.run",
            return_value=subprocess.CompletedProcess(["rustc"], 0, version, ""),
        ) as run:
            self.assertEqual(nvx_cli.host_target(), "x86_64-unknown-linux-gnu")
        self.assertEqual(run.call_args.args[0], ("rustc", "-vV"))
        # From the repository root, so that rust-toolchain.toml applies.
        self.assertEqual(run.call_args.kwargs["cwd"], BuildConstants.REPO_ROOT)
        for result in (
            subprocess.CompletedProcess(["rustc"], 1, "", "no rustc"),
            subprocess.CompletedProcess(["rustc"], 0, "release: 1.95.0\n", ""),
        ):
            with (
                self.subTest(result=result),
                patch("subprocess.run", return_value=result),
                self.assertRaisesRegex(common.ScriptError, "host target"),
            ):
                nvx_cli.host_target()
        with (
            patch("subprocess.run", side_effect=OSError("missing")),
            self.assertRaisesRegex(common.ScriptError, "failed to run rustc"),
        ):
            nvx_cli.host_target()

    def test_provenance_binds_the_executable_that_nvx_py_runs(self):
        linux = nvx_cli.RELEASE_TARGETS["linux"]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "nvx"
            other = root / "other-nvx"
            provenance = root / "build" / nvx_cli.PROVENANCE_NAME
            with (
                patch.object(nvx_cli, "binary_path", return_value=binary),
                patch.object(nvx_cli, "provenance_path", return_value=provenance),
                patch.object(nvx_cli, "host_target", return_value="host-triple"),
            ):
                with (
                    _without_nvx_variable(),
                    self.assertRaisesRegex(
                        common.ScriptError,
                        f"nvx is not built at {re.escape(str(binary))}",
                    ),
                ):
                    nvx_cli.record_provenance()
                self.assertFalse(provenance.exists())

                binary.write_bytes(b"nvx")
                with _without_nvx_variable():
                    self.assertEqual(nvx_cli.record_provenance(), provenance)
                self.assertEqual(
                    json.loads(provenance.read_text(encoding="utf-8")),
                    {
                        "format": 1,
                        "source_sha256": nvx_cli.source_sha256(),
                        "toolchain": GuestRustBuildConstants.TOOLCHAIN,
                        "target": "host-triple",
                        "features": ["cli"],
                        "executable_sha256": common.sha256_file(binary),
                    },
                )

                # The executable that NVX names is the one that nvx.py
                # runs, and so the one that the record describes.
                other.write_bytes(b"other nvx")
                with patch.dict(os.environ, {nvx_cli.VARIABLE: str(other)}):
                    nvx_cli.record_provenance("x86_64-pc-windows-msvc")
                record = json.loads(provenance.read_text(encoding="utf-8"))
                self.assertEqual(record["target"], "x86_64-pc-windows-msvc")
                self.assertEqual(record["executable_sha256"], common.sha256_file(other))

                # A record for the Linux release target needs the static
                # executable that such a build gives.
                with (
                    _without_nvx_variable(),
                    self.assertRaisesRegex(
                        common.ScriptError, f"invalid {linux} nvx executable"
                    ),
                ):
                    nvx_cli.record_provenance(linux)
                binary.write_bytes(
                    _elf_file(((1, b"code"), (3, b"/lib/ld-musl.so.1\0")), elf_type=3)
                )
                with (
                    _without_nvx_variable(),
                    self.assertRaisesRegex(common.ScriptError, "is not static"),
                ):
                    nvx_cli.record_provenance(linux)
                binary.write_bytes(_elf_file(elf_type=3))
                with _without_nvx_variable():
                    nvx_cli.record_provenance(linux)
                self.assertEqual(
                    json.loads(provenance.read_text(encoding="utf-8"))["target"],
                    linux,
                )

    def test_record_command_records_the_provenance(self):
        args = nvx.parse_args(
            ["record-nvx-provenance", "--target", "x86_64-unknown-linux-musl"]
        )
        self.assertEqual(args.target, "x86_64-unknown-linux-musl")
        self.assertIsNone(nvx.parse_args(["record-nvx-provenance"]).target)
        recorded = Path("build") / nvx_cli.PROVENANCE_NAME
        with (
            patch.object(nvx_cli, "record_provenance", return_value=recorded) as record,
            contextlib.redirect_stdout(io.StringIO()) as stdout,
        ):
            args.handler(args)
        record.assert_called_once_with("x86_64-unknown-linux-musl")
        self.assertEqual(
            stdout.getvalue(), f">> recorded the provenance of nvx in {recorded}\n"
        )

    def test_failed_builds_report_cargo(self):
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "nvx"
            for returncode, message in (
                (101, r"Cargo failed to build nvx \(exit 101\)"),
                (0, "Cargo did not build"),
            ):
                with (
                    self.subTest(returncode=returncode),
                    _without_nvx_variable(),
                    patch.object(nvx_cli, "binary_path", return_value=binary),
                    patch("shutil.which", return_value="cargo"),
                    patch("subprocess.run", return_value=_completed(returncode)),
                    patch.object(nvx_cli, "record_provenance") as record,
                    contextlib.redirect_stderr(io.StringIO()),
                    self.assertRaisesRegex(common.ScriptError, message),
                ):
                    nvx_cli.require_binary()
                record.assert_not_called()

    def test_command_lines_run_nvx_on_the_checkout_or_another_root(self):
        # The microVM scenarios run nvx itself, on this checkout's
        # artifacts, or on another artifact root, such as one of another
        # runtime identity.
        binary = Path("bin") / "nvx"
        with patch.object(nvx_cli, "require_binary", return_value=binary):
            self.assertEqual(
                nvx_cli.command_line(("sandbox", "exec")),
                [
                    os.fspath(binary),
                    "--root",
                    os.fspath(BuildConstants.REPO_ROOT),
                    "sandbox",
                    "exec",
                ],
            )
            other = Path("other-runtime")
            self.assertEqual(
                nvx_cli.command_line(("run",), root=other),
                [os.fspath(binary), "--root", os.fspath(other), "run"],
            )

    def test_unstartable_nvx_is_an_error(self):
        with (
            patch.object(nvx_cli, "require_binary", return_value=Path("nvx")),
            patch("subprocess.run", side_effect=OSError("no such file")),
            self.assertRaisesRegex(
                common.ScriptError, "failed to run nvx: no such file"
            ),
        ):
            nvx_cli.query(["artifacts"])

    def test_queries_report_the_codes_and_messages_of_nvx(self):
        for stderr, code, message in (
            (
                b"error[unsupported]: invalid egress policy\r\n",
                "unsupported",
                "invalid egress policy",
            ),
            # nvx prints a line break of a message as `\n`.
            (b"error[io]: first\\nsecond\n", "io", "first\\nsecond"),
            (
                b"usage: nvx egress compile [-h] PATH\nnvx egress compile: "
                b"error[argument-missing]: the following arguments are required: PATH\n",
                "argument-missing",
                "the following arguments are required: PATH",
            ),
            (b"usage: nvx\n", None, "usage: nvx"),
            (b"", None, "nvx exited with status 1"),
        ):
            with (
                self.subTest(stderr=stderr),
                patch.object(nvx_cli, "require_binary", return_value=Path("nvx")),
                patch("subprocess.run", return_value=_completed(1, stderr=stderr)),
                self.assertRaises(nvx_cli.NvxError) as raised,
            ):
                nvx_cli.query(["egress", "compile", "policy.json"])
            self.assertEqual(str(raised.exception), message)
            self.assertEqual(raised.exception.code, code)
            self.assertIsInstance(raised.exception, common.ScriptError)
        with (
            patch.object(nvx_cli, "require_binary", return_value=Path("nvx")),
            patch("subprocess.run", return_value=_completed(0, stdout=b"{")) as run,
            self.assertRaisesRegex(common.ScriptError, "printed invalid JSON"),
        ):
            nvx_cli.query(["artifacts"])
        # A query runs nvx on this checkout's artifacts.
        run.assert_called_once_with(
            ["nvx", "--root", os.fspath(BuildConstants.REPO_ROOT), "artifacts"],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            check=False,
        )

    def test_reported_errors_are_the_last_that_nvx_printed(self):
        # A workload's output precedes the error of sandbox exec.
        self.assertEqual(
            nvx_cli.reported_error(
                b"error[workload]: printed by the workload\n"
                b"error[sandbox-exited]: sandbox runtime state is stale\n"
            ),
            nvx_cli.ReportedError("sandbox-exited", "sandbox runtime state is stale"),
        )
        # nvx ends a line that the workload left open before its error.
        self.assertEqual(
            nvx_cli.reported_error(
                b"progress\r\nerror[outcome-report-exists]: "
                b"outcome report already exists: /reports/exec.json\r\n"
            ),
            nvx_cli.ReportedError(
                "outcome-report-exists",
                "outcome report already exists: /reports/exec.json",
            ),
        )
        # A message that names an error marker, as a file of that name does,
        # leaves the error its own code, and the program its own name.
        self.assertEqual(
            nvx_cli.reported_error(
                b"error[io]: failed to read egress policy file: "
                b"error[unsupported]: missing.json\n"
            ),
            nvx_cli.ReportedError(
                "io",
                "failed to read egress policy file: error[unsupported]: missing.json",
            ),
        )
        self.assertEqual(
            nvx_cli.reported_error(
                b"nvx sandbox: error[argument-invalid]: argument --layer: "
                b"error[x]: y.erofs\n"
            ),
            nvx_cli.ReportedError(
                "argument-invalid", "argument --layer: error[x]: y.erofs"
            ),
        )
        # nvx prints a line break of a value that a message names as `\n`,
        # so that the message holds no line in the form of an error.
        self.assertEqual(
            nvx_cli.reported_error(
                b"error[artifact-missing]: OpenVMM release binary not found: "
                b"/tmp/\\nerror[unsupported]: openvmm\n"
            ),
            nvx_cli.ReportedError(
                "artifact-missing",
                "OpenVMM release binary not found: /tmp/\\nerror[unsupported]: openvmm",
            ),
        )
        self.assertEqual(
            nvx_cli.reported_error(
                "usage: nvx sandbox [{run,provision,start,exec,stop,deprovision}] "
                "[OPTIONS]\r\nnvx sandbox: error[argument-invalid]: "
                "argument --workload-user: --workload-user UID must be between 1 and "
                "4294967294\r\n"
            ),
            nvx_cli.ReportedError(
                "argument-invalid",
                "argument --workload-user: --workload-user UID must be between 1 and "
                "4294967294",
            ),
        )
        for stderr in (
            b"",
            b"warning: sandbox ran under another runtime identity\n",
            b"Interrupted\n",
            b"error: the form without a code\n",
            b"error[Not-A-Code]: message\n",
            b"text before error[unsupported]: on the same line, not a prefix\n",
        ):
            with self.subTest(stderr=stderr):
                self.assertIsNone(nvx_cli.reported_error(stderr))

    def test_usage_documents_every_error_code_of_nvx(self):
        # nvx/README.md lists every code that nvx prints, which a test of
        # nvx checks, and doc/usage.md lists the same codes with the status
        # with which nvx exits: 2 for usage errors and 1 for any other.
        def table(path: Path) -> list[tuple[str, str]]:
            lines = path.read_text(encoding="utf-8").splitlines()
            rows: list[tuple[str, str]] = []
            for line in lines[lines.index("### Error codes") + 1 :]:
                if line.startswith("#"):
                    break
                cells = [cell.strip() for cell in line.strip("|").split("|")]
                if len(cells) > 2 and re.fullmatch(r"`[a-z-]+`", cells[0]):
                    rows.append((cells[0].strip("`"), cells[1]))
            return rows

        readme = table(nvx_cli.DIRECTORY / "README.md")
        usage = table(BuildConstants.REPO_ROOT / "doc" / "usage.md")
        self.assertGreater(len(readme), 8)
        self.assertEqual(
            sorted(code for code, _ in usage), sorted(code for code, _ in readme)
        )
        for code, status in usage:
            with self.subTest(code=code):
                self.assertEqual(status, "2" if code.startswith("argument-") else "1")

    def test_readme_gives_each_module_a_short_row_and_a_section(self):
        # The module table of nvx/README.md says in one line, of at most 200
        # characters, what each module is for, and links to the module's
        # section after the table, which holds the module's details.
        readme = nvx_cli.DIRECTORY / "README.md"
        lines = readme.read_text(encoding="utf-8").splitlines()
        table = lines.index("| Module | Purpose |")
        rows: list[str] = []
        for line in lines[table + 2 :]:
            if not line.startswith("|"):
                break
            rows.append(line)
        sections = {
            line.removeprefix("### "): index
            for index, line in enumerate(lines)
            if line.startswith("### ")
        }
        self.assertGreater(len(rows), 25)
        for row in rows:
            link = row.split(" | ")[0].removeprefix("| ")
            with self.subTest(module=link):
                self.assertLessEqual(
                    len(row), 200, "a module's details go in its section"
                )
                match = re.fullmatch(r"\[(.+)\]\(#(.+)\)", link)
                assert match is not None, "a row links to its module's section"
                module, anchor = match.group(1), match.group(2)
                self.assertGreater(
                    sections.get(module, table),
                    table,
                    "the module's section follows the table",
                )
                self.assertEqual(
                    anchor, check_markdown_links.github_slug(module.replace("`", ""))
                )

    @unittest.skipUnless(os.environ.get("NVX"), "set NVX to an nvx executable to run")
    def test_usage_documents_every_option_of_nvx(self):
        # The nvx chapter of doc/usage.md documents each option of nvx
        # run and sandbox, as the rest documents those of each nvx.py command.
        option = re.compile(r"(?<![\w-])--[a-z0-9]+(?:-[a-z0-9]+)*(?![\w*-])")
        document = (BuildConstants.REPO_ROOT / "doc" / "usage.md").read_text(
            encoding="utf-8"
        )
        chapter = document.split("\n## nvx\n", 1)[1].split("\n## ", 1)[0]
        for command in ("run", "sandbox"):
            with self.subTest(command=command):
                section = re.search(
                    rf"^### `{command}`\n(.*?)(?=^### |\Z)",
                    chapter,
                    re.MULTILINE | re.DOTALL,
                )
                assert section is not None, command
                completed = subprocess.run(
                    [os.environ["NVX"], command, "--help"],
                    capture_output=True,
                    text=True,
                    timeout=60,
                    check=True,
                )
                self.assertEqual(
                    set(option.findall(section.group(1))),
                    set(option.findall(completed.stdout)) - {"--help"},
                )

    @unittest.skipUnless(os.environ.get("NVX"), "set NVX to an nvx executable to run")
    def test_run_boots_each_guest_with_its_default_memory(self):
        # nvx run boots each guest with the memory that guests.py gives
        # test-microvm, as the OpenVMM command of its dry run shows. A dry run
        # launches nothing, so placeholders stand in for the artifacts.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            build_dir = BuildConstants.BUILD_DIR.relative_to(BuildConstants.REPO_ROOT)
            for relative in (
                common.openvmm_binary_path().relative_to(BuildConstants.REPO_ROOT),
                build_dir / KernelBuildConstants.BINARY_NAME,
                *(
                    build_dir / guests.guest_descriptor(name).initramfs_name
                    for name in guests.GUEST_NAMES
                ),
            ):
                (root / relative).parent.mkdir(parents=True, exist_ok=True)
                (root / relative).write_bytes(b"placeholder\n")
            with patch.dict(os.environ):
                os.environ.pop("NVX_OPENVMM", None)
                for name in guests.GUEST_NAMES:
                    default = guests.guest_descriptor(name).default_memory_mib
                    with self.subTest(guest=name):
                        completed = subprocess.run(
                            nvx_cli.command_line(
                                ("run", "--guest", name, "--dry-run"), root=root
                            ),
                            stdin=subprocess.DEVNULL,
                            capture_output=True,
                            text=True,
                            timeout=60,
                            check=True,
                        )
                        command = completed.stdout.splitlines()[0]
                        self.assertTrue(command.startswith(">> "), command)
                        words = command.split()
                        self.assertEqual(
                            [
                                value
                                for option, value in itertools.pairwise(words)
                                if option == "--memory"
                            ],
                            [f"{default}M"],
                        )

    def test_egress_policies_compile_to_the_rules_that_nvx_prints(self):
        with patch.object(
            nvx_cli,
            "query",
            return_value={"allow": ["192.0.2.0/24:tcp:80"], "deny": []},
        ) as query:
            self.assertEqual(
                nvx_cli.compile_egress_policy(Path("policy.json")),
                nvx_cli.EgressRules(allow=("192.0.2.0/24:tcp:80",), deny=()),
            )
        query.assert_called_once_with(("egress", "compile", "--", "policy.json"))
        # The path follows "--", so that nvx reads one that starts with "-"
        # as the path rather than as an option.
        with patch.object(
            nvx_cli, "query", return_value={"allow": [], "deny": []}
        ) as query:
            nvx_cli.compile_egress_policy(Path("-policy.json"))
        query.assert_called_once_with(("egress", "compile", "--", "-policy.json"))
        invalid: tuple[object, ...] = ([], {"allow": []}, {"allow": [1], "deny": []})
        for document in invalid:
            with (
                self.subTest(document=document),
                patch.object(nvx_cli, "query", return_value=document),
                self.assertRaises(common.ScriptError),
            ):
                nvx_cli.compile_egress_policy(Path("policy.json"))

    @unittest.skipUnless(os.environ.get("NVX"), "set NVX to an nvx executable to run")
    def test_the_real_nvx_prints_what_nvx_py_reads(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            policy = root / "policy.json"
            policy.write_text(
                json.dumps(
                    {
                        "allow": [
                            {"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]}
                        ],
                        "deny": [{"cidr": "198.51.100.0/24", "protocol": "icmp"}],
                    }
                ),
                encoding="utf-8",
            )
            with patch.object(nvx_cli, "ROOT", root), patch.dict(os.environ):
                os.environ.pop("NVX_OPENVMM", None)
                self.assertEqual(
                    nvx_cli.compile_egress_policy(policy),
                    nvx_cli.EgressRules(
                        allow=("192.0.2.0/25",), deny=("198.51.100.0/24:icmp",)
                    ),
                )
                # nvx refuses a policy that is not an object with the stable
                # code of its error, and nvx.py reads the message, one line
                # without that code, whatever its wording.
                policy.write_text("[]", encoding="utf-8")
                with self.assertRaises(nvx_cli.NvxError) as refused:
                    nvx_cli.compile_egress_policy(policy)
                self.assertEqual(refused.exception.code, "unsupported")
                self.assertRegex(str(refused.exception), r"\A(?!error\[)[^\r\n]+\Z")
                # A caller may pass a path as a command line gave it, which may
                # be relative and start with "-".
                (root / "-policy.json").write_text(
                    json.dumps({"allow": [{"cidr": "192.0.2.0/24"}]}),
                    encoding="utf-8",
                )
                previous = os.getcwd()
                os.chdir(root)
                try:
                    dashed = nvx_cli.compile_egress_policy(Path("-policy.json"))
                finally:
                    os.chdir(previous)
                self.assertEqual(
                    dashed, nvx_cli.EgressRules(allow=("192.0.2.0/24",), deny=())
                )


if __name__ == "__main__":
    unittest.main()

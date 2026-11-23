#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import contextlib
import io
import struct
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import common, elf  # noqa: E402
from nvx_tools_testing import elf_file as _elf_file  # noqa: E402


def _dynamic_entries(*entries: tuple[int, int]) -> bytes:
    return b"".join(struct.pack("<qQ", tag, value) for tag, value in entries)


class StaticElfTests(unittest.TestCase):
    """The ELF check of the guest binaries, which run without a dynamic loader."""

    INTERPRETER = (3, b"/lib/ld-musl-x86_64.so.1\0")
    NEEDED = (2, _dynamic_entries((1, 1), (0, 0)))

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def _write(self, name: str, data: bytes) -> Path:
        path = self.root / name
        path.write_bytes(data)
        return path

    def test_static_and_static_pie_executables_pass(self):
        executable = elf.read_elf_executable(self._write("exit", _elf_file()))
        self.assertTrue(executable.static)
        self.assertEqual(
            (
                executable.elf_type,
                executable.interpreter,
                executable.needed,
                executable.dynamic,
            ),
            ("EXEC", None, 0, False),
        )
        self.assertEqual(
            executable.describe(),
            f"{executable.path}: ELF EXEC, {executable.size} bytes, static",
        )
        # A static PIE relocates itself through its dynamic section; entries
        # after DT_NULL do not count.
        dynamic = _dynamic_entries((7, 0x1000), (0, 0), (1, 1))
        pie = elf.read_elf_executable(
            self._write("pie", _elf_file(((1, b"code"), (2, dynamic)), elf_type=3))
        )
        self.assertTrue(pie.static)
        self.assertEqual((pie.elf_type, pie.dynamic), ("DYN", True))
        self.assertTrue(pie.describe().endswith(" bytes, static-pie"))

    def test_an_interpreter_or_a_needed_library_fails(self):
        dynamic = elf.read_elf_executable(
            self._write(
                "dynamic",
                _elf_file(((1, b"code"), self.INTERPRETER, self.NEEDED), elf_type=3),
            )
        )
        self.assertFalse(dynamic.static)
        self.assertEqual(
            (dynamic.interpreter, dynamic.needed), ("/lib/ld-musl-x86_64.so.1", 1)
        )
        self.assertIn(
            "dynamic: interpreter /lib/ld-musl-x86_64.so.1, 1 needed libraries",
            dynamic.describe(),
        )
        needed = elf.read_elf_executable(
            self._write("needed", _elf_file(((1, b"code"), self.NEEDED), elf_type=3))
        )
        self.assertFalse(needed.static)

    def test_rejects_files_that_are_not_64_bit_executables(self):
        truncated_segment = _elf_file(((1, b"code"), self.INTERPRETER))[:-4]
        for name, data in (
            ("script", b"#!/bin/sh\nexit 0\n"),
            ("elf32", _elf_file(elf_class=1)),
            ("object", _elf_file(elf_type=1)),
            ("header", _elf_file()[:40]),
            ("segment", truncated_segment),
        ):
            with self.subTest(name=name), self.assertRaises(ValueError):
                elf.read_elf_executable(self._write(name, data))

    def test_an_executable_stack_fails(self):
        # The C build passed -z noexecstack; rustc marks the stack itself, and
        # only a PF_X stack header would make it executable on x86-64.
        for name, stack_flags, executable_stack in (
            ("default", None, False),
            ("rw", 6, False),
            ("rwx", 7, True),
        ):
            with self.subTest(name=name):
                executable = elf.read_elf_executable(
                    self._write(name, _elf_file(stack_flags=stack_flags))
                )
                self.assertTrue(executable.static)
                self.assertEqual(executable.executable_stack, executable_stack)
        writable, executable = self.root / "rw", self.root / "rwx"
        self.assertTrue(
            elf.read_elf_executable(executable)
            .describe()
            .endswith(" bytes, static, executable stack")
        )
        output = io.StringIO()
        with (
            contextlib.redirect_stdout(output),
            self.assertRaisesRegex(
                common.ScriptError, "executables with an executable stack: .*rwx$"
            ),
        ):
            elf.check_static_executables([writable, executable])
        self.assertIn(f"{writable}: ELF EXEC", output.getvalue())
        with (
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()) as error,
        ):
            self.assertEqual(nvx.main(["check-static-elf", str(executable)]), 1)
        self.assertIn("executable stack", error.getvalue())

    def test_check_lists_every_executable_and_fails_on_dynamic_ones(self):
        static = self._write("static", _elf_file())
        dynamic = self._write("dynamic", _elf_file(((1, b"code"), self.INTERPRETER)))
        output = io.StringIO()
        with (
            contextlib.redirect_stdout(output),
            self.assertRaisesRegex(
                common.ScriptError, "not static executables: .*dynamic"
            ),
        ):
            elf.check_static_executables([static, dynamic])
        self.assertIn(f"{static}: ELF EXEC", output.getvalue())
        self.assertIn(f"{dynamic}: ELF EXEC", output.getvalue())

        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(nvx.main(["check-static-elf", str(static)]), 0)
        self.assertEqual(
            output.getvalue(),
            f"{static}: ELF EXEC, {static.stat().st_size} bytes, static\n",
        )
        with (
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()) as error,
        ):
            self.assertEqual(
                nvx.main(["check-static-elf", str(static), str(dynamic)]), 1
            )
        self.assertIn("not static executables", error.getvalue())


if __name__ == "__main__":
    unittest.main()

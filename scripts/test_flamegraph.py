#!/usr/bin/env python3
# Copyright(c) The microvm authors.
# Licensed under the MIT License.

"""
Unit tests for the flamegraph post-processing scripts.

These exercise the pure parsing/merging logic end to end without invoking any external binaries
(xperf/perf/inferno), so they run in CI on both Linux and Windows.
"""

from __future__ import annotations

import hashlib
import os
import re
import sys
import tempfile
import unittest

from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import flamegraph  # noqa: E402
import flamegraph_host as host  # noqa: E402
import flamegraph_merge as merge  # noqa: E402


class WinMatchProcess(unittest.TestCase):
    def setUp(self) -> None:
        self.proc_re = re.compile(r"microvm(?:\.exe)?", re.IGNORECASE)

    def test_pid_exact_match(self) -> None:
        self.assertTrue(
            host._win_match_process("microvm.exe (1234)", self.proc_re, 1234)
        )
        self.assertFalse(
            host._win_match_process("microvm.exe (1234)", self.proc_re, 9999)
        )

    def test_pid_target_rejects_same_name_other_pid(self) -> None:
        # A concurrent process with the same image name but a different PID must not contaminate.
        self.assertFalse(host._win_match_process("microvm.exe (2)", self.proc_re, 1))

    def test_name_fallback_without_pid(self) -> None:
        self.assertTrue(
            host._win_match_process("microvm.exe (1234)", self.proc_re, None)
        )
        self.assertFalse(host._win_match_process("System (4)", self.proc_re, None))


class WinFoldDumper(unittest.TestCase):
    def test_kernel_and_user_fragments_both_survive(self) -> None:
        # A single sample's kernel and user fragments reuse FrameNos 1/2. All Stack records sharing
        # the sample's (timestamp, thread) accumulate in trace order (leaf-first) and are emitted
        # root-first, so neither fragment is dropped.
        lines = [
            "BeginHeader",
            "EndHeader",
            "SampledProfile, 100, microvm.exe (1234), 7, 0xffff",
            "Stack, 100, 7, 1, n/a, kernel_leaf",
            "Stack, 100, 7, 2, n/a, kernel_entry",
            "Stack, 100, 7, 1, n/a, user_leaf",
            "Stack, 100, 7, 2, n/a, user_main",
        ]
        folded, cswitch = host._win_fold_dumper(
            iter(lines), re.compile("microvm"), 1234
        )
        self.assertEqual(cswitch, 0)
        self.assertEqual(len(folded), 1)
        ((stack, count),) = folded.items()
        self.assertEqual(count, 1)
        self.assertEqual(stack, "user_main;user_leaf;kernel_entry;kernel_leaf")

    def test_counts_cswitch_separately_from_samples(self) -> None:
        # A context switch is counted for the scheduling summary and flushes the active sample; it
        # never contributes a frame to the CPU flamegraph.
        lines = [
            "SampledProfile, 100, microvm.exe (1234), 7, 0xffff",
            "Stack, 100, 7, 1, n/a, foo",
            "CSwitch, 102, microvm.exe (1234), 7, 0, a, b, c, idle (0), 0",
        ]
        folded, cswitch = host._win_fold_dumper(
            iter(lines), re.compile("microvm"), 1234
        )
        self.assertEqual(cswitch, 1)
        self.assertEqual(dict(folded), {"foo": 1})

    def test_pid_filter_excludes_same_name_other_pid(self) -> None:
        lines = [
            "SampledProfile, 100, microvm.exe (1), 7, 0xffff",
            "Stack, 100, 7, 1, n/a, keep",
            "SampledProfile, 200, microvm.exe (2), 8, 0xffff",
            "Stack, 200, 8, 1, n/a, drop",
        ]
        folded, _ = host._win_fold_dumper(iter(lines), re.compile("microvm"), 1)
        self.assertEqual(dict(folded), {"keep": 1})

    def test_name_fallback_when_no_pid(self) -> None:
        lines = [
            "SampledProfile, 100, microvm.exe (1), 7, 0xffff",
            "Stack, 100, 7, 1, n/a, foo",
        ]
        folded, _ = host._win_fold_dumper(iter(lines), re.compile("microvm"), None)
        self.assertEqual(dict(folded), {"foo": 1})

    def test_new_sample_flushes_previous(self) -> None:
        lines = [
            "SampledProfile, 100, microvm.exe (1234), 7, 0xffff",
            "Stack, 100, 7, 1, n/a, a",
            "SampledProfile, 200, microvm.exe (1234), 8, 0xffff",
            "Stack, 200, 8, 1, n/a, b",
        ]
        folded, _ = host._win_fold_dumper(iter(lines), re.compile("microvm"), 1234)
        self.assertEqual(dict(folded), {"a": 1, "b": 1})

    def test_keeps_unresolved_frames_as_placeholder(self) -> None:
        # Unresolved frames are kept as `[unknown]` rather than dropped, so distinct stacks are not
        # merged and an all-unknown sample is still counted.
        lines = [
            "SampledProfile, 100, microvm.exe (1234), 7, 0xffff",
            "Stack, 100, 7, 1, n/a, Unknown",
            "Stack, 100, 7, 2, n/a, realfn",
        ]
        folded, _ = host._win_fold_dumper(iter(lines), re.compile("microvm"), 1234)
        self.assertEqual(dict(folded), {"realfn;[unknown]": 1})

    def test_all_unknown_sample_is_counted(self) -> None:
        lines = [
            "SampledProfile, 100, microvm.exe (1234), 7, 0xffff",
            "Stack, 100, 7, 1, n/a, Unknown",
        ]
        folded, _ = host._win_fold_dumper(iter(lines), re.compile("microvm"), 1234)
        self.assertEqual(dict(folded), {"[unknown]": 1})


class Provenance(unittest.TestCase):
    def test_parse_manifest(self) -> None:
        self.assertEqual(
            host._parse_manifest("pid=42\nrun=abc-1\n\n"),
            {"pid": "42", "run": "abc-1"},
        )

    def test_parse_manifest_with_image_dir(self) -> None:
        # The recorder adds an `image_dir` line so the extractor can resolve the VMM's own PDB.
        self.assertEqual(
            host._parse_manifest("pid=42\nrun=abc-1\nimage_dir=C:\\build\\release\n"),
            {"pid": "42", "run": "abc-1", "image_dir": "C:\\build\\release"},
        )

    def test_matching_run_ids_pass(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            guest = Path(d) / "p.folded"
            trace = Path(d) / "p.host.etl"
            guest.with_name(guest.name + ".run").write_text(
                "run=abc\n", encoding="utf-8"
            )
            trace.with_name(trace.name + ".pid").write_text(
                "pid=9\nrun=abc\n", encoding="utf-8"
            )
            self.assertEqual(host._read_run_id(guest), "abc")
            self.assertEqual(host._read_host_manifest(trace).get("pid"), "9")
            self.assertTrue(host._provenance_ok(guest, trace))

    def test_missing_guest_run_id_fails(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            guest = Path(d) / "p.folded"
            trace = Path(d) / "p.host.etl"
            trace.with_name(trace.name + ".pid").write_text(
                "pid=9\nrun=abc\n", encoding="utf-8"
            )
            self.assertFalse(host._provenance_ok(guest, trace))

    def test_missing_host_manifest_fails(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            guest = Path(d) / "p.folded"
            trace = Path(d) / "p.host.etl"
            guest.with_name(guest.name + ".run").write_text(
                "run=abc\n", encoding="utf-8"
            )
            self.assertFalse(host._provenance_ok(guest, trace))

    def test_mismatched_run_ids_fail(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            guest = Path(d) / "p.folded"
            trace = Path(d) / "p.host.etl"
            guest.with_name(guest.name + ".run").write_text(
                "run=abc\n", encoding="utf-8"
            )
            trace.with_name(trace.name + ".pid").write_text(
                "pid=9\nrun=xyz\n", encoding="utf-8"
            )
            self.assertFalse(host._provenance_ok(guest, trace))

    def test_windows_trace_pin_copies_digest_bound_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            trace = root / "p.host.etl"
            manifest = root / "p.host.etl.pid"
            contents = b"current-etl"
            trace.write_bytes(contents)
            manifest_bytes = b"pid=87\nrun=nonce\n"
            manifest.write_bytes(manifest_bytes)

            pinned = host._pin_windows_trace(
                trace,
                root / "out",
                manifest,
                manifest_bytes,
                hashlib.sha256(contents).hexdigest(),
            )

            self.assertIsNotNone(pinned)
            assert pinned is not None
            self.assertEqual(pinned.read_bytes(), contents)

    def test_windows_trace_pin_rejects_digest_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            trace = root / "p.host.etl"
            manifest = root / "p.host.etl.pid"
            trace.write_bytes(b"replacement-etl")
            manifest_bytes = b"pid=87\nrun=nonce\n"
            manifest.write_bytes(manifest_bytes)

            pinned = host._pin_windows_trace(
                trace,
                root / "out",
                manifest,
                manifest_bytes,
                hashlib.sha256(b"original-etl").hexdigest(),
            )

            self.assertIsNone(pinned)
            self.assertEqual(list((root / "out").iterdir()), [])

    def test_windows_trace_pin_rejects_manifest_replacement_race(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            trace = root / "p.host.etl"
            manifest = root / "p.host.etl.pid"
            contents = b"current-etl"
            trace.write_bytes(contents)
            manifest_bytes = b"pid=87\nrun=nonce\n"
            manifest.write_bytes(manifest_bytes)
            copy_trace = host._copy_trace_and_hash

            def replace_manifest(source: object, destination: object) -> str:
                digest = copy_trace(source, destination)
                manifest.write_bytes(b"pid=88\nrun=replacement\n")
                return digest

            with mock.patch.object(
                host, "_copy_trace_and_hash", side_effect=replace_manifest
            ):
                pinned = host._pin_windows_trace(
                    trace,
                    root / "out",
                    manifest,
                    manifest_bytes,
                    hashlib.sha256(contents).hexdigest(),
                )

            self.assertIsNone(pinned)
            self.assertEqual(list((root / "out").iterdir()), [])


class SymbolPathEnv(unittest.TestCase):
    """`_xperf_env` prepends the VMM image directory to `_NT_SYMBOL_PATH` for the xperf child."""

    def test_no_symbol_path_inherits_parent(self) -> None:
        # No symbol path: return None so the child inherits the parent environment unchanged.
        self.assertIsNone(host._xperf_env(None))
        self.assertIsNone(host._xperf_env(""))

    def test_symbol_path_without_ambient(self) -> None:
        saved = os.environ.pop("_NT_SYMBOL_PATH", None)
        try:
            env = host._xperf_env(r"C:\build\release")
            assert env is not None
            self.assertEqual(env["_NT_SYMBOL_PATH"], r"C:\build\release")
        finally:
            if saved is not None:
                os.environ["_NT_SYMBOL_PATH"] = saved

    def test_symbol_path_prepends_and_preserves_ambient(self) -> None:
        saved = os.environ.get("_NT_SYMBOL_PATH")
        os.environ["_NT_SYMBOL_PATH"] = r"srv*C:\sym*https://example/symbols"
        try:
            env = host._xperf_env(r"C:\build\release")
            assert env is not None
            self.assertEqual(
                env["_NT_SYMBOL_PATH"],
                r"C:\build\release;srv*C:\sym*https://example/symbols",
            )
        finally:
            if saved is None:
                os.environ.pop("_NT_SYMBOL_PATH", None)
            else:
                os.environ["_NT_SYMBOL_PATH"] = saved


class XperfDiscovery(unittest.TestCase):
    @mock.patch.object(host.shutil, "which", return_value=None)
    @mock.patch.object(host.sys, "platform", "win32")
    def test_finds_standard_wpt_install_outside_path(self, which: mock.Mock) -> None:
        with tempfile.TemporaryDirectory() as directory:
            xperf = (
                Path(directory)
                / "Windows Kits"
                / "10"
                / "Windows Performance Toolkit"
                / "xperf.exe"
            )
            xperf.parent.mkdir(parents=True)
            xperf.touch()
            with mock.patch.dict(os.environ, {"ProgramFiles(x86)": directory}):
                self.assertEqual(host._find_xperf(), str(xperf))

        which.assert_called_once_with("xperf")


class PrefixIdempotency(unittest.TestCase):
    def test_prefixes_once(self) -> None:
        self.assertEqual(merge._prefix(["a;b 3"], "[GUEST]"), ["[GUEST];a;b 3"])

    def test_does_not_double_prefix(self) -> None:
        self.assertEqual(merge._prefix(["[GUEST];a;b 3"], "[GUEST]"), ["[GUEST];a;b 3"])

    def test_keeps_bare_root(self) -> None:
        self.assertEqual(merge._prefix(["[GUEST] 3"], "[GUEST]"), ["[GUEST] 3"])


class FoldedHasSamples(unittest.TestCase):
    def test_detects_samples(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "g.folded"
            p.write_text("a;b 5\n", encoding="utf-8")
            self.assertTrue(flamegraph._folded_has_samples(p))

    def test_empty_file(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "g.folded"
            p.write_text("", encoding="utf-8")
            self.assertFalse(flamegraph._folded_has_samples(p))

    def test_missing_file(self) -> None:
        self.assertFalse(flamegraph._folded_has_samples(Path("does-not-exist.folded")))


class EtwLossDetection(unittest.TestCase):
    def test_bare_notice_is_loss(self) -> None:
        self.assertTrue(host._line_reports_loss("Events were lost in this trace."))
        self.assertTrue(host._line_reports_loss("Warning: buffers lost"))
        self.assertTrue(host._line_reports_loss("lost events detected"))

    def test_positive_count_is_loss(self) -> None:
        self.assertTrue(host._line_reports_loss("5 events were lost"))
        self.assertTrue(host._line_reports_loss("buffers lost = 12"))
        self.assertTrue(host._line_reports_loss("Events lost: 3"))
        self.assertTrue(host._line_reports_loss("lost 7 buffers"))

    def test_zero_count_is_not_loss(self) -> None:
        self.assertFalse(host._line_reports_loss("0 events were lost"))
        self.assertFalse(host._line_reports_loss("Events lost: 0"))
        self.assertFalse(host._line_reports_loss("buffers lost = 0"))
        self.assertFalse(host._line_reports_loss("lost 0 events"))

    def test_unrelated_line_is_not_loss(self) -> None:
        self.assertFalse(host._line_reports_loss("Processed 1000 events successfully"))
        self.assertFalse(host._line_reports_loss("Stack, 123, 4, 5, module, symbol"))

    def test_reports_event_loss_scans_all_lines(self) -> None:
        text = "Processed 1000 events\nEvents lost: 0\n4 buffers lost\nDone"
        self.assertTrue(host._reports_event_loss(text))
        clean = "Processed 1000 events\nEvents lost: 0\nDone"
        self.assertFalse(host._reports_event_loss(clean))


class DemangleFolded(unittest.TestCase):
    """Per-frame demangling must never change the number of `;`-separated folded frames (#8)."""

    def test_apply_preserves_frame_count(self) -> None:
        # The mapping value is already sanitized (a demangled `;` became `,`); applying it keeps the
        # line at exactly two frames rather than splitting the array type into bogus frames.
        mapping = {"MANGLED": "<[u8, 32] as X>::f"}
        line = flamegraph._apply_demangle_to_line("MANGLED;other 4", mapping)
        self.assertEqual(line, "<[u8, 32] as X>::f;other 4")
        stack, _, _ = line.rpartition(" ")
        self.assertEqual(len(stack.split(";")), 2)

    def test_apply_leaves_unmapped_frames(self) -> None:
        self.assertEqual(flamegraph._apply_demangle_to_line("a;b 7", {}), "a;b 7")

    def test_apply_passes_through_non_folded(self) -> None:
        self.assertEqual(flamegraph._apply_demangle_to_line("", {}), "")
        self.assertEqual(flamegraph._apply_demangle_to_line("no-count", {}), "no-count")

    def test_demangle_frames_sanitizes_semicolons(self) -> None:
        # A Rust symbol that demangles to a name containing ';' must have it rewritten to ',' so it
        # is never mistaken for a folded frame separator.
        fake = mock.Mock(returncode=0, stdout="<[u8; 32] as Trait>::f\n")
        with mock.patch.object(flamegraph.subprocess, "run", return_value=fake):
            mapping = flamegraph._demangle_frames({"_RMANGLED"})
        self.assertEqual(mapping, {"_RMANGLED": "<[u8, 32] as Trait>::f"})
        self.assertNotIn(";", mapping["_RMANGLED"])

    def test_demangle_frames_rejects_line_count_mismatch(self) -> None:
        # If rustfilt output does not line up 1:1 with the inputs, leave every frame unchanged
        # rather than risk mis-mapping demangled names onto the wrong frames.
        fake = mock.Mock(returncode=0, stdout="a\nb\n")
        with mock.patch.object(flamegraph.subprocess, "run", return_value=fake):
            self.assertEqual(flamegraph._demangle_frames({"only_one"}), {})

    def test_demangle_frames_empty_input(self) -> None:
        self.assertEqual(flamegraph._demangle_frames(set()), {})


class ProvenanceExpectedRun(unittest.TestCase):
    """A pinned run id must be used instead of a possibly-replaced source sidecar (#9)."""

    def test_expected_run_overrides_replaced_sidecar(self) -> None:
        # A concurrent run replaced the source profile and its sidecars with a self-consistent
        # "new" set. The id pinned before copying ("orig") must still reject the "new" host trace.
        with tempfile.TemporaryDirectory() as d:
            guest = Path(d) / "p.folded"
            trace = Path(d) / "p.host.etl"
            guest.with_name(guest.name + ".run").write_text(
                "run=new\n", encoding="utf-8"
            )
            trace.with_name(trace.name + ".pid").write_text(
                "pid=9\nrun=new\n", encoding="utf-8"
            )
            self.assertFalse(host._provenance_ok(guest, trace, expected_run="orig"))

    def test_expected_run_matches_host(self) -> None:
        # The pinned id validates the host trace even when the source sidecar is gone entirely.
        with tempfile.TemporaryDirectory() as d:
            guest = Path(d) / "p.folded"
            trace = Path(d) / "p.host.etl"
            trace.with_name(trace.name + ".pid").write_text(
                "pid=9\nrun=orig\n", encoding="utf-8"
            )
            self.assertTrue(host._provenance_ok(guest, trace, expected_run="orig"))


if __name__ == "__main__":
    unittest.main()

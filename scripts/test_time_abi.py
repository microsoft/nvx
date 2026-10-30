"""Unit tests for the harness-side NVX time ABI checks and host qualification."""

from __future__ import annotations

import ast
import contextlib
import dataclasses
import io
import json
import math
import queue
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import types
import unittest
from collections.abc import Sequence
from pathlib import Path
from typing import cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))

from nvx_tools import benchmark, doctor, openvmm_process, time_abi  # noqa: E402
from nvx_tools.time_abi import TimeAbiFailure, TimeAbiMonitor  # noqa: E402

BOOT_LINE = (
    "NVX-TIME-ABI: v=1 phase=boot status=ok cpus=4 tsc_hz=2194804000 "
    "lapic_hz=1000000000 generation=0 elapsed_us=1873\n"
)
RESTORE_LINE = (
    "NVX-TIME-ABI: v=1 phase=restore status=ok cpus=4 tsc_hz=2194804000 "
    "lapic_hz=1000000000 generation=2 elapsed_us=41\n"
)
# nvx-time status prints the last check's marker, then this line.
RUNTIME_LINE = (
    "NVX-TIME-ABI: v=1 phase=runtime status=unsynchronized generation=0 "
    "discontinuities=0 offset_ns=0 uncertainty_ns=0 rejected_samples=0 "
    "last_sample_error=none\n"
)
STATUS_OK = "NVX-TIME-STATUS-EXIT status=0\n"
KVM_BOOT = ["openvmm", "--hypervisor", "kvm", "--kernel", "vmlinux"]
MSHV_RESTORE = ["openvmm", "--hypervisor", "mshv", "--restore-snapshot", "snap"]
# OpenVMM's openvmm_entry fatal_error_message, after a guest prompt.
FATAL_DETAIL = "[E_TSC_SYNC_UNSUPPORTED] failed to launch vm worker"
FATAL = f"~ # fatal error: {FATAL_DETAIL}\r\n\r\nCaused by:\r\n".encode()


def warp_output(
    *,
    pairs: int = 6,
    backward: int = 0,
    offset: int = 15,
    verdict: str = "PASS",
    stalled: int = 0,
    conclusive: int = 1,
) -> str:
    return (
        "pair 0-1: warp_iterations=881154 warps=0 max_backward_ns=0\n"
        f"NVX-TIME-PROBE warp max_backward_cycles=0 max_backward_ns={backward} "
        f"max_abs_offset_ns={offset} pairs={pairs} verdict={verdict} bound_ns=1000\r\n"
        "NVX-TIME-PROBE warp-detail max_abs_offset_cycles=33.0 max_uncertainty_ns=127 "
        "max_skew_bound_ns=130 total_warps=0 inconsistent_pairs=0 "
        f"stalled_pairs={stalled} cpus=0-3 duration_ms=100 tsc_hz=2194804000 "
        f"tsc_hz_source=kmsg-detected-processor conclusive={conclusive}\n"
    )


def write_openvmm_profiles(
    checkout: Path, *generations: time_abi.CpuGeneration, revision: int = 1
) -> Path:
    """Write ``generations`` as revision ``revision`` of the pinned CPU
    profiles of an OpenVMM checkout at ``checkout``, with the fields that NVX
    reads, and return the checkout."""
    directory = checkout / time_abi.OPENVMM_CPU_PROFILES
    directory.mkdir(parents=True, exist_ok=True)
    for generation in generations:
        profile_id = (
            f"{time_abi.PROFILE_VENDORS[generation.vendor]}.{generation.name}"
            f".v{revision}"
        )
        document = {
            "id": profile_id,
            "vendor": generation.vendor,
            "generation": {
                "name": generation.name,
                "cpus": [
                    {
                        "family": cpu.family,
                        "model": cpu.model,
                        "steppings": [cpu.steppings.start, cpu.steppings.stop - 1],
                    }
                    for cpu in generation.cpus
                ],
            },
        }
        (directory / f"{profile_id}.json").write_text(
            json.dumps(document), encoding="utf-8"
        )
    return checkout


# A Rust integer constant, `[pub] const NAME: TYPE = EXPRESSION;`.
RUST_INTEGER_CONSTANT = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?const\s+(\w+)\s*:\s*[iu](?:8|16|32|64|128|size)"
    r"\s*=\s*([^;]+);",
    re.MULTILINE,
)


def rust_integer_constants(source: str) -> dict[str, int]:
    """Return the integer constants of a Rust source whose values are integer
    literals, or sums, differences, products, and left shifts of literals and
    of earlier such constants."""
    constants: dict[str, int] = {}

    def evaluate(node: ast.expr) -> int:
        if isinstance(node, ast.Constant) and type(node.value) is int:
            return node.value
        if isinstance(node, ast.Name):
            return constants[node.id]
        if isinstance(node, ast.BinOp):
            left, right = evaluate(node.left), evaluate(node.right)
            if isinstance(node.op, ast.Add):
                return left + right
            if isinstance(node.op, ast.Sub):
                return left - right
            if isinstance(node.op, ast.Mult):
                return left * right
            if isinstance(node.op, ast.LShift):
                return left << right
        raise ValueError(f"unsupported expression {ast.dump(node)}")

    for name, expression in RUST_INTEGER_CONSTANT.findall(source):
        try:
            constants[name] = evaluate(ast.parse(expression.strip(), mode="eval").body)
        except (KeyError, SyntaxError, ValueError):
            continue
    return constants


def pinned_openvmm_file(test: unittest.TestCase, path: str) -> str:
    """Return the file at ``path`` in the OpenVMM submodule's pinned revision.

    The file is read from the gitlink's commit, not from the submodule's
    working tree, which can hold another revision: a self-hosted CI runner
    keeps an earlier job's OpenVMM checkout until the job checks out its own.
    Skips ``test`` where the submodule lacks the pinned revision.
    """
    root = Path(__file__).resolve().parents[1]
    submodule = root / "openvmm"
    if not (submodule / ".git").exists():
        test.skipTest("the OpenVMM submodule is not initialized")

    def git(directory: Path, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", "-C", str(directory), *arguments],
            capture_output=True,
            encoding="utf-8",
            check=False,
        )

    pin = git(root, "rev-parse", ":openvmm").stdout.strip()
    if not pin or git(submodule, "cat-file", "-e", f"{pin}^{{commit}}").returncode:
        test.skipTest(
            f"the OpenVMM submodule lacks its pinned revision {pin or '(unknown)'}"
        )
    shown = git(submodule, "show", f"{pin}:{path}")
    test.assertEqual(shown.returncode, 0, f"{path} at {pin}: {shown.stderr}")
    return shown.stdout


class FieldParsingTests(unittest.TestCase):
    def test_parses_plain_and_quoted_fields_with_guest_escapes(self):
        fields = time_abi.parse_fields(
            'v=1 code=G_TSC_WARP detail="say \\"hi\\" \\\\ \\x07 end" phase=boot'
        )
        self.assertEqual(
            fields,
            {
                "v": "1",
                "code": "G_TSC_WARP",
                "detail": 'say "hi" \\ \x07 end',
                "phase": "boot",
            },
        )

    def test_rejects_malformed_fields(self):
        for text in (
            "novalue",
            "=x",
            'detail="unterminated',
            'detail="bad \\q escape"',
            'detail="\\x4"',
            "v=1 v=2",
        ):
            with self.subTest(text=text), self.assertRaises(ValueError):
                time_abi.parse_fields(text)

    def test_parses_markers_and_violations(self):
        marker = time_abi.parse_marker(BOOT_LINE)
        self.assertIsNotNone(marker)
        assert marker is not None
        self.assertEqual(marker["phase"], "boot")
        self.assertEqual(marker["tsc_hz"], "2194804000")
        self.assertIsNone(time_abi.parse_marker("NVX-TIME-REPORT: v=1 phase=boot"))
        with self.assertRaises(ValueError):
            time_abi.parse_marker("NVX-TIME-ABI: v=1 status=ok")
        violation = time_abi.parse_violation(
            "noise NVX-TIME-ABI-VIOLATION: v=1 code=G_RCU_STALL source=watcher "
            'phase=runtime generation=1 boottime_ns=5 detail="rcu: INFO: x"\r'
        )
        self.assertIsNotNone(violation)
        assert violation is not None
        self.assertEqual(violation["code"], "G_RCU_STALL")
        self.assertEqual(violation["detail"], "rcu: INFO: x")

    def test_describes_only_time_abi_exit_statuses(self):
        self.assertIn("conformance", time_abi.describe_exit_status(193) or "")
        self.assertIn("runtime violation", time_abi.describe_exit_status(194) or "")
        self.assertIn("restore repair", time_abi.describe_exit_status(195) or "")
        for status in (None, 0, 1, 37, 192, 196, 255):
            self.assertIsNone(time_abi.describe_exit_status(status))

    def test_maps_cpu_generations(self):
        def name(model: int, stepping: int, vendor: str = "GenuineIntel"):
            generation = time_abi.cpu_generation(vendor, 6, model, stepping)
            return generation.name if generation else None

        self.assertEqual(name(85, 4), "skylake-sp")
        self.assertEqual(name(106, 6), "icelake-sp")
        self.assertEqual(name(207, 2), "emeraldrapids")
        # Alder Lake-S and Alder Lake-P and -H share one profile.
        self.assertEqual(name(151, 2), "alderlake")
        self.assertEqual(name(154, 3), "alderlake")
        # Cascade Lake and Cooper Lake share model 85 but have no profile.
        self.assertIsNone(name(85, 7))
        self.assertIsNone(name(85, 11))
        self.assertIsNone(name(143, 8))
        # Tiger Lake and Raptor Lake have no profile either.
        self.assertIsNone(name(140, 1))
        self.assertIsNone(name(183, 1))
        self.assertIsNone(name(1, 1, "AuthenticAMD"))
        # Milan's profile serves every stepping of AMD's family 25 model 1,
        # Milan-X's 2 included, Genoa's every stepping of model 17, and
        # Turin's every stepping of family 26 model 2, and none of them
        # another vendor's CPU with its signature.
        for family, model, expected in (
            (25, 1, "milan"),
            (25, 17, "genoa"),
            (26, 2, "turin"),
        ):
            for stepping in (0, 1, 2, 15):
                generation = time_abi.cpu_generation(
                    "AuthenticAMD", family, model, stepping
                )
                assert generation is not None
                self.assertEqual(generation.name, expected)
        # Vermeer and Raphael (Ryzen), Turin Dense, and Granite Ridge have no
        # profile, and neither has another vendor's CPU with the signature of
        # an AMD generation.
        for vendor, family, model in (
            ("AuthenticAMD", 25, 33),
            ("AuthenticAMD", 25, 97),
            ("AuthenticAMD", 26, 17),
            ("AuthenticAMD", 26, 68),
            ("GenuineIntel", 25, 1),
            ("HygonGenuine", 25, 1),
            ("GenuineIntel", 26, 2),
        ):
            with self.subTest(vendor=vendor, family=family, model=model):
                self.assertIsNone(time_abi.cpu_generation(vendor, family, model, 1))

    def test_names_the_catalog_profiles(self):
        # Each generation's latest profile, which auto selects, serves every
        # backend.
        self.assertEqual(
            [generation.profile_id for generation in time_abi.CPU_GENERATIONS],
            [
                "intel.skylake-sp.v1",
                "intel.icelake-sp.v1",
                "intel.emeraldrapids.v1",
                "intel.alderlake.v1",
                "amd.milan.v1",
                "amd.genoa.v2",
                "amd.turin.v1",
            ],
        )
        # A generation is the CPUs that it covers, whatever its profile's
        # revision.
        genoa = time_abi.cpu_generation("AuthenticAMD", 25, 17, 1)
        assert genoa is not None
        self.assertEqual(dataclasses.replace(genoa, revision=1), genoa)
        self.assertEqual(
            time_abi.describe_cpu_generations(),
            "skylake-sp 6/85 steppings 0-4, icelake-sp 6/106, emeraldrapids 6/207, "
            "alderlake 6/151 and 6/154, milan 25/1, genoa 25/17, turin 26/2",
        )

    def test_the_catalog_copy_matches_openvmm_pinned_profiles(self):
        # NVX's copy must not drift from the profiles that OpenVMM pins. They
        # are read from the gitlink's commit, not from the submodule's working
        # tree, which can hold another revision: a self-hosted CI runner keeps
        # an earlier job's OpenVMM checkout until the job checks out its own.
        root = Path(__file__).resolve().parents[1]
        submodule = root / "openvmm"
        if not (submodule / ".git").exists():
            self.skipTest("the OpenVMM submodule is not initialized")

        def git(directory: Path, *arguments: str) -> str | None:
            result = subprocess.run(
                ["git", "-C", str(directory), *arguments],
                capture_output=True,
                encoding="utf-8",
                check=False,
            )
            return result.stdout if result.returncode == 0 else None

        pin = (git(root, "rev-parse", ":openvmm") or "").strip()
        if not pin or git(submodule, "cat-file", "-e", f"{pin}^{{commit}}") is None:
            self.skipTest(
                f"the OpenVMM submodule lacks its pinned revision {pin or '(unknown)'}"
            )
        listing = git(
            submodule,
            "ls-tree",
            "-z",
            "--name-only",
            pin,
            "--",
            "vmm_core/cpu_profile/profiles/",
        )
        assert listing is not None
        pinned: dict[str, tuple[str, str, tuple[tuple[int, int, range], ...]]] = {}
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        checkout = Path(temporary.name)
        profiles = checkout / time_abi.OPENVMM_CPU_PROFILES
        profiles.mkdir(parents=True)
        for path in sorted(listing.split("\0")):
            if not path.endswith(".json"):
                continue
            content = git(submodule, "show", f"{pin}:{path}")
            assert content is not None
            (profiles / Path(path).name).write_text(content, encoding="utf-8")
            document = json.loads(content)
            generation = document["generation"]
            # Keyed by profile ID, so that a revision that the copy lacks fails
            # the comparison instead of replacing another.
            pinned[document["id"]] = (
                generation["name"],
                document["vendor"],
                tuple(
                    (
                        cpu["family"],
                        cpu["model"],
                        range(cpu["steppings"][0], cpu["steppings"][1] + 1),
                    )
                    for cpu in generation["cpus"]
                ),
            )
        self.assertTrue(pinned)
        # OpenVMM keeps every earlier revision of a generation's profile
        # pinned.
        copy = {
            f"{time_abi.PROFILE_VENDORS[generation.vendor]}.{generation.name}"
            f".v{revision}": (
                generation.name,
                generation.vendor,
                tuple(
                    (cpu.family, cpu.model, cpu.steppings) for cpu in generation.cpus
                ),
            )
            for generation in time_abi.CPU_GENERATIONS
            for revision in range(1, generation.revision + 1)
        }
        self.assertEqual(copy, pinned)
        # benchmark reads the same generations from these documents, as it
        # reads them from the OpenVMM checkout that it builds.
        self.assertEqual(
            set(time_abi.openvmm_cpu_generations(checkout)),
            set(time_abi.CPU_GENERATIONS),
        )

    def test_reads_the_cpu_profiles_of_an_openvmm_checkout(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        directory = Path(temporary.name)
        icelake, alderlake = time_abi.CPU_GENERATIONS[1], time_abi.CPU_GENERATIONS[3]
        granite_rapids = time_abi.CpuGeneration(
            "graniterapids", "GenuineIntel", (time_abi.CpuModel(6, 173),)
        )
        checkout = write_openvmm_profiles(
            directory / "openvmm", icelake, alderlake, granite_rapids
        )
        # A second revision of a generation's profile adds no generation.
        write_openvmm_profiles(checkout, icelake, revision=2)
        generations = time_abi.openvmm_cpu_generations(checkout)
        self.assertEqual(set(generations), {icelake, alderlake, granite_rapids})
        self.assertEqual(len(generations), 3)
        # A CPU that only the checkout's profiles cover is benchmarked with
        # them, and not with NVX's copy of the catalog.
        granite_rapids_host = time_abi.HostCpu("GenuineIntel", 6, 173, 1)
        self.assertIsNone(
            time_abi.benchmark_cpu_profile_refusal(
                granite_rapids_host, "openvmm", "kvm", generations
            )
        )
        self.assertIsNotNone(
            time_abi.benchmark_cpu_profile_refusal(
                granite_rapids_host, "openvmm", "kvm"
            )
        )
        # A checkout that pins no profile, and a malformed profile, are errors,
        # naming the checkout or the profile.
        with self.assertRaisesRegex(ValueError, "pins no CPU profile"):
            time_abi.openvmm_cpu_generations(directory / "empty")
        valid = {
            "vendor": "GenuineIntel",
            "generation": {
                "name": "icelake-sp",
                "cpus": [{"family": 6, "model": 106, "steppings": [0, 15]}],
            },
        }
        cases: tuple[tuple[str, object, str], ...] = (
            ("not-json", None, "cannot read the CPU profile .*not-json.json"),
            ("list", [], "it is not a JSON object"),
            ("no-generation", {"vendor": "GenuineIntel"}, "names no vendor"),
            (
                "no-cpus",
                {**valid, "generation": {"name": "icelake-sp", "cpus": []}},
                "names no CPU models",
            ),
            (
                "stepping-pair",
                {
                    **valid,
                    "generation": {
                        "name": "icelake-sp",
                        "cpus": [{"family": 6, "model": 106, "steppings": [3]}],
                    },
                },
                "not a first and a last",
            ),
            (
                "stepping-order",
                {
                    **valid,
                    "generation": {
                        "name": "icelake-sp",
                        "cpus": [{"family": 6, "model": 106, "steppings": [9, 3]}],
                    },
                },
                "steppings 9 to 3 are not a range",
            ),
            (
                "boolean-family",
                {
                    **valid,
                    "generation": {
                        "name": "icelake-sp",
                        "cpus": [{"family": True, "model": 106, "steppings": [0, 1]}],
                    },
                },
                "its family is not an integer",
            ),
        )
        for name, document, message in cases:
            with self.subTest(name=name):
                malformed = directory / name
                profiles = malformed / time_abi.OPENVMM_CPU_PROFILES
                profiles.mkdir(parents=True)
                (profiles / f"{name}.json").write_text(
                    "{" if document is None else json.dumps(document),
                    encoding="utf-8",
                )
                with self.assertRaisesRegex(ValueError, message):
                    time_abi.openvmm_cpu_generations(malformed)

    def test_tells_catalog_profiles_from_host_profiles(self):
        for profile_id, catalog, host in (
            ("intel.icelake-sp.v1", True, False),
            ("intel.raptorlake.v2", True, False),
            ("amd.milan.v1", True, False),
            ("intel.host.v1", False, True),
            ("amd.host.v1", False, True),
            # Host profile IDs take the revision syntax of every profile ID.
            ("intel.host.v0", False, False),
            ("interim.host.kvm.v1", False, False),
            ("intel.icelake-sp.v2-rc", False, False),
            ("intel.icelake-sp.v0", False, False),
            ("none", False, False),
            ("", False, False),
        ):
            with self.subTest(profile_id=profile_id):
                self.assertEqual(time_abi.is_catalog_profile_id(profile_id), catalog)
                self.assertEqual(time_abi.is_host_profile_id(profile_id), host)

    def test_names_cpus_and_prefills_cpu_profile_requests(self):
        granite_rapids = time_abi.HostCpu(
            "GenuineIntel", 6, 173, 1, "Intel(R) Xeon(R) 6973P-C"
        )
        self.assertEqual(granite_rapids.signature(), "GenuineIntel 6/173/1")
        self.assertEqual(
            granite_rapids.describe(), "GenuineIntel 6/173/1 (Intel(R) Xeon(R) 6973P-C)"
        )
        # The link that OpenVMM's fallback warning gives the same CPU
        # (openvmm_core's time_abi tests).
        self.assertEqual(
            time_abi.cpu_profile_request_url(granite_rapids),
            "https://github.com/microsoft/nvx/issues/new?template=cpu-profile.yml"
            "&title=CPU%20profile%3A%20GenuineIntel%206%2F173%2F1%20%28Intel%28R%29"
            "%20Xeon%28R%29%206973P-C%29&signature=GenuineIntel%206%2F173%2F1"
            "&cpu=Intel%28R%29%20Xeon%28R%29%206973P-C",
        )
        # A CPU without a brand string prefills only its signature.
        brandless = time_abi.HostCpu("GenuineIntel", 6, 173, 1)
        self.assertEqual(brandless.describe(), "GenuineIntel 6/173/1")
        self.assertEqual(
            time_abi.cpu_profile_request_url(brandless),
            f"{time_abi.CPU_PROFILE_REQUEST_URL}&title=CPU%20profile%3A%20"
            "GenuineIntel%206%2F173%2F1&signature=GenuineIntel%206%2F173%2F1",
        )
        self.assertIn(
            "%26%3D%3F%23%25%2B",
            time_abi.cpu_profile_request_url(
                time_abi.HostCpu("GenuineIntel", 6, 173, 1, "&=?#%+")
            ),
        )

    def test_parses_the_cpu_profile_fallback_warning(self):
        digest = "ab" * 32
        warning = (
            "   0.012345s  WARN openvmm_core::worker::dispatch::time_abi: "
            "NVX-CPU-PROFILE-FALLBACK: no built-in CPU profile serves this host's "
            "CPU, GenuineIntel 6/173/1 (Intel(R) Xeon(R) 6973P-C), so --cpu-profile "
            f"auto fell back to intel.host.v1 (sha256:{digest}), a profile derived "
            "from this host's kvm backend for development.\n"
            "It is not pinned: a microcode, firmware, hypervisor, or OS update can "
            "change it.\n"
        )
        self.assertEqual(time_abi.parse_cpu_profile_fallback(warning), "intel.host.v1")
        self.assertEqual(
            time_abi.parse_cpu_profile_fallback(
                warning.replace("intel.host.v1", "amd.host.v1")
            ),
            "amd.host.v1",
        )
        self.assertIsNone(time_abi.parse_cpu_profile_fallback("no warning\n"))
        for output, message in (
            (warning + warning, "logged 2 NVX-CPU-PROFILE-FALLBACK: warnings"),
            (warning.replace("intel.host.v1", "intel.icelake-sp.v1"), "malformed"),
            (warning.replace(f"sha256:{digest}", "sha256:ab"), "malformed"),
            (warning.replace("fell back to", "selected"), "malformed"),
        ):
            with self.subTest(message=message):
                with self.assertRaisesRegex(ValueError, message):
                    time_abi.parse_cpu_profile_fallback(output)

    def test_benchmark_refuses_cpus_that_no_built_in_profile_serves(self):
        granite_rapids = time_abi.HostCpu(
            "GenuineIntel", 6, 173, 1, "Intel(R) Xeon(R) 6973P-C"
        )
        refusal = time_abi.benchmark_cpu_profile_refusal(
            granite_rapids, "openvmm", "kvm"
        )
        assert refusal is not None
        self.assertTrue(
            refusal.startswith(
                "benchmark refuses this host: no built-in CPU profile serves its CPU, "
                "GenuineIntel 6/173/1 (Intel(R) Xeon(R) 6973P-C), so --cpu-profile "
                "auto would fall back to a CPU profile derived from this host, whose "
                "every cold boot fingerprints the hypervisor first"
            ),
            refusal,
        )
        self.assertIn(time_abi.describe_cpu_generations(), refusal)
        self.assertIn(
            "\n  openvmm --hypervisor kvm --cpu-fingerprint fingerprint.json\n", refusal
        )
        self.assertTrue(
            refusal.endswith(time_abi.cpu_profile_request_url(granite_rapids)), refusal
        )
        # Another vendor's CPU cannot boot at all, and gets no request form.
        hygon = time_abi.benchmark_cpu_profile_refusal(
            time_abi.HostCpu("HygonGenuine", 24, 0, 1), "openvmm", "kvm"
        )
        assert hygon is not None
        self.assertIn("host CPU profiles serve only Intel and AMD CPUs", hygon)
        self.assertIn("E_PROFILE_HOST_UNKNOWN", hygon)
        self.assertNotIn("github.com", hygon)
        # A CPU that cannot be identified is refused too: OpenVMM, which reads
        # the CPU itself, could fall back on it.
        unknown = time_abi.benchmark_cpu_profile_refusal(None, "openvmm", "kvm")
        assert unknown is not None
        self.assertTrue(
            unknown.startswith(
                "benchmark refuses this host: it cannot identify the host's CPU"
            ),
            unknown,
        )
        # Its vendor is unknown too, so the refusal qualifies the fallback by
        # vendor.
        self.assertIn(
            "On an Intel or AMD CPU that none serves, --cpu-profile auto falls "
            "back to a CPU profile",
            unknown,
        )
        self.assertIn(
            "on another vendor's CPU, every cold boot fails with "
            f"{time_abi.PROFILE_HOST_UNKNOWN}",
            unknown,
        )
        # A CPU that a built-in profile serves is benchmarked.
        for host in (
            time_abi.HostCpu("GenuineIntel", 6, 106, 6),
            time_abi.HostCpu("AuthenticAMD", 25, 1, 1),
        ):
            with self.subTest(host=host):
                self.assertIsNone(
                    time_abi.benchmark_cpu_profile_refusal(host, "openvmm", "whp")
                )
        # The profiles that decide are those of the OpenVMM that benchmark
        # builds: one that adds a profile for a CPU serves it, and one that
        # lacks a profile refuses its CPU, listing its own profiles.
        graniterapids = time_abi.CpuGeneration(
            "graniterapids", "GenuineIntel", (time_abi.CpuModel(6, 173),)
        )
        self.assertIsNone(
            time_abi.benchmark_cpu_profile_refusal(
                granite_rapids,
                "openvmm",
                "kvm",
                (*time_abi.CPU_GENERATIONS, graniterapids),
            )
        )
        icelake = time_abi.HostCpu("GenuineIntel", 6, 106, 6)
        refusal = time_abi.benchmark_cpu_profile_refusal(
            icelake, "openvmm", "kvm", (graniterapids,)
        )
        assert refusal is not None
        self.assertIn(
            "no built-in CPU profile serves its CPU, GenuineIntel 6/106/6", refusal
        )
        self.assertIn("The built-in profiles cover graniterapids 6/173.\n", refusal)
        # A CPU in more than one generation fails every cold boot, because
        # auto rejects it as a catalog defect without falling back, as OpenVMM
        # does; revisions of one generation's profile do not make it ambiguous.
        icelake_x = time_abi.CpuGeneration(
            "icelake-x", "GenuineIntel", (time_abi.CpuModel(6, 106),)
        )
        ambiguous = time_abi.benchmark_cpu_profile_refusal(
            icelake, "openvmm", "kvm", (*time_abi.CPU_GENERATIONS, icelake_x)
        )
        assert ambiguous is not None
        self.assertTrue(
            ambiguous.startswith(
                "benchmark refuses this host: its CPU, GenuineIntel 6/106/6, is in "
                "more than one generation of the built-in CPU profiles (icelake-sp, "
                "icelake-x), so every cold boot with --cpu-profile auto fails with "
                f"{time_abi.PROFILE_HOST_UNKNOWN}."
            ),
            ambiguous,
        )
        self.assertNotIn("github.com", ambiguous)
        revised = time_abi.CpuGeneration(
            "icelake-sp",
            "GenuineIntel",
            (time_abi.CpuModel(6, 106), time_abi.CpuModel(6, 108)),
        )
        self.assertIsNone(
            time_abi.benchmark_cpu_profile_refusal(
                icelake, "openvmm", "kvm", (*time_abi.CPU_GENERATIONS, revised)
            )
        )

    def test_cpu_profile_requests_match_openvmm_and_the_issue_form(self):
        # OpenVMM's fallback warning and NVX link to the same form, which must
        # define the text fields that the links prefill, and NVX matches the
        # warning's marker. OpenVMM's constants are read from the gitlink's
        # commit, as the catalog copy's comparison reads its profiles.
        root = Path(__file__).resolve().parents[1]
        form = (root / ".github" / "ISSUE_TEMPLATE" / "cpu-profile.yml").read_text(
            encoding="utf-8"
        )
        fields = re.findall(r"^  - type: (\w+)\n    id: ([\w-]+)$", form, re.MULTILINE)
        for prefilled in ("cpu", "signature"):
            self.assertIn(("input", prefilled), fields)
        self.assertRegex(form, r"(?m)^name: CPU profile request$")
        self.assertEqual(
            time_abi.CPU_PROFILE_REQUEST_URL.rpartition("template=")[2],
            "cpu-profile.yml",
        )

        submodule = root / "openvmm"
        if not (submodule / ".git").exists():
            self.skipTest("the OpenVMM submodule is not initialized")
        pin = subprocess.run(
            ["git", "-C", str(root), "rev-parse", ":openvmm"],
            capture_output=True,
            encoding="utf-8",
            check=False,
        ).stdout.strip()
        source = subprocess.run(
            [
                "git",
                "-C",
                str(submodule),
                "show",
                f"{pin}:openvmm/openvmm_core/src/worker/dispatch/time_abi.rs",
            ],
            capture_output=True,
            encoding="utf-8",
            check=False,
        )
        if not pin or source.returncode != 0:
            self.skipTest(
                f"the OpenVMM submodule lacks its pinned revision {pin or '(unknown)'}"
            )
        constants = dict(
            re.findall(
                r'const (FALLBACK_MARKER|CPU_PROFILE_REQUEST_URL): &str =\s*"([^"]*)";',
                source.stdout,
            )
        )
        self.assertEqual(
            constants,
            {
                "FALLBACK_MARKER": time_abi.CPU_PROFILE_FALLBACK_MARKER,
                "CPU_PROFILE_REQUEST_URL": time_abi.CPU_PROFILE_REQUEST_URL,
            },
        )
        for key in ("&title=", "&signature=", "&cpu="):
            self.assertIn(key, source.stdout)

    def test_computes_the_checks_cpu_time_budget(self):
        # The final budgets: a base plus an increment per additional CPU, one
        # budget per backend and phase.
        for backend in ("kvm", "mshv", "whp"):
            self.assertEqual(
                set(time_abi.CHECK_CPU_BUDGET_US[backend]),
                {"boot", "capture", "restore"},
            )
        self.assertEqual(time_abi.check_cpu_budget_us("kvm", "boot", 1), 6_000)
        self.assertEqual(time_abi.check_cpu_budget_us("kvm", "capture", 1), 1_500)
        self.assertEqual(time_abi.check_cpu_budget_us("kvm", "capture", 8), 4_300)
        self.assertEqual(time_abi.check_cpu_budget_us("kvm", "restore", 1), 7_000)
        self.assertEqual(time_abi.check_cpu_budget_us("kvm", "restore", 8), 21_000)
        # The binding MSHV boot cell: 4 vCPUs (4.94 ms measured).
        self.assertEqual(time_abi.check_cpu_budget_us("mshv", "boot", 4), 6_000)
        self.assertEqual(time_abi.check_cpu_budget_us("mshv", "restore", 8), 6_000)
        self.assertEqual(time_abi.check_cpu_budget_us("whp", "restore", 1), 35_000)
        # The binding WHP cells: boot at 2 vCPUs (21.84 ms measured on an 8573C
        # runner), capture at 1 (0.986 ms), and restore at 4 (42.20 ms).
        self.assertEqual(time_abi.check_cpu_budget_us("whp", "boot", 2), 26_500)
        self.assertEqual(time_abi.check_cpu_budget_us("whp", "capture", 1), 1_200)
        self.assertEqual(time_abi.check_cpu_budget_us("whp", "capture", 2), 1_600)
        self.assertEqual(time_abi.check_cpu_budget_us("whp", "restore", 4), 53_000)
        self.assertIsNone(time_abi.check_cpu_budget_us("hvf", "boot", 1))
        self.assertIsNone(time_abi.check_cpu_budget_us("kvm", "runtime", 1))
        self.assertIsNone(time_abi.check_cpu_budget_us("kvm", "boot", 0))


class MonitorTests(unittest.TestCase):
    def test_classifies_commands(self):
        monitor = TimeAbiMonitor(KVM_BOOT)
        self.assertEqual(monitor.backend, "kvm")
        self.assertTrue(monitor.cold_boot)
        restore = TimeAbiMonitor(MSHV_RESTORE)
        self.assertEqual(restore.backend, "mshv")
        self.assertFalse(restore.cold_boot)

    def test_records_markers_across_chunk_boundaries(self):
        monitor = TimeAbiMonitor(KVM_BOOT)
        data = (
            "[    0.1] early\n"
            + " ALPINE-MICROVM-BOOT-OK: 3.22.1\n"
            + BOOT_LINE
            + RUNTIME_LINE
            + "NVX-TIME-ABI: v=1 phase=restore status=ok cpus=4 tsc_hz=2194804000 "
            + "lapic_hz=1000000000 generation=1 elapsed_us=12\n"
        ).encode()
        for index in range(0, len(data), 7):
            monitor.feed(data[index : index + 7])
        self.assertTrue(monitor.guest_booted)
        self.assertEqual(monitor.require_boot("test", online_cpus=4)["cpus"], "4")
        assert monitor.runtime is not None
        self.assertEqual(monitor.runtime["status"], "unsynchronized")
        self.assertEqual(monitor.restores[0]["generation"], "1")
        # The runtime line is the discipline's state and never fails a run.
        TimeAbiMonitor(KVM_BOOT).feed(
            RUNTIME_LINE.replace("unsynchronized", "uncertain").encode()
        )

    def test_reports_a_check_that_stayed_pending(self):
        pending = (
            "NVX-TIME-ABI: v=1 phase=restore status=pending cpus=4 "
            "tsc_hz=2194804000 lapic_hz=200000000 generation=1\n"
        )
        with self.assertRaisesRegex(
            TimeAbiFailure,
            "restore check was still pending when nvx-time status stopped "
            "waiting after 30 s",
        ):
            TimeAbiMonitor(MSHV_RESTORE).feed(pending.encode() + STATUS_OK.encode())

    def test_fails_fast_on_violation_events_and_failed_checks(self):
        monitor = TimeAbiMonitor(KVM_BOOT)
        with self.assertRaisesRegex(TimeAbiFailure, "violation G_TSC_UNSTABLE"):
            monitor.feed(
                b"NVX-TIME-ABI-VIOLATION: v=1 code=G_TSC_UNSTABLE source=watcher "
                b'phase=runtime generation=0 boottime_ns=1 detail="Marking"\n'
            )
        self.assertIn("G_TSC_UNSTABLE", monitor.violation or "")
        with self.assertRaisesRegex(TimeAbiFailure, "boot check C9 failed: token"):
            TimeAbiMonitor(KVM_BOOT).feed(
                b'NVX-TIME-ABI: v=1 phase=boot status=fail check=C9 detail="token"\n'
            )
        with self.assertRaisesRegex(TimeAbiFailure, "malformed"):
            TimeAbiMonitor(KVM_BOOT).feed(b"NVX-TIME-ABI: v=1 phase=boot\n")
        with self.assertRaisesRegex(TimeAbiFailure, "unknown time ABI marker status"):
            TimeAbiMonitor(KVM_BOOT).feed(
                b"NVX-TIME-ABI: v=1 phase=boot status=maybe\n"
            )

    def test_rejects_a_report_only_boot_when_a_status_query_exits(self):
        monitor = TimeAbiMonitor(KVM_BOOT)
        monitor.feed(
            b"NVX-TIME-REPORT-VIOLATION: v=1 code=G_TSC_WARP source=watcher "
            b'phase=boot generation=0 boottime_ns=1 detail="x"\n'
            b"NVX-GUEST-BOOT-OK: alpine\n"
        )
        self.assertTrue(monitor.report_only)
        # A report-only guest whose check failed exits 1; the failure names
        # report-only mode, not the exit status.
        report = (
            b"NVX-TIME-REPORT: v=1 phase=boot status=fail cpus=1 tsc_hz=2194804000 "
            b"lapic_hz=1000000000 generation=0 elapsed_us=9 failures=3\n"
        )
        with self.assertRaisesRegex(TimeAbiFailure, "report-only"):
            monitor.feed(report + b"NVX-TIME-STATUS-EXIT status=1\n")
        restored = TimeAbiMonitor(MSHV_RESTORE)
        with self.assertRaisesRegex(
            TimeAbiFailure,
            "restore marker before nvx-time status exited; .*report-only",
        ):
            restored.feed(
                report.replace(b"phase=boot", b"phase=restore")
                + b"NVX-TIME-STATUS-EXIT status=0\n"
            )

    def test_status_queries_require_the_line_of_the_launch_kind(self):
        # A quiet guest reaches its shell without printing any time ABI line.
        quiet = TimeAbiMonitor(KVM_BOOT)
        quiet.feed(b" ALPINE-MICROVM-BOOT-OK: 3.22.1\nNVX-GUEST-BOOT-OK: alpine\n")
        self.assertTrue(quiet.guest_booted)
        with self.assertRaisesRegex(
            TimeAbiFailure,
            "nvx-time status did not finish before the scenario; the guest never "
            "reported its time ABI state",
        ):
            quiet.require_status("the scenario")
        # The shell's echo of the query is not its exit line.
        quiet.feed(
            b'/ # /sbin/nvx-time status; echo "NVX-TIME-STATUS-EXIT status=$?"\n'
        )
        self.assertEqual(quiet.status_queries, 0)
        with self.assertRaisesRegex(
            TimeAbiFailure,
            "did not report the NVX-TIME-ABI boot marker before nvx-time status "
            "exited; the guest image or OpenVMM does not implement time ABI v1",
        ):
            quiet.feed(b"NVX-TIME-STATUS-EXIT status=0\r\n")

        booted = TimeAbiMonitor(KVM_BOOT)
        booted.feed((BOOT_LINE + RUNTIME_LINE + STATUS_OK).encode())
        self.assertEqual(booted.status_queries, 1)
        booted.require_status("the scenario")
        self.assertEqual(booted.require_boot("x", online_cpus=4)["cpus"], "4")
        mismatched = BOOT_LINE.replace("lapic_hz=1000000000", "lapic_hz=200000000")
        monitor = TimeAbiMonitor(KVM_BOOT)
        monitor.feed(mismatched.encode())
        with self.assertRaisesRegex(TimeAbiFailure, "is not the kvm rate"):
            monitor.feed(b"NVX-TIME-STATUS-EXIT status=0\n")

        with self.assertRaisesRegex(
            TimeAbiFailure,
            "did not report an NVX-TIME-ABI restore marker before nvx-time status "
            "exited",
        ):
            TimeAbiMonitor(MSHV_RESTORE).feed(b"NVX-TIME-STATUS-EXIT status=0\n")
        restored = TimeAbiMonitor(MSHV_RESTORE)
        restored.feed(
            RESTORE_LINE.replace("lapic_hz=1000000000", "lapic_hz=200000000").encode()
            + RUNTIME_LINE.replace("generation=0", "generation=2").encode()
            + b"NVX-TIME-STATUS-EXIT status=0\n"
        )
        self.assertEqual(
            restored.require_restore("x", online_cpus=4)["generation"], "2"
        )
        # After a restore, status prints every recorded phase, oldest first,
        # each with the values from when its check ran: the boot and capture
        # lines keep their boot-time CPU count, and only the newest line counts.
        activated = TimeAbiMonitor(MSHV_RESTORE)
        mshv = "lapic_hz=200000000"
        activated.feed(
            (
                BOOT_LINE.replace("cpus=4", "cpus=1").replace(
                    "lapic_hz=1000000000", mshv
                )
                + BOOT_LINE.replace("phase=boot", "phase=capture")
                .replace("cpus=4", "cpus=1")
                .replace("lapic_hz=1000000000", mshv)
                + RESTORE_LINE.replace("generation=2", "generation=1").replace(
                    "lapic_hz=1000000000", mshv
                )
                + RUNTIME_LINE.replace("generation=0", "generation=1")
                + STATUS_OK
            ).encode()
        )
        self.assertEqual(
            activated.require_restore("x", online_cpus=4, generation=1)["cpus"], "4"
        )
        assert activated.boot is not None
        self.assertEqual(activated.boot["cpus"], "1")
        # A launch that neither boots a kernel nor restores has no line to require.
        TimeAbiMonitor(["openvmm"]).feed(b"NVX-TIME-STATUS-EXIT status=0\n")

        with self.assertRaisesRegex(
            TimeAbiFailure, "exited 2: the guest image has no status subcommand"
        ):
            # An old nvx-time's usage text can end without a newline.
            TimeAbiMonitor(KVM_BOOT).feed(
                b"usage: nvx-time boot|exhaustiveNVX-TIME-STATUS-EXIT status=2\n"
            )
        with self.assertRaisesRegex(TimeAbiFailure, "nvx-time status exited 1$"):
            TimeAbiMonitor(KVM_BOOT).feed(
                BOOT_LINE.encode() + b"NVX-TIME-STATUS-EXIT status=1\n"
            )

    def test_builds_the_status_query(self):
        self.assertEqual(
            time_abi.status_script(),
            '/sbin/nvx-time status; echo "NVX-TIME-STATUS-EXIT status=$?"\n',
        )
        # The harness waits longer than the guest, which reports pending checks.
        self.assertGreater(
            time_abi.STATUS_TIMEOUT_SECONDS, time_abi.STATUS_WAIT_SECONDS
        )

    def test_validates_boot_marker_fields_against_the_backend(self):
        cases = (
            ("v=1", "v=2", "version"),
            ("lapic_hz=1000000000", "lapic_hz=200000000", "kvm rate"),
            ("tsc_hz=2194804000", "tsc_hz=400000000", "outside 500 MHz"),
            ("tsc_hz=2194804000", "tsc_hz=fast", "not an integer"),
            ("generation=0", "generation=3", "not 0 at cold boot"),
            ("cpus=4", "cpus=2", "cpus=2 is not 4"),
        )
        for valid, invalid, message in cases:
            fields = time_abi.parse_marker(BOOT_LINE.replace(valid, invalid))
            assert fields is not None
            with self.subTest(invalid=invalid):
                with self.assertRaisesRegex(TimeAbiFailure, message):
                    time_abi.validate_boot_marker(fields, backend="kvm", online_cpus=4)
        mshv = time_abi.parse_marker(
            BOOT_LINE.replace("lapic_hz=1000000000", "lapic_hz=200000000")
        )
        assert mshv is not None
        time_abi.validate_boot_marker(mshv, backend="mshv")

    def test_validates_restore_marker_fields_against_the_backend(self):
        restore = time_abi.parse_marker(RESTORE_LINE)
        assert restore is not None
        time_abi.validate_restore_marker(restore, backend="kvm", online_cpus=4)
        cases = (
            ("generation=2", "generation=0", "generation=0 is not 1 or later"),
            ("generation=2", "generation=x", "generation=x is not 1 or later"),
            ("phase=restore", "phase=boot", "not a passing restore check"),
            ("lapic_hz=1000000000", "lapic_hz=200000000", "kvm rate"),
            ("cpus=4", "cpus=8", "cpus=8 is not 4"),
        )
        for valid, invalid, message in cases:
            fields = time_abi.parse_marker(RESTORE_LINE.replace(valid, invalid))
            assert fields is not None
            with self.subTest(invalid=invalid):
                with self.assertRaisesRegex(
                    TimeAbiFailure, f"restore marker is invalid: .*{message}"
                ):
                    time_abi.validate_restore_marker(
                        fields, backend="kvm", online_cpus=4
                    )
        # A restore of a snapshot captured at generation 0 carries generation 1.
        with self.assertRaisesRegex(TimeAbiFailure, "generation=2 is not 1$"):
            time_abi.validate_restore_marker(restore, backend="kvm", generation=1)
        time_abi.validate_restore_marker(restore, backend="kvm", generation=2)

    def test_classifies_time_abi_exit_statuses_with_the_violation_event(self):
        monitor = TimeAbiMonitor(MSHV_RESTORE)
        monitor.check_exit(0)
        monitor.check_exit(None)
        with self.assertRaisesRegex(
            TimeAbiFailure, "status 195 \\(restore repair\\): no NVX-TIME-ABI"
        ):
            monitor.check_exit(195)
        try:
            monitor.feed(
                b"NVX-TIME-ABI-VIOLATION: v=1 code=G_REPAIR_SAMPLE source=repair "
                b'phase=restore generation=1 boottime_ns=9 detail="epsilon"'
            )
            monitor.finish()
        except TimeAbiFailure:
            pass
        with self.assertRaisesRegex(TimeAbiFailure, "status 195.*G_REPAIR_SAMPLE"):
            monitor.check_exit(195)

    def test_bounds_an_unterminated_line(self):
        monitor = TimeAbiMonitor(KVM_BOOT)
        monitor.feed(b"x" * (time_abi._MAX_PENDING_LINE + 10))  # pyright: ignore[reportPrivateUsage]
        monitor.feed(b"\n" + BOOT_LINE.encode())
        self.assertIsNotNone(monitor.boot)

    def test_records_the_first_fatal_time_abi_error_for_exit_errors(self):
        monitor = TimeAbiMonitor(KVM_BOOT)
        monitor.feed(b"fatal error: failed to open the kernel\r\n")
        self.assertIsNone(monitor.fatal)
        self.assertEqual(str(monitor.exit_error(1)), "OpenVMM exited with status 1")
        # A guest prompt without a newline can precede OpenVMM's line.
        monitor.feed(FATAL + b"    0: [E_TSC_SYNC_UNSUPPORTED] no synchronized set\r\n")
        monitor.feed(b"fatal error: [E_TEST_HOOK] later\r\n")
        self.assertEqual(monitor.fatal, FATAL_DETAIL)
        # The line only explains the exit; it never changes its classification.
        monitor.check_exit(1)
        self.assertEqual(
            str(monitor.exit_error(1, "snapshot source", "during teardown")),
            f"snapshot source exited with status 1 during teardown: {FATAL_DETAIL}",
        )


class WarpProbeTests(unittest.TestCase):
    def test_accepts_conclusive_passing_measurements(self):
        results = time_abi.check_warp_probe(warp_output(), cpus=4, context="test")
        self.assertEqual(results[0]["max_abs_offset_ns"], "15")
        single = time_abi.check_warp_probe(
            warp_output(pairs=0, offset=0), cpus=1, context="one vCPU"
        )
        self.assertEqual(single[0]["pairs"], "0")
        self.assertEqual(
            time_abi.warp_probe_command(),
            "/sbin/nvx-time-probe warp --bound-ns 1000",
        )
        self.assertTrue(time_abi.warp_probe_command(cpus="0-3").endswith("--cpus 0-3"))

    def test_rejects_skew_stalls_and_inconclusive_measurements(self):
        cases = (
            (warp_output(backward=1001), "max_backward_ns=1001"),
            (warp_output(offset=1500, verdict="FAIL"), "max_abs_offset_ns=1500"),
            (warp_output(stalled=1, verdict="FAIL"), "1 CPU pair"),
            (warp_output(conclusive=0), "inconclusive"),
            (warp_output(pairs=3), "measured 3 CPU pairs instead of 6"),
            (warp_output(verdict="FAIL"), "verdict=FAIL"),
            ("nothing here\n", "printed no summary"),
            (
                warp_output().split("NVX-TIME-PROBE warp-detail")[0],
                "1 summaries and 0 detail",
            ),
            (
                "NVX-TIME-PROBE warp pairs=6\nNVX-TIME-PROBE warp-detail x=1\n",
                "incomplete",
            ),
            ('NVX-TIME-PROBE warp detail="\n', "malformed"),
        )
        for text, message in cases:
            with self.subTest(message=message):
                with self.assertRaisesRegex(TimeAbiFailure, message):
                    time_abi.check_warp_probe(text, cpus=4, context="restore 4")

    def test_counts_rounds_and_names_the_failing_round(self):
        self.assertEqual(
            [time_abi.warp_rounds(cpus) for cpus in (1, 2, 8)],
            [1, time_abi.WARP_PROBE_ROUNDS, time_abi.WARP_PROBE_ROUNDS],
        )
        self.assertEqual(time_abi.CI_WARP_GAPS, ("1",))
        self.assertEqual(
            [
                time_abi.warp_rounds(cpus, time_abi.QUALIFICATION_WARP_GAPS)
                for cpus in (1, 8)
            ],
            [1, 5],
        )
        with self.assertRaisesRegex(ValueError, "invalid warp probe idle gaps"):
            time_abi.warp_probe_script(("1; reboot",))
        two = warp_output() + warp_output(offset=30)
        results = time_abi.check_warp_probe(two, cpus=4, context="boot", rounds=2)
        self.assertEqual(
            [result["max_abs_offset_ns"] for result in results], ["15", "30"]
        )
        with self.assertRaisesRegex(TimeAbiFailure, "ran 1 rounds instead of 2"):
            time_abi.check_warp_probe(warp_output(), cpus=4, context="boot", rounds=2)
        with self.assertRaisesRegex(
            TimeAbiFailure, "failed in round 2: max_backward_ns=2000"
        ):
            time_abi.check_warp_probe(
                warp_output() + warp_output(backward=2000, verdict="FAIL"),
                cpus=4,
                context="boot",
            )

    def test_warp_fragment_idles_between_rounds_and_powers_off_on_failure(self):
        shell = shutil.which("sh")
        if shell is None and (git := shutil.which("git")) is not None:
            candidate = Path(git).parent.parent / "bin" / "sh.exe"
            shell = str(candidate) if candidate.is_file() else None
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        fragment = (
            time_abi.warp_probe_script()
            .replace(time_abi.WARP_PROBE_PATH, "probe")
            .replace("nvx-exit", "nvx_exit")
        )

        def run_fragment(
            fragment: str, online: str, failing_round: int = 0
        ) -> subprocess.CompletedProcess[str]:
            assert shell is not None
            return subprocess.run(
                [shell],
                input=(
                    f"cat() {{ printf '%s\\n' '{online}'; }}\n"
                    "calls=0\n"
                    "probe() {\n"
                    "    calls=$((calls + 1))\n"
                    '    printf "probe %s\\n" "$*"\n'
                    f'    [ "$calls" -ne {failing_round} ]\n'
                    "}\n"
                    'sleep() { printf "SLEEP %s\\n" "$1"; }\n'
                    'nvx_exit() { printf "NVX-EXIT %s\\n" "$1"; }\n'
                    + fragment
                    + "echo after\n"
                ),
                text=True,
                capture_output=True,
                timeout=10,
                check=False,
            )

        for online, failing_round, calls, sleeps in (
            ("0-3", 0, 2, 1),
            ("0-3", 2, 2, 1),
            ("0-3", 1, 1, 0),
            ("0", 0, 1, 0),
        ):
            with self.subTest(online=online, failing_round=failing_round):
                result = run_fragment(fragment, online, failing_round)
                lines = result.stdout.splitlines()
                self.assertEqual(
                    lines.count(f"probe warp --bound-ns 1000 --cpus {online}"),
                    calls,
                    result.stdout + result.stderr,
                )
                self.assertEqual(
                    lines.count(f"SLEEP {time_abi.WARP_PROBE_IDLE_SECONDS}"), sleeps
                )
                if failing_round:
                    self.assertEqual(result.returncode, 97)
                    self.assertIn(
                        f"NVX-WARP-PROBE-FAIL status=1 round={failing_round}", lines
                    )
                    self.assertIn("NVX-EXIT 97", lines)
                    self.assertNotIn("NVX-WARP-PROBE-OK", lines)
                    self.assertNotIn("after", lines)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("NVX-WARP-PROBE-OK", lines)
                    self.assertIn("after", lines)
        # H6's qualification schedule idles for each gap in order.
        qualification = fragment.replace(
            'warp_gaps="1"',
            f'warp_gaps="{" ".join(time_abi.QUALIFICATION_WARP_GAPS)}"',
        )
        self.assertEqual(
            qualification,
            time_abi.warp_probe_script(time_abi.QUALIFICATION_WARP_GAPS)
            .replace(time_abi.WARP_PROBE_PATH, "probe")
            .replace("nvx-exit", "nvx_exit"),
        )
        result = run_fragment(qualification, "0-7")
        probe = "probe warp --bound-ns 1000 --cpus 0-7"
        self.assertEqual(
            result.stdout.splitlines(),
            [
                probe,
                "SLEEP 0.1",
                probe,
                "SLEEP 1",
                probe,
                "SLEEP 5",
                probe,
                "SLEEP 1",
                probe,
                "NVX-WARP-PROBE-OK",
                "after",
            ],
            result.stderr,
        )


VIOLATION = (
    b"NVX-TIME-ABI-VIOLATION: v=1 code=G_RCU_STALL source=watcher phase=runtime "
    b'generation=1 boottime_ns=99 detail="rcu: INFO: rcu_preempt self-detected stall"\n'
)


class FakeProcess:
    pid = 123

    def __init__(self, status: int) -> None:
        self.status = status
        self.exited = False
        self.returncode: int | None = None

    def poll(self) -> int | None:
        if self.exited:
            self.returncode = self.status
        return self.returncode

    def wait(self, timeout: float | None = None) -> int:
        del timeout
        self.exited = True
        self.returncode = self.status
        return self.status

    def terminate(self) -> None:
        self.exited = True

    def kill(self) -> None:
        self.exited = True


class FakeInteraction:
    def __init__(self, chunks: list[bytes], status: int) -> None:
        self.process = FakeProcess(status)
        self.chunks = chunks
        self.writes: list[bytes] = []

    def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
        for chunk in self.chunks:
            chunks.put(chunk)
        self.process.exited = True
        chunks.put(None)

    def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
        chunks.put(None)

    def write_input(self, data: bytes) -> None:
        self.writes.append(data)

    def close(self) -> None:
        pass


class ScriptedInteraction(FakeInteraction):
    """A guest that answers each console write with the next scripted reply
    and exits after the last one."""

    def __init__(
        self, chunks: list[bytes], status: int, replies: list[list[bytes]]
    ) -> None:
        super().__init__(chunks, status)
        self.replies = list(replies)
        self._sink: queue.Queue[bytes | None] | None = None

    def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
        self._sink = chunks
        for chunk in self.chunks:
            chunks.put(chunk)
        if not self.replies:
            self._exit()

    def write_input(self, data: bytes) -> None:
        self.writes.append(data)
        assert self._sink is not None
        for chunk in self.replies.pop(0):
            self._sink.put(chunk)
        if not self.replies:
            self._exit()

    def _exit(self) -> None:
        assert self._sink is not None
        self.process.exited = True
        self._sink.put(None)


class RunnerWiringTests(unittest.TestCase):
    @staticmethod
    def _fake_exit(process: FakeProcess, _timeout: float) -> int:
        """Wait for a fake OpenVMM, whose PID is not a real process."""
        return process.wait()

    @staticmethod
    def _teardown_timeout(process: FakeProcess, timeout: float) -> int:
        raise subprocess.TimeoutExpired(str(process.pid), timeout)

    def _measure(
        self,
        chunks: list[bytes],
        status: int,
        command: list[str],
        *,
        teardown_mode: str = "guest-exit",
        teardown_timeout: bool = False,
        log_path: Path | None = None,
    ):
        interaction = FakeInteraction(chunks, status)
        with (
            patch.object(benchmark, "InteractiveProcess", return_value=interaction),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            # Never pidfd_open the fake's PID.
            patch.object(
                benchmark,
                "wait_for_process_exit",
                side_effect=(
                    self._teardown_timeout if teardown_timeout else self._fake_exit
                ),
            ),
        ):
            return benchmark.measure_once(
                command,
                environment={},
                timeout=5,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                teardown_mode=teardown_mode,
                guest_exit_prequeued=True,
                log_path=log_path,
            )

    def test_measure_once_fails_fast_on_a_violation_event(self):
        with self.assertRaisesRegex(RuntimeError, "violation G_RCU_STALL") as raised:
            self._measure(
                [b"restored\n", VIOLATION, benchmark.RESTORE_MARKER + b"\n"],
                194,
                MSHV_RESTORE,
            )
        self.assertIn("--- OpenVMM output ---", str(raised.exception))

    def test_measure_once_classifies_a_time_abi_power_off(self):
        with self.assertRaisesRegex(
            RuntimeError, "status 195 \\(restore repair\\): no NVX-TIME-ABI"
        ):
            self._measure([b"restoring\n"], 195, MSHV_RESTORE)
        with self.assertRaisesRegex(RuntimeError, "OpenVMM exited with status 1"):
            self._measure([b"restoring\n"], 1, MSHV_RESTORE)

    def test_exit_errors_lead_with_openvmm_fatal_time_abi_code(self):
        expected = f"exited with status 1: {re.escape(FATAL_DETAIL)}\n"
        with self.assertRaisesRegex(RuntimeError, f"^OpenVMM {expected}"):
            self._measure([b"restoring\n", FATAL], 1, MSHV_RESTORE)
        interaction = FakeInteraction([b"booting\n", FATAL], 1)
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(
                benchmark, "InteractiveProcess", return_value=interaction
            ):
                with self.assertRaisesRegex(
                    RuntimeError, f"^snapshot source {expected}"
                ):
                    benchmark.capture_snapshot(
                        KVM_BOOT, Path(temporary) / "snapshot", timeout=5
                    )
            log_path = Path(temporary) / "process.log"
            with patch.object(
                openvmm_process,
                "InteractiveProcess",
                return_value=FakeInteraction([FATAL], 1),
            ):
                with openvmm_process.OpenvmmProcess(KVM_BOOT, log_path) as process:
                    with self.assertRaisesRegex(
                        RuntimeError,
                        f"^OpenVMM exited with status 1 before .*: "
                        f"{re.escape(FATAL_DETAIL)}\n",
                    ):
                        process.wait_for(b"NEVER", 1)
                # An expected failure still returns its status and output.
                with patch.object(
                    openvmm_process,
                    "InteractiveProcess",
                    return_value=FakeInteraction([FATAL], 1),
                ):
                    with openvmm_process.OpenvmmProcess(KVM_BOOT, log_path) as process:
                        self.assertEqual(process.wait(1).returncode, 1)

    def test_measure_once_classifies_a_power_off_during_teardown(self):
        interaction = FakeInteraction([benchmark.RESTORE_MARKER + b"\n"], 194)
        with (
            patch.object(benchmark, "InteractiveProcess", return_value=interaction),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            patch.object(benchmark, "wait_for_process_exit", return_value=194),
        ):
            with self.assertRaisesRegex(RuntimeError, "status 194 \\(runtime"):
                benchmark.measure_once(
                    MSHV_RESTORE,
                    environment={},
                    timeout=5,
                    marker=benchmark.RESTORE_MARKER,
                    marker_must_be_line=True,
                    guest_exit_prequeued=True,
                )

    def test_measure_once_scans_the_console_after_the_marker(self):
        # The guest can print a violation during teardown and still exit 0,
        # when its power-off loses the race against the queued guest exit.
        marker = benchmark.RESTORE_MARKER + b"\n"
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "restore.log"
            for logged in (False, True):
                with self.subTest(logged=logged):
                    with self.assertRaisesRegex(
                        RuntimeError, "violation G_RCU_STALL"
                    ) as raised:
                        self._measure(
                            [marker, VIOLATION],
                            0,
                            MSHV_RESTORE,
                            log_path=log_path if logged else None,
                        )
                    self.assertIn("--- OpenVMM output ---", str(raised.exception))
            self.assertIn(VIOLATION, log_path.read_bytes())
        _elapsed, peak, teardown, _wall = self._measure(
            [marker, b"~ # "], 0, MSHV_RESTORE
        )
        self.assertEqual(peak, 1024)
        self.assertIsNotNone(teardown)

    def test_measure_once_scans_an_unterminated_last_line(self):
        marker = benchmark.RESTORE_MARKER + b"\n"
        failed_check = (
            b'NVX-TIME-ABI: v=1 phase=boot status=fail check=C4 detail="kvm-clock"'
        )
        for line, error in (
            (VIOLATION.rstrip(b"\n"), "violation G_RCU_STALL"),
            (failed_check, "boot check C4 failed: kvm-clock"),
        ):
            with self.subTest(error=error):
                with self.assertRaisesRegex(RuntimeError, error):
                    self._measure([marker, line], 0, MSHV_RESTORE)

    def test_measure_once_scans_the_console_after_a_teardown_timeout(self):
        # The harness kills OpenVMM and returns the sample without a teardown
        # time, unless the console or a power-off that beat the kill fails.
        marker = benchmark.RESTORE_MARKER + b"\n"
        _elapsed, peak, teardown, _wall = self._measure(
            [marker], -9, MSHV_RESTORE, teardown_timeout=True
        )
        self.assertEqual(peak, 1024)
        self.assertIsNone(teardown)
        with self.assertRaisesRegex(RuntimeError, "violation G_RCU_STALL"):
            self._measure([marker, VIOLATION], -9, MSHV_RESTORE, teardown_timeout=True)
        with self.assertRaisesRegex(RuntimeError, "status 194 \\(runtime violation"):
            self._measure([marker], 194, MSHV_RESTORE, teardown_timeout=True)

    def test_measure_once_scans_the_console_of_a_terminated_openvmm(self):
        # The status of the termination, -15 on Linux and 1 on Windows, never
        # fails the launch, but the console and a power-off that beat the
        # termination do.
        marker = benchmark.RESTORE_MARKER + b"\n"
        for status in (-15, 1):
            with self.subTest(status=status):
                result = self._measure(
                    [marker], status, MSHV_RESTORE, teardown_mode="host-terminate"
                )
                self.assertEqual(result[1], 1024)
                with self.assertRaisesRegex(RuntimeError, "violation G_RCU_STALL"):
                    self._measure(
                        [marker, VIOLATION],
                        status,
                        MSHV_RESTORE,
                        teardown_mode="host-terminate",
                    )
        with self.assertRaisesRegex(RuntimeError, "status 194 \\(runtime violation"):
            self._measure([marker], 194, MSHV_RESTORE, teardown_mode="host-terminate")

    def test_measure_once_failure_paths_scan_without_masking_their_error(self):
        # The rest of the console still reaches the monitor and the log, but
        # no time ABI failure is raised over the error being handled.
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "restore.log"
            interaction = FakeInteraction([b"NVX-TEST-FAIL broken\n", VIOLATION], 1)
            with patch.object(
                benchmark, "InteractiveProcess", return_value=interaction
            ):
                with self.assertRaises(benchmark.GuestFailureReported) as reported:
                    benchmark.measure_once(
                        MSHV_RESTORE,
                        environment={},
                        timeout=5,
                        marker=benchmark.RESTORE_MARKER,
                        marker_must_be_line=True,
                        guest_exit_prequeued=True,
                        log_path=log_path,
                        failure_marker=b"NVX-TEST-FAIL",
                    )
            self.assertEqual(reported.exception.line, "NVX-TEST-FAIL broken")
            self.assertIn(VIOLATION, log_path.read_bytes())

        killed = threading.Event()

        class KilledProcess(FakeProcess):
            def kill(self) -> None:
                super().kill()
                killed.set()

        class LateEventInteraction(FakeInteraction):
            """A guest whose event reaches the console as OpenVMM stops."""

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(b"booting\n")
                killed.wait(5)
                chunks.put(VIOLATION)
                chunks.put(None)

        interaction = LateEventInteraction([], -9)
        interaction.process = KilledProcess(-9)
        with patch.object(benchmark, "InteractiveProcess", return_value=interaction):
            with self.assertRaisesRegex(
                RuntimeError, "^guest marker was not observed within 0.2s"
            ) as timed_out:
                benchmark.measure_once(
                    MSHV_RESTORE,
                    environment={},
                    timeout=0.2,
                    marker=benchmark.RESTORE_MARKER,
                    marker_must_be_line=True,
                    guest_exit_prequeued=True,
                )
        self.assertIn("G_RCU_STALL", str(timed_out.exception))

    def test_run_guest_script_classifies_a_conformance_power_off(self):
        interaction = FakeInteraction(
            [
                b'NVX-TIME-ABI: v=1 phase=boot status=fail check=C4 detail="kvm-clock"',
            ],
            193,
        )
        with patch.object(benchmark, "InteractiveProcess", return_value=interaction):
            with self.assertRaisesRegex(
                RuntimeError, "boot check C4 failed: kvm-clock"
            ):
                benchmark.run_guest_script(
                    KVM_BOOT, "true\n", b"DONE", timeout=5, boot_marker=b"BOOT"
                )

    def test_cold_boot_runners_query_the_status_before_other_input(self):
        status = time_abi.status_script().encode()
        boot = benchmark.BOOT_MARKER + b": 3.22.1\n"
        # A quiet guest that answers the query without a boot line fails, and
        # never receives the script.
        quiet = ScriptedInteraction([boot], 0, [[STATUS_OK.encode()], [b"DONE\n"]])
        with patch.object(benchmark, "InteractiveProcess", return_value=quiet):
            with self.assertRaisesRegex(RuntimeError, "NVX-TIME-ABI boot marker"):
                benchmark.run_guest_script(
                    KVM_BOOT, "true\n", b"DONE", timeout=5, time_abi_status=True
                )
        self.assertEqual(quiet.writes, [status])
        answered = [BOOT_LINE.encode(), STATUS_OK.encode()]
        conformant = ScriptedInteraction([boot], 0, [answered, [b"DONE\n"]])
        with patch.object(benchmark, "InteractiveProcess", return_value=conformant):
            result = benchmark.run_guest_script(
                KVM_BOOT, "true\n", b"DONE", timeout=5, time_abi_status=True
            )
        self.assertEqual(conformant.writes, [status, b"true\n"])
        self.assertIn("phase=boot status=ok", result["text"])
        # A guest that exits during the query names the query.
        exiting = ScriptedInteraction([boot], 0, [[]])
        with patch.object(benchmark, "InteractiveProcess", return_value=exiting):
            with self.assertRaisesRegex(
                RuntimeError, "nvx-time status did not finish before the guest exited"
            ):
                benchmark.run_guest_script(
                    KVM_BOOT, "true\n", b"DONE", timeout=5, time_abi_status=True
                )
        # Benchmarks and restores never query: the console stays quiet.
        for command in (KVM_BOOT, MSHV_RESTORE):
            silent = ScriptedInteraction([boot], 0, [[b"DONE\n"]])
            with patch.object(benchmark, "InteractiveProcess", return_value=silent):
                benchmark.run_guest_script(
                    command,
                    "true\n",
                    b"DONE",
                    timeout=5,
                    time_abi_status=command is MSHV_RESTORE,
                )
            self.assertEqual(silent.writes, [b"true\n"])
        self._measure([boot, benchmark.RESTORE_MARKER + b"\n"], 0, KVM_BOOT)

    def test_capture_snapshot_queries_the_status_before_the_capture_script(self):
        boot = benchmark.BOOT_MARKER + b": 3.22.1\n"
        answered = [BOOT_LINE.encode(), STATUS_OK.encode()]
        for queried in (False, True):
            replies = [[b"capture script exited\n"]]
            if queried:
                replies.insert(0, answered)
            interaction = ScriptedInteraction([boot], 0, replies)
            with tempfile.TemporaryDirectory() as temporary:
                with patch.object(
                    benchmark, "InteractiveProcess", return_value=interaction
                ):
                    with self.assertRaisesRegex(
                        RuntimeError, "exited before its snapshot request"
                    ):
                        benchmark.capture_snapshot(
                            KVM_BOOT,
                            Path(temporary) / "snapshot",
                            timeout=5,
                            processors=1,
                            time_abi_status=queried,
                        )
            with self.subTest(queried=queried):
                capture = benchmark.prepare_snapshot_capture_script(
                    1, teardown_mode="guest-exit"
                ).encode()
                status = time_abi.status_script().encode()
                expected = [status, capture] if queried else [capture]
                self.assertEqual(interaction.writes, expected)
        quiet = ScriptedInteraction([boot], 0, [[STATUS_OK.encode()]])
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(benchmark, "InteractiveProcess", return_value=quiet):
                with self.assertRaisesRegex(RuntimeError, "NVX-TIME-ABI boot marker"):
                    benchmark.capture_snapshot(
                        KVM_BOOT,
                        Path(temporary) / "snapshot",
                        timeout=5,
                        processors=1,
                        time_abi_status=True,
                    )
        self.assertEqual(len(quiet.writes), 1)

    def test_openvmm_process_queries_the_status_before_the_first_input(self):
        status = time_abi.status_script().encode()
        answered = [BOOT_LINE.encode(), STATUS_OK.encode()]
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "process.log"
            # The scenario's substring wait can return before the boot line
            # ends; its input still follows the finished query.
            guest = ScriptedInteraction(
                [b"NVX-GUEST-BOOT-OK:"], 0, [[b" alpine\n", *answered], [b"OK\n"]]
            )
            with patch.object(
                openvmm_process, "InteractiveProcess", return_value=guest
            ):
                with openvmm_process.OpenvmmProcess(KVM_BOOT, log_path) as process:
                    process.wait_for(b"NVX-GUEST-BOOT-OK:", 1)
                    process.send_line("echo OK")
                    self.assertEqual(process.time_abi.status_queries, 1)
                    process.wait_for_line(b"OK", 1)
                    self.assertEqual(process.wait(1).returncode, 0)
            self.assertEqual(guest.writes, [status, b"echo OK\n"])

            quiet = ScriptedInteraction(
                [b"NVX-GUEST-BOOT-OK: alpine\n"], 0, [[STATUS_OK.encode()], []]
            )
            with patch.object(
                openvmm_process, "InteractiveProcess", return_value=quiet
            ):
                with openvmm_process.OpenvmmProcess(KVM_BOOT, log_path) as process:
                    process.wait_for(b"NVX-GUEST-BOOT-OK:", 1)
                    with self.assertRaisesRegex(
                        RuntimeError, "did not report the NVX-TIME-ABI boot marker"
                    ):
                        process.send_line("echo OK")
            self.assertEqual(quiet.writes, [status])

            # Restores, guests whose shell is on another console, and opted-out
            # processes receive only the scenario's input.
            for command, output, enabled in (
                (MSHV_RESTORE, b"NVX-GUEST-BOOT-OK: alpine\n", True),
                (KVM_BOOT, b"booting\n", True),
                (KVM_BOOT, b"NVX-GUEST-BOOT-OK: alpine\n", False),
            ):
                other = ScriptedInteraction([output], 0, [[]])
                with patch.object(
                    openvmm_process, "InteractiveProcess", return_value=other
                ):
                    with openvmm_process.OpenvmmProcess(
                        command, log_path, time_abi_status=enabled
                    ) as process:
                        process.wait_for(output.rstrip(b"\n"), 1)
                        process.send_bytes(b"\x01")
                        self.assertEqual(process.wait(1).returncode, 0)
                with self.subTest(command=command, output=output, enabled=enabled):
                    self.assertEqual(other.writes, [b"\x01"])

    def test_capture_snapshot_classifies_a_failed_capture_check(self):
        interaction = FakeInteraction([b"booting\n"], 193)
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(
                benchmark, "InteractiveProcess", return_value=interaction
            ):
                with self.assertRaisesRegex(RuntimeError, "status 193 \\(conformance"):
                    benchmark.capture_snapshot(
                        KVM_BOOT,
                        Path(temporary) / "snapshot",
                        timeout=5,
                    )

    def test_openvmm_process_reports_violations_and_time_abi_statuses(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "process.log"
            with patch.object(
                openvmm_process,
                "InteractiveProcess",
                return_value=FakeInteraction([b"FIRST\n", VIOLATION], 194),
            ):
                with openvmm_process.OpenvmmProcess(KVM_BOOT, log_path) as process:
                    process.wait_for(b"FIRST", 1)
                    with self.assertRaisesRegex(RuntimeError, "G_RCU_STALL"):
                        process.wait_for(b"NEVER", 1)
            self.assertIn(b"G_RCU_STALL", log_path.read_bytes())
            with patch.object(
                openvmm_process,
                "InteractiveProcess",
                return_value=FakeInteraction([b"working\n"], 194),
            ):
                with openvmm_process.OpenvmmProcess(KVM_BOOT, log_path) as process:
                    with self.assertRaisesRegex(RuntimeError, "status 194"):
                        process.wait(1)
            with patch.object(
                openvmm_process,
                "InteractiveProcess",
                return_value=FakeInteraction([b"working\n"], 195),
            ):
                with openvmm_process.OpenvmmProcess(KVM_BOOT, log_path) as process:
                    with self.assertRaisesRegex(RuntimeError, "status 195"):
                        process.wait_for_line(b"NEVER", 1)

    def test_openvmm_process_waits_for_time_abi_phase_markers(self):
        restore = (
            b"NVX-TIME-ABI: v=1 phase=restore status=ok cpus=2 tsc_hz=2300000000 "
            b"lapic_hz=200000000 generation=1 elapsed_us=310\n"
        )
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "process.log"
            with patch.object(
                openvmm_process,
                "InteractiveProcess",
                return_value=FakeInteraction([b"x\n", restore], 0),
            ):
                with openvmm_process.OpenvmmProcess(MSHV_RESTORE, log_path) as process:
                    marker = process.wait_for_time_abi("restore", 1)
                    self.assertEqual(marker["generation"], "1")
                    self.assertEqual(process.wait(1).returncode, 0)
            with patch.object(
                openvmm_process,
                "InteractiveProcess",
                return_value=FakeInteraction([b"x\n"], 0),
            ):
                with openvmm_process.OpenvmmProcess(MSHV_RESTORE, log_path) as process:
                    with self.assertRaisesRegex(RuntimeError, "restore marker"):
                        process.wait_for_time_abi("restore", 1)

    def test_tcp_console_monitor_reports_violations(self):
        connection, peer = socket.socketpair()
        console = openvmm_process.TcpConsole(connection, TimeAbiMonitor(KVM_BOOT))
        peer.sendall(VIOLATION)
        with self.assertRaisesRegex(TimeAbiFailure, "G_RCU_STALL"):
            console.wait_for(b"NEVER", 1.0)
        console.close()
        peer.close()

    def test_tcp_console_finish_scans_the_tail(self):
        # A violation after the last awaited marker still fails the scenario,
        # including an unterminated final line.
        connection, peer = socket.socketpair()
        console = openvmm_process.TcpConsole(connection, TimeAbiMonitor(KVM_BOOT))
        peer.sendall(b"READY\n")
        console.wait_for(b"READY", 1.0)
        peer.sendall(VIOLATION.rstrip(b"\n"))
        peer.close()
        with self.assertRaisesRegex(TimeAbiFailure, "G_RCU_STALL"):
            console.finish()
        self.assertIn(b"G_RCU_STALL", console.finish(check=False))
        with self.assertRaisesRegex(TimeAbiFailure, "G_RCU_STALL"):
            console.finish()

        # Error paths keep the tail in the log without raising over the error.
        connection, peer = socket.socketpair()
        console = openvmm_process.TcpConsole(connection, TimeAbiMonitor(KVM_BOOT))
        peer.sendall(VIOLATION)
        peer.close()
        self.assertEqual(console.finish(check=False), VIOLATION)

    def test_tcp_console_queries_a_cold_boot_before_the_first_input(self):
        connection, peer = socket.socketpair()
        peer.settimeout(1.0)
        monitor = TimeAbiMonitor(KVM_BOOT)
        console = openvmm_process.TcpConsole(connection, monitor, time_abi_status=True)
        peer.sendall(b"NVX-GUEST-BOOT-OK: alpine\n")
        console.wait_for(b"NVX-GUEST-BOOT-OK", 1.0)
        peer.sendall((BOOT_LINE + RUNTIME_LINE + STATUS_OK).encode())
        console.send_bytes(b"INPUT\n")
        console.send_bytes(b"MORE\n")
        expected = time_abi.status_script().encode() + b"INPUT\nMORE\n"
        sent = b""
        while len(sent) < len(expected):
            sent += peer.recv(4096)
        self.assertEqual(sent, expected)
        self.assertEqual(monitor.status_queries, 1)
        self.assertIsNotNone(monitor.boot)
        console.close()
        peer.close()

        # A query without a passing boot line fails before the input is sent.
        connection, peer = socket.socketpair()
        console = openvmm_process.TcpConsole(
            connection, TimeAbiMonitor(KVM_BOOT), time_abi_status=True
        )
        peer.sendall(b"NVX-GUEST-BOOT-OK: alpine\n")
        console.wait_for(b"NVX-GUEST-BOOT-OK", 1.0)
        peer.sendall(STATUS_OK.encode())
        with self.assertRaisesRegex(TimeAbiFailure, "boot marker"):
            console.send_bytes(b"INPUT\n")
        console.close()
        peer.close()

    def test_tcp_console_waits_for_a_status_query_to_exit(self):
        connection, peer = socket.socketpair()
        monitor = TimeAbiMonitor(MSHV_RESTORE)
        console = openvmm_process.TcpConsole(connection, monitor)
        restore = RESTORE_LINE.replace("lapic_hz=1000000000", "lapic_hz=200000000")
        peer.sendall((restore + RUNTIME_LINE + STATUS_OK).encode())
        console.wait_for_time_abi_status(1.0)
        self.assertEqual(monitor.status_queries, 1)
        self.assertEqual(len(monitor.restores), 1)
        # A query that reports no restore line fails when it exits.
        peer.sendall(STATUS_OK.encode())
        monitor.restores.clear()
        with self.assertRaisesRegex(TimeAbiFailure, "restore marker"):
            console.wait_for_time_abi_status(1.0)
        with self.assertRaisesRegex(TimeoutError, "did not exit"):
            console.wait_for_time_abi_status(0.2)
        console.close()
        peer.close()
        unmonitored = socket.socket()
        self.addCleanup(unmonitored.close)
        with self.assertRaisesRegex(ValueError, "no time ABI monitor"):
            openvmm_process.TcpConsole(unmonitored).wait_for_time_abi_status(1.0)

    def test_tcp_console_status_query_consumed_with_restore_milestone(self):
        class ScriptedConnection:
            """Returns each scripted chunk from one recv, so the reads do not depend on TCP segmenting."""

            def __init__(self, *chunks: bytes) -> None:
                self._chunks = list(chunks)

            def settimeout(self, _timeout: float) -> None:
                pass

            def recv(self, _size: int) -> bytes:
                if not self._chunks:
                    raise TimeoutError("scripted console is quiet")
                return self._chunks.pop(0)

            def close(self) -> None:
                pass

        monitor = TimeAbiMonitor(MSHV_RESTORE)
        restore = RESTORE_LINE.replace("lapic_hz=1000000000", "lapic_hz=200000000")
        # One read carries the milestone and the whole status query after it.
        connection = ScriptedConnection(
            (
                "NVX-SNAPSHOT-TIER-RELEASED\n" + restore + RUNTIME_LINE + STATUS_OK
            ).encode()
        )
        console = openvmm_process.TcpConsole(cast(socket.socket, connection), monitor)
        baseline = console.status_queries
        console.wait_for(b"NVX-SNAPSHOT-TIER-RELEASED", 1.0)
        self.assertEqual(monitor.status_queries, 1)
        # The query the milestone read consumed counts for a wait since the baseline.
        console.wait_for_time_abi_status(0.2, since=baseline)
        self.assertEqual(monitor.status_queries, 1)
        self.assertEqual(len(monitor.restores), 1)
        # Without a baseline the wait is for the next query, which never comes.
        with self.assertRaisesRegex(TimeoutError, "did not exit"):
            console.wait_for_time_abi_status(0.2)
        console.close()

    def test_tcp_console_queries_only_cold_boots_that_ask(self):
        for command, enabled in (
            (MSHV_RESTORE, True),
            (KVM_BOOT, False),
        ):
            with self.subTest(command=command, enabled=enabled):
                connection, peer = socket.socketpair()
                peer.settimeout(1.0)
                monitor = TimeAbiMonitor(command)
                console = openvmm_process.TcpConsole(
                    connection, monitor, time_abi_status=enabled
                )
                peer.sendall(b"NVX-GUEST-BOOT-OK: alpine\n")
                console.wait_for(b"NVX-GUEST-BOOT-OK", 1.0)
                console.send_bytes(b"Z")
                self.assertEqual(peer.recv(4096), b"Z")
                self.assertEqual(monitor.status_queries, 0)
                console.close()
                peer.close()


def doctor_context(root: Path, backend: str = "kvm") -> doctor.DoctorContext:
    return doctor.DoctorContext(
        backend=backend,
        openvmm=root / "openvmm",
        kernel=root / "vmlinux",
        initrd=root / "initramfs.cpio.gz",
        openvmm_args=(),
        probe_directory=root / "probe",
        timeout=30,
    )


def completed(stdout: str, returncode: int = 0, stderr: str = ""):
    return subprocess.CompletedProcess(["x"], returncode, stdout, stderr)


# Why a host whose chronyd keeps the kernel's PLL off has no PLL frequency.
PLL_OFF = "the kernel PLL is off (adjtimex status=0x0000)"


class FakeClock:
    """A monotonic clock that advances only when something sleeps."""

    def __init__(self) -> None:
        self.now = 0.0
        self.sleeps: list[float] = []

    def monotonic(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.sleeps.append(seconds)
        self.now += seconds


CPU = {
    "vendor": "GenuineIntel",
    "family": "6",
    "model": "106",
    "stepping": "6",
    "microcode": "0xffffffff",
    "brand": "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz",
    "os": "Linux 6.6.150.1-1.azl3",
    "invariant_tsc": "yes",
}
# OpenVMM's cpu_profile FingerprintCheck::summary_line, printed on stderr.
PROFILE_PASS = (
    "NVX-CPU-PROFILE: status=pass backend=kvm generation=icelake-sp "
    f"profile=intel.icelake-sp.v1 profile_digest=sha256:{'a3' * 32} "
    "surface_digest=sha256:6286956acbef host_invariant_tsc=yes\n"
)


class DoctorTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self):
        self.temporary.cleanup()

    def test_check_lines_escape_details_for_field_parsers(self):
        line = doctor.CheckResult("H5", False, 'say "x" \\ y').line()
        self.assertTrue(line.startswith("NVX-DOCTOR: check=H5 status=fail "))
        fields = time_abi.parse_fields(line.removeprefix(doctor.DOCTOR_PREFIX))
        self.assertEqual(fields["detail"], 'say "x" \\ y')

    def test_backend_check_requires_a_usable_device(self):
        context = doctor_context(self.root)
        existing: set[str] = set()

        def exists(path: Path) -> bool:
            return path.as_posix() in existing

        with (
            patch.object(doctor, "host_is_windows", return_value=False),
            patch.object(doctor.Path, "exists", exists),
            patch.object(doctor.os, "access", return_value=True) as access,
        ):
            self.assertIn("does not exist", doctor.check_backend(context).detail)
            existing.add("/dev/kvm")
            self.assertTrue(doctor.check_backend(context).passed)
            existing.add("/dev/mshv")
            self.assertIn("select MSHV", doctor.check_backend(context).detail)
            existing.discard("/dev/mshv")
            access.return_value = False
            self.assertIn("not readable", doctor.check_backend(context).detail)
        with patch.object(doctor, "host_is_windows", return_value=False):
            whp = doctor.check_backend(doctor_context(self.root, "whp"))
        self.assertFalse(whp.passed)
        self.assertIn("requires Windows", whp.detail)

    def test_cpu_check_names_the_generation_and_records_invariant_tsc_as_evidence(
        self,
    ):
        context = doctor_context(self.root)
        with patch.object(doctor, "host_cpu", return_value=dict(CPU)):
            result = doctor.check_cpu(context)
        self.assertTrue(result.passed)
        self.assertIn("generation=icelake-sp", result.detail)
        self.assertIn("profile=intel.icelake-sp.v1", result.detail)
        self.assertEqual(context.facts["generation"], "icelake-sp")
        for unknown in (dict(CPU, model="143"), dict(CPU, model="85", stepping="7")):
            with patch.object(doctor, "host_cpu", return_value=unknown):
                result = doctor.check_cpu(doctor_context(self.root))
            self.assertFalse(result.passed)
            self.assertTrue(result.detail.startswith("[E_PROFILE_HOST_UNKNOWN] "))
            self.assertIn(time_abi.describe_cpu_generations(), result.detail)
        alder_lake = dict(CPU, model="154", stepping="3")
        with patch.object(doctor, "host_cpu", return_value=alder_lake):
            result = doctor.check_cpu(doctor_context(self.root, "whp"))
        self.assertTrue(result.passed, result.detail)
        self.assertIn("generation=alderlake profile=intel.alderlake.v1", result.detail)
        emerald = dict(CPU, model="207", stepping="2")
        with patch.object(doctor, "host_cpu", return_value=emerald):
            for backend in ("kvm", "mshv", "whp"):
                result = doctor.check_cpu(doctor_context(self.root, backend))
                self.assertTrue(result.passed, result.detail)
                self.assertIn("profile=intel.emeraldrapids.v1", result.detail)
        # The host OS's invariant-TSC flags are evidence, never a gate.
        variant = dict(CPU, invariant_tsc="no (missing nonstop_tsc)")
        with patch.object(doctor, "host_cpu", return_value=variant):
            for backend in ("kvm", "mshv", "whp"):
                context = doctor_context(self.root, backend)
                result = doctor.check_cpu(context)
                self.assertTrue(result.passed, result.detail)
                self.assertIn("invariant_tsc=no (missing nonstop_tsc)", result.detail)
                self.assertEqual(
                    context.facts["invariant_tsc"], "no (missing nonstop_tsc)"
                )

    def test_cpu_check_verifies_the_profile_with_openvmm_fingerprint(self):
        fingerprint = self.root / "out" / "fingerprint.json"

        def context_with_openvmm(backend: str = "kvm") -> doctor.DoctorContext:
            context = doctor_context(self.root, backend)
            context.openvmm.write_bytes(b"")
            context.fingerprint = fingerprint
            return context

        def check(
            result: subprocess.CompletedProcess[str],
            cpu: dict[str, str] = CPU,
            backend: str = "kvm",
        ) -> tuple[doctor.CheckResult, doctor.DoctorContext]:
            context = context_with_openvmm(backend)
            with (
                patch.object(doctor, "host_cpu", return_value=dict(cpu)),
                patch.object(doctor.subprocess, "run", return_value=result),
            ):
                return doctor.check_cpu(context), context

        context = context_with_openvmm()
        with (
            patch.object(doctor, "host_cpu", return_value=dict(CPU)),
            patch.object(
                doctor.subprocess, "run", return_value=completed("", 0, PROFILE_PASS)
            ) as run,
        ):
            result = doctor.check_cpu(context)
        self.assertTrue(result.passed, result.detail)
        self.assertEqual(
            run.call_args.args[0],
            [
                str(context.openvmm),
                "--hypervisor",
                "kvm",
                "--cpu-fingerprint",
                str(fingerprint),
            ],
        )
        self.assertEqual(run.call_args.kwargs["env"]["OPENVMM_LOG"], "off")
        self.assertTrue(fingerprint.parent.is_dir())
        self.assertIn("surface_digest=sha256:6286956acbef", result.detail)
        self.assertEqual(context.facts["profile"], "intel.icelake-sp.v1")
        self.assertEqual(context.facts["profile_digest"], f"sha256:{'a3' * 32}")
        self.assertEqual(context.facts["surface_digest"], "sha256:6286956acbef")
        # A later revision of the generation's profile is fine.
        revised = PROFILE_PASS.replace("icelake-sp.v1", "icelake-sp.v2")
        self.assertTrue(check(completed("", 0, revised))[0].passed)
        # OpenVMM's catalog, not NVX's copy, decides which hosts it serves: a
        # generation that only OpenVMM pins passes.
        raptor_lake = PROFILE_PASS.replace("icelake-sp", "raptorlake")
        result, context = check(
            completed("", 0, raptor_lake), dict(CPU, model="183", stepping="1")
        )
        self.assertTrue(result.passed, result.detail)
        self.assertIn(
            "generation=raptorlake profile=intel.raptorlake.v1", result.detail
        )
        self.assertEqual(context.facts["generation"], "raptorlake")
        self.assertEqual(context.facts["profile"], "intel.raptorlake.v1")

        unsupported = (
            "NVX-CPU-PROFILE: status=fail backend=whp generation=icelake-sp "
            "profile=intel.icelake-sp.v1 profile_digest=sha256:a3 "
            "surface_digest=sha256:d67f host_invariant_tsc=no "
            'code=E_PROFILE_UNSUPPORTED detail="CPUID 0x7.0 EDX bits 26, 27 are '
            'not supported; leaf \\"7\\"\\u{a0}subleaf"\n'
        )
        result, context = check(completed("", 1, unsupported), backend="whp")
        self.assertFalse(result.passed)
        self.assertTrue(
            result.detail.startswith(
                "[E_PROFILE_UNSUPPORTED] OpenVMM's CPU profile check failed (exit 1): "
                'CPUID 0x7.0 EDX bits 26, 27 are not supported; leaf "7"\xa0subleaf;'
            ),
            result.detail,
        )
        unknown = (
            "NVX-CPU-PROFILE: status=fail backend=kvm generation=none profile=none "
            "surface_digest=sha256:1 host_invariant_tsc=yes "
            'code=E_PROFILE_HOST_UNKNOWN detail="GenuineIntel family 6 model 143"\n'
        )
        result, context = check(completed("", 1, unknown), dict(CPU, model="143"))
        self.assertFalse(result.passed)
        self.assertTrue(result.detail.startswith("[E_PROFILE_HOST_UNKNOWN] "))
        self.assertIn("[E_PROFILE_HOST_UNKNOWN] OpenVMM's CPU profile", result.detail)
        self.assertEqual(context.facts["profile"], "none")
        for output, message in (
            (
                completed("", 2, "error: unexpected argument '--cpu-fingerprint'"),
                "predates the --cpu-fingerprint tool",
            ),
            (completed("", 1, "fatal error: no backend"), "fatal error: no backend"),
            (
                completed("", 0, PROFILE_PASS.replace("=kvm", "=mshv")),
                "fingerprinted the mshv backend",
            ),
            (
                completed(
                    "",
                    0,
                    PROFILE_PASS.replace(
                        "generation=icelake-sp", "generation=emeraldrapids"
                    ),
                ),
                "CPU profile intel.icelake-sp.v1 of generation icelake-sp for "
                "generation emeraldrapids",
            ),
            (
                completed(
                    "",
                    0,
                    PROFILE_PASS.replace(
                        "profile=intel.icelake-sp.v1", "profile=intel.icelake-sp.v2-rc"
                    ),
                ),
                "CPU profile intel.icelake-sp.v2-rc, which is not a catalog profile ID",
            ),
            (
                completed(
                    "",
                    0,
                    PROFILE_PASS.replace(
                        "generation=icelake-sp profile=intel.icelake-sp.v1",
                        "generation=host profile=intel.host.v1",
                    ),
                ),
                "host profile intel.host.v1, which qualification never accepts",
            ),
        ):
            with self.subTest(message=message):
                result = check(output)[0]
                self.assertFalse(result.passed)
                self.assertIn(message, result.detail)
        # A passing line that lacks the generation or the profile, or leaves
        # one empty, says so instead of classifying a profile that OpenVMM did
        # not report.
        for output_line, unreported in (
            (PROFILE_PASS.replace("generation=icelake-sp ", ""), "generation"),
            (
                PROFILE_PASS.replace("profile=intel.icelake-sp.v1 ", "profile= "),
                "profile",
            ),
            (
                PROFILE_PASS.replace(
                    "generation=icelake-sp profile=intel.icelake-sp.v1 ", ""
                ),
                "generation and profile",
            ),
        ):
            with self.subTest(unreported=unreported):
                result, context = check(completed("", 0, output_line))
                self.assertFalse(result.passed)
                self.assertIn(
                    "OpenVMM's CPU profile check passed without reporting its "
                    f"{unreported};",
                    result.detail,
                )
                self.assertNotIn("OpenVMM reported", result.detail)
                self.assertEqual(
                    context.facts["generation"],
                    "none" if "generation" in unreported else "icelake-sp",
                )
                self.assertEqual(
                    context.facts["profile"],
                    "none" if "profile" in unreported else "intel.icelake-sp.v1",
                )
        missing = doctor_context(self.root / "none")
        missing.fingerprint = fingerprint
        with patch.object(doctor, "host_cpu", return_value=dict(CPU)):
            result = doctor.check_cpu(missing)
        self.assertFalse(result.passed)
        self.assertIn("was not found", result.detail)
        self.assertIn("--no-openvmm", result.detail)
        # Without OpenVMM, H2 checks the identity and generation only.
        with (
            patch.object(doctor, "host_cpu", return_value=dict(CPU)),
            patch.object(doctor.subprocess, "run") as run,
        ):
            result = doctor.check_cpu(doctor_context(self.root))
        self.assertTrue(result.passed)
        self.assertIn("CPU profile not checked", result.detail)
        run.assert_not_called()

    def test_reads_the_first_processor_from_cpuinfo(self):
        path = self.root / "cpuinfo"
        path.write_text(
            "processor\t: 0\nvendor_id\t: GenuineIntel\ncpu family\t: 6\n"
            "model\t\t: 85\nstepping\t: 4\nmicrocode\t: 0x2007108\n"
            "flags\t\t: fpu tsc constant_tsc nonstop_tsc\n\n"
            "processor\t: 1\nvendor_id\t: Other\n",
            encoding="utf-8",
        )
        fields = doctor._linux_cpuinfo(path)  # pyright: ignore[reportPrivateUsage]
        self.assertEqual(fields["vendor_id"], "GenuineIntel")
        self.assertEqual(fields["model"], "85")
        self.assertIn("nonstop_tsc", fields["flags"])

    def test_qualification_rejects_a_host_on_which_auto_falls_back(self):
        """auto falls back to a host profile on a CPU that no built-in profile
        serves, but qualification does not: H2's fingerprint check never falls
        back, and H3 rejects the host profile that OpenVMM's preflight selects,
        so CI's microVM jobs still require a built-in profile."""
        granite_rapids = dict(
            CPU, model="173", stepping="1", brand="Intel(R) Xeon(R) 6973P-C"
        )
        fingerprint = self.root / "fingerprint.json"
        unknown = (
            "NVX-CPU-PROFILE: status=fail backend=kvm generation=none profile=none "
            "surface_digest=sha256:1 host_invariant_tsc=yes "
            'code=E_PROFILE_HOST_UNKNOWN detail="no pinned CPU profile serves '
            'the host CPU, GenuineIntel family 6 model 173 stepping 1"\n'
        )
        fallback = (
            "NVX-TIME-ABI-VERIFY: v=1 status=ok backend=kvm cpu_profile=intel.host.v1 "
            "tsc_hz=2793437000 native_tsc_hz=2793437000 lapic_hz=1000000000 "
            "msr_route=ExitToVmm sync=CommonOffset\n"
        )

        def run(command: list[str], **_: object) -> subprocess.CompletedProcess[str]:
            if "--cpu-fingerprint" in command:
                return completed("", 1, unknown)
            return completed(fallback)

        context = doctor_context(self.root)
        for path in (context.openvmm, context.kernel, context.initrd):
            path.write_bytes(b"")
        context.fingerprint = fingerprint
        with (
            patch.object(doctor, "host_cpu", return_value=granite_rapids),
            patch.object(doctor.subprocess, "run", side_effect=run),
            contextlib.redirect_stdout(io.StringIO()) as lines,
        ):
            cpu, preflight = doctor.run_checks(context, ("H2", "H3"))
        self.assertIn("NVX-DOCTOR: check=H2 status=fail", lines.getvalue())
        self.assertIn("NVX-DOCTOR: check=H3 status=fail", lines.getvalue())
        self.assertEqual((cpu.check, cpu.passed), ("H2", False))
        self.assertTrue(cpu.detail.startswith("[E_PROFILE_HOST_UNKNOWN] "), cpu.detail)
        self.assertEqual((preflight.check, preflight.passed), ("H3", False))
        self.assertIn(
            "cpu_profile=intel.host.v1 (a host profile, to which --cpu-profile auto "
            "falls back",
            preflight.detail,
        )
        # Without OpenVMM, NVX's copy of the catalog fails the CPU too.
        context.fingerprint = None
        with patch.object(doctor, "host_cpu", return_value=granite_rapids):
            result = doctor.check_cpu(context)
        self.assertFalse(result.passed)
        self.assertTrue(result.detail.startswith("[E_PROFILE_HOST_UNKNOWN] "))

    def test_identifies_the_host_cpu_signature(self):
        HostCpu = time_abi.HostCpu
        brand = "12th Gen Intel(R) Core(TM) i9-12900H"

        def registry(key: str, name: str) -> object:
            self.assertEqual(
                (key, name), (doctor.WINDOWS_PROCESSOR_KEY, "ProcessorNameString")
            )
            return f"{brand}  "

        with (
            patch.object(doctor, "host_is_windows", return_value=True),
            patch.object(doctor, "_windows_registry_value", side_effect=registry),
        ):
            for processor, expected in (
                (
                    "Intel64 Family 6 Model 154 Stepping 3, GenuineIntel",
                    HostCpu("GenuineIntel", 6, 154, 3, brand),
                ),
                (
                    "AMD64 Family 25 Model 33 Stepping 0, AuthenticAMD",
                    HostCpu("AuthenticAMD", 25, 33, 0, brand),
                ),
                ("", None),
            ):
                with (
                    self.subTest(processor=processor),
                    patch.object(doctor.platform, "processor", return_value=processor),
                ):
                    self.assertEqual(doctor.host_cpu_signature(), expected)
        # A brand string that the registry lacks leaves the signature.
        with (
            patch.object(doctor, "host_is_windows", return_value=True),
            patch.object(
                doctor.platform,
                "processor",
                return_value="Intel64 Family 6 Model 154 Stepping 3, GenuineIntel",
            ),
            patch.object(
                doctor, "_windows_registry_value", side_effect=OSError("no value")
            ),
        ):
            self.assertEqual(
                doctor.host_cpu_signature(), HostCpu("GenuineIntel", 6, 154, 3)
            )
        cpuinfo = {
            "vendor_id": "AuthenticAMD",
            "cpu family": "25",
            "model": "17",
            "stepping": "1",
        }
        with patch.object(doctor, "host_is_windows", return_value=False):
            for fields, expected in (
                (cpuinfo, HostCpu("AuthenticAMD", 25, 17, 1)),
                (
                    {**cpuinfo, "model name": "AMD EPYC 9V74 80-Core Processor"},
                    HostCpu(
                        "AuthenticAMD", 25, 17, 1, "AMD EPYC 9V74 80-Core Processor"
                    ),
                ),
                ({**cpuinfo, "model": "?"}, None),
                ({"vendor_id": "AuthenticAMD"}, None),
            ):
                with (
                    self.subTest(fields=fields),
                    patch.object(doctor, "_linux_cpuinfo", return_value=fields),
                ):
                    self.assertEqual(doctor.host_cpu_signature(), expected)
            with patch.object(
                doctor, "_linux_cpuinfo", side_effect=FileNotFoundError("cpuinfo")
            ):
                self.assertIsNone(doctor.host_cpu_signature())

    def test_preflight_parses_openvmm_verification(self):
        # The line format of OpenVMM's openvmm_entry verify.rs.
        line = (
            "NVX-TIME-ABI-VERIFY: v=1 status=ok backend=kvm "
            "cpu_profile=intel.icelake-sp.v1 tsc_hz=2793437000 "
            "native_tsc_hz=2793437000 lapic_hz=1000000000 msr_route=ExitToVmm "
            "sync=CommonOffset\n"
        )

        def context_with_files(backend: str = "kvm") -> doctor.DoctorContext:
            context = doctor_context(self.root, backend)
            for path in (context.openvmm, context.kernel, context.initrd):
                path.write_bytes(b"")
            return context

        context = context_with_files()
        with patch.object(
            doctor.subprocess, "run", return_value=completed(line)
        ) as run:
            result = doctor.check_openvmm_preflight(context)
        self.assertTrue(result.passed, result.detail)
        self.assertEqual(context.facts["native_tsc_hz"], "2793437000")
        self.assertEqual(context.facts["profile"], "intel.icelake-sp.v1")
        command = run.call_args.args[0]
        self.assertEqual(command[-1], "--x-time-abi-verify")
        self.assertEqual(command[command.index("--machine") + 1], "microvm")
        self.assertEqual(command[command.index("--kernel") + 1], str(context.kernel))
        # --openvmm-arg passes development options, such as a test hook.
        hooked = context_with_files()
        hooked.openvmm_args = ("--x-time-abi-test-hook", "force-utc-downtime")
        with patch.object(
            doctor.subprocess, "run", return_value=completed(line)
        ) as run:
            self.assertTrue(doctor.check_openvmm_preflight(hooked).passed)
        self.assertEqual(
            run.call_args.args[0][-3:],
            ["--x-time-abi-test-hook", "force-utc-downtime", "--x-time-abi-verify"],
        )
        failure = (
            "NVX-TIME-ABI-VERIFY: v=1 status=fail backend=kvm "
            'code=E_TSC_SYNC_UNSUPPORTED detail="failed: \\"sync\\" unsupported"\n'
        )
        cases = (
            (
                completed(failure, 1),
                '[E_TSC_SYNC_UNSUPPORTED] OpenVMM verification failed (exit 1): failed: "sync" unsupported',
            ),
            (
                completed(
                    "", 2, "error: unexpected argument '--x-time-abi-verify' found"
                ),
                "predates the time ABI",
            ),
            # The flip removed --x-time-abi-v1; passing it is reported as is.
            (
                completed("", 2, "error: unexpected argument '--x-time-abi-v1' found"),
                "unexpected argument '--x-time-abi-v1' found",
            ),
            (completed(line.replace("1000000000", "200000000")), "[E_LAPIC_RATE_"),
            (
                completed(line.replace("tsc_hz=2793437000", "tsc_hz=4000", 1)),
                "[E_TSC_RATE_",
            ),
            (
                completed(line.replace("backend=kvm", "backend=mshv")),
                "verified backend mshv",
            ),
            # Without H2 in the same run, H3 still requires a catalog profile.
            (
                completed(line.replace(" cpu_profile=intel.icelake-sp.v1", "")),
                "verified no catalog CPU profile: cpu_profile=none",
            ),
            (
                completed(line.replace("intel.icelake-sp.v1", "interim.host.kvm.v1")),
                "verified no catalog CPU profile: cpu_profile=interim.host.kvm.v1",
            ),
            # Qualification never accepts a host profile, such as the one to
            # which auto falls back on a CPU that no built-in profile serves.
            (
                completed(line.replace("intel.icelake-sp.v1", "intel.host.v1")),
                "verified no catalog CPU profile: cpu_profile=intel.host.v1 (a host "
                "profile, to which --cpu-profile auto falls back on a CPU that no "
                "built-in profile serves, and which qualification never accepts)",
            ),
        )
        for result_value, message in cases:
            with self.subTest(message=message):
                with patch.object(doctor.subprocess, "run", return_value=result_value):
                    result = doctor.check_openvmm_preflight(context_with_files())
                self.assertFalse(result.passed)
                self.assertIn(message, result.detail)
        # H3 records a missing profile as none, as H2 does.
        context = context_with_files()
        unprofiled = line.replace(" cpu_profile=intel.icelake-sp.v1", "")
        with patch.object(doctor.subprocess, "run", return_value=completed(unprofiled)):
            self.assertFalse(doctor.check_openvmm_preflight(context).passed)
        self.assertEqual(context.facts["profile"], "none")
        for selected, passed in (
            ("intel.icelake-sp.v2", True),
            ("intel.skylake-sp.v1", False),
            # A revision is the lineage plus .v and a number.
            ("intel.icelake-sp.v2-rc", False),
            # OpenVMM selects catalog profiles, so an interim one is a regression.
            ("interim.host.kvm.v1", False),
        ):
            with self.subTest(selected=selected):
                context = context_with_files()
                context.facts["profile"] = "intel.icelake-sp.v1"
                output = line.replace("intel.icelake-sp.v1", selected)
                with patch.object(
                    doctor.subprocess, "run", return_value=completed(output)
                ):
                    result = doctor.check_openvmm_preflight(context)
                self.assertEqual(result.passed, passed, result.detail)
                self.assertEqual(context.facts["profile"], selected)
        missing = doctor.check_openvmm_preflight(doctor_context(self.root / "none"))
        self.assertFalse(missing.passed)
        self.assertIn("was not found", missing.detail)
        # Without H2, a catalog profile of a generation that only OpenVMM pins
        # passes.
        with patch.object(
            doctor.subprocess,
            "run",
            return_value=completed(
                line.replace("intel.icelake-sp.v1", "intel.raptorlake.v1")
            ),
        ):
            result = doctor.check_openvmm_preflight(context_with_files())
        self.assertTrue(result.passed, result.detail)

    def test_the_rate_limit_is_the_guests_frequency_budget(self):
        # #1008: H4's limit is what the guest's wall-clock discipline leaves
        # for the host's clock correction: Linux's frequency limit, to which
        # nvx-rs clamps the correction, less a restore's rate deviation,
        # which nvx-rs's restore packet decoder bounds by the time ABI's
        # restore tolerance, the one that OpenVMM's rate policy applies.
        root = Path(__file__).resolve().parents[1]
        decoders = rust_integer_constants(
            (root / "nvx-rs" / "src" / "guest" / "time" / "decoders.rs").read_text(
                encoding="utf-8"
            )
        )
        scaled_ppm = decoders["SCALED_PPM"]
        self.assertEqual(scaled_ppm, 1 << 16)
        self.assertEqual(
            decoders["FREQUENCY_LIMIT"], doctor.GUEST_FREQUENCY_LIMIT_PPM * scaled_ppm
        )
        self.assertEqual(
            decoders["RATE_DEVIATION_LIMIT"], doctor.RESTORE_TOLERANCE_PPM * scaled_ppm
        )
        self.assertEqual(
            doctor.RATE_TOLERANCE_PPM,
            (decoders["FREQUENCY_LIMIT"] - decoders["RATE_DEVIATION_LIMIT"])
            / scaled_ppm,
        )
        rate = rust_integer_constants(
            pinned_openvmm_file(self, "vmm_core/virt/src/time_abi/rate.rs")
        )
        self.assertEqual(rate["TSC_TOLERANCE_PPM"], doctor.RESTORE_TOLERANCE_PPM)

    def test_rate_check_applies_the_spec_bounds_to_sleep_separated_samples(self):
        def records(
            rates: list[float],
            *,
            disciplined: list[float] | None = None,
            interval_s: float = 10.0,
            bracket: int = 40,
            clock_hz: int = 1_000_000_000,
            resolution_ns: float = 1.0,
            names: tuple[str, str] = ("CLOCK_MONOTONIC", "CLOCK_MONOTONIC_RAW"),
        ) -> list[tuple[str, dict[str, str]]]:
            """Probe records whose consecutive samples measure ``rates`` against
            the stability clock and ``disciplined`` (default ``rates``) against
            the rate clock."""
            result: list[tuple[str, dict[str, str]]] = []
            for role, name in zip(("rate", "stability"), names, strict=True):
                clock = {"role": role, "name": name, "hz": str(clock_hz)}
                clock["resolution_ns"] = f"{resolution_ns:.3f}"
                result.append(("clock", clock))
            series = {"rate": disciplined or rates, "stability": rates}
            state = {role: (10**12, 5 * clock_hz) for role in series}
            for index in range(len(rates) + 1):
                sample = {"index": str(index)}
                for role, role_rates in series.items():
                    tsc, ticks = state[role]
                    if index:
                        tsc += round(role_rates[index - 1] * interval_s)
                        ticks += round(interval_s * clock_hz)
                    state[role] = (tsc, ticks)
                    sample[f"{role}_tsc"] = str(tsc)
                    sample[f"{role}_clock"] = str(ticks)
                    sample[f"{role}_bracket"] = str(bracket)
                result.append(("sample", sample))
            return result

        def check(
            context: doctor.DoctorContext,
            probe_records: list[tuple[str, dict[str, str]]],
            clocksource: str = "tsc",
            windows: bool = False,
            chrony: doctor.ChronyFrequency
            | str
            | Sequence[doctor.ChronyFrequency | str] = "chronyc is not installed",
            clock: FakeClock | None = None,
            kernel: doctor.KernelFrequency | str = PLL_OFF,
            boot: doctor.BootCorrection | None = None,
        ) -> doctor.CheckResult:
            """Run H4; a list of chronyd readings answers its re-reads too."""
            clock = clock or FakeClock()
            readings = chrony if isinstance(chrony, list | tuple) else None
            with (
                patch.object(doctor, "run_probe", return_value=probe_records) as run,
                patch.object(doctor, "host_clocksource", return_value=clocksource),
                patch.object(doctor, "host_is_windows", return_value=windows),
                patch.object(
                    doctor,
                    "chrony_frequency",
                    return_value=None if readings else chrony,
                    side_effect=readings,
                ) as frequency,
                patch.object(doctor, "kernel_frequency", return_value=kernel) as pll,
                patch.object(doctor, "boot_correction", return_value=boot) as average,
                patch.object(doctor.time, "monotonic", clock.monotonic),
                patch.object(doctor.time, "sleep", clock.sleep),
            ):
                result = doctor.check_rate(context)
            # Windows reads QueryPerformanceCounter, which no daemon steers.
            self.assertEqual(frequency.called, not windows)
            self.assertEqual(average.called, not windows)
            # H4 reads the kernel's PLL only without chronyd's estimate.
            first = readings[0] if readings else chrony
            self.assertEqual(pll.called, not windows and isinstance(first, str))
            self.assertEqual(
                run.call_args.args[1:],
                (
                    "rate",
                    "--samples",
                    str(context.schedule.rate_samples),
                    "--interval-ms",
                    str(context.schedule.rate_interval_ms),
                ),
            )
            # The probe sleeps through its sampling window, which the timeout
            # allows on top of --timeout (120 s on the qualification schedule).
            self.assertEqual(
                run.call_args.kwargs,
                {
                    "duration": (context.schedule.rate_samples - 1)
                    * context.schedule.rate_interval_ms
                    / 1000
                },
            )
            return result

        def ci_context(backend: str = "kvm") -> doctor.DoctorContext:
            context = doctor_context(self.root, backend)
            context.schedule = doctor.CI_SCHEDULE
            return context

        steady = [2793437000.0 + jitter for jitter in (0, 200, -150, 100) * 3]
        context = doctor_context(self.root)
        context.facts["tsc_hz"] = "2793437000"
        result = check(context, records(steady))
        self.assertTrue(result.passed, result.detail)
        self.assertIn(
            "against CLOCK_MONOTONIC from 13 samples 10 s apart; against "
            "CLOCK_MONOTONIC_RAW (resolution 1 ns) the interval rates agree within "
            "0.125 ppm, largest interval uncertainty 0.004 ppm",
            result.detail,
        )
        self.assertIn("0.013 ppm from the backend's 2793437000 Hz", result.detail)
        self.assertIn("host clocksource tsc (evidence)", result.detail)
        self.assertIn(
            "no chronyd frequency estimate: chronyc is not installed (evidence); "
            f"no kernel PLL frequency: {PLL_OFF} (evidence)",
            result.detail,
        )
        self.assertEqual(context.facts["rate_agreement_ppm"], "0.125")
        self.assertEqual(context.facts["rate_deviation_ppm"], "0.013")
        self.assertEqual(context.facts["rate_reference"], "CLOCK_MONOTONIC")
        # chrony steers CLOCK_MONOTONIC's rate by ppm between seconds, which
        # says nothing about the TSC: the undisciplined clock judges stability,
        # and the disciplined one the rate.
        steered = [2300000100.0 + jitter for jitter in (0, 6900, -4600)] * 4
        context = doctor_context(self.root, "mshv")
        context.facts["native_tsc_hz"] = "2300000000"
        result = check(context, records([2300000100.0] * 12, disciplined=steered))
        self.assertTrue(result.passed, result.detail)
        self.assertIn("agree within 0.000 ppm", result.detail)
        self.assertEqual(context.facts["measured_tsc_hz"], "2300000867")
        self.assertIn("0.377 ppm from the backend's 2300000000 Hz", result.detail)
        # CI's short schedule; Windows reads QueryPerformanceCounter, which time
        # synchronization never steers, in both roles. Its 100 ns tick keeps a
        # 1 s interval conclusive at 0.1 ppm.
        qpc = records(
            [2300000100.0, 2300000200.0],
            interval_s=1.0,
            bracket=0,
            clock_hz=10_000_000,
            resolution_ns=100.0,
            names=("QueryPerformanceCounter", "QueryPerformanceCounter"),
        )
        windows = check(ci_context("whp"), qpc, windows=True)
        self.assertTrue(windows.passed, windows.detail)
        self.assertIn(
            "against QueryPerformanceCounter from 3 samples 1 s apart", windows.detail
        )
        self.assertIn("uncertainty 0.100 ppm", windows.detail)
        self.assertNotIn("clocksource", windows.detail)
        # Hyper-V's reference TSC page advances CLOCK_MONOTONIC_RAW in 100 ns
        # steps: the resolution, not the empty brackets, bounds the measurement.
        tsc_page = records(
            [2300000100.0, 2300000200.0], interval_s=1.0, bracket=0, resolution_ns=100.0
        )
        result = check(ci_context("mshv"), tsc_page, "hyperv_clocksource_tsc_page")
        self.assertTrue(result.passed, result.detail)
        self.assertIn("CLOCK_MONOTONIC_RAW (resolution 100 ns)", result.detail)
        self.assertIn("uncertainty 0.100 ppm", result.detail)
        coarse = records(
            [2300000100.0, 2300000200.0], interval_s=1.0, bracket=0, resolution_ns=300.0
        )
        result = check(ci_context("mshv"), coarse)
        self.assertFalse(result.passed)
        self.assertIn(
            "inconclusive: an interval's uncertainty is 0.300 ppm", result.detail
        )
        for probe_records, message in (
            (
                records([2793437000.0, 2793440000.0] * 6),
                "unstable: interval rates differ by 1.074 ppm > 1.0 ppm",
            ),
            (
                records([2793437000.0] * 2, interval_s=1.0, bracket=400),
                "inconclusive: an interval's uncertainty is 0.401 ppm > 0.25 ppm",
            ),
            (records([2794200000.0] * 12), "273.140 ppm from the measured rate"),
            (records([100_000_000.0] * 12), "[E_TSC_RATE_IMPLAUSIBLE]"),
        ):
            with self.subTest(message=message):
                context = doctor_context(self.root)
                if "inconclusive" in message:
                    context = ci_context()
                context.facts["tsc_hz"] = "2793437000"
                result = check(context, probe_records)
                self.assertFalse(result.passed)
                self.assertIn(message, result.detail)
        with self.assertRaisesRegex(doctor.ScriptError, "printed 3 samples, not 13"):
            check(doctor_context(self.root), records([2793437000.0] * 2))
        with self.assertRaisesRegex(doctor.ScriptError, "no stability clock"):
            check(ci_context(), records([2793437000.0] * 2, interval_s=1.0)[:1])
        # The clocksource is evidence on every backend.
        for backend, clocksource in (
            ("kvm", "hpet"),
            ("mshv", "hyperv_clocksource_tsc_page"),
        ):
            with self.subTest(backend=backend, clocksource=clocksource):
                context = doctor_context(self.root, backend)
                context.facts["native_tsc_hz"] = "2300000000"
                result = check(context, records([2300000100.0] * 12), clocksource)
                self.assertTrue(result.passed, result.detail)
                self.assertIn(
                    f"host clocksource {clocksource} (evidence)", result.detail
                )
                self.assertEqual(context.facts["host_clocksource"], clocksource)
        # Without H3 (validate-runner), H4 still checks stability.
        standalone = check(ci_context(), records([2300000100.0] * 2, interval_s=1.0))
        self.assertTrue(standalone.passed, standalone.detail)
        self.assertIn("not compared with the backend's rate", standalone.detail)
        # #768: on azure-mshv-1, whose host clock jumps, chronyd slewed
        # CLOCK_MONOTONIC so that a 2 s window read 946.1 ppm, while the
        # TSC ran at its usual rate, 44 ppm from the declared one, against
        # chronyd's frequency estimate.
        slewed = records(
            [2300000000.0] * 2, disciplined=[2302176486.0] * 2, interval_s=1.0
        )
        estimate = doctor.ChronyFrequency("PHC0", 44.3, 4.2)
        context = ci_context("mshv")
        context.facts["native_tsc_hz"] = "2300000507"
        result = check(context, slewed, chrony=estimate)
        self.assertTrue(result.passed, result.detail)
        self.assertIn(
            "tsc_hz=2300101895 against CLOCK_MONOTONIC_RAW corrected by chronyd's "
            "frequency estimate of +44.300 ppm (skew 4.200 ppm, reference PHC0) "
            "from 3 samples 1 s apart",
            result.detail,
        )
        self.assertIn("44.082 ppm from the backend's 2300000507 Hz", result.detail)
        self.assertIn(
            "2302176486 Hz against CLOCK_MONOTONIC, which chronyd also slews "
            "(evidence)",
            result.detail,
        )
        self.assertEqual(context.facts["measured_tsc_hz"], "2300101895")
        self.assertEqual(
            context.facts["rate_reference"], "chronyd +44.300 ppm (skew 4.200 ppm)"
        )
        # Without chronyd's estimate or a kernel PLL, the slewed window is all
        # H4 can measure; a chronyd that isn't synchronized leaves the kernel
        # clock unsynchronized too.
        context = ci_context("mshv")
        context.facts["native_tsc_hz"] = "2300000507"
        unsynchronized = (
            "the kernel clock is not synchronized (adjtimex state=5 status=0x0041)"
        )
        result = check(
            context,
            slewed,
            chrony="chronyd is not synchronized",
            kernel=unsynchronized,
        )
        self.assertFalse(result.passed)
        self.assertIn(
            "the backend rate is 946.078 ppm from the measured rate (limit 250 ppm)",
            result.detail,
        )
        self.assertIn(
            "no chronyd frequency estimate: chronyd is not synchronized (evidence); "
            f"no kernel PLL frequency: {unsynchronized} (evidence)",
            result.detail,
        )
        # #1008: a kernel PLL that another time service drives, such as
        # systemd-timesyncd, keeps its frequency estimate apart from the
        # offsets that it slews, so H4 corrects CLOCK_MONOTONIC_RAW by that
        # instead of reading the slewed window.
        context = ci_context("mshv")
        context.facts["native_tsc_hz"] = "2300000507"
        result = check(context, slewed, kernel=doctor.KernelFrequency(-44.3))
        self.assertTrue(result.passed, result.detail)
        self.assertIn(
            "tsc_hz=2300101895 against CLOCK_MONOTONIC_RAW corrected by the kernel "
            "PLL's frequency of -44.300 ppm from 3 samples 1 s apart",
            result.detail,
        )
        self.assertIn("44.082 ppm from the backend's 2300000507 Hz", result.detail)
        self.assertIn(
            "no chronyd frequency estimate: chronyc is not installed (evidence); "
            "2302176486 Hz against CLOCK_MONOTONIC, which the kernel PLL also slews "
            "(evidence)",
            result.detail,
        )
        self.assertEqual(context.facts["rate_reference"], "kernel PLL -44.300 ppm")
        # #1008: the average correction since boot, CLOCK_MONOTONIC_RAW's rate
        # against CLOCK_MONOTONIC over the uptime, averages the reference's
        # wander out. On azure-mshv-1, it read +43.559 ppm over 38.5 h while
        # chronyd's estimate wandered. It is evidence only.
        boot = doctor.BootCorrection(43.559, 138_435.1)
        average = (
            "2300100190 Hz against CLOCK_MONOTONIC_RAW corrected by the average "
            "correction since boot, +43.559 ppm over 38.5 h"
        )
        context = ci_context("mshv")
        context.facts["native_tsc_hz"] = "2300000507"
        wandering = doctor.ChronyFrequency("PHC0", 189.3, 1)
        result = check(context, slewed, chrony=wandering, boot=boot)
        self.assertTrue(result.passed, result.detail)
        self.assertIn("189.115 ppm from the backend's 2300000507 Hz", result.detail)
        self.assertIn(
            f"; {average}, 43.340 ppm from the backend's rate (evidence)",
            result.detail,
        )
        self.assertEqual(context.facts["boot_correction"], "+43.559 ppm over 38.5 h")
        # Without H3, it has no rate to compare.
        result = check(ci_context("mshv"), slewed, chrony=wandering, boot=boot)
        self.assertIn(f"; {average} (evidence)", result.detail)
        # chronyd's estimate gates the backend's rate, whatever the window
        # reads, and its skew bounds the estimate: the rate passes only when
        # every rate within the skew lies within the limit.
        steady = records([2300000000.0] * 2, interval_s=1.0)
        for estimate, declared, passed, message in (
            (
                doctor.ChronyFrequency("PHC0", 80, 10),
                "2300000507",
                True,
                "79.786 ppm from the",
            ),
            # #1008: the 100 ppm of the time ABI v1 bounded the TSC's error
            # alone, which a measurement can't tell from the reference's
            # wander; azure-mshv-1 failed at +127.9 ppm, 117.3 to 138.6 ppm
            # within chronyd's skew.
            (
                doctor.ChronyFrequency("PHC0", 150, 1),
                "2300000507",
                True,
                "149.802 ppm from the",
            ),
            (
                doctor.ChronyFrequency("PHC0", 240, 15),
                "2300000507",
                False,
                "the measurement is inconclusive: the backend rate is 239.837 ppm "
                "from the measured rate, 224.830 to 254.845 ppm within chronyd's "
                "skew of 15.000 ppm, which straddles the limit of 250 ppm",
            ),
            (
                doctor.ChronyFrequency("PHC0", 300, 1),
                "2300000507",
                False,
                "the backend rate is 299.870 ppm from the measured rate, 298.869 "
                "to 300.870 ppm within chronyd's skew of 1.000 ppm (limit 250 ppm)",
            ),
            # The correction isn't linear in the frequency: the estimate's
            # upper bound, 250 ppm from the declared rate, corrects the rate
            # by 250.063 ppm, and a range that includes the declared rate
            # starts at zero.
            (
                doctor.ChronyFrequency("PHC0", 0, 250),
                "2300000000",
                False,
                "the measurement is inconclusive: the backend rate is 0.000 ppm "
                "from the measured rate, 0.000 to 250.063 ppm within chronyd's "
                "skew of 250.000 ppm, which straddles the limit of 250 ppm",
            ),
        ):
            with self.subTest(estimate=estimate):
                context = ci_context("mshv")
                context.facts["native_tsc_hz"] = declared
                clock = FakeClock()
                result = check(context, steady, chrony=estimate, clock=clock)
                self.assertEqual(result.passed, passed, result.detail)
                self.assertIn(message, result.detail)
                # Only a deviation that passes above 100 ppm warns.
                deviation = float(context.facts["rate_deviation_ppm"])
                self.assertEqual(bool(result.warning), passed and deviation > 100)
                # A range that straddles the limit stays inconclusive when
                # chronyd's estimate doesn't move for the 60 s of re-reads.
                straddles = "straddles" in message
                self.assertEqual(clock.sleeps, [2.0] * 30 if straddles else [])
                self.assertEqual(
                    "no re-read of chronyd's estimate in 60 s was conclusive"
                    in result.detail,
                    straddles,
                )
        # #1008: a range that straddles the limit makes H4 re-read chronyd's
        # estimate every 2 s, for up to 60 s, and use the first conclusive
        # one. On azure-mshv-1, chronyd's estimate read +189.3 ppm with a skew
        # of 130.6 ppm, and +117.1 ppm with a skew of 55.1 ppm 50 s later.
        straddling = doctor.ChronyFrequency("PHC0", 189.3, 130.6)
        for readings, passed, measured, message in (
            (
                [
                    straddling,
                    straddling,
                    "chronyc tracking exited 1: 506 Cannot talk to daemon",
                    doctor.ChronyFrequency("PHC0", 117.1, 55.1),
                ],
                True,
                "2300269362",
                "116.893 ppm from the backend's 2300000507 Hz; chronyd's first "
                "estimate, +189.300 ppm (skew 130.600 ppm), straddled the limit, "
                "so H4 re-read it for 6 s (evidence)",
            ),
            # The first conclusive estimate decides, even when it fails.
            (
                [straddling, doctor.ChronyFrequency("PHC0", 300, 1)],
                False,
                "2300690207",
                "the backend rate is 299.870 ppm from the measured rate, 298.869 "
                "to 300.870 ppm within chronyd's skew of 1.000 ppm (limit 250 ppm)",
            ),
        ):
            with self.subTest(readings=readings):
                context = ci_context("mshv")
                context.facts["native_tsc_hz"] = "2300000507"
                clock = FakeClock()
                result = check(context, steady, chrony=readings, clock=clock)
                self.assertEqual(result.passed, passed, result.detail)
                self.assertIn(message, result.detail)
                self.assertEqual(clock.sleeps, [2.0] * (len(readings) - 1))
                # The settled +117.1 ppm passes with a warning.
                self.assertEqual(bool(result.warning), passed)
                # Only chronyd's estimate changes, so H4 corrects the same
                # samples by the estimate that it uses.
                self.assertEqual(context.facts["measured_tsc_hz"], measured)
                used = cast(doctor.ChronyFrequency, readings[-1])
                self.assertEqual(
                    context.facts["rate_reference"],
                    f"chronyd {used.ppm:+.3f} ppm (skew {used.skew_ppm:.3f} ppm)",
                )
        # Without the backend's rate, H4 has no range to re-read for.
        clock = FakeClock()
        result = check(ci_context("mshv"), steady, chrony=straddling, clock=clock)
        self.assertTrue(result.passed, result.detail)
        self.assertEqual(clock.sleeps, [])
        # The deviation keeps three decimals, so a failure never reads as the
        # limit itself: #768's second failure was 100.027 ppm, at the 100 ppm
        # limit of the time.
        context = ci_context("mshv")
        context.facts["native_tsc_hz"] = "2300000507"
        result = check(context, records([2300575569.0] * 2, interval_s=1.0))
        self.assertFalse(result.passed)
        self.assertIn(
            "the backend rate is 250.027 ppm from the measured rate (limit 250 ppm)",
            result.detail,
        )
        self.assertEqual(context.facts["rate_deviation_ppm"], "250.027")
        self.assertEqual(result.warning, "")
        # #1008: above the time ABI v1's 100 ppm, H4 passes with a warning
        # that leads its detail and says how much of the guest's budget the
        # host's correction uses; 100 ppm itself doesn't warn.
        for rate, warning in (
            (
                2300230569.0,
                "the backend rate is 100.027 ppm from the measured rate, more than "
                "100 ppm: the host's clock correction uses 40% of the guest's "
                "250 ppm budget, and one run can't tell the TSC's error from the "
                "reference clock's wander",
            ),
            (2300230507.0, ""),
        ):
            with self.subTest(rate=rate):
                context = ci_context("mshv")
                context.facts["native_tsc_hz"] = "2300000507"
                result = check(context, records([rate] * 2, interval_s=1.0))
                self.assertTrue(result.passed, result.detail)
                self.assertEqual(result.warning, warning)
                self.assertEqual(
                    result.detail.startswith(f"warning: {warning}; tsc_hz="),
                    bool(warning),
                )

    def test_chrony_frequency_reads_a_synchronized_tracking_report(self):
        report = (
            "50484330,PHC0,1,1792707652.172549767,-0.000001016,-0.000001624,"
            "0.000031799,38.638,-0.024,4.116,0.000000001,0.000059705,8.0,{leap}\n"
        )
        chronyc = "/usr/bin/chronyc"
        with (
            patch.object(doctor.shutil, "which", return_value=chronyc),
            patch.object(
                doctor.subprocess,
                "run",
                return_value=completed(report.format(leap="Normal")),
            ) as run,
        ):
            self.assertEqual(
                doctor.chrony_frequency(),
                doctor.ChronyFrequency("PHC0", 38.638, 4.116),
            )
        self.assertEqual(run.call_args.args[0], [chronyc, "-c", "-n", "tracking"])
        self.assertEqual(
            run.call_args.kwargs["timeout"], doctor.CHRONYC_TIMEOUT_SECONDS
        )
        unrecognized = "chronyc tracking printed an unrecognized report"
        for result_value, reason in (
            (
                completed(report.format(leap="Not synchronised")),
                "chronyd is not synchronized",
            ),
            (
                completed("", 1, "506 Cannot talk to daemon\n"),
                "chronyc tracking exited 1: 506 Cannot talk to daemon",
            ),
            (completed("Reference ID : 50484330 (PHC0)\n"), unrecognized),
            (
                completed(report.format(leap="Normal").replace("38.638", "nan")),
                unrecognized,
            ),
            (completed(report.format(leap="Normal,extra")), unrecognized),
            # chronyd's skew is an error bound, never negative.
            (
                completed(report.format(leap="Normal").replace("4.116", "-4.116")),
                unrecognized,
            ),
            (subprocess.TimeoutExpired(["chronyc"], 5), "chronyc tracking failed"),
            (
                FileNotFoundError(2, "No such file or directory"),
                "chronyc tracking failed",
            ),
        ):
            with self.subTest(reason=reason, result=result_value):
                run_patch = (
                    patch.object(doctor.subprocess, "run", side_effect=result_value)
                    if isinstance(result_value, BaseException)
                    else patch.object(
                        doctor.subprocess, "run", return_value=result_value
                    )
                )
                with (
                    patch.object(doctor.shutil, "which", return_value=chronyc),
                    run_patch,
                ):
                    frequency = doctor.chrony_frequency()
                self.assertIsInstance(frequency, str)
                self.assertIn(reason, cast(str, frequency))
        with patch.object(doctor.shutil, "which", return_value=None):
            self.assertEqual(doctor.chrony_frequency(), "chronyc is not installed")

    def test_kernel_frequency_reads_a_synchronized_kernel_pll(self):
        # adjtimex's state and the status, freq, and tick of its struct timex.
        def kernel(state: int, status: int, freq: int, tick: int = 10_000):
            timex = types.SimpleNamespace(status=status, freq=freq, tick=tick)
            return patch.object(doctor, "_adjtimex", return_value=(state, timex))

        # systemd-timesyncd sets STA_PLL and STA_NANO. The frequency is in ppm
        # scaled by 2^16, and each microsecond of tick above 10,000 adds
        # 100 ppm, as chronyd's ticks of 9,999 take 100 ppm away.
        for tick, ppm in ((10_000, -44.25), (10_001, 55.75), (9_999, -144.25)):
            with self.subTest(tick=tick), kernel(0, 0x2001, -2_899_968, tick):
                self.assertEqual(doctor.kernel_frequency(), doctor.KernelFrequency(ppm))
        for state, status, reason in (
            # chronyd keeps the PLL off, and slews through the frequency.
            (0, 0x0000, "the kernel PLL is off (adjtimex status=0x0000)"),
            (
                5,
                0x2041,
                "the kernel clock is not synchronized (adjtimex state=5 status=0x2041)",
            ),
            (
                5,
                0x2001,
                "the kernel clock is not synchronized (adjtimex state=5 status=0x2001)",
            ),
            (
                0,
                0x0041,
                "the kernel clock is not synchronized (adjtimex state=0 status=0x0041)",
            ),
        ):
            with self.subTest(state=state, status=status):
                with kernel(state, status, -2_899_968):
                    self.assertEqual(doctor.kernel_frequency(), reason)
        with patch.object(doctor.ctypes, "CDLL", side_effect=OSError("no libc")):
            self.assertEqual(
                doctor.kernel_frequency(), "adjtimex is unavailable: no libc"
            )

    def test_boot_correction_averages_the_clocks_since_boot(self):
        if sys.platform == "win32":
            # Windows has no CLOCK_MONOTONIC_RAW, and no daemon steers
            # QueryPerformanceCounter.
            self.assertIsNone(doctor.boot_correction())
            return
        # azure-mshv-1, 38.5 h after boot: CLOCK_MONOTONIC trailed
        # CLOCK_MONOTONIC_RAW by 6.03 s.
        clocks = {
            time.CLOCK_MONOTONIC_RAW: 138_435_100_000_000,
            time.CLOCK_MONOTONIC: 138_429_069_887_000,
        }
        with patch.object(
            doctor.time, "clock_gettime_ns", side_effect=clocks.__getitem__
        ):
            correction = doctor.boot_correction()
        assert correction is not None
        self.assertAlmostEqual(correction.ppm, 43.559, places=3)
        self.assertEqual(correction.seconds, 138_435.1)
        # This host's clocks give a finite average over its uptime.
        correction = doctor.boot_correction()
        assert correction is not None
        self.assertTrue(math.isfinite(correction.ppm))
        self.assertGreater(correction.seconds, 0)

    def test_rate_check_continues_when_the_clocksource_is_unreadable(self):
        # #507: the host clocksource is evidence only, so an unreadable
        # sysfs file must not fail H4; the native probe still runs and the
        # verdict depends on the measurements alone.
        def records(gaps: tuple[int, int]) -> list[tuple[str, dict[str, str]]]:
            result: list[tuple[str, dict[str, str]]] = [
                (
                    "clock",
                    {
                        "role": role,
                        "name": name,
                        "hz": "1000000000",
                        "resolution_ns": "1.000",
                    },
                )
                for role, name in (
                    ("rate", "CLOCK_MONOTONIC"),
                    ("stability", "CLOCK_MONOTONIC_RAW"),
                )
            ]
            tsc = 0
            for index in range(3):
                sample = {"index": str(index)}
                for role in ("rate", "stability"):
                    sample[f"{role}_tsc"] = str(tsc)
                    sample[f"{role}_clock"] = str(index * 1_000_000_000)
                    sample[f"{role}_bracket"] = "20"
                tsc += gaps[min(index, 1)]
                result.append(("sample", sample))
            return result

        error = FileNotFoundError(2, "No such file or directory", "/sys/fake")
        for gaps, passed, marker in (
            ((3_000_000_000, 3_000_000_000), True, "host clocksource unknown"),
            ((3_000_000_000, 3_100_000_000), False, "interval rates differ"),
        ):
            with self.subTest(passed=passed):
                context = doctor_context(self.root)
                context.schedule = doctor.CI_SCHEDULE
                with (
                    patch.object(
                        doctor, "run_probe", return_value=records(gaps)
                    ) as run,
                    patch.object(doctor, "host_clocksource", side_effect=error),
                    patch.object(doctor, "host_is_windows", return_value=False),
                    patch.object(
                        doctor,
                        "chrony_frequency",
                        return_value="chronyc is not installed",
                    ),
                    patch.object(doctor, "kernel_frequency", return_value=PLL_OFF),
                ):
                    result = doctor.check_rate(context)
                self.assertTrue(run.called)
                self.assertEqual(result.passed, passed, result.detail)
                self.assertIn(marker, result.detail)
                self.assertIn("evidence", result.detail)
                self.assertEqual(
                    context.facts["host_clocksource"],
                    f"unknown ({error})",
                )

    def test_host_skew_check_applies_the_bound(self):
        summary = {
            "pairs": "28",
            "cpus": "0-7",
            "max_abs_offset_ns": "40",
            "max_uncertainty_ns": "120",
            "stalled_pairs": "0",
            "conclusive": "1",
        }
        context = doctor_context(self.root)
        context.facts["measured_tsc_hz"] = "2793437000"
        with patch.object(doctor, "run_probe", return_value=[("skew", summary)]) as run:
            self.assertTrue(doctor.check_host_skew(context).passed)
        self.assertIn("--tsc-hz", run.call_args.args)
        for changes, message in (
            ({"max_abs_offset_ns": "1500"}, "exceeds 1000"),
            ({"stalled_pairs": "2", "conclusive": "0"}, "2 CPU pair(s) stalled"),
            ({"conclusive": "0"}, "inconclusive"),
        ):
            with (
                self.subTest(message=message),
                patch.object(
                    doctor, "run_probe", return_value=[("skew", summary | changes)]
                ),
            ):
                result = doctor.check_host_skew(doctor_context(self.root))
                self.assertFalse(result.passed)
                self.assertIn(message, result.detail)

    def test_guest_warp_check_runs_the_idle_schedules_and_applies_the_bound(self):
        context = doctor_context(self.root)
        for path in (context.openvmm, context.kernel, context.initrd):
            path.write_bytes(b"")

        def boot(cpus: int) -> str:
            line = BOOT_LINE.replace("cpus=4", f"cpus={cpus}")
            return " ALPINE-MICROVM-BOOT-OK: 3.22.1\n" + line + STATUS_OK

        smp = boot(8) + warp_output(pairs=28) * 4 + warp_output(pairs=28, offset=60)
        # A newer guest also reports the boot check's CPU time.
        single = boot(1).replace("elapsed_us=1873", "elapsed_us=1873 cpu_us=1100")
        single += warp_output(pairs=0, offset=0)
        with (
            patch.object(doctor.os, "cpu_count", return_value=8),
            patch.object(
                doctor,
                "run_guest_script",
                side_effect=[{"text": smp}, {"text": single}],
            ) as run,
        ):
            result = doctor.check_guest_warp(context)
        self.assertTrue(result.passed, result.detail)
        self.assertIn(
            "vcpus=8 rounds=5 idle_gaps_s=0.1,1,5,1 boot_elapsed_us=1873;",
            result.detail,
        )
        self.assertIn(
            "vcpus=1 rounds=1 idle_gaps_s=none boot_elapsed_us=1873 boot_cpu_us=1100",
            result.detail,
        )
        self.assertEqual(context.facts["guest_warp_ns"], "60")
        (smp_call, single_call) = run.call_args_list
        for call, processors, gaps in (
            (smp_call, "8", time_abi.QUALIFICATION_WARP_GAPS),
            (single_call, "1", ()),
        ):
            command, script, marker = call.args
            self.assertEqual(command[command.index("--processors") + 1], processors)
            self.assertEqual(script, time_abi.warp_probe_script(gaps) + "nvx-exit 0\n")
            self.assertEqual(marker, time_abi.WARP_PROBE_COMPLETION_MARKER)
            self.assertIs(call.kwargs["time_abi_status"], True)
        # CI's schedule runs the probe twice with a 1 s gap.
        context.schedule = doctor.CI_SCHEDULE
        ci = boot(8) + warp_output(pairs=28) * 2
        with (
            patch.object(doctor.os, "cpu_count", return_value=8),
            patch.object(
                doctor,
                "run_guest_script",
                side_effect=[{"text": ci}, {"text": single}],
            ),
        ):
            result = doctor.check_guest_warp(context)
        self.assertTrue(result.passed, result.detail)
        self.assertIn("vcpus=8 rounds=2 idle_gaps_s=1 ", result.detail)
        context.schedule = doctor.QUALIFICATION_SCHEDULE
        cases = (
            (
                [boot(8) + warp_output(pairs=28) * 2],
                "vcpus=8",
                "ran 2 rounds instead of 5",
            ),
            (
                [
                    boot(8)
                    + warp_output(pairs=28) * 4
                    + warp_output(pairs=28, offset=1500)
                ],
                "vcpus=8",
                "in round 5: max_abs_offset_ns=1500",
            ),
            (
                [STATUS_OK + warp_output(pairs=28) * 5],
                "vcpus=8",
                "NVX-TIME-ABI boot marker",
            ),
            (
                [BOOT_LINE.replace("cpus=4", "cpus=8") + warp_output(pairs=28) * 5],
                "vcpus=8",
                "nvx-time status did not finish before the warp probe",
            ),
            (
                [smp, boot(1) + warp_output(pairs=0, backward=2000, verdict="FAIL")],
                "vcpus=1",
                "max_backward_ns=2000",
            ),
        )
        for outputs, guest, message in cases:
            with (
                self.subTest(message=message),
                patch.object(doctor.os, "cpu_count", return_value=8),
                patch.object(
                    doctor,
                    "run_guest_script",
                    side_effect=[{"text": output} for output in outputs],
                ),
            ):
                result = doctor.check_guest_warp(context)
                self.assertFalse(result.passed)
                self.assertTrue(result.detail.startswith(f"{guest}: "), result.detail)
                self.assertIn(message, result.detail)
        # A 1-vCPU host runs only the 1-vCPU guest.
        with (
            patch.object(doctor.os, "cpu_count", return_value=1),
            patch.object(
                doctor, "run_guest_script", return_value={"text": single}
            ) as run,
        ):
            result = doctor.check_guest_warp(context)
        self.assertTrue(result.passed, result.detail)
        self.assertEqual(run.call_count, 1)
        self.assertTrue(result.detail.startswith("vcpus=1 rounds=1 idle_gaps_s=none"))
        missing = doctor.check_guest_warp(doctor_context(self.root / "missing"))
        self.assertIn("OpenVMM was not found", missing.detail)

    def test_utc_check_reads_the_host_clock_discipline(self):
        context = doctor_context(self.root)
        with patch.object(doctor, "host_is_windows", return_value=False):
            with patch.object(doctor, "_linux_clock_state", return_value=(0, 0x2001)):
                self.assertTrue(doctor.check_utc(context).passed)
            with patch.object(doctor, "_linux_clock_state", return_value=(5, 0x0041)):
                self.assertIn("STA_UNSYNC", doctor.check_utc(context).detail)
        status = (
            "Leap Indicator: 0(no warning)\nStratum: 2\n"
            "Source: VM IC Time Synchronization Provider\n"
        )
        with (
            patch.object(doctor, "host_is_windows", return_value=True),
            patch.object(doctor.subprocess, "run", return_value=completed(status)),
        ):
            result = doctor.check_utc(context)
        self.assertTrue(result.passed, result.detail)
        self.assertIn("VM IC Time Synchronization Provider", result.detail)
        for unsynchronized in (
            status.replace("0(no warning)", "3(not synchronized)"),
            "Leap Indicator: 0\nSource: Local CMOS Clock\n",
        ):
            with (
                patch.object(doctor, "host_is_windows", return_value=True),
                patch.object(
                    doctor.subprocess, "run", return_value=completed(unsynchronized)
                ),
            ):
                self.assertFalse(doctor.check_utc(context).passed)
        # Output H7 can't read, such as a localized w32tm's, fails it with a
        # stable leading phrase instead of passing as an unknown source.
        unverified = "cannot verify host UTC synchronization: "
        for outcome in (
            completed(
                "Sprungindikator: 0(keine Warnung)\nQuelle: time.windows.com,0x9\n"
            ),
            completed("Leap Indicator: 0(no warning)\nStratum: 2\n"),
            completed("Stratum: 2\nSource: VM IC Time Synchronization Provider\n"),
            completed(status.replace("0(no warning)", "unknown")),
            completed("The service has not been started. (0x80070426)\n", 1),
            subprocess.TimeoutExpired(["w32tm"], 30),
        ):
            with (
                patch.object(doctor, "host_is_windows", return_value=True),
                patch.object(doctor.subprocess, "run", side_effect=[outcome]),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                (result,) = doctor.run_checks(context, ("H7",))
            self.assertFalse(result.passed, result.detail)
            self.assertTrue(result.detail.startswith(unverified), result.detail)
        # So does an adjtimex clock state Linux doesn't define, or no adjtimex.
        with (
            patch.object(doctor, "host_is_windows", return_value=False),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            with patch.object(doctor, "_linux_clock_state", return_value=(6, 0)):
                (unknown_state,) = doctor.run_checks(context, ("H7",))
            with patch.object(doctor.ctypes, "CDLL", side_effect=OSError("no libc")):
                (no_adjtimex,) = doctor.run_checks(context, ("H7",))
        for result in (unknown_state, no_adjtimex):
            self.assertFalse(result.passed, result.detail)
            self.assertTrue(result.detail.startswith(unverified), result.detail)

    def test_run_prints_lines_writes_the_summary_and_fails_closed(self):
        def passing(check: str):
            def run_check(context: doctor.DoctorContext) -> doctor.CheckResult:
                context.facts["generation"] = "emeraldrapids"
                return doctor.CheckResult(check, True, f"{check} ok")

            return run_check

        def broken(context: doctor.DoctorContext) -> doctor.CheckResult:
            del context
            raise doctor.ScriptError("rustc is required to build the host time probe")

        summary = self.root / "summary.md"
        arguments = doctor_parser().parse_args(
            [
                "--backend",
                "mshv",
                "--checks",
                "H5",
                "H2",
                "--summary",
                str(summary),
                "--probe-dir",
                str(self.root),
            ]
        )
        stdout = io.StringIO()
        with (
            patch.dict(doctor.CHECKS, {"H2": passing("H2"), "H5": passing("H5")}),
            contextlib.redirect_stdout(stdout),
        ):
            self.assertEqual(doctor.run(arguments), 0)
        lines = stdout.getvalue().splitlines()
        self.assertEqual(
            [line.split()[1] for line in lines if line.startswith("NVX-DOCTOR")],
            ["check=H2", "check=H5"],
        )
        self.assertIn("passed; generation emeraldrapids", lines[-1])
        self.assertIn("(mshv): passed", summary.read_text(encoding="utf-8"))
        with (
            patch.dict(doctor.CHECKS, {"H2": passing("H2"), "H5": broken}),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(doctor.run(arguments), 1)
        text = summary.read_text(encoding="utf-8")
        self.assertIn("**fail** | rustc is required", text)
        self.assertIn("Generation: `emeraldrapids`", text)

        # Unreadable values that break a computation, such as a zero rate,
        # fail only their own check, and the later checks still run.
        def arithmetic(context: doctor.DoctorContext) -> doctor.CheckResult:
            del context
            raise ZeroDivisionError("float division by zero")

        stdout = io.StringIO()
        with (
            patch.dict(doctor.CHECKS, {"H2": arithmetic, "H5": passing("H5")}),
            contextlib.redirect_stdout(stdout),
        ):
            self.assertEqual(doctor.run(arguments), 1)
        self.assertIn(
            'check=H2 status=fail detail="float division by zero"', stdout.getvalue()
        )
        self.assertIn("check=H5 status=pass", stdout.getvalue())

    def test_warnings_reach_the_line_the_summary_and_ci_annotations(self):
        warning = "the host's clock correction uses 60% of the guest's 250 ppm budget"

        def warned(context: doctor.DoctorContext) -> doctor.CheckResult:
            del context
            return doctor.CheckResult(
                "H4", True, f"warning: {warning}; tsc_hz=2300000000", warning
            )

        context = doctor_context(self.root, "mshv")
        results: list[doctor.CheckResult] = []
        for actions, annotated in (("true", True), ("", False)):
            with self.subTest(actions=actions):
                stdout = io.StringIO()
                with (
                    patch.dict(doctor.CHECKS, {"H4": warned}),
                    patch.dict(doctor.os.environ, {"GITHUB_ACTIONS": actions}),
                    contextlib.redirect_stdout(stdout),
                ):
                    results = doctor.run_checks(context, ("H4",))
                lines = stdout.getvalue().splitlines()
                # The check passes, and its line states the warning.
                self.assertEqual(lines[0], results[0].line())
                self.assertIn('status=pass detail="warning: ', lines[0])
                # In GitHub Actions, an annotation repeats it, escaped.
                annotation = (
                    "::warning title=Time ABI host qualification (mshv)::H4 host TSC "
                    "rate stability: the host's clock correction uses 60%25 of the "
                    "guest's 250 ppm budget"
                )
                self.assertEqual(lines[1:], [annotation] if annotated else [])
        summary = doctor.summary_markdown("mshv", results, {})
        self.assertIn("(mshv): passed", summary)
        self.assertIn(f"\n\n**H4 warning:** {warning}\n", summary)

    def test_run_selects_the_cpu_fingerprint_or_runs_without_openvmm(self):
        seen: list[Path | None] = []
        schedules: list[doctor.Schedule] = []

        def cpu(context: doctor.DoctorContext) -> doctor.CheckResult:
            seen.append(context.fingerprint)
            schedules.append(context.schedule)
            context.facts["surface_digest"] = "sha256:62"
            return doctor.CheckResult("H2", True, "ok")

        summary = self.root / "summary.md"
        common = ["--backend", "kvm", "--checks", "H2", "--summary", str(summary)]
        common += ["--probe-dir", str(self.root)]
        with (
            patch.dict(doctor.CHECKS, {"H2": cpu}),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            for extra in (
                [],
                ["--cpu-fingerprint", str(self.root / "fingerprint.json")],
                ["--no-openvmm", "--ci-schedule"],
            ):
                arguments = doctor_parser().parse_args([*common, *extra])
                self.assertEqual(doctor.run(arguments), 0)
        self.assertEqual(
            seen,
            [
                self.root / "nvx-cpu-fingerprint-kvm.json",
                self.root / "fingerprint.json",
                None,
            ],
        )
        self.assertEqual(
            schedules,
            [doctor.QUALIFICATION_SCHEDULE] * 2 + [doctor.CI_SCHEDULE],
        )
        self.assertEqual(doctor.QUALIFICATION_SCHEDULE.rate_samples, 13)
        self.assertEqual(doctor.QUALIFICATION_SCHEDULE.rate_interval_ms, 10_000)
        self.assertEqual(doctor.CI_SCHEDULE.rate_samples, 3)
        self.assertEqual(doctor.CI_SCHEDULE.rate_interval_ms, 1_000)
        self.assertIn("CPU surface digest: `sha256:62`", summary.read_text("utf-8"))
        arguments = doctor_parser().parse_args(["--backend", "kvm", "--no-openvmm"])
        with self.assertRaisesRegex(doctor.ScriptError, "cannot run H3 and H6"):
            doctor.run(arguments)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            doctor_parser().parse_args(
                ["--backend", "kvm", "--no-openvmm", "--cpu-fingerprint", "x"]
            )

    def test_builds_the_host_probe_once_per_source_version(self):
        target = doctor.probe_path(self.root)
        self.assertRegex(target.name, r"^nvx-host-time-probe-[0-9a-f]{16}(\.exe)?$")

        def rustc(command: list[str], **_kwargs: object):
            Path(command[command.index("-o") + 1]).write_bytes(b"probe")
            return completed("")

        with patch.object(
            doctor, "require_tool", side_effect=doctor.ScriptError("rustc is required")
        ) as require_tool:
            with self.assertRaisesRegex(doctor.ScriptError, "rustc is required"):
                doctor.build_probe(self.root)
        require_tool.assert_called_once_with(
            "rustc", "rustc is required to build the host time probe"
        )
        with (
            patch.object(doctor, "require_tool", return_value="rustc"),
            patch.object(doctor.subprocess, "run", side_effect=rustc) as run,
        ):
            self.assertEqual(doctor.build_probe(self.root), target)
            self.assertEqual(doctor.build_probe(self.root), target)
        run.assert_called_once()
        self.assertEqual(target.read_bytes(), b"probe")
        target.unlink()
        with (
            patch.object(doctor, "require_tool", return_value="rustc"),
            patch.object(
                doctor.subprocess, "run", return_value=completed("", 1, "error[E0425]")
            ),
        ):
            with self.assertRaisesRegex(doctor.ScriptError, "E0425"):
                doctor.build_probe(self.root)

    def test_parses_probe_records(self):
        context = doctor_context(self.root)
        context.probe = self.root / "probe.exe"
        output = (
            "noise\nNVX-HOST-TIME-PROBE rate index=1 tsc_hz=1.5\n"
            "NVX-HOST-TIME-PROBE skew pairs=1 cpus=0-1\n"
        )
        with patch.object(
            doctor.subprocess, "run", return_value=completed(output)
        ) as run:
            records = doctor.run_probe(context, "skew")
        self.assertEqual(run.call_args.kwargs["timeout"], context.timeout)
        self.assertEqual(
            records,
            [
                ("rate", {"index": "1", "tsc_hz": "1.5"}),
                ("skew", {"pairs": "1", "cpus": "0-1"}),
            ],
        )
        with patch.object(
            doctor.subprocess, "run", return_value=completed(output)
        ) as run:
            doctor.run_probe(context, "rate", duration=120.0)
        self.assertEqual(run.call_args.kwargs["timeout"], context.timeout + 120.0)
        with patch.object(
            doctor.subprocess, "run", return_value=completed("", 3, "no CPU")
        ):
            with self.assertRaisesRegex(doctor.ScriptError, "skew failed: no CPU"):
                doctor.run_probe(context, "skew")


def doctor_parser():
    import argparse

    parser = argparse.ArgumentParser()
    doctor.configure_parser(parser)
    return parser


if __name__ == "__main__":
    unittest.main()

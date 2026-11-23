"""Fixture-only architecture wiring for the current public managed helpers."""

import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import (  # noqa: E402
    managed_exec_tests as managed,
)
from nvx_tools import (  # noqa: E402
    sandbox_lifecycle_tests as lifecycle,
)
from nvx_tools import (  # noqa: E402
    sandbox_proxy_tests as proxy,
)
from nvx_tools import (  # noqa: E402
    sandbox_ram_upper_tests as ram,
)
from nvx_tools import ubuntu  # noqa: E402
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
    InitramfsBuildConstants,
    OpenVMMBuildConstants,
    UbuntuBuildConstants,
)
from nvx_tools.common import ScriptError, sha256_file  # noqa: E402


class Stage3ManagedTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(
            prefix=".s3-managed-", dir=Path(__file__).resolve().parents[1]
        )
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.build = self.root / "build"
        build_directory = patch.object(BuildConstants, "BUILD_DIR", self.build)
        build_directory.start()
        self.addCleanup(build_directory.stop)
        self.artifacts = managed.WorkloadArtifacts(
            self.root / "distro.erofs",
            "12345678-1234-1234-1234-123456789abc",
            self.root / "scratch.ext4",
        )
        self.artifacts.distro.write_bytes(b"fixture distro")
        self.artifacts.scratch_template.write_bytes(b"fixture scratch")

    def acceptance(
        self,
        kind: type[lifecycle.LifecycleAcceptance] = lifecycle.LifecycleAcceptance,
        **selection: Any,
    ) -> lifecycle.LifecycleAcceptance:
        return kind(
            "whp",
            timeout=2,
            output_dir=self.root / "results",
            artifacts=self.artifacts,
            root=self.root / "fixture",
            **selection,
        )

    def arm_manifest(self) -> tuple[Path, Path, dict[str, object]]:
        paths = managed.workload_artifact_paths(architecture="aarch64")
        for path, _ in paths:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"owned artifact")
        distro, manifest, _ = (path for path, _ in paths)
        digest = ubuntu.converter_input_sha256(
            ubuntu.customization_files(architecture="aarch64"), architecture="aarch64"
        )
        document: dict[str, object] = {
            "format": UbuntuBuildConstants.PACKAGE_MANIFEST_VERSION,
            "guest": UbuntuBuildConstants.GUEST_NAME,
            "architecture": "arm64",
            "artifact": distro.name,
            "artifact_sha256": sha256_file(distro),
            "input_sha256": digest,
            "uuid": ubuntu.erofs_uuid(digest),
        }
        manifest.write_text(json.dumps(document), encoding="utf-8")
        return distro, manifest, document

    def test_artifact_namespaces_and_legacy_loader_shape(self) -> None:
        legacy = managed.workload_artifact_paths()
        arm = managed.workload_artifact_paths(architecture="aarch64")
        self.assertEqual(
            [path for path, _ in arm],
            [self.build / "aarch64" / path.name for path, _ in legacy],
        )
        for path, _ in legacy:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"legacy artifact")
        legacy[1][0].write_text(
            json.dumps({"uuid": self.artifacts.layer_uuid}), encoding="utf-8"
        )
        with patch.object(
            managed, "workload_artifact_paths", wraps=managed.workload_artifact_paths
        ) as paths:
            loaded = managed.load_workload_artifacts()
        paths.assert_called_once_with()
        self.assertEqual(loaded.distro, legacy[0][0])
        self.assertEqual(loaded.layer_uuid, self.artifacts.layer_uuid)
        with self.assertRaises(ScriptError):
            managed.workload_artifact_paths(architecture="unknown")

    def test_arm_admission_binds_manifest_isa_current_inputs_image_and_uuid(
        self,
    ) -> None:
        distro, manifest, document = self.arm_manifest()
        loaded = managed.load_workload_artifacts(architecture="aarch64")
        self.assertEqual(loaded.distro, distro)
        self.assertEqual(loaded.layer_uuid, document["uuid"])
        mutations: dict[str, object] = {
            "architecture": "amd64",
            "input_sha256": "0" * 64,
            "artifact_sha256": "0" * 64,
            "uuid": self.artifacts.layer_uuid,
            "format": 999,
        }
        for name, value in mutations.items():
            with self.subTest(field=name):
                manifest.write_text(
                    json.dumps({**document, name: value}), encoding="utf-8"
                )
                with self.assertRaises(ScriptError):
                    managed.load_workload_artifacts(architecture="aarch64")
        manifest.write_text(json.dumps(document), encoding="utf-8")
        distro.write_bytes(b"changed actual artifact")
        with self.assertRaisesRegex(ScriptError, "artifact_sha256"):
            managed.load_workload_artifacts(architecture="aarch64")

    def test_wrappers_preserve_x86_calls_and_forward_only_explicit_selections(
        self,
    ) -> None:
        cases = (
            (lifecycle, "run_sandbox_lifecycle", "LifecycleAcceptance", 1),
            (ram, "run_sandbox_ram_upper", "RamUpperAcceptance", 2),
            (proxy, "run_sandbox_proxy_only", "ProxyOnlyAcceptance", 1),
        )
        for module, name, class_name, cycles in cases:
            for architecture in ("x86_64", "aarch64"):
                with self.subTest(helper=name, architecture=architecture):
                    fixture = self.root / f"{name}-{architecture}"
                    fixture.mkdir()
                    selection: dict[str, Any] = {}
                    if architecture == "aarch64":
                        selection["architecture"] = architecture
                        if module is not proxy:
                            selection["start_stop_cycles"] = 10
                    kind = getattr(module, class_name)
                    with (
                        patch.object(
                            module,
                            "load_workload_artifacts",
                            return_value=self.artifacts,
                        ) as load,
                        patch.object(module, class_name, wraps=kind) as construct,
                        patch.object(
                            module.tempfile, "mkdtemp", return_value=str(fixture)
                        ),
                        patch.object(module, "run_acceptance") as run,
                    ):
                        getattr(module, name)(
                            "whp",
                            timeout=2,
                            output_dir=self.root / "results",
                            **selection,
                        )
                    load.assert_called_once_with(
                        **({"architecture": architecture} if selection else {})
                    )
                    self.assertEqual(
                        {
                            key: value
                            for key, value in construct.call_args.kwargs.items()
                            if key in ("architecture", "start_stop_cycles")
                        },
                        selection,
                    )
                    acceptance = run.call_args.args[0]
                    self.assertEqual(acceptance.architecture, architecture)
                    self.assertEqual(
                        acceptance.start_stop_cycles,
                        10 if selection and module is not proxy else cycles,
                    )

    def test_managed_exec_selects_isa_on_provision_only(self) -> None:
        for architecture in ("x86_64", "aarch64"):
            with self.subTest(architecture=architecture):
                fixture = self.root / architecture
                fixture.mkdir()
                selection = (
                    {"architecture": architecture} if architecture != "x86_64" else {}
                )
                with (
                    patch.object(
                        managed, "load_workload_artifacts", return_value=self.artifacts
                    ) as load,
                    patch.object(
                        managed.tempfile, "mkdtemp", return_value=str(fixture)
                    ),
                    patch.object(
                        managed.nvx_cli, "command_line", return_value=["nvx", "sandbox"]
                    ),
                    patch.object(
                        managed.subprocess,
                        "run",
                        side_effect=RuntimeError("fixture boundary"),
                    ) as run,
                    self.assertRaisesRegex(RuntimeError, "fixture boundary"),
                ):
                    managed.run_managed_exec_configuration(
                        "whp", timeout=2, output_dir=self.root / "results", **selection
                    )
                load.assert_called_once_with(**selection)
                command = run.call_args.args[0]
                self.assertEqual(command[:3], ["nvx", "sandbox", "provision"])
                self.assertEqual("--architecture" in command, architecture == "aarch64")
                if architecture == "aarch64":
                    self.assertEqual(
                        command[command.index("--architecture") + 1], architecture
                    )
                self.assertNotIn("--cpu-profile", command)

    def test_launch_argv_and_record_driven_transition_argv(self) -> None:
        for architecture in ("x86_64", "aarch64"):
            for kind in (
                lifecycle.LifecycleAcceptance,
                ram.RamUpperAcceptance,
                proxy.ProxyOnlyAcceptance,
            ):
                with self.subTest(architecture=architecture, kind=kind.__name__):
                    acceptance = self.acceptance(kind, architecture=architecture)
                    options = (
                        acceptance.ram_options()
                        if isinstance(acceptance, ram.RamUpperAcceptance)
                        else acceptance.launch_options(acceptance.scratch)
                    )
                    expected = [
                        "--hypervisor",
                        "whp",
                        "--layer",
                        f"distro,{self.artifacts.distro},{self.artifacts.layer_uuid}",
                        *(
                            ["--upper", "ram"]
                            if kind is ram.RamUpperAcceptance
                            else ["--scratch", str(acceptance.scratch)]
                        ),
                        "--memory-mib",
                        "256",
                    ]
                    self.assertEqual(
                        options,
                        (
                            ["--architecture", "aarch64"]
                            if architecture == "aarch64"
                            else []
                        )
                        + expected,
                    )
                    with (
                        patch.object(
                            lifecycle.nvx_cli,
                            "command_line",
                            return_value=["nvx", "sandbox"],
                        ),
                        patch.object(lifecycle.subprocess, "run") as run,
                    ):
                        for operation in ("start", "exec", "stop", "deprovision"):
                            acceptance.run_cli(operation, state=acceptance.state)
                    for call in run.call_args_list:
                        self.assertNotIn("--architecture", call.args[0])
                        self.assertNotIn("--cpu-profile", call.args[0])

    def test_versioned_runtime_requires_matching_isa_and_owned_capability(self) -> None:
        state = self.root / "state"
        state.mkdir()
        capability = bytes(range(32))
        (state / lifecycle.CAPABILITY).write_bytes(capability)
        record: dict[str, object] = {
            "vmDir": str(state.resolve()),
            "capability": list(capability),
            "endpoint": "fixture-endpoint",
            "runtime": {},
            "process": {"pid": 42},
        }
        path = state / lifecycle.RUNTIME
        path.write_text(json.dumps(record), encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "unexpected fields"):
            lifecycle.read_runtime(state)
        for architecture in ("x86_64", "aarch64"):
            current = {**record, "version": 2, "architecture": architecture}
            path.write_text(json.dumps(current), encoding="utf-8")
            self.assertEqual(lifecycle.read_runtime(state), current)
            self.assertEqual(
                lifecycle.read_runtime(state, architecture=architecture), current
            )
        current = {**record, "version": 2, "architecture": "aarch64"}
        invalid = (
            record,
            {**current, "version": 1},
            {**current, "version": True},
            {**current, "version": None},
            {**current, "architecture": "x86_64"},
            {**current, "extra": 1},
            {name: value for name, value in current.items() if name != "architecture"},
            {name: value for name, value in current.items() if name != "version"},
            {**current, "capability": [0] * 32},
            {**current, "vmDir": str(self.root)},
        )
        for value in invalid:
            with self.subTest(record=value):
                path.write_text(json.dumps(value), encoding="utf-8")
                with self.assertRaises(RuntimeError):
                    lifecycle.read_runtime(state, architecture="aarch64")
        path.write_text(json.dumps(current), encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "ISA/version"):
            lifecycle.read_runtime(state, architecture="x86_64")

    def test_arm_other_runtime_uses_namespaced_image_provenance_without_fallback(
        self,
    ) -> None:
        self.check_other_runtime_provenance("aarch64")

    def test_x86_other_runtime_preserves_shared_provenance_paths(self) -> None:
        self.check_other_runtime_provenance("x86_64")

    def check_other_runtime_provenance(self, architecture: str) -> None:
        names = (
            OpenVMMBuildConstants.PROVENANCE_NAME,
            "Image.provenance.json"
            if architecture == "aarch64"
            else lifecycle.PROVENANCE_NAMES[1],
            InitramfsBuildConstants.PROVENANCE_NAME,
        )
        guest_directory = (
            self.build / architecture if architecture == "aarch64" else self.build
        )
        expected: dict[str, dict[str, object]] = {}
        for name, field in zip(
            names,
            ("executable_sha256", "kernel_sha256", "initramfs_sha256"),
            strict=True,
        ):
            directory = self.build if name == names[0] else guest_directory
            path = directory / name
            path.parent.mkdir(parents=True, exist_ok=True)
            expected[name] = {
                "format": 1,
                field: hashlib.sha256(f"owned fixture: {name}".encode()).hexdigest(),
            }
            path.write_text(json.dumps(expected[name]), encoding="utf-8")
        records = lifecycle.read_build_provenance(architecture=architecture)
        self.assertEqual(records, expected)
        if architecture == "aarch64":
            for path, field in (
                (guest_directory / names[0], "executable_sha256"),
                (self.build / names[1], "kernel_sha256"),
                (self.build / names[2], "initramfs_sha256"),
            ):
                path.write_text(
                    json.dumps(
                        {
                            "format": 1,
                            field: hashlib.sha256(
                                f"unselected fixture: {path.name}".encode()
                            ).hexdigest(),
                        }
                    ),
                    encoding="utf-8",
                )
            self.assertEqual(
                lifecycle.read_build_provenance(architecture=architecture), records
            )
        acceptance = self.acceptance(architecture=architecture)
        acceptance.write_other_runtime()
        other = acceptance.other_root / BuildConstants.BUILD_DIRECTORY_NAME
        expected_paths = {
            Path(name)
            if architecture == "x86_64" or name == names[0]
            else Path(architecture) / name
            for name in names
        }
        self.assertEqual(
            {path.relative_to(other) for path in other.rglob("*") if path.is_file()},
            expected_paths,
        )
        with patch.object(BuildConstants, "BUILD_DIR", other):
            alternate = lifecycle.read_build_provenance(architecture=architecture)
        self.assertEqual(
            [name for name in names if records[name] != alternate[name]],
            [InitramfsBuildConstants.PROVENANCE_NAME],
        )
        for name, record in records.items():
            copied = alternate[name]
            if name == InitramfsBuildConstants.PROVENANCE_NAME:
                self.assertEqual(
                    copied,
                    {
                        **record,
                        "initramfs_sha256": hashlib.sha256(
                            str(record["initramfs_sha256"]).encode()
                        ).hexdigest(),
                    },
                )
            else:
                self.assertEqual(copied, record)
        self.assertEqual(
            lifecycle.read_build_provenance(architecture=architecture), records
        )

    def test_scratch_cycles_reuse_the_provision_and_persistence_stop_oracles(
        self,
    ) -> None:
        for cycles in (1, 10):
            with self.subTest(cycles=cycles):
                acceptance = self.acceptance(
                    architecture="aarch64", start_stop_cycles=cycles
                )
                runtime = {"fixture": True}
                result = subprocess.CompletedProcess(
                    [], 0, lifecycle.STATE_CONTENT, b""
                )
                with (
                    patch.object(acceptance, "start", return_value=runtime) as start,
                    patch.object(acceptance, "stop") as stop,
                    patch.object(acceptance, "exec", return_value=result) as execute,
                    patch.object(acceptance, "copy_evidence"),
                    patch.object(acceptance, "invoke") as invoke,
                ):
                    acceptance.check_start_stop_cycles(runtime, "layer-digest")
                self.assertEqual(stop.call_count, cycles)
                self.assertEqual(start.call_count, cycles - 1)
                self.assertEqual(execute.call_count, cycles - 1)
                invoke.assert_not_called()
                for call in stop.call_args_list:
                    self.assertEqual(call.args, (runtime, "layer-digest"))

    def test_ram_cycles_reuse_one_provision_and_reset_each_upper(self) -> None:
        for architecture in ("x86_64", "aarch64"):
            for cycles in (2, 10):
                with self.subTest(architecture=architecture, cycles=cycles):
                    self.check_ram_cycles(architecture, cycles)

    def test_scratch_cycles_reject_lost_persistent_state(self) -> None:
        acceptance = self.acceptance(architecture="aarch64", start_stop_cycles=10)
        with (
            patch.object(acceptance, "stop"),
            patch.object(acceptance, "start", return_value={}),
            patch.object(
                acceptance,
                "exec",
                return_value=subprocess.CompletedProcess([], 0, b"", b""),
            ),
            self.assertRaisesRegex(RuntimeError, "restarted sandbox's state"),
        ):
            acceptance.check_start_stop_cycles({}, "layer-digest")

    def test_arm_ram_configuration_rejects_lost_isa_or_legacy_format(self) -> None:
        acceptance = self.acceptance(ram.RamUpperAcceptance, architecture="aarch64")
        assert isinstance(acceptance, ram.RamUpperAcceptance)
        acceptance.state.mkdir(parents=True)

        def provision(*arguments: Any, **options: Any) -> None:
            (acceptance.state / lifecycle.CONFIG).write_text(
                json.dumps(config), encoding="utf-8"
            )

        for selection, message in (
            ({"format": 11}, "another ISA"),
            ({"format": 11, "architecture": "x86_64"}, "another ISA"),
            ({"format": 10, "architecture": "aarch64"}, "RAM upper as"),
        ):
            config = {
                **selection,
                "upper": {"kind": "ram", "max_memory_percent": 50},
                "scratch_binding": None,
            }
            with (
                self.subTest(selection=selection),
                patch.object(acceptance, "invoke", side_effect=provision),
                patch.object(acceptance, "require_entries"),
                patch.object(
                    acceptance, "start", side_effect=AssertionError("must not start")
                ),
                self.assertRaisesRegex(RuntimeError, message),
            ):
                acceptance.check_managed()

    def check_ram_cycles(self, architecture: str, cycles: int) -> None:
        acceptance = self.acceptance(
            ram.RamUpperAcceptance, architecture=architecture, start_stop_cycles=cycles
        )
        self.assertIsInstance(acceptance, ram.RamUpperAcceptance)
        assert isinstance(acceptance, ram.RamUpperAcceptance)
        memory: bytes | None = None

        def invoke(
            operation: str, *arguments: str, **options: Any
        ) -> subprocess.CompletedProcess[bytes]:
            if operation == "provision":
                acceptance.state.mkdir(parents=True)
                config = {
                    "format": 10 if architecture == "x86_64" else 11,
                    "upper": {"kind": "ram", "max_memory_percent": 50},
                    "scratch_binding": None,
                    **(
                        {"architecture": architecture}
                        if architecture == "aarch64"
                        else {}
                    ),
                }
                (acceptance.state / lifecycle.CONFIG).write_text(
                    json.dumps(config), encoding="utf-8"
                )
            elif operation == "deprovision":
                shutil.rmtree(acceptance.state)
            else:
                self.fail(f"unexpected operation: {operation}")
            return subprocess.CompletedProcess([], 0, b"", b"")

        def execute(
            entrypoint: str, *arguments: str, **options: Any
        ) -> subprocess.CompletedProcess[bytes]:
            nonlocal memory
            stdout = b""
            if entrypoint == ram.PROBE:
                stdout = b"\n".join(ram.probe_markers(entry=True)) + b"\n"
                if "--arg=ram-fill" in arguments:
                    stdout += RamFillOracleTests.observation()
            elif entrypoint == "/bin/cat":
                self.assertIsNotNone(memory)
                stdout = memory or b""
            elif any(argument.startswith("--arg=printf ") for argument in arguments):
                memory = ram.STATE_CONTENT
            elif any(argument.startswith("--arg=test ! -e ") for argument in arguments):
                self.assertIsNone(memory)
            else:
                self.fail(f"unexpected exec: {entrypoint} {arguments}")
            return subprocess.CompletedProcess([], 0, stdout, b"")

        def stop(description: str) -> None:
            nonlocal memory
            memory = None

        with (
            patch.object(acceptance, "invoke", side_effect=invoke) as calls,
            patch.object(acceptance, "start", return_value={}) as start,
            patch.object(acceptance, "stop_sandbox", side_effect=stop) as stopped,
            patch.object(acceptance, "exec", side_effect=execute) as executed,
            patch.object(acceptance, "require_entries"),
            patch.object(acceptance, "check_guest_view"),
            patch.object(acceptance, "save_evidence"),
        ):
            acceptance.check_managed()
        self.assertEqual(
            [call.args[0] for call in calls.call_args_list],
            ["provision", "deprovision"],
        )
        self.assertEqual(start.call_count, cycles)
        self.assertEqual(stopped.call_count, cycles)
        empty_checks = [
            call
            for call in executed.call_args_list
            if any(str(arg).startswith("--arg=test ! -e ") for arg in call.args)
        ]
        self.assertEqual(len(empty_checks), cycles - 1)
        self.assertFalse(acceptance.scratch.exists())

    def test_invalid_cycles_fail_before_allocating_a_fixture(self) -> None:
        for module, name, values in (
            (lifecycle, "run_sandbox_lifecycle", (0, -1, True)),
            (ram, "run_sandbox_ram_upper", (0, 1, True)),
        ):
            for cycles in values:
                with (
                    self.subTest(helper=name, cycles=cycles),
                    patch.object(module, "load_workload_artifacts") as load,
                    patch.object(module.tempfile, "mkdtemp") as allocate,
                    self.assertRaisesRegex(ScriptError, "start_stop_cycles"),
                ):
                    getattr(module, name)(
                        "whp",
                        timeout=2,
                        output_dir=self.root / "results",
                        start_stop_cycles=cycles,
                    )
                load.assert_not_called()
                allocate.assert_not_called()


class RamFillOracleTests(unittest.TestCase):
    @staticmethod
    def observation(**changes: str) -> bytes:
        fields = {
            "page": "4096",
            "memory": "268435456",
            "total": "134217728",
            "available": "134213632",
            "written": "134213632",
            "dd_status": "1",
            "error": "ENOSPC",
            "after": "nvx-ram-upper",
            "cleanup": "complete",
            **changes,
        }
        return (
            ram.RAM_FILL_MARKER
            + b" "
            + " ".join(f"{name}={value}" for name, value in fields.items()).encode()
            + b"\n"
        )

    def test_fill_preserves_strict_original_slack_and_page_rounding(self) -> None:
        result = subprocess.CompletedProcess([], 0, self.observation(), b"")
        self.assertEqual(ram.check_ram_fill(result)["written"], 134213632)
        result.stdout = self.observation(written=str(134213632 - (1 << 20) + 1))
        ram.check_ram_fill(result)
        result.stdout = self.observation(written=str(134213632 - (1 << 20)))
        with self.assertRaisesRegex(RuntimeError, "original slack"):
            ram.check_ram_fill(result)

    def test_explicit_half_percent_rounds_odd_full_pages_up(self) -> None:
        result = subprocess.CompletedProcess(
            [],
            0,
            self.observation(memory="268439552", total="134221824"),
            b"",
        )
        self.assertEqual(ram.check_ram_fill(result)["total"], 134221824)
        for total in ("134217728", "134225920"):
            with self.subTest(total=total):
                result.stdout = self.observation(memory="268439552", total=total)
                with self.assertRaisesRegex(RuntimeError, "page-rounded half-memory"):
                    ram.check_ram_fill(result)

    def test_fill_refuses_false_enospc_capacity_accounting_and_recovery(self) -> None:
        for change in (
            {"dd_status": "0"},
            {"dd_status": "2"},
            {"error": "EIO"},
            {"after": "wrong"},
            {"cleanup": "failed"},
            {"page": "65536"},
            {"total": "134217729"},
            {"written": "134213633"},
            {"available": "134217729"},
            {"written": "0"},
            {"memory": "-1"},
            {"memory": str(1 << 64)},
        ):
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                ram.check_ram_fill(
                    subprocess.CompletedProcess([], 0, self.observation(**change), b"")
                )

    def test_fill_refuses_failed_commands_and_missing_or_ambiguous_facts(self) -> None:
        observation = self.observation()
        for result in (
            subprocess.CompletedProcess([], 1, observation, b""),
            subprocess.CompletedProcess([], 0, observation, b"failed cleanup"),
            subprocess.CompletedProcess([], 0, b"", b""),
            subprocess.CompletedProcess([], 0, observation * 2, b""),
            subprocess.CompletedProcess(
                [], 0, observation.replace(b" page=", b" page=oops"), b""
            ),
            subprocess.CompletedProcess(
                [], 0, observation.rstrip() + b" written=1\n", b""
            ),
        ):
            with self.subTest(result=result), self.assertRaises(RuntimeError):
                ram.check_ram_fill(result)


class RestageOracleTests(unittest.TestCase):
    @staticmethod
    def configuration(stamp: int, image: str = "/private/image.erofs") -> bytes:
        return json.dumps(
            {
                "image": {"path": image},
                "distro_binding": {
                    "image": {"mtime_ns": stamp},
                    "policy": {"revision": 1, "result": "accepted"},
                },
            }
        ).encode()

    def test_restage_requires_a_new_binding_for_the_same_owned_image(self) -> None:
        before = self.configuration(1)
        after = self.configuration(2)
        lifecycle.check_restage_bindings(before, after, "/private/image.erofs")
        with self.assertRaisesRegex(RuntimeError, "kept the stale"):
            lifecycle.check_restage_bindings(before, before, "/private/image.erofs")
        with self.assertRaisesRegex(RuntimeError, "same owned image"):
            lifecycle.check_restage_bindings(
                before,
                self.configuration(2, "/different/image.erofs"),
                "/private/image.erofs",
            )

    def test_restage_refuses_missing_rejected_and_malformed_binding(self) -> None:
        before = self.configuration(1)
        after = json.loads(self.configuration(2))
        malformed = [
            b"not JSON",
            b"[]",
            json.dumps({"image": {"path": "/private/image.erofs"}}).encode(),
        ]
        for policy in (
            {"revision": 1, "result": "rejected"},
            {"revision": True, "result": "accepted"},
        ):
            changed = json.loads(json.dumps(after))
            changed["distro_binding"]["policy"] = policy
            malformed.append(json.dumps(changed).encode())
        for document in malformed:
            with self.subTest(document=document), self.assertRaises(RuntimeError):
                lifecycle.check_restage_bindings(
                    before, document, "/private/image.erofs"
                )

    def test_changed_layer_requires_actual_refusal_without_workload_output(
        self,
    ) -> None:
        image = "/private/image.erofs"
        error = (
            f"error[unavailable]: {image} changed after its distro binding was made: "
            "its modification time differs; deprovision the sandbox and provision "
            "it again\n"
        ).encode()
        lifecycle.check_changed_layer_start(
            subprocess.CompletedProcess([], 1, b"", error), image
        )
        for result in (
            subprocess.CompletedProcess([], 0, b"", error),
            subprocess.CompletedProcess([], 2, b"", error),
            subprocess.CompletedProcess([], 1, b"workload ran", error),
            subprocess.CompletedProcess([], 1, b"", b"error[io]: failed\n"),
            subprocess.CompletedProcess(
                [], 1, b"", error.replace(b"modification time", b"length")
            ),
        ):
            with self.subTest(result=result), self.assertRaises(RuntimeError):
                lifecycle.check_changed_layer_start(result, image)


if __name__ == "__main__":
    unittest.main()

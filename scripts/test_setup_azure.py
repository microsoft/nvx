import base64
import gzip
import hashlib
import io
import json
import lzma
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from typing import Literal, TypedDict, cast, overload
from unittest.mock import patch

import yaml

from scripts.setup.azure import build_package as builder
from scripts.setup.azure.nvx_azure import windows_stage


class CloudFiles(TypedDict):
    write_files: list[dict[str, str]]
    packages: list[str]


class PackageTests(unittest.TestCase):
    @overload
    def configuration(self, platform: Literal["linux"]) -> builder.LinuxConfig: ...

    @overload
    def configuration(self, platform: Literal["windows"]) -> builder.WindowsConfig: ...

    @overload
    def configuration(
        self, platform: Literal["linux", "windows"]
    ) -> builder.RunnerConfig: ...

    def configuration(
        self, platform: Literal["linux", "windows"]
    ) -> builder.RunnerConfig:
        return builder.load_config(builder.ROOT / platform / "config.example.json")

    def linux_guest_files(self, payload: bytes) -> tuple[CloudFiles, dict[str, bytes]]:
        cloud = cast(CloudFiles, yaml.safe_load(payload))
        files: dict[str, bytes] = {}
        for entry in cloud["write_files"]:
            if entry.get("encoding") == "gz+b64":
                self.assertTrue(entry["path"].startswith("/opt/nvx-bootstrap/"))
                files[entry["path"].removeprefix("/opt/nvx-bootstrap/")] = (
                    gzip.decompress(base64.b64decode(entry["content"], validate=True))
                )
        return cloud, files

    def test_linux_files_round_trip(self):
        config = self.configuration("linux")
        with tempfile.TemporaryDirectory() as temporary:
            manifest = builder.build_package(config, temporary)
            cloud, names = self.linux_guest_files(
                (Path(temporary) / "cloud-init.yaml").read_bytes()
            )
            # setup-linux.sh runner mode requires diff, and the controller python3.
            self.assertIn("diffutils", cloud["packages"])
            self.assertIn("python3", cloud["packages"])
            modules = {f"nvx_azure/{name}" for name in builder.LINUX_MODULES}
            self.assertEqual(
                set(names),
                {
                    "config.json",
                    "setup-linux.sh",
                    "tool-versions.conf",
                    "__main__.py",
                    *modules,
                },
            )
            self.assertEqual(set(manifest["guestFiles"]), set(names))
            for entry in cloud["write_files"]:
                if entry.get("encoding") != "gz+b64":
                    continue
                name = entry["path"].removeprefix("/opt/nvx-bootstrap/")
                self.assertEqual(entry["owner"], "root:root")
                self.assertEqual(
                    hashlib.sha256(names[name]).hexdigest(),
                    manifest["guestFiles"][name],
                )
                expected_mode = (
                    "0600"
                    if name == "config.json"
                    else ("0755" if name.endswith(".sh") else "0644")
                )
                self.assertEqual(entry["permissions"], expected_mode)
            # Only the shared installer remains a shell program.
            self.assertEqual(
                [name for name in names if name.endswith(".sh")],
                ["setup-linux.sh"],
            )
            for module in modules:
                self.assertEqual(names[module], builder.source(module))
            self.assertEqual(names["__main__.py"], builder.GUEST_ENTRY_POINT)
            units = {
                entry["path"].removeprefix("/etc/systemd/system/"): entry
                for entry in cloud["write_files"]
                if entry["path"].startswith("/etc/systemd/system/")
            }
            self.assertEqual(
                set(units),
                {
                    "setup-nvx-github-runner.service",
                    "nvx-runner-metadata-block.service",
                },
            )
            for unit, command in (
                ("setup-nvx-github-runner.service", "linux-bootstrap"),
                ("nvx-runner-metadata-block.service", "linux-block-metadata"),
            ):
                self.assertEqual(units[unit]["permissions"], "0644")
                self.assertIn(
                    "\nExecStart=/usr/bin/python3 -I -B /opt/nvx-bootstrap "
                    + command
                    + "\n",
                    units[unit]["content"],
                )
            self.assertEqual(json.loads(names["config.json"]), config)
            self.assertEqual(names["setup-linux.sh"], builder.source("setup-linux.sh"))
            self.assertEqual(
                names["tool-versions.conf"], builder.source("tool-versions.conf")
            )
            self.assertLessEqual(manifest["customDataBytes"], 65535)

    def test_packaged_linux_guest_runs_in_isolated_mode(self):
        _, files, _ = builder.linux_payload(self.configuration("linux"))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "nvx-bootstrap"
            for name, data in files.items():
                (root / name).parent.mkdir(parents=True, exist_ok=True)
                (root / name).write_bytes(data)
            result = subprocess.run(
                [sys.executable, "-I", "-B", str(root), "--help"],
                capture_output=True,
                text=True,
                timeout=60,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            for command in (
                "linux-bootstrap",
                "linux-refresh-metadata",
                "linux-block-metadata",
                "linux-verify-mshv",
            ):
                self.assertIn(command, result.stdout)
            # Every packaged module imports from the guest directory alone.
            modules = ", ".join(
                "nvx_azure." + name.removesuffix(".py")
                for name in builder.LINUX_MODULES
                if name != "__init__.py"
            )
            result = subprocess.run(
                [
                    sys.executable,
                    "-I",
                    "-B",
                    "-c",
                    f"import sys; sys.path.insert(0, sys.argv[1]); import {modules}",
                    str(root),
                ],
                capture_output=True,
                text=True,
                timeout=60,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(list(root.rglob("__pycache__")), [])

    def test_linux_gzip_headers_are_independent_of_builder_platform(self):
        original_compress = gzip.compress
        outputs: list[bytes] = []
        for host_os in (0, 3):

            def host_compress(
                data: bytes,
                compresslevel: int = 9,
                *,
                mtime: int | None = None,
                os_byte: int = host_os,
            ) -> bytes:
                compressed = bytearray(
                    original_compress(data, compresslevel=compresslevel, mtime=mtime)
                )
                compressed[9] = os_byte
                return bytes(compressed)

            with patch.object(builder.gzip, "compress", side_effect=host_compress):
                payload, files, _ = builder.linux_payload(self.configuration("linux"))
                outputs.append(payload)
                cloud = cast(CloudFiles, yaml.safe_load(payload))
                for entry in cloud["write_files"]:
                    if entry.get("encoding") != "gz+b64":
                        continue
                    compressed = base64.b64decode(entry["content"], validate=True)
                    self.assertEqual(compressed[:4], b"\x1f\x8b\x08\x00")
                    self.assertEqual(compressed[4:8], b"\0" * 4)
                    self.assertEqual(compressed[9], 255)
                    self.assertEqual(
                        gzip.decompress(compressed),
                        files[entry["path"].removeprefix("/opt/nvx-bootstrap/")],
                    )
        self.assertEqual(outputs[0], outputs[1])

    def test_mshv_modes_share_the_guest_files(self):
        config = self.configuration("linux")
        config["backend"] = "mshv"
        config["runnerLabels"] = ["linux", "mshv", "virtual-machine"]
        guest_files: list[dict[str, str]] = []
        for mode in ("preinstalled", "azurelinux-mshv"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as temporary:
                config["kernelMode"] = mode
                manifest = builder.build_package(config, temporary)
                self.assertLessEqual(manifest["customDataBytes"], 65535)
                guest_files.append(manifest["guestFiles"])
        # Only the configuration differs: every mode ships the MSHV module.
        self.assertIn("nvx_azure/linux_mshv.py", guest_files[0])
        self.assertEqual(
            {
                name
                for name in guest_files[0]
                if guest_files[0][name] != guest_files[1][name]
            },
            {"config.json"},
        )

    def windows_files(self, package: bytes) -> dict[str, bytes]:
        files: dict[str, bytes] = {}
        with tarfile.open(fileobj=io.BytesIO(package), mode="r:xz") as archive:
            for member in archive.getmembers():
                self.assertTrue(member.isreg(), member.name)
                self.assertNotIn(member.name, files)
                stream = archive.extractfile(member)
                assert stream is not None
                files[member.name] = stream.read()
        return files

    def test_windows_package_round_trip(self):
        config = self.configuration("windows")
        with tempfile.TemporaryDirectory() as temporary:
            manifest = builder.build_package(config, temporary)
            payload = json.loads((Path(temporary) / "custom-data.json").read_text())
            self.assertEqual(
                set(payload),
                {
                    "script",
                    "adminUsername",
                    "publicKeys",
                    "defaultShell",
                    "python",
                    "package",
                },
            )
            package = base64.b64decode(payload["package"]["data"], validate=True)
            self.assertEqual(
                hashlib.sha256(package).hexdigest(), payload["package"]["sha256"]
            )
            self.assertEqual((Path(temporary) / "sources.tar.xz").read_bytes(), package)
            files = self.windows_files(package)
            self.assertEqual(set(files), set(manifest["guestFiles"]))
            self.assertEqual(frozenset(files), windows_stage.PACKAGE_FILES)
            for name, digest in manifest["guestFiles"].items():
                self.assertEqual(hashlib.sha256(files[name]).hexdigest(), digest)
            packaged_config = json.loads(files["config.json"])
            self.assertEqual(
                packaged_config,
                {
                    key: value
                    for key, value in config.items()
                    if key not in builder.WINDOWS_OUTER_ONLY_FIELDS
                },
            )
            for field in builder.WINDOWS_OUTER_ONLY_FIELDS:
                self.assertNotIn(field, packaged_config)
            self.assertEqual(files["__main__.py"], builder.GUEST_ENTRY_POINT)
            for name in ("setup-windows-whp.ps1", "tool-versions.conf"):
                self.assertEqual(files[name], builder.source(name))
            for name in builder.WINDOWS_ADAPTERS:
                self.assertEqual(files[name], builder.source(f"windows/{name}"))
            for name in builder.WINDOWS_MODULES:
                self.assertEqual(
                    files[f"nvx_azure/{name}"], builder.source(f"nvx_azure/{name}")
                )
            self.assertEqual(
                gzip.decompress(base64.b64decode(payload["script"])),
                builder.source("windows/vmss-bootstrap.ps1"),
            )
            self.assertEqual(payload["python"], builder.WINDOWS_PYTHON)
            self.assertEqual(
                (
                    payload["adminUsername"],
                    payload["publicKeys"],
                    payload["defaultShell"],
                ),
                (
                    config["adminUsername"],
                    config["publicKeys"],
                    config["sshDefaultShell"],
                ),
            )
            self.assertLessEqual(manifest["customDataBytes"], 65535)
            self.assertIn(
                "CustomData.bin",
                (Path(temporary) / "extension-command.txt").read_text(),
            )

    def test_large_windows_public_keys_are_packaged_only_once(self):
        config = self.configuration("windows")
        config["publicKeys"] = [
            "ssh-rsa "
            + base64.b64encode(
                b"".join(
                    hashlib.sha512(f"{key}:{block}".encode()).digest()
                    for block in range(9)
                )[:535]
            ).decode()
            for key in range(3)
        ]
        with tempfile.TemporaryDirectory() as temporary:
            manifest = builder.build_package(config, temporary)
            payload = json.loads((Path(temporary) / "custom-data.json").read_text())
            files = self.windows_files(
                (Path(temporary) / "sources.tar.xz").read_bytes()
            )
        self.assertEqual(payload["publicKeys"], config["publicKeys"])
        packaged_config = json.loads(files["config.json"])
        for field in builder.WINDOWS_OUTER_ONLY_FIELDS:
            self.assertNotIn(field, packaged_config)
        self.assertLessEqual(manifest["customDataBytes"], 65535)

    def test_packaged_windows_guest_runs_in_isolated_mode(self):
        _, _, outputs = builder.windows_payload(self.configuration("windows"))
        with tempfile.TemporaryDirectory() as temporary:
            package = Path(temporary) / "package.tar.xz"
            package.write_bytes(outputs["sources.tar.xz"])
            staging = Path(temporary) / "staging"
            # The entry shim unpacks the package with Python's own extractor.
            commands = (
                [
                    "-m",
                    "tarfile",
                    "--filter",
                    "data",
                    "-e",
                    str(package),
                    str(staging),
                ],
                [str(staging), "--help"],
                [
                    "-c",
                    "import sys; sys.path.insert(0, sys.argv[1]); import "
                    + ", ".join(
                        "nvx_azure." + name.removesuffix(".py")
                        for name in builder.WINDOWS_MODULES
                        if name != "__init__.py"
                    ),
                    str(staging),
                ],
            )
            outputs_seen: list[str] = []
            for arguments in commands:
                result = subprocess.run(
                    [sys.executable, "-I", "-B", *arguments],
                    capture_output=True,
                    text=True,
                    timeout=60,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                outputs_seen.append(result.stdout)
            for command in ("windows-stage", "windows-bootstrap"):
                self.assertIn(command, outputs_seen[1])
            self.assertEqual(list(staging.rglob("__pycache__")), [])

    def test_guest_packages_ship_their_platform_modules(self):
        sources = {
            path.name for path in (builder.ROOT / builder.GUEST_PACKAGE).glob("*.py")
        }
        for platform, modules in (
            ("linux_", builder.LINUX_MODULES),
            ("windows_", builder.WINDOWS_MODULES),
        ):
            with self.subTest(platform=platform):
                self.assertEqual(
                    set(modules),
                    {name for name in sources if name.startswith(platform)}
                    | set(builder.COMMON_MODULES),
                )
        self.assertEqual(set(windows_stage.MODULES), set(builder.WINDOWS_MODULES))
        self.assertEqual(
            {path.name for path in (builder.ROOT / "windows").glob("*.ps1")},
            {*builder.WINDOWS_ADAPTERS, "vmss-bootstrap.ps1"},
        )

    def test_model_patch_is_exact_payload(self):
        cases: tuple[tuple[Literal["linux", "windows"], str], ...] = (
            ("linux", "cloud-init.yaml"),
            ("windows", "custom-data.json"),
        )
        for platform, filename in cases:
            with (
                self.subTest(platform=platform),
                tempfile.TemporaryDirectory() as temporary,
            ):
                manifest = builder.build_package(
                    self.configuration(platform), temporary
                )
                payload = (Path(temporary) / filename).read_bytes()
                model = json.loads((Path(temporary) / "model-patch.json").read_text())
                encoded = model["properties"]["virtualMachineProfile"]["osProfile"][
                    "customData"
                ]
                self.assertEqual(base64.b64decode(encoded, validate=True), payload)
                self.assertEqual(
                    hashlib.sha256(payload).hexdigest(), manifest["customDataSha256"]
                )
                self.assertEqual(manifest["rustVersion"], builder.rust_version())

    def test_packages_are_deterministic(self):
        for platform in ("linux", "windows"):
            with (
                self.subTest(platform=platform),
                tempfile.TemporaryDirectory() as left,
                tempfile.TemporaryDirectory() as right,
            ):
                config = self.configuration(platform)
                builder.build_package(config, left)
                builder.build_package(config, right)
                first = {p.name: p.read_bytes() for p in Path(left).iterdir()}
                second = {p.name: p.read_bytes() for p in Path(right).iterdir()}
                self.assertEqual(first, second)

    def test_windows_package_is_a_canonical_archive(self):
        _, files, outputs = builder.windows_payload(self.configuration("windows"))
        package = outputs["sources.tar.xz"]
        # An xz stream checked with CRC64, holding a USTAR archive of regular
        # files in name order whose metadata does not depend on the builder.
        self.assertEqual(package[:8], b"\xfd7zXZ\x00\x00\x04")
        archive_bytes = lzma.decompress(package)
        with tarfile.open(fileobj=io.BytesIO(archive_bytes), mode="r:") as archive:
            members = archive.getmembers()
        self.assertEqual([member.name for member in members], sorted(files))
        for member in members:
            with self.subTest(member=member.name):
                header = archive_bytes[member.offset : member.offset + 512]
                self.assertEqual(header[257:265], b"ustar\x0000")
                self.assertEqual(
                    (
                        member.type,
                        member.mode,
                        member.mtime,
                        member.uid,
                        member.gid,
                        member.uname,
                        member.gname,
                        member.pax_headers,
                    ),
                    (tarfile.REGTYPE, 0o644, 0, 0, 0, "", "", {}),
                )

    def test_windows_packages_normalize_powershell_checkout_line_endings(self):
        outputs: list[dict[str, bytes]] = []
        original_read_bytes = Path.read_bytes
        for ending in (b"\n", b"\r\n"):

            def read_checkout(path: Path, newline: bytes = ending) -> bytes:
                data = original_read_bytes(path)
                if path.suffix == ".ps1" and path.is_relative_to(builder.SETUP):
                    return data.replace(b"\r\n", b"\n").replace(b"\n", newline)
                return data

            with (
                self.subTest(ending=ending),
                tempfile.TemporaryDirectory() as temporary,
                patch.object(Path, "read_bytes", read_checkout),
            ):
                builder.build_package(self.configuration("windows"), temporary)
                outputs.append(
                    {path.name: path.read_bytes() for path in Path(temporary).iterdir()}
                )
        self.assertEqual(outputs[0], outputs[1])
        for name, data in self.windows_files(outputs[0]["sources.tar.xz"]).items():
            if name.endswith(".ps1"):
                self.assertNotIn(b"\r", data, name)
        payload = json.loads(outputs[0]["custom-data.json"])
        self.assertNotIn(b"\r", gzip.decompress(base64.b64decode(payload["script"])))

    def test_generated_outputs_stay_outside_source_tree(self):
        with self.assertRaisesRegex(ValueError, "outside"):
            builder.build_package(
                self.configuration("linux"), builder.ROOT / "generated"
            )
        self.assertFalse((builder.ROOT / "generated").exists())
        with self.assertRaisesRegex(ValueError, "outside"):
            builder.build_package(
                self.configuration("windows"), builder.REPO / "generated"
            )
        self.assertFalse((builder.REPO / "generated").exists())

    def test_package_output_must_be_new_or_empty(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "package"
            builder.build_package(self.configuration("windows"), output)
            files = {path.name: path.read_bytes() for path in output.iterdir()}
            with self.assertRaisesRegex(ValueError, "must be empty"):
                builder.build_package(self.configuration("linux"), output)
            self.assertEqual(
                {path.name: path.read_bytes() for path in output.iterdir()}, files
            )
            empty = Path(temporary) / "empty"
            empty.mkdir()
            builder.build_package(self.configuration("linux"), empty)
            self.assertEqual(
                {path.name for path in empty.iterdir()},
                {"cloud-init.yaml", "manifest.json", "model-patch.json"},
            )

    def test_invalid_common_values(self):
        cases: list[tuple[str, object]] = [
            ("os", []),
            ("subscriptionId", "not-a-uuid"),
            ("resourceGroup", "bad/group"),
            ("vmssName", "bad name"),
            ("repository", "https://github.com/microsoft/nvx"),
            ("adminUsername", "admin;command"),
            ("keyVaultName", "bad.vault"),
            ("bootstrapSecretName", "secret?query"),
            ("runnerNamePrefix", "../runner"),
            ("runnerNumberOffset", True),
            ("runnerNumberOffset", 2),
            ("runnerLabels", []),
            ("runnerLabels", ["kvm", "KVM"]),
            ("runnerLabels", ["label with spaces"]),
        ]
        for name, value in cases:
            with self.subTest(name=name, value=value):
                config: dict[str, object] = dict(self.configuration("linux"))
                config[name] = value
                with self.assertRaises(ValueError):
                    builder.normalize_config(config)

    def test_unknown_configuration_and_credentials_rejected(self):
        config: dict[str, object] = dict(self.configuration("linux"))
        config["runnerToken"] = "do-not-store"
        with self.assertRaisesRegex(ValueError, "Unknown"):
            builder.normalize_config(config)
        config = dict(self.configuration("linux"))
        config["runnerNamePrefix"] = "ghp_" + "A" * 30
        with self.assertRaisesRegex(ValueError, "credential-like"):
            builder.normalize_config(config)

    def test_required_and_duplicate_fields(self):
        config: dict[str, object] = dict(self.configuration("linux"))
        del config["vmssName"]
        with self.assertRaisesRegex(ValueError, "Missing"):
            builder.normalize_config(config)
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "duplicate.json"
            path.write_text('{"os":"linux","os":"windows"}')
            with self.assertRaisesRegex(ValueError, "Duplicate"):
                builder.load_config(path)

    def test_linux_disk_paths_and_backend(self):
        for value in ("/", "/etc", "/mnt/../etc", "/mnt/data/./nested", "/mnt//data"):
            with self.subTest(value=value):
                config = self.configuration("linux")
                config["dataDisk"]["mountPoint"] = value
                with self.assertRaises(ValueError):
                    builder.normalize_config(config)
        for value in ("/etc", "/opt/nvx", "/opt/nvx-bootstrap", "/opt/nvx/runners"):
            with self.subTest(runner=value):
                config = self.configuration("linux")
                config["runnerDirectory"] = value
                with self.assertRaises(ValueError):
                    builder.normalize_config(config)
        config = self.configuration("linux")
        config["kernelMode"] = "azurelinux-mshv"
        with self.assertRaisesRegex(ValueError, "requires the mshv"):
            builder.normalize_config(config)
        config = self.configuration("linux")
        config["runnerLabels"] = ["kvm", "linux", "virtual-machine"]
        with self.assertRaisesRegex(ValueError, "runnerLabels must start"):
            builder.normalize_config(config)
        config = self.configuration("linux")
        config["adminUsername"] = "root"
        with self.assertRaisesRegex(ValueError, "must not be root"):
            builder.normalize_config(config)

    def test_required_runner_labels_ignore_case(self):
        cases: tuple[tuple[Literal["linux", "windows"], list[str]], ...] = (
            ("linux", ["LINUX", "KVM", "Virtual-Machine"]),
            ("windows", ["Windows", "WHP", "Virtual-Machine"]),
        )
        for platform, labels in cases:
            with self.subTest(platform=platform):
                config = self.configuration(platform)
                config["runnerLabels"] = labels
                self.assertEqual(
                    builder.normalize_config(config)["runnerLabels"], labels
                )

    def test_runner_roles_are_mutually_exclusive(self):
        for platform in ("linux", "windows"):
            for role in (None, "general", "benchmark"):
                with self.subTest(platform=platform, role=role):
                    config = self.configuration(platform)
                    if role is not None:
                        config["runnerLabels"].append(role)
                    self.assertEqual(
                        builder.normalize_config(config)["runnerLabels"],
                        config["runnerLabels"],
                    )
            config = self.configuration(platform)
            config["runnerLabels"].extend(("general", "benchmark"))
            with self.subTest(platform=platform, role="both"):
                with self.assertRaisesRegex(
                    ValueError, "must not contain both general and benchmark"
                ):
                    builder.normalize_config(config)

    def test_benchmark_name_selects_one_general_runner(self):
        for platform in ("linux", "windows"):
            config = self.configuration(platform)
            prefix = config["runnerNamePrefix"]
            config["runnerLabels"].extend(("general", "isolated"))
            config["benchmarkRunnerName"] = prefix + "-5"
            self.assertEqual(
                builder.normalize_config(config).get("benchmarkRunnerName"),
                prefix + "-5",
            )
            for name in (
                prefix + "-0",
                prefix + "-05",
                prefix + "-other",
                "other-5",
                None,
            ):
                with self.subTest(platform=platform, name=name):
                    invalid = dict(config)
                    invalid["benchmarkRunnerName"] = name
                    with self.assertRaisesRegex(ValueError, "benchmarkRunnerName"):
                        builder.normalize_config(invalid)
            for role in (None, "benchmark"):
                with self.subTest(platform=platform, role=role):
                    invalid = dict(config)
                    invalid["runnerLabels"] = config["runnerLabels"][:3] + (
                        [] if role is None else [role]
                    )
                    with self.assertRaisesRegex(ValueError, "requires general labels"):
                        builder.normalize_config(invalid)

    def test_windows_drive_and_path_validation(self):
        for update in (
            {"driveLetter": "C"},
            {"driveLetter": "F:"},
            {"temporaryDriveFallback": "F"},
            {"lun": True},
            {"lun": 64},
            {"unknown": 1},
        ):
            with self.subTest(update=update):
                config = self.configuration("windows")
                invalid = {**config, "dataDisk": {**config["dataDisk"], **update}}
                with self.assertRaises(ValueError):
                    builder.normalize_config(invalid)
        for value in (
            r"C:\Windows",
            r"C:\ProgramData\runner",
            r"C:\a\..\b",
            "relative",
        ):
            with self.subTest(path=value):
                config = self.configuration("windows")
                config["runnerDirectory"] = value
                with self.assertRaises(ValueError):
                    builder.normalize_config(config)
        config = self.configuration("windows")
        config["benchmarkScratchDirectory"] = r"C:\scratch"
        with self.assertRaisesRegex(ValueError, "configured data drive"):
            builder.normalize_config(config)
        for value in ("", False, 0):
            with self.subTest(scratch=value):
                invalid: dict[str, object] = dict(self.configuration("windows"))
                invalid["benchmarkScratchDirectory"] = value
                with self.assertRaisesRegex(ValueError, "benchmarkScratchDirectory"):
                    builder.normalize_config(invalid)
        config = self.configuration("windows")
        config["runnerLabels"] = ["windows", "virtual-machine", "whp"]
        with self.assertRaisesRegex(ValueError, "runnerLabels must start"):
            builder.normalize_config(config)

    def test_windows_default_scratch_uses_configured_data_drive(self):
        for drive in ("f", "G"):
            for include_null in (False, True):
                with self.subTest(drive=drive, include_null=include_null):
                    config = self.configuration("windows")
                    config["dataDisk"]["driveLetter"] = drive
                    config["runnerDirectory"] = (
                        rf"{drive.upper()}:\nvx-benchmark-scratch2\runner"
                    )
                    raw: dict[str, object] = dict(config)
                    raw.pop("benchmarkScratchDirectory")
                    if include_null:
                        raw["benchmarkScratchDirectory"] = None
                    expected = rf"{drive.upper()}:\nvx-benchmark-scratch"
                    normalized = builder.normalize_config(raw)
                    self.assertEqual(
                        normalized.get("benchmarkScratchDirectory"), expected
                    )
                    with tempfile.TemporaryDirectory() as temporary:
                        builder.build_package(raw, temporary)
                        files = self.windows_files(
                            (Path(temporary) / "sources.tar.xz").read_bytes()
                        )
                        packaged = json.loads(files["config.json"])
                        self.assertEqual(
                            packaged["benchmarkScratchDirectory"], expected
                        )

    def test_windows_default_scratch_rejects_runner_overlap(self):
        for include_null in (False, True):
            for directory in (
                r"F:\nvx-benchmark-scratch",
                r"f:\NVX-BENCHMARK-SCRATCH",
                r"F:\nvx-benchmark-scratch\runner",
            ):
                with self.subTest(directory=directory, include_null=include_null):
                    config: dict[str, object] = dict(self.configuration("windows"))
                    config["runnerDirectory"] = directory
                    config.pop("benchmarkScratchDirectory")
                    if include_null:
                        config["benchmarkScratchDirectory"] = None
                    with self.assertRaisesRegex(ValueError, "runner installation"):
                        builder.normalize_config(config)

    def test_windows_paths_reject_trailing_period_aliases(self):
        for field in ("runnerDirectory", "benchmarkScratchDirectory"):
            for path in (
                r"F:\runner.\bin",
                r"F:\pool...\scratch",
                r"F:\Windows.\runner",
                r"F:\runner\scratch.",
            ):
                with self.subTest(field=field, path=path):
                    config: dict[str, object] = dict(self.configuration("windows"))
                    config[field] = path
                    with self.assertRaisesRegex(ValueError, "trailing-period"):
                        builder.normalize_config(config)
        self.assertEqual(
            builder.windows_path(r"F:\pool.v1\runner", "path"),
            r"F:\pool.v1\runner",
        )

    def test_windows_paths_reject_reserved_dos_components(self):
        for field in ("runnerDirectory", "benchmarkScratchDirectory"):
            for component in ("NUL", "con", "PRN.txt", "aux.log", "COM1.txt", "lpt9"):
                for path in (rf"F:\{component}\runner", rf"F:\pool\{component}"):
                    with self.subTest(field=field, path=path):
                        config: dict[str, object] = dict(self.configuration("windows"))
                        config[field] = path
                        with self.assertRaisesRegex(ValueError, "reserved DOS"):
                            builder.normalize_config(config)
        for component in ("COM10", "LPT10", "AUXILIARY", "CONSOLE", "file.NUL"):
            path = rf"F:\pool\{component}"
            self.assertEqual(builder.windows_path(path, "path"), path)

    def test_windows_scratch_does_not_overlap_protected_runner_paths(self):
        for scratch, valid in (
            (r"F:\runner", False),
            (r"f:\RUNNER", False),
            (r"F:\runner\scratch", False),
            (r"f:\RUNNER\_worker\scratch", False),
            (r"F:\runner\_workingscratch", False),
            (r"F:\runner\_work", True),
            (r"f:\RUNNER\_WORK\scratch", True),
            (r"F:\runner2\scratch", True),
            (r"F:\scratch", True),
        ):
            with self.subTest(scratch=scratch):
                config = self.configuration("windows")
                config["runnerDirectory"] = r"F:\runner"
                config["benchmarkScratchDirectory"] = scratch
                if valid:
                    self.assertEqual(
                        builder.normalize_config(config).get(
                            "benchmarkScratchDirectory"
                        ),
                        scratch,
                    )
                else:
                    with self.assertRaisesRegex(ValueError, "runner installation"):
                        builder.normalize_config(config)

    def test_windows_scratch_must_not_contain_runner_directory(self):
        for scratch, valid in (
            (r"F:\pool", False),
            (r"f:\POOL", False),
            (r"F:\pool\runner", False),
            (r"F:\pool\runner2", True),
            (r"F:\pool\runner\_work", True),
        ):
            with self.subTest(scratch=scratch):
                config = self.configuration("windows")
                config["runnerDirectory"] = r"F:\pool\runner"
                config["benchmarkScratchDirectory"] = scratch
                if valid:
                    self.assertEqual(
                        builder.normalize_config(config).get(
                            "benchmarkScratchDirectory"
                        ),
                        scratch,
                    )
                else:
                    with self.assertRaisesRegex(ValueError, "runner installation"):
                        builder.normalize_config(config)

    def test_optional_defaults(self):
        for platform in ("linux", "windows"):
            with self.subTest(platform=platform):
                config = self.configuration(platform)
                for name in builder.COMMON_OPTIONAL:
                    config.pop(name, None)
                if platform == "linux":
                    config.pop("kernelMode")
                else:
                    for name in builder.PLATFORM_FIELDS[platform]:
                        config.pop(name, None)
                normalized = builder.normalize_config(config)
                self.assertEqual(normalized["runnerNumberOffset"], 1)
                self.assertEqual(normalized["repository"], "microsoft/nvx")
                self.assertTrue(normalized["runnerDirectory"])

    def test_oversized_custom_data_rejected_before_output(self):
        config = self.configuration("windows")
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "package"
            with patch.object(
                builder, "windows_payload", return_value=(b"x" * 65536, {}, {})
            ):
                with self.assertRaisesRegex(ValueError, "65,535"):
                    builder.build_package(config, output)
            self.assertFalse(output.exists())

    def test_rust_release_comes_from_upstream_manifest_and_toolchain(self):
        cases = [
            (b"rust.toolchain=stable\n", b'channel = "stable"\n'),
            (b"rust.toolchain=3.2.1\n", b'channel = "3.2.2"\n'),
            (b"rust.toolchain=3.2.1\nrust.toolchain=3.2.1\n", b'channel = "3.2.1"\n'),
        ]
        for manifest, toolchain in cases:
            with self.subTest(manifest=manifest):
                sources = {
                    "tool-versions.conf": manifest,
                    "rust-toolchain.toml": toolchain,
                }
                with patch.object(builder, "source", side_effect=sources.__getitem__):
                    with self.assertRaisesRegex(ValueError, "same explicit"):
                        builder.rust_version()


if __name__ == "__main__":
    unittest.main()

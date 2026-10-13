import base64
import contextlib
import gzip
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
import urllib.request
import zipfile
from pathlib import Path
from types import CodeType
from typing import Literal, Protocol, TypedDict, cast, overload
from unittest.mock import patch

import yaml

from scripts.setup.azure import build_package as builder


class HttpOpener(Protocol):
    def open(self, request: urllib.request.Request, timeout: int) -> io.StringIO: ...


class CloudFiles(TypedDict):
    write_files: list[dict[str, str]]


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

    def embedded_linux(self, index: int) -> CodeType:
        chunks = re.findall(
            r"<<'PY'\n(.*?)\nPY",
            builder.source("linux/bootstrap-runner.sh").decode(),
            re.S,
        )
        self.assertEqual(len(chunks), 3)
        return compile(chunks[index], "bootstrap-runner.sh:python", "exec")

    def execute_linux(
        self, index: int, config: builder.LinuxConfig, opener: HttpOpener | None = None
    ) -> str:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            path.write_bytes(builder.configuration_bytes(config))
            output = io.StringIO()
            with (
                patch.object(sys, "argv", ["bootstrap", str(path)]),
                contextlib.redirect_stdout(output),
            ):
                if opener is None:
                    exec(self.embedded_linux(index), {})
                else:
                    with patch.object(
                        urllib.request, "build_opener", return_value=opener
                    ):
                        exec(self.embedded_linux(index), {})
            return output.getvalue()

    def test_embedded_linux_configuration_reader(self):
        config = self.configuration("linux")
        values = self.execute_linux(0, config).splitlines()
        self.assertEqual(
            values,
            [
                "kvm",
                "ciadmin",
                "microsoft/nvx",
                "",
                "/opt/nvx-runner",
                "0",
                "/mnt/data",
                "preinstalled",
            ],
        )

    def test_embedded_linux_passes_only_extra_labels_to_generic_setup(self):
        config = self.configuration("linux")
        config["runnerLabels"].append("isolated")
        self.assertEqual(self.execute_linux(0, config).splitlines()[3], "isolated")

    def test_embedded_linux_metadata_identity(self):
        config = self.configuration("linux")
        metadata = {
            "subscriptionId": config["subscriptionId"],
            "resourceGroupName": config["resourceGroup"].upper(),
            "vmScaleSetName": config["vmssName"].upper(),
            "name": config["vmssName"] + "_7",
        }

        class Opener:
            def open(
                self, request: urllib.request.Request, timeout: int
            ) -> io.StringIO:
                return io.StringIO(json.dumps(metadata))

        self.assertEqual(self.execute_linux(1, config, Opener()).strip(), "nvx-linux-8")
        config["runnerNumberOffset"] = 0
        self.assertEqual(self.execute_linux(1, config, Opener()).strip(), "nvx-linux-7")
        metadata["vmScaleSetName"] = "another-vmss"
        with self.assertRaisesRegex(SystemExit, "does not identify"):
            self.execute_linux(1, config, Opener())

    def test_embedded_linux_registration_flow(self):
        requests: list[urllib.request.Request] = []

        class Opener:
            def open(
                self, request: urllib.request.Request, timeout: int
            ) -> io.StringIO:
                requests.append(request)
                if "/metadata/identity/" in request.full_url:
                    data = {"access_token": "test-managed-identity"}
                elif ".vault.azure.net/" in request.full_url:
                    self.assert_header(request, "Bearer test-managed-identity")
                    data = {"value": "test-github-bootstrap"}
                elif request.full_url.endswith("/registration-token"):
                    self.assert_header(request, "Bearer test-github-bootstrap")
                    if request.get_method() != "POST":
                        raise AssertionError("Registration must use POST")
                    data = {"token": "test-registration"}
                else:
                    raise AssertionError("Unexpected URL")
                return io.StringIO(json.dumps(data))

            def assert_header(
                self, request: urllib.request.Request, expected: str
            ) -> None:
                if request.get_header("Authorization") != expected:
                    raise AssertionError("Wrong authorization header")

        self.assertEqual(
            self.execute_linux(2, self.configuration("linux"), Opener()).strip(),
            "test-registration",
        )
        self.assertEqual(len(requests), 3)

    def test_embedded_linux_empty_credential_fails(self):
        requests: list[urllib.request.Request] = []

        class Opener:
            def open(
                self, request: urllib.request.Request, timeout: int
            ) -> io.StringIO:
                requests.append(request)
                data = (
                    {"access_token": "test-managed-identity"}
                    if "/metadata/identity/" in request.full_url
                    else {"value": ""}
                )
                return io.StringIO(json.dumps(data))

        with self.assertRaisesRegex(SystemExit, "no runner bootstrap credential"):
            self.execute_linux(2, self.configuration("linux"), Opener())
        self.assertEqual(len(requests), 2)

    def test_linux_files_round_trip(self):
        config = self.configuration("linux")
        with tempfile.TemporaryDirectory() as temporary:
            manifest = builder.build_package(config, temporary)
            cloud = cast(
                CloudFiles,
                yaml.safe_load((Path(temporary) / "cloud-init.yaml").read_text()),
            )
            names: dict[str, bytes] = {}
            for entry in cloud["write_files"]:
                if entry.get("encoding") == "gz+b64":
                    content = gzip.decompress(
                        base64.b64decode(entry["content"], validate=True)
                    )
                    name = entry["path"].split("/")[-1]
                    names[name] = content
                    self.assertEqual(
                        hashlib.sha256(content).hexdigest(),
                        manifest["guestFiles"][name],
                    )
                    expected_mode = (
                        "0600"
                        if name == "config.json"
                        else ("0644" if name == "tool-versions.conf" else "0755")
                    )
                    self.assertEqual(entry["permissions"], expected_mode)
            self.assertEqual(
                set(names),
                {
                    "config.json",
                    "setup-linux-runner.sh",
                    "tool-versions.conf",
                    "bootstrap-runner.sh",
                    "block-runner-metadata.sh",
                    "setup-data-disk.sh",
                },
            )
            metadata_unit = next(
                entry
                for entry in cloud["write_files"]
                if entry["path"]
                == "/etc/systemd/system/nvx-runner-metadata-block.service"
            )
            self.assertEqual(metadata_unit["permissions"], "0644")
            self.assertIn(
                "ExecStart=/opt/nvx-bootstrap/block-runner-metadata.sh",
                metadata_unit["content"],
            )
            bootstrap = names["bootstrap-runner.sh"]
            self.assertLess(
                bootstrap.index(b'systemctl restart "$metadata_service"'),
                bootstrap.index(b"runner_token=$("),
            )
            self.assertIn(b'systemctl enable "$metadata_service"', bootstrap)
            self.assertIn(b"Requires=%s", bootstrap)
            self.assertIn(b"ExecStartPre=+%s", bootstrap)
            self.assertEqual(json.loads(names["config.json"]), config)
            self.assertEqual(
                names["setup-linux-runner.sh"], builder.source("setup-linux-runner.sh")
            )
            self.assertEqual(
                names["tool-versions.conf"], builder.source("tool-versions.conf")
            )
            self.assertLessEqual(manifest["customDataBytes"], 65535)

    def test_mshv_modes(self):
        config = self.configuration("linux")
        config["backend"] = "mshv"
        config["runnerLabels"] = ["linux", "mshv", "virtual-machine"]
        modes: tuple[tuple[str, set[str]], ...] = (
            ("preinstalled", set()),
            ("azurelinux-mshv", {"setup-mshv.sh", "verify-mshv.sh"}),
        )
        for mode, additional in modes:
            with self.subTest(mode=mode):
                config["kernelMode"] = mode
                with tempfile.TemporaryDirectory() as temporary:
                    manifest = builder.build_package(config, temporary)
                    self.assertEqual(
                        set(manifest["guestFiles"])
                        - {
                            "config.json",
                            "setup-linux-runner.sh",
                            "tool-versions.conf",
                            "setup-data-disk.sh",
                            "bootstrap-runner.sh",
                            "block-runner-metadata.sh",
                        },
                        additional,
                    )
                    self.assertLessEqual(manifest["customDataBytes"], 65535)

    def test_linux_runner_metadata_firewall_is_idempotent_and_fail_closed(self):
        shell = shutil.which("bash")
        if shell is None:
            self.skipTest("Bash is unavailable")
        source = builder.source("linux/block-runner-metadata.sh").decode()
        mock = r"""
id() {
    case "$*" in
        -u) printf '0\n' ;;
        '-u nvx-runner') printf '1001\n' ;;
        *) return 1 ;;
    esac
}
iptables() {
    case "$*" in
        '-w 10 -C OUTPUT -d 169.254.169.254/32 -m owner --uid-owner 1001 -j REJECT')
            [ "$installed" -eq 1 ] ;;
        '-w 10 -D OUTPUT -d 169.254.169.254/32 -m owner --uid-owner 1001 -j REJECT')
            [ "$installed" -eq 1 ] || return 1
            installed=0 ;;
        '-w 10 -I OUTPUT 1 -d 169.254.169.254/32 -m owner --uid-owner 1001 -j REJECT')
            [ "$fail_insert" -eq 0 ] || return 1
            installed=1
            insertions=$((insertions + 1)) ;;
        *) printf 'Unexpected firewall operation: %s\n' "$*" >&2; return 1 ;;
    esac
}
""".replace("\r\n", "\n")
        for initially_installed, fail_insert in ((0, 0), (1, 0), (0, 1)):
            with self.subTest(existing=initially_installed, failure=fail_insert):
                script = (
                    f"installed={initially_installed}\nfail_insert={fail_insert}\n"
                    "insertions=0\n"
                    + mock
                    + source
                    + '\ntest "$installed" -eq 1 && test "$insertions" -eq 1\n'
                )
                result = subprocess.run(
                    [shell, "-s"],
                    input=script.encode(),
                    capture_output=True,
                    timeout=30,
                    check=False,
                )
                if fail_insert:
                    self.assertNotEqual(result.returncode, 0)
                else:
                    self.assertEqual(
                        result.returncode,
                        0,
                        result.stdout.decode() + result.stderr.decode(),
                    )

    def test_windows_package_round_trip(self):
        config = self.configuration("windows")
        with tempfile.TemporaryDirectory() as temporary:
            manifest = builder.build_package(config, temporary)
            payload = json.loads((Path(temporary) / "custom-data.json").read_text())
            package = base64.b64decode(payload["package"]["data"], validate=True)
            self.assertEqual(
                hashlib.sha256(package).hexdigest(), payload["package"]["sha256"]
            )
            with zipfile.ZipFile(io.BytesIO(package)) as archive:
                self.assertEqual(set(archive.namelist()), set(manifest["guestFiles"]))
                for name, digest in manifest["guestFiles"].items():
                    self.assertEqual(
                        hashlib.sha256(archive.read(name)).hexdigest(), digest
                    )
                self.assertEqual(json.loads(archive.read("config.json")), config)
                self.assertEqual(
                    archive.read("setup-windows-whp.ps1"),
                    builder.source("setup-windows-whp.ps1"),
                )
                self.assertEqual(
                    archive.read("tool-versions.conf"),
                    builder.source("tool-versions.conf"),
                )
            self.assertEqual(
                base64.b64decode(payload["script"]),
                builder.source("windows/vmss-bootstrap.ps1"),
            )
            extra = base64.b64decode(payload["extraScriptBase64"]).decode()
            self.assertIn(
                base64.b64encode(
                    builder.source("windows/install-package.ps1")
                ).decode(),
                extra,
            )
            self.assertLessEqual(manifest["customDataBytes"], 65535)
            self.assertIn(
                "CustomData.bin",
                (Path(temporary) / "extension-command.txt").read_text(),
            )

    def test_windows_installer_and_vmss_identity_without_a_guest(self):
        powershell = shutil.which("powershell.exe")
        if os.name != "nt" or powershell is None:
            self.skipTest("Windows PowerShell is unavailable")
        installer = builder.source("windows/install-package.ps1")
        self.assertEqual(installer.count(b"$root = 'C:\\ProgramData\\NvxBootstrap'"), 1)
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "package"
            bootstrap_root = Path(temporary) / "bootstrap"
            manifest = builder.build_package(self.configuration("windows"), output)
            script = r"""
$ErrorActionPreference = 'Stop'
function Set-Acl { param($LiteralPath, $AclObject) }
$script:bootstraps = 0
function powershell.exe {
    $script:bootstraps++
    $global:LASTEXITCODE = 0
}
$scriptText = [IO.File]::ReadAllText($env:NVX_TEST_INSTALLER)
$needle = '$root = ' + [char]39 + 'C:\ProgramData\NvxBootstrap' + [char]39
$replacement = '$root = ' + [char]39 +
    $env:NVX_TEST_BOOTSTRAP.Replace("'", "''") + [char]39
$source = $scriptText.Replace($needle, $replacement)
$configPath = Join-Path $env:NVX_TEST_PACKAGE 'custom-data.json'
& ([scriptblock]::Create($source)) -ConfigPath $configPath
if ($script:bootstraps -ne 1) { throw 'Installer did not dispatch exactly one bootstrap' }
$manifest = Get-Content -LiteralPath (Join-Path $env:NVX_TEST_PACKAGE 'manifest.json') -Raw |
    ConvertFrom-Json
foreach ($file in $manifest.guestFiles.PSObject.Properties) {
    $algorithm = [Security.Cryptography.SHA256]::Create()
    try {
        $content = [IO.File]::ReadAllBytes((Join-Path $env:NVX_TEST_BOOTSTRAP $file.Name))
        $actual = ([BitConverter]::ToString($algorithm.ComputeHash($content))).
            Replace('-', '').ToLowerInvariant()
    }
    finally { $algorithm.Dispose() }
    if ($actual -ne $file.Value) { throw "Extracted file checksum mismatch: $($file.Name)" }
}
$payload = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
$payload.package.sha256 = '0' * 64
$badPath = Join-Path $env:NVX_TEST_PACKAGE 'invalid.json'
$payload | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $badPath
$rejected = $false
try { & ([scriptblock]::Create($source)) -ConfigPath $badPath }
catch {
    if ($_.Exception.Message -notmatch 'checksum mismatch') { throw }
    $rejected = $true
}
if (-not $rejected -or $script:bootstraps -ne 1) {
    throw 'Invalid package was not rejected before bootstrap'
}
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile(
    $env:NVX_TEST_WINDOWS_BOOTSTRAP, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw 'Azure Windows bootstrap has parse errors' }
$definition = $ast.Find({
    param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
    $node.Name -eq 'Get-RunnerName'
}, $true)
. ([scriptblock]::Create($definition.Extent.Text))
$labelsDefinition = $ast.Find({
    param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
    $node.Name -eq 'Get-ExtraRunnerLabels'
}, $true)
. ([scriptblock]::Create($labelsDefinition.Extent.Text))
if ((Get-ExtraRunnerLabels -RunnerLabels @('Windows', 'WHP', 'Virtual-Machine', 'isolated')) -ne
    'isolated') { throw 'Windows bootstrap rejected valid case-insensitive runner labels' }
$rejected = $false
try { Get-ExtraRunnerLabels -RunnerLabels @('windows', 'kvm', 'virtual-machine') | Out-Null }
catch { $rejected = $true }
if (-not $rejected) { throw 'Windows bootstrap accepted the wrong backend label' }
$script:config = Get-Content -LiteralPath (Join-Path $env:NVX_TEST_BOOTSTRAP 'config.json') -Raw |
    ConvertFrom-Json
$script:metadata = [pscustomobject]@{
    subscriptionId = $config.subscriptionId
    resourceGroupName = $config.resourceGroup
    vmScaleSetName = $config.vmssName
    name = $config.vmssName + '_7'
}
function Invoke-RestMethod {
    param($Uri, $Headers, $TimeoutSec)
    return $script:metadata
}
if ((Get-RunnerName) -ne ($config.runnerNamePrefix + '-8')) {
    throw 'Windows runner offset 1 produced the wrong name'
}
$config.runnerNumberOffset = 0
if ((Get-RunnerName) -ne ($config.runnerNamePrefix + '-7')) {
    throw 'Windows runner offset 0 produced the wrong name'
}
$metadata.resourceGroupName = 'another-group'
$rejected = $false
try { Get-RunnerName | Out-Null }
catch {
    if ($_.Exception.Message -notmatch 'configured VMSS') { throw }
    $rejected = $true
}
if (-not $rejected) { throw 'Windows VMSS identity mismatch was accepted' }
Write-Output NVX_AZURE_WINDOWS_MOCK_OK
"""
            result = subprocess.run(
                [
                    powershell,
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    script,
                ],
                env={
                    **os.environ,
                    "NVX_TEST_INSTALLER": str(
                        builder.ROOT / "windows/install-package.ps1"
                    ),
                    "NVX_TEST_WINDOWS_BOOTSTRAP": str(
                        builder.ROOT / "windows/bootstrap-runner.ps1"
                    ),
                    "NVX_TEST_BOOTSTRAP": str(bootstrap_root),
                    "NVX_TEST_PACKAGE": str(output),
                },
                capture_output=True,
                text=True,
                timeout=90,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("NVX_AZURE_WINDOWS_MOCK_OK", result.stdout)
            self.assertEqual(len(manifest["guestFiles"]), 5)

    def test_windows_disk_selection_requires_exact_guest_lun(self):
        powershell = shutil.which("powershell.exe")
        if os.name != "nt" or powershell is None:
            self.skipTest("Windows PowerShell is unavailable")
        script = r"""
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile(
    $env:NVX_TEST_DISK_SCRIPT, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw 'Data disk helper has parse errors' }
foreach ($name in @('Get-DiskAddress', 'Test-ManagedNvmeDisk',
        'Get-ManagedNvmeLun', 'Select-ManagedDataDisk')) {
    $definition = $ast.Find({
        param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq $name
    }, $true)
    if (-not $definition) { throw "Missing selection function: $name" }
    . ([scriptblock]::Create($definition.Extent.Text))
}
$nvme = [pscustomobject]@{
    Number = 2; IsBoot = $false; IsSystem = $false
    FriendlyName = 'MSFT NVMe Accelerator'; Model = ''
    Location = 'Integrated : Adapter 1 : Target 0 : LUN 0'
}
$guest = [pscustomobject]@{
    Index = 2; DeviceID = '\\.\PHYSICALDRIVE2'; SCSILogicalUnit = 1
}
if ((Select-ManagedDataDisk -Disks @($nvme) -GuestDisks @($guest) -Lun 0).Number -ne 2) {
    throw 'Windows NVMe namespace 1 did not map to Azure LUN 0'
}
if ($null -ne (Select-ManagedDataDisk -Disks @($nvme) -GuestDisks @($guest) -Lun 1)) {
    throw 'A sole disk at the wrong Azure LUN was selected'
}
if ($null -ne (Select-ManagedDataDisk -Disks @($nvme) -GuestDisks @() -Lun 0)) {
    throw 'An NVMe disk without a verified guest mapping was selected'
}
$guest.SCSILogicalUnit = 0
if ($null -ne (Select-ManagedDataDisk -Disks @($nvme) -GuestDisks @($guest) -Lun 0)) {
    throw 'An NVMe disk without a data namespace was selected'
}
$guest.SCSILogicalUnit = 1
if ($null -ne (Select-ManagedDataDisk -Disks @($nvme) -GuestDisks @($guest, $guest) -Lun 0)) {
    throw 'A duplicate guest mapping was selected'
}
$other = [pscustomobject]@{
    Number = 3; IsBoot = $false; IsSystem = $false
    FriendlyName = 'Virtual_Disk NVMe Premium'; Model = ''
    Location = 'NVMe controller'
}
$otherGuest = [pscustomobject]@{
    Index = 3; DeviceID = '\\.\PHYSICALDRIVE3'; SCSILogicalUnit = 1
}
if ($null -ne (Select-ManagedDataDisk -Disks @($nvme, $other) `
        -GuestDisks @($guest, $otherGuest) -Lun 0)) {
    throw 'Ambiguous Azure NVMe LUN was selected'
}
$scsi = [pscustomobject]@{
    Number = 4; IsBoot = $false; IsSystem = $false
    FriendlyName = 'Microsoft Virtual Disk'; Model = ''
    Location = 'Integrated : Adapter 1 : Target 0 : LUN 4'
}
if ((Select-ManagedDataDisk -Disks @($scsi) -GuestDisks @() -Lun 4).Number -ne 4) {
    throw 'Exact SCSI LUN no longer selects its disk'
}
$scsi.Location = 'Integrated : Adapter 1 : Target 0 : LUN 0'
if ($null -ne (Select-ManagedDataDisk -Disks @($scsi, $nvme) `
        -GuestDisks @($guest) -Lun 0)) {
    throw 'A disk was selected despite contradictory controller mappings'
}
"""
        result = subprocess.run(
            [
                powershell,
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                script,
            ],
            env={
                **os.environ,
                "NVX_TEST_DISK_SCRIPT": str(
                    builder.ROOT / "windows/setup-data-disk.ps1"
                ),
            },
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

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
        config = self.configuration("windows")
        config["runnerLabels"] = ["windows", "virtual-machine", "whp"]
        with self.assertRaisesRegex(ValueError, "runnerLabels must start"):
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
                        config.pop(name)
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

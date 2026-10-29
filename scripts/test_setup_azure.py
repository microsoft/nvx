import base64
import gzip
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from typing import Literal, TypedDict, cast, overload
from unittest.mock import patch

import yaml

from scripts.powershell_testing import run_powershell
from scripts.setup.azure import build_package as builder


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

    def execute_windows(self, script: str, **environment: str) -> str:
        powershell = shutil.which("powershell.exe")
        if os.name != "nt" or powershell is None:
            self.skipTest("Windows PowerShell is unavailable")
        sentinel = "NVX_AZURE_WINDOWS_MOCK_OK"
        result = run_powershell(
            [
                powershell,
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                script + f"\nWrite-Output {sentinel}\n",
            ],
            env={**os.environ, **environment},
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(sentinel, result.stdout)
        return result.stdout

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
        bootstrap = builder.source("windows/bootstrap-runner.ps1")
        self.assertIn(b"$registrationArguments += '-BenchmarkRunner'", bootstrap)
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "package"
            bootstrap_root = Path(temporary) / "bootstrap"
            manifest = builder.build_package(self.configuration("windows"), output)
            script = r"""
$ErrorActionPreference = 'Stop'
$script:trustedOwners = 0
function Set-Acl {
    param($LiteralPath, $AclObject)
    $owner = $AclObject.GetOwner([Security.Principal.SecurityIdentifier])
    if ($owner.Value -ne 'S-1-5-32-544' -or -not $AclObject.AreAccessRulesProtected) {
        throw 'Bootstrap directory ACL does not establish trusted ownership'
    }
    $script:trustedOwners++
}
$script:bootstraps = 0
$script:taskRegistrations = 0
$script:taskStarts = 0
$script:handoffFailure = ''
function powershell.exe {
    $script:bootstraps++
    throw 'Installer executed bootstrap synchronously'
}
function New-ScheduledTaskAction {
    param($Execute, $Argument)
    $expected = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' +
        (Join-Path $env:NVX_TEST_BOOTSTRAP 'bootstrap-runner.ps1') +
        '" -ConfigPath "' + (Join-Path $env:NVX_TEST_BOOTSTRAP 'config.json') + '"'
    if ($Execute -ne 'powershell.exe' -or $Argument -ne $expected) {
        throw 'Scheduled bootstrap action does not use the extracted sources'
    }
    return [pscustomobject]@{ Execute = $Execute; Argument = $Argument }
}
function New-ScheduledTaskPrincipal {
    param($UserId, $LogonType, $RunLevel)
    if ($UserId -ne 'SYSTEM' -or $LogonType -ne 'ServiceAccount' -or $RunLevel -ne 'Highest') {
        throw 'Bootstrap task does not run as privileged SYSTEM'
    }
    return [pscustomobject]@{ UserId = $UserId }
}
function New-ScheduledTaskTrigger {
    param([switch]$AtStartup)
    if (-not $AtStartup) { throw 'Bootstrap task cannot continue after reboot' }
    return 'startup'
}
function New-ScheduledTaskSettingsSet {
    param([switch]$StartWhenAvailable, $ExecutionTimeLimit, $RestartCount, $RestartInterval)
    if (-not $StartWhenAvailable -or $ExecutionTimeLimit -ne (New-TimeSpan -Hours 4) -or
        $RestartCount -ne 60 -or $RestartInterval -ne (New-TimeSpan -Minutes 2)) {
        throw 'Bootstrap task lost its execution budget or retry settings'
    }
    return [pscustomobject]@{ ExecutionTimeLimit = $ExecutionTimeLimit }
}
function Register-ScheduledTask {
    param($TaskName, $Action, $Trigger, $Principal, $Settings, [switch]$Force)
    if ($TaskName -ne 'Nvx-Setup-GitHub-Runner' -or -not $Force -or
        $Trigger -ne 'startup' -or $Principal.UserId -ne 'SYSTEM' -or
        $script:trustedOwners -lt 1) {
        throw 'Bootstrap was dispatched without trusted ownership and continuation'
    }
    if ($script:handoffFailure -eq 'registration') { throw 'fixture registration failure' }
    $script:taskRegistrations++
}
function Start-ScheduledTask {
    param($TaskName)
    if ($TaskName -ne 'Nvx-Setup-GitHub-Runner' -or $script:taskRegistrations -lt 1) {
        throw 'Bootstrap task was started before registration'
    }
    if ($script:handoffFailure -eq 'start') { throw 'fixture start failure' }
    $script:taskStarts++
}
$scriptText = [IO.File]::ReadAllText($env:NVX_TEST_INSTALLER)
$needle = '$root = ' + [char]39 + 'C:\ProgramData\NvxBootstrap' + [char]39
$replacement = '$root = ' + [char]39 +
    $env:NVX_TEST_BOOTSTRAP.Replace("'", "''") + [char]39
$source = $scriptText.Replace($needle, $replacement)
$configPath = Join-Path $env:NVX_TEST_PACKAGE 'custom-data.json'
New-Item -ItemType Directory -Path $env:NVX_TEST_BOOTSTRAP | Out-Null
$staging = @(& ([scriptblock]::Create($source)) -ConfigPath $configPath)
if ($script:bootstraps -ne 0 -or $script:trustedOwners -ne 1 -or
    $script:taskRegistrations -ne 1 -or $script:taskStarts -ne 1 -or
    $staging -notcontains 'NVX_CLOUD_RUNNER_BOOTSTRAP_STAGED=1' -or
    $staging -contains 'NVX_CLOUD_RUNNER_SETUP_COMPLETE=1') {
    throw 'Installer did not stage bootstrap independently of extension execution'
}
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
if (-not $rejected -or $script:bootstraps -ne 0 -or
    $script:taskRegistrations -ne 1 -or $script:taskStarts -ne 1) {
    throw 'Invalid package was not rejected before bootstrap'
}
foreach ($failure in @('registration', 'start')) {
    $script:handoffFailure = $failure
    $captured = [Collections.Generic.List[string]]::new()
    $rejected = $false
    try {
        & ([scriptblock]::Create($source)) -ConfigPath $configPath |
            ForEach-Object { $captured.Add([string]$_) }
    }
    catch {
        if ($_.Exception.Message -ne "fixture $failure failure") { throw }
        $rejected = $true
    }
    if (-not $rejected -or
        $captured -contains 'NVX_CLOUD_RUNNER_BOOTSTRAP_STAGED=1') {
        throw 'Installer reported successful staging after a task handoff failure'
    }
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
$roleDefinition = $ast.Find({
    param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
    $node.Name -eq 'Test-BenchmarkRunner'
}, $true)
. ([scriptblock]::Create($roleDefinition.Extent.Text))
. ([scriptblock]::Create($labelsDefinition.Extent.Text))
foreach ($name in @('Get-InstanceRunnerLabels', 'Select-SafeAzureComputeDisk',
        'ConvertTo-SafeAzureComputeMetadata', 'Write-AzureComputeSnapshot')) {
    $definition = $ast.Find({
        param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq $name
    }, $true)
    if (-not $definition) { throw "Windows bootstrap is missing $name" }
    . ([scriptblock]::Create($definition.Extent.Text))
}
if ((Get-ExtraRunnerLabels -RunnerLabels @('Windows', 'WHP', 'Virtual-Machine', 'isolated')) -ne
    'isolated') { throw 'Windows bootstrap rejected valid case-insensitive runner labels' }
if (Test-BenchmarkRunner -RunnerLabels @('windows', 'whp', 'virtual-machine')) {
    throw 'Windows bootstrap classified a default runner as benchmark'
}
if (Test-BenchmarkRunner -RunnerLabels @('windows', 'whp', 'virtual-machine', 'general')) {
    throw 'Windows bootstrap classified a general runner as benchmark'
}
if (-not (Test-BenchmarkRunner -RunnerLabels @(
    'windows', 'whp', 'virtual-machine', 'benchmark'
))) { throw 'Windows bootstrap did not classify the benchmark runner' }
$rejected = $false
try {
    Get-ExtraRunnerLabels -RunnerLabels @(
        'windows', 'whp', 'virtual-machine', 'general', 'benchmark'
    ) | Out-Null
}
catch { $rejected = $true }
if (-not $rejected) { throw 'Windows bootstrap accepted both runner roles' }
$rejected = $false
try { Get-ExtraRunnerLabels -RunnerLabels @('windows', 'kvm', 'virtual-machine') | Out-Null }
catch { $rejected = $true }
if (-not $rejected) { throw 'Windows bootstrap accepted the wrong backend label' }
$script:config = Get-Content -LiteralPath (Join-Path $env:NVX_TEST_BOOTSTRAP 'config.json') -Raw |
    ConvertFrom-Json
$config.runnerLabels += @('general', 'isolated')
$config | Add-Member -NotePropertyName benchmarkRunnerName -NotePropertyValue (
    $config.runnerNamePrefix + '-8'
)
$benchmarkLabels = @(Get-InstanceRunnerLabels -Config $config -RunnerName (
    $config.runnerNamePrefix + '-8'
))
$generalLabels = @(Get-InstanceRunnerLabels -Config $config -RunnerName (
    $config.runnerNamePrefix + '-9'
))
if (-not (Test-BenchmarkRunner -RunnerLabels $benchmarkLabels) -or
    $benchmarkLabels -contains 'general' -or
    (Test-BenchmarkRunner -RunnerLabels $generalLabels) -or
    $generalLabels -notcontains 'general' -or
    (Get-ExtraRunnerLabels -RunnerLabels $benchmarkLabels) -ne 'isolated' -or
    (Get-ExtraRunnerLabels -RunnerLabels $generalLabels) -ne 'isolated') {
    throw 'Windows bootstrap did not select exactly one benchmark runner'
}
$config.PSObject.Properties.Remove('benchmarkRunnerName')
if (Test-BenchmarkRunner -RunnerLabels @(
    Get-InstanceRunnerLabels -Config $config -RunnerName ($config.runnerNamePrefix + '-8')
)) { throw 'Windows bootstrap changed the default runner role' }
$script:metadata = [pscustomobject]@{
    subscriptionId = $config.subscriptionId
    resourceGroupName = $config.resourceGroup
    vmScaleSetName = $config.vmssName
    name = $config.vmssName + '_7'
    vmSize = 'Standard_D8s_v5'
    userData = 'DO-NOT-LEAK'
    storageProfile = [pscustomobject]@{
        osDisk = [pscustomobject]@{
            diskSizeGB = '128'
            caching = 'ReadWrite'
            writeAcceleratorEnabled = 'false'
            diffDiskSettings = [pscustomobject]@{ option = '' }
            managedDisk = [pscustomobject]@{
                storageAccountType = 'Premium_LRS'
                id = 'DO-NOT-LEAK'
            }
            name = 'DO-NOT-LEAK'
        }
        dataDisks = @([pscustomobject]@{
            lun = '0'
            diskSizeGB = '512'
            caching = 'None'
            writeAcceleratorEnabled = 'false'
            bytesPerSecondThrottle = ''
            opsPerSecondThrottle = ''
            managedDisk = [pscustomobject]@{
                storageAccountType = 'Premium_LRS'
                id = 'DO-NOT-LEAK'
            }
        })
        resourceDisk = [pscustomobject]@{ size = '614400' }
    }
}
$safe = ConvertTo-SafeAzureComputeMetadata -Metadata $metadata |
    ConvertTo-Json -Depth 8 -Compress
$projected = $safe | ConvertFrom-Json
if ($safe -match 'DO-NOT-LEAK' -or $safe -match 'subscriptionId' -or
    $projected.vmSize -ne 'Standard_D8s_v5' -or
    $projected.storageProfile.osDisk.managedDisk.storageAccountType -ne 'Premium_LRS' -or
    $projected.storageProfile.dataDisks[0].lun -ne '0' -or
    $projected.storageProfile.resourceDisk.size -ne '614400') {
    throw 'Windows bootstrap published unsafe or incomplete host metadata'
}
$script:metadataAclWrites = 0
function Set-Acl {
    param($LiteralPath, $AclObject)
    $owner = $AclObject.GetOwner([Security.Principal.SecurityIdentifier])
    $rules = @($AclObject.GetAccessRules(
        $true, $false, [Security.Principal.SecurityIdentifier]
    ))
    $principals = @($rules | ForEach-Object { $_.IdentityReference.Value })
    $networkService = @($rules | Where-Object {
        $_.IdentityReference.Value -eq 'S-1-5-20'
    })
    if ($owner.Value -ne 'S-1-5-32-544' -or
        -not $AclObject.AreAccessRulesProtected -or
        @(Compare-Object @('S-1-5-18', 'S-1-5-20', 'S-1-5-32-544') $principals).Count -ne 0 -or
        $networkService.Count -ne 1 -or
        ($networkService[0].FileSystemRights -band [Security.AccessControl.FileSystemRights]::Write) -ne 0 -or
        ($networkService[0].FileSystemRights -band [Security.AccessControl.FileSystemRights]::Delete) -ne 0) {
        throw "Host metadata ACL allows untrusted modification: $LiteralPath"
    }
    $script:metadataAclWrites++
}
$hostMetadataRoot = Join-Path $env:NVX_TEST_BOOTSTRAP 'host-metadata'
Write-AzureComputeSnapshot -Metadata $metadata
Write-AzureComputeSnapshot -Metadata $metadata
$snapshot = Get-Content -LiteralPath (Join-Path $hostMetadataRoot 'compute.json') -Raw
if ($script:metadataAclWrites -lt 6 -or $snapshot -match 'DO-NOT-LEAK' -or
    $snapshot -notmatch 'Standard_D8s_v5') {
    throw 'Windows host metadata snapshot was not sanitized and protected'
}
$metadata.vmSize = 'DO-NOT-LEAK'
$rejected = $false
try { ConvertTo-SafeAzureComputeMetadata -Metadata $metadata | Out-Null }
catch { $rejected = $true }
if (-not $rejected) { throw 'Windows bootstrap accepted an invalid VM size' }
$metadata.vmSize = 'Standard_D8s_v5'
if ((Get-RunnerName -Metadata $metadata) -ne ($config.runnerNamePrefix + '-8')) {
    throw 'Windows runner offset 1 produced the wrong name'
}
$config.runnerNumberOffset = 0
if ((Get-RunnerName -Metadata $metadata) -ne ($config.runnerNamePrefix + '-7')) {
    throw 'Windows runner offset 0 produced the wrong name'
}
$metadata.resourceGroupName = 'another-group'
$rejected = $false
try { Get-RunnerName -Metadata $metadata | Out-Null }
catch {
    if ($_.Exception.Message -notmatch 'configured VMSS') { throw }
    $rejected = $true
}
if (-not $rejected) { throw 'Windows VMSS identity mismatch was accepted' }
Write-Output NVX_AZURE_WINDOWS_MOCK_OK
"""
            result = run_powershell(
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
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("NVX_AZURE_WINDOWS_MOCK_OK", result.stdout)
            self.assertEqual(len(manifest["guestFiles"]), 5)

    def test_windows_bootstrap_reports_failure_after_scheduled_handoff(self):
        script = r"""
Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile(
    $env:NVX_TEST_WINDOWS_BOOTSTRAP, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw 'Windows bootstrap has parse errors' }
$main = @($ast.EndBlock.Statements | Where-Object {
    $_ -is [Management.Automation.Language.TryStatementAst]
})
if ($main.Count -ne 1) { throw 'Expected one top-level bootstrap operation' }
$continuationTask = 'Nvx-Setup-GitHub-Runner'
$ConfigPath = 'fixture-config.json'
$script:stops = 0
function Stop-Transcript { $script:stops++ }
function New-ScheduledTaskAction { throw 'Bootstrap must reuse the staged task' }
function powershell.exe { throw 'Unexpected disk or prerequisite operation' }
function Get-Content {
    param($LiteralPath, [switch]$Raw)
    if ($LiteralPath -ne $ConfigPath -or -not $Raw) { throw 'Unexpected configuration read' }
    if ($script:scenario -eq 'invalid-config') { return '{"os":"linux"}' }
    return '{"os":"windows"}'
}
function Get-ScheduledTask {
    param($TaskName)
    if ($TaskName -ne $continuationTask) { throw 'Wrong continuation task' }
    if ($script:scenario -ne 'missing-task') { return [pscustomobject]@{ TaskName = $TaskName } }
}
$script:firewallChecks = 0
function Set-RunnerMetadataFirewall { $script:firewallChecks++ }
function Invoke-RestMethod {
    if ($script:firewallChecks -eq 0) { throw 'IMDS was queried before the runner firewall' }
    throw 'fixture metadata failure'
}
foreach ($scenario in @('invalid-config', 'missing-task', 'metadata-failure')) {
    $script:scenario = $scenario
    $captured = [Collections.Generic.List[string]]::new()
    $rejected = $false
    try {
        & ([scriptblock]::Create($main[0].Extent.Text)) |
            ForEach-Object { $captured.Add([string]$_) }
    }
    catch {
        $expected = if ($scenario -eq 'invalid-config') {
            'Expected a Windows runner configuration'
        } elseif ($scenario -eq 'missing-task') {
            'Runner continuation task is missing'
        } else { 'fixture metadata failure' }
        if ($_.Exception.Message -notlike "$expected*") { throw }
        $rejected = $true
    }
    if (-not $rejected -or
        $captured -notcontains 'NVX_CLOUD_RUNNER_SETUP_FAILED=1' -or
        $captured -contains 'NVX_CLOUD_RUNNER_SETUP_COMPLETE=1') {
        throw 'Bootstrap failure was swallowed or reported as completed staging'
    }
}
if ($script:stops -ne 3) { throw 'Bootstrap did not close its transcript after failure' }
if ($script:firewallChecks -ne 1) { throw 'Runner firewall was not installed before IMDS' }
"""
        self.execute_windows(
            script,
            NVX_TEST_WINDOWS_BOOTSTRAP=str(
                builder.ROOT / "windows/bootstrap-runner.ps1"
            ),
        )

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
        result = run_powershell(
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
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_windows_raw_disk_scan_is_complete_bounded_and_fail_closed(self):
        source = builder.source("windows/setup-data-disk.ps1").decode()
        self.assertIn("$setupTimeout = New-TimeSpan -Hours 4", source)
        self.assertIn("$mutex.WaitOne($setupTimeout)", source)
        self.assertIn("-ExecutionTimeLimit $setupTimeout", source)
        script = r"""
Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile(
    $env:NVX_TEST_DISK_SCRIPT, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw 'Disk setup has parse errors' }
foreach ($name in @('Test-ZeroFilledStream', 'Test-DiskIsZero')) {
    $definition = $ast.Find({
        param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq $name
    }, $true)
    if (-not $definition) { throw "Missing disk verification function: $name" }
    $source = $definition.Extent.Text
    if ($name -eq 'Test-DiskIsZero') {
        $needle = '$path = "\\.\PhysicalDrive$($Disk.Number)"'
        if (-not $source.Contains($needle)) { throw 'Unexpected physical-drive path' }
        $source = $source.Replace($needle, '$path = $env:NVX_TEST_STREAM_FILE')
    }
    . ([scriptblock]::Create($source))
}
Add-Type -TypeDefinition @'
using System;
using System.IO;
namespace NvxDiskTests
{
    public sealed class ObservedStream : MemoryStream
    {
        public int MaxChunk = Int32.MaxValue;
        public long ThrowAt = -1;
        public int MaxRequested;
        public long BytesRead;
        public ObservedStream(byte[] bytes) : base(bytes) {}
        public override int Read(byte[] buffer, int offset, int count)
        {
            MaxRequested = Math.Max(MaxRequested, count);
            if (ThrowAt >= 0 && Position >= ThrowAt)
                throw new IOException("Mock disk read failure.");
            int read = base.Read(buffer, offset, Math.Min(count, MaxChunk));
            BytesRead += read;
            return read;
        }
    }
}
'@
$bytes = New-Object byte[] (2MB + 17)
foreach ($partial in @($false, $true)) {
    $stream = [NvxDiskTests.ObservedStream]::new($bytes)
    try {
        if ($partial) { $stream.MaxChunk = 4093 }
        if (-not (Test-ZeroFilledStream -Stream $stream -Length $bytes.Length)) {
            throw 'A zero-filled stream was rejected'
        }
        if ($stream.BytesRead -ne $bytes.Length -or $stream.MaxRequested -gt 1MB) {
            throw 'Disk verification did not use complete bounded reads'
        }
    }
    finally { $stream.Dispose() }
}
$bytes[$bytes.Length - 1] = 1
[IO.File]::WriteAllBytes($env:NVX_TEST_STREAM_FILE, $bytes)
$disk = [pscustomobject]@{ Number = 7; Size = [uint64]$bytes.Length }
if (Test-DiskIsZero $disk) { throw 'Nonzero data beyond the first MiB was accepted' }
$bytes[$bytes.Length - 1] = 0
[IO.File]::WriteAllBytes($env:NVX_TEST_STREAM_FILE, $bytes)
if (-not (Test-DiskIsZero $disk)) { throw 'A complete zero-filled disk was rejected' }

foreach ($failure in @('short', 'read')) {
    $stream = [NvxDiskTests.ObservedStream]::new($bytes)
    try {
        $length = [long]$bytes.Length
        if ($failure -eq 'short') { $length++ } else { $stream.ThrowAt = 1MB }
        $rejected = $false
        try { Test-ZeroFilledStream -Stream $stream -Length $length | Out-Null }
        catch {
            $expected = if ($failure -eq 'short') { 'reported size' } else { 'Mock disk read failure' }
            if ($_.Exception.Message -notlike "*$expected*") { throw }
            $rejected = $true
        }
        if (-not $rejected) { throw "Disk verification ignored a $failure failure" }
    }
    finally { $stream.Dispose() }
}
foreach ($size in @($null, 0, -1, $true, '1024', 1.5, [double]::NaN, [uint64]::MaxValue)) {
    $rejected = $false
    try { Test-DiskIsZero ([pscustomobject]@{ Number = 7; Size = $size }) | Out-Null }
    catch {
        if ($_.Exception.Message -notlike 'Invalid disk size*') { throw }
        $rejected = $true
    }
    if (-not $rejected) { throw "Invalid device size was accepted: $size" }
}
"""
        with tempfile.TemporaryDirectory() as temporary:
            self.execute_windows(
                script,
                NVX_TEST_DISK_SCRIPT=str(builder.ROOT / "windows/setup-data-disk.ps1"),
                NVX_TEST_STREAM_FILE=str(Path(temporary) / "disk"),
            )

    def test_windows_disk_initialization_requires_owned_state(self):
        script = r"""
Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$text = [IO.File]::ReadAllText($env:NVX_TEST_DISK_SCRIPT)
$start = $text.IndexOf("`n`$allPartitions = @(Get-Partition -DiskNumber")
$end = $text.IndexOf("`nif (`$volume.FileSystem -ne 'NTFS')", $start)
if ($start -lt 0 -or $end -le $start) { throw 'Missing initialization/formatting block' }
$body = $text.Substring($start, $end - $start).
    Replace('exit 1', "throw 'NVX_DISK_SETUP_REJECTED'")
$initialize = [scriptblock]::Create($body)
$initMarkerType = '{8f8f0f40-5c84-4a2a-8c22-8fbff2e8bc91}'
$basicDataType = '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'
function Write-DiskLog {
    param($Message)
    $script:messages += $Message
}
function Test-DiskIsZero {
    param($Disk)
    $script:scans++
    if ($script:scanError) { throw 'Mock disk read failure' }
    return $script:blank
}
function Initialize-Disk {
    param($Number, $PartitionStyle)
    if ($Number -ne 7 -or $PartitionStyle -ne 'GPT' -or
        $script:disk.PartitionStyle -ne 'RAW' -or -not $script:blank) {
        throw 'Unexpected disk initialization'
    }
    $script:initializations++
    $script:disk.PartitionStyle = 'GPT'
}
function Get-Disk {
    param($Number)
    if ($Number -ne 7) { throw 'Unexpected disk query' }
    return $script:disk
}
function Get-Partition {
    param($DiskNumber, $PartitionNumber, $ErrorAction)
    if ($DiskNumber -ne 7) { throw 'Unexpected partition query' }
    return @($script:parts | Where-Object {
        -not $PartitionNumber -or $_.PartitionNumber -eq $PartitionNumber
    })
}
function New-Partition {
    param($DiskNumber, $Size, $GptType, [switch]$UseMaximumSize)
    if ($DiskNumber -ne 7) { throw 'Unexpected partition target' }
    if ($GptType -eq $initMarkerType) {
        if ($script:initializations -ne 1 -or $Size -ne 16MB) {
            throw 'An unowned disk was claimed with a new marker'
        }
        $script:newMarkers++
        $partition = [pscustomobject]@{
            DiskNumber = 7; PartitionNumber = 1; Type = 'Unknown'; GptType = $GptType
        }
    }
    elseif ($GptType -eq $basicDataType -and $UseMaximumSize) {
        if (@($script:parts | Where-Object GptType -eq $initMarkerType).Count -ne 1) {
            throw 'Data partition created without an owned marker'
        }
        $script:newData++
        $partition = [pscustomobject]@{
            DiskNumber = 7; PartitionNumber = 2; Type = 'Basic'; GptType = $GptType
        }
    }
    else { throw 'Unexpected partition creation' }
    $script:parts += $partition
    return $partition
}
function Get-Volume {
    param($Partition, $ErrorAction)
    if (-not $Partition -or $Partition.DiskNumber -ne 7) {
        throw 'Unexpected volume query'
    }
    return $script:volume
}
function Format-Volume {
    param($Partition, $FileSystem, $NewFileSystemLabel, [switch]$Force, $Confirm)
    if ($args.Count -ne 0 -or -not $Partition -or
        $Partition.DiskNumber -ne 7 -or $Partition.PartitionNumber -ne 2 -or
        $FileSystem -ne 'NTFS' -or $NewFileSystemLabel -ne 'data' -or
        -not $Force -or $Confirm) { throw 'Unexpected formatting request' }
    if (@($script:parts | Where-Object GptType -eq $initMarkerType).Count -ne 1) {
        throw 'Unowned partition was formatted'
    }
    $script:formats++
    $script:volume.FileSystem = 'NTFS'
    $script:volume.FileSystemType = 'NTFS'
}
$cases = @(
    @{ Name='raw-zero'; Style='RAW'; Blank=$true; Marker=0; Data='none';
       Reject=$false; Init=1; NewMarker=1; Format=1 },
    @{ Name='raw-data'; Style='RAW'; Blank=$false; Marker=0; Data='none';
       Reject=$true; Init=0; NewMarker=0; Format=0 },
    @{ Name='raw-read-error'; Style='RAW'; Blank=$true; Marker=0; Data='none';
       Reject=$true; Init=0; NewMarker=0; Format=0 },
    @{ Name='unmarked-gpt'; Style='GPT'; Blank=$true; Marker=0; Data='none';
       Reject=$true; Init=0; NewMarker=0; Format=0 },
    @{ Name='reserved-only'; Style='GPT'; Blank=$true; Marker=0; Data='none';
       Reject=$true; Init=0; NewMarker=0; Format=0 },
    @{ Name='owned-empty'; Style='GPT'; Blank=$true; Marker=1; Data='none';
       Reject=$false; Init=0; NewMarker=0; Format=1 },
    @{ Name='owned-unformatted'; Style='GPT'; Blank=$true; Marker=1; Data='Unknown';
       Reject=$false; Init=0; NewMarker=0; Format=1 },
    @{ Name='owned-incomplete-ntfs'; Style='GPT'; Blank=$true; Marker=1; Data='NTFS';
       Reject=$false; Init=0; NewMarker=0; Format=1 },
    @{ Name='existing-ntfs'; Style='GPT'; Blank=$true; Marker=0; Data='NTFS';
       Reject=$false; Init=0; NewMarker=0; Format=0 },
    @{ Name='unowned-unformatted'; Style='GPT'; Blank=$true; Marker=0; Data='Unknown';
       Reject=$true; Init=0; NewMarker=0; Format=0 }
)
foreach ($case in $cases) {
    $script:disk = [pscustomobject]@{ Number=7; PartitionStyle=$case.Style }
    $script:parts = @()
    $script:initializations = 0
    $script:newMarkers = 0
    $script:newData = 0
    $script:formats = 0
    $script:scans = 0
    $script:messages = @()
    $script:blank = $case.Blank
    $script:scanError = $case.Name -eq 'raw-read-error'
    $script:volume = [pscustomobject]@{
        FileSystem = if ($case.Data -eq 'NTFS') { 'NTFS' } else { '' }
        FileSystemType = if ($case.Data -eq 'NTFS') { 'NTFS' } else { 'Unknown' }
    }
    if ($case.Marker) {
        $script:parts += [pscustomobject]@{
            DiskNumber=7; PartitionNumber=1; Type='Unknown'; GptType=$initMarkerType
        }
    }
    if ($case.Data -ne 'none') {
        $script:parts += [pscustomobject]@{
            DiskNumber=7; PartitionNumber=2; Type='Basic'; GptType=$basicDataType
        }
    }
    if ($case.Name -eq 'reserved-only') {
        $script:parts += [pscustomobject]@{
            DiskNumber=7; PartitionNumber=1; Type='Reserved'; GptType='reserved'
        }
    }
    $dataDisk = $script:disk
    $rejected = $false
    try { & $initialize }
    catch {
        if ($_.Exception.Message -notin @('NVX_DISK_SETUP_REJECTED', 'Mock disk read failure')) {
            throw
        }
        $rejected = $true
    }
    if ($rejected -ne $case.Reject -or
        $script:initializations -ne $case.Init -or
        $script:newMarkers -ne $case.NewMarker -or $script:formats -ne $case.Format) {
        throw "Unexpected disk operations for $($case.Name)"
    }
    if ($case.Style -eq 'RAW' -and $script:scans -ne 1) {
        throw 'RAW initialization skipped device verification'
    }
    if ($case.Style -eq 'GPT' -and $script:scans -ne 0) {
        throw 'An existing GPT disk was treated as a blank RAW disk'
    }
}
"""
        self.execute_windows(
            script,
            NVX_TEST_DISK_SCRIPT=str(builder.ROOT / "windows/setup-data-disk.ps1"),
        )

    def test_windows_pending_disk_reboot_invalidates_completion_before_continuing(self):
        script = r"""
Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$diskAst = [Management.Automation.Language.Parser]::ParseFile(
    $env:NVX_TEST_DISK_SCRIPT, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw 'Data disk helper has parse errors' }
$pending = $diskAst.Find({
    param($node)
    $node -is [Management.Automation.Language.IfStatementAst] -and
    $node.Clauses[0].Item1.Extent.Text -eq '$activePagefile -or $configuredPagefile'
}, $true)
if (-not $pending) { throw 'Missing pagefile reboot branch' }
# Keep the successful exit inside the isolated test scope.
$pendingBranch = [scriptblock]::Create($pending.Extent.Text.Replace('exit 0', 'return'))
$bootstrapAst = [Management.Automation.Language.Parser]::ParseFile(
    $env:NVX_TEST_WINDOWS_BOOTSTRAP, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw 'Windows bootstrap has parse errors' }
$readCompletion = $bootstrapAst.Find({
    param($node)
    $node -is [Management.Automation.Language.AssignmentStatementAst] -and
    $node.Left.Extent.Text -eq '$diskReady'
}, $true)
$gate = $bootstrapAst.Find({
    param($node)
    $node -is [Management.Automation.Language.IfStatementAst] -and
    $node.Clauses[0].Item1.Extent.Text -eq '-not $diskReady -or $diskReady.Completed -ne 1'
}, $true)
if (-not $readCompletion -or -not $gate) { throw 'Missing disk completion gate' }
$checkGate = [scriptblock]::Create(
    $readCompletion.Extent.Text + "`n" + $gate.Extent.Text + "`n" +
    '$script:gatePassed = $true')
$pendingPath = 'HKLM:\SOFTWARE\NvxRunner\DataDiskSetup'
$DataDriveLetter = 'F'
$fallback = 'T'
$conflictDisk = [pscustomobject]@{ Number = 1 }
function Write-DiskLog { param($Message) }
function Ensure-RegistryKey { param($Path) }
function Get-CimInstance {
    param($ClassName)
    if ($ClassName -ne 'Win32_OperatingSystem') { throw 'Unexpected CIM query' }
    return [pscustomobject]@{ LastBootUpTime = [datetime]'2026-10-16T00:00:00Z' }
}
function Get-RegistryValue {
    param($Path, $Name)
    return $script:state[$Name]
}
function Set-ItemProperty {
    param($Path, $Name, $Value)
    if ($Path -eq $pendingPath) {
        if ($Name -ne 'Completed' -and $script:state.Completed -ne 0) {
            throw 'Completion was not invalidated before pending reboot state'
        }
        $script:state[$Name] = $Value
    }
    elseif ($Name -ne 'PagingFiles') { throw 'Unexpected registry write' }
}
function Register-Continuation {
    if ($script:state.Completed -ne 0) { throw 'Continuation retained completion' }
    $script:continuations++
}
function shutdown.exe {
    if ($script:state.Completed -ne 0 -or $script:continuations -ne 1) {
        throw 'Reboot scheduled before pending state was ready'
    }
    $script:reboots++
    $global:LASTEXITCODE = 0
}
function Get-ItemProperty {
    param($Path, $Name, $ErrorAction)
    if ($Path -ne $pendingPath -or $Name -ne 'Completed') {
        throw 'Unexpected registry read'
    }
    return [pscustomobject]$script:state
}
foreach ($case in @(
    @{ Active = $true; Configured = $false },
    @{ Active = $false; Configured = $true },
    @{ Active = $true; Configured = $true },
    @{ Active = $false; Configured = $false }
)) {
    $activePagefile = $case.Active
    $configuredPagefile = $case.Configured
    $script:state = @{ Completed = 1 }
    $script:continuations = 0
    $script:reboots = 0
    & $pendingBranch
    $expectsReboot = $activePagefile -or $configuredPagefile
    if ($script:state.Completed -ne [int](-not $expectsReboot) -or
        $script:continuations -ne [int]$expectsReboot -or
        $script:reboots -ne [int]$expectsReboot) {
        throw 'Unexpected completion or reboot state'
    }
    $script:gatePassed = $false
    & $checkGate
    if ($script:gatePassed -eq $expectsReboot) {
        throw 'Bootstrap did not honor pending disk completion'
    }
    $script:state.Completed = 1
    $script:gatePassed = $false
    & $checkGate
    if (-not $script:gatePassed) { throw 'Completed disk setup did not allow bootstrap' }
}
"""
        self.execute_windows(
            script,
            NVX_TEST_DISK_SCRIPT=str(builder.ROOT / "windows/setup-data-disk.ps1"),
            NVX_TEST_WINDOWS_BOOTSTRAP=str(
                builder.ROOT / "windows/bootstrap-runner.ps1"
            ),
        )

    def test_windows_runner_metadata_firewall_is_idempotent_and_fail_closed(self):
        script = r"""
Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile(
    $env:NVX_TEST_WINDOWS_BOOTSTRAP, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw 'Windows bootstrap has parse errors' }
$definition = $ast.Find({
    param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
    $node.Name -eq 'Set-RunnerMetadataFirewall'
}, $true)
if (-not $definition) { throw 'Missing metadata firewall function' }
. ([scriptblock]::Create($definition.Extent.Text))
function Reset-Firewall {
    $script:rule = [pscustomobject]@{
        Direction = 'Outbound'; Action = 'Block'; Enabled = 'True'
        Profile = 'Any'; Platform = @()
        PolicyAppId = ''; PackageFamilyName = ''; RemoteDynamicKeywordAddresses = @()
        IsEffective = $false
    }
    $script:services = @{ BFE = 'Running'; MpsSvc = 'Running' }
    $script:profiles = @('Domain', 'Private', 'Public' | ForEach-Object {
        [pscustomobject]@{
            Name = $_; Enabled = 'True'; AllowLocalFirewallRules = 'NotConfigured'
            DisabledInterfaceAliases = @('NotConfigured')
        }
    })
    $script:missingEffective = $false
    $script:duplicateEffective = $false
    $script:effectiveAction = $null
    $script:effectivePorts = $null
    $script:primaryStatus = 'OK'
    $script:enforcement = [uint16[]]@(1)
    $script:activeQueries = 0
    $script:addresses = @('169.254.169.254')
    $script:localAddresses = @('Any')
    $script:ports = [pscustomobject]@{
        Protocol = 'Any'; LocalPort = @('Any'); RemotePort = @('Any')
        IcmpType = @('Any'); DynamicTarget = 'Any'
    }
    $script:application = [pscustomobject]@{ Program = 'Any'; Package = 'Any' }
    $script:service = [pscustomobject]@{ Service = 'Any' }
    $script:interface = [pscustomobject]@{ InterfaceAlias = @('Any') }
    $script:interfaceType = [pscustomobject]@{ InterfaceType = 'Any' }
    $script:security = [pscustomobject]@{
        Authentication = 'NotRequired'; Encryption = 'NotRequired'
        RemoteUser = 'Any'; RemoteMachine = 'Any'
    }
    $script:users = 'D:(A;;CC;;;NS)'
    $script:creations = 0
    $script:failCreation = $false
}
function Get-Service {
    param($Name, $ErrorAction)
    if (-not $script:services.ContainsKey($Name) -or $ErrorAction -ne 'Stop') {
        throw 'Unexpected firewall service query'
    }
    return [pscustomobject]@{ Name = $Name; Status = $script:services[$Name] }
}
function Get-NetFirewallProfile {
    param($PolicyStore, $ErrorAction)
    if ($PolicyStore -ne 'ActiveStore' -or $ErrorAction -ne 'Stop') {
        throw 'Expected effective firewall profile query'
    }
    return $script:profiles
}
function Get-NetFirewallRule {
    param($Name, $PolicyStore, [switch]$TracePolicyStore, $ErrorAction)
    if ($Name -ne 'NvxRunnerDenyMetadata') { throw 'Unexpected firewall rule name' }
    if ($PolicyStore -eq 'PersistentStore') { return $script:rule }
    if ($PolicyStore -ne 'ActiveStore' -or -not $TracePolicyStore -or
        $ErrorAction -ne 'Stop') { throw 'Expected traced effective firewall rule query' }
    $script:activeQueries++
    if ($script:missingEffective) { return $null }
    $effective = $script:rule | Select-Object *
    $effective.IsEffective = $true
    $effective | Add-Member -NotePropertyMembers @{
        PrimaryStatus = $script:primaryStatus
        CimInstanceProperties = @{
            EnforcementStatus = [pscustomobject]@{ Value = $script:enforcement }
        }
    }
    if ($script:effectiveAction) { $effective.Action = $script:effectiveAction }
    if ($script:duplicateEffective) { return @($effective, $effective) }
    return $effective
}
function New-NetFirewallRule {
    param($Name, $DisplayName, $Direction, $Action, $RemoteAddress, $LocalUser, $Profile,
        $Authentication, $Encryption)
    if ($Name -ne 'NvxRunnerDenyMetadata' -or $DisplayName -ne $Name -or
        $Direction -ne 'Outbound' -or $Action -ne 'Block' -or
        $RemoteAddress -ne '169.254.169.254' -or
        $LocalUser -ne 'D:(A;;CC;;;S-1-5-20)' -or $Profile -ne 'Any' -or
        $Authentication -ne 'NotRequired' -or $Encryption -ne 'NotRequired') {
        throw 'Incorrect metadata firewall creation arguments'
    }
    if ($script:failCreation) { throw 'Mock firewall creation failed' }
    $script:creations++
    $script:rule = [pscustomobject]@{
        Direction = $Direction; Action = $Action; Enabled = 'True'
        Profile = $Profile; Platform = @()
        PolicyAppId = ''; PackageFamilyName = ''; RemoteDynamicKeywordAddresses = @()
        IsEffective = $false
    }
    $script:addresses = @($RemoteAddress)
    $script:users = $LocalUser
    return $script:rule
}
function Assert-EffectiveRule {
    param($Rule)
    if (-not $Rule.IsEffective) { throw 'Filters were read from the stored rule' }
}
function Get-NetFirewallAddressFilter {
    param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process {
        Assert-EffectiveRule $InputObject
        [pscustomobject]@{
            LocalAddress = $script:localAddresses; RemoteAddress = $script:addresses
        }
    }
}
function Get-NetFirewallPortFilter {
    param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process {
        Assert-EffectiveRule $InputObject
        if ($script:effectivePorts) { $script:effectivePorts } else { $script:ports }
    }
}
function Get-NetFirewallApplicationFilter {
    param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process { Assert-EffectiveRule $InputObject; $script:application }
}
function Get-NetFirewallServiceFilter {
    param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process { Assert-EffectiveRule $InputObject; $script:service }
}
function Get-NetFirewallInterfaceFilter {
    param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process { Assert-EffectiveRule $InputObject; $script:interface }
}
function Get-NetFirewallInterfaceTypeFilter {
    param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process { Assert-EffectiveRule $InputObject; $script:interfaceType }
}
function Get-NetFirewallSecurityFilter {
    param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process {
        Assert-EffectiveRule $InputObject
        [pscustomobject]@{
            LocalUser = $script:users
            RemoteUser = $script:security.RemoteUser
            RemoteMachine = $script:security.RemoteMachine
            Authentication = $script:security.Authentication
            Encryption = $script:security.Encryption
        }
    }
}
Reset-Firewall
Set-RunnerMetadataFirewall
if ($script:creations -ne 0 -or $script:activeQueries -ne 1) {
    throw 'Existing firewall rule was not verified in the effective store'
}
Reset-Firewall
foreach ($profile in $script:profiles) {
    $profile.AllowLocalFirewallRules = 'True'
    $profile.DisabledInterfaceAliases = @()
}
Set-RunnerMetadataFirewall
Reset-Firewall
$script:enforcement = [uint16[]]@(5, 1)
Set-RunnerMetadataFirewall
Reset-Firewall
$script:ports.Protocol = 256
Set-RunnerMetadataFirewall
Reset-Firewall
$script:application.Package = $null
foreach ($field in @('PolicyAppId', 'PackageFamilyName', 'RemoteDynamicKeywordAddresses')) {
    $script:rule.PSObject.Properties.Remove($field)
}
Set-RunnerMetadataFirewall
Reset-Firewall
$script:rule = $null
Set-RunnerMetadataFirewall
Set-RunnerMetadataFirewall
if ($script:creations -ne 1) { throw 'Firewall creation was not idempotent' }
foreach ($case in @('principal', 'address', 'extra-address', 'missing-address',
        'direction', 'action', 'disabled', 'profile', 'platform', 'local-address',
        'protocol', 'local-port', 'remote-port', 'icmp-type', 'dynamic-target',
        'program', 'package', 'service', 'interface', 'interface-type',
        'remote-user', 'remote-machine', 'authentication', 'encryption',
        'policy-app', 'package-family', 'dynamic-address', 'firewall-stopped',
        'filter-engine-stopped', 'profile-disabled', 'profile-unconfigured',
        'local-rules-disabled', 'local-rules-unknown', 'disabled-interface',
        'missing-profile', 'duplicate-profile', 'missing-effective',
        'duplicate-effective', 'unenforced', 'not-applicable', 'empty-enforcement',
        'inactive-only', 'partially-unenforced', 'unknown-enforcement',
        'bad-status', 'effective-allow', 'effective-port')) {
    Reset-Firewall
    switch ($case) {
        'principal' { $script:users = 'D:(A;;CC;;;S-1-5-18)' }
        'address' { $script:addresses = @('169.254.169.253') }
        'extra-address' { $script:addresses += '169.254.169.253' }
        'missing-address' { $script:addresses = @() }
        'direction' { $script:rule.Direction = 'Inbound' }
        'action' { $script:rule.Action = 'Allow' }
        'disabled' { $script:rule.Enabled = 'False' }
        'profile' { $script:rule.Profile = 'Domain' }
        'platform' { $script:rule.Platform = @('6.3') }
        'local-address' { $script:localAddresses = @('10.0.0.4') }
        'protocol' { $script:ports.Protocol = 'TCP' }
        'local-port' { $script:ports.LocalPort = @('443') }
        'remote-port' { $script:ports.RemotePort = @('443') }
        'icmp-type' { $script:ports.IcmpType = @('8') }
        'dynamic-target' { $script:ports.DynamicTarget = 'ProximityApps' }
        'program' { $script:application.Program = 'C:\restricted.exe' }
        'package' { $script:application.Package = 'S-1-15-2-1' }
        'service' { $script:service.Service = 'RestrictedService' }
        'interface' { $script:interface.InterfaceAlias = @('RestrictedInterface') }
        'interface-type' { $script:interfaceType.InterfaceType = 'Wireless' }
        'remote-user' { $script:security.RemoteUser = 'D:(A;;CC;;;S-1-5-18)' }
        'remote-machine' { $script:security.RemoteMachine = 'D:(A;;CC;;;S-1-5-18)' }
        'authentication' { $script:security.Authentication = 'Required' }
        'encryption' { $script:security.Encryption = 'Required' }
        'policy-app' { $script:rule.PolicyAppId = 'RestrictedPolicy' }
        'package-family' { $script:rule.PackageFamilyName = 'RestrictedPackage' }
        'dynamic-address' { $script:rule.RemoteDynamicKeywordAddresses = @('{00000000-0000-0000-0000-000000000001}') }
        'firewall-stopped' { $script:services.MpsSvc = 'Stopped' }
        'filter-engine-stopped' { $script:services.BFE = 'Stopped' }
        'profile-disabled' { $script:profiles[0].Enabled = 'False' }
        'profile-unconfigured' { $script:profiles[0].Enabled = 'NotConfigured' }
        'local-rules-disabled' { $script:profiles[0].AllowLocalFirewallRules = 'False' }
        'local-rules-unknown' { $script:profiles[0].AllowLocalFirewallRules = 'Unknown' }
        'disabled-interface' { $script:profiles[0].DisabledInterfaceAliases = @('Ethernet') }
        'missing-profile' { $script:profiles = @($script:profiles | Select-Object -First 2) }
        'duplicate-profile' { $script:profiles[0].Name = 'Public' }
        'missing-effective' { $script:missingEffective = $true }
        'duplicate-effective' { $script:duplicateEffective = $true }
        'unenforced' { $script:enforcement = [uint16[]]@(2) }
        'not-applicable' { $script:enforcement = [uint16[]]@(0) }
        'empty-enforcement' { $script:enforcement = [uint16[]]@() }
        'inactive-only' { $script:enforcement = [uint16[]]@(5) }
        'partially-unenforced' { $script:enforcement = [uint16[]]@(1, 2) }
        'unknown-enforcement' { $script:enforcement = [uint16[]]@(1, 26) }
        'bad-status' { $script:primaryStatus = 'Error' }
        'effective-allow' { $script:effectiveAction = 'Allow' }
        'effective-port' {
            $script:effectivePorts = $script:ports | Select-Object *
            $script:effectivePorts.RemotePort = @('443')
        }
    }
    $rejected = $false
    try { Set-RunnerMetadataFirewall }
    catch {
        if (-not $_.Exception.Message.StartsWith(
            'The runner account is not blocked from Azure instance metadata.',
            [StringComparison]::Ordinal)) { throw }
        $rejected = $true
    }
    if (-not $rejected -or $script:creations -ne 0) {
        throw "Invalid firewall rule was accepted: $case"
    }
}
Reset-Firewall
$script:rule = $null
$script:failCreation = $true
$rejected = $false
try { Set-RunnerMetadataFirewall }
catch {
    if ($_.Exception.Message -ne 'Mock firewall creation failed') { throw }
    $rejected = $true
}
if (-not $rejected) { throw 'Firewall creation failure was ignored' }
"""
        self.execute_windows(
            script,
            NVX_TEST_WINDOWS_BOOTSTRAP=str(
                builder.ROOT / "windows/bootstrap-runner.ps1"
            ),
        )

    def test_windows_missing_ssh_advice_uses_supported_image_requirement(self):
        source = builder.source("windows/vmss-bootstrap.ps1").decode()
        self.assertNotIn("enableSsh", source)
        self.assertIn("Use an image that supports OpenSSH Server.", source)

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

    def test_windows_zip_metadata_is_independent_of_builder_platform(self):
        outputs: list[dict[str, bytes]] = []
        for host_system in (0, 3):

            class HostZipInfo(zipfile.ZipInfo):
                def __init__(
                    self,
                    filename: str = "NoName",
                    date_time: tuple[int, int, int, int, int, int] = (
                        1980,
                        1,
                        1,
                        0,
                        0,
                        0,
                    ),
                    *,
                    creator_system: int = host_system,
                ) -> None:
                    super().__init__(filename, date_time)
                    self.create_system = creator_system

            with (
                self.subTest(host_system=host_system),
                tempfile.TemporaryDirectory() as temporary,
                patch.object(builder.zipfile, "ZipInfo", HostZipInfo),
            ):
                builder.build_package(self.configuration("windows"), temporary)
                outputs.append(
                    {path.name: path.read_bytes() for path in Path(temporary).iterdir()}
                )
        self.assertEqual(outputs[0], outputs[1])
        with zipfile.ZipFile(io.BytesIO(outputs[0]["sources.zip"])) as archive:
            for entry in archive.infolist():
                self.assertEqual(entry.create_system, 3)
                self.assertEqual(entry.date_time, (1980, 1, 1, 0, 0, 0))
                self.assertEqual(entry.external_attr, 0o100644 << 16)

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
        with zipfile.ZipFile(io.BytesIO(outputs[0]["sources.zip"])) as archive:
            for name in archive.namelist():
                if name.endswith(".ps1"):
                    self.assertNotIn(b"\r", archive.read(name))

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
                        with zipfile.ZipFile(
                            Path(temporary) / "sources.zip"
                        ) as archive:
                            packaged = json.loads(archive.read("config.json"))
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

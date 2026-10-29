"""Tests of the Windows commands of the Azure runner bootstrap.

The Python decisions run against fake adapters, registry values, commands,
and HTTP endpoints on every platform. On Windows, native PowerShell tests
also run each adapter and the entry shim against mocked cmdlets, so no test
changes the host's disks, firewall, services, tasks, or registry.
"""

import base64
import contextlib
import hashlib
import io
import json
import lzma
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import unittest
from collections.abc import Callable, Generator, Mapping, Sequence
from pathlib import Path
from typing import BinaryIO, cast
from unittest.mock import patch

from scripts.powershell_testing import run_powershell
from scripts.setup.azure import build_package as builder
from scripts.setup.azure.nvx_azure import (
    cli,
    common,
    windows_bootstrap,
    windows_disk,
    windows_firewall,
    windows_stage,
    windows_system,
)
from scripts.setup.azure.nvx_azure.common import BootstrapError, Completed, JsonObject

AdapterHandler = Callable[[JsonObject], object]


class Adapters:
    """Answer adapter operations from handlers, recording every call."""

    def __init__(self, handlers: Mapping[tuple[str, str], AdapterHandler]) -> None:
        self.handlers = dict(handlers)
        self.calls: list[tuple[str, str, JsonObject]] = []

    def __call__(
        self,
        script: str,
        operation: str,
        request: JsonObject | None = None,
        root: Path = windows_system.ROOT,
    ) -> object:
        self.calls.append((script, operation, dict(request or {})))
        handler = self.handlers.get((script, operation))
        if handler is None:
            raise AssertionError(f"unexpected adapter call: {script} {operation}")
        return handler(request or {})

    def operations(self) -> list[str]:
        return [operation for _, operation, _ in self.calls]


class Registry:
    """Model HKEY_LOCAL_MACHINE values, recording every write in order."""

    def __init__(self, values: Mapping[tuple[str, str], object] | None = None) -> None:
        self.values: dict[tuple[str, str], object] = dict(values or {})
        self.writes: list[tuple[str, object]] = []

    def get(self, key: str, name: str) -> object | None:
        return self.values.get((key, name))

    def set(self, key: str, name: str, value: object) -> None:
        self.writes.append((name, value))
        self.values[(key, name)] = value

    def delete(self, key: str, name: str) -> None:
        self.writes.append((name, None))
        self.values.pop((key, name), None)

    def patch(self) -> contextlib.ExitStack:
        stack = contextlib.ExitStack()
        stack.enter_context(patch.object(windows_system, "registry_value", self.get))
        stack.enter_context(
            patch.object(windows_system, "set_registry_value", self.set)
        )
        stack.enter_context(
            patch.object(windows_system, "delete_registry_value", self.delete)
        )
        return stack


class Commands:
    """Answer external commands from a handler, recording every call."""

    def __init__(self, handler: Callable[[list[str], bytes | None], Completed]) -> None:
        self.handler = handler
        self.calls: list[list[str]] = []
        self.inputs: list[bytes | None] = []
        self.environments: list[Mapping[str, str] | None] = []

    def __call__(
        self,
        args: Sequence[str],
        *,
        stdin: bytes | None = None,
        check: bool = True,
        capture: bool = True,
        output: BinaryIO | None = None,
        env: Mapping[str, str] | None = None,
        cwd: Path | None = None,
    ) -> Completed:
        command = list(args)
        self.calls.append(command)
        self.inputs.append(stdin)
        self.environments.append(env)
        result = self.handler(command, stdin)
        if output is not None:
            output.write(result.stdout)
            output.write(result.stderr)
            output.flush()
            result = Completed(result.returncode)
        if check and result.returncode:
            raise BootstrapError(
                f"{command[0]} failed with exit code {result.returncode}"
            )
        return result


def run_adapter_powershell(
    args: Sequence[str],
    *,
    stdin: bytes | None = None,
    check: bool = True,
    capture: bool = True,
    output: BinaryIO | None = None,
    env: Mapping[str, str] | None = None,
    cwd: Path | None = None,
) -> Completed:
    """Run an adapter under the tests' shared PowerShell timeout."""
    if not capture or output is not None:
        raise AssertionError("adapter tests must capture their byte-valued result")
    raw = run_powershell(
        list(args),
        cwd=cwd,
        env=env,
        input=None if stdin is None else stdin.decode("utf-8"),
    )
    result = Completed(raw.returncode, raw.stdout.encode(), raw.stderr.encode())
    if check and result.returncode:
        raise BootstrapError(f"{args[0]} failed with exit code {result.returncode}")
    return result


def windows_config() -> builder.WindowsConfig:
    config = builder.load_config(builder.ROOT / "windows" / "config.example.json")
    assert config["os"] == "windows"
    return config


def as_json(config: builder.WindowsConfig) -> JsonObject:
    return cast(JsonObject, json.loads(builder.configuration_bytes(config)))


def captured_log() -> tuple[common.Log, io.StringIO]:
    return common.Log(), io.StringIO()


def firewall_rule() -> JsonObject:
    return {
        "direction": "Outbound",
        "action": "Block",
        "enabled": "True",
        "profile": "Any",
        "platform": "",
        "primaryStatus": "OK",
        "enforcement": [1],
        "restrictions": {
            "policyAppId": "",
            "packageFamilyName": "",
            "remoteDynamicKeywordAddresses": "",
        },
        "package": "Any",
        "protocol": "Any",
        "authentication": "NotRequired",
        "encryption": "NotRequired",
        "remoteAddresses": ["169.254.169.254"],
        "filters": {
            name: "Any"
            for name in (
                "localAddress",
                "localPort",
                "remotePort",
                "icmpType",
                "dynamicTarget",
                "program",
                "service",
                "interfaceAlias",
                "interfaceType",
                "remoteUser",
                "remoteMachine",
            )
        },
        "localUser": "D:(A;;CC;;;NS)",
    }


def firewall_facts() -> JsonObject:
    return {
        "services": {"BFE": "Running", "MpsSvc": "Running"},
        "profiles": [
            {
                "name": name,
                "enabled": "True",
                "allowLocalFirewallRules": "NotConfigured",
                "disabledInterfaceAliases": ["NotConfigured"],
            }
            for name in ("Domain", "Private", "Public")
        ],
        "persistent": True,
        "effective": [firewall_rule()],
        "principal": "D:(A;;CC;;;NS)",
    }


def mutate(facts: JsonObject, case: str) -> None:
    """Apply one way in which a firewall would leave metadata reachable."""
    rule = cast(JsonObject, cast(list[object], facts["effective"])[0])
    filters = cast(JsonObject, rule["filters"])
    restrictions = cast(JsonObject, rule["restrictions"])
    services = cast(JsonObject, facts["services"])
    profiles = cast(list[JsonObject], facts["profiles"])
    updates: dict[str, Callable[[], object]] = {
        "principal": lambda: rule.update(localUser="D:(A;;CC;;;SY)"),
        "address": lambda: rule.update(remoteAddresses=["169.254.169.253"]),
        "extra-address": lambda: rule.update(
            remoteAddresses=["169.254.169.254", "169.254.169.253"]
        ),
        "missing-address": lambda: rule.update(remoteAddresses=[]),
        "direction": lambda: rule.update(direction="Inbound"),
        "action": lambda: rule.update(action="Allow"),
        "disabled": lambda: rule.update(enabled="False"),
        "profile": lambda: rule.update(profile="Domain"),
        "platform": lambda: rule.update(platform="6.3"),
        "local-address": lambda: filters.update(localAddress="10.0.0.4"),
        "protocol": lambda: rule.update(protocol="TCP"),
        "local-port": lambda: filters.update(localPort="443"),
        "remote-port": lambda: filters.update(remotePort="443"),
        "icmp-type": lambda: filters.update(icmpType="8"),
        "dynamic-target": lambda: filters.update(dynamicTarget="ProximityApps"),
        "program": lambda: filters.update(program="C:\\restricted.exe"),
        "package": lambda: rule.update(package="S-1-15-2-1"),
        "service": lambda: filters.update(service="RestrictedService"),
        "interface": lambda: filters.update(interfaceAlias="RestrictedInterface"),
        "interface-type": lambda: filters.update(interfaceType="Wireless"),
        "remote-user": lambda: filters.update(remoteUser="D:(A;;CC;;;S-1-5-18)"),
        "remote-machine": lambda: filters.update(remoteMachine="D:(A;;CC;;;S-1-5-18)"),
        "authentication": lambda: rule.update(authentication="Required"),
        "encryption": lambda: rule.update(encryption="Required"),
        "policy-app": lambda: restrictions.update(policyAppId="RestrictedPolicy"),
        "package-family": lambda: restrictions.update(
            packageFamilyName="RestrictedPackage"
        ),
        "dynamic-address": lambda: restrictions.update(
            remoteDynamicKeywordAddresses="{00000000-0000-0000-0000-000000000001}"
        ),
        "missing-filter": lambda: filters.pop("remoteMachine"),
        "firewall-stopped": lambda: services.update(MpsSvc="Stopped"),
        "filter-engine-stopped": lambda: services.update(BFE="Stopped"),
        "profile-disabled": lambda: profiles[0].update(enabled="False"),
        "profile-unconfigured": lambda: profiles[0].update(enabled="NotConfigured"),
        "local-rules-disabled": lambda: profiles[0].update(
            allowLocalFirewallRules="False"
        ),
        "local-rules-unknown": lambda: profiles[0].update(
            allowLocalFirewallRules="Unknown"
        ),
        "disabled-interface": lambda: profiles[0].update(
            disabledInterfaceAliases=["Ethernet"]
        ),
        "missing-profile": lambda: profiles.pop(),
        "duplicate-profile": lambda: profiles[0].update(name="Public"),
        "missing-effective": lambda: facts.update(effective=[]),
        "duplicate-effective": lambda: facts.update(
            effective=[firewall_rule(), firewall_rule()]
        ),
        "unenforced": lambda: rule.update(enforcement=[2]),
        "not-applicable": lambda: rule.update(enforcement=[0]),
        "empty-enforcement": lambda: rule.update(enforcement=[]),
        "partially-unenforced": lambda: rule.update(enforcement=[1, 2]),
        "unknown-enforcement": lambda: rule.update(enforcement=[1, 26]),
        "inactive-only": lambda: rule.update(enforcement=[5]),
        "duplicate": lambda: rule.update(primaryStatus="Inactive", enforcement=[25]),
        "boolean-enforcement": lambda: rule.update(enforcement=[True]),
        "bad-status": lambda: rule.update(primaryStatus="Error"),
    }
    updates[case]()


FIREWALL_CASES = (
    "principal",
    "address",
    "extra-address",
    "missing-address",
    "direction",
    "action",
    "disabled",
    "profile",
    "platform",
    "local-address",
    "protocol",
    "local-port",
    "remote-port",
    "icmp-type",
    "dynamic-target",
    "program",
    "package",
    "service",
    "interface",
    "interface-type",
    "remote-user",
    "remote-machine",
    "authentication",
    "encryption",
    "policy-app",
    "package-family",
    "dynamic-address",
    "missing-filter",
    "firewall-stopped",
    "filter-engine-stopped",
    "profile-disabled",
    "profile-unconfigured",
    "local-rules-disabled",
    "local-rules-unknown",
    "disabled-interface",
    "missing-profile",
    "duplicate-profile",
    "missing-effective",
    "duplicate-effective",
    "unenforced",
    "not-applicable",
    "empty-enforcement",
    "partially-unenforced",
    "unknown-enforcement",
    "inactive-only",
    "duplicate",
    "boolean-enforcement",
    "bad-status",
)


class WindowsFirewallTests(unittest.TestCase):
    def run_block(self, facts: list[JsonObject], create: AdapterHandler) -> Adapters:
        inspections = iter(facts)
        adapters = Adapters(
            {
                ("firewall.ps1", "Inspect"): lambda request: next(inspections),
                ("firewall.ps1", "Create"): create,
            }
        )
        with patch.object(windows_system, "adapter", adapters):
            windows_firewall.ensure_metadata_block()
        return adapters

    def test_existing_rule_is_verified_in_the_effective_store(self):
        adapters = self.run_block([firewall_facts()], lambda request: True)
        self.assertEqual(adapters.operations(), ["Inspect"])
        self.assertEqual(
            adapters.calls[0][2],
            {"name": "NvxRunnerDenyMetadata", "principal": "D:(A;;CC;;;S-1-5-20)"},
        )

    def test_accepted_equivalent_settings(self):
        # The real-host shapes: one active profile, and a profile that reports
        # its disabled interfaces as an empty name.
        for case in (
            "explicit-profiles",
            "empty-alias",
            "protocol-number",
            "no-package",
            "inactive-profiles",
        ):
            with self.subTest(case=case):
                facts = firewall_facts()
                rule = cast(JsonObject, cast(list[object], facts["effective"])[0])
                if case in ("explicit-profiles", "empty-alias"):
                    aliases = [] if case == "explicit-profiles" else [""]
                    for profile in cast(list[JsonObject], facts["profiles"]):
                        profile.update(
                            allowLocalFirewallRules="True",
                            disabledInterfaceAliases=aliases,
                        )
                elif case == "protocol-number":
                    rule["protocol"] = "256"
                elif case == "no-package":
                    rule["package"] = ""
                else:
                    rule["enforcement"] = [5, 1]
                self.run_block([facts], lambda request: True)

    def test_missing_rule_is_created_once_and_then_verified(self):
        missing = firewall_facts()
        missing.update(persistent=False, effective=[])
        created: list[JsonObject] = []
        adapters = self.run_block(
            [missing, firewall_facts()], lambda request: created.append(request)
        )
        self.assertEqual(adapters.operations(), ["Inspect", "Create", "Inspect"])
        self.assertEqual(
            created,
            [
                {
                    "name": "NvxRunnerDenyMetadata",
                    "address": "169.254.169.254",
                    "principal": "D:(A;;CC;;;S-1-5-20)",
                }
            ],
        )

    def test_creation_failure_is_not_ignored(self):
        missing = firewall_facts()
        missing.update(persistent=False, effective=[])

        def fail(request: JsonObject) -> object:
            raise BootstrapError("Mock firewall creation failed")

        with self.assertRaisesRegex(BootstrapError, "Mock firewall creation failed"):
            self.run_block([missing], fail)

    def test_invalid_firewall_states_are_rejected(self):
        for case in FIREWALL_CASES:
            with self.subTest(case=case):
                facts = firewall_facts()
                mutate(facts, case)
                with self.assertRaises(BootstrapError) as raised:
                    self.run_block([facts], lambda request: self.fail("created"))
                self.assertTrue(
                    str(raised.exception).startswith(windows_firewall.FAILURE),
                    raised.exception,
                )

    def test_a_stopped_service_is_rejected_before_creating_a_rule(self):
        missing = firewall_facts()
        missing.update(persistent=False, effective=[])
        cast(JsonObject, missing["services"])["BFE"] = "Stopped"
        with self.assertRaisesRegex(BootstrapError, "service BFE is not running"):
            self.run_block([missing], lambda request: self.fail("created"))


def nvme_disk(number: int = 2, name: str = "MSFT NVMe Accelerator") -> JsonObject:
    return {
        "number": number,
        "isBoot": False,
        "isSystem": False,
        "friendlyName": name,
        "model": "",
        "location": "Integrated : Adapter 1 : Target 0 : LUN 0",
    }


def guest_disk(index: int = 2, unit: object = 1) -> JsonObject:
    return {
        "index": index,
        "deviceId": f"\\\\.\\PHYSICALDRIVE{index}",
        "scsiLogicalUnit": unit,
    }


class WindowsDiskSelectionTests(unittest.TestCase):
    def test_disk_selection_requires_the_exact_guest_lun(self):
        select = windows_disk.select_managed_disk
        nvme = nvme_disk()
        selected = select([nvme], [guest_disk()], 0)
        self.assertEqual(selected and selected["number"], 2)
        # A sole disk at another LUN, a missing or data-less namespace, and a
        # duplicate guest mapping select nothing.
        self.assertIsNone(select([nvme], [guest_disk()], 1))
        self.assertIsNone(select([nvme], [], 0))
        self.assertIsNone(select([nvme], [guest_disk(unit=0)], 0))
        self.assertIsNone(select([nvme], [guest_disk(unit=None)], 0))
        self.assertIsNone(select([nvme], [guest_disk(unit=True)], 0))
        self.assertIsNone(select([nvme], [guest_disk(), guest_disk()], 0))
        other = nvme_disk(3, "Virtual_Disk NVMe Premium")
        other["location"] = "NVMe controller"
        self.assertIsNone(select([nvme, other], [guest_disk(), guest_disk(3)], 0))
        scsi: JsonObject = {
            "number": 4,
            "isBoot": False,
            "isSystem": False,
            "friendlyName": "Microsoft Virtual Disk",
            "model": "",
            "location": "Integrated : Adapter 1 : Target 0 : LUN 4",
        }
        selected = select([scsi], [], 4)
        self.assertEqual(selected and selected["number"], 4)
        scsi["location"] = "Integrated : Adapter 1 : Target 0 : LUN 0"
        self.assertIsNone(select([scsi, nvme], [guest_disk()], 0))
        # The boot disk and the temporary disk's adapter never qualify.
        self.assertIsNone(select([{**scsi, "isBoot": True}], [], 0))
        self.assertIsNone(select([{**scsi, "isSystem": True}], [], 0))
        self.assertIsNone(
            select(
                [{**scsi, "location": "Integrated : Adapter 0 : Target 0 : LUN 0"}],
                [],
                0,
            )
        )

    def test_temporary_disk_is_positively_identified(self):
        temporary: JsonObject = {
            "number": 1,
            "friendlyName": "Microsoft Virtual Disk",
            "model": "",
            "location": "Integrated : Adapter 0 : Port 0 : Target 0 : LUN 1",
        }
        partition: JsonObject = {"diskNumber": 1, "partitionNumber": 1}
        adapters = Adapters({("storage.ps1", "Volume"): lambda request: None})
        with patch.object(windows_system, "adapter", adapters):
            self.assertTrue(windows_disk.is_temporary_disk(temporary, partition))
            self.assertTrue(
                windows_disk.is_temporary_disk(
                    {
                        **temporary,
                        "location": "",
                        "friendlyName": "Microsoft NVMe Direct Disk",
                    },
                    partition,
                )
            )
            self.assertFalse(
                windows_disk.is_temporary_disk({**temporary, "location": ""}, partition)
            )
        labelled = Adapters(
            {
                ("storage.ps1", "Volume"): lambda request: {
                    "fileSystem": "NTFS",
                    "fileSystemType": "NTFS",
                    "fileSystemLabel": "Temporary Storage",
                }
            }
        )
        with patch.object(windows_system, "adapter", labelled):
            self.assertTrue(
                windows_disk.is_temporary_disk({**temporary, "location": ""}, partition)
            )


class StorageModel:
    """Model one data disk for the partition and formatting decisions."""

    def __init__(self, style: str, blank: bool, scan_error: bool = False) -> None:
        self.disk: JsonObject = {"number": 7, "partitionStyle": style, "size": 4096}
        self.parts: list[JsonObject] = []
        self.volume: JsonObject | None = None
        self.blank = blank
        self.scan_error = scan_error
        self.scans = 0
        self.initializations = 0
        self.new_markers = 0
        self.formats = 0

    def scan(self, disk: JsonObject) -> bool:
        self.scans += 1
        if self.scan_error:
            raise OSError("Mock disk read failure")
        return self.blank

    def markers(self) -> list[JsonObject]:
        return [
            part
            for part in self.parts
            if part["gptType"] == windows_disk.INIT_MARKER_TYPE
        ]

    def initialize(self, request: JsonObject) -> object:
        if (
            request != {"number": 7}
            or self.disk["partitionStyle"] != "RAW"
            or not self.blank
        ):
            raise AssertionError("Unexpected disk initialization")
        self.initializations += 1
        self.disk = {**self.disk, "partitionStyle": "GPT"}
        return self.disk

    def new_partition(self, request: JsonObject) -> object:
        if request.get("diskNumber") != 7:
            raise AssertionError("Unexpected partition target")
        if request.get("gptType") == windows_disk.INIT_MARKER_TYPE:
            if self.initializations != 1 or request.get("size") != 16 * 1024 * 1024:
                raise AssertionError("An unowned disk was claimed with a new marker")
            self.new_markers += 1
            part: JsonObject = {
                "diskNumber": 7,
                "partitionNumber": 1,
                "type": "Unknown",
                "gptType": windows_disk.INIT_MARKER_TYPE,
                "offset": 1024,
            }
        elif (
            request.get("gptType") == windows_disk.BASIC_DATA_TYPE
            and "size" not in request
        ):
            if len(self.markers()) != 1:
                raise AssertionError("Data partition created without an owned marker")
            part = {
                "diskNumber": 7,
                "partitionNumber": 2,
                "type": "Basic",
                "gptType": windows_disk.BASIC_DATA_TYPE,
                "offset": 16 * 1024 * 1024 + 1024,
            }
        else:
            raise AssertionError("Unexpected partition creation")
        self.parts.append(part)
        return part

    def format(self, request: JsonObject) -> object:
        if request != {"diskNumber": 7, "partitionNumber": 2}:
            raise AssertionError("Unexpected formatting request")
        if len(self.markers()) != 1:
            raise AssertionError("Unowned partition was formatted")
        self.formats += 1
        self.volume = {
            "fileSystem": "NTFS",
            "fileSystemType": "NTFS",
            "fileSystemLabel": "data",
        }
        return self.volume

    def adapters(self) -> Adapters:
        return Adapters(
            {
                ("storage.ps1", "Partitions"): lambda request: list(self.parts),
                ("storage.ps1", "InitializeGpt"): self.initialize,
                ("storage.ps1", "NewPartition"): self.new_partition,
                ("storage.ps1", "Volume"): lambda request: self.volume,
                ("storage.ps1", "FormatNtfs"): self.format,
            }
        )


class WindowsDiskTests(unittest.TestCase):
    def setUp(self) -> None:
        self.mutexes: list[tuple[str, float]] = []

        @contextlib.contextmanager
        def held(name: str, timeout_seconds: float) -> Generator[None, None, None]:
            self.mutexes.append((name, timeout_seconds))
            yield

        mutex = patch.object(windows_system, "named_mutex", held)
        mutex.start()
        self.addCleanup(mutex.stop)

    def test_raw_disk_scan_is_complete_and_validates_the_reported_size(self):
        with tempfile.TemporaryDirectory() as temporary:
            image = Path(temporary) / "disk"
            data = bytearray(2 * 1024 * 1024 + 17)
            data[-1] = 1
            image.write_bytes(bytes(data))
            disk: JsonObject = {"number": 7, "size": len(data)}
            with patch.object(windows_disk, "physical_drive", return_value=str(image)):
                self.assertFalse(windows_disk.disk_is_zero(disk))
                data[-1] = 0
                image.write_bytes(bytes(data))
                self.assertTrue(windows_disk.disk_is_zero(disk))
                with self.assertRaisesRegex(BootstrapError, "ended before"):
                    windows_disk.disk_is_zero({**disk, "size": len(data) + 1})
                for size in (None, 0, -1, True, "1024", 1.5, float("nan"), 2**64 - 1):
                    with self.subTest(size=size):
                        with self.assertRaisesRegex(
                            BootstrapError, "Invalid disk size"
                        ):
                            windows_disk.disk_is_zero({**disk, "size": size})

    def test_raw_disk_open_and_read_errors_keep_disk_context(self):
        disk: JsonObject = {"number": 7, "size": 1}
        with (
            patch("builtins.open", side_effect=OSError("Mock open failure")),
            self.assertRaises(BootstrapError) as raised,
        ):
            windows_disk.disk_is_zero(disk)
        self.assertEqual(
            str(raised.exception),
            r"could not read managed data disk 7 at \\.\PhysicalDrive7: Mock open failure",
        )
        self.assertIsInstance(raised.exception.__cause__, OSError)

        with (
            patch("builtins.open", return_value=io.BytesIO(b"\0")),
            patch.object(
                common, "zero_filled", side_effect=OSError("Mock read failure")
            ),
            self.assertRaises(BootstrapError) as raised,
        ):
            windows_disk.disk_is_zero(disk)
        self.assertEqual(
            str(raised.exception),
            r"could not read managed data disk 7 at \\.\PhysicalDrive7: Mock read failure",
        )
        self.assertIsInstance(raised.exception.__cause__, OSError)

    def check_initialization(
        self,
        name: str,
        style: str,
        blank: bool,
        marker: bool,
        data: str,
        rejected: bool,
        counts: tuple[int, int, int],
    ) -> None:
        model = StorageModel(style, blank, scan_error=name == "raw-read-error")
        if marker:
            model.parts.append(
                {
                    "diskNumber": 7,
                    "partitionNumber": 1,
                    "type": "Unknown",
                    "gptType": windows_disk.INIT_MARKER_TYPE,
                    "offset": 1024,
                }
            )
        if data != "none":
            model.parts.append(
                {
                    "diskNumber": 7,
                    "partitionNumber": 2,
                    "type": "Basic",
                    "gptType": windows_disk.BASIC_DATA_TYPE,
                    "offset": 16 * 1024 * 1024 + 1024,
                }
            )
            model.volume = {
                "fileSystem": "NTFS" if data == "NTFS" else "",
                "fileSystemType": "NTFS" if data == "NTFS" else "Unknown",
                "fileSystemLabel": "",
            }
        if name == "reserved-only":
            model.parts.append(
                {
                    "diskNumber": 7,
                    "partitionNumber": 1,
                    "type": "Reserved",
                    "gptType": "{e3c9e316-0b5c-4db8-817d-f92df00215ae}",
                }
            )
        with (
            patch.object(windows_system, "adapter", model.adapters()),
            patch.object(windows_disk, "disk_is_zero", model.scan),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            if rejected:
                with self.assertRaises((BootstrapError, OSError)):
                    windows_disk.prepare_volume(model.disk, common.Log())
            else:
                partition, volume, _ = windows_disk.prepare_volume(
                    model.disk, common.Log()
                )
                self.assertEqual(partition["partitionNumber"], 2)
                self.assertEqual(volume["fileSystem"], "NTFS")
        self.assertEqual(
            (model.initializations, model.new_markers, model.formats), counts
        )
        # Only a RAW disk is scanned; an existing GPT disk never counts as blank.
        self.assertEqual(model.scans, 1 if style == "RAW" else 0)

    def test_disk_initialization_requires_owned_state(self):
        for case in (
            ("raw-zero", "RAW", True, False, "none", False, (1, 1, 1)),
            ("raw-data", "RAW", False, False, "none", True, (0, 0, 0)),
            ("raw-read-error", "RAW", True, False, "none", True, (0, 0, 0)),
            ("unmarked-gpt", "GPT", True, False, "none", True, (0, 0, 0)),
            ("reserved-only", "GPT", True, False, "none", True, (0, 0, 0)),
            ("owned-empty", "GPT", True, True, "none", False, (0, 0, 1)),
            ("owned-unformatted", "GPT", True, True, "Unknown", False, (0, 0, 1)),
            ("owned-incomplete-ntfs", "GPT", True, True, "NTFS", False, (0, 0, 1)),
            ("existing-ntfs", "GPT", True, False, "NTFS", False, (0, 0, 0)),
            ("unowned-unformatted", "GPT", True, False, "Unknown", True, (0, 0, 0)),
            ("mbr", "MBR", True, False, "none", True, (0, 0, 0)),
        ):
            with self.subTest(case=case[0]):
                self.check_initialization(*case)

    def test_two_data_partitions_are_never_chosen_between(self):
        model = StorageModel("GPT", True)
        for number in (2, 3):
            model.parts.append(
                {
                    "diskNumber": 7,
                    "partitionNumber": number,
                    "type": "Basic",
                    "gptType": windows_disk.BASIC_DATA_TYPE,
                }
            )
        with (
            patch.object(windows_system, "adapter", model.adapters()),
            patch.object(windows_disk, "disk_is_zero", model.scan),
            self.assertRaisesRegex(BootstrapError, "unexpected partition layout"),
        ):
            windows_disk.prepare_volume(model.disk, common.Log())

    def conflict(
        self,
        registry: Registry,
        *,
        active: bool,
        configured: bool,
        task: bool = True,
        location: str = "Integrated : Adapter 0 : Target 0 : LUN 1",
    ) -> tuple[Adapters, Commands, bool]:
        letters = {"F": {"diskNumber": 1, "partitionNumber": 1}}
        registry.values[(windows_disk.MEMORY_KEY, "PagingFiles")] = (
            ["f:\\pagefile.sys 0 0"] if configured else ["C:\\pagefile.sys 0 0"]
        )

        def move(request: JsonObject) -> object:
            self.assertEqual(
                request, {"diskNumber": 1, "partitionNumber": 1, "letter": "T"}
            )
            letters["T"] = letters.pop("F")
            return True

        adapters = Adapters(
            {
                ("storage.ps1", "PartitionAtLetter"): lambda request: letters.get(
                    str(request["letter"])
                ),
                ("storage.ps1", "Disk"): lambda request: {
                    "number": 1,
                    "friendlyName": "Microsoft Virtual Disk",
                    "model": "",
                    "location": location,
                },
                ("storage.ps1", "Volume"): lambda request: None,
                ("storage.ps1", "UsedLetters"): lambda request: ["C", "D", "F"],
                ("storage.ps1", "ActivePagefiles"): lambda request: (
                    ["F:\\pagefile.sys"] if active else []
                ),
                ("storage.ps1", "SetPartitionLetter"): move,
                ("system.ps1", "BootTime"): lambda request: "boot-2",
                ("system.ps1", "TaskExists"): lambda request: task,
            }
        )
        commands = Commands(lambda command, stdin: Completed(0))
        with (
            registry.patch(),
            patch.object(windows_system, "adapter", adapters),
            patch.object(common, "run_command", commands),
            patch.object(common, "sleep"),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            released = windows_disk.release_letter(
                {"number": 7}, "F", "T", common.Log()
            )
        return adapters, commands, released

    def test_pending_disk_reboot_invalidates_completion_before_continuing(self):
        for active, configured in (
            (True, False),
            (False, True),
            (True, True),
            (False, False),
        ):
            with self.subTest(active=active, configured=configured):
                registry = Registry({(windows_disk.PENDING_KEY, "Completed"): 1})
                adapters, commands, released = self.conflict(
                    registry, active=active, configured=configured
                )
                reboot = active or configured
                self.assertEqual(released, not reboot)
                completed = registry.values[(windows_disk.PENDING_KEY, "Completed")]
                self.assertEqual(completed, 0 if reboot else 1)
                if reboot:
                    # Completion is invalidated before any other pending state,
                    # and the pagefile moves only once continuation is certain.
                    self.assertEqual(
                        [name for name, _ in registry.writes],
                        [
                            "Completed",
                            "TempDiskNumber",
                            "TempDriveLetter",
                            "RebootRequestedFromBoot",
                            "PagingFiles",
                        ],
                    )
                    self.assertEqual(registry.writes[0], ("Completed", 0))
                    self.assertEqual(
                        registry.writes[-1], ("PagingFiles", ["C:\\pagefile.sys 0 0"])
                    )
                    self.assertEqual(
                        commands.calls,
                        [
                            [
                                "shutdown.exe",
                                "/r",
                                "/t",
                                "60",
                                "/c",
                                "Completing persistent Azure data-disk setup after "
                                "moving the pagefile.",
                            ]
                        ],
                    )
                    self.assertNotIn("SetPartitionLetter", adapters.operations())
                else:
                    self.assertEqual(registry.writes, [])
                    self.assertEqual(commands.calls, [])
                    self.assertIn("SetPartitionLetter", adapters.operations())

    def test_persisting_pagefile_conflict_after_its_reboot_fails(self):
        registry = Registry(
            {
                (windows_disk.PENDING_KEY, "Completed"): 1,
                (windows_disk.PENDING_KEY, "RebootRequestedFromBoot"): "boot-1",
            }
        )
        with self.assertRaisesRegex(
            BootstrapError, "persists after the continuation reboot"
        ):
            self.conflict(registry, active=True, configured=False)
        self.assertEqual(registry.values[(windows_disk.PENDING_KEY, "Completed")], 0)

    def test_pagefile_moves_only_with_a_continuation_task(self):
        registry = Registry()
        with self.assertRaisesRegex(BootstrapError, "continuation task is missing"):
            self.conflict(registry, active=True, configured=False, task=False)
        self.assertEqual(registry.writes, [("Completed", 0)])
        self.assertNotIn(
            (windows_disk.PENDING_KEY, "RebootRequestedFromBoot"), registry.values
        )

    def test_unidentified_letter_owner_is_preserved(self):
        with self.assertRaisesRegex(BootstrapError, "not positively identified"):
            self.conflict(
                Registry(),
                active=False,
                configured=False,
                location="Integrated : Adapter 2 : Target 0 : LUN 3",
            )

    def test_setup_mounts_the_verified_volume_and_records_completion(self):
        model = StorageModel("RAW", True)
        letters: dict[str, JsonObject] = {}
        disk: JsonObject = {
            "number": 7,
            "isBoot": False,
            "isSystem": False,
            "isOffline": True,
            "isReadOnly": False,
            "partitionStyle": "RAW",
            "friendlyName": "Microsoft Virtual Disk",
            "model": "",
            "location": "Integrated : Adapter 1 : Target 0 : LUN 0",
            "size": 4096,
        }
        model.disk = dict(disk)

        def remove(request: JsonObject) -> object:
            self.assertEqual(request, {"diskNumber": 7, "partitionNumber": 1})
            model.parts = [part for part in model.parts if part["partitionNumber"] != 1]
            return True

        def assign(request: JsonObject) -> object:
            self.assertEqual(
                request, {"diskNumber": 7, "partitionNumber": 2, "letter": "F"}
            )
            letters["F"] = {"diskNumber": 7, "partitionNumber": 2}
            return True

        adapters = model.adapters()
        adapters.handlers.update(
            {
                ("storage.ps1", "Disks"): lambda request: {
                    "disks": [disk],
                    "guestDisks": [],
                },
                ("storage.ps1", "PartitionAtLetter"): lambda request: letters.get(
                    str(request["letter"])
                ),
                ("storage.ps1", "SetDisk"): lambda request: (
                    self.assertEqual(
                        request, {"number": 7, "online": True, "writable": False}
                    ),
                    {**model.disk, "isOffline": False},
                )[1],
                ("storage.ps1", "RemovePartition"): remove,
                ("storage.ps1", "VolumeAtLetter"): lambda request: (
                    model.volume if "F" in letters else None
                ),
                ("storage.ps1", "SetPartitionLetter"): assign,
            }
        )
        registry = Registry(
            {(windows_disk.PENDING_KEY, "RebootRequestedFromBoot"): "boot-1"}
        )
        output = io.StringIO()
        with (
            registry.patch(),
            patch.object(windows_system, "adapter", adapters),
            patch.object(windows_disk, "disk_is_zero", model.scan),
            contextlib.redirect_stdout(output),
        ):
            self.assertTrue(windows_disk.setup(0, "f:", "T", common.Log()))
        self.assertEqual(registry.values[(windows_disk.PENDING_KEY, "Completed")], 1)
        self.assertNotIn(
            (windows_disk.PENDING_KEY, "RebootRequestedFromBoot"), registry.values
        )
        self.assertEqual(model.parts[0]["partitionNumber"], 2)
        self.assertIn("finished OK; managed disk is mounted at F:", output.getvalue())
        # The earlier disk task's mutex serializes setup with a run of it.
        self.assertEqual(
            self.mutexes, [(r"Global\NvxRunner-SetupDataDisk", 4 * 60 * 60)]
        )

    def test_unidentified_disk_stops_after_discovery(self):
        adapters = Adapters(
            {
                ("storage.ps1", "Disks"): lambda request: {
                    "disks": [nvme_disk()],
                    "guestDisks": [],
                }
            }
        )
        sleeps: list[float] = []
        with (
            patch.object(windows_system, "adapter", adapters),
            patch.object(common, "sleep", side_effect=sleeps.append),
            contextlib.redirect_stdout(io.StringIO()),
            self.assertRaisesRegex(BootstrapError, "could not uniquely identify"),
        ):
            windows_disk.setup(0, "F", "T", common.Log())
        self.assertEqual(len(sleeps), windows_disk.DISCOVERY_ATTEMPTS - 1)


def tar_xz(members: Sequence[tuple[str, bytes | str | None]]) -> bytes:
    """Build a package; bytes are a file, a string a link to it, None a directory."""
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, content in members:
            entry = tarfile.TarInfo(name)
            if isinstance(content, bytes):
                entry.size = len(content)
                archive.addfile(entry, io.BytesIO(content))
            elif isinstance(content, str):
                entry.type = tarfile.SYMTYPE
                entry.linkname = content
                archive.addfile(entry)
            else:
                entry.type = tarfile.DIRTYPE
                archive.addfile(entry)
    return lzma.compress(stream.getvalue(), format=lzma.FORMAT_XZ)


def openssh_adapters(
    protected: list[tuple[str, bytes]],
    sshd: bool = True,
    warning: str | None = None,
    legacy_task: bool = False,
) -> Adapters:
    def protect(request: JsonObject) -> object:
        path = Path(str(request["path"]))
        protected.append((path.name, path.read_bytes()))
        return True

    return Adapters(
        {
            ("openssh.ps1", "Prepare"): lambda request: {
                "capability": "unknown" if warning else "present",
                "warning": warning,
                "sshd": sshd,
                "services": [{"name": "sshd", "status": "Running"}] if sshd else [],
            },
            ("system.ps1", "ProtectAuthorizedKeys"): protect,
            ("openssh.ps1", "AllowInbound"): lambda request: {"created": True},
            ("openssh.ps1", "Restart"): lambda request: {"status": "Running"},
            ("system.ps1", "TaskExists"): lambda request: legacy_task,
            ("system.ps1", "UnregisterTask"): lambda request: True,
            ("system.ps1", "RegisterTask"): lambda request: True,
            ("system.ps1", "StartTask"): lambda request: True,
        }
    )


POWERSHELL = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"


class WindowsStageTests(unittest.TestCase):
    def setUp(self) -> None:
        data, self.files, outputs = builder.windows_payload(windows_config())
        self.payload = cast(JsonObject, json.loads(data))
        self.package = outputs["sources.tar.xz"]
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        environment = patch.dict(
            os.environ,
            {"ProgramData": str(self.directory), "SystemRoot": r"C:\Windows"},
        )
        environment.start()
        self.addCleanup(environment.stop)

    def write_package(self, data: bytes) -> tuple[Path, str]:
        path = self.directory / "package.tar.xz"
        path.write_bytes(data)
        return path, hashlib.sha256(data).hexdigest()

    def test_public_keys_are_split_deduplicated_and_uncommented(self):
        payload: JsonObject = {
            "publicKeys": [
                "ssh-ed25519 AAAA one\r\n# retired key\nssh-rsa BBBB two",
                " ssh-ed25519 AAAA one ",
                "",
            ]
        }
        self.assertEqual(
            windows_stage.public_keys(payload),
            ["ssh-ed25519 AAAA one", "ssh-rsa BBBB two"],
        )
        for keys in ("ssh-ed25519 AAAA", [1], None):
            with self.subTest(keys=keys), self.assertRaises(BootstrapError):
                windows_stage.public_keys({"publicKeys": keys})

    def test_package_digest_is_verified(self):
        package, digest = self.write_package(self.package)
        self.assertEqual(digest, cast(JsonObject, self.payload["package"])["sha256"])
        windows_stage.verify_package(package, digest)
        with self.assertRaisesRegex(BootstrapError, "checksum mismatch"):
            windows_stage.verify_package(package, "0" * 64)
        for text in ("", digest.upper(), digest[:-1], digest + "0"):
            with (
                self.subTest(digest=text),
                self.assertRaisesRegex(BootstrapError, "no valid package digest"),
            ):
                windows_stage.verify_package(package, text)

    def test_package_holds_exactly_the_expected_regular_files(self):
        regular: list[tuple[str, bytes | str | None]] = sorted(self.files.items())
        self.assertEqual(frozenset(self.files), windows_stage.PACKAGE_FILES)
        cases: dict[str, list[tuple[str, bytes | str | None]]] = {
            "extra": [*regular, ("nvx_azure/extra.py", b"")],
            "missing": [item for item in regular if item[0] != "system.ps1"],
            "duplicate": [*regular, ("config.json", b"{}")],
            "directory": [
                ("config.json", None) if name == "config.json" else (name, data)
                for name, data in regular
            ],
            "link": [
                ("system.ps1", "../outside.ps1")
                if name == "system.ps1"
                else (name, data)
                for name, data in regular
            ],
            "traversal": [
                ("../system.ps1", data) if name == "system.ps1" else (name, data)
                for name, data in regular
            ],
        }
        for case, members in cases.items():
            with self.subTest(case=case):
                package, digest = self.write_package(tar_xz(members))
                with self.assertRaisesRegex(BootstrapError, "unexpected or duplicate"):
                    windows_stage.verify_package(package, digest)

    def test_extraction_replaces_each_file(self):
        package, _ = self.write_package(self.package)
        root = self.directory / "root"
        root.mkdir()
        (root / "config.json").write_bytes(b"stale")
        windows_stage.extract_package(package, root)
        extracted = {
            path.relative_to(root).as_posix(): path.read_bytes()
            for path in root.rglob("*")
            if path.is_file()
        }
        self.assertEqual(extracted, self.files)

    def check_refused_target(self, case: str) -> None:
        package, _ = self.write_package(self.package)
        root = self.directory / case
        root.mkdir()
        outside = self.directory / "outside"
        if case == "file":
            (root / "config.json").mkdir()
            message = "is not a regular file"
        elif case == "parent":
            (root / "nvx_azure").write_bytes(b"")
            message = "is not a regular directory"
        else:
            try:
                (root / "config.json").symlink_to(outside)
            except OSError:
                self.skipTest("this account cannot create symbolic links")
            message = "is not a regular file"
        with self.assertRaisesRegex(BootstrapError, message):
            windows_stage.extract_package(package, root)
        self.assertFalse(outside.exists())
        self.assertEqual(list(root.rglob("*.tmp")), [])

    def test_extraction_refuses_non_regular_targets(self):
        for case in ("file", "parent", "link"):
            with self.subTest(case=case):
                self.check_refused_target(case)

    def test_default_shell_paths(self):
        with patch.object(shutil, "which", return_value=None):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                for shell, expected in (
                    ("powershell", POWERSHELL),
                    ("cmd", r"C:\Windows\System32\cmd.exe"),
                    ("pwsh", POWERSHELL),
                ):
                    self.assertEqual(
                        windows_stage.shell_path(shell, common.Log()), expected
                    )
                with self.assertRaisesRegex(BootstrapError, "Unsupported SSH default"):
                    windows_stage.shell_path("bash", common.Log())
            self.assertIn("WARN: pwsh.exe not found", output.getvalue())
        pwsh = r"C:\Program Files\PowerShell\7\pwsh.exe"
        with patch.object(shutil, "which", return_value=pwsh):
            self.assertEqual(windows_stage.shell_path("pwsh", common.Log()), pwsh)

    def configure(
        self, payload: JsonObject, adapters: Adapters
    ) -> tuple[Registry, str]:
        registry = Registry()
        output = io.StringIO()
        with (
            registry.patch(),
            patch.object(windows_system, "adapter", adapters),
            contextlib.redirect_stdout(output),
        ):
            windows_stage.configure_openssh(payload, self.directory, common.Log())
        return registry, output.getvalue()

    def test_openssh_uses_only_the_deployed_keys(self):
        keys = self.directory / "ssh" / "administrators_authorized_keys"
        keys.parent.mkdir()
        keys.write_bytes(b"ssh-ed25519 OLD revoked\n")
        protected: list[tuple[str, bytes]] = []
        adapters = openssh_adapters(protected)
        payload: JsonObject = {
            "defaultShell": "cmd",
            "publicKeys": ["ssh-ed25519 AAAA one", "ssh-rsa BBBB two"],
        }
        registry, _ = self.configure(payload, adapters)
        expected = b"ssh-ed25519 AAAA one\nssh-rsa BBBB two\n"
        self.assertEqual(keys.read_bytes(), expected)
        self.assertFalse(keys.with_name(keys.name + ".new").exists())
        # Both the new file and the replaced one are protected, so sshd never
        # reads a key file that others could write.
        self.assertEqual(
            protected,
            [
                ("administrators_authorized_keys.new", expected),
                ("administrators_authorized_keys", expected),
            ],
        )
        self.assertEqual(
            adapters.operations(),
            [
                "Prepare",
                "ProtectAuthorizedKeys",
                "ProtectAuthorizedKeys",
                "AllowInbound",
                "Restart",
            ],
        )
        self.assertEqual(
            registry.writes,
            [
                ("DefaultShell", r"C:\Windows\System32\cmd.exe"),
                ("DefaultShellCommandOption", "/c"),
            ],
        )

    def test_openssh_writes_keys_only_into_the_data_directory_of_sshd(self):
        # A start failure that Prepare suppresses leaves sshd's directory
        # missing; one created here would inherit ProgramData's permissions.
        payload: JsonObject = {
            "defaultShell": "cmd",
            "publicKeys": ["ssh-ed25519 AAAA one"],
        }
        outside = self.directory / "outside"
        outside.mkdir()
        for case in ("missing", "file", "link", "key-directory", "key-link"):
            with self.subTest(case=case):
                program_data = self.directory / case
                program_data.mkdir()
                ssh = program_data / "ssh"
                keys = ssh / "administrators_authorized_keys"
                message = "missing or not a regular directory"
                if case == "file":
                    ssh.write_bytes(b"")
                elif case.startswith("key-"):
                    ssh.mkdir()
                    message = "is not a regular file"
                    if case == "key-directory":
                        keys.mkdir()
                try:
                    if case == "link":
                        ssh.symlink_to(outside, target_is_directory=True)
                    elif case == "key-link":
                        keys.symlink_to(outside / "keys")
                except OSError:
                    self.skipTest("this account cannot create symbolic links")
                protected: list[tuple[str, bytes]] = []
                adapters = openssh_adapters(protected)
                with (
                    patch.dict(os.environ, {"ProgramData": str(program_data)}),
                    self.assertRaisesRegex(BootstrapError, message),
                ):
                    self.configure(payload, adapters)
                self.assertEqual(adapters.operations(), ["Prepare"])
                self.assertEqual(protected, [])
                self.assertEqual(list(outside.iterdir()), [])
                if case == "missing":
                    self.assertFalse(ssh.exists())
                else:
                    self.assertFalse(keys.with_name(keys.name + ".new").exists())

    def test_openssh_without_keys_keeps_the_existing_file(self):
        keys = self.directory / "ssh" / "administrators_authorized_keys"
        keys.parent.mkdir()
        keys.write_bytes(b"ssh-ed25519 KEEP existing\n")
        protected: list[tuple[str, bytes]] = []
        adapters = openssh_adapters(protected)
        registry, output = self.configure(
            {"defaultShell": "powershell", "publicKeys": []}, adapters
        )
        self.assertEqual(keys.read_bytes(), b"ssh-ed25519 KEEP existing\n")
        self.assertEqual(protected, [])
        self.assertEqual(adapters.operations(), ["Prepare", "AllowInbound", "Restart"])
        self.assertEqual(
            registry.writes,
            [("DefaultShell", POWERSHELL), ("DefaultShellCommandOption", "-Command")],
        )
        self.assertIn("No public keys supplied", output)

    def test_windows_missing_ssh_advice(self):
        adapters = openssh_adapters([], sshd=False, warning="Mock capability failure")
        output = io.StringIO()
        registry = Registry()
        with (
            registry.patch(),
            patch.object(windows_system, "adapter", adapters),
            contextlib.redirect_stdout(output),
            self.assertRaises(BootstrapError) as raised,
        ):
            windows_stage.configure_openssh(
                {"defaultShell": "powershell", "publicKeys": ["ssh-ed25519 AAAA"]},
                self.directory,
                common.Log(),
            )
        self.assertEqual(
            str(raised.exception),
            "OpenSSH Server is not available on this image. Use an image that "
            "supports OpenSSH Server.",
        )
        self.assertIn(
            "OpenSSH capability check failed: Mock capability", output.getvalue()
        )
        self.assertEqual(adapters.operations(), ["Prepare"])
        self.assertEqual(registry.writes, [])

    def test_hand_off_reports_staging_only_after_the_task_starts(self):
        adapters = openssh_adapters([])
        root = Path(r"C:\ProgramData\NvxBootstrap")
        python = r"C:\ProgramData\NvxBootstrap\python\python.exe"
        output = io.StringIO()
        with (
            patch.object(windows_system, "adapter", adapters),
            contextlib.redirect_stdout(output),
        ):
            windows_stage.hand_off(python, root, common.Log())
        self.assertEqual(
            adapters.calls,
            [
                ("system.ps1", "TaskExists", {"name": "NvxRunner-SetupDataDisk"}),
                (
                    "system.ps1",
                    "RegisterTask",
                    {
                        "name": "Nvx-Setup-GitHub-Runner",
                        "execute": python,
                        "arguments": r'-I -B "C:\ProgramData\NvxBootstrap" '
                        r'windows-bootstrap --config "C:\ProgramData\NvxBootstrap\config.json"',
                        "workingDirectory": r"C:\ProgramData\NvxBootstrap",
                        "executionHours": 4,
                        "restartCount": 60,
                        "restartMinutes": 2,
                    },
                ),
                ("system.ps1", "StartTask", {"name": "Nvx-Setup-GitHub-Runner"}),
            ],
        )
        self.assertEqual(output.getvalue(), "NVX_CLOUD_RUNNER_BOOTSTRAP_STAGED=1\n")

        def fail(request: JsonObject) -> object:
            raise BootstrapError("Mock task start failure")

        adapters.handlers[("system.ps1", "StartTask")] = fail
        output = io.StringIO()
        with (
            patch.object(windows_system, "adapter", adapters),
            contextlib.redirect_stdout(output),
            self.assertRaisesRegex(BootstrapError, "Mock task start failure"),
        ):
            windows_stage.hand_off(python, root, common.Log())
        self.assertNotIn("STAGED", output.getvalue())

    def test_hand_off_removes_the_legacy_disk_task(self):
        adapters = openssh_adapters([], legacy_task=True)
        output = io.StringIO()
        with (
            patch.object(windows_system, "adapter", adapters),
            contextlib.redirect_stdout(output),
        ):
            windows_stage.hand_off("python.exe", self.directory, common.Log())
        self.assertEqual(
            adapters.calls[:2],
            [
                ("system.ps1", "TaskExists", {"name": "NvxRunner-SetupDataDisk"}),
                ("system.ps1", "UnregisterTask", {"name": "NvxRunner-SetupDataDisk"}),
            ],
        )
        self.assertEqual(adapters.operations()[2:], ["RegisterTask", "StartTask"])
        self.assertIn(
            "Removed the legacy NvxRunner-SetupDataDisk task", output.getvalue()
        )

    def stage(self, package: bytes, adapters: Adapters) -> Path:
        root = self.directory / "NvxBootstrap"
        root.mkdir()
        path = root / "package.tar.xz"
        path.write_bytes(package)
        payload = self.directory / "vmss-bootstrap-config.json"
        payload.write_text(json.dumps(self.payload), encoding="utf-8")
        with (
            Registry().patch(),
            patch.object(windows_system, "adapter", adapters),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            windows_stage.stage(payload, path, root, common.Log(), "python.exe")
        return root

    def test_stage_extracts_the_verified_package_before_handing_off(self):
        adapters = openssh_adapters([])
        root = self.stage(self.package, adapters)
        for name, data in self.files.items():
            self.assertEqual(root.joinpath(*name.split("/")).read_bytes(), data)
        self.assertEqual(
            adapters.operations(),
            [
                "Prepare",
                "AllowInbound",
                "Restart",
                "TaskExists",
                "RegisterTask",
                "StartTask",
            ],
        )

    def test_stage_configures_openssh_before_persistent_extraction_failure(self):
        root = self.directory / "NvxBootstrap-failure"
        root.mkdir()
        package = root / "package.tar.xz"
        package.write_bytes(self.package)
        (root / "config.json").mkdir()
        # The data directory that sshd creates when it first starts.
        (self.directory / "ssh").mkdir()
        payload = self.directory / "vmss-bootstrap-config-failure.json"
        payload_data = {**self.payload, "publicKeys": ["ssh-ed25519 AAAA diagnostic"]}
        payload.write_text(json.dumps(payload_data), encoding="utf-8")
        protected: list[tuple[str, bytes]] = []
        adapters = openssh_adapters(protected)
        with (
            Registry().patch(),
            patch.object(windows_system, "adapter", adapters),
            contextlib.redirect_stdout(io.StringIO()),
            self.assertRaisesRegex(BootstrapError, "is not a regular file"),
        ):
            windows_stage.stage(payload, package, root, common.Log(), "python.exe")
        self.assertEqual(
            adapters.operations(),
            [
                "Prepare",
                "ProtectAuthorizedKeys",
                "ProtectAuthorizedKeys",
                "AllowInbound",
                "Restart",
            ],
        )
        self.assertEqual(len(protected), 2)

    def test_stage_refuses_a_tampered_package_before_any_change(self):
        tampered = bytearray(self.package)
        tampered[len(tampered) // 2] ^= 1
        adapters = openssh_adapters([])
        with self.assertRaisesRegex(BootstrapError, "checksum mismatch"):
            self.stage(bytes(tampered), adapters)
        self.assertEqual(adapters.calls, [])
        self.assertEqual(
            sorted(path.name for path in (self.directory / "NvxBootstrap").iterdir()),
            ["package.tar.xz"],
        )

    def test_staging_failures_are_logged_and_raised(self):
        log = self.directory / "logs" / "vmss-bootstrap.log"
        package = self.directory / "NvxBootstrap" / "package.tar.xz"
        payload = self.directory / "payload.json"
        failure = BootstrapError("Mock staging failure")
        with (
            patch.object(windows_stage, "LOG_PATH", log),
            patch.object(windows_stage, "stage", side_effect=failure) as stage,
            contextlib.redirect_stdout(io.StringIO()),
            self.assertRaisesRegex(BootstrapError, "Mock staging failure"),
        ):
            windows_stage.main(payload, package)
        self.assertEqual(stage.call_args.args[:3], (payload, package, package.parent))
        self.assertEqual(stage.call_args.args[4], sys.executable)
        self.assertIn("ERROR: Mock staging failure", log.read_text(encoding="utf-8"))


def instance_metadata() -> JsonObject:
    return {
        "name": "nvx-windows-ci-vmss_2",
        "subscriptionId": "00000000-0000-0000-0000-000000000000",
        "resourceGroupName": "nvx-windows-ci-rg",
        "vmScaleSetName": "nvx-windows-ci-vmss",
        "vmSize": "Standard_D8ds_v5",
        "tagsList": [{"name": "secret", "value": "never-in-the-snapshot"}],
        "storageProfile": {
            "osDisk": {
                "diskSizeGB": "128",
                "managedDisk": {"storageAccountType": "Premium_LRS"},
                "caching": "ReadWrite",
                "writeAcceleratorEnabled": "false",
                "diffDiskSettings": {"option": ""},
            },
            "dataDisks": [
                {
                    "lun": "0",
                    "diskSizeGB": "512",
                    "managedDisk": {"storageAccountType": "PremiumV2_LRS"},
                    "caching": "None",
                    "writeAcceleratorEnabled": "false",
                    "bytesPerSecondThrottle": "",
                    "opsPerSecondThrottle": "",
                }
            ],
            "resourceDisk": {"size": "307200"},
        },
    }


TOKEN = "mock-registration-token"
APPX = r"C:\Program Files\WindowsApps\Microsoft.VCLibs.140.00_14.0.33519.0_x64__8wekyb3d8bbwe"
# Holds a named mutex until it is killed.
MUTEX_HOLDER = """
import ctypes
import sys
import time
from ctypes import wintypes

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
kernel32.CreateMutexW.argtypes = (ctypes.c_void_p, wintypes.BOOL, wintypes.LPCWSTR)
kernel32.CreateMutexW.restype = wintypes.HANDLE
print("held" if kernel32.CreateMutexW(None, True, sys.argv[1]) else "failed", flush=True)
time.sleep(60)
"""


class Instance:
    """Model a Windows instance across runs of the runner bootstrap."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.config = as_json(windows_config())
        self.task = True
        self.boot = "boot-1"
        self.features = {
            "HypervisorPlatform": "Enabled",
            "Microsoft-Hyper-V": "Enabled",
        }
        self.disk_ready = True
        self.disk_error: BootstrapError | None = None
        self.metadata_error: OSError | None = None
        self.firewall = firewall_facts()
        self.results: list[int] = []
        self.events: list[str] = []
        self.registry = Registry()
        self.paths: list[list[str]] = []
        self.commands = Commands(self.command)

    def command(self, command: list[str], stdin: bytes | None) -> Completed:
        if command[0] == "shutdown.exe":
            self.events.append("reboot")
            return Completed(0)
        self.events.append("setup")
        return Completed(self.results.pop(0), b"setup output\n", b"setup errors")

    def inspect(self, request: JsonObject) -> object:
        self.events.append("firewall")
        return json.loads(json.dumps(self.firewall))

    def metadata(self) -> JsonObject:
        self.events.append("metadata")
        if self.metadata_error is not None:
            raise self.metadata_error
        return instance_metadata()

    def token(self, config: JsonObject) -> str:
        self.events.append("token")
        return TOKEN

    def disk(self, lun: int, letter: str, fallback: str, log: common.Log) -> bool:
        self.events.append(f"disk {lun} {letter} {fallback}")
        if self.disk_error is not None:
            raise self.disk_error
        if self.disk_ready:
            self.registry.values[(windows_disk.PENDING_KEY, "Completed")] = 1
        return self.disk_ready

    def record(self, event: str, result: object = True) -> AdapterHandler:
        def handler(request: JsonObject) -> object:
            self.events.append(event)
            return result

        return handler

    def adapters(self) -> Adapters:
        return Adapters(
            {
                ("system.ps1", "TaskExists"): lambda request: self.task,
                ("firewall.ps1", "Inspect"): self.inspect,
                ("system.ps1", "ProtectHostMetadata"): self.record("protect"),
                ("storage.ps1", "VolumeAtLetter"): lambda request: {
                    "fileSystem": "NTFS",
                    "fileSystemType": "NTFS",
                    "fileSystemLabel": "",
                },
                ("system.ps1", "BootTime"): lambda request: self.boot,
                ("system.ps1", "Features"): lambda request: [
                    {"name": name, "state": state}
                    for name, state in self.features.items()
                ],
                ("system.ps1", "EnableFeatures"): lambda request: (
                    self.events.append(
                        "enable " + ",".join(cast(list[str], request["names"]))
                    )
                    or True
                ),
                ("system.ps1", "AppxPaths"): lambda request: [APPX],
                ("system.ps1", "UnregisterTask"): self.record("unregister"),
            }
        )

    def update_path(self, paths: list[str]) -> str:
        self.paths.append(paths)
        return r"C:\Windows\system32"

    def run(self) -> str:
        config = self.root / "config.json"
        config.write_text(json.dumps(self.config), encoding="utf-8")
        output = io.StringIO()
        with (
            self.registry.patch(),
            patch.object(windows_system, "adapter", self.adapters()),
            patch.object(common, "run_command", self.commands),
            patch.object(common, "compute_metadata", self.metadata),
            patch.object(common, "registration_token", self.token),
            patch.object(windows_disk, "setup", self.disk),
            patch.object(windows_disk, "LOG_PATH", self.root / "setup-datadisk.log"),
            patch.object(windows_bootstrap, "update_machine_path", self.update_path),
            patch.object(
                windows_bootstrap, "HOST_METADATA_ROOT", self.root / "metadata"
            ),
            patch.dict(
                os.environ,
                {
                    "SystemDrive": "C:",
                    "SystemRoot": r"C:\Windows",
                    "PSModulePath": r"C:\Program Files\PowerShell\Modules",
                },
            ),
            contextlib.redirect_stdout(output),
        ):
            windows_bootstrap.bootstrap(config, common.Log(), self.root)
        return output.getvalue()


class WindowsBootstrapTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.instance = Instance(self.root)

    def setup_command(self, *arguments: str) -> list[str]:
        return [
            POWERSHELL,
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            str(windows_bootstrap.PureWindowsPath(self.root) / "setup-windows-whp.ps1"),
            "-RunnerOnly",
            "-RepositoryUrl",
            "https://github.com/microsoft/nvx",
            "-RunnerDirectory",
            r"C:\actions-runner",
            *arguments,
            "-BenchmarkScratchDirectory",
            r"F:\nvx-benchmark-scratch",
        ]

    def test_setup_output_is_truncated_and_streamed_before_exit(self):
        output = self.root / "setup.log"
        output.write_bytes(b"stale output")

        def run(*_args: object, **kwargs: object) -> Completed:
            raw_output = kwargs.get("output")
            assert raw_output is not None
            stream = cast(BinaryIO, raw_output)
            self.assertEqual(output.read_bytes(), b"")
            stream.write(b"setup started\n")
            stream.flush()
            self.assertEqual(output.read_bytes(), b"setup started\n")
            stream.write(b"setup failed\n")
            return Completed(7)

        with patch.object(common, "run_command", run):
            result = windows_bootstrap.run_setup([], output, root=self.root)
        self.assertEqual(result, 7)
        self.assertEqual(output.read_bytes(), b"setup started\nsetup failed\n")

    def test_registration_passes_the_token_on_standard_input_only(self):
        self.instance.results = [0, 0]
        output = self.instance.run()
        self.assertEqual(
            self.instance.events,
            [
                "firewall",
                "metadata",
                "protect",
                "protect",
                "protect",
                "disk 0 F T",
                "firewall",
                "setup",
                "token",
                "setup",
                "unregister",
            ],
        )
        self.assertEqual(
            self.instance.commands.calls,
            [
                self.setup_command(),
                [
                    *self.setup_command(),
                    "-RunnerName",
                    "nvx-windows-3",
                    "-RunnerTokenStdin",
                ],
            ],
        )
        self.assertEqual(self.instance.commands.inputs, [b"", (TOKEN + "\n").encode()])
        # The setup inherits the updated path, but not a PowerShell 7 module path.
        for environment in self.instance.commands.environments:
            assert environment is not None
            names = {name.upper(): value for name, value in environment.items()}
            self.assertEqual(names["PATH"], r"C:\Windows\system32")
            self.assertNotIn("PSMODULEPATH", names)
        self.assertTrue(output.endswith("NVX_CLOUD_RUNNER_SETUP_COMPLETE=1\n"))
        self.assertEqual(self.instance.paths, [[APPX]])
        self.assertEqual(
            (self.root / "setup-registration.log").read_bytes(),
            b"setup output\nsetup errors",
        )
        snapshot = (self.root / "metadata" / "compute.json").read_bytes()
        self.assertEqual(
            snapshot,
            common.snapshot_bytes(common.safe_compute_metadata(instance_metadata())),
        )
        self.assertNotIn(b"never-in-the-snapshot", snapshot)
        self.assertNotIn(b"nvx-windows-ci-vmss", snapshot)

    def test_benchmark_runner_and_extra_labels(self):
        self.instance.config["benchmarkRunnerName"] = "nvx-windows-3"
        self.instance.config["runnerLabels"] = [
            "windows",
            "whp",
            "virtual-machine",
            "general",
            "nvx-extra",
        ]
        self.instance.results = [0, 0]
        self.instance.run()
        extra = ("-ExtraRunnerLabels", "nvx-extra")
        self.assertEqual(
            self.instance.commands.calls[1],
            [
                *self.setup_command(*extra),
                "-RunnerName",
                "nvx-windows-3",
                "-RunnerTokenStdin",
                "-BenchmarkRunner",
            ],
        )

    def test_windows_labels_keep_the_platform_and_one_role(self):
        required = windows_bootstrap.REQUIRED_LABELS
        self.assertEqual(
            common.split_labels(
                ["Windows", "WHP", "Virtual-Machine", "isolated"], required
            ),
            ("isolated", False),
        )
        self.assertEqual(
            common.split_labels(
                ["windows", "whp", "virtual-machine", "benchmark"], required
            ),
            ("", True),
        )
        for labels in (
            ["windows", "kvm", "virtual-machine"],
            ["windows", "whp"],
            ["windows", "whp", "virtual-machine", "general", "benchmark"],
            ["windows", "whp", "virtual-machine", "bad label"],
        ):
            with self.subTest(labels=labels), self.assertRaises(BootstrapError):
                common.split_labels(labels, required)

    def test_metadata_is_blocked_before_it_is_read(self):
        mutate(self.instance.firewall, "principal")
        with self.assertRaisesRegex(BootstrapError, windows_firewall.FAILURE):
            self.instance.run()
        self.assertEqual(self.instance.events, ["firewall"])

    def test_metadata_failures_stop_bootstrap_after_the_block(self):
        self.instance.metadata_error = OSError("Mock metadata failure")
        with self.assertRaisesRegex(OSError, "Mock metadata failure"):
            self.instance.run()
        self.assertEqual(self.instance.events, ["firewall", "metadata"])

    def test_invalid_starts_change_nothing(self):
        for case in ("task", "os"):
            with self.subTest(case=case):
                instance = Instance(self.root)
                if case == "task":
                    instance.task = False
                    message = "continuation task is missing"
                else:
                    instance.config["os"] = "linux"
                    message = "Expected a Windows runner configuration"
                with self.assertRaisesRegex(BootstrapError, message):
                    instance.run()
                self.assertEqual(instance.events, [])

    def test_data_disk_must_not_use_the_system_drive(self):
        self.instance.config["dataDisk"] = {
            "lun": 0,
            "driveLetter": "c",
            "temporaryDriveFallback": "T",
        }
        with self.assertRaisesRegex(BootstrapError, "system drive letter"):
            self.instance.run()
        self.assertNotIn("disk 0 c T", self.instance.events)

    def test_disk_failures_are_reported_in_both_logs(self):
        self.instance.disk_error = BootstrapError("Mock disk failure")
        with self.assertRaisesRegex(
            BootstrapError, "^Managed data disk setup failed: Mock disk failure$"
        ):
            self.instance.run()
        self.assertIn(
            "ERROR: Mock disk failure",
            (self.root / "setup-datadisk.log").read_text(encoding="utf-8"),
        )

    def test_pending_disk_reboot_defers_runner_setup(self):
        self.instance.disk_ready = False
        output = self.instance.run()
        self.assertEqual(self.instance.events[-1], "disk 0 F T")
        self.assertIn("Data disk setup scheduled a reboot", output)

    def test_features_reboot_once_and_must_then_be_enabled(self):
        marker = self.root / "feature-reboot-pending.txt"
        self.instance.features = {
            "HypervisorPlatform": "Disabled",
            "Microsoft-Hyper-V": "EnablePending",
        }
        output = self.instance.run()
        self.assertEqual(
            self.instance.events[-2:], ["enable HypervisorPlatform", "reboot"]
        )
        self.assertEqual(marker.read_text(encoding="utf-8"), "boot-1")
        self.assertIn("NVX_CLOUD_RUNNER_REBOOT_SCHEDULED=1", output)
        self.instance.boot = "boot-2"
        with self.assertRaisesRegex(
            BootstrapError, "still disabled after their reboot"
        ):
            self.instance.run()
        self.instance.features = dict.fromkeys(self.instance.features, "Enabled")
        self.instance.results = [0, 0]
        self.instance.events = []
        self.instance.run()
        self.assertFalse(marker.exists())
        self.assertEqual(self.instance.events[-1], "unregister")

    def test_features_enabled_without_their_reboot_still_reboot(self):
        (self.root / "feature-reboot-pending.txt").write_text(
            "boot-1", encoding="utf-8"
        )
        self.instance.run()
        self.assertEqual(self.instance.events[-1], "reboot")
        self.assertNotIn("setup", self.instance.events)

    def test_prerequisite_reboots_are_tracked_across_boots(self):
        marker = self.root / "prerequisite-reboot-pending.txt"
        self.instance.results = [windows_bootstrap.RESTART_REQUIRED]
        self.instance.run()
        self.assertEqual(self.instance.events[-2:], ["setup", "reboot"])
        self.assertEqual(marker.read_text(encoding="utf-8"), "boot-1")
        self.instance.boot = "boot-2"
        self.instance.results = [windows_bootstrap.RESTART_REQUIRED]
        with self.assertRaisesRegex(BootstrapError, "still require a reboot"):
            self.instance.run()
        self.instance.results = [0, windows_bootstrap.RESTART_REQUIRED]
        self.instance.events = []
        self.instance.run()
        self.assertEqual(self.instance.events[-3:], ["token", "setup", "reboot"])
        self.assertEqual(marker.read_text(encoding="utf-8"), "boot-2")
        self.instance.boot = "boot-3"
        self.instance.results = [0, 0]
        self.instance.run()
        self.assertFalse(marker.exists())

    def test_setup_failures_keep_the_continuation_task(self):
        for results, message in (
            ([5], "Runner prerequisites failed: 5"),
            ([0, 7], "Runner registration failed: 7"),
        ):
            with self.subTest(results=results):
                instance = Instance(self.root)
                instance.results = results
                with self.assertRaisesRegex(BootstrapError, message):
                    instance.run()
                self.assertNotIn("unregister", instance.events)

    def test_failures_are_reported_with_a_marker(self):
        def unlocked(path: Path) -> contextlib.nullcontext[None]:
            return contextlib.nullcontext()

        output = io.StringIO()
        with (
            patch.object(windows_system, "exclusive", unlocked),
            patch.object(
                windows_bootstrap,
                "bootstrap",
                side_effect=BootstrapError("Mock bootstrap failure"),
            ),
            contextlib.redirect_stdout(output),
            self.assertRaisesRegex(BootstrapError, "Mock bootstrap failure"),
        ):
            windows_bootstrap.main(self.root / "config.json", self.root)
        self.assertIn("NVX_CLOUD_RUNNER_SETUP_FAILED=1", output.getvalue())
        log = (self.root / "setup-runner.log").read_text(encoding="utf-8")
        self.assertIn("NVX_CLOUD_RUNNER_SETUP_FAILED=1", log)
        self.assertIn("ERROR: Mock bootstrap failure", log)
        self.assertNotIn("Traceback (most recent call last):", log)

    def test_unexpected_failure_traceback_is_persisted(self):
        def unlocked(path: Path) -> contextlib.nullcontext[None]:
            return contextlib.nullcontext()

        with (
            patch.object(windows_system, "exclusive", unlocked),
            patch.object(
                windows_bootstrap,
                "bootstrap",
                side_effect=RuntimeError("Mock unexpected failure"),
            ),
            contextlib.redirect_stdout(io.StringIO()),
            self.assertRaisesRegex(RuntimeError, "Mock unexpected failure"),
        ):
            windows_bootstrap.main(self.root / "config.json", self.root)
        log = (self.root / "setup-runner.log").read_text(encoding="utf-8")
        self.assertIn("NVX_CLOUD_RUNNER_SETUP_FAILED=1", log)
        self.assertIn("Traceback (most recent call last):", log)
        self.assertIn("RuntimeError: Mock unexpected failure", log)
        self.assertIn("ERROR: Mock unexpected failure", log)

    @unittest.skipUnless(sys.platform == "win32", "uses Windows file locks")
    def test_one_bootstrap_runs_at_a_time(self):
        lock = self.root / "bootstrap.lock"
        with windows_system.exclusive(lock):
            with (
                self.assertRaisesRegex(BootstrapError, "another Windows bootstrap"),
                windows_system.exclusive(lock),
            ):
                pass
        with windows_system.exclusive(lock):
            pass


class WindowsSystemTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def check_snapshot(self, existing: bool) -> None:
        root = self.root / f"metadata-{existing}"
        root.mkdir()
        target = root / "compute.json"
        if existing:
            target.write_bytes(b"stale")
        safe = common.safe_compute_metadata(instance_metadata())
        seen: list[tuple[list[str], dict[str, bytes]]] = []

        def protect(request: JsonObject) -> object:
            contents = {path.name: path.read_bytes() for path in root.iterdir()}
            seen.append((cast(list[str], request["paths"]), contents))
            return True

        adapters = Adapters({("system.ps1", "ProtectHostMetadata"): protect})
        with patch.object(windows_system, "adapter", adapters):
            windows_bootstrap.write_host_snapshot(safe, root)
        snapshot = common.snapshot_bytes(safe)
        self.assertEqual(target.read_bytes(), snapshot)
        self.assertEqual(sorted(path.name for path in root.iterdir()), ["compute.json"])
        self.assertEqual(len(seen), 3)
        # The directory and the old snapshot are protected before anything
        # is written in the directory.
        before = {"compute.json": b"stale"} if existing else {}
        self.assertEqual(
            seen[0], ([str(root)] + ([str(target)] if existing else []), before)
        )
        [temporary] = seen[1][0]
        self.assertEqual(seen[1][1], {**before, Path(temporary).name: snapshot})
        self.assertEqual(seen[2], ([str(target)], {"compute.json": snapshot}))

    def test_host_snapshot_is_written_only_in_a_protected_directory(self):
        for existing in (True, False):
            with self.subTest(existing=existing):
                self.check_snapshot(existing)

    def test_host_snapshot_must_be_a_regular_file(self):
        root = self.root / "metadata"
        (root / "compute.json").mkdir(parents=True)
        adapters = Adapters(
            {("system.ps1", "ProtectHostMetadata"): lambda request: True}
        )
        with (
            patch.object(windows_system, "adapter", adapters),
            self.assertRaisesRegex(BootstrapError, "is not a regular file"),
        ):
            windows_bootstrap.write_host_snapshot(
                common.safe_compute_metadata(instance_metadata()), root
            )
        self.assertEqual(adapters.calls, [])

    def test_machine_path_puts_current_dependencies_first(self):
        old = r"C:\Program Files\WindowsApps\Microsoft.VCLibs.140.00_14.0.30704.0_x64__8wekyb3d8bbwe"
        xaml = r"C:\Program Files\WindowsApps\Microsoft.UI.Xaml.2.8_8.2310.30001.0_x64__8wekyb3d8bbwe"
        current = rf"%SystemRoot%\system32;{old};;C:\Tools;{xaml}"
        self.assertEqual(
            windows_bootstrap.merge_machine_path(current, [APPX, xaml]),
            rf"{APPX};{xaml};%SystemRoot%\system32;C:\Tools",
        )
        self.assertEqual(
            windows_bootstrap.merge_machine_path(r"C:\Tools", []), r"C:\Tools"
        )

    def test_adapters_run_isolated_from_profiles_with_a_json_request(self):
        # Windows PowerShell writes warnings and host messages to the same
        # stream as the marked result line.
        output = (
            b"WARNING: Mock restart warning\r\nMock host message\r\n"
            b'NVX-ADAPTER-RESULT {"value":1}\r\n'
        )
        commands = Commands(lambda command, stdin: Completed(0, output))
        powershell_7 = r"C:\Program Files\PowerShell\Modules"
        with (
            patch.object(common, "run_command", commands),
            patch.dict(
                os.environ, {"SystemRoot": r"C:\Windows", "PSModulePath": powershell_7}
            ),
        ):
            result = windows_system.adapter(
                "system.ps1", "BootTime", {"x": 1}, self.root
            )
        environment = commands.environments[0]
        assert environment is not None
        self.assertNotIn("PSMODULEPATH", {name.upper() for name in environment})
        self.assertIn(r"C:\Windows", environment.values())
        self.assertEqual(result, {"value": 1})
        self.assertEqual(
            commands.calls,
            [
                [
                    POWERSHELL,
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(self.root / "system.ps1"),
                    "-Operation",
                    "BootTime",
                ]
            ],
        )
        self.assertEqual(commands.inputs, [b'{"x": 1}'])

    def test_adapter_failures_are_reported(self):
        for result, message in (
            (
                Completed(1, b"", b"Mock cmdlet failure"),
                "system.ps1 Features failed: Mock",
            ),
            (Completed(1, b"Mock output", b""), "failed: Mock output"),
            (Completed(0, b'{"value":1}\r\n'), "returned no JSON result"),
            (Completed(0, b"NVX-ADAPTER-RESULT not json"), "returned no JSON result"),
            (
                Completed(0, b"NVX-ADAPTER-RESULT 1\r\nNVX-ADAPTER-RESULT 2\r\n"),
                "returned no JSON result",
            ),
        ):
            with self.subTest(message=message, output=result.stdout):
                commands = Commands(lambda command, stdin, result=result: result)
                with (
                    patch.object(common, "run_command", commands),
                    self.assertRaisesRegex(BootstrapError, message),
                ):
                    windows_system.adapter("system.ps1", "Features", root=self.root)

    def check_reboot(self, task: bool) -> None:
        commands = Commands(lambda command, stdin: Completed(0))
        adapters = Adapters({("system.ps1", "TaskExists"): lambda request: task})
        output = io.StringIO()
        with (
            patch.object(windows_system, "adapter", adapters),
            patch.object(common, "run_command", commands),
            contextlib.redirect_stdout(output),
        ):
            if task:
                windows_system.request_reboot("Mock reason", common.Log())
            else:
                with self.assertRaisesRegex(BootstrapError, "refusing to reboot"):
                    windows_system.request_reboot("Mock reason", common.Log())
        self.assertEqual(
            commands.calls,
            [["shutdown.exe", "/r", "/t", "60", "/c", "Mock reason"]] if task else [],
        )
        self.assertEqual(
            "NVX_CLOUD_RUNNER_REBOOT_SCHEDULED=1" in output.getvalue(), task
        )

    def test_reboots_require_the_continuation_task(self):
        for task in (False, True):
            with self.subTest(task=task):
                self.check_reboot(task)

    @unittest.skipUnless(sys.platform == "win32", "uses Windows named mutexes")
    def test_named_mutex_waits_for_its_holder(self):
        name = f"Local\\NvxTest612-{os.getpid()}"
        holder = subprocess.Popen(
            [sys.executable, "-I", "-c", MUTEX_HOLDER, name],
            stdout=subprocess.PIPE,
            text=True,
        )
        try:
            assert holder.stdout is not None
            self.assertEqual(holder.stdout.readline().strip(), "held")
            with (
                self.assertRaisesRegex(BootstrapError, "Timed out waiting"),
                windows_system.named_mutex(name, 0.2),
            ):
                pass
            # A holder that exits without releasing the mutex abandons it,
            # which counts as acquired, as in the earlier disk setup.
            killer = threading.Timer(0.5, holder.kill)
            killer.start()
            with windows_system.named_mutex(name, 30):
                pass
            killer.join()
        finally:
            holder.kill()
            holder.wait()
            if holder.stdout is not None:
                holder.stdout.close()
        with windows_system.named_mutex(name, 0.2):
            pass

    def test_cli_dispatches_the_windows_commands(self):
        with patch.object(windows_stage, "main", return_value=0) as stage:
            self.assertEqual(
                cli.main(
                    ["windows-stage", "--payload", "payload.json", "--package", "p"]
                ),
                0,
            )
        stage.assert_called_once_with(Path("payload.json"), Path("p"))
        with patch.object(windows_bootstrap, "main", return_value=0) as bootstrap:
            self.assertEqual(cli.main(["windows-bootstrap"]), 0)
        bootstrap.assert_called_once_with(cli.WINDOWS_ROOT / "config.json")
        errors = io.StringIO()
        with (
            patch.object(
                windows_bootstrap, "main", side_effect=BootstrapError("Mock failure")
            ),
            contextlib.redirect_stderr(errors),
        ):
            self.assertEqual(cli.main(["windows-bootstrap", "--config", "c.json"]), 1)
        self.assertEqual(errors.getvalue(), "ERROR: Mock failure\n")


NATIVE_PREAMBLE = r"""
Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
function Invoke-Adapter {
    param([string]$Name, [string]$Operation, $Request = @{})
    $json = ConvertTo-Json -InputObject $Request -Depth 6 -Compress
    $output = @(& (Join-Path $env:NVX_TEST_WINDOWS $Name) -Operation $Operation -Request $json)
    $prefix = 'NVX-ADAPTER-RESULT '
    if ($output.Count -ne 1 -or $output[0] -isnot [string] -or -not $output[0].StartsWith($prefix)) {
        throw "$Name $Operation did not write one result line"
    }
    return $output[0].Substring($prefix.Length)
}
function Get-Failure {
    param([scriptblock]$Block)
    try { & $Block | Out-Null }
    catch { return $_.Exception.Message }
    return ''
}
function Write-Report {
    param($Report)
    Write-Output ('NVX-REPORT ' + (ConvertTo-Json -InputObject $Report -Depth 10 -Compress))
}
"""


def native_report(
    test: unittest.TestCase, script: str, **environment: str
) -> JsonObject:
    """Run a test script in Windows PowerShell and return the report it writes."""
    executable = shutil.which("powershell.exe")
    if sys.platform != "win32" or executable is None:
        test.skipTest("Windows PowerShell is unavailable")
    with tempfile.TemporaryDirectory() as temporary:
        path = Path(temporary) / "test.ps1"
        path.write_text(NATIVE_PREAMBLE + script, encoding="utf-8-sig")
        result = run_powershell(
            [
                executable,
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(path),
            ],
            env={
                **windows_system.powershell_environment(),
                "NVX_TEST_WINDOWS": str(builder.ROOT / "windows"),
                "NVX_TEST_ROOT": temporary,
                **environment,
            },
        )
    details = result.stdout + result.stderr
    test.assertEqual(result.returncode, 0, details)
    reports = [
        line.removeprefix("NVX-REPORT ")
        for line in result.stdout.splitlines()
        if line.startswith("NVX-REPORT ")
    ]
    test.assertEqual(len(reports), 1, details)
    return common.json_object(json.loads(reports[0]), "The native test report")


def decoded(report: JsonObject, key: str) -> object:
    value = report[key]
    assert isinstance(value, str), key
    return json.loads(value)


FIREWALL_MOCKS = r"""
$global:stored = $true
$global:effective = 1
$global:remotePort = 'Any'
$global:policyAppId = $null
$global:created = @()
function Get-Service {
    [CmdletBinding()] param([string]$Name)
    [pscustomobject]@{ Name = $Name; Status = 'Running' }
}
function Get-NetFirewallProfile {
    [CmdletBinding()] param([string]$PolicyStore)
    if ($PolicyStore -ne 'ActiveStore') { throw 'Profiles must come from ActiveStore' }
    foreach ($name in 'Domain', 'Private', 'Public') {
        [pscustomobject]@{
            Name = $name; Enabled = 'True'; AllowLocalFirewallRules = 'NotConfigured'
            DisabledInterfaceAliases = @('NotConfigured')
        }
    }
}
function Get-NetFirewallRule {
    [CmdletBinding()] param([string]$Name, [string]$PolicyStore, [switch]$TracePolicyStore)
    if ($Name -ne 'NvxRunnerDenyMetadata') { throw "Unexpected rule $Name" }
    if ($PolicyStore -eq 'PersistentStore') {
        if ($global:stored) { [pscustomobject]@{ Name = $Name; Active = $false } }
        return
    }
    if ($PolicyStore -ne 'ActiveStore' -or -not $TracePolicyStore) {
        throw 'The effective rule must be traced in ActiveStore'
    }
    for ($index = 0; $index -lt $global:effective; $index++) {
        $rule = [pscustomobject]@{
            Name = $Name; Active = $true; Direction = 'Outbound'; Action = 'Block'
            Enabled = 'True'; Profile = 'Any'; Platform = @(); PrimaryStatus = 'OK'
            CimInstanceProperties = @{
                EnforcementStatus = [pscustomobject]@{ Value = [uint16[]]@(5, 1) }
            }
        }
        if ($null -ne $global:policyAppId) {
            $rule | Add-Member -NotePropertyName PolicyAppId -NotePropertyValue $global:policyAppId
        }
        $rule
    }
}
function Assert-Active {
    param($Rule)
    if (-not $Rule.Active) { throw 'A filter was read from the stored rule' }
}
function Get-NetFirewallAddressFilter {
    [CmdletBinding()] param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process {
        Assert-Active $InputObject
        [pscustomobject]@{ LocalAddress = @('Any'); RemoteAddress = @('169.254.169.254') }
    }
}
function Get-NetFirewallPortFilter {
    [CmdletBinding()] param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process {
        Assert-Active $InputObject
        [pscustomobject]@{
            Protocol = 'Any'; LocalPort = @('Any'); RemotePort = @($global:remotePort)
            IcmpType = @('Any'); DynamicTarget = 'Any'
        }
    }
}
function Get-NetFirewallApplicationFilter {
    [CmdletBinding()] param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process { Assert-Active $InputObject; [pscustomobject]@{ Program = 'Any'; Package = 'Any' } }
}
function Get-NetFirewallServiceFilter {
    [CmdletBinding()] param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process { Assert-Active $InputObject; [pscustomobject]@{ Service = 'Any' } }
}
function Get-NetFirewallInterfaceFilter {
    [CmdletBinding()] param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process { Assert-Active $InputObject; [pscustomobject]@{ InterfaceAlias = @('Any') } }
}
function Get-NetFirewallInterfaceTypeFilter {
    [CmdletBinding()] param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process { Assert-Active $InputObject; [pscustomobject]@{ InterfaceType = 'Any' } }
}
function Get-NetFirewallSecurityFilter {
    [CmdletBinding()] param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process {
        Assert-Active $InputObject
        [pscustomobject]@{
            Authentication = 'NotRequired'; Encryption = 'NotRequired'
            RemoteUser = 'Any'; RemoteMachine = 'Any'; LocalUser = 'D:(A;;CC;;;S-1-5-20)'
        }
    }
}
function New-NetFirewallRule {
    [CmdletBinding()]
    param($Name, $DisplayName, $Direction, $Action, $RemoteAddress, $LocalUser, $Profile,
        $Authentication, $Encryption)
    $global:created += ,@($Name, $DisplayName, $Direction, $Action, $RemoteAddress,
        $LocalUser, $Profile, $Authentication, $Encryption)
    [pscustomobject]@{ Name = $Name }
}
$inspect = @{ name = 'NvxRunnerDenyMetadata'; principal = 'D:(A;;CC;;;S-1-5-20)' }
$report = [ordered]@{}
$report.present = Invoke-Adapter firewall.ps1 Inspect $inspect
$global:remotePort = '443'
$report.narrowed = Invoke-Adapter firewall.ps1 Inspect $inspect
$global:remotePort = 'Any'
$global:policyAppId = 'Restricted'
$report.restricted = Invoke-Adapter firewall.ps1 Inspect $inspect
$global:policyAppId = $null
$global:effective = 2
$report.duplicate = Invoke-Adapter firewall.ps1 Inspect $inspect
$global:stored = $false
$global:effective = 0
$report.missing = Invoke-Adapter firewall.ps1 Inspect $inspect
$report.create = Invoke-Adapter firewall.ps1 Create @{
    name = 'NvxRunnerDenyMetadata'; address = '169.254.169.254'; principal = 'D:(A;;CC;;;S-1-5-20)'
}
$report.created = $global:created
$report.unsupported = Get-Failure { Invoke-Adapter firewall.ps1 Remove $inspect }
Write-Report $report
"""


class WindowsFirewallAdapterTests(unittest.TestCase):
    def test_inspection_feeds_the_python_decision(self):
        report = native_report(self, FIREWALL_MOCKS)
        present = common.json_object(decoded(report, "present"), "facts")
        windows_firewall.check_environment(present)
        windows_firewall.check_rule(present)
        rule = cast(JsonObject, cast(list[object], present["effective"])[0])
        self.assertEqual(rule["localUser"], "D:(A;;CC;;;NS)")
        self.assertEqual(present["principal"], "D:(A;;CC;;;NS)")
        self.assertEqual(rule["enforcement"], [5, 1])
        for case in ("narrowed", "restricted", "duplicate"):
            with self.subTest(case=case), self.assertRaises(BootstrapError):
                windows_firewall.check_rule(
                    common.json_object(decoded(report, case), "facts")
                )
        missing = common.json_object(decoded(report, "missing"), "facts")
        self.assertIs(missing["persistent"], False)
        self.assertEqual(missing["effective"], [])
        self.assertIs(decoded(report, "create"), True)
        self.assertEqual(
            report["created"],
            [
                [
                    "NvxRunnerDenyMetadata",
                    "NvxRunnerDenyMetadata",
                    "Outbound",
                    "Block",
                    "169.254.169.254",
                    "D:(A;;CC;;;S-1-5-20)",
                    "Any",
                    "NotRequired",
                    "NotRequired",
                ]
            ],
        )
        self.assertIn(
            "Unsupported firewall operation: Remove", str(report["unsupported"])
        )


STORAGE_MOCKS = r"""
$global:calls = [Collections.Generic.List[string]]::new()
function New-MockDisk {
    param([int]$Number, [string]$Location, [bool]$Boot = $false)
    [pscustomobject]@{
        Number = $Number; IsBoot = $Boot; IsSystem = $Boot; IsOffline = (-not $Boot)
        IsReadOnly = $false; PartitionStyle = 'RAW'; BusType = 'SCSI'
        FriendlyName = 'Microsoft Virtual Disk'; Model = 'Virtual Disk'; Location = $Location
        Size = [uint64]1099511627776
    }
}
$global:disks = @(
    New-MockDisk 0 'Integrated : Adapter 0 : Port 0 : Target 0 : LUN 0' $true
    New-MockDisk 1 'Integrated : Adapter 0 : Port 0 : Target 0 : LUN 1'
    New-MockDisk 2 'Integrated : Adapter 1 : Port 0 : Target 0 : LUN 0'
)
$global:partitions = @(
    [pscustomobject]@{
        DiskNumber = 1; PartitionNumber = 1; Type = 'Basic'; GptType = $null
        Offset = [uint64]1048576; DriveLetter = [char]'d'
    }
)
$global:volumes = @(
    [pscustomobject]@{ DiskNumber = 0; DriveLetter = [char]'C'; FileSystem = 'NTFS'; FileSystemType = 'NTFS'; FileSystemLabel = 'Windows' }
    [pscustomobject]@{ DiskNumber = 1; DriveLetter = [char]'D'; FileSystem = 'NTFS'; FileSystemType = 'NTFS'; FileSystemLabel = 'Temporary Storage' }
    [pscustomobject]@{ DiskNumber = 0; DriveLetter = [char]0; FileSystem = 'NTFS'; FileSystemType = 'NTFS'; FileSystemLabel = 'Recovery' }
)
function Update-HostStorageCache { [CmdletBinding()] param() $global:calls.Add('Update-HostStorageCache') }
function Get-Disk {
    [CmdletBinding()] param($Number)
    if (-not $PSBoundParameters.ContainsKey('Number')) { return $global:disks }
    $global:disks | Where-Object { $_.Number -eq $Number }
}
function Get-CimInstance {
    [CmdletBinding()] param([string]$ClassName)
    switch ($ClassName) {
        'Win32_DiskDrive' { [pscustomobject]@{ Index = 2; DeviceID = '\\.\PHYSICALDRIVE2'; SCSILogicalUnit = [uint16]1 } }
        'Win32_PageFileUsage' { [pscustomobject]@{ Name = 'D:\pagefile.sys' } }
        default { throw "Unexpected CIM class $ClassName" }
    }
}
function Get-Partition {
    [CmdletBinding()] param($DiskNumber, $PartitionNumber, $DriveLetter)
    $byDisk = $PSBoundParameters.ContainsKey('DiskNumber')
    $byNumber = $PSBoundParameters.ContainsKey('PartitionNumber')
    $byLetter = $PSBoundParameters.ContainsKey('DriveLetter')
    $global:partitions | Where-Object {
        (-not $byDisk -or $_.DiskNumber -eq $DiskNumber) -and
        (-not $byNumber -or $_.PartitionNumber -eq $PartitionNumber) -and
        (-not $byLetter -or [string]$_.DriveLetter -eq [string]$DriveLetter)
    }
}
function Get-Volume {
    [CmdletBinding()] param($Partition, $DriveLetter)
    if ($PSBoundParameters.ContainsKey('Partition')) {
        return $global:volumes | Where-Object { [string]$_.DriveLetter -eq [string]$Partition.DriveLetter }
    }
    if ($PSBoundParameters.ContainsKey('DriveLetter')) {
        return $global:volumes | Where-Object { [string]$_.DriveLetter -eq [string]$DriveLetter }
    }
    $global:volumes
}
function Set-Disk {
    [CmdletBinding()] param($Number, $IsOffline, $IsReadOnly)
    $change = if ($PSBoundParameters.ContainsKey('IsOffline')) { "IsOffline=$IsOffline" } else { "IsReadOnly=$IsReadOnly" }
    $global:calls.Add("Set-Disk $Number $change")
}
function Set-Partition {
    [CmdletBinding()] param($DiskNumber, $PartitionNumber, $NewDriveLetter)
    $global:calls.Add("Set-Partition $DiskNumber $PartitionNumber $NewDriveLetter")
}
function Initialize-Disk {
    [CmdletBinding()] param($Number, $PartitionStyle)
    $global:calls.Add("Initialize-Disk $Number $PartitionStyle")
    ($global:disks | Where-Object { $_.Number -eq $Number }).PartitionStyle = $PartitionStyle
}
function New-Partition {
    [CmdletBinding()] param($DiskNumber, $Size, [switch]$UseMaximumSize, $GptType)
    $extent = if ($UseMaximumSize) { 'maximum' } else { "size=$Size" }
    $global:calls.Add("New-Partition $DiskNumber $extent $GptType")
    [pscustomobject]@{
        DiskNumber = $DiskNumber; PartitionNumber = 2; Type = 'Basic'; GptType = $GptType
        Offset = [uint64]17825792; DriveLetter = [char]0
    }
}
function Format-Volume {
    [CmdletBinding(SupportsShouldProcess = $true)]
    param($Partition, $FileSystem, $NewFileSystemLabel, [switch]$Force)
    $global:calls.Add("Format-Volume $($Partition.DiskNumber) $($Partition.PartitionNumber) $FileSystem $NewFileSystemLabel $Force")
    [pscustomobject]@{ FileSystem = $FileSystem }
}
function Remove-Partition {
    [CmdletBinding(SupportsShouldProcess = $true)] param($DiskNumber, $PartitionNumber)
    $global:calls.Add("Remove-Partition $DiskNumber $PartitionNumber")
}
$report = [ordered]@{}
$report.disks = Invoke-Adapter storage.ps1 Disks
$report.disk = Invoke-Adapter storage.ps1 Disk @{ number = 2 }
$report.setDisk = Invoke-Adapter storage.ps1 SetDisk @{ number = 2; online = $true; writable = $false }
$report.noPartitions = Invoke-Adapter storage.ps1 Partitions @{ diskNumber = 2 }
$report.partitions = Invoke-Adapter storage.ps1 Partitions @{ diskNumber = 1 }
$report.atLetter = Invoke-Adapter storage.ps1 PartitionAtLetter @{ letter = 'D' }
$report.freeLetter = Invoke-Adapter storage.ps1 PartitionAtLetter @{ letter = 'F' }
$report.volume = Invoke-Adapter storage.ps1 Volume @{ diskNumber = 1; partitionNumber = 1 }
$report.volumeAtLetter = Invoke-Adapter storage.ps1 VolumeAtLetter @{ letter = 'F' }
$report.usedLetters = Invoke-Adapter storage.ps1 UsedLetters
$report.pagefiles = Invoke-Adapter storage.ps1 ActivePagefiles
$report.setLetter = Invoke-Adapter storage.ps1 SetPartitionLetter @{ diskNumber = 1; partitionNumber = 1; letter = 'T' }
$report.initialize = Invoke-Adapter storage.ps1 InitializeGpt @{ number = 2 }
$report.marker = Invoke-Adapter storage.ps1 NewPartition @{
    diskNumber = 2; size = 16777216; gptType = '{8f8f0f40-5c84-4a2a-8c22-8fbff2e8bc91}'
}
$report.data = Invoke-Adapter storage.ps1 NewPartition @{
    diskNumber = 2; gptType = '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'
}
$global:partitions += [pscustomobject]@{
    DiskNumber = 2; PartitionNumber = 2; Type = 'Basic'; GptType = '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'
    Offset = [uint64]17825792; DriveLetter = [char]'F'
}
$global:volumes += [pscustomobject]@{ DiskNumber = 2; DriveLetter = [char]'F'; FileSystem = 'NTFS'; FileSystemType = 'NTFS'; FileSystemLabel = 'data' }
$report.format = Invoke-Adapter storage.ps1 FormatNtfs @{ diskNumber = 2; partitionNumber = 2 }
$report.remove = Invoke-Adapter storage.ps1 RemovePartition @{ diskNumber = 2; partitionNumber = 1 }
$report.unsupported = Get-Failure { Invoke-Adapter storage.ps1 ClearDisk @{ number = 2 } }
$report.calls = @($global:calls)
Write-Report $report
"""


class WindowsStorageAdapterTests(unittest.TestCase):
    def test_storage_results_feed_the_python_decisions(self):
        report = native_report(self, STORAGE_MOCKS)
        inventory = common.json_object(decoded(report, "disks"), "inventory")
        disks = windows_disk.results(inventory["disks"])
        guest_disks = windows_disk.results(inventory["guestDisks"])
        self.assertEqual([disk["number"] for disk in disks], [0, 1, 2])
        self.assertEqual(disks[2]["size"], 1099511627776)
        selected = windows_disk.select_managed_disk(disks, guest_disks, 0)
        self.assertEqual(selected and selected["number"], 2)
        self.assertEqual(
            guest_disks,
            [{"index": 2, "deviceId": "\\\\.\\PHYSICALDRIVE2", "scsiLogicalUnit": 1}],
        )
        disk = common.json_object(decoded(report, "disk"), "disk")
        self.assertEqual((disk["number"], disk["isOffline"]), (2, True))
        self.assertEqual(decoded(report, "noPartitions"), [])
        self.assertEqual(
            decoded(report, "partitions"),
            [
                {
                    "diskNumber": 1,
                    "partitionNumber": 1,
                    "type": "Basic",
                    "gptType": "",
                    "offset": 1048576,
                    "driveLetter": "D",
                }
            ],
        )
        self.assertEqual(
            decoded(report, "atLetter"),
            cast(list[object], decoded(report, "partitions"))[0],
        )
        self.assertIsNone(decoded(report, "freeLetter"))
        self.assertEqual(
            decoded(report, "volume"),
            {
                "fileSystem": "NTFS",
                "fileSystemType": "NTFS",
                "fileSystemLabel": "Temporary Storage",
            },
        )
        self.assertIsNone(decoded(report, "volumeAtLetter"))
        self.assertEqual(decoded(report, "usedLetters"), ["C", "D"])
        self.assertEqual(decoded(report, "pagefiles"), ["D:\\pagefile.sys"])
        self.assertIs(decoded(report, "setLetter"), True)
        initialized = common.json_object(decoded(report, "initialize"), "disk")
        self.assertEqual(initialized["partitionStyle"], "GPT")
        marker = common.json_object(decoded(report, "marker"), "partition")
        self.assertEqual(
            (marker["gptType"], marker["driveLetter"]),
            (windows_disk.INIT_MARKER_TYPE, None),
        )
        self.assertEqual(
            decoded(report, "format"),
            {"fileSystem": "NTFS", "fileSystemType": "NTFS", "fileSystemLabel": "data"},
        )
        self.assertIs(decoded(report, "remove"), True)
        self.assertEqual(
            report["calls"],
            [
                "Update-HostStorageCache",
                "Set-Disk 2 IsOffline=False",
                "Set-Partition 1 1 T",
                "Initialize-Disk 2 GPT",
                f"New-Partition 2 size=16777216 {windows_disk.INIT_MARKER_TYPE}",
                f"New-Partition 2 maximum {windows_disk.BASIC_DATA_TYPE}",
                "Format-Volume 2 2 NTFS data True",
                "Remove-Partition 2 1",
            ],
        )
        self.assertIn(
            "Unsupported storage operation: ClearDisk", str(report["unsupported"])
        )


SYSTEM_MOCKS = r"""
$global:calls = [Collections.Generic.List[string]]::new()
$global:acls = [Collections.Generic.List[object]]::new()
$global:remaining = @()
function Get-ScheduledTask {
    [CmdletBinding()] param([string]$TaskName)
    if ($TaskName -eq 'Present') { [pscustomobject]@{ TaskName = $TaskName } }
}
function New-ScheduledTaskAction {
    [CmdletBinding()] param($Execute, $Argument, $WorkingDirectory)
    [ordered]@{ execute = $Execute; argument = $Argument; workingDirectory = $WorkingDirectory }
}
function New-ScheduledTaskPrincipal {
    [CmdletBinding()] param($UserId, $LogonType, $RunLevel)
    [ordered]@{ userId = $UserId; logonType = $LogonType; runLevel = $RunLevel }
}
function New-ScheduledTaskSettingsSet {
    [CmdletBinding()]
    param([switch]$StartWhenAvailable, $MultipleInstances, $ExecutionTimeLimit, $RestartCount,
        $RestartInterval)
    [ordered]@{
        startWhenAvailable = [bool]$StartWhenAvailable; multipleInstances = [string]$MultipleInstances
        executionTimeLimit = $ExecutionTimeLimit.ToString(); restartCount = $RestartCount
        restartInterval = $RestartInterval.ToString()
    }
}
function New-ScheduledTaskTrigger {
    [CmdletBinding()] param([switch]$AtStartup)
    [ordered]@{ atStartup = [bool]$AtStartup }
}
function Register-ScheduledTask {
    [CmdletBinding()] param($TaskName, $Action, $Trigger, $Principal, $Settings, [switch]$Force)
    $global:registered = [ordered]@{
        name = $TaskName; action = $Action; trigger = $Trigger; principal = $Principal
        settings = $Settings; force = [bool]$Force
    }
}
function Start-ScheduledTask {
    [CmdletBinding()] param($TaskName)
    $global:calls.Add("Start-ScheduledTask $TaskName")
}
function Unregister-ScheduledTask {
    [CmdletBinding(SupportsShouldProcess = $true)] param($TaskName)
    $global:calls.Add("Unregister-ScheduledTask $TaskName Confirm=$($PSBoundParameters['Confirm'])")
}
function Get-CimInstance {
    [CmdletBinding()] param($ClassName)
    if ($ClassName -ne 'Win32_OperatingSystem') { throw "Unexpected class $ClassName" }
    [pscustomobject]@{
        LastBootUpTime = [datetime]::new(2026, 10, 30, 1, 2, 3, [DateTimeKind]::Utc).ToLocalTime()
    }
}
function Get-WindowsOptionalFeature {
    [CmdletBinding()] param([switch]$Online, $FeatureName)
    if (-not $Online) { throw 'Features must be read from the running system' }
    $state = if ($FeatureName -eq 'HypervisorPlatform') { 'Enabled' } else { 'Disabled' }
    [pscustomobject]@{ FeatureName = $FeatureName; State = $state }
}
function Enable-WindowsOptionalFeature {
    [CmdletBinding()] param([switch]$Online, $FeatureName, [switch]$All, [switch]$NoRestart)
    $global:calls.Add("Enable-WindowsOptionalFeature $FeatureName Online=$Online All=$All NoRestart=$NoRestart")
}
function New-MockPackage {
    param([string]$Name, [string]$Version, [string]$Folder)
    [pscustomobject]@{
        Name = $Name; Version = $Version; InstallLocation = 'C:\Program Files\WindowsApps\' + $Folder
    }
}
function Get-AppxPackage {
    [CmdletBinding()] param([switch]$AllUsers)
    if (-not $AllUsers) { throw 'Packages must be listed for all users' }
    New-MockPackage 'Microsoft.VCLibs.140.00' '14.0.30704.0' 'Microsoft.VCLibs.140.00_14.0.30704.0_x64__8wekyb3d8bbwe'
    New-MockPackage 'Microsoft.VCLibs.140.00' '14.0.33519.0' 'Microsoft.VCLibs.140.00_14.0.33519.0_x64__8wekyb3d8bbwe'
    New-MockPackage 'Microsoft.VCLibs.140.00' '14.0.33519.0' 'Microsoft.VCLibs.140.00_14.0.33519.0_x64__8wekyb3d8bbwe'
    New-MockPackage 'Microsoft.UI.Xaml.2.8' '8.2310.30001.0' 'Microsoft.UI.Xaml.2.8_8.2310.30001.0_x64__8wekyb3d8bbwe'
    New-MockPackage 'Microsoft.WindowsStore' '22.0.0.0' 'Microsoft.WindowsStore_22.0.0.0_x64__8wekyb3d8bbwe'
}
function Set-Acl {
    [CmdletBinding()] param([string]$LiteralPath, $AclObject)
    $rules = @($AclObject.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) |
        ForEach-Object {
            [ordered]@{
                sid = $_.IdentityReference.Value; rights = [int]$_.FileSystemRights
                inheritance = [string]$_.InheritanceFlags; type = [string]$_.AccessControlType
            }
        })
    $global:acls.Add([ordered]@{
        path = $LiteralPath
        owner = $AclObject.GetOwner([Security.Principal.SecurityIdentifier]).Value
        protected = $AclObject.AreAccessRulesProtected
        rules = $rules
    })
}
function Get-Acl {
    [CmdletBinding()] param([string]$LiteralPath)
    [pscustomobject]@{
        Access = @($global:remaining | ForEach-Object {
            [pscustomobject]@{ IdentityReference = [Security.Principal.SecurityIdentifier]::new($_) }
        })
    }
}
$metadata = Join-Path $env:NVX_TEST_ROOT 'metadata'
New-Item -ItemType Directory -Path $metadata | Out-Null
$snapshot = Join-Path $metadata 'compute.json'
Set-Content -LiteralPath $snapshot -Value '{}'
$keys = Join-Path $env:NVX_TEST_ROOT 'administrators_authorized_keys'
Set-Content -LiteralPath $keys -Value 'ssh-ed25519 AAAA'
$link = Join-Path $env:NVX_TEST_ROOT 'link'
New-Item -ItemType Junction -Path $link -Target $metadata | Out-Null
$report = [ordered]@{}
$report.present = Invoke-Adapter system.ps1 TaskExists @{ name = 'Present' }
$report.absent = Invoke-Adapter system.ps1 TaskExists @{ name = 'Absent' }
$report.register = Invoke-Adapter system.ps1 RegisterTask @{
    name = 'Nvx-Setup-GitHub-Runner'; execute = 'C:\ProgramData\NvxBootstrap\python\python.exe'
    arguments = '-I -B "C:\ProgramData\NvxBootstrap" windows-bootstrap'
    workingDirectory = 'C:\ProgramData\NvxBootstrap'; executionHours = 4; restartCount = 60
    restartMinutes = 2
}
$report.registered = $global:registered
$report.start = Invoke-Adapter system.ps1 StartTask @{ name = 'Nvx-Setup-GitHub-Runner' }
$report.unregister = Invoke-Adapter system.ps1 UnregisterTask @{ name = 'Nvx-Setup-GitHub-Runner' }
$report.boot = Invoke-Adapter system.ps1 BootTime
$report.features = Invoke-Adapter system.ps1 Features @{ names = @('HypervisorPlatform', 'Microsoft-Hyper-V') }
$report.feature = Invoke-Adapter system.ps1 Features @{ names = @('Microsoft-Hyper-V') }
$report.enable = Invoke-Adapter system.ps1 EnableFeatures @{ names = @('Microsoft-Hyper-V') }
$report.appx = Invoke-Adapter system.ps1 AppxPaths
$report.protectMetadata = Invoke-Adapter system.ps1 ProtectHostMetadata @{ paths = @($metadata, $snapshot) }
$report.metadataAcls = @($global:acls)
$global:acls.Clear()
$report.link = Get-Failure { Invoke-Adapter system.ps1 ProtectHostMetadata @{ paths = @($link) } }
$report.linkAcls = $global:acls.Count
$global:remaining = @('S-1-5-18', 'S-1-5-32-544')
$report.protectKeys = Invoke-Adapter system.ps1 ProtectAuthorizedKeys @{ path = $keys }
$report.keyAcls = @($global:acls)
$global:remaining = @('S-1-5-18', 'S-1-5-32-544', 'S-1-5-32-545')
$report.untrustedKeys = Get-Failure { Invoke-Adapter system.ps1 ProtectAuthorizedKeys @{ path = $keys } }
$report.calls = @($global:calls)
$report.unsupported = Get-Failure { Invoke-Adapter system.ps1 Reboot }
Write-Report $report
"""

FULL_CONTROL = 0x1F01FF
# Rights that let a principal change a file or directory, or its security.
MODIFY_RIGHTS = 0x2 | 0x4 | 0x10 | 0x40 | 0x100 | 0x10000 | 0x40000 | 0x80000
SYSTEM_SID = "S-1-5-18"
ADMINISTRATORS_SID = "S-1-5-32-544"
NETWORK_SERVICE_SID = "S-1-5-20"


class WindowsSystemAdapterTests(unittest.TestCase):
    def check_protected(self, acl: JsonObject, readers: bool, directory: bool) -> None:
        self.assertEqual(acl["owner"], ADMINISTRATORS_SID)
        self.assertIs(acl["protected"], True)
        rules = {
            str(rule["sid"]): rule
            for rule in cast(list[JsonObject], acl["rules"])
            if rule["type"] == "Allow"
        }
        self.assertEqual(len(rules), len(cast(list[object], acl["rules"])))
        expected = {SYSTEM_SID, ADMINISTRATORS_SID} | (
            {NETWORK_SERVICE_SID} if readers else set[str]()
        )
        self.assertEqual(set(rules), expected)
        inheritance = "ContainerInherit, ObjectInherit" if directory else "None"
        for sid, rule in rules.items():
            self.assertEqual(rule["inheritance"], inheritance, sid)
            rights = cast(int, rule["rights"])
            if sid == NETWORK_SERVICE_SID:
                self.assertTrue(rights & 0x1, "Network Service cannot read")
                self.assertFalse(rights & MODIFY_RIGHTS, "Network Service can modify")
            else:
                self.assertEqual(rights, FULL_CONTROL, sid)

    def test_system_operations(self):
        report = native_report(self, SYSTEM_MOCKS)
        self.assertIs(decoded(report, "present"), True)
        self.assertIs(decoded(report, "absent"), False)
        self.assertIs(decoded(report, "register"), True)
        self.assertEqual(
            report["registered"],
            {
                "name": "Nvx-Setup-GitHub-Runner",
                "action": {
                    "execute": r"C:\ProgramData\NvxBootstrap\python\python.exe",
                    "argument": r'-I -B "C:\ProgramData\NvxBootstrap" windows-bootstrap',
                    "workingDirectory": r"C:\ProgramData\NvxBootstrap",
                },
                "trigger": {"atStartup": True},
                "principal": {
                    "userId": "SYSTEM",
                    "logonType": "ServiceAccount",
                    "runLevel": "Highest",
                },
                "settings": {
                    "startWhenAvailable": True,
                    "multipleInstances": "IgnoreNew",
                    "executionTimeLimit": "04:00:00",
                    "restartCount": 60,
                    "restartInterval": "00:02:00",
                },
                "force": True,
            },
        )
        self.assertEqual(decoded(report, "boot"), "2026-10-30T01:02:03.0000000Z")
        self.assertEqual(
            decoded(report, "features"),
            [
                {"name": "HypervisorPlatform", "state": "Enabled"},
                {"name": "Microsoft-Hyper-V", "state": "Disabled"},
            ],
        )
        self.assertEqual(
            decoded(report, "feature"),
            [{"name": "Microsoft-Hyper-V", "state": "Disabled"}],
        )
        base = "C:\\Program Files\\WindowsApps\\"
        self.assertEqual(
            decoded(report, "appx"),
            [
                base + "Microsoft.UI.Xaml.2.8_8.2310.30001.0_x64__8wekyb3d8bbwe",
                base + "Microsoft.VCLibs.140.00_14.0.33519.0_x64__8wekyb3d8bbwe",
                base + "Microsoft.VCLibs.140.00_14.0.30704.0_x64__8wekyb3d8bbwe",
            ],
        )
        self.assertEqual(
            report["calls"],
            [
                "Start-ScheduledTask Nvx-Setup-GitHub-Runner",
                "Unregister-ScheduledTask Nvx-Setup-GitHub-Runner Confirm=False",
                "Enable-WindowsOptionalFeature Microsoft-Hyper-V Online=True All=True "
                "NoRestart=True",
            ],
        )
        metadata = cast(list[JsonObject], report["metadataAcls"])
        self.assertEqual(len(metadata), 2)
        self.check_protected(metadata[0], readers=True, directory=True)
        self.check_protected(metadata[1], readers=True, directory=False)
        self.assertIn("Refusing to protect a reparse point", str(report["link"]))
        self.assertEqual(report["linkAcls"], 0)
        keys = cast(list[JsonObject], report["keyAcls"])
        self.assertEqual(len(keys), 1)
        self.check_protected(keys[0], readers=False, directory=False)
        self.assertIn("Unexpected ACEs remain", str(report["untrustedKeys"]))
        self.assertIn(
            "Unsupported system operation: Reboot", str(report["unsupported"])
        )


OPENSSH_MOCKS = r"""
$global:calls = [Collections.Generic.List[string]]::new()
$global:capability = 'NotPresent'
$global:services = @{ sshd = 'Stopped'; 'ssh-agent' = 'Stopped' }
$global:rule = $false
function Get-WindowsCapability {
    [CmdletBinding()] param([switch]$Online, $Name)
    if ($global:capability -eq 'Unavailable') { throw 'Mock capability failure' }
    [pscustomobject]@{ Name = 'OpenSSH.Server~~~~0.0.1.0'; State = $global:capability }
}
function Add-WindowsCapability {
    [CmdletBinding()] param([switch]$Online, $Name)
    $global:calls.Add("Add-WindowsCapability $Name Online=$Online")
}
function Get-Service {
    [CmdletBinding()] param($Name)
    if ($global:services.ContainsKey($Name)) {
        [pscustomobject]@{ Name = $Name; Status = $global:services[$Name] }
    }
}
function Set-Service {
    [CmdletBinding()] param($Name, $StartupType)
    $global:calls.Add("Set-Service $Name $StartupType")
}
function Start-Service {
    [CmdletBinding()] param($Name)
    $global:calls.Add("Start-Service $Name")
    $global:services[$Name] = 'Running'
}
function Restart-Service {
    [CmdletBinding()] param($Name, [switch]$Force)
    $global:calls.Add("Restart-Service $Name Force=$Force")
}
function Get-NetFirewallRule {
    [CmdletBinding()] param($Name)
    if ($global:rule) { [pscustomobject]@{ Name = $Name } }
}
function New-NetFirewallRule {
    [CmdletBinding()] param($Name, $DisplayName, $Enabled, $Direction, $Protocol, $Action, $LocalPort)
    $global:calls.Add("New-NetFirewallRule $Name|$DisplayName|$Enabled|$Direction|$Protocol|$Action|$LocalPort")
    $global:rule = $true
}
function Enable-NetFirewallRule {
    [CmdletBinding()] param($Name)
    $global:calls.Add("Enable-NetFirewallRule $Name")
}
$report = [ordered]@{}
$report.added = Invoke-Adapter openssh.ps1 Prepare
$global:capability = 'Unavailable'
$report.unknown = Invoke-Adapter openssh.ps1 Prepare
$global:capability = 'Installed'
$global:services = @{}
$report.missing = Invoke-Adapter openssh.ps1 Prepare
$report.create = Invoke-Adapter openssh.ps1 AllowInbound
$report.enable = Invoke-Adapter openssh.ps1 AllowInbound
$global:services = @{ sshd = 'Running' }
$report.restart = Invoke-Adapter openssh.ps1 Restart
$report.calls = @($global:calls)
$report.unsupported = Get-Failure { Invoke-Adapter openssh.ps1 Stop }
Write-Report $report
"""


class WindowsOpenSshAdapterTests(unittest.TestCase):
    def test_openssh_operations(self):
        report = native_report(self, OPENSSH_MOCKS)
        running = [
            {"name": "sshd", "status": "Running"},
            {"name": "ssh-agent", "status": "Running"},
        ]
        self.assertEqual(
            decoded(report, "added"),
            {"capability": "added", "warning": None, "sshd": True, "services": running},
        )
        self.assertEqual(
            decoded(report, "unknown"),
            {
                "capability": "unknown",
                "warning": "Mock capability failure",
                "sshd": True,
                "services": running,
            },
        )
        self.assertEqual(
            decoded(report, "missing"),
            {"capability": "present", "warning": None, "sshd": False, "services": []},
        )
        self.assertEqual(decoded(report, "create"), {"created": True})
        self.assertEqual(decoded(report, "enable"), {"created": False})
        self.assertEqual(decoded(report, "restart"), {"status": "Running"})
        services = [
            "Set-Service sshd Automatic",
            "Start-Service sshd",
            "Set-Service ssh-agent Automatic",
            "Start-Service ssh-agent",
        ]
        self.assertEqual(
            report["calls"],
            [
                "Add-WindowsCapability OpenSSH.Server~~~~0.0.1.0 Online=True",
                *services,
                *services,
                "New-NetFirewallRule OpenSSH-Server-In-TCP|OpenSSH Server (sshd)|True|"
                "Inbound|TCP|Allow|22",
                "Enable-NetFirewallRule OpenSSH-Server-In-TCP",
                "Restart-Service sshd Force=True",
            ],
        )
        self.assertIn("Unsupported OpenSSH operation: Stop", str(report["unsupported"]))


class WindowsAdapterProcessTests(unittest.TestCase):
    """Run real adapters, read-only, the way the Python controllers do."""

    def setUp(self) -> None:
        if sys.platform != "win32" or shutil.which("powershell.exe") is None:
            self.skipTest("Windows PowerShell is unavailable")
        self.root = builder.ROOT / "windows"
        command = patch.object(common, "run_command", run_adapter_powershell)
        command.start()
        self.addCleanup(command.stop)

    def test_requests_and_results_cross_the_process_boundary(self):
        missing = f"Nvx-Test-Missing-\u00c4-{os.getpid()}"
        self.assertIs(
            windows_system.adapter(
                "system.ps1", "TaskExists", {"name": missing}, self.root
            ),
            False,
        )
        boot = windows_system.adapter("system.ps1", "BootTime", root=self.root)
        self.assertRegex(str(boot), r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{7}Z$")
        with self.assertRaisesRegex(
            BootstrapError, "Unsupported system operation: Reboot"
        ):
            windows_system.adapter("system.ps1", "Reboot", root=self.root)

    def test_warnings_and_host_messages_leave_the_result_intact(self):
        source = (self.root / "system.ps1").read_text(encoding="utf-8")
        anchor = "switch ($Operation) {"
        self.assertEqual(source.count(anchor), 1)
        noise = "Write-Warning 'Mock restart warning'\nWrite-Host 'Mock host message'\n"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "system.ps1").write_text(
                source.replace(anchor, noise + anchor), encoding="utf-8"
            )
            raw = run_powershell(
                [
                    windows_system.powershell(),
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(root / "system.ps1"),
                    "-Operation",
                    "BootTime",
                ],
                input="{}",
                env=windows_system.powershell_environment(),
            )
            # Both share standard output with the result.
            self.assertIn("Mock restart warning", raw.stdout)
            self.assertIn("Mock host message", raw.stdout)
            boot = windows_system.adapter("system.ps1", "BootTime", root=root)
        self.assertRegex(str(boot), r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{7}Z$")


SHIM_LOADER = r"""
$source = [IO.File]::ReadAllText((Join-Path $env:NVX_TEST_WINDOWS 'vmss-bootstrap.ps1'))
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw 'The entry shim has parse errors' }
$statements = @($ast.EndBlock.Statements)
foreach ($definition in $statements) {
    if ($definition -is [Management.Automation.Language.FunctionDefinitionAst]) {
        . ([scriptblock]::Create($definition.Extent.Text))
    }
}
$main = @($statements | Where-Object { $_ -is [Management.Automation.Language.TryStatementAst] })
if ($main.Count -ne 1) { throw 'The entry shim has no single main block' }
$main = [scriptblock]::Create($main[0].Extent.Text)
$root = Join-Path $env:NVX_TEST_ROOT 'NvxBootstrap'
$logPath = Join-Path $env:NVX_TEST_ROOT 'logs\vmss-bootstrap.log'
function Get-Names {
    param([string]$Path)
    @(Get-ChildItem -LiteralPath $Path -Force | ForEach-Object { $_.Name } | Sort-Object)
}
"""

SHIM_TRUST = r"""
# The shim creates the directory with its final security; without elevation a
# test cannot make Administrators own a directory, so this stands in for it and
# records the security it would apply. Entries of a protected directory stand
# for SYSTEM's unless they are listed as another account's, and a raced
# creation keeps the security of the parent, as if another account had won.
$global:created = [Collections.Generic.List[object]]::new()
$global:protected = [Collections.Generic.List[string]]::new()
$global:untrusted = [Collections.Generic.List[string]]::new()
$global:raced = $false
function New-ProtectedDirectory {
    param([string]$Path)
    New-Item -ItemType Directory -Path $Path | Out-Null
    $security = New-TrustedSecurity
    $global:created.Add([ordered]@{
        path = $Path
        trusted = Test-TrustedSecurity $security
        sddl = $security.GetSecurityDescriptorSddlForm('Owner, Access')
    })
    if (-not $global:raced) { $global:protected.Add($Path) }
}
function Get-Acl {
    [CmdletBinding()] param([string]$LiteralPath)
    if ($global:untrusted -contains $LiteralPath) {
        $security = [Security.AccessControl.FileSecurity]::new()
        $security.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-545'))
        return $security
    }
    foreach ($directory in $global:protected) {
        if ($LiteralPath -eq $directory -or $LiteralPath.StartsWith($directory + '\')) {
            return New-TrustedSecurity
        }
    }
    Microsoft.PowerShell.Security\Get-Acl -LiteralPath $LiteralPath
}
function Get-Aside {
    param([string]$Name)
    @(Get-ChildItem -LiteralPath $env:NVX_TEST_ROOT -Directory -Filter "$Name.legacy-*")
}
$report = [ordered]@{}
Set-TrustedDirectory $root
$report.created = @($global:created)
$report.createdEntries = @(Get-Names $root)
$global:created.Clear()
Set-TrustedDirectory $root
$report.reused = $global:created.Count
# A protected directory without the mark, as earlier installers left it,
# holding an entry that another account may have planted.
$legacy = Join-Path $env:NVX_TEST_ROOT 'Legacy'
New-Item -ItemType Directory -Path (Join-Path $legacy 'python') | Out-Null
Set-Content -LiteralPath (Join-Path $legacy 'python\python.exe') -Value 'planted'
$global:protected.Add($legacy)
Set-TrustedDirectory $legacy
$report.legacyAside = @(Get-Aside 'Legacy' | ForEach-Object { @(Get-Names $_.FullName) })
$report.legacyEntries = @(Get-Names $legacy)
# A mark that another account planted does not count.
$planted = Join-Path $env:NVX_TEST_ROOT 'Planted'
New-Item -ItemType Directory -Path $planted | Out-Null
Set-Content -LiteralPath (Join-Path $planted 'created-protected') -Value 'planted'
$global:protected.Add($planted)
$global:untrusted.Add((Join-Path $planted 'created-protected'))
Set-TrustedDirectory $planted
$report.plantedAside = @(Get-Aside 'Planted').Count
$existing = Join-Path $env:NVX_TEST_ROOT 'Existing'
New-Item -ItemType Directory -Path $existing | Out-Null
$report.untrusted = Get-Failure { Set-TrustedDirectory $existing }
$report.untrustedAside = @(Get-Aside 'Existing').Count
$global:raced = $true
$racedPath = Join-Path $env:NVX_TEST_ROOT 'raced'
$report.raced = Get-Failure { Set-TrustedDirectory $racedPath }
$global:raced = $false
$report.racedRetry = Get-Failure { Set-TrustedDirectory $racedPath }
$report.racedEntries = @(Get-Names $racedPath)
$target = Join-Path $env:NVX_TEST_ROOT 'target'
New-Item -ItemType Directory -Path $target | Out-Null
$junction = Join-Path $env:NVX_TEST_ROOT 'junction'
New-Item -ItemType Junction -Path $junction -Target $target | Out-Null
$report.junction = Get-Failure { Set-TrustedDirectory $junction }
$file = Join-Path $env:NVX_TEST_ROOT 'file'
Set-Content -LiteralPath $file -Value 'x'
$report.file = Get-Failure { Set-TrustedDirectory $file }
$report.untrustedSecurity = Test-TrustedSecurity (Microsoft.PowerShell.Security\Get-Acl -LiteralPath $target)
$bytes = [Text.Encoding]::ASCII.GetBytes('package')
$digest = Get-Sha256 $bytes
$package = [pscustomobject]@{ data = [Convert]::ToBase64String($bytes); sha256 = $digest }
$path = Join-Path $root 'package.tar.xz'
[IO.File]::WriteAllText($path, 'previous')
$report.saved = Save-Package $package
$report.content = [IO.File]::ReadAllText($path)
$package.sha256 = '0' * 64
$report.mismatch = Get-Failure { Save-Package $package }
$package.sha256 = $digest.ToUpperInvariant()
$report.uppercase = Get-Failure { Save-Package $package }
$report.kept = [IO.File]::ReadAllText($path)
Remove-Item -LiteralPath $path
New-Item -ItemType Directory -Path $path | Out-Null
$package.sha256 = $digest
$report.directory = Get-Failure { Save-Package $package }
$report.entries = @(Get-Names $root)
$report.root = $root
Write-Report $report
"""

SHIM_PYTHON = r"""
Add-Type -AssemblyName System.IO.Compression.FileSystem
$global:downloads = 0
$global:failures = 0
$global:sleeps = [Collections.Generic.List[int]]::new()
$global:archive = $null
function New-PythonArchive {
    param([string]$Version, [string[]]$Extra = @())
    $directory = Join-Path $env:NVX_TEST_ROOT ([guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $directory | Out-Null
    [IO.File]::WriteAllText((Join-Path $directory 'python.exe'), "Python $Version")
    foreach ($name in $Extra) { [IO.File]::WriteAllText((Join-Path $directory $name), $name) }
    $path = "$directory.zip"
    [IO.Compression.ZipFile]::CreateFromDirectory($directory, $path)
    Remove-Item -LiteralPath $directory -Recurse -Force
    [pscustomobject]@{
        path = $path; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}
function New-Pin {
    param($Archive)
    $global:archive = $Archive.path
    [pscustomobject]@{
        version = '3.14.10'; sha256 = $Archive.sha256
        url = 'https://www.python.org/ftp/python/3.14.10/python-3.14.10-embeddable-amd64.zip'
    }
}
function Invoke-WebRequest {
    [CmdletBinding()] param([switch]$UseBasicParsing, $Uri, $OutFile)
    $global:downloads++
    if (-not $UseBasicParsing -or $Uri -notlike 'https://www.python.org/ftp/python/*') {
        throw 'Unexpected download'
    }
    if ($global:failures -gt 0) {
        $global:failures--
        throw 'Mock network failure'
    }
    Copy-Item -LiteralPath $global:archive -Destination $OutFile
}
function Start-Sleep {
    [CmdletBinding()] param([int]$Seconds)
    $global:sleeps.Add($Seconds)
}
# The archives hold a text file that stands in for the interpreter.
function Test-Python {
    param([string]$Executable, [string]$Version)
    (Test-Path -LiteralPath $Executable -PathType Leaf) -and
        [IO.File]::ReadAllText($Executable) -eq "Python $Version"
}
New-Item -ItemType Directory -Path $root | Out-Null
$python = Join-Path $root 'python'
function Get-State {
    $recorded = Join-Path $python 'nvx-archive.sha256'
    [ordered]@{
        downloads = $global:downloads
        files = @(if (Test-Path -LiteralPath $python) { Get-Names $python })
        recorded = $(if (Test-Path -LiteralPath $recorded) { [IO.File]::ReadAllText($recorded) })
        entries = @(Get-Names $root)
    }
}
$report = [ordered]@{}
$first = New-PythonArchive '3.14.10' @('python314._pth')
$report.firstDigest = $first.sha256
$pin = New-Pin $first
$report.installed = Install-Python $pin
$report.fresh = Get-State
$report.reused = Install-Python $pin
$report.reuse = Get-State
[IO.File]::WriteAllText((Join-Path $python 'python.exe'), 'Python 3.14.9')
$null = Install-Python $pin
$report.repaired = Get-State
$second = New-PythonArchive '3.14.10' @('python314._pth', 'extra.txt')
$report.secondDigest = $second.sha256
[IO.File]::WriteAllText((Join-Path $python 'stale.txt'), 'stale')
$pin = New-Pin $second
$null = Install-Python $pin
$report.updated = Get-State
$pin.sha256 = '0' * 64
$report.mismatch = Get-Failure { Install-Python $pin }
$report.afterMismatch = Get-State
$wrong = New-PythonArchive '3.14.9'
$report.wrongVersion = Get-Failure { Install-Python (New-Pin $wrong) }
$report.afterWrongVersion = Get-State
$global:failures = 2
$null = Install-Python (New-Pin (New-PythonArchive '3.14.10' @('third.txt')))
$report.retried = [ordered]@{ sleeps = @($global:sleeps); state = Get-State }
$global:sleeps.Clear()
$global:failures = 5
$report.exhausted = Get-Failure { Install-Python (New-Pin (New-PythonArchive '3.14.10' @('fourth.txt'))) }
$report.afterExhausted = [ordered]@{ sleeps = @($global:sleeps); state = Get-State }
$global:failures = 0
$before = $global:downloads
$report.invalid = @(
    @{ version = '3.14'; url = 'https://www.python.org/ftp/python/x.zip'; sha256 = $first.sha256 }
    @{ version = '3.14.10'; url = 'https://www.python.org/ftp/python/x.zip'; sha256 = $first.sha256.ToUpperInvariant() }
    @{ version = '3.14.10'; url = 'http://www.python.org/ftp/python/x.zip'; sha256 = $first.sha256 }
    @{ version = '3.14.10'; url = 'https://example.com/ftp/python/x.zip'; sha256 = $first.sha256 }
) | ForEach-Object {
    $invalid = [pscustomobject]$_
    Get-Failure { Install-Python $invalid }
}
$report.invalidDownloads = $global:downloads - $before
Remove-Item -LiteralPath $python -Recurse -Force
$elsewhere = Join-Path $env:NVX_TEST_ROOT 'elsewhere'
New-Item -ItemType Directory -Path $elsewhere | Out-Null
[IO.File]::WriteAllText((Join-Path $elsewhere 'keep.txt'), 'keep')
New-Item -ItemType Junction -Path $python -Target $elsewhere | Out-Null
$report.junction = Get-Failure { Install-Python (New-Pin $first) }
$report.kept = @(Get-Names $elsewhere)
Write-Report $report
"""

SHIM_RECOVERY = r"""
$env:ProgramData = $env:NVX_TEST_ROOT
$global:calls = [Collections.Generic.List[string]]::new()
function Get-WindowsCapability {
    [CmdletBinding()] param([switch]$Online, [string]$Name)
    [pscustomobject]@{ Name = 'OpenSSH.Server~~~~0.0.1.0'; State = 'NotPresent' }
}
function Add-WindowsCapability {
    [CmdletBinding()] param([switch]$Online, [string]$Name)
    $global:calls.Add("Add $Name")
}
function Get-Service { [CmdletBinding()] param([string]$Name) [pscustomobject]@{ Name = $Name } }
function Set-Service { [CmdletBinding()] param([string]$Name, [string]$StartupType) $global:calls.Add("Set $Name $StartupType") }
function Start-Service {
    [CmdletBinding()] param([string]$Name)
    $global:calls.Add("Start $Name")
    New-Item -ItemType Directory -Path (Join-Path $env:ProgramData 'ssh') -Force | Out-Null
}
function Get-NetFirewallRule { [CmdletBinding()] param([string]$Name) $null }
function New-NetFirewallRule {
    [CmdletBinding()] param($Name, $DisplayName, $Enabled, $Direction, $Protocol, $Action, $LocalPort)
    $global:calls.Add("Firewall $Name $Enabled $Direction $Protocol $Action $LocalPort")
}
function Enable-NetFirewallRule { [CmdletBinding()] param([string]$Name) throw 'unexpected existing rule' }
function Set-Acl { [CmdletBinding()] param([string]$LiteralPath, $AclObject) $global:calls.Add('Protect keys') }
$payload = [pscustomobject]@{
    publicKeys = @('ssh-ed25519 AAAA one', "# retired`nssh-rsa BBBB two", 'ssh-ed25519 AAAA one')
}
Initialize-RecoverySsh $payload
$target = Join-Path $env:ProgramData 'ssh\administrators_authorized_keys'
$source = [IO.File]::ReadAllText((Join-Path $env:NVX_TEST_WINDOWS 'vmss-bootstrap.ps1'))
$report = [ordered]@{
    calls = @($global:calls)
    keys = [IO.File]::ReadAllText($target)
    recoveryBeforeTrust = $source.IndexOf('Initialize-RecoverySsh $payload') -lt $source.IndexOf('Set-TrustedDirectory $root')
    recoveryBeforePython = $source.IndexOf('Initialize-RecoverySsh $payload') -lt $source.IndexOf('$python = Install-Python')
}
Write-Report $report
"""

SHIM_MAIN = r"""
function Initialize-RecoverySsh { param($Payload) }
function Install-Python { param($Pin) $env:NVX_TEST_PYTHON }
function New-ProtectedDirectory { param([string]$Path) New-Item -ItemType Directory -Path $Path | Out-Null }
function Get-Acl { [CmdletBinding()] param([string]$LiteralPath) New-TrustedSecurity }
$report = [ordered]@{}
foreach ($case in 'success', 'failure', 'unsafe', 'mismatch') {
    $ConfigPath = Join-Path $env:NVX_TEST_INPUT "$case.json"
    $report[$case] = Get-Failure { & $main }
    $report["$case-entries"] = @(Get-Names $root)
}
$report.runs = [IO.File]::ReadAllText((Join-Path $root 'stager-runs.json'))
$report.log = [IO.File]::ReadAllText($logPath)
$report.root = $root
Write-Report $report
"""

SHIM_PROTECTED = r"""
$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
$report = [ordered]@{
    elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}
if ($report.elevated) {
    $path = Join-Path $env:NVX_TEST_ROOT 'protected'
    Set-TrustedDirectory $path
    $acl = Get-Acl -LiteralPath $path
    $report.trusted = Test-TrustedSecurity $acl
    $report.owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
    $report.protected = $acl.AreAccessRulesProtected
    $report.rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) |
        ForEach-Object {
            '{0} {1} {2} inherited={3}' -f $_.IdentityReference.Value, $_.FileSystemRights,
                $_.InheritanceFlags, $_.IsInherited
        })
    Set-TrustedDirectory $path
    $report.reused = $true
    $report.mark = Test-TrustedMark (Join-Path $path 'created-protected')
    $report.legacyBefore = @(Get-Names $env:NVX_TEST_ROOT | Where-Object { $_ -like '*legacy*' })
    # A directory protected only after its creation, as earlier installers did.
    $legacy = Join-Path $env:NVX_TEST_ROOT 'legacy'
    New-Item -ItemType Directory -Path $legacy | Out-Null
    Set-Acl -LiteralPath $legacy -AclObject (New-TrustedSecurity)
    Set-TrustedDirectory $legacy
    $report.migrated = @(Get-Names $env:NVX_TEST_ROOT)
    $report.legacyMark = Test-TrustedMark (Join-Path $legacy 'created-protected')
}
Write-Report $report
"""

# A stand-in for the stager that records its arguments and exits with the
# status that its package names.
FAKE_STAGER = b"""import json
import sys
from pathlib import Path

here = Path(__file__).resolve().parent
runs = here.parent / "stager-runs.json"
recorded = json.loads(runs.read_text()) if runs.exists() else []
recorded.append(sys.argv[1:])
runs.write_text(json.dumps(recorded))
sys.exit(int((here / "exit-code").read_text()))
"""


class WindowsEntryShimTests(unittest.TestCase):
    def test_extension_command_unpacks_and_runs_the_compressed_shim(self):
        executable = shutil.which("powershell.exe")
        if sys.platform != "win32" or executable is None:
            self.skipTest("Windows PowerShell is unavailable")
        prefix = 'powershell.exe -ExecutionPolicy Bypass -NoProfile -Command "'
        self.assertTrue(builder.WINDOWS_EXTENSION_COMMAND.startswith(prefix))
        self.assertTrue(builder.WINDOWS_EXTENSION_COMMAND.endswith('"'))
        script = (
            b"param([string]$ConfigPath)\n"
            b"[IO.File]::WriteAllText($env:NVX_TEST_RESULT, "
            b"[IO.File]::ReadAllText($ConfigPath))\n"
        )
        payload = {"script": base64.b64encode(builder.gzip_bytes(script)).decode()}
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "CustomData.bin").write_text(json.dumps(payload), encoding="utf-8")
            result_path = root / "result.json"
            command = builder.WINDOWS_EXTENSION_COMMAND[len(prefix) : -1].replace(
                r"C:\AzureData", str(root).replace("'", "''")
            )
            result = run_powershell(
                [
                    executable,
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    command,
                ],
                env={**os.environ, "NVX_TEST_RESULT": str(result_path)},
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(
                json.loads(result_path.read_text(encoding="utf-8")), payload
            )
            self.assertEqual((root / "vmss-bootstrap.ps1").read_bytes(), script)

    def test_ssh_recovery_is_configured_before_python_download(self):
        report = native_report(self, SHIM_LOADER + SHIM_RECOVERY)
        self.assertEqual(
            report["calls"],
            [
                "Add OpenSSH.Server~~~~0.0.1.0",
                "Set sshd Automatic",
                "Start sshd",
                "Protect keys",
                "Firewall OpenSSH-Server-In-TCP True Inbound TCP Allow 22",
            ],
        )
        self.assertEqual(report["keys"], "ssh-ed25519 AAAA one\nssh-rsa BBBB two\n")
        self.assertIs(report["recoveryBeforeTrust"], True)
        self.assertIs(report["recoveryBeforePython"], True)

    def test_bootstrap_directory_must_be_protected(self):
        report = native_report(self, SHIM_LOADER + SHIM_TRUST)
        root = str(report["root"])
        created = cast(list[JsonObject], report["created"])
        self.assertEqual(len(created), 1)
        self.assertEqual(created[0]["path"], root)
        self.assertIs(created[0]["trusted"], True)
        self.assertEqual(created[0]["sddl"], "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
        self.assertEqual(report["createdEntries"], ["created-protected"])
        self.assertEqual(report["reused"], 0)
        # A protected directory without a trusted mark, as an earlier installer
        # left it, is moved aside whole, and a new one takes its place.
        self.assertEqual(report["legacyAside"], ["python"])
        self.assertEqual(report["legacyEntries"], ["created-protected"])
        self.assertEqual(report["plantedAside"], 1)
        # A directory that another account created, before this run or by
        # winning the race to create it, is refused now and on every retry.
        for case in ("untrusted", "raced", "racedRetry"):
            with self.subTest(case=case):
                self.assertIn("Bootstrap directory is not protected", str(report[case]))
        self.assertEqual(report["untrustedAside"], 0)
        self.assertEqual(report["racedEntries"], [])
        self.assertIn("not a regular directory", str(report["junction"]))
        self.assertIn("not a regular directory", str(report["file"]))
        self.assertIs(report["untrustedSecurity"], False)
        self.assertEqual(report["saved"], root + "\\package.tar.xz")
        self.assertEqual(report["content"], "package")
        self.assertEqual(report["mismatch"], "Runner package checksum mismatch.")
        self.assertEqual(report["uppercase"], "Runner package checksum mismatch.")
        self.assertEqual(report["kept"], "package")
        self.assertIn("not a regular file", str(report["directory"]))
        self.assertEqual(report["entries"], ["created-protected", "package.tar.xz"])

    def test_bootstrap_directory_is_created_protected(self):
        report = native_report(self, SHIM_LOADER + SHIM_PROTECTED)
        if not report["elevated"]:
            self.skipTest("only an elevated account can give Administrators ownership")
        self.assertIs(report["trusted"], True)
        self.assertEqual(report["owner"], ADMINISTRATORS_SID)
        self.assertIs(report["protected"], True)
        inheritance = "FullControl ContainerInherit, ObjectInherit inherited=False"
        self.assertEqual(
            sorted(cast(list[str], report["rules"])),
            [f"{SYSTEM_SID} {inheritance}", f"{ADMINISTRATORS_SID} {inheritance}"],
        )
        self.assertIs(report["reused"], True)
        self.assertIs(report["mark"], True)
        self.assertEqual(report["legacyBefore"], [])
        migrated = cast(list[str], report["migrated"])
        self.assertIn("legacy", migrated)
        self.assertEqual(
            len([n for n in migrated if n.startswith("legacy.legacy-")]), 1
        )
        self.assertIs(report["legacyMark"], True)

    def test_python_is_installed_only_from_the_pinned_archive(self):
        report = native_report(self, SHIM_LOADER + SHIM_PYTHON)
        first, second = report["firstDigest"], report["secondDigest"]
        state = cast(dict[str, JsonObject], report)
        self.assertTrue(
            str(report["installed"]).endswith("\\NvxBootstrap\\python\\python.exe")
        )
        self.assertEqual(
            state["fresh"],
            {
                "downloads": 1,
                "files": ["nvx-archive.sha256", "python.exe", "python314._pth"],
                "recorded": first,
                "entries": ["python"],
            },
        )
        self.assertEqual(report["reused"], report["installed"])
        self.assertEqual(state["reuse"]["downloads"], 1)
        self.assertEqual(
            (state["repaired"]["downloads"], state["repaired"]["recorded"]), (2, first)
        )
        self.assertEqual(
            state["updated"],
            {
                "downloads": 3,
                "files": [
                    "extra.txt",
                    "nvx-archive.sha256",
                    "python.exe",
                    "python314._pth",
                ],
                "recorded": second,
                "entries": ["python"],
            },
        )
        self.assertIn("Python archive checksum mismatch", str(report["mismatch"]))
        self.assertIn("does not provide Python 3.14.10", str(report["wrongVersion"]))
        for key, downloads in (("afterMismatch", 4), ("afterWrongVersion", 5)):
            with self.subTest(case=key):
                self.assertEqual(
                    state[key],
                    {**state["updated"], "downloads": downloads},
                )
        retried = cast(dict[str, JsonObject], report["retried"])
        self.assertEqual(retried["sleeps"], [15, 30])
        self.assertEqual(retried["state"]["downloads"], 8)
        self.assertIn("third.txt", cast(list[str], retried["state"]["files"]))
        self.assertIn("Mock network failure", str(report["exhausted"]))
        exhausted = cast(dict[str, JsonObject], report["afterExhausted"])
        self.assertEqual(exhausted["sleeps"], [15, 30, 45, 60])
        self.assertEqual(exhausted["state"], {**retried["state"], "downloads": 13})
        self.assertEqual(
            report["invalid"], ["The payload has no valid Python pin."] * 4
        )
        self.assertEqual(report["invalidDownloads"], 0)
        self.assertIn("not a regular directory", str(report["junction"]))
        self.assertEqual(report["kept"], ["keep.txt"])

    def test_shim_unpacks_and_stages_only_a_verified_package(self):
        with tempfile.TemporaryDirectory() as temporary:
            inputs = Path(temporary)
            packages = {
                "success": tar_xz([("__main__.py", FAKE_STAGER), ("exit-code", b"0")]),
                "failure": tar_xz([("__main__.py", FAKE_STAGER), ("exit-code", b"3")]),
                "unsafe": tar_xz(
                    [
                        ("__main__.py", FAKE_STAGER),
                        ("exit-code", b"0"),
                        ("../escape.py", b""),
                    ]
                ),
                "mismatch": tar_xz([("__main__.py", FAKE_STAGER), ("exit-code", b"0")]),
            }
            for case, package in packages.items():
                digest = hashlib.sha256(package).hexdigest()
                payload = {
                    "python": builder.WINDOWS_PYTHON,
                    "package": {
                        "data": base64.b64encode(package).decode(),
                        "sha256": "0" * 64 if case == "mismatch" else digest,
                    },
                }
                (inputs / f"{case}.json").write_text(
                    json.dumps(payload), encoding="utf-8"
                )
            report = native_report(
                self,
                SHIM_LOADER + SHIM_MAIN,
                NVX_TEST_INPUT=temporary,
                NVX_TEST_PYTHON=sys.executable,
            )
        root = str(report["root"])
        staged = ["created-protected", "package.tar.xz", "stager-runs.json"]
        self.assertEqual(report["success"], "")
        self.assertEqual(report["failure"], "Windows staging failed with exit code 3.")
        self.assertIn("Could not unpack the runner package", str(report["unsafe"]))
        self.assertEqual(report["mismatch"], "Runner package checksum mismatch.")
        for case in packages:
            with self.subTest(case=case):
                self.assertEqual(report[f"{case}-entries"], staged)
        self.assertEqual(
            json.loads(str(report["runs"])),
            [
                [
                    "windows-stage",
                    "--payload",
                    str(Path(temporary) / f"{case}.json"),
                    "--package",
                    root + "\\package.tar.xz",
                ]
                for case in ("success", "failure")
            ],
        )
        log = str(report["log"])
        for line in (
            "=== vmss bootstrap starting ===",
            "=== vmss bootstrap completed successfully ===",
            "ERROR: Windows staging failed with exit code 3.",
            "ERROR: Runner package checksum mismatch.",
        ):
            self.assertIn(line, log)


if __name__ == "__main__":
    unittest.main()

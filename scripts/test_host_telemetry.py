#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

"""Tests for benchmark host provenance and storage telemetry."""

import argparse
import contextlib
import datetime
import http.client
import http.server
import io
import json
import os
import platform
import socket
import struct
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, cast
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import host_telemetry, performance, storage_events  # noqa: E402

MIB = 1024 * 1024
GIB = 1024 * MIB
TICKS_PER_SECOND = 10_000_000


def word(text: bytes) -> int:
    """Return the CPUID register that holds ``text``, padded with NULs."""
    return int.from_bytes(text.ljust(4, b"\0"), "little")


CpuidLeaves = dict[int, tuple[int, int, int, int]]
"""The EAX, EBX, ECX, and EDX that CPUID returns for each leaf."""


SUBSCRIPTION = "00000000-1111-2222-3333-444444444444"
RESOURCE_GROUP = f"/subscriptions/{SUBSCRIPTION}/resourceGroups/nvx-runners-rg"
IMDS_COMPUTE: dict[str, object] = {
    "azEnvironment": "AzurePublicCloud",
    "location": "westus2",
    "name": "runner-vm-name",
    "osProfile": {
        "adminUsername": "runneradmin",
        "computerName": "VMSSVAOAN000002",
    },
    "publicKeys": [{"keyData": "ssh-rsa AAAA", "path": "/home/runneradmin"}],
    "resourceGroupName": "nvx-runners-rg",
    "resourceId": f"{RESOURCE_GROUP}/providers/Microsoft.Compute/virtualMachines/x",
    "subscriptionId": SUBSCRIPTION,
    "tags": "owner:someone",
    "tagsList": [{"name": "owner", "value": "someone"}],
    "userData": "c2VjcmV0",
    "vmId": "55555555-6666-7777-8888-999999999999",
    "vmScaleSetName": "nvx-windows",
    "vmSize": "Standard_D16ds_v5",
    "storageProfile": {
        "dataDisks": [
            {
                "bytesPerSecondThrottle": "",
                "caching": "None",
                "createOption": "Empty",
                "diskCapacityBytes": "",
                "diskSizeGB": "512",
                "isSharedDisk": "false",
                "isUltraDisk": "false",
                "lun": "0",
                "managedDisk": {
                    "id": f"{RESOURCE_GROUP}/providers/Microsoft.Compute/disks/data",
                    "storageAccountType": "Premium_LRS",
                },
                "name": "data-disk-name",
                "opsPerSecondThrottle": "",
                "vhd": {"uri": ""},
                "writeAcceleratorEnabled": "false",
            },
            {
                "bytesPerSecondThrottle": "979202048",
                "caching": "None",
                "diskSizeGB": "1024",
                "isUltraDisk": "true",
                "lun": "1",
                "managedDisk": {
                    "id": f"{RESOURCE_GROUP}/providers/Microsoft.Compute/disks/ultra",
                    "storageAccountType": "UltraSSD_LRS",
                },
                "name": "ultra-disk-name",
                "opsPerSecondThrottle": "65280",
                "writeAcceleratorEnabled": "false",
            },
        ],
        "imageReference": {"offer": "WindowsServer", "sku": "2022-datacenter"},
        "osDisk": {
            "caching": "ReadWrite",
            "createOption": "FromImage",
            "diffDiskSettings": {"option": ""},
            "diskSizeGB": "128",
            "encryptionSettings": {"enabled": "false"},
            "managedDisk": {
                "id": f"{RESOURCE_GROUP}/providers/Microsoft.Compute/disks/os",
                "storageAccountType": "Premium_LRS",
            },
            "name": "os-disk-name",
            "osType": "Windows",
            "writeAcceleratorEnabled": "false",
        },
        "resourceDisk": {"size": "614400"},
    },
}


class AzureProvenanceTests(unittest.TestCase):
    def setUp(self) -> None:
        host_telemetry._azure_vm_provenance.cache_clear()
        self.addCleanup(host_telemetry._azure_vm_provenance.cache_clear)
        snapshots = tempfile.TemporaryDirectory()
        self.addCleanup(snapshots.cleanup)
        for name in (
            "WINDOWS_AZURE_METADATA_SNAPSHOT",
            "LINUX_AZURE_METADATA_SNAPSHOT",
        ):
            snapshot_patch = patch.object(
                host_telemetry, name, Path(snapshots.name) / name / "compute.json"
            )
            snapshot_patch.start()
            self.addCleanup(snapshot_patch.stop)

    def test_keeps_only_vm_size_and_disk_performance_settings(self):
        provenance = host_telemetry.azure_compute_provenance(IMDS_COMPUTE)

        self.assertEqual(
            provenance,
            {
                "vm_size": "Standard_D16ds_v5",
                "os_disk": {
                    "storage_account_type": "Premium_LRS",
                    "size_gib": 128,
                    "caching": "ReadWrite",
                    "write_accelerator_enabled": False,
                    "ephemeral_option": None,
                },
                "data_disks": [
                    {
                        "lun": 0,
                        "storage_account_type": "Premium_LRS",
                        "size_gib": 512,
                        "caching": "None",
                        "write_accelerator_enabled": False,
                        "ephemeral_option": None,
                        "bytes_per_second_throttle": None,
                        "ops_per_second_throttle": None,
                    },
                    {
                        "lun": 1,
                        "storage_account_type": "UltraSSD_LRS",
                        "size_gib": 1024,
                        "caching": "None",
                        "write_accelerator_enabled": False,
                        "ephemeral_option": None,
                        "bytes_per_second_throttle": 979202048,
                        "ops_per_second_throttle": 65280,
                    },
                ],
                "resource_disk_size_kib": 614400,
            },
        )
        serialized = json.dumps(provenance)
        for sensitive in (
            SUBSCRIPTION,
            "nvx-runners-rg",
            "disk-name",
            "runneradmin",
            "ssh-rsa",
            "someone",
            "55555555",
            "westus2",
            "nvx-windows",
            "c2VjcmV0",
            "VMSSVAOAN000002",
            "WindowsServer",
        ):
            self.assertNotIn(sensitive, serialized)

    def test_rejects_metadata_without_a_vm_size(self):
        cases: tuple[object, ...] = ([], {}, {"vmSize": ""})
        for compute in cases:
            with (
                self.subTest(compute=compute),
                self.assertRaisesRegex(ValueError, "no vmSize"),
            ):
                host_telemetry.azure_compute_provenance(compute)

    def test_provisioned_runner_uses_only_the_trusted_snapshot(self):
        safe: dict[str, object] = {
            "vmSize": IMDS_COMPUTE["vmSize"],
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
                        "managedDisk": {"storageAccountType": "Premium_LRS"},
                        "caching": "None",
                        "writeAcceleratorEnabled": "false",
                        "bytesPerSecondThrottle": "",
                        "opsPerSecondThrottle": "",
                    }
                ],
                "resourceDisk": {"size": "614400"},
            },
        }
        for platform_name, constant in (
            ("win32", "WINDOWS_AZURE_METADATA_SNAPSHOT"),
            ("linux", "LINUX_AZURE_METADATA_SNAPSHOT"),
        ):
            with self.subTest(platform=platform_name):
                self.check_trusted_snapshot(platform_name, constant, safe)

    def check_trusted_snapshot(
        self, platform_name: str, constant: str, safe: dict[str, object]
    ) -> None:
        host_telemetry._azure_vm_provenance.cache_clear()
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "compute.json"
            snapshot.write_text(json.dumps(safe), encoding="utf-8")
            with (
                patch.object(host_telemetry.sys, "platform", platform_name),
                patch.object(host_telemetry, "may_be_azure_vm", return_value=True),
                patch.object(host_telemetry, constant, snapshot),
                patch.object(host_telemetry, "read_azure_compute_metadata") as read,
            ):
                expected = host_telemetry.azure_compute_provenance(safe)
                self.assertEqual(host_telemetry.azure_vm_provenance(), expected)
                read.assert_not_called()

                snapshot.unlink()
                host_telemetry._azure_vm_provenance.cache_clear()
                missing = host_telemetry.azure_vm_provenance()
                assert missing is not None
                self.assertIn("snapshot is invalid", str(missing["error"]))
                read.assert_not_called()

                for content in (
                    '{"vmSize": ""}',
                    '{"vmSize": "Standard_D16ds_v5", "storageProfile": {}}',
                    "x" * 65537,
                ):
                    with self.subTest(content=content[:20]):
                        snapshot.write_text(content, encoding="utf-8")
                        host_telemetry._azure_vm_provenance.cache_clear()
                        invalid = host_telemetry.azure_vm_provenance()
                        assert invalid is not None
                        self.assertIn("snapshot is invalid", str(invalid["error"]))
                        read.assert_not_called()

    def test_queries_instance_metadata_once_and_only_on_a_possible_azure_vm(self):
        with (
            patch.object(host_telemetry, "may_be_azure_vm", return_value=False),
            patch.object(host_telemetry, "read_azure_compute_metadata") as read,
        ):
            self.assertIsNone(host_telemetry.azure_vm_provenance())
        read.assert_not_called()

        host_telemetry._azure_vm_provenance.cache_clear()
        with (
            patch.object(host_telemetry, "may_be_azure_vm", return_value=True),
            patch.object(
                host_telemetry, "read_azure_compute_metadata", return_value=IMDS_COMPUTE
            ) as read,
        ):
            first = host_telemetry.azure_vm_provenance()
            assert first is not None
            first["vm_size"] = "mutated by a caller"
            second = host_telemetry.azure_vm_provenance()
        read.assert_called_once_with()
        assert second is not None
        self.assertEqual(second["vm_size"], "Standard_D16ds_v5")

        host_telemetry._azure_vm_provenance.cache_clear()
        with (
            patch.object(host_telemetry, "may_be_azure_vm", return_value=True),
            patch.object(
                host_telemetry,
                "read_azure_compute_metadata",
                side_effect=TimeoutError("timed out"),
            ) as read,
        ):
            self.assertEqual(
                host_telemetry.azure_vm_provenance(),
                {"error": "Azure instance metadata is unavailable: timed out"},
            )
        self.assertEqual(read.call_count, host_telemetry.AZURE_IMDS_ATTEMPTS)

        # Unavailable metadata fails the benchmark host check, so a transient
        # timeout is retried, but malformed metadata is not.
        host_telemetry._azure_vm_provenance.cache_clear()
        with (
            patch.object(host_telemetry, "may_be_azure_vm", return_value=True),
            patch.object(
                host_telemetry,
                "read_azure_compute_metadata",
                side_effect=[TimeoutError("timed out"), IMDS_COMPUTE],
            ),
        ):
            provenance = host_telemetry.azure_vm_provenance()
        assert provenance is not None
        self.assertEqual(provenance["vm_size"], "Standard_D16ds_v5")
        host_telemetry._azure_vm_provenance.cache_clear()
        with (
            patch.object(host_telemetry, "may_be_azure_vm", return_value=True),
            patch.object(
                host_telemetry, "read_azure_compute_metadata", return_value={}
            ) as read,
        ):
            provenance = host_telemetry.azure_vm_provenance()
        assert provenance is not None
        self.assertIn("no vmSize", str(provenance["error"]))
        read.assert_called_once_with()

        # Another service on the link-local address must not fail the run.
        host_telemetry._azure_vm_provenance.cache_clear()
        with (
            patch.object(host_telemetry, "may_be_azure_vm", return_value=True),
            patch.object(
                host_telemetry,
                "read_azure_compute_metadata",
                side_effect=http.client.BadStatusLine("SSH-2.0"),
            ),
        ):
            provenance = host_telemetry.azure_vm_provenance()
        assert provenance is not None
        self.assertIn("SSH-2.0", str(provenance["error"]))

    def test_metadata_request_bypasses_proxies(self):
        class Response:
            def __enter__(self) -> "Response":
                return self

            def __exit__(self, *_: object) -> None:
                return None

            def read(self, _size: int) -> bytes:
                return b'{"vmSize": "Standard_D4s_v5"}'

            payload = b'{"vmSize": "Standard_D4s_v5"}'

            def read1(self, _size: int) -> bytes:
                chunk, self.payload = self.payload, b""
                return chunk

        opener = MagicMock()
        opener.open.return_value = Response()
        with patch.object(
            host_telemetry.urllib.request, "build_opener", return_value=opener
        ) as build:
            document = host_telemetry.read_azure_compute_metadata(timeout=1.5)

        handler = build.call_args.args[0]
        self.assertIsInstance(handler, urllib.request.ProxyHandler)
        self.assertEqual(vars(handler)["proxies"], {})
        request = cast(urllib.request.Request, opener.open.call_args.args[0])
        self.assertEqual(request.full_url, host_telemetry.AZURE_IMDS_URL)
        self.assertIn("api-version=2021-05-01", request.full_url)
        self.assertEqual(request.get_header("Metadata"), "true")
        self.assertEqual(opener.open.call_args.kwargs, {"timeout": 1.5})
        self.assertEqual(document, {"vmSize": "Standard_D4s_v5"})

    def test_metadata_read_gives_up_at_the_deadline_on_a_trickling_response(self):
        body = b'{"vmSize": "Standard_D4s_v5", "pad": "' + b"x" * 400 + b'"}'

        class TrickleResponse:
            # Each piece arrives within the socket timeout, so only an
            # end-to-end deadline can stop the read.
            def __enter__(self) -> "TrickleResponse":
                return self

            def __exit__(self, *_: object) -> None:
                return None

            def read(self, _size: int) -> bytes:
                time.sleep(0.02)
                return body

            def read1(self, _size: int) -> bytes:
                time.sleep(0.02)
                return body[:1]

        opener = MagicMock()
        opener.open.return_value = TrickleResponse()
        started = time.monotonic()
        with (
            patch.object(
                host_telemetry.urllib.request, "build_opener", return_value=opener
            ),
            self.assertRaises(TimeoutError),
        ):
            host_telemetry.read_azure_compute_metadata(timeout=0.1)
        self.assertLess(time.monotonic() - started, 1.0)

    def test_metadata_open_gives_up_at_the_deadline_when_headers_trickle(self):
        class Response:
            def __enter__(self) -> "Response":
                return self

            def __exit__(self, *_: object) -> None:
                return None

            def read1(self, _size: int) -> bytes:
                return b""

        def trickle_headers(*_args: object, **_kwargs: object) -> Response:
            # The status line and headers are read inside opener.open(), so a
            # deadline checked only while reading the body cannot stop them.
            time.sleep(1.0)
            return Response()

        opener = MagicMock()
        opener.open.side_effect = trickle_headers
        started = time.monotonic()
        with (
            patch.object(
                host_telemetry.urllib.request, "build_opener", return_value=opener
            ),
            self.assertRaises(TimeoutError),
        ):
            host_telemetry.read_azure_compute_metadata(timeout=0.1)
        self.assertLess(time.monotonic() - started, 0.5)

    def test_metadata_request_is_aborted_at_the_deadline(self):
        server = socket.create_server(("127.0.0.1", 0))
        self.addCleanup(server.close)
        aborted = threading.Event()

        def trickle_headers() -> None:
            # Each header line arrives within the socket timeout, and no blank
            # line ends the header block, so the client stays in opener.open().
            connection, _ = server.accept()
            with connection:
                connection.sendall(b"HTTP/1.1 200 OK\r\n")
                try:
                    for _ in range(100):
                        connection.sendall(b"X-Pad: 0123456789\r\n")
                        time.sleep(0.05)
                except OSError:
                    aborted.set()

        threading.Thread(target=trickle_headers, daemon=True).start()
        url = f"http://127.0.0.1:{server.getsockname()[1]}/metadata"
        # Workers left running by earlier tests are not this request's worker.
        earlier = set(threading.enumerate())
        started = time.monotonic()
        with (
            patch.object(host_telemetry, "AZURE_IMDS_URL", url),
            self.assertRaises(TimeoutError),
        ):
            host_telemetry.read_azure_compute_metadata(timeout=0.2)
        self.assertLess(time.monotonic() - started, 1.0)
        self.assertTrue(aborted.wait(2), "the metadata connection was left open")
        workers = [
            t
            for t in threading.enumerate()
            if t.name == "azure-imds" and t not in earlier and t.is_alive()
        ]
        self.assertEqual(
            workers,
            [],
            "the metadata worker is still running after the request was aborted",
        )


class HostProvenanceTests(unittest.TestCase):
    def test_records_runner_cpu_memory_and_volume_roles(self):
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.dict(os.environ, {"RUNNER_NAME": "azure-windows-3"}),
            patch.object(
                host_telemetry,
                "azure_vm_provenance",
                return_value={"vm_size": "Standard_D16ds_v5"},
            ),
        ):
            scratch = Path(temporary).resolve()
            provenance = host_telemetry.host_provenance(
                {"workspace": Path(__file__).resolve().parent, "scratch": scratch}
            )

        json.dumps(provenance)
        self.assertEqual(provenance["schema_version"], 1)
        self.assertEqual(provenance["runner_name"], "azure-windows-3")
        self.assertEqual(provenance["logical_processors"], os.cpu_count())
        self.assertEqual(provenance["azure"], {"vm_size": "Standard_D16ds_v5"})
        self.assertEqual(
            provenance["hypervisor"], host_telemetry.hypervisor_provenance()
        )
        self.assertEqual(
            set(cast(dict[str, object], provenance["cpu"])),
            {"vendor", "family", "model", "stepping", "microcode"},
        )
        version = cast(dict[str, object], provenance["os_version"])
        self.assertEqual(set(version), {"system", "release", "revision"})
        if sys.platform == "win32":
            self.assertEqual(version["release"], platform.version())
            self.assertIsInstance(version["revision"], int)
        else:
            self.assertEqual(version["release"], platform.release())
            self.assertIsNone(version["revision"])
        memory = provenance["memory_bytes"]
        self.assertIsInstance(memory, int)
        self.assertGreater(cast(int, memory), 0)
        volumes = cast(dict[str, dict[str, object]], provenance["volumes"])
        self.assertEqual(set(volumes), {"workspace", "scratch"})
        self.assertEqual(volumes["scratch"]["path"], str(scratch))
        self.assertNotIn("error", volumes["scratch"])
        self.assertIn("volume", volumes["scratch"])
        self.assertGreater(cast(int, volumes["scratch"]["size_bytes"]), 0)

    def test_reads_the_windows_cpu_signature_and_microcode(self):
        processor = "Intel64 Family 6 Model 106 Stepping 6, GenuineIntel"
        # The registry keeps the revision in the high doubleword.
        revision = bytes.fromhex("00000000ffffffff")

        self.assertEqual(
            host_telemetry._windows_cpu_signature(processor, revision),
            {
                "vendor": "GenuineIntel",
                "family": 6,
                "model": 106,
                "stepping": 6,
                "microcode": "0xffffffff",
            },
        )
        self.assertEqual(
            host_telemetry._windows_cpu_signature(
                "AMD64 Family 25 Model 17 Stepping 1, AuthenticAMD",
                bytes.fromhex("01121b0a"),
            )["microcode"],
            "0xa1b1201",
        )
        self.assertEqual(
            host_telemetry._windows_cpu_signature("", None),
            dict.fromkeys(("vendor", "family", "model", "stepping", "microcode")),
        )

    def test_reads_the_linux_cpu_signature_and_microcode(self):
        cpuinfo = "\n".join(
            (
                "processor\t: 0",
                "vendor_id\t: GenuineIntel",
                "cpu family\t: 6",
                "model\t\t: 207",
                "model name\t: INTEL(R) XEON(R) PLATINUM 8573C",
                "stepping\t: 2",
                "microcode\t: 0xffffffff",
                "",
                "processor\t: 1",
                "vendor_id\t: AuthenticAMD",
                "",
            )
        )
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "cpuinfo"
            path.write_text(cpuinfo, encoding="utf-8")
            with (
                patch.object(host_telemetry, "_CPUINFO", path),
                patch.object(host_telemetry.sys, "platform", "linux"),
            ):
                signature = host_telemetry.cpu_signature()
                model = host_telemetry.cpu_model()

        self.assertEqual(
            signature,
            {
                "vendor": "GenuineIntel",
                "family": 6,
                "model": 207,
                "stepping": 2,
                "microcode": "0xffffffff",
            },
        )
        self.assertEqual(model, "INTEL(R) XEON(R) PLATINUM 8573C")
        self.assertEqual(
            host_telemetry._linux_cpu_signature({"vendor_id": "GenuineIntel"}),
            {
                "vendor": "GenuineIntel",
                "family": None,
                "model": None,
                "stepping": None,
                "microcode": None,
            },
        )

    def test_os_version_records_the_kernel_release_or_windows_update_revision(self):
        def registry(key: str, name: str) -> object:
            self.assertEqual(
                (key, name),
                (r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "UBR"),
            )
            return 33438

        with (
            patch.object(host_telemetry.sys, "platform", "win32"),
            patch.object(host_telemetry.platform, "version", return_value="10.0.26100"),
            patch.object(
                host_telemetry, "_windows_registry_value", side_effect=registry
            ),
        ):
            self.assertEqual(
                host_telemetry.os_version(),
                {"system": "Windows", "release": "10.0.26100", "revision": 33438},
            )
        with (
            patch.object(host_telemetry.sys, "platform", "win32"),
            patch.object(host_telemetry.platform, "version", return_value="10.0.26100"),
            patch.object(
                host_telemetry,
                "_windows_registry_value",
                side_effect=FileNotFoundError("UBR"),
            ),
        ):
            self.assertIsNone(host_telemetry.os_version()["revision"])
        with (
            patch.object(host_telemetry.sys, "platform", "linux"),
            patch.object(host_telemetry.platform, "system", return_value="Linux"),
            patch.object(
                host_telemetry.platform, "release", return_value="6.6.157.1-1.azl3"
            ),
        ):
            self.assertEqual(
                host_telemetry.os_version(),
                {"system": "Linux", "release": "6.6.157.1-1.azl3", "revision": None},
            )

    def test_identifies_the_hypervisor_from_cpuid(self):
        def host(
            leaves: CpuidLeaves, hypervisor: bool = True
        ) -> tuple[host_telemetry.Cpuid, list[int]]:
            # CPUID.1:ECX[31] reports a hypervisor.
            table = {1: (0x000C06F2, 0, (1 << 31) if hypervisor else 0, 0), **leaves}
            calls: list[int] = []

            def cpuid(leaf: int, subleaf: int) -> tuple[int, int, int, int]:
                self.assertEqual(subleaf, 0)
                calls.append(leaf)
                return table.get(leaf, (0, 0, 0, 0))

            return cpuid, calls

        microsoft = (0x4000000C, word(b"Micr"), word(b"osof"), word(b"t Hv"))
        hv1 = (word(b"Hv#1"), 0, 0, 0)
        cases: tuple[tuple[str, CpuidLeaves, dict[str, object]], ...] = (
            # Leaf 0x40000002 as azure-mshv-4 and azure-mshv-scus-1 report it
            # (#980): build 26100, version 10.0, service pack 9, service 9444.
            (
                "MSHV root partition",
                {
                    0x40000000: microsoft,
                    0x40000001: hv1,
                    0x40000002: (0x65F4, 0x000A0000, 9, 0x24E4),
                },
                {"vendor": "Microsoft Hv", "build": "10.0.26100.9444"},
            ),
            # prometheus30's older hypervisor, of the same build number.
            (
                "older service",
                {
                    0x40000000: microsoft,
                    0x40000001: hv1,
                    0x40000002: (0x65F4, 0x000A0000, 30, 0x7530),
                },
                {"vendor": "Microsoft Hv", "build": "10.0.26100.30000"},
            ),
            # The service branch, EDX[31:24], is not part of the version.
            (
                "service branch",
                {
                    0x40000000: microsoft,
                    0x40000001: hv1,
                    0x40000002: (26100, 0x000A0000, 0, 0x01001234),
                },
                {"vendor": "Microsoft Hv", "build": "10.0.26100.4660"},
            ),
            # Only the Microsoft hypervisor interface defines leaf 0x40000002.
            (
                "another interface",
                {
                    0x40000000: (0x40000001, word(b"KVMK"), word(b"VMKV"), word(b"M")),
                    0x40000001: (0x01007AFB, 0, 0, 0),
                    0x40000002: (1, 2, 3, 4),
                },
                {"vendor": "KVMKVMKVM", "build": None},
            ),
            (
                "no version leaf",
                {0x40000000: (0x40000001, *microsoft[1:]), 0x40000001: hv1},
                {"vendor": "Microsoft Hv", "build": None},
            ),
        )
        for name, leaves, expected in cases:
            with self.subTest(name):
                cpuid, _ = host(leaves)
                self.assertEqual(host_telemetry.identify_hypervisor(cpuid), expected)
        cpuid, calls = host({0x40000000: microsoft}, hypervisor=False)
        self.assertEqual(
            host_telemetry.identify_hypervisor(cpuid), {"vendor": None, "build": None}
        )
        # Without a hypervisor, the hypervisor leaves hold no hypervisor data.
        self.assertEqual(calls, [1])

    def test_reads_the_hypervisor_only_on_x86_64_linux(self):
        cpuid_error = OSError(13, "mprotect failed: Permission denied")
        for system, machine, error, expected in (
            ("win32", "AMD64", None, None),
            ("linux", "aarch64", None, None),
            (
                "linux",
                "x86_64",
                cpuid_error,
                {"error": f"CPUID is unavailable: {cpuid_error}"},
            ),
        ):
            with (
                self.subTest(system=system, machine=machine),
                patch.object(host_telemetry.sys, "platform", system),
                patch.object(host_telemetry.platform, "machine", return_value=machine),
                patch.object(
                    host_telemetry, "_linux_cpuid", side_effect=error
                ) as linux_cpuid,
            ):
                self.assertEqual(host_telemetry.hypervisor_provenance(), expected)
                self.assertEqual(linux_cpuid.called, error is not None)

    @unittest.skipUnless(
        sys.platform == "linux" and platform.machine() == "x86_64",
        "Python runs CPUID only on x86-64 Linux",
    )
    def test_runs_cpuid_on_this_host(self):
        with host_telemetry._linux_cpuid() as cpuid:
            _, ebx, ecx, edx = cpuid(0, 0)
            hypervisor = host_telemetry.identify_hypervisor(cpuid)

        # The vendor of leaf 0 is in EBX, EDX, and ECX.
        self.assertEqual(
            host_telemetry._register_text(ebx, edx, ecx),
            host_telemetry.cpu_signature()["vendor"],
        )
        self.assertEqual(host_telemetry.hypervisor_provenance(), hypervisor)
        if hypervisor["vendor"] == "Microsoft Hv":
            self.assertRegex(str(hypervisor["build"]), r"^\d+\.\d+\.\d+\.\d+$")

    def test_volume_errors_are_recorded_instead_of_raised(self):
        reader = (
            "_windows_volume_provenance"
            if sys.platform == "win32"
            else "_linux_volume_provenance"
        )
        with patch.object(
            host_telemetry, reader, side_effect=PermissionError("access denied")
        ):
            self.assertEqual(
                host_telemetry.volume_provenance(Path("scratch")),
                {"path": "scratch", "error": "access denied"},
            )

    def test_linux_mount_selects_the_deepest_mount_of_the_device(self):
        mountinfo = "\n".join(
            (
                "22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw",
                "30 22 8:17 / /mnt/data rw,relatime shared:2 - xfs /dev/sdb1 rw",
                "31 30 8:17 /nested /mnt/data/my\\040scratch rw shared:3 - xfs "
                "/dev/sdb1 rw",
                "40 22 0:45 / /tmp rw,nosuid - tmpfs tmpfs rw",
            )
        )

        self.assertEqual(
            host_telemetry.linux_mount("/mnt/data/my scratch/job", "8:17", mountinfo),
            ("/mnt/data/my scratch", "xfs"),
        )
        self.assertEqual(
            host_telemetry.linux_mount("/mnt/data/other", "8:17", mountinfo),
            ("/mnt/data", "xfs"),
        )
        self.assertEqual(
            host_telemetry.linux_mount("/home/runner", "8:1", mountinfo),
            ("/", "ext4"),
        )
        self.assertEqual(
            host_telemetry.linux_mount("/tmp/x", "0:45", mountinfo), ("/tmp", "tmpfs")
        )
        self.assertIsNone(
            host_telemetry.linux_mount("/mnt/database", "8:17", mountinfo)
        )

    def test_volume_keys_ignore_drive_letter_case_and_trailing_separators(self):
        for mount_point, key in (
            ("c:\\", "C:"),
            ("F:\\", "F:"),
            ("C:\\mnt\\data\\", "C:\\mnt\\data"),
            ("/", "/"),
            ("/mnt/data/", "/mnt/data"),
        ):
            with self.subTest(mount_point=mount_point):
                self.assertEqual(host_telemetry._volume_key(mount_point), key)

    def test_provenance_rows_describe_runner_azure_disks_and_volumes(self):
        host: dict[str, object] = {
            "schema_version": 1,
            "runner_name": "azure-windows-3",
            "machine_name": "VMSSVAOAN000002",
            "os": "Windows-2022Server-10.0.20348-SP0",
            "os_version": {
                "system": "Windows",
                "release": "10.0.20348",
                "revision": 4294,
            },
            "cpu_model": "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz",
            "cpu": {
                "vendor": "GenuineIntel",
                "family": 6,
                "model": 106,
                "stepping": 6,
                "microcode": "0xffffffff",
            },
            "logical_processors": 16,
            "memory_bytes": 64 * GIB,
            "azure": host_telemetry.azure_compute_provenance(IMDS_COMPUTE),
            "volumes": {
                "workspace": {
                    "path": r"C:\actions-runner\_work\nvx\nvx",
                    "volume": "C:",
                    "filesystem": "NTFS",
                    "size_bytes": 127 * GIB,
                    "system_volume": True,
                    "disk_number": 0,
                    "scsi_address": "0:0:0:0",
                },
                "temporary": {"path": r"C:\Windows\TEMP", "error": "access denied"},
                "scratch": {
                    "path": r"F:\nvx-benchmark-scratch\job",
                    "volume": "F:",
                    "filesystem": "NTFS",
                    "size_bytes": 512 * GIB,
                    "system_volume": False,
                    "disk_number": 2,
                    "scsi_address": "1:0:0:0",
                },
            },
        }

        rows = dict(host_telemetry.host_provenance_rows(host))

        self.assertEqual(
            rows,
            {
                "Runner": "azure-windows-3",
                "Machine": "VMSSVAOAN000002",
                "Operating system": (
                    "Windows-2022Server-10.0.20348-SP0, build 10.0.20348.4294"
                ),
                "CPU": (
                    "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz "
                    "(GenuineIntel 6/106/6, microcode 0xffffffff), "
                    "16 logical processors"
                ),
                "Memory": "64.0 GiB",
                "Azure VM size": "Standard_D16ds_v5",
                "Azure OS disk": "Premium_LRS, 128 GiB, ReadWrite caching",
                "Azure data disk LUN 0": "Premium_LRS, 512 GiB, None caching",
                "Azure data disk LUN 1": (
                    "UltraSSD_LRS, 1024 GiB, None caching, 934 MiB/s limit, "
                    "65280 IOPS limit"
                ),
                "Azure temporary disk": "0.6 GiB",
                "Workspace volume": (
                    "C: NTFS, 127.0 GiB, system volume, disk 0, SCSI 0:0:0:0, "
                    r"at C:\actions-runner\_work\nvx\nvx"
                ),
                "Temporary-directory volume": "unavailable (access denied)",
                "Benchmark scratch volume": (
                    "F: NTFS, 512.0 GiB, disk 2, SCSI 1:0:0:0, "
                    r"at F:\nvx-benchmark-scratch\job"
                ),
            },
        )
        self.assertTrue(
            host_telemetry.describe_host_provenance(host).startswith(
                "Runner: azure-windows-3; Machine: VMSSVAOAN000002; "
            )
        )

    def test_provenance_rows_report_missing_runner_and_metadata_errors(self):
        rows = dict(
            host_telemetry.host_provenance_rows(
                {"azure": {"error": "Azure instance metadata is unavailable: x"}}
            )
        )

        self.assertEqual(rows["Runner"], "not recorded")
        self.assertEqual(rows["CPU"], "unknown CPU")
        self.assertEqual(
            rows["Azure metadata"], "Azure instance metadata is unavailable: x"
        )
        self.assertNotIn("Hypervisor", rows)

    def test_provenance_rows_describe_the_hypervisor(self):
        for hypervisor, description in (
            (
                {"vendor": "Microsoft Hv", "build": "10.0.26100.9444"},
                "Microsoft Hv 10.0.26100.9444",
            ),
            ({"vendor": "KVMKVMKVM", "build": None}, "KVMKVMKVM"),
            ({"vendor": None, "build": None}, "none"),
            (
                {"error": "CPUID is unavailable: [Errno 13] mprotect failed"},
                "CPUID is unavailable: [Errno 13] mprotect failed",
            ),
        ):
            with self.subTest(hypervisor=hypervisor):
                rows = host_telemetry.host_provenance_rows(
                    {"os": "Linux-6.18.34.mshv3-1.azl3", "hypervisor": hypervisor}
                )
                self.assertEqual(
                    [label for label, _ in rows][2:4],
                    ["Operating system", "Hypervisor"],
                )
                self.assertEqual(dict(rows)["Hypervisor"], description)
        self.assertNotIn(
            "Hypervisor",
            dict(host_telemetry.host_provenance_rows({"hypervisor": None})),
        )


class CounterParserTests(unittest.TestCase):
    def test_parses_writeback_counters_at_kernel_offsets(self):
        data = bytearray(344)
        expected = {
            "available_pages": (44, 4_000_000),
            "modified_pages_written": (96, 11),
            "modified_page_writes": (100, 12),
            "mapped_pages_written": (104, 13),
            "mapped_page_writes": (108, 14),
            "lazy_write_ios": (280, 15),
            "lazy_write_pages": (284, 16),
            "cache_flushes": (288, 17),
            "cache_flush_pages": (292, 18),
        }
        for offset, value in expected.values():
            struct.pack_into("<I", data, offset, value)
        struct.pack_into("<QQ", data, 312, 2048, 65536)
        base = {name: value for name, (_, value) in expected.items()}

        self.assertEqual(
            host_telemetry.parse_system_performance(bytes(data)),
            {**base, "cache_dirty_pages": 2048, "cache_dirty_page_threshold": 65536},
        )
        self.assertEqual(
            host_telemetry.parse_system_performance(bytes(data[:312])), base
        )
        with self.assertRaisesRegex(ValueError, "only 311 bytes"):
            host_telemetry.parse_system_performance(bytes(data[:311]))

    def test_sums_defender_process_times_and_transfers(self):
        base = 0x7FF000000000
        processes = (
            ("", 0, 0, (0, 0, 0)),
            ("MsMpEng.exe", 30, 70, (1000, 2000, 3000)),
            # The same name length as MsMpEng.exe, so the name must match.
            ("notepad.exe", 5, 5, (9, 9, 9)),
            ("MpDefenderCoreService.exe", 1, 2, (4, 5, 6)),
        )
        names_offset = 256 * len(processes)
        entries = bytearray(names_offset)
        names = bytearray()
        for index, (name, user, kernel, transfers) in enumerate(processes):
            offset = index * 256
            next_entry = 0 if index == len(processes) - 1 else 256
            encoded = name.encode("utf-16-le")
            address = base + names_offset + len(names) if encoded else 0
            struct.pack_into("<I", entries, offset, next_entry)
            struct.pack_into("<qq", entries, offset + 40, user, kernel)
            struct.pack_into(
                "<HH4xQ", entries, offset + 56, len(encoded), len(encoded), address
            )
            struct.pack_into("<6Q", entries, offset + 208, 1, 2, 3, *transfers)
            names += encoded
        data = bytes(entries + names)

        self.assertEqual(
            host_telemetry.parse_defender_processes(data, base),
            {
                "processes": 2,
                "cpu_ticks": 103,
                "read_bytes": 1004,
                "write_bytes": 2005,
                "other_bytes": 3006,
            },
        )
        with self.assertRaisesRegex(ValueError, "truncated"):
            host_telemetry.parse_defender_processes(data[:300], base)
        with self.assertRaisesRegex(ValueError, "outside the buffer"):
            host_telemetry.parse_defender_processes(data, base + 4096)


def counters_at(elapsed_ms: int, *, processes: bool) -> dict[str, object]:
    """Return counters that grow linearly: the scratch volume writes 120
    MiB/s at 500 IOPS with 30 ms latency and 90% busy time, the workspace
    volume writes 2 MiB/s, CPUs are 25% busy, the lazy writer writes 10
    MiB/s, and Defender uses a quarter of one CPU and reads 1 MiB/s."""
    writes = elapsed_ms // 2
    sample: dict[str, object] = {
        "elapsed_ns": elapsed_ms * 1_000_000,
        "volumes": {
            "F:": {
                "bytes_read": 0,
                "bytes_written": 120 * MIB * elapsed_ms // 1000,
                "read_ticks": 0,
                "write_ticks": writes * 300_000,
                "idle_ticks": elapsed_ms * 1_000,
                "reads": 0,
                "writes": writes,
                "queue_depth": 8 if elapsed_ms == 200 else 4,
                "query_ticks": elapsed_ms * 10_000,
            },
            "C:": {
                "bytes_read": 0,
                "bytes_written": 2 * MIB * elapsed_ms // 1000,
                "read_ticks": 0,
                "write_ticks": elapsed_ms * 100,
                "idle_ticks": elapsed_ms * 9_900,
                "reads": 0,
                "writes": elapsed_ms // 10,
                "queue_depth": 0,
                "query_ticks": elapsed_ms * 10_000,
            },
        },
        "cpu": {
            "idle_ticks": elapsed_ms * 120_000,
            "kernel_ticks": elapsed_ms * 136_000,
            "user_ticks": elapsed_ms * 24_000,
        },
        "memory": {
            "available_pages": 1_000_000 - elapsed_ms,
            "modified_pages_written": 7,
            "mapped_pages_written": 9,
            "lazy_write_pages": elapsed_ms * 256 // 100,
            "cache_flush_pages": elapsed_ms * 3072 // 100,
            "cache_dirty_pages": 51_200 if elapsed_ms == 200 else 25_600,
            "cache_dirty_page_threshold": 262_144,
        },
    }
    if processes:
        sample["defender"] = {
            "processes": 2,
            "cpu_ticks": elapsed_ms * 2_500,
            "read_bytes": MIB * elapsed_ms // 1000,
            "write_bytes": 0,
            "other_bytes": 0,
        }
    return sample


def counter_series(*elapsed_ms: int) -> list[dict[str, object]]:
    process_samples = {elapsed_ms[0], elapsed_ms[-1]}
    return [
        counters_at(elapsed, processes=elapsed in process_samples)
        for elapsed in elapsed_ms
    ]


class StorageWindowTests(unittest.TestCase):
    def test_summarizes_disk_cpu_writeback_and_defender_activity(self):
        samples = counter_series(*range(0, 501, 50))

        window = host_telemetry.summarize_storage_window(
            samples, 100_000_000, 400_000_000
        )

        self.assertEqual(
            window,
            {
                "start_ns": 100_000_000,
                "end_ns": 400_000_000,
                "sampled_start_ns": 100_000_000,
                "sampled_end_ns": 400_000_000,
                "volumes": {
                    "F:": {
                        "read_mib_per_second": 0.0,
                        "write_mib_per_second": 120.0,
                        "read_iops": 0.0,
                        "write_iops": 500.0,
                        "average_read_latency_ms": None,
                        "average_write_latency_ms": 30.0,
                        "average_queue_length": 15.0,
                        "busy_percent": 90.0,
                        "max_queue_depth": 8,
                    },
                    "C:": {
                        "read_mib_per_second": 0.0,
                        "write_mib_per_second": 2.0,
                        "read_iops": 0.0,
                        "write_iops": 100.0,
                        "average_read_latency_ms": None,
                        "average_write_latency_ms": 0.1,
                        "average_queue_length": 0.01,
                        "busy_percent": 1.0,
                        "max_queue_depth": 0,
                    },
                },
                "cpu": {"busy_percent": 25.0},
                "memory": {
                    "available_mib_min": 3904.688,
                    "cache_dirty_mib_max": 200.0,
                    "cache_dirty_threshold_mib": 1024.0,
                    "lazy_write_mib_per_second": 10.0,
                    "cache_flush_mib_per_second": 120.0,
                    "modified_write_mib_per_second": 0.0,
                    "mapped_write_mib_per_second": 0.0,
                },
                # Process counters come only from the first and last samples.
                "defender": {
                    "processes": 2,
                    "cpu_percent": 25.0,
                    "read_mib_per_second": 1.0,
                    "write_mib_per_second": 0.0,
                    "other_mib_per_second": 0.0,
                    "sampled_start_ns": 0,
                    "sampled_end_ns": 500_000_000,
                },
            },
        )

    def test_each_counter_group_uses_its_nearest_complete_samples(self):
        samples = counter_series(*range(0, 501, 50))
        # Transient read failures at the phase's nearest samples.
        failed_start, failed_end = samples[2], samples[8]
        failed_start["volumes"] = {}
        del failed_start["cpu"]
        del failed_end["memory"]

        window = host_telemetry.summarize_storage_window(
            samples, 100_000_000, 400_000_000
        )

        assert window is not None
        self.assertEqual(window["sampled_start_ns"], 100_000_000)
        self.assertEqual(window["sampled_end_ns"], 400_000_000)
        volumes = cast(dict[str, dict[str, object]], window["volumes"])
        self.assertEqual(volumes["F:"]["write_mib_per_second"], 120.0)
        self.assertEqual(
            (volumes["F:"]["sampled_start_ns"], volumes["F:"]["sampled_end_ns"]),
            (50_000_000, 400_000_000),
        )
        self.assertEqual(
            window["cpu"],
            {
                "busy_percent": 25.0,
                "sampled_start_ns": 50_000_000,
                "sampled_end_ns": 400_000_000,
            },
        )
        memory = cast(dict[str, object], window["memory"])
        self.assertEqual(memory["lazy_write_mib_per_second"], 10.0)
        self.assertEqual(
            (memory["sampled_start_ns"], memory["sampled_end_ns"]),
            (100_000_000, 350_000_000),
        )

    def test_uses_the_samples_nearest_to_each_phase_boundary(self):
        samples = counter_series(0, 60, 120, 180, 240, 300, 360)

        window = host_telemetry.summarize_storage_window(
            samples, 100_000_000, 290_000_000
        )

        assert window is not None
        self.assertEqual(window["sampled_start_ns"], 120_000_000)
        self.assertEqual(window["sampled_end_ns"], 300_000_000)
        self.assertIsNone(
            host_telemetry.summarize_storage_window(samples, 10_000_000, 20_000_000)
        )
        self.assertIsNone(
            host_telemetry.summarize_storage_window(samples[:1], 0, 100_000_000)
        )

    def test_request_counts_wrap_at_32_bits(self):
        samples = counter_series(0, 100, 200)
        for sample, writes in zip(samples, (2**32 - 30, 2**32 - 10, 40), strict=True):
            volumes = cast(dict[str, dict[str, int]], sample["volumes"])
            volumes["F:"]["writes"] = writes

        window = host_telemetry.summarize_storage_window(samples, 0, 200_000_000)

        assert window is not None
        volumes = cast(dict[str, dict[str, object]], window["volumes"])
        self.assertEqual(volumes["F:"]["write_iops"], 350.0)

    def test_skips_defender_totals_when_a_defender_process_starts_or_exits(self):
        samples = counter_series(0, 100, 200)
        defender = cast(dict[str, int], samples[-1]["defender"])
        defender["processes"] = 3

        window = host_telemetry.summarize_storage_window(samples, 0, 200_000_000)

        assert window is not None
        self.assertIsNone(window["defender"])

    def test_describes_a_window_with_the_scratch_volume_first(self):
        window = host_telemetry.summarize_storage_window(
            counter_series(*range(0, 501, 50)), 100_000_000, 400_000_000
        )
        assert window is not None

        self.assertEqual(
            host_telemetry.describe_storage_window(
                window, {"C:": ["workspace", "system"], "F:": ["scratch"]}
            ),
            "scratch F: write 120.0 MiB/s at 30.0 ms, queue 15.0, 90% busy; "
            "workspace/system C: write 2.0 MiB/s at 0.1 ms, queue 0.0, 1% busy; "
            "CPU 25% busy; cache dirty 200.0 MiB, lazy writes 10.0 MiB/s; "
            "Defender 25.0% CPU",
        )


class FakeReader:
    def __init__(self, *, failures: frozenset[int] = frozenset()) -> None:
        self.reads: list[bool] = []
        self.closed = 0
        self.failures = failures

    def description(self) -> dict[str, object]:
        return {
            "source": "fake",
            "page_size_bytes": 4096,
            "volumes": {"F:": ["scratch"]},
        }

    def read(self, *, processes: bool) -> tuple[dict[str, object], list[str]]:
        self.reads.append(processes)
        if len(self.reads) in self.failures:
            raise RuntimeError("counter read failed")
        return {"processes": processes}, ["F: volume counters failed: busy"]

    def close(self) -> None:
        self.closed += 1


class StorageTelemetrySamplerTests(unittest.TestCase):
    def test_reads_process_counters_on_their_own_cadence(self):
        reader = FakeReader()
        times = iter(
            1_000 + milliseconds * 1_000_000 for milliseconds in (0, 100, 300, 550, 600)
        )
        telemetry = host_telemetry.StorageTelemetry(
            reader,
            1_000,
            interval_seconds=3600,
            process_interval_seconds=0.5,
            clock=lambda: next(times),
        )

        telemetry.start()
        self.assertTrue(telemetry._sample(final=False))
        self.assertTrue(telemetry._sample(final=False))
        self.assertTrue(telemetry._sample(final=False))
        telemetry.stop()
        telemetry.stop()
        result = telemetry.result({})

        self.assertEqual(reader.reads, [True, False, False, True, True])
        self.assertEqual(reader.closed, 1)
        self.assertEqual(
            [
                sample["elapsed_ns"]
                for sample in cast(list[dict[str, object]], result["samples"])
            ],
            [0, 100_000_000, 300_000_000, 550_000_000, 600_000_000],
        )
        self.assertEqual(result["errors"], ["F: volume counters failed: busy"])
        self.assertEqual(result["source"], "fake")
        self.assertEqual(result["interval_ms"], 3_600_000)
        self.assertEqual(result["process_interval_ms"], 500)
        self.assertFalse(result["truncated"])

    def test_truncation_still_records_the_final_sample(self):
        reader = FakeReader()
        times = iter(range(0, 10_000_000_000, 1_000_000))
        telemetry = host_telemetry.StorageTelemetry(
            reader, 0, interval_seconds=3600, max_samples=2, clock=lambda: next(times)
        )

        telemetry.start()
        self.assertTrue(telemetry._sample(final=False))
        self.assertFalse(telemetry._sample(final=False))
        telemetry.stop()
        result = telemetry.result({})

        self.assertTrue(result["truncated"])
        self.assertEqual(len(cast(list[object], result["samples"])), 3)

    def test_counter_failures_are_recorded_without_stopping_sampling(self):
        reader = FakeReader(failures=frozenset({2}))
        times = iter(range(0, 10_000_000_000, 1_000_000))
        telemetry = host_telemetry.StorageTelemetry(
            reader, 0, interval_seconds=3600, clock=lambda: next(times)
        )

        telemetry.start()
        telemetry._sample(final=False)
        telemetry._sample(final=False)
        telemetry.stop()
        result = telemetry.result({})

        samples = cast(list[dict[str, object]], result["samples"])
        self.assertEqual(len(samples), 4)
        self.assertEqual(samples[1], {"elapsed_ns": 1_000_000})
        self.assertEqual(
            result["errors"],
            [
                "F: volume counters failed: busy",
                "counter sampling failed: counter read failed",
            ],
        )

    def test_result_summarizes_only_telemetry_phases(self):
        times = iter((0, 50_000_000, 100_000_000))
        telemetry = host_telemetry.StorageTelemetry(
            FakeReader(), 0, interval_seconds=3600, clock=lambda: next(times)
        )
        telemetry.start()
        telemetry._sample(final=False)

        result = telemetry.result(
            {
                "capture.mapped_memory_flush": (0, 100_000_000),
                "capture.save_state": (0, 10_000_000),
            }
        )

        self.assertEqual(
            list(cast(dict[str, object], result["phases"])),
            ["capture.mapped_memory_flush"],
        )

    def test_background_sampling_runs_until_stopped(self):
        reader = FakeReader()
        telemetry = host_telemetry.StorageTelemetry(
            reader,
            time.perf_counter_ns(),
            interval_seconds=0.005,
            process_interval_seconds=3600,
        )

        telemetry.start()
        deadline = time.monotonic() + 10
        while len(reader.reads) < 4 and time.monotonic() < deadline:
            time.sleep(0.005)
        telemetry.stop()
        count = len(reader.reads)
        time.sleep(0.05)

        self.assertGreaterEqual(count, 5)
        self.assertEqual(len(reader.reads), count)
        self.assertTrue(reader.reads[0])
        self.assertTrue(reader.reads[-1])
        self.assertFalse(any(reader.reads[1:-1]))

    def test_describe_summarizes_a_window_in_one_line(self):
        series = iter(counter_series(0, 100, 200, 300, 400))

        class SeriesReader(FakeReader):
            def read(self, *, processes: bool) -> tuple[dict[str, object], list[str]]:
                self.reads.append(processes)
                sample = next(series)
                del sample["elapsed_ns"]
                return sample, []

        times = iter(range(0, 500_000_000, 100_000_000))
        reader = SeriesReader()
        telemetry = host_telemetry.StorageTelemetry(
            reader, 0, interval_seconds=3600, clock=lambda: next(times)
        )
        telemetry.start()
        for _ in range(3):
            telemetry._sample(final=False)

        self.assertEqual(
            telemetry.describe(0, 400_000_000),
            "scratch F: write 120.0 MiB/s at 30.0 ms, queue 15.0, 90% busy; "
            "volume C: write 2.0 MiB/s at 0.1 ms, queue 0.0, 1% busy; "
            "CPU 25% busy; cache dirty 200.0 MiB, lazy writes 10.0 MiB/s; "
            "Defender 25.0% CPU",
        )
        # Describing stops the sampling.
        self.assertEqual(reader.closed, 1)

    def test_describe_appends_the_counter_errors(self):
        times = iter(range(0, 500_000_000, 100_000_000))
        telemetry = host_telemetry.StorageTelemetry(
            FakeReader(), 0, interval_seconds=3600, clock=lambda: next(times)
        )
        telemetry.start()

        self.assertEqual(
            telemetry.describe(0, 100_000_000),
            "CPU n/a busy (errors: F: volume counters failed: busy)",
        )


class PlatformTelemetryTests(unittest.TestCase):
    def test_windows_reader_keeps_disk_counters_enabled_until_closed(self):
        advapi32 = MagicMock()
        with (
            patch.object(host_telemetry, "_enable_disk_performance", return_value=1234),
            patch.object(host_telemetry, "_advapi32", return_value=advapi32),
        ):
            reader = host_telemetry.WindowsCounterReader({})
            advapi32.WmiCloseBlock.assert_not_called()
            reader.close()
            reader.close()

        advapi32.WmiCloseBlock.assert_called_once_with(1234)

    def test_windows_reader_records_unavailable_disk_counters(self):
        with patch.object(
            host_telemetry,
            "_enable_disk_performance",
            side_effect=PermissionError("access denied"),
        ):
            reader = host_telemetry.WindowsCounterReader({})
        reader.close()

        self.assertEqual(
            reader._errors, ["disk counters could not be enabled: access denied"]
        )

    @unittest.skipUnless(sys.platform == "win32", "requires Windows")
    def test_windows_reads_counters_without_privileges(self):
        reader = host_telemetry.WindowsCounterReader(
            {
                "scratch": Path(tempfile.gettempdir()),
                "system": Path(os.environ["SystemRoot"]),
            }
        )
        try:
            first, first_errors = reader.read(processes=True)
            second, second_errors = reader.read(processes=False)
        finally:
            reader.close()

        self.assertEqual(first_errors, [])
        self.assertEqual(second_errors, [])
        roles = cast(dict[str, list[str]], reader.description()["volumes"])
        self.assertEqual(
            sorted(role for values in roles.values() for role in values),
            ["scratch", "system"],
        )
        volumes = cast(dict[str, dict[str, int]], first["volumes"])
        self.assertEqual(set(volumes), set(roles))
        for counters in volumes.values():
            self.assertGreater(counters["query_ticks"], 0)
        cpu = cast(dict[str, int], first["cpu"])
        self.assertGreaterEqual(cpu["kernel_ticks"], cpu["idle_ticks"])
        memory = cast(dict[str, int], first["memory"])
        self.assertGreater(memory["available_pages"], 0)
        self.assertIn("cache_dirty_page_threshold", memory)
        self.assertEqual(
            set(cast(dict[str, int], first["defender"])),
            {"processes", "cpu_ticks", "read_bytes", "write_bytes", "other_bytes"},
        )
        self.assertNotIn("defender", second)

    @unittest.skipIf(sys.platform == "win32", "Windows samples storage counters")
    def test_other_hosts_record_no_storage_telemetry(self):
        self.assertIsNone(
            host_telemetry.create_storage_telemetry(
                {"scratch": Path(tempfile.gettempdir())}, 0
            )
        )
        self.assertIsNone(
            host_telemetry.describe_volume_activity(
                {"scratch": Path(tempfile.gettempdir())}, 0.1
            )
        )

    @unittest.skipUnless(sys.platform == "win32", "requires Windows")
    def test_windows_describes_a_volume_s_activity_without_privileges(self):
        activity = host_telemetry.describe_volume_activity(
            {"scratch": Path(tempfile.gettempdir())}, 0.2
        )

        assert activity is not None
        self.assertRegex(activity, r"^scratch \S+ write \S+ MiB/s at ")
        self.assertRegex(activity, r"; CPU \d+% busy")
        self.assertNotIn("errors", activity)


def rendered_event(
    provider: str,
    event_id: int,
    level: str,
    time: str,
    message: str,
    data: dict[str, str],
) -> str:
    """Return an event as wevtutil renders it."""
    fields = "".join(
        f"<Data Name='{name}'>{value}</Data>" for name, value in data.items()
    )
    return (
        "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'>"
        f"<System><Provider Name='{provider}'/><EventID Qualifiers='0'>{event_id}"
        f"</EventID><Level>3</Level><TimeCreated SystemTime='{time}'/>"
        "<Computer>runner</Computer></System>"
        f"<EventData>{fields}</EventData><RenderingInfo Culture='en-US'>"
        f"<Message>{message}</Message><Level>{level}</Level></RenderingInfo>"
        "</Event>"
    )


DEVICE = {"PortNumber": "1", "PathID": "0", "TargetID": "0", "LUN": "0"}
SLOW_IO = rendered_event(
    "Microsoft-Windows-StorPort",
    557,
    "Warning",
    "2026-10-25T00:49:12.3456789Z",
    "An IO took more than 30000 ms to complete for Storport Device (Port = 1, "
    "Path = 0, Target = 0, Lun = 0).&#13;&#10;Second line.",
    {
        **DEVICE,
        "Threshold_ms": "30000",
        "RequestDuration_ms": "35123",
        "WaitDuration_ms": "12",
    },
)
LATENCY = rendered_event(
    "Microsoft-Windows-StorPort",
    505,
    "Information",
    "2026-10-25T00:40:00.0000000Z",
    "Performance summary for Storport Device",
    {
        **DEVICE,
        "IoLatencyBuckets": "128us, 256us, 512us, 1ms, 4ms, 16ms, 64ms, 128ms, "
        "256ms, 512ms, 1000ms, 2000ms, 10000ms, 10000+ms,",
        **{f"BucketIoSuccess{index}": "9" for index in range(1, 12)},
        "BucketIoSuccess12": "2",
        "BucketIoSuccess13": "3",
        "BucketIoFailed13": "1",
        "BucketIoSuccess14": "1",
    },
)
TIMEOUT = rendered_event(
    "Microsoft-Windows-StorPort",
    500,
    "Warning",
    "2026-10-25T00:48:40.0000000Z",
    "A request timed out for Storport Device",
    {**DEVICE, "SrbTimeout": "60"},
)
MINIPORT_RESET = rendered_event(
    "storvsc",
    129,
    "Warning",
    "2026-10-25T00:48:37.0000000Z",
    "Reset to device, \\Device\\RaidPort1, was issued.",
    {},
)


def rendered_events(*events: str) -> bytes:
    """Return a document of rendered events, as wevtutil writes it in UTF-16."""
    return f"<Events>\r\n{''.join(events)}\r\n</Events>\r\n".encode("utf-16")


class StorageEventTests(unittest.TestCase):
    def test_parses_round_trip_and_offset_times(self):
        self.assertEqual(
            storage_events.parse_utc("2026-10-28T23:51:46.1234567Z"),
            datetime.datetime(2026, 10, 28, 23, 51, 46, 123456, datetime.timezone.utc),
        )
        self.assertEqual(
            storage_events.parse_utc("2026-10-28T16:51:46-07:00"),
            datetime.datetime(2026, 10, 28, 23, 51, 46, tzinfo=datetime.timezone.utc),
        )
        for invalid in ("", "2026-10-28T23:51:46", "yesterday"):
            with self.assertRaises(ValueError, msg=invalid):
                storage_events.parse_utc(invalid)

    def test_queries_a_window_of_a_channel_or_of_its_providers(self):
        since = datetime.datetime(
            2026, 10, 28, 23, 51, 46, 123999, datetime.timezone.utc
        )
        until = datetime.datetime(2026, 10, 29, 0, 20, tzinfo=datetime.timezone.utc)
        window = (
            "TimeCreated[@SystemTime>='2026-10-28T23:51:46.123Z' and "
            "@SystemTime<='2026-10-29T00:20:00.000Z']"
        )
        self.assertEqual(
            storage_events.event_query(since, until), f"*[System[{window}]]"
        )
        self.assertEqual(
            storage_events.event_query(since, until, ("storvsc", "disk")),
            f"*[System[Provider[@Name='storvsc' or @Name='disk'] and {window}]]",
        )

    @unittest.skipUnless(sys.platform == "win32", "only Windows has event logs")
    def test_reads_a_window_of_a_windows_log(self):
        until = datetime.datetime.now(datetime.timezone.utc)
        query = storage_events.event_query(
            until - datetime.timedelta(minutes=1),
            until,
            storage_events.SYSTEM_STORAGE_PROVIDERS,
        )
        document = storage_events.read_events(storage_events.SYSTEM_CHANNEL, query)

        for event in storage_events.parse_events("System", document):
            self.assertIn(event.provider, storage_events.SYSTEM_STORAGE_PROVIDERS)
        with self.assertRaisesRegex(OSError, "^wevtutil failed with status 15007"):
            storage_events.read_events("NVX-Missing-Channel/Operational", "*")

    def test_a_query_is_killed_and_left_behind_once_it_overruns(self):
        # Like any process, wevtutil may wait for a stalled disk even after
        # its kill.
        released = threading.Event()

        def communicate() -> tuple[bytes, bytes]:
            released.wait(30)
            return b"", b""

        process = MagicMock()
        process.communicate.side_effect = communicate
        with patch.object(
            storage_events.subprocess, "Popen", return_value=process
        ) as popen:
            started = time.monotonic()
            try:
                with self.assertRaisesRegex(
                    TimeoutError, r"^wevtutil did not finish within 0\.1s$"
                ):
                    storage_events.read_events(
                        storage_events.STORPORT_CHANNEL, "*", timeout=0.1
                    )
                elapsed = time.monotonic() - started
            finally:
                released.set()

        process.kill.assert_called_once_with()
        self.assertLess(elapsed, 10)
        self.assertEqual(
            popen.call_args.args[0][:3],
            ["wevtutil", "query-events", storage_events.STORPORT_CHANNEL],
        )

    def test_parses_rendered_events(self):
        [event] = storage_events.parse_events("Storport", rendered_events(SLOW_IO))

        self.assertEqual(event.provider, "Microsoft-Windows-StorPort")
        self.assertEqual((event.event_id, event.level), (557, "Warning"))
        self.assertEqual(event.time, "2026-10-25T00:49:12.3456789Z")
        self.assertEqual(event.device, "1:0:0:0")
        self.assertEqual(event.data["RequestDuration_ms"], "35123")
        self.assertEqual(
            event.summary(),
            "2026-10-25T00:49:12.3456789Z Storport 557 Warning: An IO took more "
            "than 30000 ms to complete for Storport Device (Port = 1, Path = 0, "
            "Target = 0, Lun = 0).",
        )
        [reset] = storage_events.parse_events("System", rendered_events(MINIPORT_RESET))
        self.assertIsNone(reset.device)
        self.assertTrue(reset.summary().startswith("2026-10-25T00:48:37.0000000Z "))
        self.assertIn(" System storvsc 129 Warning: Reset to device", reset.summary())

    def test_counts_the_io_of_latency_buckets_above_a_second(self):
        [event] = storage_events.parse_events("Storport", rendered_events(LATENCY))

        self.assertEqual(
            storage_events.slow_latency_counts(event),
            [("1000ms-2000ms", 2), ("2000ms-10000ms", 4), ("10000+ms", 1)],
        )

    def test_saves_and_summarizes_a_window_and_warns_of_slow_io(self):
        documents = {
            storage_events.STORPORT_CHANNEL: rendered_events(LATENCY, TIMEOUT, SLOW_IO),
            storage_events.SYSTEM_CHANNEL: rendered_events(MINIPORT_RESET),
        }
        queries: list[tuple[str, str]] = []
        calls: list[str] = []

        def read(channel: str, query: str) -> bytes:
            queries.append((channel, query))
            calls.append(channel)
            return documents[channel]

        def named(volumes: dict[str, Path]) -> dict[str, str]:
            del volumes
            calls.append("volumes")
            return {"1:0:0:0": "workspace/scratch F:"}

        since = datetime.datetime(2026, 10, 25, 0, 30, tzinfo=datetime.timezone.utc)
        until = datetime.datetime(2026, 10, 25, 1, 0, tzinfo=datetime.timezone.utc)
        volumes = {"scratch": Path("F:/scratch")}
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(storage_events, "read_events", side_effect=read),
            patch.object(
                storage_events, "volume_devices", side_effect=named
            ) as devices,
        ):
            output = Path(temporary) / "storage-events"
            report = storage_events.save_storage_events(
                since, output, lambda: volumes, now=until
            )
            saved = {path.name: path.read_bytes() for path in output.iterdir()}

        devices.assert_called_once_with(volumes)
        # The volumes, which may have stalled, are probed once the events are
        # saved.
        self.assertEqual(
            calls,
            [storage_events.STORPORT_CHANNEL, storage_events.SYSTEM_CHANNEL, "volumes"],
        )
        self.assertIn("@Name='storvsc'", queries[1][1])
        for _, query in queries:
            self.assertIn(
                "@SystemTime>='2026-10-25T00:30:00.000Z' and "
                "@SystemTime<='2026-10-25T01:00:00.000Z'",
                query,
            )
        self.assertEqual(
            saved["storport-operational.xml"],
            documents[storage_events.STORPORT_CHANNEL],
        )
        self.assertEqual(
            saved["system-storage.xml"], documents[storage_events.SYSTEM_CHANNEL]
        )
        device = "device 1:0:0:0 (workspace/scratch F:)"
        lines = [
            "Storage events from 2026-10-25T00:30:00.000Z to 2026-10-25T01:00:00.000Z",
            "Storport: 3 events: 1 of 500, 1 of 505, 1 of 557",
            "System: 1 event: 1 of storvsc 129",
            f"2026-10-25T00:40:00.0000000Z latency summary of {device}: 2 in "
            "1000ms-2000ms, 4 in 2000ms-10000ms, 1 in 10000+ms",
            "2026-10-25T00:48:37.0000000Z storvsc reset: Reset to device, "
            "\\Device\\RaidPort1, was issued.",
            f"2026-10-25T00:48:40.0000000Z request timeout on {device} after 60 s",
            f"2026-10-25T00:49:12.3456789Z slow I/O on {device}: took 35123 ms, "
            "waited 12 ms, threshold 30000 ms",
        ]
        self.assertEqual(report.lines(), lines)
        summary = saved[storage_events.SUMMARY_FILENAME].decode("utf-8")
        self.assertTrue(summary.startswith("\n".join(lines) + "\n\nAll events:\n"))
        self.assertEqual(summary.count("\n  2026-10-25T"), 4)
        self.assertEqual(
            report.warnings(),
            [
                (
                    "Slow storage I/O",
                    "Storport recorded 1 I/O slower than its threshold of 30000 "
                    f"ms in this job: the slowest took 35123 ms on {device} at "
                    "2026-10-25T00:49:12.3456789Z. The diagnostics artifact keeps "
                    "the job's storage events.",
                )
            ],
        )

    def test_reports_a_log_that_cannot_be_read(self):
        def read(channel: str, _query: str) -> bytes:
            if channel == storage_events.STORPORT_CHANNEL:
                raise OSError("wevtutil failed with status 5: Access is denied.")
            return rendered_events()

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(storage_events, "read_events", side_effect=read),
        ):
            output = Path(temporary) / "storage-events"
            report = storage_events.save_storage_events(
                datetime.datetime(2026, 10, 25, tzinfo=datetime.timezone.utc), output
            )
            saved = sorted(path.name for path in output.iterdir())

        self.assertEqual(saved, ["storage-events.txt", "system-storage.xml"])
        self.assertEqual(
            report.lines()[1:],
            [
                "Storport: unavailable: wevtutil failed with status 5: Access is "
                "denied.",
                "System: 0 events",
            ],
        )
        self.assertEqual(
            report.warnings(),
            [
                (
                    "Storage events",
                    "Could not read the Storport events: wevtutil failed with "
                    "status 5: Access is denied.",
                )
            ],
        )

    def test_keeps_the_events_when_probing_the_volumes_stalls_or_fails(self):
        released = threading.Event()

        def stalled() -> dict[str, Path]:
            released.wait(30)
            return {}

        def failed() -> dict[str, Path]:
            raise OSError("[WinError 21] The device is not ready")

        def read(channel: str, _query: str) -> bytes:
            if channel == storage_events.STORPORT_CHANNEL:
                return rendered_events(SLOW_IO)
            return rendered_events()

        cases = (
            (stalled, "probing them did not finish within 0.1s"),
            (failed, "[WinError 21] The device is not ready"),
        )
        for volumes, failure in cases:
            with (
                self.subTest(failure=failure),
                tempfile.TemporaryDirectory() as temporary,
                patch.object(storage_events, "read_events", side_effect=read),
                patch.object(storage_events, "VOLUME_NAMING_TIMEOUT_SECONDS", 0.1),
            ):
                output = Path(temporary) / "storage-events"
                try:
                    report = storage_events.save_storage_events(
                        datetime.datetime(2026, 10, 25, tzinfo=datetime.timezone.utc),
                        output,
                        volumes,
                    )
                finally:
                    released.set()
                summary = (output / storage_events.SUMMARY_FILENAME).read_text(
                    encoding="utf-8"
                )

            lines = report.lines()
            self.assertEqual(
                lines[1:4],
                [
                    "Storport: 1 event: 1 of 557",
                    "System: 0 events",
                    f"Volumes: unavailable: {failure}",
                ],
            )
            self.assertTrue(
                lines[4].startswith(
                    "2026-10-25T00:49:12.3456789Z slow I/O on device 1:0:0:0: "
                ),
                lines[4],
            )
            self.assertTrue(summary.startswith("\n".join(lines) + "\n"))

    def test_keeps_reading_and_reports_the_files_that_it_cannot_save(self):
        # The workspace can share the stalled volume, which can hold a write.
        released = threading.Event()

        def write(path: Path, contents: bytes) -> None:
            if path.name == "storport-operational.xml":
                released.wait(30)
                return
            path.write_bytes(contents)

        def read(channel: str, _query: str) -> bytes:
            if channel == storage_events.STORPORT_CHANNEL:
                return rendered_events(SLOW_IO)
            return rendered_events()

        since = datetime.datetime(2026, 10, 25, tzinfo=datetime.timezone.utc)
        stalled = "the write did not finish within 0.1s"
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(storage_events, "read_events", side_effect=read),
            patch.object(storage_events, "_write", side_effect=write),
            patch.object(storage_events, "WRITE_TIMEOUT_SECONDS", 0.1),
        ):
            output = Path(temporary) / "storage-events"
            try:
                report = storage_events.save_storage_events(since, output)
            finally:
                released.set()
            saved = sorted(path.name for path in output.iterdir())
            summary = (output / storage_events.SUMMARY_FILENAME).read_text(
                encoding="utf-8"
            )

        self.assertEqual(saved, ["storage-events.txt", "system-storage.xml"])
        lines = report.lines()
        self.assertEqual(
            lines[1:4],
            [
                "Storport: 1 event: 1 of 557",
                "System: 0 events",
                f"Not saved: storport-operational.xml: {stalled}",
            ],
        )
        self.assertTrue(summary.startswith("\n".join(lines) + "\n"))
        self.assertIn(
            ("Storage events", f"Could not save storport-operational.xml: {stalled}"),
            report.warnings(),
        )

        # Without its directory, no file is saved, but the events are read.
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(storage_events, "read_events", side_effect=read),
        ):
            output = Path(temporary) / "storage-events"
            output.write_bytes(b"")
            report = storage_events.save_storage_events(since, output)
        self.assertEqual(
            sorted(report.unsaved),
            ["storage-events.txt", "storport-operational.xml", "system-storage.xml"],
        )
        self.assertEqual(report.lines()[1], "Storport: 1 event: 1 of 557")

    def test_the_command_prints_the_summary_and_annotates_slow_io(self):
        report = storage_events.StorageEventReport(
            since="2026-10-25T00:30:00.000Z",
            until="2026-10-25T01:00:00.000Z",
            events=storage_events.parse_events("Storport", rendered_events(SLOW_IO)),
            failures={},
        )
        args = performance_arguments(
            "storage-events",
            "--since",
            "2026-10-25T00:30:00.0000000Z",
            "--output-dir",
            "events",
            "--scratch-dir",
            "F:/nvx-benchmark-scratch",
        )
        stdout = io.StringIO()
        with (
            patch.object(performance.sys, "platform", "win32"),
            patch.object(
                storage_events, "save_storage_events", return_value=report
            ) as save,
            patch.object(
                performance.benchmark_host,
                "benchmark_volumes",
                return_value={"scratch": Path("F:/nvx-benchmark-scratch")},
            ) as volumes,
            contextlib.redirect_stdout(stdout),
        ):
            status = performance.command_performance(args)
            # The command leaves the probing of the volumes to the save,
            # which bounds it after the export.
            volumes.assert_not_called()
            since, output, volume_source = save.call_args.args
            probed = volume_source()

        self.assertEqual(status, 0)
        self.assertEqual(
            since, datetime.datetime(2026, 10, 25, 0, 30, tzinfo=datetime.timezone.utc)
        )
        self.assertEqual(output, Path("events"))
        self.assertEqual(probed["scratch"], Path("F:/nvx-benchmark-scratch"))
        volumes.assert_called_once_with(Path("F:/nvx-benchmark-scratch"))
        printed = stdout.getvalue().splitlines()
        self.assertEqual(printed[:2], report.lines()[:2])
        self.assertIn("Saved the storage events in events", printed)
        self.assertTrue(
            printed[-1].startswith(
                "::warning title=Slow storage I/O::Storport recorded 1 I/O"
            ),
            printed[-1],
        )

    @unittest.skipIf(sys.platform == "win32", "Windows records storage events")
    def test_other_hosts_have_no_storage_events(self):
        args = performance_arguments(
            "storage-events", "--since", "2026-10-25T00:30:00Z", "--output-dir", "x"
        )
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(performance.command_performance(args), 2)
        self.assertIn("only on Windows", stderr.getvalue())


def performance_arguments(*arguments: str) -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    performance.configure_parser(parser)
    return parser.parse_args(list(arguments))


class AzureMetadataRequestTests(unittest.TestCase):
    def test_a_redirect_is_not_followed(self):
        served: list[str] = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                served.append(self.path)
                if self.path == "/redirect":
                    self.send_response(302)
                    self.send_header("Location", "/elsewhere")
                else:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b"{}")

            def log_message(self, format: str, *args: object) -> None:
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.addCleanup(server.server_close)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        url = f"http://127.0.0.1:{server.server_port}/redirect"

        with patch.object(host_telemetry, "AZURE_IMDS_URL", url):
            with self.assertRaises(urllib.error.HTTPError) as refused:
                host_telemetry.read_azure_compute_metadata(timeout=2)
            refused.exception.close()

        self.assertEqual(served, ["/redirect"])

    def test_a_refused_connection_closes_its_socket(self):
        created: list[socket.socket] = []
        real_socket = socket.socket

        class RefusingSocket(real_socket):
            def __init__(self, *args: Any, **kwargs: Any) -> None:
                super().__init__(*args, **kwargs)
                created.append(self)

            def connect(self, address: object) -> None:
                raise ConnectionRefusedError("refused")

        with (
            patch.object(socket, "socket", RefusingSocket),
            patch.object(
                host_telemetry, "AZURE_IMDS_URL", "http://127.0.0.1:9/compute"
            ),
        ):
            with self.assertRaises(OSError):
                host_telemetry.read_azure_compute_metadata(timeout=2)

        self.assertEqual(len(created), 1)
        self.assertEqual(created[0].fileno(), -1, "the refused socket was left open")

    def test_a_connect_still_in_progress_is_shut_down_at_the_deadline(self):
        blocked = threading.Event()
        released = threading.Event()
        real_socket = socket.socket

        class BlockingSocket(real_socket):
            def connect(self, address: object) -> None:
                blocked.set()
                if not released.wait(5):
                    raise OSError("the pending connect was never shut down")
                raise OSError("connect shut down")

            def shutdown(self, how: int) -> None:
                released.set()
                with contextlib.suppress(OSError):
                    super().shutdown(how)

        with (
            patch.object(socket, "socket", BlockingSocket),
            patch.object(
                host_telemetry, "AZURE_IMDS_URL", "http://127.0.0.1:9/compute"
            ),
        ):
            with self.assertRaises(TimeoutError):
                host_telemetry.read_azure_compute_metadata(timeout=0.2)

        self.assertTrue(blocked.is_set())
        self.assertTrue(released.is_set(), "the pending connect was never shut down")


if __name__ == "__main__":
    unittest.main()

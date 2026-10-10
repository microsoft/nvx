#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

"""Tests for benchmark host provenance and storage telemetry."""

import http.client
import json
import os
import struct
import sys
import tempfile
import time
import unittest
import urllib.request
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import host_telemetry  # noqa: E402

MIB = 1024 * 1024
GIB = 1024 * MIB
TICKS_PER_SECOND = 10_000_000
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
            ),
        ):
            self.assertEqual(
                host_telemetry.azure_vm_provenance(),
                {"error": "Azure instance metadata is unavailable: timed out"},
            )

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
        memory = provenance["memory_bytes"]
        self.assertIsInstance(memory, int)
        self.assertGreater(cast(int, memory), 0)
        volumes = cast(dict[str, dict[str, object]], provenance["volumes"])
        self.assertEqual(set(volumes), {"workspace", "scratch"})
        self.assertEqual(volumes["scratch"]["path"], str(scratch))
        self.assertNotIn("error", volumes["scratch"])
        self.assertIn("volume", volumes["scratch"])
        self.assertGreater(cast(int, volumes["scratch"]["size_bytes"]), 0)

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
            "cpu_model": "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz",
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
                "Operating system": "Windows-2022Server-10.0.20348-SP0",
                "CPU": (
                    "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz, "
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


if __name__ == "__main__":
    unittest.main()

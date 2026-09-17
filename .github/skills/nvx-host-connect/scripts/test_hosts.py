#!/usr/bin/env python3

import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import hosts  # noqa: E402


class HostInventoryTests(unittest.TestCase):
    def write_inventory(self, root: Path, inventory: object) -> Path:
        path = root / ".nvx-hosts.json"
        path.write_text(json.dumps(inventory), encoding="utf-8")
        return path

    def profile(self, host_type: str | None = "baremetal") -> dict[str, str]:
        profile = {
            "ssh_target": "nvx-lab",
            "backend": "kvm",
        }
        if host_type is not None:
            profile["host_type"] = host_type
        return profile

    def test_cli_lists_and_resolves_valid_host_types(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write_inventory(
                Path(temporary),
                {
                    "hosts": {
                        "bare": self.profile("baremetal"),
                        "virtual": self.profile("virtual-machine"),
                    },
                },
            )

            for arguments, key in (([], "hosts"), (["virtual"], "host")):
                output = io.StringIO()
                with (
                    patch.object(
                        sys, "argv", ["hosts.py", "--file", str(path), *arguments]
                    ),
                    redirect_stdout(output),
                ):
                    self.assertEqual(hosts.main(), 0)

                payload = json.loads(output.getvalue())
                if key == "hosts":
                    self.assertEqual(
                        [profile["host_type"] for profile in payload[key]],
                        ["baremetal", "virtual-machine"],
                    )
                else:
                    self.assertEqual(payload[key]["host_type"], "virtual-machine")

    def test_rejects_missing_host_type(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write_inventory(
                Path(temporary),
                {"hosts": {"lab": self.profile(None)}},
            )

            with self.assertRaisesRegex(
                hosts.InventoryError,
                r"hosts\.lab\.host_type must be a non-empty string",
            ):
                hosts.load_inventory(path)

    def test_rejects_unsupported_host_type(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write_inventory(
                Path(temporary),
                {"hosts": {"lab": self.profile("container")}},
            )

            with self.assertRaisesRegex(
                hosts.InventoryError,
                r"host_type must be one of: baremetal, virtual-machine",
            ):
                hosts.load_inventory(path)

    def test_rejects_version_field(self):
        for version in (1, 2):
            with (
                self.subTest(version=version),
                tempfile.TemporaryDirectory() as temporary,
            ):
                path = self.write_inventory(
                    Path(temporary),
                    {
                        "version": version,
                        "hosts": {"lab": self.profile()},
                    },
                )

                with self.assertRaisesRegex(
                    hosts.InventoryError,
                    r"inventory has unsupported fields: version",
                ):
                    hosts.load_inventory(path)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import azurelinux, common  # noqa: E402
from nvx_tools.build_constants import (  # noqa: E402
    AzureLinuxBuildConstants,
    BuildConstants,
    InitramfsBuildConstants,
)


class AzureLinuxBuildTests(unittest.TestCase):
    def test_azurelinux_manifest_and_package_lock_match_build_pins(self):
        manifest = json.loads(
            (BuildConstants.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(
                encoding="utf-8"
            )
        )["azurelinux"]
        self.assertEqual(manifest["image"], AzureLinuxBuildConstants.IMAGE)
        self.assertEqual(manifest["package_lock"], "azurelinux/packages.lock.json")
        self.assertEqual(
            manifest["package_lock_sha256"],
            azurelinux.package_lock_sha256(),
        )
        packages = azurelinux.load_package_lock()
        self.assertEqual(
            {"busybox", "util-linux"} - {package["name"] for package in packages},
            set(),
        )
        for package in packages:
            self.assertTrue(
                package["url"].startswith(
                    f"{AzureLinuxBuildConstants.REPOSITORY_URL}/Packages/"
                )
            )
        attributes = (BuildConstants.REPO_ROOT / ".gitattributes").read_text(
            encoding="utf-8"
        )
        self.assertIn("azurelinux/packages.lock.json text eol=lf", attributes)

    def test_azurelinux_package_lock_rejects_unpinned_records(self):
        document = json.loads(
            azurelinux.package_lock_path().read_text(encoding="utf-8")
        )
        first = document["packages"][0]
        cases: tuple[tuple[str, dict[str, object], str], ...] = (
            ("image", {"image": "mcr.microsoft.com/azurelinux/base/core:3.0"}, "image"),
            (
                "url",
                {
                    "packages": [
                        {**first, "url": first["url"].replace("/prod/", "/preview/")},
                        *document["packages"][1:],
                    ]
                },
                "must be fetched from",
            ),
            (
                "sha256",
                {
                    "packages": [
                        {**first, "sha256": "0" * 63},
                        *document["packages"][1:],
                    ]
                },
                "invalid SHA-256",
            ),
            (
                "order",
                {"packages": list(reversed(document["packages"]))},
                "sorted by name",
            ),
        )
        for name, update, message in cases:
            with (
                self.subTest(case=name),
                tempfile.TemporaryDirectory() as temporary,
            ):
                path = Path(temporary) / "packages.lock.json"
                path.write_text(json.dumps({**document, **update}), encoding="utf-8")
                with self.assertRaisesRegex(common.ScriptError, message):
                    azurelinux.load_package_lock(path)

    def test_azurelinux_download_packages_verifies_each_locked_rpm(self):
        destination = Path("rpms")
        with patch.object(azurelinux, "download_verified") as download_verified:
            downloaded = azurelinux.download_packages(destination)

        packages = azurelinux.load_package_lock()
        self.assertEqual(
            download_verified.call_args_list,
            [
                call(
                    package["url"],
                    destination / package["url"].rsplit("/", maxsplit=1)[-1],
                    package["sha256"],
                )
                for package in packages
            ],
        )
        self.assertEqual(len(downloaded), len(packages))

    def test_azurelinux_initramfs_uses_locked_busybox_applets(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        base_stage = dockerfile.split("FROM base AS kernel", 1)[0]
        self.assertNotIn("busybox", base_stage)
        azure_stage = dockerfile.split("FROM guest-base AS azurelinux-initramfs", 1)[1]
        self.assertIn("ln -sf ../sbin/busybox /rootfs/usr/bin/busybox", azure_stage)
        applets = (
            azure_stage.split("for utility in ", 1)[1]
            .split("; do", 1)[0]
            .replace("\\\n", " ")
            .split()
        )
        for applet in ("sh", "arp", "nc", "wget", "mdev", "ifconfig", "route"):
            self.assertIn(applet, applets)
        # util-linux provides these; the BusyBox applets lack required options.
        self.assertNotIn("setpriv", applets)
        self.assertNotIn("unshare", applets)
        self.assertIn(
            "busybox", [package["name"] for package in azurelinux.load_package_lock()]
        )

    def test_azurelinux_initramfs_gives_nobody_a_home(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        azure_stage = dockerfile.split("FROM guest-base AS azurelinux-initramfs", 1)[1]
        self.assertIn(
            "sed -i 's#^\\(nobody:[^:]*:65534:65534:[^:]*:\\)/dev/null:"
            "#\\1/nonexistent:#'",
            azure_stage,
        )
        self.assertIn(
            "grep -q '^nobody:[^:]*:65534:65534:[^:]*:/nonexistent:' "
            "/rootfs/etc/passwd",
            azure_stage,
        )
        self.assertIn("install -d -m 0755 /rootfs/nonexistent", azure_stage)

    def test_azurelinux_rootfs_installs_only_locked_rpms(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("tdnf install", dockerfile)
        packages_stage = dockerfile.split("FROM base AS azurelinux-packages", 1)[1]
        self.assertIn(
            "from nvx_tools.azurelinux import download_packages",
            packages_stage.split("\nFROM ", 1)[0],
        )
        rootfs_stage = dockerfile.split(
            "FROM ${AZURELINUX_IMAGE} AS azurelinux-rootfs", 1
        )[1].split("\nFROM ", 1)[0]
        self.assertIn(
            "COPY --from=azurelinux-packages /out/rpms/ /tmp/azurelinux-rpms/",
            rootfs_stage,
        )
        checksig = rootfs_stage.index("rpm --checksig /tmp/azurelinux-rpms/*.rpm")
        install = rootfs_stage.index("rpm --upgrade --verbose --hash")
        self.assertLess(checksig, install)

    def test_azurelinux_manifest_uses_package_metadata(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertIn("rpm -qa --queryformat", dockerfile)
        self.assertIn("if name == 'gpg-pubkey':", dockerfile)
        self.assertNotIn("'type': 'deb'", dockerfile)
        self.assertIn("'packages': json.loads", dockerfile)

    def test_azurelinux_initramfs_packs_normalized_rootfs(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        azure_stage = dockerfile.split("FROM guest-base AS azurelinux-initramfs", 1)[1]
        cleanup = azure_stage.index(
            "rm -rf /rootfs/usr/lib/sysimage/tdnf /rootfs/var/cache/tdnf \\\n"
            "    /rootfs/var/cache/ldconfig /rootfs/var/lib/rpm"
        )
        normalize = azure_stage.index(
            "find . -exec touch --no-dereference "
            f"--date=@{InitramfsBuildConstants.TIMESTAMP} {{}} +"
        )
        pack = azure_stage.index("find . -print0 | LC_ALL=C sort -z | cpio")
        self.assertLess(cleanup, normalize)
        self.assertLess(normalize, pack)

    def test_azurelinux_manifest_records_shared_input_digest(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        azure_stage = dockerfile.split("FROM guest-base AS azurelinux-initramfs", 1)[1]
        self.assertIn("from nvx_tools.azurelinux import input_sha256", azure_stage)
        self.assertIn(
            "'input_sha256': input_sha256(\n"
            "        image=os.environ['AZURELINUX_IMAGE'],\n"
            "        version=os.environ['AZURELINUX_VERSION'],\n"
            "    ),",
            azure_stage,
        )

    def test_azurelinux_input_digest_tracks_guest_sources_and_dockerfile(self):
        checkout_digest = azurelinux.input_sha256()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for source in azurelinux.input_files():
                destination = root / source.relative_to(BuildConstants.REPO_ROOT)
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, destination)
            with patch.object(BuildConstants, "REPO_ROOT", root):
                baseline = azurelinux.input_sha256()
                self.assertEqual(baseline, checkout_digest)
                self.assertNotEqual(
                    azurelinux.input_sha256(image="example.invalid/other@sha256:0"),
                    baseline,
                )
                self.assertNotEqual(azurelinux.input_sha256(version="3.1"), baseline)
                for relative in (
                    "guest/common/init",
                    "docker/Dockerfile",
                    "azurelinux/packages.lock.json",
                    "scripts/nvx_tools/azurelinux.py",
                    "scripts/nvx_tools/build_constants.py",
                    "scripts/nvx_tools/common.py",
                    "scripts/nvx_tools/guest_binaries.py",
                    "nvx/src/lib.rs",
                    "rust-toolchain.toml",
                ):
                    with self.subTest(path=relative):
                        path = root / relative
                        original = path.read_bytes()
                        path.write_bytes(original + b"\n# changed\n")
                        self.assertNotEqual(azurelinux.input_sha256(), baseline)
                        path.write_bytes(original)
                        self.assertEqual(azurelinux.input_sha256(), baseline)
                unrelated = root / "guest" / "ubuntu" / "nvx-bashrc"
                unrelated.parent.mkdir(parents=True, exist_ok=True)
                unrelated.write_text("changed\n", encoding="utf-8")
                self.assertEqual(azurelinux.input_sha256(), baseline)


if __name__ == "__main__":
    unittest.main()

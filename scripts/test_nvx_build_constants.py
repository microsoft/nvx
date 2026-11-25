#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import ast
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path, PurePosixPath
from typing import cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import check_test_layout  # noqa: E402
from nvx_tools import (  # noqa: E402
    archive,
    benchmark,
    build,
    build_config,
    ci,
    collect_alpine_sources,
    collect_ubuntu_sources,
    common,
    guests,
    release,
    ubuntu,
)
from nvx_tools.build_constants import (  # noqa: E402
    AlpineBuildConstants,
    BuildConstants,
    DockerBuildConstants,
    GuestRustBuildConstants,
    InitramfsBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
    ReleaseBuildConstants,
    UbuntuBuildConstants,
    ZstdBuildConstants,
)


class BuildConstantsTests(unittest.TestCase):
    def test_constants_are_grouped_class_namespaces(self):
        for namespace in (
            BuildConstants,
            KernelBuildConstants,
            OpenVMMBuildConstants,
            AlpineBuildConstants,
            UbuntuBuildConstants,
            InitramfsBuildConstants,
            DockerBuildConstants,
            ZstdBuildConstants,
            ReleaseBuildConstants,
        ):
            with self.subTest(namespace=namespace.__name__):
                self.assertEqual(namespace.__module__, "nvx_tools.build_constants")
                self.assertTrue(
                    all(
                        name.isupper()
                        for name in vars(namespace)
                        if not name.startswith("__")
                    )
                )

    def test_build_pins_and_artifact_names_have_one_literal_owner(self):
        centralized = {
            KernelBuildConstants.VERSION,
            KernelBuildConstants.URL,
            KernelBuildConstants.SHA256,
            KernelBuildConstants.BINARY_NAME,
            KernelBuildConstants.CONFIG_NAME,
            KernelBuildConstants.PROVENANCE_NAME,
            KernelBuildConstants.DEBUG_BINARY_NAME,
            KernelBuildConstants.DEBUG_CONFIG_NAME,
            KernelBuildConstants.DEBUG_PROVENANCE_NAME,
            OpenVMMBuildConstants.GNU_RUST_TARGET,
            OpenVMMBuildConstants.MUSL_RUST_TARGET,
            OpenVMMBuildConstants.WINDOWS_RUST_TARGET,
            OpenVMMBuildConstants.PROVENANCE_NAME,
            OpenVMMBuildConstants.CONTROL_CONTRACT_REVISION,
            AlpineBuildConstants.VERSION,
            AlpineBuildConstants.BRANCH,
            AlpineBuildConstants.MINIROOTFS_SHA256,
            AlpineBuildConstants.INITRAMFS_NAME,
            AlpineBuildConstants.PACKAGE_MANIFEST_NAME,
            UbuntuBuildConstants.VERSION,
            UbuntuBuildConstants.BASE_URL,
            UbuntuBuildConstants.BASE_SHA256,
            UbuntuBuildConstants.INITRAMFS_NAME,
            UbuntuBuildConstants.PACKAGE_MANIFEST_NAME,
            UbuntuBuildConstants.DISTRO_NAME,
            UbuntuBuildConstants.DISTRO_MANIFEST_NAME,
            InitramfsBuildConstants.PROVENANCE_NAME,
            ZstdBuildConstants.VERSION,
            ZstdBuildConstants.SHA256,
        }
        duplicates: list[tuple[str, int, str]] = []
        for path in (BuildConstants.REPO_ROOT / "scripts").rglob("*.py"):
            if (
                path.name == "build_constants.py"
                or path.name.startswith("test_")
                or path.relative_to(BuildConstants.REPO_ROOT).as_posix()
                in check_test_layout.PYTHON_TEST_SUPPORT
            ):
                continue
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
            duplicates.extend(
                (path.name, node.lineno, node.value)
                for node in ast.walk(tree)
                if isinstance(node, ast.Constant)
                and isinstance(node.value, str)
                and node.value in centralized
            )
        self.assertEqual(duplicates, [])

    def test_old_module_level_constants_are_not_reexported(self):
        names = (
            "REPO_ROOT",
            "BUILD_DIR",
            "SOURCE_DIR",
            "OPENVMM_DIR",
            "DEFAULT_KERNEL_VERSION",
            "DEFAULT_KERNEL_URL",
            "DEFAULT_KERNEL_SHA256",
            "DEFAULT_ALPINE_VERSION",
            "DEFAULT_ALPINE_BRANCH",
            "DEFAULT_ALPINE_MINIROOTFS_SHA256",
            "DEFAULT_UBUNTU_VERSION",
            "DEFAULT_UBUNTU_CODENAME",
            "DEFAULT_UBUNTU_ARCHITECTURE",
            "DEFAULT_UBUNTU_BASE_URL",
            "DEFAULT_UBUNTU_BASE_SHA256",
            "UBUNTU_PACKAGE_LOCK",
            "UBUNTU_EROFS_FORMAT",
            "MICROVM_ABI_VERSION",
            "CONTROL_SESSION_PROTOCOL_VERSION",
            "CONTROL_CONTRACT_REVISION",
            "OPENVMM_PROVENANCE_NAME",
            "KERNEL_PROVENANCE_NAME",
            "INITRAMFS_PROVENANCE_NAME",
            "REQUIRED_VIRTIO_CONSOLE_CONFIG",
            "REQUIRED_SHARED_STATUS_KERNEL_CONFIG",
            "REQUIRED_SANDBOX_KERNEL_CONFIG",
            "ZSTD_VERSION",
            "ZSTD_ARCHIVE",
            "ZSTD_URL",
            "ZSTD_SHA256",
            "OPENVMM_RUST_TOOLCHAIN",
            "OPENVMM_GUEST_RUST_TARGET",
            "OPENVMM_UEFI_RUST_TARGET",
            "OPENVMM_LINUX_MUSL_RUST_TARGET",
            "OPENVMM_RUST_TARGETS",
            "APORTS_URL",
            "REPOSITORIES",
            "UBUNTU_ARCHIVE_KEYRING_URL",
            "UBUNTU_ARCHIVE_KEYRING_SHA256",
            "UBUNTU_SNAPSHOT_ARCHIVE_URL",
            "UBUNTU_SOURCE_INDEXES",
            "_UBUNTU_POCKET_SUITES",
            "_UBUNTU_COMPONENTS",
            "_LAUNCHPAD_ARCHIVE_API",
            "_LAUNCHPAD_SERIES_API",
            "_GENERATED_OUTPUT_ENTRIES",
            "_KNOWN_PACKAGE_CONTROL_ACTIONS",
            "PROJECT_SOURCE_PATHS",
            "GUEST_RELEASE_NAMES",
            "_ZIP_TIMESTAMP",
        )
        for module in (
            build,
            build_config,
            common,
            ubuntu,
            ci,
            collect_alpine_sources,
            collect_ubuntu_sources,
            release,
            benchmark,
            archive,
        ):
            with self.subTest(module=module.__name__):
                self.assertFalse(set(names).intersection(vars(module)))

    def test_guest_descriptors_and_source_manifest_share_build_pins(self):
        manifest = json.loads(
            (BuildConstants.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(KernelBuildConstants.VERSION, manifest["linux"]["version"])
        self.assertEqual(KernelBuildConstants.URL, manifest["linux"]["upstream_url"])
        self.assertEqual(
            KernelBuildConstants.SHA256,
            manifest["linux"]["upstream_archive_sha256"],
        )
        self.assertEqual(
            AlpineBuildConstants.MINIROOTFS_URL, manifest["alpine"]["minirootfs_url"]
        )
        self.assertEqual(
            AlpineBuildConstants.MINIROOTFS_SHA256,
            manifest["alpine"]["minirootfs_sha256"],
        )
        for descriptor in (guests.ALPINE_GUEST, guests.UBUNTU_GUEST):
            with self.subTest(guest=descriptor.name):
                pinned = manifest[descriptor.name]
                self.assertEqual(descriptor.release, pinned["version"])
                self.assertEqual(descriptor.architecture, pinned["architecture"])
                self.assertEqual(
                    f"build/{descriptor.package_manifest_name}",
                    pinned["package_manifests"][0],
                )
        self.assertEqual(
            OpenVMMBuildConstants.MICROVM_ABI_VERSION,
            manifest["openvmm"]["microvm_abi_version"],
        )
        self.assertEqual(
            OpenVMMBuildConstants.CONTROL_SESSION_PROTOCOL_VERSION,
            manifest["openvmm"]["control_session_protocol_version"],
        )
        self.assertEqual(
            OpenVMMBuildConstants.CONTROL_CONTRACT_REVISION,
            manifest["openvmm"]["control_contract_revision"],
        )
        self.assertEqual(
            manifest["guest_rust"],
            {
                "toolchain": GuestRustBuildConstants.TOOLCHAIN,
                "target": GuestRustBuildConstants.TARGET,
            },
        )
        for guest in ("alpine", "ubuntu", "azurelinux"):
            with self.subTest(guest=guest):
                self.assertTrue(
                    set(GuestRustBuildConstants.SOURCE_PATHS).issubset(
                        manifest[guest]["guest_sources"]
                    )
                )

    def test_cache_defaults_are_resolved_for_each_configuration(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for configured in ("", str(root / "first"), str(root / "second")):
                with (
                    self.subTest(configured=configured),
                    patch.dict(os.environ, {"NVX_CACHE_DIR": configured}),
                ):
                    expected = (
                        Path(configured)
                        if configured
                        else BuildConstants.REPO_ROOT / ".cache"
                    ).resolve()
                    self.assertEqual(
                        build_config.KernelBuildConfig().cache_directory, expected
                    )
                    self.assertEqual(common.cache_root(), expected)
                    self.assertEqual(
                        build_config.KernelBuildConfig(
                            cache_directory=root / "explicit"
                        ).cache_directory,
                        root / "explicit",
                    )

    def test_openvmm_executable_default_is_resolved_at_configuration_time(self):
        for host, binary in (("nt", "openvmm.exe"), ("posix", "openvmm")):
            expected = OpenVMMBuildConstants.DIRECTORY / "target" / "release" / binary
            with self.subTest(host=host), patch.object(common.os, "name", host):
                self.assertEqual(common.openvmm_binary_path(), expected)
                self.assertEqual(build_config.OpenVmmBuildConfig().output, expected)

    def test_initramfs_provenance_tracks_constants_source(self):
        relative = "scripts/nvx_tools/build_constants.py"
        source = BuildConstants.REPO_ROOT / relative
        original = build.initramfs_provenance_inputs()
        records = cast(list[dict[str, str]], original["source_files"])
        self.assertEqual(
            [record for record in records if record["path"] == relative],
            [{"path": relative, "sha256": common.normalized_sha256_file(source)}],
        )

        def changed_sha256(path: Path) -> str:
            return "f" * 64 if path == source else common.normalized_sha256_file(path)

        with patch.object(build, "normalized_sha256_file", side_effect=changed_sha256):
            changed = build.initramfs_provenance_inputs()
        self.assertNotEqual(original, changed)
        self.assertEqual(original["alpine"], changed["alpine"])

    def test_constants_are_in_docker_and_release_source_inputs(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertIn("COPY scripts/ /repo/scripts/", dockerfile)
        self.assertIn("scripts", ReleaseBuildConstants.PROJECT_SOURCE_PATHS)
        self.assertIn("nvx", ReleaseBuildConstants.PROJECT_SOURCE_PATHS)
        self.assertIn(
            Path("nvx") / "target",
            ReleaseBuildConstants.PROJECT_SOURCE_EXCLUDES,
        )
        self.assertTrue(
            (
                BuildConstants.REPO_ROOT
                / "scripts"
                / "nvx_tools"
                / "build_constants.py"
            ).is_file()
        )

    def test_project_source_paths_exist_and_are_tracked(self):
        for relative in ReleaseBuildConstants.PROJECT_SOURCE_PATHS:
            with self.subTest(path=relative):
                self.assertTrue((BuildConstants.REPO_ROOT / relative).exists())
                tracked = subprocess.run(
                    ["git", "ls-files", "--error-unmatch", "--", relative],
                    cwd=BuildConstants.REPO_ROOT,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(tracked.returncode, 0, tracked.stderr)

    def test_no_c_or_assembler_sources_remain_outside_openvmm(self):
        tracked = subprocess.run(
            ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
            cwd=BuildConstants.REPO_ROOT,
            capture_output=True,
            check=True,
        ).stdout.split(b"\0")
        forbidden = sorted(
            path.decode("utf-8")
            for path in tracked
            if path
            and (BuildConstants.REPO_ROOT / path.decode("utf-8")).is_file()
            and not path.startswith(b"openvmm/")
            and PurePosixPath(path.decode("utf-8")).suffix.lower() in {".c", ".h", ".s"}
        )
        self.assertEqual(forbidden, [])

    def test_guest_binary_third_party_notices_cover_the_rust_runtime(self):
        notices = (BuildConstants.REPO_ROOT / "THIRD_PARTY_NOTICES.md").read_text(
            encoding="utf-8"
        )
        musl = next(
            line for line in notices.splitlines() if line.startswith("| musl libc |")
        )
        self.assertEqual(
            set(re.findall(r"`/sbin/(nvx-[a-z-]+)`", musl)),
            set(GuestRustBuildConstants.BINARIES),
        )
        for component in (
            "Rust core and standard libraries",
            "Rust `compiler_builtins`",
            "Rust `libc` crate",
            "Rust `nix` crate 0.31.3",
            "Rust `getrandom` crate 0.4.3",
            "Rust `bitflags` crate 2.13.2",
            "Rust `cfg-if` crate 1.0.5",
            "Rust `memoffset` crate 0.9.1",
            "Apache-2.0 WITH LLVM-exception",
        ):
            with self.subTest(component=component):
                self.assertIn(component, notices)

    def test_nvx_third_party_notices_cover_every_linked_crate(self):
        # The crates of `cargo tree --locked --features cli -e normal` for the
        # release targets, x86_64-unknown-linux-musl and x86_64-pc-windows-msvc:
        # what bin/nvx[.exe] links, and serde_derive/document-features with
        # the crates that they run on while nvx builds.
        shipped = {
            "bitflags",
            "block-buffer",
            "cfg-if",
            "cpufeatures",
            "crossterm",
            "crossterm_winapi",
            "crypto-common",
            "digest",
            "document-features",
            "generic-array",
            "getrandom",
            "itoa",
            "libc",
            "linux-raw-sys",
            "litrs",
            "lock_api",
            "memchr",
            "memoffset",
            "nix",
            "parking_lot",
            "parking_lot_core",
            "proc-macro2",
            "quote",
            "rustix",
            "scopeguard",
            "serde",
            "serde_core",
            "serde_derive",
            "serde_json",
            "sha2",
            "smallvec",
            "syn",
            "typenum",
            "unicode-ident",
            "winapi",
            "windows-link",
            "windows-sys",
            "zmij",
        }
        # Build scripts, other targets, and the tests, such as proptest and the
        # crates that it brings.
        unshipped = {
            "autocfg",
            "cfg_aliases",
            "chacha20",
            "core_detect",
            "errno",
            "fastrand",
            "num-traits",
            "once_cell",
            "proptest",
            "r-efi",
            "rand",
            "rand_core",
            "rand_xorshift",
            "redox_syscall",
            "regex-syntax",
            "tempfile",
            "unarray",
            "version_check",
            "winapi-i686-pc-windows-gnu",
            "winapi-x86_64-pc-windows-gnu",
        }
        # Versions of shipped crates that only the tests use: proptest's
        # chacha20 takes cpufeatures 0.3, and sha2 the cpufeatures that ships.
        unshipped_versions = {("cpufeatures", "0.3.1")}
        lock = (BuildConstants.REPO_ROOT / "nvx" / "Cargo.lock").read_text(
            encoding="utf-8"
        )
        packages = re.findall(
            r'^\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"$',
            lock,
            re.MULTILINE,
        )
        # A new dependency is one or the other, and a shipped one needs notices.
        self.assertEqual({name for name, _ in packages} - {"nvx"}, shipped | unshipped)
        self.assertFalse(shipped & unshipped)
        self.assertLessEqual(unshipped_versions, set(packages))
        notices = (BuildConstants.REPO_ROOT / "THIRD_PARTY_NOTICES.md").read_text(
            encoding="utf-8"
        )
        rows = {
            line.split(" | ", 1)[0]: line
            for line in notices.splitlines()
            if line.startswith("| ")
        }
        for name in sorted(shipped):
            with self.subTest(crate=name):
                # Each shipped crate ships one version, whose notices name it.
                versions = [
                    version
                    for crate, version in packages
                    if crate == name and (crate, version) not in unshipped_versions
                ]
                self.assertEqual(len(versions), 1, versions)
                row = rows[f"| Rust `{name}` crate {versions[0]}"]
                self.assertIn("bin/nvx", row)
                self.assertIn("nvx/Cargo.lock", row)
        # The runtimes that the release executables link statically.
        for component, location in (
            ("| Rust core and standard libraries", "`bin/nvx`"),
            ("| Rust `compiler_builtins`", "`bin/nvx[.exe]`"),
            ("| musl libc", "`bin/nvx` of the `linux-kvm` and `linux-mshv`"),
            ("| LLVM `libunwind`", "the Linux `bin/nvx`"),
            (
                "| Microsoft C runtime: the Visual C++ runtime and the Universal C "
                "Runtime",
                "`bin/nvx.exe`",
            ),
        ):
            with self.subTest(component=component):
                self.assertIn(location, rows[component])

    def test_nvx_ships_the_repository_license(self):
        # A published crate holds only the files under its directory, so
        # nvx/ keeps a copy of the repository's license. The checkout
        # writes nvx/ with LF line endings on every platform and the
        # repository's license with the platform's (.gitattributes), so
        # compare the text.
        root = BuildConstants.REPO_ROOT
        self.assertEqual(
            (root / "nvx" / "LICENSE").read_bytes().replace(b"\r\n", b"\n"),
            (root / "LICENSE").read_bytes().replace(b"\r\n", b"\n"),
        )

    def test_project_source_archive_excludes_the_nvx_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            crate = root / "nvx"
            (crate / "src").mkdir(parents=True)
            (crate / "src" / "lib.rs").write_text("source\n", encoding="utf-8")
            (crate / "target" / "debug").mkdir(parents=True)
            (crate / "target" / "debug" / "nvx").write_text(
                "build output\n", encoding="utf-8"
            )
            output = root / "source.tar.gz"
            archive.create_reproducible_tar_gz(
                output,
                [(crate, "nvx-project-source/nvx")],
            )
            with tarfile.open(output, "r:gz") as source_archive:
                names = source_archive.getnames()

        self.assertIn("nvx-project-source/nvx/src/lib.rs", names)
        self.assertFalse(any("/target" in name for name in names))

    def test_constants_consumers_import_in_either_order(self):
        modules = (
            "nvx_tools.build_constants",
            "nvx_tools.guests",
            "nvx_tools.common",
            "nvx_tools.build_config",
            "nvx_tools.build",
            "nvx_tools.release",
            "nvx",
        )
        for ordered in (modules, tuple(reversed(modules))):
            with self.subTest(first=ordered[0]):
                result = subprocess.run(
                    [
                        sys.executable,
                        "-c",
                        "import importlib, sys; "
                        "[importlib.import_module(name) for name in sys.argv[1:]]",
                        *ordered,
                    ],
                    cwd=BuildConstants.REPO_ROOT / "scripts",
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()

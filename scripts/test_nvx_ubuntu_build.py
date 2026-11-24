#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from collections.abc import Mapping, Sequence
from pathlib import Path, PurePosixPath
from typing import cast
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import build, build_config, common, ubuntu  # noqa: E402
from nvx_tools.build_constants import BuildConstants, UbuntuBuildConstants  # noqa: E402


class UbuntuBuildTests(unittest.TestCase):
    def test_ubuntu_input_digest_orders_paths_alike_on_every_os(self):
        # Linux hosts build the Ubuntu artifacts, and every host checks their
        # manifests, so the digest must take the files in the order of POSIX
        # paths: by component, with case. Windows paths compare without case,
        # which put build.rs before Cargo.toml.
        names = (
            "nvx/build.rs",
            "nvx/Cargo.toml",
            "nvx/src/cli.rs",
            "nvx/src/cli/args.rs",
            "guest/common/init",
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            files = [root / name for name in names]
            for path in files:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(path.name.encode())
            with (
                patch.object(BuildConstants, "REPO_ROOT", root),
                patch.object(ubuntu, "load_package_lock", return_value=()),
                patch.object(ubuntu.json, "dumps", wraps=ubuntu.json.dumps) as dumps,
            ):
                forward = ubuntu.converter_input_sha256(files)
                backward = ubuntu.converter_input_sha256(files[::-1])

        self.assertEqual(forward, backward)
        document = cast(dict[str, object], dumps.call_args.args[0])
        self.assertEqual(
            [
                cast(dict[str, str], entry)["path"]
                for entry in cast(list[object], document["customization_files"])
            ],
            [
                "guest/common/init",
                "nvx/Cargo.toml",
                "nvx/build.rs",
                "nvx/src/cli/args.rs",
                "nvx/src/cli.rs",
            ],
        )

    def test_ubuntu_manifest_and_package_lock_match_build_pins(self):
        manifest = json.loads(
            (BuildConstants.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(
                encoding="utf-8"
            )
        )
        ubuntu_manifest = manifest["ubuntu"]
        self.assertEqual(
            ubuntu_manifest["version"],
            UbuntuBuildConstants.VERSION,
        )
        self.assertEqual(
            ubuntu_manifest["base_url"],
            UbuntuBuildConstants.BASE_URL,
        )
        self.assertEqual(
            ubuntu_manifest["base_sha256"],
            UbuntuBuildConstants.BASE_SHA256,
        )
        self.assertEqual(
            ubuntu_manifest["archive_keyring_url"],
            UbuntuBuildConstants.ARCHIVE_KEYRING_URL,
        )
        self.assertEqual(
            ubuntu_manifest["archive_keyring_sha256"],
            UbuntuBuildConstants.ARCHIVE_KEYRING_SHA256,
        )
        self.assertEqual(
            ubuntu_manifest["package_lock_sha256"],
            ubuntu.package_lock_sha256(),
        )
        self.assertEqual(
            ubuntu_manifest["package_lock_sha256"],
            "fcdd30223b96fc26e24fcf4b6763a4a2504e9cd25882e68e89fbf6bf45277ef0",
        )
        packages = ubuntu.load_package_lock()
        self.assertEqual(
            [package["name"] for package in packages],
            [
                "busybox-static",
                "iputils-ping",
                "libcap2",
                "libidn2-0",
                "libunistring5",
                "net-tools",
                "netcat-openbsd",
            ],
        )

    def test_ubuntu_package_lock_read_errors_are_actionable(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "packages.json"
            path.write_text("{", encoding="utf-8")

            with self.assertRaisesRegex(
                common.ScriptError, "failed to read Ubuntu package lock"
            ):
                ubuntu.load_package_lock(path)

    def test_ubuntu_safe_extractor_rejects_archive_symlink_escape(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive_path = root / "unsafe.tar.gz"
            with tarfile.open(archive_path, "w:gz") as archive_file:
                symlink = tarfile.TarInfo("etc")
                symlink.type = tarfile.SYMTYPE
                symlink.linkname = "../../outside"
                archive_file.addfile(symlink)
                payload = b"nameserver 192.0.2.1\n"
                resolver = tarfile.TarInfo("etc/resolv.conf")
                resolver.size = len(payload)
                archive_file.addfile(resolver, io.BytesIO(payload))

            with self.assertRaisesRegex(common.ScriptError, "escapes"):
                ubuntu.safe_extract_tar(
                    archive_path,
                    root / "extracted",
                    label="test archive",
                )
            self.assertFalse((root / "outside").exists())

    def test_ubuntu_safe_extractor_roots_absolute_symlinks(self):
        self.assertEqual(
            ubuntu._virtual_symlink_target(
                PurePosixPath("var/run"),
                "/run",
                "test archive",
            ),
            "../run",
        )
        self.assertEqual(
            ubuntu._virtual_symlink_target(
                PurePosixPath("etc/alternatives/awk"),
                "/usr/bin/mawk",
                "test archive",
            ),
            "../../usr/bin/mawk",
        )

    def test_ubuntu_safe_extractor_rejects_absolute_and_parent_paths(self):
        for member_name in ("/absolute", "../parent"):
            with self.subTest(member_name=member_name):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    archive_path = root / "unsafe.tar"
                    with tarfile.open(archive_path, "w") as archive_file:
                        payload = b"x"
                        member = tarfile.TarInfo(member_name)
                        member.size = len(payload)
                        archive_file.addfile(member, io.BytesIO(payload))
                    with self.assertRaisesRegex(
                        common.ScriptError,
                        "absolute path|path traversal",
                    ):
                        ubuntu.safe_extract_tar(
                            archive_path,
                            root / "extracted",
                            label="test archive",
                        )

    def test_ubuntu_rootfs_digest_and_erofs_uuid_are_deterministic(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = root / "first"
            second = root / "second"
            for directory in (first, second):
                (directory / "etc").mkdir(parents=True)
                (directory / "etc" / "os-release").write_text(
                    "ID=ubuntu\n",
                    encoding="ascii",
                )
            os.utime(second / "etc" / "os-release", (1234, 1234))

            first_digest = ubuntu.rootfs_sha256(first)
            second_digest = ubuntu.rootfs_sha256(second)
            self.assertEqual(first_digest, second_digest)
            self.assertEqual(
                ubuntu.erofs_uuid(first_digest),
                ubuntu.erofs_uuid(second_digest),
            )

    def test_ubuntu_prepare_root_normalizes_root_directory_mode(self):
        with tempfile.TemporaryDirectory() as temporary:
            temporary_root = Path(temporary)
            work = temporary_root / "work"
            expected_root = work / "root"

            def extract(
                archive: Path,
                destination: Path,
                *,
                label: str,
            ) -> None:
                self.assertEqual(
                    archive.name,
                    f"ubuntu-base-{UbuntuBuildConstants.VERSION}-base-amd64.tar.gz",
                )
                self.assertEqual(label, "Ubuntu Base archive")
                status = destination / "var" / "lib" / "dpkg" / "status"
                status.parent.mkdir(parents=True)
                status.write_text("", encoding="utf-8")

            with (
                patch.object(
                    ubuntu,
                    "cache_root",
                    return_value=temporary_root / "cache",
                ),
                patch.object(ubuntu, "download_verified"),
                patch.object(ubuntu, "safe_extract_tar", side_effect=extract),
                patch.object(ubuntu, "_validate_ubuntu_identity"),
                patch.object(ubuntu, "load_package_lock", return_value=()),
                patch.object(ubuntu, "_validate_package_closure"),
                patch.object(ubuntu, "require_posix_permissions") as permissions,
                patch.object(ubuntu, "precompile_python") as precompile,
                patch.object(ubuntu, "_customize_root"),
                patch.object(ubuntu.Path, "chmod", autospec=True) as chmod,
            ):
                self.assertEqual(ubuntu.prepare_root(work), expected_root)

            chmod.assert_called_once_with(expected_root, 0o755)
            permissions.assert_called_once_with(work)
            precompile.assert_called_once_with(expected_root)

    def test_ubuntu_customization_installs_busybox_wget(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for relative in ("etc", "usr/bin", "usr/sbin"):
                (root / relative).mkdir(parents=True)
            with (
                patch.object(ubuntu, "_clear_directory"),
                patch.object(ubuntu, "_validate_accounts"),
                patch.object(ubuntu, "_validate_usr_merge"),
                patch.object(ubuntu, "_ensure_symlink") as ensure_symlink,
            ):
                ubuntu._customize_root(root)

            ensure_symlink.assert_any_call(root, "usr/bin/wget", "busybox")

    def test_ubuntu_customization_keeps_wget_installed_by_a_package(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            root = directory / "root"
            (root / "etc").mkdir(parents=True)
            executable = b"#!/bin/sh\nprintf 'package wget\\n'\n"
            package, archive = self._ubuntu_deb(
                directory, "wget", (), {"usr/bin/wget": executable}
            )
            ubuntu._install_deb(root, package, archive)
            wget = root / "usr/bin/wget"
            wget.chmod(0o755)
            before = wget.stat().st_mode
            with (
                patch.object(ubuntu, "_validate_accounts"),
                patch.object(ubuntu, "_validate_usr_merge"),
            ):
                ubuntu._customize_root(root)

            self.assertFalse(wget.is_symlink())
            self.assertEqual(wget.read_bytes(), executable)
            self.assertEqual(wget.stat().st_mode, before)
            self.assertIn(
                "/usr/bin/wget",
                (root / "var/lib/dpkg/info/wget.list").read_text(encoding="utf-8"),
            )

    def test_ubuntu_customization_keeps_an_existing_wget_link(self):
        for target in ("busybox", "wget.real"):
            with self.subTest(target=target):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    (root / "etc").mkdir()
                    (root / "usr/bin").mkdir(parents=True)
                    wget = root / "usr/bin/wget"
                    # Preserve the link itself, even before its target is installed.
                    wget.symlink_to(target)
                    with (
                        patch.object(ubuntu, "_validate_accounts"),
                        patch.object(ubuntu, "_validate_usr_merge"),
                    ):
                        ubuntu._customize_root(root)

                    self.assertTrue(wget.is_symlink())
                    self.assertEqual(os.readlink(wget), target)

    def test_ubuntu_package_lock_accepts_architecture_independent_packages(self):
        document = json.loads(UbuntuBuildConstants.PACKAGE_LOCK.read_text("utf-8"))
        package = dict(document["packages"][0])
        package.update(
            name="zz-data",
            architecture="all",
            url=(
                "https://archive.ubuntu.com/ubuntu/pool/main/z/zz-data/"
                "zz-data_1_all.deb"
            ),
            license_path="/usr/share/doc/zz-data/copyright",
        )
        document["packages"].append(package)
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "packages.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            packages = ubuntu.load_package_lock(path)
            self.assertEqual(packages[-1]["name"], "zz-data")
            self.assertEqual(packages[-1]["architecture"], "all")

            package["architecture"] = "arm64"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(
                common.ScriptError, "unsupported architecture arm64"
            ):
                ubuntu.load_package_lock(path)

    def test_ubuntu_customization_links_python_only_when_python3_exists(self):
        for has_python3 in (True, False):
            with self.subTest(has_python3=has_python3):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    for relative in ("etc", "usr/bin", "usr/sbin"):
                        (root / relative).mkdir(parents=True)
                    if has_python3:
                        (root / "usr" / "bin" / "python3").touch()
                    with (
                        patch.object(ubuntu, "_clear_directory"),
                        patch.object(ubuntu, "_validate_accounts"),
                        patch.object(ubuntu, "_validate_usr_merge"),
                        patch.object(ubuntu, "_ensure_symlink") as ensure_symlink,
                    ):
                        ubuntu._customize_root(root)

                    linked = (
                        call(root, "usr/bin/python", "python3")
                        in ensure_symlink.call_args_list
                    )
                    self.assertEqual(linked, has_python3)

    @staticmethod
    def _python_tree(root: Path, library_name: str) -> Path:
        library = root / "usr" / "lib" / library_name
        (library / "package").mkdir(parents=True)
        (library / "os.py").write_text("PATH = 'os'\n", encoding="utf-8")
        (library / "package" / "__init__.py").write_text(
            "def value():\n    return 1\n", encoding="utf-8"
        )
        return library

    def test_python_precompile_is_deterministic_unchecked_hash_bytecode(self):
        if sys.version_info.releaselevel != "final":
            self.skipTest("byte-compiling needs a final-release interpreter")
        name = f"python{sys.version_info.major}.{sys.version_info.minor}"
        runs: list[dict[str, bytes]] = []
        roots: list[str] = []
        for _ in range(2):
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                roots.append(str(root))
                library = self._python_tree(root, name)
                ubuntu.precompile_python(root)
                runs.append(
                    {
                        path.relative_to(library).as_posix(): path.read_bytes()
                        for path in sorted(library.rglob("*.pyc"))
                    }
                )
        self.assertEqual(len(runs[0]), 2)
        self.assertEqual(runs[0], runs[1])
        for relative, data in runs[0].items():
            with self.subTest(relative=relative):
                # Bytes 4..8 are the flags: 0b01 is a hash-based file that is
                # not checked against its source.
                self.assertEqual(int.from_bytes(data[4:8], "little"), 0b01)
        # The recorded source path is the one inside the guest, never the
        # build directory.
        os_module = runs[0][f"__pycache__/os.{sys.implementation.cache_tag}.pyc"]
        guest_path = os.path.join(f"/usr/lib/{name}", "os.py")
        self.assertIn(guest_path.encode(), os_module)
        for run, root in zip(runs, roots, strict=True):
            for data in run.values():
                self.assertNotIn(root.encode(), data)

    def _precompile_in_subprocess(
        self, root: Path, *interpreter_flags: str, env: Mapping[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        code = (
            "import sys; from pathlib import Path; "
            "from nvx_tools import ubuntu; "
            "ubuntu.precompile_python(Path(sys.argv[1]))"
        )
        scripts = Path(__file__).resolve().parent
        environment = {**os.environ, "PYTHONPATH": str(scripts), **(env or {})}
        return subprocess.run(
            [sys.executable, *interpreter_flags, "-c", code, str(root)],
            capture_output=True,
            text=True,
            env=environment,
        )

    def test_python_precompile_ignores_the_build_optimization_level(self):
        if sys.version_info.releaselevel != "final":
            self.skipTest("byte-compiling needs a final-release interpreter")
        name = f"python{sys.version_info.major}.{sys.version_info.minor}"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            library = self._python_tree(root, name)
            result = self._precompile_in_subprocess(root, "-O")
            self.assertEqual(result.returncode, 0, result.stderr)
            names = sorted(path.name for path in library.rglob("*.pyc"))
        tag = sys.implementation.cache_tag
        self.assertEqual(names, [f"__init__.{tag}.pyc", f"os.{tag}.pyc"])

    def test_python_precompile_ignores_the_build_cache_prefix(self):
        if sys.version_info.releaselevel != "final":
            self.skipTest("byte-compiling needs a final-release interpreter")
        name = f"python{sys.version_info.major}.{sys.version_info.minor}"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "root"
            prefix = Path(temporary) / "host-cache"
            library = self._python_tree(root, name)
            result = self._precompile_in_subprocess(
                root, env={"PYTHONPYCACHEPREFIX": str(prefix)}
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(list(library.rglob("*.pyc"))), 2)
            # The interpreter that runs this build imports its own modules
            # under the prefix; only the root's modules, which the prefix
            # mirrors under the root's own path, must stay out of it.
            leaked = [
                p
                for p in prefix.rglob("*.pyc")
                if Path(temporary).name in p.relative_to(prefix).parts
            ]
            self.assertEqual(leaked, [])

    @unittest.skipIf(sys.version_info < (3, 11), "debug ranges start in Python 3.11")
    def test_python_precompile_refuses_suppressed_debug_ranges(self):
        if sys.version_info.releaselevel != "final":
            self.skipTest("byte-compiling needs a final-release interpreter")
        name = f"python{sys.version_info.major}.{sys.version_info.minor}"
        for flags, env in (
            (("-X", "no_debug_ranges"), {}),
            ((), {"PYTHONNODEBUGRANGES": "1"}),
        ):
            with (
                self.subTest(flags=flags, env=env),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                library = self._python_tree(root, name)
                result = self._precompile_in_subprocess(root, *flags, env=env)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "needs the build interpreter's debug ranges", result.stderr
                )
                self.assertEqual(list(library.rglob("*.pyc")), [])

    def test_python_precompile_needs_the_matching_interpreter(self):
        minor = sys.version_info.minor + 1
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self._python_tree(root, f"python3.{minor}")
            with self.assertRaisesRegex(
                common.ScriptError,
                f"needs a final-release Python 3.{minor} build interpreter; .*"
                f"build the Ubuntu root natively with Python 3.{minor}$",
            ):
                ubuntu.precompile_python(root)

    def test_python_precompile_ignores_a_root_without_python(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            ubuntu.precompile_python(root)
            (root / "usr" / "lib").mkdir(parents=True)
            ubuntu.precompile_python(root)
            self.assertEqual(sorted(root.rglob("*.pyc")), [])

    @staticmethod
    def _ubuntu_deb(
        directory: Path,
        name: str,
        actions: Sequence[str],
        files: Mapping[str, bytes],
    ) -> tuple[ubuntu.UbuntuLockedPackage, Path]:
        def tar(members: Mapping[str, tuple[bytes, int]]) -> bytes:
            buffer = io.BytesIO()
            with tarfile.open(fileobj=buffer, mode="w") as archive:
                for member, (content, mode) in sorted(members.items()):
                    info = tarfile.TarInfo(f"./{member}")
                    info.size = len(content)
                    info.mode = mode
                    archive.addfile(info, io.BytesIO(content))
            return buffer.getvalue()

        control = f"Package: {name}\nVersion: 1\nArchitecture: amd64\n".encode()
        control_files = {"control": (control, 0o644)} | {
            action: (f"# {name} {action}\n".encode(), 0o755) for action in actions
        }
        data = {path: (content, 0o644) for path, content in files.items()}
        deb = bytearray(b"!<arch>\n")
        for member, content in (
            ("debian-binary", b"2.0\n"),
            ("control.tar", tar(control_files)),
            ("data.tar", tar(data)),
        ):
            deb += f"{member:<16}{0:<12}{0:<6}{0:<6}{100644:<8}{len(content):<10}`\n".encode()
            deb += content + b"\n" * (len(content) % 2)
        path = directory / f"{name}_1_amd64.deb"
        path.write_bytes(deb)
        package: ubuntu.UbuntuLockedPackage = {
            "name": name,
            "source_name": name,
            "source_version": "1",
            "version": "1",
            "architecture": "amd64",
            "url": f"https://archive.ubuntu.com/ubuntu/pool/main/p/{name}/{path.name}",
            "sha256": hashlib.sha256(deb).hexdigest(),
            "license_path": f"/usr/share/doc/{name}/copyright",
            "required_by": ["python3"],
            "depends": "",
            "pre_depends": "",
            "component": "main",
            "suite": UbuntuBuildConstants.CODENAME,
        }
        return package, path

    def test_ubuntu_prepare_root_byte_compiles_the_python_that_it_installs(self):
        if sys.version_info.releaselevel != "final":
            self.skipTest("byte-compiling needs a final-release interpreter")
        library = f"usr/lib/python{sys.version_info.major}.{sys.version_info.minor}"
        # Ubuntu's python3 and the packages that it adds to Ubuntu Base, with
        # the maintainer actions of their Ubuntu 26.04 builds. They install the
        # library of the interpreter that runs this test, the only one that
        # precompile_python can compile.
        packages: dict[str, tuple[tuple[str, ...], dict[str, bytes]]] = {
            "libexpat1": (("triggers",), {}),
            "libffi8": (("triggers",), {}),
            "libpython3-stdlib": ((), {}),
            "libpython3.14-minimal": (
                ("postinst", "postrm", "prerm"),
                {f"{library}/os.py": b"PATH = 'os'\n"},
            ),
            "libpython3.14-stdlib": (
                ("prerm",),
                {f"{library}/json/__init__.py": b"def loads():\n    return 1\n"},
            ),
            "libreadline8t64": (("postrm", "preinst", "triggers"), {}),
            "libsqlite3-0": (("triggers",), {}),
            "media-types": ((), {}),
            "netbase": (("postinst", "postrm"), {}),
            "python3": (("postinst", "postrm", "preinst", "prerm"), {}),
            "python3-minimal": (("postinst", "prerm"), {}),
            "python3.14": (("postinst", "prerm"), {}),
            "python3.14-minimal": (
                ("postinst", "postrm", "preinst", "prerm"),
                {"usr/bin/python3.14": b""},
            ),
            "readline-common": (("postinst", "postrm"), {}),
            "tzdata": (("postinst", "postrm"), {}),
        }
        with tempfile.TemporaryDirectory() as temporary:
            temporary_root = Path(temporary)
            cache = temporary_root / "cache"
            downloads = (
                cache
                / BuildConstants.DOWNLOAD_DIRECTORY_NAME
                / UbuntuBuildConstants.PACKAGE_CACHE_DIRECTORY_NAME
            )
            downloads.mkdir(parents=True)
            lock = tuple(
                self._ubuntu_deb(downloads, name, actions, files)[0]
                for name, (actions, files) in sorted(packages.items())
            )

            def extract(
                archive: Path,
                destination: Path,
                *,
                label: str,
            ) -> None:
                status = destination / "var" / "lib" / "dpkg" / "status"
                status.parent.mkdir(parents=True)
                status.write_text("", encoding="utf-8")

            with (
                patch.object(ubuntu, "cache_root", return_value=cache),
                patch.object(ubuntu, "download_verified"),
                patch.object(ubuntu, "safe_extract_tar", side_effect=extract),
                patch.object(ubuntu, "_validate_ubuntu_identity"),
                patch.object(ubuntu, "load_package_lock", return_value=lock),
                patch.object(ubuntu, "_customize_root"),
            ):
                root = ubuntu.prepare_root(temporary_root / "work")

            tag = sys.implementation.cache_tag
            self.assertEqual(
                sorted(
                    path.relative_to(root / library).as_posix()
                    for path in (root / library).rglob("*.pyc")
                ),
                [f"__pycache__/os.{tag}.pyc", f"json/__pycache__/__init__.{tag}.pyc"],
            )
            info = root / "var" / "lib" / "dpkg" / "info"
            self.assertTrue((info / "python3.14-minimal.postinst").is_file())
            installed = [
                record["Package"]
                for record in ubuntu.parse_deb822(
                    (info.parent / "status").read_text(encoding="utf-8")
                )
            ]
            self.assertEqual(installed, sorted(packages))

    def test_ubuntu_install_refuses_unreviewed_python_maintainer_actions(self):
        reviewed = UbuntuBuildConstants.KNOWN_PACKAGE_CONTROL_ACTIONS[
            "python3.14-minimal"
        ]
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            root = directory / "root"
            status = root / "var" / "lib" / "dpkg" / "status"
            status.parent.mkdir(parents=True)
            status.write_text("", encoding="utf-8")
            for actions, difference in (
                ((*sorted(reviewed), "triggers"), "triggers"),
                (sorted(reviewed - {"preinst"}), "preinst"),
            ):
                with self.subTest(difference=difference):
                    package, path = self._ubuntu_deb(
                        directory, "python3.14-minimal", actions, {}
                    )
                    with self.assertRaisesRegex(
                        common.ScriptError,
                        f"has unsupported maintainer actions: {difference}$",
                    ):
                        ubuntu._install_deb(root, package, path)

    def test_posix_permission_check_follows_the_filesystem(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            with patch.object(ubuntu.sys, "platform", "win32"):
                ubuntu.require_posix_permissions(work / "never-created")

            with (
                patch.object(ubuntu.sys, "platform", "linux"),
                patch.object(ubuntu.stat, "S_IMODE", return_value=0o777),
                self.assertRaisesRegex(
                    common.ScriptError, "does not preserve POSIX permissions"
                ),
            ):
                ubuntu.require_posix_permissions(work)
            self.assertEqual(list(work.iterdir()), [])

            if os.name == "posix":
                with patch.object(ubuntu.sys, "platform", "linux"):
                    ubuntu.require_posix_permissions(work)

    def test_distro_layer_refuses_existing_output_without_replace(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "ubuntu.erofs"
            output.touch()
            with (
                patch.object(build.sys, "platform", "linux"),
                self.assertRaisesRegex(common.ScriptError, "refusing to replace"),
            ):
                build.build_distro_layer(
                    build_config.DistroLayerBuildConfig(output=output)
                )

    def test_ubuntu_initramfs_manifest_binds_artifact_digest(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "initramfs-ubuntu.cpio.gz"
            output.write_bytes(b"ubuntu initramfs")
            with patch.object(
                ubuntu,
                "package_manifest",
                return_value={"format": 1, "guest": "ubuntu"},
            ):
                build._write_ubuntu_manifest(root, output, {}, "a" * 64)

            manifest = json.loads(
                output.with_name(f"{output.name}.packages.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(manifest["artifact"], output.name)
            self.assertEqual(
                manifest["artifact_sha256"],
                common.sha256_file(output),
            )
            self.assertEqual(manifest["input_sha256"], "a" * 64)

    def test_ubuntu_initramfs_rejects_source_mutation_during_pack(self):
        # The manifest must describe the sources the build actually consumed,
        # so an edit made while the root is packed must fail the build.
        for mutate in (False, True):
            with (
                self.subTest(mutate=mutate),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                source = root / "nvx-bashrc"
                source.write_bytes(b"old\n")
                output = root / "output" / "initramfs-ubuntu.cpio.gz"
                output.parent.mkdir()

                def digest(files: tuple[Path, ...]) -> str:
                    content = b"".join(Path(path).read_bytes() for path in files)
                    return hashlib.sha256(content).hexdigest()

                def pack(
                    _root: Path,
                    archive: Path,
                    mutate: bool = mutate,
                    source: Path = source,
                ) -> None:
                    archive.write_bytes(b"old initramfs")
                    if mutate:
                        source.write_bytes(b"new\n")

                with (
                    patch.object(build, "_require_linux"),
                    patch.object(build, "_prepare_guest_root", return_value=root),
                    patch.object(build, "_install_guest_files", return_value={}),
                    patch.object(build, "_pack_initramfs", side_effect=pack),
                    patch.object(ubuntu, "customization_files", return_value=(source,)),
                    patch.object(ubuntu, "converter_input_sha256", side_effect=digest),
                    patch.object(ubuntu, "apply_metadata_policy"),
                    patch.object(
                        ubuntu,
                        "package_manifest",
                        return_value={"format": 1, "guest": "ubuntu"},
                    ),
                ):
                    config = build_config.InitramfsBuildConfig(
                        guest="ubuntu",
                        work=root / "work",
                        output=output,
                    )
                    if mutate:
                        with self.assertRaisesRegex(
                            common.ScriptError,
                            "inputs changed during the build",
                        ):
                            build.build_initramfs(config)
                        self.assertFalse(output.exists())
                        self.assertFalse(
                            output.with_name(f"{output.name}.packages.json").exists()
                        )
                    else:
                        build.build_initramfs(config)
                        manifest = json.loads(
                            output.with_name(f"{output.name}.packages.json").read_text(
                                encoding="utf-8"
                            )
                        )
                        self.assertEqual(
                            manifest["input_sha256"],
                            digest((source,)),
                        )


if __name__ == "__main__":
    unittest.main()

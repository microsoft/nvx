"""OpenVMM, Linux-native, and Docker-backed artifact build workflows."""

from __future__ import annotations

import json
import os
import shlex
import shutil
import ssl
import subprocess
import sys
from pathlib import Path
from typing import TypedDict

from . import ubuntu
from .build_config import (
    BuildConfig,
    DistroLayerBuildConfig,
    DockerBuildConfig,
    InitramfsBuildConfig,
    KernelBuildConfig,
    OpenVmmBackend,
    OpenVmmBuildConfig,
    OpenVmmPlatform,
)
from .build_constants import (
    AlpineBuildConstants,
    BuildConstants,
    DockerBuildConstants,
    InitramfsBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
    UbuntuBuildConstants,
)
from .common import (
    ScriptError,
    artifact_path,
    download_verified,
    format_size,
    require_file,
    require_success,
    require_tool,
    run_capture,
    run_checked,
    sha256_file,
)
from .guests import GuestDescriptor, guest_descriptor


class ApkPackage(TypedDict):
    name: str
    version: str | None
    architecture: str | None
    license: str | None
    origin: str | None
    url: str | None
    description: str | None
    aports_commit: str | None
    build_time: str | None


def _run_openvmm_command(
    args: list[str | os.PathLike[str]],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
) -> None:
    command = [os.fspath(arg) for arg in args]
    print(f">> {shlex.join(command)}")
    if cwd is None and env is None:
        run_checked(command)
    elif env is None:
        run_checked(command, cwd=cwd)
    else:
        run_checked(command, cwd=cwd, env=env)


def _assert_kernel_config(
    path: Path, required: tuple[str, ...], error_prefix: str
) -> None:
    configured = set(path.read_text(encoding="utf-8").splitlines())
    missing = [setting for setting in required if setting not in configured]
    if missing:
        raise ScriptError(error_prefix + ", ".join(missing))


def _assert_virtio_console_kernel_config(path: Path) -> None:
    _assert_kernel_config(
        path,
        KernelBuildConstants.REQUIRED_VIRTIO_CONSOLE_CONFIG,
        "kernel configuration cannot provide /dev/hvc1: ",
    )


def _assert_sandbox_kernel_config(path: Path) -> None:
    _assert_kernel_config(
        path,
        KernelBuildConstants.REQUIRED_SANDBOX_CONFIG,
        "kernel configuration cannot support sandbox workloads: ",
    )


def _assert_shared_status_kernel_config(path: Path) -> None:
    _assert_kernel_config(
        path,
        KernelBuildConstants.REQUIRED_SHARED_STATUS_CONFIG,
        "kernel configuration cannot consume shared virtio interrupt status: ",
    )


def assert_required_kernel_config(path: Path) -> None:
    """Validate the generated configuration required by the NVX platform."""
    _assert_virtio_console_kernel_config(path)
    _assert_sandbox_kernel_config(path)
    _assert_shared_status_kernel_config(path)


def _require_linux(workflow: str) -> None:
    if sys.platform != "linux":
        raise ScriptError(
            f"{workflow} requires Linux; use build-guest without --native for Docker"
        )


def _alpine_tarball(config: InitramfsBuildConfig) -> Path:
    return config.work / AlpineBuildConstants.MINIROOTFS_NAME


def _kernel_patch_files() -> tuple[Path, ...]:
    patches = tuple(
        sorted(
            (BuildConstants.REPO_ROOT / KernelBuildConstants.PATCH_DIRECTORY).glob(
                "*.patch"
            )
        )
    )
    if not patches:
        raise ScriptError("no kernel patches were found")
    return patches


def materialize_kernel_provenance_inputs() -> None:
    """Write kernel provenance inputs from immutable run-head blobs."""
    tracked = run_capture(
        [
            "git",
            "ls-tree",
            "-r",
            "-z",
            "--name-only",
            "HEAD",
            "--",
            KernelBuildConstants.INPUT_CONFIG.as_posix(),
            KernelBuildConstants.PATCH_DIRECTORY.as_posix(),
        ],
        cwd=BuildConstants.REPO_ROOT,
    )
    require_success(tracked, "run-head kernel provenance input query")
    tree_paths = tuple(
        path for path in tracked.stdout.decode("utf-8").split("\0") if path
    )
    config_path = KernelBuildConstants.INPUT_CONFIG.as_posix()
    patch_paths = tuple(
        path
        for path in tree_paths
        if path.startswith(f"{KernelBuildConstants.PATCH_DIRECTORY.as_posix()}/")
        and path.endswith(".patch")
    )
    if config_path not in tree_paths:
        raise ScriptError("kernel config is missing from the run head")
    if not patch_paths:
        raise ScriptError("kernel patches are missing from the run head")

    head_patch_paths = set(patch_paths)
    worktree_patch_paths = {
        path.relative_to(BuildConstants.REPO_ROOT).as_posix(): path
        for path in (
            BuildConstants.REPO_ROOT / KernelBuildConstants.PATCH_DIRECTORY
        ).glob("*.patch")
    }
    for relative in sorted(worktree_patch_paths.keys() - head_patch_paths):
        worktree_patch_paths[relative].unlink()
        print(f">> removed stale {relative} absent from the run head")

    for relative in (config_path, *patch_paths):
        blob = run_capture(
            ["git", "cat-file", "blob", f"HEAD:{relative}"],
            cwd=BuildConstants.REPO_ROOT,
        )
        require_success(blob, f"run-head kernel provenance input read for {relative}")
        (BuildConstants.REPO_ROOT / relative).write_bytes(blob.stdout)
        print(f">> materialized {relative} from the run head")


def _kernel_source_fingerprint() -> str:
    return json.dumps(
        {
            "version": KernelBuildConstants.VERSION,
            "upstream_url": KernelBuildConstants.URL,
            "upstream_archive_sha256": KernelBuildConstants.SHA256,
            "patches": [
                {
                    "path": patch.relative_to(BuildConstants.REPO_ROOT).as_posix(),
                    "sha256": sha256_file(patch),
                }
                for patch in _kernel_patch_files()
            ],
        },
        sort_keys=True,
    )


def _kernel_provenance_inputs(
    source_fingerprint: str,
    input_config_sha256: str,
) -> dict[str, object]:
    return {
        "source": json.loads(source_fingerprint),
        "input_config": {
            "path": KernelBuildConstants.INPUT_CONFIG.as_posix(),
            "sha256": input_config_sha256,
        },
    }


def kernel_provenance_inputs() -> dict[str, object]:
    """Return the current source and input-config identity for a kernel build."""
    return _kernel_provenance_inputs(
        _kernel_source_fingerprint(),
        sha256_file(BuildConstants.REPO_ROOT / KernelBuildConstants.INPUT_CONFIG),
    )


def _initramfs_source_files() -> tuple[Path, ...]:
    sources = [
        BuildConstants.REPO_ROOT / DockerBuildConstants.DOCKERFILE,
        BuildConstants.REPO_ROOT / "scripts" / "nvx_tools" / "build.py",
        BuildConstants.REPO_ROOT / "scripts" / "nvx_tools" / "build_config.py",
        BuildConstants.REPO_ROOT / "scripts" / "nvx_tools" / "build_constants.py",
        BuildConstants.REPO_ROOT / "scripts" / "nvx_tools" / "common.py",
        BuildConstants.REPO_ROOT / "scripts" / "nvx_tools" / "guests.py",
        *(
            path
            for directory in AlpineBuildConstants.GUEST_SOURCE_DIRECTORIES
            for path in (BuildConstants.REPO_ROOT / directory).rglob("*")
            if path.is_file()
        ),
    ]
    return tuple(
        sorted(
            (require_file(path, "initramfs provenance input") for path in sources),
            key=lambda path: path.relative_to(BuildConstants.REPO_ROOT).as_posix(),
        )
    )


def initramfs_provenance_inputs() -> dict[str, object]:
    """Return the current source identity for an initramfs build."""
    return {
        "alpine": {
            "version": AlpineBuildConstants.VERSION,
            "branch": AlpineBuildConstants.BRANCH,
            "minirootfs_sha256": AlpineBuildConstants.MINIROOTFS_SHA256,
        },
        "source_files": [
            {
                "path": path.relative_to(BuildConstants.REPO_ROOT).as_posix(),
                "sha256": sha256_file(path),
            }
            for path in _initramfs_source_files()
        ],
    }


def record_openvmm_provenance(config: OpenVmmBuildConfig) -> None:
    """Bind an OpenVMM executable to the checked-out submodule revision."""
    head = run_capture(["git", "-C", config.directory, "rev-parse", "HEAD"])
    require_success(head, "OpenVMM revision query")
    gitlink = run_capture(
        ["git", "-C", BuildConstants.REPO_ROOT, "rev-parse", ":openvmm"]
    )
    require_success(gitlink, "OpenVMM gitlink query")
    status = run_capture(["git", "-C", config.directory, "status", "--porcelain"])
    require_success(status, "OpenVMM status query")
    source_revision = head.stdout.decode("ascii").strip()
    expected_revision = gitlink.stdout.decode("ascii").strip()
    if source_revision != expected_revision:
        raise ScriptError(
            f"OpenVMM submodule is at {source_revision}, expected {expected_revision}"
        )
    executable = require_file(config.output, "OpenVMM release binary")
    provenance = {
        "format": OpenVMMBuildConstants.PROVENANCE_FORMAT,
        "source_revision": source_revision,
        "source_clean": not status.stdout.strip(),
        "executable_sha256": sha256_file(executable),
    }
    path = config.build_directory / OpenVMMBuildConstants.PROVENANCE_NAME
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(provenance, indent=2) + "\n", encoding="utf-8")


def detect_openvmm_platform(backend: OpenVmmBackend | None = None) -> OpenVmmPlatform:
    """Select a build target without requiring runtime hypervisor access."""
    if sys.platform == "win32":
        if backend in (None, "whp"):
            return "windows-msvc"
    elif sys.platform == "linux":
        if backend in (None, "kvm"):
            return "linux-gnu"
        if backend == "mshv":
            return "linux-musl"
    else:
        raise ScriptError(f"OpenVMM builds are unsupported on {sys.platform}")
    raise ScriptError(f"OpenVMM backend {backend!r} is unsupported on {sys.platform}")


def _restore_openvmm_packages(config: OpenVmmBuildConfig) -> None:
    if config.skip_restore:
        return
    _run_openvmm_command(
        ["cargo", "xflowey", "restore-packages", "--no-compat-igvm"],
        cwd=config.directory,
    )


def _build_openvmm_musl(
    config: OpenVmmBuildConfig,
    platform: OpenVmmPlatform,
) -> None:
    target = config.openvmm_target(platform)
    _run_openvmm_command(["rustup", "target", "add", target])
    sysroot = config.directory.resolve() / OpenVMMBuildConstants.MUSL_SYSROOT
    require_file(
        sysroot / "lib" / "libsymcrypt.a",
        "restored OpenVMM musl SymCrypt library",
    )
    environment = os.environ.copy()
    environment.update(
        {
            "X86_64_UNKNOWN_LINUX_MUSL_OPENSSL_DIR": os.fspath(sysroot),
            "X86_64_UNKNOWN_LINUX_MUSL_OPENSSL_NO_VENDOR": "1",
            "X86_64_UNKNOWN_LINUX_MUSL_OPENSSL_STATIC": "1",
            "X86_64_UNKNOWN_LINUX_MUSL_SYMCRYPT_LIB_PATH": os.fspath(sysroot / "lib"),
            "X86_64_UNKNOWN_LINUX_MUSL_SYMCRYPT_STATIC": "1",
        }
    )
    _run_openvmm_command(
        [
            "cargo",
            "build",
            "--release",
            "--target",
            target,
            "-p",
            OpenVMMBuildConstants.PACKAGE_NAME,
            "--bin",
            OpenVMMBuildConstants.BINARY_NAME,
        ],
        cwd=config.directory,
        env=environment,
    )


def build_openvmm(
    config: OpenVmmBuildConfig,
    *,
    platform: OpenVmmPlatform | None = None,
) -> None:
    require_file(
        config.directory / "Cargo.toml",
        "initialized OpenVMM submodule",
    )
    selected = platform or detect_openvmm_platform(config.backend)
    mode = config.openvmm_build_mode(selected)
    _restore_openvmm_packages(config)
    if mode == "musl":
        _build_openvmm_musl(config, selected)
    else:
        _run_openvmm_command(
            [
                "cargo",
                "build",
                "--release",
                "-p",
                OpenVMMBuildConstants.PACKAGE_NAME,
                "--bin",
                OpenVMMBuildConstants.BINARY_NAME,
            ],
            cwd=config.directory,
        )
    source = require_file(
        config.openvmm_target_output(selected),
        f"OpenVMM {config.openvmm_target(selected)} release binary",
    )
    if not config.output.exists() or not source.samefile(config.output):
        config.output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, config.output)
    if mode == "musl":
        config.output.chmod(config.output.stat().st_mode | 0o111)
    record_openvmm_provenance(config)


def build_guest(config: BuildConfig) -> None:
    if config.native_guest:
        build_kernel(config.kernel)
        for guest in config.selected_guests():
            build_initramfs(config.initramfs_config(guest))
        if config.guest == "all":
            build_distro_layer(config.distro_layer_config())
        return
    build_docker_artifacts(config.docker, config.guest)


def build_all(config: BuildConfig) -> None:
    platform = detect_openvmm_platform(config.openvmm.backend)
    build_guest(config)
    build_openvmm(config.openvmm, platform=platform)


def prepare_kernel_source(config: KernelBuildConfig) -> tuple[Path, str]:
    """Download, verify, extract, and patch the pinned Linux source."""
    version = KernelBuildConstants.VERSION
    for tool in ("patch", "tar"):
        require_tool(tool)
    cache = config.cache_directory
    downloads = cache / BuildConstants.DOWNLOAD_DIRECTORY_NAME
    source_parent = cache / KernelBuildConstants.SOURCE_DIRECTORY_NAME
    tarball = downloads / KernelBuildConstants.UPSTREAM_ARCHIVE_NAME
    source = source_parent / KernelBuildConstants.SOURCE_NAME
    stamp = source_parent / KernelBuildConstants.SOURCE_STAMP_NAME
    fingerprint = _kernel_source_fingerprint()

    downloads.mkdir(parents=True, exist_ok=True)
    source_parent.mkdir(parents=True, exist_ok=True)
    download_verified(KernelBuildConstants.URL, tarball, KernelBuildConstants.SHA256)

    cached_fingerprint = stamp.read_text(encoding="utf-8") if stamp.is_file() else None
    if source.is_dir() and cached_fingerprint != fingerprint:
        shutil.rmtree(source)
    if not source.is_dir():
        stamp.unlink(missing_ok=True)
        print(f">> extracting and patching Linux {version}")
        run_checked(["tar", "-xf", tarball, "-C", source_parent])
        if not (source / "Makefile").is_file():
            raise ScriptError(f"Linux archive did not produce {source}")
        for patch in _kernel_patch_files():
            print(f">> applying {patch.name}")
            run_checked(
                ["patch", "--batch", "--forward", "-p1", "-i", patch],
                cwd=source,
            )
        stamp.write_text(fingerprint, encoding="utf-8")
    return source, fingerprint


def _prepare_alpine_root(config: InitramfsBuildConfig) -> Path:
    config.work.mkdir(parents=True, exist_ok=True)
    tarball = _alpine_tarball(config)
    download_verified(
        AlpineBuildConstants.MINIROOTFS_URL,
        tarball,
        AlpineBuildConstants.MINIROOTFS_SHA256,
    )
    root = config.work / InitramfsBuildConstants.ROOT_DIRECTORY_NAME
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    require_tool("tar")
    run_checked(["tar", "-xzf", tarball, "-C", root])
    print(">> installing sandbox utilities into the Alpine rootfs")
    _apk_add(root, *AlpineBuildConstants.PACKAGES)
    resolver = root / "etc" / "resolv.conf"
    resolver.unlink(missing_ok=True)
    resolver.touch()
    return root


def _install(source: Path, destination: Path) -> dict[str, str]:
    destination.write_bytes(source.read_bytes().replace(b"\r\n", b"\n"))
    destination.chmod(0o755)
    return {
        "source_sha256": sha256_file(source),
        "binary_sha256": sha256_file(destination),
    }


def _build_static_helper(
    work: Path,
    source: Path,
    destination: Path,
) -> dict[str, str]:
    compiler = require_tool("cc")
    output = work / source.stem
    run_checked(
        [
            compiler,
            *InitramfsBuildConstants.STATIC_HELPER_CFLAGS,
            "-o",
            output,
            source,
        ]
    )
    shutil.copyfile(output, destination)
    destination.chmod(0o755)
    return {
        "source_sha256": sha256_file(source),
        "binary_sha256": sha256_file(output),
    }


def _build_device_io_helper(work: Path, destination: Path) -> dict[str, str]:
    compiler = require_tool("cc")
    source = BuildConstants.REPO_ROOT / "guest" / "common" / "nvx-device-io.c"
    output = work / "nvx-device-io"
    run_checked(
        [
            compiler,
            *InitramfsBuildConstants.DEVICE_IO_CFLAGS,
            "-o",
            output,
            source,
        ]
    )
    shutil.copyfile(output, destination)
    destination.chmod(0o755)
    return {
        "source_sha256": sha256_file(source),
        "binary_sha256": sha256_file(output),
    }


def _apk_add(root: Path, *packages: str) -> None:
    loader = root / "lib" / "ld-musl-x86_64.so.1"
    environment = os.environ.copy()
    environment["LD_LIBRARY_PATH"] = f"{root / 'lib'}:{root / 'usr' / 'lib'}"
    host_ca_file = ssl.get_default_verify_paths().cafile
    if host_ca_file:
        environment.setdefault("SSL_CERT_FILE", host_ca_file)
    run_checked(
        [
            loader,
            root / "sbin" / "apk",
            "--root",
            root,
            "--no-cache",
            "--no-interactive",
            "add",
            *packages,
        ],
        env=environment,
    )


def _normalize_initramfs_metadata(root: Path) -> None:
    (root / "var" / "log" / "apk.log").unlink(missing_ok=True)
    for path in (*root.rglob("*"), root):
        try:
            os.utime(
                path,
                (InitramfsBuildConstants.TIMESTAMP, InitramfsBuildConstants.TIMESTAMP),
                follow_symlinks=False,
            )
        except (NotImplementedError, OSError) as error:
            if not path.is_symlink():
                raise ScriptError(
                    f"failed to normalize initramfs timestamp for {path}: {error}"
                ) from error


def _pack_initramfs(root: Path, output: Path) -> None:
    require_tool("find")
    require_tool("sort")
    require_tool("cpio")
    require_tool("gzip")
    _normalize_initramfs_metadata(root)
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("wb") as archive:
        finder = subprocess.Popen(
            ["find", ".", "-print0"], cwd=root, stdout=subprocess.PIPE
        )
        assert finder.stdout is not None
        sort_environment = os.environ.copy()
        sort_environment["LC_ALL"] = "C"
        sorter = subprocess.Popen(
            ["sort", "-z"],
            cwd=root,
            stdin=finder.stdout,
            stdout=subprocess.PIPE,
            env=sort_environment,
        )
        finder.stdout.close()
        assert sorter.stdout is not None
        cpio = subprocess.Popen(
            [
                "cpio",
                "--null",
                "--quiet",
                "--reproducible",
                "--owner=0:0",
                "-o",
                "-H",
                InitramfsBuildConstants.CPIO_FORMAT,
            ],
            cwd=root,
            stdin=sorter.stdout,
            stdout=subprocess.PIPE,
        )
        sorter.stdout.close()
        assert cpio.stdout is not None
        gzip = subprocess.Popen(
            ["gzip", "-n", f"-{InitramfsBuildConstants.GZIP_COMPRESSION_LEVEL}"],
            stdin=cpio.stdout,
            stdout=archive,
        )
        cpio.stdout.close()
        gzip_code = gzip.wait()
        cpio_code = cpio.wait()
        sorter_code = sorter.wait()
        finder_code = finder.wait()
    if finder_code or sorter_code or cpio_code or gzip_code:
        output.unlink(missing_ok=True)
        raise ScriptError(
            "failed to pack initramfs "
            f"(find={finder_code}, sort={sorter_code}, cpio={cpio_code}, "
            f"gzip={gzip_code})"
        )


def _write_apk_manifest(
    root: Path,
    output: Path,
    helpers: dict[str, dict[str, str]],
) -> None:
    installed = root / "lib" / "apk" / "db" / "installed"
    packages: list[ApkPackage] = []
    for record in installed.read_text(encoding="utf-8").split("\n\n"):
        fields: dict[str, str] = {}
        for line in record.splitlines():
            if len(line) >= 2 and line[1] == ":":
                fields[line[0]] = line[2:]
        if "P" not in fields:
            continue
        packages.append(
            {
                "name": fields["P"],
                "version": fields.get("V"),
                "architecture": fields.get("A"),
                "license": fields.get("L"),
                "origin": fields.get("o"),
                "url": fields.get("U"),
                "description": fields.get("T"),
                "aports_commit": fields.get("c"),
                "build_time": fields.get("t"),
            }
        )
    packages.sort(key=lambda package: package["name"])
    manifest = output.with_name(
        f"{output.name}{BuildConstants.PACKAGE_MANIFEST_SUFFIX}"
    )
    manifest.write_text(
        json.dumps(
            {
                "format": AlpineBuildConstants.PACKAGE_MANIFEST_VERSION,
                "alpine_version": AlpineBuildConstants.VERSION,
                "alpine_branch": AlpineBuildConstants.BRANCH,
                "architecture": AlpineBuildConstants.ARCHITECTURE,
                "packages": packages,
                "helpers": helpers,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )


def _install_guest_files(
    config: InitramfsBuildConfig,
    root: Path,
    descriptor: GuestDescriptor,
) -> dict[str, dict[str, str]]:
    common = BuildConstants.REPO_ROOT / BuildConstants.COMMON_GUEST_DIRECTORY
    helpers: dict[str, dict[str, str]] = {}
    scripts = [
        ("init", common / "init", root / "init"),
        ("nvx-exit", common / "nvx-exit", root / "sbin" / "nvx-exit"),
        (
            "nvx-hostmount",
            common / "nvx-hostmount",
            root / "sbin" / "nvx-hostmount",
        ),
        (
            "nvx-identity-probe",
            common / "nvx-identity-probe",
            root / "sbin" / "nvx-identity-probe",
        ),
        (
            "nvx-snapshot",
            common / "nvx-snapshot",
            root / "sbin" / "nvx-snapshot",
        ),
        (
            "nvx-sandbox-smoke",
            common / "nvx-sandbox-smoke",
            root / "sbin" / "nvx-sandbox-smoke",
        ),
        (
            "nvx-virtio-restore-probe",
            common / "nvx-virtio-restore-probe",
            root / "sbin" / "nvx-virtio-restore-probe",
        ),
    ]
    if descriptor.sandbox_control:
        alpine = BuildConstants.REPO_ROOT / AlpineBuildConstants.GUEST_DIRECTORY
        scripts.extend(
            [
                (
                    "nvx-init-agent",
                    common / "nvx-init-agent",
                    root / "sbin" / "nvx-init-agent",
                ),
                (
                    "nvx-container-enter",
                    alpine / "nvx-container-enter",
                    root / "sbin" / "nvx-container-enter",
                ),
                (
                    "nvx-container-launch",
                    alpine / "nvx-container-launch",
                    root / "sbin" / "nvx-container-launch",
                ),
            ]
        )
    else:
        scripts.append(
            (
                "nvx-bashrc",
                BuildConstants.REPO_ROOT
                / UbuntuBuildConstants.GUEST_DIRECTORY
                / "nvx-bashrc",
                root / "etc" / "nvx-bashrc",
            )
        )
    for name, source, destination in scripts:
        helpers[name] = _install(source, destination)
        if name == "nvx-bashrc":
            destination.chmod(0o644)

    for name in InitramfsBuildConstants.STATIC_HELPERS:
        helpers[name] = _build_static_helper(
            config.work,
            common / f"{name}.c",
            root / "sbin" / name,
        )
    helpers["nvx-device-io"] = _build_device_io_helper(
        config.work,
        root / "sbin" / "nvx-device-io",
    )
    return helpers


def _prepare_guest_root(
    config: InitramfsBuildConfig,
    descriptor: GuestDescriptor,
) -> Path:
    if descriptor.name == "alpine":
        return _prepare_alpine_root(config)
    if descriptor.name == "ubuntu":
        return ubuntu.prepare_root(config.work)
    raise AssertionError(f"missing rootfs preparer for {descriptor.name}")


def _write_ubuntu_manifest(
    root: Path,
    output: Path,
    helpers: dict[str, dict[str, str]],
    input_sha256: str,
) -> None:
    manifest = output.with_name(
        f"{output.name}{BuildConstants.PACKAGE_MANIFEST_SUFFIX}"
    )
    document = ubuntu.package_manifest(root, helpers)
    document.update(
        {
            "artifact": output.name,
            "artifact_sha256": sha256_file(output),
            "input_sha256": input_sha256,
        }
    )
    manifest.write_text(
        json.dumps(document, indent=2) + "\n",
        encoding="utf-8",
    )


def _guest_customization_files(descriptor: GuestDescriptor) -> tuple[Path, ...]:
    if descriptor.name == "ubuntu":
        return ubuntu.customization_files()
    common = tuple(
        path
        for path in sorted(
            (BuildConstants.REPO_ROOT / BuildConstants.COMMON_GUEST_DIRECTORY).iterdir()
        )
        if path.is_file()
    )
    alpine = tuple(
        path
        for path in sorted(
            (BuildConstants.REPO_ROOT / AlpineBuildConstants.GUEST_DIRECTORY).iterdir()
        )
        if path.is_file()
    )
    return (*common, *alpine)


def build_initramfs(config: InitramfsBuildConfig) -> None:
    _require_linux("build-initramfs")
    descriptor = guest_descriptor(config.guest)
    output = config.output or artifact_path(descriptor.initramfs_name)
    package_manifest = output.with_name(
        f"{output.name}{BuildConstants.PACKAGE_MANIFEST_SUFFIX}"
    )
    provenance_inputs = (
        initramfs_provenance_inputs() if descriptor.name == "alpine" else None
    )
    provenance_path = output.with_name(InitramfsBuildConstants.PROVENANCE_NAME)
    if provenance_inputs is not None:
        provenance_path.unlink(missing_ok=True)
    root = _prepare_guest_root(config, descriptor)
    helpers = _install_guest_files(config, root, descriptor)
    if descriptor.name == "ubuntu":
        ubuntu.apply_metadata_policy(root)
    else:
        _write_apk_manifest(root, output, helpers)
    _pack_initramfs(root, output)
    if descriptor.name == "ubuntu":
        _write_ubuntu_manifest(
            root,
            output,
            helpers,
            ubuntu.converter_input_sha256(ubuntu.customization_files()),
        )
    if (
        provenance_inputs is not None
        and initramfs_provenance_inputs() != provenance_inputs
    ):
        output.unlink(missing_ok=True)
        package_manifest.unlink(missing_ok=True)
        raise ScriptError("initramfs source inputs changed during the build")
    if provenance_inputs is not None:
        provenance_path.write_text(
            json.dumps(
                {
                    "format": InitramfsBuildConstants.PROVENANCE_FORMAT,
                    "inputs": provenance_inputs,
                    "initramfs_sha256": sha256_file(output),
                    "package_manifest_sha256": sha256_file(package_manifest),
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
    print(f">> built {output} ({format_size(output.stat().st_size)})")


def build_distro_layer(config: DistroLayerBuildConfig) -> None:
    _require_linux("build-distro-layer")
    descriptor = guest_descriptor(config.guest)
    if descriptor.name != "ubuntu":
        raise ScriptError("build-distro-layer currently supports only --guest ubuntu")
    output = config.output.resolve()
    manifest = output.with_name(f"{output.name}{BuildConstants.DISTRO_MANIFEST_SUFFIX}")
    existing = [str(path) for path in (output, manifest) if path.exists()]
    if existing and not config.replace:
        raise ScriptError(
            "refusing to replace existing Ubuntu distro artifact: "
            + ", ".join(existing)
            + "; pass --replace"
        )

    initramfs_config = InitramfsBuildConfig(
        guest=descriptor.name,
        work=config.work,
    )
    root = _prepare_guest_root(initramfs_config, descriptor)
    helpers = _install_guest_files(initramfs_config, root, descriptor)
    ubuntu.apply_metadata_policy(root)
    if (root / "sbin" / "init").exists() or any(
        package["name"] == "systemd" for package in ubuntu.package_records(root)
    ):
        raise ScriptError("systemd is unsupported in the Ubuntu sandbox layer profile")
    _normalize_initramfs_metadata(root)

    input_sha256 = ubuntu.converter_input_sha256(_guest_customization_files(descriptor))
    filesystem_uuid = ubuntu.erofs_uuid(input_sha256)
    document = ubuntu.package_manifest(root, helpers)
    document.update(
        {
            "artifact": output.name,
            "converter_format": UbuntuBuildConstants.EROFS_FORMAT,
            "input_sha256": input_sha256,
            "uuid": filesystem_uuid,
            "compression": UbuntuBuildConstants.EROFS_COMPRESSION,
        }
    )

    mkfs = require_tool(
        "mkfs.erofs",
        "mkfs.erofs was not found on PATH; install erofs-utils",
    )
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary_output = output.with_name(f"{output.name}.part")
    temporary_manifest = manifest.with_name(f"{manifest.name}.part")
    temporary_output.unlink(missing_ok=True)
    temporary_manifest.unlink(missing_ok=True)
    try:
        run_checked(
            [
                mkfs,
                "--quiet",
                "--all-root",
                "-T",
                str(UbuntuBuildConstants.EROFS_TIMESTAMP),
                "-U",
                filesystem_uuid,
                "-z",
                UbuntuBuildConstants.EROFS_COMPRESSION,
                temporary_output,
                root,
            ]
        )
        document["artifact_sha256"] = sha256_file(temporary_output)
        temporary_manifest.write_text(
            json.dumps(document, indent=2) + "\n",
            encoding="utf-8",
        )
        temporary_output.replace(output)
        temporary_manifest.replace(manifest)
    finally:
        temporary_output.unlink(missing_ok=True)
        temporary_manifest.unlink(missing_ok=True)
    print(f">> built {output} ({format_size(output.stat().st_size)})")


def verify_guest_determinism(work: Path, guest: str) -> None:
    _require_linux("verify-guest-determinism")
    descriptor = guest_descriptor(guest)
    if descriptor.name != "ubuntu":
        raise ScriptError(
            "verify-guest-determinism currently supports only --guest ubuntu"
        )
    artifact_names = (
        descriptor.initramfs_name,
        descriptor.package_manifest_name,
        UbuntuBuildConstants.DISTRO_NAME,
        UbuntuBuildConstants.DISTRO_MANIFEST_NAME,
    )
    attempts: list[Path] = []
    digests: list[dict[str, str]] = []
    for attempt in (1, 2):
        attempt_root = work / f"attempt-{attempt}"
        if attempt_root.exists():
            shutil.rmtree(attempt_root)
        attempt_root.mkdir(parents=True)
        build_initramfs(
            InitramfsBuildConfig(
                guest=descriptor.name,
                work=(
                    attempt_root
                    / InitramfsBuildConstants.DETERMINISM_INITRAMFS_WORK_DIRECTORY_NAME
                ),
                output=attempt_root / descriptor.initramfs_name,
            )
        )
        build_distro_layer(
            DistroLayerBuildConfig(
                guest=descriptor.name,
                work=(
                    attempt_root
                    / InitramfsBuildConstants.DETERMINISM_DISTRO_WORK_DIRECTORY_NAME
                ),
                output=attempt_root / UbuntuBuildConstants.DISTRO_NAME,
            )
        )
        attempts.append(attempt_root)
        digests.append(
            {name: sha256_file(attempt_root / name) for name in artifact_names}
        )
    if digests[0] != digests[1]:
        first_inventory = ubuntu.rootfs_inventory(
            attempts[0]
            / InitramfsBuildConstants.DETERMINISM_INITRAMFS_WORK_DIRECTORY_NAME
            / InitramfsBuildConstants.ROOT_DIRECTORY_NAME
        )
        second_inventory = ubuntu.rootfs_inventory(
            attempts[1]
            / InitramfsBuildConstants.DETERMINISM_INITRAMFS_WORK_DIRECTORY_NAME
            / InitramfsBuildConstants.ROOT_DIRECTORY_NAME
        )
        differing_paths = sorted(
            path
            for path in first_inventory.keys() | second_inventory.keys()
            if first_inventory.get(path) != second_inventory.get(path)
        )
        if differing_paths:
            path = differing_paths[0]
            diagnostic = (
                f"; first rootfs difference at {path}: "
                f"{first_inventory.get(path)!r} != "
                f"{second_inventory.get(path)!r}"
            )
        else:
            diagnostic = "; normalized rootfs inventories are identical"
        raise ScriptError(
            f"Ubuntu guest artifacts are not deterministic: "
            f"{digests[0]!r} != {digests[1]!r}{diagnostic}"
        )
    print(">> Ubuntu initramfs and EROFS artifacts are deterministic")


def build_kernel(config: KernelBuildConfig) -> None:
    _require_linux("build-kernel")
    for tool in ("make", "readelf"):
        require_tool(tool)
    source, source_fingerprint = prepare_kernel_source(config)
    input_config = BuildConstants.REPO_ROOT / KernelBuildConstants.INPUT_CONFIG
    input_config_sha256 = sha256_file(input_config)
    provenance_inputs = _kernel_provenance_inputs(
        source_fingerprint,
        input_config_sha256,
    )
    build_fingerprint = json.dumps(
        {
            "source": source_fingerprint,
            "input_config_sha256": input_config_sha256,
        },
        sort_keys=True,
    )
    build_stamp = config.work / KernelBuildConstants.BUILD_STAMP_NAME
    cached_build_fingerprint = (
        build_stamp.read_text(encoding="utf-8") if build_stamp.is_file() else None
    )
    if config.work.is_dir() and cached_build_fingerprint != build_fingerprint:
        shutil.rmtree(config.work)
    config.work.mkdir(parents=True, exist_ok=True)
    build_stamp.write_text(build_fingerprint, encoding="utf-8")
    kernel_config = config.work / KernelBuildConstants.BUILD_CONFIG_NAME
    provenance_path = config.output.with_name(KernelBuildConstants.PROVENANCE_NAME)
    provenance_path.unlink(missing_ok=True)
    shutil.copy2(input_config, kernel_config)
    if sha256_file(kernel_config) != input_config_sha256:
        raise ScriptError("kernel input config changed while it was being copied")
    make = ["make", "-C", source, f"O={config.work}"]
    run_checked([*make, "olddefconfig"])
    assert_required_kernel_config(kernel_config)
    jobs = os.cpu_count() or 1
    print(f">> building vmlinux with {jobs} jobs")
    run_checked([*make, f"-j{jobs}", KernelBuildConstants.BINARY_NAME])
    config.output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(config.work / KernelBuildConstants.BINARY_NAME, config.output)
    generated_config = config.output.with_name(f"{config.output.name}.config")
    shutil.copy2(kernel_config, generated_config)
    print(f">> built {config.output}")

    notes = run_capture(["readelf", "-n", config.output])
    if "Xen" in notes.text and "0x00000012" in notes.text:
        print(">> PVH entry note present")
    else:
        config.output.unlink(missing_ok=True)
        raise ScriptError("PVH entry note 0x12 is missing from the built vmlinux")

    if (
        _kernel_source_fingerprint() != source_fingerprint
        or sha256_file(input_config) != input_config_sha256
    ):
        config.output.unlink(missing_ok=True)
        generated_config.unlink(missing_ok=True)
        raise ScriptError("kernel source inputs changed during the build")
    provenance = {
        "format": KernelBuildConstants.PROVENANCE_FORMAT,
        **provenance_inputs,
        "kernel_sha256": sha256_file(config.output),
        "config_sha256": sha256_file(generated_config),
    }
    provenance_path.write_text(
        json.dumps(provenance, indent=2) + "\n",
        encoding="utf-8",
    )


def docker_build_command(config: DockerBuildConfig, target: str) -> list[str | Path]:
    configured_destination = (
        config.linux_source_destination
        if target == DockerBuildConstants.LINUX_SOURCE_TARGET
        else config.artifact_destination
    )
    destination = _docker_destination(configured_destination)
    command: list[str | Path] = [
        "docker",
        "build",
        "-f",
        BuildConstants.REPO_ROOT / DockerBuildConstants.DOCKERFILE,
        "--target",
        target,
    ]
    command.extend(
        [
            "--output",
            f"type={DockerBuildConstants.OUTPUT_TYPE},dest={destination}",
            BuildConstants.REPO_ROOT,
        ]
    )
    return command


def _docker_destination(destination: Path) -> Path:
    if not destination.is_absolute():
        destination = BuildConstants.REPO_ROOT / destination
    return destination.resolve()


def build_docker_linux_source(config: DockerBuildConfig) -> Path:
    """Build and export the pinned Linux corresponding-source archive."""
    require_tool(
        "docker",
        "docker was not found on PATH; install Docker with the Linux engine first",
    )
    destination = _docker_destination(config.linux_source_destination)
    print(f">> building Linux corresponding source into '{destination}'")
    run_checked(
        docker_build_command(config, DockerBuildConstants.LINUX_SOURCE_TARGET),
        cwd=BuildConstants.REPO_ROOT,
    )
    archive = destination / KernelBuildConstants.SOURCE_ARCHIVE_NAME
    if not archive.is_file():
        raise ScriptError(f"Docker build did not produce {archive.name}")
    print(f">> built {archive} ({format_size(archive.stat().st_size)})")
    return archive


def build_docker_artifacts(
    config: DockerBuildConfig,
    guest: str = InitramfsBuildConstants.DEFAULT_GUEST,
) -> None:
    require_tool(
        "docker",
        "docker was not found on PATH; install Docker with the Linux engine first",
    )
    destination = _docker_destination(config.artifact_destination)
    if guest == "all":
        target = DockerBuildConstants.ALL_GUESTS_TARGET
        expected = DockerBuildConstants.ALL_GUEST_ARTIFACT_NAMES
        guest_label = "Alpine and Ubuntu"
    else:
        descriptor = guest_descriptor(guest)
        target = (
            DockerBuildConstants.ALPINE_TARGET
            if descriptor.name == AlpineBuildConstants.GUEST_NAME
            else DockerBuildConstants.UBUNTU_TARGET
        )
        expected = (
            KernelBuildConstants.BINARY_NAME,
            KernelBuildConstants.CONFIG_NAME,
            KernelBuildConstants.PROVENANCE_NAME,
            descriptor.initramfs_name,
            descriptor.package_manifest_name,
        )
        if descriptor.name == "alpine":
            expected = (*expected, InitramfsBuildConstants.PROVENANCE_NAME)
        guest_label = descriptor.distribution
    print(
        f">> building Linux artifacts into '{destination}' "
        f"(kernel {KernelBuildConstants.VERSION}, {guest_label})"
    )
    run_checked(docker_build_command(config, target), cwd=BuildConstants.REPO_ROOT)
    missing = [name for name in expected if not (destination / name).is_file()]
    if missing:
        raise ScriptError(f"Docker build did not produce: {', '.join(missing)}")
    print(">> done:")
    for name in expected:
        path = destination / name
        print(f"  {path} ({format_size(path.stat().st_size)})")

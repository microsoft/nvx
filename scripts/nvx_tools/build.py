"""Linux-native and Docker-backed artifact build workflows."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path

from .backends.base import HostBackend
from .common import (
    REPO_ROOT,
    ScriptError,
    download,
    format_size,
    require_tool,
    require_success,
    run_capture,
    run_checked,
)


DEFAULT_KERNEL_VERSION = "6.18.38"
DEFAULT_KERNEL_URL = (
    "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.18.38.tar.xz"
)
DEFAULT_KERNEL_SHA256 = (
    "ac26e508abd56e9f8b89872b6e10c49fc823bcc70d8068a5d8504c1a7c4ff045"
)
DEFAULT_ALPINE_VERSION = "3.24.1"
DEFAULT_ALPINE_BRANCH = "v3.24"
DEFAULT_ALPINE_MINIROOTFS_SHA256 = (
    "41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081"
)
DEFAULT_PYTHON_PROFILE = "full"

REQUIRED_VIRTIO_CONSOLE_CONFIG = (
    "CONFIG_HVC_DRIVER=y",
    "CONFIG_VIRTIO=y",
    "CONFIG_VIRTIO_CONSOLE=y",
    "CONFIG_VIRTIO_MMIO=y",
    "CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y",
)


def _assert_virtio_console_kernel_config(path: Path) -> None:
    configured = set(path.read_text(encoding="utf-8").splitlines())
    missing = [
        setting
        for setting in REQUIRED_VIRTIO_CONSOLE_CONFIG
        if setting not in configured
    ]
    if missing:
        raise ScriptError(
            "kernel configuration cannot provide /dev/hvc1: " + ", ".join(missing)
        )


@dataclass(frozen=True)
class PythonInitramfsProfile:
    """Packages, guest inputs, and output contract for a Python initramfs."""

    packages: tuple[str, ...]
    sources: tuple[tuple[str, str], ...]
    artifact_name: str
    work_name: str
    docker_target: str
    forbidden_packages: tuple[str, ...] = ()
    prune_runtime: bool = False


PYTHON_INITRAMFS_PROFILES = {
    "full": PythonInitramfsProfile(
        packages=("python3", "py3-numpy", "py3-pandas"),
        sources=(
            ("init.python", "init"),
            ("snapshot-trampoline.py", "snapshot-trampoline.py"),
            ("mxc-agent.py", "mxc-agent.py"),
            ("nvx-snapshot", "sbin/nvx-snapshot"),
            ("nvx-exit", "sbin/nvx-exit"),
        ),
        artifact_name="initramfs-python.cpio.gz",
        work_name="initramfs-python",
        docker_target="python-artifacts",
        prune_runtime=True,
    ),
    "agent": PythonInitramfsProfile(
        packages=("python3",),
        sources=(
            ("init.python", "init"),
            ("mxc-agent.py", "mxc-agent.py"),
            ("nvx-snapshot", "sbin/nvx-snapshot"),
            ("nvx-exit", "sbin/nvx-exit"),
        ),
        artifact_name="initramfs-python-agent.cpio.gz",
        work_name="initramfs-python-agent",
        docker_target="python-agent-artifacts",
        forbidden_packages=(
            "py3-numpy",
            "py3-pandas",
            "openblas",
            "blas",
            "lapack",
            "libgfortran",
            "libquadmath",
            "gfortran",
        ),
        prune_runtime=True,
    ),
}


@dataclass(frozen=True)
class AlpineBuildConfig:
    version: str = DEFAULT_ALPINE_VERSION
    branch: str = DEFAULT_ALPINE_BRANCH
    work: Path = Path.home() / "build" / "initramfs"
    output: Path = Path.home() / "build" / "initramfs.cpio.gz"


@dataclass(frozen=True)
class KernelBuildConfig:
    version: str = DEFAULT_KERNEL_VERSION
    work: Path = Path.home() / "build" / "kernel"
    output: Path = Path.home() / "build" / "vmlinux"
    profiling: bool = False


@dataclass(frozen=True)
class DockerBuildConfig:
    destination: Path = Path("build")
    kernel_version: str = DEFAULT_KERNEL_VERSION
    alpine_version: str = DEFAULT_ALPINE_VERSION
    alpine_branch: str = DEFAULT_ALPINE_BRANCH
    profiling: bool = False


def python_initramfs_profile(name: str) -> PythonInitramfsProfile:
    """Returns a named Python image profile or raises a CLI-friendly error."""
    try:
        return PYTHON_INITRAMFS_PROFILES[name]
    except KeyError as error:
        choices = ", ".join(PYTHON_INITRAMFS_PROFILES)
        raise ScriptError(
            f"unknown Python initramfs profile {name!r}; choose {choices}"
        ) from error


def _require_linux(backend: HostBackend, workflow: str) -> None:
    if backend.name != "linux-kvm":
        raise ScriptError(
            f"{workflow} requires a Linux host; use build-linux-artifacts with Docker"
        )


def _alpine_tarball(config: AlpineBuildConfig) -> Path:
    return config.work / f"alpine-minirootfs-{config.version}-x86_64.tar.gz"


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _cache_root() -> Path:
    configured = os.environ.get("NVX_CACHE_DIR")
    return (
        Path(configured).expanduser().resolve()
        if configured
        else (REPO_ROOT / ".cache").resolve()
    )


def _kernel_patch_files() -> tuple[Path, ...]:
    patches = tuple(sorted((REPO_ROOT / "kernel" / "patches").glob("*.patch")))
    if not patches:
        raise ScriptError("no kernel patches were found")
    return patches


def _kernel_source_fingerprint() -> str:
    return json.dumps(
        {
            "archive_sha256": DEFAULT_KERNEL_SHA256,
            "patches": [
                {"name": patch.name, "sha256": _sha256(patch)}
                for patch in _kernel_patch_files()
            ],
        },
        sort_keys=True,
    )


def prepare_kernel_source(version: str = DEFAULT_KERNEL_VERSION) -> tuple[Path, str]:
    """Download, verify, extract, and patch the pinned Linux source."""
    if version != DEFAULT_KERNEL_VERSION:
        raise ScriptError(
            "this source tree pins Linux "
            f"{DEFAULT_KERNEL_VERSION}; requested {version}"
        )
    for tool in ("patch", "tar"):
        require_tool(tool)
    cache = _cache_root()
    downloads = cache / "downloads"
    source_parent = cache / "linux"
    tarball = downloads / f"linux-{version}.tar.xz"
    source = source_parent / f"linux-{version}"
    stamp = source_parent / f"linux-{version}.nvx-source.json"
    fingerprint = _kernel_source_fingerprint()

    downloads.mkdir(parents=True, exist_ok=True)
    source_parent.mkdir(parents=True, exist_ok=True)
    if not tarball.is_file():
        print(f">> downloading {tarball.name}")
        download(DEFAULT_KERNEL_URL, tarball)
    actual_sha256 = _sha256(tarball)
    if actual_sha256 != DEFAULT_KERNEL_SHA256:
        raise ScriptError(
            f"{tarball.name} SHA-256 is {actual_sha256}, "
            f"expected {DEFAULT_KERNEL_SHA256}"
        )

    cached_fingerprint = (
        stamp.read_text(encoding="utf-8") if stamp.is_file() else None
    )
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


def _prepare_alpine_root(config: AlpineBuildConfig) -> Path:
    if config.version != DEFAULT_ALPINE_VERSION:
        raise ScriptError(
            "this source tree pins Alpine "
            f"{DEFAULT_ALPINE_VERSION}; requested {config.version}"
        )
    config.work.mkdir(parents=True, exist_ok=True)
    tarball = _alpine_tarball(config)
    if not tarball.is_file():
        print(f">> downloading {tarball.name}")
        download(
            "https://dl-cdn.alpinelinux.org/alpine/"
            f"{config.branch}/releases/x86_64/{tarball.name}",
            tarball,
        )
    actual_sha256 = _sha256(tarball)
    if actual_sha256 != DEFAULT_ALPINE_MINIROOTFS_SHA256:
        raise ScriptError(
            f"{tarball.name} SHA-256 is {actual_sha256}, "
            f"expected {DEFAULT_ALPINE_MINIROOTFS_SHA256}"
        )
    root = config.work / "root"
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    require_tool("tar")
    run_checked(["tar", "-xzf", tarball, "-C", root])
    return root


def _install(source: Path, destination: Path) -> None:
    destination.write_bytes(source.read_bytes().replace(b"\r\n", b"\n"))
    destination.chmod(0o755)


def _apk_add(root: Path, *packages: str) -> None:
    loader = root / "lib" / "ld-musl-x86_64.so.1"
    environment = os.environ.copy()
    environment["LD_LIBRARY_PATH"] = f"{root / 'lib'}:{root / 'usr' / 'lib'}"
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


def _apk_installed_packages(root: Path) -> tuple[str, ...]:
    loader = root / "lib" / "ld-musl-x86_64.so.1"
    environment = os.environ.copy()
    environment["LD_LIBRARY_PATH"] = f"{root / 'lib'}:{root / 'usr' / 'lib'}"
    result = run_capture(
        [
            loader,
            root / "sbin" / "apk",
            "--root",
            root,
            "info",
        ],
        env=environment,
    )
    require_success(result, "listing installed APK packages")
    return tuple(sorted(result.stdout.decode("utf-8").splitlines()))


def _prune_python_runtime(root: Path) -> None:
    for relative in (
        "usr/include",
        "usr/lib/pkgconfig",
        "usr/share/aclocal",
        "usr/share/doc",
        "usr/share/info",
        "usr/share/man",
    ):
        shutil.rmtree(root / relative, ignore_errors=True)

    python_roots = tuple((root / "usr" / "lib").glob("python3.*"))
    for python_root in python_roots:
        for relative in ("idlelib", "test", "turtledemo"):
            shutil.rmtree(python_root / relative, ignore_errors=True)
        for config_dir in python_root.glob("config-*"):
            shutil.rmtree(config_dir, ignore_errors=True)
        extension_dir = python_root / "lib-dynload"
        for pattern in (
            "_*test*.so",
            "_xxtestfuzz*.so",
            "xxlimited*.so",
            "xxsubtype*.so",
        ):
            for extension in extension_dir.glob(pattern):
                extension.unlink(missing_ok=True)
        site_packages = python_root / "site-packages"
        if site_packages.is_dir():
            for tests in (
                *site_packages.glob("*/test"),
                *site_packages.glob("*/tests"),
            ):
                shutil.rmtree(tests, ignore_errors=True)

    for pattern in ("*.a", "*.la"):
        for path in root.rglob(pattern):
            path.unlink(missing_ok=True)


def _validate_agent_runtime(root: Path, profile: PythonInitramfsProfile) -> None:
    installed = _apk_installed_packages(root)
    forbidden = tuple(
        package
        for package in installed
        if any(
            package == prefix or package.startswith(f"{prefix}-")
            for prefix in profile.forbidden_packages
        )
    )
    if forbidden:
        raise ScriptError(
            "agent initramfs contains forbidden APK packages: " + ", ".join(forbidden)
        )

    forbidden_paths = []
    for path in root.rglob("*"):
        relative = path.relative_to(root).as_posix()
        components = tuple(component.lower() for component in path.parts)
        if (
            any(component.startswith(("numpy", "pandas")) for component in components)
            or path.suffix in {".a", ".la"}
            or relative.startswith("usr/include/")
            or (
                path.is_dir()
                and path.name in {"idlelib", "test", "tests", "turtledemo"}
            )
            or (
                path.is_dir()
                and path.parent.name.startswith("python3.")
                and path.name.startswith("config-")
            )
            or (
                path.parent.name == "lib-dynload"
                and path.name.startswith(
                    ("_ctypes_test", "_test", "_xxtestfuzz", "xxlimited", "xxsubtype")
                )
            )
        ):
            forbidden_paths.append(relative)
    if forbidden_paths:
        preview = ", ".join(forbidden_paths[:8])
        raise ScriptError(
            f"agent initramfs contains forbidden runtime content: {preview}"
        )


def _normalize_initramfs_metadata(root: Path) -> None:
    (root / "var" / "log" / "apk.log").unlink(missing_ok=True)
    for path in (*root.rglob("*"), root):
        try:
            os.utime(path, (0, 0), follow_symlinks=False)
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
                "newc",
            ],
            cwd=root,
            stdin=sorter.stdout,
            stdout=subprocess.PIPE,
        )
        sorter.stdout.close()
        assert cpio.stdout is not None
        gzip = subprocess.Popen(["gzip", "-n", "-9"], stdin=cpio.stdout, stdout=archive)
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
    config: AlpineBuildConfig,
) -> None:
    installed = root / "lib" / "apk" / "db" / "installed"
    packages = []
    for record in installed.read_text(encoding="utf-8").split("\n\n"):
        fields = {}
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
    manifest = output.with_name(f"{output.name}.packages.json")
    manifest.write_text(
        json.dumps(
            {
                "format": 1,
                "alpine_version": config.version,
                "alpine_branch": config.branch,
                "architecture": "x86_64",
                "packages": packages,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )


def build_initramfs(config: AlpineBuildConfig, backend: HostBackend) -> None:
    _require_linux(backend, "build-initramfs")
    root = _prepare_alpine_root(config)
    print(">> installing busybox-extras into the rootfs")
    _apk_add(root, "busybox-extras")
    resolver = root / "etc" / "resolv.conf"
    resolver.unlink(missing_ok=True)
    resolver.touch()
    _install(REPO_ROOT / "alpine" / "init", root / "init")
    _install(REPO_ROOT / "alpine" / "nvx-exit", root / "sbin" / "nvx-exit")
    _install(
        REPO_ROOT / "alpine" / "nvx-hostmount",
        root / "sbin" / "nvx-hostmount",
    )
    _install(REPO_ROOT / "alpine" / "nvx-snapshot", root / "sbin" / "nvx-snapshot")
    _install(
        REPO_ROOT / "alpine" / "snapshot-dispatcher.sh",
        root / "snapshot-dispatcher.sh",
    )
    config.output.parent.mkdir(parents=True, exist_ok=True)
    _write_apk_manifest(root, config.output, config)
    _pack_initramfs(root, config.output)
    print(f">> built {config.output} ({format_size(config.output.stat().st_size)})")


def build_python_initramfs_native(
    config: AlpineBuildConfig,
    backend: HostBackend,
    profile_name: str = DEFAULT_PYTHON_PROFILE,
) -> None:
    _require_linux(backend, "native build-python-initramfs")
    profile = python_initramfs_profile(profile_name)
    root = _prepare_alpine_root(config)
    print(f">> installing {', '.join(profile.packages)} into the {profile_name} rootfs")
    _apk_add(root, *profile.packages)
    for source_name, destination_name in profile.sources:
        _install(REPO_ROOT / "alpine" / source_name, root / destination_name)
    if profile.prune_runtime:
        _prune_python_runtime(root)
    if profile.forbidden_packages:
        _validate_agent_runtime(root, profile)
    config.output.parent.mkdir(parents=True, exist_ok=True)
    _write_apk_manifest(root, config.output, config)
    _pack_initramfs(root, config.output)
    print(f">> built {config.output} ({format_size(config.output.stat().st_size)})")


def build_kernel(config: KernelBuildConfig, backend: HostBackend) -> None:
    _require_linux(backend, "build-kernel")
    for tool in ("make", "readelf"):
        require_tool(tool)
    source, source_fingerprint = prepare_kernel_source(config.version)
    build_fingerprint = json.dumps(
        {
            "source": source_fingerprint,
            "profiling": config.profiling,
        },
        sort_keys=True,
    )
    build_stamp = config.work / ".nvx-build.json"
    cached_build_fingerprint = (
        build_stamp.read_text(encoding="utf-8") if build_stamp.is_file() else None
    )
    if config.work.is_dir() and cached_build_fingerprint != build_fingerprint:
        shutil.rmtree(config.work)
    config.work.mkdir(parents=True, exist_ok=True)
    build_stamp.write_text(build_fingerprint, encoding="utf-8")
    kernel_config = config.work / ".config"
    shutil.copy2(REPO_ROOT / "kernel" / "config-microvm", kernel_config)
    if config.profiling:
        print(
            ">> applying profiling kernel config overlay (frame pointers, ORC disabled)"
        )
        run_checked(
            [
                source / "scripts" / "kconfig" / "merge_config.sh",
                "-O",
                config.work,
                "-m",
                kernel_config,
                REPO_ROOT / "kernel" / "config-microvm-profiling",
            ],
            cwd=config.work,
        )
    make = ["make", "-C", source, f"O={config.work}"]
    run_checked([*make, "olddefconfig"])
    _assert_virtio_console_kernel_config(kernel_config)
    jobs = os.cpu_count() or 1
    print(f">> building vmlinux with {jobs} jobs")
    run_checked([*make, f"-j{jobs}", "vmlinux"])
    config.output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(config.work / "vmlinux", config.output)
    shutil.copy2(kernel_config, config.output.with_name(f"{config.output.name}.config"))
    print(f">> built {config.output}")

    notes = run_capture(["readelf", "-n", config.output])
    if "Xen" in notes.text and "0x00000012" in notes.text:
        print(">> PVH entry note present")
    else:
        config.output.unlink(missing_ok=True)
        raise ScriptError("PVH entry note 0x12 is missing from the built vmlinux")


def docker_build_command(config: DockerBuildConfig, target: str) -> list[str | Path]:
    if (
        config.kernel_version != DEFAULT_KERNEL_VERSION
        or config.alpine_version != DEFAULT_ALPINE_VERSION
        or config.alpine_branch != DEFAULT_ALPINE_BRANCH
    ):
        raise ScriptError(
            "Docker builds are pinned to Linux "
            f"{DEFAULT_KERNEL_VERSION} and Alpine {DEFAULT_ALPINE_VERSION} "
            f"({DEFAULT_ALPINE_BRANCH})"
        )
    destination = _docker_destination(config.destination)
    command: list[str | Path] = [
        "docker",
        "build",
        "-f",
        REPO_ROOT / "docker" / "Dockerfile",
        "--target",
        target,
    ]
    command.extend(
        [
            "--output",
            f"type=local,dest={destination}",
            REPO_ROOT,
        ]
    )
    return command


def _docker_destination(destination: Path) -> Path:
    if not destination.is_absolute():
        destination = REPO_ROOT / destination
    return destination.resolve()


def build_docker_linux_source(config: DockerBuildConfig) -> Path:
    """Build and export the pinned Linux corresponding-source archive."""
    require_tool(
        "docker",
        "docker was not found on PATH; install Docker with the Linux engine first",
    )
    destination = _docker_destination(config.destination)
    print(f">> building Linux corresponding source into '{destination}'")
    run_checked(
        docker_build_command(config, "linux-source-artifacts"),
        cwd=REPO_ROOT,
    )
    archive = destination / f"nvx-linux-source-{DEFAULT_KERNEL_VERSION}.tar.gz"
    if not archive.is_file():
        raise ScriptError(f"Docker build did not produce {archive.name}")
    print(f">> built {archive} ({format_size(archive.stat().st_size)})")
    return archive


def build_docker_artifacts(
    config: DockerBuildConfig,
    python_only: bool,
    output: Path | None = None,
    python_profile_name: str = DEFAULT_PYTHON_PROFILE,
) -> None:
    require_tool(
        "docker",
        "docker was not found on PATH; install Docker with the Linux engine first",
    )
    python_profile = python_initramfs_profile(python_profile_name)
    target = (
        python_profile.docker_target
        if python_only
        else "artifacts-profiling"
        if config.profiling
        else "artifacts"
    )
    destination = _docker_destination(config.destination)
    if python_only:
        print(
            f">> building the {python_profile_name} Python initramfs into "
            f"'{destination}' (Alpine {config.alpine_version})"
        )
    else:
        kind = " profiling" if config.profiling else ""
        print(
            f">> building Linux{kind} artifacts into '{destination}' "
            f"(kernel {config.kernel_version}, Alpine {config.alpine_version})"
        )
    run_checked(docker_build_command(config, target), cwd=REPO_ROOT)
    expected = (
        (python_profile.artifact_name,)
        if python_only
        else (
            "vmlinux-profiling" if config.profiling else "vmlinux",
            "initramfs.cpio.gz",
        )
    )
    missing = [name for name in expected if not (destination / name).is_file()]
    if missing:
        raise ScriptError(f"Docker build did not produce: {', '.join(missing)}")
    requested_output: Path | None = None
    if output is not None:
        if not python_only:
            raise ScriptError(
                "an explicit output file is only supported for Python artifacts"
            )
        requested_output = output.expanduser().resolve()
        requested_output.parent.mkdir(parents=True, exist_ok=True)
        source = destination / expected[0]
        if source.resolve() != requested_output:
            shutil.copy2(source, requested_output)
    print(">> done:")
    for name in expected:
        path = destination / name
        print(f"  {path} ({format_size(path.stat().st_size)})")
    if requested_output is not None and requested_output != (destination / expected[0]):
        print(f"  {requested_output} ({format_size(requested_output.stat().st_size)})")

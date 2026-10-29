"""Build deterministic Azure bootstrap payloads from shared platform sources."""

import argparse
import base64
import gzip
import hashlib
import io
import json
import lzma
import ntpath
import re
import tarfile
import uuid
from collections.abc import Mapping
from pathlib import Path
from typing import Literal, TypedDict, cast

import yaml

ROOT = Path(__file__).resolve().parent
SETUP = ROOT.parent
REPO = SETUP.parent.parent
# The stdlib-only guest package. Guests run its commands from their bootstrap
# directory as ``python -I -B <directory> <command>``: isolated mode keeps
# environment variables and user site-packages out, and this zipapp-style
# entry point at the directory's root imports the package next to it.
GUEST_PACKAGE = "nvx_azure"
GUEST_ENTRY_POINT = (
    b'"""Run an NVX Azure bootstrap command from this directory."""\n\n'
    b"import sys\n\nfrom nvx_azure.cli import main\n\nsys.exit(main())\n"
)
COMMON_MODULES = ("__init__.py", "cli.py", "common.py")
LINUX_MODULES = (
    *COMMON_MODULES,
    "linux_bootstrap.py",
    "linux_disk.py",
    "linux_firewall.py",
    "linux_mshv.py",
)
LINUX_ENTRY_POINT = "/usr/bin/python3 -I -B /opt/nvx-bootstrap"
WINDOWS_MODULES = (
    *COMMON_MODULES,
    "windows_bootstrap.py",
    "windows_disk.py",
    "windows_firewall.py",
    "windows_stage.py",
    "windows_system.py",
)
# Small PowerShell adapters for the cmdlets that the Windows modules call.
WINDOWS_ADAPTERS = ("firewall.ps1", "openssh.ps1", "storage.ps1", "system.ps1")
# Windows staging consumes these only from the outer Custom Script payload.
# Keeping them out of the package avoids duplicating large SSH keys.
WINDOWS_OUTER_ONLY_FIELDS = frozenset({"publicKeys", "sshDefaultShell"})
# The interpreter of the Windows bootstrap, which runs before the runner
# toolchain installs its own Python: python.org's embeddable package for x64,
# pinned by the SHA-256 that python.org's Windows release manifest publishes
# (https://www.python.org/ftp/python/3.14.10/windows-3.14.10.json).
WINDOWS_PYTHON = {
    "version": "3.14.10",
    "url": "https://www.python.org/ftp/python/3.14.10/python-3.14.10-embeddable-amd64.zip",
    "sha256": "b553e7dac884469b40e0740bb527271c3378f700b427e0485835f1bb6424361c",
}


class LinuxDisk(TypedDict):
    lun: int
    mountPoint: str


class WindowsDisk(TypedDict):
    lun: int
    driveLetter: str
    temporaryDriveFallback: str


class CommonOptionalConfig(TypedDict, total=False):
    benchmarkRunnerName: str


class CommonConfig(CommonOptionalConfig):
    subscriptionId: str
    resourceGroup: str
    vmssName: str
    adminUsername: str
    keyVaultName: str
    runnerNamePrefix: str
    runnerLabels: list[str]
    repository: str
    bootstrapSecretName: str
    runnerNumberOffset: int


class LinuxConfig(CommonConfig):
    os: Literal["linux"]
    backend: str
    kernelMode: str
    runnerDirectory: str
    dataDisk: LinuxDisk


class WindowsConfig(CommonConfig):
    os: Literal["windows"]
    runnerDirectory: str
    dataDisk: WindowsDisk
    benchmarkScratchDirectory: str
    sshDefaultShell: str
    publicKeys: list[str]


RunnerConfig = LinuxConfig | WindowsConfig


class PackageManifest(TypedDict):
    os: str
    rustVersion: str
    resourceId: str
    customDataBytes: int
    customDataSha256: str
    guestFiles: dict[str, str]


COMMON_REQUIRED = {
    "os",
    "subscriptionId",
    "resourceGroup",
    "vmssName",
    "adminUsername",
    "keyVaultName",
    "runnerNamePrefix",
    "runnerLabels",
    "dataDisk",
}
COMMON_OPTIONAL = {
    "repository",
    "bootstrapSecretName",
    "runnerNumberOffset",
    "runnerDirectory",
    "benchmarkRunnerName",
}
PLATFORM_FIELDS = {
    "linux": {"backend", "kernelMode"},
    "windows": {
        "benchmarkScratchDirectory",
        "sshDefaultShell",
        "publicKeys",
    },
}
WINDOWS_EXTENSION_COMMAND = (
    'powershell.exe -ExecutionPolicy Bypass -NoProfile -Command "'
    "$ErrorActionPreference='Stop'; $d='C:\\AzureData'; "
    "$raw=Get-Content -Raw -LiteralPath (Join-Path $d 'CustomData.bin'); "
    "if ($raw.TrimStart().StartsWith('{')) { $payload=$raw } "
    "else { $payload=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($raw.Trim())) }; "
    "$cfg=Join-Path $d 'vmss-bootstrap-config.json'; "
    "[IO.File]::WriteAllText($cfg,$payload,[Text.UTF8Encoding]::new($false)); "
    "$s=Join-Path $d 'vmss-bootstrap.ps1'; $p=$payload|ConvertFrom-Json; "
    "$b=[Convert]::FromBase64String($p.script); $i=[IO.MemoryStream]::new($b); "
    "$g=[IO.Compression.GZipStream]::new($i,[IO.Compression.CompressionMode]::Decompress); "
    "$o=[IO.File]::Create($s); try{$g.CopyTo($o)}finally{$o.Dispose();$g.Dispose();$i.Dispose()}; "
    '& $s -ConfigPath $cfg"'
)


def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"Duplicate configuration field: {key}")
        result[key] = value
    return result


def text(value: object, name: str, pattern: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(pattern, value):
        raise ValueError(f"Invalid {name}")
    return value


def integer(value: object, name: str, minimum: int, maximum: int) -> int:
    if (
        not isinstance(value, int)
        or isinstance(value, bool)
        or not minimum <= value <= maximum
    ):
        raise ValueError(f"{name} must be an integer from {minimum} to {maximum}")
    return value


def linux_path(value: object, name: str) -> str:
    value = text(value, name, r"/[A-Za-z0-9_./-]+")
    if (
        value == "/"
        or any(part in (".", "..") for part in value.split("/"))
        or "//" in value
    ):
        raise ValueError(
            f"{name} must be an absolute path without traversal or empty segments"
        )
    return value.rstrip("/")


def windows_path(value: object, name: str) -> str:
    value = text(value, name, r"[A-Za-z]:\\[A-Za-z0-9_.\\-]+")
    parts = value[3:].split("\\")
    if any(part in (".", "..", "") for part in parts):
        raise ValueError(
            f"{name} must be an absolute drive path without traversal or empty segments"
        )
    if any(part.endswith(".") for part in parts):
        raise ValueError(f"{name} must not contain trailing-period path components")
    for part in parts:
        stem = part.split(".", 1)[0].upper()
        if stem in ("CON", "PRN", "AUX", "NUL") or re.fullmatch(
            r"(?:COM|LPT)[1-9]", stem
        ):
            raise ValueError(f"{name} must not contain reserved DOS device names")
    return ntpath.normpath(value)


def normalize_config(config: object) -> LinuxConfig | WindowsConfig:
    if not isinstance(config, dict):
        raise ValueError("Configuration must be a JSON object")
    decoded = cast(dict[object, object], config)
    if any(not isinstance(key, str) for key in decoded):
        raise ValueError("Configuration keys must be strings")
    values = cast(dict[str, object], decoded)
    os_type = values.get("os")
    if not isinstance(os_type, str) or os_type not in ("linux", "windows"):
        raise ValueError("os must be linux or windows")
    allowed = COMMON_REQUIRED | COMMON_OPTIONAL | PLATFORM_FIELDS[os_type]
    missing = COMMON_REQUIRED - values.keys()
    unknown = values.keys() - allowed
    if missing:
        raise ValueError(f"Missing configuration fields: {', '.join(sorted(missing))}")
    if unknown:
        raise ValueError(f"Unknown configuration fields: {', '.join(sorted(unknown))}")
    serialized = json.dumps(values)
    if re.search(
        r"(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,})|"
        r"-----BEGIN (?:RSA |OPENSSH |EC )?PRIVATE KEY-----|(?i:AccountKey=|[?&]sig=)",
        serialized,
    ):
        raise ValueError(
            "Configuration contains credential-like content; use Key Vault references"
        )
    subscription = text(values["subscriptionId"], "subscriptionId", r"[0-9A-Fa-f-]{36}")
    try:
        subscription = str(uuid.UUID(subscription))
    except ValueError as error:
        raise ValueError("subscriptionId must be a UUID") from error
    resource_group = text(
        values["resourceGroup"], "resourceGroup", r"[A-Za-z0-9_().-]{1,90}"
    )
    if resource_group.endswith("."):
        raise ValueError("resourceGroup must not end with a period")
    vmss_name = text(values["vmssName"], "vmssName", r"[A-Za-z0-9][A-Za-z0-9-]{0,63}")
    admin_username = text(
        values["adminUsername"], "adminUsername", r"[A-Za-z_][A-Za-z0-9_-]{0,31}"
    )
    key_vault = text(
        values["keyVaultName"], "keyVaultName", r"[A-Za-z][A-Za-z0-9-]{1,22}[A-Za-z0-9]"
    )
    if "--" in key_vault:
        raise ValueError("keyVaultName must not contain consecutive hyphens")
    name_prefix = text(
        values["runnerNamePrefix"],
        "runnerNamePrefix",
        r"[A-Za-z0-9][A-Za-z0-9_.-]{0,49}",
    )
    if ".." in name_prefix:
        raise ValueError("runnerNamePrefix must not contain consecutive periods")
    repository = text(
        values.get("repository", "microsoft/nvx"),
        "repository",
        r"[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9][A-Za-z0-9_.-]*",
    )
    secret_name = text(
        values.get("bootstrapSecretName", "github-runner-bootstrap-token"),
        "bootstrapSecretName",
        r"[A-Za-z0-9-]{1,127}",
    )
    offset = integer(values.get("runnerNumberOffset", 1), "runnerNumberOffset", 0, 1)
    labels_value = values["runnerLabels"]
    if not isinstance(labels_value, list) or not labels_value:
        raise ValueError("runnerLabels must be a nonempty array")
    labels = [
        text(label, "runner label", r"[A-Za-z0-9_.-]+")
        for label in cast(list[object], labels_value)
    ]
    if len(set(label.lower() for label in labels)) != len(labels):
        raise ValueError("runnerLabels must not contain duplicates")
    roles = {
        label.lower() for label in labels if label.lower() in {"general", "benchmark"}
    }
    if len(roles) > 1:
        raise ValueError("runnerLabels must not contain both general and benchmark")
    benchmark_name: str | None = None
    if "benchmarkRunnerName" in values:
        benchmark_name = text(
            values["benchmarkRunnerName"],
            "benchmarkRunnerName",
            rf"{re.escape(name_prefix)}-(?:0|[1-9][0-9]{{0,15}})",
        )
        if "general" not in roles or int(benchmark_name.rsplit("-", 1)[1]) < offset:
            raise ValueError(
                "benchmarkRunnerName requires general labels and a valid VMSS instance number"
            )
    disk_value = values["dataDisk"]
    if not isinstance(disk_value, dict):
        raise ValueError("dataDisk must be an object")
    decoded_disk = cast(dict[object, object], disk_value)
    if any(not isinstance(key, str) for key in decoded_disk):
        raise ValueError("dataDisk keys must be strings")
    disk = cast(dict[str, object], decoded_disk)
    lun = integer(disk.get("lun"), "dataDisk.lun", 0, 63)
    common: CommonConfig = {
        "subscriptionId": subscription,
        "resourceGroup": resource_group,
        "vmssName": vmss_name,
        "adminUsername": admin_username,
        "keyVaultName": key_vault,
        "runnerNamePrefix": name_prefix,
        "runnerLabels": labels,
        "repository": repository,
        "bootstrapSecretName": secret_name,
        "runnerNumberOffset": offset,
    }
    if benchmark_name is not None:
        common["benchmarkRunnerName"] = benchmark_name
    if os_type == "linux":
        if admin_username in ("root", "nvx-runner"):
            raise ValueError("Linux adminUsername must not be root or nvx-runner")
        if set(disk) != {"lun", "mountPoint"}:
            raise ValueError("Linux dataDisk must contain only lun and mountPoint")
        mount_point = linux_path(disk["mountPoint"], "dataDisk.mountPoint")
        if not mount_point.startswith(("/mnt/", "/srv/")):
            raise ValueError("Linux dataDisk.mountPoint must be below /mnt or /srv")
        backend = text(values.get("backend"), "backend", r"(?:kvm|mshv)")
        kernel_mode = text(
            values.get("kernelMode", "preinstalled"),
            "kernelMode",
            r"(?:preinstalled|azurelinux-mshv)",
        )
        if kernel_mode == "azurelinux-mshv" and backend != "mshv":
            raise ValueError("azurelinux-mshv requires the mshv backend")
        directory = linux_path(
            values.get("runnerDirectory", "/opt/nvx-runner"), "runnerDirectory"
        )
        if directory == mount_point:
            raise ValueError(
                "runnerDirectory must not be the managed data disk mount point"
            )
        if not directory.startswith(("/opt/", mount_point + "/")):
            raise ValueError(
                "Linux runnerDirectory must be below /opt or the configured data mount"
            )
        for protected in ("/opt/nvx", "/opt/nvx-bootstrap"):
            if directory == protected or directory.startswith(protected + "/"):
                raise ValueError(
                    "runnerDirectory must not overlap the trusted tools or bootstrap directory"
                )
        required_labels = ["linux", backend, "virtual-machine"]
        if [label.lower() for label in labels[:3]] != required_labels:
            raise ValueError(
                f"runnerLabels must start with {', '.join(required_labels)}"
            )
        linux_config: LinuxConfig = {
            **common,
            "os": "linux",
            "backend": backend,
            "kernelMode": kernel_mode,
            "runnerDirectory": directory,
            "dataDisk": {"lun": lun, "mountPoint": mount_point},
        }
        return linux_config
    if set(disk) != {"lun", "driveLetter", "temporaryDriveFallback"}:
        raise ValueError(
            "Windows dataDisk must contain only lun, driveLetter and temporaryDriveFallback"
        )
    drive = text(disk["driveLetter"], "dataDisk.driveLetter", r"[D-Zd-z]").upper()
    fallback = text(
        disk["temporaryDriveFallback"], "dataDisk.temporaryDriveFallback", r"[D-Zd-z]"
    ).upper()
    if drive == fallback:
        raise ValueError("Windows data disk and fallback drive letters must differ")
    directory = windows_path(
        values.get("runnerDirectory", r"C:\actions-runner"), "runnerDirectory"
    )
    if directory[3:].split("\\")[0].lower() in (
        "windows",
        "programdata",
        "users",
        "program files",
    ):
        raise ValueError(
            "Windows runnerDirectory must not be inside an operating system directory"
        )
    scratch_value = values.get("benchmarkScratchDirectory")
    scratch = windows_path(
        rf"{drive}:\nvx-benchmark-scratch" if scratch_value is None else scratch_value,
        "benchmarkScratchDirectory",
    )
    if not scratch.upper().startswith(drive + ":\\"):
        raise ValueError(
            "benchmarkScratchDirectory must be on the configured data drive"
        )
    runner_path = ntpath.normcase(directory)
    scratch_path = ntpath.normcase(scratch)
    work_path = ntpath.join(runner_path, "_work")
    if (
        scratch_path == runner_path
        or scratch_path.startswith(runner_path + "\\")
        or runner_path.startswith(scratch_path + "\\")
    ) and not (scratch_path == work_path or scratch_path.startswith(work_path + "\\")):
        raise ValueError(
            "benchmarkScratchDirectory must not overlap the runner installation outside _work"
        )
    default_shell = text(
        values.get("sshDefaultShell", "powershell"),
        "sshDefaultShell",
        r"(?:powershell|pwsh|cmd)",
    )
    public_key_values = values.get("publicKeys", [])
    if not isinstance(public_key_values, list):
        raise ValueError("publicKeys must be an array")
    public_keys = [
        text(
            key,
            "SSH public key",
            r"(?:ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp(?:256|384|521)) [A-Za-z0-9+/=]+(?: [^\r\n]*)?",
        )
        for key in cast(list[object], public_key_values)
    ]
    for key in public_keys:
        try:
            if not base64.b64decode(key.split(" ")[1], validate=True):
                raise ValueError("Empty SSH public key")
        except ValueError as error:
            raise ValueError("Invalid SSH public key encoding") from error
    required_labels = ["windows", "whp", "virtual-machine"]
    if [label.lower() for label in labels[:3]] != required_labels:
        raise ValueError(f"runnerLabels must start with {', '.join(required_labels)}")
    windows_config: WindowsConfig = {
        **common,
        "os": "windows",
        "runnerDirectory": directory,
        "dataDisk": {
            "lun": lun,
            "driveLetter": drive,
            "temporaryDriveFallback": fallback,
        },
        "benchmarkScratchDirectory": scratch,
        "sshDefaultShell": default_shell,
        "publicKeys": public_keys,
    }
    return windows_config


def load_config(path: str | Path) -> LinuxConfig | WindowsConfig:
    with Path(path).open(encoding="utf-8-sig") as stream:
        return normalize_config(json.load(stream, object_pairs_hook=unique_object))


def source(path: str) -> bytes:
    if path in {"setup-linux.sh", "setup-windows-whp.ps1", "tool-versions.conf"}:
        location = SETUP / path
    elif path == "rust-toolchain.toml":
        location = REPO / path
    elif (
        path.startswith(("linux/", "windows/", GUEST_PACKAGE + "/"))
        and ".." not in Path(path).parts
    ):
        location = ROOT / path
    else:
        raise ValueError(f"Unsupported package source: {path}")
    data = location.read_bytes()
    if path.endswith((".sh", ".py")) and b"\r" in data:
        raise ValueError(f"Shell and Python sources must use LF line endings: {path}")
    if path.endswith(".ps1"):
        data = data.replace(b"\r\n", b"\n")
    return data


def rust_version() -> str:
    releases = re.findall(
        rb"^rust\.toolchain=(\d+\.\d+\.\d+)\r?$", source("tool-versions.conf"), re.M
    )
    channels = re.findall(
        rb'^channel = "([^"]+)"\r?$', source("rust-toolchain.toml"), re.M
    )
    if len(releases) != 1 or channels != releases:
        raise ValueError(
            "The tool manifest and rust-toolchain.toml must pin the same explicit Rust release"
        )
    return releases[0].decode()


def configuration_bytes(config: Mapping[str, object]) -> bytes:
    return (json.dumps(config, sort_keys=True, indent=2) + "\n").encode()


def gzip_bytes(data: bytes) -> bytes:
    stream = io.BytesIO()
    with gzip.GzipFile(
        filename="", mode="wb", fileobj=stream, compresslevel=9, mtime=0
    ) as compressed:
        compressed.write(data)
    return stream.getvalue()


def guest_modules(names: tuple[str, ...]) -> dict[str, bytes]:
    return {
        f"{GUEST_PACKAGE}/{name}": source(f"{GUEST_PACKAGE}/{name}") for name in names
    }


def linux_payload(
    config: LinuxConfig,
) -> tuple[bytes, dict[str, bytes], dict[str, bytes]]:
    files = {
        "config.json": configuration_bytes(config),
        "setup-linux.sh": source("setup-linux.sh"),
        "tool-versions.conf": source("tool-versions.conf"),
        "__main__.py": GUEST_ENTRY_POINT,
        **guest_modules(LINUX_MODULES),
    }
    entries = [
        {
            "path": "/opt/nvx-bootstrap/" + name,
            "owner": "root:root",
            "permissions": "0600"
            if name == "config.json"
            else ("0755" if name.endswith(".sh") else "0644"),
            "encoding": "gz+b64",
            "content": base64.b64encode(gzip_bytes(data)).decode(),
        }
        for name, data in files.items()
    ]
    service = (
        "[Unit]\nDescription=Configure the NVX CI runner\nWants=network-online.target\n"
        "After=network-online.target cloud-final.service waagent.service\n\n"
        "[Service]\nType=oneshot\nRemainAfterExit=yes\n"
        f"ExecStart={LINUX_ENTRY_POINT} linux-bootstrap\n"
        "Restart=on-failure\nRestartSec=60\nTimeoutStartSec=4h\n\n"
        "[Install]\nWantedBy=multi-user.target\n"
    )
    entries.append(
        {
            "path": "/etc/systemd/system/setup-nvx-github-runner.service",
            "owner": "root:root",
            "permissions": "0644",
            "content": service,
        }
    )
    entries.append(
        {
            "path": "/etc/systemd/system/nvx-runner-metadata-block.service",
            "owner": "root:root",
            "permissions": "0644",
            "content": (
                "[Unit]\nDescription=Block Azure metadata for NVX CI runner jobs\n\n"
                "[Service]\nType=oneshot\nRemainAfterExit=yes\n"
                f"ExecStart={LINUX_ENTRY_POINT} linux-block-metadata\n\n"
                "[Install]\nWantedBy=multi-user.target\n"
            ),
        }
    )
    document = {
        "package_upgrade": False,
        "packages": [
            "ca-certificates",
            "curl",
            "python3",
            "util-linux",
            "e2fsprogs",
            "diffutils",
        ],
        "write_files": entries,
        "runcmd": [
            ["systemctl", "daemon-reload"],
            ["systemctl", "enable", "setup-nvx-github-runner.service"],
            ["systemctl", "start", "--no-block", "setup-nvx-github-runner.service"],
        ],
    }
    data = (
        "#cloud-config\n" + yaml.safe_dump(document, sort_keys=False, width=120)
    ).encode()
    return data, files, {"cloud-init.yaml": data}


def windows_package(files: dict[str, bytes]) -> bytes:
    """Return a canonical tar.xz of the files: solid compression keeps the
    payload within Azure's custom-data limit, and fixed metadata and order
    keep it independent of the builder."""
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name in sorted(files):
            entry = tarfile.TarInfo(name)
            entry.size = len(files[name])
            entry.mode = 0o644
            entry.mtime = 0
            archive.addfile(entry, io.BytesIO(files[name]))
    return lzma.compress(
        stream.getvalue(),
        format=lzma.FORMAT_XZ,
        check=lzma.CHECK_CRC64,
        preset=9 | lzma.PRESET_EXTREME,
    )


def windows_payload(
    config: WindowsConfig,
) -> tuple[bytes, dict[str, bytes], dict[str, bytes]]:
    guest_config = {
        key: value
        for key, value in config.items()
        if key not in WINDOWS_OUTER_ONLY_FIELDS
    }
    files = {
        "config.json": configuration_bytes(guest_config),
        "setup-windows-whp.ps1": source("setup-windows-whp.ps1"),
        "tool-versions.conf": source("tool-versions.conf"),
        "__main__.py": GUEST_ENTRY_POINT,
        **guest_modules(WINDOWS_MODULES),
        **{name: source(f"windows/{name}") for name in WINDOWS_ADAPTERS},
    }
    package = windows_package(files)
    payload = {
        "script": base64.b64encode(
            gzip_bytes(source("windows/vmss-bootstrap.ps1"))
        ).decode(),
        "adminUsername": config["adminUsername"],
        "publicKeys": config["publicKeys"],
        "defaultShell": config["sshDefaultShell"],
        "python": WINDOWS_PYTHON,
        "package": {
            "data": base64.b64encode(package).decode(),
            "sha256": hashlib.sha256(package).hexdigest(),
        },
    }
    data = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    outputs = {
        "custom-data.json": data,
        "sources.tar.xz": package,
        "extension-command.txt": (WINDOWS_EXTENSION_COMMAND + "\n").encode(),
    }
    return data, files, outputs


def build_package(config: object, output: str | Path) -> PackageManifest:
    config = normalize_config(config)
    version = rust_version()
    output = Path(output)
    if output.is_symlink():
        raise ValueError("Package output must not be a symbolic link")
    output = output.resolve()
    if output == REPO or output.is_relative_to(REPO):
        raise ValueError("Generated packages must be outside the NVX source directory")
    data, files, generated = (
        linux_payload(config) if config["os"] == "linux" else windows_payload(config)
    )
    if len(data) > 65535:
        raise ValueError(f"Azure customData exceeds 65,535 decoded bytes: {len(data)}")
    patch = {
        "properties": {
            "virtualMachineProfile": {
                "osProfile": {"customData": base64.b64encode(data).decode()}
            }
        }
    }
    manifest: PackageManifest = {
        "os": config["os"],
        "rustVersion": version,
        "resourceId": (
            f"/subscriptions/{config['subscriptionId']}/resourceGroups/{config['resourceGroup']}"
            f"/providers/Microsoft.Compute/virtualMachineScaleSets/{config['vmssName']}"
        ),
        "customDataBytes": len(data),
        "customDataSha256": hashlib.sha256(data).hexdigest(),
        "guestFiles": {
            name: hashlib.sha256(content).hexdigest() for name, content in files.items()
        },
    }
    generated["model-patch.json"] = (json.dumps(patch, indent=2) + "\n").encode()
    generated["manifest.json"] = (json.dumps(manifest, indent=2) + "\n").encode()
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        raise ValueError(f"Package output directory must be empty: {output}")
    for name, content in generated.items():
        target = output / name
        if target.is_symlink():
            raise ValueError(f"Output must not be a symbolic link: {target}")
        temporary = output / (name + ".tmp")
        if temporary.exists() or temporary.is_symlink():
            raise ValueError(f"Temporary output already exists: {temporary}")
        try:
            with temporary.open("xb") as stream:
                stream.write(content)
            temporary.replace(target)
        finally:
            if temporary.exists():
                temporary.unlink()
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--config", type=Path, required=True, help="VMSS configuration JSON"
    )
    parser.add_argument(
        "--output", type=Path, help="Package directory outside the source tree"
    )
    parser.add_argument(
        "--validate-only", action="store_true", help="Validate without generating files"
    )
    args = parser.parse_args()
    if not args.validate_only and args.output is None:
        parser.error("--output is required unless --validate-only is used")
    try:
        config = load_config(args.config)
        if args.validate_only:
            print(
                f"Valid {config['os']} configuration; shared Rust release {rust_version()}"
            )
        else:
            manifest = build_package(config, args.output)
            print(
                f"Built {manifest['os']} package: {manifest['customDataBytes']} decoded bytes; Rust {manifest['rustVersion']}"
            )
    except (ValueError, OSError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()

"""Provision an Azure Linux scale-set instance as an NVX CI runner.

The controller verifies the instance identity, prepares the data disk and the
backend, installs the runner's metadata block, and only then requests a
registration token, which it passes to the generic setup on standard input.
"""

import os
import re
import stat
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from . import common, linux_disk, linux_mshv
from .common import BootstrapError, JsonObject

# Where cloud-init writes the bootstrap files, as commands name it.
SETUP_ROOT = PurePosixPath("/opt/nvx-bootstrap")
BOOTSTRAP_LOG = Path("/var/log/nvx-runner-bootstrap.log")
METADATA_DIRECTORY = Path("/var/lib/nvx-ci-host-metadata")
SYSTEMD_ROOT = Path("/etc/systemd/system")
# The units and the runner's drop-in start every command through this
# interpreter, with isolated mode and without writing bytecode, from the
# root-owned bootstrap directory that cloud-init writes.
PYTHON = "/usr/bin/python3"
ENTRY_POINT = (PYTHON, "-I", "-B", str(SETUP_ROOT))
METADATA_SERVICE = "nvx-runner-metadata-block.service"
PROVISIONING_SERVICE = "setup-nvx-github-runner.service"
RUNNER_ACCOUNT = "nvx-runner"


@dataclass(frozen=True)
class Settings:
    backend: str
    admin_username: str
    repository: str
    runner_directory: str
    lun: int
    mount_point: str
    kernel_mode: str

    @property
    def required_labels(self) -> list[str]:
        return ["linux", self.backend, "virtual-machine"]


def read_settings(config: JsonObject) -> Settings:
    """Validate the configuration values that provisioning relies on."""
    backend = config.get("backend")
    if config.get("os") != "linux" or backend not in ("kvm", "mshv"):
        raise BootstrapError("Expected a Linux KVM or MSHV configuration")
    admin_username = common.text(config, "adminUsername")
    if admin_username in ("root", RUNNER_ACCOUNT):
        raise BootstrapError("The runner administrator must not be root or nvx-runner")
    disk = common.json_object(config.get("dataDisk"), "The dataDisk configuration")
    settings = Settings(
        backend=common.text(config, "backend"),
        admin_username=admin_username,
        repository=common.text(config, "repository"),
        runner_directory=common.text(config, "runnerDirectory"),
        lun=common.integer(disk, "lun", "dataDisk configuration"),
        mount_point=common.text(disk, "mountPoint", "dataDisk configuration"),
        kernel_mode=common.text(config, "kernelMode"),
    )
    common.split_labels(
        common.strings(config, "runnerLabels"), settings.required_labels
    )
    return settings


def setup_arguments(settings: Settings, extra_labels: str) -> list[str]:
    arguments = [
        "--runner-only",
        "--backend",
        settings.backend,
        "--repository-url",
        f"https://github.com/{settings.repository}",
        "--runner-directory",
        settings.runner_directory,
    ]
    if extra_labels:
        arguments += ["--extra-runner-labels", extra_labels]
    return arguments


def write_metadata_snapshot(
    safe: JsonObject,
    directory: Path = METADATA_DIRECTORY,
    owner: tuple[int, int] | None = (0, 0),
) -> None:
    """Atomically replace the root-owned compute snapshot that jobs may read."""
    if directory.is_symlink():
        raise BootstrapError("host metadata directory must not be a symlink")
    directory.mkdir(mode=0o755, exist_ok=True)
    if not stat.S_ISDIR(os.lstat(directory).st_mode):
        raise BootstrapError("host metadata directory is not a directory")
    if owner is not None and sys.platform != "win32":
        os.chown(directory, *owner)
    os.chmod(directory, 0o755)
    descriptor, temporary = tempfile.mkstemp(prefix="compute.", dir=directory)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(common.snapshot_bytes(safe))
        os.chmod(temporary, 0o644)
        os.replace(temporary, directory / "compute.json")
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise


def refresh_metadata(
    config: JsonObject,
    settings: Settings,
    directory: Path = METADATA_DIRECTORY,
    owner: tuple[int, int] | None = (0, 0),
) -> tuple[str, str, bool]:
    """Verify the instance identity and refresh the compute snapshot.

    Returns the runner name, its extra labels, and whether it is the
    benchmark runner.
    """
    metadata = common.compute_metadata()
    name = common.runner_name(config, metadata)
    extra_labels, benchmark = common.split_labels(
        common.instance_labels(config, name), settings.required_labels
    )
    write_metadata_snapshot(common.safe_compute_metadata(metadata), directory, owner)
    return name, extra_labels, benchmark


def admin_uid(username: str) -> int:
    if sys.platform == "win32":
        raise BootstrapError("Linux runner bootstrap requires Linux")
    import pwd

    try:
        uid = pwd.getpwnam(username).pw_uid
    except KeyError as missing:
        raise BootstrapError(
            f"the runner administrator {username} does not exist"
        ) from missing
    if uid == 0:
        raise BootstrapError("the runner administrator must not be root")
    return uid


def is_character_device(path: Path) -> bool:
    try:
        return stat.S_ISCHR(os.stat(path).st_mode)
    except OSError:
        return False


def unit_argument(value: str) -> str:
    if not re.fullmatch(r"[A-Za-z0-9_./:=+-]+", value):
        raise BootstrapError(f"cannot pass {value!r} in a systemd unit")
    return value


def finalize_runner(
    runner_directory: Path, config_path: Path, systemd_root: Path = SYSTEMD_ROOT
) -> None:
    """Require the metadata block for every start of the registered runner.

    The runner keeps running: registration started it, and restarting it
    could interrupt a job that it already accepted.
    """
    lines = (runner_directory / ".service").read_text(encoding="utf-8").split("\n")
    service = lines[0].strip(" \t")
    if (
        not re.fullmatch(r"actions\.runner\.[A-Za-z0-9_.-]+\.service", service)
        or ".." in service
        or common.run_command(
            ["systemctl", "show", service, "--property=User", "--value"], check=False
        )
        .text()
        .strip()
        != RUNNER_ACCOUNT
    ):
        raise BootstrapError("the registered runner service is not owned by nvx-runner")
    entry = " ".join(ENTRY_POINT)
    drop_in = systemd_root / f"{service}.d"
    drop_in.mkdir(parents=True, exist_ok=True)
    (drop_in / "azure-metadata.conf").write_text(
        "[Unit]\n"
        f"Requires={METADATA_SERVICE}\n"
        "Wants=network-online.target\n"
        f"After={METADATA_SERVICE} network-online.target\n"
        "[Service]\n"
        f"ExecStartPre=+{entry} linux-block-metadata\n"
        f"ExecStartPre=+{entry} linux-refresh-metadata "
        f"--config {unit_argument(config_path.as_posix())}\n",
        encoding="utf-8",
    )
    common.run_command(["systemctl", "daemon-reload"])
    if common.run_command(
        ["systemctl", "is-active", "--quiet", service], check=False
    ).returncode:
        raise BootstrapError(f"the registered runner service {service} is not active")
    common.run_command(["systemctl", "disable", PROVISIONING_SERVICE])
    common.marker("NVX_CLOUD_RUNNER_SETUP_COMPLETE")


def run_setup(admin: str, arguments: list[str], token: str | None = None) -> None:
    """Run the generic setup as the administrator; a token travels on stdin only."""
    command = [
        "sudo",
        "-u",
        admin,
        "-H",
        "sh",
        str(SETUP_ROOT / "setup-linux.sh"),
    ]
    if token is None:
        common.run_command([*command, *arguments], capture=False)
    else:
        common.run_command(
            [*command, *arguments, "--runner-token-stdin"],
            stdin=(token + "\n").encode(),
            capture=False,
        )


def prepare_backend(
    settings: Settings, paths: linux_mshv.MshvPaths | None = None
) -> bool:
    """Prepare the backend device; False means that a reboot was scheduled."""
    if settings.backend == "mshv" and settings.kernel_mode == "azurelinux-mshv":
        paths = paths or linux_mshv.MshvPaths()
        if not is_character_device(paths.device):
            if paths.done.exists():
                raise BootstrapError(
                    "MSHV is unavailable after host-stack enablement; "
                    "inspect the Dom0 boot path"
                )
            linux_mshv.setup(paths)
            common.run_command(
                [
                    "shutdown",
                    "-r",
                    "+1",
                    "Booting the MSHV Dom0 entry for the CI runner",
                ]
            )
            common.marker("NVX_CLOUD_RUNNER_REBOOT_SCHEDULED")
            return False
        if not linux_mshv.verify(paths):
            raise BootstrapError("MSHV verification failed")
    if not is_character_device(Path("/dev") / settings.backend):
        raise BootstrapError(f"/dev/{settings.backend} is unavailable")
    return True


def bootstrap(config_path: Path) -> None:
    config_path = Path(os.path.abspath(config_path))
    config = common.load_json(config_path, "The configuration")
    settings = read_settings(config)
    admin_uid(settings.admin_username)
    name, extra_labels, benchmark = refresh_metadata(config, settings)
    linux_disk.setup(settings.lun, settings.mount_point)
    if not linux_disk.is_mounted(settings.mount_point):
        raise BootstrapError("managed data disk is not mounted")
    if not prepare_backend(settings):
        return
    arguments = setup_arguments(settings, extra_labels)
    run_setup(settings.admin_username, arguments)
    # Jobs must not reach instance metadata, so the block is effective
    # before any credential is requested or the runner registers.
    common.run_command(["systemctl", "enable", METADATA_SERVICE])
    common.run_command(["systemctl", "restart", METADATA_SERVICE])
    common.run_command(["systemctl", "is-active", "--quiet", METADATA_SERVICE])
    token = common.registration_token(config)
    registration = [*arguments, "--runner-name", name]
    if benchmark:
        registration.append("--benchmark-runner")
    run_setup(settings.admin_username, registration, token)
    del token
    finalize_runner(Path(settings.runner_directory), config_path)


def redirect_output(path: Path) -> None:
    """Send this process's and its children's output to the bootstrap log."""
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    sys.stdout.flush()
    sys.stderr.flush()
    os.dup2(descriptor, 1)
    os.dup2(descriptor, 2)
    os.close(descriptor)


def require_root() -> None:
    if sys.platform == "win32":
        raise BootstrapError("Linux runner bootstrap requires Linux")
    if os.geteuid() != 0:
        raise BootstrapError("Linux runner bootstrap must run as root")


def bootstrap_main(config_path: Path) -> int:
    redirect_output(BOOTSTRAP_LOG)
    require_root()
    bootstrap(config_path)
    return 0


def refresh_main(config_path: Path) -> int:
    """Refresh only the compute snapshot, as before every runner start."""
    redirect_output(BOOTSTRAP_LOG)
    require_root()
    config = common.load_json(Path(os.path.abspath(config_path)), "The configuration")
    settings = read_settings(config)
    admin_uid(settings.admin_username)
    refresh_metadata(config, settings)
    return 0

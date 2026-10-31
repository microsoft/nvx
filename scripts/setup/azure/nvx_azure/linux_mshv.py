"""Install the Azure Linux 3 MSHV host stack and verify its Dom0 boot.

Booting the mshv kernel alone is not enough: GRUB has to chainload
HvLoader.efi first, so that the hypervisor runs before the Linux root
partition starts. kernel-mshv ships /etc/grub.d/50_mariner_mshv_menuentry,
which generates that entry, and /etc/default/grub.d/50_mariner_mshv.cfg,
which makes it the default.

Setup deliberately fails closed. The generated Dom0 entry is fail-open by
design: if HvLoader cannot start the hypervisor, GRUB falls through and boots
Linux bare, giving an mshv kernel with no /dev/mshv. Any ambiguity therefore
stops setup instead of requesting an optimistic reboot.
"""

import glob
import platform
import re
import shutil
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from . import common
from .common import BootstrapError, Log

# The MSHV host stack that CI measures, by package name: the hypervisor and
# its loader, the root partition's kernel, and the EFI loader that starts the
# hypervisor. The expected Linux/MSHV benchmark host,
# data/linux-mshv-virtual-machine-host.json, requires this hypervisor build
# and kernel series, so move a pin only in the pull request that accepts the
# new host and resets the series (doc/benchmarks.md, "Expected benchmark
# hosts"). Unpinned, every reimage installed the newest releases (#980).
MSHV_PINS = {
    "mshv": "26100.9444.2609032029.1-1.azl3",
    "mshv-bootloader-lx": "26100.9444.2609032029.1-1.azl3",
    "kernel-mshv": "6.18.34.mshv3-1.azl3",
    "edk2-hvloader": "20240524git3e722403cd16-21.azl3",
}
INSTALL_ATTEMPTS = 3
RETRY_SECONDS = 30


@dataclass(frozen=True)
class MshvPaths:
    log: Path = Path("/var/log/enable-mshv.log")
    # Means that configuration succeeded, not that MSHV runs: only the
    # verification after the reboot confirms that.
    done: Path = Path("/var/log/enable-mshv.done")
    grub_cfg: Path = Path("/boot/grub2/grub.cfg")
    efi: Path = Path("/boot/efi")
    boot: Path = Path("/boot")
    firmware: Path = Path("/sys/firmware/efi")
    mounts: Path = Path("/proc/mounts")
    cpuinfo: Path = Path("/proc/cpuinfo")
    device: Path = Path("/dev/mshv")


def pinned_packages() -> list[str]:
    return [f"{name}-{release}.x86_64" for name, release in MSHV_PINS.items()]


def preflight(paths: MshvPaths) -> None:
    """Check the cheap invariants whose violation fails confusingly later."""
    arch = platform.machine()
    if arch != "x86_64":
        raise BootstrapError(f"MSHV requires x86_64, found {arch}")
    if not paths.firmware.is_dir():
        raise BootstrapError("not booted in UEFI mode, MSHV needs the ESP")
    if f" {paths.efi.as_posix()} " not in paths.mounts.read_text(encoding="utf-8"):
        raise BootstrapError(f"{paths.efi} is not mounted, cannot stage HvLoader")
    if not re.search(
        r"^flags\s*:.*\s(vmx|svm)(\s|$)",
        paths.cpuinfo.read_text(encoding="utf-8"),
        re.M,
    ):
        raise BootstrapError(
            "no vmx/svm flag: this VM size does not support nested virtualization"
        )


class _StandardOutput:
    """Write streamed command bytes to the redirected bootstrap output."""

    def write(self, data: bytes) -> int:
        return sys.stdout.write(data.decode("utf-8", "replace"))

    def flush(self) -> None:
        sys.stdout.flush()


def _install_attempt(command: list[str], log: Log) -> common.Completed:
    outputs: list[common.Writable] = [_StandardOutput()]
    if log.path is None:
        return common.run_command(command, check=False, capture=False, outputs=outputs)
    with log.path.open("ab") as stream:
        outputs.append(stream)
        return common.run_command(command, check=False, capture=False, outputs=outputs)


def install_pinned_stack(log: Log) -> None:
    """Install exactly the pinned boot components, with bounded retries."""
    packages = pinned_packages()
    log("pinned MSHV stack:\n" + "".join(f"  {package}\n" for package in packages))
    for attempt in range(1, INSTALL_ATTEMPTS + 1):
        result = _install_attempt(["dnf", "install", "-y", *packages], log)
        if result.returncode == 0:
            break
        if attempt == INSTALL_ATTEMPTS:
            raise BootstrapError(
                f"could not install MSHV boot components after {INSTALL_ATTEMPTS} attempts"
            )
        log(f"dnf attempt {attempt} failed, retrying in {RETRY_SECONDS}s")
        common.sleep(RETRY_SECONDS)
    listing = common.run_command(["rpm", "-qa"], check=False).text()
    log(
        "--- installed MSHV stack ---\n"
        + "\n".join(
            sorted(
                line
                for line in listing.splitlines()
                if re.search("mshv|hvloader", line, re.I)
            )
        )
    )
    # Fail closed unless exactly the pinned releases are installed:
    # kernel-mshv is an install-only package, so dnf keeps another release
    # beside the pinned one, and GRUB could boot it.
    query = common.run_command(
        [
            "rpm",
            "-q",
            "--queryformat",
            "%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}\\n",
            *MSHV_PINS,
        ],
        check=False,
    )
    installed = sorted(query.text().splitlines())
    if query.returncode != 0:
        raise BootstrapError(
            f"a pinned MSHV package is not installed: {', '.join(installed)}"
        )
    if installed != sorted(packages):
        raise BootstrapError(
            f"the installed MSHV stack is not the pinned one: {', '.join(installed)}"
        )
    log("installed the pinned MSHV stack")


def dom0_entry(config: str) -> list[str]:
    """Return the lines of the generated Dom0 menu entry."""
    body: list[str] = []
    for line in config.splitlines():
        if body or line.startswith('menuentry "Dom0"'):
            body.append(line)
            if line.startswith("}"):
                break
    return body


def validate_dom0_entry(config: str, paths: MshvPaths) -> list[str]:
    body = dom0_entry(config)
    if not body:
        raise BootstrapError(
            f"no 'Dom0' menuentry in {paths.grub_cfg} (kernel-mshv grub snippet did "
            f"not run; check {paths.log} for the HvLoader.efi warning)"
        )
    if not any(re.search(r"chainloader .*HvLoader\.efi", line) for line in body):
        raise BootstrapError("Dom0 entry does not chainload HvLoader.efi")
    if not any("MSHV_SCHEDULER_TYPE=ROOT" in line for line in body):
        raise BootstrapError("Dom0 entry does not request the root scheduler")
    if not any(re.match(r"\s*linux(16|efi)?\s.*mshv", line) for line in body):
        raise BootstrapError("Dom0 entry does not load an mshv kernel")
    return body


def active_defaults(config: str) -> list[str]:
    """Return the values of the configuration's ``set default=`` commands."""
    values: list[str] = []
    for line in config.splitlines():
        if re.match(r"\s*set\s+default=", line):
            value = re.sub(r"\s*#.*", "", line.split("=", 1)[1], count=1)
            values.append(value.replace('"', "").replace("'", "").strip())
    return values


def verify_dom0_default(paths: MshvPaths) -> None:
    """Require Dom0 as the default that the active GRUB configuration boots.

    A saved entry matters only when the generated configuration uses it.
    """
    config = paths.grub_cfg.read_text(encoding="utf-8")
    default_count = 0
    saved_default = False
    for value in active_defaults(config):
        if value == "Dom0":
            default_count += 1
        elif value in ("$saved_entry", "${saved_entry}"):
            default_count += 1
            saved_default = True
        elif value in ("$next_entry", "${next_entry}"):
            if default_count:
                raise BootstrapError("GRUB overrides its default after selecting Dom0")
        else:
            raise BootstrapError(f"GRUB default '{value}' does not select Dom0")
    if default_count != 1:
        raise BootstrapError("GRUB has no unique Dom0 default")
    if saved_default:
        if not re.search(r"^\s*load_env\s*(#.*)?$", config, re.M):
            raise BootstrapError(
                "GRUB uses saved_entry without loading the default grubenv"
            )
        if common.run_command(["grub2-set-default", "Dom0"], check=False).returncode:
            raise BootstrapError("could not set the saved Dom0 default")
    listing = common.run_command(["grub2-editenv", "-", "list"], check=False)
    if listing.returncode:
        raise BootstrapError("could not read grubenv")
    environment = listing.text().splitlines()
    if saved_default and "saved_entry=Dom0" not in environment:
        raise BootstrapError("the saved GRUB default is not Dom0")
    next_entry = "\n".join(
        line.removeprefix("next_entry=")
        for line in environment
        if line.startswith("next_entry=")
    )
    if next_entry not in ("", "Dom0"):
        raise BootstrapError(f"the one-time GRUB entry '{next_entry}' overrides Dom0")


def configure_boot(paths: MshvPaths, log: Log) -> None:
    # Confirm the pieces that the Dom0 boot path depends on. A partial
    # install would otherwise only show up as a silent fail-open boot.
    for loader in ("HvLoader.efi", "lxhvloader.dll"):
        if not (paths.efi / loader).is_file():
            raise BootstrapError(f"{loader} missing from the ESP after install")
    if not glob.glob(str(paths.boot / "vmlinuz-*mshv*")):
        raise BootstrapError(f"no mshv kernel found in {paths.boot} after install")
    if not paths.grub_cfg.is_file():
        raise BootstrapError(
            f"{paths.grub_cfg} not found, cannot configure the Dom0 boot entry"
        )
    # Keep the first backup: a rerun's configuration may already be modified.
    backup = paths.grub_cfg.with_name(paths.grub_cfg.name + ".pre-mshv")
    if backup.exists():
        log(f"{backup} already exists, keeping the original backup")
    else:
        shutil.copy2(paths.grub_cfg, backup)
        log(f"saved pristine grub.cfg to {backup}")
    # The kernel-mshv scriptlet runs grub2-mkconfig during the transaction,
    # before edk2-hvloader places HvLoader.efi on the ESP, so its menu entry
    # generator skips the Dom0 entry. Regenerate now that every package is
    # on disk so that the result is deterministic.
    log("--- regenerating grub config with HvLoader.efi present ---")
    result = common.run_command(
        ["grub2-mkconfig", "-o", paths.grub_cfg.as_posix()], check=False
    )
    log((result.stdout + result.stderr).decode("utf-8", "replace").strip())
    if result.returncode:
        raise BootstrapError("grub2-mkconfig failed")
    log("--- validating the Dom0 boot entry ---")
    config = paths.grub_cfg.read_text(encoding="utf-8")
    log("Dom0 entry validated:\n" + "\n".join(validate_dom0_entry(config, paths)))
    # Catch a syntactically broken configuration before rebooting into it.
    if common.which("grub2-script-check") is not None:
        if common.run_command(
            ["grub2-script-check", paths.grub_cfg.as_posix()], check=False
        ).returncode:
            raise BootstrapError(f"{paths.grub_cfg} failed grub2-script-check")
        log("grub2-script-check passed")
    verify_dom0_default(paths)
    log("Dom0 is the default boot entry")
    entries = [
        f"{number}:{line}"
        for number, line in enumerate(config.splitlines(), start=1)
        if line.startswith("menuentry ")
    ]
    log("--- resulting menuentries ---\n" + "\n".join(entries))


def setup(paths: MshvPaths | None = None) -> None:
    """Install the pinned host stack and make Dom0 the default boot entry."""
    paths = paths or MshvPaths()
    log = Log(paths.log)
    log(f"=== enable-mshv started at {datetime.now(timezone.utc).isoformat()} ===")
    try:
        log("--- preflight ---")
        preflight(paths)
        log("preflight OK (arch=x86_64, UEFI, ESP mounted, nested virt available)")
        log("--- installing MSHV boot components ---")
        install_pinned_stack(log)
        configure_boot(paths, log)
    except BootstrapError as error:
        log(f"ERROR: {error}")
        log("=== enable-mshv FAILED, not rebooting ===")
        raise
    paths.done.touch()
    log("=== enable-mshv finished, rebooting into Dom0 ===")


def verify(paths: MshvPaths | None = None) -> bool:
    """Report whether this VM booted as the MSHV root partition."""
    paths = paths or MshvPaths()
    passed = True
    kernel = platform.release()
    print(f"== kernel ==\n{kernel}")
    if "mshv" in kernel:
        print("PASS: running an mshv kernel")
    else:
        print("FAIL: not an mshv kernel, the Dom0 boot entry was not used")
        passed = False
    # The device is decisive: the Dom0 entry is fail-open, so the kernel
    # check passes while the hypervisor never started, for example when
    # Secure Boot blocks HvLoader.efi.
    print("\n== mshv device nodes ==")
    if paths.device.exists():
        diagnostic = paths.device.with_name(paths.device.name + "_diag")
        nodes = [
            node.as_posix() for node in (paths.device, diagnostic) if node.exists()
        ]
        print(common.run_command(["ls", "-l", *nodes], check=False).text().rstrip())
        print(f"PASS: {paths.device} present, this OS is the MSHV root partition")
    else:
        print(f"FAIL: {paths.device} missing, the hypervisor did not start")
        print("      Most likely cause: Secure Boot is enabled. Check the serial")
        print("      console for 'SHIM_LOCK verification failed'.")
        passed = False
    print("\n== dmesg ==")
    messages = common.run_command(["dmesg"], check=False).text().splitlines()
    if any(re.search("running as root partition", line, re.I) for line in messages):
        matching = [
            line for line in messages if re.search("mshv|root partition", line, re.I)
        ]
        print("\n".join(matching[:20]))
        print("PASS: kernel reports running as root partition")
    else:
        print("FAIL: no 'running as root partition' line in dmesg")
        passed = False
    print("\n== packages ==")
    # Hosts without RPM, such as Ubuntu, have no package list to report.
    if common.which("rpm") is not None:
        packages = common.run_command(["rpm", "-qa"], check=False).text().splitlines()
        print(
            "\n".join(
                sorted(
                    line for line in packages if re.search("mshv|hvloader", line, re.I)
                )
            )
        )
    if passed:
        print("\nRESULT: PASS - MSHV is running and /dev/mshv is available")
    else:
        print("\nRESULT: FAIL - see the failures above")
    return passed

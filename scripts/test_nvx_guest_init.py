#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools_testing import (
    posix_shell as _posix_shell,  # noqa: E402
)
from nvx_tools_testing import (
    shell_function as _shell_function,
)


class GuestInitRoutingTests(unittest.TestCase):
    INIT = Path(__file__).parents[1] / "guest" / "common" / "init"

    def setUp(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        self.shell = shell
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.pid = self.root / "daemon.pid"
        self.announcer = self.root / "announcer.pid"
        (self.root / "net" / "eth0").mkdir(parents=True)
        (self.root / "os-release").write_text("ID=alpine\n", encoding="utf-8")
        (self.root / "alpine-release").write_text("test\n", encoding="utf-8")
        # The kernel's default, so that the init opens the ICMP echo sockets
        # here rather than in the host's own setting.
        (self.root / "ping_group_range").write_text("1\t0\n", encoding="utf-8")
        self.helpers: dict[str, Path] = {}
        for name, script in {
            "nvx-time": f"printf '4242\\n' >{shlex.quote(self.pid.as_posix())}\n",
            "nvx-init": 'printf "new-init"; printf " <%s>" "$@"; echo\n',
            "nvx-ingress": (
                f'printf "%s\\n" "$$" >{shlex.quote(self.announcer.as_posix())}\n'
            ),
            "nvx-hostmount": "echo hostmount\n",
            "shell": "echo blockless-shell\n",
        }.items():
            path = self.root / name
            path.write_text(f"#!/bin/sh\n{script}", encoding="utf-8")
            path.chmod(0o755)
            self.helpers[name] = path

    def _run(self, cmdline: str) -> subprocess.CompletedProcess[str]:
        command_line = self.root / "cmdline"
        command_line.write_text(cmdline, encoding="utf-8")
        source = self.INIT.read_text(encoding="utf-8")
        replacements = {
            "/sbin/nvx-init": self.helpers["nvx-init"],
            "/sbin/nvx-time": self.helpers["nvx-time"],
            "/sbin/nvx-ingress": self.helpers["nvx-ingress"],
            "/sbin/nvx-hostmount": self.helpers["nvx-hostmount"],
            "/proc/cmdline": command_line,
            "/proc/sys/kernel/modules_disabled": self.root / "modules_disabled",
            "/proc/sys/net/ipv4/ping_group_range": self.root / "ping_group_range",
            "/sys/class/net": self.root / "net",
            "/run/nvx/time/daemon.pid": self.pid,
            "/etc/os-release": self.root / "os-release",
            "/etc/alpine-release": self.root / "alpine-release",
            "exec /bin/sh": self.helpers["shell"],
        }
        for original, replacement in replacements.items():
            target = shlex.quote(replacement.as_posix())
            if original == "exec /bin/sh":
                target = f"exec {target}"
            source = source.replace(original, target)
        source = source.replace("/sbin/nvx-exit", "nvx_exit")
        source = source.replace("/sbin/nvx-snapshot", "nvx_snapshot")
        return subprocess.run(
            [self.shell, "-s"],
            input=(
                "mount() { :; }\nifconfig() { :; }\nroute() { :; }\n"
                'nvx_exit() { exit "$1"; }\n'
                "nvx_snapshot() { echo blockless-snapshot; }\n"
                f"{source}"
            ),
            text=True,
            capture_output=True,
            check=False,
            timeout=40,
        )

    def test_tokenless_layered_and_fresh_sandboxes_use_new_init(self):
        for tokens in (
            "nvx_layer=distro,0xd0003000,erofs nvx_overlay_upper=ramfs",
            "nvx_snapshot_tier=platform",
            "nvx_snapshot_tier=platform nvx_mode=managed",
        ):
            with self.subTest(tokens=tokens):
                result = self._run(f"console=hvc0 nvx_sandbox=1 {tokens}")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "new-init <4242>\n")

    def test_old_lifecycle_tokens_no_longer_select_the_old_layered_init(self):
        for lifecycle in ("managed", "one-shot", ""):
            with self.subTest(lifecycle=lifecycle):
                result = self._run(
                    "nvx_sandbox=1 nvx_layer=distro,0xd0003000,erofs "
                    f"nvx_overlay_upper=ramfs nvx_lifecycle={lifecycle}"
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "new-init <4242>\n")

    def test_openvmm_workload_identity_needs_the_same_nvx_identity(self):
        # OpenVMM's identity tokens no longer name the identity: without the
        # same nvx_identity=, a boot of any route fails closed before it
        # starts anything.
        refused = (
            "NVX-WORKLOAD-ERROR: nvx_identity must name OpenVMM's workload identity\n"
        )
        openvmm = "nvx_workload_uid=4242 nvx_workload_gid=4242"
        for tokens in (
            f"nvx_exec=/bin/true {openvmm}",
            "nvx_exec=/bin/true nvx_workload_uid=4242",
            f"nvx_exec=/bin/true nvx_identity=4243:4242 {openvmm}",
            f"{openvmm} nvx_identity=4242:4243",
            f"nvx_entrypoint=/bin/true {openvmm}",
            f"nvx_mode=managed nvx_control_tty=hvc2 {openvmm}",
            f"nvx_sandbox=1 nvx_snapshot_tier=platform {openvmm}",
        ):
            with self.subTest(tokens=tokens):
                result = self._run(tokens)
                self.assertEqual(result.returncode, 125, result.stdout)
                self.assertEqual(result.stdout, "")
                self.assertIn(refused, result.stderr)
        # The same identity boots on, here to the one-shot workload's check
        # of its entrypoint.
        result = self._run(f"{openvmm} nvx_identity=4242:4242 nvx_entrypoint=bin/true")
        self.assertEqual(result.returncode, 125, result.stdout)
        self.assertIn("hostmount\n", result.stdout)
        self.assertNotIn(refused, result.stderr)
        self.assertIn(
            "NVX-WORKLOAD-ERROR: nvx_entrypoint must be an absolute path\n",
            result.stderr,
        )

    def test_exec_override_cannot_bypass_the_new_sandbox_init(self):
        result = self._run(
            "nvx_sandbox=1 nvx_snapshot_tier=platform nvx_exec=/bin/true"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "new-init <4242>\n")
        result = self._run("nvx_sandbox=1 nvx_lifecycle=managed nvx_exec=/bin/true")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "new-init <4242>\n")
        result = self._run("nvx_exec=/bin/true")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")

    def test_blockless_platform_and_snapshot_vms_keep_their_routes(self):
        for tokens in ("", "nvx_snapshot_tier=platform", "shellsnap snap"):
            with self.subTest(tokens=tokens):
                result = self._run(f"console=hvc0 {tokens}")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("hostmount\n", result.stdout)
                self.assertIn("blockless-shell\n", result.stdout)
                self.assertNotIn("new-init", result.stdout)
                if tokens == "shellsnap snap":
                    self.assertEqual(result.stdout.count("blockless-snapshot\n"), 2)
                    self.assertIn("SNAP-RESTORE-OK\n", result.stdout)

    def test_new_init_receives_the_announcers_process_id_when_present(self):
        for ingress in ("allow", "host-loopback"):
            with self.subTest(ingress=ingress):
                result = self._run(
                    "nvx_sandbox=1 nvx_snapshot_tier=platform "
                    "virtnet_ip=172.16.0.2 virtnet_gw=172.16.0.1 "
                    f"virtnet_ingress={ingress}"
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                pid = self.announcer.read_text(encoding="utf-8").strip()
                self.assertGreater(int(pid), 1)
                self.assertIn(f"new-init <4242> <{pid}>\n", result.stdout)

    def test_new_init_waits_for_the_time_daemons_deferred_publication(self):
        temporary = self.pid.with_name("daemon.pid.tmp")
        self.helpers["nvx-time"].write_text(
            "#!/bin/sh\n"
            f"(sleep 0.1; printf '4242\\n' >{shlex.quote(temporary.as_posix())}; "
            f"mv {shlex.quote(temporary.as_posix())} "
            f"{shlex.quote(self.pid.as_posix())}) &\n",
            encoding="utf-8",
        )
        result = self._run("nvx_sandbox=1 nvx_snapshot_tier=platform")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "new-init <4242>\n")

    def test_missing_helpers_or_selected_init_fail_without_fallback(self):
        self.helpers["nvx-time"].write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        self.pid.mkdir()
        result = self._run("nvx_sandbox=1 nvx_snapshot_tier=platform")
        self.assertEqual(result.returncode, 125, result.stdout)
        self.assertIn("time daemon process ID is unavailable", result.stderr)
        self.assertNotIn("new-init", result.stdout)

        self.helpers["nvx-init"].unlink()
        result = self._run("nvx_sandbox=1 nvx_snapshot_tier=platform")
        self.assertEqual(result.returncode, 125, result.stdout)
        self.assertIn("selected guest cannot act as sandbox control", result.stderr)
        result = self._run("nvx_sandbox=1 nvx_lifecycle=managed")
        self.assertEqual(result.returncode, 125, result.stdout)
        self.assertIn("selected guest cannot act as sandbox control", result.stderr)


class GuestInitWorkloadIdentityTests(unittest.TestCase):
    """The shell /init's workload account and identity checks, as a one-shot
    launch that asks for an account with nvx_workload_account=create runs
    them: on a user database of their own, with setpriv and nvx-exit stubs."""

    INIT = Path(__file__).parents[1] / "guest" / "common" / "init"
    PASSWD = (
        "root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/:/sbin/nologin\n"
    )
    GROUP = "root:x:0:\nnobody:x:65534:\n"

    def setUp(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        self.shell = shell
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        source = self.INIT.read_text(encoding="utf-8")
        self.functions = (
            "".join(
                _shell_function(source, name)
                for name in (
                    "workload_fatal",
                    "nonroot_id",
                    "resolve_workload_identity",
                    "ensure_workload_account",
                    "run_workload",
                    "read_workload_identity",
                )
            )
            .replace("/etc/passwd", '"$root/passwd"')
            .replace("/etc/group", '"$root/group"')
            .replace("/sbin/nvx-exit", "nvx_exit")
        )

    def _run(self, uid: str, gid: str) -> subprocess.CompletedProcess[str]:
        """Creates the account of the identity UID:GID and runs a workload as
        it, on a fresh copy of the user database."""
        (self.root / "passwd").write_bytes(self.PASSWD.encode())
        (self.root / "group").write_bytes(self.GROUP.encode())
        return subprocess.run(
            [self.shell, "-s", "--", self.root.as_posix(), uid, gid],
            input=(
                "root=$1\nworkload_uid=$2\nworkload_gid=$3\n"
                'nvx_exit() { echo "nvx-exit $1"; exit "$1"; }\n'
                'setpriv() { echo "setpriv $*"; }\n'
                f"{self.functions}"
                "ensure_workload_account\n"
                "run_workload /bin/busybox id\n"
            ),
            text=True,
            capture_output=True,
            check=False,
        )

    def test_account_creation_refuses_a_zero_id_however_it_is_spelled(self):
        # As numbers, zero-padded IDs are root's: the account would replace
        # root's record, and setpriv would run the workload as root.
        for uid, gid, message in (
            ("00", "65534", "workload UID is unavailable"),
            ("65534", "000", "workload GID is unavailable"),
        ):
            with self.subTest(uid=uid, gid=gid):
                result = self._run(uid, gid)
                self.assertEqual(result.returncode, 125, result.stdout)
                self.assertIn(f"NVX-WORKLOAD-ERROR: {message}\n", result.stderr)
                self.assertNotIn("setpriv", result.stdout)
                self.assertEqual(
                    (self.root / "passwd").read_bytes(), self.PASSWD.encode()
                )
                self.assertEqual(
                    (self.root / "group").read_bytes(), self.GROUP.encode()
                )

    def test_account_creation_runs_a_new_identity_as_itself(self):
        result = self._run("4242", "4242")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("setpriv --reuid 4242 --regid 4242 ", result.stdout)
        self.assertEqual(
            (self.root / "passwd").read_bytes().decode(),
            self.PASSWD + "nvx:x:4242:4242:NVX workload:/tmp:/sbin/nologin\n",
        )
        self.assertEqual(
            (self.root / "group").read_bytes().decode(), self.GROUP + "nvx:x:4242:\n"
        )

    def test_nvx_identity_needs_both_ids(self):
        # With neither, an nvx_exec workload would skip the privilege drop and
        # run as root.
        for identity, expected in (
            ("4242:4243", "read 4242:4243\n"),
            (":", None),
            ("4242:", None),
            (":4243", None),
            ("4242", None),
        ):
            with self.subTest(identity=identity):
                result = subprocess.run(
                    [self.shell, "-s", "--", identity],
                    input=(
                        'nvx_exit() { echo "nvx-exit $1"; exit "$1"; }\n'
                        f"{self.functions}"
                        'read_workload_identity "$1"\n'
                        'echo "read $workload_uid:$workload_gid"\n'
                    ),
                    text=True,
                    capture_output=True,
                    check=False,
                )
                if expected is None:
                    self.assertEqual(result.returncode, 125, result.stdout)
                    self.assertIn(
                        "NVX-WORKLOAD-ERROR: nvx_identity must be UID:GID\n",
                        result.stderr,
                    )
                    self.assertNotIn("read ", result.stdout)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, expected)


class InitPseudoMountTests(unittest.TestCase):
    def _run(
        self,
        filesystem: str,
        *,
        mount_status: int,
        existing_type: str,
        stat_status: int = 0,
        exit_returns: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        source = (Path(__file__).parents[1] / "guest/common/init").read_text(
            encoding="utf-8"
        )
        with tempfile.TemporaryDirectory() as temporary:
            port = (Path(temporary) / "port").as_posix()
            function = (
                _shell_function(source, "mount_pseudo")
                .replace("/dev/port", port)
                .replace("/bin/busybox mknod", "test_mknod")
                .replace("/sbin/nvx-exit", "test_exit")
            )
            script = (
                f"mount() {{ return {mount_status}; }}\n"
                f"stat() {{ printf '%s\\n' '{existing_type}'; "
                f"return {stat_status}; }}\n"
                # A mount point of the wrong type must not satisfy the check.
                "mountpoint() { return 0; }\n"
                "test_mknod() { printf 'CREATE-PORT %s\\n' \"$*\"; }\n"
                "test_exit() { printf 'EXIT %s\\n' \"$1\"; "
                + ("return 1;" if exit_returns else 'exit "$1";')
                + " }\n"
                "sleep() { exit 87; }\n"
                + function
                + f"\nmount_pseudo {filesystem} /{filesystem}\n"
                "echo CONTINUED\n"
            )
            return subprocess.run(
                [shell, "-c", script], capture_output=True, text=True, timeout=10
            )

    def test_successful_mount_continues_without_shutdown(self):
        result = self._run("proc", mount_status=0, existing_type="")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "CONTINUED\n")
        self.assertEqual(result.stderr, "")

    def test_correct_existing_filesystem_survives_a_failed_mount(self):
        for filesystem in ("proc", "sysfs"):
            with self.subTest(filesystem=filesystem):
                result = self._run(filesystem, mount_status=1, existing_type=filesystem)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "CONTINUED\n")

    def test_wrong_filesystem_stops_before_later_boot_steps(self):
        for filesystem in ("proc", "sysfs"):
            for existing in ("tmpfs", "rootfs", ""):
                with self.subTest(filesystem=filesystem, existing=existing):
                    result = self._run(
                        filesystem, mount_status=1, existing_type=existing
                    )
                    self.assertEqual(result.returncode, 1)
                    self.assertIn(
                        f"NVX-INIT-ERROR: cannot mount {filesystem}", result.stderr
                    )
                    self.assertIn("CREATE-PORT -m 600 ", result.stdout)
                    self.assertIn(" c 1 4\nEXIT 1\n", result.stdout)
                    self.assertNotIn("CONTINUED", result.stdout)

    def test_unreadable_filesystem_stops(self):
        result = self._run("proc", mount_status=1, existing_type="", stat_status=1)
        self.assertEqual(result.returncode, 1)
        self.assertNotIn("CONTINUED", result.stdout)

    def test_shutdown_failure_does_not_resume_boot(self):
        result = self._run("proc", mount_status=1, existing_type="", exit_returns=True)
        self.assertEqual(result.returncode, 87)
        self.assertIn("EXIT 1", result.stdout)
        self.assertNotIn("CONTINUED", result.stdout)


class InitIcmpEchoSocketsTests(unittest.TestCase):
    """Unprivileged workloads hold no capability, so the init opens ICMP echo
    sockets to every group for them, in every guest mode (#1250)."""

    INIT = Path(__file__).parents[1] / "guest/common/init"
    SETTING = "/proc/sys/net/ipv4/ping_group_range"

    def _run(self, *, setting: str | None, directory: bool = False):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        source = self.INIT.read_text(encoding="utf-8")
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "ping_group_range"
            if directory:
                path.mkdir()
            elif setting is not None:
                path.write_text(setting, encoding="utf-8")
            function = (
                _shell_function(source, "open_icmp_echo_sockets")
                .replace(self.SETTING, shlex.quote(path.as_posix()))
                .replace("/sbin/nvx-exit", "test_exit")
            )
            script = (
                'test_exit() { printf \'EXIT %s\\n\' "$1"; exit "$1"; }\n'
                "sleep() { exit 87; }\n"
                + function
                + "\nopen_icmp_echo_sockets\necho CONTINUED\n"
            )
            result = subprocess.run(
                [shell, "-c", script], capture_output=True, text=True, timeout=10
            )
            written = path.read_text(encoding="utf-8") if path.is_file() else None
        return result, written

    def test_the_kernel_default_opens_to_every_group(self):
        result, written = self._run(setting="1\t0\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "CONTINUED\n")
        self.assertEqual(result.stderr, "")
        # Linux rejects 4294967295, which no group has, and accepts the group
        # IDs before it.
        self.assertEqual(written, "0 4294967294\n")

    def test_a_kernel_without_the_setting_boots_on(self):
        result, written = self._run(setting=None)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "CONTINUED\n")
        self.assertIsNone(written)

    def test_a_setting_that_does_not_take_the_range_stops_the_boot(self):
        result, _ = self._run(setting=None, directory=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn(
            "NVX-INIT-ERROR: cannot open ICMP echo sockets to every group",
            result.stderr,
        )
        self.assertEqual(result.stdout, "EXIT 1\n")

    def test_every_guest_mode_gets_the_setting(self):
        source = self.INIT.read_text(encoding="utf-8")
        call = source.index("\nopen_icmp_echo_sockets\n")
        # The one-shot program, the sandbox's nvx-init, and the managed agent
        # all start after the call.
        for later in (
            'if [ -n "$nvx_exec" ] && [ "$sandbox_mode" = false ]; then',
            'exec /sbin/nvx-init "$@"',
            "exec /sbin/nvx-managed-agent",
        ):
            with self.subTest(later=later):
                self.assertLess(call, source.index(later))


if __name__ == "__main__":
    unittest.main()

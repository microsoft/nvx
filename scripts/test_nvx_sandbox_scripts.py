#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import hashlib
import os
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


def _aggregate_child_name(index: int, target: str) -> str:
    """Return the name that nvx gives the child of an aggregate share at
    `index` that the guest binds at `target`: the index, a hyphen, and the first
    32 hexadecimal digits of the SHA-256 digest of the target."""
    return f"{index}-{hashlib.sha256(target.encode()).hexdigest()[:32]}"


_SANDBOX_MOUNTINFO = (
    "20 1 0:20 / / rw,relatime - overlay overlay rw,"
    "lowerdir=/run/nvx/layers/distro,upperdir=/run/nvx/scratch/upper,"
    "workdir=/run/nvx/scratch/work,metacopy=on,xino=on\n"
    "21 20 0:21 / /.nvx-agent ro,nosuid,nodev,relatime - tmpfs tmpfs ro,mode=755\n"
    "22 20 0:22 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw\n"
    "23 20 0:23 / /sys ro,nosuid,nodev,noexec,relatime - sysfs sysfs ro\n"
    "24 20 0:24 / /dev rw,nosuid,relatime - tmpfs tmpfs rw,mode=755\n"
    "25 24 0:25 / /dev/pts rw,relatime - devpts devpts rw,mode=620,ptmxmode=666\n"
    "26 24 0:26 / /dev/shm rw,nosuid,nodev,relatime - tmpfs tmpfs rw\n"
)
# The kernel command line of a sandbox whose upper is on scratch, which the
# smoke check reads from /proc/cmdline.
_SCRATCH_COMMAND_LINE = (
    "console=hvc0 nvx_sandbox=1 "
    "nvx_layer=distro,0xd0003000,erofs:0123abcd-4567-89ef-0123-456789abcdef "
    "nvx_scratch=0xd0006000,ext4"
)
_SANDBOX_STATUS = (
    "Name:\tsh\nUid:\t65534\t65534\t65534\t65534\n"
    "Gid:\t65534\t65534\t65534\t65534\nGroups:\t\nNoNewPrivs:\t1\n"
    + "".join(
        f"{name}:\t0000000000000000\n"
        for name in ("CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb")
    )
)


class HostmountShareTests(unittest.TestCase):
    HOSTMOUNT = Path(__file__).parents[1] / "guest" / "common" / "nvx-hostmount"

    def setUp(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        self.shell = shell

    def test_hostmount_accepts_only_child_names_inside_the_aggregate(self):
        hostmount = self.HOSTMOUNT.read_text(encoding="utf-8")
        script = (
            'fail() { echo "FAIL: $*" >&2; exit 1; }\n'
            f"{_shell_function(hostmount, 'parse_share')}"
            'parse_share "$1"\n'
            'printf "%s|%s|%s|%s\\n" "$share_child" "$share_target" "$share_mode" '
            '"$share_kind"\n'
        )

        def parse(share: str) -> subprocess.CompletedProcess[str]:
            return subprocess.run(
                [self.shell, "-s", "--", share],
                input=script,
                text=True,
                capture_output=True,
                check=False,
            )

        name = _aggregate_child_name(0, "/workspace")
        for share, expected in (
            (f"{name},/workspace,rw", f"{name}|/workspace|rw|directory\n"),
            ("0,/opt/tools,ro", "0|/opt/tools|ro|directory\n"),
            ("a.b_C-9,/x,ro", "a.b_C-9|/x|ro|directory\n"),
            # A file child is bound at a file.
            ("1,/config/settings.json,ro,file", "1|/config/settings.json|ro|file\n"),
            ("1,/results/output.txt,rw,file", "1|/results/output.txt|rw|file\n"),
        ):
            with self.subTest(share=share):
                result = parse(share)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, expected)
        for share in (
            ",/workspace,rw",
            ".,/workspace,rw",
            "..,/workspace,rw",
            "a/b,/workspace,rw",
            "a:b,/workspace,rw",
            "a*,/workspace,rw",
            "0,/workspace,file",
            "0,/workspace,rw,dir",
        ):
            with self.subTest(share=share):
                result = parse(share)
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn("invalid live share", result.stderr)

    def test_hostmount_makes_file_mount_points_without_following_links(self):
        hostmount = self.HOSTMOUNT.read_text(encoding="utf-8")
        script = (
            f"{_shell_function(hostmount, 'make_file_mount_point')}"
            'make_file_mount_point "$1"\n'
        )

        def make(target: Path) -> int:
            return subprocess.run(
                [self.shell, "-s", "--", target.as_posix()],
                input=script,
                text=True,
                capture_output=True,
                check=False,
            ).returncode

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            # A missing file is created with its directories, and an existing
            # one is kept as it is.
            created = root / "config" / "settings.json"
            self.assertEqual(make(created), 0)
            self.assertEqual(created.read_bytes(), b"")
            existing = root / "existing.json"
            existing.write_bytes(b"{}")
            self.assertEqual(make(existing), 0)
            self.assertEqual(existing.read_bytes(), b"{}")
            (root / "directory").mkdir()
            self.assertEqual(make(root / "directory"), 1)
            if os.name == "nt":
                return
            # A link is refused before anything is created through it, whether
            # or not its target exists.
            dangling = root / "dangling.json"
            dangling.symlink_to(root / "elsewhere.json")
            self.assertEqual(make(dangling), 1)
            self.assertFalse((root / "elsewhere.json").exists())
            linked = root / "linked.json"
            linked.symlink_to(existing)
            self.assertEqual(make(linked), 1)
            self.assertEqual(existing.read_bytes(), b"{}")


@unittest.skipIf(os.name == "nt", "requires POSIX symbolic links")
class SandboxSmokeShareTests(unittest.TestCase):
    SMOKE = Path(__file__).parents[1] / "guest" / "common" / "nvx-sandbox-smoke"

    def setUp(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        self.shell = shell
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.share = self.root / "share"
        self.links = self.share / "nvx-links"
        self.links.mkdir(parents=True)
        (self.share / "nvx-host-marker").write_text("host-to-guest\n", encoding="utf-8")
        # The guest resolves nvx-links/outside to this path through ../..
        self.outside = self.root / "nvx-outside-marker"
        source = self.SMOKE.read_text(encoding="utf-8")
        self.functions = "".join(
            _shell_function(source, name)
            for name in (
                "check_share",
                "check_share_ownership",
                "check_share_denied",
                "expect_eperm",
                "check_share_policy",
                "expect_erofs",
                "check_share_symlinks",
                "check_shares_isolated",
            )
        ).replace("/tmp/nvx-tool", '"$scratch/nvx-tool"')

    def _run(self, call: str) -> subprocess.CompletedProcess[str]:
        scratch = self.root / "scratch"
        scratch.mkdir(exist_ok=True)
        return subprocess.run(
            [self.shell, "-s", "--", self.share.as_posix(), scratch.as_posix()],
            input=f"set -eu\nshare=$1\nscratch=$2\n{self.functions}{call}\n",
            text=True,
            capture_output=True,
            check=False,
        )

    def test_rw_share_creates_links_with_exact_targets(self):
        result = self._run('check_share "$share" rw')

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            f"NVX-UBUNTU-SANDBOX-SYMLINK-OK target={self.share.as_posix()}",
            result.stdout,
        )
        self.assertIn("NVX-UBUNTU-SANDBOX-SHARE-OK", result.stdout)
        self.assertEqual(
            {name: os.readlink(self.links / name) for name in os.listdir(self.links)},
            {
                "nvx-tool": "../nvx-tool",
                "absolute": "/nvx/absolute/target",
                "outside": "../../nvx-outside-marker",
                "denied": "../nvx-denied/secret",
                "denied-directory": "../nvx-denied",
            },
        )
        self.assertEqual(
            (self.share / "nvx-guest-marker").read_text(encoding="utf-8"),
            "guest-to-host\n",
        )
        self.assertTrue((self.share / "nvx-guest-directory").is_dir())

    def test_links_fail_when_they_expose_host_data(self):
        (self.share / "nvx-guest-marker").write_text(
            "guest-to-host\n", encoding="utf-8"
        )
        self.outside.write_text("host-outside\n", encoding="utf-8")
        result = self._run('check_share_symlinks "$share"')
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("NVX-UBUNTU-SANDBOX-SYMLINK-OK", result.stdout)

        for path in self.links.iterdir():
            path.unlink()
        self.outside.unlink()
        denied = self.share / "nvx-denied"
        denied.mkdir()
        (denied / "secret").write_text("secret\n", encoding="utf-8")
        result = self._run('check_share_symlinks "$share"')
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("NVX-UBUNTU-SANDBOX-SYMLINK-OK", result.stdout)

    def test_ro_share_rejects_links(self):
        geteuid = getattr(os, "geteuid", None)
        if geteuid is not None and geteuid() == 0:
            self.skipTest("root ignores directory write permissions")
        self.share.chmod(0o555)
        self.addCleanup(self.share.chmod, 0o755)

        result = self._run('check_share "$share" ro')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("NVX-UBUNTU-SANDBOX-SHARE-OK", result.stdout)
        self.assertFalse(os.path.lexists(self.share / "nvx-guest-link"))

        self.share.chmod(0o755)
        result = self._run('check_share "$share" ro')
        self.assertEqual(result.returncode, 1, result.stdout)

    def test_policy_share_requires_openvmm_to_refuse_writes(self):
        (self.share / "nvx-writable").mkdir()
        allowed = self.share / "nvx-denied" / "nvx-allowed"
        allowed.mkdir(parents=True)
        (allowed / "payload").write_text("payload\n", encoding="utf-8")

        # Only OpenVMM's access policy can make the share refuse writes
        # outside nvx-writable, so a plain directory fails the checks.
        result = self._run('check_share "$share" policy')
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertNotIn("NVX-UBUNTU-SANDBOX-POLICY-OK", result.stdout)

        # A refusal other than EROFS, such as a permission check, fails too.
        geteuid = getattr(os, "geteuid", None)
        if geteuid is not None and geteuid() == 0:
            return
        (self.share / "nvx-guest-marker").unlink(missing_ok=True)
        self.share.chmod(0o555)
        self.addCleanup(self.share.chmod, 0o755)
        result = self._run('check_share "$share" policy')
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("unexpected share error", result.stderr)
        self.assertIn("Permission denied", result.stderr)

    def test_rw_share_link_cannot_write_into_ro_share(self):
        geteuid = getattr(os, "geteuid", None)
        if geteuid is not None and geteuid() == 0:
            self.skipTest("root ignores file write permissions")
        toolcache = self.root / "toolcache"
        toolcache.mkdir()
        marker = toolcache / "nvx-host-marker"
        marker.write_text("host-to-guest\n", encoding="utf-8")
        marker.chmod(0o444)
        self.addCleanup(marker.chmod, 0o644)

        call = f'check_shares_isolated "$share" "{toolcache.as_posix()}"'
        result = self._run(call)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            f"NVX-UBUNTU-SANDBOX-SHARES-ISOLATED-OK rw={self.share.as_posix()} "
            f"ro={toolcache.as_posix()}",
            result.stdout,
        )
        self.assertFalse(os.path.lexists(self.share / "nvx-cross-share-link"))
        self.assertEqual(marker.read_text(encoding="utf-8"), "host-to-guest\n")

        # A write that reaches the read-only share fails the check.
        marker.chmod(0o644)
        result = self._run(call)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertNotIn("NVX-UBUNTU-SANDBOX-SHARES-ISOLATED-OK", result.stdout)

    def test_caller_share_owns_and_populates_its_own_directories(self):
        if sys.platform != "linux":
            self.skipTest("ownership checks use GNU stat and Linux identities")
        result = self._run('check_share "$share" caller')

        self.assertEqual(result.returncode, 0, result.stderr)
        share = self.share.as_posix()
        self.assertIn(
            f"NVX-UBUNTU-SANDBOX-OWNER-OK target={share} "
            f"identity={os.getuid()}:{os.getgid()}",
            result.stdout,
        )
        self.assertIn(f"NVX-UBUNTU-SANDBOX-SYMLINK-OK target={share}", result.stdout)
        self.assertIn(
            f"NVX-UBUNTU-SANDBOX-SHARE-OK target={share} mode=caller", result.stdout
        )
        links = self.share / "nvx-caller-links"
        self.assertEqual(
            {name: os.readlink(links / name) for name in os.listdir(links)},
            {
                "nvx-tool": "../nvx-tool",
                "absolute": "/nvx/absolute/target",
                "outside": "../../nvx-outside-marker",
                "denied": "../nvx-denied/secret",
                "denied-directory": "../nvx-denied",
            },
        )
        self.assertEqual(os.listdir(self.links), [])
        renamed = self.share / "nvx-guest-directory" / "nested" / "renamed"
        self.assertEqual(renamed.read_text(encoding="utf-8"), "nested\n")

    def test_eperm_share_requires_reads_to_fail_with_eperm_and_writes_to_fail(self):
        result = self._run('check_share "$share" eperm')
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("NVX-UBUNTU-SANDBOX-SHARE-OK", result.stdout)

        geteuid = getattr(os, "geteuid", None)
        if geteuid is not None and geteuid() == 0:
            self.skipTest("root ignores directory write permissions")
        # Writes need only fail, so a read-only host directory stands in for
        # the host's refusal.
        self.share.chmod(0o555)
        self.addCleanup(self.share.chmod, 0o755)
        denied = "".join(
            f'{command}() {{ echo "{command}: $1: Operation not permitted" >&2; '
            "return 1; }\n"
            for command in ("cat", "ls")
        )
        result = self._run(denied + 'check_share "$share" eperm')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            f"NVX-UBUNTU-SANDBOX-SHARE-OK target={self.share.as_posix()} mode=eperm",
            result.stdout,
        )
        self.assertFalse((self.share / "nvx-guest-marker").exists())

        other_error = denied.replace("Operation not permitted", "Permission denied")
        result = self._run(other_error + 'check_share "$share" eperm')
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("unexpected share error", result.stderr)

        self.share.chmod(0o755)
        result = self._run(denied + 'check_share "$share" eperm')
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("NVX-UBUNTU-SANDBOX-SHARE-OK", result.stdout)


@unittest.skipIf(os.name == "nt", "requires a POSIX shell and process tools")
class SandboxSmokeProfileTests(unittest.TestCase):
    SMOKE = Path(__file__).parents[1] / "guest" / "common" / "nvx-sandbox-smoke"

    def setUp(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        self.shell = shell
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.source = self.SMOKE.read_text(encoding="utf-8")

    def _fixture(self, name: str, content: str) -> str:
        path = self.root / name
        path.write_text(content, encoding="utf-8")
        return path.as_posix()

    def _run(
        self,
        names: tuple[str, ...],
        call: str,
        replacements: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        # The globals before fail() configure the checks.
        script = self.source[: self.source.index("\nfail() {\n") + 1] + "".join(
            _shell_function(self.source, name) for name in ("fail", *names)
        )
        for old, new in (replacements or {}).items():
            script = script.replace(old, new)
        return subprocess.run(
            [self.shell, "-s"],
            input=f"{script}{call}\n",
            text=True,
            capture_output=True,
            check=False,
        )

    def test_status_fields_join_their_values(self):
        status = self._fixture("status", _SANDBOX_STATUS)
        result = self._run(
            ("status_field",),
            f"status_field {status} Uid\nstatus_field {status} Groups\n"
            f"status_field {status} Missing",
        )

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "65534 65534 65534 65534\n\n")
        self.assertIn(f"{status} has no Missing field", result.stderr)

    def test_identity_and_capability_checks_read_the_process_status(self):
        identity = (
            'id() { case "$1" in -u | -g) echo 65534 ;; esac; }\nHOME=/nonexistent\n'
        )
        mutations = {
            "valid": ("", ""),
            "saved-uid": (
                "Uid:\t65534\t65534\t65534\t65534",
                "Uid:\t65534\t65534\t0\t65534",
            ),
            "groups": ("Groups:\t", "Groups:\t27 "),
            "bounding": (
                "CapBnd:\t0000000000000000",
                "CapBnd:\t000001ffffffffff",
            ),
            "no-new-privs": ("NoNewPrivs:\t1", "NoNewPrivs:\t0"),
        }
        for mutation, (old, new) in mutations.items():
            with self.subTest(mutation=mutation):
                status = self._fixture("status", _SANDBOX_STATUS.replace(old, new))
                result = self._run(
                    ("status_field", "check_identity", "check_capabilities"),
                    f"{identity}check_identity\ncheck_capabilities\necho checked",
                    {"/proc/self/status": status},
                )
                if mutation == "valid":
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, "checked\n")
                else:
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn("NVX-UBUNTU-SANDBOX-FAIL", result.stderr)

    def test_namespace_check_rejects_the_initial_namespaces(self):
        def run(
            links: dict[str, str],
            *,
            init_status: str = _SANDBOX_STATUS,
            command_line: str = "console=hvc0 nvx_hostname=nvx-lifecycle",
            hostname: str = "nvx-lifecycle",
        ) -> subprocess.CompletedProcess[str]:
            readlink = (
                'readlink() { case "$1" in '
                + " ".join(
                    f"*/{name}) echo '{link}' ;;" for name, link in links.items()
                )
                + " esac; }\n"
            )
            return self._run(
                ("status_field", "check_hostname", "check_namespaces"),
                f"{readlink}check_namespaces\necho checked",
                {
                    "/proc/1/status": self._fixture("init-status", init_status),
                    "/proc/cmdline": self._fixture("cmdline", command_line + "\n"),
                    "/proc/sys/kernel/hostname": self._fixture(
                        "hostname", hostname + "\n"
                    ),
                },
            )

        private = {
            "mnt": "mnt:[4026532200]",
            "pid": "pid:[4026532201]",
            "uts": "uts:[4026532202]",
        }
        result = run(private)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "checked\n")
        result = run(private, command_line="console=hvc0", hostname="nvx-sandbox")
        self.assertEqual(result.returncode, 0, result.stderr)
        # A caller's --cmdline keeps its newlines, so a token may follow one.
        multiline = "console=hvc0 quiet\nloglevel=0\nnvx_hostname=nvx-lifecycle"
        result = run(private, command_line=multiline)
        self.assertEqual(result.returncode, 0, result.stderr)
        result = run(private, command_line=multiline, hostname="nvx-sandbox")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("expected nvx-lifecycle", result.stderr)

        for name, initial in (
            ("mnt", "mnt:[4026531832]"),
            ("pid", "pid:[4026531836]"),
            ("uts", "uts:[4026531838]"),
        ):
            with self.subTest(namespace=name):
                result = run({**private, name: initial})
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn(f"shares the initial {name} namespace", result.stderr)
        for link in ("pid:[]", "pid:[42a]", "mnt:[42]", "pid:42"):
            with self.subTest(link=link):
                result = run({**private, "pid": link})
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn("unexpected pid namespace link", result.stderr)
        result = run(
            private,
            init_status=_SANDBOX_STATUS.replace(
                "Uid:\t65534\t65534\t65534\t65534", "Uid:\t0\t0\t0\t0"
            ),
        )
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("PID 1 of the workload PID namespace runs as 0", result.stderr)
        result = run(private, hostname="nvx-sandbox")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("expected nvx-lifecycle", result.stderr)

    def test_init_hostname_check_reads_the_encoded_command_line(self):
        def run(command_line: str, hostname: str) -> subprocess.CompletedProcess[str]:
            return self._run(
                ("check_hostname",),
                "nvx_init=true\ncheck_hostname\necho checked",
                {
                    "/proc/cmdline": self._fixture("cmdline", command_line + "\n"),
                    "/proc/sys/kernel/hostname": self._fixture(
                        "hostname", hostname + "\n"
                    ),
                },
            )

        # nvx-lifecycle, as nvx-init's command line encodes it.
        encoded = "nvx_hostname=6e76782d6c6966656379636c65"
        result = run(
            f"console=hvc0 {encoded} nvx_machine_id=generated", "nvx-lifecycle"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "checked\n")
        result = run(f"console=hvc0 {encoded}", "nvx-sandbox")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("expected 6e76782d6c6966656379636c65", result.stderr)

    def _check_mounts(
        self, mountinfo: str, command_line: str
    ) -> subprocess.CompletedProcess[str]:
        return self._run(
            ("mount_records", "require_mount", "check_mounts"),
            "check_mounts\necho checked",
            {
                "/proc/self/mountinfo": self._fixture("mountinfo", mountinfo),
                "/proc/cmdline": self._fixture("cmdline", command_line + "\n"),
            },
        )

    def test_mount_check_requires_private_runtime_mounts_once(self):
        result = self._check_mounts(_SANDBOX_MOUNTINFO, _SCRATCH_COMMAND_LINE)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "checked\n")

        proc = "22 20 0:22 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw\n"
        mutations = {
            "stacked": (
                proc,
                proc + proc.replace("22 20 0:22", "27 22 0:27"),
                "several mounts are stacked at /proc",
            ),
            "propagation": (
                proc,
                proc.replace("relatime - proc", "relatime shared:5 - proc"),
                "the mount at /proc propagates: shared:5",
            ),
            "writable-sysfs": (
                "/sys ro,nosuid",
                "/sys rw,nosuid",
                "/sys is mounted rw,nosuid,nodev,noexec,relatime, without ro",
            ),
            "upper": (
                "upperdir=/run/nvx/scratch/upper",
                "upperdir=/run/nvx/upper",
                "upper layer is not on scratch",
            ),
            "lower": (
                "lowerdir=/run/nvx/layers/distro",
                "lowerdir=/srv/distro",
                "does not use the image layers",
            ),
            "metacopy": (
                ",metacopy=on,",
                ",",
                "the root overlay is mounted without metacopy=on",
            ),
            "xino": (
                ",xino=on\n",
                "\n",
                "the root overlay is mounted without xino=on",
            ),
            "missing-shm": (
                "26 24 0:26 / /dev/shm rw,nosuid,nodev,relatime - tmpfs tmpfs rw\n",
                "",
                "nothing is mounted at /dev/shm",
            ),
            "host-devices": (
                "- tmpfs tmpfs rw,mode=755\n",
                "- devtmpfs devtmpfs rw,mode=755\n",
                "/dev is devtmpfs, expected tmpfs",
            ),
        }
        for mutation, (old, new, message) in mutations.items():
            with self.subTest(mutation=mutation):
                self.assertIn(old, _SANDBOX_MOUNTINFO)
                result = self._check_mounts(
                    _SANDBOX_MOUNTINFO.replace(old, new, 1), _SCRATCH_COMMAND_LINE
                )
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn(message, result.stderr)

    def test_mount_check_requires_the_upper_that_the_command_line_declares(self):
        tmpfs = _SANDBOX_MOUNTINFO.replace("/run/nvx/scratch/", "/run/nvx/tmpfs/")
        ram = _SCRATCH_COMMAND_LINE.replace(
            "nvx_scratch=0xd0006000,ext4", "nvx_overlay_upper=ramfs"
        )
        # A caller's --cmdline keeps its newlines, and the root's tokens follow
        # it.
        multiline = ram.replace("console=hvc0 ", "console=hvc0 quiet\nloglevel=0\n")
        for mountinfo, command_line, message in (
            (tmpfs, ram, None),
            (tmpfs, multiline, None),
            (_SANDBOX_MOUNTINFO, ram, "upper layer is not on tmpfs"),
            (_SANDBOX_MOUNTINFO, multiline, "upper layer is not on tmpfs"),
            (tmpfs, _SCRATCH_COMMAND_LINE, "upper layer is not on scratch"),
            # Only the RAM upper's token declares it.
            (
                tmpfs,
                _SCRATCH_COMMAND_LINE + " xnvx_overlay_upper=ramfs",
                "upper layer is not on scratch",
            ),
            (
                tmpfs,
                _SCRATCH_COMMAND_LINE + " nvx_overlay_upper=tmpfs",
                "upper layer is not on scratch",
            ),
            (
                tmpfs.replace(",metacopy=on,", ","),
                ram,
                "the root overlay is mounted without metacopy=on",
            ),
        ):
            with self.subTest(mountinfo=mountinfo, command_line=command_line):
                result = self._check_mounts(mountinfo, command_line)
                if message is None:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, "checked\n")
                else:
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn(message, result.stderr)

    def test_cgroup_check_requires_the_container_cgroup(self):
        for content, returncode in (
            ("0::/container\n", 0),
            ("0::/agent\n", 1),
            ("12:pids:/container\n", 1),
        ):
            with self.subTest(content=content):
                cgroup = self._fixture("cgroup", content)
                result = self._run(
                    ("check_cgroup",), "check_cgroup", {"/proc/self/cgroup": cgroup}
                )
                self.assertEqual(result.returncode, returncode, result.stderr)

    def test_device_creation_check_requires_the_refusal(self):
        device = self.root / "device"
        for mknod, message in (
            ("mknod() { return 1; }\n", None),
            ('mknod() { : >"$1"; }\n', "the workload can create device nodes"),
            ("PATH=/nonexistent\n", "mknod is unavailable"),
        ):
            with self.subTest(mknod=mknod):
                result = self._run(
                    ("check_device_creation",),
                    f"{mknod}check_device_creation\necho checked",
                    {"/tmp/nvx-sandbox-device": device.as_posix()},
                )
                if message is None:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, "checked\n")
                else:
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn(message, result.stderr)
                self.assertFalse(device.exists())

    def test_machine_id_check_requires_a_read_only_generated_id(self):
        machine_id = "0123456789abcdef0123456789abcdef"
        mount = (
            "30 20 0:24 /nvx/workload-machine-id /etc/machine-id "
            "ro,nosuid,nodev,noexec,relatime - tmpfs tmpfs rw,mode=755\n"
        )
        read_only = (self.root / "missing" / "machine-id").as_posix()
        writable = self._fixture("writable-machine-id", "")
        for case, content, mounts, target, message in (
            ("valid", machine_id, mount, read_only, None),
            ("upper", machine_id.upper(), mount, read_only, "machine ID is 0123"),
            ("short", machine_id[1:], mount, read_only, "machine ID is 1234"),
            ("other", f"{machine_id[1:]}g", mount, read_only, "machine ID is 1234"),
            ("empty", "", mount, read_only, "machine ID is \n"),
            ("unbound", machine_id, "", read_only, "nothing is mounted"),
            (
                "writable-mount",
                machine_id,
                mount.replace("ro,nosuid", "rw,nosuid"),
                read_only,
                "without ro",
            ),
            (
                "device-mount",
                machine_id,
                mount.replace("nosuid,nodev", "nosuid"),
                read_only,
                "without nodev",
            ),
            (
                "image-file",
                machine_id,
                mount.replace("- tmpfs tmpfs", "- overlay overlay"),
                read_only,
                "/etc/machine-id is overlay, expected tmpfs",
            ),
            ("replaceable", machine_id, mount, writable, "can replace its machine ID"),
        ):
            with self.subTest(case=case):
                result = self._run(
                    ("mount_records", "require_mount", "check_machine_id"),
                    "check_machine_id\necho checked",
                    {
                        "</etc/machine-id": "<"
                        + self._fixture("machine-id", f"{content}\n"),
                        ">/etc/machine-id": f">{target}",
                        "/proc/self/mountinfo": self._fixture(
                            "mountinfo", _SANDBOX_MOUNTINFO + mounts
                        ),
                    },
                )
                if message is None:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, "checked\n")
                else:
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn(message, result.stderr)

    def test_port_check_requires_the_fault_and_the_refusal(self):
        for read, grant, message in (
            (139, 1, None),
            (0, 1, "a port read without the permission ended with status 0"),
            (1, 1, "a port read without the permission ended with status 1"),
            (139, 0, "a request for the port permission ended with status 0"),
            (139, 139, "a request for the port permission ended with status 139"),
        ):
            with self.subTest(read=read, grant=grant):
                result = self._run(
                    ("check_ports",),
                    'nvx_time() { [ "$*" = "test port-probe snapshot" ] && '
                    f"return {read}; "
                    '[ "$*" = "test port-probe snapshot grant" ] && '
                    f"return {grant}; exit 2; }}\n"
                    "check_ports\necho checked",
                    {"/sbin/nvx-time": "nvx_time"},
                )
                if message is None:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, "checked\n")
                else:
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn(message, result.stderr)

    def test_entry_check_requires_nodev_beyond_the_devices(self):
        nodev = "".join(
            line
            if " /dev " in line or " /dev/pts " in line
            else line.replace(" rw,relatime -", " rw,nodev,relatime -")
            for line in _SANDBOX_MOUNTINFO.splitlines(keepends=True)
        )
        for case, mountinfo, message in (
            ("valid", nodev, None),
            ("root", _SANDBOX_MOUNTINFO, "/ is mounted rw,relatime, without nodev"),
            (
                "share",
                nodev + "30 20 0:30 / /workspace rw,nosuid - virtiofs share rw\n",
                "/workspace is mounted rw,nosuid, without nodev",
            ),
        ):
            with self.subTest(case=case):
                result = self._run(
                    ("check_entry",),
                    "check_entry",
                    {"/proc/self/mountinfo": self._fixture("mountinfo", mountinfo)},
                )
                if message is None:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, "NVX-UBUNTU-SANDBOX-ENTRY-OK\n")
                else:
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn(message, result.stderr)
                    self.assertNotIn("ENTRY-OK", result.stdout)

    def test_limits_check_validates_its_arguments(self):
        for arguments, message in (
            ("abc 8", "limits requires numeric MEMORY_MAX and PIDS_MAX"),
            ("33554432 ''", "limits requires numeric MEMORY_MAX and PIDS_MAX"),
            ("33554432 2", "requires PIDS_MAX of at least 3"),
        ):
            with self.subTest(arguments=arguments):
                result = self._run(
                    ("start_sleepers", "check_limits"), f"check_limits {arguments}"
                )
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn(message, result.stderr)

    @unittest.skipUnless(sys.platform == "linux", "inspects the Linux process table")
    def test_sleeper_subshell_records_and_reaps_its_sleepers(self):
        record = self.root / "sleepers"
        result = self._run(
            ("start_sleepers",),
            f'(start_sleepers "{record.as_posix()}" 3)\necho done',
            # A distinct duration identifies this test's sleepers.
            {"sleep 600": "sleep 611.25"},
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(record.read_text(encoding="utf-8"), "3\n")
        for process in Path("/proc").iterdir():
            if process.name.isdigit():
                try:
                    command = (process / "cmdline").read_bytes()
                except OSError:
                    continue
                self.assertNotEqual(command, b"sleep\x00611.25\x00")

    def test_main_body_runs_the_profile_then_limits_and_shares(self):
        body = self.source[self.source.index("\ncheck_identity\n") + 1 :]
        stubs = "".join(
            f'{name}() {{ echo "{name} $*"; }}\n'
            for name in (
                "check_identity",
                "check_capabilities",
                "check_namespaces",
                "check_hostname",
                "check_mounts",
                "check_devices",
                "check_device_creation",
                "check_machine_id",
                "check_ports",
                "check_cgroup",
                "check_image",
                "check_entry",
                "check_limits",
                "check_share",
                "check_shares_isolated",
            )
        )
        script = (
            self.source[: self.source.index("\nfail() {\n") + 1]
            + _shell_function(self.source, "fail")
            + stubs
            + body
        )

        def run(*arguments: str) -> subprocess.CompletedProcess[str]:
            return subprocess.run(
                [self.shell, "-s", "--", *arguments],
                input=script,
                text=True,
                capture_output=True,
                check=False,
            )

        result = run("entry", "limits", "33554432", "8", "/workspace", "rw", "/tools")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            [
                "check_identity ",
                "check_capabilities ",
                "check_namespaces ",
                "check_mounts ",
                "check_devices ",
                "check_device_creation ",
                "check_machine_id ",
                "check_ports ",
                "check_cgroup ",
                "check_image ",
                "NVX-UBUNTU-SANDBOX-PROFILE-OK uid=65534 gid=65534",
                "check_entry ",
                "check_limits 33554432 8",
                "check_share /workspace rw",
                "check_share /tools ro",
                "check_shares_isolated /workspace /tools",
                "NVX-UBUNTU-SANDBOX-OK uid=65534 gid=65534",
            ],
        )
        result = run("/workspace")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("check_entry ", result.stdout.splitlines())
        # An execution of nvx-init shares its namespaces and cgroup, so the
        # profile checks only its host name of them.
        result = run("init", "entry", "/workspace", "rw")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            [
                "check_identity ",
                "check_capabilities ",
                "check_hostname ",
                "check_mounts ",
                "check_devices ",
                "check_device_creation ",
                "check_machine_id ",
                "check_ports ",
                "check_image ",
                "NVX-UBUNTU-SANDBOX-PROFILE-OK uid=65534 gid=65534",
                "check_entry ",
                "check_share /workspace rw",
                "NVX-UBUNTU-SANDBOX-OK uid=65534 gid=65534",
            ],
        )
        result = run("limits", "33554432")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("limits requires MEMORY_MAX and PIDS_MAX", result.stderr)
        self.assertNotIn("NVX-UBUNTU-SANDBOX-OK", result.stdout)


if __name__ == "__main__":
    unittest.main()

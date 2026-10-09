#!/usr/bin/env python3
"""Compile the existing guest decoder on Linux and test real wire inputs."""

import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path


@unittest.skipUnless(sys.platform.startswith("linux"), "guest decoder requires Linux")
class ManagedAgentDecoderTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        compiler = shutil.which("cc")
        if compiler is None:
            raise unittest.SkipTest("guest decoder requires the guest build C compiler")
        cls.temporary = tempfile.TemporaryDirectory(prefix="nvx-agent-decoder-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.executable = cls.root / "decoder"
        cls.fault_executable = cls.root / "fault-injection"
        harness = cls.root / "decoder.c"
        harness.write_text(
            """
#define main nvx_agent_main
#include "nvx-managed-agent.c"
#undef main
int main(int argc, char **argv) {
    unsigned char bytes[65536];
    uint32_t timeout_ms = 0;
    char **arguments = NULL;
    struct exec_config config;
    memset(&config, 0xa5, sizeof(config));
    if (argc > 1 && strcmp(argv[1], "--exec-config-fd") == 0)
        return launch_workload(argc, argv);
    int launch = argc == 3 && strcmp(argv[1], "--launch") == 0;
    if (argc != 2 && !launch) return 2;
    FILE *file = fopen(argv[launch ? 2 : 1], "rb");
    if (file == NULL) return 2;
    size_t length = fread(bytes, 1, sizeof(bytes), file);
    if (ferror(file)) return 2;
    fclose(file);
    int result = decode_exec_payload(
        bytes, (uint32_t)length, &timeout_ms, &arguments, &config);
    if (result == 0 && launch) {
        int fd = create_exec_config_fd(&config, 0);
        if (fd < 0) return 2;
        int seals = F_SEAL_WRITE | F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL;
        if ((fcntl(fd, F_GET_SEALS) & seals) != seals) return 2;
        char fd_text[32];
        snprintf(fd_text, sizeof(fd_text), "%d", fd);
        char *helper_arguments[MAX_ARGUMENTS + 5] = {
            argv[0], "--exec-config-fd", fd_text, "--"
        };
        int count = 0;
        while (arguments[count] != NULL) {
            helper_arguments[count + 4] = arguments[count];
            count++;
        }
        helper_arguments[count + 4] = NULL;
        return launch_workload(count + 4, helper_arguments);
    }
    if (result == 0) {
        free_arguments(arguments);
        free_exec_config(&config);
    }
    return result == 0 ? 0 : 1;
}
""",
            encoding="utf-8",
        )
        guest = Path(__file__).resolve().parent.parent / "guest" / "common"
        subprocess.run(
            [
                compiler,
                "-D_GNU_SOURCE",
                "-std=c11",
                "-I",
                str(guest),
                str(harness),
                "-o",
                str(cls.executable),
            ],
            check=True,
            capture_output=True,
            timeout=30,
        )
        fault_harness = cls.root / "fault-injection.c"
        fault_cgroup_root = cls.root / "cgroup"
        fault_exec_cgroup = fault_cgroup_root / "nvx-exec"
        fault_exec_cgroup.mkdir(parents=True)
        (fault_exec_cgroup / "cgroup.procs").write_text("", encoding="utf-8")
        fault_harness.write_text(
            """
#define main nvx_agent_main
#include "nvx-managed-agent.c"
#undef main

static int barrier_reader = -1;

ssize_t __real_write(int, const void *, size_t);
ssize_t __wrap_write(int fd, const void *buffer, size_t length) {
    if (barrier_reader >= 0 && length == 6 &&
        memcmp(buffer, "start\\n", length) == 0) {
        close(barrier_reader);
        barrier_reader = -1;
    }
    return __real_write(fd, buffer, length);
}

int __real_setenv(const char *, const char *, int);
int __wrap_setenv(const char *name, const char *value, int overwrite) {
    const char *failure = getenv("NVX_TEST_FAIL_SETENV");
    if (failure != NULL && strcmp(name, failure) == 0) {
        errno = ENOMEM;
        return -1;
    }
    return __real_setenv(name, value, overwrite);
}

int __wrap_execv(const char *path, char *const argv[]) {
    const char *marker = getenv("NVX_TEST_EXEC_MARKER");
    (void)path;
    (void)argv;
    if (marker != NULL) {
        int fd = open(marker, O_WRONLY | O_CREAT | O_TRUNC, 0600);
        if (fd >= 0) close(fd);
    }
    errno = ENOENT;
    return -1;
}

int __wrap_execvp(const char *file, char *const argv[]) {
    return __wrap_execv(file, argv);
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "--release-closing-barrier") == 0) {
        barrier_reader = open(argv[2], O_RDONLY | O_CLOEXEC | O_NONBLOCK);
        if (barrier_reader < 0) return 2;
        errno = 0;
        int result = release_container_barrier(argv[2], -1);
        int status = errno;
        if (barrier_reader >= 0) close(barrier_reader);
        return result == -1 && status == EPIPE ? 0 : 1;
    }
    if (argc == 3 && strcmp(argv[1], "--release-orphaned-barrier") == 0) {
        pid_t child = fork();
        if (child < 0) return 2;
        if (child == 0) _exit(125);
        int result = release_container_barrier(argv[2], child);
        int status = 0;
        if (waitpid(child, &status, 0) != child) return 2;
        return result == 1 && WIFEXITED(status) ? 0 : 1;
    }
    if (argc == 3 && strcmp(argv[1], "--release-live-barrier") == 0) {
        pid_t child = fork();
        if (child < 0) return 2;
        if (child == 0) {
            char message[6];
            int fd = open(argv[2], O_RDONLY | O_CLOEXEC);
            if (fd < 0 || read(fd, message, sizeof(message)) != sizeof(message))
                _exit(125);
            close(fd);
            _exit(memcmp(message, "start\\n", sizeof(message)) == 0 ? 0 : 125);
        }
        int result = release_container_barrier(argv[2], child);
        int status = 0;
        if (waitpid(child, &status, 0) != child) return 2;
        return result == 0 && WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 1;
    }
    if (argc != 3) return 2;
    struct agent_config config = {
        .rootfs = "/rootfs",
        .hostname = "sandbox",
        .uid = "1000",
        .gid = "1000",
        .user = "workload",
        .home = "/home/workload",
        .direct = 0,
    };
    char *workload[] = {"/bin/true", NULL};
    if (strcmp(argv[1], "--sandbox") == 0)
        exec_sandbox(&config, "/unused", argv[2], workload);
    if (strcmp(argv[1], "--direct") == 0)
        exec_direct(&config, argv[2], workload);
    return 2;
}
""",
            encoding="utf-8",
        )
        subprocess.run(
            [
                compiler,
                f'-DCGROUP_ROOT="{fault_cgroup_root}"',
                "-D_GNU_SOURCE",
                "-std=c11",
                "-I",
                str(guest),
                str(fault_harness),
                "-Wl,--wrap=write",
                "-Wl,--wrap=setenv",
                "-Wl,--wrap=execv",
                "-Wl,--wrap=execvp",
                "-o",
                str(cls.fault_executable),
            ],
            check=True,
            capture_output=True,
            timeout=30,
        )

    def decode(
        self,
        environment: tuple[bytes, ...],
        *,
        timeout: int = 0,
        inherit: bool = False,
    ) -> int:
        argument = b"/bin/true"
        flags = 2 | (4 if inherit else 0)
        payload = struct.pack("<IHHHHI", timeout, 1, 1, flags, len(environment), 0)
        payload += struct.pack("<I", len(argument)) + argument
        payload += b"".join(
            struct.pack("<I", len(entry)) + entry for entry in environment
        )
        source = self.root / "request.bin"
        source.write_bytes(payload)
        result = subprocess.run(
            [str(self.executable), str(source)], capture_output=True, timeout=5
        )
        return result.returncode

    def test_accepts_empty_and_exact_environment(self):
        self.assertEqual(self.decode(()), 0)
        self.assertEqual(self.decode((b"EMPTY=", b"VALUE=space = value")), 0)
        self.assertEqual(self.decode((b"VALUE=layered",), inherit=True), 0)

    def test_rejects_duplicate_environment_names(self):
        self.assertEqual(self.decode((b"VALUE=one", b"VALUE=two")), 1)

    def test_rejects_malformed_environment(self):
        for entry in (b"NO_EQUALS", b"=empty-key", b"KEY=embedded\0nul"):
            with self.subTest(entry=entry):
                self.assertEqual(self.decode((entry,)), 1)

    def test_accepts_uint32_timeout_boundaries(self):
        for timeout in (0, 3_600_001, 86_400_000, 0xFFFFFFFF):
            with self.subTest(timeout=timeout):
                self.assertEqual(self.decode((), timeout=timeout), 0)

    def launch(
        self,
        arguments: tuple[bytes, ...],
        environment: tuple[bytes, ...] | None,
        cwd: bytes = b"/",
        *,
        inherit: bool = False,
    ) -> subprocess.CompletedProcess[bytes]:
        entries = () if environment is None else environment
        flags = 1 | (2 if environment is not None else 0) | (4 if inherit else 0)
        payload = struct.pack(
            "<IHHHHI", 0, len(arguments), 1, flags, len(entries), len(cwd)
        )
        payload += b"".join(struct.pack("<I", len(arg)) + arg for arg in arguments)
        payload += cwd
        payload += b"".join(struct.pack("<I", len(entry)) + entry for entry in entries)
        source = self.root / "launch.bin"
        source.write_bytes(payload)
        return subprocess.run(
            [str(self.executable), "--launch", str(source)],
            capture_output=True,
            timeout=5,
            env={"BASE": "inherited", "NVX_EXEC_CONFIG_FD": "synthetic"},
        )

    def test_helper_roundtrip_empty_environment_and_cwd(self):
        result = self.launch((b"/usr/bin/env",), ())
        self.assertEqual(
            (result.returncode, result.stdout, result.stderr), (0, b"", b"")
        )
        result = self.launch((b"/bin/pwd",), (), b"/tmp")
        self.assertEqual(
            (result.returncode, result.stdout, result.stderr), (0, b"/tmp\n", b"")
        )

    def test_helper_preserves_defaults_without_internal_descriptor(self):
        result = self.launch((b"/usr/bin/env",), None, b"/tmp")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, b"")
        self.assertEqual(
            set(result.stdout.splitlines()),
            {b"BASE=inherited", b"PWD=/tmp"},
        )

    def test_helper_layers_explicit_environment_over_defaults(self):
        result = self.launch(
            (b"/usr/bin/env",),
            (b"VALUE=layered",),
            inherit=True,
        )
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, b"")
        self.assertEqual(
            set(result.stdout.splitlines()),
            {b"BASE=inherited", b"PWD=/", b"VALUE=layered"},
        )

        result = self.launch(
            (b"/usr/bin/env",),
            (b"PWD=/explicit",),
            b"/tmp",
            inherit=True,
        )
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, b"")
        self.assertEqual(
            set(result.stdout.splitlines()),
            {b"BASE=inherited", b"PWD=/explicit"},
        )

    def test_helper_roundtrip_large_exact_environment(self):
        entries = tuple(f"KEY{index}=".encode() + b"x" * 4000 for index in range(12))
        result = self.launch((b"/usr/bin/env",), entries)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, b"")
        self.assertEqual(result.stdout, b"\n".join(entries) + b"\n")

    def test_helper_defensively_rejects_relative_cwd(self):
        with tempfile.TemporaryFile() as config:
            config.write(struct.pack("<HHI", 1, 0, 1) + b".")
            config.seek(0)
            fd = config.fileno()
            result = subprocess.run(
                [str(self.executable), "--exec-config-fd", str(fd), "--", "/bin/pwd"],
                pass_fds=(fd,),
                capture_output=True,
                timeout=5,
                env={"BASE": "inherited"},
            )
            self.assertEqual(result.returncode, 125)
            self.assertEqual(result.stdout, b"")
            self.assertIn(b"invalid working directory", result.stderr)

    def test_helper_keeps_a_directory_that_its_caller_entered(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            tempfile.TemporaryFile() as config,
        ):
            config.write(struct.pack("<HHI", 0x8000, 0, 0))
            config.seek(0)
            fd = config.fileno()
            result = subprocess.run(
                [str(self.executable), "--exec-config-fd", str(fd), "--", "/bin/pwd"],
                pass_fds=(fd,),
                capture_output=True,
                timeout=5,
                cwd=directory,
                env={"BASE": "inherited"},
            )
            expected = f"{Path(directory).resolve()}\n".encode()
        self.assertEqual(
            (result.returncode, result.stdout, result.stderr), (0, expected, b"")
        )

    def test_helper_rejects_a_directory_that_is_both_entered_and_named(self):
        with tempfile.TemporaryFile() as config:
            config.write(struct.pack("<HHI", 0x8000 | 1, 0, 1) + b"/")
            config.seek(0)
            fd = config.fileno()
            result = subprocess.run(
                [str(self.executable), "--exec-config-fd", str(fd), "--", "/bin/pwd"],
                pass_fds=(fd,),
                capture_output=True,
                timeout=5,
                env={"BASE": "inherited"},
            )
        self.assertEqual((result.returncode, result.stdout), (125, b""))

    def test_helper_defensively_rejects_malformed_environment(self):
        for entry in (b"NO_EQUALS", b"=empty-key", b"KEY=embedded\0nul"):
            with self.subTest(entry=entry), tempfile.TemporaryFile() as config:
                config.write(struct.pack("<HHI", 2, 1, 0))
                config.write(struct.pack("<I", len(entry)) + entry)
                config.seek(0)
                fd = config.fileno()
                result = subprocess.run(
                    [
                        str(self.executable),
                        "--exec-config-fd",
                        str(fd),
                        "--",
                        "/usr/bin/env",
                    ],
                    pass_fds=(fd,),
                    capture_output=True,
                    timeout=5,
                    env={"BASE": "inherited"},
                )
                self.assertEqual(result.returncode, 125)
                self.assertEqual(result.stdout, b"")

    def test_sandbox_does_not_launch_when_descriptor_export_fails(self):
        marker = self.root / "sandbox-launched"
        result = subprocess.run(
            [str(self.fault_executable), "--sandbox", "42"],
            capture_output=True,
            timeout=5,
            env={
                "NVX_TEST_FAIL_SETENV": "NVX_EXEC_CONFIG_FD",
                "NVX_TEST_EXEC_MARKER": str(marker),
            },
        )
        self.assertEqual(result.returncode, 125)
        self.assertFalse(marker.exists())
        self.assertIn(b"cannot export execution configuration", result.stderr)

    def test_direct_does_not_launch_when_identity_export_fails(self):
        marker = self.root / "direct-launched"
        result = subprocess.run(
            [str(self.fault_executable), "--direct", "42"],
            capture_output=True,
            timeout=5,
            env={
                "NVX_TEST_FAIL_SETENV": "HOME",
                "NVX_TEST_EXEC_MARKER": str(marker),
            },
        )
        self.assertEqual(result.returncode, 125)
        self.assertFalse(marker.exists())
        self.assertIn(b"cannot configure workload environment", result.stderr)

    def test_parent_barrier_release_is_bounded_without_child_reader(self):
        barrier = self.root / "orphaned-barrier"
        barrier.unlink(missing_ok=True)
        subprocess.run(["mkfifo", str(barrier)], check=True, timeout=5)
        result = subprocess.run(
            [str(self.fault_executable), "--release-orphaned-barrier", str(barrier)],
            capture_output=True,
            timeout=5,
        )
        self.assertEqual((result.returncode, result.stderr), (0, b""))

    def test_parent_barrier_release_handles_reader_close_during_write(self):
        barrier = self.root / "closing-barrier"
        barrier.unlink(missing_ok=True)
        subprocess.run(["mkfifo", str(barrier)], check=True, timeout=5)
        result = subprocess.run(
            [str(self.fault_executable), "--release-closing-barrier", str(barrier)],
            capture_output=True,
            timeout=5,
        )
        self.assertEqual((result.returncode, result.stderr), (0, b""))

    def test_parent_barrier_release_preserves_live_child_start(self):
        barrier = self.root / "live-barrier"
        barrier.unlink(missing_ok=True)
        subprocess.run(["mkfifo", str(barrier)], check=True, timeout=5)
        result = subprocess.run(
            [str(self.fault_executable), "--release-live-barrier", str(barrier)],
            capture_output=True,
            timeout=5,
        )
        self.assertEqual((result.returncode, result.stderr), (0, b""))


MS_RDONLY = 1
MS_NOSUID = 2
MS_NODEV = 4
MS_REMOUNT = 32
MS_BIND = 4096
REMOUNT = MS_BIND | MS_REMOUNT | MS_NOSUID | MS_NODEV
APP_EXEC = 2
APP_FEATURES = 5
APP_MAPS = 6
APP_READY = 0x81
APP_ERROR = 0xFF


@unittest.skipUnless(sys.platform.startswith("linux"), "guest agent requires Linux")
class ManagedAgentHostMappingTests(unittest.TestCase):
    """Drives the host mapping table of a direct-mode agent whose mounts are recorded."""

    @classmethod
    def setUpClass(cls):
        compiler = shutil.which("cc")
        if compiler is None:
            raise unittest.SkipTest("guest agent requires the guest build C compiler")
        cls.temporary = tempfile.TemporaryDirectory(prefix="nvx-agent-mappings-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.executable = cls.root / "mappings"
        harness = cls.root / "mappings.c"
        harness.write_text(
            """
#define main nvx_agent_main
#include "nvx-managed-agent.c"
#undef main

int __wrap_mount(const char *source, const char *target, const char *type,
                 unsigned long flags, const void *data) {
    FILE *log = fopen(getenv("NVX_TEST_MOUNT_LOG"), "a");
    (void)type;
    (void)data;
    if (log == NULL) return -1;
    fprintf(log, "%s|%s|%lu\\n", source == NULL ? "-" : source, target, flags);
    fclose(log);
    return 0;
}

/*
 * mappings COMMAND_LINE REQUESTS OUTPUT: handles each request of REQUESTS, a
 * 1-byte kind and a 4-byte length before each payload, with IDs 1, 2, ..., and
 * writes the answers to OUTPUT and the mapping state to stdout.
 */
int main(int argc, char **argv) {
    static unsigned char bytes[1 << 20];
    char command_line[MAX_COMMAND_LINE];
    struct control_session session = {0};
    struct host_mappings mappings = {0};
    struct agent_config config = {
        .rootfs = "-", .hostname = "test", .uid = "1000", .gid = "1000",
        .user = "test", .home = "/", .direct = 1,
    };
    uint64_t request_id = 1;
    size_t offset = 0;
    size_t length;
    FILE *file;

    if (argc != 4) return 2;
    snprintf(command_line, sizeof(command_line), "%s", argv[1]);
    if (parse_host_mapping_count(command_line, &mappings.expected) != 0) return 3;
    file = fopen(argv[2], "rb");
    if (file == NULL) return 2;
    length = fread(bytes, 1, sizeof(bytes), file);
    fclose(file);
    session.fd = open(argv[3], O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (session.fd < 0) return 2;
    while (offset + 5 <= length) {
        struct app_request request = {0};
        request.kind = bytes[offset];
        request.payload_len = read_u32(bytes + offset + 1);
        request.payload = bytes + offset + 5;
        request.request_id = request_id++;
        offset += 5 + request.payload_len;
        if (offset > length) return 2;
        if (handle_app_request(&session, &config, &mappings, &request) < 0) return 4;
    }
    printf("%u %u %d\\n", mappings.expected, mappings.applied, mappings.failed);
    return 0;
}
""",
            encoding="utf-8",
        )
        guest = Path(__file__).resolve().parent.parent / "guest" / "common"
        subprocess.run(
            [
                compiler,
                f'-DHOSTFS_DIR="{cls.root / "hostfs"}"',
                "-D_GNU_SOURCE",
                "-std=c11",
                "-I",
                str(guest),
                str(harness),
                "-Wl,--wrap=mount",
                "-o",
                str(cls.executable),
            ],
            check=True,
            capture_output=True,
            timeout=30,
        )

    def setUp(self):
        self.case = Path(tempfile.mkdtemp(prefix="case-", dir=self.root))
        export = self.root / "hostfs" / "root"
        shutil.rmtree(export, ignore_errors=True)
        (export / "0" / "src").mkdir(parents=True)
        (export / "0" / "notes.txt").write_text("notes", encoding="utf-8")
        (export / "1").mkdir()
        self.log = self.case / "mounts.log"

    @staticmethod
    def maps(
        first: int,
        entries: Sequence[tuple[str | bytes, str | bytes, int]],
        count: int | None = None,
    ) -> bytes:
        payload = struct.pack("<II", first, len(entries) if count is None else count)
        for source, target, flags in entries:
            source_bytes = source.encode() if isinstance(source, str) else source
            target_bytes = target.encode() if isinstance(target, str) else target
            payload += struct.pack("<HHH", flags, len(source_bytes), len(target_bytes))
            payload += source_bytes + target_bytes
        return payload

    def run_agent(
        self, command_line: str, requests: Sequence[tuple[int, bytes]]
    ) -> tuple[subprocess.CompletedProcess[bytes], list[tuple[int, int, int, bytes]]]:
        source = self.case / "requests.bin"
        source.write_bytes(
            b"".join(
                struct.pack("<BI", kind, len(payload)) + payload
                for kind, payload in requests
            )
        )
        output = self.case / "answers.bin"
        result = subprocess.run(
            [str(self.executable), command_line, str(source), str(output)],
            capture_output=True,
            timeout=10,
            env={"NVX_TEST_MOUNT_LOG": str(self.log)},
        )
        answers: list[tuple[int, int, int, bytes]] = []
        data = output.read_bytes() if output.exists() else b""
        while data:
            (length,) = struct.unpack_from("<I", data, 40)
            frame = data[44 : 44 + length]
            data = data[44 + length :]
            if frame[:4] != b"NVXC":
                continue
            kind = frame[5]
            request_id, status, payload_len = struct.unpack_from("<QiI", frame, 8)
            answers.append((kind, request_id, status, frame[24 : 24 + payload_len]))
        return result, answers

    def mounts(self) -> list[tuple[str, str, int]]:
        if not self.log.exists():
            return []
        return [
            (source, target, int(flags))
            for source, target, flags in (
                line.split("|") for line in self.log.read_text().splitlines()
            )
        ]

    def test_mappings_mount_in_order_and_unblock_workloads(self):
        export = self.root / "hostfs" / "root"
        src = str(self.case / "guest" / "src")
        notes = str(self.case / "guest" / "src" / "notes.txt")
        out = str(self.case / "guest" / "out")
        result, answers = self.run_agent(
            # A stale token of an older host is ignored.
            "quiet nvx_map=0,/stale,rw nvx_maps=3",
            [
                (APP_EXEC, b""),
                (
                    APP_MAPS,
                    self.maps(0, [("0/src", src, 1), ("0/notes.txt", notes, 0)]),
                ),
                (APP_EXEC, b""),
                (APP_MAPS, self.maps(2, [("1", out, 0)])),
                # An empty request is malformed, so it fails only once the mappings are complete.
                (APP_EXEC, b""),
            ],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"3 3 0\n")
        self.assertEqual(
            [(kind, status, payload) for kind, _, status, payload in answers],
            [
                (APP_ERROR, 11, b"mappings-incomplete"),
                (APP_READY, 0, b""),
                (APP_ERROR, 11, b"mappings-incomplete"),
                (APP_READY, 0, b""),
                (APP_ERROR, 22, b"invalid-request"),
            ],
        )
        self.assertEqual(
            self.mounts(),
            [
                (str(export / "0" / "src"), src, MS_BIND),
                ("-", src, REMOUNT | MS_RDONLY),
                (str(export / "0" / "notes.txt"), notes, MS_BIND),
                ("-", notes, REMOUNT),
                (str(export / "1"), out, MS_BIND),
                ("-", out, REMOUNT),
            ],
        )
        self.assertTrue(Path(src).is_dir())
        self.assertTrue(Path(notes).is_file())
        self.assertTrue(Path(out).is_dir())

    def test_entries_beyond_the_announcement_or_out_of_order_are_refused(self):
        target = str(self.case / "guest" / "src")
        entry = [("0/src", target, 0)]
        result, answers = self.run_agent(
            "nvx_maps=1",
            [
                (APP_MAPS, self.maps(1, entry)),
                (APP_MAPS, self.maps(0, entry * 2)),
                (APP_MAPS, self.maps(0, entry)),
                (APP_MAPS, self.maps(1, entry)),
            ],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"1 1 0\n")
        self.assertEqual(
            [(kind, status) for kind, _, status, _ in answers],
            [(APP_ERROR, 22), (APP_ERROR, 22), (APP_READY, 0), (APP_ERROR, 22)],
        )
        self.assertEqual(len(self.mounts()), 2)

    def test_malformed_tables_mount_nothing(self):
        target = str(self.case / "guest" / "x")
        good = ("0/src", target, 0)
        malformed = [
            self.maps(0, [good, ("0/../1", target, 0)]),
            self.maps(0, [good, ("/0/src", target, 0)]),
            self.maps(0, [good, ("0/src", "relative", 0)]),
            self.maps(0, [good, (".", target, 0)]),
            self.maps(0, [good, ("0//src", target, 0)]),
            self.maps(0, [good, (b"0/s\0rc", target, 0)]),
            self.maps(0, [good, ("0/src", target, 2)]),
            self.maps(0, [good, ("", target, 0)]),
            self.maps(0, [good]) + b"\0",
            self.maps(0, [good], count=2),
            self.maps(0, [], count=0),
            b"\0\0\0",
        ]
        result, answers = self.run_agent(
            "nvx_maps=2", [(APP_MAPS, payload) for payload in malformed]
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"2 0 0\n")
        self.assertEqual(
            [(kind, status, payload) for kind, _, status, payload in answers],
            [(APP_ERROR, 22, b"invalid-request")] * len(malformed),
        )
        self.assertEqual(self.mounts(), [])

    def test_without_an_announcement_tables_are_refused(self):
        target = str(self.case / "guest" / "src")
        result, answers = self.run_agent(
            "nvx_map=0,/w,rw",
            [(APP_MAPS, self.maps(0, [("0/src", target, 0)])), (APP_EXEC, b"")],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"0 0 0\n")
        self.assertEqual(
            [(kind, status, payload) for kind, _, status, payload in answers],
            [(APP_ERROR, 22, b"invalid-request"), (APP_ERROR, 22, b"invalid-request")],
        )
        self.assertEqual(self.mounts(), [])

    def test_a_failed_mount_keeps_refusing_workloads(self):
        target = str(self.case / "guest" / "missing")
        result, answers = self.run_agent(
            "nvx_maps=2",
            [
                (APP_MAPS, self.maps(0, [("0/missing", target, 0)])),
                (APP_EXEC, b""),
                (APP_MAPS, self.maps(0, [("0/src", target, 0)])),
            ],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"2 0 1\n")
        self.assertEqual(
            [(kind, status, payload) for kind, _, status, payload in answers],
            [
                (APP_ERROR, 2, b"mapping-failed"),
                (APP_ERROR, 11, b"mappings-incomplete"),
                (APP_ERROR, 22, b"invalid-request"),
            ],
        )

    def test_mapping_counts_are_validated(self):
        for command_line in [
            "nvx_maps=0",
            "nvx_maps=",
            "nvx_maps=4097",
            "nvx_maps=99999999999",
            "nvx_maps=1x",
            "nvx_maps=1 nvx_maps=1",
        ]:
            result, _ = self.run_agent(command_line, [])
            self.assertEqual(result.returncode, 3, command_line)
        result, _ = self.run_agent("nvx_maps=4096", [])
        self.assertEqual(result.stdout, b"4096 0 0\n")

    def test_features_announce_the_mapping_table_but_not_retired_bit_one(self):
        _, answers = self.run_agent("", [(APP_FEATURES, b"")])
        self.assertEqual(len(answers), 1)
        kind, _, status, payload = answers[0]
        self.assertEqual((kind, status), (APP_READY, 0))
        (features,) = struct.unpack("<I", payload)
        self.assertEqual(features, 0b111_1101)


if __name__ == "__main__":
    unittest.main()

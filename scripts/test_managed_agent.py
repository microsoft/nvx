#!/usr/bin/env python3
"""Compile the existing guest decoder on Linux and test real wire inputs."""

import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
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
        int fd = create_exec_config_fd(&config);
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

    def decode(self, environment: tuple[bytes, ...], *, timeout: int = 0) -> int:
        argument = b"/bin/true"
        payload = struct.pack("<IHHHHI", timeout, 1, 1, 2, len(environment), 0)
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
    ) -> subprocess.CompletedProcess[bytes]:
        entries = () if environment is None else environment
        flags = 1 | (2 if environment is not None else 0)
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
        result = self.launch((b"/usr/bin/env",), None)
        self.assertEqual(
            (result.returncode, result.stdout, result.stderr),
            (0, b"BASE=inherited\n", b""),
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


if __name__ == "__main__":
    unittest.main()

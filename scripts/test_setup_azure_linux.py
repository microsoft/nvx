"""Tests of the Linux commands of the Azure runner bootstrap.

They exercise the controllers directly against fake commands, HTTP endpoints,
and temporary paths, so no test changes the host's disks, firewall, boot
configuration, or services.
"""

import contextlib
import io
import json
import os
import sys
import tempfile
import threading
import unittest
import urllib.request
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import BinaryIO, cast
from unittest.mock import patch

from scripts.setup.azure import build_package as builder
from scripts.setup.azure.nvx_azure import (
    cli,
    common,
    linux_bootstrap,
    linux_disk,
    linux_firewall,
    linux_mshv,
)
from scripts.setup.azure.nvx_azure.common import BootstrapError, Completed, JsonObject

Handler = Callable[[list[str], bytes | None], Completed]


def ok(stdout: str = "") -> Completed:
    return Completed(0, stdout.encode())


def status(code: int, stdout: str = "", stderr: str = "") -> Completed:
    return Completed(code, stdout.encode(), stderr.encode())


class Commands:
    """Answer external commands from a handler, recording every call."""

    def __init__(self, handler: Handler) -> None:
        self.handler = handler
        self.calls: list[list[str]] = []
        self.inputs: list[bytes | None] = []
        self.environments: list[Mapping[str, str] | None] = []

    def __call__(
        self,
        args: Sequence[str],
        *,
        stdin: bytes | None = None,
        check: bool = True,
        capture: bool = True,
        output: BinaryIO | None = None,
        outputs: Sequence[common.Writable] | None = None,
        env: Mapping[str, str] | None = None,
        cwd: Path | None = None,
    ) -> Completed:
        command = list(args)
        self.calls.append(command)
        self.inputs.append(stdin)
        self.environments.append(env)
        result = self.handler(command, stdin)
        if outputs is not None:
            data = result.stdout + result.stderr
            for stream in outputs:
                stream.write(data)
                stream.flush()
            result = Completed(result.returncode)
        if check and result.returncode:
            raise BootstrapError(
                f"{command[0]} failed with exit code {result.returncode}"
            )
        return result


def unexpected(command: list[str], stdin: bytes | None) -> Completed:
    raise AssertionError(f"unexpected command: {command}")


def linux_config() -> builder.LinuxConfig:
    config = builder.load_config(builder.ROOT / "linux" / "config.example.json")
    assert config["os"] == "linux"
    return config


def as_json(config: builder.LinuxConfig) -> JsonObject:
    return cast(JsonObject, json.loads(builder.configuration_bytes(config)))


def compute_metadata(config: builder.LinuxConfig, instance: int = 7) -> JsonObject:
    disk: JsonObject = {
        "diskSizeGB": "512",
        "managedDisk": {"storageAccountType": "Premium_LRS"},
        "caching": "None",
        "writeAcceleratorEnabled": "false",
    }
    return {
        "subscriptionId": config["subscriptionId"],
        "resourceGroupName": config["resourceGroup"].upper(),
        "vmScaleSetName": config["vmssName"].upper(),
        "name": f"{config['vmssName']}_{instance}",
        "vmSize": "Standard_D8s_v5",
        "storageProfile": {
            "osDisk": {**disk, "diskSizeGB": "128", "caching": "ReadWrite"},
            "dataDisks": [{**disk, "lun": "0"}],
            "resourceDisk": {"size": ""},
        },
    }


class Opener:
    """Serve JSON responses to urllib requests from a handler."""

    def __init__(self, handler: Callable[[urllib.request.Request], object]) -> None:
        self.handler = handler
        self.requests: list[urllib.request.Request] = []

    def open(self, request: urllib.request.Request, timeout: float) -> io.StringIO:
        if timeout != common.HTTP_TIMEOUT_SECONDS:
            raise AssertionError("requests must use the bootstrap timeout")
        self.requests.append(request)
        return io.StringIO(json.dumps(self.handler(request)))


def serve(opener: Opener) -> contextlib.AbstractContextManager[object]:
    return patch.object(urllib.request, "build_opener", return_value=opener)


class CommonCommandTests(unittest.TestCase):
    def test_uncaptured_command_flushes_parent_output_first(self):
        events: list[str] = []

        class Stream:
            def __init__(self, name: str) -> None:
                self.name = name

            def flush(self) -> None:
                events.append(self.name)

        def run(*_args: object, **_kwargs: object) -> Completed:
            events.append("run")
            return Completed(0)

        with (
            patch.object(common.sys, "stdout", Stream("stdout")),
            patch.object(common.sys, "stderr", Stream("stderr")),
            patch.object(common.subprocess, "run", run),
        ):
            common.run_command(["setup"], capture=False)

        self.assertEqual(events, ["stdout", "stderr", "run"])

    def test_failure_flushes_standard_output_before_the_error(self):
        events: list[str] = []

        class Stream:
            def __init__(self, name: str) -> None:
                self.name = name

            def write(self, text: str) -> None:
                events.append(f"{self.name}.write:{text}")

            def flush(self) -> None:
                events.append(f"{self.name}.flush")

        with (
            patch.object(common.sys, "stdout", Stream("stdout")),
            patch.object(common.sys, "stderr", Stream("stderr")),
        ):
            self.assertEqual(common.fail("Mock failure"), 1)

        self.assertEqual(events[0], "stdout.flush")
        self.assertIn("stderr.write:ERROR: Mock failure", events)

    def test_streamed_output_arrives_before_the_child_exits(self):
        class Sink:
            def __init__(self) -> None:
                self.data = bytearray()
                self.wrote = threading.Event()

            def write(self, data: bytes) -> None:
                self.data.extend(data)
                self.wrote.set()

            def flush(self) -> None:
                pass

        with tempfile.TemporaryDirectory() as temporary:
            release = Path(temporary) / "release"
            sink = Sink()
            errors: list[BaseException] = []

            def run() -> None:
                try:
                    common.run_command(
                        [
                            sys.executable,
                            "-c",
                            "import pathlib, sys, time\n"
                            "print('started', flush=True)\n"
                            "release = pathlib.Path(sys.argv[1])\n"
                            "while not release.exists():\n"
                            "    time.sleep(.05)\n",
                            str(release),
                        ],
                        capture=False,
                        outputs=[sink],
                    )
                except BaseException as error:
                    errors.append(error)

            worker = threading.Thread(target=run)
            worker.start()
            try:
                self.assertTrue(sink.wrote.wait(5), "child output remained buffered")
                self.assertTrue(
                    worker.is_alive(), "child exited before its output arrived"
                )
                self.assertIn(b"started", sink.data)
            finally:
                release.touch()
                worker.join(5)
            self.assertFalse(worker.is_alive(), "child did not exit after release")
            self.assertEqual(errors, [])

    def test_command_streams_stdout_and_stderr_to_one_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "command.log"
            with output.open("wb") as stream:
                completed = common.run_command(
                    [
                        sys.executable,
                        "-c",
                        "import sys; print('stdout', flush=True); "
                        "print('stderr', file=sys.stderr, flush=True)",
                    ],
                    capture=False,
                    output=stream,
                )
            self.assertEqual(output.read_bytes().splitlines(), [b"stdout", b"stderr"])
        self.assertEqual(completed, Completed(0))


class LinuxSettingsTests(unittest.TestCase):
    def test_settings_reader(self):
        self.assertEqual(
            linux_bootstrap.read_settings(as_json(linux_config())),
            linux_bootstrap.Settings(
                backend="kvm",
                admin_username="ciadmin",
                repository="microsoft/nvx",
                runner_directory="/opt/nvx-runner",
                lun=0,
                mount_point="/mnt/data",
                kernel_mode="preinstalled",
            ),
        )

    def test_reserved_administrator_is_rejected_before_host_setup(self):
        for administrator in ("root", "nvx-runner"):
            with self.subTest(administrator=administrator):
                config = linux_config()
                config["adminUsername"] = administrator
                with self.assertRaisesRegex(ValueError, "root or nvx-runner"):
                    builder.normalize_config(config)
                with self.assertRaisesRegex(BootstrapError, "root or nvx-runner"):
                    linux_bootstrap.read_settings(as_json(config))

    def test_only_linux_backends_are_accepted(self):
        for update in ({"os": "windows"}, {"backend": "whp"}):
            with self.subTest(update=update):
                config = {**as_json(linux_config()), **update}
                with self.assertRaisesRegex(BootstrapError, "Linux KVM or MSHV"):
                    linux_bootstrap.read_settings(config)

    def test_only_extra_labels_reach_the_generic_setup(self):
        config = linux_config()
        config["runnerLabels"].append("isolated")
        settings = linux_bootstrap.read_settings(as_json(config))
        self.assertEqual(
            common.split_labels(config["runnerLabels"], settings.required_labels),
            ("isolated", False),
        )
        self.assertEqual(
            linux_bootstrap.setup_arguments(settings, "isolated"),
            [
                "--runner-only",
                "--backend",
                "kvm",
                "--repository-url",
                "https://github.com/microsoft/nvx",
                "--runner-directory",
                "/opt/nvx-runner",
                "--extra-runner-labels",
                "isolated",
            ],
        )
        self.assertNotIn(
            "--extra-runner-labels", linux_bootstrap.setup_arguments(settings, "")
        )

    def test_runner_roles_map_to_the_setup_switch(self):
        for role, benchmark in ((None, False), ("general", False), ("benchmark", True)):
            with self.subTest(role=role):
                config = linux_config()
                if role is not None:
                    config["runnerLabels"].append(role)
                self.assertEqual(
                    common.split_labels(
                        config["runnerLabels"], ["linux", "kvm", "virtual-machine"]
                    ),
                    ("", benchmark),
                )
        config = linux_config()
        config["runnerLabels"].extend(("general", "benchmark"))
        with self.assertRaisesRegex(BootstrapError, "both general and benchmark"):
            linux_bootstrap.read_settings(as_json(config))

    def test_required_labels_keep_platform_and_backend(self):
        for labels in (
            ["linux", "mshv", "virtual-machine"],
            ["linux", "kvm"],
            ["kvm", "linux", "virtual-machine"],
        ):
            with self.subTest(labels=labels):
                config = {**as_json(linux_config()), "runnerLabels": labels}
                with self.assertRaisesRegex(BootstrapError, "platform and backend"):
                    linux_bootstrap.read_settings(config)
        self.assertEqual(
            common.split_labels(
                ["LINUX", "KVM", "Virtual-Machine", "Isolated"],
                ["linux", "kvm", "virtual-machine"],
            ),
            ("Isolated", False),
        )


class LinuxIdentityTests(unittest.TestCase):
    def test_metadata_identity_names_the_runner(self):
        config = linux_config()
        metadata = compute_metadata(config)
        self.assertEqual(common.runner_name(as_json(config), metadata), "nvx-linux-8")
        config["runnerNumberOffset"] = 0
        self.assertEqual(common.runner_name(as_json(config), metadata), "nvx-linux-7")
        for field, value in (
            ("vmScaleSetName", "another-vmss"),
            ("resourceGroupName", "another-group"),
            ("subscriptionId", "11111111-1111-1111-1111-111111111111"),
            ("name", "another-vmss_7"),
        ):
            with self.subTest(field=field):
                with self.assertRaisesRegex(BootstrapError, "does not identify"):
                    common.runner_name(as_json(config), {**metadata, field: value})
        # str.isdecimal() would accept the Arabic-Indic digit seven.
        for name in (
            "nvx-linux-ci-vmss_",
            "nvx-linux-ci-vmss_x7",
            "nvx-linux-ci-vmss_\u0667",
        ):
            with self.subTest(name=name):
                with self.assertRaisesRegex(BootstrapError, "numeric VMSS instance"):
                    common.runner_name(as_json(config), {**metadata, "name": name})

    def refresh(self, config: builder.LinuxConfig, instance: int) -> tuple[str, bool]:
        settings = linux_bootstrap.read_settings(as_json(config))
        opener = Opener(lambda _: compute_metadata(config, instance))
        with tempfile.TemporaryDirectory() as temporary, serve(opener):
            _, extra, benchmark = linux_bootstrap.refresh_metadata(
                as_json(config), settings, Path(temporary), owner=None
            )
        return extra, benchmark

    def test_benchmark_runner_name_selects_one_instance(self):
        config = linux_config()
        config["runnerLabels"].extend(("general", "isolated"))
        config["benchmarkRunnerName"] = "nvx-linux-8"
        for instance, offset, expected in ((7, 1, True), (8, 1, False), (8, 0, True)):
            with self.subTest(instance=instance, offset=offset):
                config["runnerNumberOffset"] = offset
                self.assertEqual(self.refresh(config, instance), ("isolated", expected))

    def test_snapshot_projects_only_safe_host_metadata(self):
        config = linux_config()
        metadata = compute_metadata(config)
        metadata["tags"] = "private-tag"
        metadata["userData"] = "private-user-data"
        snapshot = common.snapshot_bytes(common.safe_compute_metadata(metadata))
        self.assertEqual(set(json.loads(snapshot)), {"vmSize", "storageProfile"})
        for private in ("private-tag", "private-user-data", config["vmssName"]):
            self.assertNotIn(private.encode(), snapshot)
        metadata["vmSize"] = "Standard_E8-4s_v5"
        self.assertEqual(
            common.safe_compute_metadata(metadata)["vmSize"], "Standard_E8-4s_v5"
        )
        invalid: tuple[JsonObject, ...] = (
            {"vmSize": "invalid-size"},
            {"vmSize": 8},
            {"storageProfile": {"dataDisks": []}},
            {"storageProfile": []},
        )
        for update in invalid:
            with self.subTest(update=update):
                with self.assertRaisesRegex(BootstrapError, "Azure compute metadata"):
                    common.safe_compute_metadata({**metadata, **update})
        profile = cast(JsonObject, metadata["storageProfile"])
        data_disk = cast(list[JsonObject], profile["dataDisks"])[0]
        for field, value in (
            ("lun", "64"),
            ("caching", "Sometimes"),
            ("diskSizeGB", 512),
            ("bytesPerSecondThrottle", "fast"),
        ):
            with self.subTest(field=field):
                disks = [{**data_disk, field: value}]
                with self.assertRaisesRegex(BootstrapError, "Azure compute metadata"):
                    common.safe_compute_metadata(
                        {**metadata, "storageProfile": {**profile, "dataDisks": disks}}
                    )

    def check_refresh(self, scenario: str) -> None:
        config = linux_config()
        settings = linux_bootstrap.read_settings(as_json(config))
        fresh = compute_metadata(config)
        profile = cast(JsonObject, fresh["storageProfile"])
        profile["osDisk"] = {
            **cast(JsonObject, profile["osDisk"]),
            "caching": "ReadOnly",
        }

        def respond(request: urllib.request.Request) -> object:
            if scenario == "query-failure":
                raise OSError("metadata endpoint unavailable")
            return fresh

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(common, "run_command", Commands(unexpected)),
            serve(Opener(respond)),
        ):
            directory = Path(temporary) / "metadata"
            directory.mkdir()
            snapshot = directory / "compute.json"
            snapshot.write_text('{"original":true}', encoding="utf-8")
            if scenario == "success":
                name, _, _ = linux_bootstrap.refresh_metadata(
                    as_json(config), settings, directory, owner=None
                )
                self.assertEqual(name, "nvx-linux-8")
                expected: object = common.safe_compute_metadata(fresh)
            else:
                with self.assertRaisesRegex(OSError, "metadata endpoint"):
                    linux_bootstrap.refresh_metadata(
                        as_json(config), settings, directory, owner=None
                    )
                expected = {"original": True}
            self.assertEqual(json.loads(snapshot.read_text()), expected)
            self.assertEqual(
                [path.name for path in directory.iterdir()], ["compute.json"]
            )

    def test_refresh_replaces_the_snapshot_without_provisioning(self):
        for scenario in ("success", "query-failure"):
            with self.subTest(scenario=scenario):
                self.check_refresh(scenario)

    def test_snapshot_directory_must_not_be_a_symlink(self):
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "target"
            target.mkdir()
            link = Path(temporary) / "link"
            try:
                link.symlink_to(target, target_is_directory=True)
            except OSError:
                self.skipTest("symbolic links are unavailable")
            with self.assertRaisesRegex(BootstrapError, "must not be a symlink"):
                linux_bootstrap.write_metadata_snapshot({}, link, owner=None)
            self.assertEqual(list(target.iterdir()), [])


class LinuxRegistrationTests(unittest.TestCase):
    def test_registration_flow(self):
        def respond(request: urllib.request.Request) -> object:
            if "/metadata/identity/" in request.full_url:
                self.assertEqual(request.get_header("Metadata"), "true")
                return {"access_token": "test-managed-identity"}
            if ".vault.azure.net/" in request.full_url:
                self.assertEqual(
                    request.full_url,
                    "https://nvx-ci-example-vault.vault.azure.net/secrets/"
                    "github-runner-bootstrap-token?api-version=7.4",
                )
                self.assertEqual(
                    request.get_header("Authorization"), "Bearer test-managed-identity"
                )
                return {"value": "test-github-bootstrap"}
            self.assertEqual(
                request.full_url,
                "https://api.github.com/repos/microsoft/nvx/actions/runners/"
                "registration-token",
            )
            self.assertEqual(
                request.get_header("Authorization"), "Bearer test-github-bootstrap"
            )
            self.assertEqual(request.get_method(), "POST")
            return {"token": "test-registration"}

        opener = Opener(respond)
        with patch.object(
            urllib.request, "build_opener", return_value=opener
        ) as build_opener:
            token = common.registration_token(as_json(linux_config()))
        self.assertEqual(token, "test-registration")
        self.assertEqual(len(opener.requests), 3)
        # Only instance metadata bypasses proxies.
        handlers = [call.args for call in build_opener.call_args_list]
        self.assertEqual(len(handlers[0]), 1)
        self.assertIsInstance(handlers[0][0], urllib.request.ProxyHandler)
        self.assertEqual(handlers[1:], [(), ()])

    def check_missing_credential(self, missing: str, expected: str, count: int) -> None:
        def respond(request: urllib.request.Request) -> object:
            if "/metadata/identity/" in request.full_url:
                if missing == "identity":
                    return {"access_token": ""}
                return {
                    "access_token": "sensitive\nidentity"
                    if missing == "identity-multiline"
                    else "identity"
                }
            if ".vault.azure.net/" in request.full_url:
                if missing == "secret":
                    return {"value": ""}
                return {
                    "value": "sensitive\nsecret"
                    if missing == "secret-multiline"
                    else "credential"
                }
            return {
                "token": "sensitive\nregistration" if missing == "multiline" else ""
            }

        opener = Opener(respond)
        with (
            serve(opener),
            self.assertRaisesRegex(BootstrapError, expected) as raised,
        ):
            common.registration_token(as_json(linux_config()))
        self.assertNotIn("sensitive", str(raised.exception))
        self.assertEqual(len(opener.requests), count)

    def test_missing_or_malformed_credentials_stop_registration(self):
        for missing, expected, count in (
            ("identity", "no Key Vault access token", 1),
            ("identity-multiline", "invalid Key Vault access token", 1),
            ("secret", "no runner bootstrap credential", 2),
            ("secret-multiline", "invalid runner bootstrap credential", 2),
            ("token", "invalid runner registration token", 3),
            ("multiline", "invalid runner registration token", 3),
        ):
            with self.subTest(missing=missing):
                self.check_missing_credential(missing, expected, count)

    def check_bootstrap(self, role: str | None, instance: int) -> None:
        config = linux_config()
        if role is not None:
            config["runnerLabels"].append(role)
        if role == "general":
            config["benchmarkRunnerName"] = "nvx-linux-10"
        events: list[str] = []

        def respond(request: urllib.request.Request) -> object:
            if request.full_url == common.COMPUTE_METADATA_URL:
                return compute_metadata(config, instance)
            if "restart" not in events:
                raise AssertionError("credential requested before the block")
            events.append("credential")
            if "/metadata/identity/" in request.full_url:
                return {"access_token": "identity"}
            if ".vault.azure.net/" in request.full_url:
                return {"value": "credential"}
            return {"token": "registration"}

        def handle(command: list[str], stdin: bytes | None) -> Completed:
            if command[:2] == ["systemctl", "restart"]:
                events.append("restart")
            elif command[:1] == ["sudo"]:
                events.append("setup" if stdin is None else "register")
            return ok()

        commands = Commands(handle)
        with (
            tempfile.TemporaryDirectory() as temporary,
            serve(Opener(respond)),
            patch.object(common, "run_command", commands),
            patch.object(linux_bootstrap, "admin_uid", return_value=1000),
            patch.object(linux_bootstrap, "write_metadata_snapshot"),
            patch.object(linux_disk, "setup") as disk,
            patch.object(linux_disk, "is_mounted", return_value=True),
            patch.object(linux_bootstrap, "prepare_backend", return_value=True),
            patch.object(linux_bootstrap, "finalize_runner") as finalize,
        ):
            config_path = Path(temporary) / "config.json"
            config_path.write_bytes(builder.configuration_bytes(config))
            linux_bootstrap.bootstrap(config_path)
        disk.assert_called_once_with(0, "/mnt/data")
        finalize.assert_called_once_with(Path("/opt/nvx-runner"), config_path)
        self.assertEqual(
            events,
            ["setup", "restart", "credential", "credential", "credential", "register"],
        )
        self.assertEqual(
            [call for call in commands.calls if call[0] == "systemctl"],
            [
                ["systemctl", "enable", linux_bootstrap.METADATA_SERVICE],
                ["systemctl", "restart", linux_bootstrap.METADATA_SERVICE],
                ["systemctl", "is-active", "--quiet", linux_bootstrap.METADATA_SERVICE],
            ],
        )
        setups = [
            (call, stdin)
            for call, stdin in zip(commands.calls, commands.inputs, strict=True)
            if call[0] == "sudo"
        ]
        prefix = ["sudo", "-u", "ciadmin", "-H", "sh"]
        prefix.append("/opt/nvx-bootstrap/setup-linux.sh")
        self.assertEqual(len(setups), 2)
        self.assertEqual(setups[0][0][:6], prefix)
        self.assertIsNone(setups[0][1])
        registration, stdin = setups[1]
        self.assertEqual(registration[:6], prefix)
        # The token travels on standard input only.
        self.assertEqual(stdin, b"registration\n")
        self.assertNotIn("registration", " ".join(registration))
        self.assertEqual(registration[-1], "--runner-token-stdin")
        name = "nvx-linux-8" if instance == 7 else "nvx-linux-10"
        self.assertEqual(registration[registration.index("--runner-name") + 1], name)
        self.assertEqual("--benchmark-runner" in registration, role is not None)

    def test_bootstrap_blocks_metadata_before_requesting_credentials(self):
        for role, instance in ((None, 7), ("benchmark", 7), ("general", 9)):
            with self.subTest(role=role):
                self.check_bootstrap(role, instance)


class LinuxBackendTests(unittest.TestCase):
    def settings(self, mode: str = "azurelinux-mshv") -> linux_bootstrap.Settings:
        return linux_bootstrap.Settings(
            backend="mshv",
            admin_username="ciadmin",
            repository="microsoft/nvx",
            runner_directory="/opt/nvx-runner",
            lun=0,
            mount_point="/mnt/data",
            kernel_mode=mode,
        )

    def test_mshv_host_stack_reboots_once_into_dom0(self):
        commands = Commands(lambda command, stdin: ok())
        output = io.StringIO()
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(common, "run_command", commands),
            patch.object(linux_mshv, "setup") as setup,
            patch.object(linux_bootstrap, "is_character_device", return_value=False),
            contextlib.redirect_stdout(output),
        ):
            paths = linux_mshv.MshvPaths(
                done=Path(temporary) / "done", device=Path(temporary) / "mshv"
            )
            self.assertFalse(linux_bootstrap.prepare_backend(self.settings(), paths))
            setup.assert_called_once_with(paths)
            self.assertEqual(
                commands.calls,
                [
                    [
                        "shutdown",
                        "-r",
                        "+1",
                        "Booting the MSHV Dom0 entry for the CI runner",
                    ]
                ],
            )
            self.assertIn("NVX_CLOUD_RUNNER_REBOOT_SCHEDULED=1", output.getvalue())
            paths.done.touch()
            with self.assertRaisesRegex(BootstrapError, "after host-stack enablement"):
                linux_bootstrap.prepare_backend(self.settings(), paths)
            setup.assert_called_once()

    def test_running_mshv_must_pass_verification(self):
        with (
            patch.object(linux_bootstrap, "is_character_device", return_value=True),
            patch.object(linux_mshv, "verify", side_effect=(True, False)) as verify,
        ):
            self.assertTrue(linux_bootstrap.prepare_backend(self.settings()))
            with self.assertRaisesRegex(BootstrapError, "MSHV verification failed"):
                linux_bootstrap.prepare_backend(self.settings())
        self.assertEqual(verify.call_count, 2)

    def test_preinstalled_backend_requires_its_device(self):
        with patch.object(linux_mshv, "verify") as verify:
            with patch.object(
                linux_bootstrap, "is_character_device", return_value=False
            ):
                with self.assertRaisesRegex(BootstrapError, "/dev/mshv is unavailable"):
                    linux_bootstrap.prepare_backend(self.settings("preinstalled"))
            with patch.object(
                linux_bootstrap, "is_character_device", return_value=True
            ):
                self.assertTrue(
                    linux_bootstrap.prepare_backend(self.settings("preinstalled"))
                )
        verify.assert_not_called()


class LinuxFinalizationTests(unittest.TestCase):
    SERVICE = "actions.runner.test.pool.service"

    def check_finalization(self, scenario: str) -> None:
        name = "actions.runner..service" if scenario == "invalid-name" else self.SERVICE
        verified: list[bool] = []

        def handle(command: list[str], stdin: bytes | None) -> Completed:
            if command == ["systemctl", "show", name, "--property=User", "--value"]:
                return ok("root\n" if scenario == "wrong-user" else "nvx-runner\n")
            if command == ["systemctl", "daemon-reload"]:
                return ok()
            if command == ["systemctl", "is-active", "--quiet", self.SERVICE]:
                if scenario == "inactive":
                    return status(3)
                verified.append(True)
                return ok()
            if command == [
                "systemctl",
                "disable",
                linux_bootstrap.PROVISIONING_SERVICE,
            ]:
                if not verified:
                    raise AssertionError("provisioning disabled before verification")
                return status(1) if scenario == "disable-failure" else ok()
            raise AssertionError(f"unexpected service operation: {command}")

        output = io.StringIO()
        config_path = Path("/opt/nvx-bootstrap/config.json")
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(common, "run_command", Commands(handle)),
            contextlib.redirect_stdout(output),
        ):
            root = Path(temporary)
            runner = root / "runner"
            runner.mkdir()
            (runner / ".service").write_text(name + "\n", encoding="utf-8")
            drop_in = root / "systemd" / f"{self.SERVICE}.d" / "azure-metadata.conf"
            if scenario == "success":
                linux_bootstrap.finalize_runner(runner, config_path, root / "systemd")
                entry = "/usr/bin/python3 -I -B /opt/nvx-bootstrap"
                self.assertEqual(
                    drop_in.read_text(encoding="utf-8"),
                    "[Unit]\n"
                    "Requires=nvx-runner-metadata-block.service\n"
                    "Wants=network-online.target\n"
                    "After=nvx-runner-metadata-block.service network-online.target\n"
                    "[Service]\n"
                    f"ExecStartPre=+{entry} linux-block-metadata\n"
                    f"ExecStartPre=+{entry} linux-refresh-metadata "
                    "--config /opt/nvx-bootstrap/config.json\n",
                )
            else:
                with self.assertRaises(BootstrapError):
                    linux_bootstrap.finalize_runner(
                        runner, config_path, root / "systemd"
                    )
                if scenario in ("wrong-user", "invalid-name"):
                    self.assertFalse(drop_in.exists())
        complete = "NVX_CLOUD_RUNNER_SETUP_COMPLETE=1" in output.getvalue()
        self.assertEqual(complete, scenario == "success")

    def test_finalization_does_not_restart_registered_service(self):
        for scenario in (
            "success",
            "wrong-user",
            "invalid-name",
            "inactive",
            "disable-failure",
        ):
            with self.subTest(scenario=scenario):
                self.check_finalization(scenario)

    def test_units_and_drop_in_share_the_entry_point(self):
        self.assertEqual(
            " ".join(linux_bootstrap.ENTRY_POINT), builder.LINUX_ENTRY_POINT
        )
        with self.assertRaisesRegex(BootstrapError, "systemd unit"):
            linux_bootstrap.unit_argument("/opt/nvx bootstrap/config.json")


class FakeIptables:
    """Model the OUTPUT chain, fail on request, and count every change
    that leaves the runner account unprotected."""

    KINDS = {
        "-d 169.254.169.254/32 -m owner --uid-owner 1001 -j REJECT": "old",
        "-d 169.254.169.254/32 -m owner --uid-owner 1001 -m comment --comment "
        "nvx-runner-metadata-pending -j REJECT": "pending",
        "-d 169.254.169.254/32 -m owner --uid-owner 1001 -m comment --comment "
        "nvx-runner-metadata -j REJECT": "stable",
    }

    def __init__(
        self, rules: list[str], fail_insert: int, fail_probe: str, fail_delete: bool
    ) -> None:
        self.rules = rules
        self.fail_insert = fail_insert
        self.fail_probe = fail_probe
        self.fail_delete = fail_delete
        self.insertions = 0
        self.violations = 0
        self.lock_checks: list[bool] = []
        self.lock_path: Path | None = None

    def protected(self) -> bool:
        for item in self.rules:
            if item in ("old", "pending", "stable"):
                return True
            if item == "ACCEPT":
                return False
        return False

    def __call__(self, command: list[str], stdin: bytes | None) -> Completed:
        if self.lock_path is not None:
            self.lock_checks.append(lock_is_held(self.lock_path))
        if command[:3] != ["iptables", "-w", "10"]:
            return status(2)
        operation, chain, *rest = command[3:]
        if chain != "OUTPUT":
            return status(2)
        if operation == "-I":
            if rest[0] != "1":
                return status(2)
            rest = rest[1:]
        kind = self.KINDS.get(" ".join(rest))
        if kind is None:
            return status(2, stderr="unexpected firewall match")
        if operation == "-C":
            if (self.fail_probe, self.insertions, kind) in (
                ("pending", 1, "pending"),
                ("stable", 2, "stable"),
            ):
                return status(2)
            return ok() if kind in self.rules else status(1)
        if operation == "-I":
            self.insertions += 1
            if self.insertions == self.fail_insert:
                return status(1, stderr="mock firewall insertion failed")
            self.rules.insert(0, kind)
        elif operation == "-D":
            if self.fail_delete or kind not in self.rules:
                return status(1, stderr="mock firewall deletion failed")
            self.rules.remove(kind)
        else:
            return status(2)
        if not self.protected():
            self.violations += 1
        return ok()


def lock_is_held(path: Path) -> bool:
    if sys.platform == "win32":
        return False
    import fcntl

    descriptor = os.open(path, os.O_RDONLY)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        return True
    finally:
        os.close(descriptor)
    return False


class LinuxMetadataFirewallTests(unittest.TestCase):
    def check_block(
        self, initial: str, insertion: int, probe: str, deletion: bool, succeeds: bool
    ) -> None:
        fake = FakeIptables(initial.split(), insertion, probe, deletion)
        with patch.object(common, "run_command", Commands(fake)):
            if succeeds:
                linux_firewall.block_metadata(1001)
            else:
                with self.assertRaises(BootstrapError):
                    linux_firewall.block_metadata(1001)
        state = fake.rules
        self.assertEqual(fake.violations, 0, state)
        self.assertEqual(state.count("ACCEPT"), initial.split().count("ACCEPT"))
        if succeeds:
            self.assertEqual(state[0], "stable")
            self.assertEqual(state.count("stable"), 1)
            self.assertNotIn("old", state)
            self.assertNotIn("pending", state)
        else:
            if initial:
                self.assertTrue({"old", "pending", "stable"}.intersection(state))
            if insertion == 1:
                self.assertEqual(state, initial.split())

    def test_metadata_firewall_is_idempotent_and_fail_closed(self):
        for initial, insertion, probe, deletion, succeeds in (
            ("", 0, "none", False, True),
            ("old", 0, "none", False, True),
            ("stable", 0, "none", False, True),
            ("pending", 0, "none", False, True),
            ("ACCEPT old stable pending old", 0, "none", False, True),
            ("", 1, "none", False, False),
            ("old", 1, "none", False, False),
            ("old", 2, "none", False, False),
            ("old", 0, "pending", False, False),
            ("old", 0, "stable", False, False),
            ("old", 0, "none", True, False),
        ):
            with self.subTest(
                initial=initial, insertion=insertion, probe=probe, deletion=deletion
            ):
                self.check_block(initial, insertion, probe, deletion, succeeds)

    @unittest.skipIf(sys.platform == "win32", "flock is Linux-only")
    def test_metadata_firewall_changes_only_under_the_update_lock(self):
        with tempfile.TemporaryDirectory() as temporary:
            lock = Path(temporary) / "metadata.lock"
            fake = FakeIptables(["old"], 0, "none", False)
            fake.lock_path = lock
            with patch.object(common, "run_command", Commands(fake)):
                linux_firewall.locked_block(1001, lock)
            self.assertTrue(fake.lock_checks)
            self.assertTrue(all(fake.lock_checks))
            self.assertEqual(lock.stat().st_mode & 0o777, 0o600)
            self.assertFalse(lock_is_held(lock))
            if sys.platform != "win32":
                import fcntl

                # A concurrent update waits for the lock and then gives up
                # without touching the firewall.
                descriptor = os.open(lock, os.O_WRONLY)
                fcntl.flock(descriptor, fcntl.LOCK_EX)
                try:
                    with (
                        patch.object(linux_firewall, "LOCK_TIMEOUT_SECONDS", 0.2),
                        patch.object(common, "run_command", Commands(unexpected)),
                        self.assertRaisesRegex(BootstrapError, "update lock"),
                    ):
                        linux_firewall.locked_block(1001, lock)
                finally:
                    os.close(descriptor)

    def test_unexpected_probe_status_stops_the_update(self):
        with patch.object(common, "run_command", Commands(lambda c, s: status(4))):
            with self.assertRaisesRegex(BootstrapError, r"exit=4"):
                linux_firewall.rule_exists(["OUTPUT", "-j", "REJECT"])


class LinuxDiskTests(unittest.TestCase):
    def test_failed_disk_change_logs_all_output_before_raising(self):
        commands = Commands(
            lambda command, stdin: status(
                17, stdout="partition output\n", stderr="filesystem error\n"
            )
        )
        output = io.StringIO()
        with (
            patch.object(common, "run_command", commands),
            contextlib.redirect_stdout(output),
            self.assertRaisesRegex(BootstrapError, "sfdisk failed with exit code 17"),
        ):
            linux_disk.logged(["sfdisk", "/dev/mock"], common.Log())
        self.assertIn("partition output\n", output.getvalue())
        self.assertIn("filesystem error", output.getvalue())
        self.assertEqual(commands.calls, [["sfdisk", "/dev/mock"]])

    def test_mount_point_and_lun_validation(self):
        for lun, mount_point in (
            (-1, "/mnt/data"),
            (64, "/mnt/data"),
            (0, "/etc/data"),
            (0, "/mnt/../etc"),
            (0, "/mnt/data/./nested"),
            (0, "/mnt//data"),
            (0, "/mnt/data/"),
            (0, "/mnt/da ta"),
        ):
            with self.subTest(lun=lun, mount_point=mount_point):
                with self.assertRaises(BootstrapError):
                    linux_disk.validate(lun, mount_point)
        linux_disk.validate(63, "/srv/runner-data")

    def check_partitioning_tool(
        self,
        manager: str,
        installed: bool,
        result: int,
        supplied: bool,
        expected: list[str] | str | None,
    ) -> None:
        state = {"installed": installed}

        def which(command: str) -> str | None:
            if command == "sfdisk":
                return "/usr/sbin/sfdisk" if state["installed"] else None
            return f"/usr/bin/{command}" if command == manager else None

        def install(command: list[str], stdin: bytes | None) -> Completed:
            if result == 0:
                state["installed"] = supplied
            return status(result)

        commands = Commands(install)
        with (
            patch.object(common, "which", side_effect=which),
            patch.object(common, "run_command", commands),
        ):
            if isinstance(expected, str):
                with self.assertRaisesRegex(BootstrapError, expected):
                    linux_disk.ensure_partitioning_tool()
            else:
                linux_disk.ensure_partitioning_tool()
        if installed or manager == "none":
            self.assertEqual(commands.calls, [])
            return
        self.assertEqual(len(commands.calls), 1)
        if isinstance(expected, list):
            self.assertEqual(commands.calls[0], expected)
        if manager == "apt-get":
            environment = commands.environments[0]
            assert environment is not None
            self.assertEqual(environment["DEBIAN_FRONTEND"], "noninteractive")

    def test_partitioning_tools_use_distribution_packages(self):
        for manager, installed, result, supplied, expected in (
            ("none", True, 0, True, None),
            ("apt-get", False, 0, True, ["apt-get", "install", "-y", "fdisk"]),
            ("tdnf", False, 0, True, ["tdnf", "install", "-y", "util-linux"]),
            ("dnf", False, 0, True, ["dnf", "install", "-y", "util-linux"]),
            ("none", False, 0, False, "no supported package manager"),
            ("apt-get", False, 1, False, "apt-get failed"),
            ("apt-get", False, 0, False, "unavailable after installing"),
        ):
            with self.subTest(manager=manager, installed=installed, result=result):
                self.check_partitioning_tool(
                    manager, installed, result, supplied, expected
                )

    def check_initialization_guard(
        self, contents: bytes | None, size: str, marker: str, succeeds: bool
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            disk = Path(temporary) / "disk"
            if contents is not None:
                disk.write_bytes(contents)

            def handle(command: list[str], stdin: bytes | None) -> Completed:
                if command == ["blockdev", "--getsize64", disk.as_posix()]:
                    return status(1) if size == "error" else ok(size + "\n")
                if command == ["wipefs", "-n", disk.as_posix()]:
                    if marker == "probe-failure":
                        return status(1)
                    return ok("" if marker == "none" else "gpt signature\n")
                if command == ["sfdisk", "--dump", disk.as_posix()]:
                    if marker == "dump-failure":
                        return status(1)
                    label = (
                        "unrelated"
                        if marker == "other"
                        else linux_disk.INIT_TABLE_ID.upper()
                    )
                    dump = f"label: gpt\nlabel-id: {label}\ndevice: {disk}\n\n"
                    if marker == "partitioned":
                        dump += f"{disk}1 : start=2048, size=2048, type=x\n"
                    return ok(dump)
                raise AssertionError(
                    f"unexpected destructive disk operation: {command}"
                )

            with (
                patch.object(common, "run_command", Commands(handle)),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                if succeeds:
                    linux_disk.require_blank_or_owned(disk, common.Log())
                else:
                    with self.assertRaises(BootstrapError):
                        linux_disk.require_blank_or_owned(disk, common.Log())

    def test_initialization_requires_verified_blankness_or_owned_marker(self):
        zeros = bytes(2 * 1024 * 1024 + 17)
        raw = zeros[:-1] + b"X"
        cases: tuple[tuple[str, bytes | None, str, str, bool], ...] = (
            ("zero", zeros, str(len(zeros)), "none", True),
            ("unrecognized-tail", raw, str(len(raw)), "none", False),
            ("owned-recovery", raw, str(len(raw)), "owned", True),
            ("unknown-marker", raw, str(len(raw)), "other", False),
            ("owned-with-partition", raw, str(len(raw)), "partitioned", False),
            ("dump-failure", raw, str(len(raw)), "dump-failure", False),
            ("probe-failure", zeros, str(len(zeros)), "probe-failure", False),
            ("short-read", zeros[:1024], str(len(zeros)), "none", False),
            ("read-error", None, str(len(zeros)), "none", False),
            ("empty-size", zeros, "0", "none", False),
            ("size-error", zeros, "error", "none", False),
        )
        for case, contents, size, marker, succeeds in cases:
            with self.subTest(case=case):
                self.check_initialization_guard(contents, size, marker, succeeds)

    def test_zero_scan_is_complete_bounded_and_fail_closed(self):
        class Observed(io.BytesIO):
            def __init__(
                self, data: bytes, chunk: int = 1 << 30, fail_at: int = -1
            ) -> None:
                super().__init__(data)
                self.chunk = chunk
                self.fail_at = fail_at
                self.largest = 0
                self.consumed = 0

            def read(self, size: int | None = -1, /) -> bytes:
                assert size is not None and size > 0
                self.largest = max(self.largest, size)
                if 0 <= self.fail_at <= self.tell():
                    raise OSError("mock disk read failure")
                data = super().read(min(size, self.chunk))
                self.consumed += len(data)
                return data

        data = bytes(2 * 1024 * 1024 + 17)
        for chunk in (1 << 30, 4093):
            stream = Observed(data, chunk)
            self.assertTrue(common.zero_filled(stream, len(data)))
            self.assertEqual(stream.consumed, len(data))
            self.assertLessEqual(stream.largest, common.ZERO_SCAN_CHUNK_BYTES)
        for position in (0, 1024 * 1024 + 5, len(data) - 1):
            dirty = bytearray(data)
            dirty[position] = 1
            self.assertFalse(common.zero_filled(Observed(bytes(dirty)), len(data)))
        with self.assertRaisesRegex(BootstrapError, "ended before"):
            common.zero_filled(Observed(data), len(data) + 1)
        with self.assertRaisesRegex(OSError, "mock disk read failure"):
            common.zero_filled(Observed(data, fail_at=1024 * 1024), len(data))
        for length in (0, -1):
            with self.assertRaisesRegex(BootstrapError, "positive device size"):
                common.zero_filled(Observed(data), length)

    def check_mount(
        self, mounted: bool, device: str, target: str, succeeds: bool
    ) -> None:
        selected = "/dev/mock-selected"
        state = {"mounted": mounted, "mounts": 0}
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            mount_point = (root / "mount").as_posix()
            fstab = root / "fstab"
            original = (
                "# comment\n"
                f"UUID=original {mount_point} ext4 defaults 0 2\n"
                "UUID=other /srv/other ext4 defaults 0 2\n"
            )
            fstab.write_text(original, encoding="utf-8")

            def handle(command: list[str], stdin: bytes | None) -> Completed:
                if command == ["mountpoint", "-q", mount_point]:
                    return ok() if state["mounted"] else status(32)
                if command == ["findmnt", "-rn", "-S", selected]:
                    return status(1)
                if command == [
                    "findmnt",
                    "-rn",
                    "-o",
                    "MAJ:MIN",
                    "--target",
                    mount_point,
                ]:
                    return status(1) if device == "query-failure" else ok(device + "\n")
                if command == ["lsblk", "-dno", "MAJ:MIN", selected]:
                    return ok(f" {target}\n")
                if command == ["blkid", "-s", "PARTUUID", "-o", "value", selected]:
                    return ok("selected-part\n")
                if command in (
                    ["e2fsck", "-n", selected],
                    ["systemctl", "daemon-reload"],
                ):
                    return ok()
                if command == ["mount", mount_point]:
                    state["mounted"] = True
                    state["mounts"] += 1
                    return ok()
                raise AssertionError(f"unexpected command: {command}")

            paths = linux_disk.DiskPaths(
                fsck_log=root / "fsck.log", fstab=fstab, owner=None
            )
            with (
                patch.object(common, "run_command", Commands(handle)),
                patch.object(common, "which", return_value=None),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                if succeeds:
                    linux_disk.mount(mount_point, Path(selected), paths, common.Log())
                else:
                    with self.assertRaises(BootstrapError):
                        linux_disk.mount(
                            mount_point, Path(selected), paths, common.Log()
                        )
            if not succeeds and mounted:
                self.assertEqual(fstab.read_text(encoding="utf-8"), original)
            if succeeds:
                self.assertEqual(
                    fstab.read_text(encoding="utf-8"),
                    "# comment\nUUID=other /srv/other ext4 defaults 0 2\n"
                    f"PARTUUID=selected-part {mount_point} ext4 "
                    f"{linux_disk.MOUNT_OPTIONS} 1 2\n",
                )
                self.assertEqual(state["mounts"], 0 if mounted else 1)

    def test_disk_checks_identity_before_fstab_and_after_mount(self):
        for mounted, device, target, succeeds in (
            (True, "8:1", "8:1", True),
            (True, "8:9", "8:1", False),
            (False, "8:1", "8:1", True),
            (False, "8:9", "8:1", False),
            (True, "query-failure", "8:1", False),
            (True, "8:1", "", False),
        ):
            with self.subTest(mounted=mounted, device=device, target=target):
                self.check_mount(mounted, device, target, succeeds)

    def test_fstab_is_rewritten_only_when_its_entry_differs(self):
        expected = f"PARTUUID=part /mnt/data ext4 {linux_disk.MOUNT_OPTIONS} 1 2"
        output = io.StringIO()
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(common, "which", return_value=None),
            contextlib.redirect_stdout(output),
        ):
            fstab = Path(temporary) / "fstab"
            paths = linux_disk.DiskPaths(fstab=fstab, owner=None)
            linux_disk.update_fstab("PARTUUID=part", "/mnt/data", paths, common.Log())
            self.assertEqual(fstab.read_text(encoding="utf-8"), expected + "\n")
            unchanged = f"/dev/sda1 / ext4 defaults 0 1\n{expected}"
            fstab.write_text(unchanged, encoding="utf-8")
            linux_disk.update_fstab("PARTUUID=part", "/mnt/data", paths, common.Log())
            self.assertEqual(fstab.read_text(encoding="utf-8"), unchanged)
            fstab.write_text(f"{expected}\n{expected}\n", encoding="utf-8")
            linux_disk.update_fstab("PARTUUID=part", "/mnt/data", paths, common.Log())
            self.assertEqual(fstab.read_text(encoding="utf-8"), expected + "\n")
            self.assertEqual(
                [path.name for path in Path(temporary).iterdir()], ["fstab"]
            )
        self.assertIn("already contains exactly", output.getvalue())

    def check_target_selection(
        self,
        name: str,
        whole: str,
        partitions: list[str],
        kind: str,
        filesystem: str,
        error: str | None,
        formatting: list[str],
    ) -> None:
        disk = Path("/dev/mock")
        partition = Path("/dev/mock1")
        state = {"filesystem": filesystem}
        formats: list[list[str]] = []

        def handle(command: list[str], stdin: bytes | None) -> Completed:
            if command == ["blkid", "-p", "-s", "TYPE", "-o", "value", disk.as_posix()]:
                return ok(whole + "\n") if whole else status(2)
            if command == ["lsblk", "-lnpo", "NAME,TYPE", disk.as_posix()]:
                listed = "".join(f"{item} part\n" for item in partitions)
                return ok(f"{disk} disk\n{listed}")
            if command == ["lsblk", "-dno", "PARTTYPE", partition.as_posix()]:
                return ok(kind.upper() + "\n")
            if command == [
                "blkid",
                "-p",
                "-s",
                "TYPE",
                "-o",
                "value",
                partition.as_posix(),
            ]:
                found = state["filesystem"]
                return ok(found + "\n") if found else status(2)
            if command == ["wipefs", "-n", partition.as_posix()]:
                return ok("signature\n" if name == "signed-blank" else "")
            if command[0] == "mkfs.ext4":
                formats.append(command[:-1])
                self.assertEqual(command[-1], partition.as_posix())
                state["filesystem"] = "ext4"
                return ok()
            if command in (
                ["e2fsck", "-fn", partition.as_posix()],
                [
                    "sfdisk",
                    "--part-type",
                    disk.as_posix(),
                    "1",
                    linux_disk.DATA_PART_TYPE,
                ],
                ["udevadm", "settle"],
            ):
                return ok()
            raise AssertionError(f"unexpected command: {command}")

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(common, "run_command", Commands(handle)),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            sysfs = Path(temporary) / "sys"
            (sysfs / partition.name).mkdir(parents=True)
            (sysfs / partition.name / "partition").write_text("1\n", encoding="ascii")
            paths = linux_disk.DiskPaths(
                fsck_log=Path(temporary) / "fsck.log", sysfs_block=sysfs
            )
            if error is None:
                target = linux_disk.select_target(disk, paths, common.Log())
                self.assertEqual(target, disk if whole else partition)
            else:
                with self.assertRaisesRegex(BootstrapError, error):
                    linux_disk.select_target(disk, paths, common.Log())
        self.assertEqual(formats, [formatting] if formatting else [])

    def test_target_selection_finishes_only_owned_initialization(self):
        data = linux_disk.DATA_PART_TYPE
        marked = linux_disk.INIT_PART_TYPE
        one = ["/dev/mock1"]
        cases: tuple[
            tuple[str, str, list[str], str, str, str | None, list[str]], ...
        ] = (
            # Case, whole-disk filesystem, partitions, partition type,
            # partition filesystem, error, formatting command.
            ("whole-disk-ext4", "ext4", [], "", "", None, []),
            ("whole-disk-xfs", "xfs", [], "", "", "not ext4", []),
            ("ambiguous", "ext4", one, "", "", "refusing to guess", []),
            ("existing-data", "", one, data, "ext4", None, []),
            ("foreign-ntfs", "", one, "ebd0a0a2", "ntfs", "not ext4", []),
            ("owned-blank", "", one, marked, "", None, ["mkfs.ext4"]),
            ("owned-formatted", "", one, marked, "ext4", None, ["mkfs.ext4", "-F"]),
            ("unowned-blank", "", one, data, "", "lacks the runner", []),
            ("signed-blank", "", one, marked, "", "contains a signature", []),
            ("two", "", [*one, "/dev/mock2"], "", "", "2 partitions", []),
        )
        for case in cases:
            with self.subTest(case=case[0]):
                self.check_target_selection(*case)

    def test_blank_disk_initialization_creates_one_marked_partition(self):
        disk = Path("/dev/mock")
        partition = Path("/dev/mock1")
        created: list[bytes | None] = []
        types: list[str] = []

        def handle(command: list[str], stdin: bytes | None) -> Completed:
            if command == ["sfdisk", disk.as_posix()]:
                created.append(stdin)
                return ok()
            if command == ["lsblk", "-lnpo", "NAME,TYPE", disk.as_posix()]:
                return ok(f"{disk} disk\n{partition} part\n" if created else "")
            if command == ["wipefs", "-n", partition.as_posix()]:
                return ok()
            if command[:2] == ["sfdisk", "--part-type"]:
                types.append(command[-1])
                return ok()
            if command in (
                ["udevadm", "settle"],
                ["mkfs.ext4", partition.as_posix()],
                ["e2fsck", "-fn", partition.as_posix()],
            ):
                return ok()
            raise AssertionError(f"unexpected command: {command}")

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(common, "run_command", Commands(handle)),
            patch.object(linux_disk, "require_blank_or_owned") as guard,
            patch.object(linux_disk, "_is_block_device", return_value=True),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            sysfs = Path(temporary) / "sys"
            (sysfs / partition.name).mkdir(parents=True)
            (sysfs / partition.name / "partition").write_text("1\n", encoding="ascii")
            paths = linux_disk.DiskPaths(
                fsck_log=Path(temporary) / "fsck.log", sysfs_block=sysfs
            )
            self.assertEqual(
                linux_disk.initialize_blank_disk(disk, paths, common.Log()), partition
            )
        guard.assert_called_once()
        self.assertEqual(
            created,
            [
                (
                    f"label: gpt\nlabel-id: {linux_disk.INIT_TABLE_ID}\n"
                    f",,{linux_disk.INIT_PART_TYPE}\n"
                ).encode()
            ],
        )
        self.assertEqual(types, [linux_disk.DATA_PART_TYPE])

    def test_missing_disk_fails_after_discovery_timeout(self):
        sleeps: list[float] = []
        paths = linux_disk.DiskPaths(lun_links=("/nonexistent/nvx-test-lun{lun}",))
        with patch.object(common, "sleep", side_effect=sleeps.append):
            with self.assertRaisesRegex(BootstrapError, "LUN 3 was not found"):
                linux_disk.find_disk(3, paths)
        self.assertEqual(len(sleeps), linux_disk.DISCOVERY_SECONDS - 1)

    def test_filesystem_probe_failures_fail_closed(self):
        for code, expected in ((0, "ext4"), (2, ""), (4, None), (8, None)):
            with self.subTest(code=code):
                commands = Commands(lambda c, s, code=code: status(code, "ext4\n"))
                with patch.object(common, "run_command", commands):
                    if expected is None:
                        with self.assertRaisesRegex(
                            BootstrapError, "refusing to guess"
                        ):
                            linux_disk.filesystem_type(Path("/dev/mock"))
                    else:
                        self.assertEqual(
                            linux_disk.filesystem_type(Path("/dev/mock")), expected
                        )


class MshvTests(unittest.TestCase):
    def check_dom0_default(
        self,
        config: str,
        saved_entry: str,
        next_entry: str,
        result: int,
        persist: bool,
        succeeds: bool,
        calls: int,
    ) -> None:
        state = {"saved": saved_entry}
        set_calls: list[str] = []

        def handle(command: list[str], stdin: bytes | None) -> Completed:
            if command == ["grub2-set-default", "Dom0"]:
                set_calls.append(command[1])
                if result == 0 and persist:
                    state["saved"] = "Dom0"
                return status(result)
            if command == ["grub2-editenv", "-", "list"]:
                return ok(f"saved_entry={state['saved']}\nnext_entry={next_entry}\n")
            raise AssertionError(f"unexpected command: {command}")

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(common, "run_command", Commands(handle)),
        ):
            grub = Path(temporary) / "grub.cfg"
            grub.write_text(config, encoding="utf-8")
            paths = linux_mshv.MshvPaths(grub_cfg=grub)
            if succeeds:
                linux_mshv.verify_dom0_default(paths)
            else:
                with self.assertRaises(BootstrapError):
                    linux_mshv.verify_dom0_default(paths)
        self.assertEqual(len(set_calls), calls)

    def test_dom0_default_requires_active_selection(self):
        saved = 'load_env\nset default="${saved_entry}"\n'
        conditional = (
            'load_env\nif [ "${next_entry}" ]; then\n'
            '  set default="${next_entry}"\nelse\n'
            '  set default="${saved_entry}"\nfi\n'
        )
        another = 'load_env -f /another/grubenv\nset default="${saved_entry}"\n'
        for case in (
            ('set default="Dom0"\n', "Linux", "", 0, True, True, 0),
            ('set default="0"\n', "Dom0", "", 0, True, False, 0),
            ('set default="Dom00"\n', "Dom0", "", 0, True, False, 0),
            (saved, "Linux", "", 0, True, True, 1),
            (conditional, "Linux", "", 0, True, True, 1),
            (conditional, "Dom0", "Linux", 0, True, False, 1),
            ('set default="${saved_entry}"\n', "Dom0", "", 0, True, False, 0),
            (another, "Dom0", "", 0, True, False, 0),
            (saved, "Linux", "", 1, True, False, 1),
            (saved, "Dom00", "", 0, False, False, 1),
            ('set default="Dom0"\nset default="0"\n', "Dom0", "", 0, True, False, 0),
            ("set default='Dom0' # comment\n", "Linux", "", 0, True, True, 0),
        ):
            with self.subTest(config=case[0], next_entry=case[2], result=case[3]):
                self.check_dom0_default(*case)

    def check_installation(
        self,
        succeeds_after: int,
        installed: list[str],
        query: int,
        error: str | None,
    ) -> None:
        packages = linux_mshv.pinned_packages()
        attempts: list[int] = []
        sleeps: list[float] = []

        def handle(command: list[str], stdin: bytes | None) -> Completed:
            if command == ["dnf", "install", "-y", *packages]:
                attempts.append(1)
                return status(0 if len(attempts) >= succeeds_after else 1)
            if command == ["rpm", "-qa"]:
                return ok("\n".join(installed) + "\n")
            if command == [
                "rpm",
                "-q",
                "--queryformat",
                "%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}\\n",
                *linux_mshv.MSHV_PINS,
            ]:
                return status(query, "\n".join(installed) + "\n")
            raise AssertionError(f"unexpected MSHV package request: {command}")

        output = io.StringIO()
        with (
            patch.object(common, "run_command", Commands(handle)),
            patch.object(common, "sleep", side_effect=sleeps.append),
            contextlib.redirect_stdout(output),
        ):
            if error is None:
                linux_mshv.install_pinned_stack(common.Log())
            else:
                with self.assertRaisesRegex(BootstrapError, error):
                    linux_mshv.install_pinned_stack(common.Log())
        self.assertEqual(len(attempts), min(succeeds_after, 3))
        self.assertEqual(sleeps, [30] * (min(succeeds_after, 3) - 1))
        lines = [line.split("] ", 1)[1] for line in output.getvalue().splitlines()]
        # The log names each pinned package before it installs them.
        start = lines.index("pinned MSHV stack:")
        self.assertEqual(lines[start + 1 : start + 5], [f"  {p}" for p in packages])
        self.assertEqual("installed the pinned MSHV stack" in lines, error is None)

    def test_failed_install_attempt_streams_to_both_logs(self):
        packages = linux_mshv.pinned_packages()
        attempts = 0

        def handle(command: list[str], stdin: bytes | None) -> Completed:
            nonlocal attempts
            if command == ["dnf", "install", "-y", *packages]:
                attempts += 1
                return status(
                    1,
                    stdout=f"attempt {attempts} output\n",
                    stderr=f"attempt {attempts} error\n",
                )
            raise AssertionError(f"unexpected MSHV package request: {command}")

        with tempfile.TemporaryDirectory() as temporary:
            install_log = Path(temporary) / "enable-mshv.log"
            output = io.StringIO()
            with (
                patch.object(common, "run_command", Commands(handle)),
                patch.object(common, "sleep"),
                contextlib.redirect_stdout(output),
                self.assertRaisesRegex(BootstrapError, "after 3 attempts"),
            ):
                linux_mshv.install_pinned_stack(common.Log(install_log))
            persisted = install_log.read_text(encoding="utf-8")
        for attempt in range(1, 4):
            for suffix in ("output", "error"):
                message = f"attempt {attempt} {suffix}"
                self.assertIn(message, output.getvalue())
                self.assertIn(message, persisted)

    def test_installation_installs_only_the_pinned_boot_components(self):
        source = Path(linux_mshv.__file__).read_text(encoding="utf-8")
        self.assertNotIn("cloud-hypervisor", source)
        self.assertNotIn("kata", source)
        self.assertEqual(
            list(linux_mshv.MSHV_PINS),
            ["mshv", "mshv-bootloader-lx", "kernel-mshv", "edk2-hvloader"],
        )
        packages = linux_mshv.pinned_packages()
        self.assertEqual(
            packages,
            [f"{name}-{pin}.x86_64" for name, pin in linux_mshv.MSHV_PINS.items()],
        )
        # rpm lists every installed release of a name, and reports a missing
        # name on standard output.
        newer_kernel = "kernel-mshv-6.18.40.mshv3-1.azl3.x86_64"
        newer_mshv = "mshv-26100.9500.2610010000.1-1.azl3.x86_64"
        missing = "package edk2-hvloader is not installed"
        not_pinned = "the installed MSHV stack is not the pinned one"
        for case in (
            (1, packages, 0, None),
            (3, packages, 0, None),
            (4, packages, 0, "could not install MSHV boot components"),
            (1, [*packages, newer_kernel], 0, not_pinned),
            (1, [newer_mshv, *packages[1:]], 0, not_pinned),
            (1, [*packages[:3], missing], 1, "a pinned MSHV package is not installed"),
        ):
            with self.subTest(succeeds_after=case[0], installed=case[1][-1]):
                self.check_installation(*case)

    def test_pins_install_the_expected_benchmark_host(self):
        # The pins install the host of the Linux/MSHV series' history, so a
        # pin moves only with a new expected host (#980).
        host = json.loads(
            (builder.REPO / "data" / "linux-mshv-virtual-machine-host.json").read_text(
                encoding="utf-8"
            )
        )
        pins = linux_mshv.MSHV_PINS
        self.assertEqual(pins["kernel-mshv"], host["os"]["release"])
        # The mshv package's version starts with the hypervisor's build and
        # service: mshv 26100.9444.2609032029.1 is hypervisor 10.0.26100.9444.
        _, _, build, service = host["hypervisor_build"].split(".")
        self.assertTrue(pins["mshv"].startswith(f"{build}.{service}."), pins["mshv"])
        self.assertEqual(pins["mshv-bootloader-lx"], pins["mshv"])

    def test_dom0_entry_must_chainload_the_hypervisor(self):
        entry = (
            'menuentry "Dom0" {\n'
            "  chainloader /HvLoader.efi MSHV_SCHEDULER_TYPE=ROOT\n"
            "  linux /vmlinuz-6.18.34.mshv3-1.azl3 root=/dev/sda2\n"
            "}\n"
        )
        paths = linux_mshv.MshvPaths()
        self.assertEqual(
            linux_mshv.validate_dom0_entry('menuentry "Linux" {\n}\n' + entry, paths),
            entry.splitlines(),
        )
        for broken, error in (
            (entry.replace("HvLoader.efi", "grubx64.efi"), "does not chainload"),
            (entry.replace("MSHV_SCHEDULER_TYPE=ROOT", ""), "root scheduler"),
            (entry.replace("mshv3", "azl3"), "mshv kernel"),
            (entry.replace('"Dom0"', '"Linux"'), "no 'Dom0' menuentry"),
        ):
            with self.subTest(error=error):
                with self.assertRaisesRegex(BootstrapError, error):
                    linux_mshv.validate_dom0_entry(broken, paths)

    def check_verification(self, present: bool, rpm: bool = True) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            device = Path(temporary) / "mshv"
            if present:
                device.touch()

            def handle(command: list[str], stdin: bytes | None) -> Completed:
                if command == ["ls", "-l", device.as_posix()]:
                    return ok(f"crw------- 1 root root 10, 58 {device}\n")
                if command == ["dmesg"]:
                    return ok("[    0.1] mshv: running as root partition\n")
                if command == ["rpm", "-qa"] and rpm:
                    return ok("kernel-mshv-1\nbash-5\nedk2-hvloader-1\n")
                raise AssertionError(f"unexpected command: {command}")

            output = io.StringIO()
            with (
                patch.object(common, "run_command", Commands(handle)),
                patch.object(
                    common, "which", return_value="/usr/bin/rpm" if rpm else None
                ),
                patch.object(
                    linux_mshv.platform, "release", return_value="6.6.148-test.mshv"
                ),
                contextlib.redirect_stdout(output),
            ):
                passed = linux_mshv.verify(linux_mshv.MshvPaths(device=device))
        self.assertEqual(passed, present)
        report = output.getvalue()
        self.assertIn("RESULT: PASS" if present else "RESULT: FAIL", report)
        # Without RPM, as on Ubuntu, only the package listing is skipped.
        self.assertEqual("edk2-hvloader-1\nkernel-mshv-1\n" in report, rpm)
        self.assertNotIn("bash-5", report)

    def test_verification_reports_the_root_partition(self):
        for present, rpm in ((False, True), (True, True), (True, False)):
            with self.subTest(device=present, rpm=rpm):
                self.check_verification(present, rpm)


class LinuxCommandTests(unittest.TestCase):
    def test_commands_dispatch_to_their_controllers(self):
        with patch.object(linux_bootstrap, "bootstrap_main", return_value=0) as main:
            self.assertEqual(cli.main(["linux-bootstrap"]), 0)
        main.assert_called_once_with(cli.LINUX_CONFIG)
        with patch.object(linux_bootstrap, "refresh_main", return_value=0) as main:
            self.assertEqual(
                cli.main(["linux-refresh-metadata", "--config", "/tmp/nvx.json"]), 0
            )
        main.assert_called_once_with(Path("/tmp/nvx.json"))
        with patch.object(linux_firewall, "main", return_value=0) as main:
            self.assertEqual(cli.main(["linux-block-metadata"]), 0)
        main.assert_called_once_with()
        with patch.object(linux_mshv, "verify", side_effect=(True, False)):
            self.assertEqual(cli.main(["linux-verify-mshv"]), 0)
            self.assertEqual(cli.main(["linux-verify-mshv"]), 1)

    def test_failures_report_errors_without_tracebacks(self):
        errors = io.StringIO()
        with (
            patch.object(
                linux_firewall, "main", side_effect=BootstrapError("blocked badly")
            ),
            contextlib.redirect_stderr(errors),
        ):
            self.assertEqual(cli.main(["linux-block-metadata"]), 1)
        self.assertEqual(errors.getvalue(), "ERROR: blocked badly\n")


if __name__ == "__main__":
    unittest.main()

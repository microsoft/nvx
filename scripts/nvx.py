#!/usr/bin/env python3
"""Build, run, benchmark, and package the OpenVMM/NVX distribution."""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import cast

from nvx_tools import sandbox_lifecycle
from nvx_tools.aci_edge_sandboxes_tests import (
    configure_parser as configure_aci_edge_sandboxes_test_parser,
)
from nvx_tools.adversarial import configure_parser as configure_adversarial_parser
from nvx_tools.benchmark import configure_parser as configure_benchmark_parser
from nvx_tools.benchmark import run as run_benchmark
from nvx_tools.build import (
    build_all,
    build_distro_layer,
    build_docker_initramfs,
    build_guest,
    build_initramfs,
    build_kernel,
    build_openvmm,
    materialize_kernel_provenance_inputs,
    record_openvmm_provenance,
    verify_guest_determinism,
)
from nvx_tools.build_config import (
    BuildConfig,
    DistroLayerBuildConfig,
    DockerBuildConfig,
    KernelBuildConfig,
    OpenVmmBuildConfig,
)
from nvx_tools.build_constants import (
    AlpineBuildConstants,
    BuildConstants,
    InitramfsBuildConstants,
    KernelBuildConstants,
    UbuntuBuildConstants,
)
from nvx_tools.ci import (
    OPENVMM_TEST_BACKENDS,
    REQUIRED_CI_RESULT_ENVIRONMENTS,
    CiChanges,
    classify_ci_changes,
    describe_ci_changes,
    required_ci_failures,
    run_openvmm_tests,
    run_openvmm_unit_tests,
    setup_cross_os_cache,
)
from nvx_tools.collect_alpine_sources import (
    configure_parser as configure_alpine_sources_parser,
)
from nvx_tools.collect_ubuntu_sources import (
    configure_parser as configure_ubuntu_sources_parser,
)
from nvx_tools.common import (
    ScriptError,
    artifact_path,
    openvmm_binary_path,
    require_file,
    sha256_file,
)
from nvx_tools.control_session import encode_exec_environment
from nvx_tools.create_linux_source_archive import (
    configure_parser as configure_linux_source_archive_parser,
)
from nvx_tools.doctor import configure_parser as configure_doctor_parser
from nvx_tools.doctor import host_cpu_signature
from nvx_tools.egress_policy import compile_policy_file
from nvx_tools.guests import GUEST_NAMES, guest_descriptor
from nvx_tools.microvm_tests import configure_parser as configure_microvm_test_parser
from nvx_tools.performance import configure_parser as configure_performance_parser
from nvx_tools.release import (
    collect_release_sources,
    create_release_archive,
    download_checkout_release,
    package_release,
    verify_source_tree,
)
from nvx_tools.sandbox import (
    MAX_MOUNTS,
    MOUNT_OWNERS,
    MOUNT_POLICY_OPTIONS,
    SandboxLaunch,
    SandboxLayer,
    SandboxMount,
    parse_workload_identity,
    require_mount_owner_supported,
)
from nvx_tools.time_abi import (
    benchmark_cpu_profile_refusal,
    host_cpu_profile_guidance,
    openvmm_cpu_generations,
)

DEFAULT_RELEASE_REPOSITORY = "microsoft/nvx"
HYPERVISORS = ("auto", "whp", "kvm", "mshv")
NETWORK_PROFILES = ("portable",)
NETWORK_EGRESS_RULE_METAVAR = "CIDR[:PROTOCOL[:PORT[-PORT]]]"
NETWORK_EGRESS_RULE_HELP = (
    "{action} guest egress to an IPv4 CIDR, optionally only for PROTOCOL tcp, "
    "udp, or icmp, and for tcp or udp only on PORT or on the inclusive range "
    "PORT-PORT; repeat to add rules"
)
MAX_ENVIRONMENT_FILE_BYTES = 1024 * 1024
SYSTEMD_ENTRYPOINTS = frozenset(("/usr/lib/systemd/systemd", "/lib/systemd/systemd"))


def _mount_policy_destination(option: str) -> str:
    """Return the argparse destination of a share access-policy option."""
    return option.removeprefix("--").replace("-", "_")


class _MountPolicyPathAction(argparse.Action):
    """Record each --mount-deny, --mount-allow, or --mount-write with the index
    of the --mount before it."""

    def __call__(
        self,
        parser: argparse.ArgumentParser,
        namespace: argparse.Namespace,
        values: str | Sequence[object] | None,
        option_string: str | None = None,
    ) -> None:
        mounts = cast(list[str], getattr(namespace, "mount", None) or [])
        paths = list(
            cast(list[tuple[int, str]], getattr(namespace, self.dest, None) or [])
        )
        paths.append((len(mounts) - 1, cast(str, values)))
        setattr(namespace, self.dest, paths)


def _sandbox_mounts(args: argparse.Namespace) -> tuple[SandboxMount, ...]:
    """Attribute each access-policy path to its share and parse the --mount
    options.

    With one --mount, every --mount-deny, --mount-allow, and --mount-write
    names a path in it. With several, each names a path in the --mount that
    precedes it.
    """
    mounts = cast(list[str], args.mount)
    attributed: dict[str, list[list[str]]] = {}
    for kind, option in MOUNT_POLICY_OPTIONS.items():
        paths: list[list[str]] = [[] for _ in mounts]
        destination = _mount_policy_destination(option)
        for index, path in cast(list[tuple[int, str]], getattr(args, destination)):
            if len(mounts) == 1:
                index = 0
            elif index < 0:
                raise ScriptError(
                    f"with several --mount options, each {option} must follow the "
                    "--mount whose host directory it names"
                )
            paths[index].append(path)
        attributed[kind] = paths
    owner = args.mount_owner or "vmm"
    return tuple(
        SandboxMount.parse(
            value,
            tuple(attributed["denied"][index]),
            owner,
            allowed_paths=tuple(attributed["allowed"][index]),
            writable_paths=tuple(attributed["writable"][index]),
        )
        for index, value in enumerate(mounts)
    )


def _run(
    args: list[str | os.PathLike[str]], *, cwd: Path = BuildConstants.REPO_ROOT
) -> None:
    command = [os.fspath(arg) for arg in args]
    print(f">> {shlex.join(command)}")
    subprocess.run(command, cwd=cwd, check=True)


def _validate_sandbox_systemd_policy(launch: SandboxLaunch) -> None:
    if launch.entrypoint in SYSTEMD_ENTRYPOINTS:
        raise ScriptError(
            "systemd entrypoints are unsupported by the sandbox security profile"
        )
    distro = next(
        (layer for layer in launch.layers if layer.role == "distro"),
        None,
    )
    if distro is None:
        return
    manifest = distro.path.with_name(
        f"{distro.path.name}{BuildConstants.DISTRO_MANIFEST_SUFFIX}"
    )
    if not manifest.exists():
        return
    try:
        document: object = json.loads(manifest.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as error:
        raise ScriptError(f"invalid sandbox distro manifest: {error}") from error
    if not isinstance(document, dict):
        raise ScriptError("invalid sandbox distro manifest: expected a JSON object")
    manifest_document = cast(dict[str, object], document)
    if (
        manifest_document.get("format") != 1
        or manifest_document.get("artifact") != distro.path.name
        or manifest_document.get("artifact_sha256") != sha256_file(distro.path)
    ):
        raise ScriptError("sandbox distro manifest does not match its artifact")
    raw_packages = manifest_document.get("packages")
    if not isinstance(raw_packages, list):
        raise ScriptError("sandbox distro manifest has invalid package metadata")
    packages: list[dict[str, object]] = []
    for raw_package in cast(list[object], raw_packages):
        if not isinstance(raw_package, dict):
            raise ScriptError("sandbox distro manifest has invalid package metadata")
        package = cast(dict[str, object], raw_package)
        if not isinstance(package.get("name"), str):
            raise ScriptError("sandbox distro manifest has invalid package metadata")
        packages.append(package)
    if any(package["name"] == "systemd" for package in packages):
        raise ScriptError(
            "systemd images are unsupported by the sandbox security profile"
        )


def command_init(_: argparse.Namespace) -> None:
    _run(["git", "submodule", "update", "--init", "--recursive"])


def _openvmm_build_config(args: argparse.Namespace) -> OpenVmmBuildConfig:
    return OpenVmmBuildConfig(
        skip_restore=getattr(args, "skip_restore", False),
        backend=getattr(args, "backend", None),
    )


def _build_config(args: argparse.Namespace) -> BuildConfig:
    return BuildConfig(
        guest=getattr(args, "guest", InitramfsBuildConstants.DEFAULT_GUEST),
        native_guest=getattr(args, "native", False),
        debug_kernel=getattr(args, "debug_kernel", False),
        openvmm=_openvmm_build_config(args),
    )


def command_build_guest(args: argparse.Namespace) -> None:
    build_guest(_build_config(args))


def command_build_kernel(args: argparse.Namespace) -> None:
    build_kernel(
        KernelBuildConfig.debug_variant()
        if getattr(args, "debug", False)
        else KernelBuildConfig()
    )


def command_build_initramfs(args: argparse.Namespace) -> None:
    if not guest_descriptor(args.guest).native_build_supported:
        build_docker_initramfs(DockerBuildConfig(), args.guest)
        return
    build_initramfs(BuildConfig.initramfs_config(args.guest))


def command_build_distro_layer(args: argparse.Namespace) -> None:
    build_distro_layer(
        DistroLayerBuildConfig(
            guest=args.guest,
            work=(
                BuildConstants.BUILD_DIR
                / InitramfsBuildConstants.DISTRO_WORK_DIRECTORY_TEMPLATE.format(
                    guest=args.guest
                )
            ),
            output=args.output,
            replace=args.replace,
        )
    )


def command_verify_guest_determinism(args: argparse.Namespace) -> None:
    verify_guest_determinism(args.work_dir, args.guest)


def command_build_openvmm(args: argparse.Namespace) -> None:
    build_openvmm(_openvmm_build_config(args))


def command_record_openvmm_provenance(_: argparse.Namespace) -> None:
    record_openvmm_provenance(OpenVmmBuildConfig())


def command_materialize_kernel_provenance_inputs(_: argparse.Namespace) -> None:
    materialize_kernel_provenance_inputs()


def command_setup_cross_os_cache(_: argparse.Namespace) -> None:
    setup_cross_os_cache()


def command_classify_ci_changes(args: argparse.Namespace) -> None:
    changed_files = args.changed_files.read_text(encoding="utf-8").splitlines()
    changes = classify_ci_changes(args.event_name, changed_files)
    if args.summary is not None:
        with args.summary.open("a", encoding="utf-8") as summary:
            summary.write(describe_ci_changes(args.event_name, changed_files, changes))
    for name, selected in changes.outputs().items():
        print(f"{name}={str(selected).lower()}")


def command_check_required_ci(args: argparse.Namespace) -> None:
    results = {
        job: os.environ.get(environment, "")
        for job, environment in REQUIRED_CI_RESULT_ENVIRONMENTS.items()
    }
    failures = required_ci_failures(
        args.event_name,
        same_repository=args.same_repository == "true",
        changes=CiChanges(
            run_tests=args.run_tests == "true",
            run_workloads=args.run_workloads == "true",
            run_openvmm_unit_tests=args.run_openvmm_unit_tests == "true",
            run_openvmm_vmm_tests=args.run_openvmm_vmm_tests == "true",
        ),
        results=results,
    )
    if failures:
        for failure in failures:
            print(f"::error::{failure}")
        raise ScriptError(f"{len(failures)} required CI job result(s) did not match")


def command_test_openvmm(args: argparse.Namespace) -> None:
    run_openvmm_tests(args.backend)


def command_test_openvmm_unit(_: argparse.Namespace) -> None:
    run_openvmm_unit_tests()


def command_benchmark(args: argparse.Namespace) -> int:
    """Run the benchmark coordinator, after refusing a host whose CPU no
    built-in CPU profile of the selected OpenVMM checkout serves before it
    builds or measures anything. Only the phase 2 suite, which boots no
    microVM, runs on any host. A KVM worker, which the Windows coordinator
    reinvokes inside WSL without its --openvmm-dir, runs the binaries that the
    coordinator staged after this check, so it skips the check."""
    if args.suite != "phase2" and not args._kvm_worker:
        try:
            generations = openvmm_cpu_generations(args.openvmm_dir)
        except ValueError as error:
            raise ScriptError(
                "benchmark cannot confirm that a built-in CPU profile serves this "
                f"host's CPU: {error}"
            ) from error
        hypervisor = "whp" if args.backend == "both" else args.backend
        refusal = benchmark_cpu_profile_refusal(
            host_cpu_signature(), "openvmm", hypervisor, generations
        )
        if refusal is not None:
            raise ScriptError(refusal)
    return run_benchmark(args)


def command_build(args: argparse.Namespace) -> None:
    build_all(_build_config(args))


def _hypervisor(selected: str) -> str:
    if selected != "auto":
        return selected
    return "whp" if os.name == "nt" else "kvm"


def _release_platform(hypervisor: str) -> str:
    selected = _hypervisor(hypervisor)
    if sys.platform == "win32":
        host = "windows"
        supported = ("whp",)
    elif sys.platform.startswith("linux"):
        host = "linux"
        supported = ("kvm", "mshv")
    else:
        raise ScriptError(f"release downloads are unsupported on {sys.platform}")
    if selected not in supported:
        raise ScriptError(f"{selected} is not supported on {host}")
    return f"{host}-{selected}"


def command_download(args: argparse.Namespace) -> None:
    download_checkout_release(args.repository, _release_platform(args.hypervisor))


def _format_command(command: list[str]) -> str:
    return subprocess.list2cmdline(command) if os.name == "nt" else shlex.join(command)


def _resolve_network_egress_rules(
    args: argparse.Namespace,
) -> tuple[tuple[str, ...], tuple[str, ...]]:
    policy_path = args.network_egress_policy_file
    explicit_allow = tuple(args.network_egress_allow)
    explicit_deny = tuple(args.network_egress_deny)
    if policy_path is None:
        return explicit_allow, explicit_deny
    if explicit_allow or explicit_deny:
        raise ScriptError(
            "--network-egress-policy-file cannot be combined with "
            "--network-egress-allow or --network-egress-deny"
        )
    if args.network_egress is None:
        raise ScriptError(
            "--network-egress is required with --network-egress-policy-file"
        )
    compiled = compile_policy_file(policy_path)
    return compiled.allow, compiled.deny


def _extend_network_arguments(
    command: list[str],
    args: argparse.Namespace,
    network_egress_allow: tuple[str, ...],
    network_egress_deny: tuple[str, ...],
) -> None:
    if args.net is not None:
        command.extend(["--net", args.net, "--network-profile", args.network_profile])
    if args.network_egress is not None:
        command.extend(["--network-egress", args.network_egress])
    if args.network_ingress is not None:
        command.extend(["--network-ingress", args.network_ingress])
    for rule in network_egress_allow:
        command.extend(["--network-egress-allow", rule])
    for rule in network_egress_deny:
        command.extend(["--network-egress-deny", rule])
    if args.host_loopback is not None:
        command.extend(["--host-loopback", args.host_loopback])
    if args.network_proxy is not None:
        command.extend(["--network-proxy", args.network_proxy])
    for forward in args.host_loopback_forward:
        command.extend(["--host-loopback-forward", forward])


def command_run(args: argparse.Namespace) -> None:
    if (args.net is None) != (args.network_profile is None):
        raise ScriptError("--net and --network-profile must be specified together")
    if args.restore_ready_path is not None and args.restore_snapshot is None:
        raise ScriptError("--restore-ready-path requires --restore-snapshot")
    if args.restore_processors is not None and args.restore_snapshot is None:
        raise ScriptError("--restore-processors requires --restore-snapshot")
    if args.restore_image_slots is not None and args.restore_snapshot is None:
        raise ScriptError("--restore-image-slots requires --restore-snapshot")
    if args.restore_memory_mib is not None and args.restore_snapshot is None:
        raise ScriptError("--restore-memory-mib requires --restore-snapshot")
    if args.memory_capacity_mib is not None and args.restore_snapshot is not None:
        raise ScriptError("--memory-capacity-mib is only valid for a fresh boot")
    descriptor = guest_descriptor(args.guest)
    if args.restore_snapshot is not None and descriptor.name != "alpine":
        raise ScriptError(
            "--guest is not accepted for restore; the snapshot already fixes the guest"
        )
    memory_mib = (
        descriptor.default_memory_mib if args.memory_mib is None else args.memory_mib
    )
    if args.memory_capacity_mib is not None and args.memory_capacity_mib < memory_mib:
        raise ScriptError("--memory-capacity-mib cannot be below --memory-mib")
    if args.restore_processors is not None:
        if args.restore_processors > args.processors:
            raise ScriptError(
                "--restore-processors cannot exceed --processors capacity"
            )
    network_egress_allow, network_egress_deny = _resolve_network_egress_rules(args)
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
    command = [
        str(executable),
        "--single-process",
        "--machine",
        args.machine,
        "--processors",
        str(args.processors),
        "--hypervisor",
        _hypervisor(args.hypervisor),
    ]
    if args.restore_snapshot is not None:
        command.extend(["--restore-snapshot", str(args.restore_snapshot)])
        if args.restore_processors is not None:
            command.extend(["--restore-processors", str(args.restore_processors)])
        if args.restore_image_slots is not None:
            command.extend(["--restore-image-slots", str(args.restore_image_slots)])
        if args.restore_memory_mib is not None:
            command.extend(["--restore-memory", f"{args.restore_memory_mib}M"])
        if args.restore_ready_path is not None:
            command.extend(["--restore-ready-path", str(args.restore_ready_path)])
    else:
        kernel = require_file(
            artifact_path(KernelBuildConstants.BINARY_NAME), "Linux direct kernel"
        )
        initrd = require_file(
            artifact_path(descriptor.initramfs_name),
            f"{descriptor.distribution} initramfs",
        )
        command.extend(
            [
                "--memory",
                f"{memory_mib}M",
                "--kernel",
                str(kernel),
                "--initrd",
                str(initrd),
            ]
        )
        if args.memory_capacity_mib is not None:
            command.extend(["--memory-capacity", f"{args.memory_capacity_mib}M"])
    if args.cpu_profile is not None:
        command.extend(["--cpu-profile", args.cpu_profile])
    if len(args.mount) > MAX_MOUNTS:
        raise ScriptError(f"--mount is accepted at most {MAX_MOUNTS} times")
    for mount in args.mount:
        if mount.count(",") not in (1, 2):
            raise ScriptError("--mount must be GUEST_TARGET,HOST_PATH[,ro|rw]")
        command.extend(["--mount", mount])
    if args.mount_owner is not None and not args.mount:
        raise ScriptError("--mount-owner requires --mount")
    for option in MOUNT_POLICY_OPTIONS.values():
        for path in getattr(args, _mount_policy_destination(option)):
            command.extend([option, str(path)])
    if args.mount_owner is not None:
        require_mount_owner_supported(args.mount_owner)
        command.extend(["--mount-owner", args.mount_owner])
    _extend_network_arguments(
        command,
        args,
        network_egress_allow,
        network_egress_deny,
    )
    if args.outcome_report is not None:
        command.extend(["--microvm-report", str(args.outcome_report)])
    if args.cmdline:
        command.extend(["--cmdline", args.cmdline])
    print(f">> {_format_command(command)}")
    if not args.dry_run:
        raise SystemExit(
            _run_openvmm(
                command,
                args.cpu_profile,
                args.restore_snapshot is None,
                _hypervisor(args.hypervisor),
            )
        )


def _run_openvmm(
    command: list[str], cpu_profile: str | None, cold_boot: bool, hypervisor: str
) -> int:
    """Run OpenVMM on the terminal, and explain the next steps when a cold boot
    fails on a host whose CPU no built-in CPU profile serves, with the
    ``hypervisor`` backend.

    OpenVMM keeps all three standard streams: it restores the terminal settings
    that its console changes only when its standard error is a terminal, and
    writes its log for a terminal there."""
    returncode = subprocess.run(command).returncode
    if returncode != 0 and cold_boot:
        guidance = host_cpu_profile_guidance(
            cpu_profile, host_cpu_signature(), _format_command(command[:1]), hypervisor
        )
        if guidance is not None:
            print(guidance, file=sys.stderr)
    return returncode


def command_sandbox(args: argparse.Namespace) -> None:
    operation = args.sandbox_operation
    exec_environment: tuple[str, ...] | None = None
    if args.dry_run and operation != "run":
        raise ScriptError("--dry-run is only valid for sandbox run")
    if args.environment and args.environment_file is not None:
        raise ScriptError("--environment and --environment-file are mutually exclusive")
    if operation != "exec" and (
        args.cwd is not None
        or args.environment
        or args.environment_file is not None
        or args.inherit_default_environment
    ):
        raise ScriptError(
            "managed execution options require the sandbox exec operation"
        )
    if operation == "exec":
        if args.environment_file is not None:
            try:
                with args.environment_file.open("rb") as stream:
                    data = stream.read(MAX_ENVIRONMENT_FILE_BYTES + 1)
                if len(data) > MAX_ENVIRONMENT_FILE_BYTES:
                    raise ScriptError(
                        "managed execution environment file exceeds "
                        f"{MAX_ENVIRONMENT_FILE_BYTES}-byte limit"
                    )
                value = json.loads(data.decode("utf-8"))
            except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
                raise ScriptError(
                    f"failed to read managed execution environment: "
                    f"{args.environment_file}"
                ) from error
            entries = cast(list[object], value) if isinstance(value, list) else None
            if entries is None or not all(isinstance(entry, str) for entry in entries):
                raise ScriptError(
                    "managed execution environment file must contain a JSON "
                    "array of KEY=VALUE strings"
                )
            exec_environment = tuple(cast(list[str], entries))
        elif args.environment:
            exec_environment = tuple(args.environment)
        if exec_environment is not None:
            try:
                encode_exec_environment(exec_environment)
            except (TypeError, ValueError) as error:
                raise ScriptError(str(error)) from error
    network_options = (
        args.net,
        args.network_profile,
        args.network_egress,
        args.network_ingress,
        args.network_egress_policy_file,
        args.host_loopback,
        args.network_proxy,
    )
    if operation not in ("run", "provision") and (
        any(value is not None for value in network_options)
        or args.network_egress_allow
        or args.network_egress_deny
        or args.host_loopback_forward
    ):
        raise ScriptError(
            "network policy options are only valid for sandbox run or provision"
        )
    if operation in ("run", "provision", "exec") and (
        args.entrypoint in SYSTEMD_ENTRYPOINTS
    ):
        raise ScriptError(
            "systemd entrypoints are unsupported by the sandbox security profile"
        )
    if args.outcome_report is not None and operation not in ("run", "exec"):
        raise ScriptError(
            "--outcome-report is only valid for one-shot run or managed exec"
        )
    for option in MOUNT_POLICY_OPTIONS.values():
        if getattr(args, _mount_policy_destination(option)) and not args.mount:
            raise ScriptError(f"{option} requires --mount")
    if args.mount_owner is not None and not args.mount:
        raise ScriptError("--mount-owner requires --mount")
    if args.mount and operation not in ("run", "provision"):
        raise ScriptError("--mount is only valid for sandbox run or provision")
    image_operation_options = (
        args.image_slot,
        args.image_path,
        args.image_identity,
    )
    if operation != "bind-image-slot" and any(
        value is not None for value in image_operation_options
    ):
        raise ScriptError(
            "--image-slot, --image-path, and --image-identity require "
            "sandbox bind-image-slot"
        )
    if args.image_slot_boot_count is not None and operation != "provision":
        raise ScriptError("--image-slot-boot-count is only valid for sandbox provision")
    if operation in ("run", "provision"):
        if (args.net is None) != (args.network_profile is None):
            raise ScriptError("--net and --network-profile must be specified together")
        if args.scratch is None:
            raise ScriptError(f"sandbox {operation} requires --scratch")
        if not args.layer and args.image_slot_boot_count is None:
            raise ScriptError(
                f"sandbox {operation} requires --layer unless image slots are declared"
            )
        network_egress_allow, network_egress_deny = _resolve_network_egress_rules(args)
        launch = SandboxLaunch(
            layers=tuple(args.layer),
            scratch=args.scratch,
            entrypoint=args.entrypoint,
            args=tuple(args.sandbox_arg),
            hostname=args.hostname,
            workload_identity=args.workload_user,
            memory_max=args.memory_max,
            pids_max=args.pids_max,
            mounts=_sandbox_mounts(args),
            image_slot_boot_count=args.image_slot_boot_count,
        ).validated()
        _validate_sandbox_systemd_policy(launch)
    else:
        launch = None
        network_egress_allow = ()
        network_egress_deny = ()

    if operation == "provision":
        if args.state_dir is None:
            raise ScriptError("sandbox provision requires --state-dir")
        assert launch is not None
        sandbox_lifecycle.provision(
            args.state_dir,
            launch,
            hypervisor=_hypervisor(args.hypervisor),
            memory_mib=args.memory_mib,
            net=args.net,
            network_profile=args.network_profile,
            network_egress=args.network_egress,
            network_ingress=args.network_ingress,
            network_egress_allow=network_egress_allow,
            network_egress_deny=network_egress_deny,
            host_loopback=args.host_loopback,
            network_proxy=args.network_proxy,
            host_loopback_forward=tuple(args.host_loopback_forward),
            cmdline=args.cmdline,
        )
        return
    if operation == "start":
        if args.state_dir is None:
            raise ScriptError("sandbox start requires --state-dir")
        sandbox_lifecycle.start(args.state_dir, args.timeout)
        return
    if operation == "exec":
        if args.state_dir is None:
            raise ScriptError("sandbox exec requires --state-dir")
        if args.outcome_report is not None:
            sandbox_lifecycle.validate_outcome_destination(args.outcome_report)
        result = sandbox_lifecycle.exec_workload(
            args.state_dir,
            (args.entrypoint, *args.sandbox_arg),
            timeout_ms=args.exec_timeout_ms,
            response_timeout=args.timeout,
            cwd=args.cwd,
            environment=exec_environment,
            inherit_default_environment=args.inherit_default_environment,
        )
        sys.stdout.buffer.write(result.stdout)
        sys.stdout.buffer.flush()
        sys.stderr.buffer.write(result.stderr)
        sys.stderr.buffer.flush()
        if args.outcome_report is not None:
            sandbox_lifecycle.write_exec_outcome(args.outcome_report, result)
        raise SystemExit(result.returncode)
    if operation == "query-image-slots":
        if args.state_dir is None:
            raise ScriptError("sandbox query-image-slots requires --state-dir")
        slots = sandbox_lifecycle.query_image_slots(args.state_dir, args.timeout)
        print(json.dumps(slots, indent=2, sort_keys=True))
        return
    if operation == "bind-image-slot":
        if args.state_dir is None:
            raise ScriptError("sandbox bind-image-slot requires --state-dir")
        if (
            args.image_slot is None
            or args.image_path is None
            or args.image_identity is None
        ):
            raise ScriptError(
                "sandbox bind-image-slot requires --image-slot, --image-path, "
                "and --image-identity"
            )
        sandbox_lifecycle.bind_image_slot(
            args.state_dir,
            args.image_slot,
            args.image_path,
            args.image_identity,
            args.timeout,
        )
        return
    if operation == "stop":
        if args.state_dir is None:
            raise ScriptError("sandbox stop requires --state-dir")
        sandbox_lifecycle.stop(args.state_dir, args.timeout)
        return
    if operation == "deprovision":
        if args.state_dir is None:
            raise ScriptError("sandbox deprovision requires --state-dir")
        sandbox_lifecycle.deprovision(args.state_dir)
        return
    if args.state_dir is not None:
        raise ScriptError(
            "one-shot sandbox execution rejects persistent --state-dir settings"
        )
    assert operation == "run"
    assert launch is not None
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
    kernel = require_file(
        artifact_path(KernelBuildConstants.BINARY_NAME), "Linux direct kernel"
    )
    initrd = require_file(
        artifact_path(AlpineBuildConstants.INITRAMFS_NAME),
        "initramfs",
    )
    command = [
        str(executable),
        *launch.openvmm_arguments(),
        "--microvm-lifecycle",
        "one-shot",
        "--single-process",
        "--hypervisor",
        _hypervisor(args.hypervisor),
        "--memory",
        f"{args.memory_mib}M",
        "--kernel",
        str(kernel),
        "--initrd",
        str(initrd),
        "--cmdline",
        launch.kernel_command_line(args.cmdline),
    ]
    _extend_network_arguments(
        command,
        args,
        network_egress_allow,
        network_egress_deny,
    )
    if args.outcome_report is not None:
        command.extend(["--microvm-report", str(args.outcome_report)])
    print(f">> {_format_command(command)}")
    if not args.dry_run:
        raise SystemExit(subprocess.run(command).returncode)


def command_collect_sources(_: argparse.Namespace) -> None:
    collect_release_sources(DockerBuildConfig())


def command_package(args: argparse.Namespace) -> None:
    package_release(
        version=args.version,
        destination=args.destination,
        include_source=args.include_source,
        force=args.force,
    )


def command_archive_release(args: argparse.Namespace) -> None:
    create_release_archive(args.source, args.destination)


def command_verify(_: argparse.Namespace) -> None:
    verify_source_tree()


def _add_guest_options(
    parser: argparse.ArgumentParser,
    *,
    allow_all: bool,
) -> None:
    choices = (*GUEST_NAMES, "all") if allow_all else GUEST_NAMES
    parser.add_argument(
        "--guest",
        choices=choices,
        default=InitramfsBuildConstants.DEFAULT_GUEST,
        help=(
            "guest userland to build "
            f"(default: {InitramfsBuildConstants.DEFAULT_GUEST})"
        ),
    )
    parser.add_argument(
        "--native",
        action="store_true",
        help="build directly on Linux instead of using Docker",
    )
    parser.add_argument(
        "--debug-kernel",
        action="store_true",
        help=(
            "also build the CI debug kernel (build/vmlinux-debug), which adds "
            "the soft-lockup, hung-task, and RCU stall diagnostics"
        ),
    )


def _add_openvmm_options(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--skip-restore", action="store_true")
    parser.add_argument(
        "--backend",
        choices=OPENVMM_TEST_BACKENDS,
        help=(
            "select build target: kvm=GNU, mshv=musl, whp=MSVC "
            "(default: native target for the host OS)"
        ),
    )


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    init = subparsers.add_parser("init", help="initialize the private submodule")
    init.set_defaults(handler=command_init)

    guest = subparsers.add_parser("build-guest", help="build Linux guest artifacts")
    _add_guest_options(guest, allow_all=True)
    guest.set_defaults(handler=command_build_guest)

    kernel = subparsers.add_parser(
        "build-kernel",
        help="fetch, patch, and build the pinned kernel natively on Linux",
    )
    kernel.add_argument(
        "--debug",
        action="store_true",
        help=(
            "build the CI debug variant (build/vmlinux-debug) by applying "
            "kernel/config-microvm-debug"
        ),
    )
    kernel.set_defaults(handler=command_build_kernel)

    initramfs = subparsers.add_parser(
        "build-initramfs",
        help=(
            "build a selected guest initramfs (natively on Linux, or via "
            "Docker for guests that require it)"
        ),
    )
    initramfs.add_argument(
        "--guest",
        choices=GUEST_NAMES,
        default=InitramfsBuildConstants.DEFAULT_GUEST,
        help=(
            "guest userland to build "
            f"(default: {InitramfsBuildConstants.DEFAULT_GUEST})"
        ),
    )
    initramfs.set_defaults(handler=command_build_initramfs)

    distro_layer = subparsers.add_parser(
        "build-distro-layer",
        help="build a deterministic EROFS distro layer natively on Linux",
    )
    distro_layer.add_argument("--guest", choices=GUEST_NAMES, required=True)
    distro_layer.add_argument(
        "--output",
        type=Path,
        default=artifact_path(UbuntuBuildConstants.DISTRO_NAME),
    )
    distro_layer.add_argument("--replace", action="store_true")
    distro_layer.set_defaults(handler=command_build_distro_layer)

    determinism = subparsers.add_parser(
        "verify-guest-determinism",
        help="build Ubuntu guest artifacts twice and compare them",
    )
    determinism.add_argument("--guest", choices=GUEST_NAMES, required=True)
    determinism.add_argument(
        "--work-dir",
        type=Path,
        default=(
            BuildConstants.BUILD_DIR
            / InitramfsBuildConstants.DETERMINISM_DIRECTORY_NAME
        ),
    )
    determinism.set_defaults(handler=command_verify_guest_determinism)

    openvmm = subparsers.add_parser("build-openvmm", help="build OpenVMM")
    _add_openvmm_options(openvmm)
    openvmm.set_defaults(handler=command_build_openvmm)

    provenance = subparsers.add_parser(
        "record-openvmm-provenance",
        help="bind an existing OpenVMM binary to the pinned source revision",
    )
    provenance.set_defaults(handler=command_record_openvmm_provenance)

    kernel_provenance = subparsers.add_parser(
        "materialize-kernel-provenance-inputs",
        help="write kernel provenance inputs from raw run-head blobs",
    )
    kernel_provenance.set_defaults(handler=command_materialize_kernel_provenance_inputs)

    cache = subparsers.add_parser(
        "setup-cross-os-cache",
        help="install GNU tar and zstd for GitHub Actions cross-OS caches",
    )
    cache.set_defaults(handler=command_setup_cross_os_cache)

    classify_changes = subparsers.add_parser(
        "classify-ci-changes",
        help="select the CI suites that a list of changed files runs",
    )
    classify_changes.add_argument(
        "--event-name",
        choices=("pull_request", "push"),
        required=True,
    )
    classify_changes.add_argument(
        "--changed-files",
        type=Path,
        required=True,
        help="file listing one changed repository path per line",
    )
    classify_changes.add_argument(
        "--summary",
        type=Path,
        help="Markdown file to append the reasons for each decision to",
    )
    classify_changes.set_defaults(handler=command_classify_ci_changes)

    required_ci = subparsers.add_parser(
        "check-required-ci",
        help="validate required GitHub Actions job results",
    )
    required_ci.add_argument(
        "--event-name",
        choices=("pull_request", "push"),
        required=True,
    )
    required_ci.add_argument(
        "--same-repository",
        choices=("false", "true"),
        required=True,
    )
    required_ci.add_argument("--run-tests", required=True)
    required_ci.add_argument("--run-workloads", required=True)
    required_ci.add_argument("--run-openvmm-unit-tests", required=True)
    required_ci.add_argument("--run-openvmm-vmm-tests", required=True)
    required_ci.set_defaults(handler=command_check_required_ci)

    openvmm_unit_tests = subparsers.add_parser(
        "test-openvmm-unit",
        help="run OpenVMM unit and documentation tests",
    )
    openvmm_unit_tests.set_defaults(handler=command_test_openvmm_unit)

    openvmm_tests = subparsers.add_parser(
        "test-openvmm",
        help="run OpenVMM Petri VMM tests",
    )
    openvmm_tests.add_argument(
        "--backend",
        choices=OPENVMM_TEST_BACKENDS,
        required=True,
    )
    openvmm_tests.set_defaults(handler=command_test_openvmm)

    microvm_tests = subparsers.add_parser(
        "test-microvm",
        help="run NVX-owned OpenVMM microVM correctness tests",
    )
    configure_microvm_test_parser(microvm_tests)

    doctor = subparsers.add_parser(
        "doctor",
        help="qualify this host for the NVX time ABI",
    )
    configure_doctor_parser(doctor)

    aci_edge_sandboxes_tests = subparsers.add_parser(
        "test-aci-edge-sandboxes",
        help="run the aci_edge_sandboxes crate lifecycle test on a real hypervisor",
    )
    configure_aci_edge_sandboxes_test_parser(aci_edge_sandboxes_tests)

    adversarial_tests = subparsers.add_parser(
        "test-adversarial",
        help="run a brokered Copilot-driven adversarial campaign",
    )
    configure_adversarial_parser(adversarial_tests)

    build = subparsers.add_parser("build", help="build guest artifacts and OpenVMM")
    _add_guest_options(build, allow_all=True)
    _add_openvmm_options(build)
    build.set_defaults(handler=command_build)

    download = subparsers.add_parser(
        "download",
        help=(
            "download and install the platform package for HEAD or its nearest "
            "released first-parent ancestor"
        ),
    )
    download.add_argument(
        "--repository",
        default=DEFAULT_RELEASE_REPOSITORY,
        metavar="OWNER/REPOSITORY",
    )
    download.add_argument("--hypervisor", choices=HYPERVISORS, default="auto")
    download.set_defaults(handler=command_download)

    run = subparsers.add_parser("run", help="run an OpenVMM microVM")
    run.add_argument("--guest", choices=GUEST_NAMES, default="alpine")
    run.add_argument("--hypervisor", choices=HYPERVISORS, default="auto")
    run.add_argument(
        "--machine",
        choices=("microvm",),
        default="microvm",
    )
    run.add_argument(
        "--memory-mib",
        type=int,
        help=(
            "guest RAM in MiB (default by --guest: "
            + ", ".join(
                f"{name} {guest_descriptor(name).default_memory_mib}"
                for name in GUEST_NAMES
            )
            + ")"
        ),
    )
    run.add_argument("--memory-capacity-mib", type=int)
    run.add_argument("--processors", type=int, choices=(1, 2, 4, 8), default=1)
    run.add_argument(
        "--cpu-profile",
        metavar="ID",
        help=(
            "OpenVMM CPU profile: auto (OpenVMM's default) for the built-in "
            "profile of the host's CPU, a built-in profile ID, or host to "
            "derive a development profile from this host (doc/usage.md)"
        ),
    )
    run.add_argument(
        "--mount",
        action="append",
        default=[],
        metavar="GUEST_TARGET,HOST_PATH[,ro|rw]",
        help=(
            "live-share a host directory; repeat once for a second share with "
            "its own guest target and mode"
        ),
    )
    run.add_argument("--mount-deny", action="append", type=Path, default=[])
    run.add_argument(
        "--mount-allow",
        action="append",
        type=Path,
        default=[],
        metavar="HOST_PATH",
        help="expose one existing path inside a --mount-deny path again",
    )
    run.add_argument(
        "--mount-write",
        action="append",
        type=Path,
        default=[],
        metavar="HOST_PATH",
        help=(
            "make one existing path one of the only writable parts of a "
            "read-write --mount"
        ),
    )
    run.add_argument(
        "--mount-owner",
        choices=MOUNT_OWNERS,
        help=(
            "host identity of the share's file operations: vmm (default) or "
            "caller, with guest root squashed to the directory owner (Linux only)"
        ),
    )
    run.add_argument("--net", metavar="IPV4/PREFIX")
    run.add_argument("--network-profile", choices=NETWORK_PROFILES)
    run.add_argument("--network-egress", choices=("allow", "deny"))
    run.add_argument("--network-ingress", choices=("allow", "deny"))
    run.add_argument(
        "--network-egress-allow",
        action="append",
        default=[],
        metavar=NETWORK_EGRESS_RULE_METAVAR,
        help=NETWORK_EGRESS_RULE_HELP.format(action="allow"),
    )
    run.add_argument(
        "--network-egress-deny",
        action="append",
        default=[],
        metavar=NETWORK_EGRESS_RULE_METAVAR,
        help=NETWORK_EGRESS_RULE_HELP.format(action="deny"),
    )
    run.add_argument(
        "--network-egress-policy-file",
        type=Path,
        metavar="PATH",
        help="load bounded MXC-shaped IPv4 egress rules from JSON",
    )
    run.add_argument("--host-loopback", choices=("allow", "deny"))
    run.add_argument("--network-proxy", metavar="IPV4:TCP-PORT")
    run.add_argument("--host-loopback-forward", action="append", default=[])
    run.add_argument(
        "--outcome-report",
        type=Path,
        help="write a bounded local JSON outcome report",
    )
    run.add_argument("--cmdline", default="")
    run.add_argument("--restore-snapshot", type=Path)
    run.add_argument("--restore-processors", type=int, choices=(1, 2, 4, 8))
    run.add_argument("--restore-image-slots", type=int, choices=(1, 2, 3, 4))
    run.add_argument("--restore-memory-mib", type=int)
    run.add_argument("--restore-ready-path", type=Path)
    run.add_argument("--dry-run", action="store_true")
    run.set_defaults(handler=command_run)

    sandbox = subparsers.add_parser(
        "sandbox",
        help="run or manage workloads over EROFS layers and private ext4 scratch",
    )

    def sandbox_layer(value: str) -> SandboxLayer:
        try:
            return SandboxLayer.parse(value)
        except ScriptError as error:
            raise argparse.ArgumentTypeError(str(error)) from error

    def sandbox_identity(value: str) -> tuple[int, int]:
        try:
            return parse_workload_identity(value)
        except ScriptError as error:
            raise argparse.ArgumentTypeError(str(error)) from error

    sandbox.add_argument(
        "sandbox_operation",
        nargs="?",
        choices=(
            "run",
            "provision",
            "start",
            "exec",
            "query-image-slots",
            "bind-image-slot",
            "stop",
            "deprovision",
        ),
        default="run",
    )
    sandbox.add_argument(
        "--layer",
        action="append",
        default=[],
        type=sandbox_layer,
        metavar="ROLE,PATH,EROFS_UUID",
    )
    sandbox.add_argument("--scratch", type=Path)
    sandbox.add_argument(
        "--image-slot-boot-count",
        type=int,
        choices=(1, 2, 3, 4),
        help="declare four image slots and activate this prefix at cold boot",
    )
    sandbox.add_argument("--image-slot", type=int, choices=(0, 1, 2, 3))
    sandbox.add_argument("--image-path", type=Path)
    sandbox.add_argument("--image-identity")
    sandbox.add_argument("--state-dir", type=Path)
    sandbox.add_argument("--entrypoint", default="/bin/sh")
    sandbox.add_argument("--arg", action="append", default=[], dest="sandbox_arg")
    sandbox.add_argument("--hostname", default="nvx-sandbox")
    sandbox.add_argument(
        "--workload-user",
        type=sandbox_identity,
        default=parse_workload_identity("65534:65534"),
        metavar="UID:GID",
        help="fixed non-root workload identity (default: 65534:65534)",
    )
    sandbox.add_argument("--memory-max", type=int)
    sandbox.add_argument("--pids-max", type=int)
    sandbox.add_argument("--memory-mib", type=int, default=256)
    sandbox.add_argument(
        "--timeout",
        type=float,
        default=60.0,
        help="control operation timeout in seconds (default: 60)",
    )
    sandbox.add_argument(
        "--exec-timeout-ms",
        type=int,
        default=0,
        help="guest workload timeout in milliseconds; zero disables it",
    )
    sandbox.add_argument(
        "--cwd",
        help="absolute guest working directory for managed exec (default: /)",
    )
    sandbox.add_argument(
        "--environment",
        action="append",
        default=[],
        metavar="KEY=VALUE",
        help="set the exact managed exec environment; repeat for multiple entries",
    )
    sandbox.add_argument(
        "--environment-file",
        type=Path,
        metavar="PATH",
        help="read the exact managed exec environment from a JSON string array",
    )
    sandbox.add_argument(
        "--inherit-default-environment",
        action="store_true",
        help=(
            "layer the managed exec environment over the guest default "
            "environment instead of replacing it"
        ),
    )
    sandbox.add_argument("--hypervisor", choices=HYPERVISORS, default="auto")
    sandbox.add_argument(
        "--mount",
        action="append",
        default=[],
        metavar="GUEST_TARGET,HOST_PATH[,ro|rw]",
        help=(
            "live-share a host directory inside the container rootfs; repeat "
            f"to attach up to {MAX_MOUNTS} shares, each with its own target and "
            "mode"
        ),
    )
    sandbox.add_argument(
        "--mount-deny",
        action=_MountPolicyPathAction,
        default=[],
        metavar="HOST_PATH",
        help=(
            "hide one existing path inside a --mount host directory; with "
            "several --mount options, it applies to the --mount before it"
        ),
    )
    sandbox.add_argument(
        "--mount-allow",
        action=_MountPolicyPathAction,
        default=[],
        metavar="HOST_PATH",
        help=(
            "expose one existing path inside a --mount-deny path of the same "
            "share again; the hidden directories on the way list only it"
        ),
    )
    sandbox.add_argument(
        "--mount-write",
        action=_MountPolicyPathAction,
        default=[],
        metavar="HOST_PATH",
        help=(
            "make one existing path one of the only writable parts of a "
            "read-write --mount; the rest of the share is read-only"
        ),
    )
    sandbox.add_argument(
        "--mount-owner",
        choices=MOUNT_OWNERS,
        help=(
            "host identity of the share's file operations: vmm (default) or "
            "caller, the workload identity with guest root squashed to the "
            "directory owner (Linux only)"
        ),
    )
    sandbox.add_argument("--net", metavar="IPV4/PREFIX")
    sandbox.add_argument("--network-profile", choices=NETWORK_PROFILES)
    sandbox.add_argument("--network-egress", choices=("allow", "deny"))
    sandbox.add_argument("--network-ingress", choices=("allow", "deny"))
    sandbox.add_argument(
        "--network-egress-allow",
        action="append",
        default=[],
        metavar=NETWORK_EGRESS_RULE_METAVAR,
        help=NETWORK_EGRESS_RULE_HELP.format(action="allow"),
    )
    sandbox.add_argument(
        "--network-egress-deny",
        action="append",
        default=[],
        metavar=NETWORK_EGRESS_RULE_METAVAR,
        help=NETWORK_EGRESS_RULE_HELP.format(action="deny"),
    )
    sandbox.add_argument(
        "--network-egress-policy-file",
        type=Path,
        metavar="PATH",
        help="load bounded IPv4 ranges and rule-local exclusions for run/provision",
    )
    sandbox.add_argument("--host-loopback", choices=("allow", "deny"))
    sandbox.add_argument("--network-proxy", metavar="IPV4:TCP-PORT")
    sandbox.add_argument("--host-loopback-forward", action="append", default=[])
    sandbox.add_argument(
        "--outcome-report",
        type=Path,
        help="write a bounded local JSON outcome report for run or exec",
    )
    sandbox.add_argument("--cmdline", default="")
    sandbox.add_argument(
        "--dry-run",
        action="store_true",
        help="print the one-shot run command without launching it",
    )
    sandbox.set_defaults(handler=command_sandbox)

    benchmark = subparsers.add_parser(
        "benchmark",
        help="run the OpenVMM-native benchmark coordinator",
    )
    configure_benchmark_parser(benchmark, BuildConstants.REPO_ROOT)
    benchmark.set_defaults(handler=command_benchmark)

    performance = subparsers.add_parser(
        "performance",
        help="collect, persist, and gate CI performance results",
    )
    configure_performance_parser(performance)

    sources = subparsers.add_parser(
        "collect-sources",
        help="materialize verified Linux, Alpine, and Ubuntu release sources",
    )
    sources.set_defaults(handler=command_collect_sources)

    alpine_sources = subparsers.add_parser(
        "collect-alpine-sources",
        help="collect exact Alpine recipes and upstream sources",
    )
    configure_alpine_sources_parser(alpine_sources)

    ubuntu_sources = subparsers.add_parser(
        "collect-ubuntu-sources",
        help="collect exact Ubuntu source packages",
    )
    configure_ubuntu_sources_parser(ubuntu_sources)

    linux_source_archive = subparsers.add_parser(
        "create-linux-source-archive",
        help="create the Linux corresponding-source archive from pinned inputs",
    )
    configure_linux_source_archive_parser(linux_source_archive)

    package = subparsers.add_parser("package", help="stage a binary distribution")
    package.add_argument("--version")
    package.add_argument("--destination", type=Path)
    source_mode = package.add_mutually_exclusive_group(required=True)
    source_mode.add_argument("--include-source", action="store_true")
    source_mode.add_argument(
        "--binary-only",
        action="store_true",
        help="stage binaries only; corresponding source must be published separately",
    )
    package.add_argument("--force", action="store_true")
    package.set_defaults(handler=command_package)

    archive_release = subparsers.add_parser(
        "archive-release",
        help="create a deterministic archive from a staged distribution",
    )
    archive_release.add_argument("--source", type=Path, required=True)
    archive_release.add_argument("--destination", type=Path, required=True)
    archive_release.set_defaults(handler=command_archive_release)

    verify = subparsers.add_parser("verify", help="verify source and submodule inputs")
    verify.set_defaults(handler=command_verify)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        result = args.handler(args)
    except KeyboardInterrupt:
        print("Interrupted", file=sys.stderr)
        return 130
    except (
        ScriptError,
        OSError,
        RuntimeError,
        ValueError,
        subprocess.CalledProcessError,
    ) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return result if result is not None else 0


if __name__ == "__main__":
    raise SystemExit(main())

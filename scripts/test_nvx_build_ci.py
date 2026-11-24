#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import yaml

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import (  # noqa: E402
    azurelinux,
    build,
    build_config,
    common,
    guest_binaries,
)
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
    DockerBuildConstants,
    UbuntuBuildConstants,
)
from nvx_tools_testing import (
    composite_action_script as _composite_action_script,
)
from nvx_tools_testing import (
    composite_action_step as _composite_action_step,
)  # noqa: E402
from nvx_tools_testing import (
    expression_conjuncts as _expression_conjuncts,
)
from nvx_tools_testing import (
    outer_parentheses_enclose as _outer_parentheses_enclose,
)
from nvx_tools_testing import (
    yaml_scalar as _yaml_scalar,
)


class BuildCiTests(unittest.TestCase):
    def test_ci_kernel_cache_key_includes_patches(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        for cache_input in (
            "SOURCE-MANIFEST.json",
            "kernel/config-microvm",
            "kernel/patches/**",
            "scripts/nvx_tools/build.py",
            "scripts/nvx_tools/common.py",
        ):
            self.assertIn(cache_input, action)
        self.assertIn("linux-kernel-v1-", action)
        self.assertEqual(action.count("build/vmlinux.provenance.json"), 2)
        self.assertEqual(action.count("build/initramfs.provenance.json"), 2)

    def test_ci_guest_kernel_keys_include_the_patch_identity(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        self.assertIn("kernel-patch-identity", action)
        self.assertIn(
            "linux-kernel-v1-${KERNEL_VERSION}-${KERNEL_INPUT_HASH}-${patch_identity}",
            action,
        )
        self.assertIn(
            "linux-kernel-debug-v1-${KERNEL_VERSION}-${DEBUG_KERNEL_INPUT_HASH}-${patch_identity}",
            action,
        )

    def test_ci_guest_image_keys_include_the_rust_source_identity(self):
        # hashFiles hashes contents only, so a module move that kept every
        # file's bytes reused images whose provenance named the old paths.
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        self.assertIn(
            "rust_source_identity=$(python3 scripts/nvx.py guest-binary-source-identity)",
            action,
        )
        for key in (
            "alpine=alpine-image-v2-${ALPINE_VERSION}-${ALPINE_INPUT_HASH}",
            "ubuntu=ubuntu-initramfs-v2-${UBUNTU_VERSION}-${UBUNTU_INPUT_HASH}",
            "ubuntu-distro=ubuntu-distro-v3-${UBUNTU_VERSION}-${UBUNTU_INPUT_HASH}",
            "azurelinux=azurelinux-initramfs-v2-${AZURELINUX_VERSION}-${AZURELINUX_INPUT_HASH}",
        ):
            with self.subTest(key=key.split("=", 1)[0]):
                self.assertIn(f'echo "{key}-${{rust_source_identity}}"', action)

    def test_ci_guest_cache_keys_include_build_configuration(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        cache_names = (
            "KERNEL_INPUT_HASH",
            "DEBUG_KERNEL_INPUT_HASH",
            "ALPINE_INPUT_HASH",
            "UBUNTU_INPUT_HASH",
            "AZURELINUX_INPUT_HASH",
        )
        # Every artifact, both kernels included, builds inside the image that
        # docker/Dockerfile pins, so a toolchain change must miss every cache.
        self.assertEqual(
            action.count("--file docker/Dockerfile"),
            action.count("docker build"),
        )
        for cache_name in cache_names:
            with self.subTest(cache=cache_name):
                cache_input = next(
                    line
                    for line in action.splitlines()
                    if line.strip().startswith(f"{cache_name}:")
                )
                self.assertIn("'docker/Dockerfile'", cache_input)
                self.assertIn("'scripts/nvx_tools/build_config.py'", cache_input)
                self.assertIn("'scripts/nvx_tools/build_constants.py'", cache_input)

    def test_ci_azurelinux_cache_key_covers_build_input_digest(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        cache_input = next(
            line
            for line in action.splitlines()
            if line.strip().startswith("AZURELINUX_INPUT_HASH:")
        )
        patterns = cache_input.split("'")[1::2]
        self.assertIn("scripts/nvx_tools/azurelinux.py", patterns)
        # The key ends with the source identity of the Rust guest binaries,
        # which covers the files that their build reads.
        self.assertIn('-${AZURELINUX_INPUT_HASH}-${rust_source_identity}"', action)
        identity = set(guest_binaries.guest_binary_source_files())
        for path in azurelinux.input_files():
            relative = path.relative_to(BuildConstants.REPO_ROOT).as_posix()
            with self.subTest(path=relative):
                self.assertTrue(
                    path in identity
                    or any(
                        relative == pattern
                        or (
                            pattern.endswith("/**")
                            and relative.startswith(pattern[:-2])
                        )
                        for pattern in patterns
                    )
                )

    def test_ci_debug_kernel_is_opt_in_and_keyed_by_its_fragment(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        cache_input = next(
            line
            for line in action.splitlines()
            if line.strip().startswith("DEBUG_KERNEL_INPUT_HASH:")
        )
        for path in (
            "kernel/config-microvm",
            "kernel/config-microvm-debug",
            "kernel/patches/**",
            "scripts/nvx_tools/build.py",
            "scripts/nvx_tools/build_constants.py",
        ):
            with self.subTest(path=path):
                self.assertIn(f"'{path}'", cache_input)
        kernel_input = next(
            line
            for line in action.splitlines()
            if line.strip().startswith("KERNEL_INPUT_HASH:")
        )
        self.assertNotIn("config-microvm-debug", kernel_input)
        build_step = _composite_action_step(action, "Build Linux debug kernel (Docker)")
        self.assertIn("steps.components.outputs.debug-kernel == 'true'", build_step)
        self.assertIn(
            f"--target {DockerBuildConstants.DEBUG_KERNEL_TARGET}", build_step
        )
        components = yaml.safe_load(action)["inputs"]["components"]
        self.assertNotIn("debug-kernel", components["default"].split())
        for name in DockerBuildConstants.DEBUG_KERNEL_ARTIFACT_NAMES:
            with self.subTest(artifact=name):
                self.assertEqual(
                    [line.strip() for line in action.splitlines()].count(
                        f"build/{name}"
                    ),
                    2,
                )

    def test_ci_guest_artifact_components_select_their_steps(self):
        # Each component's restore, build, and save steps follow its own
        # selection, so that jobs beside one another can each build some
        # components without building or saving the others.
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        components = yaml.safe_load(action)["inputs"]["components"]
        # Copilot sessions restore every guest image, but not the CI-only
        # debug kernel, through the default.
        self.assertEqual(
            components["default"].split(), ["kernel", "alpine", "ubuntu", "azurelinux"]
        )
        self.assertNotIn("guest-images:", action)
        self.assertNotIn("inputs.debug-kernel", action)
        steps = [
            line.strip().removeprefix("- name: ")
            for line in action.splitlines()
            if line.startswith("    - name: ")
        ]
        self.assertEqual(steps[0], "Select guest artifact components")
        always = {
            "Select guest artifact components",
            "Resolve guest artifact versions",
            "Resolve guest cache keys",
            # A record and its reuse cover every selected component.
            "Reuse the merged pull request's guest artifacts",
            "Record guest artifact keys and digests",
            "Upload guest artifact record",
        }
        owners = (
            ("debug kernel", "debug-kernel"),
            ("Linux kernel", "kernel"),
            ("Alpine", "alpine"),
            ("Azure Linux", "azurelinux"),
            ("Ubuntu", "ubuntu"),
        )

        def required(expression: str) -> list[str]:
            # The top-level conjuncts, and those of parenthesized conjunctions.
            conjuncts: list[str] = []
            for conjunct in _expression_conjuncts(expression):
                if _outer_parentheses_enclose(conjunct):
                    inner = conjunct.strip()[1:-1]
                    if len(_expression_conjuncts(inner)) > 1:
                        conjuncts.extend(required(inner))
                        continue
                conjuncts.append(conjunct)
            return conjuncts

        selected: dict[str, int] = {}
        for name in steps:
            with self.subTest(step=name):
                step = _composite_action_step(action, name)
                if name in always:
                    self.assertNotIn("steps.components.outputs.", step)
                    continue
                owner = next(component for key, component in owners if key in name)
                selected[owner] = selected.get(owner, 0) + 1
                selections = re.findall(
                    r"steps\.components\.outputs\.([a-z-]+) == 'true'", step
                )
                self.assertEqual(selections, [owner])
                self.assertIn(
                    f"steps.components.outputs.{owner} == 'true'",
                    required(_yaml_scalar(step, "if")),
                )
        self.assertEqual(
            selected,
            {
                "kernel": 3,
                "debug-kernel": 3,
                "alpine": 3,
                "azurelinux": 3,
                # The initramfs, the distro layer, which holds the sandbox
                # scratch template, and their determinism check.
                "ubuntu": 7,
            },
        )

    @unittest.skipUnless(os.name == "posix", "requires bash")
    def test_ci_guest_artifact_component_selection_fails_closed(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        script = _composite_action_script(action, "Select guest artifact components")
        names = ("kernel", "alpine", "ubuntu", "azurelinux", "debug-kernel")
        cases = (
            (
                "kernel alpine ubuntu azurelinux",
                {"kernel", "alpine", "ubuntu", "azurelinux"},
            ),
            ("debug-kernel", {"debug-kernel"}),
            ("  ubuntu\nazurelinux  ", {"ubuntu", "azurelinux"}),
            ("kernel kernel", {"kernel"}),
            ("", None),
            ("   ", None),
            ("kernel bogus", None),
            ("Kernel", None),
            ("kernel,alpine", None),
            ("*", None),
        )
        for components, expected in cases:
            with (
                self.subTest(components=components),
                tempfile.TemporaryDirectory() as temporary,
            ):
                output = Path(temporary) / "output"
                # A glob that matched a file would select its name.
                (Path(temporary) / "kernel").touch()
                result = subprocess.run(
                    ["bash", "-c", script],
                    cwd=temporary,
                    env={
                        **os.environ,
                        "COMPONENTS": components,
                        "GITHUB_OUTPUT": os.fspath(output),
                    },
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if expected is None:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("::error::", result.stderr)
                    continue
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(
                    output.read_text(encoding="utf-8").splitlines(),
                    [f"{name}={str(name in expected).lower()}" for name in names],
                )

    def test_ci_guest_cache_keys_include_shared_build_modules(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        for variable in (
            "ALPINE_INPUT_HASH",
            "UBUNTU_INPUT_HASH",
            "AZURELINUX_INPUT_HASH",
        ):
            assignment = next(line for line in action.splitlines() if variable in line)
            self.assertIn("'scripts/nvx_tools/common.py'", assignment)
            self.assertIn("'scripts/nvx_tools/guests.py'", assignment)
            self.assertIn("'scripts/nvx_tools/elf.py'", assignment)
            self.assertIn("'scripts/nvx_tools/guest_binaries.py'", assignment)
            # The Rust guest source identity at the end of each key covers the
            # files of the nvx crate that the guest build reads, and the
            # toolchain pin.
            self.assertNotIn("'nvx/", assignment)
            self.assertNotIn("rust-toolchain.toml", assignment)
        for prefix, version in (
            ("alpine-image", 2),
            ("ubuntu-initramfs", 2),
            # The distro layer's entry also holds the sandbox scratch template.
            ("ubuntu-distro", 3),
            ("azurelinux-initramfs", 2),
        ):
            with self.subTest(cache=prefix):
                self.assertIn(f"{prefix}-v{version}-", action)
                for earlier in range(1, version):
                    self.assertNotIn(f"{prefix}-v{earlier}-", action)

    def test_quality_checks_include_shared_identity_probe(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "check-quality"
            / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(action.count("guest/common/nvx-identity-probe"), 2)

    def test_docker_guest_builder_pins_erofs_toolchain(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "ARG DEBIAN_IMAGE=debian:12-slim@sha256:"
            "3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251",
            dockerfile,
        )
        self.assertIn("ARG EROFS_UTILS_VERSION=1.5-1", dockerfile)
        self.assertIn("erofs-utils=${EROFS_UTILS_VERSION}", dockerfile)

    def test_docker_builds_one_shared_rust_guest_binary_set(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            f"ARG RUST_BUILDER_IMAGE={DockerBuildConstants.RUST_BUILDER_IMAGE}",
            dockerfile,
        )
        builder = dockerfile.split("FROM ${RUST_BUILDER_IMAGE} AS guest-binaries", 1)[
            1
        ].split("\nFROM ", 1)[0]
        self.assertIn("COPY rust-toolchain.toml /repo/", builder)
        self.assertIn("COPY nvx/ /repo/nvx/", builder)
        self.assertIn(
            'rustup target add --toolchain "$toolchain" x86_64-unknown-linux-musl',
            builder,
        )
        self.assertIn(
            "python3 scripts/nvx.py build-guest-binaries --output /out/guest-binaries",
            builder,
        )
        self.assertEqual(dockerfile.count("build-guest-binaries"), 1)

        base = dockerfile.split("FROM ${DEBIAN_IMAGE} AS base", 1)[1].split(
            "\nFROM ", 1
        )[0]
        self.assertIn("        build-essential \\\n", base)
        self.assertNotIn("musl-tools", base)
        self.assertNotIn("nvx/", base)
        self.assertNotIn("rust-toolchain.toml", base)
        self.assertNotIn("guest-binaries", base)

        guest_base = dockerfile.split("FROM base AS guest-base", 1)[1].split(
            "\nFROM ", 1
        )[0]
        self.assertIn("COPY nvx/ /repo/nvx/", guest_base)
        self.assertIn("COPY rust-toolchain.toml /repo/", guest_base)
        self.assertIn(
            "COPY --from=guest-binaries /out/guest-binaries/ "
            "/repo/build/guest-binaries/",
            guest_base,
        )
        self.assertIn(
            "ENV NVX_GUEST_BINARIES=/repo/build/guest-binaries",
            guest_base,
        )

        determinism = dockerfile.split("FROM guest-base AS ubuntu-determinism", 1)[
            1
        ].split("\nFROM ", 1)[0]
        self.assertIn(
            "COPY --from=guest-binaries /usr/local/cargo/ /usr/local/cargo/",
            determinism,
        )
        self.assertIn(
            "COPY --from=guest-binaries /usr/local/rustup/ /usr/local/rustup/",
            determinism,
        )
        self.assertIn(
            "RUN NVX_GUEST_BINARIES= python3 scripts/nvx.py verify-guest-determinism",
            determinism,
        )

        azure = dockerfile.split("FROM guest-base AS azurelinux-initramfs", 1)[1].split(
            "\nFROM ", 1
        )[0]
        self.assertIn(
            "source = Path('/repo/build/guest-binaries')",
            azure,
        )
        self.assertIn("inspect_guest_binaries", azure)
        self.assertIn("for name in GuestRustBuildConstants.BINARIES:", azure)
        self.assertIn("'helpers': helpers", azure)
        self.assertNotIn("azurelinux-helpers", dockerfile)
        self.assertNotIn("musl-gcc", dockerfile)
        self.assertNotIn("cc -static", dockerfile)

    def test_docker_kernel_targets_do_not_build_the_guest_binaries(self):
        # BuildKit builds every stage that a target inherits or copies from, so
        # a kernel-only build must not compile the Rust guest binaries.
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        dependencies: dict[str, set[str]] = {}
        stage = ""
        for line in dockerfile.splitlines():
            if match := re.fullmatch(r"FROM (\S+) AS (\S+)", line):
                stage = match.group(2)
                dependencies[stage] = {match.group(1)}
            elif match := re.match(r"COPY --from=(\S+) ", line):
                dependencies[stage].add(match.group(1))

        def stages_built_for(target: str) -> set[str]:
            built: set[str] = set()
            pending = [target]
            while pending:
                for dependency in dependencies.get(pending.pop(), set()):
                    if dependency not in built:
                        built.add(dependency)
                        pending.append(dependency)
            return built

        for target in (
            "kernel-artifacts",
            DockerBuildConstants.DEBUG_KERNEL_TARGET,
            DockerBuildConstants.LINUX_SOURCE_TARGET,
        ):
            with self.subTest(target=target):
                self.assertIn("base", stages_built_for(target))
                self.assertNotIn("guest-binaries", stages_built_for(target))
        for target in (
            DockerBuildConstants.ALPINE_TARGET,
            DockerBuildConstants.UBUNTU_TARGET,
            DockerBuildConstants.AZURELINUX_TARGET,
            "ubuntu-determinism",
        ):
            with self.subTest(target=target):
                self.assertIn("guest-binaries", stages_built_for(target))

    def test_docker_distro_layer_targets_export_the_scratch_template(self):
        # The template needs only the base image's mkfs.ext4, so it builds no
        # guest binary, and it leaves the build with the distro layer.
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        stage = dockerfile.split("\nFROM base AS ubuntu-scratch-template\n", 1)[
            1
        ].split("\nFROM ", 1)[0]
        self.assertEqual(
            stage.strip(), "RUN python3 scripts/nvx.py build-scratch-template"
        )
        name = UbuntuBuildConstants.SCRATCH_TEMPLATE_NAME
        copy = f"COPY --from=ubuntu-scratch-template /repo/build/{name} /{name}"
        self.assertEqual(dockerfile.count(copy), 2)
        for target in (
            "ubuntu-distro-layer-artifacts",
            DockerBuildConstants.ALL_GUESTS_TARGET,
        ):
            with self.subTest(target=target):
                section = dockerfile.split(f"\nFROM scratch AS {target}\n", 1)[1].split(
                    "\nFROM ", 1
                )[0]
                self.assertIn(copy, section)

    def test_docker_debug_kernel_target_exports_debug_artifacts(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "FROM base AS kernel-debug\n"
            "RUN python3 scripts/nvx.py build-kernel --debug\n",
            dockerfile,
        )
        self.assertIn(
            f"FROM scratch AS {DockerBuildConstants.DEBUG_KERNEL_TARGET}", dockerfile
        )
        for name in DockerBuildConstants.DEBUG_KERNEL_ARTIFACT_NAMES:
            with self.subTest(artifact=name):
                self.assertIn(
                    f"COPY --from=kernel-debug /repo/build/{name} /{name}", dockerfile
                )

        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)

            def export(_command: object, **_kwargs: object) -> None:
                for name in DockerBuildConstants.DEBUG_KERNEL_ARTIFACT_NAMES:
                    (destination / name).write_bytes(b"artifact")

            config = build_config.DockerBuildConfig(artifact_destination=destination)
            with (
                patch.object(build, "require_tool", return_value="docker"),
                patch.object(build, "run_checked", side_effect=export) as run,
            ):
                build.build_docker_debug_kernel(config)
            command = run.call_args.args[0]
            self.assertEqual(
                command[command.index("--target") + 1],
                DockerBuildConstants.DEBUG_KERNEL_TARGET,
            )

            for name in DockerBuildConstants.DEBUG_KERNEL_ARTIFACT_NAMES:
                (destination / name).unlink()
            with (
                patch.object(build, "require_tool", return_value="docker"),
                patch.object(build, "run_checked"),
                self.assertRaisesRegex(common.ScriptError, "did not produce"),
            ):
                build.build_docker_debug_kernel(config)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Tests for the canonical setup-tool manifest and the scripts that read it.

scripts/setup/tool-versions.conf pins every tool that the setup scripts
install (#129). These tests check that the manifest pins each tool for both
platforms, that the POSIX shell and PowerShell readers in the setup scripts
accept, reject, and resolve manifests exactly as the reference reader below
does, that each setup script reads the manifest before it inspects or changes
the host, and that no script, workflow, or guide hard-codes a pin.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SETUP = REPO_ROOT / "scripts" / "setup"
MANIFEST = SETUP / "tool-versions.conf"
LINUX_RUNNER = SETUP / "setup-linux-runner.sh"
LINUX_MSHV = SETUP / "setup-linux-mshv.sh"
WINDOWS = SETUP / "setup-windows-whp.ps1"
SPECULA = REPO_ROOT / ".github" / "specula" / "setup-runner.sh"
COPILOT_SETUP = REPO_ROOT / ".github" / "workflows" / "copilot-setup-steps.yml"
GUIDES = (
    SETUP / "README.md",
    REPO_ROOT / "doc" / "setup.md",
    REPO_ROOT / "doc" / "ci.md",
    REPO_ROOT / ".github" / "copilot-instructions.md",
)

PLATFORMS = ("linux-x86_64", "windows-x86_64")
RELEASES = ("rust.toolchain", "cargo_nextest.version")
ARTIFACT_TOOLS = ("rustup", "sccache", "actions_runner")
# What the installers assume about each artifact: rustup's installer runs
# directly, each sccache archive holds a directory named after it, and
# Expand-Archive accepts only a .zip.
ARTIFACT_SUFFIXES = {
    ("rustup", "linux-x86_64"): "/rustup-init",
    ("rustup", "windows-x86_64"): "/rustup-init.exe",
    ("sccache", "linux-x86_64"): ".tar.gz",
    ("sccache", "windows-x86_64"): ".tar.gz",
    ("actions_runner", "linux-x86_64"): ".tar.gz",
    ("actions_runner", "windows-x86_64"): ".zip",
}
# Words that name each pinned tool. A line that holds one of them and a
# release hard-codes a pin.
TOOL_WORDS = {
    "rust.toolchain": r"rust[-_.]?toolchain|rust_version",
    "cargo_nextest.version": r"nextest",
    "rustup.version": r"rustup",
    "sccache.version": r"sccache",
    "actions_runner.version": r"actions[-_ ]?runner|runner_?version",
}
# Rustup and the build tooling cannot read the manifest, so these files repeat
# its Rust release; test_nvx_tools.py keeps them equal to it.
PIN_MIRRORS = {
    "rust.toolchain": (REPO_ROOT / "scripts" / "nvx_tools" / "build_constants.py",),
}

_RELEASE_PATTERN = r"(?<![0-9])(?<![0-9]\.){}(?![0-9])(?!\.[0-9])"
_RELEASE = re.compile(_RELEASE_PATTERN.format(r"[0-9]+\.[0-9]+\.[0-9]+"))
_CHECKSUM = re.compile(r"(?<![0-9A-Fa-f])[0-9A-Fa-f]{64}(?![0-9A-Fa-f])")
_KEY = re.compile(
    r"[a-z][a-z0-9_]*\.(?:(?P<field>version|toolchain)"
    r"|artifacts\.(?P<platform>[a-z0-9_-]+)\.(?P<artifact>name|sha256))"
)
_VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")
_VALUES = {
    "version": _VERSION,
    "toolchain": _VERSION,
    "sha256": re.compile(r"[0-9a-f]{64}"),
    "name": re.compile(
        r"[A-Za-z0-9_-][A-Za-z0-9._-]*(?:/[A-Za-z0-9_-][A-Za-z0-9._-]*)*"
    ),
}


class ManifestError(ValueError):
    def __init__(self, line: int, reason: str) -> None:
        super().__init__(f"line {line}: {reason}")
        self.line = line
        self.reason = reason


def parse_manifest(text: str) -> dict[str, str]:
    """Reads manifest text as the setup scripts do.

    Raises ManifestError for the first line that is not blank, a comment,
    or a KEY=VALUE pin of a supported platform with a well-formed value.
    """
    entries: dict[str, str] = {}
    for number, raw in enumerate(text.split("\n"), start=1):
        line = raw.removesuffix("\r")
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition("=")
        if not separator or " " in line or "\t" in line:
            raise ManifestError(number, "expected KEY=VALUE without spaces")
        match = _KEY.fullmatch(key)
        if match is None:
            raise ManifestError(number, f"unsupported key: {key}")
        platform = match["platform"]
        if platform is not None and platform not in PLATFORMS:
            raise ManifestError(number, f"unsupported platform: {platform}")
        if key in entries:
            raise ManifestError(number, f"duplicate key: {key}")
        field = match["field"] or match["artifact"]
        candidate = value.replace("{version}", "v") if field == "name" else value
        if _VALUES[field].fullmatch(candidate) is None:
            raise ManifestError(number, f"invalid value for {key}: {value}")
        entries[key] = value
    return entries


@dataclass(frozen=True)
class Artifact:
    version: str
    name: str
    sha256: str


def artifact_keys(tool: str, platform: str) -> tuple[str, str, str]:
    """Returns the keys of a tool's release, artifact name, and checksum in
    the order in which the readers look them up."""
    prefix = f"{tool}.artifacts.{platform}"
    return f"{tool}.version", f"{prefix}.name", f"{prefix}.sha256"


def resolve_artifact(entries: dict[str, str], tool: str, platform: str) -> Artifact:
    version, name, sha256 = (entries[key] for key in artifact_keys(tool, platform))
    return Artifact(version, name.replace("{version}", version), sha256)


def expected_reader_output(path: str, text: str | None, platform: str) -> list[str]:
    """Returns what the reader harnesses print for a manifest at `path` with
    `text`, or for a missing manifest when `text` is None."""
    if text is None:
        return [f"error: tool manifest not found: {path}"]
    try:
        entries = parse_manifest(text)
    except ManifestError as error:
        return [f"error: invalid tool manifest {path}:{error.line}: {error.reason}"]
    output: list[str] = []
    for key in RELEASES:
        if key not in entries:
            return [*output, f"error: tool manifest {path} does not define {key}"]
        output.append(f"{key} {entries[key]}")
    for tool in ARTIFACT_TOOLS:
        for key in artifact_keys(tool, platform):
            if key not in entries:
                return [*output, f"error: tool manifest {path} does not define {key}"]
        artifact = resolve_artifact(entries, tool, platform)
        output.append(f"{tool} {artifact.version} {artifact.name} {artifact.sha256}")
    return output


def _with_line(text: str, key: str, line: str | None) -> str:
    """Replaces the pin of `key` in manifest `text` with `line`, or removes
    it when `line` is None."""
    lines = text.split("\n")
    [index] = [
        index for index, current in enumerate(lines) if current.startswith(f"{key}=")
    ]
    if line is None:
        del lines[index]
    else:
        lines[index] = line
    return "\n".join(lines)


def reader_cases() -> list[tuple[str, str | None, str | None]]:
    """Returns manifests derived from the canonical one as (case, text,
    reason) triples. Text is None for a missing manifest, and reason is the
    start of the reference reader's error, or None when it accepts the text."""
    base = MANIFEST.read_text(encoding="utf-8")
    entries = parse_manifest(base)
    linux_sccache = "sccache.artifacts.linux-x86_64.sha256"
    linux_rustup = "rustup.artifacts.linux-x86_64.name"
    checksum = entries[linux_sccache]

    def replace(key: str, value: str) -> str:
        return _with_line(base, key, f"{key}={value}")

    return [
        ("canonical", base, None),
        ("crlf", base.replace("\n", "\r\n"), None),
        ("no-final-newline", base.rstrip("\n"), None),
        ("extra-tool", f"{base}foo_bar.version=1.2.3\n", None),
        ("empty", "", None),
        ("comments-only", "# none\n\n", None),
        ("incomplete-linux", _with_line(base, linux_sccache, None), None),
        (
            "incomplete-windows",
            _with_line(base, "actions_runner.artifacts.windows-x86_64.name", None),
            None,
        ),
        ("incomplete-release", _with_line(base, "cargo_nextest.version", None), None),
        ("missing", None, None),
        ("byte-order-mark", f"\ufeff{base}", "expected KEY=VALUE without spaces"),
        (
            "no-separator",
            f"{base}rust.toolchain\n",
            "expected KEY=VALUE without spaces",
        ),
        (
            "spaced",
            _with_line(
                base, "rustup.version", f"rustup.version = {entries['rustup.version']}"
            ),
            "expected KEY=VALUE without spaces",
        ),
        (
            "tabbed",
            replace("sccache.version", f"\t{entries['sccache.version']}"),
            "expected KEY=VALUE without spaces",
        ),
        (
            "uppercase-key",
            _with_line(
                base, "sccache.version", f"Sccache.version={entries['sccache.version']}"
            ),
            "unsupported key",
        ),
        ("unknown-field", f"{base}rustup.revision=1.2.3\n", "unsupported key"),
        (
            "nested-platform",
            f"{base}rustup.artifacts.linux.x86_64.sha256={checksum}\n",
            "unsupported key",
        ),
        (
            "unsupported-platform",
            f"{base}sccache.artifacts.linux-aarch64.sha256={checksum}\n",
            "unsupported platform",
        ),
        ("duplicate", f"{base}sccache.version=0.0.1\n", "duplicate key"),
        ("floating-toolchain", replace("rust.toolchain", "stable"), "invalid value"),
        ("two-part-version", replace("cargo_nextest.version", "1.2"), "invalid value"),
        ("suffixed-version", replace("rustup.version", "1.2.3-beta"), "invalid value"),
        ("empty-value", replace("rustup.version", ""), "invalid value"),
        (
            "uppercase-checksum",
            replace(linux_sccache, checksum.upper()),
            "invalid value",
        ),
        ("short-checksum", replace(linux_sccache, checksum[:-1]), "invalid value"),
        (
            "trailing-comment",
            replace(linux_sccache, f"{checksum} # pinned"),
            "expected KEY=VALUE without spaces",
        ),
        ("parent-name", replace(linux_rustup, "../rustup-init"), "invalid value"),
        ("absolute-name", replace(linux_rustup, "/rustup-init"), "invalid value"),
        ("hidden-name", replace(linux_rustup, "bin/.rustup-init"), "invalid value"),
        ("trailing-slash", replace(linux_rustup, "bin/"), "invalid value"),
        (
            "unknown-placeholder",
            replace(linux_rustup, "rustup-{release}/rustup-init"),
            "invalid value",
        ),
    ]


def _write_case(directory: Path, text: str | None) -> Path:
    """Stages a manifest case in `directory` and returns its path."""
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / "tool-versions.conf"
    if text is not None:
        path.write_bytes(text.encode("utf-8"))
    return path


def _shell_function(source: str, name: str) -> str:
    start = source.index(f"\n{name}() {{\n") + 1
    return source[start : source.index("\n}\n", start) + 3]


def _git_sibling(name: str) -> Path | None:
    """Returns a program from Git for Windows, whose sh and bash run the
    shell scripts on Windows."""
    git = shutil.which("git")
    program = Path(git).parent.parent / "bin" / name if git else None
    return program if program is not None and program.is_file() else None


def _posix_shells() -> list[list[str]]:
    """Returns commands for the distinct POSIX shells available, such as
    dash, Bash in POSIX mode, and BusyBox, or Git's sh on Windows."""
    if os.name == "nt":
        shell = _git_sibling("sh.exe")
        return [[str(shell)]] if shell is not None else []
    shells: list[list[str]] = []
    seen: set[str] = set()
    for command in (["sh"], ["dash"], ["bash", "--posix"], ["busybox", "sh"]):
        program = shutil.which(command[0])
        if program is None or os.path.realpath(program) in seen:
            continue
        seen.add(os.path.realpath(program))
        shells.append([program, *command[1:]])
    return shells


def _bash() -> str | None:
    if os.name == "nt":
        bash = _git_sibling("bash.exe")
        return str(bash) if bash is not None else None
    return shutil.which("bash")


def _powershells() -> list[str]:
    """Returns Windows PowerShell and PowerShell 7 where they are available."""
    names = ("powershell.exe", "pwsh") if os.name == "nt" else ("pwsh",)
    return [program for name in names if (program := shutil.which(name)) is not None]


def _blocks(output: str) -> list[list[str]]:
    """Splits harness output into the lines that follow each ### header."""
    blocks: list[list[str]] = []
    for line in output.splitlines():
        if line.startswith("### "):
            blocks.append([])
        elif blocks:
            blocks[-1].append(line.rstrip())
    return blocks


_ERRORS = (
    (
        "missing",
        re.compile(r"error: tool manifest not found: .*[\\/]tool-versions\.conf"),
    ),
    (
        "invalid",
        re.compile(
            r"error: invalid tool manifest .*[\\/]tool-versions\.conf:(\d+): (.*)"
        ),
    ),
    (
        "undefined",
        re.compile(
            r"error: tool manifest .*[\\/]tool-versions\.conf does not define (\S+)"
        ),
    ),
)


def _error(line: str) -> tuple[str, ...]:
    """Classifies a reader error independently of the path that it names."""
    for kind, pattern in _ERRORS:
        match = pattern.fullmatch(line.rstrip())
        if match is not None:
            return (kind, *match.groups())
    return ("unexpected", line)


_SHELL_READER_FUNCTIONS = (
    "die",
    "tool_manifest_error",
    "load_tool_manifest",
    "tool_manifest_value",
    "tool_manifest_artifact",
)
# Resolves every pin of each manifest argument for both platforms. A failed
# read ends only its subshell, so later cases still run.
_SHELL_READER = """
for manifest in "$@"; do
    for tool_platform in linux-x86_64 windows-x86_64; do
        printf '### %s\\n' "$tool_platform"
        (
            tool_manifest=$manifest
            load_tool_manifest
            for release in rust.toolchain cargo_nextest.version; do
                tool_manifest_value "$release"
                printf '%s %s\\n' "$release" "$tool_value"
            done
            for harness_tool in rustup sccache actions_runner; do
                tool_manifest_artifact "$harness_tool"
                printf '%s %s %s %s\\n' "$harness_tool" "$tool_version" \\
                    "$tool_artifact" "$tool_sha256"
            done
        ) 2>&1 || true
    done
done
"""
_POWERSHELL_FUNCTIONS = """
Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $env:NVX_TEST_SETUP_SCRIPT,
    [ref]$tokens,
    [ref]$errors
)
if ($errors.Count -ne 0) {
    throw "setup script has parse errors"
}
foreach ($name in "Read-ToolManifest", "Get-ToolManifestValue", "Get-ToolArtifact") {
    $definition = $ast.Find({
            param($node)
            $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
            $node.Name -eq $name
        }, $true)
    . ([scriptblock]::Create($definition.Extent.Text))
}
"""
_POWERSHELL_READER = """
foreach ($path in $env:NVX_TEST_MANIFESTS.Split([char]10)) {
    foreach ($platform in "linux-x86_64", "windows-x86_64") {
        [Console]::Out.WriteLine("### $platform")
        try {
            $manifest = Read-ToolManifest $path
            foreach ($key in "rust.toolchain", "cargo_nextest.version") {
                $value = Get-ToolManifestValue $manifest $key
                [Console]::Out.WriteLine("$key $value")
            }
            foreach ($tool in "rustup", "sccache", "actions_runner") {
                $artifact = Get-ToolArtifact $manifest $tool $platform
                [Console]::Out.WriteLine(
                    "$tool $($artifact.Version) $($artifact.Name) $($artifact.Sha256)")
            }
        }
        catch {
            [Console]::Out.WriteLine("error: " + $_.Exception.Message)
        }
    }
}
"""
# Runs the staged Windows setup script with the newline-separated arguments in
# NVX_TEST_SETUP_ARGUMENTS and reports its first error.
_POWERSHELL_SETUP = """
$setupArguments = $env:NVX_TEST_SETUP_ARGUMENTS.Split([char]10)
try {
    & $env:NVX_TEST_SETUP_SCRIPT @setupArguments
}
catch {
    [Console]::Out.WriteLine("error: " + $_.Exception.Message)
    exit 1
}
"""


def _run_powershell(
    powershell: str, script: str, environment: dict[str, str]
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            powershell,
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ],
        env={**os.environ, **environment},
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )


class ToolManifestTests(unittest.TestCase):
    def setUp(self) -> None:
        self.entries = parse_manifest(MANIFEST.read_text(encoding="utf-8"))

    def test_manifest_pins_every_tool_for_both_platforms(self):
        expected = {
            *RELEASES,
            *(
                key
                for tool in ARTIFACT_TOOLS
                for platform in PLATFORMS
                for key in artifact_keys(tool, platform)
            ),
        }
        self.assertEqual(set(self.entries), expected)
        self.assertEqual(
            set(ARTIFACT_SUFFIXES),
            {(tool, platform) for tool in ARTIFACT_TOOLS for platform in PLATFORMS},
        )
        for (tool, platform), suffix in ARTIFACT_SUFFIXES.items():
            with self.subTest(tool=tool, platform=platform):
                artifact = resolve_artifact(self.entries, tool, platform)
                self.assertTrue(artifact.name.endswith(suffix), artifact.name)
                # Names refer to the release only through {version}, so an
                # upgrade changes the version and the checksums alone.
                self.assertNotIn(
                    artifact.version,
                    self.entries[f"{tool}.artifacts.{platform}.name"],
                )
        checksums = [
            value for key, value in self.entries.items() if key.endswith(".sha256")
        ]
        self.assertEqual(len(checksums), len(set(checksums)))

    def test_manifest_reaches_linux_hosts_with_lf_line_endings(self):
        # A Windows checkout would otherwise stage CRLF lines onto Linux hosts.
        attributes = (REPO_ROOT / ".gitattributes").read_text(encoding="utf-8")
        self.assertIn("\nscripts/setup/tool-versions.conf text eol=lf\n", attributes)

    def test_reference_reader_accepts_only_well_formed_manifests(self):
        for case, text, reason in reader_cases():
            with self.subTest(case=case):
                if text is None:
                    continue
                if reason is None:
                    parse_manifest(text)
                    continue
                with self.assertRaises(ManifestError) as raised:
                    parse_manifest(text)
                self.assertTrue(
                    raised.exception.reason.startswith(reason),
                    raised.exception.reason,
                )

    def test_reference_reader_resolves_artifacts_for_each_platform(self):
        text = MANIFEST.read_text(encoding="utf-8")
        for platform in PLATFORMS:
            with self.subTest(platform=platform):
                output = expected_reader_output("manifest", text, platform)
                self.assertEqual(len(output), len(RELEASES) + len(ARTIFACT_TOOLS))
                self.assertFalse(any(line.startswith("error:") for line in output))
                self.assertFalse(any("{version}" in line for line in output))
        incomplete = _with_line(text, "sccache.artifacts.linux-x86_64.sha256", None)
        self.assertEqual(
            expected_reader_output("manifest", incomplete, "linux-x86_64")[-1],
            "error: tool manifest manifest does not define "
            "sccache.artifacts.linux-x86_64.sha256",
        )
        self.assertEqual(
            expected_reader_output("manifest", None, "linux-x86_64"),
            ["error: tool manifest not found: manifest"],
        )


class ToolManifestReaderTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.cases = reader_cases()
        self.paths = [
            _write_case(self.root / case, text) for case, text, _ in self.cases
        ]

    def _assert_reader_output(self, output: str, native_paths: bool) -> None:
        blocks = _blocks(output)
        self.assertEqual(len(blocks), len(self.cases) * len(PLATFORMS), output)
        index = 0
        for (case, text, _), path in zip(self.cases, self.paths, strict=True):
            shown = str(path) if native_paths else path.as_posix()
            for platform in PLATFORMS:
                with self.subTest(case=case, platform=platform):
                    expected = expected_reader_output(shown, text, platform)
                    self.assertEqual(
                        blocks[index], [line.rstrip() for line in expected]
                    )
                index += 1

    def test_linux_setups_share_one_manifest_reader(self):
        runner = LINUX_RUNNER.read_text(encoding="utf-8")
        mshv = LINUX_MSHV.read_text(encoding="utf-8")
        for name in _SHELL_READER_FUNCTIONS:
            with self.subTest(function=name):
                self.assertEqual(
                    _shell_function(runner, name), _shell_function(mshv, name)
                )

    def test_shell_reader_matches_the_reference_reader(self):
        shells = _posix_shells()
        if not shells:
            self.skipTest("a POSIX shell is unavailable")
        source = LINUX_RUNNER.read_text(encoding="utf-8")
        functions = "".join(
            _shell_function(source, name) for name in _SHELL_READER_FUNCTIONS
        )
        for shell in shells:
            with self.subTest(shell=" ".join(shell)):
                result = subprocess.run(
                    [
                        *shell,
                        "-c",
                        f"set -eu\n{functions}{_SHELL_READER}",
                        "reader",
                        *(path.as_posix() for path in self.paths),
                    ],
                    capture_output=True,
                    text=True,
                    timeout=120,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stderr, "")
                self._assert_reader_output(result.stdout, native_paths=False)

    def test_powershell_reader_matches_the_reference_reader(self):
        powershells = _powershells()
        if not powershells:
            self.skipTest("PowerShell is unavailable")
        for powershell in powershells:
            with self.subTest(powershell=Path(powershell).name):
                result = _run_powershell(
                    powershell,
                    _POWERSHELL_FUNCTIONS + _POWERSHELL_READER,
                    {
                        "NVX_TEST_SETUP_SCRIPT": str(WINDOWS),
                        "NVX_TEST_MANIFESTS": "\n".join(map(str, self.paths)),
                    },
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self._assert_reader_output(result.stdout, native_paths=True)

    def test_linux_setups_pin_their_tools_to_the_manifest(self):
        shells = _posix_shells()
        if not shells:
            self.skipTest("a POSIX shell is unavailable")
        text = MANIFEST.read_text(encoding="utf-8")
        entries = parse_manifest(text)
        pins = {
            "RUST_TOOLCHAIN": entries["rust.toolchain"],
            "CARGO_NEXTEST_VERSION": entries["cargo_nextest.version"],
        }
        for prefix, tool in (
            ("RUSTUP", "rustup"),
            ("SCCACHE", "sccache"),
            ("RUNNER", "actions_runner"),
        ):
            artifact = resolve_artifact(entries, tool, "linux-x86_64")
            pins[f"{prefix}_VERSION"] = artifact.version
            pins[f"{prefix}_ARTIFACT"] = artifact.name
            pins[f"{prefix}_SHA256"] = artifact.sha256
        mshv_pins = {
            name: value
            for name, value in pins.items()
            if not name.startswith(("SCCACHE_", "RUNNER_"))
        }
        incomplete = _with_line(text, "sccache.artifacts.linux-x86_64.sha256", None)
        for script, expected in ((LINUX_RUNNER, pins), (LINUX_MSHV, mshv_pins)):
            source = script.read_text(encoding="utf-8")
            # The scripts install the x86_64 Linux builds of each tool.
            self.assertIn("\ntool_platform=linux-x86_64\n", source)
            self.assertIn(
                "\ntool_manifest=${script_directory}/tool-versions.conf\n", source
            )
            functions = "".join(
                _shell_function(source, name)
                for name in (*_SHELL_READER_FUNCTIONS, "read_tool_versions")
            )
            report = "".join(
                f"printf '%s=%s\\n' {name} \"${name}\"\n" for name in expected
            )
            for case, manifest in (("canonical", text), ("incomplete", incomplete)):
                with self.subTest(script=script.name, case=case):
                    path = _write_case(self.root / script.stem / case, manifest)
                    result = subprocess.run(
                        [
                            *shells[0],
                            "-c",
                            "set -eu\n"
                            f"{functions}tool_manifest=$1\n"
                            "tool_platform=linux-x86_64\n"
                            f"read_tool_versions\n{report}",
                            "pins",
                            path.as_posix(),
                        ],
                        capture_output=True,
                        text=True,
                        timeout=60,
                        check=False,
                    )
                    # Only the runner installs sccache, so only it needs the
                    # sccache checksum.
                    if case == "incomplete" and script == LINUX_RUNNER:
                        self.assertEqual(result.returncode, 1)
                        self.assertEqual(
                            _error(result.stderr),
                            ("undefined", "sccache.artifacts.linux-x86_64.sha256"),
                        )
                        continue
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(
                        result.stdout.splitlines(),
                        [f"{name}={value}" for name, value in expected.items()],
                    )

    def test_windows_setup_pins_its_tools_to_the_manifest(self):
        powershells = _powershells()
        if not powershells:
            self.skipTest("PowerShell is unavailable")
        source = WINDOWS.read_text(encoding="utf-8")
        start = source.index("\n$ToolPlatform = ") + 1
        block = source[start : source.index("\n\n", start) + 1]
        self.assertIn('$ToolPlatform = "windows-x86_64"\n', block)
        self.assertEqual(block.count("$PSScriptRoot"), 1)
        block = block.replace("$PSScriptRoot", "$env:NVX_TEST_SETUP_DIRECTORY")
        names = {
            "RustToolchain": "rust.toolchain",
            "CargoNextestVersion": "cargo_nextest.version",
        }
        text = MANIFEST.read_text(encoding="utf-8")
        entries = parse_manifest(text)
        expected = {name: entries[key] for name, key in names.items()}
        for prefix, tool in (
            ("Rustup", "rustup"),
            ("Sccache", "sccache"),
            ("Runner", "actions_runner"),
        ):
            artifact = resolve_artifact(entries, tool, "windows-x86_64")
            expected[f"{prefix}Version"] = artifact.version
            expected[f"{prefix}Artifact"] = artifact.name
            expected[f"{prefix}Sha256"] = artifact.sha256
        report = "".join(
            f'[Console]::Out.WriteLine("{name}=" + ${name})\n' for name in expected
        )
        script = (
            f"{_POWERSHELL_FUNCTIONS}try {{\n{block}{report}}}\n"
            "catch {\n"
            '    [Console]::Out.WriteLine("error: " + $_.Exception.Message)\n'
            "    exit 1\n"
            "}\n"
        )
        cases = (
            ("canonical", text, None),
            (
                "incomplete-linux",
                _with_line(text, "sccache.artifacts.linux-x86_64.sha256", None),
                None,
            ),
            (
                "incomplete-windows",
                _with_line(text, "rustup.artifacts.windows-x86_64.sha256", None),
                ("undefined", "rustup.artifacts.windows-x86_64.sha256"),
            ),
        )
        for powershell in powershells:
            for case, manifest, error in cases:
                with self.subTest(powershell=Path(powershell).name, case=case):
                    directory = self.root / "windows" / case
                    _write_case(directory, manifest)
                    result = _run_powershell(
                        powershell,
                        script,
                        {
                            "NVX_TEST_SETUP_SCRIPT": str(WINDOWS),
                            "NVX_TEST_SETUP_DIRECTORY": str(directory),
                        },
                    )
                    if error is not None:
                        self.assertEqual(result.returncode, 1, result.stderr)
                        self.assertEqual(_error(result.stdout), error)
                        continue
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(
                        result.stdout.splitlines(),
                        [f"{name}={value}" for name, value in expected.items()],
                    )

    def test_setups_read_the_manifest_before_they_inspect_the_host(self):
        runner = LINUX_RUNNER.read_text(encoding="utf-8")
        self.assertEqual(runner.count("\nread_tool_versions\n"), 1)
        for later in (
            "\nreport_invariant_tsc /proc/cpuinfo\n",
            '\nsudo -n true || die "passwordless sudo is required"\n',
            "\n    install_packages\n",
        ):
            self.assertLess(runner.index("\nread_tool_versions\n"), runner.index(later))
        mshv = LINUX_MSHV.read_text(encoding="utf-8")
        self.assertEqual(mshv.count("\nread_tool_versions\n"), 1)
        for later in (
            "\nworkspace=$(CDPATH='' cd -- \"$workspace\" 2>/dev/null && pwd) ||\n",
            '\nif [ "$bundle_only" = true ]; then\n',
            '\nif [ "$check_only" = true ]; then\n',
            "\ninstall_packages\n",
        ):
            self.assertLess(mshv.index("\nread_tool_versions\n"), mshv.index(later))
        windows = WINDOWS.read_text(encoding="utf-8")
        load = windows.index("\n$ToolManifest = Read-ToolManifest ")
        for later in (
            "\n    $Workspace = (Resolve-Path -LiteralPath $Workspace).Path\n",
            "\nAssert-SupportedHost -SkipWorkspace:$RunnerOnly\n",
            "\nAssert-Administrator\n",
            "\nInstall-Toolchain\n",
        ):
            self.assertLess(load, windows.index(later))

    def _staged_failures(self) -> list[tuple[str, str | None, tuple[str, ...]]]:
        """Returns manifests that every setup script rejects, with the error
        that each script reports for it."""
        failures: list[tuple[str, str | None, tuple[str, ...]]] = []
        for case, text, _ in self.cases:
            if case == "missing":
                failures.append((case, text, ("missing",)))
            elif case == "unsupported-platform" and text is not None:
                try:
                    parse_manifest(text)
                except ManifestError as error:
                    failures.append(
                        (case, text, ("invalid", str(error.line), error.reason))
                    )
            elif case == "incomplete-release":
                failures.append((case, text, ("undefined", "cargo_nextest.version")))
        self.assertEqual(len(failures), 3)
        return failures

    def test_staged_linux_setups_reject_a_bad_manifest_before_anything_else(self):
        shells = _posix_shells()
        if not shells:
            self.skipTest("a POSIX shell is unavailable")
        # A missing workspace must not hide a bad manifest.
        missing = (self.root / "no-workspace").as_posix()
        for script, arguments in (
            (LINUX_RUNNER, ("--backend", "kvm", "--check-only")),
            (LINUX_MSHV, ("--check-only", "--skip-build", "--workspace", missing)),
        ):
            for case, text, error in self._staged_failures():
                with self.subTest(script=script.name, case=case):
                    directory = self.root / "staged" / script.stem / case
                    _write_case(directory, text)
                    staged = directory / script.name
                    shutil.copyfile(script, staged)
                    result = subprocess.run(
                        [*shells[0], staged.as_posix(), *arguments],
                        stdin=subprocess.DEVNULL,
                        capture_output=True,
                        text=True,
                        timeout=60,
                        check=False,
                    )
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertEqual(result.stdout, "")
                    self.assertEqual(len(result.stderr.splitlines()), 1, result.stderr)
                    self.assertEqual(_error(result.stderr), error)

    def test_staged_windows_setup_rejects_a_bad_manifest_before_anything_else(self):
        powershells = _powershells()
        if not powershells:
            self.skipTest("PowerShell is unavailable")
        modes = {
            "runner": ("-RunnerOnly", "-CheckOnly"),
            # A missing workspace must not hide a bad manifest.
            "development": (
                "-CheckOnly",
                "-SkipBuild",
                "-Workspace",
                str(self.root / "no-workspace"),
            ),
        }
        for powershell in powershells:
            for mode, arguments in modes.items():
                for case, text, error in self._staged_failures():
                    with self.subTest(
                        powershell=Path(powershell).name, mode=mode, case=case
                    ):
                        directory = (
                            self.root / "staged" / Path(powershell).stem / mode / case
                        )
                        _write_case(directory, text)
                        staged = directory / WINDOWS.name
                        shutil.copyfile(WINDOWS, staged)
                        result = _run_powershell(
                            powershell,
                            _POWERSHELL_SETUP,
                            {
                                "NVX_TEST_SETUP_SCRIPT": str(staged),
                                "NVX_TEST_SETUP_ARGUMENTS": "\n".join(arguments),
                            },
                        )
                        self.assertEqual(result.returncode, 1, result.stderr)
                        self.assertEqual(
                            len(result.stdout.splitlines()), 1, result.stdout
                        )
                        self.assertEqual(_error(result.stdout), error)


class ToolManifestConsumerTests(unittest.TestCase):
    def test_consumers_read_the_manifest(self):
        for path in (LINUX_RUNNER, LINUX_MSHV, WINDOWS, SPECULA, COPILOT_SETUP):
            with self.subTest(path=path.name):
                self.assertIn("tool-versions.conf", path.read_text(encoding="utf-8"))

    def test_consumers_and_guides_hard_code_no_pin(self):
        words = re.compile("|".join(TOOL_WORDS.values()), re.IGNORECASE)
        paths = {
            path
            for path in (*SETUP.iterdir(), SPECULA, COPILOT_SETUP, *GUIDES)
            if path.is_file() and path != MANIFEST
        }
        for path in sorted(paths):
            text = path.read_text(encoding="utf-8")
            for number, line in enumerate(text.splitlines(), start=1):
                location = f"{path.relative_to(REPO_ROOT).as_posix()}:{number}"
                with self.subTest(location=location):
                    if words.search(line):
                        self.assertIsNone(_RELEASE.search(line), line)
                    # Every checksum that setup verifies comes from the manifest.
                    if path.parent == SETUP:
                        self.assertIsNone(_CHECKSUM.search(line), line)

    def test_only_the_manifest_holds_its_pins(self):
        git = shutil.which("git")
        listing = (
            subprocess.run(
                [git, "ls-files", "-z"],
                cwd=REPO_ROOT,
                capture_output=True,
                timeout=60,
                check=False,
            )
            if git is not None
            else None
        )
        if listing is None or listing.returncode != 0:
            self.skipTest("the tracked files of a Git work tree are unavailable")
        entries = parse_manifest(MANIFEST.read_text(encoding="utf-8"))
        checksums = [
            value.encode("ascii")
            for key, value in entries.items()
            if key.endswith(".sha256")
        ]
        releases = {
            key: (
                re.compile(_RELEASE_PATTERN.format(re.escape(entries[key]))),
                re.compile(words, re.IGNORECASE),
            )
            for key, words in TOOL_WORDS.items()
        }
        for name in listing.stdout.decode("utf-8").split("\0"):
            path = REPO_ROOT / name
            if not name or path == MANIFEST or not path.is_file():
                continue
            data = path.read_bytes()
            lowered = data.lower()
            for checksum in checksums:
                self.assertFalse(
                    checksum in lowered, f"{name} repeats a manifest checksum"
                )
            text = data.decode("utf-8", "replace")
            for key, (release, words) in releases.items():
                if path in PIN_MIRRORS.get(key, ()) or release.search(text) is None:
                    continue
                for number, line in enumerate(text.splitlines(), start=1):
                    if release.search(line) and words.search(line):
                        self.fail(f"{name}:{number} hard-codes {key}: {line.strip()}")

    def test_specula_setup_reads_its_pins_from_the_manifest(self):
        source = SPECULA.read_text(encoding="utf-8")
        self.assertIn(
            '\nTOOL_MANIFEST="$SCRIPT_DIR/../../scripts/setup/tool-versions.conf"\n',
            source,
        )
        bash = _bash()
        if bash is None:
            self.skipTest("Bash is unavailable")
        start = source.index("\nVERSION_PATTERN=") + 1
        end = source.index("\n", source.index("\nCARGO_NEXTEST_VERSION=") + 1) + 1
        names = (
            "RUST_VERSION",
            "RUSTUP_VERSION",
            "RUSTUP_ARTIFACT",
            "RUSTUP_SHA256",
            "CARGO_NEXTEST_VERSION",
        )
        harness = (
            "set -euo pipefail\nTOOL_MANIFEST=$1\n"
            + _shell_function(source, "tool_value")
            + source[start:end]
            + "".join(f"printf '%s=%s\\n' {name} \"${name}\"\n" for name in names)
        )
        text = MANIFEST.read_text(encoding="utf-8")
        entries = parse_manifest(text)
        rustup = resolve_artifact(entries, "rustup", "linux-x86_64")
        expected = [
            f"RUST_VERSION={entries['rust.toolchain']}",
            f"RUSTUP_VERSION={rustup.version}",
            f"RUSTUP_ARTIFACT={rustup.name}",
            f"RUSTUP_SHA256={rustup.sha256}",
            f"CARGO_NEXTEST_VERSION={entries['cargo_nextest.version']}",
        ]
        checksum = "rustup.artifacts.linux-x86_64.sha256"
        cases = (
            ("canonical", text, None),
            ("missing", None, "rust.toolchain"),
            ("duplicate", f"{text}rustup.version=0.0.1\n", "rustup.version"),
            (
                "uppercase-checksum",
                _with_line(text, checksum, f"{checksum}={rustup.sha256.upper()}"),
                checksum,
            ),
        )
        with tempfile.TemporaryDirectory() as temporary:
            for case, manifest, invalid in cases:
                with self.subTest(case=case):
                    path = _write_case(Path(temporary) / case, manifest)
                    result = subprocess.run(
                        [bash, "-c", harness, "specula", path.as_posix()],
                        capture_output=True,
                        text=True,
                        timeout=60,
                        check=False,
                    )
                    if invalid is None:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertEqual(result.stdout.splitlines(), expected)
                        continue
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn(f"does not define a valid {invalid}\n", result.stderr)

    def test_copilot_setup_installs_the_manifest_nextest(self):
        workflow = COPILOT_SETUP.read_text(encoding="utf-8")
        # Copilot revalidates its setup when a pin that it installs changes.
        self.assertIn("\n      - scripts/setup/tool-versions.conf\n", workflow)
        step = workflow.split("      - name: Resolve pinned tool versions\n", 1)[1]
        step = step.split("\n      - name: ", 1)[0]
        script = "".join(
            line.removeprefix("          ") + "\n"
            for line in step.split("        run: |\n", 1)[1].splitlines()
        )
        self.assertIn("scripts/setup/tool-versions.conf", script)
        # The step reads LF-only checkouts, as on the hosted Linux runner.
        bash = shutil.which("bash")
        if os.name != "posix" or bash is None:
            return
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "output"
            result = subprocess.run(
                [bash, "-c", script],
                cwd=REPO_ROOT,
                env={**os.environ, "GITHUB_OUTPUT": str(output)},
                capture_output=True,
                text=True,
                timeout=60,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            entries = parse_manifest(MANIFEST.read_text(encoding="utf-8"))
            self.assertIn(
                f"nextest={entries['cargo_nextest.version']}",
                output.read_text(encoding="utf-8").splitlines(),
            )


if __name__ == "__main__":
    unittest.main()

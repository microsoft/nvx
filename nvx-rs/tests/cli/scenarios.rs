//! The tests that replace each scenario of `tests/fixtures/cli.json`, and those that guard against
//! the regressions that microsoft/nvx#722 traces, until the replay of the scenarios retires
//! (microsoft/nvx#728). This mapping retires with it.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use nvx::host::artifacts::MANIFEST_NAME;
use serde_json::Value;

use crate::{artifacts, dry_run, egress, lifecycle, one_shot, usage, validation};

/// `usage::usage_errors_exit_2_and_name_the_option`.
const USAGE: &str = "usage_errors_exit_2_and_name_the_option";
/// `usage::help_prints_the_usage_and_the_options_and_exits_0`.
const HELP: &str = "help_prints_the_usage_and_the_options_and_exits_0";
/// `validation::validation_errors_exit_1_before_any_side_effect`.
const VALIDATION: &str = "validation_errors_exit_1_before_any_side_effect";
/// `dry_run::dry_runs_print_the_openvmm_command_and_launch_nothing`.
const DRY_RUNS: &str = "dry_runs_print_the_openvmm_command_and_launch_nothing";
/// `one_shot::one_shot_runs_exit_with_the_workload_or_openvmm_status`.
const ONE_SHOT: &str = "one_shot_runs_exit_with_the_workload_or_openvmm_status";
/// `lifecycle::lifecycles_of_every_configuration`.
const LIFECYCLES: &str = "lifecycles_of_every_configuration";
/// `lifecycle::managed_execs_exit_with_the_workload_status`.
const EXECS: &str = "managed_execs_exit_with_the_workload_status";
/// `lifecycle::refusals_of_lifecycle_operations`.
const REFUSALS: &str = "refusals_of_lifecycle_operations";
/// `egress::egress_compile_prints_the_library_lowering`.
const EGRESS: &str = "egress_compile_prints_the_library_lowering";
/// `artifacts::artifacts_list_every_guest_in_order`.
const ARTIFACTS: &str = "artifacts_list_every_guest_in_order";

/// The sources of this crate that define the tests that the mapping names: those of the CLI's
/// tests, the unit tests of its control sessions, and those of the guest agent's launches.
const SOURCES: [&str; 10] = [
    include_str!("../cli.rs"),
    include_str!("artifacts.rs"),
    include_str!("dry_run.rs"),
    include_str!("egress.rs"),
    include_str!("lifecycle.rs"),
    include_str!("one_shot.rs"),
    include_str!("usage.rs"),
    include_str!("validation.rs"),
    include_str!("../../src/host/cli/session/tests.rs"),
    include_str!("../../src/guest/managed_agent/launch.rs"),
];

/// A recorded scenario or a regression, and the tests that check what it checked: each test by
/// its name, with the name of a case of its table, or an empty name for a test without one.
type Mapping = (&'static str, &'static [(&'static str, &'static str)]);

/// Each recorded scenario, of either section of the fixture, by its name.
const SCENARIOS: &[Mapping] = &[
    ("egress compile: empty", &[(EGRESS, "empty")]),
    ("egress compile: allow", &[(EGRESS, "allow")]),
    ("egress compile: both", &[(EGRESS, "both")]),
    ("egress compile: invalid JSON", &[(EGRESS, "invalid JSON")]),
    ("egress compile: invalid rule", &[(EGRESS, "invalid rule")]),
    (
        "egress compile: unknown field",
        &[(EGRESS, "unknown field")],
    ),
    ("egress compile: missing file", &[(EGRESS, "missing file")]),
    ("egress compile: directory", &[(EGRESS, "directory")]),
    ("artifacts: checkout", &[(ARTIFACTS, "checkout")]),
    (
        "artifacts: OpenVMM override",
        &[(ARTIFACTS, "OpenVMM override")],
    ),
    ("usage: help", &[(HELP, "sandbox")]),
    (
        "usage: help after operation",
        &[(HELP, "sandbox after an operation")],
    ),
    (
        "usage: timeout not a number",
        &[(USAGE, "timeout not a number")],
    ),
    ("usage: zero timeout", &[(USAGE, "zero timeout")]),
    ("usage: negative timeout", &[(USAGE, "negative timeout")]),
    ("usage: infinite timeout", &[(USAGE, "infinite timeout")]),
    (
        "usage: timeout not finite",
        &[(USAGE, "timeout not finite")],
    ),
    (
        "usage: timeout not a number either",
        &[(USAGE, "timeout not a number either")],
    ),
    (
        "usage: exec timeout not an integer",
        &[(USAGE, "exec timeout not an integer")],
    ),
    (
        "usage: memory not an integer",
        &[(USAGE, "memory not an integer")],
    ),
    (
        "usage: invalid hypervisor",
        &[(USAGE, "invalid hypervisor")],
    ),
    ("usage: invalid operation", &[(USAGE, "invalid operation")]),
    ("usage: second operation", &[(USAGE, "second operation")]),
    (
        "usage: invalid mount owner",
        &[(USAGE, "invalid mount owner")],
    ),
    (
        "usage: invalid network profile",
        &[(USAGE, "invalid network profile")],
    ),
    (
        "usage: invalid egress default",
        &[(USAGE, "invalid egress default")],
    ),
    (
        "usage: invalid host loopback",
        &[(USAGE, "invalid host loopback")],
    ),
    ("usage: missing value", &[(USAGE, "missing value")]),
    ("usage: option as value", &[(USAGE, "option as value")]),
    ("usage: dash value", &[(USAGE, "dash value")]),
    ("usage: flag with value", &[(USAGE, "flag with value")]),
    (
        "usage: unrecognized option",
        &[(USAGE, "unrecognized option")],
    ),
    (
        "usage: unrecognized short option",
        &[(USAGE, "unrecognized short option")],
    ),
    (
        "usage: unrecognized after separator",
        &[(USAGE, "unrecognized after separator")],
    ),
    (
        "validation: dry run outside run",
        &[(VALIDATION, "dry run outside run")],
    ),
    (
        "validation: environment and environment file",
        &[(VALIDATION, "environment and environment file")],
    ),
    (
        "validation: cwd outside exec",
        &[(VALIDATION, "cwd outside exec")],
    ),
    (
        "validation: environment outside exec",
        &[(VALIDATION, "environment outside exec")],
    ),
    (
        "validation: environment file outside exec",
        &[(VALIDATION, "environment file outside exec")],
    ),
    (
        "validation: inherited environment outside exec",
        &[(VALIDATION, "inherited environment outside exec")],
    ),
    (
        "validation: environment file too large",
        &[(VALIDATION, "environment file too large")],
    ),
    (
        "validation: environment file at the limit",
        &[(VALIDATION, "environment file at the limit")],
    ),
    (
        "validation: environment file missing",
        &[(VALIDATION, "environment file missing")],
    ),
    (
        "validation: environment file directory",
        &[(VALIDATION, "environment file directory")],
    ),
    (
        "validation: environment file not UTF-8",
        &[(VALIDATION, "environment file not UTF-8")],
    ),
    (
        "validation: environment file not JSON",
        &[(VALIDATION, "environment file not JSON")],
    ),
    (
        "validation: environment file object",
        &[(VALIDATION, "environment file object")],
    ),
    (
        "validation: environment file non-string",
        &[(VALIDATION, "environment file non-string")],
    ),
    (
        "validation: environment file entry without equals",
        &[(VALIDATION, "environment file entry without equals")],
    ),
    (
        "validation: environment file entry with NUL",
        &[(VALIDATION, "environment file entry with NUL")],
    ),
    (
        "validation: environment file duplicate entry",
        &[(VALIDATION, "environment file duplicate entry")],
    ),
    (
        "validation: environment entry without name",
        &[(VALIDATION, "environment entry without name")],
    ),
    (
        "validation: environment entry with space",
        &[(VALIDATION, "environment entry with space")],
    ),
    (
        "validation: network outside run",
        &[(VALIDATION, "network outside run")],
    ),
    (
        "validation: egress rule outside run",
        &[(VALIDATION, "egress rule outside run")],
    ),
    (
        "validation: forward outside run",
        &[(VALIDATION, "forward outside run")],
    ),
    (
        "validation: policy file outside run",
        &[(VALIDATION, "policy file outside run")],
    ),
    (
        "validation: systemd entrypoint of run",
        &[(VALIDATION, "systemd entrypoint of run")],
    ),
    (
        "validation: systemd entrypoint of exec",
        &[(VALIDATION, "systemd entrypoint of exec")],
    ),
    (
        "validation: outcome report outside run",
        &[(VALIDATION, "outcome report outside run")],
    ),
    (
        "validation: deny without mount",
        &[(VALIDATION, "deny without mount")],
    ),
    (
        "validation: allow without mount",
        &[(VALIDATION, "allow without mount")],
    ),
    (
        "validation: write without mount",
        &[(VALIDATION, "write without mount")],
    ),
    (
        "validation: owner without mount",
        &[(VALIDATION, "owner without mount")],
    ),
    (
        "validation: mount outside run",
        &[(VALIDATION, "mount outside run")],
    ),
    (
        "validation: net without profile",
        &[(VALIDATION, "net without profile")],
    ),
    (
        "validation: profile without net",
        &[(VALIDATION, "profile without net")],
    ),
    (
        "validation: run without layers",
        &[(VALIDATION, "run without layers")],
    ),
    (
        "validation: provision without scratch",
        &[(VALIDATION, "provision without scratch")],
    ),
    (
        "validation: run without layer",
        &[(VALIDATION, "run without layer")],
    ),
    (
        "validation: policy file with rules",
        &[(VALIDATION, "policy file with rules")],
    ),
    (
        "validation: policy file without egress",
        &[(VALIDATION, "policy file without egress")],
    ),
    (
        "validation: policy file missing",
        &[(VALIDATION, "policy file missing")],
    ),
    (
        "validation: policy file invalid",
        &[(VALIDATION, "policy file invalid")],
    ),
    (
        "validation: provision without state directory",
        &[(VALIDATION, "provision without state directory")],
    ),
    (
        "validation: start without state directory",
        &[(VALIDATION, "start without state directory")],
    ),
    (
        "validation: exec without state directory",
        &[(VALIDATION, "exec without state directory")],
    ),
    (
        "validation: stop without state directory",
        &[(VALIDATION, "stop without state directory")],
    ),
    (
        "validation: deprovision without state directory",
        &[(VALIDATION, "deprovision without state directory")],
    ),
    (
        "validation: run with state directory",
        &[(VALIDATION, "run with state directory")],
    ),
    (
        "validation: invalid layer UUID",
        &[(USAGE, "invalid layer UUID")],
    ),
    (
        "validation: invalid layer role",
        &[(USAGE, "invalid layer role")],
    ),
    (
        "validation: retired layer role",
        &[(USAGE, "retired layer role")],
    ),
    (
        "validation: missing layer",
        &[(VALIDATION, "missing layer")],
    ),
    (
        "validation: duplicate layer",
        &[(VALIDATION, "duplicate layer")],
    ),
    (
        "validation: missing scratch",
        &[(VALIDATION, "missing scratch")],
    ),
    (
        "validation: relative entrypoint",
        &[(VALIDATION, "relative entrypoint")],
    ),
    (
        "validation: invalid hostname",
        &[(VALIDATION, "invalid hostname")],
    ),
    (
        "validation: argument with space",
        &[(VALIDATION, "argument with space")],
    ),
    ("validation: root workload", &[(USAGE, "root workload")]),
    (
        "validation: invalid workload user",
        &[(USAGE, "invalid workload user")],
    ),
    (
        "validation: invalid memory limit",
        &[(USAGE, "invalid memory limit")],
    ),
    (
        "validation: invalid process limit",
        &[(VALIDATION, "invalid process limit")],
    ),
    (
        "validation: relative mount target",
        &[(VALIDATION, "relative mount target")],
    ),
    (
        "validation: missing mount host",
        &[(VALIDATION, "missing mount host")],
    ),
    (
        "validation: invalid mount mode",
        &[(VALIDATION, "invalid mount mode")],
    ),
    (
        "validation: deny outside share",
        &[(VALIDATION, "deny outside share")],
    ),
    (
        "validation: allow outside denied path",
        &[(VALIDATION, "allow outside denied path")],
    ),
    (
        "validation: write in read-only share",
        &[(VALIDATION, "write in read-only share")],
    ),
    (
        "validation: reserved command-line token",
        &[(VALIDATION, "reserved command-line token")],
    ),
    (
        "validation: systemd distro image",
        &[(VALIDATION, "systemd distro image")],
    ),
    (
        "validation: mismatched distro manifest",
        &[(VALIDATION, "mismatched distro manifest")],
    ),
    (
        "validation: invalid distro manifest",
        &[(VALIDATION, "invalid distro manifest")],
    ),
    (
        "validation: distro manifest without packages",
        &[(VALIDATION, "distro manifest without packages")],
    ),
    ("dry run: minimal", &[(DRY_RUNS, "minimal")]),
    (
        "dry run: every layer and option",
        &[(DRY_RUNS, "every layer and option")],
    ),
    (
        "dry run: explicit hypervisor",
        &[(DRY_RUNS, "explicit hypervisor")],
    ),
    ("dry run: one share", &[(DRY_RUNS, "one share")]),
    ("dry run: writable share", &[(DRY_RUNS, "writable share")]),
    ("dry run: file share", &[(DRY_RUNS, "file share")]),
    (
        "dry run: several shares with policies",
        &[(DRY_RUNS, "several shares with policies")],
    ),
    ("dry run: network", &[(DRY_RUNS, "network")]),
    (
        "dry run: proxy-only network",
        &[(DRY_RUNS, "proxy-only network")],
    ),
    (
        "dry run: egress policy file",
        &[(DRY_RUNS, "egress policy file")],
    ),
    (
        "dry run: quoted arguments",
        &[(DRY_RUNS, "quoted arguments")],
    ),
    (
        "dry run: caller-owned share",
        &[(DRY_RUNS, "caller-owned share")],
    ),
    ("run: echo", &[(ONE_SHOT, "echo")]),
    ("run: default operation", &[(ONE_SHOT, "default operation")]),
    ("run: failure", &[(ONE_SHOT, "failure")]),
    ("run: missing program", &[(ONE_SHOT, "missing program")]),
    ("run: exit status", &[(ONE_SHOT, "exit status")]),
    ("run: outcome report", &[(ONE_SHOT, "outcome report")]),
    (
        "run: network outcome report",
        &[(ONE_SHOT, "network outcome report")],
    ),
    (
        "run: network options without --net",
        &[(VALIDATION, "network options without --net")],
    ),
    ("run: missing OpenVMM", &[(VALIDATION, "missing OpenVMM")]),
    ("run: missing kernel", &[(VALIDATION, "missing kernel")]),
    (
        "run: missing initramfs",
        &[(VALIDATION, "missing initramfs")],
    ),
    (
        "lifecycle: minimal",
        &[
            (LIFECYCLES, "minimal"),
            (EXECS, "echo"),
            (EXECS, "working directory"),
            (EXECS, "exit status with output"),
            (EXECS, "exact environment"),
            (EXECS, "inherited environment"),
            (EXECS, "environment file"),
            (EXECS, "empty environment file"),
            (EXECS, "timeout"),
            (EXECS, "signal"),
            (EXECS, "missing program"),
            (EXECS, "missing working directory"),
            (EXECS, "relative working directory"),
            (EXECS, "existing outcome report"),
            (EXECS, "outcome report in a missing directory"),
        ],
    ),
    ("lifecycle: one share", &[(LIFECYCLES, "one share")]),
    (
        "lifecycle: caller-owned share",
        &[(LIFECYCLES, "caller-owned share")],
    ),
    (
        "lifecycle: several shares",
        &[(LIFECYCLES, "several shares")],
    ),
    (
        "lifecycle: share policies",
        &[(LIFECYCLES, "share policies")],
    ),
    ("lifecycle: file share", &[(LIFECYCLES, "file share")]),
    (
        "lifecycle: network and launch options",
        &[(LIFECYCLES, "network and launch options")],
    ),
    (
        "lifecycle: proxy-only network",
        &[(LIFECYCLES, "proxy-only network")],
    ),
    (
        "state: operations without a sandbox",
        &[(REFUSALS, "operations without a sandbox")],
    ),
    (
        "state: refusals of a running sandbox",
        &[(REFUSALS, "refusals of a running sandbox")],
    ),
    (
        "state: relative and nested state directory",
        &[(LIFECYCLES, "relative and nested state directory")],
    ),
    ("state: failed start", &[(REFUSALS, "failed start")]),
    (
        "state: start without OpenVMM",
        &[(REFUSALS, "start without OpenVMM")],
    ),
    (
        "state: malformed configuration",
        &[(REFUSALS, "malformed configuration")],
    ),
    ("state: not a directory", &[(REFUSALS, "not a directory")]),
    ("state: foreign file", &[(REFUSALS, "foreign file")]),
    ("run, usage: help", &[(HELP, "run")]),
    (
        "run, usage: help after options",
        &[(HELP, "run after options")],
    ),
    (
        "run, usage: help before an invalid value",
        &[(HELP, "run before an invalid value")],
    ),
    (
        "run, usage: invalid value before help",
        &[(USAGE, "run: invalid value before help")],
    ),
    (
        "run, usage: invalid guest",
        &[(USAGE, "run: invalid guest")],
    ),
    (
        "run, usage: invalid second guest",
        &[(USAGE, "run: invalid second guest")],
    ),
    (
        "run, usage: invalid hypervisor",
        &[(USAGE, "run: invalid hypervisor")],
    ),
    (
        "run, usage: invalid machine",
        &[(USAGE, "run: invalid machine")],
    ),
    (
        "run, usage: memory not an integer",
        &[(USAGE, "run: memory not an integer")],
    ),
    (
        "run, usage: memory not a whole number",
        &[(USAGE, "run: memory not a whole number")],
    ),
    (
        "run, usage: memory with a space",
        &[(USAGE, "run: memory with a space")],
    ),
    (
        "run, usage: memory with other digits",
        &[(USAGE, "run: memory with other digits")],
    ),
    (
        "run, usage: memory capacity not an integer",
        &[(USAGE, "run: memory capacity not an integer")],
    ),
    (
        "run, usage: restore memory not an integer",
        &[(USAGE, "run: restore memory not an integer")],
    ),
    (
        "run, usage: processors not an integer",
        &[(USAGE, "run: processors not an integer")],
    ),
    (
        "run, usage: processors not a choice",
        &[(USAGE, "run: processors not a choice")],
    ),
    (
        "run, usage: processors converted before the choice",
        &[(USAGE, "run: processors converted before the choice")],
    ),
    (
        "run, usage: processors with a leading zero",
        &[(USAGE, "run: processors with a leading zero")],
    ),
    (
        "run, usage: negative processors",
        &[(USAGE, "run: negative processors")],
    ),
    (
        "run, usage: restore processors not an integer",
        &[(USAGE, "run: restore processors not an integer")],
    ),
    (
        "run, usage: restore processors not a choice",
        &[(USAGE, "run: restore processors not a choice")],
    ),
    (
        "run, usage: invalid mount owner",
        &[(USAGE, "run: invalid mount owner")],
    ),
    (
        "run, usage: invalid network profile",
        &[(USAGE, "run: invalid network profile")],
    ),
    (
        "run, usage: invalid network egress",
        &[(USAGE, "run: invalid network egress")],
    ),
    (
        "run, usage: invalid network ingress",
        &[(USAGE, "run: invalid network ingress")],
    ),
    (
        "run, usage: invalid host loopback",
        &[(USAGE, "run: invalid host loopback")],
    ),
    (
        "run, usage: missing value",
        &[(USAGE, "run: missing value")],
    ),
    ("run, usage: missing path", &[(USAGE, "run: missing path")]),
    (
        "run, usage: option as value",
        &[(USAGE, "run: option as value")],
    ),
    ("run, usage: dash value", &[(USAGE, "run: dash value")]),
    (
        "run, usage: flag with value",
        &[(USAGE, "run: flag with value")],
    ),
    (
        "run, usage: unrecognized option",
        &[(USAGE, "run: unrecognized option")],
    ),
    (
        "run, usage: unrecognized short option",
        &[(USAGE, "run: unrecognized short option")],
    ),
    (
        "run, usage: unrecognized positional",
        &[(USAGE, "run: unrecognized positional")],
    ),
    (
        "run, usage: unrecognized after separator",
        &[(USAGE, "run: unrecognized after separator")],
    ),
    (
        "run, usage: unrecognized after options",
        &[(USAGE, "run: unrecognized after options")],
    ),
    (
        "run, validation: net without profile",
        &[(VALIDATION, "run: net without profile")],
    ),
    (
        "run, validation: profile without net",
        &[(VALIDATION, "run: profile without net")],
    ),
    (
        "run, validation: ready path without snapshot",
        &[(VALIDATION, "run: ready path without snapshot")],
    ),
    (
        "run, validation: restore processors without snapshot",
        &[(VALIDATION, "run: restore processors without snapshot")],
    ),
    (
        "run, validation: restore memory without snapshot",
        &[(VALIDATION, "run: restore memory without snapshot")],
    ),
    (
        "run, validation: memory capacity of a restore",
        &[(VALIDATION, "run: memory capacity of a restore")],
    ),
    (
        "run, validation: Ubuntu guest of a restore",
        &[(VALIDATION, "run: Ubuntu guest of a restore")],
    ),
    (
        "run, validation: Azure Linux guest of a restore",
        &[(VALIDATION, "run: Azure Linux guest of a restore")],
    ),
    (
        "run, validation: memory capacity below memory",
        &[(VALIDATION, "run: memory capacity below memory")],
    ),
    (
        "run, validation: memory capacity below the guest default",
        &[(VALIDATION, "run: memory capacity below the guest default")],
    ),
    (
        "run, validation: restore processors above processors",
        &[(VALIDATION, "run: restore processors above processors")],
    ),
    (
        "run, validation: policy file with allow rules",
        &[(VALIDATION, "run: policy file with allow rules")],
    ),
    (
        "run, validation: policy file with deny rules",
        &[(VALIDATION, "run: policy file with deny rules")],
    ),
    (
        "run, validation: policy file without egress",
        &[(VALIDATION, "run: policy file without egress")],
    ),
    (
        "run, validation: policy file missing",
        &[(VALIDATION, "run: policy file missing")],
    ),
    (
        "run, validation: policy file directory",
        &[(VALIDATION, "run: policy file directory")],
    ),
    (
        "run, validation: policy file invalid",
        &[(VALIDATION, "run: policy file invalid")],
    ),
    (
        "run, validation: policy file not JSON",
        &[(VALIDATION, "run: policy file not JSON")],
    ),
    (
        "run, validation: policy errors before missing artifacts",
        &[(VALIDATION, "run: policy errors before missing artifacts")],
    ),
    (
        "run, validation: missing OpenVMM",
        &[(VALIDATION, "run: missing OpenVMM")],
    ),
    (
        "run, validation: missing kernel",
        &[(VALIDATION, "run: missing kernel")],
    ),
    (
        "run, validation: missing Alpine initramfs",
        &[(VALIDATION, "run: missing Alpine initramfs")],
    ),
    (
        "run, validation: missing Ubuntu initramfs",
        &[(VALIDATION, "run: missing Ubuntu initramfs")],
    ),
    (
        "run, validation: missing Azure Linux initramfs",
        &[(VALIDATION, "run: missing Azure Linux initramfs")],
    ),
    (
        "run, validation: missing artifacts before invalid shares",
        &[(VALIDATION, "run: missing artifacts before invalid shares")],
    ),
    (
        "run, validation: mount without host path",
        &[(VALIDATION, "run: mount without host path")],
    ),
    (
        "run, validation: mount with empty host path",
        &[(VALIDATION, "run: mount with empty host path")],
    ),
    (
        "run, validation: mount with empty host path and mode",
        &[(VALIDATION, "run: mount with empty host path and mode")],
    ),
    (
        "run, validation: mount with invalid mode",
        &[(VALIDATION, "run: mount with invalid mode")],
    ),
    (
        "run, validation: mount with commas but no mode",
        &[(VALIDATION, "run: mount with commas but no mode")],
    ),
    (
        "run, validation: invalid second mount",
        &[(VALIDATION, "run: invalid second mount")],
    ),
    (
        "run, validation: invalid mount before invalid targets",
        &[(VALIDATION, "run: invalid mount before invalid targets")],
    ),
    (
        "run, validation: owner without mount",
        &[(VALIDATION, "run: owner without mount")],
    ),
    (
        "run, validation: relative deny path with several shares",
        &[(VALIDATION, "run: relative deny path with several shares")],
    ),
    (
        "run, validation: relative allow path with a shared file",
        &[(VALIDATION, "run: relative allow path with a shared file")],
    ),
    (
        "run, validation: relative write path with several shares",
        &[(VALIDATION, "run: relative write path with several shares")],
    ),
    (
        "run, validation: relative paths in kind order",
        &[(VALIDATION, "run: relative paths in kind order")],
    ),
    (
        "run, validation: relative target with several shares",
        &[(VALIDATION, "run: relative target with several shares")],
    ),
    (
        "run, validation: relative target of a shared file",
        &[(VALIDATION, "run: relative target of a shared file")],
    ),
    (
        "run, validation: root target with several shares",
        &[(VALIDATION, "run: root target with several shares")],
    ),
    (
        "run, validation: target with a space",
        &[(VALIDATION, "run: target with a space")],
    ),
    (
        "run, validation: target with an equals sign",
        &[(VALIDATION, "run: target with an equals sign")],
    ),
    (
        "run, validation: target with a backslash",
        &[(VALIDATION, "run: target with a backslash")],
    ),
    (
        "run, validation: target with a dot component",
        &[(VALIDATION, "run: target with a dot component")],
    ),
    (
        "run, validation: target with a parent component",
        &[(VALIDATION, "run: target with a parent component")],
    ),
    (
        "run, validation: target with an empty component",
        &[(VALIDATION, "run: target with an empty component")],
    ),
    (
        "run, validation: equal targets",
        &[(VALIDATION, "run: equal targets")],
    ),
    (
        "run, validation: nested targets",
        &[(VALIDATION, "run: nested targets")],
    ),
    (
        "run, validation: target containing another",
        &[(VALIDATION, "run: target containing another")],
    ),
    (
        "run, validation: target containing the aggregate",
        &[(VALIDATION, "run: target containing the aggregate")],
    ),
    (
        "run, validation: target in the aggregate",
        &[(VALIDATION, "run: target in the aggregate")],
    ),
    (
        "run, validation: too many shares for the budget",
        &[(VALIDATION, "run: too many shares for the budget")],
    ),
    (
        "run, validation: budget exceeded by a byte",
        &[(VALIDATION, "run: budget exceeded by a byte")],
    ),
    (
        "run, validation: budget in UTF-8 bytes",
        &[(VALIDATION, "run: budget in UTF-8 bytes")],
    ),
    (
        "run, validation: missing OpenVMM override",
        &[(VALIDATION, "run: missing OpenVMM override")],
    ),
    (
        "run, dry run: Alpine default",
        &[(DRY_RUNS, "run: Alpine default")],
    ),
    (
        "run, dry run: Ubuntu default",
        &[(DRY_RUNS, "run: Ubuntu default")],
    ),
    (
        "run, dry run: Azure Linux default",
        &[(DRY_RUNS, "run: Azure Linux default")],
    ),
    (
        "run, dry run: Alpine with memory",
        &[(DRY_RUNS, "run: Alpine with memory")],
    ),
    (
        "run, dry run: Ubuntu with memory and capacity",
        &[(DRY_RUNS, "run: Ubuntu with memory and capacity")],
    ),
    (
        "run, dry run: Azure Linux with the default capacity",
        &[(DRY_RUNS, "run: Azure Linux with the default capacity")],
    ),
    ("run, dry run: last guest", &[(DRY_RUNS, "run: last guest")]),
    (
        "run, dry run: every boot option",
        &[(DRY_RUNS, "run: every boot option")],
    ),
    (
        "run, dry run: auto hypervisor",
        &[(DRY_RUNS, "run: auto hypervisor")],
    ),
    ("run, dry run: KVM", &[(DRY_RUNS, "run: KVM")]),
    ("run, dry run: WHP", &[(DRY_RUNS, "run: WHP")]),
    (
        "run, dry run: canonical integers",
        &[(DRY_RUNS, "run: canonical integers")],
    ),
    (
        "run, dry run: other digits",
        &[(DRY_RUNS, "run: other digits")],
    ),
    (
        "run, dry run: negative memory",
        &[(DRY_RUNS, "run: negative memory")],
    ),
    (
        "run, dry run: attached negative memory",
        &[(DRY_RUNS, "run: attached negative memory")],
    ),
    (
        "run, dry run: equal memory and capacity",
        &[(DRY_RUNS, "run: equal memory and capacity")],
    ),
    (
        "run, dry run: last processors",
        &[(DRY_RUNS, "run: last processors")],
    ),
    (
        "run, dry run: cpu profile auto",
        &[(DRY_RUNS, "run: cpu profile auto")],
    ),
    (
        "run, dry run: cpu profile host",
        &[(DRY_RUNS, "run: cpu profile host")],
    ),
    ("run, dry run: restore", &[(DRY_RUNS, "run: restore")]),
    (
        "run, dry run: restore without guest artifacts",
        &[(DRY_RUNS, "run: restore without guest artifacts")],
    ),
    (
        "run, dry run: restore with every option",
        &[(DRY_RUNS, "run: restore with every option")],
    ),
    (
        "run, dry run: restore of the Alpine guest",
        &[(DRY_RUNS, "run: restore of the Alpine guest")],
    ),
    (
        "run, dry run: restore with equal processors",
        &[(DRY_RUNS, "run: restore with equal processors")],
    ),
    (
        "run, dry run: restore ignores the memory",
        &[(DRY_RUNS, "run: restore ignores the memory")],
    ),
    (
        "run, dry run: restore paths",
        &[(DRY_RUNS, "run: restore paths")],
    ),
    ("run, dry run: one share", &[(DRY_RUNS, "run: one share")]),
    (
        "run, dry run: one writable share",
        &[(DRY_RUNS, "run: one writable share")],
    ),
    (
        "run, dry run: one relative share",
        &[(DRY_RUNS, "run: one relative share")],
    ),
    (
        "run, dry run: one share with relative policy paths",
        &[(DRY_RUNS, "run: one share with relative policy paths")],
    ),
    (
        "run, dry run: policy paths without a share",
        &[(DRY_RUNS, "run: policy paths without a share")],
    ),
    (
        "run, dry run: several shares with policy paths",
        &[(DRY_RUNS, "run: several shares with policy paths")],
    ),
    (
        "run, dry run: several shares with a command line",
        &[(DRY_RUNS, "run: several shares with a command line")],
    ),
    (
        "run, dry run: one shared file",
        &[(DRY_RUNS, "run: one shared file")],
    ),
    (
        "run, dry run: one relative shared file",
        &[(DRY_RUNS, "run: one relative shared file")],
    ),
    (
        "run, dry run: a directory and a file",
        &[(DRY_RUNS, "run: a directory and a file")],
    ),
    (
        "run, dry run: host paths with commas",
        &[(DRY_RUNS, "run: host paths with commas")],
    ),
    (
        "run, dry run: one directory with commas",
        &[(DRY_RUNS, "run: one directory with commas")],
    ),
    (
        "run, dry run: host path ending in a mode",
        &[(DRY_RUNS, "run: host path ending in a mode")],
    ),
    (
        "run, dry run: host path ending in a comma",
        &[(DRY_RUNS, "run: host path ending in a comma")],
    ),
    ("run, dry run: vmm owner", &[(DRY_RUNS, "run: vmm owner")]),
    (
        "run, dry run: vmm owner of several shares",
        &[(DRY_RUNS, "run: vmm owner of several shares")],
    ),
    (
        "run, dry run: restore with shares",
        &[(DRY_RUNS, "run: restore with shares")],
    ),
    (
        "run, dry run: restore with shares and a command line",
        &[(DRY_RUNS, "run: restore with shares and a command line")],
    ),
    ("run, dry run: network", &[(DRY_RUNS, "run: network")]),
    (
        "run, dry run: egress default",
        &[(DRY_RUNS, "run: egress default")],
    ),
    (
        "run, dry run: host loopback forward",
        &[(DRY_RUNS, "run: host loopback forward")],
    ),
    (
        "run, dry run: egress policy file",
        &[(DRY_RUNS, "run: egress policy file")],
    ),
    (
        "run, dry run: empty egress policy file",
        &[(DRY_RUNS, "run: empty egress policy file")],
    ),
    (
        "run, dry run: outcome report path",
        &[(DRY_RUNS, "run: outcome report path")],
    ),
    (
        "run, dry run: quoted command line",
        &[(DRY_RUNS, "run: quoted command line")],
    ),
    (
        "run, dry run: command line starting with a dash",
        &[(DRY_RUNS, "run: command line starting with a dash")],
    ),
    (
        "run, dry run: command line of help",
        &[(DRY_RUNS, "run: command line of help")],
    ),
    (
        "run, dry run: empty command line",
        &[(DRY_RUNS, "run: empty command line")],
    ),
    (
        "run, dry run: blank command line",
        &[(DRY_RUNS, "run: blank command line")],
    ),
    (
        "run, dry run: last command line",
        &[(DRY_RUNS, "run: last command line")],
    ),
    (
        "run, dry run: long command line of one directory share",
        &[(DRY_RUNS, "run: long command line of one directory share")],
    ),
    (
        "run, dry run: restore without the budget",
        &[(DRY_RUNS, "run: restore without the budget")],
    ),
    (
        "run, dry run: budget fits",
        &[(DRY_RUNS, "run: budget fits")],
    ),
    (
        "run, dry run: caller owner",
        &[(DRY_RUNS, "run: caller owner")],
    ),
    ("run, launch: boot", &[(ONE_SHOT, "run: boot")]),
    (
        "run, launch: exit status",
        &[(ONE_SHOT, "run: exit status")],
    ),
    (
        "run, launch: failed boot",
        &[(ONE_SHOT, "run: failed boot")],
    ),
    ("run, launch: Ubuntu", &[(ONE_SHOT, "run: Ubuntu")]),
    (
        "run, launch: every boot option",
        &[(ONE_SHOT, "run: every boot option")],
    ),
    ("run, launch: restore", &[(ONE_SHOT, "run: restore")]),
    (
        "run, launch: restore without guest artifacts",
        &[(ONE_SHOT, "run: restore without guest artifacts")],
    ),
    (
        "run, launch: missing snapshot",
        &[(ONE_SHOT, "run: missing snapshot")],
    ),
    (
        "run, launch: outcome report",
        &[(ONE_SHOT, "run: outcome report")],
    ),
    (
        "run, launch: restore outcome report",
        &[(ONE_SHOT, "run: restore outcome report")],
    ),
    (
        "run, launch: network outcome report",
        &[(ONE_SHOT, "run: network outcome report")],
    ),
    ("run, launch: one share", &[(ONE_SHOT, "run: one share")]),
    (
        "run, launch: several shares and a file",
        &[(ONE_SHOT, "run: several shares and a file")],
    ),
    (
        "run, launch: OpenVMM refuses its options",
        &[(ONE_SHOT, "run: OpenVMM refuses its options")],
    ),
    (
        "run, launch: relative OpenVMM override",
        &[(ONE_SHOT, "run: relative OpenVMM override")],
    ),
    (
        "run, launch: missing OpenVMM",
        &[(VALIDATION, "run: missing OpenVMM at launch")],
    ),
    (
        "run, launch: missing kernel",
        &[(VALIDATION, "run: missing kernel at launch")],
    ),
    (
        "run, launch: missing initramfs",
        &[(VALIDATION, "run: missing initramfs at launch")],
    ),
    (
        "run, launch: missing Ubuntu initramfs",
        &[(VALIDATION, "run: missing Ubuntu initramfs at launch")],
    ),
    (
        "validation: caller-owned share on Windows",
        &[(VALIDATION, "caller-owned share on Windows")],
    ),
    (
        "run, validation: caller owner on Windows",
        &[(VALIDATION, "run: caller owner on Windows")],
    ),
];

/// The regressions that microsoft/nvx#722 traces and the tests of this crate guard against, by
/// their issues: of the managed sandbox, of its executions, and of the egress policies.
const REGRESSIONS: &[Mapping] = &[
    // A record that names the ID of a process that is not its OpenVMM.
    (
        "microsoft/nvx#369",
        &[
            (REFUSALS, "recycled process ID"),
            (
                "a_stop_under_another_identity_never_ends_a_process_that_its_record_does_not_name",
                "",
            ),
            (
                "a_sandbox_whose_openvmm_exited_reports_it_once_and_starts_again",
                "",
            ),
        ],
    ),
    // `--dry-run` of a lifecycle operation, and the `PWD` of the environment that the sandbox
    // agent gives a workload in a working directory.
    (
        "microsoft/nvx#373",
        &[
            (VALIDATION, "dry run outside run"),
            (REFUSALS, "dry runs of lifecycle operations"),
            (
                "sandbox_launch_runs_the_workload_itself_through_its_entry",
                "",
            ),
        ],
    ),
    // A scoped IPv6 range whose host bits normalization drops.
    ("microsoft/nvx#462", &[(EGRESS, "scoped IPv6 range")]),
    // Concurrent starts, and a request that the control endpoint does not read.
    (
        "microsoft/nvx#464",
        &[
            (REFUSALS, "concurrent starts"),
            (
                "request_to_a_stalled_endpoint_times_out_and_closes_the_session",
                "",
            ),
        ],
    ),
];

/// The regressions that microsoft/nvx#722 traces and checks in other parts of the repository
/// guard against, by their issues, with each check by the path of its source in the repository
/// and a fragment that only it holds. The package of this crate lacks those sources, so the
/// mapping test reads them at run time, from a checkout.
const ELSEWHERE: &[(&str, &[(&str, &str)])] = &[
    // The `PWD` that a real guest gives a workload in a working directory: the
    // `managed-exec-config` microVM scenario checks it with `/usr/bin/env`, which, unlike a
    // shell, exports no `PWD` of its own.
    (
        "microsoft/nvx#373",
        &[(
            "scripts/nvx_tools/managed_exec_tests.py",
            "public managed environment did not point PWD at the",
        )],
    ),
    // An asynchronous collection of output that the workload's timeout ends, as a synchronous
    // one does, also when the workload fills a stream that the caller does not read.
    (
        "microsoft/nvx#513",
        &[(
            "aci_edge_sandboxes/tests/async_api.rs",
            "fn async_wait_with_output_honors_the_workload_timeout(",
        )],
    ),
];

/// Returns the names of the cases of the table of each test that has one, by the name of the test.
fn inventory() -> BTreeMap<&'static str, Vec<&'static str>> {
    BTreeMap::from([
        (
            USAGE,
            usage::USAGE_ERRORS.iter().map(|case| case.name).collect(),
        ),
        (HELP, usage::HELP.iter().map(|case| case.name).collect()),
        (
            VALIDATION,
            validation::sandbox_refusals()
                .into_iter()
                .chain(validation::run_refusals())
                .map(|case| case.name)
                .collect(),
        ),
        (
            DRY_RUNS,
            dry_run::sandbox_dry_runs()
                .into_iter()
                .chain(dry_run::run_dry_runs())
                .map(|case| case.name)
                .collect(),
        ),
        (
            ONE_SHOT,
            one_shot::sandbox_launches()
                .into_iter()
                .chain(one_shot::run_launches())
                .map(|case| case.name)
                .collect(),
        ),
        (
            LIFECYCLES,
            lifecycle::configurations()
                .into_iter()
                .map(|case| case.name)
                .collect(),
        ),
        (
            EXECS,
            lifecycle::executions()
                .into_iter()
                .map(|case| case.name)
                .collect(),
        ),
        (
            REFUSALS,
            lifecycle::REFUSALS.iter().map(|(name, _)| *name).collect(),
        ),
        (
            EGRESS,
            egress::COMPILATIONS.iter().map(|case| case.name).collect(),
        ),
        (
            ARTIFACTS,
            artifacts::LISTINGS.iter().map(|case| case.name).collect(),
        ),
    ])
}

#[test]
fn every_recorded_scenario_maps_to_a_test() {
    let fixture: Value = serde_json::from_str(crate::FIXTURE).unwrap();
    let mut recorded: Vec<&str> = Vec::new();
    for platform in ["posix", "windows"] {
        for scenario in fixture[platform].as_array().unwrap() {
            let name = scenario["name"].as_str().unwrap();
            if !recorded.contains(&name) {
                recorded.push(name);
            }
        }
    }
    let mapped: Vec<&str> = SCENARIOS.iter().map(|(name, _)| *name).collect();
    let unmapped: Vec<&&str> = recorded
        .iter()
        .filter(|name| !mapped.contains(name))
        .collect();
    assert!(
        unmapped.is_empty(),
        "scenarios that map to no test: {unmapped:?}"
    );
    let unrecorded: Vec<&&str> = mapped
        .iter()
        .filter(|name| !recorded.contains(name))
        .collect();
    assert!(
        unrecorded.is_empty(),
        "mapped scenarios that the fixture lacks: {unrecorded:?}"
    );
    assert_eq!(
        mapped.len(),
        recorded.len(),
        "a scenario maps more than once"
    );
    let inventory = inventory();
    for (test, cases) in &inventory {
        let mut names = cases.clone();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), cases.len(), "{test} names a case twice");
    }
    for (name, tests) in SCENARIOS.iter().chain(REGRESSIONS) {
        assert!(!tests.is_empty(), "{name} maps to no test");
        for (test, case) in *tests {
            let defining = SOURCES
                .iter()
                .filter(|source| source.contains(&format!("fn {test}(")))
                .count();
            assert_eq!(defining, 1, "{name}: no test {test}");
            match inventory.get(test) {
                Some(cases) => assert!(cases.contains(case), "{name}: {test} has no case {case:?}"),
                None => assert!(case.is_empty(), "{name}: {test} has no table of cases"),
            }
        }
    }
    // A checkout holds the release manifest at its root, and the other parts of the repository,
    // which the package of this crate lacks.
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    if repository.join(MANIFEST_NAME).is_file() {
        for (name, checks) in ELSEWHERE {
            for (path, fragment) in *checks {
                let path = repository.join(path);
                let source = fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
                assert_eq!(
                    source.matches(fragment).count(),
                    1,
                    "{name}: {} does not hold {fragment:?} once",
                    path.display()
                );
            }
        }
    }
}

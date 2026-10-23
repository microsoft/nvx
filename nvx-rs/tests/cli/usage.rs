//! The usage errors of the parsers of `nvx-rs sandbox` and `nvx-rs run`, and their help.

use crate::harness::{LAYER, LINE_END, Root, SCRATCH};

/// The help of `nvx-rs run`, which `nvx.py run` prints too, since it parses the options of
/// `nvx-rs run` before it runs it: a test of `nvx.py` checks its help against this one.
const RUN_HELP: &str = include_str!("../fixtures/run-help.txt");

/// A command line that a parser of `nvx-rs` refuses.
struct UsageError {
    /// The name of the case.
    name: &'static str,
    /// The arguments after `--root ROOT`, with placeholders.
    arguments: &'static [&'static str],
    /// The code of the error: `argument-unrecognized`, `argument-missing`, or `argument-invalid`.
    code: &'static str,
    /// What the message names: the option or argument that the parser refused and, if any, the
    /// value that it refused.
    fragments: &'static [&'static str],
}

/// The command lines that the parsers refuse before they check anything else.
const USAGE_ERRORS: &[UsageError] = &[
    UsageError {
        name: "timeout not a number",
        arguments: &["sandbox", "--timeout", "x"],
        code: "argument-invalid",
        fragments: &["--timeout", "'x'"],
    },
    UsageError {
        name: "zero timeout",
        arguments: &["sandbox", "--timeout", "0"],
        code: "argument-invalid",
        fragments: &["--timeout", "greater than 0"],
    },
    UsageError {
        name: "negative timeout",
        arguments: &["sandbox", "--timeout", "-1"],
        code: "argument-invalid",
        fragments: &["--timeout", "greater than 0"],
    },
    UsageError {
        name: "infinite timeout",
        arguments: &["sandbox", "--timeout", "inf"],
        code: "argument-invalid",
        fragments: &["--timeout", "finite"],
    },
    UsageError {
        name: "timeout not finite",
        arguments: &["sandbox", "--timeout=1e400"],
        code: "argument-invalid",
        fragments: &["--timeout", "finite"],
    },
    UsageError {
        name: "timeout not a number either",
        arguments: &["sandbox", "--timeout", "nan"],
        code: "argument-invalid",
        fragments: &["--timeout", "finite"],
    },
    UsageError {
        name: "exec timeout not an integer",
        arguments: &["sandbox", "--exec-timeout-ms", "1.5"],
        code: "argument-invalid",
        fragments: &["--exec-timeout-ms", "'1.5'"],
    },
    UsageError {
        name: "memory not an integer",
        arguments: &["sandbox", "--memory-mib", "x"],
        code: "argument-invalid",
        fragments: &["--memory-mib", "'x'"],
    },
    UsageError {
        name: "invalid hypervisor",
        arguments: &["sandbox", "--hypervisor", "hyperv"],
        code: "argument-invalid",
        fragments: &["--hypervisor", "'hyperv'"],
    },
    UsageError {
        name: "invalid operation",
        arguments: &["sandbox", "restart"],
        code: "argument-invalid",
        fragments: &["'restart'", "'deprovision'"],
    },
    UsageError {
        name: "second operation",
        arguments: &["sandbox", "run", "start"],
        code: "argument-unrecognized",
        fragments: &["unrecognized arguments: start"],
    },
    UsageError {
        name: "invalid mount owner",
        arguments: &["sandbox", "--mount-owner", "root"],
        code: "argument-invalid",
        fragments: &["--mount-owner", "'root'"],
    },
    UsageError {
        name: "invalid network profile",
        arguments: &["sandbox", "--network-profile", "open"],
        code: "argument-invalid",
        fragments: &["--network-profile", "'open'"],
    },
    UsageError {
        name: "invalid egress default",
        arguments: &["sandbox", "--network-egress", "block"],
        code: "argument-invalid",
        fragments: &["--network-egress", "'block'"],
    },
    UsageError {
        name: "invalid host loopback",
        arguments: &["sandbox", "--host-loopback", "yes"],
        code: "argument-invalid",
        fragments: &["--host-loopback", "'yes'"],
    },
    UsageError {
        name: "missing value",
        arguments: &["sandbox", "--layer"],
        code: "argument-missing",
        fragments: &["--layer", "one argument"],
    },
    UsageError {
        name: "option as value",
        arguments: &["sandbox", "--layer", "--scratch"],
        code: "argument-missing",
        fragments: &["--layer", "one argument"],
    },
    UsageError {
        name: "dash value",
        arguments: &["sandbox", "--arg", "-x"],
        code: "argument-missing",
        fragments: &["--arg", "one argument"],
    },
    UsageError {
        name: "flag with value",
        arguments: &["sandbox", "--dry-run=1"],
        code: "argument-invalid",
        fragments: &["--dry-run", "'1'"],
    },
    UsageError {
        name: "unrecognized option",
        arguments: &["sandbox", "--bogus"],
        code: "argument-unrecognized",
        fragments: &["unrecognized arguments: --bogus"],
    },
    UsageError {
        name: "unrecognized short option",
        arguments: &["sandbox", "-x"],
        code: "argument-unrecognized",
        fragments: &["unrecognized arguments: -x"],
    },
    UsageError {
        name: "unrecognized after separator",
        arguments: &["sandbox", "run", "--", "extra"],
        code: "argument-unrecognized",
        fragments: &["unrecognized arguments: extra"],
    },
    UsageError {
        name: "invalid layer UUID",
        arguments: &[
            "sandbox",
            "run",
            "--layer",
            "distro,{root/distro.erofs},nope",
            "--scratch",
            SCRATCH,
        ],
        code: "argument-invalid",
        fragments: &["--layer", "UUID", "'nope'"],
    },
    UsageError {
        name: "invalid layer role",
        arguments: &[
            "sandbox",
            "run",
            "--layer",
            "base,{root/distro.erofs},11111111-1111-1111-1111-111111111111",
        ],
        code: "argument-invalid",
        fragments: &["--layer", "'base'"],
    },
    UsageError {
        name: "retired layer role",
        arguments: &[
            "sandbox",
            "run",
            "--layer",
            "runtime,{root/runtime.erofs},22222222-2222-2222-2222-222222222222",
        ],
        code: "argument-invalid",
        fragments: &["--layer", "'runtime'"],
    },
    UsageError {
        name: "root workload",
        arguments: &[
            "sandbox",
            "run",
            "--layer",
            LAYER,
            "--scratch",
            SCRATCH,
            "--workload-user",
            "0:0",
        ],
        code: "argument-invalid",
        fragments: &["--workload-user", "UID"],
    },
    UsageError {
        name: "invalid workload user",
        arguments: &[
            "sandbox",
            "run",
            "--layer",
            LAYER,
            "--scratch",
            SCRATCH,
            "--workload-user",
            "x",
        ],
        code: "argument-invalid",
        fragments: &["--workload-user", "UID:GID"],
    },
    UsageError {
        name: "invalid memory limit",
        arguments: &[
            "sandbox",
            "run",
            "--layer",
            LAYER,
            "--scratch",
            SCRATCH,
            "--memory-max",
            "x",
        ],
        code: "argument-invalid",
        fragments: &["--memory-max", "'x'"],
    },
    UsageError {
        name: "run: invalid value before help",
        arguments: &["run", "--memory-mib", "x", "--help"],
        code: "argument-invalid",
        fragments: &["--memory-mib", "'x'"],
    },
    UsageError {
        name: "run: invalid guest",
        arguments: &["run", "--guest", "debian"],
        code: "argument-invalid",
        fragments: &["--guest", "'debian'", "'azurelinux'"],
    },
    UsageError {
        name: "run: invalid second guest",
        arguments: &["run", "--guest", "alpine", "--guest", "debian"],
        code: "argument-invalid",
        fragments: &["--guest", "'debian'"],
    },
    UsageError {
        name: "run: invalid hypervisor",
        arguments: &["run", "--hypervisor", "hyperv"],
        code: "argument-invalid",
        fragments: &["--hypervisor", "'hyperv'"],
    },
    UsageError {
        name: "run: invalid machine",
        arguments: &["run", "--machine", "microvm-v2"],
        code: "argument-invalid",
        fragments: &["--machine", "'microvm-v2'"],
    },
    UsageError {
        name: "run: memory not an integer",
        arguments: &["run", "--memory-mib", "x"],
        code: "argument-invalid",
        fragments: &["--memory-mib", "'x'"],
    },
    UsageError {
        name: "run: memory not a whole number",
        arguments: &["run", "--memory-mib", "1.5"],
        code: "argument-invalid",
        fragments: &["--memory-mib", "'1.5'"],
    },
    UsageError {
        name: "run: memory with a space",
        arguments: &["run", "--memory-mib", "1 2"],
        code: "argument-invalid",
        fragments: &["--memory-mib", "'1 2'"],
    },
    UsageError {
        name: "run: memory with other digits",
        arguments: &["run", "--memory-mib", "\u{661}\u{662}\u{668}x"],
        code: "argument-invalid",
        fragments: &["--memory-mib", "'\u{661}\u{662}\u{668}x'"],
    },
    UsageError {
        name: "run: memory capacity not an integer",
        arguments: &["run", "--memory-capacity-mib", "1e3"],
        code: "argument-invalid",
        fragments: &["--memory-capacity-mib", "'1e3'"],
    },
    UsageError {
        name: "run: restore memory not an integer",
        arguments: &["run", "--restore-memory-mib", ""],
        code: "argument-invalid",
        fragments: &["--restore-memory-mib", "''"],
    },
    UsageError {
        name: "run: processors not an integer",
        arguments: &["run", "--processors", "two"],
        code: "argument-invalid",
        fragments: &["--processors", "'two'"],
    },
    UsageError {
        name: "run: processors not a choice",
        arguments: &["run", "--processors", "3"],
        code: "argument-invalid",
        fragments: &["--processors", "'3'", "'8'"],
    },
    UsageError {
        name: "run: processors converted before the choice",
        arguments: &["run", "--processors", "1_6"],
        code: "argument-invalid",
        fragments: &["--processors", "'16'"],
    },
    UsageError {
        name: "run: processors with a leading zero",
        arguments: &["run", "--processors", "03"],
        code: "argument-invalid",
        fragments: &["--processors", "'3'"],
    },
    UsageError {
        name: "run: negative processors",
        arguments: &["run", "--processors", "-5"],
        code: "argument-invalid",
        fragments: &["--processors", "'-5'"],
    },
    UsageError {
        name: "run: restore processors not an integer",
        arguments: &["run", "--restore-processors", "x"],
        code: "argument-invalid",
        fragments: &["--restore-processors", "'x'"],
    },
    UsageError {
        name: "run: restore processors not a choice",
        arguments: &["run", "--restore-processors", "16"],
        code: "argument-invalid",
        fragments: &["--restore-processors", "'16'"],
    },
    UsageError {
        name: "run: invalid mount owner",
        arguments: &["run", "--mount-owner", "root"],
        code: "argument-invalid",
        fragments: &["--mount-owner", "'root'"],
    },
    UsageError {
        name: "run: invalid network profile",
        arguments: &["run", "--network-profile", "open"],
        code: "argument-invalid",
        fragments: &["--network-profile", "'open'"],
    },
    UsageError {
        name: "run: invalid network egress",
        arguments: &["run", "--network-egress", "block"],
        code: "argument-invalid",
        fragments: &["--network-egress", "'block'"],
    },
    UsageError {
        name: "run: invalid network ingress",
        arguments: &["run", "--network-ingress", "open"],
        code: "argument-invalid",
        fragments: &["--network-ingress", "'open'"],
    },
    UsageError {
        name: "run: invalid host loopback",
        arguments: &["run", "--host-loopback", "yes"],
        code: "argument-invalid",
        fragments: &["--host-loopback", "'yes'"],
    },
    UsageError {
        name: "run: missing value",
        arguments: &["run", "--memory-mib"],
        code: "argument-missing",
        fragments: &["--memory-mib", "one argument"],
    },
    UsageError {
        name: "run: missing path",
        arguments: &["run", "--restore-snapshot"],
        code: "argument-missing",
        fragments: &["--restore-snapshot", "one argument"],
    },
    UsageError {
        name: "run: option as value",
        arguments: &["run", "--cpu-profile", "--dry-run"],
        code: "argument-missing",
        fragments: &["--cpu-profile", "one argument"],
    },
    UsageError {
        name: "run: dash value",
        arguments: &["run", "--cmdline", "-x"],
        code: "argument-missing",
        fragments: &["--cmdline", "one argument"],
    },
    UsageError {
        name: "run: flag with value",
        arguments: &["run", "--dry-run=1"],
        code: "argument-invalid",
        fragments: &["--dry-run", "'1'"],
    },
    UsageError {
        name: "run: unrecognized option",
        arguments: &["run", "--bogus"],
        code: "argument-unrecognized",
        fragments: &["unrecognized arguments: --bogus"],
    },
    UsageError {
        name: "run: unrecognized short option",
        arguments: &["run", "-x"],
        code: "argument-unrecognized",
        fragments: &["unrecognized arguments: -x"],
    },
    UsageError {
        name: "run: unrecognized positional",
        arguments: &["run", "extra"],
        code: "argument-unrecognized",
        fragments: &["unrecognized arguments: extra"],
    },
    UsageError {
        name: "run: unrecognized after separator",
        arguments: &["run", "--", "extra"],
        code: "argument-unrecognized",
        fragments: &["unrecognized arguments: -- extra"],
    },
    UsageError {
        name: "run: unrecognized after options",
        arguments: &["run", "--dry-run", "--guest", "alpine", "extra"],
        code: "argument-unrecognized",
        fragments: &["unrecognized arguments: extra"],
    },
];

#[test]
fn usage_errors_exit_2_and_name_the_option() {
    let root = Root::new();
    let before = root.snapshot();
    for case in USAGE_ERRORS {
        let outcome = root.nvx_rs(case.arguments);
        let fragments: Vec<String> = case
            .fragments
            .iter()
            .map(|fragment| root.expand(fragment))
            .collect();
        let fragments: Vec<&str> = fragments.iter().map(String::as_str).collect();
        outcome.assert_usage_error(case.code, &fragments);
        assert_eq!(root.snapshot(), before, "{}", case.name);
    }
}

/// A command line that asks a parser for its help, and options that the help describes.
struct Help {
    /// The name of the case.
    name: &'static str,
    /// The arguments after `--root ROOT`.
    arguments: &'static [&'static str],
    /// How its usage starts.
    usage: &'static str,
    /// Options that it describes.
    options: &'static [&'static str],
}

/// The command lines that print help, which comes before any value that the parser would refuse
/// after it.
const HELP: [Help; 5] = [
    Help {
        name: "sandbox",
        arguments: &["sandbox", "-h"],
        usage: "usage: nvx-rs sandbox [-h]",
        options: &[
            "--layer ROLE,PATH,EROFS_UUID",
            "--state-dir STATE_DIR",
            "--dry-run",
        ],
    },
    Help {
        name: "sandbox after an operation",
        arguments: &["sandbox", "exec", "--help"],
        usage: "usage: nvx-rs sandbox [-h]",
        options: &["--environment KEY=VALUE", "--outcome-report OUTCOME_REPORT"],
    },
    Help {
        name: "run",
        arguments: &["run", "-h"],
        usage: "usage: nvx-rs run [-h]",
        options: &[
            "--guest {alpine,ubuntu,azurelinux}",
            "--restore-snapshot",
            "--dry-run",
        ],
    },
    Help {
        name: "run after options",
        arguments: &["run", "--guest", "ubuntu", "--help"],
        usage: "usage: nvx-rs run [-h]",
        options: &["--memory-mib", "--processors"],
    },
    Help {
        name: "run before an invalid value",
        arguments: &["run", "--help", "--memory-mib", "x"],
        usage: "usage: nvx-rs run [-h]",
        options: &["--memory-mib"],
    },
];

#[test]
fn help_prints_the_usage_and_the_options_and_exits_0() {
    let root = Root::new();
    for case in &HELP {
        let outcome = root.nvx_rs(case.arguments);
        outcome.assert_success();
        let help = outcome.stdout();
        assert!(help.starts_with(case.usage), "{}: {help}", case.name);
        assert!(
            help.contains("\n  -h, --help ") && help.ends_with(LINE_END),
            "{}: {help}",
            case.name
        );
        for option in case.options {
            assert!(
                help.contains(&format!("  {option}")),
                "{}: {option} is missing: {help}",
                case.name
            );
        }
    }
}

#[test]
fn run_prints_the_help_of_nvx_py_run() {
    let outcome = Root::new().nvx_rs(&["run", "--help"]);
    outcome.assert_success();
    assert_eq!(outcome.stdout(), RUN_HELP.replace('\n', LINE_END));
}

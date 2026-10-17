//! `nvx-rs egress compile PATH`: compiles an egress policy file, as `nvx.py run` and `nvx.py
//! sandbox` compile their `--network-egress-policy-file`, and prints the native OpenVMM rules as
//! a JSON object, `{"allow": [...], "deny": [...]}`, with the rules of each action in the order
//! in which OpenVMM receives them.

use std::path::Path;

use super::args::{Parser, Spec};
use super::{Exit, Result, args, output};
use crate::host::network::policy::compile_policy_file;
use crate::host::python::dumps::{self, Value};

/// The name of the parser in usage messages.
const PROG: &str = "nvx-rs egress";
/// The usage of the parser.
const USAGE: &str = "usage: nvx-rs egress [-h] {compile} ...";
/// The help of the parser.
const HELP: &str = "\
usage: nvx-rs egress [-h] {compile} ...

Compile egress policy files.

positional arguments:
  {compile}
    compile   compile an egress policy file into native OpenVMM rules

options:
  -h, --help  show this help message and exit";
/// The actions, in the order in which usage messages list them.
const ACTIONS: [&str; 1] = ["compile"];
/// The name of the `compile` parser in usage messages.
const COMPILE_PROG: &str = "nvx-rs egress compile";
/// The usage of the `compile` parser.
const COMPILE_USAGE: &str = "usage: nvx-rs egress compile [-h] PATH";
/// The help of the `compile` parser.
const COMPILE_HELP: &str = "\
usage: nvx-rs egress compile [-h] PATH

Compile the egress policy file at PATH, as --network-egress-policy-file does,
and print its native rules as a JSON object: {\"allow\": [...], \"deny\": [...]}.

positional arguments:
  PATH        the egress policy file: at most 1 MiB of JSON with allow and deny
              lists of rules

options:
  -h, --help  show this help message and exit";

/// Parses the arguments of `egress`, and returns the policy file and the arguments that no
/// parser recognizes.
pub(super) fn parse(arguments: &[String]) -> Result<(String, Vec<String>)> {
    let parser = Parser::new(&[], &[Spec::one("action")]);
    let (index, mut extras) = parser
        .split_command(arguments, &mut |_: &'static str, _: Option<&str>| Ok(()))
        .map_err(|stop| Exit::from_stop(stop, PROG, USAGE, HELP))?;
    if arguments[index] != "compile" {
        return Err(Exit::Usage {
            usage: USAGE,
            prog: PROG,
            message: format!(
                "argument action: {}",
                args::invalid_choice(&arguments[index], &ACTIONS)
            ),
        });
    }
    let mut path = String::new();
    let unrecognized = Parser::new(&[], &[Spec::one("PATH")])
        .parse(
            &arguments[index + 1..],
            &mut |_: &'static str, value: Option<&str>| {
                path = value.unwrap_or_default().to_owned();
                Ok(())
            },
        )
        .map_err(|stop| Exit::from_stop(stop, COMPILE_PROG, COMPILE_USAGE, COMPILE_HELP))?;
    extras.extend(unrecognized);
    Ok((path, extras))
}

/// Compiles the policy file at `path` and prints its rules.
pub(super) fn run(path: &str) -> Result {
    let policy = compile_policy_file(Path::new(path))?;
    let rules = |rules: Vec<String>| Value::List(rules.into_iter().map(Value::Str).collect());
    let value = dumps::dict([("allow", rules(policy.allow)), ("deny", rules(policy.deny))]);
    output::print_stdout(&dumps::dumps(&value))
        .map_err(|error| Exit::error(output::stream_error(&error)))
}

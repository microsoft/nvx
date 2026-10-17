//! `nvx-rs egress compile PATH`: compiles an egress policy file, as `nvx.py run` and `nvx.py
//! sandbox` compile their `--network-egress-policy-file`, and prints the native OpenVMM rules as
//! a JSON object, `{"allow": [...], "deny": [...]}`, with the rules of each action in the order
//! in which OpenVMM receives them.

use std::path::Path;

use super::args::{Parser, Spec, Syntax};
use super::{Exit, Result, args, output};
use crate::host::network::policy::compile_policy_file;
use crate::host::python::dumps::{self, Value};

/// The usage and help of the parser.
const SYNTAX: Syntax = Syntax {
    command: "egress",
    optionals: &["[-h]"],
    positionals: &["{compile} ..."],
    help: "\
Compile egress policy files.

positional arguments:
  {compile}
    compile   compile an egress policy file into native OpenVMM rules

options:
  -h, --help  show this help message and exit",
};
/// The actions, in the order in which usage messages list them.
const ACTIONS: [&str; 1] = ["compile"];
/// The usage and help of the `compile` parser.
const COMPILE_SYNTAX: Syntax = Syntax {
    command: "egress compile",
    optionals: &["[-h]"],
    positionals: &["PATH"],
    help: "\
Compile the egress policy file at PATH, as --network-egress-policy-file does,
and print its native rules as a JSON object: {\"allow\": [...], \"deny\": [...]}.

positional arguments:
  PATH        the egress policy file: at most 1 MiB of JSON with allow and deny
              lists of rules

options:
  -h, --help  show this help message and exit",
};

/// Parses the arguments of `egress` for the program `program`, and returns the policy file and
/// the arguments that no parser recognizes.
pub(super) fn parse(arguments: &[String], program: &str) -> Result<(String, Vec<String>)> {
    let parser = Parser::new(&[], &[Spec::one("action")]);
    let (index, mut extras) = parser
        .split_command(arguments, &mut |_: &'static str, _: Option<&str>| Ok(()))
        .map_err(|stop| Exit::from_stop(stop, &SYNTAX, program))?;
    if arguments[index] != "compile" {
        return Err(Exit::usage(
            &SYNTAX,
            program,
            format!(
                "argument action: {}",
                args::invalid_choice(&arguments[index], &ACTIONS)
            ),
        ));
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
        .map_err(|stop| Exit::from_stop(stop, &COMPILE_SYNTAX, program))?;
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

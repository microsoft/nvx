//! Applies `process.env` to a workload.
//!
//! The guest agent starts every workload with the guest's default environment and has no field
//! for another one, so the backend starts the workload through `env`. The agent runs `env` after
//! it has dropped its privileges and `env` replaces itself with the workload, so the entries reach
//! only the workload, which keeps its identity, its cgroup, and its exit status.

use super::SHELL;
use super::protocol::{MAX_ARGUMENT_BYTES, MAX_ARGUMENTS};
use crate::error::{Error, Result};
use crate::model::ProcessSpec;

/// Alpine links this path to BusyBox's `env`.
const ENV: &str = "/usr/bin/env";

/// Shell script that runs its arguments as a command.
const EXEC_SCRIPT: &str = "exec \"$@\"";

/// Returns the arguments that start `argv` in the environment that `process` requests.
///
/// Omitting `env` selects the default environment, as does layering an empty list over it, and
/// neither needs `env`. Entries replace the default environment, unless `inheritDefaultEnv` layers
/// them over it, and an empty list therefore starts the workload with no variables at all.
pub(super) fn apply(process: &ProcessSpec, mut argv: Vec<String>) -> Result<Vec<String>> {
    let Some(entries) = &process.env else {
        return Ok(argv);
    };
    let layered = process.inherit_default_env == Some(true);
    if layered && entries.is_empty() {
        return Ok(argv);
    }
    if let Some(index) = entries
        .iter()
        .position(|entry| entry.len() > MAX_ARGUMENT_BYTES)
    {
        return Err(Error::policy_validation(format!(
            "process.env[{index}] exceeds the {MAX_ARGUMENT_BYTES}-byte limit of the openvmm \
             backend"
        )));
    }

    let mut wrapped = vec![ENV.to_owned()];
    if !layered {
        wrapped.push("-i".to_owned());
    }
    // `--` keeps an entry whose name starts with `-` from being read as an option.
    wrapped.push("--".to_owned());
    wrapped.extend(entries.iter().cloned());
    // `env` reads a program name that contains `=` as one more entry.
    if argv.first().is_some_and(|program| program.contains('=')) {
        wrapped.extend([
            SHELL.to_owned(),
            "-c".to_owned(),
            EXEC_SCRIPT.to_owned(),
            SHELL.to_owned(),
        ]);
    }
    wrapped.append(&mut argv);
    if wrapped.len() > MAX_ARGUMENTS {
        return Err(Error::policy_validation(format!(
            "process.env has {} entries, but the openvmm backend passes them as arguments of \
             {ENV}: it, the entries, and the workload's own arguments need {} of the \
             {MAX_ARGUMENTS} arguments that the guest agent accepts",
            entries.len(),
            wrapped.len()
        )));
    }
    Ok(wrapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;
    use crate::model::ExecRequest;

    fn strings<const N: usize>(words: [&str; N]) -> Vec<String> {
        words.map(str::to_owned).to_vec()
    }

    fn wrap(request: &ExecRequest) -> Result<Vec<String>> {
        apply(&request.process, strings(["/bin/sh", "-c", "env"]))
    }

    #[test]
    fn the_default_environment_needs_no_wrapper() {
        let plain = strings(["/bin/sh", "-c", "env"]);
        for request in [
            ExecRequest::command_line("env"),
            ExecRequest::command_line("env").with_inherit_default_env(true),
            ExecRequest::command_line("env").with_inherit_default_env(false),
            ExecRequest::command_line("env")
                .with_environment(Vec::<String>::new())
                .with_inherit_default_env(true),
        ] {
            assert_eq!(wrap(&request).unwrap(), plain, "{request:?}");
        }
    }

    #[test]
    fn entries_replace_the_default_environment() {
        let request = ExecRequest::command_line("env")
            .with_env("FOO=bar")
            .with_env("EMPTY=");
        assert_eq!(
            wrap(&request).unwrap(),
            [
                "/usr/bin/env",
                "-i",
                "--",
                "FOO=bar",
                "EMPTY=",
                "/bin/sh",
                "-c",
                "env"
            ]
        );
        let request = request.with_inherit_default_env(false);
        assert_eq!(wrap(&request).unwrap()[..3], ["/usr/bin/env", "-i", "--"]);
    }

    #[test]
    fn an_explicitly_empty_environment_clears_the_default_one() {
        let request = ExecRequest::argv(["/usr/bin/env"]).with_environment(Vec::<String>::new());
        assert_eq!(
            apply(&request.process, strings(["/usr/bin/env"])).unwrap(),
            ["/usr/bin/env", "-i", "--", "/usr/bin/env"]
        );
    }

    #[test]
    fn layered_entries_keep_the_default_environment() {
        let request = ExecRequest::command_line("env")
            .with_env("FOO=bar")
            .with_inherit_default_env(true);
        assert_eq!(
            wrap(&request).unwrap(),
            ["/usr/bin/env", "--", "FOO=bar", "/bin/sh", "-c", "env"]
        );
    }

    #[test]
    fn each_entry_is_one_argument_whatever_it_contains() {
        let entries = [
            "GREETING=hello big world",
            "SPACED=  padded  ",
            "QUOTED=\"a\" 'b' $HOME `date` ; | &",
            "MULTILINE=first\nsecond",
            "EQUALS=a=b=c",
            "UNICODE=héllo ☃",
            "-DASHED=1",
            "EMPTY=",
        ];
        let request = ExecRequest::command_line("env").with_environment(entries);
        let wrapped = wrap(&request).unwrap();
        assert_eq!(wrapped[..3], ["/usr/bin/env", "-i", "--"]);
        assert_eq!(wrapped[3..3 + entries.len()], entries);
        assert_eq!(wrapped[3 + entries.len()..], ["/bin/sh", "-c", "env"]);
    }

    #[test]
    fn programs_with_an_equals_sign_run_through_the_shell() {
        let request = ExecRequest::argv(["/opt/a=b/run", "arg"]).with_env("FOO=bar");
        assert_eq!(
            apply(&request.process, strings(["/opt/a=b/run", "arg"])).unwrap(),
            [
                "/usr/bin/env",
                "-i",
                "--",
                "FOO=bar",
                "/bin/sh",
                "-c",
                "exec \"$@\"",
                "/bin/sh",
                "/opt/a=b/run",
                "arg",
            ]
        );
        // A program without one is started directly.
        assert_eq!(
            apply(&request.process, strings(["/opt/run", "a=b"])).unwrap(),
            ["/usr/bin/env", "-i", "--", "FOO=bar", "/opt/run", "a=b"]
        );
    }

    #[test]
    fn entries_and_arguments_share_the_guest_agent_limit() {
        let entries = |count: usize| (0..count).map(|index| format!("V{index}=x"));
        // The wrapper takes `env`, `-i`, and `--`; the shell command line takes three more.
        let fits = ExecRequest::command_line("env").with_environment(entries(MAX_ARGUMENTS - 6));
        assert_eq!(wrap(&fits).unwrap().len(), MAX_ARGUMENTS);
        let beyond = ExecRequest::command_line("env").with_environment(entries(MAX_ARGUMENTS - 5));
        let error = wrap(&beyond).unwrap_err();
        assert_eq!(error.code(), ErrorCode::PolicyValidation);
        assert!(
            error.message().contains("process.env has 59 entries"),
            "{error}"
        );
        // Layering needs no `-i`.
        let layered = ExecRequest::command_line("env")
            .with_environment(entries(MAX_ARGUMENTS - 5))
            .with_inherit_default_env(true);
        assert_eq!(wrap(&layered).unwrap().len(), MAX_ARGUMENTS);
    }

    #[test]
    fn entries_have_the_argument_size_limit() {
        let entry = |length: usize| format!("A={}", "x".repeat(length - 2));
        let fits = ExecRequest::command_line("env").with_env(entry(MAX_ARGUMENT_BYTES));
        wrap(&fits).unwrap();
        let beyond = ExecRequest::command_line("env")
            .with_env("A=1")
            .with_env(entry(MAX_ARGUMENT_BYTES + 1));
        let error = wrap(&beyond).unwrap_err();
        assert_eq!(error.code(), ErrorCode::PolicyValidation);
        assert!(error.message().contains("process.env[1]"), "{error}");
    }
}

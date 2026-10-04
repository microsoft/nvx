//! Applies `process.env` to a workload.
//!
//! The guest agent starts every workload with the guest's default environment and has no field
//! for another one, so the backend starts the workload through `env`. The agent runs `env` after
//! it has dropped its privileges and `env` replaces itself with the workload, so the entries reach
//! only the workload, which keeps its identity, its cgroup, and its exit status.
//!
//! Nothing may run between `env` and the workload. A shell there would export `PWD`, `OLDPWD`, and
//! `SHLVL` of its own and rewrite entries with those names, so a program that needs a shell, such
//! as one that must enter a working directory first, is started by a shell *in front of* `env`.

use super::protocol::{MAX_ARGUMENT_BYTES, MAX_ARGUMENTS};
use crate::error::{Error, Result};
use crate::model::ProcessSpec;

/// Alpine links this path to BusyBox's `env`.
const ENV: &str = "/usr/bin/env";

/// The launcher that the guest agent starts every workload with. Given a program and no options,
/// it executes the program and leaves the environment alone.
const SETPRIV: &str = "/bin/setpriv";

/// Returns the arguments that start `argv` in the environment that `process` requests.
///
/// Omitting `env` selects the default environment, as does layering an empty list over it, and
/// neither needs `env`. Entries replace the default environment, unless `inheritDefaultEnv` layers
/// them over it, and an empty list therefore starts the workload with no variables at all.
///
/// `reserved` is the number of arguments that the caller puts in front of the result. They count
/// toward the guest agent's limit with it.
pub(super) fn apply(
    process: &ProcessSpec,
    mut argv: Vec<String>,
    reserved: usize,
) -> Result<Vec<String>> {
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
    // `env` reads a program name that contains `=` as one more entry, so another launcher has to
    // start such a program. A shell would change the environment, but `setpriv` does not.
    if argv.first().is_some_and(|program| program.contains('=')) {
        wrapped.push(SETPRIV.to_owned());
    }
    wrapped.append(&mut argv);
    let total = reserved + wrapped.len();
    if total > MAX_ARGUMENTS {
        return Err(Error::policy_validation(format!(
            "process.env has {} entries, but the openvmm backend passes them as arguments of \
             {ENV}, so the request needs {total} of the {MAX_ARGUMENTS} arguments that the guest \
             agent accepts",
            entries.len(),
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
        apply(&request.process, strings(["/bin/sh", "-c", "env"]), 0)
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
            apply(&request.process, strings(["/usr/bin/env"]), 0).unwrap(),
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
    fn programs_with_an_equals_sign_start_through_setpriv_not_a_shell() {
        let request = ExecRequest::argv(["/opt/a=b/run", "arg"]).with_env("FOO=bar");
        assert_eq!(
            apply(&request.process, strings(["/opt/a=b/run", "arg"]), 0).unwrap(),
            [
                "/usr/bin/env",
                "-i",
                "--",
                "FOO=bar",
                "/bin/setpriv",
                "/opt/a=b/run",
                "arg",
            ]
        );
        let layered = request.with_inherit_default_env(true);
        assert_eq!(
            apply(&layered.process, strings(["/opt/a=b/run"]), 0).unwrap(),
            [
                "/usr/bin/env",
                "--",
                "FOO=bar",
                "/bin/setpriv",
                "/opt/a=b/run"
            ]
        );
        // A program without one is started directly, whatever its arguments contain.
        let direct = ExecRequest::argv(["/opt/run"]).with_env("FOO=bar");
        assert_eq!(
            apply(&direct.process, strings(["/opt/run", "a=b"]), 0).unwrap(),
            ["/usr/bin/env", "-i", "--", "FOO=bar", "/opt/run", "a=b"]
        );
        // Without entries, nothing needs the launcher.
        let plain = ExecRequest::argv(["/opt/a=b/run"]);
        assert_eq!(
            apply(&plain.process, strings(["/opt/a=b/run"]), 0).unwrap(),
            ["/opt/a=b/run"]
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
        assert!(error.message().contains("needs 65 of the 64"), "{error}");
        // Layering needs no `-i`.
        let layered = ExecRequest::command_line("env")
            .with_environment(entries(MAX_ARGUMENTS - 5))
            .with_inherit_default_env(true);
        assert_eq!(wrap(&layered).unwrap().len(), MAX_ARGUMENTS);
    }

    #[test]
    fn arguments_in_front_of_the_wrapper_count_toward_the_limit() {
        let entries = |count: usize| (0..count).map(|index| format!("V{index}=x"));
        let program = || strings(["/bin/true"]);
        // Five arguments in front, then the wrapper's three, the entries, and the program's one.
        let fits = ExecRequest::argv(["/bin/true"]).with_environment(entries(MAX_ARGUMENTS - 9));
        let wrapped = apply(&fits.process, program(), 5).unwrap();
        assert_eq!(5 + wrapped.len(), MAX_ARGUMENTS);
        let beyond = ExecRequest::argv(["/bin/true"]).with_environment(entries(MAX_ARGUMENTS - 8));
        let error = apply(&beyond.process, program(), 5).unwrap_err();
        assert_eq!(error.code(), ErrorCode::PolicyValidation);
        assert!(error.message().contains("needs 65 of the 64"), "{error}");
        // So does the launcher of a program whose name contains `=`.
        let launched = ExecRequest::argv(["/opt/a=b"]).with_environment(entries(MAX_ARGUMENTS - 5));
        assert_eq!(
            apply(&launched.process, strings(["/opt/a=b"]), 0)
                .unwrap()
                .len(),
            MAX_ARGUMENTS
        );
        let launched = ExecRequest::argv(["/opt/a=b"]).with_environment(entries(MAX_ARGUMENTS - 4));
        let error = apply(&launched.process, strings(["/opt/a=b"]), 0).unwrap_err();
        assert!(error.message().contains("needs 65 of the 64"), "{error}");
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

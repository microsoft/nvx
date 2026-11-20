use super::*;
use std::process::ExitStatus;

#[test]
fn messages_print_on_one_line() {
    assert!(matches!(one_line("plain"), Cow::Borrowed("plain")));
    assert_eq!(
        one_line("not found: /tmp/\nerror[unsupported]: openvmm"),
        "not found: /tmp/\\nerror[unsupported]: openvmm"
    );
    assert_eq!(one_line("a\r\nb\rc"), "a\\r\\nb\\rc");
}

#[test]
fn failed_programs_report_their_standard_error_or_their_status() {
    #[cfg(unix)]
    fn status(code: i32) -> ExitStatus {
        std::os::unix::process::ExitStatusExt::from_raw(code << 8)
    }
    #[cfg(windows)]
    fn status(code: i32) -> ExitStatus {
        std::os::windows::process::ExitStatusExt::from_raw(code as u32)
    }
    fn failed(stderr: &[u8]) -> Output {
        Output {
            status: status(2),
            stdout: b"ignored".to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    assert_eq!(
        failure_detail("tar", &failed(b"\n  tar: no such file\r\n")),
        "tar: no such file"
    );
    assert_eq!(
        failure_detail("git", &failed(b"bad \xff byte")),
        "bad \u{fffd} byte"
    );
    let exited = format!("curl exited with {}", status(2));
    assert_eq!(failure_detail("curl", &failed(b"")), exited);
    assert_eq!(failure_detail("curl", &failed(b" \r\n\t")), exited);
}

#[test]
fn usages_are_one_line_and_helps_list_every_option() {
    for (syntax, options) in [
        (&SYNTAX, &OPTIONS[..]),
        (&artifacts::SYNTAX, &[][..]),
        (&download::SYNTAX, &download::OPTIONS[..]),
        (&egress::SYNTAX, &[][..]),
        (&egress::COMPILE_SYNTAX, &[][..]),
        (&run::SYNTAX, &run::options()[..]),
        (&sandbox::tests::SYNTAX, &sandbox::tests::OPTIONS[..]),
    ] {
        let usage = syntax.usage();
        assert!(usage.starts_with("usage: nvx"), "{usage}");
        assert!(!usage.contains('\n'), "{usage}");
        syntax.assert_lists(options);
    }
    assert_eq!(run::SYNTAX.usage(), "usage: nvx run [OPTIONS]");
    assert_eq!(
        sandbox::tests::SYNTAX.usage(),
        "usage: nvx sandbox [{run,provision,start,exec,stop,deprovision}] [OPTIONS]"
    );
}

#[test]
fn the_run_help_states_the_memory_default_of_each_guest() {
    let help = run::SYNTAX
        .help
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for guest in crate::host::artifacts::Guest::ALL {
        let default = format!("{} {}", guest.name(), guest.default_memory_mib());
        assert!(help.contains(&default), "{default}: {help}");
    }
}

#[test]
fn os_errors_read_as_rust_writes_them() {
    let error = std::fs::metadata("definitely/missing/path").unwrap_err();
    let expected = error.to_string();
    let code = error.raw_os_error().unwrap();
    let exit = Exit::from_os_error(error);
    assert_eq!(exit.code(), Some(ErrorCode::Io));
    let message = exit.message().unwrap();
    assert_eq!(message, expected);
    assert!(
        message.ends_with(&format!("(os error {code})")),
        "{message}"
    );
    assert!(!message.contains("missing"), "{message}");
    let error = std::io::Error::other("closed");
    assert_eq!(Exit::from_os_error(error).message(), Some("closed"));
    let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    let exit = io_error(ErrorCode::Io, &denied, Path::new("state\n/config.json"));
    assert_eq!(
        exit.message(),
        Some(format!("\"state\\n/config.json\": {denied}").as_str())
    );
}

#[test]
fn errors_keep_the_codes_of_library_errors() {
    let exit = Exit::from(crate::Error::new(
        ErrorCode::SandboxBusy,
        "another lifecycle transition is still in progress",
    ));
    assert_eq!(exit.code(), Some(ErrorCode::SandboxBusy));
    assert_eq!(
        exit.message(),
        Some("another lifecycle transition is still in progress")
    );
    assert_eq!(Exit::EndpointClosed.code(), Some(ErrorCode::EndpointClosed));
    assert_eq!(Exit::Interrupted.code(), None);
    assert_eq!(Exit::Status(3).code(), None);
}

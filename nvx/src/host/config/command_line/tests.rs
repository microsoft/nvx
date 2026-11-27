//! Tests of the Windows process command line's length, as the Windows platform quotes it, and of
//! its fit in the limit.

use super::*;
use crate::ErrorKind;

#[test]
fn process_command_lines_are_measured_as_windows_quotes_them() {
    let cases: [(&str, &[&str], &str); 8] = [
        (
            r"C:\Program Files\openvmm.exe",
            &["--a", "b c", ""],
            r#""C:\Program Files\openvmm.exe" --a "b c" """#,
        ),
        ("x", &[r#"say "hi""#], r#""x" "say \"hi\"""#),
        ("x", &[r#"a"b"#], r#""x" a\"b"#),
        ("x", &[r"dir\ with\"], r#""x" "dir\ with\\""#),
        ("x", &[r"a\\b", "é"], r#""x" a\\b é"#),
        ("", &[""], r#""" """#),
        ("x", &["a\tb"], "\"x\" \"a\tb\""),
        ("x", &["\u{1f680}"], "\"x\" \u{1f680}"),
    ];
    for (program, arguments, line) in cases {
        let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
        assert_eq!(
            process_command_line_units(OsStr::new(program), &arguments),
            line.encode_utf16().count() + 1,
            "{line}"
        );
    }
}

#[test]
fn a_process_command_line_fits_while_it_leaves_its_reserve() {
    // Any program and arguments, such as an adapter's: a command line of the limit fits,
    // one more code unit does not, and a reserve takes its part of the limit.
    let arguments: Vec<OsString> = ["--a", "b c"].map(OsString::from).into();
    let base = process_command_line_units(OsStr::new("/o"), &arguments);
    let program = |extra: usize| OsString::from(format!("/o{}", "o".repeat(extra)));
    let fitting = program(PROCESS_COMMAND_LINE_LIMIT - base);
    assert_eq!(
        process_command_line_units(&fitting, &arguments),
        PROCESS_COMMAND_LINE_LIMIT
    );
    fit_process_command_line(&fitting, &arguments, 0).unwrap();
    let held = fit_process_command_line(&fitting, &arguments, 1).unwrap_err();
    assert_eq!(held.kind(), ErrorKind::Unsupported);
    assert!(
        held.to_string()
            .contains("would take 32767 of the 32767 characters"),
        "{held}"
    );
    assert!(
        held.to_string().contains("with 1 held in reserve"),
        "{held}"
    );
    let longer = program(PROCESS_COMMAND_LINE_LIMIT - base + 1);
    let error = fit_process_command_line(&longer, &arguments, 0).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    // A reserve past the limit leaves no room for any command line.
    assert!(fit_process_command_line(OsStr::new("o"), &[], usize::MAX).is_err());
    // The arguments alone, which a plan renders, take what they take on the command line.
    let alone = process_command_line_units(OsStr::new(""), &arguments)
        - process_command_line_units(OsStr::new(""), &[]);
    let reserve = PROCESS_COMMAND_LINE_LIMIT - alone;
    fit_arguments(&arguments, reserve).unwrap();
    let held = fit_arguments(&arguments, reserve + 1).unwrap_err();
    assert!(
        held.to_string()
            .contains("held in reserve for the rest of the launch"),
        "{held}"
    );
}

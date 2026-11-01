use super::*;

#[test]
fn windows_command_lines_quote_as_the_c_runtime_reads() {
    let cases: [(&[&str], &str); 9] = [
        (&["a", "b c", ""], "a \"b c\" \"\""),
        (&["a\tb"], "\"a\tb\""),
        (&[r#"say "hi""#], r#""say \"hi\"""#),
        (&[r#"a"b"#], r#"a\"b"#),
        (&[r"a\\b"], r"a\\b"),
        (&[r#"a\"b"#], r#"a\\\"b"#),
        (&[r"dir\ with\"], r#""dir\ with\\""#),
        (&[r"C:\path\"], r"C:\path\"),
        (&[r#"x\\" y"#], r#""x\\\\\" y""#),
    ];
    for (arguments, expected) in cases {
        assert_eq!(windows_command_line(arguments), expected, "{arguments:?}");
    }
}

#[test]
fn shell_quoting_quotes_what_a_shell_would_expand() {
    let cases: [(&str, &str); 7] = [
        ("", "''"),
        ("plain-_@%+=:,./0", "plain-_@%+=:,./0"),
        ("a b", "'a b'"),
        ("it's", "'it'\"'\"'s'"),
        ("$HOME", "'$HOME'"),
        ("é", "'é'"),
        ("a\nb", "'a\nb'"),
    ];
    for (text, expected) in cases {
        assert_eq!(shell_quote(text), expected, "{text:?}");
    }
    assert_eq!(
        shell_command_line(&["/bin/echo", "a b", ""]),
        "/bin/echo 'a b' ''"
    );
}

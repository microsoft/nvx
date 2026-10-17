//! How Python quotes command lines for display: `subprocess.list2cmdline` on Windows, and
//! `shlex.join` elsewhere.
//!
//! `nvx.py` prints the OpenVMM command that it runs as `>> COMMAND`, quoted with
//! [`list2cmdline`] on Windows and [`shlex_join`] on other hosts; [`format_command`] picks the
//! one of this host.

/// Returns `subprocess.list2cmdline(arguments)`: the arguments joined by spaces, as the
/// Microsoft C runtime splits them again.
///
/// An argument that is empty or holds a space or a tab is quoted. Backslashes are literal except
/// before a quote, where they are doubled and the quote escaped, and before the closing quote of
/// a quoted argument, where they are doubled.
pub(crate) fn list2cmdline<S: AsRef<str>>(arguments: &[S]) -> String {
    let mut result = String::new();
    for argument in arguments {
        let argument = argument.as_ref();
        if !result.is_empty() {
            result.push(' ');
        }
        let quoted = argument.is_empty() || argument.contains([' ', '\t']);
        if quoted {
            result.push('"');
        }
        let mut backslashes = 0;
        for character in argument.chars() {
            match character {
                '\\' => backslashes += 1,
                '"' => {
                    result.extend(std::iter::repeat_n('\\', backslashes * 2));
                    backslashes = 0;
                    result.push_str("\\\"");
                }
                _ => {
                    result.extend(std::iter::repeat_n('\\', backslashes));
                    backslashes = 0;
                    result.push(character);
                }
            }
        }
        result.extend(std::iter::repeat_n('\\', backslashes));
        if quoted {
            result.extend(std::iter::repeat_n('\\', backslashes));
            result.push('"');
        }
    }
    result
}

/// Returns `shlex.quote(text)`: `text` itself if it is a nonempty run of the characters that a
/// POSIX shell keeps as they are, `[A-Za-z0-9_@%+=:,./-]`, and otherwise `text` in single
/// quotes, with each single quote written as `'"'"'`.
pub(crate) fn shlex_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".to_owned();
    }
    let safe = |byte: u8| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte);
    if text.bytes().all(safe) {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// Returns `shlex.join(arguments)`: each argument quoted with [`shlex_quote`], joined by spaces.
pub(crate) fn shlex_join<S: AsRef<str>>(arguments: &[S]) -> String {
    arguments
        .iter()
        .map(|argument| shlex_quote(argument.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns `_format_command(arguments)` of `nvx.py`: [`list2cmdline`] on Windows, and
/// [`shlex_join`] on other hosts.
pub(crate) fn format_command<S: AsRef<str>>(arguments: &[S]) -> String {
    if cfg!(windows) {
        list2cmdline(arguments)
    } else {
        shlex_join(arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list2cmdline_quotes_as_the_c_runtime_reads() {
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
            assert_eq!(list2cmdline(arguments), expected, "{arguments:?}");
        }
    }

    #[test]
    fn shlex_quotes_what_a_shell_would_expand() {
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
            assert_eq!(shlex_quote(text), expected, "{text:?}");
        }
        assert_eq!(shlex_join(&["/bin/echo", "a b", ""]), "/bin/echo 'a b' ''");
    }
}

//! How the CLI quotes the commands that it prints as `>> COMMAND`, such as the OpenVMM command
//! that `run` runs: as the Microsoft C runtime splits a command line on Windows, with
//! [`windows_command_line`], and as a POSIX shell splits one elsewhere, with
//! [`shell_command_line`]. [`format_command`] picks the one of this host.

/// Returns `arguments` joined by spaces, quoted so that the Microsoft C runtime splits them back
/// into `arguments`.
///
/// An argument that is empty or holds a space or a tab is quoted. Backslashes are literal except
/// before a quote, where they are doubled and the quote escaped, and before the closing quote of
/// a quoted argument, where they are doubled.
pub(crate) fn windows_command_line<S: AsRef<str>>(arguments: &[S]) -> String {
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

/// Returns `text` quoted for a POSIX shell: `text` itself if it is a nonempty run of the
/// characters that a shell keeps as they are, `[A-Za-z0-9_@%+=:,./-]`, and otherwise `text` in
/// single quotes, with each single quote written as `'"'"'`.
pub(crate) fn shell_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".to_owned();
    }
    let safe = |byte: u8| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte);
    if text.bytes().all(safe) {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// Returns `arguments` quoted with [`shell_quote`], joined by spaces.
pub(crate) fn shell_command_line<S: AsRef<str>>(arguments: &[S]) -> String {
    arguments
        .iter()
        .map(|argument| shell_quote(argument.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns `arguments` as the CLI prints a command: [`windows_command_line`] on Windows, and
/// [`shell_command_line`] on other hosts.
pub(crate) fn format_command<S: AsRef<str>>(arguments: &[S]) -> String {
    if cfg!(windows) {
        windows_command_line(arguments)
    } else {
        shell_command_line(arguments)
    }
}

#[cfg(test)]
mod tests;

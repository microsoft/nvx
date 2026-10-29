//! A model of how Linux 6.18 reads its command line, for the tests: a transcription of the C code
//! with which the kernel splits the command line into parameters and matches them to their
//! handlers.
//!
//! - [`isspace`] is the `isspace` of `include/linux/ctype.h`, the `_S` class of the `_ctype`
//!   table of `lib/ctype.c`, and [`skip_spaces`] is the `skip_spaces` of `lib/string_helpers.c`.
//! - [`next_arg`] is the `next_arg` of `lib/cmdline.c`, which splits the next parameter off a
//!   NUL-terminated buffer in place: it writes NULs where the parameter, its name, and its quotes
//!   end.
//! - [`read`] runs the loop of `parse_args` of `kernel/params.c` on a NUL-terminated copy of a
//!   command line: it skips the leading whitespace, calls `next_arg` until the text ends, and
//!   stops at a `--` without a value, which leaves the rest of the text to init.
//! - [`parameqn`] and [`parameq`] are those of `kernel/params.c`, which compare names with
//!   dashes and underscores alike, and [`line`] is the text of a parameter as the
//!   `repair_env_string` of `init/main.c` restores it before `obsolete_checksetup` matches it with
//!   the `__setup` handlers.
//!
//! [`parameters`](super::parameters) reads a command line in place, without a copy. The tests
//! check it against this model, and read with the model the command lines that NVX renders, to
//! check what the guest's kernel makes of them.

/// What the kernel reads from a command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reading {
    /// The parameters that `parse_args` passes to their handlers, in order: the name of each, and
    /// its value, if it has one.
    pub(crate) parameters: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// The text after the `--` at which `parse_args` stopped, which the kernel leaves to init, or
    /// `None` if it read the command line to its end.
    pub(crate) rest: Option<Vec<u8>>,
}

impl Reading {
    /// Returns the text of each parameter, as [`line`] restores it.
    pub(crate) fn lines(&self) -> Vec<Vec<u8>> {
        self.parameters
            .iter()
            .map(|(name, value)| line(name, value.as_deref()))
            .collect()
    }
}

/// Returns what the kernel reads from `command_line`, which `parse_args` splits in place as a C
/// string: up to its first NUL.
pub(crate) fn read(command_line: &[u8]) -> Reading {
    let mut buffer = command_line.to_vec();
    buffer.push(0);
    let mut reading = Reading::default();
    // Chew leading spaces
    let mut args = skip_spaces(&buffer, 0);
    while buffer[args] != 0 {
        let (next, param, val) = next_arg(&mut buffer, args);
        args = next;
        let param = string(&buffer, param);
        let val = val.map(|val| string(&buffer, val));
        // Stop at --
        if val.is_none() && param == b"--" {
            reading.rest = Some(string(&buffer, args));
            return reading;
        }
        reading.parameters.push((param, val));
    }
    reading
}

/// Splits the parameter that starts at `args` off `buffer`, a NUL-terminated command line, as
/// `next_arg` does: returns where the next parameter starts, after any whitespace, and where the
/// parameter's name and its value, if it has one, start, each a NUL-terminated string in
/// `buffer`.
pub(crate) fn next_arg(buffer: &mut [u8], mut args: usize) -> (usize, usize, Option<usize>) {
    let mut equals = 0;
    let mut in_quote = false;
    let mut quoted = false;

    if buffer[args] == b'"' {
        args += 1;
        in_quote = true;
        quoted = true;
    }

    let mut i = 0;
    while buffer[args + i] != 0 {
        if isspace(buffer[args + i]) && !in_quote {
            break;
        }
        if equals == 0 && buffer[args + i] == b'=' {
            equals = i;
        }
        if buffer[args + i] == b'"' {
            in_quote = !in_quote;
        }
        i += 1;
    }

    let param = args;
    let val = if equals == 0 {
        None
    } else {
        buffer[args + equals] = 0;
        let mut val = args + equals + 1;

        // Don't include quotes in value.
        if buffer[val] == b'"' {
            val += 1;
            if buffer[args + i - 1] == b'"' {
                buffer[args + i - 1] = 0;
            }
        }
        Some(val)
    };
    if quoted && i > 0 && buffer[args + i - 1] == b'"' {
        buffer[args + i - 1] = 0;
    }

    let next = if buffer[args + i] != 0 {
        buffer[args + i] = 0;
        args + i + 1
    } else {
        args + i
    };

    // Chew up trailing spaces.
    (skip_spaces(buffer, next), param, val)
}

/// Returns where the first byte at or after `index` in `buffer`, a NUL-terminated string, that is
/// not whitespace is, as `skip_spaces` does.
pub(crate) fn skip_spaces(buffer: &[u8], mut index: usize) -> usize {
    while isspace(buffer[index]) {
        index += 1;
    }
    index
}

/// Returns whether `c` is whitespace, as `isspace` tells: whether the `_ctype` table puts it in
/// the `_S` class.
pub(crate) fn isspace(c: u8) -> bool {
    /// The bytes of the `_S` class: the tab, the newline, the vertical tab, the form feed, and the
    /// carriage return, 9 to 13; the space, 32; and Latin-1's no-break space, 160.
    const S: [u8; 7] = [9, 10, 11, 12, 13, 32, 160];
    S.contains(&c)
}

/// Returns whether the first `n` bytes of the strings `a` and `b` match, as `parameqn` compares
/// them: byte for byte, with dashes and underscores alike. As a C string does, a string reads as
/// NUL past its end.
pub(crate) fn parameqn(a: &[u8], b: &[u8], n: usize) -> bool {
    let at = |string: &[u8], i: usize| string.get(i).copied().unwrap_or(0);
    (0..n).all(|i| dash2underscore(at(a, i)) == dash2underscore(at(b, i)))
}

/// Returns whether the strings `a` and `b` match, as `parameq` compares them: [`parameqn`] over
/// `a` and its NUL.
pub(crate) fn parameq(a: &[u8], b: &[u8]) -> bool {
    parameqn(a, b, a.len() + 1)
}

/// Returns `c`, or an underscore for a dash, as `dash2underscore` does.
fn dash2underscore(c: u8) -> u8 {
    if c == b'-' { b'_' } else { c }
}

/// Returns the text of the parameter `name` with `value`, as `repair_env_string` restores it for
/// the handlers that match a parameter by its text: `NAME=VALUE`, without the quotes that
/// `next_arg` dropped, or `NAME` alone.
pub(crate) fn line(name: &[u8], value: Option<&[u8]>) -> Vec<u8> {
    let mut line = name.to_vec();
    if let Some(value) = value {
        line.push(b'=');
        line.extend_from_slice(value);
    }
    line
}

/// Returns the NUL-terminated string that starts at `index` in `buffer`.
fn string(buffer: &[u8], index: usize) -> Vec<u8> {
    buffer[index..]
        .iter()
        .take_while(|&&byte| byte != 0)
        .copied()
        .collect()
}

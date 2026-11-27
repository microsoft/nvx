//! The Windows process command line that runs OpenVMM, as [`PROCESS_COMMAND_LINE_LIMIT`] bounds
//! it on every host: what its arguments may hold, how long the Windows platform's quoting makes
//! it, and whether it fits.

use std::ffi::{OsStr, OsString};

use crate::error::{Error, Result};

/// Longest Windows process command line, in UTF-16 code units, including the terminating NUL.
///
/// OpenVMM's command line is held to it on every host, so a policy behaves alike on all of them.
pub const PROCESS_COMMAND_LINE_LIMIT: usize = 32_767;

/// Checks OpenVMM arguments for NULs that cannot be passed to a process.
pub(crate) fn check_argument_nuls(arguments: &[OsString]) -> Result<()> {
    if arguments
        .iter()
        .any(|argument| argument.to_string_lossy().contains('\0'))
    {
        return Err(Error::unsupported(
            "OpenVMM's arguments must not contain a NUL",
        ));
    }
    Ok(())
}

/// Returns the length in UTF-16 code units, including the terminating NUL, of the Windows command
/// line that runs `program` with `arguments`, quoted the way the Windows platform quotes it: the
/// program always in quotes, and each argument in quotes if it is empty or holds a space or a
/// tab, with a backslash before each quote that it holds and a backslash before each backslash
/// that precedes a quote, its closing quote included, and one space between them.
///
/// It is the length that [`PROCESS_COMMAND_LINE_LIMIT`] bounds, whatever the host:
/// [`fit_process_command_line`] checks it.
pub fn process_command_line_units(program: &OsStr, arguments: &[OsString]) -> usize {
    argument_units(program, true) + arguments_units(arguments)
}

/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) unless the Windows command
/// line that runs `program`, such as OpenVMM, with `arguments`, as [`process_command_line_units`]
/// counts it, leaves `reserve` code units of [`PROCESS_COMMAND_LINE_LIMIT`] unused: none for the
/// complete command line of a launch about to spawn OpenVMM, and some for what may change before
/// then, such as the paths of the artifacts.
///
/// The error says how long the command line is, and how to shorten it.
pub fn fit_process_command_line(
    program: &OsStr,
    arguments: &[OsString],
    reserve: usize,
) -> Result<()> {
    let length = process_command_line_units(program, arguments);
    let held = if reserve == 0 {
        String::new()
    } else {
        format!(", with {reserve} held in reserve")
    };
    fit("OpenVMM's command line", length, reserve, &held)
}

/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) unless OpenVMM's
/// `arguments`, which a plan renders, without the program and the rest of the launch, leave
/// `reserve` code units of [`PROCESS_COMMAND_LINE_LIMIT`] to that rest.
pub(super) fn fit_arguments(arguments: &[OsString], reserve: usize) -> Result<()> {
    let held = if reserve == 0 {
        String::new()
    } else {
        format!(", with {reserve} held in reserve for the rest of the launch")
    };
    fit(
        "OpenVMM's arguments",
        arguments_units(arguments),
        reserve,
        &held,
    )
}

/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), saying that `subject`
/// would take `length` code units, `held` describing `reserve`, unless they leave `reserve` of
/// [`PROCESS_COMMAND_LINE_LIMIT`] unused.
fn fit(subject: &str, length: usize, reserve: usize, held: &str) -> Result<()> {
    if length.saturating_add(reserve) <= PROCESS_COMMAND_LINE_LIMIT {
        return Ok(());
    }
    Err(Error::unsupported(format!(
        "{subject} would take {length} of the {PROCESS_COMMAND_LINE_LIMIT} characters that a \
         Windows process command line allows{held}; map, hide, or allow fewer or shorter \
         filesystem paths, or use fewer network rules"
    )))
}

/// Returns the length in UTF-16 code units of `arguments` on a Windows command line, quoted as
/// [`process_command_line_units`] quotes them, each with the space before it.
fn arguments_units(arguments: &[OsString]) -> usize {
    arguments
        .iter()
        .map(|argument| argument_units(argument, false))
        .sum()
}

/// Returns the length in UTF-16 code units of `argument` on a Windows command line, quoted as
/// [`process_command_line_units`] quotes it, always if it is the `program`, with one separating
/// space, or the terminating NUL after the last argument.
fn argument_units(argument: &OsStr, program: bool) -> usize {
    let text = argument.to_string_lossy();
    let quoted = program || text.is_empty() || text.contains([' ', '\t']);
    let mut length = 1 + text.encode_utf16().count() + if quoted { 2 } else { 0 };
    let mut backslashes = 0;
    for character in text.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                length += backslashes + 1;
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
    }
    if quoted {
        length += backslashes;
    }
    length
}

#[cfg(test)]
mod tests;

//! `EXEC` payloads: the workload that an [`APP_EXEC`](super::APP_EXEC) request asks the guest agent
//! to run.
//!
//! A payload starts with the timeout in milliseconds (`u32`), the number of arguments (`u16`), and
//! an extension field (`u16`). A legacy payload, whose extension is zero, continues with the
//! arguments. An extended payload, whose extension is [`EXEC_EXTENDED`], continues with its flags
//! (`u16`), the number of environment entries (`u16`), and the length of the working directory
//! (`u32`), followed by the arguments, the working directory, and the environment entries. Each
//! argument and entry is its length (`u32`) followed by its bytes.

use std::collections::BTreeSet;

use super::{APP_HEADER_LEN, OUTER_MAX_PAYLOAD, ProtocolError, u16_at, u32_at, violation};

/// Largest number of workload arguments, including the program.
pub const MAX_ARGUMENTS: usize = 64;
/// Largest encoded size of one workload argument, working directory, or environment entry.
pub const MAX_ARGUMENT_BYTES: usize = 4096;
/// Largest working directory that hosts send: Linux's `PATH_MAX` without the terminating NUL, one
/// byte below the agent's bound.
pub const MAX_CWD_BYTES: usize = 4095;
/// Largest working directory that the C guest decoder accepts. Unlike host paths, the guest's
/// wire bound includes all 4096 non-NUL bytes because its terminating NUL is allocated separately.
pub const MAX_GUEST_CWD_BYTES: usize = MAX_ARGUMENT_BYTES;
/// Largest number of workload environment entries.
pub const MAX_ENVIRONMENT: usize = 256;
/// Largest workload timeout the guest agent accepts: any value of the exec request's 32-bit
/// timeout field, which is also the largest `process.timeout` that MXC's schema allows.
pub const MAX_TIMEOUT_MS: u32 = u32::MAX;
/// Largest combined stdout and stderr volume the guest agent forwards for one execution.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Extension field of a payload that carries flags, a working directory, and an environment.
pub const EXEC_EXTENDED: u16 = 1;
/// Flag of an extended payload that carries a working directory.
pub const EXEC_CWD_PRESENT: u16 = 1 << 0;
/// Flag of an extended payload that carries an explicit environment.
pub const EXEC_ENVIRONMENT_PRESENT: u16 = 1 << 1;
/// Flag of an extended payload whose environment entries layer over the default environment.
pub const EXEC_INHERIT_DEFAULT_ENV: u16 = 1 << 2;

/// The environment that the guest agent gives a workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadEnvironment<'a> {
    /// The guest's default environment.
    Default,
    /// Exactly these `KEY=VALUE` entries; an empty slice is an empty environment.
    Replaced(&'a [String]),
    /// These entries layered over the default environment, each replacing the default variable
    /// of the same name.
    Layered(&'a [String]),
}

/// A workload that an `EXEC` request asks the guest agent to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workload<'a> {
    /// The program, an absolute guest path, followed by its arguments.
    pub argv: Vec<String>,
    /// Timeout in milliseconds; zero disables it.
    pub timeout_ms: u32,
    /// Absolute guest directory that the guest agent enters before it starts the program;
    /// `None` selects `/`.
    pub cwd: Option<&'a str>,
    /// The program's environment.
    pub environment: WorkloadEnvironment<'a>,
}

/// Encodes the payload of an `EXEC` request.
///
/// The guest agent requires 1 to [`MAX_ARGUMENTS`] non-empty arguments without NUL bytes, each
/// at most [`MAX_ARGUMENT_BYTES`] long, an absolute program path, an absolute working directory
/// of at most [`MAX_ARGUMENT_BYTES`] without NUL bytes, and at most [`MAX_ENVIRONMENT`]
/// `KEY=VALUE` environment entries with unique names. It accepts any timeout up to
/// [`MAX_TIMEOUT_MS`], and zero disables it. A workload without a working directory or an
/// explicit environment keeps the legacy payload.
pub fn encode_exec_payload(workload: &Workload<'_>) -> Result<Vec<u8>, ProtocolError> {
    let argv = &workload.argv;
    if argv.is_empty() || argv.len() > MAX_ARGUMENTS {
        return Err(violation(format!(
            "exec requires 1 to {MAX_ARGUMENTS} arguments"
        )));
    }
    if !argv[0].starts_with('/') {
        return Err(violation("exec program must be an absolute guest path"));
    }
    let cwd = workload.cwd.map(str::as_bytes);
    if let Some(cwd) = cwd
        && (!cwd.starts_with(b"/") || cwd.len() > MAX_ARGUMENT_BYTES || cwd.contains(&0))
    {
        return Err(violation(format!(
            "exec working directory must be an absolute guest path of at most \
             {MAX_ARGUMENT_BYTES} bytes without NUL characters"
        )));
    }
    let (environment, mut flags) = match workload.environment {
        WorkloadEnvironment::Default => (&[][..], 0),
        WorkloadEnvironment::Replaced(entries) => (entries, EXEC_ENVIRONMENT_PRESENT),
        WorkloadEnvironment::Layered(entries) => {
            (entries, EXEC_ENVIRONMENT_PRESENT | EXEC_INHERIT_DEFAULT_ENV)
        }
    };
    if environment.len() > MAX_ENVIRONMENT {
        return Err(violation(format!(
            "exec environment must not exceed {MAX_ENVIRONMENT} entries"
        )));
    }
    if cwd.is_some() {
        flags |= EXEC_CWD_PRESENT;
    }
    let count = u16::try_from(argv.len()).unwrap_or(u16::MAX);
    let mut payload = Vec::new();
    payload.extend_from_slice(&workload.timeout_ms.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    if flags == 0 {
        payload.extend_from_slice(&0u16.to_le_bytes());
    } else {
        let environment_count = u16::try_from(environment.len()).unwrap_or(u16::MAX);
        let cwd_len = u32::try_from(cwd.map_or(0, <[u8]>::len)).unwrap_or(u32::MAX);
        payload.extend_from_slice(&EXEC_EXTENDED.to_le_bytes());
        payload.extend_from_slice(&flags.to_le_bytes());
        payload.extend_from_slice(&environment_count.to_le_bytes());
        payload.extend_from_slice(&cwd_len.to_le_bytes());
    }
    for argument in argv {
        let bytes = argument.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_ARGUMENT_BYTES || bytes.contains(&0) {
            return Err(violation(format!(
                "exec arguments must be 1 to {MAX_ARGUMENT_BYTES} bytes without NUL characters"
            )));
        }
        let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        payload.extend_from_slice(&length.to_le_bytes());
        payload.extend_from_slice(bytes);
    }
    payload.extend_from_slice(cwd.unwrap_or_default());
    let mut names = BTreeSet::new();
    for entry in environment {
        let bytes = entry.as_bytes();
        let name = entry
            .split_once('=')
            .map(|(name, _)| name)
            .filter(|name| !name.is_empty());
        let Some(name) = name.filter(|_| bytes.len() <= MAX_ARGUMENT_BYTES && !bytes.contains(&0))
        else {
            return Err(violation(format!(
                "exec environment entries must be KEY=VALUE strings of at most \
                 {MAX_ARGUMENT_BYTES} bytes without NUL characters"
            )));
        };
        // The guest agent refuses a request that names a variable twice.
        if !names.insert(name) {
            return Err(violation(format!(
                "exec environment names must be unique, but {name:?} repeats"
            )));
        }
        let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        payload.extend_from_slice(&length.to_le_bytes());
        payload.extend_from_slice(bytes);
    }
    if APP_HEADER_LEN + payload.len() > OUTER_MAX_PAYLOAD {
        return Err(violation("exec request exceeds the control protocol limit"));
    }
    Ok(payload)
}

/// An `EXEC` payload as the guest agent reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecPayload {
    /// Timeout in milliseconds; zero disables it.
    pub timeout_ms: u32,
    /// The program, followed by its arguments.
    pub argv: Vec<String>,
    /// Absolute guest directory to start the program in; `None` selects `/`.
    pub cwd: Option<String>,
    /// The explicit `KEY=VALUE` environment entries, or `None` for the default environment.
    pub environment: Option<Vec<String>>,
    /// Whether the program starts from the default environment: true without explicit entries,
    /// and true when the entries layer over the default environment rather than replace it.
    pub inherit_default_env: bool,
}

/// An `EXEC` payload as the guest agent reads it, without imposing UTF-8 on guest byte strings.
///
/// Linux arguments, paths, and environment entries are byte strings. Keeping this form separate
/// from [`ExecPayload`] preserves the host and fake-OpenVMM APIs, which intentionally use UTF-8,
/// while accepting every non-NUL byte sequence that the C guest accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestExecPayload {
    /// Timeout in milliseconds; zero disables it.
    pub timeout_ms: u32,
    /// The absolute program, followed by its arguments.
    pub argv: Vec<Vec<u8>>,
    /// Absolute guest directory to start the program in; `None` selects `/`.
    pub cwd: Option<Vec<u8>>,
    /// Explicit `KEY=VALUE` entries, or `None` for the default environment.
    pub environment: Option<Vec<Vec<u8>>>,
    /// Whether explicit entries layer over the default environment.
    pub inherit_default_env: bool,
}

/// The fixed fields of an `EXEC` payload, which both decoders read and check alike.
struct ExecHeader {
    /// Timeout in milliseconds; zero disables it.
    timeout_ms: u32,
    /// Number of arguments, from 1 to [`MAX_ARGUMENTS`].
    count: usize,
    /// Flags of an extended payload; zero for a legacy one.
    flags: u16,
    /// Number of environment entries.
    environment_count: usize,
    /// Length of the working directory; zero without one.
    cwd_len: usize,
    /// Offset of the first argument.
    offset: usize,
}

/// Decodes the fixed fields of an `EXEC` payload, whose working directory may be 1 to `max_cwd`
/// bytes long.
///
/// It refuses a truncated header, a number of arguments outside 1 to [`MAX_ARGUMENTS`], an
/// unknown extension or flag, a working-directory length without the working-directory flag or
/// the reverse, environment entries without the environment flag, and more than
/// [`MAX_ENVIRONMENT`] entries.
fn decode_exec_header(payload: &[u8], max_cwd: usize) -> Result<ExecHeader, ProtocolError> {
    let truncated = || violation("the exec request is truncated");
    if payload.len() < 8 {
        return Err(truncated());
    }
    let timeout_ms = u32_at(payload, 0).ok_or_else(truncated)?;
    let count = usize::from(u16_at(payload, 4).ok_or_else(truncated)?);
    let extension = u16_at(payload, 6).ok_or_else(truncated)?;
    if count == 0 || count > MAX_ARGUMENTS {
        return Err(violation(format!(
            "exec requires 1 to {MAX_ARGUMENTS} arguments"
        )));
    }
    let (flags, environment_count, cwd_len, offset) = match extension {
        0 => (0, 0, 0, 8),
        EXEC_EXTENDED if payload.len() >= 16 => {
            let flags = u16_at(payload, 8).ok_or_else(truncated)?;
            let environment_count = usize::from(u16_at(payload, 10).ok_or_else(truncated)?);
            let cwd_len = u32_at(payload, 12).ok_or_else(truncated)? as usize;
            let known = EXEC_CWD_PRESENT | EXEC_ENVIRONMENT_PRESENT | EXEC_INHERIT_DEFAULT_ENV;
            let cwd_valid = if flags & EXEC_CWD_PRESENT == 0 {
                cwd_len == 0
            } else {
                (1..=max_cwd).contains(&cwd_len)
            };
            let environment_valid = flags & EXEC_ENVIRONMENT_PRESENT != 0
                || (environment_count == 0 && flags & EXEC_INHERIT_DEFAULT_ENV == 0);
            if flags & !known != 0
                || !cwd_valid
                || !environment_valid
                || environment_count > MAX_ENVIRONMENT
            {
                return Err(violation(
                    "the exec request has unknown flags or inconsistent fields",
                ));
            }
            (flags, environment_count, cwd_len, 16)
        }
        EXEC_EXTENDED => return Err(truncated()),
        _ => return Err(violation("the exec request has an unknown extension")),
    };
    Ok(ExecHeader {
        timeout_ms,
        count,
        flags,
        environment_count,
        cwd_len,
        offset,
    })
}

/// An `EXEC` payload as both decoders read it, with each string as one decoder holds it.
struct DecodedExec<S> {
    /// Timeout in milliseconds; zero disables it.
    timeout_ms: u32,
    /// Flags of an extended payload; zero for a legacy one.
    flags: u16,
    /// The absolute program, followed by its arguments.
    argv: Vec<S>,
    /// Absolute guest directory to start the program in; `None` selects `/`.
    cwd: Option<S>,
    /// The explicit `KEY=VALUE` entries, or `None` for the default environment.
    environment: Option<Vec<S>>,
}

/// Decodes an `EXEC` payload, whose working directory may be 1 to `max_cwd` bytes long, as both
/// decoders read it: it checks the payload's bytes, and converts each string to `S` as soon as it
/// reads it.
///
/// It refuses what [`decode_exec_header`] refuses, an empty argument, working directory, or
/// entry, or one longer than [`MAX_ARGUMENT_BYTES`], one that contains NUL characters or that `S`
/// refuses, a relative program or working directory, an entry without a name, a name that
/// repeats, and trailing bytes.
fn decode_exec<S: ExecString>(
    payload: &[u8],
    max_cwd: usize,
) -> Result<DecodedExec<S>, ProtocolError> {
    let ExecHeader {
        timeout_ms,
        count,
        flags,
        environment_count,
        cwd_len,
        mut offset,
    } = decode_exec_header(payload, max_cwd)?;
    let mut argv = Vec::with_capacity(count);
    for _ in 0..count {
        argv.push(prefixed_string::<S>(payload, &mut offset, "arguments")?);
    }
    if argv.first().and_then(|program| program.as_ref().first()) != Some(&b'/') {
        return Err(violation("exec program must be an absolute guest path"));
    }
    let cwd = if flags & EXEC_CWD_PRESENT == 0 {
        None
    } else {
        let what = "working directory";
        let cwd = S::convert(bytes_at(payload, offset, cwd_len, what)?.to_vec(), what)?;
        offset = offset
            .checked_add(cwd_len)
            .ok_or_else(|| violation("the exec request is truncated"))?;
        if cwd.as_ref().first() != Some(&b'/') {
            return Err(violation(
                "exec working directory must be an absolute guest path",
            ));
        }
        Some(cwd)
    };
    let mut environment = Vec::with_capacity(environment_count);
    let mut names = BTreeSet::new();
    for _ in 0..environment_count {
        let entry = prefixed_string::<S>(payload, &mut offset, "environment entries")?;
        // Like the guest agent, refuse entries without a name and repeated names.
        let bytes = entry.as_ref();
        let Some(name) = bytes
            .iter()
            .position(|&byte| byte == b'=')
            .filter(|&at| at != 0)
            .map(|at| &bytes[..at])
        else {
            return Err(S::unnamed_entry());
        };
        if !names.insert(name.to_vec()) {
            return Err(S::repeated_name(name));
        }
        environment.push(entry);
    }
    if offset != payload.len() {
        return Err(violation("the exec request has trailing bytes"));
    }
    Ok(DecodedExec {
        timeout_ms,
        flags,
        argv,
        cwd,
        environment: (flags & EXEC_ENVIRONMENT_PRESENT != 0).then_some(environment),
    })
}

/// Decodes the payload of an `EXEC` request.
///
/// It refuses what the guest agent refuses: a truncated payload, a number of arguments outside 1
/// to [`MAX_ARGUMENTS`], an unknown extension or flag, a working-directory length without the
/// working-directory flag or the reverse, environment entries without the environment flag, more
/// than [`MAX_ENVIRONMENT`] entries, an empty argument, working directory, or entry, or one
/// longer than [`MAX_ARGUMENT_BYTES`], text that is not UTF-8 or contains NUL characters, a
/// relative program or working directory, an entry without a name, a name that repeats, and
/// trailing bytes.
pub fn decode_exec_payload(payload: &[u8]) -> Result<ExecPayload, ProtocolError> {
    let DecodedExec {
        timeout_ms,
        flags,
        argv,
        cwd,
        environment,
    } = decode_exec::<String>(payload, MAX_ARGUMENT_BYTES)?;
    Ok(ExecPayload {
        timeout_ms,
        argv,
        cwd,
        environment,
        inherit_default_env: flags & EXEC_ENVIRONMENT_PRESENT == 0
            || flags & EXEC_INHERIT_DEFAULT_ENV != 0,
    })
}

/// Decodes an `EXEC` payload with exactly the C guest agent's byte-oriented acceptance rules.
///
/// It reads the payload as [`decode_exec_payload`] does, with the same checks in the same order,
/// and accepts what that decoder accepts and the strings that are not UTF-8, which Linux permits.
/// It bounds the working directory by [`MAX_GUEST_CWD_BYTES`], the C guest's bound, which equals
/// the [`MAX_ARGUMENT_BYTES`] of the host's decoder. Its `inherit_default_env` follows
/// [`EXEC_INHERIT_DEFAULT_ENV`] alone, so unlike the host's, it is false without explicit entries.
/// The host-facing decoder remains text-oriented, so this guest decoder does not broaden host
/// behavior.
pub fn decode_guest_exec_payload(payload: &[u8]) -> Result<GuestExecPayload, ProtocolError> {
    let DecodedExec {
        timeout_ms,
        flags,
        argv,
        cwd,
        environment,
    } = decode_exec::<Vec<u8>>(payload, MAX_GUEST_CWD_BYTES)?;
    Ok(GuestExecPayload {
        timeout_ms,
        argv,
        cwd,
        environment,
        inherit_default_env: flags & EXEC_INHERIT_DEFAULT_ENV != 0,
    })
}

/// A string of an `EXEC` payload, such as an argument, the working directory, or an environment
/// entry, as one decoder holds it. Each decoder's implementation holds what it does differently
/// from the other: the host's reads UTF-8 text, the guest's reads Linux byte strings, and they
/// word the refusals of an entry's name differently.
trait ExecString: AsRef<[u8]> + Sized {
    /// Converts the bytes of a string of the payload, which `what` names, as soon as they are
    /// read.
    fn convert(bytes: Vec<u8>, what: &str) -> Result<Self, ProtocolError>;

    /// Returns the error of an environment entry without a name before its first `=`.
    fn unnamed_entry() -> ProtocolError;

    /// Returns the error of an environment entry whose name, `name`, an earlier entry has.
    fn repeated_name(name: &[u8]) -> ProtocolError;
}

/// The host's decoder refuses bytes that are not UTF-8, and names a variable that repeats.
impl ExecString for String {
    fn convert(bytes: Vec<u8>, what: &str) -> Result<Self, ProtocolError> {
        String::from_utf8(bytes).map_err(|_| violation(format!("exec {what} must be valid UTF-8")))
    }

    fn unnamed_entry() -> ProtocolError {
        violation("exec environment entries must be KEY=VALUE strings")
    }

    fn repeated_name(name: &[u8]) -> ProtocolError {
        // The name of a UTF-8 entry ends before an ASCII `=`, so it is UTF-8 too.
        let name = String::from_utf8_lossy(name);
        violation(format!(
            "exec environment names must be unique, but {name:?} repeats"
        ))
    }
}

/// The guest's decoder keeps Linux byte strings as they are.
impl ExecString for Vec<u8> {
    fn convert(bytes: Vec<u8>, _what: &str) -> Result<Self, ProtocolError> {
        Ok(bytes)
    }

    fn unnamed_entry() -> ProtocolError {
        violation("exec environment entries must be KEY=VALUE byte strings")
    }

    fn repeated_name(_name: &[u8]) -> ProtocolError {
        violation("exec environment names must be unique")
    }
}

/// Reads the string at `offset` that its `u32` length precedes, of 1 to [`MAX_ARGUMENT_BYTES`]
/// bytes, as `S`, and advances `offset` past it.
fn prefixed_string<S: ExecString>(
    payload: &[u8],
    offset: &mut usize,
    what: &str,
) -> Result<S, ProtocolError> {
    S::convert(prefixed_bytes(payload, offset, what)?, what)
}

/// Reads one little-endian length-prefixed byte string and advances `offset`.
fn prefixed_bytes(
    payload: &[u8],
    offset: &mut usize,
    what: &str,
) -> Result<Vec<u8>, ProtocolError> {
    let length = u32_at(payload, *offset)
        .ok_or_else(|| violation("the exec request is truncated"))? as usize;
    *offset = offset
        .checked_add(4)
        .ok_or_else(|| violation("the exec request is truncated"))?;
    if length == 0 || length > MAX_ARGUMENT_BYTES {
        return Err(violation(format!(
            "exec {what} must be 1 to {MAX_ARGUMENT_BYTES} bytes"
        )));
    }
    let bytes = bytes_at(payload, *offset, length, what)?.to_vec();
    *offset = offset
        .checked_add(length)
        .ok_or_else(|| violation("the exec request is truncated"))?;
    Ok(bytes)
}

/// Returns one bounded non-NUL byte range without imposing UTF-8.
fn bytes_at<'a>(
    payload: &'a [u8],
    offset: usize,
    length: usize,
    what: &str,
) -> Result<&'a [u8], ProtocolError> {
    let bytes = offset
        .checked_add(length)
        .and_then(|end| payload.get(offset..end))
        .ok_or_else(|| violation("the exec request is truncated"))?;
    if bytes.contains(&0) {
        return Err(violation(format!(
            "exec {what} must not contain NUL characters"
        )));
    }
    Ok(bytes)
}

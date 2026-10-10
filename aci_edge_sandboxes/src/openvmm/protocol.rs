//! Codec for the OpenVMM control-console protocol.
//!
//! OpenVMM brokers an authenticated stream of outer records (magic `NVXS`) between one host
//! client and the guest agent. Data records carry application frames (magic `NVXC`) that belong
//! to the guest agent in `guest/common/nvx-managed-agent.c`. All integers are little-endian.

use std::collections::BTreeSet;
use std::fmt;

pub(crate) const OUTER_MAGIC: [u8; 4] = *b"NVXS";
pub(crate) const OUTER_VERSION: u16 = 1;
pub(crate) const OUTER_HEADER_LEN: usize = 44;
pub(crate) const OUTER_MAX_PAYLOAD: usize = 65_536;

pub(crate) const OUTER_HOST_ATTACH: u8 = 2;
pub(crate) const OUTER_RESET: u8 = 3;
pub(crate) const OUTER_DATA: u8 = 5;
pub(crate) const OUTER_WAIT: u8 = 6;
pub(crate) const OUTER_READY: u8 = 7;
pub(crate) const OUTER_ERROR: u8 = 8;

pub(crate) const APP_MAGIC: [u8; 4] = *b"NVXC";
pub(crate) const APP_VERSION: u8 = 1;
pub(crate) const APP_HEADER_LEN: usize = 24;

pub(crate) const APP_PING: u8 = 1;
pub(crate) const APP_EXEC: u8 = 2;
pub(crate) const APP_STOP: u8 = 3;
pub(crate) const APP_CANCEL: u8 = 4;
pub(crate) const APP_FEATURES: u8 = 5;
pub(crate) const APP_MAPS: u8 = 6;
pub(crate) const APP_READY: u8 = 0x81;
pub(crate) const APP_STDOUT: u8 = 0x82;
pub(crate) const APP_STDERR: u8 = 0x83;
pub(crate) const APP_EXIT: u8 = 0x84;
pub(crate) const APP_STOPPED: u8 = 0x85;
pub(crate) const APP_ERROR: u8 = 0xff;

/// Error category with which a guest agent refuses a request kind that it does not know.
pub(crate) const UNSUPPORTED_OPERATION: &[u8] = b"unsupported-operation";
/// Error category with which a guest agent reports a workload that it could not launch.
pub(crate) const LAUNCH_FAILED: &str = "launch-failed";
/// Error category with which a guest agent reports a working directory that the workload cannot
/// enter. The status is the guest's error number, and nothing of the workload ran.
pub(crate) const CWD_FAILED: &str = "cwd-failed";
/// Error category with which a guest agent refuses a workload until it has applied every host
/// mapping that the kernel command line announced.
pub(crate) const MAPPINGS_INCOMPLETE: &str = "mappings-incomplete";
/// Error category with which a guest agent reports a host mapping that it could not apply. The
/// status is the guest's error number.
pub(crate) const MAPPING_FAILED: &str = "mapping-failed";

/// Length of the launch capability that authenticates the host client.
pub(crate) const CAPABILITY_LEN: usize = 32;
/// Largest number of workload arguments, including the program.
pub(crate) const MAX_ARGUMENTS: usize = 64;
/// Largest encoded size of one workload argument.
pub(crate) const MAX_ARGUMENT_BYTES: usize = 4096;
/// Largest working directory that the openvmm backend accepts: Linux's `PATH_MAX` without the
/// terminating NUL, one byte below the agent's bound.
pub(crate) const MAX_CWD_BYTES: usize = 4095;
/// Largest number of workload environment entries.
pub(crate) const MAX_ENVIRONMENT: usize = 256;
/// Largest workload timeout the guest agent accepts: any value of the exec request's 32-bit
/// timeout field, which is also the largest `process.timeout` that MXC's schema allows.
pub(crate) const MAX_TIMEOUT_MS: u32 = u32::MAX;
/// Largest combined stdout and stderr volume the guest agent forwards for one execution.
pub(crate) const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Largest number of host mappings that the guest agent accepts.
pub(crate) const MAX_HOST_MAPPINGS: usize = 4096;
/// Largest source or target of one host mapping: Linux's `PATH_MAX` without the terminating NUL.
pub(crate) const MAX_MAP_PATH_BYTES: usize = 4095;

const EXEC_EXTENDED: u16 = 1;
const EXEC_CWD_PRESENT: u16 = 1 << 0;
const EXEC_ENVIRONMENT_PRESENT: u16 = 1 << 1;
const EXEC_INHERIT_DEFAULT_ENV: u16 = 1 << 2;
const MAPS_HEADER_LEN: usize = 8;
const MAP_ENTRY_HEADER_LEN: usize = 6;
const MAP_READ_ONLY: u16 = 1 << 0;

/// Protocol violation detected while encoding or decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtocolError(pub(crate) String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn violation(message: impl Into<String>) -> ProtocolError {
    ProtocolError(message.into())
}

/// Decoded outer record header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OuterHeader {
    pub(crate) record_type: u8,
    pub(crate) instance_id: [u8; 16],
    pub(crate) epoch: u64,
    pub(crate) sequence: u64,
    pub(crate) length: usize,
}

/// Encodes one outer record.
pub(crate) fn encode_outer(
    record_type: u8,
    instance_id: &[u8; 16],
    epoch: u64,
    sequence: u64,
    payload: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|&length| length as usize <= OUTER_MAX_PAYLOAD)
        .ok_or_else(|| violation("control payload exceeds the outer protocol limit"))?;
    let mut record = Vec::with_capacity(OUTER_HEADER_LEN + payload.len());
    record.extend_from_slice(&OUTER_MAGIC);
    record.extend_from_slice(&OUTER_VERSION.to_le_bytes());
    record.push(record_type);
    record.push(0);
    record.extend_from_slice(instance_id);
    record.extend_from_slice(&epoch.to_le_bytes());
    record.extend_from_slice(&sequence.to_le_bytes());
    record.extend_from_slice(&length.to_le_bytes());
    record.extend_from_slice(payload);
    Ok(record)
}

/// Decodes and validates an outer record header.
pub(crate) fn decode_outer_header(
    header: &[u8; OUTER_HEADER_LEN],
) -> Result<OuterHeader, ProtocolError> {
    let version = u16::from_le_bytes([header[4], header[5]]);
    let flags = header[7];
    let length = u32::from_le_bytes(array(&header[40..44])) as usize;
    if header[..4] != OUTER_MAGIC || version != OUTER_VERSION || flags != 0 {
        return Err(violation(
            "control endpoint returned an invalid outer record",
        ));
    }
    if length > OUTER_MAX_PAYLOAD {
        return Err(violation(
            "control endpoint returned an oversized outer record",
        ));
    }
    Ok(OuterHeader {
        record_type: header[6],
        instance_id: array(&header[8..24]),
        epoch: u64::from_le_bytes(array(&header[24..32])),
        sequence: u64::from_le_bytes(array(&header[32..40])),
        length,
    })
}

/// Decoded application frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AppFrame<'a> {
    pub(crate) kind: u8,
    pub(crate) request_id: u64,
    pub(crate) status: i32,
    pub(crate) payload: &'a [u8],
}

/// Encodes one application frame.
pub(crate) fn encode_app(kind: u8, request_id: u64, status: i32, payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let mut frame = Vec::with_capacity(APP_HEADER_LEN + payload.len());
    frame.extend_from_slice(&APP_MAGIC);
    frame.push(APP_VERSION);
    frame.push(kind);
    frame.extend_from_slice(&0u16.to_le_bytes());
    frame.extend_from_slice(&request_id.to_le_bytes());
    frame.extend_from_slice(&status.to_le_bytes());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Decodes and validates an application frame.
pub(crate) fn decode_app(frame: &[u8]) -> Result<AppFrame<'_>, ProtocolError> {
    if frame.len() < APP_HEADER_LEN {
        return Err(violation(
            "control endpoint returned a truncated application frame",
        ));
    }
    let flags = u16::from_le_bytes([frame[6], frame[7]]);
    let length = u32::from_le_bytes(array(&frame[20..24])) as usize;
    let payload = &frame[APP_HEADER_LEN..];
    if frame[..4] != APP_MAGIC || frame[4] != APP_VERSION || flags != 0 || length != payload.len() {
        return Err(violation(
            "control endpoint returned an invalid application frame",
        ));
    }
    Ok(AppFrame {
        kind: frame[5],
        request_id: u64::from_le_bytes(array(&frame[8..16])),
        status: i32::from_le_bytes(array(&frame[16..20])),
        payload,
    })
}

/// The environment that the guest agent gives a workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkloadEnvironment<'a> {
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
pub(crate) struct Workload<'a> {
    /// The program, an absolute guest path, followed by its arguments.
    pub(crate) argv: Vec<String>,
    /// Timeout in milliseconds; zero disables it.
    pub(crate) timeout_ms: u32,
    /// Absolute guest directory that the guest agent enters before it starts the program;
    /// `None` selects `/`.
    pub(crate) cwd: Option<&'a str>,
    /// The program's environment.
    pub(crate) environment: WorkloadEnvironment<'a>,
}

/// Encodes the payload of an `EXEC` request.
///
/// The guest agent requires 1 to [`MAX_ARGUMENTS`] non-empty arguments without NUL bytes, each
/// at most [`MAX_ARGUMENT_BYTES`] long, an absolute program path, an absolute working directory
/// of at most [`MAX_ARGUMENT_BYTES`] without NUL bytes, and at most [`MAX_ENVIRONMENT`]
/// `KEY=VALUE` environment entries with unique names. It accepts any timeout up to
/// [`MAX_TIMEOUT_MS`], and zero disables it. A workload without a working directory or an
/// explicit environment keeps the legacy payload.
pub(crate) fn encode_exec_payload(workload: &Workload<'_>) -> Result<Vec<u8>, ProtocolError> {
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

/// One bind mount of the host mapping table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostMap {
    /// Path of the mapped object relative to the guest's mount of the export, `/`-separated.
    pub(crate) source: String,
    /// Absolute guest path at which the guest agent mounts it.
    pub(crate) target: String,
    /// Whether the guest agent mounts it read-only.
    pub(crate) read_only: bool,
}

/// Encodes the host mapping table as the payloads of consecutive `APP_MAPS` requests, each at
/// most `limit` bytes long.
///
/// A payload holds the index of its first entry and its number of entries, so the guest agent
/// applies the entries in order and only up to the number that the kernel command line
/// announced. Each entry holds its flags, the lengths of its source and target, and then both.
/// The guest agent requires 1 to [`MAX_HOST_MAPPINGS`] entries, a relative source and an
/// absolute target of at most [`MAX_MAP_PATH_BYTES`] without NUL bytes.
pub(crate) fn encode_maps_payloads(
    maps: &[HostMap],
    limit: usize,
) -> Result<Vec<Vec<u8>>, ProtocolError> {
    if maps.is_empty() || maps.len() > MAX_HOST_MAPPINGS {
        return Err(violation(format!(
            "a host mapping table holds 1 to {MAX_HOST_MAPPINGS} entries"
        )));
    }
    let mut payloads = Vec::new();
    let mut payload = Vec::new();
    let mut count = 0u32;
    for (index, map) in maps.iter().enumerate() {
        let (source, target) = (map.source.as_bytes(), map.target.as_bytes());
        if source.is_empty()
            || source.starts_with(b"/")
            || !target.starts_with(b"/")
            || source.len() > MAX_MAP_PATH_BYTES
            || target.len() > MAX_MAP_PATH_BYTES
            || source.contains(&0)
            || target.contains(&0)
        {
            return Err(violation(format!(
                "a host mapping needs a relative source and an absolute target of at most \
                 {MAX_MAP_PATH_BYTES} bytes without NUL characters"
            )));
        }
        let entry_len = MAP_ENTRY_HEADER_LEN + source.len() + target.len();
        if MAPS_HEADER_LEN + entry_len > limit {
            return Err(violation(
                "a host mapping exceeds the control protocol limit",
            ));
        }
        if count > 0 && payload.len() + entry_len > limit {
            payload[4..8].copy_from_slice(&count.to_le_bytes());
            payloads.push(std::mem::take(&mut payload));
            count = 0;
        }
        if count == 0 {
            let first = u32::try_from(index).unwrap_or(u32::MAX);
            payload.extend_from_slice(&first.to_le_bytes());
            payload.extend_from_slice(&0u32.to_le_bytes());
        }
        let flags = if map.read_only { MAP_READ_ONLY } else { 0 };
        let length = |bytes: &[u8]| u16::try_from(bytes.len()).unwrap_or(u16::MAX);
        payload.extend_from_slice(&flags.to_le_bytes());
        payload.extend_from_slice(&length(source).to_le_bytes());
        payload.extend_from_slice(&length(target).to_le_bytes());
        payload.extend_from_slice(source);
        payload.extend_from_slice(target);
        count += 1;
    }
    payload[4..8].copy_from_slice(&count.to_le_bytes());
    payloads.push(payload);
    Ok(payloads)
}

/// Control features of a guest image, advertised in the response to `APP_FEATURES`.
///
/// The bits are defined by the guest agent in `guest/common/nvx-managed-agent.c`, which also
/// speaks for the features of its image's init script. The openvmm backend drives a sandbox
/// only when its guest provides every feature in [`GuestFeatures::REQUIRED`]; an older image
/// would silently ignore the policy or the request that depends on the missing feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GuestFeatures(u32);

impl GuestFeatures {
    /// What a guest agent that predates the `APP_FEATURES` request provides.
    pub(crate) const NONE: Self = Self(0);
    /// Terminates a workload on `APP_CANCEL` and reports the `cancelled` exit category.
    pub(crate) const CANCEL: Self = Self(1 << 0);
    // Bit 1 announced the bind mounts that `nvx_map=` kernel command-line tokens listed. Guests
    // no longer set it, so a host that still sends those tokens refuses them.
    /// Creates the account that `nvx_workload_account=create` selects.
    pub(crate) const WORKLOAD_ACCOUNT: Self = Self(1 << 2);
    /// Runs each workload in a cgroup of its own and kills whatever it leaves behind.
    pub(crate) const EXEC_CGROUP: Self = Self(1 << 3);
    /// Applies the working directory and the explicit environment of an extended `EXEC` request
    /// to that execution alone, and layers the environment over the default one on request.
    pub(crate) const EXEC_ENVIRONMENT: Self = Self(1 << 4);
    /// Starts each workload in the working directory that its `EXEC` request names, or in `/`,
    /// entered with the workload's identity, and refuses the launch with the `cwd-failed`
    /// category when the workload cannot enter it.
    pub(crate) const EXEC_CWD: Self = Self(1 << 5);
    /// Bind-mounts the host mapping table of `APP_MAPS` requests, as many entries as the
    /// `nvx_maps=` kernel command-line token announces, keeps the shared export root-only, and
    /// refuses workloads until every entry is mounted.
    pub(crate) const HOST_MAPPING_TABLE: Self = Self(1 << 6);
    /// The features that the openvmm backend depends on.
    pub(crate) const REQUIRED: Self = Self(
        Self::CANCEL.0
            | Self::WORKLOAD_ACCOUNT.0
            | Self::EXEC_CGROUP.0
            | Self::EXEC_ENVIRONMENT.0
            | Self::EXEC_CWD.0
            | Self::HOST_MAPPING_TABLE.0,
    );

    const NAMES: [(Self, &'static str); 6] = [
        (Self::CANCEL, "cancellation"),
        (Self::WORKLOAD_ACCOUNT, "workload accounts"),
        (Self::EXEC_CGROUP, "workload containment"),
        (Self::EXEC_ENVIRONMENT, "per-execution environments"),
        (Self::EXEC_CWD, "working directories"),
        (Self::HOST_MAPPING_TABLE, "host path mapping tables"),
    ];

    /// Decodes the payload of a features response.
    ///
    /// Later revisions may append fields, so trailing bytes are ignored.
    pub(crate) fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        let bits = payload
            .get(..4)
            .ok_or_else(|| violation("the guest agent returned a truncated features response"))?;
        Ok(Self(u32::from_le_bytes(array(bits))))
    }

    /// Encodes the payload of a features response, as a guest agent sends it.
    #[cfg(test)]
    pub(crate) fn encode(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    /// Names the features of `required` that this set lacks.
    pub(crate) fn missing(self, required: Self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .filter(|(feature, _)| required.0 & feature.0 != 0 && self.0 & feature.0 == 0)
            .map(|(_, name)| *name)
            .collect()
    }
}

/// Exit category reported by the guest agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitCategory {
    Exit,
    Timeout,
    OutputLimit,
    Signal,
    Failed,
    Cancelled,
}

impl ExitCategory {
    pub(crate) fn parse(payload: &[u8]) -> Result<Self, ProtocolError> {
        match payload {
            b"exit" => Ok(Self::Exit),
            b"timeout" => Ok(Self::Timeout),
            b"output-limit" => Ok(Self::OutputLimit),
            b"signal" => Ok(Self::Signal),
            b"failed" => Ok(Self::Failed),
            b"cancelled" => Ok(Self::Cancelled),
            _ => Err(violation(
                "guest agent returned an unsupported exit category",
            )),
        }
    }
}

fn array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut output = [0u8; N];
    output.copy_from_slice(bytes);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// A workload that runs `argv` without a timeout in `/` with the default environment.
    fn program<'a>(argv: &[&str]) -> Workload<'a> {
        Workload {
            argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
            timeout_ms: 0,
            cwd: None,
            environment: WorkloadEnvironment::Default,
        }
    }

    // Golden vectors produced with Python's struct module using the formats of
    // scripts/nvx_tools/control_session.py: "<4sHBB16sQQI", "<4sBBHQiI", "<IHH", and "<IHHHHI".
    #[test]
    fn host_attach_matches_the_python_client() {
        let capability: Vec<u8> = (1..=32).collect();
        let record = encode_outer(OUTER_HOST_ATTACH, &[0; 16], 0, 0, &capability).unwrap();
        assert_eq!(
            hex(&record),
            concat!(
                "4e56585301000200",
                "00000000000000000000000000000000",
                "0000000000000000",
                "0000000000000000",
                "20000000",
                "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20",
            )
        );
    }

    #[test]
    fn data_record_matches_the_python_client() {
        let instance: [u8; 16] = array(&(0x10..0x20).collect::<Vec<u8>>());
        let frame = encode_app(APP_PING, 0x0102_0304_0506_0708, 0, &[]);
        assert_eq!(
            hex(&frame),
            "4e5658430101000008070605040302010000000000000000"
        );
        let record = encode_outer(OUTER_DATA, &instance, 3, 7, &frame).unwrap();
        assert_eq!(
            hex(&record[..OUTER_HEADER_LEN]),
            concat!(
                "4e56585301000500101112131415161718191a1b1c1d1e1f",
                "0300000000000000",
                "0700000000000000",
                "18000000",
            )
        );
        let header = decode_outer_header(&array(&record[..OUTER_HEADER_LEN])).unwrap();
        assert_eq!(
            header,
            OuterHeader {
                record_type: OUTER_DATA,
                instance_id: instance,
                epoch: 3,
                sequence: 7,
                length: APP_HEADER_LEN,
            }
        );
    }

    #[test]
    fn exec_payload_matches_the_python_client() {
        let workload = Workload {
            timeout_ms: 5000,
            ..program(&["/bin/sh", "-c", "echo hi"])
        };
        let payload = encode_exec_payload(&workload).unwrap();
        assert_eq!(
            hex(&payload),
            concat!(
                "8813000003000000",
                "070000002f62696e2f7368",
                "020000002d63",
                "070000006563686f206869",
            )
        );
    }

    #[test]
    fn exec_payload_carries_every_32_bit_timeout() {
        let workload = Workload {
            timeout_ms: MAX_TIMEOUT_MS,
            ..program(&["/bin/true"])
        };
        assert_eq!(
            hex(&encode_exec_payload(&workload).unwrap()),
            concat!("ffffffff01000000", "090000002f62696e2f74727565"),
        );
        for timeout_ms in [0, 3_600_001, 86_400_000, MAX_TIMEOUT_MS] {
            let workload = Workload {
                timeout_ms,
                cwd: Some("/tmp"),
                ..program(&["/bin/true"])
            };
            let payload = encode_exec_payload(&workload).unwrap();
            assert_eq!(payload[..4], timeout_ms.to_le_bytes(), "{timeout_ms}");
        }
    }

    #[test]
    fn exec_payload_carries_the_working_directory_natively() {
        let workload = Workload {
            cwd: Some("/tmp"),
            ..program(&["/bin/pwd"])
        };
        assert_eq!(
            hex(&encode_exec_payload(&workload).unwrap()),
            concat!(
                "0000000001000100",
                "0100000004000000",
                "080000002f62696e2f707764",
                "2f746d70",
            )
        );
        let environment = ["FOO=a b".to_owned(), "EMPTY=".to_owned()];
        let layered = Workload {
            cwd: Some("/tmp"),
            environment: WorkloadEnvironment::Layered(&environment),
            ..program(&["/bin/env"])
        };
        assert_eq!(
            hex(&encode_exec_payload(&layered).unwrap()),
            concat!(
                "0000000001000100",
                "0700020004000000",
                "080000002f62696e2f656e76",
                "2f746d70",
                "07000000464f4f3d612062",
                "06000000454d5054593d",
            )
        );
        for cwd in ["", "tmp", &format!("/{}", "a".repeat(4096)), "/t\0mp"] {
            let workload = Workload {
                cwd: Some(cwd),
                ..program(&["/bin/pwd"])
            };
            assert!(encode_exec_payload(&workload).is_err(), "{cwd:?}");
        }
        let longest = format!("/{}", "a".repeat(4095));
        let workload = Workload {
            cwd: Some(&longest),
            ..program(&["/bin/pwd"])
        };
        assert!(encode_exec_payload(&workload).is_ok());
    }

    #[test]
    fn exec_payload_distinguishes_explicit_environments() {
        let environment = ["FOO=a b".to_owned(), "EMPTY=".to_owned()];
        let replaced = Workload {
            environment: WorkloadEnvironment::Replaced(&environment),
            ..program(&["/bin/env"])
        };
        assert_eq!(
            hex(&encode_exec_payload(&replaced).unwrap()),
            concat!(
                "0000000001000100",
                "0200020000000000",
                "080000002f62696e2f656e76",
                "07000000464f4f3d612062",
                "06000000454d5054593d",
            )
        );
        let empty = Workload {
            environment: WorkloadEnvironment::Replaced(&[]),
            ..program(&["/bin/env"])
        };
        let empty = encode_exec_payload(&empty).unwrap();
        assert_eq!(&empty[6..16], &[1, 0, 2, 0, 0, 0, 0, 0, 0, 0]);
        let layered = Workload {
            environment: WorkloadEnvironment::Layered(&environment),
            ..program(&["/bin/env"])
        };
        let layered = encode_exec_payload(&layered).unwrap();
        assert_eq!(&layered[8..10], &6u16.to_le_bytes());
        let default = encode_exec_payload(&program(&["/bin/env"])).unwrap();
        assert_eq!(&default[6..8], &[0, 0]);
    }

    #[test]
    fn exec_payload_enforces_agent_limits() {
        let encode = |argv: Vec<String>, timeout_ms| {
            encode_exec_payload(&Workload {
                argv,
                timeout_ms,
                ..program(&[])
            })
        };
        let argument = |value: &str| vec![value.to_owned()];
        assert!(encode(argument("bin/sh"), 0).is_err());
        assert!(encode(Vec::new(), 0).is_err());
        assert!(encode(argument(&format!("/{}", "a".repeat(4096))), 0).is_err());
        assert!(encode(vec!["/bin/true".to_owned(); 65], 0).is_err());
        assert!(encode(vec!["/bin/echo".to_owned(), String::new()], 0).is_err());
        assert!(encode(vec!["/bin/true".to_owned(); 64], MAX_TIMEOUT_MS).is_ok());
        let unique = |count: usize| -> Vec<String> {
            (0..count).map(|index| format!("KEY{index}=")).collect()
        };
        let encode_environment = |environment: &[String]| {
            encode_exec_payload(&Workload {
                environment: WorkloadEnvironment::Replaced(environment),
                ..program(&["/bin/env"])
            })
        };
        assert!(encode_environment(&unique(MAX_ENVIRONMENT)).is_ok());
        assert!(encode_environment(&unique(MAX_ENVIRONMENT + 1)).is_err());
        assert!(encode_environment(&["A=1".to_owned(), "A=2".to_owned()]).is_err());
        assert!(encode_environment(&["A=1".to_owned(), "AB=2".to_owned()]).is_ok());
        for entry in ["NOVALUE", "=value", &format!("A={}", "x".repeat(4096))] {
            assert!(encode_environment(&[entry.to_owned()]).is_err());
        }
        // Everything shares the 64 KiB control record.
        let large = |count: usize| -> Vec<String> {
            (0..count)
                .map(|index| format!("KEY{index:02}={}", "x".repeat(4000)))
                .collect()
        };
        assert!(encode_environment(&large(15)).is_ok());
        assert!(encode_environment(&large(17)).is_err());
        // The working directory counts toward the same limit.
        let full = vec![format!("/{}", "a".repeat(MAX_ARGUMENT_BYTES - 1)); 15];
        assert!(encode(full.clone(), 0).is_ok());
        let cwd = format!("/{}", "d".repeat(MAX_CWD_BYTES - 1));
        let with_cwd = Workload {
            argv: full,
            cwd: Some(&cwd),
            ..program(&[])
        };
        assert!(encode_exec_payload(&with_cwd).is_err());
    }

    #[test]
    fn decoders_reject_malformed_input() {
        let mut header = [0u8; OUTER_HEADER_LEN];
        assert!(decode_outer_header(&header).is_err());
        header[..4].copy_from_slice(&OUTER_MAGIC);
        header[4] = 1;
        header[40..44].copy_from_slice(&(OUTER_MAX_PAYLOAD as u32 + 1).to_le_bytes());
        assert!(decode_outer_header(&header).is_err());

        let mut frame = encode_app(APP_STDOUT, 1, 0, b"abc");
        assert_eq!(decode_app(&frame).unwrap().payload, b"abc");
        frame.push(0);
        assert!(decode_app(&frame).is_err());
        assert!(decode_app(&frame[..10]).is_err());
    }

    #[test]
    fn exit_categories_match_the_guest_agent() {
        assert_eq!(ExitCategory::parse(b"exit").unwrap(), ExitCategory::Exit);
        assert_eq!(
            ExitCategory::parse(b"timeout").unwrap(),
            ExitCategory::Timeout
        );
        assert_eq!(
            ExitCategory::parse(b"output-limit").unwrap(),
            ExitCategory::OutputLimit
        );
        assert_eq!(
            ExitCategory::parse(b"signal").unwrap(),
            ExitCategory::Signal
        );
        assert_eq!(
            ExitCategory::parse(b"failed").unwrap(),
            ExitCategory::Failed
        );
        assert_eq!(
            ExitCategory::parse(b"cancelled").unwrap(),
            ExitCategory::Cancelled
        );
        assert!(ExitCategory::parse(b"canceled").is_err());
    }

    #[test]
    fn guest_features_report_what_a_guest_lacks() {
        let required = GuestFeatures::REQUIRED;
        let complete = GuestFeatures::decode(&required.encode()).unwrap();
        assert!(complete.missing(required).is_empty());

        // Trailing bytes belong to later revisions.
        let cancel_only = GuestFeatures::decode(&[1, 0, 0, 0, 0xaa]).unwrap();
        assert_eq!(cancel_only, GuestFeatures::CANCEL);
        assert_eq!(
            cancel_only.missing(required),
            [
                "workload accounts",
                "workload containment",
                "per-execution environments",
                "working directories",
                "host path mapping tables",
            ]
        );
        assert!(GuestFeatures::decode(&[1, 0, 0]).is_err());

        // A guest of the previous release announced its host mappings with bit 1, which no
        // longer counts.
        let previous = GuestFeatures::decode(&0b11_1111u32.to_le_bytes()).unwrap();
        assert_eq!(previous.missing(required), ["host path mapping tables"]);

        // Every required feature has a name, so a refusal can say what is missing.
        assert_eq!(
            GuestFeatures::NONE.missing(required).len(),
            required.0.count_ones() as usize
        );
    }

    #[test]
    fn guest_features_use_distinct_bits() {
        let mut seen = 1u32 << 1;
        for (feature, name) in GuestFeatures::NAMES {
            assert_eq!(feature.0.count_ones(), 1, "{name}");
            assert_eq!(seen & feature.0, 0, "{name} reuses a feature bit");
            seen |= feature.0;
        }
    }

    fn map(source: &str, target: &str, read_only: bool) -> HostMap {
        HostMap {
            source: source.to_owned(),
            target: target.to_owned(),
            read_only,
        }
    }

    // Golden vector produced with Python's struct module: "<II", then "<HHH" before each entry.
    #[test]
    fn maps_payload_lists_each_entry_after_its_position() {
        let payloads = encode_maps_payloads(
            &[map("0", "/w", false), map("1/a b", "/t/x", true)],
            OUTER_MAX_PAYLOAD - APP_HEADER_LEN,
        )
        .unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(
            hex(&payloads[0]),
            concat!(
                "0000000002000000",
                "000001000200",
                "30",
                "2f77",
                "010005000400",
                "312f612062",
                "2f742f78",
            )
        );
    }

    #[test]
    fn maps_payloads_are_split_to_fit_the_limit() {
        let maps: Vec<HostMap> = (0..10)
            .map(|index| {
                map(
                    &format!("0/{index:02}"),
                    &format!("/work/{index:02}"),
                    index % 2 == 0,
                )
            })
            .collect();
        // Each entry takes 6 + 4 + 8 bytes, so a 64-byte payload holds 3 entries.
        let payloads = encode_maps_payloads(&maps, 64).unwrap();
        assert_eq!(payloads.len(), 4);
        let mut next = 0u32;
        for payload in &payloads {
            assert!(payload.len() <= 64);
            assert_eq!(u32::from_le_bytes(array(&payload[..4])), next);
            let count = u32::from_le_bytes(array(&payload[4..8]));
            let mut offset = MAPS_HEADER_LEN;
            for index in next..next + count {
                let field =
                    |at: usize| usize::from(u16::from_le_bytes(array(&payload[at..at + 2])));
                let flags = field(offset);
                let source_len = field(offset + 2);
                let target_len = field(offset + 4);
                offset += MAP_ENTRY_HEADER_LEN;
                let expected = &maps[index as usize];
                assert_eq!(flags == usize::from(MAP_READ_ONLY), expected.read_only);
                assert_eq!(
                    &payload[offset..offset + source_len],
                    expected.source.as_bytes()
                );
                offset += source_len;
                assert_eq!(
                    &payload[offset..offset + target_len],
                    expected.target.as_bytes()
                );
                offset += target_len;
            }
            assert_eq!(offset, payload.len());
            next += count;
        }
        assert_eq!(next, 10);

        // The largest table fits the control protocol in a few requests.
        let long = format!("/{}", "t".repeat(MAX_MAP_PATH_BYTES - 1));
        let largest: Vec<HostMap> = (0..MAX_HOST_MAPPINGS)
            .map(|index| map(&format!("0/{index}"), &long, false))
            .collect();
        let payloads = encode_maps_payloads(&largest, OUTER_MAX_PAYLOAD - APP_HEADER_LEN).unwrap();
        assert!(
            payloads
                .iter()
                .all(|payload| APP_HEADER_LEN + payload.len() <= OUTER_MAX_PAYLOAD)
        );
        assert_eq!(
            payloads
                .iter()
                .map(|payload| u32::from_le_bytes(array(&payload[4..8])))
                .sum::<u32>() as usize,
            MAX_HOST_MAPPINGS
        );
    }

    #[test]
    fn maps_payloads_enforce_agent_limits() {
        let limit = OUTER_MAX_PAYLOAD - APP_HEADER_LEN;
        let too_long = format!("/{}", "t".repeat(MAX_MAP_PATH_BYTES));
        for invalid in [
            map("", "/w", false),
            map("/0", "/w", false),
            map("0", "w", false),
            map("0", &too_long, false),
            map("0\0", "/w", false),
            map("0", "/w\0", false),
        ] {
            assert!(
                encode_maps_payloads(std::slice::from_ref(&invalid), limit).is_err(),
                "{invalid:?}"
            );
        }
        assert!(encode_maps_payloads(&[], limit).is_err());
        let too_many = vec![map("0", "/w", true); MAX_HOST_MAPPINGS + 1];
        assert!(encode_maps_payloads(&too_many, limit).is_err());
        // An entry that cannot fit any request is refused rather than split.
        assert!(encode_maps_payloads(&[map("0", "/abcdefghijklmnop", true)], 20).is_err());
    }

    fn guest_agent_source() -> Option<String> {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()?
            .join("guest")
            .join("common")
            .join("nvx-managed-agent.c");
        std::fs::read_to_string(source).ok()
    }

    #[test]
    fn feature_definitions_match_the_guest_agent_source() {
        let Some(source) = guest_agent_source() else {
            return;
        };
        let mut definitions = vec![
            format!("#define APP_FEATURES {APP_FEATURES}U"),
            format!("#define APP_MAPS {APP_MAPS}U"),
        ];
        for (name, feature) in [
            ("CANCEL", GuestFeatures::CANCEL),
            ("WORKLOAD_ACCOUNT", GuestFeatures::WORKLOAD_ACCOUNT),
            ("EXEC_CGROUP", GuestFeatures::EXEC_CGROUP),
            ("EXEC_ENVIRONMENT", GuestFeatures::EXEC_ENVIRONMENT),
            ("EXEC_CWD", GuestFeatures::EXEC_CWD),
            ("HOST_MAPPING_TABLE", GuestFeatures::HOST_MAPPING_TABLE),
        ] {
            let bit = feature.0.trailing_zeros();
            definitions.push(format!("#define FEATURE_{name} (1U << {bit})"));
        }
        for definition in definitions {
            assert!(
                source.contains(&definition),
                "the guest agent does not define `{definition}`"
            );
        }
        // The retired bit stays unused, so hosts that need it refuse the image.
        assert!(!source.contains("(1U << 1)"));
    }

    #[test]
    fn host_mapping_table_matches_the_guest_agent_source() {
        let Some(source) = guest_agent_source() else {
            return;
        };
        for expected in [
            format!("#define MAX_HOST_MAPPINGS {MAX_HOST_MAPPINGS}U"),
            format!("#define MAX_GUEST_PATH {}U", MAX_MAP_PATH_BYTES + 1),
            format!("#define MAPS_HEADER_LEN {MAPS_HEADER_LEN}U"),
            format!("#define MAP_ENTRY_HEADER_LEN {MAP_ENTRY_HEADER_LEN}U"),
            format!("#define MAP_READ_ONLY {MAP_READ_ONLY}U"),
            "#define MAPS_TOKEN \"nvx_maps=\"".to_owned(),
            format!("\"{MAPPINGS_INCOMPLETE}\""),
            format!("\"{MAPPING_FAILED}\""),
        ] {
            assert!(
                source.contains(&expected),
                "the guest agent does not contain `{expected}`"
            );
        }
    }

    #[test]
    fn exec_extension_matches_the_guest_agent_source() {
        // The agent accepts a working directory as long as one argument, more than any directory
        // that a workload can enter.
        const { assert!(MAX_CWD_BYTES < MAX_ARGUMENT_BYTES) };
        let Some(source) = guest_agent_source() else {
            return;
        };
        for expected in [
            format!("#define EXEC_EXTENDED {EXEC_EXTENDED}U"),
            format!("#define EXEC_CWD_PRESENT {EXEC_CWD_PRESENT}U"),
            format!("#define EXEC_ENVIRONMENT_PRESENT {EXEC_ENVIRONMENT_PRESENT}U"),
            format!("#define EXEC_INHERIT_DEFAULT_ENV {EXEC_INHERIT_DEFAULT_ENV}U"),
            format!("#define MAX_ARGUMENT_LEN {MAX_ARGUMENT_BYTES}U"),
            format!("#define MAX_ENVIRONMENT {MAX_ENVIRONMENT}U"),
            format!("\"{CWD_FAILED}\""),
            format!("\"{LAUNCH_FAILED}\""),
        ] {
            assert!(
                source.contains(&expected),
                "the guest agent does not contain `{expected}`"
            );
        }
    }
}

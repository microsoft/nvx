//! Codec for the OpenVMM control-console protocol.
//!
//! OpenVMM brokers an authenticated stream of outer records (magic `NVXS`) between one host
//! client and the guest agent. Data records carry application frames (magic `NVXC`) that belong
//! to the guest agent in `guest/common/nvx-managed-agent.c`. All integers are little-endian.

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
pub(crate) const APP_READY: u8 = 0x81;
pub(crate) const APP_STDOUT: u8 = 0x82;
pub(crate) const APP_STDERR: u8 = 0x83;
pub(crate) const APP_EXIT: u8 = 0x84;
pub(crate) const APP_STOPPED: u8 = 0x85;
pub(crate) const APP_ERROR: u8 = 0xff;

/// Error category with which a guest agent refuses a request kind that it does not know.
pub(crate) const UNSUPPORTED_OPERATION: &[u8] = b"unsupported-operation";

/// Length of the launch capability that authenticates the host client.
pub(crate) const CAPABILITY_LEN: usize = 32;
/// Largest number of workload arguments, including the program.
pub(crate) const MAX_ARGUMENTS: usize = 64;
/// Largest encoded size of one workload argument.
pub(crate) const MAX_ARGUMENT_BYTES: usize = 4096;
/// Largest workload timeout the guest agent accepts.
pub(crate) const MAX_TIMEOUT_MS: u32 = 60 * 60 * 1000;
/// Largest combined stdout and stderr volume the guest agent forwards for one execution.
pub(crate) const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

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

/// Encodes the payload of an `EXEC` request.
///
/// The guest agent requires 1 to [`MAX_ARGUMENTS`] non-empty arguments without NUL bytes, each
/// at most [`MAX_ARGUMENT_BYTES`] long, an absolute program path, and a timeout of at most
/// [`MAX_TIMEOUT_MS`] (zero disables it).
pub(crate) fn encode_exec_payload(
    argv: &[String],
    timeout_ms: u32,
) -> Result<Vec<u8>, ProtocolError> {
    if argv.is_empty() || argv.len() > MAX_ARGUMENTS {
        return Err(violation(format!(
            "exec requires 1 to {MAX_ARGUMENTS} arguments"
        )));
    }
    if !argv[0].starts_with('/') {
        return Err(violation("exec program must be an absolute guest path"));
    }
    if timeout_ms > MAX_TIMEOUT_MS {
        return Err(violation(format!(
            "exec timeout must not exceed {MAX_TIMEOUT_MS} ms"
        )));
    }
    let count = u16::try_from(argv.len()).unwrap_or(u16::MAX);
    let mut payload = Vec::new();
    payload.extend_from_slice(&timeout_ms.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
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
    if APP_HEADER_LEN + payload.len() > OUTER_MAX_PAYLOAD {
        return Err(violation("exec request exceeds the control protocol limit"));
    }
    Ok(payload)
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
    /// Bind-mounts the `nvx_map=` host paths and keeps the shared export root-only.
    pub(crate) const HOST_MAPPINGS: Self = Self(1 << 1);
    /// Creates the account that `nvx_workload_account=create` selects.
    pub(crate) const WORKLOAD_ACCOUNT: Self = Self(1 << 2);
    /// Runs each workload in a cgroup of its own and kills whatever it leaves behind.
    pub(crate) const EXEC_CGROUP: Self = Self(1 << 3);
    /// The features that the openvmm backend depends on.
    pub(crate) const REQUIRED: Self = Self(
        Self::CANCEL.0 | Self::HOST_MAPPINGS.0 | Self::WORKLOAD_ACCOUNT.0 | Self::EXEC_CGROUP.0,
    );

    const NAMES: [(Self, &'static str); 4] = [
        (Self::CANCEL, "cancellation"),
        (Self::HOST_MAPPINGS, "host path mappings"),
        (Self::WORKLOAD_ACCOUNT, "workload accounts"),
        (Self::EXEC_CGROUP, "workload containment"),
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

    // Golden vectors produced with Python's struct module using the formats of
    // scripts/nvx_tools/control_session.py: "<4sHBB16sQQI", "<4sBBHQiI", and "<IHH".
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
        let argv = ["/bin/sh", "-c", "echo hi"].map(String::from);
        let payload = encode_exec_payload(&argv, 5000).unwrap();
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
    fn exec_payload_enforces_agent_limits() {
        let argument = |value: &str| vec![value.to_owned()];
        assert!(encode_exec_payload(&argument("bin/sh"), 0).is_err());
        assert!(encode_exec_payload(&[], 0).is_err());
        assert!(encode_exec_payload(&argument("/bin/sh"), MAX_TIMEOUT_MS + 1).is_err());
        assert!(encode_exec_payload(&argument(&format!("/{}", "a".repeat(4096))), 0).is_err());
        assert!(encode_exec_payload(&vec!["/bin/true".to_owned(); 65], 0).is_err());
        assert!(encode_exec_payload(&["/bin/echo".to_owned(), String::new()], 0).is_err());
        assert!(encode_exec_payload(&vec!["/bin/true".to_owned(); 64], MAX_TIMEOUT_MS).is_ok());
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
                "host path mappings",
                "workload accounts",
                "workload containment"
            ]
        );
        assert!(GuestFeatures::decode(&[1, 0, 0]).is_err());

        // Every required feature has a name, so a refusal can say what is missing.
        assert_eq!(
            GuestFeatures::NONE.missing(required).len(),
            required.0.count_ones() as usize
        );
    }

    #[test]
    fn feature_definitions_match_the_guest_agent_source() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("guest")
            .join("common")
            .join("nvx-managed-agent.c");
        let Ok(source) = std::fs::read_to_string(source) else {
            return;
        };
        let mut definitions = vec![format!("#define APP_FEATURES {APP_FEATURES}U")];
        for (name, feature) in [
            ("CANCEL", GuestFeatures::CANCEL),
            ("HOST_MAPPINGS", GuestFeatures::HOST_MAPPINGS),
            ("WORKLOAD_ACCOUNT", GuestFeatures::WORKLOAD_ACCOUNT),
            ("EXEC_CGROUP", GuestFeatures::EXEC_CGROUP),
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
    }
}

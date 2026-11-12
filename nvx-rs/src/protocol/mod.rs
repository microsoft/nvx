//! Codec of the OpenVMM control-console protocol.
//!
//! OpenVMM brokers an authenticated stream of outer records (magic `NVXS`) between one host
//! client and the guest's agent. Data records carry the bytes of an application protocol. NVX's
//! own, `NVXC`, with its frames, its `EXEC` payloads and the control features that a guest image
//! advertises, is private to this crate: only NVX's programs speak it, the CLI, nvx-init, the
//! managed agent and the fake OpenVMM. All integers are little-endian.
//!
//! - Outer records: [`encode_outer`] and [`decode_outer_header`], and the `OUTER_*` record types.
//! - `MAPS` payloads: the [`HostMap`] table that the host encodes with [`encode_maps_payloads`],
//!   and the [`MapsPayload`] that the guest agent decodes with [`decode_maps_payload`].
//! - The kernel command-line tokens through which the host configures the guest agent at boot,
//!   such as [`MAPS_TOKEN`].
//! - [`cmdline`]: the kernel command line as Linux reads it, for the readers that need the
//!   kernel's view of it: the parameters into which the kernel splits it, and the virtio-mmio
//!   devices, of any type, that its `virtio_mmio.device` parameters describe; and the guest's DNS
//!   server, which OpenVMM's `virtnet_dns=` token names, with the resolver file that names it.
//! - [`root`]: the boot tokens of a layered root, which name its image's format and identity and
//!   its upper, with the host's renderer and the guest's parser.
//! - [`view`]: a cold boot's hostname bytes and machine-ID mode, with their renderer and parser.
//! - [`export`]: the boot tokens of the host export, the aggregate virtio-fs device from which a
//!   guest maps host paths, and the guest's strict parser of them.
//! - [`share`]: the guest targets that a sandbox's live shares may not take, which the host and
//!   the guest both refuse.
//! - [`nvxs`]: the NVXS session without I/O: validated records, incremental parsing, and the
//!   state machines of the host's and the guest's roles, which carry application bytes of any
//!   protocol.
//! - [`status`]: the exit statuses with which a clone's guest ends its VM once its repair or its
//!   activation failed, the time ABI's included, from which the host classifies a clone's end.
//!
//! The module depends on nothing outside the standard library, so the host, the guest, and test
//! doubles share it. Tests pin its wire constants as literals, and its records, frames, and
//! payloads with the conformance vectors of the crate's `tests/vectors/`, whose bytes no codec
//! produced.

use std::fmt;

mod app;
mod boot;
pub mod cmdline;
#[cfg(any(
    feature = "cli",
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) mod exec;
pub mod export;
#[cfg(any(
    feature = "cli",
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
mod features;
pub mod maps;
pub mod nvxs;
mod outer;
pub mod restore_ready;
pub mod root;
pub mod share;
pub mod status;
#[cfg(test)]
mod tests;
pub mod view;

#[cfg(any(
    feature = "cli",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) use app::APP_ACTIVATE;
#[cfg(any(
    feature = "cli",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) use app::APP_CAPTURE;
#[cfg(any(
    feature = "cli",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) use app::APP_HEADER_LEN;
#[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
pub(crate) use app::AppFrame;
#[cfg(any(feature = "cli", feature = "testing"))]
pub(crate) use app::ExitCategory;
#[cfg(feature = "cli")]
pub(crate) use app::encode_activate_payload;
#[cfg(feature = "cli")]
pub(crate) use app::encode_capture_payload;
#[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
pub(crate) use app::v2;
#[cfg(any(
    feature = "cli",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) use app::workload;
#[cfg(any(
    feature = "cli",
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) use app::{
    APP_CANCEL, APP_ERROR, APP_EXEC, APP_EXIT, APP_FEATURES, APP_MAPS, APP_PING, APP_READY,
    APP_STDERR, APP_STDOUT, APP_STOP, APP_STOPPED, MAPPING_FAILED, UNSUPPORTED_OPERATION,
    decode_app, encode_app,
};
#[cfg(any(
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) use app::{BUSY, CWD_FAILED, INVALID_REQUEST, LAUNCH_FAILED, MAPPINGS_INCOMPLETE};
#[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
pub(crate) use app::{decode_activate_payload, decode_capture_payload};
pub use boot::{MAPS_TOKEN, parse_host_mapping_count};
#[cfg(any(
    feature = "cli",
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) use exec::MAX_OUTPUT_BYTES;
#[cfg(feature = "testing")]
pub(crate) use exec::decode_exec_payload;
#[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
pub(crate) use exec::{GuestExecPayload, decode_guest_exec_payload};
#[cfg(feature = "cli")]
pub(crate) use exec::{Workload, WorkloadEnvironment, encode_exec_payload};
#[cfg(any(
    feature = "cli",
    feature = "testing",
    all(feature = "guest", target_os = "linux", target_arch = "x86_64")
))]
pub(crate) use features::GuestFeatures;
pub use maps::{
    GuestHostMap, GuestMapsPayload, HostMap, MAP_ENTRY_HEADER_LEN, MAP_READ_ONLY, MAPS_HEADER_LEN,
    MAX_HOST_MAPPINGS, MAX_MAP_PATH_BYTES, MapsPayload, decode_guest_maps_payload,
    decode_maps_payload, encode_maps_payloads,
};
pub use outer::{
    CAPABILITY_LEN, OUTER_ACK, OUTER_CREDIT, OUTER_DATA, OUTER_ERROR, OUTER_GUEST_ATTACH,
    OUTER_HEADER_LEN, OUTER_HOST_ATTACH, OUTER_MAGIC, OUTER_MAX_PAYLOAD, OUTER_MAX_RECEIVE_WINDOW,
    OUTER_MIN_RECEIVE_WINDOW, OUTER_READY, OUTER_RECEIVE_WINDOW, OUTER_RESET, OUTER_VERSION,
    OUTER_WAIT, OuterHeader, decode_ack_payload, decode_credit_payload, decode_outer_header,
    encode_ack_payload, encode_credit_payload, encode_outer,
};

/// Protocol violation detected while encoding or decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError(String);

impl ProtocolError {
    /// Returns the description of the violation.
    pub fn message(&self) -> &str {
        &self.0
    }

    /// Returns the description of the violation, consuming the error.
    pub fn into_message(self) -> String {
        self.0
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

/// Constructs a protocol error whose message describes the violated wire contract.
fn violation(message: impl Into<String>) -> ProtocolError {
    ProtocolError(message.into())
}

/// Copies an exactly `N`-byte wire field into an array for integer decoding.
fn array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut output = [0u8; N];
    output.copy_from_slice(bytes);
    output
}

/// Reads the little-endian `u16` at `offset`, or `None` past the end of `bytes`.
fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let field = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes(array(field)))
}

/// Reads the little-endian `u32` at `offset`, or `None` past the end of `bytes`.
fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let field = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(array(field)))
}

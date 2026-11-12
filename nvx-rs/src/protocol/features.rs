//! The control features that a guest image advertises in response to `APP_FEATURES`, which only
//! version 1 of NVXC has.
//!
//! NVX's host asked for them before it spoke version 2, whose Ready names the protocol version
//! instead. The guest agents and the fake OpenVMM still advertise them to version 1's `FEATURES`
//! until SL4.4d deletes them, and with them the host's side, which no host path uses any longer:
//! only some builds of the tests use it, so no expectation of dead code holds in every build.
#![cfg_attr(
    feature = "cli",
    allow(
        dead_code,
        reason = "NVX's host sends no FEATURES since SL4.4c, and SL4.4d deletes them"
    )
)]

use std::ops::BitOr;

#[cfg(feature = "cli")]
use super::{ProtocolError, array, violation};

/// Control features of a guest image, advertised in the response to `APP_FEATURES`.
///
/// The Rust guest agent uses these shared definitions and also speaks for the features of its
/// image's init script. A host drove a sandbox only when its guest provided every feature that
/// the host depended on, such as [`LAYERED_SANDBOX`](Self::LAYERED_SANDBOX) for `nvx-rs sandbox`;
/// an older image would silently ignore the policy or the request that depends on the missing
/// feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GuestFeatures(u32);

impl GuestFeatures {
    /// What a guest agent that predates the `APP_FEATURES` request provides.
    #[cfg(feature = "cli")]
    pub(crate) const NONE: Self = Self(0);
    /// Terminates a workload on `APP_CANCEL` and reports the `cancelled` exit category.
    pub(crate) const CANCEL: Self = Self(1 << 0);
    // Bit 1 announced the bind mounts that `nvx_map=` kernel command-line tokens listed. Guests
    // no longer set it, so a host that still sends those tokens refuses them.
    /// Creates the account that `nvx_workload_account=create` selects. Only the blockless root
    /// creates accounts; the layered root's executions require an existing one.
    pub(crate) const WORKLOAD_ACCOUNT: Self = Self(1 << 2);
    /// Runs each workload in a cgroup of its own and kills whatever it leaves behind.
    #[cfg(feature = "cli")]
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
    /// Features announced by the direct-mode managed agent.
    #[cfg(all(feature = "guest", target_os = "linux", target_arch = "x86_64"))]
    pub(crate) const DIRECT_AGENT: Self = Self(
        Self::CANCEL.0
            | Self::WORKLOAD_ACCOUNT.0
            | Self::EXEC_ENVIRONMENT.0
            | Self::EXEC_CWD.0
            | Self::HOST_MAPPING_TABLE.0,
    );
    /// Features that the fake OpenVMM's guest announces for a layered sandbox, as the managed
    /// agent's sandbox mode did.
    #[cfg(feature = "testing")]
    pub(crate) const SANDBOX_AGENT: Self =
        Self(Self::CANCEL.0 | Self::EXEC_ENVIRONMENT.0 | Self::HOST_MAPPING_TABLE.0);
    /// The features that `nvx-rs sandbox` depended on in the layered sandbox root while it spoke
    /// version 1:
    /// [`EXEC_ENVIRONMENT`](Self::EXEC_ENVIRONMENT), because `exec` sends a working directory and
    /// an environment in an extended `EXEC`, and
    /// [`HOST_MAPPING_TABLE`](Self::HOST_MAPPING_TABLE), because `start` sends the mappings of
    /// the sandbox's shares as `MAPS` requests.
    ///
    /// It holds no [`CANCEL`](Self::CANCEL), because the CLI sent no `APP_CANCEL` before it spoke
    /// version 2: as `nvx.py` did, Ctrl-C ended an `exec` by closing the session. It holds no
    /// [`WORKLOAD_ACCOUNT`](Self::WORKLOAD_ACCOUNT), because no sandbox launch requests account
    /// creation, which the layered root neither performs nor announces (microsoft/nvx#566).
    #[cfg(feature = "cli")]
    pub(crate) const LAYERED_SANDBOX: Self =
        Self(Self::EXEC_ENVIRONMENT.0 | Self::HOST_MAPPING_TABLE.0);

    /// Human-readable names of every required feature, used in compatibility errors.
    #[cfg(feature = "cli")]
    pub(crate) const NAMES: [(Self, &'static str); 6] = [
        (Self::CANCEL, "cancellation"),
        (Self::WORKLOAD_ACCOUNT, "workload accounts"),
        (Self::EXEC_CGROUP, "workload containment"),
        (Self::EXEC_ENVIRONMENT, "per-execution environments"),
        (Self::EXEC_CWD, "working directories"),
        (Self::HOST_MAPPING_TABLE, "host path mapping tables"),
    ];

    /// Returns the features whose bits are set in `bits`.
    #[cfg(feature = "testing")]
    pub(crate) const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Returns the bits of these features.
    #[cfg(feature = "testing")]
    pub(crate) const fn bits(self) -> u32 {
        self.0
    }

    /// Decodes the payload of a features response.
    ///
    /// Later revisions may append fields, so trailing bytes are ignored.
    #[cfg(feature = "cli")]
    pub(crate) fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        let bits = payload
            .get(..4)
            .ok_or_else(|| violation("the guest agent returned a truncated features response"))?;
        Ok(Self(u32::from_le_bytes(array(bits))))
    }

    /// Encodes the payload of a features response, as a guest agent sends it.
    #[cfg(any(
        feature = "testing",
        all(feature = "guest", target_os = "linux", target_arch = "x86_64")
    ))]
    pub(crate) const fn encode(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    /// Names the features of `required` that this set lacks.
    #[cfg(feature = "cli")]
    pub(crate) fn missing(self, required: Self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .filter(|(feature, _)| required.0 & feature.0 != 0 && self.0 & feature.0 == 0)
            .map(|(_, name)| *name)
            .collect()
    }
}

impl BitOr for GuestFeatures {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

//! The control features that a guest image advertises in response to `APP_FEATURES`.

use std::ops::BitOr;

use super::{ProtocolError, array, violation};

/// Control features of a guest image, advertised in the response to
/// [`APP_FEATURES`](super::APP_FEATURES).
///
/// The Rust guest agent uses these shared definitions and also speaks for the features of its
/// image's init script. A host drives a sandbox only when its guest provides every feature in
/// [`GuestFeatures::REQUIRED`]; an older image would silently ignore the policy or the request
/// that depends on the missing feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestFeatures(u32);

impl GuestFeatures {
    /// What a guest agent that predates the `APP_FEATURES` request provides.
    pub const NONE: Self = Self(0);
    /// Terminates a workload on `APP_CANCEL` and reports the `cancelled` exit category.
    pub const CANCEL: Self = Self(1 << 0);
    // Bit 1 announced the bind mounts that `nvx_map=` kernel command-line tokens listed. Guests
    // no longer set it, so a host that still sends those tokens refuses them.
    /// Creates the account that `nvx_workload_account=create` selects.
    pub const WORKLOAD_ACCOUNT: Self = Self(1 << 2);
    /// Runs each workload in a cgroup of its own and kills whatever it leaves behind.
    pub const EXEC_CGROUP: Self = Self(1 << 3);
    /// Applies the working directory and the explicit environment of an extended `EXEC` request
    /// to that execution alone, and layers the environment over the default one on request.
    pub const EXEC_ENVIRONMENT: Self = Self(1 << 4);
    /// Starts each workload in the working directory that its `EXEC` request names, or in `/`,
    /// entered with the workload's identity, and refuses the launch with the `cwd-failed`
    /// category when the workload cannot enter it.
    pub const EXEC_CWD: Self = Self(1 << 5);
    /// Bind-mounts the host mapping table of `APP_MAPS` requests, as many entries as the
    /// `nvx_maps=` kernel command-line token announces, keeps the shared export root-only, and
    /// refuses workloads until every entry is mounted.
    pub const HOST_MAPPING_TABLE: Self = Self(1 << 6);
    /// Features announced by the direct-mode managed agent.
    pub const DIRECT_AGENT: Self = Self(
        Self::CANCEL.0
            | Self::WORKLOAD_ACCOUNT.0
            | Self::EXEC_CGROUP.0
            | Self::EXEC_ENVIRONMENT.0
            | Self::EXEC_CWD.0
            | Self::HOST_MAPPING_TABLE.0,
    );
    /// Features announced by the managed agent that launches a sandbox container.
    pub const SANDBOX_AGENT: Self =
        Self(Self::CANCEL.0 | Self::WORKLOAD_ACCOUNT.0 | Self::EXEC_ENVIRONMENT.0);
    /// The features that the host depends on.
    pub const REQUIRED: Self = Self::DIRECT_AGENT;
    /// The features that `nvx-rs sandbox` depends on in the layered sandbox root, whose agent
    /// announces [`SANDBOX_AGENT`](Self::SANDBOX_AGENT): only
    /// [`EXEC_ENVIRONMENT`](Self::EXEC_ENVIRONMENT), because `exec` sends a working directory and
    /// an environment in an extended `EXEC`.
    ///
    /// It holds no [`CANCEL`](Self::CANCEL), because the CLI sends no `APP_CANCEL`: as `nvx.py`
    /// did, Ctrl-C ends an `exec` by closing the session, and the guest agent ends the workload
    /// when OpenVMM resets the session that it ran for. It holds no
    /// [`WORKLOAD_ACCOUNT`](Self::WORKLOAD_ACCOUNT), because no sandbox launch requests account
    /// creation, which the layered root does not perform (microsoft/nvx#566).
    pub const LAYERED_SANDBOX: Self = Self::EXEC_ENVIRONMENT;

    /// Human-readable names of every required feature, used in compatibility errors.
    pub(crate) const NAMES: [(Self, &'static str); 6] = [
        (Self::CANCEL, "cancellation"),
        (Self::WORKLOAD_ACCOUNT, "workload accounts"),
        (Self::EXEC_CGROUP, "workload containment"),
        (Self::EXEC_ENVIRONMENT, "per-execution environments"),
        (Self::EXEC_CWD, "working directories"),
        (Self::HOST_MAPPING_TABLE, "host path mapping tables"),
    ];

    /// Returns the features whose bits are set in `bits`.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Returns the bits of these features.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns whether this set holds every feature of `other`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Decodes the payload of a features response.
    ///
    /// Later revisions may append fields, so trailing bytes are ignored.
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        let bits = payload
            .get(..4)
            .ok_or_else(|| violation("the guest agent returned a truncated features response"))?;
        Ok(Self(u32::from_le_bytes(array(bits))))
    }

    /// Encodes the payload of a features response, as a guest agent sends it.
    pub const fn encode(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    /// Names the features of `required` that this set lacks.
    pub fn missing(self, required: Self) -> Vec<&'static str> {
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

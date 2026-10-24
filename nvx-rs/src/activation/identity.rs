//! The configuration identity of an activation record, which the [module](super#identity)
//! describes: [`ActivationRecord::identity`] computes it, and [`ReadyActivation::of`] reports it.
//! It needs `sha2`, which the `host` and the `guest` features enable.

use sha2::{Digest, Sha256};

use super::{ActivationRecord, ConfigurationIdentity, ReadyActivation};

#[cfg(test)]
mod tests;

/// What a configuration identity's digest hashes before the record's encoding: the label that
/// names version 1 of the identity, and the NUL that ends it.
const LABEL: &[u8] = b"nvx activation configuration identity v1\0";

impl ActivationRecord {
    /// Returns the record's configuration identity, as the [module](super#identity) describes it:
    /// the SHA-256 digest of a label and of the record's canonical [encoding](Self::encode).
    pub fn identity(&self) -> ConfigurationIdentity {
        let digest = Sha256::new()
            .chain_update(LABEL)
            .chain_update(self.encode())
            .finalize();
        ConfigurationIdentity::from_bytes(digest.into())
    }
}

impl ReadyActivation {
    /// Returns what Ready reports about the activation of `record`: the record's
    /// [identity](ActivationRecord::identity), and the transaction ID of a clone's restore.
    pub fn of(record: &ActivationRecord) -> Self {
        Self {
            identity: record.identity(),
            transaction: record.restore().map(|restore| restore.transaction),
        }
    }
}

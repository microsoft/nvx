//! The canonical encoding of what Ready reports about an activation, which the
//! [module](super#ready) describes: [`ReadyActivation::encode`] writes it, and
//! [`ReadyActivation::decode`] reads it back.

use super::{ActivationError, ConfigurationIdentity, ReadyActivation, TransactionId};

#[cfg(test)]
mod tests;

/// The byte that follows the identity of a cold boot's activation, which has no transaction.
const COLD: u8 = 0;
/// The byte that follows the identity of a clone's activation, before its transaction ID.
const CLONE: u8 = 1;

impl ReadyActivation {
    /// The bytes of a cold boot's activation: its identity, and a `0`.
    pub const COLD_LEN: usize = ConfigurationIdentity::LEN + 1;

    /// The bytes of a clone's activation: its identity, a `1`, and its transaction ID.
    pub const CLONE_LEN: usize = Self::COLD_LEN + TransactionId::LEN;

    /// Returns the activation's canonical encoding, as the [module](super#ready) describes it: the
    /// identity, then `0` for a cold boot, or `1` and a clone's transaction ID.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(Self::CLONE_LEN);
        bytes.extend_from_slice(self.identity.as_bytes());
        match &self.transaction {
            None => bytes.push(COLD),
            Some(transaction) => {
                bytes.push(CLONE);
                bytes.extend_from_slice(transaction.as_bytes());
            }
        }
        bytes
    }

    /// Reads an activation from its canonical encoding, as the [module](super#ready) describes it.
    ///
    /// Fails with [`ActivationError::Ready`] unless the bytes are an identity followed by `0`, or
    /// by `1` and a transaction ID, and nothing more.
    pub fn decode(bytes: &[u8]) -> Result<Self, ActivationError> {
        let (identity, rest) = bytes
            .split_first_chunk::<{ ConfigurationIdentity::LEN }>()
            .ok_or(ActivationError::Ready)?;
        let transaction = match rest {
            [COLD] => None,
            [CLONE, transaction @ ..] => Some(TransactionId::from_bytes(
                transaction.try_into().map_err(|_| ActivationError::Ready)?,
            )),
            _ => return Err(ActivationError::Ready),
        };
        Ok(Self {
            identity: ConfigurationIdentity::from_bytes(*identity),
            transaction,
        })
    }
}

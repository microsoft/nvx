//! What Ready reports about a guest restore, which the [module](super#ready) describes:
//! [`GuestRestoreReady::of`] returns it for a record, [`GuestRestoreReady::encode`] writes its
//! canonical encoding, and [`GuestRestoreReady::decode`] reads it back.

use super::{GuestRestoreError, GuestRestoreReady, GuestRestoreRecord, TransactionId};

#[cfg(test)]
mod tests;

/// The byte of a cold boot's report, which has no transaction.
const COLD: u8 = 0;
/// The byte of a clone's report, before its transaction ID.
const CLONE: u8 = 1;

impl GuestRestoreReady {
    /// The bytes of a cold boot's report: a `0`.
    pub const COLD_LEN: usize = 1;

    /// The bytes of a clone's report: a `1`, and its transaction ID.
    pub const CLONE_LEN: usize = Self::COLD_LEN + TransactionId::LEN;

    /// Returns what Ready reports about the guest restore of `record`: the transaction ID of a
    /// clone's restore, and nothing more for a cold boot.
    pub fn of(record: &GuestRestoreRecord) -> Self {
        Self {
            transaction: record.restore().map(|restore| restore.transaction),
        }
    }

    /// Returns the report's canonical encoding, as the [module](super#ready) describes it:
    /// `0` for a cold boot, or `1` and a clone's transaction ID.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(Self::CLONE_LEN);
        match &self.transaction {
            None => bytes.push(COLD),
            Some(transaction) => {
                bytes.push(CLONE);
                bytes.extend_from_slice(transaction.as_bytes());
            }
        }
        bytes
    }

    /// Reads a report from its canonical encoding, as the [module](super#ready) describes it.
    ///
    /// Fails with [`GuestRestoreError::Ready`] unless the bytes are `0`, or `1` and a transaction
    /// ID, and nothing more.
    pub fn decode(bytes: &[u8]) -> Result<Self, GuestRestoreError> {
        let transaction = match bytes {
            [COLD] => None,
            [CLONE, transaction @ ..] => Some(TransactionId::from_bytes(
                transaction
                    .try_into()
                    .map_err(|_| GuestRestoreError::Ready)?,
            )),
            _ => return Err(GuestRestoreError::Ready),
        };
        Ok(Self { transaction })
    }
}

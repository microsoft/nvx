//! What the tests of the `NVXS` session read of a record parser beyond its public API.

use super::RecordParser;

impl RecordParser {
    /// Returns the capacity of the payload buffer, which tests check against the declared
    /// length.
    pub(in crate::protocol::nvxs) fn payload_capacity(&self) -> usize {
        self.body
            .as_ref()
            .map_or(0, |(_, payload)| payload.capacity())
    }
}

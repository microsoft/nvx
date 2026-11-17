//! Merges, orders, and encodes the listener announcement.

use super::{Listener, MAX_ANNOUNCED, MAX_TRACKED};

/// Magic that opens every announcement, without which OpenVMM's gateway ignores the datagram.
///
/// It and the other constants of the NVXL format must equal those of OpenVMM's
/// `net_backend_resources::ingress`, which parses the announcement.
const MAGIC: [u8; 4] = *b"NVXL";

/// Version of the announcement format, the only one that OpenVMM's gateway accepts.
const VERSION: u8 = 1;

/// Flag of an announcement that omits listeners beyond `MAX_ANNOUNCED`; OpenVMM then warns that
/// it publishes only the lowest. The gateway rejects an announcement with any other flag.
const FLAG_OMITTED: u8 = 0x01;

/// Length of an announcement's header: the magic, the version, the flags, and the big-endian
/// 16-bit count of the entries that follow.
const HEADER_LENGTH: usize = 8;

/// Length of each entry: the IP protocol number, the family mask, and the big-endian 16-bit
/// port.
const ENTRY_LENGTH: usize = 4;

/// Bounded listener set merged by protocol and port.
#[derive(Default)]
pub(super) struct ListenerTable {
    /// Distinct listener entries in discovery order.
    entries: Vec<Listener>,
}

impl ListenerTable {
    /// Adds reachable families to one protocol and port, subject to the tracking bound.
    pub(super) fn add(&mut self, protocol: u8, port: u16, families: u8) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.protocol == protocol && entry.port == port)
        {
            entry.families |= families;
        } else if self.entries.len() < MAX_TRACKED {
            self.entries.push(Listener {
                protocol,
                families,
                port,
            });
        }
    }

    /// Sorts, bounds, and encodes this table as one NVXL version-1 payload.
    fn encode(mut self) -> Vec<u8> {
        self.entries
            .sort_unstable_by_key(|entry| (entry.protocol, entry.port));
        let announced = self.entries.len();
        let count = announced.min(MAX_ANNOUNCED);
        let flags = if announced > MAX_ANNOUNCED {
            FLAG_OMITTED
        } else {
            0
        };
        let mut payload = Vec::with_capacity(HEADER_LENGTH + count * ENTRY_LENGTH);
        payload.extend_from_slice(&MAGIC);
        payload.push(VERSION);
        payload.push(flags);
        payload.extend_from_slice(&(count as u16).to_be_bytes());
        for entry in &self.entries[..count] {
            payload.push(entry.protocol);
            payload.push(entry.families);
            payload.extend_from_slice(&entry.port.to_be_bytes());
        }
        payload
    }
}

/// Consumes and encodes one listener table.
pub(super) fn encode(table: ListenerTable) -> Vec<u8> {
    table.encode()
}

#[cfg(test)]
mod tests;

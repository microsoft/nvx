// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # Device Bus
//!
//! Routes guest port-mapped I/O (PMIO) to the modelled devices:
//!
//! - the **portb console** — a bidirectional per-byte console backing the kernel `hvc0`:
//!   `outb` to the **`0xE9` data port** transmits a byte (one exit per byte), while the guest
//!   receives input by polling the **`0xEA` status port** and reading the **`0xE9` data port**;
//! - a **`0x604` control port** — a write requests VM shutdown (Nanvix `DEFAULT_VMM_PORT`,
//!   also the ACPI `PM1a_CNT` poweroff register);
//! - a **`0x605` control port** — a write requests that the VMM take a snapshot.
//!
//! Every other port is treated as an unoccupied bus (reads return all-ones, writes are
//! dropped), which keeps a PCI-less, ACPI-less kernel from stalling on legacy probes.
//!

pub mod portb;

use ::std::sync::{
    Arc,
    Mutex,
};

use self::portb::PortConsole;

/// Console data port: guest `outb` transmits a byte; guest `inb` reads a received byte.
pub const DATA_PORT: u16 = 0xe9;
/// Console status port: guest `inb` returns bit 0 set when a received byte is available.
pub const STATUS_PORT: u16 = 0xea;
/// Control port: a write requests VM shutdown (Nanvix `DEFAULT_VMM_PORT` / ACPI `PM1a_CNT`).
pub const VMM_PORT: u16 = 0x604;
/// Control port: a write requests that the VMM take a snapshot (Nanvix-style snapshot command).
pub const SNAPSHOT_PORT: u16 = 0x605;

/// Action the VMM should take after servicing a guest port write.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PioAction {
    /// Continue running the guest.
    None,
    /// Shut the VM down.
    Shutdown,
    /// Take a snapshot of the VM.
    Snapshot,
}

/// The guest device bus.
pub struct DeviceBus {
    /// The portb console device, shared with the host input thread.
    console: Arc<Mutex<PortConsole>>,
}

impl DeviceBus {
    /// Creates a bus with the given console device.
    pub fn new(console: Arc<Mutex<PortConsole>>) -> Self {
        Self { console }
    }

    /// Returns a handle to the console device.
    pub fn console(&self) -> Arc<Mutex<PortConsole>> {
        Arc::clone(&self.console)
    }

    /// Services a guest read from `port`, filling `data`.
    pub fn pio_read(&self, port: u16, data: &mut [u8]) {
        match port {
            DATA_PORT => {
                let value: u8 = self.console.lock().expect("console poisoned").receive();
                Self::fill(data, value);
            },
            STATUS_PORT => {
                let value: u8 = self.console.lock().expect("console poisoned").rx_status();
                Self::fill(data, value);
            },
            _ => {
                // Unoccupied bus: reads float to all-ones.
                for byte in data.iter_mut() {
                    *byte = 0xff;
                }
                ::log::trace!("unhandled pio read: port={port:#06x}");
            },
        }
    }

    /// Services a guest write of `data` to `port`. Returns the [`PioAction`] the VMM should
    /// take (continue, shut down, or snapshot).
    #[must_use]
    pub fn pio_write(&self, port: u16, data: &[u8]) -> PioAction {
        match port {
            DATA_PORT => {
                if let Some(&byte) = data.first() {
                    self.console.lock().expect("console poisoned").transmit(byte);
                }
                PioAction::None
            },
            VMM_PORT => {
                ::log::info!("guest requested shutdown via control port {port:#06x}");
                PioAction::Shutdown
            },
            SNAPSHOT_PORT => {
                ::log::info!("guest requested snapshot via control port {port:#06x}");
                PioAction::Snapshot
            },
            _ => {
                ::log::trace!("unhandled pio write: port={port:#06x}");
                PioAction::None
            },
        }
    }

    /// Writes `value` into the first byte of `data` and zeroes the rest.
    fn fill(data: &mut [u8], value: u8) {
        if let Some(first) = data.first_mut() {
            *first = value;
        }
        for byte in data.iter_mut().skip(1) {
            *byte = 0;
        }
    }
}

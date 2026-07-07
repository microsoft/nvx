// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # Device Bus
//!
//! Routes guest port-mapped I/O (PMIO) to the modelled devices:
//!
//! - the primary **16550 UART** (`0x3F8..=0x3FF`) — the interactive `ttyS0` console;
//! - a Nanvix-style **`0xE9` debug port** — one `outb` per byte, a lower-overhead console
//!   path than the UART (which additionally polls the line-status register per byte);
//! - a **`0x604` control port** — a write requests VM shutdown (Nanvix `DEFAULT_VMM_PORT`,
//!   also the ACPI `PM1a_CNT` poweroff register).
//!
//! Every other port is treated as an unoccupied bus (reads return all-ones, writes are
//! dropped), which keeps a PCI-less, ACPI-less kernel from stalling on legacy probes.
//!

pub mod serial;

use ::std::sync::{
    Arc,
    Mutex,
};

use self::serial::Serial;
use crate::console::Console;

/// Base port of the primary serial controller (`ttyS0`).
pub const COM1_BASE: u16 = 0x3f8;
/// Last port of the primary serial controller.
const COM1_LAST: u16 = 0x3ff;
/// Nanvix-style per-byte debug console port (`DEFAULT_STDOUT_PORT`).
pub const DEBUGCON_PORT: u16 = 0xe9;
/// Control port: a write requests VM shutdown (Nanvix `DEFAULT_VMM_PORT` / ACPI `PM1a_CNT`).
pub const VMM_PORT: u16 = 0x604;

/// The guest device bus.
pub struct DeviceBus {
    /// Primary UART, shared with the host input thread.
    serial: Arc<Mutex<Serial>>,
    /// Shared host console sink (used by the UART and the `0xE9` debug port).
    console: Arc<Mutex<Console>>,
}

impl DeviceBus {
    /// Creates a bus with the given UART and console.
    pub fn new(serial: Arc<Mutex<Serial>>, console: Arc<Mutex<Console>>) -> Self {
        Self { serial, console }
    }

    /// Returns a handle to the primary UART.
    pub fn serial(&self) -> Arc<Mutex<Serial>> {
        Arc::clone(&self.serial)
    }

    /// Services a guest read from `port`, filling `data`.
    pub fn pio_read(&self, port: u16, data: &mut [u8]) {
        match port {
            COM1_BASE..=COM1_LAST => {
                let offset: u8 = (port - COM1_BASE) as u8;
                let value: u8 = self.serial.lock().expect("serial poisoned").read(offset);
                Self::fill(data, value);
            },
            // The QEMU debug port returns its own address to advertise its presence.
            DEBUGCON_PORT => Self::fill(data, 0xe9),
            _ => {
                // Unoccupied bus: reads float to all-ones.
                for byte in data.iter_mut() {
                    *byte = 0xff;
                }
                ::log::trace!("unhandled pio read: port={port:#06x}");
            },
        }
    }

    /// Services a guest write of `data` to `port`. Returns `true` if the guest requested
    /// that the VM shut down.
    #[must_use]
    pub fn pio_write(&self, port: u16, data: &[u8]) -> bool {
        match port {
            COM1_BASE..=COM1_LAST => {
                let offset: u8 = (port - COM1_BASE) as u8;
                if let Some(&value) = data.first() {
                    self.serial.lock().expect("serial poisoned").write(offset, value);
                }
                false
            },
            DEBUGCON_PORT => {
                if let Some(&byte) = data.first() {
                    self.console.lock().expect("console poisoned").write_byte(byte);
                }
                false
            },
            VMM_PORT => {
                ::log::info!("guest requested shutdown via control port {port:#06x}");
                true
            },
            _ => {
                ::log::trace!("unhandled pio write: port={port:#06x}");
                false
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

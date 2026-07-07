// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # Device Bus
//!
//! Routes guest port-mapped I/O (PMIO) to the modelled devices. Only the primary 16550
//! UART is emulated; every other port is treated as an unoccupied bus (reads return
//! all-ones, writes are dropped), which keeps a PCI-less, ACPI-less kernel from stalling on
//! legacy probes (i8042, CMOS/RTC, POST codes, ...).
//!

pub mod serial;

use ::std::sync::{
    Arc,
    Mutex,
};

use self::serial::Serial;

/// Base port of the primary serial controller (`ttyS0`).
pub const COM1_BASE: u16 = 0x3f8;
/// Last port of the primary serial controller.
const COM1_LAST: u16 = 0x3ff;

/// The guest device bus.
pub struct DeviceBus {
    /// Primary UART, shared with the host input thread.
    serial: Arc<Mutex<Serial>>,
}

impl DeviceBus {
    /// Creates a bus with the given primary UART.
    pub fn new(serial: Arc<Mutex<Serial>>) -> Self {
        Self { serial }
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
                if let Some(first) = data.first_mut() {
                    *first = value;
                }
                for byte in data.iter_mut().skip(1) {
                    *byte = 0;
                }
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

    /// Services a guest write of `data` to `port`.
    pub fn pio_write(&self, port: u16, data: &[u8]) {
        match port {
            COM1_BASE..=COM1_LAST => {
                let offset: u8 = (port - COM1_BASE) as u8;
                if let Some(&value) = data.first() {
                    self.serial.lock().expect("serial poisoned").write(offset, value);
                }
            },
            _ => {
                ::log::trace!("unhandled pio write: port={port:#06x}");
            },
        }
    }
}

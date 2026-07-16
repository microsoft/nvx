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
//! - on KVM, a minimal **MC146818 RTC/CMOS** on `0x70`/`0x71` so the mandatory early x86
//!   wall-clock read completes without a one-second timeout.
//!
//! Every other port is treated as an unoccupied bus (reads return all-ones, writes are dropped),
//! which keeps a PCI-less, ACPI-less kernel from stalling on unrelated legacy probes.
//!

pub mod portb;
#[cfg(target_os = "linux")]
pub mod rtc;

use ::std::sync::{
    Arc,
    Mutex,
};

use self::portb::PortConsole;
#[cfg(target_os = "linux")]
use self::rtc::Rtc;

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
    /// Minimal legacy wall clock used by early x86 Linux initialization.
    #[cfg(target_os = "linux")]
    rtc: Mutex<Rtc>,
}

impl DeviceBus {
    /// Creates a bus with the given console device.
    pub fn new(console: Arc<Mutex<PortConsole>>) -> Self {
        Self {
            console,
            #[cfg(target_os = "linux")]
            rtc: Mutex::new(Rtc::new()),
        }
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
            #[cfg(target_os = "linux")]
            port if Rtc::owns(port) => {
                let value: u8 = self.rtc.lock().expect("rtc poisoned").read(port);
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
            #[cfg(target_os = "linux")]
            port if Rtc::owns(port) => {
                if let Some(&value) = data.first() {
                    self.rtc.lock().expect("rtc poisoned").write(port, value);
                }
                PioAction::None
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

    /// Serializes PMIO device state. The RTC byte trails the existing console format, so older
    /// snapshots remain readable and the console parser safely ignores the extension.
    #[cfg(target_os = "linux")]
    pub fn snapshot(&self) -> Vec<u8> {
        let mut state = self.console.lock().expect("console poisoned").snapshot();
        state.push(self.rtc.lock().expect("rtc poisoned").snapshot());
        state
    }

    /// Restores PMIO device state, accepting snapshots created before the RTC byte was added.
    #[cfg(target_os = "linux")]
    pub fn restore(&self, state: &[u8]) {
        self.console.lock().expect("console poisoned").restore(state);
        if state.len() < 4 {
            return;
        }
        let queue_len = u32::from_le_bytes([state[0], state[1], state[2], state[3]]) as usize;
        if let Some(&index) = state.get(4 + queue_len) {
            self.rtc.lock().expect("rtc poisoned").restore(index);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::console::Console;

    fn bus() -> DeviceBus {
        let output = Arc::new(Mutex::new(Console::with_timing_markers(true, "marker", &[])));
        DeviceBus::new(Arc::new(Mutex::new(PortConsole::new(output))))
    }

    #[test]
    fn device_state_round_trips_rtc_index() {
        let source = bus();
        let _ = source.pio_write(rtc::RTC_INDEX, &[0x8b]);
        let restored = bus();
        restored.restore(&source.snapshot());
        assert_eq!(restored.rtc.lock().unwrap().snapshot(), 0x0b);
    }

    #[test]
    fn device_state_accepts_pre_rtc_snapshots() {
        let restored = bus();
        restored.restore(&[0, 0, 0, 0]);
        assert_eq!(restored.rtc.lock().unwrap().snapshot(), 0);
    }
}

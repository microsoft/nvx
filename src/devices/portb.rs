// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # portb console
//!
//! The guest console device, backing the kernel `hvc0` (`console=hvc0`). It replaces the
//! 16550 UART with a minimal, bidirectional per-byte port protocol:
//!
//! - **TX** — the guest writes one byte with `outb` to [`DATA_PORT`](super::DATA_PORT) (`0xE9`);
//!   the byte is forwarded to the shared host [`Console`] sink (one VM exit per byte).
//! - **RX** — host input is queued by [`enqueue`](PortConsole::enqueue). The guest polls
//!   [`STATUS_PORT`](super::STATUS_PORT) (`0xEA`, bit 0 set when a byte is ready) and then
//!   reads the byte with `inb` from [`DATA_PORT`](super::DATA_PORT).
//!
//! There is no interrupt line: the `hvc` driver polls, so the VMM never has to raise an IRQ.
//!

use ::std::collections::VecDeque;
use ::std::sync::{
    Arc,
    Mutex,
};

use crate::console::Console;

/// The guest console device: an output path to the host console plus a host-input queue.
pub struct PortConsole {
    /// Bytes received from the host, awaiting delivery to the guest.
    rx: VecDeque<u8>,
    /// Destination for transmitted bytes (the shared host console sink).
    console: Arc<Mutex<Console>>,
}

impl PortConsole {
    /// Creates a console device that transmits through `console`.
    pub fn new(console: Arc<Mutex<Console>>) -> Self {
        Self {
            rx: VecDeque::new(),
            console,
        }
    }

    /// Queues bytes received from the host for delivery to the guest.
    pub fn enqueue(&mut self, bytes: &[u8]) {
        self.rx.extend(bytes.iter().copied());
    }

    /// Transmits one byte from the guest (`outb` to the data port) to the host console.
    pub fn transmit(&mut self, byte: u8) {
        self.console.lock().expect("console poisoned").write_byte(byte);
    }

    /// Receive status (`inb` from the status port): bit 0 is set when input is available.
    pub fn rx_status(&self) -> u8 {
        u8::from(!self.rx.is_empty())
    }

    /// Receive data (`inb` from the data port): the next queued input byte, or `0` if none.
    pub fn receive(&mut self) -> u8 {
        self.rx.pop_front().unwrap_or(0)
    }

    /// Serializes the pending input queue into a compact byte vector (for snapshots).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(4 + self.rx.len());
        out.extend((self.rx.len() as u32).to_le_bytes());
        out.extend(self.rx.iter().copied());
        out
    }

    /// Restores the pending input queue produced by [`snapshot`](Self::snapshot).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn restore(&mut self, data: &[u8]) {
        if data.len() < 4 {
            return;
        }
        let n: usize = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        self.rx.clear();
        self.rx.extend(data.iter().skip(4).take(n).copied());
    }
}

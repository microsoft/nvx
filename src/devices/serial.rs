// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # 16550A UART
//!
//! A compact model of the 16550A serial controller sufficient to drive a Linux console
//! (`console=ttyS0`). Transmitted bytes are written to the provided output; received bytes
//! are queued by [`Serial::enqueue`] and delivered to the guest, raising the receive
//! interrupt when enabled.
//!

use ::std::collections::VecDeque;
use ::std::sync::{
    Arc,
    Mutex,
};

use crate::console::Console;

// Register offsets from the UART base address.
const REG_DATA: u8 = 0; // RBR / THR, or DLL when DLAB is set.
const REG_IER: u8 = 1; // Interrupt enable, or DLM when DLAB is set.
const REG_IIR: u8 = 2; // Interrupt identification (read) / FIFO control (write).
const REG_LCR: u8 = 3; // Line control.
const REG_MCR: u8 = 4; // Modem control.
const REG_LSR: u8 = 5; // Line status.
const REG_MSR: u8 = 6; // Modem status.
const REG_SCR: u8 = 7; // Scratch.

// Interrupt enable bits.
const IER_RECV: u8 = 0x01; // Received data available.
const IER_THR: u8 = 0x02; // Transmitter holding register empty.

// Interrupt identification values.
const IIR_NONE: u8 = 0x01; // No interrupt pending.
const IIR_THR_EMPTY: u8 = 0x02; // Transmitter holding register empty.
const IIR_RECV: u8 = 0x04; // Received data available.

// Line control bits.
const LCR_DLAB: u8 = 0x80; // Divisor latch access bit.

// Modem control bits.
const MCR_LOOP: u8 = 0x10; // Loopback mode.

// Line status bits.
const LSR_DATA_READY: u8 = 0x01;
const LSR_THR_EMPTY: u8 = 0x20;
const LSR_TEMT: u8 = 0x40;

/// State of a single 16550A UART.
pub struct Serial {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    /// Latched "transmitter empty" interrupt, set on THR write and cleared on IIR read.
    thre_int: bool,
    /// Receive queue feeding the guest.
    rx: VecDeque<u8>,
    /// Destination for transmitted bytes (the shared host console).
    console: Arc<Mutex<Console>>,
}

impl Serial {
    /// Creates a UART that transmits through `console`.
    pub fn new(console: Arc<Mutex<Console>>) -> Self {
        Self {
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            dll: 0x0c,
            dlm: 0,
            thre_int: false,
            rx: VecDeque::new(),
            console,
        }
    }

    /// Returns `true` if the divisor latch is currently selected.
    fn dlab(&self) -> bool {
        self.lcr & LCR_DLAB != 0
    }

    /// Queues bytes received from the host for delivery to the guest.
    pub fn enqueue(&mut self, bytes: &[u8]) {
        self.rx.extend(bytes.iter().copied());
    }

    /// Serializes the UART register state into a compact byte vector (for snapshots).
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(8 + self.rx.len());
        out.push(self.ier);
        out.push(self.lcr);
        out.push(self.mcr);
        out.push(self.scr);
        out.push(self.dll);
        out.push(self.dlm);
        out.push(u8::from(self.thre_int));
        out.extend((self.rx.len() as u32).to_le_bytes());
        out.extend(self.rx.iter().copied());
        out
    }

    /// Restores UART register state produced by [`snapshot`](Self::snapshot).
    pub fn restore(&mut self, data: &[u8]) {
        if data.len() < 11 {
            return;
        }
        self.ier = data[0];
        self.lcr = data[1];
        self.mcr = data[2];
        self.scr = data[3];
        self.dll = data[4];
        self.dlm = data[5];
        self.thre_int = data[6] != 0;
        let n: usize = u32::from_le_bytes([data[7], data[8], data[9], data[10]]) as usize;
        self.rx.clear();
        self.rx.extend(data.iter().skip(11).take(n).copied());
    }

    /// Returns `true` if the UART is currently asserting its interrupt line.
    pub fn interrupt_pending(&self) -> bool {
        (self.ier & IER_RECV != 0 && !self.rx.is_empty())
            || (self.ier & IER_THR != 0 && self.thre_int)
    }

    /// Computes the interrupt identification register value.
    fn iir(&self) -> u8 {
        if self.ier & IER_RECV != 0 && !self.rx.is_empty() {
            IIR_RECV
        } else if self.ier & IER_THR != 0 && self.thre_int {
            IIR_THR_EMPTY
        } else {
            IIR_NONE
        }
    }

    /// Reads the register at `offset` (0..=7 from the UART base).
    pub fn read(&mut self, offset: u8) -> u8 {
        match offset {
            REG_DATA => {
                if self.dlab() {
                    self.dll
                } else {
                    self.rx.pop_front().unwrap_or(0)
                }
            },
            REG_IER => {
                if self.dlab() {
                    self.dlm
                } else {
                    self.ier
                }
            },
            REG_IIR => {
                let value: u8 = self.iir();
                // Reading IIR acknowledges a pending THR-empty interrupt.
                self.thre_int = false;
                value
            },
            REG_LCR => self.lcr,
            REG_MCR => self.mcr,
            REG_LSR => {
                let mut value: u8 = LSR_THR_EMPTY | LSR_TEMT;
                if !self.rx.is_empty() {
                    value |= LSR_DATA_READY;
                }
                value
            },
            REG_MSR => 0,
            REG_SCR => self.scr,
            _ => 0,
        }
    }

    /// Writes `value` to the register at `offset` (0..=7 from the UART base).
    pub fn write(&mut self, offset: u8, value: u8) {
        match offset {
            REG_DATA => {
                if self.dlab() {
                    self.dll = value;
                } else if self.mcr & MCR_LOOP != 0 {
                    // In loopback mode transmitted data is looped back to the receiver.
                    self.rx.push_back(value);
                } else {
                    self.console.lock().expect("console poisoned").write_byte(value);
                    if self.ier & IER_THR != 0 {
                        self.thre_int = true;
                    }
                }
            },
            REG_IER => {
                if self.dlab() {
                    self.dlm = value;
                } else {
                    self.ier = value & 0x0f;
                    // The transmitter is always empty, so enabling its interrupt arms it.
                    if self.ier & IER_THR != 0 {
                        self.thre_int = true;
                    }
                }
            },
            REG_IIR => {}, // FIFO control register: FIFOs are not modelled.
            REG_LCR => self.lcr = value,
            REG_MCR => self.mcr = value,
            REG_LSR | REG_MSR => {}, // Read-only.
            REG_SCR => self.scr = value,
            _ => {},
        }
    }
}

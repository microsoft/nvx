// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # Calibration PIT (WHP)
//!
//! A deliberately minimal Intel 8254 programmable interval timer, present only on the WHP
//! backend. The KVM backend gets an in-kernel PIT for free (`KVM_CREATE_PIT2`); WHP has no
//! in-hypervisor PIT, so the guest's PIT port accesses would otherwise exit unhandled.
//!
//! The guest never depends on a PIT *interrupt* here (the timer is the in-hypervisor LAPIC
//! timer, and the console is polled), so this device models only what Linux needs to
//! **calibrate the TSC**: a channel-2 counter that counts down from its reload value in real
//! wall-clock time, and the channel-2 "OUT" status bit (port `0x61`, bit 5) that Linux polls
//! for terminal count. Both `pit_calibrate_tsc()` and `quick_pit_calibrate()` are served by
//! this. It is intentionally hang-safe: OUT is a pure function of elapsed real time, so it
//! always eventually asserts.
//!
//! Channels 0 and 1 accept programming but are inert (no interrupt is ever raised); they exist
//! only so the kernel's PIT clock-event setup does not fault on absent ports.
//!

use ::windows::Win32::System::Performance::{
    QueryPerformanceCounter,
    QueryPerformanceFrequency,
};

/// PIT input clock: 1.193182 MHz.
const PIT_FREQUENCY: u64 = 1_193_182;

/// Low PIT data port (channel 0). Channels are `BASE + channel`.
pub const PIT_CH0: u16 = 0x40;
/// PIT channel 2 data port.
pub const PIT_CH2: u16 = 0x42;
/// PIT mode/command port.
pub const PIT_CMD: u16 = 0x43;
/// NMI status and control port; bit 0 gates channel 2, bit 5 reports its OUT line.
pub const PIT_PORT61: u16 = 0x61;

/// Byte-access phase for reads/writes of a 16-bit count through an 8-bit port.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    /// Access the low byte only.
    LoByte,
    /// Access the high byte only.
    HiByte,
    /// Access the low byte then the high byte.
    LoHi,
}

impl Access {
    fn to_u8(self) -> u8 {
        match self {
            Access::LoByte => 0,
            Access::HiByte => 1,
            Access::LoHi => 2,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => Access::LoByte,
            1 => Access::HiByte,
            _ => Access::LoHi,
        }
    }
}

/// State of a single PIT counter.
struct Channel {
    /// Reload value; 0 represents a full 65536-count period.
    reload: u16,
    /// Programmed access mode.
    access: Access,
    /// Operating mode (0..5); only mode-0 terminal-count semantics matter here.
    mode: u8,
    /// Wall-clock instant (QPC ticks) at which the current countdown started.
    start: i64,
    /// Whether the counter is currently counting (channel 2 follows the port-`0x61` gate).
    gated: bool,
    /// Latched count from a counter-latch command, consumed by the next read(s).
    latched: Option<u16>,
    /// Pending high byte from a partial `LoHi` write.
    write_lo: Option<u8>,
    /// Pending high byte from a partial `LoHi` read.
    read_state: u8,
}

impl Channel {
    fn new() -> Self {
        Self {
            reload: 0,
            access: Access::LoHi,
            mode: 0,
            start: now_qpc(),
            gated: true,
            latched: None,
            write_lo: None,
            read_state: 0,
        }
    }

    /// Ticks elapsed (at the PIT frequency) since this channel started counting.
    fn elapsed_ticks(&self) -> u64 {
        if !self.gated {
            return 0;
        }
        let delta: i64 = now_qpc().saturating_sub(self.start);
        if delta <= 0 {
            return 0;
        }
        (delta as u128 * PIT_FREQUENCY as u128 / qpc_frequency() as u128) as u64
    }

    /// Effective reload period (0 means the full 65536).
    fn period(&self) -> u64 {
        if self.reload == 0 {
            65536
        } else {
            u64::from(self.reload)
        }
    }

    /// The current 16-bit counter value (counts down, wrapping through 0).
    fn current_count(&self) -> u16 {
        let elapsed: u64 = self.elapsed_ticks();
        let period: u64 = self.period();
        let remaining: i64 = period as i64 - (elapsed % 65536) as i64;
        remaining.rem_euclid(65536) as u16
    }

    /// The channel-2 OUT line for mode 0: low until terminal count, then high.
    fn out(&self) -> bool {
        self.elapsed_ticks() >= self.period()
    }

    /// (Re)starts the countdown from now.
    fn restart(&mut self) {
        self.start = now_qpc();
        self.latched = None;
        self.read_state = 0;
    }
}

/// The calibration PIT: three channels plus the channel-2 gate.
pub struct Pit {
    channels: [Channel; 3],
}

impl Default for Pit {
    fn default() -> Self {
        Self::new()
    }
}

impl Pit {
    /// Creates a PIT with all channels idle.
    pub fn new() -> Self {
        Self {
            channels: [Channel::new(), Channel::new(), Channel::new()],
        }
    }

    /// Returns `true` if `port` belongs to this device.
    pub fn owns(port: u16) -> bool {
        matches!(port, PIT_CH0..=PIT_CMD | PIT_PORT61)
    }

    /// Services a guest write of `value` to `port`.
    pub fn write(&mut self, port: u16, value: u8) {
        match port {
            PIT_CMD => self.command(value),
            PIT_CH0 | 0x41 | PIT_CH2 => {
                let idx: usize = (port - PIT_CH0) as usize;
                self.write_counter(idx, value);
            },
            PIT_PORT61 => {
                let gate: bool = value & 0x01 != 0;
                let ch2 = &mut self.channels[2];
                if gate && !ch2.gated {
                    ch2.gated = true;
                    ch2.restart();
                } else if !gate {
                    ch2.gated = false;
                }
            },
            _ => {},
        }
    }

    /// Services a guest read from `port`.
    pub fn read(&mut self, port: u16) -> u8 {
        match port {
            PIT_CH0 | 0x41 | PIT_CH2 => {
                let idx: usize = (port - PIT_CH0) as usize;
                self.read_counter(idx)
            },
            PIT_PORT61 => {
                let mut val: u8 = 0;
                if self.channels[2].gated {
                    val |= 0x01;
                }
                if self.channels[2].out() {
                    val |= 0x20;
                }
                val
            },
            // The command port is write-only.
            _ => 0xff,
        }
    }

    /// Serializes the programmable state of the three channels for a snapshot (the real-time
    /// countdown origin is not saved — counters simply restart on restore, which is harmless
    /// since nothing post-boot depends on the exact PIT phase).
    pub fn save(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(3 * 11);
        for ch in &self.channels {
            out.extend(ch.reload.to_le_bytes()); // [0..2]
            out.push(ch.access.to_u8()); // [2]
            out.push(ch.mode); // [3]
            out.push(u8::from(ch.gated)); // [4]
            out.push(ch.read_state); // [5]
            out.push(u8::from(ch.latched.is_some())); // [6]
            out.extend(ch.latched.unwrap_or(0).to_le_bytes()); // [7..9]
            out.push(u8::from(ch.write_lo.is_some())); // [9]
            out.push(ch.write_lo.unwrap_or(0)); // [10]
        }
        out
    }

    /// Restores state produced by [`save`](Self::save).
    pub fn load(&mut self, data: &[u8]) {
        const STRIDE: usize = 11;
        for (i, ch) in self.channels.iter_mut().enumerate() {
            let Some(b) = data.get(i * STRIDE..i * STRIDE + STRIDE) else {
                break;
            };
            ch.reload = u16::from_le_bytes([b[0], b[1]]);
            ch.access = Access::from_u8(b[2]);
            ch.mode = b[3];
            ch.gated = b[4] != 0;
            ch.read_state = b[5];
            ch.latched = if b[6] != 0 {
                Some(u16::from_le_bytes([b[7], b[8]]))
            } else {
                None
            };
            ch.write_lo = if b[9] != 0 { Some(b[10]) } else { None };
            ch.restart();
        }
    }

    /// Decodes a write to the mode/command port.
    fn command(&mut self, value: u8) {
        let channel: usize = (value >> 6) as usize;
        if channel > 2 {
            // Read-back command (0b11) is not used by the calibration paths.
            return;
        }
        let access_bits: u8 = (value >> 4) & 0b11;
        if access_bits == 0 {
            // Counter-latch command: freeze the current count for subsequent reads.
            let count: u16 = self.channels[channel].current_count();
            self.channels[channel].latched = Some(count);
            self.channels[channel].read_state = 0;
            return;
        }
        let access: Access = match access_bits {
            0b01 => Access::LoByte,
            0b10 => Access::HiByte,
            _ => Access::LoHi,
        };
        let ch = &mut self.channels[channel];
        ch.access = access;
        ch.mode = (value >> 1) & 0b111;
        ch.write_lo = None;
        ch.read_state = 0;
        ch.latched = None;
    }

    /// Writes one byte of a counter's reload value, starting the countdown when complete.
    fn write_counter(&mut self, idx: usize, value: u8) {
        let ch = &mut self.channels[idx];
        let complete: bool = match ch.access {
            Access::LoByte => {
                ch.reload = u16::from(value);
                true
            },
            Access::HiByte => {
                ch.reload = u16::from(value) << 8;
                true
            },
            Access::LoHi => match ch.write_lo.take() {
                None => {
                    ch.write_lo = Some(value);
                    false
                },
                Some(lo) => {
                    ch.reload = u16::from(lo) | (u16::from(value) << 8);
                    true
                },
            },
        };
        if complete {
            ch.restart();
        }
    }

    /// Reads one byte of a counter (latched value if a latch command preceded the read).
    fn read_counter(&mut self, idx: usize) -> u8 {
        let ch = &mut self.channels[idx];
        let count: u16 = ch.latched.unwrap_or_else(|| ch.current_count());
        match ch.access {
            Access::LoByte => {
                ch.latched = None;
                (count & 0xff) as u8
            },
            Access::HiByte => {
                ch.latched = None;
                (count >> 8) as u8
            },
            Access::LoHi => {
                if ch.read_state == 0 {
                    ch.read_state = 1;
                    (count & 0xff) as u8
                } else {
                    ch.read_state = 0;
                    ch.latched = None;
                    (count >> 8) as u8
                }
            },
        }
    }
}

/// Reads the host performance counter (monotonic).
fn now_qpc() -> i64 {
    let mut v: i64 = 0;
    // SAFETY: `v` is a valid, writable `i64`.
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

/// Reads the host performance-counter frequency (ticks per second).
fn qpc_frequency() -> i64 {
    let mut v: i64 = 1;
    // SAFETY: `v` is a valid, writable `i64`.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut v);
    }
    if v == 0 { 1 } else { v }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_the_pit_and_gate_ports() {
        assert!(Pit::owns(0x40));
        assert!(Pit::owns(0x43));
        assert!(Pit::owns(0x61));
        assert!(!Pit::owns(0xe9));
        assert!(!Pit::owns(0x44));
    }

    #[test]
    fn channel2_out_asserts_after_the_programmed_period() {
        let mut pit = Pit::new();
        // Enable the channel-2 gate (port 0x61 bit 0).
        pit.write(PIT_PORT61, 0x01);
        // Program channel 2, mode 0, lo/hi access, with the smallest possible period so the
        // OUT line asserts almost immediately in real time.
        pit.write(PIT_CMD, 0b10_11_000_0);
        pit.write(PIT_CH2, 1);
        pit.write(PIT_CH2, 0);
        // Busy-wait a touch of real time; OUT must eventually read high (bit 5 of port 0x61).
        let mut out_high = false;
        for _ in 0..1_000_000 {
            if pit.read(PIT_PORT61) & 0x20 != 0 {
                out_high = true;
                break;
            }
        }
        assert!(out_high, "channel-2 OUT never asserted");
    }
}

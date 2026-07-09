// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # Minimal RTC / CMOS (WHP)
//!
//! A tiny Motorola MC146818-compatible real-time clock on ports `0x70` (index) and `0x71`
//! (data), present only on the WHP backend.
//!
//! It exists to *stop a boot hang*, not to provide a full RTC. With no device on `0x70`/`0x71`
//! the ports float to all-ones, so the RTC's status register A reads back with the
//! **update-in-progress (UIP)** bit permanently set; the kernel's `mc146818_get_time()` then
//! spins waiting for UIP to clear (observed as tens of thousands of `0x70`/`0x71` exits per
//! second). Returning `UIP = 0`, a valid `VRT`, and the host wall-clock time in the standard
//! registers makes every RTC read succeed immediately.
//!
//! Time is reported in **binary, 24-hour** form (status register B advertises `DM`/`24h`), read
//! from the host local clock. No periodic interrupt is modelled (the guest keeps time from the
//! TSC / LAPIC timer).
//!

use ::windows::Win32::Foundation::SYSTEMTIME;
use ::windows::Win32::System::SystemInformation::GetLocalTime;

/// RTC index port: selects the CMOS register (low 7 bits; bit 7 is the NMI-disable flag).
pub const RTC_INDEX: u16 = 0x70;
/// RTC data port: reads/writes the selected CMOS register.
pub const RTC_DATA: u16 = 0x71;

/// A minimal MC146818 RTC/CMOS.
pub struct Rtc {
    /// Currently selected CMOS register index.
    index: u8,
}

impl Default for Rtc {
    fn default() -> Self {
        Self::new()
    }
}

impl Rtc {
    /// Creates an RTC with register 0 selected.
    pub fn new() -> Self {
        Self { index: 0 }
    }

    /// Returns `true` if `port` belongs to this device.
    pub fn owns(port: u16) -> bool {
        port == RTC_INDEX || port == RTC_DATA
    }

    /// Services a guest write of `value` to `port`.
    pub fn write(&mut self, port: u16, value: u8) {
        if port == RTC_INDEX {
            // Ignore the NMI-disable flag (bit 7); keep only the register index.
            self.index = value & 0x7f;
        }
        // Writes to the data port (setting the clock or scratch CMOS RAM) are ignored.
    }

    /// Services a guest read from `port`.
    pub fn read(&self, port: u16) -> u8 {
        if port != RTC_DATA {
            // The index port is not meaningfully readable.
            return 0xff;
        }
        let t: SYSTEMTIME = now_local();
        match self.index {
            0x00 => t.wSecond as u8,
            0x02 => t.wMinute as u8,
            0x04 => t.wHour as u8,
            0x06 => (t.wDayOfWeek + 1) as u8, // CMOS weekday is 1..=7 (Sunday = 1)
            0x07 => t.wDay as u8,
            0x08 => t.wMonth as u8,
            0x09 => (t.wYear % 100) as u8,
            0x32 => (t.wYear / 100) as u8, // century
            0x0a => 0x26,                  // status A: UIP = 0, 32 kHz base, 1024 Hz rate
            0x0b => 0x06,                  // status B: 24-hour (bit1) + binary data mode (bit2)
            0x0c => 0x00,                  // status C: no pending interrupt flags
            0x0d => 0x80,                  // status D: VRT = 1 (RTC/battery valid)
            _ => 0x00,                     // other CMOS RAM
        }
    }

    /// Serializes state for a snapshot (just the selected register index).
    pub fn save(&self) -> Vec<u8> {
        vec![self.index]
    }

    /// Restores state produced by [`save`](Self::save).
    pub fn load(&mut self, data: &[u8]) {
        if let Some(&index) = data.first() {
            self.index = index & 0x7f;
        }
    }
}

/// Reads the host local time.
fn now_local() -> SYSTEMTIME {
    // SAFETY: `GetLocalTime` takes no inputs and returns the current local time by value.
    unsafe { GetLocalTime() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_only_the_rtc_ports() {
        assert!(Rtc::owns(0x70));
        assert!(Rtc::owns(0x71));
        assert!(!Rtc::owns(0x72));
        assert!(!Rtc::owns(0xea));
    }

    #[test]
    fn status_a_reports_uip_clear() {
        let mut rtc = Rtc::new();
        rtc.write(RTC_INDEX, 0x0a);
        // The load-bearing invariant: bit 7 (UIP) must be clear, or the guest spins.
        assert_eq!(rtc.read(RTC_DATA) & 0x80, 0, "UIP must read as clear");
    }

    #[test]
    fn status_d_reports_valid_time() {
        let mut rtc = Rtc::new();
        rtc.write(RTC_INDEX, 0x0d);
        assert_eq!(rtc.read(RTC_DATA) & 0x80, 0x80, "VRT must be set");
    }
}

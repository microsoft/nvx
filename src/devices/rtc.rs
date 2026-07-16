// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//! Minimal MC146818 RTC/CMOS for the KVM backend.
//!
//! Linux reads the standard PC wall clock during early x86 initialization even when
//! `CONFIG_RTC_CLASS` is disabled. Floating `0x70`/`0x71` reads leave the update-in-progress bit
//! set and make `mc146818_get_time()` wait for its one-second timeout. This device provides only
//! the binary, 24-hour wall-clock registers needed to make that early read complete immediately.

/// RTC index port: selects the CMOS register (bit 7 is the NMI-disable flag).
pub const RTC_INDEX: u16 = 0x70;
/// RTC data port: reads the selected CMOS register.
pub const RTC_DATA: u16 = 0x71;

/// Minimal MC146818 register file.
pub struct Rtc {
    index: u8,
}

impl Rtc {
    pub fn new() -> Self {
        Self { index: 0 }
    }

    pub fn owns(port: u16) -> bool {
        port == RTC_INDEX || port == RTC_DATA
    }

    pub fn write(&mut self, port: u16, value: u8) {
        if port == RTC_INDEX {
            self.index = value & 0x7f;
        }
    }

    pub fn read(&self, port: u16) -> u8 {
        if port != RTC_DATA {
            return 0xff;
        }
        let time = now_local();
        match self.index {
            0x00 => time.tm_sec as u8,
            0x02 => time.tm_min as u8,
            0x04 => time.tm_hour as u8,
            0x06 => (time.tm_wday + 1) as u8,
            0x07 => time.tm_mday as u8,
            0x08 => (time.tm_mon + 1) as u8,
            0x09 => ((time.tm_year + 1900) % 100) as u8,
            0x32 => ((time.tm_year + 1900) / 100) as u8,
            0x0a => 0x26, // UIP clear, 32 kHz base, 1024 Hz rate.
            0x0b => 0x06, // 24-hour, binary data mode.
            0x0c => 0x00, // No pending interrupt flags.
            0x0d => 0x80, // RTC/battery valid.
            _ => 0x00,
        }
    }

    pub fn snapshot(&self) -> u8 {
        self.index
    }

    pub fn restore(&mut self, index: u8) {
        self.index = index & 0x7f;
    }
}

fn now_local() -> ::libc::tm {
    // SAFETY: `time` writes nowhere when passed null; `localtime_r` receives valid pointers to
    // initialized storage and fills the `tm` value synchronously.
    unsafe {
        let now: ::libc::time_t = ::libc::time(::core::ptr::null_mut());
        let mut local: ::libc::tm = ::core::mem::zeroed();
        ::libc::localtime_r(&now, &mut local);
        local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_registers_avoid_linux_rtc_timeout() {
        let mut rtc = Rtc::new();
        rtc.write(RTC_INDEX, 0x0a);
        assert_eq!(rtc.read(RTC_DATA) & 0x80, 0);
        rtc.write(RTC_INDEX, 0x0d);
        assert_eq!(rtc.read(RTC_DATA) & 0x80, 0x80);
    }

    #[test]
    fn snapshot_round_trips_selected_register() {
        let mut rtc = Rtc::new();
        rtc.write(RTC_INDEX, 0x8b);
        let state = rtc.snapshot();
        let mut restored = Rtc::new();
        restored.restore(state);
        assert_eq!(restored.snapshot(), 0x0b);
    }
}

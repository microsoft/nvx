// Copyright(c) The microvm authors.
// Licensed under the MIT License.

//!
//! # 8259A PIC (WHP)
//!
//! A minimal Intel 8259A programmable interrupt controller (master + slave), present only on
//! the WHP backend. The KVM backend gets an in-kernel PIC for free (`KVM_CREATE_IRQCHIP`); WHP
//! does not, so without this the guest's PIC probe (`probe_8259A`, which writes and reads back
//! the interrupt-mask register on port `0x21`) fails, the kernel falls back to the *NULL legacy
//! PIC*, and `request_irq(0, timer_interrupt)` fails.
//!
//! That matters because a PVH guest with no ACPI/MP tables runs in APIC "virtual wire, no
//! configuration" mode, in which Linux does **not** set up the local-APIC timer and instead
//! relies on the **PIT** (i8253) as its clock-event device — delivered as **IRQ0** through this
//! PIC. So this device does two jobs: it is detectable (faithful mask-register and
//! init-sequence handling) so the kernel wires up the legacy IRQ path, and it turns a periodic
//! host tick into the guest's IRQ0 timer interrupt (see [`raise_irq0`](Pic::raise_irq0)).
//!
//! Only what the timer path needs is modelled: the ICW1–ICW4 init sequence (to capture the
//! vector base the guest programs), OCW1 (mask), OCW2 (end-of-interrupt), OCW3 (read IRR/ISR),
//! and IRQ0 request/in-service/EOI bookkeeping on the master. The slave exists only to satisfy
//! the two-controller init the kernel performs.
//!

/// Master PIC command port.
pub const PIC_MASTER_CMD: u16 = 0x20;
/// Master PIC data (mask) port.
pub const PIC_MASTER_DATA: u16 = 0x21;
/// Slave PIC command port.
pub const PIC_SLAVE_CMD: u16 = 0xa0;
/// Slave PIC data (mask) port.
pub const PIC_SLAVE_DATA: u16 = 0xa1;

/// One 8259A controller.
#[derive(Default)]
struct Chip {
    /// Interrupt mask register (OCW1); a set bit masks that line.
    imr: u8,
    /// Interrupt request register: lines awaiting service.
    irr: u8,
    /// In-service register: lines currently being serviced.
    isr: u8,
    /// Vector base programmed via ICW2 (line N delivers `base + N`).
    base: u8,
    /// Remaining ICW step (0 = initialised/idle, 1..=3 = expecting ICW2/3/4).
    icw_step: u8,
    /// Whether ICW4 is expected (from ICW1 bit 0).
    icw4: bool,
    /// Whether the next command-port read returns ISR (set by OCW3) instead of IRR.
    read_isr: bool,
    /// Whether the controller has completed its init sequence at least once.
    initialised: bool,
}

impl Chip {
    /// Handles a write to the command port (`0x20`/`0xA0`). Returns the line cleared by an
    /// end-of-interrupt so the caller can complete the matching local-APIC EOI.
    fn write_cmd(&mut self, value: u8) -> Option<u8> {
        if value & 0x10 != 0 {
            // ICW1: begin initialisation. Real hardware leaves the lines masked until the
            // kernel programs OCW1; keep them masked so a host tick cannot inject IRQ0 into the
            // init window before the guest has installed its handler (which would wedge the
            // in-service state, as no end-of-interrupt would ever arrive).
            self.icw4 = value & 0x01 != 0;
            self.icw_step = 1;
            self.imr = 0xff;
            self.isr = 0;
            self.irr = 0;
            self.read_isr = false;
            None
        } else if value & 0x08 != 0 {
            // OCW3: select the register exposed on the next command-port read.
            if value & 0x03 == 0x03 {
                self.read_isr = true;
            } else if value & 0x03 == 0x02 {
                self.read_isr = false;
            }
            None
        } else if value & 0x20 != 0 {
            // OCW2: end-of-interrupt. Non-specific clears the highest in-service line;
            // specific (bit 6) clears the addressed line. In virtual-wire mode the guest issues
            // only this 8259 end-of-interrupt (never a local-APIC one), so report whether a line
            // that was in service has now been acknowledged, letting the caller complete the
            // matching local-APIC EOI for whichever vector was injected (timer IRQ0 or NIC IRQ5).
            let line: u8 = if value & 0x40 != 0 {
                value & 0x07
            } else if self.isr != 0 {
                self.isr.trailing_zeros() as u8
            } else {
                return None;
            };
            let bit: u8 = 1 << line;
            if self.isr & bit == 0 {
                return None;
            }
            self.isr &= !bit;
            Some(line)
        } else {
            None
        }
    }

    /// Handles a write to the data port (`0x21`/`0xA1`).
    fn write_data(&mut self, value: u8) {
        match self.icw_step {
            1 => {
                // ICW2: vector base (low three bits are ignored by the 8259A).
                self.base = value & 0xf8;
                self.icw_step = 2;
            },
            2 => {
                // ICW3: cascade wiring (not modelled).
                self.icw_step = if self.icw4 { 3 } else { 0 };
                if self.icw_step == 0 {
                    self.initialised = true;
                }
            },
            3 => {
                // ICW4: mode (not modelled).
                self.icw_step = 0;
                self.initialised = true;
            },
            _ => {
                // OCW1: set the interrupt mask.
                self.imr = value;
            },
        }
    }

    /// Reads the command port: the IRR or ISR, per the last OCW3.
    fn read_cmd(&self) -> u8 {
        if self.read_isr { self.isr } else { self.irr }
    }
}

/// A cascaded master/slave 8259A pair.
#[derive(Default)]
pub struct Pic {
    master: Chip,
    slave: Chip,
}

impl Pic {
    /// Creates a PIC with both controllers uninitialised (masks all-clear).
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if `port` belongs to this device.
    pub fn owns(port: u16) -> bool {
        matches!(
            port,
            PIC_MASTER_CMD | PIC_MASTER_DATA | PIC_SLAVE_CMD | PIC_SLAVE_DATA
        )
    }

    /// Services a guest write of `value` to `port`. Returns the vector acknowledged by an
    /// end-of-interrupt so the caller can issue the matching local-APIC EOI.
    pub fn write(&mut self, port: u16, value: u8) -> Option<u8> {
        match port {
            PIC_MASTER_CMD => self
                .master
                .write_cmd(value)
                .map(|line| self.master.base.wrapping_add(line)),
            PIC_MASTER_DATA => {
                self.master.write_data(value);
                None
            },
            PIC_SLAVE_CMD => {
                self.slave.write_cmd(value);
                None
            },
            PIC_SLAVE_DATA => {
                self.slave.write_data(value);
                None
            },
            _ => None,
        }
    }

    /// Services a guest read from `port`.
    pub fn read(&self, port: u16) -> u8 {
        match port {
            PIC_MASTER_CMD => self.master.read_cmd(),
            PIC_MASTER_DATA => self.master.imr,
            PIC_SLAVE_CMD => self.slave.read_cmd(),
            PIC_SLAVE_DATA => self.slave.imr,
            _ => 0xff,
        }
    }

    ///
    /// # Description
    ///
    /// Requests IRQ0 (the timer line) and, if it can be delivered right now, returns the
    /// interrupt vector to inject into the guest.
    ///
    /// Delivery is suppressed when the controller is not yet initialised, when IRQ0 is masked,
    /// or while any master-PIC line is awaiting the guest's end-of-interrupt. This keeps a steady
    /// host tick from overlapping an active timer or NIC interrupt.
    ///
    pub fn raise_irq0(&mut self) -> Option<u8> {
        // Marking a request in service happens before WHP delivers its fixed interrupt. Keep only
        // one master-PIC line in flight so a queued higher-priority timer cannot consume the
        // non-specific EOI for an active NIC interrupt (leaving the NIC's LAPIC vector stuck).
        if !self.master.initialised || self.master.imr & 0x01 != 0 || self.master.isr != 0 {
            return None;
        }
        self.master.isr |= 0x01;
        self.master.irr &= !0x01;
        Some(self.master.base)
    }

    ///
    /// # Description
    ///
    /// Requests master-PIC IRQ `line` (0..=7) and, if it can be delivered right now, returns the
    /// interrupt vector to inject. Used for the NIC (IRQ5); gating mirrors
    /// [`raise_irq0`](Self::raise_irq0): suppressed while the controller is uninitialised, the line
    /// is masked, or another master-PIC interrupt is in service.
    ///
    pub fn raise_irq(&mut self, line: u8) -> Option<u8> {
        if line > 7 {
            return None;
        }
        let bit: u8 = 1 << line;
        if !self.master.initialised || self.master.imr & bit != 0 || self.master.isr != 0 {
            return None;
        }
        self.master.isr |= bit;
        self.master.irr &= !bit;
        Some(self.master.base.wrapping_add(line))
    }

    /// Rolls back an in-service bit when the corresponding WHP interrupt request fails.
    pub fn cancel_irq(&mut self, line: u8) {
        if line <= 7 {
            self.master.isr &= !(1 << line);
        }
    }

    /// Serializes both controllers' state for a snapshot (16 bytes).
    pub fn save(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(16);
        for chip in [&self.master, &self.slave] {
            out.push(chip.imr);
            out.push(chip.irr);
            out.push(chip.isr);
            out.push(chip.base);
            out.push(chip.icw_step);
            out.push(u8::from(chip.icw4));
            out.push(u8::from(chip.read_isr));
            out.push(u8::from(chip.initialised));
        }
        out
    }

    /// Restores state produced by [`save`](Self::save).
    pub fn load(&mut self, data: &[u8]) {
        if data.len() < 16 {
            return;
        }
        for (i, chip) in [&mut self.master, &mut self.slave].into_iter().enumerate() {
            let b: &[u8] = &data[i * 8..i * 8 + 8];
            chip.imr = b[0];
            chip.irr = b[1];
            chip.isr = b[2];
            chip.base = b[3];
            chip.icw_step = b[4];
            chip.icw4 = b[5] != 0;
            chip.read_isr = b[6] != 0;
            chip.initialised = b[7] != 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_register_round_trips_for_detection() {
        // probe_8259A writes a value to the mask register and reads it back.
        let mut pic = Pic::new();
        pic.write(PIC_MASTER_DATA, 0xfb);
        assert_eq!(pic.read(PIC_MASTER_DATA), 0xfb);
    }

    #[test]
    fn init_sequence_captures_the_vector_base() {
        let mut pic = Pic::new();
        // ICW1 (with ICW4), ICW2 base = 0x30, ICW3, ICW4.
        pic.write(PIC_MASTER_CMD, 0x11);
        pic.write(PIC_MASTER_DATA, 0x30);
        pic.write(PIC_MASTER_DATA, 0x04);
        pic.write(PIC_MASTER_DATA, 0x01);
        // Unmask IRQ0: it now delivers the captured vector base + 0.
        pic.write(PIC_MASTER_DATA, 0xfe);
        assert_eq!(pic.raise_irq0(), Some(0x30), "IRQ0 delivers vector base + 0");
        // Masking suppresses delivery, and unmasking does not bypass the in-service state.
        pic.write(PIC_MASTER_DATA, 0xff);
        assert_eq!(pic.raise_irq0(), None);
        pic.write(PIC_MASTER_DATA, 0xfe);
        assert_eq!(pic.raise_irq0(), None);
        // EOI identifies the vector so the caller can mirror it at the LAPIC, then IRQ0 may fire
        // again.
        assert_eq!(
            pic.write(PIC_MASTER_CMD, 0x20),
            Some(0x30),
            "non-specific EOI acknowledges the IRQ0 vector"
        );
        assert_eq!(pic.raise_irq0(), Some(0x30));
    }

    #[test]
    fn eoi_identifies_the_acknowledged_vector() {
        let mut pic = Pic::new();
        pic.write(PIC_MASTER_CMD, 0x11);
        pic.write(PIC_MASTER_DATA, 0x30);
        pic.write(PIC_MASTER_DATA, 0x04);
        pic.write(PIC_MASTER_DATA, 0x01);
        pic.write(PIC_MASTER_DATA, 0x00);

        assert_eq!(pic.raise_irq(5), Some(0x35));
        assert_eq!(pic.write(PIC_MASTER_CMD, 0x20), Some(0x35));
        assert_eq!(pic.write(PIC_MASTER_CMD, 0x20), None);
    }

    #[test]
    fn serializes_timer_and_nic_interrupts() {
        let mut pic = Pic::new();
        pic.write(PIC_MASTER_CMD, 0x11);
        pic.write(PIC_MASTER_DATA, 0x30);
        pic.write(PIC_MASTER_DATA, 0x04);
        pic.write(PIC_MASTER_DATA, 0x01);
        pic.write(PIC_MASTER_DATA, 0x00);

        assert_eq!(pic.raise_irq(5), Some(0x35));
        assert_eq!(pic.raise_irq0(), None, "timer waits for the NIC EOI");
        assert_eq!(pic.write(PIC_MASTER_CMD, 0x20), Some(0x35));
        assert_eq!(pic.raise_irq0(), Some(0x30));
        assert_eq!(pic.raise_irq(5), None, "NIC waits for the timer EOI");
        assert_eq!(pic.write(PIC_MASTER_CMD, 0x20), Some(0x30));
        assert_eq!(pic.raise_irq(5), Some(0x35));
    }

    #[test]
    fn masked_irq0_is_not_delivered() {
        let mut pic = Pic::new();
        pic.write(PIC_MASTER_CMD, 0x11);
        pic.write(PIC_MASTER_DATA, 0x30);
        pic.write(PIC_MASTER_DATA, 0x04);
        pic.write(PIC_MASTER_DATA, 0x01);
        pic.write(PIC_MASTER_DATA, 0xff); // mask everything, including IRQ0
        assert_eq!(pic.raise_irq0(), None);
    }

}

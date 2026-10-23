//! The sender's side of a pool: see the [module](super) documentation.

use crate::exec::{Unit, Violation, Window};

/// The credit that a receiver granted the sender of one pool.
#[derive(Debug)]
pub(in crate::exec) struct Credit {
    /// The pool's window.
    window: Window,
    /// The units that the sender may still send.
    available: u32,
}

impl Credit {
    /// Returns the credit of a pool with `window`, before the receiver's first grant.
    pub(in crate::exec) fn new(window: Window) -> Self {
        Self {
            window,
            available: 0,
        }
    }

    /// Adds a grant of `units`. A grant of no units, or of more than the units in flight, which
    /// would leave the sender more than the window, is refused and changes nothing.
    pub(in crate::exec) fn grant(&mut self, units: u32) -> Result<(), Violation> {
        if units == 0
            || u64::from(self.available) + u64::from(units) > u64::from(self.window.size())
        {
            return Err(Violation::Credit);
        }
        self.available += units;
        Ok(())
    }

    /// Returns how many of the first `length` queued bytes one data message may carry now: as
    /// many as the bytes of credit, or all of them while a frame of credit is left.
    pub(in crate::exec) fn sendable(&self, length: usize) -> usize {
        match self.window.unit() {
            Unit::Bytes => length.min(self.available as usize),
            Unit::Frames if self.available > 0 => length,
            Unit::Frames => 0,
        }
    }

    /// Spends the credit of one data message of `length` bytes, which
    /// [`sendable`](Self::sendable) allowed.
    pub(in crate::exec) fn spend(&mut self, length: usize) {
        self.available -= self.window.cost(length);
    }
}

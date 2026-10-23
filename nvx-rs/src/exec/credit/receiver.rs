//! The receiver's side of a pool: see the [module](super) documentation.

use crate::exec::{Violation, Window};

/// What the receiver of one pool granted, and what its consumer freed.
#[derive(Debug)]
pub(in crate::exec) struct Granted {
    /// The pool's window.
    window: Window,
    /// The units that the sender may still send: granted, and not used yet.
    open: u32,
    /// The units that the consumer freed, or that the receiver never granted, which it grants
    /// next.
    returnable: u32,
}

impl Granted {
    /// Returns the window of a pool before its first grant, which opens the whole window.
    pub(in crate::exec) fn new(window: Window) -> Self {
        Self {
            window,
            open: 0,
            returnable: window.size(),
        }
    }

    /// Charges a data message of `length` bytes, which the caller checked against the maximum
    /// chunk. A message beyond the units that remain open is refused and changes nothing.
    pub(in crate::exec) fn charge(&mut self, length: usize) -> Result<(), Violation> {
        let cost = self.window.cost(length);
        if cost > self.open {
            return Err(Violation::Window);
        }
        self.open -= cost;
        Ok(())
    }

    /// Frees `units` that the consumer took, to grant them again.
    pub(in crate::exec) fn free(&mut self, units: u32) {
        self.returnable += units;
    }

    /// Returns the units to grant now, if any, and opens them.
    pub(in crate::exec) fn take_grant(&mut self) -> Option<u32> {
        let units = std::mem::take(&mut self.returnable);
        self.open += units;
        (units > 0).then_some(units)
    }
}

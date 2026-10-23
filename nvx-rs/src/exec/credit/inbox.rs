//! The receiver's side of a stream: see the [module](super) documentation.

use std::collections::VecDeque;

use super::units;
use crate::exec::Unit;

/// The bytes that one stream's receiver holds for its consumer, in order, with the bounds of the
/// frames that they arrived in when the pool counts frames.
#[derive(Debug)]
pub(in crate::exec) struct Inbox {
    /// What the pool counts.
    unit: Unit,
    /// The bytes, in order.
    bytes: VecDeque<u8>,
    /// The bytes left of each frame that the inbox holds, oldest first, when the pool counts
    /// frames.
    frames: VecDeque<usize>,
}

impl Inbox {
    /// Returns an empty inbox of a pool that counts `unit`.
    pub(in crate::exec) fn new(unit: Unit) -> Self {
        Self {
            unit,
            bytes: VecDeque::new(),
            frames: VecDeque::new(),
        }
    }

    /// Returns the number of bytes held.
    pub(in crate::exec) fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether the inbox holds nothing.
    pub(in crate::exec) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Appends one data message, which its pool charged.
    pub(in crate::exec) fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes);
        if self.unit == Unit::Frames {
            self.frames.push_back(bytes.len());
        }
    }

    /// Returns the bytes at the front, as many as are contiguous: a part of what the inbox holds
    /// while it holds anything.
    pub(in crate::exec) fn front(&self) -> &[u8] {
        self.bytes.as_slices().0
    }

    /// Removes the first `count` bytes, at most as many as the inbox holds, and returns the units
    /// that this frees: each byte, or each frame whose last byte it removes.
    pub(in crate::exec) fn pop(&mut self, count: usize) -> u32 {
        let count = count.min(self.bytes.len());
        self.bytes.drain(..count);
        match self.unit {
            Unit::Bytes => units(count),
            Unit::Frames => {
                let mut left = count;
                let mut freed = 0;
                while left > 0 {
                    let Some(frame) = self.frames.front_mut() else {
                        break;
                    };
                    let taken = left.min(*frame);
                    *frame -= taken;
                    left -= taken;
                    if *frame == 0 {
                        self.frames.pop_front();
                        freed += 1;
                    }
                }
                freed
            }
        }
    }

    /// Removes everything, releases the storage, and returns how many bytes it removed and the
    /// units that this frees.
    pub(in crate::exec) fn clear(&mut self) -> (usize, u32) {
        let length = self.bytes.len();
        let freed = self.pop(length);
        self.bytes = VecDeque::new();
        (length, freed)
    }
}

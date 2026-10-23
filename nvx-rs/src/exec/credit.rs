//! The credit arithmetic of an execution's pools, and the bounded queues of its streams.
//!
//! A sender holds [`Credit`]: the units of a pool that the receiver granted it and that it has
//! not used, which it spends on each data message it sends. A receiver holds [`Granted`]: the
//! units that it granted and that the sender has not used yet, which bound each data message it
//! accepts, and the units that its consumer freed, which it grants again. Both start from the
//! pool's window: the receiver's first grant opens the whole window, and a grant can never open
//! more than the window, so the units in flight never exceed it.

mod inbox;
mod receiver;
mod sender;

use std::collections::VecDeque;

pub(super) use self::inbox::Inbox;
pub(super) use self::receiver::Granted;
pub(super) use self::sender::Credit;

/// Returns `count` bytes as units of a byte pool, which a chunk's bound keeps within `u32`.
pub(super) fn units(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Moves the first bytes of `queue` into `buffer`, at most `length` of them, and returns how
/// many it moved.
pub(super) fn take(queue: &mut VecDeque<u8>, buffer: &mut [u8], length: usize) -> usize {
    let length = length.min(buffer.len()).min(queue.len());
    let (first, second) = queue.as_slices();
    let head = length.min(first.len());
    buffer[..head].copy_from_slice(&first[..head]);
    buffer[head..length].copy_from_slice(&second[..length - head]);
    queue.drain(..length);
    length
}

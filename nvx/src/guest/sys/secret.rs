//! The erasure of secrets, such as a restore's seed, from memory that the process still owns.

use std::ptr;
use std::sync::atomic::{Ordering, compiler_fence};

/// Overwrites `bytes` with zeros, with one volatile write per byte, which the compiler keeps
/// even when nothing reads the bytes again, as before their memory is freed, and a compiler
/// fence, which keeps the writes before the code that follows, such as that free.
pub(crate) fn zeroize(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: `byte` is an exclusive reference, so it points to one initialized, aligned,
        // and writable byte.
        unsafe { ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests;

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
mod tests {
    use super::*;

    #[test]
    fn zeroize_overwrites_every_byte_of_its_slice_alone() {
        let mut bytes: [u8; 67] = std::array::from_fn(|index| index as u8 | 1);
        zeroize(&mut bytes[1..66]);
        assert_eq!((bytes[0], bytes[66]), (1, 67));
        assert!(bytes[1..66].iter().all(|&byte| byte == 0));
        zeroize(&mut []);
    }
}

//! ARM binding admission over the same caller-owned platform-window transaction.

use super::{SampleError, lock_portb};
use crate::guest::counter::{ClockContext, ClockError};
use crate::guest::sys::aarch64::{ClockBindingError, PlatformDevice};
use crate::guest::sys::{self, FileLock, FinishTimeAccess, kernel};

/// Checks the actual clocksource, selected binding and CNTFRQ under an existing or owned lock.
pub(super) fn clock_context(window_lock: Option<&FileLock>) -> Result<ClockContext, SampleError> {
    let source = sys::read_line(kernel::CURRENT_CLOCKSOURCE).map_err(SampleError::Device)?;
    if source != b"arch_sys_counter" {
        return Err(SampleError::Binding(ClockError::Invalid(
            "ARM kernel clocksource must be arch_sys_counter",
        )));
    }
    let owned_lock;
    let lock = if let Some(lock) = window_lock {
        lock
    } else {
        owned_lock = lock_portb().map_err(SampleError::Device)?;
        &owned_lock
    };
    let device = PlatformDevice::open().map_err(SampleError::Device)?;
    let context = device
        .read_clock_context(lock)
        .map_err(|error| match error {
            ClockBindingError::Device(error) => SampleError::Device(error),
            ClockBindingError::Invalid(error) => SampleError::Binding(error),
        });
    device.finish().map_err(SampleError::Device)?;
    context
}

#[cfg(test)]
mod tests;

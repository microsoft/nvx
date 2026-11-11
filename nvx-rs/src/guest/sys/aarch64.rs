//! Architected EL0 counter reads, not CPU-profile or backend qualification.

use super::Errno;

/// Reads the architectural 32-bit counter frequency without substituting a host observation.
pub(crate) fn counter_frequency_hz() -> Result<u64, Errno> {
    let frequency: u64;
    // SAFETY: Linux permits this architected EL0 read or emulates it through the timer driver.
    unsafe {
        core::arch::asm!(
            "isb",
            "mrs {frequency}, cntfrq_el0",
            frequency = out(reg) frequency,
            options(nostack, preserves_flags),
        );
    }
    if frequency == 0 || frequency > u64::from(u32::MAX) {
        return Err(Errno::EPROTO);
    }
    Ok(frequency)
}

/// Reads the virtual counter between instruction barriers that also order compiler accesses.
pub(crate) fn virtual_counter() -> u64 {
    let counter: u64;
    // SAFETY: Linux permits this architected EL0 read or emulates it through the timer driver.
    unsafe {
        core::arch::asm!(
            "isb",
            "mrs {counter}, cntvct_el0",
            "isb",
            counter = out(reg) counter,
            options(nostack, preserves_flags),
        );
    }
    counter
}

#[cfg(test)]
mod tests;

//! Borrowed scalar access to the actual time window, without an ISA-specific address carrier.

use super::Errno;

#[cfg(target_arch = "x86_64")]
/// Scoped x86 I/O-port permission for a time transaction.
pub(crate) type TimeAccess<'a> = super::PortAccess<'a>;
#[cfg(target_arch = "x86_64")]
/// Failure to acquire or release x86 time-port permissions.
pub(crate) type TimeAccessError = super::PortError;
#[cfg(target_arch = "aarch64")]
/// Owned restricted platform descriptor for a time transaction.
pub(crate) type TimeAccess<'a> = super::aarch64::PlatformDevice;
#[cfg(target_arch = "aarch64")]
/// Failure to access or close the restricted platform descriptor.
pub(crate) type TimeAccessError = Errno;

/// Explicit completion of the architecture's real access owner.
pub(crate) trait FinishTimeAccess {
    /// Releases port permission or closes the restricted descriptor, reporting failure.
    fn finish(self) -> Result<(), TimeAccessError>;
}

/// Shared generation FIFO and diagnostic access, separate from an ISA-specific address.
pub(crate) trait TimeChannel: TimeWindow {
    /// Reads one generation/identity word at the hardware-required width.
    fn read_data_word(&self) -> Result<u32, Errno>;
    /// Writes one diagnostic byte without admitting wider access.
    fn write_debug_byte(&self, byte: u8) -> Result<(), Errno>;
}

/// Fallible scalar transactions shared by the x86 port window and ARM platform device.
pub(crate) trait TimeWindow {
    /// Reads the one-byte capability/status register.
    fn window_status(&self) -> Result<u8, Errno>;
    /// Selects a transaction while the caller owns the shared window lock.
    fn select_window(&self, selector: u8) -> Result<(), Errno>;
    /// Reads and advances the selected window by one 32-bit word.
    fn read_window_word(&self) -> Result<u32, Errno>;

    /// Reads successive little-endian words, copying only the requested trailing bytes.
    fn read_window(&self, bytes: &mut [u8]) -> Result<(), Errno> {
        for chunk in bytes.chunks_mut(size_of::<u32>()) {
            let word = self.read_window_word()?.to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
impl TimeWindow for super::PortAccess<'_> {
    fn window_status(&self) -> Result<u8, Errno> {
        Ok(self.read_u8(super::Port::PortbStatus))
    }

    fn select_window(&self, selector: u8) -> Result<(), Errno> {
        self.write_u8(super::Port::PortbStatus, selector);
        Ok(())
    }

    fn read_window_word(&self) -> Result<u32, Errno> {
        Ok(self.read_u32(super::Port::PortbWindow))
    }
}

#[cfg(target_arch = "aarch64")]
impl TimeWindow for super::aarch64::PlatformDevice {
    fn window_status(&self) -> Result<u8, Errno> {
        self.status()
    }

    fn select_window(&self, selector: u8) -> Result<(), Errno> {
        self.select(selector)
    }

    fn read_window_word(&self) -> Result<u32, Errno> {
        self.read_time_word()
    }
}

#[cfg(target_arch = "x86_64")]
impl FinishTimeAccess for super::PortAccess<'_> {
    fn finish(self) -> Result<(), TimeAccessError> {
        self.release()
    }
}

#[cfg(target_arch = "aarch64")]
impl FinishTimeAccess for super::aarch64::PlatformDevice {
    fn finish(self) -> Result<(), TimeAccessError> {
        self.close()
    }
}

#[cfg(target_arch = "x86_64")]
impl TimeChannel for super::PortAccess<'_> {
    fn read_data_word(&self) -> Result<u32, Errno> {
        Ok(self.read_u32(super::Port::PortbData))
    }

    fn write_debug_byte(&self, byte: u8) -> Result<(), Errno> {
        self.write_u8(super::Port::PortbData, byte);
        Ok(())
    }
}

#[cfg(target_arch = "aarch64")]
impl TimeChannel for super::aarch64::PlatformDevice {
    fn read_data_word(&self) -> Result<u32, Errno> {
        super::aarch64::PlatformDevice::read_data_word(self)
    }

    fn write_debug_byte(&self, byte: u8) -> Result<(), Errno> {
        self.write_console(&[byte])
    }
}

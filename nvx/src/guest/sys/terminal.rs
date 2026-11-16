//! Safe terminal configuration and output-drain operations.

use nix::sys::termios::{ControlFlags, SetArg, cfmakeraw, tcdrain, tcgetattr, tcsetattr};

use super::{Errno, FileDescriptor};

/// Configures a terminal for raw local operation.
pub(crate) fn configure_raw(descriptor: &FileDescriptor) -> Result<(), Errno> {
    let mut settings = tcgetattr(descriptor.borrowed())?;
    cfmakeraw(&mut settings);
    settings.control_flags |= ControlFlags::CLOCAL;
    tcsetattr(descriptor.borrowed(), SetArg::TCSANOW, &settings)
}

/// Waits until all terminal output queued on `descriptor` has been transmitted.
pub(crate) fn drain(descriptor: &FileDescriptor) -> Result<(), Errno> {
    tcdrain(descriptor.borrowed())
}

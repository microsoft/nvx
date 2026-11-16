use std::ffi::CStr;

use crate::guest::sys::{Errno, FileDescriptor};
use crate::guest::terminal::Terminal;

use super::seam::System;

/// Opens the control terminal, waiting while the kernel sets it up during boot, as
/// [`Terminal::open`] does.
pub(crate) fn open(path: &CStr) -> Result<FileDescriptor, Errno> {
    Terminal::open_c(path).map(Terminal::into_descriptor)
}

/// Applies raw local-terminal settings through the replaceable seam.
pub(crate) fn configure(
    system: &mut impl System,
    descriptor: &FileDescriptor,
) -> Result<(), Errno> {
    system.configure_tty(descriptor)
}

/// Best-effort drains queued control-terminal output.
pub(crate) fn drain(system: &mut impl System, descriptor: &FileDescriptor) {
    let _ = system.drain_tty(descriptor);
}

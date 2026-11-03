//! The guest's descriptor and socket primitives, which `nvx`'s guest OS layer re-exports.

pub mod fd;
pub mod socket;

use nix::errno::Errno;

use fd::FileDescriptor;

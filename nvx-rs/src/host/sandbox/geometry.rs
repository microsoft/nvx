//! The block geometry of a sandbox block device, which the checks of its files read as OpenVMM
//! presents it, and which a template records for the devices of its capture, from
//! [`config`](crate::host::config), where a capture's configuration names it.
//! [`template`](crate::host::template) re-exports it.

pub use crate::host::config::BlockGeometry;

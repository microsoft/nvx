//! The block geometry of a sandbox block device, which the checks of its files read as OpenVMM
//! presents it, and which a template records for the devices of its capture.
//! [`template`](crate::host::template) re-exports what is public here.

/// The length and block sizes of a sandbox block device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockGeometry {
    /// The length that the device presents, in bytes: nonzero, and a multiple of the logical
    /// block size.
    pub length: u64,
    /// The logical block size, in bytes: a power of two of at least 512.
    pub logical_block_size: u32,
    /// The physical block size, in bytes: a power of two of at least the logical block size.
    pub physical_block_size: u32,
}

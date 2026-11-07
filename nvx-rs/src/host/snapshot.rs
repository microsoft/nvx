//! A snapshot from which a VM may later be restored.

/// An opaque snapshot input. Creating a VM from a snapshot is not supported yet.
#[derive(Debug)]
pub struct Snapshot {
    /// Reserved for snapshot metadata.
    pub(crate) _private: (),
}

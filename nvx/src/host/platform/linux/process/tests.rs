use super::*;

impl ProcessHandle {
    /// Returns a handle without a pidfd, as hosts without pidfds have.
    pub(crate) fn without_pidfd() -> Self {
        Self(None)
    }
}

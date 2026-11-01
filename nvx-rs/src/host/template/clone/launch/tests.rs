use super::*;

impl<'s> Restoring<'s> {
    /// Returns the launch, which a test commits as a clone's restore does once its VMM runs.
    pub(crate) fn into_starting(self) -> Starting<'s> {
        self.starting
    }
}

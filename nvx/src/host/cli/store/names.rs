//! The files that the sandbox commands keep in a state directory.

use crate::host::{vm, vm_directory};

#[cfg(test)]
mod tests;

/// The prefix of a staging file that the `tempfile` crate's `NamedTempFile::new_in` names, as
/// OpenVMM's outcome report is staged.
const RANDOM_STAGING_PREFIX: &str = ".tmp";

/// The random ASCII letters and digits that the `tempfile` crate puts after
/// [`RANDOM_STAGING_PREFIX`] by default.
const RANDOM_STAGING_CHARS: usize = 6;

/// The suffix of a staging file named `.FILE.SUFFIX.tmp`, as [`temporary_path`] and
/// [`platform::write_private`] name them.
///
/// [`temporary_path`]: crate::host::cli::temporary_path
/// [`platform::write_private`]: crate::host::platform::write_private
const NAMED_STAGING_SUFFIX: &str = ".tmp";

/// Returns whether `name` is that of a staging file that `tempfile` names: `.tmp` and
/// [`RANDOM_STAGING_CHARS`] ASCII letters or digits.
pub(in crate::host::cli) fn is_random_staging(name: &str) -> bool {
    name.strip_prefix(RANDOM_STAGING_PREFIX)
        .is_some_and(|random| {
            random.len() == RANDOM_STAGING_CHARS
                && random.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

/// Splits the name of a staging file named `.FILE.SUFFIX.tmp` into `FILE` and `SUFFIX`, at the
/// last dot before `.tmp`, or returns `None` for a name of another form.
pub(in crate::host::cli) fn split_staging(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix('.')
        .and_then(|name| name.strip_suffix(NAMED_STAGING_SUFFIX))
        .and_then(|name| name.rsplit_once('.'))
}

/// A file that the sandbox commands keep in a state directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StateFile {
    /// The configuration of the provisioned sandbox, which `provision` writes, `start` reads, and
    /// `deprovision` removes.
    Config,
    /// The lock that serializes the sandbox's lifecycle transitions.
    Lock,
    /// The journal of a start in progress, or of one that was interrupted.
    Journal,
    /// The runtime record of the sandbox's running VM, through which `exec` and `stop` find it.
    Runtime,
    /// The control capability of the running VM, with which `exec` and `stop` connect to it.
    Capability,
    /// The control endpoint of the running VM, on Linux.
    Socket,
    /// OpenVMM's output, which each start replaces.
    Log,
    /// OpenVMM's outcome report of the latest start.
    Outcome,
    /// The activation record that the latest start of a restored sandbox handed its guest in its
    /// restore packet.
    ActivationRecord,
}

impl StateFile {
    pub(in crate::host::cli) fn owns_temporary(name: &str) -> bool {
        is_random_staging(name)
            || split_staging(name).is_some_and(|(file, suffix)| {
                Self::ALL.iter().any(|name| name.name() == file)
                    && (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                        || file == Self::Config.name()
                            && suffix.len() == 32
                            && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()))
            })
    }

    /// Every file that the sandbox commands keep in a state directory.
    pub(in crate::host::cli) const ALL: [Self; 9] = [
        Self::Config,
        Self::Lock,
        Self::Journal,
        Self::Runtime,
        Self::Capability,
        Self::Socket,
        Self::Log,
        Self::Outcome,
        Self::ActivationRecord,
    ];

    /// Returns the name of the file in the state directory.
    pub(in crate::host::cli) const fn name(self) -> &'static str {
        match self {
            Self::Config => "config.json",
            Self::Lock => "sandbox.lock",
            Self::Journal => "launch.json",
            Self::Runtime => "runtime.json",
            Self::Capability => "control.capability",
            Self::Socket => vm_directory::CONTROL,
            Self::Log => vm_directory::LOG,
            Self::Outcome => vm_directory::OUTCOME,
            Self::ActivationRecord => vm::ACTIVATION_RECORD,
        }
    }
}

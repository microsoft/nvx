//! The files that the sandbox commands keep in a state directory.

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
}

impl StateFile {
    pub(crate) fn owns_temporary(name: &str) -> bool {
        name.strip_prefix(".tmp").is_some_and(|suffix| {
            suffix.len() == 6 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        }) || name
            .strip_prefix('.')
            .and_then(|name| name.strip_suffix(".tmp"))
            .and_then(|name| name.rsplit_once('.'))
            .is_some_and(|(file, suffix)| {
                Self::ALL.iter().any(|name| name.name() == file)
                    && (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                        || file == Self::Config.name()
                            && suffix.len() == 32
                            && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()))
            })
    }

    /// Every file that the sandbox commands keep in a state directory.
    pub(crate) const ALL: [Self; 8] = [
        Self::Config,
        Self::Lock,
        Self::Journal,
        Self::Runtime,
        Self::Capability,
        Self::Socket,
        Self::Log,
        Self::Outcome,
    ];

    /// Returns the name of the file in the state directory.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Config => "config.json",
            Self::Lock => "sandbox.lock",
            Self::Journal => "launch.json",
            Self::Runtime => "runtime.json",
            Self::Capability => "control.capability",
            Self::Socket => "control.sock",
            Self::Log => "openvmm.log",
            Self::Outcome => "outcome.json",
        }
    }
}

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use super::artifacts::{Artifacts, absolute};
use super::launch;
use crate::error::{Error, Result};

/// Hypervisor that OpenVMM uses to run the sandbox VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hypervisor {
    /// Linux KVM through `/dev/kvm`.
    Kvm,
    /// Linux Microsoft Hypervisor through `/dev/mshv`.
    Mshv,
    /// Windows Hypervisor Platform.
    Whp,
}

impl Hypervisor {
    /// Returns the OpenVMM spelling of this hypervisor.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kvm => "kvm",
            Self::Mshv => "mshv",
            Self::Whp => "whp",
        }
    }

    /// Environment variable that overrides [`Hypervisor::from_env_or_default`]: `kvm`, `mshv`, or
    /// `whp`.
    pub const ENV: &'static str = "NVX_HYPERVISOR";

    /// Returns the hypervisor named by [`Hypervisor::ENV`], or the
    /// [platform default](Self::platform_default) when the variable is unset.
    ///
    /// Fails with [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable) when the
    /// variable names an unknown hypervisor or the host has no default.
    pub fn from_env_or_default() -> Result<Self> {
        match std::env::var(Self::ENV) {
            Ok(value) if !value.is_empty() => value.parse(),
            Ok(_) | Err(std::env::VarError::NotPresent) => {
                Self::platform_default().ok_or_else(|| {
                    Error::backend_unavailable(format!(
                        "this host has no default hypervisor; set {}",
                        Self::ENV
                    ))
                })
            }
            Err(error) => Err(Error::backend_unavailable(format!(
                "{} is not valid Unicode",
                Self::ENV
            ))
            .with_source(error)),
        }
    }

    /// Returns the conventional hypervisor for this host: KVM on Linux and WHP on Windows.
    pub fn platform_default() -> Option<Self> {
        if cfg!(target_os = "linux") {
            Some(Self::Kvm)
        } else if cfg!(windows) {
            Some(Self::Whp)
        } else {
            None
        }
    }

    fn supported_on_host(self) -> bool {
        match self {
            Self::Kvm | Self::Mshv => cfg!(target_os = "linux"),
            Self::Whp => cfg!(windows),
        }
    }
}

impl fmt::Display for Hypervisor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Hypervisor {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "kvm" => Ok(Self::Kvm),
            "mshv" => Ok(Self::Mshv),
            "whp" => Ok(Self::Whp),
            _ => Err(Error::backend_unavailable(format!(
                "unknown hypervisor {value:?}; choose kvm, mshv, or whp"
            ))),
        }
    }
}

/// Configuration of the [`OpenVmmBackend`](super::OpenVmmBackend).
///
/// Construct it with [`OpenVmmConfig::discover`], [`OpenVmmConfig::from_artifacts`],
/// [`OpenVmmConfig::new`], [`OpenVmmConfig::from_release_dir`], or
/// [`OpenVmmConfig::from_repo_layout`], then adjust the public fields. Defaults match
/// `scripts/nvx.py sandbox`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpenVmmConfig {
    /// The `openvmm` executable.
    pub openvmm: PathBuf,
    /// The NVX guest kernel (`vmlinux`).
    pub kernel: PathBuf,
    /// The NVX Alpine initramfs (`initramfs.cpio.gz`): the guest userland and its control agent.
    pub initrd: PathBuf,
    /// Hypervisor used by OpenVMM.
    pub hypervisor: Hypervisor,
    /// Directory that holds per-sandbox state. Choose a directory private to the current user.
    pub state_root: PathBuf,
    /// Default guest memory in MiB. Provision requests may override it.
    pub memory_mib: u32,
    /// Non-root user ID of workloads. The account must exist in the guest's `/etc/passwd`.
    pub workload_uid: u32,
    /// Primary group ID of workloads.
    pub workload_gid: u32,
    /// Linux hosts only: run the workloads of a sandbox that maps host paths under the calling
    /// user's own IDs, so host file permissions apply to them as they do to that user.
    ///
    /// virtio-fs shows host owners and modes unchanged on Linux, so [`Self::workload_uid`] could
    /// otherwise not read private files or write to the user's directories. The guest creates an
    /// account for the IDs if its image lacks a usable one, replacing an image account for the
    /// UID whose primary group or home does not fit. Root callers keep the configured identity.
    /// Windows hosts present mapped files as accessible to every guest user and ignore this.
    pub map_host_identity: bool,
    /// UTS hostname of workloads.
    pub hostname: String,
    /// Static guest IPv4 address and prefix used when a network device is attached.
    pub guest_network: String,
    /// Extra kernel parameters. `nvx_*`, `tsc=`, and `hostname=` tokens are reserved.
    ///
    /// They share the guest's 1024-byte kernel command line with the `hostname=` token and the
    /// bind mounts of mapped host paths. A configuration whose parameters and hostname alone do
    /// not fit is rejected.
    pub kernel_command_line: String,
    /// Time allowed for the guest agent to become ready after `start`.
    pub start_timeout: Duration,
    /// Time allowed to connect to a sandbox's control endpoint and receive a cancellation
    /// outcome.
    pub control_timeout: Duration,
    /// Time allowed for a graceful stop before the VM is terminated.
    pub stop_timeout: Duration,
    /// Extra time to wait for an outcome after a workload's own timeout elapses.
    pub exec_response_grace: Duration,
    /// Windows only: start OpenVMM outside the caller's job object so the sandbox VM survives
    /// the caller. The job must permit breakaway.
    pub breakaway_from_job: bool,
    /// Test hook: skips the hypervisor check so tests can drive a fake `openvmm` executable on
    /// hosts without a hypervisor. Available only with the `testing` feature.
    #[cfg(feature = "testing")]
    pub skip_hypervisor_probe: bool,
}

impl OpenVmmConfig {
    /// Default guest memory in MiB.
    pub const DEFAULT_MEMORY_MIB: u32 = 256;
    /// Default workload user and group ID (Alpine's `nobody`).
    pub const DEFAULT_WORKLOAD_ID: u32 = 65534;
    /// Default workload hostname.
    pub const DEFAULT_HOSTNAME: &'static str = "nvx-sandbox";
    /// Default guest network address.
    pub const DEFAULT_GUEST_NETWORK: &'static str = "10.0.0.2/24";
    /// Longest accepted `start_timeout`, `control_timeout`, `stop_timeout`, or
    /// `exec_response_grace`: 30 days.
    ///
    /// The bound keeps every deadline derived from these timeouts, including a workload's own
    /// timeout plus `exec_response_grace`, representable as an [`Instant`](std::time::Instant) and
    /// within the roughly 49-day range of Windows' millisecond waits.
    pub const MAX_TIMEOUT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

    /// Creates a configuration from explicit artifact paths.
    pub fn new(
        openvmm: impl Into<PathBuf>,
        kernel: impl Into<PathBuf>,
        initrd: impl Into<PathBuf>,
        hypervisor: Hypervisor,
        state_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            openvmm: openvmm.into(),
            kernel: kernel.into(),
            initrd: initrd.into(),
            hypervisor,
            state_root: state_root.into(),
            memory_mib: Self::DEFAULT_MEMORY_MIB,
            workload_uid: Self::DEFAULT_WORKLOAD_ID,
            workload_gid: Self::DEFAULT_WORKLOAD_ID,
            map_host_identity: true,
            hostname: Self::DEFAULT_HOSTNAME.to_owned(),
            guest_network: Self::DEFAULT_GUEST_NETWORK.to_owned(),
            kernel_command_line: String::new(),
            start_timeout: Duration::from_secs(60),
            control_timeout: Duration::from_secs(60),
            stop_timeout: Duration::from_secs(30),
            exec_response_grace: Duration::from_secs(30),
            breakaway_from_job: false,
            #[cfg(feature = "testing")]
            skip_hypervisor_probe: false,
        }
    }

    /// Creates a configuration from located [`Artifacts`].
    pub fn from_artifacts(
        artifacts: Artifacts,
        hypervisor: Hypervisor,
        state_root: impl Into<PathBuf>,
    ) -> Self {
        Self::new(
            artifacts.openvmm,
            artifacts.kernel,
            artifacts.initrd,
            hypervisor,
            state_root,
        )
    }

    /// Creates a configuration with no NVX-specific input: the artifacts come from
    /// [`Artifacts::discover`], the hypervisor from [`Hypervisor::from_env_or_default`], and the
    /// state root from [`OpenVmmConfig::default_state_root`].
    pub fn discover() -> Result<Self> {
        Ok(Self::from_artifacts(
            Artifacts::discover()?,
            Hypervisor::from_env_or_default()?,
            Self::default_state_root()?,
        ))
    }

    /// Uses the artifacts of an extracted NVX release archive (`bin/` and `guest/`).
    ///
    /// Fails with [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable) when
    /// the release's `SOURCE-MANIFEST.json` declares an incompatible control contract.
    pub fn from_release_dir(
        release_dir: impl AsRef<Path>,
        hypervisor: Hypervisor,
        state_root: impl Into<PathBuf>,
    ) -> Result<Self> {
        Ok(Self::from_artifacts(
            Artifacts::from_release_dir(release_dir)?,
            hypervisor,
            state_root,
        ))
    }

    /// Uses the artifacts of an NVX repository checkout after `nvx.py download` or a local build.
    pub fn from_repo_layout(
        repo_root: impl AsRef<Path>,
        hypervisor: Hypervisor,
        state_root: impl Into<PathBuf>,
    ) -> Result<Self> {
        Ok(Self::from_artifacts(
            Artifacts::from_repo_layout(repo_root)?,
            hypervisor,
            state_root,
        ))
    }

    /// Returns the conventional per-user state root.
    ///
    /// This is `%LOCALAPPDATA%\nvx\sandboxes` on Windows and
    /// `$XDG_STATE_HOME/nvx/sandboxes` (default `~/.local/state/nvx/sandboxes`) elsewhere.
    pub fn default_state_root() -> Result<PathBuf> {
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|home| PathBuf::from(home).join(".local").join("state"))
                })
        };
        base.filter(|path| path.is_absolute())
            .map(|path| path.join("nvx").join("sandboxes"))
            .ok_or_else(|| {
                Error::backend_unavailable("cannot determine the per-user NVX state directory")
            })
    }

    /// Resolves relative paths and checks every field.
    pub(crate) fn normalized(mut self) -> Result<Self> {
        self.openvmm = absolute(&self.openvmm)?;
        self.kernel = absolute(&self.kernel)?;
        self.initrd = absolute(&self.initrd)?;
        self.state_root = absolute(&self.state_root)?;
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<()> {
        if !self.hypervisor.supported_on_host() {
            return Err(Error::backend_unavailable(format!(
                "the {} hypervisor is not available on this host",
                self.hypervisor
            )));
        }
        let invalid = |message: String| Err(Error::backend_unavailable(message));
        if self.memory_mib == 0 {
            return invalid("memory_mib must be positive".to_owned());
        }
        if self.workload_uid == 0 || self.workload_gid == 0 {
            return invalid("workloads must run as a non-root UID and GID".to_owned());
        }
        if !valid_hostname(&self.hostname) {
            return invalid(format!(
                "hostname {:?} must be a lowercase RFC 1123 label of up to 63 characters",
                self.hostname
            ));
        }
        if !valid_guest_network(&self.guest_network) {
            return invalid(format!(
                "guest_network {:?} must be an IPv4 address with a /1 to /30 prefix",
                self.guest_network
            ));
        }
        if let Some(problem) = kernel_command_line_problem(&self.kernel_command_line) {
            return invalid(format!("kernel_command_line {problem}"));
        }
        // Every sandbox's command line starts with these parameters and the hostname, and ends
        // with a NUL; provisioning then checks the tokens that each sandbox adds.
        let base = launch::base_command_line(&self.kernel_command_line, &self.hostname);
        if base.len() + 1 > launch::COMMAND_LINE_BUDGET {
            return invalid(format!(
                "kernel_command_line and the hostname leave no room in the {}-byte kernel \
                 command line",
                launch::COMMAND_LINE_BUDGET
            ));
        }
        for (name, value, positive) in [
            ("start_timeout", self.start_timeout, true),
            ("control_timeout", self.control_timeout, true),
            ("stop_timeout", self.stop_timeout, true),
            ("exec_response_grace", self.exec_response_grace, false),
        ] {
            if positive && value.is_zero() {
                return invalid(format!("{name} must be positive"));
            }
            if value > Self::MAX_TIMEOUT {
                return invalid(format!(
                    "{name} must be at most {} days",
                    Self::MAX_TIMEOUT.as_secs() / (24 * 60 * 60)
                ));
            }
        }
        if cfg!(unix) {
            // sockaddr_un holds 108 bytes including the terminating NUL.
            let socket = self
                .state_root
                .join("0".repeat(32))
                .join(super::state::SOCKET_NAME);
            if socket.as_os_str().len() >= 108 {
                return invalid(format!(
                    "state_root {} is too long for a Unix control socket path",
                    self.state_root.display()
                ));
            }
        }
        Ok(())
    }
}

fn valid_hostname(hostname: &str) -> bool {
    let bytes = hostname.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && bytes.first() != Some(&b'-')
        && bytes.last() != Some(&b'-')
}

fn valid_guest_network(value: &str) -> bool {
    let Some((address, prefix)) = value.split_once('/') else {
        return false;
    };
    address.parse::<std::net::Ipv4Addr>().is_ok()
        && prefix
            .parse::<u8>()
            .is_ok_and(|prefix| (1..=30).contains(&prefix))
        && !prefix.starts_with('0')
}

/// Describes why extra kernel parameters are unacceptable, if they are.
pub(crate) fn kernel_command_line_problem(value: &str) -> Option<String> {
    if value.contains('\0') {
        return Some("contains a NUL character".to_owned());
    }
    if value.contains(['"', '\'']) {
        return Some("must not contain quotes".to_owned());
    }
    value.split_whitespace().find_map(|token| {
        if token == "--" {
            Some("must not contain the -- delimiter".to_owned())
        } else if token.starts_with("nvx_")
            || token.starts_with("tsc=")
            || token.starts_with("hostname=")
        {
            Some(format!("uses the reserved token {token:?}"))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    #[test]
    fn validation_rejects_bad_settings() {
        let Some(hypervisor) = Hypervisor::platform_default() else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let base = OpenVmmConfig::new("openvmm", "vmlinux", "initrd", hypervisor, directory.path());
        base.clone().normalized().unwrap();
        let mut cases = Vec::new();
        let mut config = base.clone();
        config.memory_mib = 0;
        cases.push(config);
        let mut config = base.clone();
        config.workload_uid = 0;
        cases.push(config);
        let mut config = base.clone();
        config.hostname = "Bad_Host".to_owned();
        cases.push(config);
        let mut config = base.clone();
        config.guest_network = "10.0.0.2/31".to_owned();
        cases.push(config);
        let mut config = base.clone();
        config.kernel_command_line = "quiet nvx_exec=/bin/sh".to_owned();
        cases.push(config);
        // The parameters, a space, the hostname token, and a NUL fill the command line exactly.
        let room =
            launch::COMMAND_LINE_BUDGET - launch::base_command_line("", &base.hostname).len() - 2;
        let mut fitting = base.clone();
        fitting.kernel_command_line = "x".repeat(room);
        fitting.normalized().unwrap();
        let mut config = base.clone();
        config.kernel_command_line = "x".repeat(room + 1);
        cases.push(config);
        let mut config = base.clone();
        config.hypervisor = if hypervisor == Hypervisor::Whp {
            Hypervisor::Kvm
        } else {
            Hypervisor::Whp
        };
        cases.push(config);
        let timeouts: [fn(&mut OpenVmmConfig, Duration); 4] = [
            |config, value| config.start_timeout = value,
            |config, value| config.control_timeout = value,
            |config, value| config.stop_timeout = value,
            |config, value| config.exec_response_grace = value,
        ];
        let mut longest = base.clone();
        for set in timeouts {
            for value in [
                OpenVmmConfig::MAX_TIMEOUT + Duration::from_nanos(1),
                Duration::MAX,
            ] {
                let mut config = base.clone();
                set(&mut config, value);
                cases.push(config);
            }
            set(&mut longest, OpenVmmConfig::MAX_TIMEOUT);
        }
        longest.normalized().unwrap();
        for config in cases {
            assert_eq!(
                config.clone().normalized().unwrap_err().code(),
                ErrorCode::BackendUnavailable,
                "{config:?}"
            );
        }
    }

    #[test]
    fn helpers_accept_expected_values() {
        assert!(valid_hostname("nvx-sandbox"));
        assert!(!valid_hostname("-nvx"));
        assert!(!valid_hostname(&"a".repeat(64)));
        assert!(valid_guest_network("10.0.0.2/24"));
        assert!(!valid_guest_network("10.0.0.2"));
        assert!(!valid_guest_network("10.0.0.256/24"));
        assert!(kernel_command_line_problem("quiet loglevel=0").is_none());
        assert!(kernel_command_line_problem("tsc=reliable").is_some());
        assert!(kernel_command_line_problem("hostname=other").is_some());
        assert!(kernel_command_line_problem("a -- b").is_some());
        assert_eq!("mshv".parse::<Hypervisor>().unwrap(), Hypervisor::Mshv);
    }
}

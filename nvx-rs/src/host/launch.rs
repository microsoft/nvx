//! OpenVMM command line of a managed microVM, and the inputs that an adapter's launch shares.
//!
//! The VM runs the guest's own Alpine Linux userland directly, with no sandbox layers or scratch
//! disk. OpenVMM's managed lifecycle adds the control tty and workload identity tokens; the guest
//! init then starts the managed agent, which runs workloads as the configured non-root identity.
//! The arguments mirror the managed-lifecycle check of `scripts/nvx_tools/microvm_tests.py`.
//!
//! [`LaunchSpec::check_process_command_line`] holds a managed microVM's command line to
//! [`PROCESS_COMMAND_LINE_LIMIT`], the longest that Windows starts a process with, on every host,
//! so that a launch behaves alike on all of them, as [`process_command_line_units`] counts it and
//! [`fit_process_command_line`] checks it, and any other caller may check a command line with
//! them.
//!
//! An adapter's [`PreparedLaunch`](super::sandbox::PreparedLaunch) names the inputs of its launch
//! as a [`LaunchSpec`] does, whatever its devices: [`VmmLaunch`] renders them for both, and
//! [`PreparedLaunch::adapter_arguments`](super::sandbox::PreparedLaunch::adapter_arguments) holds
//! an adapter's command line to the limit too.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::artifacts::Artifacts;
pub use super::config::COMMAND_LINE_BUDGET;
use super::filesystem::ExportPlan;
use super::network::{self, NetworkPolicy};
use crate::error::{Error, Result};
use crate::hypervisor::Hypervisor;
use crate::net::{is_valid_guest_network, is_valid_hostname};
use crate::protocol::WORKLOAD_ACCOUNT_TOKEN;
use crate::protocol::cmdline::{self, Parameter};

pub use super::config::DEFAULT_GUEST_NETWORK;

/// Longest Windows process command line, in UTF-16 code units, including the terminating NUL.
///
/// OpenVMM's command line is held to it on every host, so a policy behaves alike on all of them.
pub const PROCESS_COMMAND_LINE_LIMIT: usize = 32_767;
/// Part of that limit kept for what may change between provision and start, such as the
/// configured OpenVMM and guest paths.
const PROCESS_COMMAND_LINE_RESERVE: usize = 1024;
/// Longest accepted start, control, or stop timeout, or extra time to wait for a workload's
/// outcome: 30 days.
///
/// The bound keeps every deadline derived from these timeouts, including a workload's own
/// timeout plus the extra time, representable as an [`Instant`](std::time::Instant) and within
/// the roughly 49-day range of Windows' millisecond waits.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Default guest memory in MiB.
pub const DEFAULT_MEMORY_MIB: u32 = 256;
/// Default workload hostname.
pub const DEFAULT_HOSTNAME: &str = "nvx-sandbox";
/// The name of the kernel parameter that sets the workload hostname, which follows the extra
/// kernel parameters on every command line.
const HOSTNAME_PARAMETER: &str = "hostname";

pub use crate::protocol::workload::WorkloadIdentity;

impl WorkloadIdentity {
    /// Alpine's `nobody`, which every guest image has.
    pub const DEFAULT: Self = Self {
        uid: 65534,
        gid: 65534,
        create_account: false,
    };

    /// Returns the identity of the workloads of a VM.
    ///
    /// On Linux hosts, virtio-fs shows host owners and modes unchanged, so a fixed identity could
    /// not read the calling user's private files or write to their directories. With
    /// `map_host_identity`, the workloads of a VM that maps host paths therefore run under the
    /// calling user's own IDs, and the guest creates an account for them, so host file
    /// permissions apply to them as they do to that user. Root callers, VMs that map nothing,
    /// and Windows hosts, which present mapped files as accessible to every guest user, keep
    /// `uid` and `gid`.
    pub fn select(uid: u32, gid: u32, map_host_identity: bool, maps_host_paths: bool) -> Self {
        match host_identity(map_host_identity && maps_host_paths) {
            Some((uid, gid)) => Self {
                uid,
                gid,
                create_account: true,
            },
            None => Self {
                uid,
                gid,
                create_account: false,
            },
        }
    }
}

/// Returns the calling user's IDs when `wanted`, unless the caller is root or the host is not a
/// Unix host.
fn host_identity(wanted: bool) -> Option<(u32, u32)> {
    if !wanted {
        return None;
    }
    #[cfg(unix)]
    {
        let (uid, gid) = super::sys::effective_ids();
        (uid != 0 && gid != 0).then_some((uid, gid))
    }
    #[cfg(not(unix))]
    None
}

/// Everything that OpenVMM's command line for one VM depends on.
///
/// Paths are used as given, and OpenVMM runs in the instance's directory, so make them absolute,
/// for example with [`absolute`](super::artifacts::absolute). The other settings must pass
/// [`check`](Self::check), which every method that builds a command line applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// The `openvmm` executable.
    pub openvmm: PathBuf,
    /// The NVX guest kernel (`vmlinux`).
    pub kernel: PathBuf,
    /// The NVX Alpine initramfs (`initramfs.cpio.gz`): the guest userland and its control agent.
    pub initrd: PathBuf,
    /// Hypervisor used by OpenVMM.
    pub hypervisor: Hypervisor,
    /// Guest memory in MiB; positive.
    pub memory_mib: u32,
    /// Identity of the workloads; neither ID may be root's.
    pub identity: WorkloadIdentity,
    /// UTS hostname of the workloads, set through the kernel's `hostname=` parameter: a lowercase
    /// RFC 1123 label of up to 63 characters.
    pub hostname: String,
    /// Extra kernel parameters; see [`kernel_parameters_problem`] for the ones that are refused.
    pub kernel_parameters: String,
    /// Static guest IPv4 address and prefix used when a network device is attached, in the form
    /// that [`is_valid_guest_network`] accepts.
    ///
    /// OpenVMM derives the guest's IPv6 address by embedding this one in `fd00::/96`: the default
    /// `10.0.0.2/24` yields `fd00::a00:2/120`, whose gateway is `fd00::a00:1`.
    pub guest_network: String,
    /// Network posture; `None` attaches no network device.
    pub network: Option<NetworkPolicy>,
    /// Host paths to export; `None` maps nothing.
    pub filesystem: Option<ExportPlan>,
    /// Windows only: start OpenVMM outside the caller's job object so the VM survives the
    /// caller. The job must permit breakaway.
    pub breakaway_from_job: bool,
}

impl LaunchSpec {
    /// Creates a specification that runs `artifacts` under `hypervisor` with the defaults:
    /// [`DEFAULT_MEMORY_MIB`], [`WorkloadIdentity::DEFAULT`], [`DEFAULT_HOSTNAME`], no extra
    /// kernel parameters, [`DEFAULT_GUEST_NETWORK`], and neither a network device nor host paths.
    pub fn new(artifacts: Artifacts, hypervisor: Hypervisor) -> Self {
        Self {
            openvmm: artifacts.openvmm,
            kernel: artifacts.kernel,
            initrd: artifacts.initrd,
            hypervisor,
            memory_mib: DEFAULT_MEMORY_MIB,
            identity: WorkloadIdentity::DEFAULT,
            hostname: DEFAULT_HOSTNAME.to_owned(),
            kernel_parameters: String::new(),
            guest_network: DEFAULT_GUEST_NETWORK.to_owned(),
            network: None,
            filesystem: None,
            breakaway_from_job: false,
        }
    }

    /// Checks the settings that OpenVMM's command line carries: a positive memory size, a
    /// non-root workload identity, a hostname that [`is_valid_hostname`] accepts, a guest network
    /// that [`is_valid_guest_network`] accepts, and extra kernel parameters that
    /// [`kernel_parameters_problem`] finds no problem with.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) naming the first
    /// setting that does not pass.
    pub fn check(&self) -> Result<()> {
        let unsupported = |message: String| Err(Error::unsupported(message));
        if self.memory_mib == 0 {
            return unsupported("memory_mib must be positive".to_owned());
        }
        if self.identity.uid == 0 || self.identity.gid == 0 {
            return unsupported("workloads must run as a non-root UID and GID".to_owned());
        }
        if !is_valid_hostname(&self.hostname) {
            return unsupported(format!(
                "hostname {:?} must be a lowercase RFC 1123 label of up to 63 characters",
                self.hostname
            ));
        }
        if !is_valid_guest_network(&self.guest_network) {
            return unsupported(format!(
                "guest_network {:?} must be an IPv4 address with a /1 to /30 prefix that is not \
                 its network's network, broadcast, or gateway (first) address",
                self.guest_network
            ));
        }
        if let Some(problem) = kernel_parameters_problem(&self.kernel_parameters, &self.hostname) {
            return unsupported(format!("kernel_parameters {problem}"));
        }
        Ok(())
    }

    /// Builds the kernel command line: the extra parameters, the workload hostname, and the
    /// tokens that configure the guest agent, such as the number of host mappings that it
    /// receives over the control channel.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when the
    /// specification fails [`check`](Self::check), or when the command line exceeds
    /// [`COMMAND_LINE_BUDGET`].
    pub fn kernel_command_line(&self) -> Result<String> {
        self.check()?;
        self.checked_kernel_command_line()
    }

    /// Builds the kernel command line of a specification that passed [`check`](Self::check).
    fn checked_kernel_command_line(&self) -> Result<String> {
        let mut tokens = vec![base_command_line(&self.kernel_parameters, &self.hostname)];
        if self.identity.create_account {
            tokens.push(WORKLOAD_ACCOUNT_TOKEN.to_owned());
        }
        if let Some(plan) = &self.filesystem {
            tokens.push(plan.kernel_token());
        }
        let command_line = tokens.join(" ");
        if command_line.len() + 1 > COMMAND_LINE_BUDGET {
            return Err(Error::unsupported(format!(
                "the kernel command line exceeds its {COMMAND_LINE_BUDGET}-byte budget; shorten \
                 the extra kernel parameters or the hostname"
            )));
        }
        Ok(command_line)
    }

    /// Builds the complete OpenVMM argument list for a VM whose control console listens on
    /// `endpoint` and whose outcome report goes to `report`.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when the
    /// specification fails [`check`](Self::check), or when the kernel command line or the network
    /// posture cannot be expressed.
    pub fn openvmm_arguments(&self, endpoint: &str, report: &Path) -> Result<Vec<OsString>> {
        self.check()?;
        let mut arguments: Vec<OsString> = vec!["--machine".into(), "microvm".into()];
        arguments.push("--microvm-workload-identity".into());
        arguments.push(format!("{}:{}", self.identity.uid, self.identity.gid).into());
        arguments.push("--microvm-lifecycle".into());
        arguments.push("managed".into());
        arguments.extend(
            self.vmm_launch(endpoint, report)
                .arguments(&self.checked_kernel_command_line()?),
        );
        arguments.extend(
            network::openvmm_arguments(self.network.as_ref(), &self.guest_network)?
                .into_iter()
                .map(OsString::from),
        );
        if let Some(plan) = &self.filesystem {
            arguments.extend(plan.openvmm_arguments().into_iter().map(OsString::from));
        }
        Ok(arguments)
    }

    /// Returns the inputs of the launch of a VM of the specification whose control console listens
    /// on `endpoint` and whose outcome report goes to `report`: its OpenVMM executable,
    /// hypervisor, memory, kernel, and initramfs, the endpoint, and the report.
    ///
    /// An adapter that launches its own guest with the specification's artifacts gives them to
    /// [`PreparedLaunch::adapter_arguments`](super::sandbox::PreparedLaunch::adapter_arguments).
    pub fn vmm_launch<'a>(&'a self, endpoint: &'a str, report: &'a Path) -> VmmLaunch<'a> {
        VmmLaunch {
            openvmm: &self.openvmm,
            hypervisor: self.hypervisor,
            memory_mib: self.memory_mib,
            kernel: &self.kernel,
            initrd: &self.initrd,
            endpoint,
            report,
        }
    }

    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) if OpenVMM's command
    /// line would not fit [`PROCESS_COMMAND_LINE_LIMIT`] with room to spare, as can happen with
    /// many filesystem paths.
    pub fn check_process_command_line(&self, endpoint: &str, report: &Path) -> Result<()> {
        let arguments = self.openvmm_arguments(endpoint, report)?;
        self.fit_process_command_line(&arguments, PROCESS_COMMAND_LINE_RESERVE)
    }

    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) unless OpenVMM's
    /// command line with `arguments` leaves `reserve` of [`PROCESS_COMMAND_LINE_LIMIT`] unused, as
    /// [`fit_process_command_line`] checks it.
    pub(crate) fn fit_process_command_line(
        &self,
        arguments: &[OsString],
        reserve: usize,
    ) -> Result<()> {
        fit_process_command_line(self.openvmm.as_os_str(), arguments, reserve)
    }
}

/// The inputs of a cold launch that OpenVMM's command line names besides the VM's devices and
/// its kernel command line: the OpenVMM executable that runs the VM, the hypervisor, the guest's
/// memory, kernel, and initramfs, the endpoint on which the control console listens, and the
/// path of OpenVMM's outcome report.
///
/// [`arguments`](Self::arguments) renders them for a [`LaunchSpec`]'s launch, after the managed
/// lifecycle, and for an adapter's, after the devices of its plan, with
/// [`PreparedLaunch::adapter_arguments`](super::sandbox::PreparedLaunch::adapter_arguments).
/// Paths are used as given, and OpenVMM runs in the directory that its launch chooses, so make
/// them absolute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmmLaunch<'a> {
    /// The `openvmm` executable, which the command line runs.
    pub openvmm: &'a Path,
    /// Hypervisor used by OpenVMM.
    pub hypervisor: Hypervisor,
    /// Guest memory in MiB; positive.
    pub memory_mib: u32,
    /// The guest kernel (`vmlinux`).
    pub kernel: &'a Path,
    /// The initramfs.
    pub initrd: &'a Path,
    /// The endpoint on which the control console listens, such as the path of a socket.
    pub endpoint: &'a str,
    /// The path of OpenVMM's outcome report.
    pub report: &'a Path,
}

impl VmmLaunch<'_> {
    /// The part of [`PROCESS_COMMAND_LINE_LIMIT`] that the planning of an adapter's launch keeps
    /// for the rest of the launch, in UTF-16 code units, as
    /// [`SandboxPlan::check_adapter_launch`](super::sandbox::SandboxPlan::check_adapter_launch)
    /// leaves it: what the plan does not render.
    ///
    /// It holds the rest of a launch whose OpenVMM executable, kernel, initramfs, endpoint, and
    /// outcome report take up to 260 code units each, Windows' `MAX_PATH`: the executable and the
    /// arguments that [`arguments`](Self::arguments) renders besides the kernel command line, the
    /// longest memory and hypervisor included, and the names by which a
    /// [`PreparedLaunch`](super::sandbox::PreparedLaunch) gives OpenVMM the checked files of the
    /// plan's two block devices, which take up to 262 code units more each than the paths of the
    /// plan: a quoted final path of up to `MAX_PATH` behind `\\?\` in place of a path of at least
    /// 4. A launch whose inputs are longer may still not fit, which its complete check refuses
    /// before OpenVMM starts.
    pub const RESERVE: usize = 2048;

    /// Returns the arguments that name the launch's inputs, with the kernel command line
    /// `kernel_command_line`: the VM in one process, `--single-process`, on the hypervisor,
    /// `--hypervisor`, with the memory, `--memory {memory_mib}M`, the kernel, `--kernel`, the
    /// initramfs, `--initrd`, and the kernel command line, `--cmdline`, then no virtio console,
    /// `--virtio-console none`, the control console listening on the endpoint,
    /// `--microvm-control-console listen={endpoint}`, authenticated by the capability that
    /// OpenVMM reads from its standard input, `--microvm-control-auth-stdin`, and the outcome
    /// report, `--microvm-report`.
    pub fn arguments(&self, kernel_command_line: &str) -> Vec<OsString> {
        let memory = format!("{}M", self.memory_mib);
        let control = format!("listen={}", self.endpoint);
        [
            OsStr::new("--single-process"),
            OsStr::new("--hypervisor"),
            OsStr::new(self.hypervisor.as_str()),
            OsStr::new("--memory"),
            OsStr::new(&memory),
            OsStr::new("--kernel"),
            self.kernel.as_os_str(),
            OsStr::new("--initrd"),
            self.initrd.as_os_str(),
            OsStr::new("--cmdline"),
            OsStr::new(kernel_command_line),
            OsStr::new("--virtio-console"),
            OsStr::new("none"),
            OsStr::new("--microvm-control-console"),
            OsStr::new(&control),
            OsStr::new("--microvm-control-auth-stdin"),
            OsStr::new("--microvm-report"),
            self.report.as_os_str(),
        ]
        .map(OsStr::to_os_string)
        .into()
    }
}

/// Returns the start of every kernel command line that `extra` parameters and `hostname`
/// determine alone: the parameters and the workload hostname.
pub fn base_command_line(extra: &str, hostname: &str) -> String {
    let hostname = format!("{HOSTNAME_PARAMETER}={hostname}");
    match extra.trim() {
        "" => hostname,
        extra => format!("{extra} {hostname}"),
    }
}

/// Describes why `extra` kernel parameters are unacceptable for VMs named `hostname`, if they
/// are.
///
/// The parameters are checked as the guest's kernel reads them in the command line that
/// [`base_command_line`] renders, as [`cmdline::parameters`] reads it: trimmed, split at every
/// byte that the kernel takes for whitespace, including the 0xa0 that ends the UTF-8 encoding of
/// characters such as `à`, with names that match whether they spell a dash or an underscore. The
/// parameters must not contain NUL characters, quotes, or the `--` delimiter, after which the
/// kernel would leave the `hostname=` parameter that follows them to init. The parameters named
/// `tsc` or `hostname`, with or without a value, and those whose names start with `nvx_` or
/// `nvx-` are reserved. They share the guest's 1024-byte kernel command line with the `hostname=`
/// parameter and the tokens that configure the guest agent, so parameters and a hostname that
/// alone leave no room in it are refused too. The description continues a sentence whose subject
/// names the parameters.
pub fn kernel_parameters_problem(extra: &str, hostname: &str) -> Option<String> {
    if extra.contains('\0') {
        return Some("contains a NUL character".to_owned());
    }
    if extra.contains(['"', '\'']) {
        return Some("must not contain quotes".to_owned());
    }
    let extra = extra.trim();
    if let Some(reserved) =
        cmdline::parameters(extra.as_bytes()).find(|&parameter| is_reserved(parameter))
    {
        return Some(format!("uses the reserved token {:?}", text(reserved)));
    }
    // Every VM's command line starts with these parameters and the hostname, and ends with a
    // NUL; launching then checks the tokens that each VM adds.
    let command_line = base_command_line(extra, hostname);
    // None of the parameters is named `hostname`, so the kernel reads none only when a `--` ends
    // its parameters before the hostname's.
    if !cmdline::parameters(command_line.as_bytes())
        .any(|parameter| parameter.is_named(HOSTNAME_PARAMETER))
    {
        return Some("must not contain the -- delimiter".to_owned());
    }
    (command_line.len() + 1 > COMMAND_LINE_BUDGET).then(|| {
        format!(
            "and the hostname leave no room in the {COMMAND_LINE_BUDGET}-byte kernel command line"
        )
    })
}

/// Returns whether extra kernel parameters may not hold `parameter`: the workload hostname's,
/// which the launch sets; `tsc`, which the time ABI governs; and those that configure the guest,
/// whose names start with `nvx_` or, since the kernel matches them alike, `nvx-`.
fn is_reserved(parameter: Parameter<'_>) -> bool {
    parameter.name.starts_with(b"nvx_")
        || parameter.name.starts_with(b"nvx-")
        || parameter.is_named("tsc")
        || parameter.is_named(HOSTNAME_PARAMETER)
}

/// Returns the text of `parameter` as the kernel reads it: `NAME=VALUE`, or `NAME` alone.
fn text(parameter: Parameter<'_>) -> String {
    let name = String::from_utf8_lossy(parameter.name);
    match parameter.value {
        Some(value) => format!("{name}={}", String::from_utf8_lossy(value)),
        None => name.into_owned(),
    }
}

/// Describes why a start, control, or stop timeout, or the extra time to wait for a workload's
/// outcome, is unacceptable, if it is: it must be at most [`MAX_TIMEOUT`], and positive when
/// `positive` is set. The description continues a sentence whose subject names the timeout.
pub fn timeout_problem(value: Duration, positive: bool) -> Option<String> {
    if positive && value.is_zero() {
        return Some("must be positive".to_owned());
    }
    (value > MAX_TIMEOUT).then(|| {
        format!(
            "must be at most {} days",
            MAX_TIMEOUT.as_secs() / (24 * 60 * 60)
        )
    })
}

/// Returns the length in UTF-16 code units, including the terminating NUL, of the Windows command
/// line that runs `program` with `arguments`, quoted the way the Windows platform quotes it: the
/// program always in quotes, and each argument in quotes if it is empty or holds a space or a
/// tab, with a backslash before each quote that it holds and a backslash before each backslash
/// that precedes a quote, its closing quote included, and one space between them.
///
/// It is the length that [`PROCESS_COMMAND_LINE_LIMIT`] bounds, whatever the host:
/// [`fit_process_command_line`] checks it.
pub fn process_command_line_units(program: &OsStr, arguments: &[OsString]) -> usize {
    argument_units(program, true) + arguments_units(arguments)
}

/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) unless the Windows command
/// line that runs `program`, such as OpenVMM, with `arguments`, as [`process_command_line_units`]
/// counts it, leaves `reserve` code units of [`PROCESS_COMMAND_LINE_LIMIT`] unused: none for the
/// complete command line of a launch about to spawn OpenVMM, and some for what may change before
/// then, such as the paths of the artifacts.
///
/// The error says how long the command line is, and how to shorten it.
pub fn fit_process_command_line(
    program: &OsStr,
    arguments: &[OsString],
    reserve: usize,
) -> Result<()> {
    let length = process_command_line_units(program, arguments);
    let held = if reserve == 0 {
        String::new()
    } else {
        format!(", with {reserve} held in reserve")
    };
    fit("OpenVMM's command line", length, reserve, &held)
}

/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) unless OpenVMM's
/// `arguments`, which a plan renders, without the program and the rest of the launch, leave
/// `reserve` code units of [`PROCESS_COMMAND_LINE_LIMIT`] to that rest.
pub(crate) fn fit_arguments(arguments: &[OsString], reserve: usize) -> Result<()> {
    let held = format!(", with {reserve} held in reserve for the rest of the launch");
    fit(
        "OpenVMM's arguments",
        arguments_units(arguments),
        reserve,
        &held,
    )
}

/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), saying that `subject`
/// would take `length` code units, `held` describing `reserve`, unless they leave `reserve` of
/// [`PROCESS_COMMAND_LINE_LIMIT`] unused.
fn fit(subject: &str, length: usize, reserve: usize, held: &str) -> Result<()> {
    if length.saturating_add(reserve) <= PROCESS_COMMAND_LINE_LIMIT {
        return Ok(());
    }
    Err(Error::unsupported(format!(
        "{subject} would take {length} of the {PROCESS_COMMAND_LINE_LIMIT} characters that a \
         Windows process command line allows{held}; map, hide, or allow fewer or shorter \
         filesystem paths, or use fewer network rules"
    )))
}

/// Returns the length in UTF-16 code units of `arguments` on a Windows command line, quoted as
/// [`process_command_line_units`] quotes them, each with the space before it.
fn arguments_units(arguments: &[OsString]) -> usize {
    arguments
        .iter()
        .map(|argument| argument_units(argument, false))
        .sum()
}

/// Returns the length in UTF-16 code units of `argument` on a Windows command line, quoted as
/// [`process_command_line_units`] quotes it, always if it is the `program`, with one separating
/// space, or the terminating NUL after the last argument.
fn argument_units(argument: &OsStr, program: bool) -> usize {
    let text = argument.to_string_lossy();
    let quoted = program || text.is_empty() || text.contains([' ', '\t']);
    let mut length = 1 + text.encode_utf16().count() + if quoted { 2 } else { 0 };
    let mut backslashes = 0;
    for character in text.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                length += backslashes + 1;
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
    }
    if quoted {
        length += backslashes;
    }
    length
}

#[cfg(test)]
mod tests;

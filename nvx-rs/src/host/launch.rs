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
use super::filesystem::ExportPlan;
use super::network::{self, NetworkPolicy};
use crate::error::{Error, Result};
use crate::hypervisor::Hypervisor;
use crate::net::{is_valid_guest_network, is_valid_hostname};
use crate::protocol::WORKLOAD_ACCOUNT_TOKEN;
use crate::protocol::cmdline::{self, Parameter};

/// Bytes of the x86 kernel command line available to caller tokens, including the NUL.
pub const COMMAND_LINE_BUDGET: usize = 1024;
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
/// Default guest network address.
pub const DEFAULT_GUEST_NETWORK: &str = "10.0.0.2/24";
/// The name of the kernel parameter that sets the workload hostname, which follows the extra
/// kernel parameters on every command line.
const HOSTNAME_PARAMETER: &str = "hostname";

/// The identity that workloads run as: a non-root user and group ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkloadIdentity {
    /// User ID.
    pub uid: u32,
    /// Primary group ID.
    pub gid: u32,
    /// Whether the guest creates an account for the IDs if its image lacks a usable one,
    /// replacing an image account for the UID whose primary group or home does not fit.
    pub create_account: bool,
}

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
/// which the launch sets; `tsc`, which time ABI v1 governs; and those that configure the guest,
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
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::ErrorKind;
    use crate::host::filesystem::{Bind, ExportChild, Owner};
    use crate::protocol::cmdline::model;

    fn spec() -> LaunchSpec {
        LaunchSpec::new(
            Artifacts::new("/o", "/k/vmlinux", "/k/initrd"),
            Hypervisor::Kvm,
        )
    }

    #[test]
    fn kernel_command_line_sets_the_hostname() {
        let mut spec = spec();
        spec.kernel_parameters = "quiet loglevel=0".to_owned();
        assert_eq!(
            spec.kernel_command_line().unwrap(),
            "quiet loglevel=0 hostname=nvx-sandbox"
        );
        spec.kernel_parameters = String::new();
        assert_eq!(spec.kernel_command_line().unwrap(), "hostname=nvx-sandbox");
        spec.kernel_parameters = "x".repeat(COMMAND_LINE_BUDGET);
        assert_eq!(
            spec.kernel_command_line().unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn arguments_select_the_managed_direct_guest() {
        let arguments = spec()
            .openvmm_arguments("/state/t/control.sock", Path::new("/state/t/outcome.json"))
            .unwrap();
        let arguments: Vec<&str> = arguments.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            arguments,
            [
                "--machine",
                "microvm",
                "--microvm-workload-identity",
                "65534:65534",
                "--microvm-lifecycle",
                "managed",
                "--single-process",
                "--hypervisor",
                "kvm",
                "--memory",
                "256M",
                "--kernel",
                "/k/vmlinux",
                "--initrd",
                "/k/initrd",
                "--cmdline",
                "hostname=nvx-sandbox",
                "--virtio-console",
                "none",
                "--microvm-control-console",
                "listen=/state/t/control.sock",
                "--microvm-control-auth-stdin",
                "--microvm-report",
                "/state/t/outcome.json",
            ]
        );
        assert!(!arguments.contains(&"--microvm-sandbox-block"));
    }

    #[test]
    fn mappings_add_the_export_and_the_mapping_count() {
        let mut spec = spec();
        spec.filesystem = Some(ExportPlan {
            children: vec![ExportChild {
                root: "/host/work".into(),
                file: false,
                writable: false,
                denied: vec!["/host/work/secret".into()],
                denied_identities: vec![None],
                allowed: Vec::new(),
                write: Vec::new(),
            }],
            binds: vec![Bind {
                child: 0,
                source: String::new(),
                target: "/host/work".to_owned(),
                read_only: true,
                identity: None,
            }],
            owner: Owner::Vmm,
        });
        assert_eq!(
            spec.kernel_command_line().unwrap(),
            "hostname=nvx-sandbox nvx_maps=1"
        );
        spec.identity.create_account = true;
        assert_eq!(
            spec.kernel_command_line().unwrap(),
            "hostname=nvx-sandbox nvx_workload_account=create nvx_maps=1"
        );
        spec.identity.create_account = false;
        let arguments = spec.openvmm_arguments("/e", Path::new("/r")).unwrap();
        let arguments: Vec<&str> = arguments.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            arguments[arguments.len() - 6..],
            [
                "--mount-aggregate",
                "/run/nvx/hostfs/root",
                "--mount-child",
                "0,/host/work,ro",
                "--mount-deny",
                "/host/work/secret",
            ]
        );
    }

    #[test]
    fn process_command_lines_are_measured_as_windows_quotes_them() {
        let cases: [(&str, &[&str], &str); 5] = [
            (
                r"C:\Program Files\openvmm.exe",
                &["--a", "b c", ""],
                r#""C:\Program Files\openvmm.exe" --a "b c" """#,
            ),
            ("x", &[r#"say "hi""#], r#""x" "say \"hi\"""#),
            ("x", &[r#"a"b"#], r#""x" a\"b"#),
            ("x", &[r"dir\ with\"], r#""x" "dir\ with\\""#),
            ("x", &[r"a\\b", "é"], r#""x" a\\b é"#),
        ];
        for (program, arguments, line) in cases {
            let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
            assert_eq!(
                process_command_line_units(OsStr::new(program), &arguments),
                line.encode_utf16().count() + 1,
                "{line}"
            );
        }
    }

    #[test]
    fn oversized_process_command_lines_are_rejected() {
        let mut spec = spec();
        let report = Path::new("/state/t/outcome.json");
        spec.check_process_command_line("/state/t/control.sock", report)
            .unwrap();
        let denied: Vec<PathBuf> = (0..128)
            .map(|index| format!("/host/work/{index:03}-{}", "d".repeat(240)).into())
            .collect();
        spec.filesystem = Some(ExportPlan {
            children: vec![ExportChild {
                root: "/host/work".into(),
                file: false,
                writable: false,
                denied_identities: vec![None; denied.len()],
                denied,
                allowed: Vec::new(),
                write: Vec::new(),
            }],
            binds: Vec::new(),
            owner: Owner::Vmm,
        });
        let error = spec
            .check_process_command_line("/state/t/control.sock", report)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn launches_may_use_the_reserve_that_provisioning_keeps() {
        let endpoint = "/state/t/control.sock";
        let report = Path::new("/state/t/outcome.json");
        let mut spec = spec();
        let arguments = spec.openvmm_arguments(endpoint, report).unwrap();
        let base = process_command_line_units(spec.openvmm.as_os_str(), &arguments);
        // Lengthening the program's path lengthens the command line alike.
        let program = |extra: usize| PathBuf::from(format!("/o{}", "o".repeat(extra)));
        spec.openvmm = program(PROCESS_COMMAND_LINE_LIMIT - base);
        assert_eq!(
            process_command_line_units(spec.openvmm.as_os_str(), &arguments),
            PROCESS_COMMAND_LINE_LIMIT
        );
        spec.fit_process_command_line(&arguments, 0).unwrap();
        let reserved = spec
            .check_process_command_line(endpoint, report)
            .unwrap_err();
        assert!(
            reserved
                .to_string()
                .contains("allows, with 1024 held in reserve; map"),
            "{reserved}"
        );
        spec.openvmm = program(PROCESS_COMMAND_LINE_LIMIT - base + 1);
        let error = spec.fit_process_command_line(&arguments, 0).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(
            error
                .to_string()
                .contains("a Windows process command line allows; map"),
            "{error}"
        );
    }

    #[test]
    fn a_process_command_line_fits_while_it_leaves_its_reserve() {
        // Any program and arguments, such as an adapter's: a command line of the limit fits,
        // one more code unit does not, and a reserve takes its part of the limit.
        let arguments: Vec<OsString> = ["--a", "b c"].map(OsString::from).into();
        let base = process_command_line_units(OsStr::new("/o"), &arguments);
        let program = |extra: usize| OsString::from(format!("/o{}", "o".repeat(extra)));
        let fitting = program(PROCESS_COMMAND_LINE_LIMIT - base);
        assert_eq!(
            process_command_line_units(&fitting, &arguments),
            PROCESS_COMMAND_LINE_LIMIT
        );
        fit_process_command_line(&fitting, &arguments, 0).unwrap();
        let held = fit_process_command_line(&fitting, &arguments, 1).unwrap_err();
        assert_eq!(held.kind(), ErrorKind::Unsupported);
        assert!(
            held.to_string()
                .contains("would take 32767 of the 32767 characters"),
            "{held}"
        );
        assert!(
            held.to_string().contains("with 1 held in reserve"),
            "{held}"
        );
        let longer = program(PROCESS_COMMAND_LINE_LIMIT - base + 1);
        let error = fit_process_command_line(&longer, &arguments, 0).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        // A reserve past the limit leaves no room for any command line.
        assert!(fit_process_command_line(OsStr::new("o"), &[], usize::MAX).is_err());
        // The arguments alone, which a plan renders, take what they take on the command line.
        let alone = process_command_line_units(OsStr::new(""), &arguments)
            - process_command_line_units(OsStr::new(""), &[]);
        let reserve = PROCESS_COMMAND_LINE_LIMIT - alone;
        fit_arguments(&arguments, reserve).unwrap();
        let held = fit_arguments(&arguments, reserve + 1).unwrap_err();
        assert!(
            held.to_string()
                .contains("held in reserve for the rest of the launch"),
            "{held}"
        );
    }

    #[test]
    fn every_launch_names_its_own_inputs_alike() {
        let launch = VmmLaunch {
            openvmm: Path::new("/o/openvmm"),
            hypervisor: Hypervisor::Mshv,
            memory_mib: 512,
            kernel: Path::new("/k/vmlinux"),
            initrd: Path::new("/k/initramfs.cpio.gz"),
            endpoint: "/state/t/control.sock",
            report: Path::new("/state/t/outcome.json"),
        };
        let arguments = launch.arguments("quiet nvx_sandbox=1");
        let arguments: Vec<&str> = arguments.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            arguments,
            [
                "--single-process",
                "--hypervisor",
                "mshv",
                "--memory",
                "512M",
                "--kernel",
                "/k/vmlinux",
                "--initrd",
                "/k/initramfs.cpio.gz",
                "--cmdline",
                "quiet nvx_sandbox=1",
                "--virtio-console",
                "none",
                "--microvm-control-console",
                "listen=/state/t/control.sock",
                "--microvm-control-auth-stdin",
                "--microvm-report",
                "/state/t/outcome.json",
            ]
        );
        // A managed launch names them after its lifecycle, with its own command line.
        let spec = spec();
        let endpoint = "/state/t/control.sock";
        let report = Path::new("/state/t/outcome.json");
        let launch = spec.vmm_launch(endpoint, report);
        assert_eq!(
            launch,
            VmmLaunch {
                openvmm: Path::new("/o"),
                hypervisor: Hypervisor::Kvm,
                memory_mib: DEFAULT_MEMORY_MIB,
                kernel: Path::new("/k/vmlinux"),
                initrd: Path::new("/k/initrd"),
                endpoint,
                report,
            }
        );
        let managed = spec.openvmm_arguments(endpoint, report).unwrap();
        let own = launch.arguments(&spec.kernel_command_line().unwrap());
        assert_eq!(managed[6..6 + own.len()], own);
        assert_eq!(managed[4..6], ["--microvm-lifecycle", "managed"]);
    }

    #[test]
    fn the_reserve_of_a_plan_holds_the_rest_of_a_launch_of_inputs_of_max_path() {
        // Each input of `MAX_PATH` code units, with spaces, which the command line quotes.
        let path = |name: &str| format!("C:\\{name} {}", "x".repeat(260 - 4 - name.len()));
        let [openvmm, kernel, initrd, endpoint, report] =
            ["openvmm", "vmlinux", "initrd", "control", "report"].map(path);
        for input in [&openvmm, &kernel, &initrd, &endpoint, &report] {
            assert_eq!(input.encode_utf16().count(), 260);
        }
        let launch = VmmLaunch {
            openvmm: Path::new(&openvmm),
            hypervisor: Hypervisor::Mshv,
            memory_mib: u32::MAX,
            kernel: Path::new(&kernel),
            initrd: Path::new(&initrd),
            endpoint: &endpoint,
            report: Path::new(&report),
        };
        // The plan's options count the kernel command line, whatever it is: the rest is the
        // program and the other arguments.
        let kernel_command_line = "nvx_sandbox=1";
        let own = launch.arguments(kernel_command_line);
        let cmdline: Vec<OsString> = ["--cmdline", kernel_command_line]
            .map(OsString::from)
            .into();
        let rest = process_command_line_units(launch.openvmm.as_os_str(), &own)
            - arguments_units(&cmdline);
        // A checked file's name that takes the most more than its path in the plan: a quoted
        // final path of `MAX_PATH` behind `\\?\`, for a path of 4.
        let named = |path: &str| arguments_units(&[OsString::from(format!("distro:file:{path}"))]);
        let final_path = format!(r"\\?\{}", path("distro"));
        let growth = named(&final_path) - named(r"C:\d");
        assert_eq!(growth, 262);
        assert!(
            rest + 2 * growth <= VmmLaunch::RESERVE,
            "{rest} + 2 * {growth}"
        );
    }

    #[test]
    fn kernel_parameters_are_checked() {
        let hostname = DEFAULT_HOSTNAME;
        assert!(kernel_parameters_problem("quiet loglevel=0", hostname).is_none());
        for reserved in [
            "tsc=reliable",
            "hostname=other",
            "quiet nvx_exec=/bin/sh",
            "a -- b",
            "say \"hi\"",
            "nul\0",
        ] {
            assert!(
                kernel_parameters_problem(reserved, hostname).is_some(),
                "{reserved:?}"
            );
        }
        // The parameters, a space, the hostname token, and a NUL fill the command line exactly.
        let room = COMMAND_LINE_BUDGET - base_command_line("", hostname).len() - 2;
        assert!(kernel_parameters_problem(&"x".repeat(room), hostname).is_none());
        assert_eq!(
            kernel_parameters_problem(&"x".repeat(room + 1), hostname).unwrap(),
            "and the hostname leave no room in the 1024-byte kernel command line"
        );
    }

    #[test]
    fn kernel_parameters_are_read_as_the_kernel_reads_them() {
        let hostname = DEFAULT_HOSTNAME;
        // `à`, U+00E0, is C3 A0 in UTF-8, and the kernel takes the byte 0xa0 for whitespace, so it
        // reads the text after it as another parameter (microsoft/nvx#723). So do U+00A0 and
        // U+0120, and the vertical tab, the form feed, and the carriage return.
        for (extra, problem) in [
            (
                "quiet\u{e0}tsc=unstable",
                "uses the reserved token \"tsc=unstable\"",
            ),
            ("quiet\u{e0}--", "must not contain the -- delimiter"),
            (
                "quiet\u{a0}-- loglevel=0",
                "must not contain the -- delimiter",
            ),
            (
                "quiet\u{a0}hostname=other",
                "uses the reserved token \"hostname=other\"",
            ),
            (
                "quiet\u{120}nvx_maps=5",
                "uses the reserved token \"nvx_maps=5\"",
            ),
            (
                "quiet\u{b}nvx_workload_account=create",
                "uses the reserved token \"nvx_workload_account=create\"",
            ),
            (
                "quiet\u{c}tsc=reliable",
                "uses the reserved token \"tsc=reliable\"",
            ),
            (
                "quiet\rhostname=other",
                "uses the reserved token \"hostname=other\"",
            ),
            // The kernel matches names whether they spell a dash or an underscore. A launch also
            // reserves `tsc` and `hostname` without a value: the early handler of `hostname`
            // copies the value that such a parameter lacks.
            ("nvx-maps=5", "uses the reserved token \"nvx-maps=5\""),
            ("tsc", "uses the reserved token \"tsc\""),
            ("quiet hostname", "uses the reserved token \"hostname\""),
            // The command line starts with the parameters trimmed of any whitespace.
            (
                "\u{2003}tsc=unstable",
                "uses the reserved token \"tsc=unstable\"",
            ),
        ] {
            assert_eq!(
                kernel_parameters_problem(extra, hostname).as_deref(),
                Some(problem),
                "{extra:?}"
            );
        }
        // The kernel reads one parameter where none of its whitespace separates two, and a `--`
        // with a value is no delimiter.
        for extra in [
            "quiet\u{2003}tsc=unstable",
            "quiet\u{3000}nvx_maps=5",
            "quiet\u{800}tsc=unstable",
            "x=tsc=unstable",
            "tscx=1",
            "hostnames=a",
            "xnvx_maps=5",
            "a--",
            "--=1",
        ] {
            assert_eq!(
                kernel_parameters_problem(extra, hostname),
                None,
                "{extra:?}"
            );
        }
    }

    /// Returns a strategy for what the kernel takes for whitespace between two parameters: an
    /// ASCII byte that `isspace` accepts, or a character above U+007F whose code point ends with
    /// the six bits 0x20, which UTF-8 encodes in a last byte of 0xa0.
    fn kernel_separator() -> impl Strategy<Value = String> {
        prop_oneof![
            prop::sample::select(vec![' ', '\t', '\n', '\u{b}', '\u{c}', '\r']),
            (0x80_u32..0x11_0000)
                .prop_filter_map("a surrogate", |code| char::from_u32((code & !0x3f) | 0x20)),
        ]
        .prop_map(String::from)
    }

    /// Returns a strategy for extra kernel parameters: mostly letters, equals signs, dashes,
    /// underscores, the names that a launch reserves, and the whitespace of the kernel and of
    /// Unicode, and now and then a quote or a NUL.
    fn extra_parameters() -> impl Strategy<Value = String> {
        /// The pieces of the parameters. U+0800 holds the byte 0xa0 before its last byte, and
        /// U+2003 and U+3000 hold none.
        const PIECES: [&str; 23] = [
            "a", "b", "=", "-", "--", "_", "nvx_", "nvx-", "tsc", "hostname", " ", "\t", "\n",
            "\u{b}", "\u{c}", "\r", "\u{a0}", "\u{e0}", "\u{120}", "\u{2020}", "\u{800}",
            "\u{2003}", "\u{3000}",
        ];
        prop::collection::vec(
            prop_oneof![
                24 => prop::sample::select(PIECES.to_vec()),
                1 => prop::sample::select(vec!["\"", "'", "\0"]),
            ],
            0..24,
        )
        .prop_map(|pieces| pieces.concat())
    }

    proptest! {
        /// No reserved parameter passes, whatever separates it from the parameter before it.
        #[test]
        fn reserved_parameters_are_refused_whatever_separates_them(
            before in "[a-z]{1,8}",
            separator in kernel_separator(),
            reserved in prop::sample::select(vec![
                "tsc=unstable",
                "tsc",
                "hostname=other",
                "hostname",
                "nvx_maps=5",
                "nvx-maps=5",
                "nvx_workload_account=create",
                "--",
            ]),
            after in "( [a-z]{1,8})?",
        ) {
            let extra = format!("{before}{separator}{reserved}{after}");
            prop_assert!(kernel_parameters_problem(&extra, DEFAULT_HOSTNAME).is_some(), "{:?}", extra);
        }

        /// The parameters that a launch accepts are those in which the kernel reads no reserved
        /// parameter, as it compares names, and after which it reads the `hostname=` parameter:
        /// the parameters hold no NUL and no quote, and no `--` ends them.
        #[test]
        fn the_kernel_reads_the_hostname_after_accepted_parameters(extra in extra_parameters()) {
            let hostname = DEFAULT_HOSTNAME;
            let reading = model::read(base_command_line(&extra, hostname).as_bytes());
            let reserved = |name: &[u8]| {
                model::parameqn(name, b"nvx_", 4)
                    || model::parameq(name, b"tsc")
                    || model::parameq(name, b"hostname")
            };
            let readable = !extra.contains(['\0', '"', '\''])
                && reading.rest.is_none()
                && reading.parameters.split_last().is_some_and(|((name, value), others)| {
                    name == b"hostname"
                        && value.as_deref() == Some(hostname.as_bytes())
                        && !others.iter().any(|(name, _)| reserved(name))
                });
            prop_assert_eq!(
                kernel_parameters_problem(&extra, hostname).is_none(),
                readable,
                "{:?}",
                extra
            );
        }
    }

    #[test]
    fn every_command_line_checks_the_specification() {
        let endpoint = "/state/t/control.sock";
        let report = Path::new("/state/t/outcome.json");
        spec().check().unwrap();
        let changes: [fn(&mut LaunchSpec); 11] = [
            |spec| spec.memory_mib = 0,
            |spec| spec.identity.uid = 0,
            |spec| spec.identity.gid = 0,
            // A hostname cannot smuggle further kernel parameters in.
            |spec| spec.hostname = "a nvx_maps=1".to_owned(),
            |spec| spec.hostname = "Sandbox".to_owned(),
            |spec| spec.guest_network = "10.0.0.1/24".to_owned(),
            |spec| spec.kernel_parameters = "nvx_maps=5".to_owned(),
            |spec| spec.kernel_parameters = "nvx_workload_account=create".to_owned(),
            |spec| spec.kernel_parameters = "quiet hostname=other".to_owned(),
            // The kernel reads the byte 0xa0 of `à` as whitespace (microsoft/nvx#723).
            |spec| spec.kernel_parameters = "quiet\u{e0}tsc=unstable".to_owned(),
            |spec| spec.kernel_parameters = "quiet\u{e0}--".to_owned(),
        ];
        for change in changes {
            let mut spec = spec();
            change(&mut spec);
            for error in [
                spec.check().unwrap_err(),
                spec.kernel_command_line().unwrap_err(),
                spec.openvmm_arguments(endpoint, report).unwrap_err(),
                spec.check_process_command_line(endpoint, report)
                    .unwrap_err(),
            ] {
                assert_eq!(error.kind(), ErrorKind::Unsupported, "{spec:?}");
            }
        }
        let mut root = spec();
        root.identity.gid = 0;
        assert_eq!(
            root.check().unwrap_err().to_string(),
            "workloads must run as a non-root UID and GID"
        );
        let mut reserved = spec();
        reserved.kernel_parameters = "nvx_maps=5".to_owned();
        assert_eq!(
            reserved.check().unwrap_err().to_string(),
            "kernel_parameters uses the reserved token \"nvx_maps=5\""
        );
    }

    #[test]
    fn timeouts_are_bounded() {
        assert_eq!(timeout_problem(Duration::ZERO, false), None);
        assert_eq!(
            timeout_problem(Duration::ZERO, true).unwrap(),
            "must be positive"
        );
        assert_eq!(timeout_problem(MAX_TIMEOUT, true), None);
        for beyond in [MAX_TIMEOUT + Duration::from_nanos(1), Duration::MAX] {
            assert_eq!(
                timeout_problem(beyond, false).unwrap(),
                "must be at most 30 days"
            );
        }
    }

    #[test]
    fn workloads_that_map_host_paths_take_the_host_identity() {
        let configured = WorkloadIdentity::select(1000, 1000, false, true);
        assert_eq!(
            configured,
            WorkloadIdentity {
                uid: 1000,
                gid: 1000,
                create_account: false,
            }
        );
        assert_eq!(WorkloadIdentity::select(7, 8, true, false).uid, 7);
        let mapped = WorkloadIdentity::select(7, 8, true, true);
        match host_identity(true) {
            Some((uid, gid)) => assert_eq!(
                mapped,
                WorkloadIdentity {
                    uid,
                    gid,
                    create_account: true,
                }
            ),
            None => assert_eq!(mapped, WorkloadIdentity::select(7, 8, false, false)),
        }
    }
}

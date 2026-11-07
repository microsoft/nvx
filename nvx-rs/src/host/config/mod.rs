//! The configuration of a VM, as one value that [`VmConfig::check`] checks without reading the
//! host: the VM's processors and memory, its [`RootConfig`], the [`View`] that its workload has
//! of it, its [`NetworkConfig`], the host paths that it exports, its [`ConsoleAttachment`], and
//! what only its cold boot takes, a [`Boot`]. An internal renderer builds a whole
//! cold launch for [`crate::host::vm::create`], from that configuration and a
//! [`Runtime`], without opening runtime paths or allocating endpoints.

use std::ffi::{OsStr, OsString};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::filesystem::{ExportPlan, ReservedTargets};
use super::network::{self, NetworkPolicy};
use super::runtime::{GuestPowerAction, Runtime};
use crate::activation::{self, Journal, MAX_HOSTNAME_BYTES, OverlayMode, View};
use crate::error::{Error, Result};
use crate::hypervisor::Hypervisor;
use crate::net::Ipv4Cidr;
use crate::protocol::cmdline::{self, Parameter};
use crate::protocol::maps::HostMap;
use crate::protocol::root::{ImageFormat, RootImage, RootLayout, RootUpper};
use crate::protocol::view;

/// Bytes of the x86 kernel command line available to caller tokens, including the NUL.
pub const COMMAND_LINE_BUDGET: usize = 1024;

/// Longest Windows process command line, in UTF-16 code units, including the terminating NUL.
///
/// OpenVMM's command line is held to it on every host, so a policy behaves alike on all of them.
pub const PROCESS_COMMAND_LINE_LIMIT: usize = 32_767;

/// Room for the runtime's files and the VM's output paths, as legacy adapter planning reserves.
pub(crate) const OPENVMM_COMMAND_LINE_RESERVE: usize = 2048;

/// The guest address of a VM's NIC at a cold boot whose configuration names none, in the form
/// that [`Ipv4Cidr::parse`] reads.
pub const DEFAULT_GUEST_NETWORK: &str = "10.0.0.2/24";

/// The caps of a RAM upper that [`VmConfig::check`] accepts, in percent of the VM's memory.
const RAM_UPPER_CAPS: RangeInclusive<u8> = 1..=90;

/// The configuration of a VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmConfig {
    /// The number of processors: at least 1.
    pub processors: u32,
    /// The memory, in MiB: at least 1.
    pub memory_mib: u64,
    /// The root that the guest runs.
    pub root: RootConfig,
    /// The view that the VM's workload has of it.
    pub view: View,
    /// The VM's network, if it has a NIC.
    pub network: Option<NetworkConfig>,
    /// The host paths that the VM exports to its guest, if any.
    pub filesystem: Option<ExportPlan>,
    /// Where the output of the VM's boot console goes.
    pub console: ConsoleAttachment,
    /// What only a cold boot takes: a VM that boots needs it, and one that a snapshot restores
    /// takes none.
    pub boot: Option<Boot>,
}

/// The root of a VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootConfig {
    /// The guest's own system, from its initramfs, without a block device.
    Blockless,
    /// A read-only image under an upper, which overlayfs combines.
    Layered(LayeredRoot),
}

/// A layered root: a read-only image under an upper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayeredRoot {
    /// The image, which OpenVMM opens by its path.
    pub image: PathBuf,
    /// The image's format, and the identity that the guest expects of the image, if any.
    pub format: RootImage,
    /// The most bytes that the first partition of a GPT disk may hold, if bounded: at least 1. An
    /// EROFS image has none.
    pub gpt_first_partition_bound: Option<u64>,
    /// Where the overlay keeps its upper.
    pub upper: UpperConfig,
    /// The overlay's mode.
    pub overlay: OverlayMode,
}

/// Where a layered root keeps the upper of its overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpperConfig {
    /// A whole-device ext4 filesystem on a scratch image.
    Scratch {
        /// The scratch image, which OpenVMM opens by its path.
        path: PathBuf,
        /// Whether the scratch's filesystem may have a journal.
        journal: Journal,
    },
    /// RAM, up to a cap.
    Ram {
        /// The most that the upper may hold, in percent of the VM's memory: 1 to 90.
        max_memory_percent: u8,
    },
}

impl UpperConfig {
    /// The cap of a RAM upper that a caller sets when it has no other: half of the VM's memory.
    pub const DEFAULT_MAX_MEMORY_PERCENT: u8 = 50;
}

/// Where the output of a VM's boot console goes. The console's device exists either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsoleAttachment {
    /// OpenVMM listens for a client of the console.
    Listener,
    /// OpenVMM discards the console's output.
    Disconnected,
}

/// The network of a VM: the address of its NIC, and the policy that OpenVMM enforces on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkConfig {
    /// The NIC's static address, if the caller names one. Without one, a cold boot takes
    /// [`DEFAULT_GUEST_NETWORK`], and a restore the snapshot's.
    pub guest_network: Option<Ipv4Cidr>,
    /// The policy that OpenVMM enforces on the VM's traffic.
    pub policy: NetworkPolicy,
}

/// The NIC that a VM has, with the static address that it settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NicConfig {
    /// The NIC's static address.
    pub guest_network: Ipv4Cidr,
}

/// What only a VM's cold boot takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    /// The caller's own kernel command-line tokens, to which NVX adds those of what it lowers.
    /// The root's tokens, `nvx_hostname`, `nvx_machine_id`, `nvx_maps` and `virtfs_*` are reserved;
    /// every other token, including `nvx_entrypoint`, `nvx_arg` and `nvx_share`, is caller-owned.
    pub command_line: String,
}

impl VmConfig {
    /// Returns the configured export's mapping table, in exactly the order and form of
    /// [`ExportPlan::guest_table`], or an empty table without an export.
    ///
    /// This is a pure projection: it reads no host path and does not recheck the plan. A cold
    /// boot's rendered `nvx_maps` token counts these entries.
    pub fn mapping_table(&self) -> Vec<HostMap> {
        self.filesystem
            .as_ref()
            .map_or_else(Vec::new, ExportPlan::guest_table)
    }

    /// Renders a cold boot's kernel command line without reading the host: the root's tokens,
    /// the view's tokens, the export's mapping count when present, then the caller's
    /// [`Boot::command_line`] without trimming or rewriting it.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when
    /// [`check`](Self::check) refuses the configuration or when it has no [`Boot`]. A restore
    /// receives its view through its activation record, not a kernel command line.
    pub fn kernel_command_line(&self) -> Result<String> {
        self.check()?;
        let boot = self
            .boot
            .as_ref()
            .ok_or_else(|| Error::unsupported("a kernel command line needs a cold boot's Boot"))?;
        let mut tokens = Vec::new();
        if let RootConfig::Layered(root) = &self.root {
            let (upper, journal, max_memory_percent) = match &root.upper {
                UpperConfig::Scratch { journal, .. } => (RootUpper::Scratch, Some(*journal), None),
                UpperConfig::Ram { max_memory_percent } => {
                    (RootUpper::Ram, None, Some(*max_memory_percent))
                }
            };
            tokens.extend(
                RootLayout {
                    image: root.format,
                    upper,
                    overlay: Some(root.overlay),
                    journal,
                    gpt_first_partition_bound: root.gpt_first_partition_bound,
                    max_memory_percent,
                }
                .tokens(),
            );
        }
        tokens.extend(
            view::tokens(&self.view)
                .map_err(|error| Error::unsupported(error.to_string()).with_source(error))?,
        );
        if let Some(export) = &self.filesystem {
            tokens.push(export.kernel_token());
        }
        if !boot.command_line.is_empty() {
            tokens.push(boot.command_line.clone());
        }
        Ok(tokens.join(" "))
    }

    /// Renders a cold boot's complete OpenVMM arguments from this configuration and `runtime`.
    ///
    /// `endpoint`, `report` and `console_endpoint` are already chosen output names, not launch
    /// settings: this method creates no directory, file, listener, or process, and uses paths as
    /// given. A [`ConsoleAttachment::Listener`] needs a nonempty `console_endpoint`; a
    /// [`ConsoleAttachment::Disconnected`] needs none and retains the console device with a
    /// discarded attachment. The control endpoint always listens and authenticates from stdin.
    /// Listener names must be distinct, with ASCII-case-insensitive comparison for WHP.
    /// The caller runs `runtime.openvmm` with these arguments.
    ///
    /// [`crate::host::vm::create`] calls this internal whole-config renderer. Neither managed
    /// lifecycle nor workload-identity flags are rendered; the caller owns its guest tokens.
    /// `runtime.windows_breakaway` is process-launch behavior, not an OpenVMM argument.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a refused
    /// configuration, no [`Boot`], empty or NUL-containing required names or runtime paths,
    /// duplicate listener names, block paths that are not Unicode or hold disk-option delimiters
    /// or unbalanced brackets, an invalid authentication timeout, kernel text with double quotes
    /// or the standalone `--` delimiter, or a complete command line beyond the Windows limit.
    /// The complete limit check runs on every host, without a reserve.
    pub(crate) fn openvmm_arguments(
        &self,
        runtime: &Runtime,
        endpoint: &str,
        report: &Path,
        console_endpoint: Option<&str>,
    ) -> Result<Vec<OsString>> {
        let command_line = self.control_kernel_command_line()?;
        check_endpoint(endpoint, "endpoint")?;
        for (name, path) in [
            ("Runtime.openvmm", runtime.openvmm.as_path()),
            ("Runtime.kernel", runtime.kernel.as_path()),
            ("Runtime.initramfs", runtime.initramfs.as_path()),
            ("report", report),
        ] {
            if path.as_os_str().is_empty() || path.as_os_str().to_string_lossy().contains('\0') {
                return Err(Error::unsupported(format!(
                    "{name} must name a path without a NUL"
                )));
            }
        }
        let console = match (self.console, console_endpoint) {
            (ConsoleAttachment::Listener, Some(console_endpoint)) => {
                check_endpoint(console_endpoint, "console_endpoint")?;
                if console_endpoint == endpoint
                    || (runtime.hypervisor == Hypervisor::Whp
                        && console_endpoint.eq_ignore_ascii_case(endpoint))
                {
                    return Err(Error::unsupported(
                        "boot and control console listeners require distinct endpoints",
                    ));
                }
                format!("listen={console_endpoint}")
            }
            (ConsoleAttachment::Disconnected, None) => "none".to_owned(),
            (ConsoleAttachment::Listener, None) => {
                return Err(Error::unsupported(
                    "a console listener needs console_endpoint",
                ));
            }
            (ConsoleAttachment::Disconnected, Some(_)) => {
                return Err(Error::unsupported(
                    "a disconnected console takes no console_endpoint",
                ));
            }
        };
        let mut arguments = self.device_arguments()?;
        arguments.extend(boot_arguments(
            runtime.hypervisor,
            self.memory_mib,
            &runtime.kernel,
            &runtime.initramfs,
            &command_line,
        ));
        arguments.extend(console_arguments(&console, endpoint, report));
        if let Some(profile) = &runtime.cpu_profile {
            if profile.is_empty() {
                return Err(Error::unsupported("Runtime.cpu_profile must not be empty"));
            }
            arguments.extend(["--cpu-profile".into(), profile.as_str().into()]);
        }
        let shutdown = match runtime.options.guest_shutdown {
            GuestPowerAction::Exit => "exit",
            GuestPowerAction::Halt => "halt",
            GuestPowerAction::Reset => "reset",
        };
        arguments.extend(["--guest-shutdown-action".into(), shutdown.into()]);
        if let Some(timeout) = runtime.options.control_auth_timeout {
            if timeout < Duration::from_millis(1)
                || timeout > Duration::from_secs(60)
                || timeout.subsec_nanos() % 1_000_000 != 0
            {
                return Err(Error::unsupported(
                    "LaunchOptions.control_auth_timeout must be whole milliseconds from 1 to 60000",
                ));
            }
            arguments.extend([
                "--microvm-control-auth-timeout-ms".into(),
                timeout.as_millis().to_string().into(),
            ]);
        }
        check_argument_nuls(&arguments)?;
        fit_process_command_line(runtime.openvmm.as_os_str(), &arguments, 0)?;
        Ok(arguments)
    }

    /// Checks the configuration's part of a cold boot's OpenVMM command line on every host,
    /// without reading or changing the host, with 2048 UTF-16 code units reserved for the
    /// runtime's files, launch options, and the VM's output paths.
    ///
    /// Like legacy adapter planning, the reserve foresees runtime-file and output names of up to
    /// 260 code units each, Windows' `MAX_PATH`. It does not certify arbitrary runtime paths or
    /// CPU-profile names: [`crate::host::vm::create`] checks the complete command line with the
    /// actual runtime and output names, without a reserve.
    /// A listener's backend prefix is counted here; its endpoint name stays in the reserve.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a refused
    /// configuration or no [`Boot`], an unlowerable network policy, a NUL-containing argument,
    /// a non-Unicode block path or one that OpenVMM's disk syntax cannot hold literally, kernel
    /// text with double quotes or the standalone `--` delimiter, or insufficient room for the
    /// reserve.
    pub fn check_command_line(&self) -> Result<()> {
        let command_line = self.control_kernel_command_line()?;
        let mut arguments = self.device_arguments()?;
        arguments.extend([
            "--memory".into(),
            format!("{}M", self.memory_mib).into(),
            "--cmdline".into(),
            command_line.into(),
            "--virtio-console".into(),
            match self.console {
                ConsoleAttachment::Listener => "listen=",
                ConsoleAttachment::Disconnected => "none",
            }
            .into(),
        ]);
        check_argument_nuls(&arguments)?;
        fit_arguments(&arguments, OPENVMM_COMMAND_LINE_RESERVE)
    }

    /// Renders kernel text without the double quotes or delimiter OpenVMM's control console refuses.
    fn control_kernel_command_line(&self) -> Result<String> {
        let command_line = self.kernel_command_line()?;
        if command_line.contains('"')
            || command_line
                .split_ascii_whitespace()
                .any(|token| token == "--")
        {
            return Err(Error::unsupported(
                "boot.command_line cannot contain double quotes or a standalone -- with the \
                 OpenVMM control console",
            ));
        }
        Ok(command_line)
    }

    /// Renders only the configuration's devices; no runtime or output name is needed.
    fn device_arguments(&self) -> Result<Vec<OsString>> {
        let mut arguments = vec![
            "--machine".into(),
            "microvm".into(),
            "--processors".into(),
            self.processors.to_string().into(),
        ];
        if let RootConfig::Layered(root) = &self.root {
            let scratch = match &root.upper {
                UpperConfig::Scratch { path, .. } => Some(path),
                UpperConfig::Ram { .. } => None,
            };
            for path in [Some(&root.image), scratch].into_iter().flatten() {
                if path.to_str().is_none() {
                    return Err(Error::unsupported(
                        "block paths must be Unicode for OpenVMM's string arguments",
                    ));
                }
                reject_disk_path(path, &path.display().to_string())?;
            }
            let mut image = OsString::from("distro:file:");
            image.push(&root.image);
            image.push(",ro");
            arguments.extend(["--microvm-sandbox-block".into(), image]);
            if let UpperConfig::Scratch { path, .. } = &root.upper {
                let mut scratch = OsString::from("scratch:file:");
                scratch.push(path);
                arguments.extend(["--microvm-sandbox-block".into(), scratch]);
            }
        }
        if let Some(network) = &self.network {
            let guest_network = network.guest_network.map_or_else(
                || DEFAULT_GUEST_NETWORK.to_owned(),
                |address| address.to_string(),
            );
            let mut nic = network::openvmm_arguments(Some(&network.policy), &guest_network)?;
            if nic.is_empty() {
                // A configured NIC remains present even under the deny-all posture.
                nic = network::template_arguments(&guest_network);
            }
            arguments.extend(nic.into_iter().map(OsString::from));
        }
        if let Some(export) = &self.filesystem {
            arguments.extend(export.openvmm_arguments().into_iter().map(OsString::from));
        }
        Ok(arguments)
    }

    /// Checks the whole configuration without reading the host: it opens no path, and checks no
    /// support of the host.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), naming the first
    /// refusal in the order of the fields: no processor; no memory; a layered root without an image
    /// path, with a bound of an EROFS image's first partition or a bound of 0, with a scratch upper
    /// without a path, or with a RAM upper whose cap is not 1 to 90 percent; a hostname that is
    /// empty, longer than [`MAX_HOSTNAME_BYTES`], or holds a NUL; an export that
    /// [`ExportPlan::check`] refuses for the root, whose error is the source; and caller boot
    /// tokens that collide with the root's, the view's, `nvx_maps`, or any `virtfs_*` name.
    ///
    /// The NIC's address is checked when it is built, and the network's policy where it is
    /// lowered, with the guest's address. Boot tokens are read as both Linux and the guest's init
    /// read them, with dashes and underscores matching as Linux matches parameter names.
    /// [`kernel_command_line`](Self::kernel_command_line) appends accepted caller text unchanged.
    pub fn check(&self) -> Result<()> {
        if self.processors == 0 {
            return Err(Error::unsupported("a VM needs at least one processor"));
        }
        if self.memory_mib == 0 {
            return Err(Error::unsupported("a VM needs memory"));
        }
        let reserved = match &self.root {
            RootConfig::Blockless => ReservedTargets::BLOCKLESS_ROOT,
            RootConfig::Layered(root) => {
                root.check()?;
                ReservedTargets::CONTAINER_ROOT
            }
        };
        if !activation::is_hostname(&self.view.hostname) {
            return Err(Error::unsupported(format!(
                "the hostname is not 1 to {MAX_HOSTNAME_BYTES} bytes without a NUL"
            )));
        }
        if let Some(plan) = &self.filesystem {
            plan.check(reserved).map_err(|error| {
                Error::unsupported("the export plan is invalid").with_source(error)
            })?;
        }
        if let Some(boot) = &self.boot {
            let guest = cmdline::init_tokens(boot.command_line.as_bytes()).map(|token| Parameter {
                name: token.name,
                value: token.value,
            });
            for parameter in cmdline::parameters(boot.command_line.as_bytes()).chain(guest) {
                if [
                    "nvx_layer",
                    "nvx_scratch",
                    "nvx_overlay_upper",
                    "nvx_overlay_mode",
                    "nvx_scratch_journal",
                    "nvx_first_partition_bound",
                    "nvx_ram_upper_percent",
                    "nvx_hostname",
                    "nvx_machine_id",
                    "nvx_maps",
                ]
                .iter()
                .any(|name| parameter.is_named(name))
                    || parameter.name.starts_with(b"virtfs_")
                    || parameter.name.starts_with(b"virtfs-")
                {
                    return Err(Error::unsupported(format!(
                        "Boot.command_line uses the reserved token {:?}",
                        String::from_utf8_lossy(parameter.name)
                    )));
                }
            }
        }
        Ok(())
    }
}

impl LayeredRoot {
    /// Checks the root's fields, as [`VmConfig::check`] describes.
    fn check(&self) -> Result<()> {
        if self.image.as_os_str().is_empty() {
            return Err(Error::unsupported("the layered root names no image"));
        }
        match (self.format.format, self.gpt_first_partition_bound) {
            (ImageFormat::Erofs, Some(_)) => {
                return Err(Error::unsupported(
                    "an EROFS image has no first partition to bound, as a GPT disk has",
                ));
            }
            (_, Some(0)) => {
                return Err(Error::unsupported(
                    "the bound of a GPT disk's first partition is 0, but a partition holds at \
                     least one block",
                ));
            }
            _ => {}
        }
        match &self.upper {
            UpperConfig::Scratch { path, .. } if path.as_os_str().is_empty() => Err(
                Error::unsupported("the scratch upper names no scratch image"),
            ),
            UpperConfig::Ram { max_memory_percent }
                if !RAM_UPPER_CAPS.contains(max_memory_percent) =>
            {
                Err(Error::unsupported(format!(
                    "the RAM upper's cap of {max_memory_percent}% of the VM's memory is not 1 to \
                     90%"
                )))
            }
            _ => Ok(()),
        }
    }
}

/// Refuses a disk path that OpenVMM's `--microvm-sandbox-block` value could not hold: one with
/// a comma, semicolon, or improperly nested brackets. The message shows `path` as `shown`.
pub(crate) fn reject_disk_path(path: &Path, shown: &str) -> Result<()> {
    let depth = path
        .to_string_lossy()
        .bytes()
        .try_fold(0_usize, |depth, byte| match byte {
            b'[' => Some(depth + 1),
            b']' => depth.checked_sub(1),
            b',' | b';' => None,
            _ => Some(depth),
        });
    if depth != Some(0) {
        return Err(Error::unsupported(format!(
            "disk paths containing commas, semicolons, or unbalanced brackets are unsupported: \
             {shown}"
        )));
    }
    Ok(())
}

/// Common cold-boot inputs, shared with the legacy VmmLaunch without narrowing its public types.
pub(crate) fn boot_arguments(
    hypervisor: Hypervisor,
    memory_mib: u64,
    kernel: &Path,
    initrd: &Path,
    kernel_command_line: &str,
) -> Vec<OsString> {
    let memory = format!("{memory_mib}M");
    [
        OsStr::new("--single-process"),
        OsStr::new("--hypervisor"),
        OsStr::new(hypervisor.as_str()),
        OsStr::new("--memory"),
        OsStr::new(&memory),
        OsStr::new("--kernel"),
        kernel.as_os_str(),
        OsStr::new("--initrd"),
        initrd.as_os_str(),
        OsStr::new("--cmdline"),
        OsStr::new(kernel_command_line),
    ]
    .map(OsStr::to_os_string)
    .into()
}

/// Common console, control authentication, and report arguments of a cold launch.
pub(crate) fn console_arguments(console: &str, endpoint: &str, report: &Path) -> Vec<OsString> {
    let control = format!("listen={endpoint}");
    [
        OsStr::new("--virtio-console"),
        OsStr::new(console),
        OsStr::new("--microvm-control-console"),
        OsStr::new(&control),
        OsStr::new("--microvm-control-auth-stdin"),
        OsStr::new("--microvm-report"),
        report.as_os_str(),
    ]
    .map(OsStr::to_os_string)
    .into()
}

/// Refuses empty or NUL-containing caller-selected endpoint names.
fn check_endpoint(endpoint: &str, name: &str) -> Result<()> {
    if endpoint.is_empty() || endpoint.contains('\0') {
        return Err(Error::unsupported(format!(
            "{name} must name a nonempty endpoint without a NUL",
        )));
    }
    Ok(())
}

/// Checks OpenVMM arguments for NULs that cannot be passed to a process.
fn check_argument_nuls(arguments: &[OsString]) -> Result<()> {
    if arguments
        .iter()
        .any(|argument| argument.to_string_lossy().contains('\0'))
    {
        return Err(Error::unsupported(
            "OpenVMM's arguments must not contain a NUL",
        ));
    }
    Ok(())
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
pub(crate) fn arguments_units(arguments: &[OsString]) -> usize {
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

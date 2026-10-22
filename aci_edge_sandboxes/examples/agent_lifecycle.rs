//! Runs one sandbox lifecycle through a separately supplied native agent library.
//!
//! Built with the `agent-bundled` feature, every option has a default: the bundle that the build
//! staged beside this executable, a state root beside this executable, a public image reference,
//! and a command that prints `READY` and the guest's OS release.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

#[cfg(feature = "agent-bundled")]
use aci_edge_sandboxes::agent::BundledAgent;
use aci_edge_sandboxes::agent::{
    AgentBackend, AgentConfig, BundleSource, CpuProfile, Diagnostics, Hypervisor, ImageDigest,
    ImageSettings, ReferenceSettings, RuntimeFiles, RuntimeSource, SandboxDefaults, SetupConfig,
};
use aci_edge_sandboxes::{
    Access, AciEdgeSandbox, EgressPolicy, ExecOutcome, ExecRequest, FilesystemPolicy,
    ForwardProtocol, HostLoopbackForward, ImageSource, NetworkPolicy, NetworkRule, Protocol,
    ProvisionRequest, SandboxSpec,
};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut openvmm = None;
    let mut kernel = None;
    let mut initrd = None;
    let mut bundle = None;
    let mut bundle_digest = None;
    let mut image = None;
    let mut image_ref = None;
    let mut library = None;
    let mut digest = None;
    let mut image_digest = ImageDigest::Compute;
    let mut pull_timeout_ms = None;
    let mut registry_auth_file = None;
    let mut root = None;
    let mut hypervisor = None;
    let mut filesystem = FilesystemPolicy::default();
    let mut egress = None;
    let mut allow = Vec::new();
    let mut deny = Vec::new();
    let mut host_loopback = None;
    let mut proxy = None;
    let mut forwards = Vec::new();
    let mut environment = Vec::new();
    let mut cwd = None;
    let mut memory_mib = None;
    let mut cpu_profile = CpuProfile::Auto;
    let mut guest_debug = false;
    let mut command = None;
    let mut args = std::env::args().skip(1);
    while let Some(option) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{option} requires a value"))
        };
        match option.as_str() {
            "--openvmm" => openvmm = Some(value()?),
            "--kernel" => kernel = Some(value()?),
            "--initrd" => initrd = Some(value()?),
            "--bundle" => bundle = Some(value()?),
            "--bundle-sha256" => bundle_digest = Some(parse_digest(&option, &value()?)?),
            "--image" => image = Some(value()?),
            "--image-ref" => image_ref = Some(value()?),
            "--registry-auth-file" => registry_auth_file = Some(value()?),
            "--host-library" => library = Some(value()?),
            "--host-sha256" => digest = Some(parse_digest(&option, &value()?)?),
            "--image-sha256" => {
                image_digest = ImageDigest::Expect(parse_digest(&option, &value()?)?);
            }
            "--pull-timeout-ms" => {
                pull_timeout_ms = Some(
                    value()?
                        .parse::<u64>()
                        .map_err(|_| format!("{option} must be a number of milliseconds"))?,
                );
            }
            "--state-root" => root = Some(value()?),
            "--hypervisor" => hypervisor = Some(value()?.parse::<Hypervisor>().map_err(describe)?),
            "--readonly" => filesystem.readonly_paths.push(value()?.into()),
            "--enumerate" => filesystem.enumerate_paths.push(value()?.into()),
            "--readwrite" => filesystem.readwrite_paths.push(value()?.into()),
            "--denied" => filesystem.denied_paths.push(value()?.into()),
            "--egress" => egress = Some(parse_access(&option, &value()?)?),
            "--egress-allow" => allow.push(parse_rule(&option, &value()?)?),
            "--egress-deny" => deny.push(parse_rule(&option, &value()?)?),
            "--host-loopback" => host_loopback = Some(parse_access(&option, &value()?)?),
            "--network-proxy" => proxy = Some(value()?),
            "--host-loopback-forward" => forwards.push(parse_forward(&option, &value()?)?),
            "--env" => environment.push(value()?),
            "--cwd" => cwd = Some(value()?),
            "--memory-mib" => {
                memory_mib = Some(
                    value()?
                        .parse::<u32>()
                        .map_err(|_| format!("{option} must be a number of MiB"))?,
                );
            }
            "--cpu-profile" => cpu_profile = parse_cpu_profile(&option, &value()?)?,
            "--guest-debug" => guest_debug = true,
            "--" => {
                command = Some(args.by_ref().collect::<Vec<_>>().join(" "));
                break;
            }
            other => return Err(format!("unexpected option {other}")),
        }
    }
    let required =
        |name: &str, value: Option<String>| value.ok_or_else(|| format!("missing required {name}"));
    let runtime_given = bundle.is_some()
        || bundle_digest.is_some()
        || openvmm.is_some()
        || kernel.is_some()
        || initrd.is_some();
    let library_given = library.is_some() || digest.is_some();
    let defaults = Defaults::new(!runtime_given || !library_given)?;
    let runtime = match (bundle, bundle_digest, defaults.runtime) {
        (Some(path), Some(manifest_sha256), _) => RuntimeSource::Bundle(BundleSource {
            path: path.into(),
            manifest_sha256,
        }),
        (None, None, Some(staged)) if !runtime_given => staged,
        (None, None, _) => RuntimeSource::Files(RuntimeFiles {
            openvmm: required("--openvmm", openvmm)?.into(),
            kernel: required("--kernel", kernel)?.into(),
            initrd: required("--initrd", initrd)?.into(),
            approved_sha256: None,
        }),
        _ => return Err("--bundle and --bundle-sha256 go together".to_owned()),
    };
    let image_ref = match (&image, image_ref, defaults.image) {
        (None, None, Some(reference)) if matches!(&runtime, RuntimeSource::Bundle(_)) => {
            Some(reference.to_owned())
        }
        (_, image_ref, _) => image_ref,
    };
    let image = parse_image_source(image, image_ref)?;
    if matches!(&image, ImageSource::Reference(_)) {
        if !matches!(&runtime, RuntimeSource::Bundle(_)) {
            return Err("--image-ref requires --bundle and --bundle-sha256".to_owned());
        }
        if !image_digest.is_compute() {
            return Err("--image-sha256 applies only to --image".to_owned());
        }
    } else if pull_timeout_ms.is_some() || registry_auth_file.is_some() {
        return Err("--pull-timeout-ms and --registry-auth-file require --image-ref".to_owned());
    }
    let (library, digest) = match (library, digest, defaults.library) {
        (None, None, Some(staged)) => staged,
        (library, digest, _) => (
            PathBuf::from(required("--host-library", library)?),
            digest.ok_or("missing required --host-sha256")?,
        ),
    };
    let root = match (root, defaults.state_root) {
        (Some(root), _) => PathBuf::from(root),
        (None, Some(staged)) => staged,
        (None, None) => return Err("missing required --state-root".to_owned()),
    };
    let command = command
        .filter(|command| !command.is_empty())
        .or(defaults.command.map(str::to_owned))
        .ok_or("pass the guest command after --")?;
    let mut request = ProvisionRequest::new();
    if !filesystem.is_empty() {
        request = request.with_filesystem(filesystem);
    }
    match egress {
        Some(default) => {
            let mut network = NetworkPolicy {
                egress: EgressPolicy {
                    default,
                    allow,
                    deny,
                },
                ..NetworkPolicy::deny_all()
            };
            if let Some(access) = host_loopback {
                network.ingress.host_loopback = Some(access);
            }
            request = request.with_network(network);
        }
        None if allow.is_empty() && deny.is_empty() && host_loopback.is_none() => {}
        None => {
            return Err(
                "--egress-allow, --egress-deny, and --host-loopback require --egress".to_owned(),
            );
        }
    }
    if let Some(proxy) = proxy {
        request = request.with_network_proxy(proxy);
    }
    let mut spec = SandboxSpec::new();
    for forward in forwards {
        spec = spec.with_host_loopback_forward(forward);
    }
    if let Some(memory_mib) = memory_mib {
        spec = spec.with_memory_mib(memory_mib);
    }
    let mut exec = ExecRequest::command_line(command);
    if let Some(cwd) = cwd {
        exec = exec.with_cwd(cwd);
    }
    if !environment.is_empty() {
        exec = exec.with_envs(environment).with_inherit_default_env(true);
    }

    let mut setup = SetupConfig::new(root, runtime)
        .with_cpu_profile(cpu_profile)
        .with_defaults(
            SandboxDefaults::default()
                .with_image(image)
                .with_image_digest(image_digest),
        )
        .with_diagnostics(Diagnostics {
            guest_debug,
            ..Diagnostics::default()
        });
    if let Some(hypervisor) = hypervisor {
        setup = setup.with_hypervisor(hypervisor);
    }
    let mut references = ReferenceSettings::default();
    if let Some(timeout) = pull_timeout_ms {
        references = references.with_pull_timeout_ms(timeout);
    }
    if let Some(auth_file) = registry_auth_file {
        references = references.with_auth_file(auth_file);
    }
    setup = setup.with_images(ImageSettings::default().with_references(references));
    setup.validate().map_err(describe)?;
    let backend =
        Arc::new(AgentBackend::new(AgentConfig::new(setup, library, digest)).map_err(describe)?);
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let id = client
        .provision_with(&request, &spec)
        .map_err(describe)?
        .sandbox_id;
    let started = client.start(&id);
    let succeeded = started.is_ok();
    let executed = started.and_then(|_| {
        let output = client.exec(&id, &exec)?.wait_with_output()?;
        let logs = backend.guest_logs(&id)?;
        if !logs.windows(5).any(|window| window == b"exec:") {
            return Err(aci_edge_sandboxes::Error::new(
                aci_edge_sandboxes::ErrorCode::BackendError,
                "the guest log omitted the executed command",
            ));
        }
        Ok(output)
    });
    // A failed start can leave OpenVMM running when its cleanup fails, so stop either way; only
    // after a failed start is a sandbox that is not running expected.
    let stopped = match client.stop(&id) {
        Err(error)
            if !succeeded && error.code() == aci_edge_sandboxes::ErrorCode::AlreadyStopped =>
        {
            None
        }
        stopped => Some(stopped),
    };
    let deprovisioned = if stopped.as_ref().is_none_or(Result::is_ok) {
        Some(client.deprovision(&id))
    } else {
        None
    };
    let mut failures = Vec::new();
    if let Err(error) = &executed {
        failures.push(format!("guest lifecycle: {error}"));
    }
    if let Some(Err(error)) = &stopped {
        failures.push(format!("stop: {error}"));
    }
    if let Some(Err(error)) = &deprovisioned {
        failures.push(format!("deprovision: {error}"));
    }
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }
    let stopped = stopped
        .expect("started guests always have a stop result")
        .map_err(describe)?;
    if stopped
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("forced"))
        .and_then(serde_json::Value::as_bool)
        != Some(false)
    {
        return Err("the guest did not shut down gracefully".to_owned());
    }
    let output = executed.map_err(describe)?;
    std::io::stdout()
        .write_all(&output.stdout)
        .map_err(|error| error.to_string())?;
    std::io::stderr()
        .write_all(&output.stderr)
        .map_err(|error| error.to_string())?;
    if output.outcome != ExecOutcome::Exited(0) {
        return Err(format!("guest command {}", output.outcome));
    }
    Ok(())
}

/// Directory beside this executable that holds the state of a build with `agent-bundled`.
#[cfg(feature = "agent-bundled")]
const STATE_DIRECTORY: &str = "aci-edge-sandboxes-state";
/// Image that a build with `agent-bundled` boots unless `--image` or `--image-ref` is given.
#[cfg(feature = "agent-bundled")]
const IMAGE: &str = "mcr.microsoft.com/azurelinux/base/core:3.0";
/// Command that a build with `agent-bundled` runs unless one is given after `--`.
#[cfg(feature = "agent-bundled")]
const COMMAND: &str = "printf READY; echo; head -n 2 /etc/os-release";

/// What a build with the `agent-bundled` feature uses for an option that is not given.
#[derive(Default)]
struct Defaults {
    runtime: Option<RuntimeSource>,
    library: Option<(PathBuf, [u8; 32])>,
    state_root: Option<PathBuf>,
    image: Option<&'static str>,
    command: Option<&'static str>,
}

impl Defaults {
    /// Returns the defaults, with the runtime and library of the staged bundle if `bundle` is
    /// set.
    #[cfg(feature = "agent-bundled")]
    fn new(bundle: bool) -> Result<Self, String> {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let directory = executable
            .parent()
            .ok_or("this executable is not in a directory")?;
        let mut defaults = Self {
            state_root: Some(directory.join(STATE_DIRECTORY)),
            image: Some(IMAGE),
            command: Some(COMMAND),
            ..Self::default()
        };
        if bundle {
            let staged = BundledAgent::discover().map_err(describe)?;
            eprintln!("using the agent bundle in {}", staged.directory().display());
            defaults.runtime = Some(staged.runtime());
            defaults.library = Some((staged.library().to_path_buf(), staged.library_sha256()));
        }
        Ok(defaults)
    }

    /// Returns no defaults: without `agent-bundled`, every option is required.
    #[cfg(not(feature = "agent-bundled"))]
    fn new(_bundle: bool) -> Result<Self, String> {
        Ok(Self::default())
    }
}

fn parse_image_source(
    image: Option<String>,
    image_ref: Option<String>,
) -> Result<ImageSource, String> {
    match (image, image_ref) {
        (Some(path), None) => Ok(ImageSource::Path(
            std::path::absolute(PathBuf::from(path)).map_err(|error| error.to_string())?,
        )),
        (None, Some(reference)) if !reference.trim().is_empty() => {
            Ok(ImageSource::Reference(reference))
        }
        (None, Some(_)) => Err("--image-ref must not be empty".to_owned()),
        (Some(_), Some(_)) => Err("pass either --image or --image-ref, not both".to_owned()),
        (None, None) => Err("missing required --image or --image-ref".to_owned()),
    }
}

fn parse_digest(option: &str, hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(format!(
            "{option} must contain exactly 64 hexadecimal digits"
        ));
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| format!("{option} must be hexadecimal"))?;
    }
    Ok(digest)
}

fn parse_access(option: &str, text: &str) -> Result<Access, String> {
    match text {
        "allow" => Ok(Access::Allow),
        "deny" => Ok(Access::Deny),
        _ => Err(format!("{option} must be allow or deny")),
    }
}

/// Parses `auto`, the built-in profile of the host's CPU, or `host`, a host profile.
fn parse_cpu_profile(option: &str, text: &str) -> Result<CpuProfile, String> {
    match text {
        "auto" => Ok(CpuProfile::Auto),
        "host" => Ok(CpuProfile::Host),
        _ => Err(format!("{option} must be auto or host")),
    }
}

/// Parses `CIDR` or `CIDR:tcp|udp:PORT`, such as `192.0.2.0/24` or `192.0.2.1:tcp:443`.
fn parse_rule(option: &str, text: &str) -> Result<NetworkRule, String> {
    let malformed = || format!("{option} must be CIDR or CIDR:tcp|udp:PORT");
    let mut parts = text.split(':');
    let rule = NetworkRule::to(
        parts
            .next()
            .filter(|cidr| !cidr.is_empty())
            .ok_or_else(malformed)?,
    );
    match (parts.next(), parts.next(), parts.next()) {
        (None, ..) => Ok(rule),
        (Some(protocol), Some(port), None) => {
            let protocol = match protocol {
                "tcp" => Protocol::Tcp,
                "udp" => Protocol::Udp,
                _ => return Err(malformed()),
            };
            Ok(rule.on_port(protocol, port.parse().map_err(|_| malformed())?))
        }
        _ => Err(malformed()),
    }
}

/// Parses `tcp|udp:HOST-PORT:GUEST-PORT`, such as `tcp:3000:8080`.
fn parse_forward(option: &str, text: &str) -> Result<HostLoopbackForward, String> {
    let malformed = || format!("{option} must be tcp|udp:HOST-PORT:GUEST-PORT");
    let parts: Vec<&str> = text.split(':').collect();
    let [protocol, host, guest] = parts.as_slice() else {
        return Err(malformed());
    };
    let protocol = match *protocol {
        "tcp" => ForwardProtocol::Tcp,
        "udp" => ForwardProtocol::Udp,
        _ => return Err(malformed()),
    };
    let port = |text: &str| text.parse::<u16>().ok().filter(|port| *port != 0);
    match (port(host), port(guest)) {
        (Some(host), Some(guest)) => Ok(HostLoopbackForward::new(protocol, host, guest)),
        _ => Err(malformed()),
    }
}

/// Describes an error of the crate or of NVX, with the crate's error code.
fn describe(error: impl Into<aci_edge_sandboxes::Error>) -> String {
    error.into().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_select_either_a_local_path_or_a_registry_reference() {
        let local = parse_image_source(Some("layers.gpt".into()), None).unwrap();
        assert!(matches!(local, ImageSource::Path(path) if path.is_absolute()));
        let reference = "registry.example:5443/team/image:1";
        assert_eq!(
            parse_image_source(None, Some(reference.into())).unwrap(),
            ImageSource::Reference(reference.into())
        );
        assert!(parse_image_source(None, None).is_err());
        assert!(parse_image_source(Some("layers.gpt".into()), Some(reference.into())).is_err());
        assert!(parse_image_source(None, Some(" ".into())).is_err());
    }

    #[test]
    fn digest_requires_exact_hex_bytes() {
        let parse = |hex: &str| parse_digest("--image-sha256", hex);
        assert_eq!(parse(&"ab".repeat(32)).unwrap(), [0xab; 32]);
        assert!(parse("ab").unwrap_err().starts_with("--image-sha256 "));
        assert!(parse(&"é".repeat(32)).is_err());
        assert!(parse(&"gg".repeat(32)).is_err());
    }

    #[test]
    fn egress_rules_name_a_network_and_optionally_one_port() {
        let parse = |text: &str| parse_rule("--egress-allow", text);
        assert_eq!(
            parse("192.0.2.0/24").unwrap(),
            NetworkRule::to("192.0.2.0/24")
        );
        assert_eq!(
            parse("192.0.2.1:udp:53").unwrap(),
            NetworkRule::to("192.0.2.1").on_port(Protocol::Udp, 53)
        );
        for malformed in [
            "",
            ":tcp:1",
            "192.0.2.1:tcp",
            "192.0.2.1:icmp:1",
            "192.0.2.1:tcp:x",
        ] {
            assert!(parse(malformed).is_err(), "{malformed}");
        }
        assert_eq!(parse_access("--egress", "deny").unwrap(), Access::Deny);
        assert!(parse_access("--egress", "Deny").is_err());
    }

    #[test]
    fn cpu_profiles_are_auto_or_host() {
        assert_eq!(
            parse_cpu_profile("--cpu-profile", "auto").unwrap(),
            CpuProfile::Auto
        );
        assert_eq!(
            parse_cpu_profile("--cpu-profile", "host").unwrap(),
            CpuProfile::Host
        );
        for other in ["", "Host", "amd.milan.v1"] {
            assert!(
                parse_cpu_profile("--cpu-profile", other).is_err(),
                "{other}"
            );
        }
    }

    #[test]
    fn forwards_name_a_protocol_and_two_ports() {
        let parse = |text: &str| parse_forward("--host-loopback-forward", text);
        assert_eq!(
            parse("tcp:3000:8080").unwrap(),
            HostLoopbackForward::new(ForwardProtocol::Tcp, 3000, 8080)
        );
        assert_eq!(
            parse("udp:5353:53").unwrap(),
            HostLoopbackForward::new(ForwardProtocol::Udp, 5353, 53)
        );
        for malformed in [
            "",
            "tcp:3000",
            "tcp:0:80",
            "tcp:80:0",
            "icmp:1:1",
            "tcp:1:2:3",
        ] {
            assert!(parse(malformed).is_err(), "{malformed}");
        }
    }
}

//! Runs the image-backed guest lifecycle with a separately supplied native host library.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use aci_edge_sandboxes::openvmm::{
    Hypervisor, ImageDigest, NvxHostBackend, NvxHostConfig, OpenVmmConfig, resolve_guest_path,
};
use aci_edge_sandboxes::{
    Access, AciEdgeSandbox, EgressPolicy, ExecOutcome, ExecRequest, FilesystemPolicy,
    ForwardProtocol, HostLoopbackForward, NetworkPolicy, NetworkRule, Protocol, ProvisionRequest,
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
    let mut image = None;
    let mut library = None;
    let mut digest = None;
    let mut image_digest = ImageDigest::Compute;
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
            "--image" => image = Some(value()?),
            "--host-library" => library = Some(value()?),
            "--host-sha256" => digest = Some(parse_digest(&option, &value()?)?),
            "--image-sha256" => {
                image_digest = ImageDigest::Expect(parse_digest(&option, &value()?)?);
            }
            "--state-root" => root = Some(value()?),
            "--hypervisor" => hypervisor = Some(value()?.parse::<Hypervisor>().map_err(describe)?),
            "--readonly" => filesystem.readonly_paths.push(value()?.into()),
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
            "--" => {
                command = Some(args.by_ref().collect::<Vec<_>>().join(" "));
                break;
            }
            other => return Err(format!("unexpected option {other}")),
        }
    }
    let required =
        |name: &str, value: Option<String>| value.ok_or_else(|| format!("missing required {name}"));
    let openvmm = required("--openvmm", openvmm)?;
    let kernel = required("--kernel", kernel)?;
    let initrd = required("--initrd", initrd)?;
    let image = required("--image", image)?;
    let library = required("--host-library", library)?;
    let root = required("--state-root", root)?;
    let digest = digest.ok_or("missing required --host-sha256")?;
    let hypervisor = hypervisor.ok_or("missing required --hypervisor")?;
    let command = command
        .filter(|command| !command.is_empty())
        .ok_or("pass the guest command after --")?;
    let mut request = ProvisionRequest::new();
    if !filesystem.is_empty() {
        for path in filesystem
            .readonly_paths
            .iter()
            .chain(&filesystem.readwrite_paths)
        {
            if let Some(guest) = resolve_guest_path(path) {
                eprintln!("{} is {guest} in the guest", path.display());
            }
        }
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
    for forward in forwards {
        request = request.with_host_loopback_forward(forward);
    }
    let mut exec = ExecRequest::command_line(command);
    if let Some(cwd) = cwd {
        exec = exec.with_cwd(cwd);
    }
    if !environment.is_empty() {
        exec = exec.with_envs(environment).with_inherit_default_env(true);
    }

    let config = OpenVmmConfig::new(openvmm, kernel, initrd, hypervisor, PathBuf::from(root));
    let backend = Arc::new(
        NvxHostBackend::new(
            NvxHostConfig::new(config, image, library, digest).with_image_digest(image_digest),
        )
        .map_err(describe)?,
    );
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let id = client.provision(&request).map_err(describe)?.sandbox_id;
    let started = client.start(&id);
    let succeeded = started.is_ok();
    let executed = started.and_then(|_| {
        let output = client.exec(&id, &exec)?.wait_with_output()?;
        let logs = backend.guest_logs(&id)?;
        if logs.is_empty() || !logs.windows(8).any(|window| window == b"execute:") {
            return Err(aci_edge_sandboxes::Error::new(
                aci_edge_sandboxes::ErrorCode::BackendError,
                "the guest did not return its execution log",
            ));
        }
        Ok(output)
    });
    let stopped = succeeded.then(|| client.stop(&id));
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

fn describe(error: aci_edge_sandboxes::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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

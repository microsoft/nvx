//! Runs the image-backed guest lifecycle with a separately supplied native host library.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use aci_edge_sandboxes::openvmm::{Hypervisor, NvxHostBackend, NvxHostConfig, OpenVmmConfig};
use aci_edge_sandboxes::{AciEdgeSandbox, ExecOutcome, ExecRequest, ProvisionRequest};

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
    let mut root = None;
    let mut hypervisor = None;
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
            "--host-sha256" => digest = Some(parse_digest(&value()?)?),
            "--state-root" => root = Some(value()?),
            "--hypervisor" => hypervisor = Some(value()?.parse::<Hypervisor>().map_err(describe)?),
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

    let config = OpenVmmConfig::new(openvmm, kernel, initrd, hypervisor, PathBuf::from(root));
    let backend = Arc::new(
        NvxHostBackend::new(NvxHostConfig::new(config, image, library, digest))
            .map_err(describe)?,
    );
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let id = client
        .provision(&ProvisionRequest::new())
        .map_err(describe)?
        .sandbox_id;
    let started = client.start(&id);
    let succeeded = started.is_ok();
    let executed = started.and_then(|_| {
        let output = client
            .exec(&id, &ExecRequest::command_line(command))?
            .wait_with_output()?;
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

fn parse_digest(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 || !hex.is_ascii() {
        return Err("--host-sha256 must contain exactly 64 hexadecimal digits".to_owned());
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| "--host-sha256 must be hexadecimal")?;
    }
    Ok(digest)
}

fn describe(error: aci_edge_sandboxes::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_requires_exact_hex_bytes() {
        assert_eq!(parse_digest(&"ab".repeat(32)).unwrap(), [0xab; 32]);
        assert!(parse_digest("ab").is_err());
        assert!(parse_digest(&"é".repeat(32)).is_err());
        assert!(parse_digest(&"gg".repeat(32)).is_err());
    }
}

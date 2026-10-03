//! Provisions a sandbox, runs one command line in it, and releases it again.
//!
//! ```text
//! cargo run --example lifecycle -- --release-dir /opt/nvx --hypervisor kvm -- cat /etc/os-release
//! ```
//!
//! Use `--repo <checkout>` instead of `--release-dir` to run the artifacts of an NVX repository
//! checkout. Without either option, the artifacts are located with `Artifacts::discover`. The
//! command runs in the guest's Alpine Linux userland.

use std::io::Write;
use std::process::ExitCode;

use aci_edge_sandboxes::openvmm::{Artifacts, Hypervisor, OpenVmmConfig};
use aci_edge_sandboxes::{AciEdgeSandbox, ExecRequest, ProvisionRequest};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let mut release_dir = None;
    let mut repo = None;
    let mut hypervisor = None;
    let mut command_line = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| format!("{argument} requires a value"))
        };
        match argument.as_str() {
            "--release-dir" => release_dir = Some(value()?),
            "--repo" => repo = Some(value()?),
            "--hypervisor" => hypervisor = Some(value()?.parse().map_err(describe)?),
            "--" => {
                command_line = Some(arguments.by_ref().collect::<Vec<_>>().join(" "));
            }
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    let hypervisor = match hypervisor {
        Some(hypervisor) => hypervisor,
        None => Hypervisor::from_env_or_default().map_err(describe)?,
    };
    let command_line = command_line
        .filter(|command_line| !command_line.is_empty())
        .ok_or("pass the command line after --")?;
    let artifacts = match (release_dir, repo) {
        (Some(release_dir), None) => Artifacts::from_release_dir(release_dir),
        (None, Some(repo)) => Artifacts::from_repo_layout(repo),
        (None, None) => Artifacts::discover(),
        _ => return Err("pass at most one of --release-dir and --repo".to_owned()),
    }
    .map_err(describe)?;
    let state_root = OpenVmmConfig::default_state_root().map_err(describe)?;
    let config = OpenVmmConfig::from_artifacts(artifacts, hypervisor, state_root);

    let client = AciEdgeSandbox::openvmm(config).map_err(describe)?;
    let sandbox = client
        .provision(&ProvisionRequest::new())
        .map_err(describe)?
        .sandbox_id;
    let result = client.start(&sandbox).and_then(|_| {
        client
            .exec(&sandbox, &ExecRequest::command_line(command_line))?
            .wait_with_output()
    });
    // Stopping fails harmlessly when the sandbox never started.
    let _ = client.stop(&sandbox);
    client.deprovision(&sandbox).map_err(describe)?;

    let output = result.map_err(describe)?;
    let _ = std::io::stdout().write_all(&output.stdout);
    let _ = std::io::stderr().write_all(&output.stderr);
    match output.outcome.exit_code() {
        Some(code) => Ok(ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX))),
        None => Err(format!("the workload {}", output.outcome)),
    }
}

fn describe(error: aci_edge_sandboxes::Error) -> String {
    error.to_string()
}

//! Deterministic backend policies agree with dry-run validation without runtime dependencies.
#![cfg(all(feature = "openvmm", any(target_os = "linux", windows)))]

use std::ffi::OsString;
use std::fs;
use std::io;
use std::time::Duration;

use aci_edge_sandboxes::openvmm::{Hypervisor, OpenVmmConfig};
use aci_edge_sandboxes::{
    Access, AciEdgeSandbox, ErrorCode, ExecIo, ExecRequest, FilesystemPolicy, NetworkPolicy,
    NetworkPort, NetworkRule, OutputSink, Protocol, ProvisionRequest, SandboxId,
};

fn config(directory: &tempfile::TempDir) -> OpenVmmConfig {
    OpenVmmConfig::new(
        directory.path().join("missing-openvmm"),
        directory.path().join("missing-kernel"),
        directory.path().join("missing-initrd"),
        Hypervisor::platform_default().unwrap(),
        directory.path().join("state"),
    )
}

fn client() -> (tempfile::TempDir, AciEdgeSandbox) {
    let directory = tempfile::tempdir().unwrap();
    let client = AciEdgeSandbox::openvmm(config(&directory)).unwrap();
    (directory, client)
}

fn state_entries(directory: &tempfile::TempDir) -> Vec<OsString> {
    let mut entries: Vec<_> = fs::read_dir(directory.path().join("state"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    entries.sort();
    entries
}

fn stale_id() -> SandboxId {
    "aci-edge-sandboxes:00000000000000000000000000000000"
        .parse()
        .unwrap()
}

struct UnusedSink;

impl OutputSink for UnusedSink {
    fn write(&mut self, _chunk: &[u8]) -> io::Result<()> {
        panic!("validation must not produce output")
    }
}

fn unused_io() -> ExecIo {
    ExecIo {
        stdout: Box::new(UnusedSink),
        stderr: Box::new(UnusedSink),
        stdin: None,
    }
}

fn assert_exec_rejected(client: &AciEdgeSandbox, request: &ExecRequest) {
    assert_eq!(
        client.validate_exec(request).unwrap_err().code(),
        ErrorCode::PolicyValidation
    );
    assert_eq!(
        client.exec(&stale_id(), request).unwrap_err().code(),
        ErrorCode::PolicyValidation
    );
    let error = match client.backend().exec(&stale_id(), request, unused_io()) {
        Err(error) => error,
        Ok(_) => panic!("the backend accepted an invalid exec request"),
    };
    assert_eq!(error.code(), ErrorCode::PolicyValidation);
}

#[test]
fn exec_validation_and_execution_share_guest_policy_checks() {
    let (directory, client) = client();
    let before = state_entries(&directory);
    for request in [
        ExecRequest::argv(["relative-program"]),
        ExecRequest::argv(["relative-program"]).with_cwd("/tmp"),
        ExecRequest::command_line("pwd").with_cwd("relative"),
        ExecRequest::command_line("x".repeat(4097)),
        ExecRequest::argv(["/bin/echo".to_owned(), "x".repeat(4097)]),
        ExecRequest::argv(vec!["/bin/true"; 65]),
        ExecRequest::argv(vec!["/bin/true"; 60]).with_cwd("/tmp"),
        ExecRequest::argv(["/bin/echo".to_owned(), "x".repeat(4097)]).with_cwd("/tmp"),
        ExecRequest::command_line("true").with_cwd(format!("/{}", "x".repeat(4096))),
        ExecRequest::command_line("true").with_timeout(Duration::from_millis(3_600_001)),
    ] {
        assert_exec_rejected(&client, &request);
    }
    assert_eq!(state_entries(&directory), before);
}

#[test]
fn exec_validation_accepts_the_exact_guest_limits() {
    let (directory, client) = client();
    let before = state_entries(&directory);
    let cwd_prelude = "cd -- \"$1\" || exit 125\nshift\n";
    for request in [
        ExecRequest::command_line("x".repeat(4096)),
        ExecRequest::argv(["/bin/echo".to_owned(), "x".repeat(4096)]),
        ExecRequest::argv(vec!["/bin/true"; 64]),
        ExecRequest::argv(vec!["/bin/true"; 59]).with_cwd("/tmp"),
        ExecRequest::command_line("x".repeat(4096 - cwd_prelude.len())).with_cwd("/tmp"),
        ExecRequest::command_line("true").with_cwd(format!("/{}", "x".repeat(4095))),
        ExecRequest::command_line("true").with_timeout(Duration::from_millis(3_600_000)),
    ] {
        client.validate_exec(&request).unwrap();
        client.backend().validate_exec(&request).unwrap();
    }
    assert_exec_rejected(
        &client,
        &ExecRequest::command_line("x".repeat(4097 - cwd_prelude.len())).with_cwd("/tmp"),
    );
    assert_eq!(state_entries(&directory), before);
}

fn network_request(rule: NetworkRule) -> ProvisionRequest {
    let mut network = NetworkPolicy::egress(Access::Deny);
    network.egress.allow.push(rule);
    ProvisionRequest::new().with_network(network)
}

fn port_range(protocol: Protocol, end: u16) -> NetworkRule {
    let mut rule = NetworkRule::to("192.0.2.0/24");
    rule.ports.push(NetworkPort {
        protocol,
        port: Some(1),
        end_port: Some(end),
    });
    rule
}

#[test]
fn provision_validation_rejects_backend_policies_before_probing() {
    let (directory, client) = client();
    let before = state_entries(&directory);
    let mut icmp = NetworkRule::to("192.0.2.0/24");
    icmp.ports.push(NetworkPort {
        protocol: Protocol::Icmp,
        port: None,
        end_port: None,
    });
    for request in [
        network_request(NetworkRule::to("::/0")),
        network_request(icmp),
        network_request(port_range(Protocol::Tcp, 257)),
        network_request(port_range(Protocol::Any, 129)),
    ] {
        assert_eq!(
            client.validate_provision(&request).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
        assert_eq!(
            client.provision(&request).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
        assert_eq!(
            client.backend().provision(&request).unwrap_err().code(),
            ErrorCode::PolicyValidation
        );
    }
    assert_eq!(state_entries(&directory), before);
}

#[test]
fn kernel_command_lines_without_room_are_rejected_before_probing() {
    // The artifacts are missing, so a check that ran after probing them would report them instead.
    let directory = tempfile::tempdir().unwrap();
    let mut oversized = config(&directory);
    oversized.kernel_command_line = "x".repeat(1024);
    let error = AciEdgeSandbox::openvmm(oversized).unwrap_err();
    assert_eq!(error.code(), ErrorCode::BackendUnavailable);
    assert!(
        error.message().contains("1024-byte kernel command line"),
        "{error}"
    );
    assert!(!directory.path().join("state").exists());
}

#[test]
fn validation_is_side_effect_free_and_host_checks_remain_in_operations() {
    let (directory, client) = client();
    let before = state_entries(&directory);
    for request in [
        ProvisionRequest::new(),
        network_request(port_range(Protocol::Tcp, 256)),
        network_request(port_range(Protocol::Any, 128)),
        ProvisionRequest::new().with_filesystem(FilesystemPolicy {
            readonly_paths: vec![directory.path().join("missing-mapped-file")],
            ..FilesystemPolicy::default()
        }),
    ] {
        client.validate_provision(&request).unwrap();
        client.backend().validate_provision(&request).unwrap();
        assert_eq!(
            client.provision(&request).unwrap_err().code(),
            ErrorCode::BackendUnavailable
        );
    }
    assert_eq!(state_entries(&directory), before);
}

#[test]
fn structural_and_capability_errors_precede_backend_policy_checks() {
    let (_directory, client) = client();
    assert_eq!(
        client
            .validate_exec(&ExecRequest::argv([""]))
            .unwrap_err()
            .code(),
        ErrorCode::MalformedRequest
    );
    let request = ExecRequest::argv(["relative-program"]).with_env("KEY=value");
    let error = client.validate_exec(&request).unwrap_err();
    assert_eq!(error.code(), ErrorCode::PolicyValidation);
    assert!(error.message().contains("process.env"));
    assert_eq!(
        client
            .validate_provision(&ProvisionRequest::new().with_memory_mib(0))
            .unwrap_err()
            .code(),
        ErrorCode::MalformedRequest
    );
}

#[cfg(feature = "async")]
#[test]
fn async_execution_uses_the_same_backend_validation_hooks() {
    let (directory, client) = client();
    let before = state_entries(&directory);
    let client = aci_edge_sandboxes::AsyncAciEdgeSandbox::from(client);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let error = client
                .exec(stale_id(), ExecRequest::argv(["relative-program"]))
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::PolicyValidation);
            let error = client
                .provision(network_request(NetworkRule::to("::/0")))
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::PolicyValidation);
        });
    assert_eq!(state_entries(&directory), before);
}

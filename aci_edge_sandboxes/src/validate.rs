//! Backend-independent request validation.
//!
//! Structural checks run first and report [`ErrorCode::MalformedRequest`]. Capability checks
//! then compare the request against the backend's [`Capabilities`] and report
//! [`ErrorCode::PolicyValidation`]. Both run before the backend sees the request.
//!
//! [`ErrorCode::MalformedRequest`]: crate::ErrorCode::MalformedRequest
//! [`ErrorCode::PolicyValidation`]: crate::ErrorCode::PolicyValidation

use std::path::Path;

use crate::capabilities::Capabilities;
use crate::cidr::Cidr;
use crate::error::{Error, Result};
use crate::model::{
    Access, Command, ExecRequest, FilesystemPolicy, NetworkRule, Protocol, ProvisionRequest,
    StdinMode, duration_millis,
};

pub(crate) fn provision_structure(request: &ProvisionRequest) -> Result<()> {
    if request.microvm.provision.memory_mib == Some(0) {
        return Err(Error::malformed_request(
            "microvm.provision.memoryMib must be positive",
        ));
    }
    if let Some(filesystem) = &request.filesystem {
        for (paths, field) in filesystem_fields(filesystem) {
            for path in paths {
                host_path(path, field)?;
            }
        }
    }
    if let Some(network) = &request.network {
        for (rules, field) in [
            (&network.egress.allow, "network.egress.allow"),
            (&network.egress.deny, "network.egress.deny"),
        ] {
            for (index, rule) in rules.iter().enumerate() {
                rule_structure(rule, &format!("{field}[{index}]"))?;
            }
        }
    }
    Ok(())
}

fn rule_structure(rule: &NetworkRule, field: &str) -> Result<()> {
    let malformed = |message: String| Err(Error::malformed_request(format!("{field}{message}")));
    for (index, peer) in rule.to.iter().enumerate() {
        let cidr = match Cidr::parse(&peer.cidr) {
            Ok(cidr) => cidr,
            Err(message) => return malformed(format!(".to[{index}].cidr: {message}")),
        };
        for (exception, value) in peer.except.iter().enumerate() {
            match Cidr::parse(value) {
                Ok(excluded) if cidr.contains(excluded) => {}
                Ok(_) => {
                    return malformed(format!(
                        ".to[{index}].except[{exception}] must lie within {:?}",
                        peer.cidr
                    ));
                }
                Err(message) => {
                    return malformed(format!(".to[{index}].except[{exception}]: {message}"));
                }
            }
        }
    }
    for (index, port) in rule.ports.iter().enumerate() {
        let path = format!(".ports[{index}]");
        if port.port == Some(0) || port.end_port == Some(0) {
            return malformed(format!("{path}: ports must be between 1 and 65535"));
        }
        match (port.port, port.end_port) {
            (None, Some(_)) => return malformed(format!("{path}.endPort requires port")),
            (Some(start), Some(end)) if end < start => {
                return malformed(format!("{path}.endPort must not be below port"));
            }
            _ => {}
        }
        if port.protocol == Protocol::Icmp && port.port.is_some() {
            return malformed(format!("{path}: ICMP has no ports"));
        }
    }
    Ok(())
}

pub(crate) fn provision_capabilities(
    request: &ProvisionRequest,
    capabilities: &Capabilities,
) -> Result<()> {
    let backend = &capabilities.backend;
    if let Some(filesystem) = &request.filesystem {
        let supported = &capabilities.filesystem;
        for ((paths, field), honored) in filesystem_fields(filesystem).into_iter().zip([
            supported.readonly_paths,
            supported.readwrite_paths,
            supported.denied_paths,
        ]) {
            if !paths.is_empty() && !honored {
                return Err(Error::policy_validation(format!(
                    "the {backend} backend cannot enforce {field}"
                )));
            }
        }
    }
    if let Some(network) = &request.network {
        let supported = &capabilities.network;
        let checks = [
            (
                "network.egress.default",
                network.egress.default,
                supported.egress_allow,
                supported.egress_deny,
            ),
            (
                "network.ingress.default",
                network.ingress.default,
                supported.ingress_allow,
                supported.ingress_deny,
            ),
        ];
        for (field, access, allow, deny) in checks {
            check_access(backend, field, access, allow, deny)?;
        }
        if let Some(access) = network.ingress.host_loopback {
            check_access(
                backend,
                "network.ingress.hostLoopback",
                access,
                supported.host_loopback_allow,
                supported.host_loopback_deny,
            )?;
        }
        let has_rules = !network.egress.allow.is_empty() || !network.egress.deny.is_empty();
        if has_rules && !supported.egress_rules {
            return Err(Error::policy_validation(format!(
                "the {backend} backend cannot enforce network.egress allow or deny rules"
            )));
        }
    }
    Ok(())
}

pub(crate) fn exec_structure(request: &ExecRequest) -> Result<()> {
    let process = &request.process;
    match &process.command {
        Command::CommandLine(command_line) => {
            if command_line.trim().is_empty() {
                return Err(Error::malformed_request("process.commandLine is empty"));
            }
            no_nul(command_line, "process.commandLine")?;
        }
        Command::Argv(argv) => {
            if argv.is_empty() {
                return Err(Error::malformed_request("process.argv is empty"));
            }
            for argument in argv {
                if argument.is_empty() {
                    return Err(Error::malformed_request(
                        "process.argv contains an empty argument",
                    ));
                }
                no_nul(argument, "process.argv")?;
            }
        }
    }
    if let Some(cwd) = &process.cwd {
        if cwd.is_empty() {
            return Err(Error::malformed_request("process.cwd is empty"));
        }
        no_nul(cwd, "process.cwd")?;
    }
    for entry in &process.env {
        let valid = entry
            .split_once('=')
            .is_some_and(|(name, _)| !name.is_empty());
        if !valid {
            return Err(Error::malformed_request(
                "process.env entries must have the form KEY=VALUE",
            ));
        }
        no_nul(entry, "process.env")?;
    }
    Ok(())
}

pub(crate) fn exec_capabilities(request: &ExecRequest, capabilities: &Capabilities) -> Result<()> {
    let backend = &capabilities.backend;
    let supported = &capabilities.exec;
    let process = &request.process;
    let unsupported = |feature: &str| {
        Err(Error::policy_validation(format!(
            "the {backend} backend cannot honor {feature}"
        )))
    };
    match process.command {
        Command::CommandLine(_) if !supported.command_line => {
            return unsupported("process.commandLine");
        }
        Command::Argv(_) if !supported.argv => return unsupported("process.argv"),
        _ => {}
    }
    if request.stdin == StdinMode::Piped && !supported.stdin {
        return unsupported("live standard input; use StdinMode::Null");
    }
    if process.cwd.is_some() && !supported.cwd {
        return unsupported("process.cwd");
    }
    if !process.env.is_empty() && !supported.env {
        return unsupported("process.env");
    }
    if process.inherit_default_env == Some(false) && !supported.clear_default_env {
        return unsupported("process.inheritDefaultEnv: false");
    }
    if let (Some(timeout), Some(maximum)) = (process.timeout, supported.max_timeout_ms)
        && duration_millis(timeout) > maximum
    {
        return Err(Error::policy_validation(format!(
            "process.timeout exceeds the {backend} backend limit of {maximum} ms"
        )));
    }
    Ok(())
}

fn filesystem_fields(filesystem: &FilesystemPolicy) -> [(&[std::path::PathBuf], &'static str); 3] {
    [
        (&filesystem.readonly_paths, "filesystem.readonlyPaths"),
        (&filesystem.readwrite_paths, "filesystem.readwritePaths"),
        (&filesystem.denied_paths, "filesystem.deniedPaths"),
    ]
}

fn check_access(backend: &str, field: &str, access: Access, allow: bool, deny: bool) -> Result<()> {
    let honored = match access {
        Access::Allow => allow,
        Access::Deny => deny,
    };
    if honored {
        Ok(())
    } else {
        Err(Error::policy_validation(format!(
            "the {backend} backend cannot enforce {field}: {access}"
        )))
    }
}

fn host_path(path: &Path, field: &str) -> Result<()> {
    let Some(value) = path.to_str() else {
        return Err(Error::malformed_request(format!(
            "{field} must be valid UTF-8"
        )));
    };
    no_nul(value, field)?;
    if !path.is_absolute() {
        return Err(Error::malformed_request(format!(
            "{field} must be an absolute host path: {value}"
        )));
    }
    Ok(())
}

fn no_nul(value: &str, field: &str) -> Result<()> {
    if value.contains('\0') {
        Err(Error::malformed_request(format!(
            "{field} contains a NUL character"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::ErrorCode;
    use crate::model::NetworkPolicy;

    fn absolute(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:\\nvx\\{name}"))
        } else {
            PathBuf::from(format!("/nvx/{name}"))
        }
    }

    fn request() -> ProvisionRequest {
        ProvisionRequest::new()
    }

    fn capabilities() -> Capabilities {
        let mut capabilities = Capabilities::new("test");
        capabilities.exec.command_line = true;
        capabilities.exec.max_timeout_ms = Some(1_000);
        capabilities.network.egress_allow = true;
        capabilities.network.egress_deny = true;
        capabilities.network.ingress_deny = true;
        capabilities.network.host_loopback_deny = true;
        capabilities
    }

    fn code(result: Result<()>) -> ErrorCode {
        result.unwrap_err().code()
    }

    #[test]
    fn provision_structure_rejects_shape_errors() {
        assert_eq!(
            code(provision_structure(&request().with_memory_mib(0))),
            ErrorCode::MalformedRequest
        );
        let relative = request().with_filesystem(FilesystemPolicy {
            readonly_paths: vec![PathBuf::from("source")],
            ..FilesystemPolicy::default()
        });
        assert_eq!(
            code(provision_structure(&relative)),
            ErrorCode::MalformedRequest
        );
        provision_structure(&request()).unwrap();
        provision_structure(&request().with_memory_mib(512)).unwrap();
    }

    #[test]
    fn provision_capabilities_reject_unsupported_policy() {
        let capabilities = capabilities();
        let filesystem = request().with_filesystem(FilesystemPolicy {
            readonly_paths: vec![absolute("source")],
            ..FilesystemPolicy::default()
        });
        assert_eq!(
            code(provision_capabilities(&filesystem, &capabilities)),
            ErrorCode::PolicyValidation
        );
        let mut ingress = NetworkPolicy::deny_all();
        ingress.ingress.default = Access::Allow;
        let network = request().with_network(ingress);
        assert_eq!(
            code(provision_capabilities(&network, &capabilities)),
            ErrorCode::PolicyValidation
        );
        let empty_filesystem = request()
            .with_filesystem(FilesystemPolicy::default())
            .with_network(NetworkPolicy::deny_all());
        provision_capabilities(&empty_filesystem, &capabilities).unwrap();
    }

    #[test]
    fn egress_rules_are_checked_for_shape_and_support() {
        use crate::model::{NetworkPeer, NetworkPort};

        let with_rule = |rule: NetworkRule| {
            request().with_network(NetworkPolicy {
                egress: crate::model::EgressPolicy::new(Access::Deny).with_allow(rule),
                ..NetworkPolicy::deny_all()
            })
        };
        let port = |protocol, port, end_port| NetworkPort {
            protocol,
            port,
            end_port,
        };
        for rule in [
            NetworkRule::to("10.0.0.1/8"),
            NetworkRule::to("example.com"),
            NetworkRule {
                to: vec![NetworkPeer {
                    cidr: "10.0.0.0/8".to_owned(),
                    except: vec!["11.0.0.0/16".to_owned()],
                }],
                ports: Vec::new(),
            },
            NetworkRule::to("10.0.0.0/8").on_port(Protocol::Tcp, 0),
            NetworkRule {
                to: Vec::new(),
                ports: vec![port(Protocol::Tcp, None, Some(80))],
            },
            NetworkRule {
                to: Vec::new(),
                ports: vec![port(Protocol::Tcp, Some(90), Some(80))],
            },
            NetworkRule {
                to: Vec::new(),
                ports: vec![port(Protocol::Icmp, Some(1), None)],
            },
        ] {
            assert_eq!(
                code(provision_structure(&with_rule(rule.clone()))),
                ErrorCode::MalformedRequest,
                "{rule:?}"
            );
        }
        let valid = with_rule(NetworkRule {
            to: vec![NetworkPeer {
                cidr: "10.0.0.0/8".to_owned(),
                except: vec!["10.1.0.0/16".to_owned()],
            }],
            ports: vec![port(Protocol::Tcp, Some(80), Some(81))],
        });
        provision_structure(&valid).unwrap();
        assert_eq!(
            code(provision_capabilities(&valid, &capabilities())),
            ErrorCode::PolicyValidation
        );
        let mut supported = capabilities();
        supported.network.egress_rules = true;
        provision_capabilities(&valid, &supported).unwrap();
    }

    #[test]
    fn exec_structure_rejects_shape_errors() {
        for request in [
            ExecRequest::command_line("  "),
            ExecRequest::argv(Vec::<String>::new()),
            ExecRequest::argv(["/bin/echo", ""]),
            ExecRequest::command_line("echo").with_env("NOVALUE"),
            ExecRequest::command_line("echo").with_env("=value"),
            ExecRequest::command_line("echo").with_cwd(""),
            ExecRequest::command_line("echo\0"),
        ] {
            assert_eq!(
                code(exec_structure(&request)),
                ErrorCode::MalformedRequest,
                "{request:?}"
            );
        }
        exec_structure(&ExecRequest::command_line("echo").with_env("A=")).unwrap();
    }

    #[test]
    fn exec_capabilities_reject_unsupported_features() {
        let capabilities = capabilities();
        for request in [
            ExecRequest::argv(["/bin/true"]),
            ExecRequest::command_line("cat").with_stdin(StdinMode::Piped),
            ExecRequest::command_line("pwd").with_cwd("/tmp"),
            ExecRequest::command_line("env").with_env("A=B"),
            ExecRequest::command_line("env").with_inherit_default_env(false),
            ExecRequest::command_line("sleep 2").with_timeout(Duration::from_secs(2)),
        ] {
            assert_eq!(
                code(exec_capabilities(&request, &capabilities)),
                ErrorCode::PolicyValidation,
                "{request:?}"
            );
        }
        exec_capabilities(
            &ExecRequest::command_line("true")
                .with_inherit_default_env(true)
                .with_timeout(Duration::from_secs(1)),
            &capabilities,
        )
        .unwrap();
    }
}

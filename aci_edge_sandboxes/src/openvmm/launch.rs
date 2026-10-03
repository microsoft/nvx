//! OpenVMM command line of a managed microVM.
//!
//! The VM runs the guest's own Alpine Linux userland directly, with no sandbox layers or scratch
//! disk. OpenVMM's managed lifecycle adds the control tty and workload identity tokens; the guest
//! init then starts the managed agent, which runs workloads as the configured non-root identity.
//! The arguments mirror the managed-lifecycle check of `scripts/nvx_tools/microvm_tests.py`.

use std::ffi::OsString;
use std::path::Path;

use super::config::OpenVmmConfig;
use super::state::SandboxRecord;
use super::{filesystem, network};
use crate::error::{Error, Result};

/// Bytes of the x86 kernel command line available to caller tokens, including the NUL.
pub(crate) const COMMAND_LINE_BUDGET: usize = 1024;

/// Returns the start of every sandbox's kernel command line, which the configuration alone
/// determines: the caller's extra parameters and the workload hostname.
pub(crate) fn base_command_line(extra: &str, hostname: &str) -> String {
    let hostname = format!("hostname={hostname}");
    match extra.trim() {
        "" => hostname,
        extra => format!("{extra} {hostname}"),
    }
}

/// Builds the kernel command line: the caller's extra parameters, the workload hostname, and
/// the guest's bind mounts of mapped host paths.
pub(crate) fn kernel_command_line(extra: &str, record: &SandboxRecord) -> Result<String> {
    let mut tokens = vec![base_command_line(extra, &record.hostname)];
    if record.create_workload_account {
        tokens.push("nvx_workload_account=create".to_owned());
    }
    if let Some(mapping) = &record.filesystem {
        tokens.extend(filesystem::kernel_tokens(mapping));
    }
    let command_line = tokens.join(" ");
    if command_line.len() + 1 > COMMAND_LINE_BUDGET {
        return Err(Error::policy_validation(format!(
            "the kernel command line exceeds its {COMMAND_LINE_BUDGET}-byte budget; map fewer or \
             shorter filesystem paths"
        )));
    }
    Ok(command_line)
}

/// Builds the complete OpenVMM argument list for starting a sandbox.
pub(crate) fn openvmm_arguments(
    config: &OpenVmmConfig,
    record: &SandboxRecord,
    endpoint: &str,
    report: &Path,
) -> Result<Vec<OsString>> {
    let mut arguments: Vec<OsString> = vec!["--machine".into(), "microvm".into()];
    arguments.push("--microvm-workload-identity".into());
    arguments.push(format!("{}:{}", record.workload_uid, record.workload_gid).into());
    for argument in [
        "--microvm-lifecycle",
        "managed",
        "--single-process",
        "--hypervisor",
        config.hypervisor.as_str(),
    ] {
        arguments.push(argument.into());
    }
    arguments.push("--memory".into());
    arguments.push(format!("{}M", record.memory_mib).into());
    arguments.push("--kernel".into());
    arguments.push(config.kernel.clone().into());
    arguments.push("--initrd".into());
    arguments.push(config.initrd.clone().into());
    arguments.push("--cmdline".into());
    arguments.push(kernel_command_line(&config.kernel_command_line, record)?.into());
    arguments.push("--virtio-console".into());
    arguments.push("none".into());
    arguments.push("--microvm-control-console".into());
    arguments.push(format!("listen={endpoint}").into());
    arguments.push("--microvm-control-auth-stdin".into());
    arguments.push("--microvm-report".into());
    arguments.push(report.into());
    arguments.extend(
        network::network_arguments(record.network.as_ref(), &config.guest_network)?
            .into_iter()
            .map(OsString::from),
    );
    if let Some(mapping) = &record.filesystem {
        arguments.extend(
            filesystem::openvmm_arguments(mapping)
                .into_iter()
                .map(OsString::from),
        );
    }
    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openvmm::config::Hypervisor;
    use crate::openvmm::state::{BACKEND_KEY, STATE_FORMAT};

    fn record() -> SandboxRecord {
        SandboxRecord {
            format: STATE_FORMAT,
            backend: BACKEND_KEY.to_owned(),
            network: None,
            filesystem: None,
            memory_mib: 256,
            workload_uid: 65534,
            workload_gid: 65534,
            create_workload_account: false,
            hostname: "nvx-sandbox".to_owned(),
        }
    }

    #[test]
    fn kernel_command_line_sets_the_hostname() {
        assert_eq!(
            kernel_command_line("quiet loglevel=0", &record()).unwrap(),
            "quiet loglevel=0 hostname=nvx-sandbox"
        );
        assert_eq!(
            kernel_command_line("", &record()).unwrap(),
            "hostname=nvx-sandbox"
        );
        let long = "x".repeat(COMMAND_LINE_BUDGET);
        assert!(kernel_command_line(&long, &record()).is_err());
    }

    #[test]
    fn arguments_select_the_managed_direct_guest() {
        let config = OpenVmmConfig::new("/o", "/k/vmlinux", "/k/initrd", Hypervisor::Kvm, "/state");
        let arguments = openvmm_arguments(
            &config,
            &record(),
            "/state/t/control.sock",
            Path::new("/state/t/outcome.json"),
        )
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
    fn mappings_add_the_export_and_guest_bind_tokens() {
        let mut record = record();
        record.filesystem = Some(filesystem::HostMapping {
            root: "/host/work".into(),
            writable: false,
            denied: vec!["/host/work/secret".into()],
            denied_identities: Vec::new(),
            binds: vec![filesystem::Bind {
                source: String::new(),
                target: "/host/work".to_owned(),
                read_only: true,
                identity: None,
            }],
        });
        assert_eq!(
            kernel_command_line("", &record).unwrap(),
            "hostname=nvx-sandbox nvx_map=.,/host/work,ro"
        );
        record.create_workload_account = true;
        assert_eq!(
            kernel_command_line("", &record).unwrap(),
            "hostname=nvx-sandbox nvx_workload_account=create nvx_map=.,/host/work,ro"
        );
        record.create_workload_account = false;
        let config = OpenVmmConfig::new("/o", "/k/vmlinux", "/k/initrd", Hypervisor::Kvm, "/state");
        let arguments = openvmm_arguments(&config, &record, "/e", Path::new("/r")).unwrap();
        let arguments: Vec<&str> = arguments.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            arguments[arguments.len() - 4..],
            [
                "--mount",
                "/run/nvx/hostfs/root,/host/work,ro",
                "--mount-deny",
                "/host/work/secret",
            ]
        );
    }
}

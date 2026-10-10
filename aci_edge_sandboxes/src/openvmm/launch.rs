//! OpenVMM command line of a managed microVM.
//!
//! The VM runs the guest's own Alpine Linux userland directly, with no sandbox layers or scratch
//! disk. OpenVMM's managed lifecycle adds the control tty and workload identity tokens; the guest
//! init then starts the managed agent, which runs workloads as the configured non-root identity.
//! The arguments mirror the managed-lifecycle check of `scripts/nvx_tools/microvm_tests.py`.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use super::config::OpenVmmConfig;
use super::state::SandboxRecord;
use super::{filesystem, network};
use crate::error::{Error, Result};

/// Bytes of the x86 kernel command line available to caller tokens, including the NUL.
pub(crate) const COMMAND_LINE_BUDGET: usize = 1024;
/// Longest Windows process command line, in UTF-16 code units, including the terminating NUL.
///
/// The backend holds OpenVMM's command line to it on every host, so a policy behaves alike on
/// all of them.
pub(crate) const PROCESS_COMMAND_LINE_LIMIT: usize = 32_767;
/// Part of that limit kept for what may change between provision and start, such as the
/// configured OpenVMM and guest paths.
const PROCESS_COMMAND_LINE_RESERVE: usize = 1024;

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
/// the number of host mappings that the guest agent receives over the control channel.
pub(crate) fn kernel_command_line(extra: &str, record: &SandboxRecord) -> Result<String> {
    let mut tokens = vec![base_command_line(extra, &record.hostname)];
    if record.create_workload_account {
        tokens.push("nvx_workload_account=create".to_owned());
    }
    if let Some(mapping) = &record.filesystem {
        tokens.push(filesystem::kernel_token(mapping));
    }
    let command_line = tokens.join(" ");
    if command_line.len() + 1 > COMMAND_LINE_BUDGET {
        return Err(Error::policy_validation(format!(
            "the kernel command line exceeds its {COMMAND_LINE_BUDGET}-byte budget; shorten the \
             extra kernel parameters or the hostname"
        )));
    }
    Ok(command_line)
}

/// Fails if OpenVMM's command line for `record` would not fit [`PROCESS_COMMAND_LINE_LIMIT`]
/// with room to spare, as can happen with many filesystem paths.
pub(crate) fn check_process_command_line(
    config: &OpenVmmConfig,
    record: &SandboxRecord,
    endpoint: &str,
    report: &Path,
) -> Result<()> {
    let arguments = openvmm_arguments(config, record, endpoint, report)?;
    let length = process_command_line_units(config.openvmm.as_os_str(), &arguments);
    if length + PROCESS_COMMAND_LINE_RESERVE > PROCESS_COMMAND_LINE_LIMIT {
        return Err(Error::policy_validation(format!(
            "OpenVMM's command line would take {length} of the {PROCESS_COMMAND_LINE_LIMIT} \
             characters that a Windows process command line allows, with \
             {PROCESS_COMMAND_LINE_RESERVE} held in reserve; map, hide, or allow fewer or \
             shorter filesystem paths, or use fewer network rules"
        )));
    }
    Ok(())
}

/// Length in UTF-16 code units, including the terminating NUL, of the Windows command line that
/// runs `program` with `arguments`, quoted the way the Windows platform quotes it.
fn process_command_line_units(program: &OsStr, arguments: &[OsString]) -> usize {
    let mut length = 0;
    for (index, argument) in std::iter::once(program)
        .chain(arguments.iter().map(OsString::as_os_str))
        .enumerate()
    {
        let text = argument.to_string_lossy();
        let quoted = index == 0 || text.is_empty() || text.contains([' ', '\t']);
        // A separating space, or the terminating NUL after the last argument.
        length += 1 + text.encode_utf16().count() + if quoted { 2 } else { 0 };
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
    }
    length
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
    fn mappings_add_the_export_and_the_mapping_count() {
        let mut record = record();
        record.filesystem = Some(filesystem::HostMapping {
            children: vec![filesystem::Child {
                root: "/host/work".into(),
                file: false,
                writable: false,
                hidden: false,
                denied: vec!["/host/work/secret".into()],
                denied_identities: vec![None],
                allowed: Vec::new(),
                write: Vec::new(),
            }],
            binds: vec![filesystem::Bind {
                child: 0,
                source: String::new(),
                target: "/host/work".to_owned(),
                read_only: true,
                identity: None,
            }],
        });
        assert_eq!(
            kernel_command_line("", &record).unwrap(),
            "hostname=nvx-sandbox nvx_maps=1"
        );
        record.create_workload_account = true;
        assert_eq!(
            kernel_command_line("", &record).unwrap(),
            "hostname=nvx-sandbox nvx_workload_account=create nvx_maps=1"
        );
        record.create_workload_account = false;
        let config = OpenVmmConfig::new("/o", "/k/vmlinux", "/k/initrd", Hypervisor::Kvm, "/state");
        let arguments = openvmm_arguments(&config, &record, "/e", Path::new("/r")).unwrap();
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
        let config = OpenVmmConfig::new("/o", "/k/vmlinux", "/k/initrd", Hypervisor::Kvm, "/state");
        let mut record = record();
        let report = Path::new("/state/t/outcome.json");
        check_process_command_line(&config, &record, "/state/t/control.sock", report).unwrap();
        let denied: Vec<std::path::PathBuf> = (0..128)
            .map(|index| format!("/host/work/{index:03}-{}", "d".repeat(240)).into())
            .collect();
        record.filesystem = Some(filesystem::HostMapping {
            children: vec![filesystem::Child {
                root: "/host/work".into(),
                file: false,
                writable: false,
                hidden: false,
                denied_identities: vec![None; denied.len()],
                denied,
                allowed: Vec::new(),
                write: Vec::new(),
            }],
            binds: Vec::new(),
        });
        let error = check_process_command_line(&config, &record, "/state/t/control.sock", report)
            .unwrap_err();
        assert_eq!(error.code(), crate::ErrorCode::PolicyValidation);
    }
}

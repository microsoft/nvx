use super::seam::tests::TestSystem;
use super::*;
pub(crate) use crate::guest::sys::tests::isolate_process_test;

#[cfg(feature = "cli")]
/// In-process host/broker/agent integration harness.
mod loopback;

/// Serializes tests that temporarily alter process-global state.
#[cfg(feature = "cli")]
pub(crate) static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl AgentConfig {
    /// Returns the configuration of a test agent of the workload `1000:1000`, named `workload`
    /// and homed in `/home/workload`.
    #[cfg(feature = "cli")]
    pub(crate) fn for_test() -> Self {
        Self {
            uid: c"1000".to_owned(),
            gid: c"1000".to_owned(),
            user: c"workload".to_owned(),
            home: c"/home/workload".to_owned(),
            restored_proxy: None,
        }
    }
}

#[test]
fn a_layered_cold_boot_takes_only_the_clis_workload_identity() {
    for command_line in [
        b"nvx_workload_uid=9 nvx_workload_gid=10 nvx_identity=1000:1001".as_slice(),
        b"x\x0cnvx_identity=1001:1001 nvx_identity=1000:1001",
    ] {
        let config = AgentConfig::layered(
            command_line,
            c"workload".to_owned(),
            c"/home/workload".to_owned(),
        )
        .unwrap();
        assert_eq!(config.uid.as_bytes(), b"1000");
        assert_eq!(config.gid.as_bytes(), b"1001");
        assert_eq!(config.user.as_c_str(), c"workload");
        assert_eq!(config.home.as_c_str(), c"/home/workload");
    }
}

#[test]
fn a_layered_cold_boot_refuses_missing_repeated_or_invalid_cli_identities() {
    for command_line in [
        b"nvx_workload_uid=1000 nvx_workload_gid=1000".as_slice(),
        b"nvx_identity=1000:1000 nvx_identity=1001:1001",
        b"nvx_identity=",
        b"nvx_identity=1000",
        b"nvx_identity=:1000",
        b"nvx_identity=1000:",
        b"nvx_identity=1000:1000:1000",
        b"nvx_identity=0:1000",
        b"nvx_identity=1000:000",
        b"nvx_identity=4294967295:1000",
        b"nvx_identity=1000:4294967296",
        b"nvx_identity=+1000:1000",
        b"x\rnvx_identity=1000:1000",
        b"x\x0cnvx_identity=1000:1000",
    ] {
        assert_eq!(
            AgentConfig::layered(command_line, c"workload".to_owned(), c"/tmp".to_owned()).err(),
            Some(Errno::EINVAL),
            "{command_line:?}"
        );
    }
}

#[test]
fn the_agent_refuses_any_root_but_its_own() {
    let mut system = TestSystem::new(Paths::default());
    let arguments = [
        "nvx-managed-agent",
        "/dev/null",
        "/run/nvx/rootfs",
        "sandbox",
        "65534",
        "65534",
        "nobody",
        "/nonexistent",
    ]
    .map(|argument| CString::new(argument).unwrap());
    assert_eq!(run(&mut system, &arguments), Err(CommandError::Agent));
    assert!(system.diagnostics.is_empty());
    assert_eq!(system.tty_configured, 0);
    assert!(system.execs.is_empty());
}

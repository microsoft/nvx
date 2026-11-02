use super::seam::tests::TestSystem;
use super::*;
use crate::guest::entry::AccountMode;

#[cfg(feature = "host")]
/// In-process host/broker/agent integration harness.
mod loopback;

/// Serializes tests that temporarily alter process-global state.
pub(crate) static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs a process-global agent test in a dedicated test-harness subprocess.
///
/// Returns `true` in the parent after the isolated run succeeds and `false` in the child that
/// should execute the test body. Isolation prevents PID-1-style `waitpid(-1)` behavior from
/// reaping children that unrelated parallel host tests own.
pub(crate) fn isolate_process_test(marker: &str, test: &str) -> bool {
    if std::env::var_os(marker).is_some() {
        return false;
    }
    let status = std::process::Command::new(
        std::env::current_exe().expect("the test executable has a path"),
    )
    .args(["--exact", test, "--nocapture"])
    .env(marker, "1")
    .status()
    .expect("the isolated test process starts");
    assert!(status.success(), "the isolated test process failed");
    true
}

impl AgentConfig {
    /// Returns the configuration of a test agent of the workload `1000:1000`, named `workload`
    /// and homed in `/home/workload`: a direct agent, or a sandbox agent whose assembled root is
    /// `rootfs`.
    pub(crate) fn for_test(rootfs: Option<&std::path::Path>) -> Self {
        let rootfs = rootfs.map_or(c"-".to_owned(), |rootfs| {
            CString::new(rootfs.as_os_str().as_bytes()).expect("a test path has no NUL")
        });
        let direct = rootfs.as_bytes() == b"-";
        // The test system prepares each sandbox execution itself.
        let entry = (!direct).then(|| {
            let path = std::ffi::OsStr::from_bytes(rootfs.as_bytes());
            Entry::new(path, Identity::new(1000, 1000))
        });
        Self {
            direct,
            rootfs,
            hostname: c"test".to_owned(),
            uid: c"1000".to_owned(),
            gid: c"1000".to_owned(),
            user: c"workload".to_owned(),
            home: c"/home/workload".to_owned(),
            entry,
        }
    }
}

fn sandbox_config(hostname: &[u8], uid: &[u8]) -> AgentConfig {
    AgentConfig {
        rootfs: c"/run/nvx/rootfs".to_owned(),
        hostname: CString::new(hostname).unwrap(),
        uid: CString::new(uid).unwrap(),
        gid: c"65534".to_owned(),
        user: c"nobody".to_owned(),
        home: c"/nonexistent".to_owned(),
        direct: false,
        entry: None,
    }
}

#[test]
fn the_sandbox_entry_takes_an_existing_account_and_one_machine_id_per_boot() {
    let (_temporary, system) = TestSystem::with_container_cgroup();
    let config = sandbox_config(b"sandbox", b"65534");
    let entry = sandbox_entry(system.paths(), &config).unwrap();
    let identity = entry.identity();
    assert_eq!((identity.uid(), identity.gid()), (65534, 65534));
    assert_eq!(identity.account_mode(), AccountMode::Existing);
    assert!(identity.capability_set().is_empty());
    assert!(identity.supplementary_groups().is_empty());
    let machine_id = std::fs::read(&system.paths.machine_id).unwrap();
    assert_eq!(machine_id.len(), 33);
    assert!(
        machine_id[..32]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    );
    // Another agent of the same boot would find the machine ID taken.
    assert_eq!(
        sandbox_entry(system.paths(), &config).err(),
        Some(Errno::EEXIST)
    );
    assert_eq!(std::fs::read(&system.paths.machine_id).unwrap(), machine_id);
}

#[test]
fn only_a_guest_kvm_device_gives_the_sandbox_entry_dev_kvm() {
    let config = sandbox_config(b"sandbox", b"65534");
    // The default nodes of every entry.
    let all = Entry::new("/", Identity::new(1, 1)).device_set();
    for (device, expected) in [
        (None, all),
        (Some(std::path::Path::new("/dev/null")), all | 1 << 6),
    ] {
        let (temporary, mut system) = TestSystem::with_container_cgroup();
        let missing = temporary.path().join("kvm");
        system.paths.kvm_device = device.map_or(missing, ToOwned::to_owned);
        let entry = sandbox_entry(system.paths(), &config).unwrap();
        assert_eq!(entry.device_set(), expected);
    }
    // A file of another kind, or a link to a device, is no KVM device.
    let (temporary, mut system) = TestSystem::with_container_cgroup();
    let file = temporary.path().join("kvm");
    std::fs::write(&file, b"").unwrap();
    system.paths.kvm_device = file;
    assert_eq!(
        sandbox_entry(system.paths(), &config).unwrap().device_set(),
        all
    );
    let (temporary, mut system) = TestSystem::with_container_cgroup();
    let link = temporary.path().join("kvm");
    std::os::unix::fs::symlink("/dev/null", &link).unwrap();
    system.paths.kvm_device = link;
    assert_eq!(
        sandbox_entry(system.paths(), &config).unwrap().device_set(),
        all
    );
}

#[test]
fn a_refused_sandbox_entry_generates_nothing_and_stops_the_agent() {
    let (_temporary, mut system) = TestSystem::with_container_cgroup();
    for (hostname, uid) in [
        (&[b'h'; 65][..], b"65534".as_slice()),
        (b"", b"65534"),
        (b"sandbox", b"0"),
        (b"sandbox", b"4294967295"),
    ] {
        assert_eq!(
            sandbox_entry(system.paths(), &sandbox_config(hostname, uid)).err(),
            Some(Errno::EINVAL)
        );
        assert!(!system.paths.machine_id.exists());
    }
    let arguments = [
        "nvx-managed-agent",
        "/dev/null",
        "/run/nvx/rootfs",
        "sandbox",
        "0",
        "65534",
        "nobody",
        "/nonexistent",
    ]
    .map(|argument| CString::new(argument).unwrap());
    assert_eq!(run(&mut system, &arguments), Err(CommandError::Agent));
    assert_eq!(
        system.diagnostics,
        format!(
            "NVX-MANAGED-ERROR: stage=workload-entry status={}\n",
            Errno::EINVAL as i32
        )
        .as_bytes()
    );
    assert_eq!(system.tty_configured, 0);
    assert_eq!(system.runtime_binds, 0);
}

#[test]
fn a_failed_runtime_bind_stops_the_sandbox_agent_before_its_session() {
    let (_temporary, mut system) = TestSystem::with_container_cgroup();
    system.fail_runtime_bind = Some(Errno::EXDEV);
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
    assert_eq!(system.runtime_binds, 1);
    assert!(system.paths.machine_id.exists());
    assert_eq!(
        system.diagnostics,
        format!(
            "NVX-MANAGED-ERROR: stage=workload-entry status={}\n",
            Errno::EXDEV as i32
        )
        .as_bytes()
    );
    assert_eq!(system.tty_configured, 0);
}

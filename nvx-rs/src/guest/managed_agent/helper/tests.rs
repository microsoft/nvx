use crate::guest::sys::FileDescriptor;
use crate::protocol::{self, GuestExecPayload};

use super::super::seam::Paths;
use super::super::seam::tests::TestSystem;
use super::*;

fn execution(environment: Option<Vec<Vec<u8>>>, inherit: bool) -> GuestExecPayload {
    GuestExecPayload {
        timeout_ms: 0,
        argv: vec![b"/bin/true".to_vec()],
        cwd: None,
        environment,
        inherit_default_env: inherit,
    }
}

fn arguments(
    system: &mut TestSystem,
    descriptor: FileDescriptor,
    workload: &[&[u8]],
) -> Vec<CString> {
    let mut arguments = vec![
        c_string(b"nvx-managed-agent").unwrap(),
        c_string(b"--exec-config-fd").unwrap(),
        system.transfer_inherited_descriptor(descriptor),
        c_string(b"--").unwrap(),
    ];
    arguments.extend(workload.iter().map(|argument| c_string(argument).unwrap()));
    arguments
}

/// Returns the inherited environment.
fn inherited() -> Vec<Vec<u8>> {
    environment::inherited()
        .unwrap()
        .into_iter()
        .map(|entry| entry.into_bytes())
        .collect()
}

/// Returns the environment of the helper's only `exec`.
fn executed_environment(system: &TestSystem) -> Vec<Vec<u8>> {
    let [(_, _, environment)] = system.execs.as_slice() else {
        panic!("expected one exec, not {}", system.execs.len());
    };
    environment
        .clone()
        .expect("the helper passes an environment")
}

#[test]
fn helper_applies_exact_empty_and_layered_environments() {
    let fd = exec_config::create(&execution(Some(Vec::new()), false)).unwrap();
    let mut system = TestSystem::new(Paths::default());
    let helper_arguments = arguments(&mut system, fd, &[b"/usr/bin/env"]);
    assert_eq!(run(&mut system, &helper_arguments), Ok(()));
    assert!(executed_environment(&system).is_empty());

    let layered = vec![b"VALUE=layered".to_vec(), b"VALUE=again".to_vec()];
    let fd = exec_config::create(&execution(Some(layered), true)).unwrap();
    let mut system = TestSystem::new(Paths::default());
    let helper_arguments = arguments(&mut system, fd, &[b"/usr/bin/env"]);
    assert_eq!(run(&mut system, &helper_arguments), Ok(()));
    let mut expected: Vec<_> = inherited()
        .into_iter()
        .map(|entry| CString::new(entry).unwrap())
        .collect();
    environment::set(&mut expected, b"VALUE", b"again").unwrap();
    let expected: Vec<_> = expected.into_iter().map(CString::into_bytes).collect();
    assert_eq!(executed_environment(&system), expected);
}

#[test]
fn helper_rejects_bad_syntax_and_configurations_that_name_a_directory() {
    let mut system = TestSystem::new(Paths::default());
    assert_eq!(
        run(&mut system, &[c_string(b"agent").unwrap()]),
        Err(CommandError::Agent)
    );
    // The helper no longer enters a working directory, so a configuration that names one,
    // in its former layout, is malformed.
    let named = exec_config::tests::file_with(
        &[
            protocol::EXEC_CWD_PRESENT.to_le_bytes().as_slice(),
            &0u16.to_le_bytes(),
            &1u32.to_le_bytes(),
            b"/",
        ]
        .concat(),
    );
    let helper_arguments = arguments(&mut system, named, &[b"/bin/pwd"]);
    assert_eq!(
        run(&mut system, &helper_arguments),
        Err(CommandError::Agent)
    );
    assert!(system.execs.is_empty());
}

#[test]
fn helper_does_not_exec_when_an_environment_export_fails() {
    let fd = exec_config::create(&execution(Some(vec![b"VALUE=layered".to_vec()]), true)).unwrap();
    let mut system = TestSystem::new(Paths::default());
    system.fail_environment = Some(b"VALUE".to_vec());
    let helper_arguments = arguments(&mut system, fd, &[b"/bin/true"]);
    assert_eq!(
        run(&mut system, &helper_arguments),
        Err(CommandError::Agent)
    );
    assert!(system.execs.is_empty());
}

#[test]
fn helper_keeps_the_inherited_environment_without_entries() {
    let fd = exec_config::create(&execution(None, false)).unwrap();
    let mut system = TestSystem::new(Paths::default());
    let helper_arguments = arguments(&mut system, fd, &[b"/bin/pwd"]);
    assert_eq!(run(&mut system, &helper_arguments), Ok(()));
    assert_eq!(executed_environment(&system), inherited());
    assert_eq!(system.execs[0].1, [b"/bin/pwd".to_vec()]);
}

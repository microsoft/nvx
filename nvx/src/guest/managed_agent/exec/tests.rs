use super::super::seam::Paths;
use super::super::seam::tests::TestSystem;
use super::*;

fn config() -> AgentConfig {
    AgentConfig {
        uid: c"1000".to_owned(),
        gid: c"1000".to_owned(),
        user: c"workload".to_owned(),
        home: c"/home/workload".to_owned(),
        restored_proxy: None,
    }
}

fn execution(environment: Option<Vec<Vec<u8>>>) -> GuestExecPayload {
    GuestExecPayload {
        timeout_ms: 0,
        argv: vec![b"/bin/true".to_vec()],
        cwd: None,
        inherit_default_env: environment.is_some(),
        environment,
    }
}

fn validate(failing: &[u8], execution: &GuestExecPayload) -> Result<(), EnvironmentFailure> {
    let mut system = TestSystem::new(Paths::default());
    system.fail_environment = Some(failing.to_vec());
    let result = validate_launch_environment(&mut system, &config(), execution);
    assert!(system.execs.is_empty());
    result
}

#[test]
fn environment_failures_stop_direct_launches_before_exec() {
    for variable in [b"HOME".as_slice(), b"USER", b"LOGNAME"] {
        let failure = validate(variable, &execution(None)).unwrap_err();
        assert!(
            matches!(
                failure,
                EnvironmentFailure::Child(Errno::ENOMEM, ENVIRONMENT_DIAGNOSTIC)
            ),
            "{failure:?}"
        );
    }
    let failure = validate(b"PWD", &execution(None)).unwrap_err();
    assert!(matches!(failure, EnvironmentFailure::Launch(Errno::ENOMEM)));
    let layered = execution(Some(vec![b"LAYERED=value".to_vec()]));
    let failure = validate(b"LAYERED", &layered).unwrap_err();
    assert!(matches!(
        failure,
        EnvironmentFailure::Child(Errno::ENOMEM, ENVIRONMENT_DIAGNOSTIC)
    ));
    // An exact environment replaces the workload's `PWD`, so none is validated.
    let exact = GuestExecPayload {
        inherit_default_env: false,
        ..execution(Some(Vec::new()))
    };
    assert!(validate(b"PWD", &exact).is_ok());
}

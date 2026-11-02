use super::*;

/// Returns a descriptor of a file that holds `bytes`, from which [`read`] reads a configuration
/// as it reads a sealed one.
pub(crate) fn file_with(bytes: &[u8]) -> FileDescriptor {
    use std::io::{Seek, Write};

    let mut file = tempfile::tempfile().unwrap();
    file.write_all(bytes).unwrap();
    file.rewind().unwrap();
    FileDescriptor::from_owned(file.into())
}

fn execution(environment: Option<Vec<Vec<u8>>>) -> GuestExecPayload {
    GuestExecPayload {
        timeout_ms: 0,
        argv: vec![b"/bin/true".to_vec()],
        cwd: Some(b"/tmp".to_vec()),
        environment,
        inherit_default_env: false,
    }
}

#[test]
fn helper_configuration_round_trips_empty_exact_and_large_environments() {
    for environment in [
        Vec::new(),
        vec![b"EMPTY=".to_vec(), b"VALUE=space = value".to_vec()],
        (0..12)
            .map(|index| {
                let mut entry = format!("KEY{index}=").into_bytes();
                entry.extend(std::iter::repeat_n(b'x', 4000));
                entry
            })
            .collect(),
    ] {
        let descriptor = create(&execution(Some(environment.clone()))).unwrap();
        let decoded = read(&descriptor).unwrap();
        assert_eq!(decoded.environment, Some(environment));
        assert!(!decoded.inherit_default_env);
    }
}

#[test]
fn helper_configuration_is_sealed_and_omits_the_working_directory() {
    let descriptor = create(&execution(None)).unwrap();
    assert_eq!(descriptor.is_sealed_immutable(), Ok(true));
    let decoded = read(&descriptor).unwrap();
    assert_eq!(
        decoded,
        HelperConfig {
            environment: None,
            inherit_default_env: false,
        }
    );
}

#[test]
fn helper_defensively_rejects_unknown_flags_and_malformed_environment() {
    for bytes in [
        encode_fields(0x4000, &[]),
        encode_fields(0x0001, &[]),
        encode_fields(EXEC_INHERIT_DEFAULT_ENV, &[]),
        encode_fields(EXEC_ENVIRONMENT_PRESENT, &[b"NO_EQUALS".to_vec()]),
        encode_fields(EXEC_ENVIRONMENT_PRESENT, &[b"=value".to_vec()]),
    ]
    .map(Result::unwrap)
    {
        let descriptor = file_with(&bytes);
        assert!(read(&descriptor).is_err(), "{bytes:?}");
    }
}

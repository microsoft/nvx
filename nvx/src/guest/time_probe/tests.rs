use super::*;
use crate::guest::status::ExitStatus;
use crate::guest::status::tests::command_line;

/// Returns the command line of `nvx-time-probe` with `values`.
fn args(values: &[&[u8]]) -> Vec<CString> {
    command_line("nvx-time-probe", values)
}

#[test]
fn version_help_and_usage_are_byte_exact() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(run(&args(&[b"version"]), &mut out, &mut err), Ok(()));
    assert_eq!(out, b"nvx-time-probe 1.1\n");
    assert!(err.is_empty());

    out.clear();
    assert_eq!(run(&args(&[b"--help"]), &mut out, &mut err), Ok(()));
    assert_eq!(out, USAGE);

    err.clear();
    assert_eq!(
        run(&args(&[]), &mut out, &mut err),
        Err(CommandError::Usage)
    );
    assert_eq!(err, USAGE);
}

#[test]
fn the_shared_statuses_keep_the_probes_contract() {
    let cases = [
        (Ok(()), 0),
        (Err(CommandError::Operation), 1),
        (Err(CommandError::Usage), 2),
        (Err(CommandError::Setup), 3),
    ];
    for (result, status) in cases {
        assert_eq!(ExitStatus::from(result) as u8, status, "{result:?}");
    }
}

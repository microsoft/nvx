use super::*;

/// Returns the command line of `program` with `arguments`, as the C strings that [`run_main`]
/// passes a guest program's `run`: the program's name, then each argument. It panics on an
/// argument that holds a NUL byte, which no command line can.
pub(crate) fn command_line<A: AsRef<[u8]>>(program: &str, arguments: &[A]) -> Vec<CString> {
    std::iter::once(program.as_bytes())
        .chain(arguments.iter().map(AsRef::as_ref))
        .map(|argument| CString::new(argument).unwrap())
        .collect()
}

#[test]
fn failures_select_the_documented_statuses() {
    let cases = [
        (Ok(()), 0),
        (Err(CommandError::Operation), 1),
        (Err(CommandError::Usage), 2),
        (Err(CommandError::Setup), 3),
        (Err(CommandError::Transfer), 4),
        (Err(CommandError::Agent), 125),
        (Err(CommandError::Output), 255),
    ];
    for (result, code) in cases {
        assert_eq!(ExitStatus::from(result) as u8, code, "{result:?}");
    }
}

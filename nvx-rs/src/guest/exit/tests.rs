use std::ffi::CString;
#[cfg(target_arch = "x86_64")]
use std::fs;
#[cfg(target_arch = "x86_64")]
use std::os::unix::ffi::OsStrExt;

use super::*;
#[cfg(target_arch = "x86_64")]
use crate::guest::status::ExitStatus;
use crate::guest::status::tests::command_line;

/// Returns the command line of `nvx-exit` with `arguments`.
fn args(arguments: &[&[u8]]) -> Vec<CString> {
    command_line("nvx-exit", arguments)
}

#[test]
fn exit_code_takes_a_decimal_number_up_to_255() {
    let cases: &[(&[&[u8]], u8)] = &[
        (&[], 1),
        (&[b""], 1),
        (&[b"0"], 0),
        (&[b"37"], 37),
        (&[b"255"], 255),
        (&[b"256"], 1),
        (&[b"-1"], 1),
        (&[b"+5"], 1),
        (&[b" 7"], 1),
        (&[b"7 "], 1),
        (&[b"12a"], 1),
        (&[b"0x10"], 1),
        (&[b"99999999999999999999"], 1),
        (&[b"4294967296"], 1),
        (&[b"4294967551"], 1),
        (&[b"00000000000000000255"], 255),
        (&[b"010"], 10),
        (&[b"7", b"9"], 7),
    ];
    for &(arguments, expected) in cases {
        let args = args(arguments);
        let code = exit_code(args.get(1).map(|argument| argument.as_bytes()));
        assert_eq!(code, expected, "{arguments:?}");
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn writes_one_byte_at_the_control_port() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let port = CString::new(file.path().as_os_str().as_bytes()).unwrap();
    let status = ExitStatus::from(run(&args(&[b"37", b"extra"]), &port));
    assert_eq!(status, ExitStatus::Success);
    let control = Port::Control.offset() as usize;
    let mut expected = vec![0; control];
    expected.push(37);
    assert_eq!(fs::read(file.path()).unwrap(), expected);
    assert_eq!(
        ExitStatus::from(run(&args(&[]), &port)),
        ExitStatus::Success
    );
    expected[control] = 1;
    assert_eq!(fs::read(file.path()).unwrap(), expected);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn fails_when_it_cannot_open_or_write_the_device() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("port");
    let missing = CString::new(missing.as_os_str().as_bytes()).unwrap();
    for port in [missing.as_c_str(), c"/dev/full"] {
        let status = ExitStatus::from(run(&args(&[b"0"]), port));
        assert_eq!(status, ExitStatus::Failure, "{port:?}");
    }
}

use super::*;

#[test]
fn kernel_threads_are_known_by_their_flags() {
    let user = b"512 (sh) S 1 512 512 34816 512 4194560 147 0 0 0 0 0 0 0 20 0 1 0 72\n";
    let kernel = b"2 (kthreadd) S 0 0 0 0 -1 2129984 0 0 0 0 0 0 0 0 20 0 1 0 1\n";
    assert_eq!(is_kernel_thread(user), Some(false));
    assert_eq!(is_kernel_thread(kernel), Some(true));
    // A command with a parenthesis and spaces of its own.
    let tricky = b"77 (a) b (c)) S 1 77 77 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 9\n";
    assert_eq!(is_kernel_thread(tricky), Some(false));
    for malformed in [
        &b""[..],
        b"77 (sh S 1 77 77 0 -1 4194560",
        b"77 (sh) S 1 77 77 0 -1",
        b"77 (sh) S 1 77 77 0 -1 0x10 0",
    ] {
        assert_eq!(
            is_kernel_thread(malformed),
            None,
            "{}",
            malformed.escape_ascii()
        );
    }
}

#[test]
fn membership_lists_are_decimal_lines() {
    assert_eq!(processes(b"").unwrap(), Vec::<u32>::new());
    assert_eq!(processes(b"1\n42\n4194304\n").unwrap(), [1, 42, 4_194_304]);
    for malformed in [
        &b"1"[..],
        b"\n",
        b"1\n\n",
        b"0\n",
        b"x\n",
        b"-1\n",
        b"4294967296\n",
    ] {
        assert_eq!(
            processes(malformed).unwrap_err().raw_os_error(),
            Some(Errno::EPROTO as i32),
            "{}",
            malformed.escape_ascii()
        );
    }
}

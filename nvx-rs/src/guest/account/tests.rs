//! Tests of the account database parsers shared by rooted account readers.

use super::*;

/// The passwd database of the tests' roots.
pub(crate) const PASSWD_DATABASE: &[u8] = b"root:x:0:0:root:/root:/bin/sh\n\
# A comment names no account.\n\
\n\
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n\
workload:x:1000:1001::/home/workload:/bin/sh\n";
/// The group database of the tests' roots.
pub(crate) const GROUP_DATABASE: &[u8] = b"root:x:0:\nworkload:x:1001:\nnogroup:x:65534:\n";

#[test]
fn passwd_entries_are_parsed_and_checked() {
    let account = passwd_account(PASSWD_DATABASE, 65534, 65534).unwrap();
    assert_eq!(
        (account.name(), account.home()),
        (b"nobody".as_slice(), b"/nonexistent".as_slice())
    );
    let account = passwd_account(PASSWD_DATABASE, 1000, 1001).unwrap();
    assert_eq!(account.home(), b"/home/workload");
    assert!(matches!(
        passwd_account(PASSWD_DATABASE, 1001, 1001),
        Err(AccountError::Missing)
    ));
    assert!(matches!(
        passwd_account(b"", 0, 0),
        Err(AccountError::Missing)
    ));
    let duplicate = [PASSWD_DATABASE, b"other:x:1000:1001::/home/other:/bin/sh\n"].concat();
    assert!(matches!(
        passwd_account(&duplicate, 1000, 1001),
        Err(AccountError::Duplicate)
    ));
    // Leading zeros are decimal, as the shell init's comparison read them.
    let padded = b"padded:x:0065534:65534::/home/padded:/bin/sh";
    assert!(passwd_account(padded, 65534, 65534).is_ok());
    assert!(matches!(
        passwd_account(PASSWD_DATABASE, 1000, 1000),
        Err(AccountError::PrimaryGroup(1001))
    ));
    for malformed in [
        b"short:x:1000:1001::/home/short".as_slice(),
        b"long:x:1000:1001::/home/long:/bin/sh:extra",
        b":x:1000:1001::/home/nameless:/bin/sh",
        b"group:x:1000:not-a-number::/home/group:/bin/sh",
        b"group:x:1000:-1::/home/group:/bin/sh",
        b"group:x:1000:4294967296::/home/group:/bin/sh",
    ] {
        assert!(
            matches!(
                passwd_account(malformed, 1000, 1001),
                Err(AccountError::Malformed)
            ),
            "{}",
            String::from_utf8_lossy(malformed)
        );
    }
    for home in [b"".as_slice(), b"relative/home", b"~"] {
        let line = [b"workload:x:1000:1001::".as_slice(), home, b":/bin/sh"].concat();
        assert!(matches!(
            passwd_account(&line, 1000, 1001),
            Err(AccountError::Home)
        ));
    }
    // Blank lines, comments, and lines whose third field is no ID name no account.
    assert!(matches!(
        passwd_account(
            b"# 1000:1000:1000\n  #x:x:1000:1000::/home/x:/bin/sh\n\n+::::::\n",
            1000,
            1000
        ),
        Err(AccountError::Missing)
    ));
}

#[test]
fn group_entries_are_found_by_id() {
    assert!(has_group(GROUP_DATABASE, 1001));
    assert!(has_group(GROUP_DATABASE, 65534));
    assert!(!has_group(GROUP_DATABASE, 1000));
    assert!(!has_group(b"# x:x:1000:\n\n", 1000));
}

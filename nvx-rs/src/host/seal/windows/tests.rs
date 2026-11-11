use super::*;

#[test]
fn file_times_count_from_the_unix_epoch() {
    let at = |ticks: i128| {
        time(FILETIME {
            dwLowDateTime: (ticks & 0xffff_ffff) as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        })
    };
    assert_eq!(
        at(UNIX_EPOCH_TICKS),
        SealTime {
            seconds: 0,
            nanoseconds: 0
        }
    );
    assert_eq!(
        at(UNIX_EPOCH_TICKS + 15 * TICKS_PER_SECOND + 1),
        SealTime {
            seconds: 15,
            nanoseconds: 100
        }
    );
    // Times before 1970 count back from it, with nanoseconds forward within their second.
    assert_eq!(
        at(UNIX_EPOCH_TICKS - 1),
        SealTime {
            seconds: -1,
            nanoseconds: 999_999_900
        }
    );
    assert_eq!(
        at(0),
        SealTime {
            seconds: -11_644_473_600,
            nanoseconds: 0
        }
    );
}

#[test]
fn final_paths_hold_no_nul_whatever_their_length() {
    use std::os::windows::ffi::OsStrExt;

    // A path shorter than the first buffer, and one longer, which takes a second call with
    // the length, terminator included, that the first one asked for.
    let directory = tempfile::tempdir().unwrap();
    let mut deep = directory.path().to_owned();
    while deep.as_os_str().len() < 300 {
        deep.push("a-directory-that-lengthens-the-final-path");
    }
    std::fs::create_dir_all(&deep).unwrap();
    for path in [directory.path().join("short"), deep.join("long")] {
        std::fs::write(&path, b"").unwrap();
        let file = File::open(&path).unwrap();
        let found = final_path(&file).unwrap();
        assert!(!found.as_os_str().encode_wide().any(|unit| unit == 0));
        assert_eq!(found, std::fs::canonicalize(&path).unwrap());
    }
    let file = File::open(deep.join("long")).unwrap();
    assert!(final_path(&file).unwrap().as_os_str().len() > 260);
}

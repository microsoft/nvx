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

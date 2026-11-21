#[cfg(target_os = "linux")]
pub(crate) use super::process::tests::{
    clear_close_on_exec, mark_listed_descriptors_on_spawn, refuse_peer_pidfd_on_spawn,
    refuse_umask_on_spawn,
};

#[cfg(target_os = "linux")]
#[test]
fn poll_timeouts_round_up_and_saturate_without_becoming_infinite() {
    use std::time::Duration;

    use nix::poll::PollTimeout;

    for (duration, expected) in [
        (None, PollTimeout::NONE),
        (Some(Duration::ZERO), PollTimeout::ZERO),
        (Some(Duration::from_nanos(1)), PollTimeout::from(1u8)),
        (Some(Duration::from_millis(1)), PollTimeout::from(1u8)),
        (
            Some(Duration::from_nanos(1_000_001)),
            PollTimeout::from(2u8),
        ),
        (Some(Duration::from_millis(2_147_483_647)), PollTimeout::MAX),
        (
            Some(Duration::from_nanos(2_147_483_647_000_001)),
            PollTimeout::MAX,
        ),
        (Some(Duration::from_millis(2_147_483_648)), PollTimeout::MAX),
        (Some(Duration::MAX), PollTimeout::MAX),
    ] {
        assert_eq!(super::poll_timeout(duration), expected, "{duration:?}");
    }
}

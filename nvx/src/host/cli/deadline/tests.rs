use super::*;

#[test]
fn deadlines_pass_after_their_seconds() {
    assert!(!Deadline::after(60.0).passed());
    assert!(Deadline::after(1e-300).passed());
    let unbounded = Deadline::after(1e300);
    assert_eq!(unbounded.remaining(), None);
    assert!(!unbounded.passed());
    assert!(unbounded.instant() > Instant::now());
    let short = Deadline::after(0.01);
    thread::sleep(Duration::from_millis(20));
    assert!(!short.pause(RETRY_INTERVAL).unwrap());
    assert_eq!(short.remaining(), Some(Duration::ZERO));
}

#[test]
fn slices_take_the_time_left_up_to_their_interval() {
    assert_eq!(Deadline::after(60.0).slice(RETRY_INTERVAL), RETRY_INTERVAL);
    let hour = Duration::from_secs(3600);
    let deadline = Deadline::after(60.0);
    let before = deadline.remaining().unwrap();
    let left = deadline.slice(hour);
    let after = deadline.remaining().unwrap();
    assert!(
        after <= left && left <= before,
        "{after:?} {left:?} {before:?}"
    );
    assert_eq!(
        Deadline::after(1e-300).slice(RETRY_INTERVAL),
        Duration::ZERO
    );
    assert_eq!(Deadline::after(1e300).slice(hour), hour);
}

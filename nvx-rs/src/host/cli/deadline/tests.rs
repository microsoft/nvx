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

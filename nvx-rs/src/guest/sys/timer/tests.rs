//! Checks of the timer's expiration.

use super::*;
use crate::guest::sys::{Pollable, poll};

#[test]
fn an_armed_timer_expires_once() {
    let timer = Timer::new().unwrap();
    let idle = poll([Pollable::Timer(&timer)], Some(Duration::ZERO)).unwrap();
    assert!(!idle[0].readable);
    timer.arm_after(Duration::from_millis(1)).unwrap();
    let expired = poll([Pollable::Timer(&timer)], Some(Duration::from_secs(5))).unwrap();
    assert!(expired[0].readable);
    timer.consume();
    let consumed = poll([Pollable::Timer(&timer)], Some(Duration::ZERO)).unwrap();
    assert!(!consumed[0].readable);
}

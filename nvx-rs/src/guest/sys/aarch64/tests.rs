use super::*;

#[test]
fn linux_el0_counter_reads_do_not_invent_a_frequency() {
    let frequency = counter_frequency_hz().unwrap();
    assert!(frequency > 0 && frequency <= u64::from(u32::MAX));
    let mut previous = virtual_counter();
    for _ in 0..32 {
        let next = virtual_counter();
        assert!(next >= previous);
        previous = next;
    }
}

use super::*;
use crate::guest::kernel_text::{TickDevice, TimerBase};

#[test]
fn online_capacity_checks_neither_guess_cpu_numbers_nor_ignore_duplicates() {
    assert_eq!(online_cpus(b"0-3\n", 4).unwrap(), [0, 1, 2, 3]);
    for text in [b"0-2".as_slice(), b"0,0,1,2", b"1-4", b"0-3,4", b"bad"] {
        assert!(online_cpus(text, 4).is_err(), "{text:?}");
    }
}

#[test]
fn every_cpu_needs_the_standard_architectural_high_resolution_timer() {
    let mut list = TimerList::default();
    for cpu in 0..2 {
        list.tick_devices.insert(
            cpu,
            TickDevice {
                mode: Some(1),
                device: Some("arch_sys_timer".into()),
                state: Some(3),
                handler: Some("hrtimer_interrupt".into()),
            },
        );
        list.timer_bases.insert(
            cpu,
            TimerBase {
                hres_active: Some(1),
                nohz: Some(1),
            },
        );
    }
    assert!(check_timer_list(&list, 2).is_ok());
    list.tick_devices.get_mut(&1).unwrap().state = Some(4);
    assert!(check_timer_list(&list, 2).is_ok());
    list.tick_devices.get_mut(&1).unwrap().device = Some("unrelated_timer".into());
    assert!(check_timer_list(&list, 2).is_err());
    list.tick_devices.remove(&1);
    assert!(check_timer_list(&list, 2).is_err());
}

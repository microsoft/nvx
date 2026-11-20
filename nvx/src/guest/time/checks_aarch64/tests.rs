use super::*;
use crate::guest::counter::tests::arm_context;

#[test]
fn cpu_inventory_needs_the_explicit_arm_binding_and_its_actual_capacity() {
    let binding = arm_context();
    let cpu0 = CpuMask::parse(b"0").unwrap();
    assert_eq!(inventory_verdict(1, cpu0, &binding), Ok(()));
    for (possible, online) in [
        (2, cpu0),
        (1, CpuMask::EMPTY),
        (1, CpuMask::parse(b"1").unwrap()),
    ] {
        assert_eq!(
            inventory_verdict(possible, online, &binding)
                .unwrap_err()
                .id,
            CheckId::C6
        );
    }
    let x86 = ClockContext::x86(19_200_001, 200_000_000).unwrap();
    assert_eq!(
        inventory_verdict(1, cpu0, &x86).unwrap_err().id,
        CheckId::C3
    );
}

#[test]
fn the_effective_clocksource_must_be_the_architectural_counter() {
    assert_eq!(clocksource_verdict(b"arch_sys_counter\n"), Ok(()));
    for source in [b"tsc".as_slice(), b"jiffies", b""] {
        assert_eq!(clocksource_verdict(source).unwrap_err().id, CheckId::C6);
    }
}

#[test]
fn arm_command_lines_refuse_x86_clock_overrides_instead_of_ignoring_their_isa() {
    assert_eq!(
        cmdline_verdict(b"quiet clocksource=arch_sys_counter nvx_time_abi=enforce\n"),
        Ok(())
    );
    for text in [
        b"clocksource=tsc".as_slice(),
        b"tsc=reliable",
        b"nolapic",
        b"lapic-timer-hz=1000",
        b"tsc_early_khz=\"19200\"",
        b"hpet=force",
        b"notsc",
    ] {
        let failure = cmdline_verdict(text).unwrap_err();
        assert_eq!(failure.id, CheckId::C9);
        assert!(failure.detail.starts_with(b"command line contains "));
    }
}

#[test]
fn every_online_cpu_needs_an_active_oneshot_architectural_timer() {
    let text = "\
cpu: 0
 .hres_active : 1
Tick Device: mode: 1
Per CPU device: 0
Clock Event Device: arch_sys_timer
 mode: 3
";
    let cpu0 = CpuMask::parse(b"0").unwrap();
    assert_eq!(
        timer_verdict(&TimerList::parse(text.as_bytes()), cpu0),
        Ok(())
    );
    for changed in [
        text.replace("arch_sys_timer", "lapic"),
        text.replace("mode: 3", "mode: 2"),
        text.replace("hres_active : 1", "hres_active : 0"),
        String::new(),
    ] {
        assert_eq!(
            timer_verdict(&TimerList::parse(changed.as_bytes()), cpu0)
                .unwrap_err()
                .id,
            CheckId::C7
        );
    }
    assert!(
        timer_verdict(
            &TimerList::parse(text.as_bytes()),
            CpuMask::parse(b"0-1").unwrap()
        )
        .is_err()
    );
    assert!(timer_verdict(&TimerList::parse(text.as_bytes()), CpuMask::EMPTY).is_err());
}

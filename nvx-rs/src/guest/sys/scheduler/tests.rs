//! Checks of affinity and scheduling in this test's thread.

use super::*;

#[test]
fn pinning_moves_the_thread_and_the_guard_restores_its_mask() {
    let original = current_affinity().unwrap();
    let cpus = original.cpus();
    assert!(!cpus.is_empty());
    {
        let _guard = AffinityGuard::save().unwrap();
        pin_current_thread(cpus[0]).unwrap();
        assert_eq!(current_cpu().unwrap(), cpus[0]);
        assert_eq!(current_affinity().unwrap().cpus(), [cpus[0]]);
    }
    assert_eq!(current_affinity().unwrap().cpus(), cpus);
    assert_eq!(pin_current_thread(CpuSet::count()), Err(Errno::EINVAL));
}

#[test]
fn the_idle_priority_lasts_while_its_owner_lives() {
    let idle = IdlePriority::enter();
    let idle_policy = Ok(SchedulingPolicy::Idle.number());
    assert_eq!(scheduling_policy(None), idle_policy);
    assert_eq!(scheduling_policy(Some(current_thread())), idle_policy);
    drop(idle);
    let policy = scheduling_policy(None);
    // Leaving SCHED_IDLE needs CAP_SYS_NICE or an RLIMIT_NICE of at least 20.
    assert!(
        policy == Ok(SchedulingPolicy::Normal.number()) || policy == idle_policy,
        "{policy:?}"
    );
    assert!(thread_count().unwrap() >= 1);
}

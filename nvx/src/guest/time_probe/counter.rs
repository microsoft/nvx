//! ISA-specific timestamp instructions; the pairwise algorithms stay shared.

#[cfg(target_arch = "x86_64")]
pub(super) use crate::guest::sys::x86::{
    pause as spin_hint, rdtsc as ticks, rdtsc_fenced as ordered_ticks,
    rdtsc_lfence as load_ordered_ticks,
};

#[cfg(target_arch = "aarch64")]
pub(super) use crate::guest::sys::aarch64::{
    counter_ticks_ordered as load_ordered_ticks, counter_ticks_ordered as ordered_ticks,
    counter_ticks_ordered as ticks,
};
#[cfg(target_arch = "aarch64")]
pub(super) use std::hint::spin_loop as spin_hint;

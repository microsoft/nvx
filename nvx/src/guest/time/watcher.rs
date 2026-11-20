//! Kernel-message policy used by the runtime watcher and x86 boot-log checks.

use super::events::ViolationCode;
use crate::guest::kernel_text;

/// The prefix of the time tools' own kernel-log records, which the watcher ignores.
pub(super) const OWN_RECORD_PREFIX: &[u8] = b"NVX-TIME-";

/// Ordered kernel-message fragments that identify one violation.
struct WatchRule {
    /// Stable event reported when this rule is the first match.
    code: ViolationCode,
    /// Fragments all required in the message.
    all: &'static [&'static str],
    /// At least one required alternative, or no additional condition when empty.
    any: &'static [&'static str],
}

/// Existing x86 rule order with only applicable, architecture-neutral rules on ARM.
const WATCH_RULES: &[WatchRule] = &[
    #[cfg(target_arch = "x86_64")]
    WatchRule {
        code: ViolationCode::TscUnstable,
        all: &["Marking TSC unstable due to "],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::ClocksourceUnstable,
        all: &[
            "timekeeping watchdog on CPU",
            "as unstable because the skew is too large",
        ],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::ClocksourceSwitch,
        all: &["clocksource: Switched to clocksource "],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::ClocksourceSkew,
        all: &["clocksource: "],
        any: &[" ahead of CPU ", " behind CPU "],
    },
    #[cfg(target_arch = "x86_64")]
    WatchRule {
        code: ViolationCode::TscWarp,
        all: &[],
        any: &[
            "TSC synchronization [CPU#",
            "cycles TSC warp between CPUs",
            "TSC warped randomly between CPUs",
        ],
    },
    #[cfg(target_arch = "x86_64")]
    WatchRule {
        code: ViolationCode::TscAdjust,
        all: &["TSC ADJUST"],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::RcuStall,
        all: &["rcu: INFO: "],
        any: &[
            " detected stalls on CPUs/tasks",
            " self-detected stall on CPU",
            " detected expedited stalls",
        ],
    },
    WatchRule {
        code: ViolationCode::RcuStarved,
        all: &["rcu: "],
        any: &[
            " kthread starved for ",
            " kthread timer wakeup didn't happen for ",
        ],
    },
    WatchRule {
        code: ViolationCode::SoftLockup,
        all: &["watchdog: BUG: soft lockup - CPU#"],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::HardLockup,
        all: &["Watchdog detected hard LOCKUP"],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::HungTask,
        all: &["INFO: task ", " blocked for more than "],
        any: &[],
    },
    #[cfg(target_arch = "x86_64")]
    WatchRule {
        code: ViolationCode::UncheckedMsr,
        all: &["unchecked MSR access error"],
        any: &[],
    },
    #[cfg(target_arch = "x86_64")]
    WatchRule {
        code: ViolationCode::ApicIdMismatch,
        all: &["APIC ID mismatch"],
        any: &[],
    },
];

/// Returns the first applicable violation, excluding the time tools' own records.
pub(crate) fn match_watch_rule(message: &[u8]) -> Option<ViolationCode> {
    if message.starts_with(OWN_RECORD_PREFIX) {
        return None;
    }
    let contains = |part: &&str| kernel_text::contains(message, part.as_bytes());
    WATCH_RULES
        .iter()
        .find(|rule| {
            rule.all.iter().all(contains) && (rule.any.is_empty() || rule.any.iter().any(contains))
        })
        .map(|rule| rule.code)
}

#[cfg(test)]
mod tests;

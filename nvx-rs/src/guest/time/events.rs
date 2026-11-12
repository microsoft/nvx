//! Shared time-ABI violation events, their codes, watcher rules and byte-exact escaping.

use std::fmt::{self, Display, Write as _};

use super::sample::Phase;
use crate::guest::kernel_text;

/// Maximum event record length before its terminating newline.
pub(crate) const EVENT_MAX: usize = 512;
/// Maximum detail buffer for x86 exhaustive-check diagnostics.
#[cfg(target_arch = "x86_64")]
pub(crate) const DETAIL_MAX: usize = 384;

/// The conformance checks, whose failures the time ABI reports as `G_CONFORMANCE_Cn`.
///
/// Keep the complete wire-v1 namespace on every ISA, even when its cold-boot checks emit only
/// a subset of the IDs. The shared formatter and its tests retain the other IDs too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(all(not(target_arch = "x86_64"), not(test)), allow(dead_code))]
pub(crate) enum CheckId {
    /// The Hyper-V identity leaves, on every CPU that the checks run on.
    C1,
    /// The time feature bits and leaves of CPUID.
    C2,
    /// The Hyper-V time MSRs.
    C3,
    /// The boot log.
    C4,
    /// `/proc/cpuinfo`.
    C5,
    /// The CPU sets and the clocksources.
    C6,
    /// The clock-event devices of `/proc/timer_list`.
    C7,
    /// The platform: no VMBus and no frequency scaling, and RCU's stall settings.
    C8,
    /// The kernel command line.
    C9,
    /// The time daemon and the published state.
    C10,
    /// The soft-lockup and hung-task detectors.
    C11,
    /// The initial host time sample.
    C12,
}

impl Display for CheckId {
    /// Writes the check's name, such as `C3`, which is its variant's.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

/// The codes of the time ABI's violation events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViolationCode {
    /// A conformance check failed.
    Conformance(CheckId),
    /// The boot log reports an insecure writable and executable mapping.
    KernelWx,
    /// The kernel overwrote log records before the watcher read them.
    KmsgOverrun,
    /// The kernel marked the TSC unstable.
    TscUnstable,
    /// The clocksource watchdog marked a clocksource unstable.
    ClocksourceUnstable,
    /// The kernel switched clocksources.
    ClocksourceSwitch,
    /// The clocksource watchdog measured a skew between CPUs.
    ClocksourceSkew,
    /// The kernel found the TSCs of two CPUs out of step.
    TscWarp,
    /// The kernel found or corrected an `IA32_TSC_ADJUST` value.
    TscAdjust,
    /// RCU reported a CPU stall.
    RcuStall,
    /// An RCU kthread starved.
    RcuStarved,
    /// The soft-lockup detector fired.
    SoftLockup,
    /// The hard-lockup detector fired.
    HardLockup,
    /// The hung-task detector fired.
    HungTask,
    /// The kernel accessed an MSR that faulted.
    UncheckedMsr,
    /// A CPU's APIC ID differs from the firmware's.
    ApicIdMismatch,
    /// Restore repair could not correct the wall clock.
    /// Kept as wire-v1 event vocabulary on cold-boot-only guests, not as restore support.
    #[cfg_attr(all(not(target_arch = "x86_64"), not(test)), allow(dead_code))]
    RepairClock,
    /// Restore repair found the wrong VM generation.
    #[cfg_attr(all(not(target_arch = "x86_64"), not(test)), allow(dead_code))]
    RepairGeneration,
    /// Restore repair read a malformed restore packet.
    #[cfg_attr(all(not(target_arch = "x86_64"), not(test)), allow(dead_code))]
    RepairPacket,
    /// Restore repair could not take a narrow enough time sample.
    #[cfg_attr(all(not(target_arch = "x86_64"), not(test)), allow(dead_code))]
    RepairSample,
    /// Restore repair could not restore the stall detectors.
    #[cfg_attr(all(not(target_arch = "x86_64"), not(test)), allow(dead_code))]
    RepairSuppression,
}

impl Display for ViolationCode {
    /// Writes the code's name, such as `G_HUNG_TASK`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Conformance(check) => return write!(formatter, "G_CONFORMANCE_{check}"),
            Self::KernelWx => "G_KERNEL_WX",
            Self::KmsgOverrun => "G_KMSG_OVERRUN",
            Self::TscUnstable => "G_TSC_UNSTABLE",
            Self::ClocksourceUnstable => "G_CLOCKSOURCE_UNSTABLE",
            Self::ClocksourceSwitch => "G_CLOCKSOURCE_SWITCH",
            Self::ClocksourceSkew => "G_CLOCKSOURCE_SKEW",
            Self::TscWarp => "G_TSC_WARP",
            Self::TscAdjust => "G_TSC_ADJUST",
            Self::RcuStall => "G_RCU_STALL",
            Self::RcuStarved => "G_RCU_STARVED",
            Self::SoftLockup => "G_SOFT_LOCKUP",
            Self::HardLockup => "G_HARD_LOCKUP",
            Self::HungTask => "G_HUNG_TASK",
            Self::UncheckedMsr => "G_UNCHECKED_MSR",
            Self::ApicIdMismatch => "G_APIC_ID_MISMATCH",
            Self::RepairClock => "G_REPAIR_CLOCK",
            Self::RepairGeneration => "G_REPAIR_GENERATION",
            Self::RepairPacket => "G_REPAIR_PACKET",
            Self::RepairSample => "G_REPAIR_SAMPLE",
            Self::RepairSuppression => "G_REPAIR_SUPPRESSION",
        };
        formatter.write_str(name)
    }
}

/// The part of the time tools that reports a violation event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventSource {
    /// A conformance check.
    Conformance,
    /// The kernel-log and discipline watcher.
    Watcher,
    /// Restore repair.
    /// Kept as wire-v1 event vocabulary on cold-boot-only guests, not as restore support.
    #[cfg_attr(all(not(target_arch = "x86_64"), not(test)), allow(dead_code))]
    Repair,
}

impl Display for EventSource {
    /// Writes the source's name, such as `watcher`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Conformance => "conformance",
            Self::Watcher => "watcher",
            Self::Repair => "repair",
        })
    }
}

/// Escapes a diagnostic as the time ABI does, with at most `limit` bytes and without splitting
/// an escape: printable ASCII stays, except `"` and `\`, which a backslash escapes, and every
/// other byte becomes `\xNN`.
pub(crate) fn escape_text(text: &[u8], limit: usize) -> String {
    let mut escaped = String::with_capacity(text.len().min(limit));
    for &byte in text {
        let start = escaped.len();
        match byte {
            b'"' | b'\\' => {
                escaped.push('\\');
                escaped.push(char::from(byte));
            }
            0x20..=0x7e => escaped.push(char::from(byte)),
            _ => {
                let _ = write!(escaped, "\\x{byte:02x}");
            }
        }
        if escaped.len() > limit {
            escaped.truncate(start);
            break;
        }
    }
    escaped
}

/// Formats one conformance or report-only violation event, at most `EVENT_MAX` bytes long
/// before its newline.
pub(crate) fn format_event(
    report_only: bool,
    code: impl Display,
    source: impl Display,
    phase: Phase,
    generation: u32,
    boottime_ns: i64,
    detail: &[u8],
) -> String {
    let prefix = if report_only {
        "NVX-TIME-REPORT"
    } else {
        "NVX-TIME-ABI"
    };
    let mut line = format!(
        "{prefix}-VIOLATION: v=1 code={code} source={source} phase={} generation={generation} \
         boottime_ns={boottime_ns} detail=\"",
        phase.name()
    );
    let available = EVENT_MAX.saturating_sub(line.len() + 2);
    line.push_str(&escape_text(detail, available));
    line.push_str("\"\n");
    line
}

/// Kernel-message signature associated with one existing violation code.
struct WatchRule {
    /// Stable ABI violation code.
    code: ViolationCode,
    /// Substrings that must all occur.
    all: &'static [&'static str],
    /// Alternatives of which at least one must occur.
    any: &'static [&'static str],
}

/// Existing ordered watcher rules, shared rather than duplicated for the ARM daemon.
const WATCH_RULES: &[WatchRule] = &[
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
    WatchRule {
        code: ViolationCode::TscWarp,
        all: &[],
        any: &[
            "TSC synchronization [CPU#",
            "cycles TSC warp between CPUs",
            "TSC warped randomly between CPUs",
        ],
    },
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
    WatchRule {
        code: ViolationCode::UncheckedMsr,
        all: &["unchecked MSR access error"],
        any: &[],
    },
    WatchRule {
        code: ViolationCode::ApicIdMismatch,
        all: &["APIC ID mismatch"],
        any: &[],
    },
];

/// Returns the existing first matching violation, excluding the time tools' own records.
pub(crate) fn match_watch_rule(message: &[u8]) -> Option<ViolationCode> {
    if message.starts_with(b"NVX-TIME-") {
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

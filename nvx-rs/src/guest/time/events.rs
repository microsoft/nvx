//! Time-ABI violation events, their codes, and their byte-exact escaping.

use std::fmt::{self, Display, Write as _};

use super::Phase;

/// Maximum event record length before its terminating newline.
pub(crate) const EVENT_MAX: usize = 512;
/// Maximum unescaped detail bytes considered for one event.
pub(crate) const DETAIL_MAX: usize = 384;

/// The conformance checks, whose failures the time ABI reports as `G_CONFORMANCE_Cn`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    RepairClock,
    /// Restore repair found the wrong VM generation.
    RepairGeneration,
    /// Restore repair read a malformed restore packet.
    RepairPacket,
    /// Restore repair could not take a narrow enough time sample.
    RepairSample,
    /// Restore repair could not restore the stall detectors.
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

#[cfg(test)]
mod tests;

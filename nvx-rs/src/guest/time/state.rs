//! The shared, unchanged `/run/nvx/time/state` representation and atomic publication.

use std::fmt::{self, Write as _};
use std::str::FromStr;

use super::sample::{self as decoders, BestSample, Phase};
use crate::guest::sys::{self, CreatedFile, Errno, Mode};

/// Existing published time-state path, shared by both counter architectures.
pub(crate) const STATE_PATH: &std::ffi::CStr = c"/run/nvx/time/state";
/// Existing temporary file for an atomic state replacement.
pub(crate) const STATE_TEMP_PATH: &std::ffi::CStr = c"/run/nvx/time/.state";

/// Defines an enumeration of the tokens of one state-file field, with each token's spelling.
macro_rules! tokens {
    (
        $(#[$meta:meta])*
        $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $token:literal,)+
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum $name {
            $($(#[$variant_meta])* $variant,)+
        }

        impl $name {
            /// Returns the token that `text` spells.
            fn parse(text: &str) -> Option<Self> {
                match text {
                    $($token => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl fmt::Display for $name {
            /// Writes the token's spelling.
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(match self {
                    $(Self::$variant => $token,)+
                })
            }
        }
    };
}

tokens! {
    /// The outcome of one phase's conformance checks.
    CheckStatus {
        /// The checks have not finished.
        Pending => "pending",
        /// The checks passed.
        Ok => "ok",
        /// A check failed in report-only mode, which records failures.
        Fail => "fail",
    }
}

tokens! {
    /// The latest discontinuity of the wall clock.
    #[derive(Default)]
    Discontinuity {
        /// None since boot.
        #[default]
        None => "none",
        /// The discipline stepped the clock.
        Step => "step",
        /// A restore's repair stepped the clock.
        Restore => "restore",
    }
}

tokens! {
    /// The host clock that measured the latest downtime.
    #[derive(Default)]
    DowntimeSource {
        /// No downtime since boot.
        #[default]
        None => "none",
        /// The host's UTC clock.
        Utc => "utc",
        /// The host's monotonic clock.
        Monotonic => "monotonic",
    }
}

tokens! {
    /// Why the discipline rejected its latest time sample.
    #[derive(Default)]
    SampleRejection {
        /// The discipline accepted the latest sample.
        #[default]
        None => "none",
        /// No sample was narrow enough.
        Uncertain => "G_SAMPLE_UNCERTAIN",
        /// A sample's guest TSC lay outside the guest's reads of its TSC around the sample's
        /// latch, by more than the sample's pairing uncertainty allows.
        Tsc => "G_SAMPLE_TSC",
        /// No attempt took a valid sample, or the discipline could not apply one.
        Invalid => "G_SAMPLE_INVALID",
    }
}

tokens! {
    /// The status of host UTC that the latest accepted time sample reported.
    #[derive(Default)]
    HostClockStatus {
        /// The host does not report its clock's status.
        #[default]
        Unknown => "unknown",
        /// A time service synchronizes host UTC with a reference.
        Synchronized => "synchronized",
        /// No time service synchronizes host UTC, which runs on its own.
        FreeRunning => "free-running",
        /// The host reports a fault of its clock.
        Unreliable => "unreliable",
    }
}

impl HostClockStatus {
    /// Returns the status that a time sample's status byte encodes, if any.
    pub(crate) const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Unknown),
            1 => Some(Self::Synchronized),
            2 => Some(Self::FreeRunning),
            3 => Some(Self::Unreliable),
            _ => None,
        }
    }
}

/// Writes an optional bound of the state file: its value, or `unknown`.
struct Bound(Option<u64>);

impl fmt::Display for Bound {
    /// Writes the value, or `unknown` for none.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(value) => write!(formatter, "{value}"),
            None => formatter.write_str("unknown"),
        }
    }
}

/// Persisted outcome and accounting fields for one check phase.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CheckState {
    /// The outcome, or `None` until the phase's checks first run.
    pub(crate) status: Option<CheckStatus>,
    /// Number of CPUs covered by the phase.
    pub(crate) cpus: u64,
    /// Monotonic elapsed time in microseconds.
    pub(crate) elapsed_us: i64,
    /// Process CPU time in microseconds.
    pub(crate) cpu_us: i64,
    /// VM generation to which this phase applies.
    pub(crate) generation: u64,
    /// Number of conformance failures, which only report-only mode records.
    pub(crate) failures: Option<u64>,
}

impl CheckState {
    /// Records a completed phase, including actual report-only failures on either ISA.
    pub(crate) fn completed(
        cpus: u64,
        elapsed_us: i64,
        cpu_us: i64,
        failures: Option<u64>,
    ) -> Self {
        Self {
            status: Some(if failures.is_some_and(|failures| failures != 0) {
                CheckStatus::Fail
            } else {
                CheckStatus::Ok
            }),
            cpus,
            elapsed_us,
            cpu_us,
            failures,
            ..Self::default()
        }
    }
}

/// Waits for boot-only or all persisted checks using the existing status deadline.
pub(crate) fn wait_for_checks(boot_only: bool) -> Result<(TimeState, bool), Errno> {
    use super::deferred::{STATUS_POLL, STATUS_WAIT};
    let deadline = std::time::Instant::now() + STATUS_WAIT;
    loop {
        let state = TimeState::load()?;
        if !state.pending(boot_only) {
            return Ok((state, true));
        }
        if std::time::Instant::now() >= deadline {
            return Ok((state, false));
        }
        std::thread::sleep(STATUS_POLL);
    }
}

/// Complete state published under `/run/nvx/time/state`, of version one of its format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimeState {
    /// Current VM generation.
    pub(crate) generation: u64,
    /// Boot, capture, and restore records, in the order of `Phase::CHECKS`.
    checks: [CheckState; 3],
    /// Validated counter Hz: TSC on x86, CNTVCT on ARM, in the compatible `tsc_hz` slot.
    pub(crate) tsc_hz: u64,
    /// Validated local-APIC timer Hz on x86; zero on ARM, which has no local APIC.
    pub(crate) lapic_hz: u64,
    /// Number of observed clock discontinuities.
    pub(crate) discontinuities: u64,
    /// The latest discontinuity.
    pub(crate) last_discontinuity: Discontinuity,
    /// Signed nanoseconds applied by the latest step.
    pub(crate) last_step_ns: i64,
    /// Realtime clock immediately after the latest step.
    pub(crate) last_step_realtime_ns: i64,
    /// Most recently reported VM downtime in nanoseconds.
    pub(crate) last_downtime_ns: u64,
    /// The host clock that measured the latest downtime.
    pub(crate) last_downtime_source: DowntimeSource,
    /// Whether recent accepted samples keep the clock synchronized.
    pub(crate) synchronized: bool,
    /// Latest accepted guest-to-host clock offset in nanoseconds.
    pub(crate) offset_ns: i64,
    /// Latest accepted sample uncertainty in nanoseconds.
    pub(crate) uncertainty_ns: i64,
    /// Current frequency correction in parts per billion.
    pub(crate) frequency_ppb: i64,
    /// Whether Linux's limit of the frequency correction, ±500 ppm, clamped the correction that
    /// the host's clock correction asked for.
    pub(crate) frequency_limited: bool,
    /// The status of host UTC that the latest accepted sample reported.
    pub(crate) host_status: HostClockStatus,
    /// The host's clock correction that the latest accepted sample published: how fast host UTC
    /// runs against the counter counted at the declared rate, minus one, in parts per billion.
    pub(crate) host_correction_ppb: i64,
    /// The host's bound on the error of its correction, in parts per billion, if it knows one.
    pub(crate) host_correction_bound_ppb: Option<u64>,
    /// The host time service's bound on the error of host UTC, in microseconds, if it reports
    /// one.
    pub(crate) host_max_error_us: Option<u64>,
    /// The disruption counter of the latest accepted sample, which changes when host UTC steps,
    /// the host resumes from a suspension, or the guest counter's offset from the host's changes.
    pub(crate) host_disruption: u64,
    /// Whether the latest accepted sample carried the test-hook flag: host overrides, which only
    /// tests make, of the time that it published were active.
    pub(crate) host_test_hooks: bool,
    /// Number of accepted time samples.
    pub(crate) samples: u64,
    /// Number of rejected time samples.
    pub(crate) rejected_samples: u64,
    /// Why the latest time sample was rejected.
    pub(crate) last_sample_error: SampleRejection,
    /// Number of emitted ABI violations.
    pub(crate) violations: u64,
}

impl Default for TimeState {
    /// Returns a fresh state, whose only record is the pending boot check.
    fn default() -> Self {
        let boot = CheckState {
            status: Some(CheckStatus::Pending),
            ..CheckState::default()
        };
        Self {
            generation: 0,
            checks: [boot, CheckState::default(), CheckState::default()],
            tsc_hz: 0,
            lapic_hz: 0,
            discontinuities: 0,
            last_discontinuity: Discontinuity::None,
            last_step_ns: 0,
            last_step_realtime_ns: 0,
            last_downtime_ns: 0,
            last_downtime_source: DowntimeSource::None,
            synchronized: false,
            offset_ns: 0,
            uncertainty_ns: 0,
            frequency_ppb: 0,
            frequency_limited: false,
            host_status: HostClockStatus::Unknown,
            host_correction_ppb: 0,
            host_correction_bound_ppb: None,
            host_max_error_us: None,
            host_disruption: 0,
            host_test_hooks: false,
            samples: 0,
            rejected_samples: 0,
            last_sample_error: SampleRejection::None,
            violations: 0,
        }
    }
}

impl TimeState {
    /// Loads the existing state format without replacing a malformed record with a default.
    pub(crate) fn load() -> Result<Self, Errno> {
        let text = sys::read_file(STATE_PATH)?;
        Self::parse(&text).ok_or(Errno::EINVAL)
    }

    /// Durably replaces the published record using the existing ownership and mode.
    pub(crate) fn store(&self) -> Result<(), Errno> {
        let mode = Mode::S_IRUSR | Mode::S_IWUSR | Mode::S_IRGRP | Mode::S_IROTH;
        let file = CreatedFile::create(STATE_TEMP_PATH, false, mode)?;
        file.write_all(self.to_string().as_bytes())?;
        file.sync()?;
        file.rename(STATE_PATH)
    }

    /// Records the accepted time sample `sample`: the wall clock's offset from host UTC and its
    /// uncertainty, the frequency correction that the discipline holds, whether Linux's limit
    /// clamped it, the discipline of host UTC that the sample published, and whether host test
    /// hooks altered it.
    pub(crate) fn record_sample(&mut self, sample: &BestSample) {
        let host = &sample.host;
        self.samples += 1;
        self.offset_ns = sample.theta;
        self.uncertainty_ns = sample.epsilon;
        self.frequency_ppb = decoders::frequency_to_ppb(sample.frequency);
        self.frequency_limited = sample.limited;
        self.host_status = host.status;
        self.host_correction_ppb = decoders::frequency_to_ppb(host.correction);
        self.host_correction_bound_ppb = host.correction_bound.map(decoders::bound_to_ppb);
        self.host_max_error_us = host.max_error_us.map(u64::from);
        self.host_disruption = u64::from(host.disruption);
        self.host_test_hooks = sample.test_hooks;
        self.last_sample_error = SampleRejection::None;
    }

    /// Parses the state file's `key=value` lines, ignoring unknown keys, or returns `None` for a
    /// known key whose value is malformed.
    pub(crate) fn parse(text: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(text).ok()?;
        let mut state = Self::default();
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            state.set(key, value)?;
        }
        Some(state)
    }

    /// Sets the field that `key` names to `value`, or returns `None` for a malformed value. An
    /// unknown key changes nothing.
    fn set(&mut self, key: &str, value: &str) -> Option<()> {
        /// Parses a decimal number.
        fn number<T: FromStr>(value: &str) -> Option<T> {
            value.parse().ok()
        }
        /// Parses an optional bound: a decimal number, or `unknown` for none.
        fn bound(value: &str) -> Option<Option<u64>> {
            match value {
                "unknown" => Some(None),
                value => number(value).map(Some),
            }
        }
        let check = Phase::CHECKS
            .into_iter()
            .zip(&mut self.checks)
            .find_map(|(phase, check)| {
                let field = key.strip_prefix(phase.name())?.strip_prefix('_')?;
                Some((phase, check, field))
            });
        if let Some((phase, check, field)) = check {
            match field {
                "status" => check.status = Some(CheckStatus::parse(value)?),
                "cpus" => check.cpus = number(value)?,
                "elapsed_us" => check.elapsed_us = number(value)?,
                "cpu_us" => check.cpu_us = number(value)?,
                // The boot check always applies to the boot generation, which the file omits.
                "generation" if phase != Phase::Boot => check.generation = number(value)?,
                "failures" => check.failures = Some(number(value)?),
                _ => {}
            }
            return Some(());
        }
        match key {
            "generation" => self.generation = number(value)?,
            "tsc_hz" => self.tsc_hz = number(value)?,
            "lapic_hz" => self.lapic_hz = number(value)?,
            "discontinuities" => self.discontinuities = number(value)?,
            "last_discontinuity" => self.last_discontinuity = Discontinuity::parse(value)?,
            "last_step_ns" => self.last_step_ns = number(value)?,
            "last_step_realtime_ns" => self.last_step_realtime_ns = number(value)?,
            "last_downtime_ns" => self.last_downtime_ns = number(value)?,
            "last_downtime_source" => self.last_downtime_source = DowntimeSource::parse(value)?,
            "synchronized" => self.synchronized = number::<u64>(value)? != 0,
            "offset_ns" => self.offset_ns = number(value)?,
            "uncertainty_ns" => self.uncertainty_ns = number(value)?,
            "frequency_ppb" => self.frequency_ppb = number(value)?,
            "frequency_limited" => self.frequency_limited = number::<u64>(value)? != 0,
            "host_status" => self.host_status = HostClockStatus::parse(value)?,
            "host_correction_ppb" => self.host_correction_ppb = number(value)?,
            "host_correction_bound_ppb" => self.host_correction_bound_ppb = bound(value)?,
            "host_max_error_us" => self.host_max_error_us = bound(value)?,
            "host_disruption" => self.host_disruption = number(value)?,
            "host_test_hooks" => self.host_test_hooks = number::<u64>(value)? != 0,
            "samples" => self.samples = number(value)?,
            "rejected_samples" => self.rejected_samples = number(value)?,
            "last_sample_error" => self.last_sample_error = SampleRejection::parse(value)?,
            "violations" => self.violations = number(value)?,
            _ => {}
        }
        Some(())
    }

    /// Returns the record of `phase`, unless it is the runtime phase, which has none.
    pub(crate) fn check(&self, phase: Phase) -> Option<&CheckState> {
        self.checks.get(phase.check_index()?)
    }

    /// Returns the record of `phase`, unless it is the runtime phase, which has none.
    pub(crate) fn check_mut(&mut self, phase: Phase) -> Option<&mut CheckState> {
        self.checks.get_mut(phase.check_index()?)
    }

    /// Returns the phases whose checks have run, with their records and outcomes, in state-file
    /// order.
    fn recorded(&self) -> impl Iterator<Item = (Phase, &CheckState, CheckStatus)> {
        Phase::CHECKS.into_iter().filter_map(|phase| {
            let check = self.check(phase)?;
            Some((phase, check, check.status?))
        })
    }

    /// Returns whether the boot check, or unless `boot_only` any recorded check, is pending.
    pub(crate) fn pending(&self, boot_only: bool) -> bool {
        self.recorded()
            .filter(|&(phase, ..)| !boot_only || phase == Phase::Boot)
            .any(|(.., status)| status == CheckStatus::Pending)
    }

    /// Returns whether every recorded check passed.
    pub(crate) fn checks_ok(&self) -> bool {
        self.recorded()
            .all(|(.., status)| status == CheckStatus::Ok)
    }

    /// Formats the status markers: one per recorded check, then the discipline's.
    pub(crate) fn status_markers(&self, report_only: bool) -> String {
        let prefix = if report_only {
            "NVX-TIME-REPORT"
        } else {
            "NVX-TIME-ABI"
        };
        let mut text = String::new();
        for (phase, check, status) in self.recorded() {
            let _ = write!(
                text,
                "{prefix}: v=1 phase={} status={status} cpus={} tsc_hz={} lapic_hz={} \
                 generation={}",
                phase.name(),
                check.cpus,
                self.tsc_hz,
                self.lapic_hz,
                check.generation
            );
            if status != CheckStatus::Pending {
                let _ = write!(
                    text,
                    " elapsed_us={} cpu_us={}",
                    check.elapsed_us, check.cpu_us
                );
            }
            if report_only {
                let _ = write!(text, " failures={}", check.failures.unwrap_or(0));
            }
            text.push('\n');
        }
        let _ = writeln!(
            text,
            "{prefix}: v=1 phase=runtime status={} generation={} discontinuities={} \
             offset_ns={} uncertainty_ns={} rejected_samples={} last_sample_error={}",
            if self.synchronized {
                "synchronized"
            } else {
                "unsynchronized"
            },
            self.generation,
            self.discontinuities,
            self.offset_ns,
            self.uncertainty_ns,
            self.rejected_samples,
            self.last_sample_error
        );
        text
    }
}

impl fmt::Display for TimeState {
    /// Writes the state file, with its keys in the order that the ABI fixes.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "version=1")?;
        writeln!(formatter, "generation={}", self.generation)?;
        for (phase, _, status) in self.recorded() {
            writeln!(formatter, "{}_status={status}", phase.name())?;
        }
        for (phase, check, _) in self.recorded() {
            writeln!(formatter, "{}_cpus={}", phase.name(), check.cpus)?;
        }
        for (phase, check, _) in self.recorded() {
            writeln!(
                formatter,
                "{}_elapsed_us={}",
                phase.name(),
                check.elapsed_us
            )?;
        }
        for (phase, check, _) in self.recorded() {
            writeln!(formatter, "{}_cpu_us={}", phase.name(), check.cpu_us)?;
        }
        for (phase, check, _) in self.recorded().filter(|&(phase, ..)| phase != Phase::Boot) {
            writeln!(
                formatter,
                "{}_generation={}",
                phase.name(),
                check.generation
            )?;
        }
        for (phase, check, _) in self.recorded() {
            if let Some(failures) = check.failures {
                writeln!(formatter, "{}_failures={failures}", phase.name())?;
            }
        }
        writeln!(formatter, "tsc_hz={}", self.tsc_hz)?;
        writeln!(formatter, "lapic_hz={}", self.lapic_hz)?;
        writeln!(formatter, "discontinuities={}", self.discontinuities)?;
        writeln!(formatter, "last_discontinuity={}", self.last_discontinuity)?;
        writeln!(formatter, "last_step_ns={}", self.last_step_ns)?;
        writeln!(
            formatter,
            "last_step_realtime_ns={}",
            self.last_step_realtime_ns
        )?;
        writeln!(formatter, "last_downtime_ns={}", self.last_downtime_ns)?;
        writeln!(
            formatter,
            "last_downtime_source={}",
            self.last_downtime_source
        )?;
        writeln!(formatter, "synchronized={}", u8::from(self.synchronized))?;
        writeln!(formatter, "offset_ns={}", self.offset_ns)?;
        writeln!(formatter, "uncertainty_ns={}", self.uncertainty_ns)?;
        writeln!(formatter, "frequency_ppb={}", self.frequency_ppb)?;
        writeln!(
            formatter,
            "frequency_limited={}",
            u8::from(self.frequency_limited)
        )?;
        writeln!(formatter, "host_status={}", self.host_status)?;
        writeln!(
            formatter,
            "host_correction_ppb={}",
            self.host_correction_ppb
        )?;
        writeln!(
            formatter,
            "host_correction_bound_ppb={}",
            Bound(self.host_correction_bound_ppb)
        )?;
        writeln!(
            formatter,
            "host_max_error_us={}",
            Bound(self.host_max_error_us)
        )?;
        writeln!(formatter, "host_disruption={}", self.host_disruption)?;
        writeln!(
            formatter,
            "host_test_hooks={}",
            u8::from(self.host_test_hooks)
        )?;
        writeln!(formatter, "samples={}", self.samples)?;
        writeln!(formatter, "rejected_samples={}", self.rejected_samples)?;
        writeln!(formatter, "last_sample_error={}", self.last_sample_error)?;
        writeln!(formatter, "violations={}", self.violations)
    }
}

#[cfg(test)]
mod tests;

//! The C formats of the diagnostics' numbers and times.
//!
//! Rust formats a float with a precision exactly, rounding a tie to even, as musl's `printf`
//! does in its default rounding mode, so the fixed-point fields equal C's `%.Nf`. Only the
//! spellings of the values that are not finite differ, which [`fixed`] maps to C's.

/// Seconds in a day, which split a time into its day and the seconds within it.
#[cfg(target_arch = "x86_64")]
const SECONDS_PER_DAY: i64 = 24 * 60 * 60;
/// Days from 0000-03-01 to 1970-01-01 in the proleptic Gregorian calendar, which
/// [`civil_date`] counts from.
#[cfg(target_arch = "x86_64")]
const DAYS_TO_UNIX_EPOCH: i64 = 719_468;
/// Days in a 400-year cycle of the Gregorian calendar.
#[cfg(target_arch = "x86_64")]
const DAYS_PER_ERA: i64 = 146_097;
/// Years in a 400-year cycle of the Gregorian calendar.
#[cfg(target_arch = "x86_64")]
const YEARS_PER_ERA: i64 = 400;

/// Formats `value` with C's `%.{precision}f`.
fn fixed(value: f64, precision: usize) -> String {
    if value.is_nan() {
        if value.is_sign_negative() {
            "-nan"
        } else {
            "nan"
        }
        .to_owned()
    } else {
        format!("{value:.precision$}")
    }
}

/// Formats a whole-number field with C `%.0f`.
pub(super) fn f0(value: f64) -> String {
    fixed(value, 0)
}

/// Formats a one-decimal field with C `%.1f`.
pub(super) fn f1(value: f64) -> String {
    fixed(value, 1)
}

/// Formats a nine-decimal seconds field with C `%.9f`.
#[cfg(target_arch = "x86_64")]
pub(super) fn f9(value: f64) -> String {
    fixed(value, 9)
}

/// Returns the proleptic Gregorian year, month, and day of `days` since 1970-01-01, by Howard
/// Hinnant's `civil_from_days`, which counts from 0000-03-01 so that a leap day ends its year.
#[cfg(target_arch = "x86_64")]
fn civil_date(days: i64) -> (i64, i64, i64) {
    let days = days + DAYS_TO_UNIX_EPOCH;
    let era = days.div_euclid(DAYS_PER_ERA);
    let day_of_era = days.rem_euclid(DAYS_PER_ERA);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * YEARS_PER_ERA + i64::from(month <= 2);
    (year, month, day)
}

/// Formats Unix `seconds` as C's `strftime` with `%Y-%m-%dT%H:%M:%S` formats `gmtime_r`'s
/// UTC broken-down time.
#[cfg(target_arch = "x86_64")]
pub(super) fn utc_seconds(seconds: i64) -> String {
    let (year, month, day) = civil_date(seconds.div_euclid(SECONDS_PER_DAY));
    let time = seconds.rem_euclid(SECONDS_PER_DAY);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

#[cfg(test)]
mod tests;

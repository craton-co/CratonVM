// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The proleptic Julian calendar, and `GregorianCalendar`'s Julian/Gregorian
//! hybrid cutover.
//!
//! # Why this exists
//!
//! `java.util.GregorianCalendar` is a **hybrid** calendar: for any instant
//! before its Gregorian cutover (`1582-10-15` by default), `get(YEAR/MONTH/
//! DAY_OF_MONTH)` — and therefore anything that reads epoch millis through it,
//! including `Formatter`'s `Calendar`-branch printer for a `Date`/`Long`/
//! `Calendar` argument — returns **Julian calendar** fields, not proleptic
//! Gregorian. `civil_date.rs` is deliberately pure proleptic Gregorian (it
//! backs `java.time`, which is ISO-8601 by specification and must never see a
//! cutover), so the hybrid decode needs its own calendar and its own module.
//! See `jdk-only/gregoriancalendar-julian-cutover-not-implemented-for-pre-1582-dates-20260920-RETIRED-20260921.md`.
//!
//! # The algorithm
//!
//! Same closed-form shape as `civil_date::to_epoch_day`/`from_epoch_day`
//! (Howard Hinnant's `days_from_civil`/`civil_from_days`, March-shifted so the
//! leap day is the last day of the shifted year), specialised for the Julian
//! calendar's simpler leap rule: every 4th year, with no century exception.
//! That collapses `civil_date`'s 400-year era (`146097` days, with `-yoe/100
//! +yoe/400` century corrections) to a 4-year era (`1461 = 3*365 + 366` days,
//! no correction terms at all — the era is short enough that the single leap
//! year in it is accounted for entirely by which day-of-era a date falls on).
//!
//! The epoch-day offset (`719_470`, vs. `civil_date`'s `719_468`) is
//! calibrated so the two calendars agree on what a physical day *is*: epoch
//! day 0 (1970-01-01 proleptic Gregorian) is 1970-01-01 proleptic **Julian**
//! plus 13 days, i.e. `hybrid_from_epoch_day`/`to_epoch_day` treat "epoch day"
//! as one linear, calendar-agnostic count of physical days, and each calendar
//! only supplies a different `(year, month, day)` label for the same day
//! number. That is what makes weekday continuity across the 1582 reform fall
//! out for free (see [`crate::lang_string`]'s `day_of_week_sun0`, which computes
//! the weekday from the hybrid epoch day rather than from calendar-specific
//! fields for exactly this reason) — the reform deleted ten calendar *dates*,
//! never ten physical days.

/// Proleptic Julian leap year: every 4th year, no century exception.
///
/// Correct for `year <= 0` as written, same as `civil_date::is_leap_year`:
/// Rust's `%` gives a negative remainder for a negative dividend, and the only
/// comparison here is against zero.
#[inline]
pub fn is_leap_year(year: i32) -> bool {
    year % 4 == 0
}

/// Days in each month of a NON-leap year, indexed `month - 1`. Identical to
/// `civil_date`'s table — the Julian and Gregorian calendars agree on every
/// month's length except February's in a year one calls leap and the other
/// does not.
const DAYS_IN_MONTH: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// Length of `month` (**1-12**) in `year`, Julian rule.
///
/// # Panics
/// Panics on a month outside 1..=12 — see `civil_date::days_in_month`'s same
/// contract; the callers here are the same shape (a validated field decoder,
/// never the closed-form epoch-day conversion).
#[inline]
pub fn days_in_month(year: i32, month: i32) -> i32 {
    if month == 2 && is_leap_year(year) {
        29
    } else {
        DAYS_IN_MONTH[(month - 1) as usize]
    }
}

/// Day of year, 1-based, for a valid Julian-calendar `(year, month, day)`.
#[inline]
pub fn day_of_year(year: i32, month: i32, day: i32) -> i32 {
    let mut doy = day;
    for m in 1..month {
        doy += days_in_month(year, m);
    }
    doy
}

/// `(year, month[1-12], day[1-31])`, Julian calendar → days since
/// 1970-01-01 (the same linear epoch-day numbering `civil_date` uses).
#[inline]
pub fn to_epoch_day(year: i32, month: i32, day: i32) -> i64 {
    let y = year as i64 - i64::from(month <= 2);
    let m = month as i64;
    let d = day as i64;
    let era = y.div_euclid(4);
    let yoe = y - era * 4; // [0, 3]
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + doy; // [0, 1460]
    era * 1461 + doe - 719_470
}

/// Days since 1970-01-01 → `(year, month[1-12], day[1-31])`, Julian calendar.
/// Exact inverse of [`to_epoch_day`].
#[inline]
pub fn from_epoch_day(epoch_day: i64) -> (i32, i32, i32) {
    let z = epoch_day + 719_470;
    let era = z.div_euclid(1461);
    let doe = z - era * 1461; // [0, 1460]
    let yoe = (doe - doe.div_euclid(1460)) / 365; // [0, 3]
    let y = yoe + era * 4;
    let doy = doe - yoe * 365; // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    ((y + i64::from(m <= 2)) as i32, m as i32, d as i32)
}

/// `GregorianCalendar`'s default cutover, expressed as the epoch day of its
/// first Gregorian day (1582-10-15, proleptic Gregorian — the well-known
/// `DEFAULT_GREGORIAN_CUTOVER` instant, `-12219292800000L` ms). Every epoch
/// day strictly before this one decodes via the Julian calendar instead.
#[inline]
pub fn gregorian_cutover_epoch_day() -> i64 {
    crate::civil_date::to_epoch_day(1582, 10, 15)
}

/// Decode an epoch day into `GregorianCalendar`'s hybrid Julian/Gregorian
/// fields — the shared decode `Calendar.get(YEAR/MONTH/DAY_OF_MONTH)` and
/// `Formatter`'s `%t`/`%T` printer for a `Date`/`Long`/`Calendar` argument
/// both need. `epoch_day` is calendar-agnostic (a plain count of physical
/// days since 1970-01-01); this only decides which calendar's *labels* apply
/// to it.
#[inline]
pub fn hybrid_from_epoch_day(epoch_day: i64) -> (i32, i32, i32) {
    if epoch_day < gregorian_cutover_epoch_day() {
        from_epoch_day(epoch_day)
    } else {
        crate::civil_date::from_epoch_day(epoch_day)
    }
}

/// Encode `GregorianCalendar`'s hybrid fields into an epoch day — the inverse
/// of [`hybrid_from_epoch_day`] for every date outside the ten calendar dates
/// the 1582 reform deleted.
///
/// The two interpretations agree with each other everywhere except inside
/// that gap (Julian Oct 5-14, 1582 have no Gregorian equivalent, and
/// Gregorian Oct 5-14, 1582 have no Julian one — the reform skipped straight
/// from Julian Oct 4 to Gregorian Oct 15). Outside the gap, exactly one
/// interpretation is self-consistent: its own epoch day falls on the side of
/// the cutover that interpretation itself represents. A triple inside the gap
/// satisfies neither test; there is no historical date it can mean, so this
/// falls back to the Gregorian reading, the same direction a lenient
/// `GregorianCalendar` normalizes toward.
#[inline]
pub fn hybrid_to_epoch_day(year: i32, month: i32, day: i32) -> i64 {
    let cutover = gregorian_cutover_epoch_day();
    let julian_candidate = to_epoch_day(year, month, day);
    if julian_candidate < cutover {
        return julian_candidate;
    }
    crate::civil_date::to_epoch_day(year, month, day)
}

/// Day of year, 1-based, for `GregorianCalendar`'s hybrid fields — the Julian
/// calendar's own day-of-year before the cutover (a Julian leap year has a
/// day 366 that a same-numbered Gregorian year may not, and vice versa), the
/// proleptic Gregorian one on or after it.
#[inline]
pub fn hybrid_day_of_year(year: i32, month: i32, day: i32) -> i32 {
    if hybrid_to_epoch_day(year, month, day) < gregorian_cutover_epoch_day() {
        day_of_year(year, month, day)
    } else {
        crate::civil_date::day_of_year(year, month, day)
    }
}

#[cfg(test)]
mod tests;

// SPDX-License-Identifier: Apache-2.0
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
pub(crate) fn is_leap_year(year: i32) -> bool {
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
pub(crate) fn days_in_month(year: i32, month: i32) -> i32 {
    if month == 2 && is_leap_year(year) {
        29
    } else {
        DAYS_IN_MONTH[(month - 1) as usize]
    }
}

/// Day of year, 1-based, for a valid Julian-calendar `(year, month, day)`.
#[inline]
pub(crate) fn day_of_year(year: i32, month: i32, day: i32) -> i32 {
    let mut doy = day;
    for m in 1..month {
        doy += days_in_month(year, m);
    }
    doy
}

/// `(year, month[1-12], day[1-31])`, Julian calendar → days since
/// 1970-01-01 (the same linear epoch-day numbering `civil_date` uses).
#[inline]
pub(crate) fn to_epoch_day(year: i32, month: i32, day: i32) -> i64 {
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
pub(crate) fn from_epoch_day(epoch_day: i64) -> (i32, i32, i32) {
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
pub(crate) fn gregorian_cutover_epoch_day() -> i64 {
    crate::civil_date::to_epoch_day(1582, 10, 15)
}

/// Decode an epoch day into `GregorianCalendar`'s hybrid Julian/Gregorian
/// fields — the shared decode `Calendar.get(YEAR/MONTH/DAY_OF_MONTH)` and
/// `Formatter`'s `%t`/`%T` printer for a `Date`/`Long`/`Calendar` argument
/// both need. `epoch_day` is calendar-agnostic (a plain count of physical
/// days since 1970-01-01); this only decides which calendar's *labels* apply
/// to it.
#[inline]
pub(crate) fn hybrid_from_epoch_day(epoch_day: i64) -> (i32, i32, i32) {
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
pub(crate) fn hybrid_to_epoch_day(year: i32, month: i32, day: i32) -> i64 {
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
pub(crate) fn hybrid_day_of_year(year: i32, month: i32, day: i32) -> i32 {
    if hybrid_to_epoch_day(year, month, day) < gregorian_cutover_epoch_day() {
        day_of_year(year, month, day)
    } else {
        crate::civil_date::day_of_year(year, month, day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::civil_date;

    /// The one historical fact this whole module exists to reproduce: Julian
    /// Thursday 1582-10-04 was immediately followed by Gregorian Friday
    /// 1582-10-15 — a jump of one physical day, ten calendar dates deleted.
    #[test]
    fn the_cutover_boundary_is_one_physical_day() {
        let cutover = gregorian_cutover_epoch_day();
        assert_eq!(civil_date::from_epoch_day(cutover), (1582, 10, 15));
        assert_eq!(from_epoch_day(cutover - 1), (1582, 10, 4));
        assert_eq!(to_epoch_day(1582, 10, 4), cutover - 1);
        assert_eq!(hybrid_from_epoch_day(cutover - 1), (1582, 10, 4));
        assert_eq!(hybrid_from_epoch_day(cutover), (1582, 10, 15));
    }

    /// 1970 is 13 days off between the two calendars in the direction that
    /// matches every 20th/21st-century "Julian is 13 days behind Gregorian"
    /// reference: Julian 1970-01-01 is Gregorian 1970-01-14.
    #[test]
    fn nineteen_seventy_is_thirteen_days_off() {
        assert_eq!(civil_date::to_epoch_day(1970, 1, 14), to_epoch_day(1970, 1, 1));
        assert_eq!(from_epoch_day(civil_date::to_epoch_day(1970, 1, 14)), (1970, 1, 1));
    }

    /// The doc's own reproduction: `Asia/Kolkata`, epoch millis
    /// `-63549548877000L`, which after the +5:30 offset lands on proleptic
    /// year -44 (1 - year = 45, `%tY`'s era-relative form) Julian-calendar
    /// day 15 — HotSpot's answer for `%tF` of the same instant is
    /// `0045-03-15`. The unconditional civil-date (proleptic Gregorian)
    /// decode this module replaces gave day 13 for the same epoch day, which
    /// is what CratonVM printed as `0045-03-13` before this fix.
    #[test]
    fn the_known_issue_reproduction_now_matches_hotspot() {
        let ms = -63549548877000i64 + 19_800_000; // + Kolkata's +5:30 offset
        let epoch_day = ms.div_euclid(86_400_000);
        assert_eq!(
            civil_date::from_epoch_day(epoch_day),
            (-44, 3, 13),
            "the old unconditional proleptic-Gregorian decode, for contrast"
        );
        assert_eq!(hybrid_from_epoch_day(epoch_day), (-44, 3, 15));
    }

    /// A full round trip, Julian calendar, over a range with no gap to dodge
    /// (the Julian calendar itself has no deleted dates — only the hybrid
    /// encode/decode at the cutover does). Same oracle discipline as
    /// `civil_date`'s own sweep: an independently-advanced calendar, not just
    /// self-consistency.
    #[test]
    fn every_day_over_five_centuries_round_trips_and_matches_the_calendar() {
        let first = to_epoch_day(1000, 1, 1);
        let last = to_epoch_day(1500, 1, 1);
        let (mut y, mut m, mut d) = (1000, 1, 1);
        for ed in first..last {
            assert_eq!(
                from_epoch_day(ed),
                (y, m, d),
                "epoch day {ed} must decode as {y:04}-{m:02}-{d:02}",
            );
            assert_eq!(
                to_epoch_day(y, m, d),
                ed,
                "{y:04}-{m:02}-{d:02} must encode as epoch day {ed}",
            );
            d += 1;
            if d > days_in_month(y, m) {
                d = 1;
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
            }
        }
    }

    /// Julian leap years have no century exception: 1500 and 1900 are both
    /// leap under the Julian rule (unlike the Gregorian one, where only 1500
    /// pre-dates the cutover anyway and 1900 is famously NOT leap).
    #[test]
    fn julian_leap_years_have_no_century_exception() {
        for year in [1500, 1900, 4, 1996, -4] {
            assert!(is_leap_year(year), "{year} must be a Julian leap year");
            assert_eq!(from_epoch_day(to_epoch_day(year, 2, 29)), (year, 2, 29));
        }
        for year in [1, 99, 1899, -3] {
            assert!(!is_leap_year(year), "{year} must NOT be a Julian leap year");
        }
    }

    /// The hybrid encode/decode agree on every date at least a full Julian
    /// leap cycle away from the cutover, on both sides.
    #[test]
    fn hybrid_round_trips_away_from_the_gap() {
        for (y, m, d) in [
            (45, 3, 15),
            (-44, 3, 15),
            (1, 1, 1),
            (1581, 12, 31),
            (1582, 1, 1),
            (1582, 9, 1),
            (1582, 10, 15),
            (1582, 12, 31),
            (1583, 1, 1),
            (1970, 1, 1),
            (2024, 2, 29),
        ] {
            let ed = hybrid_to_epoch_day(y, m, d);
            assert_eq!(hybrid_from_epoch_day(ed), (y, m, d), "{y:04}-{m:02}-{d:02}");
        }
    }

    /// `hybrid_day_of_year` uses the calendar the date is actually on: a
    /// Julian leap year's day 366 is a real day even when the same-numbered
    /// Gregorian year would not have one.
    #[test]
    fn hybrid_day_of_year_uses_the_right_calendars_leap_rule() {
        // 1500 is a Julian leap year (pre-cutover): Dec 31 is day 366.
        assert_eq!(hybrid_day_of_year(1500, 12, 31), 366);
        // 1900 post-cutover is Gregorian and NOT a leap year: Dec 31 is day 365.
        assert_eq!(hybrid_day_of_year(1900, 12, 31), 365);
    }
}

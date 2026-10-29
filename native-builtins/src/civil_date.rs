// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! One proleptic-Gregorian calendar for the whole crate.
//!
//! Four modules used to carry their own copy of an epoch-day ↔ `(year, month,
//! day)` pair — `lang_string` (`String.format`'s `%t` conversions), `lib.rs`
//! (the synthetic `java.time` support), `jdbc` (`java.sql` date/time) and
//! `util_time` — and all four copies were the same wrong algorithm, because
//! they were the same text. `phases_early`'s `Calendar` pair was the one that
//! had been written correctly, from the same source this module now uses.
//!
//! # The defect the copies shared
//!
//! They computed the start of a year as
//!
//! ```text
//! year_start(y) = 365*y + y/4 - y/100 + y/400
//! ```
//!
//! and then walked months off `abs_day - year_start(y)`, adding
//! `days_in_month(y, m)` until the remainder fit. That expression steps by 366
//! days between `y` and `y+1` exactly when **`y + 1`** is a leap year, while
//! the month walk spends the leap day of **`y`**. The two disagree for every
//! year whose leap-ness differs from its successor's, which is most of them:
//!
//! * **Every day of a leap year decoded as the day before.** `2024-02-29`
//!   formatted as `28`, `2024-12-31` as `30`. Over 1900-2100 that is 17 885 of
//!   73 049 days — one year in four, silently.
//! * **January 1 of a leap year did not decode at all.** The remainder came out
//!   366 in a 365-day year, so the walk ran off the end of the twelve-element
//!   month table and the native *panicked*:
//!   `index out of bounds: the len is 12 but the index is 12`, which the VM
//!   surfaces to Java as an `InternalError`. That is the Tomcat
//!   `TestOneLineFormatterPerformance` failure — the test formats
//!   `System.nanoTime()` as though it were epoch millis, so it sweeps arbitrary
//!   far-future dates and eventually lands on one, which is why it looked
//!   nondeterministic and why probes using realistic timestamps never
//!   reproduced it.
//!
//! # What is here instead
//!
//! Howard Hinnant's `days_from_civil` / `civil_from_days` (the algorithm behind
//! `<chrono>`'s `year_month_day`, and the one `phases_early` already used).
//! It is closed-form: no year search, no month walk, and **no indexed month
//! table at all**, so the out-of-bounds this page is named for is not merely
//! fixed but unrepresentable. It is exact for every proleptic Gregorian date in
//! `i64` epoch-day range, including negative (BC) years, and it uses Euclidean
//! division throughout — the copies' `/` truncates toward zero, which is a
//! second, separate wrongness for years <= 0.

/// Days in each month of a NON-leap year, indexed `month - 1`.
///
/// Used only by [`days_in_month`] and [`day_of_year`]; the epoch-day
/// conversions below never index it, which is deliberate — see the module doc.
const DAYS_IN_MONTH: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// Proleptic Gregorian leap year.
///
/// Correct for `year <= 0` as written: Rust's `%` gives a negative remainder
/// for a negative dividend, but the comparisons are all against zero, so
/// `-4 % 4 == 0` and `-100 % 100 == 0` land in the right arms.
#[inline]
pub(crate) fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// Length of `month` (**1-12**) in `year`.
///
/// # Panics
/// Panics on a month outside 1..=12. That is the contract, not an oversight:
/// the callers are month walks bounded by 12 and validated field decoders, and
/// the one caller that could reach 13 — the epoch-day conversion — no longer
/// exists (it is closed-form now). A silent clamp here would have turned the
/// Tomcat panic into a wrong date instead of removing it.
#[inline]
pub(crate) fn days_in_month(year: i32, month: i32) -> i32 {
    if month == 2 && is_leap_year(year) {
        29
    } else {
        days_in_month_common(month)
    }
}

/// Length of `month` (**1-12**) in a NON-leap year.
///
/// Same panic contract as [`days_in_month`] above, and the same reason.
///
/// Exists for the callers that have a month but no year — `java.time.Month`'s
/// `length(boolean)`, `maxLength()` and `minLength()` take the leap flag as an
/// argument or answer both bounds, so they cannot go through the year-taking
/// form. They were indexing a `DAYS_IN_MONTH` they did not have, which is why
/// the `synthetic-jdk` feature arm stopped compiling; giving them this instead
/// of a private copy of the table is what keeps this module's "one table"
/// property true.
#[inline]
pub(crate) fn days_in_month_common(month: i32) -> i32 {
    DAYS_IN_MONTH[(month - 1) as usize]
}

/// Day of year, 1-based, for a valid `(year, month, day)`.
#[inline]
pub(crate) fn day_of_year(year: i32, month: i32, day: i32) -> i32 {
    let mut doy = day;
    for m in 1..month {
        doy += days_in_month(year, m);
    }
    doy
}

/// `(year, month[1-12], day[1-31])` → days since 1970-01-01.
///
/// Hinnant's `days_from_civil`. Shifts the year to start in March so the leap
/// day is the last day of the shifted year and needs no special case, which is
/// the whole trick: `153 * mp + 2) / 5` is an exact closed form for the day
/// offset of a March-based month.
#[inline]
pub(crate) fn to_epoch_day(year: i32, month: i32, day: i32) -> i64 {
    let y = year as i64 - i64::from(month <= 2);
    let m = month as i64;
    let d = day as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe.div_euclid(4) - yoe.div_euclid(100) + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 → `(year, month[1-12], day[1-31])`.
///
/// Hinnant's `civil_from_days`, the exact inverse of [`to_epoch_day`].
#[inline]
pub(crate) fn from_epoch_day(epoch_day: i64) -> (i32, i32, i32) {
    let z = epoch_day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    ((y + i64::from(m <= 2)) as i32, m as i32, d as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conversion is a bijection on a contiguous range that spans the
    /// Gregorian years anything in this VM can plausibly format, and every
    /// answer matches the calendar rather than merely matching its own inverse.
    ///
    /// A round-trip alone would NOT have caught the defect this module
    /// replaced: that algorithm was self-consistent for most dates and still
    /// answered the wrong day, because both halves shared the same wrong
    /// `year_start`. So the oracle here is an independent day count —
    /// `days_from_civil` of a date reached by adding one day at a time.
    #[test]
    fn every_day_over_five_centuries_round_trips_and_matches_the_calendar() {
        // 1700-01-01 .. 2200-01-01, one day at a time.
        let first = to_epoch_day(1700, 1, 1);
        let last = to_epoch_day(2200, 1, 1);
        let (mut y, mut m, mut d) = (1700, 1, 1);
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
            // Advance the independent calendar by exactly one day.
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

    /// The exact inputs the old algorithm got wrong, named rather than covered
    /// by a range: January 1 of a leap year is the one it PANICKED on, and the
    /// rest of a leap year is what it silently shifted by a day.
    #[test]
    fn the_leap_year_dates_the_previous_algorithm_lost() {
        // It panicked here — `index out of bounds: the len is 12 but the index
        // is 12` — because the remainder came out 366 in a 365-day year.
        for year in [1904, 1972, 2000, 2020, 2024, 2400] {
            let ed = to_epoch_day(year, 1, 1);
            assert_eq!(from_epoch_day(ed), (year, 1, 1));
        }
        // And it answered the day before, all year long, for these.
        for (y, m, d) in [
            (2024, 2, 29),
            (2024, 12, 31),
            (2000, 2, 29),
            (1904, 12, 31),
            (2020, 6, 15),
        ] {
            assert_eq!(from_epoch_day(to_epoch_day(y, m, d)), (y, m, d));
        }
        // The epoch itself, and the two anchors every copy hard-coded.
        assert_eq!(to_epoch_day(1970, 1, 1), 0);
        assert_eq!(from_epoch_day(0), (1970, 1, 1));
    }

    /// Negative epoch days and years <= 0. The replaced copies used truncating
    /// division, so this range was wrong for a second, independent reason.
    #[test]
    fn bc_years_and_negative_epoch_days_are_exact() {
        for (y, m, d) in [
            (1969, 12, 31),
            (1900, 1, 1),
            (1600, 2, 29), // leap: divisible by 400
            (1, 1, 1),
            (0, 2, 29), // proleptic year 0 IS a leap year
            (-1, 12, 31),
            (-44, 3, 15),
            (-400, 2, 29),
        ] {
            let ed = to_epoch_day(y, m, d);
            assert_eq!(
                from_epoch_day(ed),
                (y, m, d),
                "{y}-{m:02}-{d:02} (epoch day {ed})",
            );
        }
        assert_eq!(from_epoch_day(-1), (1969, 12, 31));
    }

    /// `day_of_year` agrees with the conversion it is not built on.
    #[test]
    fn day_of_year_agrees_with_the_epoch_day_difference() {
        for year in [1999, 2000, 2023, 2024] {
            for month in 1..=12 {
                for day in [1, 15, days_in_month(year, month)] {
                    let expected =
                        (to_epoch_day(year, month, day) - to_epoch_day(year, 1, 1) + 1) as i32;
                    assert_eq!(day_of_year(year, month, day), expected);
                }
            }
        }
    }
}

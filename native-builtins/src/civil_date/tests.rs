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

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
    assert_eq!(
        civil_date::to_epoch_day(1970, 1, 14),
        to_epoch_day(1970, 1, 1)
    );
    assert_eq!(
        from_epoch_day(civil_date::to_epoch_day(1970, 1, 14)),
        (1970, 1, 1)
    );
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

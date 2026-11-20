use super::*;



#[test]
fn the_expected_throw_rate_rule() {
    let floor = EXPECTED_THROW_TRIP_FLOOR;
    // Never at or under the floor, whatever the entries.
    assert!(!expected_throw_trip_rate_exceeded(floor, 0));
    assert!(!expected_throw_trip_rate_exceeded(1, 0));
    // Past it: more than half the entries.
    assert!(expected_throw_trip_rate_exceeded(floor + 1, 0));
    assert!(expected_throw_trip_rate_exceeded(1000, 1000));
    assert!(expected_throw_trip_rate_exceeded(1000, 1999));
    assert!(!expected_throw_trip_rate_exceeded(1000, 2000));
    // One throw in eight calls keeps the mark.
    assert!(!expected_throw_trip_rate_exceeded(1000, 8000));
    assert!(!expected_throw_trip_rate_exceeded(u32::MAX, u64::MAX));
}

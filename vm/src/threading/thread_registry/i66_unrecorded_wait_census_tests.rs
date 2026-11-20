use super::*;

#[test]
fn the_mode_is_read_from_the_value() {
    assert_eq!(
        UnrecordedWaitMode::from_value(None),
        UnrecordedWaitMode::Off
    );
    assert_eq!(
        UnrecordedWaitMode::from_value(Some("0")),
        UnrecordedWaitMode::Off
    );
    assert_eq!(
        UnrecordedWaitMode::from_value(Some("off")),
        UnrecordedWaitMode::Off
    );
    assert_eq!(
        UnrecordedWaitMode::from_value(Some("1")),
        UnrecordedWaitMode::Count
    );
    assert_eq!(
        UnrecordedWaitMode::from_value(Some("yes")),
        UnrecordedWaitMode::Count
    );
    assert_eq!(
        UnrecordedWaitMode::from_value(Some("trace")),
        UnrecordedWaitMode::Trace
    );
    assert_eq!(
        UnrecordedWaitMode::from_value(Some(" TRACE ")),
        UnrecordedWaitMode::Trace
    );
    assert_eq!(
        UnrecordedWaitMode::from_value(Some("assert")),
        UnrecordedWaitMode::Assert
    );
}

#[test]
fn regions_are_counted_by_call_site_and_door_only_when_on() {
    let mut registry = ThreadRegistry::new();
    registry.set_unrecorded_wait_mode_for_test(UnrecordedWaitMode::Off);
    let here = std::panic::Location::caller();
    registry.note_unrecorded_wait_region(here, "begin_blocking_region");
    assert!(!registry.unrecorded_wait_census_on());
    assert!(
        registry.unrecorded_wait_sites().is_empty(),
        "off: nothing counted"
    );

    registry.set_unrecorded_wait_mode_for_test(UnrecordedWaitMode::Count);
    assert!(registry.unrecorded_wait_census_on());
    let a = std::panic::Location::caller();
    for _ in 0..3 {
        registry.note_unrecorded_wait_region(a, "begin_blocking_region");
    }
    registry.note_unrecorded_wait_region(a, "begin_timed_blocking_region");
    let rows = registry.unrecorded_wait_sites();
    assert_eq!(rows.len(), 2, "one row per site and door: {rows:?}");
    assert_eq!(rows[0].count, 3, "most regions first");
    assert_eq!(rows[0].door, "begin_blocking_region");
    assert_eq!(rows[1].count, 1);
    assert_eq!(rows[1].door, "begin_timed_blocking_region");
    assert!(
        rows[0]
            .site
            .contains("i66_unrecorded_wait_census_tests.rs:"),
        "{}",
        rows[0].site
    );
}

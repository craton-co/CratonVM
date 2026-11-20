use super::*;

/// gci i6/mv: the audit may skip an address only when every occurrence of
/// it in the re-scan is a band word the active map proves dead.
#[test]
fn gci_i6mv_only_map_dead_needs_every_occurrence_dead() {
    let mut c = BandRootCensus::default();
    c.note(0x1000, true);
    c.note(0x1000, true);
    assert!(c.only_map_dead(0x1000, 2), "two dead words, two occurrences");
    // A third occurrence from another source (a live, unrewritten word
    // of any section) is a finding.
    assert!(!c.only_map_dead(0x1000, 3));
    // A list that merged entries disagrees with the census: report.
    assert!(!c.only_map_dead(0x1000, 1));
    // One live band word beside a dead one: report.
    c.note(0x2000, true);
    c.note(0x2000, false);
    assert_eq!((c.rooted(0x2000), c.map_dead(0x2000)), (2, 1));
    assert!(!c.only_map_dead(0x2000, 2));
    assert!(!c.only_map_dead(0x2000, 1));
    // Nothing recorded: never skipped.
    assert!(!c.only_map_dead(0x3000, 0));
    assert!(!c.only_map_dead(0x3000, 1));
}

/// gci i6/mv: armed by the audit only, recorded per thread, taken once.
#[test]
fn gci_i6mv_band_root_census_is_armed_recorded_and_taken_once() {
    assert!(take_band_root_census().is_none(), "never armed on a fresh thread");
    note_band_root_census(0x10, true);
    assert!(!band_root_census_armed());
    assert!(take_band_root_census().is_none(), "an unarmed note records nothing");
    arm_band_root_census();
    assert!(band_root_census_armed());
    note_band_root_census(0x10, true);
    note_band_root_census(0x18, false);
    let c = take_band_root_census().expect("armed");
    assert_eq!((c.rooted(0x10), c.map_dead(0x10)), (1, 1));
    assert_eq!((c.rooted(0x18), c.map_dead(0x18)), (1, 0));
    assert!(!band_root_census_armed(), "taking disarms");
    assert!(take_band_root_census().is_none());
}

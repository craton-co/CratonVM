use super::oldmark_census_sections;

fn marks_off(_: usize) -> &'static str {
    "<marks-off>"
}

fn two_steps(i: usize) -> &'static str {
    match i {
        0 => "<before-first-section>",
        1..=2 => "1: Thread frames",
        _ => "2: Static fields",
    }
}

/// gen r4w6/oomjit6: without marks the door still names its three parts;
/// with marks the first part is split per `collect_roots` step, and the
/// activation loaders pushed before the first mark get a name of their own.
#[test]
fn the_door_sections_name_every_part_of_the_root_vector() {
    let s = oldmark_census_sections(5, 9, None, marks_off);
    assert_eq!(s.len(), 3);
    assert_eq!(s[0].0, 0);
    assert_eq!((s[1].0, s[2].0), (5, 9));

    let s = oldmark_census_sections(5, 9, None, two_steps);
    let starts: Vec<usize> = s.iter().map(|&(i, _)| i).collect();
    assert_eq!(starts, vec![0, 1, 3, 5, 9]);
    assert!(s[0].1.starts_with("0: activation"));
    assert_eq!(s[1].1, "1: Thread frames");
    assert_eq!(s[2].1, "2: Static fields");

    // An empty `collect_roots` part does not ask for marks at index 0.
    let s = oldmark_census_sections(0, 0, None, two_steps);
    assert_eq!(s.len(), 3);
}

/// gci i4/wr (`gci-i1fr-...-review-findings` item 1): with the helper
/// pass's start recorded, the frozen-peer tail is two sections, both
/// under the old `xt frozen in-JIT peers` prefix; an index below
/// `snapshot_end` (never recorded by the driver) keeps the single section.
#[test]
fn gci_i4wr_the_frozen_peer_tail_splits_take_over_from_helper_windows() {
    let s = oldmark_census_sections(5, 9, Some(12), marks_off);
    assert_eq!(s.len(), 4);
    assert_eq!((s[2].0, s[3].0), (9, 12));
    assert!(s[2].1.starts_with("xt frozen in-JIT peers") && s[2].1.contains("take-over"));
    assert!(s[3].1.starts_with("xt frozen in-JIT peers") && s[3].1.contains("helper windows"));

    // No take-over roots: both start at `snapshot_end`; the census's stable
    // sort keeps the caller's order, so the helper section wins there.
    let s = oldmark_census_sections(5, 9, Some(9), marks_off);
    assert_eq!((s[2].0, s[3].0), (9, 9));
    assert!(s[3].1.contains("helper windows"));

    let s = oldmark_census_sections(5, 9, Some(3), marks_off);
    assert_eq!(s.len(), 3);
    assert_eq!(s[2].1, "xt frozen in-JIT peers (conservative)");
}

/// gci i4/wr: the driver records the helper pass's start before the pass,
/// and the collection door hands it to the census staging.
#[test]
fn gci_i4wr_the_driver_records_the_helper_start_before_the_pass() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let prod = src.as_str();
    let start = prod
        .find("fn stw_take_over_and_wait(")
        .expect("the take-over");
    let b = &prod[start..];
    let note = b
        .find("taken.note_census_helper_from(xt_roots.len());")
        .expect("the driver records the helper start");
    let pass = b.find("xt::helper_window_pass(").expect("pass");
    assert!(note < pass);
    assert!(prod.contains("taken.census_helper_from().map(|h| census_snapshot_end + h)"));
}

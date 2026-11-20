use super::{SelfLockCensus, SelfLockRow, ROWS, ROW_NAMES};

#[test]
fn every_row_has_one_name_and_the_last_row_closes_the_table() {
    assert_eq!(ROW_NAMES.len(), ROWS);
    let mut names = ROW_NAMES.to_vec();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), ROWS, "row names are unique");
}

#[test]
fn an_untouched_census_prints_nothing_and_a_count_prints_every_row() {
    let census = SelfLockCensus::default();
    assert!(census.line().is_none());
    census.note(SelfLockRow::EntryCompiles);
    census.note_n(SelfLockRow::DropReleasedHolds, 2);
    census.note_n(SelfLockRow::DropUnreleasedFrames, 0);
    assert_eq!(census.get(SelfLockRow::DropReleasedHolds), 2);
    let line = census.line().expect("a non-zero row prints");
    assert!(line.starts_with("[cratonvm] self-lock census: entry-compiles=1 "));
    assert!(line.contains(" drop-released-holds=2 "));
    assert!(line.contains(" drop-unreleased-frames=0 "));
    assert!(line.contains(" monitors-held-at-thread-exit=0 "));
    // JIT round 22 wave 8: the chain rows follow, in link order.
    assert!(line.ends_with(
        " exc-handover-bodies=0 hold-frames-taken=0 catch-stash-above-floor=0 orphan-drop-frames=0 door-drain-handovers=0 callee-claim-handovers=0"
    ));
    assert_eq!(census.rows().len(), ROWS);
}

#[test]
fn the_driver_bail_sites_map_to_their_rows() {
    let row = SelfLockCensus::body_refusal_row;
    assert_eq!(
        row(Some("self-lock-open-exit")),
        SelfLockRow::BodyRefusedOpenExits
    );
    assert_eq!(
        row(Some("self-lock-handler-exits")),
        SelfLockRow::BodyRefusedHandlerExits
    );
    assert_eq!(
        row(Some("self-lock-blocks-shape")),
        SelfLockRow::BodyRefusedBlocksShape
    );
    assert_eq!(
        row(Some("self-lock-handler-mode-exits")),
        SelfLockRow::BodyRefusedHandlerModeExits
    );
    assert_eq!(
        row(Some("self-lock-handler-shared-exits")),
        SelfLockRow::BodyRefusedHandlerSharedExits
    );
    assert_eq!(
        row(Some("implicit-null-unbound")),
        SelfLockRow::BodyRefusedOther
    );
    assert_eq!(row(None), SelfLockRow::BodyRefusedOther);
}

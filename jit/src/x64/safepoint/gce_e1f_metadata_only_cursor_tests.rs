/// gce e1/f (row 8 of the call-argument copies page): a metadata-only
/// safepoint records the live-frame bound on the NON-moving arm too, so
/// its map does not ship "unknown" and the band scan can drop the popped
/// argument slots above the cursor.
#[test]
fn gce_e1f_a_metadata_only_safepoint_records_the_cursor_without_moving_young() {
    let saved = super::super::licm::MOVING_YOUNG_OVERRIDE.with(|c| c.get());
    super::super::licm::set_moving_young_override(Some(false));
    let mut c = super::super::emit::tests::test_compiler();
    c.shadow_enabled = false;
    c.next_spill_offset = 136;
    c.pending_live_frame_hi = 0;
    c.emit_safepoint_metadata_only();
    let recorded = c.pending_live_frame_hi;
    super::super::licm::set_moving_young_override(saved);
    assert_eq!(
        recorded, 136,
        "the cursor, as the pre-safepoint spill takes it"
    );
}

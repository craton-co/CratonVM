#[test]
fn gcj_j3yg3_the_moving_phase_follows_rows_on_the_scans_request() {
    let src = concat!(include_str!("../gen_heap/mod.rs"), "\n", include_str!("../gen_heap/young_from_guard.rs"), "\n", include_str!("../gen_heap/geometry_cap_destination.rs"), "\n", include_str!("../gen_heap/publish_young_commit_bits.rs"), "\n", include_str!("../gen_heap/set_resolved_compact_primitive_field.rs"), "\n", include_str!("../gen_heap/set_field_full.rs"), "\n", include_str!("../gen_heap/non_moving_young_cycle.rs"), "\n", include_str!("../gen_heap/collect_garbage_inner_with_pins.rs"), "\n", include_str!("../gen_heap/young_drain_blocked.rs"), "\n", include_str!("../gen_heap/sweep_young_non_moving.rs"), "\n", include_str!("../gen_heap/sweep_old_gen_non_moving.rs"), "\n", include_str!("../gen_heap/mark_young_to_old_refs_deferring.rs"), "\n", include_str!("../gen_heap/cell_corrupt_diag_enabled.rs"), "\n", include_str!("../gen_heap/note_rejected_old_mark_candidate.rs"), "\n", include_str!("../gen_heap/zero_run_verdict.rs"));
    let follow = concat!(
        "let side_table_follow = side_table_licence || young_side_table",
        "_rows_follow();"
    );
    let rows = concat!(
        "let side_table_young_rows: Vec<(usize, Vec<usize>)> = if side_table",
        "_follow {"
    );
    let rekey = concat!("&& (side_table", "_licence || moving_major_loader_rekey_enabled());");
    assert_eq!(src.matches(follow).count(), 1);
    assert_eq!(src.matches(rows).count(), 1);
    assert_eq!(src.matches(rekey).count(), 1, "the P-C re-key keeps the licence");
    assert!(src.find(follow).expect("follow") < src.find(rows).expect("rows"));
    super::set_young_side_table_rows_follow();
    assert!(super::young_side_table_rows_follow());
    super::clear_young_side_table_rows_follow();
    assert!(!super::young_side_table_rows_follow());
}

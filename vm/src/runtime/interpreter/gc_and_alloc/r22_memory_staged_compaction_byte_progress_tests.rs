use super::*;

const MB: usize = 1 << 20;

#[test]
fn r22_memory_a_stage_that_shrinks_old_without_freeing_a_region_continues() {
    // A whole region freed: as before.
    assert!(staged_full_compaction_continues_by_bytes(1, 4, 5, 50 * MB, 50 * MB, MB));
    // No region freed, the Old extent shrank by a quarter region: one more stage.
    assert!(staged_full_compaction_continues_by_bytes(1, 4, 4, 50 * MB, 50 * MB - MB / 4, MB));
    // Less than an eighth of a region: stop.
    assert!(!staged_full_compaction_continues_by_bytes(1, 4, 4, 50 * MB, 50 * MB - MB / 16, MB));
    // The pool shrank: stop, whatever the bytes say.
    assert!(!staged_full_compaction_continues_by_bytes(1, 4, 3, 50 * MB, 40 * MB, MB));
    // The bound holds.
    assert!(!staged_full_compaction_continues_by_bytes(
        G1_STAGED_FULL_COMPACTION_PAUSES,
        4,
        4,
        50 * MB,
        40 * MB,
        MB
    ));
}

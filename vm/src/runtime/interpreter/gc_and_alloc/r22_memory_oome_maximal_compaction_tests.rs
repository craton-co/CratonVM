use super::*;


/// A maximal stage that freed no whole region still continues while it
/// shrinks Old by an eighth of a region, and stops at the bound.
#[test]
fn r22_memory_maximal_stages_continue_on_byte_progress() {
    let r = 1 << 20;
    assert!(staged_full_compaction_continues_by_bytes(1, 4, 4, 10 * r, 10 * r - r / 8, r));
    assert!(!staged_full_compaction_continues_by_bytes(1, 4, 4, 10 * r, 10 * r - 1, r));
    assert!(!staged_full_compaction_continues_by_bytes(
        G1_STAGED_FULL_COMPACTION_PAUSES,
        4,
        4,
        10 * r,
        r,
        r
    ));
}

use super::*;

fn contents(key: usize) -> Vec<usize> {
    let mut v: Vec<usize> = watched_referents_snapshot_for(key)
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default();
    v.sort_unstable();
    v
}

/// gcj j3/rf3 (`CRATONVM_GEN_WATCHED_SET_REUSE_UNCHANGED`): an unchanged
/// list reuses the row and publishes exactly the list again, with the
/// adds made after the previous publication (the String table's young
/// entries) removed; an address the list itself holds survives that
/// removal. A changed list, a held snapshot, a plain publish or an
/// unkeyed reset in between forces a rebuild, and the contents are the
/// list on every arm.
#[test]
fn gcj_j3rf3_an_unchanged_list_reuses_the_row_and_drops_the_adds() {
    set_watched_referents(&[]);
    let k = 0x7c00_0000_0000usize;
    assert!(!set_watched_referents_reusing_for(k, vec![0x10, 0x20, 0x20]), "new row");
    add_watched_referents(&[0x30, 0x10]); // 0x10 is in the list already
    assert_eq!(contents(k), vec![0x10, 0x20, 0x30]);
    assert!(set_watched_referents_reusing_for(k, vec![0x10, 0x20, 0x20]), "reused");
    assert_eq!(contents(k), vec![0x10, 0x20], "the add is gone, 0x10 stays");
    // A changed list rebuilds.
    assert!(!set_watched_referents_reusing_for(k, vec![0x10, 0x28]));
    assert_eq!(contents(k), vec![0x10, 0x28]);
    // A held snapshot forbids the in-place reuse; contents still exact.
    add_watched_referents_for(k, &[0x40]);
    let held = watched_referents_snapshot_for(k).expect("published");
    assert!(!set_watched_referents_reusing_for(k, vec![0x10, 0x28]), "snapshot held");
    assert_eq!(contents(k), vec![0x10, 0x28]);
    assert!(held.contains(&0x40), "the snapshot keeps what it was taken with");
    drop(held);
    assert!(set_watched_referents_reusing_for(k, vec![0x10, 0x28]), "reusable again");
    // A plain publish drops the memo.
    set_watched_referents_for(k, &[0x10, 0x28]);
    assert!(!set_watched_referents_reusing_for(k, vec![0x10, 0x28]), "memo dropped");
    // An unkeyed reset drops the row and its memo; an add re-creates the
    // row without one, so the next reusing publish rebuilds.
    set_watched_referents(&[]);
    add_watched_referents_for(k, &[0x50]);
    assert!(!set_watched_referents_reusing_for(k, vec![0x10, 0x28]), "no stale memo");
    assert_eq!(contents(k), vec![0x10, 0x28]);
    set_watched_referents(&[]);
}

fn major_contents(key: usize) -> Vec<usize> {
    let mut v: Vec<usize> = watched_major_snapshot_for(key)
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default();
    v.sort_unstable();
    v
}

/// gcj j3/rf3 (`CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG`): the seed-only
/// row is read by the major's view only (the young sweeps' snapshot does
/// not see it), joins the watched row there, and is cleared by the heap's
/// next publication on both publish paths; another heap's row is apart.
/// Without a seed-only row the major's view shares the watched storage.
#[test]
fn gcj_j3rf3_the_seed_only_row_is_read_by_the_major_view_only() {
    set_watched_referents(&[]);
    let (a, b) = (0x7b00_0000_0000usize, 0x7a00_0000_0000usize);
    set_watched_referents_for(a, &[0x10]);
    let plain = watched_major_snapshot_for(a).expect("published");
    let sweep = watched_referents_snapshot_for(a).expect("published");
    assert!(plain.shares_storage_with(&sweep), "no seed-only row: the same set");
    drop((plain, sweep));
    add_seed_only_referents(&[0x20, 0x30]);
    assert_eq!(contents(a), vec![0x10], "the sweeps do not see it");
    assert_eq!(major_contents(a), vec![0x10, 0x20, 0x30], "the major does");
    set_watched_referents_for(b, &[0x40]);
    assert_eq!(major_contents(b), vec![0x40], "another heap's row is apart");
    assert_eq!(major_contents(a), vec![0x10, 0x20, 0x30], "and keeps its own");
    set_watched_referents_for(a, &[0x10]);
    assert_eq!(major_contents(a), vec![0x10], "the next publish clears it");
    add_seed_only_referents(&[0x50]);
    assert!(!set_watched_referents_reusing_for(a, vec![0x10]), "plain publish: no memo");
    add_seed_only_referents(&[0x50]);
    assert!(set_watched_referents_reusing_for(a, vec![0x10]), "reused");
    assert_eq!(major_contents(a), vec![0x10], "the reuse path clears it too");
    // Seed-only with an empty watched row still reaches the major.
    set_watched_referents_for(a, &[]);
    add_seed_only_referents(&[0x60]);
    assert!(watched_referents_snapshot_for(a).is_none());
    assert_eq!(major_contents(a), vec![0x60]);
    set_watched_referents(&[]);
    assert!(watched_major_snapshot_for(a).is_none(), "the reset drops every row");
}

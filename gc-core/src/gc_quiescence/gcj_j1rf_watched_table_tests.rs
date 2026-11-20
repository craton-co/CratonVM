use super::*;

/// gcj j1/rf (section 2 of
/// `common-watched-referent-set-is-per-thread-not-per-heap-20261012`): a
/// second heap's publication nested inside the first heap's window (after
/// its publish, before its collection reads) neither replaces the first
/// heap's row nor is read by it; the unkeyed reads answer for the most
/// recent publication, as the one-slot set did.
#[test]
fn gcj_j1rf_a_nested_publication_for_another_heap_keeps_each_heaps_row() {
    set_watched_referents(&[]);
    let (a, b) = (0x7f00_0000_0000usize, 0x7e00_0000_0000usize);
    set_watched_referents_for(a, &[0x10, 0x20]);
    add_watched_referents(&[0x30]); // the string pool's add, right after
    set_watched_referents_for(b, &[0x40]);
    // B's collection reads B only.
    let snap_b = watched_referents_snapshot_for(b).expect("B published");
    assert_eq!(snap_b.len(), 1);
    assert!(snap_b.contains(&0x40) && !snap_b.contains(&0x10));
    assert!(is_watched_referent_for(b, 0x40) && !is_watched_referent_for(b, 0x30));
    // The unkeyed reads answer for the most recent publication (B).
    assert!(is_watched_referent(0x40) && !is_watched_referent(0x10));
    // A's collection still reads A's set, its add included.
    let snap_a = watched_referents_snapshot_for(a).expect("A kept");
    assert_eq!(snap_a.len(), 3);
    for addr in [0x10usize, 0x20, 0x30] {
        assert!(snap_a.contains(&addr) && is_watched_referent_for(a, addr));
    }
    assert!(!snap_a.contains(&0x40));
    // A heap with no row of its own and no unkeyed row reads nothing.
    assert!(watched_referents_snapshot_for(0x7d00_0000_0000).is_none());
    drop((snap_a, snap_b));
    set_watched_referents(&[]);
    assert!(watched_referents_snapshot_for(a).is_none(), "an unkeyed publish resets");
    assert!(watched_referents_snapshot_for(b).is_none());
}

/// The kill-switch shape (`CRATONVM_GC_WATCHED_SET_PER_HEAP=0`): every
/// publication is unkeyed, so a keyed reader falls back to the one key-0
/// row -- the one-slot set exactly, including the positive control: a
/// nested unkeyed publication REPLACES the first one (the hazard).
#[test]
fn gcj_j1rf_unkeyed_publications_are_the_one_slot_set() {
    let a = 0x7f00_0000_0000usize;
    set_watched_referents(&[0x10]);
    assert!(is_watched_referent_for(a, 0x10), "the fallback row");
    set_watched_referents(&[0x40]); // a nested VM's unkeyed publish
    let snap = watched_referents_snapshot_for(a).expect("the last publish");
    assert!(snap.contains(&0x40) && !snap.contains(&0x10), "replaced");
    drop(snap);
    // A keyed row wins over the fallback for its own heap only.
    set_watched_referents_for(a, &[0x50]);
    assert!(is_watched_referent_for(a, 0x50) && !is_watched_referent_for(a, 0x40));
    assert!(is_watched_referent_for(0x1234_0000, 0x40), "others still fall back");
    set_watched_referents(&[]);
    assert!(watched_referents_snapshot().is_none());
}

/// The table holds at most `WATCHED_TABLE_ROWS` heaps; the least recently
/// published row is the one evicted, and republishing refreshes a row.
#[test]
fn gcj_j1rf_the_table_evicts_the_least_recently_published_row() {
    set_watched_referents(&[]);
    let key = |i: usize| 0x1000_0000 * (i + 1);
    for i in 0..WATCHED_TABLE_ROWS {
        set_watched_referents_for(key(i), &[i + 8]);
    }
    // Refresh row 0, then one more heap evicts row 1 (the oldest now).
    set_watched_referents_for(key(0), &[8]);
    set_watched_referents_for(key(WATCHED_TABLE_ROWS), &[99]);
    assert!(is_watched_referent_for(key(0), 8), "refreshed row kept");
    assert!(!is_watched_referent_for(key(1), 9), "oldest row evicted");
    assert!(is_watched_referent_for(key(2), 10));
    assert!(is_watched_referent_for(key(WATCHED_TABLE_ROWS), 99));
    set_watched_referents(&[]);
}

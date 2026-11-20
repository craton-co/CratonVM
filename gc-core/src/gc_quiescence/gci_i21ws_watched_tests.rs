use super::*;

/// Deterministic pseudo-random heap-like addresses (8-aligned, some
/// duplicated, some sharing low bits), so the equality test below covers
/// collisions in both hashers' buckets.
fn addrs(n: usize) -> Vec<usize> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let a = 0x7f00_0000_0000usize + ((x as usize) & 0x00ff_fff8);
        out.push(a);
        if i % 5 == 0 {
            out.push(a); // a reference row naming the same word twice
        }
        if i % 7 == 0 {
            out.push(0x7f00_0000_0000usize + (i << 12)); // page-aligned run
        }
    }
    out
}

/// The Fx-hashed published set answers EXACTLY as the std `HashSet` it
/// replaced: the same members (sorted iteration equal), the same `len`,
/// and the same `contains` verdict for every published address, every
/// near miss (+/-8, +4) and a run of unrelated words -- through the
/// snapshot and through `is_watched_referent`.
#[test]
fn gci_i21ws_the_fx_watched_set_answers_as_the_std_set() {
    let published = addrs(20_000);
    let reference: std::collections::HashSet<usize> = published.iter().copied().collect();
    set_watched_referents(&published);
    let snap = watched_referents_snapshot().expect("non-empty");
    assert_eq!(snap.len(), reference.len());
    let mut ours: Vec<usize> = snap.iter().copied().collect();
    let mut theirs: Vec<usize> = reference.iter().copied().collect();
    ours.sort_unstable();
    theirs.sort_unstable();
    assert_eq!(ours, theirs, "same members");
    let mut via_into_iter: Vec<usize> = (&snap).into_iter().copied().collect();
    via_into_iter.sort_unstable();
    assert_eq!(via_into_iter, theirs);
    for &a in &published {
        for q in [a, a.wrapping_add(8), a.wrapping_sub(8), a.wrapping_add(4)] {
            assert_eq!(snap.contains(&q), reference.contains(&q), "{q:#x}");
            assert_eq!(is_watched_referent(q), reference.contains(&q), "{q:#x}");
        }
    }
    for q in (0..4096usize).map(|i| 0x1000 + i * 8) {
        assert_eq!(snap.contains(&q), reference.contains(&q), "{q:#x}");
    }
    drop(snap);
    // An add after the publish answers as the std set's `extend`.
    let extra = [0x10usize, 0x18, published[3]];
    add_watched_referents(&extra);
    let mut reference = reference;
    reference.extend(extra.iter().copied());
    let snap = watched_referents_snapshot().expect("non-empty");
    assert_eq!(snap.len(), reference.len());
    for q in extra.iter().chain(published.iter()) {
        assert_eq!(snap.contains(q), reference.contains(q), "{q:#x}");
    }
    drop(snap);
    set_watched_referents(&[]);
    assert!(watched_referents_snapshot().is_none());
}

/// Two snapshots of one publication share it (no copy); both answer as
/// the published addresses; an empty publication answers `None`.
#[test]
fn gci_i21ws_watched_snapshot_shares_the_published_set() {
    set_watched_referents(&[0x1000, 0x2000, 0x3000, 0x2000]);
    let a = watched_referents_snapshot().expect("non-empty");
    let b = watched_referents_snapshot().expect("non-empty");
    assert!(a.shares_storage_with(&b), "a snapshot is an Arc clone, not a copy");
    assert_eq!(a.len(), 3, "duplicates collapse");
    for addr in [0x1000usize, 0x2000, 0x3000] {
        assert!(a.contains(&addr) && is_watched_referent(addr));
    }
    assert!(!a.contains(&0x1008) && !is_watched_referent(0x1008));
    drop(a);
    drop(b);
    set_watched_referents(&[]);
    assert!(watched_referents_snapshot().is_none());
    assert!(!is_watched_referent(0x1000));
}

/// A snapshot held across a republish or an add keeps the contents it was
/// taken with (copy-on-write), and the thread's set has the new ones.
#[test]
fn gci_i21ws_a_held_snapshot_never_changes_under_its_holder() {
    set_watched_referents(&[0x10, 0x20]);
    let held = watched_referents_snapshot().expect("non-empty");
    add_watched_referents(&[0x30]);
    assert!(!held.contains(&0x30), "the add copied, the holder kept its set");
    assert!(is_watched_referent(0x30));
    set_watched_referents(&[0x40]);
    assert!(held.contains(&0x10) && held.contains(&0x20) && !held.contains(&0x40));
    assert!(is_watched_referent(0x40) && !is_watched_referent(0x10));
    drop(held);
    // Unshared again: republishing reuses the set in place.
    set_watched_referents(&[0x50]);
    assert!(is_watched_referent(0x50) && !is_watched_referent(0x40));
    add_watched_referents(&[]);
    assert_eq!(watched_referents_snapshot().map(|s| s.len()), Some(1));
    set_watched_referents(&[]);
}

/// `from_addrs` / `collect` build the same set the publisher does.
#[test]
fn gci_i21ws_watched_snapshot_constructors_agree() {
    let a = WatchedSnapshot::from_addrs(&[8, 16, 16]);
    let b: WatchedSnapshot = [16usize, 8].into_iter().collect();
    assert_eq!(a.len(), 2);
    assert!(b.contains(&8) && b.contains(&16) && !b.is_empty());
    assert!(!a.shares_storage_with(&b));
    assert!(WatchedSnapshot::default().is_empty());
}

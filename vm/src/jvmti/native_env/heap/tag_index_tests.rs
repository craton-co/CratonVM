//! i87-L1 (interpreter round i4 wave 92): the tag table's address index
//! under a heap that moves objects without bumping its collection count.
use super::*;

/// A table of `tags` (weak handle, tag), indexed at `addrs` (handle to
/// address) as a refresh at an unchanged count would have left it.
fn table(exact: bool, tags: &[(JObject, i64)], addrs: &HashMap<JObject, usize>) -> TagTable {
    let mut t = TagTable {
        entries: tags.to_vec(),
        index: HashMap::new(),
        epoch: Some(7),
        exact,
    };
    t.reindex(&mut |w| addrs.get(&w).copied());
    t
}

#[test]
fn a_relocation_between_counts_is_seen_when_exact() {
    let mut heap: HashMap<JObject, usize> = HashMap::from([(0x11, 0x1000), (0x21, 0x2000)]);
    let mut t = table(true, &[(0x11, 5), (0x21, 9)], &heap);
    // The object tagged 5 moves onto 0x3000, and the one tagged 9 onto
    // 0x1000 (the first's old address); the count does not change.
    heap.insert(0x11, 0x3000);
    heap.insert(0x21, 0x1000);
    let tag_at = |t: &mut TagTable, addr: usize, heap: &HashMap<JObject, usize>| {
        t.entry_of(addr, |w| heap.get(&w).copied())
            .map_or(0, |i| t.entries[i].1)
    };
    assert_eq!(
        tag_at(&mut t, 0x3000, &heap),
        5,
        "a moved object keeps its tag"
    );
    assert_eq!(
        tag_at(&mut t, 0x1000, &heap),
        9,
        "an old address names its new tenant"
    );
    assert_eq!(
        tag_at(&mut t, 0x2000, &heap),
        0,
        "a vacated address is untagged"
    );
    // A SetTag on the moved object finds its entry: no second entry.
    assert!(t.entry_of(0x3000, |w| heap.get(&w).copied()).is_some());
    assert_eq!(t.entries.len(), 2);
}

#[test]
fn a_pause_collector_trusts_its_index() {
    let heap: HashMap<JObject, usize> = HashMap::from([(0x11, 0x1000)]);
    let mut t = table(false, &[(0x11, 5)], &heap);
    let mut resolved = 0;
    let hit = t.entry_of(0x1000, |w| {
        resolved += 1;
        heap.get(&w).copied()
    });
    assert_eq!(hit, Some(0));
    let miss = t.entry_of(0x2000, |w| {
        resolved += 1;
        heap.get(&w).copied()
    });
    assert_eq!(miss, None);
    assert_eq!(
        resolved, 0,
        "no weak reference is resolved off the exact path"
    );
}

#[test]
fn a_cleared_reference_is_left_for_refresh() {
    let mut heap: HashMap<JObject, usize> = HashMap::from([(0x11, 0x1000), (0x21, 0x2000)]);
    let mut t = table(true, &[(0x11, 5), (0x21, 9)], &heap);
    heap.remove(&0x11);
    assert_eq!(t.entry_of(0x1000, |w| heap.get(&w).copied()), None);
    // Still held, so the next refresh frees it and posts its ObjectFree.
    assert_eq!(t.entries.len(), 2);
    assert_eq!(t.entry_of(0x2000, |w| heap.get(&w).copied()), Some(1));
}

/// gci i20/mr finding 8: an index entry past the end of `entries` (left
/// by an untag that could not re-key the entry it moved) is a miss on
/// both paths and is skipped by the walk's snapshot -- never an index
/// out of bounds.
#[test]
fn gci_i20mr_a_stale_index_past_the_end_is_a_miss() {
    let heap: HashMap<JObject, usize> = HashMap::from([(0x11, 0x1000)]);
    for exact in [false, true] {
        let mut t = table(exact, &[(0x11, 5)], &heap);
        t.index.insert(0x4000, 1);
        assert_eq!(
            t.entry_of(0x4000, |w| heap.get(&w).copied()),
            None,
            "{exact}"
        );
        assert_eq!(
            t.entry_of(0x1000, |w| heap.get(&w).copied()),
            Some(0),
            "{exact}"
        );
        let mut t = table(exact, &[(0x11, 5)], &heap);
        t.index.insert(0x4000, 3);
        let snap = t.snapshot();
        assert_eq!(snap.get(&0x1000), Some(&5));
        assert!(!snap.contains_key(&0x4000));
    }
}

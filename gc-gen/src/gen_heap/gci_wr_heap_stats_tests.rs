use super::HeapStats;
use std::sync::atomic::Ordering;

/// gci i2/wr (`gci-i1wr-address-memo-epoch-and-weak-table-review-findings`
/// item 3): the two-load count answers exactly what the 19-load snapshot's
/// two fields add up to, and moves with either kind of collection.
#[test]
fn gci_wr_two_load_collection_count_matches_the_snapshot() {
    let stats = HeapStats::default();
    assert_eq!(stats.collection_count(), 0);
    stats.minor_gc_count.fetch_add(3, Ordering::Relaxed);
    stats.old_allocations.fetch_add(99, Ordering::Relaxed);
    assert_eq!(stats.collection_count(), 3);
    stats.major_gc_count.fetch_add(2, Ordering::Relaxed);
    let s = stats.snapshot();
    assert_eq!(stats.collection_count(), s.minor_gc_count + s.major_gc_count);
    assert_eq!(stats.collection_count(), 5);
}

use super::*;

/// `gch-h4g1-g1-young-only-pauses-stamp-the-old-pools-collection-usage`:
/// only a `Mixed` record moves `mixed_collection_count()` (the GC event
/// plumbing reads an unmoved count as a young-only pause); every record
/// moves `collection_count()`.
#[test]
fn gci_i5g_g1o_only_mixed_records_move_the_mixed_count() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 1,
        ..G1CollectorConfig::default()
    });
    let stats = GcStats {
        objects_copied: 0,
        bytes_copied: 0,
        bytes_freed: 0,
    };
    assert_eq!(gc.mixed_collection_count(), 0);
    gc.record_collection(G1CollectionType::YoungOnly, 10, &stats);
    assert_eq!(gc.mixed_collection_count(), 0, "a young-only pause");
    gc.record_collection(G1CollectionType::Mixed, 10, &stats);
    assert_eq!(gc.mixed_collection_count(), 1, "a mixed pause");
    // A drain pass inside that mixed pause records YoungOnly: the pause
    // still reads mixed across its two edges.
    gc.record_collection(G1CollectionType::YoungOnly, 1, &stats);
    assert_eq!(gc.mixed_collection_count(), 1);
    assert_eq!(gc.collection_count(), 3);
}

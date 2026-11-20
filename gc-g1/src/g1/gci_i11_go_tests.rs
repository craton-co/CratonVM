//! gci i11/go: the pause-type / cause naming accessors and the opt-in
//! `Prepare Mixed` pause (`gci-i10rh-...-review-findings-20261013`
//! findings 4 and 9).
use super::*;
use cratonvm_gc_core::collector::GarbageCollector;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: these tests run the heap single-threaded; no mutator runs.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 16 * 1024 * 1024,
        initial_heap_size: 16 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 1,
        ..G1CollectorConfig::default()
    })
}

/// HotSpot's `is_humongous`: strictly more than half a region.
#[test]
fn gci_i11_go_humongous_request_is_more_than_half_a_region() {
    let gc = collector();
    assert_eq!(gc.region_size(), 1024 * 1024);
    assert!(!gc.is_humongous_request(512 * 1024));
    assert!(gc.is_humongous_request(512 * 1024 + 1));
}

/// The peek answers like the level test plus `check_ihop` and moves no
/// census.
#[test]
fn gci_i11_go_would_start_marking_has_no_side_effects() {
    let gc = collector();
    let mb = 1024 * 1024;
    gc.marking_threshold_bytes.store(0, Ordering::Relaxed);
    gc.old_gen_bytes.store(4 * mb, Ordering::Relaxed);
    assert!(!gc.would_start_marking(), "no threshold, no cycle");
    gc.marking_threshold_bytes.store(4 * mb, Ordering::Relaxed);
    assert!(!gc.would_start_marking(), "at the threshold is not over it");
    gc.old_gen_bytes.store(5 * mb, Ordering::Relaxed);
    gc.last_cycle_end_old_bytes.store(0, Ordering::Relaxed);
    assert!(gc.would_start_marking(), "the first cycle is never suppressed");
    // A futile last cycle that ended at 5 MiB: one region of growth is
    // required (backoff on), and the deadline is not armed (no cycle end).
    gc.last_cycle_reclaimed_bytes.store(0, Ordering::Relaxed);
    gc.last_cycle_end_old_bytes.store(5 * mb, Ordering::Relaxed);
    gc.mark_backoff_regions.store(1, Ordering::Relaxed);
    gc.last_cycle_end_collections.store(u64::MAX, Ordering::Relaxed);
    let before = gc.mark_backoff_suppressions.load(Ordering::Relaxed);
    assert_eq!(gc.would_start_marking(), !gc_flags().g1_ihop_backoff);
    gc.old_gen_bytes.store(6 * mb, Ordering::Relaxed);
    assert!(gc.would_start_marking(), "the required growth arrived");
    assert_eq!(gc.mark_backoff_suppressions.load(Ordering::Relaxed), before);
}

/// Switch arm: the first pause after cleanup is young-only and counted;
/// the next one is the mixed gate's ordinary decision. Default arm: the
/// flag is never armed.
#[test]
fn gci_i11_go_prepare_mixed_takes_one_young_pause_on_the_switch_arm() {
    let mut gc = collector();
    gc.prepare_mixed = true;
    gc.prepare_mixed_pending.store(true, Ordering::Relaxed);
    gc.marking_complete.store(true, Ordering::Release);
    gc.mixed_gc_remaining.store(8, Ordering::Relaxed);
    let mut roots: Vec<ObjectRef> = Vec::new();
    gc.collect_garbage(&stw(), &mut roots, &NoMonitors);
    assert_eq!(gc.prepare_mixed_pause_count(), 1);
    assert_eq!(gc.mixed_collection_count(), 0, "young-only");
    assert!(!gc.prepare_mixed_pending.load(Ordering::Relaxed), "consumed");
    assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 8, "the count is not spent");
    gc.collect_garbage(&stw(), &mut roots, &NoMonitors);
    assert_eq!(gc.prepare_mixed_pause_count(), 1, "one per phase");    }
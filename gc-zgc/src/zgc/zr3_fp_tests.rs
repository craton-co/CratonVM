use super::*;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

#[test]
fn the_trim_target_needs_a_floor_and_four_times_slack() {
    let f = ZGC_FP_TRIM_FLOOR;
    // At or under the floor: never.
    assert_eq!(zgc_fp_trim_target(0, f), None);
    // Mostly used: never.
    assert_eq!(zgc_fp_trim_target(f, 4 * f), None);
    assert_eq!(zgc_fp_trim_target(f + 1, 4 * f), None);
    // Empty and big: back to the floor.
    assert_eq!(zgc_fp_trim_target(0, 64 * f), Some(f));
    // A quarter or less used: keep twice what is held.
    assert_eq!(zgc_fp_trim_target(10 * f, 64 * f), Some(20 * f));
    // Never a shrink that does not at least halve the allocation.
    assert_eq!(zgc_fp_trim_target(f / 8, f + 1), None);
    assert_eq!(zgc_fp_trim_target(f / 8, 2 * f), Some(f));
}

#[test]
fn the_hash_table_estimate_covers_buckets_and_control_bytes() {
    assert_eq!(zgc_fp_hash_table_bytes(0, 24), 0);
    // 7 usable slots fit 8 buckets.
    assert_eq!(zgc_fp_hash_table_bytes(7, 24), 8 * 25);
    let mut m: FxHashMap<usize, (usize, u32)> = FxHashMap::default();
    m.reserve(100_000);
    let entry = std::mem::size_of::<(usize, (usize, u32))>();
    let est = zgc_fp_hash_table_bytes(m.capacity(), entry);
    assert!(est >= m.capacity() * entry, "{est} under the entries alone");
    assert!(
        est <= 2 * m.capacity() * (entry + 1) + entry + 1,
        "{est} more than twice the entries"
    );
}


/// One small cycle after a large one keeps the large buffer (the reuse
/// `the_dead_scratch_is_cleared_and_its_capacity_reused` pins); a run of
/// `ZGC_FP_SCRATCH_IDLE_CYCLES` small cycles gives it back.
#[test]
fn the_dead_scratch_is_trimmed_only_after_a_run_of_small_cycles() {
    let fp = ZFootprintCounters::from_env();
    fp.trim_on.store(true, Ordering::Relaxed);
    let mut v: Vec<usize> = Vec::with_capacity(1 << 20);
    let cap = v.capacity();
    for _ in 0..ZGC_FP_SCRATCH_IDLE_CYCLES - 1 {
        v.clear();
        v.extend(0..100);
        fp.after_dead_scratch(&mut v);
        assert_eq!(v.capacity(), cap, "trimmed before the idle run ended");
    }
    // A large cycle restarts the run.
    v.clear();
    v.extend(0..(cap / 2));
    fp.after_dead_scratch(&mut v);
    for _ in 0..ZGC_FP_SCRATCH_IDLE_CYCLES - 1 {
        v.clear();
        fp.after_dead_scratch(&mut v);
    }
    assert_eq!(v.capacity(), cap, "the large cycle must restart the run");
    v.clear();
    fp.after_dead_scratch(&mut v);
    assert!(
        v.capacity() <= 2 * ZGC_FP_TRIM_FLOOR,
        "kept {}",
        v.capacity()
    );
    assert_eq!(fp.trims.load(Ordering::Relaxed), 1);
    assert_eq!(
        fp.dead_scratch_peak.load(Ordering::Relaxed),
        cap * std::mem::size_of::<usize>()
    );
    // Kill switch: never.
    let off = ZFootprintCounters::from_env();
    off.trim_on.store(false, Ordering::Relaxed);
    let mut w: Vec<usize> = Vec::with_capacity(1 << 20);
    let wcap = w.capacity();
    for _ in 0..4 * ZGC_FP_SCRATCH_IDLE_CYCLES {
        off.after_dead_scratch(&mut w);
    }
    assert_eq!(w.capacity(), wcap);
    assert_eq!(off.trims.load(Ordering::Relaxed), 0);
}



/// The sparse clear leaves the same words as the dense one, counts the
/// zero words it skipped, and the noted-page estimate sees the set bits.
#[test]
fn the_sparse_clear_matches_the_dense_one_and_skips_zero_words() {
    const BASE: usize = 0x7000_0000_0000;
    const SPAN: usize = 8 * 1024 * 1024;
    let sparse = ZObjectStartBits::labelled(BASE, SPAN, "mark");
    let dense = ZObjectStartBits::labelled(BASE, SPAN, "mark");
    assert_eq!(sparse.reserved_bytes(), SPAN / 64);
    assert_eq!(sparse.noted_page_bytes(), 0);
    for b in [&sparse, &dense] {
        for i in 0..100usize {
            // One start every 64 KiB of arena = every 1 KiB of words, so
            // four starts per 4 KiB page of words: 25 pages.
            assert!(b.claim(BASE + i * 64 * 1024));
        }
    }
    assert_eq!(sparse.noted_page_bytes(), 25 * 4096);
    let ranges = [(BASE, BASE + SPAN)];
    let skipped = sparse.clear_within_sparse(&ranges);
    dense.clear_within(&ranges);
    let words = SPAN / 512;
    assert_eq!(skipped, (words - 100) as u64);
    for w in 0..words {
        assert_eq!(sparse.word_at(w), 0);
        assert_eq!(dense.word_at(w), 0);
    }
}


/// End to end: two collections over a dead population stamp the freed
/// granules' uncommit clock at the end of the sweep and skip the zero words
/// of the mark bitmap; with the switches off they do neither.
#[test]
fn a_collection_stamps_freed_granules_and_clears_the_mark_bitmap_sparsely() {
    for on in [true, false] {
        let heap = ZgcRealHeap::with_capacity(64 * 1024 * 1024);
        heap.set_tlab_enabled(false);
        heap.set_uncommit_options(true, 300);
        heap.set_footprint_switches(on, on, on);
        if heap.commit_stats().0 == heap.heap_capacity() {
            return; // wholly-committed fallback store: no clock to start
        }
        let mut roots: Vec<ObjectRef> = Vec::new();
        while heap.allocated_bytes() < 24 * 1024 * 1024 {
            heap.alloc_object(ClassId::new(4), 8);
        }
        {
            // SAFETY: single-threaded unit test.
            let stw = unsafe { StopTheWorldToken::new() };
            heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        }
        let s = heap.footprint_stats();
        assert_eq!(s.arena_reserved, heap.heap_capacity());
        assert!(s.arena_committed_peak >= 16 * 1024 * 1024, "{s:?}");
        assert!(s.arena_ever_written >= 16 * 1024 * 1024, "{s:?}");
        assert!(s.snapshot_peak > 0, "{s:?}");
        assert_eq!(s.start_bits_reserved, heap.heap_capacity() / 64);
        if on {
            assert!(s.idle_stamps > 0, "no granule stamped at the sweep: {s:?}");
            if heap.mark_bits.is_some() {
                assert!(s.sparse_words_skipped > 0, "{s:?}");
            }
        } else {
            assert_eq!(s.idle_stamps, 0, "{s:?}");
            assert_eq!(s.sparse_words_skipped, 0, "{s:?}");
        }
        // Nothing was released: the delay is 300 s.
        assert_eq!(s.arena_uncommitted_total, 0, "{s:?}");
    }
}

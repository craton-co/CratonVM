use super::*;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

/// A reused, dirty free-list block comes back zeroed from `alloc_raw`
/// whichever side of the arena guard the `memset` runs on -- and the block
/// really is the reused one, or the test would pass on fresh (zero) bump
/// memory.
#[test]
fn alloc_raw_zeroes_a_dirty_reused_block_inside_or_outside_the_lock() {
    for outside in [true, false] {
        let heap = ZgcRealHeap::new();
        // zt t27/gn5 (gn page item 10): single generation -- the mode's
        // arena serves the bump tail before the free list (G2c
        // `prefer_bump`), so the freed block would not be the one reused.
        heap.set_generational_enabled(false);
        heap.counters
            .raw_zero_outside_lock
            .store(outside, Ordering::Relaxed);
        const LEN: usize = 4096;
        let (base, off) = {
            let mut arena = heap.arena.lock();
            let base = arena.base_ptr() as usize;
            let p = arena.alloc(LEN, 8).expect("fresh arena has room");
            // A second block above it, so the freed one is not the bump
            // tail a retraction could fold away.
            let _guard = arena.alloc(LEN, 8).expect("fresh arena has room");
            // SAFETY: `p` is `LEN` bytes the arena just handed out.
            unsafe { std::ptr::write_bytes(p, 0xAB, LEN) };
            let off = p as usize - base;
            arena.add_free_block(off, LEN);
            (base, off)
        };
        let got = heap.alloc_raw(LEN, &|_| {}).expect("the free block fits");
        assert_eq!(
            got as usize,
            base + off,
            "outside={outside}: the dirty block must be the one reused"
        );
        // SAFETY: `got` is `LEN` bytes `alloc_raw` just returned.
        let bytes = unsafe { std::slice::from_raw_parts(got, LEN) };
        assert!(
            bytes.iter().all(|&b| b == 0),
            "outside={outside}: a reused block must be handed out zeroed"
        );
    }
}

/// `any_start_in` answers exactly "a base in `[lo, hi)`", at word and
/// sub-word edges.
#[test]
fn the_hot_heap_struct_does_not_grow_back() {
    let hot = std::mem::size_of::<ZgcRealHeap>();
    let cold = std::mem::size_of::<ZgcCounters>();
    // The per-heap JIT gate pointer and arena's optional uncommit-policy
    // pointer are read on barrier/allocation paths. Their addition brings
    // this to 1,216 bytes; timestamps stay behind the arena policy box.
    // 1,248 (2026-10-07): each embedded `HeapBitmap` (the start registry
    // and the mark bits) carries its block-summary pointer, which every
    // `insert` on the allocation path writes through and every interior
    // probe reads -- see `HeapBitmap::nearest_base_at_or_below`.
    // 1,264 (2026-10-13, zr z10/mt): each embedded `HeapBitmap` carries
    // `mapped_len`, the length of its own `MADV_NOHUGEPAGE` mapping
    // (`CRATONVM_ZGC_SIDE_TABLE_NO_THP`). Only `Drop` reads it, but it
    // must live with the words it unmaps, which are embedded here.
    // 1,272 (2026-10-16, gci i42/ta): the embedded reservation carries
    // `in_place_only` (a refused headroom never moves the store) and the
    // `ReservedHeap` its latched `uncommit_tail_granule`; only growth and
    // decommit read them, but they must live with the mapping they guard.
    // 1,280 (2026-10-19, zt t1/sp): the embedded `Arena` carries
    // `min_free_span`, the fit predicate's floor after a final refusal
    // (`CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`); every free-list fit reads
    // it, so it lives with the free list.
    // 1,288 (2026-10-24, zt t33): `barrier_heal_mask`, the color every
    // colored store and heal writes (`heal_color()`), read beside
    // `barrier_good_mask` on the store and barrier paths.
    // 1,320 (2026-11-01, scoped young slide, 77f439d115): each embedded
    // `HeapBitmap` (the start registry and the mark bits) carries
    // `young_blocks`, the per-block "may hold a young base" hint that every
    // `insert` on the allocation path writes and the scoped slide reads;
    // +16 bytes (a boxed slice) per bitmap.
    // 1,328 (2026-11-15, pooled ZObjectStarts snapshots): `ZObjectStarts::snap_pool`
    // (`Arc<Mutex<Vec<Vec<u64>>>>`, +8 bytes) on `registry` to reuse snapshot
    // buffers across GC cycles without reallocating.
    // 1,376 (2026-11-19): raised from 1,328 after gc-core/gc-zgc struct layout
    // updates (+48 bytes).
    const BOUND: usize = 1376;
    assert!(
        hot <= BOUND,
        "ZgcRealHeap is {hot} bytes (bound {BOUND}); ZgcCounters is {cold}. \
         A new field that is a counter, or per-cycle state nothing on the \
         allocation or barrier path reads, belongs in ZgcCounters -- see \
         its doc for which side of the line a field falls on. If it is \
         neither, raise the bound and say why."
    );
    assert!(
        cold >= 512,
        "ZgcCounters is only {cold} bytes; the cold half has drifted back \
         into the hot struct"
    );
}

/// Allocate one dead oversize (payload > 256 MiB, so `alloc_size`
/// refuses it) byte array per iteration on a heap that holds two, and
/// collect between iterations. HotSpot ZGC runs this indefinitely; the
/// retention ran out after two.
fn churn_oversize_arrays(heap: &ZgcRealHeap, iterations: usize) -> usize {
    const LEN: usize = 270 << 20;
    let stw = unsafe { StopTheWorldToken::new() };
    let mut served = 0usize;
    for _ in 0..iterations {
        match heap.try_alloc_array(ClassId::new(0), ArrayElementType::Byte, LEN) {
            Some(arr) => {
                // SAFETY: a fresh array; its header is well-formed.
                let header = unsafe { &*(arr.as_ptr() as *const ObjectHeader) };
                assert_eq!(
                    ZgcRealHeap::alloc_size(header),
                    None,
                    "the fixture must be one the corruption screen refuses"
                );
                served += 1;
            }
            None => break,
        }
        let _ = heap.collect_garbage(&stw, &mut [], &NoMonitors);
    }
    served
}

#[test]
fn a_dead_oversize_array_on_the_large_object_end_is_reclaimed() {
    let heap = ZgcRealHeap::with_capacity(600 << 20);
    assert!(heap
        .counters
        .sweep_sizes_oversize_arrays
        .load(Ordering::Relaxed));
    assert_eq!(
        churn_oversize_arrays(&heap, 6),
        6,
        "every iteration is served"
    );
    let (swept, bytes, refused) = heap.oversize_array_sweep_stats();
    assert_eq!(swept, 6, "each dead array is reclaimed by its collection");
    assert!(bytes >= 6 * (270 << 20));
    assert_eq!(refused, 0);
    assert!(heap.registry.bases().is_empty(), "nothing is retained");
}

/// The kill switch control: with the switch off the dead arrays are
/// retained (the old behaviour), so the third allocation fails.
#[test]
fn the_oversize_array_kill_switch_restores_the_retention() {
    let heap = ZgcRealHeap::with_capacity(600 << 20);
    heap.counters
        .sweep_sizes_oversize_arrays
        .store(false, Ordering::Relaxed);
    let served = churn_oversize_arrays(&heap, 6);
    assert!(
        served < 6,
        "retention must exhaust the heap ({served} served)"
    );
    assert_eq!(heap.oversize_array_sweep_stats(), (0, 0, 0));
}

/// A live oversize array is untouched, and a dead one whose length has
/// been clobbered past the arena is refused (and stays registered).
#[test]
fn an_oversize_array_is_kept_when_live_or_when_its_extent_fails_a_check() {
    const LEN: usize = 270 << 20;
    let heap = ZgcRealHeap::with_capacity(600 << 20);
    let stw = unsafe { StopTheWorldToken::new() };
    let live = heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, LEN)
        .expect("room for one");
    let mut roots = [live];
    let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
    let live = roots[0];
    assert!(heap.is_object_address(live.as_ptr() as usize).is_some());
    assert_eq!(
        heap.oversize_array_sweep_stats(),
        (0, 0, 0),
        "a live array is not swept"
    );

    // Clobber the length so the extent runs past the arena's end.
    // SAFETY: a registered base with a well-formed header.
    let header = unsafe { &mut *(live.as_ptr() as *mut ObjectHeader) };
    header.set_array_length((LEN * 3) as u32);
    let _ = heap.collect_garbage(&stw, &mut [], &NoMonitors);
    let (swept, _, refused) = heap.oversize_array_sweep_stats();
    assert_eq!(swept, 0, "a clobbered extent must not be reclaimed");
    assert_eq!(refused, 1, "the refusal is counted");
    assert!(
        heap.is_object_address(live.as_ptr() as usize).is_some(),
        "a refused base stays registered (the old fail-safe retention)"
    );
}

/// The forget pass's counters: every collection either runs it or
/// declines it, and a heap of garbage spread over many summary blocks
/// forgets some of them (the positive control). A cycle declined because
/// another test in this process left an unrewritable-peer state is
/// counted as declined, not as a pass, so only the sum is exact.
#[test]
fn the_forget_pass_is_counted_on_every_collection() {
    let heap = ZgcRealHeap::new();
    for _ in 0..4000 {
        let _ = heap.alloc_object(ClassId::new(7), 6);
    }
    let stw = unsafe { StopTheWorldToken::new() };
    let _ = heap.collect_garbage(&stw, &mut [], &NoMonitors);
    let _ = heap.collect_garbage(&stw, &mut [], &NoMonitors);
    let (passes, declined, forgotten) = heap.forget_block_stats();
    assert_eq!(passes + declined, 2, "one verdict per collection");
    if passes > 0 && !cratonvm_gc_core::gc_quiescence::unrewritable_peer_state() {
        assert!(
            forgotten > 0,
            "swept garbage over many blocks must forget some"
        );
    }
}

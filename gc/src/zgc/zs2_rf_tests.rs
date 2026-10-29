//! zs s2/rf (2026-10-14): the per-object sweep frees a DEAD over-long array
//! (`zgc-zs1dg-over-long-array-residual-consumers-20261014.md`, row 1), the
//! low region's screen bounds, and the over-long screen's per-pause memo.
//! A child module of `zgc` so it reaches the heap's private state; kept out of
//! `zgc.rs` (a file several lanes edit).

use super::*;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

/// Payload past 256 MiB: `alloc_size` refuses it, the allocator admits it.
const LEN: usize = 270 << 20;

/// A heap whose sweep takes the PER-OBJECT arm (`sweep_one`): the complement
/// refuses when dead runs are off.
fn per_object_heap(cap: usize, frees: bool, workers: usize) -> ZgcRealHeap {
    let heap = ZgcRealHeap::with_capacity(cap);
    heap.set_tlab_enabled(false);
    heap.set_relocation_enabled(false);
    heap.set_gen_dead_runs_enabled(false);
    heap.sweep_worker_count.store(workers, Ordering::Relaxed);
    heap.counters
        .sweep_one_frees_oversize
        .store(frees, Ordering::Relaxed);
    heap
}

/// Drop one oversize array per iteration; the heap holds two.
fn churn(heap: &ZgcRealHeap, iterations: usize) -> usize {
    let stw = unsafe { StopTheWorldToken::new() };
    let mut served = 0usize;
    for _ in 0..iterations {
        match heap.try_alloc_array(ClassId::new(0), ArrayElementType::Byte, LEN) {
            Some(arr) => {
                // SAFETY: a fresh array with a well-formed header.
                let header = unsafe { &*(arr.as_ptr() as *const ObjectHeader) };
                assert_eq!(
                    ZgcRealHeap::alloc_size(header),
                    None,
                    "fixture precondition"
                );
                served += 1;
            }
            None => break,
        }
        let _ = heap.collect_garbage(&stw, &mut [], &NoMonitors);
    }
    served
}

/// Large-object end, per-object sweep, serial and sharded: every dead array
/// is freed on the default arm; the kill arm retains them (the pre-fix leak:
/// the third allocation fails -- the positive control).
#[test]
fn the_per_object_sweep_frees_a_dead_over_long_array_in_the_large_object_end() {
    for workers in [1usize, 4] {
        let heap = per_object_heap(600 << 20, true, workers);
        assert_eq!(
            churn(&heap, 6),
            6,
            "every iteration served (workers={workers})"
        );
        assert_eq!(
            heap.sweep_one_oversize_freed(),
            6,
            "engagement (workers={workers})"
        );
        let (swept, bytes, refused) = heap.oversize_array_sweep_stats();
        assert_eq!(swept, 6);
        assert!(bytes >= 6 * LEN);
        assert_eq!(refused, 0);
        assert!(heap.registry.bases().is_empty(), "nothing retained");

        let kill = per_object_heap(600 << 20, false, workers);
        let served = churn(&kill, 6);
        assert!(
            served < 6,
            "kill arm must exhaust the heap ({served} served)"
        );
        assert_eq!(kill.sweep_one_oversize_freed(), 0);
    }
}

/// A dead over-long array in the LOW region (served from a low free hole
/// because the large-object end refused it) is freed by the per-object sweep
/// on the default arm and retained on the kill arm.
#[test]
fn the_per_object_sweep_frees_a_dead_over_long_array_in_the_low_region() {
    for frees in [true, false] {
        let heap = ZgcRealHeap::with_capacity(400 << 20);
        heap.set_tlab_enabled(false);
        heap.set_relocation_enabled(false);
        heap.counters
            .sweep_one_frees_oversize
            .store(frees, Ordering::Relaxed);
        let stw = unsafe { StopTheWorldToken::new() };
        // ~330 MB of small (low-region) garbage, then one live object above
        // it so the bump cursor cannot retract over the hole.
        for _ in 0..5500 {
            heap.try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 60_000)
                .expect("garbage fits");
        }
        let top = heap.alloc_object(ClassId::new(1), 0);
        let mut roots = [top];
        let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        let big = heap
            .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, LEN)
            .expect("the low hole holds the array");
        let b = big.as_ptr() as usize;
        let (arena_base, high_floor) = {
            let a = heap.arena.lock();
            (a.base_ptr() as usize, a.high_cursor())
        };
        assert!(
            b - arena_base < high_floor,
            "fixture precondition: the array is in the low region"
        );
        // Dead now; the per-object sweep judges it.
        heap.set_gen_dead_runs_enabled(false);
        let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        if frees {
            assert_eq!(heap.sweep_one_oversize_freed(), 1, "engagement");
            assert!(!heap.registry.contains(b), "freed");
            assert!(
                heap.try_alloc_array(ClassId::new(0), ArrayElementType::Byte, LEN)
                    .is_some(),
                "its bytes are allocatable again"
            );
        } else {
            assert_eq!(heap.sweep_one_oversize_freed(), 0);
            assert!(
                heap.registry.contains(b),
                "kill arm: retained (positive control)"
            );
        }
        assert!(
            heap.registry.contains(roots[0].as_ptr() as usize),
            "the root lives"
        );
    }
}

/// The low region's screen, each refusal on its own (zs s3/rv2: the s2 form
/// of this test clobbered a length whose extent ran past a 64 MiB arena, so
/// both calls were refused by the capacity bound before any of the checks it
/// named). The arena is large enough that the clobbered extent fits under the
/// capacity and below the large-object end; the ~300 MB of garbage after the
/// array puts the low bump cursor past that extent. Then:
/// - a registered neighbour inside the extent: refused (check 3);
/// - POSITIVE CONTROL: with the garbage unregistered, the screen admits it;
/// - the high-end caller (`low_region_ok = false`): refused;
/// - an end past the true low cursor: refused;
/// - a free block overlapping the extent: refused (check 4).
#[test]
fn the_low_region_screen_refuses_swallowed_neighbours_and_the_high_caller() {
    let heap = per_object_heap(640 << 20, true, 1);
    let a = heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 1024)
        .expect("small");
    let mut garbage = Vec::with_capacity(5000);
    for _ in 0..5000 {
        garbage.push(
            heap.try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 60_000)
                .expect("garbage fits")
                .as_ptr() as usize,
        );
    }
    let base = a.as_ptr() as usize;
    let cfg = {
        let arena = heap.arena.lock();
        ZSweepCfg {
            arena_base: arena.base_ptr() as usize,
            high_floor: arena.high_cursor(),
            arena_used: arena.used(),
            zero_header_only: true,
            skip_dead_headers: false,
            merge_dead_runs: false,
            bulk_clearable: true,
            want_dead: false,
            collect_dead_hashes: false,
            region_skip: false,
            gen_on: false,
            promo_age: 0,
            record_live_cells: false,
            reserve_live_hints: 0,
            size_memo: None,
            time_walk: false,
        }
    };
    // SAFETY: a registered base with a well-formed header.
    let header = unsafe { &mut *(base as *mut ObjectHeader) };
    let over = ZgcRealHeap::MAX_PLAUSIBLE_ARRAY_LEN + 1;
    header.set_array_length(over as u32);
    let end = base + ARRAY_DATA_OFFSET + over;
    assert!(
        end - cfg.arena_base < cfg.high_floor,
        "fixture precondition: the clobbered extent is in the low region"
    );
    // The neighbour kept registered for the first case: one inside the extent.
    let inside = garbage
        .iter()
        .copied()
        .find(|&g| g > base + (64 << 20))
        .expect("a garbage array inside the extent");
    assert!(inside < end);
    for &g in &garbage {
        if g != inside {
            heap.registry.remove(g);
        }
    }
    let screen = |header: &ObjectHeader, low_ok: bool| {
        let snap = heap.registry.snapshot();
        let refused0 = heap.oversize_array_sweep_stats().2;
        let got = heap.oversize_array_extent(base, header, &cfg, Some(&snap), low_ok);
        (got, heap.oversize_array_sweep_stats().2 - refused0)
    };
    assert_eq!(
        screen(header, true),
        (None, 1),
        "a swallowed neighbour is refused"
    );
    heap.registry.remove(inside);
    assert_eq!(
        screen(header, true),
        (Some((ARRAY_DATA_OFFSET + over + 7) & !7), 0),
        "positive control: an unobstructed low extent below the cursor is admitted"
    );
    assert_eq!(screen(header, false), (None, 1), "the high-end caller");
    let past = {
        let arena = heap.arena.lock();
        arena.used_low_for_compaction() + 4096 - ARRAY_DATA_OFFSET
    };
    header.set_array_length(past as u32);
    assert_eq!(
        screen(header, true),
        (None, 1),
        "an end past the low cursor"
    );
    header.set_array_length(over as u32);
    assert!(screen(header, true).0.is_some(), "restored");
    heap.arena
        .lock()
        .add_free_block(inside - cfg.arena_base, 60_000);
    assert_eq!(
        screen(header, true),
        (None, 1),
        "a free block inside the extent"
    );
    header.set_array_length(1024);
}

/// The memo: an over-long array live across stop-the-world collections is
/// screened at most once per pause on the default arm (hits > 0); at `=0`
/// every consumer rescans (no hits, more scans: the positive control). Same
/// survivor either way.
#[test]
fn the_oversize_screen_is_scanned_once_per_pause_with_the_memo() {
    let mut scans_by_arm = [0usize; 2];
    for (i, memo) in [true, false].into_iter().enumerate() {
        let heap = ZgcRealHeap::with_capacity(320 << 20);
        heap.set_tlab_enabled(false);
        heap.set_relocation_enabled(true);
        heap.counters
            .oversize_screen_memo_on
            .store(memo, Ordering::Relaxed);
        let stw = unsafe { StopTheWorldToken::new() };
        let big = heap
            .try_alloc_array(
                ClassId::new(0),
                ArrayElementType::Byte,
                ZgcRealHeap::MAX_PLAUSIBLE_ARRAY_LEN + 4096,
            )
            .expect("room");
        let mut roots = vec![big];
        for round in 0..3 {
            // Garbage below a few survivors each round, so the slide has
            // work and its census / partition / rewrite consult the screen.
            for _ in 0..4096 {
                heap.alloc_object(ClassId::new(1), 4);
            }
            for _ in 0..16 {
                roots.push(heap.alloc_object(ClassId::new(1), 0));
            }
            let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            eprintln!(
                "memo={memo} round={round} (hits, scans)={:?}",
                heap.oversize_screen_memo_stats()
            );
        }
        assert_eq!(roots[0], big, "never moved");
        assert!(heap.registry.contains(big.as_ptr() as usize));
        assert!(
            !heap
                .counters
                .oversize_screen_memo_armed
                .load(Ordering::Relaxed),
            "disarmed after the pause"
        );
        assert!(heap.counters.oversize_screen_memo.lock().is_empty());
        let (hits, scans) = heap.oversize_screen_memo_stats();
        assert!(scans > 0, "the screen ran (arm memo={memo})");
        if memo {
            assert!(hits > 0, "memo engaged (hits={hits} scans={scans})");
            assert!(scans <= 3, "at most one scan per pause, got {scans}");
        } else {
            assert_eq!(hits, 0);
        }
        scans_by_arm[i] = scans;
    }
    assert!(
        scans_by_arm[1] > scans_by_arm[0],
        "the kill arm rescans per consumer: {scans_by_arm:?}"
    );
}

/// Row 3 of the dg residual page, which is a soundness hole on generational
/// ZGC with element cards: the store side cards an element granule of any
/// reference array of `len >= Z_ELEMENT_CARD_MIN_LEN`, and the young cycle's
/// `scan_element_granule` used to drop the card of an array past
/// `MAX_PLAUSIBLE_ARRAY_LEN` without rooting its young target. Default arm:
/// the target is rooted; with the screen off
/// (`rewrite_oversize_arrays = false`) the refused-screen arm still roots it
/// conservatively, and only with that arm off too is the card dropped and
/// nothing rooted -- the pre-fix behaviour, the positive control. Needs a
/// 2.6 GB arena: host row H1,
/// `cargo test -p cratonvm-gc --lib zs2_rf_tests -- --ignored`.
#[test]
#[ignore = "allocates a 2 GiB reference array"]
fn an_over_long_reference_arrays_element_card_roots_its_young_target() {
    const OVER_LEN: usize = ZgcRealHeap::MAX_PLAUSIBLE_ARRAY_LEN + 4096;
    // `refused_scan`: CRATONVM_ZGC_ELEMENT_CARD_REFUSED_SCAN (2026-10-17). With
    // the screen off it roots the target conservatively; only with both off is
    // the card dropped (the pre-fix behaviour, the positive control).
    for (on, refused_scan) in [(true, true), (false, true), (false, false)] {
        let heap = ZgcRealHeap::with_capacity(2600 << 20);
        heap.set_tlab_enabled(false);
        heap.counters
            .rewrite_oversize_arrays
            .store(on, Ordering::Relaxed);
        heap.counters
            .element_card_refused_scan
            .store(refused_scan, Ordering::Relaxed);
        let target = heap.alloc_object(ClassId::new(1), 0);
        let arr = heap
            .try_alloc_array(ClassId::new(2), ArrayElementType::Reference, OVER_LEN)
            .expect("room for one over-long Object[]");
        let last = OVER_LEN - 1;
        heap.set_array_element(arr, last, Value::Object(Some(target)))
            .expect("in bounds");
        let a = arr.as_ptr() as usize;
        assert_eq!(
            heap.large_reference_array_len(a),
            on.then_some(OVER_LEN),
            "arm on={on}"
        );
        let slot = a + ARRAY_DATA_OFFSET + last * cratonvm_types::ref_element_size();
        let mut roots = Vec::new();
        let mut last_array = None;
        // Promotion age above the target's age: the target is young.
        let keep = heap.scan_element_granule(slot & !7, u8::MAX, &mut last_array, &mut roots);
        if on || refused_scan {
            assert!(keep, "the card is kept (on={on})");
            assert_eq!(
                roots,
                vec![target.as_ptr() as usize],
                "the young target is rooted (on={on})"
            );
            assert_eq!(heap.element_card_refused_scans(), usize::from(!on));
        } else {
            assert!(!keep, "kill arm: the card is dropped (positive control)");
            assert!(roots.is_empty(), "kill arm: nothing rooted");
        }
    }
}

/// zs s3/rv2: a memoised POSITIVE is re-validated on the hit. Inside one
/// armed pause the screen answers `Some(size)` for a live over-long array;
/// once its base leaves the registry (the sweep freed it) the next query
/// answers `None`, not the memoised size. Before the fix the hit returned the
/// stale `Some`.
#[test]
fn a_memoised_positive_is_revalidated_after_the_base_leaves_the_registry() {
    let heap = per_object_heap(640 << 20, true, 1);
    heap.counters
        .oversize_screen_memo_on
        .store(true, Ordering::Relaxed);
    heap.counters
        .rewrite_oversize_arrays
        .store(true, Ordering::Relaxed);
    let a = heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 1024)
        .expect("small");
    let base = a.as_ptr() as usize;
    // SAFETY: a registered base with a well-formed header.
    let header = unsafe { &mut *(base as *mut ObjectHeader) };
    let over = ZgcRealHeap::MAX_PLAUSIBLE_ARRAY_LEN + 1;
    header.set_array_length(over as u32);
    let size = (ARRAY_DATA_OFFSET + over + 7) & !7; // 8-byte rounded
    {
        let _memo = OversizeScreenMemoGuard::arm(&heap);
        assert_eq!(
            heap.oversize_live_array_extent(base, header),
            Some(size),
            "positive control: the live screen sizes the array"
        );
        let (hits0, _) = heap.oversize_screen_memo_stats();
        assert_eq!(heap.oversize_live_array_extent(base, header), Some(size));
        assert_eq!(heap.oversize_screen_memo_stats().0, hits0 + 1, "a memo hit");
        heap.registry.remove(base);
        assert_eq!(
            heap.oversize_live_array_extent(base, header),
            None,
            "a freed base is not answered from the memo"
        );
        assert_eq!(
            heap.oversize_screen_memo_stats().0,
            hits0 + 2,
            "the answer came through the memo's hit path, which re-checks"
        );
    }
    header.set_array_length(1024);
}

/// zs s2/rf residual (item 1) and zs s3/rv2 §7 (F1 had only an ignored 2.6 GB
/// test): an element card on a reference array whose header claims a length
/// past `MAX_PLAUSIBLE_ARRAY_LEN` that the live screen REFUSES -- here the
/// claimed extent runs past the arena, so the screen cannot size it -- used to
/// be dropped without rooting its young target. In-process, with a forged
/// header on a small `Object[]`: the default arm scans the granule's own slots
/// conservatively and keeps the card; the kill arm
/// (`element_card_refused_scan = false`) drops it (the pre-fix behaviour, the
/// positive control). A granule inside the header is never scanned.
#[test]
fn a_screen_refused_over_long_reference_arrays_element_card_roots_its_young_target() {
    for on in [true, false] {
        let heap = ZgcRealHeap::with_capacity(64 << 20);
        heap.set_tlab_enabled(false);
        heap.counters
            .element_card_refused_scan
            .store(on, Ordering::Relaxed);
        let target = heap.alloc_object(ClassId::new(1), 0);
        let arr = heap
            .try_alloc_array(ClassId::new(2), ArrayElementType::Reference, 1024)
            .expect("small Object[]");
        let idx = 37;
        heap.set_array_element(arr, idx, Value::Object(Some(target)))
            .expect("in bounds");
        let a = arr.as_ptr() as usize;
        // SAFETY: a registered base with a well-formed header; restored below.
        let header = unsafe { &mut *(a as *mut ObjectHeader) };
        header.set_array_length((ZgcRealHeap::MAX_PLAUSIBLE_ARRAY_LEN + 1) as u32);
        assert_eq!(
            heap.large_reference_array_len(a),
            None,
            "precondition: the live screen refuses (extent past the arena)"
        );
        let slot = a + ARRAY_DATA_OFFSET + idx * cratonvm_types::ref_element_size();
        let mut roots = Vec::new();
        let mut last_array = None;
        // Promotion age above the target's age: the target is young.
        let keep = heap.scan_element_granule(slot & !7, u8::MAX, &mut last_array, &mut roots);
        assert!(last_array.is_none(), "a refused array is never cached");
        if on {
            assert!(keep, "the card is kept");
            assert_eq!(
                roots,
                vec![target.as_ptr() as usize],
                "the target is rooted"
            );
            assert_eq!(heap.element_card_refused_scans(), 1, "engagement");
        } else {
            assert!(!keep, "kill arm: the card is dropped (positive control)");
            assert!(roots.is_empty(), "kill arm: nothing rooted");
            assert_eq!(heap.element_card_refused_scans(), 0);
        }
        // A granule inside the array header is not an element: never scanned.
        let mut roots = Vec::new();
        assert!(!heap.scan_element_granule(a + 8, u8::MAX, &mut None, &mut roots));
        assert!(roots.is_empty());
        header.set_array_length(1024);
    }
}

use super::*;

struct NoMonitors;
impl cratonvm_gc_core::collector::MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn non_moving_cycle(heap: &GenerationalHeap, roots: &mut Vec<ObjectRef>) -> GcResult {
    cratonvm_gc_core::gc_quiescence::publish_moving_young_enabled(false);
    cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
    cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
    cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
    cratonvm_gc_core::gc_quiescence::enter();
    // SAFETY: these tests run the heap single-threaded.
    let token = unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() };
    let result = heap.collect_garbage(&token, roots, &NoMonitors);
    cratonvm_gc_core::gc_quiescence::leave();
    result
}

fn moving_cycle(heap: &GenerationalHeap, roots: &mut Vec<ObjectRef>) -> GcResult {
    cratonvm_gc_core::gc_quiescence::publish_moving_young_enabled(true);
    cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
    cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
    cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
    // SAFETY: these tests run the heap single-threaded.
    let token = unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() };
    heap.collect_garbage(&token, roots, &NoMonitors)
}

/// `sweep_survivor_mark` leaves exactly the word the two RMWs it replaced
/// (`clear_gc_flags(GC_FLAG_MARKED)`, then `set_gc_age(gc_age() + 1)`)
/// leave, for every age and every flag mix the sweep can meet, and
/// reports the age they leave.
#[test]
fn f2y_the_folded_survivor_word_is_the_two_rmws_word() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
    let o = heap.alloc_object(ClassId::new(3), 1);
    let h = heap.get_header(o);
    let original = h.mark_word.load(Ordering::Relaxed);
    for age in 0..=cratonvm_types::MAX_GC_AGE {
        for flags in [
            0u8,
            GC_FLAG_MARKED,
            GC_FLAG_OLD_GEN,
            GC_FLAG_MARKED | GC_FLAG_OLD_GEN,
        ] {
            h.mark_word.store(original, Ordering::Relaxed);
            h.set_gc_flags(flags);
            h.set_gc_age(age);
            let m = h.mark_word.load(Ordering::Relaxed);
            h.clear_gc_flags(GC_FLAG_MARKED);
            h.set_gc_age(h.gc_age().saturating_add(1));
            let want = h.mark_word.load(Ordering::Relaxed);
            let (word, new_age) = sweep_survivor_mark(m);
            assert_eq!(word, want, "age {age} flags {flags:#x}");
            assert_eq!(new_age, h.gc_age(), "age {age} flags {flags:#x}");
        }
    }
    h.mark_word.store(original, Ordering::Relaxed);
}

/// The tenuring age table of a non-moving sweep counts the survivors the
/// MAIN walk ages in place (header-marked ones), not only the selective
/// walk's deferred bumps: one header-marked ~1 KiB survivor is above a
/// `TargetSurvivorRatio=1` target on a 1 MiB semi-space (eden 512 KiB,
/// 81 words), so the next threshold is HotSpot's 1 (promotion age 2).
/// Before gcf f2/y `adapt` ran before the main walk and the threshold
/// stayed at the ceiling. Positive control: the same heap without the
/// header mark keeps the ceiling.
#[test]
fn f2y_header_marked_sweep_survivors_reach_the_age_table() {
    for header_mark in [false, true] {
        let heap = GenerationalHeap::with_sizes(1024 * 1024, 1024 * 1024);
        heap.set_tenuring_config(&TenuringConfig {
            max_tenuring_threshold: Some(15),
            target_survivor_ratio: Some(1),
            ..TenuringConfig::default()
        });
        assert_eq!(heap.tenuring_threshold(), 16);
        let holder = heap.alloc_object(ClassId::new(1), 1);
        // 64 slots: ~1 KiB, far above the 648-byte target on its own.
        let big = heap.alloc_object(ClassId::new(2), 64);
        if header_mark {
            heap.get_header(big).add_gc_flags(GC_FLAG_MARKED);
        }
        let mut roots = vec![holder];
        let result = non_moving_cycle(&heap, &mut roots);
        assert_eq!(result.stats.objects_copied, 0, "the non-moving sweep ran");
        if header_mark {
            let h = heap.get_header(big);
            assert_eq!(
                h.gc_flags() & GC_FLAG_MARKED,
                0,
                "the sweep cleared the mark"
            );
            assert_eq!(h.gc_age(), 1, "and aged the header-marked survivor once");
            assert_eq!(
                heap.tenuring_threshold(),
                2,
                "the header-marked survivor's bytes did not reach the age table"
            );
        } else {
            assert_eq!(
                heap.tenuring_threshold(),
                16,
                "control: one small side-marked survivor stays under the target"
            );
        }
        assert_eq!(heap.tenuring_census()[0], 1, "one adaptation per sweep");
    }
}

/// The defragmentation gate's predicate: at least half of the allocatable
/// bytes in sub-TLAB holes.
#[test]
fn f2y_young_space_fragmented_is_half_the_allocatable_bytes_in_sub_tlab_holes() {
    assert!(
        !young_space_fragmented(0, 0, 0),
        "an exhausted arena is not 'fragmented'"
    );
    assert!(young_space_fragmented(500, 1000, 0));
    assert!(!young_space_fragmented(499, 1000, 0));
    assert!(young_space_fragmented(1000, 1000, 1000));
    assert!(
        !young_space_fragmented(1000, 1000, 1001),
        "a large bump tail is dense space"
    );
    // The small bytes are part of the free list; a larger count is capped.
    assert!(!young_space_fragmented(5000, 100, 1000));
}


/// gch h2/yg (`gcg-g5yg-proposal-defrag-gate-counts-large-request-misses`):
/// the miss rule, pure.
#[test]
fn gch_h2yg_a_large_miss_on_a_roomy_free_list_is_a_fragmentation_miss() {
    let k = 1024;
    assert!(
        young_miss_is_fragmentation(64 * k, 512 * k),
        "64 KiB, 8x free"
    );
    assert!(young_miss_is_fragmentation(1024 * k, 16 * 1024 * k));
    assert!(
        !young_miss_is_fragmentation(64 * k, 512 * k - 8),
        "under 8x: young is full"
    );
    assert!(
        !young_miss_is_fragmentation(64 * k - 8, 64 * 1024 * k),
        "a small request"
    );
    assert!(!young_miss_is_fragmentation(64 * k, 0));
}


/// The parallel evacuator (gcf f2/y; the spin-before-park arm it was
/// written for is deleted by gcg g4/yg, the park-at-once drain keeps the
/// test): a 64 x 96 DAG, most nodes with two parents, copied exactly
/// once over three consecutive parallel cycles on eight workers.
#[test]
fn f2y_the_spinning_parallel_evacuator_copies_a_wide_dag_exactly_once() {
    let heap = GenerationalHeap::with_sizes(64 * 1024 * 1024, 16 * 1024 * 1024);
    const WIDTH: usize = 64;
    const DEPTH: usize = 96;
    let mut layers: Vec<Vec<ObjectRef>> = Vec::with_capacity(DEPTH);
    for d in 0..DEPTH {
        let layer: Vec<ObjectRef> = (0..WIDTH)
            .map(|w| {
                let o = heap.alloc_object(ClassId::new(11), 3);
                heap.set_field(o, 2, Value::Int((d * WIDTH + w) as i32));
                o
            })
            .collect();
        layers.push(layer);
    }
    for d in 0..DEPTH - 1 {
        for w in 0..WIDTH {
            heap.set_field(layers[d][w], 0, Value::Object(Some(layers[d + 1][w])));
            heap.set_field(
                layers[d][w],
                1,
                Value::Object(Some(layers[d + 1][(w + 1) % WIDTH])),
            );
        }
    }
    let mut roots = layers[0].clone();
    let before = crate::gen_evac::par_evac_cycles_on_this_thread();
    for cycle in 0..3 {
        let result = cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_GC_PAR_THREADS", Some("8")),
                ("CRATONVM_GC_PAR_EVAC", Some("1")),
            ],
            || moving_cycle(&heap, &mut roots),
        );
        assert_eq!(result.stats.objects_copied, WIDTH * DEPTH, "cycle {cycle}");
        let mut seen: FxHashSet<usize> = FxHashSet::default();
        let mut ids: Vec<i32> = Vec::new();
        let mut queue: Vec<ObjectRef> = roots.clone();
        while let Some(o) = queue.pop() {
            if !seen.insert(o.as_ptr() as usize) {
                continue;
            }
            ids.push(heap.get_field(o, 2).as_int().expect("payload"));
            for slot in 0..2 {
                if let Value::Object(Some(child)) = heap.get_field(o, slot) {
                    queue.push(child);
                }
            }
        }
        ids.sort_unstable();
        assert_eq!(
            ids,
            (0..(WIDTH * DEPTH) as i32).collect::<Vec<_>>(),
            "cycle {cycle}"
        );
    }
    assert!(
        crate::gen_evac::par_evac_cycles_on_this_thread() > before,
        "the parallel evacuator never engaged; the test measured the serial one"
    );
}

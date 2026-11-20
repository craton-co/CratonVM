use super::*;

struct NoMonitors;
impl cratonvm_gc_core::collector::MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// The gen-alloc line carries the young-to-young copy totals the
/// adaptive-tenuring gate's item 4 now reads, BEFORE the tenuring keys
/// (`e2y_the_gen_alloc_line_carries_the_tenuring_gate_counters` pins the
/// line's last key).
#[test]
fn f1y_the_gen_alloc_line_carries_the_young_copy_totals() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 1024 * 1024);
    let line = heap.alloc_path_census_line();
    let copied = line
        .find("gen_objects_copied_young=0 gen_bytes_copied_young=0 ")
        .unwrap_or_else(|| panic!("young copy totals missing from {line}"));
    let promoted = line
        .find("gen_objects_promoted=")
        .expect("promotion totals");
    assert!(copied < promoted, "{line}");
}

/// A pinned in-place cycle whose dead bytes pass the parallel zeroing
/// bound zeroes them on the heap's persistent pool (positive control: the
/// SERIAL pinned arm builds no pool of its own, so before gcf f1/y the
/// pool stayed unbuilt and the zeroing spawned scoped threads), and every
/// dead byte away from the pinned page reads zero afterwards.
#[test]
fn f1y_the_pinned_rebuild_zeroes_its_dead_bytes_on_the_heaps_pool() {
    if gc_flags().gen_pinned_young_copy_parallel {
        // The parallel pinned arm builds the pool for its own drain.
        return;
    }
    let heap = GenerationalHeap::with_sizes(64 * 1024 * 1024, 8 * 1024 * 1024)
        .with_fixed_promotion_age();
    let a = heap.alloc_object(ClassId::new(7), 2);
    heap.set_field(a, 1, Value::Int(0xA));
    // 20 MiB of garbage: past `CRATONVM_GC_PAR_MIN_BYTES`' default
    // (16 MiB), so the cycle asks for more than one worker on any
    // multi-core host, and far past the 1 MiB parallel-zeroing bound.
    let n = (20 * 1024 * 1024) / 48;
    let mut probe = 0usize;
    for i in 0..n {
        let g = heap
            .try_alloc_object(ClassId::new(1), 2)
            .expect("the nursery has room");
        if i == n / 2 {
            probe = g.as_ptr() as usize;
        }
    }
    let b = heap.alloc_object(ClassId::new(7), 2);
    heap.set_field(b, 1, Value::Int(0xB));
    heap.set_field(a, 0, Value::Object(Some(b)));
    let a0 = a.as_ptr() as usize;
    let threads = cratonvm_gc_core::young_mark::young_gc_threads(heap.young_from_used());
    // SAFETY: a live young object's first word, read before the cycle.
    assert_ne!(
        unsafe { (probe as *const u64).read() },
        0,
        "the probe object has a header"
    );

    cratonvm_gc_core::gc_quiescence::publish_moving_young_enabled(true);
    cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
    cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
    cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
    let pinned_before = heap.pinned_young_census()[0];
    let mut roots = vec![a, b];
    let words = [a0];
    let _ =
        heap.collect_garbage_inner_with_pins(&mut roots, &[], &NoMonitors, Some(&words[..]));

    assert_eq!(
        heap.pinned_young_census()[0],
        pinned_before + 1,
        "the pinned cycle ran"
    );
    assert_eq!(roots[0].as_ptr() as usize, a0, "the pinned object stays");
    assert_eq!(heap.get_field(roots[1], 1).as_int(), Some(0xB));
    // SAFETY: from-space is not reset or decommitted by a pinned cycle;
    // the probe's bytes are dead, below the starting cursor, committed.
    let words: Vec<u64> = (0..6)
        .map(|k| unsafe { ((probe + 8 * k) as *const u64).read() })
        .collect();
    assert!(
        words.iter().all(|&w| w == 0),
        "a dead object's bytes were not zeroed: {words:x?}"
    );
    if threads > 1 {
        assert!(
            heap.evac_pool.get().is_some(),
            "a {threads}-worker pinned rebuild did not zero on the heap's pool"
        );
    }
}

/// The non-moving sweep's folded header writes (gcf f1/y, item 4 of the
/// pin10 round-three proposal): a young survivor still ages by exactly
/// one per non-moving cycle, never past `MAX_GC_AGE`, and a child the
/// selective pass promotes lands old with `GC_FLAG_OLD_GEN` set,
/// `GC_FLAG_MARKED` clear and its payload intact. (The root holder itself
/// is a root value, which the selective pass pins, so it stays young and
/// keeps aging; the promotion half is checked when it happens.)
#[test]
fn f1y_the_sweep_ages_by_one_and_promotes_with_the_old_flag() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024).with_fixed_promotion_age();
    let holder = heap.alloc_object(ClassId::new(1), 2);
    let child = heap.alloc_object(ClassId::new(1), 1);
    heap.set_field(child, 0, Value::Int(0x5EED));
    heap.set_field(holder, 0, Value::Object(Some(child)));
    let mut roots = vec![holder];
    let mut last_age = heap.get_header(holder).gc_age();
    for cycle in 0..20 {
        cratonvm_gc_core::gc_quiescence::publish_moving_young_enabled(false);
        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
        cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
        cratonvm_gc_core::gc_quiescence::enter();
        // SAFETY: this test runs the heap single-threaded.
        let token = unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() };
        let result = heap.collect_garbage(&token, &mut roots, &NoMonitors);
        cratonvm_gc_core::gc_quiescence::leave();
        assert_eq!(
            result.stats.objects_copied, 0,
            "cycle {cycle}: the non-moving sweep ran"
        );
        let age = heap.get_header(roots[0]).gc_age();
        assert!(age <= cratonvm_types::MAX_GC_AGE);
        // By one, or not at all once tenurable: the walk skips the bump
        // of an `aged` survivor, and the selective pass pins a root value
        // instead of promoting it.
        assert!(
            age == last_age.saturating_add(1).min(cratonvm_types::MAX_GC_AGE)
                || (age == last_age
                    && should_tenure(age, false, heap.effective_promotion_age())),
            "cycle {cycle}: age {last_age} -> {age}"
        );
        if cycle == 0 {
            assert_eq!(age, 1, "the first non-moving cycle ages its survivor");
        }
        last_age = age;
        let c = match heap.get_field(roots[0], 0) {
            Value::Object(Some(c)) => c,
            other => panic!("cycle {cycle}: the holder lost its child: {other:?}"),
        };
        assert_eq!(heap.get_field(c, 0).as_int(), Some(0x5EED), "cycle {cycle}");
        if !heap.is_in_young(c.as_ptr()) {
            let flags = heap.get_header(c).gc_flags();
            assert_ne!(
                flags & GC_FLAG_OLD_GEN,
                0,
                "a promoted copy carries GC_FLAG_OLD_GEN"
            );
            assert_eq!(
                flags & GC_FLAG_MARKED,
                0,
                "a promoted copy is not header-marked"
            );
        }
    }
}

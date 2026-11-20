#[allow(unused_imports)]
use super::*;

    /// A HUMONGOUS object WITH A FINALIZER must survive its own unreachability
    /// until `finalize()` has run.
    ///
    /// This is the exact shape the wholesale `finalizer_pause` decline existed
    /// for, and the reason removing that decline needed a replacement rather
    /// than a deletion: the object is unreachable (that is why it is being
    /// finalized), a humongous span is never in the CSet so Phase 3.5 never
    /// resurrects it, and nothing else in `eager_reclaim_humongous_locked`
    /// would name it live. Naming it from `finalizer_addrs_this_pause` is what
    /// keeps it, and this test is what keeps that true.
    ///
    /// The gate it replaced was not free: `finalizer_pause` is set for ANY
    /// registered not-yet-enqueued finalizable object, so a single live one
    /// disabled eager humongous reclaim for the whole process — measured on H2
    /// as 830 of 1024 regions humongous and 76 787 young pauses per 900 s,
    /// against 26 and 88 without it.
    #[test]
    pub fn a_humongous_object_awaiting_finalization_is_never_eagerly_reclaimed() {
        let gc = make_collector();
        let doomed = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        assert_eq!(gc.count_regions(RegionType::HumongousStart), 1);
        let addr = doomed.as_ptr() as usize;
        // Unreachable from any root, and registered for finalization: exactly
        // the state the reference processor hands to the collector.
        let _churn = gc.alloc_object(ClassId::new(1), 1);
        let mut roots: Vec<ObjectRef> = vec![];
        let (_result, _dead) =
            gc.collect_garbage_with_finalizers(&stw(), &mut roots, &[addr], &NoopMonitors);

        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            1,
            "a humongous object registered for finalization must not be reclaimed \
             before finalize() runs: it is unreachable by construction, and a \
             humongous span is never in the CSet, so nothing else names it live"
        );
    }

    /// ...and the other half of that change: a humongous span that is merely
    /// unreachable, on a pause that HAS finalizer candidates, is still
    /// reclaimed. The old gate declined outright here, which is what disabled
    /// the feature for any process holding one finalizable object.
    #[test]
    pub fn an_unrelated_finalizer_candidate_no_longer_suppresses_eager_reclaim() {
        let gc = make_collector();
        let keeper = gc.alloc_object(ClassId::new(1), 1);
        let _dead_span = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        assert_eq!(gc.count_regions(RegionType::HumongousStart), 1);

        let _churn = gc.alloc_object(ClassId::new(1), 1);
        let mut roots: Vec<ObjectRef> = vec![keeper];
        // A finalizer candidate that is NOT the humongous span.
        let (_result, _dead) = gc.collect_garbage_with_finalizers(
            &stw(),
            &mut roots,
            &[keeper.as_ptr() as usize],
            &NoopMonitors,
        );

        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            0,
            "an unreachable humongous span must still be reclaimed on a pause \
             that merely HAS finalizer candidates -- declining here is what left \
             830 of 1024 regions humongous on the H2 workload"
        );
    }

    /// A JNI pin is a promise the object will not move OR go away. Cleanup
    /// already honours it; the pause path must too.
    #[test]
    pub fn a_pinned_humongous_span_is_never_eagerly_reclaimed() {
        let gc = make_collector();
        let h = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        let idx = gc.lookup_region_for_addr(h.as_ptr() as usize).unwrap();
        gc.pin_region(idx);
        let _churn = gc.alloc_object(ClassId::new(1), 1);

        let mut roots: Vec<ObjectRef> = vec![];
        gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            1,
            "a pinned span must survive even with nothing referencing it"
        );
    }

    /// A dead object must not be its own reason to live. Without the
    /// self-reference exclusion in `collect_outgoing_cross_region_edges`, any
    /// humongous object holding a pointer into itself would be immortal.
    #[test]
    pub fn a_self_referential_dead_humongous_span_is_still_reclaimed() {
        let gc = make_collector();
        let h = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 100_000);
        assert!(gc.is_humongous(h));
        gc.set_array_element(h, 0, Value::Object(Some(h))).unwrap();
        let _churn = gc.alloc_object(ClassId::new(1), 1);

        let mut roots: Vec<ObjectRef> = vec![];
        gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            0,
            "a span that only points at itself is unreachable"
        );
    }

    /// A span reachable only from another humongous span is still reachable.
    /// Both are dead here, so this also documents the deliberate
    /// over-approximation: the holder goes this pause, the held one goes the
    /// next.
    #[test]
    pub fn a_humongous_span_held_by_another_humongous_span_survives_the_pause() {
        let gc = make_collector();
        let target = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        gc.set_array_element(target, 0, Value::Long(0xB0B)).unwrap();
        let holder = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 100_000);
        gc.set_array_element(holder, 0, Value::Object(Some(target)))
            .unwrap();
        assert_eq!(gc.count_regions(RegionType::HumongousStart), 2);

        let mut roots: Vec<ObjectRef> = vec![holder];
        let _churn = gc.alloc_object(ClassId::new(1), 1);
        gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            2,
            "a span referenced from another span must not be freed under it"
        );
        assert_eq!(gc.get_array_element(target, 0).unwrap(), Value::Long(0xB0B));
    }

    /// A humongous object with an unrun finalizer must survive the pause.
    ///
    /// This one nearly got away. The obvious gate — "are there pending
    /// finalizer roots?" — is useless where the reclaim runs, because Phase 3.5
    /// consumes that list with `std::mem::take` long before Phase 5, so the
    /// test passes unconditionally. And the mechanism that normally protects
    /// finalizable garbage does not apply here either: Phase 3.5 resurrects
    /// dead finalizable objects *out of the CSet*, and a humongous object is
    /// never in a CSet, so it is never resurrected and never appears in
    /// `resurrected_finalizers`. Without the pause-scoped flag this frees the
    /// object out from under a `finalize()` that has not run.
    #[test]
    pub fn a_humongous_span_awaiting_finalization_is_not_eagerly_reclaimed() {
        let gc = make_collector();
        let h = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        let addr = h.as_ptr() as usize;
        let _churn = gc.alloc_object(ClassId::new(1), 1);

        let mut roots: Vec<ObjectRef> = vec![];
        let _ = gc.collect_garbage_with_finalizers(&stw(), &mut roots, &[addr], &NoopMonitors);

        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            1,
            "a span awaiting finalize() must not be freed"
        );

        // ...and the flag is per-pause, so the NEXT ordinary collection does
        // reclaim it. A latch that never cleared would disable eager reclaim
        // for the rest of the process after one finalizer pause.
        let _churn2 = gc.alloc_object(ClassId::new(1), 1);
        gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            0,
            "the finalizer gate must be scoped to its own pause"
        );
    }

    /// Same guarantee on the mixed driver, which frees Old regions as well as
    /// young ones and reaches the reclaim through a different call site.
    #[test]
    pub fn a_mixed_pause_reclaims_and_preserves_humongous_spans_the_same_way() {
        let gc = make_collector();
        let live = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        gc.set_array_element(live, 0, Value::Long(0xA11AE)).unwrap();
        let _dead = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        assert_eq!(gc.count_regions(RegionType::HumongousStart), 2);

        let _churn = gc.alloc_object(ClassId::new(1), 1);
        let mut roots: Vec<ObjectRef> = vec![live];
        gc.mixed_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            1,
            "the mixed driver must reclaim the unreferenced span and keep the rooted one"
        );
        assert_eq!(
            gc.get_array_element(roots[0], 0).unwrap(),
            Value::Long(0xA11AE)
        );
    }

    #[test]
    pub fn cleanup_reclaims_dead_humongous_span_and_keeps_marked_span() {
        let gc = make_collector();
        let live = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        let _dead = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);

        gc.set_array_element(live, 199_999, Value::Long(0x1234_5678))
            .unwrap();
        assert_eq!(gc.count_regions(RegionType::HumongousStart), 2);
        assert_eq!(gc.count_regions(RegionType::HumongousContinuation), 2);

        gc.start_concurrent_mark(&stw());
        gc.remark(&stw(), &[live]);
        assert!(gc.concurrent_mark_step(usize::MAX));
        gc.cleanup(&stw());

        assert_eq!(gc.count_regions(RegionType::HumongousStart), 1);
        assert_eq!(gc.count_regions(RegionType::HumongousContinuation), 1);
        assert!(gc.is_humongous(live));
        assert_eq!(
            gc.get_array_element(live, 199_999).unwrap(),
            Value::Long(0x1234_5678)
        );
    }

    /// r12w1 g1store: cleanup keeps an UNMARKED humongous span that a live
    /// compiled frame pins. The pin is what a rejected interior word publishes
    /// (the span base, `band_reject_pin`); the word is never a mark root, so
    /// the bitmap alone reads the span as dead.
    #[test]
    pub fn r12w1_cleanup_keeps_a_humongous_span_a_compiled_frame_pins() {
        let gc = make_collector();
        let held = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        gc.set_array_element(held, 199_999, Value::Long(0x7E57))
            .unwrap();
        let interior = held.as_ptr() as usize + (5 << 18);

        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        cratonvm_gc_core::gc_quiescence::clear_pinned_jit_roots();
        let owed = gc.band_reject_pin(interior).expect("owed pin").addr;
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[owed]);
        cratonvm_gc_core::gc_quiescence::enter();
        gc.start_concurrent_mark(&stw());
        gc.remark(&stw(), &[]);
        assert!(gc.concurrent_mark_step(usize::MAX));
        gc.cleanup(&stw());
        cratonvm_gc_core::gc_quiescence::leave();
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[]);

        {
            assert_eq!(
                gc.count_regions(RegionType::HumongousStart),
                1,
                "a span a live frame pins must survive cleanup"
            );
            assert_eq!(
                gc.get_array_element(held, 199_999).unwrap(),
                Value::Long(0x7E57)
            );
        }
    }


    // C2 (round-12 gc): a humongous array spans multiple non-contiguous region
    // buffers. Element access at HIGH indices must land in the owning
    // continuation region — never OOB in unrelated heap memory. Round-trip
    // values at low / boundary / high indices and confirm they read back.
    #[test]
    pub fn humongous_int_array_multi_region_roundtrip() {
        let gc = make_collector();
        // 1 MB regions → usable payload ≈ (1MB-40)/4 ≈ 262133 ints/region.
        // 400_000 ints (~1.6 MB data) spans at least two regions.
        let n = 400_000;
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        assert!(gc.count_regions(RegionType::HumongousContinuation) >= 1);
        // Write a recognizable pattern at indices in both the start region and
        // the continuation region(s).
        for &i in &[0usize, 1, 262_000, 262_133, 262_134, 300_000, n - 1] {
            gc.set_array_element(arr, i, Value::Int((i as i32).wrapping_mul(7) ^ 0x5a5a))
                .unwrap();
        }
        for &i in &[0usize, 1, 262_000, 262_133, 262_134, 300_000, n - 1] {
            assert_eq!(
                gc.get_array_element(arr, i).unwrap().as_int(),
                Some((i as i32).wrapping_mul(7) ^ 0x5a5a),
                "mismatch at index {i}",
            );
        }
        // OOB index is rejected, not a wild write.
        assert!(gc.set_array_element(arr, n, Value::Int(1)).is_err());
        assert!(gc.get_array_element(arr, n).is_err());
    }

    /// `may_be_humongous` is what licenses every accessor to skip the `regions`
    /// lock, so it must never answer `false` for an object the allocator
    /// actually placed in a `HumongousStart` region. If it ever does, that
    /// object's field/element access takes the FLAT path and reads or writes
    /// the wrong memory — silently.
    ///
    /// This pins the predicate against the allocator's own admission rule
    /// rather than against a hardcoded size, which is the only way the two can
    /// be kept in agreement as `region_size` or the humongous threshold move.
    #[test]
    pub fn may_be_humongous_covers_everything_the_allocator_made_humongous() {
        let gc = make_collector();
        let region_size = gc.config.region_size;
        // Walk element counts across the humongous threshold from well below to
        // well above, including the exact boundary, and check the predicate
        // against the region classification the allocator chose.
        let per_elem = cratonvm_gc_core::heap::element_byte_size(ArrayElementType::Int);
        let boundary_elems = (region_size / 2).saturating_sub(ARRAY_DATA_OFFSET) / per_elem;
        for delta in [
            -(boundary_elems as isize) / 2,
            -2,
            -1,
            0,
            1,
            2,
            boundary_elems as isize,
        ] {
            let n = ((boundary_elems as isize) + delta).max(1) as usize;
            let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
            let total =
                ARRAY_DATA_OFFSET + cratonvm_gc_core::heap::array_data_size(n, ArrayElementType::Int).unwrap();
            let really_humongous = gc.is_humongous(arr);
            assert!(
                !really_humongous || gc.may_be_humongous(total),
                "n={n} total={total} region_size={region_size}: the allocator made this \
                 humongous but may_be_humongous said no, so every accessor would take \
                 the flat path over a region-spanning object"
            );
            // And it must still round-trip through the accessors, whichever
            // path the predicate routed it down.
            for &i in &[0usize, n / 2, n - 1] {
                gc.set_array_element(arr, i, Value::Int(i as i32 ^ 0x1234))
                    .unwrap();
                assert_eq!(
                    gc.get_array_element(arr, i).unwrap().as_int(),
                    Some(i as i32 ^ 0x1234),
                    "n={n} index={i} did not round-trip"
                );
            }
        }
    }

    /// The same agreement for an ORDINARY object's field slots: a plain object
    /// is never humongous, so `set_field`/`get_field` must round-trip on the
    /// lock-free path.
    #[test]
    pub fn an_ordinary_object_field_round_trips_without_the_regions_lock() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(0), 8);
        assert!(!gc.is_humongous(obj));
        let total = object_total_size(gc.get_header(obj));
        assert!(
            !gc.may_be_humongous(total),
            "an 8-field object must take the lock-free accessor path"
        );
        for i in 0..8usize {
            gc.set_field(obj, i, Value::Int(i as i32 * 31));
        }
        for i in 0..8usize {
            assert_eq!(gc.get_field(obj, i).as_int(), Some(i as i32 * 31));
        }
    }

    // G1 SIGSEGV regression (CpuOnlyBench / gpu-bench-cpu): a humongous array
    // must be ONE physically-contiguous block so the JIT's flat
    // `base + HEADER_SIZE + i*stride` array addressing — which bypasses the
    // GC's region-aware accessors entirely — reads/writes every element
    // correctly. The old region-fragmented layout backed each region with a
    // SEPARATE `Vec<u8>`, so flat access past the first region's payload hit
    // unrelated heap memory: silent zero tail at ~1–2 MB arrays, hard SIGSEGV
    // at ~4 MB+. This test writes/reads through a RAW FLAT POINTER (exactly as
    // JIT-compiled code does), including a full sweep over every element.
    #[test]
    pub fn humongous_int_array_is_contiguous_for_flat_jit_access() {
        let gc = make_collector();
        let n = 400_000; // ~1.6 MB → spans multiple 1 MB regions
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        assert!(gc.is_humongous(arr), "test array must be humongous");
        assert!(gc.count_regions(RegionType::HumongousContinuation) >= 1);

        // Raw flat base pointer to element 0 — what the JIT computes from the
        // array oop (`obj + HEADER_SIZE`), with no region-aware translation.
        let base = unsafe { arr.as_ptr().add(ARRAY_DATA_OFFSET) as *mut i32 };

        // 1) GC accessor writes, flat pointer reads back — including the tail
        //    element, which lives in a continuation region.
        let probes = [0usize, 1, 262_143, 262_144, 300_000, n - 1];
        for &i in &probes {
            gc.set_array_element(arr, i, Value::Int((i as i32).wrapping_mul(31) ^ 0x1234))
                .unwrap();
        }
        for &i in &probes {
            let v = unsafe { std::ptr::read(base.add(i)) };
            assert_eq!(
                v,
                (i as i32).wrapping_mul(31) ^ 0x1234,
                "flat JIT-style read mismatch at index {i}"
            );
        }

        // 2) Flat pointer writes EVERY element, GC accessor reads back. This is
        //    the crashing direction: a tight JIT store loop over the whole
        //    array. Pre-fix this would corrupt unrelated heap / SIGSEGV.
        for i in 0..n {
            unsafe { std::ptr::write(base.add(i), i as i32) };
        }
        for &i in &[0usize, 100_000, 262_144, 350_000, n - 1] {
            assert_eq!(
                gc.get_array_element(arr, i).unwrap().as_int(),
                Some(i as i32),
                "accessor read-back mismatch at index {i}"
            );
        }

        // 3) Contiguity invariant: the byte just past the last element stays
        //    inside the reserved span [start, start + regions_needed*region_size).
        let rs = gc.config.region_size;
        let total = ARRAY_DATA_OFFSET + n * 4;
        let regions_needed = total.div_ceil(rs);
        let start_base = arr.as_ptr() as usize;
        assert!(
            start_base + total <= start_base + regions_needed * rs,
            "humongous tail escapes its reserved contiguous span"
        );
    }

    // Residual humongous-OOB fix: `is_humongous` must distinguish a
    // multi-region humongous array from an ordinary single-region one, so
    // `VmHeap::array_data_ptr` can refuse to hand out a flat pointer for the
    // former (whose payload is non-contiguous).
    #[test]
    pub fn is_humongous_detects_multi_region_arrays() {
        let gc = make_collector();
        // Small array fits in a single region → NOT humongous.
        let small = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 10);
        assert!(
            !gc.is_humongous(small),
            "small array misclassified as humongous"
        );
        // Large array (~1.6 MB) spans multiple regions → humongous.
        let large = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 400_000);
        assert!(gc.count_regions(RegionType::HumongousStart) >= 1);
        assert!(gc.is_humongous(large), "humongous array not detected");
        // A plain object is not humongous either.
        let obj = gc.alloc_object(ClassId::new(1), 3);
        assert!(
            !gc.is_humongous(obj),
            "small object misclassified as humongous"
        );
    }

    // C2: long[] elements are 8 bytes; the per-region payload capacity
    // (region_size - HEADER_SIZE) is not a multiple of 8 for HEADER_SIZE=40
    // only when region_size isn't — but verify the straddle-safe copy path by
    // exercising elements adjacent to a region boundary.
    #[test]
    pub fn humongous_long_array_boundary_roundtrip() {
        let gc = make_collector();
        let n = 200_000; // 8 bytes each ≈ 1.6 MB → multi-region
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, n);
        assert!(gc.count_regions(RegionType::HumongousContinuation) >= 1);
        // usable/8 elements per region; probe around the first boundary.
        let usable = (gc.config.region_size - HEADER_SIZE) / 8;
        for &i in &[usable - 1, usable, usable + 1, n - 1] {
            gc.set_array_element(arr, i, Value::Long(0x0123_4567_89ab_cdef ^ i as i64))
                .unwrap();
        }
        for &i in &[usable - 1, usable, usable + 1, n - 1] {
            assert_eq!(
                gc.get_array_element(arr, i).unwrap().as_long(),
                Some(0x0123_4567_89ab_cdef ^ i as i64),
                "mismatch at index {i}",
            );
        }
    }

    // C2 / C2b: a humongous *object* (many reference slots) round-trips field
    // values across regions, and out-of-layout field access is dropped rather
    // than reading/writing neighboring memory.
    #[test]
    pub fn humongous_object_field_roundtrip_and_bounds() {
        let gc = make_collector();
        // > 512 KB of slots (16 bytes each) → humongous, multi-region.
        let num_fields = (1024 * 1024) / SLOT_SIZE + 4; // > 1 region of slots
        let obj = gc.alloc_object(ClassId::new(1), num_fields);
        assert!(gc.count_regions(RegionType::HumongousContinuation) >= 1);
        let per_region = (gc.config.region_size - HEADER_SIZE) / SLOT_SIZE;
        for &i in &[
            0usize,
            per_region - 1,
            per_region,
            per_region + 1,
            num_fields - 1,
        ] {
            gc.set_field(obj, i, Value::Int(i as i32));
        }
        for &i in &[
            0usize,
            per_region - 1,
            per_region,
            per_region + 1,
            num_fields - 1,
        ] {
            assert_eq!(gc.get_field(obj, i).as_int(), Some(i as i32), "field {i}");
        }
        // Out-of-layout field read/write must be a benign no-op (drop), not OOB.
        let oob = num_fields + 1000;
        assert_eq!(gc.get_field(obj, oob), Value::Object(None));
        gc.set_field(obj, oob, Value::Int(0xdead_u32 as i32)); // dropped silently
    }

    // -- Young collection --

    #[test]
    pub fn young_collection_basic() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(42));

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert!(result.stats.objects_copied >= 1);
        let new_obj = roots[0];
        // Should have been copied
        assert_eq!(gc.get_field(new_obj, 0).as_int(), Some(42));
    }

    #[test]
    pub fn young_collection_with_reference_chain() {
        let gc = make_collector();
        let a = gc.alloc_object(ClassId::new(1), 1);
        let b = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(a, 0, Value::Object(Some(b)));
        gc.set_field(b, 0, Value::Int(99));

        let mut roots = vec![a];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(result.stats.objects_copied, 2);
        let new_a = roots[0];
        let new_b_val = gc.get_field(new_a, 0);
        if let Value::Object(Some(new_b)) = new_b_val {
            assert_eq!(gc.get_field(new_b, 0).as_int(), Some(99));
        } else {
            panic!("expected reference to B");
        }
    }

    #[test]
    pub fn young_collection_promotion() {
        let mut cfg = small_config();
        cfg.promotion_age = 1; // promote after 1 GC cycle
        let gc = G1Collector::new(cfg);

        let obj = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![obj];

        // First GC: object goes to Survivor with age 1
        let _r1 = gc.young_collection(&mut roots, &NoopMonitors);
        // The object's gc_age should now be 1 which >= promotion_age
        // So next GC it should go to Old if it was in Survivor
        // Actually with promotion_age=1, age >= 1 means immediate promotion to Old
        let header = gc.get_header(roots[0]);
        // With promotion_age=1, objects with gc_age >= 1 go to Old.
        // After first GC, gc_age is incremented to 1.
        assert!(header.gc_age() >= 1);
    }

    #[test]
    pub fn young_collection_frees_eden() {
        let gc = make_collector();
        let _obj = gc.alloc_object(ClassId::new(1), 1);
        assert_eq!(gc.count_regions(RegionType::Eden), 1);

        let mut roots = vec![_obj];
        gc.young_collection(&mut roots, &NoopMonitors);

        // Eden should be freed after collection
        assert_eq!(gc.count_regions(RegionType::Eden), 0);
    }

    #[test]
    pub fn young_collection_unreachable_freed() {
        let gc = make_collector();
        let live = gc.alloc_object(ClassId::new(1), 0);
        let _dead = gc.alloc_object(ClassId::new(2), 0);

        let mut roots = vec![live]; // _dead not in roots
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        // Only the live object should be copied
        assert_eq!(result.stats.objects_copied, 1);
    }

    #[test]
    pub fn young_collection_pointer_map() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 0);
        let old_addr = obj.as_ptr() as usize;

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert!(result.pointer_map.contains_key(&old_addr));
        assert_eq!(result.pointer_map[&old_addr], roots[0].as_ptr() as usize);
    }

    /// MOVING-GC empirical confirmation for the JNI non-critical
    /// `Get/Release<Type>ArrayElements` copy-back handle.
    ///
    /// When a G1 young evacuation relocates an array between `GetArrayElements`
    /// and `ReleaseArrayElements`, the raw `array` jobject the native side holds
    /// (a CratonVM local ref is exactly `obj.as_ptr()`, captured at Get) becomes
    /// a stale from-space pointer. The OLD copy-back re-resolved THAT pointer via
    /// `is_heap_addr` (as `jobject_to_obj` does for a local ref) and therefore
    /// either silently dropped (region freed → `None`) or wrote into a recycled
    /// object. The FIX records a *remappable* handle instead — a JNI global ref,
    /// whose boxed `ObjectRef` `update_after_gc` rewrites through the very
    /// `pointer_map` produced here — so the copy-back follows the array.
    ///
    /// This reproduces the exact relocation and proves BOTH halves: (a) the
    /// stale raw handle no longer resolves to a live heap address, and (b) the
    /// remapped handle resolves to the array's new location with data intact and
    /// a copy-back write lands in the live array.
    #[test]
    pub fn jni_array_raw_handle_stale_after_evacuation_but_remap_survives() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
        gc.set_array_element(arr, 0, Value::Int(111)).unwrap();
        gc.set_array_element(arr, 1, Value::Int(222)).unwrap();
        gc.set_array_element(arr, 2, Value::Int(333)).unwrap();

        // The raw `array` jobject value native code holds across the window,
        // captured BEFORE the GC (exactly what the old Release re-resolved).
        let stale_handle = arr.as_ptr() as usize;

        // `roots` models the remappable keep-alive handle: a JNI global ref boxes
        // an `ObjectRef` that the collector rewrites in place via this same
        // pointer-map mechanism (`JniGlobalRefs::update_after_gc`).
        let mut roots = vec![arr];
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        let remapped = roots[0];
        let new_addr = remapped.as_ptr() as usize;

        // The evacuation actually MOVED the array (otherwise the test is vacuous).
        assert_eq!(result.pointer_map.get(&stale_handle), Some(&new_addr));
        assert_ne!(
            stale_handle, new_addr,
            "array must relocate for this test to be meaningful"
        );

        // (a) BUG path: the old raw handle is now stale — its region was
        // evacuated and freed, so the local-ref re-resolve fails. The old
        // copy-back would silently drop the native mutations here.
        assert!(
            !gc.is_addr_in_live_region(stale_handle),
            "evacuated array's old address should be a freed region"
        );
        assert!(
            gc.is_heap_addr(stale_handle).is_none(),
            "stale local-ref handle must not resolve to a live object"
        );

        // (b) FIX path: the remapped handle resolves to the array's CURRENT
        // location with the data preserved across the copy.
        assert!(gc.is_addr_in_live_region(new_addr));
        assert_eq!(gc.array_length(remapped), 3);
        assert_eq!(
            gc.get_array_element(remapped, 0).unwrap().as_int(),
            Some(111)
        );
        assert_eq!(
            gc.get_array_element(remapped, 1).unwrap().as_int(),
            Some(222)
        );
        assert_eq!(
            gc.get_array_element(remapped, 2).unwrap().as_int(),
            Some(333)
        );

        // A write through the remapped handle (the actual copy-back) is visible
        // in the live array — never in the dead from-space copy.
        gc.set_array_element(remapped, 1, Value::Int(999)).unwrap();
        assert_eq!(
            gc.get_array_element(remapped, 1).unwrap().as_int(),
            Some(999)
        );
    }

    // -- Mixed collection --

    /// Ten-findings item 2: a mixed pause's fix-up is narrow, and the narrow
    /// set still carries the GC-internal edge rebuild a LATER pause depends
    /// on. An untouched Old region proves the narrowing; a rootless young
    /// pause afterwards proves the rebuild.
    #[test]
    pub fn a_narrow_mixed_pause_still_records_the_edges_a_later_young_pause_needs() {
        let gc = make_collector();
        let region_size = gc.config.region_size;
        let (a_idx, b_idx, c_idx) = (1usize, 2usize, 3usize);
        // Three Old regions built by hand. A holds the only reference to an
        // object in B; B is the one mixed candidate (64 bytes live); A and C
        // are far too live to be candidates, and C is touched by nothing.
        let (a_obj, b_obj) = {
            let mut regions = gc.regions.write();
            for (idx, live) in [(a_idx, region_size - 64), (b_idx, 64), (c_idx, region_size)] {
                regions[idx].set_region_type(RegionType::Old);
                regions[idx].live_bytes = live;
                regions[idx].gc_efficiency = live as f64 / region_size as f64;
            }
            let mut place = |idx: usize| -> ObjectRef {
                let (ptr, _) = regions[idx]
                    .bump_alloc(HEADER_SIZE + SLOT_SIZE, 8, "test")
                    .expect("room for one object");
                let header = ObjectHeader::new(
                    ClassId::new(1),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    1,
                );
                unsafe {
                    std::ptr::write(ptr as *mut ObjectHeader, header);
                    ObjectRef::from_raw(ptr)
                }
            };
            let a = place(a_idx);
            let b = place(b_idx);
            let _ = place(c_idx);
            (a, b)
        };
        gc.with_regions_mut(|_| {});
        gc.set_field(b_obj, 0, Value::Int(0x1234));
        gc.set_field(a_obj, 0, Value::Object(Some(b_obj))); // barrier: B.rset += A

        gc.marking_complete.store(true, Ordering::Relaxed);
        gc.mixed_gc_remaining.store(1, Ordering::Relaxed);
        let b_before = b_obj.as_ptr();
        let mut roots = vec![a_obj];
        let r = gc.mixed_collection(&mut roots, &NoopMonitors);
        assert!(
            r.stats.objects_copied >= 1,
            "B's object must have been evacuated"
        );
        let moved = match gc.get_field(roots[0], 0) {
            Value::Object(Some(o)) => o,
            other => panic!("A lost its reference: {other:?}"),
        };
        assert_ne!(moved.as_ptr(), b_before);
        assert_eq!(gc.get_field(moved, 0), Value::Int(0x1234));

        let last = gc
            .pause_history_snapshot()
            .last()
            .cloned()
            .expect("the mixed pause was recorded");
        let occupied = gc.num_regions() - free_region_count(&gc);
        assert!(
            (last.phases.fixup_regions as usize) < occupied,
            "the mixed fix-up was wide: it walked {} of {occupied} occupied regions",
            last.phases.fixup_regions
        );

        // The copy now lives in a region the next young pause collects, and
        // that pause has NO roots: only the edge the narrow rebuild recorded
        // for A keeps it alive.
        let mut no_roots: Vec<ObjectRef> = vec![];
        gc.young_collection(&mut no_roots, &NoopMonitors);
        let kept = match gc.get_field(a_obj, 0) {
            Value::Object(Some(o)) => o,
            other => panic!("A lost its reference across the young pause: {other:?}"),
        };
        assert_eq!(gc.get_field(kept, 0), Value::Int(0x1234));
    }

    /// Item 2: a completed mark cycle leaves a budget of mixed pauses; when no
    /// Old region is worth collecting, the phase ends at the first of them
    /// instead of burning all eight as whole-heap young pauses.
    #[test]
    pub fn the_mixed_phase_ends_early_when_no_old_region_is_worth_collecting() {
        let gc = make_collector();
        gc.marking_complete.store(true, Ordering::Relaxed);
        gc.mixed_gc_remaining.store(8, Ordering::Relaxed);
        assert!(gc.needs_mixed_gc());
        let _churn = gc.alloc_object(ClassId::new(1), 1);
        let mut roots: Vec<ObjectRef> = vec![];
        let _ = GarbageCollector::collect_garbage(&gc, &stw(), &mut roots, &NoopMonitors);
        assert!(
            !gc.needs_mixed_gc(),
            "with no candidate the phase must end, not count down"
        );
        assert_eq!(gc.mixed_gc_remaining.load(Ordering::Relaxed), 0);
        let last = gc
            .pause_history_snapshot()
            .last()
            .cloned()
            .expect("a pause was recorded");
        assert_eq!(last.collection_type, G1CollectionType::YoungOnly);
    }

    /// Item 3: the live threshold keeps a nearly-live region out of every
    /// selector, and the waste floor ends the phase when what is left to
    /// reclaim is not worth a pause.
    #[test]
    pub fn the_mixed_phase_respects_the_live_threshold_and_the_waste_floor() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_waste_percent: 5,
            ..small_config()
        });
        let region_size = gc.config.region_size;
        gc.with_regions_mut(|regions| {
            regions[1].set_region_type(RegionType::Old);
            regions[1].set_cursor(region_size);
            regions[1].live_bytes = region_size * 9 / 10;
            regions[1].gc_efficiency = 0.9;
        });
        assert!(
            gc.select_old_regions_for_mixed_gc().is_empty(),
            "90% live is above the 85% threshold"
        );
        assert!(!gc.mixed_phase_has_work());

        gc.with_regions_mut(|regions| {
            regions[2].set_region_type(RegionType::Old);
            regions[2].set_cursor(region_size);
            regions[2].live_bytes = region_size / 2;
            regions[2].gc_efficiency = 0.5;
        });
        assert_eq!(gc.select_old_regions_for_mixed_gc(), vec![2]);
        assert!(
            gc.mixed_phase_has_work(),
            "512 KiB of garbage is 6.25% of an 8 MiB heap, above the 5% floor"
        );

        gc.with_regions_mut(|regions| {
            regions[2].live_bytes = region_size * 78 / 100;
            regions[2].gc_efficiency = 0.78;
        });
        assert_eq!(
            gc.select_old_regions_for_mixed_gc(),
            vec![2],
            "78% live is still a candidate by the threshold"
        );
        assert!(
            !gc.mixed_phase_has_work(),
            "224 KiB of garbage is 2.7% of the heap, below the 5% floor"
        );

        // …until the Free pool is tight: then the floor is waived, because the
        // mixed sequence is the only thing that can relieve the pressure.
        consume_regions(&gc, 5); // 8 regions, 2 Old, 5 consumed -> 1 Free, under 2x the 25% trigger
        assert!(
            gc.mixed_phase_has_work(),
            "with the Free pool under twice the trigger the waste floor must not refuse work"
        );
    }

    /// Allocation failure is the only place G1 is allowed to override the
    /// regular mixed policy.  The selected window must contain every blocker
    /// in its CSet and retain enough *outside* Free regions for their
    /// destinations, so a successful serial evacuation leaves the requested
    /// contiguous span rather than merely moving the holes around.
    #[test]
    pub fn humongous_compaction_plan_keeps_destinations_outside_the_target_window() {
        let gc = make_collector();
        let region_size = gc.config.region_size;
        gc.with_regions_mut(|regions| {
            for idx in [2usize, 3] {
                regions[idx].set_region_type(RegionType::Old);
                regions[idx].set_cursor(region_size);
                regions[idx].live_bytes = region_size;
            }
        });
        let regions = gc.regions.read();
        let plan = gc
            .plan_humongous_compaction(&regions, &[], 4)
            .expect("the six Free regions leave room to evacuate one blocker");
        assert_eq!((plan.target_start, plan.target_end), (3, 7));
        assert_eq!(plan.sources, vec![3]);
        assert_eq!(plan.young_sources, 0);
        assert!(
            regions
                .iter()
                .enumerate()
                .filter(|(idx, _)| !(*idx >= plan.target_start && *idx < plan.target_end))
                .filter(|(_, region)| region.region_type() == RegionType::Free)
                .count()
                >= plan.sources.len(),
            "the plan's conservative region-count check must leave a full destination outside"
        );
    }

    /// r16 G1 page: a live humongous span that splits the heap no longer
    /// blocks the allocation-failure compaction. The planner moves it to a
    /// Free run outside the window instead of reporting no window.
    #[test]
    pub fn humongous_compaction_plans_to_move_a_span_that_splits_the_heap() {
        let gc = make_collector();
        let region_size = gc.config.region_size;
        gc.with_regions_mut(|regions| {
            regions[3].set_region_type(RegionType::HumongousStart);
            regions[3].set_cursor(2 * region_size - 64);
            regions[3].live_bytes = 2 * region_size - 64;
            regions[4].set_region_type(RegionType::HumongousContinuation);
            regions[4].set_cursor(0);
        });
        let regions = gc.regions.read();
        let plan = gc
            .plan_humongous_compaction(&regions, &[], 6)
            .expect("moving the span out leaves a six-region window");
        assert_eq!((plan.target_start, plan.target_end), (0, 6));
        assert!(plan.sources.is_empty());
        assert_eq!(
            plan.humongous_moves,
            vec![HumongousMove {
                src_start: 3,
                len: 2,
                dst_start: 6
            }]
        );
    }

    /// r16 G1 page, end to end: the compacting mixed pause relocates the span,
    /// rewrites the root to the copy, keeps every element, frees the window,
    /// and the failed request then fits.
    #[test]
    pub fn the_compacting_pause_relocates_a_humongous_array_and_frees_the_window() {
        let gc = make_collector();
        let region_size = gc.config.region_size;
        // Occupy regions 0..3 so the array lands in the middle (3..5).
        gc.with_regions_mut(|regions| {
            for r in &mut regions[0..3] {
                r.set_region_type(RegionType::Old);
                r.set_cursor(HEADER_SIZE);
            }
        });
        let len = region_size * 3 / 2 / 4;
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, len);
        let old_addr = arr.as_ptr() as usize;
        assert_eq!(
            gc.lookup_region_for_addr(old_addr),
            Some(3),
            "fixture placement"
        );
        for i in (0..len).step_by(997) {
            gc.set_array_element(arr, i, Value::Int(i as i32)).unwrap();
        }
        gc.with_regions_mut(|regions| {
            for r in &mut regions[0..3] {
                r.set_region_type(RegionType::Free);
                r.set_cursor(0);
            }
        });
        gc.pending_humongous_compaction
            .lock()
            .replace(HumongousCompactionPlan {
                requested_regions: 6,
                target_start: 0,
                target_end: 6,
                sources: Vec::new(),
                young_sources: 0,
                humongous_moves: Vec::new(),
                staged_for: None,
            });
        let mut roots = vec![arr];
        let r = gc.mixed_collection(&mut roots, &NoopMonitors);
        let moved = roots[0];
        assert_ne!(moved.as_ptr() as usize, old_addr, "the root names the copy");
        assert!(r.pointer_map.get(&old_addr) == Some(&(moved.as_ptr() as usize)));
        assert_eq!(gc.lookup_region_for_addr(moved.as_ptr() as usize), Some(6));
        assert_eq!(gc.array_length(moved), len);
        for i in (0..len).step_by(997) {
            assert_eq!(gc.get_array_element(moved, i), Ok(Value::Int(i as i32)));
        }
        let mut regions = gc.regions.write();
        assert!(regions[0..6]
            .iter()
            .all(|r| r.region_type() == RegionType::Free));
        assert_eq!(
            regions
                .iter()
                .filter(|r| r.region_type() == RegionType::HumongousStart)
                .count(),
            1
        );
        drop(regions);
        let mut regions = gc.regions.write();
        assert_eq!(
            gc.claim_humongous_span_locked(&mut regions, 6 * region_size - 64),
            Some((0, 6))
        );
    }

    /// The r16 staging fixture, on 16 one-MB regions: JNI pins at 0, 7 and 12
    /// leave exactly one six-region window, `[1, 7)`, and it holds a live
    /// two-region humongous span at 3..5. Outside it the Free regions
    /// (9, 11, 13, 15) are separated by live-looking empty Old regions
    /// (8, 10, 14), so no destination run of two exists.
    pub fn r16_staging_fixture(regions: &mut [G1Region], region_size: usize, with_span: bool) {
        for idx in [0usize, 7, 12] {
            regions[idx].set_region_type(RegionType::Old);
            regions[idx].live_bytes = region_size / 2;
            regions[idx].pinned = true;
        }
        for idx in [8usize, 10, 14] {
            regions[idx].set_region_type(RegionType::Old);
            regions[idx].set_cursor(0);
            regions[idx].live_bytes = region_size / 2;
        }
        if with_span {
            regions[3].set_region_type(RegionType::HumongousStart);
            regions[3].set_cursor(2 * region_size - 64);
            regions[3].live_bytes = 2 * region_size - 64;
            regions[4].set_region_type(RegionType::HumongousContinuation);
            regions[4].set_cursor(0);
        }
    }

    pub fn sixteen_region_collector() -> G1Collector {
        G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            initial_heap_size: 16 * 1024 * 1024,
            ..small_config()
        })
    }

    /// r16 G1 page, the capacity case (`blocked_by_capacity=77`, 86 Free
    /// regions, longest run 31): the only window holds a span whose
    /// destination run is fragmented by movable Old regions. The planner
    /// stages: it evacuates the cheapest run's blocker first.
    #[test]
    pub fn humongous_compaction_stages_a_destination_run_when_none_is_free() {
        let gc = sixteen_region_collector();
        let region_size = gc.config.region_size;
        gc.with_regions_mut(|regions| r16_staging_fixture(regions, region_size, true));
        let regions = gc.regions.read();
        let plan = gc
            .plan_humongous_compaction(&regions, &[], 6)
            .expect("a staging plan clears a destination run");
        assert_eq!(plan.staged_for, Some(6));
        assert_eq!((plan.target_start, plan.target_end), (8, 10));
        assert_eq!(plan.sources, vec![8]);
        assert!(plan.humongous_moves.is_empty());
        drop(regions);
        // The bound: a ladder that has already staged twice reports no window.
        gc.compaction_stages
            .store(HUMONGOUS_COMPACTION_MAX_STAGES, Ordering::Release);
        let regions = gc.regions.read();
        assert!(gc.plan_humongous_compaction(&regions, &[], 6).is_none());
    }

    /// r16 G1 page, end to end: the staging pause frees the run and re-arms
    /// the request; the next pause relocates the span into it and the failed
    /// request then fits in the window.
    #[test]
    pub fn a_staged_compaction_relocates_the_span_on_the_next_pause() {
        let gc = sixteen_region_collector();
        let region_size = gc.config.region_size;
        // Occupy 0..3 so the array lands at 3..5.
        gc.with_regions_mut(|regions| {
            for r in &mut regions[0..3] {
                r.set_region_type(RegionType::Old);
                r.set_cursor(HEADER_SIZE);
            }
        });
        let len = region_size * 3 / 2 / 4;
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, len);
        let old_addr = arr.as_ptr() as usize;
        assert_eq!(
            gc.lookup_region_for_addr(old_addr),
            Some(3),
            "fixture placement"
        );
        for i in (0..len).step_by(991) {
            gc.set_array_element(arr, i, Value::Int(i as i32)).unwrap();
        }
        gc.with_regions_mut(|regions| {
            for r in &mut regions[0..3] {
                r.set_region_type(RegionType::Free);
                r.set_cursor(0);
            }
            r16_staging_fixture(regions, region_size, false);
        });
        gc.pending_humongous_compaction
            .lock()
            .replace(HumongousCompactionPlan {
                requested_regions: 6,
                target_start: 0,
                target_end: 6,
                sources: Vec::new(),
                young_sources: 0,
                humongous_moves: Vec::new(),
                staged_for: None,
            });
        let mut roots = vec![arr];
        gc.mixed_collection(&mut roots, &NoopMonitors);
        assert_eq!(
            roots[0].as_ptr() as usize,
            old_addr,
            "the staging pause moves no span"
        );
        assert!(gc.humongous_compaction_pending(), "the request is re-armed");
        {
            let regions = gc.regions.read();
            assert!(regions[8..10]
                .iter()
                .all(|r| r.region_type() == RegionType::Free));
        }
        gc.mixed_collection(&mut roots, &NoopMonitors);
        assert!(!gc.humongous_compaction_pending());
        let moved = roots[0];
        assert_eq!(gc.lookup_region_for_addr(moved.as_ptr() as usize), Some(8));
        assert_eq!(gc.array_length(moved), len);
        for i in (0..len).step_by(991) {
            assert_eq!(gc.get_array_element(moved, i), Ok(Value::Int(i as i32)));
        }
        let mut regions = gc.regions.write();
        assert!(regions[1..7]
            .iter()
            .all(|r| r.region_type() == RegionType::Free));
        assert_eq!(
            gc.claim_humongous_span_locked(&mut regions, 6 * region_size - 64),
            Some((1, 6))
        );
    }

    #[test]
    pub fn humongous_recovery_uses_the_latest_request_after_marking_frees_separate_holes() {
        let gc = make_collector();
        gc.note_humongous_failure(6, 1, 2);
        gc.note_humongous_failure(1, 1, 4);
        assert_eq!(gc.last_failed_humongous_span.load(Ordering::Acquire), 4);
        gc.with_regions_mut(|regions| {
            for idx in [2usize, 5] {
                regions[idx].set_region_type(RegionType::Old);
                regions[idx].set_cursor(HEADER_SIZE);
            }
        });
        gc.marking_complete.store(true, Ordering::Release);
        assert!(gc.has_fragmented_humongous_request());
        gc.arm_fragmented_humongous_compaction();
        let pending = gc.pending_humongous_compaction.lock();
        assert_eq!(pending.as_ref().unwrap().requested_regions, 4);
        assert_eq!(gc.last_failed_humongous_span.load(Ordering::Acquire), 0);
    }

    /// gcki-ngr: the armed request is answered by the Free table's shape --
    /// a run long enough (and the retry's reserve) fits it, holes do not, and
    /// a request a claim has served is no longer asked about.
    #[test]
    pub fn gcki_ngr_a_failed_humongous_request_fits_only_a_long_enough_run() {
        let gc = make_collector();
        assert!(!gc.failed_humongous_request_fits(), "nothing armed");
        gc.note_humongous_failure(1, 1, 4);
        gc.marking_complete.store(true, Ordering::Release);
        gc.arm_fragmented_humongous_compaction();
        assert!(gc.failed_humongous_request_fits(), "an empty heap has the run");
        gc.with_regions_mut(|regions| {
            for (idx, r) in regions.iter_mut().enumerate() {
                if idx % 3 == 2 {
                    r.set_region_type(RegionType::Old);
                    r.set_cursor(HEADER_SIZE);
                }
            }
        });
        assert!(!gc.failed_humongous_request_fits(), "runs of two");
        gc.with_regions_mut(|regions| {
            for r in regions.iter_mut() {
                r.set_region_type(RegionType::Free);
                r.set_cursor(0);
            }
        });
        let region_size = gc.config.region_size;
        let mut regions = gc.regions.write();
        assert!(gc
            .claim_humongous_span_locked(&mut regions, 4 * region_size - 64)
            .is_some());
        drop(regions);
        assert!(!gc.failed_humongous_request_fits(), "the claim served it");
    }

    /// gcd d10/o retirement: an empty heap can absorb everything but the
    /// to-space reserve; each region claimed out of the Free pool comes off
    /// it, and a pool at or below the reserve absorbs nothing.
    #[test]
    pub fn old_absorbable_bytes_is_the_free_pool_beyond_the_reserve() {
        let gc = make_collector();
        let rs = gc.config.region_size;
        let total = gc.heap_target_regions();
        let reserve = to_space_reserve_regions(total);
        gc.free_region_count.store(total, Ordering::Relaxed);
        assert_eq!(gc.old_absorbable_bytes(), (total - reserve) * rs);
        gc.free_region_count.store(reserve + 1, Ordering::Relaxed);
        assert_eq!(gc.old_absorbable_bytes(), rs);
        gc.free_region_count.store(reserve, Ordering::Relaxed);
        assert_eq!(gc.old_absorbable_bytes(), 0);
        gc.free_region_count.store(0, Ordering::Relaxed);
        assert_eq!(gc.old_absorbable_bytes(), 0);
        assert_eq!(to_space_reserve_regions(128), 12);
        assert_eq!(to_space_reserve_regions(4), 1);
    }

    #[test]
    pub fn headroom_bounds_a_retention_phase_change_after_zero_survival() {
        let gc = make_collector();
        gc.young_survival_permille.store(0, Ordering::Relaxed);
        assert!(
            gc.young_exceeds_evac_headroom(40, 32, 128),
            "zero survival in the preceding pause cannot admit more young than to-space"
        );
        assert!(!gc.young_exceeds_evac_headroom(4, 32, 128));
    }

    /// The near-full proxy row (`CRATONVM_G1_BREACHED_RESERVE_EDEN`): the
    /// low-Free arms hold off only while the reserve was breached before this
    /// Eden and the predicted survivors (at least one region) leave a Free
    /// region over.
    #[test]
    pub fn a_breached_reserve_defers_the_low_free_arms_until_one_region_is_left() {
        let mut gc = G1Collector::new(G1CollectorConfig {
            heap_size: 64 * 1024 * 1024,
            initial_heap_size: 64 * 1024 * 1024,
            region_size: 1024 * 1024,
            ..G1CollectorConfig::default()
        });
        assert!(gc.breached_reserve_eden);
        assert_eq!(to_space_reserve_regions(64), 6);
        gc.young_survival_permille.store(500, Ordering::Relaxed);
        gc.survivor_region_count.store(1, Ordering::Relaxed);
        // 1 Survivor + 1 Eden, 2 Free: 3 <= 6 breached, 1 predicted < 2.
        assert!(gc.breached_reserve_defers(2, 2, 64));
        // 1 Survivor + 2 Eden, 1 Free: the survivors need the last region.
        assert!(!gc.breached_reserve_defers(3, 1, 64));
        // Free 10: the reserve was not breached; the arms keep their rule.
        assert!(!gc.breached_reserve_defers(2, 10, 64));
        // 4 Eden over 3 Free: 7 > 6, the reserve held when this Eden began.
        assert!(!gc.breached_reserve_defers(5, 3, 64));
        // Zero measured survival still keeps one region for the survivors.
        gc.young_survival_permille.store(0, Ordering::Relaxed);
        assert!(gc.breached_reserve_defers(2, 2, 64));
        assert!(!gc.breached_reserve_defers(3, 1, 64));
        gc.breached_reserve_eden = false;
        assert!(!gc.breached_reserve_defers(2, 2, 64));
    }

    /// The near-full proxy row (`CRATONVM_G1_SCREEN_FORWARD_IN_ARENA`): a
    /// forwarded-looking candidate passes the screen's forward check only
    /// when its chain stays on aligned committed arena addresses.
    #[test]
    pub fn the_object_screen_follows_no_forward_out_of_the_arena() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            initial_heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            ..G1CollectorConfig::default()
        });
        let base = gc.arena_base;
        // SAFETY: the first 128 bytes of the committed arena, unused here.
        unsafe { std::ptr::write_bytes(base as *mut u8, 0, 128) };
        // Bytes that decode as a forward to `t` (what a stale word may name):
        // the mark's FORWARDED state (the mark word follows `class_id`) and
        // `t` in the second word.
        let fake_forward = |at: usize, t: usize| unsafe {
            ((at + 8) as *mut u64).write(t as u64);
            let h = &*(at as *const ObjectHeader);
            h.mark_word.store(cratonvm_types::MARK_FORWARDED, Ordering::Relaxed);
        };
        let h = unsafe { &*(base as *const ObjectHeader) };
        assert!(gc.forward_chain_in_arena(h), "not forwarded");
        fake_forward(base, base + 64);
        assert!(gc.forward_chain_in_arena(h), "an arena target with a plain header");
        fake_forward(base, 0x0101_0000_0000);
        assert!(!gc.forward_chain_in_arena(h), "the crash's target");
        fake_forward(base, 0x2000);
        assert!(!gc.forward_chain_in_arena(h));
        fake_forward(base, base + 68);
        assert!(!gc.forward_chain_in_arena(h), "misaligned");
        // A second hop out of the arena is refused too.
        fake_forward(base + 64, 0x2000);
        fake_forward(base, base + 64);
        assert!(!gc.forward_chain_in_arena(h));
    }

    /// The near-full proxy row (`CRATONVM_G1_BREACHED_RESERVE_TENURES`): a
    /// collection that starts with the reserve breached tenures everything.
    #[test]
    pub fn a_collection_under_a_breached_reserve_tenures_every_young_object() {
        let mut gc = G1Collector::new(G1CollectorConfig {
            heap_size: 64 * 1024 * 1024,
            initial_heap_size: 64 * 1024 * 1024,
            region_size: 1024 * 1024,
            ..G1CollectorConfig::default()
        });
        assert!(gc.breached_reserve_tenures);
        gc.survivor_region_count.store(1, Ordering::Relaxed);
        // 2 Eden (+1 Survivor) over 3 Free: 5 <= 6.
        gc.young_region_count.store(3, Ordering::Relaxed);
        gc.free_region_count.store(3, Ordering::Relaxed);
        assert!(gc.breached_reserve_tenures());
        // 4 Eden over 3 Free: 7 > 6, the reserve held when this Eden began.
        gc.young_region_count.store(5, Ordering::Relaxed);
        assert!(!gc.breached_reserve_tenures());
        gc.young_region_count.store(3, Ordering::Relaxed);
        gc.breached_reserve_tenures = false;
        assert!(!gc.breached_reserve_tenures());
    }

    /// The near-full proxy row (`CRATONVM_G1_BREACHED_RESERVE_BACKOFF`): a
    /// futile last cycle holds the next one back while the reserve is
    /// breached, and only then unless `CRATONVM_G1_IHOP_BACKOFF` is on.
    #[test]
    pub fn a_breached_reserve_backs_off_a_futile_mark_cycle() {
        let mut gc = G1Collector::new(G1CollectorConfig {
            heap_size: 64 * 1024 * 1024,
            initial_heap_size: 64 * 1024 * 1024,
            region_size: 1024 * 1024,
            ..G1CollectorConfig::default()
        });
        let mb = 1024 * 1024;
        assert!(gc.breached_reserve_backoff);
        gc.marking_threshold_bytes.store(mb, Ordering::Relaxed);
        gc.old_gen_bytes.store(58 * mb, Ordering::Relaxed);
        gc.last_cycle_reclaimed_bytes.store(0, Ordering::Relaxed);
        gc.last_cycle_end_old_bytes.store(58 * mb, Ordering::Relaxed);
        gc.mark_backoff_regions.store(1, Ordering::Relaxed);
        gc.last_cycle_end_collections.store(u64::MAX, Ordering::Relaxed);
        let everywhere = gc_flags().g1_ihop_backoff;
        gc.survivor_region_count.store(0, Ordering::Relaxed);
        gc.young_region_count.store(0, Ordering::Relaxed);
        gc.free_region_count.store(40, Ordering::Relaxed);
        assert_eq!(gc.would_start_marking(), !everywhere, "the reserve holds");
        assert_eq!(gc.check_ihop(), !everywhere);
        // 1 Eden over 3 Free: 4 <= 6.
        gc.young_region_count.store(1, Ordering::Relaxed);
        gc.free_region_count.store(3, Ordering::Relaxed);
        assert!(!gc.would_start_marking());
        assert!(!gc.check_ihop());
        // The required region of growth re-arms it.
        gc.old_gen_bytes.store(59 * mb, Ordering::Relaxed);
        assert!(gc.check_ihop());
        gc.old_gen_bytes.store(58 * mb, Ordering::Relaxed);
        gc.breached_reserve_backoff = false;
        assert_eq!(gc.check_ihop(), !everywhere);
    }

    /// The near-full proxy row (`CRATONVM_G1_BREACHED_RESERVE_LAST_REGION`):
    /// Eden may take the last Free region of a breached reserve only while
    /// the Old tails hold twice the predicted survivors.
    #[test]
    pub fn a_breached_reserve_gives_eden_the_last_region_when_old_tails_hold_the_survivors() {
        let mut gc = G1Collector::new(G1CollectorConfig {
            heap_size: 64 * 1024 * 1024,
            initial_heap_size: 64 * 1024 * 1024,
            region_size: 1024 * 1024,
            ..G1CollectorConfig::default()
        });
        assert!(gc.breached_reserve_last_region);
        gc.survivor_region_count.store(0, Ordering::Relaxed);
        // 50 permille of 3 regions (2 Eden + the last): 157 286 bytes, twice
        // that plus 65 536.
        gc.young_survival_permille.store(50, Ordering::Relaxed);
        let need = 2 * (3 * 1024 * 1024 * 50 / 1000) + 65_536;
        gc.old_tail_bytes.store(need - 8, Ordering::Relaxed);
        assert!(!gc.breached_reserve_defers(2, 1, 64), "the tails are short");
        gc.old_tail_bytes.store(need, Ordering::Relaxed);
        assert!(gc.breached_reserve_defers(2, 1, 64));
        // No Free region left: the pause is due.
        assert!(!gc.breached_reserve_defers(2, 0, 64));
        gc.breached_reserve_last_region = false;
        assert!(!gc.breached_reserve_defers(2, 1, 64));
    }

    /// The near-full proxy row (`CRATONVM_G1_BREACHED_RESERVE_FULL`): a pause
    /// that leaves a breached heap one Eden region arms an in-place full
    /// compaction, once per fall of the Free pool below what the last one left.
    #[test]
    pub fn a_breached_reserve_down_to_one_eden_region_compacts_next() {
        let mut gc = G1Collector::new(G1CollectorConfig {
            heap_size: 64 * 1024 * 1024,
            initial_heap_size: 64 * 1024 * 1024,
            region_size: 1024 * 1024,
            ..G1CollectorConfig::default()
        });
        assert!(gc.breached_reserve_full);
        gc.tlab_reserve_floor = 1;
        gc.survivor_region_count.store(0, Ordering::Relaxed);
        gc.young_region_count.store(0, Ordering::Relaxed);
        // A compaction left 3 Free.
        gc.free_region_count.store(3, Ordering::Relaxed);
        assert!(!gc.note_breached_reserve_full(true));
        assert_eq!(gc.breached_full_free_floor.load(Ordering::Relaxed), 3);
        // 3 Free leaves Eden two regions beside the TLAB reserve.
        assert!(!gc.note_breached_reserve_full(false));
        // 2 Free: one Eden region, and below the 3 the compaction left.
        gc.free_region_count.store(2, Ordering::Relaxed);
        let armed = gc.note_breached_reserve_full(false);
        assert_eq!(armed, gc.arm_in_place_compaction());
        if armed {
            assert!(gc.in_place_only_armed.swap(false, Ordering::AcqRel));
            assert!(gc.full_compaction_armed.swap(false, Ordering::AcqRel));
            assert_eq!(gc.breached_full_arms(), 1);
        }
        gc.in_place_only_armed.store(false, Ordering::Release);
        gc.full_compaction_armed.store(false, Ordering::Release);
        // A compaction that left 2: the next arm waits for 1.
        assert!(!gc.note_breached_reserve_full(true));
        assert!(!gc.note_breached_reserve_full(false));
        // The reserve holds: nothing.
        gc.free_region_count.store(40, Ordering::Relaxed);
        gc.breached_full_free_floor.store(usize::MAX, Ordering::Relaxed);
        assert!(!gc.note_breached_reserve_full(false));
        gc.free_region_count.store(1, Ordering::Relaxed);
        gc.breached_reserve_full = false;
        assert!(!gc.note_breached_reserve_full(false));
    }

    #[test]
    pub fn humongous_recovery_preserves_a_run_already_freed_by_cleanup() {
        let gc = make_collector();
        gc.note_humongous_failure(1, 1, 4);
        gc.marking_complete.store(true, Ordering::Release);
        gc.arm_fragmented_humongous_compaction();
        let reservation = *gc.humongous_compaction_avoid.lock();
        let reservation = reservation.expect("an adequate run must survive the recovery pause");
        assert_eq!(reservation.end - reservation.start, 4);
        assert!(gc.pending_humongous_compaction.lock().is_none());
        let mut regions = gc.regions.write();
        let destination = gc
            .claim_free_region_for(&regions, RegionType::Survivor)
            .unwrap();
        assert!(!(reservation.start..reservation.end).contains(&destination));
        regions[destination].set_region_type(RegionType::Survivor);
        assert_eq!(
            gc.claim_humongous_span_locked(&mut regions, 4 * gc.config.region_size),
            Some((reservation.start, 4))
        );
        assert!(gc.humongous_compaction_avoid.lock().is_none());
    }

    #[test]
    pub fn a_humongous_reservation_cannot_refuse_a_larger_available_span() {
        let gc = make_collector();
        gc.humongous_compaction_avoid
            .lock()
            .replace(HumongousCompactionReservation {
                requested_regions: 2,
                start: 2,
                end: 4,
            });
        gc.note_humongous_failure(1, 1, 5);
        let mut regions = gc.regions.write();
        let span = gc.claim_humongous_span_locked(&mut regions, 5 * gc.config.region_size);
        assert_eq!(span, Some((0, 5)));
        assert!(gc.humongous_compaction_avoid.lock().is_none());
        assert_eq!(gc.last_failed_humongous_span.load(Ordering::Acquire), 0);
    }

    #[test]
    pub fn a_humongous_retry_can_use_an_available_span_outside_its_failed_window() {
        let gc = make_collector();
        gc.humongous_compaction_avoid
            .lock()
            .replace(HumongousCompactionReservation {
                requested_regions: 2,
                start: 0,
                end: 2,
            });
        let mut regions = gc.regions.write();
        regions[0].set_region_type(RegionType::Old);
        regions[0].set_cursor(HEADER_SIZE);
        let span = gc.claim_humongous_span_locked(&mut regions, 2 * gc.config.region_size);
        assert_eq!(span, Some((1, 2)));
        assert!(gc.humongous_compaction_avoid.lock().is_none());
    }

    #[test]
    pub fn humongous_compaction_accepts_a_window_with_only_young_blockers() {
        let gc = make_collector();
        gc.with_regions_mut(|regions| {
            for idx in [2usize, 5] {
                regions[idx].set_region_type(RegionType::Eden);
                regions[idx].set_cursor(HEADER_SIZE);
            }
        });
        let regions = gc.regions.read();
        assert_eq!(free_region_shape(&regions), (6, 2));
        let plan = gc
            .plan_humongous_compaction(&regions, &[], 4)
            .expect("young blockers can be evacuated outside the reserved window");
        assert!(plan.sources.is_empty());
        assert_eq!(plan.young_sources, 1);
    }

    #[test]
    pub fn humongous_compaction_leaves_young_blockers_to_the_normal_mixed_cset() {
        let gc = make_collector();
        let region_size = gc.config.region_size;
        gc.with_regions_mut(|regions| {
            regions[2].set_region_type(RegionType::Old);
            regions[2].set_cursor(region_size);
            regions[2].live_bytes = region_size;
            regions[3].set_region_type(RegionType::Eden);
            regions[3].set_cursor(region_size / 2);
        });
        let regions = gc.regions.read();
        let plan = gc
            .plan_humongous_compaction(&regions, &[], 4)
            .expect("the target has one movable old and one normal young blocker");
        assert!(
            !plan.sources.contains(&3),
            "a young blocker belongs to the ordinary mixed young CSet, not the old-only plan"
        );
        assert_eq!(plan.young_sources, 1);
    }

    /// LANE W6-M — the refusal census tells the three screens apart.
    ///
    /// The point of the census is that "no mixed pause happened" stops being
    /// one observation and becomes three, so this pins that the three do not
    /// collapse into each other. It drives `mixed_phase_has_work` directly
    /// rather than through a pause, because the counters it asserts on are the
    /// per-poll candidate tallies and a pause would add its own polls.
    #[test]
    pub fn the_mixed_refusal_census_separates_the_three_screens() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_waste_percent: 5,
            ..small_config()
        });
        let region_size = gc.config.region_size;

        // One Old region with no mark data at all: refused by `live_bytes == 0`
        // and by nothing else.
        gc.with_regions_mut(|regions| {
            regions[1].set_region_type(RegionType::Old);
            regions[1].set_cursor(region_size);
        });
        assert!(!gc.mixed_phase_has_work());
        let c = gc.mixed_refusal_census();
        assert_eq!(c.4, 1, "one region refused for having no mark data");
        assert_eq!(c.5, 0, "and none refused by the live threshold");

        // Now give it mark data ABOVE the threshold: the refusal must move
        // from `no_data` to `too_live`, not merely repeat.
        gc.with_regions_mut(|regions| {
            regions[1].live_bytes = region_size * 9 / 10;
            regions[1].gc_efficiency = 0.9;
        });
        assert!(!gc.mixed_phase_has_work());
        let c = gc.mixed_refusal_census();
        assert_eq!(c.4, 1, "no second no-data refusal");
        assert_eq!(c.5, 1, "one refusal by the 85% live threshold");
        assert_eq!(c.7, 0, "the waste floor was never reached: no candidate");

        // A candidate under the threshold but holding too little garbage:
        // now the waste floor is what refuses, and it is counted separately.
        gc.with_regions_mut(|regions| {
            regions[2].set_region_type(RegionType::Old);
            regions[2].set_cursor(region_size);
            regions[2].live_bytes = region_size * 78 / 100;
            regions[2].gc_efficiency = 0.78;
        });
        assert!(!gc.mixed_phase_has_work());
        let c = gc.mixed_refusal_census();
        assert_eq!(c.7, 1, "one refusal by the heap_waste_percent floor");
        assert_eq!(
            c.4, 1,
            "and the candidate screen tallies did not move for region 2"
        );
    }

    /// LANE W7-L — the futile old-to-old copy breaker: four consecutive
    /// mixed pauses that reclaimed no more than they copied must refuse the
    /// fifth, and the unarmed arm must refuse nothing while still counting.
    ///
    /// The heap state is held FIXED across every assertion here. That is the
    /// point: `mixed_phase_has_work` gives two different answers to the same
    /// regions, because the breaker is not a second opinion about the heap —
    /// it is a memory of what pauses on that heap actually did.
    #[test]
    pub fn four_futile_mixed_pauses_trip_the_breaker_and_the_unarmed_arm_refuses_nothing() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_waste_percent: 5,
            ..small_config()
        });
        let region_size = gc.config.region_size;
        gc.with_regions_mut(|regions| {
            regions[2].set_region_type(RegionType::Old);
            regions[2].set_cursor(region_size);
            regions[2].live_bytes = region_size / 2;
            regions[2].gc_efficiency = 0.5;
        });
        // Before there is any evidence the two arms are the same collector.
        assert!(gc.mixed_phase_has_work_within(true));
        assert!(gc.mixed_phase_has_work_within(false));
        let c = gc.mixed_futile_census();
        assert_eq!(
            (c.3, c.4, c.5),
            (0, 0, 0),
            "an untripped breaker polls, refuses and probes nothing"
        );

        // W6-M §5's signature, exactly: `bytes_freed == bytes_copied`.
        for _ in 0..MIXED_FUTILE_STREAK_TRIP {
            gc.note_mixed_productivity(20 * 1024 * 1024, 20 * 1024 * 1024);
        }
        let c = gc.mixed_futile_census();
        assert_eq!(c.0, MIXED_FUTILE_STREAK_TRIP, "four futile pauses counted");
        assert_eq!(c.1, 0, "and none productive");
        assert_eq!(c.2, MIXED_FUTILE_STREAK_TRIP, "the streak is consecutive");

        // The null arm: today's behaviour, and the poll counted anyway so a
        // control run can report the engagement count for the fix.
        assert!(
            gc.mixed_phase_has_work_within(false),
            "the unarmed arm must decide exactly what it decided before"
        );
        let c = gc.mixed_futile_census();
        assert_eq!(c.3, 1, "the poll reached the tripped breaker");
        assert_eq!(c.4, 0, "and was not refused");

        // Armed: refused — and counted under its own reason, not the waste
        // floor's, or the two arms' censuses would not be comparable.
        let waste_before = gc.mixed_refusal_census().7;
        assert!(
            !gc.mixed_phase_has_work_within(true),
            "the fifth futile pause must be refused"
        );
        let c = gc.mixed_futile_census();
        assert_eq!(c.4, 1, "one refusal, and it is the breaker's");
        assert_eq!(
            gc.mixed_refusal_census().7,
            waste_before,
            "the heap_waste_percent counter must not absorb a breaker refusal"
        );

        // …and one pause that reclaims closes it again, in both arms.
        gc.note_mixed_productivity(1024, 4096);
        let c = gc.mixed_futile_census();
        assert_eq!(c.2, 0, "a productive pause resets the streak");
        assert_eq!(c.1, 1, "and is counted as the denominator");
        assert!(gc.mixed_phase_has_work_within(true));
    }

    /// LANE W7-L — THE LOOP ITSELF, reproduced in the test collector, with an
    /// engagement count on both arms.
    ///
    /// W6-M's reproduction is a 24-minute release run of a Java workload that
    /// fails to terminate six times in thirteen, and no fix can be argued
    /// against a reproduction that costs that much and is a coin flip. This is
    /// the same fixed point built from the collector's own primitives, where
    /// it is deterministic and costs milliseconds:
    ///
    ///  * one Old region whose contents are ENTIRELY REACHABLE, so a mixed
    ///    pause copies all of it and reclaims nothing — `bytes_freed` and
    ///    `bytes_copied` come out equal, which is W6-M's exact signature;
    ///  * `live_bytes` restamped low before each poll, which is what a mark
    ///    cycle running against a moving old generation leaves behind and is
    ///    what makes the region look like a candidate forever. Both selectors
    ///    screen on that number, so neither can refuse this region;
    ///  * the pause routed through the real `mixed_collection`, so the
    ///    productivity sample arrives through `record_collection_with_phases`
    ///    rather than being handed to the breaker by the test.
    ///
    /// What it does NOT reproduce is why a real run's `live_bytes` is wrong —
    /// that is W6-M §6.3's open question and this fixture stipulates it. The
    /// claim here is narrower and is the one the fix rests on: GIVEN a heap
    /// the existing screens cannot refuse, the collector copies it onto itself
    /// once per poll forever, and the breaker stops it.
    ///
    /// **The numbers it produces**, over 64 polls of the same heap: unarmed,
    /// **64 mixed pauses of which 64 are futile** — the loop, at a 100% rate,
    /// with no run-to-run spread because nothing here is a race. Armed,
    /// **5**: the four the breaker needs as evidence plus one probe, which is
    /// `MIXED_FUTILE_STREAK_TRIP + rounds / MIXED_FUTILE_PROBE_EVERY` and
    /// therefore goes to a FIXED 4 plus a rate, not to a fixed total, however
    /// long the run is.
    #[test]
    pub fn an_old_generation_copied_onto_itself_loops_until_the_breaker_refuses_it() {
        /// `(mixed pauses taken, futile pauses counted)` over `rounds` polls.
        fn drive(futile_guard: bool, rounds: u64) -> (u64, u64) {
            let mut gc = make_collector();
            // gci i20/ys: the W7-L latch arm. `CRATONVM_G1_MIXED_FUTILE_RESET_PER_CYCLE`
            // is default ON since 2026-10-15 (WARNING a4af35def8), and every
            // round of this fixture is a fresh one-pause mixed phase
            // (`spend_mixed_count` closes it), so on that arm each phase's
            // first poll restarts the streak by design (fresh mark data is a
            // fresh trial; vi18-mx M2 judged that arm on this loop). This test
            // pins the breaker's rate, which is the latch arm's behaviour.
            gc.mixed_futile_reset_per_cycle = false;
            let mut roots: Vec<ObjectRef> = Vec::new();
            gc.with_regions_mut(|regions| {
                regions[1].set_region_type(RegionType::Old);
                for _ in 0..8 {
                    let (ptr, _) = regions[1]
                        .bump_alloc(HEADER_SIZE + SLOT_SIZE, 8, "w7l")
                        .expect("room in a 1 MiB region");
                    let header = ObjectHeader::new(
                        ClassId::new(1),
                        ObjectKind::Object,
                        ArrayElementType::Reference,
                        0,
                        1,
                    );
                    // Past the tenuring threshold, or the evacuator sends the
                    // survivors of this Old region to a Survivor region and
                    // the fixture stops being an OLD-to-old copy after one
                    // pass. A real old generation's objects are tenured.
                    header.set_gc_age(cratonvm_types::MAX_GC_AGE);
                    // SAFETY: `ptr` is the base of a span this region just
                    // carved and nothing else has a handle to it.
                    unsafe {
                        std::ptr::write(ptr as *mut ObjectHeader, header);
                        roots.push(ObjectRef::from_raw(ptr));
                    }
                }
            });

            let mut taken = 0u64;
            for _ in 0..rounds {
                // What a mark cycle leaves: mark data that under-reports the
                // liveness of a region every byte of which is reachable.
                gc.with_regions_mut(|regions| {
                    for r in regions.iter_mut() {
                        if r.region_type() == RegionType::Old && r.cursor() > 0 {
                            r.live_bytes = (r.cursor() / 4).max(1);
                            r.gc_efficiency = 0.25;
                        }
                    }
                });
                gc.marking_complete.store(true, Ordering::Relaxed);
                gc.mixed_gc_remaining.store(1, Ordering::Relaxed);
                if !gc.mixed_phase_has_work_within(futile_guard) {
                    continue;
                }
                taken += 1;
                let r = gc.mixed_collection(&mut roots, &NoopMonitors);
                assert!(
                    r.stats.bytes_freed <= r.stats.bytes_copied * 3 / 2,
                    "the fixture is supposed to be futile, but this pause \
                     copied {} and freed {}",
                    r.stats.bytes_copied,
                    r.stats.bytes_freed
                );
            }
            let census = gc.mixed_futile_census();
            (taken, census.0)
        }

        const ROUNDS: u64 = 64;

        // The null arm IS today's collector: every poll takes a pause, and
        // every pause is futile. This is the engagement count the fix has to
        // be measured against, and it is 64 of 64.
        let (unarmed_taken, unarmed_futile) = drive(false, ROUNDS);
        assert_eq!(
            unarmed_taken, ROUNDS,
            "today's collector takes a mixed pause on every poll — that is the loop"
        );
        assert!(
            unarmed_futile >= ROUNDS - 1,
            "and all but at most one of them reclaimed nothing (got {unarmed_futile})"
        );

        // Armed: the first `TRIP` pauses still happen — the breaker needs
        // evidence and does not get it for free — and after that only the
        // periodic probe does.
        let (armed_taken, _) = drive(true, ROUNDS);
        let ceiling = MIXED_FUTILE_STREAK_TRIP + ROUNDS / MIXED_FUTILE_PROBE_EVERY + 1;
        assert!(
            armed_taken >= MIXED_FUTILE_STREAK_TRIP && armed_taken <= ceiling,
            "the armed arm must take between {MIXED_FUTILE_STREAK_TRIP} and \
             {ceiling} pauses, got {armed_taken}"
        );
        assert!(
            armed_taken * 8 < unarmed_taken,
            "the whole point is the ratio: {armed_taken} against {unarmed_taken}"
        );
    }

    /// LANE W7-L — a tripped breaker is a RATE, not a latch.
    ///
    /// The failure this pins is the one a plain refusal would have shipped: a
    /// breaker with no probe can never collect the evidence that would close
    /// it, so four pauses' evidence would cost the process its
    /// old-generation reclamation for good.
    #[test]
    pub fn the_tripped_breaker_still_admits_one_poll_in_thirty_two() {
        let mut gc = G1Collector::new(G1CollectorConfig {
            heap_waste_percent: 5,
            ..small_config()
        });
        // gci i20/ys: the latch arm (the reset-per-cycle switch is default ON
        // since 2026-10-15, and this fixture's first poll is a phase's first
        // poll, which that arm answers with a fresh trial; covered by
        // `gci_i18mx_the_reset_per_cycle_switch_restarts_a_tripped_streak_at_a_phase_start`).
        gc.mixed_futile_reset_per_cycle = false;
        let region_size = gc.config.region_size;
        gc.with_regions_mut(|regions| {
            regions[2].set_region_type(RegionType::Old);
            regions[2].set_cursor(region_size);
            regions[2].live_bytes = region_size / 2;
            regions[2].gc_efficiency = 0.5;
        });
        for _ in 0..MIXED_FUTILE_STREAK_TRIP {
            gc.note_mixed_productivity(4096, 4096);
        }
        let rounds = MIXED_FUTILE_PROBE_EVERY * 3;
        let admitted = (0..rounds)
            .filter(|_| gc.mixed_phase_has_work_within(true))
            .count() as u64;
        assert_eq!(
            admitted, 3,
            "three probes in {rounds} polls, not zero and not {rounds}"
        );
        let c = gc.mixed_futile_census();
        assert_eq!(c.5, 3, "each admission is counted as a probe");
        assert_eq!(c.4, rounds - 3, "and every other poll as a refusal");
        assert_eq!(c.3, rounds, "refused + probes is every poll past the trip");
    }

    /// gci i17/mx -- mixed pauses over HotSpot-legitimate candidates (80 %
    /// live: `freed / copied` = 1.25, under the legacy 150 % bar) are
    /// productive, so any number of them in a row leaves the breaker closed
    /// and the armed gate keeps admitting the phase. Before the fix four of
    /// them tripped it and the mixed phase was refused for ~32 marking cycles
    /// (`GciGo11G1NamesProbe mixed` at -Xmx96m: Old 71 -> 92 MB, then OOME).
    #[test]
    pub fn gci_i17mx_candidate_only_mixed_pauses_do_not_trip_the_breaker() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_waste_percent: 5,
            ..small_config()
        });
        assert_eq!(gc.config.mixed_gc_live_threshold_percent, 85);
        let region_size = gc.config.region_size;
        gc.with_regions_mut(|regions| {
            regions[2].set_region_type(RegionType::Old);
            regions[2].set_cursor(region_size);
            regions[2].live_bytes = region_size / 2;
            regions[2].gc_efficiency = 0.5;
        });
        for _ in 0..(MIXED_FUTILE_STREAK_TRIP * 4) {
            gc.note_mixed_productivity(800 * 1024, 1000 * 1024);
        }
        let c = gc.mixed_futile_census();
        assert_eq!(c.0, 0, "no pause over 85 %-threshold candidates is futile");
        assert_eq!(c.1, MIXED_FUTILE_STREAK_TRIP * 4, "every one is productive");
        assert_eq!(c.2, 0, "the streak never starts");
        assert!(gc.mixed_phase_has_work_within(true), "the armed gate admits the phase");
        assert_eq!(gc.mixed_futile_census().3, 0, "an untripped breaker is not polled");

        // The futile loop's signature (`freed == copied`) still trips it,
        // and still on the fourth pause.
        for _ in 0..MIXED_FUTILE_STREAK_TRIP {
            gc.note_mixed_productivity(1000 * 1024, 1000 * 1024);
        }
        assert_eq!(gc.mixed_futile_census().2, MIXED_FUTILE_STREAK_TRIP);
        assert!(!gc.mixed_phase_has_work_within(true), "the loop is still refused");
        // One candidate-only pause closes it again.
        gc.note_mixed_productivity(800 * 1024, 1000 * 1024);
        assert_eq!(gc.mixed_futile_census().2, 0);
        assert!(gc.mixed_phase_has_work_within(true));
    }

    /// gci i18/mx (i17/mx finding 2) -- an evacuation-failure mixed pause and
    /// a compaction pause reach the breaker's funnel with `freed == copied` or
    /// worse by construction; neither is a sample, so neither feeds the streak.
    /// A clean mixed pause with the same numbers still does.
    #[test]
    pub fn gci_i18mx_evacuation_failure_and_compaction_pauses_do_not_feed_the_streak() {
        let gc = make_collector();
        let futile = GcStats {
            objects_copied: 1,
            bytes_copied: 1024 * 1024,
            bytes_freed: 1024 * 1024,
        };
        for _ in 0..MIXED_FUTILE_STREAK_TRIP {
            gc.record_collection_with_phases_within(
                G1CollectionType::Mixed,
                1,
                &futile,
                G1PausePhases {
                    evacuation_failed: true,
                    ..G1PausePhases::default()
                },
                true,
                false,
            );
            gc.record_collection_with_phases_within(
                G1CollectionType::Mixed,
                1,
                &futile,
                G1PausePhases {
                    compaction: true,
                    ..G1PausePhases::default()
                },
                true,
                false,
            );
        }
        let c = gc.mixed_futile_census();
        assert_eq!((c.0, c.1, c.2), (0, 0, 0), "no sample was taken");
        assert_eq!(
            gc.mixed_futile_i18_census().0,
            MIXED_FUTILE_STREAK_TRIP * 2,
            "every skipped pause is counted"
        );
        for _ in 0..MIXED_FUTILE_STREAK_TRIP {
            gc.record_collection_with_phases_within(
                G1CollectionType::Mixed,
                1,
                &futile,
                G1PausePhases::default(),
                true,
                false,
            );
        }
        assert_eq!(
            gc.mixed_futile_census().2,
            MIXED_FUTILE_STREAK_TRIP,
            "a clean futile mixed pause is still a sample"
        );
    }

    /// gci i18/mx (i17/mx finding 1, `CRATONVM_G1_MIXED_FUTILE_RESET_PER_CYCLE`)
    /// -- on the switch arm the first breaker poll of a mixed phase restarts a
    /// tripped streak; polls inside the phase do not, and a phase end (either
    /// door) re-arms the restart. The switch-off arm keeps the W7-L latch
    /// (gci i20/ys: the switch is default ON since 2026-10-15, WARNING
    /// a4af35def8; both arms are set explicitly below).
    #[test]
    pub fn gci_i18mx_the_reset_per_cycle_switch_restarts_a_tripped_streak_at_a_phase_start() {
        fn collector(reset_per_cycle: bool) -> G1Collector {
            let mut gc = G1Collector::new(G1CollectorConfig {
                heap_waste_percent: 5,
                ..small_config()
            });
            gc.mixed_futile_reset_per_cycle = reset_per_cycle;
            let region_size = gc.config.region_size;
            gc.with_regions_mut(|regions| {
                regions[2].set_region_type(RegionType::Old);
                regions[2].set_cursor(region_size);
                regions[2].live_bytes = region_size / 2;
                regions[2].gc_efficiency = 0.5;
            });
            for _ in 0..MIXED_FUTILE_STREAK_TRIP {
                gc.note_mixed_productivity(4096, 4096);
            }
            gc
        }

        // Switch-off arm: the trip latches across the phase end.
        let gc = collector(false);
        gc.end_mixed_phase("test");
        assert!(!gc.mixed_phase_has_work_within(true), "default arm latches");
        assert_eq!(gc.mixed_futile_i18_census().1, 0);

        // Switch arm.
        let gc = collector(true);
        assert!(
            gc.mixed_phase_has_work_within(true),
            "the first poll of a phase is a fresh trial"
        );
        assert_eq!(gc.mixed_futile_census().2, 0, "the streak restarted");
        assert_eq!(gc.mixed_futile_i18_census().1, 1, "one tripped streak broken");
        for _ in 0..MIXED_FUTILE_STREAK_TRIP {
            gc.note_mixed_productivity(4096, 4096);
        }
        assert!(
            !gc.mixed_phase_has_work_within(true),
            "inside the phase a new trip still refuses"
        );
        assert_eq!(gc.mixed_futile_i18_census().1, 1);
        // `end_mixed_phase` re-arms the restart.
        gc.end_mixed_phase("test");
        assert!(gc.mixed_phase_has_work_within(true));
        assert_eq!(gc.mixed_futile_i18_census().1, 2);
        // So does the last pause of a budgeted phase (`spend_mixed_count`).
        for _ in 0..MIXED_FUTILE_STREAK_TRIP {
            gc.note_mixed_productivity(4096, 4096);
        }
        gc.mixed_gc_remaining.store(1, Ordering::Relaxed);
        gc.spend_mixed_count(false);
        assert!(!gc.mixed_futile_phase_polled.load(Ordering::Relaxed));
        assert!(gc.mixed_phase_has_work_within(true));
        assert_eq!(gc.mixed_futile_i18_census().1, 3);
    }

    /// LANE W7-L — what is and is not a sample for the breaker.
    ///
    /// A young pause is not one (it has no old half at all), and neither is a
    /// drain pass, whose closure chases one pause's self-forwarded residue
    /// with no to-space left and therefore reads futile by construction. A
    /// drain that taught the breaker would trip it on the evacuation-failure
    /// path, where the mixed sequence is this collector's full GC and is the
    /// last thing that should be refused.
    #[test]
    pub fn only_a_mixed_pause_that_is_not_a_drain_teaches_the_futile_breaker() {
        let gc = make_collector();
        let stats = GcStats {
            objects_copied: 1,
            bytes_copied: 1024,
            bytes_freed: 1024,
        };
        gc.record_collection_with_phases_within(
            G1CollectionType::YoungOnly,
            1,
            &stats,
            G1PausePhases::default(),
            true,
            false,
        );
        assert_eq!(
            gc.mixed_futile_census().0,
            0,
            "a young pause is not a sample of old-to-old copying"
        );

        let drain = G1PausePhases {
            drain_pass: true,
            ..G1PausePhases::default()
        };
        gc.record_collection_with_phases_within(
            G1CollectionType::Mixed,
            1,
            &stats,
            drain,
            true,
            false,
        );
        assert_eq!(
            gc.mixed_futile_census().0,
            0,
            "a drain pass is not a sample either"
        );

        gc.record_collection_with_phases_within(
            G1CollectionType::Mixed,
            1,
            &stats,
            G1PausePhases::default(),
            true,
            false,
        );
        assert_eq!(
            gc.mixed_futile_census().0,
            1,
            "an ordinary mixed pause with freed == copied is one"
        );

        // A pause that copied nothing has no ratio, and must not be allowed to
        // reset a streak by alternating with futile ones.
        let empty = GcStats {
            objects_copied: 0,
            bytes_copied: 0,
            bytes_freed: 0,
        };
        gc.record_collection_with_phases_within(
            G1CollectionType::Mixed,
            1,
            &empty,
            G1PausePhases::default(),
            true,
            false,
        );
        let c = gc.mixed_futile_census();
        assert_eq!(
            (c.0, c.1, c.2),
            (1, 0, 1),
            "0/0 is not a sample of anything"
        );
    }

    /// Item 10: the mixed copy budget is charged for the fix-up walk. Two
    /// cheap candidates both fit a budget nothing else has spent; once the
    /// walk is expected to take the whole goal, only the forward-progress
    /// region is taken.
    #[test]
    pub fn the_mixed_copy_budget_is_charged_for_the_fix_up_walk() {
        // `old_cset_region_threshold_percent` caps at max(1, 10% of 8) = 1
        // region; lift it so the budget is what decides.
        let gc = G1Collector::new(G1CollectorConfig {
            old_cset_region_threshold_percent: 100,
            ..small_config()
        });
        gc.with_regions_mut(|regions| {
            for idx in [1usize, 2] {
                regions[idx].set_region_type(RegionType::Old);
                regions[idx].set_cursor(1000);
                regions[idx].live_bytes = 100;
                regions[idx].gc_efficiency = 0.1;
            }
        });
        assert_eq!(gc.select_old_regions_for_mixed_gc(), vec![1, 2]);
        gc.fixup_ns_ema.store(
            gc.config.max_gc_pause_ms.saturating_mul(1_000_000),
            Ordering::Relaxed,
        );
        assert_eq!(gc.old_cset_copy_budget_ns(), 0);
        assert_eq!(
            gc.select_old_regions_for_mixed_gc(),
            vec![1],
            "with the goal spent on the walk, only the forward-progress region is taken"
        );
    }

    /// Item 9: a long reference array is marked in chunks — one pop marks the
    /// array and scans its first chunk, and the remainder waits in the gray
    /// set as a continuation entry rather than being scanned under one hold of
    /// the region guard. Every element is still reached.
    #[test]
    pub fn a_long_reference_array_is_marked_in_chunks() {
        let gc = make_collector();
        let len = MARK_ARRAY_CHUNK * 2 + 7;
        let arr = gc.alloc_array(ClassId::new(7), ArrayElementType::Reference, len);
        let mut leaves = Vec::with_capacity(len);
        for i in 0..len {
            let o = gc.alloc_object(ClassId::new(1), 0);
            gc.set_array_element(arr, i, Value::Object(Some(o)))
                .unwrap();
            leaves.push(o);
        }
        gc.start_concurrent_mark(&stw());
        gc.remark(&stw(), &[arr]);
        assert!(!gc.concurrent_mark_step(1), "one pop leaves work behind");
        assert_eq!(
            gc.dbg_gray_chunk_entries(),
            1,
            "the array's remainder is one continuation entry"
        );
        while !gc.concurrent_mark_step(usize::MAX) {}
        for (i, o) in leaves.iter().enumerate() {
            assert!(
                gc.dbg_is_marked(o.as_ptr() as usize),
                "leaf {i} was not reached"
            );
        }
        gc.cleanup(&stw());
    }

    #[test]
    pub fn mixed_collection_selects_old_regions() {
        let gc = make_collector();

        // Manually set up some old regions with gc_efficiency data.
        // `live_bytes > 0` marks them as carrying marking data from a
        // completed cycle — regions without it (post-cleanup promotions)
        // are ineligible for the mixed CSet (G1CORE-7 gate).
        {
            let mut regions = gc.regions.write();
            regions[0].set_region_type(RegionType::Old);
            regions[0].set_cursor(100);
            regions[0].live_bytes = 10;
            regions[0].gc_efficiency = 0.1; // 10% live = 90% garbage, best candidate
            regions[1].set_region_type(RegionType::Old);
            regions[1].set_cursor(100);
            regions[1].live_bytes = 90;
            regions[1].gc_efficiency = 0.9; // 90% live = 10% garbage, poor candidate
        }

        // Force marking complete to trigger mixed GC
        gc.marking_complete.store(true, Ordering::Relaxed);
        gc.mixed_gc_remaining.store(1, Ordering::Relaxed);

        let mut roots = vec![];
        let _result = gc.mixed_collection(&mut roots, &NoopMonitors);

        // Region 0 (lowest gc_efficiency = most garbage) should have been collected.
        // Region 1 (high gc_efficiency) should remain.
        assert_eq!(gc.count_regions(RegionType::Old), 1);
    }

    /// gcd d10/o — the shape `Gcd10oG1DropRetentionProbe` left behind: Old
    /// regions that are almost all garbage, each kept by a few live bytes.
    /// The ordinary mixed pause takes only its percentage cap of them; one
    /// full-compaction pause takes every one the Free pool can hold.
    pub fn d10o_mostly_dead_old_regions(gc: &G1Collector, n: usize) {
        let half = gc.config.region_size / 2;
        gc.with_regions_mut(|rs| {
            for r in rs.iter_mut().take(n) {
                r.set_region_type(RegionType::Old);
                r.set_cursor(half);
                r.live_bytes = 240;
                r.gc_efficiency = 240.0 / gc.config.region_size as f64;
            }
        });
    }


    #[test]
    pub fn d10o_a_full_compaction_pause_ignores_the_mixed_caps() {
        let build = || {
            let mut cfg = small_config();
            cfg.old_cset_region_threshold_percent = 1; // one region per mixed pause
            let gc = G1Collector::new(cfg);
            d10o_mostly_dead_old_regions(&gc, 4);
            gc.marking_complete.store(true, Ordering::Relaxed);
            gc.mixed_gc_remaining.store(1, Ordering::Relaxed);
            gc
        };
        let mut roots = vec![];

        let ordinary = build();
        let _ = ordinary.mixed_collection(&mut roots, &NoopMonitors);
        assert_eq!(ordinary.count_regions(RegionType::Old), 3);

        let full = build();
        full.full_compaction_this_pause
            .store(true, Ordering::Relaxed);
        let _ = full.mixed_collection(&mut roots, &NoopMonitors);
        full.full_compaction_this_pause
            .store(false, Ordering::Relaxed);
        assert_eq!(full.count_regions(RegionType::Old), 0);
    }

    #[test]
    pub fn d10o_full_compaction_arms_only_on_a_trusted_mark() {
        let gc = make_collector();
        gc.last_mark_trusted.store(false, Ordering::Release);
        assert!(!gc.arm_full_compaction());
        assert!(!gc.full_compaction_armed.load(Ordering::Acquire));
        gc.last_mark_trusted.store(true, Ordering::Release);
        assert_eq!(gc.arm_full_compaction(), true);
    }

    // -- IHOP --

    #[test]
    pub fn ihop_threshold_calculation() {
        let mut cfg = small_config();
        cfg.ihop_percent = 50;
        let gc = G1Collector::new(cfg.clone());

        let expected = cfg.heap_size / 2;
        assert_eq!(gc.marking_threshold_bytes(), expected);
    }

    #[test]
    pub fn ihop_check_below_threshold() {
        let gc = make_collector();
        gc.old_gen_bytes.store(0, Ordering::Relaxed);
        assert!(!gc.check_ihop());
    }

    #[test]
    pub fn ihop_check_above_threshold() {
        let gc = make_collector();
        let threshold = gc.marking_threshold_bytes();
        gc.old_gen_bytes.store(threshold + 1, Ordering::Relaxed);
        assert!(gc.check_ihop());
    }

    #[test]
    pub fn ihop_adaptive_adjustment_lower() {
        let gc = make_collector();
        let before = gc.marking_threshold_bytes();
        // Pause way over target should lower threshold
        gc.update_ihop(gc.config.max_gc_pause_ms + 100);
        let after = gc.marking_threshold_bytes();
        assert!(after < before);
    }

    #[test]
    pub fn ihop_adaptive_adjustment_raise() {
        let gc = make_collector();
        let before = gc.marking_threshold_bytes();
        // Pause well under half the target should raise threshold
        gc.update_ihop(0);
        let after = gc.marking_threshold_bytes();
        assert!(after >= before);
    }

    // -- F-16: the heap is reserved, and committed on demand --

    #[test]
    pub fn a_large_heap_does_not_commit_itself_at_startup() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 512 * 1024 * 1024,
            initial_heap_size: 4 * 1024 * 1024,
            ..small_config()
        });
        assert_eq!(gc.reserved_bytes(), 512 * 1024 * 1024);
        if gc.heap_is_reserved() {
            assert_eq!(
                gc.committed_bytes(),
                4 * 1024 * 1024,
                "-Xmx is address space; -Xms is memory"
            );
        } else {
            assert_eq!(
                gc.committed_bytes(),
                gc.reserved_bytes(),
                "the fallback commits everything, which is the pre-F-16 behaviour"
            );
        }
    }

    #[test]
    pub fn allocating_past_the_initial_prefix_grows_it() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            initial_heap_size: 1024 * 1024,
            ..small_config()
        });
        if !gc.heap_is_reserved() {
            return; // nothing to grow; the fallback is fully committed
        }
        let before = gc.committed_bytes();
        assert_eq!(before, 1024 * 1024, "one region committed to start with");

        // Consume more than one region's worth. Each object is small, so this
        // is a lot of them — but the point is the region CLAIM, so drive it
        // through the allocator rather than reaching into the region table.
        let mut kept: Vec<ObjectRef> = Vec::new();
        for _ in 0..40_000 {
            kept.push(gc.alloc_object(ClassId::new(1), 2));
        }
        assert!(
            gc.committed_bytes() > before,
            "claiming a region above the committed prefix must commit through it \
             ({} -> {})",
            before,
            gc.committed_bytes()
        );
        assert!(gc.committed_bytes() <= gc.reserved_bytes());

        // Every object handed out must be readable — this is the assertion that
        // would fault, not fail, if a region were handed out uncommitted.
        for (i, obj) in kept.iter().enumerate() {
            gc.set_field(*obj, 0, Value::Int(i as i32));
        }
        for (i, obj) in kept.iter().enumerate() {
            assert_eq!(gc.get_field(*obj, 0), Value::Int(i as i32));
        }
    }

    /// `g1-parallel-survivor-claim-commits-the-whole-reservation` (retired
    /// 2026-09-30): the parallel evacuator's first Survivor claim used to take
    /// the highest Free region of the whole TABLE, and `commit_through_region`
    /// then committed the entire reservation below it — one parallel young
    /// pause with survivors committed `-Xmx`. Two shapes: an `-Xms` with Free
    /// room inside it (the claim must stay inside the prefix), and an `-Xms`
    /// of one region (the committed part of the pool is empty, so the claim
    /// must grow the prefix one region at a time rather than jump).
    #[test]
    pub fn a_parallel_young_pause_claims_survivor_space_inside_the_committed_prefix() {
        const MB: usize = 1024 * 1024;
        for initial in [8 * MB, MB] {
            let gc = G1Collector::new(G1CollectorConfig {
                heap_size: 64 * MB,
                initial_heap_size: initial,
                region_size: MB,
                ..small_config()
            });
            if !gc.heap_is_reserved() {
                return; // the fallback commits everything; nothing to observe
            }
            let mut roots: Vec<ObjectRef> = (0..64)
                .map(|i| gc.alloc_object(ClassId::new(1), 1 + i % 4))
                .collect();
            for (i, r) in roots.iter().enumerate() {
                gc.set_field(*r, 0, Value::Int(i as i32));
            }
            let before = gc.committed_bytes();
            let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
            assert!(
                result.stats.objects_copied > 0,
                "fixture: the pause must have copied survivors (initial={initial})"
            );
            assert!(
                gc.count_regions(RegionType::Survivor) > 0,
                "fixture: survivors must have landed in Survivor space (initial={initial})"
            );
            let after = gc.committed_bytes();
            assert!(
                after < gc.reserved_bytes() / 2,
                "one parallel young pause committed {after} of {} bytes (initial={initial}, before={before})",
                gc.reserved_bytes()
            );
            assert!(
                after <= before.max(initial) + 4 * MB,
                "the prefix must grow by what the pause needs, not jump: {before} -> {after} (initial={initial})"
            );
            // Every survivor is readable: the region it landed in is committed.
            for (i, r) in roots.iter().enumerate() {
                assert_eq!(gc.get_field(*r, 0), Value::Int(i as i32), "root {i}");
            }
        }
    }

    #[test]
    pub fn a_humongous_span_commits_every_region_it_covers() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            initial_heap_size: 1024 * 1024,
            ..small_config()
        });
        // Three regions' worth of payload: the span crosses region boundaries
        // and the object's bytes flow straight through them, so a partially
        // committed span is a fault in the middle of an array write.
        let elems = 3 * 1024 * 1024 / 8;
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, elems);
        assert!(
            gc.committed_bytes() >= 3 * 1024 * 1024,
            "the whole span must be committed before a byte of it is written"
        );
        // Touch both ends, which is where a short commit would show.
        gc.set_array_element(arr, 0, Value::Long(0x1111)).unwrap();
        gc.set_array_element(arr, elems - 1, Value::Long(0x2222))
            .unwrap();
        assert_eq!(gc.get_array_element(arr, 0).unwrap(), Value::Long(0x1111));
        assert_eq!(
            gc.get_array_element(arr, elems - 1).unwrap(),
            Value::Long(0x2222)
        );
    }

    /// The JIT read bounds say "a raw load anywhere in this range cannot
    /// fault". Under a reserved heap that is only true of the COMMITTED prefix,
    /// and a bound that outran it would be a fault in compiled code rather than
    /// a failing test.
    #[test]
    pub fn the_published_jit_read_bounds_never_outrun_the_committed_prefix() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            initial_heap_size: 1024 * 1024,
            ..small_config()
        });
        let check = |when: &str| {
            let (base, end) = cratonvm_gc_core::jit_bounds::jit_read_bounds_slot(0);
            if base == 0 && end == 0 {
                return; // another collector in this process owns the table
            }
            if base != gc.arena_base {
                return; // ...or it names a different heap
            }
            let (base2, end2) = cratonvm_gc_core::jit_bounds::jit_read_bounds_slot(0);
            if base2 != gc.arena_base || end != end2 {
                return; // concurrent test updated the table
            }
            assert!(
                end <= gc.arena_base + gc.committed_bytes(),
                "{when}: published read bound {end:#x} is past the committed \
                 prefix {:#x} — a compiled getfield would issue a raw load into \
                 memory that is reserved but not mapped",
                gc.arena_base + gc.committed_bytes()
            );
        };
        check("at construction");
        for _ in 0..40_000 {
            let _ = gc.alloc_object(ClassId::new(1), 2);
            check("after growth");
        }
    }

    #[test]
    pub fn a_trailing_run_of_free_regions_is_returned_to_the_os() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            initial_heap_size: 1024 * 1024,
            ..small_config()
        });
        if !gc.heap_is_reserved() {
            return; // the fallback cannot give pages back and says so
        }
        // Grow the prefix to the whole heap, then use only the first three
        // regions — the classic "finished a burst" shape.
        assert!(gc.commit_through_region(15));
        assert_eq!(gc.committed_bytes(), 16 * 1024 * 1024);
        gc.with_regions_mut(|regions| {
            for (i, r) in regions.iter_mut().enumerate() {
                r.set_region_type(if i < 3 {
                    RegionType::Old
                } else {
                    RegionType::Free
                });
            }
        });

        let released = {
            let regions = gc.regions.read();
            gc.uncommit_trailing_free_regions_within(&regions, true)
        };
        assert_eq!(released, 13 * 1024 * 1024, "regions 3..16 are Free");
        assert_eq!(gc.committed_bytes(), 3 * 1024 * 1024);
        assert_eq!(
            gc.reserved_bytes(),
            16 * 1024 * 1024,
            "the RESERVATION is untouched — the address space is still ours"
        );

        // The published read bound must have followed it down, or compiled code
        // is told it may load from pages that are gone.
        let (base, end) = cratonvm_gc_core::jit_bounds::jit_read_bounds_slot(0);
        if base == gc.arena_base {
            assert!(
                end <= gc.arena_base + gc.committed_bytes(),
                "published bound {end:#x} outruns the committed prefix {:#x}",
                gc.arena_base + gc.committed_bytes()
            );
        }

        // And the heap must still work: claiming a region re-commits it.
        let obj = gc.alloc_object(ClassId::new(1), 2);
        gc.set_field(obj, 0, Value::Int(0x1234));
        assert_eq!(gc.get_field(obj, 0), Value::Int(0x1234));
    }

    #[test]
    pub fn the_shrink_stops_at_the_highest_region_still_in_use() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            initial_heap_size: 1024 * 1024,
            ..small_config()
        });
        if !gc.heap_is_reserved() {
            return;
        }
        assert!(gc.commit_through_region(15));
        // A single live region near the TOP. The committed set has to stay a
        // prefix, so nothing below it can be given back however empty it is —
        // that is the cost of the prefix rule, and it should be visible.
        gc.with_regions_mut(|regions| {
            for r in regions.iter_mut() {
                r.set_region_type(RegionType::Free);
            }
            regions[12].set_region_type(RegionType::Old);
        });
        let regions = gc.regions.read();
        assert_eq!(
            gc.uncommit_trailing_free_regions_within(&regions, true),
            3 * 1024 * 1024,
            "only regions 13..16 are above the highest one in use"
        );
        assert_eq!(gc.committed_bytes(), 13 * 1024 * 1024);
    }

    #[test]
    pub fn the_shrink_never_goes_below_the_initial_heap_size() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            initial_heap_size: 8 * 1024 * 1024,
            ..small_config()
        });
        if !gc.heap_is_reserved() {
            return;
        }
        assert!(gc.commit_through_region(15));
        gc.with_regions_mut(|regions| {
            for r in regions.iter_mut() {
                r.set_region_type(RegionType::Free);
            }
        });
        let regions = gc.regions.read();
        assert_eq!(
            gc.uncommit_trailing_free_regions_within(&regions, true),
            8 * 1024 * 1024,
        );
        assert_eq!(
            gc.committed_bytes(),
            8 * 1024 * 1024,
            "-Xms is a floor: an operator who asked for an initial heap asked \
             not to pay for growing back into it"
        );
    }

    #[test]
    pub fn the_shrink_is_opt_in() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            initial_heap_size: 1024 * 1024,
            ..small_config()
        });
        if !gc.heap_is_reserved() {
            return;
        }
        assert!(gc.commit_through_region(15));
        gc.with_regions_mut(|regions| {
            for r in regions.iter_mut() {
                r.set_region_type(RegionType::Free);
            }
        });
        let regions = gc.regions.read();
        assert_eq!(
            gc.uncommit_trailing_free_regions_within(&regions, false),
            0,
            "growth on demand carries F-16 on its own; the shrink is the half \
             whose failure mode is a fault, so it is off unless asked for"
        );
        assert_eq!(gc.committed_bytes(), 16 * 1024 * 1024);
    }

    #[test]
    pub fn an_initial_size_at_the_heap_size_commits_everything() {
        // The `CRATONVM_G1_RESERVE_HEAP=0` shape, expressed through the config
        // rather than the flag: nothing is lazy, which is the pre-F-16 world.
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 8 * 1024 * 1024,
            initial_heap_size: 8 * 1024 * 1024,
            ..small_config()
        });
        assert_eq!(gc.committed_bytes(), gc.reserved_bytes());    }

    // -- F-15: adaptive IHOP on the allocation rate --

    #[test]
    pub fn an_unmeasured_ihop_leaves_the_configured_threshold_alone() {
        let mut cfg = small_config();
        cfg.ihop_percent = 50;
        let gc = G1Collector::new(cfg.clone());
        gc.recompute_marking_threshold();
        assert_eq!(
            gc.marking_threshold_bytes(),
            cfg.heap_size / 2,
            "with no cycle measured, the operator's number is the best one \
             available and must stand"
        );
        assert_eq!(gc.ihop_model_state(), (0, 0, 100, 0, 0, 0));
    }

    #[test]
    pub fn a_measured_allocation_rate_starts_marking_earlier() {
        // A high configured IHOP, so the ceiling is not what decides the
        // answer: the point of this test is the MEASUREMENT binding, and with
        // the default 45% ceiling on an 8 MiB heap the operator's number is
        // already earlier than any rate this test would state.
        let cfg = G1CollectorConfig {
            ihop_percent: 95,
            ..small_config()
        };
        let gc = G1Collector::new(cfg.clone());
        let ceiling = gc.marking_threshold_bytes();

        // A cycle that took 100 ms during which the old generation grew by
        // 1 MiB: 10 KiB/ms. The next cycle must start with at least that much
        // room left, so the threshold drops below the configured ceiling.
        gc.fold_mark_cycle_sample(100, 1024 * 1024);
        let after = gc.marking_threshold_bytes();
        assert!(
            after < ceiling,
            "a measured growth rate must reserve headroom (ceiling {ceiling}, \
             threshold {after})"
        );
        let (mark_ms, rate, boost, late, disengaged, backoff) = gc.ihop_model_state();
        assert_eq!(
            (mark_ms, rate, boost, late, disengaged, backoff),
            (100, 10, 100, 0, 0, 0)
        );
        assert_eq!(
            after,
            cfg.heap_size - 10 * 100 * 1024,
            "threshold is heap minus (rate x duration x margin)"
        );
    }

    /// The finding itself: the OLD model raised the threshold when pauses were
    /// fast, regardless of how fast the old generation was filling. The new one
    /// must not — that combination is precisely the workload that OOMs with a
    /// mostly-dead heap.
    #[test]
    pub fn a_fast_pause_no_longer_raises_the_marking_threshold() {
        let gc = G1Collector::new(G1CollectorConfig {
            ihop_percent: 95,
            ..small_config()
        });
        gc.fold_mark_cycle_sample(100, 1024 * 1024);
        let planned = gc.marking_threshold_bytes();

        // The legacy arm, for contrast: a pause well under half the goal raises
        // the threshold on no evidence about the old generation at all.
        gc.update_ihop(0);
        assert!(
            gc.marking_threshold_bytes() > planned,
            "test setup: the legacy pause-time model is supposed to raise here"
        );

        // The new model re-plans from the same measurement and puts it back.
        gc.recompute_marking_threshold();
        assert_eq!(
            gc.marking_threshold_bytes(),
            planned,
            "pause time must not move the marking threshold"
        );
    }

    #[test]
    pub fn running_out_of_to_space_makes_the_next_cycle_start_earlier() {
        let gc = G1Collector::new(small_config());
        gc.fold_mark_cycle_sample(100, 1024 * 1024);
        let before = gc.marking_threshold_bytes();
        assert_eq!(
            before,
            gc.ihop_static_ceiling(),
            "test setup: on this heap the configured ceiling is earlier than the measured rate asks for, so the threshold sits AT the ceiling -- which is exactly the case in which a headroom-scaled penalty was swallowed and moved nothing"
        );

        gc.note_to_space_exhausted();
        let after = gc.marking_threshold_bytes();
        assert!(
            after < before,
            "an evacuation failure is the one direct observation that the last \
             cycle started too late ({before} -> {after})"
        );
        let (_, _, boost, late, _, _) = gc.ihop_model_state();
        assert_eq!((boost, late), (125, 1), "the margin grows and is counted");

        // ...and a cycle that completes without one gives a quarter of the
        // penalty back, so a transient burst does not pessimise it forever.
        gc.fold_mark_cycle_sample(100, 1024 * 1024);
        let (_, _, boost_after, _, _, _) = gc.ihop_model_state();
        assert!(
            (100..125).contains(&boost_after),
            "a clean cycle must decay the penalty toward 100 (got {boost_after})"
        );
    }

    #[test]
    pub fn the_adaptive_threshold_stays_between_its_floor_and_the_configured_ceiling() {
        let mut cfg = small_config();
        cfg.ihop_percent = 50;
        let gc = G1Collector::new(cfg.clone());
        let ceiling = cfg.heap_size / 2;
        let floor = (cfg.heap_size / 100).max(cfg.region_size);

        // A growth rate that would demand more headroom than the whole heap.
        gc.fold_mark_cycle_sample(1000, cfg.heap_size * 4);
        assert_eq!(
            gc.marking_threshold_bytes(),
            floor,
            "the threshold must not decay to zero — the VM reads a zero \
             threshold as 'marking disabled', permanently"
        );

        // A workload that promotes nothing during marking asks for no headroom,
        // and must not be allowed to float above the configured IHOP: G1 has no
        // full-GC fallback.
        let quiet = G1Collector::new(cfg.clone());
        quiet.fold_mark_cycle_sample(1000, 0);
        assert_eq!(quiet.marking_threshold_bytes(), ceiling);
    }

    #[test]
    pub fn an_untimed_mark_cycle_is_not_folded_in() {
        let gc = G1Collector::new(small_config());
        gc.fold_mark_cycle_sample(100, 1024 * 1024);
        let planned = gc.marking_threshold_bytes();
        let state = gc.ihop_model_state();

        // A cycle that completed inside one millisecond measures neither
        // quantity; folding a zero would drag both estimates toward zero and
        // quietly retire the headroom.
        gc.fold_mark_cycle_sample(0, 64 * 1024 * 1024);
        assert_eq!(gc.marking_threshold_bytes(), planned);
        assert_eq!(gc.ihop_model_state(), state);
    }

    // -- F-18: adaptive tenuring --

    #[test]
    pub fn the_tenuring_threshold_starts_at_the_configured_promotion_age() {
        let gc = G1Collector::new(G1CollectorConfig {
            promotion_age: 7,
            ..small_config()
        });
        assert_eq!(gc.tenuring_threshold(), 7);
        let (threshold, hist) = gc.tenuring_state();
        assert_eq!(threshold, 7);
        assert!(hist.iter().all(|&b| b == 0), "the histogram starts empty");
    }

    #[test]
    pub fn survivors_that_fit_keep_the_configured_threshold() {
        let gc = G1Collector::new(small_config());
        let configured = gc.config.promotion_age;
        // Well under the survivor target: an eighth of the young generation.
        let target = gc.survivor_target_bytes();
        gc.note_survivor_age(1, target / 4);
        gc.update_tenuring_threshold();
        assert_eq!(
            gc.tenuring_threshold(),
            configured,
            "a pause whose survivors comfortably fit must not tenure early"
        );
    }

    #[test]
    pub fn survivors_that_overflow_the_target_tenure_earlier() {
        let gc = G1Collector::new(small_config());
        let configured = gc.config.promotion_age;
        let target = gc.survivor_target_bytes();

        // Ages 1 and 2 each hold two thirds of the target, so the cumulative
        // total first exceeds it at age 2 — everything at or above 2 tenures.
        gc.note_survivor_age(1, target * 2 / 3);
        gc.note_survivor_age(2, target * 2 / 3);
        gc.update_tenuring_threshold();
        assert_eq!(gc.tenuring_threshold(), 2);
        assert!(
            gc.tenuring_threshold() < configured,
            "the whole point is to stop copying objects that will not fit"
        );
    }

    #[test]
    pub fn the_histogram_is_consumed_by_each_pause_and_does_not_accumulate() {
        let gc = G1Collector::new(small_config());
        let target = gc.survivor_target_bytes();
        gc.note_survivor_age(1, target * 2);
        gc.update_tenuring_threshold();
        assert_eq!(gc.tenuring_threshold(), 1);

        // The LIVE array, not `tenuring_state()` — that returns the retained
        // snapshot of the last pause, which is a different question.
        assert!(
            gc.survivor_age_bytes
                .iter()
                .all(|b| b.load(Ordering::Relaxed) == 0),
            "a histogram that is filled but never cleared grows across pauses \
             and makes every later threshold wrong"
        );
        // ...and the snapshot kept for diagnostics must hold what was cleared.
        // A diagnostic that is structurally always zero is worse than none,
        // because a zero reads as an answer.
        let (_, snapshot) = gc.tenuring_state();
        assert_eq!(
            snapshot[1],
            target * 2,
            "the retained snapshot is the histogram the pause actually had"
        );

        // A quiet pause therefore recovers the configured threshold rather than
        // staying pinned by the previous one's burst.
        gc.update_tenuring_threshold();
        assert_eq!(gc.tenuring_threshold(), gc.config.promotion_age);
    }

    #[test]
    pub fn the_adaptive_threshold_never_exceeds_the_configured_one_nor_reaches_zero() {
        let gc = G1Collector::new(G1CollectorConfig {
            promotion_age: 3,
            ..small_config()
        });
        // Enormous pressure at age 1: the rule wants to tenure as early as it
        // can, but promoting on the FIRST collection would defeat generational
        // filtering entirely.
        gc.note_survivor_age(1, usize::MAX / 4);
        gc.update_tenuring_threshold();
        assert_eq!(gc.tenuring_threshold(), 1);

        // And an empty histogram must not float the threshold above what the
        // operator asked for.
        gc.update_tenuring_threshold();
        assert_eq!(gc.tenuring_threshold(), 3);
    }

    #[test]
    pub fn the_flag_off_arm_uses_the_configured_promotion_age() {
        let gc = G1Collector::new(small_config());
        let target = gc.survivor_target_bytes();
        gc.note_survivor_age(1, target * 4);
        gc.update_tenuring_threshold();
        // The stored value moved...
        assert_eq!(gc.tenuring_threshold.load(Ordering::Relaxed), 1);
        // ...and the accessor reports it only while the flag is on, which it is
        // by default. With it off the configured age is what the evacuators see.
        {
            assert_eq!(gc.tenuring_threshold(), 1);
        }
    }

    /// A real pause must fill the histogram — the policy above is only worth
    /// anything if the evacuators actually report what they copied.
    ///
    /// The histogram cannot be inspected AFTER a pause: every driver funnels
    /// through `record_collection_with_phases`, which consumes and clears it.
    /// So the observable is the THRESHOLD, and the pause is arranged to produce
    /// enough survivors to move it — which is also the end-to-end claim worth
    /// making.
    ///
    /// gci i21/fx: with the young-pause-target loop OFF for the pause. The
    /// survivor target is read from `young_target_regions` AFTER the pause's
    /// sizing step (`record_collection_with_phases_within` re-sizes the young
    /// target before `update_tenuring_threshold`, the order HotSpot's
    /// `update_survivors_policy` reads `young_list_target_length` in), so the
    /// `store(1)` below survived only while the old 20 % loop happened to
    /// leave a tiny target near 1. Since the HotSpot young model went default
    /// ON (2026-10-15) the pause re-sizes it from the predictors (one cheap
    /// pause predicts a large young length), the survivor target grows with
    /// it, and the threshold rightly stays at 3. The claim under test is the evacuator's age
    /// report, not the sizing policy, so the sizing step is held still.
    #[test]
    pub fn a_real_pause_reports_the_ages_it_copied() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_G1_YOUNG_PAUSE_TARGET", Some("0"))],
            a_real_pause_reports_the_ages_it_copied_inner,
        );
    }

    pub fn a_real_pause_reports_the_ages_it_copied_inner() {
        let gc = make_collector();
        assert_eq!(
            gc.tenuring_threshold(),
            gc.config.promotion_age,
            "test setup: nothing has moved the threshold yet"
        );

        // Shrink the young target so the survivor target is as small as it
        // gets: gci i22/sz, HotSpot's `ceil(young / 8)` regions at
        // `TargetSurvivorRatio` 50 %, i.e. half a region for a young target of
        // one (it was `region / 8` before, which one array could overflow).
        gc.young_target_regions.store(1, Ordering::Relaxed);
        let target = gc.survivor_target_bytes();
        assert_eq!(target, gc.config.region_size / 2);

        // Live arrays that TOGETHER exceed the survivor target, each a quarter
        // region, so none is humongous (humongous objects are never evacuated
        // and would report no age at all): half a region is the largest
        // non-humongous object, and it cannot exceed half a region by itself.
        let elems = gc.config.region_size / 4 / 8;
        let count = target / (elems * 8) + 1;
        let mut roots: Vec<ObjectRef> = (0..count)
            .map(|_| gc.alloc_array(ClassId::new(0), ArrayElementType::Long, elems))
            .collect();

        gc.young_collection_serial(&mut roots, &NoopMonitors);

        assert_eq!(
            gc.survivor_target_bytes(),
            target,
            "test setup: the pause must not re-size the young target the \
             survivor target is read from"
        );
        assert_eq!(
            gc.tenuring_threshold(),
            1,
            "the arrays survived at age 1 and together exceed the survivor \
             target ({target} bytes), so the next pause must tenure at 1 — if \
             the evacuator reported nothing, the threshold would still be {}",
            gc.config.promotion_age
        );
    }

    // T19.3.G1 — GC allocation-storm follow-ups.

    #[test]
    pub fn t19_default_ihop_is_70_percent() {
        // T19.3.G1 raised the default IHOP from 45% → 70% so static-init
        // bursts on 256 MiB heaps don't fire concurrent marking while
        // the heap is still essentially empty.
        let cfg = G1CollectorConfig::default();
        assert_eq!(cfg.ihop_percent, 70);
    }

    #[test]
    pub fn t19_default_ihop_threshold_bytes_is_70_percent_of_heap() {
        let cfg = G1CollectorConfig::default();
        let gc = G1Collector::new(cfg.clone());
        // 70 % of 256 MiB == 179.2 MiB. Accept any value within 1% of
        // the exact calculation so future rounding tweaks don't break.
        let expected = cfg.heap_size * 70 / 100;
        let actual = gc.marking_threshold_bytes();
        let diff = actual.abs_diff(expected);
        assert!(
            diff * 100 <= expected,
            "ihop threshold {actual} deviates from {expected} by more than 1 %"
        );
    }

    #[test]
    pub fn t19_default_ihop_does_not_fire_at_40_percent_occupancy() {
        // A Quarkus static-init burst at 40 % old-gen occupancy used to
        // trigger concurrent marking under the old 45 % threshold. Post-fix
        // the heap must tolerate 40 % without firing.
        let gc = G1Collector::new(G1CollectorConfig::default());
        let forty_percent = gc.config.heap_size * 40 / 100;
        gc.old_gen_bytes.store(forty_percent, Ordering::Relaxed);
        assert!(
            !gc.check_ihop(),
            "IHOP fired at 40% occupancy with 70% threshold"
        );
    }

    #[test]
    pub fn t19_default_ihop_still_fires_at_75_percent_occupancy() {
        // Above 70 %, the threshold is crossed and concurrent marking
        // must start. This keeps the safety net intact.
        let gc = G1Collector::new(G1CollectorConfig::default());
        let seventy_five_percent = gc.config.heap_size * 75 / 100;
        gc.old_gen_bytes
            .store(seventy_five_percent, Ordering::Relaxed);
        assert!(
            gc.check_ihop(),
            "IHOP didn't fire at 75% occupancy with 70% threshold"
        );
    }

    // -- Region pinning --

    #[test]
    pub fn region_pinning() {
        let gc = make_collector();
        assert!(!gc.is_pinned(0));
        gc.pin_region(0);
        assert!(gc.is_pinned(0));
        gc.unpin_region(0);
        assert!(!gc.is_pinned(0));
    }

    #[test]
    pub fn pinned_region_not_collected() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(77));

        // Find the region and pin it
        let region_idx = {
            let regions = gc.regions.read();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        gc.pin_region(region_idx);

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        // Pinned region should not be collected
        assert_eq!(result.stats.objects_copied, 0);
        // Object should still be at the same address
        assert_eq!(roots[0].as_ptr(), obj.as_ptr());
    }

    // -- Incomplete JIT root coverage must stop evacuation entirely --

    /// The defect both G1 full-suite crash reports name: G1 evacuated while
    /// the collection's own root scan had already recorded that it could not
    /// enumerate every live compiled frame. `jit_pinned_region_set` cannot
    /// cover that case — the frames in question are exactly the ones the
    /// conservative scan never reached, so they publish no address to pin.
    ///
    /// The LEVER's two arms, stated on the pure decision function because
    /// `gc_flags()` latches process-wide and a test cannot flip it from inside
    /// one. Off (the shipped default) G1 evacuates regardless; on, it refuses.
    #[test]
    pub fn the_coverage_lever_gates_only_the_refusal() {
        let reason = Some(cratonvm_gc_core::gc_quiescence::incomplete_reason::UNREGISTERED_JIT_FRAME);
        assert_eq!(
            G1Collector::refuse_evacuation(reason, false),
            None,
            "default: an incomplete root set is RECORDED, not acted on — the \
             conservative scan that produced the pins already covered it"
        );
        assert_eq!(
            G1Collector::refuse_evacuation(reason, true),
            reason,
            "lever on: this pause must not evacuate"
        );
        assert_eq!(
            G1Collector::refuse_evacuation(None, true),
            None,
            "lever on but coverage complete: evacuate normally"
        );
    }

    /// The default must still EVACUATE on an incomplete-coverage pause — the
    /// measured alternative starves reclamation (330263 of 330264 pauses report
    /// incomplete on `MovingYoungConcurrentProbe`). Stated through
    /// `collect_garbage` because that is the single entry to G1's two
    /// object-moving paths.
    #[test]
    pub fn incomplete_root_coverage_still_evacuates_by_default() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(77));

        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        cratonvm_gc_core::gc_quiescence::mark_moving_young_coverage_incomplete_because(
            cratonvm_gc_core::gc_quiescence::incomplete_reason::UNREGISTERED_JIT_FRAME,
        );

        let mut roots = vec![obj];
        let result = <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::collect_garbage(
            &gc,
            &stw(),
            &mut roots,
            &NoopMonitors,
        );

        assert!(result.stats.objects_copied >= 1);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(77));

        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
    }

    /// …but the pause must SAY that its root set was incomplete. A collector
    /// that keeps that to itself is why both 2026-08 full-suite G1 pages had to
    /// infer the mechanism from a crash dump.
    #[test]
    pub fn incomplete_root_coverage_is_recorded_even_when_it_is_not_acted_on() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);

        let (pauses_before, incomplete_before) = cratonvm_gc_core::gc_metrics::g1_pause_coverage_counts();

        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        cratonvm_gc_core::gc_quiescence::mark_moving_young_coverage_incomplete_because(
            cratonvm_gc_core::gc_quiescence::incomplete_reason::FOREIGN_INNERMOST_RBP,
        );

        let mut roots = vec![obj];
        let _ = <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::collect_garbage(
            &gc,
            &stw(),
            &mut roots,
            &NoopMonitors,
        );

        let d = cratonvm_gc_core::gc_metrics::last_collector_decision().expect("a decision was recorded");
        assert_eq!(d.backend, "g1");
        assert_eq!(
            d.incomplete_reason,
            cratonvm_gc_core::gc_quiescence::incomplete_reason::FOREIGN_INNERMOST_RBP,
            "the obligation that failed must reach the decision record"
        );

        let (pauses, incomplete) = cratonvm_gc_core::gc_metrics::g1_pause_coverage_counts();
        assert!(pauses > pauses_before, "the pause must be counted");
        assert!(
            incomplete > incomplete_before,
            "and counted as incomplete — the rate is the whole point of the counter"
        );

        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
    }

    /// Complete coverage is the common case and must be untouched: the same
    /// collection with the flag clear still evacuates.
    #[test]
    pub fn complete_root_coverage_still_evacuates() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(77));

        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        assert!(G1Collector::root_coverage_incomplete_reason().is_none());

        let mut roots = vec![obj];
        let result = <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::collect_garbage(
            &gc,
            &stw(),
            &mut roots,
            &NoopMonitors,
        );

        assert!(result.stats.objects_copied >= 1);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(77));
    }

    /// `force_non_moving_jit_roots` is set by the root gatherer's OSR-shadow
    /// fallback WITHOUT a reason code. It is a second, independent way for the
    /// root set to be incomplete, and G1 must honour it too — with a reason
    /// that is not `NONE`, so the record never claims a complete proof while
    /// refusing to act on one.
    #[test]
    pub fn force_non_moving_jit_roots_alone_blocks_g1_evacuation() {
        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
        assert!(G1Collector::root_coverage_incomplete_reason().is_none());

        cratonvm_gc_core::gc_quiescence::set_force_non_moving_jit_roots();
        assert_eq!(
            G1Collector::root_coverage_incomplete_reason(),
            Some(cratonvm_gc_core::gc_quiescence::incomplete_reason::OSR_SHADOW),
        );
        cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
    }

    /// The empty-publication fail-safe's two arms, stated on the pure decision
    /// function for the same reason as `the_coverage_lever_gates_only_the_refusal`:
    /// `gc_flags()` latches process-wide and a test cannot flip it from inside
    /// one.
    ///
    /// Both G1 refusals are opt-in, for different measured reasons. The
    /// coverage lever because its detection fires on ~99.99% of pauses and so
    /// selects nothing; this one because refusing on a PERSISTENTLY empty
    /// publication starves the heap — on `PolynomialTest` it turns one wrong
    /// answer into 1444 no-op pauses and `OutOfMemoryError` on 8 tests.
    #[test]
    pub fn the_empty_publication_refusal_is_opt_in() {
        assert!(
            !G1Collector::refuse_evacuation_for_empty_publication(true, false),
            "default: the unsafe state is DETECTED and counted, not acted on -- \
             declining the pause does not repair the scan, it only trades a \
             wrong answer for an OutOfMemoryError"
        );
        assert!(
            G1Collector::refuse_evacuation_for_empty_publication(true, true),
            "lever on: this pause must not evacuate, so a failure that survives \
             it is not caused by evacuating under an empty publication"
        );
        assert!(
            !G1Collector::refuse_evacuation_for_empty_publication(false, false),
            "publication non-empty: the scan produced roots, pin them and \
             evacuate the rest as usual"
        );
        assert!(
            !G1Collector::refuse_evacuation_for_empty_publication(false, true),
            "publication non-empty: the lever changes nothing here"
        );
    }

    /// No compiled frame is live, so an empty publication is genuinely EMPTY
    /// and not merely unobserved. This is the overwhelmingly common case and it
    /// must not trip the fail-safe -- otherwise every pause on a `--nojit` run
    /// would refuse to evacuate.
    #[test]
    pub fn a_pause_with_no_live_compiled_frame_is_not_an_empty_publication() {
        let gc = make_collector();
        cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
        assert!(
            !gc.empty_jit_publication(),
            "no JIT frame live: pin_addrs=0 means none, not unknown"
        );
    }

    /// An unregistered compiled frame counts as live. `is_active()` only sees
    /// JIT entries that pushed a `JitEntryGuard`; a frame reached without one
    /// holds the only reference to an object exactly as well, which is why
    /// `empty_jit_publication` asks both detectors. Per-thread flag, so this
    /// cannot disturb a concurrently-running test.
    #[test]
    pub fn an_unregistered_compiled_frame_also_makes_an_empty_publication_unsafe() {
        let gc = make_collector();
        cratonvm_gc_core::gc_quiescence::clear_pinned_jit_roots();
        cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
        assert!(!gc.empty_jit_publication());

        cratonvm_gc_core::gc_quiescence::set_unregistered_jit_frame_on_stack();
        let detected = gc.empty_jit_publication();
        cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
        assert!(
            detected,
            "an unregistered frame with nothing published is the same hazard as \
             a registered one"
        );
    }

    /// The end-to-end statement of what the SHIPPED default does, which is not
    /// what a reader hoping for a fix would guess: G1 detects the unsafe state
    /// and evacuates anyway.
    ///
    /// This is a live defect, deliberately pinned rather than papered over.
    /// `empty_jit_publication()` is true — a compiled frame is running and the
    /// conservative scan published nothing — and the object still moves, so a
    /// register or spill slot holding the only reference now names the old
    /// address. That is exactly the mechanism behind `PolynomialTest`
    /// formatting `%d` against a bare `java.lang.Object`.
    ///
    /// What this does NOT say is that the state is reachable in production. It
    /// is constructed here directly — `enter()` plus an empty pin map — because
    /// the point is G1's response to it, not its cause. In a real run the pin
    /// map is filled by `memory::roots::collect_roots`, which since the
    /// precise-only fix always runs the conservative JIT scan under G1, so an
    /// empty publication with a live frame should not occur at all. That is why
    /// `record_g1_pause_empty_jit_publication` counts it on every run: this
    /// test states the consequence if it ever does.
    ///
    /// Declining the pause is not the answer and the measurement says why:
    /// under `CRATONVM_G1_PIN_EMPTY_PUBLICATION`, back when the state was
    /// permanent, the same test took 1444 consecutive refused pauses and died
    /// of `OutOfMemoryError` on 8 tests. A refusal reclaims nothing.
    ///
    /// Driven through `collect_garbage` because that is G1's single production
    /// entry to both object-moving paths, exactly as
    /// `incomplete_root_coverage_still_evacuates_by_default` is.
    ///
    /// Under `cfg(test)` the JIT-active depth is thread-local, so the `enter()`
    /// here is invisible to other tests. The pin map is process-wide; this
    /// thread clears its own entry, and no other gc test publishes one except
    /// `zgc`'s, which clears again immediately.
    #[test]
    pub fn a_live_compiled_frame_with_an_empty_publication_still_evacuates_by_default() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(77));
        let before = obj.as_ptr() as usize;

        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        cratonvm_gc_core::gc_quiescence::clear_pinned_jit_roots();
        cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();

        cratonvm_gc_core::gc_quiescence::enter();
        assert!(
            gc.empty_jit_publication(),
            "precondition: a compiled frame is live and nothing is published, \
             which is the state the collector cannot safely evacuate against"
        );
        let mut roots = vec![obj];
        let result = <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::collect_garbage(
            &gc,
            &stw(),
            &mut roots,
            &NoopMonitors,
        );
        cratonvm_gc_core::gc_quiescence::leave();

        assert_eq!(
            result.stats.objects_copied, 1,
            "shipped default: the pause evacuates despite the unproven root set"
        );
        assert_ne!(
            roots[0].as_ptr() as usize,
            before,
            "and the object MOVES -- the `roots` copy is rewritten, but a JIT \
             register or spill slot holding the only reference is not, which is \
             the defect this record is open for"
        );
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(77));
    }

    /// r12w1 g1store: a compiled frame live WITHOUT a `JitEntryGuard` (the A5
    /// case the root scan reports through `unregistered_jit_frame_on_stack`)
    /// has its published pins honoured. Before, the pin snapshot was read only
    /// under `is_active()`, so with no guarded entry the object its raw slots
    /// name was evacuated out from under them.
    #[test]
    pub fn r12w1_an_unregistered_compiled_frame_alone_still_pins_its_roots() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(77));
        let before = obj.as_ptr() as usize;

        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        cratonvm_gc_core::gc_quiescence::clear_pinned_jit_roots();
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[before]);
        cratonvm_gc_core::gc_quiescence::set_unregistered_jit_frame_on_stack();
        assert!(
            !cratonvm_gc_core::gc_quiescence::is_active(),
            "precondition: no guarded JIT entry on this test thread"
        );
        let mut roots = vec![obj];
        let _ = <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::collect_garbage(
            &gc,
            &stw(),
            &mut roots,
            &NoopMonitors,
        );
        cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[]);

        {
            assert_eq!(
                roots[0].as_ptr() as usize,
                before,
                "the unregistered frame's pinned object must stay where its raw slots point"
            );
        }
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(77));
    }

    /// r12w2 g1store: a pin a PEER published is honoured even when neither
    /// detector fires on the initiator (no guarded JIT entry anywhere, and the
    /// initiator's own stack holds no compiled frame). That is a peer parked or
    /// blocked in a compiled frame without a `JitEntryGuard`: its deposit found
    /// the frame and published the pin, and the pause used to ignore it.
    #[test]
    pub fn r12w2_a_published_pin_is_honoured_with_no_detector_on_the_initiator() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(78));
        let before = obj.as_ptr() as usize;

        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        cratonvm_gc_core::gc_quiescence::clear_pinned_jit_roots();
        cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[before]);
        assert!(
            !cratonvm_gc_core::gc_quiescence::is_active(),
            "precondition: no guarded JIT entry on this test thread"
        );
        let mut roots = vec![obj];
        let _ = <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::collect_garbage(
            &gc,
            &stw(),
            &mut roots,
            &NoopMonitors,
        );
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[]);

        {
            assert_eq!(
                roots[0].as_ptr() as usize,
                before,
                "a published pin keeps its object in place whoever published it"
            );
        }
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(78));
    }

    /// The two refusals must stay distinguishable in the decision report. They
    /// mean different things -- "the roots are not rewritable" (normal, ~100%
    /// of pauses) versus "there were no roots and a compiled frame was running"
    /// (rare, and the one that corrupts the heap) -- and a report that spells
    /// them the same way is how the rare one gets triaged as the normal one.
    #[test]
    pub fn the_two_g1_refusals_are_distinguishable_in_the_report() {
        use cratonvm_gc_core::gc_metrics::decision_reason as dr;
        assert_ne!(
            dr::NON_MOVING_G1_ROOT_COVERAGE_INCOMPLETE,
            dr::NON_MOVING_G1_EMPTY_JIT_PUBLICATION
        );
        assert_ne!(
            dr::label(dr::NON_MOVING_G1_ROOT_COVERAGE_INCOMPLETE),
            dr::label(dr::NON_MOVING_G1_EMPTY_JIT_PUBLICATION)
        );
        assert_ne!(
            dr::label(dr::NON_MOVING_G1_EMPTY_JIT_PUBLICATION),
            "unknown"
        );
        assert!(
            !dr::is_moving(dr::NON_MOVING_G1_EMPTY_JIT_PUBLICATION),
            "the pause moves nothing, so it is not a moving young collection"
        );

        use cratonvm_gc_core::gc_metrics::g1_degraded as gd;
        assert_eq!(
            gd::labels(gd::EMPTY_COLLECTION_SET | gd::JIT_PUBLICATION_EMPTY).len(),
            2,
            "both bits are set on a refused pause and both must have labels"
        );
        assert_ne!(gd::ALL & gd::JIT_PUBLICATION_EMPTY, 0);
    }

    // -- Reference arrays as a generic Value store --

    /// A REFERENCE array element is a raw 8-byte pointer, but every backend
    /// must accept an arbitrary `Value` in one: natives across the tree use a
    /// reference array as a generic `Value` store, and the generational heap
    /// has always honoured that by auto-boxing into an `AUTOBOX_CLASS_ID`
    /// wrapper.
    ///
    /// G1 had neither half. `Value::Long(42)` was encoded as `0u64` — the
    /// `_ => 0u64` arm of `array_element_to_unaligned_ptr` — and read back as
    /// `Value::Object(None)`. In Java that made
    /// `stream.mapToDouble(Double::doubleValue).toArray()` return all zeros
    /// under `-XX:+UseG1GC` and the right values under the default collector,
    /// with no collection involved: deterministic, and identical with `--nojit`
    /// and at a heap large enough that no GC runs.
    #[test]
    pub fn reference_array_round_trips_every_value_kind() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 6);
        let obj = gc.alloc_object(ClassId::new(2), 1);

        let cases = [
            Value::Long(-42),
            Value::Double(2.5),
            Value::Int(7),
            Value::Float(1.5),
            Value::Object(Some(obj)),
            Value::Object(None),
        ];
        for (i, v) in cases.iter().enumerate() {
            <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::set_array_element(&gc, arr, i, *v)
                .expect("in bounds");
        }
        for (i, want) in cases.iter().enumerate() {
            let got =
                <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::get_array_element(&gc, arr, i)
                    .expect("in bounds");
            match (want, got) {
                (Value::Object(Some(w)), Value::Object(Some(g))) => {
                    assert_eq!(w.as_ptr(), g.as_ptr(), "element {i}: reference identity")
                }
                (w, g) => assert_eq!(*w, g, "element {i} did not round-trip"),
            }
        }
    }

    /// The boxing must not leak into a PRIMITIVE array: those store the value
    /// directly and a wrapper there would be a wrong-width write.
    #[test]
    pub fn primitive_arrays_are_not_auto_boxed() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(1), ArrayElementType::Double, 2);
        <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::set_array_element(
            &gc,
            arr,
            0,
            Value::Double(3.25),
        )
        .expect("in bounds");
        assert_eq!(
            <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::get_array_element(&gc, arr, 0),
            Ok(Value::Double(3.25))
        );
    }

    /// An ordinary object stored in a reference array must come back as
    /// itself, not be mistaken for a wrapper. The un-box keys on
    /// `AUTOBOX_CLASS_ID`, so this pins that a real object's class id cannot
    /// collide with it.
    #[test]
    pub fn an_ordinary_object_is_not_mistaken_for_an_auto_box() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 1);
        let obj = gc.alloc_object(ClassId::new(3), 1);
        gc.set_field(obj, 0, Value::Int(99));
        <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::set_array_element(
            &gc,
            arr,
            0,
            Value::Object(Some(obj)),
        )
        .expect("in bounds");
        match <G1Collector as cratonvm_gc_core::collector::GarbageCollector>::get_array_element(&gc, arr, 0) {
            Ok(Value::Object(Some(got))) => assert_eq!(got.as_ptr(), obj.as_ptr()),
            other => panic!("expected the object back, got {other:?}"),
        }
    }

    #[test]
    pub fn pin_out_of_bounds() {
        let gc = make_collector();
        gc.pin_region(9999); // should not panic
        assert!(!gc.is_pinned(9999));
    }

    // -- INT-3: frozen-peer TLAB skip regions --

    /// INT-3 — a region holding a published un-retired TLAB tail must be
    /// excluded from the CSet: its (frozen) owner resumes bump-allocating
    /// into `[cursor, end)` after the pause, and objects it already
    /// allocated there may be addressed by un-rewritable frozen-peer state.
    /// Deliberately run WITHOUT `gc_quiescence` JIT activity: the exclusion
    /// must hold even when no thread is in JIT (a blocked thread that missed
    /// its retire publishes a tail too).
    #[test]
    pub fn jit_tlab_skip_region_excluded_from_cset() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(41));

        // Carve a mutator TLAB from the current Eden region — the region
        // cursor now covers `len` bytes the "mutator" never initialized,
        // exactly the state a frozen in-JIT peer leaves behind.
        let (tlab_ptr, len) = gc.refill_tlab(4096).expect("TLAB refill");
        let obj_region = {
            let regions = gc.regions.read();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        let tlab_region = gc.lookup_region_for_addr(tlab_ptr as usize).unwrap();
        assert_eq!(
            obj_region, tlab_region,
            "test precondition: object and TLAB share the current Eden region"
        );

        gc.set_jit_tlab_skip_regions(&[(tlab_ptr as usize, tlab_ptr as usize + len)]);
        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(
            result.stats.objects_copied, 0,
            "region holding a frozen TLAB tail must be excluded from the CSet"
        );
        assert_eq!(
            roots[0].as_ptr(),
            obj.as_ptr(),
            "object sharing the frozen peer's Eden region must not move"
        );
        assert_eq!(gc.get_field(obj, 0).as_int(), Some(41));

        // After the (VM-driven) clear, the next collection evacuates normally.
        gc.clear_jit_tlab_skip_regions();
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        assert!(
            result.stats.objects_copied > 0,
            "clearing the skip list must restore normal evacuation"
        );
    }

    /// INT-3 — every linear region walker must stride over a published
    /// frozen TLAB tail instead of parsing its uninitialized bytes (zeroed
    /// test-arena bytes parse as a run of phantom 0-slot objects; real
    /// frozen-peer garbage can desync the stride entirely).
    #[test]
    pub fn walk_objects_skips_published_frozen_tlab_tail() {
        let gc = make_collector();
        let before = gc.alloc_object(ClassId::new(1), 2);
        let (tlab_ptr, len) = gc.refill_tlab(4096).expect("TLAB refill");
        let after = gc.alloc_object(ClassId::new(2), 3);
        let span = (tlab_ptr as usize, tlab_ptr as usize + len);
        assert_eq!(
            gc.lookup_region_for_addr(before.as_ptr() as usize),
            gc.lookup_region_for_addr(after.as_ptr() as usize),
            "test precondition: allocations straddle the carved TLAB in one region"
        );
        assert!(
            after.as_ptr() as usize >= span.1,
            "test precondition: `after` lands beyond the carved TLAB"
        );

        gc.set_jit_tlab_skip_regions(&[span]);
        let walked = gc.walk_objects();
        let ptrs: Vec<usize> = walked.iter().map(|&(p, _)| p as usize).collect();
        assert!(ptrs.contains(&(before.as_ptr() as usize)));
        assert!(
            ptrs.contains(&(after.as_ptr() as usize)),
            "walker must stride over the frozen tail and reach objects behind it"
        );
        assert!(
            ptrs.iter().all(|&p| p < span.0 || p >= span.1),
            "no phantom object may be reported inside the frozen TLAB tail"
        );
        gc.clear_jit_tlab_skip_regions();
    }

    /// gcd d1/e (`gcd-d1e-g1-zero-header-screens-read-num-slots-before-the-mark`):
    /// G1's zeroed / empty-header screens test the mark before `num_slots()`,
    /// so bytes that merely look FORWARDED are never followed. Three shapes:
    /// a forward into unmapped space (the pin9 fault shape), a BUSY-looking
    /// forward (the pin10 spin shape), and a plain forward naming a zeroed
    /// header.
    #[test]
    pub fn zero_header_screens_test_the_mark_before_following_a_forward() {
        // [candidate: class_id 0 | mark][second word][zeroed header word 0][word 1]
        let mut words: [u64; 4] = [0; 4];
        let base = words.as_mut_ptr() as usize;
        let real_zero = base + 16;
        let set = |words: &mut [u64; 4], mark: u32, second: u64| {
            words[0] = (mark as u64) << 32;
            words[1] = second;
        };
        // SAFETY: `base` is the 8-aligned start of `words`; every read below
        // is of a header's first two words, inside the array.
        let header = |at: usize| unsafe { &*(at as *const ObjectHeader) };
        let third_site =
            |h: &ObjectHeader| h.mark_word.load(Ordering::Relaxed) == 0 && header_reads_zeroed(h);

        for (what, mark, second) in [
            ("forward into unmapped space", 0x7673u32, 0x1_0000_0000u64),
            ("BUSY-looking forward", 0x0000_000b, real_zero as u64),
            (
                "forward naming a zeroed header",
                0x0000_0003,
                real_zero as u64,
            ),
        ] {
            set(&mut words, mark, second);
            let h = header(base);
            assert!(
                ObjectHeader::is_forwarded_mark(h.mark()),
                "{what}: the shape under test"
            );
            assert!(
                !header_has_empty_shape(h),
                "{what}: empty-header grid screen"
            );
            assert!(!header_reads_zeroed(h), "{what}: dbg_scan_for_zeroed_refs");
            assert!(!third_site(h), "{what}: the stale-reference report");
        }

        // Real zeroed memory still reads as zeroed / empty everywhere.
        set(&mut words, 0, 0);
        let h = header(base);
        assert!(header_reads_zeroed(h) && third_site(h));
        assert!(header_has_empty_shape(h));
        // A hashed / locked (non-forwarded) mark keeps the first two answers.
        set(&mut words, 0x10, 0);
        let h = header(base);
        assert!(!ObjectHeader::is_forwarded_mark(h.mark()));
        assert!(
            header_reads_zeroed(h),
            "a non-zero, unforwarded mark is legal there"
        );
        assert!(!third_site(h), "the third site also requires a zero mark");
    }

    /// `gengc-r5w3-unload7-g1-remark-may-unregister-layouts-of-unswept-instances`:
    /// the census walk names every object's class, and is granted once per
    /// (old regions reclaimed, retained total) pair.
    #[test]
    pub fn layout_census_walk_names_every_class_and_runs_once_per_reclaim() {
        let gc = make_collector();
        let _a = gc.alloc_object(ClassId::new(4101), 2);
        let _b = gc.alloc_object(ClassId::new(4102), 3);
        let (ids, complete) = gc.object_class_ids();
        assert!(complete, "a freshly allocated heap parses to every cursor");
        assert!(ids.contains(&4101) && ids.contains(&4102), "{ids:?}");
        assert!(!ids.contains(&4103));

        assert!(gc.layout_census_due(1), "first census after a retention");
        assert!(!gc.layout_census_due(1), "nothing changed: no second walk");
        assert!(gc.layout_census_due(2), "a new retention re-arms it");
        gc.old_regions_reclaimed.fetch_add(1, Ordering::Relaxed);
        assert!(gc.layout_census_due(2), "an old reclamation re-arms it");
        assert!(!gc.layout_census_due(2));
        assert_eq!(gc.layout_census_counts(), (1, 0));
    }

    #[test]
    pub fn pin_region_for_addr_keeps_jni_critical_array_in_place() {
        // Step 6 (JEP 423): GetPrimitiveArrayCritical pins the backing array's
        // region so a moving young/mixed collection cannot relocate it before
        // the copy-back at Release. Without the pin the object is evacuated and
        // its Get-time address goes stale (copy-back to a vacated/recycled slot
        // — data loss or corruption).
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(123));
        let addr = obj.as_ptr() as usize;

        let idx = gc
            .pin_region_for_addr(addr)
            .expect("freshly allocated object must live in a region");
        // Resolves to the same region `region_for_ptr` would.
        let expected = {
            let regions = gc.regions.read();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        assert_eq!(idx, expected);
        assert!(gc.is_pinned(idx));

        // A young GC must NOT relocate the pinned array.
        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(
            result.stats.objects_copied, 0,
            "pinned region must be excluded from the collection set"
        );
        assert_eq!(
            roots[0].as_ptr(),
            obj.as_ptr(),
            "pinned JNI-critical array must not move"
        );
        // The data the copy-back would read is intact and at the same address.
        assert_eq!(gc.get_field(obj, 0).as_int(), Some(123));

        gc.unpin_region(idx);
        assert!(!gc.is_pinned(idx));
    }

    /// gci i14/pn: `pin_array_payload_for_critical` pins an ordinary array's
    /// region (a young pause then leaves the body where native code reads
    /// it), refuses a payload that would run past the header's region, and
    /// is refcounted with `unpin_region`.
    #[test]
    pub fn gci_i14_pn_critical_pin_keeps_a_small_array_body_in_place() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 16);
        let addr = arr.as_ptr() as usize;
        // A corrupt length (two regions of payload in an Eden object): no pin.
        assert_eq!(
            gc.pin_array_payload_for_critical(addr, ARRAY_DATA_OFFSET, 2 * gc.config.region_size),
            None
        );
        let idx = gc
            .pin_array_payload_for_critical(addr, ARRAY_DATA_OFFSET, 16 * 4)
            .expect("an Eden array's payload is inside its region");
        assert!(gc.is_pinned(idx));
        // A second section on the same array: two pins, one region.
        assert_eq!(
            gc.pin_array_payload_for_critical(addr, ARRAY_DATA_OFFSET, 16 * 4),
            Some(idx)
        );
        // gci i16/pl: the second pin is a shared-guard one (finding 8).
        assert_eq!(gc.regions.read()[idx].pin_total(), 2);

        let mut roots = vec![arr];
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(result.stats.objects_copied, 0, "the pinned region is out of the CSet");
        assert_eq!(roots[0].as_ptr() as usize, addr, "the critical body did not move");

        gc.unpin_region(idx);
        assert!(gc.is_pinned(idx), "the other section still holds it");
        gc.unpin_region(idx);
        assert!(!gc.is_pinned(idx));
    }

    /// gci i14/pn: a humongous array's critical pin is its start region, and
    /// only when every region the payload reaches is a continuation of the span.
    #[test]
    pub fn gci_i14_pn_critical_pin_covers_a_humongous_span_from_its_start() {
        let gc = make_collector();
        let len = 200_000usize; // 1.6 MB of `long`: a start plus one continuation
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, len);
        let addr = arr.as_ptr() as usize;
        assert_eq!(gc.count_regions(RegionType::HumongousContinuation), 1);
        // Past the span's end (a third region that is not a continuation).
        assert_eq!(
            gc.pin_array_payload_for_critical(addr, ARRAY_DATA_OFFSET, 3 * gc.config.region_size),
            None
        );
        let idx = gc
            .pin_array_payload_for_critical(addr, ARRAY_DATA_OFFSET, len * 8)
            .expect("a humongous payload is one contiguous span");
        {
            let regions = gc.regions.read();
            assert_eq!(regions[idx].region_type(), RegionType::HumongousStart);
            assert_eq!(regions[idx].pin_count, 1);
            assert!(!regions[idx + 1].pinned, "the start region carries the span's pin");
        }
        gc.unpin_region(idx);
        assert!(!gc.is_pinned(idx));
    }

    /// gci i14/pn: the full-compaction pause's old half skips a region a JNI
    /// critical section pins, and takes it again once the pin is released.
    #[test]
    pub fn gci_i14_pn_full_compaction_skips_a_critical_pinned_old_region() {
        let gc = make_collector();
        d10o_mostly_dead_old_regions(&gc, 4);
        gc.pin_region(1);
        let picked = |gc: &G1Collector| -> Vec<usize> {
            let regions = gc.regions.read();
            gc.full_compaction_candidates(&regions, &RegionSet::new(), 0)
                .into_iter()
                .map(|(i, _)| i)
                .collect()
        };
        assert_eq!(picked(&gc), vec![0, 2, 3]);
        gc.unpin_region(1);
        assert_eq!(picked(&gc), vec![0, 1, 2, 3]);
    }

    /// A JNI-pinned young region is excluded from the CSet but its objects may
    /// hold the ONLY reference to objects that ARE collected. Coverage comes
    /// from the remembered set: the cross-region ref store recorded the
    /// pinned holder's region in the target's RSet, so Phase 2's source walk
    /// visits the pinned region, evacuates the referent, and rewrites the
    /// holder's slot in place. (Spotted during the marking-soundness audit as
    /// a suspected gap — this test pins the invariant that makes it a
    /// non-gap. If the RSet ever starts filtering young→young edges, or a
    /// store path skips `post_write_barrier_rset`, this fails with Q freed
    /// under P.)
    #[test]
    pub fn jni_pinned_young_region_holder_keeps_cset_referent_alive() {
        let gc = make_collector();
        // P — the holder — lands in the current Eden region.
        let p = gc.alloc_object(ClassId::new(1), 1);
        let p_region = {
            let regions = gc.regions.read();
            gc.region_for_ptr(&regions, p.as_ptr()).unwrap()
        };
        // Allocate until a NEW Eden region opens so Q is cross-region from P.
        let mut q = gc.alloc_object(ClassId::new(2), 1);
        loop {
            let q_region = {
                let regions = gc.regions.read();
                gc.region_for_ptr(&regions, q.as_ptr()).unwrap()
            };
            if q_region != p_region {
                break;
            }
            q = gc.alloc_object(ClassId::new(2), 1);
        }
        gc.set_field(q, 0, Value::Int(4242));
        // The ONLY path to Q: a field of P (cross-region ref store → the
        // post-write barrier records P's region in Q's region's RSet).
        gc.set_field(p, 0, Value::Object(Some(q)));

        // JNI critical section pins P's region (GetPrimitiveArrayCritical
        // shape: the pin covers the whole region, holder objects included).
        gc.pin_region(p_region);

        // Young collection with NO roots: P's region is excluded from the
        // CSet by the pin; Q must still be evacuated and P's slot rewritten.
        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        let q_new = result
            .pointer_map
            .get(&(q.as_ptr() as usize))
            .copied()
            .expect("Q, referenced only from the pinned region, must be evacuated");
        let q_new_ref = unsafe { ObjectRef::from_raw(q_new as *mut u8) };
        assert_eq!(
            gc.get_field(p, 0),
            Value::Object(Some(q_new_ref)),
            "pinned holder's slot must be rewritten to Q's new location"
        );
        assert_eq!(gc.get_field(q_new_ref, 0).as_int(), Some(4242));
        gc.unpin_region(p_region);
    }

    /// r23 wave 3 review: two G1 collectors alive at once leave the JIT's
    /// process tables POISONED (`JIT_G1_BARRIER` and `JIT_G1_YOUNG`), whichever
    /// is dropped first, and never owned by a dropped collector. Robust to the
    /// other tests of this binary building collectors concurrently: once two
    /// collectors have both published while alive, the claims are contended,
    /// and contention is sticky for the process.
    #[test]
    pub fn r23_w3_two_live_g1_collectors_poison_the_jit_tables() {
        use cratonvm_gc_core::jit_bounds::{
            JIT_G1_BARRIER, JIT_G1_BARRIER_CONTENDED, JIT_G1_BARRIER_OWNER,
            JIT_G1_BARRIER_POISON_MASK, JIT_G1_YOUNG,
        };
        let poisoned = || {
            assert_eq!(
                JIT_G1_BARRIER_OWNER.load(Ordering::SeqCst),
                JIT_G1_BARRIER_CONTENDED
            );
            assert_eq!(JIT_G1_BARRIER.words[1].load(Ordering::SeqCst), 0, "not live");
            assert_eq!(JIT_G1_BARRIER.words[0].load(Ordering::SeqCst), 0);
            assert_eq!(
                JIT_G1_BARRIER.words[2].load(Ordering::SeqCst),
                JIT_G1_BARRIER_POISON_MASK
            );
            assert_eq!(JIT_G1_YOUNG.words[3].load(Ordering::SeqCst), 0, "young shift");
        };
        for a_first in [true, false] {
            let a = make_collector();
            let b = make_collector();
            poisoned();
            if a_first {
                drop(a);
                poisoned();
                drop(b);
            } else {
                drop(b);
                poisoned();
                drop(a);
            }
            poisoned();
        }
    }

    /// JIT round 23 wave 3, lane L4: with the young-source filter on, a store
    /// whose holder is young records nothing (HotSpot's young-card filter);
    /// the prerequisite's pause walk of an EXCLUDED young region then records
    /// the edge and dirties the holder's card, which is what the pinned-holder
    /// regression (`jni_pinned_young_region_holder_keeps_cset_referent_alive`)
    /// relies on; a region in the set is not walked.
    #[test]
    pub fn r23_w3_young_holder_store_is_filtered_and_rewalked_when_excluded() {
        let gc = make_collector();
        if !gc.young_source_filter_on() {
            return;
        }
        let p = gc.alloc_object(ClassId::new(1), 1);
        let (p_region, mut q, mut q_region) = {
            let regions = gc.regions.read();
            let pr = gc.region_for_ptr(&regions, p.as_ptr()).unwrap();
            (pr, p, pr)
        };
        while q_region == p_region {
            q = gc.alloc_object(ClassId::new(2), 1);
            let regions = gc.regions.read();
            q_region = gc.region_for_ptr(&regions, q.as_ptr()).unwrap();
        }
        assert!(gc.region_is_young_unlocked(p_region), "a fresh object is in Eden");
        assert!(gc.young_holder_skips_post_barrier(p), "a compiled store from it skips");
        // The store as compiled code makes it under the filter: the slot
        // written, no edge and no card (emulated by clearing what the
        // accessor's own barrier recorded).
        gc.set_field(p, 0, Value::Object(Some(q)));
        gc.with_regions_mut(|regions| regions[q_region].rset.clear());
        gc.cards.clear_range(p.as_ptr() as usize, 8);
        {
            let regions = gc.regions.read();
            assert!(!regions[q_region].rset.names_source(p_region));
        }
        // In the set: not walked.
        {
            let mut regions = gc.regions.write();
            let n = gc.record_excluded_young_sources(&mut regions, &|_| true);
            assert_eq!(n, 0);
            assert!(!regions[q_region].rset.names_source(p_region));
        }
        // Excluded (pinned): walked, the edge and the card recorded.
        {
            let mut regions = gc.regions.write();
            let n = gc.record_excluded_young_sources(&mut regions, &|i| i != p_region);
            assert_eq!(n, 1, "only the excluded region");
            assert!(regions[q_region].rset.names_source(p_region));
        }
        assert!(gc.cards.is_dirty_addr(p.as_ptr() as usize));
    }

    /// JIT round 23 wave 2d, lane L4: the post barrier's last-edge memo key
    /// matches only itself; any epoch, holder, region or collector change
    /// misses.
    #[test]
    pub fn r23_w2d_last_edge_memo_hits_only_the_exact_key() {
        let k: G1LastEdgeKey = (7, 0x1000, 3, 10, 20);
        assert!(g1_last_edge_memo_hit(Some(k), k));
        assert!(!g1_last_edge_memo_hit(None, k));
        for other in [
            (8, 0x1000, 3, 10, 20),
            (7, 0x1008, 3, 10, 20),
            (7, 0x1000, 4, 10, 20),
            (7, 0x1000, 3, 11, 20),
            (7, 0x1000, 3, 10, 21),
        ] {
            assert!(!g1_last_edge_memo_hit(Some(other), k), "{other:?}");
        }
    }

    /// JIT round 23 wave 2d, lane L4: a repeated cross-region store is served
    /// by the last-edge memo, but a card cleaner in between (here the region
    /// reset's `clear_range`) moves `clean_epoch`, so the next store dirties
    /// the holder's card again and the edge stays in the destination's rset.
    #[test]
    pub fn r23_w2d_last_edge_memo_is_invalidated_by_card_cleaning() {
        let gc = make_collector();
        let p = gc.alloc_object(ClassId::new(1), 1);
        let p_region = {
            let regions = gc.regions.read();
            gc.region_for_ptr(&regions, p.as_ptr()).unwrap()
        };
        let mut q = gc.alloc_object(ClassId::new(2), 1);
        loop {
            let q_region = {
                let regions = gc.regions.read();
                gc.region_for_ptr(&regions, q.as_ptr()).unwrap()
            };
            if q_region != p_region {
                break;
            }
            q = gc.alloc_object(ClassId::new(2), 1);
        }
        let p_addr = p.as_ptr() as usize;
        gc.post_write_barrier_rset(p, q);
        assert!(gc.cards.is_dirty_addr(p_addr), "first store dirties the card");
        let before = gc.cards.clean_epoch();
        gc.post_write_barrier_rset(p, q); // memo hit: nothing to redo
        assert!(gc.cards.is_dirty_addr(p_addr));
        // A cleaner runs (as a pause would): the card is clean again.
        gc.cards.clear_range(p_addr, 8);
        assert!(gc.cards.clean_epoch() > before, "the cleaner moved the epoch");
        assert!(!gc.cards.is_dirty_addr(p_addr));
        gc.post_write_barrier_rset(p, q);
        assert!(
            gc.cards.is_dirty_addr(p_addr),
            "a memo recorded before the clean must not skip the re-dirty"
        );
    }

    /// Parallel-evacuator twin of
    /// [`jni_pinned_young_region_holder_keeps_cset_referent_alive`].
    #[test]
    pub fn jni_pinned_young_region_holder_keeps_cset_referent_alive_parallel() {
        let gc = G1Collector::new(parallel_config(2, 8));
        let p = gc.alloc_object(ClassId::new(1), 1);
        let p_region = {
            let regions = gc.regions.read();
            gc.region_for_ptr(&regions, p.as_ptr()).unwrap()
        };
        let mut q = gc.alloc_object(ClassId::new(2), 1);
        loop {
            let q_region = {
                let regions = gc.regions.read();
                gc.region_for_ptr(&regions, q.as_ptr()).unwrap()
            };
            if q_region != p_region {
                break;
            }
            q = gc.alloc_object(ClassId::new(2), 1);
        }
        gc.set_field(q, 0, Value::Int(2424));
        gc.set_field(p, 0, Value::Object(Some(q)));
        gc.pin_region(p_region);

        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);

        let q_new = result
            .pointer_map
            .get(&(q.as_ptr() as usize))
            .copied()
            .expect("Q, referenced only from the pinned region, must be evacuated (parallel)");
        let q_new_ref = unsafe { ObjectRef::from_raw(q_new as *mut u8) };
        assert_eq!(gc.get_field(p, 0), Value::Object(Some(q_new_ref)));
        assert_eq!(gc.get_field(q_new_ref, 0).as_int(), Some(2424));
        gc.unpin_region(p_region);
    }

    #[test]
    pub fn region_pin_refcount_balances() {
        // Overlapping critical sections on arrays in the same region (or nested
        // checkouts of one array) must refcount: one Release cannot unpin while
        // another section is still live.
        let gc = make_collector();
        gc.pin_region(0);
        gc.pin_region(0);
        assert!(gc.is_pinned(0));
        gc.unpin_region(0);
        assert!(gc.is_pinned(0), "still pinned after 1 of 2 unpins");
        gc.unpin_region(0);
        assert!(!gc.is_pinned(0), "unpinned after the final unpin");
        // An extra (unbalanced) unpin is tolerated and stays unpinned.
        gc.unpin_region(0);
        assert!(!gc.is_pinned(0));
    }

    // -- String deduplication --

    #[test]
    pub fn string_dedup_disabled_by_default() {
        let gc = make_collector();
        assert_eq!(gc.deduplicate_string(12345, 0x1000), None);
    }

    #[test]
    pub fn string_dedup_enabled() {
        let mut cfg = small_config();
        cfg.string_dedup_enabled = true;
        let gc = G1Collector::new(cfg);

        // First string with this hash: not deduped, just registered.
        assert_eq!(gc.deduplicate_string(42, 0x1000), None);
        // Second string with same hash: deduped — caller gets the canonical
        // address so it can redirect its pointer.
        assert_eq!(gc.deduplicate_string(42, 0x2000), Some(0x1000));
        // Different hash: not deduped, registered fresh.
        assert_eq!(gc.deduplicate_string(99, 0x3000), None);
        // Re-hit the second hash with yet another address — still points
        // at the original canonical instance.
        assert_eq!(gc.deduplicate_string(99, 0x4000), Some(0x3000));
    }

    // -- GC logging --

    #[test]
    pub fn gc_logging_toggle() {
        let gc = make_collector();
        assert!(!gc.gc_log_enabled.load(Ordering::Relaxed));
        gc.enable_gc_logging();
        assert!(gc.gc_log_enabled.load(Ordering::Relaxed));
        gc.disable_gc_logging();
        assert!(!gc.gc_log_enabled.load(Ordering::Relaxed));
    }

    // -- Collection counting --

    #[test]
    pub fn collection_count_increments() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![obj];

        assert_eq!(gc.collection_count(), 0);
        gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.collection_count(), 1);
        gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.collection_count(), 2);
    }

    // -- Pause sink (§7 item 6) --

    #[test]
    pub fn pause_history_records_each_collection() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 0);
        let mut roots = vec![obj];

        assert!(gc.pause_summary().is_none(), "no history before any GC");

        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);

        let hist = gc.pause_history_snapshot();
        assert_eq!(hist.len(), 2, "one record per collection");
        assert!(hist
            .iter()
            .all(|r| r.collection_type == G1CollectionType::YoungOnly));

        let summary = gc.pause_summary().expect("history present");
        assert_eq!(summary.young.count, 2);
        assert_eq!(summary.mixed.count, 0);
        // Microsecond accumulator agrees with the per-record total.
        let recorded: u64 = hist.iter().map(|r| r.pause_us).sum();
        assert_eq!(gc.total_pause_us(), recorded);
    }

    #[test]
    pub fn pause_percentiles_nearest_rank() {
        // Drive the reduction directly with a known distribution so the
        // percentile math is exercised independently of wall-clock timing.
        let gc = make_collector();
        let stats = GcStats {
            objects_copied: 0,
            bytes_copied: 0,
            bytes_freed: 0,
        };
        // pauses 10,20,...,100 us (10 young collections).
        for i in 1..=10u64 {
            gc.record_collection(G1CollectionType::YoungOnly, i * 10, &stats);
        }
        let s = gc.pause_summary().unwrap();
        assert_eq!(s.young.count, 10);
        assert_eq!(s.young.max_us, 100);
        // nearest-rank: p50 -> ceil(0.5*10)=5th -> 50; p99 -> ceil(0.99*10)=10th -> 100.
        assert_eq!(s.young.p50_us, 50);
        assert_eq!(s.young.p99_us, 100);
        assert_eq!(s.young.total_us, 550);
    }

    #[test]
    pub fn pause_history_ring_is_bounded() {
        let gc = make_collector();
        let stats = GcStats {
            objects_copied: 0,
            bytes_copied: 0,
            bytes_freed: 0,
        };
        for _ in 0..(PAUSE_HISTORY_CAP + 100) {
            gc.record_collection(G1CollectionType::YoungOnly, 1, &stats);
        }
        assert_eq!(gc.pause_history_snapshot().len(), PAUSE_HISTORY_CAP);
        assert_eq!(
            gc.pause_history_dropped.load(Ordering::Relaxed),
            100,
            "evictions are counted so the summary stays honest"
        );
        // collection_count keeps the true total, not the ring size.
        assert_eq!(gc.collection_count(), (PAUSE_HISTORY_CAP + 100) as u64);
    }

    // -- Concurrent marking --

    #[test]
    pub fn concurrent_mark_phases() {
        let gc = make_collector();
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::Idle);

        gc.start_concurrent_mark(&stw());
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::ConcurrentMark);

        let done = gc.concurrent_mark_step(1000);
        assert!(done); // no objects to mark, should complete immediately

        let obj = gc.alloc_object(ClassId::new(1), 0);
        gc.remark(&stw(), &[obj]);
        // G1AUD-9: `remark` is an instant, not a resting phase. It is called
        // from the INITIAL-mark pause to seed the gray set, and leaving the
        // phase at `Remark` afterwards made `gc_phase()` misreport the whole
        // concurrent-mark window. It hands the marking phase back on the way
        // out; `cleanup`/`abort_concurrent_mark` are what end the cycle.
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::ConcurrentMark);
        gc.cleanup(&stw());
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::Idle);
    }

    /// G1AUD-9 regression — a `remark` that is NOT inside a mark cycle must
    /// not invent one. The failure this guards is the mirror of the phase bug
    /// it accompanies: restoring `resume_phase` unconditionally would leave a
    /// stray `Remark` (or worse, resurrect a phase) on the abort/teardown path
    /// where the cycle is already over.
    #[test]
    pub fn remark_outside_a_cycle_leaves_the_phase_idle() {
        let gc = make_collector();
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::Idle);
        let obj = gc.alloc_object(ClassId::new(1), 0);
        gc.remark(&stw(), &[obj]);
        assert_eq!(
            gc.gc_phase(),
            ConcurrentGcPhase::Remark,
            "no cycle was open, so there is no marking phase to hand back to"
        );
        gc.abort_concurrent_mark();
        assert_eq!(gc.gc_phase(), ConcurrentGcPhase::Idle);
    }

    #[test]
    pub fn cleanup_computes_live_bytes() {
        // Round-2 fix (HIGH — GC #5): bitmaps now live per-region and
        // are keyed off each region's heap-allocated `data.as_ptr()`,
        // so `try_mark`/`is_marked` accept the real addresses of
        // objects living inside the region. This means `cleanup` can
        // now correctly attribute live bytes to a marked object.
        //
        // The previous version of this test documented the BUG —
        // marking a real address against a `[0, heap_size)` bitmap
        // always silently failed, so `live_bytes` was always 0. After
        // the per-region-bitmap fix, marking succeeds and `cleanup`
        // reports a non-zero `live_bytes` for the region holding the
        // marked object.
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 2);
        let obj_addr = obj.as_ptr() as usize;

        // Locate the region the allocator placed the object in, mark
        // the object in *that* region's bitmap, then promote every
        // Eden region (including ours) to Old so `cleanup` walks them.
        {
            let mut regions = gc.regions.write();
            let region_idx = gc
                .region_for_ptr(&regions, obj.as_ptr())
                .expect("freshly-allocated object must live in some region");
            // F-06: through the accumulator, like every production mark site.
            let marked = regions[region_idx].try_mark_and_account(obj_addr);
            assert!(
                marked,
                "per-region bitmap must accept real heap addresses post-fix"
            );
            for r in regions.iter_mut() {
                if r.region_type() == RegionType::Eden {
                    r.set_region_type(RegionType::Old);
                }
            }
        }

        gc.cleanup(&stw());

        // The region containing our marked object must report non-zero
        // live_bytes (was always 0 under the buggy global bitmap). The
        // exact byte count equals one ObjectHeader + 2 SLOT_SIZE
        // fields = HEADER_SIZE + 2*SLOT_SIZE. We assert >0 to keep the
        // test resilient to header-size tuning.
        let regions = gc.regions.read();
        let holding_region = regions
            .iter()
            .find(|r| {
                let base = r.data.as_ptr() as usize;
                obj_addr >= base && obj_addr < base + r.data.len()
            })
            .expect("holding region must still exist");
        assert!(
            holding_region.live_bytes > 0,
            "per-region bitmap fix: cleanup must report >0 live_bytes for the marked region (got {})",
            holding_region.live_bytes,
        );
        assert_eq!(
            holding_region.region_type(),
            RegionType::Old,
            "non-empty old region must not be freed by cleanup"
        );

        // Other old regions had no marked objects, so they should have
        // been freed (live_bytes == 0, type went back to Free).
        let old_count = regions
            .iter()
            .filter(|r| r.region_type() == RegionType::Old)
            .count();
        assert_eq!(
            old_count, 1,
            "only the region containing the marked object should remain Old; \
             empty old regions must be freed"
        );
    }

    // =====================================================================
    // Marking soundness across evacuation pauses (SATB keep-alive) and the
    // restored cleanup in-place free — regression tests for the SteadyChurn
    // "[FREED] cleanup region=N while live holders point in" defect stack.
    // =====================================================================

    /// A gray worklist entry whose object the evacuation closure does not
    /// reach (dead at PAUSE time, live at MARK START) must be evacuated and
    /// re-grayed, not dropped: under SATB its unscanned subtree is
    /// snapshot-live. Pre-fix, the young pause dropped the gray and the
    /// subtree was silently unmarked.
    #[test]
    pub fn gray_cset_entry_is_evacuated_and_subtree_marked_across_young_pause() {
        let gc = make_collector();
        // G (young) → H (young); at the pause below NEITHER is reachable
        // from any root — the evacuation closure alone judges both dead.
        let g = gc.alloc_object(ClassId::new(1), 1);
        let h = gc.alloc_object(ClassId::new(2), 0);
        gc.set_field(g, 0, Value::Object(Some(h)));

        gc.start_concurrent_mark(&stw());
        // Seed G gray (as the initial-mark root scan would); do NOT drain —
        // the young pause interrupts marking mid-cycle.
        gc.remark(&stw(), &[g]);

        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        let g_new = *result
            .pointer_map
            .get(&(g.as_ptr() as usize))
            .expect("snapshot-live gray must be evacuated by the keep-alive, not dropped");
        let h_new = *result
            .pointer_map
            .get(&(h.as_ptr() as usize))
            .expect("the gray's referent must be evacuated with it");

        // Drain the marker: both copies must end up marked.
        while !gc.concurrent_mark_step(usize::MAX) {}
        let regions = gc.regions.read();
        let g_idx = gc.region_for_ptr(&regions, g_new as *mut u8).unwrap();
        let h_idx = gc.region_for_ptr(&regions, h_new as *mut u8).unwrap();
        assert!(
            regions[g_idx].mark_bitmap.is_marked(g_new),
            "evacuated gray must be re-grayed and marked"
        );
        assert!(
            regions[h_idx].mark_bitmap.is_marked(h_new),
            "the gray's unscanned subtree must reach the bitmap (SATB snapshot)"
        );
    }

    /// SATB-logged references must survive an evacuation pause: the log is
    /// drained into the gray set at pause start and CSet-resident entries
    /// are evacuated. Pre-fix the raw addresses sat in the queue while the
    /// pause reset their regions (dangling), and at runtime the log was
    /// never consumed at all.
    #[test]
    pub fn satb_entry_survives_young_pause_and_gets_marked() {
        let gc = make_collector();
        let x = gc.alloc_object(ClassId::new(7), 0);

        gc.start_concurrent_mark(&stw());
        // A mutator overwrites the last reference to X mid-cycle: the SATB
        // pre-barrier logs X. Flush the thread-local buffer immediately so
        // a concurrent test's registry-wide flush cannot steal the entry
        // into a different collector's queue (the buffer is process-global).
        gc.satb_pre_barrier(x.as_ptr() as usize);
        cratonvm_gc_core::satb::flush_thread_satb_buffer(gc.satb_queue());

        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        let x_new = *result
            .pointer_map
            .get(&(x.as_ptr() as usize))
            .expect("SATB-logged object must be kept alive across the pause");
        while !gc.concurrent_mark_step(usize::MAX) {}
        let regions = gc.regions.read();
        let idx = gc.region_for_ptr(&regions, x_new as *mut u8).unwrap();
        assert!(
            regions[idx].mark_bitmap.is_marked(x_new),
            "SATB entry must reach the bitmap after the pause"
        );
    }

    /// Mixed pauses move OLD regions — where the gray set concentrates —
    /// and must remap the mark worklist exactly like young pauses do.
    /// Pre-fix, neither mixed path touched the worklist at all: every gray
    /// pointing into an evacuated Old region dangled into freed memory.
    #[test]
    pub fn mixed_collection_keeps_and_remaps_gray_old_entry() {
        let cfg = G1CollectorConfig {
            promotion_age: 1,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);
        // Promote OBJ to Old with two young collections.
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(41));
        let mut roots = vec![obj];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        let obj = roots[0];
        {
            let regions = gc.regions.read();
            let idx = gc.region_for_ptr(&regions, obj.as_ptr()).unwrap();
            assert_eq!(
                regions[idx].region_type(),
                RegionType::Old,
                "test setup: object must be promoted"
            );
        }

        // A NEW marking cycle overlaps the post-cleanup mixed sequence
        // (reachable in production: start_concurrent_mark does not clear
        // marking_complete).
        gc.start_concurrent_mark(&stw());
        gc.remark(&stw(), &[obj]); // OBJ gray, unscanned
        gc.marking_complete.store(true, Ordering::Relaxed);
        gc.mixed_gc_remaining.store(1, Ordering::Relaxed);

        let result = gc.mixed_collection(&mut roots, &NoopMonitors);

        // No gray may dangle into a freed region…
        {
            let regions = gc.regions.read();
            let worklist = gc.mark_worklist.lock();
            for &addr in worklist.iter() {
                let idx = gc
                    .region_for_ptr(&regions, addr as *mut u8)
                    .expect("gray entry must point into the heap");
                assert_ne!(
                    regions[idx].region_type(),
                    RegionType::Free,
                    "gray entry {addr:#x} dangles into a freed region"
                );
            }
        }
        // …and OBJ (possibly relocated) must be marked after draining.
        let obj_final = result
            .pointer_map
            .get(&(obj.as_ptr() as usize))
            .copied()
            .unwrap_or(obj.as_ptr() as usize);
        while !gc.concurrent_mark_step(usize::MAX) {}
        let regions = gc.regions.read();
        let idx = gc.region_for_ptr(&regions, obj_final as *mut u8).unwrap();
        assert!(regions[idx].mark_bitmap.is_marked(obj_final));
    }


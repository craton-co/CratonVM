#[allow(unused_imports)]
use super::*;


    /// No-op monitor cleanup for tests.
    pub struct NoopMonitors;
    impl MonitorCleanup for NoopMonitors {
        fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
    }

    /// Test-only `StopTheWorldToken`. Single-threaded test harness, so the
    /// STW invariant is trivially satisfied.
    #[inline]
    pub fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
        // SAFETY: these unit tests run the heap single-threaded.
        unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
    }

    pub fn small_config() -> G1CollectorConfig {
        G1CollectorConfig {
            heap_size: 8 * 1024 * 1024, // 8 MB
            // F-16: the WHOLE heap committed up front, deliberately.
            //
            // Production commits a region as it is claimed, and every path that
            // writes into one commits it first. Dozens of tests in this module
            // bypass those paths entirely — they reach into the region table,
            // set a `region_type` by hand and call `bump_alloc` directly — which
            // is a reasonable way to build a fixture and is exactly what lazy
            // commit makes fault. Those tests are asserting things about
            // evacuation, remembered sets and cards, not about commit state, so
            // the fixture gives them the pre-F-16 world and says so.
            //
            // The tests that ARE about commit state build their own config with
            // an explicit `initial_heap_size`; see
            // `a_large_heap_does_not_commit_itself_at_startup` and its
            // neighbours.
            initial_heap_size: 8 * 1024 * 1024,
            region_size: 1024 * 1024, // 1 MB
            max_gc_pause_ms: 200,
            ihop_percent: 45,
            promotion_age: 3,
            gc_worker_threads: 1,
            string_dedup_enabled: false,
            mixed_gc_count_target: 8,
            old_cset_region_threshold_percent: 10,
            mixed_gc_live_threshold_percent: 85,
            // The fixtures below hand-build Old regions a few hundred bytes
            // long; a waste floor would end their mixed phases before they ran.
            heap_waste_percent: 0,
        }
    }

    pub fn make_collector() -> G1Collector {
        G1Collector::new(small_config())
    }

    /// Is `addr` marked in its region's bitmap?
    pub fn r11w18_is_marked(gc: &G1Collector, addr: usize) -> bool {
        let regions = gc.regions.read();
        let idx = gc
            .region_for_ptr(&regions, addr as *mut u8)
            .expect("fixture address is in a live region");
        regions[idx].mark_bitmap.is_marked(addr)
    }

    /// r11w18 g1store — the arena-pointer screens test the header's REAL
    /// first word, `class_id` + mark word.
    ///
    /// An array whose class id and LENGTH recombine to an arena address -- the
    /// shape of the w17 hang log's TLAB filler `int[442]` in an arena at
    /// `0x1ba_cb12_0000` -- is an object; before the fix the screen refused
    /// it. A heap pointer written over a header's first eight bytes is not an
    /// object; before the fix the screen paired its low half with the shape
    /// word and missed it.
    #[test]
    pub fn r11w18_arena_word0_screen_pairs_the_class_id_with_the_mark_word() {
        let gc = make_collector();
        let base = gc.arena_base as u64;
        let header = ObjectHeader::new(
            ClassId::new(base as u32),
            ObjectKind::Array,
            ArrayElementType::Int,
            (base >> 32) as u32,
            0,
        );
        assert_eq!(
            (header.class_id.as_u32() as u64) | ((header.num_slots() as u64) << 32),
            base,
            "fixture: the old class-id + shape pairing names the arena base"
        );
        assert_eq!(
            gc.holder_word0_arena_pointer(&header),
            None,
            "a real array header must never read as an arena pointer"
        );

        let words: [u64; 2] = [base + 64, 0];
        // SAFETY: sixteen readable, 8-aligned bytes; only header words are read.
        let smashed = unsafe { &*(words.as_ptr() as *const ObjectHeader) };
        assert_eq!(
            gc.holder_word0_arena_pointer(smashed),
            Some(base + 64),
            "a heap pointer over the first eight bytes must be refused"
        );
    }

    /// r11w18 g1store — a young pause EVACUATES an array whose class id and
    /// length pair into the arena. Before the fix the candidate screen refused
    /// it: the array stayed in the collected Eden region and the holder's slot
    /// kept naming from-space the pause freed.
    #[test]
    pub fn r11w18_an_array_whose_class_id_and_length_pair_into_the_arena_is_evacuated() {
        let gc = make_collector();
        let base = gc.arena_base as u64;
        let len = (base >> 32) as usize;
        let arr = gc.alloc_array(ClassId::new(base as u32), ArrayElementType::Reference, len);
        let holder = gc.alloc_object(ClassId::new(0x00AB_CDE5), 1);
        gc.set_field(holder, 0, Value::Object(Some(arr)));
        let old_arr = arr.as_ptr() as usize;

        let mut roots = vec![holder];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        let moved = match gc.get_field(roots[0], 0) {
            Value::Object(Some(a)) => a.as_ptr() as usize,
            other => panic!("the holder's slot lost its array: {other:?}"),
        };
        assert_ne!(
            moved, old_arr,
            "the array must be evacuated, not left in the collected region"
        );
        assert_eq!(result.pointer_map.get(&old_arr).copied(), Some(moved));
        // SAFETY: `moved` is the evacuated copy the holder now names.
        let moved_ref = unsafe { ObjectRef::from_raw(moved as *mut u8) };
        assert_eq!(gc.array_length(moved_ref), len);
    }

    /// r11w18 g1store — a mark root that is the PAYLOAD word of a reference
    /// cell (a heap pointer: the shape of `obj + 8` of a compact instance in a
    /// compiled frame) seeds the object that contains it.
    ///
    /// Before the root screen `remark` pushed it raw: the marker read the
    /// pointer's halves as a flag-less legacy header, `num_slots` from the next
    /// cell, "scanned" that, and marked neither the holder nor its child.
    #[test]
    pub fn r11w18_a_pointer_word_mark_root_seeds_its_containing_object() {
        let gc = make_collector();
        let child = gc.alloc_object(ClassId::new(0x00AB_CDE2), 0);
        let holder = gc.alloc_object(ClassId::new(0x00AB_CDE1), 2);
        // SAFETY: `holder` is a freshly allocated object.
        let holder_header = unsafe { &*(holder.as_ptr() as *const ObjectHeader) };
        assert!(
            !cratonvm_types::is_compact_object(holder_header),
            "fixture: the holder must use the legacy cell layout"
        );
        gc.set_field(holder, 0, Value::Object(Some(child)));
        let root_addr =
            holder.as_ptr() as usize + HEADER_SIZE + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET;
        // SAFETY: cell 0's payload word, inside the holder's body.
        let word = unsafe { std::ptr::read(root_addr as *const u64) };
        assert_eq!(
            word,
            child.as_ptr() as u64,
            "fixture: the root word is the child pointer"
        );

        gc.start_concurrent_mark(&stw());
        // SAFETY: an 8-aligned in-heap address; `ObjectRef` is only carried.
        let root = unsafe { ObjectRef::from_raw(root_addr as *mut u8) };
        gc.remark(&stw(), &[root]);
        while !gc.concurrent_mark_step(usize::MAX) {}

        assert!(
            r11w18_is_marked(&gc, holder.as_ptr() as usize),
            "the interior root must seed the object that contains it"
        );
        assert!(
            r11w18_is_marked(&gc, child.as_ptr() as usize),
            "...and the holder must be SCANNED, so its child is marked"
        );
        assert!(
            !r11w18_is_marked(&gc, root_addr),
            "the interior word itself must never become a mark"
        );
        assert!(
            !gc.mark_saw_implausible.load(Ordering::Relaxed),
            "a placed interior root must not impugn the cycle"
        );
    }

    /// r11w18 g1store — a mark root at or inside a TLAB filler seeds nothing:
    /// the filler is dead space, not an object. A real object root beside it
    /// is still marked.
    #[test]
    pub fn r11w18_a_mark_root_in_a_tlab_filler_seeds_nothing() {
        let gc = make_collector();
        let real = gc.alloc_object(ClassId::new(0x00AB_CDE3), 1);
        let filler = gc.alloc_array(cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID, ArrayElementType::Int, 32);
        let filler_addr = filler.as_ptr() as usize;
        let inside_addr = filler_addr + 40;

        gc.start_concurrent_mark(&stw());
        // SAFETY: an 8-aligned in-heap address; `ObjectRef` is only carried.
        let inside = unsafe { ObjectRef::from_raw(inside_addr as *mut u8) };
        gc.remark(&stw(), &[real, filler, inside]);
        while !gc.concurrent_mark_step(usize::MAX) {}

        assert!(r11w18_is_marked(&gc, real.as_ptr() as usize));
        assert!(
            !r11w18_is_marked(&gc, filler_addr),
            "a filler is never seeded"
        );
        assert!(!r11w18_is_marked(&gc, inside_addr));
        assert!(!gc.mark_saw_implausible.load(Ordering::Relaxed));
    }

    /// r11w18 g1store — a mark root inside a humongous CONTINUATION region
    /// seeds the humongous object, which begins at its start region's base.
    /// Before the screen the marker refused the address (`NotAllocated`).
    #[test]
    pub fn r11w18_a_humongous_continuation_mark_root_seeds_the_span_object() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        assert_eq!(gc.count_regions(RegionType::HumongousContinuation), 1);
        // 1.25 MiB into a 1.6 MB object with 1 MiB regions: the continuation.
        let root_addr = arr.as_ptr() as usize + (5 << 18);
        {
            let regions = gc.regions.read();
            let idx = gc.region_for_ptr(&regions, root_addr as *mut u8).unwrap();
            assert_eq!(
                regions[idx].region_type(),
                RegionType::HumongousContinuation
            );
        }

        gc.start_concurrent_mark(&stw());
        // SAFETY: an 8-aligned in-heap address; `ObjectRef` is only carried.
        let root = unsafe { ObjectRef::from_raw(root_addr as *mut u8) };
        gc.remark(&stw(), &[root]);
        while !gc.concurrent_mark_step(usize::MAX) {}

        assert!(
            r11w18_is_marked(&gc, arr.as_ptr() as usize),
            "a continuation root must seed the humongous object at its span's base"
        );
        assert!(!gc.mark_saw_implausible.load(Ordering::Relaxed));
    }

    /// r11w18 g1store — the concurrent overflow rescan reaches a marked Eden
    /// object that sits AFTER bytes the object grid cannot stride (here an
    /// array length no region can hold, the shape of a header being written
    /// by a mutator's TLAB bump). Its only child lives in an Old region.
    /// Before the fix the rescan's grid walk broke at the blocker and the child
    /// stayed white.
    #[test]
    pub fn r11w18_the_overflow_rescan_reaches_marked_eden_objects_past_an_unwalkable_header() {
        let gc = make_collector();
        let blocker = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 4);
        let holder = gc.alloc_object(ClassId::new(0x00AB_CDE4), 1);
        let child = {
            let mut regions = gc.regions.write();
            let idx = regions
                .iter()
                .position(|r| r.region_type() == RegionType::Free)
                .expect("a free region");
            regions[idx].set_region_type(RegionType::Old);
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
            // SAFETY: a fresh, zeroed span of the header's size.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
                ObjectRef::from_raw(ptr)
            }
        };
        gc.with_regions_mut(|_| {}); // republish the counters after the hand edit
        gc.set_field(holder, 0, Value::Object(Some(child)));
        let holder_idx = {
            let regions = gc.regions.read();
            let bi = gc.region_for_ptr(&regions, blocker.as_ptr()).unwrap();
            let hi = gc.region_for_ptr(&regions, holder.as_ptr()).unwrap();
            assert_eq!(bi, hi, "fixture: blocker and holder share a region");
            assert_eq!(regions[hi].region_type(), RegionType::Eden);
            assert!((blocker.as_ptr() as usize) < (holder.as_ptr() as usize));
            hi
        };

        gc.start_concurrent_mark(&stw());
        // The holder is black-without-scan, as the cap arms leave a seed, and
        // the overflow is recorded.
        {
            let regions = gc.regions.read();
            assert!(regions[holder_idx].try_mark_and_account(holder.as_ptr() as usize));
        }
        gc.mark_worklist_overflowed.store(true, Ordering::Relaxed);
        // SAFETY: the blocker's length word, inside its header.
        unsafe {
            std::ptr::write(
                (blocker.as_ptr() as usize + cratonvm_types::ARRAY_LENGTH_OFFSET) as *mut u32,
                0x7fff_ffff,
            );
        }
        while !gc.concurrent_mark_step(usize::MAX) {}

        assert!(
            r11w18_is_marked(&gc, child.as_ptr() as usize),
            "the rescan must scan the marked holder past the unwalkable header"
        );
    }

    /// r11w19 g1store (proposal W18-1) — the marker's gray gate refuses a
    /// header without `GC_FLAG_HEADER` as a TORN header.
    ///
    /// The word is a legacy reference cell's payload, a heap pointer: the shape
    /// of `obj + 8` of a compact instance. Its halves read as a class id and a
    /// flag-less NEUTRAL legacy mark, which the size decode accepts, so before
    /// the flag test the gate passed it and the marker walked the "cells" it
    /// declared. Real objects on either side still pass.
    #[test]
    pub fn r11w19_the_gray_gate_refuses_a_flagless_header_as_torn() {
        let gc = make_collector();
        let child = gc.alloc_object(ClassId::new(0x00AB_CDE6), 0);
        let holder = gc.alloc_object(ClassId::new(0x00AB_CDE7), 2);
        // Keeps the region's cursor well past the holder, so no refusal below
        // can come from the cursor bound.
        let _tail = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 64);
        gc.set_field(holder, 0, Value::Object(Some(child)));
        let word_addr =
            holder.as_ptr() as usize + HEADER_SIZE + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET;
        // SAFETY: cell 0's payload word, inside the holder's body.
        let word = unsafe { std::ptr::read(word_addr as *const u64) };
        assert_eq!(
            word,
            child.as_ptr() as u64,
            "fixture: the word is the child pointer"
        );

        let regions = gc.regions.read();
        let gate = |addr: usize| {
            let idx = gc
                .region_for_ptr(&regions, addr as *mut u8)
                .expect("fixture address is in a live region");
            classify_mark_scan_target(&regions[idx], addr, &gc.mark_gray_without_header_flag)
        };
        assert_eq!(
            gate(holder.as_ptr() as usize),
            Ok(()),
            "a real holder passes"
        );
        assert_eq!(gate(child.as_ptr() as usize), Ok(()), "a real child passes");
        let flagless_before = gc.mark_gray_without_header_flag.load(Ordering::Relaxed);
        assert_eq!(
            gate(word_addr),
            Err(GrayRefusal::TornHeader),
            "a flag-less header inside the allocated prefix must impugn, never be walked"
        );
        // r12w1 (W19-2): counted on THIS collector, not a process static.
        assert_eq!(
            gc.mark_gray_without_header_flag.load(Ordering::Relaxed),
            flagless_before + 1
        );
    }

    /// r11w19 g1store (proposal W18-3) — a seed that reaches a gray-set cap arm
    /// passes the gray gate before it is marked black.
    ///
    /// With the gray set full, `push_gray_or_mark` marks instead of pushing.
    /// Before the screen it marked the interior word itself, a mark bit on
    /// bytes that are not an object; now that word is refused and impugns the
    /// cycle, exactly as its gray pop would have, while a real seed at the cap
    /// is still marked black.
    #[test]
    pub fn r11w19_a_cap_arm_seed_passes_the_gray_gate_before_it_is_marked() {
        let gc = make_collector();
        let child = gc.alloc_object(ClassId::new(0x00AB_CDE8), 0);
        let holder = gc.alloc_object(ClassId::new(0x00AB_CDE9), 2);
        let saturator = gc.alloc_object(ClassId::new(2), 0);
        let _tail = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 64);
        gc.set_field(holder, 0, Value::Object(Some(child)));
        let word_addr =
            holder.as_ptr() as usize + HEADER_SIZE + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET;

        gc.start_concurrent_mark(&stw());
        gc.mark_worklist
            .lock()
            .extend(std::iter::repeat(saturator.as_ptr() as usize).take(MARK_WORKLIST_CAP));
        {
            let regions = gc.regions.read();
            gc.push_gray_or_mark(&regions, word_addr);
            gc.push_gray_or_mark(&regions, child.as_ptr() as usize);
        }

        assert!(
            !r11w18_is_marked(&gc, word_addr),
            "an interior word at the cap must not become a mark bit"
        );
        assert!(
            r11w18_is_marked(&gc, child.as_ptr() as usize),
            "a real seed at the cap is still marked black"
        );
        assert!(
            gc.mark_saw_implausible.load(Ordering::Relaxed),
            "the refused in-prefix seed impugns the cycle, as its gray pop would have"
        );
        assert!(gc.mark_worklist_overflowed.load(Ordering::Relaxed));
        // Finish the cycle normally: the rescan the overflow forces re-screens
        // every mark bit, and cleanup takes the fail-safe the refusal asked for.
        gc.mark_worklist.lock().clear();
        while !gc.concurrent_mark_step(usize::MAX) {}
        gc.cleanup(&stw());
        assert!(
            !gc.mark_saw_implausible.load(Ordering::Relaxed),
            "the fail-safe flag is per-cycle"
        );
    }

    /// The header initializer runs under the regions guard, over a span that is
    /// already zeroed, and before the allocator hands the pointer back.
    ///
    /// It does NOT assert "before the allocation is published", and must not:
    /// under F-11 the compare-exchange that claims the range IS the
    /// publication, so nothing can run before it. What this guards is the half
    /// that is still true and still worth having -- the four header-writing
    /// callers cannot individually forget, and the span carries a decodable
    /// header the moment `alloc_in_region` returns rather than after the guard
    /// is dropped. See `G1Region::bump_alloc_initialized`.
    #[test]
    pub fn the_allocation_initializer_runs_under_the_guard_over_a_zeroed_span() {
        let gc = make_collector();
        let ran = std::cell::Cell::new(false);
        let got = gc.alloc_in_region_initialized(64, |p| {
            assert!(
                gc.regions.try_write().is_none(),
                "the regions guard must still be held while the header is written"
            );
            // SAFETY: `p` is the base of the 64-byte span this allocation just
            // claimed; the initializer owns it exclusively.
            let bytes = unsafe { std::slice::from_raw_parts(p, 64) };
            assert!(
                bytes.iter().all(|b| *b == 0),
                "the span must already be zeroed when the initializer runs"
            );
            ran.set(true);
        });
        assert!(got.is_some(), "the allocation must succeed on a fresh heap");
        assert!(ran.get(), "the initializer must have run");
    }

    /// ...and the initializer is actually wired to the header write: an array
    /// allocated out of line reads back through the same decoders the region
    /// walk uses. `num_slots` mirroring `array_length` is the property that
    /// makes a torn header indistinguishable from a legacy object, so both are
    /// asserted.
    #[test]
    pub fn out_of_line_array_allocation_publishes_a_decodable_array_header() {
        let gc = make_collector();
        // Larger than `TLAB_MAX_ALLOC` (32 KiB) in the real VM: this entry
        // point is the one every big array takes.
        let arr = gc
            .try_alloc_array(ClassId::new(0), ArrayElementType::Int, 8192)
            .expect("fresh heap must serve one 32 KiB array");
        // SAFETY: `try_alloc_array` returned a live object reference.
        let header = unsafe { &*(arr.as_ptr() as *const ObjectHeader) };
        assert_eq!(header.kind(), ObjectKind::Array);
        assert_eq!(header.element_type(), ArrayElementType::Int);
        assert_eq!(header.array_length(), 8192);
        assert_eq!(header.num_slots(), 8192);
        let regions = gc.regions.read();
        assert!(
            gc.candidate_header_is_plausible(&regions, arr.as_ptr() as usize),
            "the published array must satisfy the same screen the evacuator applies"
        );
    }

    /// Ten-findings item 5. The region table is walked linearly by every
    /// pause, the census, and the free-region searches; an entry that carries
    /// kilobytes of inline diagnostic ring turns each of those into a
    /// multi-megabyte stream. The bound is generous on purpose — it is here to
    /// catch the next inline array, not to pin a layout.
    #[test]
    pub fn a_region_table_entry_stays_small() {
        let size = std::mem::size_of::<G1Region>();
        assert!(
            size <= 512,
            "G1Region is {size} bytes; a linear pass over the region table streams \
             this much per region, so keep diagnostics out of line"
        );
    }

    /// Item 6: the Free/young counters are maintained at the claim funnel and
    /// agree with a fresh census, so `refill_tlab` no longer has to rescan the
    /// table to learn how many regions are left.
    #[test]
    pub fn the_free_region_count_tracks_claims_without_a_rescan() {
        let gc = make_collector();
        let total = gc.num_regions();
        assert_eq!(gc.free_region_count.load(Ordering::Relaxed), total);
        assert_eq!(gc.young_region_count(), 0);

        // One Eden region claimed by a TLAB refill.
        gc.refill_tlab(4096).expect("a fresh heap refills a TLAB");
        assert_eq!(gc.free_region_count.load(Ordering::Relaxed), total - 1);
        assert_eq!(gc.young_region_count(), 1);

        // A humongous span of two regions.
        let _h = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 150_000);
        assert_eq!(gc.free_region_count.load(Ordering::Relaxed), total - 3);
        assert_eq!(gc.young_region_count(), 1);

        // The census, re-derived from the table, says the same thing.
        let (free_before, young_before) = (
            gc.free_region_count.load(Ordering::Relaxed),
            gc.young_region_count(),
        );
        gc.with_regions_mut(|_| {});
        assert_eq!(gc.free_region_count.load(Ordering::Relaxed), free_before);
        assert_eq!(gc.young_region_count(), young_before);
    }

    /// Item 8: young regions are claimed from the top of the committed prefix
    /// and humongous spans from the bottom, so the two populations do not
    /// interleave and a humongous run is not broken up by Eden claims.
    #[test]
    pub fn young_regions_claim_from_the_top_and_humongous_from_the_bottom() {
        let gc = make_collector();
        let total = gc.num_regions();
        gc.refill_tlab(4096).expect("a fresh heap refills a TLAB");
        let mut eden_idx = usize::MAX;
        gc.with_regions_mut(|regions| {
            eden_idx = regions
                .iter()
                .position(|r| r.region_type() == RegionType::Eden)
                .expect("one Eden region");
        });
        assert_eq!(eden_idx, total - 1, "Eden is the highest committed region");

        let h = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 150_000);
        let h_idx = gc
            .lookup_region_for_addr(h.as_ptr() as usize)
            .expect("the span lives in the arena");
        assert_eq!(h_idx, 0, "the humongous span starts at the bottom");

        // A second Eden claim lands directly below the first, not in the
        // hole the humongous span would have needed.
        gc.with_regions_mut(|regions| regions[eden_idx].set_cursor(regions[eden_idx].data.len()));
        gc.refill_tlab(4096).expect("a second TLAB");
        let mut edens: Vec<usize> = Vec::new();
        gc.with_regions_mut(|regions| {
            edens = regions
                .iter()
                .enumerate()
                .filter(|(_, r)| r.region_type() == RegionType::Eden)
                .map(|(i, _)| i)
                .collect();
        });
        assert_eq!(edens, vec![total - 2, total - 1]);
    }

    /// Item 8, the lazy-commit half: a young claim takes a Free region INSIDE
    /// the committed prefix while one exists, and grows the prefix by exactly
    /// one region only when none does.
    #[test]
    pub fn young_claims_do_not_grow_the_committed_prefix_while_it_has_room() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 64 * 1024 * 1024,
            initial_heap_size: 4 * 1024 * 1024,
            ..small_config()
        });
        if !gc.heap_is_reserved() {
            return; // the fallback commits everything; nothing to observe
        }
        let region = 1024 * 1024;
        assert_eq!(gc.committed_bytes(), 4 * region);
        for expected in (0..4).rev() {
            let (ptr, _) = gc.refill_tlab(4096).expect("a TLAB");
            let idx = gc.lookup_region_for_addr(ptr as usize).expect("in arena");
            assert_eq!(idx, expected, "claims walk down through the prefix");
            assert_eq!(
                gc.committed_bytes(),
                4 * region,
                "no growth while the prefix has room"
            );
            gc.with_regions_mut(|regions| regions[idx].set_cursor(regions[idx].data.len()));
        }
        let (ptr, _) = gc.refill_tlab(4096).expect("a TLAB from a grown prefix");
        let idx = gc.lookup_region_for_addr(ptr as usize).expect("in arena");
        assert_eq!(
            idx, 4,
            "the prefix grew by one region and the claim took it"
        );
        assert_eq!(gc.committed_bytes(), 5 * region);
    }

    /// Item 7: a humongous span reused after its previous occupant died is
    /// zeroed before it is handed out, on the path that now zeroes under the
    /// shared guard.
    #[test]
    pub fn a_reused_humongous_span_is_zeroed_before_it_is_handed_out() {
        let gc = make_collector();
        let first = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 150_000);
        for i in [0usize, 77_777, 149_999] {
            gc.set_array_element(first, i, Value::Long(0x5EED_5EED))
                .unwrap();
        }
        let first_addr = first.as_ptr() as usize;
        let _churn = gc.alloc_object(ClassId::new(1), 1);
        let mut roots: Vec<ObjectRef> = vec![];
        gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(gc.count_regions(RegionType::HumongousStart), 0);

        let second = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 150_000);
        assert_eq!(second.as_ptr() as usize, first_addr, "the span was reused");
        for i in [0usize, 77_777, 149_999] {
            assert_eq!(gc.get_array_element(second, i).unwrap(), Value::Long(0));
        }
    }

    #[test]
    pub fn refill_tlab_zeroes_dirty_eden_bytes() {
        let gc = make_collector();
        let _seed = gc.alloc_object(ClassId::new(1), 0);

        let expected_ptr = {
            let mut regions = gc.regions.write();
            let idx = gc.dbg_current_eden();
            let region = &mut regions[idx];
            let ptr = unsafe { region.data.as_mut_ptr().add(region.cursor()) };
            unsafe { std::ptr::write_bytes(ptr, 0xa5, 4096) };
            region.dirty_high_water = (region.cursor() + 4096) as u32;
            ptr
        };

        let (ptr, len) = gc.refill_tlab(4096).expect("TLAB refill");
        assert_eq!(ptr, expected_ptr);
        assert_eq!(len, 4096);
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        assert!(
            bytes.iter().all(|&byte| byte == 0),
            "G1 must return a fully zeroed TLAB even from dirty recycled Eden"
        );
    }

    // -----------------------------------------------------------------------
    // F-11 — allocation under a shared regions guard
    // -----------------------------------------------------------------------

    /// F-11, engagement. Once an Eden region exists, TLAB refills come out of
    /// it under the SHARED guard; only a region change pays the exclusive one.
    ///
    /// This is the census. A returned TLAB pointer says nothing about which
    /// lock produced it, so without these counters "the fast path is running"
    /// is unfalsifiable — and `shared == 0` is precisely the reading that would
    /// tell you the gate is inert (kill switch on, or a future edit removed
    /// the probe).
    ///
    /// Verified capable of failing: with the shared probe in `refill_tlab`
    /// disabled — the `CRATONVM_G1_SHARED_ALLOC=0` arm — `shared` is 0 and
    /// `exclusive` is 8, and both assertions go red.
    #[test]
    pub fn tlab_refills_after_the_first_are_served_under_the_shared_guard() {
        let gc = make_collector();
        // 64 KiB carves against 1 MiB regions: 16 fit, so 8 refills cross no
        // region boundary once the first one has installed an Eden.
        for _ in 0..8 {
            gc.refill_tlab(64 * 1024).expect("TLAB refill");
        }
        let shared = gc.tlab_shared_carves.load(Ordering::Relaxed);
        let exclusive = gc.tlab_exclusive_carves.load(Ordering::Relaxed);
        assert!(
            shared >= 6,
            "8 refills produced only {shared} shared-guard carves ({exclusive} exclusive); the \
             first refill has to claim a region, but the rest must not (F-11)"
        );
        assert!(
            exclusive <= 2,
            "8 refills took the exclusive guard {exclusive} times — F-11's point is that only a \
             region CHANGE needs it"
        );
    }

    /// F-11, the concurrency claim itself. An allocation must complete while
    /// another thread holds the regions guard for reading.
    ///
    /// That reader stands in for the concurrent marker, which since F-10 scans
    /// its batches under `read()`. Before F-11 an allocation needed `write()`,
    /// so it could not overlap a marker batch at all — this test would sit on
    /// `recv_timeout` until the guard was dropped. The timeout is what makes it
    /// an observation rather than an assertion: a pass means the allocation
    /// genuinely finished with the reader still holding on.
    ///
    /// Verified capable of failing: with the shared probe in `alloc_in_region`
    /// disabled (the kill-switch arm), the allocating thread blocks on
    /// `write()` and the `recv_timeout` expires.
    #[test]
    pub fn an_allocation_completes_while_a_marker_style_reader_holds_the_guard() {
        let gc = Arc::new(make_collector());
        // Establish a current Eden region with room in it, so the allocation
        // below has no reason to want the exclusive guard.
        gc.alloc_object(ClassId::new(1), 0);

        let reader = gc.regions.read();

        let (tx, rx) = std::sync::mpsc::channel();
        let allocator = {
            let gc = Arc::clone(&gc);
            std::thread::spawn(move || {
                let obj = gc.alloc_object(ClassId::new(2), 1);
                let _ = tx.send(obj.as_ptr() as usize);
            })
        };

        let outcome = rx.recv_timeout(std::time::Duration::from_secs(5));
        // Hold the read guard across the assertion, then release: dropping it
        // early would let a blocked writer through and hide the failure.
        let addr = outcome.expect(
            "an allocation must not need the EXCLUSIVE regions guard while a reader holds it \
             (F-11); this timed out, which is the pre-F-11 behaviour",
        );
        drop(reader);
        allocator.join().expect("allocator thread");
        assert!(addr != 0);
    }

    /// F-11, safety of the compare-exchange claim. Concurrent allocators must
    /// never be handed overlapping memory.
    ///
    /// The cursor bump is now a CAS loop instead of a read-modify-write under
    /// an exclusive lock, and the failure mode of getting that wrong is two
    /// threads receiving the same address (or overlapping ranges) and then
    /// scribbling on each other. A single-threaded test cannot see it, so this
    /// runs four real threads and checks two things a duplicate claim breaks:
    /// every address is distinct, and every object still reads back the field
    /// value its own thread wrote.
    ///
    /// Verified capable of failing: replacing the compare-exchange in
    /// `bump_alloc` with a load-then-store (the non-atomic shape the exclusive
    /// lock used to make safe) produces duplicate addresses within a few runs.
    #[test]
    pub fn concurrent_bump_claims_never_hand_out_the_same_bytes_twice() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 1500;

        let gc = Arc::new(make_collector());
        let workers: Vec<_> = (0..THREADS)
            .map(|tid| {
                let gc = Arc::clone(&gc);
                std::thread::spawn(move || {
                    let mut mine = Vec::with_capacity(PER_THREAD);
                    for _ in 0..PER_THREAD {
                        let obj = gc.alloc_object(ClassId::new(11), 1);
                        gc.set_field(obj, 0, Value::Int(tid as i32));
                        mine.push(obj);
                    }
                    // Read back only after this thread has finished allocating,
                    // so an overlap with a LATER allocation is still visible.
                    for obj in &mine {
                        assert_eq!(
                            gc.get_field(*obj, 0).as_int(),
                            Some(tid as i32),
                            "another thread's allocation overwrote this object's field"
                        );
                    }
                    mine.into_iter()
                        .map(|o| o.as_ptr() as usize)
                        .collect::<Vec<_>>()
                })
            })
            .collect();

        let mut all = Vec::new();
        for w in workers {
            all.extend(w.join().expect("allocator thread"));
        }
        assert_eq!(all.len(), THREADS * PER_THREAD);
        let distinct: std::collections::HashSet<usize> = all.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            all.len(),
            "the atomic claim handed the same address to two threads"
        );
    }

    /// F-11, the zeroing contract under a concurrent claim.
    ///
    /// `G1Region::bump_alloc` is the SINGLE establishment of the TLAB zeroing
    /// contract (`G1Region::reset` no longer scrubs a freed region, because
    /// that was 42% of a young pause), and the JIT's inline `new` depends on
    /// it. Moving the bump to a compare-exchange moved the zeroing off the
    /// exclusive lock, so this checks it still covers exactly the claimed range
    /// when several threads claim at once out of an Eden region that has been
    /// deliberately dirtied above its cursor.
    ///
    /// Verified capable of failing: deleting the `write_bytes` in `bump_alloc`
    /// makes every carve come back full of the 0xa5 pattern.
    #[test]
    pub fn concurrent_tlab_carves_are_each_fully_zeroed() {
        const THREADS: usize = 4;
        const CARVE: usize = 16 * 1024;

        let gc = Arc::new(make_collector());
        gc.alloc_object(ClassId::new(1), 0);
        // Dirty everything above the current cursor in this Eden region, the
        // way a recycled region is dirty since the free-scrub deletion.
        {
            let mut regions = gc.regions.write();
            let idx = gc.dbg_current_eden();
            let region = &mut regions[idx];
            let from = region.cursor();
            let len = region.data.len() - from;
            region.dirty_high_water = region.data.len() as u32;
            region.dirty_high_water = region.data.len() as u32;
            unsafe { std::ptr::write_bytes(region.data.as_mut_ptr().add(from), 0xa5, len) };
        }

        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let gc = Arc::clone(&gc);
                std::thread::spawn(move || {
                    for _ in 0..8 {
                        let (ptr, len) = gc.refill_tlab(CARVE).expect("TLAB refill");
                        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
                        assert!(
                            bytes.iter().all(|&b| b == 0),
                            "a concurrently-carved TLAB was not fully zeroed"
                        );
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().expect("carver thread");
        }
    }

    /// F-11, striping. Threads that allocate at the same time must land in
    /// DIFFERENT Eden regions.
    ///
    /// This is the half of F-11 that the lock change alone does not buy.
    /// Moving the bump off the exclusive lock only helps if the threads then
    /// stop fighting over one cursor word and one set of cache lines — and
    /// measured at four threads, shared-guard allocation into a single Eden
    /// region was slower than the exclusive lock it replaced. So the property
    /// under test is not "allocation works" (a single-threaded test covers
    /// that) but "two concurrently allocating threads are handed memory from
    /// two different regions".
    ///
    /// Verified capable of failing: with `eden_stripe_count` forced to 1 — the
    /// `CRATONVM_G1_EDEN_STRIPES=1` arm — every thread reports the same region
    /// index and the distinct count is 1.
    #[test]
    pub fn concurrent_allocators_are_spread_across_eden_stripes() {
        // 64 regions, so the "at most an eighth of the heap" cap allows 8
        // stripes; the machine's parallelism decides the rest.
        let gc = Arc::new(G1Collector::new(G1CollectorConfig {
            heap_size: 64 * 1024 * 1024,
            region_size: 1024 * 1024,
            ..small_config()
        }));
        let stripes = gc.eden_slots.len();
        if stripes < 2 {
            // A single-core machine (or an explicit `=1`) has nothing to
            // spread across. Say so rather than passing quietly: a vacuous
            // pass that looks like a real one is how a dead gate survives.
            eprintln!("[F-11] eden striping not exercised: {stripes} stripe(s) on this machine");
            return;
        }

        const THREADS: usize = 4;
        let start = Arc::new(std::sync::Barrier::new(THREADS));
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let gc = Arc::clone(&gc);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    let (_ptr, region) = gc.alloc_in_region(64).expect("allocation");
                    region
                })
            })
            .collect();
        let used: std::collections::HashSet<usize> = workers
            .into_iter()
            .map(|w| w.join().expect("allocator thread"))
            .collect();

        assert!(
            used.len() >= 2,
            "{THREADS} concurrent allocators all landed in the same Eden region ({used:?}) despite {stripes} stripes being available — striping is what makes the shared allocation guard pay (F-11)"
        );
    }

    // -----------------------------------------------------------------------
    // F-10 — the concurrent marker's critical section
    // -----------------------------------------------------------------------

    /// F-10, engagement. A mark step must acquire the regions lock once per
    /// BATCH, not once per step.
    ///
    /// This is the census, not the claim: it reads the counter the batching
    /// loop bumps, so it fails the moment `MARK_LOCK_BATCH` stops bounding the
    /// hold (a future edit hoisting the guard back out of the loop, or the
    /// `CRATONVM_G1_MARK_LOCK_YIELD=0` arm being taken by accident). The
    /// companion test below is the one that observes the concurrency.
    ///
    /// Verified capable of failing: with `batch` forced to `usize::MAX` — the
    /// pre-F-10 behaviour and exactly what the kill switch selects — the
    /// counter reads 1 and this assertion goes red.
    #[test]
    pub fn a_mark_step_takes_the_regions_lock_once_per_batch_not_once_per_step() {
        const GRAYS: usize = 200;
        let gc = make_collector();
        let grays: Vec<usize> = (0..GRAYS)
            .map(|_| gc.alloc_object(ClassId::new(1), 0).as_ptr() as usize)
            .collect();

        // `start_concurrent_mark` clears the gray set, so seed it afterwards.
        gc.start_concurrent_mark(&stw());
        gc.mark_worklist.lock().extend(grays);

        gc.mark_lock_batches.store(0, Ordering::Relaxed);
        while !gc.concurrent_mark_step(usize::MAX) {}
        let batches = gc.mark_lock_batches.load(Ordering::Relaxed);

        assert!(
            batches >= GRAYS / MARK_LOCK_BATCH,
            "the marker drained {GRAYS} grays in {batches} regions-lock acquisition(s); at MARK_LOCK_BATCH={MARK_LOCK_BATCH} it must take at least {} — a reading of 1 means the guard is held for the whole step again (F-10)",
            GRAYS / MARK_LOCK_BATCH
        );
    }

    /// F-10, the concurrency claim itself. A writer that wants the region table
    /// while the marker is mid-step must get it BEFORE the step ends.
    ///
    /// A quiet single-threaded suite cannot distinguish "the lock is held for a
    /// batch" from "the lock is held for the step" — the same objects are
    /// marked either way. So this runs a real competing thread: it takes the
    /// regions WRITE guard in a loop (which is what an allocation claiming a
    /// fresh region, and the whole of a stop-the-world pause, do), and the
    /// assertion is on how many times it got in while a single
    /// `concurrent_mark_step(usize::MAX)` was running.
    ///
    /// The interleaving is not left to luck: `parking_lot`'s `RwLock` is
    /// task-fair, so once the observer is parked on `write()` the marker's next
    /// `read()` queues behind it. The observer is started and confirmed to have
    /// acquired at least once before the step begins, so "the thread never got
    /// scheduled" cannot masquerade as a pass.
    ///
    /// Verified capable of failing: with `batch` forced to `usize::MAX` the
    /// observer is parked on `write()` for the entire step and the count during
    /// the step is 0.
    #[test]
    pub fn a_long_mark_step_admits_a_waiting_writer_before_it_finishes() {
        // Large enough that one step is hundreds of batches, small enough to
        // fit an 8 MB heap several times over.
        const GRAYS: usize = 20_000;
        // An attempt is INCONCLUSIVE when the marker drains the whole step
        // before this thread can queue behind it. That is rare (we queue after
        // two batches of ~GRAYS/MARK_LOCK_BATCH) and it is not a failure, so
        // try again -- but never silently pass: running out of attempts is red.
        const ATTEMPTS: usize = 8;
        let mut observed: Option<(usize, usize)> = None;
        for _ in 0..ATTEMPTS {
            let gc = Arc::new(make_collector());
            let grays: Vec<usize> = (0..GRAYS)
                .map(|_| gc.alloc_object(ClassId::new(1), 0).as_ptr() as usize)
                .collect();
            gc.start_concurrent_mark(&stw());
            gc.mark_worklist.lock().extend(grays);
            let before = gc.mark_lock_batches.load(Ordering::Relaxed);

            // THE MARKER RUNS ON ITS OWN THREAD AND THIS THREAD IS THE WAITING
            // WRITER, which is the whole of the repair.
            //
            // The previous shape had it the other way round: a spawned observer
            // POLLED `regions.write()` in a loop while this thread ran the step,
            // and the assertion was on how many times it got in. That asked
            // whether a competing OS thread was SCHEDULED during a sub-second
            // step, which is the scheduler's answer, not this collector's -- it
            // failed in 3 of 6 gc-lib-suite runs on unmodified `dev` at host
            // load ~20 (2026-09-06), while the same binary passed 40 of 40 when
            // the test was run alone. An earlier session had already cut the
            // assertion from `>= 8` to `> 0` for the same reason; `> 0` was the
            // same defect one notch down.
            //
            // Blocking removes the scheduler from the claim. `parking_lot`'s
            // `RwLock` is task-fair, so a thread parked in `write()` is QUEUED:
            // the marker's next `read()` lines up behind it and the guard is
            // handed over at the next batch boundary whether or not this thread
            // ever gets a time slice in between. And the counter that says WHEN
            // it was handed over is `mark_lock_batches`, which no scheduler
            // touches -- while we hold the write guard the marker cannot take a
            // batch, so the value we read is exactly the count at handoff.
            let marker = {
                let gc = Arc::clone(&gc);
                std::thread::spawn(move || gc.concurrent_mark_step(usize::MAX))
            };

            // Do not queue until the marker is demonstrably inside the step, or
            // "it was admitted before the step began" would read as a pass.
            while gc.mark_lock_batches.load(Ordering::Relaxed) - before < 2 && !marker.is_finished()
            {
                std::hint::spin_loop();
            }
            if marker.is_finished() {
                let done = marker.join().expect("marker thread");
                let batches = gc.mark_lock_batches.load(Ordering::Relaxed) - before;
                // "The step outran us" and "the guard was never dropped" both
                // arrive here, and only the first is a retry. The batch counter
                // separates them, so the pre-F-10 regression asserts on the
                // spot with the message that names it instead of being retried
                // eight times and reported as an unobservable claim.
                assert!(
                    batches >= GRAYS / MARK_LOCK_BATCH,
                    "the marker drained {GRAYS} grays in {batches} regions-lock acquisition(s); at MARK_LOCK_BATCH={MARK_LOCK_BATCH} it must take at least {} - a reading of 1 means the guard is held for the whole step again (F-10)",
                    GRAYS / MARK_LOCK_BATCH
                );
                assert!(done, "one unbounded step must drain the whole gray set");
                continue;
            }

            let granted_at = {
                let _exclusive = gc.regions.write();
                gc.mark_lock_batches.load(Ordering::Relaxed) - before
            };
            let done = marker.join().expect("marker thread");
            let batches = gc.mark_lock_batches.load(Ordering::Relaxed) - before;

            assert!(done, "one unbounded step must drain the whole gray set");
            // The MAGNITUDE claim, from the same scheduler-free counter: the
            // marker dropped and retook the guard once per batch. Deterministic,
            // so it is checked on every attempt.
            assert!(
                batches >= GRAYS / MARK_LOCK_BATCH,
                "the marker drained {GRAYS} grays in {batches} regions-lock acquisition(s); at MARK_LOCK_BATCH={MARK_LOCK_BATCH} it must take at least {} - a reading of 1 means the guard is held for the whole step again (F-10)",
                GRAYS / MARK_LOCK_BATCH
            );
            observed = Some((granted_at, batches));
            break;
        }

        // THE ADMISSION CLAIM. Pre-F-10 the marker holds the guard from the
        // first gray to the last, so a writer that queues mid-step is served
        // only when the step ends and `granted_at == batches`. Post-F-10 it is
        // served at the next batch boundary, hundreds of batches short of the
        // end. There is no thread race left in either reading.
        let (granted_at, batches) = observed.expect(
            "every attempt was inconclusive: the marker finished the whole step before this thread could queue behind it, so the admission claim was never observed",
        );
        assert!(
            granted_at < batches,
            "a writer that queued after 2 of {batches} batches was not served until batch {granted_at} - the marker held the region table for the entire step (F-10)"
        );
    }

    /// `HIB-DCAST-LATEPHASE.1`, mutator side. `compact_object_field_storage`
    /// returns `None` both for a legacy object and for a genuinely compact one
    /// whose `(class_id, field_count)` no longer resolves to a registered
    /// layout, and `get_field`/`set_field` collapsed the two into
    /// `.unwrap_or((index * SLOT_SIZE, SLOT_SIZE))`. On a real instance of the
    /// second state `alloc_object` sized the body with
    /// `compact_object_body_size` and `num_slots()` is the FIELD COUNT, so the
    /// `index >= num_slots` screen does not bound that legacy stride: the read
    /// escapes the allocation and the write puts a 16-byte `Value` cell past
    /// it. This collector's own walkers already separate the two cases via the
    /// `GC_FLAG_COMPACT` header bit; the accessors must too.
    #[test]
    pub fn field_accessors_refuse_a_compact_object_with_no_registered_layout() {
        let gc = make_collector();
        // No layout is registered for this class id, so the allocation is
        // LEGACY and the cells written below are real, readable cells. Setting
        // the bit afterwards reproduces the racing state (header says compact,
        // registry cannot serve it) without a live redefinition.
        let obj = gc.alloc_object(ClassId::new(999_997), 4);
        gc.set_field(obj, 0, Value::Int(1));
        gc.set_field(obj, 1, Value::Int(2));

        // SAFETY: flips the per-object compact bit on a live, fully
        // initialized allocation; both flag helpers take `&self` and drive the
        // atomic mark word.
        let header = unsafe { &*(obj.as_ptr() as *const ObjectHeader) };
        header.mark_word.fetch_or(
            u32::from(cratonvm_types::GC_FLAG_COMPACT) << 24,
            std::sync::atomic::Ordering::Relaxed,
        );

        assert!(
            matches!(gc.get_field(obj, 0), Value::Object(None)),
            "an unresolvable compact receiver must read as null, not as the \
             legacy 16-byte cell at index * SLOT_SIZE"
        );

        // The write must be dropped, not striped over the legacy cell.
        gc.set_field(obj, 1, Value::Int(77));

        header.mark_word.fetch_and(
            !(u32::from(cratonvm_types::GC_FLAG_COMPACT) << 24),
            std::sync::atomic::Ordering::Relaxed,
        );
        assert!(
            matches!(gc.get_field(obj, 1), Value::Int(2)),
            "the dropped write must not have reached the object at all"
        );
    }

    /// The root array is over-approximate on purpose (`collect_roots` screens
    /// operand-stack slots with the RANGE check `is_heap_addr`, because the
    /// strict probe dropped genuine young / mid-init roots), so a word that is
    /// not an object start reaches the evacuator. It must neither be evacuated —
    /// `evacuate_object` would size a memcpy from those bytes and install a
    /// forwarding entry the VM's post-GC remap applies to the slot, rewriting a
    /// Java `long` — nor be dropped, since the collector cannot prove it is not
    /// a reference. Its region is pinned instead.
    ///
    /// Measured shape, on `org.h2.test.unit.TestValueMemory`: 6-8 such roots per
    /// affected pause, every one of them COPIED before this fix.
    #[test]
    pub fn an_interior_pointer_root_pins_its_region_instead_of_being_evacuated() {
        let gc = make_collector();
        let holder = gc.alloc_object(ClassId::new(1), 4);
        let interior_addr = holder.as_ptr() as usize + HEADER_SIZE;

        // Fabricate a header at the interior address whose TAG byte does not
        // decode — the measured verdicts were `BadElementTag` /
        // `ImplausibleShape`, i.e. bytes that are not a header at all.
        // `object_total_size` still returns >= HEADER_SIZE for it, which is all
        // the pre-fix evacuator checked before copying.
        //
        // SAFETY: `interior_addr` is the first field cell of a live 4-slot
        // object this test owns, so both writes are in-bounds.
        unsafe {
            std::ptr::write(
                interior_addr as *mut ObjectHeader,
                ObjectHeader::new(
                    ClassId::new(2),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    0,
                ),
            );
            (interior_addr as *mut u8)
                .add(cratonvm_types::KIND_TAGS_BYTE_OFFSET)
                .write(0x7f);
        }
        let interior = unsafe { ObjectRef::from_raw(interior_addr as *mut u8) };

        let mut roots = vec![holder, interior];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            roots[1].as_ptr() as usize,
            interior_addr,
            "a root that is not an object must not be rewritten: the collector \
             cannot tell it from a primitive long, and rewriting it changes a \
             Java value"
        );
        assert!(
            !result.pointer_map.contains_key(&interior_addr),
            "no forwarding entry may be installed for a non-object address — the \
             VM's post-GC remap applies the map to the very slot it came from"
        );
        assert_eq!(
            roots[0].as_ptr() as usize,
            holder.as_ptr() as usize,
            "the cost of the pin, stated: the real object in that region does \
             not move either"
        );
    }

    /// The other arm: an ordinary root must NOT pin its region, or the fix above
    /// would be satisfied by pinning everything and G1 would stop collecting.
    #[test]
    pub fn an_ordinary_object_root_does_not_pin_its_region() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let idx = gc
            .lookup_region_for_addr(obj.as_ptr() as usize)
            .expect("the allocation is in a region");
        let regions = gc.regions.read();
        assert!(
            !gc.pinned_region_set_including_non_object_roots(&regions, &[obj])
                .contains(&idx),
            "a real object root must leave its region collectable"
        );
        // THE LIMITATION THIS TEST USED TO PIN IS CLOSED (2026-09-08), and the
        // assertion is inverted rather than deleted so the reason survives.
        //
        // It read: "a zeroed cell decodes as a plausible header -- this screen
        // cannot see it, and a test claiming otherwise would be describing a
        // collector we do not have". That was true of every screen this file
        // had, and it is the door
        // `g1-eight-byte-write-at-a-live-objects-base-20260906` came through:
        // a null `Value` cell is `[4, 0]`, so the address of its PAYLOAD word
        // has `[0, <the next cell's discriminant>]` as its own sixteen bytes
        // and reads as a valid zero-field class-0 object. Following one
        // installs a forwarding mark word at `addr + 8` -- the next cell's
        // discriminant word, inside a live object -- which is that page's
        // corrupt-cell family bit for bit.
        //
        // `empty_header_is_a_real_object` closes it with the only oracle that
        // can: the region's own object grid, walked once, and only for this
        // shape. `CRATONVM_G1_EVAC_EMPTY_HEADER_GRID_PROOF=0` restores the
        // behaviour this assertion used to describe.
        let zeroed_cell =
            unsafe { ObjectRef::from_raw((obj.as_ptr() as usize + HEADER_SIZE) as *mut u8) };
        assert!(
            gc.pinned_region_set_including_non_object_roots(&regions, &[zeroed_cell])
                .contains(&idx),
            "a zeroed field cell is not an object start, and the region's own \
             object grid is what says so"
        );

        // What it DOES catch is the measured shape: bytes that do not decode.
        let garbage_addr = obj.as_ptr() as usize + HEADER_SIZE;
        // SAFETY: the first field cell of a live 1-slot object this test owns.
        unsafe {
            (garbage_addr as *mut u8)
                .add(cratonvm_types::KIND_TAGS_BYTE_OFFSET)
                .write(0x7f);
        }
        let garbage = unsafe { ObjectRef::from_raw(garbage_addr as *mut u8) };
        assert!(
            gc.pinned_region_set_including_non_object_roots(&regions, &[garbage])
                .contains(&idx),
            "an address whose tag bytes do not decode is not an object start"
        );
    }

    /// A self-forwarded SEED handed to `record_outgoing_rset_edges` only has to
    /// be region-RESIDENT to reach the walk — `lookup_region_for_addr` answers
    /// "inside some region's span", not "is an object". A word above the
    /// source region's allocation cursor satisfies the first and fails the
    /// second, and the walk then reads an arbitrary `array_length` /
    /// `num_slots` out of whatever bytes are there.
    ///
    /// That is the measured H2 crash (`TestKillProcessWhileWriting`,
    /// `TestRandomMapOps`, both SIGSEGV at `addr=0x20084400000` with the fault
    /// PC symbolizing to `collect_garbage -> retry_after_evacuation_failure ->
    /// record_outgoing_rset_edges -> for_each_flat_object_reference_trusting_header`). Here the
    /// fabricated header is deliberately *readable* so the pre-fix behaviour is
    /// an observable wrong rset edge rather than a process-killing fault.
    #[test]
    pub fn kept_seed_rset_recording_refuses_a_non_object_seed() {
        let gc = make_collector();
        let live = gc.alloc_object(ClassId::new(1), 1);
        let src_idx = gc
            .lookup_region_for_addr(live.as_ptr() as usize)
            .expect("the live allocation is in a region");

        // A second live region to be the edge's TARGET: an edge is only
        // recorded when the destination is a different, non-Free region.
        let (dst_idx, target_addr) = {
            let mut regions = gc.regions.write();
            let dst_idx = if src_idx + 1 < regions.len() {
                src_idx + 1
            } else {
                src_idx - 1
            };
            regions[dst_idx].set_region_type(RegionType::Old);
            regions[dst_idx].set_cursor(4096);
            let addr = regions[dst_idx].data.as_ptr() as usize;
            (dst_idx, addr)
        };

        // The seed: inside the source region's span, above its cursor. The
        // fabricated header is a one-element reference array pointing at the
        // target region — what the pre-fix walk would have followed.
        let seed_addr = {
            let mut regions = gc.regions.write();
            let region = &mut regions[src_idx];
            let addr = region.data.as_ptr() as usize + region.cursor() + 64;
            assert_eq!(addr & 0x7, 0, "seed must stay 8-aligned");
            // SAFETY: `addr` is inside the region's own `region_size`-byte
            // buffer (the cursor is far below it in a fresh collector), so both
            // writes are in-bounds of an allocation this test owns.
            unsafe {
                std::ptr::write(
                    addr as *mut ObjectHeader,
                    ObjectHeader::new(
                        ClassId::new(1),
                        ObjectKind::Array,
                        ArrayElementType::Reference,
                        1,
                        0,
                    ),
                );
                std::ptr::write((addr + HEADER_SIZE) as *mut u64, target_addr as u64);
            }
            addr
        };
        assert_eq!(
            gc.lookup_region_for_addr(seed_addr),
            Some(src_idx),
            "the seed has to be region-resident, or the walk would be skipped \
             for the wrong reason and this test would pass vacuously"
        );

        let before = KEPT_SEED_REJECTED.load(Ordering::Relaxed);
        {
            let mut regions = gc.regions.write();
            gc.record_outgoing_rset_edges(&mut regions, seed_addr);
        }

        let regions = gc.regions.read();
        assert!(
            !regions[dst_idx].rset.sources().contains(&src_idx),
            "a seed above its region's allocation cursor is not an object; its \
             fabricated header must not be walked, and no rset edge may be \
             recorded from it"
        );
        assert_eq!(
            KEPT_SEED_REJECTED.load(Ordering::Relaxed),
            before + 1,
            "the refusal must be counted, so a real run can be asked whether \
             the guard ever fired"
        );
    }

    /// The other arm of the same guard: a genuine live object seed is still
    /// walked, and its cross-region edge is still recorded. Without this, the
    /// test above could be satisfied by a `record_outgoing_rset_edges` that
    /// refuses everything.
    #[test]
    pub fn kept_seed_rset_recording_still_records_a_real_object_seed() {
        let gc = make_collector();
        let holder = gc.alloc_object(ClassId::new(1), 1);
        let src_idx = gc
            .lookup_region_for_addr(holder.as_ptr() as usize)
            .expect("the live allocation is in a region");

        let (dst_idx, target_addr) = {
            let mut regions = gc.regions.write();
            let dst_idx = if src_idx + 1 < regions.len() {
                src_idx + 1
            } else {
                src_idx - 1
            };
            regions[dst_idx].set_region_type(RegionType::Old);
            regions[dst_idx].set_cursor(4096);
            let addr = regions[dst_idx].data.as_ptr() as usize;
            (dst_idx, addr)
        };

        // Point the holder's only field at the other region.
        // SAFETY: `target_addr` is the non-null base of a live region's buffer.
        let target = unsafe { ObjectRef::from_raw(target_addr as *mut u8) };
        gc.set_field(holder, 0, Value::Object(Some(target)));

        {
            let mut regions = gc.regions.write();
            gc.record_outgoing_rset_edges(&mut regions, holder.as_ptr() as usize);
        }

        let regions = gc.regions.read();
        assert!(
            regions[dst_idx].rset.sources().contains(&src_idx),
            "a real live seed's cross-region edge must still be remembered"
        );
    }

    pub fn unaligned_ptr(buf: &mut [u8], align: usize) -> *mut u8 {
        for offset in 0..align {
            let ptr = unsafe { buf.as_mut_ptr().add(offset) };
            if (ptr as usize) % align != 0 {
                return ptr;
            }
        }
        unreachable!("buffer base cannot be aligned to every offset")
    }

    pub unsafe fn corrupt_header_byte(obj: ObjectRef, offset: usize, value: u8) {
        unsafe {
            (obj.as_ptr() as *mut u8).add(offset).write(value);
        }
    }

    #[test]
    pub fn is_object_address_rejects_invalid_raw_header_tags() {
        let gc = make_collector();
        let invalid_kind = gc.alloc_object(ClassId::new(1), 0);
        unsafe {
            corrupt_header_byte(invalid_kind, cratonvm_types::KIND_TAGS_BYTE_OFFSET, 0x7f);
        }
        assert!(gc
            .is_object_address(invalid_kind.as_ptr() as usize)
            .is_none());

        let invalid_element = gc.alloc_array(ClassId::new(2), ArrayElementType::Int, 1);
        unsafe {
            corrupt_header_byte(invalid_element, cratonvm_types::KIND_TAGS_BYTE_OFFSET, 0x7f);
        }
        assert!(gc
            .is_object_address(invalid_element.as_ptr() as usize)
            .is_none());
    }

    /// str17: a candidate whose header's second word would lie in the next
    /// region is not an object start, and must be refused before that word is
    /// read (the next region may be decommitted). Every real object -- the
    /// first one in a region, one ending exactly at the region's end -- fits.
    #[test]
    pub fn a_candidate_in_a_regions_last_word_is_refused_before_its_header_is_read() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 0);
        let addr = obj.as_ptr() as usize;
        let region = 1usize << gc.region_shift;
        let base = gc.arena_base + (((addr - gc.arena_base) >> gc.region_shift) << gc.region_shift);
        let end = base + region;
        assert!(gc.min_object_fits_in_region(addr));
        assert!(gc.min_object_fits_in_region(base));
        assert!(gc.min_object_fits_in_region(end - cratonvm_types::MIN_OBJECT_SIZE));
        assert!(!gc.min_object_fits_in_region(end - 8));
        assert!(gc.is_object_address(end - 8).is_none());
    }

    #[test]
    pub fn scratch_value_helpers_accept_unaligned_buffers() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(99), 0);
        let values = [
            Value::Int(-7),
            Value::Long(0x0123_4567_89ab_cdef),
            Value::Float(1.25),
            Value::Double(-9.5),
            Value::Object(None),
            Value::Object(Some(obj)),
            Value::ReturnAddress(0x1234),
            Value::Uninitialized,
        ];
        let mut backing = [0u8; SLOT_SIZE + 8];
        let ptr = unaligned_ptr(&mut backing, std::mem::align_of::<Value>());

        for value in values {
            backing.fill(0xa5);
            unsafe {
                value_to_unaligned_ptr(value, ptr);
                assert_eq!(value_from_unaligned_ptr(ptr), value);
            }
        }
    }

    #[test]
    pub fn scratch_array_element_helpers_accept_unaligned_buffers() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(100), 0);
        let cases = [
            (ArrayElementType::Int, Value::Int(-123_456)),
            (ArrayElementType::Long, Value::Long(0x1020_3040_5060_7080)),
            (ArrayElementType::Float, Value::Float(3.5)),
            (ArrayElementType::Double, Value::Double(-17.25)),
            (ArrayElementType::Byte, Value::Int(-5)),
            (ArrayElementType::Boolean, Value::Int(1)),
            (ArrayElementType::Short, Value::Int(-1234)),
            (ArrayElementType::Char, Value::Int(0x03bb)),
            (ArrayElementType::Reference, Value::Object(None)),
            (ArrayElementType::Reference, Value::Object(Some(obj))),
        ];
        let mut backing = [0u8; 16];
        let ptr = unaligned_ptr(&mut backing, std::mem::align_of::<u64>());

        for (element_type, value) in cases {
            backing.fill(0x5a);
            unsafe {
                array_element_to_unaligned_ptr(element_type, value, ptr);
                assert_eq!(array_element_from_unaligned_ptr(element_type, ptr), value);
            }
        }
    }

    #[test]
    pub fn boolean_array_element_decode_zero_extends_raw_byte() {
        let raw = [0xffu8; 1];

        unsafe {
            assert_eq!(
                array_element_from_unaligned_ptr(ArrayElementType::Byte, raw.as_ptr()),
                Value::Int(-1)
            );
            assert_eq!(
                array_element_from_unaligned_ptr(ArrayElementType::Boolean, raw.as_ptr()),
                Value::Int(255)
            );
        }
    }

    #[test]
    pub fn descriptor_defaults_initialize_reference_and_tail_slots() {
        let gc = make_collector();
        let obj = gc.alloc_object_with_descriptors(ClassId::new(7), 4, b"IL");

        assert_eq!(gc.get_field(obj, 0), Value::Int(0));
        assert_eq!(gc.get_field(obj, 1), Value::Object(None));
        assert_eq!(gc.get_field(obj, 2), Value::Object(None));
        assert_eq!(gc.get_field(obj, 3), Value::Object(None));
    }

    #[test]
    pub fn try_descriptor_defaults_initialize_reference_and_tail_slots() {
        let gc = make_collector();
        let obj = gc
            .try_alloc_object_with_descriptors(ClassId::new(8), 4, b"JL")
            .expect("small descriptor-aware allocation should fit");

        assert_eq!(gc.get_field(obj, 0), Value::Long(0));
        assert_eq!(gc.get_field(obj, 1), Value::Object(None));
        assert_eq!(gc.get_field(obj, 2), Value::Object(None));
        assert_eq!(gc.get_field(obj, 3), Value::Object(None));
    }

    // -- Config defaults --

    #[test]
    pub fn config_defaults() {
        let cfg = G1CollectorConfig::default();
        assert_eq!(cfg.heap_size, 256 * 1024 * 1024);
        assert_eq!(cfg.region_size, 1024 * 1024);
        assert_eq!(cfg.max_gc_pause_ms, 200);
        // T19.3.G1 raised from 45 → 70.
        assert_eq!(cfg.ihop_percent, 70);
        assert_eq!(cfg.promotion_age, 15);
        // F-13: 0 means "derive from the machine" — see the field doc.
        assert_eq!(cfg.gc_worker_threads, 0);
        assert!(!cfg.string_dedup_enabled);
        assert_eq!(cfg.mixed_gc_count_target, 8);
        assert_eq!(cfg.old_cset_region_threshold_percent, 10);
    }

    // -- F-03: the region-index bitset --

    #[test]
    pub fn a_region_set_answers_membership_len_and_order() {
        let mut set = RegionSet::new();
        assert!(set.is_empty());
        assert!(!set.contains(&0), "an empty set contains nothing");
        assert!(
            !set.contains(&(usize::MAX / 2)),
            "including far out of range"
        );

        assert!(set.insert(63), "63 and 64 straddle the first word boundary");
        assert!(set.insert(64));
        assert!(set.insert(0));
        assert!(!set.insert(64), "re-inserting reports not-new");
        assert_eq!(set.len(), 3, "and does not double-count");

        for i in [0usize, 63, 64] {
            assert!(set.contains(&i), "{i} was inserted");
        }
        for i in [1usize, 62, 65, 4096] {
            assert!(!set.contains(&i), "{i} was not");
        }
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec![0, 63, 64],
            "iteration is ascending -- several call sites feed it to a walk \
             that is cheaper in address order"
        );
    }

    #[test]
    pub fn a_region_set_collects_extends_and_clears() {
        let mut set: RegionSet = [5usize, 1, 5, 200].into_iter().collect();
        assert_eq!(set.len(), 3, "collect dedups");
        set.extend([200usize, 201]);
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![1, 5, 200, 201]);

        let copy = set.clone();
        assert_eq!(copy, set, "equality is by membership");

        set.clear();
        assert!(set.is_empty());
        assert!(!set.contains(&1));
        assert_ne!(copy, set);
    }

    #[test]
    pub fn a_region_set_sized_up_front_still_grows() {
        // `with_region_capacity` reserves; it does not bound.
        let mut set = RegionSet::with_region_capacity(8);
        assert!(set.is_empty(), "capacity is not membership");
        set.insert(4096);
        assert!(set.contains(&4096));
        assert_eq!(set.len(), 1);
    }

    /// The tripwire that keeps a RegionSet a set of REGIONS. Both kinds of
    /// `usize` set in this file look alike, and a dense bitset handed a heap
    /// address would try to reserve terabytes -- an out-of-memory abort naming
    /// nothing.
    #[test]
    #[should_panic(expected = "is not a region index")]
    pub fn a_region_set_refuses_a_heap_address() {
        let mut set = RegionSet::new();
        set.insert(0x7f_1234_5000);
    }

    // -- F-09 / F-13 / F-14: geometry and worker ergonomics --

    #[test]
    pub fn a_region_size_is_rounded_up_to_a_power_of_two() {
        for already in [0usize, 1, 8, 4096, 1024 * 1024, 32 * 1024 * 1024] {
            assert_eq!(
                normalize_region_size(already),
                already,
                "{already} is already a power of two (or the zero case) and must \
                 pass through unchanged"
            );
        }
        assert_eq!(normalize_region_size(3), 4);
        assert_eq!(normalize_region_size(1024 * 1024 + 1), 2 * 1024 * 1024);
        // 1 MiB - 4 is the unaligned size `arena.rs` records arriving live from
        // the heap ergonomics.
        assert_eq!(normalize_region_size(1024 * 1024 - 4), 1024 * 1024);
    }

    /// The rounding must reach the collector's whole geometry, not just the
    /// shift -- otherwise the allocator and the address lookup disagree about
    /// which region owns an address, which is a use-after-free waiting to be
    /// written.
    #[test]
    pub fn a_collector_built_with_an_odd_region_size_has_one_geometry() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 8 * 1024 * 1024,
            region_size: 1024 * 1024 - 4,
            ..small_config()
        });
        assert_eq!(gc.config.region_size, 1024 * 1024);
        assert_eq!(gc.region_shift, 20);
        assert_eq!(gc.num_regions(), 8);
        assert_eq!(
            gc.reserved_bytes(),
            8 * 1024 * 1024,
            "the reservation is num_regions * the ROUNDED size"
        );
        assert!(
            gc.committed_bytes() <= gc.reserved_bytes(),
            "F-16: committed tracks what has actually been claimed"
        );
        let bases: Vec<usize> = gc.regions.read().iter().map(|r| r.data.addr()).collect();
        for (i, base) in bases.into_iter().enumerate() {
            assert_eq!(gc.lookup_region_for_addr(base), Some(i));
            assert_eq!(gc.lookup_region_for_addr(base + 1024 * 1024 - 1), Some(i));
        }
    }

    /// F-14 — the two free-region searches are instrumented, and the counters
    /// have to move or the measurement the bitmap decision rests on is vacuous.
    #[test]
    pub fn the_free_region_searches_are_counted() {
        let gc = G1Collector::new(small_config());
        let (c0, p0, _, cc0, cp0, _) = free_region_scan_counts();

        // A single-region claim goes through `find_free_region_from`.
        let _ = gc.alloc_object(ClassId::new(1), 1);
        let (c1, p1, w1, _, _, _) = free_region_scan_counts();
        assert!(c1 > c0, "the single-region search must be counted");
        assert!(p1 > p0, "and so must the regions it probed");
        assert!(w1 >= 1, "a call that found a region probed at least one");

        // A humongous allocation goes through `find_contiguous_free`.
        let _ = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        let (_, _, _, cc1, cp1, cw1) = free_region_scan_counts();
        assert!(cc1 > cc0, "the contiguous search must be counted");
        assert!(cp1 > cp0);
        assert!(cw1 >= 1);
    }

    #[test]
    pub fn the_worker_ergonomic_matches_hotspots_taper() {
        // One per CPU up to 8, then five eighths of the rest.
        for (cpus, want) in [
            (0usize, 1usize),
            (1, 1),
            (4, 4),
            (8, 8),
            (16, 13),
            (32, 23),
            (64, 43),
        ] {
            assert_eq!(ergonomic_gc_worker_threads(cpus), want, "cpus={cpus}");
        }
    }

    #[test]
    pub fn an_explicit_worker_count_wins_over_the_ergonomic() {
        let gc = G1Collector::new(G1CollectorConfig {
            gc_worker_threads: 2,
            ..small_config()
        });
        if gc_flags().g1_workers.is_none() {
            let avail = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .max(1);
            assert_eq!(
                gc.parallel_worker_count(),
                2,
                "an explicit count is honoured"
            );
        }
    }

    #[test]
    pub fn an_explicit_worker_count_can_exceed_the_cpu_count() {
        if gc_flags().g1_workers.is_none() {
            let requested = std::thread::available_parallelism().map_or(1, |n| n.get()) + 2;
            let config = G1CollectorConfig {
                gc_worker_threads: requested,
                ..small_config()
            };
            assert_eq!(gc_worker_threads_for(&config), requested);
        }
    }

    #[test]
    pub fn the_default_worker_count_is_derived_from_the_machine() {
        let gc = G1Collector::new(G1CollectorConfig {
            gc_worker_threads: 0,
            ..small_config()
        });
        let avail = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1);
        // Only meaningful when the env override is absent -- which is the
        // normal test environment; skip rather than fail if a soak sets it.
        if gc_flags().g1_workers.is_none() {
            assert_eq!(
                gc.parallel_worker_count(),
                ergonomic_gc_worker_threads(avail)
            );
        }
    }

    // -- F-01: the parallel evacuator's dispatch, and Phase 3.5 on it --

    #[test]
    pub fn the_parallel_evacuator_is_no_longer_vetoed_by_a_live_compiled_frame() {
        // enabled, in_jit, allowed_in_jit -> parallel?
        for (enabled, in_jit, allowed, want) in [
            (true, false, true, true),
            (true, true, true, true),
            (true, true, false, false),
            (true, false, false, true),
            (false, false, true, false),
            (false, true, true, false),
        ] {
            assert_eq!(
                use_parallel_evacuator(enabled, in_jit, allowed),
                want,
                "enabled={enabled} in_jit={in_jit} allowed_in_jit={allowed}"
            );
        }
    }

    #[test]
    pub fn native_allocation_recovery_uses_the_serial_young_evacuator() {
        assert!(use_parallel_young_evacuator(false, true, false, true));
        assert!(
            !use_parallel_young_evacuator(true, true, false, true),
            "a fallible native refusal must recover through the serial to-space path"
        );
    }

    /// Phase 3.5 used to exist only on the serial drivers, which is why the
    /// dispatch forced serial whenever finalizer candidates were pending. With
    /// the default flags this pause now takes the PARALLEL driver, so this test
    /// fails on the pre-F-01 parallel path by losing the object entirely.
    #[test]
    pub fn the_parallel_driver_resurrects_a_dead_finalizable_object() {
        let gc = make_collector();

        let obj = gc.alloc_object(ClassId::new(77), 2);
        let addr = obj.as_ptr() as usize;
        gc.set_field(obj, 0, Value::Int(0xF1A));
        // Something else in Eden so the pause has work and a CSet.
        let _churn = gc.alloc_object(ClassId::new(1), 1);

        // No root names it: it is dead, and only its finalizer registration
        // keeps it alive for one more pause.
        let mut roots: Vec<ObjectRef> = vec![];
        let (result, dead) =
            gc.collect_garbage_with_finalizers(&stw(), &mut roots, &[addr], &NoopMonitors);

        assert_eq!(
            dead.len(),
            1,
            "a dead-but-finalizable CSet object must be resurrected so finalize() has valid memory to run against"
        );
        let moved = dead[0];
        assert_eq!(
            result.pointer_map.get(&addr).copied(),
            Some(moved),
            "and the caller must be handed its POST-copy address"
        );
        let resurrected = unsafe { ObjectRef::from_raw(moved as *mut u8) };
        assert_eq!(
            gc.get_field(resurrected, 0),
            Value::Int(0xF1A),
            "the resurrected copy must still hold the object's fields"
        );
    }

    /// `common-w4b-loader-pin-collision-unroots-another-vms-statics`, the G1
    /// marker side: with two live VMs' user-loader classes on one class id,
    /// the mark-side snapshot keeps THIS heap's loader row, not whichever VM
    /// wrote last (which named a foreign address the marker then rejected,
    /// leaving its own loader without the instance->loader edge).
    #[test]
    pub fn the_mark_side_tables_keep_this_heaps_loader_row_on_a_class_id_collision() {
        use cratonvm_types::loader_pin;
        const VM_OWN: usize = 0x61_0A01;
        const VM_OTHER: usize = 0x61_0B01;
        const CID: u32 = 4_461;
        let gc = make_collector();
        let loader = gc.alloc_object(ClassId::new(90), 1).as_ptr() as usize;
        // A foreign VM's loader: an 8-aligned address no heap can own (the
        // first page is never mapped). Not one just past this arena, which a
        // parallel test's heap could hold -- the registry is process-wide, and
        // a young Generational cycle roots every registered loader address.
        let foreign = 0x7000usize;
        loader_pin::set_loader_pin(VM_OWN, CID, loader);
        loader_pin::set_loader_pin(VM_OTHER, CID, foreign);
        assert_eq!(
            loader_pin::loader_pin_addr(CID),
            Some(foreign),
            "test setup: the VM-less answer must be the foreign row"
        );
        let tables = MarkSideTables::capture(0, gc.arena_base..gc.arena_end);
        let picked = tables.loaders.as_ref().and_then(|m| m.get(&CID).copied());
        loader_pin::forget_vm_loader_pins(VM_OWN);
        loader_pin::forget_vm_loader_pins(VM_OTHER);
        assert_eq!(
            picked,
            Some(loader),
            "the snapshot must name this heap's loader"
        );
    }

    /// `take_resurrection_closure` (the depth-2 set of
    /// `common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot`):
    /// Phase 3.5 reports, at PRE-collection addresses, the dead finalizable
    /// and the child only it reaches -- and never an object the strong
    /// closure copied first, even when the finalizable reaches it too (that
    /// object would otherwise lose its weak references).
    #[test]
    pub fn phase_3_5_reports_its_resurrection_closure_at_pre_collection_addresses() {
        let gc = make_collector();
        let shared = gc.alloc_object(ClassId::new(88), 1);
        let child = gc.alloc_object(ClassId::new(89), 1);
        let fin = gc.alloc_object(ClassId::new(77), 2);
        gc.set_field(fin, 0, Value::Object(Some(child)));
        gc.set_field(fin, 1, Value::Object(Some(shared)));
        let (shared_a, child_a, fin_a) = (
            shared.as_ptr() as usize,
            child.as_ptr() as usize,
            fin.as_ptr() as usize,
        );
        let mut roots: Vec<ObjectRef> = vec![shared];
        let (_result, dead) =
            gc.collect_garbage_with_finalizers(&stw(), &mut roots, &[fin_a], &NoopMonitors);
        assert_eq!(
            dead.len(),
            1,
            "test setup: Phase 3.5 must have resurrected the finalizable"
        );
        let closure = gc.take_resurrection_closure();
        assert!(
            closure.contains(&fin_a),
            "the finalizable itself: {closure:x?}"
        );
        assert!(
            closure.contains(&child_a),
            "the child only it reaches: {closure:x?}"
        );
        assert!(
            !closure.contains(&shared_a),
            "a strongly reachable object must never be reported: {closure:x?}"
        );
        assert!(gc.take_resurrection_closure().is_empty(), "drained");

        // A pause with no dead finalizable reports nothing, whatever the last
        // one recorded.
        let (_r, dead) = gc.collect_garbage_with_finalizers(&stw(), &mut roots, &[], &NoopMonitors);
        assert!(dead.is_empty());
        assert!(gc.take_resurrection_closure().is_empty());
    }

    /// Phase 3.5 must not make a SECOND copy of an object the closure already
    /// evacuated.
    ///
    /// The two evacuators agree that "where did this object go" is answered by
    /// the forwarding tag in the from-space mark word (F-02). The parallel
    /// driver used to RETIRE those tags at the end of `parallel_evacuate`,
    /// which is before Phase 3.5 rather than after Phase 4 where the serial
    /// drivers retire theirs — so every already-copied object Phase 3.5 reached
    /// looked untouched, was copied again, and had its `pointer_map` entry
    /// overwritten with the duplicate.
    ///
    /// Nothing crashes when that happens: `retire_forwards` preserves the shape
    /// quartet, so the duplicate is a well-formed object. What breaks is
    /// IDENTITY — the root reaches one copy and the resurrected finalizable's
    /// field reaches the other. That is what this test measures, rather than
    /// the counter, because identity is the property the heap owes its users
    /// and it holds however the collector chooses to implement the check.
    ///
    /// Shape of the fixture: one live object named by BOTH a root (so the
    /// closure copies it in Phase 3) and a dead finalizable (so Phase 3.5
    /// reaches it again while resurrecting).
    #[test]
    pub fn phase_3_5_does_not_split_the_identity_of_an_object_the_closure_copied() {
        let gc = make_collector();

        let shared = gc.alloc_object(ClassId::new(88), 1);
        gc.set_field(shared, 0, Value::Int(0x5EED));

        let fin = gc.alloc_object(ClassId::new(77), 2);
        let fin_addr = fin.as_ptr() as usize;
        gc.set_field(fin, 0, Value::Object(Some(shared)));
        // Something else in Eden so the pause has work beyond these two.
        let _churn = gc.alloc_object(ClassId::new(1), 1);

        // `shared` is rooted; `fin` is not, so only its finalizer registration
        // keeps it for one more pause.
        let mut roots: Vec<ObjectRef> = vec![shared];
        let (result, dead) =
            gc.collect_garbage_with_finalizers(&stw(), &mut roots, &[fin_addr], &NoopMonitors);

        assert_eq!(
            dead.len(),
            1,
            "test setup: the dead finalizable must have been resurrected, or Phase 3.5 never ran and this test checks nothing"
        );
        let resurrected = unsafe { ObjectRef::from_raw(dead[0] as *mut u8) };
        let via_finalizable = match gc.get_field(resurrected, 0) {
            Value::Object(Some(r)) => r.as_ptr() as usize,
            other => panic!("the resurrected finalizable's slot 0 reads {other:?}"),
        };
        let via_root = roots[0].as_ptr() as usize;
        assert_eq!(
            via_root, via_finalizable,
            "the root and the resurrected finalizable must reach the SAME object. Two addresses here is an identity split: Phase 3.5 copied an object the closure had already copied, because the forwarding tag it decides on had been retired before it ran."
        );
        assert_eq!(
            result.pointer_map.get(&(shared.as_ptr() as usize)).copied(),
            Some(via_root),
            "and the pause's forwarding map must name that same copy — it is what every root, monitor and worklist remap outside the collector is rewritten through"
        );
    }

    /// The PARALLEL twin of
    /// `no_object_carries_a_forwarding_tag_after_a_pause_that_keeps_regions`.
    ///
    /// That test drives `young_collection_serial` explicitly, with a comment
    /// saying the dispatcher would otherwise pick the parallel evacuator "which
    /// retires its forwards inside `parallel_evacuate` and would make this test
    /// vacuous". Both drivers now retire at the same protocol point, so the
    /// obligation is the same one and both arms must be checked — a mirror that
    /// only moves in one direction is how the serial arm's retirement test
    /// passed with `retire_forwards` deleted.
    #[test]
    pub fn the_parallel_driver_also_leaves_no_forwarding_tag_behind() {
        let gc = make_collector();

        let head = gc.alloc_object(ClassId::new(1), 1);
        let mut prev = head;
        for _ in 0..32 {
            let next = gc.alloc_object(ClassId::new(1), 1);
            gc.set_field(prev, 0, Value::Object(Some(next)));
            prev = next;
        }

        // Leave the evacuator nowhere to copy to, so objects self-forward and
        // their regions are KEPT — a forward left in a freed region is zeroed
        // by Phase 5 and would make this vacuous.
        let region_count = gc.num_regions();
        for i in 0..region_count {
            assert!(gc.commit_through_region(i), "fixture: commit must succeed");
        }
        gc.with_regions_mut(|regions| {
            for r in regions.iter_mut() {
                if r.region_type() == RegionType::Free {
                    r.set_region_type(RegionType::Old);
                    r.set_cursor(r.data.len());
                }
            }
        });

        let mut roots: Vec<ObjectRef> = vec![head];
        let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert!(
            result.pointer_map.iter().any(|(k, v)| k == v),
            "test setup: the pause must have SELF-FORWARDED something, or no region is kept and this test cannot observe a surviving forward"
        );

        let offenders = forwarded_objects_in_heap(&gc);
        assert!(
            offenders.is_empty(),
            "{} object(s) still carry a forwarding tag after a PARALLEL pause, e.g. 0x{:x}",
            offenders.len(),
            offenders.first().copied().unwrap_or(0)
        );
    }

    /// The same object, still REACHABLE, must not be reported dead — otherwise
    /// the test above would pass on an implementation that resurrects
    /// everything.
    #[test]
    pub fn a_live_finalizable_object_is_not_reported_dead_by_the_parallel_driver() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(77), 2);
        let addr = obj.as_ptr() as usize;
        let _churn = gc.alloc_object(ClassId::new(1), 1);

        let mut roots: Vec<ObjectRef> = vec![obj];
        let (_result, dead) =
            gc.collect_garbage_with_finalizers(&stw(), &mut roots, &[addr], &NoopMonitors);
        assert!(
            dead.is_empty(),
            "a rooted finalizable object survived normally and must not be enqueued"
        );
    }

    // -- F-02: forwarding lives in the mark word --

    /// Every object below every region's cursor whose mark word still carries
    /// a forwarding tag.
    pub fn forwarded_objects_in_heap(gc: &G1Collector) -> Vec<usize> {
        let mut out = Vec::new();
        let regions = gc.regions.read();
        for r in regions.iter() {
            if matches!(
                r.region_type(),
                RegionType::Free | RegionType::HumongousContinuation
            ) {
                continue;
            }
            let base = r.data.as_ptr() as usize;
            let mut off = 0usize;
            while off < r.cursor() {
                let addr = base + off;
                let header = unsafe { &*(addr as *const ObjectHeader) };
                let size = object_total_size(header);
                if size < HEADER_SIZE || off + size > r.cursor() {
                    break;
                }
                if header.is_forwarded() {
                    out.push(addr);                }
                off += size;
            }
        }
        out
    }

    /// F-02 — the serial evacuator installs forwards in from-space mark words,
    /// so it inherits the obligation to retire them. A forward that survives a
    /// pause is read by the NEXT cycle's fast path as a this-cycle answer,
    /// which strands a root on an abandoned object.
    ///
    /// The pause that can show this is one that KEEPS a region. Phase 5
    /// zero-fills a freed region, so a forward left in one is invisible — and
    /// the first version of this test ran a healthy pause and passed with
    /// `retire_forwards` commented out, i.e. it was checking nothing. To-space
    /// is therefore exhausted deliberately here: every region that is not Eden
    /// is made a full Old region, so no destination of either type has room and
    /// no Free region can be claimed, and every reached object self-forwards.
    #[test]
    pub fn no_object_carries_a_forwarding_tag_after_a_pause_that_keeps_regions() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 4 * 1024 * 1024,
            region_size: 256 * 1024,
            ..small_config()
        });

        // A retained chain, so the pause has live objects to relocate.
        let head = gc.alloc_object(ClassId::new(1), 1);
        let mut prev = head;
        for _ in 0..32 {
            let next = gc.alloc_object(ClassId::new(1), 1);
            gc.set_field(prev, 0, Value::Object(Some(next)));
            prev = next;
        }

        // Leave the evacuator nowhere to copy to. F-16: a region the fixture
        // declares FULL has to be committed as well as re-typed — a real full
        // heap reached that state by allocating into it, and the walkers this
        // pause runs will read every byte below the cursor.
        let region_count = gc.num_regions();
        for i in 0..region_count {
            assert!(gc.commit_through_region(i), "fixture: commit must succeed");
        }
        gc.with_regions_mut(|regions| {
            for r in regions.iter_mut() {
                if r.region_type() == RegionType::Free {
                    r.set_region_type(RegionType::Old);
                    r.set_cursor(r.data.len());
                }
            }
        });

        let mut roots: Vec<ObjectRef> = vec![head];
        // The SERIAL body, explicitly: the dispatcher would pick the parallel
        // evacuator here, which retires its forwards inside `parallel_evacuate`
        // and would make this test vacuous.
        let result = gc.young_collection_serial(&mut roots, &NoopMonitors);
        assert!(
            result.pointer_map.iter().any(|(k, v)| k == v),
            "test setup: the pause must have SELF-FORWARDED something, or no \
             region is kept and this test cannot observe a surviving forward"
        );
        assert_eq!(
            gc.count_regions(RegionType::Survivor),
            0,
            "a failed young region must become Old, without a surviving forward"
        );

        let offenders = forwarded_objects_in_heap(&gc);
        assert!(
            offenders.is_empty(),
            "{} object(s) still carry a forwarding tag after the pause, e.g. \
             0x{:x} — `retire_forwards` did not cover them",
            offenders.len(),
            offenders.first().copied().unwrap_or(0)
        );
    }

    /// A self-forward's saved mark comes back on retirement, and ONLY for an
    /// address the retiring pause's map records as a self-forward: the saved
    /// list lives on the collector, so a stale entry must not be replayed onto
    /// whatever its address holds later.
    #[test]
    pub fn a_saved_self_forward_mark_is_restored_only_for_this_pauses_self_forwards() {
        let gc = make_collector();
        let a = gc
            .alloc_array(ClassId::new(9), ArrayElementType::Long, 4)
            .as_ptr() as usize;
        let b = gc
            .alloc_array(ClassId::new(9), ArrayElementType::Long, 4)
            .as_ptr() as usize;
        let mark_of = |addr: usize| unsafe { (*(addr as *const ObjectHeader)).mark() };
        let (a0, b0) = (mark_of(a), mark_of(b));
        // `a` is self-forwarded this pause and was THIN_LOCKED before it.
        let a_saved = (a0 & !0xFFFF)
            | cratonvm_types::MARK_THIN_LOCKED
            | (5 << cratonvm_types::MARK_PAYLOAD_SHIFT);
        unsafe { (*(a as *const ObjectHeader)).set_forwarding_address(a as *mut u8) };
        // `b` has a stale entry from some earlier pause and is not in this map.
        let b_stale = (b0 & !0xFFFF)
            | cratonvm_types::MARK_THIN_LOCKED
            | (7 << cratonvm_types::MARK_PAYLOAD_SHIFT);
        gc.self_forward_marks.lock().push((a, a_saved));
        gc.self_forward_marks.lock().push((b, b_stale));
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(a, a);
        gc.retire_forwards(&map, &RegionSet::default());
        assert_eq!(
            mark_of(a) & 0xFFFF,
            a_saved & 0xFFFF,
            "a's lock state comes back"
        );
        assert_eq!(mark_of(b), b0, "b's word is untouched by the stale entry");
        assert!(
            gc.self_forward_marks.lock().is_empty(),
            "the list is drained"
        );
    }

    /// An interior root whose bytes decode as a plausible header but carry no
    /// `GC_FLAG_HEADER` pins its region instead of being evacuated. The shape
    /// is a compact instance's field seen from a JIT frame's saved-register
    /// image (`obj + 8`), reproduced here inside an array's data: class id 5,
    /// a zero mark, a zero shape word.
    #[test]
    pub fn an_interior_root_without_the_header_flag_pins_its_region() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(9), ArrayElementType::Long, 8);
        let interior = arr.as_ptr() as usize + HEADER_SIZE;
        // SAFETY: the first two elements of the live array.
        unsafe {
            std::ptr::write(interior as *mut u32, 5);
            std::ptr::write((interior + 4) as *mut u32, 0);
        }
        let idx = gc
            .lookup_region_for_addr(interior)
            .expect("the array is in a region");
        let regions = gc.regions.read();
        let before = ROOT_WITHOUT_HEADER_FLAG_PINNED.load(Ordering::Relaxed);
        let root = unsafe { ObjectRef::from_raw(interior as *mut u8) };
        let set = gc.pinned_region_set_including_non_object_roots(&regions, &[root]);
        assert!(set.contains(&idx), "the interior root's region is pinned");
        assert!(ROOT_WITHOUT_HEADER_FLAG_PINNED.load(Ordering::Relaxed) > before);
        // The array itself, a real header, is not refused by this rule.
        let real = unsafe { ObjectRef::from_raw(arr.as_ptr()) };
        let set = gc.pinned_region_set_including_non_object_roots(&regions, &[real]);
        assert!(!set.contains(&idx), "a stamped header is still followed");
    }

    /// r11w19 g1store — the band scans' entry KEEPS a flag-less interior word
    /// that fits below its region's cursor, and that is its contract, not a
    /// gap (page `r11-rt-g1-object-check-accepts-interior-words`, Wave 19).
    ///
    /// Every root the band scan accepts is also a G1 pin: the root gatherer
    /// pins `roots[jit_scan_start..]` by address, and this word's pin keeps its
    /// REGION out of the collection set, so the object it is interior to does
    /// not move under a derived pointer. A reject here would drop that pin. The
    /// start proof lives at the consumers instead: the evacuator pins (never
    /// follows) the word, `remark` places it in its containing object, and the
    /// gray gate refuses it as torn. This test fails if the entry ever starts
    /// rejecting the word without those consumers being revisited.
    #[test]
    pub fn r11w19_the_root_scan_entry_keeps_a_flagless_interior_word_for_its_region_pin() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(9), ArrayElementType::Long, 8);
        let interior = arr.as_ptr() as usize + HEADER_SIZE;
        // SAFETY: the first two elements of the live array.
        unsafe {
            std::ptr::write(interior as *mut u32, 5);
            std::ptr::write((interior + 4) as *mut u32, 0);
        }
        assert!(
            gc.is_object_address_for_root_scan(arr.as_ptr() as usize)
                .is_some(),
            "the array itself is a root"
        );
        assert!(
            gc.is_object_address_for_root_scan(interior).is_some(),
            "the interior word must stay a root: its acceptance is its region's pin"
        );
        let idx = gc
            .lookup_region_for_addr(interior)
            .expect("the array is in a region");
        let regions = gc.regions.read();
        // SAFETY: an 8-aligned in-heap address; `ObjectRef` is only carried.
        let root = unsafe { ObjectRef::from_raw(interior as *mut u8) };
        let set = gc.pinned_region_set_including_non_object_roots(&regions, &[root]);
        assert!(
            set.contains(&idx),
            "the evacuator pins the word's region, never follows it"
        );
    }

    /// r12w1 g1store (proposal W19-3): a remark root that the object grid
    /// places inside a decodable header WITHOUT `GC_FLAG_HEADER` is not
    /// seeded. It is `Unresolved` (the cleanup fail-safe the gray gate would
    /// have reached one pop later) and counted on the collector, while a root
    /// inside a published object still seeds that object.
    #[test]
    pub fn r12w1_a_root_in_a_flagless_grid_object_is_unresolved_not_seeded() {
        let gc = make_collector();
        let a = gc.alloc_array(ClassId::new(9), ArrayElementType::Long, 8);
        let b = gc.alloc_array(ClassId::new(9), ArrayElementType::Long, 8);
        assert_eq!(
            gc.lookup_region_for_addr(a.as_ptr() as usize),
            gc.lookup_region_for_addr(b.as_ptr() as usize),
            "fixture: both arrays in one region"
        );
        // SAFETY: `b` is a live array; its header is read and written through
        // the atomic mark-word accessors only.
        let hb = unsafe { &*(b.as_ptr() as *const ObjectHeader) };
        hb.clear_gc_flags(cratonvm_types::GC_FLAG_HEADER);
        let root_a = a.as_ptr() as usize + HEADER_SIZE + 8;
        let root_b = b.as_ptr() as usize + HEADER_SIZE + 8;
        let before = gc.mark_gray_without_header_flag.load(Ordering::Relaxed);
        {
            let regions = gc.regions.read();
            let mut grids = MarkRootGrids::default();
            assert_eq!(
                gc.mark_root_seed(&regions, root_a, &mut grids),
                MarkRootSeed::Object(a.as_ptr() as usize),
                "a root inside a published object seeds that object"
            );
            if g1_mark_gray_requires_header_flag() {
                assert_eq!(
                    gc.mark_root_seed(&regions, root_b, &mut grids),
                    MarkRootSeed::Unresolved,
                    "a root inside a flag-less header is not a start to seed"
                );
                assert_eq!(
                    gc.mark_gray_without_header_flag.load(Ordering::Relaxed),
                    before + 1
                );
            }
        }
        hb.add_gc_flags(cratonvm_types::GC_FLAG_HEADER);
    }

    /// The G1 near-full proxy row (`CRATONVM_G1_MARK_ROOT_SCREEN_DECODES`): a
    /// root naming payload bytes that read as a published header, but one
    /// whose extent the gray gate refuses, is not its own seed. The grid
    /// places it in the array that holds it. Before, the seed was pushed, the
    /// marker refused it as torn, and the cycle retained every region.
    #[test]
    pub fn a_root_whose_header_the_gray_gate_refuses_seeds_the_array_around_it() {
        let gc = make_collector();
        let small = gc.alloc_array(ClassId::new(9), ArrayElementType::Long, 8);
        let holder = gc.alloc_array(ClassId::new(9), ArrayElementType::Long, 64);
        let base = holder.as_ptr() as usize;
        // SAFETY: the live array's header.
        let size = object_total_size(unsafe { &*(base as *const ObjectHeader) });
        // The copy sits in the holder's last 32 bytes, so the 80-byte extent
        // its header claims runs past everything allocated after it.
        let fake = base + size - 32;
        // SAFETY: `fake..fake + HEADER_SIZE` lies inside the holder's payload;
        // the source is the live `small` array's header.
        unsafe {
            std::ptr::copy_nonoverlapping(
                small.as_ptr() as *const u8,
                fake as *mut u8,
                HEADER_SIZE,
            );
        }
        let regions = gc.regions.read();
        let idx = gc.lookup_region_for_addr(fake).expect("in a region");
        assert_eq!(
            regions[idx].cursor(),
            base + size - regions[idx].data.as_ptr() as usize,
            "fixture: the holder ends the region's allocations"
        );
        assert!(
            classify_mark_scan_target(&regions[idx], fake, &AtomicU64::new(0)).is_err(),
            "fixture: the gray gate refuses the copy"
        );
        assert!(
            gc.classify_candidate_header(&regions, fake).0 == HeaderVerdict::Object
                && gc.addr_is_followable_object(&regions, fake, "test"),
            "fixture: the screens before the gate's accept the copy"
        );
        let mut grids = MarkRootGrids::default();
        let seed = gc.mark_root_seed(&regions, fake, &mut grids);
        {
            assert_eq!(seed, MarkRootSeed::Object(base), "the array that holds the word");
        }
    }

    /// r12w2 g1store (proposals W1-2/W1-3): the pin census tells a pin whose
    /// object a root already seeds from one only the pin names (an orphan),
    /// places interior words in their objects, and returns the orphans' starts
    /// only, once each.
    #[test]
    pub fn r12w2_jit_pin_census_tells_covered_pins_from_orphans() {
        let gc = make_collector();
        let a = gc.alloc_array(ClassId::new(9), ArrayElementType::Long, 8);
        let b = gc.alloc_array(ClassId::new(9), ArrayElementType::Long, 8);
        let (a0, b0) = (a.as_ptr() as usize, b.as_ptr() as usize);
        let regions = gc.regions.read();
        let mut grids = MarkRootGrids::default();
        let pins = [
            a0,
            a0 + HEADER_SIZE + 8,
            b0 + HEADER_SIZE + 8,
            b0 + HEADER_SIZE + 8,
            0x1000,
        ];
        let (seeds, tally) = gc.jit_pin_mark_seeds(&regions, &pins, &[a0], &mut grids);
        assert_eq!(seeds, vec![b0], "only the object no root names is seeded");
        assert_eq!(tally.pins, 4, "distinct pins");
        assert_eq!(tally.covered, 2, "a0 itself, and the word inside a0");
        assert_eq!(tally.interior, 2);
        assert_eq!(tally.nothing, 1, "outside the arena");
        assert_eq!(tally.unresolved, 0);
        assert_eq!(tally.orphans, 1);
        assert_eq!(tally.orphan_interior, 1);
        assert_eq!(tally.orphan_humongous, 0);
    }

    /// r12w2 g1store (proposal W1-2): an object a compiled frame reaches only
    /// through a rejected band word is MARKED, and so is what it references.
    /// The word's pin (the humongous span base, `band_reject_pin`) was never a
    /// mark root, so before this the holder stayed unmarked (cleanup kept its
    /// span through the pin, wave 1) and the Old object only it references
    /// read dead to remark-time reference processing and to cleanup.
    #[test]
    pub fn r12w2_remark_marks_what_only_a_jit_pin_names() {
        let cfg = G1CollectorConfig {
            promotion_age: 1,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);
        let child = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(child, 0, Value::Int(41));
        let mut roots = vec![child];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        let child = roots[0];
        roots.clear();
        let holder = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 100_000);
        assert!(gc.is_humongous(holder));
        gc.set_array_element(holder, 5, Value::Object(Some(child)))
            .unwrap();
        // An element cursor: rejected by the root screen, owes the span base.
        let cursor = holder.as_ptr() as usize + 64;
        assert!(gc.is_object_address_for_root_scan(cursor).is_none());

        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        cratonvm_gc_core::gc_quiescence::clear_pinned_jit_roots();
        let owed = gc.band_reject_pin(cursor).expect("owed pin").addr;
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[owed]);
        cratonvm_gc_core::gc_quiescence::enter();
        gc.start_concurrent_mark(&stw());
        gc.remark(&stw(), &[]);
        while !gc.concurrent_mark_step(usize::MAX) {}
        let holder_live = gc.is_live_after_mark(holder.as_ptr() as usize);
        let child_live = gc.is_live_after_mark(child.as_ptr() as usize);
        gc.abort_concurrent_mark();
        cratonvm_gc_core::gc_quiescence::leave();
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[]);

        if g1_mark_root_screen() {
            assert!(holder_live, "the pinned humongous holder is marked");
            assert!(child_live, "and the Old object only it references");
        }
    }

    /// JIT round 21 lane parkcredit -- the wiring half of `jr21_movclaim_tests`
    /// (which pins down `g1_kept_pin_words` alone): the initiator's movable claim is a
    /// thread-local of the CALLING thread, so it may cancel only the calling
    /// thread's own pin words. A word another thread published (a deposit, or
    /// a blocked peer's helper window) names a slot nobody rewrites, and must
    /// keep its region out of the collection set even when one of the
    /// initiator's oop-map slots claims the same object movable.
    #[test]
    pub fn jr21_initiator_movable_claim_does_not_unpin_a_peer_word() {
        let gc = G1Collector::new(small_config());
        let shared = gc.alloc_object(ClassId::new(1), 1);
        let addr = shared.as_ptr() as usize;
        let region = gc.lookup_region_for_addr(addr).unwrap();

        cratonvm_gc_core::gc_quiescence::clear_pinned_jit_roots();
        cratonvm_gc_core::gc_quiescence::clear_movable_jit_roots();
        let (published_tx, published_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let peer = std::thread::spawn(move || {
            cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[addr]);
            published_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[]);
        });
        published_rx.recv().unwrap();
        // The initiator's own band scan claims the object movable.
        cratonvm_gc_core::gc_quiescence::add_movable_jit_root(addr);
        cratonvm_gc_core::gc_quiescence::enter();
        let peer_pinned = gc.jit_pinned_region_set().contains(&region);
        // Control: the same claim does cancel the initiator's OWN word.
        release_tx.send(()).unwrap();
        peer.join().unwrap();
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[addr]);
        let own_pinned = gc.jit_pinned_region_set().contains(&region);
        cratonvm_gc_core::gc_quiescence::leave();
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[]);
        cratonvm_gc_core::gc_quiescence::clear_movable_jit_roots();

        assert!(
            peer_pinned,
            "a peer's published word pins its region whatever the initiator claims"
        );
        if g1_movable_pins_enabled() {
            assert!(
                !own_pinned,
                "the initiator's movable claim cancels its own word"
            );
        }
    }

    /// JIT round 23 wave 1b, lane L7 (R23W1L7-2): the OOME ladder line's pin
    /// figure counts the regions the published words name (a region once,
    /// however many words name it), and the words. Other tests' words lie in
    /// other collectors' arenas and name no region of this one.
    #[test]
    pub fn r23w1b_published_jit_pin_census_counts_regions_not_words() {
        let gc = G1Collector::new(small_config());
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let addr = obj.as_ptr() as usize;
        cratonvm_gc_core::gc_quiescence::clear_pinned_jit_roots();
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[addr, addr + 8]);
        cratonvm_gc_core::gc_quiescence::enter();
        let (regions, words) = gc.published_jit_pin_census();
        cratonvm_gc_core::gc_quiescence::leave();
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[]);
        assert_eq!(regions, 1, "two words in one region name one region");
        assert!(words >= 2, "both words are counted, got {words}");
    }

    /// `g1-jit-windows-parknanos-cached-invoke-target-drop-sigsegv`: a word the
    /// cross-thread scan captured from a blocked peer's native stack pins its
    /// region, with no compiled frame live, so a young pause leaves the object
    /// where it is and the wake has nothing to store over a word that may not
    /// be a reference.
    #[test]
    pub fn g1pn_blocked_peer_stack_word_pins_its_region() {
        let gc = G1Collector::new(small_config());
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let addr = obj.as_ptr() as usize;
        let region = gc.lookup_region_for_addr(addr).unwrap();

        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000, addr);
        let pinned = gc.jit_pinned_region_set().contains(&region);
        let mut roots = vec![obj];
        gc.young_collection(&mut roots, &NoopMonitors);
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();

        if cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled() {
            assert!(pinned, "the captured word pins its region");
            assert_eq!(roots[0].as_ptr() as usize, addr, "the object did not move");
        }
    }

    /// ngr: a region leaving the Free pool takes a hand-out stamp above every
    /// earlier one; reclassifying a region that is not Free keeps its stamp.
    #[test]
    pub fn ngr_region_handout_stamp_moves_only_on_leaving_free() {
        let gc = G1Collector::new(small_config());
        gc.with_regions_mut(|regions| {
            let free: Vec<usize> = (0..regions.len())
                .filter(|&i| regions[i].region_type() == RegionType::Free)
                .take(2)
                .collect();
            let (a, b) = (free[0], free[1]);
            regions[a].set_region_type(RegionType::Eden);
            let first = regions[a].handout_stamp();
            assert!(first > 0);
            regions[a].set_region_type(RegionType::Old);
            assert_eq!(regions[a].handout_stamp(), first, "Eden -> Old keeps it");
            regions[b].set_region_type(RegionType::HumongousStart);
            assert!(regions[b].handout_stamp() > first);
            regions[a].set_region_type(RegionType::Free);
            regions[a].set_region_type(RegionType::Eden);
            assert!(regions[a].handout_stamp() > regions[b].handout_stamp());
        });
    }

    /// ngr: the post-block screen's decision.
    #[test]
    pub fn ngr_post_block_region_decision() {
        assert!(!peer_word_names_post_block_region(10, None), "no stamp: kept");
        assert!(!peer_word_names_post_block_region(9, Some(10)));
        assert!(!peer_word_names_post_block_region(10, Some(10)));
        assert!(peer_word_names_post_block_region(11, Some(10)));
    }

    /// ngr (`native-growth-reclaim-probe-fails-on-g1`): a blocked peer's
    /// captured word naming a region handed out after the peer published its
    /// block stamp neither pins that region nor stays in the capture buffer
    /// the blocked-wake fold reads. A word into a region handed out before the
    /// stamp keeps its pin, and so does any word of a peer with no stamp, except
    /// in cleanup's set, which no peer word enters.
    #[test]
    pub fn ngr_post_block_peer_word_neither_pins_nor_reaches_the_fold() {
        let gc = G1Collector::new(small_config());
        let early = gc.alloc_object(ClassId::new(1), 1);
        let early_addr = early.as_ptr() as usize;
        let early_region = gc.lookup_region_for_addr(early_addr).unwrap();
        // The peer blocks after `early` exists and before `late` does.
        let (stamped, unstamped) = (0x6e67_0001u32, 0x6e67_0002u32);
        let slot = cratonvm_gc_core::gc_quiescence::register_self_block_stamp_slot(stamped);
        slot.store(
            cratonvm_gc_core::gc_quiescence::region_handout_clock_at_block(),
            Ordering::Release,
        );
        let late = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        let late_addr = late.as_ptr() as usize;
        let late_region = gc.lookup_region_for_addr(late_addr).unwrap();

        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(stamped, 0x1000, early_addr);
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(stamped, 0x1008, late_addr);
        let pinned = gc.jit_pinned_region_set();
        let mut left = Vec::new();
        cratonvm_gc_core::gc_quiescence::peer_stack_slot_values_into(&mut left, |_| true);
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(unstamped, 0x2000, late_addr);
        let pinned_unstamped = gc.jit_pinned_region_set();
        let mut cleanup_pinned = RegionSet::new();
        gc.with_regions_mut(|regions| cleanup_pinned = gc.cleanup_jit_pinned_regions(regions));
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::unregister_block_stamp_slot(stamped);

        if g1_peer_pins_skip_post_block_regions()
            && cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled()
        {
            assert!(pinned.contains(&early_region), "a pre-block region stays pinned");
            assert!(!pinned.contains(&late_region), "a post-block region is not pinned");
            assert_eq!(left, vec![early_addr], "the fold never sees the stale word");
            assert!(
                pinned_unstamped.contains(&late_region),
                "a peer with no stamp keeps every pin"
            );
        }
        if g1_cleanup_ignores_peer_stack_words() {
            assert!(
                !cleanup_pinned.contains(&late_region),
                "cleanup frees; a peer's word holds nothing there"
            );
        }
    }

    /// g1pin: under `g1_pins_peer_stack_objects` a captured word pins the
    /// object it lies in (here an interior word), not its region: the pause
    /// keeps that object in place, still evacuates its region neighbour, and
    /// keeps the region young.
    #[test]
    pub fn g1pin_blocked_peer_stack_word_pins_only_its_object() {
        let gc = G1Collector::new(small_config());
        let pinned_obj = gc.alloc_object(ClassId::new(1), 2);
        let neighbour = gc.alloc_object(ClassId::new(1), 1);
        let pinned_addr = pinned_obj.as_ptr() as usize;
        let neighbour_addr = neighbour.as_ptr() as usize;
        let region = gc.lookup_region_for_addr(pinned_addr).unwrap();
        assert_eq!(gc.lookup_region_for_addr(neighbour_addr), Some(region));

        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000, pinned_addr + 16);
        let region_pinned = {
            let regions = gc.regions.read();
            gc.pause_pinned_region_set(&regions, &[]).contains(&region)
        };
        let mut roots = vec![neighbour];
        gc.young_collection(&mut roots, &NoopMonitors);
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        gc.clear_peer_object_pins();

        if cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled()
        {
            assert!(!region_pinned, "the word pins its object, not the region");
            assert_ne!(roots[0].as_ptr() as usize, neighbour_addr, "the neighbour moved");
            let regions = gc.regions.read();
            assert_eq!(
                regions[region].region_type(),
                RegionType::Survivor,
                "a region kept only for a pin stays young, so the next pause reclaims its dead"
            );
            // SAFETY: the pinned object stayed at its address in a kept region.
            let header = unsafe { &*(pinned_addr as *const ObjectHeader) };
            assert!(!ObjectHeader::is_forwarded_mark(header.mark_word.load(Ordering::Relaxed)));
        }
    }

    /// g1pin: a word the region walk cannot place (here one above the cursor,
    /// where no object was ever allocated) still pins its whole region, and
    /// pins no object.
    #[test]
    pub fn g1pin_an_unplaceable_word_keeps_the_region_pin() {
        let gc = G1Collector::new(small_config());
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let addr = obj.as_ptr() as usize;
        let region = gc.lookup_region_for_addr(addr).unwrap();
        let above = {
            let regions = gc.regions.read();
            regions[region].data.as_ptr() as usize + regions[region].cursor() + 64
        };
        assert_eq!(gc.lookup_region_for_addr(above), Some(region));

        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000, above);
        let region_pinned = {
            let regions = gc.regions.read();
            gc.pinned_region_set_with_pin_screen(&regions, &[], false, true)
                .contains(&region)
        };
        let pins = gc.peer_object_pins_snapshot();
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        gc.clear_peer_object_pins();

        if cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled() {
            assert!(region_pinned, "an unplaceable word pins its region");
            assert!(pins.is_empty(), "and no object");
        }
    }

    /// g1pin: a region kept only for a pin keeps only the pinned object; the
    /// rest of it (the neighbour that moved, the one that died) is filler, as
    /// if the pause had freed it.
    #[test]
    pub fn g1pin_a_region_kept_for_a_pin_holds_only_the_pinned_object() {
        let gc = G1Collector::new(small_config());
        let dead_before = gc.alloc_object(ClassId::new(1), 1).as_ptr() as usize;
        let pinned = gc.alloc_object(ClassId::new(1), 2).as_ptr() as usize;
        let live = gc.alloc_object(ClassId::new(1), 1);
        let dead_after = gc.alloc_object(ClassId::new(1), 3).as_ptr() as usize;
        let region = gc.lookup_region_for_addr(pinned).unwrap();
        assert!([dead_before, live.as_ptr() as usize, dead_after]
            .iter()
            .all(|&a| gc.lookup_region_for_addr(a) == Some(region)));

        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000, pinned);
        let mut roots = vec![live];
        gc.young_collection(&mut roots, &NoopMonitors);
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        gc.clear_peer_object_pins();

        if cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled()
        {
            let regions = gc.regions.read();
            assert_eq!(regions[region].region_type(), RegionType::Survivor);
            let (base, cursor) = (regions[region].data.as_ptr() as usize, regions[region].cursor());
            let mut objects = Vec::new();
            let mut offset = 0;
            while offset < cursor {
                let addr = base + offset;
                match peer_walk_step(addr, cursor - offset, &[]) {
                    PeerWalkStep::Object(len) => {
                        objects.push(addr);
                        offset += len;
                    }
                    PeerWalkStep::NoObject(len) => offset += len,
                    PeerWalkStep::Stop => panic!("the kept region does not parse at {addr:#x}"),
                }
            }
            assert_eq!(objects, vec![pinned], "only the pinned object is left");
        }
    }

    /// g1pin: the fill around a region's pins keeps a pinned object at the
    /// region's very first byte.
    #[test]
    pub fn g1pin_a_pin_at_the_region_base_survives_the_fill() {
        let gc = G1Collector::new(small_config());
        let pinned = gc.alloc_object(ClassId::new(1), 2).as_ptr() as usize;
        let dead = gc.alloc_object(ClassId::new(1), 1).as_ptr() as usize;
        let region = gc.lookup_region_for_addr(pinned).unwrap();
        let base = gc.regions.read()[region].data.as_ptr() as usize;
        assert_eq!(pinned, base, "the fixture needs the pin at the region base");
        assert_eq!(gc.lookup_region_for_addr(dead), Some(region));

        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000, pinned);
        let mut roots: Vec<ObjectRef> = Vec::new();
        gc.young_collection(&mut roots, &NoopMonitors);
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        gc.clear_peer_object_pins();

        if cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled()
        {
            let regions = gc.regions.read();
            let cursor = regions[region].cursor();
            match peer_walk_step(pinned, cursor, &[]) {
                PeerWalkStep::Object(len) => assert_eq!(len, dead - pinned, "still the pinned object"),
                _ => panic!("the pinned object at the region base was filled over"),
            }
            assert!(matches!(peer_walk_step(dead, cursor - (dead - pinned), &[]), PeerWalkStep::NoObject(_)));
        }
    }

    /// g1pin: the region-scoped forward visits answer what a filtered full
    /// iteration answers, in the sorted and the hashed form: inside a set of
    /// regions, and outside one (keys outside the arena included).
    #[test]
    pub fn g1pin_region_scoped_forward_visits_match_a_full_filter() {
        let gc = G1Collector::new(small_config());
        let rs = gc.config.region_size;
        let span = |i: usize| gc.arena_base + i * rs;
        let keys: Vec<usize> = vec![
            8,
            span(0),
            span(0) + 64,
            span(1) + 8,
            span(2),
            span(2) + rs - 8,
            span(3) + 16,
            gc.arena_end + 64,
        ];
        let pairs: Vec<(usize, usize)> = keys.iter().map(|&k| (k, k + 1)).collect();
        let sorted = cratonvm_types::PointerMap::from_pairs_sorted(pairs.clone());
        let mut hashed = cratonvm_types::PointerMap::new();
        for &(k, v) in &pairs {
            hashed.insert(k, v);
        }
        let set: RegionSet = [0usize, 2].into_iter().collect();
        let region_of = |k: usize| gc.lookup_region_for_addr(k);
        let want_in: Vec<usize> =
            keys.iter().copied().filter(|&k| region_of(k).is_some_and(|i| set.contains(&i))).collect();
        let want_out: Vec<usize> =
            keys.iter().copied().filter(|&k| !region_of(k).is_some_and(|i| set.contains(&i))).collect();
        for map in [&sorted, &hashed] {
            let mut got_in = Vec::new();
            gc.for_each_forward_in_regions(map, &set, |k, v| {
                assert_eq!(v, k + 1);
                got_in.push(k)
            });
            let mut got_out = Vec::new();
            gc.for_each_forward_outside_regions(map, &set, |k, _| got_out.push(k));
            got_in.sort_unstable();
            got_out.sort_unstable();
            assert_eq!(got_in, want_in, "inside, sorted_form={}", map.is_sorted_form());
            assert_eq!(got_out, want_out, "outside, sorted_form={}", map.is_sorted_form());
        }
    }

    /// g1pin: a peer-stack pin is not an evacuation failure for the fix-up
    /// skip either. A surviving holder that references the pinned object, and
    /// the pinned object's own child, which the serial self-forward drain
    /// copies, leave the closure's to-space edges complete, so Phase 4 does
    /// not walk the fresh Survivor region again.
    #[test]
    pub fn g1pin_a_pinned_referent_leaves_the_closure_edges_trusted() {
        let gc = G1Collector::new(small_config());
        let slot = |arr: usize| (arr + ARRAY_DATA_OFFSET) as *mut u64;
        let child = gc.alloc_object(ClassId::new(1), 1).as_ptr() as usize;
        let pinned = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 1);
        let holder = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 1);
        let pinned_addr = pinned.as_ptr() as usize;
        // SAFETY: both are one-element reference arrays this test allocated.
        unsafe {
            std::ptr::write(slot(pinned_addr), child as u64);
            std::ptr::write(slot(holder.as_ptr() as usize), pinned_addr as u64);
        }

        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000, pinned_addr);
        let refused = || gc.phase_totals.fixup_closure_refused.load(Ordering::Relaxed);
        let before = refused();
        let mut roots = vec![holder];
        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        gc.clear_peer_object_pins();

        if cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled()
        {
            assert_ne!(roots[0].as_ptr(), holder.as_ptr(), "the holder survived by copy");
            // SAFETY: the holder's copy is a live one-element reference array.
            let held = unsafe { std::ptr::read(slot(roots[0].as_ptr() as usize)) } as usize;
            assert_eq!(held, pinned_addr, "the pinned referent stayed in place");
            assert_eq!(refused(), before, "the pin did not refuse the closure's edges");
        }
    }

    /// g1pin: the walk resolves words deep in a region: a word at an object's
    /// exact start pins that object (not the one ending there), an interior
    /// word pins the object around it, a minimum-size object is pinnable, and
    /// no other object is pinned.
    #[test]
    pub fn g1pin_the_walk_pins_exactly_the_objects_the_words_lie_in() {
        let gc = G1Collector::new(small_config());
        let objs: Vec<usize> = [3, 1, 0, 2, 4, 2]
            .iter()
            .map(|&n| gc.alloc_object(ClassId::new(1), n).as_ptr() as usize)
            .collect();
        let region = gc.lookup_region_for_addr(objs[0]).unwrap();
        assert!(objs.iter().all(|&a| gc.lookup_region_for_addr(a) == Some(region)));
        assert_eq!(objs[3] - objs[2], cratonvm_types::MIN_OBJECT_SIZE, "objs[2] is minimum-size");

        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000, objs[2]);
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1008, objs[4] + 24);
        let mut set = RegionSet::new();
        let declined = {
            let regions = gc.regions.read();
            gc.pin_peer_stack_objects(&regions, false, &mut set)
        };
        let pins = gc.peer_object_pins_snapshot();
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        gc.clear_peer_object_pins();

        if cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled() {
            assert_eq!(declined, 0);
            assert!(!set.contains(&region), "every word was placed");
            assert_eq!(pins, vec![objs[2], objs[4]]);
        }
    }

    /// ngr2: on the region-pin path a peer's words pin their region only when
    /// one of them is an object's start. An interior word, an unaligned word, a
    /// word in a filler, a word in a humongous continuation region and a word
    /// past the last object start name no start; an exact start, and a humongous
    /// span's base, do.
    #[test]
    pub fn ngr2_peer_words_pin_a_region_only_through_an_object_start() {
        let gc = G1Collector::new(small_config());
        let a = gc.alloc_object(ClassId::new(1), 2).as_ptr() as usize;
        let filled = gc.alloc_object(ClassId::new(1), 2).as_ptr() as usize;
        let b = gc.alloc_object(ClassId::new(1), 4).as_ptr() as usize;
        let region = gc.lookup_region_for_addr(a).unwrap();
        assert!([filled, b].iter().all(|&x| gc.lookup_region_for_addr(x) == Some(region)));
        // SAFETY: `filled` is a whole object this test allocated.
        unsafe { cratonvm_gc_core::heap::write_filler(filled, b - filled) };
        let big = gc
            .alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000)
            .as_ptr() as usize;
        let big_region = gc.lookup_region_for_addr(big).unwrap();
        let continuation = big + gc.config.region_size;
        let continuation_region = gc.lookup_region_for_addr(continuation).unwrap();

        let names = |idx: usize, words: &[usize]| {
            let mut words = words.to_vec();
            words.sort_unstable();
            let regions = gc.regions.read();
            peer_words_name_an_object_start(&regions, idx, &words, &[])
        };
        assert!(!names(region, &[a + 8, b + 3, filled]), "interior, unaligned, filler");
        assert!(!names(region, &[b + 16]), "inside the last object");
        assert!(names(region, &[a + 8, b]), "an exact start");
        assert!(names(region, &[a]), "the region's first object");
        assert!(names(big_region, &[big]), "a humongous span's base");
        assert!(!names(big_region, &[big + 64]), "inside a humongous object");
        assert!(!names(continuation_region, &[continuation]), "a continuation region");

        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000, a + 8);
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1008, continuation);
        let interior_only = gc.jit_pinned_region_set();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1010, b);
        let with_start = gc.jit_pinned_region_set();
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();

        if cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled()
        {
            assert!(!interior_only.contains(&region), "no start, no pin");
            assert!(!interior_only.contains(&continuation_region));
            assert!(with_start.contains(&region), "a start pins its region");
        }
    }

    /// g1pin: a word in a filler names no object, so it pins nothing (and does
    /// not stop a word beside it from pinning its object); a word in an object
    /// the region's cursor cuts short cannot be placed, so its region is
    /// pinned instead.
    #[test]
    pub fn g1pin_a_filler_word_pins_nothing_and_a_cut_object_pins_its_region() {
        let gc = G1Collector::new(small_config());
        let a = gc.alloc_object(ClassId::new(1), 2).as_ptr() as usize;
        let filled = gc.alloc_object(ClassId::new(1), 2).as_ptr() as usize;
        let cut = gc.alloc_object(ClassId::new(1), 4).as_ptr() as usize;
        let region = gc.lookup_region_for_addr(a).unwrap();
        // SAFETY: `filled` is a whole 24-byte object this test allocated.
        unsafe { cratonvm_gc_core::heap::write_filler(filled, cut - filled) };
        let resolve = |words: &[usize]| {
            cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
            for (i, &word) in words.iter().enumerate() {
                cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000 + 8 * i, word);
            }
            let mut set = RegionSet::new();
            let regions = gc.regions.read();
            gc.pin_peer_stack_objects(&regions, false, &mut set);
            cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
            let pins = gc.peer_object_pins_snapshot();
            gc.clear_peer_object_pins();
            (set.contains(&region), pins)
        };
        let in_filler = resolve(&[filled + 8]);
        let beside = resolve(&[filled + 8, a + 8]);
        let reason = |word: usize| {
            let regions = gc.regions.read();
            gc.peer_words_to_object_starts(&regions, region, &[word], &[]).err()
        };
        let filler_reason = reason(filled + 8);
        {
            let regions = gc.regions.read();
            let base = regions[region].data.as_ptr() as usize;
            regions[region].set_cursor(cut - base + 16);
        }
        let in_cut = resolve(&[cut + 8]);
        let cut_reason = reason(cut + 8);

        if cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled() {
            assert_eq!(in_filler, (false, vec![]), "a filler word pins nothing");
            assert_eq!(beside, (false, vec![a]), "nor stops its neighbour's object pin");
            assert_eq!(in_cut, (true, vec![]), "an object past the cursor pins its region");
            assert_eq!(filler_reason, None, "a filler word is no fallback");
            assert_eq!(cut_reason, Some(PeerWordFallback::Unparseable));
        }
    }

    /// g1pin: a word the above-cursor screen declines pins nothing and is
    /// counted.
    #[test]
    pub fn g1pin_the_above_cursor_screen_declines_and_counts_a_word() {
        let gc = G1Collector::new(small_config());
        let addr = gc.alloc_object(ClassId::new(1), 1).as_ptr() as usize;
        let region = gc.lookup_region_for_addr(addr).unwrap();
        let above = {
            let regions = gc.regions.read();
            regions[region].data.as_ptr() as usize + regions[region].cursor() + 64
        };
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000, above);
        let mut set = RegionSet::new();
        let declined = {
            let regions = gc.regions.read();
            gc.pin_peer_stack_objects(&regions, true, &mut set)
        };
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        gc.clear_peer_object_pins();

        if cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled() {
            assert_eq!(declined, 1);
            assert!(!set.contains(&region));
        }
    }

    /// g1pin: a word in an Old region pins its object too, not the region. A
    /// mixed pause may evacuate the region; a young pause leaves it alone, and
    /// a region pin there only bought a wholesale source walk.
    #[test]
    pub fn g1pin_a_word_in_an_old_region_pins_its_object() {
        let gc = G1Collector::new(small_config());
        let obj = gc.alloc_object(ClassId::new(1), 2);
        let addr = obj.as_ptr() as usize;
        let region = gc.lookup_region_for_addr(addr).unwrap();
        gc.regions.write()[region].set_region_type(RegionType::Old);

        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000, addr + 8);
        let region_pinned = {
            let regions = gc.regions.read();
            gc.pause_pinned_region_set(&regions, &[]).contains(&region)
        };
        let object_pinned = gc.peer_object_pinned(addr);
        cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
        gc.clear_peer_object_pins();

        if cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled()
        {
            assert!(!region_pinned, "the word pins its object, not the Old region");
            assert!(object_pinned, "the object the word lies in is pinned");
        }
    }

    /// gci i16/nc -- `CleanupFreedTargets::note` takes every source of a freed
    /// region's rset, and a coarsened rset widens the walk to every region.
    #[test]
    pub fn gci_i16_nc_freed_targets_take_every_source_and_the_coarsened_bit() {
        let rset = RememberedSet::default();
        rset.add_reference(3);
        rset.add_reference(5);
        let mut t = CleanupFreedTargets::default();
        t.note(1, &rset);
        assert!(t.freed.contains(&1));
        assert!(t.sources.contains(&3) && t.sources.contains(&5));
        assert!(!t.sources.contains(&1));
        assert!(!t.coarsened);
        rset.coarsen();
        t.note(2, &rset);
        assert!(t.freed.contains(&2));
        assert!(t.coarsened, "a coarsened rset names every region");
    }

    /// gci i16/nc -- the vi7-g1y net-of-pins shape, humongous half. A holder at
    /// or above its region's TAMS (here: allocated after the snapshot; in the
    /// field: the evacuated copy of an object that was already unreachable at
    /// the snapshot) names a dead humongous span. The bitmap scrub cannot see
    /// the holder, cleanup reclaims the span, and before the fix the holder
    /// kept a slot into the freed regions for the next pause's whole-region
    /// source walk to follow. The slot into the KEPT span is untouched.
    #[test]
    pub fn gci_i16_nc_cleanup_nulls_an_above_tams_slot_into_a_reclaimed_humongous_span() {
        let gc = make_collector();
        let live = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        let dead = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        assert!(gc.is_humongous(live) && gc.is_humongous(dead));
        gc.set_array_element(live, 7, Value::Long(0x5eed)).unwrap();

        gc.start_concurrent_mark(&stw());
        let holder = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
        gc.set_array_element(holder, 0, Value::Object(Some(dead)))
            .unwrap();
        gc.set_array_element(holder, 1, Value::Object(Some(live)))
            .unwrap();
        let holder_idx = gc
            .lookup_region_for_addr(holder.as_ptr() as usize)
            .expect("holder in a region");
        let dead_idx = gc
            .lookup_region_for_addr(dead.as_ptr() as usize)
            .expect("span in a region");
        {
            let regions = gc.regions.read();
            assert!(
                regions[dead_idx].rset.names_source(holder_idx),
                "fixture: the barrier recorded the holder's region in the dead span's rset"
            );
            let holder_off =
                holder.as_ptr() as usize - regions[holder_idx].data.as_ptr() as usize;
            assert!(
                holder_off >= regions[holder_idx].tams(),
                "fixture: the holder is at or above TAMS, so the bitmap scrub skips it"
            );
        }
        gc.remark(&stw(), &[live]);
        while !gc.concurrent_mark_step(usize::MAX) {}
        let slots_before = gc.scrub_counters.slots.load(Ordering::Relaxed);
        gc.cleanup(&stw());

        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            1,
            "fixture: the dead span was reclaimed"
        );
        assert!(gc.is_humongous(live));
        if gc.cleanup_scrub_freed_targets {
            assert_eq!(
                gc.get_array_element(holder, 0).unwrap(),
                Value::Object(None),
                "a slot into a span cleanup reclaimed must not survive cleanup"
            );
            assert!(gc.scrub_counters.slots.load(Ordering::Relaxed) > slots_before);
        }
        assert!(
            matches!(
                gc.get_array_element(holder, 1).unwrap(),
                Value::Object(Some(r)) if r.as_ptr() == live.as_ptr()
            ),
            "a slot into a kept span is untouched"
        );
        assert_eq!(gc.get_array_element(live, 7).unwrap(), Value::Long(0x5eed));
    }

    /// gci i16/nc -- the same shape for an Old region cleanup frees IN PLACE,
    /// then the pauses that follow: the freed region is reused by live arrays
    /// (the stale address lands INSIDE one of them, as in the vi7-g1y np rows,
    /// where an `int[]` of ascending positions was read as a header) and a
    /// young and a mixed pause run with the holder in the collection set. The
    /// holder's slot is null after cleanup, so neither pause follows it, and
    /// every live array keeps its contents.
    #[test]
    pub fn gci_i16_nc_cleanup_nulls_an_above_tams_slot_into_an_old_region_it_frees_in_place() {
        let cfg = G1CollectorConfig {
            promotion_age: 1,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);
        // Promoted together, so `target` sits past the start of its region.
        let first = gc.alloc_object(ClassId::new(1), 1);
        let target = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(target, 0, Value::Int(17));
        let mut roots = vec![first, target];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        let target = roots[1];
        let r_idx = {
            let regions = gc.regions.read();
            let idx = gc.region_for_ptr(&regions, target.as_ptr()).unwrap();
            assert_eq!(regions[idx].region_type(), RegionType::Old);
            idx
        };
        roots.clear();

        gc.start_concurrent_mark(&stw());
        let holder = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(holder, 0, Value::Object(Some(target)));
        gc.remark(&stw(), &[]);
        while !gc.concurrent_mark_step(usize::MAX) {}
        gc.cleanup(&stw());
        {
            let regions = gc.regions.read();
            assert_eq!(
                regions[r_idx].region_type(),
                RegionType::Free,
                "fixture: the wholly dead Old region is freed in place"
            );
        }
        let scrub = gc.cleanup_scrub_freed_targets;
        if scrub {
            assert_eq!(
                gc.get_field(holder, 0),
                Value::Object(None),
                "a slot into a region cleanup freed in place must not survive cleanup"
            );
        }

        // Reuse the freed region with live arrays until one covers the stale
        // address. Arrays placed elsewhere are dropped (garbage).
        let stale = target.as_ptr() as usize;
        const LEN: usize = 64;
        let mut live_arrays: Vec<ObjectRef> = Vec::new();
        let mut covered = false;
        // At most ~4.3 MB of 272-byte arrays: room in the 8 MB fixture heap,
        // which holds only the holder's region once cleanup has run.
        for _ in 0..16_000 {
            let a = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, LEN);
            let start = a.as_ptr() as usize;
            if gc.lookup_region_for_addr(start) != Some(r_idx) {
                continue;
            }
            for i in 0..LEN {
                gc.set_array_element(a, i, Value::Int(0x1000 + i as i32))
                    .unwrap();
            }
            live_arrays.push(a);
            if start < stale && stale < start + ARRAY_DATA_OFFSET + LEN * 4 {
                covered = true;
                break;
            }
        }
        let mut roots = vec![holder];
        roots.extend(live_arrays.iter().copied());
        let _ = gc.young_collection(&mut roots, &NoopMonitors);
        let _ = gc.mixed_collection(&mut roots, &NoopMonitors);
        for (k, &a) in roots.iter().enumerate().skip(1) {
            for i in 0..LEN {
                assert_eq!(
                    gc.get_array_element(a, i).unwrap(),
                    Value::Int(0x1000 + i as i32),
                    "live array {k} (covers the stale address: {covered}) element {i}"
                );
            }
        }
        if scrub {
            assert_eq!(gc.get_field(roots[0], 0), Value::Object(None));
        }
    }

    /// gci i17/nr -- make `obj` (a primitive array) an extent no linear walker
    /// can trust: its length now runs far past its region's cursor. Returns
    /// the real length for [`gci_i17_nr_restore_length`].
    pub fn gci_i17_nr_break_walks_at(obj: ObjectRef) -> u32 {
        let hdr = obj.as_ptr() as *mut ObjectHeader;
        // SAFETY: a test heap, single-threaded; `obj` is a published array.
        unsafe {
            let real = (*hdr).array_length();
            (*hdr).set_array_length(0x0100_0000);
            real
        }
    }

    pub fn gci_i17_nr_restore_length(obj: ObjectRef, len: u32) {
        // SAFETY: as above.
        unsafe { (*(obj.as_ptr() as *mut ObjectHeader)).set_array_length(len) };
    }

    /// gci i17/nr (i16/nc residual R2), humongous half: the freed-target
    /// scrub's walk of the holder's region stops at an untrusted extent BEFORE
    /// the holder, so the holder's slot into the dead span is never examined.
    /// Before the fix cleanup reclaimed the span anyway and the slot named
    /// freed memory; now the span is kept for a cycle and the slot names an
    /// object that still exists.
    #[test]
    pub fn gci_i17_nr_cleanup_keeps_a_span_a_broken_source_walk_may_still_name() {
        let gc = make_collector();
        let dead = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        assert!(gc.is_humongous(dead));
        gc.set_array_element(dead, 3, Value::Long(0x5eed)).unwrap();

        gc.start_concurrent_mark(&stw());
        // Both after the snapshot, so the bitmap scrub never reads them.
        let breaker = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 4);
        let holder = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
        gc.set_array_element(holder, 0, Value::Object(Some(dead)))
            .unwrap();
        let holder_idx = gc
            .lookup_region_for_addr(holder.as_ptr() as usize)
            .expect("holder in a region");
        let dead_idx = gc
            .lookup_region_for_addr(dead.as_ptr() as usize)
            .expect("span in a region");
        assert_eq!(
            gc.lookup_region_for_addr(breaker.as_ptr() as usize),
            Some(holder_idx),
            "fixture: the breaker shares the holder's region"
        );
        assert!(
            (breaker.as_ptr() as usize) < (holder.as_ptr() as usize),
            "fixture: a linear walk meets the breaker first"
        );
        assert!(
            gc.regions.read()[dead_idx].rset.names_source(holder_idx),
            "fixture: the barrier recorded the holder's region in the span's rset"
        );
        gc.remark(&stw(), &[]);
        while !gc.concurrent_mark_step(usize::MAX) {}

        let real = gci_i17_nr_break_walks_at(breaker);
        let retained_before = gc.scrub_counters.retained.load(Ordering::Relaxed);
        gc.cleanup(&stw());
        gci_i17_nr_restore_length(breaker, real);

        if gc.cleanup_scrub_freed_targets {
            assert_eq!(
                gc.count_regions(RegionType::HumongousStart),
                1,
                "a span a source region may still name from behind its break is kept"
            );
            assert!(
                matches!(
                    gc.get_array_element(holder, 0).unwrap(),
                    Value::Object(Some(r)) if r.as_ptr() == dead.as_ptr()
                ),
                "the unexamined slot names an object that still exists"
            );
            assert_eq!(gc.get_array_element(dead, 3).unwrap(), Value::Long(0x5eed));
            assert!(gc.scrub_counters.retained.load(Ordering::Relaxed) > retained_before);
        }
    }

    /// gci i17/nr (R2), in-place half: the same broken walk keeps a wholly
    /// dead Old region in place (not reset, contents intact), while a dead
    /// span no broken source names is still reclaimed in the same cleanup.
    #[test]
    pub fn gci_i17_nr_cleanup_keeps_an_old_region_a_broken_source_walk_may_still_name() {
        let cfg = G1CollectorConfig {
            promotion_age: 1,
            ..small_config()
        };
        let gc = G1Collector::new(cfg);
        let first = gc.alloc_object(ClassId::new(1), 1);
        let target = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(target, 0, Value::Int(17));
        let mut roots = vec![first, target];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        let target = roots[1];
        let r_idx = {
            let regions = gc.regions.read();
            let idx = gc.region_for_ptr(&regions, target.as_ptr()).unwrap();
            assert_eq!(regions[idx].region_type(), RegionType::Old);
            idx
        };
        roots.clear();
        // A dead span nothing names: reclaimed whatever the broken walk does.
        let lone = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        assert!(gc.is_humongous(lone));

        gc.start_concurrent_mark(&stw());
        let breaker = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 4);
        let holder = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(holder, 0, Value::Object(Some(target)));
        let holder_idx = gc
            .lookup_region_for_addr(holder.as_ptr() as usize)
            .expect("holder in a region");
        assert_eq!(
            gc.lookup_region_for_addr(breaker.as_ptr() as usize),
            Some(holder_idx),
            "fixture: the breaker shares the holder's region"
        );
        assert!((breaker.as_ptr() as usize) < (holder.as_ptr() as usize));
        assert!(
            gc.regions.read()[r_idx].rset.names_source(holder_idx),
            "fixture: the barrier recorded the holder's region in the Old region's rset"
        );
        gc.remark(&stw(), &[]);
        while !gc.concurrent_mark_step(usize::MAX) {}

        let real = gci_i17_nr_break_walks_at(breaker);
        gc.cleanup(&stw());
        gci_i17_nr_restore_length(breaker, real);

        if gc.cleanup_scrub_freed_targets {
            let regions = gc.regions.read();
            assert_eq!(
                regions[r_idx].region_type(),
                RegionType::Old,
                "the wholly dead Old region a broken source may still name is kept"
            );
            drop(regions);
            assert_eq!(gc.get_field(target, 0), Value::Int(17), "and not zero-filled");
            assert!(matches!(
                gc.get_field(holder, 0),
                Value::Object(Some(r)) if r.as_ptr() == target.as_ptr()
            ));
            assert_eq!(
                gc.count_regions(RegionType::HumongousStart),
                0,
                "a span whose rset names no broken source is reclaimed as before"
            );
        }
    }

    /// gci i17/nr (i16/nc residual R1): under allocation pressure the
    /// remembered-set source walk steps over a holder a trusted completed mark
    /// proves dead. It used to step over it with its slot into the collection
    /// set intact, so once the young region was freed and reused the slot
    /// named reused memory for the next cycle's pauses (no filter while
    /// marking) to follow. The skipped holder's slots are now nulled.
    #[test]
    pub fn gci_i17_nr_rset_filter_nulls_the_slots_of_a_holder_it_skips_as_proven_dead() {
        let gc = G1Collector::new(many_region_config());
        let (holder, _target, h_idx, t_idx) = lane_b_cross_region_holder(&gc);
        arm_completed_mark_with_dead_old_region(&gc, h_idx);
        {
            let regions = gc.regions.read();
            assert_eq!(
                regions[t_idx].region_type(),
                RegionType::Eden,
                "fixture: the target is young, so the pause collects its region"
            );
            assert!(
                regions[t_idx].rset.names_source(h_idx),
                "fixture: the barrier recorded the holder's region"
            );
            if g1_rset_dead_holder_filter() {
                let dead = gc
                    .low_space_rset_source_liveness(&regions)
                    .expect("fixture: a trusted completed mark");
                assert!(dead.proves_dead(&regions[h_idx], h_idx, holder.as_ptr() as usize));
            }
        }
        gc.note_native_alloc_pressure();
        let before = gc
            .scrub_counters
            .rset_dead_holder_slots_nulled
            .load(Ordering::Relaxed);
        let mut roots: Vec<ObjectRef> = Vec::new();
        let _ = gc.young_collection(&mut roots, &NoopMonitors);
        gc.clear_native_alloc_pressure();
        if g1_rset_dead_holder_filter() {
            assert_eq!(
                gc.get_field(holder, 0),
                Value::Object(None),
                "a holder the walk skips as proven dead keeps no slot into the freed young region"
            );
            assert!(
                gc.scrub_counters
                    .rset_dead_holder_slots_nulled
                    .load(Ordering::Relaxed)
                    > before
            );
        }
    }

    /// gci i18/nr (i17/nr N1): a completed mark proves nothing dead unless the
    /// remark both screened its roots and seeded every JIT pin -- the
    /// condition cleanup's bitmap scrub already asks of a JIT-pinned region.
    #[test]
    pub fn gci_i18_nr_mark_proof_needs_both_jit_pin_switches() {
        assert!(mark_proof_covers_jit_pins(true, true));
        assert!(!mark_proof_covers_jit_pins(false, true));
        assert!(!mark_proof_covers_jit_pins(true, false));
        assert!(!mark_proof_covers_jit_pins(false, false));
    }

    /// gci i18/nr (gci i17/rv finding 3): the freed-target scrub returns at
    /// once for a source region with no dirty card -- no object examined, and
    /// a walk break inside it is not reported, because no object in it can
    /// hold a cross-region slot. Without the card screen, or once a
    /// cross-region store dirties a card in the region, the walk runs and
    /// reports the break as before.
    #[test]
    pub fn gci_i18_nr_freed_target_scrub_skips_a_source_region_with_no_dirty_card() {
        let gc = make_collector();
        let breaker = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 4);
        let holder = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
        let far = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        assert!(gc.is_humongous(far));
        let idx = gc
            .lookup_region_for_addr(breaker.as_ptr() as usize)
            .expect("breaker in a region");
        assert_eq!(
            gc.lookup_region_for_addr(holder.as_ptr() as usize),
            Some(idx),
            "fixture: the holder shares the breaker's region"
        );
        let freed = RegionSet::with_region_capacity(gc.regions.read().len());
        let scrub = |card_screen: bool| -> FreedTargetScrub {
            let regions = gc.regions.read();
            let region = &regions[idx];
            assert!(!region.pinned, "fixture: an unpinned source region");
            // SAFETY: single-threaded test heap, nothing allocates during the
            // walk, and the walk stays below the region's cursor.
            unsafe {
                scrub_slots_into_freed_regions(
                    &gc,
                    region,
                    region.cursor(),
                    &freed,
                    &[],
                    card_screen,
                )
            }
        };
        let real = gci_i17_nr_break_walks_at(breaker);
        {
            let regions = gc.regions.read();
            let base = regions[idx].data.as_ptr() as usize;
            assert!(
                !gc.cards().any_dirty_in(base, regions[idx].cursor()),
                "fixture: no cross-region store yet, so no dirty card"
            );
        }
        let clean = scrub(true);
        assert_eq!(clean.sources, 1);
        assert_eq!(clean.objects, 0, "a clean source region is not walked");
        assert_eq!(clean.breaks, 0, "and reports no break");
        let unscreened = scrub(false);
        assert_eq!(unscreened.breaks, 1, "fixture: the walk itself breaks");

        gci_i17_nr_restore_length(breaker, real);
        gc.set_array_element(holder, 0, Value::Object(Some(far)))
            .unwrap();
        let real = gci_i17_nr_break_walks_at(breaker);
        let dirty = scrub(true);
        gci_i17_nr_restore_length(breaker, real);
        assert_eq!(
            dirty.breaks, 1,
            "a region with a dirty card is walked, and its break reported"
        );
        assert!(matches!(
            gc.get_array_element(holder, 0).unwrap(),
            Value::Object(Some(r)) if r.as_ptr() == far.as_ptr()
        ));
    }

    /// gci i18/nr (gci i17/rv finding 11): the freed-target scrub's counters
    /// and switch are per heap -- a cleanup on one collector moves its own
    /// counters, never another collector's.
    #[test]
    pub fn gci_i18_nr_scrub_counters_are_per_heap() {
        let a = make_collector();
        let b = make_collector();
        let dead = a.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        assert!(a.is_humongous(dead));
        a.start_concurrent_mark(&stw());
        let holder = a.alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
        a.set_array_element(holder, 0, Value::Object(Some(dead)))
            .unwrap();
        a.remark(&stw(), &[]);
        while !a.concurrent_mark_step(usize::MAX) {}
        a.cleanup(&stw());
        assert_eq!(
            a.cleanup_scrub_freed_targets,
            b.cleanup_scrub_freed_targets,
            "both read the same switch"
        );
        if a.cleanup_scrub_freed_targets {
            assert_eq!(a.cleanup_scrub_counters().runs.load(Ordering::Relaxed), 1);
            assert!(a.cleanup_scrub_counters().slots.load(Ordering::Relaxed) >= 1);
        }
        let bc = b.cleanup_scrub_counters();
        for (name, c) in [
            ("runs", &bc.runs),
            ("sources", &bc.sources),
            ("holders", &bc.holders),
            ("slots", &bc.slots),
            ("walk_breaks", &bc.walk_breaks),
            ("retained", &bc.retained),
            ("evac_candidate_flagless", &bc.evac_candidate_flagless),
            ("rset_dead_holder_slots_nulled", &bc.rset_dead_holder_slots_nulled),
        ] {
            assert_eq!(c.load(Ordering::Relaxed), 0, "{name}: another heap's counter moved");
        }
    }

    /// gci i18/nr (vi17-nr R1 fallout): the freed-target scrub counts a holder
    /// it nulls whose mark bit is set (an under-mark lead), and does not count
    /// an unmarked one. Both slots are still nulled (no behaviour change).
    #[test]
    pub fn gci_i18_nr_freed_target_scrub_counts_a_marked_holder() {
        let gc = make_collector();
        let target = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        assert!(gc.is_humongous(target));
        let marked = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
        let unmarked = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
        gc.set_array_element(marked, 0, Value::Object(Some(target)))
            .unwrap();
        gc.set_array_element(unmarked, 0, Value::Object(Some(target)))
            .unwrap();
        let idx = gc
            .lookup_region_for_addr(marked.as_ptr() as usize)
            .expect("holder in a region");
        assert_eq!(
            gc.lookup_region_for_addr(unmarked.as_ptr() as usize),
            Some(idx),
            "fixture: both holders share a region"
        );
        let t_idx = gc
            .lookup_region_for_addr(target.as_ptr() as usize)
            .expect("span in a region");
        let mut freed = RegionSet::with_region_capacity(gc.regions.read().len());
        freed.insert(t_idx);
        let out = {
            let regions = gc.regions.read();
            let region = &regions[idx];
            assert!(region.mark_bitmap.try_mark(marked.as_ptr() as usize));
            // SAFETY: single-threaded test heap; the walk stays below the cursor.
            unsafe {
                scrub_slots_into_freed_regions(&gc, region, region.cursor(), &freed, &[], false)
            }
        };
        assert_eq!(out.holders, 2, "both holders' slots are nulled");
        assert_eq!(out.marked_holders, 1, "only the marked one is counted");
        assert_eq!(gc.get_array_element(marked, 0).unwrap(), Value::Object(None));
        assert_eq!(gc.get_array_element(unmarked, 0).unwrap(), Value::Object(None));
    }

    /// gci i18/nr (vi17-nr R1 fallout): a cleanup that takes the
    /// retain-everything fail-safe is counted per heap.
    #[test]
    pub fn gci_i18_nr_cleanup_fail_safe_is_counted() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let _ = retype_region_of(&gc, obj.as_ptr() as usize, RegionType::Old);
        gc.start_concurrent_mark(&stw());
        gc.mark_worklist.lock().push(obj.as_ptr() as usize);
        assert_eq!(gc.cleanup_scrub_counters().retain_all_cycles.load(Ordering::Relaxed), 0);
        gc.cleanup(&stw());
        assert_eq!(
            gc.cleanup_scrub_counters().retain_all_cycles.load(Ordering::Relaxed),
            1,
            "an undrained gray set takes the fail-safe, and it is counted"
        );
    }

    #[test]
    pub fn cleanup_scrubs_dead_holders_sharing_a_jit_pinned_region() {
        for source_type in [RegionType::Eden, RegionType::Survivor, RegionType::Old] {
            let gc = make_collector();
            let dead_holder = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
            let live = gc.alloc_object(ClassId::new(9), 1);
            gc.set_field(live, 0, Value::Int(42));
            let source_idx = gc.lookup_region_for_addr(live.as_ptr() as usize).unwrap();
            assert_eq!(
                gc.lookup_region_for_addr(dead_holder.as_ptr() as usize),
                Some(source_idx)
            );
            gc.with_regions_mut(|regions| regions[source_idx].set_region_type(source_type));
            let dead_target = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
            gc.set_array_element(dead_holder, 0, Value::Object(Some(dead_target)))
                .unwrap();

            cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
            cratonvm_gc_core::gc_quiescence::clear_pinned_jit_roots();
            cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[live.as_ptr() as usize]);
            cratonvm_gc_core::gc_quiescence::enter();
            gc.start_concurrent_mark(&stw());
            gc.remark(&stw(), &[]);
            while !gc.concurrent_mark_step(usize::MAX) {}
            let mut pinned = false;
            gc.with_regions_mut(|regions| {
                pinned = gc.cleanup_jit_pinned_regions(regions).contains(&source_idx);
            });
            gc.cleanup(&stw());
            cratonvm_gc_core::gc_quiescence::leave();
            cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[]);

            if g1_mark_root_screen()
                && g1_cleanup_scrub()
            {
                assert!(
                    pinned,
                    "fixture: final remark traced a pin in the source region"
                );
                assert_eq!(gc.get_field(live, 0), Value::Int(42));
                assert_eq!(
                    gc.get_array_element(dead_holder, 0).unwrap(),
                    Value::Object(None),
                    "a dead holder must not retain an edge into the humongous span cleanup freed"
                );
                assert_eq!(gc.count_regions(RegionType::HumongousStart), 0);
            }
        }
    }

    /// r12w1 g1store (proposal W19-1, page
    /// `r11w19-g1store-band-root-rejects-drop-derived-pointer-pins`): a band
    /// word the root screen REJECTS still owes its pin, and
    /// `band_reject_pin` names it. For a humongous interior word, in the
    /// start region or a continuation, the pin is the span BASE, so it lands
    /// where an accepted base root would. End to end: a humongous span that a
    /// compiled frame names only through an interior word survives a pause
    /// that would otherwise eagerly reclaim it.
    #[test]
    pub fn r12w1_a_rejected_humongous_interior_word_pins_its_span() {
        let gc = make_collector();
        let h = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        assert_eq!(gc.count_regions(RegionType::HumongousContinuation), 1);
        let base = h.as_ptr() as usize;
        let start_region = gc
            .lookup_region_for_addr(base)
            .expect("the span is in a region");
        // An element cursor in the start region, and one 1.25 MiB in (the
        // continuation, with 1 MiB regions).
        let start_word = base + 64;
        let cont_word = base + (5 << 18);
        for w in [start_word, cont_word] {
            assert!(
                gc.is_object_address_for_root_scan(w).is_none(),
                "precondition: the root screen rejects a humongous interior word"
            );
            let pin = gc
                .band_reject_pin(w)
                .expect("a live humongous word owes a pin");
            assert!(pin.humongous);
            assert_eq!(pin.addr, base, "the pin names the span base");
            assert_eq!(pin.region, start_region);
        }
        // An Eden interior word pins its own region, by the raw word.
        let small = gc.alloc_array(ClassId::new(9), ArrayElementType::Long, 8);
        let interior = small.as_ptr() as usize + HEADER_SIZE + 8;
        let pin = gc.band_reject_pin(interior).expect("a live Eden word");
        assert!(!pin.humongous);
        assert_eq!(pin.addr, interior);
        assert_eq!(Some(pin.region), gc.lookup_region_for_addr(interior));
        // Outside the arena there is nothing to pin.
        assert!(gc.band_reject_pin(0x1000).is_none());

        // Published as the root gatherer does, with a compiled frame live.
        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        cratonvm_gc_core::gc_quiescence::clear_pinned_jit_roots();
        let owed = gc.band_reject_pin(cont_word).expect("owed pin").addr;
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[owed]);
        cratonvm_gc_core::gc_quiescence::enter();
        let mut roots: Vec<ObjectRef> = vec![];
        gc.young_collection(&mut roots, &NoopMonitors);
        cratonvm_gc_core::gc_quiescence::leave();
        cratonvm_gc_core::gc_quiescence::publish_pinned_jit_roots(&[]);
        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            1,
            "a span named only through an interior word must not be eagerly reclaimed"
        );
    }

    /// ...and the retirement must not destroy the QUARTET. `kind` and
    /// `element_type` live in the mark word, and every linear region walker
    /// sizes a from-space object from them while Phase 5 has not yet zeroed the
    /// region. Storing a bare `MARK_NEUTRAL` would leave an abandoned array
    /// claiming to be a zero-slot plain object, and the walk would desynchronize.
    #[test]
    pub fn retiring_a_forward_preserves_the_kind_quartet() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(9), ArrayElementType::Long, 4);
        let addr = arr.as_ptr() as usize;

        let before = {
            let header = unsafe { &*(addr as *const ObjectHeader) };
            (
                header.kind(),
                header.element_type(),
                object_total_size(header),
            )
        };

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(addr, addr);
        gc.retire_forwards(&map, &RegionSet::default());

        let header = unsafe { &*(addr as *const ObjectHeader) };
        assert!(
            !header.is_forwarded(),
            "the tag must be gone — that is the whole job"
        );
        assert_eq!(
            (
                header.kind(),
                header.element_type(),
                object_total_size(header)
            ),
            before,
            "but the quartet must survive, or a linear walk over the abandoned \
             body desynchronizes"
        );
    }

    /// F-02 — the Phase-4 screen is equivalent, not an approximation: it
    /// answers for a collection-set resident and declines for everything else,
    /// which is sound only because every key of a pause's forwarding map IS a
    /// collection-set resident.
    #[test]
    pub fn the_phase4_forward_lookup_screens_on_collection_set_membership() {
        let gc = make_collector();
        let a = gc.alloc_object(ClassId::new(1), 0);
        let b = gc.alloc_object(ClassId::new(2), 0);
        let (a_addr, b_addr) = (a.as_ptr() as usize, b.as_ptr() as usize);

        let a_region = gc.lookup_region_for_addr(a_addr).unwrap();
        let mut cset = RegionSet::new();
        cset.insert(a_region);

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(a_addr, 0xdead_0000);
        // A key outside the collection set cannot occur in production; putting
        // one here is how the screen's behaviour becomes observable.
        map.insert(b_addr, 0xbeef_0000);

        let regions_for_lookup = gc.regions.read();
        let lookup = ForwardLookup {
            collector: &gc,
            cset: &cset,
            map: &map,
            regions: RegionView::Slice(&regions_for_lookup),
        };
        assert_eq!(
            lookup.resolve(a_addr),
            Some(0xdead_0000),
            "a collection-set resident with a forward resolves"
        );
        assert_eq!(
            lookup.resolve(b_addr),
            if gc.lookup_region_for_addr(b_addr) == Some(a_region) {
                // Both objects landed in one region, so `b` IS in the screened
                // set and the map answers for it. The assertion below is the
                // one that carries the test in that case.
                Some(0xbeef_0000)
            } else {
                None
            },
        );
        assert_eq!(
            lookup.resolve(0x10),
            None,
            "an address outside the heap resolves to nothing rather than hashing"
        );
    }

    // -- F-07: the phase breakdown is a partition --

    #[test]
    pub fn the_phase_breakdown_accounts_for_the_whole_pause() {
        let mut phases = G1PausePhases {
            roots_us: 10,
            rset_us: 20,
            closure_us: 300,
            fixup_us: 40,
            free_us: 5,
            verify_us: 25,
            ..Default::default()
        };
        assert_eq!(phases.measured_us(), 400);
        phases.close(1000);
        assert_eq!(phases.other_us, 600);
        assert_eq!(
            phases.measured_us() + phases.other_us,
            1000,
            "the six phases plus `other` must BE the pause -- a table whose rows \
             do not sum to the total cannot show that a cost was removed rather \
             than moved"
        );
    }

    #[test]
    pub fn a_phase_breakdown_that_overruns_its_pause_reports_no_remainder() {
        // Each phase truncates to whole microseconds against a separately
        // sampled pause clock, so on a short pause the parts can exceed the
        // whole. That must saturate, not wrap.
        let mut phases = G1PausePhases {
            closure_us: 7,
            ..Default::default()
        };
        phases.close(3);
        assert_eq!(phases.other_us, 0);
    }

    // -- Region basics --

    #[test]
    pub fn region_new_is_free() {
        let r = G1Region::new(1024);
        assert_eq!(r.region_type(), RegionType::Free);
        assert_eq!(r.cursor(), 0);
        assert!(!r.pinned);
        assert_eq!(r.age(), 0);
        assert_eq!(r.live_bytes, 0);
    }

    #[test]
    pub fn region_bump_alloc() {
        let mut r = G1Region::new(4096);
        let (ptr, offset) = r.bump_alloc(64, 8, "test").unwrap();
        assert!(!ptr.is_null());
        assert_eq!(offset, 0);
        assert_eq!(r.cursor(), 64);

        let (ptr2, offset2) = r.bump_alloc(128, 8, "test").unwrap();
        assert!(!ptr2.is_null());
        assert_eq!(offset2, 64);
        assert_eq!(r.cursor(), 192);
    }

    /// A TLAB carve must end 8-aligned, because `Tlab::new` rounds its `end`
    /// DOWN to 8 while `bump_alloc` commits the full size to `region.cursor()`.
    /// Any gap between those two is heap no man's land — see
    /// [`tlab_carve_size`] for what a linear walk does when it arrives there.
    ///
    /// `0x11664` is the size actually observed corrupting a walk.
    #[test]
    pub fn tlab_carve_size_never_leaves_an_unaligned_tail() {
        for (req, rem) in [
            (0x11664usize, 1 << 20),
            (0x1d7e4, 1 << 20),
            (0xebf2, 1 << 20),
            (0x8b32, 1 << 20),
            (0x4599, 1 << 20),
            // The cap side must round too: `remaining` is a cursor delta and
            // carries the same mod-8 dregs a request does.
            (1 << 20, 0x207fc),
            (1 << 20, 261),
        ] {
            let actual = tlab_carve_size(req, rem);
            assert_eq!(actual & 7, 0, "req={req:#x} rem={rem:#x} -> {actual:#x}");
            assert!(actual <= req && actual <= rem);
            // Never rounds a usable carve away entirely: both production call
            // sites are guarded by `remaining >= 256`, so the mask can shave at
            // most 7 bytes off something already >= 256.
            assert!(actual > 0, "req={req:#x} rem={rem:#x}");
        }
    }

    /// The exact geometry from the failing run, asserted end to end: with the
    /// mask, the region cursor after a carve is where `Tlab::new` would put the
    /// TLAB's end, so no orphan sliver exists for a walk to trip on.
    #[test]
    pub fn a_masked_carve_leaves_region_cursor_equal_to_the_tlab_end() {
        let mut r = G1Region::new(1 << 20);
        // Put the carve at the observed offset.
        r.bump_alloc(0xf198, 8, "test").unwrap();
        let actual = tlab_carve_size(0x11664, r.remaining());
        let (ptr, off) = r.bump_alloc(actual, 8, "test").unwrap();
        let tlab_end_addr = (ptr as usize + actual) & !7usize; // what Tlab::new keeps
        let region_end_addr = ptr as usize + actual; // what the cursor commits
        assert_eq!(
            tlab_end_addr, region_end_addr,
            "carve at {off:#x} of {actual:#x} leaves [{tlab_end_addr:#x},{region_end_addr:#x}) \
             owned by neither the TLAB nor any object",
        );
        assert_eq!(r.cursor(), off + actual);
        assert_eq!(r.cursor() & 7, 0);
    }

    #[test]
    pub fn region_bump_alloc_full() {
        let mut r = G1Region::new(128);
        assert!(r.bump_alloc(64, 8, "test").is_some());
        assert!(r.bump_alloc(64, 8, "test").is_some());
        assert!(r.bump_alloc(1, 8, "test").is_none()); // full
    }

    #[test]
    pub fn region_remaining() {
        let mut r = G1Region::new(1024);
        assert_eq!(r.remaining(), 1024);
        r.bump_alloc(100, 8, "test");
        assert_eq!(r.remaining(), 924);
    }

    #[test]
    pub fn region_reset() {
        let mut r = G1Region::new(1024);
        r.set_region_type(RegionType::Eden);
        r.set_cursor(500);
        r.live_bytes = 200;
        r.pinned = true;
        r.set_age(5);
        r.reset(0);
        assert_eq!(r.region_type(), RegionType::Free);
        assert_eq!(r.cursor(), 0);
        assert!(!r.pinned);
        assert_eq!(r.age(), 0);
    }

    // -- Collector creation --

    /// `forwarding_target` strips the low TWO bits; `make_forwarded` asserts
    /// the low THREE are clear. A word that is not a forward at all but whose
    /// low two bits are `0b11` therefore decodes to an address that no
    /// installation could have produced -- and that value used to be returned
    /// to the evacuator, stored into the reference slot being scanned, and
    /// pushed onto the gray worklist as a holder.
    ///
    /// See `G1Collector::decode_forwarding_target` for the measurement: four
    /// of four reference-slot writes carrying a tagged value had `low3 = 4`,
    /// and their payloads were ASCII.
    #[test]
    pub fn a_word_that_is_forwarded_only_by_coincidence_does_not_decode_to_a_target() {
        let gc = make_collector();
        // A forwarded holder keeps its header word and carries the target in
        // its second word; build one whose second word is whatever the case
        // needs.
        #[repr(C, align(8))]
        struct Holder([u64; 2]);
        let holder_with = |target: u64| {
            let h = Box::new(Holder([
                (cratonvm_types::MARK_FORWARDED as u64) << 32,
                target,
            ]));
            let addr = &*h as *const Holder as usize;
            (h, addr)
        };
        let mark = cratonvm_types::MARK_FORWARDED;

        // A real install round-trips.
        let real = gc.arena_base + 0x1000;
        let (_h, holder) = holder_with(real as u64);
        assert_eq!(
            gc.decode_forwarding_target(mark, holder),
            Some(real as *mut u8),
            "a target this collector installed must decode back to itself"
        );

        // The ASCII payload of a Java string sitting where a target belongs:
        // bit 2 set, so it cannot be an object address.
        let (_h, holder) = holder_with(0x0000_636a_6163_6a2c_u64);
        assert_eq!(
            gc.decode_forwarding_target(mark, holder),
            None,
            "a word no install could have written must not decode"
        );

        // ...and an 8-ALIGNED coincidence, which alignment alone cannot catch,
        // is refused by the arena bound.
        let (_h, holder) = holder_with(0x0000_636a_6163_6a28_u64);
        assert_eq!(gc.decode_forwarding_target(mark, holder), None);
    }

    #[test]
    pub fn collector_creation() {
        let gc = make_collector();
        assert_eq!(gc.num_regions(), 8);
        assert_eq!(gc.collection_count(), 0);
        assert_eq!(gc.total_pause_ms(), 0);
    }

    #[test]
    pub fn collector_default() {
        let gc = G1Collector::with_defaults();
        assert_eq!(gc.num_regions(), 256);
    }

    // -- Allocation --

    #[test]
    pub fn alloc_object_basic() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 2);
        let header = gc.get_header(obj);
        assert_eq!(header.class_id, ClassId::new(1));
        assert_eq!(header.kind(), ObjectKind::Object);
        assert_eq!(header.num_slots(), 2);
        assert_eq!(gc.count_regions(RegionType::Eden), 1);
    }

    #[test]
    pub fn alloc_multiple_objects_same_region() {
        let gc = make_collector();
        let _obj1 = gc.alloc_object(ClassId::new(1), 1);
        let _obj2 = gc.alloc_object(ClassId::new(2), 1);
        assert_eq!(gc.count_regions(RegionType::Eden), 1);
    }

    #[test]
    pub fn alloc_array_basic() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(5), ArrayElementType::Int, 10);
        assert_eq!(gc.array_length(arr), 10);
        assert_eq!(gc.kind_of(arr), ObjectKind::Array);
    }

    #[test]
    pub fn alloc_field_read_write() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 3);
        gc.set_field(obj, 0, Value::Int(42));
        gc.set_field(obj, 1, Value::Long(1000));
        gc.set_field(obj, 2, Value::Float(3.14));
        assert_eq!(gc.get_field(obj, 0).as_int(), Some(42));
        assert_eq!(gc.get_field(obj, 1).as_long(), Some(1000));
    }

    #[test]
    pub fn alloc_array_elements() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
        gc.set_array_element(arr, 0, Value::Int(10)).unwrap();
        gc.set_array_element(arr, 1, Value::Int(20)).unwrap();
        gc.set_array_element(arr, 2, Value::Int(30)).unwrap();
        assert_eq!(gc.get_array_element(arr, 0).unwrap().as_int(), Some(10));
        assert_eq!(gc.get_array_element(arr, 1).unwrap().as_int(), Some(20));
        assert_eq!(gc.get_array_element(arr, 2).unwrap().as_int(), Some(30));
    }

    #[test]
    pub fn boolean_array_elements_zero_extend_like_shared_heap_path() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Boolean, 1);

        gc.set_array_element(arr, 0, Value::Int(255)).unwrap();

        assert_eq!(gc.get_array_element(arr, 0).unwrap(), Value::Int(255));
    }

    #[test]
    pub fn alloc_array_bounds_check() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 2);
        assert!(gc.get_array_element(arr, 2).is_err());
        assert!(gc.set_array_element(arr, 5, Value::Int(0)).is_err());
    }

    // -- Humongous allocation --

    #[test]
    pub fn humongous_allocation() {
        let gc = make_collector();
        // region_size is 1MB, so > 512KB is humongous
        // A large array: HEADER_SIZE + 600000 * 4 bytes = ~2.4 MB
        let large = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 150_000);
        assert_eq!(gc.array_length(large), 150_000);
        assert!(gc.count_regions(RegionType::HumongousStart) >= 1);
    }

    // -----------------------------------------------------------------------
    // Eager humongous reclaim (`CRATONVM_G1_EAGER_HUMONGOUS`)
    //
    // A humongous span is never evacuated, so before this the only thing that
    // ever freed one was a concurrent-mark cleanup. These pin the pause-time
    // reclaim: what it frees, and — far more important — every case in which it
    // must refuse to.
    // -----------------------------------------------------------------------

    /// Item 1's MARKING half, in the one shape it exists for: a holder the
    /// pause SCANS. A young object in the collection set references the span,
    /// so `scan_and_evacuate_refs` walks that slot and
    /// `note_humongous_reference` records the span -- and eager reclaim can
    /// then answer without reading the span's remembered set or walking a
    /// single source region.
    ///
    /// Distinguishes "the marking is implemented and simply does not engage on
    /// a given workload" from "the marking never fires at all". Every
    /// production measurement taken on 2026-09-05 reported `marked_live=0`, so
    /// the difference matters: this test says the mechanism works, and the
    /// census says the edges it can see are not the edges real heaps have.
    #[test]
    pub fn a_span_referenced_from_a_scanned_young_object_is_marked_not_walked() {
        // Opt-in since 2026-09-05 — see `note_humongous_reference` for the
        // measured engagement that put it behind a flag.
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_G1_HUMONGOUS_MARKS", Some("1"))],
            || a_span_referenced_from_a_scanned_young_object_is_marked_not_walked_body(),
        );
    }

    pub fn a_span_referenced_from_a_scanned_young_object_is_marked_not_walked_body() {
        let gc = make_collector();
        let h = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 150_000);
        // A YOUNG holder, which the pause will evacuate and therefore scan.
        let holder = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(holder, 0, Value::Object(Some(h)));

        gc.clear_humongous_references();
        let regions = gc.regions.read();
        let span = gc
            .region_for_ptr(&regions, h.as_ptr())
            .and_then(|i| humongous_span_start(&regions, i))
            .expect("the span's start region");
        drop(regions);

        // What the slot loops do for a humongous referent.
        let regions = gc.regions.read();
        let idx = gc
            .region_for_ptr(&regions, h.as_ptr())
            .expect("the referent's region");
        gc.note_humongous_reference(&regions, idx);
        drop(regions);

        let marks = gc.take_humongous_references();
        assert!(
            marks.contains(&span),
            "a slot scanned into the span must mark its START region"
        );
        assert!(
            gc.take_humongous_references().is_empty(),
            "taking the marks must clear them for the next pause"
        );
    }

    /// Ten-findings item 1: with a humongous span present, a young pause no
    /// longer walks the whole heap to take a census. An Old region this pause
    /// never touches is not walked, and the span it holds still survives,
    /// because liveness now comes from the span's remembered set — and when
    /// the holder drops its reference, the stale-by-death entry is walked,
    /// found empty, and the span is reclaimed.
    #[test]
    pub fn a_humongous_span_held_by_an_untouched_old_object_survives_a_narrow_pause() {
        let gc = make_collector();
        let h = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        gc.set_array_element(h, 5, Value::Long(0xABCD)).unwrap();

        // An Old holder placed by hand, the way `cleanup` leaves one, storing
        // its reference through the barrier so the edge is recorded.
        let holder = {
            let mut regions = gc.regions.write();
            let idx = regions
                .iter()
                .position(|r| r.region_type() == RegionType::Free)
                .expect("a free region");
            regions[idx].set_region_type(RegionType::Old);
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
        gc.with_regions_mut(|_| {}); // republish the counters after the hand edit
        gc.set_field(holder, 0, Value::Object(Some(h)));

        // Young churn so the pause has a non-empty CSet.
        let _churn = gc.alloc_object(ClassId::new(2), 1);
        let mut roots: Vec<ObjectRef> = vec![];
        gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            1,
            "the span is held by the untouched Old object"
        );
        assert_eq!(gc.get_array_element(h, 5).unwrap(), Value::Long(0xABCD));
        let last = gc
            .pause_history_snapshot()
            .last()
            .cloned()
            .expect("a pause was recorded");
        let occupied = gc.num_regions() - free_region_count(&gc);
        assert!(
            (last.phases.fixup_regions as usize) < occupied,
            "the fix-up walked {} of {occupied} occupied regions — the humongous \
             span forced a wide walk",
            last.phases.fixup_regions
        );

        gc.set_field(holder, 0, Value::Object(None));
        let _churn = gc.alloc_object(ClassId::new(2), 1);
        gc.young_collection(&mut roots, &NoopMonitors);
        // TEMPORARY PROBE: how many further pauses until the span goes?
        for n in 1..=6u32 {
            if gc.count_regions(RegionType::HumongousStart) == 0 {
                eprintln!("[probe] span reclaimed after {} extra pause(s)", n - 1);
                break;
            }
            let _c = gc.alloc_object(ClassId::new(2), 1);
            gc.young_collection(&mut roots, &NoopMonitors);
            if n == 6 {
                eprintln!(
                    "[probe] span STILL held after 6 extra pauses: {}",
                    gc.count_regions(RegionType::HumongousStart)
                );
            }
        }
        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            0,
            "nothing references the span any more, and its rset entry alone \
             must not keep it"
        );
    }

    /// A promoting pause must take up the Old region an earlier one left partly
    /// filled, instead of consuming another Free region.
    ///
    /// Without this the old generation grows by one region per evacuation
    /// WORKER per young pause however little is promoted, which on a 32-CPU
    /// box (23 workers) consumed 2026 of 2048 regions on the Tomcat
    /// `TestHostConfigAutomaticDeploymentXmlExternalWarXml` class and OOMed a
    /// workload HotSpot runs in 20 MB. See `SharedEvac::resume`.
    ///
    /// TWO pauses per promotion, and `promotion_age: 1` rather than 0: an
    /// object has to reach the tenuring threshold in survivor space first, and
    /// the threshold cannot be driven to zero -- `update_tenuring_threshold`
    /// floors the configured age at 1, so a config of 0 promotes on the first
    /// pause and never again, which is how the first draft of this test failed
    /// while the fix was working.
    #[test]
    pub fn a_promoting_pause_takes_up_the_old_region_an_earlier_one_left_partly_filled() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 64 * 1024 * 1024,
            initial_heap_size: 64 * 1024 * 1024,
            promotion_age: 1,
            ..small_config()
        });
        let resumed = || PARALLEL_EVAC_RESUMED_DEST_REGIONS.load(Ordering::Relaxed);

        // Batch one: pause to survivor space, then pause again to promote.
        let mut roots: Vec<ObjectRef> = (0..8)
            .map(|i| gc.alloc_object(ClassId::new(1), i % 3))
            .collect();
        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        let old_after_first = gc.count_regions(RegionType::Old);
        assert!(
            old_after_first > 0,
            "the fixture is wrong: nothing was promoted, so there is no partly-filled Old region for a later pause to take up"
        );

        // Batch two, the same way. Its promotions have somewhere to go.
        roots.extend((0..8).map(|i| gc.alloc_object(ClassId::new(1), i % 3)));
        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        let before = resumed();
        let promoting = gc.young_collection_parallel(&mut roots, &NoopMonitors);

        assert!(
            promoting.stats.objects_copied > 0,
            "the fixture is wrong: the promoting pause evacuated nothing"
        );
        assert_eq!(
            gc.count_regions(RegionType::Old),
            old_after_first,
            "a later promoting pause consumed another Free region for its promotion TLAB instead of resuming the one an earlier pause left partly filled"
        );
        assert!(
            resumed() > before,
            "Old did not grow, but the resume path is not what placed the promotions -- this assertion exists so the test cannot pass because the pause promoted nothing. resumed {before}->{} old={old_after_first} free={} shared_dest={}",
            resumed(),
            free_region_count(&gc),
            PARALLEL_SHARED_DEST_ALLOCS.load(Ordering::Relaxed),
        );
    }

    /// PARITY: the per-pause region census is filled on BOTH young arms.
    ///
    /// It was filled by `young_collection_serial` only, so a default build --
    /// which runs the PARALLEL arm -- printed
    /// `free_regions=0 eden_regions=0 surv_regions=0 old_regions=0
    /// hum_regions=0 cset_regions=0` on every `[GC-STAT]` line, and an
    /// operator reading it saw an empty heap rather than a missing
    /// instrument. Measured on the Tomcat
    /// `TestHostConfigAutomaticDeploymentXmlExternalWarXml` class: 63 pauses,
    /// every one of those eight fields zero.
    ///
    /// Both arms are driven DIRECTLY rather than through `young_collection`,
    /// for the reason that dispatcher's own doc comment gives: a test that
    /// goes through it exercises whichever arm the ambient flags select, and
    /// this is exactly the defect such a test cannot see.
    #[test]
    pub fn both_young_arms_report_a_region_census() {
        for parallel in [false, true] {
            let label = if parallel { "parallel" } else { "serial" };
            let gc = make_collector();
            // Something to evacuate, so this is a real pause rather than the
            // empty-CSet early return (which reports no census by design).
            let _live = gc.alloc_object(ClassId::new(1), 1);
            let _churn = gc.alloc_object(ClassId::new(2), 1);
            let mut roots: Vec<ObjectRef> = vec![];
            if parallel {
                gc.young_collection_parallel(&mut roots, &NoopMonitors);
            } else {
                gc.young_collection_serial(&mut roots, &NoopMonitors);
            }
            let last = gc
                .pause_history_snapshot()
                .last()
                .cloned()
                .expect("a pause was recorded");
            let counted = (last.phases.free_regions
                + last.phases.eden_regions
                + last.phases.surv_regions
                + last.phases.old_regions
                + last.phases.hum_regions) as usize;
            assert_eq!(
                counted,
                gc.num_regions(),
                "{label}: the census must account for every region; it counted {counted} of {}",
                gc.num_regions()
            );
            assert!(
                last.phases.cset_regions > 0,
                "{label}: a pause that evacuated must report its collection set"
            );
        }
    }

    /// The point of the feature: an unreferenced span goes away at the next
    /// pause rather than waiting for a mark cycle that may never come.
    #[test]
    pub fn a_young_pause_reclaims_a_humongous_span_nothing_references() {
        let gc = make_collector();
        let dead = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        assert!(gc.is_humongous(dead));
        assert_eq!(gc.count_regions(RegionType::HumongousStart), 1);

        // Give the pause something to actually collect, so this exercises a
        // real evacuation rather than the empty-CSet early return.
        let _churn = gc.alloc_object(ClassId::new(1), 1);

        let mut roots: Vec<ObjectRef> = vec![];
        gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            0,
            "an evacuation pause must reclaim a humongous span nothing points at"
        );
    }

    /// A root is not something the Phase-4 heap walk can see, so it has to be
    /// consulted separately. Forgetting it frees the object a thread is using.
    #[test]
    pub fn a_humongous_span_named_by_a_root_survives_the_pause() {
        let gc = make_collector();
        let live = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        gc.set_array_element(live, 99_999, Value::Long(0x5EED))
            .unwrap();
        let _churn = gc.alloc_object(ClassId::new(1), 1);

        let mut roots: Vec<ObjectRef> = vec![live];
        gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(gc.count_regions(RegionType::HumongousStart), 1);
        assert_eq!(
            gc.get_array_element(roots[0], 99_999).unwrap(),
            Value::Long(0x5EED)
        );
    }

    /// THE ordering hazard, and the reason liveness here comes from the Phase-4
    /// walk rather than from the remembered set.
    ///
    /// Y lives in Eden and holds the only reference to humongous H, so H's rset
    /// names Eden. This pause copies Y to Survivor and frees Eden — at which
    /// point H's only remembered-set entry names a region that has just been
    /// zero-filled, and the live Survivor copy of Y is in no rset at all. A
    /// reclaim that asked the remembered set would free H here. Asking the
    /// Phase-4 walk (which visits the Survivor copy, because to-space is typed
    /// and cursor-committed before Phase 4 runs) gets it right.
    #[test]
    pub fn a_humongous_span_held_only_by_an_evacuated_young_object_survives() {
        let gc = make_collector();
        let h = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        gc.set_array_element(h, 99_999, Value::Long(0xFEED))
            .unwrap();

        let holder = gc.alloc_object(ClassId::new(1), 1);
        let holder_before = holder.as_ptr();
        gc.set_field(holder, 0, Value::Object(Some(h)));

        let mut roots: Vec<ObjectRef> = vec![holder];
        gc.young_collection(&mut roots, &NoopMonitors);

        assert_ne!(
            roots[0].as_ptr(),
            holder_before,
            "the holder must actually have been evacuated, or this test proves nothing"
        );
        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            1,
            "the span is still reachable through the evacuated holder"
        );
        match gc.get_field(roots[0], 0) {
            Value::Object(Some(r)) => assert_eq!(
                gc.get_array_element(r, 99_999).unwrap(),
                Value::Long(0xFEED),
                "the span survived but its contents did not"
            ),
            other => panic!("the holder lost its reference to the span: {other:?}"),
        }
    }

    /// Under SATB, "unreferenced right now" is not "dead": an object dropped
    /// after the mark-start snapshot is still snapshot-live, and freeing it
    /// unmarks its whole unscanned subtree. The pause must decline outright
    /// while a cycle is open.
    #[test]
    pub fn an_open_mark_cycle_suppresses_eager_humongous_reclaim() {
        // The `CRATONVM_G1_EAGER_RECLAIM_DURING_MARK=0` arm (default ON since
        // 2026-10-17 frees a primitive-array span during a mark, as HotSpot does).
        let mut gc = make_collector();
        gc.eager_reclaim_during_mark = false;
        let _dead = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
        assert_eq!(gc.count_regions(RegionType::HumongousStart), 1);

        gc.start_concurrent_mark(&stw());
        let _churn = gc.alloc_object(ClassId::new(1), 1);

        let mut roots: Vec<ObjectRef> = vec![];
        gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            gc.count_regions(RegionType::HumongousStart),
            1,
            "an open mark cycle must suppress eager reclaim: SATB snapshot \
             liveness applies and the gray set holds addresses this pause \
             never walked"
        );
    }


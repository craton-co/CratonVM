    use super::*;

    struct NoopMonitors;
    impl MonitorCleanup for NoopMonitors {
        fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
    }

    /// g1ep7: a movable region's table from its granule marks is its marked
    /// objects, in address order, with their sizes; a mark inside a marked
    /// object, or one past the cursor's last whole object, refuses.
    #[test]
    fn g1ep7_the_live_table_is_the_marked_objects_and_refuses_an_interior_mark() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 8 * 1024 * 1024,
            initial_heap_size: 8 * 1024 * 1024,
            region_size: 1024 * 1024,
            gc_worker_threads: 1,
            ..G1CollectorConfig::default()
        });
        let objs: Vec<ObjectRef> = (0..4).map(|i| gc.alloc_object(ClassId::new(0x7B00 + i), 2)).collect();
        let idx = gc.lookup_region_for_addr(objs[0].as_ptr() as usize).expect("in the heap");
        let regions = gc.regions.read();
        let (base, cursor) = (regions[idx].data.addr(), regions[idx].cursor());
        assert!(objs.iter().all(|o| gc.lookup_region_for_addr(o.as_ptr() as usize) == Some(idx)));
        let marks: Vec<AtomicU64> = (0..(cursor / 8).div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
        let mark = |addr: usize| {
            let g = (addr - base) / 8;
            marks[g / 64].fetch_or(1u64 << (g % 64), Ordering::Relaxed);
        };
        // Objects 3 and 1, out of order: the table is in address order.
        mark(objs[3].as_ptr() as usize);
        mark(objs[1].as_ptr() as usize);
        // SAFETY: every mark is an object start below the cursor.
        let (offs, sizes) = unsafe { live_table_from_marks(base, cursor, &marks) }.expect("sound");
        let want: Vec<u32> = [1usize, 3].iter().map(|&i| (objs[i].as_ptr() as usize - base) as u32).collect();
        assert_eq!(offs, want);
        for (k, &i) in [1usize, 3].iter().enumerate() {
            // SAFETY: a live object of this region.
            let h = unsafe { &*(objs[i].as_ptr() as *const ObjectHeader) };
            assert_eq!(sizes[k] as usize, object_total_size(h));
        }
        let all = RegionObjs::all_live(base, offs, sizes);
        assert_eq!(all.live_indices().collect::<Vec<_>>(), vec![0, 1]);
        // A word inside object 1 is no object start.
        mark(objs[1].as_ptr() as usize + 8);
        // SAFETY: as above; the interior mark is below the cursor.
        assert!(unsafe { live_table_from_marks(base, cursor, &marks) }.is_err());
    }

    /// g1ep: the start bitmap answers every offset as the binary search over
    /// `offs` does -- starts, interiors, past the last object, word edges --
    /// and an unaligned offset list falls back to the search.
    #[test]
    fn g1ep_the_start_index_answers_the_binary_search() {
        let mut offs = Vec::new();
        let mut at = 0u32;
        for i in 0..5000u32 {
            offs.push(at);
            at += 16 + 8 * (i % 37);
        }
        let r = RegionObjs::new(0x1000_0000, offs.clone(), vec![16; offs.len()]);
        assert!(!r.starts.is_empty());
        for off in (0..at + 1024).step_by(4) {
            assert_eq!(r.index_of(off), offs.binary_search(&off).ok(), "off={off}");
        }
        let odd = RegionObjs::new(0x1000_0000, vec![0, 12, 40], vec![12, 28, 16]);
        assert!(odd.starts.is_empty());
        for off in 0..64 {
            assert_eq!(odd.index_of(off), [0u32, 12, 40].binary_search(&off).ok());
        }
        assert_eq!(RegionObjs::new(0, Vec::new(), Vec::new()).index_of(0), None);
    }

    const CHAINS: usize = 64;
    const DEPTH: usize = 120;

    /// A 16 MB heap whose 64 chains of `DEPTH` nodes (each holding a 40-slot
    /// payload) are all tenured into Old regions; then every odd chain is cut
    /// off, so the Old regions hold its garbage interleaved with the even
    /// chains' live nodes.
    fn tenured_chains() -> (G1Collector, Vec<ObjectRef>) {
        tenured_chains_with(1)
    }

    fn tenured_chains_with(workers: usize) -> (G1Collector, Vec<ObjectRef>) {
        let (gc, roots) = tenured_chains_uncut(workers);
        for c in (1..CHAINS).step_by(2) {
            gc.set_field(roots[0], c, Value::Object(None));
        }
        (gc, roots)
    }

    /// [`tenured_chains_with`] before the cut: every chain live.
    fn tenured_chains_uncut(workers: usize) -> (G1Collector, Vec<ObjectRef>) {
        let gc = chains_gc(workers);
        let mut roots = Vec::new();
        grow_tenured_chains(&gc, &mut roots);
        (gc, roots)
    }

    /// The chains' 16 MB heap of 1 MB regions.
    fn chains_gc(workers: usize) -> G1Collector {
        G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            initial_heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            gc_worker_threads: workers,
            promotion_age: 1,
            ..G1CollectorConfig::default()
        })
    }

    /// Push the chains' ring as `roots[0]` and grow every chain, tenured.
    fn grow_tenured_chains(gc: &G1Collector, roots: &mut Vec<ObjectRef>) {
        let ring = gc.alloc_object(ClassId::new(1), CHAINS);
        roots.insert(0, ring);
        for d in 0..DEPTH {
            for c in 0..CHAINS {
                let node = gc.alloc_object(ClassId::new(1), 3);
                let payload = gc.alloc_object(ClassId::new(2), 40);
                gc.set_field(payload, 0, Value::Int((c * DEPTH + d) as i32));
                let prev = gc.get_field(roots[0], c);
                gc.set_field(node, 0, prev);
                gc.set_field(node, 1, Value::Int((c * DEPTH + d) as i32));
                gc.set_field(node, 2, Value::Object(Some(payload)));
                gc.set_field(roots[0], c, Value::Object(Some(node)));
            }
            if d % 20 == 19 {
                gc.pause_tenures_all.store(true, Ordering::Relaxed);
                let _ = gc.young_collection_serial(roots, &NoopMonitors);
                gc.pause_tenures_all.store(false, Ordering::Relaxed);
            }
        }
        gc.pause_tenures_all.store(true, Ordering::Relaxed);
        let _ = gc.young_collection_serial(roots, &NoopMonitors);
        gc.pause_tenures_all.store(false, Ordering::Relaxed);
    }

    /// Every even chain is intact, in order, payloads included; every odd
    /// chain is cut. Answers the nodes seen.
    fn check_chains(gc: &G1Collector, ring: ObjectRef) -> usize {
        let mut seen = 0;
        for c in 0..CHAINS {
            let mut cur = gc.get_field(ring, c);
            if c % 2 == 1 {
                assert_eq!(cur, Value::Object(None), "chain {c} was cut");
                continue;
            }
            let mut d = DEPTH;
            while let Value::Object(Some(node)) = cur {
                d -= 1;
                let tag = (c * DEPTH + d) as i32;
                assert_eq!(gc.get_field(node, 1), Value::Int(tag), "chain {c} node {d}");
                let Value::Object(Some(payload)) = gc.get_field(node, 2) else {
                    panic!("chain {c} node {d} lost its payload");
                };
                assert_eq!(gc.get_field(payload, 0), Value::Int(tag), "chain {c} payload {d}");
                seen += 1;
                cur = gc.get_field(node, 0);
            }
            assert_eq!(d, 0, "chain {c} lost {d} node(s)");
        }
        seen
    }

    #[test]
    fn a_slide_frees_nothing_when_its_free_set_is_the_old_one() {
        // Without Free destinations: declines only when every region keeps data.
        assert!(slide_frees_nothing(&[8, 8, 8], &[false, false, false]));
        assert!(!slide_frees_nothing(&[8, 8, 0], &[false, false, false]));
        // With them: a Free region filled and the top one emptied is a change.
        assert!(slide_frees_nothing(&[8, 0, 8], &[false, true, false]));
        assert!(!slide_frees_nothing(&[8, 8, 0], &[false, true, false]));
    }

    #[test]
    fn the_starved_predicate_is_an_eighth_of_the_heap_at_least_eight() {
        assert!(in_place_compaction_starved(0, 64));
        assert!(in_place_compaction_starved(7, 64));
        assert!(!in_place_compaction_starved(8, 64));
        assert!(in_place_compaction_starved(127, 1024));
        assert!(!in_place_compaction_starved(128, 1024));
        assert!(!in_place_compaction_starved(4, 4), "a 4-region heap: never more than its regions");
    }

    #[test]
    fn an_in_place_compaction_keeps_the_live_graph_and_frees_the_garbage() {
        let (gc, mut roots) = tenured_chains();
        let old_before = gc.count_regions(RegionType::Old);
        let free_before = gc.count_regions(RegionType::Free);
        let ring_before = roots[0];
        let result = gc
            .full_compaction_in_place(&mut roots, &NoopMonitors)
            .expect("the pass runs on a tenured heap with half its Old data dead");
        let old_after = gc.count_regions(RegionType::Old);
        let free_after = gc.count_regions(RegionType::Free);
        eprintln!(
            "in-place: old {old_before}->{old_after} free {free_before}->{free_after} \
             moved={} freed={}",
            result.stats.bytes_copied, result.stats.bytes_freed
        );
        assert!(free_after > free_before, "regions were reclaimed");
        assert_eq!(old_before - old_after, free_after - free_before);
        assert!(result.stats.bytes_freed >= 1024 * 1024);
        // The ring is a key (identity or moved), and the root was rewritten
        // to its value.
        assert_eq!(
            result.pointer_map.get(&(ring_before.as_ptr() as usize)).copied(),
            Some(roots[0].as_ptr() as usize)
        );
        assert_eq!(check_chains(&gc, roots[0]), (CHAINS / 2) * DEPTH);
        assert!(!gc.last_pause_slid_ranges().is_empty());
        // The heap goes on working: allocate and collect over the result.
        for _ in 0..2000 {
            let _ = gc.alloc_object(ClassId::new(2), 8);
        }
        let _ = gc.young_collection_serial(&mut roots, &NoopMonitors);
        assert_eq!(check_chains(&gc, roots[0]), (CHAINS / 2) * DEPTH);
    }

    /// The R22 aastore-humongous page (`CRATONVM_G1_FRAGMENTED_HUMONGOUS_SLIDE`):
    /// a dead ring tenured below a live one, its regions reclaimed as a
    /// cleanup would, leaves Free holes under the live Old data. The plain
    /// slide frees nothing there (the live regions pack no lower than they
    /// sit); the Free-destination slide packs the live data into the holes
    /// and leaves every Free region in one run.
    #[test]
    fn a_free_destination_slide_gathers_the_free_regions_into_one_run() {
        let gc = chains_gc(1);
        let mut roots = Vec::new();
        grow_tenured_chains(&gc, &mut roots);
        grow_tenured_chains(&gc, &mut roots);
        for c in (1..CHAINS).step_by(2) {
            gc.set_field(roots[0], c, Value::Object(None));
        }
        // The regions each ring's objects sit in.
        let regions_of = |ring: ObjectRef| -> std::collections::BTreeSet<usize> {
            let mut set = std::collections::BTreeSet::new();
            let mut note = |o: ObjectRef| {
                set.insert(
                    gc.lookup_region_for_addr(o.as_ptr() as usize)
                        .expect("in the heap"),
                );
            };
            note(ring);
            for c in 0..CHAINS {
                let mut cur = gc.get_field(ring, c);
                while let Value::Object(Some(node)) = cur {
                    note(node);
                    if let Value::Object(Some(p)) = gc.get_field(node, 2) {
                        note(p);
                    }
                    cur = gc.get_field(node, 0);
                }
            }
            set
        };
        let live = regions_of(roots[0]);
        let dead = regions_of(roots[1]);
        roots.truncate(1);
        {
            let mut regions = gc.regions.write();
            let generation = gc.rset_generation();
            for &i in dead.difference(&live) {
                if regions[i].region_type() == RegionType::Old {
                    regions[i].reset(generation);
                }
            }
            gc.publish_region_census(&regions);
        }
        let (free_before, longest_before) = free_region_shape(&gc.regions.read());
        assert!(
            longest_before < free_before,
            "holes under the live data: {free_before}/{longest_before}"
        );
        let ring_before = roots[0];
        let result = gc
            .full_compaction_in_place_with(&mut roots, &NoopMonitors, true)
            .expect("the slide runs into the holes");
        let (free_after, longest_after) = free_region_shape(&gc.regions.read());
        eprintln!(
            "fill-free slide: free {free_before}->{free_after} longest {longest_before}->{longest_after}"
        );
        assert!(free_after >= free_before);
        assert_eq!(longest_after, free_after, "one Free run");
        assert_eq!(
            result
                .pointer_map
                .get(&(ring_before.as_ptr() as usize))
                .copied(),
            Some(roots[0].as_ptr() as usize)
        );
        assert_eq!(check_chains(&gc, roots[0]), (CHAINS / 2) * DEPTH);
        for _ in 0..2000 {
            let _ = gc.alloc_object(ClassId::new(2), 8);
        }
        let _ = gc.young_collection_serial(&mut roots, &NoopMonitors);
        assert_eq!(check_chains(&gc, roots[0]), (CHAINS / 2) * DEPTH);
    }

    /// r23 w6 (`CRATONVM_G1_STARVED_FULL`): an armed starved full is a whole
    /// pause through `collect_garbage` -- its old half copies nothing and the
    /// in-place pass runs whatever the Free pool holds (this 16-region heap is
    /// not under the ordinary starved test after its tenuring pauses).
    #[test]
    fn a_starved_full_pause_slides_old_and_keeps_the_graph() {
        let (gc, mut roots) = tenured_chains();
        let free_before = gc.count_regions(RegionType::Free);
        let (runs_before, _) = g1_starved_full_census();
        assert!(gc.arm_starved_full());
        // SAFETY: this unit test runs the heap single-threaded.
        let stw = unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() };
        let _ = gc.collect_garbage(&stw, &mut roots, &NoopMonitors);
        assert!(gc.count_regions(RegionType::Free) > free_before, "the slide freed regions");
        assert_eq!(check_chains(&gc, roots[0]), (CHAINS / 2) * DEPTH);
        assert!(g1_starved_full_census().0 > runs_before, "counted as a starved full");
        assert!(!gc.starved_full_this_pause.load(Ordering::Relaxed), "the pause's flag is spent");
        // The next pause is an ordinary one.
        let _ = gc.collect_garbage(&stw, &mut roots, &NoopMonitors);
        assert_eq!(check_chains(&gc, roots[0]), (CHAINS / 2) * DEPTH);
    }


    /// A JIT-pinned region (here: an interior root names a live node's field,
    /// as a conservative word would) is HELD: the pass runs, nothing in it
    /// moves, its cursor stays, the root is not rewritten, its dead nodes are
    /// fillers afterwards, and the other regions still slide.
/// t1 W13 (`docs/feature-designs/jit-ir-optimizations.md`):
    /// a stale JIT pin word naming a DEAD payload holds its region. By default
    /// Live chains (none cut) tenured into Old, then the payloads in the
    /// topmost Old region of every `every`-th chain dropped (with `every` 1,
    /// most of its bytes), one of them kept by an interior root word that
    /// holds the region: a held region of dead space above packed Old
    /// regions. Answers its index and the dropped tags.
    fn live_chains_under_a_pinned_top_region(
        every: usize,
    ) -> (G1Collector, Vec<ObjectRef>, usize, std::collections::HashSet<i32>) {
        let (gc, mut roots) = tenured_chains_uncut(1);
        let top = gc
            .regions
            .read()
            .iter()
            .enumerate()
            .filter(|(_, r)| r.region_type() == RegionType::Old)
            .map(|(i, _)| i)
            .max()
            .expect("tenured chains");
        let mut dropped = std::collections::HashSet::new();
        let mut keeper = None;
        for c in (0..CHAINS).step_by(every) {
            let mut cur = gc.get_field(roots[0], c);
            while let Value::Object(Some(n)) = cur {
                if let Value::Object(Some(p)) = gc.get_field(n, 2) {
                    if gc.lookup_region_for_addr(p.as_ptr() as usize) == Some(top) {
                        let Value::Int(tag) = gc.get_field(n, 1) else { panic!("a tag") };
                        dropped.insert(tag);
                        keeper.get_or_insert(p);
                        gc.set_field(n, 2, Value::Object(None));
                    }
                }
                cur = gc.get_field(n, 0);
            }
        }
        let keeper = keeper.expect("payloads in the topmost Old region");
        // SAFETY: a root word the pass only compares and looks up.
        roots.push(unsafe { ObjectRef::from_raw((keeper.as_ptr() as usize + 8) as *mut u8) });
        (gc, roots, top, dropped)
    }

    /// Every chain whole and in order; each payload present with its tag,
    /// except the `dropped` ones, which are gone. Answers the payloads seen.
    fn check_chains_but(
        gc: &G1Collector,
        ring: ObjectRef,
        dropped: &std::collections::HashSet<i32>,
    ) -> usize {
        let mut payloads = 0;
        for c in 0..CHAINS {
            let mut cur = gc.get_field(ring, c);
            let mut d = DEPTH;
            while let Value::Object(Some(node)) = cur {
                d -= 1;
                let tag = (c * DEPTH + d) as i32;
                assert_eq!(gc.get_field(node, 1), Value::Int(tag), "chain {c} node {d}");
                match gc.get_field(node, 2) {
                    Value::Object(Some(p)) => {
                        assert!(!dropped.contains(&tag), "chain {c} node {d}: dropped");
                        assert_eq!(gc.get_field(p, 0), Value::Int(tag), "chain {c} payload {d}");
                        payloads += 1;
                    }
                    _ => assert!(dropped.contains(&tag), "chain {c} node {d} lost its payload"),
                }
                cur = gc.get_field(node, 0);
            }
            assert_eq!(d, 0, "chain {c} lost {d} node(s)");
        }
        payloads
    }

    /// One in-place pass over [`live_chains_under_a_pinned_top_region`] with
    /// the lift on or off: the regions it freed and the objects it moved into
    /// the held region. The graph is checked after the pass and after a young
    /// pause over its result.
    fn run_over_a_pinned_top_region(every: usize) -> (isize, usize) {
        let (gc, mut roots, last, dropped) = live_chains_under_a_pinned_top_region(every);
        let free_before = gc.count_regions(RegionType::Free);
        let outcome = gc.full_compaction_in_place(&mut roots, &NoopMonitors);
        let (base, end) = {
            let regions = gc.regions.read();
            (regions[last].data.addr(), regions[last].data.addr() + regions[last].data.len())
        };
        let into_top = outcome
            .as_ref()
            .map(|r| {
                r.pointer_map
                    .iter()
                    .filter(|&(&old, &new)| old != new && (base..end).contains(&new))
                    .count()
            })
            .unwrap_or(0);
        roots.pop();
        let payloads = CHAINS * DEPTH - dropped.len();
        assert_eq!(check_chains_but(&gc, roots[0], &dropped), payloads);
        let _ = gc.young_collection_serial(&mut roots, &NoopMonitors);
        assert_eq!(check_chains_but(&gc, roots[0], &dropped), payloads);
        (gc.count_regions(RegionType::Free) as isize - free_before as isize, into_top)
    }

    /// The lift (`CRATONVM_G1_IN_PLACE_LIFT_INTO_HELD_HOLES`): a held region at
    /// the top of the heap, mostly dead, takes objects from below it, which
    /// the downward slide could never put there; the pass frees more regions
    /// than without it, and the graph is intact.
    #[test]
    fn a_held_top_region_takes_objects_from_below_by_a_lift() {
        let (freed_on, into_top_on) = run_over_a_pinned_top_region(1);
        assert!(into_top_on > 0, "the lift moved objects into the top region's holes");
        assert!(freed_on > 0, "and freed a region");    }

    /// Half the holes (every other chain drops its payloads there), smaller
    /// than the movable regions' live bytes: single objects are lifted and
    /// still free a region. A quarter of them cannot free one however they are
    /// filled: the pass declines before anything moves, lift or not. The
    /// graph is intact every time.
    #[test]
    fn single_objects_are_lifted_when_that_frees_a_region_and_only_then() {
        let (freed_on, into_top) = run_over_a_pinned_top_region(2);
        assert!(into_top > 0 && freed_on > 0);
        let (freed, into_top) = run_over_a_pinned_top_region(4);
        assert_eq!((freed, into_top), (0, 0));    }

    /// The near-full proxy row (`CRATONVM_G1_IN_PLACE_CLEARS_KEPT_UNRESOLVED`):
    /// a pass that ran forgets the kept-unresolved state of a wedged drain in
    /// the same pause, which named only the recorded self-forwards live in
    /// regions the pass then compacted; one that declined keeps it.
    #[test]
    fn a_completed_pass_clears_a_wedged_drains_kept_regions() {
        let (gc, mut roots) = tenured_chains();
        let (idx, addr) = {
            let regions = gc.regions.read();
            let idx = regions
                .iter()
                .position(|r| r.region_type() == RegionType::Old)
                .expect("tenured chains");
            (idx, regions[idx].data.addr())
        };
        let arm = |gc: &G1Collector| {
            gc.kept_unresolved_regions.lock().insert(idx);
            gc.kept_unresolved_live.lock().insert(addr);
            gc.kept_unresolved_any.store(true, Ordering::Release);
            gc.in_place_only_this_pause.store(true, Ordering::Relaxed);
        };
        let empty = || GcResult {
            stats: GcStats {
                objects_copied: 0,
                bytes_copied: 0,
                bytes_freed: 0,
            },
            pointer_map: cratonvm_types::PointerMap::default(),
        };
        arm(&gc);
        let free_before = gc.count_regions(RegionType::Free);
        let _ = gc.full_compaction_in_place_after(empty(), &mut roots, &NoopMonitors);
        assert!(gc.count_regions(RegionType::Free) > free_before, "the pass ran");
        assert!(!gc.kept_unresolved_any.load(Ordering::Acquire));
        assert!(gc.kept_unresolved_regions.lock().is_empty());
        assert!(gc.kept_unresolved_live.lock().is_empty());
        // Nothing left to reclaim: the pass declines and the state stays.
        arm(&gc);
        let _ = gc.full_compaction_in_place_after(empty(), &mut roots, &NoopMonitors);
        assert!(gc.kept_unresolved_any.load(Ordering::Acquire), "a declined pass moved nothing");
        assert_eq!(check_chains(&gc, roots[0]), (CHAINS / 2) * DEPTH);
    }

    /// A chain-shaped graph (one root array of chain heads, every stack one or
    /// two deep) on an 8-worker pool: the idle spill shares the heads, and the
    /// pass keeps the graph.
    #[test]
    fn a_chain_shaped_graph_marks_and_slides_on_a_wide_pool() {
        let (gc, mut roots) = tenured_chains_with(8);
        let before = gc.count_regions(RegionType::Free);
        gc.full_compaction_in_place(&mut roots, &NoopMonitors)
            .expect("the pass runs");
        assert!(gc.count_regions(RegionType::Free) > before);
        assert_eq!(check_chains(&gc, roots[0]), (CHAINS / 2) * DEPTH);
    }


    /// A hole is split at each pin word strictly inside it; pins on a
    /// boundary or outside every hole change nothing.
    #[test]
    fn holes_split_at_the_pin_words_inside_them() {
        let holes = vec![(0, 64), (128, 1024)];
        assert_eq!(split_holes_at(holes.clone(), &mut []), holes);
        assert_eq!(
            split_holes_at(holes.clone(), &mut [512, 0, 96, 1024, 136]),
            vec![(0, 64), (128, 136), (136, 512), (512, 1024)]
        );
        assert_eq!(split_holes_at(holes.clone(), &mut [64, 128]), holes);
    }

    #[test]
    fn g1sz_the_start_index_answers_what_the_binary_search_does() {
        for offs in [
            vec![],
            vec![0u32],
            vec![0, 24, 48, 512, 520, 4096, 4104, 1 << 19],
            (0..3000u32).map(|i| i * 40).collect(),
            vec![0, 12, 40],
        ] {
            let n = offs.len();
            let r = RegionObjs::new(0, offs.clone(), vec![8; n]);
            for off in 0..(offs.last().copied().unwrap_or(0) + 600) {
                assert_eq!(r.index_of(off), offs.binary_search(&off).ok(), "off {off} of {offs:?}");
            }
        }
    }

    #[test]
    fn plan_chunks_cover_the_set_once() {
        assert_eq!(plan_chunks(60, 8), vec![(0, 60)]);
        assert_eq!(plan_chunks(127, 8), vec![(0, 127)]);
        let c = plan_chunks(1000, 8);
        assert_eq!(c.len(), 8);
        assert_eq!(c[0].0, 0);
        assert_eq!(c.last().unwrap().1, 1000);
        assert!(c.windows(2).all(|w| w[0].1 == w[1].0));
        assert_eq!(plan_chunks(200, 8), vec![(0, 67), (67, 134), (134, 200)]);
    }

    /// Eden left out of the pause's collection set is an ordinary source: an
    /// Eden object naming compacted Old objects is adjusted, and an Old object
    /// naming an Eden object keeps it.
    #[test]
    fn eden_objects_are_sources_and_keep_their_targets() {
        let (gc, mut roots) = tenured_chains();
        let holder = gc.alloc_object(ClassId::new(3), CHAINS);
        for c in (0..CHAINS).step_by(2) {
            let head = gc.get_field(roots[0], c);
            gc.set_field(holder, c, head);
        }
        let young = gc.alloc_object(ClassId::new(4), 1);
        gc.set_field(young, 0, Value::Int(4242));
        // An Old -> Eden edge, through the cut slot 1 of the (Old) ring.
        gc.set_field(roots[0], 1, Value::Object(Some(young)));
        roots.push(holder);
        assert!(gc.count_regions(RegionType::Eden) > 0);
        gc.full_compaction_in_place(&mut roots, &NoopMonitors)
            .expect("Eden is a source, not a refusal");
        let Value::Object(Some(y)) = gc.get_field(roots[0], 1) else {
            panic!("the Old -> Eden edge survived");
        };
        assert_eq!(gc.get_field(y, 0), Value::Int(4242));
        gc.set_field(roots[0], 1, Value::Object(None));
        assert_eq!(check_chains(&gc, roots[0]), (CHAINS / 2) * DEPTH);
        for c in (0..CHAINS).step_by(2) {
            assert_eq!(
                gc.get_field(roots[1], c),
                gc.get_field(roots[0], c),
                "the Eden holder's slot {c} follows the slid head"
            );
        }
    }

    /// A reference into the compaction set that names no object start makes
    /// the pass decline before anything moves.
    #[test]
    fn an_interior_reference_declines_and_changes_nothing() {
        let (gc, mut roots) = tenured_chains();
        let Value::Object(Some(node)) = gc.get_field(roots[0], 0) else {
            panic!("chain 0 has a head");
        };
        // An interior word of a live Old node, stored in the ring's (cut)
        // slot 1.
        let interior = unsafe { ObjectRef::from_raw((node.as_ptr() as usize + 8) as *mut u8) };
        gc.set_field(roots[0], 1, Value::Object(Some(interior)));
        let old_before = gc.count_regions(RegionType::Old);
        let ring = roots[0];
        let got = gc.full_compaction_in_place(&mut roots, &NoopMonitors);
        assert_eq!(got.err(), Some("interior-reference-into-compaction-set"));
        assert_eq!(gc.count_regions(RegionType::Old), old_before);
        assert_eq!(roots[0], ring);
        assert!(gc.last_pause_slid_ranges().is_empty());
        gc.set_field(roots[0], 1, Value::Object(None));
        assert_eq!(check_chains(&gc, roots[0]), (CHAINS / 2) * DEPTH);
    }

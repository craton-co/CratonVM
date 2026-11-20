use super::*;


/// The plan end to end on a real heap. Reference arrays packed on the
/// first pages, then spaced 128 KiB apart over garbage (sparse regions the
/// selector takes), linked in a ring by element 0 and across by element 1.
/// A simulated concurrent mark leaves every element in the mark color, so
/// the plan copies nothing in the pause; the map's lookups (the roots)
/// copy their objects; a barriered read of element 0 copies its target;
/// the drain copies the rest; the next mark's visits heal element 1; the
/// release gives the regions back. Every raw word then names the planned
/// address, and no planned object is left at its old base in the registry.
#[test]
fn zt32_the_plan_copies_lazily_and_the_ring_survives() {
    use cratonvm_gc_core::collector::GarbageCollector;
    use cratonvm_gc_core::heap::ArrayElementType;
    use cratonvm_types::{ClassId, ObjectRef, Value, ARRAY_DATA_OFFSET};
    const PAGE: usize = super::super::ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
    const FIELDS: usize = 500;
    let _colored = crate::zgc::tests::colored_arming_test_lock();
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_ZGC_REMAP_IN_MARK", Some("1"))],
        || {
            let heap = super::super::ZgcRealHeap::with_capacity(64 * 1024 * 1024);
            heap.set_tlab_enabled(false);
            assert!(heap.arm_load_barrier_colored_stores());
            assert!(heap.arm_load_barrier_color_flip());
            heap.counters.crl.set_enabled_for_test(true);
            let base = heap.arena.lock().base_ptr() as usize;
            let arr = |heap: &super::super::ZgcRealHeap| {
                heap.try_alloc_array(ClassId::new(0), ArrayElementType::Reference, 2)
                    .expect("room")
            };
            let mut kept: Vec<ObjectRef> = Vec::new();
            loop {
                let o = arr(&heap);
                kept.push(o);
                if o.as_ptr() as usize >= base + 2 * PAGE {
                    break;
                }
            }
            loop {
                let o = arr(&heap);
                kept.push(o);
                if o.as_ptr() as usize >= base + 6 * PAGE {
                    break;
                }
                let start = o.as_ptr() as usize;
                while heap.alloc_object(ClassId::new(1), FIELDS).as_ptr() as usize - start
                    < 128 * 1024
                {}
            }
            let n = kept.len();
            for i in 0..n {
                heap.set_array_element(kept[i], 0, Value::Object(Some(kept[(i + 1) % n])))
                    .unwrap();
                heap.set_array_element(kept[i], 1, Value::Object(Some(kept[(i * 7 + 3) % n])))
                    .unwrap();
            }
            let mut live: Vec<usize> = kept.iter().map(|o| o.as_ptr() as usize).collect();
            live.sort_unstable();
            let visit_all = |heap: &super::super::ZgcRealHeap, objs: &[usize]| {
                heap.set_mark_active(true);
                heap.color_flip_to_mark();
                for &b in objs {
                    heap.visit_strong_refs_at(b as *mut u8, None, &mut |_| {});
                }
                heap.set_mark_active(false);
                heap.color_flip_to_remap();
            };
            // ---- the closing mark, then the plan
            visit_all(&heap, &live);
            heap.counters.mark_root_filter.rebuild();
            let skip = heap.counters.rim_last_mark_color.load(Ordering::Acquire);
            let stride = cratonvm_types::ref_element_size();
            let not_skipped = live
                .iter()
                .flat_map(|&b| {
                    [0, 1].map(|k| (b + ARRAY_DATA_OFFSET + k * stride) as *const u64)
                })
                .filter(|&p| unsafe { *p } & super::super::vaddr::Z_METADATA_MASK != skip)
                .count();
            assert_eq!(not_skipped, 0, "every element in the mark color {skip:#x}");
            // The pin / overlay owners the filter answers "may own" for:
            // none in a fresh process, every base when a provider some
            // other test registered process-wide forces the overlay
            // answer. Only they are copied in the pause.
            let owners: std::collections::HashSet<usize> = live
                .iter()
                .copied()
                .filter(|&b| {
                    let (p, o) = heap.counters.mark_root_filter.extra_root_probe(b);
                    p || o
                })
                .collect();
            let (planned, map) = heap
                .concurrent_relocate_start(&|| live.clone())
                .expect("a plan");
            heap.counters.mark_root_filter.disarm();
            assert!(planned > 0, "the sparse regions were planned");
            let planned_owners = map.iter().filter(|(k, _)| owners.contains(k)).count();
            let r = heap.concurrent_relocate_report();
            assert_eq!((r.cycles, r.planned), (1, planned));
            assert_eq!(
                r.eager, planned_owners,
                "every element left in the mark color: only the owners are eager"
            );
            // ---- the roots: the map's lookups copy their objects
            let now: Vec<usize> = kept
                .iter()
                .map(|o| {
                    map.get(&(o.as_ptr() as usize))
                        .copied()
                        .unwrap_or(o.as_ptr() as usize)
                })
                .collect();
            let moved = (0..n)
                .filter(|&i| now[i] != kept[i].as_ptr() as usize)
                .count();
            assert_eq!(moved, planned, "the map names every planned object");
            assert_eq!(
                heap.concurrent_relocate_report().hit,
                planned - planned_owners,
                "each lookup copied what the pause had not"
            );
            // ---- the mutator interval: element 0, barriered
            for i in 0..n {
                let at = unsafe { ObjectRef::from_raw(now[i] as *mut u8) };
                match heap.get_array_element(at, 0) {
                    Ok(Value::Object(Some(t))) => {
                        assert_eq!(t.as_ptr() as usize, now[(i + 1) % n], "element 0 of {i}");
                    }
                    other => panic!("element 0 of {i}: {other:?}"),
                }
            }
            let _ = heap.run_concurrent_relocate_drain();
            // ---- the next mark heals element 1; its end releases
            let mut sorted = now.clone();
            sorted.sort_unstable();
            visit_all(&heap, &sorted);
            heap.release_evac_quarantine();
            let r = heap.concurrent_relocate_report();
            assert!(r.released_bytes > 0, "the regions went back");
            assert!(!heap.counters.crl.published(), "withdrawn");
            let stride = cratonvm_types::ref_element_size();
            for i in 0..n {
                let data = unsafe { (now[i] as *mut u8).add(ARRAY_DATA_OFFSET) };
                let e0 = unsafe { cratonvm_types::narrow_oop::read_ref_slot(data) };
                let e1 = unsafe { cratonvm_types::narrow_oop::read_ref_slot(data.add(stride)) };
                assert_eq!(e0 as usize, now[(i + 1) % n], "raw element 0 of {i}");
                assert_eq!(e1 as usize, now[(i * 7 + 3) % n], "raw element 1 of {i}");
                assert!(
                    heap.registry.contains(now[i]),
                    "{i} registered at its new base"
                );
                if now[i] != kept[i].as_ptr() as usize {
                    assert!(
                        !heap.registry.contains(kept[i].as_ptr() as usize),
                        "{i}: the old base left the registry"
                    );
                }
            }
        },
    );
}

/// zt t33: the plan walks only the legacy holders. Positive controls of
/// the walk check (`CRATONVM_DBG_ZGC_CRL_WALK_CHECK`): a plain element
/// planted after the closing mark is found as `Plain`; with the legacy
/// pass skipped (`CRATONVM_DBG_ZGC_CRL_NO_LEGACY_PASS`) a legacy cell
/// naming a planned object is found as `Legacy`; with the pass on, the
/// check finds no legacy cell and the cells name the planned addresses.
#[test]
fn zt33_the_walk_check_finds_what_the_legacy_pass_leaves() {
    use cratonvm_gc_core::collector::GarbageCollector;
    use cratonvm_gc_core::heap::ArrayElementType;
    use cratonvm_types::{ClassId, ObjectRef, Value, ARRAY_DATA_OFFSET};
    const PAGE: usize = super::super::ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
    // No layout is ever registered for this class: its objects are legacy.
    const LEGACY_CLASS: u32 = 0x00f3_3333;
    let _colored = crate::zgc::tests::colored_arming_test_lock();
    let run = |skip_legacy_pass: bool| -> (ZConcRelocReport, usize, usize) {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_REMAP_IN_MARK", Some("1"))],
            || {
                let heap = super::super::ZgcRealHeap::with_capacity(64 * 1024 * 1024);
                heap.set_tlab_enabled(false);
                assert!(heap.arm_load_barrier_colored_stores());
                assert!(heap.arm_load_barrier_color_flip());
                heap.counters.crl.set_enabled_for_test(true);
                heap.counters
                    .crl
                    .set_walk_check_for_test(true, skip_legacy_pass);
                let base = heap.arena.lock().base_ptr() as usize;
                let arr = |heap: &super::super::ZgcRealHeap| {
                    heap.try_alloc_array(ClassId::new(0), ArrayElementType::Reference, 2)
                        .expect("room")
                };
                // Sparse survivors over pages 2..6 (planned), dense below.
                let mut kept: Vec<ObjectRef> = Vec::new();
                loop {
                    let o = arr(&heap);
                    kept.push(o);
                    if o.as_ptr() as usize >= base + 2 * PAGE {
                        break;
                    }
                }
                loop {
                    let o = arr(&heap);
                    kept.push(o);
                    if o.as_ptr() as usize >= base + 6 * PAGE {
                        break;
                    }
                    let start = o.as_ptr() as usize;
                    while heap.alloc_object(ClassId::new(1), 500).as_ptr() as usize - start
                        < 128 * 1024
                    {}
                }
                let n = kept.len();
                // One legacy holder naming every survivor.
                let holder = heap.alloc_object(ClassId::new(LEGACY_CLASS), n);
                assert!(
                    !cratonvm_types::is_compact_object(heap.header_ref(holder.as_ptr())),
                    "the holder is legacy-shaped"
                );
                for i in 0..n {
                    heap.set_array_element(kept[i], 0, Value::Object(Some(kept[(i + 1) % n])))
                        .unwrap();
                    heap.set_field(holder, i, Value::Object(Some(kept[i])));
                }
                let mut live: Vec<usize> = kept.iter().map(|o| o.as_ptr() as usize).collect();
                live.push(holder.as_ptr() as usize);
                live.sort_unstable();
                // The closing mark.
                heap.set_mark_active(true);
                heap.color_flip_to_mark();
                for &b in &live {
                    heap.visit_strong_refs_at(b as *mut u8, None, &mut |_| {});
                }
                heap.set_mark_active(false);
                heap.color_flip_to_remap();
                heap.counters.mark_root_filter.rebuild();
                // A plain element 1 in every array, as a raw writer would
                // leave it after the mark.
                let stride = cratonvm_types::ref_element_size();
                for i in 0..n {
                    let slot =
                        (kept[i].as_ptr() as usize + ARRAY_DATA_OFFSET + stride) as *mut u8;
                    let t = kept[(i * 7 + 3) % n].as_ptr() as u64;
                    unsafe { std::ptr::write(slot as *mut u64, t) };
                }
                let (planned, map) = heap
                    .concurrent_relocate_start(&|| live.clone())
                    .expect("a plan");
                heap.counters.mark_root_filter.disarm();
                assert!(planned > 0, "the sparse regions were planned");
                let plain_expected = (0..n)
                    .filter(|&i| {
                        map.get(&(kept[(i * 7 + 3) % n].as_ptr() as usize))
                            .is_some()
                    })
                    .count();
                let legacy_expected = (0..n)
                    .filter(|&i| map.get(&(kept[i].as_ptr() as usize)).is_some())
                    .count();
                // Every cell and element names the planned address now.
                for i in 0..n {
                    let slot =
                        (kept[i].as_ptr() as usize + ARRAY_DATA_OFFSET + stride) as *const u64;
                    let t = kept[(i * 7 + 3) % n].as_ptr() as usize;
                    let want = map.get(&t).copied().unwrap_or(t);
                    assert_eq!(unsafe { *slot } as usize, want, "element 1 of {i}");
                }
                let h_now = map
                    .get(&(holder.as_ptr() as usize))
                    .copied()
                    .unwrap_or(holder.as_ptr() as usize);
                let h_now = unsafe { ObjectRef::from_raw(h_now as *mut u8) };
                for i in 0..n {
                    let t = kept[i].as_ptr() as usize;
                    let want = map.get(&t).copied().unwrap_or(t);
                    match heap.get_field(h_now, i) {
                        Value::Object(Some(o)) => {
                            assert_eq!(o.as_ptr() as usize, want, "cell {i}")
                        }
                        other => panic!("cell {i}: {other:?}"),
                    }
                }
                (
                    heap.concurrent_relocate_report(),
                    plain_expected,
                    legacy_expected,
                )
            },
        )
    };
    let (r, plain, legacy) = run(false);
    assert_eq!((r.walk_checked, r.legacy_holders), (1, 1));
    assert!(plain > 0 && legacy > 0, "planned targets on both sides");
    assert_eq!(r.walk_missed, [plain, 0, 0], "the pass left no legacy cell");
    let (r, plain, legacy) = run(true);
    assert_eq!(
        r.walk_missed,
        [plain, 0, legacy],
        "without the pass the check finds every planned cell"
    );
}

/// zt t33: from the closing pause's remap flip until its end, a store
/// writes the mark color the flip left (bad after the release), when R6
/// may plan; the pause's end and a slide give the good color back. Off
/// R6 the flip's heal color is the good color, as before.
#[test]
fn zt33_the_closing_pause_stores_the_mark_color() {
    use super::super::vaddr::{Z_METADATA_MASK, Z_REMAPPED};
    use cratonvm_gc_core::collector::GarbageCollector;
    use cratonvm_gc_core::heap::ArrayElementType;
    use cratonvm_types::{ClassId, Value, ARRAY_DATA_OFFSET};
    let _colored = crate::zgc::tests::colored_arming_test_lock();
    let color_after_store = |plan: bool| -> (u64, u64, u64) {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_REMAP_IN_MARK", Some("1"))],
            || {
                let heap = super::super::ZgcRealHeap::with_capacity(16 * 1024 * 1024);
                heap.set_tlab_enabled(false);
                assert!(heap.arm_load_barrier_colored_stores());
                assert!(heap.arm_load_barrier_color_flip());
                heap.counters.crl.set_enabled_for_test(plan);
                let a = heap
                    .try_alloc_array(ClassId::new(0), ArrayElementType::Reference, 1)
                    .expect("room");
                let b = heap
                    .try_alloc_array(ClassId::new(0), ArrayElementType::Reference, 1)
                    .expect("room");
                let slot = (a.as_ptr() as usize + ARRAY_DATA_OFFSET) as *const u64;
                let store = || {
                    heap.set_array_element(a, 0, Value::Object(Some(b)))
                        .unwrap();
                    (unsafe { *slot }) & Z_METADATA_MASK
                };
                heap.set_mark_active(true);
                heap.color_flip_to_mark();
                let marked = store();
                heap.set_mark_active(false);
                heap.color_flip_to_remap();
                let closing = store();
                heap.end_closing_heal_color();
                let after = store();
                assert_ne!(marked, Z_REMAPPED, "the mark stores its color");
                (marked, closing, after)
            },
        )
    };
    let (marked, closing, after) = color_after_store(true);
    assert_eq!(
        closing, marked,
        "the closing pause stores the leaving mark color"
    );
    assert_eq!(
        after, Z_REMAPPED,
        "the pause's end gives the good color back"
    );
    let (_, closing, after) = color_after_store(false);
    assert_eq!(
        (closing, after),
        (Z_REMAPPED, Z_REMAPPED),
        "off R6: the good color"
    );
}

/// zt t33: a pause whose relocation refused leaves its mark color unread;
/// the next cycle marked stop-the-world (no mark flip, so the remap flip
/// is a noop) must not plan by it. Before the fix the plan read the stale
/// color and planned objects whose words the marker had healed to
/// Remapped, which reads good: round stress lost weak referents.
#[test]
fn zt33_a_stop_the_world_mark_does_not_inherit_a_stale_mark_color() {
    let _colored = crate::zgc::tests::colored_arming_test_lock();
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_ZGC_REMAP_IN_MARK", Some("1"))],
        || {
            let heap = super::super::ZgcRealHeap::with_capacity(16 * 1024 * 1024);
            heap.set_tlab_enabled(false);
            assert!(heap.arm_load_barrier_colored_stores());
            assert!(heap.arm_load_barrier_color_flip());
            heap.counters.crl.set_enabled_for_test(true);
            heap.set_mark_active(true);
            heap.color_flip_to_mark();
            heap.set_mark_active(false);
            heap.color_flip_to_remap();
            assert_ne!(
                heap.counters.rim_last_mark_color.load(Ordering::Acquire),
                0,
                "a concurrent mark leaves its color"
            );
            heap.end_closing_heal_color();
            // The refused pause never slid; the next mark is stop-the-world.
            heap.color_flip_to_remap();
            assert_eq!(
                heap.counters.rim_last_mark_color.load(Ordering::Acquire),
                0,
                "a noop flip leaves no mark color"
            );
            assert!(heap.concurrent_relocate_start(&Vec::new).is_none());
            assert_eq!(
                heap.counters.crl.last_refusal.load(Ordering::Relaxed),
                ZConcRelocRefusal::NoMarkColor as u8
            );
        },
    );
}

/// zt t34: the mark books a straddling object onto every region it
/// reaches, and a blackened range as live wherever it lies.
#[test]
fn zt34_the_mark_books_objects_and_ranges_by_region() {
    const R: usize = ZRELOC_REGION_BYTES;
    let base = 0x1000_0000usize;
    let live = ZMarkLiveness::new(true, base, 4 * R);
    live.arm();
    assert!(live.armed());
    live.note_object(base + 64, Some(48), false);
    // From region 0 into region 2.
    live.note_object(base + R - 16, Some(R + 32), true);
    live.note_range(base + 3 * R - 8, base + 3 * R + 100);
    live.note_object(base + 3 * R + 200, None, false);
    let grid = ZRegionGrid::new(base, base + 4 * R, R).expect("grid");
    let m = live.take(&grid).expect("armed");
    assert!(!live.armed(), "taking disarms");
    assert_eq!(m.liveness.live_bytes, vec![48 + R + 32, 0, 8, 100]);
    assert_eq!(m.liveness.live_objects, vec![2, 0, 1, 1]);
    assert_eq!(m.liveness.straddle_in_bytes, vec![0, R, 16, 100]);
    assert_eq!(m.liveness.unsizable, 1);
    assert_eq!(m.legacy, vec![base + R - 16]);
    assert_eq!(m.black, vec![(base + 3 * R - 8, base + 3 * R + 100)]);
    assert!(live.take(&grid).is_none(), "nothing kept after a take");
    live.arm();
    assert_eq!(live.take(&grid).expect("armed").legacy, Vec::<usize>::new());
}

/// zt t34: the plan from the mark's liveness selects what t33's sizing
/// loop selects on the ring heap, and finds the legacy holders without a
/// header scan: one the mark claimed, one born black in a blackened
/// range. The liveness check reads 0 under-estimated regions and 0
/// missed holders, and every cell of both holders names the planned
/// address after the plan.
#[test]
fn zt34_the_plan_from_the_mark_matches_the_sizing_plan() {
    use cratonvm_gc_core::collector::GarbageCollector;
    use cratonvm_gc_core::heap::ArrayElementType;
    use cratonvm_types::{ClassId, ObjectRef, Value};
    const PAGE: usize = super::super::ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
    const LEGACY_CLASS: u32 = 0x00f3_4444;
    let _colored = crate::zgc::tests::colored_arming_test_lock();
    let run = |from_mark: bool| -> (Vec<usize>, ZConcRelocReport) {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_REMAP_IN_MARK", Some("1"))],
            || {
                let heap = super::super::ZgcRealHeap::with_capacity(64 * 1024 * 1024);
                heap.set_tlab_enabled(false);
                assert!(heap.arm_load_barrier_colored_stores());
                assert!(heap.arm_load_barrier_color_flip());
                heap.counters.crl.set_enabled_for_test(true);
                heap.counters.crl.set_liveness_check_for_test(true);
                let base = heap.arena.lock().base_ptr() as usize;
                let arr = |heap: &super::super::ZgcRealHeap| {
                    heap.try_alloc_array(ClassId::new(0), ArrayElementType::Reference, 2)
                        .expect("room")
                };
                let mut kept: Vec<ObjectRef> = Vec::new();
                loop {
                    let o = arr(&heap);
                    kept.push(o);
                    if o.as_ptr() as usize >= base + 2 * PAGE {
                        break;
                    }
                }
                loop {
                    let o = arr(&heap);
                    kept.push(o);
                    if o.as_ptr() as usize >= base + 6 * PAGE {
                        break;
                    }
                    let start = o.as_ptr() as usize;
                    while heap.alloc_object(ClassId::new(1), 500).as_ptr() as usize - start
                        < 128 * 1024
                    {}
                }
                let n = kept.len();
                let claimed = heap.alloc_object(ClassId::new(LEGACY_CLASS), n);
                let born = heap.alloc_object(ClassId::new(LEGACY_CLASS), n);
                for i in 0..n {
                    heap.set_array_element(kept[i], 0, Value::Object(Some(kept[(i + 1) % n])))
                        .unwrap();
                    heap.set_field(claimed, i, Value::Object(Some(kept[i])));
                    heap.set_field(born, i, Value::Object(Some(kept[(i + 5) % n])));
                }
                let mut live: Vec<usize> = kept.iter().map(|o| o.as_ptr() as usize).collect();
                live.push(claimed.as_ptr() as usize);
                live.push(born.as_ptr() as usize);
                live.sort_unstable();
                // The closing mark: claims (booked when armed) and heals;
                // `born` is blackened as a TLAB chunk would be.
                if from_mark {
                    heap.crl_liveness_arm_for_mark();
                    assert!(heap.counters.crl.live.armed(), "R6 may plan: armed");
                }
                heap.set_mark_active(true);
                heap.color_flip_to_mark();
                let born_at = born.as_ptr() as usize;
                let born_end = born_at
                    + heap
                        .live_extent(born_at, heap.header_ref(born.as_ptr()))
                        .expect("sized");
                for &b in &live {
                    if b == born_at {
                        assert!(heap.blacken_range(born_at, born_end));
                    } else {
                        assert!(heap.mark_claim(b));
                    }
                    heap.visit_strong_refs_at(b as *mut u8, None, &mut |_| {});
                }
                heap.set_mark_active(false);
                heap.color_flip_to_remap();
                // The sweep's registry prune (`starts &= marks`): the plan
                // runs after it, so the registry is the live set.
                heap.registry
                    .retain_marked(heap.mark_bits.as_ref().expect("bitmap arm"));
                heap.counters.mark_root_filter.rebuild();
                let (planned, map) = heap
                    .concurrent_relocate_start(&|| live.clone())
                    .unwrap_or_else(|| {
                        panic!(
                            "no plan (mark={from_mark}): refusal {}",
                            heap.counters.crl.last_refusal.load(Ordering::Relaxed)
                        )
                    });
                heap.counters.mark_root_filter.disarm();
                assert!(planned > 0, "the sparse regions were planned");
                let froms: Vec<usize> = live
                    .iter()
                    .copied()
                    .filter(|b| map.get(b).is_some())
                    .collect();
                assert_eq!(froms.len(), planned);
                for (holder, shift) in [(claimed, 0usize), (born, 5)] {
                    let h = holder.as_ptr() as usize;
                    let h_now = map.get(&h).copied().unwrap_or(h);
                    let h_now = unsafe { ObjectRef::from_raw(h_now as *mut u8) };
                    for i in 0..n {
                        let t = kept[(i + shift) % n].as_ptr() as usize;
                        let want = map.get(&t).copied().unwrap_or(t);
                        match heap.get_field(h_now, i) {
                            Value::Object(Some(o)) => {
                                assert_eq!(o.as_ptr() as usize, want, "cell {i} of {h:#x}")
                            }
                            other => panic!("cell {i}: {other:?}"),
                        }
                    }
                }
                // Arena offsets: the two heaps are laid out alike.
                let froms = froms.iter().map(|&b| b - base).collect();
                (froms, heap.concurrent_relocate_report())
            },
        )
    };
    let (walk_set, r_walk) = run(false);
    let (mark_set, r_mark) = run(true);
    assert_eq!((r_walk.mark_plans, r_mark.mark_plans), (0, 1));
    assert_eq!(r_mark.check, [1, 0, 0], "the estimate bounds the exact figures");
    assert_eq!(r_mark.black_legacy, 1, "the holder born black was found");
    assert_eq!((r_walk.legacy_holders, r_mark.legacy_holders), (2, 2));
    assert_eq!(mark_set, walk_set, "the same survivors planned");
}

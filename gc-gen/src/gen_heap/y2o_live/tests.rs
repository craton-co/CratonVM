use std::sync::atomic::Ordering;
use super::*;
use cratonvm_gc_core::heap::{ArrayElementType, GC_FLAG_OLD_GEN, HEADER_SIZE, SLOT_SIZE};
use cratonvm_types::{ClassId, ObjectRef, Value};

/// A heap with room for a handful of objects in each generation.
fn heap() -> GenerationalHeap {
    GenerationalHeap::with_sizes(64 * 1024, 64 * 1024)
}

/// An old-generation object of `slots` legacy reference cells, allocated
/// directly (as selective promotion would place it).
fn old_object(heap: &GenerationalHeap, class: u32, slots: u32) -> ObjectRef {
    let size = HEADER_SIZE + slots as usize * SLOT_SIZE;
    let mut og = heap.old_gen_lock();
    let p = og.alloc(size, 8).expect("old-gen room");
    // SAFETY: a freshly allocated (zeroed) old-gen block of `size` bytes.
    unsafe {
        let h = &mut *(p as *mut ObjectHeader);
        h.class_id = ClassId::new(class);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(slots);
        h.set_gc_flags(GC_FLAG_OLD_GEN);
        ObjectRef::from_raw(p)
    }
}

fn addr(r: ObjectRef) -> usize {
    r.as_ptr() as usize
}

fn request<'a>(
    roots: &'a [usize],
    hidden: &'a [usize],
    live_only: bool,
) -> YoungToOldSeedRequest<'a> {
    YoungToOldSeedRequest {
        roots,
        extra_live: &[],
        hidden_young_refs: hidden,
        live_only,
    }
}

/// Owner membership must survive duplicate class rows, and each pause
/// must observe removals instead of reusing a previous address set. A
/// zero-headed owner cannot be resolved through the young object grid:
/// its side-table membership must still force the conservative fallback.
#[test]
fn loader_owner_fallback_tracks_duplicate_rows_and_fresh_pause_snapshots() {
    let heap = heap();
    let owner = heap.alloc_object(ClassId::new(1), 2);
    let _after = heap.alloc_object(ClassId::new(1), 0);
    // SAFETY: exactly this young allocation's initialized extent. Turn
    // it into the zero-headed stretch that owner admission must refuse.
    unsafe {
        std::ptr::write_bytes(owner.as_ptr(), 0, HEADER_SIZE + 2 * SLOT_SIZE);
    }
    let roots = [addr(owner)];
    let vm = addr(owner);
    const FIRST: u32 = 0x7f4301;
    const SECOND: u32 = 0x7f4302;
    struct Pins(usize);
    impl Drop for Pins {
        fn drop(&mut self) {
            cratonvm_types::loader_pin::forget_vm_loader_pins(self.0);
        }
    }
    let _pins = Pins(vm);
    let collect = || heap.collect_young_to_old_seeds(&request(&roots, &[], true));
    assert!(
        collect().from_live_set,
        "an unowned empty stretch is harmless"
    );

    cratonvm_types::loader_pin::set_loader_pin(vm, FIRST, addr(owner));
    cratonvm_types::loader_pin::set_loader_pin(vm, SECOND, addr(owner));
    let with_both = collect();
    assert!(!with_both.from_live_set);
    assert_eq!(
        with_both.fallback,
        Some(Y2oFallback::UnresolvedOwnerCandidate)
    );

    cratonvm_types::loader_pin::remove_loader_pin(vm, FIRST);
    assert_eq!(
        collect().fallback,
        Some(Y2oFallback::UnresolvedOwnerCandidate),
        "one remaining class still owns the loader"
    );
    cratonvm_types::loader_pin::remove_loader_pin(vm, SECOND);
    assert!(
        collect().from_live_set,
        "a new pause must discard stale membership"
    );
}

/// THE PAGE'S CLAIM: a dead young object's old referent is not seeded, a
/// live one's is — and the all-young enumeration (the default, and the
/// fallback) seeds both, exactly as `collect_young_to_old_roots` does.
#[test]
fn a_dead_young_objects_old_referent_is_not_seeded_and_a_live_ones_is() {
    let heap = heap();
    let dead_target = old_object(&heap, 2, 0);
    let live_target = old_object(&heap, 2, 0);
    let live = heap.alloc_object(ClassId::new(1), 1);
    let dead = heap.alloc_object(ClassId::new(1), 1);
    heap.set_field(live, 0, Value::Object(Some(live_target)));
    heap.set_field(dead, 0, Value::Object(Some(dead_target)));

    let roots = [addr(live)];
    let seeds = heap.collect_young_to_old_seeds(&request(&roots, &[], true));
    assert!(seeds.from_live_set, "fell back: {:?}", seeds.fallback);
    assert!(
        seeds.old_refs.contains(&addr(live_target)),
        "the live holder seeds"
    );
    assert!(
        !seeds.old_refs.contains(&addr(dead_target)),
        "a young object nothing reaches must not keep its old referent"
    );
    assert_eq!(seeds.seeding_objects, 1);

    let all = heap.collect_young_to_old_seeds(&request(&roots, &[], false));
    assert!(!all.from_live_set);
    assert!(all.old_refs.contains(&addr(live_target)));
    assert!(all.old_refs.contains(&addr(dead_target)));
    let mut legacy = heap.collect_young_to_old_roots();
    let mut ours = all.old_refs.clone();
    legacy.sort_unstable();
    ours.sort_unstable();
    assert_eq!(ours, legacy, "flag off: the same seeds as before");
}

/// A side-table-only young mirror must not feed its defining old loader
/// back into a concurrent unload mark, while other references in that
/// mirror still contribute Y2O seeds. Outside that narrowly licensed
/// snapshot, ordinary minor-GC pin semantics keep both edges.
#[test]
fn concurrent_unload_mirror_seed_omits_only_the_old_owner_edge() {
    let heap = heap();
    let loader = old_object(&heap, 2, 0);
    let target = old_object(&heap, 2, 0);
    let mirror = heap.alloc_object(ClassId::new(1), 2);
    heap.set_field(mirror, 0, Value::Object(Some(loader)));
    heap.set_field(mirror, 1, Value::Object(Some(target)));
    let vm = addr(mirror);
    struct Pins(usize);
    impl Drop for Pins {
        fn drop(&mut self) {
            cratonvm_types::mirror_pin::forget_vm_mirror_pins(self.0);
        }
    }
    let _pins = Pins(vm);
    cratonvm_types::mirror_pin::add_mirror_pin(vm, addr(loader), addr(mirror));

    let seeds = cratonvm_gc_core::gc_quiescence::with_class_unload_marking(|| {
        heap.collect_young_to_old_seeds(&request(&[], &[], true))
    });
    assert!(seeds.from_live_set, "fell back: {:?}", seeds.fallback);
    assert!(
        !seeds.old_refs.contains(&addr(loader)),
        "a side-table-only mirror must not retain its old owner"
    );
    assert!(
        seeds.old_refs.contains(&addr(target)),
        "other mirror referents remain seeded"
    );

    let roots = [addr(mirror)];
    let rooted = cratonvm_gc_core::gc_quiescence::with_class_unload_marking(|| {
        heap.collect_young_to_old_seeds(&request(&roots, &[], true))
    });
    assert!(rooted.from_live_set, "fell back: {:?}", rooted.fallback);
    assert!(
        rooted.old_refs.contains(&addr(loader)),
        "a mirror reached from the pause's true roots keeps its defining loader"
    );

    let ordinary = heap.collect_young_to_old_seeds(&request(&[], &[], true));
    assert!(ordinary.from_live_set, "fell back: {:?}", ordinary.fallback);
    assert!(ordinary.old_refs.contains(&addr(loader)));
    assert!(ordinary.old_refs.contains(&addr(target)));
}

/// A conservative INTERIOR root keeps the object it points into, and the
/// young objects that one reaches keep their old referents too.
#[test]
fn an_interior_root_and_its_young_closure_seed() {
    let heap = heap();
    let target = old_object(&heap, 2, 0);
    let inner = heap.alloc_object(ClassId::new(1), 1);
    let outer = heap.alloc_object(ClassId::new(1), 2);
    heap.set_field(inner, 0, Value::Object(Some(target)));
    heap.set_field(outer, 1, Value::Object(Some(inner)));

    let roots = [addr(outer) + HEADER_SIZE + 8];
    let seeds = heap.collect_young_to_old_seeds(&request(&roots, &[], true));
    assert!(seeds.from_live_set, "fell back: {:?}", seeds.fallback);
    assert!(seeds.old_refs.contains(&addr(target)));
}

/// Minor-GC semantics for the old generation: a young object whose only
/// holder is OLD is live (every old object is scanned, not only the dirty
/// cards), and so its own old referent is seeded; `extra_live` (the
/// finalizable objects, the JNI weak referents) seeds like a root.
#[test]
fn a_young_object_held_from_old_or_passed_as_extra_live_seeds() {
    let heap = heap();
    let holder = old_object(&heap, 2, 1);
    let target_a = old_object(&heap, 2, 0);
    let target_b = old_object(&heap, 2, 0);
    let via_old = heap.alloc_object(ClassId::new(1), 1);
    let finalizable = heap.alloc_object(ClassId::new(1), 1);
    heap.set_field(via_old, 0, Value::Object(Some(target_a)));
    heap.set_field(finalizable, 0, Value::Object(Some(target_b)));
    heap.set_field(holder, 0, Value::Object(Some(via_old)));

    let extra = [addr(finalizable)];
    let req = YoungToOldSeedRequest {
        roots: &[],
        extra_live: &extra,
        hidden_young_refs: &[],
        live_only: true,
    };
    let seeds = heap.collect_young_to_old_seeds(&req);
    assert!(seeds.from_live_set, "fell back: {:?}", seeds.fallback);
    assert!(
        seeds.old_refs.contains(&addr(target_a)),
        "held from an old object"
    );
    assert!(
        seeds.old_refs.contains(&addr(target_b)),
        "an extra live object"
    );
}

/// A listed young Reference's weak referent edge cannot retain an old
/// object through a young intermediate. The target and its descendants
/// remain live when another strong path reaches them.
#[test]
fn a_hidden_young_reference_does_not_seed_its_old_referent() {
    let heap = heap();
    let referent = old_object(&heap, 2, 0);
    let queue = old_object(&heap, 2, 0);
    let reference = heap.alloc_object(ClassId::new(1), 2);
    heap.set_field(reference, 0, Value::Object(Some(referent)));
    heap.set_field(reference, 1, Value::Object(Some(queue)));

    let roots = [addr(reference)];
    let hidden = [addr(reference)];
    for live_only in [true, false] {
        let seeds = heap.collect_young_to_old_seeds(&request(&roots, &hidden, live_only));
        assert!(
            !seeds.old_refs.contains(&addr(referent)),
            "live_only={live_only}"
        );
        assert!(
            seeds.old_refs.contains(&addr(queue)),
            "live_only={live_only}"
        );
        assert_eq!(seeds.hidden_referents_skipped, 1);
        let open = heap.collect_young_to_old_seeds(&request(&roots, &[], live_only));
        assert!(
            open.old_refs.contains(&addr(referent)),
            "not listed: seeded"
        );
    }

    // A young referent is not made strongly reachable by its weak slot,
    // so its old referent is not seeded through that weak-only path.
    let far = old_object(&heap, 2, 0);
    let young_referent = heap.alloc_object(ClassId::new(1), 1);
    heap.set_field(young_referent, 0, Value::Object(Some(far)));
    heap.set_field(reference, 0, Value::Object(Some(young_referent)));
    let seeds = heap.collect_young_to_old_seeds(&request(&roots, &hidden, true));
    assert!(seeds.from_live_set, "fell back: {:?}", seeds.fallback);
    assert!(!seeds.old_refs.contains(&addr(far)));
    assert_eq!(seeds.hidden_referents_skipped, 0);

    let strong_roots = [addr(reference), addr(young_referent)];
    let strongly_reached = heap.collect_young_to_old_seeds(&request(&strong_roots, &hidden, true));
    assert!(strongly_reached.from_live_set, "fell back: {:?}", strongly_reached.fallback);
    assert!(strongly_reached.old_refs.contains(&addr(far)));
}

/// THE DEFAULT-PATH FIX: a young object the walk cannot parse (here an
/// implausible `num_slots`) is dropped from the enumeration together with
/// everything up to the next free block. `collect_young_to_old_roots`
/// (every concurrent pause, flag or not) now word-scans that stretch, as
/// the stop-the-world seed always did, so the old object only that young
/// holder names is still seeded; the live path declines (its grid is not
/// complete) and says so.
#[test]
fn the_concurrent_seed_word_scans_an_unparseable_young_stretch() {
    let heap = heap();
    let target = old_object(&heap, 2, 0);
    let holder = heap.alloc_object(ClassId::new(1), 1);
    heap.set_field(holder, 0, Value::Object(Some(target)));
    let _after = heap.alloc_object(ClassId::new(1), 0);
    // SAFETY: `holder` is a live young object; only its slot count is
    // rewritten, to a value every walk rejects as implausible.
    unsafe { (*(holder.as_ptr() as *mut ObjectHeader)).set_num_slots(1 << 30) };

    let (objs, stretches) = heap.walk_young_objects_and_gaps();
    assert!(
        !stretches.is_empty(),
        "precondition: the walk met an anomaly"
    );
    assert!(
        !objs.iter().any(|&(p, _)| p == holder.as_ptr()),
        "precondition: the unparseable holder is not enumerated"
    );
    assert!(
        heap.collect_young_to_old_roots().contains(&addr(target)),
        "the stretch's words must seed the old object its holder names"
    );

    let roots = [addr(holder)];
    let seeds = heap.collect_young_to_old_seeds(&request(&roots, &[], true));
    assert!(!seeds.from_live_set);
    assert_eq!(seeds.fallback, Some(Y2oFallback::YoungWalkAnomaly));
    assert!(!seeds.young_walk_complete, "the class ids are incomplete");
    assert!(seeds.old_refs.contains(&addr(target)));
}

/// gcd d1/c (`gengc-r5w6-conc10-remark-weak-refs-to-young-objects-behind-a-retained-finalizable`):
/// an old "finalizable" F whose slot names young R, R naming young R2, and
/// nothing else naming either: with F excluded from the strong holders, R
/// and R2 are reported as reachable only through F; a young object that a
/// root (or a strong old holder) also reaches is not; with F a strong
/// holder nothing is reported.
#[test]
fn gcd_d1c_young_objects_behind_a_retained_holder_are_reported() {
    let heap = heap();
    let fin = old_object(&heap, 2, 3);
    let strong_old = old_object(&heap, 2, 1);
    let r = heap.alloc_object(ClassId::new(1), 1);
    let r2 = heap.alloc_object(ClassId::new(1), 0);
    let shared_with_root = heap.alloc_object(ClassId::new(1), 0);
    let shared_with_old = heap.alloc_object(ClassId::new(1), 0);
    heap.set_field(r, 0, Value::Object(Some(r2)));
    heap.set_field(fin, 0, Value::Object(Some(r)));
    heap.set_field(fin, 1, Value::Object(Some(shared_with_root)));
    heap.set_field(fin, 2, Value::Object(Some(shared_with_old)));
    heap.set_field(strong_old, 0, Value::Object(Some(shared_with_old)));

    let roots = [addr(shared_with_root)];
    let through = [addr(fin)];
    let not_fin = |a: usize| a != addr(fin);
    let mut only = heap
        .young_reached_only_through(&roots, &[], &not_fin, &through)
        .expect("the live marking is complete");
    only.sort_unstable();
    let mut want = vec![addr(r), addr(r2)];
    want.sort_unstable();
    assert_eq!(only, want);

    let every = |_: usize| true;
    let none = heap
        .young_reached_only_through(&roots, &[], &every, &through)
        .expect("complete");
    assert!(none.is_empty(), "{none:x?}");
    let nothing = heap
        .young_reached_only_through(&roots, &[], &not_fin, &[])
        .expect("complete");
    assert!(nothing.is_empty());
}

/// The class ids come from the seeding objects only on the live path (the
/// concurrent class unload's `young_instance_loaders`).
#[test]
fn the_class_ids_are_the_live_objects_classes() {
    let heap = heap();
    let live = heap.alloc_object(ClassId::new(11), 0);
    let _dead = heap.alloc_object(ClassId::new(12), 0);
    let roots = [addr(live)];
    let seeds = heap.collect_young_to_old_seeds(&request(&roots, &[], true));
    assert!(seeds.from_live_set, "fell back: {:?}", seeds.fallback);
    assert!(seeds.class_ids.contains(&11));
    assert!(!seeds.class_ids.contains(&12));
    let all = heap.collect_young_to_old_seeds(&request(&roots, &[], false));
    assert!(all.class_ids.contains(&11) && all.class_ids.contains(&12));
}

/// gch h3/mj (g4og step 0): the pure census -- duplicates count once, a
/// ref a root-reached young object also names is not the old holders'
/// share, and only an exact walked object start contributes bytes.
#[test]
fn gch_h3mj_old_holder_census_of_dedups_and_sizes_exact_starts() {
    let (a, b, c) = (0x1000usize, 0x2000usize, 0x3008usize);
    let walked = [
        (a as *mut u8, 32usize),
        (0x2000 as *mut u8, 24usize),
        (0x3000 as *mut u8, 48usize),
    ];
    let census = old_holder_census_of(&[a, a, b, c], &[b], &walked);
    assert_eq!(
        census,
        Y2oOldHolderCensus {
            seeded: 3,
            via_old_holders: 2,
            via_old_holders_bytes: 32,
        },
        "c is interior to the walked 0x3000 object: counted, not sized"
    );
    assert_eq!(
        old_holder_census_of(&[], &[b], &walked),
        Y2oOldHolderCensus::default()
    );
}

/// gch h3/mj (g4og step 0, `CRATONVM_GEN_CONC_Y2O_OLD_HOLDER_CENSUS`): an
/// old holder H names young C naming old T; a rooted young R names old
/// U. The live seed takes T and U (every old object is live to it); the
/// census says one of the two (T) is named only through an old holder,
/// with T's walked size. It runs only in a concurrent INITIAL MARK and
/// only under its switch, and never changes the seeds.
#[test]
fn gch_h3mj_the_old_holder_census_runs_at_an_initial_mark_under_its_switch_only() {
    let mut heap = heap();
    let holder = old_object(&heap, 2, 1);
    let via_holder = old_object(&heap, 2, 0);
    let via_root = old_object(&heap, 2, 0);
    let child = heap.alloc_object(ClassId::new(1), 1);
    let rooted = heap.alloc_object(ClassId::new(1), 1);
    heap.set_field(child, 0, Value::Object(Some(via_holder)));
    heap.set_field(rooted, 0, Value::Object(Some(via_root)));
    heap.set_field(holder, 0, Value::Object(Some(child)));
    let via_holder_size = {
        let og = heap.old_gen_lock();
        let (walked, _gaps) = og.walk_objects_with_gaps();
        walked
            .iter()
            .find(|&&(p, _)| p as usize == addr(via_holder))
            .map(|&(_, size)| size)
            .expect("the target is walked")
    };
    let state = std::sync::Arc::new(crate::concurrent_mark::ConcurrentGcState::new());
    heap.enable_concurrent_gc(
        std::sync::Arc::new(cratonvm_gc_core::satb::SatbQueue::new()),
        state.clone(),
    );
    let roots = [addr(rooted)];
    let run = |on: bool| {
        cratonvm_types::flags::with_thread_overrides(
            &[(Y2O_OLD_HOLDER_CENSUS_FLAG, on.then_some("1"))],
            || heap.collect_young_to_old_seeds(&request(&roots, &[], true)),
        )
    };
    let sorted = |s: &YoungToOldSeeds| {
        let mut v = s.old_refs.clone();
        v.sort_unstable();
        v.dedup();
        v
    };

    // No cycle open (Idle): nothing, switch or not.
    let idle = run(true);
    assert!(idle.from_live_set, "fell back: {:?}", idle.fallback);
    assert!(idle.old_holder_census.is_none());

    assert!(
        state.try_open_cycle(),
        "precondition: the initial mark is claimed"
    );
    let off = run(false);
    assert!(off.old_holder_census.is_none(), "default off");
    let on = run(true);
    assert!(on.from_live_set, "fell back: {:?}", on.fallback);
    assert_eq!(sorted(&on), sorted(&off), "the census never changes a seed");
    let mut want = vec![addr(via_holder), addr(via_root)];
    want.sort_unstable();
    assert_eq!(sorted(&on), want);
    assert_eq!(
        on.old_holder_census,
        Some(Ok(Y2oOldHolderCensus {
            seeded: 2,
            via_old_holders: 1,
            via_old_holders_bytes: via_holder_size,
        })),
    );
    state.note_y2o_seeds(&on, 0);
    let line = heap.concurrent_driver_census_line().expect("attached");
    let holders = cratonvm_types::flags::with_thread_overrides(
        &[(Y2O_OLD_HOLDER_CENSUS_FLAG, Some("1"))],
        || state.y2o_holder_census_line(),
    )
    .expect("the switch is on");
    assert_eq!(
        holders,
        format!(
            "[GC] conc_y2o_holders: census_initial_marks=1 census_fallbacks=0 \
             conc_y2o_old_seeded=2 conc_y2o_old_via_old_holders=1 \
             conc_y2o_old_via_old_holders_bytes={via_holder_size}"
        )
    );
    assert!(!line.contains("conc_y2o_holders"), "switch off: no line");
}


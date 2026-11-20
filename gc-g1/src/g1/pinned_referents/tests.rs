//! `cargo test -j 5 -p cratonvm-gc --lib gci_i27_gr_`.
use super::*;
use cratonvm_gc_core::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_types::{ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 16 * 1024 * 1024,
        initial_heap_size: 16 * 1024 * 1024,
        region_size: 1024 * 1024,
        promotion_age: 3,
        gc_worker_threads: 1,
        string_dedup_enabled: false,
        ..Default::default()
    })
}

fn region_of(gc: &G1Collector, obj: ObjectRef) -> usize {
    gc.lookup_region_for_addr(obj.as_ptr() as usize)
        .expect("fixture object in a region")
}

fn addr(obj: ObjectRef) -> usize {
    obj.as_ptr() as usize
}

/// Allocate a one-slot object in a region other than `avoid`.
fn alloc_outside(gc: &G1Collector, avoid: usize) -> ObjectRef {
    loop {
        let o = gc.alloc_object(ClassId::new(2), 1);
        if region_of(gc, o) != avoid {
            return o;
        }
    }
}

/// The page's fixture: in one pinned Eden region a dead referent, a
/// referent a rooted object of the same region holds, a dead self-cycle, a
/// referent held by a copied holder of another region, and one an interior
/// root word holds. After the pause the region is still young and in
/// place, and the verdict names exactly the dead two.
#[test]
fn gci_i27_gr_a_kept_region_s_unheld_referents_are_named_and_held_ones_are_not() {
    let gc = collector();
    let reference = gc.alloc_object(ClassId::new(1), 2); // the `Reference`, slot 0 nulled
    let pinned = region_of(&gc, reference);
    let dead = gc.alloc_object(ClassId::new(1), 1);
    let held_in_region = gc.alloc_object(ClassId::new(1), 1);
    let holder_in_region = gc.alloc_object(ClassId::new(1), 1);
    gc.set_field(holder_in_region, 0, Value::Object(Some(held_in_region)));
    let self_cycle = gc.alloc_object(ClassId::new(1), 1);
    gc.set_field(self_cycle, 0, Value::Object(Some(self_cycle)));
    let held_from_outside = gc.alloc_object(ClassId::new(1), 1);
    let interior = gc.alloc_object(ClassId::new(1), 2);
    for o in [dead, held_in_region, holder_in_region, self_cycle, held_from_outside, interior] {
        assert_eq!(region_of(&gc, o), pinned, "fixture: one Eden region");
    }
    let outside = alloc_outside(&gc, pinned);
    gc.set_field(outside, 0, Value::Object(Some(held_from_outside)));

    gc.pin_region(pinned);
    let mut roots = vec![reference, holder_in_region, outside];
    let result = gc.young_collection(&mut roots, &NoopMonitors);
    assert!(
        result.pointer_map.contains_key(&addr(outside)),
        "fixture: the outside holder was copied"
    );
    assert!(
        !result.pointer_map.contains_key(&addr(dead)),
        "fixture: the kept region's objects are not in the map"
    );
    assert!(gc.pinned_referent_verdict_armed(), "a pause that kept a region arms it");

    let candidates = [
        addr(reference),
        addr(dead),
        addr(held_in_region),
        addr(self_cycle),
        addr(held_from_outside),
        addr(interior),
    ];
    let mut root_words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
    root_words.push(addr(interior) + HEADER_SIZE + 8);
    let verdict = gc
        .pinned_young_referents_unheld_within(&candidates, &root_words, usize::MAX, false)
        .expect("a verdict");
    gc.unpin_region(pinned);
    assert_eq!(verdict.candidates, 6);
    assert_eq!(verdict.confirmed, 6);
    let expected: FxHashSet<usize> = [addr(dead), addr(self_cycle)].into_iter().collect();
    assert_eq!(verdict.unheld, expected);
}

/// gci i28/wt (`gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`):
/// the VM now offers ONE candidate list per pause for every weak consumer
/// (the reference rows and the JNI weak globals' referents). That is sound
/// only if a candidate's verdict does not depend on which other candidates
/// are offered: asked alone, in pairs, or with all the others, each
/// fixture object gets the same answer.
#[test]
fn gci_i28_wt_a_candidate_s_verdict_does_not_depend_on_the_others() {
    let gc = collector();
    let reference = gc.alloc_object(ClassId::new(1), 2);
    let pinned = region_of(&gc, reference);
    let dead = gc.alloc_object(ClassId::new(1), 1);
    let held_in_region = gc.alloc_object(ClassId::new(1), 1);
    let holder_in_region = gc.alloc_object(ClassId::new(1), 1);
    gc.set_field(holder_in_region, 0, Value::Object(Some(held_in_region)));
    // A dead holder still holds: `dead_holder` is garbage, its target is
    // over-retained whether or not `dead_holder` itself is a candidate.
    let dead_holder = gc.alloc_object(ClassId::new(1), 1);
    let held_by_garbage = gc.alloc_object(ClassId::new(1), 1);
    gc.set_field(dead_holder, 0, Value::Object(Some(held_by_garbage)));
    for o in [dead, held_in_region, holder_in_region, dead_holder, held_by_garbage] {
        assert_eq!(region_of(&gc, o), pinned, "fixture: one Eden region");
    }
    // gci i30/rp hand-back 1: a rooted young object OUTSIDE the pinned
    // region, so the pause has a non-empty collection set. With the pinned
    // region the only young region, the CSet is empty, the pause returns
    // before the record funnel (`young_collection_serial` /
    // `young_collection_parallel`, `if cset.is_empty()`), and nothing arms
    // the verdict -- by design (i27/gr: "a refused / empty-CSet pause
    // records nothing"). The i27 fixture does the same (`outside`). Since
    // gci i32/ec such a pause arms it too (`note_refproc_empty_cset_pause`,
    // test `gci_i32_ec_an_empty_cset_pause_...`); `outside` stays so this
    // test reads the record-funnel arm.
    let outside = alloc_outside(&gc, pinned);
    gc.pin_region(pinned);
    let mut roots = vec![reference, holder_in_region, outside];
    gc.young_collection(&mut roots, &NoopMonitors);
    assert!(
        gc.pinned_referent_verdict_armed(),
        "a pause with a collection set that kept a region arms it"
    );
    let root_words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();

    let all = [
        addr(reference),
        addr(dead),
        addr(held_in_region),
        addr(dead_holder),
        addr(held_by_garbage),
    ];
    let whole = gc
        .pinned_young_referents_unheld_within(&all, &root_words, usize::MAX, false)
        .expect("a verdict");
    let expected: FxHashSet<usize> = [addr(dead), addr(dead_holder)].into_iter().collect();
    assert_eq!(whole.unheld, expected);
    for (i, &a) in all.iter().enumerate() {
        let alone = gc
            .pinned_young_referents_unheld_within(&[a], &root_words, usize::MAX, false)
            .expect("a verdict alone");
        assert_eq!(
            alone.unheld.contains(&a),
            whole.unheld.contains(&a),
            "candidate {i} alone"
        );
        let other = all[(i + 1) % all.len()];
        let pair = gc
            .pinned_young_referents_unheld_within(&[a, other], &root_words, usize::MAX, false)
            .expect("a verdict in a pair");
        for b in [a, other] {
            assert_eq!(pair.unheld.contains(&b), whole.unheld.contains(&b), "pair {i}");
        }
    }
    gc.unpin_region(pinned);
}

/// gci i30/rp (`i27-gr-report.md` review item 4): a referent in a kept
/// region that only a dead finalizable's resurrected copy names is
/// `finalizer_only` (held for every in-place verdict, finalizer-reachable
/// for the reference round), one a strongly reachable copy also names is
/// held, and one nothing names is unheld.
#[test]
fn gci_i30_rp_a_referent_only_a_resurrected_finalizable_holds_is_finalizer_only() {
    let gc = collector();
    let reference = gc.alloc_object(ClassId::new(1), 2);
    let pinned = region_of(&gc, reference);
    let only_fin = gc.alloc_object(ClassId::new(1), 1);
    let shared = gc.alloc_object(ClassId::new(1), 1);
    let dead = gc.alloc_object(ClassId::new(1), 1);
    for o in [only_fin, shared, dead] {
        assert_eq!(region_of(&gc, o), pinned, "fixture: one Eden region");
    }
    let fin = alloc_outside(&gc, pinned);
    gc.set_field(fin, 0, Value::Object(Some(only_fin)));
    let strong = alloc_outside(&gc, pinned);
    gc.set_field(strong, 0, Value::Object(Some(shared)));
    let fin_twin = alloc_outside(&gc, pinned);
    gc.set_field(fin_twin, 0, Value::Object(Some(shared)));
    gc.pin_region(pinned);
    let mut roots = vec![reference, strong];
    // SAFETY: the unit test runs the heap single-threaded; no mutator runs.
    let stw = unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() };
    let (_result, resurrected) = gc.collect_garbage_with_finalizers(
        &stw,
        &mut roots,
        &[addr(fin), addr(fin_twin)],
        &NoopMonitors,
    );
    assert_eq!(resurrected.len(), 2, "fixture: Phase 3.5 resurrected both finalizables");
    assert!(gc.pinned_referent_verdict_armed(), "fixture: the pause kept the region");
    let root_words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
    let verdict = gc
        .pinned_young_referents_unheld_within(
            &[addr(reference), addr(only_fin), addr(shared), addr(dead)],
            &root_words,
            usize::MAX,
            false,
        )
        .expect("a verdict");
    gc.unpin_region(pinned);
    let unheld: FxHashSet<usize> = [addr(dead)].into_iter().collect();
    let finalizer_only: FxHashSet<usize> = [addr(only_fin)].into_iter().collect();
    assert_eq!(verdict.unheld, unheld);
    assert_eq!(verdict.finalizer_only, finalizer_only);
    // The next collection forgets the holders at its start.
    assert!(!gc.resurrection_holders.lock().is_empty(), "written by the pause");
    gc.reset_refproc_kept_young_token();
    assert!(gc.resurrection_holders.lock().is_empty());
}

/// No verdict without an armed pause, over budget, or with no candidate in
/// a young region; a pause that kept nothing does not arm it.
#[test]
fn gci_i27_gr_every_way_of_not_knowing_declines() {
    let gc = collector();
    let a = gc.alloc_object(ClassId::new(1), 1);
    assert_eq!(
        gc.pinned_young_referents_unheld_within(&[addr(a)], &[], usize::MAX, false),
        Err(G1PinnedReferentDecline::NotArmed),
        "no collection yet"
    );
    let mut roots = vec![a];
    gc.young_collection(&mut roots, &NoopMonitors);
    assert!(!gc.pinned_referent_verdict_armed(), "nothing kept, nothing armed");

    let b = gc.alloc_object(ClassId::new(1), 1);
    let pinned = region_of(&gc, b);
    gc.pin_region(pinned);
    let mut roots = vec![roots[0]];
    gc.young_collection(&mut roots, &NoopMonitors);
    assert!(gc.pinned_referent_verdict_armed());
    assert_eq!(
        gc.pinned_young_referents_unheld_within(&[addr(b)], &[], 0, false),
        Err(G1PinnedReferentDecline::OverBudget)
    );
    assert_eq!(
        gc.pinned_young_referents_unheld_within(&[0, 0x1000, addr(b) + 4], &[], usize::MAX, false),
        Err(G1PinnedReferentDecline::NoCandidates)
    );
    let v = gc
        .pinned_young_referents_unheld_within(&[addr(b)], &[], usize::MAX, false)
        .expect("a verdict");
    assert!(v.unheld.contains(&addr(b)), "nothing holds b");
    // A later record of another collection count disarms it until it
    // keeps a region again.
    gc.unpin_region(pinned);
    gc.reset_refproc_kept_young_token();
    assert!(!gc.pinned_referent_verdict_armed());
}

/// The record funnel keeps the "kept" bit across a drain pass of the same
/// collection and refuses a full record.
#[test]
fn gci_i27_gr_the_token_follows_the_records() {
    let gc = collector();
    let kept = G1PausePhases {
        jit_pinned_out: 1,
        ..Default::default()
    };
    let plain = G1PausePhases::default();
    gc.collection_count.fetch_add(1, Ordering::Relaxed);
    gc.note_refproc_evacuation_record(G1CollectionType::YoungOnly, &kept);
    assert!(gc.pinned_referent_verdict_armed());
    gc.collection_count.fetch_add(1, Ordering::Relaxed);
    gc.note_refproc_evacuation_record(G1CollectionType::YoungOnly, &plain);
    assert!(gc.pinned_referent_verdict_armed(), "a drain pass keeps the bit");
    gc.reset_refproc_kept_young_token();
    gc.collection_count.fetch_add(1, Ordering::Relaxed);
    gc.note_refproc_evacuation_record(G1CollectionType::Mixed, &plain);
    assert!(!gc.pinned_referent_verdict_armed(), "a fresh collection that kept nothing");
    gc.collection_count.fetch_add(1, Ordering::Relaxed);
    gc.note_refproc_evacuation_record(G1CollectionType::Full, &kept);
    assert!(!gc.pinned_referent_verdict_armed(), "a full record never arms it");
    gc.collection_count.fetch_add(1, Ordering::Relaxed);
    gc.note_refproc_evacuation_record(G1CollectionType::Mixed, &kept);
    gc.collection_count.fetch_add(1, Ordering::Relaxed);
    assert!(
        !gc.pinned_referent_verdict_armed(),
        "a later collection count disarms it"
    );
}

/// gci i32/ec (`i30-rp-report.md` hand-back 1): a pause whose ONLY young
/// region is pinned has an empty collection set and returns before the
/// record funnel; it now arms the verdict (both young drivers), so the
/// dead referent of that region is named at that pause, and the rooted one
/// is not. The collection count does not move (the pause records
/// nothing).
#[test]
fn gci_i32_ec_an_empty_cset_pause_that_pinned_every_young_region_arms_the_verdict() {
    for parallel in [false, true] {
        let gc = collector();
        let reference = gc.alloc_object(ClassId::new(1), 2);
        let pinned = region_of(&gc, reference);
        let dead = gc.alloc_object(ClassId::new(1), 1);
        let held = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(reference, 1, Value::Object(Some(held)));
        for o in [dead, held] {
            assert_eq!(region_of(&gc, o), pinned, "fixture: one Eden region");
        }
        gc.pin_region(pinned);
        let count_before = gc.collection_count.load(Ordering::Relaxed);
        let mut roots = vec![reference];
        let result = if parallel {
            gc.young_collection_parallel(&mut roots, &NoopMonitors)
        } else {
            gc.young_collection_serial(&mut roots, &NoopMonitors)
        };
        assert!(result.pointer_map.is_empty(), "fixture: an empty collection set");
        assert_eq!(
            gc.collection_count.load(Ordering::Relaxed),
            count_before,
            "an empty pause records nothing"
        );
        assert!(
            gc.pinned_referent_verdict_armed(),
            "parallel={parallel}: an empty CSet that pinned out a young region arms it"
        );
        let root_words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        let verdict = gc
            .pinned_young_referents_unheld_within(
                &[addr(reference), addr(dead), addr(held)],
                &root_words,
                usize::MAX,
                false,
            )
            .expect("a verdict");
        let expected: FxHashSet<usize> = [addr(dead)].into_iter().collect();
        assert_eq!(verdict.unheld, expected, "parallel={parallel}");
        gc.unpin_region(pinned);
        gc.reset_refproc_kept_young_token();
    }
}

/// gci i32/ec (`i31-gq-report.md` residual): a dead finalizable `fin` in a
/// pinned region names `child` (another Eden region, so the pause copies
/// it), `child` names `grandchild`, and `fin` names `shared` that a rooted
/// object also names. The private closure is `child` and `grandchild` at
/// their post-pause addresses; `shared` is strongly held; the seed is
/// never listed. A walk cap below the closure's depth stops early and
/// only over-retains.
#[test]
fn gci_i32_ec_a_kept_finalizable_s_copied_private_children_are_finalizer_only() {
    let gc = collector();
    let fin = gc.alloc_object(ClassId::new(1), 2);
    let pinned = region_of(&gc, fin);
    let child = alloc_outside(&gc, pinned);
    let grandchild = alloc_outside(&gc, pinned);
    let shared = alloc_outside(&gc, pinned);
    let rooted = alloc_outside(&gc, pinned);
    gc.set_field(fin, 0, Value::Object(Some(child)));
    gc.set_field(fin, 1, Value::Object(Some(shared)));
    gc.set_field(child, 0, Value::Object(Some(grandchild)));
    gc.set_field(rooted, 0, Value::Object(Some(shared)));
    gc.pin_region(pinned);
    let mut roots = vec![rooted];
    let result = gc.young_collection(&mut roots, &NoopMonitors);
    let moved = |o: ObjectRef| -> usize {
        *result
            .pointer_map
            .get(&addr(o))
            .expect("fixture: the pause copied the outside objects")
    };
    let (child_now, grandchild_now, shared_now) = (moved(child), moved(grandchild), moved(shared));
    assert!(
        !result.pointer_map.contains_key(&addr(fin)),
        "fixture: the kept region's objects are not in the map"
    );
    assert!(gc.pinned_referent_verdict_armed(), "fixture: the pause kept the region");
    let root_words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();

    let whole = gc
        .kept_finalizables_private_closure_within(&[addr(fin)], &root_words, usize::MAX, false, 4)
        .expect("a closure");
    let expected: FxHashSet<usize> = [child_now, grandchild_now].into_iter().collect();
    assert_eq!(whole.finalizer_only, expected);
    assert!(!whole.finalizer_only.contains(&shared_now), "shared is strongly held");
    assert!(whole.complete);
    assert_eq!(whole.closure, 4, "fin, child, shared, grandchild");

    // One walk confirms only the seed: nothing reported.
    let one = gc
        .kept_finalizables_private_closure_within(&[addr(fin)], &root_words, usize::MAX, false, 1)
        .expect("a closure");
    assert!(one.finalizer_only.is_empty());
    assert!(!one.complete);
    // Two walks confirm the children, not the grandchild.
    let two = gc
        .kept_finalizables_private_closure_within(&[addr(fin)], &root_words, usize::MAX, false, 2)
        .expect("a closure");
    let child_only: FxHashSet<usize> = [child_now].into_iter().collect();
    assert_eq!(two.finalizer_only, child_only);
    assert!(!two.complete);

    // A root word on the child makes it and its grandchild strong.
    let mut rooted_child = root_words.clone();
    rooted_child.push(child_now);
    let held = gc
        .kept_finalizables_private_closure_within(&[addr(fin)], &rooted_child, usize::MAX, false, 4)
        .expect("a closure");
    assert!(held.finalizer_only.is_empty());
    // gci i38/ec: the holder census names the root word, and on the
    // unrooted answer only the outside slot that holds `shared`.
    assert_eq!((held.held_by_root, held.held_by_pin), (1, 0));
    assert_eq!((whole.held_by_slot, whole.held_by_root, whole.held_by_pin), (1, 0, 0));
    // gci i41/hc: and WHICH words those were -- the root entry by its
    // index in the slice, the outside slot by its holder (slots first:
    // the walk runs before the root words are read).
    let shared_by_rooted = G1ClosureHolder {
        kind: G1ClosureHolderKind::Slot {
            holder: root_words[0],
        },
        word: shared_now,
        member: shared_now,
    };
    assert_eq!(
        held.holders,
        vec![
            shared_by_rooted,
            G1ClosureHolder {
                kind: G1ClosureHolderKind::Root {
                    index: rooted_child.len() - 1
                },
                word: child_now,
                member: child_now,
            }
        ]
    );
    assert_eq!(whole.holders, vec![shared_by_rooted]);
    assert_eq!((held.holders_dropped, whole.holders_dropped), (0, 0));
    // The members of the last walk, ascending, seed included.
    let mut member_starts: Vec<usize> = whole.members.iter().map(|&(a, _)| a).collect();
    let mut expected_members = vec![addr(fin), child_now, shared_now, grandchild_now];
    expected_members.sort_unstable();
    assert_eq!(member_starts, expected_members);
    member_starts.dedup();
    assert_eq!(member_starts.len(), 4);
    assert!(whole.members.iter().all(|&(_, size)| size >= HEADER_SIZE));

    // Every way of not knowing declines.
    assert_eq!(
        gc.kept_finalizables_private_closure_within(&[addr(fin)], &root_words, 0, false, 4),
        Err(G1PinnedReferentDecline::OverBudget)
    );
    assert_eq!(
        gc.kept_finalizables_private_closure_within(&[0, 0x1000], &root_words, usize::MAX, false, 4),
        Err(G1PinnedReferentDecline::NoCandidates)
    );
    gc.unpin_region(pinned);
    gc.reset_refproc_kept_young_token();
    assert_eq!(
        gc.kept_finalizables_private_closure_within(&[addr(fin)], &root_words, usize::MAX, false, 4),
        Err(G1PinnedReferentDecline::NotArmed)
    );
}

/// gci i41/hc: the holder record's pure parts -- the pin publisher split
/// (own entry first, then this cycle's helper-window words, else a peer's
/// entry) and the cap (recorded up to the bound, counted past it).
#[test]
fn gci_i41_hc_closure_holder_pin_source_and_cap() {
    let own: FxHashSet<usize> = [0x10].into_iter().collect();
    let helper: FxHashSet<usize> = [0x10, 0x20].into_iter().collect();
    assert_eq!(closure_pin_source(0x10, &own, &helper), G1ClosurePinSource::Own);
    assert_eq!(
        closure_pin_source(0x20, &own, &helper),
        G1ClosurePinSource::HelperWindow
    );
    assert_eq!(closure_pin_source(0x30, &own, &helper), G1ClosurePinSource::Peer);
    let mut holders: Vec<G1ClosureHolder> = Vec::new();
    let mut dropped = 0usize;
    for k in 0..KEPT_FINALIZABLE_CLOSURE_HOLDERS_MAX + 3 {
        push_closure_holder(
            &mut holders,
            &mut dropped,
            G1ClosureHolder {
                kind: G1ClosureHolderKind::Root { index: k },
                word: 0x1000 + 8 * k,
                member: 0x1000,
            },
        );
    }
    assert_eq!(holders.len(), KEPT_FINALIZABLE_CLOSURE_HOLDERS_MAX);
    assert_eq!(dropped, 3);
    assert_eq!(holders[0].kind, G1ClosureHolderKind::Root { index: 0 });
}


/// gci i32/ec: the three empty-CSet returns of the evacuation drivers call
/// the arming; `collect_garbage`'s refusals do not (they return before any
/// driver).
#[test]
fn gci_i32_ec_the_empty_cset_returns_are_wired() {
    let src = concat!(include_str!("../../g1/mod.rs"), "\n", include_str!("../../g1/value_from_unaligned_ptr.rs"), "\n", include_str!("../../g1/config.rs"), "\n", include_str!("../../g1/needs_gc_recount_due.rs"), "\n", include_str!("../../g1/collector.rs"), "\n", include_str!("../../g1/set_resolved_compact_primitive_field.rs"), "\n", include_str!("../../g1/futile_young_escalation_count.rs"), "\n", include_str!("../../g1/count_unretired_forwards.rs"), "\n", include_str!("../../g1/collect_outgoing_cross_region_edges.rs"), "\n", include_str!("../../g1/mark_root_grid_place.rs"), "\n", include_str!("../../g1/record_collection_with_phases_within.rs"), "\n", include_str!("../../g1/bump_satb_pre_barrier_epoch.rs"), "\n", include_str!("../../g1/evac_copy_fits_a_region.rs"), "\n", include_str!("../../g1/old_plab_bytes_for.rs")).replace("\r\n", "\n");
    for driver in [
        "pub fn young_collection_serial(",
        "pub fn young_collection_parallel(",
        "pub fn mixed_collection_parallel(",
    ] {
        let at = src.find(driver).expect(driver);
        let rest = &src[at..];
        let end = rest[1..].find("\n    pub").map_or(rest.len(), |e| e + 1);
        let body = &rest[..end];
        let empty = body.find("if cset.is_empty()").expect(driver);
        let arm = body
            .find("self.note_refproc_empty_cset_pause(jni_pinned_out + jit_pinned_out);")
            .expect(driver);
        let ret = empty + body[empty..].find("return GcResult").expect(driver);
        assert!(empty < arm && arm < ret, "{driver}: armed inside the empty-CSet return");
    }
    assert_eq!(src.matches("self.note_refproc_empty_cset_pause(").count(), 3);
}

/// g1pin x gci i27/gr (`WeakClearNoLoadProbe`, `-XX:+UseG1GC -Xmx256m`): a
/// region kept only for a blocked peer's stack word is filled around the
/// pinned object (`fill_around_kept_pins`), which erases the headers of
/// everything else in it. The young drivers never flagged such a pause as
/// one that kept a region, and a walk could not find a dead referent there
/// as an object start, so the weak referent next to the pinned object read
/// live (the region-granular answer) at every pause. The pause arms the
/// verdict, and a candidate the erased region held that no walk finds is
/// dead.
#[test]
fn g1pin_a_dead_referent_in_a_region_kept_for_a_peer_pin_is_unheld() {
    let gc = collector();
    let reference = gc.alloc_object(ClassId::new(1), 2);
    let dead = gc.alloc_object(ClassId::new(1), 1);
    let pinned = gc.alloc_object(ClassId::new(1), 1);
    let (dead_addr, pinned_addr) = (addr(dead), addr(pinned));
    cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
    cratonvm_gc_core::gc_quiescence::record_peer_stack_slot(7, 0x1000, pinned_addr);
    let mut roots = vec![reference];
    let result = gc.young_collection(&mut roots, &NoopMonitors);
    cratonvm_gc_core::gc_quiescence::clear_peer_stack_slots();
    gc.clear_peer_object_pins();
    if !(true
        && true
        && cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled())
    {
        return;
    }
    assert_eq!(result.pointer_map.get(&pinned_addr), Some(&pinned_addr), "fixture: pinned");
    assert!(!result.pointer_map.contains_key(&dead_addr), "fixture: dead, not copied");
    assert!(gc.pinned_referent_verdict_armed(), "a region kept only for a pin arms it");
    let root_words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
    let verdict = gc
        .pinned_young_referents_unheld_within(&[dead_addr], &root_words, usize::MAX, false)
        .expect("a verdict");
    let expected: FxHashSet<usize> = [dead_addr].into_iter().collect();
    assert_eq!(verdict.unheld, expected);
    gc.reset_refproc_kept_young_token();
    assert!(!gc.pinned_referent_verdict_armed());
    assert!(gc.pin_filled_regions.lock().is_empty(), "the next collection forgets them");
}

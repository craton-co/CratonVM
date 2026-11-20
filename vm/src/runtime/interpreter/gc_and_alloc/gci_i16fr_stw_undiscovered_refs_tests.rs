use super::*;

/// X reaches W (a `Reference` whose slot the pre-collection pass nulled)
/// and A; W's referent Y reaches W2, whose referent is Y2. The walk sees
/// Y and Y2 only through `referent_of`, and gives up past its budget.
#[test]
fn gci_i16fr_the_probe_walk_follows_fields_and_noted_referents() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let heap = &shared.mem.heap;
    let cid = crate::classloading::ClassId::new(0);
    let x = heap.alloc_object(cid, 2);
    let w = heap.alloc_object(cid, 2);
    let a_obj = heap.alloc_object(cid, 1);
    let y = heap.alloc_object(cid, 1);
    let w2 = heap.alloc_object(cid, 2);
    let y2 = heap.alloc_object(cid, 1);
    heap.set_field(x, 0, Value::Object(Some(w)));
    heap.set_field(x, 1, Value::Object(Some(a_obj)));
    heap.set_field(y, 0, Value::Object(Some(w2)));
    let a = |o: ObjectRef| o.as_ptr() as usize;
    let (aw, ay, aw2, ay2) = (a(w), a(y), a(w2), a(y2));
    let referent_of = |addr: usize| -> Option<usize> {
        if addr == aw {
            Some(ay)
        } else if addr == aw2 {
            Some(ay2)
        } else {
            None
        }
    };
    let mut seen = stw_probe_closure(&shared, &[a(x)], &referent_of, 64).expect("in budget");
    seen.sort_unstable();
    let mut want = vec![a(x), aw, a(a_obj), ay, aw2, ay2];
    want.sort_unstable();
    assert_eq!(seen, want);

    // Without the noted referents the walk stops at W.
    let mut plain = stw_probe_closure(&shared, &[a(x)], &|_| None, 64).expect("in budget");
    plain.sort_unstable();
    let mut want = vec![a(x), aw, a(a_obj)];
    want.sort_unstable();
    assert_eq!(plain, want);

    // Past the budget: abandoned.
    assert!(stw_probe_closure(&shared, &[a(x)], &referent_of, 2).is_none());
}

/// gcj j3/x2 (og2's growth span): the initial-mark pause re-reads the old
/// generation's extent INSIDE the pause, when the bitmap spans the growth
/// limit, and both the reference-skip screen and the class-unload capture
/// take that pair, not the pre-pause one (fail-first: before, both took
/// the pre-pause capacity, so a loader in a span grown in the take-over
/// window had no row and the remark, no longer refusing, swept it).
#[test]
fn gcj3x2_the_initial_mark_capture_takes_the_in_pause_old_gen_extent() {
    let src = GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let start = src.find("fn maybe_concurrent_gc_at(").expect("driver");
    let body = &src[start..];
    let body = &body[..body.find("\n\u{7d}\n").expect("end")];
    let pos = |needle: &str| body.find(needle).expect(needle);
    let pre = pos("let (old_gen_base, old_gen_size) = shared.mem.heap.old_gen_info();");
    let take_over = pos(concat!("pause_guard.hold(stw_take_over", "_and_wait("));
    let reread = pos("let (old_gen_base, old_gen_size) = if old_gen_mark_span > old_gen_size {");
    let skip = pos("gen_conc_reference_skip_candidates(shared, old_gen_base, old_gen_size)");
    let capture = pos(concat!(
        "ClassUnloadTables::capture(\n",
        "                old_gen_base,"
    ));
    assert!(
        pre < take_over && take_over < reread && reread < skip && reread < capture,
        "{pre} {take_over} {reread} {skip} {capture}"
    );
    assert_eq!(
        body.matches("ClassUnloadTables::capture(").count(),
        1,
        "one capture"
    );
}

/// gcj j3/x2: the counted walk gives the plain walk's answer and counts
/// the walk; an abandoned one is counted as abandoned (fail-first: before,
/// abandonment left no trace), and the door's probe walks through the
/// counted helper. Process totals: deltas, `>=` (other tests may walk).
#[test]
fn gcj3x2_the_stw_probe_walk_counts_its_cost_and_its_abandonment() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let heap = &shared.mem.heap;
    let cid = crate::classloading::ClassId::new(0);
    let x = heap.alloc_object(cid, 1);
    let y = heap.alloc_object(cid, 1);
    heap.set_field(x, 0, Value::Object(Some(y)));
    let a = |o: ObjectRef| o.as_ptr() as usize;
    let before = stw_probe_census();
    let (walked, complete) = stw_probe_closure_counted(&shared, &[a(x)], &|_| None, 64);
    assert!(complete);
    assert_eq!(
        Some(walked),
        stw_probe_closure(&shared, &[a(x)], &|_| None, 64)
    );
    let mid = stw_probe_census();
    assert!(mid.walks >= before.walks + 1);
    assert!(mid.visited >= before.visited + 2);
    // gcj j2/x1: past the budget the visited prefix comes back.
    let (prefix, complete) = stw_probe_closure_counted(&shared, &[a(x)], &|_| None, 1);
    assert!(!complete);
    assert_eq!(prefix, vec![a(x)]);
    let after = stw_probe_census();
    assert!(after.abandoned >= mid.abandoned + 1);
    assert!(after.walks >= mid.walks + 1);
    let src = GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let at = src
        .find("fn before_collection_if(")
        .expect("before_collection_if");
    let body = &src[at..];
    let body = &body[..body.find("\n    \u{7d}\n").expect("end")];
    assert!(body.contains("stw_probe_closure_counted(shared, &registered, &referent_of,"));
}

/// gcj j2/x1 (`gcj-j2x1-wave-j2-default-flips-...` item (ii), row
/// `Gcj3X2StwProbeBudgetProbe big`): the round-robin walk finishes a small
/// root's closure (X -> W, W's noted referent Y) although another root
/// reaches more than the budget, in either root order; the walk reports
/// itself unfinished. Within the budget it visits exactly the closure's set.
#[test]
fn gcj2x1_a_big_root_past_the_budget_does_not_cost_a_small_root_its_closure() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let heap = &shared.mem.heap;
    let cid = crate::classloading::ClassId::new(0);
    let big = heap.alloc_array(cid, ArrayElementType::Reference, 200);
    for i in 0..200 {
        let o = heap.alloc_object(cid, 1);
        heap.set_array_element(big, i, Value::Object(Some(o)))
            .expect("in range");
    }
    let x = heap.alloc_object(cid, 1);
    let w = heap.alloc_object(cid, 1);
    let y = heap.alloc_object(cid, 1);
    heap.set_field(x, 0, Value::Object(Some(w)));
    let a = |o: ObjectRef| o.as_ptr() as usize;
    let (aw, ay) = (a(w), a(y));
    let referent_of = |addr: usize| (addr == aw).then_some(ay);
    for roots in [[a(big), a(x)], [a(x), a(big)]] {
        let (seen, complete) = stw_probe_walk_fair(&shared, &roots, &referent_of, 40);
        assert!(!complete, "{roots:?}: the big root passes the budget");
        assert_eq!(seen.len(), 40);
        for want in [a(x), aw, ay] {
            assert!(
                seen.contains(&want),
                "{roots:?}: the small root's closure is whole"
            );
        }
        let (mut all, complete) = stw_probe_walk_fair(&shared, &roots, &referent_of, 1 << 10);
        assert!(complete);
        let mut plain =
            stw_probe_closure(&shared, &roots, &referent_of, 1 << 10).expect("in budget");
        all.sort_unstable();
        plain.sort_unstable();
        assert_eq!(
            all, plain,
            "{roots:?}: a finished walk visits the closure's set"
        );
    }
}

/// The collection resurrected X (dead finalizable) and the probe
/// referents Y and Z; X reaches Z by a strong field. Y and Z leave the
/// dead-finalizer list; Y is candidate-only (its phantom P is enqueued by
/// the round), Z is kept by X's closure (its phantom Q is not).
#[test]
fn gci_i16fr_after_collection_drops_probe_referents_and_unmarks_candidate_only_ones() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let heap = &shared.mem.heap;
    let cid = crate::classloading::ClassId::new(0);
    let x = heap.alloc_object(cid, 1);
    let y = heap.alloc_object(cid, 1);
    let z = heap.alloc_object(cid, 1);
    let p = heap.alloc_object(cid, 2);
    let q = heap.alloc_object(cid, 2);
    let queue = heap.alloc_object(cid, 1);
    heap.set_field(x, 0, Value::Object(Some(z)));
    let a = |o: ObjectRef| o.as_ptr() as usize;
    {
        let mut rp = shared.mem.ref_processor.lock();
        rp.discover_reference(
            cratonvm_gc::reference::ReferenceType::Phantom,
            a(p),
            a(y),
            Some(a(queue)),
        );
        rp.discover_reference(
            cratonvm_gc::reference::ReferenceType::Phantom,
            a(q),
            a(z),
            Some(a(queue)),
        );
        rp.note_resurrected_finalizables(&[a(x), a(y), a(z)]);
    }
    let mut referents = vec![a(y), a(z)];
    referents.sort_unstable();
    let mut candidates = vec![a(x), a(y), a(z)];
    candidates.sort_unstable();
    let probe = StwUndiscoveredProbe {
        pairs: vec![(a(p), a(y)), (a(q), a(z))],
        candidates,
        referents,
    };
    let map = cratonvm_types::PointerMap::default();
    let real = probe.after_collection(&shared, &map, vec![a(x), a(y), a(z)]);
    assert_eq!(real, vec![a(x)]);
    let result = shared
        .mem
        .ref_processor
        .lock()
        .process_references_with_finalizer_trace(&|_: usize| true, None, 64, 0);
    assert_eq!(result.to_enqueue, vec![(a(p), a(queue))]);
}

/// gci i17/fz (i16/fr's residual): what ONLY a candidate-only referent
/// reaches is unmarked for the round too. X is the real dead finalizable
/// (reaches S); Y is candidate-only (its `Reference` PY was discovered)
/// and reaches C (resurrection-only), S (also X's) and G (strongly
/// marked: not in the resurrection set); K is the referent of R, a
/// `Reference` marked only through the resurrection (undiscovered: kept),
/// and reaches KC. Phantoms to Y and C are enqueued in this round; those
/// to S, G, KC and R's own row are not.
#[test]
fn gci_i17fz_only_what_a_candidate_only_referent_reaches_is_unmarked() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let heap = &shared.mem.heap;
    let cid = crate::classloading::ClassId::new(0);
    let x = heap.alloc_object(cid, 1);
    let y = heap.alloc_object(cid, 3);
    let c = heap.alloc_object(cid, 1);
    let s = heap.alloc_object(cid, 1);
    let g = heap.alloc_object(cid, 1);
    let k = heap.alloc_object(cid, 1);
    let kc = heap.alloc_object(cid, 1);
    let r = heap.alloc_object(cid, 2);
    let py = heap.alloc_object(cid, 2);
    let pc = heap.alloc_object(cid, 2);
    let ps = heap.alloc_object(cid, 2);
    let pg = heap.alloc_object(cid, 2);
    let pkc = heap.alloc_object(cid, 2);
    let queue = heap.alloc_object(cid, 1);
    heap.set_field(x, 0, Value::Object(Some(s)));
    heap.set_field(y, 0, Value::Object(Some(c)));
    heap.set_field(y, 1, Value::Object(Some(s)));
    heap.set_field(y, 2, Value::Object(Some(g)));
    heap.set_field(k, 0, Value::Object(Some(kc)));
    let a = |o: ObjectRef| o.as_ptr() as usize;
    {
        let mut rp = shared.mem.ref_processor.lock();
        for (reference_obj, referent) in [(py, y), (pc, c), (ps, s), (pg, g), (pkc, kc), (r, k)] {
            rp.discover_reference(
                cratonvm_gc::reference::ReferenceType::Phantom,
                a(reference_obj),
                a(referent),
                Some(a(queue)),
            );
        }
        // The resurrection closure: everything but G.
        rp.note_resurrected_finalizables(&[a(x), a(y), a(k), a(c), a(s), a(kc), a(r)]);
    }
    let mut referents = vec![a(y), a(k)];
    referents.sort_unstable();
    let mut candidates = vec![a(x), a(y), a(k)];
    candidates.sort_unstable();
    let probe = StwUndiscoveredProbe {
        pairs: vec![(a(py), a(y)), (a(r), a(k))],
        candidates,
        referents,
    };
    let map = cratonvm_types::PointerMap::default();
    let real = probe.after_collection(&shared, &map, vec![a(x), a(y), a(k)]);
    assert_eq!(real, vec![a(x)]);
    let result = shared
        .mem
        .ref_processor
        .lock()
        .process_references_with_finalizer_trace(&|_: usize| true, None, 64, 0);
    let mut enqueued = result.to_enqueue.clone();
    enqueued.sort_unstable();
    let mut want = vec![(a(py), a(queue)), (a(pc), a(queue))];
    want.sort_unstable();
    assert_eq!(enqueued, want);
}

/// The door's order: the probe is taken after the finalizable snapshot,
/// hands the collection its candidates, and splits the dead list after
/// the resurrection note and before the round and the enqueue.
#[test]
fn gci_i16fr_the_door_runs_the_probe_around_its_collection() {
    let src = GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let at = src
        .find("fn run_collection_pause(")
        .expect("run_collection_pause");
    let body = &src[at..];
    let body = &body[..body.find("\n\u{7d}\n").expect("end")];
    let pos = |needle: &str| body.find(needle).expect(needle);
    let snapshot = pos("let (fin_roots, queued_fin_roots) = pause_finalizable_inputs(shared);");
    let before =
        pos("StwUndiscoveredProbe::before_collection_if(shared, &fin_roots, &probe_engaged);");
    let cands = pos("let fin_candidates = stw_probe.collector_candidates(&fin_roots);");
    let collect = pos(".collect_garbage_with_finalizers(");
    let handed = pos(concat!(
        "            fin_candidates,\n",
        "            &shared.threads.monitors,"
    ));
    let note = pos("note_resurrected_finalizables_for_reference_processing(\n        shared,\n        fin_candidates,");
    let split = pos("stw_probe.after_collection(shared, &result.pointer_map, dead_finalizers);");
    let round = pos("    process_references_after_gc(\n");
    let enqueue = pos("enqueue_resurrected_finalizers(shared, &dead_finalizers)");
    assert!(
        snapshot < before
            && before < cands
            && cands < collect
            && collect < handed
            && handed < note
            && note < split
            && split < round
            && round < enqueue,
        "{snapshot} {before} {cands} {collect} {handed} {note} {split} {round} {enqueue}"
    );
}

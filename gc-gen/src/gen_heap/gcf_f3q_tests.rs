use super::*;

/// A `(class_id, field_count)` pair no other gc test allocates with:
/// `[Int payload, Reference next]`, 24 bytes. Registration is monotone
/// process state; re-registering the identical layout is harmless.
const CID: u32 = 47_391;
/// Unregistered: allocates as a legacy (16-byte `Value` cell) instance.
const LEGACY: u32 = 47_393;
/// A second registered layout, used only to move `layout_generation`.
const BUMP: u32 = 47_395;

fn register(heap: &GenerationalHeap, cid: u32) {
    cratonvm_types::register_class_layout(
        heap.layout_domain(),
        cid,
        std::sync::Arc::new(CompactLayout {
            field_disps: vec![8, 16],
            is_ref: vec![false, true],
            field_kinds: vec![FieldStorageKind::Int, FieldStorageKind::Reference],
            ref_disps: vec![16],
            total_size: 24,
        }),
    );
}

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

/// Patch 1: the memo answers exactly what `gen_object_total_size`
/// answers -- for a compact instance (served from the memo on a repeat),
/// a legacy instance and an array (never memoised) -- re-asks after
/// `layout_generation` moves, and never answers when unarmed.
#[test]
fn f3q_the_walk_size_memo_answers_what_gen_object_total_size_answers() {
    let heap = GenerationalHeap::with_sizes(1 << 20, 1 << 20);
    register(&heap, CID);
    let a = heap.alloc_object(ClassId::new(CID), 2);
    let b = heap.alloc_object(ClassId::new(CID), 2);
    let legacy = heap.alloc_object(ClassId::new(LEGACY), 3);
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 17);
    let (ha, hb) = (heap.get_header(a), heap.get_header(b));
    let (hl, harr) = (heap.get_header(legacy), heap.get_header(arr));
    assert!(!hl.is_short(), "an unregistered class allocates legacy");

    let mut off = GridSizeMemo::new(false);
    let mut memo = GridSizeMemo::new(true);
    for h in [hl, harr, hl, harr] {
        assert_eq!(memo.total_size(h), gen_object_total_size(h));
        assert_eq!(off.total_size(h), gen_object_total_size(h));
    }
    assert_eq!(
        memo.hits, 0,
        "legacy instances and arrays are never memoised"
    );
    if !ha.is_short() {
        assert!(
            !compact_ref_fields_enabled(),
            "a registered layout must allocate compact"
        );
        return;
    }
    // Many repeats: another test may register a layout (moving the
    // generation) between two calls, which turns a would-be hit into a
    // miss -- never into a wrong answer.
    for _ in 0..1000 {
        for h in [ha, hb, hl, ha] {
            assert_eq!(memo.total_size(h), gen_object_total_size(h));
            assert_eq!(off.total_size(h), gen_object_total_size(h));
        }
    }
    assert!(
        memo.hits > 0,
        "positive control: a repeated compact class is served from the memo"
    );
    assert_eq!(off.hits, 0, "an unarmed memo never answers");
    // A registration moves the generation: the next lookup re-asks.
    memo.total_size(ha);
    let before = memo.hits;
    register(&heap, BUMP);
    let generation = cratonvm_types::layout_generation();
    assert_ne!(
        memo.generation, generation,
        "the registration moved the generation"
    );
    assert_eq!(memo.total_size(ha), gen_object_total_size(ha));
    assert!(memo.generation >= generation, "the miss re-keyed the memo");
    assert_eq!(memo.hits, before, "a moved generation is never a hit");
}

/// Patch 2: the tally retracts an unwind exactly as the atomics did and
/// publishes the same sums into the heap's own counters.
#[test]
fn f3q_the_batched_census_tally_publishes_the_unbatched_sums() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
    let objects0 = heap.stats.objects_promoted.load(Ordering::Relaxed);
    let bytes0 = heap.stats.bytes_promoted.load(Ordering::Relaxed);
    let mut t = EvacWalkTally {
        candidates: 9,
        pinned: 1,
        unaged: 2,
        evacuated: 5,
        old_full: 0,
        promoted_objects: 5,
        promoted_bytes: 200,
    };
    t.retract(2, 80);
    assert_eq!(
        (t.evacuated, t.promoted_objects, t.promoted_bytes),
        (3, 3, 120)
    );
    assert_eq!(
        (t.candidates, t.pinned, t.unaged),
        (9, 1, 2),
        "an unwind keeps the census"
    );
    let sp_before = selective_promotion_census();
    t.publish(&heap.stats);
    assert_eq!(
        heap.stats.objects_promoted.load(Ordering::Relaxed) - objects0,
        3
    );
    assert_eq!(
        heap.stats.bytes_promoted.load(Ordering::Relaxed) - bytes0,
        120
    );
    // `SP_CENSUS` is process-wide; other tests only ever add to it.
    let sp_after = selective_promotion_census();
    assert!(sp_after.3 >= sp_before.3 + 9 && sp_after.6 >= sp_before.6 + 3);
    // Over-retraction saturates rather than wrapping a diagnostic.
    let mut z = EvacWalkTally::default();
    z.retract(1, 8);
    assert_eq!(z, EvacWalkTally::default());
}

/// What one run of the promoting scenario leaves behind.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    /// Payloads read from the root's list and from each old holder.
    lists: Vec<Vec<i32>>,
    /// List node visits (over all four lists) found in young / old.
    young_nodes: usize,
    old_nodes: usize,
    objects_promoted: u64,
    bytes_promoted: u64,
}

fn read_list(
    heap: &GenerationalHeap,
    mut cur: Option<ObjectRef>,
    young: &mut usize,
    old: &mut usize,
) -> Vec<i32> {
    let mut out = Vec::new();
    while let Some(n) = cur {
        assert!(out.len() < 10_000, "the list grew a cycle");
        if heap.is_in_young(n.as_ptr()) {
            *young += 1;
        } else {
            *old += 1;
        }
        out.push(heap.get_field(n, 0).as_int().expect("a node payload"));
        cur = match heap.get_field(n, 1) {
            Value::Object(next) => next,
            other => panic!("a node's next field reads {other:?}"),
        };
    }
    out
}

/// A young list of `len` nodes `[payload, next]` with dead objects
/// interleaved (two compact, one legacy, after every node); returns its
/// head.
fn build_list(heap: &GenerationalHeap, base: i32, len: i32) -> ObjectRef {
    let nodes: Vec<ObjectRef> = (0..len)
        .map(|i| {
            let n = heap.alloc_object(ClassId::new(CID), 2);
            heap.set_field(n, 0, Value::Int(base + i));
            heap.alloc_object(ClassId::new(CID), 2);
            heap.alloc_object(ClassId::new(CID), 2);
            heap.alloc_object(ClassId::new(LEGACY), 1);
            n
        })
        .collect();
    for w in nodes.windows(2) {
        heap.set_field(w[0], 1, Value::Object(Some(w[1])));
    }
    heap.set_field(nodes[nodes.len() - 1], 1, Value::Object(None));
    nodes[0]
}

/// Eight non-moving cycles over two young lists: list A hangs off a
/// (pinned) young root holder, list B off three OLD holders (a legacy
/// object, a compact object and a reference array, each on a dirty
/// card). Both lists age and promote (`PROMOTION_AGE` is 3), so fix-ups
/// 3a (the root holder's field), 3b and 3c (the three old holders) all
/// have something to rewrite. Returns the outcome and the census.
fn promoting_scenario(edits: &[(&str, Option<&str>)]) -> (Outcome, [u64; YP_LEN]) {
    // gcg: the scenario promotes on the fixed `PROMOTION_AGE`.
    let heap = GenerationalHeap::with_sizes(1 << 20, 4 << 20).with_fixed_promotion_age();
    register(&heap, CID);
    let root = heap.alloc_object(ClassId::new(LEGACY), 2);
    let head_a = build_list(&heap, 0, 64);
    heap.set_field(root, 0, Value::Object(Some(head_a)));
    let head_b = build_list(&heap, 1000, 32);
    let old_legacy = heap
        .try_alloc_object_old(ClassId::new(LEGACY), 2)
        .expect("old room");
    heap.set_field(old_legacy, 0, Value::Object(Some(head_b)));
    let old_compact = heap
        .try_alloc_object_old(ClassId::new(CID), 2)
        .expect("old room");
    heap.set_field(old_compact, 0, Value::Int(-1));
    heap.set_field(old_compact, 1, Value::Object(Some(head_b)));
    let old_arr = heap
        .try_alloc_array_humongous(ClassId::new(0), ArrayElementType::Reference, 10_000)
        .expect("old gen has room");
    let arr_slot =
        (old_arr.as_ptr() as usize + ARRAY_DATA_OFFSET + 7 * ref_element_size()) as *mut u8;
    // SAFETY: element 7 of the live old array.
    unsafe { write_ref_slot(arr_slot, head_b.as_ptr() as u64) };
    // The barrier's job, done explicitly: every old->young edge is on a
    // dirty card when the first pause starts.
    for holder in [old_legacy, old_compact, old_arr] {
        heap.card_table.mark_dirty(holder.as_ptr() as usize);
    }
    let mut roots = vec![root];
    for cycle in 0..8 {
        for _ in 0..40 {
            heap.alloc_object(ClassId::new(CID), 2);
            heap.alloc_object(ClassId::new(LEGACY), 2);
        }
        let result = cratonvm_types::flags::with_thread_overrides(edits, || {
            non_moving_cycle(&heap, &mut roots)
        });
        assert_eq!(
            result.stats.objects_copied, 0,
            "cycle {cycle}: the non-moving sweep ran"
        );
    }
    let (mut young_nodes, mut old_nodes) = (0, 0);
    let head_of = |v: Value| match v {
        Value::Object(o) => o,
        other => panic!("a holder field reads {other:?}"),
    };
    // SAFETY: element 7 of the live old array.
    let arr_raw = unsafe { read_ref_slot(arr_slot) } as usize;
    let arr_head = if arr_raw == 0 {
        None
    } else {
        // SAFETY: a reference slot the collector maintains.
        Some(unsafe { ObjectRef::from_raw(arr_raw as *mut u8) })
    };
    let (yn, on) = (&mut young_nodes, &mut old_nodes);
    let lists = vec![
        read_list(&heap, head_of(heap.get_field(roots[0], 0)), yn, on),
        read_list(&heap, head_of(heap.get_field(old_legacy, 0)), yn, on),
        read_list(&heap, head_of(heap.get_field(old_compact, 1)), yn, on),
        read_list(&heap, arr_head, yn, on),
    ];
    let census: [u64; YP_LEN] =
        std::array::from_fn(|i| heap.young_premium_census[i].load(Ordering::Relaxed));
    (
        Outcome {
            lists,
            young_nodes,
            old_nodes,
            objects_promoted: heap.stats.objects_promoted.load(Ordering::Relaxed),
            bytes_promoted: heap.stats.bytes_promoted.load(Ordering::Relaxed),
        },
        census,
    )
}

/// Patches 1-3 together against all of them off: the same
/// lists, read back intact through the root and all three old holders,
/// the same nodes promoted, the same promotion accounting. Positive
/// controls: with the switches on, the census was batched, 3a REPLAYED
/// (and never re-walked) and (compact layouts on) the memo answered;
/// with them off, none of that, and 3a re-walked. (gcg g4/yg: patch 4
/// and GP-3, the listed-slot 3c and the map reserve, are deleted.)


/// f2/y review finding 4: a survivor that is side-marked AND carries a
/// stale `GC_FLAG_MARKED` ages by one on a non-moving cycle, like any
/// other survivor (before, neither the selective walk nor the main walk
/// aged it). Control: an unmarked twin ages the same.
#[test]
fn f3q_a_side_marked_survivor_with_a_stale_header_mark_still_ages() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
    let holder = heap.alloc_object(ClassId::new(LEGACY), 2);
    let stale = heap.alloc_object(ClassId::new(LEGACY), 1);
    let twin = heap.alloc_object(ClassId::new(LEGACY), 1);
    heap.set_field(holder, 0, Value::Object(Some(stale)));
    heap.set_field(holder, 1, Value::Object(Some(twin)));
    heap.get_header(stale).add_gc_flags(GC_FLAG_MARKED);
    let mut roots = vec![holder];
    let _ = non_moving_cycle(&heap, &mut roots);
    let child = |i: usize| match heap.get_field(roots[0], i) {
        Value::Object(Some(c)) => c,
        other => panic!("the holder lost child {i}: {other:?}"),
    };
    let (s, t) = (child(0), child(1));
    assert!(
        heap.is_in_young(s.as_ptr()) && heap.is_in_young(t.as_ptr()),
        "a first cycle promotes nothing"
    );
    assert_eq!(
        heap.get_header(t).gc_age(),
        1,
        "control: the twin aged once"
    );
    assert_eq!(
        heap.get_header(s).gc_age(),
        1,
        "the stale-marked survivor was aged by neither pass"
    );
}

// ----- gch h1/yy -------------------------------------------------------

/// `CRATONVM_GEN_SWEEP_PARALLEL_PROMOTION_WALK`: the promoting scenario
/// (fix-ups 3a, 3b and 3c all rewrite) leaves exactly what the serial walk
/// leaves -- the same lists through every holder, the same nodes promoted,
/// the same promotion accounting -- and the 3a replay still proves itself
/// from the summed chunk counts. Both arms run four workers and a 4 KiB
/// anchor stride, so the switch is the only difference. Positive control:
/// the parallel walk engaged; negative control: never with the switch off.


/// The chunk walk itself: one chunk over the arena, and the same arena
/// split at a real object start, decide the same candidates, age bumps and
/// census in the same order; a split one word inside an object is refused
/// (the first chunk's chain overshoots its end); the census classifies a
/// pinned survivor as pinned and an unaged one as unaged.
#[test]
fn yy1_a_promotion_walk_chunk_splits_exactly_on_an_object_start_and_refuses_off_grid() {
    let heap = GenerationalHeap::with_sizes(1 << 20, 1 << 20);
    let objs: Vec<ObjectRef> = (0..32)
        .map(|_| heap.alloc_object(ClassId::new(LEGACY), 2))
        .collect();
    let (base, used, free) = {
        let from = heap.lock_young_from();
        (
            from.base_ptr() as usize,
            from.used(),
            from.free_blocks_sorted(),
        )
    };
    let bits = cratonvm_gc_core::young_mark::YoungMarkBits::new(base, used);
    for o in objs.iter().step_by(3) {
        bits.try_mark(o.as_ptr() as usize);
    }
    let side_sorted = bits.collect_marked();
    let pinned: FxHashSet<usize> = [objs[3].as_ptr() as usize].into_iter().collect();
    // Everything tenurable: candidates are the marked, unpinned objects.
    let tenure = PromotionWalkCtx {
        from_base: base,
        used,
        free_blocks: &free,
        side_bits: &bits,
        side_sorted: &side_sorted,
        pinned: &pinned,
        selective_on: true,
        force_tenure: true,
        promotion_age: 1,
        size_memo: false,
        fuse: None,
    };
    // Nothing tenurable: no candidates, every marked object aged once.
    let young = PromotionWalkCtx {
        from_base: base,
        used,
        free_blocks: &free,
        side_bits: &bits,
        side_sorted: &side_sorted,
        pinned: &pinned,
        selective_on: true,
        force_tenure: false,
        promotion_age: 16,
        size_memo: false,
        fuse: None,
    };
    type Joined = (Vec<(usize, usize)>, Vec<usize>, u64, u64, u64, usize, usize);
    let join = |chunks: &[PromotionWalkChunk]| -> Joined {
        let mut j: Joined = (Vec::new(), Vec::new(), 0, 0, 0, 0, 0);
        for c in chunks {
            j.0.extend_from_slice(&c.candidates);
            j.1.extend_from_slice(&c.age_bumps);
            j.2 += c.census_candidates;
            j.3 += c.census_unaged;
            j.4 += c.census_pinned;
            j.5 += c.side_marked;
            j.6 += c.header_marked;
        }
        j
    };
    let mid = objs[16].as_ptr() as usize - base;
    let whole = parallel_promotion_walk(&tenure, &[0, used], 1, None).expect("one chunk");
    let split = parallel_promotion_walk(&tenure, &[0, mid, used], 1, None)
        .expect("a split at an object start is on the grid");
    assert_eq!(whole.len(), 1);
    assert_eq!(split.len(), 2);
    assert_eq!(join(&whole), join(&split));
    let marked = objs.iter().step_by(3).count();
    let j = join(&whole);
    assert_eq!(
        j.0.len(),
        marked - 1,
        "every marked object but the pinned one"
    );
    for &(addr, size) in &j.0 {
        assert_ne!(
            addr,
            objs[3].as_ptr() as usize,
            "the pinned object is no candidate"
        );
        // SAFETY: a candidate is a parsed object start of the live arena.
        assert_eq!(
            size,
            gen_object_total_size(unsafe { &*(addr as *const ObjectHeader) })
        );
    }
    assert_eq!(
        (j.2, j.3, j.4),
        (marked as u64, 0, 1),
        "census: candidates/unaged/pinned"
    );
    assert_eq!(j.5, marked, "every side mark is a grid object");
    assert!(j.1.is_empty(), "nothing is aged when everything tenures");
    let aged = join(&parallel_promotion_walk(&young, &[0, mid, used], 1, None).expect("grid"));
    assert!(aged.0.is_empty());
    assert_eq!(aged.1.len(), marked);
    assert_eq!(aged.3, marked as u64, "all unaged");
    // One word inside `objs[16]`: the first chunk cannot land on its end.
    assert!(parallel_promotion_walk(&tenure, &[0, mid + 8, used], 1, None).is_none());
}

/// What `card_seed_scenario` leaves: every element's payload, read back
/// through the old array, and how many elements now point into old gen.
fn card_seed_scenario(edits: &[(&str, Option<&str>)]) -> (Vec<i32>, usize, [u64; YP_LEN]) {
    const N: usize = 6_000;
    let heap = GenerationalHeap::with_sizes(4 << 20, 8 << 20).with_fixed_promotion_age();
    let arr = heap
        .try_alloc_array_humongous(ClassId::new(0), ArrayElementType::Reference, N as u32)
        .expect("old gen has room");
    let slot_of = |i: usize| {
        (arr.as_ptr() as usize + ARRAY_DATA_OFFSET + i * ref_element_size()) as *mut u8
    };
    for i in 0..N {
        let o = heap.alloc_object(ClassId::new(LEGACY), 1);
        heap.set_field(o, 0, Value::Int(i as i32));
        // SAFETY: element `i` of the live old array.
        unsafe { write_ref_slot(slot_of(i), o.as_ptr() as u64) };
        heap.alloc_object(ClassId::new(LEGACY), 1); // a dead neighbour
    }
    // The barrier's job, done explicitly (a dirty header card reads the
    // whole array).
    heap.card_table.mark_dirty(arr.as_ptr() as usize);
    let mut roots: Vec<ObjectRef> = Vec::new();
    for cycle in 0..5 {
        for _ in 0..64 {
            heap.alloc_object(ClassId::new(LEGACY), 2);
        }
        let result = cratonvm_types::flags::with_thread_overrides(edits, || {
            non_moving_cycle(&heap, &mut roots)
        });
        assert_eq!(
            result.stats.objects_copied, 0,
            "cycle {cycle}: the non-moving sweep ran"
        );
    }
    let mut old = 0usize;
    let payloads = (0..N)
        .map(|i| {
            // SAFETY: element `i` of the live old array.
            let raw = unsafe { read_ref_slot(slot_of(i)) } as usize;
            assert_ne!(raw, 0, "element {i} lost its referent");
            // SAFETY: a reference slot the collector maintains.
            let r = unsafe { ObjectRef::from_raw(raw as *mut u8) };
            if !heap.is_in_young(r.as_ptr()) {
                old += 1;
            }
            heap.get_field(r, 0).as_int().expect("a payload")
        })
        .collect();
    let census: [u64; YP_LEN] =
        std::array::from_fn(|i| heap.young_premium_census[i].load(Ordering::Relaxed));
    (payloads, old, census)
}

// `CRATONVM_GEN_SWEEP_PARALLEL_CARD_SEED`: 6000 old->young edges from one
// old array, seeded on four workers, keep every referent and promote the
// same ones as the serial seed. Positive control: the parallel seed ran;
// negative control: not with the switch off.


// gci i1/yg: `CRATONVM_GEN_SWEEP_PARALLEL_CARD_SEED` is default ON (unset
// reads on, and an unset run seeds on the pool), `=0` kills it.

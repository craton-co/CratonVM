use super::*;

fn oref(n: &str) -> VType {
    VType::ObjectRef(Arc::from(n))
}

// -- Locals epoch: row reuse must equal row recomputation ------------------

/// SBR round 5, lane verify: `record` (with the frame's locals epoch) reuses
/// the previous locals row when nothing touched the locals. Drive a frame
/// through stores, a same-value store, a declared-frame adoption, a pad and
/// an in-place replace, record every step both ways, and require identical
/// maps.
#[test]
fn epoch_row_reuse_matches_recomputing_every_row() {
    use crate::verify_frame::VerificationFrame;
    use std::sync::Arc as A;

    let this: A<str> = A::from("Foo");
    let (_, mut frame) =
        VerificationFrame::initial_frames(&this, "m", "(Ljava/lang/String;J)V", false, 6, 4);
    let mut declared = frame.clone();
    declared.locals[3] = oref("Bar");
    declared.locals[5] = oref("Baz");

    let mut fast = MethodTypeMapsBuilder::new(6, 4);
    let mut slow = MethodTypeMapsBuilder::new(6, 4);
    let mut step = 0u32;
    let mut record = |frame: &VerificationFrame, fast: &mut MethodTypeMapsBuilder| {
        fast.record(step, frame);
        slow.record_slots(step, &frame.locals, &frame.stack);
        step += 1;
    };

    record(&frame, &mut fast); // initial
    frame.push(VType::Int).unwrap();
    record(&frame, &mut fast); // stack change only: locals row reused
    frame.local_store(4, oref("Qux")).unwrap();
    record(&frame, &mut fast); // a store: recomputed
    frame.local_store(4, oref("Qux")).unwrap();
    record(&frame, &mut fast); // same value: reused, still identical
    frame.local_store(4, VType::Int).unwrap();
    record(&frame, &mut fast); // reference -> int: recomputed
    frame.adopt_declared(&declared, 6);
    record(&frame, &mut fast); // declared frame adopted
    frame.pad_locals_to(8);
    record(&frame, &mut fast); // widened past max_locals
    for slot in frame.locals.iter_mut() {
        if *slot == oref("Bar") {
            *slot = VType::Null;
        }
    }
    record(&frame, &mut fast); // in-place replace
    frame.push(VType::Float).unwrap();
    record(&frame, &mut fast);

    let (fast, slow) = (fast.finish(true), slow.finish(true));
    assert_eq!(fast.entry_count(), slow.entry_count());
    assert_eq!(fast.locals_fully_described(), slow.locals_fully_described());
    assert_eq!(fast.fast_path_veto(), slow.fast_path_veto());
    for pc in 0..step {
        let (a, b) = (
            fast.frame_map_at(pc).unwrap(),
            slow.frame_map_at(pc).unwrap(),
        );
        assert_eq!(
            a.locals.as_bytes(),
            b.locals.as_bytes(),
            "locals row at {pc}"
        );
        assert_eq!(a.stack.as_bytes(), b.stack.as_bytes(), "stack row at {pc}");
        assert_eq!(a.stack_depth, b.stack_depth, "depth at {pc}");
    }
}

// -- Row encoding --------------------------------------------------------

/// zs s2/fp2: a method signature is 28 bytes (thin names), and the
/// by-name lookup answers as before, sharing the caller's interned `Arc`s.
#[test]
fn zs2fp2_method_sig_is_28_bytes_and_lookup_is_unchanged() {
    assert_eq!(std::mem::size_of::<MethodSig>(), 28);
    let name: Arc<str> = Arc::from("run");
    let d1: Arc<str> = Arc::from("()V");
    let d2: Arc<str> = Arc::from("(I)V");
    let maps = ClassTypeMaps::new(vec![
        (Arc::clone(&name), Arc::clone(&d1), None),
        (Arc::clone(&name), Arc::clone(&d2), None),
    ]);
    assert_eq!(maps.method_index_of("run", "()V"), Some(0));
    assert_eq!(maps.method_index_of("run", "(I)V"), Some(1));
    assert_eq!(maps.method_index_of("run", "(J)V"), None);
    assert!(maps.sigs[1].name.ptr_eq_arc(&name));
    // The two signatures and the local `name` hold it.
    assert_eq!(Arc::strong_count(&name), 3);
    drop(maps);
    assert_eq!(Arc::strong_count(&name), 1);
}

/// zs s1/fp: the four tables share one exact-size block and the header is
/// a quarter of the old 120 bytes; every answer is the same, on the
/// narrow arms and on the wide-pc / wide-depth ones.
#[test]
fn zs1fp_method_type_maps_are_one_block_with_a_small_header() {
    assert!(
        std::mem::size_of::<MethodTypeMaps>() <= 32,
        "{}",
        std::mem::size_of::<MethodTypeMaps>()
    );
    assert_eq!(
        std::mem::size_of::<Option<MethodTypeMaps>>(),
        std::mem::size_of::<MethodTypeMaps>(),
        "the Box niche keeps an absent method's slot the same size"
    );
    for (pc_base, depth) in [(0u32, 3usize), (70_000, 3), (0, 300), (70_000, 300)] {
        let max_stack = (depth + 1) as u16;
        let mut b = MethodTypeMapsBuilder::new(9, max_stack);
        let locals = [
            oref("A"),
            VType::Int,
            VType::Int,
            VType::Int,
            VType::Int,
            VType::Int,
            VType::Int,
            VType::Int,
            oref("B"),
        ];
        let mut stack = vec![VType::Int; depth];
        stack[0] = oref("C");
        b.record_slots(pc_base, &locals, &[]);
        b.record_slots(pc_base + 3, &locals[..1], &stack);
        b.record_slots(pc_base + 7, &locals, &stack[..1]);
        let m = b.finish(true);
        let pc_width = if pc_base > u16::MAX as u32 { 4 } else { 2 };
        let depth_width = if depth > 255 { 2 } else { 1 };
        let stack_stride = stride_for(max_stack);
        assert_eq!(
            m.heap_bytes(),
            3 * (pc_width + 2 + stack_stride + depth_width),
            "pc={pc_base} depth={depth}"
        );
        assert_eq!(m.entry_count(), 3);
        assert_eq!(m.pc_at_index(1), Some(pc_base + 3));
        assert_eq!(m.pc_at_index(3), None);
        assert_eq!(m.index_of_pc(pc_base + 7), Some(2));
        assert!(!m.has_pc(pc_base + 1));
        let f = m.frame_map_at(pc_base + 3).unwrap();
        assert_eq!(f.stack_depth as usize, depth);
        assert!(f.stack.get(0) && !f.stack.get(1));
        assert!(
            f.locals.get(0) && !f.locals.get(8),
            "a short row's tail is clear"
        );
        assert_eq!(m.stack_depth_at(pc_base + 7), Some(1));
        assert_eq!(m.stack_depth_at(pc_base), Some(0));
        let l = m.local_oops_at(pc_base + 7).unwrap();
        assert_eq!(l.iter_set().collect::<Vec<_>>(), vec![0, 8]);
        assert!(!m.locals_fully_described(), "the pc+3 row was short");
        assert!(m.safe_for_fast_path());
    }
    // No rows: one empty block, every lookup `None`.
    let m = MethodTypeMapsBuilder::new(4, 4).finish(true);
    assert_eq!(m.heap_bytes(), 0);
    assert!(m.oop_map_at(0).is_none() && m.pc_at_index(0).is_none());
    assert!(m.locals_fully_described());
}

/// JIT round 23 wave 5 (`CRATONVM_VERIFY_TYPE_MAP_BUFFER_REUSE`): maps
/// built through a reused [`TypeMapSpare`] are byte-identical to fresh
/// builders' ones, for a larger method followed by a smaller one (the spare
/// carries more capacity and stale contents, which must not leak).
#[test]
fn a_reused_spare_builds_identical_maps() {
    let first = |b: &mut MethodTypeMapsBuilder| {
        for pc in 0..9u32 {
            b.record_slots(
                pc * 2,
                &[oref("java/lang/String"), VType::Int, VType::Null, VType::Top],
                &[oref("A"), VType::Int],
            );
        }
    };
    let second = |b: &mut MethodTypeMapsBuilder| {
        b.record_slots(0, &[VType::Int, oref("B")], &[]);
        b.record_slots(3, &[oref("C"), VType::Int], &[VType::Int]);
    };
    let same = |a: &MethodTypeMaps, b: &MethodTypeMaps| {
        assert_eq!(a.data, b.data);
        assert_eq!(a.rows, b.rows);
        assert_eq!(a.flags, b.flags);
        assert_eq!(a.veto, b.veto);
        assert_eq!((a.max_locals, a.max_stack), (b.max_locals, b.max_stack));
    };
    let mut fresh1 = MethodTypeMapsBuilder::new(4, 2);
    first(&mut fresh1);
    let fresh1 = fresh1.finish(true);
    let mut fresh2 = MethodTypeMapsBuilder::new(2, 1);
    second(&mut fresh2);
    let fresh2 = fresh2.finish(false);

    let mut spare = TypeMapSpare::default();
    let mut reused1 = MethodTypeMapsBuilder::with_spare(4, 2, &mut spare);
    first(&mut reused1);
    let reused1 = reused1.finish_into_spare(true, &mut spare);
    assert!(spare.local_bytes.capacity() > 0, "the spare keeps the capacity");
    let mut reused2 = MethodTypeMapsBuilder::with_spare(2, 1, &mut spare);
    second(&mut reused2);
    let reused2 = reused2.finish_into_spare(false, &mut spare);
    same(&fresh1, &reused1);
    same(&fresh2, &reused2);
}

#[test]
fn refs_in_locals_are_recorded() {
    let mut b = MethodTypeMapsBuilder::new(4, 2);
    b.record_slots(
        0,
        &[
            oref("java/lang/String"),
            VType::Int,
            VType::Null,
            VType::Top,
        ],
        &[],
    );
    let m = b.finish(true);

    let (locals, stack) = m.oop_map_at(0).expect("map at pc 0");
    assert_eq!(locals.len(), 4);
    assert!(locals.get(0), "slot 0 is an ObjectRef");
    assert!(!locals.get(1), "slot 1 is Int");
    assert!(locals.get(2), "slot 2 is Null — still a reference slot");
    assert!(!locals.get(3), "slot 3 is Top");
    assert!(stack.is_all_clear());
    assert_eq!(locals.iter_set().collect::<Vec<_>>(), vec![0, 2]);
    // Out-of-range reads answer false rather than panicking.
    assert!(!locals.get(99));
}

#[test]
fn uninitialized_this_counts_as_a_reference() {
    // Between `new` and `<init>` the slot holds a real heap pointer, so a
    // moving collector MUST see it.
    let mut b = MethodTypeMapsBuilder::new(2, 2);
    b.record_slots(0, &[VType::UninitializedThis, VType::Uninitialized(7)], &[]);
    let m = b.finish(true);
    let locals = m.local_oops_at(0).unwrap();
    assert!(locals.get(0));
    assert!(locals.get(1));
}

#[test]
fn long_spans_two_local_slots_and_one_stack_slot() {
    // locals: [ref, Long, Top, Int]  — JVMS slot space, cat-2 = 2 slots.
    // stack:  [ref, Long, Top]       — JVMS; runtime is [ref, long] = 2.
    let mut b = MethodTypeMapsBuilder::new(4, 3);
    b.record_slots(
        0,
        &[oref("Foo"), VType::Long, VType::Top, VType::Int],
        &[oref("Bar"), VType::Long, VType::Top],
    );
    let m = b.finish(true);

    let map = m.frame_map_at(0).expect("map at pc 0");
    // Locals keep the JVMS two-slot layout, matching `Frame::locals`.
    assert!(map.locals.get(0));
    assert!(!map.locals.get(1), "Long is not a reference");
    assert!(
        !map.locals.get(2),
        "upper half of a Long is not a reference"
    );
    assert!(!map.locals.get(3));

    // The operand stack is compressed: the Long/Top pair is ONE runtime
    // slot, so the depth is 2, not 3.
    assert_eq!(map.stack_depth, 2);
    assert!(map.stack.get(0), "Bar is a reference at runtime slot 0");
    assert!(!map.stack.get(1), "the long occupies runtime slot 1");
    assert_eq!(m.stack_depth_at(0), Some(2));
    assert!(m.safe_for_fast_path());
}

#[test]
fn malformed_category_two_pair_vetoes() {
    let mut b = MethodTypeMapsBuilder::new(1, 2);
    // `Long` without its `Top` upper half.
    b.record_slots(0, &[VType::Top], &[VType::Long, VType::Int]);
    let m = b.finish(true);
    assert!(!m.safe_for_fast_path());
    assert_eq!(m.fast_path_veto(), Some(FastPathVeto::MalformedCategory2));
}

// -- Merge / handler shapes ---------------------------------------------

#[test]
fn branch_target_merge_narrows_to_the_merged_type() {
    // Two predecessors reach pc 8 with `String` and `Integer` in local 1;
    // the verifier merges to `Object`, which is still a reference, so the
    // map at the merge point must keep the bit set. Local 2 merges a ref
    // with an Int, which lands on `Top` — the bit must be CLEAR there,
    // because a moving collector must not treat an int as a pointer.
    let mut b = MethodTypeMapsBuilder::new(3, 1);
    b.record_slots(0, &[oref("A"), oref("java/lang/String"), oref("A")], &[]);
    b.record_slots(4, &[oref("A"), oref("java/lang/Integer"), VType::Int], &[]);
    // pc 8 = the merge point.
    b.record_slots(8, &[oref("A"), oref("java/lang/Object"), VType::Top], &[]);
    let m = b.finish(true);

    assert_eq!(m.entry_count(), 3);
    let merged = m.local_oops_at(8).unwrap();
    assert!(merged.get(0));
    assert!(merged.get(1), "merged reference stays a reference");
    assert!(!merged.get(2), "ref ⊔ int = Top, which is NOT scannable");

    // Each predecessor keeps its own row.
    assert!(m.local_oops_at(0).unwrap().get(2));
    assert!(!m.local_oops_at(4).unwrap().get(2));
    // An address that is not an instruction start is unproven.
    assert!(m.oop_map_at(6).is_none());
    assert!(m.oop_map_at(9).is_none());
}

#[test]
fn exception_handler_entry_has_the_thrown_object_on_the_stack() {
    // A handler entry's frame is `locals unchanged, stack = [throwable]`.
    let mut b = MethodTypeMapsBuilder::new(2, 2);
    b.record_slots(0, &[oref("Self"), VType::Int], &[]);
    b.record_slots(
        12,
        &[oref("Self"), VType::Int],
        &[oref("java/lang/Throwable")],
    );
    let m = b.finish(true);

    let handler = m.frame_map_at(12).expect("handler map");
    assert_eq!(handler.stack_depth, 1);
    assert!(handler.stack.get(0), "the caught throwable is a root");
    assert!(handler.locals.get(0));
    assert!(!handler.locals.get(1));
    // Depth 0 at method entry.
    assert_eq!(m.stack_depth_at(0), Some(0));
}

// -- safe_for_fast_path --------------------------------------------------

#[test]
fn subroutine_method_is_not_safe_for_fast_path() {
    let mut b = MethodTypeMapsBuilder::new(4, 2);
    b.record_slots(0, &vec![VType::Top; 4], &[]);
    b.observe_instruction(&Instruction::Jsr(10));
    let m = b.finish(true);
    assert!(
        !m.safe_for_fast_path(),
        "jsr/ret must veto the unchecked fast path"
    );
    assert_eq!(m.fast_path_veto(), Some(FastPathVeto::Subroutine));
    // The rows are still usable for GC scanning.
    assert!(m.oop_map_at(0).is_some());
}

#[test]
fn out_of_range_local_index_is_not_safe_for_fast_path() {
    let mut b = MethodTypeMapsBuilder::new(2, 2);
    b.record_slots(0, &[VType::Top, VType::Top], &[]);
    // `astore 5` with max_locals = 2 would index `Frame::locals` out of
    // bounds through `set_local_unchecked`.
    b.observe_instruction(&Instruction::Astore(5));
    let m = b.finish(true);
    assert!(!m.safe_for_fast_path());
    assert_eq!(m.fast_path_veto(), Some(FastPathVeto::LocalIndexOutOfRange));
}

#[test]
fn category_two_local_needs_both_slots_in_range() {
    // max_locals = 2: `lstore 1` writes slots 1 and 2 → slot 2 is OOB.
    let mut b = MethodTypeMapsBuilder::new(2, 2);
    b.record_slots(0, &[VType::Top, VType::Top], &[]);
    b.observe_instruction(&Instruction::Lstore(1));
    assert!(b.is_vetoed());
    assert!(!b.finish(true).safe_for_fast_path());

    // max_locals = 3: slots 1 and 2 are both in range.
    let mut ok = MethodTypeMapsBuilder::new(3, 2);
    ok.record_slots(0, &vec![VType::Top; 3], &[]);
    ok.observe_instruction(&Instruction::Lstore(1));
    assert!(ok.finish(true).safe_for_fast_path());
}

#[test]
fn incomplete_walk_is_not_safe_for_fast_path() {
    let mut b = MethodTypeMapsBuilder::new(1, 1);
    b.record_slots(0, &[VType::Top], &[]);
    let m = b.finish(/* walk_complete */ false);
    assert!(!m.safe_for_fast_path());
    assert_eq!(m.fast_path_veto(), Some(FastPathVeto::IncompleteWalk));
}

#[test]
fn plain_method_is_safe_for_fast_path() {
    let mut b = MethodTypeMapsBuilder::new(3, 2);
    b.record_slots(0, &[oref("Self"), VType::Int, VType::Top], &[]);
    b.observe_instruction(&Instruction::Aload(0));
    b.record_slots(1, &[oref("Self"), VType::Int, VType::Top], &[oref("Self")]);
    b.observe_instruction(&Instruction::Astore(2));
    let m = b.finish(true);
    assert!(m.safe_for_fast_path());
    assert_eq!(m.fast_path_veto(), None);
}

#[test]
fn unsorted_pcs_discard_the_map_entirely() {
    let mut b = MethodTypeMapsBuilder::new(1, 1);
    b.record_slots(10, &[VType::Top], &[]);
    b.record_slots(4, &[VType::Top], &[]);
    let m = b.finish(true);
    assert_eq!(m.entry_count(), 0, "a wrong oop map is worse than none");
    assert!(!m.safe_for_fast_path());
    assert_eq!(m.fast_path_veto(), Some(FastPathVeto::UnsortedPcs));
    assert!(m.oop_map_at(10).is_none());
}

#[test]
fn duplicate_pc_records_are_idempotent() {
    let mut b = MethodTypeMapsBuilder::new(1, 1);
    b.record_slots(0, &[oref("A")], &[]);
    b.record_slots(0, &[VType::Int], &[]);
    let m = b.finish(true);
    assert_eq!(m.entry_count(), 1);
    assert!(m.local_oops_at(0).unwrap().get(0), "first record wins");
}

// -- Clamping contract ---------------------------------------------------

#[test]
fn stack_clamps_to_the_runtime_depth() {
    // At an invoke pc the verifier still sees the arguments on the stack,
    // but the interpreter has already popped them into the callee frame.
    let mut b = MethodTypeMapsBuilder::new(1, 4);
    b.record_slots(
        0,
        &[VType::Top],
        &[oref("Recv"), oref("Arg0"), oref("Arg1")],
    );
    let m = b.finish(true);
    let map = m.frame_map_at(0).unwrap();
    assert_eq!(map.stack_depth, 3);
    // While the callee runs, the caller's real depth is 0.
    let live = map.stack_for_runtime_depth(0);
    assert_eq!(live.len(), 0);
    assert_eq!(live.iter_set().count(), 0);
    // Halfway (a hypothetical partial pop) still reads only real slots.
    assert_eq!(map.stack_for_runtime_depth(1).iter_set().count(), 1);
    // A runtime depth larger than the map never over-reads.
    assert_eq!(map.stack_for_runtime_depth(99).len(), 3);
}

#[test]
fn locals_clamp_to_the_runtime_array_length() {
    let mut b = MethodTypeMapsBuilder::new(8, 1);
    b.record_slots(
        0,
        &[
            oref("A"),
            oref("B"),
            VType::Int,
            VType::Top,
            VType::Top,
            VType::Top,
            VType::Top,
            oref("C"),
        ],
        &[],
    );
    let m = b.finish(true);
    let map = m.frame_map_at(0).unwrap();
    assert_eq!(map.locals.iter_set().collect::<Vec<_>>(), vec![0, 1, 7]);
    assert_eq!(
        map.locals_for_runtime_len(2).iter_set().collect::<Vec<_>>(),
        vec![0, 1]
    );
}

// -- Sizing --------------------------------------------------------------

#[test]
fn representation_is_compact() {
    // 20 instruction starts, 6 locals, max_stack 4 → 1-byte strides,
    // u16 pcs, u8 depths.
    let mut b = MethodTypeMapsBuilder::new(6, 4);
    for i in 0..20u32 {
        b.record_slots(i * 2, &vec![oref("A"); 6], &[]);
    }
    let m = b.finish(true);
    assert_eq!(m.entry_count(), 20);
    // 20*2 (pcs) + 20*1 (locals) + 20*1 (stack) + 20*1 (depths) = 100.
    assert_eq!(m.heap_bytes(), 100);
    // ~5 bytes of row data per instruction start, plus a fixed header.
    assert!(m.heap_bytes() / m.entry_count() <= 5);
    assert!(m.total_bytes() < 400);
}

#[test]
fn wide_locals_use_a_wider_stride() {
    let mut b = MethodTypeMapsBuilder::new(70, 9);
    b.record_slots(0, &vec![VType::Int; 70], &[]);
    let m = b.finish(true);
    // ceil(70/8) = 9 local bytes, ceil(9/8) = 2 stack bytes, 2 pc, 1 depth.
    assert_eq!(m.heap_bytes(), 9 + 2 + 2 + 1);
    assert_eq!(m.local_oops_at(0).unwrap().len(), 70);
}

#[test]
fn short_locals_row_flags_the_method_as_not_fully_described() {
    let mut b = MethodTypeMapsBuilder::new(4, 1);
    b.record_slots(0, &[oref("A"), VType::Int], &[]); // only 2 of 4 slots
    let m = b.finish(true);
    assert!(
        !m.locals_fully_described(),
        "an undescribed local tail must be advertised, not silently zeroed"
    );
    // A fully padded row keeps the flag set.
    let mut ok = MethodTypeMapsBuilder::new(4, 1);
    ok.record_slots(0, &[oref("A"), VType::Int, VType::Top, VType::Top], &[]);
    assert!(ok.finish(true).locals_fully_described());
}

#[test]
fn zero_locals_and_zero_stack_allocate_nothing() {
    let mut b = MethodTypeMapsBuilder::new(0, 0);
    b.record_slots(0, &[], &[]);
    let m = b.finish(true);
    assert_eq!(m.entry_count(), 1);
    let (l, s) = m.oop_map_at(0).unwrap();
    assert!(l.is_empty() && s.is_empty());
    assert_eq!(m.stack_depth_at(0), Some(0));
    // pcs (2) + depths (1) only.
    assert_eq!(m.heap_bytes(), 3);
}

#[test]
fn out_of_spec_pcs_widen_to_u32() {
    let mut b = MethodTypeMapsBuilder::new(1, 1);
    b.record_slots(0, &[VType::Top], &[]);
    b.record_slots(70_000, &[oref("A")], &[]);
    let m = b.finish(true);
    assert!(m.local_oops_at(70_000).unwrap().get(0));
    assert!(m.oop_map_at(1).is_none());
    // 2 pcs × 4 bytes now, plus 2 rows × (1 local + 1 stack + 1 depth).
    assert_eq!(m.heap_bytes(), 8 + 2 + 2 + 2);
}

// -- Store ---------------------------------------------------------------

fn tiny_maps(safe: bool) -> MethodTypeMaps {
    let mut b = MethodTypeMapsBuilder::new(1, 1);
    b.record_slots(0, &[oref("A")], &[]);
    if !safe {
        b.mark_unsafe_for_fast_path(FastPathVeto::Subroutine);
    }
    b.finish(true)
}

#[test]
fn store_round_trips_by_index_and_by_name() {
    let id = ClassId::new(900_001);
    let entries = vec![
        (
            Arc::<str>::from("<init>"),
            Arc::<str>::from("()V"),
            Some(tiny_maps(true)),
        ),
        (
            Arc::<str>::from("run"),
            Arc::<str>::from("()V"),
            Some(tiny_maps(false)),
        ),
        // An abstract/native method contributes an index but no maps.
        (Arc::<str>::from("nat"), Arc::<str>::from("()I"), None),
    ];
    assert!(publish_class_type_maps(id, ClassTypeMaps::new(entries)));

    assert_eq!(verification_status(id), VerificationStatus::Verified);
    assert!(type_maps_for(id, 0).unwrap().safe_for_fast_path());
    assert!(!type_maps_for(id, 1).unwrap().safe_for_fast_path());
    assert!(type_maps_for(id, 2).is_none(), "no Code attribute");
    assert!(type_maps_for(id, 3).is_none(), "index out of range");

    assert!(type_maps_for_named(id, "run", "()V").is_some());
    assert!(
        type_maps_for_named(id, "run", "()I").is_none(),
        "descriptor must match too"
    );
    assert_eq!(
        class_type_maps(id).unwrap().method_index_of("nat", "()I"),
        Some(2)
    );
    assert!(class_type_maps(id).unwrap().heap_bytes() > 0);
}

#[test]
fn unknown_class_is_unknown_not_empty() {
    let id = ClassId::new(900_777);
    assert_eq!(verification_status(id), VerificationStatus::Unknown);
    assert!(class_type_maps(id).is_none());
    assert!(type_maps_for(id, 0).is_none());
}

#[test]
fn skipped_verification_is_distinguishable_from_verified() {
    let id = ClassId::new(900_055);
    assert!(mark_class_verification_skipped(id));
    assert_eq!(verification_status(id), VerificationStatus::Skipped);
    // A skipped class yields NO maps — never a wrong-but-plausible one.
    assert!(type_maps_for(id, 0).is_none());
    assert!(type_maps_for_named(id, "run", "()V").is_none());

    // If the class is later verified for real, the marker is upgraded.
    let entries = vec![(
        Arc::<str>::from("run"),
        Arc::<str>::from("()V"),
        Some(tiny_maps(true)),
    )];
    assert!(publish_class_type_maps(id, ClassTypeMaps::new(entries)));
    assert_eq!(verification_status(id), VerificationStatus::Verified);
    assert!(type_maps_for(id, 0).is_some());
}

#[test]
fn duplicate_publish_keeps_the_winner() {
    let id = ClassId::new(900_123);
    let mk = || {
        ClassTypeMaps::new(vec![(
            Arc::<str>::from("m"),
            Arc::<str>::from("()V"),
            Some(tiny_maps(true)),
        )])
    };
    assert!(publish_class_type_maps(id, mk()));
    assert!(
        !publish_class_type_maps(id, mk()),
        "second publish is a no-op"
    );
    assert!(type_maps_for(id, 0).is_some());

    // Redefinition explicitly replaces.
    assert!(replace_class_type_maps(
        id,
        ClassTypeMaps::new(vec![(
            Arc::<str>::from("m"),
            Arc::<str>::from("()V"),
            Some(tiny_maps(false)),
        )])
    ));
    assert!(!type_maps_for(id, 0).unwrap().safe_for_fast_path());
}

fn one_method(name: &str, safe: bool) -> ClassTypeMaps {
    ClassTypeMaps::new(vec![(
        Arc::<str>::from(name),
        Arc::<str>::from("()I"),
        Some(tiny_maps(safe)),
    )])
}

/// Two VMs number their classes from the same origin, so one class id
/// names a different class in each. VM B's vetoed maps must not be
/// shadowed by VM A's proven maps for the same `(id, name, descriptor)` —
/// in the old process-wide table B's publish was dropped as a "duplicate"
/// and B's fast-path gate read A's "proven".
#[test]
fn two_vm_stores_keep_their_own_maps_for_one_class_id() {
    let a = TypeMapStore::new();
    let b = TypeMapStore::new();
    let id = ClassId::new(7);
    assert!(a.publish(id, one_method("m", true)));
    assert!(
        b.publish(id, one_method("m", false)),
        "not a duplicate in B"
    );
    assert!(a
        .type_maps_for_named(id, "m", "()I")
        .unwrap()
        .safe_for_fast_path());
    assert!(!b
        .type_maps_for_named(id, "m", "()I")
        .unwrap()
        .safe_for_fast_path());
    // A class published in one VM is unknown in another.
    let only_in_c = ClassId::new(0x0030_0007);
    let c = TypeMapStore::new();
    assert!(c.publish(only_in_c, one_method("m", true)));
    assert_eq!(
        a.verification_status(only_in_c),
        VerificationStatus::Unknown
    );
    assert_eq!(
        c.verification_status(only_in_c),
        VerificationStatus::Verified
    );
    assert_eq!(b.class_count(), 1);
}

/// Replace retires the displaced maps instead of freeing them under a
/// possible reader, and the store frees everything, retired maps included,
/// when it drops (the old process-wide table leaked every VM's maps).
#[test]
fn a_replaced_store_answers_the_new_maps_and_drops_cleanly() {
    let store = TypeMapStore::new();
    let id = ClassId::new(3);
    assert!(store.mark_skipped(id));
    assert_eq!(store.verification_status(id), VerificationStatus::Skipped);
    assert!(
        store.publish(id, one_method("m", true)),
        "upgrades the marker"
    );
    assert!(store.replace(id, one_method("m", false)));
    assert!(!store.type_maps_for(id, 0).unwrap().safe_for_fast_path());
    assert_eq!(store.class_count(), 1);
    assert!(store.heap_bytes() > 0);
    assert_eq!(store.retired.lock().map(|r| r.len()).unwrap_or(0), 2);
    drop(store);
}

/// A VM store keeps its maps to itself: nothing it publishes appears in
/// the process store. (Until wave 8 an armed `CRATONVM_DBG_VERIFY_OOP_MAPS`
/// mirrored every publish there, first writer winning across VMs.)
#[test]
fn a_vm_store_never_writes_the_process_store() {
    let id = ClassId::new(0x0030_0101);
    let store = TypeMapStore::new();
    assert!(store.publish(id, one_method("not_mirrored", true)));
    assert!(store
        .type_maps_for_named(id, "not_mirrored", "()I")
        .is_some());
    assert!(type_maps_for_named(id, "not_mirrored", "()I").is_none());
}

#[test]
fn class_ids_beyond_the_addressable_range_degrade_gracefully() {
    let id = ClassId::new(u32::MAX);
    assert_eq!(verification_status(id), VerificationStatus::Unknown);
    assert!(!mark_class_verification_skipped(id));
    assert!(type_maps_for(id, 0).is_none());
}

/// zs s3/fp3: a store that drops rows keeps every per-method verdict and
/// answers every row query `None` (the conservative "unproven"); a plain
/// store keeps the rows. The kill switch and the debug readers' key are
/// read by `rows_dropped_by_default`, which a class store applies.
#[test]
fn zs3fp3_a_row_dropping_store_keeps_the_verdicts() {
    let id = ClassId::new(0x0030_0201);
    let keep = TypeMapStore::new();
    assert!(!keep.drops_rows());
    let drop = TypeMapStore::new();
    drop.set_drop_rows(true);
    for (safe, name) in [(true, "s"), (false, "u")] {
        assert!(keep.publish(
            ClassId::new(id.as_u32() + safe as u32),
            one_method(name, safe)
        ));
        assert!(drop.publish(
            ClassId::new(id.as_u32() + safe as u32),
            one_method(name, safe)
        ));
        let k = keep
            .type_maps_for(ClassId::new(id.as_u32() + safe as u32), 0)
            .unwrap();
        let d = drop
            .type_maps_for(ClassId::new(id.as_u32() + safe as u32), 0)
            .unwrap();
        assert_eq!(k.safe_for_fast_path(), safe);
        assert_eq!(d.safe_for_fast_path(), k.safe_for_fast_path());
        assert_eq!(d.fast_path_veto(), k.fast_path_veto());
        assert_eq!(d.max_locals(), k.max_locals());
        assert_eq!(d.max_stack(), k.max_stack());
        assert_eq!(d.locals_fully_described(), k.locals_fully_described());
        // Positive control: the plain store has the row.
        assert_eq!(k.entry_count(), 1);
        assert!(k.local_oops_at(0).is_some());
        assert_eq!(d.entry_count(), 0);
        assert_eq!(d.heap_bytes(), 0);
        assert!(d.oop_map_at(0).is_none());
        assert!(d.frame_map_at(0).is_none());
        assert!(d.local_oops_at(0).is_none());
        assert!(d.stack_oops_at(0).is_none());
        assert!(d.stack_depth_at(0).is_none());
        assert!(!d.has_pc(0));
        assert!(drop
            .type_maps_for_named(ClassId::new(id.as_u32() + safe as u32), name, "()I")
            .is_some());
    }
    assert!(drop.heap_bytes() < keep.heap_bytes());
    // Switched back off, later installs keep their rows.
    drop.set_drop_rows(false);
    let later = ClassId::new(id.as_u32() + 5);
    assert!(drop.publish(later, one_method("k", true)));
    assert_eq!(drop.type_maps_for(later, 0).unwrap().entry_count(), 1);
}

/// Phase 5 fast boot (lane p5-define): a verdict-only builder finishes to
/// exactly what `strip_rows` leaves of the full builder's maps, over
/// random row sequences that hit every veto (unsorted / repeated pcs,
/// over-wide locals, short locals, stack past `max_stack`, broken
/// category-2 pairs) and the row budget.
#[test]
fn p5_verdict_only_builder_matches_stripped_full_maps() {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n.max(1)
    };
    let pick = |k: u64| match k {
        0 => VType::Int,
        1 => VType::Top,
        2 => VType::Long,
        3 => VType::Null,
        _ => oref("A"),
    };
    for case in 0..4000u32 {
        let max_locals = next(12) as u16 + if case % 97 == 0 { 3000 } else { 0 };
        let max_stack = next(10) as u16 + if case % 89 == 0 { 3000 } else { 0 };
        let mut full = MethodTypeMapsBuilder::new(max_locals, max_stack);
        let mut verdict = MethodTypeMapsBuilder::new(max_locals, max_stack);
        verdict.set_verdict_only();
        let rows = next(if case % 50 == 0 { 3000 } else { 40 });
        let mut pc = 0u32;
        for _ in 0..rows {
            pc = match next(20) {
                0 => pc.saturating_sub(1),
                1 => pc,
                _ => pc + 1 + next(4) as u32,
            };
            let nl = (u64::from(max_locals) + next(3)).saturating_sub(1).min(4000);
            let locals: Vec<VType> = (0..nl).map(|_| pick(next(5))).collect();
            let ns = next(u64::from(max_stack) + 3).min(4000);
            let stack: Vec<VType> = (0..ns).map(|_| pick(next(5))).collect();
            full.record_slots(pc, &locals, &stack);
            verdict.record_slots(pc, &locals, &stack);
        }
        let complete = next(4) != 0;
        let mut full = full.finish(complete);
        full.strip_rows();
        let verdict = verdict.finish(complete);
        assert_eq!(full.veto, verdict.veto, "case {case}");
        assert_eq!(full.flags, verdict.flags, "case {case}");
        assert_eq!(full.rows, verdict.rows, "case {case}");
        assert_eq!(full.data, verdict.data, "case {case}");
        assert_eq!(
            (full.max_locals, full.max_stack),
            (verdict.max_locals, verdict.max_stack)
        );
    }
}

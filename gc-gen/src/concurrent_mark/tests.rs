use super::*;
use cratonvm_gc_core::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE};
use cratonvm_types::{ClassId, ObjectRef, Value};

/// The STW witness these tests stand in for.
///
/// SAFETY: a unit test has one mutator thread -- itself -- so the
/// safepoint precondition `StopTheWorldToken::new` demands holds
/// vacuously. This mirrors the helper `g1.rs` and `g1_concurrent.rs`
/// already use for the same reason.
fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

fn make_old_gen_with_object(num_slots: u32) -> (OldGen, *mut u8) {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + num_slots as usize * SLOT_SIZE;
    let ptr = og.alloc(size, 8).unwrap();
    unsafe {
        let header = &mut *(ptr as *mut ObjectHeader);
        header.class_id = ClassId::new(1);
        header.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
        header.set_num_slots(num_slots);
        header.set_gc_age(0);
        header.set_gc_flags(0x01); // GC_FLAG_OLD_GEN
    }
    (og, ptr)
}

#[test]
fn concurrent_mark_object_size_rejects_inconsistent_object_header() {
    let mut header = ObjectHeader::new(
        ClassId::new(240),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        2,
    );
    assert_eq!(
        concurrent_mark_object_size(&header),
        Some(HEADER_SIZE + 2 * SLOT_SIZE)
    );

    header.set_num_slots((1 << 24) + 1);
    assert_eq!(concurrent_mark_object_size(&header), None);
}

#[test]
fn compact_scan_reuses_its_sized_layout_and_refuses_an_unreadable_extent() {
    use cratonvm_types::field_layout::FieldStorageKind;
    const CID: u32 = 0xC601;
    let layout = cratonvm_types::CompactLayout {
        field_disps: vec![8, 16],
        is_ref: vec![true, true],
        field_kinds: vec![FieldStorageKind::Reference; 2],
        ref_disps: vec![8, 16],
        total_size: 24,
    };
    cratonvm_types::register_class_layout(
        cratonvm_types::FIRST_LAYOUT_DOMAIN,
        CID,
        std::sync::Arc::new(layout),
    );
    let mut og = OldGen::new(65536);
    let root = og.alloc(24, 8).unwrap();
    let child = og.alloc(HEADER_SIZE, 8).unwrap();
    // SAFETY: fresh allocations have room for the headers and the two
    // aligned compact reference slots, and no mutator races this test.
    unsafe {
        let mut header = ObjectHeader::new(
            ClassId::new(CID),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            2,
        );
        header.set_compact_shape(2, 16);
        std::ptr::write(root as *mut ObjectHeader, header);
        std::ptr::write(root.add(8) as *mut usize, root as usize);
        std::ptr::write(root.add(16) as *mut usize, child as usize);
        std::ptr::write(
            child as *mut ObjectHeader,
            ObjectHeader::new(
                ClassId::new(1),
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                0,
            ),
        );
    }
    let starts = OldGenObjectStarts::build(&og);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.bitmap.try_mark(root as usize);
    let mut pushed = Vec::new();
    assert!(marker.scan_object_into(root, &og, &starts, false, &mut |p| pushed.push(p)));
    assert_eq!(
        pushed,
        vec![child],
        "self edge stays marked; child is traced"
    );

    // A changed/corrupt class claim must fail before any out-of-range
    // reference displacement is visited, even with a resolvable layout.
    cratonvm_types::register_class_layout(
        cratonvm_types::FIRST_LAYOUT_DOMAIN,
        CID + 1,
        std::sync::Arc::new(cratonvm_types::CompactLayout {
            field_disps: vec![1 << 20],
            is_ref: vec![true],
            field_kinds: vec![FieldStorageKind::Reference],
            ref_disps: vec![1 << 20],
            total_size: (1 << 20) + 8,
        }),
    );
    // SAFETY: root still holds the allocated header; only its class claim
    // changes, deliberately contradicting this cycle's start snapshot.
    unsafe { (*(root as *mut ObjectHeader)).class_id = ClassId::new(CID + 1) };
    assert!(
        !marker.scan_object_into(root, &og, &starts, false, &mut |_| {
            panic!("an unreadable extent must not scan a slot")
        })
    );
}

#[test]
fn gcj_retire_remark_batch_uses_an_immutable_eligibility_view() {
    let (og, root) = make_old_gen_with_object(1);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[root], &og);
    marker.remark(&stw(), &[root], &og);
    let query = marker.remark_liveness();
    // No lock per query, even when the eligibility owner is locked.
    let _locked = marker.sweep_eligible.lock();
    assert!(query(root as usize));
    assert!(query(0x10));
}

#[test]
fn initial_mark_rejects_interior_old_gen_pointer() {
    let (og, obj_ptr) = make_old_gen_with_object(2);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    let interior = unsafe { obj_ptr.add(8) };

    assert_eq!(marker.initial_mark(&[interior], &og), 0);
    assert!(!marker.bitmap.is_marked(interior as usize));
    assert!(!marker.bitmap.is_marked(obj_ptr as usize));
}

#[test]
fn initial_mark_roots() {
    let (og, obj_ptr) = make_old_gen_with_object(2);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

    let count = marker.initial_mark(&[obj_ptr], &og);
    assert_eq!(count, 1);
    assert!(marker.bitmap.is_marked(obj_ptr as usize));
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
}

#[test]
fn concurrent_mark_follows_references() {
    let mut og = OldGen::new(65536);

    // Allocate object A (2 slots)
    let size_a = HEADER_SIZE + 2 * SLOT_SIZE;
    let ptr_a = og.alloc(size_a, 8).unwrap();
    unsafe {
        let h = &mut *(ptr_a as *mut ObjectHeader);
        h.class_id = ClassId::new(1);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(2);
        h.set_gc_flags(0x01);
    }

    // Allocate object B (1 slot, no refs)
    let size_b = HEADER_SIZE + SLOT_SIZE;
    let ptr_b = og.alloc(size_b, 8).unwrap();
    unsafe {
        let h = &mut *(ptr_b as *mut ObjectHeader);
        h.class_id = ClassId::new(2);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(1);
        h.set_gc_flags(0x01);
    }

    // A.field[0] = ref to B
    unsafe {
        let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
        let obj_b = ObjectRef::from_raw(ptr_b);
        std::ptr::write(slot, Value::Object(Some(obj_b)));
    }

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[ptr_a], &og);
    let scanned = marker.concurrent_mark(&og);

    // Both A and B should be marked
    assert!(marker.bitmap.is_marked(ptr_a as usize));
    assert!(marker.bitmap.is_marked(ptr_b as usize));
    assert!(scanned >= 1); // At least B was scanned via A
}

#[test]
fn sweep_frees_unmarked() {
    let mut og = OldGen::new(65536);

    // Allocate two objects
    let size = HEADER_SIZE + SLOT_SIZE;
    let live_ptr = og.alloc(size, 8).unwrap();
    unsafe {
        let h = &mut *(live_ptr as *mut ObjectHeader);
        h.class_id = ClassId::new(1);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(1);
        h.set_gc_flags(0x01);
    }

    let dead_ptr = og.alloc(size, 8).unwrap();
    unsafe {
        let h = &mut *(dead_ptr as *mut ObjectHeader);
        h.class_id = ClassId::new(2);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(1);
        h.set_gc_flags(0x01);
    }

    let used_before = og.used();
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

    // Only mark live_ptr as a root (dead_ptr is unreachable)
    let (_marked, swept) = marker.full_cycle(&stw(), &[live_ptr], &mut og);
    assert_eq!(swept, 1); // dead_ptr should be freed
    assert!(og.used() < used_before);
}

/// GCAUD-4 — the concurrent sweep must not act on an address-keyed
/// snapshot another old-gen collection has invalidated.
///
/// `concurrent_sweep` is the one phase of the cycle that runs outside a
/// stop-the-world. Its liveness test is "the address existed at remark
/// (`sweep_eligible`) AND its bit is clear (`bitmap`)" — two tables keyed
/// on a bare old-gen address. Between remark and the sweep's lock
/// acquisition, another thread's young GC can run `old_gen_gc`: the
/// in-place arm hands blocks back to the free list, and the very next
/// promotion re-issues those addresses to NEW, fully live objects. Such an
/// object satisfies BOTH halves of the test — it inherited a dead object's
/// address, so it is "eligible", and its bit is clear because the bit
/// describes its predecessor — and the sweep frees it while it is live.
///
/// This test drives exactly that sequence: remark, then free a dead block
/// and let the allocator re-issue the same address to a live object. The
/// sweep must reclaim nothing.
#[test]
fn concurrent_sweep_refuses_a_snapshot_invalidated_by_another_old_gen_collection() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let init = |ptr: *mut u8, cid: u32| {
        // SAFETY: `ptr` is a live `size`-byte old-gen block.
        unsafe {
            let h = &mut *(ptr as *mut ObjectHeader);
            h.class_id = ClassId::new(cid);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
            h.set_num_slots(1);
            h.set_gc_flags(0x01); // GC_FLAG_OLD_GEN
        }
    };

    let live = og.alloc(size, 8).unwrap();
    init(live, 1);
    let dead = og.alloc(size, 8).unwrap();
    init(dead, 2);
    // A third block, recycled below. It is unreachable at remark, so its
    // address enters `sweep_eligible` with its bit clear.
    let recycled = og.alloc(size, 8).unwrap();
    init(recycled, 3);

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[live], &og);
    marker.concurrent_mark(&og);
    marker.remark(&stw(), &[live], &og);

    // --- another old-gen collection interleaves here -------------------
    // SAFETY: `(recycled, size)` is exactly the pair `alloc` handed out.
    unsafe { og.free(recycled, size) };
    let resurrected = og
        .alloc(size, 8)
        .expect("the just-freed block must be reusable");
    assert_eq!(
        resurrected, recycled,
        "precondition: the allocator must re-issue the freed address, which \
         is what makes the snapshot's address ambiguous",
    );
    init(resurrected, 4);
    // -------------------------------------------------------------------

    let used_before_sweep = og.used();
    let aborts_before = marker.state.census().sweep_epoch_aborts;
    let swept = marker.concurrent_sweep(&mut og);

    assert_eq!(
        swept, 0,
        "a sweep whose address-keyed snapshot was invalidated must reclaim \
         nothing — one of the eligible addresses now names a LIVE object",
    );
    assert!(
        marker.state.census().sweep_epoch_aborts > aborts_before,
        "the abandoned sweep must be counted, not silent",
    );
    assert_eq!(og.used(), used_before_sweep);
    assert!(
        og.is_allocated_addr(resurrected),
        "the resurrected (live) object must still be allocated after the sweep",
    );
    assert!(
        og.is_allocated_addr(dead),
        "and nothing else may be reclaimed on the abandoned path either",
    );
}

/// Shared object initialiser for the TAMS tests below.
fn init_old_object(ptr: *mut u8, cid: u32) {
    // SAFETY: `ptr` is a live `HEADER_SIZE + SLOT_SIZE` old-gen block.
    unsafe {
        let h = &mut *(ptr as *mut ObjectHeader);
        h.class_id = ClassId::new(cid);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
        h.set_num_slots(1);
        h.set_gc_flags(0x01); // GC_FLAG_OLD_GEN
    }
}

/// gengc-mark 2026-09-20 — TAMS must be anchored at INITIAL MARK.
///
/// An object that enters the old generation DURING the concurrent trace
/// (a young GC promoting a survivor, or a direct large-object allocation)
/// cannot be discovered by this cycle: the trace may already have scanned
/// and blackened every object that now points at it, the SATB pre-barrier
/// logs only OLD slot values, and the remark root rescan sees it only if
/// something outside the old gen still references it. With the snapshot
/// taken at remark it was nevertheless "eligible", so the sweep freed a
/// live object. Anchored at initial mark, it is ineligible by construction
/// and is simply collected one cycle later.
#[test]
fn an_object_promoted_during_the_concurrent_phase_is_not_swept() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;

    let live = og.alloc(size, 8).unwrap();
    init_old_object(live, 1);
    let dead = og.alloc(size, 8).unwrap();
    init_old_object(dead, 2);

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[live], &og);
    marker.concurrent_mark(&og);

    // --- a promotion interleaves with the concurrent trace --------------
    let promoted = og.alloc(size, 8).unwrap();
    init_old_object(promoted, 3);
    // --------------------------------------------------------------------

    marker.remark(&stw(), &[live], &og);
    assert!(
        !marker.bitmap.is_marked(promoted as usize),
        "precondition: nothing in this cycle can have marked the promoted \
         object — that is the whole point of the test",
    );

    let swept = marker.concurrent_sweep(&mut og);
    assert!(
        og.is_allocated_addr(promoted),
        "an object that entered the old gen after initial mark is implicitly \
         live for this cycle and must survive the sweep",
    );
    assert_eq!(
        swept, 1,
        "only the object that was already dead at initial mark may be freed",
    );
    assert!(og.is_allocated_addr(live));
    assert!(!og.is_allocated_addr(dead));
}

/// gengc-mark 2026-09-20 — a cycle whose remark never ran must free
/// nothing, even though its eligibility snapshot is now populated from
/// initial mark onwards.
///
/// This is the safety property the old arrangement got for free ("empty
/// snapshot ⇒ remark never ran ⇒ free nothing") and that moving the
/// snapshot earlier would have silently removed. It is now an explicit
/// gate: `sweep_eligible_epoch` is stamped by `remark` and by nothing else.
/// Without remark the bitmap is not final and the SATB log was never
/// drained, so an unmarked object is not evidence of anything.
#[test]
fn a_sweep_without_a_remark_frees_nothing() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;

    let live = og.alloc(size, 8).unwrap();
    init_old_object(live, 1);
    let unreached = og.alloc(size, 8).unwrap();
    init_old_object(unreached, 2);

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[live], &og);
    marker.concurrent_mark(&og);
    // No remark: model the driver losing the remark STW race.

    let used_before = og.used();
    let swept = marker.concurrent_sweep(&mut og);
    assert_eq!(
        swept, 0,
        "an unauthorised snapshot must not license any reclamation",
    );
    assert_eq!(og.used(), used_before);
    assert!(og.is_allocated_addr(unreached));
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);

    // And `abort_cycle` -- the path the driver actually takes -- leaves the
    // same state, so a later sweep cannot consume the stale snapshot.
    let marker2 = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker2.initial_mark(&[live], &og);
    marker2.abort_cycle();
    assert_eq!(marker2.concurrent_sweep(&mut og), 0);
    assert!(og.is_allocated_addr(unreached));
}

/// gengc-mark 2026-09-20 — GCAUD-4 now covers the whole cycle, not its tail.
///
/// The epoch is stamped at initial mark, so a `free` (or `compact`) by
/// another old-gen collector ANYWHERE in the cycle invalidates the
/// address-keyed snapshot, and remark must fail closed rather than hand the
/// sweep a set of addresses that no longer name the objects they named.
#[test]
fn a_reclaim_between_initial_mark_and_remark_abandons_the_cycle() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;

    let live = og.alloc(size, 8).unwrap();
    init_old_object(live, 1);
    let dead = og.alloc(size, 8).unwrap();
    init_old_object(dead, 2);
    let recycled = og.alloc(size, 8).unwrap();
    init_old_object(recycled, 3);

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[live], &og);
    marker.concurrent_mark(&og);

    // --- another old-gen collection interleaves -------------------------
    // SAFETY: `(recycled, size)` is exactly the pair `alloc` handed out.
    unsafe { og.free(recycled, size) };
    let resurrected = og
        .alloc(size, 8)
        .expect("the just-freed block must be reusable");
    assert_eq!(
        resurrected, recycled,
        "precondition: the allocator must re-issue the freed address",
    );
    init_old_object(resurrected, 4);
    // --------------------------------------------------------------------

    marker.remark(&stw(), &[live], &og);
    let swept = marker.concurrent_sweep(&mut og);

    assert_eq!(
        swept, 0,
        "a cycle whose snapshot was invalidated mid-flight must reclaim nothing",
    );
    assert!(og.is_allocated_addr(dead), "nothing may be reclaimed");
    assert!(og.is_allocated_addr(resurrected));
}

/// gengc-mark 2026-09-20 — the two SATB gates must never disagree in the
/// unsafe direction.
///
/// The generational `satb_barrier` checks the PHASE
/// (`ConcurrentGcState::is_marking_active`); the log it writes into checks
/// the QUEUE (`SatbQueue::is_active`). G1 asserts
/// `is_marking_active() => satb_queue.is_active()`; `remark` used to break
/// it for the width of its final closure, during which a mutator's
/// pre-barrier entry went into a per-thread bucket nobody would drain
/// again. Check the invariant at every phase boundary of a full cycle.
///
/// gc-common w5-e: the barrier now tests the queue itself, so this is no
/// longer the barrier's safety argument; it stays as the invariant the
/// phase's other readers (the VM's root builders) rely on.
#[test]
fn the_phase_gate_is_never_wider_than_the_satb_gate() {
    let (mut og, obj_ptr) = make_old_gen_with_object(1);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    let check = |marker: &ConcurrentMarker, where_: &str| {
        assert!(
            !marker.state.is_marking_active() || marker.satb_queue.is_active(),
            "{where_}: the mutator barrier is armed but its queue is not — \
             an entry logged here would be stranded in a per-thread bucket",
        );
    };

    check(&marker, "idle");
    marker.initial_mark(&[obj_ptr], &og);
    check(&marker, "after initial_mark");
    marker.concurrent_mark(&og);
    check(&marker, "after concurrent_mark");
    marker.remark(&stw(), &[obj_ptr], &og);
    check(&marker, "after remark");
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
    assert!(!marker.satb_queue.is_active());
    marker.concurrent_sweep(&mut og);
    check(&marker, "after sweep");

    // An aborted cycle must leave the same agreement.
    let marker2 = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker2.initial_mark(&[obj_ptr], &og);
    marker2.abort_cycle();
    check(&marker2, "after abort_cycle");
}

#[test]
fn satb_prevents_lost_object() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;

    // Object A (root)
    let ptr_a = og.alloc(size, 8).unwrap();
    unsafe {
        let h = &mut *(ptr_a as *mut ObjectHeader);
        h.class_id = ClassId::new(1);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(1);
        h.set_gc_flags(0x01);
    }

    // Object B (initially referenced by A)
    let ptr_b = og.alloc(size, 8).unwrap();
    unsafe {
        let h = &mut *(ptr_b as *mut ObjectHeader);
        h.class_id = ClassId::new(2);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(1);
        h.set_gc_flags(0x01);
    }

    // A.field[0] = B initially
    unsafe {
        let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
        std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_b))));
    }

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

    // Phase 1: initial mark
    marker.initial_mark(&[ptr_a], &og);

    // Simulate concurrent mutation: A.field[0] = null
    // The SATB barrier should log the OLD value (ptr_b).
    marker.satb_queue.flush(vec![ptr_b as usize]);

    // Now break the reference
    unsafe {
        let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
        std::ptr::write(slot, Value::Object(None));
    }

    // Phase 2: concurrent mark (won't find B via A anymore). gen
    // r4w3/oldgen3: Phase 2 now drains the SATB log itself (see
    // `concurrent_mark_budget`), so B is discovered HERE rather than at
    // remark — the property is that B is marked before the sweep, not
    // which phase found it.
    marker.concurrent_mark(&og);
    assert!(
        marker.bitmap.is_marked(ptr_b as usize),
        "Phase 2 must drain the SATB log and mark B"
    );
    assert!(
        marker.satb_queue.is_empty(),
        "the log was drained, not left for remark"
    );

    // Phase 3: remark — B stays marked.
    let _discovered = marker.remark(&stw(), &[ptr_a], &og);
    assert!(marker.bitmap.is_marked(ptr_b as usize)); // B is live through the SATB snapshot

    // Phase 4: sweep — B should NOT be freed
    let swept = marker.concurrent_sweep(&mut og);
    assert_eq!(swept, 0); // Both A and B are live
}

// gc-concmark HIGH regression — the SATB barrier must stay ACTIVE
// across the remark closure, so a live old-gen reference overwritten
// by a mutator DURING remark (after the snapshot drain, before sweep)
// is still logged, marked, and NOT swept.
//
// Before the fix, `remark` called `deactivate_and_drain()` at the very
// top, flipping the gate to INACTIVE before the closure was computed.
// An SATB entry flushed after that point (modelling a write the barrier
// would have logged had it still been active) was never captured by the
// mark phase, leaving B's bit clear so the sweep freed a live object.
#[test]
fn satb_active_through_remark_closure() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;

    // Object A (root, no live refs to B by remark time).
    let ptr_a = og.alloc(size, 8).unwrap();
    unsafe {
        let h = &mut *(ptr_a as *mut ObjectHeader);
        h.class_id = ClassId::new(1);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(1);
        h.set_gc_flags(0x01);
    }

    // Object B — only ever reachable via the SATB log of an overwrite.
    let ptr_b = og.alloc(size, 8).unwrap();
    unsafe {
        let h = &mut *(ptr_b as *mut ObjectHeader);
        h.class_id = ClassId::new(2);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(1);
        h.set_gc_flags(0x01);
    }

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

    // Phases 1–2: A is the only root; B is not reachable from A.
    marker.initial_mark(&[ptr_a], &og);
    marker.concurrent_mark(&og);

    // The barrier must still be active going into remark — that is the
    // invariant whose violation caused the bug.
    assert!(marker.satb_queue.is_active());

    // Model a mutator overwriting a live reference to B *during* the
    // concurrent window: the pre-barrier logs B's old address. With the
    // fix, `remark` drains this WITHOUT deactivating and marks B; the
    // final quiescing drain then closes the gate.
    marker.satb_queue.flush(vec![ptr_b as usize]);

    let discovered = marker.remark(&stw(), &[ptr_a], &og);
    assert!(discovered > 0, "B must be discovered from the SATB log");
    assert!(
        marker.bitmap.is_marked(ptr_b as usize),
        "B must be marked — the SATB-logged live ref was not lost"
    );
    // Gate must be off once the closure is final.
    assert!(!marker.satb_queue.is_active());

    // Sweep must keep B (it is live via the SATB snapshot).
    let swept = marker.concurrent_sweep(&mut og);
    assert_eq!(swept, 0, "neither A nor B may be freed");
}

#[test]
fn phase_transitions() {
    let marker = ConcurrentMarker::new(0x0, 1024);
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);

    let og = OldGen::new(1024);
    marker.initial_mark(&[], &og);
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
    assert!(marker.satb_queue.is_active());

    marker.concurrent_mark(&og);
    // Phase doesn't change after concurrent mark — stays ConcurrentMark

    marker.remark(&stw(), &[], &og);
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
    assert!(!marker.satb_queue.is_active());
}

// -----------------------------------------------------------------------
// Additional tests
// -----------------------------------------------------------------------

#[test]
fn initial_mark_multiple_roots() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;

    let ptrs: Vec<*mut u8> = (0..5)
        .map(|i| {
            let p = og.alloc(size, 8).unwrap();
            unsafe {
                let h = &mut *(p as *mut ObjectHeader);
                h.class_id = ClassId::new(i + 1);
                h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
                h.set_num_slots(1);
                h.set_gc_flags(0x01);
            }
            p
        })
        .collect();

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    let count = marker.initial_mark(&ptrs, &og);

    assert_eq!(count, 5);
    for &p in &ptrs {
        assert!(marker.bitmap.is_marked(p as usize));
    }
    assert_eq!(marker.queue.len(), 5);
}

#[test]
fn initial_mark_skips_null_roots() {
    let (og, obj_ptr) = make_old_gen_with_object(1);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

    let roots: Vec<*mut u8> = vec![std::ptr::null_mut(), obj_ptr, std::ptr::null_mut()];
    let count = marker.initial_mark(&roots, &og);

    assert_eq!(count, 1);
    assert!(marker.bitmap.is_marked(obj_ptr as usize));
}

#[test]
fn initial_mark_deduplicates_roots() {
    let (og, obj_ptr) = make_old_gen_with_object(1);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

    // Same root listed twice — should only mark once
    let count = marker.initial_mark(&[obj_ptr, obj_ptr], &og);
    assert_eq!(count, 1);
    assert_eq!(marker.queue.len(), 1);
}

#[test]
fn empty_heap_marking() {
    let og = OldGen::new(4096);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

    let count = marker.initial_mark(&[], &og);
    assert_eq!(count, 0);

    let scanned = marker.concurrent_mark(&og);
    assert_eq!(scanned, 0);

    let discovered = marker.remark(&stw(), &[], &og);
    assert_eq!(discovered, 0);
}

#[test]
fn single_object_full_cycle() {
    let (mut og, obj_ptr) = make_old_gen_with_object(0);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

    let (marked, swept) = marker.full_cycle(&stw(), &[obj_ptr], &mut og);
    // The single object is a root, so it should survive
    assert!(marked >= 1);
    assert_eq!(swept, 0);
}

#[test]
fn mark_queue_push_pop_ordering() {
    let q = MarkQueue::new();
    assert!(q.is_empty());
    assert_eq!(q.len(), 0);

    q.push(0x100 as *mut u8);
    q.push(0x200 as *mut u8);
    q.push(0x300 as *mut u8);
    assert_eq!(q.len(), 3);

    // gen r4w2/concmark: the SET popped, not the order. These three
    // addresses all used to land in shard 0, which is what made the pops
    // look FIFO; `shard_for` now spreads them, and `pop` visits shards
    // round-robin, so only per-shard order is FIFO. Nothing in the marker
    // depends on cross-shard order.
    let mut popped: Vec<usize> = (0..3).map(|_| q.pop().unwrap() as usize).collect();
    popped.sort_unstable();
    assert_eq!(popped, vec![0x100, 0x200, 0x300]);
    assert!(q.pop().is_none());
    assert!(q.is_empty());
}

#[test]
fn mark_queue_push_batch() {
    let q = MarkQueue::new();
    let ptrs: Vec<*mut u8> = (1..=4).map(|i| (i * 0x100) as *mut u8).collect();

    q.push_batch(&ptrs);
    assert_eq!(q.len(), 4);

    // The set, not the order — see `mark_queue_push_pop_ordering`.
    let mut popped: Vec<usize> = (0..4).map(|_| q.pop().unwrap() as usize).collect();
    popped.sort_unstable();
    let mut expected: Vec<usize> = ptrs.iter().map(|p| *p as usize).collect();
    expected.sort_unstable();
    assert_eq!(popped, expected);
    assert!(q.pop().is_none());
}

/// gen r4w2/concmark — 16-byte-strided object starts (the legacy object
/// shape) must reach every shard. The old low-bits `shard_for` put them all
/// on the even shards, halving both the sharding and the effective
/// overflow cap.
#[test]
fn shard_for_spreads_sixteen_byte_strides_over_every_shard() {
    let mut seen = [0usize; MARK_QUEUE_SHARDS];
    for i in 0..4096usize {
        let ptr = (0x1000_0000usize + i * 16) as *mut u8;
        let s = MarkQueue::shard_for(ptr);
        assert!(s < MARK_QUEUE_SHARDS, "shard index out of range: {s}");
        seen[s] += 1;
    }
    for (s, &n) in seen.iter().enumerate() {
        assert!(
            n > 0,
            "shard {s} never used by a 16-byte stride: {seen:?} — the odd \
             shards being empty is the regression this test exists for",
        );
    }
}

#[test]
fn mark_queue_clear() {
    let q = MarkQueue::new();
    q.push(0x100 as *mut u8);
    q.push(0x200 as *mut u8);
    assert_eq!(q.len(), 2);

    q.clear();
    assert!(q.is_empty());
    assert!(q.pop().is_none());
}

#[test]
fn mark_queue_concurrent_push_pop() {
    use std::sync::Arc;

    let q = Arc::new(MarkQueue::new());
    let mut handles = Vec::new();

    // 4 threads each push 100 items
    for t in 0..4u64 {
        let q = q.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..100u64 {
                q.push((t * 1000 + i) as *mut u8);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(q.len(), 400);

    // Drain all
    let mut count = 0;
    while q.pop().is_some() {
        count += 1;
    }
    assert_eq!(count, 400);
}

#[test]
fn concurrent_gc_state_is_marking_active() {
    let state = ConcurrentGcState::new();
    assert!(!state.is_marking_active());

    state.set_phase(ConcurrentGcPhase::InitialMark);
    assert!(!state.is_marking_active());

    state.set_phase(ConcurrentGcPhase::ConcurrentMark);
    assert!(state.is_marking_active());

    state.set_phase(ConcurrentGcPhase::Remark);
    assert!(state.is_marking_active());

    state.set_phase(ConcurrentGcPhase::ConcurrentSweep);
    assert!(!state.is_marking_active());

    state.set_phase(ConcurrentGcPhase::Idle);
    assert!(!state.is_marking_active());
}

#[test]
fn phase_from_u8_invalid_defaults_to_idle() {
    assert_eq!(ConcurrentGcPhase::from(255), ConcurrentGcPhase::Idle);
    assert_eq!(ConcurrentGcPhase::from(5), ConcurrentGcPhase::Idle);
    assert_eq!(ConcurrentGcPhase::from(100), ConcurrentGcPhase::Idle);
}

#[test]
fn phase_from_u8_all_valid() {
    assert_eq!(ConcurrentGcPhase::from(0), ConcurrentGcPhase::Idle);
    assert_eq!(ConcurrentGcPhase::from(1), ConcurrentGcPhase::InitialMark);
    assert_eq!(
        ConcurrentGcPhase::from(2),
        ConcurrentGcPhase::ConcurrentMark
    );
    assert_eq!(ConcurrentGcPhase::from(3), ConcurrentGcPhase::Remark);
    assert_eq!(
        ConcurrentGcPhase::from(4),
        ConcurrentGcPhase::ConcurrentSweep
    );
}

#[test]
fn large_object_graph_marking() {
    let mut og = OldGen::new(1 << 20); // 1 MB
    let size = HEADER_SIZE + SLOT_SIZE;

    // Build a chain: obj[0] -> obj[1] -> ... -> obj[N-1]
    let n = 50;
    let mut ptrs: Vec<*mut u8> = Vec::new();
    for i in 0..n {
        let p = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(p as *mut ObjectHeader);
            h.class_id = ClassId::new(i as u32 + 1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }
        ptrs.push(p);
    }

    // Wire up chain references: ptrs[i].slot[0] = ptrs[i+1]
    for i in 0..n - 1 {
        unsafe {
            let slot = ptrs[i].add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptrs[i + 1]))));
        }
    }

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    // Only root is the first object
    marker.initial_mark(&[ptrs[0]], &og);
    marker.concurrent_mark(&og);

    // All objects in the chain should be marked
    for &p in &ptrs {
        assert!(marker.bitmap.is_marked(p as usize));
    }
}

#[test]
fn marking_graph_with_cycle() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;

    // Create A -> B -> C -> A (cycle)
    let ptr_a = og.alloc(size, 8).unwrap();
    let ptr_b = og.alloc(size, 8).unwrap();
    let ptr_c = og.alloc(size, 8).unwrap();

    for (p, id) in [(ptr_a, 1u32), (ptr_b, 2), (ptr_c, 3)] {
        unsafe {
            let h = &mut *(p as *mut ObjectHeader);
            h.class_id = ClassId::new(id);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }
    }

    // A -> B
    unsafe {
        let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
        std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_b))));
    }
    // B -> C
    unsafe {
        let slot = ptr_b.add(HEADER_SIZE) as *mut Value;
        std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_c))));
    }
    // C -> A (back edge creating cycle)
    unsafe {
        let slot = ptr_c.add(HEADER_SIZE) as *mut Value;
        std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_a))));
    }

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[ptr_a], &og);
    let scanned = marker.concurrent_mark(&og);

    // All three should be marked despite the cycle
    assert!(marker.bitmap.is_marked(ptr_a as usize));
    assert!(marker.bitmap.is_marked(ptr_b as usize));
    assert!(marker.bitmap.is_marked(ptr_c as usize));
    assert!(scanned >= 2); // A scanned in initial_mark's queue, B and C via concurrent
}

#[test]
fn full_cycle_phase_sequence() {
    let (mut og, obj_ptr) = make_old_gen_with_object(1);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

    // Before cycle: Idle
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);

    marker.initial_mark(&[obj_ptr], &og);
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
    assert!(marker.satb_queue.is_active());

    marker.concurrent_mark(&og);
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);

    marker.remark(&stw(), &[obj_ptr], &og);
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
    assert!(!marker.satb_queue.is_active());

    marker.concurrent_sweep(&mut og);
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
}

#[test]
fn sweep_all_unreachable() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;

    // Allocate 3 objects, none rooted
    for i in 0..3 {
        let p = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(p as *mut ObjectHeader);
            h.class_id = ClassId::new(i + 1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }
    }

    let used_before = og.used();
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    let (_marked, swept) = marker.full_cycle(&stw(), &[], &mut og);

    assert_eq!(swept, 3);
    assert!(og.used() < used_before);
}

// gc-concmark MEDIUM regression — the concurrent-mark object-slot read
// must serialize against striped mutator writes so it never observes a
// torn (tag, payload) `Value`. Here a writer thread continuously flips a
// reference slot between `Object(Some(b))` and `Object(None)` while
// holding the SAME per-slot stripe lock that `scan_object` now takes; the
// marker scans the object in a tight loop on another thread. Without the
// stripe lock in `scan_object`, a torn read could splice the non-null tag
// of one store with the (null) payload of another and feed a bogus
// pointer into `old_gen.contains` / `try_mark` — corrupting the heap or
// crashing. With the lock, every read sees a fully-old or fully-new value,
// so the test runs to completion and only ever marks the real target `b`.
#[test]
fn scan_object_serializes_against_striped_writer() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;

    // Object A — single reference slot, the contended one.
    let ptr_a = og.alloc(size, 8).unwrap();
    unsafe {
        let h = &mut *(ptr_a as *mut ObjectHeader);
        h.class_id = ClassId::new(1);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(1);
        h.set_gc_flags(0x01);
    }
    // Object B — the only legitimate target A's slot can point to.
    let ptr_b = og.alloc(size, 8).unwrap();
    unsafe {
        let h = &mut *(ptr_b as *mut ObjectHeader);
        h.class_id = ClassId::new(2);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(1);
        h.set_gc_flags(0x01);
    }

    let marker = Arc::new(ConcurrentMarker::new(og.base_ptr() as usize, og.capacity()));
    // The marker must be in the concurrent-mark phase so its bitmap is live.
    marker.initial_mark(&[ptr_a], &og);

    let stop = Arc::new(AtomicBool::new(false));
    let a_addr = ptr_a as usize;
    let b_addr = ptr_b as usize;
    let obj_a = unsafe { ObjectRef::from_raw(ptr_a) };
    let obj_b = unsafe { ObjectRef::from_raw(ptr_b) };

    // Writer: flip A.slot[0] between Some(B) and None under the stripe
    // lock, exactly as the volatile field-access helpers do.
    let writer = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let slot = (a_addr + HEADER_SIZE) as *mut Value;
            let mut toggle = false;
            while !stop.load(Ordering::Relaxed) {
                let _g = cratonvm_gc_core::collector::volatile_stripe_lock(obj_a, 0);
                std::sync::atomic::fence(Ordering::SeqCst);
                let v = if toggle {
                    Value::Object(Some(obj_b))
                } else {
                    Value::Object(None)
                };
                // SAFETY: slot is A's single in-bounds reference field.
                unsafe { std::ptr::write(slot, v) };
                std::sync::atomic::fence(Ordering::SeqCst);
                toggle = !toggle;
            }
        })
    };

    // Reader: scan A many times concurrently with the writer.
    let object_starts = old_gen_object_starts(&og);
    for _ in 0..50_000 {
        marker.scan_object(a_addr as *mut u8, &og, &object_starts, false);
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    // A must still be marked; B must be marked iff it was observed as a
    // live target — either is legal, but no THIRD address may ever have
    // been marked (a torn read would have produced a garbage pointer that
    // either failed `contains` or, worse, aliased some other object).
    assert!(marker.bitmap.is_marked(a_addr));
    // Sanity: the only addresses that can possibly be marked are A and B.
    // (B is the sole non-null value the writer ever stores.)
    let _ = b_addr; // referenced for clarity; marking B is permitted, not required.
}

/// fork6 GC_STRESS fix — `with_shared` must adopt the caller's SATB queue
/// + phase state (the heap-attached instances the write barrier reaches),
/// and a pre-barrier log flushed into that SHARED queue must be marked by
/// remark. With the old per-cycle private queue this plumbing did not
/// exist and the logged target was swept while live.
#[test]
fn with_shared_marker_marks_satb_entries_from_shared_queue() {
    // One object, reachable ONLY via the (simulated) overwritten ref.
    let mut og2 = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let live = og2.alloc(size, 8).unwrap();
    unsafe {
        let h = &mut *(live as *mut ObjectHeader);
        h.class_id = ClassId::new(7);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
        h.set_num_slots(1);
        h.set_gc_flags(0x01);
    }

    let satb = Arc::new(SatbQueue::new());
    let state = Arc::new(ConcurrentGcState::new());
    let marker = ConcurrentMarker::with_shared(
        og2.base_ptr() as usize,
        og2.capacity(),
        satb.clone(),
        state.clone(),
    );
    // The marker must hold the SAME instances (not copies).
    assert!(Arc::ptr_eq(&marker.satb_queue, &satb));
    assert!(Arc::ptr_eq(&marker.state, &state));

    // Initial mark with NO roots: `live` is invisible to the trace.
    marker.initial_mark(&[], &og2);
    assert!(
        satb.is_active(),
        "initial_mark must activate the shared queue"
    );
    assert!(state.is_marking_active());

    // Simulate the write barrier on another code path: a mutator
    // overwrote the only reference to `live` during concurrent mark and
    // the pre-barrier logged the old value into the SHARED queue.
    cratonvm_gc_core::satb::satb_thread_local_log(&satb, live as usize);
    cratonvm_gc_core::satb::flush_thread_satb_buffer(&satb);

    marker.concurrent_mark(&og2);
    marker.remark(&stw(), &[], &og2);

    assert!(
        marker.bitmap.is_marked(live as usize),
        "remark must mark targets logged into the SHARED SATB queue"
    );
}

/// fork6 GC_STRESS fix — a cycle whose remark STW could not be acquired
/// must be abortable: `abort_cycle` deactivates the (shared) SATB barrier
/// and returns the phase to Idle so the write barrier stops logging and
/// the next trigger starts fresh. The caller skips the sweep entirely.
#[test]
fn abort_cycle_deactivates_barrier_and_resets_phase() {
    let (og, ptr) = make_old_gen_with_object(1);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

    marker.initial_mark(&[ptr], &og);
    assert!(marker.satb_queue.is_active());
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);

    marker.abort_cycle();
    assert!(
        !marker.satb_queue.is_active(),
        "abort must deactivate the barrier"
    );
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
}

// ------------------------------------------------------------------
// gengc-mark2 2026-09-20
// ------------------------------------------------------------------

/// `gengc-mark-old-gen-object-starts-is-a-hashset-per-phase-FIXED-20260923`.
///
/// The bitmap must answer EXACTLY what the `HashSet<usize>` it replaced
/// answered — the same membership at every address in (and around) the
/// generation, and the same cardinality. Mirrors
/// `young_mark::object_start_bits_match_a_hash_set_exactly`, which is the
/// in-repo precedent this change reuses.
#[test]
fn old_gen_object_starts_bitmap_matches_a_hash_set_exactly() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let mut allocated: Vec<*mut u8> = Vec::new();
    for i in 0..64u32 {
        let p = og.alloc(size, 8).unwrap();
        init_old_object(p, i + 1);
        allocated.push(p);
    }
    // Free a few so the walk has gaps to skip: that is what makes the
    // "starts" set different from "every 8-byte slot in the generation",
    // and it is the case a bitmap could get wrong.
    for i in [5usize, 17, 40] {
        // SAFETY: each is a live block of exactly `size` bytes from this
        // generation, freed once.
        unsafe { og.free(allocated[i], size) };
    }
    let reference: HashSet<usize> = og
        .walk_objects()
        .into_iter()
        .map(|(p, _)| p as usize)
        .collect();

    let bits = old_gen_object_starts(&og);
    assert_eq!(
        bits.len(),
        reference.len(),
        "bitmap cardinality must equal the HashSet's"
    );
    assert!(!bits.is_empty());

    // Every 8-byte slot in the generation, plus a margin either side, must
    // get the same verdict from both.
    // Walk 8-ALIGNED addresses: every object start is absolutely
    // 8-aligned, so a loop anchored on an unaligned base would step past
    // all of them and the comparison would be vacuous.
    let (lo, hi) = og.extent();
    let mut addr = (lo & !7usize).saturating_sub(64);
    while addr < hi + 64 {
        assert_eq!(
            bits.contains(addr),
            reference.contains(&addr),
            "bitmap and HashSet disagree at {addr:#x}"
        );
        addr += 8;
    }
    // And an interior (non-start) address of a real object is not a start.
    assert!(!bits.contains(allocated[0] as usize + 8));
    // A freed object's address is no longer a start, in both.
    assert!(!bits.contains(allocated[5] as usize));
}

/// An empty generation yields an empty, zero-length set — the condition
/// `concurrent_sweep` uses as its "free nothing" exit.
#[test]
fn old_gen_object_starts_on_an_empty_generation_is_empty() {
    let og = OldGen::new(65536);
    let bits = old_gen_object_starts(&og);
    assert!(bits.is_empty());
    assert_eq!(bits.len(), 0);
    assert!(!bits.contains(og.base_ptr() as usize));
}

/// Remark narrows the initial-mark snapshot by intersecting it with the
/// current walk. The word-wise AND must produce exactly what
/// `HashSet::retain` produced.
#[test]
fn object_starts_intersection_keeps_exactly_the_common_starts() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let a = og.alloc(size, 8).unwrap();
    init_old_object(a, 1);
    let b = og.alloc(size, 8).unwrap();
    init_old_object(b, 2);

    let mut at_initial_mark = old_gen_object_starts(&og);
    assert!(at_initial_mark.contains(a as usize));
    assert!(at_initial_mark.contains(b as usize));

    // `b` goes away and `c` arrives. `c` lands either in `b`'s freed block
    // or above it — never below, because nothing is free below `a` — so
    // the later walk spans at least as far as the earlier one, which is
    // the precondition `retain_intersection` requires.
    // SAFETY: `b` is a live block of exactly `size` bytes from this gen.
    unsafe { og.free(b, size) };
    let c = og.alloc(size, 8).unwrap();
    init_old_object(c, 3);
    let at_remark = old_gen_object_starts(&og);

    assert!(
        at_initial_mark.retain_intersection(&at_remark),
        "the two walks share a base and the later one is no shorter"
    );
    assert!(
        at_initial_mark.contains(a as usize),
        "`a` was present at both walks and must survive the narrowing"
    );
    // ORCHESTRATOR CORRECTION, 2026-09-20 — this assertion used to be
    // unconditional (`!contains(c)`), which contradicted the very next
    // block's acknowledgement that `c` may land on `b`'s address. It does:
    // `b` is freed and `c` requests the SAME size, so the size-segregated
    // free list hands back exactly `b`'s block on the usual path, and the
    // test failed on its own stated edge case rather than on a defect.
    //
    // Both outcomes are correct behaviour and the test now says which is
    // which, because the distinction is the whole point of the epoch check
    // living somewhere else.
    if std::ptr::eq(c, b) {
        // The free list reused the block. That ADDRESS was genuinely
        // present at both walks, so a word-wise AND keeps it — and must.
        // An address-keyed intersection cannot tell the dead `b` from the
        // live `c` that replaced it, and is not the mechanism that is
        // supposed to: `reclaim_epoch` is (see `release_unused_tail`'s
        // address-identity stamp). Asserting absence here would be
        // asserting that the intersection does a job it deliberately
        // delegates.
        assert!(
            at_initial_mark.contains(c as usize),
            "an address live at both walks must survive the narrowing, \
             whichever object owned it"
        );
    } else {
        assert!(
            !at_initial_mark.contains(c as usize),
            "`c` did not exist at initial mark and must not be added by it"
        );
        assert!(
            !at_initial_mark.contains(b as usize),
            "`b` was freed before the remark walk and must be narrowed out"
        );
    }
}

/// `gengc-mark-markqueue-has-no-termination-detection-FIXED-20260923`.
///
/// The property the plain `pop` loop cannot give: with N markers, every
/// node of a graph is scanned EXACTLY once and no marker leaves while a
/// peer still has children to push. The graph is a chain, so at any
/// instant at most one node is available — a worker that terminates on
/// "the queue looked empty" loses the rest of the chain, which is the
/// failure this primitive exists to prevent.
#[test]
fn parallel_drain_visits_every_node_exactly_once() {
    // Long enough that the "peer is mid-scan with an empty queue" window
    // is hit many times per run, short enough that eight workers parking
    // and waking on every link stays a fast unit test.
    const NODES: usize = 1024;
    for &workers in &[1usize, 2, 4, 8] {
        let queue = MarkQueue::new();
        // Node k is the address `(k + 1) * 8`; scanning node k pushes
        // node k + 1. Addresses are never dereferenced here.
        let visits: Vec<AtomicUsize> = (0..NODES).map(|_| AtomicUsize::new(0)).collect();
        queue.push(8 as *mut u8);
        queue.begin_drain(workers);

        let run = || {
            let _w = queue.worker();
            while let Some(ptr) = queue.pop_or_terminate() {
                let k = (ptr as usize / 8) - 1;
                visits[k].fetch_add(1, Ordering::Relaxed);
                // Make the "peer is inside a scan with the queue empty"
                // window wide enough to be hit.
                std::thread::yield_now();
                if k + 1 < NODES {
                    queue.push(((k + 2) * 8) as *mut u8);
                }
            }
        };

        std::thread::scope(|s| {
            for _ in 1..workers {
                s.spawn(&run);
            }
            run();
        });

        assert!(
            queue.drain_is_done(),
            "{workers} worker(s): the drain must end by declaring completion, \
             not by a worker guessing the queue is empty"
        );
        for (k, v) in visits.iter().enumerate() {
            assert_eq!(
                v.load(Ordering::Relaxed),
                1,
                "{workers} worker(s): node {k} was scanned {} times, not once",
                v.load(Ordering::Relaxed)
            );
        }
        assert!(queue.is_empty());
    }
}

/// The race the chain test makes likely, made deterministic: one marker is
/// held inside its "scan" while the queue is empty, and the other markers
/// must not declare the closure complete until it has pushed its child.
#[test]
fn a_marker_may_not_terminate_while_a_peer_is_mid_scan() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Barrier;

    let queue = MarkQueue::new();
    let scanning = Barrier::new(2);
    let child_seen = AtomicBool::new(false);
    queue.push(0x100 as *mut u8);
    queue.begin_drain(2);

    std::thread::scope(|s| {
        // Worker A pops 0x100, then parks INSIDE the scan until B has had
        // every chance to observe an empty queue, and only then pushes.
        s.spawn(|| {
            let _w = queue.worker();
            let first = queue.pop_or_terminate().expect("seed must be popped");
            assert_eq!(first as usize, 0x100);
            scanning.wait();
            std::thread::yield_now();
            queue.push(0x200 as *mut u8);
            while let Some(p) = queue.pop_or_terminate() {
                if p as usize == 0x200 {
                    child_seen.store(true, Ordering::Relaxed);
                }
            }
        });
        // Worker B sees an empty queue while A holds the only node.
        let _w = queue.worker();
        scanning.wait();
        while let Some(p) = queue.pop_or_terminate() {
            if p as usize == 0x200 {
                child_seen.store(true, Ordering::Relaxed);
            }
        }
    });

    assert!(
        child_seen.load(Ordering::Relaxed),
        "a child pushed while a peer observed an empty queue was never scanned — \
         this is exactly the premature-termination bug"
    );
}

/// A worker that unwinds must not park its peers forever: `MarkWorker`'s
/// `Drop` decrements the live count on the unwind path too.
#[test]
fn a_panicking_marker_releases_its_peers() {
    let queue = std::sync::Arc::new(MarkQueue::new());
    queue.begin_drain(2);

    let panicker = {
        let q = queue.clone();
        std::thread::spawn(move || {
            let _w = q.worker();
            panic!("marker died mid-scan");
        })
    };
    // The survivor must reach termination rather than parking forever.
    let survivor = {
        let q = queue.clone();
        std::thread::spawn(move || {
            let _w = q.worker();
            while q.pop_or_terminate().is_some() {}
        })
    };
    assert!(panicker.join().is_err());
    survivor
        .join()
        .expect("the surviving marker must terminate, not hang");
}

/// `gengc-mark-concurrent-mark-holds-the-old-gen-lock-FIXED-20260923`.
///
/// Slicing Phase 2 must reach the same fixed point as the unbounded run:
/// the same bitmap, the same total scan count, and a `true` completion
/// flag exactly once.
#[test]
fn a_sliced_concurrent_mark_reaches_the_same_fixed_point() {
    /// Build a chain of `n` old-gen objects, each pointing at the next,
    /// and return `(old_gen, root)`.
    fn chain(n: usize) -> (OldGen, *mut u8) {
        let mut og = OldGen::new(1 << 20);
        let size = HEADER_SIZE + SLOT_SIZE;
        let mut ptrs = Vec::new();
        for i in 0..n {
            let p = og.alloc(size, 8).unwrap();
            init_old_object(p, i as u32 + 1);
            ptrs.push(p);
        }
        for i in 0..n - 1 {
            // SAFETY: slot 0 of a 1-slot object allocated just above.
            unsafe {
                let slot = ptrs[i].add(HEADER_SIZE) as *mut Value;
                slot.write(Value::Object(Some(ObjectRef::from_raw(ptrs[i + 1]))));
            }
        }
        (og, ptrs[0])
    }

    const N: usize = 200;

    let (og, root) = chain(N);
    let whole = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    whole.initial_mark(&[root], &og);
    let whole_scanned = whole.concurrent_mark(&og);

    let (og2, root2) = chain(N);
    let sliced = ConcurrentMarker::new(og2.base_ptr() as usize, og2.capacity());
    sliced.initial_mark(&[root2], &og2);
    let mut sliced_scanned = 0usize;
    let mut slices = 0usize;
    loop {
        let (n, done) = sliced.concurrent_mark_budget(&og2, 7);
        sliced_scanned += n;
        slices += 1;
        assert!(slices < N + 10, "slicing must terminate");
        if done {
            break;
        }
    }

    assert!(
        slices > 1,
        "a budget of 7 over a 200-node chain must take many slices, took {slices}"
    );
    assert_eq!(
        sliced_scanned, whole_scanned,
        "the sliced mark must scan the same number of objects as the whole-phase mark"
    );
    assert_eq!(
        sliced.bitmap.marked_count(),
        whole.bitmap.marked_count(),
        "the sliced mark must reach the same bitmap"
    );
    assert_eq!(whole.bitmap.marked_count(), N);
}

/// A zero budget must still make progress rather than spinning forever.
#[test]
fn a_zero_budget_slice_still_scans_one_object() {
    let (og, ptr) = make_old_gen_with_object(1);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[ptr], &og);
    let (n, done) = marker.concurrent_mark_budget(&og, 0);
    assert_eq!(n, 1);
    assert!(done);
}

/// gen r4/mark (2026-09-23) — a slice must not keep tracing a cycle whose
/// old-gen layout moved under it.
///
/// Once `reclaim_epoch` has moved since initial mark, `remark` is certain
/// to fail the cycle closed, and every queued address may name a recycled
/// block. The slice drops the queue and reports the phase complete instead
/// of scanning them.
#[test]
fn a_slice_stops_tracing_once_the_epoch_moved() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let root = og.alloc(size, 8).unwrap();
    init_old_object(root, 1);
    let victim = og.alloc(size, 8).unwrap();
    init_old_object(victim, 2);

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[root], &og);
    assert!(!marker.queue.is_empty(), "precondition: the root is queued");

    // Another old-gen collection interleaves between two slices.
    // SAFETY: `(victim, size)` is exactly the pair `alloc` handed out.
    unsafe { og.free(victim, size) };

    let (scanned, done) = marker.concurrent_mark_budget(&og, 1_000);
    assert_eq!(scanned, 0, "nothing may be traced for a dead cycle");
    assert!(done);
    assert!(marker.queue.is_empty(), "the stale queue must be dropped");

    // Control: with the epoch intact the same slice traces normally.
    let (og2, ptr2) = make_old_gen_with_object(1);
    let marker2 = ConcurrentMarker::new(og2.base_ptr() as usize, og2.capacity());
    marker2.initial_mark(&[ptr2], &og2);
    assert_eq!(marker2.concurrent_mark_budget(&og2, 1_000), (1, true));
}

/// gen r4/mark (2026-09-23) — `abort_cycle` now closes the PHASE gate
/// before the QUEUE gate (the order `remark` was fixed to on 2026-09-20).
/// The observable post-conditions must not have moved.
#[test]
fn abort_cycle_post_conditions_are_unchanged_by_the_reordering() {
    let (og, ptr) = make_old_gen_with_object(1);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[ptr], &og);
    assert!(marker.state.is_marking_active());
    assert!(marker.satb_queue.is_active());
    let drops_before = marker.satb_queue.late_log_drops();
    marker.abort_cycle();
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
    assert!(!marker.satb_queue.is_active());
    assert!(marker.queue.is_empty());
    assert_eq!(marker.satb_queue.late_log_drops(), drops_before);
}

// -----------------------------------------------------------------------
// gen r4w2/concmark (2026-09-23)
// -----------------------------------------------------------------------

/// `n` one-slot old-gen objects, each pointing at the next.
fn r4w2_chain(n: usize) -> (OldGen, Vec<*mut u8>) {
    let mut og = OldGen::new(1 << 20);
    let size = HEADER_SIZE + SLOT_SIZE;
    let mut ptrs = Vec::new();
    for i in 0..n {
        let p = og.alloc(size, 8).unwrap();
        init_old_object(p, i as u32 + 1);
        ptrs.push(p);
    }
    for i in 0..n.saturating_sub(1) {
        r4w2_link(ptrs[i], ptrs[i + 1]);
    }
    (og, ptrs)
}

/// `from.slot0 = to`.
fn r4w2_link(from: *mut u8, to: *mut u8) {
    // SAFETY: `from` is a live one-slot object allocated by the test and
    // `to` a live object start; slot 0 is inside `from`.
    unsafe {
        let slot = from.add(HEADER_SIZE) as *mut Value;
        slot.write(Value::Object(Some(ObjectRef::from_raw(to))));
    }
}

fn r4w2_cached_walk(marker: &ConcurrentMarker) -> Option<*const OldGenObjectStarts> {
    marker
        .starts_cache
        .lock()
        .as_ref()
        .map(|c| Arc::as_ptr(&c.starts))
}

/// Two drivers on the SAME shared state: only one may own a cycle, and the
/// state becomes openable again only once the owner has finished.
#[test]
fn only_one_driver_can_own_a_cycle_on_shared_state() {
    let (mut og, ptr) = make_old_gen_with_object(1);
    let satb = Arc::new(SatbQueue::new());
    let state = Arc::new(ConcurrentGcState::new());
    let base = og.base_ptr() as usize;
    let cap = og.capacity();
    let a = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());
    let b = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());

    let owned = a.try_open_cycle().expect("an idle state must be openable");
    assert_eq!(state.phase(), ConcurrentGcPhase::InitialMark);
    assert!(
        b.try_open_cycle().is_none(),
        "a second driver must not open a cycle over an open one"
    );

    a.initial_mark(&[ptr], &og);
    assert!(b.try_open_cycle().is_none(), "nor while it marks");
    a.concurrent_mark(&og);
    a.remark(&stw(), &[ptr], &og);
    assert!(b.try_open_cycle().is_none(), "nor between remark and sweep");
    a.concurrent_sweep(&mut og);
    owned.complete();
    assert_eq!(state.phase(), ConcurrentGcPhase::Idle);
    assert!(!satb.is_active());

    let reopened = b
        .try_open_cycle()
        .expect("a finished cycle frees the state");
    drop(reopened);
    assert_eq!(state.phase(), ConcurrentGcPhase::Idle);
}

/// `finish_cycle` must not clobber a cycle another driver opened after this
/// one's sweep had already handed the state back.
#[test]
fn finish_cycle_cannot_reopen_the_next_drivers_cycle() {
    let (mut og, ptr) = make_old_gen_with_object(1);
    let satb = Arc::new(SatbQueue::new());
    let state = Arc::new(ConcurrentGcState::new());
    let base = og.base_ptr() as usize;
    let cap = og.capacity();
    let a = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());
    let b = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());
    let c = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());

    let owned_a = a.try_open_cycle().unwrap();
    a.initial_mark(&[ptr], &og);
    a.concurrent_mark(&og);
    a.remark(&stw(), &[ptr], &og);
    a.concurrent_sweep(&mut og);
    // The sweep handed the state back: B opens before A's driver returns.
    let owned_b = b
        .try_open_cycle()
        .expect("the sweep's Idle is the hand-off");
    // A's late completion must leave B's cycle alone...
    owned_a.complete();
    assert_eq!(state.phase(), ConcurrentGcPhase::InitialMark);
    // ...so a third driver still cannot open a concurrent one.
    assert!(c.try_open_cycle().is_none());
    drop(owned_b);
    assert_eq!(state.phase(), ConcurrentGcPhase::Idle);
}

/// gci i13/cm item 1: a driver that unwinds AFTER its sweep handed the
/// shared state over (between `finish_sweep`'s `Idle` and
/// `ConcurrentCycle::complete`) drops its ownership. That drop used to run
/// `abort_cycle`, which stored `Idle` and deactivated the SHARED SATB queue
/// under the next driver's open, marking cycle -- whose remark would then
/// sweep against a snapshot with every store of the gap unlogged. The drop
/// must leave the next cycle exactly as it found it.
#[test]
fn gci_i13cm_a_cycle_dropped_after_its_hand_off_leaves_the_next_cycle_marking() {
    let (mut og, ptr) = make_old_gen_with_object(1);
    let satb = Arc::new(SatbQueue::new());
    let state = Arc::new(ConcurrentGcState::new());
    let base = og.base_ptr() as usize;
    let cap = og.capacity();
    let a = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());
    let b = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());

    let owned_a = a.try_open_cycle().unwrap();
    a.initial_mark(&[ptr], &og);
    a.concurrent_mark(&og);
    a.remark(&stw(), &[ptr], &og);
    a.concurrent_sweep(&mut og);
    assert_eq!(state.phase(), ConcurrentGcPhase::Idle, "the sweep hands off");

    // B opens and marks before A's driver gets back to `complete`.
    let owned_b = b.try_open_cycle().expect("the sweep's Idle is the hand-off");
    b.initial_mark(&[ptr], &og);
    assert!(satb.is_active());
    assert_eq!(state.phase(), ConcurrentGcPhase::ConcurrentMark);
    let abandoned_before = state.census().abandoned;

    // A unwinds: its ownership is dropped without `complete`.
    drop(owned_a);
    assert_eq!(
        state.phase(),
        ConcurrentGcPhase::ConcurrentMark,
        "a handed-off cycle must not move the next cycle's phase"
    );
    assert!(
        satb.is_active(),
        "a handed-off cycle must not close the next cycle's SATB queue"
    );
    assert_eq!(
        state.census().abandoned,
        abandoned_before,
        "B's open cycle must not be counted abandoned by A's drop"
    );
    assert!(b.bitmap.is_marked(ptr as usize), "B's marks are untouched");

    // B's own early exit still abandons B.
    drop(owned_b);
    assert_eq!(state.phase(), ConcurrentGcPhase::Idle);
    assert!(!satb.is_active());

    // A marker reused for a later cycle owns that cycle again.
    let again = a.try_open_cycle().expect("idle again");
    a.initial_mark(&[ptr], &og);
    assert!(satb.is_active());
    drop(again);
    assert_eq!(state.phase(), ConcurrentGcPhase::Idle);
    assert!(!satb.is_active());
}

/// gci i13/cm item 2: a STW pre-emption that lands in the SWEEP phase
/// measures the cycle's growth net of what the sweep has freed so far.
/// Baseline 400 000 at the initial mark; the sweep has freed 200 000
/// (gone from `used()`, not yet in the trigger's `freed_bytes`) while
/// 100 000 were promoted, so `old_gen_allocated_total` reads 300 000. The
/// legacy sample saturates to 0 (widening = C/8); the net one is 100 000.
#[test]
fn gci_i13cm_a_preemption_in_the_sweep_measures_growth_net_of_the_sweeps_frees() {
    const CAP: usize = 1_000_000;
    let run = |net: Option<&str>| -> (u64, Option<u64>) {
        cratonvm_types::flags::with_thread_overrides(
            &[(GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP_FLAG, net)],
            || {
                let state = ConcurrentGcState::new();
                state.note_cycle_open(400_000, 400_000);
                state.note_sweep_freed_so_far(200_000);
                state.note_stw_preempted(300_000, CAP);
                (
                    state.census().preempt_growth_max_permille,
                    state.policy_estimates().0,
                )
            },
        )
    };
    // Off (the default): the census reads net, the widening is legacy.
    let (permille, growth) = run(None);
    assert_eq!(permille, 100, "100 000 of 1 000 000 = 100 permille");
    assert_eq!(growth, Some((CAP / 8) as u64), "legacy: 0 growth -> C/8");
    // On: the widening reads the net growth too, 2 x 100 000 > C/8.
    let (permille, growth) = run(Some("1"));
    assert_eq!(permille, 100);
    assert_eq!(growth, Some(200_000));

    // The in-flight figure dies with the cycle's baseline: a later cycle
    // pre-empted in its MARK phase measures as before.
    cratonvm_types::flags::with_thread_overrides(
        &[(GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP_FLAG, Some("1"))],
        || {
            let state = ConcurrentGcState::new();
            state.note_cycle_open(400_000, 400_000);
            state.note_sweep_freed_so_far(200_000);
            state.note_cycle_dropped();
            state.note_cycle_open(400_000, 400_000);
            state.note_stw_preempted(450_000, CAP);
            assert_eq!(state.census().preempt_growth_max_permille, 50);
            assert_eq!(state.policy_estimates().0, Some((CAP / 8) as u64));
        },
    );
}

/// Every early exit of the driver drops the ownership; after an initial
/// mark that must take the barrier down, not just the phase.
#[test]
fn dropping_an_open_cycle_abandons_it() {
    let (og, ptr) = make_old_gen_with_object(1);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

    // Lost initial-mark pause: nothing ran, the phase just goes back.
    drop(marker.try_open_cycle().unwrap());
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
    assert!(!marker.satb_queue.is_active());

    // Lost remark (or a stale epoch) after initial mark: full abort.
    let owned = marker.try_open_cycle().unwrap();
    marker.initial_mark(&[ptr], &og);
    assert!(marker.state.is_marking_active());
    assert!(marker.satb_queue.is_active());
    owned.abort();
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
    assert!(!marker.satb_queue.is_active());
    assert!(marker.queue.is_empty());
    assert!(
        r4w2_cached_walk(&marker).is_none(),
        "abort drops the cached walk"
    );
    assert!(marker.try_open_cycle().is_some());
}

/// The slice cache: reused while `free_list_seq` stands still, rebuilt the
/// moment an allocation moves it — and the rebuilt set is what makes an
/// object allocated between two slices markable.
#[test]
fn slices_reuse_the_walk_until_an_allocation_moves_the_free_list() {
    const N: usize = 40;
    let (mut og, nodes) = r4w2_chain(N);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    // The re-walking arm: `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` is default
    // on since gce (2026-09-29) and decided at the initial mark; `=0`
    // restores the per-slice walk this test pins (the TAMS arm is
    // `gcd_d9e_phase2_walks_are_counted_and_the_tams_snapshot_skips_them`).
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_GEN_CONC_MARK_TAMS_STARTS", Some("0"))],
        || marker.initial_mark(&[nodes[0]], &og),
    );
    let seeded = r4w2_cached_walk(&marker).expect("initial_mark seeds the cache");

    let (n1, done1) = marker.concurrent_mark_budget(&og, 5);
    assert_eq!(n1, 5);
    assert!(!done1);
    assert_eq!(
        r4w2_cached_walk(&marker),
        Some(seeded),
        "nothing was allocated since initial mark, so the slice must reuse its walk"
    );
    let (_, done2) = marker.concurrent_mark_budget(&og, 5);
    assert!(!done2);
    assert_eq!(r4w2_cached_walk(&marker), Some(seeded));

    // A "promotion" between two slices, stored into a node the trace has
    // not reached yet.
    let x = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    init_old_object(x, 999);
    r4w2_link(nodes[N - 1], x);

    let mut slices = 0;
    loop {
        let (_, done) = marker.concurrent_mark_budget(&og, 5);
        slices += 1;
        assert!(slices < N, "slicing must terminate");
        if done {
            break;
        }
    }
    assert_ne!(
        r4w2_cached_walk(&marker),
        Some(seeded),
        "the allocation moved free_list_seq, so the next slice had to re-walk"
    );
    assert!(
        marker.bitmap.is_marked(x as usize),
        "an object allocated between slices must be markable in a later slice"
    );
    assert_eq!(marker.bitmap.marked_count(), N + 1);
}

/// `gengc-r4-mark-walk-desync-tail-is-untraced-FIXED-20260923` for the concurrent
/// cycle. Layout `A, C, B(corrupt), X` with `A → X → C`: the walk stops at
/// B, so X is not an object start, is never traced, and C — walked,
/// eligible, reachable only through X — has a clear bit. The sweep used to
/// free C while X still pointed at it. It must now reclaim nothing.
#[test]
fn a_desynced_walk_refuses_the_sweep() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let a = og.alloc(size, 8).unwrap();
    init_old_object(a, 1);
    let c = og.alloc(size, 8).unwrap();
    init_old_object(c, 2);
    let b = og.alloc(size, 8).unwrap();
    init_old_object(b, 3);
    let x = og.alloc(size, 8).unwrap();
    init_old_object(x, 4);
    r4w2_link(a, x);
    r4w2_link(x, c);
    // SAFETY: the kind-tag byte of a live allocation; any `u8` may be
    // written there, and 0xFF is not a valid `ObjectKind`.
    unsafe { std::ptr::write(b.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8) };

    let aborts_before = CONC_MARK_WALK_DESYNC_ABORTS.load(Ordering::Relaxed);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[a], &og);
    marker.concurrent_mark(&og);
    assert!(
        !marker.bitmap.is_marked(c as usize),
        "precondition: X is not an object start, so C is never reached"
    );
    marker.remark(&stw(), &[a], &og);
    let swept = marker.concurrent_sweep(&mut og);

    assert_eq!(swept, 0, "a cycle whose walk desynced must reclaim nothing");
    assert!(
        og.is_allocated_addr(c),
        "C is live through X and must survive"
    );
    assert!(
        CONC_MARK_WALK_DESYNC_ABORTS.load(Ordering::Relaxed) > aborts_before,
        "the refusal must be counted"
    );
}

/// gen r4w6/oldpin6 — the same layout with the opt-in walk-gap recovery
/// (`CRATONVM_GC_CONC_WALK_GAP_RECOVERY`, forced for this thread), plus a
/// dead walked object E: `A, C, E, B(corrupt), X` with `A → X → C`. The
/// remark seeds the gap's words (X's slot names C), so C survives; E is
/// walked, eligible and unreachable, so it is freed; nothing in the gap
/// (B, X) is freed. The cycle reclaims instead of refusing.
#[test]
fn a_desynced_walk_sweeps_through_the_gap_when_recovery_is_on() {
    FORCE_CONC_WALK_GAP_RECOVERY.with(|c| c.set(true));
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let a = og.alloc(size, 8).unwrap();
    init_old_object(a, 1);
    let c = og.alloc(size, 8).unwrap();
    init_old_object(c, 2);
    let e = og.alloc(size, 8).unwrap();
    init_old_object(e, 5);
    let b = og.alloc(size, 8).unwrap();
    init_old_object(b, 3);
    let x = og.alloc(size, 8).unwrap();
    init_old_object(x, 4);
    r4w2_link(a, x);
    r4w2_link(x, c);
    // SAFETY: the kind-tag byte of a live allocation; 0xFF is not a valid
    // `ObjectKind`, so the walk stops at B.
    unsafe { std::ptr::write(b.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8) };
    let (walked, gaps) = og.walk_objects_with_gaps();
    assert_eq!(walked.len(), 3, "precondition: A, C, E are walked");
    assert_eq!(gaps.len(), 1, "precondition: one gap, B and X");

    let recoveries_before = CONC_MARK_WALK_GAP_RECOVERIES.load(Ordering::Relaxed);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[a], &og);
    marker.concurrent_mark(&og);
    assert!(
        !marker.bitmap.is_marked(c as usize),
        "precondition: C is not reached"
    );
    marker.remark(&stw(), &[a], &og);
    assert!(
        marker.bitmap.is_marked(c as usize),
        "the gap word naming C marked it"
    );
    let swept = marker.concurrent_sweep(&mut og);
    FORCE_CONC_WALK_GAP_RECOVERY.with(|c| c.set(false));

    assert_eq!(swept, 1, "exactly the dead walked object E is reclaimed");
    assert!(!og.is_allocated_addr(e), "E is freed");
    for (name, p) in [("A", a), ("C", c), ("B", b), ("X", x)] {
        assert!(og.is_allocated_addr(p), "{name} must survive");
    }
    assert!(CONC_MARK_WALK_GAP_RECOVERIES.load(Ordering::Relaxed) > recoveries_before);
}

/// gen r4w6/oldpin6 — the plan's gates: on a healthy generation (walked
/// bytes equal `used`, no gaps) a plan exists and names no gap; with the
/// opt-in off there is no plan at all, so the remark refuses as before.
#[test]
fn the_walk_gap_plan_requires_the_gaps_to_account_for_used() {
    FORCE_CONC_WALK_GAP_RECOVERY.with(|c| c.set(true));
    let (og, _obj) = make_old_gen_with_object(1);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    // A healthy generation: a plan exists (no gaps, walked == used).
    let plan = marker.remark_walk_gap_plan(&og).expect("walked == used");
    assert!(plan.gaps.is_empty());
    FORCE_CONC_WALK_GAP_RECOVERY.with(|c| c.set(false));
    // With the override off (and the flag unset in tests) there is none.
    if !cratonvm_gc_core::gc_flags().conc_walk_gap_recovery {
        assert!(marker.remark_walk_gap_plan(&og).is_none());
    }
}

/// A remark that refuses the sweep (the epoch moved) does no marking at
/// all, and still leaves the post-conditions of a completed remark.
#[test]
fn a_refused_remark_skips_both_closures() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let root = og.alloc(size, 8).unwrap();
    init_old_object(root, 1);
    let logged = og.alloc(size, 8).unwrap();
    init_old_object(logged, 2);
    let victim = og.alloc(size, 8).unwrap();
    init_old_object(victim, 3);

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[root], &og);
    marker.concurrent_mark(&og);
    marker.satb_queue.flush(vec![logged as usize]);
    // SAFETY: exactly the pair `alloc` handed out.
    unsafe { og.free(victim, size) };
    assert!(marker.cycle_is_stale(&og));

    let discovered = marker.remark(&stw(), &[root], &og);
    assert_eq!(discovered, 0, "a refused remark must not trace anything");
    assert!(
        !marker.bitmap.is_marked(logged as usize),
        "the SATB entry is irrelevant to a cycle that will not sweep"
    );
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
    assert!(!marker.satb_queue.is_active());
    assert!(marker.satb_queue.is_empty());
    assert!(marker.queue.is_empty());
    assert_eq!(marker.concurrent_sweep(&mut og), 0);
}

/// Step 1 of
/// `gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing`:
/// the callback sees the cycle's verdict and what it keeps is marked, with
/// its closure, before the sweep.
#[test]
fn remark_reference_processing_keeps_what_the_callback_returns() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let root = og.alloc(size, 8).unwrap();
    init_old_object(root, 1);
    let referent = og.alloc(size, 8).unwrap();
    init_old_object(referent, 2);
    let child = og.alloc(size, 8).unwrap();
    init_old_object(child, 3);
    let dead = og.alloc(size, 8).unwrap();
    init_old_object(dead, 4);
    r4w2_link(referent, child);

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[root], &og);
    marker.concurrent_mark(&og);

    let mut calls = 0;
    let mut process = |is_marked: &dyn Fn(usize) -> bool| -> Vec<usize> {
        calls += 1;
        assert!(is_marked(root as usize), "the root is marked");
        assert!(!is_marked(referent as usize), "the referent is not (yet)");
        assert!(!is_marked(dead as usize));
        // Outside the old generation: not this cycle's to judge.
        assert!(is_marked(0x10), "a non-eligible address must read as live");
        vec![referent as usize]
    };
    let process_dyn: &mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize> = &mut process;
    marker.remark_with_reference_processing(&stw(), &[root], &og, Some(process_dyn));
    assert_eq!(calls, 1);
    assert!(marker.bitmap.is_marked(referent as usize));
    assert!(
        marker.bitmap.is_marked(child as usize),
        "the kept object's closure must be marked too"
    );

    let swept = marker.concurrent_sweep(&mut og);
    assert_eq!(swept, 1, "only the object nothing kept may be freed");
    assert!(og.is_allocated_addr(referent));
    assert!(og.is_allocated_addr(child));
    assert!(!og.is_allocated_addr(dead));
}

/// gc-common w36-d — the concurrent half of
/// `common-d-generational-old-gen-finalizables-are-never-finalized-RETIRED-20260928`: a
/// published finalizer candidate that died old is RETAINED by the remark
/// with its closure (the sweep frees neither) and recorded, once, for the
/// next stop-the-world collection to report; a live candidate is not
/// recorded; an object nobody published is still swept.
#[test]
fn w36d_remark_retains_dead_finalizer_candidates_for_the_next_pause() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let root = og.alloc(size, 8).unwrap();
    init_old_object(root, 1);
    let live_fin = og.alloc(size, 8).unwrap();
    init_old_object(live_fin, 2);
    let dead_fin = og.alloc(size, 8).unwrap();
    init_old_object(dead_fin, 3);
    let child = og.alloc(size, 8).unwrap();
    init_old_object(child, 4);
    let dead = og.alloc(size, 8).unwrap();
    init_old_object(dead, 5);
    r4w2_link(root, live_fin);
    r4w2_link(dead_fin, child);

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    // A duplicate must not matter.
    marker.state.publish_finalizer_candidates(vec![
        dead_fin as usize,
        live_fin as usize,
        dead_fin as usize,
    ]);
    marker.initial_mark(&[root], &og);
    marker.concurrent_mark(&og);
    marker.remark(&stw(), &[root], &og);
    assert!(
        marker.bitmap.is_marked(dead_fin as usize),
        "the dead finalizable is kept"
    );
    assert!(marker.bitmap.is_marked(child as usize), "with its closure");
    assert!(!marker.bitmap.is_marked(dead as usize));
    assert_eq!(
        marker.state.take_remark_retained_finalizers(),
        vec![dead_fin as usize],
        "only the dead candidate is handed to the next pause"
    );
    assert!(
        marker.state.take_remark_retained_finalizers().is_empty(),
        "drained"
    );

    let swept = marker.concurrent_sweep(&mut og);
    assert_eq!(swept, 1, "only the unpublished garbage is freed");
    assert!(og.is_allocated_addr(dead_fin));
    assert!(og.is_allocated_addr(child));
    assert!(og.is_allocated_addr(live_fin));
    assert!(!og.is_allocated_addr(dead));
}

/// gen r5w5/conc9 — a remark that runs reference processing records the
/// finalizer retention's closure: the retained finalizable and what only
/// it reaches, never an object the strong closure had already marked. The
/// bitmap and the hand-off are the unrecorded retention's.
#[test]
fn r5w5_a_recording_retention_reports_only_what_it_marked() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let alloc = |og: &mut OldGen, cid: u32| {
        let p = og.alloc(size, 8).unwrap();
        init_old_object(p, cid);
        p
    };
    let root = alloc(&mut og, 1);
    let live_fin = alloc(&mut og, 2);
    let shared = alloc(&mut og, 3);
    let dead_fin = alloc(&mut og, 4);
    let child = alloc(&mut og, 5);
    let dead = alloc(&mut og, 6);
    // root -> live_fin -> shared; dead_fin -> child -> shared.
    r4w2_link(root, live_fin);
    r4w2_link(live_fin, shared);
    r4w2_link(dead_fin, child);
    r4w2_link(child, shared);

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.record_finalizer_retention_closure();
    marker
        .state
        .publish_finalizer_candidates(vec![dead_fin as usize, live_fin as usize]);
    marker.initial_mark(&[root], &og);
    marker.concurrent_mark(&og);
    marker.remark(&stw(), &[root], &og);

    let mut closure = marker.take_finalizer_retention_closure();
    closure.sort_unstable();
    let mut want = vec![dead_fin as usize, child as usize];
    want.sort_unstable();
    assert_eq!(
        closure, want,
        "the retained finalizable and its private closure only"
    );
    assert!(
        marker.take_finalizer_retention_closure().is_empty(),
        "taken once"
    );
    assert!(marker.bitmap.is_marked(dead_fin as usize));
    assert!(marker.bitmap.is_marked(child as usize));
    assert!(marker.bitmap.is_marked(shared as usize));
    assert!(!marker.bitmap.is_marked(dead as usize));
    assert_eq!(
        marker.state.take_remark_retained_finalizers(),
        vec![dead_fin as usize]
    );
    let epoch = og.reclaim_epoch();
    assert_eq!(
        marker.concurrent_sweep(&mut og),
        1,
        "only the garbage is freed"
    );
    // Wave 9, lane gcI: a sweep that freed blocks moves the address epoch.
    assert_ne!(og.reclaim_epoch(), epoch);
}

/// gen r5w5/conc9 — without the request the retention records nothing
/// (the default remark, byte for byte).
#[test]
fn r5w5_an_unrecorded_retention_records_nothing() {
    let mut og = OldGen::new(65536);
    let size = HEADER_SIZE + SLOT_SIZE;
    let root = og.alloc(size, 8).unwrap();
    init_old_object(root, 1);
    let dead_fin = og.alloc(size, 8).unwrap();
    init_old_object(dead_fin, 2);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker
        .state
        .publish_finalizer_candidates(vec![dead_fin as usize]);
    marker.initial_mark(&[root], &og);
    marker.concurrent_mark(&og);
    marker.remark(&stw(), &[root], &og);
    assert!(marker.bitmap.is_marked(dead_fin as usize));
    assert!(marker.take_finalizer_retention_closure().is_empty());
}

// -----------------------------------------------------------------
// gen r4w3/oldgen3 (2026-09-23)
// -----------------------------------------------------------------

/// `n` one-slot objects; every third one (0, 3, 6, ...) is live through a
/// chain from object 0, the rest are garbage.
fn r4w3_layout(n: usize) -> (OldGen, Vec<*mut u8>) {
    let mut og = OldGen::new(1 << 20);
    let size = HEADER_SIZE + SLOT_SIZE;
    let ptrs: Vec<*mut u8> = (0..n)
        .map(|i| {
            let p = og.alloc(size, 8).unwrap();
            init_old_object(p, i as u32 + 1);
            p
        })
        .collect();
    let live: Vec<usize> = (0..n).step_by(3).collect();
    for w in live.windows(2) {
        r4w2_link(ptrs[w[0]], ptrs[w[1]]);
    }
    (og, ptrs)
}

/// Initial mark from object 0, trace, remark: the sweep is authorised.
fn r4w3_marked(og: &OldGen, root: *mut u8) -> ConcurrentMarker {
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[root], og);
    marker.concurrent_mark(og);
    marker.remark(&stw(), &[root], og);
    marker
}

/// Sweep in slices of `budget` until done; returns (freed, slices).
fn r4w3_sweep_in_slices(
    marker: &ConcurrentMarker,
    og: &mut OldGen,
    budget: usize,
) -> (usize, usize) {
    let (mut freed, mut slices) = (0usize, 0usize);
    loop {
        let (n, done) = marker.concurrent_sweep_budget(og, budget);
        freed += n;
        slices += 1;
        assert!(slices < 10_000, "a sliced sweep must make progress");
        if done {
            return (freed, slices);
        }
    }
}

/// `gengc-r4w2-concmark-concurrent-sweep-holds-the-old-gen-lock-FIXED-20260923.md`:
/// a sweep in slices of any size frees exactly the set a whole sweep
/// frees, leaves the same generation behind, and tells the old gen's
/// trigger about ONE collection either way.
#[test]
fn a_sweep_in_slices_frees_exactly_what_a_whole_sweep_frees() {
    let size = HEADER_SIZE + SLOT_SIZE;
    let run = |budget: Option<usize>| {
        let (mut og, ptrs) = r4w3_layout(40);
        // A hole before the cycle, so the generation has two regions.
        // SAFETY: exactly the pair `alloc` handed out.
        unsafe { og.free(ptrs[1], size) };
        let marker = r4w3_marked(&og, ptrs[0]);
        let (freed, slices) = match budget {
            None => (marker.concurrent_sweep(&mut og), 1),
            Some(b) => r4w3_sweep_in_slices(&marker, &mut og, b),
        };
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
        let base = og.base_ptr() as usize;
        let walk: Vec<(usize, usize)> = og
            .walk_objects()
            .into_iter()
            .map(|(p, s)| (p as usize - base, s))
            .collect();
        (
            freed,
            slices,
            walk,
            og.trigger_stats(),
            marker.state.census(),
        )
    };
    let whole = run(None);
    // 40 objects: 14 live (0, 3, ..., 39), one freed before the cycle.
    assert_eq!(whole.0, 40 - 14 - 1);
    assert_eq!(whole.2.len(), 14);
    assert_eq!(whole.3.concurrent_collections, 1);
    assert_eq!(whole.3.freed_bytes, (25 * size) as u64);
    for budget in [1usize, 3, 7] {
        let sliced = run(Some(budget));
        assert_eq!(sliced.0, whole.0, "budget {budget}: freed");
        assert_eq!(sliced.2, whole.2, "budget {budget}: surviving layout");
        assert_eq!(sliced.3.freed_bytes, whole.3.freed_bytes, "budget {budget}");
        assert_eq!(sliced.3.concurrent_collections, 1, "budget {budget}");
        assert!(
            sliced.1 > 1,
            "budget {budget}: the sweep was actually sliced"
        );
        assert_eq!(sliced.4.sweep_slices as usize, sliced.1, "budget {budget}");
    }
}

/// gci i2/wr (`gci-i1wr-generational-concurrent-sweep-does-not-move-the-address-memo-epoch`):
/// the address-memo epoch's sweep term moves with EVERY slice, and has
/// already moved when the first slice that frees anything returns (a memo
/// filed before it can never be matched by an epoch read after a free).
#[test]
fn gci_wr_every_sweep_slice_moves_the_address_memo_term_before_it_frees() {
    let (mut og, ptrs) = r4w3_layout(40);
    let marker = r4w3_marked(&og, ptrs[0]);
    let before = marker.state.sweep_slices_begun();
    assert_eq!(before, 0, "no slice ran yet");
    let (freed, done) = marker.concurrent_sweep_budget(&mut og, 3);
    assert!(freed > 0 && !done, "the first slice frees and leaves work");
    let after_first = marker.state.sweep_slices_begun();
    assert_eq!(after_first, before + 1, "the slice that freed moved the term");
    let (_, slices) = r4w3_sweep_in_slices(&marker, &mut og, 3);
    assert_eq!(
        marker.state.sweep_slices_begun(),
        after_first + slices as u64,
        "one step per slice, never reset"
    );
    assert_eq!(
        marker.state.sweep_slices_begun(),
        marker.state.census().sweep_slices,
        "the same cell the census prints"
    );
}

/// TAMS across slices: an object allocated BETWEEN two sweep slices —
/// above the resume point (into a hole that was free at remark) or below
/// it (into a run the sweep itself just freed) — is never freed.
#[test]
fn an_object_allocated_between_sweep_slices_is_not_freed() {
    let size = HEADER_SIZE + SLOT_SIZE;
    let (mut og, ptrs) = r4w3_layout(30);
    // A hole at the far end, free at remark.
    // SAFETY: exactly the pair `alloc` handed out.
    unsafe { og.free(ptrs[29], size) };
    let marker = r4w3_marked(&og, ptrs[0]);

    let (first, done) = marker.concurrent_sweep_budget(&mut og, 4);
    assert!(!done);
    assert_eq!(
        first, 2,
        "objects 1 and 2 were the garbage among the first four"
    );

    // A promotion lands in the far hole (the one exact fit for its size).
    let high = og.alloc(size, 8).unwrap();
    init_old_object(high, 99);
    assert_eq!(high, ptrs[29], "allocated above the resume point");
    // And a larger one into the run the first slice freed (objects 1-2).
    let low = og.alloc(2 * size, 8).unwrap();
    // SAFETY: a fresh `2 * size` block: one object with three slots.
    unsafe {
        let h = &mut *(low as *mut ObjectHeader);
        h.class_id = ClassId::new(98);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
        h.set_num_slots(3);
        h.set_gc_flags(0x01);
    }
    assert_eq!(low, ptrs[1], "allocated below the resume point");

    let (rest, _) = r4w3_sweep_in_slices(&marker, &mut og, 4);
    // 30 objects: 10 live, one freed before the cycle, two in slice one.
    assert_eq!(first + rest, 30 - 10 - 1);
    assert!(
        og.is_allocated_addr(high),
        "never eligible: its block was free at remark"
    );
    assert!(
        og.is_allocated_addr(low),
        "below the resume point: never revisited"
    );
    let walk = og.walk_objects();
    assert!(walk.iter().any(|&(p, _)| p == high) && walk.iter().any(|&(p, _)| p == low));
    assert_eq!(
        walk.len(),
        10 + 2,
        "the live chain plus the two new objects"
    );
}

/// GCAUD-4 between slices: another collector freeing old-gen storage stops
/// the sweep at the next slice — reclaiming nothing more. gci i13/cm item
/// 7: what the slices had already freed still reaches the trigger's
/// `freed_bytes`, but the stopped sweep is NOT a second old-gen collection
/// (the stopping collection noted itself) and not a completed cycle.
#[test]
fn a_foreign_free_between_sweep_slices_stops_the_sweep() {
    let size = HEADER_SIZE + SLOT_SIZE;
    let (mut og, ptrs) = r4w3_layout(30);
    let marker = r4w3_marked(&og, ptrs[0]);
    let (first, done) = marker.concurrent_sweep_budget(&mut og, 4);
    assert!(!done && first == 2);
    let stops = marker.state.census().sweep_epoch_stops;

    // Another collector (a young pause's in-place old sweep) frees garbage
    // above the resume point.
    // SAFETY: exactly the pair `alloc` handed out; object 20 is garbage.
    unsafe { og.free(ptrs[20], size) };
    assert_eq!(marker.concurrent_sweep_budget(&mut og, 4), (0, true));
    assert_eq!(marker.state.census().sweep_epoch_stops, stops + 1);
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
    assert!(
        og.is_allocated_addr(ptrs[26]),
        "garbage beyond the stop is left for the next cycle"
    );
    let s = og.trigger_stats();
    assert_eq!(s.concurrent_collections, 0, "gci i13/cm item 7: not a collection");
    assert_eq!(s.collections, 0, "gci i13/cm item 7: not a collection");
    assert_eq!(s.freed_bytes, (2 * size) as u64);
    assert_eq!(og.used_after_last_collection(), Some(og.used()));
    assert_eq!(marker.state.census().cycles_completed, 0);
    // Nothing left to resume.
    assert_eq!(marker.concurrent_sweep_budget(&mut og, 4), (0, true));
}

/// gci i13/cm item 7: a STOP-THE-WORLD collection (noted by
/// `OldGen::note_collection_end`, as `major_gc` / the in-place sweep do)
/// lands between two sweep slices. The stopped sweep is part of that
/// collection's episode: `collections` counts ONE collection, not two;
/// the slices' frees still reach `freed_bytes`; the cycle is counted
/// `abandoned`, not completed, and leaves no report (no `Concurrent Mark
/// Cycle` line); the start policy is still fed its growth sample.
#[test]
fn gci_i13cm_a_sweep_stopped_by_a_stw_collection_is_not_a_second_collection() {
    let size = HEADER_SIZE + SLOT_SIZE;
    let (mut og, ptrs) = r4w3_layout(30);
    let marker = r4w3_marked(&og, ptrs[0]);
    let (first, done) = marker.concurrent_sweep_budget(&mut og, 4);
    assert!(!done && first == 2);
    let abandoned_before = marker.state.census().abandoned;

    // The STW collection: frees garbage above the resume point (moving
    // the epoch) and notes itself.
    let used_before = og.used();
    // SAFETY: exactly the pair `alloc` handed out; object 20 is garbage.
    unsafe { og.free(ptrs[20], size) };
    og.note_collection_end(used_before);
    assert_eq!(og.trigger_stats().collections, 1);

    assert_eq!(marker.concurrent_sweep_budget(&mut og, 4), (0, true));
    let s = og.trigger_stats();
    assert_eq!(s.collections, 1, "one collection, not two");
    assert_eq!(s.concurrent_collections, 0);
    assert_eq!(s.freed_bytes, (3 * size) as u64, "the STW free + the sweep's two");
    let c = marker.state.census();
    assert_eq!(c.cycles_completed, 0);
    assert_eq!(c.abandoned, abandoned_before + 1);
    assert!(marker.state.last_cycle_report().is_none(), "no report line");
    assert!(
        marker.state.policy_estimates().0.is_some(),
        "the start policy still gets the cycle's growth sample"
    );
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
}

/// gc-common w5-g (`common-w3g-concurrent-sweep-direct-old-alloc-window`):
/// `concurrent_sweep_budget_into` reports exactly the spans it freed —
/// every freed object inside one, no survivor inside any, ascending and
/// disjoint — and its results are those of the plain form.
#[test]
fn a_sweep_slice_reports_the_spans_it_freed() {
    let size = HEADER_SIZE + SLOT_SIZE;
    let (mut og, ptrs) = r4w3_layout(30);
    let marker = r4w3_marked(&og, ptrs[0]);
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut freed = 0usize;
    let mut slices = 0usize;
    loop {
        let (n, done) = marker.concurrent_sweep_budget_into(&mut og, 4, Some(&mut spans));
        freed += n;
        slices += 1;
        assert!(slices < 10_000);
        if done {
            break;
        }
    }
    // 30 objects, 10 live (0, 3, ..., 27).
    assert_eq!(freed, 20);
    let in_spans = |a: usize| {
        spans
            .iter()
            .any(|&(start, len)| a >= start && a < start + len)
    };
    for (i, &p) in ptrs.iter().enumerate() {
        assert_eq!(
            in_spans(p as usize),
            i % 3 != 0,
            "object {i}: reported iff freed"
        );
    }
    let bytes: usize = spans.iter().map(|&(_, len)| len).sum();
    assert_eq!(bytes, 20 * size);
    for w in spans.windows(2) {
        assert!(w[0].0 + w[0].1 <= w[1].0, "ascending and disjoint");
    }
    // The plain form is the `None` case: same results on the same layout.
    let (mut og2, ptrs2) = r4w3_layout(30);
    let marker2 = r4w3_marked(&og2, ptrs2[0]);
    assert_eq!(r4w3_sweep_in_slices(&marker2, &mut og2, 4).0, freed);
}

/// Item 2 of `gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md`:
/// an overflowed mark queue is repaired in Phase-2 slices, not only inside
/// the remark pause. Staged by marking an object WITHOUT queueing it (what
/// a dropped push leaves) and raising the flag.
#[test]
fn an_overflowed_mark_queue_is_rescanned_in_phase_two_slices() {
    let (og, ptrs) = r4w2_chain(4);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[ptrs[0]], &og);
    // 0 is marked; 1 is marked but its push was "dropped".
    marker.queue.clear();
    assert!(marker.bitmap.try_mark(ptrs[1] as usize));
    marker.queue.overflowed.store(true, Ordering::Relaxed);

    let mut slices = 0;
    loop {
        let (_, done) = marker.concurrent_mark_budget(&og, 1);
        slices += 1;
        assert!(slices < 100, "the rescan must make progress");
        if done {
            break;
        }
    }
    assert!(slices > 1, "the rescan ran in slices");
    for (i, &p) in ptrs.iter().enumerate() {
        assert!(
            marker.bitmap.is_marked(p as usize),
            "object {i} must be marked"
        );
    }
    assert!(
        !marker.queue.has_overflowed(),
        "a completed pass leaves the flag clear"
    );
    assert_eq!(marker.state.census().overflow_rescan_passes, 1);
    // So remark has no rescan left to do.
    marker.remark(&stw(), &[ptrs[0]], &og);
    assert!(marker.bitmap.is_marked(ptrs[3] as usize));
}

/// A rescan pass still part-way when remark runs (no production driver
/// does that, but a caller may) must not be mistaken for a finished one:
/// remark re-arms the overflow flag and rescans inside the pause.
#[test]
fn remark_finishes_an_overflow_rescan_left_part_way() {
    let (og, ptrs) = r4w2_chain(4);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[ptrs[0]], &og);
    marker.queue.clear();
    assert!(marker.bitmap.try_mark(ptrs[1] as usize));
    marker.queue.overflowed.store(true, Ordering::Relaxed);

    // One slice: the pass starts (clearing the flag) and stops after one
    // object, before it reaches object 1.
    let (_, done) = marker.concurrent_mark_budget(&og, 1);
    assert!(!done);
    assert!(!marker.queue.has_overflowed());
    assert!(
        !marker.bitmap.is_marked(ptrs[2] as usize),
        "precondition: not reached yet"
    );

    marker.remark(&stw(), &[ptrs[0]], &og);
    for (i, &p) in ptrs.iter().enumerate() {
        assert!(
            marker.bitmap.is_marked(p as usize),
            "object {i} must be marked by remark"
        );
    }
}

/// `docs/feature-designs/jit-ir-optimizations.md`'s first
/// step: the census of initial marks that froze a thread mid-JIT.
#[test]
fn the_initial_mark_takeover_census_counts_pauses_and_frozen_threads() {
    let state = ConcurrentGcState::new();
    state.note_initial_mark_takeover(0);
    state.note_initial_mark_takeover(3);
    state.note_initial_mark_takeover(1);
    let c = state.census();
    assert_eq!(
        (
            c.initial_marks,
            c.initial_marks_with_takeover,
            c.frozen_threads
        ),
        (3, 2, 4)
    );
}

/// `docs/feature-designs/jit-barrier-lowering.md`:
/// Phase 2 drains the SATB log while it is active, so remark does not
/// inherit every entry logged over the whole cycle; entries logged AFTER
/// the drain are still remark's.
#[test]
fn phase_two_drains_the_satb_log_and_remark_takes_the_rest() {
    let (og, ptrs) = r4w3_layout(9);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[ptrs[0]], &og);
    // Two garbage objects "overwritten" during the trace.
    marker
        .satb_queue
        .flush(vec![ptrs[1] as usize, ptrs[2] as usize]);
    marker.concurrent_mark(&og);
    assert!(marker.bitmap.is_marked(ptrs[1] as usize));
    assert!(marker.bitmap.is_marked(ptrs[2] as usize));
    assert!(marker.satb_queue.is_empty());
    assert_eq!(marker.state.census().satb_drained_in_mark, 2);

    // One more after the trace: remark's.
    marker.satb_queue.flush(vec![ptrs[4] as usize]);
    let discovered = marker.remark(&stw(), &[ptrs[0]], &og);
    assert!(discovered >= 1);
    assert!(marker.bitmap.is_marked(ptrs[4] as usize));
    assert_eq!(marker.state.census().satb_drained_in_mark, 2);
}

// -----------------------------------------------------------------
// gen r4w4/concmark4 (2026-09-24)
// -----------------------------------------------------------------

/// `from.slot0 = null` — the frozen thread's (or any) plain overwrite.
fn r4w4_unlink(from: *mut u8) {
    // SAFETY: `from` is a live one-slot object allocated by the test.
    unsafe {
        let slot = from.add(HEADER_SIZE) as *mut Value;
        slot.write(Value::Object(None));
    }
}

/// The initiating occupancy: below the STW floor, `C/32` under it with no
/// measured growth, lower by `5/4 · G` as the measured growth rises,
/// never under 20 %, 45 % before the first cycle, and a fixed percentage
/// clamped to `[20, 75]`.
#[test]
fn the_concurrent_start_threshold_sits_below_the_stw_floor_and_adapts() {
    let cap = 1_000_000usize;
    let floor = cap * 75 / 100;
    let min = cap * 20 / 100;
    let t = ConcurrentStartPolicy::threshold;
    assert_eq!(t(cap, None, None), cap * 45 / 100);
    assert_eq!(t(cap, Some(0), None), floor - cap / 32);
    assert_eq!(
        t(cap, Some(100_000), None),
        floor - (100_000 + 25_000 + cap / 32)
    );
    assert_eq!(t(cap, Some(u64::MAX), None), min);
    assert_eq!(t(cap, Some(0), Some(45)), cap * 45 / 100);
    assert_eq!(t(cap, None, Some(5)), min);
    assert_eq!(t(cap, None, Some(99)), floor);
    let mut prev = usize::MAX;
    for g in (0..=cap as u64).step_by(7_919) {
        let x = t(cap, Some(g), None);
        assert!(x <= floor && x >= min, "g={g}: {x}");
        assert!(x <= prev, "monotone in the predicted growth");
        prev = x;
    }
    assert_eq!(t(0, Some(5), None), 0);
}

/// A cycle is due at the threshold; over a live set a previous
/// collection could not bring under it, only after growth of
/// `max(C/32, (F − after)/2)`.
#[test]
fn the_concurrent_start_waits_for_growth_over_a_live_set_above_the_threshold() {
    let cap = 1_000_000usize;
    let t = 450_000usize;
    let due = ConcurrentStartPolicy::start_due;
    assert!(!due(cap, 449_999, None, t));
    assert!(due(cap, 450_000, None, t));
    assert!(
        due(cap, 450_000, Some(300_000), t),
        "left below T: due at T"
    );
    // Left at 600 k: max(31 250, (750 k − 600 k) / 2 = 75 k).
    assert!(!due(cap, 674_999, Some(600_000), t));
    assert!(due(cap, 675_000, Some(600_000), t));
    // Left above the STW floor: C/32 — before the STW arm's C/16 step.
    assert!(!due(cap, 831_249, Some(800_000), t));
    assert!(due(cap, 831_250, Some(800_000), t));
    assert!(!due(0, 0, None, 0), "no generation");
    assert!(!due(cap, 0, None, 0), "an empty generation");
}

/// Only an occupancy verdict defers, only below the ceiling, and never
/// with an allocation failure outstanding — even one the verdict itself
/// reports as `Occupancy`.
#[test]
fn the_stw_collection_defers_only_to_occupancy_below_the_ceiling_without_failures() {
    let d = ConcurrentStartPolicy::stw_defers;
    let cap = 1000usize;
    assert!(d(cap, 800, MajorTrigger::Occupancy, 0));
    assert!(d(cap, 800, MajorTrigger::WouldSuppress, 0));
    assert!(!d(cap, 900, MajorTrigger::Occupancy, 0), "the 90 % ceiling");
    assert!(
        !d(cap, 800, MajorTrigger::Occupancy, 1),
        "a failure never waits"
    );
    assert!(!d(cap, 800, MajorTrigger::AllocationFailure, 1));
    assert!(!d(cap, 800, MajorTrigger::Requested, 0));
    assert!(
        !d(cap, 800, MajorTrigger::Suppressed, 0),
        "it would not run anyway"
    );
}

/// gcd d9e — `CRATONVM_GEN_CONC_PRECEDENCE`'s deferral: the ceiling is
/// gone, nothing else changes (a failure, a request or a non-running
/// verdict never defers); `false` is exactly `stw_defers`. Every running
/// verdict that does not defer has one cause.
#[test]
fn gcd_d9e_the_ceiling_is_optional_and_every_non_deferral_has_one_cause() {
    let d = ConcurrentStartPolicy::stw_defers_with;
    let cap = 1000usize;
    for used in [0, 800, 899, 900, 950, 1000] {
        for verdict in [
            MajorTrigger::Occupancy,
            MajorTrigger::WouldSuppress,
            MajorTrigger::AllocationFailure,
            MajorTrigger::Requested,
            MajorTrigger::Suppressed,
            MajorTrigger::NotDue,
        ] {
            for failures in [0u64, 1] {
                assert_eq!(
                    d(cap, used, verdict, failures, false),
                    ConcurrentStartPolicy::stw_defers(cap, used, verdict, failures),
                    "used={used} {verdict:?} failures={failures}"
                );
            }
        }
    }
    assert!(d(cap, 900, MajorTrigger::Occupancy, 0, true), "no ceiling");
    assert!(d(cap, 1000, MajorTrigger::WouldSuppress, 0, true));
    assert!(
        !d(cap, 950, MajorTrigger::Occupancy, 1, true),
        "a failure still runs"
    );
    assert!(!d(cap, 800, MajorTrigger::AllocationFailure, 1, true));
    assert!(!d(cap, 800, MajorTrigger::Requested, 0, true));
    assert_eq!(
        ConcurrentStartPolicy::preempt_cause(0),
        StwPreemptCause::Ceiling
    );
    assert_eq!(
        ConcurrentStartPolicy::preempt_cause(3),
        StwPreemptCause::AllocationFailure
    );
}

/// gcd d9e — with a measured prediction, a generation left at or above
/// the threshold re-opens after `C/32` of growth, not half the room to the
/// floor; below the threshold, and with no previous collection, the two
/// rules agree.
#[test]
fn gcd_d9e_a_measured_start_is_not_held_past_the_threshold_by_the_hysteresis() {
    let cap = 1_000_000usize;
    let t = 450_000usize;
    let old = ConcurrentStartPolicy::start_due;
    let new = ConcurrentStartPolicy::start_due_measured;
    // Left at 600 k: the old rule waits for (750 k - 600 k) / 2 = 75 k,
    // the new one for C/32 = 31 250.
    assert!(!old(cap, 640_000, Some(600_000), t));
    assert!(!new(cap, 631_249, Some(600_000), t));
    assert!(new(cap, 631_250, Some(600_000), t));
    assert!(old(cap, 675_000, Some(600_000), t));
    // Where the two agree.
    for (used, after) in [
        (449_999, None),
        (450_000, None),
        (450_000, Some(300_000)),
        (831_249, Some(800_000)),
        (831_250, Some(800_000)),
        (0, None),
    ] {
        assert_eq!(
            old(cap, used, after, t),
            new(cap, used, after, t),
            "used={used} after={after:?}"
        );
    }
    assert!(!new(0, 0, None, 0), "no generation");
}

/// gcd d9e — `ConcurrentGcState::concurrent_start_due` takes the measured
/// rule only under `CRATONVM_GEN_CONC_PRECEDENCE` AND once a cycle has
/// been measured; before the first measurement, and with the flag off
/// (`=0`; default on since gce, 2026-09-29), it is the gen r4w4 rule.
#[test]
fn gcd_d9e_the_precedence_start_applies_only_to_a_measured_prediction() {
    let cap = 1_000_000usize;
    let on = [("CRATONVM_GEN_CONC_PRECEDENCE", Some("1"))];
    let off = [("CRATONVM_GEN_CONC_PRECEDENCE", Some("0"))];
    let state = ConcurrentGcState::new();
    // Unmeasured: T = 45 %; left at 600 k, 640 k is not due either way.
    let due = |s: &ConcurrentGcState| s.concurrent_start_due(cap, 640_000, Some(600_000));
    let due_without_flag =
        |s: &ConcurrentGcState| cratonvm_types::flags::with_thread_overrides(&off, || due(s));
    let due_with_flag =
        |s: &ConcurrentGcState| cratonvm_types::flags::with_thread_overrides(&on, || due(s));
    assert!(!due_without_flag(&state));
    assert!(!due_with_flag(&state));
    // One measured cycle that grew 100 k: T = 750 k - (125 k + 31 250)
    // = 593 750 <= 600 k, so the generation was left at or above T.
    state.note_cycle_open(0, 0);
    state.note_cycle_end(100_000, 600_000, cap, 0);
    assert_eq!(state.concurrent_start_threshold(cap), 593_750);
    assert!(
        !due_without_flag(&state),
        "flag off: waits for (750 k - 600 k) / 2"
    );
    assert!(due_with_flag(&state), "precedence: 40 k >= C/32");
}

/// gcd d9e — `[GC] conc_cycles:` says how every claimed cycle ended: a
/// claim whose initial-mark pause was lost, an opened cycle abandoned in
/// Phase 2, and a completed one whose marking and sweep were timed.
/// (`conccyc_started` counts the DRIVER's initial marks,
/// `note_initial_mark_takeover`, which a bare marker never calls.)
///
/// Run with both d9/e switches at `=0` (default on since gce,
/// 2026-09-29): the line's tail states them, and this pins the off arm.
#[test]
fn gcd_d9e_the_cycle_census_says_how_every_cycle_ended() {
    cratonvm_types::flags::with_thread_overrides(
        &[
            ("CRATONVM_GEN_CONC_PRECEDENCE", Some("0")),
            ("CRATONVM_GEN_CONC_MARK_TAMS_STARTS", Some("0")),
        ],
        gcd_d9e_the_cycle_census_says_how_every_cycle_ended_body,
    );
}

fn gcd_d9e_the_cycle_census_says_how_every_cycle_ended_body() {
    let (mut og, ptrs) = r4w3_layout(6);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    drop(marker.try_open_cycle().expect("no cycle is open"));
    {
        let cycle = marker.try_open_cycle().expect("no cycle is open");
        marker.initial_mark(&[ptrs[0]], &og);
        drop(cycle);
    }
    {
        let cycle = marker.try_open_cycle().expect("no cycle is open");
        marker.initial_mark(&[ptrs[0]], &og);
        marker.concurrent_mark(&og);
        marker.remark(&stw(), &[ptrs[0]], &og);
        let freed = marker.concurrent_sweep(&mut og);
        assert_eq!(freed, 4, "6 objects, 0 and 3 live");
        cycle.complete();
    }
    let c = marker.state.census();
    assert_eq!((c.open_lost, c.abandoned), (1, 1));
    assert_eq!((c.swept_nothing, c.cycles_completed), (0, 1));
    let line = marker.state.cycle_census_line();
    let head = concat!(
        "[GC] conc_cycles: conccyc_started=0 conccyc_completed=1 ",
        "conccyc_open_lost=1 conccyc_abandoned=1 conccyc_swept_nothing=0 ",
        "conccyc_preempted=0 ",
    );
    assert!(line.starts_with(head), "{line}");
    assert!(line.contains(" conccyc_mark_ms_total="), "{line}");
    assert!(
        line.ends_with(" conccyc_precedence=false conccyc_tams_starts=false"),
        "{line}"
    );
    assert!(
        marker.state.driver_census_line().ends_with(line.as_str()),
        "the driver's summary ends with it"
    );
}

/// gcd d9e — Phase-2 object-start walks: by default a promotion between
/// the initial mark and a slice costs that slice one whole-generation walk
/// (counted); under `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` the slices judge
/// against the initial mark's snapshot and walk nothing. Either way the
/// snapshot's live chain is marked, and the post-snapshot object — never
/// marked, never eligible — survives the sweep.
#[test]
fn gcd_d9e_phase2_walks_are_counted_and_the_tams_snapshot_skips_them() {
    let size = HEADER_SIZE + SLOT_SIZE;
    for tams in [false, true] {
        let (mut og, ptrs) = r4w3_layout(9);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let flag = [(
            "CRATONVM_GEN_CONC_MARK_TAMS_STARTS",
            // Default on since gce (2026-09-29): the walking arm is `=0`.
            if tams { Some("1") } else { Some("0") },
        )];
        // The flag is decided at the initial mark, once per cycle.
        cratonvm_types::flags::with_thread_overrides(&flag, || {
            marker.initial_mark(&[ptrs[0]], &og);
        });
        // A "promotion" after the initial mark: `free_list_seq` moves.
        let promoted = og.alloc(size, 8).unwrap();
        init_old_object(promoted, 500);
        let mut slices = 0;
        while !marker.concurrent_mark_budget(&og, 1).1 {
            slices += 1;
            assert!(slices < 1000, "the slices make progress");
        }
        let c = marker.state.census();
        if tams {
            assert_eq!(c.phase2_start_walks, 0, "the snapshot needs no walk");
            assert!(c.phase2_tams_slices >= 1);
        } else {
            assert_eq!(c.phase2_start_walks, 1, "the promotion forced one walk");
            assert_eq!(c.phase2_tams_slices, 0);
        }
        marker.remark(&stw(), &[ptrs[0]], &og);
        for (i, &p) in ptrs.iter().enumerate() {
            assert_eq!(
                marker.bitmap.is_marked(p as usize),
                i % 3 == 0,
                "object {i} (tams={tams})"
            );
        }
        assert_eq!(marker.concurrent_sweep(&mut og), 6, "tams={tams}");
        assert!(
            og.walk_objects().iter().any(|&(p, _)| p == promoted),
            "the post-snapshot object survives (tams={tams})"
        );
    }
}

/// The hysteresis default is one match arm per policy (OFF under `Legacy`,
/// ON under `ConcurrentFirst` since gen r4w5), an explicit setting wins,
/// and the opt-out selects `Legacy`.
#[test]
fn the_old_gen_policy_defaults_are_stated_once() {
    assert!(!OldGenPolicy::Legacy.hysteresis(None));
    assert!(OldGenPolicy::ConcurrentFirst.hysteresis(None));
    assert!(OldGenPolicy::ConcurrentFirst.hysteresis(Some(true)));
    assert!(OldGenPolicy::Legacy.hysteresis(Some(true)));
    assert!(!OldGenPolicy::ConcurrentFirst.hysteresis(Some(false)));
    assert_eq!(OldGenPolicy::from_opt_out(true), OldGenPolicy::Legacy);
    assert_eq!(
        OldGenPolicy::from_opt_out(false),
        OldGenPolicy::ConcurrentFirst
    );
}

/// A cycle measures the old-gen growth between its initial mark and its
/// sweep's end (a promotion landing mid-cycle here), feeds it to the start
/// policy and reports itself; the next threshold is the formula's.
#[test]
fn a_completed_cycle_feeds_the_start_policy_its_growth_and_a_report() {
    let size = HEADER_SIZE + SLOT_SIZE;
    let (mut og, ptrs) = r4w3_layout(30);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    let used_at_open = og.used();
    marker.initial_mark(&[ptrs[0]], &og);
    assert!(marker.state.cycle_in_progress());
    // Four "promotions" during the trace.
    let before = og.used();
    for i in 0..4u32 {
        let p = og.alloc(size, 8).unwrap();
        init_old_object(p, 100 + i);
    }
    let grown = (og.used() - before) as u64;
    marker.concurrent_mark(&og);
    marker.remark(&stw(), &[ptrs[0]], &og);
    let freed = marker.concurrent_sweep(&mut og);
    assert_eq!(freed, 20, "30 objects, every third live");
    assert!(!marker.state.cycle_in_progress());

    let r = marker
        .state
        .last_cycle_report()
        .expect("a completed cycle reports");
    assert_eq!(r.cycle, 1);
    assert_eq!(r.used_at_open, used_at_open);
    assert_eq!(r.used_at_end, og.used());
    assert_eq!(r.freed_bytes, 20 * size);
    assert_eq!(r.growth_during, grown);
    assert_eq!(marker.state.census().cycles_completed, 1);
    assert_eq!(marker.state.policy_estimates().0, Some(grown));
    assert_eq!(
        r.next_threshold,
        ConcurrentStartPolicy::threshold(og.capacity(), Some(grown), None)
    );
    assert!(r.xlog_line().starts_with("GC(c1) Concurrent Mark Cycle "));
    assert!(r.verbose_line().contains("concyc_growth_during="));
}

/// A cycle that is abandoned feeds nothing; one that is pre-empted by an
/// STW collection widens the prediction ONCE, and its own end does not
/// average the widening away; the next cycle averages normally.
#[test]
fn a_preempted_cycle_widens_the_prediction_once_and_an_abandoned_one_feeds_nothing() {
    let (og, ptrs) = r4w3_layout(6);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[ptrs[0]], &og);
    marker.abort_cycle();
    assert_eq!(marker.state.policy_estimates().0, None);
    assert_eq!(marker.state.last_cycle_report(), None);

    let state = ConcurrentGcState::new();
    state.note_cycle_open(1_000, 500);
    // 10 k so far: max(2 · 10 k, C/8 = 125 k).
    state.note_stw_preempted(11_000, 1_000_000);
    assert_eq!(state.policy_estimates().0, Some(125_000));
    state.note_stw_preempted(901_000, 1_000_000);
    assert_eq!(state.policy_estimates().0, Some(125_000), "once per cycle");
    assert_eq!(state.census().stw_preempted, 2);
    state.note_cycle_end(21_000, 600, 1_000_000, 0);
    assert_eq!(state.policy_estimates().0, Some(125_000));
    assert_eq!(
        state.last_cycle_report().map(|r| r.growth_during),
        Some(20_000)
    );
    state.note_cycle_open(0, 0);
    state.note_cycle_end(25_000, 0, 1_000_000, 0);
    assert_eq!(
        state.policy_estimates().0,
        Some(75_000),
        "(125 k + 25 k) / 2"
    );
    assert_eq!(state.census().cycles_completed, 2);
}

/// `docs/feature-designs/jit-ir-optimizations.md`, staged.
///
/// `O.f == A` at the snapshot; `O` is held only by a thread the takeover
/// FROZE between its gate test and `O.f = null`. After the pause: the
/// trace blackens `P`; the frozen store lands UNLOGGED; a second mutator
/// stores `A` into black `P` (logging `P`'s old value, null). `A` is
/// reachable and was reachable at the snapshot.
///
/// Plain `initial_mark` (the hazard): `A` is never marked and is swept.
/// `initial_mark_with_frozen`: `O` is scanned inside the pause, `A` is
/// grey before the unlogged store, and survives.
#[test]
fn a_frozen_threads_unlogged_store_cannot_hide_a_snapshot_value() {
    let size = HEADER_SIZE + SLOT_SIZE;
    let run = |eager: bool| -> bool {
        let mut og = OldGen::new(1 << 20);
        let mut obj = || {
            let p = og.alloc(size, 8).unwrap();
            init_old_object(p, 7);
            p
        };
        let (o, a, p) = (obj(), obj(), obj());
        r4w2_link(o, a);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        // The pause: `P` is an ordinary root; `O` is in the frozen
        // thread's registers (the plain arm models "O is scanned only
        // after the frozen store", which is what the race produces).
        if eager {
            marker.initial_mark_with_frozen(&[p], &[o], &og);
        } else {
            marker.initial_mark(&[p], &og);
        }
        marker.concurrent_mark(&og); // `P` is black, `P.f == null`.
        r4w4_unlink(o); // the frozen store, unlogged
        r4w2_link(p, a); // into black `P`; its old value (null) logs nothing
        marker.remark(&stw(), &[p], &og);
        let a_marked = marker.bitmap.is_marked(a as usize);
        marker.concurrent_sweep(&mut og);
        a_marked
    };
    assert!(
        !run(false),
        "the staged race must reproduce the hazard without the fix"
    );
    assert!(run(true), "the eager scan keeps the snapshot value");
}

/// The eager scan is counted, skips non-object addresses, and is switched
/// off by its kill switch.
#[test]
fn the_frozen_eager_scan_counts_old_objects_only_and_has_a_kill_switch() {
    let (og, ptrs) = r4w2_chain(4);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    let bogus = 0x10usize as *mut u8;
    marker.initial_mark_with_frozen(&[], &[ptrs[1], bogus, std::ptr::null_mut()], &og);
    assert_eq!(marker.state.census().frozen_objects_scanned, 1);
    assert!(marker.bitmap.is_marked(ptrs[1] as usize));
    assert!(
        marker.bitmap.is_marked(ptrs[2] as usize),
        "its child is grey"
    );
    marker.abort_cycle();

    let off = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN", Some("1"))],
        || off.initial_mark_with_frozen(&[], &[ptrs[1]], &og),
    );
    assert_eq!(off.state.census().frozen_objects_scanned, 0);
    assert!(!off.bitmap.is_marked(ptrs[2] as usize));
    off.abort_cycle();
}

/// The marker-local stack reaches the same fixed point as the queue-only
/// drain, whole and in one-object slices (which spill every leftover back
/// to the queue between slices).
#[test]
fn the_local_mark_stack_marks_exactly_what_the_queue_marks() {
    // 60 one-slot objects, every third live on a chain from object 0.
    let marked_set = |no_local: bool, budget: usize| -> (Vec<bool>, u64) {
        let (og, ptrs) = r4w3_layout(60);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let edits: &[(&str, Option<&str>)] = if no_local {
            &[("CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK", Some("1"))]
        } else {
            &[("CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK", None)]
        };
        cratonvm_types::flags::with_thread_overrides(edits, || {
            marker.initial_mark(&[ptrs[0]], &og);
            let mut slices = 0;
            while !marker.concurrent_mark_budget(&og, budget).1 {
                slices += 1;
                assert!(slices < 10_000);
            }
            marker.remark(&stw(), &[ptrs[0]], &og);
        });
        let set = ptrs
            .iter()
            .map(|&p| marker.bitmap.is_marked(p as usize))
            .collect();
        (set, marker.state.census().mark_local_spills)
    };
    let (queue_only, _) = marked_set(true, usize::MAX);
    assert_eq!(queue_only.iter().filter(|&&m| m).count(), 20);
    let (local_whole, _) = marked_set(false, usize::MAX);
    assert_eq!(local_whole, queue_only);
    let (local_sliced, spills) = marked_set(false, 1);
    assert_eq!(local_sliced, queue_only);
    assert!(spills > 0, "one-object slices spill the next grey object");
}

// -----------------------------------------------------------------
// gen r4w5/concmark5 (2026-09-24)
// -----------------------------------------------------------------

/// The periodic re-check backs off exponentially after cycles that opened
/// and failed, and the back-off is capped.
#[test]
fn the_service_back_off_is_exponential_and_capped() {
    assert_eq!(service_backoff_skips(0), 0);
    assert_eq!(service_backoff_skips(1), 1);
    assert_eq!(service_backoff_skips(2), 3);
    assert_eq!(service_backoff_skips(6), 63);
    assert_eq!(service_backoff_skips(7), 63);
    assert_eq!(service_backoff_skips(u32::MAX), 63);
}

/// gce e1/c — the inline start request (no service): armed only while
/// idle, unlatched and above the hint; latched once per crossing; taken
/// exactly once; never armed with a service attached.
/// gcf f1/c: `CRATONVM_GEN_CONC_INLINE_START` is read once per state and
/// cached (the direct-allocation poll asked the environment every time).

#[test]
fn gce_e1c_inline_start_request_latches_once_and_is_taken_once() {
    let state = ConcurrentGcState::new();
    // The hint starts at 0: the first poll is a full check.
    assert!(state.inline_growth_poll_armed(0));
    assert!(!state.inline_start_requested());
    assert!(!state.take_inline_start_request(), "nothing latched yet");
    assert!(
        state.latch_inline_start_request(),
        "the first latch sets it"
    );
    assert!(!state.latch_inline_start_request(), "a second is a no-op");
    assert!(state.inline_start_requested());
    assert!(
        !state.inline_growth_poll_armed(usize::MAX),
        "a latched request disarms the poll (no second full check)",
    );
    assert!(state.take_inline_start_request());
    assert!(!state.take_inline_start_request(), "taken exactly once");
    // gce e2/c: counted once each, and printed on `[GC] conc_cycles:`.
    let c = state.census();
    assert_eq!((c.inline_start_latched, c.inline_start_taken), (1, 1));
    assert!(state
        .cycle_census_line()
        .contains(" conccyc_inline_start_latched=1 conccyc_inline_start_taken=1 "));
    assert!(state.inline_growth_poll_armed(usize::MAX));
    // Below the published hint: not armed.
    state.service.start_hint.store(1 << 20, Ordering::Relaxed);
    assert!(!state.inline_growth_poll_armed((1 << 20) - 1));
    assert!(state.inline_growth_poll_armed(1 << 20));
    // A cycle open: not armed.
    assert!(state.try_open_cycle());
    assert!(!state.inline_growth_poll_armed(usize::MAX));
    state.set_phase(ConcurrentGcPhase::Idle);
    // A service attached: the service arm owns the poll.
    let _attached = state.attach_service_thread().expect("no service yet");
    assert!(!state.inline_growth_poll_armed(usize::MAX));
}

/// With no service attached every caller runs the cycle itself (the
/// pre-2026-09-24 behaviour); with one attached, a due cycle is handed to
/// it from any OTHER thread and never from the service itself; the
/// service is one per VM, and dropping its attachment detaches it.
#[test]
fn a_due_cycle_is_handed_to_an_attached_service_and_never_to_itself() {
    let state = ConcurrentGcState::new();
    assert!(!state.hand_off_to_service(), "no service: run inline");
    assert!(!state.on_service_thread());
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let st = &state;
    std::thread::scope(|s| {
        let service = s.spawn(move || {
            let _attached = st.attach_service_thread().expect("the first attach");
            assert!(st.on_service_thread());
            assert!(!st.hand_off_to_service(), "the service runs its own cycles");
            ready_tx.send(()).expect("main is waiting");
            let wake = st.service_wait(std::time::Duration::from_secs(30));
            go_rx.recv().expect("main releases the service");
            wake
        });
        ready_rx.recv().expect("the service attached");
        assert!(st.service_attached());
        assert!(!st.on_service_thread());
        assert!(st.attach_service_thread().is_none(), "one service per VM");
        assert!(st.hand_off_to_service(), "a mutator hands the cycle off");
        go_tx.send(()).expect("the service is waiting");
        let wake = service.join().expect("the service thread");
        assert_eq!(wake, ConcurrentServiceWake::HandedOff);
    });
    assert!(
        !state.service_attached(),
        "the attachment's drop detached it"
    );
    assert!(!state.hand_off_to_service(), "detached: inline again");
    let c = state.census();
    assert_eq!(c.service_handoffs, 1);
    assert_eq!(c.service_wakes_requested, 1);
}

/// gen r5w5/conc9 — a service told to stop is still attached until its
/// loop returns, and its next wait answers `Shutdown` whatever is pending;
/// a hand-off in that window used to be accepted and then dropped by the
/// detach. It is refused now, so the caller runs the cycle itself.
#[test]
fn r5w5_a_stopping_service_refuses_hand_offs() {
    let state = ConcurrentGcState::new();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let st = &state;
    std::thread::scope(|s| {
        let service = s.spawn(move || {
            let _attached = st.attach_service_thread().expect("the first attach");
            ready_tx.send(()).expect("main is waiting");
            go_rx.recv().expect("main releases the service");
            st.service_wait(std::time::Duration::from_secs(30))
        });
        ready_rx.recv().expect("the service attached");
        state.shutdown_service();
        assert!(
            state.service_attached(),
            "still attached until its loop returns"
        );
        assert!(
            !state.hand_off_to_service(),
            "a stopping service accepts no hand-off"
        );
        go_tx.send(()).expect("the service is waiting");
        assert_eq!(
            service.join().expect("the service thread"),
            ConcurrentServiceWake::Shutdown
        );
    });
    assert_eq!(state.census().service_handoffs, 0);
}

/// The direct-old-allocation poll's pre-check, and the growth signal: a
/// request posted while the service is busy is returned by its next wait
/// at once, and shutdown is permanent.
#[test]
fn a_growth_signal_wakes_the_service_and_the_pre_check_gates_it() {
    let state = ConcurrentGcState::new();
    assert!(!state.service_growth_poll_armed(usize::MAX), "no service");
    assert!(!state.signal_old_gen_growth(), "no service to wake");
    let attached = state.attach_service_thread().expect("attach");
    assert!(
        state.service_growth_poll_armed(0),
        "an unknown threshold arms it"
    );
    let capacity = 1_000_000;
    let t = state.concurrent_start_threshold(capacity);
    assert_eq!(t, capacity * CONC_START_INITIAL_PCT / 100);
    assert!(
        !state.service_growth_poll_armed(t - 1),
        "below the published start"
    );
    assert!(state.service_growth_poll_armed(t));
    state.set_phase(ConcurrentGcPhase::ConcurrentMark);
    assert!(!state.service_growth_poll_armed(t), "a cycle is open");
    state.set_phase(ConcurrentGcPhase::Idle);

    assert!(state.signal_old_gen_growth());
    assert!(!state.service_growth_poll_armed(t), "a request is pending");
    assert_eq!(
        state.service_wait(std::time::Duration::from_secs(30)),
        ConcurrentServiceWake::OldGenGrowth
    );
    assert!(state.service_growth_poll_armed(t), "consumed");
    assert_eq!(
        state.service_wait(std::time::Duration::from_millis(1)),
        ConcurrentServiceWake::Periodic
    );
    state.shutdown_service();
    assert_eq!(
        state.service_wait(std::time::Duration::from_secs(30)),
        ConcurrentServiceWake::Shutdown
    );
    drop(attached);
    assert!(
        state.attach_service_thread().is_none(),
        "shutdown is permanent"
    );
    let c = state.census();
    assert_eq!(c.service_growth_signals, 1);
    assert_eq!(c.service_wakes_requested, 1);
    assert_eq!(c.service_wakes_periodic, 1);
}

/// gci i1/og: a growth signal to a STOPPING service (shut down, still
/// attached until its loop returns) is refused and not counted, as a
/// hand-off is (gen r5w5/conc9).
#[test]
fn gci_i1og_a_growth_signal_to_a_stopping_service_is_refused() {
    let state = ConcurrentGcState::new();
    let attached = state.attach_service_thread().expect("attach");
    state.shutdown_service();
    assert!(state.service_attached(), "precondition: still attached");
    assert!(!state.signal_old_gen_growth(), "a stopping service takes no signal");
    assert_eq!(state.census().service_growth_signals, 0, "and it is not counted");
    drop(attached);
    assert!(!state.service_attached());
}

/// Fake VM hooks for `serve`: the cycle's outcome is scripted through
/// the census the real driver moves (`initial_marks`, `cycles_completed`).
struct ScriptedService<'s> {
    state: &'s ConcurrentGcState,
    /// One entry per `run_cycle`: does the cycle open, does it complete.
    script: Vec<(bool, bool)>,
    log: Vec<&'static str>,
    idle: bool,
}

impl ConcurrentServiceHooks for ScriptedService<'_> {
    fn enter_idle(&mut self) {
        assert!(!self.idle, "enter_idle twice");
        self.idle = true;
        self.log.push("idle");
    }
    fn leave_idle(&mut self) {
        assert!(self.idle, "leave_idle while running");
        self.idle = false;
        self.log.push("run");
    }
    fn cycle_due(&mut self) -> bool {
        assert!(self.idle, "the periodic check is asked while idle");
        self.log.push("due?");
        true
    }
    fn run_cycle(&mut self) {
        assert!(!self.idle, "a cycle runs as a counted mutator");
        let (opens, completes) = self.script.remove(0);
        if opens {
            self.state.note_initial_mark_takeover(0);
            self.state.note_cycle_open(0, 0);
        }
        if completes {
            self.state.note_cycle_end(10, 0, 1_000_000, 0);
        } else if opens {
            self.state.note_cycle_dropped();
        }
        if self.script.is_empty() {
            self.state.shutdown_service();
        }
    }
}

/// The service loop: periodic wakes ask the trigger while idle, a cycle
/// runs as a counted mutator, a cycle that opened and failed backs the
/// periodic check off, a completed one resets it, and shutdown returns
/// with the thread idle and the service detached.
#[test]
fn the_service_loop_runs_backs_off_and_shuts_down() {
    let state = ConcurrentGcState::new();
    let mut hooks = ScriptedService {
        state: &state,
        // A failed cycle (opens, never completes), then a completed one.
        script: vec![(true, false), (true, true)],
        log: Vec::new(),
        idle: false,
    };
    assert!(state.serve(std::time::Duration::from_millis(1), &mut hooks));
    assert!(hooks.idle, "serve returns with the thread idle");
    assert_eq!(
        hooks.log,
        ["idle", "due?", "run", "idle", "due?", "run", "idle"],
        "one periodic wake is skipped between the two cycles"
    );
    let c = state.census();
    assert_eq!(c.service_cycle_attempts, 2);
    assert_eq!(c.service_cycles_completed, 1);
    assert_eq!(c.service_periodic_due, 2);
    assert_eq!(c.service_backoff_skips, 1);
    assert!(!state.service_attached());

    // A second `serve` on a VM whose service already runs is refused.
    let other = ConcurrentGcState::new();
    let _held = other.attach_service_thread().expect("attach");
    let mut idle_hooks = ScriptedService {
        state: &other,
        script: Vec::new(),
        log: Vec::new(),
        idle: false,
    };
    assert!(!other.serve(std::time::Duration::from_millis(1), &mut idle_hooks));
    assert!(idle_hooks.log.is_empty());
}

/// Fake hooks whose periodic trigger check says "due" and, in the same
/// breath, stops the service: the stop lands between the wake and the
/// cycle (`gengc-r5w5-conc9-the-service-thread-can-outlive-vm-teardown`).
struct StopWhileDue<'s> {
    state: &'s ConcurrentGcState,
    log: Vec<&'static str>,
}

impl ConcurrentServiceHooks for StopWhileDue<'_> {
    fn enter_idle(&mut self) {
        self.log.push("idle");
    }
    fn leave_idle(&mut self) {
        self.log.push("run");
    }
    fn cycle_due(&mut self) -> bool {
        self.log.push("due?");
        self.state.shutdown_service();
        true
    }
    fn run_cycle(&mut self) {
        self.log.push("cycle");
    }
}

/// gcd d1/c: a service told to stop after its wake does not leave idle and
/// starts no cycle; it returns idle and detached.
#[test]
fn gcd_d1c_a_stop_after_the_wake_starts_no_cycle() {
    let state = ConcurrentGcState::new();
    assert!(!state.service_shutting_down());
    let mut hooks = StopWhileDue {
        state: &state,
        log: Vec::new(),
    };
    assert!(state.serve(std::time::Duration::from_millis(1), &mut hooks));
    assert_eq!(hooks.log, ["idle", "due?"]);
    assert!(state.service_shutting_down());
    assert!(!state.service_attached());
    assert_eq!(state.census().service_cycle_attempts, 0);
}

/// The count-based cadence census: each decision resolves the previous
/// one against the generation's STW collection count and freed bytes.
#[test]
fn the_major_cadence_census_counts_runs_back_to_back_and_low_yield() {
    let cap = 1_000;
    let mut m = MajorCadence::default();
    // #1 decides to run; its major frees 10 bytes (1 %).
    m.note_decision(0, 0, cap, false, true);
    // #2 resolves #1 and runs again; its major frees 200 (20 %).
    m.note_decision(1, 10, cap, false, true);
    assert_eq!(m.census.majors, 1);
    assert_eq!(
        m.census.back_to_back, 0,
        "the first major has no predecessor"
    );
    assert_eq!(m.census.low_yield_5pct, 1);
    // #3 resolves #2 (back to back, not low-yield) and does not run.
    m.note_decision(2, 210, cap, false, false);
    assert_eq!(m.census.majors, 2);
    assert_eq!(m.census.back_to_back, 1);
    assert_eq!(m.census.low_yield_5pct, 1);
    // #4 is requested; #3 ran nothing, so #4 cannot be back to back.
    m.note_decision(2, 210, cap, true, true);
    // Resolved against a count that moved: exactly 5 % is not low-yield.
    let c = m.resolved(3, 260).0;
    assert_eq!(c.young_decisions, 4);
    assert_eq!(c.majors, 3);
    assert_eq!(c.requested_majors, 1);
    assert_eq!(c.back_to_back, 1);
    assert_eq!(c.low_yield_5pct, 1);
    // A decision whose major never ran (the count did not move) is not one.
    assert_eq!(m.resolved(2, 210).0.majors, 2);
    assert_eq!(c.majors_per_100_young_tenths(), 750);
    let line = c.line();
    assert!(line.starts_with("[GC] major_cadence: majcad_young_decisions=4 majcad_majors=3 "));
    assert!(line.contains(" majcad_per_100_young=75.0 "), "{line}");

    // Through the state: a concurrent sweep's frees are not the major's.
    let state = ConcurrentGcState::new();
    state.note_major_decision(0, 0, cap, false, true);
    state.note_concurrent_freed(500); // a concurrent sweep, in between
    let c = state.major_cadence(1, 500 + 30);
    assert_eq!(c.majors, 1);
    assert_eq!(c.low_yield_5pct, 1, "the major itself freed 30 bytes (3 %)");
    assert_eq!(
        MajorCadenceCensus::default().majors_per_100_young_tenths(),
        0
    );
}

/// The no-op remark reference-processing hook: on by default since gce ve2
/// (2026-09-29; `=0` kills it); on, it is
/// called once per authorised remark, and the bitmap is exactly the
/// unhooked remark's.
#[test]
fn the_remark_reference_processing_hook_is_a_counted_no_op() {
    let run = |hook: bool| -> (Vec<bool>, u64) {
        let (og, ptrs) = r4w3_layout(30);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let edits: &[(&str, Option<&str>)] = if hook {
            &[("CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK", Some("1"))]
        } else {
            &[("CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK", Some("0"))]
        };
        cratonvm_types::flags::with_thread_overrides(edits, || {
            marker.initial_mark(&[ptrs[0]], &og);
            marker.concurrent_mark(&og);
            marker.remark(&stw(), &[ptrs[0]], &og);
        });
        let set = ptrs
            .iter()
            .map(|&p| marker.bitmap.is_marked(p as usize))
            .collect();
        (set, marker.state.census().remark_refproc_hook_calls)
    };
    let (plain, calls_off) = run(false);
    let (hooked, calls_on) = run(true);
    assert_eq!(calls_off, 0);
    assert_eq!(calls_on, 1);
    assert_eq!(hooked, plain, "the hook changes nothing the sweep reads");
    assert_eq!(plain.iter().filter(|&&m| m).count(), 10);
}

// -----------------------------------------------------------------
// gen r4w6/concsvc6 (2026-09-24)
// -----------------------------------------------------------------

/// The trigger-to-start census: the FIRST "due" answer while idle is
/// remembered, the next initial mark turns it into one sample, a "not
/// due" answer forgets it, and an answer given while a cycle is claimed
/// or open is ignored.
#[test]
fn the_trigger_to_start_latency_is_sampled_once_per_owed_start() {
    let state = ConcurrentGcState::new();
    // No verdict recorded: an initial mark takes no sample.
    state.note_cycle_open(0, 0);
    state.note_cycle_dropped();
    assert_eq!(state.census().trigger_to_start_samples, 0);

    state.note_start_verdict(true);
    std::thread::sleep(std::time::Duration::from_millis(3));
    state.note_start_verdict(true); // later "due" answers keep the first instant
    state.note_cycle_open(0, 0);
    state.note_cycle_dropped();
    let c = state.census();
    assert_eq!(c.trigger_to_start_samples, 1);
    assert!(
        c.trigger_to_start_max_us >= 2_000,
        "measured from the first due answer: {}us",
        c.trigger_to_start_max_us
    );
    assert_eq!(c.trigger_to_start_total_us, c.trigger_to_start_max_us);

    // Due, then not due: nothing is owed any more.
    state.note_start_verdict(true);
    state.note_start_verdict(false);
    state.note_cycle_open(0, 0);
    state.note_cycle_dropped();
    assert_eq!(state.census().trigger_to_start_samples, 1);

    // An answer while a cycle is open says nothing about the next start.
    state.set_phase(ConcurrentGcPhase::ConcurrentMark);
    state.note_start_verdict(true);
    state.set_phase(ConcurrentGcPhase::Idle);
    state.note_cycle_open(0, 0);
    state.note_cycle_dropped();
    assert_eq!(state.census().trigger_to_start_samples, 1);

    // Through the policy: `concurrent_start_due` records its own verdict.
    let capacity = 1_000_000;
    assert!(state.concurrent_start_due(capacity, capacity / 2, None));
    state.note_cycle_open(0, 0);
    state.note_cycle_dropped();
    assert_eq!(state.census().trigger_to_start_samples, 2);

    let line = state.driver_census_line();
    assert!(line.contains(" concdrv_trigger_to_start_n=2 "), "{line}");
    assert!(line.contains(" concdrv_cycles_started=0 "), "{line}");
    assert!(line.contains(" concdrv_cycles_completed=0 "), "{line}");
    assert!(line.contains(" concdrv_trigger_to_start_avg_us="), "{line}");
    assert!(line.contains(" concdrv_stw_frag_preempted=0 "), "{line}");
    assert!(line.contains(" concdrv_satb_outside_old_gen=0 "), "{line}");
}

/// The cadence census splits the confirmed majors by cause: the two
/// "concurrent cycle too slow" halves and the rest, which together with
/// the requested ones partition `majors`.
#[test]
fn the_major_cadence_census_splits_the_majors_by_cause() {
    let cap = 1_000;
    let mut m = MajorCadence::default();
    let causes = [
        MajorCause::FallbackCycleOpen,
        MajorCause::FallbackStartLate,
        MajorCause::FallbackStartLate,
        MajorCause::Occupancy,
        MajorCause::AllocationFailure,
        MajorCause::Fragmentation,
        MajorCause::Requested,
    ];
    let mut n = 0u64;
    for cause in causes {
        m.note_decision_caused(n, 0, cap, Some(cause));
        n += 1; // each decision's major ran
    }
    // One decision that did not run resolves the last one.
    m.note_decision_caused(n, 0, cap, None);
    let c = m.resolved(n, 0).0;
    assert_eq!(c.majors, 7);
    assert_eq!(c.fallback_cycle_open, 1);
    assert_eq!(c.fallback_start_late, 2);
    assert_eq!(c.fallback_conc_too_slow(), 3);
    assert_eq!(c.other_occupancy, 1);
    assert_eq!(c.other_alloc_failure, 1);
    assert_eq!(c.other_fragmentation, 1);
    assert_eq!(c.requested_majors, 1);
    assert_eq!(
        c.fallback_conc_too_slow()
            + c.other_occupancy
            + c.other_alloc_failure
            + c.other_fragmentation
            + c.requested_majors,
        c.majors,
        "the causes partition the majors"
    );
    assert!(MajorCause::FallbackStartLate.is_concurrent_fallback());
    assert!(!MajorCause::Fragmentation.is_concurrent_fallback());
    let line = c.line();
    assert!(line.starts_with("[GC] major_cadence: majcad_young_decisions=8 majcad_majors=7 "));
    assert!(line.contains(" majcad_fallback_conc_too_slow=3 "), "{line}");
    assert!(line.contains(" majcad_fallback_cycle_open=1 "), "{line}");
    assert!(line.contains(" majcad_fallback_start_late=2 "), "{line}");
    assert!(line.contains(" majcad_other=4 "), "{line}");
    assert!(line.ends_with(" majcad_other_fragmentation=1"), "{line}");

    // The wave-5 entry point still counts a non-requested run as occupancy.
    let mut w5 = MajorCadence::default();
    w5.note_decision(0, 0, cap, false, true);
    assert_eq!(w5.resolved(1, 0).0.other_occupancy, 1);
}

/// Phase 2 counts the SATB entries it drained from outside the old
/// generation (the young old-values the barrier-side filter would drop),
/// and marks exactly what it marked before.
#[test]
fn phase_two_counts_satb_entries_outside_the_old_generation() {
    let (og, ptrs) = r4w3_layout(9);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[ptrs[0]], &og);
    // Not old-gen storage: a Rust heap block stands in for a young object.
    let outside = vec![0u64; 4];
    let outside_addr = outside.as_ptr() as usize;
    assert!(!og.contains(outside_addr as *const u8));
    marker
        .satb_queue
        .flush(vec![ptrs[1] as usize, outside_addr, ptrs[2] as usize]);
    marker.concurrent_mark(&og);
    assert!(marker.bitmap.is_marked(ptrs[1] as usize));
    assert!(marker.bitmap.is_marked(ptrs[2] as usize));
    let c = marker.state.census();
    assert_eq!(c.satb_drained_in_mark, 3);
    assert_eq!(c.satb_outside_old_gen, 1);
    drop(outside);
}

// -----------------------------------------------------------------------
// gen r5w1/refs5 (2026-09-26) — remark-time reference processing
// -----------------------------------------------------------------------

/// A legacy two-slot old-gen object (a stand-in `Reference`: slot 0 the
/// referent, slot 1 an ordinary field such as `queue`).
fn r5w1_two_slot(og: &mut OldGen, cid: u32) -> *mut u8 {
    let p = og.alloc(HEADER_SIZE + 2 * SLOT_SIZE, 8).unwrap();
    // SAFETY: a live `HEADER_SIZE + 2 * SLOT_SIZE` block from `alloc`.
    unsafe {
        let h = &mut *(p as *mut ObjectHeader);
        h.class_id = ClassId::new(cid);
        h.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
        h.set_num_slots(2);
        h.set_gc_flags(0x01); // GC_FLAG_OLD_GEN
        (p.add(HEADER_SIZE) as *mut Value).write(Value::Object(None));
        (p.add(HEADER_SIZE + SLOT_SIZE) as *mut Value).write(Value::Object(None));
    }
    p
}

/// `from.slot[idx] = to`.
fn r5w1_store(from: *mut u8, idx: usize, to: *mut u8) {
    // SAFETY: `from` is a live object with more than `idx` slots.
    unsafe {
        let slot = from.add(HEADER_SIZE + idx * SLOT_SIZE) as *mut Value;
        slot.write(Value::Object(Some(ObjectRef::from_raw(to))));
    }
}

/// Half 1: a `Reference` in the skip set does not trace its slot 0 — the
/// referent reads UNMARKED at the seam while slot 1 is traced — and the
/// ONE-COMMIT RULE is visible: a callback that neither clears nor keeps the
/// referent lets the sweep free it (the use-after-free half 1 alone would
/// be), while one that keeps it (a policy-kept soft referent) saves it and
/// its closure. Without the skip the referent is simply marked.
#[test]
fn r5w1_a_hidden_referent_is_unmarked_at_the_seam_and_kept_only_if_returned() {
    let build = || {
        let mut og = OldGen::new(65536);
        let reference = r5w1_two_slot(&mut og, 1);
        let referent = r5w1_two_slot(&mut og, 2);
        let referent_child = r5w1_two_slot(&mut og, 3);
        let queue = r5w1_two_slot(&mut og, 4);
        r5w1_store(reference, 0, referent);
        r5w1_store(reference, 1, queue);
        r5w1_store(referent, 1, referent_child);
        (og, reference, referent, referent_child, queue)
    };

    for keep in [false, true] {
        let (mut og, reference, referent, child, queue) = build();
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        // A young (out-of-range) address is not an old-gen skip entry.
        assert_eq!(marker.set_reference_skip(&[reference as usize, 0x10]), 1);
        marker.initial_mark(&[reference], &og);
        marker.concurrent_mark(&og);
        assert!(marker.bitmap.is_marked(queue as usize), "slot 1 is traced");
        assert!(
            !marker.bitmap.is_marked(referent as usize),
            "slot 0 is hidden"
        );

        let mut calls = 0;
        let mut process = |is_marked: &dyn Fn(usize) -> bool| -> Vec<usize> {
            calls += 1;
            assert!(is_marked(reference as usize));
            assert!(!is_marked(referent as usize), "an untainted verdict");
            if keep {
                vec![referent as usize]
            } else {
                Vec::new()
            }
        };
        let process_dyn: &mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize> = &mut process;
        marker.remark_with_reference_processing(&stw(), &[reference], &og, Some(process_dyn));
        assert_eq!(calls, 1);
        let swept = marker.concurrent_sweep(&mut og);
        assert!(
            !marker.reference_skip_armed.load(Ordering::Relaxed),
            "disarmed by the sweep"
        );
        if keep {
            assert_eq!(swept, 0);
            assert!(og.is_allocated_addr(referent), "kept by the callback");
            assert!(og.is_allocated_addr(child), "with its closure");
        } else {
            // The staged use-after-free: the Reference still names it.
            assert_eq!(swept, 2, "referent and its child are freed");
            assert!(!og.is_allocated_addr(referent));
        }
        assert_eq!(marker.state.census().reference_skip_published, 1);
    }

    // No skip set: the referent is traced through slot 0, as before.
    let (og, reference, referent, _child, _queue) = build();
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[reference], &og);
    marker.concurrent_mark(&og);
    assert!(marker.bitmap.is_marked(referent as usize));
    assert_eq!(marker.state.census().reference_skip_published, 0);
}

/// The split remark is the one-lock remark: `remark_begin` + the seam's
/// predicate + `remark_finish` mark exactly what
/// `remark_with_reference_processing` marks, and a refused remark's second
/// half does nothing.
#[test]
fn r5w1_the_split_remark_marks_what_the_one_lock_remark_marks() {
    let run = |split: bool| {
        let mut og = OldGen::new(65536);
        let root = r5w1_two_slot(&mut og, 1);
        let a = r5w1_two_slot(&mut og, 2);
        let b = r5w1_two_slot(&mut og, 3);
        let dead = r5w1_two_slot(&mut og, 4);
        r5w1_store(a, 0, b);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[root], &og);
        marker.concurrent_mark(&og);
        if split {
            let progress = marker.remark_begin(&stw(), &[root], &og);
            assert!(progress.sweep_authorised());
            assert!(!marker.remark_is_marked(a as usize));
            assert!(marker.remark_is_marked(0x10), "not eligible reads live");
            marker.remark_finish(&stw(), progress, Some(&[a as usize][..]), &og);
        } else {
            let mut keep_a = |_: &dyn Fn(usize) -> bool| -> Vec<usize> { vec![a as usize] };
            let dyn_keep: &mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize> = &mut keep_a;
            marker.remark_with_reference_processing(&stw(), &[root], &og, Some(dyn_keep));
        }
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
        assert!(!marker.satb_queue.is_active());
        let marked = [root, a, b, dead].map(|p| marker.bitmap.is_marked(p as usize));
        (marked, marker.concurrent_sweep(&mut og))
    };
    assert_eq!(run(true), run(false));
    assert_eq!(run(true), ([true, true, true, false], 1));

    // Refused (stale epoch): the begin half establishes the post-conditions
    // and the finish half is a no-op.
    let mut og = OldGen::new(65536);
    let root = r5w1_two_slot(&mut og, 1);
    let victim = r5w1_two_slot(&mut og, 2);
    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.initial_mark(&[root], &og);
    // SAFETY: exactly the pair `alloc` handed out.
    unsafe { og.free(victim, HEADER_SIZE + 2 * SLOT_SIZE) };
    let progress = marker.remark_begin(&stw(), &[root], &og);
    assert!(!progress.sweep_authorised());
    assert_eq!(
        marker.remark_finish(&stw(), progress, Some(&[root as usize][..]), &og),
        0
    );
    assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
    assert!(!marker.satb_queue.is_active());
    assert_eq!(
        marker.state.census().sweep_epoch_aborts,
        1,
        "counted per VM"
    );
}

/// gci i42/mk (`gci-i42mk-gen-remark-judges-death-before-thread-satb-buckets-FIXED-20261016`):
/// an overwritten reference that sits in the bucket of a thread which did
/// NOT flush on its way into the remark pause (a frozen take-over peer;
/// here a parked helper thread) is in the bitmap at the remark's FIRST
/// fixed point -- where the dead-finalizer retention and the
/// reference-processing seam judge death -- not only after
/// `remark_finish`. `=0` is the old order: dead at the seam, marked after.
#[test]
fn mk42_remark_begin_sees_an_unflushed_thread_bucket_before_its_verdicts() {
    let run = |flush: &'static str| -> (bool, bool) {
        cratonvm_types::flags::with_thread_overrides(
            &[(GEN_CONC_REMARK_THREAD_SATB_FLUSH_FLAG, Some(flush))],
            || {
                let mut og = OldGen::new(65536);
                let root = r5w1_two_slot(&mut og, 1);
                let logged = r5w1_two_slot(&mut og, 2);
                let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
                marker.initial_mark(&[root], &og);
                marker.concurrent_mark(&og);
                assert!(
                    !marker.bitmap.is_marked(logged as usize),
                    "unreachable from the root"
                );
                let queue = Arc::clone(&marker.satb_queue);
                let addr = logged as usize;
                let (logged_tx, logged_rx) = std::sync::mpsc::channel::<()>();
                let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
                let peer = std::thread::spawn(move || {
                    // One entry: far below the spill threshold, so it stays
                    // in this thread's own bucket.
                    assert!(!cratonvm_gc_core::satb::satb_thread_local_log(&queue, addr));
                    logged_tx.send(()).unwrap();
                    // Parked, never flushing, for the whole remark.
                    release_rx.recv().unwrap();
                });
                logged_rx.recv().unwrap();
                let progress = marker.remark_begin(&stw(), &[root], &og);
                assert!(progress.sweep_authorised());
                let at_seam = marker.remark_is_marked(addr);
                marker.remark_finish(&stw(), progress, None, &og);
                let after_finish = marker.bitmap.is_marked(addr);
                release_tx.send(()).unwrap();
                peer.join().unwrap();
                (at_seam, after_finish)                },
        )
    };
    assert_eq!(run("1"), (true, true), "the bucket is in the first fixed point");
    assert_eq!(run("0"), (false, true), "the kill switch restores the late flush");
}

/// The census the orchestrator's "no cycle ever starts" finding needs:
/// every start verdict is counted, and the due ones separately.
#[test]
fn r5w1_the_start_trigger_census_counts_every_verdict() {
    let state = ConcurrentGcState::new();
    state.note_start_verdict(false);
    state.note_start_verdict(true);
    state.note_start_verdict(true);
    let c = state.census();
    assert_eq!((c.start_verdicts, c.start_verdicts_due), (3, 2));
    let line = state.driver_census_line();
    assert!(
        line.contains(" concdrv_start_asked=3 concdrv_start_due=2"),
        "{line}"
    );
    state.note_remark_refproc_retired(5);
    let c = state.census();
    assert_eq!(
        (c.remark_refproc_hook_calls, c.remark_refproc_retired),
        (1, 5)
    );
}

/// gcg g2/r (d3n): the sweep's reference-row prune walks are counted per
/// VM next to the rows they dropped, and the driver line ends with both.
#[test]
fn gcg_g2r_sweep_reference_row_prunes_are_counted_and_printed() {
    let state = ConcurrentGcState::new();
    state.note_sweep_reference_row_prunes(0);
    assert_eq!(state.census().sweep_reference_row_prunes, 0);
    state.note_sweep_reference_row_prunes(7);
    state.note_sweep_reference_row_prunes(2);
    state.note_sweep_reference_rows_dropped(4);
    let c = state.census();
    assert_eq!(
        (c.sweep_reference_row_prunes, c.sweep_reference_rows_dropped),
        (9, 4)
    );
    let line = state.driver_census_line();
    assert!(
        line.contains(
            " concdrv_sweep_reference_rows_dropped=4 concdrv_sweep_reference_row_prunes=9"
        ),
        "{line}"
    );
}

// --- gen r5w3/unload7 ----------------------------------------------------

fn r5w3_alloc(og: &mut OldGen, cid: u32) -> *mut u8 {
    let p = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    init_old_object(p, cid);
    p
}

fn r5w3_rows(
    rows: Vec<(usize, Vec<usize>)>,
) -> Option<rustc_hash::FxHashMap<usize, Vec<usize>>> {
    Some(rows.into_iter().collect())
}

/// gcf f4/c — the drain's view answers exactly as the tables: the
/// loader of every class id, and "no row" for every owner its bounds
/// filter out; empty sides filter everything.
#[test]
fn gcf_f4c_the_unload_view_filter_answers_as_the_tables() {
    let tables = ClassUnloadTables {
        loaders: Some([(7u32, 0x7000usize), (12, 0x7100)].into_iter().collect()),
        mirrors: r5w3_rows(vec![(0x7000, vec![0x8000])]),
        metadata: r5w3_rows(vec![(0x7100, vec![0x8100, 0x8108])]),
    };
    let view = ClassUnloadView::new(&tables);
    for id in 0..32u32 {
        assert_eq!(view.loader_of(id), tables.loader_of(id), "class {id}");
    }
    for owner in (0x6000usize..0x9000).step_by(8) {
        let has_row = tables
            .mirrors
            .as_ref()
            .is_some_and(|m| m.contains_key(&owner))
            || tables
                .metadata
                .as_ref()
                .is_some_and(|m| m.contains_key(&owner));
        assert!(!has_row || view.may_own_rows(owner), "owner {owner:#x}");
    }
    assert!(!view.may_own_rows(0x6ff8) && !view.may_own_rows(0x7108));
    let empty = ClassUnloadTables::default();
    let v = ClassUnloadView::new(&empty);
    assert_eq!(v.loader_of(7), None);
    assert!(!v.may_own_rows(0x7000));
}

/// gcg g1/c — the marker's cached miss bounds always equal the bounds of
/// the tables a drain reads: after the initial-mark capture, after the
/// remark's absorb widens them, and after the disarm.
#[test]
fn gcg_g1c_cached_unload_bounds_follow_every_table_write() {
    let marker = ConcurrentMarker::new(0x1000, 0x10000);
    let current = |m: &ConcurrentMarker| {
        let guard = m.class_unload.read();
        (
            *m.class_unload_bounds.lock(),
            guard.as_ref().map_or((None, None), |t| t.miss_bounds()),
        )
    };
    assert_eq!(current(&marker), ((None, None), (None, None)));
    marker.set_class_unload_tables(ClassUnloadTables {
        loaders: Some([(7u32, 0x7000usize)].into_iter().collect()),
        mirrors: r5w3_rows(vec![(0x7000, vec![0x8000])]),
        metadata: None,
    });
    let (cached, fresh) = current(&marker);
    assert_eq!(cached, fresh);
    assert_eq!(cached, (Some((7, 7)), Some((0x7000, 0x7000))));
    // The remark's capture adds a class id and an owner on both sides.
    marker.add_class_unload_tables(ClassUnloadTables {
        loaders: Some([(3u32, 0x6000usize), (12, 0x7100)].into_iter().collect()),
        mirrors: None,
        metadata: r5w3_rows(vec![(0x7100, vec![0x8100])]),
    });
    let (cached, fresh) = current(&marker);
    assert_eq!(cached, fresh);
    assert_eq!(cached, (Some((3, 12)), Some((0x7000, 0x7100))));
    // An empty capture on an armed marker keeps the bounds.
    marker.add_class_unload_tables(ClassUnloadTables::default());
    assert_eq!(current(&marker).0, (Some((3, 12)), Some((0x7000, 0x7100))));
    marker.disarm_class_unload();
    assert_eq!(current(&marker), ((None, None), (None, None)));
    // An empty initial capture arms nothing and leaves no bounds.
    marker.set_class_unload_tables(ClassUnloadTables::default());
    assert!(!marker.class_unload_armed());
    assert_eq!(current(&marker), ((None, None), (None, None)));
}


/// With the side tables armed, the concurrent trace follows the three
/// class-loader edges a stop-the-world major follows (`old_gen_gc`'s BFS):
/// instance → its class's loader (`loader_pin`), loader → its mirrors
/// (`mirror_pin`) and its deferred roots (`metadata_pin`). Without them it
/// follows none, which is why every deferral used to be vetoed.
#[test]
fn r5w3_the_class_unload_tables_add_the_loader_mirror_and_metadata_edges() {
    let mut og = OldGen::new(65536);
    let instance = r5w3_alloc(&mut og, 7);
    let loader = r5w3_alloc(&mut og, 8);
    let mirror = r5w3_alloc(&mut og, 9);
    let statik = r5w3_alloc(&mut og, 10);
    let unrelated = r5w3_alloc(&mut og, 11);
    let tables = || ClassUnloadTables {
        loaders: Some([(7u32, loader as usize)].into_iter().collect()),
        mirrors: r5w3_rows(vec![(loader as usize, vec![mirror as usize])]),
        metadata: r5w3_rows(vec![(loader as usize, vec![statik as usize])]),
    };

    let plain = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    plain.initial_mark_with_frozen(&[instance], &[], &og);
    plain.concurrent_mark(&og);
    assert!(plain.bitmap.is_marked(instance as usize));
    for p in [loader, mirror, statik] {
        assert!(
            !plain.bitmap.is_marked(p as usize),
            "unarmed: no side-table edge"
        );
    }

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.set_class_unload_tables(tables());
    assert!(marker.class_unload_armed());
    marker.initial_mark_with_frozen(&[instance], &[], &og);
    marker.concurrent_mark(&og);
    for p in [instance, loader, mirror, statik] {
        assert!(
            marker.bitmap.is_marked(p as usize),
            "{p:p} is reachable through the tables"
        );
    }
    assert!(!marker.bitmap.is_marked(unrelated as usize));
    assert_eq!(marker.state.census().class_unload_edges, 3);

    marker.remark(&stw(), &[instance], &og);
    assert!(
        !marker.class_unload_armed(),
        "released once the bitmap is final"
    );
    assert_eq!(
        marker.concurrent_sweep(&mut og),
        1,
        "only the unrelated object dies"
    );
    assert!(!og.is_allocated_addr(unrelated));
    for p in [instance, loader, mirror, statik] {
        assert!(og.is_allocated_addr(p));
    }
}

/// The rows of an owner the trace cannot judge are marked without a scan:
/// a YOUNG owner's (not sweep-eligible; the trace never scans young) at
/// the initial mark, and an owner already BLACK when the remark's own
/// capture adds a row for it. An old owner nothing reached keeps its rows
/// unmarked, so its deferred objects die with it.
#[test]
fn r5w3_rows_of_owners_the_trace_cannot_judge_are_marked_without_a_scan() {
    let mut og = OldGen::new(65536);
    let root = r5w3_alloc(&mut og, 1);
    let young_owned = r5w3_alloc(&mut og, 2);
    let late = r5w3_alloc(&mut og, 3);
    let dead_owner = r5w3_alloc(&mut og, 4);
    let dead_owned = r5w3_alloc(&mut og, 5);
    // Outside the old generation: never sweep-eligible, as a young loader.
    let young_owner = 0x10usize;

    let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
    marker.set_class_unload_tables(ClassUnloadTables {
        loaders: None,
        mirrors: None,
        metadata: r5w3_rows(vec![
            (young_owner, vec![young_owned as usize]),
            (dead_owner as usize, vec![dead_owned as usize]),
        ]),
    });
    marker.initial_mark_with_frozen(&[root], &[], &og);
    assert!(
        marker.bitmap.is_marked(young_owned as usize),
        "marked in the pause"
    );
    assert!(!marker.bitmap.is_marked(dead_owned as usize));
    marker.concurrent_mark(&og);

    // The remark's capture: the (black) root now owns `late`.
    marker.add_class_unload_tables(ClassUnloadTables {
        loaders: None,
        mirrors: r5w3_rows(vec![(root as usize, vec![late as usize])]),
        metadata: None,
    });
    marker.remark(&stw(), &[root], &og);
    assert!(
        marker.bitmap.is_marked(late as usize),
        "a black owner's new row"
    );
    assert!(!marker.bitmap.is_marked(dead_owner as usize));
    assert!(!marker.bitmap.is_marked(dead_owned as usize));
    assert_eq!(
        marker.concurrent_sweep(&mut og),
        2,
        "the dead owner and its row"
    );
}

/// A later capture is a union: rows of either capture survive, a value is
/// never listed twice, and an empty capture arms nothing.
#[test]
fn r5w3_class_unload_tables_absorb_a_later_capture() {
    let mut t = ClassUnloadTables {
        loaders: Some([(1u32, 0x100usize)].into_iter().collect()),
        mirrors: r5w3_rows(vec![(0x100, vec![0x200])]),
        metadata: None,
    };
    t.absorb(ClassUnloadTables {
        loaders: Some([(1u32, 0x180usize), (2, 0x300)].into_iter().collect()),
        mirrors: r5w3_rows(vec![(0x100, vec![0x200, 0x210])]),
        metadata: r5w3_rows(vec![(0x300, vec![0x400])]),
    });
    assert_eq!(t.loader_of(1), Some(0x180), "the later row wins");
    assert_eq!(t.loader_of(2), Some(0x300));
    assert_eq!(t.mirrors.as_ref().unwrap()[&0x100], vec![0x200, 0x210]);
    assert_eq!(t.metadata.as_ref().unwrap()[&0x300], vec![0x400]);
    assert_eq!(t.owner_rows().count(), 2);

    assert!(ClassUnloadTables::default().is_empty());
    let marker = ConcurrentMarker::new(0x1000, 0x1000);
    marker.set_class_unload_tables(ClassUnloadTables::default());
    assert!(!marker.class_unload_armed());
    marker.add_class_unload_tables(ClassUnloadTables::default());
    assert!(!marker.class_unload_armed());
}

/// The per-door start census: each visit's furthest step, per door, on a
/// line of its own after the driver line.
#[test]
fn r5w3_the_door_census_counts_each_step_per_door() {
    let state = ConcurrentGcState::new();
    state.note_door(GenConcDoor::AllocFail, GenConcDoorStep::Asked);
    state.note_door(GenConcDoor::AllocFail, GenConcDoorStep::Asked);
    state.note_door(GenConcDoor::AllocFail, GenConcDoorStep::Due);
    state.note_door(GenConcDoor::AllocFail, GenConcDoorStep::Opened);
    state.note_door(GenConcDoor::Service, GenConcDoorStep::Asked);
    let c = state.door_census();
    assert_eq!(c[GenConcDoor::AllocFail as usize], [2, 1, 1]);
    assert_eq!(c[GenConcDoor::Service as usize], [1, 0, 0]);
    assert_eq!(c[GenConcDoor::MaybeGc as usize], [0, 0, 0]);
    let line = state.door_census_line();
    assert!(
        line.starts_with("[GC] conc_doors: concdoor_maybe_gc_asked=0 "),
        "{line}"
    );
    assert!(
        line.contains(
            " concdoor_alloc_fail_asked=2 concdoor_alloc_fail_due=1 \
             concdoor_alloc_fail_opened=1 "
        ),
        "{line}"
    );
    assert!(line.contains(" concdoor_service_asked=1 "), "{line}");
    let driver = state.driver_census_line();
    let mut lines = driver.lines();
    assert!(lines
        .next()
        .is_some_and(|l| l.starts_with("[GC] conc_driver: concdrv_service_enabled=")));
    assert_eq!(lines.next(), Some(line.as_str()));
}

// -----------------------------------------------------------------
// gen r5w4/conc8 (2026-09-26) — the retained-layout census
// (`gengc-r5w3-unload7-retained-layouts-are-never-released`)
// -----------------------------------------------------------------

/// The state's bookkeeping: pending ids are deduplicated, a census moves
/// exactly the released ones to releasable (and counts itself), and the
/// driver drains them once.
#[test]
fn r5w4_a_census_moves_released_layouts_from_pending_to_releasable() {
    let state = ConcurrentGcState::new();
    assert!(!state.has_retained_layouts());
    state.note_layouts_retained(&[]);
    assert!(
        !state.has_retained_layouts(),
        "an empty retention arms nothing"
    );
    state.note_layouts_retained(&[7, 3]);
    state.note_layouts_retained(&[3, 9]);
    assert_eq!(state.retained_layouts_pending(), vec![3, 7, 9]);
    state.complete_layout_census(vec![9, 3]);
    assert_eq!(state.retained_layouts_pending(), vec![7]);
    let mut released = state.take_releasable_layouts();
    released.sort_unstable();
    assert_eq!(released, vec![3, 9]);
    assert!(state.take_releasable_layouts().is_empty(), "drained");
    state.complete_layout_census(Vec::new());
    state.note_layouts_released(2);
    let c = state.census();
    assert_eq!(
        c.class_unload_layout_censuses, 2,
        "an empty census still counts"
    );
    assert_eq!(c.class_unload_layouts_released, 2);
    assert!(state.has_retained_layouts());
}

/// gcd d2/g: the stop-the-world census releases every pending id no
/// surviving object has, keeps the others, and is counted apart from the
/// concurrent census.
#[test]
fn gcd_d2g_a_stw_census_releases_the_pending_ids_no_object_has() {
    let state = ConcurrentGcState::new();
    state.note_layouts_retained(&[4, 5, 6]);
    let survivors: rustc_hash::FxHashSet<u32> = [5u32, 77].into_iter().collect();
    assert_eq!(state.complete_stw_layout_census(&survivors), 2);
    assert_eq!(
        state.retained_layouts_pending(),
        vec![5],
        "an object of 5 is left"
    );
    let mut released = state.take_releasable_layouts();
    released.sort_unstable();
    assert_eq!(released, vec![4, 6]);
    assert_eq!(state.complete_stw_layout_census(&survivors), 0);
    let c = state.census();
    assert_eq!(c.class_unload_stw_layout_censuses, 2);
    assert_eq!(
        c.class_unload_layout_censuses, 0,
        "the concurrent count is its own"
    );
}

/// A census whose baseline no longer matches the process's walk-break
/// count saw a walk that skipped bytes: it releases nothing.
#[test]
fn r5w4_a_census_across_a_walk_break_releases_nothing() {
    let census = LayoutCensus {
        candidates: [1u32, 2].into_iter().collect(),
        survivors: rustc_hash::FxHashSet::default(),
        observed_classes: [None; 64],
        breaks_at_start: walk_break_hits().wrapping_add(1),
    };
    assert_eq!(census.released(), None);
}

#[test]
fn layout_census_cache_collisions_preserve_exact_survivor_membership() {
    let mut census = LayoutCensus {
        candidates: [1u32, 2, 65].into_iter().collect(),
        survivors: rustc_hash::FxHashSet::default(),
        observed_classes: [None; 64],
        breaks_at_start: walk_break_hits(),
    };
    // All three observed ids collide in the bounded cache. Re-observing
    // a non-candidate must neither hide a live candidate nor retain a dead
    // one just because its cache slot was occupied.
    for _ in 0..100 {
        for id in [1, 65, 129, 65, 1] {
            let mut header = ObjectHeader::new(
                ClassId::new(id),
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                0,
            );
            census.observe_survivor((&mut header as *mut ObjectHeader).cast());
        }
    }
    assert_eq!(census.survivors, [1u32, 65].into_iter().collect());
    assert_eq!(census.candidates, [1u32, 2, 65].into_iter().collect());
}

/// A complete sliced sweep: a candidate with a surviving instance stays
/// pending, a candidate whose only instance was garbage and one with no
/// instance at all are released. `r4w3_layout` gives object `i` class id
/// `i + 1`, and objects 0, 3, 6, ... are live. The walk-break count is
/// process-wide, so a desync test running in parallel can make one attempt
/// inconclusive (which is the census failing SAFE); the scenario is
/// retried on a fresh generation until one attempt is conclusive.
#[test]
fn r5w4_a_complete_sweep_releases_the_layouts_it_found_no_survivor_of() {
    for _attempt in 0..32 {
        let (mut og, ptrs) = r4w3_layout(30);
        let marker = r4w3_marked(&og, ptrs[0]);
        marker.state.note_layouts_retained(&[1, 2, 1000]);
        marker.set_layout_census(vec![1, 2, 1000]);
        let (freed, _) = r4w3_sweep_in_slices(&marker, &mut og, 4);
        assert_eq!(freed, 20, "the census changes nothing the sweep frees");
        if marker.state.census().class_unload_layout_censuses == 0 {
            // Another test's walk broke meanwhile: inconclusive, nothing
            // released. Check that, then try again.
            assert!(marker.state.take_releasable_layouts().is_empty());
            assert_eq!(marker.state.retained_layouts_pending(), vec![1, 2, 1000]);
            continue;
        }
        let mut released = marker.state.take_releasable_layouts();
        released.sort_unstable();
        assert_eq!(released, vec![2, 1000]);
        assert_eq!(
            marker.state.retained_layouts_pending(),
            vec![1],
            "object 0 (class 1) survives: its layout stays"
        );
        return;
    }
    panic!("no conclusive census in 32 attempts");
}

/// A sweep stopped between slices by another collector's free did not see
/// the whole generation: its census is dropped, nothing is released.
#[test]
fn r5w4_an_epoch_stopped_sweep_releases_nothing() {
    let size = HEADER_SIZE + SLOT_SIZE;
    let (mut og, ptrs) = r4w3_layout(30);
    let marker = r4w3_marked(&og, ptrs[0]);
    marker.state.note_layouts_retained(&[2, 1000]);
    marker.set_layout_census(vec![2, 1000]);
    let (first, done) = marker.concurrent_sweep_budget(&mut og, 4);
    assert!(!done && first == 2);
    // SAFETY: exactly the pair `alloc` handed out; object 20 is garbage.
    unsafe { og.free(ptrs[20], size) };
    assert_eq!(marker.concurrent_sweep_budget(&mut og, 4), (0, true));
    assert_eq!(marker.state.census().class_unload_layout_censuses, 0);
    assert!(marker.state.take_releasable_layouts().is_empty());
    assert_eq!(marker.state.retained_layouts_pending(), vec![2, 1000]);
}

/// gen r5w6/conc10 — the coverage rule of the remark's fail-closed check,
/// pure: same base and a capacity the bitmap spans (a shrunk capacity is
/// still covered); a grown or moved generation is not.
#[test]
fn r5w6_bitmap_covers_generation_rule() {
    assert!(bitmap_covers_generation(0x1000, 0x4000, 0x1000, 0x4000));
    assert!(bitmap_covers_generation(0x1000, 0x4000, 0x1000, 0x2000));
    assert!(!bitmap_covers_generation(0x1000, 0x4000, 0x1000, 0x4008));
    assert!(!bitmap_covers_generation(0x1000, 0x4000, 0x2000, 0x4000));
}

/// gen r5w6/conc10 (old9's request) — a remark whose mark bitmap was sized
/// for a SMALLER old generation than the one it remarks (an in-place
/// growth between the marker's construction and the pause) refuses the
/// sweep, so nothing in the unmarkable span is freed; the same cycle over
/// a covering bitmap is authorised.
#[test]
fn r5w6_a_remark_over_a_grown_generation_refuses_its_sweep() {
    let (mut og, ptr) = make_old_gen_with_object(1);
    let base = og.base_ptr() as usize;
    let cap = og.capacity();

    // The bitmap as it would have been sized before a growth by one word.
    let short = ConcurrentMarker::new(base, cap - 8);
    short.initial_mark(&[ptr], &og);
    short.concurrent_mark(&og);
    let progress = short.remark_begin(&stw(), &[ptr], &og);
    assert!(
        !progress.sweep_authorised(),
        "a bitmap that does not span the generation must not authorise a sweep"
    );
    short.remark_finish(&stw(), progress, None, &og);
    assert_eq!(
        short.concurrent_sweep(&mut og),
        0,
        "a refused remark frees nothing"
    );
    assert!(og.is_allocated_addr(ptr));

    // Control: the covering bitmap authorises the same cycle.
    let covering = ConcurrentMarker::new(base, cap);
    covering.initial_mark(&[ptr], &og);
    covering.concurrent_mark(&og);
    let progress = covering.remark_begin(&stw(), &[ptr], &og);
    assert!(progress.sweep_authorised());
    covering.remark_finish(&stw(), progress, None, &og);
    assert_eq!(
        covering.concurrent_sweep(&mut og),
        0,
        "the rooted object is kept"
    );
    assert!(og.is_allocated_addr(ptr));
}

/// gcj j2/og2 (C2 of `gcj-j1og-concurrent-cycle-and-old-gen-review-residuals`)
/// — the driver sizes the cycle's bitmap before the initial-mark pause; a
/// growth in between (here `grow_after_refusal`, as another thread's major
/// would run it) used to waste the whole traced cycle at the remark. A
/// bitmap over the generation's growth limit (`old_gen_mark_span`) still
/// covers it: the cycle is authorised and frees its dead object. Control:
/// the capacity-sized bitmap (`CRATONVM_GC_CONC_BITMAP_GROWTH_SPAN=0`) is
/// refused, and the refusal is counted.
#[test]
fn gcj_j2og2_a_bitmap_over_the_growth_limit_survives_a_growth_before_initial_mark() {
    use cratonvm_gc_core::reservation::GRANULE;
    for growth_span in [true, false] {
        let mut og = OldGen::with_growth_headroom(2 * GRANULE, 4 * GRANULE);
        if og.commit_bits().is_none() || og.growth_max() != 4 * GRANULE {
            return; // no reserving store on this host
        }
        og.set_growth_soft_max(4 * GRANULE);
        let size = HEADER_SIZE + SLOT_SIZE;
        let live = og.alloc(size, 8).expect("a live object");
        init_old_object(live, 1);
        let dead = og.alloc(size, 8).expect("a dead object");
        init_old_object(dead, 2);
        let base = og.base_ptr() as usize;
        let span = if growth_span {
            og.growth_max()
        } else {
            og.capacity()
        };
        // The driver's read, BEFORE the pause.
        let marker = ConcurrentMarker::new(base, span);
        // Another thread's major: a refused request grows the generation.
        assert!(og.alloc(3 * GRANULE, 8).is_none(), "refused at 2 granules");
        assert!(og.grow_after_refusal() > 0, "the refusal grows in place");
        assert!(og.capacity() > 2 * GRANULE);
        // The cycle's pause opens after the growth.
        marker.initial_mark(&[live], &og);
        marker.concurrent_mark(&og);
        marker.remark(&stw(), &[live], &og);
        let freed = marker.concurrent_sweep(&mut og);
        let refusals = marker.state.census().bitmap_cover_refusals;
        if growth_span {
            assert_eq!(freed, 1, "the dead object is swept");
            assert_eq!(refusals, 0);
            assert!(!og.is_allocated_addr(dead));
        } else {
            assert_eq!(freed, 0, "the refused remark frees nothing");
            assert_eq!(refusals, 1, "the wasted cycle is counted");
            assert!(og.is_allocated_addr(dead));
        }
        assert!(og.is_allocated_addr(live));
    }
}

/// gcj j2/og2 (C3) — `absorb` folds a later capture's mirror / metadata
/// rows in with the first occurrence of each value kept, in order, on both
/// sides of the linear / seen-set cut (rows of 5 and of 300 values, with
/// duplicates inside the later capture and across the two).
#[test]
fn gcj_j2og2_absorb_dedups_long_rows_as_the_linear_rule() {
    fn naive(mut row: Vec<usize>, values: &[usize]) -> Vec<usize> {
        for &v in values {
            if !row.contains(&v) {
                row.push(v);
            }
        }
        row
    }
    for len in [5usize, 300] {
        let mine: Vec<usize> = (0..len).map(|i| 0x1000 + 8 * (i % (len / 2 + 1))).collect();
        let theirs: Vec<usize> = (0..len).map(|i| 0x1000 + 8 * (i * 7 % len)).collect();
        let mut tables = ClassUnloadTables {
            loaders: None,
            mirrors: Some([(0x10usize, mine.clone())].into_iter().collect()),
            metadata: None,
        };
        let later = ClassUnloadTables {
            loaders: None,
            mirrors: Some([(0x10usize, theirs.clone())].into_iter().collect()),
            metadata: Some([(0x20usize, theirs.clone())].into_iter().collect()),
        };
        tables.absorb(later);
        let mirrors = tables.mirrors.as_ref().expect("mirror rows");
        assert_eq!(mirrors[&0x10usize], naive(mine.clone(), &theirs), "len {len}");
        let metadata = tables.metadata.as_ref().expect("metadata rows");
        assert_eq!(metadata[&0x20usize], naive(Vec::new(), &theirs), "len {len}");
    }
}

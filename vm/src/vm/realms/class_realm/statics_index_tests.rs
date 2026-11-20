use super::*;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

/// A lock-free static read must never observe halves of two different
/// writes.
///
/// [`StaticsIndex::get`] used to copy the whole 16-byte `Value` cell in one
/// go, and a 16-byte access is not single-copy atomic on x86-64: a reader
/// racing a `putstatic` of a `long` saw the high half of one value with the
/// low half of another. `probes/StaticRaceProbe.java` reproduces it from
/// Java in seconds — on the interpreter too, since `get_static_shared`
/// routes through here — and for a `volatile` long that is a JLS §17.7
/// violation, not merely the permitted non-atomic plain-long read.
///
/// This is the same race in miniature: one writer flipping a slot between
/// two patterns, four readers asserting they only ever see those two.
///
/// **It does not, on its own, reproduce the historical tear.** Whether
/// rustc emits one wide copy or two 8-byte loads depends on the inlining
/// context, and in this test's context it already chose the narrow form:
/// verified by reverting the fix, at which point the Java probe still tore
/// within seconds and this test still passed. So read it as a cheap CI pin
/// on the invariant, not as the reproduction — that is
/// `probes/StaticRaceProbe.java`, and a future rewrite of `get` has to be
/// re-checked against the probe, not against this.
#[test]
fn get_never_returns_a_torn_long() {
    const A: i64 = 0x0123_4567_89AB_CDEF;
    const B: i64 = 0x7EDC_BA98_7654_3210;

    let block = StaticsBlock::new(4);
    let index = Arc::new(StaticsIndex::new());
    index.publish(ClassId::new(3), &block);
    // The block is leaked by construction, so a raw base pointer outlives
    // both threads; `usize` to carry it across the spawn boundary.
    let base = block.base_ptr() as usize;

    let stop = Arc::new(AtomicBool::new(false));
    let writer_stop = Arc::clone(&stop);
    let writer = std::thread::spawn(move || {
        let slot = base as *mut Value;
        let mut flip = false;
        while !writer_stop.load(AtomicOrdering::Relaxed) {
            flip = !flip;
            // Exactly what `set_static_shared` does to the cell, minus the
            // lock the lock-free reader does not take either.
            // SAFETY: `slot` is the leaked block's first cell.
            unsafe { *slot = Value::Long(if flip { B } else { A }) };
        }
    });

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let index = Arc::clone(&index);
            std::thread::spawn(move || {
                for _ in 0..200_000 {
                    match index.get(ClassId::new(3), 0) {
                        Some(Value::Long(v)) => assert!(
                            v == A || v == B,
                            "torn long static read: {v:#x} is neither {A:#x} nor {B:#x}"
                        ),
                        // The writer only ever stores `Value::Long`; the
                        // initial `Value::Int(0)` is legitimate until its
                        // first store lands.
                        Some(Value::Int(0)) => {}
                        other => panic!("unexpected static cell: {other:?}"),
                    }
                }
            })
        })
        .collect();

    for r in readers {
        r.join().expect("reader thread");
    }
    stop.store(true, AtomicOrdering::Relaxed);
    writer.join().expect("writer thread");
}

/// zt t18/zb1 (zr1br item 2): `StaticsBlock::load_cell` / `store_cell`
/// round-trip every variant through per-word atomics, write the unused
/// half of a narrow variant as zero, and refuse an index past the end
/// (no read, no write).
#[test]
fn statics_block_cells_round_trip_through_atomic_words() {
    let block = StaticsBlock::new(3);
    // SAFETY: never dereferenced; only its bits are stored and compared.
    let r = unsafe { ObjectRef::from_raw(0x1000usize as *mut u8) };
    for v in [
        Value::Int(-5),
        Value::Long(i64::MIN + 3),
        Value::Float(1.5),
        Value::Double(-0.25),
        Value::Object(Some(r)),
        Value::Object(None),
    ] {
        block.store_cell(1, v);
        assert_eq!(block.load_cell(1), Some(v), "{v:?}");
    }
    block.store_cell(1, Value::Long(-1));
    block.store_cell(1, Value::Int(9));
    // SAFETY: cell 1 of a live 3-cell block; its payload word is 8-aligned.
    let payload = unsafe { *((block.base_ptr().add(1) as *const u64).add(1)) };
    assert_eq!(payload, 0, "a narrow store zeroes the payload word");
    assert_eq!(block.load_cell(3), None, "past the end");
    block.store_cell(3, Value::Int(1)); // a no-op, not an out-of-bounds write
    assert_eq!(block.load_cell(0), Some(Value::Int(0)));
    assert_eq!(block.load_cell(2), Some(Value::Int(0)));
}

/// The address handed to compiled code is the POINTER CELL, so a
/// republication (a grown or re-prepared block) is followed with no
/// patching — that is the whole reason the backend pays an extra load.
#[test]
fn base_cell_addr_is_stable_across_republication() {
    let index = StaticsIndex::new();
    let first = StaticsBlock::new(2);
    index.publish(ClassId::new(9), &first);
    let cell = index
        .base_cell_addr(ClassId::new(9), 1)
        .expect("published block must resolve");

    let second = StaticsBlock::new(8);
    assert_ne!(
        first.base_ptr(),
        second.base_ptr(),
        "the two blocks must be distinct allocations for this test to mean anything"
    );
    index.publish(ClassId::new(9), &second);

    assert_eq!(
        index.base_cell_addr(ClassId::new(9), 1),
        Some(cell),
        "the pointer cell address must not move when the block does"
    );
    // And what compiled code loads through it is the NEW block.
    // SAFETY: `cell` is the address of this index's `AtomicPtr` slot.
    let observed = unsafe {
        (*(cell as *const std::sync::atomic::AtomicPtr<Value>)).load(Ordering::Acquire)
    };
    assert_eq!(observed, second.base_ptr());
}

/// Republication in both directions: a grow and a shrink (the
/// `prepare_class_shared`-after-early-store shape) each leave the index
/// answering exactly the newest block's bounds, and `cell_ptr` names a
/// cell of that block.
#[test]
fn republication_bounds_follow_the_newest_block_both_ways() {
    let index = StaticsIndex::new();
    let id = ClassId::new(21);
    let small = StaticsBlock::new(2);
    index.publish(id, &small);
    assert!(index.cell_ptr(id, 1).is_some());
    assert!(index.cell_ptr(id, 2).is_none());

    let big = StaticsBlock::new(8);
    index.publish(id, &big);
    assert_eq!(index.cell_ptr(id, 5), Some(big.base_ptr().wrapping_add(5)));
    assert!(index.get(id, 7).is_some());
    assert!(index.get(id, 8).is_none());

    let shrunk = StaticsBlock::new(3);
    index.publish(id, &shrunk);
    assert_eq!(
        index.cell_ptr(id, 2),
        Some(shrunk.base_ptr().wrapping_add(2))
    );
    assert!(
        index.cell_ptr(id, 5).is_none(),
        "shrink must not keep the old length"
    );
    assert!(index.get(id, 3).is_none());
}

/// Out of range in either direction means "keep the helper", never a baked
/// address that would read past the block.
#[test]
fn base_cell_addr_refuses_unpublished_and_out_of_bounds() {
    let index = StaticsIndex::new();
    assert_eq!(index.base_cell_addr(ClassId::new(4), 0), None);
    let block = StaticsBlock::new(2);
    index.publish(ClassId::new(4), &block);
    assert!(index.base_cell_addr(ClassId::new(4), 1).is_some());
    assert_eq!(index.base_cell_addr(ClassId::new(4), 2), None);
    assert_eq!(
        index.base_cell_addr(ClassId::new(StaticsIndex::CAPACITY as u32), 0),
        None
    );
}

/// An unloaded class's entry stops answering every lock-free consumer, and
/// the class can be published again afterwards.
#[test]
fn unpublish_withdraws_every_lock_free_answer() {
    let index = StaticsIndex::new();
    let id = ClassId::new(33);
    // Unpublishing in a VM that never published is a no-op.
    index.unpublish(id);
    let block = StaticsBlock::new(4);
    index.publish(id, &block);
    let cell = index.base_cell_addr(id, 0);
    assert!(cell.is_some());
    index.unpublish(id);
    assert!(index.get(id, 0).is_none());
    assert!(index.cell_ptr(id, 0).is_none());
    assert!(index.base_cell_addr(id, 0).is_none());
    // A baked cell still names a readable, non-null base.
    let baked = cell.map(|addr| {
        // SAFETY: the slot array is allocated once and never freed.
        let base_cell = unsafe { &*(addr as *const std::sync::atomic::AtomicPtr<Value>) };
        base_cell.load(AtomicOrdering::Acquire)
    });
    assert_eq!(baked, Some(block.base_ptr()));
    let again = StaticsBlock::new(2);
    index.publish(id, &again);
    assert_eq!(
        index.cell_ptr(id, 1),
        Some(again.base_ptr().wrapping_add(1))
    );
    // Out-of-capacity ids are ignored.
    index.unpublish(ClassId::new(StaticsIndex::CAPACITY as u32));
}

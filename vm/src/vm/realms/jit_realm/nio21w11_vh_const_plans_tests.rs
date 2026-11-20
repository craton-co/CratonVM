use super::VhConstPlans;
use cratonvm_jit::ir_lower::{
    vh_const_query, vh_const_query_decode, VhConstPlan, VhConstRefRow,
    VH_CONST_REF_NAME_PROVES, WELL_KNOWN_VH_CONST_PLAN_TAG, WELL_KNOWN_VH_CONST_REF_ROW_TAG,
};

/// A filed word reads back; an unfiled key answers 0; a second filing
/// under the same key does not replace the first (the row addresses the
/// compile reads must not move).
#[test]
fn a_plan_is_filed_once_and_reads_back() {
    let plans = VhConstPlans::new();
    let word = VhConstPlan {
        receiver_class_id: 77,
        slot: 3,
        kind: b'J',
        read_only: false,
    }
    .to_word();
    assert_eq!(plans.plan_word((5, 1)), 0);
    plans.file((5, 1), word, None);
    assert_eq!(plans.plan_word((5, 1)), word);
    assert_eq!(
        plans.ref_row_addr((5, 1)),
        0,
        "a primitive field has no row"
    );
    plans.file((5, 1), word + 1, None);
    assert_eq!(plans.plan_word((5, 1)), word, "first filing wins");
    assert_eq!(VhConstPlan::from_word(word).map(|p| p.slot), Some(3));
}

/// A reference row is boxed, names its class through a stable pointer, and
/// keeps its address when more rows are filed (the map may rehash).
#[test]
fn a_reference_row_is_stable_and_names_its_class() {
    let plans = VhConstPlans::new();
    let word = VhConstPlan {
        receiver_class_id: 9,
        slot: 0,
        kind: b'L',
        read_only: false,
    }
    .to_word();
    plans.file(
        (9, 2),
        word,
        Some((VH_CONST_REF_NAME_PROVES, 41, Some(Box::from("a/b/Node")))),
    );
    let addr = plans.ref_row_addr((9, 2));
    assert_ne!(addr, 0);
    for i in 0..512u32 {
        plans.file((1000 + i, 0), word, Some((0, 0, None)));
    }
    assert_eq!(plans.ref_row_addr((9, 2)), addr, "the row did not move");
    // SAFETY: `addr` names the boxed row `plans` owns, alive here.
    let row = unsafe { &*(addr as usize as *const VhConstRefRow) };
    assert_eq!(row.flags, VH_CONST_REF_NAME_PROVES);
    assert_eq!(row.type_class_id, 41);
    // SAFETY: the row's name describes the boxed `str` the same entry owns.
    let name = unsafe { std::slice::from_raw_parts(row.name_ptr as *const u8, row.name_len) };
    assert_eq!(name, b"a/b/Node");
}


/// JIT r25 wave 8 (W8-2): one `instanceof` memo per (target, VM), at one
/// address across asks (recompiles share it), every way starting at the
/// target's own id; another VM's displays own another memo; none for
/// class id 0 or a VM that keeps no displays.
#[test]
fn r25_w8_a_miss_memo_is_per_target_and_vm_and_starts_at_the_target() {
    use super::SupersDisplays;
    use std::sync::atomic::{AtomicU32, Ordering};
    let (a, b) = (SupersDisplays::new(), SupersDisplays::new());
    assert_eq!(a.miss_memo_addr(0), 0);
    if !a.enabled() {
        assert_eq!(a.miss_memo_addr(77), 0, "no displays, no memo");
        return;
    }
    let memo = a.miss_memo_addr(77);
    assert_ne!(memo, 0);
    assert_eq!(memo % std::mem::align_of::<AtomicU32>(), 0);
    assert_eq!(memo, a.miss_memo_addr(77), "recompiles share it");
    assert_ne!(memo, a.miss_memo_addr(78), "per target");
    assert_ne!(memo, b.miss_memo_addr(77), "per VM");
    // SAFETY: the address of a `[AtomicU32; 4]` that `a` owns, alive here.
    let ways = unsafe { &*(memo as *const [AtomicU32; 4]) };
    for way in ways {
        assert_eq!(way.load(Ordering::Relaxed), 77);
    }
}

/// nio21 wave 12 L2: a published display reads back through the cell in
/// the layout the compiled test reads; growth keeps old entries and the
/// old table; a second publish is ignored.
#[test]
fn supers_displays_publish_grow_and_read_back() {
    use super::SupersDisplays;
    let d = SupersDisplays::new();
    assert_eq!(d.depth_of(7), None);
    d.publish(7, &[1, 7], &[]);
    d.publish(9, &[1, 9, 5], &[]); // not ending with the class: ignored
    assert_eq!(d.depth_of(9), None);
    let first_table = {
        // SAFETY: the cell holds a table `d` owns, alive here.
        unsafe { *(d.cell_addr() as *const usize) }
    };
    d.publish(5000, &[1, 7, 5000], &[40, 41]);
    d.publish(7, &[1, 2, 7], &[]);
    assert_eq!(d.depth_of(7), Some(1), "first publish wins");
    assert_eq!(d.depth_of(5000), Some(2));
    // nio21 wave 14 L4: the lock-free probe the lazy publishers ask.
    assert!(d.is_published(5000) && d.is_published(7));
    assert!(!d.is_published(9) && !d.is_published(1_000_000));
    // SAFETY: as above.
    let table = unsafe { *(d.cell_addr() as *const usize) } as *const u64;
    assert_ne!(table as usize, first_table, "grown");
    // SAFETY: the table and displays are owned by `d`, alive here; the
    // indices are inside the published capacity.
    unsafe {
        let cap = *table;
        assert!(cap > 5000);
        let entry = *table.add(1 + 5000) as *const u32;
        assert_eq!(
            std::slice::from_raw_parts(entry, 7),
            &[3, 1, 7, 5000, 2, 40, 41]
        );
        let entry7 = *table.add(1 + 7) as *const u32;
        assert_eq!(std::slice::from_raw_parts(entry7, 3), &[2, 1, 7]);
        assert_eq!(*table.add(1 + 9), 0);
        // The old table still holds what it held.
        let old = first_table as *const u64;
        assert_eq!(*(*old.add(1 + 7) as *const u32), 2);
    }
    // JIT r20 wave 17 (lane pAlloc2): the trailing verdict word, `0`
    // through `publish`, as given through `publish_with_verdict`.
    d.publish_with_verdict(6000, &[1, 7, 6000], &[40], true);
    // SAFETY: as above; 6000 is inside the grown capacity.
    unsafe {
        let table = *(d.cell_addr() as *const usize) as *const u64;
        let entry = *table.add(1 + 6000) as *const u32;
        assert_eq!(
            std::slice::from_raw_parts(entry, 7),
            &[3, 1, 7, 6000, 1, 40, 1]
        );
        let entry5000 = *table.add(1 + 5000) as *const u32;
        assert_eq!(*entry5000.add(7), 0, "publish writes verdict 0");
    }
}

/// The query word round-trips through the VM's decode, and no small
/// well-known code decodes as one.
#[test]
fn the_query_words_round_trip() {
    for tag in [
        WELL_KNOWN_VH_CONST_PLAN_TAG,
        WELL_KNOWN_VH_CONST_REF_ROW_TAG,
    ] {
        let which = vh_const_query(tag, u32::MAX, u16::MAX);
        assert_eq!(vh_const_query_decode(which), Some((tag, u32::MAX, 0xFFFF)));
        let which = vh_const_query(tag, 0x1234, 7);
        assert_eq!(vh_const_query_decode(which), Some((tag, 0x1234, 7)));
    }
    for small in [0i64, 1, 2, 3, 8, 0x5E55_0040, 0x1D_0001, -1] {
        assert_eq!(vh_const_query_decode(small), None, "{small:#x}");
    }
}

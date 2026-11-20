use super::*;
use cratonvm_types::field_layout::{CompactLayout, FieldStorageKind};

/// Class ids no other gc test registers (layouts are process-wide).
const MEMO_CLASS_A: u32 = 4291;
const MEMO_CLASS_B: u32 = 4292;

fn layout(total_size: u32) -> CompactLayout {
    CompactLayout {
        field_disps: vec![8, 16, 24],
        is_ref: vec![true, false, true],
        field_kinds: vec![
            FieldStorageKind::Reference,
            FieldStorageKind::Int,
            FieldStorageKind::Reference,
        ],
        ref_disps: vec![8, 24],
        total_size,
    }
}

fn header(r: ObjectRef) -> &'static ObjectHeader {
    // SAFETY: test objects stay allocated (no collection runs) for the
    // whole test, and the reference is only read.
    unsafe { &*(r.as_ptr() as *const ObjectHeader) }
}

/// The memo answers exactly what `object_total_size` answers, for compact
/// objects of two classes, legacy objects, primitive and reference
/// arrays, on the first (miss) and every later (hit) ask, in every order.
#[test]
fn r23_w8_walk_size_memo_answers_object_total_size_exactly() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..G1CollectorConfig::default()
    });
    for (id, total) in [(MEMO_CLASS_A, 32u32), (MEMO_CLASS_B, 32u32)] {
        cratonvm_types::register_class_layout(
            gc.layout_domain(),
            id,
            std::sync::Arc::new(layout(total)),
        );
    }
    let mut objs: Vec<ObjectRef> = Vec::new();
    for i in 0..64 {
        objs.push(gc.try_alloc_object(ClassId::new(MEMO_CLASS_A), 3).expect("room"));
        objs.push(gc.try_alloc_object(ClassId::new(MEMO_CLASS_B), 3).expect("room"));
        objs.push(gc.alloc_object(ClassId::new(1), 1 + i % 5));
        objs.push(gc.alloc_array(ClassId::new(0), ArrayElementType::Byte, 3 + i));
        objs.push(gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 1 + i % 7));
    }
    assert!(
        objs.iter().any(|&o| cratonvm_types::is_compact_object(header(o))),
        "the fixture holds compact objects (else the memo arm is never asked)"
    );
    let mut on = WalkSizeMemo::with_enabled(true);
    let mut off = WalkSizeMemo::with_enabled(false);
    for _ in 0..3 {
        for &o in objs.iter().chain(objs.iter().rev()) {
            let want = object_total_size(header(o));
            assert_eq!(on.size(header(o)), want, "memo ON");
            assert_eq!(off.size(header(o)), want, "memo OFF");
        }
    }
}

/// A layout registration after the memo was filled (the generation moves)
/// is seen: the memo never answers a size the registry no longer gives.
#[test]
fn r23_w8_walk_size_memo_follows_a_layout_generation_change() {
    const MEMO_CLASS_C: u32 = 4293;
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..G1CollectorConfig::default()
    });
    cratonvm_types::register_class_layout(
        gc.layout_domain(),
        MEMO_CLASS_C,
        std::sync::Arc::new(layout(32)),
    );
    let o = gc.try_alloc_object(ClassId::new(MEMO_CLASS_C), 3).expect("room");
    let mut memo = WalkSizeMemo::with_enabled(true);
    let first = memo.size(header(o));
    assert_eq!(first, object_total_size(header(o)));
    // Same class, same field count, a different registered size.
    cratonvm_types::register_class_layout(
        gc.layout_domain(),
        MEMO_CLASS_C,
        std::sync::Arc::new(layout(40)),
    );
    assert_eq!(
        memo.size(header(o)),
        object_total_size(header(o)),
        "after a re-registration the memo answers the registry's new size"
    );
}

/// `CRATONVM_G1_MARK_SIZE_MEMO`: the marker's sized mark credits exactly
/// the bytes `try_mark_and_measure`'s rule credits (`object_total_size`
/// below TAMS, inside the cursor) and marks the bit once, for compact,
/// legacy and array objects, with the memo ON (asked repeatedly, so its
/// hit path answers) and OFF.
#[test]
fn r23_w8_mark_size_memo_credits_exactly_what_the_plain_mark_credits() {
    const MARK_CLASS: u32 = 4294;
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..G1CollectorConfig::default()
    });
    cratonvm_types::register_class_layout(
        gc.layout_domain(),
        MARK_CLASS,
        std::sync::Arc::new(layout(32)),
    );
    let mut objs: Vec<ObjectRef> = Vec::new();
    for i in 0..48usize {
        for _ in 0..2 {
            objs.push(gc.try_alloc_object(ClassId::new(MARK_CLASS), 3).expect("room"));
            objs.push(gc.alloc_object(ClassId::new(1), 1 + i % 4));
            objs.push(gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 2 + i));
        }
    }
    let mut on = WalkSizeMemo::with_enabled(true);
    let mut off = WalkSizeMemo::with_enabled(false);
    let mut credited = 0usize;
    let regions = gc.regions.read();
    for (i, &o) in objs.iter().enumerate() {
        let addr = o.as_ptr() as usize;
        let idx = gc.lookup_region_for_addr(addr).expect("in the heap");
        let r = &regions[idx];
        let rel = addr - r.data.as_ptr() as usize;
        // `try_mark_and_measure`'s rule, spelled out.
        let want = if rel >= r.tams() {
            0
        } else {
            let size = object_total_size(header(o));
            if size >= HEADER_SIZE && rel + size <= r.cursor() {
                size
            } else {
                0
            }
        };
        let memo = if i % 2 == 0 { &mut on } else { &mut off };
        assert_eq!(r.try_mark_and_measure_sized(addr, memo), Some(want), "object {i}");
        assert!(r.mark_bitmap.is_marked(addr), "object {i} is black");
        assert_eq!(r.try_mark_and_measure_sized(addr, memo), None, "object {i}: once");
        credited += want;
    }
    assert!(credited > 0, "the fixture lies below TAMS, so bytes were credited");
}

#[test]
fn r23_w6_survivors_dominate_when_the_pause_copied_half_of_what_it_freed() {
    assert!(survivors_dominated(47 << 20, 49 << 20), "an all-live Eden");
    assert!(survivors_dominated(1, 2));
    assert!(!survivors_dominated(1, 3));
    assert!(!survivors_dominated(50 << 10, 1 << 20), "a collecting Eden");
    assert!(survivors_dominated(0, 0), "an empty pause decides nothing either way");
}

#[test]
fn r23_w6_a_starved_young_pause_is_under_the_reserve_with_a_cycle_open() {
    assert!(starved_young_pause(true, 102, 1024));
    assert!(!starved_young_pause(true, 103, 1024));
    assert!(!starved_young_pause(false, 0, 1024), "no open cycle, never starved");
    assert!(starved_young_pause(true, 6, 64));
    assert!(!starved_young_pause(true, 7, 64));
}


#[test]
fn r23_w6_arming_the_starved_full_arms_a_full_compaction_unless_in_place_is_off() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..G1CollectorConfig::default()
    });
    assert!(gc.arm_starved_full());
    assert!(gc.full_compaction_armed.load(Ordering::Acquire));
    assert!(gc.starved_full_armed.load(Ordering::Acquire));
}


#[test]
fn r23_w8_a_futile_young_pause_is_a_starved_pool_under_an_open_cycle() {
    // 1024 regions: under 3% is 30 or fewer Free.
    assert!(futile_young_pause(true, 17, 1024));
    assert!(futile_young_pause(true, 30, 1024));
    assert!(!futile_young_pause(true, 31, 1024));
    assert!(!futile_young_pause(false, 0, 1024), "no open cycle, never futile");
    // -Xmx64m, 1 MB regions: only a Free pool of 0 or 1 counts.
    assert!(futile_young_pause(true, 1, 64));
    assert!(!futile_young_pause(true, 2, 64));
}


fn escalation_streak_body(gc: &G1Collector) {
    // Start from a clean streak (the default-arm probe above filled it).
    gc.note_futile_young_pause_with(true, 500, 1024);
    let on = true;
    for _ in 0..FUTILE_YOUNG_STREAK - 1 {
        gc.note_futile_young_pause_with(true, 0, 1024);
    }
    assert!(!gc.take_futile_young_escalation_with(true), "one short of the streak");
    // A healthy pause resets it.
    gc.note_futile_young_pause_with(true, 500, 1024);
    for _ in 0..FUTILE_YOUNG_STREAK - 1 {
        gc.note_futile_young_pause_with(true, 0, 1024);
    }
    assert!(!gc.take_futile_young_escalation_with(true), "the reset restarted the count");
    gc.note_futile_young_pause_with(true, 0, 1024);
    assert!(
        !gc.take_futile_young_escalation_with(false),
        "never without an open cycle"
    );
    assert_eq!(gc.take_futile_young_escalation_with(true), on, "a full streak escalates");
    assert!(!gc.take_futile_young_escalation_with(true), "and is consumed");
    assert!(gc.futile_young_escalation_count() >= u64::from(on));
}

use super::{
    fragmented_pinned_compaction_cadence_due, fragmented_pinned_compaction_due,
    fragmented_pinned_compaction_pin_budget_ok,
};

#[test]
fn a_pending_full_heap_refusal_does_not_admit_fragmentation_compaction() {
    let mut old = crate::old_gen::OldGen::new(64 * 1024);
    assert!(old.alloc(60 * 1024, 8).is_some());
    assert!(old.note_humongous_refusal(16 * 1024));
    let request = old.fragmentation_compaction_request_bytes();
    assert!(old.take_fragmentation_compaction_request());
    let reactive = old.fragmented_for(request);
    assert!(!reactive);
    assert!(!old.fragmentation_repair_due());
    assert!(!fragmented_pinned_compaction_due(
        old.fragmentation_repair_due(),
        reactive,
        4,
        None
    ));
    assert!(fragmented_pinned_compaction_due(true, false, 4, Some(0)));
    assert!(!fragmented_pinned_compaction_due(true, false, 5, Some(4)));
}

#[test]
fn a_small_blocked_request_keeps_its_own_scale_through_admission() {
    let mut old = crate::old_gen::OldGen::new(1024 * 1024);
    assert!(!old.fragmented_for(usize::MAX));
    assert!(old.alloc(512 * 1024, 8).is_some());
    let blocks: Vec<_> = (0..512).map(|_| old.alloc(1024, 8).unwrap()).collect();
    for (i, block) in blocks.into_iter().enumerate() {
        if i % 2 == 0 {
            // SAFETY: one complete allocated block, freed exactly once.
            unsafe { old.free(block, 1024) };
        }
    }
    old.coalesce_free_blocks();
    assert!(
        !old.fragmentation_repair_due(),
        "the 512 KiB historical scale is too large"
    );
    assert!(old.alloc(4096, 8).is_none());
    assert!(old.arm_fragmentation_compaction());
    let request = old.fragmentation_compaction_request_bytes();
    assert_eq!(request, 4096);
    assert!(old.take_fragmentation_compaction_request());
    assert_eq!(old.fragmentation_compaction_request_bytes(), 0);
    let reactive = old.fragmented_for(request);
    assert!(reactive);
    assert!(fragmented_pinned_compaction_due(false, reactive, 4, None));
    assert!(fragmented_pinned_compaction_due(
        false,
        reactive,
        5,
        Some(4)
    ));
}

#[test]
fn fragmented_pinned_compaction_is_capped_to_one_in_four_majors() {
    assert!(fragmented_pinned_compaction_cadence_due(0, None));
    assert!(!fragmented_pinned_compaction_cadence_due(1, Some(0)));
    assert!(!fragmented_pinned_compaction_cadence_due(2, Some(0)));
    assert!(!fragmented_pinned_compaction_cadence_due(3, Some(0)));
    assert!(fragmented_pinned_compaction_cadence_due(4, Some(0)));
}

#[test]
fn a_currently_refused_fragmented_allocation_bypasses_proactive_cadence() {
    // A refusal names a real request that cannot fit any current block.
    // The ordinary four-collection throttle applies only to proactive
    // repairs, never to this OOM-ladder repair.
    assert!(fragmented_pinned_compaction_due(false, true, 1, Some(0)));
    assert!(fragmented_pinned_compaction_due(false, true, 4, Some(3)));
    assert!(!fragmented_pinned_compaction_due(true, false, 3, Some(0)));
    assert!(fragmented_pinned_compaction_due(true, false, 4, Some(0)));
}

#[test]
fn fragmentation_cadence_admits_the_first_request_at_any_phase_and_handles_wrap() {
    assert!(fragmented_pinned_compaction_cadence_due(3, None));
    assert!(!fragmented_pinned_compaction_cadence_due(4, Some(3)));
    assert!(!fragmented_pinned_compaction_cadence_due(6, Some(3)));
    assert!(fragmented_pinned_compaction_cadence_due(7, Some(3)));
    assert!(fragmented_pinned_compaction_cadence_due(
        2,
        Some(u64::MAX - 1)
    ));
}

#[test]
fn fragmented_pinned_compaction_refuses_ten_percent_and_empty_marks() {
    assert!(fragmented_pinned_compaction_pin_budget_ok(9, 100));
    assert!(!fragmented_pinned_compaction_pin_budget_ok(10, 100));
    assert!(!fragmented_pinned_compaction_pin_budget_ok(0, 0));
    assert!(!fragmented_pinned_compaction_pin_budget_ok(usize::MAX, 1));
}

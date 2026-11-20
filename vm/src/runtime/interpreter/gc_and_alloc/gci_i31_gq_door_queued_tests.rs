use super::*;

#[test]
fn gci_i31_gq_only_full_collection_doors_carry() {
    for door in [
        GcDoor::SystemGc,
        GcDoor::MetadataThreshold,
        GcDoor::DiagnosticCommand,
        GcDoor::HeapInspection,
        GcDoor::HeapDump,
        GcDoor::JvmtiForceGc,
    ] {
        assert!(g1_full_collection_door(door), "{door:?}");
    }
    for door in [
        GcDoor::AllocationThreshold,
        GcDoor::AllocationFailure,
        GcDoor::G1CompactionPause,
        GcDoor::G1HumongousAllocation,
    ] {
        assert!(!g1_full_collection_door(door), "{door:?}");
    }
}

#[test]
fn gci_i31_gq_only_entries_the_queue_still_holds_are_held_out() {
    assert_eq!(
        g1_door_queued_still_queued(
            vec![0x300, 0, 0x100, 0x200, 0x100],
            vec![0x200, 0x100, 0x900]
        ),
        vec![0x100, 0x200]
    );
    assert!(g1_door_queued_still_queued(vec![0x100], Vec::new()).is_empty());
}

#[test]
fn gci_i31_gq_the_hold_out_touches_only_the_queue_section() {
    // SAFETY: test words, never dereferenced.
    let r = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    let words = |v: &[ObjectRef]| {
        v.iter()
            .map(|o| o.as_ptr() as usize)
            .collect::<Vec<usize>>()
    };
    // A thread root names 0x100 too: it stays; the queue's 0x100 goes.
    let mut roots = vec![r(0x100), r(0x500), r(0x100), r(0x200), r(0x300)];
    g1_hold_out_mark_roots(&mut roots, 2, &[0x100, 0x300]);
    assert_eq!(words(&roots), vec![0x100, 0x500, 0x200]);
    let mut same = vec![r(0x100), r(0x200)];
    g1_hold_out_mark_roots(&mut same, 2, &[0x100]);
    assert_eq!(words(&same), vec![0x100, 0x200], "an empty queue section");
    g1_hold_out_mark_roots(&mut same, 0, &[]);
    assert_eq!(words(&same), vec![0x100, 0x200], "nothing held out");
}

/// Source pins: the door carries after its enqueue; the full mark takes
/// the carry inside its pause, holds the queue's section out right after
/// appending it, and its callback marks the closure before the reference
/// pass and unstages after it; the round takes the staged set under the
/// processor lock before anything reads `resurrected`.
#[test]
fn gci_i31_gq_the_steps_are_wired_in_order() {
    let src = GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let body = |start: &str| -> String {
        let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
        let rest = &src[at..];
        rest[..rest.find("\n\u{7d}\n").expect("end")].to_string()
    };
    let pos = |b: &str, needle: &str| b.find(needle).unwrap_or_else(|| panic!("{needle}"));
    let door = body("fn run_collection_pause(");
    assert!(
        pos(
            &door,
            "enqueue_resurrected_finalizers(shared, &dead_finalizers);"
        ) < pos(
            &door,
            "g1_carry_door_queued_finalizables(shared, door, &dead_finalizers);"
        )
    );
    let stw = body("fn g1_stw_full_mark_cycle(");
    let take = pos(
        &stw,
        "let door_queued = g1_take_door_queued_held_out(shared);",
    );
    let from = pos(&stw, "let fin_from = snapshot_roots.len();");
    let append = pos(
        &stw,
        "append_pending_finalizer_mark_roots(shared, &mut snapshot_roots);",
    );
    let hold = pos(
        &stw,
        "g1_hold_out_mark_roots(&mut snapshot_roots, fin_from, &door_queued);",
    );
    let all = pos(&stw, "let all_roots");
    let held = pos(
        &stw,
        "g1_remark_hold_out_door_queued(shared, is_live, &door_queued);",
    );
    let pass = pos(&stw, "g1_remark_reference_pass(shared, is_live)");
    let unstage = pos(&stw, "g1_remark_unstage_door_queued(shared, &door_queued);");
    assert!(take < from && from < append && append < hold && hold < all);
    assert!(all < held && held < pass && pass < unstage);
    let hold_fn = body("fn g1_remark_hold_out_door_queued(");
    let closure = pos(&hold_fn, "g1_remark_reference_closure(shared, is_live,");
    let mark = pos(&hold_fn, "g1_remark_mark_kept_closure(shared, held_out);");
    let stage = pos(&hold_fn, ".stage_remark_resurrected(closure);");
    assert!(closure < mark && mark < stage);
    let rows = body("fn remark_reference_rows(");
    let lock = pos(&rows, "let mut ref_proc = shared.mem.ref_processor.lock();");
    let staged = pos(
        &rows,
        "let staged = ref_proc.take_remark_staged_resurrected();",
    );
    let first_use = pos(&rows, "resurrected.is_empty()");
    assert!(lock < staged && staged < first_use);
}

/// zt t31c source pins: the carry holds what it carried back from the
/// delivery thread under its switch, inside the pause that queued it; the
/// full mark ends the hold as it takes the carry, before it reads the queue;
/// the door arms its end-of-door release only once it collected.
#[test]
fn zt_t31c_the_carry_holds_the_queue_until_the_full_mark_takes_it() {
    let src = GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let body = |start: &str| -> String {
        let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
        let rest = &src[at..];
        rest[..rest.find("\n\u{7d}\n").expect("end")].to_string()
    };
    let pos = |b: &str, needle: &str| b.find(needle).unwrap_or_else(|| panic!("{needle}"));
    let carry = body("fn g1_carry_door_queued_finalizables(");
    let guard = pos(&carry, "if !queued.is_empty()");
    let hold = pos(&carry, ".hold_until_judged(queued);");
    assert!(pos(&carry, ".carry_door_queued_finalizables(queued, stamp);") < hold);
    assert!(guard < hold);
    let take = body("fn g1_take_door_queued_held_out(");
    let release = pos(&take, "finalizer_thread.release_held();");
    assert!(release < pos(&take, "if !shared.mem.heap.is_g1()"));
    assert!(release < pos(&take, ".pending_addresses()"));
    let door = body("fn force_full_collection(");
    let armed = pos(&door, "held.armed = collected;");
    assert!(pos(&door, "let mut held = G1DoorHeldFinalizables {") < armed);
    assert!(pos(&door, "run_collection_pause(shared, thread, door)") < armed);
}

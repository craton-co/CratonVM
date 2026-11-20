use super::*;

fn plain_entry(sp: usize) -> JitFrameChainEntry {
    JitFrameChainEntry {
        entry_sp: sp,
        precise: None,
        interp_depth: 3,
        depth_stripe: cratonvm_types::striped_counter::StripeToken::UNSET,
        exec_token: cratonvm_jit::JitExecutionToken::UNSET,
        tally_skipped: false,
    }
}

fn history() -> (u64, usize) {
    JIT_CHAIN_HISTORY.with(|c| {
        let h = c.get();
        (h.mutations, h.residue_hi)
    })
}

/// Run one push and pop in each form (on a fresh thread each, so the two
/// start from the same history) and compare what they leave behind.
#[test]
fn t1s_door_push_pop_leaves_what_the_ordinary_form_leaves() {
    let run = |door: bool| {
        std::thread::spawn(move || {
            let sp = current_stack_pointer();
            let (m0, r0) = history();
            let depth0 = current_thread_jit_depth();
            let qd0 = cratonvm_gc::gc_quiescence::depth();
            let d = push_entry_impl(plain_entry(sp), door);
            assert_eq!(d, depth0 + 1);
            let (m1, _) = history();
            assert!(cratonvm_gc::gc_quiescence::depth() >= 1);
            assert_eq!(pop_jit_entry_impl(door), Some(sp));
            let (m2, r2) = history();
            assert_eq!(current_thread_jit_depth(), depth0);
            let _ = qd0;
            (m1 - m0, m2 - m0, r0, r2 >= sp)
        })
        .join()
        .unwrap()
    };
    let ordinary = run(false);
    let door = run(true);
    assert_eq!(ordinary.0, door.0, "one mutation per push");
    assert_eq!(ordinary.1, door.1, "one mutation per push and per pop");
    assert!(ordinary.3 && door.3, "the pop raises the residue mark to its entry");
}

/// The guard's drop pops through the form it was built with, and a nested
/// door entry inside an ordinary one unwinds to the ordinary depth.
#[test]
fn t1s_mixed_forms_nest() {
    std::thread::spawn(|| {
        let before = current_thread_jit_depth();
        let outer = push_jit_entry();
        let sp = current_stack_pointer();
        assert_eq!(push_entry_impl(plain_entry(sp), true), outer + 1);
        assert_eq!(pop_jit_entry_impl(true), Some(sp));
        assert_eq!(current_thread_jit_depth(), outer);
        assert!(pop_jit_entry().is_some());
        assert_eq!(current_thread_jit_depth(), before);
    })
    .join()
    .unwrap();
}

/// t1 wave 9, lane JITA (`t1-I25-door-form-quiescence-tallies-drift-on-
/// mixed-pops`): the entry records which tally form its push took, whatever
/// a constructor passed, and every way it leaves the chain -- the other
/// form's pop, a prune -- reads that record. The tallies themselves are
/// process-wide and moved by concurrent tests, so the record is what is
/// checked here.
#[test]
fn t1jita_entry_records_its_tally_form() {
    std::thread::spawn(|| {
        let top_skipped =
            || JIT_ENTRY_CHAIN.with(|c| c.borrow().last().map(|e| e.tally_skipped));
        let before = current_thread_jit_depth();
        let sp = current_stack_pointer();
        let mut forged = plain_entry(sp);
        forged.tally_skipped = true;
        assert_eq!(push_entry_impl(forged, false), before + 1);
        assert_eq!(top_skipped(), Some(false), "an ordinary push counts");
        assert_eq!(push_entry_impl(plain_entry(sp), true), before + 2);
        assert_eq!(top_skipped(), Some(true), "a door push skips the tally");
        // The other form pops the door entry, then the ordinary one.
        assert_eq!(pop_jit_entry_impl(false), Some(sp));
        assert_eq!(top_skipped(), Some(false));
        assert_eq!(pop_jit_entry_impl(true), Some(sp));
        assert_eq!(current_thread_jit_depth(), before);
        // A returned door entry reclaimed by a prune.
        let low = sp - 4096;
        assert_eq!(push_entry_impl(plain_entry(low), true), before + 1);
        assert_eq!(top_skipped(), Some(true));
        assert_eq!(prune_returned_jit_entries(sp), 1);
        assert_eq!(current_thread_jit_depth(), before);
    })
    .join()
    .unwrap();
}

/// The door's floor variant records the depth it is handed, and reads the
/// stash itself when handed none.
#[test]
fn t1s_floor_at_records_the_given_depth() {
    std::thread::spawn(|| {
        if !local_handler_drops_orphans() {
            assert_eq!(enter_exc_stash_floor_at(Some(5)), None);
            return;
        }
        let prev = enter_exc_stash_floor_at(Some(5)).expect("recorded");
        assert_eq!(recorded_exc_stash_floor(), Some(5));
        let inner = enter_exc_stash_floor_at(None).expect("recorded");
        assert_eq!(inner, 5);
        assert_eq!(
            recorded_exc_stash_floor(),
            Some(cratonvm_jit::deopt::exceptional_stash_depth())
        );
        restore_exc_stash_floor(inner);
        restore_exc_stash_floor(prev);
        assert_eq!(recorded_exc_stash_floor(), None);
    })
    .join()
    .unwrap();
}

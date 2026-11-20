use super::*;
use cratonvm_jit::deopt::{
    exceptional_stash_depth, restash_exceptional_frame_with_point,
    truncate_exceptional_stash_to, ReconstructedFrame,
};

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

/// Publish `n` exceptional frames, as `n` compiled activations unwinding out
/// of a door's span do.
fn publish(n: usize) {
    for _ in 0..n {
        restash_exceptional_frame_with_point(ReconstructedFrame::default(), None, 0);
    }
}

/// A guard as `enter` builds it (chain entry, then the stash depth recorded
/// as the floor), with the entry at an `sp` the test chooses.
fn guard_at(sp: usize) -> JitEntryGuard {
    let depth_at_push = push_entry_impl(plain_entry(sp), true);
    JitEntryGuard {
        depth_at_push,
        active_class_id: None,
        saved_native_unwind: 0,
        saved_exc_floor: enter_exc_stash_floor_at(None),
        door: true,
    }
}

/// What a compiled catch does at its commit point
/// (`drop_orphaned_exceptional_frames_at_compiled_catch`, without the
/// releases): truncate to the innermost live door's floor, or nothing.
fn catch_truncation() -> usize {
    exceptional_stash_floor().map_or(0, truncate_exceptional_stash_to)
}

/// Nested doors: the innermost door's floor is the one a catch reads, each
/// catch drops only what its own span published, a frame an outer sink
/// stashed before any door survives every catch, and each door hands its
/// enclosing floor back as it closes.
#[test]
fn stash20_nested_doors_truncate_only_their_own_span_and_hand_floors_back() {
    std::thread::spawn(|| {
        if !local_handler_drops_orphans() {
            return;
        }
        let sp = current_stack_pointer();
        assert_eq!(exceptional_stash_floor(), None, "no door, no floor");
        publish(2); // an outer sink's frames, below every door
        assert_eq!(catch_truncation(), 0, "a catch with no door drops nothing");
        assert_eq!(exceptional_stash_depth(), 2);

        let outer = guard_at(sp);
        assert_eq!(exceptional_stash_floor(), Some(2));
        publish(3); // callees of the outer door's body, unwound into it
        let inner = guard_at(sp);
        assert_eq!(
            exceptional_stash_floor(),
            Some(5),
            "the innermost door wins"
        );
        publish(1);
        assert_eq!(
            catch_truncation(),
            1,
            "the inner catch drops the inner span only"
        );
        assert_eq!(exceptional_stash_depth(), 5);

        drop(inner);
        assert_eq!(
            exceptional_stash_floor(),
            Some(2),
            "the outer floor is back"
        );
        assert_eq!(
            catch_truncation(),
            3,
            "the outer catch drops the outer span"
        );
        assert_eq!(exceptional_stash_depth(), 2, "the pre-door frames survive");

        drop(outer);
        assert_eq!(exceptional_stash_floor(), None, "no door again");
        assert_eq!(catch_truncation(), 0);
        assert_eq!(truncate_exceptional_stash_to(0), 2, "test cleanup");
    })
    .join()
    .expect("test thread");
}

/// A prune can reclaim a live guard's chain entry before the guard drops
/// (`t1intb_guard_drop_after_prune_leaves_the_enclosing_entry`). The guard
/// must still hand the enclosing door its floor back: the restore does not
/// ride on the entry being popped.
#[test]
fn stash20_a_guard_whose_entry_was_pruned_still_hands_its_floor_back() {
    std::thread::spawn(|| {
        if !local_handler_drops_orphans() {
            return;
        }
        let sp = current_stack_pointer();
        publish(1);
        let outer = guard_at(sp);
        let outer_depth = current_thread_jit_depth();
        publish(2);
        // An entry whose frame "returned": below the scanner SP.
        let inner = guard_at(sp - 4096);
        assert_eq!(exceptional_stash_floor(), Some(3));
        assert_eq!(
            prune_returned_jit_entries(sp),
            1,
            "only the inner entry went"
        );
        assert_eq!(current_thread_jit_depth(), outer_depth);
        drop(inner);
        assert_eq!(
            current_thread_jit_depth(),
            outer_depth,
            "the enclosing entry is still on the chain"
        );
        assert_eq!(
            exceptional_stash_floor(),
            Some(1),
            "the enclosing door's floor is back although the entry was already gone"
        );
        drop(outer);
        assert_eq!(exceptional_stash_floor(), None);
        assert_eq!(truncate_exceptional_stash_to(0), 3, "test cleanup");
    })
    .join()
    .expect("test thread");
}

/// A continuation slice runs with no door of the carrier's: its chain
/// segment sets the floor aside (a catch in the slice's own compiled code
/// before any slice door sees none), a slice door records its own, and the
/// carrier's floor is back when the segment closes. Pairs with
/// `t1e2_stashes_set_aside_and_restore`, which sets the stash aside at the
/// same point (`CarrierCompiledBase::enter`).
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[test]
fn stash20_a_chain_segment_sets_the_floor_aside_and_gives_it_back() {
    std::thread::spawn(|| {
        let carrier_floor = record_exc_stash_floor(7).expect("tls alive");
        assert_eq!(recorded_exc_stash_floor(), Some(7));
        let base = (current_stack_pointer() + 2048) & !7;
        let segment = ChainSegment::enter(base, None);
        assert_eq!(
            recorded_exc_stash_floor(),
            None,
            "the slice starts with no floor"
        );
        assert_eq!(exceptional_stash_floor(), None);
        if local_handler_drops_orphans() {
            let slice_door = guard_at(base - 1024);
            assert_eq!(exceptional_stash_floor(), Some(exceptional_stash_depth()));
            drop(slice_door);
            assert_eq!(
                recorded_exc_stash_floor(),
                None,
                "the slice door hands back 'none'"
            );
        }
        drop(segment);
        assert_eq!(
            recorded_exc_stash_floor(),
            Some(7),
            "the carrier's floor is back"
        );
        restore_exc_stash_floor(carrier_floor);
    })
    .join()
    .expect("test thread");
}

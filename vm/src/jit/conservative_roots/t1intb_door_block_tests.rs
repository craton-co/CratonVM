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

fn chain_cell_addr() -> usize {
    JIT_ENTRY_CHAIN.with(|c| c as *const RefCell<Vec<JitFrameChainEntry>> as usize)
}

/// The five records the stage-4 refactor folded are fields of the one
/// thread-local, and the chain cell address a peer reads
/// (`current_thread_jit_cells`, published once) is the block's field and
/// survives chain growth, pushes and pops.
#[test]
fn t1intb_records_share_one_block_and_the_published_chain_address_is_stable() {
    std::thread::spawn(|| {
        let (base, len) = DOOR_BLOCK
            .with(|b| (b as *const DoorBlock as usize, std::mem::size_of::<DoorBlock>()));
        let inside = |p: usize| p >= base && p < base + len;
        let chain = chain_cell_addr();
        assert!(inside(chain), "chain");
        assert!(inside(JIT_CHAIN_HISTORY.with(|c| c.as_ptr() as usize)), "history");
        assert!(inside(TOP_RBP.with(|c| c.as_ptr() as usize)), "top_rbp");
        assert!(inside(EXC_STASH_FLOOR.with(|c| c.as_ptr() as usize)), "exc_floor");
        assert!(inside(SELF_JIT_DEPTH_SLOT.with(|c| c.as_ptr() as usize)), "depth_slot");

        let published = current_thread_jit_cells();
        assert_eq!(published.chain, chain, "peers read the block's chain cell");

        // Grow the chain well past any initial capacity (the Vec buffer
        // moves; the cell must not), then drain it.
        let before = current_thread_jit_depth();
        let sp = current_stack_pointer();
        for _ in 0..64 {
            push_entry_impl(plain_entry(sp), false);
        }
        assert_eq!(current_thread_jit_depth(), before + 64);
        assert_eq!(chain_cell_addr(), chain);
        assert_eq!(current_thread_jit_cells().chain, published.chain);
        for _ in 0..64 {
            assert_eq!(pop_jit_entry_impl(false), Some(sp));
        }
        assert_eq!(current_thread_jit_depth(), before);
        assert_eq!(current_thread_jit_cells().chain, published.chain);
    })
    .join()
    .unwrap();
}

/// A door-form guard's drop pops its own entry and every abandoned
/// descendant (of either form), one mutation per pop, and leaves the chain
/// where it found it.
#[test]
fn t1intb_door_guard_drop_pops_abandoned_descendants_of_both_forms() {
    std::thread::spawn(|| {
        let before = current_thread_jit_depth();
        let sp = current_stack_pointer();
        let depth_at_push = push_entry_form(plain_entry(sp), true, false);
        assert_eq!(depth_at_push, before + 1);
        let guard = JitEntryGuard {
            depth_at_push,
            active_class_id: None,
            saved_native_unwind: 0,
            saved_exc_floor: None,
            door: true,
        };
        push_entry_impl(plain_entry(sp), false);
        push_entry_impl(plain_entry(sp), true);
        assert_eq!(current_thread_jit_depth(), before + 3);
        let (m0, _) = history();
        drop(guard);
        assert_eq!(current_thread_jit_depth(), before, "all three entries are gone");
        let (m1, r1) = history();
        assert_eq!(m1 - m0, 3, "one mutation per pop");
        assert!(r1 >= sp, "the pops raise the residue mark");
    })
    .join()
    .unwrap();
}

/// The guard's drop does not pop an enclosing guard's entry when a prune
/// already reclaimed its own.
#[test]
fn t1intb_guard_drop_after_prune_leaves_the_enclosing_entry() {
    std::thread::spawn(|| {
        let before = current_thread_jit_depth();
        let sp = current_stack_pointer();
        let outer_depth = push_entry_impl(plain_entry(sp), false);
        assert_eq!(outer_depth, before + 1);
        // An entry whose frame "returned": below the scanner SP.
        let inner_depth = push_entry_impl(plain_entry(sp - 4096), true);
        let guard = JitEntryGuard {
            depth_at_push: inner_depth,
            active_class_id: None,
            saved_native_unwind: 0,
            saved_exc_floor: None,
            door: true,
        };
        assert_eq!(prune_returned_jit_entries(sp), 1);
        assert_eq!(current_thread_jit_depth(), outer_depth);
        drop(guard);
        assert_eq!(
            current_thread_jit_depth(),
            outer_depth,
            "the enclosing entry is still on the chain"
        );
        assert!(pop_jit_entry().is_some());
        assert_eq!(current_thread_jit_depth(), before);
    })
    .join()
    .unwrap();
}

/// One guard suspends the native-allocation unwind permission and records
/// the exceptional-stash floor on entry, and gives both back on drop.
#[test]
fn t1intb_guard_restores_unwind_permission_and_stash_floor_together() {
    std::thread::spawn(|| {
        let outer = crate::runtime::native_oom::enter_native_call();
        assert!(crate::runtime::native_oom::unwind_ok());
        let kept = local_handler_drops_orphans();
        let prev_floor = kept.then(|| record_exc_stash_floor(1_000_003)).flatten();
        {
            let _guard = JitEntryGuard::enter();
            assert!(
                !crate::runtime::native_oom::unwind_ok(),
                "a compiled frame suspends the permission"
            );
            if kept {
                assert_eq!(
                    recorded_exc_stash_floor(),
                    Some(cratonvm_jit::deopt::exceptional_stash_depth()),
                    "the guard publishes the stash depth as its floor"
                );
            }
        }
        assert!(
            crate::runtime::native_oom::unwind_ok(),
            "the permission is back after the guard"
        );
        if kept {
            assert_eq!(
                recorded_exc_stash_floor(),
                Some(1_000_003),
                "the enclosing floor is back"
            );
            if let Some(prev) = prev_floor {
                restore_exc_stash_floor(prev);
            }
        }
        crate::runtime::native_oom::restore(outer);
    })
    .join()
    .unwrap();
}

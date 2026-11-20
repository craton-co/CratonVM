use super::*;
use cratonvm_types::striped_counter::{StripeToken, DOOR_DEPTH};

fn plain_entry(sp: usize) -> JitFrameChainEntry {
    JitFrameChainEntry {
        entry_sp: sp,
        precise: None,
        interp_depth: 3,
        depth_stripe: StripeToken::UNSET,
        exec_token: cratonvm_jit::JitExecutionToken::UNSET,
        tally_skipped: false,
    }
}

fn top_counts() -> Option<(StripeToken, bool)> {
    JIT_ENTRY_CHAIN.with(|c| {
        c.borrow()
            .last()
            .map(|e| (e.depth_stripe, e.exec_token.counts_door_depth()))
    })
}

#[test]
fn t1i25_one_counter_entries_count_once_and_settle_on_every_path() {
    std::thread::spawn(|| {
        let door0 = DOOR_DEPTH.get();
        let before = current_thread_jit_depth();
        let sp = current_stack_pointer();

        // Its own form's push and pop.
        assert_eq!(push_entry_form(plain_entry(sp), true, true), before + 1);
        assert_eq!(top_counts(), Some((StripeToken::UNSET, true)));
        assert_eq!(DOOR_DEPTH.get(), door0 + 1, "one count for the entry");
        assert!(any_thread_in_jit(), "GLOBAL_JIT_DEPTH folds it in");
        assert!(global_jit_depth_raw() >= 1);
        assert!(
            cratonvm_gc::gc_quiescence::is_active(),
            "the collector's quiescence depth folds it in"
        );
        assert!(cratonvm_gc::gc_quiescence::depth() >= 1);
        assert_eq!(pop_jit_entry_impl(true), Some(sp));
        assert_eq!(DOOR_DEPTH.get(), door0);
        assert_eq!(current_thread_jit_depth(), before);

        // Popped by the ordinary form (a guard's descendant loop).
        push_entry_form(plain_entry(sp), true, true);
        assert_eq!(DOOR_DEPTH.get(), door0 + 1);
        assert_eq!(pop_jit_entry_impl(false), Some(sp));
        assert_eq!(DOOR_DEPTH.get(), door0, "settled by the entry, not the form");

        // An entry of the other forms never touches the door counter.
        push_entry_impl(plain_entry(sp), false);
        assert!(!top_counts().expect("pushed").1);
        assert_eq!(DOOR_DEPTH.get(), door0);
        assert_eq!(pop_jit_entry_impl(true), Some(sp));
        push_entry_impl(plain_entry(sp), true);
        assert_eq!(DOOR_DEPTH.get(), door0);
        assert_eq!(pop_jit_entry_impl(true), Some(sp));

        // A prune of a returned one-counter entry settles it too.
        let returned_sp = sp - 4096;
        push_entry_form(plain_entry(returned_sp), true, true);
        assert_eq!(DOOR_DEPTH.get(), door0 + 1);
        assert_eq!(prune_returned_jit_entries(sp), 1);
        assert_eq!(DOOR_DEPTH.get(), door0);
        assert_eq!(current_thread_jit_depth(), before);
    })
    .join()
    .unwrap();
}

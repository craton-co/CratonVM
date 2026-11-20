use super::*;

/// A take-over records the registers of every peer it froze, in freeze
/// order; one that froze nobody records none.
#[test]
fn gcj2jt2_frozen_register_words_accumulate_per_peer() {
    let mut taken = TakenOver::default();
    assert!(taken.frozen_register_words().is_empty());
    taken.note_frozen_registers(&[1, 2, 3]);
    taken.note_frozen_registers(&[4]);
    assert_eq!(taken.frozen_register_words(), &[1, 2, 3, 4]);
}

/// The resume completes nothing while the heap's JIT has compiled no
/// post-only site, then dirties the card of each frozen register word in
/// the old generation (and only those) once one exists; a take-over that
/// froze nobody completes nothing. G1 completes nothing either.
#[test]
fn gcj2jt2_resume_completes_the_post_only_marks_of_the_frozen_registers() {
    use crate::memory::{GcBackend, VmHeap};
    const MB: usize = 1024 * 1024;
    let heap = VmHeap::new(GcBackend::Generational, 16 * MB);
    let (view, old_base, old_end) = heap
        .jit_card_table_info()
        .expect("a generational card view");
    assert!(old_base + 64 < old_end);
    let frozen = |words: &[usize]| {
        let mut t = TakenOver::default();
        t.note_frozen_registers(words);
        t
    };
    let words = [old_base + 64, 0, 0x1000, old_end];
    assert_eq!(
        resume_completing_post_only_card_marks(frozen(&words), &heap),
        0,
        "no post-only site: nothing owed"
    );
    // The compiler's bump at a post-only site.
    // SAFETY: `view` is the live table's boxed `JitCardView`, which lives
    // as long as `heap`.
    let v = unsafe { &*(view as *const cratonvm_gc::card_table::JitCardView) };
    v.post_only_sites.fetch_add(1, Ordering::Release);
    assert_eq!(
        resume_completing_post_only_card_marks(frozen(&words), &heap),
        1,
        "the one old-generation word"
    );
    assert_eq!(
        resume_completing_post_only_card_marks(TakenOver::default(), &heap),
        0,
        "nobody frozen"
    );
    let g1 = VmHeap::new(GcBackend::G1, 64 * MB);
    assert_eq!(resume_completing_post_only_card_marks(frozen(&words), &g1), 0);
}

#[test]
fn credit_demoted_covered_chain_depth_credits_only_complete_readable_demoted() {
    let ledger = std::sync::Arc::new(cratonvm_gc::gc_quiescence::PauseLedger::new());
    let tid = 0x7FFF_EE01u32;
    let slot = cratonvm_gc::gc_quiescence::register_self_jit_depth_slot(tid);
    slot.store(3, Ordering::Release);
    cratonvm_gc::gc_quiescence::with_pause_ledger(&ledger, || {
        cratonvm_gc::gc_quiescence::clear_xt_cycle_pinned_jit_depth();
        credit_demoted_covered_chain_depth(tid, false, true, false);
        assert_eq!(cratonvm_gc::gc_quiescence::xt_cycle_pinned_jit_depth(), 0);
        credit_demoted_covered_chain_depth(tid, true, false, false);
        assert_eq!(cratonvm_gc::gc_quiescence::xt_cycle_pinned_jit_depth(), 0);
        credit_demoted_covered_chain_depth(tid, true, true, true);
        assert_eq!(cratonvm_gc::gc_quiescence::xt_cycle_pinned_jit_depth(), 0);
        credit_demoted_covered_chain_depth(tid, true, true, false);
        assert_eq!(cratonvm_gc::gc_quiescence::xt_cycle_pinned_jit_depth(), 3);
    });
}

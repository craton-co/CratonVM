use super::*;

/// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`): the take-over's
/// completion is a no-op until a post-only site is counted in the card
/// view, then dirties the card of an in-range (old-generation) register
/// word and ignores the rest; G1 never completes anything (it publishes no
/// generational card view, so no post-only site can exist for it).
#[test]
fn gcj2jt2_the_completion_waits_for_a_post_only_site_and_is_generational_only() {
    const MB: usize = 1024 * 1024;
    let heap = VmHeap::new(GcBackend::Generational, 16 * MB);
    let (view, old_base, old_end) = heap
        .jit_card_table_info()
        .expect("a generational card view");
    assert!(old_base + 64 < old_end);
    let word = old_base + 64;
    assert_eq!(heap.jit_post_only_card_sites(), 0);
    assert_eq!(
        heap.takeover_complete_post_only_card_marks(&[word]),
        0,
        "no post-only site compiled yet: nothing owed"
    );
    // The compiler's bump (the JIT writes this word through the view).
    // SAFETY: `view` is the live table's boxed `JitCardView`, which lives
    // as long as `heap`.
    let v = unsafe { &*(view as *const cratonvm_gc_core::card_table::JitCardView) };
    v.post_only_sites
        .fetch_add(1, std::sync::atomic::Ordering::Release);
    assert_eq!(heap.jit_post_only_card_sites(), 1);
    assert_eq!(
        heap.takeover_complete_post_only_card_marks(&[0, word, old_end]),
        1,
        "only the old-generation word"
    );
    let g1 = VmHeap::new(GcBackend::G1, 64 * MB);
    assert_eq!(g1.jit_post_only_card_sites(), 0);
    assert_eq!(g1.takeover_complete_post_only_card_marks(&[word]), 0);
}

use super::nio20w5_identity_hash_inline_tests::{compact, run};
use cratonvm_types::ObjectHeader;
use std::sync::atomic::Ordering;

#[test]
#[cfg(target_arch = "x86_64")]
fn an_inflated_word_that_kept_its_hash_answers_it() {
    for cid in [0u32, 7, 1000, 0x8000_0001, u32::MAX - 1] {
        for mint in [1i32, 0x3FFF, 0x4000, 0xF_FFFF, 0x1234_5678, -5] {
            let h = compact(cid);
            let want = h.identity_hash(|| mint).unwrap();
            let m = h.mark();
            h.mark_word.store(
                ObjectHeader::make_inflated_keeping_hash(m),
                Ordering::Relaxed,
            );
            assert_eq!(
                h.identity_hash(|| 99).unwrap(),
                want,
                "the header decoder answers the kept bits (cid {cid} mint {mint:#x})"
            );
            assert_eq!(run(&h), Some(want), "cid {cid} mint {mint:#x}");
            // The same word without its bits declines (the monitor answers).
            h.mark_word
                .store(ObjectHeader::make_inflated(m), Ordering::Relaxed);
            assert_eq!(run(&h), None, "bare INFLATED declines");
        }
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn a_hash_published_while_inflated_is_read_back_and_thin_locks_decline() {
    let h = compact(42);
    let m = h.mark();
    h.mark_word
        .store(ObjectHeader::make_inflated(m), Ordering::Relaxed);
    let hash = ObjectHeader::short_hash_from_mint(42, 0x0ABC_DEF1);
    assert!(h.publish_inflated_hash(hash));
    assert_eq!(run(&h), Some(hash));
    assert_eq!(h.identity_hash(|| 1).unwrap(), hash);
    // Thin-locked: the payload is an owner, never a hash.
    h.mark_word.store(
        ObjectHeader::make_thin_locked(m, 0x7FF, 7),
        Ordering::Relaxed,
    );
    assert_eq!(run(&h), None);
}

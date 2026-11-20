use super::prevalidated_jit_ref;

/// Only a set bit for that exact index stands for a walk; a clear bit, an
/// index past the mask and a null word all fall back to walking.
#[test]
fn the_mask_answers_only_its_own_indices() {
    let word = 0x1000_0000_i64;
    assert!(prevalidated_jit_ref(0b10, 1, word).is_some());
    assert!(prevalidated_jit_ref(0b10, 0, word).is_none());
    assert!(prevalidated_jit_ref(0, 1, word).is_none());
    assert!(prevalidated_jit_ref(u64::MAX, 64, word).is_none());
    assert!(prevalidated_jit_ref(u64::MAX, 3, 0).is_none());
}

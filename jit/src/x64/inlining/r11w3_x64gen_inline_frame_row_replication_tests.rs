use super::*;

/// A copy the native unroller makes of a loop body carries the body's
/// rows at the shifted return addresses; a row AT `body_start` belongs to
/// the code before the header and is not copied.
#[test]
fn an_unrolled_copy_resolves_the_same_chain_at_its_shifted_return_address() {
    begin_inline_frame_recording();
    if !inline_frame_recording() {
        // `CRATONVM_JIT_NO_INLINE_FRAME_MAP` is set: nothing is recorded.
        let _ = finish_inline_frame_recording(0);
        return;
    }
    push_inline_frame_scope("p/A.f:(I)I".to_string(), 7, None, 3);
    set_inline_frame_scope_pc(5);
    record_inline_frame_row(100, 10); // at body_start: before the header
    record_inline_frame_row(120, 10); // inside the body
    record_inline_frame_row(150, 10); // ends exactly at body_end
    pop_inline_frame_scope();
    replicate_inline_frame_rows(100, 150, 60);
    let map = finish_inline_frame_recording(1000);
    let orig = map.chain_for_native_offset(120).map(|c| c.to_vec());
    assert!(orig.is_some());
    assert_eq!(map.chain_for_native_offset(180).map(|c| c.to_vec()), orig);
    assert_eq!(map.chain_for_native_offset(210).map(|c| c.to_vec()), orig);
    assert!(map.chain_for_native_offset(160).is_none());
    assert_eq!(map.len(), 5);
}

/// A spliced volatile `putstatic` owes one StoreLoad barrier after the
/// store, as the top-level arm emits it; the only `MFENCE` left in the
/// splice walk is the opt-in volatile LOAD fence.
#[test]
fn the_splice_walk_emits_mfence_only_for_the_opt_in_load_fence() {
    let src = include_str!("../inlining.rs");
    let mfence = ["[0x0F, 0xAE, ", "0xF0]"].concat();
    assert_eq!(src.matches(mfence.as_str()).count(), 1);
    let store_fence = ["self.emit_volatile_", "store_fence()"].concat();
    assert_eq!(src.matches(store_fence.as_str()).count(), 1);
}

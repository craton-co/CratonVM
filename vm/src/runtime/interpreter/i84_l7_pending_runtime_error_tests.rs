use super::*;

#[test]
fn the_parked_error_is_one_word_in_the_loop() {
    assert_eq!(
        std::mem::size_of::<PendingRuntimeError>(),
        std::mem::size_of::<usize>()
    );
    // What it replaced, for the record: at least the error itself.
    assert!(
        std::mem::size_of::<Option<(RuntimeError, usize)>>()
            > std::mem::size_of::<PendingRuntimeError>()
    );
}

/// The census cursor is boxed on its first dispatch and then rewritten
/// in place. Non-successor pcs only, so nothing is recorded into the
/// process-wide census another test counts.
#[test]
fn the_pair_census_cursor_is_boxed_on_first_use() {
    let code = [0u8; 4];
    let p = code.as_ptr();
    let mut slot: Option<Box<OpcodePairCursor>> = None;
    note_opcode_pair_in(&mut slot, 3, p, 100, 0x00);
    let first = slot.as_deref().copied().expect("boxed");
    assert_eq!((first.frame_idx, first.next_pc, first.opcode), (3, 101, 0x00));
    note_opcode_pair_in(&mut slot, 3, p, 200, 0x00);
    let second = slot.as_deref().copied().expect("still boxed");
    assert_eq!((second.frame_idx, second.next_pc), (3, 201));
}

#[test]
fn a_parked_error_comes_back_with_its_pc() {
    let parked = park_runtime_error(RuntimeError::aioobe(5, 3), 17);
    let (re, pc) = *parked.expect("parked");
    assert_eq!(pc, 17);
    assert!(matches!(
        re,
        RuntimeError::ArrayIndexOutOfBoundsException { index: 5, .. }
    ));
}

use super::{unsafe_arena_allocate, unsafe_arena_copy_in, unsafe_arena_copy_out};

/// A zero-length copy is a no-op and must succeed at ANY offset in the
/// block, INCLUDING one past the last byte -- that is where a direct
/// `ByteBuffer` sitting at its own limit points. Before the fix,
/// `locate`'s exclusive range test rejected `base + size` and the
/// ByteBuffer natives raised `IllegalStateException: ByteBuffer.put:
/// direct destination write failed`.
#[test]
fn zero_length_copy_at_end_of_block_succeeds() {
    let _arena = super::arena_layout_test_lock();
    let base = unsafe_arena_allocate(16);
    assert!(base > 0, "arena allocate returned {base}");
    let end = base + 16;
    assert!(
        unsafe_arena_copy_in(end, &[]),
        "0-byte write one past the end must succeed"
    );
    let mut empty: [u8; 0] = [];
    assert!(
        unsafe_arena_copy_out(end, &mut empty),
        "0-byte read one past the end must succeed"
    );
    // Interior offsets keep working, and a NON-empty copy past the end is
    // still refused.
    assert!(unsafe_arena_copy_in(base + 8, &[1, 2, 3, 4]));
    let mut got = [0u8; 4];
    assert!(unsafe_arena_copy_out(base + 8, &mut got));
    assert_eq!(got, [1, 2, 3, 4]);
    assert!(
        !unsafe_arena_copy_in(end, &[9]),
        "a 1-byte write past the end must still be refused"
    );
}

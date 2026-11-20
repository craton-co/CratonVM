//! Interpreter round i6 wave 120, lane L1 (i40): the exit index of a
//! reviewed-`Intrinsic` shadow's frame-only report.
use super::last_return_index;

/// The walk steps whole instructions: an operand byte equal to a return
/// opcode is not a return.
#[test]
fn last_return_index_walks_instructions() {
    // `Class.getName`'s shape: aload_0; getfield #0x00b0; astore_1;
    // aload_1; ifnonnull +10; aload_0; invokevirtual #2; goto +4;
    // aload_1; areturn.
    let get_name = [
        0x2a, 0xb4, 0x00, 0xb0, 0x4c, 0x2b, 0xc7, 0x00, 0x0a, 0x2a, 0xb6, 0x00, 0x02, 0xa7,
        0x00, 0x04, 0x2b, 0xb0,
    ];
    assert_eq!(last_return_index(&get_name), Some(17));
    // `return`, then (unreachable) `sipush 0x00ac`: the return at 0.
    assert_eq!(last_return_index(&[0xb1, 0x11, 0x00, 0xac]), Some(0));
    // Two returns: the last one.
    assert_eq!(last_return_index(&[0x03, 0xac, 0x04, 0xac]), Some(3));
    // No return (`athrow`).
    assert_eq!(last_return_index(&[0x01, 0xbf]), None);
}

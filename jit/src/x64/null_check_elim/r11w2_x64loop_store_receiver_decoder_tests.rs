use super::{array_store_receiver_with_starts, bytecode_analysis};

fn decode(code: &[u8], pc: usize) -> Option<(usize, usize, usize)> {
    let starts = bytecode_analysis::instruction_starts(code, code.len());
    array_store_receiver_with_starts(code, pc, &starts)
}

/// The three javac store shapes the load decoder cannot see.
#[test]
fn javac_store_shapes_name_their_receiver() {
    // aload_1; iload_2; iload_3; iastore; return       a[i] = v
    assert_eq!(decode(&[0x2b, 0x1c, 0x1d, 0x4f, 0xb1], 3), Some((1, 1, 2)));
    // aload_1; iconst_0; iconst_5; iastore; return     a[0] = 5
    assert_eq!(decode(&[0x2b, 0x03, 0x08, 0x4f, 0xb1], 3), Some((1, 1, 2)));
    // aload 5; iload 6; aload 7; aastore; return       a[i] = o
    assert_eq!(
        decode(&[0x19, 5, 0x15, 6, 0x19, 7, 0x53, 0xb1], 6),
        Some((5, 2, 4))
    );
    // aload_0; sipush 300; ldc2_w #1; lastore; return  a[300] = 1L
    assert_eq!(
        decode(&[0x2a, 0x11, 0x01, 0x2c, 0x14, 0x00, 0x01, 0x50, 0xb1], 7),
        Some((0, 1, 4))
    );
}

/// A computed value (`a[i] = x + 1`) or index (`a[i + 1] = v`) is not a
/// single push, so the receiver is not three instructions back: refuse.
#[test]
fn computed_operands_are_refused() {
    // aload_1; iload_2; iload_3; iconst_1; iadd; iastore
    assert_eq!(decode(&[0x2b, 0x1c, 0x1d, 0x04, 0x60, 0x4f, 0xb1], 5), None);
    // aload_1; iload_2; iconst_1; iadd; iload_3; iastore
    assert_eq!(decode(&[0x2b, 0x1c, 0x04, 0x60, 0x1d, 0x4f, 0xb1], 5), None);
    // A load opcode is not a store.
    assert_eq!(decode(&[0x2b, 0x1c, 0x2e, 0xb1], 2), None);
    // An `sipush` whose low byte looks like `iconst_0` must not be read as
    // an instruction: aload_1; iload_2; sipush 0x0003; iastore.
    assert_eq!(
        decode(&[0x2b, 0x1c, 0x11, 0x00, 0x03, 0x4f, 0xb1], 5),
        Some((1, 1, 2))
    );
}

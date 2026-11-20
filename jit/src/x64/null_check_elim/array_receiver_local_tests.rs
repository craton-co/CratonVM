// `super::` is the enclosing `null_check_elim` module, where these
// helpers now live.
use super::{array_receiver_local, bytecode_analysis};

// Opcodes used below:
//   0x2A aload_0, 0x19 aload, 0x11 sipush, 0x10 bipush, 0x03 iconst_0,
//   0x32 aaload, 0x53 aastore, 0x2E iaload.

#[test]
fn simple_aload0_iconst0_iaload() {
    // aload_0; iconst_0; iaload
    let code = [0x2Au8, 0x03, 0x2E];
    assert_eq!(array_receiver_local(&code, 2), Some(0));
}

#[test]
fn aload_u8_then_sipush_index() {
    // aload 5; sipush 0x0003; aaload
    // sipush operand low byte is 0x03 (== iconst_0). The OLD backward scan
    // read code[pc-1]=0x03 and mis-decoded it as a single-byte index op,
    // mislocating the aload. The forward-validated decode must still find
    // the real aload at offset 0 (local 5).
    let code = [0x19u8, 0x05, 0x11, 0x00, 0x03, 0x32];
    // pc of aaload = 5.
    assert_eq!(array_receiver_local(&code, 5), Some(5));
}

#[test]
fn misaligned_collision_is_rejected_or_correct() {
    // Construct a method where a backward scan would be fooled but the
    // forward walk disambiguates. bipush index whose operand equals 0x2A
    // (aload_0): aload_1; bipush 0x2A; iastore-style aaload.
    // aload_1 = 0x2B, bipush = 0x10, operand 0x2A, aaload = 0x32.
    let code = [0x2Bu8, 0x10, 0x2A, 0x32];
    // Forward walk: 0:aload_1, 1:bipush(2), 3:aaload. Receiver local = 1.
    assert_eq!(array_receiver_local(&code, 3), Some(1));
}

#[test]
fn no_aload_returns_none() {
    // iconst_1; iconst_0; iaload — no array receiver aload present.
    let code = [0x04u8, 0x03, 0x2E];
    assert_eq!(array_receiver_local(&code, 2), None);
}

#[test]
fn instruction_start_map_basic() {
    // aload_0; sipush 0x0102; aaload
    let code = [0x2Au8, 0x11, 0x01, 0x02, 0x32];
    let starts = bytecode_analysis::instruction_starts(&code, code.len());
    assert_eq!(starts, vec![true, true, false, false, true]);
}

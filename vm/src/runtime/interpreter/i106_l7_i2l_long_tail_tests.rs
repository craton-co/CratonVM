use super::{i2l_long_tail, I2lLongTail};

/// Decode `code` (padded with two zero bytes) with the operation at pc 1
/// (pc 0 is the `i2l`).
fn run(code: &[u8]) -> Option<I2lLongTail> {
    let mut padded = code.to_vec();
    padded.extend([0, 0]);
    i2l_long_tail(&padded, code.len(), 1)
}

fn t(op: u8, store: Option<usize>, next_pc: usize) -> Option<I2lLongTail> {
    Some(I2lLongTail { op, store, next_pc })
}

#[test]
fn javac_tails_decode() {
    // i2l; ladd; lstore_1
    assert_eq!(run(&[0x85, 0x61, 0x40, 0xb1]), t(0x61, Some(1), 3));
    // i2l; lmul; lstore 6
    assert_eq!(run(&[0x85, 0x69, 0x37, 6, 0xb1]), t(0x69, Some(6), 4));
    // i2l; lxor; lreturn
    assert_eq!(run(&[0x85, 0x83, 0xad]), t(0x83, None, 2));
    // i2l; lcmp; ifge -- never a store.
    assert_eq!(run(&[0x85, 0x94, 0x9c, 0x00, 0x05, 0xb1]), t(0x94, None, 2));
    // An `lstore n` whose index byte is past the end: no store taken.
    assert_eq!(run(&[0x85, 0x61, 0x37]), t(0x61, None, 2));
}

#[test]
fn other_tails_decline() {
    // ldiv / lrem can throw; a shift takes an int distance, not i2l's long.
    assert_eq!(run(&[0x85, 0x6d, 0xad]), None);
    assert_eq!(run(&[0x85, 0x71, 0xad]), None);
    assert_eq!(run(&[0x85, 0x79, 0xad]), None);
    // i2l; lstore (a widening store) is not a tail.
    assert_eq!(run(&[0x85, 0x40, 0xb1]), None);
    // The operation would sit past the end of the code.
    assert_eq!(run(&[0x85]), None);
}

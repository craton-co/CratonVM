//! The operand walk of [`splice_call_operand_sources_with_calls`] steps over
//! a call's inline varargs array (`anewarray; dup; idx; value; aastore`), so
//! the operands before it keep their sources; a `dup` whose two copies feed
//! two operands still refuses.

use super::*;

#[test]
fn an_inline_varargs_array_is_one_operand_and_the_earlier_ones_keep_their_sources() {
    // 0: getstatic M  3: getstatic L  6: iconst_1  7: anewarray #3  10: dup
    // 11: iconst_0  12: iload_0  13: invokestatic Integer.valueOf  16: aastore
    // 17: invokevirtual Method.invoke(Object, Object[])  20: areturn
    let code = [
        0xb2, 0, 1, 0xb2, 0, 2, 0x04, 0xbd, 0, 3, 0x59, 0x03, 0x1a, 0xb8, 0, 4, 0x53, 0xb6, 0, 5, 0xb0,
    ];
    let valueof = |pc: usize| (pc == 13).then_some((1usize, 1usize));
    let sources = splice_call_operand_sources_with_calls(&code, code.len(), 17, 3, &[], &valueof);
    assert_eq!(
        sources,
        Some(vec![
            SpliceOperandSource::GetStatic(0),
            SpliceOperandSource::GetStatic(3),
            SpliceOperandSource::Other,
        ])
    );
}

#[test]
fn a_dup_shared_by_two_operands_refuses() {
    // 0: aload_0  1: dup  2: invokestatic g(Object, Object)V  5: return
    let code = [0x2a, 0x59, 0xb8, 0, 1, 0xb1];
    assert_eq!(splice_call_operand_sources_with_calls(&code, code.len(), 2, 2, &[], &|_| None), None);
}

#[test]
fn an_array_store_before_the_operands_is_not_an_operand() {
    // 0: aload_1  1: iconst_0  2: aload_2  3: aastore  4: aload_0  5: invokestatic h(Object)V
    let code = [0x2b, 0x03, 0x2c, 0x53, 0x2a, 0xb8, 0, 1, 0xb1];
    assert_eq!(
        splice_call_operand_sources_with_calls(&code, code.len(), 5, 1, &[], &|_| None),
        Some(vec![SpliceOperandSource::Local(0)])
    );
}

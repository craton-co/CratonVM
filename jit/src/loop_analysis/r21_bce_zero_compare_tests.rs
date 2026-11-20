use super::*;


/// `static void clear(int[] b, int n) { for (int i = n; i >= 0; i--) b[i] = 0; }`
/// as javac lays it out: the header test is `iload_2; iflt` (`i >= 0`
/// compiles to a zero compare, not `iconst_0; if_icmplt`).
///
/// ```text
///  0: iload_1  1: istore_2
///  2: iload_2  3: iflt -> 16
///  6: aload_0  7: iload_2  8: iconst_0  9: iastore
/// 10: iinc 2, -1  13: goto -> 2
/// 16: return
/// ```
pub(crate) fn zero_compare_down_count() -> Vec<u8> {
    vec![
        0x1b, 0x3d, 0x1c, 0x9b, 0x00, 0x0d, 0x2a, 0x1c, 0x03, 0x4f, 0x84, 0x02, 0xff, 0xa7,
        0xff, 0xf5, 0xb1,
    ]
}

fn none_math(_: u16) -> Option<MinMax> {
    None
}

fn hoisted_entry(code: &[u8]) -> Option<CountedLoop> {
    let t = LoopMethodTables::new(code, code.len(), Some(&[]));
    t.analyze_counted_loop_with_hoisted_fields(2, 13, LoopForm::PreTested, &none_math, &|_| None)
}

/// Off (the default) the zero compare is not a counted-loop test.
#[test]
fn a_zero_compare_test_is_not_counted_by_default() {
    assert!(hoisted_entry(&zero_compare_down_count()).is_none());    }
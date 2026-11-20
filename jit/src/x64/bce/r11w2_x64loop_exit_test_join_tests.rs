use super::*;

/// A rotated `while ((c ? i : j) < a.length) { a[j] = 0; j++; }` (the
/// ecj layout from `loop_analysis`'s
/// `an_exit_test_entered_past_its_iload_is_not_that_locals_test`).
///
/// ```text
///  0: iconst_0  1: istore_3  2: goto -> 12
///  5: aload_0  6: iload_3  7: iconst_0  8: iastore  9: iinc 3,1
/// 12: iload_1  13: ifeq -> 20  16: iload_2  17: goto -> 21
/// 20: iload_3  21: aload_0  22: arraylength  23: if_icmplt -> 5
/// 26: return
/// ```
fn ternary_rotated_test() -> Vec<u8> {
    vec![
        0x03, 0x3e, 0xa7, 0x00, 0x0a, 0x2a, 0x1d, 0x03, 0x4f, 0x84, 0x03, 0x01, 0x1b, 0x99,
        0x00, 0x07, 0x1c, 0xa7, 0x00, 0x04, 0x1d, 0x2a, 0xbe, 0xa1, 0xff, 0xee, 0xb1,
    ]
}

#[test]
fn pattern_b_refuses_a_test_entered_past_its_iload() {
    let mut code = ternary_rotated_test();
    let n = code.len();
    assert_eq!(n, 27);
    let tables = BceMethodTables::build(&code, n);
    assert!(
        locate_exit_test(&code, n, &tables, 5, 23, 26, &|_| None).is_none(),
        "the `c` path tests `i`, so `j < a.length` is not this loop's exit test"
    );
    // MUST ACCEPT twin: `while (j < a.length)`.
    code[12..20].fill(0x00);
    let tables = BceMethodTables::build(&code, n);
    let t =
        locate_exit_test(&code, n, &tables, 5, 23, 26, &|_| None).expect("plain rotated test");
    assert_eq!(t.iv_local, 3);
    assert!(matches!(t.form, LoopForm::PreTested));
}

/// Pattern A with a side path that jumps onto the `aload` of the header's
/// `iload_1; aload_0; arraylength; if_icmpge`, carrying another int.
///
/// ```text
///  0: iconst_0  1: istore_1
///  2: iload_1  3: aload_0  4: arraylength  5: if_icmpge -> 18
///  8: iinc 1,1  11: goto -> 2
/// 14: iload_2  15: goto -> 3
/// 18: return
/// ```
#[test]
fn pattern_a_refuses_a_test_entered_past_its_iload() {
    let mut code: Vec<u8> = vec![
        0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0d, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
        0x1c, 0xa7, 0xff, 0xf4, 0xb1,
    ];
    let n = code.len();
    assert_eq!(n, 19);
    let tables = BceMethodTables::build(&code, n);
    assert!(locate_exit_test(&code, n, &tables, 2, 11, 14, &|_| None).is_none());
    // MUST ACCEPT twin: the side path lands on the header itself.
    code[17] = 0xf3; // goto -> 2
    let tables = BceMethodTables::build(&code, n);
    let t = locate_exit_test(&code, n, &tables, 2, 11, 14, &|_| None).expect("pattern A");
    assert_eq!(t.iv_local, 1);
}

/// The tables answer `falls_through_into` exactly as the old per-loop
/// decode from pc 0 did.
#[test]
fn falls_through_into_reads_the_previous_instruction_start() {
    let code = ternary_rotated_test();
    let tables = BceMethodTables::build(&code, code.len());
    // 5 follows `goto` at 2: no fall-through.
    assert!(!falls_through_into(&code, &tables.starts, 5));
    // 12 follows `iinc` at 9: falls through.
    assert!(falls_through_into(&code, &tables.starts, 12));
    // pc 0 and a non-instruction-start answer the pessimistic `true`.
    assert!(falls_through_into(&code, &tables.starts, 0));
    assert!(falls_through_into(&code, &tables.starts, 3));
}

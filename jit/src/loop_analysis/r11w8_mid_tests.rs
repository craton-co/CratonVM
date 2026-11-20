use super::*;

fn no_math(_: u16) -> Option<MinMax> {
    None
}

/// `(header, back_edge)` of `code`, pre-tested, with the exception table
/// `table`.
fn counted(
    code: &[u8],
    header: usize,
    back_edge: usize,
    table: &[(usize, usize, usize)],
) -> Option<(CountedLoop, usize)> {
    analyze_counted_loop_at_with_handlers(
        code,
        code.len(),
        header,
        back_edge,
        LoopForm::PreTested,
        &no_math,
        Some(table),
    )
}

/// `int i = 0; while (i < a.length) { if (a[i] >= 0) { i++; continue; }
/// i++; }` -- one advance per path, and two back edges.
///
/// ```text
///  0: iconst_0   1: istore_1   2: iload_1 (header)   3: aload_0
///  4: arraylength   5: if_icmpge -> 26   8: aload_0   9: iload_1
/// 10: iaload   11: ifge -> 20   14: iinc 1,1   17: goto -> 2
/// 20: iinc 1,1   23: goto -> 2   26: return
/// ```
fn continue_loop() -> Vec<u8> {
    vec![
        0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x15, 0x2a, 0x1b, 0x2e, 0x9c, 0x00, 0x09,
        0x84, 0x01, 0x01, 0xa7, 0xff, 0xf1, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xeb, 0xb1,
    ]
}

/// The loop merged per header (its farthest back edge, 23) is counted:
/// every path runs one `iinc`, and the entry value is the constant.
#[test]
fn a_continue_that_advances_is_one_advance_per_iteration() {
    let code = continue_loop();
    assert_eq!(code.len(), 27);
    let (l, test_pc) = counted(&code, 2, 23, &[]).expect("merged continue loop is counted");
    assert_eq!(test_pc, 2);
    assert_eq!(l.iv.local, 1);
    assert_eq!(l.iv.stride, Stride::Const(1));
    assert_eq!(l.iv.init, IntRange::constant(0));
    assert_eq!(l.cmp, ExitCmp::Ge);
    assert_eq!(l.bound, BoundSource::ArrayLength(0));
    assert!(!l.has_other_exit, "the continue path stays in the loop");
    // Per pair, `(2, 17)` still reads the second back edge as a side entry
    // onto the header, so its entry value is unknown.
    let (per_pair, _) = counted(&code, 2, 17, &[]).expect("per-pair loop");
    assert!(per_pair.iv.init.is_unknown());
}

/// A path that runs BOTH advances (the `continue` removed, so the first
/// arm falls into the second) and advances of different strides are not a
/// per-iteration stride.
#[test]
fn two_advances_on_one_path_or_of_two_strides_are_refused() {
    let mut both = continue_loop();
    both[17..20].fill(0x00);
    assert!(counted(&both, 2, 23, &[]).is_none());
    let mut strides = continue_loop();
    strides[22] = 0x02;
    assert!(counted(&strides, 2, 23, &[]).is_none());
}

/// `if (c) i++; else i++;` with one back edge.
///
/// ```text
///  0: iconst_0   1: istore_1   2: iload_1 (header)   3: iload_2
///  4: if_icmpge -> 23   7: iload_3   8: ifeq -> 17   11: iinc 1,1
/// 14: goto -> 20   17: iinc 1,1   20: goto -> 2   23: return
/// ```
#[test]
fn an_advance_in_each_arm_of_an_if_is_one_per_iteration() {
    let code: Vec<u8> = vec![
        0x03, 0x3c, 0x1b, 0x1c, 0xa2, 0x00, 0x13, 0x1d, 0x99, 0x00, 0x09, 0x84, 0x01, 0x01,
        0xa7, 0x00, 0x06, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xee, 0xb1,
    ];
    assert_eq!(code.len(), 24);
    let (l, _) = counted(&code, 2, 20, &[]).expect("counted");
    assert_eq!(l.iv.stride, Stride::Const(1));
    assert_eq!(l.iv.init, IntRange::constant(0));
}

/// Exception edges carry the count too: a handler on the header for code
/// ahead of both advances runs an iteration with none; one landing on the
/// second arm from the first arm's `iinc` (count 0 before it) is fine.
#[test]
fn a_handler_edge_is_an_edge_for_the_several_advance_proof() {
    let code = continue_loop();
    assert!(counted(&code, 2, 23, &[(8, 11, 2)]).is_none());
    assert!(counted(&code, 2, 23, &[(14, 17, 20)]).is_some());
    // From the `goto` after the first advance (count 1) to the second arm
    // (count 0): the arm would advance twice.
    assert!(counted(&code, 2, 23, &[(17, 20, 20)]).is_none());
}

/// ecj's rotated `int i = 0; while (i < a.length) { do { a[i] = 0; i++; }
/// while (p); }`: the `do` loop and the `while` loop share the header, and
/// the `do` back edge re-enters it without the `while` test. Reading the
/// pair `(5, 19)` as counted by `i < a.length` let `a[i]` lose its check
/// while `p` walks `i` past the end.
///
/// ```text
///  0: iconst_0   1: istore_1   2: goto -> 16
///  5: aload_0 (header)   6: iload_1   7: iconst_0   8: iastore
///  9: iinc 1,1   12: iload_2   13: ifne -> 5
/// 16: iload_1   17: aload_0   18: arraylength   19: if_icmplt -> 5
/// 22: return
/// ```
#[test]
fn a_back_edge_that_skips_a_rotated_test_is_not_counted_by_it() {
    let mut code: Vec<u8> = vec![
        0x03, 0x3c, 0xa7, 0x00, 0x0e, 0x2a, 0x1b, 0x03, 0x4f, 0x84, 0x01, 0x01, 0x1c, 0x9a,
        0xff, 0xf8, 0x1b, 0x2a, 0xbe, 0xa1, 0xff, 0xf2, 0xb1,
    ];
    assert_eq!(code.len(), 23);
    assert!(counted(&code, 5, 19, &[]).is_none());
    // MUST ACCEPT twin: without the `do` edge the test closes every
    // iteration.
    code[12..16].fill(0x00);
    let (l, test_pc) = counted(&code, 5, 19, &[]).expect("rotated counted loop");
    assert_eq!(test_pc, 16);
    assert_eq!(l.iv.init, IntRange::constant(0));
    assert_eq!(l.bound, BoundSource::ArrayLength(0));
}

/// One [`LoopMethodTables`] answers exactly what the per-call functions
/// answer, for every loop of a method.
#[test]
fn method_tables_agree_with_the_per_call_functions() {
    // Two sibling loops sharing slot 1 (see the round-11 sibling test).
    let code: Vec<u8> = vec![
        0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
        0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
        0xb1,
    ];
    let n = code.len();
    let no_table: Option<&[(usize, usize, usize)]> = None;
    for handlers in [no_table, Some(&[][..])] {
        let t = LoopMethodTables::new(&code, n, handlers);
        for (h, b, end) in [(2, 11, 14), (16, 25, 28)] {
            let init = t.constant_iv_init(h, end, 1);
            assert_eq!(init, Some(0));
            assert_eq!(
                init,
                constant_iv_init_with_handlers(&code, n, h, end, 1, handlers)
            );
            let form = LoopForm::PreTested;
            assert_eq!(
                t.analyze_counted_loop(h, b, form, &no_math),
                analyze_counted_loop_with_handlers(&code, n, h, b, form, &no_math, handlers)
            );
        }
    }
    // A `code_len` past the buffer is clamped rather than read.
    let t = LoopMethodTables::new(&code, n + 10, Some(&[]));
    assert_eq!(t.constant_iv_init(2, 14, 1), Some(0));
}

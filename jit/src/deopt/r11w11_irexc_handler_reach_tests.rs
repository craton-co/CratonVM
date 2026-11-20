use super::{pc_reachable_from_handlers, pcs_reachable_from_handlers};

/// `R11SyncCallerHoldsCalleeLock.throwLoop`'s shape: a counted loop whose
/// `catch` bumps a counter and falls back into the loop.
///
/// ```text
///  0 iconst_0; 1 istore_1; 2 iconst_0; 3 istore_2
///  4 iload_2                <- loop header
///  5 iload_0; 6 if_icmpge 27
///  9 iload_2                <- try [9, 14)
/// 10 invokestatic #1; 13 pop
/// 14 goto 21
/// 17 astore_3               <- handler
/// 18 iinc 1, 1
/// 21 iinc 2, 1; 24 goto 4
/// 27 iload_1; 28 ireturn
/// ```
const CATCH_CONTINUES: [u8; 29] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x15, 0x1c, 0xb8, 0x00, 0x01, 0x57, 0xa7,
    0x00, 0x07, 0x4e, 0x84, 0x01, 0x01, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xec, 0x1b, 0xac,
];

/// The same loop whose handler rethrows (`astore_3; aload_3; athrow`) —
/// the shape of javac's `synchronized`-block and `finally` handlers.
const CATCH_RETHROWS: [u8; 29] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x15, 0x1c, 0xb8, 0x00, 0x01, 0x57, 0xa7,
    0x00, 0x07, 0x4e, 0x2d, 0xbf, 0x00, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xec, 0x1b, 0xac,
];

/// The same loop whose handler leaves it (`astore_3; goto 27`).
const CATCH_LEAVES: [u8; 29] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x15, 0x1c, 0xb8, 0x00, 0x01, 0x57, 0xa7,
    0x00, 0x07, 0x4e, 0xa7, 0x00, 0x09, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xec, 0x1b, 0xac,
];

#[test]
fn a_handler_that_falls_back_into_the_loop_reaches_its_header() {
    let reach = pcs_reachable_from_handlers(&CATCH_CONTINUES, &[17]);
    assert!(reach.is_some(), "the fixture must verify");
    assert!(
        pc_reachable_from_handlers(reach.as_deref(), 4),
        "the loop header"
    );
    assert!(
        pc_reachable_from_handlers(reach.as_deref(), 17),
        "the handler entry itself"
    );
    assert!(
        !pc_reachable_from_handlers(reach.as_deref(), 0),
        "the method prologue"
    );
}

#[test]
fn a_handler_that_rethrows_or_leaves_the_loop_does_not_reach_its_header() {
    let rethrows = pcs_reachable_from_handlers(&CATCH_RETHROWS, &[17]);
    assert_eq!(rethrows.as_deref(), Some(&[17u32, 18, 19][..]));
    assert!(!pc_reachable_from_handlers(rethrows.as_deref(), 4));
    let leaves = pcs_reachable_from_handlers(&CATCH_LEAVES, &[17]);
    assert_eq!(leaves.as_deref(), Some(&[17u32, 18, 27, 28][..]));
    assert!(!pc_reachable_from_handlers(leaves.as_deref(), 4));
}

/// The `padded_bytecode` form (two trailing zero bytes, decoded as two
/// unreachable `nop`s) answers the same, so a door may pass the frame's
/// buffer as it is.
#[test]
fn the_padded_buffer_answers_the_same() {
    let mut padded = CATCH_CONTINUES.to_vec();
    padded.extend_from_slice(&[0, 0]);
    assert_eq!(
        pcs_reachable_from_handlers(&padded, &[17]),
        pcs_reachable_from_handlers(&CATCH_CONTINUES, &[17])
    );
}

#[test]
fn no_handler_reaches_nothing_and_an_unanalysable_answer_fails_closed() {
    assert_eq!(
        pcs_reachable_from_handlers(&CATCH_CONTINUES, &[]),
        Some(Vec::new())
    );
    // 16 is inside the `goto` at 14: not an instruction start.
    let bad = pcs_reachable_from_handlers(&CATCH_CONTINUES, &[16]);
    assert_eq!(bad, None);
    assert!(pc_reachable_from_handlers(bad.as_deref(), 4));
    // Truncated code does not verify.
    let bad = pcs_reachable_from_handlers(&[0x11, 0x01], &[0]);
    assert_eq!(bad, None);
    assert!(pc_reachable_from_handlers(None, 0));
}

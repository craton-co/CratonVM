use super::*;

/// A normal `goto` into a handler entry. Local 1 is `null` until the
/// protected `new; astore_1`, so an exception thrown by the `new` reaches
/// the handler with local 1 still null. The `goto` path alone proves it
/// non-null, and without the table that was the handler's whole IN.
#[rustfmt::skip]
fn goto_into_handler() -> Vec<u8> {
    vec![
        0x01,             // 0: aconst_null
        0x4C,             // 1: astore_1
        0xBB, 0x00, 0x02, // 2: new #2            (protected, may throw)
        0x4C,             // 5: astore_1          (local 1 non-null)
        0x2C,             // 6: aload_2           (a Throwable, say)
        0xA7, 0x00, 0x03, // 7: goto -> 10        (a NORMAL edge into the handler)
        0x4E,             // 10: astore_3         (handler entry)
        0x2B,             // 11: aload_1
        0xC6, 0x00, 0x03, // 12: ifnull -> 15
        0xB1,             // 15: return
    ]
}

#[test]
fn a_handler_entry_a_branch_also_reaches_starts_from_the_handler_seed() {
    let code = goto_into_handler();
    let table = [(2usize, 7usize, 10usize)];
    let info = analyze_with_receiver_and_handlers(&code, code.len(), false, Some(&table));
    assert!(
        !info.is_nonnull(12, 1),
        "the exception path reaches the handler with local 1 == null"
    );
    assert!(info.is_merge_point(10));
}

/// Without the table the old answer stands: the goto path's fact is kept.
/// Pinned so the test above is known to exercise the difference.
#[test]
fn without_the_table_only_the_normal_edge_is_seen() {
    let code = goto_into_handler();
    let info = analyze_with_receiver(&code, code.len(), false);
    assert!(info.is_nonnull(12, 1));
}

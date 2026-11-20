use super::*;

/// `int v = (c ? a : b).x; return b == null ? -1 : v;`
/// The `getfield` is a join of `aload_1` and `aload_2`, but its textual
/// predecessor is `aload_2`, so "b is non-null" was proven on both paths.
#[test]
fn a_dereference_at_a_join_proves_nothing_about_its_textual_predecessor() {
    let code = vec![
        0x1a, //             0: iload_0
        0x99, 0x00, 0x07, // 1: ifeq -> 8
        0x2b, //             4: aload_1
        0xa7, 0x00, 0x04, // 5: goto -> 9
        0x2c, //             8: aload_2
        0xb4, 0x00, 0x01, // 9: getfield #1   (join)
        0x36, 0x03, //      12: istore 3
        0x2c, //            14: aload_2
        0xc7, 0x00, 0x05, // 15: ifnonnull -> 20
        0x02, //            18: iconst_m1
        0xac, //            19: ireturn
        0x1d, //            20: iload_3
        0xac, //            21: ireturn
    ];
    let info = analyze(&code, code.len());
    assert!(info.is_merge_point(9));
    assert!(
        !info.is_nonnull(14, 2),
        "b was never dereferenced on the c == true path"
    );
    assert!(!info.is_nonnull(15, 2));
    assert!(!info.is_nonnull(14, 1));
}

/// A `tableswitch` case nulls local 1 and joins a path on which a
/// `getfield` proved it non-null. Switch edges were not decoded, so the
/// case never reached the join.
#[test]
fn a_switch_case_participates_in_the_join_it_reaches() {
    let code = vec![
        0x2b, //                          0: aload_1
        0xb4, 0x00, 0x01, //              1: getfield     (local 1 non-null)
        0x57, //                          4: pop
        0x1c, //                          5: iload_2
        0x99, 0x00, 0x1b, //              6: ifeq -> 33
        0x1d, //                          9: iload_3
        0xaa, //                         10: tableswitch
        0x00, //                         11: padding
        0x00, 0x00, 0x00, 0x17, //       12: default -> 33
        0x00, 0x00, 0x00, 0x00, //       16: low 0
        0x00, 0x00, 0x00, 0x00, //       20: high 0
        0x00, 0x00, 0x00, 0x12, //       24: case 0 -> 28
        0x01, //                         28: aconst_null
        0x4c, //                         29: astore_1
        0xa7, 0x00, 0x03, //             30: goto -> 33
        0x2b, //                         33: aload_1      (join)
        0xc6, 0x00, 0x05, //             34: ifnull -> 39
        0x03, //                         37: iconst_0
        0xac, //                         38: ireturn
        0x04, //                         39: iconst_1
        0xac, //                         40: ireturn
    ];
    let info = analyze(&code, code.len());
    assert!(info.is_merge_point(28));
    assert!(info.is_merge_point(33));
    assert!(
        !info.is_nonnull(33, 1),
        "the switch case stored null into local 1"
    );
    assert!(!info.is_nonnull(34, 1));
}

/// `try { mayThrow(); } catch (E e) { a = null; } if (a == null) ...`
/// No exception table is consulted; the handler used to stay unvisited,
/// so the join kept "a non-null" from the normal path alone.
#[test]
fn code_only_an_exception_edge_reaches_is_seeded_into_the_join() {
    let code = vec![
        0x2b, //             0: aload_1
        0xb4, 0x00, 0x01, // 1: getfield     (local 1 non-null)
        0x57, //             4: pop
        0xb8, 0x00, 0x02, // 5: invokestatic
        0xa7, 0x00, 0x06, // 8: goto -> 14
        0x57, //            11: pop          (handler entry)
        0x01, //            12: aconst_null
        0x4c, //            13: astore_1
        0x2b, //            14: aload_1      (join)
        0xc6, 0x00, 0x05, // 15: ifnull -> 20
        0x03, //            18: iconst_0
        0xac, //            19: ireturn
        0x04, //            20: iconst_1
        0xac, //            21: ireturn
    ];
    let info = analyze(&code, code.len());
    assert!(
        info.is_merge_point(11),
        "a handler-only entry is a merge point"
    );
    assert!(
        !info.is_nonnull(14, 1),
        "the handler stored null into local 1"
    );
    assert!(!info.is_nonnull(15, 1));
    // The fact still holds where only the normal path reaches.
    assert!(info.is_nonnull(5, 1));
}

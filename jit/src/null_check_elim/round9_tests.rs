use super::*;

/// `wide astore 1` is a store. It used to be invisible to `astore_at`, so
/// the `new; astore_1` fact survived an explicit null store.
#[test]
fn a_wide_astore_kills_the_fact() {
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0xBB, 0x00, 0x01,       // 0: new #1
        0x4C,                   // 3: astore_1          (local 1 non-null)
        0x01,                   // 4: aconst_null
        0xC4, 0x3A, 0x00, 0x01, // 5: wide astore 1     (local 1 = null)
        0x2B,                   // 9: aload_1
        0xB1,                   // 10: return
    ];
    let info = analyze(&code, code.len());
    assert!(info.is_nonnull(4, 1), "the fixture must establish the fact");
    assert!(
        !info.is_nonnull(9, 1),
        "local 1 was just nulled by a wide store; proving it non-null \
         elides the program's own null test"
    );
}

/// `aload K; astore N` copies K's fact to N, directly or through a
/// `checkcast`; an unproven source proves nothing.
#[test]
fn a_copy_of_a_non_null_local_is_non_null() {
    #[rustfmt::skip]
    let direct: Vec<u8> = vec![
        0xBB, 0x00, 0x01, // 0: new #1
        0x4C,             // 3: astore_1   (proven)
        0x2B,             // 4: aload_1
        0x4D,             // 5: astore_2   (copy)
        0x2C,             // 6: aload_2
        0xB1,             // 7: return
    ];
    assert!(analyze(&direct, direct.len()).is_nonnull(6, 2));

    #[rustfmt::skip]
    let cast: Vec<u8> = vec![
        0xBB, 0x00, 0x01, // 0: new #1
        0x4C,             // 3: astore_1
        0x2B,             // 4: aload_1
        0xC0, 0x00, 0x02, // 5: checkcast #2
        0x4D,             // 8: astore_2
        0x2C,             // 9: aload_2
        0xB1,             // 10: return
    ];
    assert!(analyze(&cast, cast.len()).is_nonnull(9, 2));

    #[rustfmt::skip]
    let unproven: Vec<u8> = vec![
        0xBB, 0x00, 0x01, // 0: new #1
        0x4C,             // 3: astore_1
        0x2D,             // 4: aload_3    (nothing known about local 3)
        0x4D,             // 5: astore_2
        0x2C,             // 6: aload_2
        0xB1,             // 7: return
    ];
    assert!(!analyze(&unproven, unproven.len()).is_nonnull(6, 2));
}

/// A copy whose store is a join proves nothing: the stored value may come
/// from the other arm.
#[test]
fn a_copy_at_a_join_is_not_trusted() {
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0xBB, 0x00, 0x01, // 0: new #1
        0x4C,             // 3: astore_1           (local 1 non-null)
        0x1A,             // 4: iload_0
        0x99, 0x00, 0x07, // 5: ifeq -> 12
        0x2B,             // 8: aload_1
        0xA7, 0x00, 0x04, // 9: goto -> 13
        0x01,             // 12: aconst_null
        0x4D,             // 13: astore_2          (join)
        0x2C,             // 14: aload_2
        0xB1,             // 15: return
    ];
    let info = analyze(&code, code.len());
    assert!(info.is_merge_point(13));
    assert!(!info.is_nonnull(14, 2));
}

/// The receiver survives a join with an exception handler when nothing
/// stores into local 0 — and does not when something does.
#[test]
fn the_receiver_seed_reaches_handler_code() {
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0xB8, 0x00, 0x02, // 0: invokestatic #2
        0xA7, 0x00, 0x05, // 3: goto -> 8
        0x57,             // 6: pop                (handler entry)
        0x00,             // 7: nop
        0x2A,             // 8: aload_0            (join)
        0xC6, 0x00, 0x05, // 9: ifnull -> 14
        0x03,             // 12: iconst_0
        0xAC,             // 13: ireturn
        0x04,             // 14: iconst_1
        0xAC,             // 15: ireturn
    ];
    let seeded = analyze_with_receiver(&code, code.len(), true);
    assert!(
        seeded.is_nonnull(8, 0),
        "`this` is non-null on the handler path too"
    );
    let unseeded = analyze_with_receiver(&code, code.len(), false);
    assert!(!unseeded.is_nonnull(8, 0));

    #[rustfmt::skip]
    let reassigned: Vec<u8> = vec![
        0xB8, 0x00, 0x02, // 0: invokestatic #2
        0xA7, 0x00, 0x06, // 3: goto -> 9
        0x57,             // 6: pop                (handler entry)
        0x01,             // 7: aconst_null
        0x4B,             // 8: astore_0           (local 0 is not `this` any more)
        0x2A,             // 9: aload_0            (join)
        0xC6, 0x00, 0x05, // 10: ifnull -> 15
        0x03,             // 13: iconst_0
        0xAC,             // 14: ireturn
        0x04,             // 15: iconst_1
        0xAC,             // 16: ireturn
    ];
    let info = analyze_with_receiver(&reassigned, reassigned.len(), true);
    assert!(!info.is_nonnull(9, 0));
}

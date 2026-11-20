use super::{
    osr_entries_a_caught_throw_may_reenter, osr_exception_exit_bcis_uncovered,
    osr_guard_exits_caught_off_every_entry, osr_handler_reachable_pcs,
};
use cratonvm_reader::attribute::ExceptionTableEntry;

/// `R16MonitorCrowd.shortCs(I)V` as javac 25 emits it, plus the frame's
/// padding: `for (i = 0; i < n; i++) synchronized (LOCK) { B.acc +=
/// B.counter; B.counter++; }`. Header 2, the block [13, 44), the
/// catch-all handler 47 (`astore_3; aload_2; monitorexit; aload_3; athrow`).
const SHORT_CS: [u8; 61] = [
    0x03, 0x3c, 0x1b, 0x1a, 0xa2, 0x00, 0x36, 0xb2, 0x00, 0x09, 0x59, 0x4d, 0xc2, 0xb2, 0x00,
    0x0f, 0x59, 0xb4, 0x00, 0x13, 0xb2, 0x00, 0x0f, 0xb4, 0x00, 0x19, 0x61, 0xb5, 0x00, 0x13,
    0xb2, 0x00, 0x0f, 0x59, 0xb4, 0x00, 0x19, 0x0a, 0x61, 0xb5, 0x00, 0x19, 0x2c, 0xc3, 0xa7,
    0x00, 0x08, 0x4e, 0x2c, 0xc3, 0x2d, 0xbf, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xcb, 0xb1, 0x00,
    0x00,
];

fn table() -> [ExceptionTableEntry; 2] {
    [
        ExceptionTableEntry {
            start_pc: 13,
            end_pc: 44,
            handler_pc: 47,
            catch_type: 0,
        },
        ExceptionTableEntry {
            start_pc: 47,
            end_pc: 50,
            handler_pc: 47,
            catch_type: 0,
        },
    ]
}

/// The guards at 27 and 39 lie in the catch-all row, so the shipped rule
/// refuses them (the positive control); the handler reaches only its own
/// release-and-rethrow, never the header 2, so the widening admits the
/// body entered at 2 -- and refuses it entered at a handler-reached pc,
/// with no listed guard exit, or with an unanalysable handler.
#[test]
fn a_sync_blocks_guards_are_admitted_only_off_every_handler_path() {
    let table = table();
    let guards: &[u32] = &[27, 39];
    assert!(!osr_exception_exit_bcis_uncovered(Some(guards), &table));
    let reach = osr_handler_reachable_pcs(&SHORT_CS, &table);
    assert_eq!(reach.as_deref(), Some(&[47u32, 48, 49, 50, 51][..]));
    assert!(osr_guard_exits_caught_off_every_entry(
        Some(guards),
        reach.as_deref(),
        &[2]
    ));
    assert!(
        !osr_guard_exits_caught_off_every_entry(Some(guards), reach.as_deref(), &[2, 48]),
        "an entry on a handler path refuses"
    );
    assert!(
        !osr_guard_exits_caught_off_every_entry(None, reach.as_deref(), &[2]),
        "a body whose guards the lowerer did not list is never admitted"
    );
    assert!(
        !osr_guard_exits_caught_off_every_entry(Some(guards), None, &[2]),
        "unanalysable handlers refuse"
    );
}

/// The `catch` that falls back into the loop (`throwLoop`'s shape, the
/// round-11 reach test's bytes) reaches the header: refused.
#[test]
fn a_catch_that_continues_the_loop_refuses() {
    // 0 iconst_0; 1 istore_1; 2 iconst_0; 3 istore_2; 4 iload_2 <- header
    // 5 iload_0; 6 if_icmpge 27; 9 iload_2 <- try [9, 14)
    // 10 invokestatic #1; 13 pop; 14 goto 21; 17 astore_3 <- handler
    // 18 iinc 1, 1; 21 iinc 2, 1; 24 goto 4; 27 iload_1; 28 ireturn; pad
    let continues: [u8; 31] = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x15, 0x1c, 0xb8, 0x00, 0x01, 0x57,
        0xa7, 0x00, 0x07, 0x4e, 0x84, 0x01, 0x01, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xec, 0x1b,
        0xac, 0x00, 0x00,
    ];
    let table = [ExceptionTableEntry {
        start_pc: 9,
        end_pc: 14,
        handler_pc: 17,
        catch_type: 0,
    }];
    let reach = osr_handler_reachable_pcs(&continues, &table);
    assert!(reach.is_some());
    assert!(!osr_guard_exits_caught_off_every_entry(
        Some(&[10u32][..]),
        reach.as_deref(),
        &[4]
    ));
    // OSR round (2026-10-06): the same header entered through an OPAQUE
    // arm (every OSR-door body's) assumes nothing about the handler's
    // state, so the door admits the body (`FerOsrTry`); the kill switch
    // restores the refusal.
    let exempt = osr_entries_a_caught_throw_may_reenter([4u32].into_iter(), &[4], true);
    assert!(exempt.is_empty());
    assert!(osr_guard_exits_caught_off_every_entry(
        Some(&[10u32][..]),
        reach.as_deref(),
        &exempt
    ));
    let kept = osr_entries_a_caught_throw_may_reenter([4u32].into_iter(), &[4], false);
    assert!(!osr_guard_exits_caught_off_every_entry(
        Some(&[10u32][..]),
        reach.as_deref(),
        &kept
    ));
    // A non-opaque entry is never exempt.
    let mixed = osr_entries_a_caught_throw_may_reenter([4u32, 21].into_iter(), &[21], true);
    assert_eq!(mixed, vec![4]);
}

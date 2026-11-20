use super::indy_sites_all_outside_loops;

/// JIT round 22 wave 2 (lane throws, THROWS22-2): an `invokedynamic` after
/// the loop qualifies; one inside it, or a method with no loop, does not.
#[test]
fn an_indy_after_the_loop_is_outside_every_loop() {
    //  0: iconst_0  1: istore_0  2: iload_0  3: bipush 10  5: if_icmpge 14
    //  8: iinc 0 1  11: goto 2   14: invokedynamic #1  19: areturn
    let code = [
        0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x09, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xf7,
        0xba, 0x00, 0x01, 0x00, 0x00, 0xb0, 0, 0,
    ];
    assert!(indy_sites_all_outside_loops(&code, 20, &[(14, 1)], &[]));
    // A site inside [2, 11] (the loop's textual range) is on the cycle.
    assert!(!indy_sites_all_outside_loops(&code, 20, &[(14, 1), (8, 1)], &[]));
    // No back edge: no loop, so the plan is not taken.
    let straight = [0xba, 0x00, 0x01, 0x00, 0x00, 0xb0, 0, 0];
    assert!(!indy_sites_all_outside_loops(&straight, 6, &[(0, 1)], &[]));
    // JIT r22 wave 3: a handler at pc 14 protecting [14, 20) re-enters
    // itself through its exception edge (no backward branch): the indy at
    // 14 is on that cycle. A handler past its range adds nothing.
    let row = |start_pc: u16, end_pc: u16, handler_pc: u16| {
        cratonvm_reader::attribute::ExceptionTableEntry {
            start_pc,
            end_pc,
            handler_pc,
            catch_type: 0,
        }
    };
    assert!(!indy_sites_all_outside_loops(&code, 20, &[(14, 1)], &[row(14, 20, 14)]));
    assert!(indy_sites_all_outside_loops(&code, 20, &[(14, 1)], &[row(0, 2, 14)]));
}

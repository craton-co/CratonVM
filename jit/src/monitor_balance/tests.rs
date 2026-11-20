use super::*;

fn site(code: &[u8], table: &[(usize, usize, usize)], all: &[bool]) -> Option<usize> {
    unbalanced_monitor_site(code, code.len(), table, all, BalanceRules::ALL).map(|(pc, _)| pc)
}

fn site_depth_only(code: &[u8]) -> Option<usize> {
    unbalanced_monitor_site(code, code.len(), &[], &[], BalanceRules::DEPTH_ONLY)
        .map(|(pc, _)| pc)
}

/// javac's `synchronized (o) { o.hashCode(); }` and two nested blocks.
#[rustfmt::skip]
const JAVAC_BLOCK: [u8; 20] = [
    0x2a, 0x59, 0x4c, 0xc2,       // 0: aload_0 dup astore_1 monitorenter
    0x2a, 0xb6, 0x00, 0x02, 0x57, // 4: aload_0 invokevirtual pop
    0x2b, 0xc3,                   // 9: aload_1 monitorexit
    0xa7, 0x00, 0x08,             // 11: goto 19
    0x4d, 0x2b, 0xc3, 0x2c, 0xbf, // 14: astore_2 aload_1 monitorexit aload_2 athrow
    0xb1,                         // 19: return
];
const JAVAC_BLOCK_TABLE: [(usize, usize, usize); 2] = [(4, 11, 14), (14, 17, 14)];

#[test]
fn javac_shapes_are_balanced() {
    assert_eq!(site(&JAVAC_BLOCK, &JAVAC_BLOCK_TABLE, &[true, true]), None);
    #[rustfmt::skip]
    let nested = [
        0x2a, 0x59, 0x4c, 0xc2,       // 0: enter a (locals 1, 0)
        0x2b, 0x59, 0x4d, 0xc2,       // 4: enter b (locals 2, 1)
        0x2c, 0xc3,                   // 8: exit b
        0xa7, 0x00, 0x08,             // 10: goto 18
        0x4e, 0x2c, 0xc3, 0x2d, 0xbf, // 13: inner handler
        0x2b, 0xc3,                   // 18: exit a
        0xa7, 0x00, 0x0a,             // 20: goto 30
        0x3a, 0x04, 0x2b, 0xc3, 0x19, 0x04, 0xbf, // 23: outer handler
        0xb1,                         // 30: return
    ];
    let table = [(8, 10, 13), (13, 16, 13), (4, 20, 23), (23, 27, 23)];
    assert_eq!(site(&nested, &table, &[true, true, true, true]), None);
    // kotlinc's `synchronized(lock)`: `aload_1; astore_2; aload_2;
    // monitorenter`, released through local 2.
    #[rustfmt::skip]
    let kotlin = [
        0x2b, 0x4d, 0x2c, 0xc2, // 0: aload_1 astore_2 aload_2 monitorenter
        0x03, 0x3e,             // 4: iconst_0 istore_3
        0x2c, 0xc3,             // 6: aload_2 monitorexit
        0xb1,                   // 8: return
    ];
    assert_eq!(site(&kotlin, &[], &[]), None);
}

#[test]
fn the_depth_rules_refuse_at_their_pc() {
    assert_eq!(site(&[0x2a, 0xc2, 0xb1], &[], &[]), Some(2));
    assert_eq!(site(&[0x2a, 0xc3, 0xb1], &[], &[]), Some(1));
    #[rustfmt::skip]
    let branch = [
        0x1b, 0x99, 0x00, 0x05, // 0: iload_1 ifeq 6
        0x2a, 0xc2,             // 4: aload_0 monitorenter
        0xb1,                   // 6: return
    ];
    assert_eq!(site(&branch, &[], &[]), Some(6));
    #[rustfmt::skip]
    let join = [
        0x1b, 0x99, 0x00, 0x06, // 0: iload_1 ifeq 7
        0x2a, 0xc2,             // 4: aload_0 monitorenter
        0x2a, 0xc3,             // 6: aload_0; 7: monitorexit
        0xb1,                   // 8: return
    ];
    assert_eq!(site(&join, &[], &[]), Some(7));
    assert_eq!(site(&[0x2a, 0xc2, 0x01, 0xbf], &[], &[]), Some(3));
}

/// Residual 1: an implicit trap under a monitor with no catch-all.
#[test]
fn an_uncovered_implicit_trap_is_refused_only_under_its_rule() {
    // static int m() { o(); monitorenter; o(); monitorexit; return 1; }
    #[rustfmt::skip]
    let fixture = [0xb8, 0x00, 0x01, 0xc2, 0xb8, 0x00, 0x01, 0xc3, 0x04, 0xac];
    assert_eq!(site(&fixture, &[], &[]), Some(4));
    assert_eq!(site_depth_only(&fixture), None);
    // The same call inside javac's catch-all is admitted.
    assert_eq!(site(&JAVAC_BLOCK, &JAVAC_BLOCK_TABLE, &[true, true]), None);
    // A typed handler alone does not end the search.
    assert_eq!(site(&JAVAC_BLOCK, &JAVAC_BLOCK_TABLE, &[false, true]), Some(5));
}

/// Residual 2: `enter a; exit b` at the same depth.
#[test]
fn an_exit_of_another_local_is_refused_only_under_its_rule() {
    let wrong = [0x2a, 0xc2, 0x2b, 0xc3, 0xb1];
    assert_eq!(site(&wrong, &[], &[]), Some(3));
    assert_eq!(site_depth_only(&wrong), None);
    // A copy `aload_0; astore_2` made while the lock is held is the same
    // object.
    let copied = [0x2a, 0xc2, 0x2a, 0x4d, 0x2c, 0xc3, 0xb1];
    assert_eq!(site(&copied, &[], &[]), None);
    // A store over the only local holding the lock leaves nothing tied to
    // it: the exit through local 1 is not provably the lock's (refused
    // since wave 7; HotSpot refuses it too).
    let forgot = [0x2a, 0xc2, 0x01, 0x4b, 0x2b, 0xc3, 0xb1];
    assert_eq!(site(&forgot, &[], &[]), Some(5));
    assert_eq!(site_depth_only(&forgot), None);
    // Non-LIFO release of two locals: the first exit names the outer.
    // (Under every rule the inner enter, which can raise with the outer
    // held and no handler, is refused first, at pc 3.)
    let non_lifo = [0x2a, 0xc2, 0x2b, 0xc2, 0x2a, 0xc3, 0x2b, 0xc3, 0xb1];
    let operands_only = BalanceRules {
        implicit_traps: false,
        operands: true,
    };
    assert_eq!(
        unbalanced_monitor_site(&non_lifo, non_lifo.len(), &[], &[], operands_only)
            .map(|(pc, _)| pc),
        Some(5)
    );
    assert_eq!(site(&non_lifo, &[], &[]), Some(3));
    assert_eq!(
        site_depth_only(&[0x2a, 0xc2, 0x2b, 0xc2, 0x2a, 0xc3, 0x2b, 0xc3, 0xb1]),
        None
    );
}

const OPERANDS_ONLY: BalanceRules = BalanceRules {
    implicit_traps: false,
    operands: true,
};

fn site_operands_only(code: &[u8]) -> Option<usize> {
    unbalanced_monitor_site(code, code.len(), &[], &[], OPERANDS_ONLY).map(|(pc, _)| pc)
}

/// Wave 7 (review w45 item 2): the lock's identity follows copies made
/// BEFORE the enter, as HotSpot's `replace_all_CTS_matches` does.
#[test]
fn a_copy_made_before_the_enter_is_the_lock() {
    // aload_0; astore_2; aload_0; monitorenter; aload_2; monitorexit; return
    let before = [0x2a, 0x4d, 0x2a, 0xc2, 0x2c, 0xc3, 0xb1];
    assert_eq!(site(&before, &[], &[]), None);
    // The same with a copy of local 1: another object.
    let other = [0x2b, 0x4d, 0x2a, 0xc2, 0x2c, 0xc3, 0xb1];
    assert_eq!(site(&other, &[], &[]), Some(5));
    assert_eq!(site_depth_only(&other), None);
}

/// Wave 7: an exit (or enter) operand nothing ties to a local is refused,
/// not admitted.
#[test]
fn an_unknown_operand_is_refused() {
    // aload_0; monitorenter; aload_0; getfield #1; monitorexit; return
    let field = [0x2a, 0xc2, 0x2a, 0xb4, 0x00, 0x01, 0xc3, 0xb1];
    assert_eq!(site_operands_only(&field), Some(6));
    assert_eq!(site_depth_only(&field), None);
    // invokestatic #1; monitorenter; aload_0; monitorexit; return: the
    // locked value is in no local, so the exit cannot be proved to be it.
    let call = [0xb8, 0x00, 0x01, 0xc2, 0x2a, 0xc3, 0xb1];
    assert_eq!(site_operands_only(&call), Some(5));
    assert_eq!(site_depth_only(&call), None);
}

/// Wave 7: a join keeps only what both paths prove.
#[test]
fn a_join_keeps_an_alias_only_when_both_paths_prove_it() {
    #[rustfmt::skip]
    let mut code = [
        0x2a, 0xc2,             // 0: aload_0 monitorenter
        0x1d, 0x99, 0x00, 0x08, // 2: iload_3 ifeq 11
        0x2a, 0x4d,             // 6: aload_0 astore_2
        0xa7, 0x00, 0x05,       // 8: goto 13
        0x01, 0x4d,             // 11: aconst_null astore_2
        0x2c, 0xc3,             // 13: aload_2 monitorexit
        0xb1,                   // 15: return
    ];
    assert_eq!(site(&code, &[], &[]), Some(14));
    code[11] = 0x2a; // 11: aload_0 astore_2 on both paths
    assert_eq!(site(&code, &[], &[]), None);
}

/// Wave 7: `o = this; while (..) { synchronized (o) { if (c) {} } o = f(); }`
/// -- the lock local changes between iterations; the alias inside the
/// block survives the refinement of the loop header's state.
#[test]
fn a_block_in_a_loop_over_a_changing_local_is_balanced() {
    #[rustfmt::skip]
    let code = [
        0x2a, 0x4c,                   //  0: aload_0 astore_1
        0x2b, 0x59, 0x4d, 0xc2,       //  2: aload_1 dup astore_2 monitorenter
        0x1d, 0x99, 0x00, 0x04,       //  6: iload_3 ifeq 11
        0x00,                         // 10: nop
        0x2c, 0xc3,                   // 11: aload_2 monitorexit
        0xb8, 0x00, 0x01, 0x4c,       // 13: invokestatic astore_1
        0x1d, 0x9a, 0x00, 0x06,       // 17: iload_3 ifne 24
        0xa7, 0xff, 0xed,             // 21: goto 2
        0xb1,                         // 24: return
    ];
    assert_eq!(site(&code, &[], &[]), None);
}

/// Wave 7: an exhausted work budget refuses (the gate fails closed).
#[test]
fn an_exhausted_budget_refuses() {
    let tiny = walk_balance(
        &JAVAC_BLOCK,
        JAVAC_BLOCK.len(),
        &JAVAC_BLOCK_TABLE,
        &[true, true],
        BalanceRules::ALL,
        Some(1),
        None,
    );
    assert_eq!(tiny.map(|(pc, _)| pc), Some(0));
    assert_eq!(site(&JAVAC_BLOCK, &JAVAC_BLOCK_TABLE, &[true, true]), None);
}

/// Wave 10 (review w68 item 1): javac's block with its lock temporary in
/// slot 2000 (`wide astore` / `wide aload`), in a method whose locals
/// reach slot 4999, is balanced; the same with the exit through another
/// high slot is refused.
#[test]
fn a_lock_local_above_slot_1023_is_tracked() {
    #[rustfmt::skip]
    let mut code = [
        0x2a, 0x59, 0xc4, 0x3a, 0x07, 0xd0, 0xc2, // 0: aload_0 dup wide astore 2000; 6: monitorenter
        0x2a, 0xb6, 0x00, 0x02, 0x57,             // 7: aload_0 invokevirtual pop
        0xc4, 0x19, 0x07, 0xd0, 0xc3,             // 12: wide aload 2000; 16: monitorexit
        0xa7, 0x00, 0x16,                         // 17: goto 39
        0xc4, 0x3a, 0x07, 0xd1,                   // 20: wide astore 2001
        0xc4, 0x19, 0x07, 0xd0, 0xc3,             // 24: wide aload 2000; 28: monitorexit
        0x03, 0xc4, 0x36, 0x13, 0x87,             // 29: iconst_0 wide istore 4999
        0xc4, 0x19, 0x07, 0xd1, 0xbf,             // 34: wide aload 2001; 38: athrow
        0xb1,                                     // 39: return
    ];
    let table = [(7, 17, 20), (20, 29, 20)];
    assert_eq!(site(&code, &table, &[true, true]), None);
    // The normal-path exit through slot 2002: another object.
    code[15] = 0xd2;
    assert_eq!(site(&code, &table, &[true, true]), Some(16));
    let rules = BalanceRules::DEPTH_ONLY;
    assert_eq!(
        unbalanced_monitor_site(&code, code.len(), &table, &[true, true], rules),
        None
    );
}

/// Wave 10: a method with 5000 locals whose lock reaches its enter
/// through copies in slots 3000 and 4000 and is released through
/// `aload_0`: the copy chain's source slots are tracked although no
/// monitor op names them.
#[test]
fn a_copy_chain_through_untouched_high_slots_is_followed() {
    #[rustfmt::skip]
    let mut code = [
        0x2a, 0xc4, 0x3a, 0x0b, 0xb8, // 0: aload_0 wide astore 3000
        0xc4, 0x19, 0x0b, 0xb8,       // 5: wide aload 3000
        0xc4, 0x3a, 0x0f, 0xa0,       // 9: wide astore 4000
        0x03, 0xc4, 0x36, 0x13, 0x87, // 13: iconst_0 wide istore 4999
        0xc4, 0x19, 0x0f, 0xa0, 0xc2, // 18: wide aload 4000; 22: monitorenter
        0x2a, 0xc3,                   // 23: aload_0 monitorexit
        0xb1,                         // 25: return
    ];
    assert_eq!(site(&code, &[], &[]), None);
    // Only the slots that can carry the lock are cells.
    let shape = Shape::new(&code, code.len(), &[]);
    let (tracked, enters) = Tracked::new(&code, code.len(), &shape);
    assert_eq!(tracked.slots, vec![0, 3000, 4000]);
    assert_eq!(enters, 1);
    // The chain starts from local 1 instead: another object.
    code[0] = 0x2b;
    assert_eq!(site(&code, &[], &[]), Some(24));
}

/// Wave 10 (review w68 item 3): the allocation-free meet numbers its
/// classes by first occurrence over the locals then the monitors.
#[test]
fn the_meet_is_canonical() {
    let mut s = MeetScratch::default();
    let a = Aliases {
        locals: vec![7, 7, 9],
        monitors: vec![7],
        next: 10,
    };
    let b = Aliases {
        locals: vec![4, 5, 5],
        monitors: vec![4],
        next: 6,
    };
    assert_eq!(meet_into(&a, &b, &mut s), 3);
    assert_eq!(s.out, vec![0, 1, 2, 0]);
    assert_eq!(meet_into(&a, &a, &mut s), 2);
    assert_eq!(s.out, vec![0, 0, 1, 0]);
    let empty = Aliases::empty();
    assert_eq!(meet_into(&empty, &empty, &mut s), 0);
    assert!(s.out.is_empty());
}

/// Wave 10: a 30 KiB block of calls under javac's catch-all (one handler
/// merge per call) passes within the proven budget.
#[test]
fn a_huge_covered_block_passes_within_its_budget() {
    const CALLS: usize = 10_000;
    let mut code = vec![0x2a, 0x59, 0x4c, 0xc2]; // aload_0 dup astore_1 monitorenter
    for _ in 0..CALLS {
        code.extend_from_slice(&[0xb8, 0x00, 0x01]); // invokestatic #1
    }
    let exit = code.len();
    code.extend_from_slice(&[0x2b, 0xc3]); // aload_1 monitorexit
    let goto = code.len();
    code.extend_from_slice(&[0xa7, 0x00, 0x08]); // goto return
    let handler = code.len();
    code.extend_from_slice(&[0x4d, 0x2b, 0xc3, 0x2c, 0xbf]); // astore_2 aload_1 monitorexit aload_2 athrow
    code.push(0xb1);
    assert_eq!(code.len(), handler + 6);
    assert_eq!(exit + 2, goto);
    let table = [(4, goto, handler), (handler, handler + 3, handler)];
    assert_eq!(site(&code, &table, &[true, true]), None);
}

#[test]
fn what_is_not_analysed_is_admitted() {
    assert_eq!(site(&[0x03, 0xac], &[], &[]), None);
    assert_eq!(site(&[0x2a, 0xc2, 0xb1], &[(0, 3, 3)], &[]), None);
}

/// SYNC22-4: the local that holds each open monitor's object at a pc.
#[test]
fn sync22_open_monitor_locals_name_the_slot_holding_each_lock() {
    let at = |code: &[u8], table: &[(usize, usize, usize)], all: &[bool], pc: usize| {
        open_monitor_locals_at(code, code.len(), table, all, pc)
    };
    // javac's block: the lock temporary is a copy of `aload_0`, so slots 0
    // and 1 both name it inside the block, at the exit and in the
    // handler; nothing before or after.
    let javac = |pc| at(&JAVAC_BLOCK, &JAVAC_BLOCK_TABLE, &[true, true], pc);
    assert_eq!(javac(0), Some(vec![]));
    assert_eq!(javac(4), Some(vec![vec![0, 1]]));
    assert_eq!(javac(10), Some(vec![vec![0, 1]]));
    assert_eq!(javac(16), Some(vec![vec![0, 1]]));
    assert_eq!(javac(19), Some(vec![]));
    // The original local overwritten inside the block: only the lock
    // temporary (slot 1) still names the object.
    #[rustfmt::skip]
    let reassigned = [
        0x2a, 0x59, 0x4c, 0xc2, // 0: aload_0 dup astore_1 monitorenter
        0x01, 0x4b,             // 4: aconst_null astore_0
        0x2b, 0xc3,             // 6: aload_1 monitorexit
        0xb1,                   // 8: return
    ];
    assert_eq!(at(&reassigned, &[], &[], 4), Some(vec![vec![0, 1]]));
    assert_eq!(at(&reassigned, &[], &[], 6), Some(vec![vec![1]]));
    // Two nested blocks on DIFFERENT objects (locals 0 and 3): each open
    // monitor names its own object's slots, outermost first.
    #[rustfmt::skip]
    let nested = [
        0x2a, 0x59, 0x4c, 0xc2,       // 0: enter local 0 (copy in 1)
        0x2d, 0x59, 0x4d, 0xc2,       // 4: enter local 3 (copy in 2)
        0x2c, 0xc3,                   // 8: exit local 2
        0xa7, 0x00, 0x08,             // 10: goto 18
        0x4e, 0x2c, 0xc3, 0x2d, 0xbf, // 13: inner handler
        0x2b, 0xc3,                   // 18: exit local 1
        0xa7, 0x00, 0x0a,             // 20: goto 30
        0x3a, 0x04, 0x2b, 0xc3, 0x19, 0x04, 0xbf, // 23: outer handler
        0xb1,                         // 30: return
    ];
    let table = [(8, 10, 13), (13, 16, 13), (4, 20, 23), (23, 27, 23)];
    let all = [true, true, true, true];
    assert_eq!(site(&nested, &table, &all), None, "the fixture is balanced");
    assert_eq!(at(&nested, &table, &all, 4), Some(vec![vec![0, 1]]));
    assert_eq!(at(&nested, &table, &all, 8), Some(vec![vec![0, 1], vec![2, 3]]));
    assert_eq!(at(&nested, &table, &all, 18), Some(vec![vec![0, 1]]));
    assert_eq!(at(&nested, &table, &all, 30), Some(vec![]));
    // No answer: unbalanced (a return with one held), paths that
    // disagree on the depth, a pc past the code, a method with no
    // monitor bytecode.
    assert_eq!(at(&[0x2a, 0xc2, 0xb1], &[], &[], 2), None);
    #[rustfmt::skip]
    let join = [0x1b, 0x99, 0x00, 0x06, 0x2a, 0xc2, 0x2a, 0xc3, 0xb1];
    assert_eq!(at(&join, &[], &[], 7), None);
    assert_eq!(at(&JAVAC_BLOCK, &JAVAC_BLOCK_TABLE, &[true, true], 99), None);
    assert_eq!(at(&[0x03, 0xac], &[], &[], 0), None);
}

#[test]
fn the_depth_answer_is_per_pc_and_lenient() {
    let at = |code: &[u8], pc: usize| block_monitor_depth_at(code, code.len(), &[], &[], pc);
    // exitOnly: nothing of the frame's own is held at its exit.
    assert_eq!(at(&[0x2a, 0xc3, 0xb1], 1), Some(0));
    // exitWrong: one of its own is held at the exit of `b`.
    assert_eq!(at(&[0x2a, 0xc2, 0x2b, 0xc3, 0xb1], 3), Some(1));
    // javac's block: one held at both releases, the handler's included.
    let t = |pc| {
        block_monitor_depth_at(&JAVAC_BLOCK, JAVAC_BLOCK.len(), &JAVAC_BLOCK_TABLE, &[true, true], pc)
    };
    assert_eq!(t(10), Some(1));
    assert_eq!(t(16), Some(1));
    assert_eq!(t(19), Some(0));
    // Paths that disagree answer nothing past the join.
    #[rustfmt::skip]
    let join = [0x1b, 0x99, 0x00, 0x06, 0x2a, 0xc2, 0x2a, 0xc3, 0xb1];
    assert_eq!(at(&join, 7), None);
    // Not analysed: no answer.
    assert_eq!(at(&[0x03, 0xac], 0), None);
}

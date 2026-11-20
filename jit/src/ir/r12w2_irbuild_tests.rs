use super::*;

/// `for (i = 0; i < 10; i++) { <bci 8>; STATIC = 1; }` — the putstatic at
/// pc 10 sits AFTER bci 8 in pc order, and before it in execution order
/// from the second iteration on.
///
/// ```text
///  0: iconst_0          1: istore_0
///  2: iload_0           3: bipush 10       5: if_icmpge 19
///  8: nop               9: iconst_1       10: putstatic #1
/// 13: iinc 0 1         16: goto 2         19: return
/// ```
const LOOP_STORE_AFTER_TRAP: [u8; 20] = [
    0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0e, 0x00, 0x04, 0xb3, 0x00, 0x01, 0x84, 0x00,
    0x01, 0xa7, 0xff, 0xf2, 0xb1,
];

#[test]
fn a_trap_inside_a_loop_covers_the_whole_loop_body() {
    let code = LOOP_STORE_AFTER_TRAP;
    assert_eq!(replay_committed_extent(&code, code.len(), 8), 19);
    // The textual prefix is pure; the loop-closed range is not.
    assert!(!crate::bytecode_commits_side_effect(&code, 8));
    assert!(crate::bytecode_commits_side_effect(&code, 19));
}

#[test]
fn straight_line_code_keeps_the_textual_prefix() {
    let code = LOOP_STORE_AFTER_TRAP;
    // Before the loop header nothing can have come round again.
    assert_eq!(replay_committed_extent(&code, code.len(), 0), 0);
    assert_eq!(replay_committed_extent(&code, code.len(), 1), 1);
    // After the loop: the back edge ends at 19 already.
    assert_eq!(replay_committed_extent(&code, code.len(), 19), 19);
}

/// An inner loop's extent reaches an outer loop's back-edge TARGET, so the
/// outer body joins the range too — the fixpoint, not one pass.
///
/// ```text
///  0: nop   1: nop (outer)   2: nop (inner)   3: iconst_0
///  4: ifeq 2                 7: iconst_0      8: ifeq 1      11: return
/// ```
#[test]
fn nested_loops_close_to_the_outermost_back_edge() {
    let code = [
        0x00, 0x00, 0x00, 0x03, 0x99, 0xff, 0xfe, 0x03, 0x99, 0xff, 0xf9, 0xb1,
    ];
    assert_eq!(replay_committed_extent(&code, code.len(), 3), 11);
    assert_eq!(replay_committed_extent(&code, code.len(), 0), 0);
}

/// `jsr`/`ret` return edges are not statically known: the whole method.
#[test]
fn a_subroutine_answers_the_whole_method() {
    // 0: jsr 5   3: nop   4: return   5: astore_1   6: ret 1
    let code = [0xa8, 0x00, 0x05, 0x00, 0xb1, 0x4c, 0xa9, 0x01];
    assert_eq!(replay_committed_extent(&code, code.len(), 3), code.len());
}

/// `for (i = 0; i < 10; i++) { if (i > 0) <bci 12>; STATIC = 1; }` — the
/// trap is skipped in iteration 0, which commits the putstatic first.
///
/// ```text
///  0: iconst_0          1: istore_0
///  2: iload_0           3: bipush 10       5: if_icmpge 23
///  8: iload_0           9: ifle 13        12: nop
/// 13: iconst_1         14: putstatic #1   17: iinc 0 1
/// 20: goto 2           23: return
/// ```
const LOOP_TRAP_SKIPPED_FIRST: [u8; 24] = [
    0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x12, 0x1a, 0x9e, 0x00, 0x04, 0x00, 0x04, 0xb3,
    0x00, 0x01, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xee, 0xb1,
];

/// The producer's predicate now refuses the trap the textual rule planted.
#[test]
fn the_trap_replay_rule_sees_the_store_iteration_zero_committed() {
    let code = LOOP_TRAP_SKIPPED_FIRST;
    assert!(
        !crate::bytecode_commits_side_effect(&code, 12),
        "the textual prefix is pure, which is what the old rule read"
    );
    assert!(commits_before_first_arrival(&code, code.len(), 12));
    assert_eq!(replay_committed_extent(&code, code.len(), 12), 23);
    let b = IrBuilder::new(0, 1);
    assert!(
        !b.trap_replay_is_safe(&code, code.len(), 12),
        "iteration 0's putstatic would be committed again by a replay"
    );
    // Before the loop a replay is still exact.
    assert!(b.trap_replay_is_safe(&code, code.len(), 1));
}

/// An unconditional trap at the TOP of the loop body fires before anything
/// after it ever ran, so it is still planted: the tighter first-arrival
/// walk is what keeps `for (…) { <indy>; … }` in the optimizing tier.
#[test]
fn an_unconditional_trap_before_the_store_is_still_planted() {
    let code = LOOP_STORE_AFTER_TRAP;
    assert!(!commits_before_first_arrival(&code, code.len(), 8));
    let b = IrBuilder::new(0, 1);
    assert!(b.trap_replay_is_safe(&code, code.len(), 8));
}

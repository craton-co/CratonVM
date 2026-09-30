// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 13, wave 10, lane `exc4`.
//!
//! `ir::replay_committed_extent_with_handlers`: the loop-closed replay extent
//! also follows an exception-table row whose handler lies below its range's
//! end, a way back to the trap that a frame running its own `catch` block
//! (the single-pass tier's local handlers) can take
//! (`docs/internal/fixed-bugs/r13w9-replay5-replay-extent-ignores-exception-edges-FIXED-20260928.md`).
//! javac never places a handler before its range, so these bodies are built
//! by hand.

use cratonvm_jit::ir::{replay_committed_extent, replay_committed_extent_with_handlers};

/// The page's shape: handler 3 protects `[15, 18)` and falls back into the
/// guard at 10 with a forward `goto`.
///
/// ```text
///  0: goto 10          3: astore_1        4: goto 10       7-9: nop
/// 10: iconst_1        11: nop            12: putstatic #1
/// 15: invokestatic #2 18: return
/// ```
const HANDLER_BEFORE_RANGE: [u8; 19] = [
    0xa7, 0x00, 0x0a, 0x4c, 0xa7, 0x00, 0x06, 0x00, 0x00, 0x00, 0x04, 0x00, 0xb3, 0x00, 0x01,
    0xb8, 0x00, 0x02, 0xb1,
];

#[test]
fn the_handler_row_closes_the_extent_over_the_store() {
    let code = HANDLER_BEFORE_RANGE;
    let len = code.len();
    // No backward branch at all: the branch-only extent is the prefix.
    assert_eq!(replay_committed_extent(&code, len, 10), 10);
    assert_eq!(
        replay_committed_extent_with_handlers(&code, len, 10, &[(15, 18, 3)]),
        18,
        "the call at 15 can throw back to 3, so the store at 12 may have run"
    );
    assert!(cratonvm_jit::bytecode_commits_side_effect(&code, 18));
    assert!(!cratonvm_jit::bytecode_commits_side_effect(&code, 10));
}

#[test]
fn forward_and_out_of_range_rows_change_nothing() {
    let code = HANDLER_BEFORE_RANGE;
    let len = code.len();
    // javac's layout: the handler at or past the range end.
    assert_eq!(
        replay_committed_extent_with_handlers(&code, len, 10, &[(12, 15, 18)]),
        10
    );
    // A row past the code (a combined buffer's length bound, a bad table).
    assert_eq!(
        replay_committed_extent_with_handlers(&code, len, 10, &[(30, 40, 3)]),
        10
    );
    // A trap before the handler cannot be reached from it.
    assert_eq!(
        replay_committed_extent_with_handlers(&code, len, 2, &[(15, 18, 3)]),
        2
    );
}

/// The handler row and a branch back edge feed one fixpoint: the handler
/// edge brings pc 4 into the extent, which the `ifeq` at 6 targets.
///
/// ```text
/// 0: nop  1: nop (handler)  2: nop (trap)  3: iconst_0 (protected [3, 4))
/// 4: nop  5: iconst_0       6: ifeq 4      9: return
/// ```
#[test]
fn a_handler_edge_and_a_branch_edge_close_together() {
    let code = [0x00, 0x00, 0x00, 0x03, 0x00, 0x03, 0x99, 0xff, 0xfe, 0xb1];
    let len = code.len();
    assert_eq!(replay_committed_extent(&code, len, 2), 2);
    assert_eq!(
        replay_committed_extent_with_handlers(&code, len, 2, &[(3, 4, 1)]),
        9
    );
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 13, wave 8, lane `irexc2`.
//!
//! `CRATONVM_JIT_OSR_OPAQUE_HANDLER_HEADERS` (default ON): with
//! `CRATONVM_JIT_OSR_OPAQUE_ENTRY` off, `lib.rs` still hands the OSR door's
//! compile of a precise-frame method the arm liveness, with `covered` cleared
//! at every pc no exception handler reaches
//! (`deopt::pcs_reachable_from_handlers`). These tests build that hand-off
//! directly (no environment variable involved) and pin what the builder does
//! with it: an arm at a header the handler falls back into, none at a header
//! it does not reach.

use cratonvm_jit::deopt::pcs_reachable_from_handlers;
use cratonvm_jit::ir::{Graph, IrBuilder, Op};
use cratonvm_jit::regalloc::live_locals_per_pc_all;

/// `static int f(int n) { int s = 0; for (int i = 0; i < n; i++) try { s += i; } catch (..) { continue; } return s; }`,
/// shaped by hand: the counted loop of `r11_osrmerge_opaque_entry_merge.rs`
/// with a handler at 21 (`astore_3; goto 4`) for the range `[9, 13)`. The
/// handler is not normally reachable, so the builder never walks it.
const CATCH_CONTINUES: [u8; 25] = [
    0x03, // 0: iconst_0
    0x3c, // 1: istore_1
    0x03, // 2: iconst_0
    0x3d, // 3: istore_2
    0x1c, // 4: iload_2      <-- header
    0x1a, // 5: iload_0
    0xa2, 0x00, 0x0d, // 6: if_icmpge +13 -> 19
    0x1b, // 9: iload_1
    0x1c, // 10: iload_2
    0x60, // 11: iadd
    0x3c, // 12: istore_1
    0x84, 0x02, 0x01, // 13: iinc 2, 1
    0xa7, 0xff, 0xf4, // 16: goto -12 -> 4
    0x1b, // 19: iload_1
    0xac, // 20: ireturn
    0x4e, // 21: astore_3     <-- handler
    0xa7, 0xff, 0xee, // 22: goto -18 -> 4
];

/// The same loop whose handler leaves it and the method (`astore_3; iload_1;
/// ireturn`).
///
/// Not `astore_3; goto 19`: that `goto` is a BACKWARD branch (22 -> 19), and
/// the builder takes its loop headers from the verifier's backward-branch
/// targets over the whole code (`IrBuilder::build`, `verified.loop_headers()`,
/// filtered to reachable pcs), so pc 19 became a header. It is also
/// handler-reached, so it rightly got an arm, which is what the first version
/// of this test tripped over. A handler with no backward branch keeps the
/// question to the loop header at 4.
const CATCH_LEAVES: [u8; 24] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02, 0x01,
    0xa7, 0xff, 0xf4, 0x1b, 0xac, 0x4e, 0x1b, 0xac,
];

const HANDLER_PC: usize = 21;

/// The `lib.rs` hand-off under the handler-headers switch, spelled out.
fn build_handler_headers_only(code: &[u8]) -> Graph {
    let handlers = [(9usize, 13usize, HANDLER_PC)];
    let (rows, mut covered, words) = live_locals_per_pc_all(code, code.len(), 1, &[], &handlers, 4);
    // Cast: a fixture pc fits u32.
    let reach =
        pcs_reachable_from_handlers(code, &[HANDLER_PC as u32]).expect("the fixture decodes");
    for (pc, c) in covered.iter_mut().enumerate() {
        // Cast: a fixture pc fits u32.
        if reach.binary_search(&(pc as u32)).is_err() {
            *c = false;
        }
    }
    let mut b = IrBuilder::new(1, 4);
    if covered.iter().any(|&c| c) {
        b.set_osr_opaque_liveness(rows, covered, words);
    }
    b.build(code, code.len()).expect("the loop builds")
}

fn arm_headers(g: &Graph) -> Vec<usize> {
    let mut v: Vec<usize> = g
        .nodes
        .iter()
        .filter_map(|n| match n.op {
            Op::OsrEntryFlag { header_bci } => Some(header_bci),
            _ => None,
        })
        .collect();
    v.sort_unstable();
    v
}

#[test]
fn the_fixtures_reach_what_they_say() {
    let continues = pcs_reachable_from_handlers(&CATCH_CONTINUES, &[21]).expect("decodes");
    assert!(
        continues.binary_search(&4).is_ok(),
        "the catch falls back into the loop"
    );
    let leaves = pcs_reachable_from_handlers(&CATCH_LEAVES, &[21]).expect("decodes");
    assert_eq!(leaves, vec![21, 22, 23], "the catch leaves the loop");
}

#[test]
fn a_header_the_handler_reaches_gets_an_arm() {
    let g = build_handler_headers_only(&CATCH_CONTINUES);
    assert_eq!(arm_headers(&g), vec![4]);
}

#[test]
fn a_header_no_handler_reaches_stays_ordinary() {
    let g = build_handler_headers_only(&CATCH_LEAVES);
    assert!(
        !arm_headers(&g).contains(&4),
        "the loop header no handler reaches got an arm"
    );
    assert!(
        arm_headers(&g).is_empty(),
        "an uncovered header must not get an opaque arm"
    );
    assert!(g
        .nodes
        .iter()
        .all(|n| !matches!(n.op, Op::OsrLocal(_) | Op::OsrMemory)));
}

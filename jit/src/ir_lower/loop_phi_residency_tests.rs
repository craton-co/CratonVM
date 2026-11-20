// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 2, lane RESIDENCY (`CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY`): the
//! loop-aware spill cost on a real bytecode loop, through the residency plan
//! and through executed code. The allocator half is pinned on hand-built
//! intervals in `regalloc::loop_residency::tests`.

use super::*;
use crate::ir::IrBuilder;
use crate::ir_schedule;
use cratonvm_types::flags::with_thread_overrides;

/// ```java
/// static int f(int n) {
///     int k = n * 3, m = n + 7;           // two loop invariants (not parameters)
///     int a = 0, b = 1, c = 2;
///     for (int i = 0; i < n; i++) { a += i ^ k; b += a + m; c ^= b + k; }
///     return a + b + c;
/// }
/// ```
///
/// Four per-trip phis (`i`, `a`, `b`, `c`) and two invariants against the
/// five-register narrow GP file: the pressure shape of the `run` kernel.
const PRESSURE_LOOP: [u8; 64] = [
    0x1a, 0x06, 0x68, 0x3c, // 0: k = n * 3
    0x1a, 0x10, 0x07, 0x60, 0x3d, // 4: m = n + 7
    0x03, 0x3e, // 9: a = 0
    0x04, 0x36, 0x04, // 11: b = 1
    0x05, 0x36, 0x05, // 14: c = 2
    0x03, 0x36, 0x06, // 17: i = 0
    0x15, 0x06, 0x1a, 0xa2, 0x00, 0x21, // 20: if (i >= n) goto 56
    0x1d, 0x15, 0x06, 0x1b, 0x82, 0x60, 0x3e, // 26: a += i ^ k
    0x15, 0x04, 0x1d, 0x1c, 0x60, 0x60, 0x36, 0x04, // 33: b += a + m
    0x15, 0x05, 0x15, 0x04, 0x1b, 0x60, 0x82, 0x36, 0x05, // 41: c ^= b + k
    0x84, 0x06, 0x01, // 50: i++
    0xa7, 0xff, 0xdf, // 53: goto 20
    0x1d, 0x15, 0x04, 0x60, 0x15, 0x05, 0x60, 0xac, // 56: return a + b + c
];

/// `f(n)` modelled with Java `int` wrap-around (HotSpot answers the same).
pub(super) const EXPECTED: [(i64, i32); 8] = [
    (0, 3),
    (1, 28),
    (2, 107),
    (5, 621),
    (10, 3231),
    (100, 3_556_516),
    (1000, -1_415_282_113),
    (100_000, -1_170_721_005),
];

pub(super) fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
    // `usize`, so all-zero is a valid value; the loop reaches no helper.
    unsafe { std::mem::zeroed() }
}

pub(super) fn optimized_pressure_loop() -> (Graph, Schedule) {
    let mut graph = IrBuilder::new(1, 7)
        .build(&PRESSURE_LOOP, PRESSURE_LOOP.len())
        .expect("the pressure loop builds");
    crate::ir_optimize::optimize(&mut graph);
    let schedule = ir_schedule::schedule(&graph);
    (graph, schedule)
}

/// [`optimized_pressure_loop`] with the unroller off: ONE loop with the four
/// per-trip phis, the shape the residency rule is pinned on. j27 BUILDFIX:
/// with `CRATONVM_JIT_UNROLL` at its default the loop now unrolls (12 phis,
/// main + remainder), and three carried phis of that shape get no register;
/// see `docs/feature-designs/jit-ir-optimizations.md`.
fn rolled_pressure_loop() -> (Graph, Schedule) {
    with_thread_overrides(&[("CRATONVM_JIT_UNROLL", Some("0"))], optimized_pressure_loop)
}

/// [`optimized_pressure_loop`] with the unroller at its default (ON): the
/// main/post shape, a pc-less main loop plus the original loop as its
/// remainder (`ir_optimize::main_post_unroll_loop`). j27 wave 5 (lane
/// UNROLLRES): it unrolled from the start -- nothing in this crate set
/// `CRATONVM_JIT_UNROLL=0` before BUILDFIX did, so the old `OnceLock` latch
/// held ON -- and the module's "four phis" was never observed: lanes do not
/// build.
fn unrolled_pressure_loop() -> (Graph, Schedule) {
    with_thread_overrides(&[("CRATONVM_JIT_UNROLL", None)], optimized_pressure_loop)
}

/// The main header of a main/post unroll: the partially unrolled header
/// without a bci.
fn main_loop_header(graph: &Graph) -> Option<NodeId> {
    graph
        .partially_unrolled_headers
        .iter()
        .copied()
        .find(|&h| graph.node_opt(h).is_some_and(|n| n.bytecode_pc.is_none()))
}












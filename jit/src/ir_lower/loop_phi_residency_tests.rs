//! j27 wave 2, lane RESIDENCY (`CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY`): the
//! loop-aware spill cost on a real bytecode loop, through the residency plan
//! and through executed code. The allocator half is pinned on hand-built
//! intervals in `regalloc::loop_residency::tests`.

use super::*;
use crate::ir::IrBuilder;
use crate::ir_schedule;
use cratonvm_types::flags::with_thread_overrides;

const SWITCH: &str = "CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY";

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
/// see `docs/internal/jit/j27-loop-phi-residency-misses-unrolled-pressure-loop-FIXED-20261029.md`.
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

/// The allocator switches that change the scan's population and that the j27
/// triage flipped default ON (`WARNING j27-user-flip-20261028`). This file was
/// written while all three were off, so [`under`] pins them to `=0` (unset
/// would now mean ON).
const J27_FLIPPED_POPULATION: [&str; 3] = [
    "CRATONVM_JIT_IR_LS_CALLER_SAVED",
    "CRATONVM_JIT_IR_LS_CALLER_SAVED_SPLITS",
    "CRATONVM_JIT_IR_RESIDENCY_FRAME_PINNED",
];

/// The switch at `on`, every other allocator switch that changes the
/// population off, the linear scan forced on.
fn under<R>(on: bool, f: impl FnOnce() -> R) -> R {
    under_with(on, &[], f)
}

/// [`under`], except that the [`J27_FLIPPED_POPULATION`] keys named in
/// `at_default` stay unset (their j27 default, ON) instead of `=0`.
fn under_with<R>(on: bool, at_default: &[&str], f: impl FnOnce() -> R) -> R {
    let _ls = LsForce::on();
    let mut edits: Vec<(&str, Option<&str>)> = vec![
        (SWITCH, Some(if on { "1" } else { "0" })),
        // Both still opt-in: unset is their OFF.
        ("CRATONVM_JIT_IR_REG_ALLOC", None),
        ("CRATONVM_JIT_IR_GP_WIDE", None),
    ];
    for key in J27_FLIPPED_POPULATION {
        let word = if at_default.contains(&key) { None } else { Some("0") };
        edits.push((key, word));
    }
    with_thread_overrides(&edits, f)
}

/// The carried-and-read `int` phis of the pressure loop that hold no GP
/// register under the switch (the [`J27_FLIPPED_POPULATION`] keys in
/// `at_default` at their default), and the residency they were read from.
fn carried_phis_without_a_register(
    graph: &Graph,
    schedule: &Schedule,
    at_default: &[&str],
) -> (Vec<usize>, RegResidency) {
    let live = crate::regalloc::build_live_model(graph, schedule);
    let carried_phis: Vec<usize> = (0..graph.nodes.len())
        .filter(|&id| graph.nodes[id].op == Op::Phi && graph.nodes[id].ty == IrType::Int)
        .filter(|&id| live.carried_and_read.get(id).copied().unwrap_or(false))
        .collect();
    assert!(
        carried_phis.len() >= 4,
        "fixture: the counter and three accumulators are carried phis; found {carried_phis:?}"
    );
    let residency = under_with(true, at_default, || {
        let plan = plan_slots(graph, schedule, None);
        plan_register_residency(graph, schedule, &plan)
            .expect("the allocation verifies")
            .expect("the file engages on the pressure loop")
    });
    let missing = carried_phis
        .into_iter()
        .filter(|&id| residency.gp_reg_of[id].is_none())
        .collect();
    (missing, residency)
}

/// Under the switch every carried-and-read `int` phi of the loop holds a GP
/// register for its whole range (a split phi is refused, and a refused phi
/// is loaded from and stored to its home on every trip).
#[test]
fn j27_residency_every_carried_phi_of_the_pressure_loop_is_resident() {
    let (graph, schedule) = rolled_pressure_loop();
    let (missing, residency) = carried_phis_without_a_register(&graph, &schedule, &[]);
    assert!(
        missing.is_empty(),
        "carried phis {missing:?} reached no register under the switch; census {:?}",
        residency.phi_census
    );
    // The GC obligation does not move: nothing of type `Ref` is in the file.
    for (id, reg) in residency.gp_reg_of.iter().enumerate() {
        if reg.is_some() {
            assert_ne!(graph.nodes[id].ty, IrType::Ref, "n{id}");
        }
    }
}

/// j27 wave 5 (lane UNROLLRES,
/// `docs/internal/jit/j27-loop-phi-residency-misses-unrolled-pressure-loop-FIXED-20261029.md`):
/// the rule on the ambient (unrolled) fixture. The live model gives each phi
/// one interval from its first incoming edge to its last, so the remainder's
/// phis can overlap the main loop's, and eight carried phis do not fit five
/// registers. The defined loser is the remainder (at most `factor - 1` trips
/// per entry): every carried phi of the MAIN loop holds a register, and a phi
/// without one belongs to the remainder or to the join in front of it.
/// Checked with the j27 population switches pinned off and at their defaults.
#[test]
fn j27_residency_every_carried_phi_of_the_unrolled_main_loop_is_resident() {
    let (graph, schedule) = unrolled_pressure_loop();
    let main = main_loop_header(&graph).expect("fixture: the default unroller builds a main loop");
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    let of_main =
        |id: usize| graph.nodes[id].op == Op::Phi && graph.nodes[id].input_opt(0) == Some(main);
    let main_phis: Vec<usize> = (0..graph.nodes.len())
        .filter(|&id| of_main(id) && graph.nodes[id].ty == IrType::Int)
        .filter(|&id| live.carried_and_read.get(id).copied().unwrap_or(false))
        .collect();
    assert_eq!(
        main_phis.len(),
        4,
        "fixture: the main loop carries the counter and three accumulators; found {main_phis:?}"
    );
    let arms: [&[&str]; 2] = [&[], &J27_FLIPPED_POPULATION];
    let defeated: Vec<String> = arms
        .into_iter()
        .filter_map(|at_default| {
            let (missing, residency) =
                carried_phis_without_a_register(&graph, &schedule, at_default);
            let main_missing: Vec<usize> =
                missing.iter().copied().filter(|&id| of_main(id)).collect();
            (!main_missing.is_empty()).then(|| {
                format!(
                    "{at_default:?} at default: main-loop phis {main_missing:?} without a register \
                     (all carried phis without one: {missing:?}); census {:?}",
                    residency.phi_census
                )
            })
        })
        .collect();
    assert!(defeated.is_empty(), "{}", defeated.join("\n"));
}

/// The executed answer is Java's in both arms; a placement change may only
/// move cost.
#[test]
fn j27_residency_the_pressure_loop_computes_javas_answer_in_both_arms() {
    let run = |on: bool| -> Vec<i32> {
        under(on, || {
            let (graph, schedule) = optimized_pressure_loop();
            let cm = lower(&graph, &schedule, 1, 7, &no_helpers()).expect("lowers");
            EXPECTED
                .iter()
                .map(|&(n, _)| {
                    // SAFETY: one `int` argument, as the fixture's signature;
                    // the body calls no helper and runs on this thread.
                    let raw = unsafe { cm.try_call(&[n]).expect("call") };
                    // The `int` result is the low half of RAX.
                    raw as i32
                })
                .collect()
        })
    };
    let want: Vec<i32> = EXPECTED.iter().map(|&(_, v)| v).collect();
    assert_eq!(run(true), want, "switch on");
    assert_eq!(run(false), want, "switch off");
    // j27 FLIPFIX: and with the flipped population switches at their j27
    // default (ON), the arm a `=1` user now gets.
    let defaults = under_with(true, &J27_FLIPPED_POPULATION, || {
        let (graph, schedule) = optimized_pressure_loop();
        let cm = lower(&graph, &schedule, 1, 7, &no_helpers()).expect("lowers");
        EXPECTED
            .iter()
            .map(|&(n, _)| {
                // SAFETY: as above.
                let raw = unsafe { cm.try_call(&[n]).expect("call") };
                raw as i32
            })
            .collect::<Vec<i32>>()
    });
    assert_eq!(defaults, want, "switch on, j27 defaults");
}

/// j27 FLIPFIX: does the residency rule survive each allocator switch the
/// j27 flip turned default ON? The pressure-loop test above passed before the
/// flip and failed after it (`carried phi n18 reached no register`, census
/// `resident: 4, no_register: 8`) because its `under` pinned
/// `CRATONVM_JIT_IR_LS_CALLER_SAVED` with `None`, which now means ON, and left
/// the other two unpinned. The failure message names the switch(es) that
/// defeat the rule.
///
/// j27 wave 4 (lane RESIDFLIP): no longer ignored. The defeating arm was
/// `CRATONVM_JIT_IR_RESIDENCY_FRAME_PINNED`: its single-use back-edge sources
/// re-arrive at their one read with score 0 and used to tie with the relieved
/// phis, and the tie split a phi (`ls_pick_victim_with`, now an explicit
/// `evictable` key). See
/// `docs/internal/jit/j27-loop-phi-residency-vs-flipped-population-switches-FIXED-20261029.md`.
#[test]
fn j27_residency_survives_each_flipped_population_switch_at_its_default() {
    let (graph, schedule) = rolled_pressure_loop();
    let arms: Vec<Vec<&str>> = J27_FLIPPED_POPULATION
        .iter()
        .map(|&key| vec![key])
        .chain(std::iter::once(J27_FLIPPED_POPULATION.to_vec()))
        .collect();
    let defeated: Vec<String> = arms
        .iter()
        .filter_map(|arm| {
            let (missing, residency) = carried_phis_without_a_register(&graph, &schedule, arm);
            (!missing.is_empty()).then(|| {
                format!(
                    "{arm:?} at default: carried phis {missing:?} without a register; census {:?}",
                    residency.phi_census
                )
            })
        })
        .collect();
    assert!(defeated.is_empty(), "{}", defeated.join("\n"));
}

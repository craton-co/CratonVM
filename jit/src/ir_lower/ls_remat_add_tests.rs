//! j27 wave 4, lane REMAT (`CRATONVM_JIT_IR_LS_REMAT_ADD`): the `x + c`
//! remat through the split plan's planner (`ls_split_transitions`), through
//! the emission on an installed plan (`ls_apply_transitions`), and through
//! executed code. The encoder and the form choice are pinned in
//! `ls_remat_add::tests`, the shape in `regalloc::remat_add::tests`.

use super::loop_phi_residency_tests::{no_helpers, optimized_pressure_loop, EXPECTED};
use super::tests::lowerer_over;
use super::*;
use crate::regalloc::{
    Allocation, PhysReg, PosRange, RematAdd, Segment, SpillEvent, SpillKind,
};
use cratonvm_types::flags::with_thread_overrides;

const SWITCH: &str = "CRATONVM_JIT_IR_LS_REMAT_ADD";
const RBX: u8 = 3;
const R12: u8 = 12;
/// The position the hand-installed plan puts its one transition at.
const AT: usize = 5;

/// `int f(int a) { int s = a + 1; return s + g(s); }`: `s` is `x + c` and is
/// live across the call. Returns `(graph, schedule, a, s, call)`.
fn add_across_a_call() -> (Graph, Schedule, NodeId, NodeId, NodeId) {
    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
    let s = graph.add(Op::Add, IrType::Int, vec![a, one], None);
    let call = graph.add(Op::Call { info_ptr: 0 }, IrType::Int, vec![ctrl, mem, s], None);
    let total = graph.add(Op::Add, IrType::Int, vec![s, call], None);
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, total], None);
    let schedule = crate::ir_schedule::schedule(&graph);
    (graph, schedule, a, s, call)
}

// ── The planner ──────────────────────────────────────────────────────────

/// `s` in RBX, out of it AT the call, reloaded into RBX right after it.
fn reload_after(n: usize, s: NodeId, lo: usize, cut: usize, hi: usize) -> Allocation {
    let rbx = Some(PhysReg::gp(RBX));
    let mut alloc = Allocation {
        segments: vec![Vec::new(); n],
        stack_slot: vec![None; n],
        ..Allocation::default()
    };
    alloc.segments[s as usize] = vec![
        Segment { range: PosRange { lo, hi: cut - 1 }, reg: rbx },
        Segment { range: PosRange { lo: cut, hi: cut }, reg: None },
        Segment { range: PosRange { lo: cut + 1, hi }, reg: rbx },
    ];
    alloc.events = vec![SpillEvent {
        node: s,
        pos: cut + 1,
        kind: SpillKind::Load,
        from: None,
        to: rbx,
    }];
    alloc
}

#[test]
fn j27_remat_the_planner_turns_an_x_plus_c_reload_into_a_remat_only_under_the_switch() {
    let (graph, schedule, a, s, call) = add_across_a_call();
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    let node_pos = ls_node_positions(&schedule, &live);
    let def = live.pos_of[s as usize].expect("s is scheduled");
    let cut = live.pos_of[call as usize].expect("the call is scheduled");
    let range = live.range[s as usize].expect("s wants a location");
    let alloc = reload_after(graph.nodes.len(), s, range.lo, cut, range.hi);
    let reload = |on: Option<&str>| {
        with_thread_overrides(&[(SWITCH, on)], || {
            ls_split_transitions(&graph, &alloc, s as usize, true, def, Some(RBX), &node_pos)
                .expect("placeable")
                .into_iter()
                .find(|(pos, _)| *pos == cut + 1)
                .map(|(_, t)| t)
        })
    };
    assert_eq!(
        reload(Some("1")),
        Some(LsTransition::RematAdd {
            node: s,
            reg: RBX,
            remat: RematAdd { base: a, addend: 1, wide: false },
        })
    );
    assert_eq!(
        reload(None),
        Some(LsTransition::Load { node: s, reg: RBX, gp: true }),
        "off (the default) plans the home load it always did"
    );
}

// ── The emission ─────────────────────────────────────────────────────────

/// A lowerer with `s` managed (one `RematAdd` into RBX at [`AT`]) and `a`
/// assigned R12; `a` is published there when `base_live`. Every value is
/// unnamed by any deopt unless the caller says otherwise.
fn installed(base_reg: u8, base_live: bool) -> (Lowerer<'static>, NodeId, NodeId) {
    let (graph, schedule, a, s, _) = add_across_a_call();
    let plan = plan_slots(&graph, &schedule, None);
    let graph: &'static Graph = Box::leak(Box::new(graph));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan));
    let n = graph.nodes.len();
    let mut at = vec![Vec::new(); AT + 1];
    at[AT].push(LsTransition::RematAdd {
        node: s,
        reg: RBX,
        remat: RematAdd { base: a, addend: 1, wide: false },
    });
    let mut managed = vec![false; n];
    managed[s as usize] = true;
    let mut gp_reg_of = vec![None; n];
    gp_reg_of[a as usize] = Some(base_reg);
    let mut lo = lowerer_over(graph, schedule, plan, 4096);
    lo.set_residency(RegResidency {
        reg_of: vec![None; n],
        gp_reg_of,
        promoted: 1,
        demoted: 0,
        peak_live: 2,
        splits: LsSplitPlan {
            block_start: vec![0],
            managed,
            values: 1,
            transitions: 1,
            at,
            edges: Vec::new(),
        },
        phi_census: PhiCensus::default(),
        poll_fp_reloads: HashMap::new(),
        caller_saved_republish: Vec::new(),
    });
    lo.alloc_slot_checked(s).expect("s has a planned home word");
    lo.deopt_named_reachable = vec![false; n];
    if base_live {
        lo.mark_gp_reg_live(a);
    }
    (lo, a, s)
}

fn apply(lo: &mut Lowerer<'static>) -> Vec<u8> {
    let before = lo.buf.pos();
    lo.ls_apply_transitions(AT);
    lo.buf.as_slice()[before..].to_vec()
}

/// `MOV r64, [RBP - x]` into RBX: the home load.
fn is_home_load_into_rbx(code: &[u8]) -> bool {
    code.len() >= 3 && code[0] == 0x48 && code[1] == 0x8B && (code[2] >> 3) & 7 == RBX
}

#[test]
fn j27_remat_a_resident_base_emits_the_32_bit_lea_and_no_load() {
    let (mut lo, a, s) = installed(R12, true);
    let code = apply(&mut lo);
    assert_eq!(code, [0x41, 0x8D, 0x5C, 0x24, 0x01], "LEA EBX, [R12 + 1]: {code:02x?}");
    assert_eq!(lo.resident_gpr(s), Some(RBX), "s is read out of RBX from here on");
    assert_eq!(lo.resident_gpr(a), Some(R12), "and the base was only read");
    assert_eq!(lo.ls_split_emitted, [0, 1, 0, 0], "counted as a remat");
}

#[test]
fn j27_remat_a_deopt_named_value_keeps_its_home_load() {
    let (mut lo, _, s) = installed(R12, true);
    lo.deopt_named_reachable[s as usize] = true;
    let code = apply(&mut lo);
    assert!(is_home_load_into_rbx(&code), "deopt naming wins: {code:02x?}");
    assert_eq!(lo.resident_gpr(s), Some(RBX));
    assert_eq!(lo.ls_split_emitted, [1, 0, 0, 0], "counted as a load");
}

#[test]
fn j27_remat_a_base_in_no_register_keeps_the_home_load() {
    let (mut lo, _, _) = installed(R12, false);
    let code = apply(&mut lo);
    assert!(is_home_load_into_rbx(&code), "{code:02x?}");
    assert_eq!(lo.ls_split_emitted, [1, 0, 0, 0]);
}

#[test]
fn j27_remat_a_base_in_the_destination_keeps_the_home_load() {
    // The plan is overwriting the base's register with `s`: reading it would
    // be reading the very register being written.
    let (mut lo, _, _) = installed(RBX, true);
    let code = apply(&mut lo);
    assert!(is_home_load_into_rbx(&code), "{code:02x?}");
    assert_eq!(lo.ls_split_emitted, [1, 0, 0, 0]);
}

// ── Executed ─────────────────────────────────────────────────────────────

/// The RESIDENCY pressure loop (`m = n + 7` is an `x + c` invariant read every
/// trip, `n` the loop bound) computes Java's answer with the remat on and off,
/// under both victim rules and with the splits on.
#[test]
fn j27_remat_the_pressure_loop_computes_javas_answer_in_every_arm() {
    let want: Vec<i32> = EXPECTED.iter().map(|&(_, v)| v).collect();
    for remat in [None, Some("1")] {
        for loop_phi in [None, Some("1")] {
            let got: Vec<i32> = {
                let _ls = LsForce::on();
                with_thread_overrides(
                    &[
                        (SWITCH, remat),
                        ("CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY", loop_phi),
                        ("CRATONVM_JIT_IR_LS_SPLITS", Some("1")),
                    ],
                    || {
                        let (graph, schedule) = optimized_pressure_loop();
                        let cm =
                            lower(&graph, &schedule, 1, 7, &no_helpers()).expect("lowers");
                        EXPECTED
                            .iter()
                            .map(|&(n, _)| {
                                // SAFETY: one `int` argument, as the fixture's
                                // signature; the body calls no helper.
                                let raw = unsafe { cm.try_call(&[n]).expect("call") };
                                raw as i32
                            })
                            .collect()
                    },
                )
            };
            assert_eq!(got, want, "remat={remat:?} loop_phi={loop_phi:?}");
        }
    }
}

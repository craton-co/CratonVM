//! JIT round 19 proposal wave 11, lane lv11: the split gate on the
//! `R19Lv10SplitGate.pressure` shape (LV10-6 / i64-L2,
//! `CRATONVM_JIT_IR_LS_SPLITS_CARRIED_FIRST`), the Ref-carrying fixture
//! (LV10-4), and the token-read guard's strict test mode, debug default and
//! point-range rule (TOK10-1/2/3).

use super::*;
use crate::ir::IrBuilder;
use crate::ir_schedule;
use cratonvm_types::flags::with_thread_overrides;

const SPLITS: &str = "CRATONVM_JIT_IR_LS_SPLITS";
const PHI_RANGES: &str = "CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES";
const EDGE_RES: &str = "CRATONVM_JIT_IR_LS_EDGE_RESOLUTION";
const CARRIED_FIRST: &str = "CRATONVM_JIT_IR_LS_SPLITS_CARRIED_FIRST";
const GUARD: &str = "CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD";
const GUARD_DEBUG: &str = "CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD_DEBUG_DEFAULT";
const GUARD_POINT: &str = "CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD_POINT_RANGE";
const SLOTS_TOKEN: &str = "CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES";

/// `static long pressure(int n)` of `R19Lv10SplitGate`: eight `long`
/// accumulators carried around a loop whose body divides.
pub(crate) fn pressure_loop() -> (Graph, Schedule) {
    let code: [u8; 162] = [
        0x10, 0x01, 0x85, 0x37, 0x01, // 0: a = 1L
        0x10, 0x02, 0x85, 0x37, 0x03, // 5: b = 2L
        0x10, 0x03, 0x85, 0x37, 0x05, // 10: c = 3L
        0x10, 0x05, 0x85, 0x37, 0x07, // 15: d = 5L
        0x10, 0x07, 0x85, 0x37, 0x09, // 20: e = 7L
        0x10, 0x0b, 0x85, 0x37, 0x0b, // 25: f = 11L
        0x10, 0x0d, 0x85, 0x37, 0x0d, // 30: g = 13L
        0x10, 0x11, 0x85, 0x37, 0x0f, // 35: h = 17L
        0x04, 0x36, 0x11, // 40: i = 1
        0x15, 0x11, 0x1a, 0xa2, 0x00, 0x5c, // 43: if (i >= n) goto 138
        0x15, 0x11, 0x10, 0x1f, 0x68, // 49: i * 31
        0x15, 0x11, 0x10, 0x07, 0x70, 0x04, 0x60, // 54: i % 7 + 1
        0x6c, 0x36, 0x12, // 61: q = .. / ..
        0x16, 0x01, 0x15, 0x12, 0x85, 0x61, 0x37, 0x01, // 64: a += q
        0x16, 0x03, 0x16, 0x01, 0x83, 0x37, 0x03, // 72: b ^= a
        0x16, 0x05, 0x16, 0x03, 0x06, 0x7d, 0x61, 0x37, 0x05, // 79: c += b >>> 3
        0x16, 0x07, 0x16, 0x05, 0x06, 0x85, 0x69, 0x61, 0x37, 0x07, // 88: d += c * 3
        0x16, 0x09, 0x16, 0x07, 0x83, 0x37, 0x09, // 98: e ^= d
        0x16, 0x0b, 0x16, 0x09, 0x08, 0x7d, 0x61, 0x37, 0x0b, // 105: f += e >>> 5
        0x16, 0x0d, 0x16, 0x0b, 0x61, 0x37, 0x0d, // 114: g += f
        0x16, 0x0f, 0x16, 0x0d, 0x15, 0x12, 0x85, 0x61, 0x83, 0x37, 0x0f, // 121: h ^= g + q
        0x84, 0x11, 0x01, // 132: i++
        0xa7, 0xff, 0xa4, // 135: goto 43
        0x16, 0x01, 0x16, 0x03, 0x61, 0x16, 0x05, 0x61, 0x16, 0x07, 0x61, // 138: a+b+c+d
        0x16, 0x09, 0x61, 0x16, 0x0b, 0x61, 0x16, 0x0d, 0x61, 0x16, 0x0f, 0x61, // +e+f+g+h
        0xad, // 161: lreturn
    ];
    let mut builder = IrBuilder::new(1, 19);
    builder.set_param_types(&[IrType::Int]);
    let graph = builder
        .build(&code, code.len())
        .expect("the pressure loop builds");
    let schedule = ir_schedule::schedule(&graph);
    (graph, schedule)
}

/// LV10-4: `static int f(Object[] a) { int s = 0; for (int i = 0; i <
/// a.length; i++) s += h(a[i]); return s; }` -- a loop over an object array
/// whose element (a `Ref` the colouring gives a home) is live into a call.
pub(crate) fn ref_array_call_loop() -> (Graph, Schedule) {
    let code: [u8; 27] = [
        0x03, 0x3c, // 0: s = 0
        0x03, 0x3d, // 2: i = 0
        0x1c, 0x2a, 0xbe, // 4: i, a.length
        0xa2, 0x00, 0x12, // 7: if_icmpge 25
        0x1b, // 10: iload_1 (s)
        0x2a, 0x1c, 0x32, // 11: a[i]
        0xb8, 0x00, 0x02, // 14: invokestatic h(Object)I
        0x60, 0x3c, // 17: s += ..
        0x84, 0x02, 0x01, // 19: i++
        0xa7, 0xff, 0xee, // 22: goto 4
        0x1b, 0xac, // 25: return s
    ];
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "pkg/Lv11",
        method_name: "h",
        descriptor: "(Ljava/lang/Object;)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut builder = IrBuilder::new(1, 3);
    builder.set_param_types(&[IrType::Ref]);
    let mut invoke_info = HashMap::new();
    invoke_info.insert(
        14usize,
        (info as *const JitInvokeInfo as usize, 1usize, b'I'),
    );
    builder.set_invoke_info(invoke_info);
    let graph = builder
        .build(&code, code.len())
        .expect("the object-array call loop builds");
    let schedule = ir_schedule::schedule(&graph);
    (graph, schedule)
}

/// JIT round 25 item 1b: the caller-saved switches. These tests pin the
/// split gate's shapes with both OFF (`residency_under`); with them on, the
/// block-local long arithmetic of the pressure loop's body (`i * 31`,
/// `i % 7 + 1`, `q`) takes whole-range R8-R11 registers out of the scan
/// instead of split callee-saved segments, which is a different allocation by
/// design, not a split-placement regression. The arm-on twin is
/// `r25_caller_saved_takes_the_split_population_out_of_the_scan`.
const CS: &str = "CRATONVM_JIT_IR_LS_CALLER_SAVED";
const CS_SPLITS: &str = "CRATONVM_JIT_IR_LS_CALLER_SAVED_SPLITS";
/// JIT round 25 wave 7 (W7-A): the register-allocator switch admits more
/// single-use values to the scan, a different allocation by design; pinned
/// off by [`residency_under`] like the two above.
const REG_ALLOC: &str = "CRATONVM_JIT_IR_REG_ALLOC";

/// The plan, the residency and the `ranges_agree` gate under `edits`, the
/// linear scan forced on, the caller-saved switches off unless `edits` names
/// them.
fn residency_under(
    graph: &Graph,
    schedule: &Schedule,
    edits: &[(&str, Option<&str>)],
) -> (SlotPlan, RegResidency, bool) {
    let mut all: Vec<(&str, Option<&str>)> = Vec::with_capacity(edits.len() + 3);
    // The caller-saved pair is default ON since j27 2026-10-28, so OFF is
    // `=0`; `REG_ALLOC` is still opt-in, so unset is its OFF.
    for (key, off) in [(CS, Some("0")), (CS_SPLITS, Some("0")), (REG_ALLOC, None)] {
        if !edits.iter().any(|(k, _)| *k == key) {
            all.push((key, off));
        }
    }
    all.extend_from_slice(edits);
    residency_under_exact(graph, schedule, &all)
}

/// [`residency_under`] with exactly `edits` (no caller-saved default).
fn residency_under_exact(
    graph: &Graph,
    schedule: &Schedule,
    edits: &[(&str, Option<&str>)],
) -> (SlotPlan, RegResidency, bool) {
    let _ls = LsForce::on();
    with_thread_overrides(edits, || {
        let plan = plan_slots(graph, schedule, None);
        let live = crate::regalloc::build_live_model(graph, schedule);
        let agree = slot_and_live_ranges_agree(&plan, &live);
        let r = plan_register_residency(graph, schedule, &plan)
            .expect("no bailout")
            .expect("the file engages on the pressure loop");
        (plan, r, agree)
    })
}

const ARM_A: [(&str, Option<&str>); 4] = [
    (SPLITS, None),
    (PHI_RANGES, Some("0")),
    (EDGE_RES, None),
    (CARRIED_FIRST, None),
];
const ARM_B: [(&str, Option<&str>); 4] = [
    (SPLITS, Some("1")),
    (PHI_RANGES, Some("0")),
    (EDGE_RES, None),
    (CARRIED_FIRST, None),
];
const ARM_C: [(&str, Option<&str>); 4] = [
    (SPLITS, Some("1")),
    (PHI_RANGES, Some("1")),
    (EDGE_RES, None),
    (CARRIED_FIRST, None),
];
const ARM_D: [(&str, Option<&str>); 4] = [
    (SPLITS, Some("1")),
    (PHI_RANGES, Some("1")),
    (EDGE_RES, Some("1")),
    (CARRIED_FIRST, None),
];
const ARM_E: [(&str, Option<&str>); 4] = [
    (SPLITS, Some("1")),
    (PHI_RANGES, Some("1")),
    (EDGE_RES, None),
    (CARRIED_FIRST, Some("1")),
];

/// Whole-range GP residents (not split-managed), sorted by id.
fn whole_gp(r: &RegResidency) -> Vec<(usize, u8)> {
    r.gp_reg_of
        .iter()
        .enumerate()
        .filter(|&(id, _)| !r.splits.managed.get(id).copied().unwrap_or(false))
        .filter_map(|(id, g)| g.map(|g| (id, g)))
        .collect()
}

/// i64-L2 on the probe's own shape: the gate is shut by φ ranges alone, the
/// φ edge rule opens it, and the frame does not move.
#[test]
fn lv11_the_pressure_gate_is_shut_by_phi_ranges_alone() {
    let (graph, schedule) = pressure_loop();
    let (plan_b, r_b, agree_b) = residency_under(&graph, &schedule, &ARM_B);
    assert!(!agree_b, "without the φ rule the gate is shut");
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    let mut differing = 0usize;
    for (id, (p, l)) in plan_b.range.iter().zip(live.range.iter()).enumerate() {
        let same = match (p, l) {
            (Some(p), Some(l)) => p.lo == l.lo && p.hi == l.hi,
            (None, None) => true,
            _ => false,
        };
        if !same {
            differing += 1;
            assert!(
                matches!(graph.nodes[id].op, Op::Phi),
                "n{id} {:?}: only φ ranges differ",
                graph.nodes[id].op
            );
        }
    }
    assert!(differing > 0);
    assert_eq!(r_b.splits.values, 0, "a shut gate places no split value");

    let (plan_c, _, agree_c) = residency_under(&graph, &schedule, &ARM_C);
    assert!(
        agree_c,
        "the φ edge rule opens the gate on the probe's shape"
    );
    assert_eq!(plan_b.slots, plan_c.slots, "same frame");
    assert_eq!(plan_b.node_color, plan_c.node_color, "same colouring");
    assert!(
        plan_c.peak_live <= plan_b.peak_live + 1,
        "peak_live {} -> {}",
        plan_b.peak_live,
        plan_c.peak_live
    );
}

/// The LV10-6 answer: opening the gate admits split values whose registers
/// the loop-carried reservation can no longer hand out, so carried
/// accumulators lose their whole-loop registers -- and the edge resolution
/// half (arm D) changes nothing on this shape, so the host's C-vs-D gap
/// (1357 vs 881 us) was not the code.
#[test]
fn lv11_opening_the_gate_trades_carried_registers_for_split_values() {
    let (graph, schedule) = pressure_loop();
    let (_, r_a, _) = residency_under(&graph, &schedule, &ARM_A);
    let (_, r_c, _) = residency_under(&graph, &schedule, &ARM_C);
    let (_, r_d, _) = residency_under(&graph, &schedule, &ARM_D);
    assert_eq!(r_a.splits.values, 0);
    assert!(r_c.splits.values > 0, "the open gate places split values");
    let (a, c) = (whole_gp(&r_a), whole_gp(&r_c));
    assert!(
        c.len() < a.len(),
        "split values cost whole-range registers: {a:?} -> {c:?}"
    );
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    let lost_carried = a
        .iter()
        .filter(|&&(id, _)| live.carried_and_read.get(id).copied().unwrap_or(false))
        .filter(|&&(id, _)| r_c.gp_reg_of[id].is_none())
        .count();
    assert!(lost_carried > 0, "a carried value lost its register");
    assert_eq!(r_c.gp_reg_of, r_d.gp_reg_of, "arm D = arm C here");
    assert_eq!(r_c.splits.values, r_d.splits.values);
    assert_eq!(r_c.splits.transitions, r_d.splits.transitions);
}

/// The arm-on twin of the test above (JIT round 25 item 1b): with
/// `CRATONVM_JIT_IR_LS_CALLER_SAVED` on, values that are block-local and
/// cross no call leave the scan for R8-R11, so the open gate has at most as
/// many split values, every one it lost is accounted for by a caller-saved
/// assignment, the carried accumulators lose no more registers than with the
/// switch off, and the pressure loop still lowers in that arm.
#[test]
fn r25_caller_saved_takes_the_split_population_out_of_the_scan() {
    let (graph, schedule) = pressure_loop();
    let with_cs = |arm: &[(&'static str, Option<&'static str>)]| {
        let mut edits: Vec<(&str, Option<&str>)> = arm.to_vec();
        edits.push((CS, Some("1")));
        edits.push((CS_SPLITS, Some("0")));
        edits
    };
    let (_, r_c, _) = residency_under(&graph, &schedule, &ARM_C);
    let (_, r_c_on, _) = residency_under(&graph, &schedule, &with_cs(&ARM_C));
    let caller_saved = r_c_on
        .gp_reg_of
        .iter()
        .filter(|r| r.is_some_and(|r| IR_LS_CALLER_SAVED_GPRS.contains(&r)))
        .count();
    assert!(
        r_c_on.splits.values <= r_c.splits.values,
        "pinning block-local values out can only shrink the split population: {} -> {}",
        r_c.splits.values,
        r_c_on.splits.values
    );
    assert!(
        r_c.splits.values - r_c_on.splits.values <= caller_saved,
        "every split value the arm lost is a caller-saved one now \
         ({} -> {} splits, {caller_saved} caller-saved)",
        r_c.splits.values,
        r_c_on.splits.values
    );
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    let (_, r_a, _) = residency_under(&graph, &schedule, &ARM_A);
    let lost = |r: &RegResidency| {
        whole_gp(&r_a)
            .iter()
            .filter(|&&(id, _)| live.carried_and_read.get(id).copied().unwrap_or(false))
            .filter(|&&(id, _)| r.gp_reg_of[id].is_none())
            .count()
    };
    assert!(
        lost(&r_c_on) <= lost(&r_c),
        "the caller-saved file does not cost carried registers: {} vs {}",
        lost(&r_c_on),
        lost(&r_c)
    );
    for (id, reg) in r_c_on.gp_reg_of.iter().enumerate() {
        if reg.is_some_and(|r| IR_LS_CALLER_SAVED_GPRS.contains(&r)) {
            assert!(matches!(graph.nodes[id].ty, IrType::Int | IrType::Long));
            assert!(!live.carried_and_read.get(id).copied().unwrap_or(false));
        }
    }
    let _ls = LsForce::on();
    let got = with_thread_overrides(&with_cs(&ARM_C), || lowered(&graph, &schedule, 1, 19));
    assert!(got.is_some(), "arm C with the caller-saved file compiles the pressure loop");
}

/// `CRATONVM_JIT_IR_LS_SPLITS_CARRIED_FIRST`: the split values yield, and the
/// register assignment is the gate-shut one again. Inert without splits.
#[test]
fn lv11_carried_first_gives_the_carried_registers_back() {
    let (graph, schedule) = pressure_loop();
    let (_, r_a, _) = residency_under(&graph, &schedule, &ARM_A);
    let (_, r_e, agree_e) = residency_under(&graph, &schedule, &ARM_E);
    assert!(agree_e, "the gate is open in arm E too");
    assert_eq!(r_e.splits.values, 0, "every split value here yields");
    assert_eq!(r_e.gp_reg_of, r_a.gp_reg_of, "the gate-shut assignment");
    assert_eq!(r_e.reg_of, r_a.reg_of);
    let off_splits = [
        (SPLITS, None),
        (PHI_RANGES, Some("1")),
        (EDGE_RES, None),
        (CARRIED_FIRST, Some("1")),
    ];
    let (_, r_off, _) = residency_under(&graph, &schedule, &off_splits);
    assert_eq!(r_off.gp_reg_of, r_a.gp_reg_of, "no split, nothing to yield");
    assert!(!with_thread_overrides(
        &[(CARRIED_FIRST, None)],
        ir_ls_splits_carried_first_enabled
    ));
    assert!(with_thread_overrides(
        &[(CARRIED_FIRST, Some("1"))],
        ir_ls_splits_carried_first_enabled
    ));
}

/// The minimal `lower_inner` call the token-scope tests use.
fn lowered(graph: &Graph, schedule: &Schedule, params: usize, locals: usize) -> Option<Vec<u8>> {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the bodies are compared, never run.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    lower_inner(
        graph,
        schedule,
        params,
        locals,
        &helpers,
        &HashMap::new(),
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &crate::ir::IrInlineFrameSites::default(),
    )
    .map(|cm| cm.code_bytes().to_vec())
}

/// End to end: whatever the split arms do to the pressure loop, a compile the
/// gate-shut arm accepts is accepted in every split arm too (the lowering's
/// own `verify_allocation` / colouring checks run inside).
#[test]
fn lv11_the_pressure_loop_lowers_in_every_split_arm() {
    let (graph, schedule) = pressure_loop();
    let _ls = LsForce::on();
    let base = with_thread_overrides(&ARM_A, || lowered(&graph, &schedule, 1, 19));
    assert!(
        base.is_some(),
        "the gate-shut arm compiles the pressure loop"
    );
    for (name, arm) in [("C", ARM_C), ("D", ARM_D), ("E", ARM_E)] {
        let got = with_thread_overrides(&arm, || lowered(&graph, &schedule, 1, 19));
        assert!(got.is_some(), "arm {name} refuses what arm A accepts");
    }
}

/// LV10-4: a stock fixture that reaches the dead-home value ranges -- a real
/// coloured `Ref` (the array element, live into the call), not a retyped
/// `Int`.
#[test]
fn lv11_the_ref_array_call_loop_reaches_the_value_range_fixed_point() {
    let (graph, schedule) = ref_array_call_loop();
    assert!(
        graph.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })),
        "the loop calls"
    );
    let plan = plan_slots(&graph, &schedule, None);
    let coloured_refs: Vec<usize> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(id, n)| {
            n.ty == IrType::Ref
                && !matches!(n.op, Op::Param(_))
                && plan.node_color.get(*id).copied().flatten().is_some()
        })
        .map(|(id, _)| id)
        .collect();
    assert!(!coloured_refs.is_empty(), "a computed Ref gets a home");
    let mut trace = crate::regalloc::LsLegacyRun::default();
    let _ = value_use_ranges_with(
        &graph,
        &schedule,
        &plan,
        SLOT_PLAN_MAX_ITERATIONS,
        crate::regalloc::LivenessFixedPoint::RoundRobin,
        Some(&mut trace),
    );
    assert!(
        trace.ran,
        "the value-range fixed point runs on a stock graph"
    );
    verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");
}

/// TOK10-3: the exemption rule. Identity first; a range that does not cover
/// the reader's position takes it away; no range or no position keeps it.
#[test]
fn lv11_token_exempt_at_asks_identity_then_the_point() {
    let r = |lo, hi| Some(LiveRange { lo, hi });
    let pinned = Some(SlotClass::Pinned);
    let prim = Some(SlotClass::Prim);
    // Not exempt by identity: never exempt.
    assert!(!token_exempt_at(prim, false, r(0, 100), Some(5)));
    assert!(!token_exempt_at(None, false, None, None));
    // By identity, no point rule (switch off: `reader_pos` None).
    assert!(token_exempt_at(pinned, false, r(10, 20), None));
    assert!(token_exempt_at(prim, true, r(10, 20), None));
    // The point rule, endpoint-inclusive.
    for (pos, want) in [(9, false), (10, true), (15, true), (20, true), (21, false)] {
        assert_eq!(
            token_exempt_at(pinned, false, r(10, 20), Some(pos)),
            want,
            "pinned at {pos}"
        );
        assert_eq!(
            token_exempt_at(prim, true, r(10, 20), Some(pos)),
            want,
            "named at {pos}"
        );
    }
    // No range: nothing contradicts the identity answer.
    assert!(token_exempt_at(prim, true, None, Some(3)));
}

/// The three new switches are off by default; TOK10-2's debug default turns
/// the guard on in this (debug) test build unless the guard says `0`.
#[test]
fn lv11_the_token_switches_read_as_documented() {
    let clean = [
        (GUARD, None),
        (GUARD_DEBUG, None),
        (GUARD_POINT, None),
        (SLOTS_TOKEN, None),
    ];
    with_thread_overrides(&clean, || {
        assert!(!ir_lower_token_read_guard_enabled());
        assert!(!ir_lower_token_read_guard_debug_default_enabled());
        assert!(!ir_lower_token_read_guard_point_range_enabled());
    });
    let debug_on = [(GUARD, None), (GUARD_DEBUG, Some("1")), (SLOTS_TOKEN, None)];
    with_thread_overrides(&debug_on, || {
        assert_eq!(ir_lower_token_read_guard_enabled(), cfg!(debug_assertions));
    });
    let killed = [
        (GUARD, Some("0")),
        (GUARD_DEBUG, Some("1")),
        (SLOTS_TOKEN, None),
    ];
    with_thread_overrides(&killed, || {
        assert!(
            !ir_lower_token_read_guard_enabled(),
            "`0` kills the debug default"
        );
    });
    with_thread_overrides(&[(GUARD, Some("strict"))], || {
        assert!(
            ir_lower_token_read_guard_enabled(),
            "`strict` is an on value"
        );
        assert!(token_read_guard_strict());
    });
    with_thread_overrides(
        &[(GUARD, Some("1"))],
        || assert!(!token_read_guard_strict()),
    );
    with_thread_overrides(&[(GUARD_POINT, Some("1"))], || {
        assert!(ir_lower_token_read_guard_point_range_enabled())
    });
}

/// TOK10-1: under `strict` a refusal is a panic naming the funnel.
#[test]
#[should_panic(expected = "token-read guard (strict test mode)")]
fn lv11_the_strict_mode_turns_a_refusal_into_a_panic() {
    with_thread_overrides(&[(GUARD, Some("strict"))], || {
        let mut lo = super::tests::lowerer_with_resident_xmm(4096, None);
        lo.token_only_inputs = vec![(0, 1, false)];
        let _ = lo.slot_of_checked(0);
    });
}

/// TOK10-3 end to end: with the untrimmed (default) plans every identity-
/// exempt home covers its reader, so the point rule refuses nothing more:
/// guard-on compiles are accepted alike and are the same size with and
/// without it.
#[test]
fn lv11_the_point_range_rule_leaves_untrimmed_compiles_unchanged() {
    let sum_loop: [u8; 21] = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02,
        0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac,
    ];
    let mut cases: Vec<(&str, Graph, Schedule, usize, usize)> = Vec::new();
    let (g, s) = pressure_loop();
    cases.push(("pressure", g, s, 1, 19));
    let (g, s) = ref_array_call_loop();
    cases.push(("ref-array-call", g, s, 1, 3));
    let mut builder = IrBuilder::new(1, 3);
    builder.set_param_types(&[IrType::Int]);
    let g = builder.build(&sum_loop, sum_loop.len()).expect("sum loop");
    let s = ir_schedule::schedule(&g);
    cases.push(("sum-loop", g, s, 1, 3));
    let mut compared = 0usize;
    for (name, graph, schedule, params, locals) in &cases {
        let run = |point: Option<&str>| {
            with_thread_overrides(&[(GUARD, Some("1")), (GUARD_POINT, point)], || {
                lowered(graph, schedule, *params, *locals)
            })
        };
        // Compared by verdict and length: a body with a deopt pad embeds heap
        // addresses (its frame-state tables), which differ between two
        // compiles of one graph.
        let off = run(None).map(|b| b.len());
        let on = run(Some("1")).map(|b| b.len());
        assert_eq!(
            on, off,
            "{name}: the point rule changed the code size or the verdict"
        );
        compared += usize::from(off.is_some());
    }
    assert!(
        compared >= 1,
        "something compiled ({compared}/{})",
        cases.len()
    );
}

// ── JIT round 19 proposal wave 12, lane lv12: LV11-1..4 ──────────────────

const PRICED: &str = "CRATONVM_JIT_IR_LS_SPLITS_CARRIED_PRICED";
const NO_GAIN: &str = "CRATONVM_JIT_IR_LS_SPLITS_REFUSE_NO_GAIN";
const PREPIN_REFS: &str = "CRATONVM_JIT_IR_LS_PREPIN_REFS_UNDER_SPLITS";
const POINT_CHECK: &str = "CRATONVM_JIT_IR_DEOPT_POINT_RANGE_CHECK";

/// LV11-3: `REFUSE_NO_GAIN` refuses the split value that is only "reload,
/// then read once"; the other keeps a positive benefit.
#[test]
fn lv12_refuse_no_gain_refuses_pressures_reload_only_splits() {
    // JIT r23 wave 8c (lane W8-L4): scheduled with
    // `CRATONVM_JIT_IR_SINK_COMPARE_TO_GUARD=0`, the shape this census was
    // written against. With the switch on, the `idiv`'s zero guard compare
    // (`i % 7 + 1 != 0`) leaves the header block -- where schedule-early put it
    // beside the floating division -- for the guard's own block, and fuses
    // with the guard. Its boolean no longer crosses the exit test, but the
    // divisor `i % 7 + 1` now has a second read in the body: the very value
    // this test names "defined in memory, loaded, read once" changes shape,
    // and the split population with it. The fixture moved; the rule under test
    // did not.
    let (graph, schedule) = with_thread_overrides(
        &[("CRATONVM_JIT_IR_SINK_COMPARE_TO_GUARD", Some("0"))],
        pressure_loop,
    );
    let (_, r_a, _) = residency_under(&graph, &schedule, &ARM_A);
    let (_, r_c, _) = residency_under(&graph, &schedule, &ARM_C);
    assert!(r_c.splits.values > 0);
    let arm = [
        (SPLITS, Some("1")),
        (PHI_RANGES, Some("1")),
        (EDGE_RES, None),
        (CARRIED_FIRST, None),
        (NO_GAIN, Some("1")),
    ];
    let (_, r, _) = residency_under(&graph, &schedule, &arm);
    // `i % 7 + 1` (defined in memory, loaded, read once) is refused; `q`
    // keeps a positive benefit by this measure and stays split -- so LV11-3
    // alone does not give the carried set its registers back (the priced
    // form below does).
    assert_eq!(
        r.splits.values + 1,
        r_c.splits.values,
        "one reload-only split refused"
    );
    assert_ne!(
        r.gp_reg_of, r_a.gp_reg_of,
        "the survivor still borrows a carried register"
    );
    assert!(!with_thread_overrides(
        &[(NO_GAIN, None)],
        ir_ls_splits_refuse_no_gain_enabled
    ));
}

/// LV11-2: the priced form yields where the split value buys nothing
/// (benefit 0 against the displaced carried weight), i.e. here exactly as
/// `CARRIED_FIRST` does.
#[test]
fn lv12_carried_priced_yields_a_split_that_buys_nothing() {
    let (graph, schedule) = pressure_loop();
    let (_, r_a, _) = residency_under(&graph, &schedule, &ARM_A);
    let arm = [
        (SPLITS, Some("1")),
        (PHI_RANGES, Some("1")),
        (EDGE_RES, None),
        (CARRIED_FIRST, None),
        (PRICED, Some("1")),
    ];
    let (_, r, _) = residency_under(&graph, &schedule, &arm);
    assert_eq!(r.splits.values, 0);
    assert_eq!(r.gp_reg_of, r_a.gp_reg_of);
    assert!(!with_thread_overrides(
        &[(PRICED, None)],
        ir_ls_splits_carried_priced_enabled
    ));
}

/// The benefit measure itself: a register held over every read is worth the
/// reads' loop weights; a `Load` before each read cancels it; reads outside
/// any register segment count nothing.
#[test]
fn lv12_split_register_benefit_counts_register_reads_minus_loads() {
    use crate::regalloc::{PhysReg, PosRange, Segment};
    let (graph, schedule) = pressure_loop();
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    let (id, uses) = live
        .uses
        .iter()
        .enumerate()
        .find(|(_, u)| u.len() >= 2)
        .map(|(id, u)| (id, u.clone()))
        .expect("a value read twice");
    let (lo, hi) = (uses[0], *uses.last().unwrap());
    let reg = Segment {
        range: PosRange { lo, hi },
        reg: Some(PhysReg::gp(3)),
    };
    let mem = Segment {
        range: PosRange { lo, hi },
        reg: None,
    };
    let held = split_register_benefit(&live, id, &[reg], &[]);
    assert!(held >= uses.len() as i128, "{held}");
    let loads: Vec<(usize, LsTransition)> = uses
        .iter()
        .map(|&u| {
            (
                u,
                LsTransition::Load {
                    node: id as NodeId,
                    reg: 3,
                    gp: true,
                },
            )
        })
        .collect();
    assert_eq!(split_register_benefit(&live, id, &[reg], &loads), 0);
    assert_eq!(split_register_benefit(&live, id, &[mem], &[]), 0);
}

/// LV11-1: under `LS_SPLITS` (reference residency off) a `Ref` is a certain
/// refusal only with the new switch.
#[test]
fn lv12_prepin_lists_refs_under_splits_only_with_the_switch() {
    let (graph, schedule) = ref_array_call_loop();
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    let refs = |edits: &[(&str, Option<&str>)]| {
        with_thread_overrides(edits, || {
            ls_certain_refusals(&graph, &schedule, &live)
                .into_iter()
                .filter(|&id| graph.nodes[id as usize].ty == IrType::Ref)
                .count()
        })
    };
    let ref_res = ("CRATONVM_JIT_IR_REF_RESIDENCY", None);
    let without = refs(&[(SPLITS, Some("1")), (PREPIN_REFS, None), ref_res]);
    let with = refs(&[(SPLITS, Some("1")), (PREPIN_REFS, Some("1")), ref_res]);
    let no_splits = refs(&[(SPLITS, None), (PREPIN_REFS, None), ref_res]);
    assert!(
        with > without,
        "more Refs are listed with the switch ({without} -> {with})"
    );
    assert_eq!(with, no_splits, "the same list as without LS_SPLITS");
}

/// LV11-4: the rule, on a real plan -- a pinned or homeless value always
/// passes; a shareable home passes exactly inside its colouring range.
#[test]
fn lv12_point_home_covers_asks_the_colouring_range_of_a_shared_home() {
    let (graph, schedule) = pressure_loop();
    let plan = plan_slots(&graph, &schedule, None);
    let ranges = plan.colouring_ranges();
    let mut checked = 0usize;
    for id in 0..graph.nodes.len() {
        let class = plan.class.get(id).copied().flatten();
        let range = ranges.get(id).copied().flatten();
        match (plan.node_color[id], class, range) {
            (None, _, _) | (_, Some(SlotClass::Pinned) | None, _) => {
                assert!(point_home_covers(&plan, id, 0));
                assert!(point_home_covers(&plan, id, usize::MAX));
            }
            (Some(_), Some(_), Some(r)) => {
                assert!(point_home_covers(&plan, id, r.lo));
                assert!(point_home_covers(&plan, id, r.hi));
                assert!(!point_home_covers(&plan, id, r.hi + 1));
                if r.lo > 0 {
                    assert!(!point_home_covers(&plan, id, r.lo - 1));
                }
                checked += 1;
            }
            _ => {}
        }
    }
    assert!(checked > 0, "some shareable home was checked");
}

/// LV11-4 end to end: with the default (pinned) plans no point names a home
/// past its range, so the check changes no verdict and no code size.
#[test]
fn lv12_the_deopt_point_range_check_leaves_default_compiles_unchanged() {
    let mut cases: Vec<(&str, Graph, Schedule, usize, usize)> = Vec::new();
    let (g, s) = pressure_loop();
    cases.push(("pressure", g, s, 1, 19));
    let (g, s) = ref_array_call_loop();
    cases.push(("ref-array-call", g, s, 1, 3));
    let mut compared = 0usize;
    for (name, graph, schedule, params, locals) in &cases {
        let run = |v: Option<&str>| {
            with_thread_overrides(&[(POINT_CHECK, v)], || {
                lowered(graph, schedule, *params, *locals).map(|b| b.len())
            })
        };
        let off = run(None);
        assert_eq!(run(Some("1")), off, "{name}");
        compared += usize::from(off.is_some());
    }
    assert!(compared >= 1);
    assert!(!with_thread_overrides(
        &[(POINT_CHECK, None)],
        ir_deopt_point_range_check_enabled
    ));
}

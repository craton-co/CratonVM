// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 19 proposal wave 11, lane lv11: the split gate on the
//! `R19Lv10SplitGate.pressure` shape (LV10-6 / i64-L2,
//! `CRATONVM_JIT_IR_LS_SPLITS_CARRIED_FIRST`), the Ref-carrying fixture
//! (LV10-4), and the token-read guard's strict test mode, debug default and
//! point-range rule (TOK10-1/2/3).

use super::*;
use crate::ir::IrBuilder;
use crate::ir_schedule;
use cratonvm_types::flags::with_thread_overrides;

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
    assert!(token_exempt_at(prim, true, None, Some(3)));}


// ── JIT round 19 proposal wave 12, lane lv12: LV11-1..4 ──────────────────

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


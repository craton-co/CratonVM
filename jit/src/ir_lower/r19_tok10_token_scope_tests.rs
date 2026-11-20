use super::*;
use crate::ir::IrBuilder;
use crate::regalloc::linear_scan_tests::nested_loop_with_a_token_read;


/// `burnt` of the i58 shape: names `stop` only as its memory token.
fn burnt_of(graph: &Graph, stop: NodeId) -> NodeId {
    graph
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::LoadStatic { .. }) && n.inputs.get(1) == Some(&stop))
        .expect("`burnt` names `stop` as its token") as NodeId
}

/// One node: exactly `token_only_inputs_in`. Several: the union, once
/// each, minus anything one of them reads as a value.
#[test]
fn r19_tok10_a_scope_set_is_the_union_minus_value_reads() {
    let (graph, _, _, stop) = nested_loop_with_a_token_read();
    let burnt = &graph.nodes[burnt_of(&graph, stop) as usize];
    let one = token_only_inputs_across(&[burnt], |_| false);
    assert_eq!(one, token_only_inputs_in(burnt, |_| false));
    assert_eq!(one, vec![(stop, 1, false)]);
    assert_eq!(
        token_only_inputs_across(&[burnt, burnt], |_| false),
        one,
        "once"
    );

    // A fused compare that reads `stop` as a value: the scope reads it.
    let mut cmp = graph
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::Cmp(_)))
        .expect("the loop compares")
        .clone();
    cmp.inputs[0] = stop;
    assert!(token_only_inputs_across(&[burnt, &cmp], |_| false).is_empty());
    assert!(token_only_inputs_across(&[&cmp, burnt], |_| false).is_empty());
    // Exemption is per id, as in the data scope.
    assert_eq!(
        token_only_inputs_across(&[burnt], |id| id == stop),
        vec![(stop, 1, true)]
    );
}

/// A lowerer over the i58 shape plus an `athrow` terminator whose memory
/// token is `stop` (the one terminator with a token slot), the guard on.
fn throw_fixture() -> (Lowerer<'static>, NodeId, NodeId) {
    let (mut graph, schedule, inner, stop) = nested_loop_with_a_token_read();
    let ctrl = schedule.blocks[inner].ctrl;
    let exc = graph.add(Op::Const(0), IrType::Ref, vec![], None);
    let throw = graph.add(Op::Throw, IrType::Control, vec![ctrl, stop, exc], None);
    let graph: &'static Graph = Box::leak(Box::new(graph));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
    lo.token_read_guard = true;
    (lo, throw, stop)
}


fn r19_tok10_the_term_scope_refuses_a_terminator_read_through_its_token_body() {
    let (mut lo, throw, stop) = throw_fixture();
    assert_eq!(lo.term_token_scope_nodes(throw), vec![throw], "not an `If`");
    let saved = lo.token_scope_enter(&[throw], TokenScope::Term);
    assert!(saved.is_some(), "the guard is on");
    assert_eq!(lo.token_scope, TokenScope::Term);
    assert_eq!(lo.token_scope_reader, throw);
    let ids: Vec<_> = lo.token_only_inputs.iter().map(|e| (e.0, e.1)).collect();
    assert_eq!(ids, vec![(stop, 1)], "the athrow's token producer");
    assert_eq!(lo.token_scope_nodes, [0, 1, 0]);

    // Deterministic whatever the plan's pinning: an unexempt entry.
    lo.token_only_inputs = vec![(stop, 1, false)];
    let _ = lo.slot_of_checked(stop);
    let refused = lo
        .take_latched_bailout()
        .expect("a read through a token slot");
    assert!(
        refused
            .context
            .as_deref()
            .is_some_and(|c| c.contains("scope=term") && c.contains(&format!("n{throw} "))),
        "{refused:?}"
    );
    assert_eq!(lo.token_scope_refusals.get(), [0, 1, 0]);
    assert_eq!(lo.token_read_refusals.get(), 1, "the sum counts it too");

    lo.token_only_inputs = vec![(stop, 1, true)];
    let _ = lo.resident_gpr(stop);
    assert!(lo.take_latched_bailout().is_none(), "exempt: counted only");
    assert_eq!(lo.token_scope_pinned_hits.get(), [0, 1, 0]);

    lo.token_scope_exit(saved);
    assert!(lo.token_only_inputs.is_empty());
    assert_eq!(lo.token_scope, TokenScope::Data);
    assert_eq!(lo.token_scope_reader, NO_NODE);
    let _ = lo.slot_of_checked(stop);
    assert!(
        lo.take_latched_bailout().is_none(),
        "outside the scope: no check"
    );
    assert_eq!(
        lo.token_scope_census(),
        "data:0/0/0,term:1/1/1,mir:0/0/0",
        "the census line's scope= field"
    );
}

/// Off, the scopes compute nothing and change nothing.
#[test]
fn r19_tok10_a_scope_is_inert_with_the_guard_off() {
    let (mut lo, throw, _) = throw_fixture();
    lo.token_read_guard = false;
    assert!(lo.token_scope_enter(&[throw], TokenScope::Term).is_none());
    assert!(lo.token_only_inputs.is_empty());
    assert_eq!(lo.token_scope, TokenScope::Data);
    assert_eq!(lo.token_scope_nodes, [0, 0, 0]);
    assert!(lo.token_guard_named.is_empty(), "nothing computed");
}

/// An `If` whose compare is fused into the branch: the scope is the
/// branch, the compare, and an `lcmp`/`fcmp` fused under the compare --
/// and nothing for an unfused compare.
#[test]
fn r19_tok10_a_fused_compare_joins_the_term_scope() {
    let (graph, schedule, _, _) = nested_loop_with_a_token_read();
    let (if_id, cond) = graph
        .nodes
        .iter()
        .enumerate()
        .find_map(|(i, n)| {
            let cond = *n.inputs.get(1)?;
            let is_cmp = matches!(graph.nodes.get(cond as usize)?.op, Op::Cmp(_));
            (matches!(n.op, Op::If) && is_cmp).then_some((i as NodeId, cond))
        })
        .expect("the loop branches on a compare");
    let operand = graph.nodes[cond as usize].inputs[0];
    let graph: &'static Graph = Box::leak(Box::new(graph));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
    let n = graph.nodes.len();
    lo.fused_cmp = vec![false; n];
    lo.fused_lcmp = vec![false; n];
    lo.fused_fcmp = vec![false; n];
    assert_eq!(lo.term_token_scope_nodes(if_id), vec![if_id], "unfused");
    lo.fused_cmp[cond as usize] = true;
    assert_eq!(lo.term_token_scope_nodes(if_id), vec![if_id, cond]);
    lo.fused_lcmp[operand as usize] = true;
    assert_eq!(lo.term_token_scope_nodes(if_id), vec![if_id, cond, operand]);
}


/// Why carried values need no scope of their own: a carry is planned only
/// into an `op_reads_rax_then_rcx` consumer, and none of those has a
/// memory-token slot, so a carry's read is never a token read.
#[test]
fn r19_tok10_no_carry_consumer_has_a_token_slot() {
    let consumers = [
        Op::Add,
        Op::Sub,
        Op::Mul,
        Op::Div,
        Op::Rem,
        Op::Neg,
        Op::And,
        Op::Or,
        Op::Xor,
        Op::Shl,
        Op::Shr,
        Op::UShr,
        Op::I2L,
        Op::L2I,
    ];
    for op in consumers {
        assert!(op_reads_rax_then_rcx(&op), "{op:?}");
        assert!(op.memory_shape().is_none(), "{op:?} has a token slot");
    }
}

/// `static int f(int n) { int s = 0; for (int i = 0; i < n; i++) s += i;
/// return s; }` -- a fused loop compare, ALU tiles.
const SUM_LOOP: [u8; 23] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02,
    0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
];

/// `static void t(Object o) { throw o; }` -- an `athrow` terminator.
const THROW_PARAM: [u8; 4] = [0x2a, 0xbf, 0, 0];

fn lowered(code: &[u8], params: &[IrType], locals: usize) -> Option<Vec<u8>> {
    let mut builder = IrBuilder::new(params.len(), locals);
    builder.set_param_types(params);
    let graph = builder.build(code, code.len() - 2)?;
    let schedule = crate::ir_schedule::schedule(&graph);
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the bodies are compared, never run.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    lower_inner(
        &graph,
        &schedule,
        params.len(),
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


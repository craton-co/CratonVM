use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, NO_NODE};

/// `for (i = 0; i < n; i++) ...`: entry block, header (merge, phi, test),
/// body. Returns `(graph, i's phi, the body's increment, the param)`.
fn loop_graph() -> (Graph, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let region = g.add(Op::Merge, IrType::Control, vec![c0], Some(10));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, n], Some(12));
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(12));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(22));
    g.nodes[iv as usize].inputs.push(next);
    g.nodes[region as usize].inputs.push(stay);
    g.exit = g.add(Op::Return, IrType::Int, vec![exit, iv], None);
    (g, iv, next, n)
}

/// A pad whose frame names the loop phi is undefined in the pre-header and
/// fine in the loop; a chain-less site with no frame at its bci is not judged.
#[test]
fn a_pad_naming_a_loop_phi_is_undefined_in_the_pre_header() {
    let (mut g, iv, _next, n) = loop_graph();
    g.push_safepoint(crate::ir::SafepointSnapshot::new(7, vec![iv, n], vec![], vec![]));
    let schedule = crate::ir_schedule::schedule(&g);
    let graph: &'static Graph = Box::leak(Box::new(g));
    let schedule: &'static _ = Box::leak(Box::new(schedule));
    let plan: &'static _ = Box::leak(Box::new(super::super::plan_slots(graph, schedule, None)));
    let mut lo = super::super::tests::lowerer_over(graph, schedule, plan, 4096);
    lo.cur_def = Some(n);
    assert!(lo.site_frame_undefined(false, 7, 7), "the pre-header has not written the phi");
    assert!(!lo.site_frame_undefined(false, 9, 9), "no frame at bci 9: not judged");
    lo.cur_def = Some(iv);
    assert!(!lo.site_frame_undefined(false, 7, 7), "the header defines it");
    lo.cur_def = Some(n);
    lo.latch_if_site_frame_undefined(false, 7, 7);
    assert!(lo.take_latched_bailout().is_some(), "the compile is refused");
}

/// A loop phi (and anything the body computes) is not defined in the
/// pre-header, where LICM puts a hoisted read that keeps its in-loop frame;
/// it is defined in the loop and after it.
#[test]
fn a_loop_phi_is_not_defined_in_the_pre_header_but_is_in_the_loop() {
    let (g, iv, next, n) = loop_graph();
    let schedule = crate::ir_schedule::schedule(&g);
    let graph: &'static Graph = Box::leak(Box::new(g));
    let schedule: &'static _ = Box::leak(Box::new(schedule));
    let plan: &'static _ = Box::leak(Box::new(super::super::plan_slots(graph, schedule, None)));
    let mut lo = super::super::tests::lowerer_over(graph, schedule, plan, 4096);
    let block_of = |id: NodeId| schedule.node_to_block[id as usize];
    assert_ne!(block_of(iv), block_of(n), "the phi is in the header block");
    // The pre-header (entry block): a site whose block the header does not dominate.
    lo.cur_def = Some(n);
    assert!(!lo.frame_value_defined_at_site(iv), "the phi's copy has not run");
    assert!(!lo.frame_value_defined_at_site(next), "nor the body's value");
    assert!(lo.frame_value_defined_at_site(n), "a parameter always is");
    assert!(lo.frame_value_defined_at_site(NO_NODE), "a dead slot always is");
    // The header's own block, and the body the header dominates.
    for site in [iv, next] {
        lo.cur_def = Some(site);
        assert!(lo.frame_value_defined_at_site(iv), "site n{site}");
    }
}

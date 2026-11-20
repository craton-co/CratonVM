use super::*;
use crate::ir::{
    Graph, IrType, MemKind, NodeId, Op, SafepointSnapshot, SpliceScopeChain, SpliceScopeFrame,
};


fn chain(bci: usize, local: NodeId, caller_local: NodeId) -> SafepointSnapshot {
    let mut sp = SafepointSnapshot::new(bci, vec![local], vec![caller_local], Vec::new());
    sp.splice_scope = Some(std::sync::Arc::new(SpliceScopeChain {
        scopes: vec![
            SpliceScopeFrame {
                class_id: 0,
                method_key: "P.f:()V".to_string(),
                bci: 2,
                code_len: 5,
                max_locals: 1,
                locals_len: 0,
                stack_len: 0,
            },
            SpliceScopeFrame {
                class_id: 0,
                method_key: String::new(),
                bci: 1,
                code_len: 0,
                max_locals: 0,
                locals_len: 1,
                stack_len: 0,
            },
        ],        }));
    sp    }

/// A trapping load at combined pc 12 of the body spliced for the `invoke`
/// at bci 1; a flat frame at 1 and a chain frame at 12 naming `alloc`,
/// which only the spliced body holds. Returns `(graph, alloc)`.
fn graph() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, p, zero],
        Some(12),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, load], Some(4));
    let alloc = g.add(Op::Const(3), IrType::Ref, vec![], None);
    g.push_safepoint(SafepointSnapshot::new(1, vec![p], vec![p], Vec::new()));
    g.push_safepoint(chain(12, alloc, p));
    (g, alloc)
}

fn described(alloc: NodeId) -> ir_lower::ScalarReplacementMap {
    let mut map = ir_lower::ScalarReplacementMap::default();
    map.objects.insert(
        alloc,
        ir_lower::VirtualObjectInfo {
            loop_phis: None,
            class_id: 5,
            array_element_type: None,
            num_fields: 0,
            field_values: Vec::new(),
            new_ctrl: 0,
            store_ctrls: Vec::new(),
            store_nodes: Vec::new(),
            store_fields: Vec::new(),
            non_null_cond: None,
        },
    );
    map
}


/// The box fold clears a slot instead of leaving a recipe, so it must see
/// the chains the allocation planner may ignore.
#[test]
fn every_chain_counts_for_a_rewrite_that_leaves_no_recipe() {
    let (g, alloc) = graph();
    assert!(
        !ea_snapshot_names(&g, alloc),
        "the planner's view: an unclaimed chain"
    );
    assert!(
        ea_any_chain_snapshot_names(&g, alloc),
        "the fold's view: any chain"
    );
    let p = g.safepoints[0].locals[0];
    assert!(
        ea_any_chain_snapshot_names(&g, p),
        "the chain's caller scope names it too"
    );
}

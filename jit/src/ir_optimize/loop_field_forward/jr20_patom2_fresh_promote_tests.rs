use super::{fresh_base_unpublished, fresh_promote_enabled, promote_fresh_loop_fields};
use crate::ir::{Graph, IrType, MemKind, NodeId, Op, SafepointSnapshot};

/// What the graph holds besides the accumulator loop.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Extra {
    None,
    /// A `long` cell.
    Long,
    /// A call in the loop body.
    Call,
    /// A guard (a deopt point) in the loop body.
    Guard,
    /// The object is stored into another object's field before the loop.
    StoredBefore,
    /// The object is passed to a call before the loop (an unspliced `<init>`).
    CallArgBefore,
    /// The object is stored into another object's field AFTER the loop:
    /// still a store of the base, so still refused (the rule names no time).
    StoredAfter,
}

struct Built {
    g: Graph,
    base: NodeId,
    load: NodeId,
    store: NodeId,
    after: NodeId,
    header: NodeId,
}

/// `Acc a = new Acc(); for (i = 0; i < 100; i++) a.v += i; x = a.v;
/// return a;` -- the allocation before the header, the header `Merge` at
/// bci 2, `getfield` at 5, `putfield` at 8, the read after the loop at 20.
fn build(extra: Extra) -> Built {
    let long = extra == Extra::Long;
    let (ty, kind) = if long {
        (IrType::Long, MemKind::Long)
    } else {
        (IrType::Int, MemKind::Int)
    };
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let other = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let base = g.add(
        Op::New {
            class_id: 5,
            num_fields: 2,
        },
        IrType::Ref,
        vec![entry, mem0],
        Some(0),
    );
    let idx = g.add(Op::Const(16), IrType::Int, vec![], None);
    let ref_idx = g.add(Op::Const(24), IrType::Int, vec![], None);
    let mut pre_mem = base;
    if extra == Extra::StoredBefore {
        let st = g.add(
            Op::Store(MemKind::Ref),
            IrType::Memory,
            vec![entry, pre_mem, other, ref_idx, base],
            Some(1),
        );
        g.field_decl_class.insert(st, 6);
        pre_mem = st;
    }
    if extra == Extra::CallArgBefore {
        pre_mem = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Void,
            vec![entry, pre_mem, base],
            Some(1),
        );
    }
    let header = g.add(Op::Merge, IrType::Control, vec![entry], Some(2));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let hundred = g.add(Op::Const(100), IrType::Int, vec![], None);
    let i = g.add(Op::Phi, IrType::Int, vec![header, zero], None);
    let mphi = g.add(Op::Phi, IrType::Memory, vec![header, pre_mem], None);
    let cmp = g.add(
        Op::Cmp(crate::ir::CmpOp::Lt),
        IrType::Int,
        vec![i, hundred],
        None,
    );
    let iff = g.add(Op::If, IrType::Control, vec![header, cmp], Some(4));
    let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let load = g.add(Op::Load(kind), ty, vec![stay, mphi, base, idx], Some(5));
    g.field_decl_class.insert(load, 5);
    let addend = if long {
        g.add(Op::I2L, IrType::Long, vec![i], None)
    } else {
        i
    };
    let sum = g.add(Op::Add, ty, vec![load, addend], None);
    let mut mem = load;
    if extra == Extra::Call {
        mem = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Void,
            vec![stay, mem],
            Some(6),
        );
    }
    if extra == Extra::Guard {
        let c = g.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![i, hundred],
            None,
        );
        g.add(Op::Guard { bci: 7 }, IrType::Void, vec![stay, c], Some(7));
    }
    let store = g.add(
        Op::Store(kind),
        IrType::Memory,
        vec![stay, mem, base, idx, sum],
        Some(8),
    );
    g.field_decl_class.insert(store, 5);
    let next = g.add(Op::Add, IrType::Int, vec![i, one], None);
    g.nodes[header as usize].inputs.push(stay);
    g.nodes[i as usize].inputs.push(next);
    g.nodes[mphi as usize].inputs.push(store);
    let after = g.add(Op::Load(kind), ty, vec![exit, mphi, base, idx], Some(20));
    g.field_decl_class.insert(after, 5);
    if extra == Extra::StoredAfter {
        let st = g.add(
            Op::Store(MemKind::Ref),
            IrType::Memory,
            vec![exit, after, other, ref_idx, base],
            Some(22),
        );
        g.field_decl_class.insert(st, 6);
    }
    let ret = g.add(Op::Return, IrType::Void, vec![exit, base], None);
    g.exit = ret;
    g.push_safepoint(SafepointSnapshot::new(2, vec![i, base], vec![], vec![]));
    Built {
        g,
        base,
        load,
        store,
        after,
        header,
    }
}

fn live_field_stores(g: &Graph) -> Vec<NodeId> {
    g.nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::Store(MemKind::Int | MemKind::Long)))
        // Cast: arena index to node id.
        .map(|(id, _)| id as NodeId)
        .collect()
}

#[test]
fn an_accumulator_of_a_fresh_returned_object_is_written_back_at_the_exit() {
    for extra in [Extra::None, Extra::Long] {
        let mut b = build(extra);
        assert!(fresh_base_unpublished(&b.g, b.base).is_ok(), "{extra:?}");
        assert_eq!(promote_fresh_loop_fields(&mut b.g), 1, "{extra:?}");
        assert_eq!(
            b.g.nodes[b.load as usize].op,
            Op::Dead,
            "the in-loop read is gone"
        );
        assert_eq!(
            b.g.nodes[b.store as usize].op,
            Op::Dead,
            "the in-loop store is gone"
        );
        let stores = live_field_stores(&b.g);
        assert_eq!(stores.len(), 1, "one exit write-back");
        let wb = stores[0];
        assert_eq!(
            b.g.nodes[wb as usize].inputs[2], b.base,
            "through the same base"
        );
        assert_eq!(b.g.promoted_cell_writebacks, vec![(b.header, wb)]);
        assert_eq!(b.g.promoted_cell_header_bcis, vec![2]);
        let taken = b.g.nodes[wb as usize].inputs[0];
        assert_eq!(b.g.nodes[taken as usize].op, Op::Proj(0));
        // The read after the loop reads the write-back's memory φ.
        let after_mem = b.g.nodes[b.after as usize].inputs[1];
        let phi = &b.g.nodes[after_mem as usize];
        assert!(phi.op == Op::Phi && phi.ty == IrType::Memory);
        assert!(phi.inputs.contains(&wb), "the φ joins the write-back");
        // One read of the cell before the loop, at the header's bci.
        let pre =
            b.g.nodes
                .iter()
                .filter(|n| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(2))
                .count();
        assert_eq!(pre, 1, "{extra:?}");
    }
}


/// A parameter is not a fresh object of this compile.
#[test]
fn only_an_allocation_of_this_compile_is_fresh() {
    let b = build(Extra::None);
    let param =
        b.g.nodes
            .iter()
            .position(|n| matches!(n.op, Op::Param(_)))
            .expect("the fixture's parameter");
    // Cast: arena index to node id.
    assert!(fresh_base_unpublished(&b.g, param as NodeId).is_err());
}


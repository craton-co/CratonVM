use super::{
    promote_loop_statics, static_promote_enabled, static_promote_method_entry_enabled,
};
use crate::ir::{Graph, IrType, NodeId, Op, SafepointSnapshot};

/// What the loop body holds besides `COUNTER = COUNTER + i`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Extra {
    None,
    /// The counter is volatile.
    Volatile,
    /// A call in the body.
    Call,
    /// A guard (a deopt point) in the body.
    Guard,
    /// The cell is not on the compile's direct list (helper route).
    NotDirect,
    /// A `long` counter.
    Long,
}

struct Built {
    g: Graph,
    load: NodeId,
    store: NodeId,
    after: NodeId,
    header: NodeId,
}

/// `for (i = 0; i < 100; i++) COUNTER += i; x = COUNTER;` -- header
/// `Merge(entry, back)` at bci 2, `getstatic` at 5, `putstatic` at 8,
/// the read after the loop at 20.
fn build(extra: Extra) -> Built {
    let long = extra == Extra::Long;
    let (ty, tag) = if long {
        (IrType::Long, b'J')
    } else {
        (IrType::Int, b'I')
    };
    let is_volatile = extra == Extra::Volatile;
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let header = g.add(Op::Merge, IrType::Control, vec![entry], Some(2));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let hundred = g.add(Op::Const(100), IrType::Int, vec![], None);
    let i = g.add(Op::Phi, IrType::Int, vec![header, zero], None);
    let mphi = g.add(Op::Phi, IrType::Memory, vec![header, mem0], None);
    let cmp = g.add(
        Op::Cmp(crate::ir::CmpOp::Lt),
        IrType::Int,
        vec![i, hundred],
        None,
    );
    let iff = g.add(Op::If, IrType::Control, vec![header, cmp], Some(4));
    let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let get = Op::LoadStatic {
        class_id: 9,
        field_index: 0,
        type_tag: tag,
        is_volatile,
    };
    let load = g.add(get.clone(), ty, vec![stay, mphi], Some(5));
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
        Op::StoreStatic {
            class_id: 9,
            field_index: 0,
            type_tag: tag,
            is_volatile,
        },
        IrType::Memory,
        vec![stay, mem, sum],
        Some(8),
    );
    let next = g.add(Op::Add, IrType::Int, vec![i, one], None);
    g.nodes[header as usize].inputs.push(stay);
    g.nodes[i as usize].inputs.push(next);
    g.nodes[mphi as usize].inputs.push(store);
    let after = g.add(get, ty, vec![exit, mphi], Some(20));
    let ret = g.add(Op::Return, IrType::Void, vec![exit], None);
    g.exit = ret;
    g.push_safepoint(SafepointSnapshot::new(2, vec![i], vec![], vec![]));
    if extra != Extra::NotDirect {
        g.direct_static_fields = vec![(9, 0)];
    }
    Built {
        g,
        load,
        store,
        after,
        header,
    }
}

fn live_static_stores(g: &Graph) -> Vec<NodeId> {
    g.nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::StoreStatic { .. }))
        // Cast: arena index to node id.
        .map(|(id, _)| id as NodeId)
        .collect()
}

#[test]
fn a_counter_loop_reads_once_and_writes_back_at_the_exit() {
    for extra in [Extra::None, Extra::Long] {
        let mut b = build(extra);
        assert_eq!(promote_loop_statics(&mut b.g), 1, "{extra:?}");
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
        let stores = live_static_stores(&b.g);
        assert_eq!(stores.len(), 1, "one exit write-back");
        let wb = stores[0];
        assert_eq!(b.g.promoted_cell_writebacks, vec![(b.header, wb)]);
        assert_eq!(b.g.promoted_cell_header_bcis, vec![2]);
        // Conditional: under the taken edge of `If(Cmp(Ne, ..))`.
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
                .filter(|n| matches!(n.op, Op::LoadStatic { .. }) && n.bytecode_pc == Some(2))
                .count();
        assert_eq!(pre, 1, "{extra:?}");        }    }
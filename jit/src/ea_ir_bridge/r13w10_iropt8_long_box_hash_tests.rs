use super::*;
use crate::ir::{Graph, IrType, Op};

fn info(class: &'static str, method: &'static str, desc: &'static str, kind: u8) -> usize {
    // LEAK(intentional): test-only; the bridge reads it through a raw pointer.
    Box::leak(Box::new(JitInvokeInfo {
        class_name: class,
        method_name: method,
        descriptor: desc,
        num_jit_args: 1,
        return_type: b'L',
        invoke_kind: kind,
        declaring_class_id: 0,
        owner_class_id: 0,
    })) as *const JitInvokeInfo as usize
}

/// `static int f(long v) { return Long.valueOf(v).hashCode(); }`, the
/// `hashCode` declared on `declared`. Returns the graph, `v`, the call and
/// the return.
fn fixture(declared: &'static str) -> (Graph, ir::NodeId, ir::NodeId, ir::NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let v = g.add(Op::Param(0), IrType::Long, vec![], None);
    let bi = info("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;", 3);
    let boxed = g.add(
        Op::Call { info_ptr: bi },
        IrType::Ref,
        vec![ctrl, mem, v],
        Some(2),
    );
    let hi = info(declared, "hashCode", "()I", 0);
    let hash = g.add(
        Op::Call { info_ptr: hi },
        IrType::Int,
        vec![ctrl, boxed, boxed],
        Some(5),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, hash], Some(8));
    g.entry = start;
    g.exit = ret;
    (g, v, hash, ret)
}

/// `Long.valueOf(v).hashCode()` becomes `(int) (v ^ (v >>> 32))`.
#[test]
fn long_value_of_hash_code_folds_to_the_long_hash() {
    for declared in ["java/lang/Object", "java/lang/Number", "java/lang/Long"] {
        let (mut g, v, hash, ret) = fixture(declared);
        assert_eq!(ir_fold_hash_code_of_integer_box(&mut g), 1, "{declared}");
        assert_eq!(g.nodes[hash as usize].op, Op::Dead);
        let h = g.nodes[ret as usize].inputs[1];
        assert_eq!(g.nodes[h as usize].op, Op::L2I);
        let x = g.nodes[h as usize].inputs[0];
        assert_eq!(g.nodes[x as usize].op, Op::Xor);
        assert_eq!(g.nodes[x as usize].ty, IrType::Long);
        assert_eq!(g.nodes[x as usize].inputs[0], v);
        let hi = g.nodes[x as usize].inputs[1];
        assert_eq!(g.nodes[hi as usize].op, Op::UShr);
        assert_eq!(g.nodes[hi as usize].inputs[0], v);
        let c = g.nodes[hi as usize].inputs[1];
        assert_eq!(g.nodes[c as usize].op, Op::Const(32));
        assert_eq!(g.nodes[c as usize].ty, IrType::Int);
    }
}


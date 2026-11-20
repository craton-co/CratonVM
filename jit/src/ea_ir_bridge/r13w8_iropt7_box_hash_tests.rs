use super::*;
use crate::ir::{Graph, IrType, Op, NO_NODE};

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

struct HashFixture {
    g: Graph,
    v: ir::NodeId,
    boxed: ir::NodeId,
    null_cmp: ir::NodeId,
    hash: ir::NodeId,
    after: ir::NodeId,
    ret: ir::NodeId,
}

/// `static int f(int v) { Integer k = v; if (k == null) ..; int h =
/// k.hashCode(); g(); return h; }`, flattened: `valueOf` at bci 1, the null
/// compare at 4, `hashCode` (of `hash_kind`, on `recv_is_box ? k : a
/// parameter`) at 8, a token-only call at 11 and the return at 14.
fn fixture(hash_kind: u8, recv_is_box: bool) -> HashFixture {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let v = g.add(Op::Param(0), IrType::Int, vec![], None);
    let other = g.add(Op::Param(1), IrType::Ref, vec![], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let bi = info("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;", 3);
    let boxed = g.add(
        Op::Call { info_ptr: bi },
        IrType::Ref,
        vec![ctrl, mem, v],
        Some(1),
    );
    let null_cmp = g.add(
        Op::Cmp(ir::CmpOp::Eq),
        IrType::Int,
        vec![boxed, null],
        Some(4),
    );
    let hi = info("java/lang/Object", "hashCode", "()I", hash_kind);
    let recv = if recv_is_box { boxed } else { other };
    let hash = g.add(
        Op::Call { info_ptr: hi },
        IrType::Int,
        vec![ctrl, boxed, recv],
        Some(8),
    );
    let after = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![ctrl, hash],
        Some(11),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, hash], Some(14));
    g.entry = start;
    g.exit = ret;
    let sp = ir::SafepointSnapshot::new(11, vec![v, NO_NODE], vec![hash], Vec::new());
    g.push_safepoint(sp);
    HashFixture {
        g,
        v,
        boxed,
        null_cmp,
        hash,
        after,
        ret,
    }
}

/// The call goes; its value, its snapshot slot and its memory token all
/// follow; the box stays (it has another reader).
#[test]
fn integer_value_of_hash_code_folds_to_the_boxed_int() {
    let mut f = fixture(0, true);
    assert_eq!(ir_fold_hash_code_of_integer_box(&mut f.g), 1);
    assert_eq!(f.g.nodes[f.hash as usize].op, Op::Dead);
    assert_eq!(
        f.g.nodes[f.ret as usize].inputs[1], f.v,
        "hashCode() of the box is v"
    );
    assert_eq!(
        f.g.nodes[f.after as usize].inputs[1], f.boxed,
        "the token chain skips the call"
    );
    assert_eq!(
        f.g.safepoints[0].stack[0], f.v,
        "the snapshot follows the value"
    );
    assert!(
        matches!(f.g.nodes[f.boxed as usize].op, Op::Call { .. }),
        "the box is kept"
    );
}

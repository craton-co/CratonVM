//! Round 14 wave 3 (lane calls, I7-4): `"literal".hashCode()` and
//! `"literal" == null` fold.
use super::*;
use crate::ir::{Graph, IrType, Op};

fn info(class: &'static str, method: &'static str, desc: &'static str, kind: u8) -> usize {
    // LEAK(intentional): test-only; the bridge reads it through a raw pointer.
    Box::leak(Box::new(JitInvokeInfo {
        class_name: class,
        method_name: method,
        descriptor: desc,
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: kind,
        declaring_class_id: 0,
        owner_class_id: 0,
    })) as *const JitInvokeInfo as usize
}

/// `"polygenelubricants".hashCode()` is `Integer.MIN_VALUE` in Java (the
/// wrap is part of the contract, so the constant must carry the sign).
const HASH: i32 = i32::MIN;

struct Fixture {
    g: Graph,
    lit: ir::NodeId,
    null_cmp: ir::NodeId,
    hash: ir::NodeId,
    after: ir::NodeId,
    ret: ir::NodeId,
}

/// `ldc #3` (holder 7) at bci 0, `== null` at 2, `hashCode` (declared on
/// `declared`, kind `kind`, on the literal or on a parameter) at 5, a
/// token-only call at 8, the return at 11.
fn fixture(declared: &'static str, kind: u8, recv_is_literal: bool) -> Fixture {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let other = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let lit = g.add(
        Op::ConstString {
            holder_class_id: 7,
            cp_idx: 3,
            slot_addr: 0,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    let null_cmp = g.add(
        Op::Cmp(ir::CmpOp::Eq),
        IrType::Int,
        vec![lit, null],
        Some(2),
    );
    let recv = if recv_is_literal { lit } else { other };
    let hash = g.add(
        Op::Call {
            info_ptr: info(declared, "hashCode", "()I", kind),
        },
        IrType::Int,
        vec![ctrl, lit, recv],
        Some(5),
    );
    let after = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![ctrl, hash],
        Some(8),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, hash], Some(11));
    g.entry = start;
    g.exit = ret;
    Fixture {
        g,
        lit,
        null_cmp,
        hash,
        after,
        ret,
    }
}

fn hashes() -> HashMap<(u32, u16), i32> {
    HashMap::from([((7u32, 3u16), HASH)])
}

#[test]
fn a_literal_hash_code_folds_to_the_literal_hash() {
    for declared in ["java/lang/String", "java/lang/Object"] {
        let mut f = fixture(declared, 0, true);
        assert_eq!(ir_fold_hash_code_of_string_literal(&mut f.g, &hashes()), 1);
        assert_eq!(f.g.nodes[f.hash as usize].op, Op::Dead);
        let c = f.g.nodes[f.ret as usize].inputs[1];
        assert_eq!(f.g.nodes[c as usize].op, Op::Const(i64::from(HASH)));
        assert_eq!(f.g.nodes[c as usize].ty, IrType::Int);
        assert_eq!(
            f.g.nodes[f.after as usize].inputs[1], f.lit,
            "the token chain skips the call"
        );
        assert!(
            matches!(f.g.nodes[f.lit as usize].op, Op::ConstString { .. }),
            "the literal is kept"
        );
    }
}



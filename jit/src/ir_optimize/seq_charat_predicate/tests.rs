use super::*;
use crate::ir::{CmpOp, MemKind, SafepointSnapshot};

const KEY: &str = "T.m:()V";

/// A leaked `CharSequence.charAt(I)C` interface row.
fn charat_row() -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "java/lang/CharSequence",
        method_name: "charAt",
        descriptor: "(I)C",
        num_jit_args: 2,
        return_type: b'C',
        invoke_kind: 2,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

/// The prefix kind the predicate keys on is resolvable in this build.
fn prefix_names_charat() -> bool {
    !crate::x64::narrow_oops_block_inline_fields()
        && crate::ir_string_search_prefix_kind("java/lang/CharSequence", "charAt", "(I)C")
            == Some(crate::IrStringSearchKind::SeqCharAt)
}

/// `BmpCharPropertyGreedy.match`'s loop, reduced:
///
/// ```java
/// while (i < n) { c = seq.charAt(i); i++; }   // header bci 5, call pc 12
/// ```
///
/// `seq` is `Param(1)` when `invariant`, else a field read of `Param(0)`
/// inside the loop. With `arm`, the loop's entry is the opaque OSR merge the
/// OSR door's build puts in front of the header. With `frame`, a flat
/// snapshot sits at the call's pc. Returns `(graph, region, entry, call,
/// seq)`.
fn seq_loop(
    invariant: bool,
    arm: bool,
    frame: bool,
) -> (Graph, NodeId, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let this = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let s = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let entry = if arm {
        let flag = g.add(
            Op::OsrEntryFlag { header_bci: 5 },
            IrType::Int,
            vec![entry0],
            None,
        );
        let gate = g.add(Op::If, IrType::Control, vec![entry0, flag], None);
        let from_entry = g.add(Op::Proj(1), IrType::Control, vec![gate], None);
        let osr = g.add(Op::Proj(0), IrType::Control, vec![gate], None);
        g.add(Op::Merge, IrType::Control, vec![from_entry, osr], Some(5))
    } else {
        entry0
    };
    let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(5));
    let i = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
    let mem = g.add(Op::Phi, IrType::Memory, vec![region, mem0], None);
    let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![i, n], None);
    let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(8));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
    let (seq, call_mem) = if invariant {
        (s, mem)
    } else {
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let ld = g.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![stay, mem, this, f0],
            Some(10),
        );
        (ld, ld)
    };
    let call = g.add(
        Op::Call {
            info_ptr: charat_row(),
        },
        IrType::Int,
        vec![stay, call_mem, seq, i],
        Some(12),
    );
    let i1 = g.add(Op::Add, IrType::Int, vec![i, one], None);
    g.nodes[region as usize].inputs.push(stay);
    g.nodes[i as usize].inputs.push(i1);
    g.nodes[mem as usize].inputs.push(call);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(20));
    g.push_safepoint(SafepointSnapshot::new(
        5,
        vec![this, s, n, i],
        vec![],
        vec![],
    ));
    if frame {
        g.push_safepoint(SafepointSnapshot::new(
            12,
            vec![this, s, n, i],
            vec![seq, i],
            vec![],
        ));
    }
    g.string_layout = Some(crate::StringFieldLayout::new(0, Some(1), 2, 7));
    (g, region, entry, call, seq)
}



/// `(entry control, receiver, class)` of every String guard at bci 5.
fn string_guards(g: &Graph) -> Vec<(NodeId, NodeId, u32)> {
    g.nodes
        .iter()
        .filter(|n| n.op == (Op::Guard { bci: 5 }) && n.bytecode_pc == Some(5))
        .filter_map(|n| {
            let c = g.node_opt(n.input_opt(1)?)?;
            match c.op {
                Op::ExactClassIs { class_id } => {
                    Some((n.input_opt(0)?, c.input_opt(0)?, class_id))
                }
                _ => None,
            }
        })
        .collect()
}






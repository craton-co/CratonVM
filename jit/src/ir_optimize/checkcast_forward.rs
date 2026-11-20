// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 23 wave 8 (lane W8-L7, TYPECHECK21-1 part C;
//! `CRATONVM_JIT_IR_CHECKCAST_VALUE_FORWARD`, perf, **default ON**, `=0` kill):
//! a `checkcast`'s readers read its OPERAND.
//!
//! # What
//!
//! A successful `checkcast` returns its operand unchanged (JVMS 6.5), so every
//! VALUE use of an `Op::CheckCast` -- data inputs and frame-state slots alike
//! -- may read the operand instead. The cast itself stays exactly where it
//! was:
//!
//! * it is a DCE root (`is_dce_root`: it owes its `ClassCastException`);
//! * its MEMORY uses keep reading it, so every later memory operation stays
//!   ordered after the check, and every impure node of its block keeps its
//!   place in id order;
//! * it still throws through its helper (`jit_checkcast`), the same path and
//!   the same message as before: no deopt is introduced, so the
//!   `ClassCastException` text is unchanged in both modes.
//!
//! What it buys (`W2cTypecheckLoops` `cast` row, `((Sub) a[i]).v`): the cast
//! result had a home of its own, stored on the inline hit path and reloaded
//! by the field read (n23-w8c `lambda$main$1`: `mov [rbp-0B8h],rax` ... `mov
//! rax,[rbp-0B8h]`). After the forward nothing reads that home, and the
//! lowering's `CheckCast` arm skips the hit-path store for a cast listed in
//! `Graph::checkcast_value_forwarded`; the field read reads `a[i]`'s home,
//! which the `nocast` row reads too.
//!
//! # Why sound
//!
//! A value use of the cast result can only execute after the cast succeeded:
//! a data use is either ordered after it by the memory chain (loads, stores,
//! calls, guards created after it in its block) or is pure, and a pure node
//! computes the same thing from the operand as from the result, because they
//! are the same reference. A frame state naming the result names the same
//! reference through the operand. Neither the operand nor the result is ever
//! a different object.
//!
//! # Not done
//!
//! The optimizing OSR route and OSR-door graphs keep their casts as they are
//! (the operand's range would grow across an entry the door re-checks), as
//! does a cast carrying exception edges (`exception_edge_anchors`).

use crate::ir::{Graph, NodeId, Op, NO_NODE};

/// `CRATONVM_JIT_IR_CHECKCAST_VALUE_FORWARD` -- **default ON**, `=0` kill.
/// Read per compile; no `static`.
pub(super) fn checkcast_value_forward_enabled() -> bool {
    true
}

/// Forward every eligible cast's value uses to its operand (see the module
/// notes) and list it in `Graph::checkcast_value_forwarded`. Returns how many
/// casts were forwarded.
pub(super) fn forward_checkcast_values(graph: &mut Graph) -> usize {
    if graph.osr_compile || graph.optimize_osr_route {
        return 0;
    }
    let anchors = crate::ir::exception_edge_anchors(graph);
    let casts: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::CheckCast { .. }) && n.inputs.len() >= 3)
        // Cast: arena index to node id.
        .map(|(id, _)| id as NodeId)
        .filter(|id| !anchors.contains(id))
        .collect();
    let mut forwarded = 0usize;
    for cast in casts {
        let Some(obj) = graph.node_opt(cast).and_then(|n| n.input_opt(2)) else {
            continue;
        };
        if obj == NO_NODE || obj == cast || graph.node_opt(obj).is_none_or(|o| o.op == Op::Dead) {
            continue;
        }
        // The memory-token slots that name the cast keep naming it.
        let mut token_uses: Vec<(NodeId, usize)> = Vec::new();
        for (u, n) in graph.nodes.iter().enumerate() {
            if n.op == Op::Dead {
                continue;
            }
            for (k, &inp) in n.inputs.iter().enumerate() {
                if inp == cast && crate::ir::is_memory_token_slot(n, k) {
                    // Cast: arena index to node id.
                    token_uses.push((u as NodeId, k));
                }
            }
        }
        graph.replace_all_uses(cast, obj);
        for (u, k) in token_uses {
            graph.set_input(u, k, cast);
        }
        if !graph.checkcast_value_forwarded.contains(&cast) {
            graph.checkcast_value_forwarded.push(cast);
        }
        forwarded += 1;
    }
    forwarded
}



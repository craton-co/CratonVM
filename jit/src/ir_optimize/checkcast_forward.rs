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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_CHECKCAST_VALUE_FORWARD")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrType, MemKind, SafepointSnapshot};

    const SW: &str = "CRATONVM_JIT_IR_CHECKCAST_VALUE_FORWARD";

    /// `return ((Sub) a[0]).v;` with a frame naming the cast result and a
    /// field read taking the cast as both its memory token and its base.
    /// Returns `(graph, element, cast, field read)`.
    fn cast_then_read() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let el = g.add(Op::ArrayLoad(MemKind::Ref), IrType::Ref, vec![ctrl, mem0, a, zero], Some(3));
        let cast = g.add(
            Op::CheckCast { name_ptr: 0, name_len: 0 },
            IrType::Ref,
            vec![ctrl, mem0, el],
            Some(4),
        );
        let read = g.add(Op::Load(MemKind::Int), IrType::Int, vec![ctrl, cast, cast], Some(7));
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, read], Some(10));
        g.push_safepoint(SafepointSnapshot::new(7, vec![a, cast], vec![cast], vec![]));
        (g, el, cast, read)
    }

    #[test]
    fn r23_w8_l7_a_cast_result_is_read_through_its_operand() {
        let (mut g, el, cast, read) = cast_then_read();
        assert_eq!(forward_checkcast_values(&mut g), 1);
        // The value slot reads the operand; the memory slot keeps the cast,
        // so the read stays ordered after the check.
        let r = &g.nodes[read as usize];
        assert!(crate::ir::is_memory_token_slot(r, 1));
        assert_eq!(r.inputs[1], cast, "memory: still after the cast");
        assert_eq!(r.inputs[2], el, "base: the operand");
        // The frame names the operand.
        assert_eq!(g.safepoints[0].locals[1], el);
        assert_eq!(g.safepoints[0].stack[0], el);
        // The cast stays, listed, still on its own inputs.
        assert!(matches!(g.nodes[cast as usize].op, Op::CheckCast { .. }));
        assert_eq!(g.nodes[cast as usize].inputs[2], el);
        assert_eq!(g.checkcast_value_forwarded, vec![cast]);
        // It is still a DCE root (owes its ClassCastException).
        assert!(super::super::is_dce_root(&g.nodes[cast as usize].op));
    }

    #[test]
    fn r23_w8_l7_osr_graphs_keep_their_casts_and_the_switch_kills_it() {
        let (mut g, _, cast, read) = cast_then_read();
        g.osr_compile = true;
        assert_eq!(forward_checkcast_values(&mut g), 0);
        assert_eq!(g.nodes[read as usize].inputs[2], cast);
        assert!(g.checkcast_value_forwarded.is_empty());
        let (mut g, _, cast, read) = cast_then_read();
        g.optimize_osr_route = true;
        assert_eq!(forward_checkcast_values(&mut g), 0);
        assert_eq!(g.nodes[read as usize].inputs[2], cast);
        cratonvm_types::flags::with_thread_overrides(&[(SW, Some("0"))], || {
            assert!(!checkcast_value_forward_enabled());
        });
        cratonvm_types::flags::with_thread_overrides(&[(SW, None)], || {
            assert!(checkcast_value_forward_enabled(), "default ON");
        });
    }

    /// JIT r23 wave 9 (`CRATONVM_JIT_IR_CHECKCAST_HIT_TO_JOIN`): the
    /// `CheckCast` arm of `ir_lower` sends a forwarded cast's exact hit and
    /// null branch straight to its join, and only a forwarded cast. Pinned
    /// against the arm's source (the inline path needs a VM-interned target,
    /// which no unit graph has).
    #[test]
    fn r23_w9_l7_a_forwarded_casts_hits_jump_straight_to_the_join() {
        let src = include_str!("../ir_lower.rs").replace("\r\n", "\n");
        let arm = src
            .split("Op::CheckCast { name_ptr, name_len } => {")
            .nth(1)
            .expect("the CheckCast arm")
            .split("// ── cov-05 increment 1: instanceof")
            .next()
            .expect("the arm ends at instanceof");
        assert!(arm.contains(
            "let hit_to_join = result_forwarded && ir_checkcast_hit_to_join_enabled();"
        ));
        // The exact hit and round 21's row-first hits travel together.
        assert!(arm.contains("to_join.append(&mut hits);"), "the exact hit goes to the join");
        assert!(arm.contains("to_join.push(null_slow);"), "the null goes to the join");
        // Both are gated: the old patches remain for every other cast.
        assert!(arm.contains("for p in hits {\n                                    self.patch_rel32_to_here(p);"));
        assert!(arm.contains("self.patch_rel32_to_target(null_slow, hit_at);"));
        // The join patches come after the helper call and the hit jump.
        // Round 21's TCPERF21-1 spells `emit_call_return_check` out around
        // its FP reloads; the sentinel tail ends it.
        let call = arm.find("self.emit_call_return_sentinel_tail(IrType::Ref);").expect("call");
        let joins = arm.find("for p in to_join {").expect("join patches");
        assert!(joins > call, "patched at the join, after the slow path");
        const SW2: &str = "CRATONVM_JIT_IR_CHECKCAST_HIT_TO_JOIN";
        cratonvm_types::flags::with_thread_overrides(&[(SW2, Some("0"))], || {
            assert!(!crate::ir_lower::ir_checkcast_hit_to_join_enabled());
        });
        cratonvm_types::flags::with_thread_overrides(&[(SW2, None)], || {
            assert!(
                crate::ir_lower::ir_checkcast_hit_to_join_enabled(),
                "default ON since j27 2026-10-28"
            );
        });
        cratonvm_types::flags::with_thread_overrides(&[(SW2, Some("1"))], || {
            assert!(crate::ir_lower::ir_checkcast_hit_to_join_enabled());
        });
    }
}

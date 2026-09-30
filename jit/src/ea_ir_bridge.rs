// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The bridge between the production IR graph and the escape-analysis
//! connection graph: conversion in, plan formation, and application out.
//!
//! Split out of `lib.rs` on 2026-09-16, following `jfr_compile_decision`. This
//! is the one boundary on the escape-analysis path that was never drawn, and
//! the reason it was never drawn is exactly why it needed drawing.
//!
//! `escape_analysis.rs` is deliberately IR-agnostic: it knows its own node and
//! op vocabulary and nothing about `ir::Graph`, which is what lets it be tested
//! and reasoned about on tiny hand-built graphs. `ir.rs` is deliberately
//! escape-agnostic: a node has no idea whether the object it allocates escapes.
//! Both of those are properties worth keeping. But something has to speak both
//! languages at once, and that something belongs to neither module -- so it
//! ended up in `lib.rs` by default rather than by decision.
//!
//! That translator is what lives here, in the three directions it runs:
//!
//!   1. IN -- [`escape_analysis_from_ir`] and its op/edge decoders
//!      (`ir_op_to_ea_op`, `ir_load_store_field_index`, the cold-control
//!      marking fed from branch hints). This half decides what the connection
//!      graph is even allowed to see, and every over-approximation the analysis
//!      is accused of starts here.
//!   2. PLAN -- `plan_scalar_replacement` and
//!      [`build_scalar_replacement_map_with_plans`] turn an EA result into a
//!      concrete per-victim rewrite, including the deopt-describability check
//!      that decides whether an elided object can be reconstituted at an exit.
//!   3. OUT -- [`apply_ea_to_ir_planned`] (and its test-only wrappers
//!      `apply_ea_to_ir_pinned` and `apply_ea_to_ir`) mutates the real
//!      graph: kill the allocation, splice the memory-token chain, rewrite the
//!      frame states.
//!
//! The snapshot and dead-local pruning passes travelled with it rather than
//! staying behind, and that is not padding. They exist because the EA rewrite
//! is what makes a snapshot unconsumable or a local dead, and each of them runs
//! inside this same window of the pipeline. Splitting them from the rewrite
//! that creates their work would have left two files that can only be read
//! together. [`ir_verify_reject`] came along for the same reason: it is how
//! each of these mutating passes re-proves the graph before the next one runs.
//!
//! The scalar-replacement and lock-coarsening census counters are declared here
//! too, because this module is the only place that can increment them
//! truthfully -- they count plans OFFERED against plans APPLIED, and only the
//! translator sees both numbers.
//!
//! Glob-re-exported from the crate root, so every path a caller used before the
//! split still resolves -- the code moved, the API did not.

use std::collections::HashMap;

use crate::{
    bailout, deopt, escape_analysis, ir, ir_lower, ir_stage_reporting, ir_verify, metrics,
    scalar_deopt_descriptor_available, CompiledMethod, JitInvokeInfo,
};

// ---------------------------------------------------------------------------
// Escape analysis: IR graph → EA graph conversion
// ---------------------------------------------------------------------------

/// The instance-field index a full-layout `Op::Load`/`Op::Store` accesses,
/// recovered from its `Const(field_index)` offset operand (input[3]). Returns
/// `None` for a compact / malformed node (no constant offset), letting the
/// caller fall back to the `MemKind`-derived index. This is the single place
/// the EA bridge interprets a production field access's index.
///
/// `plan_scalar_replacement` no longer *forwards* through this index — it asks
/// `escape_analysis::ScalarReplacementInfo::load_value`, which is keyed by node
/// rather than by field — but it still calls this as an agreement check, and
/// `virtual_object_info_for` still indexes `field_values` by it. So the rule
/// stands: the field index the EA graph keyed its edges by and the one the
/// consumers recover here must be the same index.
fn ir_load_store_field_index(ir_graph: &ir::Graph, node_id: ir::NodeId) -> Option<usize> {
    let node = ir_graph.nodes.get(node_id as usize)?;
    let offset_node = *node.inputs.get(3)?;
    match ir_graph.nodes.get(offset_node as usize)?.op {
        ir::Op::Const(v) if v >= 0 => Some(v as usize),
        _ => None,
    }
}

/// The constant element index of an `Op::ArrayLoad` / `Op::ArrayStore`, or
/// `None` when the index is not a compile-time constant.
///
/// The array analogue of [`ir_load_store_field_index`], and it has to be a
/// separate function because the operand sits at a different slot: an array
/// access is `[ctrl, mem, array, index, (value)]`, so the index is input 3 for
/// both — but the *base* is input 2 rather than the field node's `offset` being
/// at 3. Keeping them apart is what stops a field index and an element index
/// being read out of each other's slot.
fn ir_array_const_index(ir_graph: &ir::Graph, node_id: ir::NodeId) -> Option<usize> {
    let node = ir_graph.nodes.get(node_id as usize)?;
    if !matches!(node.op, ir::Op::ArrayLoad(_) | ir::Op::ArrayStore(_)) {
        return None;
    }
    let index_node = *node.inputs.get(3)?;
    match ir_graph.nodes.get(index_node as usize)?.op {
        ir::Op::Const(v) if v >= 0 => Some(v as usize),
        _ => None,
    }
}

/// The constant length of an `Op::NewArray` (`[ctrl, mem, length]`), or `None`.
///
/// A negative constant is `None` rather than a length: `newarray -1` throws
/// `NegativeArraySizeException`, and deleting the allocation would delete the
/// throw.
fn ir_new_array_const_length(ir_graph: &ir::Graph, node_id: ir::NodeId) -> Option<usize> {
    let node = ir_graph.nodes.get(node_id as usize)?;
    if !matches!(node.op, ir::Op::NewArray { .. }) {
        return None;
    }
    let len_node = *node.inputs.get(2)?;
    match ir_graph.nodes.get(len_node as usize)?.op {
        ir::Op::Const(v) if v >= 0 => Some(v as usize),
        _ => None,
    }
}

/// Convert an `ir::Graph` to an `escape_analysis::Graph` for standalone
/// escape analysis.  The two modules define independent `Op` / `Node` /
/// `Graph` types, so we translate node-by-node.  Returns both the EA graph
/// and the id_map (`ir::NodeId` index → `escape_analysis::NodeId` value)
/// so callers can map EA results back to IR node IDs.
pub(crate) fn escape_analysis_from_ir(
    ir_graph: &ir::Graph,
) -> (escape_analysis::Graph, Vec<escape_analysis::NodeId>) {
    let mut ea = escape_analysis::Graph::new();

    // Map from ir::NodeId → ea::NodeId.  Start/Return are pre-created as
    // ea nodes 0 and 1, matching the IR entry/exit.
    let ir_node_count = ir_graph.nodes.len();
    let mut id_map: Vec<escape_analysis::NodeId> = Vec::with_capacity(ir_node_count);

    // EA graph already has nodes 0 (Start) and 1 (Return).
    // Map IR entry and exit to those, everything else gets added below.
    for i in 0..ir_node_count {
        let ir_id = i as ir::NodeId;
        if ir_id == ir_graph.entry {
            id_map.push(0); // EA Start
        } else if ir_id == ir_graph.exit {
            id_map.push(1); // EA Return
        } else {
            // Placeholder — filled in the second pass.
            id_map.push(usize::MAX);
        }
    }

    // IR nodes this bridge classified as `escape_analysis::Op::IdentityHash`.
    // Recorded in the first pass and consulted in the second, so the op choice
    // and the operand re-packing can never disagree about which calls are
    // intrinsics (an `IdentityHash` whose input 0 is the CONTROL edge would
    // attribute the observation to the wrong node — or to none).
    let mut identity_hash_calls: std::collections::HashSet<usize> =
        std::collections::HashSet::new();

    // First pass: create EA nodes for everything except entry/exit.
    for i in 0..ir_node_count {
        let ir_id = i as ir::NodeId;
        if ir_id == ir_graph.entry || ir_id == ir_graph.exit {
            continue;
        }
        let ir_node = &ir_graph.nodes[i];
        // The production builder emits full-layout `Op::Load`/`Op::Store`
        // (`[ctrl, mem, base, offset, (value)]`) carrying a `MemKind`, but the
        // EA graph keys field edges by a real **field index**. Recover it from
        // the `Const(field_index)` offset operand (input[3]); fall back to the
        // `MemKind`-derived index (`ir_op_to_ea_op`) only when the offset isn't
        // a constant (a malformed/compact node EA already treats
        // conservatively).
        let ea_op = match &ir_node.op {
            ir::Op::Load(_) => match ir_load_store_field_index(ir_graph, i as ir::NodeId) {
                Some(f) => escape_analysis::Op::Load(f),
                None => ir_op_to_ea_op(&ir_node.op),
            },
            ir::Op::Store(_) => match ir_load_store_field_index(ir_graph, i as ir::NodeId) {
                Some(f) => escape_analysis::Op::Store(f),
                None => ir_op_to_ea_op(&ir_node.op),
            },
            // ── Identity observations (docs/jit/escape-analysis.md §4, §6.2) ──
            //
            // Both arms below were previously fail-closed *by accident*: an
            // `ir::Op::Cmp` hit `ir_op_to_ea_op`'s `Op::Other` catch-all and an
            // identity-hash call hit `EaOp::Call`, whose rule escapes every
            // reference argument. The outcome was right and the reason was not
            // — it was unmeasurable, and it would silently become WRONG if
            // `Op::Other` were relaxed or `Op::Call` ever gained an "inlined
            // intrinsic, does not escape" arm.
            //
            // `EaOp::RefCompare` observes ALL its inputs and `EaOp::IdentityHash`
            // observes input 0 (`escape_analysis::find_identity_observations`),
            // and neither escapes anything — so misclassifying a node INTO
            // either variant would be a relaxation. Both predicates are
            // therefore written to fire only on a shape that provably is one.
            // ── Array element access ──────────────────────────────────
            //
            // An element access with a CONSTANT index is a field access on the
            // array: slot `i` of an N-slot allocation. Mapping it that way is
            // what lets a small constant-length array be scalar-replaced at
            // all, and it is sound in both directions:
            //
            //   * the array is an allocation of THIS graph -> the analysis
            //     resolves the holder, `field_in_range` checks `i < len`
            //     against the array's own proven length, and an out-of-range
            //     constant index is refused (its `ArrayIndexOutOfBounds` throw
            //     must survive);
            //   * the array is anything else (a `Param`, a call result) -> the
            //     holder resolves to no allocation, `field_in_range` answers
            //     false, and the access takes the same conservative path a
            //     field access on an unknown holder takes.
            //
            // A NON-constant index keeps the old `EaOp::Call` mapping, which
            // arg-escapes the array reference. That is the fail-closed answer:
            // the access names a slot the analysis cannot identify, so no slot
            // may be folded and the array must stay on the heap. It also means
            // an array with even ONE unresolvable access is refused as a whole,
            // which is exactly right.
            ir::Op::ArrayLoad(_) => match ir_array_const_index(ir_graph, i as ir::NodeId) {
                Some(idx) => escape_analysis::Op::Load(idx),
                None => escape_analysis::Op::Call,
            },
            ir::Op::ArrayStore(_) => match ir_array_const_index(ir_graph, i as ir::NodeId) {
                Some(idx) => escape_analysis::Op::Store(idx),
                None => escape_analysis::Op::Call,
            },
            // A `newarray` whose length is a compile-time constant has a slot
            // count, which is the one fact array scalar replacement needs.
            ir::Op::NewArray { element_type, .. } => escape_analysis::Op::NewArray {
                element_type: *element_type,
                length: ir_new_array_const_length(ir_graph, i as ir::NodeId),
            },
            ir::Op::Cmp(_) if ir_cmp_is_ref_compare(ir_graph, ir_node) => {
                escape_analysis::Op::RefCompare
            }
            ir::Op::Call { info_ptr } if ir_call_is_identity_hash(ir_node, *info_ptr) => {
                identity_hash_calls.insert(i);
                escape_analysis::Op::IdentityHash
            }
            _ => ir_op_to_ea_op(&ir_node.op),
        };
        // Don't wire inputs yet — we need all id_map entries populated.
        let ea_id = ea.add_node(ea_op, vec![]);
        id_map[i] = ea_id;
    }

    // Block side table (`escape_analysis::Graph::blocks`). Only the ops whose
    // control is pinned at input 0 get an entry, and only when that input names
    // a real control node — a compact or malformed node contributes nothing and
    // is then simply not eligible for the one-block dominance proof.
    //
    // `Op::Guard` is deliberately absent from the "is this a block boundary"
    // question: it produces no control token (`ir::Op::is_control`), so a null
    // or bounds check does not end a block and the accesses either side of it
    // still name the same control node.
    for i in 0..ir_node_count {
        let ir_node = &ir_graph.nodes[i];
        if !matches!(
            ir_node.op,
            ir::Op::Load(_)
                | ir::Op::Store(_)
                | ir::Op::ArrayLoad(_)
                | ir::Op::ArrayStore(_)
                | ir::Op::New { .. }
                | ir::Op::NewArray { .. }
        ) {
            continue;
        }
        let Some(&ctrl) = ir_node.inputs.first() else {
            continue;
        };
        let Some(ctrl_node) = ir_graph.nodes.get(ctrl as usize) else {
            continue;
        };
        if !ctrl_node.op.is_control() {
            continue;
        }
        let (ea_id, ea_ctrl) = (id_map[i], id_map[ctrl as usize]);
        if ea_id != usize::MAX && ea_ctrl != usize::MAX {
            ea.blocks.insert(ea_id, ea_ctrl);
        }
    }

    // Second pass: wire inputs (skip Start/Return whose inputs are already set).
    for i in 0..ir_node_count {
        let ir_id = i as ir::NodeId;
        if ir_id == ir_graph.entry {
            continue;
        }
        let ea_id = id_map[i];
        let ir_node = &ir_graph.nodes[i];
        // The EA graph reads memory-access operands positionally in a *compact*
        // layout (`Store [holder, value]`, `Load [holder]`); the full-layout IR
        // node places the holder at input[2] and the store value at input[4].
        // Translate those two shapes; everything else is forwarded verbatim.
        // (A node whose expected operand is missing yields an empty input list,
        // which EA's `store_holder`/`load_holder` treat conservatively.)
        let map_id = |inp: ir::NodeId| -> escape_analysis::NodeId {
            id_map.get(inp as usize).copied().unwrap_or(usize::MAX)
        };
        let ea_inputs: Vec<escape_analysis::NodeId> = match &ir_node.op {
            ir::Op::Store(_) if ir_node.inputs.len() >= 5 => {
                vec![map_id(ir_node.inputs[2]), map_id(ir_node.inputs[4])]
            }
            ir::Op::Load(_) if ir_node.inputs.len() >= 3 => vec![map_id(ir_node.inputs[2])],
            // Array element access, re-packed into the same compact layout the
            // EA graph reads field access in: `Store [holder, value]`,
            // `Load [holder]`. The array reference is input 2 and the stored
            // value input 4; the index is NOT an EA operand at all -- it was
            // folded into the `Op::Load(i)` / `Op::Store(i)` payload above.
            // Forwarding verbatim would make the CONTROL edge the holder.
            //
            // Guarded on the node still being classified as a field op: an
            // access whose index was not constant kept the `EaOp::Call`
            // mapping, and a `Call` must keep ALL its reference operands or the
            // arg-escape rule stops seeing the array.
            ir::Op::ArrayStore(_)
                if ir_node.inputs.len() >= 5
                    && matches!(ea.nodes[ea_id].op, escape_analysis::Op::Store(_)) =>
            {
                vec![map_id(ir_node.inputs[2]), map_id(ir_node.inputs[4])]
            }
            ir::Op::ArrayLoad(_)
                if ir_node.inputs.len() >= 4
                    && matches!(ea.nodes[ea_id].op, escape_analysis::Op::Load(_)) =>
            {
                vec![map_id(ir_node.inputs[2])]
            }
            // EA layout contract: the locked reference is input 0. The IR node
            // carries `[ctrl, mem, obj]`, exactly like `Load`/`Store`, so
            // forwarding verbatim would attribute the monitor to the MEMORY
            // TOKEN and every lock decision would be made about the wrong node.
            ir::Op::MonitorEnter | ir::Op::MonitorExit if ir_node.inputs.len() >= 3 => {
                vec![map_id(ir_node.inputs[2])]
            }
            // `escape_analysis::Op::IdentityHash` reads the observed reference
            // at input 0 — the same slot `Op::MonitorEnter` uses, so the two
            // agree about which object a header read names. The IR call carries
            // `[ctrl, mem, args…]`, so the observed reference is input 2.
            // Forwarding verbatim would attribute the observation to the
            // CONTROL edge and let the real receiver slip through unobserved.
            // `ir_call_is_identity_hash` already required the arity.
            ir::Op::Call { .. } if identity_hash_calls.contains(&i) => {
                vec![map_id(ir_node.inputs[2])]
            }
            // `Op::Cmp` → `Op::RefCompare` needs NO re-packing: the IR node's
            // inputs are `[left, right]` and `find_identity_observations` reads
            // every input of a `RefCompare`.
            //
            // Everything else is forwarded verbatim EXCEPT its incoming memory
            // token (r9-ea). A token is an ordering edge naming the previous
            // memory operation, not a value the node reads — and the builder
            // threads every `getfield` onto the chain (`self.mem = load`), so
            // the token of a call, `ldc`, `getstatic`, `checkcast`,
            // `instanceof`, `athrow` or non-constant array access is often a
            // field `Op::Load`. `is_ref_producer` accepts a `Load`, so the
            // `Call` / `Other` / `Throw` escape rules escaped it, and through
            // its field edges every allocation that load could read:
            // `Box b = pair.box; foo();` escaped `b`'s allocation for no reason
            // but the order the two instructions ran in.
            //
            // Dropping the slot removes no real edge — nothing can reach an
            // object through an ordering token — and `ir::memory_token_slot` is
            // the canonical answer for which slot that is (arity-guarded, so a
            // compact hand-built node keeps every input). A memory-typed `Op::Phi`
            // has no single token slot and keeps its inputs; it is used only as
            // another node's token, which this arm now drops.
            _ => {
                let token = ir::memory_token_slot(ir_node);
                ir_node
                    .inputs
                    .iter()
                    .enumerate()
                    .filter(|&(k, _)| Some(k) != token)
                    .map(|(_, &inp)| map_id(inp))
                    .collect()
            }
        };
        ea.nodes[ea_id].inputs = ea_inputs.clone();
        // The load's memory token, for `escape_analysis::Graph::load_mem_tokens`
        // (r9-ea): the chain order the dominance stand-ins cross-check their
        // id order against. Only for a node the first pass classified as an EA
        // field/element `Load`; a token that did not map is simply not recorded,
        // which leaves that load to the id order alone.
        //
        // And, for every memory operation, its link in the chain itself
        // (`escape_analysis::Graph::mem_pred`, round 11 wave 2), which the
        // analysis walks instead of comparing token ids.
        let token = ir::memory_token_slot(ir_node).and_then(|slot| ir_node.inputs.get(slot));
        if let Some(&tok) = token {
            let ea_tok = map_id(tok);
            if ea_tok != usize::MAX {
                ea.mem_pred.insert(ea_id, ea_tok);
                if matches!(ea.nodes[ea_id].op, escape_analysis::Op::Load(_)) {
                    ea.load_mem_tokens.insert(ea_id, ea_tok);
                }
            }
        }
        // Rebuild use-edges for the targets.
        for &inp_ea in &ea_inputs {
            if inp_ea < ea.nodes.len() {
                ea.nodes[inp_ea].uses.push(ea_id);
            }
        }
    }

    (ea, id_map)
}

/// True when `node` (an `ir::Op::Cmp`) is an `if_acmpeq`/`if_acmpne` — a
/// comparison of two object *addresses*, which is one of the three ways the JVM
/// observes an object's identity without publishing it.
///
/// Two shapes are deliberately excluded, because neither observes an address:
///
/// * a comparison with a non-`Ref` operand — an `if_icmp*` or a `lcmp`/`fcmp`
///   result test, which never sees a reference at all;
/// * **`ifnull` / `ifnonnull`**. The builder lowers those to the same
///   `Op::Cmp` against `aconst_null`, which is an `Op::Const(0)` typed
///   `IrType::Ref` — so a pure "both operands are `Ref`" test would classify
///   every null check in the program as an identity observation and refuse
///   scalar replacement for every object that is ever null-checked. A fresh
///   allocation is non-null by construction, so `o == null` reads no address;
///   it is answerable without the object existing. Only the *null literal* is
///   excluded, not `Op::Const` in general — an interned constant oop, if one is
///   ever introduced, must keep being treated as a real reference.
fn ir_cmp_is_ref_compare(ir_graph: &ir::Graph, node: &ir::Node) -> bool {
    if node.inputs.len() < 2 {
        return false;
    }
    node.inputs[..2].iter().all(|&inp| {
        ir_graph
            .nodes
            .get(inp as usize)
            .is_some_and(|n| n.ty == ir::IrType::Ref && n.op != ir::Op::Const(0))
    })
}

/// True when an `ir::Op::Call` is a **resolved** identity-hash intrinsic, whose
/// argument's address is therefore observed.
///
/// This is a *relaxation* — it stops the call escaping its reference arguments
/// through the `escape_analysis::Op::Call` rule — so it fires only where the
/// target is provably `Object`'s header-derived hash, never on a virtual call
/// that could dispatch to an override:
///
/// * `System.identityHashCode` is `static`: its answer is the header hash of
///   whatever it is handed, whatever that object's class does with `hashCode`.
///   It is the ONLY shape accepted.
/// * `Object.hashCode()` is NOT accepted, not even as an `invokespecial`
///   (r9-ea). This arm used to take `invokespecial java/lang/Object.hashCode()I`
///   on the reasoning that a non-virtual call "provably lands on `Object`'s
///   implementation". It does not: under `ACC_SUPER` (set by every modern
///   compiler) an `invokespecial` naming a SUPERCLASS of the current class
///   selects starting at the current class's DIRECT superclass (JVMS §6.5
///   `invokespecial`), so it runs the nearest override between the two —
///   arbitrary Java that may publish its receiver. javac names `Object` there
///   only when `Object` IS the direct superclass, but a spliced body from
///   another compiler need not, and classifying that call as an escape-free
///   identity observation lets lock elision delete a monitor on an object the
///   override just handed to another thread. A *virtual* `hashCode()` was
///   already refused for the same reason. The cost: a `super.hashCode()`
///   receiver now arg-escapes through the ordinary `EaOp::Call` rule, and it
///   was refused scalar replacement either way.
///
/// The arity check is load-bearing: the second pass re-packs the observed
/// reference from input 2 into `EaOp::IdentityHash`'s input 0, and an
/// `IdentityHash` with no input observes *nothing* while also no longer
/// escaping the receiver.
///
/// SAFETY / contract: `info_ptr` is the address of a `JitInvokeInfo` kept alive
/// for the whole compile by the `CompiledMethod`'s `_jit_invoke_infos` arena —
/// the same contract `ir_lower`'s `Op::Call` arm relies on when it reads
/// `invoke_kind`. A hand-built test graph that wants an `Op::Call` must use
/// `info_ptr: 0` (rejected here without a dereference) or a real interned info.
fn ir_call_is_identity_hash(node: &ir::Node, info_ptr: usize) -> bool {
    // `[ctrl, mem, arg0]` — the observed reference is arg0.
    if info_ptr == 0 || node.inputs.len() < 3 {
        return false;
    }
    // SAFETY: `info_ptr` is non-zero here and addresses an interned `JitInvokeInfo`; see this function's contract.
    let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
    match (info.class_name, info.method_name, info.descriptor) {
        // invoke_kind 3 = static.
        ("java/lang/System", "identityHashCode", "(Ljava/lang/Object;)I") => info.invoke_kind == 3,
        // `Object.hashCode()` deliberately absent, even as `invokespecial` --
        // see this function's doc.
        _ => false,
    }
}

// ── The monitor half of the bridge (docs/jit/lock-elimination.md §6.2) ──
//
// WIRED. `ir::Op::MonitorEnter`/`MonitorExit` exist, and both passes of
// `escape_analysis_from_ir` map them: the op choice in `ir_op_to_ea_op`, and the
// operand re-packing that forwards `inputs[2]`, the locked reference. (The IR
// node carries `[ctrl, mem, obj]`; forwarding it verbatim would attribute the
// monitor to the memory token.) `apply_ea_to_ir`'s elision loop is
// all-or-nothing per object, the precondition `lock-elimination.md` §6.1 sets.
//
// `Object.wait`/`notify`/`notifyAll` ⇒ `EaOp::MonitorWait`/`MonitorNotify` are
// still NOT wired. Those mappings are relaxations (they stop escaping the
// receiver through the `Op::Call` rule), so they must land together with a test
// that a waited-on receiver is refused elision (`waits_or_notifies_on`, the `E3`
// refusal), not before.
//
// `athrow` ⇒ `EaOp::Throw` — WIRED, cov-07. `ir::Op::Throw` now exists
// (`IrBuilder::build`'s `0xbf` arm) and maps below with NO operand
// re-packing: its `[ctrl, mem, exc]` layout is forwarded verbatim by the
// second pass's default arm, exactly like `Op::Return`'s `[ctrl, val]` — the
// escape rule at `escape_analysis::Op::Return | Op::Throw` iterates every
// input and filters by `is_ref_producer`, so the extra non-ref `ctrl`/`mem`
// inputs are harmless.
//
// `escape_analysis::program_order_proves_dominance` was NOT given a
// `may_throw` term, and does not need one: `Op::Throw` always LEAVES the
// frame (the compiled body never branches to an in-method handler — see
// `Op::Throw`'s own doc comment in `ir.rs`), so it cannot create a control
// edge back to a lower-id load, which is the only thing that predicate
// guards against. See `cov-07-athrow-RETIRED-*.md`.

/// Map a single `ir::Op` variant to its `escape_analysis::Op` counterpart.
fn ir_op_to_ea_op(op: &ir::Op) -> escape_analysis::Op {
    use escape_analysis::Op as EaOp;
    match op {
        ir::Op::Start => EaOp::Start,
        ir::Op::Return => EaOp::Return,
        ir::Op::If => EaOp::If,
        // A switch diverges control exactly as an `If` does, and the EA side
        // has no other way to learn that. As `Other` it was invisible to
        // `escape_analysis::program_order_proves_dominance`, and once
        // `collapse_trivial_merges` (LICM, unroll) had removed the single-input
        // merge at each case target, a method whose cases all return carried
        // no `If`, `Merge` or multi-input φ at all: `switch (k) { case 0:
        // o.x = 5; return 100; default: o.y = 1; return o.x; }` forwarded the
        // case-0 store to the default arm's load and returned 5 (round 11).
        // Its `[ctrl, key]` layout is the `If`'s `[ctrl, cond]`, and the key is
        // never a reference.
        ir::Op::Switch { .. } => EaOp::If,
        // A loop-header join is a join. It used to fall into `Other`, which
        // `program_order_proves_dominance` does not refuse (round 11 wave 2).
        ir::Op::Merge | ir::Op::Region => EaOp::Merge,
        ir::Op::Phi => EaOp::Phi,
        ir::Op::Const(v) => EaOp::Const(*v),
        ir::Op::Param(idx) => EaOp::Param(*idx),
        ir::Op::Add => EaOp::Add,
        ir::Op::Sub => EaOp::Sub,
        ir::Op::Mul => EaOp::Mul,
        ir::Op::Load(mk) => EaOp::Load(*mk as usize),
        ir::Op::Store(mk) => EaOp::Store(*mk as usize),
        // Monitors: the variants now exist, so lock elision and coarsening can
        // finally see the operations they were written for. Operand re-packing
        // is in the second pass — forwarding `[ctrl, mem, obj]` verbatim would
        // attribute the monitor to the memory token.
        ir::Op::MonitorEnter => EaOp::MonitorEnter,
        ir::Op::MonitorExit => EaOp::MonitorExit,
        // cov-07. No re-packing needed — see the section comment above.
        ir::Op::Throw => EaOp::Throw,
        ir::Op::New {
            class_id,
            num_fields,
        } => EaOp::New {
            class_id: *class_id,
            num_fields: *num_fields,
        },
        // No graph to ask, so no length: `None` is the fail-closed answer and
        // makes the array a non-candidate. `escape_analysis_from_ir` fills the
        // length itself for the nodes it bridges.
        ir::Op::NewArray { element_type, .. } => EaOp::NewArray {
            element_type: *element_type,
            length: None,
        },
        ir::Op::Call { .. } => EaOp::Call,
        // cov-01. `EaOp::Call` is the conservative mapping and the honest one:
        // all three lower to a runtime helper that can run arbitrary Java, and
        // their memory shape in `ir::Op::memory_shape` is already
        // `MemAccess::Opaque` for that reason — the two classifications must
        // not disagree. None of the three has a reference INPUT, so the
        // arg-escape half of the `Call` rule is a no-op; what matters is that
        // the reference each PRODUCES is treated like a call result rather than
        // like a fresh allocation, because none of them is one. An interned
        // literal, a class mirror and a static field's referent are all
        // pre-existing objects that other threads can already see.
        ir::Op::ConstString { .. } | ir::Op::ConstClass { .. } | ir::Op::LoadStatic { .. } => {
            EaOp::Call
        }
        // `putstatic` (round 11 wave 10). A static write publishes the stored
        // reference to every thread, so the honest state is `GlobalEscape`,
        // which is what `EaOp::Other`'s rule gives its reference operands --
        // `EaOp::Call` would say only `ArgEscape` ("reaches a callee"), and put
        // the object on the (test-only) `stack_allocatable` list. Both refuse
        // scalar replacement and lock elision; stated here rather than left
        // to the catch-all so the choice is visible. The memory token is
        // dropped by the second pass, so the value is the only operand EA sees.
        ir::Op::StoreStatic { .. } => EaOp::Other,
        ir::Op::ArrayLength => EaOp::ArrayLength,
        // Array element access, for the callers that have no graph to ask.
        // `escape_analysis_from_ir` no longer reaches this arm: it classifies
        // an access with a CONSTANT index as a field access on the array (see
        // its first pass), which is what makes a small constant-length array
        // replaceable. Everything else — a non-constant index, and every
        // caller of this graph-free function — keeps `EaOp::Call`, whose
        // handling marks every reference input `ArgEscape` and so refuses the
        // array. That is the fail-closed answer for an access naming a slot
        // nobody could identify.
        ir::Op::ArrayLoad(_) | ir::Op::ArrayStore(_) => EaOp::Call,
        // Round 11 wave 14 (lane osrmerge): the opaque OSR merge. Stated, not
        // left to the catch-all, because the design page suggested
        // `EaOp::Param` for `OsrLocal` and that would be wrong here: a
        // `Param(idx)` is an ARGUMENT index to this analysis, and an OSR local
        // is a JVM local slot. `Other` is the fail-closed answer on every
        // consumer that matters — a φ merging an allocation with it fails the
        // scalar-replacement singleton test, and `ref_operand_origin` answers
        // `Unknown` for it, so no lock on such a φ is elided. The flag and the
        // memory state carry no reference at all.
        ir::Op::OsrLocal(_) | ir::Op::OsrEntryFlag { .. } | ir::Op::OsrMemory => EaOp::Other,
        ir::Op::Dead => EaOp::Dead,
        // Any control op not named above is one this mapping has not been
        // taught. `program_order_proves_dominance` is a denylist over EA ops,
        // so mapping it to `Other` would let it split control unseen, which is
        // how `Switch` reopened the id-order hole. `If` makes the analysis
        // refuse to read id order as dominance instead. (A `Proj` only names
        // one output of a control op that is already classified.)
        other if other.is_control() && !matches!(other, ir::Op::Proj(_)) => EaOp::If,
        // All other IR ops (Proj, ConstF, conversions, bitwise, Cmp, Div, Rem,
        // Neg, etc.) have no EA-specific behaviour.
        _ => EaOp::Other,
    }
}

// ---------------------------------------------------------------------------
// Escape analysis: apply EA results to the IR graph
// ---------------------------------------------------------------------------

// ── Memory-token chain surgery (EA-local twin of `ir_optimize`'s) ────
//
// Every memory-effecting node carries an incoming *memory token* naming the
// previous writer and hands itself on as the token for the next one. That chain
// is the only ordering relation the IR has between two writes. Marking such a
// node `Op::Dead` without first rewiring its token consumers leaves the
// surviving memory operation's token slot pointing into the hole: it has lost
// the transitive ordering edge to everything written before the deleted node,
// and the scheduler is then free to hoist it above those writes. That is the
// defect `ir_optimize::eliminate_dead_stores` was fixed for in f00cc5dd8; this
// pass had exactly the same hole — for its stores, its loads AND its `Op::New`.
//
// The token predicates are not copies: `ea_memory_token_slot` /
// `ea_is_memory_token_slot` below delegate to `ir::memory_token_slot` /
// `ir::is_memory_token_slot`, the single table `ir_optimize` delegates to as
// well. (This note used to call them private twins of `ir_optimize`'s, at line
// numbers long gone; the second one still carried its own φ arm.) The *kill*
// helper,
// `kill_store_splicing_memory_chain`, is deliberately NOT reused even if it
// were public: it is `Op::Store`-only, and it refuses any node a safepoint slot
// names — whereas an eliminated `Op::New` must KEEP being named by its snapshot
// slots, because that slot is the virtual-object descriptor
// `ir_lower::resolve_frame_state` resolves through `ScalarReplacementMap`.

// ---------------------------------------------------------------------------
// Escape analysis: cold-path information
// ---------------------------------------------------------------------------

/// The control-flow predecessors of `node`, as input indices.
///
/// This is the verifier's table, `ir_verify::control_input_indices`, not a
/// copy of it: a control edge this function does not see is a path
/// `ea_cold_control_nodes` will not consider when deciding a merge is cold,
/// and the hand-kept copy that used to live here had already fallen behind
/// once (`Switch`, the monitor ops, `InstanceOf`/`CheckCast`/`Unbox` and the
/// production `ArrayLength` form). `Start` and every floating pure node answer
/// empty: a node with no control input has no fixed position, so it is never
/// cold.
fn ea_control_preds(node: &ir::Node) -> &[ir::NodeId] {
    // The range is always within `inputs` (the verifier builds it from
    // `inputs.len()`); `get` keeps a disagreement a no-edge answer, not a panic.
    node.inputs
        .get(crate::ir_verify::control_input_indices(node))
        .unwrap_or(&[])
}

/// Every IR node that executes only on a profiled-cold path.
///
/// This is the producer `escape_analysis::Graph::cold_nodes` never had. Without
/// it `EscapeAnalysisResult::partial_escapes` is always empty and the
/// `EscapeState::PartialEscape` lattice level is dead code — see
/// `docs/jit/escape-analysis.md` §2 and §6.3.
///
/// # What "cold" means here, exactly
///
/// `hints` is the `ir_branch_hints` map: keyed by a conditional branch's
/// bytecode PC (`Node::bytecode_pc` on the `Op::If`), value `true` = usually
/// TAKEN, `false` = usually NOT taken. The builder gives `Op::If` a `Proj(0)`
/// true edge that goes to the branch target and a `Proj(1)` false edge that
/// falls through, so the *cold* projection is `Proj(1)` for a usually-taken
/// branch and `Proj(0)` for a usually-not-taken one.
///
/// `profile::BranchCounts` decides "usually" at 90/10 with ≥ 20 samples, so this
/// is a **bias, not a proof of never**. That is why it may only ever feed the
/// reported-only `PartialEscape` classification: an object reported partial has
/// still escaped for real, and every *acting* consumer (`find_scalar_replacements`,
/// `find_lock_elisions`) tests the connection graph's unrefined state. A future
/// partial-escape applier that actually sinks an allocation should tighten this
/// source to a never-observed edge before relying on it.
///
/// # Why over-marking is safe and under-marking is the default
///
/// `Graph::mark_cold` is additive and monotone: more cold nodes can only move
/// an allocation from `ArgEscape`/`GlobalEscape` to the reported
/// `PartialEscape`, which no consumer acts on. The propagation below is
/// nevertheless deliberately conservative in the *under*-marking direction:
///
/// * a `Merge`/`Region` is cold only when EVERY predecessor is cold;
/// * consequently a loop whose header is entered coldly never converges to
///   cold, because its back edge is only cold once the header is — a precision
///   loss, not a soundness one;
/// * a node with no control input (a floating `Const`/`Add`/`Phi`) is never
///   cold, because it has no fixed position to be cold at.
pub(crate) fn ea_cold_control_nodes(
    ir_graph: &ir::Graph,
    hints: &HashMap<usize, bool>,
) -> std::collections::HashSet<ir::NodeId> {
    let mut cold: std::collections::HashSet<ir::NodeId> = std::collections::HashSet::new();
    if hints.is_empty() {
        return cold;
    }

    // Seed: the never-taken projection of each profiled `Op::If`. One pass over
    // the projections (each names its `If` at input 0) rather than a node scan
    // per `If`, which was O(#If × N).
    let mut worklist: Vec<ir::NodeId> = Vec::new();
    for (pidx, p) in ir_graph.nodes.iter().enumerate() {
        let k = match p.op {
            ir::Op::Proj(k @ (0 | 1)) => k,
            _ => continue,
        };
        let Some(branch) = p.inputs.first().and_then(|&b| ir_graph.node_opt(b)) else {
            continue;
        };
        if branch.op != ir::Op::If {
            continue;
        }
        let usually_taken = match branch.bytecode_pc.and_then(|pc| hints.get(&pc)) {
            Some(&t) => t,
            None => continue, // inconclusive profile — nothing is cold here
        };
        let cold_proj = if usually_taken { 1u16 } else { 0u16 };
        if k == cold_proj && cold.insert(pidx as ir::NodeId) {
            worklist.push(pidx as ir::NodeId);
        }
    }
    if cold.is_empty() {
        return cold;
    }

    // Forward closure: a live node is cold once every control predecessor is.
    // A worklist over control successors reaches the same least fixed point as
    // re-sweeping every node until nothing changes, in O(N + E) instead of up
    // to N sweeps of N nodes.
    let mut succs: Vec<Vec<ir::NodeId>> = vec![Vec::new(); ir_graph.nodes.len()];
    for (idx, n) in ir_graph.nodes.iter().enumerate() {
        if n.op == ir::Op::Dead {
            continue;
        }
        for &p in ea_control_preds(n) {
            if let Some(list) = succs.get_mut(p as usize) {
                list.push(idx as ir::NodeId);
            }
        }
    }
    while let Some(c) = worklist.pop() {
        let Some(users) = succs.get(c as usize) else {
            continue;
        };
        for &u in users {
            if cold.contains(&u) {
                continue;
            }
            let Some(n) = ir_graph.node_opt(u) else {
                continue;
            };
            if ea_control_preds(n).iter().all(|p| cold.contains(p)) {
                cold.insert(u);
                worklist.push(u);
            }
        }
    }
    cold
}

/// Feed the profiled cold paths into an EA graph built by
/// [`escape_analysis_from_ir`], translating IR ids through the same `id_map`
/// the results are read back through.
///
/// Call it between the bridge and `escape_analysis::analyze_escapes`; with an
/// empty `hints` map (profiling off — the default) it marks nothing and the
/// analysis is byte-identical to what it was before.
pub(crate) fn ea_mark_cold_from_branch_hints(
    ea: &mut escape_analysis::Graph,
    ir_graph: &ir::Graph,
    id_map: &[escape_analysis::NodeId],
    hints: &HashMap<usize, bool>,
) {
    for ir_id in ea_cold_control_nodes(ir_graph, hints) {
        // A node the bridge did not translate (`usize::MAX`) names nothing in
        // the EA graph; skipping it only under-marks, which is the safe way to
        // be wrong here.
        if let Some(&ea_id) = id_map.get(ir_id as usize) {
            if ea_id != usize::MAX {
                ea.mark_cold(ea_id);
            }
        }
    }
}

/// The input slot carrying `node`'s incoming memory token, or `None` when this
/// op does not consume one.
///
/// Mirrors `ir_optimize::memory_token_slot`, arity guard included: the compact
/// hand-built `[base, value]` `Store` layout's slot 1 is a *value*, not a token,
/// so only the documented full `[ctrl, mem, …]` form has one.
fn ea_memory_token_slot(node: &ir::Node) -> Option<usize> {
    // Delegates to the single table (`ir::Op::memory_shape`). This was the
    // third hand-maintained copy of the arity list; the moment monitors joined
    // the table the copies started answering `None` for a real token slot — and
    // this is the copy `apply_ea_to_ir`'s `value_used` check is built on, so a
    // stale answer here is what would let the EA applier treat a monitor's
    // ordering edge as a value it may rewrite.
    ir::memory_token_slot(node)
}

/// True when input `idx` of `node` is a memory *token* — an ordering edge
/// naming the previous writer — rather than a value the node reads. A
/// memory-typed φ is the one op whose token edges are every value input rather
/// than a single slot. Delegates to `ir::is_memory_token_slot`, which answers
/// exactly that.
pub(crate) fn ea_is_memory_token_slot(node: &ir::Node, idx: usize) -> bool {
    ir::is_memory_token_slot(node, idx)
}

/// Whether `victim` can be spliced out of the memory-token chain: either
/// nothing names it as a token, or it has an incoming token of its own to hand
/// on to whoever does.
///
/// Same bias as `ir_optimize::kill_store_splicing_memory_chain`: a node we
/// decline to delete is a missed optimization, a node deleted out of a chain we
/// could not repair is wrong code.
fn ea_splice_feasible(ir_graph: &ir::Graph, victim: ir::NodeId) -> bool {
    let named_as_token = ir_graph.nodes.iter().any(|n| {
        n.op != ir::Op::Dead
            && n.inputs
                .iter()
                .enumerate()
                .any(|(i, &inp)| inp == victim && ea_is_memory_token_slot(n, i))
    });
    if !named_as_token {
        return true;
    }
    ir_graph
        .node_opt(victim)
        .and_then(|n| ea_memory_token_slot(n).and_then(|s| n.input_opt(s)))
        .is_some()
}

/// True when some safepoint snapshot slot names `id` — a local, an operand-stack
/// entry, or a held monitor.
///
/// A CHAIN snapshot (round 13 wave 3, `ir::SpliceScopeChain`) that no node
/// names does not count ([`ea_snapshot_pins`]).
fn ea_snapshot_names(ir_graph: &ir::Graph, id: ir::NodeId) -> bool {
    let claimed = ea_claimed_chain_snapshots(ir_graph);
    ir_graph.safepoints.iter().enumerate().any(|(si, sp)| {
        ea_snapshot_pins(sp, si, &claimed)
            && sp
                .locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
                .any(|&v| v == id)
    })
}

/// May snapshot `si` keep an allocation it names from being scalar-replaced?
///
/// Every frame of the compiling method may. A chain snapshot recorded inside a
/// spliced body (round 13 wave 3, lane framestate) may not, unless a node names
/// it (`claimed`: an unrolled copy's per-iteration frame, which `ir_lower`
/// REQUIRES and could not replace). The unnamed ones are an alternative the
/// lowerer PREFERS to the flat frame at the outermost `invoke` -- and falls
/// back from whenever a slot holds a scalar-replaced or removed object
/// (`deopt::inlined_chain_refusal`). The flat frame never names an object the
/// spliced body allocated (it did not exist at the `invoke`), so letting the
/// chain pin one would take scalar replacement away from exactly the splices
/// that exist for it and buy nothing. [`ir_prune_unconsumable_snapshots`]
/// drops such a chain snapshot once escape analysis has removed an object it
/// names, so the install verifier never meets its dead slot.
fn ea_snapshot_pins(
    sp: &ir::SafepointSnapshot,
    si: usize,
    claimed: &std::collections::HashSet<u32>,
) -> bool {
    !sp.is_splice_state() || claimed.contains(&(si as u32))
}

/// Round 13 wave 6 (lane chain2): must an allocation stay an allocation
/// because a chain snapshot the lowerer REQUIRES ([`ea_claimed_chain_snapshots`])
/// names it, and that chain could not describe it once removed?
///
/// [`ea_snapshot_pins`] makes such a snapshot pin like a flat frame, and a flat
/// frame's pin is lifted by a virtual-object recipe ("rescued"). A chain's is
/// not, unless `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS` lets the chain carry that
/// recipe (`deopt::inlined_chain_refusal`): with the switch off the rescued
/// allocation left a required chain naming a scalar-replaced object, and the
/// lowerer refused the whole compile (`a spliced guard's required caller chain
/// cannot be published`), or the prune dropped the stale chain and the
/// lowerer refused it for having none. Keeping the allocation keeps the
/// compile. `false` at once on a graph with no chain snapshot, and with the
/// switch on (the recipe rescues it exactly as for a flat frame).
fn ea_required_chain_names(ir_graph: &ir::Graph, id: ir::NodeId) -> bool {
    if !ir_graph.safepoints.iter().any(|sp| sp.is_splice_state()) {
        return false;
    }
    let claimed = ea_claimed_chain_snapshots(ir_graph);
    if claimed.is_empty() {
        return false;
    }
    let named: Vec<usize> = claimed
        .iter()
        .map(|&si| si as usize)
        .filter(|&si| {
            ir_graph.safepoints.get(si).is_some_and(|sp| {
                sp.locals
                    .iter()
                    .chain(sp.stack.iter())
                    .chain(sp.monitors.iter())
                    .any(|&v| v == id)
            })
        })
        .collect();
    if named.is_empty() {
        return false;
    }
    // Only a chain some deopt consults: its own node's, or one at a pc a live
    // node can deopt at. The others are pruned after escape analysis
    // (`ir_prune_unconsumable_snapshots`) and pin nothing.
    let by_node = ir_claimed_snapshots(ir_graph, &std::collections::HashSet::new());
    let consulted = ea_chain_pcs_consulted(ir_graph);
    let pins = named.iter().any(|&si| {
        by_node.contains(&(si as u32))
            || ir_graph
                .safepoints
                .get(si)
                .is_some_and(|sp| consulted.contains(&sp.bci))
    });
    pins && !crate::deopt::chain_virtuals_enabled()
}

/// The raw (combined-buffer) pcs a chain snapshot can be consulted at: every
/// live node that can deopt, at its own pc, and every `Op::Guard` at its `bci`
/// -- the chain half of [`ir_consumable_snapshot_bcis`], which keeps a chain
/// snapshot exactly when its pc is one of these (or a node names it).
fn ea_chain_pcs_consulted(ir_graph: &ir::Graph) -> std::collections::HashSet<usize> {
    let mut out: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for n in &ir_graph.nodes {
        if n.op == ir::Op::Dead
            || matches!(n.op, ir::Op::Merge | ir::Op::Region)
            || ir_lower::op_cannot_deopt(&n.op)
        {
            continue;
        }
        if let Some(pc) = n.bytecode_pc {
            out.insert(pc);
        }
        if let ir::Op::Guard { bci } = n.op {
            out.insert(bci);
        }
    }
    out
}

/// Round 13 wave 6 (lane chain2): does ANY chain snapshot, claimed or not,
/// name `id`?
///
/// For a rewrite that leaves no recipe behind (the box fold): an unclaimed
/// chain does not pin an allocation because the prune drops it once the
/// object is dead (a guard there takes the flat frame), but a rewrite that
/// CLEARS the slot to `NO_NODE` leaves a live-looking chain that says
/// "undefined" for a reference the resumed callee frame reads -- a null (or an
/// int 0) where an object was.
fn ea_any_chain_snapshot_names(ir_graph: &ir::Graph, id: ir::NodeId) -> bool {
    ir_graph.safepoints.iter().any(|sp| {
        sp.is_splice_state()
            && sp
                .locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
                .any(|&v| v == id)
    })
}

/// The chain snapshots some live node names (`Node::frame_snapshot`). Empty,
/// without a scan, on every graph that has none.
///
/// EVERY chain snapshot when the graph relies on its chains
/// (`Graph::splice_chains_required`, W5-3 stage 3): the lowerer then has no
/// flat frame to fall back to, so an object a chain names must stay an object.
fn ea_claimed_chain_snapshots(ir_graph: &ir::Graph) -> std::collections::HashSet<u32> {
    if !ir_graph.safepoints.iter().any(|sp| sp.is_splice_state()) {
        return std::collections::HashSet::new();
    }
    if ir_graph.splice_chains_required {
        return ir_graph
            .safepoints
            .iter()
            .enumerate()
            .filter(|(_, sp)| sp.is_splice_state())
            .map(|(si, _)| si as u32)
            .collect();
    }
    ir_graph
        .nodes
        .iter()
        .filter(|n| n.op != ir::Op::Dead)
        .filter_map(|n| n.frame_snapshot)
        .filter(|&si| {
            ir_graph
                .safepoints
                .get(si as usize)
                .is_some_and(|sp| sp.is_splice_state())
        })
        .collect()
}

// ── Which snapshots a deopt can consult (#49) ────────────────────────
//
// `IrBuilder::build` records a snapshot at EVERY bytecode boundary, because the
// builder's own trap planting and the String/unbox intrinsics ask "is there a
// snapshot at this pc" mid-walk. Most of those snapshots describe program
// points nothing can resume at, and every one of them used to pin whatever it
// named: a fresh `new`'s reference sits on the stack or in a local at several of
// them, so escape analysis could forward its field loads but never remove the
// allocation itself (`allocation-elision-never-fires-by-default-FIXED-20260912.md`).
//
// Three narrowings, each sound on its own:
//
// 1. `ir_prune_dead_snapshot_locals_scoped` — a local the bytecode cannot read again
//    before writing it is not part of the frame an interpreter resumes into.
// 2. `ea_consumable_snapshot_names` — the allocation planner asks only about
//    snapshots at a program point a deopt can arrive at.
// 3. `ir_prune_unconsumable_snapshots` — after escape analysis, the rest are
//    dropped, so no deopt point is ever built from one.

/// The bytecode indices at which a deopt of `ir_graph` can consult a snapshot,
/// with the nodes in `ignore` treated as already gone.
///
/// A snapshot is consultable at:
///
/// * every bci a node that can transfer to the interpreter
///   (`!ir_lower::op_cannot_deopt`) resumes at — its `bytecode_pc` mapped
///   through `spliced_ranges` exactly as `ir_lower`'s `resume_bci` maps it, plus
///   an `Op::Guard`'s own `bci` field. Every deopt stub and every exceptional
///   exit the lowerer emits is emitted while lowering such a node, at its bci;
/// * every `Op::Merge` / `Op::Region` bci — a join is where an optimizing OSR
///   entry is planned and where `ir_lower` treats a loop header as trapping.
///
/// `None` means "cannot be attributed; treat EVERY snapshot as consultable":
/// some node that can deopt carries no `bytecode_pc`, or — when
/// `spliced_ranges` is not known (`None`) — resumes at a pc no snapshot
/// describes, which is what a relocated callee pc looks like before mapping.
fn ir_consumable_snapshot_bcis(
    ir_graph: &ir::Graph,
    spliced_ranges: Option<&[(usize, usize, usize)]>,
    ignore: &std::collections::HashSet<ir::NodeId>,
) -> Option<std::collections::HashSet<usize>> {
    // Frames of the compiling method only. A chain snapshot recorded inside a
    // spliced body (round 13 wave 3, lane framestate: `ir::SpliceScopeChain`)
    // is consulted by a guard at ITS OWN pc, in addition to the flat frame at
    // the outermost invoke that the guard falls back to -- so a spliced
    // trapping pc keeps both, and without `spliced_ranges` such a pc stays
    // unattributable exactly as it was before those snapshots existed.
    let snapshot_bcis: std::collections::HashSet<usize> = ir_graph
        .safepoints
        .iter()
        .filter(|sp| !sp.is_splice_state())
        .map(|sp| sp.bci)
        .collect();
    let chain_bcis: std::collections::HashSet<usize> = ir_graph
        .safepoints
        .iter()
        .filter(|sp| sp.is_splice_state())
        .map(|sp| sp.bci)
        .collect();
    let resume = |pc: usize| -> usize {
        match spliced_ranges {
            Some(ranges) => ranges
                .iter()
                .find(|&&(start, end, _)| pc >= start && pc < end)
                .map_or(pc, |&(_, _, invoke)| invoke),
            None => pc,
        }
    };
    let mut out: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (idx, n) in ir_graph.nodes.iter().enumerate() {
        if n.op == ir::Op::Dead {
            continue;
        }
        if matches!(n.op, ir::Op::Merge | ir::Op::Region) {
            if let Some(pc) = n.bytecode_pc {
                out.insert(resume(pc));
            }
            continue;
        }
        if ir_lower::op_cannot_deopt(&n.op) {
            continue;
        }
        // Attribution is checked BEFORE `ignore`: an unattributable victim is
        // as good a sign as any that this graph was not built from bytecode.
        let pc = n.bytecode_pc?;
        let at = resume(pc);
        if spliced_ranges.is_none() && !snapshot_bcis.contains(&at) {
            return None;
        }
        if ignore.contains(&(idx as ir::NodeId)) {
            continue;
        }
        out.insert(at);
        if chain_bcis.contains(&pc) {
            out.insert(pc);
        }
        if let ir::Op::Guard { bci } = n.op {
            out.insert(resume(bci));
            if chain_bcis.contains(&bci) {
                out.insert(bci);
            }
        }
    }
    Some(out)
}

/// Snapshot indices a live node outside `ignore` names through
/// `Node::frame_snapshot` — consulted by that node's own deopt whatever its bci.
fn ir_claimed_snapshots(
    ir_graph: &ir::Graph,
    ignore: &std::collections::HashSet<ir::NodeId>,
) -> std::collections::HashSet<u32> {
    ir_graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(idx, n)| n.op != ir::Op::Dead && !ignore.contains(&(*idx as ir::NodeId)))
        .filter_map(|(_, n)| n.frame_snapshot)
        .collect()
}

/// [`ea_snapshot_names`], asked only of the snapshots a deopt can consult once
/// the nodes in `ignore` (the candidate's own allocation, stores and loads, all
/// of which the plan retires) are gone.
///
/// Falls back to [`ea_snapshot_names`] when the graph cannot be attributed. The
/// answer is never narrower than what [`ir_prune_unconsumable_snapshots`] keeps
/// after the plans are applied: that pass sees the same graph minus every
/// plan's victims, with the spliced ranges known.
fn ea_consumable_snapshot_names(
    ir_graph: &ir::Graph,
    id: ir::NodeId,
    ignore: &std::collections::HashSet<ir::NodeId>,
) -> bool {
    let Some(consumable) = ir_consumable_snapshot_bcis(ir_graph, None, ignore) else {
        return ea_snapshot_names(ir_graph, id);
    };
    let claimed = ir_claimed_snapshots(ir_graph, ignore);
    let claimed_chains = ea_claimed_chain_snapshots(ir_graph);
    ir_graph.safepoints.iter().enumerate().any(|(si, sp)| {
        (consumable.contains(&sp.bci) || claimed.contains(&(si as u32)))
            && ea_snapshot_pins(sp, si, &claimed_chains)
            && sp
                .locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
                .any(|&v| v == id)
    })
}

/// Drop every snapshot no deopt of the finished graph can consult. Returns how
/// many were dropped.
///
/// Runs after escape analysis, which is the last pass that mutates the graph
/// before scheduling. A dropped snapshot would otherwise still become a
/// `DeoptimizationPoint` (at whatever native offset its bci's nodes produced)
/// naming the allocations the planner just removed. A graph that cannot be
/// attributed keeps every snapshot.
#[cfg(test)]
pub(crate) fn ir_prune_unconsumable_snapshots(
    ir_graph: &mut ir::Graph,
    spliced_ranges: &[(usize, usize, usize)],
) -> usize {
    ir_prune_unconsumable_snapshots_described(ir_graph, spliced_ranges, None)
}

/// [`ir_prune_unconsumable_snapshots`] told which removed allocations the
/// lowerer will be able to describe: `described` is the scalar-replacement map
/// it will be handed (round 13 wave 6, lane chain2;
/// `r13w5-resume-ea-chain-virtuals-producer-patch` item 2).
///
/// With `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS` on, an unclaimed chain snapshot
/// whose every removed slot is an object of that map is KEPT: the lowerer
/// describes it as `VirtualObject` / `VirtualObjectRef` (chain-wide ids) and
/// the VM re-allocates it (`deopt_resume::materialise_chain_virtuals`), so the
/// guard publishes the callee's own frame instead of the flat one -- or, under
/// the chain fences, instead of refusing the compile for want of any. A chain
/// naming any other removed node is dropped exactly as before, and with the
/// switch off (or `None`) this is [`ir_prune_unconsumable_snapshots`].
pub(crate) fn ir_prune_unconsumable_snapshots_described(
    ir_graph: &mut ir::Graph,
    spliced_ranges: &[(usize, usize, usize)],
    described: Option<&ir_lower::ScalarReplacementMap>,
) -> usize {
    let nothing_ignored: std::collections::HashSet<ir::NodeId> = std::collections::HashSet::new();
    let claimed = ir_claimed_snapshots(ir_graph, &nothing_ignored);
    // Round 13 wave 3 (lane framestate): an unnamed chain snapshot that names
    // a node escape analysis removed is dropped whatever its bci -- it never
    // pinned the object ([`ea_snapshot_pins`]), the lowerer would refuse to
    // publish it anyway (`deopt::inlined_chain_refusal`), and kept it would
    // hand the install verifier a slot naming a dead node. A guard there takes
    // the flat frame at the outermost `invoke`, which cannot name an object
    // the spliced body allocated.
    let is_removed = |v: ir::NodeId| {
        v != ir::NO_NODE
            && ir_graph
                .nodes
                .get(v as usize)
                .is_none_or(|n| n.op == ir::Op::Dead)
    };
    let names_removed = |sp: &ir::SafepointSnapshot| {
        sp.locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .any(|&v| is_removed(v))
    };
    // Round 13 wave 6 (lane chain2): ...unless every removed slot is an object
    // the lowerer will describe and chains may carry it (see the doc). The
    // switch is read once, and only when some chain would otherwise go.
    let mut virtuals_on: Option<bool> = None;
    let mut describes_every_removed = |sp: &ir::SafepointSnapshot| -> bool {
        let Some(map) = described else {
            return false;
        };
        if !*virtuals_on.get_or_insert_with(crate::deopt::chain_virtuals_enabled) {
            return false;
        }
        sp.locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .all(|&v| !is_removed(v) || map.objects.contains_key(&v))
    };
    let stale_chain: Vec<bool> = ir_graph
        .safepoints
        .iter()
        .enumerate()
        .map(|(si, sp)| {
            sp.is_splice_state()
                && !claimed.contains(&(si as u32))
                && names_removed(sp)
                && !describes_every_removed(sp)
        })
        .collect();
    let consumable = ir_consumable_snapshot_bcis(ir_graph, Some(spliced_ranges), &nothing_ignored);
    if consumable.is_none() && !stale_chain.iter().any(|&s| s) {
        return 0;
    }
    let keep: Vec<bool> = ir_graph
        .safepoints
        .iter()
        .enumerate()
        .map(|(si, sp)| {
            let consulted = consumable
                .as_ref()
                .is_none_or(|c| c.contains(&sp.bci) || claimed.contains(&(si as u32)));
            consulted && !stale_chain[si]
        })
        .collect();
    ir_graph.retain_safepoints(&keep)
}

/// One instruction as local-variable liveness sees it.
struct IrLivenessInsn {
    pc: usize,
    succs: Vec<usize>,
    uses: Vec<usize>,
    defs: Vec<usize>,
}

/// Decode `code[..code_len]` for [`body_local_liveness`]. `None` for
/// anything liveness cannot model soundly: subroutines (`jsr`/`ret`), `wide
/// ret`, a malformed switch, or a branch that leaves the method.
fn decode_for_local_liveness(code: &[u8], code_len: usize) -> Option<Vec<IrLivenessInsn>> {
    let byte = |p: usize| -> Option<u8> {
        if p < code_len {
            code.get(p).copied()
        } else {
            None
        }
    };
    let u16_at = |p: usize| -> Option<usize> {
        Some(usize::from(u16::from_be_bytes([byte(p)?, byte(p + 1)?])))
    };
    // The explicit target of an offset branch, inside the method.
    let branch_target = |pc: usize| -> Option<usize> {
        crate::bytecode_analysis::offset_branch_target(&code[..code_len.min(code.len())], pc)
            .filter(|&t| t < code_len)
    };
    let mut out: Vec<IrLivenessInsn> = Vec::new();
    let mut pc = 0usize;
    while pc < code_len {
        let op = byte(pc)?;
        let len = crate::bytecode_analysis::step(code, pc);
        if len == 0 || pc.checked_add(len)? > code_len {
            return None;
        }
        let next = pc + len;
        let mut uses: Vec<usize> = Vec::new();
        let mut defs: Vec<usize> = Vec::new();
        let mut succs: Vec<usize> = Vec::new();
        let mut falls_through = true;
        match op {
            // Loads. `lload`/`dload` read both halves of a category-2 value.
            0x15 | 0x17 | 0x19 => uses.push(usize::from(byte(pc + 1)?)),
            0x16 | 0x18 => {
                let i = usize::from(byte(pc + 1)?);
                uses.extend([i, i + 1]);
            }
            0x1a..=0x1d => uses.push(usize::from(op - 0x1a)),
            0x1e..=0x21 => {
                let i = usize::from(op - 0x1e);
                uses.extend([i, i + 1]);
            }
            0x22..=0x25 => uses.push(usize::from(op - 0x22)),
            0x26..=0x29 => {
                let i = usize::from(op - 0x26);
                uses.extend([i, i + 1]);
            }
            0x2a..=0x2d => uses.push(usize::from(op - 0x2a)),
            // Stores.
            0x36 | 0x38 | 0x3a => defs.push(usize::from(byte(pc + 1)?)),
            0x37 | 0x39 => {
                let i = usize::from(byte(pc + 1)?);
                defs.extend([i, i + 1]);
            }
            0x3b..=0x3e => defs.push(usize::from(op - 0x3b)),
            0x3f..=0x42 => {
                let i = usize::from(op - 0x3f);
                defs.extend([i, i + 1]);
            }
            0x43..=0x46 => defs.push(usize::from(op - 0x43)),
            0x47..=0x4a => {
                let i = usize::from(op - 0x47);
                defs.extend([i, i + 1]);
            }
            0x4b..=0x4e => defs.push(usize::from(op - 0x4b)),
            // `iinc` reads its slot. Its write is not modelled as a kill, which
            // can only keep the slot live longer.
            0x84 => uses.push(usize::from(byte(pc + 1)?)),
            0xc4 => {
                let widened = byte(pc + 1)?;
                let i = u16_at(pc + 2)?;
                match widened {
                    0x15 | 0x17 | 0x19 | 0x84 => uses.push(i),
                    0x16 | 0x18 => uses.extend([i, i + 1]),
                    0x36 | 0x38 | 0x3a => defs.push(i),
                    0x37 | 0x39 => defs.extend([i, i + 1]),
                    _ => return None,
                }
            }
            0x99..=0xa6 | 0xc6 | 0xc7 => succs.push(branch_target(pc)?),
            0xa7 | 0xc8 => {
                succs.push(branch_target(pc)?);
                falls_through = false;
            }
            // Subroutines make local liveness path-dependent.
            0xa8 | 0xa9 | 0xc9 => return None,
            0xaa | 0xab => {
                // Strict: a malformed table, or one with a target outside the
                // method, makes the whole method unmodelled.
                let table = crate::bytecode_analysis::switch_table(code, code_len, pc)?;
                succs.extend(table.targets());
                falls_through = false;
            }
            0xac..=0xb1 | 0xbf => falls_through = false,
            _ => {}
        }
        if falls_through && next < code_len {
            succs.push(next);
        }
        out.push(IrLivenessInsn {
            pc,
            succs,
            uses,
            defs,
        });
        pc = next;
    }
    Some(out)
}

/// Local-variable liveness of ONE method body -- the compiling method's own
/// code, or one spliced callee's relocated body -- as the snapshot prune reads
/// it. Round 13 wave 12 (lane snapliv): factored out of the prune so that each
/// scope of a chain snapshot can ask its own body, and given exception edges.
struct BodyLocalLiveness {
    /// pc -> instruction index; `usize::MAX` for a pc that starts none.
    index_of: Vec<usize>,
    /// `words` bitsets per instruction: the locals live on entry to it.
    live_in: Vec<u64>,
    words: usize,
    num_locals: usize,
    /// Every target of a backward branch.
    loop_headers: std::collections::HashSet<usize>,
}

impl BodyLocalLiveness {
    /// Is local `k` provably dead on entry to `bci` -- no path from there, the
    /// exception edges included, reads it before writing it? `false` whenever
    /// there is no answer: a pc that starts no instruction, a slot past the
    /// analysed width.
    fn dead_at(&self, bci: usize, k: usize) -> bool {
        if k >= self.num_locals {
            return false;
        }
        let Some(&i) = self.index_of.get(bci) else {
            return false;
        };
        if i == usize::MAX {
            return false;
        }
        i.checked_mul(self.words)
            .and_then(|at| at.checked_add(k / 64))
            .and_then(|at| self.live_in.get(at))
            .is_some_and(|w| w & (1u64 << (k % 64)) == 0)
    }

    fn is_loop_header(&self, bci: usize) -> bool {
        self.loop_headers.contains(&bci)
    }
}

/// JVMS local-variable liveness over `code[..code_len]`, `num_locals` slots
/// wide.
///
/// `handlers` is the body's exception table as `(start_pc, end_pc,
/// handler_pc)`. Every instruction in `[start_pc, end_pc)` gains an EXCEPTION
/// edge to `handler_pc`, and the handler's live-in joins the instruction's own
/// live-in AFTER the instruction's stores are killed: the throw can happen
/// before the instruction takes effect, so a store never hides a local the
/// handler reads (the rule `regalloc::live_locals_per_pc_all` applies through
/// `handler_live_mask`). A deopt that rethrows at a bci inside the range, or
/// resumes a frame whose callee then throws into it, therefore finds every
/// local the handler reads.
///
/// `relocated`: the body was appended to the combined buffer at an offset
/// that need not be 4-aligned, so a `tableswitch` / `lookupswitch` (padding
/// measured from the method start) proves nothing. The inline resolver
/// refuses both in a spliced body anyway.
///
/// `None` -- prune nothing -- for anything it cannot model soundly: what
/// [`decode_for_local_liveness`] refuses, a branch into the middle of an
/// instruction, a local index at or past `num_locals`, a malformed handler row,
/// or a fixed point that does not converge in `MAX_ROUNDS`.
fn body_local_liveness(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    handlers: &[(usize, usize, usize)],
    relocated: bool,
) -> Option<BodyLocalLiveness> {
    const MAX_LOCALS: usize = 1024;
    const MAX_ROUNDS: usize = 64;
    if code_len == 0 || code_len > code.len() || num_locals == 0 || num_locals > MAX_LOCALS {
        return None;
    }
    let insns = decode_for_local_liveness(code, code_len)?;
    let mut index_of: Vec<usize> = vec![usize::MAX; code_len];
    for (i, insn) in insns.iter().enumerate() {
        *index_of.get_mut(insn.pc)? = i;
    }
    let is_insn = |pc: usize| index_of.get(pc).is_some_and(|&i| i != usize::MAX);
    let mut loop_headers: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for insn in &insns {
        if relocated && matches!(code.get(insn.pc).copied(), Some(0xaa | 0xab)) {
            return None;
        }
        for &s in &insn.succs {
            // A branch into the middle of an instruction: the decode and the
            // method disagree, so prove nothing.
            if !is_insn(s) {
                return None;
            }
            if s <= insn.pc {
                loop_headers.insert(s);
            }
        }
        if insn
            .uses
            .iter()
            .chain(insn.defs.iter())
            .any(|&l| l >= num_locals)
        {
            return None;
        }
    }
    // Exception edges, as instruction indices. Empty (and never read) for a
    // body without handlers.
    let mut exc: Vec<Vec<usize>> = if handlers.is_empty() {
        Vec::new()
    } else {
        vec![Vec::new(); insns.len()]
    };
    for &(start, end, handler) in handlers {
        // JVMS 4.7.3: `start_pc` and `handler_pc` start instructions, `end_pc`
        // starts one or is the code length. Anything else proves nothing.
        if start >= end
            || end > code_len
            || !is_insn(start)
            || !is_insn(handler)
            || (end < code_len && !is_insn(end))
        {
            return None;
        }
        let h = *index_of.get(handler)?;
        for (i, insn) in insns.iter().enumerate() {
            if insn.pc >= start && insn.pc < end {
                let row = exc.get_mut(i)?;
                if !row.contains(&h) {
                    row.push(h);
                }
            }
        }
    }
    let words = num_locals.div_ceil(64);
    let n = insns.len();
    let mut live_in: Vec<u64> = vec![0; n.checked_mul(words)?];
    let mut scratch: Vec<u64> = vec![0; words];
    let mut converged = false;
    for _ in 0..MAX_ROUNDS {
        let mut changed = false;
        for i in (0..n).rev() {
            scratch.iter_mut().for_each(|w| *w = 0);
            for &s in &insns[i].succs {
                let si = index_of[s];
                for w in 0..words {
                    scratch[w] |= live_in[si * words + w];
                }
            }
            for &d in &insns[i].defs {
                scratch[d / 64] &= !(1u64 << (d % 64));
            }
            for &u in &insns[i].uses {
                scratch[u / 64] |= 1u64 << (u % 64);
            }
            // After the kill: see the doc.
            if let Some(targets) = exc.get(i) {
                for &h in targets {
                    for w in 0..words {
                        scratch[w] |= live_in[h * words + w];
                    }
                }
            }
            let row = &mut live_in[i * words..(i + 1) * words];
            if row[..] != scratch[..] {
                row.copy_from_slice(&scratch);
                changed = true;
            }
        }
        if !changed {
            converged = true;
            break;
        }
    }
    converged.then_some(BodyLocalLiveness {
        index_of,
        live_in,
        words,
        num_locals,
        loop_headers,
    })
}

/// `CRATONVM_JIT_IR_PRUNE_HANDLER_METHOD_LOCALS`, **default ON** (round 13
/// wave 12, lane snapliv, W2-1): prune the snapshots of a method WITH an
/// exception table too, the handler edges modelled
/// ([`body_local_liveness`]). `=0` restores the old rule: such a method's own
/// frames keep every local.
fn ir_prune_handler_method_locals_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_PRUNE_HANDLER_METHOD_LOCALS")
}

/// `CRATONVM_JIT_IR_PRUNE_CHAIN_SNAPSHOT_LOCALS`, **default ON** (round 13
/// wave 12, lane snapliv, CH5-1): prune the chain snapshots a build records
/// inside spliced bodies (`CRATONVM_JIT_IR_SPLICE_FRAME_STATES`), scope by
/// scope. `=0` keeps every local of every scope, as before.
fn ir_prune_chain_snapshot_locals_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_PRUNE_CHAIN_SNAPSHOT_LOCALS")
}

/// `CRATONVM_JIT_IR_PRUNE_LOOP_HEADER_LOCALS`, **default ON** since round 14
/// wave 2 (`0` turns it off; round 13 wave 12, lane snapliv, W2-1): prune the
/// snapshot at a loop header of the compiling method too, which is what lets
/// a dead loop-carried local's φ die.
///
/// It was off at first because that snapshot has a second reader: a non-opaque optimizing OSR
/// entry seeds the frame from it (`ir_lower::Lowerer::emit_osr_entry_stubs`),
/// and a value it names is seeded whether or not it is live. Unpruned, a dead
/// local there also seeds a PRE-LOOP value that a later pass (GVN, a fold,
/// LICM) may make an in-loop deopt frame name, and the entry check counts only
/// data uses. Pruned, nothing seeds it and the stub jumps past its definition.
/// See `r13w12-snapliv-loop-header-snapshot-doubles-as-the-osr-seed-list-FIXED-20260929.md`.
///
/// Round 13 wave 13 (lane jitfix): that hazard is closed by the entry
/// admission `ir_lower::Lowerer::osr_entry_skipped_frame_value` (kill switch
/// `CRATONVM_JIT_IR_OSR_FRAME_VALUE_REFUSAL`, default on), which refuses an
/// entry past which a deopt frame names a value the stub neither seeds nor
/// re-runs. The prune therefore also requires that switch: turning the
/// refusal off must not leave a pruned header seeding too little.
fn ir_prune_loop_header_locals_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_PRUNE_LOOP_HEADER_LOCALS")
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_OSR_FRAME_VALUE_REFUSAL")
}

/// Where one scope of a chain snapshot keeps its locals, and which body and
/// bci its liveness is asked at.
#[derive(Clone, Copy, Debug)]
struct ChainScopeSpan {
    /// `(base, code_len, max_locals)` of the spliced body this scope runs;
    /// `None` for the compiling method (the last scope).
    body: Option<(usize, usize, usize)>,
    /// The scope's own bci. Scope 0: the instruction the snapshot was taken
    /// at, which a deopt re-executes. Every other scope: its `invoke`, which
    /// the frame resumes AFTER -- the invoke reads and writes no local, so its
    /// live-in is the successor's plus whatever a handler covering it reads
    /// (a throw out of the callee lands there).
    bci: usize,
    /// `None`: the snapshot's `locals`. `Some((start, len))`:
    /// `stack[start .. start + len]` (see `ir::SpliceScopeChain`'s layout).
    in_stack: Option<(usize, usize)>,
}

/// The scopes of chain snapshot `sp`, checked against the spliced-body table
/// `bodies` scope by scope (method key, body length, the bci each scope is
/// parked at, the layout of `stack`), or `None` -- prune nothing in it -- for
/// any disagreement: a chain that could be read against the wrong body is
/// worse than a chain pruned by nothing.
///
/// `self_key`: the compiling method's key when it has an exception table. A
/// spliced copy of itself would then carry handlers its relocated body's
/// liveness does not model (the inline resolver refuses a callee with an
/// exception table, so no such chain exists; this is the belt).
fn chain_scope_spans(
    sp: &ir::SafepointSnapshot,
    bodies: &ir::IrInlineFrameSites,
    code_len: usize,
    self_key: Option<&str>,
) -> Option<Vec<ChainScopeSpan>> {
    let chain = sp.splice_scope.as_deref()?;
    let n = chain.scopes.len();
    let enclosing = bodies.bodies_enclosing(sp.bci);
    if n < 2 || enclosing.len() + 1 != n {
        return None;
    }
    // The layout check the lowerer makes before it publishes the chain.
    chain.split(&sp.locals, &sp.stack)?;
    let mut out: Vec<ChainScopeSpan> = Vec::with_capacity(n);
    let mut at = sp.bci;
    let mut stack_at = 0usize;
    for (i, scope) in chain.scopes.iter().enumerate() {
        let bci = usize::try_from(scope.bci).ok()?;
        let locals_len = usize::try_from(scope.locals_len).ok()?;
        let stack_len = usize::try_from(scope.stack_len).ok()?;
        let in_stack = if i == 0 {
            None
        } else {
            let start = stack_at;
            stack_at = stack_at.checked_add(locals_len)?;
            Some((start, locals_len))
        };
        stack_at = stack_at.checked_add(stack_len)?;
        let body = match enclosing.get(i) {
            Some(&(base, len, caller_pc, key)) => {
                if scope.method_key != key
                    || self_key == Some(key)
                    || usize::try_from(scope.code_len).ok()? != len
                    || at.checked_sub(base)? != bci
                    || bci >= len
                {
                    return None;
                }
                at = caller_pc;
                Some((base, len, usize::try_from(scope.max_locals).ok()?))
            }
            None => {
                if i + 1 != n || at != bci || bci >= code_len {
                    return None;
                }
                None
            }
        };
        out.push(ChainScopeSpan {
            body,
            bci,
            in_stack,
        });
    }
    (stack_at == sp.stack.len()).then_some(out)
}

/// What the prune may cut in one snapshot.
enum SnapshotCut {
    /// A frame of the compiling method: its locals, by the method's liveness.
    Flat,
    /// A chain snapshot: exactly the slots marked, `(locals, stack)`.
    Chain(Vec<bool>, Vec<bool>),
    /// Nothing.
    Keep,
}

/// Clear every safepoint-snapshot LOCAL the bytecode can no longer read, and
/// return how many were cleared.
///
/// A local that is not live-in at a bci (JVMS local-variable liveness: no path
/// from here reads it before writing it) is not part of the frame an
/// interpreter resumes into there. Resuming with `Undefined` in its place — the
/// value every resume sink turns into `Int(0)` — is exactly as correct as
/// resuming with the old value, because nothing reads it. HotSpot's
/// `MethodLiveness` makes the same cut for its debug info.
///
/// The cut is what lets an allocation stored in a local stop being named by
/// every later snapshot: `Foo o = new Foo(); int v = o.x; ... guard` no longer
/// holds `o` in the guard's frame once `o` is never read again.
///
/// `code` is the buffer the build walked (the method's own code, then every
/// spliced body), `code_len` the method's own length, `handlers` its exception
/// table as `(start_pc, end_pc, handler_pc)`, and `bodies` the spliced-body
/// table the build used. `chains_allowed == false` (`lib.rs`: a debugger may
/// read locals) leaves every chain snapshot whole: the discard check for such
/// compiles (`ir_lower::ir_artifact_hides_an_assigned_local`) asks only a
/// chain's OUTERMOST scope, so a callee local cut here would reach JDWP as 0.
/// Round 13 wave 13 (lane jitfix): `ir_lower::ir_artifact_hides_an_assigned_local_scoped`
/// asks every scope; once `lib.rs` calls it (patch page
/// `r13w13-jitfix-lib-debugger-discard-asks-chain-scopes-patch-FIXED-20260929.md`)
/// the caller passes `true` here under a debugger too.
///
/// * **Frames of the compiling method** are cut by its liveness, handler edges
///   included (round 13 wave 12, lane snapliv, W2-1; before, a method with an
///   exception table was not pruned at all). Kill switch
///   `CRATONVM_JIT_IR_PRUNE_HANDLER_METHOD_LOCALS=0`.
/// * **Chain snapshots** (a frame taken inside a spliced body, round 13 wave
///   3) are cut per scope (CH5-1): the innermost scope's locals by its own
///   body's liveness at its own bci, every caller scope's -- which ride in
///   `stack` -- by its body's liveness at its `invoke`. Before, their bci (a
///   combined-buffer pc) kept every local of every scope alive for the length
///   of the body. Kill switch `CRATONVM_JIT_IR_PRUNE_CHAIN_SNAPSHOT_LOCALS=0`.
/// * **Loop headers** of the compiling method are left untouched unless
///   `CRATONVM_JIT_IR_PRUNE_LOOP_HEADER_LOCALS=1` (see its reader for why).
///   Under it, a header slot naming a value that some snapshot this prune
///   could NOT analyse still names is kept, so the header never stops naming
///   a value a later frame in the loop reads through such a snapshot.
///
/// Conservative in every other direction: any construct liveness cannot model
/// ([`body_local_liveness`]) prunes nothing in that body, a chain that
/// disagrees with `bodies` ([`chain_scope_spans`]) prunes nothing in that
/// snapshot, and the operand stack and the monitors are never cut (a caller
/// scope's operand stack included).
pub(crate) fn ir_prune_dead_snapshot_locals_scoped(
    ir_graph: &mut ir::Graph,
    code: &[u8],
    code_len: usize,
    handlers: &[(usize, usize, usize)],
    bodies: &ir::IrInlineFrameSites,
    chains_allowed: bool,
) -> usize {
    use std::collections::HashSet;
    if ir_graph.safepoints.is_empty() || code_len == 0 || code_len > code.len() {
        return 0;
    }
    let headers_on = ir_prune_loop_header_locals_enabled();
    let chains_on = chains_allowed
        && !bodies.is_empty()
        && ir_graph.safepoints.iter().any(|sp| sp.is_splice_state())
        && ir_prune_chain_snapshot_locals_enabled();
    let self_key: Option<&str> = (!handlers.is_empty() && !ir_graph.scope_method_key.is_empty())
        .then_some(ir_graph.scope_method_key.as_str());
    let spans: Vec<Option<Vec<ChainScopeSpan>>> = ir_graph
        .safepoints
        .iter()
        .map(|sp| {
            if chains_on && sp.is_splice_state() {
                chain_scope_spans(sp, bodies, code_len, self_key)
            } else {
                None
            }
        })
        .collect();
    // The compiling method's width: its own frames', and the outermost scope
    // of every chain.
    let num_locals = ir_graph
        .safepoints
        .iter()
        .filter(|sp| !sp.is_splice_state())
        .map(|sp| sp.locals.len())
        .chain(
            spans
                .iter()
                .flatten()
                .filter_map(|scopes| scopes.last())
                .filter_map(|s| s.in_stack.map(|(_, len)| len)),
        )
        .max()
        .unwrap_or(0);
    let method = if handlers.is_empty() || ir_prune_handler_method_locals_enabled() {
        code.get(..code_len)
            .and_then(|body| body_local_liveness(body, code_len, num_locals, handlers, false))
    } else {
        None
    };
    // One analysis per spliced body. A spliced body has no exception table:
    // the inline resolver refuses such a callee (`callee-exception-table`).
    let mut callees: HashMap<(usize, usize), Option<BodyLocalLiveness>> = HashMap::new();
    for s in spans.iter().flatten().flatten() {
        if let Some((base, len, max_locals)) = s.body {
            callees.entry((base, max_locals)).or_insert_with(|| {
                base.checked_add(len)
                    .and_then(|end| code.get(base..end))
                    .and_then(|body| body_local_liveness(body, len, max_locals, &[], true))
            });
        }
    }
    // What each snapshot may lose, and (only when headers are pruned) every
    // value a snapshot this prune cannot analyse keeps naming.
    let mut protected: HashSet<ir::NodeId> = HashSet::new();
    let mut cuts: Vec<SnapshotCut> = Vec::with_capacity(spans.len());
    for (sp, scopes) in ir_graph.safepoints.iter().zip(spans.iter()) {
        if !sp.is_splice_state() {
            if sp.bci < code_len && method.is_some() {
                cuts.push(SnapshotCut::Flat);
            } else {
                if headers_on {
                    protected.extend(sp.locals.iter().copied());
                }
                cuts.push(SnapshotCut::Keep);
            }
            continue;
        }
        let Some(scopes) = scopes else {
            if headers_on {
                protected.extend(sp.locals.iter().chain(sp.stack.iter()).copied());
            }
            cuts.push(SnapshotCut::Keep);
            continue;
        };
        let mut dead_locals = vec![false; sp.locals.len()];
        let mut dead_stack = vec![false; sp.stack.len()];
        for s in scopes {
            let liveness: Option<&BodyLocalLiveness> = match s.body {
                None => method.as_ref(),
                Some((base, _, max_locals)) => {
                    callees.get(&(base, max_locals)).and_then(|l| l.as_ref())
                }
            };
            let (values, dead): (&[ir::NodeId], &mut [bool]) = match s.in_stack {
                None => (&sp.locals[..], &mut dead_locals[..]),
                Some((start, len)) => {
                    let Some(end) = start.checked_add(len) else {
                        continue;
                    };
                    match (sp.stack.get(start..end), dead_stack.get_mut(start..end)) {
                        (Some(values), Some(dead)) => (values, dead),
                        _ => continue,
                    }
                }
            };
            match liveness {
                Some(liveness) => {
                    for (k, d) in dead.iter_mut().enumerate() {
                        *d = liveness.dead_at(s.bci, k);
                    }
                }
                None => {
                    if headers_on {
                        protected.extend(values.iter().copied());
                    }
                }
            }
        }
        cuts.push(SnapshotCut::Chain(dead_locals, dead_stack));
    }
    let (mut flat_cleared, mut header_cleared, mut chain_cleared) = (0usize, 0usize, 0usize);
    // Through the tracked clearer: it REMOVES references only, which the
    // def-use lists tolerate as a stale superset (see `ir::UseLists`), and a
    // write through `ir_graph.safepoints` would bump the edge epoch (round 11
    // wave 18) and cost the next pass a rebuild.
    let cleared = ir_graph.clear_safepoint_slots_where(|slot, bci, v| {
        let k = slot.slot as usize;
        match cuts.get(slot.snapshot as usize) {
            Some(SnapshotCut::Flat) => {
                if slot.kind != ir::SafepointSlotKind::Local {
                    return false;
                }
                let Some(liveness) = method.as_ref() else {
                    return false;
                };
                let header = liveness.is_loop_header(bci);
                if header && (!headers_on || protected.contains(&v)) {
                    return false;
                }
                let dead = liveness.dead_at(bci, k);
                if dead {
                    flat_cleared += 1;
                    header_cleared += usize::from(header);
                }
                dead
            }
            Some(SnapshotCut::Chain(dead_locals, dead_stack)) => {
                let dead = match slot.kind {
                    ir::SafepointSlotKind::Local => dead_locals.get(k).copied().unwrap_or(false),
                    ir::SafepointSlotKind::Stack => dead_stack.get(k).copied().unwrap_or(false),
                    ir::SafepointSlotKind::Monitor => false,
                };
                chain_cleared += usize::from(dead);
                dead
            }
            Some(SnapshotCut::Keep) | None => false,
        }
    });
    if cleared > 0 && ir_stage_reporting() {
        eprintln!(
            "[ir] snapshot liveness: {cleared} slot(s) cleared -- {flat_cleared} in the \
             method's own frames ({header_cleared} at loop headers), {chain_cleared} in \
             chain snapshots"
        );
    }
    cleared
}

/// [`ir_prune_dead_snapshot_locals_scoped`] for a method with no exception
/// table and nothing spliced: the unit tests' entry point.
#[cfg(test)]
pub(crate) fn ir_prune_dead_snapshot_locals(
    ir_graph: &mut ir::Graph,
    code: &[u8],
    code_len: usize,
) -> usize {
    ir_prune_dead_snapshot_locals_scoped(
        ir_graph,
        code,
        code_len,
        &[],
        &ir::IrInlineFrameSites::default(),
        true,
    )
}

/// Fold `o == null` / `o != null` where `o` is a fresh `Op::New` or
/// `Op::NewArray` to the constant it must be. Returns how many compares folded.
///
/// Sound because an allocation never yields null: it either produces an object
/// or throws before producing anything (OOM, or a negative array length). Run
/// before escape analysis for two reasons. The live `Op::Cmp` is a VALUE use of
/// the allocation, which `plan_scalar_replacement` refuses to strand; and the
/// null literal makes the bridge classify the compare `EaOp::Other`, which the
/// analysis treats as a global escape. Either alone pinned the allocation.
///
/// Also runs [`ir_fold_unbox_of_fresh_box`] first (round 9 wave 10,
/// collect10): this is the one pre-optimize hook `lib.rs` calls on every IR
/// compile, and the box fold wants the same window -- after the dead-local
/// snapshot pruning, before the optimizer. Its count is not part of this
/// function's return value, which still counts compares only.
///
/// Test-only since round 14 wave 3: the compile calls
/// [`ir_fold_null_checks_on_fresh_allocations_with`] with its literal hashes.
#[cfg(test)]
pub(crate) fn ir_fold_null_checks_on_fresh_allocations(ir_graph: &mut ir::Graph) -> usize {
    ir_fold_null_checks_on_fresh_allocations_with(ir_graph, &HashMap::new())
}

/// [`ir_fold_null_checks_on_fresh_allocations`], also folding
/// `"literal".hashCode()` for every `ldc` site `literal_hashes` names
/// (`(holder_class_id, cp_idx)` -> the literal's `String.hashCode()`; see
/// [`ir_fold_hash_code_of_string_literal`]). An empty map folds no string hash.
pub(crate) fn ir_fold_null_checks_on_fresh_allocations_with(
    ir_graph: &mut ir::Graph,
    literal_hashes: &HashMap<(u32, u16), i32>,
) -> usize {
    // Round 13 wave 8 (lane iropt7): first, so that a box whose hashCode was
    // its only non-unbox reader can then fold away whole.
    let hash_folds = ir_fold_hash_code_of_integer_box(ir_graph);
    if hash_folds > 0 && ir_stage_reporting() {
        eprintln!("[ir] box-hash fold: {hash_folds} valueOf(v).hashCode() call(s) folded to v");
    }
    // Round 14 wave 3 (lane calls, I7-4).
    let literal_hash_folds = ir_fold_hash_code_of_string_literal(ir_graph, literal_hashes);
    if literal_hash_folds > 0 && ir_stage_reporting() {
        eprintln!(
            "[ir] string-literal hash fold: {literal_hash_folds} \"literal\".hashCode() call(s) folded"
        );
    }
    // Round 14 wave 3 (lane calls, I7-4): an `ldc <String>` result is never
    // null either -- its lowering sends a failed resolution to the exception
    // exit before any reader sees the value -- so a spliced `HashMap.hash`'s
    // `key == null` on a literal key folds too. Same switch as the hash fold.
    let literal_non_null = ir_string_literal_hash_fold_enabled();
    let box_folds = ir_fold_unbox_of_fresh_box(ir_graph);
    if box_folds > 0 && ir_stage_reporting() {
        eprintln!("[ir] box-unbox fold: {box_folds} valueOf site(s) folded into their unbox");
    }
    // Round 13 wave 8 (lane iropt7): a `valueOf` result is never null either
    // (`HashMap.hash`'s `key == null` on a boxed key). Same switch as the
    // hash fold.
    let box_non_null = ir_box_hash_fold_enabled();
    let mut folds: Vec<(ir::NodeId, i64)> = Vec::new();
    {
        let g: &ir::Graph = ir_graph;
        let is_fresh = |id: ir::NodeId| {
            g.node_opt(id).is_some_and(|n| match n.op {
                ir::Op::New { .. } | ir::Op::NewArray { .. } => true,
                ir::Op::ConstString { .. } => literal_non_null && n.ty == ir::IrType::Ref,
                ir::Op::Call { info_ptr } => {
                    box_non_null
                        && n.ty == ir::IrType::Ref
                        && ir_call_box_value_of(n, info_ptr).is_some()
                }
                _ => false,
            })
        };
        let is_null = |id: ir::NodeId| {
            g.node_opt(id)
                .is_some_and(|n| n.op == ir::Op::Const(0) && n.ty == ir::IrType::Ref)
        };
        for (idx, n) in g.nodes.iter().enumerate() {
            let ir::Op::Cmp(cc) = &n.op else {
                continue;
            };
            if n.inputs.len() < 2 {
                continue;
            }
            let (a, b) = (n.inputs[0], n.inputs[1]);
            let fresh_vs_null = (is_fresh(a) && is_null(b)) || (is_fresh(b) && is_null(a));
            if !fresh_vs_null {
                continue;
            }
            let value = match cc {
                ir::CmpOp::Eq => 0,
                ir::CmpOp::Ne => 1,
                _ => continue,
            };
            folds.push((idx as ir::NodeId, value));
        }
    }
    for &(cmp, value) in &folds {
        let c = ir_graph.add(ir::Op::Const(value), ir::IrType::Int, vec![], None);
        ir_graph.replace_all_uses(cmp, c);
        ir_graph.kill(cmp);
    }
    folds.len()
}

// ── Box/unbox fold (round 9 wave 10, collect10) ──────────────────────
//
// `Integer x = i; ... x.intValue()` -- and the same shape after a generic
// helper is inlined -- builds `Op::Call(Integer.valueOf)` feeding an
// `Op::Unbox { IntValue }`. The call is the whole cost: `valueOf` is a
// dispatch to `jit_integer_value_of_direct`, which either allocates a fresh
// box through the helper TLAB path or, for `-128..=127`, runs the canonical
// native under `safe_native_call` for the identity cache. MEASURED on the w9b
// binary (`BoxProbe bu`, `for (i < 10M) { Integer a = i + 1000; s +=
// a.intValue(); }`, IR/OSR body): ~650 ms, against ~2 ms on HotSpot, whose C2
// removes the box. The unbox is then a guarded load of a field whose value the
// graph already holds.
//
// When every VALUE use of the box is such an unbox, the box's identity is
// unobservable -- nothing compares it, publishes it, locks it or passes it on
// -- so whether `valueOf` would have answered a cached or a fresh object does
// not matter, and each unbox is exactly the primitive `valueOf` was given
// (`Integer.valueOf(v).intValue() == v` by the JLS, whatever the cache holds).
// The fold forwards each unbox to that primitive and deletes the call.
//
// The one other reader is a deopt: a snapshot naming the box as a local or a
// stack slot. The fold uses the same test the scalar-replacement planner uses
// -- [`ea_consumable_snapshot_names`], with the call and its unboxes treated as
// gone -- and refuses a site whose box is live at any point a deopt can still
// resume at. The remaining snapshot slots naming the box -- all of them in
// snapshots no deopt can consult, by that test -- are cleared to `NO_NODE`
// (which `ir_optimize`'s dead-slot normalisation would do anyway, since this
// runs before it), and `ir_prune_unconsumable_snapshots` drops those snapshots
// after escape analysis. Inlined code is safe under the same test with no
// spliced ranges: the builder records no snapshot inside a splice, so a
// deoptable node at a relocated pc makes `ir_consumable_snapshot_bcis` answer
// "unattributable" and the test falls back to "any snapshot names the box",
// which refuses.
//
// Kill switch: `CRATONVM_NO_IR_BOX_UNBOX_FOLD=1`.

/// The unbox that reads back exactly what `node` -- an `Op::Call` carrying
/// `info_ptr` -- boxed: `IntValue` for `Integer.valueOf(I)`, `LongValue` for
/// `Long.valueOf(J)`; `None` for any other call.
///
/// Same `info_ptr` contract as [`ir_call_is_identity_hash`]: `0` is refused
/// without a dereference, anything else addresses an interned `JitInvokeInfo`.
fn ir_call_box_value_of(node: &ir::Node, info_ptr: usize) -> Option<ir::UnboxOp> {
    // `[ctrl, mem, value]`.
    if info_ptr == 0 || node.inputs.len() != 3 {
        return None;
    }
    // SAFETY: `info_ptr` is non-zero here and addresses an interned `JitInvokeInfo`; see `ir_call_is_identity_hash`.
    let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
    // invoke_kind 3 = static. Both are `static` in the JDK; a site that
    // reached here any other way is not the method this fold reasons about.
    if info.invoke_kind != 3 {
        return None;
    }
    match (info.class_name, info.method_name, info.descriptor) {
        ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;") => Some(ir::UnboxOp::IntValue),
        ("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;") => Some(ir::UnboxOp::LongValue),
        _ => None,
    }
}

/// Every live node that reads the box `boxed` as a VALUE, when each of them is
/// an `Op::Unbox { op: uop }` taking it as its receiver; `None` as soon as one
/// is anything else. Memory-token edges naming `boxed` are not reads and are
/// skipped (they are spliced when the call goes). Ascending node order.
fn ir_box_readers(g: &ir::Graph, boxed: ir::NodeId, uop: ir::UnboxOp) -> Option<Vec<ir::NodeId>> {
    let mut readers: Vec<ir::NodeId> = Vec::new();
    for (uid, u) in g.nodes.iter().enumerate() {
        if u.op == ir::Op::Dead {
            continue;
        }
        for (i, &inp) in u.inputs.iter().enumerate() {
            if inp != boxed || ea_is_memory_token_slot(u, i) {
                continue;
            }
            // `[ctrl, mem, receiver]`: the box must be the RECEIVER (slot 2),
            // the unbox must have a token of its own to hand on, and it must
            // read back the width that was boxed.
            let reads_back = i == 2
                && u.inputs.len() == 3
                && u.input_opt(1).is_some()
                && u.ty == uop.result_type()
                && matches!(&u.op, ir::Op::Unbox { op, .. } if *op == uop);
            if !reads_back {
                return None;
            }
            let uid = uid as ir::NodeId;
            if !readers.contains(&uid) {
                readers.push(uid);
            }
        }
    }
    Some(readers)
}

/// One `valueOf` site [`ir_fold_unbox_of_fresh_box`] retires.
struct BoxFoldPlan {
    call: ir::NodeId,
    uop: ir::UnboxOp,
    /// The primitive the call boxed (`inputs[2]`).
    value: ir::NodeId,
    /// Its `Op::Unbox` readers, ascending.
    unboxes: Vec<ir::NodeId>,
}

/// Retire `victim`: memory-token edges naming it follow the chain to `token`,
/// value edges and snapshot slots move to `value` (`NO_NODE` when `None`, which
/// the planner only allows where no value edge remains), then the node dies.
fn ir_retire_folded_node(
    g: &mut ir::Graph,
    victim: ir::NodeId,
    token: ir::NodeId,
    value: Option<ir::NodeId>,
) {
    let replacement = value.unwrap_or(ir::NO_NODE);
    let mut edits: Vec<(ir::NodeId, usize, ir::NodeId)> = Vec::new();
    for (uid, n) in g.nodes.iter().enumerate() {
        if n.op == ir::Op::Dead {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            if inp != victim {
                continue;
            }
            let new = if ea_is_memory_token_slot(n, i) {
                token
            } else {
                replacement
            };
            edits.push((uid as ir::NodeId, i, new));
        }
    }
    for (user, slot, new) in edits {
        g.set_input(user, slot, new);
    }
    for s in g.safepoint_users_of(victim) {
        g.set_safepoint_slot(s.snapshot as usize, s.kind, s.slot as usize, replacement);
    }
    g.kill(victim);
}

/// Fold `Integer.valueOf(v)` / `Long.valueOf(v)` whose only value readers are
/// `intValue()` / `longValue()` unboxes: forward every unbox to `v` and delete
/// the call. Returns how many `valueOf` sites were removed. See the section
/// comment above for why this is sound and what it refuses.
pub(crate) fn ir_fold_unbox_of_fresh_box(ir_graph: &mut ir::Graph) -> usize {
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_NO_IR_BOX_UNBOX_FOLD") {
        return 0;
    }
    let mut plans: Vec<BoxFoldPlan> = Vec::new();
    {
        let g: &ir::Graph = ir_graph;
        for (idx, n) in g.nodes.iter().enumerate() {
            let ir::Op::Call { info_ptr } = &n.op else {
                continue;
            };
            let Some(uop) = ir_call_box_value_of(n, *info_ptr) else {
                continue;
            };
            let call = idx as ir::NodeId;
            // An incoming memory token to splice onto, and the boxed value.
            let (Some(_), Some(value)) = (n.input_opt(1), n.input_opt(2)) else {
                continue;
            };
            if g.type_of(value) != Some(uop.result_type()) {
                continue;
            }
            let Some(unboxes) = ir_box_readers(g, call, uop) else {
                continue;
            };
            let mut ignore: std::collections::HashSet<ir::NodeId> =
                unboxes.iter().copied().collect();
            ignore.insert(call);
            if ea_consumable_snapshot_names(g, call, &ignore) {
                continue;
            }
            // Round 13 wave 6 (lane chain2): nor one a chain snapshot names.
            // Retiring the call clears that slot to `NO_NODE`, and a guard
            // publishing the chain would resume the spliced callee with the
            // box's reference slot undefined (`ea_any_chain_snapshot_names`).
            if ea_any_chain_snapshot_names(g, call) {
                continue;
            }
            plans.push(BoxFoldPlan {
                call,
                uop,
                value,
                unboxes,
            });
        }
    }
    if plans.is_empty() {
        return 0;
    }

    // `forwarded[u]` = the primitive an already-retired unbox now stands for.
    // A later plan's boxed value can be an earlier plan's unbox
    // (`Integer.valueOf(Integer.valueOf(v).intValue())`).
    let mut forwarded: HashMap<ir::NodeId, ir::NodeId> = HashMap::new();
    let mut folded = 0usize;
    for plan in &plans {
        let mut value = plan.value;
        let mut hops = 0usize;
        while let Some(&next) = forwarded.get(&value) {
            value = next;
            hops += 1;
            if hops > plans.len() {
                break;
            }
        }
        // Re-validate on the live graph: an earlier plan only ever rewires
        // edges onto primitives and tokens, but a plan is applied whole or not
        // at all, so check rather than assume.
        let call_live = ir_graph
            .node_opt(plan.call)
            .is_some_and(|n| matches!(n.op, ir::Op::Call { .. }) && n.input_opt(1).is_some());
        let value_live = ir_graph
            .node_opt(value)
            .is_some_and(|n| n.op != ir::Op::Dead && n.ty == plan.uop.result_type());
        if !call_live
            || !value_live
            || ir_box_readers(ir_graph, plan.call, plan.uop).as_ref() != Some(&plan.unboxes)
        {
            continue;
        }
        for &u in &plan.unboxes {
            // Read at retire time: an earlier unbox of this plan may have been
            // this one's token, and has already been spliced out.
            let Some(token) = ir_graph.node_opt(u).and_then(|n| n.input_opt(1)) else {
                continue;
            };
            ir_retire_folded_node(ir_graph, u, token, Some(value));
            forwarded.insert(u, value);
        }
        let Some(token) = ir_graph.node_opt(plan.call).and_then(|n| n.input_opt(1)) else {
            continue;
        };
        ir_retire_folded_node(ir_graph, plan.call, token, None);
        folded += 1;
    }
    folded
}

// ── Box hashCode fold (round 13 wave 8, lane iropt7) ─────────────────
//
// `map.put(i, v)` / `map.get(i)` box the key with `Integer.valueOf(i)`, and the
// spliced `HashMap.hash(key)` asks it `key == null` and `key.hashCode()`. Both
// answers are known from the call that made the box: `valueOf` never returns
// null (it answers a cached box or a new one, or throws), and whichever box it
// answers holds exactly `i`, so `Integer.valueOf(i).hashCode() == i` (JLS
// `Integer.hashCode()`; the same argument `ir_fold_unbox_of_fresh_box` makes
// for `intValue()`, and the one C2's box elimination makes). `Integer` is
// final, so an `invokevirtual hashCode` on such a receiver can only select
// `Integer.hashCode()`.
//
// The call used to stay: the lowerer answered it inline through the
// box-hash-equals prefix (class test, layout guard, payload load), but in the
// graph it was an opaque `Op::Call` -- a memory barrier for load CSE and LICM,
// a safepoint, and an escape of the key box. The fold forwards the call's
// value (and every snapshot slot naming it) to `i` and splices it out of the
// memory chain. The box itself is untouched: it is still stored into the map.
//
// Not folded: `invokespecial` (`super.hashCode()` selects `Object`'s identity
// hash) and any other receiver. `Long` (round 13 wave 10, lane iropt8, I7-7):
// its hash is `(int)(v ^ v >>> 32)`, built here from `UShr` / `Xor` / `L2I`
// (`Long` is final too); `CRATONVM_JIT_IR_LONG_BOX_HASH_FOLD=0` keeps its call.
//
// Kill switch: `CRATONVM_JIT_IR_BOX_HASH_FOLD=0` (also restores the null
// compare of a `valueOf` result in `ir_fold_null_checks_on_fresh_allocations`).

/// `CRATONVM_JIT_IR_BOX_HASH_FOLD`, default ON. Read once per compile.
fn ir_box_hash_fold_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_BOX_HASH_FOLD")
}

/// Is `node` -- an `Op::Call` carrying `info_ptr` -- `recv.hashCode()` through
/// `invokevirtual` on `Object`, `Number` or `Integer`? Same `info_ptr` contract
/// as [`ir_call_is_identity_hash`].
fn ir_call_is_virtual_hash_code(node: &ir::Node, info_ptr: usize) -> bool {
    // `[ctrl, mem, receiver]`.
    if info_ptr == 0 || node.inputs.len() != 3 || node.ty != ir::IrType::Int {
        return false;
    }
    // SAFETY: `info_ptr` is non-zero here and addresses an interned `JitInvokeInfo`; see `ir_call_is_identity_hash`.
    let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
    // invoke_kind 0 = invokevirtual. Never 1: `invokespecial Object.hashCode`
    // is the identity hash whatever the receiver is.
    info.invoke_kind == 0
        && info.method_name == "hashCode"
        && info.descriptor == "()I"
        && matches!(
            info.class_name,
            "java/lang/Object" | "java/lang/Number" | "java/lang/Integer" | "java/lang/Long"
        )
}

/// The declared class of the `invokevirtual hashCode` `node` carries, when it
/// is the box's own class (the site the verifier types as `Integer` / `Long`)
/// rather than `Object` / `Number`. Same `info_ptr` contract as
/// [`ir_call_is_identity_hash`].
fn ir_hash_code_declared_box(node: &ir::Node, info_ptr: usize) -> Option<ir::UnboxOp> {
    if info_ptr == 0 || !matches!(node.op, ir::Op::Call { .. }) {
        return None;
    }
    // SAFETY: `info_ptr` is non-zero here and addresses an interned `JitInvokeInfo`; see `ir_call_is_identity_hash`.
    let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
    match info.class_name {
        "java/lang/Integer" => Some(ir::UnboxOp::IntValue),
        "java/lang/Long" => Some(ir::UnboxOp::LongValue),
        _ => None,
    }
}

/// Round 13 wave 10 (lane iropt8, proposal I7-7): `CRATONVM_JIT_IR_LONG_BOX_HASH_FOLD`,
/// default ON: [`ir_fold_hash_code_of_integer_box`] also folds
/// `Long.valueOf(v).hashCode()` to `(int) (v ^ (v >>> 32))` (JLS / `Long.hashCode(long)`),
/// built from the same `UShr` / `Xor` / `L2I` nodes `lushr` / `lxor` / `l2i`
/// build. `0` keeps the call for a `Long` box. Read once per compile.
fn ir_long_box_hash_fold_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LONG_BOX_HASH_FOLD")
}

/// The primitive `id` boxed and which box it is, when `id` is a live
/// `Integer.valueOf(I)` or `Long.valueOf(J)` call.
fn ir_box_payload(g: &ir::Graph, id: ir::NodeId) -> Option<(ir::NodeId, ir::UnboxOp)> {
    let n = g.node_opt(id)?;
    let ir::Op::Call { info_ptr } = n.op else {
        return None;
    };
    let uop = ir_call_box_value_of(n, info_ptr)?;
    let value = n.input_opt(2)?;
    let live = g.node_opt(value).is_some_and(|v| v.op != ir::Op::Dead);
    (live && g.type_of(value) == Some(uop.result_type())).then_some((value, uop))
}

/// Fold `Integer.valueOf(v).hashCode()` to `v`, and (round 13 wave 10,
/// `CRATONVM_JIT_IR_LONG_BOX_HASH_FOLD`) `Long.valueOf(v).hashCode()` to
/// `(int) (v ^ (v >>> 32))` (see the section comment). Returns how many calls
/// were removed.
pub(crate) fn ir_fold_hash_code_of_integer_box(ir_graph: &mut ir::Graph) -> usize {
    if !ir_box_hash_fold_enabled() {
        return 0;
    }
    let long_too = ir_long_box_hash_fold_enabled();
    // The box `recv` is, when `n` (an `invokevirtual hashCode`) may be folded
    // on it: a live `valueOf` of a kind this compile folds, and -- when the
    // site names a box class rather than `Object` / `Number` -- that class.
    let foldable = |g: &ir::Graph,
                    n: &ir::Node,
                    info_ptr: usize,
                    recv: ir::NodeId|
     -> Option<(ir::NodeId, ir::UnboxOp)> {
        if !ir_call_is_virtual_hash_code(n, info_ptr) {
            return None;
        }
        let (value, uop) = ir_box_payload(g, recv)?;
        if uop == ir::UnboxOp::LongValue && !long_too {
            return None;
        }
        match ir_hash_code_declared_box(n, info_ptr) {
            Some(declared) if declared != uop => None,
            _ => Some((value, uop)),
        }
    };
    let mut calls: Vec<ir::NodeId> = Vec::new();
    {
        let g: &ir::Graph = ir_graph;
        for (idx, n) in g.nodes.iter().enumerate() {
            let ir::Op::Call { info_ptr } = n.op else {
                continue;
            };
            let (Some(_), Some(recv)) = (n.input_opt(1), n.input_opt(2)) else {
                continue;
            };
            if foldable(g, n, info_ptr, recv).is_some() {
                calls.push(idx as ir::NodeId);
            }
        }
    }
    let mut folded = 0usize;
    for call in calls {
        // Re-read on the live graph: an earlier fold may have rewired the
        // boxed value (`valueOf(x.hashCode()).hashCode()`).
        let Some(n) = ir_graph.node_opt(call) else {
            continue;
        };
        let ir::Op::Call { info_ptr } = n.op else {
            continue;
        };
        let (Some(token), Some(recv)) = (n.input_opt(1), n.input_opt(2)) else {
            continue;
        };
        let Some((value, uop)) = foldable(&*ir_graph, n, info_ptr, recv) else {
            continue;
        };
        let hash = match uop {
            ir::UnboxOp::LongValue => {
                // `Long.hashCode(long)`: `(int) (v ^ (v >>> 32))`, in the node
                // shapes `lushr` / `lxor` / `l2i` build (a `long` shifted by an
                // `int` count).
                let c32 = ir_graph.add(ir::Op::Const(32), ir::IrType::Int, vec![], None);
                let hi = ir_graph.add(ir::Op::UShr, ir::IrType::Long, vec![value, c32], None);
                let x = ir_graph.add(ir::Op::Xor, ir::IrType::Long, vec![value, hi], None);
                ir_graph.add(ir::Op::L2I, ir::IrType::Int, vec![x], None)
            }
            _ => value,
        };
        ir_retire_folded_node(ir_graph, call, token, Some(hash));
        folded += 1;
    }
    folded
}

// ── String literal hashCode fold (round 14 wave 3, lane calls, I7-4) ──
//
// `map.get("name")` with the map's `get` / `hash` spliced asks the literal
// `key == null` and `key.hashCode()`. Both answers are fixed by the class file:
// `ldc <String>` yields the interned `String` whose chars are the constant's
// text (JVMS 5.1), `String` is final so an `invokevirtual hashCode` on it --
// declared `String` or `Object` -- selects `String.hashCode()`, and that is
// `s[0]*31^(n-1) + ... + s[n-1]` over the UTF-16 units with `int` wrap (its
// specification; the cached `hash` field only memoises it). The graph cannot
// see the text (`Op::ConstString` carries the SITE), so the compile hands the
// hashes in, keyed by site: `literal_hashes`, built by the VM from the constant
// pool the `ldc` resolver read (`vm_exec::java_string_hash`, over the literal's
// UTF-16 units).
//
// The call is forwarded to the constant and spliced out of the memory chain;
// the `Op::ConstString` stays (its resolution may still be the first, and the
// literal may have other readers). Not folded: `invokespecial` (never a
// `hashCode` of `String` from outside it) and any receiver that is not an
// `ldc` result.
//
// Kill switch: `CRATONVM_JIT_IR_STRING_LITERAL_HASH_FOLD=0` (also keeps the
// null compare of an `ldc <String>` result).

/// `CRATONVM_JIT_IR_STRING_LITERAL_HASH_FOLD`, default ON. Read once per compile.
fn ir_string_literal_hash_fold_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_STRING_LITERAL_HASH_FOLD")
}

/// Is `node` -- an `Op::Call` carrying `info_ptr` -- `recv.hashCode()` through
/// `invokevirtual` on `String` or `Object`? Same `info_ptr` contract as
/// [`ir_call_is_identity_hash`].
fn ir_call_is_string_hash_code(node: &ir::Node, info_ptr: usize) -> bool {
    // `[ctrl, mem, receiver]`.
    if info_ptr == 0 || node.inputs.len() != 3 || node.ty != ir::IrType::Int {
        return false;
    }
    // SAFETY: `info_ptr` is non-zero here and addresses an interned `JitInvokeInfo`; see `ir_call_is_identity_hash`.
    let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
    info.invoke_kind == 0
        && info.method_name == "hashCode"
        && info.descriptor == "()I"
        && matches!(info.class_name, "java/lang/Object" | "java/lang/String")
}

/// Fold `"literal".hashCode()` to the literal's hash for every `ldc` site in
/// `literal_hashes` (see the section comment). Returns how many calls were
/// removed.
pub(crate) fn ir_fold_hash_code_of_string_literal(
    ir_graph: &mut ir::Graph,
    literal_hashes: &HashMap<(u32, u16), i32>,
) -> usize {
    if literal_hashes.is_empty() || !ir_string_literal_hash_fold_enabled() {
        return 0;
    }
    let literal_hash = |g: &ir::Graph, recv: ir::NodeId| -> Option<i32> {
        let r = g.node_opt(recv)?;
        match r.op {
            ir::Op::ConstString {
                holder_class_id,
                cp_idx,
                ..
            } if r.ty == ir::IrType::Ref => literal_hashes.get(&(holder_class_id, cp_idx)).copied(),
            _ => None,
        }
    };
    let mut plans: Vec<(ir::NodeId, i32)> = Vec::new();
    {
        let g: &ir::Graph = ir_graph;
        for (idx, n) in g.nodes.iter().enumerate() {
            let ir::Op::Call { info_ptr } = n.op else {
                continue;
            };
            if !ir_call_is_string_hash_code(n, info_ptr) {
                continue;
            }
            let (Some(_), Some(recv)) = (n.input_opt(1), n.input_opt(2)) else {
                continue;
            };
            if let Some(hash) = literal_hash(g, recv) {
                plans.push((idx as ir::NodeId, hash));
            }
        }
    }
    let mut folded = 0usize;
    for (call, hash) in plans {
        // Read at retire time: an earlier fold may have been this call's token.
        let Some(token) = ir_graph.node_opt(call).and_then(|n| n.input_opt(1)) else {
            continue;
        };
        let c = ir_graph.add(ir::Op::Const(i64::from(hash)), ir::IrType::Int, vec![], None);
        ir_retire_folded_node(ir_graph, call, token, Some(c));
        folded += 1;
    }
    folded
}

// ── String literal query folds (round 14 wave 4, lane calls2, C14W3-4) ──
//
// The I7-4 argument (section above) with the literal's TEXT, not only its
// hash: `ldc <String>` yields the interned `String` whose UTF-16 units are the
// constant's text, and `String` is final, so on such a receiver
//
// * `length()` is the unit count, `isEmpty()` whether it is zero;
// * `charAt(k)` for a constant `0 <= k < length` is unit `k` (zero-extended;
//   an out-of-range `k` keeps its call, which throws);
// * `equals(o)` (declared `String` or `Object`) is `true` for the same node,
//   `false` for `null`, and the texts' equality for another literal (two
//   literals with equal text are the same interned object, and `equals`
//   compares units).
//
// None of them can throw on a non-null String, so the call goes (spliced out of
// the memory chain as the hash fold does) and the `Op::ConstString` stays.
// `length`/`isEmpty`/`charAt` are folded both as calls (inside a splice, where
// the string access expansion refuses, or with it switched off) and, for
// `length`/`isEmpty`, in the expansion's own shape (`IrBuilder::
// try_string_access_intrinsic`): its `char_count` is `value.length >>> coder`
// read off the receiver, `UShr(ArrayLength(Load<Ref>(r)), Load<Int>(r))`.
// `value` is `String`'s only instance reference field, so an `ArrayLength` of
// a `Load<Ref>` of a String is `value.length`, and `value.length >>> coder` is
// `length()` whatever the VM's compact-strings setting; the loads and guards
// stay (they are what the expansion ordered), only the count becomes the
// constant, which the optimizer then carries into `isEmpty`'s compare and
// `charAt`'s bounds guard. The expansion's `charAt` decode is not folded.
//
// The texts come from the compile (`(holder_class_id, cp_idx)` -> UTF-16
// units), exactly the literals whose hash the I7-4 map carries; an empty map
// folds nothing. Input patch: `r14w4-calls2-literal-text-plumbing-patch-FIXED-20260929.md`.
//
// Kill switch: `CRATONVM_JIT_IR_STRING_LITERAL_QUERY_FOLD=0`.

/// `CRATONVM_JIT_IR_STRING_LITERAL_QUERY_FOLD`, default ON. Read once per compile.
fn ir_string_literal_query_fold_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_STRING_LITERAL_QUERY_FOLD")
}

/// Which literal query an `Op::Call` asks (see the section comment).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StringLiteralQuery {
    Length,
    IsEmpty,
    CharAt,
    Equals,
}

/// The query `node` -- an `Op::Call` carrying `info_ptr` -- asks of its
/// receiver, when it is one of the four. Same `info_ptr` contract as
/// [`ir_call_is_identity_hash`]: `0` is refused without a dereference.
fn ir_call_string_literal_query(node: &ir::Node, info_ptr: usize) -> Option<StringLiteralQuery> {
    if info_ptr == 0 || node.ty != ir::IrType::Int {
        return None;
    }
    // SAFETY: `info_ptr` is non-zero here and addresses an interned `JitInvokeInfo`; see `ir_call_is_identity_hash`.
    let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
    // `invokevirtual` on `String`, or `invokeinterface` on `CharSequence`
    // (which a String receiver answers with `String`'s own method).
    let accessor = (info.invoke_kind == 0 && info.class_name == "java/lang/String")
        || (info.invoke_kind == 2 && info.class_name == "java/lang/CharSequence");
    // `[ctrl, mem, receiver, args...]`.
    match (info.method_name, info.descriptor, node.inputs.len()) {
        ("length", "()I", 3) if accessor => Some(StringLiteralQuery::Length),
        ("isEmpty", "()Z", 3) if accessor => Some(StringLiteralQuery::IsEmpty),
        ("charAt", "(I)C", 4) if accessor => Some(StringLiteralQuery::CharAt),
        ("equals", "(Ljava/lang/Object;)Z", 4)
            if info.invoke_kind == 0
                && matches!(info.class_name, "java/lang/String" | "java/lang/Object") =>
        {
            Some(StringLiteralQuery::Equals)
        }
        _ => None,
    }
}

/// Fold the literal queries of the section comment for every `ldc` site
/// `literal_texts` names (`(holder_class_id, cp_idx)` -> the literal's UTF-16
/// units). Returns how many calls and expansion counts were folded.
pub(crate) fn ir_fold_string_literal_queries(
    ir_graph: &mut ir::Graph,
    literal_texts: &HashMap<(u32, u16), Vec<u16>>,
) -> usize {
    if literal_texts.is_empty() || !ir_string_literal_query_fold_enabled() {
        return 0;
    }
    /// The UTF-16 units of the literal `id` is, when it is a named `ldc` site.
    fn text_of<'t>(
        g: &ir::Graph,
        literal_texts: &'t HashMap<(u32, u16), Vec<u16>>,
        id: ir::NodeId,
    ) -> Option<&'t [u16]> {
        let n = g.node_opt(id)?;
        match n.op {
            ir::Op::ConstString {
                holder_class_id,
                cp_idx,
                ..
            } if n.ty == ir::IrType::Ref => literal_texts
                .get(&(holder_class_id, cp_idx))
                .map(|t| t.as_slice()),
            _ => None,
        }
    }
    /// Cast: a class-file string constant has at most 65 535 UTF-8 bytes, so
    /// its unit count fits `i64` (and `int`).
    fn len_of(text: &[u16]) -> i64 {
        text.len() as i64
    }
    // `(call, value)` pairs retire the call; `(count, value)` pairs replace a
    // pure expansion count.
    let mut calls: Vec<(ir::NodeId, i64)> = Vec::new();
    let mut counts: Vec<(ir::NodeId, i64)> = Vec::new();
    {
        let g: &ir::Graph = ir_graph;
        for (idx, n) in g.nodes.iter().enumerate() {
            match n.op {
                ir::Op::Call { info_ptr } => {
                    let Some(query) = ir_call_string_literal_query(n, info_ptr) else {
                        continue;
                    };
                    let (Some(_), Some(recv)) = (n.input_opt(1), n.input_opt(2)) else {
                        continue;
                    };
                    let Some(text) = text_of(g, literal_texts, recv) else {
                        continue;
                    };
                    let value: Option<i64> = match query {
                        StringLiteralQuery::Length => Some(len_of(text)),
                        StringLiteralQuery::IsEmpty => Some(i64::from(text.is_empty())),
                        StringLiteralQuery::CharAt => n
                            .input_opt(3)
                            .and_then(|index| g.node_opt(index))
                            .and_then(|index| match index.op {
                                ir::Op::Const(at) if (0..len_of(text)).contains(&at) => {
                                    usize::try_from(at).ok()
                                }
                                _ => None,
                            })
                            .and_then(|at| text.get(at))
                            .map(|&unit| i64::from(unit)),
                        StringLiteralQuery::Equals => match n.input_opt(3) {
                            Some(arg) if arg == recv => Some(1),
                            Some(arg) => match g.node_opt(arg) {
                                Some(a) if a.op == ir::Op::Const(0) && a.ty == ir::IrType::Ref => {
                                    Some(0)
                                }
                                Some(_) => text_of(g, literal_texts, arg)
                                    .map(|other| i64::from(other == text)),
                                None => None,
                            },
                            None => None,
                        },
                    };
                    if let Some(value) = value {
                        calls.push((idx as ir::NodeId, value));
                    }
                }
                ir::Op::UShr if n.ty == ir::IrType::Int && n.inputs.len() == 2 => {
                    let (byte_len, coder) = (n.inputs[0], n.inputs[1]);
                    // `Load<Int>(recv)`: the coder read, `[ctrl, mem, recv, idx, ..]`.
                    let coder_recv = g.node_opt(coder).and_then(|c| match c.op {
                        ir::Op::Load(ir::MemKind::Int) => c.input_opt(2),
                        _ => None,
                    });
                    // `ArrayLength(Load<Ref>(recv))`: `value.length`.
                    let value_recv = g
                        .node_opt(byte_len)
                        .and_then(|l| match l.op {
                            ir::Op::ArrayLength => l.input_opt(2),
                            _ => None,
                        })
                        .and_then(|v| g.node_opt(v))
                        .and_then(|v| match v.op {
                            ir::Op::Load(ir::MemKind::Ref) => v.input_opt(2),
                            _ => None,
                        });
                    match (coder_recv, value_recv) {
                        (Some(a), Some(b)) if a == b => {
                            if let Some(text) = text_of(g, literal_texts, a) {
                                counts.push((idx as ir::NodeId, len_of(text)));
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
    let mut folded = 0usize;
    for (call, value) in calls {
        // Read at retire time: an earlier fold may have been this call's token.
        let Some(token) = ir_graph.node_opt(call).and_then(|n| n.input_opt(1)) else {
            continue;
        };
        let c = ir_graph.add(ir::Op::Const(value), ir::IrType::Int, vec![], None);
        ir_retire_folded_node(ir_graph, call, token, Some(c));
        folded += 1;
    }
    for (count, value) in counts {
        let c = ir_graph.add(ir::Op::Const(value), ir::IrType::Int, vec![], None);
        ir_graph.replace_all_uses(count, c);
        ir_graph.kill(count);
        folded += 1;
    }
    if folded > 0 && ir_stage_reporting() {
        eprintln!("[ir] string-literal query fold: {folded} length/isEmpty/charAt/equals answer(s) folded");
    }
    folded
}

/// Does a lowered artifact carry a deopt point that names an allocation escape
/// analysis removed but the lowerer could not describe?
///
/// The backstop for the rule [`scalar_deopt_descriptor_available`] documents:
/// the planner elides an allocation a consultable snapshot names only when a
/// recipe will exist, and the lowerer can still refuse that recipe at a
/// particular point (`MaterializationRequired` with an allocation cause). Such
/// a point is unresumable, and the sink falls back to re-running the method.
pub(crate) fn ir_artifact_names_an_undescribable_elided_object(cm: &CompiledMethod) -> bool {
    fn value_names_one(v: &deopt::FrameValue) -> bool {
        match v {
            deopt::FrameValue::MaterializationRequired(e) => matches!(
                e.cause,
                deopt::EliminationCause::ScalarReplacedObject
                    | deopt::EliminationCause::NestedVirtualObject
            ),
            deopt::FrameValue::VirtualObject(state) => {
                state.field_values.iter().any(value_names_one)
            }
            _ => false,
        }
    }
    fn state_names_one(fs: &deopt::FrameState) -> bool {
        let mut scope = Some(fs);
        let mut depth = 0usize;
        while let Some(s) = scope {
            if s.locals.iter().chain(s.stack.iter()).any(value_names_one)
                || s.monitors.iter().any(|m| value_names_one(&m.object))
            {
                return true;
            }
            depth += 1;
            // Deeper than any inliner builds: treat a malformed chain as
            // naming one rather than walk it further.
            if depth > 256 {
                return true;
            }
            scope = s.caller.as_deref();
        }
        false
    }
    cm.deopt_points
        .iter()
        .any(|p| state_names_one(&p.frame_state))
        || cm
            ._deopt_point_boxes
            .iter()
            .any(|p| state_names_one(&p.frame_state))
}

/// How [`apply_ea_to_ir_planned`] retires one node.
#[derive(Clone, Copy)]
enum EaVictimKind {
    /// Every *non-token* reference — node input and safepoint snapshot slot
    /// alike — is rewired to this replacement value before the node is killed.
    /// Used for a scalar-replaced field `Op::Load`, whose value is known.
    Forwarded(ir::NodeId),
    /// Killed outright. Planning has already proved that the only references
    /// left are memory tokens (spliced at kill time) and — for an eliminated
    /// `Op::New` — safepoint slots that deliberately keep naming it.
    Eliminated,
}

/// One scalar-replacement candidate's decided edit, computed without touching
/// the graph so a refusal costs nothing and can never leave a half-applied
/// object behind.
///
/// `pub(crate)` only so [`build_scalar_replacement_map_with_plans`] can hand
/// its plans to [`apply_ea_to_ir_planned`] through the `lib.rs` EA block
/// (round 11 wave 7); the fields stay private to this module.
pub(crate) struct EaScalarPlan {
    /// `(load node, replacement value)` per field load to forward. The value is
    /// `None` for a never-stored field until the shared zero default is made.
    loads: Vec<(ir::NodeId, Option<ir::NodeId>)>,
    /// The allocation and its field stores — killed only when `elide_alloc`.
    new_node: ir::NodeId,
    stores: Vec<ir::NodeId>,
    /// Whether the allocation itself (and its initialising stores) may go.
    elide_alloc: bool,
}

/// Whether forwarding `value` from an array store straight to a load of
/// element kind `load_kind` gives the load the value it would have read.
///
/// Wide element kinds (int, long, float, double, reference) store the value
/// whole, so any value fits. A narrow kind truncates on store and re-extends on
/// load, so the value must already be in range: a constant in range, the
/// builder's own `i2b`/`i2s` (`(x << k) >> k`) or `i2c` (`x & 0xFFFF`) shape, or
/// a load of the same narrow kind. A `boolean[]` (newarray atype 4) stores
/// `value & 1`, so only 0/1 constants and compare results fit it.
///
/// Round 12 wave 3 (iropt proposal W2-6): a φ fits when every value input
/// fits. A φ only ever yields a value one of its inputs produced, so if each
/// non-φ leaf reachable through φ edges fits the slot, so does every value the
/// φ can take; a φ reached again through a loop edge adds no new value, and is
/// skipped. That is what `z[0] = v > 0` compiles to (`φ(iconst_1, iconst_0)`),
/// which used to refuse scalar replacement of every such `boolean[]`. An
/// undefined edge refuses. `CRATONVM_JIT_IR_EA_NARROW_PHI=0` restores the
/// leaf-only answer.
pub(crate) fn narrow_array_value_fits(
    graph: &ir::Graph,
    value: ir::NodeId,
    load_kind: ir::MemKind,
    array_atype: Option<u8>,
) -> bool {
    let is_phi = |id: ir::NodeId| matches!(graph.node_opt(id), Some(n) if n.op == ir::Op::Phi);
    let widened = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_EA_NARROW_PHI");
    // Switched off, a φ gets the leaf answer it always got: `true` for a wide
    // slot, `false` for a narrow one.
    if !is_phi(value) || !widened {
        return narrow_array_leaf_fits(graph, value, load_kind, array_atype, widened);
    }
    // A bounded walk: a φ web wider than this is refused, not searched.
    const MAX_PHIS: usize = 32;
    let mut seen: Vec<ir::NodeId> = vec![value];
    let mut work: Vec<ir::NodeId> = vec![value];
    while let Some(phi) = work.pop() {
        let Some(node) = graph.node_opt(phi) else {
            return false;
        };
        for input in node.phi_value_inputs() {
            let Some(v) = input else {
                return false;
            };
            if seen.contains(&v) {
                continue;
            }
            if is_phi(v) {
                if seen.len() >= MAX_PHIS {
                    return false;
                }
                seen.push(v);
                work.push(v);
            } else if !narrow_array_leaf_fits(graph, v, load_kind, array_atype, widened) {
                return false;
            }
        }
    }
    true
}

/// [`narrow_array_value_fits`] for one non-φ value. `widened` admits the
/// round-12 wave-3 leaves (the explicit narrowing ops and a compare).
fn narrow_array_leaf_fits(
    graph: &ir::Graph,
    value: ir::NodeId,
    load_kind: ir::MemKind,
    array_atype: Option<u8>,
    widened: bool,
) -> bool {
    use ir::{MemKind, Op};
    let Some(node) = graph.node_opt(value) else {
        return false;
    };
    let const_of = |id: ir::NodeId| match graph.node_opt(id).map(|n| &n.op) {
        Some(Op::Const(c)) => Some(*c),
        _ => None,
    };
    if array_atype == Some(4) {
        return match &node.op {
            Op::Const(c) => matches!(*c, 0 | 1),
            Op::Cmp(_) => true,
            _ => false,
        };
    }
    let (lo, hi, shift) = match load_kind {
        MemKind::Byte => (i64::from(i8::MIN), i64::from(i8::MAX), Some(24)),
        MemKind::Short => (i64::from(i16::MIN), i64::from(i16::MAX), Some(16)),
        MemKind::Char => (0, i64::from(u16::MAX), None),
        _ => return true,
    };
    match &node.op {
        Op::Const(c) => (lo..=hi).contains(c),
        Op::ArrayLoad(mk) => *mk == load_kind,
        // `(x << k) >> k`: sign-extend the low 32 - k bits.
        Op::Shr => shift.is_some_and(|k| {
            node.inputs.len() >= 2
                && const_of(node.inputs[1]) == Some(k)
                && graph.node_opt(node.inputs[0]).is_some_and(|shl| {
                    matches!(shl.op, Op::Shl)
                        && shl.inputs.len() >= 2
                        && const_of(shl.inputs[1]) == Some(k)
                })
        }),
        // `x & 0xFFFF`.
        Op::And => {
            load_kind == MemKind::Char
                && node.inputs.len() >= 2
                && (const_of(node.inputs[1]) == Some(0xFFFF)
                    || const_of(node.inputs[0]) == Some(0xFFFF))
        }
        // Round 12 wave 3 (W2-6): the explicit narrowing ops, should a
        // producer emit them, and a compare's 0/1, which fits every kind.
        // `i2b` is in `short` range too; `i2c` is unsigned and fits only `char`.
        Op::I2B => widened && matches!(load_kind, MemKind::Byte | MemKind::Short),
        Op::I2S => widened && load_kind == MemKind::Short,
        Op::I2C => widened && load_kind == MemKind::Char,
        Op::Cmp(_) => widened,
        _ => false,
    }
}

/// Decide what may be done to one scalar-replacement candidate. It never
/// mutates `ir_graph`. `None` refuses the object entirely (it stays allocated,
/// its stores stay, its loads stay).
///
/// `note_census` says whether to count this decision in
/// `cratonvm_types::scalar_deopt_census`. Each candidate is planned ONCE per
/// round on the production path (round 11 wave 7): by
/// [`build_scalar_replacement_map_with_plans`] when descriptors are on, which
/// hands its plans to [`apply_ea_to_ir_planned`], and by the applier itself
/// otherwise. Whichever plans counts. The test-only wrappers that still plan
/// twice count only in the applier; the census used to count every
/// allocation twice.
fn plan_scalar_replacement(
    ir_graph: &ir::Graph,
    reverse_map: &HashMap<escape_analysis::NodeId, ir::NodeId>,
    info: &escape_analysis::ScalarReplacementInfo,
    deopt_descriptor_available: bool,
    descriptor_pinned: &std::collections::HashSet<ir::NodeId>,
    note_census: bool,
) -> Option<EaScalarPlan> {
    let new_node = *reverse_map.get(&info.alloc_node)?;
    // An array candidate names an `Op::NewArray` and its slots are reached by
    // `Op::ArrayLoad` / `Op::ArrayStore`; an object candidate names an
    // `Op::New` and its fields by `Op::Load` / `Op::Store`. The two node kinds
    // are checked against `info.array_element_type` rather than accepted
    // interchangeably, so a candidate whose EA-side and IR-side shapes disagree
    // is refused instead of half-applied.
    let is_array = info.array_element_type.is_some();
    let alloc_op_matches = match &ir_graph.node_opt(new_node)?.op {
        ir::Op::New { .. } => !is_array,
        ir::Op::NewArray { .. } => is_array,
        _ => false,
    };
    if !alloc_op_matches {
        return None;
    }

    // The slot index a load/store accesses. Must match the index the EA bridge
    // keyed its edges by: for a field, the real index from the `Const` offset
    // operand of a full-layout production node, falling back to the
    // `MemKind`-derived index for a compact / hand-built one; for an array
    // element, the `Const` index operand.
    let field_index = |id: ir::NodeId| -> Option<usize> {
        if is_array {
            return ir_array_const_index(ir_graph, id);
        }
        if let Some(f) = ir_load_store_field_index(ir_graph, id) {
            return Some(f);
        }
        match &ir_graph.node_opt(id)?.op {
            ir::Op::Load(mk) | ir::Op::Store(mk) => Some(*mk as usize),
            _ => None,
        }
    };
    let load_op_matches = |id: ir::NodeId| -> bool {
        match ir_graph.node_opt(id).map(|n| &n.op) {
            Some(ir::Op::Load(_)) => !is_array,
            Some(ir::Op::ArrayLoad(_)) => is_array,
            _ => false,
        }
    };
    let store_op_matches = |id: ir::NodeId| -> bool {
        match ir_graph.node_opt(id).map(|n| &n.op) {
            Some(ir::Op::Store(_)) => !is_array,
            Some(ir::Op::ArrayStore(_)) => is_array,
            _ => false,
        }
    };

    // Each load is kept with the EA node it came from, because the per-load
    // answer below is keyed by EA id — no forward `id_map` lookup is needed,
    // the EA id is already in hand here.
    let mut loads: Vec<(ir::NodeId, escape_analysis::NodeId)> = Vec::new();
    for &ea_load in &info.replaced_loads {
        // An EA node with no IR counterpart names nothing we can kill.
        let l = match reverse_map.get(&ea_load) {
            Some(&l) => l,
            None => continue,
        };
        if !load_op_matches(l) {
            return None;
        }
        // Bridge/analysis agreement check. The index itself is no longer used
        // to pick the forwarded value (`info.load_value` answers that), but a
        // node whose field index cannot be recovered is one the bridge and the
        // analysis disagree about — refuse rather than guess.
        if field_index(l).is_none() {
            return None;
        }
        loads.push((l, ea_load));
    }
    let mut stores: Vec<ir::NodeId> = Vec::new();
    for &ea_store in &info.eliminated_stores {
        let s = match reverse_map.get(&ea_store) {
            Some(&s) => s,
            None => continue,
        };
        if !store_op_matches(s) {
            return None;
        }
        if field_index(s).is_none() {
            return None;
        }
        stores.push(s);
    }

    // ── Per-load resolution (NOT `field_values`) ─────────────────────
    //
    // `info.field_values[f]` is the value of the LAST store to `f` in program
    // order. Forwarding *every* replaced load of `f` to it folds
    // `Foo o = new Foo(); int a = o.x; o.x = 42;` to `a == 42` — a value from
    // the load's own future. This pass used to do exactly that, and guarded it
    // with an id-comparison refusal ("refuse the object if any store to the
    // loaded field has a higher node id than the load"). That guard was correct
    // but pessimistic, and it did nothing for the branchy variant
    // (`if (c) o.x = 1; else o.x = 2; int a = o.x;`), where no store has a
    // higher id than the load yet `field_values[f]` is still the wrong answer
    // on one path.
    //
    // `escape_analysis::ScalarReplacementInfo::load_value` replaces both. It is
    // three-valued on purpose — collapsing `ZeroDefault` and `Unknown` into one
    // `Option<NodeId>` is precisely how a load-before-store gets a value from
    // its future — and it is gated on
    // `escape_analysis::program_order_proves_dominance`, so the branchy graph
    // above answers `Unknown` and is refused here rather than mis-forwarded.
    // See `docs/jit/escape-analysis.md` §3 and §6.1.
    let mut load_plans: Vec<(ir::NodeId, Option<ir::NodeId>)> = Vec::with_capacity(loads.len());
    for &(l, ea_load) in &loads {
        let value = match info.load_value(ea_load) {
            escape_analysis::LoadResolution::Value(ea_val) => match reverse_map.get(&ea_val) {
                Some(&v) if ir_graph.node_opt(v).is_some_and(|n| n.op != ir::Op::Dead) => Some(v),
                // The stored value has no live IR node, so this load cannot be
                // described. REFUSE — the previous code killed the load anyway
                // and left its consumers reading an `Op::Dead` node.
                _ => return None,
            },
            // No store precedes the load: it reads the freshly-allocated
            // object's zero default (`None` here; the caller materialises one
            // shared `Const(0)`). Soundness rests on the object being genuinely
            // zero-initialised — the front end only admits allocations whose
            // constructor sets no non-zero field.
            escape_analysis::LoadResolution::ZeroDefault => None,
            // Not provable (a branchy graph, or a load this candidate does not
            // describe at all). The analysis already refuses such an object
            // outright, so this is unreachable — but it is the one answer that
            // must never be guessed at, so fail closed rather than assume.
            escape_analysis::LoadResolution::Unknown => return None,
        };
        // A narrow array slot stores a TRUNCATED value: `bastore` keeps the low
        // byte (and `& 1` for a `boolean[]`), `castore`/`sastore` the low 16
        // bits, and the matching load re-extends. Forwarding the stored node to
        // the load skips both halves, so `sipush 300; bastore; ...; baload`
        // read back 300 instead of 44. javac always narrows first (`i2b`,
        // `i2c`, `i2s`), but other compilers need not; refuse unless the value
        // provably already fits the slot.
        if is_array {
            if let Some(v) = value {
                let load_kind = match &ir_graph.node_opt(l)?.op {
                    ir::Op::ArrayLoad(mk) => *mk,
                    _ => return None,
                };
                if !narrow_array_value_fits(ir_graph, v, load_kind, info.array_element_type) {
                    return None;
                }
            }
        }
        load_plans.push((l, value));
    }
    for &(l, _) in &load_plans {
        if !ea_splice_feasible(ir_graph, l) {
            return None;
        }
    }

    // ── May the ALLOCATION itself go? ────────────────────────────────
    //
    // Killing the `Op::New` removes the object from the machine frame, so a
    // deopt snapshot slot that names it can only be reconstructed from the
    // virtual-object descriptor (`ir_lower::ScalarReplacementMap` →
    // `FrameValue::VirtualObject`). When that descriptor will NOT be emitted —
    // the flags are off, or `build_scalar_replacement_map` would omit this
    // object — the slot resolves to `FrameValue::Undefined`, which every resume
    // sink turns into `Value::Int(0)`: a NULL where a live object was. That is
    // precisely the silent wrong-reconstruction `deopt`'s
    // `FrameValue::MaterializationRequired` exists to make impossible, and a
    // `SafepointSnapshot` slot — a bare `NodeId` — cannot spell it.
    //
    // So keep the allocation AND its initialising stores. The object is then a
    // real, correctly-initialised heap object at the safepoint whose slot
    // resolves the ordinary way (`StackSlotRef`), and the load forwarding above
    // still applies: this costs the elided allocation, not the optimization.
    let mut elide_alloc = true;
    // A LOAD A RECIPE NAMES (round 11 wave 2). `descriptor_pinned` holds every
    // field value an accumulated recipe names, loads included, and the caller
    // merges recipes across rounds without rewriting the earlier ones. A later
    // round that forwarded and killed such a load left that recipe naming a
    // dead node: the lowerer answers it `MaterializationRequired`, and the
    // whole optimized artifact is discarded. So a pinned load is left in place
    // as an ordinary load, which is only sound if the object it reads stays
    // allocated. The other loads are still forwarded.
    if load_plans
        .iter()
        .any(|(l, _)| descriptor_pinned.contains(l))
    {
        load_plans.retain(|(l, _)| !descriptor_pinned.contains(l));
        elide_alloc = false;
    }
    // PINNED BY AN EARLIER ROUND'S DESCRIPTOR. Escape analysis is iterated
    // (scalar replacement exposes scalar replacement), and a descriptor built
    // in an earlier round can name this allocation as another object's FIELD
    // VALUE. Deleting it WITHOUT LEAVING A RECIPE OF ITS OWN would strand that
    // reference: the enclosing object's field would resolve to a node the
    // lowerer cannot answer, and `frame_value_for_object` would refuse the
    // whole enclosing object -- turning a resumable deopt point into an
    // unresumable one.
    //
    // Leaving a recipe of its own is the escape hatch, and it is the normal
    // case: the enclosing object's field then resolves to a NESTED virtual
    // object, which the materializer has always supported (it walks the field
    // graph and allocates a shell per distinct id). So the pin only bites when
    // this allocation cannot be described at all.
    // Engagement census. These two gates are the ONLY thing
    // `CRATONVM_SCALAR_DEOPT` changes, so they are the only place that can say
    // whether the flag reached a workload — see
    // `cratonvm_types::scalar_deopt_census`. Counted once per allocation
    // (`descriptor_decided`), not once per gate: an allocation can be both
    // pinned by an earlier round and named by a snapshot, and counting it twice
    // would inflate an engagement number a soak is about to reason from.
    let mut descriptor_rescued = false;
    let mut descriptor_blocked = false;
    if descriptor_pinned.contains(&new_node) {
        if deopt_descriptor_available
            && virtual_object_info_for(ir_graph, reverse_map, info).is_some()
        {
            descriptor_rescued = true;
        } else {
            descriptor_blocked = true;
            elide_alloc = false;
        }
    }
    if stores.iter().any(|&s| ea_snapshot_names(ir_graph, s)) {
        // A snapshot naming a `Store` is already malformed — a store produces no
        // value a frame can be rebuilt from — but it is not this pass's business
        // to silently retarget it.
        elide_alloc = false;
    }
    // Only a snapshot a deopt can CONSULT pins the allocation (#49). The
    // candidate's own allocation, stores and loads are retired by this very
    // plan, so a snapshot that is consultable only because of one of them is
    // not. `ir_prune_unconsumable_snapshots` drops the rest after the plans are
    // applied, so no deopt point is ever built from one that names a dead node.
    //
    // The rule for one that IS consultable: keep the allocation unless a
    // virtual-object recipe will exist, which is exactly when precise resume is
    // on (`scalar_deopt_descriptor_available`). Never "elide and fall back to
    // re-running the method".
    let own_victims: std::collections::HashSet<ir::NodeId> = std::iter::once(new_node)
        .chain(stores.iter().copied())
        .chain(load_plans.iter().map(|&(l, _)| l))
        .collect();
    if ea_consumable_snapshot_names(ir_graph, new_node, &own_victims) {
        if deopt_descriptor_available
            && virtual_object_info_for(ir_graph, reverse_map, info).is_some()
        {
            descriptor_rescued = true;
        } else {
            descriptor_blocked = true;
            elide_alloc = false;
        }
    }
    // Round 13 wave 6 (lane chain2): a recipe does not rescue an allocation a
    // REQUIRED chain names while chains cannot carry recipes; see
    // `ea_required_chain_names`.
    if elide_alloc && ea_required_chain_names(ir_graph, new_node) {
        descriptor_blocked = true;
        elide_alloc = false;
    }
    // BLOCKED wins over RESCUED: an allocation that hit both gates and failed
    // either one is kept, so reporting it as engagement would be a lie in the
    // direction that flatters the flag.
    // (Not counted on the map builder's dry run: the applier counts this same
    // decision.)
    if note_census {
        if descriptor_blocked {
            cratonvm_types::scalar_deopt_census::note_blocked();
        } else if descriptor_rescued {
            cratonvm_types::scalar_deopt_census::note_rescued();
        }
    }
    if !ea_splice_feasible(ir_graph, new_node)
        || stores.iter().any(|&s| !ea_splice_feasible(ir_graph, s))
    {
        elide_alloc = false;
    }
    // Nothing outside the object's own (about to be killed) nodes may still read
    // the allocation or one of its stores as a VALUE. EA's escape rule should
    // already guarantee that; this is the belt-and-braces check that keeps a
    // bridge gap from turning into a live use of a removed node.
    if elide_alloc {
        'outer: for (idx, n) in ir_graph.nodes.iter().enumerate() {
            let id = idx as ir::NodeId;
            if n.op == ir::Op::Dead
                || id == new_node
                || stores.contains(&id)
                || load_plans.iter().any(|&(l, _)| l == id)
            {
                continue;
            }
            for (i, &inp) in n.inputs.iter().enumerate() {
                let names_victim = inp == new_node || stores.contains(&inp);
                if names_victim && !ea_is_memory_token_slot(n, i) {
                    elide_alloc = false;
                    break 'outer;
                }
            }
        }
    }

    if load_plans.is_empty() && !elide_alloc {
        return None; // nothing left to do for this object
    }
    Some(EaScalarPlan {
        loads: load_plans,
        new_node,
        stores,
        elide_alloc,
    })
}

/// Apply escape analysis results back onto the IR graph.
///
/// Maps EA node IDs back to IR node IDs using the reverse of `id_map`, then
/// performs scalar replacement (forward each field load to the stored value,
/// kill the stores and the allocation) and lock elision, by marking nodes as
/// `ir::Op::Dead`.
///
/// It runs after `ir_optimize::optimize` and is the last mutator before
/// `ir_schedule::schedule` / `ir_lower::lower_inner`, so the two invariants it
/// used to break are ones nothing downstream repairs:
///
/// * **the memory-token chain** — every node killed here is first *spliced* out
///   of the chain (consumers naming it as a token are rewired to its own
///   incoming token), exactly as `ir_optimize::eliminate_dead_stores` does. It
///   previously killed loads, stores and the `Op::New` with a plain
///   `op = Op::Dead`, leaving the next memory operation's token slot pointing at
///   a removed node — and, worse, rewrote token slots naming a forwarded load to
///   that load's *data* replacement, so an ordering edge became a data edge.
/// * **safepoint snapshots** — `graph.safepoints` were never touched. A slot
///   naming a forwarded load now follows the value (it used to keep naming the
///   killed load ⇒ `FrameValue::Undefined` ⇒ a wrong value at deopt), and the
///   allocation is only elided when a slot naming it can still be described.
///   See `plan_scalar_replacement` for that rule.
///
/// Test-only (round 11 wave 4): production calls [`apply_ea_to_ir_planned`]
/// directly; this wrapper serves `jit/src/tests.rs` and this module's tests.
#[cfg(test)]
pub(crate) fn apply_ea_to_ir(
    ir_graph: &mut ir::Graph,
    id_map: &[escape_analysis::NodeId],
    ea_result: &escape_analysis::EscapeAnalysisResult,
) {
    apply_ea_to_ir_pinned(
        ir_graph,
        id_map,
        ea_result,
        &std::collections::HashSet::new(),
    );
}

/// `apply_ea_to_ir` with the set of allocations an earlier round's deopt
/// descriptor already names as a field value, and a count of the plans it
/// applied.
///
/// The count is the iteration's stopping condition: a round that applies
/// nothing has reached the fixed point and the next round would see the same
/// graph.
///
/// Test-only since round 11 wave 7: it plans every candidate itself, which
/// is [`apply_ea_to_ir_planned`] given `None`. `jit/src/tests.rs` and this
/// module's tests call it.
#[cfg(test)]
pub(crate) fn apply_ea_to_ir_pinned(
    ir_graph: &mut ir::Graph,
    id_map: &[escape_analysis::NodeId],
    ea_result: &escape_analysis::EscapeAnalysisResult,
    descriptor_pinned: &std::collections::HashSet<ir::NodeId>,
) -> usize {
    apply_ea_to_ir_planned(ir_graph, id_map, ea_result, descriptor_pinned, None)
}

/// `apply_ea_to_ir_pinned` with the scalar-replacement plans optionally
/// decided already: the production entry (round 11 wave 7,
/// `r11-iropt-ea-compile-time-work-per-round`).
///
/// `planned` is `Some` exactly when the caller ran
/// [`build_scalar_replacement_map_with_plans`] on this same graph, with this
/// same `ea_result`, this round: its plans are used as they are and Phase 1
/// below does not re-plan. Each plan used to be computed twice per round (the
/// builder's dry run, then here), and each run scans the whole graph
/// (`ea_splice_feasible`, the value-use check). `None` plans here, as before.
///
/// Handing the builder's plans over is sound although the caller pins this
/// round's recipe values between the two calls: the argument is written out on
/// `r11-iropt-ea-compile-time-work-per-round` (wave 6). The pins can only make
/// a re-plan forward LESS in one corner, and a handed-over plan also matches
/// the recipe the builder wrote exactly, which a re-plan did not promise.
pub(crate) fn apply_ea_to_ir_planned(
    ir_graph: &mut ir::Graph,
    id_map: &[escape_analysis::NodeId],
    ea_result: &escape_analysis::EscapeAnalysisResult,
    descriptor_pinned: &std::collections::HashSet<ir::NodeId>,
    planned: Option<Vec<EaScalarPlan>>,
) -> usize {
    // Build reverse map: EA NodeId → IR NodeId
    let mut reverse_map: HashMap<escape_analysis::NodeId, ir::NodeId> = HashMap::new();
    for (ir_id, &ea_id) in id_map.iter().enumerate() {
        if ea_id != usize::MAX {
            reverse_map.insert(ea_id, ir_id as ir::NodeId);
        }
    }

    // Will the lowerer be handed a `ScalarReplacementMap` at all? This MUST be
    // the same predicate the caller uses to decide whether to call
    // `build_scalar_replacement_map_with_plans` (the EA block in `ir_tier`); if
    // the two ever disagree, an elided allocation loses its deopt descriptor.
    let deopt_descriptor_available = scalar_deopt_descriptor_available();

    // ── Phase 1: plan (no mutation) ──────────────────────────────────
    //
    // The two counters below are the pair, for the same reason lock coarsening
    // has one: either number alone is unreadable. A zero `CANDIDATES` is a
    // statement about escape analysis -- it found nothing to replace. A
    // non-zero `CANDIDATES` with a zero `PLANS` is a statement about this
    // planner, and sends the reader to the refusal arms rather than to the
    // analysis. `Transform::ScalarReplacement` (bumped in phase 2) is the
    // third number, the allocations actually elided.
    SCALAR_REPLACEMENT_CANDIDATES.fetch_add(
        ea_result.scalar_replaceable.len() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    let mut plans: Vec<EaScalarPlan> = match planned {
        Some(plans) => plans,
        None => {
            let mut plans = Vec::new();
            for info in &ea_result.scalar_replaceable {
                if let Some(plan) = plan_scalar_replacement(
                    ir_graph,
                    &reverse_map,
                    info,
                    deopt_descriptor_available,
                    descriptor_pinned,
                    true,
                ) {
                    plans.push(plan);
                }
            }
            plans
        }
    };
    SCALAR_REPLACEMENT_PLANS.fetch_add(plans.len() as u64, std::sync::atomic::Ordering::Relaxed);

    // Materialise ONE shared zero default PER VALUE TYPE for every never-stored
    // slot load. One shared `Const(0)` typed `Int` was right as long as the only
    // candidates were objects whose admitted `<init>` shapes read int-shaped
    // fields; it is not right in general, and array scalar replacement makes
    // the general case reachable -- a never-stored `long[]` slot forwarded to an
    // `Int`-typed node is a 32-bit value where a 64-bit one belongs.
    //
    // The load node's own `ty` is the authority: it is what the consumer of the
    // load expects, and forwarding must produce exactly that. `Long` uses
    // `Op::Const`, `Float`/`Double` the FP constant node (`Op::ConstF`, whose
    // payload is raw bits -- and +0.0's bits are 0 for both widths).
    //
    // A type this does not know how to spell a zero for (`Ref`, and the
    // non-value types) leaves the load alone: the plan is dropped rather than
    // answered with a guess. That is why `Op::NewArray` candidates are
    // restricted to primitive element types -- see `MAX_SCALAR_ARRAY_LEN`'s
    // neighbourhood in `escape_analysis`.
    let mut zero_defaults: HashMap<ir::IrType, ir::NodeId> = HashMap::new();
    for plan in plans.iter_mut() {
        for (load, value) in plan.loads.iter_mut() {
            if value.is_some() {
                continue;
            }
            let ty = match ir_graph.node_opt(*load).map(|n| n.ty) {
                Some(t) => t,
                None => continue,
            };
            let z = match zero_defaults.get(&ty) {
                Some(&z) => Some(z),
                None => {
                    let made = match ty {
                        ir::IrType::Int => {
                            Some(ir_graph.add(ir::Op::Const(0), ir::IrType::Int, vec![], None))
                        }
                        ir::IrType::Long => {
                            Some(ir_graph.add(ir::Op::Const(0), ir::IrType::Long, vec![], None))
                        }
                        ir::IrType::Float => {
                            Some(ir_graph.add(ir::Op::ConstF(0), ir::IrType::Float, vec![], None))
                        }
                        ir::IrType::Double => {
                            Some(ir_graph.add(ir::Op::ConstF(0), ir::IrType::Double, vec![], None))
                        }
                        _ => None,
                    };
                    if let Some(z) = made {
                        zero_defaults.insert(ty, z);
                    }
                    made
                }
            };
            *value = z;
        }
    }
    // A load whose zero default could not be spelled leaves its plan
    // unanswerable; drop the whole plan rather than apply it in part.
    plans.retain(|p| p.loads.iter().all(|(_, v)| v.is_some()));

    // ── Phase 2: collect victims, then apply in ascending node id ────
    //
    // Ascending order matters for the splice: a victim's incoming token is an
    // earlier (smaller-id) node, so by the time we reach a victim, every token
    // it could name has already been rewritten or recorded in `spliced`.
    let mut victims: Vec<(ir::NodeId, EaVictimKind)> = Vec::new();
    for plan in &plans {
        for &(load, value) in &plan.loads {
            if let Some(v) = value {
                victims.push((load, EaVictimKind::Forwarded(v)));
            }
        }
        if plan.elide_alloc {
            // The census note belongs HERE, not on
            // `escape_analysis::apply_scalar_replacement`, which is where it
            // was until 2026-09-16. That function has no production caller at
            // all -- every call site is one of its own unit tests -- because
            // the shipping path plans on the EA graph and applies on the IR
            // graph through `plan_scalar_replacement` and the victim splice
            // below. A counter on the unreachable applier reads zero whether
            // or not scalar replacement is happening, which is worse than no
            // counter: it answers the question wrongly instead of not at all.
            crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarReplacement);
            for &store in &plan.stores {
                victims.push((store, EaVictimKind::Eliminated));
            }
            victims.push((plan.new_node, EaVictimKind::Eliminated));
        }
    }

    // ── Lock elision: ALL-OR-NOTHING PER OBJECT ──────────────────────
    //
    // This reads `ea_result.lock_elisions` (grouped per object), NOT the flat
    // `ea_result.elide_locks`. The two carry the same monitors, but only the
    // grouped view is *correct*: a monitor sequence is balanced as a whole, so
    // dropping one node out of it leaves a `monitorexit` whose `monitorenter`
    // was elided (`IllegalMonitorStateException`) or a monitor held past the end
    // of the frame. The previous per-node loop applied exactly that partial
    // filter — see `docs/jit/lock-elimination.md` §4 and §6.1.
    //
    // Each of the three refusals below is per NODE but decides the whole GROUP:
    //
    //   * a safepoint snapshot slot names the monitor —
    //     `deopt::EliminationCause::ElidedLock` has no representation in a
    //     `SafepointSnapshot`, so the elision would be undescribable;
    //   * its memory-token chain cannot be spliced;
    //   * some non-token input still reads its value.
    //
    // `escape_analysis_from_ir` bridges `ir::Op::MonitorEnter`/`MonitorExit`,
    // so a graph whose builder emitted a monitor pair reaches this loop with
    // real plans. The loop is all-or-nothing per object on purpose: eliding
    // one half of a pair is what turns a latent hazard into a thrown
    // `IllegalMonitorStateException`.
    for plan in &ea_result.lock_elisions {
        let mut group: Vec<ir::NodeId> = Vec::with_capacity(plan.monitors.len());
        let mut ok = true;
        for &ea_lock in &plan.monitors {
            let ir_lock = match reverse_map.get(&ea_lock) {
                Some(&id) => id,
                // A monitor with no IR counterpart cannot be killed, and the
                // group is only sound applied whole — so refuse the object.
                None => {
                    ok = false;
                    break;
                }
            };
            if ir_graph.node_opt(ir_lock).is_none()
                || ea_snapshot_names(ir_graph, ir_lock)
                || !ea_splice_feasible(ir_graph, ir_lock)
            {
                ok = false;
                break;
            }
            let value_used = ir_graph.nodes.iter().any(|n| {
                n.op != ir::Op::Dead
                    && n.inputs
                        .iter()
                        .enumerate()
                        .any(|(i, &inp)| inp == ir_lock && !ea_is_memory_token_slot(n, i))
            });
            if value_used {
                ok = false;
                break;
            }
            group.push(ir_lock);
        }
        if ok {
            victims.extend(group.into_iter().map(|id| (id, EaVictimKind::Eliminated)));
        }
    }

    // How many NODES this call actually retires. The iteration in
    // `try_compile_inner` stops when a round retires nothing -- which is the
    // honest fixed-point signal. Counting PLANS is not: a plan whose
    // `elide_alloc` was refused and whose loads were already forwarded by an
    // earlier round retires nothing, and counting it would spin the loop to its
    // bound on a graph that stopped changing.
    let applied = victims.len();
    victims.sort_by_key(|&(id, _)| id);
    victims.dedup_by_key(|&mut (id, _)| id);

    // `spliced[v]` = the live memory token `v` handed on when it was killed;
    // `forwarded[v]` = the value a killed load resolved to. Both are followed
    // transitively so a chain of kills never lands on an already-dead node.
    let mut spliced: HashMap<ir::NodeId, ir::NodeId> = HashMap::new();
    let mut forwarded: HashMap<ir::NodeId, ir::NodeId> = HashMap::new();
    let node_count = ir_graph.nodes.len();

    // Who names each victim, built once (round 11 wave 4, lane mid,
    // `r11-iropt-ea-compile-time-work-per-round`). The loop below used to sweep
    // every node and every snapshot slot per victim, O(victims × (nodes +
    // slots)). A node or slot can only name a victim if it did so here, or if
    // the loop itself rewired it to one, and every such rewiring is recorded
    // as it happens. So each victim visits exactly the references the full
    // sweep would have found, in a different order. The rewrite of one
    // reference does not depend on any other, so the result is the same.
    // Duplicate entries are harmless: a second visit finds nothing still
    // naming the victim.
    let victim_ids: std::collections::HashSet<ir::NodeId> =
        victims.iter().map(|&(id, _)| id).collect();
    let mut users: HashMap<ir::NodeId, Vec<ir::NodeId>> = HashMap::new();
    for (id, n) in ir_graph.nodes.iter().enumerate() {
        if n.op == ir::Op::Dead {
            continue;
        }
        for &inp in n.inputs.iter() {
            if victim_ids.contains(&inp) {
                users.entry(inp).or_default().push(id as ir::NodeId);
            }
        }
    }
    let mut slot_refs: HashMap<ir::NodeId, Vec<(usize, ir::SafepointSlotKind, usize)>> =
        HashMap::new();
    for (si, sp) in ir_graph.safepoints.iter().enumerate() {
        let halves = [
            (ir::SafepointSlotKind::Local, &sp.locals),
            (ir::SafepointSlotKind::Stack, &sp.stack),
            (ir::SafepointSlotKind::Monitor, &sp.monitors),
        ];
        for (slot_kind, half) in halves {
            for (k, &slot) in half.iter().enumerate() {
                if victim_ids.contains(&slot) {
                    slot_refs.entry(slot).or_default().push((si, slot_kind, k));
                }
            }
        }
    }

    for (victim, kind) in victims {
        // The token this victim hands on, followed through earlier splices so a
        // run of chained kills closes onto a LIVE node instead of relocating the
        // break one link along.
        let mut token = ir_graph
            .node_opt(victim)
            .and_then(|n| ea_memory_token_slot(n).and_then(|s| n.input_opt(s)));
        let mut hops = 0usize;
        while let Some(t) = token {
            if ir_graph.node_opt(t).is_some_and(|n| n.op != ir::Op::Dead) {
                break;
            }
            token = spliced.get(&t).copied();
            hops += 1;
            if hops > node_count {
                token = None;
                break;
            }
        }
        // Likewise for the replacement value: a load can resolve to a value that
        // is itself a load this pass retires.
        let kind = match kind {
            EaVictimKind::Forwarded(v) => {
                let mut r = v;
                let mut steps = 0usize;
                while let Some(&next) = forwarded.get(&r) {
                    r = next;
                    steps += 1;
                    if steps > node_count {
                        break;
                    }
                }
                EaVictimKind::Forwarded(r)
            }
            k => k,
        };

        let mut renamed: Vec<(ir::NodeId, ir::NodeId)> = Vec::new();
        for u in users.remove(&victim).unwrap_or_default() {
            let Some(n) = ir_graph.nodes.get_mut(u as usize) else {
                continue;
            };
            if n.op == ir::Op::Dead {
                continue;
            }
            let mem_phi = matches!(n.op, ir::Op::Phi) && n.ty == ir::IrType::Memory;
            let token_slot = ea_memory_token_slot(n);
            for (i, inp) in n.inputs.iter_mut().enumerate() {
                if *inp != victim {
                    continue;
                }
                let is_token = if mem_phi {
                    i >= 1
                } else {
                    token_slot == Some(i)
                };
                let new = if is_token {
                    // An ordering edge follows the CHAIN, never the data
                    // replacement — the bug the old blanket rewrite had.
                    token
                } else if let EaVictimKind::Forwarded(v) = kind {
                    Some(v)
                } else {
                    None
                };
                if let Some(t) = new {
                    *inp = t;
                    if victim_ids.contains(&t) {
                        renamed.push((t, u));
                    }
                }
            }
        }
        for (t, u) in renamed {
            users.entry(t).or_default().push(u);
        }
        // Safepoint snapshots are references too: a slot naming a forwarded load
        // must follow the value, exactly as `ir::Graph::replace_all_uses` does.
        // A slot naming an eliminated `Op::New` is deliberately LEFT in place —
        // that slot is the "materialise this virtual object" descriptor, and
        // planning has already refused the elision when no descriptor will exist.
        //
        // The install goes through `Graph::set_safepoint_slot`, never a write
        // through the public `safepoints` field: installing a node id is the
        // one snapshot edit the def-use lists cannot notice, and this used to
        // be correct only because the `inputs` writes above happened to bump
        // the edge epoch first.
        if let EaVictimKind::Forwarded(v) = kind {
            forwarded.insert(victim, v);
            for (si, slot_kind, k) in slot_refs.remove(&victim).unwrap_or_default() {
                let names_victim = ir_graph.safepoints.get(si).and_then(|sp| match slot_kind {
                    ir::SafepointSlotKind::Local => sp.locals.get(k).copied(),
                    ir::SafepointSlotKind::Stack => sp.stack.get(k).copied(),
                    ir::SafepointSlotKind::Monitor => sp.monitors.get(k).copied(),
                }) == Some(victim);
                if names_victim
                    && ir_graph.set_safepoint_slot(si, slot_kind, k, v)
                    && victim_ids.contains(&v)
                {
                    slot_refs.entry(v).or_default().push((si, slot_kind, k));
                }
            }
        }
        if let Some(t) = token {
            spliced.insert(victim, t);
        }
        let idx = victim as usize;
        ir_graph.nodes[idx].op = ir::Op::Dead;
        ir_graph.nodes[idx].inputs.clear();
    }

    // ── Lock COARSENING, after every elision victim is dead ──────────
    //
    // `escape_analysis::find_lock_coarsening_plans` has produced these since it
    // landed and **nothing has ever applied one**: `apply_lock_coarsening` was
    // written, re-verifying and unit-tested, with its only callers in
    // `escape_analysis.rs`'s own test module. `escape_analysis.rs`'s header
    // said so outright — "`lock_coarsening` | offered, no consumer yet" — and
    // this is the consumer.
    //
    // # Why AFTER the victims, and not beside the elision loop
    //
    // Coarsening merges two adjacent monitor regions on one object by deleting
    // the inner `monitorexit`/`monitorenter` pair. Elision may already have
    // removed some of those nodes, and the two transforms disagreeing is the
    // shape that produces an unbalanced monitor sequence — a held lock at frame
    // exit, or an `IllegalMonitorStateException` from an exit whose enter is
    // gone. `apply_lock_coarsening` re-verifies all four nodes against the
    // graph as it now stands and drops the plan whole if the structure it
    // proved no longer exists, so running it on the POST-elision graph is what
    // makes the re-verification mean something. Running it first would verify
    // against a graph that is about to change.
    //
    // # Default OFF, and this is not a soak
    //
    // `CRATONVM_JIT_LOCK_COARSEN=1` opts in. Wiring a transform is not evidence
    // that it should be on: a coarsened region holds a lock for longer, which
    // is a throughput trade and not a free win, and nothing here has measured
    // one. What the wiring buys today is that the capability is REACHABLE and
    // countable instead of being dead code that reads as a feature.
    //
    // The count is deliberately not folded into `applied`. `applied` is the
    // fixed-point signal `try_compile_inner` iterates on — "did this round
    // retire a node" — and a coarsening kills nodes the same way, so it is
    // added; but it is also counted separately so a census can tell the two
    // apart. See `lock_coarsenings_applied`.
    let mut coarsened = 0usize;
    if lock_coarsening_enabled() && !ea_result.lock_coarsening.is_empty() {
        LOCK_COARSENING_PLANS_OFFERED.fetch_add(
            ea_result.lock_coarsening.len() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        for plan in &ea_result.lock_coarsening {
            // `escape_analysis::apply_lock_coarsening` operates on the EA-side
            // `Graph`, which is a DIFFERENT type from `ir::Graph` — the two
            // have similar names and distinct node numbering, bridged by
            // `reverse_map`. So this does not call the EA applier; it performs
            // the same deletion on the IR graph, through the same translation
            // and the same refusals the elision loop above uses. (The EA
            // applier keeps its unit tests and its own re-verification; it is
            // the EA-side form of this.)
            //
            // ALL-OR-NOTHING, for the reason the elision loop states at
            // length: a monitor sequence is balanced as a whole, so deleting
            // the inner `exit` without the inner `enter` leaves a lock held
            // past the end of the frame, and the reverse throws
            // `IllegalMonitorStateException`. Both or neither.
            let inner = [plan.removed_exit(), plan.removed_enter()];
            // Round 14 wave 3 (lane fixup, SS-4's EA twin): both halves of a
            // synchronized splice's window or neither; `ir_lower` pairs a
            // window only with another window and refuses the method's
            // compile for a half merged with a bytecode monitor op.
            if ea_coarsening_splits_a_sync_window(ir_graph, &reverse_map, inner) {
                continue;
            }
            let outer = [plan.surviving_enter(), plan.surviving_exit()];
            // The survivors must still BE there. Elision runs first and may
            // have removed the outer pair; deleting the inner pair then
            // produces an unbalanced sequence this pass did not create, which
            // `apply_lock_coarsening`'s own doc names as the case to refuse.
            let outer_ok = outer.iter().all(|ea| {
                reverse_map
                    .get(ea)
                    .and_then(|&ir| ir_graph.node_opt(ir))
                    .is_some_and(|n| matches!(n.op, ir::Op::MonitorEnter | ir::Op::MonitorExit))
            });
            if !outer_ok {
                continue;
            }
            let mut group: Vec<ir::NodeId> = Vec::with_capacity(inner.len());
            let mut ok = true;
            for ea_lock in inner {
                let Some(&ir_lock) = reverse_map.get(&ea_lock) else {
                    ok = false;
                    break;
                };
                // The same three refusals the elision loop applies, and for the
                // same reasons: an undescribable safepoint slot, an
                // unspliceable memory token, or a surviving non-token reader.
                if ir_graph.node_opt(ir_lock).is_none()
                    || ea_snapshot_names(ir_graph, ir_lock)
                    || !ea_splice_feasible(ir_graph, ir_lock)
                {
                    ok = false;
                    break;
                }
                let value_used = ir_graph.nodes.iter().any(|n| {
                    n.op != ir::Op::Dead
                        && n.inputs
                            .iter()
                            .enumerate()
                            .any(|(i, &inp)| inp == ir_lock && !ea_is_memory_token_slot(n, i))
                });
                if value_used {
                    ok = false;
                    break;
                }
                group.push(ir_lock);
            }
            if !ok || group.len() != inner.len() {
                continue;
            }
            // Splice each out of the memory chain, then kill it — the same two
            // steps the victim loop above performs, done here because the
            // victim list was consumed before this point.
            //
            // Through `replace_all_uses`, which reaches every slot naming the
            // node (round 11 wave 9). The hand-written rewrite this replaces
            // asked `ea_memory_token_slot`, which answers `None` for a memory
            // φ, so a join whose incoming token was the removed `monitorenter`
            // kept naming a dead node. `value_used` above already proved every
            // remaining reference is a token slot (a memory φ's value inputs
            // are all tokens), so rewriting all of them is exactly the splice.
            // The token is read AFTER the previous kill rewired it: the removed
            // exit's users include the removed enter.
            for ir_lock in group {
                let token = ir_graph
                    .node_opt(ir_lock)
                    .and_then(|n| ea_memory_token_slot(n).and_then(|s| n.input_opt(s)));
                if let Some(t) = token {
                    ir_graph.replace_all_uses(ir_lock, t);
                }
                ir_graph.kill(ir_lock);
                coarsened += 1;
            }
        }
        if coarsened > 0 {
            LOCK_COARSENINGS_APPLIED
                .fetch_add(coarsened as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }
    applied + coarsened
}

/// Would deleting the inner pair `[exit, enter]` of a coarsening plan remove
/// one half of a synchronized splice's window (`Graph::sync_splice_windows`)
/// and a monitor op outside every window? `ir_lower::sync_splice_window_refusal`
/// can re-pair two merged windows but not a window with a bytecode monitor op,
/// so such a merge only ever cost the method its optimizing compile. The
/// IR-side twin is `ir_optimize::coarsenable_monitor_pair` (SS-4).
fn ea_coarsening_splits_a_sync_window(
    ir_graph: &ir::Graph,
    reverse_map: &HashMap<escape_analysis::NodeId, ir::NodeId>,
    inner: [escape_analysis::NodeId; 2],
) -> bool {
    let in_window = |ea: escape_analysis::NodeId| {
        reverse_map.get(&ea).is_some_and(|&ir| {
            ir_graph
                .sync_splice_windows
                .iter()
                .any(|w| w.enter == ir || w.exit == ir)
        })
    };
    in_window(inner[0]) != in_window(inner[1])
}

/// Apply `escape_analysis`'s lock-coarsening plans. `CRATONVM_JIT_LOCK_COARSEN=1`.
///
/// Default OFF. The plans have been computed on every EA run since the pass
/// landed and never applied; this is the switch that makes them reachable. It
/// is not on by default because coarsening holds a lock for longer, which is a
/// throughput trade nobody here has measured — see the call site.
///
/// `escape_analysis::analyze_escapes` also reads it: with the switch off it
/// does not compute the plans at all (round 11 wave 4).
pub(crate) fn lock_coarsening_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_LOCK_COARSEN"))
}

/// Coarsening plans offered by escape analysis, and plans actually applied.
/// Scalar-replacement candidates escape analysis OFFERED, process-wide.
///
/// Paired with [`SCALAR_REPLACEMENT_PLANS`]; see the comment at the phase-1
/// loop for why one number without the other cannot be read. Added 2026-09-16
/// after a probe built specifically to be scalar-replaceable reported zero
/// replacements and nothing in the tree could say which half was responsible.
static SCALAR_REPLACEMENT_CANDIDATES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Candidates that survived `plan_scalar_replacement`'s refusals.
static SCALAR_REPLACEMENT_PLANS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(candidates, plans)` for the scalar-replacement funnel.
pub fn scalar_replacement_census() -> (u64, u64) {
    (
        SCALAR_REPLACEMENT_CANDIDATES.load(std::sync::atomic::Ordering::Relaxed),
        SCALAR_REPLACEMENT_PLANS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

static LOCK_COARSENING_PLANS_OFFERED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static LOCK_COARSENINGS_APPLIED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(offered, applied)` lock-coarsening plans this process has seen.
///
/// Both numbers, because either alone is unreadable. A zero `offered` means
/// escape analysis found no adjacent monitor regions — a statement about the
/// workload. A non-zero `offered` with a zero `applied` means every plan was
/// refused by `apply_lock_coarsening`'s re-verification, which is a statement
/// about the interaction with lock elision and is the reading that would send
/// someone to look at the order these two run in.
pub fn lock_coarsening_census() -> (u64, u64) {
    (
        LOCK_COARSENING_PLANS_OFFERED.load(std::sync::atomic::Ordering::Relaxed),
        LOCK_COARSENINGS_APPLIED.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Build the [`ir_lower::ScalarReplacementMap`] that drives guard-surviving
/// scalar replacement (Front 3.2): for each scalar-replaced object, capture the
/// metadata the deopt producer needs to emit a `FrameValue::VirtualObject`
/// (`class_id`/`num_fields`/per-field value nodes/eliminated stores), keyed by
/// the IR `NodeId` of the eliminated `Op::New`. Sourced entirely from
/// `ea_result` + `id_map`, so it is order-independent w.r.t. `apply_ea_to_ir`
/// (which only mutates the graph). An object whose `Op::New` or any *stored*
/// field value cannot be mapped to an IR node is **omitted** — the producer then
/// leaves its slot `Undefined` (safe whole-method re-run) rather than emit a
/// partial/garbage object. Only called when
/// `scalar_deopt_descriptor_available()`.
///
/// Also omitted: any candidate `apply_ea_to_ir` will *refuse* to elide. Those
/// keep a real allocation, and a `VirtualObject` recipe for a live object would
/// have a deopt materialize a second one — see the loop body.
///
/// Test-only since round 11 wave 7: production calls
/// [`build_scalar_replacement_map_with_plans`], which also returns the plans
/// it decided. This one plans without counting the census, as the dry run
/// always did, because its callers then let the applier plan again.
#[cfg(test)]
pub(crate) fn build_scalar_replacement_map(
    ir_graph: &ir::Graph,
    id_map: &[escape_analysis::NodeId],
    ea_result: &escape_analysis::EscapeAnalysisResult,
    descriptor_pinned: &std::collections::HashSet<ir::NodeId>,
) -> ir_lower::ScalarReplacementMap {
    scalar_replacement_map_and_plans(ir_graph, id_map, ea_result, descriptor_pinned, false).0
}

/// The production form of `build_scalar_replacement_map` (round 11 wave 7):
/// the same map, plus every plan it decided, in candidate order, for
/// [`apply_ea_to_ir_planned`] to apply as they are. Each candidate is planned
/// once per round instead of twice, and counted in the scalar-deopt census
/// here, since the applier no longer plans it.
pub(crate) fn build_scalar_replacement_map_with_plans(
    ir_graph: &ir::Graph,
    id_map: &[escape_analysis::NodeId],
    ea_result: &escape_analysis::EscapeAnalysisResult,
    descriptor_pinned: &std::collections::HashSet<ir::NodeId>,
) -> (ir_lower::ScalarReplacementMap, Vec<EaScalarPlan>) {
    scalar_replacement_map_and_plans(ir_graph, id_map, ea_result, descriptor_pinned, true)
}

/// The body of the two map builders above. `note_census` is passed through to
/// `plan_scalar_replacement`.
fn scalar_replacement_map_and_plans(
    ir_graph: &ir::Graph,
    id_map: &[escape_analysis::NodeId],
    ea_result: &escape_analysis::EscapeAnalysisResult,
    descriptor_pinned: &std::collections::HashSet<ir::NodeId>,
    note_census: bool,
) -> (ir_lower::ScalarReplacementMap, Vec<EaScalarPlan>) {
    // Every plan decided, `Some` ones only, in `scalar_replaceable` order: the
    // list the applier's own Phase 1 would build.
    let mut plans: Vec<EaScalarPlan> = Vec::new();
    let mut reverse_map: HashMap<escape_analysis::NodeId, ir::NodeId> = HashMap::new();
    for (ir_id, &ea_id) in id_map.iter().enumerate() {
        if ea_id != usize::MAX {
            reverse_map.insert(ea_id, ir_id as ir::NodeId);
        }
    }
    // The predicate `apply_ea_to_ir` will compute for itself. Recomputed rather
    // than passed in so the two cannot drift apart at the call site.
    let deopt_descriptor_available = scalar_deopt_descriptor_available();
    let mut objects: HashMap<ir::NodeId, ir_lower::VirtualObjectInfo> = HashMap::new();
    // IR load → the value `apply_ea_to_ir` will forward it to, for every load
    // this round retires. See the rewrite below.
    let mut forwarded: HashMap<ir::NodeId, ir::NodeId> = HashMap::new();
    // Loads this round retires to a zero default. `apply_ea_to_ir_pinned`
    // materialises that `Const` only later, so they have no entry in
    // `forwarded`; a recipe field naming one describes a zero (`None`).
    let mut zero_loads: std::collections::HashSet<ir::NodeId> = std::collections::HashSet::new();
    let mut elided_infos: Vec<&escape_analysis::ScalarReplacementInfo> = Vec::new();
    for info in &ea_result.scalar_replaceable {
        // Only objects `apply_ea_to_ir` will actually ELIDE may be described. A
        // candidate it refuses (its allocation survives — see
        // `plan_scalar_replacement`) must NOT get a `VirtualObject` recipe: the
        // JIT frame holds the real reference, and materializing the recipe at a
        // deopt would create a SECOND object with the same field values, so the
        // resumed interpreter frame would hold a different identity than the
        // compiled frame did. `plan_scalar_replacement` is pure and runs on this
        // same pre-apply graph, so the two answers are the same answer.
        let Some(plan) = plan_scalar_replacement(
            ir_graph,
            &reverse_map,
            info,
            deopt_descriptor_available,
            descriptor_pinned,
            note_census,
        ) else {
            continue;
        };
        // `apply_ea_to_ir_planned` drops a whole plan when one of its zero
        // defaults has a type it cannot spell (`plans.retain`); such a plan
        // forwards nothing, so none of its loads may be rewritten here either.
        let applied = plan.loads.iter().all(|&(l, v)| {
            v.is_some()
                || ir_graph.node_opt(l).is_some_and(|n| {
                    matches!(
                        n.ty,
                        ir::IrType::Int | ir::IrType::Long | ir::IrType::Float | ir::IrType::Double
                    )
                })
        });
        if applied {
            for &(l, v) in &plan.loads {
                match v {
                    Some(v) => {
                        forwarded.insert(l, v);
                    }
                    None => {
                        zero_loads.insert(l);
                    }
                }
            }
        }
        // A dropped plan elides nothing either: its allocation survives, and a
        // recipe for a surviving object is exactly the second-identity
        // materialization the comment above forbids (round 11 — this used to
        // test `plan.elide_alloc` alone).
        if applied && plan.elide_alloc {
            elided_infos.push(info);
        }
        plans.push(plan);
    }
    for info in elided_infos {
        if let Some((ir_new, mut vo)) = virtual_object_info_for(ir_graph, &reverse_map, info) {
            // A field whose recorded value is a LOAD this same round forwards
            // (`b.v = p.x` with `p` replaced too) would name a node the victim
            // splice is about to kill, and the lowerer answers a dead field
            // value with `MaterializationRequired`: the whole object becomes
            // undescribable, and `ir_artifact_names_an_undescribable_elided_object`
            // discards the IR artifact. Name the load's replacement instead
            // (r9-ea) — the value that load reads, which is what the field
            // holds. A chain ending in a zero-default load describes a zero
            // (`None`): that load is killed too, and naming it made the whole
            // object `MaterializationRequired` (round 11 wave 2). It also keeps
            // the caller from pinning a load this round retires, which
            // `plan_scalar_replacement` would then refuse to forward.
            for slot in vo.field_values.iter_mut() {
                if let Some(fv) = slot.as_mut() {
                    let mut hops = 0usize;
                    while let Some(&next) = forwarded.get(&*fv) {
                        *fv = next;
                        hops += 1;
                        if hops > forwarded.len() {
                            break;
                        }
                    }
                }
                if matches!(*slot, Some(v) if zero_loads.contains(&v)) {
                    *slot = None;
                }
            }
            // The per-store values get the same forwarding and zero-load
            // rewrite as the per-field values above.
            for entry in vo.store_fields.iter_mut() {
                if let Some((_, value)) = entry.as_mut() {
                    if let Some(v) = value.as_mut() {
                        let mut hops = 0usize;
                        while let Some(&next) = forwarded.get(&*v) {
                            *v = next;
                            hops += 1;
                            if hops > forwarded.len() {
                                break;
                            }
                        }
                    }
                    if matches!(*value, Some(v) if zero_loads.contains(&v)) {
                        *value = None;
                    }
                }
            }
            objects.insert(ir_new, vo);
        }
    }
    (ir_lower::ScalarReplacementMap { objects }, plans)
}

/// The [`ir_lower::VirtualObjectInfo`] describing one scalar-replacement
/// candidate, keyed by the IR `NodeId` of its `Op::New`, or `None` when the
/// object cannot be described to the deopt producer at all.
///
/// Split out of [`build_scalar_replacement_map_with_plans`] so [`plan_scalar_replacement`]
/// can ask the *same* question — "will this object have a virtual-object
/// descriptor?" — before it decides whether eliding the allocation is safe. Two
/// copies of this admission rule would mean an object the map omits could still
/// have its `Op::New` killed, and its snapshot slot would then resolve to
/// `FrameValue::Undefined` (a silent null).
///
/// MUST be called on the pre-`apply_ea_to_ir` graph: it reads the control
/// inputs of nodes that pass clears.
fn virtual_object_info_for(
    ir_graph: &ir::Graph,
    reverse_map: &HashMap<escape_analysis::NodeId, ir::NodeId>,
    info: &escape_analysis::ScalarReplacementInfo,
) -> Option<(ir::NodeId, ir_lower::VirtualObjectInfo)> {
    // Control input (slot 0) of a node, used to recover a node's block for the
    // dominance gate AFTER the node itself is marked `Op::Dead` (its inputs are
    // cleared then, but the captured control node stays live).
    let ctrl_of = |n: ir::NodeId| -> Option<ir::NodeId> {
        ir_graph
            .nodes
            .get(n as usize)
            .and_then(|node| node.inputs.first().copied())
            .filter(|&c| c != ir::NO_NODE)
    };
    let ir_new = *reverse_map.get(&info.alloc_node)?;
    // Control of the allocation — bail (omit) if it has none (hand-built /
    // malformed), so the producer can't emit without a dominance anchor.
    let new_ctrl = ctrl_of(ir_new)?;
    // Per-field IR value node. A `None` EA entry is a never-stored field
    // (zero default). A `Some(ea)` that fails to map back to an IR node means
    // we cannot reconstruct that field — omit the whole object (safe).
    let mut field_values: Vec<Option<ir::NodeId>> = Vec::with_capacity(info.field_values.len());
    for fv in &info.field_values {
        match fv {
            None => field_values.push(None),
            Some(ea) => field_values.push(Some(*reverse_map.get(ea)?)),
        }
    }
    // Capture each eliminated store's control node (its block) and the store's
    // own id (its position within that block — see
    // `ir_lower::Lowerer::eliminated_node_precedes_deopt`). A store with no
    // resolvable control omits the object (can't prove dominance).
    let mut store_ctrls: Vec<ir::NodeId> = Vec::with_capacity(info.eliminated_stores.len());
    let mut store_nodes: Vec<ir::NodeId> = Vec::with_capacity(info.eliminated_stores.len());
    // Which field each eliminated store writes, and with what value, so the
    // deopt producer can rebuild an object some of whose stores ran and some
    // did not (r11w5-lower-virtual-object-store-fields-patch). Data only here.
    let mut field_of_store: HashMap<escape_analysis::NodeId, (usize, escape_analysis::NodeId)> =
        HashMap::new();
    for (field, stores) in info.field_stores.iter().enumerate() {
        for fs in stores {
            field_of_store.insert(fs.store, (field, fs.value));
        }
    }
    let mut store_fields: Vec<Option<(usize, Option<ir::NodeId>)>> =
        Vec::with_capacity(info.eliminated_stores.len());
    for ea in &info.eliminated_stores {
        let ir_store = match reverse_map.get(ea) {
            Some(&id) => id,
            None => continue, // a store with no IR node can't have executed observably
        };
        store_ctrls.push(ctrl_of(ir_store)?);
        store_nodes.push(ir_store);
        store_fields.push(
            field_of_store
                .get(ea)
                .and_then(|&(field, v)| reverse_map.get(&v).map(|&irv| (field, Some(irv)))),
        );
    }
    Some((
        ir_new,
        ir_lower::VirtualObjectInfo {
            // An array is described by its element type and its LENGTH
            // (`num_fields`); `class_id` is meaningless for one and is not read.
            array_element_type: info.array_element_type,
            class_id: info.class_id,
            num_fields: info.num_fields,
            field_values,
            new_ctrl,
            store_ctrls,
            store_nodes,
            store_fields,
        },
    ))
}

/// Verify `graph` and, on failure, record a structured bailout.
///
/// Returns `true` when the graph must **not** be lowered. The caller's
/// response to `true` is to do nothing — the IR pipeline's failure path is
/// "fall out of the `if let Some(graph)` arm and let the single-pass backend
/// below compile the method", which is always semantically valid because the
/// optimizing tier is an optimization, never a requirement.
///
/// This is the adapter the P0 "JIT correctness" lane of
/// `deep-research-vm-c2.md` asks for: `verify_graph` returns
/// `Result`, the compile path returns `Option`, and rather than change the
/// signature of anything already in the pipeline the conversion happens here,
/// at the call site. The bailout is *counted* (`bailout::bailout_counts`) so
/// "how often does the verifier reject a graph, and for what?" is answerable
/// without parsing debug logs — the review's "measure compilation quality"
/// item. It is only *printed* under the existing `ir_stage_reporting()` flag,
/// because a bailout is a quality signal, not a fault.
pub(crate) fn ir_verify_reject(
    graph: &ir::Graph,
    phase: &str,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    ir_verify_reject_inner(graph, phase, None, class_name, method_name, descriptor)
}

/// [`ir_verify_reject`] for the `"pre-lower"` hook, which runs after escape
/// analysis and after `ir_prune_unconsumable_snapshots`: hands the verifier the
/// ids of the allocations EA scalar-replaced (`sr_map`'s keys, every round's),
/// so the frame-state lane can tell a virtual-object descriptor slot from a
/// stranded one (`ir_verify::verify_graph_with_scalar_replaced`, round 11
/// wave 18; `r11-irback-release-builds-verify-structure-only`, the `sr_map`
/// hand-off).
///
/// `None` (no map: EA replaced nothing, or precise resume is off) is the EMPTY
/// set, not "unknown". `plan_scalar_replacement` elides an allocation a
/// consultable snapshot names only when a descriptor will exist, which is only
/// when `scalar_deopt_descriptor_available()`, and then the map describes it
/// (`scalar_replacement_map_and_plans` describes every elided object);
/// `ir_prune_unconsumable_snapshots` has dropped every snapshot no deopt can
/// consult, with an attribution at least as precise as the planner's. So at
/// this hook a retained slot naming a removed node that is NOT a key is a
/// defect whichever way the map came out.
pub(crate) fn ir_verify_reject_pre_lower(
    graph: &ir::Graph,
    sr_map: Option<&ir_lower::ScalarReplacementMap>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let scalar_replaced: std::collections::HashSet<ir::NodeId> = sr_map
        .map(|m| m.objects.keys().copied().collect())
        .unwrap_or_default();
    ir_verify_reject_inner(
        graph,
        "pre-lower",
        Some(&scalar_replaced),
        class_name,
        method_name,
        descriptor,
    )
}

fn ir_verify_reject_inner(
    graph: &ir::Graph,
    phase: &str,
    scalar_replaced: Option<&std::collections::HashSet<ir::NodeId>>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // `metrics::current_phase` charges this verification run to whichever
    // compilation is innermost on this thread. A thread-local hook rather than
    // a new parameter precisely because this function has three call sites in
    // `try_compile_inner` and the task's constraint is "do not change any
    // signature". It is a no-op (and reads no clock) unless
    // `CRATONVM_JIT_METRICS=1`.
    let _metrics_phase = metrics::current_phase(metrics::Phase::Verify);
    // `for_phase`, not `from_env`: the environment can only ADD lanes, and each
    // pipeline point runs every lane that is known clean there. At
    // `PHASE_POST_OPTIMIZE` that is the frame-state and memory-chain lanes
    // (`ir_optimize` roots snapshots in DCE and splices the chain in DSE); after
    // `apply_ea_to_ir` it is whatever `ir_verify::APPLY_EA_SPLICES_MEMORY_CHAIN`
    // and `ir_verify::APPLY_EA_ROUTES_ALL_SAFEPOINTS` say. Flipping those two
    // constants is what turns the lanes on at `"post-escape-analysis"` and
    // `"pre-lower"` — see this function's callers.
    let opts = ir_verify::VerifyOptions::for_phase(phase);
    let verdict = match scalar_replaced {
        Some(sr) => ir_verify::verify_graph_with_scalar_replaced(graph, phase, opts, sr),
        None => ir_verify::verify_graph(graph, phase, opts),
    };
    match verdict {
        Ok(()) => false,
        Err(b) => {
            bailout::record_bailout(&b);
            // Attribution, not duplication: `record_bailout` above owns the
            // process-wide category counters; this attaches the same bailout to
            // *this* method and *this* phase in the per-compilation report.
            metrics::note_current_bailout(&b, phase);
            if ir_stage_reporting() {
                eprintln!("[ir] verifier rejected {class_name}.{method_name}{descriptor}: {b}");
            }
            true
        }
    }
}

#[cfg(test)]
mod r9_ea_bridge_tests {
    use super::*;
    use crate::ir::{Graph, IrType, MemKind, Op, NO_NODE};

    /// `entry == 0`, `exit == NO_NODE`, as the struct literal this replaced.
    fn empty_graph() -> Graph {
        Graph::empty(0)
    }

    /// `Holder h = new Holder(); h.f = new Inner(); Inner x = h.f; foo();`
    /// — the builder threads the `getfield` onto the memory chain, so the
    /// call's TOKEN is the field load. That ordering edge used to reach the
    /// `Call` escape rule as if it were an argument, and through the load's
    /// field edge it escaped `Inner`.
    #[test]
    fn a_field_load_used_only_as_a_memory_token_does_not_escape_what_it_reads() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let inner = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let holder = g.add(
            Op::New {
                class_id: 8,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let store = g.add(
            Op::Store(MemKind::Ref),
            IrType::Memory,
            vec![ctrl, mem, holder, off0, inner],
            None,
        );
        let load = g.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![ctrl, store, holder, off0],
            None,
        );
        // No reference ARGUMENT: the load is only the call's memory token.
        // `info_ptr: 0` keeps it a plain `escape_analysis::Op::Call`.
        let call = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Void,
            vec![ctrl, load],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl], None);
        g.entry = start;
        g.exit = ret;

        let (ea, id_map) = escape_analysis_from_ir(&g);
        assert!(
            !ea.nodes[id_map[call as usize]]
                .inputs
                .contains(&id_map[load as usize]),
            "the memory token is not an EA operand of the call"
        );
        let result = escape_analysis::analyze_escapes(&ea);
        assert_eq!(
            result.escape_states.get(&id_map[inner as usize]).copied(),
            Some(escape_analysis::EscapeState::NoEscape),
            "nothing publishes Inner: it is only READ, and the read is ordered before a call"
        );
    }

    /// The same shape with the load as a real ARGUMENT still escapes: only the
    /// token slot is dropped.
    #[test]
    fn a_field_load_passed_as_an_argument_still_escapes_what_it_reads() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let inner = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let holder = g.add(
            Op::New {
                class_id: 8,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let store = g.add(
            Op::Store(MemKind::Ref),
            IrType::Memory,
            vec![ctrl, mem, holder, off0, inner],
            None,
        );
        let load = g.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![ctrl, store, holder, off0],
            None,
        );
        let _call = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Void,
            vec![ctrl, load, load],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl], None);
        g.entry = start;
        g.exit = ret;

        let (ea, id_map) = escape_analysis_from_ir(&g);
        let result = escape_analysis::analyze_escapes(&ea);
        assert!(
            result
                .escape_states
                .get(&id_map[inner as usize])
                .copied()
                .is_some_and(|s| s.may_escape()),
            "an argument is an argument"
        );
    }

    /// `System.identityHashCode` is the one identity-hash shape; an
    /// `invokespecial java/lang/Object.hashCode()I` can select an override
    /// (JVMS §6.5, `ACC_SUPER`) and must stay an ordinary escaping call.
    #[test]
    fn only_system_identity_hash_code_is_an_identity_observation() {
        fn info(class: &'static str, name: &'static str, desc: &'static str, kind: u8) -> usize {
            // LEAK(intentional): test-only; the bridge reads it through a raw pointer.
            Box::leak(Box::new(JitInvokeInfo {
                class_name: class,
                method_name: name,
                descriptor: desc,
                num_jit_args: 1,
                return_type: b'I',
                invoke_kind: kind,
                declaring_class_id: 0,
                owner_class_id: 0,
            })) as *const JitInvokeInfo as usize
        }
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let obj = g.add(Op::Param(0), IrType::Ref, vec![], None);
        let sys = info(
            "java/lang/System",
            "identityHashCode",
            "(Ljava/lang/Object;)I",
            3,
        );
        let sup = info("java/lang/Object", "hashCode", "()I", 1);
        let c_sys = g.add(
            Op::Call { info_ptr: sys },
            IrType::Int,
            vec![ctrl, mem, obj],
            None,
        );
        let c_sup = g.add(
            Op::Call { info_ptr: sup },
            IrType::Int,
            vec![ctrl, mem, obj],
            None,
        );
        assert!(ir_call_is_identity_hash(&g.nodes[c_sys as usize], sys));
        assert!(
            !ir_call_is_identity_hash(&g.nodes[c_sup as usize], sup),
            "invokespecial Object.hashCode may run an override"
        );
    }

    /// `p.x = 7; b.v = p.x;` with BOTH objects replaced in one round: `b`'s
    /// deopt recipe must name the value `p.x` reads (7), not the load the
    /// victim splice is about to kill.
    #[test]
    fn a_recipe_field_names_the_forwarded_value_not_the_retired_load() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let b = g.add(
            Op::New {
                class_id: 8,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
        let st_p = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, p, off0, seven],
            None,
        );
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, st_p, p, off0],
            None,
        );
        let _st_b = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, load, b, off0, load],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, seven], None);
        g.entry = start;
        g.exit = ret;

        let (ea, id_map) = escape_analysis_from_ir(&g);
        let result = escape_analysis::analyze_escapes(&ea);
        assert_eq!(
            result.scalar_replaceable.len(),
            2,
            "both objects are replaceable"
        );
        let map =
            build_scalar_replacement_map(&g, &id_map, &result, &std::collections::HashSet::new());
        let recipe = map
            .objects
            .get(&b)
            .expect("b is elided, so it has a recipe");
        assert_eq!(
            recipe.field_values.first().copied().flatten(),
            Some(seven),
            "the recipe names what p.x read, not the retired load {load}"
        );
    }

    // ── Box/unbox fold (round 9 wave 10, collect10) ──────────────────

    fn box_info(class: &'static str, desc: &'static str) -> usize {
        // LEAK(intentional): test-only; the bridge reads it through a raw pointer.
        Box::leak(Box::new(JitInvokeInfo {
            class_name: class,
            method_name: "valueOf",
            descriptor: desc,
            num_jit_args: 1,
            return_type: b'L',
            invoke_kind: 3,
            declaring_class_id: 0,
            owner_class_id: 0,
        })) as *const JitInvokeInfo as usize
    }

    fn snapshot(
        bci: usize,
        locals: Vec<ir::NodeId>,
        stack: Vec<ir::NodeId>,
    ) -> ir::SafepointSnapshot {
        ir::SafepointSnapshot::new(bci, locals, stack, Vec::new())
    }

    /// `static int f(int v) { Integer a = v; g(); return a.intValue(); }`,
    /// flattened: `valueOf` at bci 3, the box on the stack at bci 5, the
    /// `intValue` at bci 7 (box in local 1 and on the stack), a token-only call
    /// at bci 9, the return at 11. With `deopt_at_5`, a deoptable node sits at
    /// bci 5, where the box is live.
    struct BoxFixture {
        g: Graph,
        mem: ir::NodeId,
        v: ir::NodeId,
        call: ir::NodeId,
        unbox: ir::NodeId,
        after: ir::NodeId,
        ret: ir::NodeId,
    }

    fn box_fixture(deopt_at_5: bool) -> BoxFixture {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let v = g.add(Op::Param(0), IrType::Int, vec![], None);
        let info = box_info("java/lang/Integer", "(I)Ljava/lang/Integer;");
        let call = g.add(
            Op::Call { info_ptr: info },
            IrType::Ref,
            vec![ctrl, mem, v],
            Some(3),
        );
        let mut chain = call;
        if deopt_at_5 {
            chain = g.add(
                Op::Call { info_ptr: 0 },
                IrType::Void,
                vec![ctrl, call],
                Some(5),
            );
        }
        let unbox = g.add(
            Op::Unbox {
                op: ir::UnboxOp::IntValue,
                class_id: 7,
            },
            IrType::Int,
            vec![ctrl, chain, call],
            Some(7),
        );
        let after = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Void,
            vec![ctrl, unbox],
            Some(9),
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, unbox], Some(11));
        g.entry = start;
        g.exit = ret;
        g.push_safepoint(snapshot(3, vec![v, NO_NODE], vec![v]));
        g.push_safepoint(snapshot(5, vec![v, NO_NODE], vec![call]));
        g.push_safepoint(snapshot(7, vec![v, call], vec![call]));
        g.push_safepoint(snapshot(9, vec![v, NO_NODE], vec![unbox]));
        g.push_safepoint(snapshot(11, vec![v, NO_NODE], vec![unbox]));
        BoxFixture {
            g,
            mem,
            v,
            call,
            unbox,
            after,
            ret,
        }
    }

    /// The fold: the unbox's readers (value edge AND snapshot slot) now name
    /// the primitive, the memory chain closes over both retired nodes, and no
    /// snapshot slot names the dead box.
    #[test]
    fn integer_value_of_read_back_only_by_int_value_folds_to_its_argument() {
        let mut f = box_fixture(false);
        assert_eq!(ir_fold_unbox_of_fresh_box(&mut f.g), 1);
        assert_eq!(
            f.g.nodes[f.call as usize].op,
            Op::Dead,
            "the valueOf call is gone"
        );
        assert_eq!(
            f.g.nodes[f.unbox as usize].op,
            Op::Dead,
            "the unbox is gone"
        );
        assert_eq!(
            f.g.nodes[f.ret as usize].inputs[1], f.v,
            "intValue() of the box is the int that was boxed"
        );
        assert_eq!(
            f.g.nodes[f.after as usize].inputs[1], f.mem,
            "the token chain closes over both retired nodes onto the entry memory"
        );
        let at = |bci: usize| {
            f.g.safepoints
                .iter()
                .find(|sp| sp.bci == bci)
                .expect("snapshot kept")
        };
        assert_eq!(
            at(9).stack[0],
            f.v,
            "a snapshot naming the unbox follows the value"
        );
        assert_eq!(at(5).stack[0], NO_NODE, "no snapshot names the dead box");
        assert_eq!(at(7).locals[1], NO_NODE, "no snapshot names the dead box");
    }

    /// A deopt that can resume where the box is live (a deoptable node at bci
    /// 5, whose snapshot holds it) needs the object: refused, graph untouched.
    #[test]
    fn a_box_live_at_a_consultable_snapshot_is_not_folded() {
        let mut f = box_fixture(true);
        assert_eq!(ir_fold_unbox_of_fresh_box(&mut f.g), 0);
        assert!(matches!(f.g.nodes[f.call as usize].op, Op::Call { .. }));
        assert!(matches!(f.g.nodes[f.unbox as usize].op, Op::Unbox { .. }));
        assert_eq!(f.g.nodes[f.ret as usize].inputs[1], f.unbox);
    }

    /// Any value use other than an unbox of the boxed width keeps the call.
    #[test]
    fn a_box_with_any_other_reader_is_not_folded() {
        // The box is returned: its identity is observable.
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let v = g.add(Op::Param(0), IrType::Int, vec![], None);
        let info = box_info("java/lang/Integer", "(I)Ljava/lang/Integer;");
        let call = g.add(
            Op::Call { info_ptr: info },
            IrType::Ref,
            vec![ctrl, mem, v],
            None,
        );
        let _unbox = g.add(
            Op::Unbox {
                op: ir::UnboxOp::IntValue,
                class_id: 7,
            },
            IrType::Int,
            vec![ctrl, call, call],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, call], None);
        g.entry = start;
        g.exit = ret;
        assert_eq!(ir_fold_unbox_of_fresh_box(&mut g), 0);
        assert!(matches!(g.nodes[call as usize].op, Op::Call { .. }));

        // Width mismatch: an `Integer` box read by a `longValue` unbox.
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let v = g.add(Op::Param(0), IrType::Int, vec![], None);
        let call = g.add(
            Op::Call { info_ptr: info },
            IrType::Ref,
            vec![ctrl, mem, v],
            None,
        );
        let wide = g.add(
            Op::Unbox {
                op: ir::UnboxOp::LongValue,
                class_id: 7,
            },
            IrType::Long,
            vec![ctrl, call, call],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, wide], None);
        g.entry = start;
        g.exit = ret;
        assert_eq!(ir_fold_unbox_of_fresh_box(&mut g), 0);
        assert!(matches!(g.nodes[call as usize].op, Op::Call { .. }));
    }

    /// The `Long` twin folds; and a nested
    /// `Integer.valueOf(Integer.valueOf(v).intValue()).intValue()` folds both
    /// levels down to `v` (the outer plan's value is the inner plan's retired
    /// unbox).
    #[test]
    fn long_value_of_and_nested_boxes_fold() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let j = g.add(Op::Param(0), IrType::Long, vec![], None);
        let linfo = box_info("java/lang/Long", "(J)Ljava/lang/Long;");
        let lcall = g.add(
            Op::Call { info_ptr: linfo },
            IrType::Ref,
            vec![ctrl, mem, j],
            None,
        );
        let lun = g.add(
            Op::Unbox {
                op: ir::UnboxOp::LongValue,
                class_id: 9,
            },
            IrType::Long,
            vec![ctrl, lcall, lcall],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, lun], None);
        g.entry = start;
        g.exit = ret;
        assert_eq!(ir_fold_unbox_of_fresh_box(&mut g), 1);
        assert_eq!(g.nodes[ret as usize].inputs[1], j);

        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let v = g.add(Op::Param(0), IrType::Int, vec![], None);
        let info = box_info("java/lang/Integer", "(I)Ljava/lang/Integer;");
        let inner = g.add(
            Op::Call { info_ptr: info },
            IrType::Ref,
            vec![ctrl, mem, v],
            None,
        );
        let inner_un = g.add(
            Op::Unbox {
                op: ir::UnboxOp::IntValue,
                class_id: 7,
            },
            IrType::Int,
            vec![ctrl, inner, inner],
            None,
        );
        let outer = g.add(
            Op::Call { info_ptr: info },
            IrType::Ref,
            vec![ctrl, inner_un, inner_un],
            None,
        );
        let outer_un = g.add(
            Op::Unbox {
                op: ir::UnboxOp::IntValue,
                class_id: 7,
            },
            IrType::Int,
            vec![ctrl, outer, outer],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, outer_un], None);
        g.entry = start;
        g.exit = ret;
        assert_eq!(ir_fold_unbox_of_fresh_box(&mut g), 2);
        assert_eq!(g.nodes[ret as usize].inputs[1], v, "both levels fold to v");
        for dead in [inner, inner_un, outer, outer_un] {
            assert_eq!(g.nodes[dead as usize].op, Op::Dead);
        }
    }
}

#[cfg(test)]
mod r11w2_mid_ea_bridge_tests {
    use super::*;
    use crate::ir::{Graph, IrType, MemKind, Op};

    /// `entry == 0`, `exit == NO_NODE`, as the struct literal this replaced.
    fn empty_graph() -> Graph {
        Graph::empty(0)
    }

    /// A switch inside a profiled-cold arm is cold, and so is every case it
    /// reaches. `ea_control_preds` had no `Switch` arm, so the switch had no
    /// control predecessor and nothing below it could ever be cold.
    #[test]
    fn a_switch_in_a_cold_arm_and_its_cases_are_cold() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let cond = g.add(Op::Const(1), IrType::Int, vec![], None);
        let iff = g.add(Op::If, IrType::Control, vec![ctrl, cond], Some(10));
        let hot = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let cold_arm = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let key = g.add(Op::Const(0), IrType::Int, vec![], None);
        let sw = g.add(
            Op::Switch { low: 0, high: 1 },
            IrType::Control,
            vec![cold_arm, key],
            Some(20),
        );
        let case0 = g.add(Op::Proj(0), IrType::Control, vec![sw], None);
        let case1 = g.add(Op::Proj(1), IrType::Control, vec![sw], None);
        let dflt = g.add(Op::Proj(2), IrType::Control, vec![sw], None);
        let ret = g.add(Op::Return, IrType::Void, vec![case0], None);
        g.entry = start;
        g.exit = ret;

        // pc 10 is usually TAKEN, so its false projection is the cold one.
        let hints: HashMap<usize, bool> = [(10usize, true)].into_iter().collect();
        let cold = ea_cold_control_nodes(&g, &hints);
        for id in [cold_arm, sw, case0, case1, dflt, ret] {
            assert!(cold.contains(&id), "node {id} is only reachable coldly");
        }
        assert!(!cold.contains(&hot), "the taken arm is not cold");
        assert!(!cold.contains(&iff), "the branch itself runs on every path");
    }

    /// `p = new P(); b = new B(); b.x = p.x;` with `p.x` never stored: the load
    /// is retired to a zero default this round, so `b`'s recipe must describe a
    /// zero rather than name the load (which the lowerer answers
    /// `MaterializationRequired`, discarding the artifact).
    #[test]
    fn a_recipe_field_read_from_a_zero_default_load_describes_zero() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let b = g.add(
            Op::New {
                class_id: 8,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, p, off0],
            None,
        );
        let _st_b = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, load, b, off0, load],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, seven], None);
        g.entry = start;
        g.exit = ret;

        let (ea, id_map) = escape_analysis_from_ir(&g);
        let result = escape_analysis::analyze_escapes(&ea);
        if result.scalar_replaceable.len() != 2 {
            // The analysis declined one of the two; nothing to check here.
            return;
        }
        let map =
            build_scalar_replacement_map(&g, &id_map, &result, &std::collections::HashSet::new());
        if let Some(recipe) = map.objects.get(&b) {
            assert_eq!(
                recipe.field_values.first().copied().flatten(),
                None,
                "b.x holds p.x's zero default, not the retired load {load}"
            );
        }
    }

    /// A load an earlier round's recipe names (it is in `descriptor_pinned`)
    /// is not forwarded and killed, and the object it reads stays allocated.
    #[test]
    fn a_load_a_recipe_names_survives_and_keeps_its_object() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
        let st_p = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, p, off0, seven],
            None,
        );
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, st_p, p, off0],
            None,
        );
        let ret = g.add(Op::Return, IrType::Int, vec![ctrl, load], None);
        g.entry = start;
        g.exit = ret;

        let (ea, id_map) = escape_analysis_from_ir(&g);
        let result = escape_analysis::analyze_escapes(&ea);
        let pinned: std::collections::HashSet<ir::NodeId> = [load].into_iter().collect();
        let _ = apply_ea_to_ir_pinned(&mut g, &id_map, &result, &pinned);
        assert!(
            matches!(g.nodes[load as usize].op, Op::Load(_)),
            "the pinned load is still a live load"
        );
        assert!(
            matches!(g.nodes[p as usize].op, Op::New { .. }),
            "the object the pinned load reads is still allocated"
        );
        assert!(
            matches!(g.nodes[st_p as usize].op, Op::Store(_)),
            "and so is its store"
        );
    }

    /// `p = new P(); p.x = 7; a = p.x; b = p.x; return a + b;` with the second
    /// load chained on the first as its memory token and a snapshot naming
    /// both. The victim loop (round 11 wave 4: per-victim user lists instead
    /// of a whole-graph sweep per victim) must leave no live input and no
    /// snapshot slot naming a killed load. The second load's token slot names
    /// the first load, which is itself a victim: that reference reaches the
    /// first load's user list only through the index.
    #[test]
    fn the_victim_loop_leaves_no_reference_to_a_killed_load() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
        let st_p = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, p, off0, seven],
            None,
        );
        let a = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, st_p, p, off0],
            None,
        );
        let b = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, a, p, off0],
            None,
        );
        let sum = g.add(Op::Add, IrType::Int, vec![a, b], None);
        let ret = g.add(Op::Return, IrType::Int, vec![ctrl, sum], None);
        g.entry = start;
        g.exit = ret;
        let snap = crate::ir::SafepointSnapshot::new(0, vec![a, b], vec![], vec![]);
        g.push_safepoint(snap);

        let (ea, id_map) = escape_analysis_from_ir(&g);
        let result = escape_analysis::analyze_escapes(&ea);
        let _ = apply_ea_to_ir_pinned(&mut g, &id_map, &result, &std::collections::HashSet::new());
        if g.nodes[a as usize].op != Op::Dead {
            // The analysis declined the object; nothing was rewritten.
            return;
        }
        for (id, n) in g.nodes.iter().enumerate() {
            if n.op == Op::Dead {
                continue;
            }
            for &inp in n.inputs.iter() {
                if let Some(x) = g.nodes.get(inp as usize) {
                    assert_ne!(
                        x.op,
                        Op::Dead,
                        "live node {id} still names killed node {inp}"
                    );
                }
            }
        }
        for &v in &g.safepoints[0].locals {
            assert_ne!(
                g.nodes[v as usize].op,
                Op::Dead,
                "a snapshot slot names killed {v}"
            );
        }
        if g.nodes[b as usize].op == Op::Dead {
            assert_eq!(g.nodes[sum as usize].inputs.as_slice(), &[seven, seven]);
            assert_eq!(g.safepoints[0].locals, vec![seven, seven]);
        }
    }
}

// ── Round 11 wave 7 (lane mid) ───────────────────────────────────────

#[cfg(test)]
mod r11w7_mid_ea_bridge_tests {
    use super::*;
    use crate::ir::{Graph, IrType, MemKind, Op};

    /// `p = new P(); p.x = 7; a = p.x; b = p.x; return a + b;`, a snapshot
    /// naming both loads.
    fn two_loads_of_one_store() -> Graph {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
        let st_p = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, p, off0, seven],
            None,
        );
        let a = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, st_p, p, off0],
            None,
        );
        let b = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, a, p, off0],
            None,
        );
        let sum = g.add(Op::Add, IrType::Int, vec![a, b], None);
        let ret = g.add(Op::Return, IrType::Int, vec![ctrl, sum], None);
        g.entry = start;
        g.exit = ret;
        let frame = crate::ir::SafepointSnapshot::new(0, vec![a, b], vec![], vec![]);
        g.safepoints.push(frame);
        g
    }

    /// What a graph looks like, as far as the rewrite can change it.
    fn shape(g: &Graph) -> (Vec<(Op, Vec<ir::NodeId>)>, Vec<Vec<ir::NodeId>>) {
        (
            g.nodes
                .iter()
                .map(|n| (n.op.clone(), n.inputs.as_slice().to_vec()))
                .collect(),
            g.safepoints.iter().map(|s| s.locals.clone()).collect(),
        )
    }

    /// Handing the map builder's plans to the applier (the production path
    /// since wave 7) rewrites the graph exactly as letting the applier plan
    /// again does, and the builder's map is the same map.
    #[test]
    fn handed_over_plans_rewrite_the_graph_like_a_re_plan() {
        let pinned = std::collections::HashSet::new();

        let mut replanned = two_loads_of_one_store();
        let (ea, id_map) = escape_analysis_from_ir(&replanned);
        let result = escape_analysis::analyze_escapes(&ea);
        let map_a = build_scalar_replacement_map(&replanned, &id_map, &result, &pinned);
        let applied_a = apply_ea_to_ir_pinned(&mut replanned, &id_map, &result, &pinned);

        let mut handed = two_loads_of_one_store();
        let (ea, id_map) = escape_analysis_from_ir(&handed);
        let result = escape_analysis::analyze_escapes(&ea);
        let (map_b, plans) =
            build_scalar_replacement_map_with_plans(&handed, &id_map, &result, &pinned);
        let applied_b = apply_ea_to_ir_planned(&mut handed, &id_map, &result, &pinned, Some(plans));

        assert_eq!(applied_a, applied_b);
        assert_eq!(shape(&replanned), shape(&handed));
        let mut keys_a: Vec<_> = map_a.objects.keys().copied().collect();
        let mut keys_b: Vec<_> = map_b.objects.keys().copied().collect();
        keys_a.sort_unstable();
        keys_b.sort_unstable();
        assert_eq!(keys_a, keys_b);
    }

    /// An empty hand-over applies no scalar replacement at all: the applier
    /// takes the plans it is given and does not plan again.
    #[test]
    fn an_empty_hand_over_is_not_re_planned() {
        let pinned = std::collections::HashSet::new();
        let mut g = two_loads_of_one_store();
        let before = shape(&g);
        let (ea, id_map) = escape_analysis_from_ir(&g);
        let result = escape_analysis::analyze_escapes(&ea);
        let _ = apply_ea_to_ir_planned(&mut g, &id_map, &result, &pinned, Some(Vec::new()));
        if result.lock_elisions.is_empty() && result.lock_coarsening.is_empty() {
            assert_eq!(shape(&g), before, "nothing was planned, so nothing changed");
        }
    }
}

// ── Round 11 wave 9 (lane mid): lock elision on real monitor ops ─────

#[cfg(test)]
mod r11w9_mid_ea_bridge_tests {
    use super::*;
    use crate::ir::{Graph, IrType, Op};

    /// `synchronized (new Object()) {}` built with the ops the builder emits:
    /// `[ctrl, mem, obj]` monitors on the memory chain. `publish` returns the
    /// object, which lets another thread reach it. Returns the graph and
    /// `[object, enter, exit]`.
    fn locked_fresh_object(publish: bool) -> (Graph, [ir::NodeId; 3]) {
        let mut g = Graph::empty(12);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let obj = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            Some(0),
        );
        let enter = g.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![ctrl, mem, obj],
            Some(4),
        );
        let exit = g.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![ctrl, enter, obj],
            Some(6),
        );
        let ret = if publish {
            g.add(Op::Return, IrType::Ref, vec![ctrl, obj], Some(8))
        } else {
            g.add(Op::Return, IrType::Void, vec![ctrl], Some(8))
        };
        g.entry = start;
        g.exit = ret;
        (g, [obj, enter, exit])
    }

    /// HotSpot's EliminateLocks shape reaches the production applier: the
    /// bridge sees both monitors, the analysis offers the object whole, and
    /// both ops die with the chain spliced.
    #[test]
    fn a_lock_on_a_confined_allocation_is_elided_whole() {
        let (mut g, [obj, enter, exit]) = locked_fresh_object(false);
        let (ea, id_map) = escape_analysis_from_ir(&g);
        let result = escape_analysis::analyze_escapes(&ea);
        assert_eq!(result.lock_elisions.len(), 1, "{:?}", result.lock_refusals);
        assert_eq!(result.lock_elisions[0].object, id_map[obj as usize]);
        assert_eq!(result.lock_elisions[0].monitors.len(), 2);
        apply_ea_to_ir(&mut g, &id_map, &result);
        assert_eq!(g.nodes[enter as usize].op, Op::Dead);
        assert_eq!(g.nodes[exit as usize].op, Op::Dead);
    }

    /// Round 14 wave 3 (lane fixup): `o.leaf(); synchronized (o) { .. }` on a
    /// fresh `o` -- a recorded splice window, then a bytecode region. EA
    /// coarsening would delete the window's exit and the region's enter;
    /// the lowerer can pair neither, so the plan is refused. Two windows, or
    /// two bytecode pairs, are not split and stay coarsenable.
    #[test]
    fn an_ea_coarsening_across_a_window_and_a_bytecode_pair_is_refused() {
        let build = |record: [bool; 2]| {
            let mut g = Graph::empty(16);
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
            let obj = g.add(
                Op::New {
                    class_id: 7,
                    num_fields: 1,
                },
                IrType::Ref,
                vec![ctrl, mem],
                Some(0),
            );
            let ew = g.add(Op::MonitorEnter, IrType::Memory, vec![ctrl, mem, obj], Some(4));
            let xw = g.add(Op::MonitorExit, IrType::Memory, vec![ctrl, ew, obj], Some(4));
            let e2 = g.add(Op::MonitorEnter, IrType::Memory, vec![ctrl, xw, obj], Some(8));
            let x2 = g.add(Op::MonitorExit, IrType::Memory, vec![ctrl, e2, obj], Some(10));
            let ret = g.add(Op::Return, IrType::Void, vec![ctrl], Some(12));
            g.entry = start;
            g.exit = ret;
            for (i, (enter, exit)) in [(ew, xw), (e2, x2)].into_iter().enumerate() {
                if record[i] {
                    g.sync_splice_windows.push(ir::SyncSpliceWindow {
                        enter,
                        exit,
                        monitor: obj,
                        static_class_id: 0,
                    });
                }
            }
            (g, xw, e2)
        };
        for (record, splits) in [
            ([true, false], true),
            ([false, true], true),
            ([true, true], false),
            ([false, false], false),
        ] {
            let (g, xw, e2) = build(record);
            // Identity numbering: EA node `k` is IR node `k`.
            let reverse_map: HashMap<escape_analysis::NodeId, ir::NodeId> = (0..g.nodes.len())
                .map(|i| (i, i as ir::NodeId))
                .collect();
            let inner = [xw as escape_analysis::NodeId, e2 as escape_analysis::NodeId];
            assert_eq!(
                ea_coarsening_splits_a_sync_window(&g, &reverse_map, inner),
                splits,
                "windows recorded {record:?}"
            );
        }
    }

    /// A returned object is reachable from another thread: its lock is a
    /// real happens-before edge and stays.
    #[test]
    fn a_lock_on_a_published_allocation_stays() {
        let (mut g, [_, enter, exit]) = locked_fresh_object(true);
        let (ea, id_map) = escape_analysis_from_ir(&g);
        let result = escape_analysis::analyze_escapes(&ea);
        assert!(result.lock_elisions.is_empty());
        apply_ea_to_ir(&mut g, &id_map, &result);
        assert_eq!(g.nodes[enter as usize].op, Op::MonitorEnter);
        assert_eq!(g.nodes[exit as usize].op, Op::MonitorExit);
    }
}

// ── Round 11 wave 10 (lane irstatic): `putstatic` publishes ──────────

#[cfg(test)]
mod r11w10_irstatic_ea_bridge_tests {
    use super::*;
    use crate::ir::{Graph, IrType, Op};

    /// `synchronized (o) {} ; S = o;` with `o` fresh: the static write is the
    /// only thing that publishes `o`, and it must publish it GLOBALLY -- no
    /// scalar replacement, no lock elision, and the EA state is
    /// `GlobalEscape` (a static is visible to every thread), not the
    /// `ArgEscape` a callee argument gets.
    #[test]
    fn a_reference_written_to_a_static_escapes_globally() {
        let mut g = Graph::empty(12);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let obj = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            Some(0),
        );
        let enter = g.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![ctrl, mem, obj],
            Some(4),
        );
        let exit = g.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![ctrl, enter, obj],
            Some(6),
        );
        let store = g.add(
            Op::StoreStatic {
                class_id: 9,
                field_index: 0,
                type_tag: b'L',
                is_volatile: false,
            },
            IrType::Memory,
            vec![ctrl, exit, obj],
            Some(8),
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl], Some(11));
        g.entry = start;
        g.exit = ret;

        let (ea, id_map) = escape_analysis_from_ir(&g);
        assert_eq!(
            ea.nodes[id_map[store as usize]].op,
            escape_analysis::Op::Other,
            "a static write is the unmodelled-publication node, not a call"
        );
        let result = escape_analysis::analyze_escapes(&ea);
        assert_eq!(
            result.escape_states.get(&id_map[obj as usize]),
            Some(&escape_analysis::EscapeState::GlobalEscape)
        );
        assert!(result.lock_elisions.is_empty());
        assert!(result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != id_map[obj as usize]));
        apply_ea_to_ir(&mut g, &id_map, &result);
        assert_eq!(g.nodes[enter as usize].op, Op::MonitorEnter);
        assert_eq!(g.nodes[exit as usize].op, Op::MonitorExit);
        assert!(matches!(g.nodes[store as usize].op, Op::StoreStatic { .. }));
    }
}

/// Round 12 wave 3 (lane iropt2): W2-6, a φ of fitting values fits a narrow
/// array slot.
#[cfg(test)]
mod r12w3_iropt2_narrow_phi_tests {
    use super::*;
    use crate::ir::{Graph, IrType, MemKind, NodeId, Op, NO_NODE};

    fn graph() -> (Graph, NodeId) {
        let mut g = Graph::empty(0);
        g.exit = 0;
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let join = g.add(Op::Merge, IrType::Control, vec![start, start], None);
        (g, join)
    }

    #[test]
    fn a_phi_of_zero_and_one_fits_a_boolean_slot() {
        let (mut g, join) = graph();
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let phi = g.add(Op::Phi, IrType::Int, vec![join, one, zero], None);
        assert!(narrow_array_value_fits(&g, phi, MemKind::Byte, Some(4)));
        // A 2 among the inputs does not fit a boolean.
        let two = g.add(Op::Const(2), IrType::Int, vec![], None);
        let bad = g.add(Op::Phi, IrType::Int, vec![join, one, two], None);
        assert!(!narrow_array_value_fits(&g, bad, MemKind::Byte, Some(4)));
    }

    #[test]
    fn a_nested_or_cyclic_phi_fits_only_if_every_leaf_does() {
        let (mut g, join) = graph();
        let c100 = g.add(Op::Const(100), IrType::Int, vec![], None);
        let c_neg = g.add(Op::Const(-100), IrType::Int, vec![], None);
        let inner = g.add(Op::Phi, IrType::Int, vec![join, c100, c_neg], None);
        // `outer = φ(inner, outer)`: a loop-carried φ re-feeding itself.
        let outer = g.add(Op::Phi, IrType::Int, vec![join, inner, NO_NODE], None);
        assert!(g.set_input(outer, 2, outer));
        assert!(narrow_array_value_fits(&g, outer, MemKind::Byte, Some(8)));
        assert!(
            !narrow_array_value_fits(&g, outer, MemKind::Char, Some(5)),
            "-100 is not a char"
        );
        // An undefined edge refuses.
        let undef = g.add(Op::Phi, IrType::Int, vec![join, c100, NO_NODE], None);
        assert!(!narrow_array_value_fits(&g, undef, MemKind::Byte, Some(8)));
    }

    #[test]
    fn the_explicit_narrowing_ops_fit_their_own_kinds() {
        let (mut g, _) = graph();
        let x = g.add(Op::Param(0), IrType::Int, vec![], None);
        let b = g.add(Op::I2B, IrType::Int, vec![x], None);
        let c = g.add(Op::I2C, IrType::Int, vec![x], None);
        assert!(narrow_array_value_fits(&g, b, MemKind::Byte, Some(8)));
        assert!(narrow_array_value_fits(&g, b, MemKind::Short, Some(9)));
        assert!(!narrow_array_value_fits(&g, b, MemKind::Char, Some(5)));
        assert!(narrow_array_value_fits(&g, c, MemKind::Char, Some(5)));
        assert!(!narrow_array_value_fits(&g, c, MemKind::Short, Some(9)));
    }

    #[test]
    fn switched_off_a_phi_fits_only_a_wide_slot() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_EA_NARROW_PHI", Some("0"))],
            || {
                let (mut g, join) = graph();
                let one = g.add(Op::Const(1), IrType::Int, vec![], None);
                let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
                let phi = g.add(Op::Phi, IrType::Int, vec![join, one, zero], None);
                assert!(!narrow_array_value_fits(&g, phi, MemKind::Byte, Some(4)));
                assert!(narrow_array_value_fits(&g, phi, MemKind::Int, Some(10)));
            },
        );
    }
}

/// Round 13 wave 3 (lane framestate): chain snapshots (`ir::SpliceScopeChain`,
/// recorded inside a spliced body) and escape analysis.
#[cfg(test)]
mod r13w3_framestate_ea_tests {
    use super::*;
    use crate::ir::{
        Graph, IrType, MemKind, NodeId, Op, SafepointSnapshot, SpliceScopeChain, SpliceScopeFrame,
    };

    fn chain(bci: usize, local: NodeId, caller_local: NodeId) -> SafepointSnapshot {
        let mut sp = SafepointSnapshot::new(bci, vec![local], vec![caller_local], Vec::new());
        sp.splice_scope = Some(Box::new(SpliceScopeChain {
            scopes: vec![
                SpliceScopeFrame {
                    method_key: "P.f:()V".to_string(),
                    bci: 2,
                    code_len: 5,
                    max_locals: 1,
                    locals_len: 0,
                    stack_len: 0,
                },
                SpliceScopeFrame {
                    method_key: String::new(),
                    bci: 1,
                    code_len: 0,
                    max_locals: 0,
                    locals_len: 1,
                    stack_len: 0,
                },
            ],
        }));
        sp
    }

    /// A graph with a trapping load at combined pc 12 of the body spliced for
    /// the `invoke` at bci 1, a flat frame at 1, and two chain frames at 12:
    /// one naming a live value and one naming an object escape analysis
    /// removed. Returns `(graph, alloc)`.
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
        g.push_safepoint(chain(12, p, p));
        g.push_safepoint(chain(12, alloc, p));
        (g, alloc)
    }

    /// An unnamed chain frame never pins an allocation (the lowerer falls back
    /// to the flat frame for any chain naming a removed object); the flat
    /// frame still does.
    #[test]
    fn an_unnamed_chain_snapshot_does_not_pin_what_it_names() {
        let (g, alloc) = graph();
        assert!(!ea_snapshot_names(&g, alloc), "named only by a chain frame");
        let p = g.safepoints[0].locals[0];
        assert!(ea_snapshot_names(&g, p), "the flat frame at the invoke names it");
    }

    /// After escape analysis removed the object, the post-EA prune drops the
    /// chain frame naming it and keeps the live one and the flat fall-back.
    #[test]
    fn the_post_ea_prune_drops_a_chain_frame_naming_a_removed_object() {
        let (mut g, alloc) = graph();
        g.kill(alloc);
        let dropped = ir_prune_unconsumable_snapshots(&mut g, &[(10, 15, 1)]);
        assert_eq!(dropped, 1);
        assert_eq!(g.safepoints.len(), 2);
        assert!(!g.safepoints[0].is_splice_state());
        assert_eq!(g.safepoints[0].bci, 1);
        assert!(g.safepoints[1].is_splice_state());
        assert_eq!(g.safepoints[1].bci, 12);
    }
}

/// Round 13 wave 6 (lane chain2): escape analysis and the chain snapshots it
/// must respect (`r13w5-resume-ea-chain-virtuals-producer-patch`).
#[cfg(test)]
mod r13w6_chain2_ea_tests {
    use super::*;
    use crate::ir::{
        Graph, IrType, MemKind, NodeId, Op, SafepointSnapshot, SpliceScopeChain, SpliceScopeFrame,
    };

    const VIRTUALS: &str = "CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS";

    fn chain(bci: usize, local: NodeId, caller_local: NodeId) -> SafepointSnapshot {
        let mut sp = SafepointSnapshot::new(bci, vec![local], vec![caller_local], Vec::new());
        sp.splice_scope = Some(Box::new(SpliceScopeChain {
            scopes: vec![
                SpliceScopeFrame {
                    method_key: "P.f:()V".to_string(),
                    bci: 2,
                    code_len: 5,
                    max_locals: 1,
                    locals_len: 0,
                    stack_len: 0,
                },
                SpliceScopeFrame {
                    method_key: String::new(),
                    bci: 1,
                    code_len: 0,
                    max_locals: 0,
                    locals_len: 1,
                    stack_len: 0,
                },
            ],
        }));
        sp
    }

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
                class_id: 5,
                array_element_type: None,
                num_fields: 0,
                field_values: Vec::new(),
                new_ctrl: 0,
                store_ctrls: Vec::new(),
                store_nodes: Vec::new(),
                store_fields: Vec::new(),
            },
        );
        map
    }

    /// Under the chain fences every chain is required, so a recipe no longer
    /// rescues an object one names while chains cannot carry recipes: removed,
    /// it cost the whole compile. With the switch on it may go.
    #[test]
    fn a_required_chain_keeps_the_object_it_names_while_chains_cannot_carry_it() {
        let (mut g, alloc) = graph();
        let ask = |g: &Graph, on: &str| {
            cratonvm_types::flags::with_thread_overrides(&[(VIRTUALS, Some(on))], || {
                ea_required_chain_names(g, alloc)
            })
        };
        assert!(!ask(&g, "0"), "an unclaimed chain pins nothing (the flat frame is the fallback)");
        g.splice_chains_required = true;
        assert!(ask(&g, "0"), "a required chain cannot describe a removed object");
        assert!(!ask(&g, "1"), "a chain that carries recipes pins like a flat frame");
    }

    /// The box fold clears a slot instead of leaving a recipe, so it must see
    /// the chains the allocation planner may ignore.
    #[test]
    fn every_chain_counts_for_a_rewrite_that_leaves_no_recipe() {
        let (g, alloc) = graph();
        assert!(!ea_snapshot_names(&g, alloc), "the planner's view: an unclaimed chain");
        assert!(ea_any_chain_snapshot_names(&g, alloc), "the fold's view: any chain");
        let p = g.safepoints[0].locals[0];
        assert!(ea_any_chain_snapshot_names(&g, p), "the chain's caller scope names it too");
    }

    /// With chains carrying recipes, the prune keeps a chain whose only removed
    /// slot is an object the lowerer will describe; otherwise it drops it as
    /// before.
    #[test]
    fn the_prune_keeps_a_chain_naming_only_described_objects_when_chains_carry_them() {
        let prune = |on: &str, with_map: bool| {
            let (mut g, alloc) = graph();
            g.kill(alloc);
            let map = described(alloc);
            let dropped = cratonvm_types::flags::with_thread_overrides(&[(VIRTUALS, Some(on))], || {
                ir_prune_unconsumable_snapshots_described(
                    &mut g,
                    &[(10, 15, 1)],
                    with_map.then_some(&map),
                )
            });
            (dropped, g.safepoints.iter().any(|s| s.is_splice_state()))
        };
        assert_eq!(prune("1", true), (0, true), "described and admitted: kept");
        assert_eq!(prune("0", true), (1, false), "switch off: dropped as before");
        assert_eq!(prune("1", false), (1, false), "no map: dropped as before");
    }
}

#[cfg(test)]
mod r13w8_iropt7_box_hash_tests {
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

    /// `invokespecial Object.hashCode` is the identity hash; a receiver that is
    /// not a `valueOf` result is unknown; and the kill switch restores the call.
    #[test]
    fn only_a_virtual_hash_code_of_a_value_of_result_folds() {
        let mut f = fixture(1, true);
        assert_eq!(ir_fold_hash_code_of_integer_box(&mut f.g), 0);
        assert!(matches!(f.g.nodes[f.hash as usize].op, Op::Call { .. }));
        let mut f = fixture(0, false);
        assert_eq!(ir_fold_hash_code_of_integer_box(&mut f.g), 0);
        assert!(matches!(f.g.nodes[f.hash as usize].op, Op::Call { .. }));
        let mut f = fixture(0, true);
        let n = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_BOX_HASH_FOLD", Some("0"))],
            || ir_fold_hash_code_of_integer_box(&mut f.g),
        );
        assert_eq!(n, 0);
        assert!(matches!(f.g.nodes[f.hash as usize].op, Op::Call { .. }));
    }

    /// `valueOf(v) == null` is a constant `0` (the call never answers null),
    /// under the same switch.
    #[test]
    fn a_value_of_result_is_never_null() {
        let mut f = fixture(1, true);
        assert_eq!(ir_fold_null_checks_on_fresh_allocations(&mut f.g), 1);
        assert_eq!(f.g.nodes[f.null_cmp as usize].op, Op::Dead);
        let mut f = fixture(1, true);
        let n = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_BOX_HASH_FOLD", Some("0"))],
            || ir_fold_null_checks_on_fresh_allocations(&mut f.g),
        );
        assert_eq!(n, 0);
        assert!(matches!(f.g.nodes[f.null_cmp as usize].op, Op::Cmp(_)));
    }
}

#[cfg(test)]
mod r13w10_iropt8_long_box_hash_tests {
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

    /// A site the verifier typed as another box never folds on a `Long`, and
    /// the Long switch (or the box-hash switch) keeps the call.
    #[test]
    fn the_long_fold_has_its_own_switch() {
        let (mut g, _, hash, _) = fixture("java/lang/Integer");
        assert_eq!(ir_fold_hash_code_of_integer_box(&mut g), 0);
        assert!(matches!(g.nodes[hash as usize].op, Op::Call { .. }));
        for switch in ["CRATONVM_JIT_IR_LONG_BOX_HASH_FOLD", "CRATONVM_JIT_IR_BOX_HASH_FOLD"] {
            let (mut g, _, hash, _) = fixture("java/lang/Object");
            let n = cratonvm_types::flags::with_thread_overrides(&[(switch, Some("0"))], || {
                ir_fold_hash_code_of_integer_box(&mut g)
            });
            assert_eq!(n, 0, "{switch}");
            assert!(matches!(g.nodes[hash as usize].op, Op::Call { .. }));
        }
    }
}

#[cfg(test)]
mod r14w3_calls_string_literal_hash_tests {
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

    /// `invokespecial`, another declared class, a receiver that is not the
    /// literal, a site the map does not name, an empty map, and the kill
    /// switch all keep the call.
    #[test]
    fn only_a_virtual_hash_code_of_a_named_literal_folds() {
        let kept = |mut f: Fixture, map: &HashMap<(u32, u16), i32>| {
            assert_eq!(ir_fold_hash_code_of_string_literal(&mut f.g, map), 0);
            assert!(matches!(f.g.nodes[f.hash as usize].op, Op::Call { .. }));
        };
        kept(fixture("java/lang/String", 1, true), &hashes());
        kept(fixture("java/lang/Integer", 0, true), &hashes());
        kept(fixture("java/lang/String", 0, false), &hashes());
        kept(
            fixture("java/lang/String", 0, true),
            &HashMap::from([((7u32, 4u16), HASH)]),
        );
        kept(fixture("java/lang/String", 0, true), &HashMap::new());
        let mut f = fixture("java/lang/String", 0, true);
        let n = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_STRING_LITERAL_HASH_FOLD", Some("0"))],
            || ir_fold_hash_code_of_string_literal(&mut f.g, &hashes()),
        );
        assert_eq!(n, 0);
        assert!(matches!(f.g.nodes[f.hash as usize].op, Op::Call { .. }));
    }

    /// `"literal" == null` is a constant `0`, with or without a hash map, and
    /// the kill switch keeps the compare.
    #[test]
    fn a_literal_is_never_null() {
        let mut f = fixture("java/lang/String", 1, true);
        assert_eq!(ir_fold_null_checks_on_fresh_allocations(&mut f.g), 1);
        assert_eq!(f.g.nodes[f.null_cmp as usize].op, Op::Dead);
        let mut f = fixture("java/lang/String", 1, true);
        let n = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_STRING_LITERAL_HASH_FOLD", Some("0"))],
            || ir_fold_null_checks_on_fresh_allocations_with(&mut f.g, &hashes()),
        );
        assert_eq!(n, 0);
        assert!(matches!(f.g.nodes[f.null_cmp as usize].op, Op::Cmp(_)));
    }
}

#[cfg(test)]
mod r14w4_calls2_string_literal_query_tests {
    //! Round 14 wave 4 (lane calls2, C14W3-4): `"literal".length()` /
    //! `isEmpty()` / `charAt(k)` / `equals(..)` fold, and the string access
    //! expansion's `char_count` on a literal.
    use super::*;
    use crate::ir::{Graph, IrType, MemKind, NodeId, Op};

    const QUERY: &str = "CRATONVM_JIT_IR_STRING_LITERAL_QUERY_FOLD";

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

    /// Holder 7: `#3` and `#4` are `"hi€"` (a unit above 0xFF, to pin the
    /// zero extension), `#5` is `"ho"`, `#6` is `""`.
    fn texts() -> HashMap<(u32, u16), Vec<u16>> {
        let hi: Vec<u16> = "hi\u{20ac}".encode_utf16().collect();
        HashMap::from([
            ((7u32, 3u16), hi.clone()),
            ((7, 4), hi),
            ((7, 5), "ho".encode_utf16().collect()),
            ((7, 6), Vec::new()),
        ])
    }

    #[derive(Clone, Copy)]
    enum Arg {
        None,
        Literal(u16),
        Same,
        Null,
        Int(i64),
        Param,
    }

    struct Site {
        g: Graph,
        call: NodeId,
        ret: NodeId,
    }

    /// `ldc #recv_cp` (holder 7), the argument, the call, `ireturn`.
    fn site(
        class: &'static str,
        method: &'static str,
        desc: &'static str,
        kind: u8,
        recv_cp: u16,
        arg: Arg,
    ) -> Site {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let lit = g.add(
            Op::ConstString {
                holder_class_id: 7,
                cp_idx: recv_cp,
                slot_addr: 0,
            },
            IrType::Ref,
            vec![ctrl, mem],
            Some(0),
        );
        let mut token = lit;
        let mut inputs = vec![ctrl, lit, lit];
        match arg {
            Arg::None => {}
            Arg::Literal(cp_idx) => {
                let other = g.add(
                    Op::ConstString {
                        holder_class_id: 7,
                        cp_idx,
                        slot_addr: 0,
                    },
                    IrType::Ref,
                    vec![ctrl, lit],
                    Some(2),
                );
                token = other;
                inputs.push(other);
            }
            Arg::Same => inputs.push(lit),
            Arg::Null => inputs.push(g.add(Op::Const(0), IrType::Ref, vec![], None)),
            Arg::Int(v) => inputs.push(g.add(Op::Const(v), IrType::Int, vec![], None)),
            Arg::Param => inputs.push(g.add(Op::Param(0), IrType::Ref, vec![], None)),
        }
        inputs[1] = token;
        let call = g.add(
            Op::Call {
                info_ptr: info(class, method, desc, kind),
            },
            IrType::Int,
            inputs,
            Some(5),
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, call], Some(8));
        g.entry = start;
        g.exit = ret;
        Site { g, call, ret }
    }

    fn folds_to(mut s: Site, want: i64) {
        assert_eq!(ir_fold_string_literal_queries(&mut s.g, &texts()), 1);
        assert_eq!(s.g.nodes[s.call as usize].op, Op::Dead);
        let c = s.g.nodes[s.ret as usize].inputs[1];
        assert_eq!(s.g.nodes[c as usize].op, Op::Const(want));
        assert_eq!(s.g.nodes[c as usize].ty, IrType::Int);
    }

    fn kept(mut s: Site) {
        assert_eq!(ir_fold_string_literal_queries(&mut s.g, &texts()), 0);
        assert!(matches!(s.g.nodes[s.call as usize].op, Op::Call { .. }));
    }

    const S: &str = "java/lang/String";
    const CS: &str = "java/lang/CharSequence";
    const EQ: &str = "(Ljava/lang/Object;)Z";

    #[test]
    fn literal_queries_fold_to_their_answers() {
        folds_to(site(S, "length", "()I", 0, 3, Arg::None), 3);
        folds_to(site(CS, "length", "()I", 2, 3, Arg::None), 3);
        folds_to(site(S, "isEmpty", "()Z", 0, 3, Arg::None), 0);
        folds_to(site(S, "isEmpty", "()Z", 0, 6, Arg::None), 1);
        folds_to(site(S, "charAt", "(I)C", 0, 3, Arg::Int(1)), i64::from(b'i'));
        folds_to(site(S, "charAt", "(I)C", 0, 3, Arg::Int(2)), 0x20ac);
        folds_to(site(S, "equals", EQ, 0, 3, Arg::Literal(4)), 1);
        folds_to(site("java/lang/Object", "equals", EQ, 0, 3, Arg::Literal(5)), 0);
        folds_to(site(S, "equals", EQ, 0, 3, Arg::Same), 1);
        folds_to(site(S, "equals", EQ, 0, 3, Arg::Null), 0);
    }

    /// An out-of-range or unknown index, an unknown argument, a receiver the
    /// map does not name, another kind or class, an empty map and the kill
    /// switch keep the call.
    #[test]
    fn only_answers_fixed_by_the_literal_fold() {
        kept(site(S, "charAt", "(I)C", 0, 3, Arg::Int(3)));
        kept(site(S, "charAt", "(I)C", 0, 3, Arg::Int(-1)));
        kept(site(S, "charAt", "(I)C", 0, 3, Arg::Param));
        kept(site(S, "equals", EQ, 0, 3, Arg::Param));
        kept(site(S, "length", "()I", 0, 9, Arg::None));
        kept(site(S, "length", "()I", 1, 3, Arg::None));
        kept(site("java/lang/StringBuilder", "length", "()I", 0, 3, Arg::None));
        kept(site(S, "equals", EQ, 2, 3, Arg::Literal(4)));
        let mut s = site(S, "length", "()I", 0, 3, Arg::None);
        assert_eq!(ir_fold_string_literal_queries(&mut s.g, &HashMap::new()), 0);
        let n = cratonvm_types::flags::with_thread_overrides(&[(QUERY, Some("0"))], || {
            ir_fold_string_literal_queries(&mut s.g, &texts())
        });
        assert_eq!(n, 0);
        assert!(matches!(s.g.nodes[s.call as usize].op, Op::Call { .. }));
    }

    /// The string access expansion's `value.length >>> coder` on a literal
    /// becomes the unit count; the loads stay. The same shape on a parameter
    /// is left alone.
    #[test]
    fn the_expansion_count_of_a_literal_folds() {
        for (literal, want) in [(true, Some(3i64)), (false, None)] {
            let mut g = Graph::empty(0);
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
            let recv = if literal {
                g.add(
                    Op::ConstString {
                        holder_class_id: 7,
                        cp_idx: 3,
                        slot_addr: 0,
                    },
                    IrType::Ref,
                    vec![ctrl, mem],
                    Some(0),
                )
            } else {
                g.add(Op::Param(0), IrType::Ref, vec![], None)
            };
            let coder_index = g.add(Op::Const(1), IrType::Int, vec![], None);
            let coder = g.add(
                Op::Load(MemKind::Int),
                IrType::Int,
                vec![ctrl, mem, recv, coder_index],
                Some(2),
            );
            let value_index = g.add(Op::Const(0), IrType::Int, vec![], None);
            let value = g.add(
                Op::Load(MemKind::Ref),
                IrType::Ref,
                vec![ctrl, coder, recv, value_index],
                Some(2),
            );
            let byte_len = g.add(
                Op::ArrayLength,
                IrType::Int,
                vec![ctrl, value, value],
                Some(2),
            );
            let count = g.add(Op::UShr, IrType::Int, vec![byte_len, coder], Some(2));
            let ret = g.add(Op::Return, IrType::Void, vec![ctrl, count], Some(5));
            g.entry = start;
            g.exit = ret;
            let n = ir_fold_string_literal_queries(&mut g, &texts());
            let out = g.nodes[ret as usize].inputs[1];
            match want {
                Some(len) => {
                    assert_eq!(n, 1);
                    assert_eq!(g.nodes[count as usize].op, Op::Dead);
                    assert_eq!(g.nodes[out as usize].op, Op::Const(len));
                    assert!(matches!(
                        g.nodes[value as usize].op,
                        Op::Load(MemKind::Ref)
                    ));
                }
                None => {
                    assert_eq!(n, 0);
                    assert_eq!(out, count);
                }
            }
        }
    }
}

#[cfg(test)]
mod r13w12_snapliv_tests {
    //! Round 13 wave 12 (lane snapliv): the dead-local snapshot prune in a
    //! method with an exception table and at loop headers (W2-1), and per
    //! scope in a chain snapshot (CH5-1).
    use super::*;
    use crate::ir::{
        Graph, IrInlineFrameSites, IrInlineSite, IrType, NodeId, Op, SafepointSnapshot,
        SpliceScopeChain, SpliceScopeFrame, NO_NODE,
    };
    use std::collections::HashMap;

    const CHAIN: &str = "CRATONVM_JIT_IR_PRUNE_CHAIN_SNAPSHOT_LOCALS";
    const HANDLERS: &str = "CRATONVM_JIT_IR_PRUNE_HANDLER_METHOD_LOCALS";
    const HEADERS: &str = "CRATONVM_JIT_IR_PRUNE_LOOP_HEADER_LOCALS";
    /// The defaults, pinned.
    const DEFAULTS: &[(&str, Option<&str>)] = &[
        (CHAIN, Some("1")),
        (HANDLERS, Some("1")),
        (HEADERS, Some("1")),
    ];
    const ALL_ON: &[(&str, Option<&str>)] = &[
        (CHAIN, Some("1")),
        (HANDLERS, Some("1")),
        (HEADERS, Some("1")),
    ];

    fn values(g: &mut Graph, n: usize) -> Vec<NodeId> {
        (0..n)
            .map(|i| g.add(Op::Const(i as i64 + 1), IrType::Int, vec![], None))
            .collect()
    }

    fn site(base: usize, code_len: usize, key: &str) -> IrInlineSite {
        IrInlineSite {
            base,
            code_len,
            num_args: 1,
            max_locals: 2,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: false,
            method_key: key.to_string(),
            class_id: 0,
        }
    }

    fn scope(
        key: &str,
        bci: u32,
        code_len: u32,
        max_locals: u32,
        locals_len: u32,
    ) -> SpliceScopeFrame {
        SpliceScopeFrame {
            method_key: key.to_string(),
            bci,
            code_len,
            max_locals,
            locals_len,
            stack_len: 0,
        }
    }

    fn chain_snapshot(
        bci: usize,
        locals: Vec<NodeId>,
        stack: Vec<NodeId>,
        scopes: Vec<SpliceScopeFrame>,
    ) -> SafepointSnapshot {
        let mut sp = SafepointSnapshot::new(bci, locals, stack, Vec::new());
        sp.splice_scope = Some(Box::new(SpliceScopeChain { scopes }));
        sp
    }

    fn prune(
        g: &mut Graph,
        code: &[u8],
        code_len: usize,
        handlers: &[(usize, usize, usize)],
        bodies: &IrInlineFrameSites,
        edits: &[(&str, Option<&str>)],
    ) -> usize {
        cratonvm_types::flags::with_thread_overrides(edits, || {
            ir_prune_dead_snapshot_locals_scoped(g, code, code_len, handlers, bodies, true)
        })
    }

    /// `static int f(int x) { int dead = 1; return g(x) + x; }`:
    /// `0 iconst_1  1 istore_1  2 iload_0  3 invokestatic g  6 iload_0  7 iadd
    /// 8 ireturn`.
    const F: [u8; 9] = [0x04, 0x3c, 0x1a, 0xb8, 0x00, 0x01, 0x1a, 0x60, 0xac];
    /// `static int g(int a) { int t = a; return a; }`:
    /// `0 iload_0  1 istore_1  2 iload_0  3 ireturn`.
    const G: [u8; 4] = [0x1a, 0x3c, 0x1a, 0xac];

    /// `g` spliced into `f` at pc 3 (base 9); the flat frame at the invoke and
    /// the chain frame at `g`'s bci 2 (combined pc 11). Returns the graph, the
    /// combined buffer, the body table and `[a, t, x, dead]`.
    fn one_level(g_key: &str, g_bci: u32) -> (Graph, Vec<u8>, IrInlineFrameSites, [NodeId; 4]) {
        let mut code = F.to_vec();
        code.extend_from_slice(&G);
        code.extend_from_slice(&[0, 0]);
        let mut sites = HashMap::new();
        sites.insert(3usize, site(9, 4, "C.g:(I)I"));
        let bodies = IrInlineFrameSites::from_sites(&sites);
        let mut g = Graph::empty(0);
        let v = values(&mut g, 4);
        let (a, t, x, dead) = (v[0], v[1], v[2], v[3]);
        g.push_safepoint(SafepointSnapshot::new(
            3,
            vec![x, dead],
            vec![x],
            Vec::new(),
        ));
        g.push_safepoint(chain_snapshot(
            11,
            vec![a, t],
            vec![x, dead],
            vec![scope(g_key, g_bci, 4, 2, 0), scope("", 3, 0, 0, 2)],
        ));
        (g, code, bodies, [a, t, x, dead])
    }

    /// CH5-1: the callee scope is cut by `g`'s liveness at its own bci, the
    /// caller scope (in `stack`) by `f`'s at its invoke. Before, the chain
    /// frame kept all four.
    #[test]
    fn a_chain_snapshot_is_cut_scope_by_scope() {
        let (mut g, code, bodies, [a, _, x, _]) = one_level("C.g:(I)I", 2);
        let n = prune(&mut g, &code, F.len(), &[], &bodies, DEFAULTS);
        assert_eq!(
            g.safepoints[1].locals,
            vec![a, NO_NODE],
            "g's `t` is never read again; `a` is read at bci 2"
        );
        assert_eq!(
            g.safepoints[1].stack,
            vec![x, NO_NODE],
            "f reads `x` after the invoke and `dead` never"
        );
        assert_eq!(g.safepoints[0].locals, vec![x, NO_NODE]);
        assert_eq!(
            g.safepoints[0].stack,
            vec![x],
            "an operand stack is never cut"
        );
        assert_eq!(n, 3);
    }

    #[test]
    fn the_chain_kill_switch_keeps_every_scope() {
        let (mut g, code, bodies, [a, t, x, dead]) = one_level("C.g:(I)I", 2);
        let off = &[
            (CHAIN, Some("0")),
            (HANDLERS, Some("1")),
            (HEADERS, Some("0")),
        ];
        assert_eq!(
            prune(&mut g, &code, F.len(), &[], &bodies, off),
            1,
            "the flat frame only"
        );
        assert_eq!(g.safepoints[1].locals, vec![a, t]);
        assert_eq!(g.safepoints[1].stack, vec![x, dead]);
    }

    /// A chain read against the wrong body would cut by the wrong liveness:
    /// any disagreement with the body table leaves the chain whole.
    #[test]
    fn a_chain_that_disagrees_with_the_body_table_is_not_cut() {
        for (key, bci) in [("C.h:(I)I", 2), ("C.g:(I)I", 1)] {
            let (mut g, code, bodies, [a, t, x, dead]) = one_level(key, bci);
            prune(&mut g, &code, F.len(), &[], &bodies, DEFAULTS);
            assert_eq!(g.safepoints[1].locals, vec![a, t], "{key} @{bci}");
            assert_eq!(g.safepoints[1].stack, vec![x, dead], "{key} @{bci}");
        }
        // No body table at all (the unit tests' entry point).
        let (mut g, code, _, [a, t, x, dead]) = one_level("C.g:(I)I", 2);
        prune(
            &mut g,
            &code,
            F.len(),
            &[],
            &IrInlineFrameSites::default(),
            DEFAULTS,
        );
        assert_eq!(g.safepoints[1].locals, vec![a, t]);
        assert_eq!(g.safepoints[1].stack, vec![x, dead]);
    }

    /// Three scopes: `f2` calls `a` at pc 1, `a` calls `b` at its pc 3. The
    /// middle scope is cut by `a`'s OWN liveness at ITS invoke.
    #[test]
    fn a_nested_chain_cuts_its_middle_scope_by_its_own_body() {
        // f2: 0 iload_0  1 invokestatic a  4 ireturn
        let f2: [u8; 5] = [0x1a, 0xb8, 0x00, 0x01, 0xac];
        // a: 0 iload_0  1 istore_1  2 iload_0  3 invokestatic b  6 iload_0
        //    7 iadd  8 ireturn
        let body_a: [u8; 9] = [0x1a, 0x3c, 0x1a, 0xb8, 0x00, 0x02, 0x1a, 0x60, 0xac];
        let mut code = f2.to_vec();
        code.extend_from_slice(&body_a);
        code.extend_from_slice(&G);
        code.extend_from_slice(&[0, 0]);
        let mut sites = HashMap::new();
        sites.insert(1usize, site(5, 9, "C.a:(I)I"));
        sites.insert(8usize, site(14, 4, "C.b:(I)I"));
        let bodies = IrInlineFrameSites::from_sites(&sites);
        let mut g = Graph::empty(0);
        let v = values(&mut g, 5);
        let (b0, b1, a0, a1, p) = (v[0], v[1], v[2], v[3], v[4]);
        g.push_safepoint(chain_snapshot(
            16,
            vec![b0, b1],
            vec![a0, a1, p],
            vec![
                scope("C.b:(I)I", 2, 4, 2, 0),
                scope("C.a:(I)I", 3, 9, 2, 2),
                scope("", 1, 0, 0, 1),
            ],
        ));
        assert_eq!(prune(&mut g, &code, f2.len(), &[], &bodies, DEFAULTS), 3);
        assert_eq!(g.safepoints[0].locals, vec![b0, NO_NODE]);
        assert_eq!(
            g.safepoints[0].stack,
            vec![a0, NO_NODE, NO_NODE],
            "a reads a0 after its invoke; a1 is never read; f2 reads nothing after its invoke"
        );
    }

    /// `0 iconst_5  1 istore_1  2 iload_0  3 iload_0  4 idiv  5 ireturn` with
    /// a handler at 6 for `[2, 6)`: `6 pop  7 iload_1  8 ireturn`. Local 1 is
    /// read ONLY by the handler.
    const H: [u8; 9] = [0x08, 0x3c, 0x1a, 0x1a, 0x6c, 0xac, 0x57, 0x1b, 0xac];

    fn handler_graph() -> (Graph, [NodeId; 2]) {
        let mut g = Graph::empty(0);
        let v = values(&mut g, 2);
        let (x, t) = (v[0], v[1]);
        g.push_safepoint(SafepointSnapshot::new(
            4,
            vec![x, t],
            vec![x, x],
            Vec::new(),
        ));
        g.push_safepoint(SafepointSnapshot::new(8, vec![x, t], vec![t], Vec::new()));
        (g, [x, t])
    }

    /// W2-1: a method with an exception table is pruned, with the handler's
    /// reads kept. Without the exception edge `t` would be dead at the `idiv`
    /// -- which is why such a method used to be skipped whole.
    #[test]
    fn a_handler_keeps_the_local_it_reads() {
        let (mut g, [_, t]) = handler_graph();
        let bodies = IrInlineFrameSites::default();
        assert_eq!(
            prune(&mut g, &H, H.len(), &[(2, 6, 6)], &bodies, DEFAULTS),
            3
        );
        assert_eq!(
            g.safepoints[0].locals,
            vec![NO_NODE, t],
            "the idiv can throw into the handler, which reads local 1"
        );
        assert_eq!(g.safepoints[1].locals, vec![NO_NODE, NO_NODE]);
    }

    #[test]
    fn the_handler_kill_switch_and_a_malformed_row_prune_nothing() {
        let bodies = IrInlineFrameSites::default();
        let off = &[
            (CHAIN, Some("1")),
            (HANDLERS, Some("0")),
            (HEADERS, Some("0")),
        ];
        let (mut g, _) = handler_graph();
        assert_eq!(prune(&mut g, &H, H.len(), &[(2, 6, 6)], &bodies, off), 0);
        // A handler past the code, a handler AT the code length, an empty
        // range, a range past the code.
        for row in [(2, 6, 99), (2, 6, 9), (6, 2, 6), (2, 10, 6)] {
            let (mut g, _) = handler_graph();
            assert_eq!(
                prune(&mut g, &H, H.len(), &[row], &bodies, DEFAULTS),
                0,
                "{row:?}"
            );
        }
    }

    /// `0 iload_0  1 ifeq 11  4 iload_0  5 istore_1  6 iload_1  7 pop
    /// 8 goto 0  11 return`: local 1 is written before it is read on every
    /// iteration, so it is dead at the header (0) and at 4.
    const LOOP: [u8; 12] = [
        0x1a, 0x99, 0x00, 0x0a, 0x1a, 0x3c, 0x1b, 0x57, 0xa7, 0xff, 0xf8, 0xb1,
    ];

    fn loop_graph() -> (Graph, [NodeId; 2]) {
        let mut g = Graph::empty(0);
        let v = values(&mut g, 2);
        let (x, t) = (v[0], v[1]);
        g.push_safepoint(SafepointSnapshot::new(
            0,
            vec![x, t],
            Vec::new(),
            Vec::new(),
        ));
        g.push_safepoint(SafepointSnapshot::new(
            4,
            vec![x, t],
            Vec::new(),
            Vec::new(),
        ));
        (g, [x, t])
    }

    #[test]
    fn loop_headers_are_cut_only_under_their_switch() {
        let bodies = IrInlineFrameSites::default();
        // The header switch is default ON since round 14 wave 2; `0` keeps the
        // header as the OSR seed list.
        const HEADERS_OFF: &[(&str, Option<&str>)] =
            &[(CHAIN, Some("1")), (HANDLERS, Some("1")), (HEADERS, Some("0"))];
        let (mut g, [x, t]) = loop_graph();
        assert_eq!(prune(&mut g, &LOOP, LOOP.len(), &[], &bodies, HEADERS_OFF), 1);
        assert_eq!(
            g.safepoints[0].locals,
            vec![x, t],
            "the header is the OSR seed list"
        );
        assert_eq!(g.safepoints[1].locals, vec![x, NO_NODE]);

        let (mut g, [x, _]) = loop_graph();
        assert_eq!(prune(&mut g, &LOOP, LOOP.len(), &[], &bodies, ALL_ON), 2);
        assert_eq!(g.safepoints[0].locals, vec![x, NO_NODE]);
    }

    /// Under the header switch, a value a snapshot the prune could not analyse
    /// still names stays named at the header.
    #[test]
    fn a_header_keeps_a_value_an_unanalysed_chain_still_names() {
        let (mut g, [x, t]) = loop_graph();
        g.push_safepoint(chain_snapshot(
            30,
            vec![t],
            vec![x],
            vec![scope("C.g:(I)I", 0, 4, 1, 0), scope("", 0, 0, 0, 1)],
        ));
        prune(
            &mut g,
            &LOOP,
            LOOP.len(),
            &[],
            &IrInlineFrameSites::default(),
            ALL_ON,
        );
        assert_eq!(g.safepoints[0].locals, vec![x, t]);
        assert_eq!(
            g.safepoints[1].locals,
            vec![x, NO_NODE],
            "not a header: cut as before"
        );
        assert_eq!(
            g.safepoints[2].locals,
            vec![t],
            "no body table: the chain is not cut"
        );
    }

    /// A relocated body's `tableswitch` padding is measured from the wrong
    /// origin, so it proves nothing there; in the method's own code it does.
    #[test]
    fn a_relocated_switch_proves_nothing() {
        // 0 iload_0  1 tableswitch (pad 2..4) default 20, low 0, high 0,
        // [20]  20 return
        let code: [u8; 21] = [
            0x1a, 0xaa, 0x00, 0x00, 0x00, 0x00, 0x00, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x13, 0xb1,
        ];
        assert!(body_local_liveness(&code, code.len(), 1, &[], false).is_some());
        assert!(body_local_liveness(&code, code.len(), 1, &[], true).is_none());
    }
}

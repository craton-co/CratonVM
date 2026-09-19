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
//!   2. PLAN -- `plan_scalar_replacement` and [`build_scalar_replacement_map`]
//!      turn an EA result into a concrete per-victim rewrite, including the
//!      deopt-describability check that decides whether an elided object can be
//!      reconstituted at an exit.
//!   3. OUT -- [`apply_ea_to_ir`] / [`apply_ea_to_ir_pinned`] mutate the real
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
        if matches!(ea.nodes[ea_id].op, escape_analysis::Op::Load(_)) {
            let token = ir::memory_token_slot(ir_node).and_then(|slot| ir_node.inputs.get(slot));
            if let Some(&tok) = token {
                let ea_tok = map_id(tok);
                if ea_tok != usize::MAX {
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
        ir::Op::Merge => EaOp::Merge,
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
        ir::Op::Dead => EaOp::Dead,
        // All other IR ops (Region, Proj, ConstF, conversions, bitwise,
        // Cmp, Div, Rem, Neg, etc.) have no EA-specific behaviour.
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
// DE-DUPLICATION NOTE: `ir_optimize::memory_token_slot` (ir_optimize.rs:1682)
// and `ir_optimize::is_memory_token_slot` (ir_optimize.rs:1708) are the
// reference predicates and the two below are equivalent to them, but both are
// private to that module. Changing those two declarations to `pub(crate) fn`
// lets `ea_memory_token_slot` / `ea_is_memory_token_slot` be deleted outright
// and the originals imported instead. The *kill* helper,
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
/// DE-DUPLICATION NOTE: this is `ir_verify::control_input_indices`
/// (ir_verify.rs:482), which is private to that module. Making that declaration
/// `pub(crate) fn` lets this one be deleted and the original imported instead;
/// the two must stay in step because a control edge this function does not see
/// is a path `ea_cold_control_nodes` will not consider when deciding a merge is
/// cold.
fn ea_control_preds(node: &ir::Node) -> &[ir::NodeId] {
    match node.op {
        // A merge takes one control edge per predecessor.
        ir::Op::Merge | ir::Op::Region => node.inputs.as_slice(),
        // Everything else pins control at input 0 (when it has one at all).
        ir::Op::Return
        | ir::Op::Throw
        | ir::Op::If
        | ir::Op::Proj(_)
        | ir::Op::Guard { .. }
        | ir::Op::Load(_)
        | ir::Op::Store(_)
        | ir::Op::ArrayLoad(_)
        | ir::Op::ArrayStore(_)
        | ir::Op::New { .. }
        | ir::Op::NewArray { .. }
        | ir::Op::Call { .. }
        | ir::Op::ConstString { .. }
        | ir::Op::ConstClass { .. }
        | ir::Op::LoadStatic { .. }
        | ir::Op::LambdaIntToDouble => &node.inputs[..node.inputs.len().min(1)],
        // Start, and every floating pure node (Const, Add, Cmp, Phi, …). A node
        // with no control input has no fixed position, so it is never cold.
        _ => &[],
    }
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

    // Seed: the never-taken projection of each profiled `Op::If`.
    for (idx, n) in ir_graph.nodes.iter().enumerate() {
        if n.op != ir::Op::If {
            continue;
        }
        let usually_taken = match n.bytecode_pc.and_then(|pc| hints.get(&pc)) {
            Some(&t) => t,
            None => continue, // inconclusive profile — nothing is cold here
        };
        let cold_proj = if usually_taken { 1u16 } else { 0u16 };
        let if_id = idx as ir::NodeId;
        for (pidx, p) in ir_graph.nodes.iter().enumerate() {
            if p.op == ir::Op::Proj(cold_proj) && p.inputs.first() == Some(&if_id) {
                cold.insert(pidx as ir::NodeId);
            }
        }
    }
    if cold.is_empty() {
        return cold;
    }

    // Forward closure. Bounded by the node count: each round adds at least one
    // node or stops, and a node is never removed.
    for _ in 0..ir_graph.nodes.len() {
        let mut changed = false;
        for (idx, n) in ir_graph.nodes.iter().enumerate() {
            let id = idx as ir::NodeId;
            if n.op == ir::Op::Dead || cold.contains(&id) {
                continue;
            }
            let preds = ea_control_preds(n);
            if preds.is_empty() {
                continue;
            }
            if preds.iter().all(|p| cold.contains(p)) {
                cold.insert(id);
                changed = true;
            }
        }
        if !changed {
            break;
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
/// naming the previous writer — rather than a value the node reads. Mirrors
/// `ir_optimize::is_memory_token_slot`; a memory-typed φ is the one op whose
/// token edges are every value input rather than a single slot.
pub(crate) fn ea_is_memory_token_slot(node: &ir::Node, idx: usize) -> bool {
    if matches!(node.op, ir::Op::Phi) {
        return node.ty == ir::IrType::Memory && idx >= 1;
    }
    ea_memory_token_slot(node) == Some(idx)
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
fn ea_snapshot_names(ir_graph: &ir::Graph, id: ir::NodeId) -> bool {
    ir_graph.safepoints.iter().any(|sp| {
        sp.locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .any(|&v| v == id)
    })
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
// 1. `ir_prune_dead_snapshot_locals` — a local the bytecode cannot read again
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
    let snapshot_bcis: std::collections::HashSet<usize> =
        ir_graph.safepoints.iter().map(|sp| sp.bci).collect();
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
        if let ir::Op::Guard { bci } = n.op {
            out.insert(resume(bci));
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
    ir_graph.safepoints.iter().enumerate().any(|(si, sp)| {
        (consumable.contains(&sp.bci) || claimed.contains(&(si as u32)))
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
pub(crate) fn ir_prune_unconsumable_snapshots(
    ir_graph: &mut ir::Graph,
    spliced_ranges: &[(usize, usize, usize)],
) -> usize {
    let nothing_ignored: std::collections::HashSet<ir::NodeId> = std::collections::HashSet::new();
    let Some(consumable) =
        ir_consumable_snapshot_bcis(ir_graph, Some(spliced_ranges), &nothing_ignored)
    else {
        return 0;
    };
    let claimed = ir_claimed_snapshots(ir_graph, &nothing_ignored);
    let keep: Vec<bool> = ir_graph
        .safepoints
        .iter()
        .enumerate()
        .map(|(si, sp)| consumable.contains(&sp.bci) || claimed.contains(&(si as u32)))
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

/// Decode `code[..code_len]` for [`ir_prune_dead_snapshot_locals`]. `None` for
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
/// Conservative in every direction it can be:
///
/// * the CALLER must not call this for a method with an exception table — a
///   handler reads locals the normal flow does not, and handler code is not
///   walked here;
/// * a loop header (any backward-branch target) is left untouched: an
///   optimizing OSR entry seeds this tier's frame from the snapshot there and
///   refuses a value live in the graph but unnamed by it;
/// * any construct liveness cannot model (subroutines, `wide ret`, a malformed
///   table, a local index past the snapshot's width) prunes nothing, and so does
///   a fixed point that fails to converge in `MAX_ROUNDS`.
///
/// Only locals are cut. The operand stack is always fully live.
pub(crate) fn ir_prune_dead_snapshot_locals(
    ir_graph: &mut ir::Graph,
    code: &[u8],
    code_len: usize,
) -> usize {
    const MAX_LOCALS: usize = 1024;
    const MAX_ROUNDS: usize = 64;
    if ir_graph.safepoints.is_empty() || code_len == 0 || code_len > code.len() {
        return 0;
    }
    let num_locals = ir_graph
        .safepoints
        .iter()
        .map(|sp| sp.locals.len())
        .max()
        .unwrap_or(0);
    if num_locals == 0 || num_locals > MAX_LOCALS {
        return 0;
    }
    let Some(insns) = decode_for_local_liveness(code, code_len) else {
        return 0;
    };
    let mut index_of: Vec<usize> = vec![usize::MAX; code_len];
    for (i, insn) in insns.iter().enumerate() {
        index_of[insn.pc] = i;
    }
    let mut loop_headers: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for insn in &insns {
        for &s in &insn.succs {
            // A branch into the middle of an instruction: the decode and the
            // method disagree, so prove nothing.
            if index_of.get(s).copied().unwrap_or(usize::MAX) == usize::MAX {
                return 0;
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
            return 0;
        }
    }
    let words = num_locals.div_ceil(64);
    let n = insns.len();
    let mut live_in: Vec<u64> = vec![0; n * words];
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
    if !converged {
        return 0;
    }
    let mut cleared = 0usize;
    for sp in ir_graph.safepoints.iter_mut() {
        if sp.bci >= code_len || loop_headers.contains(&sp.bci) {
            continue;
        }
        let i = index_of[sp.bci];
        if i == usize::MAX {
            continue;
        }
        let row = &live_in[i * words..(i + 1) * words];
        for (k, slot) in sp.locals.iter_mut().enumerate() {
            // A plain write is fine here: it REMOVES a reference, and the
            // def-use lists tolerate a stale superset entry (see `UseLists`).
            if *slot != ir::NO_NODE && row[k / 64] & (1u64 << (k % 64)) == 0 {
                *slot = ir::NO_NODE;
                cleared += 1;
            }
        }
    }
    cleared
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
pub(crate) fn ir_fold_null_checks_on_fresh_allocations(ir_graph: &mut ir::Graph) -> usize {
    let box_folds = ir_fold_unbox_of_fresh_box(ir_graph);
    if box_folds > 0 && ir_stage_reporting() {
        eprintln!("[ir] box-unbox fold: {box_folds} valueOf site(s) folded into their unbox");
    }
    let mut folds: Vec<(ir::NodeId, i64)> = Vec::new();
    {
        let g: &ir::Graph = ir_graph;
        let is_fresh = |id: ir::NodeId| {
            g.node_opt(id)
                .is_some_and(|n| matches!(n.op, ir::Op::New { .. } | ir::Op::NewArray { .. }))
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

/// How [`apply_ea_to_ir`] retires one node.
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
struct EaScalarPlan {
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
pub(crate) fn narrow_array_value_fits(
    graph: &ir::Graph,
    value: ir::NodeId,
    load_kind: ir::MemKind,
    array_atype: Option<u8>,
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
        _ => false,
    }
}

/// Decide what may be done to one scalar-replacement candidate. Pure: it never
/// mutates `ir_graph`. `None` refuses the object entirely (it stays allocated,
/// its stores stay, its loads stay).
fn plan_scalar_replacement(
    ir_graph: &ir::Graph,
    reverse_map: &HashMap<escape_analysis::NodeId, ir::NodeId>,
    info: &escape_analysis::ScalarReplacementInfo,
    deopt_descriptor_available: bool,
    descriptor_pinned: &std::collections::HashSet<ir::NodeId>,
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
    // BLOCKED wins over RESCUED: an allocation that hit both gates and failed
    // either one is kept, so reporting it as engagement would be a lie in the
    // direction that flatters the flag.
    if descriptor_blocked {
        cratonvm_types::scalar_deopt_census::note_blocked();
    } else if descriptor_rescued {
        cratonvm_types::scalar_deopt_census::note_rescued();
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

/// [`apply_ea_to_ir`] with the set of allocations an earlier round's deopt
/// descriptor already names as a field value, and a count of the plans it
/// applied.
///
/// The count is the iteration's stopping condition: a round that applies
/// nothing has reached the fixed point and the next round would see the same
/// graph.
pub(crate) fn apply_ea_to_ir_pinned(
    ir_graph: &mut ir::Graph,
    id_map: &[escape_analysis::NodeId],
    ea_result: &escape_analysis::EscapeAnalysisResult,
    descriptor_pinned: &std::collections::HashSet<ir::NodeId>,
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
    // `build_scalar_replacement_map` (the EA block in `try_compile_inner`); if
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
    let mut plans: Vec<EaScalarPlan> = Vec::new();
    for info in &ea_result.scalar_replaceable {
        if let Some(plan) = plan_scalar_replacement(
            ir_graph,
            &reverse_map,
            info,
            deopt_descriptor_available,
            descriptor_pinned,
        ) {
            plans.push(plan);
        }
    }
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

        for n in ir_graph.nodes.iter_mut() {
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
                if is_token {
                    // An ordering edge follows the CHAIN, never the data
                    // replacement — the bug the old blanket rewrite had.
                    if let Some(t) = token {
                        *inp = t;
                    }
                } else if let EaVictimKind::Forwarded(v) = kind {
                    *inp = v;
                }
            }
        }
        // Safepoint snapshots are references too: a slot naming a forwarded load
        // must follow the value, exactly as `ir::Graph::replace_all_uses` does.
        // A slot naming an eliminated `Op::New` is deliberately LEFT in place —
        // that slot is the "materialise this virtual object" descriptor, and
        // planning has already refused the elision when no descriptor will exist.
        if let EaVictimKind::Forwarded(v) = kind {
            forwarded.insert(victim, v);
            for sp in ir_graph.safepoints.iter_mut() {
                for slot in sp
                    .locals
                    .iter_mut()
                    .chain(sp.stack.iter_mut())
                    .chain(sp.monitors.iter_mut())
                {
                    if *slot == victim {
                        *slot = v;
                    }
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
            for ir_lock in group {
                let token = ir_graph
                    .node_opt(ir_lock)
                    .and_then(|n| ea_memory_token_slot(n).and_then(|s| n.input_opt(s)));
                if let Some(t) = token {
                    for node in ir_graph.nodes.iter_mut() {
                        if node.op == ir::Op::Dead {
                            continue;
                        }
                        let slot = ea_memory_token_slot(node);
                        if let Some(s) = slot {
                            if node.input_opt(s) == Some(ir_lock) {
                                if let Some(cell) = node.inputs.as_mut_slice().get_mut(s) {
                                    *cell = t;
                                }
                            }
                        }
                    }
                }
                let idx = ir_lock as usize;
                ir_graph.nodes[idx].op = ir::Op::Dead;
                ir_graph.nodes[idx].inputs.clear();
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

/// Apply `escape_analysis`'s lock-coarsening plans. `CRATONVM_JIT_LOCK_COARSEN=1`.
///
/// Default OFF. The plans have been computed on every EA run since the pass
/// landed and never applied; this is the switch that makes them reachable. It
/// is not on by default because coarsening holds a lock for longer, which is a
/// throughput trade nobody here has measured — see the call site.
fn lock_coarsening_enabled() -> bool {
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
pub(crate) fn build_scalar_replacement_map(
    ir_graph: &ir::Graph,
    id_map: &[escape_analysis::NodeId],
    ea_result: &escape_analysis::EscapeAnalysisResult,
    descriptor_pinned: &std::collections::HashSet<ir::NodeId>,
) -> ir_lower::ScalarReplacementMap {
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
        ) else {
            continue;
        };
        // `apply_ea_to_ir_pinned` drops a whole plan when one of its zero
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
                if let Some(v) = v {
                    forwarded.insert(l, v);
                }
            }
        }
        if plan.elide_alloc {
            elided_infos.push(info);
        }
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
            // holds. A chain ending in a zero-default load keeps naming that
            // load, exactly as before.
            for fv in vo.field_values.iter_mut().flatten() {
                let mut hops = 0usize;
                while let Some(&next) = forwarded.get(&*fv) {
                    *fv = next;
                    hops += 1;
                    if hops > forwarded.len() {
                        break;
                    }
                }
            }
            objects.insert(ir_new, vo);
        }
    }
    ir_lower::ScalarReplacementMap { objects }
}

/// The [`ir_lower::VirtualObjectInfo`] describing one scalar-replacement
/// candidate, keyed by the IR `NodeId` of its `Op::New`, or `None` when the
/// object cannot be described to the deopt producer at all.
///
/// Split out of [`build_scalar_replacement_map`] so [`plan_scalar_replacement`]
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
    for ea in &info.eliminated_stores {
        let ir_store = match reverse_map.get(ea) {
            Some(&id) => id,
            None => continue, // a store with no IR node can't have executed observably
        };
        store_ctrls.push(ctrl_of(ir_store)?);
        store_nodes.push(ir_store);
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
    match ir_verify::verify_graph(graph, phase, ir_verify::VerifyOptions::for_phase(phase)) {
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

    fn empty_graph() -> Graph {
        Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        }
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
        })) as *const JitInvokeInfo as usize
    }

    fn snapshot(
        bci: usize,
        locals: Vec<ir::NodeId>,
        stack: Vec<ir::NodeId>,
    ) -> ir::SafepointSnapshot {
        ir::SafepointSnapshot {
            bci,
            locals,
            stack,
            monitors: Vec::new(),
        }
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

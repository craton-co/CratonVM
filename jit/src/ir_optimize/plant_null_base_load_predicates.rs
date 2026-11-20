#[allow(unused_imports)]
use super::*;

/// Plant the null-base load predicates of every loop (see the section notes),
/// unless `despec` withdrew this planter's speculation at a loop's header.
/// Returns how many in-body reads were replaced by pre-header copies.
pub(in crate::ir_optimize) fn plant_null_base_load_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> usize {
    if graph.safepoints.is_empty() {
        return 0;
    }
    let dbg = false;
    let mut replaced = 0usize;
    let anchors = crate::ir::exception_edge_anchors(graph);
    // JIT round 23 wave 3c: the caller admits an OSR graph only under
    // `null_base_load_predicate_osr_enabled`.
    let osr_graph = graph.optimize_osr_route || graph.osr_compile;
    // JIT round 23 wave 4: a loop spliced from a callee, under its switch.
    let chain_on = null_base_load_predicate_chain_enabled();
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if !matches!(
            graph.node_opt(region).map(|n| &n.op),
            Some(Op::Region | Op::Merge)
        ) || entry_pred == NO_NODE
        {
            continue;
        }
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        if body.contains(&entry_pred) || loop_writes_memory(graph, &body) {
            continue;
        }
        // JIT round 23 wave 4 (lane L1): a header with no frame of the
        // compiling method, only an inlined chain -- a loop spliced from a
        // callee (`CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_CHAIN`).
        let spliced_header = chain_on && header_is_spliced(graph, region);
        // JIT round 23 wave 3c: an OSR graph's loop only when every entry
        // passes the pre-header (`null_base_load_predicate_osr_enabled`).
        // Wave 4: a spliced header is never an OSR entry; its own rule.
        let entered_through_pre_header = if spliced_header {
            spliced_loop_entered_only_through_its_pre_header(graph, region, &body)
        } else {
            loop_entered_only_through_its_pre_header(graph, region, entry_pred, &body)
        };
        if osr_graph && !entered_through_pre_header {
            if dbg {
                eprintln!(
                    "[ir-licm] null-base predicate: header {region} skipped -- an OSR entry \
                     may bypass its pre-header"
                );
            }
            continue;
        }
        let Some(entry_slot) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
            .map(|i| i + 1)
        else {
            continue;
        };
        let guards = LoopGuards::in_body(graph, &body);
        // `(read, base)`, in node order.
        let mut candidates: Vec<(NodeId, NodeId)> = body
            .iter()
            .copied()
            .filter(|&id| {
                graph
                    .node_opt(id)
                    .is_some_and(|n| matches!(n.op, Op::Load(_)) && !n.is_volatile_access())
            })
            .filter(|id| !anchors.contains(id))
            .filter_map(|id| {
                let n = graph.node_opt(id)?;
                if n.inputs.len() < 3 || n.inputs[0] == region {
                    return None;
                }
                // JIT round 23 wave 9 (lane W9-L7; correctness, unconditional):
                // only the documented `[ctrl, mem, base, offset]` form, the one
                // that can carry the guard as its trailing token (see the copy
                // below). A shorter compact form has no token slot.
                if n.inputs.len() != 4 {
                    return None;
                }
                let base = n.inputs[2];
                let addr = n.inputs.get(3).copied().unwrap_or(NO_NODE);
                let bn = graph.node_opt(base)?;
                if bn.ty != IrType::Ref
                    || matches!(
                        bn.op,
                        Op::New { .. }
                            | Op::NewArray { .. }
                            | Op::MultiNewArray { .. }
                            | Op::NewCp { .. }
                            | Op::Dead
                    )
                    || body.contains(&base)
                    || !is_loop_invariant(graph, base, region, &body)
                    || base_non_null_on_entry(graph, base)
                {
                    return None;
                }
                if addr != NO_NODE
                    && (body.contains(&addr) || !is_loop_invariant(graph, addr, region, &body))
                {
                    return None;
                }
                if !anchored_on_every_iteration(graph, n.inputs[0], region, &body)
                    || !guards.permits_hoist_proven(graph, id, base, addr, true)
                {
                    return None;
                }
                Some((id, base))
            })
            .collect();
        if candidates.is_empty() {
            continue;
        }
        candidates.sort_unstable();
        let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
            continue;
        };
        if preheader_traps_block_entry_predicate(graph, entry_pred, header_bci) {
            if dbg {
                eprintln!("[ir-licm] null-base predicate: header {region} pre-header may trap");
            }
            continue;
        }
        // JIT r23 w2c close: a spliced loop the checkcast predicate already
        // guarded has a second chain snapshot at the header's pc now, its
        // guard's, which `loop_entry_chain_frame`'s one-chain rule refuses.
        // That snapshot IS the loop-entry chain frame: name it again.
        let reused = if spliced_header {
            chain_predicate_guard_frame(graph, entry_pred, header_bci)
        } else {
            None
        };
        let entry_frame = match reused {
            Some(si) => graph
                .safepoints
                .get(si as usize)
                .map(|f| (header_bci, f.clone())),
            None if spliced_header => loop_entry_chain_frame(graph, region, entry_slot, &body),
            None => loop_entry_frame(graph, region, entry_slot, &body),
        };
        let Some((header_bci, frame)) = entry_frame else {
            if dbg {
                eprintln!("[ir-licm] null-base predicate: header {region} has no entry frame");
            }
            continue;
        };
        let Ok(bci32) = u32::try_from(header_bci) else {
            continue;
        };
        // A spliced loop's predicate is charged at the compiling method's
        // call site, under the chain id of the callee's header (the VM's chain
        // sinks, `deopt_resume::chain_predicate_charge_id`), and asked there.
        let withdrawn = match frame.splice_scope.as_deref() {
            Some(chain) => match (chain.scopes.first(), chain.scopes.last()) {
                (Some(inner), Some(outer)) => crate::deopt::chain_predicate_withdrawn(
                    despec,
                    method_key,
                    outer.bci,
                    &inner.method_key,
                    inner.bci,
                    crate::deopt::PredicateKind::NullBase,
                ),
                _ => true,
            },
            None => crate::deopt::predicate_withdrawn(
                despec,
                method_key,
                bci32,
                crate::deopt::PredicateKind::NullBase,
            ),
        };
        if withdrawn {
            if dbg {
                eprintln!(
                    "[ir-licm] null-base predicate: header {region} bci {header_bci} de-specialised"
                );
            }
            continue;
        }
        let entry_mem = loop_entry_memory(graph, region, entry_pred);
        let si = match reused {
            Some(si) => si,
            None => {
                graph.push_safepoint(frame);
                // Cast: a snapshot index; `push_safepoint` just grew the list.
                (graph.safepoints.len() - 1) as u32
            }
        };
        let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
        let mut guarded: Vec<NodeId> = Vec::new();
        let mut guard_of: Vec<(NodeId, NodeId)> = Vec::new();
        for &(_, base) in &candidates {
            if guarded.contains(&base) {
                continue;
            }
            // No pc on the test: only the guard that names the frame anchors
            // the header's bci in the pre-header (`class_predicate`'s rule).
            // JIT r23 w2c close: when the checkcast predicate already tested
            // this base's class in the pre-header, the test is that class
            // (`ExactClassIs` answers 0 for a null, so it is the null test
            // too). The copies name this guard as their token, so they are
            // ordered after a class proof and never read a wrong-class
            // object's field ahead of the checkcast guard (wave 9b's open
            // ordering note).
            let ne = match checkcast_predicate_class(graph, entry_pred, base) {
                Some(class_id) => {
                    graph.add(Op::ExactClassIs { class_id }, IrType::Int, vec![base], None)
                }
                None => graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![base, null], None),
            };
            let guard = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, ne],
                Some(header_bci),
            );
            if !graph.set_node_frame_snapshot(guard, si) {
                graph.kill(guard);
                graph.kill(ne);
                continue;
            }
            guarded.push(base);
            guard_of.push((base, guard));
            if spliced_header {
                graph
                    .chain_predicate_guards
                    .push((guard, crate::deopt::PredicateKind::NullBase as u8));
            }
            if dbg {
                eprintln!(
                    "[ir-licm] null-base predicate: header {region} bci {header_bci}: guard n{guard} \
                     (base n{base})"
                );
            }
        }
        for (load, base) in candidates {
            if !guarded.contains(&base) {
                continue;
            }
            let Some(n) = graph.node_opt(load) else {
                continue;
            };
            let (op, ty, pc, mut inputs) = (n.op.clone(), n.ty, n.bytecode_pc, n.inputs.clone());
            let mem = inputs[1];
            let new_mem = if is_loop_invariant(graph, mem, region, &body) && !body.contains(&mem) {
                mem
            } else {
                match entry_mem {
                    Some(m) => m,
                    None => continue,
                }
            };
            inputs[0] = entry_pred;
            inputs[1] = new_mem;
            // JIT round 23 wave 9 (lane W9-L7; correctness, unconditional):
            // the copy names its guard as its trailing guard token
            // (`Op::guard_shape`), an ORDERING edge the scheduler honours.
            // Creating the copy after the guard was not enough: nothing orders
            // two impure nodes of one block by id, and on dev 619bfa8434 the
            // copies of `h.b` / `h.k` were emitted ahead of `Guard(h != null)`
            // in `viaHolder`'s pre-header, so `viaHolder(null, 0)` -- a
            // zero-trip loop -- threw NullPointerException where HotSpot
            // returns 0 (probe `C:\craton\n23p\w9L7\npe\ZT.java`).
            let Some(&(_, guard)) = guard_of.iter().find(|&&(b, _)| b == base) else {
                continue;
            };
            let mut inputs = inputs.to_vec();
            inputs.push(guard);
            // It keeps the original's pc, which the lowering keys the field's
            // row on.
            let copy = graph.add(op, ty, inputs, pc);
            if let Some(&decl) = graph.field_decl_class.get(&load) {
                graph.field_decl_class.insert(copy, decl);
            }
            if let Some(&row) = graph.field_rows.get(&load) {
                graph.field_rows.insert(copy, row);
            }
            if let Some(&row_pc) = graph.field_row_pcs.get(&load) {
                graph.field_row_pcs.insert(copy, row_pc);
            }
            // The r23 L8 heap-buffer close: and the `byte[]` mark
            // (`ByteBuffer.hb`), which the scoped-access predicate reads.
            if graph.byte_array_nodes.contains(&load) {
                graph.byte_array_nodes.insert(copy);
            }
            // JIT r23 w2c close: the read's MEMORY-token users (the builder
            // threads every read through memory) take its own incoming
            // memory -- the rule `install_array_load_hoists` and the checkcast
            // predicate already follow -- so the loop's in-body chain stays
            // whole: a read after this one was ordered after the reads before
            // it, and still is. Naming the pre-header copy instead cut the
            // chain there, and the deopt-capable main/post unroll (which
            // orders its copies' faults by that chain) refused the loop. The
            // body writes nothing (checked above), so no write is reordered.
            {
                let in_mem = mem;
                let mem_users: Vec<(NodeId, usize)> = (0..graph.nodes.len())
                    .flat_map(|u| {
                        let n = &graph.nodes[u];
                        (0..n.inputs.len())
                            .filter(move |&k| {
                                n.inputs[k] == load && crate::ir::is_memory_token_slot(n, k)
                            })
                            // Cast: arena index to node id.
                            .map(move |k| (u as NodeId, k))
                    })
                    .collect();
                for (u, k) in mem_users {
                    graph.set_input(u, k, in_mem);
                }
            }
            graph.replace_all_uses(load, copy);
            graph.kill(load);
            replaced += 1;
            if dbg {
                eprintln!(
                    "[ir-licm] null-base predicate: header {region}: n{load} -> n{copy} \
                     (base n{base})"
                );
            }
        }
        if guarded.is_empty() && reused.is_some() {
            graph.kill(null);
        } else if guarded.is_empty() {
            graph.kill(null);
            let keep: Vec<bool> = (0..graph.safepoints.len())
                .map(|i| i != si as usize)
                .collect();
            let _ = graph.retain_safepoints(&keep);
        }
    }
    replaced
}

// ── Loop-entry checkcast predicates (JIT round 23 wave 5, lane L1, TCL23-3) ──
//
// `for (i = 0; i < n; i++) s += ((Sub) o).v + ...;` with `o` loop-invariant
// casts the same object on every iteration: an `Op::CheckCast` (a helper call,
// opaque on the memory chain) per trip, and its RESULT is an in-body node, so
// the `.v` read through it can never leave the loop
// (`r23-w2c-typecheck-loops-loads-stay-in-the-loop-FIXED-20261024.md`, TYPECHECK21-1
// part C: castfixed 2.4 ns/elem against HotSpot's 0.28 on `n23-w4`).
//
// The predicate: in the pre-header, `Guard{bci: header}(o == null ||
// ExactClassIs(o, C))` for the cast's resolved target `C`, named by the
// loop-entry frame; every in-body `CheckCast{C}(o)` is then removed -- its
// value uses read `o` itself (a successful cast returns its operand), its
// memory uses read its own incoming memory. The `.v` read's base is then `o`,
// invariant and defined outside, which the null-base load predicate after it
// can hoist.
//
// Why sound: `ExactClassIs(o, C)` true means `o` is a `C`, so the cast passes;
// a null passes any cast. `o` is one SSA value defined outside the loop, so
// every in-body cast of it answers what the pre-header test answered. A
// failing guard only deopts to the loop entry with the entry values, and the
// interpreter runs the loop itself: the cast throws `ClassCastException` at
// its own bci if the body runs, nothing if it does not. An object of a
// SUBCLASS of `C` (or of `C` itself when `C` is an interface or abstract)
// would fail the exact test although the cast passes, so only a FINAL,
// non-interface target is planned (`checkcast_exact_target`): there the test is
// exact and the guard fails only where the cast would throw. A guard that keeps
// failing (an activation that throws `ClassCastException` on entry) is
// withdrawn by the de-spec registry at the header (`PredicateKind::CheckCast`).
// Array targets are not planned (`ExactClassIs` answers 0 for every array).

/// `CRATONVM_JIT_IR_CHECKCAST_PREDICATE` (JIT round 23 wave 5, lane L1,
/// TCL23-3; perf, **default ON**, `=0` kill): see the section notes. Read per
/// compile.
pub(crate) fn checkcast_predicate_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_CHECKCAST_PREDICATE_CHAIN` (JIT r23 w2c close; perf,
/// **default ON**, `=0` kill; also needs
/// [`null_base_load_predicate_chain_enabled`]): the checkcast predicate for a
/// loop SPLICED from a callee, by the null-base predicate's chain rule -- the
/// guard names the loop-entry chain ([`loop_entry_chain_frame`]), is listed in
/// `Graph::chain_predicate_guards` under `PredicateKind::CheckCast`, is asked
/// for withdrawal at the call site, and in an OSR graph is planted only where
/// no OSR entry can bypass the pre-header
/// ([`spliced_loop_entered_only_through_its_pre_header`]). `W2cTypecheckLoops`
/// castfixed: the timed loop is `time()`'s OSR body with the lambda and
/// `castFixedRow` spliced in, where the cast and the `.v` read through it
/// stayed in the loop. Read per compile.
pub(crate) fn checkcast_predicate_chain_enabled() -> bool {
    true
}

/// The final class a pre-header checkcast predicate guard of `entry_pred`
/// proves for `base` -- `Guard(base == null || ExactClassIs(base, C))`, the
/// shape [`plant_checkcast_predicates`] plants -- or `None`.
pub(in crate::ir_optimize) fn checkcast_predicate_class(
    graph: &Graph,
    entry_pred: NodeId,
    base: NodeId,
) -> Option<u32> {
    graph.nodes.iter().find_map(|g| {
        if !matches!(g.op, Op::Guard { .. }) || g.inputs.first() != Some(&entry_pred) {
            return None;
        }
        let cond = graph.node_opt(*g.inputs.get(1)?)?;
        if cond.op != Op::Or || cond.inputs.len() != 2 {
            return None;
        }
        let (a, b) = (
            graph.node_opt(cond.inputs[0])?,
            graph.node_opt(cond.inputs[1])?,
        );
        let is_null_test = a.op == Op::Cmp(CmpOp::Eq)
            && a.inputs.first() == Some(&base)
            && a.inputs
                .get(1)
                .and_then(|&c| graph.node_opt(c))
                .is_some_and(|c| c.op == Op::Const(0) && c.ty == IrType::Ref);
        match b.op {
            Op::ExactClassIs { class_id } if is_null_test && b.inputs.first() == Some(&base) => {
                Some(class_id)
            }
            _ => None,
        }
    })
}

/// `CRATONVM_JIT_IR_CAST_AFTER_INSTANCEOF` (JIT r23 w2c close; perf, **default
/// ON**, `=0` kill): [`drop_instanceof_dominated_casts`]. Read per compile.
pub(crate) fn cast_after_instanceof_enabled() -> bool {
    true
}

/// Remove every `checkcast T` whose control is the TRUE edge of an
/// `instanceof T` of the same value -- javac's `if (o instanceof T t)` and
/// `if (o instanceof T) ((T) o)...` shapes: `If(Cmp(Eq|Ne, InstanceOf{T}(o),
/// 0))`, the cast `CheckCast{T}(o)` anchored at the projection on which the
/// `instanceof` answered 1. Returns how many casts were removed.
///
/// Why it is exact: both nodes name the SAME interned target pointer (one
/// resolved class, one loader), `instanceof` answered 1 on that path, so `o`
/// is non-null and assignable to `T`, which is exactly what makes the cast
/// pass; it would neither throw nor change the value. Its value uses read
/// `o`, its memory-token uses its own incoming memory (the checkcast
/// predicate's rewiring). `W2cTypecheckLoops` shapes: each pattern match paid
/// a second full type check on its hit arm.
pub(in crate::ir_optimize) fn drop_instanceof_dominated_casts(graph: &mut Graph) -> usize {
    let mut removed = 0usize;
    for cast in 0..graph.nodes.len() {
        // Cast: arena index to node id.
        let cast = cast as NodeId;
        let Some(n) = graph.node_opt(cast) else {
            continue;
        };
        let Op::CheckCast {
            name_ptr, name_len, ..
        } = n.op
        else {
            continue;
        };
        let (Some(&ctrl), Some(&mem_in), Some(&obj)) =
            (n.inputs.first(), n.inputs.get(1), n.inputs.get(2))
        else {
            continue;
        };
        let Some((iff, which)) = super::final_typecheck_fold::dominated_branch_proj(graph, ctrl)
        else {
            continue;
        };
        let Some(cond) = graph
            .node_opt(iff)
            .filter(|i| i.op == Op::If)
            .and_then(|i| i.inputs.get(1).copied())
        else {
            continue;
        };
        let Some(c) = graph.node_opt(cond) else {
            continue;
        };
        let is_zero = |id: NodeId| {
            graph
                .node_opt(id)
                .is_some_and(|z| z.op == Op::Const(0) && z.ty == IrType::Int)
        };
        let (eq, io) = match (&c.op, c.inputs.as_slice()) {
            (Op::Cmp(CmpOp::Eq), [a, b]) if is_zero(*b) => (true, *a),
            (Op::Cmp(CmpOp::Eq), [a, b]) if is_zero(*a) => (true, *b),
            (Op::Cmp(CmpOp::Ne), [a, b]) if is_zero(*b) => (false, *a),
            (Op::Cmp(CmpOp::Ne), [a, b]) if is_zero(*a) => (false, *b),
            (Op::InstanceOf { .. } | Op::ExactClassIs { .. }, _) => (false, cond),
            _ => continue,
        };
        let same_test = graph.node_opt(io).is_some_and(|t| match t.op {
            Op::InstanceOf { name_ptr: p, .. } => p == name_ptr && t.inputs.get(2) == Some(&obj),
            Op::ExactClassIs { class_id } => {
                t.inputs.first() == Some(&obj)
                    && checkcast_exact_target(name_ptr, name_len) == Some(class_id)
            }
            _ => false,
        });
        // `Proj(0)` is the edge taken when the condition holds: for `Eq` that
        // is `instanceof == 0` (a miss), so the hit is `Proj(1)`; for `Ne` the
        // hit is `Proj(0)`.
        let on_hit_edge = if eq { which == 1 } else { which == 0 };
        if !same_test || !on_hit_edge {
            continue;
        }
        let users: Vec<NodeId> = (0..graph.nodes.len())
            .filter(|&u| graph.nodes[u].inputs.contains(&cast))
            // Cast: arena index to node id.
            .map(|u| u as NodeId)
            .collect();
        for u in users {
            let un = &graph.nodes[u as usize];
            let slots: Vec<usize> = (0..un.inputs.len())
                .filter(|&k| un.inputs[k] == cast && crate::ir::is_memory_token_slot(un, k))
                .collect();
            for k in slots {
                graph.set_input(u, k, mem_in);
            }
        }
        graph.replace_all_uses(cast, obj);
        graph.kill(cast);
        removed += 1;
    }
    removed
}

/// The resolved target class id of `CheckCast { name_ptr, name_len }` when
/// it can stand in an `ExactClassIs` test: resolved at compile time, non-zero,
/// not an array type (whose header class id is its component's), and a
/// `final`, non-interface class (`crate::typecheck_target_is_final_class`, the
/// verdict the door interned the site with) -- for which `o instanceof C` IS
/// `ExactClassIs(o, C)` on a non-null `o`, so the guard fails exactly when the
/// cast would throw and the predicate speculates nothing.
pub(in crate::ir_optimize) fn checkcast_exact_target(
    name_ptr: usize,
    name_len: usize,
) -> Option<u32> {
    if name_ptr == 0 || name_len == 0 {
        return None;
    }
    // SAFETY: a `CheckCast` row's name is an interned, leaked `&'static str`
    // (`intern_typecheck_*`), `name_len` bytes long, never freed.
    let first = unsafe { *(name_ptr as *const u8) };
    if first == b'[' {
        return None;
    }
    if !crate::typecheck_target_is_final_class(name_ptr as *const u8) {
        return None;
    }
    crate::typecheck_target_for_site(name_ptr as *const u8).filter(|&id| id != 0)
}

/// Plant the loop-entry checkcast predicates of every loop (see the section
/// notes), unless `despec` withdrew this planter's speculation at the loop's
/// header. Returns how many in-body casts were removed.
pub(in crate::ir_optimize) fn plant_checkcast_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> usize {
    if graph.safepoints.is_empty() {
        return 0;
    }
    let dbg = false;
    let osr_graph = graph.optimize_osr_route || graph.osr_compile;
    let anchors = crate::ir::exception_edge_anchors(graph);
    // JIT r23 w2c close: a loop spliced from a callee, under its switch -- the
    // null-base predicate's chain rule (`plant_null_base_load_predicates`).
    let chain_on = checkcast_predicate_chain_enabled() && null_base_load_predicate_chain_enabled();
    let mut removed = 0usize;
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if !matches!(
            graph.node_opt(region).map(|n| &n.op),
            Some(Op::Region | Op::Merge)
        ) || entry_pred == NO_NODE
        {
            continue;
        }
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        if body.contains(&entry_pred) {
            continue;
        }
        let spliced_header = chain_on && header_is_spliced(graph, region);
        let entered_through_pre_header = if spliced_header {
            spliced_loop_entered_only_through_its_pre_header(graph, region, &body)
        } else {
            loop_entered_only_through_its_pre_header(graph, region, entry_pred, &body)
        };
        if osr_graph && !entered_through_pre_header {
            continue;
        }
        let Some(entry_slot) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
            .map(|p| p + 1)
        else {
            continue;
        };
        // `(cast, operand, target class)`, in node order.
        let mut casts: Vec<(NodeId, NodeId, u32)> = body
            .iter()
            .copied()
            .filter(|id| !anchors.contains(id))
            .filter_map(|id| {
                let n = graph.node_opt(id)?;
                let Op::CheckCast { name_ptr, name_len } = n.op else {
                    return None;
                };
                if n.inputs.len() < 3 {
                    return None;
                }
                let obj = n.inputs[2];
                let on = graph.node_opt(obj)?;
                if on.ty != IrType::Ref
                    || body.contains(&obj)
                    || !is_loop_invariant(graph, obj, region, &body)
                    || !anchored_on_every_iteration(graph, n.inputs[0], region, &body)
                {
                    return None;
                }
                Some((id, obj, checkcast_exact_target(name_ptr, name_len)?))
            })
            .collect();
        if casts.is_empty() {
            continue;
        }
        casts.sort_unstable();
        // One target per operand: two different targets for one object would
        // fail the entry test of one of them on every entry.
        let mut targets: Vec<(NodeId, u32)> = Vec::new();
        let mut conflicted: Vec<NodeId> = Vec::new();
        for &(_, obj, class_id) in &casts {
            match targets.iter().find(|&&(o, _)| o == obj) {
                Some(&(_, c)) if c != class_id => conflicted.push(obj),
                Some(_) => {}
                None => targets.push((obj, class_id)),
            }
        }
        casts.retain(|&(_, obj, _)| !conflicted.contains(&obj));
        targets.retain(|&(obj, _)| !conflicted.contains(&obj));
        if casts.is_empty() {
            continue;
        }
        let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
            continue;
        };
        if preheader_traps_block_entry_predicate(graph, entry_pred, header_bci) {
            if dbg {
                eprintln!("[ir-licm] checkcast predicate: header {region} pre-header may trap");
            }
            continue;
        }
        let entry_frame = if spliced_header {
            loop_entry_chain_frame(graph, region, entry_slot, &body)
        } else {
            loop_entry_frame(graph, region, entry_slot, &body)
        };
        let Some((header_bci, frame)) = entry_frame else {
            if dbg {
                eprintln!("[ir-licm] checkcast predicate: header {region} has no entry frame");
            }
            continue;
        };
        let Ok(bci32) = u32::try_from(header_bci) else {
            continue;
        };
        // A spliced loop's predicate is charged and asked at the compiling
        // method's call site, under the chain id of the callee's header, as
        // the null-base predicate's is.
        let withdrawn = match frame.splice_scope.as_deref() {
            Some(chain) => match (chain.scopes.first(), chain.scopes.last()) {
                (Some(inner), Some(outer)) => crate::deopt::chain_predicate_withdrawn(
                    despec,
                    method_key,
                    outer.bci,
                    &inner.method_key,
                    inner.bci,
                    crate::deopt::PredicateKind::CheckCast,
                ),
                _ => true,
            },
            None => crate::deopt::predicate_withdrawn(
                despec,
                method_key,
                bci32,
                crate::deopt::PredicateKind::CheckCast,
            ),
        };
        if withdrawn {
            if dbg {
                eprintln!(
                    "[ir-licm] checkcast predicate: header {region} bci {header_bci} de-specialised"
                );
            }
            continue;
        }
        graph.push_safepoint(frame);
        // Cast: a snapshot index; `push_safepoint` just grew the list.
        let si = (graph.safepoints.len() - 1) as u32;
        let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
        let mut guarded: Vec<NodeId> = Vec::new();
        for &(obj, class_id) in &targets {
            let is_null = graph.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![obj, null], None);
            let exact = graph.add(Op::ExactClassIs { class_id }, IrType::Int, vec![obj], None);
            let cond = graph.add(Op::Or, IrType::Int, vec![is_null, exact], None);
            let guard = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, cond],
                Some(header_bci),
            );
            if !graph.set_node_frame_snapshot(guard, si) {
                graph.kill(guard);
                graph.kill(cond);
                graph.kill(exact);
                graph.kill(is_null);
                continue;
            }
            guarded.push(obj);
            if spliced_header {
                graph
                    .chain_predicate_guards
                    .push((guard, crate::deopt::PredicateKind::CheckCast as u8));
            }
            if dbg {
                eprintln!(
                    "[ir-licm] checkcast predicate: header {region} bci {header_bci}: guard n{guard} \
                     (n{obj} exact class {class_id})"
                );
            }
        }
        for (cast, obj, _) in casts {
            if !guarded.contains(&obj) {
                continue;
            }
            let Some(mem_in) = graph.node_opt(cast).map(|n| n.inputs[1]) else {
                continue;
            };
            // The cast's MEMORY uses read its incoming memory; everything else
            // (its value uses and every frame slot naming it) reads `obj`.
            let users: Vec<NodeId> = (0..graph.nodes.len())
                .filter(|&u| graph.nodes[u].inputs.contains(&cast))
                // Cast: arena index to node id.
                .map(|u| u as NodeId)
                .collect();
            for u in users {
                let n = &graph.nodes[u as usize];
                let slots: Vec<usize> = (0..n.inputs.len())
                    .filter(|&k| n.inputs[k] == cast && crate::ir::is_memory_token_slot(n, k))
                    .collect();
                for k in slots {
                    graph.set_input(u, k, mem_in);
                }
            }
            graph.replace_all_uses(cast, obj);
            graph.kill(cast);
            removed += 1;
            if dbg {
                eprintln!("[ir-licm] checkcast predicate: header {region}: n{cast} -> n{obj}");
            }
        }
        if guarded.is_empty() {
            graph.kill(null);
            let keep: Vec<bool> = (0..graph.safepoints.len())
                .map(|i| i != si as usize)
                .collect();
            let _ = graph.retain_safepoints(&keep);
        }
    }
    removed
}

// ── Invariant array-element loads (round 16, lane matrix) ────────────
//
// `for (j = 0; j < n; j++) c[i][j] += x * b[k][j];` reads the rows `c[i]` and
// `b[k]` -- an `aaload` each -- on every iteration, with a null check and a
// bounds check apiece, although neither `c`, `b`, `i` nor `k` changes in the
// loop and the loop writes only `double` elements. HotSpot reads each row once.
// None of the LICM arms above takes an `Op::ArrayLoad`: the general arm bails
// on a body holding one (an array read is impure because it can trap), and the
// restricted read arm hoists only `Op::Load` and only out of a body that writes
// nothing. Worse, the range predicates below plant `n <= a.length` only for an
// INVARIANT array, and a row read inside the loop is not one, so every 2-D
// inner loop also kept its per-element bounds checks
// (`docs/feature-designs/jit-ir-optimizations.md`).
//
// This pass hoists such a read the way the range predicate hoists its
// `ArrayLength`: a COPY of the load is planted in the pre-header, at the
// header's bci, naming a copy of the header snapshot with every header φ at its
// entry value, and the in-loop load is replaced by it.
//
// # Why it is sound
//
// * **The value cannot change.** The base and the index are loop-invariant,
//   and no node of the body may write the element: every write's alias class
//   is checked against the load's with `Graph::may_alias` (an element of
//   another kind, a field, a static are disjoint; a call, an allocation, an
//   acquire or a volatile anywhere in the body refuse the loop). The copy
//   reads the loop's ENTRY memory, so it is ordered after every pre-header
//   write.
// * **A fault is a deopt to the loop header, never a throw.** The copy's null
//   and bounds checks deopt (`emit_array_null_bounds_guards_for`), and its frame
//   resumes at the HEADER with the state the compiled code had on entry. The
//   interpreter then runs the loop from its first test and performs (or skips)
//   the access itself, so a loop that would not have run, or a guard in the
//   body that would have left first, still gets exactly the interpreter's
//   answer. This is the range predicate's argument, and the frame is built the
//   same way; it is why the load needs no `loop_body_definitely_runs` proof.
// * **It runs after everything the pre-header does** -- the copy is a new,
//   highest-id node anchored at the entry control and reading the entry
//   memory, as the predicate's nodes are.
// * **No guard licensed it.** A load whose base or index is computed from a
//   value a body guard constrains (`LoopGuards`), or that names a guard token,
//   stays.
// * **Only an access every iteration makes**: anchored at the header or on the
//   stay edge of its exit test, so a failing copy almost always stands for an
//   access that faults on the first iteration. A loop whose copy keeps
//   deopting is withdrawn through the de-spec registry at the header bci, like
//   the predicate.
//
// # What it does not do
//
// It never runs in the optimizing OSR route's compile (`OptimizeContext::
// osr_route`): an OSR entry stub jumps to the header, past the copy, and
// `emit_osr_entry_stubs` refuses an entry with an unnamed value live across it.
// Rounds re-run the pass so a row hoisted out of `j` can leave `k` too
// (`c[i]` is invariant in both).

/// `CRATONVM_JIT_IR_LICM_ARRAY_LOAD` -- [`hoist_invariant_array_loads`].
/// Default **ON** since round 16 (`R16MatrixKernel` mm 50 -> 32 ms); `=0`
/// turns it off.
pub(in crate::ir_optimize) fn licm_array_load_enabled() -> bool {
    true
}

/// The switch and the OSR-route exclusion together, as [`optimize_with`]
/// asks them. The OSR route hoists only with
/// [`osr_remat_array_load_enabled`] on as well: its entry stubs then recompute
/// (and re-check) every hoisted read an entry jumps past.
/// `CRATONVM_JIT_IR_LICM_ARRAY_LOAD_AFTER_PREDICATES` (JIT round 23 wave 6,
/// lane L1; perf, **default ON**, `=0` kill): one more
/// [`hoist_invariant_array_loads`] round after the null-base load predicate
/// moved a field read to the pre-header, so an element read through that
/// field (`h.tab[5]` in `W2cTypecheckLoops`' cast / nocast rows) leaves the
/// loop too. The same planner, the same frame and de-spec rules as the first
/// round. Read per compile.
pub(crate) fn licm_array_load_after_predicates_enabled() -> bool {
    true
}

pub(in crate::ir_optimize) fn array_load_hoist_admitted(osr_route: bool) -> bool {
    licm_array_load_enabled() && (!osr_route || osr_remat_array_load_enabled())
}

/// `CRATONVM_JIT_IR_OSR_REMAT_ARRAY_LOAD` (round 16, lane matrix, MX16-1).
/// **Default ON** since 2026-10-06 (with `CRATONVM_JIT_IR_MULTIANEWARRAY` it
/// is what puts `R16Bench.matrix`'s OSR body on the optimizing tier: 54-72 ->
/// 4-5 ms); `=0` turns it off.
///
/// The optimizing OSR route's body is entered at a loop header, past every
/// pre-header, so neither a hoisted array read
/// ([`hoist_invariant_array_loads`]) nor a range predicate
/// ([`plant_range_check_predicates`]) runs on that path, and the facts both
/// give `ir_check_elim` (a dereferenced base, `n <= a.length`) were never
/// established for the state the interpreter hands over. With this switch on,
/// `ir_lower::Lowerer::emit_osr_entry_stubs` re-executes every such node in a
/// block dominating the entry -- the read with its null and bounds checks, the
/// predicate's length read and compare -- after seeding the frame, publishes
/// a hoisted read the body still uses, and on any failure leaves for the
/// interpreter at the entry's own header with exactly the state it was handed
/// (an `OsrExit` point), instead of entering. The array-load hoist is then
/// admitted on the OSR route, and the predicate's `UncommonTrap` counts as a
/// resumable exit (it resumes at its header with the loop-entry state), so the
/// door no longer refuses the whole body for it. No `static`.
pub(crate) fn osr_remat_array_load_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_OSR_ROUTE_ARM_PREDICATES` (JIT round 20 wave 19, lane
/// yLoopPred, LOOPOPT20-3 of `docs/feature-designs/jit-ir-optimizations.md`)
/// -- **default ON**, `=0` restores the old OSR route. On the optimizing OSR
/// route's compile, a loop whose single entry control is the opaque OSR merge
/// in front of its header ([`entry_is_opaque_osr_arm`]) gets what a
/// method-entry compile gives it: its direct `getstatic`s move to that merge
/// (`loop_static_hoist`) and its constant-masked accesses get their loop-entry
/// guard there (`mask_predicate`).
///
/// # Why it is sound
///
/// The OSR door's compile builds every covered header with an opaque arm
/// (`IrBuilder::build_opaque_osr_merge`): the stub jumps to the arm, which
/// FALLS INTO the merge, and the merge is the header's one forward
/// predecessor. A node anchored on that merge therefore runs on both ways into
/// the loop -- the normal pre-header path and the OSR entry -- before the
/// header; so a pre-header guard is not skipped by the entry, and a pre-header
/// read is computed by it. The arm brings every local and memory in through
/// merge phis (`Op::OsrLocal`, `Op::OsrMemory`), and its admission is the
/// arm's data-flow live-in set, which may hold constants only: a value the
/// guard or the read needs from before the merge refuses the entry, as it
/// already did for the loop body that reads the same value. A loop with no arm
/// (an uncovered header) keeps the old route: no move, no guard. An ORDINARY
/// entry stub anywhere else in the body re-executes a range-shaped guard a
/// block dominating it planted, or is refused
/// (`ir_lower::Lowerer::emit_osr_entry_stubs`, `CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL`),
/// which is why this also requires [`osr_remat_array_load_enabled`].
/// A failing guard resumes the interpreter at the header with the loop-entry
/// frame (the merge phis), as in a method-entry compile, which the opaque
/// guard-exit fact admits (`osr_exit::guard_exit_fact_opaque`: every frame
/// resumable in place). No `static`; read per compile.
pub(crate) fn osr_route_arm_predicates_enabled() -> bool {
    true && osr_remat_array_load_enabled()
}

/// LOOPOPT20-3: is `entry_pred` -- a loop's single entry control -- the opaque
/// OSR merge `IrBuilder::build_opaque_osr_merge` put in front of the header
/// (a `Merge` one of whose inputs is the ARM: `Proj(0)` of an `If` on
/// `Op::OsrEntryFlag`)? Then an OSR entry at that header runs everything
/// anchored on `entry_pred`. Any other shape answers `false`.
pub(crate) fn entry_is_opaque_osr_arm(graph: &Graph, entry_pred: NodeId) -> bool {
    let Some(pre) = graph.node_opt(entry_pred) else {
        return false;
    };
    if pre.op != Op::Merge {
        return false;
    }
    pre.inputs.iter().any(|&p| {
        graph.node_opt(p).is_some_and(|proj| {
            proj.op == Op::Proj(0)
                && proj
                    .input_opt(0)
                    .and_then(|iff| graph.node_opt(iff))
                    .is_some_and(|iff| {
                        iff.op == Op::If
                            && iff
                                .input_opt(1)
                                .and_then(|c| graph.node_opt(c))
                                .is_some_and(|c| matches!(c.op, Op::OsrEntryFlag { .. }))
                    })
        })
    })
}

/// At most this many loads hoisted per compile.
pub(in crate::ir_optimize) const MAX_ARRAY_LOAD_HOISTS: usize = 32;

/// Passes over every loop header: one moves a load out of one loop level.
pub(in crate::ir_optimize) const ARRAY_LOAD_HOIST_ROUNDS: usize = 3;

/// One loop's hoists, decided without touching the graph.
pub(in crate::ir_optimize) struct ArrayLoadHoistPlan {
    /// The header merge's bci: the copies' pc and resume point.
    pub(in crate::ir_optimize) header_bci: usize,
    /// The loop-entry control the copies are anchored at.
    pub(in crate::ir_optimize) entry_pred: NodeId,
    /// `(load, memory state its copy reads)`, ascending load id.
    pub(in crate::ir_optimize) loads: Vec<(NodeId, NodeId)>,
    /// The header snapshot with its φs at their entry values.
    pub(in crate::ir_optimize) frame: SafepointSnapshot,
    /// JIT r23 wave 8: a spliced header's existing loop-entry chain snapshot
    /// (a null-base predicate's), named instead of pushing `frame`.
    pub(in crate::ir_optimize) reuse_si: Option<u32>,
}

/// The memory state the first read of an in-body READ-ONLY token chain
/// starts from: walk `mem` back through in-body loads until it leaves the body.
/// `None` when the chain meets anything but a read inside the body.
pub(in crate::ir_optimize) fn read_chain_entry_token(
    graph: &Graph,
    mem: NodeId,
    body: &FxHashSet<NodeId>,
) -> Option<NodeId> {
    let mut cur = mem;
    for _ in 0..64 {
        if !body.contains(&cur) {
            return graph.node_opt(cur).map(|_| cur);
        }
        let n = graph.node_opt(cur)?;
        if !matches!(n.op, Op::Load(_) | Op::ArrayLoad(_) | Op::ArrayLength)
            || n.is_volatile_access()
        {
            return None;
        }
        cur = n.input_opt(1)?;
    }
    None
}

/// Decide the hoists for the loop at `region`, or `None` when it has none.
pub(in crate::ir_optimize) fn array_load_hoist_plan(
    graph: &Graph,
    users: &[Vec<NodeId>],
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> Option<ArrayLoadHoistPlan> {
    let [_back] = back_ctrls else {
        return None;
    };
    if entry_pred == NO_NODE {
        return None;
    }
    let region_node = graph.node_opt(region)?;
    let header_bci = region_node.bytecode_pc?;
    // The header's own snapshot (see `range_predicate_plan`): none at a
    // spliced callee's header, and a chain snapshot is not a frame to copy.
    // JIT round 23 wave 8 (lane W8-L7,
    // `CRATONVM_JIT_IR_LICM_ARRAY_LOAD_SPLICED_CHAIN`): there, the loop-entry
    // CHAIN frame a null-base predicate of this loop already names is reused
    // as is (`spliced_hoist_trap::chain_predicate_entry_frame`).
    let (source, reuse_si) = match graph
        .safepoints
        .iter()
        .position(|s| s.bci == header_bci && !s.is_splice_state())
    {
        Some(s) => (s, None),
        None => {
            let si =
                spliced_hoist_trap::chain_predicate_entry_frame(graph, entry_pred, header_bci)?;
            (si as usize, Some(si))
        }
    };
    let entry_slot = region_node.inputs.iter().position(|&p| p == entry_pred)? + 1;

    // The header's one control successor is the exit test.
    let mut test: Option<NodeId> = None;
    for id in anchored_at(graph, region) {
        let n = graph.node_opt(id)?;
        if n.op == Op::Dead || !n.op.is_control() || n.inputs.first() != Some(&region) {
            continue;
        }
        if n.op != Op::If || test.is_some() {
            return None;
        }
        test = Some(id);
    }
    let test = test?;
    let body = loop_body_with(graph, users, region, entry_pred, back_ctrls);
    let p0 = proj_user(graph, test, 0);
    let p1 = proj_user(graph, test, 1);
    let stays = |p: NodeId| p != NO_NODE && body.contains(&p);
    let stay = match (stays(p0), stays(p1)) {
        (true, false) => p0,
        (false, true) => p1,
        _ => return None,
    };

    // Everything the body may write. A call (`MemEffect::OPAQUE`), an
    // allocation, an acquire / release / fence or a volatile access refuses
    // the whole loop.
    let mut ids: Vec<NodeId> = body.iter().copied().collect();
    ids.sort_unstable();
    let mut writes: Vec<crate::ir::AliasClass> = Vec::new();
    for &id in &ids {
        let n = graph.node_opt(id)?;
        if matches!(n.op, Op::Phi | Op::Dead) || n.op.is_control() {
            continue;
        }
        if n.is_volatile_access() {
            return None;
        }
        // JIT r23 wave 12 (lane L7, `CRATONVM_JIT_IR_LICM_ARRAY_LOAD_PAST_TYPECHECK`):
        // a type check writes no Java-visible memory and allocates nothing
        // Java-visible -- the argument `licm_typecheck_not_write_enabled`
        // already applies to the field-read hoist -- so it neither refuses
        // the loop nor joins `writes`.
        if licm_typecheck_not_write_enabled()
            && matches!(n.op, Op::CheckCast { .. } | Op::InstanceOf { .. })
        {
            continue;
        }
        let eff = graph.memory_effect(id);
        if eff.order != crate::ir::MemOrder::Plain || eff.allocates {
            return None;
        }
        if eff.writes != crate::ir::AliasClass::None {
            writes.push(eff.writes);
        }
    }
    let guards = LoopGuards::in_body(graph, &body);
    let entry_mem = loop_entry_memory(graph, region, entry_pred);
    // nio21 wave 6 L2: an exception-edge anchor stays where its flags are
    // (hoisted, its null and bounds checks would fail in the pre-header and
    // deopt there, and the lowerer would leave its site unarmed).
    let anchors = crate::ir::exception_edge_anchors(graph);

    let mut loads: Vec<(NodeId, NodeId)> = Vec::new();
    for &id in &ids {
        if loads.len() >= MAX_ARRAY_LOAD_HOISTS {
            break;
        }
        let n = graph.node_opt(id)?;
        if anchors.contains(&id) {
            continue;
        }
        if !matches!(n.op, Op::ArrayLoad(_)) || n.inputs.len() != 4 {
            // A fifth input is a guard token (`ir::with_guard_token`): a guard
            // licensed this read, and it stays behind that guard.
            continue;
        }
        if crate::ir::guard_token_of(n) != NO_NODE {
            continue;
        }
        let (Some(ctrl), Some(mem), Some(base), Some(idx)) = (
            n.input_opt(0),
            n.input_opt(1),
            n.input_opt(2),
            n.input_opt(3),
        ) else {
            continue;
        };
        let every_trip = ctrl == region || reaches_through_pass_throughs(graph, ctrl, stay);
        if ctrl == entry_pred || !every_trip {
            continue;
        }
        if body.contains(&base)
            || body.contains(&idx)
            || graph.node_opt(base).map(|b| b.ty) != Some(IrType::Ref)
            || !is_loop_invariant(graph, base, region, &body)
            || !is_loop_invariant(graph, idx, region, &body)
        {
            continue;
        }
        let reads = graph.memory_effect(id).reads;
        if matches!(
            reads,
            crate::ir::AliasClass::Any | crate::ir::AliasClass::None
        ) || writes.iter().any(|&w| graph.may_alias(reads, w))
        {
            continue;
        }
        // Obligation (1) of `LoopGuards`: the address must not be computed
        // from a value a body guard constrains. Obligation (2) does not
        // arise: the copy deopts to the header, not to a bci in the body.
        match &guards {
            LoopGuards::None => {}
            LoopGuards::Opaque => continue,
            LoopGuards::Attributed(cone) => {
                if !licm_guard_attribution_enabled() {
                    continue;
                }
                let mut own: FxHashSet<NodeId> = FxHashSet::default();
                let mut budget = GUARD_CONE_BUDGET;
                if !value_cone(graph, base, &mut own, &mut budget)
                    || !value_cone(graph, idx, &mut own, &mut budget)
                    || !own.is_disjoint(cone)
                {
                    continue;
                }
            }
        }
        let new_mem = if !body.contains(&mem) {
            mem
        } else {
            match entry_mem.or_else(|| read_chain_entry_token(graph, mem, &body)) {
                Some(m) if !body.contains(&m) => m,
                _ => continue,
            }
        };
        loads.push((id, new_mem));
    }
    if loads.is_empty() {
        return None;
    }

    // The loop-entry frame, exactly as `range_predicate_plan` builds it.
    let mut frame = graph.safepoints.get(source)?.clone();
    for slot in frame
        .locals
        .iter_mut()
        .chain(frame.stack.iter_mut())
        .chain(frame.monitors.iter_mut())
    {
        let v = *slot;
        if v == NO_NODE {
            continue;
        }
        let vn = graph.node_opt(v)?;
        // A frame naming an allocation could be described against the
        // pre-header block after scalar replacement; see the predicate.
        if matches!(vn.op, Op::New { .. } | Op::NewArray { .. }) {
            return None;
        }
        // JIT r27 LOOP27-2: `i2l` of a narrowed `long` induction.
        if let Some(wide) = wide_entry_value(graph, v, region) {
            *slot = wide;
            continue;
        }
        if vn.op == Op::Phi && vn.inputs.first() == Some(&region) {
            let entry = vn.inputs.get(entry_slot).copied().unwrap_or(NO_NODE);
            if graph
                .node_opt(entry)
                .is_some_and(|e| matches!(e.op, Op::New { .. } | Op::NewArray { .. }))
            {
                return None;
            }
            *slot = entry;
            continue;
        }
        if body.contains(&v) || !is_loop_invariant(graph, v, region, &body) {
            return None;
        }
    }

    Some(ArrayLoadHoistPlan {
        header_bci,
        entry_pred,
        loads,
        frame,
        reuse_si,
    })
}

/// Plant one plan's copies and retire the loads they replace. Returns how many
/// moved (at most `limit`).
pub(in crate::ir_optimize) fn install_array_load_hoists(
    graph: &mut Graph,
    plan: ArrayLoadHoistPlan,
    limit: usize,
) -> usize {
    let ArrayLoadHoistPlan {
        header_bci,
        entry_pred,
        loads,
        frame,
        reuse_si,
    } = plan;
    let si = match reuse_si {
        Some(si) => si,
        None => {
            graph.push_safepoint(frame);
            (graph.safepoints.len() - 1) as u32
        }
    };
    let mut moved = 0usize;
    for (load, new_mem) in loads {
        if moved >= limit {
            break;
        }
        let Some(n) = graph.node_opt(load) else {
            continue;
        };
        let Op::ArrayLoad(kind) = n.op else {
            continue;
        };
        let ty = n.ty;
        let (Some(old_mem), Some(base), Some(idx)) =
            (n.input_opt(1), n.input_opt(2), n.input_opt(3))
        else {
            continue;
        };
        let copy = graph.add(
            Op::ArrayLoad(kind),
            ty,
            vec![entry_pred, new_mem, base, idx],
            Some(header_bci),
        );
        // It deopts at the header and MUST resume with the corrected frame;
        // unnamed, it would resolve the header's own snapshot, whose φ homes
        // the pre-header has not written yet.
        if !graph.set_node_frame_snapshot(copy, si) {
            graph.kill(copy);
            continue;
        }
        // Close the memory chain over the load first: a consumer that named
        // it as its TOKEN takes the load's own incoming token (the copy reads
        // the entry memory and is no part of the loop's chain); every VALUE
        // use, snapshot slots included, then follows the value to the copy.
        let mut token_uses: Vec<(NodeId, usize)> = Vec::new();
        let mut users = graph.users_of(load);
        users.sort_unstable();
        users.dedup();
        for u in users {
            let Some(un) = graph.node_opt(u) else {
                continue;
            };
            if un.op == Op::Dead {
                continue;
            }
            for (i, &inp) in un.inputs.iter().enumerate() {
                if inp == load && is_memory_token_slot(un, i) {
                    token_uses.push((u, i));
                }
            }
        }
        for (u, i) in token_uses {
            graph.set_input(u, i, old_mem);
        }
        graph.replace_all_uses(load, copy);
        graph.kill(load);
        moved += 1;
    }
    moved
}

/// Drop every per-copy frame of an unrolled loop ([`SafepointSnapshot::copy_frame`])
/// that no live node names any more. Such a frame was appended for exactly
/// one copy's nodes ([`install_copy_frames`]); once those are gone, no deopt
/// can resume through it. The original frame at the same bci (the one the
/// original body or copy 0 claims) is never marked, so it stays.
/// Fold one of two `Guard`s with the same bci, the same inputs (control and
/// condition), the same frame contents and the same predicate speculation
/// kinds: they test one condition at one program point, and each is a sound
/// deopt point on its own, so either may go. The one nothing reads goes (the
/// survivor keeps its users and its place in the schedule); with users on
/// both, the later one's users read the earlier. The folded guard's snapshot,
/// left unclaimed, is dropped (an unclaimed duplicate at a bci is what
/// `ir_verify::check_frame_states` rejects). Returns how many guards it folded.
pub(in crate::ir_optimize) fn merge_identical_guards(graph: &mut Graph) -> usize {
    let mut used = vec![false; graph.nodes.len()];
    for n in graph.nodes.iter().filter(|n| n.op != Op::Dead) {
        for &i in n.inputs.iter() {
            if let Some(u) = used.get_mut(i as usize) {
                *u = true;
            }
        }
    }
    let kinds = |graph: &Graph, g: NodeId| {
        let of = |list: &[(NodeId, u8)]| list.iter().find(|&&(x, _)| x == g).map(|&(_, k)| k);
        (
            of(&graph.predicate_guard_kinds),
            of(&graph.chain_predicate_guards),
        )
    };
    let frame = |graph: &Graph, g: NodeId| {
        graph.nodes[g as usize]
            .frame_snapshot
            .and_then(|si| graph.safepoints.get(si as usize))
            .map(|sp| format!("{sp:?}"))
    };
    let mut seen: FxHashMap<(usize, Vec<NodeId>), Vec<NodeId>> = FxHashMap::default();
    let mut freed: Vec<u32> = Vec::new();
    let mut merged = 0usize;
    for id in 0..graph.nodes.len() {
        // Cast: an arena index is a node id.
        let id = id as NodeId;
        let Op::Guard { bci } = graph.nodes[id as usize].op else {
            continue;
        };
        let key = (bci, graph.nodes[id as usize].inputs.as_slice().to_vec());
        let earlier = seen.get(&key).and_then(|cands| {
            cands.iter().copied().find(|&e| {
                frame(graph, e) == frame(graph, id) && kinds(graph, e) == kinds(graph, id)
            })
        });
        match earlier {
            Some(e) => {
                let (keep, fold) = if !used[e as usize] && used[id as usize] {
                    (id, e)
                } else {
                    (e, id)
                };
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir-guard-fold {}: n{fold} into n{keep} (bci {bci})",
                        graph.scope_method_key
                    );
                }
                freed.extend(graph.nodes[fold as usize].frame_snapshot);
                graph.replace_all_uses(fold, keep);
                graph.kill(fold);
                if keep == id {
                    if let Some(c) = seen
                        .get_mut(&key)
                        .and_then(|c| c.iter_mut().find(|c| **c == e))
                    {
                        *c = id;
                    }
                }
                merged += 1;
            }
            None => seen.entry(key).or_default().push(id),
        }
    }
    if !freed.is_empty() {
        let named = orphan_frames::snapshot_named_mask(graph);
        let keep: Vec<bool> = (0..graph.safepoints.len())
            .map(|i| named[i] || !freed.contains(&(i as u32)))
            .collect();
        let _ = graph.retain_safepoints(&keep);
    }
    merged
}

pub(in crate::ir_optimize) fn drop_unnamed_copy_frames(graph: &mut Graph) {
    if !graph.safepoints.iter().any(|sp| sp.copy_frame) {
        return;
    }
    // j27 (lane FRAMES): a per-copy post-call snapshot
    // (`CRATONVM_JIT_IR_UNROLL_POST_CALL_PER_COPY`) is named by its cloned
    // call's `post_call_of`, not by a `frame_snapshot`; it stays while that
    // call lives (it used to be dropped here, costing the clone its chain
    // post-call exit).
    let named = orphan_frames::snapshot_named_mask(graph);
    let keep: Vec<bool> = graph
        .safepoints
        .iter()
        .zip(named.iter())
        .map(|(sp, &named)| !sp.copy_frame || named)
        .collect();
    let _ = graph.retain_safepoints(&keep);
}

/// Drop every snapshot at index `first` or above that no live node names: a
/// frame copy whose only load was hoisted again, one level further out, by a
/// later round. An unnamed second snapshot at a header bci is what
/// `ir_verify::check_frame_states` rejects.
pub(in crate::ir_optimize) fn drop_unnamed_hoist_frames(graph: &mut Graph, first: usize) {
    let len = graph.safepoints.len();
    if len <= first {
        return;
    }
    let mut named = vec![false; len];
    for n in graph.nodes.iter() {
        if n.op == Op::Dead {
            continue;
        }
        let Some(si) = n.frame_snapshot else {
            continue;
        };
        if let Some(slot) = named.get_mut(si as usize) {
            *slot = true;
        }
    }
    let keep: Vec<bool> = (0..len).map(|i| i < first || named[i]).collect();
    let _ = graph.retain_safepoints(&keep);
}

/// Hoist every loop-invariant, unclobbered array element read out of its loop
/// (see the section notes above). Returns how many loads moved.
///
/// `despec`: a header the VM has de-specialised for `method_key` gets no
/// hoist. `None` (no VM in scope) hoists without that check: a copy that
/// faults costs a deopt to the header, never a wrong answer.
pub(in crate::ir_optimize) fn hoist_invariant_array_loads(
    graph: &mut Graph,
    despec: Option<&crate::deopt::DespecRegistry>,
    method_key: &str,
) -> usize {
    if graph.safepoints.is_empty() {
        return 0;
    }
    let dbg = false;
    let first_pushed = graph.safepoints.len();
    let mut hoisted = 0usize;
    for _ in 0..ARRAY_LOAD_HOIST_ROUNDS {
        let mut progressed = false;
        let headers = loop_headers(graph);
        let mut users = build_users(graph);
        for (region, entry_pred, back_ctrls) in headers {
            if hoisted >= MAX_ARRAY_LOAD_HOISTS {
                break;
            }
            if !matches!(
                graph.node_opt(region).map(|n| &n.op),
                Some(Op::Region | Op::Merge)
            ) {
                continue;
            }
            let Some(plan) = array_load_hoist_plan(graph, &users, region, entry_pred, &back_ctrls)
            else {
                continue;
            };
            if let Some(d) = despec {
                let withdrawn = match u32::try_from(plan.header_bci) {
                    Ok(b) => d.contains(method_key, b),
                    Err(_) => true,
                };
                if withdrawn {
                    if dbg {
                        eprintln!(
                            "[ir-licm] array-load hoist: header {region} bci {} de-specialised",
                            plan.header_bci
                        );
                    }
                    continue;
                }
            }
            let bci = plan.header_bci;
            let n = install_array_load_hoists(graph, plan, MAX_ARRAY_LOAD_HOISTS - hoisted);
            if n > 0 {
                hoisted += n;
                progressed = true;
                users = build_users(graph);
            }
            if dbg {
                eprintln!(
                    "[ir-licm] array-load hoist: header {region} bci {bci}: {n} load(s) to the \
                     pre-header (total {hoisted})"
                );
            }
        }
        if !progressed {
            break;
        }
    }
    drop_unnamed_hoist_frames(graph, first_pushed);
    hoisted
}

/// Run loop-invariant code motion. Returns `true` if it changed the graph.
///
/// See the module-level soundness model. Hoists invariant loads out of each
/// natural loop — `Op::Region` *or* a javac back-edge `Op::Merge` header (see
/// `loop_headers`) — into the loop pre-header (the structurally-classified
/// entry-predecessor control). A hard barrier (call / allocation / guard /
/// monitor) in the body blocks all hoisting; an in-loop `Store` blocks only the
/// loads it could alias (`loop_store_clobber` / `load_safe_past_clobber` — a
/// store to a distinct local allocation cannot clobber a load of another). Pure
/// invariant nodes
/// already float in Sea-of-Nodes, so no placement change is needed for them —
/// but recognising them lets the trailing GVN pass dedup loop-entry vs loop-body
/// copies. The store-alias oracle resolves bases through `Phi`s (cross-merge),
/// so a store via `(cond ? A : B)` is modelled as writing `{A, B}`.
///
/// The historical `bool` spelling, kept for the unit tests, which ask "did this
/// pass do anything" and not "did it make progress".
pub(in crate::ir_optimize) fn licm(graph: &mut Graph) -> bool {
    licm_outcome(graph).any_work()
}

pub(in crate::ir_optimize) fn licm_outcome(graph: &mut Graph) -> LicmOutcome {
    // Normalize the single-input `Op::Merge` control pass-throughs the builder
    // wraps around branch projections, so a javac loop header and its back-edge
    // sit adjacent to their If/Proj (mirrors the unroll pass). Without this,
    // real `Merge`-header loops are not recognised. Idempotent — a no-op when
    // unroll already ran it (or on hand-built `Region` graphs with no trivial
    // merges). Fold any structural change into `moved` — it IS a graph rewrite,
    // it is idempotent, and `optimize()`'s trailing GVN/DCE cleanup wants it.
    let live_before = graph.live_count();
    collapse_trivial_merges(graph);
    let mut changed = graph.live_count() != live_before;
    // Compact-form recognitions, reported apart from `changed`. See
    // [`LicmOutcome`].
    let mut recognized = false;

    let headers = loop_headers(graph);
    if headers.is_empty() {
        return LicmOutcome {
            moved: changed,
            recognized,
        };
    }

    // Non-vacuity diagnostic (mirrors `CRATONVM_DBG_UNROLL`): count the loads
    // this pass actually hoists, so a live soak can confirm LICM fired rather
    // than silently bailing every loop.
    let dbg = false;
    let mut hoisted = 0usize;
    if dbg {
        eprintln!("[DBG_LICM] {} candidate loop header(s)", headers.len());
    }
    // One users map for every header of this visit: see `loop_body_with` for
    // why the hoists below cannot make it stale for what it is read for.
    // The direct-static hoist (round 19, IRLOWER17-1) DOES rewire memory-token
    // edges, so it rebuilds this map whenever it moved something.
    let mut users = build_users(graph);
    // nio21 wave 6 (lane L2): a `getfield` carrying exception edges stays in
    // the body, after which its flags sit (`crate::ir::exception_edge_anchors`):
    // hoisted, its null check would deopt in the pre-header and its edge would
    // be dead. Empty unless the default-OFF edge switches built one.
    let edge_anchors = crate::ir::exception_edge_anchors(graph);

    for (region, entry_pred, back_ctrls) in headers {
        // Re-validate: the header may have been killed by an earlier hoist's
        // cleanup in this same loop set (defensive).
        if !matches!(graph.nodes[region as usize].op, Op::Region | Op::Merge) {
            continue;
        }
        let mut body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        // ── JIT round 19 (lane irops19, IRLOWER17-1 / OI-3): direct `getstatic` ──
        //
        // First, so the arms below see a body without the hoisted reads (an
        // `Op::LoadStatic` is `MemAccess::Opaque` and would otherwise be a
        // hard barrier and a "write" for them). Opt-in. See
        // [`hoist_direct_static_reads`] for the obligations.
        // HOIST11-3 (wave 12, opt-in): the OSR route only.
        if ir_licm_direct_static_enabled()
            && (graph.optimize_osr_route || !ir_licm_direct_static_osr_only_enabled())
            && entry_pred != NO_NODE
        {
            let n = hoist_direct_static_reads(graph, region, entry_pred, &mut body, dbg);
            if n > 0 {
                changed = true;
                hoisted += n;
                users = build_users(graph);
            }
        }
        if dbg {
            let nloads = body
                .iter()
                .filter(|&&id| matches!(graph.nodes[id as usize].op, Op::Load(_)))
                .count();
            // LOOPOPT20-4's first step: name the barriers, so a census can
            // tell the scoped PUT fold from a call, a guard or a volatile.
            let mut barriers: Vec<NodeId> = body
                .iter()
                .copied()
                .filter(|&id| node_is_hard_barrier(graph, &graph.nodes[id as usize]))
                .collect();
            barriers.sort_unstable();
            let named: Vec<String> = barriers
                .iter()
                .take(6)
                .map(|&id| {
                    let n = &graph.nodes[id as usize];
                    let what = match (n.op.clone(), crate::ir::scoped_raw_effect(n)) {
                        (_, Some(e)) if e.is_write() => String::from(" scoped-raw-write"),
                        (_, Some(_)) => String::from(" scoped-raw-read"),
                        (Op::Call { info_ptr }, None) if info_ptr != 0 => {
                            // SAFETY: as `charat_trap::is_seq_charat_call`: a
                            // non-zero `info_ptr` on an `Op::Call` addresses a
                            // `JitInvokeInfo` the planner keeps alive for the
                            // compiled method's life.
                            let info = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
                            format!(
                                " -> {}.{}{}",
                                info.class_name, info.method_name, info.descriptor
                            )
                        }
                        _ => String::new(),
                    };
                    format!("n{id} {:?}{what}", n.op)
                })
                .collect();
            eprintln!(
                "[DBG_LICM] header {region}: body {} node(s), {} load(s), \
                 hard_barrier={} writes_memory={} barriers=[{}]",
                body.len(),
                nloads,
                loop_has_hard_barrier(graph, &body),
                loop_writes_memory(graph, &body),
                named.join("; ")
            );
        }
        // The pre-header is the classified loop-entry predecessor — NOT
        // necessarily input slot 0, since a `Merge` header may carry the
        // back-edge at either slot.
        let preheader = entry_pred;
        // One question about the LOOP, asked once, consumed by every load this
        // header offers: may a hoist to this pre-header speculate at all?
        let body_definitely_runs = loop_body_definitely_runs(graph, region, &back_ctrls);
        // ── Loop-invariant `Op::IndyRootRef` ────────────────────────────────
        // `Op::IndyRootRef` reads an immutable constant GC root slot and cannot
        // trap (`may_raise == false`), so hoisting it to `preheader` before
        // hard-barrier checks lets downstream passes (sync block coarsening,
        // BCE, splice folds) treat it as invariant. Guarded on OSR preheader
        // dominance so OSR entry stubs never read an unmaterialized ref.
        if preheader != NO_NODE && !body.contains(&preheader) {
            let osr_safe = (!graph.optimize_osr_route && !graph.osr_compile)
                || if header_is_spliced(graph, region) {
                    spliced_loop_entered_only_through_its_pre_header(graph, region, &body)
                } else {
                    loop_entered_only_through_its_pre_header(graph, region, preheader, &body)
                };
            if osr_safe {
                let root_refs: Vec<NodeId> = (0..graph.nodes.len() as NodeId)
                    .filter(|&id| {
                        let n = &graph.nodes[id as usize];
                        matches!(n.op, Op::IndyRootRef { .. })
                            && n.inputs.len() == 1
                            && body.contains(&n.inputs[0])
                    })
                    .collect();
                for rr in root_refs {
                    graph.set_input(rr, 0, preheader);
                    changed = true;
                    hoisted += 1;
                }
            }
        }
        // ── Loop-invariant `Op::ArrayLength` ────────────────────────────────
        //
        // Run BEFORE the pre-header guard and the hard-barrier bail below, and
        // deliberately so on both counts. Taking the barrier first: an
        // in-loop `ArrayLength` IS one of those barriers (it is not pure, and
        // it is not a `Load`/`Store`/`Phi`), so a javac counted loop —
        // `for (i = 0; i < a.length; i++)` — disqualified its own LICM by the
        // very node this hoists. Measured on `probes/ArrayElemLoadCost.java`
        // with `CRATONVM_DBG_LICM=1`: every candidate header reported
        // `hard_barrier=true, 0 load(s)`, so the pass did nothing at all on the
        // one loop shape it most needed to.
        //
        // What makes this hoistable when the general load hoist below is
        // blocked:
        //
        // * **The value cannot change.** Nothing writes an array's length, so
        //   `AliasClass::ArrayLength` conflicts with no store (see the alias
        //   oracle's own note in `ir.rs`). The memory token is therefore pure
        //   ordering for this node, and re-anchoring it to the loop's ENTRY
        //   memory is value-preserving rather than a claim about aliasing.
        //   Without re-anchoring the memory the hoist would be inert:
        //   `ir_schedule::find_best_block` places a data node in the deepest
        //   block dominated by ALL its input blocks, so a node still reading
        //   the loop's memory phi stays in the loop no matter where its control
        //   points.
        //
        // * **The throw point does not move.** `ArrayLength` raises NPE on a
        //   null receiver, which is why it is impure. It is only taken when its
        //   control input IS the loop header region — i.e. it executes on every
        //   entry to the header, so in particular on the first — and the
        //   pre-header runs immediately before that first execution with
        //   nothing observable in between. `preheader_may_trap` refuses the
        //   remaining case, a pre-header block that can itself throw, where
        //   hoisting could report the NPE ahead of an exception that came first.
        //
        //   "Executes on every entry to the header" is a claim about the
        //   CONTROL EDGE, and `control_token_has_guard` is the case where the
        //   control edge is not the whole story: an `Op::Guard` anchored at the
        //   same region can leave the compiled code before this node runs, so a
        //   node behind one does NOT execute on every entry to the header. See
        //   that function for what a guard at the header licenses and why this
        //   arm cannot see which node it licenses.
        //
        //   `control_token_has_guard` answers "is there a guard here at all",
        //   which used to be the whole verdict. It is now the *diagnostic*
        //   spelling of that; the verdict is [`LoopGuards`], which asks which
        //   VALUES those guards constrain and lets a read computed from none of
        //   them out — at the price of a strictly stronger permission
        //   (`base_non_null_on_entry`, so the read cannot deopt and the hoist
        //   cannot move a deopt point). See that type for both obligations and
        //   for why no guard is ever moved.
        //
        // Both are computed once, here, BEFORE this arm repoints any control
        // edge: `header_guards` for nodes the header itself anchors, and
        // `body_guards` for nodes anchored deeper, which any guard in the body
        // may precede.
        let header_guarded = control_token_has_guard(graph, region);
        let header_guards = LoopGuards::at_control(graph, region);
        let body_guards = LoopGuards::in_body(graph, &body);
        let al_ids: Vec<NodeId> = body
            .iter()
            .copied()
            .filter(|&id| matches!(graph.nodes[id as usize].op, Op::ArrayLength))
            .collect();
        if !al_ids.is_empty() && !preheader_may_trap_except_header_resumes(graph, preheader, region)
        {
            let entry_mem = loop_entry_memory(graph, region, entry_pred);
            for al in al_ids {
                let inputs = graph.nodes[al as usize].inputs.clone();
                // Only the full `[ctrl, mem, array]` form has a control slot to
                // repoint; the compact EA-bridge form is already schedulable by
                // invariance alone.
                if inputs.len() < 3 {
                    continue;
                }
                if inputs[0] != region {
                    if dbg {
                        eprintln!(
                            "[DBG_LICM] arraylength {al}: skip — control {} is not the header {region}",
                            inputs[0]
                        );
                    }
                    continue;
                }
                let array = inputs[2];
                if !is_loop_invariant(graph, array, region, &body) {
                    if dbg {
                        eprintln!("[DBG_LICM] arraylength {al}: skip — array {array} variant");
                    }
                    continue;
                }
                // `inputs[0] == region` was checked above, so only guards at
                // the header can be in front of this node — see
                // `LoopGuards::at_control`. An unguarded header answers `true`
                // unconditionally and this arm behaves exactly as it did.
                if !header_guards.permits_hoist(graph, al, array, NO_NODE) {
                    if dbg {
                        eprintln!(
                            "[DBG_LICM] arraylength {al}: skip — a guard at header {region} may \
                             be the reason it is legal (header_guarded={header_guarded})"
                        );
                    }
                    continue;
                }
                let new_mem = if is_loop_invariant(graph, inputs[1], region, &body) {
                    inputs[1]
                } else {
                    match entry_mem {
                        Some(m) => m,
                        None => {
                            if dbg {
                                eprintln!(
                                    "[DBG_LICM] arraylength {al}: skip — no loop-entry memory"
                                );
                            }
                            continue;
                        }
                    }
                };
                // This arm is idempotent without a test of its own: the
                // `inputs[0] != region` refusal above is it. A node this arm
                // already served has its control at the pre-header, so a second
                // visit skips it there and `changed` stays honest. The other
                // two arms accept a node whose control is NOT the header and
                // therefore each carry an explicit "already there" test; see
                // [`LicmOutcome`] for why that honesty is load-bearing.
                //
                // A null `array` deopts from the pre-header now: give that deopt
                // a first-iteration frame, or refuse (round 11 wave 2).
                if !base_non_null_on_entry(graph, array)
                    && !install_preheader_deopt_frame(
                        graph,
                        al,
                        region,
                        entry_pred,
                        &body,
                        body_definitely_runs,
                    )
                {
                    if dbg {
                        eprintln!(
                            "[DBG_LICM] arraylength {al}: skip — its deopt frame cannot be \
                             described at the pre-header"
                        );
                    }
                    continue;
                }
                if base_non_null_on_entry(graph, array) {
                    name_preheader_frame_best_effort(
                        graph,
                        al,
                        region,
                        entry_pred,
                        &body,
                        body_definitely_runs,
                    );
                }
                if dbg {
                    eprintln!(
                        "[DBG_LICM] arraylength {al} (inputs {inputs:?}): HOIST to preheader \
                         {preheader}, mem {} -> {new_mem}",
                        inputs[1]
                    );
                }
                // Through the tracked mutator (round 12 wave 2): a direct write
                // to `inputs` bumps the edge epoch, and the next `users_of`
                // (the trailing load CSE, DSE's splice) then rebuilds every
                // use list of the graph. Same edges, same answer.
                graph.set_input(al, 0, preheader);
                graph.set_input(al, 1, new_mem);
                changed = true;
                hoisted += 1;
            }
        }

        // ── Restricted invariant `Op::Load` hoist ───────────────────────────
        //
        // For a loop the STRICT rule refuses and `loop_writes_memory` clears:
        // its body reads and guards, and writes nothing. Placed here, beside
        // the `Op::ArrayLength` arm and above the pre-header guard below, for
        // the same reason that arm is: for a nested INNER loop `body`
        // over-approximates and drags the enclosing loop's pre-header in with
        // it, and `loop_headers` has already established structurally that
        // `entry_pred` is outside the natural loop. Over-approximation is the
        // safe direction for every test below — a bigger `body` makes
        // `is_loop_invariant` stricter, so it can only refuse a hoist, never
        // admit a wrong one.
        //
        // Three obligations, the same three the `ArrayLength` arm discharges:
        //
        // 1. **The value cannot change.** Nothing in the body writes memory
        //    (`loop_writes_memory`), and the base is loop-invariant. So the
        //    load reads the same location and the same value on every
        //    iteration, and computing it once at the pre-header is
        //    value-preserving rather than a claim about aliasing.
        //
        // 2. **The throw point does not move.** An `Op::Load` deopts (it does
        //    not fault) on a null base, and a deopt raised from the pre-header
        //    would rebuild an interpreter frame at a bci the loop never
        //    reached. This is `hoist_is_safe`, the SAME permission the general
        //    load hoist below asks for, and it is asked here verbatim.
        //
        //    ── What this used to ask instead, and why it was circular ──
        //
        //    Until 2026-09-17 this arm had a permission of its own:
        //    `header_derefs`, the set of bases of `Load`/`ArrayLength` nodes
        //    whose control input is the header region. Membership was read as
        //    proof that a null base would already have faulted at the header,
        //    so the hoist moved no trap.
        //
        //    It proved nothing. The loops this arm exists for are exactly the
        //    ones whose header carries `Op::Guard`s — that is why they are
        //    `loop_has_hard_barrier` and not `loop_writes_memory` — and in
        //    this IR a guard and the nodes it protects share one control
        //    input. So the header-anchored loads that populated the set are
        //    themselves reached only when the guard has already deopted on the
        //    null case: the evidence was a conditionally-executed load
        //    licensing an unconditionally-executed one. The IR String
        //    expansion is the exact shape (`Guard(recv != null)` then two
        //    `Load`s off `recv`, all at the region), and hoisting either load
        //    above the guard raised the deopt from a pre-header frame at the
        //    in-body `charAt` bci — the failure obligation 2 is written
        //    against.
        //
        //    Two arms of one pass answered one question with two different
        //    rigours, and the weaker one was the default-ON one. The
        //    `header_derefs` set is gone; what remains is the general arm's
        //    `hoist_is_safe`, plus `header_guarded`, which is the part
        //    `hoist_is_safe` does not need (the general arm never sees a
        //    guard — `loop_has_hard_barrier` bails it out first).
        //
        //    Deleting `header_derefs` also closes the phase-ordering bug it
        //    had with the `ArrayLength` arm above: that arm runs first and
        //    repoints exactly the nodes whose `inputs[0] == region` test
        //    populated the set, so every base whose only header dereference
        //    was an `arraylength` — i.e. every `for (i = 0; i < a.length;
        //    i++)` — was missing from it by the time it was built. There is
        //    now no evidence set to erase.
        //
        // 3. **The memory token is re-anchored.** Without it the hoist is
        //    INERT whenever the body threads memory through a read:
        //    `ir_schedule::find_best_block` places a data node in the deepest
        //    block dominated by all its inputs, so a load still reading the
        //    loop's memory phi stays in the loop however its control points.
        if licm_read_hoist_enabled()
            && preheader != NO_NODE
            && loop_has_hard_barrier(graph, &body)
            && (!loop_writes_memory(graph, &body)
                || primitive_array_field_hoist(graph, &body)
                || field_store_disjoint_hoist(graph, &body))
            // The late round's copies run last in the pre-header (see below).
            && (graph.late_licm || !preheader_may_trap(graph, preheader))
        {
            let body_writes = loop_writes_memory(graph, &body);
            let entry_mem = loop_entry_memory(graph, region, entry_pred);
            let read_load_ids: Vec<NodeId> = body
                .iter()
                .copied()
                .filter(|&id| matches!(graph.nodes[id as usize].op, Op::Load(_)))
                .filter(|id| !edge_anchors.contains(id))
                .collect();
            for load in read_load_ids {
                let inputs = graph.nodes[load as usize].inputs.clone();
                // Full `[ctrl, mem, base, offset?]` form only: the compact
                // EA-bridge shapes carry no control or memory slot to move,
                // and this arm's whole content is moving those two.
                if inputs.len() < 3 {
                    continue;
                }
                let base = inputs[2];
                let addr = if inputs.len() >= 4 {
                    inputs[3]
                } else {
                    NO_NODE
                };
                if !is_loop_invariant(graph, base, region, &body) {
                    if dbg {
                        eprintln!("[DBG_LICM] read-hoist load {load}: skip — base {base} variant");
                    }
                    continue;
                }
                if addr != NO_NODE && !is_loop_invariant(graph, addr, region, &body) {
                    if dbg {
                        eprintln!("[DBG_LICM] read-hoist load {load}: skip — addr {addr} variant");
                    }
                    continue;
                }
                if body_writes && body_write_may_alias(graph, &body, load) {
                    if dbg {
                        eprintln!(
                            "[DBG_LICM] read-hoist load {load}: skip — a body write may alias it"
                        );
                    }
                    continue;
                }
                // Late round: a COPY at the end of the pre-header, numbered after
                // every node there (a block schedules impure nodes in id order),
                // so after the predicate guards; only a read that cannot trap.
                if graph.late_licm {
                    let covering = if inputs[0] == region {
                        &header_guards
                    } else {
                        &body_guards
                    };
                    // A body type test of `base` (a splice `If`, not a guard) may be
                    // what makes this read's offset right; keep such reads.
                    let body_types_base = body.iter().any(|&id| {
                        graph.node_opt(id).is_some_and(|n| {
                            matches!(
                                n.op,
                                Op::ExactClassIs { .. }
                                    | Op::InstanceOf { .. }
                                    | Op::CheckCast { .. }
                            ) && n.inputs.contains(&base)
                        })
                    });
                    let non_null = late_base_non_null(graph, base, preheader);
                    let permitted =
                        covering.permits_hoist_proven(graph, load, base, addr, non_null);
                    if body_types_base || !permitted || !non_null {
                        if dbg {
                            eprintln!(
                                "[DBG_LICM] late read-hoist load {load}: skip — base {base} typed_in_body={body_types_base} guards_permit={permitted} non_null={non_null}"
                            );
                        }
                        continue;
                    }
                    let new_mem = if is_loop_invariant(graph, inputs[1], region, &body) {
                        inputs[1]
                    } else {
                        match entry_mem {
                            Some(m) => m,
                            None => continue,
                        }
                    };
                    let licensing_guard = late_base_licensing_guard(graph, base, preheader);
                    let copy = late_hoist_copy_guarded(graph, load, preheader, new_mem, licensing_guard);
                    if dbg {
                        eprintln!("[DBG_LICM] late read-hoist load {load} -> n{copy} at preheader {preheader}");
                    }
                    changed = true;
                    hoisted += 1;
                    continue;
                }
                // The general load hoist's permission, verbatim — see
                // obligation 2 above — with one extra conjunct this arm needs
                // and that one does not. `inputs[0] == region` and
                // `anchored_on_every_iteration` are POSITIONAL claims ("this
                // node runs on the first pass through the header"), and a
                // `Guard` at the header falsifies both: it can leave the
                // compiled code before the node behind it is reached. The
                // general arm never has to ask, because `loop_has_hard_barrier`
                // has already bailed it out of any loop containing a guard;
                // this arm is the one that runs FOR those loops.
                //
                // `base_non_null_on_entry` is not a positional claim — it says
                // the load cannot trap at all — and it is the ONE permission
                // that survives under a guard, because it is the one that keeps
                // a hoist from moving a deopt point. It is not sufficient on
                // its own: a guard also establishes something about its operand
                // that the nodes behind it may rely on, and a guard proving an
                // object's SHAPE rather than its nullity makes a read behind it
                // a read at an offset the object may not have. `LoopGuards`
                // answers that second half — the read's address must be
                // computed from values no covering guard constrains — and
                // refuses whenever it cannot.
                //
                // Which guards cover this load depends on where it is anchored:
                // a load the header itself anchors can only be behind a guard
                // at the header, while one anchored deeper can be behind any
                // guard in the body.
                let covering = if inputs[0] == region {
                    &header_guards
                } else {
                    &body_guards
                };
                let hoist_is_safe = covering.permits_hoist(graph, load, base, addr)
                    && (inputs[0] == region
                        || base_non_null_on_entry(graph, base)
                        || (body_definitely_runs
                            && anchored_on_every_iteration(graph, inputs[0], region, &body)));
                if !hoist_is_safe {
                    if dbg {
                        eprintln!(
                            "[DBG_LICM] read-hoist load {load}: skip — base {base} may be null \
                             at the pre-header, or a guard may be what makes the read legal \
                             (header_guarded={header_guarded})"
                        );
                    }
                    continue;
                }
                let new_mem = if is_loop_invariant(graph, inputs[1], region, &body) {
                    inputs[1]
                } else {
                    match entry_mem {
                        Some(m) => m,
                        None => {
                            if dbg {
                                eprintln!(
                                    "[DBG_LICM] read-hoist load {load}: skip — no loop-entry memory"
                                );
                            }
                            continue;
                        }
                    }
                };
                if inputs[0] == preheader && inputs[1] == new_mem {
                    continue;
                }
                // A load that can still deopt from the pre-header needs a
                // first-iteration frame, or stays (round 11 wave 2).
                if inputs[0] != preheader
                    && !base_non_null_on_entry(graph, base)
                    && !install_preheader_deopt_frame(
                        graph,
                        load,
                        region,
                        entry_pred,
                        &body,
                        body_definitely_runs,
                    )
                {
                    if dbg {
                        eprintln!(
                            "[DBG_LICM] read-hoist load {load}: skip — its deopt frame cannot \
                             be described at the pre-header"
                        );
                    }
                    continue;
                }
                if inputs[0] != preheader && base_non_null_on_entry(graph, base) {
                    name_preheader_frame_best_effort(
                        graph,
                        load,
                        region,
                        entry_pred,
                        &body,
                        body_definitely_runs,
                    );
                }
                if dbg {
                    eprintln!(
                        "[DBG_LICM] read-hoist load {load} (inputs {inputs:?}): HOIST to \
                         preheader {preheader}, mem {} -> {new_mem}",
                        inputs[1]
                    );
                }
                // Tracked, as in the `ArrayLength` arm above.
                graph.set_input(load, 0, preheader);
                if inputs[1] != new_mem {
                    rethread_hoisted_load_token_users(graph, load, inputs[1], &body);
                }
                graph.set_input(load, 1, new_mem);
                changed = true;
                hoisted += 1;
            }
        }

        if preheader == NO_NODE || body.contains(&preheader) {
            // No identifiable pre-header outside the loop → cannot hoist.
            //
            // (Round 11: `loop_body`'s sweep no longer leaks out through the
            // loop exit, so for a nested inner loop the pre-header is no longer
            // dragged in and this guard no longer skips every inner loop. The
            // history below is why it sits where it does.)
            //
            // Runs AFTER the `ArrayLength` hoist above, and deliberately: for a
            // nested inner loop `body` over-approximated badly (the sweep dragged
            // the whole enclosing loop in behind the inner exit, pre-header
            // included), and `loop_headers` has ALREADY established that
            // `entry_pred` is outside the natural loop — by reachability, or by
            // dominance for the inner-loop case it added. This guard is a
            // belt-and-braces check against that over-approximation, not the
            // structural claim, so the arm above does not owe it. The general
            // load hoist below still does: it reasons about aliasing against a
            // body it must not under-read.
            continue;
        }

        // A hard barrier (call / allocation / guard / monitor) could read or
        // write arbitrary memory → no hoisting from this loop at all.
        if loop_has_hard_barrier(graph, &body) {
            continue;
        }
        // An in-loop store does NOT bail the whole loop: a load past it is
        // gated per-load by the alias oracle below. Summarise the stores once —
        // the set of local allocations they may write, or `None` if any store is
        // opaque (then no load hoists past them). Only paid when a store exists.
        let body_has_store = body
            .iter()
            .any(|&id| matches!(graph.nodes[id as usize].op, Op::Store(_)));
        let store_clobber = if body_has_store {
            loop_store_clobber(graph, &body)
        } else {
            None
        };

        // Collect hoistable loads: in the body, with invariant base/address,
        // typed as a real Load. We snapshot ids first (we mutate inputs after).
        let load_ids: Vec<NodeId> = body
            .iter()
            .copied()
            .filter(|&id| matches!(graph.nodes[id as usize].op, Op::Load(_)))
            .filter(|id| !edge_anchors.contains(id))
            .collect();

        // The memory token to use at the pre-header: the region's *entry*
        // memory phi input if a memory phi exists, else any invariant memory.
        // For loads in the EA-bridge/hand-built compact form (`[base]` or
        // `[base, index]`) there is no control/mem operand to repoint, so the
        // hoist is purely a re-anchor that GVN/scheduling already honour; we
        // still record `changed` so the trailing cleanup runs.
        for load in load_ids {
            let inputs = graph.nodes[load as usize].inputs.clone();
            // Identify base / address operands across both layouts:
            //   compact:  [base] | [base, index]
            //   full:     [ctrl, mem, base, index?]
            let (base, addr) = match inputs.len() {
                1 => (inputs[0], NO_NODE),
                2 => {
                    // Disambiguate compact [base, index] from a 2-input full
                    // form by the type of slot 0: a Memory-typed slot 0 means
                    // the full form is malformed here (we only see compact),
                    // so treat slot 0 as base.
                    (inputs[0], inputs[1])
                }
                3 => (inputs[2], NO_NODE),             // [ctrl, mem, base]
                n if n >= 4 => (inputs[2], inputs[3]), // [ctrl, mem, base, index]
                _ => (NO_NODE, NO_NODE),
            };
            if !is_loop_invariant(graph, base, region, &body) {
                if dbg {
                    eprintln!(
                        "[DBG_LICM] load {load} (inputs {inputs:?}): skip — base {base} variant ({:?})",
                        graph.nodes[base as usize].op
                    );
                }
                continue;
            }
            if addr != NO_NODE && !is_loop_invariant(graph, addr, region, &body) {
                if dbg {
                    eprintln!("[DBG_LICM] load {load}: skip — addr {addr} variant");
                }
                continue;
            }
            // If the body has in-loop stores, the load may only hoist past them
            // when it cannot alias any of them: an opaque store (`None`) blocks
            // every load, otherwise the load's points-to alloc set must be
            // disjoint from what the stores write. Without a store this check is
            // skipped (pure invariant-load loop — the historical case).
            if body_has_store {
                let safe = match &store_clobber {
                    Some(clob) => load_safe_past_clobber(graph, base, clob),
                    None => false,
                };
                if !safe {
                    if dbg {
                        eprintln!("[DBG_LICM] load {load}: skip — may alias in-loop store");
                    }
                    continue;
                }
            }
            if dbg {
                eprintln!(
                    "[DBG_LICM] load {load} (inputs {inputs:?}): HOIST base {base} addr {addr}"
                );
            }
            // The load is invariant and not clobbered by any in-loop store → safe
            // to compute once at the pre-header. Remove it from the body by
            // repointing its control input (slot 0 of the full form) to the
            // pre-header so scheduling/lowering place it before the loop. For
            // the compact form there is no control slot, so invariance alone
            // (already established) makes it loop-entry schedulable.
            if inputs.len() >= 3 {
                // Full form: repoint ctrl (slot 0) to the pre-header control.
                //
                // Slot 1 is the MEMORY edge, and until 2026-09-11 this arm left
                // it alone — which made the hoist cosmetic. `ir_schedule`'s
                // `find_best_block` places a data node in the DEEPEST block
                // dominated by every one of its input blocks, and a body load's
                // memory input is the header's memory phi, whose block IS the
                // header. So the load was re-anchored to the pre-header in the
                // graph and scheduled straight back into the loop in the code.
                //
                // Measured on `probes/FieldLoop.java`'s `sum` — the loop the
                // tiering inversion is quoted on — `CRATONVM_DBG_LICM=1` printed
                // `hoisted 1 invariant load(s) to loop pre-header(s)` while the
                // emitted body still ran the whole `getfield` sequence every
                // iteration: epoch guard, null test, compact test, the read and
                // the jump over the legacy arm, nine of its ~24 hot-path
                // instructions. A hoist nothing observes is not a hoist.
                //
                // The read-hoist arm above has always moved BOTH edges, with
                // this same `loop_entry_memory`. This is that rewrite, in the
                // arm that reaches the loops the other one declines (it fires
                // only for a loop `licm_read_hoist_enabled` singles out — one
                // that only its own reads and guards disqualify — and
                // `FieldLoop.sum` has no disqualifying barrier at all, so it
                // never went near it).
                //
                // `CRATONVM_JIT_IR_LICM_MEM_EDGE=0` turns it off. Default
                // **ON** since 2026-09-11 — it landed off, and flipped in the
                // same commit on the measurement (0.589x on `sum`, 0.472x on
                // `sumWide`). It changes where a load lands in every method
                // that hoists one, which is why it kept a kill switch.
                //
                // ── Speculation: the pre-header runs when the body does not ──
                //
                // A hoist to the pre-header is SPECULATIVE. The pre-header runs
                // on every entry and the body does not, so a load that reaches
                // the pre-header executes for a loop that iterates zero times.
                // A `getfield` carries its own null check, so speculating one
                // speculates the `NullPointerException` with it.
                //
                // That is not hypothetical. `probes/ZeroTripHoist.java` is
                // `static int walk(N o, int n) { int a = 0; for (int i = 0;
                // i < n; i++) a += o.v; return a; }`, and `walk(null, 0)` came
                // back as a thrown NPE where HotSpot returns 0 — on the
                // optimizing tier, from THIS arm: it survives
                // `CRATONVM_JIT_NO_LICM_READ_HOIST=1` and vanishes under
                // `CRATONVM_JIT_LICM=0`. A wrong answer, not a crash — and it
                // reproduced under `CRATONVM_C2_SUPERSEDE=0` too, which stops
                // the optimizing body from SUPERSEDING and not from being
                // compiled.
                //
                // So the hoist asks whether it may speculate, and three answers
                // are accepted:
                //
                //   * the load is anchored at the HEADER itself — the header
                //     runs whenever the loop is reached, zero trips included,
                //     so the pre-header is not earlier in any execution;
                //   * the base is the RECEIVER, which the JVM guarantees
                //     non-null at the call site and SSA gives no other
                //     definition — the fact `ir_check_elim` already seeds;
                //   * `definitely_non_null` already answers for the base (a
                //     fresh allocation, a materialized constant);
                //   * the BODY DEFINITELY RUNS — a constant-trip counted loop
                //     with at least one trip. Then the pre-header is not
                //     speculation at all: every fault the hoisted load can
                //     raise, the first iteration was going to raise anyway.
                //     This is the only one of the four that is a proof about
                //     the LOOP rather than about the base, and it is narrow on
                //     purpose — see [`loop_body_definitely_runs`] for what it
                //     does and does not reach.
                //
                // This is a permission, not an analysis: anything else refuses,
                // and a refusal costs an optimization rather than an answer. It
                // is deliberately NOT behind `CRATONVM_JIT_IR_LICM_MEM_EDGE` —
                // the control-edge move below is the one that was already
                // wrong, and it is default-ON.
                // `body_definitely_runs` proves the FIRST ITERATION happens,
                // not that this load runs in it: a load under a condition in
                // the body (`for (...) if (h != null) s += h.x;`) is skipped
                // on exactly the path the condition exists for, and hoisting it
                // raised the NPE in the pre-header anyway. So that permission
                // also needs the load to be anchored on every iteration's
                // spine.
                //
                // `base_non_null_on_entry` now answers for BOTH of the
                // reference sources listed above — the receiver parameter and
                // `definitely_non_null`'s ops — so the second bullet and the
                // third are one call. The separate `definitely_non_null` call
                // this arm used to make was what kept the predicate honest
                // while its own body implemented only half its doc; the body
                // implements both now.
                //
                // The `LoopGuards` conjunct is a no-op here by construction and
                // is written anyway. This arm is only reached when
                // `loop_has_hard_barrier` is false, and `licm_hoist_barrier`
                // classifies `Op::Guard`, so a guard anywhere in the body has
                // already sent this loop to the restricted arm above — meaning
                // `body_guards` is `LoopGuards::None` and this answers `true`.
                // Stating it keeps the two arms asking one question: if that
                // barrier classification ever loosens, this arm does not
                // silently become the one with no guard rule.
                let hoist_is_safe = body_guards.permits_hoist(graph, load, base, addr)
                    && (inputs[0] == region
                        || base_non_null_on_entry(graph, base)
                        || (body_definitely_runs
                            && anchored_on_every_iteration(graph, inputs[0], region, &body)));
                if !hoist_is_safe {
                    if dbg {
                        eprintln!(
                            "[DBG_LICM] load {load}: NOT hoisted — base {base} may be null at \
                             the pre-header and the body may not run"
                        );
                    }
                    continue;
                }
                let new_mem = if licm_mem_edge_enabled() {
                    if is_loop_invariant(graph, inputs[1], region, &body) {
                        inputs[1]
                    } else {
                        loop_entry_memory(graph, region, entry_pred).unwrap_or(NO_NODE)
                    }
                } else {
                    NO_NODE
                };
                let move_mem = new_mem != NO_NODE && new_mem != inputs[1];
                // The memory edge decides the block (`find_best_block`): a load
                // whose memory stays a body state is scheduled back into the
                // loop whatever its control says, BELOW the pre-header it now
                // names — the shape the release post-schedule check refuses
                // since round 11 wave 11 (`ir_schedule::anchor_rule_is_released`).
                // Only reachable with `CRATONVM_JIT_IR_LICM_MEM_EDGE=0` or no
                // loop-entry memory; such a hoist moved nothing out of the loop.
                if new_mem == NO_NODE && !is_loop_invariant(graph, inputs[1], region, &body) {
                    if dbg {
                        eprintln!(
                            "[DBG_LICM] load {load}: NOT hoisted — its memory {} stays in the \
                             loop",
                            inputs[1]
                        );
                    }
                    continue;
                }
                // A load that can still deopt from the pre-header needs a
                // first-iteration frame, or stays (round 11 wave 2).
                if graph.nodes[load as usize].inputs[0] != preheader
                    && !base_non_null_on_entry(graph, base)
                    && !install_preheader_deopt_frame(
                        graph,
                        load,
                        region,
                        entry_pred,
                        &body,
                        body_definitely_runs,
                    )
                {
                    if dbg {
                        eprintln!(
                            "[DBG_LICM] load {load}: NOT hoisted — its deopt frame cannot be \
                             described at the pre-header"
                        );
                    }
                    continue;
                }
                if graph.nodes[load as usize].inputs[0] != preheader
                    && base_non_null_on_entry(graph, base)
                {
                    name_preheader_frame_best_effort(
                        graph,
                        load,
                        region,
                        entry_pred,
                        &body,
                        body_definitely_runs,
                    );
                }
                if graph.nodes[load as usize].inputs[0] != preheader || move_mem {
                    // Tracked, as in the `ArrayLength` arm above.
                    graph.set_input(load, 0, preheader);
                    if move_mem {
                        rethread_hoisted_load_token_users(graph, load, inputs[1], &body);
                        graph.set_input(load, 1, new_mem);
                        if dbg {
                            eprintln!(
                                "[DBG_LICM] load {load}: mem {} -> {new_mem} (the edge that \
                                 decides the block)",
                                inputs[1]
                            );
                        }
                    }
                    changed = true;
                    hoisted += 1;
                }
            } else {
                // Compact form: there is no control or memory slot to repoint,
                // so nothing was rewritten and nothing ever will be — the hoist
                // is invariance alone, which scheduling already honours.
                //
                // Reported as a RECOGNITION and not as a change, which is the
                // whole of `LicmOutcome`. The trailing GVN still wants to run
                // once so duplicate compact reads dedup to one, and
                // `run_licm_pass` runs it on `any_work()`; but the answer here
                // is identical on every visit, so feeding it to `optimize()`'s
                // outer round as progress made that round undriveable and
                // pinned it at a budget of two.
                recognized = true;
                hoisted += 1;
            }
        }
    }
    if dbg && hoisted > 0 {
        eprintln!("[DBG_LICM] hoisted {hoisted} invariant load(s) to loop pre-header(s)");
    }
    LicmOutcome {
        moved: changed,
        recognized,
    }
}

/// A load LICM moves to the pre-header and re-points at the loop-entry
/// memory (`mem -> entry`) may itself be a link of the loop's memory chain:
/// the builder threads every access, reads included, so the next in-loop
/// access names the load as its memory token. Left alone, those users'
/// chain jumped from the load straight to the ENTRY state, skipping every
/// store the iteration made before them -- the loop's back-edge memory no
/// longer reached the stores ahead of the hoisted read, so ordering held only
/// positionally, and every pass that WALKS the chain stopped at the hoisted
/// load (`loop_field_forward`'s fresh-cell walk: `R20PLoopStaticBarrier`'s
/// `a.v` / `a.w` were refused "no exit value", the walk stopping at the
/// hoisted `this.k` / `this.bias`).
///
/// A read writes nothing, so the memory state after it is the state it read:
/// each in-loop token user of `load` is re-threaded to `old_mem`, the load's
/// memory input before the move -- the same state, now named directly.
/// Value users keep the load. Users outside the loop (an exit's memory merge)
/// are left: the hoisted read still dominates them and the entry state they
/// would see through it is the loop's, unchanged by the read.
pub(in crate::ir_optimize) fn rethread_hoisted_load_token_users(
    graph: &mut Graph,
    load: NodeId,
    old_mem: NodeId,
    body: &FxHashSet<NodeId>,
) {
    if old_mem == NO_NODE {
        return;
    }
    for user in graph.users_of(load) {
        if user == load || !body.contains(&user) {
            continue;
        }
        let Some(node) = graph.node_opt(user) else {
            continue;
        };
        let slots: Vec<usize> = node
            .inputs
            .as_slice()
            .iter()
            .enumerate()
            .filter(|&(i, &x)| x == load && crate::ir::is_memory_token_slot(node, i))
            .map(|(i, _)| i)
            .collect();
        for slot in slots {
            graph.set_input(user, slot, old_mem);
        }
    }
}

// ── Dead Store Elimination (DSE) ─────────────────────────────────────
//
// `eliminate_dead_nodes` (below) is a *value* DCE: it removes nodes whose
// result no node reads.  It cannot remove a store, because a store produces
// no value its consumers depend on — its effect is on memory, and DCE keeps
// any node transitively reachable from the exit through the memory-token /
// input chain.  DSE is the complementary pass that targets the *side
// effect*: a `Store` whose written location is provably overwritten by a
// later store before any read, or that is never read at all, performs no
// observable work and can be deleted.
//
// SOUNDNESS MODEL (deliberately conservative — removing a store that *was*
// observed is silent data loss):
//
//   * We only ever remove a store to a **provably-local, non-escaping
//     allocation** (`New`/`NewArray` base).  A store through a Param, a
//     loaded reference, a phi, a call result, or any base we cannot resolve
//     to a fresh allocation in this method is left untouched — another
//     thread, the caller, or a callee might observe it.
//
//   * "Overwritten before any read" is decided by an ordered scan in
//     node-id order.  A store `S` to location `L = (base, idx_key, kind)` is
//     dead iff a *later* store `S'` writes the same `L` with **no** node
//     between them that could read or alias memory.  The instant we see *any*
//     non-pure, non-store node — `Load`, `Call`, `Return`, `New`/`NewArray`
//     (constructor effects), `Guard`, `ArrayLength`, a monitor op, OR any
//     control / merge / phi / projection node — we flush every pending store
//     (give up — conservative).
//
//     WHY THIS IS SOUND DESPITE NODE-ID ORDER NOT BEING GLOBAL PROGRAM ORDER:
//     in this Sea-of-Nodes IR, node id is creation order, which is a faithful
//     linearisation only *within a single straight-line region*.  Across a
//     branch or join the order is meaningless.  But every control node
//     (`If`, `Merge`, `Region`, `Proj`, `Start`, `Return`) and every `Phi`
//     is non-pure, so it is a barrier that flushes the pending set.  Two
//     stores therefore only ever match when no control node lies between
//     them — i.e. they are in the same straight-line region where node-id
//     order *is* a valid before/after relationship.  This is intentionally
//     coarse: it fires on the common straight-line "init then overwrite" and
//     "write-only scratch field" shapes without needing a real alias oracle.
//
//   * The location key uses the *base node id*, the *index/offset node id*,
//     and the `MemKind`.  Two stores match only when all three are
//     structurally identical.  Because the base is a single SSA allocation
//     node, equal base ids are the same object; differing `MemKind`/idx are
//     different slots and never matched.
//
//     A store whose layout carries NO offset operand (`idx == NO_NODE`) does
//     not name a field at all, and is therefore not matchable in either
//     direction — see `store_matchable_location`.  `MemKind` is an access
//     *width*, not a field index: two distinct `int` fields of the same object
//     share it, so keying on it alone conflates them.  The single exception is
//     an allocation with exactly one field, where "the object's storage" IS
//     that field.  This is the may-alias/must-alias distinction: the memory
//     model calls the no-offset case `ir::AccessOffset::Absent` and defines it
//     as a MAY-alias, and a deletion needs a MUST-alias.
//
// WIDENED CASE — WRITE-ONLY, NEVER-READ (Increment 2):
//
//   The straight-line "overwritten before any read" phase above only fires
//   inside one barrier-free region.  The second phase catches the
//   complementary shape: a store to a local `New`/`NewArray` allocation that
//   is *never read on any path* and *never escapes*, so every store into it
//   is dead regardless of control flow.
//
//   SOUNDNESS: a store to allocation `A` is write-only-dead iff
//     (1) `A` is a fresh local `New`/`NewArray` (same provenance guard), AND
//     (2) NO `Load` node anywhere in the graph reads from `A`
//         (`load_base(load) == A`), AND
//     (3) `A` does not escape — it never flows, *other than as the base of a
//         Store*, into any node that could publish or read it: a `Call` arg,
//         a `Return`, a `Store` *value* slot (storing the ref into another
//         object), an `ArrayLength`, or any base we cannot classify.  A
//         `Phi`/`Proj` use is treated as escaping (conservative — we do not
//         chase the ref through merges here, the increment-1 EA does), AND
//     (4) no safepoint snapshot names `A`: a deopt hands `A` to the
//         interpreter, which may read it in bytecode the graph never built
//         (an uncommon-trap-pruned branch).
//   When all three hold, *every* Store whose base is `A` is observably inert
//   (nothing ever loads what was written, and no external code sees `A`), so
//   each such Store is removed.  This is path-insensitive and needs no
//   ordering relation, because the predicate "no load of A exists" is a whole-
//   graph property.
//
//   Array ELEMENT stores are outside both phases. (This paragraph used to say
//   the production builder never emits `Op::Store`/`Op::NewArray`; that went
//   stale when `putfield` (0xb5) and `newarray` (0xbc) landed — r9.) The
//   builder's element store is `Op::ArrayStore`, which neither phase matches:
//   phase 1 treats it as a barrier (`is_memory_barrier`) and phase 2 as an
//   escaping use of its array (the catch-all arm), so a `NewArray` written
//   through `xastore` is never a write-only candidate. Only the EA-bridge /
//   hand-built compact `Op::Store` `[base, value]` on an array base reaches
//   the write-only phase, which keys on the base alone.
//
// This pass is monotone (only ever marks nodes `Dead`) and idempotent, so it
// composes safely inside the `optimize()` fixed-point loop.

/// A memory location written by a store, used to match an overwriting store.
/// Equality is structural over (base, index, kind, ctrl).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::ir_optimize) struct StoreLoc {
    /// The base reference node (a non-escaping allocation).
    pub(in crate::ir_optimize) base: NodeId,
    /// The index/offset operand node, or `NO_NODE` for a plain field store.
    pub(in crate::ir_optimize) idx: NodeId,
    /// The access width / kind.
    pub(in crate::ir_optimize) kind: MemKind,
    /// The store's control anchor (input 0 of the full layout), or `NO_NODE`
    /// for the compact `[base, value]` layout, which has none.
    ///
    /// Part of the key since round 9 wave 6 (`irsnap6`). Phase 1 walks nodes
    /// in ID order and relies on a control node sitting between two stores
    /// whenever they are in different blocks; nothing guarantees that. A
    /// `then` arm that ends in `continue` (a `goto` to a loop test whose merge
    /// already exists) creates no node, and the `else` arm's entry merge was
    /// made at the branch, BEFORE the `then` arm. So `if (c) { p.x = 1;
    /// continue; } p.x = 2;` put the two stores next to each other in id
    /// order, the second "overwrote" the first, and the `then` arm's store was
    /// deleted although the loop can exit right after it — observed in
    /// `CRATONVM_DBG_IR_GRAPH` on the w5b binary (`DseContinueProbe.run`: the
    /// `then` arm's memory φ input was the φ itself). `collapse_trivial_merges`
    /// (unroll, LICM) opens the same gap for any diamond by killing the arms'
    /// single-input merges. Requiring one anchor keeps the match inside one
    /// block, where id order is program order — the only case the walk was
    /// ever reasoning about.
    pub(in crate::ir_optimize) ctrl: NodeId,
}

/// Read `(base, idx, value)` from a `Store` node, tolerating both the compact
/// `[base, value]` layout used by the EA bridge / hand-built test graphs and
/// the full `[ctrl, mem, base, index/offset, value]` layout documented on
/// `Op::Store`.  Returns `None` when the layout is too short to interpret,
/// so the caller treats the store conservatively (never removed).
///
/// Disambiguation: the documented full form always carries a `Memory`-typed
/// token at input slot 1, whereas the compact form puts the base reference
/// there.  We key off the type of slot 1's producer.
pub(in crate::ir_optimize) fn store_operands(
    graph: &Graph,
    store_id: NodeId,
) -> Option<(NodeId, NodeId, NodeId)> {
    let n = &graph.nodes[store_id as usize];
    debug_assert!(matches!(n.op, Op::Store(_)));
    let inputs = &n.inputs;
    match inputs.len() {
        // Compact: [base, value]
        2 => Some((inputs[0], NO_NODE, inputs[1])),
        // Full store with no explicit index: [ctrl, mem, base, value]
        4 => Some((inputs[2], NO_NODE, inputs[3])),
        // Full store with index/offset: [ctrl, mem, base, index, value]
        n if n >= 5 => Some((inputs[2], inputs[3], inputs[4])),
        _ => None,
    }
}

/// Read the base reference a `Load` node addresses, tolerating both the
/// compact (`[base]` / `[base, index]`) and full (`[ctrl, mem, base, index?]`)
/// layouts — mirrors `store_operands`. Returns `None` for an uninterpretable
/// layout (caller treats it conservatively as a potential reader of anything).
pub(in crate::ir_optimize) fn load_base(graph: &Graph, load_id: NodeId) -> Option<NodeId> {
    let n = &graph.nodes[load_id as usize];
    debug_assert!(matches!(n.op, Op::Load(_)));
    let inputs = &n.inputs;
    match inputs.len() {
        // Compact: [base] or [base, index] — slot 0 is the base.
        1 | 2 => Some(inputs[0]),
        // Full with no index: [ctrl, mem, base].
        3 => Some(inputs[2]),
        // Full with index: [ctrl, mem, base, index].
        n if n >= 4 => Some(inputs[2]),
        _ => None,
    }
}

/// True if `id` is a fresh allocation node in this method (provably local
/// provenance — the only base we are willing to delete a store to).
pub(in crate::ir_optimize) fn is_local_alloc(graph: &Graph, id: NodeId) -> bool {
    matches!(
        graph.node_opt(id),
        Some(n) if matches!(n.op, Op::New { .. } | Op::NewArray { .. })
    )
}

/// True if `id` allocates an object with exactly **one** instance field.
///
/// This is the one shape in which a store carrying *no offset operand* still
/// names a definite cell: with a single field, "the object's storage" and
/// "field 0" are the same location, so two such stores to the same base must
/// alias. See [`store_matchable_location`].
///
/// `Op::NewArray` never qualifies — an element count is not a field count, and
/// an absent index names no element.
pub(in crate::ir_optimize) fn alloc_has_single_field(graph: &Graph, id: NodeId) -> bool {
    matches!(
        graph.node_opt(id),
        Some(n) if matches!(n.op, Op::New { num_fields: 1, .. })
    )
}

/// True when a store's `(base, idx)` pair names a cell precisely enough that a
/// later store to the *same key* is a proved overwrite — a **must**-alias, not
/// a may-alias.
///
/// Deleting a store needs the strong direction. The offset operand is what
/// carries the field identity, and two of the three `Op::Store` layouts
/// `store_operands` accepts do not have one:
///
/// * compact `[base, value]` (EA-bridge / hand-built), and
/// * full-without-offset `[ctrl, mem, base, value]`
///
/// both yield `idx == NO_NODE`. `ir::AccessOffset::Absent` — the memory model's
/// name for exactly this — is documented as *"the object's storage … never
/// disjoint from another access to the same base"*, i.e. a **may**-alias. Two
/// such stores into **different fields** of the same object nonetheless share
/// the location key `(base, NO_NODE, kind)` whenever their `MemKind` widths
/// agree, and `o.x = 1; o.y = 2;` would delete the live `o.x = 1`.
///
/// The production builder always emits the 5-input `[ctrl, mem, base, offset,
/// value]` form with a distinct `Const(field_index)` offset (`ir.rs`, the
/// `putfield` arm), so this refusal costs nothing today; it removes the
/// premise, which is the part that was not established.
pub(in crate::ir_optimize) fn store_matchable_location(
    graph: &Graph,
    base: NodeId,
    idx: NodeId,
) -> bool {
    idx != NO_NODE || alloc_has_single_field(graph, base)
}

/// True if a node could read memory or otherwise observe a pending store, so
/// encountering it forces DSE to discard all pending (not-yet-overwritten)
/// stores.  This is the conservative "alias barrier": anything that is not a
/// pure data computation is treated as a potential reader.
///
/// `Op::Store` is handled explicitly by the scan (it drives the overwrite
/// match), and `Op::Load` is intercepted by an earlier explicit arm in
/// `eliminate_dead_stores` that resolves it against the pending set with
/// allocation-level alias precision (a load of a distinct local allocation
/// observes no other local's stores). A load never reaches this function;
/// keeping `Load` classified as a barrier here is the SAFE fallback if that
/// explicit arm is ever removed. Every other node that touches memory, that
/// can leave the block, or that joins control (calls, returns, allocations,
/// guards, monitors, control joins, projections, memory phis, …) is an
/// unconditional barrier.
///
/// # Three questions, not one
///
/// This used to be `!op.is_pure() && !matches!(op, Op::Store(_))`, which is
/// the same conflation `licm_hoist_barrier` carried: a pure-but-unlisted op
/// would have been treated as a memory reader. It is spelled out here because
/// DSE needs all three reasons a pending store must be published, and only
/// one of them is about memory:
///
/// * **it may be READ** — [`memory_shape_of`] is anything but
///   [`MemoryShape::None`];
/// * **control may leave** — [`may_raise`]. This is why `Op::Div` and
///   `Op::Rem` stay barriers although they touch no memory: a zero divisor
///   raises, and the handler observes every store that has happened. Dropping
///   them from this predicate would delete live stores;
/// * **control may JOIN** — `is_control`. A merge means the pending set no
///   longer describes one path.
pub(in crate::ir_optimize) fn is_memory_barrier(op: &Op) -> bool {
    // The overwrite match owns this case; see above.
    if matches!(op, Op::Store(_)) {
        return false;
    }
    memory_shape_of(op).touches_memory() || may_raise(op) || op.is_control()
}

// ── Memory-token chain surgery ───────────────────────────────────────
//
// Every memory-effecting node carries an incoming *memory token* naming the
// previous writer, and produces itself as the token for the next one. That
// chain is the only ordering relation the IR has between two writes: the
// scheduler is otherwise free to place them in either order.
//
// `Graph::kill` alone therefore cannot remove a node that sits in the chain.
// It clears the killed node's own inputs but leaves every *consumer's* token
// slot naming it, so the next memory operation's token points at an `Op::Dead`
// node and has lost the transitive dependency on everything that wrote before
// the deleted store. That is the defect `ir_verify`'s ordering lane reports as
// "takes its memory token from removed node nN"; it is a wrong-code risk (a
// hoisted store), not a tidiness complaint. The fix is to *splice*: rewire the
// consumers to the killed node's own incoming token first, then kill.

/// The input slot carrying `node`'s incoming memory token, or `None` when this
/// op does not consume one.
///
/// This must agree with `ir_verify::is_memory_token_input`, which is the
/// predicate the verifier's ordering lane uses to decide whether a slot
/// pointing at a removed node is a broken chain: a splice that disagreed would
/// leave exactly the edge the verifier checks pointing at a dead node.
///
/// The op → slot mapping is the `[ctrl, mem, …]` layout documented on
/// `ir::Op`, so the token is always slot 1. The arity guard is what
/// distinguishes that documented full form from the compact hand-built /
/// EA-bridge layouts `store_operands` and `load_base` also accept (e.g. a
/// compact `Store` is `[base, value]`, whose slot 1 is a *value*, not a
/// token); each entry is the minimum input count of the full form.
/// `Op::ArrayLength` is `[ctrl, mem, array_ref]` per `ir::Op` and is
/// recognised here; `ir_verify::is_memory_token_input` classifies it too since
/// round 11 (it used to be the one op the two sides disagreed on).
pub(crate) fn memory_token_slot(node: &Node) -> Option<usize> {
    // Delegates to the single table (`ir::Op::memory_shape`).
    //
    // This used to be a hand-maintained copy of the arity list, and a third
    // copy lived in `lib.rs`. They were only ever *numerically* identical, and
    // the moment `Op::MonitorEnter`/`MonitorExit` joined the table the copies
    // began answering `None` for a real token slot — which would have let dead-
    // store elimination and the EA applier treat a monitor's memory edge as a
    // value and splice the chain wrongly.
    crate::ir::memory_token_slot(node)
}

/// True when input `idx` of `node` is a memory *token* — an ordering edge
/// naming the previous writer — rather than a value the node reads.
///
/// A memory-typed φ is the one op whose token edges are not a single slot:
/// *every* value input (slots 1.., past the control anchor) is a token from
/// one predecessor.
pub(crate) fn is_memory_token_slot(node: &Node, idx: usize) -> bool {
    crate::ir::is_memory_token_slot(node, idx)
}

/// Kill `store`, first splicing it out of the memory-token chain: every
/// consumer that named it as a token is rewired to the store's *own* incoming
/// token, so the chain closes over the hole instead of pointing into it.
///
/// Returns `false` — leaving the store **alive** — when the splice cannot be
/// done safely:
///
/// * the store is in a compact (`[base, value]`) layout, so it has no incoming
///   token to hand on, yet something names it;
/// * its incoming token is absent, out of range, or itself already dead, so
///   splicing would just relocate the break;
/// * something names it in a slot that is *not* a memory token (a value input,
///   a safepoint slot). A `Store` produces no value, so such a reference is
///   already malformed — rewiring it to a memory token would replace one bug
///   with a quieter one.
///
/// A store we fail to delete is a missed optimization. A store deleted out of a
/// chain we could not repair is wrong code, so the bias is deliberate.
pub(in crate::ir_optimize) fn kill_store_splicing_memory_chain(
    graph: &mut Graph,
    store: NodeId,
) -> bool {
    // A VOLATILE store is never removed by this file's store eliminators
    // (round 9 wave 6): it is a full fence, and the stores it publishes are
    // ordered by it whether or not anyone reads its own cell. Refused here, at
    // the one place both dead-store phases kill through, so a caller that
    // forgets to ask cannot remove one.
    let (is_store, token) = match graph.node_opt(store) {
        Some(n) => (
            matches!(n.op, Op::Store(_)) && !n.is_volatile_access(),
            memory_token_slot(n).and_then(|slot| n.input_opt(slot)),
        ),
        None => return false,
    };
    if !is_store {
        return false;
    }

    // Who still names this store, and do they all name it as a token?
    //
    // Through the use lists (`users_of` / `safepoint_users_of`, which fall back
    // to the same full scan when the lists are stale) rather than a walk of the
    // whole arena and every snapshot per killed store — both DSE phases kill a
    // batch, and the walk made each batch O(stores × (nodes + snapshot slots)).
    let mut has_consumer = false;
    let mut spliceable = true;
    for u in graph.users_of(store) {
        let Some(n) = graph.node_opt(u) else {
            continue;
        };
        if n.op == Op::Dead {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            if inp != store {
                continue;
            }
            has_consumer = true;
            if !is_memory_token_slot(n, i) {
                spliceable = false;
            }
        }
    }
    if !graph.safepoint_users_of(store).is_empty() {
        // A frame state naming a store is already nonsense — a store produces
        // no value an interpreter frame can be rebuilt from — but it is not
        // this pass's business to silently retarget it.
        has_consumer = true;
        spliceable = false;
    }

    // Nothing names it: the chain does not run through this node, so the plain
    // kill is already correct (this is the common hand-built / compact case).
    if !has_consumer {
        graph.kill(store);
        return true;
    }
    if !spliceable {
        return false;
    }
    let token = match token {
        Some(t) => t,
        None => return false,
    };
    // Splicing onto an already-dead token would just move the break along the
    // chain. (Cannot happen when this pass kills a run of chained stores in
    // ascending id order — each splice rewrites the next one's token slot to a
    // live node before we get there — but the check is cheap and the invariant
    // is not local.)
    if !matches!(graph.node_opt(token), Some(n) if n.op != Op::Dead) {
        return false;
    }

    graph.replace_all_uses(store, token);
    graph.kill(store);
    true
}

/// Remove a store of zero (`null`, `0`) into a field of an object this graph
/// allocated, when nothing has written that field since the allocation
/// (`CRATONVM_JIT_IR_INITIAL_ZERO_STORES`, default on).
///
/// A fresh object's fields ARE zero -- the TLAB is cleared and the allocator
/// writes only the header -- so the store changes nothing. It is the store
/// every `new Node(null, null)` makes twice once its constructor is spliced in,
/// and under G1 each one was a full inline reference store: layout guard,
/// compactness test, old-value test and the store.
///
/// The proof walks the store's memory chain back to the allocation's own
/// memory input. Only two kinds of node may sit on it: a store to ANOTHER
/// constant field of the same allocation, and a pure read. Anything else -- a
/// call, a merge, a store through any other base, a volatile access -- stops
/// the walk and the store stays. The same field written earlier stops it too,
/// since the store then does change the cell.
pub(in crate::ir_optimize) fn eliminate_initial_zero_stores(graph: &mut Graph) -> bool {
    let const_of = |graph: &Graph, id: NodeId| match graph.node_opt(id).map(|n| &n.op) {
        Some(Op::Const(c)) => Some(*c),
        _ => None,
    };
    let mut doomed: Vec<NodeId> = Vec::new();
    for (idx, n) in graph.nodes.iter().enumerate() {
        let Op::Store(kind) = n.op else { continue };
        if n.is_volatile_access() || n.inputs.len() < 5 {
            continue;
        }
        if !matches!(
            kind,
            crate::ir::MemKind::Ref | crate::ir::MemKind::Int | crate::ir::MemKind::Long
        ) {
            continue;
        }
        let (token, base, field, value) = (n.inputs[1], n.inputs[2], n.inputs[3], n.inputs[4]);
        if const_of(graph, value) != Some(0) {
            continue;
        }
        let Some(field_k) = const_of(graph, field) else {
            continue;
        };
        let Some(alloc) = graph.node_opt(base) else {
            continue;
        };
        let Op::New {
            class_id,
            num_fields,
        } = alloc.op
        else {
            continue;
        };
        // COMPACT instances only. A legacy 16-byte cell of zeroes reads back
        // as `Int(0)`, not `null`, so there an explicit null store is what
        // makes the field a reference at all.
        //
        // Round 11 wave 16 (lane irback): asked through the allocators' own
        // predicate, `compact_tlab_total_size`, not a domain-blind
        // `class_layout` lookup. It folds in `compact_ref_fields_enabled()`,
        // the field-count match AND `single_layout_domain()`, which is when
        // both JIT inline bumps and the interpreter's TLAB path allocate the
        // compact shape (r11w15-hdr-ir-initial-zero-stores-domain-blind-layout).
        // In a multi-`ClassStore` process it answers `None` and the store
        // stays: a kept store is correct on either shape.
        if cratonvm_types::compact_tlab_total_size(class_id, num_fields).is_none() {
            continue;
        }
        let Some(&alloc_mem) = alloc.inputs.get(1) else {
            continue;
        };
        let mut cur = token;
        let mut proven = false;
        for _ in 0..LOAD_CSE_CHAIN_BUDGET {
            if cur == alloc_mem {
                proven = true;
                break;
            }
            let Some(m) = graph.node_opt(cur) else { break };
            if m.is_volatile_access() {
                break;
            }
            let passable = match m.op {
                Op::Store(_) => {
                    m.inputs.len() >= 5
                        && m.inputs[2] == base
                        && const_of(graph, m.inputs[3]).is_some_and(|k| k != field_k)
                }
                ref op => is_pure_memory_read(op),
            };
            if !passable {
                break;
            }
            let Some(next) = crate::ir::memory_token_slot(m).and_then(|slot| m.input_opt(slot))
            else {
                break;
            };
            cur = next;
        }
        if proven {
            // Cast: an index into the node arena is a `NodeId`.
            doomed.push(idx as NodeId);
        }
    }
    let mut changed = false;
    for store in doomed {
        if kill_store_splicing_memory_chain(graph, store) {
            INITIAL_ZERO_STORES_REMOVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            changed = true;
        }
    }
    changed
}

/// Stores [`eliminate_initial_zero_stores`] removed, process-wide.
pub static INITIAL_ZERO_STORES_REMOVED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Remove a non-volatile `putstatic` whose cell the NEXT memory operation
/// overwrites (`CRATONVM_JIT_IR_OVERWRITTEN_STATIC_STORE`, default on).
///
/// `sink = f; sink = g;` builds `S_g = StoreStatic[ctrl, S_f, g]`: the second
/// store takes the first one's token directly, under the same control anchor,
/// so nothing runs between them that could read the cell. No other thread is
/// owed the first value either: the field is not volatile, and JLS §17.4
/// lets a race read either write, or the one before them both. This is the
/// store C2 drops before escape analysis, and it is what made
/// `R21AllocCompletableFuture cf-new-three` keep three allocations where
/// HotSpot keeps one: a reference `putstatic` publishes its value (`EaOp::Other`
/// in the EA bridge), so the overwritten futures escaped through a store no
/// code could see.
///
/// What the removed helper call did besides the write, and why each is safe
/// to lose:
/// * `<clinit>`: refused unless the field is in `direct_static_fields`, whose
///   class was initialised at compile time. (A class still being initialised
///   could run the store's `<clinit>` check differently at the two pcs.)
/// * the SATB pre-barrier on the old value: the surviving store logs the cell
///   as it was before both, which is the value a snapshot taken before them
///   holds; the removed store's value is a reference written after the
///   snapshot, which the barrier never owed a log.
/// * its safepoint: a safepoint is never owed to compiled code.
///
/// The first store must have no other user (a loop's memory φ, an exception
/// edge, a frame state naming it), or its value could be read on a path that
/// does not run the second.
pub(in crate::ir_optimize) fn eliminate_overwritten_static_stores(graph: &mut Graph) -> bool {
    if !overwritten_static_store_enabled() {
        return false;
    }
    let mut doomed: Vec<NodeId> = Vec::new();
    for (idx, n) in graph.nodes.iter().enumerate() {
        let Op::StoreStatic {
            class_id,
            field_index,
            is_volatile: false,
            ..
        } = n.op
        else {
            continue;
        };
        if n.inputs.len() < 3 {
            continue;
        }
        let first = n.inputs[1];
        let Some(f) = graph.node_opt(first) else {
            continue;
        };
        let Op::StoreStatic {
            class_id: c1,
            field_index: f1,
            is_volatile: false,
            ..
        } = f.op
        else {
            continue;
        };
        if c1 != class_id
            || f1 != field_index
            || first as usize >= idx
            || f.inputs.len() < 3
            || f.inputs[0] != n.inputs[0]
            || !graph
                .direct_static_fields
                .contains(&(class_id, field_index))
            || doomed.contains(&first)
        {
            continue;
        }
        // Cast: an index into the node arena is a `NodeId`.
        let second = idx as NodeId;
        let sole_user = graph
            .users_of(first)
            .into_iter()
            .all(|u| u == second || graph.node_opt(u).is_none_or(|un| un.op == Op::Dead))
            && graph.nodes[second as usize]
                .inputs
                .iter()
                .enumerate()
                .all(|(i, &inp)| inp != first || i == 1);
        // Nothing the builder made between the two bytecodes may leave the
        // block or read memory. A guard or an implicitly null-checked
        // `putfield` there names neither store, yet a deopt or a throw at it
        // hands the interpreter (or a handler) the cell without the first
        // value. Ids in that range are the in-between bytecodes' nodes, the
        // order `eliminate_dead_stores`' phase 1 rests on; an instance `Store`
        // counts as well, since its base may be null.
        let quiet_between = ((first + 1)..second).all(|id| {
            graph.node_opt(id).is_none_or(|m| {
                m.op == Op::Dead || !(is_memory_barrier(&m.op) || matches!(m.op, Op::Store(_)))
            })
        });
        if sole_user && quiet_between && graph.safepoint_users_of(first).is_empty() {
            doomed.push(first);
        }
    }
    let mut changed = false;
    for store in doomed {
        let Some(token) = graph.node_opt(store).and_then(|n| n.input_opt(1)) else {
            continue;
        };
        if !matches!(graph.node_opt(token), Some(t) if t.op != Op::Dead) {
            continue;
        }
        graph.replace_all_uses(store, token);
        graph.kill(store);
        OVERWRITTEN_STATIC_STORES_REMOVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        changed = true;
    }
    changed
}

/// `CRATONVM_JIT_IR_OVERWRITTEN_STATIC_STORE` (default on, `=0` off):
/// [`eliminate_overwritten_static_stores`]. Also one of the switches that make
/// `lib.rs` feed `Graph::direct_static_fields`, the list the pass asks.
pub(crate) fn overwritten_static_store_enabled() -> bool {
    true
}

/// Stores [`eliminate_overwritten_static_stores`] removed, process-wide.
pub static OVERWRITTEN_STATIC_STORES_REMOVED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Dead-store elimination: remove stores whose effect is never observed.
///
/// See the module-level soundness model above.  Walks nodes in id order,
/// tracking the most recent still-live store to each location written to a
/// local allocation; an overwriting store to the same location kills the
/// earlier one, and any memory barrier flushes the pending set.
///
/// Alias refinement (DSE-WIDEN): an intervening `Load` is not an unconditional
/// barrier. Because every pending store targets a local allocation and two
/// distinct local allocations never alias, a load of a *different* local
/// allocation flushes only its own base; only a load of the same base, or of a
/// non-local/unreadable base, flushes (the latter, everything). This lets a
/// store survive an intervening read of an unrelated local object.
pub(in crate::ir_optimize) fn eliminate_dead_stores(graph: &mut Graph) {
    // The location each currently-pending store writes, in node order. A
    // pending store is one we have seen but not yet proven observed; a later
    // store to the same location proves it dead.
    let mut pending: Vec<(NodeId, StoreLoc)> = Vec::new();
    let mut to_kill: Vec<NodeId> = Vec::new();

    let len = graph.nodes.len();
    for id in 0..len {
        let op = graph.nodes[id].op.clone();
        match op {
            Op::Dead => continue,
            // A VOLATILE access (round 9 wave 6) is a barrier for this phase,
            // whatever its base: a volatile store publishes every earlier
            // store (release), so none of them is dead however it is
            // overwritten later, and the store itself is never a candidate
            // (`kill_store_splicing_memory_chain` refuses it too). A volatile
            // read is flushed as well — conservative, since a plain store may
            // sink below an acquire — so no overwrite is matched across one.
            Op::Store(_) | Op::Load(_) if graph.nodes[id].is_volatile_access() => {
                pending.clear();
            }
            Op::Store(kind) => {
                let store_id = id as NodeId;
                let (base, idx, _val) = match store_operands(graph, store_id) {
                    Some(t) => t,
                    None => {
                        // Unreadable layout — treat as a barrier so we never
                        // delete around a store we cannot understand.
                        pending.clear();
                        continue;
                    }
                };

                // Only stores to a fresh local allocation are eligible: any
                // other base may be observed outside this method.
                if !is_local_alloc(graph, base) {
                    // Non-local store: it both may be observed AND may alias
                    // pending locals (we cannot prove otherwise), so flush.
                    pending.clear();
                    continue;
                }

                // MUST-ALIAS, NOT MAY-ALIAS. A layout with no offset operand
                // does not name a field, so two such stores to the same base
                // may be two DIFFERENT cells (see `store_matchable_location`).
                // Such a store neither matches a pending store nor becomes one
                // — but it is NOT a barrier either: a store observes nothing,
                // so it cannot make an earlier pending store live.
                if !store_matchable_location(graph, base, idx) {
                    continue;
                }

                // The control anchor, for the full layout only (see
                // `StoreLoc::ctrl`). `store_operands` accepted the layout, so
                // an arity of 4+ is the full form and slot 0 is its control.
                let ctrl = if graph.nodes[id].inputs.len() >= 4 {
                    graph.nodes[id].inputs[0]
                } else {
                    NO_NODE
                };
                let loc = StoreLoc {
                    base,
                    idx,
                    kind,
                    ctrl,
                };

                // If an earlier pending store wrote the exact same location,
                // it is overwritten here with no intervening reader → dead.
                if let Some(pos) = pending.iter().position(|(_, l)| *l == loc) {
                    let (dead_store, _) = pending.remove(pos);
                    to_kill.push(dead_store);
                }
                // This store is now the live writer of `loc`.
                pending.push((store_id, loc));
            }
            Op::Load(_) => {
                // A load is only a barrier for stores it could OBSERVE. Every
                // pending store writes a local `New`/`NewArray` allocation, and
                // two distinct local allocations never alias (the same fact the
                // location key relies on). So:
                //   * a load whose base is a DIFFERENT local allocation cannot
                //     read any other local's fields — it only flushes pending
                //     stores to its own base (which it genuinely could observe);
                //   * a load from a non-local / unreadable base may alias any
                //     escaped local, so it conservatively flushes everything.
                // This lets an overwritten store stay dead-eligible across an
                // intervening read of an unrelated local object (DSE-WIDEN),
                // while remaining sound: the only way to read allocation A's
                // field is a load whose base resolves to A (flushes A) or to a
                // non-local node such as a load/phi result (flushes all).
                match load_base(graph, id as NodeId) {
                    Some(b) if is_local_alloc(graph, b) => {
                        pending.retain(|(_, loc)| loc.base != b);
                    }
                    _ => pending.clear(),
                }
            }
            other => {
                // Any non-pure, non-store node may observe memory: give up on
                // every pending store (we cannot prove they are unread past
                // this point).
                if is_memory_barrier(&other) {
                    pending.clear();
                }
            }
        }
    }

    // Splice each dead store out of the memory-token chain before killing it —
    // a plain `Graph::kill` here would leave the *next* memory operation's
    // token slot naming an `Op::Dead` node, which is how the surviving store
    // loses its transitive ordering edges. Ascending id order so that when two
    // stores in this batch are adjacent in the chain, the earlier one is
    // spliced first and the later one's token slot already names a live node
    // by the time we reach it. `sort`/`dedup`: the pending set is popped in
    // location order, not id order, so the raw vector is neither.
    to_kill.sort_unstable();
    to_kill.dedup();
    for id in to_kill {
        kill_store_splicing_memory_chain(graph, id);
    }

    // ── Phase 2: write-only, never-read local allocations ──────────────
    // Path-insensitive: a store to a local alloc that is never loaded and
    // never escapes is dead regardless of control flow. See the module-level
    // "WIDENED CASE" note for the soundness argument.
    eliminate_write_only_stores(graph);
}

/// Remove every `Store` into a local `New`/`NewArray` allocation that is never
/// read (no `Load` addresses it) and never escapes the method. Path-
/// insensitive whole-graph analysis; see the DSE module note.
pub(in crate::ir_optimize) fn eliminate_write_only_stores(graph: &mut Graph) {
    use std::collections::hash_map::Entry;

    // 1. Candidate allocations: every fresh local alloc node.
    let mut candidate: FxHashMap<NodeId, bool> = FxHashMap::default();
    for (id, node) in graph.nodes.iter().enumerate() {
        // JIT round 20 wave 12 (lane hFin): never an object the compile
        // resolved with a finalizer or reference registration
        // (`Graph::finalizable_new_classes`). Its fields have a reader this
        // graph cannot see -- `finalize()`, or the collector's reference
        // processing (`referent`, `queue`) -- so "never loaded here" is not
        // "never read".
        if let Op::New { class_id, .. } = node.op {
            if graph.finalizable_new_classes.contains(&class_id) {
                continue;
            }
        }
        if matches!(node.op, Op::New { .. } | Op::NewArray { .. }) {
            candidate.insert(id as NodeId, true); // true = still write-only
        }
    }
    if candidate.is_empty() {
        return;
    }

    // 2. Disqualify any allocation that is read or escapes. We scan every use
    //    of every candidate. A use disqualifies the candidate unless it is the
    //    *base* slot of a Store (the only inert use of a write-only alloc).
    for (id, node) in graph.nodes.iter().enumerate() {
        let id = id as NodeId;
        match &node.op {
            Op::Dead => continue,
            Op::Load(_) => {
                // Reading any candidate disqualifies it.
                if let Some(base) = load_base(graph, id) {
                    if let Entry::Occupied(mut e) = candidate.entry(base) {
                        *e.get_mut() = false;
                    }
                } else {
                    // Unreadable load layout: conservatively disqualify every
                    // candidate it could touch (all of them).
                    for v in candidate.values_mut() {
                        *v = false;
                    }
                }
            }
            Op::Store(_) => {
                // The base slot is the inert use; the *value* slot escaping the
                // ref (storing a candidate ref into another object) disqualifies
                // it. `idx` operand referencing a candidate would be nonsensical
                // for an alloc ref but is treated as escaping if it occurs.
                if let Some((base, idx, val)) = store_operands(graph, id) {
                    // value or index referencing a candidate ⇒ escapes.
                    for &slot in &[val, idx] {
                        if slot != NO_NODE {
                            if let Entry::Occupied(mut e) = candidate.entry(slot) {
                                *e.get_mut() = false;
                            }
                        }
                    }
                    // base is the inert use — does not disqualify.
                    let _ = base;
                } else {
                    // Unreadable store layout: disqualify everything it touches.
                    for &inp in &node.inputs {
                        if let Entry::Occupied(mut e) = candidate.entry(inp) {
                            *e.get_mut() = false;
                        }
                    }
                }
            }
            // Any OTHER node (Call arg, Return value, ArrayLength, Phi, Proj,
            // a second New's input, Guard, …) that references a candidate
            // publishes or observes the ref ⇒ escapes / read ⇒ disqualify.
            _ => {
                for &inp in &node.inputs {
                    if let Entry::Occupied(mut e) = candidate.entry(inp) {
                        *e.get_mut() = false;
                    }
                }
            }
        }
    }

    // 2b. A safepoint snapshot naming a candidate is a READ this scan cannot
    //     see (r9). Deopt rebuilds an interpreter frame holding the REAL
    //     object — `Op::New` is a DCE root and is never removed here — and the
    //     interpreter then runs bytecode the graph does not contain: a branch
    //     `plant_uncommon_trap` pruned, or the rest of the method after any
    //     guard. `p = new P(); p.x = 5; if (rare) return p.x;` with `rare`
    //     pruned has no `Load` of `p` anywhere in the graph, so steps 1-2 found
    //     `p` write-only and deleted `p.x = 5`; the pruned path then deopted and
    //     the interpreter read `0`. Same rule `eliminate_dead_nodes` applies:
    //     a snapshot slot is a use.
    //
    //     ...of a snapshot a deopt can CONSULT, under
    //     `CRATONVM_JIT_IR_SNAPSHOT_LIVENESS=1` (r9 wave 2): see
    //     [`consultable_snapshots`]. The pruned-trap case above is untouched by
    //     that — the trap is an `Op::Guard` at its bci, so its snapshot is
    //     consultable and still disqualifies `p`. What stops disqualifying is a
    //     snapshot at a bci nothing can resume at, which is almost all of them.
    let snapshot_mask = live_snapshot_mask(graph);
    for (si, sp) in graph.safepoints.iter().enumerate() {
        if snapshot_mask
            .as_ref()
            .is_some_and(|m| !m.get(si).copied().unwrap_or(true))
        {
            continue;
        }
        for &slot in sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
        {
            if let Some(v) = candidate.get_mut(&slot) {
                *v = false;
            }
        }
    }

    // 3. Kill every Store whose base is a surviving write-only candidate AND
    //    whose own node is not consumed as an input elsewhere. A Store
    //    referenced by another node (a memory-token chain, a Return, …) is
    //    being kept alive as an observed effect; we only remove the store when
    //    nothing depends on the store node itself, so this never severs a
    //    memory/effect edge a consumer relies on.
    //
    //    Under snapshot liveness (round 9 wave 4, irlower4) a store whose ONLY
    //    consumers name it as a memory TOKEN is admitted too, and spliced out
    //    of the chain exactly as phase 1 splices an overwritten store
    //    (`kill_store_splicing_memory_chain`, which keeps the store alive when
    //    any consumer names it in a value slot). In a built graph every store
    //    but the last one in a chain feeds the next memory op's token, so the
    //    `uses == 0` rule alone left this phase able to remove at most one
    //    store per chain. Removing a store no one can observe and closing the
    //    chain over it preserves the order of everything that remains. Gated
    //    with the liveness mask, which is the only configuration in which an
    //    allocation that is stored to routinely survives step 2b.
    let chained_ok = SNAPSHOT_LIVENESS_ON.with(|c| c.get());
    let uses = graph.use_counts();
    let mut to_kill: Vec<NodeId> = Vec::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        if let Op::Store(_) = node.op {
            if uses[id] != 0 && !chained_ok {
                continue; // store node is depended upon — keep it
            }
            // Never a volatile store (round 9 wave 6); the kill helper refuses
            // it as well, this only keeps it out of the census.
            if node.is_volatile_access() {
                continue;
            }
            if let Some((base, _, _)) = store_operands(graph, id as NodeId) {
                if candidate.get(&base).copied() == Some(true) {
                    to_kill.push(id as NodeId);
                }
            }
        }
    }
    // Unarmed, `uses[id] == 0` above already means nothing names these stores,
    // so the splice is a no-op kill — except that `use_counts` does not see
    // safepoint slots, and the splice helper does. Routing through it keeps the
    // one reference class this phase cannot count from being severed silently.
    // Armed, it is the real splice. Ascending id order (`to_kill` is built by
    // `enumerate`), which is the order the helper's dead-token check assumes
    // for a run of chained stores.
    let mut removed = 0u64;
    for id in to_kill {
        if kill_store_splicing_memory_chain(graph, id) {
            removed += 1;
        }
    }
    if removed > 0 {
        IR_WRITE_ONLY_STORES_REMOVED.fetch_add(removed, std::sync::atomic::Ordering::Relaxed);
    }
}

// ── Dead Node Elimination ────────────────────────────────────────────

/// Remove nodes not reachable from the exit (Return), from any other
/// side-effecting root, or from a safepoint snapshot.
/// Is an `op` with no consumer still observable, so dead-node elimination must
/// keep it?
///
/// Exhaustive on purpose -- see the call site in [`eliminate_dead_nodes`].
/// `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: a read of a trusted `static
/// final` the VM filed non-null (`Graph::static_final_non_null`): its class
/// is initialized and nothing writes it, so an unread one runs no `<clinit>`,
/// throws nothing and may go.
pub(in crate::ir_optimize) fn trusted_static_final_read(graph: &Graph, op: &Op) -> bool {
    matches!(*op, Op::LoadStatic { class_id, field_index, type_tag: b'L' | b'[', is_volatile: false }
        if graph.static_final_non_null.contains(&(class_id, field_index)))
}

pub(in crate::ir_optimize) fn is_dce_root(op: &Op) -> bool {
    match op {
        // Program exits: every `Return`, not just `graph.exit` (see the call
        // site), and a throw, whose exception input must stay reachable.
        Op::Return | Op::Throw => true,
        // Memory effects and calls: their token or result may have no reader.
        Op::Store(_) | Op::Call { .. } | Op::LambdaIntToDouble => true,
        // Round 16 (lane indy): a bridged `invokedynamic` runs a bootstrap or an
        // operand `toString()`, and its token may have no reader.
        Op::IndyBridge { .. } => true,
        // Round 16 (lane matrix): `multianewarray` throws on a negative
        // dimension and resolves classes, so an unread one still runs.
        Op::MultiNewArray { .. } => true,
        // nio21 w15 L3: a deferred `new` resolves and initialises its class
        // (`<clinit>`, a thrown error), so an unread one still runs.
        Op::NewCp { .. } => true,
        // cov-01: `ldc <Class>` and `getstatic` can run `<clinit>` and throw;
        // `ldc <String>` interns, which `==` on a later literal observes.
        Op::ConstString { .. } | Op::ConstClass { .. } | Op::LoadStatic { .. } => true,
        // `putstatic`: a write every other thread can see, whose token often
        // has no reader (a trailing `COUNT++`), and which can run `<clinit>`.
        Op::StoreStatic { .. } => true,
        // Reads that throw: NullPointerException, an out-of-bounds index,
        // ClassCastException, or a null unboxing receiver. An unused
        // `a.length` or `o.f` still owes its exception.
        Op::Load(_)
        | Op::ArrayLoad(_)
        | Op::ArrayStore(_)
        | Op::ByteArrayCopy
        | Op::ArrayLength
        | Op::CheckCast { .. }
        | Op::Unbox { .. } => true,
        // Allocation can run `<clinit>` (`new`), throw
        // NegativeArraySizeException (`newarray`) or OutOfMemoryError. Scalar
        // replacement retires the allocations it removes by marking them
        // `Op::Dead` itself; it does not rely on this pass.
        Op::New { .. } | Op::NewArray { .. } => true,
        // Monitors: deleting one of a pair is an IllegalMonitorStateException,
        // and deleting both is unplanned lock elision.
        Op::MonitorEnter | Op::MonitorExit => true,
        // A guard exists for the deopt it takes; it produces no value.
        Op::Guard { .. } => true,
        // nio20 L2: a keep-alive exists for the live range it extends; it
        // has no user by construction.
        Op::KeepAlive => true,
        // Control structure is kept by reachability from the roots above.
        Op::Start
        | Op::If
        // A switch is kept by reachability, exactly like `Op::If`: it
        // produces no value, and the control it names is what makes its
        // targets live. This arm is where the exhaustive match sent the
        // new variant, and it is the right one on the merits.
        | Op::Switch { .. }
        | Op::Merge
        | Op::Region
        | Op::Proj(_)
        | Op::Phi => false,
        // Pure values. `Div`/`Rem` owe ArithmeticException through the guard
        // the builder anchors beside them, not through the node itself.
        Op::Const(_)
        | Op::ConstF(_)
        | Op::Param(_)
        | Op::InstanceOf { .. }
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Neg
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::Shr
        | Op::UShr
        | Op::Cmp(_)
        | Op::LCmp
        | Op::FCmp { .. }
        | Op::I2L
        | Op::L2I
        | Op::I2F
        | Op::I2D
        | Op::L2F
        | Op::L2D
        | Op::F2I
        | Op::F2L
        | Op::F2D
        | Op::D2I
        | Op::D2L
        | Op::D2F
        | Op::I2B
        | Op::I2C
        | Op::I2S
        | Op::ScalarIntrinsic(_)
        // A total header test; the `Op::Guard` consuming it is the root.
        | Op::ExactClassIs { .. }
        // JIT round 19 (lane osr19): the chain head's header read; the `If`s
        // consuming its compares are the roots.
        | Op::ClassWordOf
        // Round 11 wave 14 (lane osrmerge): the opaque OSR merge. The flag is
        // kept by its `If` (reachable from `Start`), the local and the memory
        // state by the pre-header φs that read them; a φ nothing reads takes
        // its `OsrLocal` with it, which is exactly an arm with less to load.
        | Op::OsrEntryFlag { .. }
        | Op::OsrLocal(_)
        | Op::OsrMemory
        // nio21 wave 3 L2: a call-site exception edge. The flag is kept by
        // its `If`, the caught throwable by the handler code that reads it.
        | Op::CallThrew { .. }
        // JIT round 19 wave 3 (lane eager3): kept by the `Op::Guard` reading it.
        | Op::ClassInitTest { .. }
        | Op::CurrentThreadMirror { .. }
        | Op::VthreadBoundaryTest { .. }
        // JIT round 20 wave 11 (lane pIndy): kept by the compare reading it.
        | Op::IndyRootRef { .. }
        // JIT round 21 (lane charat, STRINTR21-5): an unchecked read that owes
        // no exception (its guards are the roots) and writes nothing, so an
        // unread one goes.
        | Op::StrCharDecode
        // JIT round 19 wave 5 (lane handler5, SPLICE4-1): the compiled handler
        // entry. The flag is kept by its `If`, the caught throwable by the
        // handler merge's operand-stack input that reads it (an unread one
        // goes, and the lowerer then clears the parked word itself).
        | Op::CatchEntryFlag { .. }
        | Op::CaughtException
        | Op::Dead => false,
    }
}

/// Is `node` a piece of control STRUCTURE that the forward walk from `Start`
/// in [`eliminate_dead_nodes`] should continue through?
///
/// Control-typed `If`/`Switch`/`Merge`/`Region`/`Proj` only. A memory
/// projection (`Proj(1)` of `Start`) is typed `Memory` and is not control; a
/// guard or call takes control but produces none the builder threads on.
pub(in crate::ir_optimize) fn is_forward_control(node: &Node) -> bool {
    node.ty == IrType::Control
        && matches!(
            node.op,
            Op::If | Op::Switch { .. } | Op::Merge | Op::Region | Op::Proj(_)
        )
}

/// Hoist `Guard(ClassInitTest)` out of single-entry loops that contain no
/// calls, allocations, stores, synchronization, or volatile accesses (Section 3
/// soundness: nothing in the loop can thaw a frozen page or introduce a
/// happens-before edge, so checking once at loop entry is equivalent).
pub(in crate::ir_optimize) fn hoist_pure_loop_class_init_guards(graph: &mut Graph) -> usize {
    const MAX_GUARDS_PER_LOOP: usize = 8;
    if graph.safepoints.is_empty() || graph.osr_compile {
        return 0;
    }
    let mut users = build_users(graph);
    let mut loops: Vec<(usize, (NodeId, NodeId, Vec<NodeId>))> = loop_headers(graph)
        .into_iter()
        .map(|l| (loop_body_with(graph, &users, l.0, l.1, &l.2).len(), l))
        .collect();
    loops.sort_by(|a, b| b.0.cmp(&a.0).then(a.1 .0.cmp(&b.1 .0)));

    let mut total_planted = 0usize;
    for (_, (region, entry_pred, back_ctrls)) in loops {
        if entry_pred == NO_NODE {
            continue;
        }
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let pure_body = body.iter().all(|&id| {
            let Some(node) = graph.node_opt(id) else {
                return true;
            };
            if node.is_volatile_access() || matches!(node.op, Op::Store(_) | Op::Throw) {
                return false;
            }
            matches!(
                node.op,
                Op::Load(_)
                    | Op::Phi
                    | Op::Dead
                    | Op::OsrMemory
                    | Op::Guard { .. }
                    | Op::ClassInitTest { .. }
            ) || node.op.is_control()
                || !memory_shape_of(&node.op).touches_memory()
        });
        if !pure_body {
            continue;
        }

        let mut guards_by_key: Vec<((usize, u8), Vec<NodeId>)> = Vec::new();
        for &id in &body {
            let Some(node) = graph.node_opt(id) else {
                continue;
            };
            let Op::Guard { .. } = node.op else {
                continue;
            };
            let guard_ctrl = node.inputs.first().copied().unwrap_or(NO_NODE);
            if guard_ctrl != region {
                continue;
            }
            let Some(&cond_id) = node.inputs.get(1) else {
                continue;
            };
            let Some(cond_node) = graph.node_opt(cond_id) else {
                continue;
            };
            let Op::ClassInitTest {
                state_addr,
                initialized,
            } = cond_node.op
            else {
                continue;
            };
            if let Some((_, list)) = guards_by_key
                .iter_mut()
                .find(|(k, _)| *k == (state_addr, initialized))
            {
                list.push(id);
            } else if guards_by_key.len() < MAX_GUARDS_PER_LOOP {
                guards_by_key.push(((state_addr, initialized), vec![id]));
            }
        }
        if guards_by_key.is_empty() {
            continue;
        }
        let Some(entry_slot) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
            .map(|p| p + 1)
        else {
            continue;
        };
        let phi_entry_slot = entry_slot;
        let Some((header_bci, frame)) =
            loop_entry_chain_frame(graph, region, phi_entry_slot, &body)
                .or_else(|| loop_entry_frame(graph, region, phi_entry_slot, &body))
        else {
            continue;
        };

        graph.push_safepoint(frame);
        let si = (graph.safepoints.len() - 1) as u32;
        let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
        let mut planted = 0usize;
        for ((state_addr, initialized), guard_ids) in guards_by_key {
            let test = graph.add(
                Op::ClassInitTest {
                    state_addr,
                    initialized,
                },
                IrType::Int,
                vec![entry_pred],
                None,
            );
            let pre_guard = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, test],
                Some(header_bci),
            );
            if !graph.set_node_frame_snapshot(pre_guard, si) {
                graph.kill(pre_guard);
                graph.kill(test);
                break;
            }
            planted += 1;
            for gid in guard_ids {
                let _ = graph.set_input(gid, 1, one);
            }
        }
        if planted == 0 {
            graph.kill(one);
            let keep: Vec<bool> = (0..graph.safepoints.len())
                .map(|i| i != si as usize)
                .collect();
            let _ = graph.retain_safepoints(&keep);
        }
        total_planted += planted;
        if planted > 0 || graph.nodes.len() != users.len() {
            users = build_users(graph);
        }
    }
    total_planted
}

/// Hoist loop-invariant `Guard(ExactClassIs)` / `Guard(Cmp(Ne/Eq))` and
/// dependent invariant `Load`s out of side-effect-free loops (including loops
/// whose only memory-touching op is a non-finalizable `Op::New` / `Op::NewArray`
/// and OSR loops whose only entry is their preheader).
pub(in crate::ir_optimize) fn hoist_invariant_guards_and_loads(
    graph: &mut Graph,
    osr_route: bool,
) -> usize {
    hoist_invariant_guards_and_loads_with_despec(graph, osr_route, None)
}

pub(in crate::ir_optimize) fn hoist_invariant_guards_and_loads_with_despec(
    graph: &mut Graph,
    osr_route: bool,
    despec: Option<(&crate::deopt::DespecRegistry, &str)>,
) -> usize {
    const MAX_GUARDS_PER_LOOP: usize = 24;
    if graph.safepoints.is_empty() {
        return 0;
    }
    let is_osr = osr_route || graph.osr_compile;
    let mut users = build_users(graph);
    let mut loops: Vec<(usize, (NodeId, NodeId, Vec<NodeId>))> = loop_headers(graph)
        .into_iter()
        .map(|l| (loop_body_with(graph, &users, l.0, l.1, &l.2).len(), l))
        .collect();
    loops.sort_by(|a, b| a.0.cmp(&b.0).then(a.1 .0.cmp(&b.1 .0)));

    let mut total_moved = 0usize;
    for (_, (region, entry_pred, back_ctrls)) in loops {
        if entry_pred == NO_NODE {
            continue;
        }
        let preheader = entry_pred;
        let mut body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        if is_osr
            && !loop_entered_only_through_its_pre_header(graph, region, entry_pred, &body)
            && !spliced_loop_entered_only_through_its_pre_header(graph, region, &body)
        {
            continue;
        }
        let clean_body = body.iter().all(|&id| {
            let Some(n) = graph.node_opt(id) else {
                return true;
            };
            if n.is_volatile_access() {
                return false;
            }
            if n.op.is_control()
                || matches!(
                    n.op,
                    Op::Phi
                        | Op::Dead
                        | Op::OsrMemory
                        | Op::Guard { .. }
                        | Op::ClassInitTest { .. }
                        | Op::Load(_)
                        | Op::ArrayLength
                        | Op::ConstString { .. }
                )
            {
                return true;
            }
            if let Op::New { class_id, .. } = n.op {
                return !graph.finalizable_new_classes.contains(&class_id);
            }
            if matches!(n.op, Op::NewArray { .. }) {
                return true;
            }
            !memory_shape_of(&n.op).may_write() && !licm_hoist_barrier(&n.op)
        });
        if !clean_body {
            continue;
        }
        let Some(entry_slot) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
            .map(|p| p + 1)
        else {
            continue;
        };
        let Some((header_bci, frame)) = loop_entry_frame(graph, region, entry_slot, &body) else {
            continue;
        };
        let header_despeced = despec.is_some_and(|(d, key)| {
            u32::try_from(header_bci)
                .ok()
                .is_some_and(|b| d.contains(key, b))
        });
        let Some(entry_mem) = loop_entry_memory(graph, region, entry_pred) else {
            continue;
        };

        let mut si: Option<u32> = None;
        let mut one_id: Option<NodeId> = None;
        let mut guards_planted = 0usize;
        let mut loop_moved = 0usize;

        for _ in 0..8 {
            let mut round_moved = 0usize;

            // Step A: hoist invariant Guard(ExactClassIs) and Guard(Cmp(Ne/Eq))
            let mut body_ids: Vec<NodeId> = body.iter().copied().collect();
            body_ids.sort_unstable();
            for gid in body_ids {
                if header_despeced || guards_planted >= MAX_GUARDS_PER_LOOP {
                    break;
                }
                let Some(gnode) = graph.node_opt(gid) else {
                    continue;
                };
                let Op::Guard { .. } = gnode.op else {
                    continue;
                };
                let guard_ctrl = gnode.inputs.first().copied().unwrap_or(NO_NODE);
                if guard_ctrl != region {
                    if users.len() != graph.nodes.len() {
                        users = build_users(graph);
                    }
                    if !back_edges_dominated_by(graph, &users, guard_ctrl, &back_ctrls) {
                        continue;
                    }
                }
                let Some(&cond_id) = gnode.inputs.get(1) else {
                    continue;
                };
                let Some(cnode) = graph.node_opt(cond_id) else {
                    continue;
                };
                enum CondKind {
                    ExactClass(u32, NodeId),
                    Cmp(CmpOp, NodeId, NodeId),
                }
                let kind = match cnode.op {
                    Op::ExactClassIs { class_id } => {
                        let Some(&base) = cnode.inputs.first() else {
                            continue;
                        };
                        if !is_loop_invariant(graph, base, region, &body) {
                            continue;
                        }
                        CondKind::ExactClass(class_id, base)
                    }
                    Op::Cmp(op @ (CmpOp::Ne | CmpOp::Eq)) => {
                        let (Some(&a), Some(&b)) = (cnode.inputs.first(), cnode.inputs.get(1))
                        else {
                            continue;
                        };
                        if !is_loop_invariant(graph, a, region, &body)
                            || !is_loop_invariant(graph, b, region, &body)
                        {
                            continue;
                        }
                        CondKind::Cmp(op, a, b)
                    }
                    _ => continue,
                };

                let snap_idx = match si {
                    Some(idx) => idx,
                    None => {
                        graph.push_safepoint(frame.clone());
                        let idx = (graph.safepoints.len() - 1) as u32;
                        si = Some(idx);
                        idx
                    }
                };
                let new_cond = match kind {
                    CondKind::ExactClass(class_id, base) => {
                        graph.add(Op::ExactClassIs { class_id }, IrType::Int, vec![base], None)
                    }
                    CondKind::Cmp(op, a, b) => {
                        graph.add(Op::Cmp(op), IrType::Int, vec![a, b], None)
                    }
                };
                let pre_guard = graph.add(
                    Op::Guard { bci: header_bci },
                    IrType::Void,
                    vec![preheader, new_cond],
                    Some(header_bci),
                );
                if !graph.set_node_frame_snapshot(pre_guard, snap_idx) {
                    graph.kill(pre_guard);
                    graph.kill(new_cond);
                    break;
                }
                let one = match one_id {
                    Some(o) => o,
                    None => {
                        let o = graph.add(Op::Const(1), IrType::Int, vec![], None);
                        one_id = Some(o);
                        o
                    }
                };
                let _ = graph.set_input(gid, 1, one);
                guards_planted += 1;
                round_moved += 1;
            }

            if round_moved > 0 {
                users = build_users(graph);
            }

            // Step B: hoist invariant Load(_) whose base is proven non-null at preheader
            let mut load_ids: Vec<NodeId> = body.iter().copied().collect();
            load_ids.sort_unstable();
            for lid in load_ids {
                let Some(lnode) = graph.node_opt(lid) else {
                    continue;
                };
                if !matches!(lnode.op, Op::Load(_)) {
                    continue;
                }
                let inputs = lnode.inputs.clone();
                if inputs.len() < 3 {
                    continue;
                }
                let base = inputs[2];
                if !is_loop_invariant(graph, base, region, &body) {
                    continue;
                }
                if inputs.len() >= 4 && !is_loop_invariant(graph, inputs[3], region, &body) {
                    continue;
                }
                let load_ctrl = inputs[0];
                if load_ctrl != region {
                    if users.len() != graph.nodes.len() {
                        users = build_users(graph);
                    }
                    if !back_edges_dominated_by(graph, &users, load_ctrl, &back_ctrls) {
                        continue;
                    }
                }
                let live_guard_types_base = body.iter().any(|&id| {
                    graph.node_opt(id).is_some_and(|gn| {
                        matches!(gn.op, Op::Guard { .. })
                            && gn
                                .inputs
                                .get(1)
                                .and_then(|&c| graph.node_opt(c))
                                .is_some_and(|cn| match cn.op {
                                    Op::ExactClassIs { .. }
                                    | Op::InstanceOf { .. }
                                    | Op::CheckCast { .. } => cn.inputs.contains(&base),
                                    Op::Cmp(CmpOp::Eq) => cn.inputs.iter().any(|&inp| {
                                        inp == base
                                            || graph.node_opt(inp).is_some_and(|in_node| {
                                                matches!(in_node.op, Op::ClassWordOf)
                                                    && in_node.inputs.contains(&base)
                                            })
                                    }),
                                    _ => false,
                                })
                    })
                });
                if live_guard_types_base {
                    continue;
                }
                let mut licensing_guard: NodeId = NO_NODE;
                let mut has_dominating_non_null_guard = false;
                for gid in 0..(graph.nodes.len() as u32) {
                    let Some(gn) = graph.node_opt(gid) else {
                        continue;
                    };
                    if !matches!(gn.op, Op::Guard { .. }) {
                        continue;
                    }
                    let gctrl = gn.inputs.first().copied().unwrap_or(NO_NODE);
                    if !control_dominates(graph, gctrl, preheader) {
                        continue;
                    }
                    let Some(&cid) = gn.inputs.get(1) else {
                        continue;
                    };
                    let Some(cn) = graph.node_opt(cid) else {
                        continue;
                    };
                    let (constrains_base, proves_non_null) = match cn.op {
                        Op::ExactClassIs { .. } if cn.inputs.contains(&base) => (true, true),
                        Op::Cmp(CmpOp::Ne) if cn.inputs.contains(&base) => {
                            let vs_null = cn.inputs.iter().any(|&x| {
                                x != base
                                    && graph.node_opt(x).is_some_and(|xn| xn.op == Op::Const(0))
                            });
                            (true, vs_null)
                        }
                        Op::Cmp(CmpOp::Eq) => {
                            let direct_or_cw = cn.inputs.iter().any(|&inp| {
                                inp == base
                                    || graph.node_opt(inp).is_some_and(|in_node| {
                                        matches!(in_node.op, Op::ClassWordOf)
                                            && in_node.inputs.contains(&base)
                                    })
                            });
                            let non_zero_cw = direct_or_cw
                                && cn.inputs.iter().any(|&inp| {
                                    graph.node_opt(inp).is_some_and(|xn| {
                                        matches!(xn.op, Op::Const(v) if v != 0)
                                            || matches!(xn.op, Op::IndyRootRef { .. })
                                    })
                                });
                            (direct_or_cw, non_zero_cw)
                        }
                        _ => (false, false),
                    };
                    if proves_non_null {
                        has_dominating_non_null_guard = true;
                    }
                    if constrains_base && gctrl == preheader {
                        licensing_guard = gid;
                    }
                }
                if !has_dominating_non_null_guard && !late_base_non_null(graph, base, preheader) {
                    if header_despeced || guards_planted >= MAX_GUARDS_PER_LOOP || inputs.len() < 4
                    {
                        continue;
                    }
                    let snap_idx = match si {
                        Some(idx) => idx,
                        None => {
                            graph.push_safepoint(frame.clone());
                            let idx = (graph.safepoints.len() - 1) as u32;
                            si = Some(idx);
                            idx
                        }
                    };
                    let null_c = graph.add(Op::Const(0), IrType::Ref, vec![], None);
                    let ne = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![base, null_c], None);
                    let pre_guard = graph.add(
                        Op::Guard { bci: header_bci },
                        IrType::Void,
                        vec![preheader, ne],
                        Some(header_bci),
                    );
                    if !graph.set_node_frame_snapshot(pre_guard, snap_idx) {
                        graph.kill(pre_guard);
                        graph.kill(ne);
                        graph.kill(null_c);
                        continue;
                    }
                    guards_planted += 1;
                    licensing_guard = pre_guard;
                }
                if licensing_guard != NO_NODE && inputs.len() < 4 {
                    continue;
                }
                let new_mem = if is_loop_invariant(graph, inputs[1], region, &body) {
                    inputs[1]
                } else {
                    entry_mem
                };
                let _copy =
                    late_hoist_copy_guarded(graph, lid, preheader, new_mem, licensing_guard);
                round_moved += 1;
            }

            if round_moved == 0 {
                break;
            }
            loop_moved += round_moved;
            users = build_users(graph);
            body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        }

        if loop_moved > 0 {
            total_moved += loop_moved;
        }
    }
    total_moved
}

pub(in crate::ir_optimize) fn eliminate_dead_nodes(graph: &mut Graph) {
    if graph.exit == NO_NODE {
        return;
    }

    // ── Defensive normalisation of already-stale snapshot slots ────────
    //
    // A slot may name a node that some *earlier* pass removed without routing
    // its safepoint references — historically this pass itself, which did not
    // root snapshots at all, so every optimized graph accumulated them. Such a
    // slot cannot be rescued: the value it named is already gone, and seeding
    // it as a root below would only resurrect an `Op::Dead` node. Normalise it
    // to `NO_NODE`, which the model already defines as "undefined at this bci"
    // and which `ir_lower` resolves to `FrameValue::Undefined` — the same
    // answer deopt would have reached anyway, but reached explicitly instead
    // of by indexing a removed node.
    //
    // This is emphatically NOT the fix for the live case handled below: a
    // value that is dead except for its snapshot reference is *kept*, never
    // cleared. Clearing that one would silently lose an interpreter local.
    if !graph.safepoints.is_empty() {
        let node_is_live: Vec<bool> = graph.nodes.iter().map(|n| n.op != Op::Dead).collect();
        // A clear only REMOVES a reference, so the tracked clearer keeps the
        // def-use lists current (a write through `graph.safepoints` would bump
        // the edge epoch since round 11 wave 18 and cost a rebuild per DCE).
        let _ = graph.clear_safepoint_slots_where(|_, _, v| {
            !node_is_live.get(v as usize).copied().unwrap_or(false)
        });
    }

    // A dense mark vector rather than a hash set: every node is asked once in
    // the kill sweep below, and ids are dense arena indices.
    let mut reachable: Vec<bool> = vec![false; graph.nodes.len()];
    // Seed the worklist with EVERY `Op::Return`, not just `graph.exit`. A method
    // with multiple `return` statements (e.g. `if (c) return A; return B;`)
    // builds several `Op::Return` terminators, but `graph.exit` records only the
    // last one — each `ireturn` overwrites it in the builder. Rooting DCE solely
    // from `graph.exit` would mark every OTHER return path unreachable and delete
    // it (control, value, and the `If`'s opposite projection), collapsing the
    // conditional into a single-successor branch that always takes the surviving
    // return. Every Return is an observable program exit and must be a root.
    // Seed from every `Op::Return`, every `Op::Store`, AND every `Op::Call`. A
    // store or call is an observable side effect whose result (a memory token /
    // return value) may be consumed by no one — a pure-write `o.x = v; return
    // v;` returns the value, not the store; a void static call
    // (`Foo.sideEffect(); return;`) has no value consumer at all — so the node
    // is unreachable from any Return. Rooting it (and, transitively, the memory
    // chain it depends on, including its argument values) keeps the effect from
    // being deleted. Strictly additive: a store removed by DSE is `Op::Dead` and
    // not matched, and a live store/call always has an observable effect.
    // `Op::ArrayLoad`/`Op::ArrayStore` are also side-effecting roots: BOTH can
    // throw (NullPointerException / ArrayIndexOutOfBoundsException via the
    // lowerer's deopt guards), an observable effect, so neither may be deleted
    // even when its result/memory token has no consumer (a `float v = a[i];`
    // whose `v` is unused still performs the bounds check).
    //
    // COV-02 adds `Op::ArrayLength` for that reason and no other. It is a pure
    // read of immutable storage — but it throws NullPointerException on a null
    // array through the same deopt guard, and that throw is the observable
    // effect. `int n = a.length;` with `n` unused still has to NPE; deleting it
    // is not a faster answer, it is a dropped exception.
    //
    // The root set is decided by `is_dce_root`, an exhaustive match: a new
    // `Op` does not compile until someone says whether deleting an unconsumed
    // one is observable. The old allowlist silently made every unlisted op
    // removable, which is how `getfield`, `checkcast`, `new` and `unbox` --
    // all of which can throw -- came to be deleted when their value was unused.
    let mut worklist: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| is_dce_root(&n.op) && !trusted_static_final_read(graph, &n.op))
        .map(|(id, _)| id as NodeId)
        .collect();
    if worklist.is_empty() {
        worklist.push(graph.exit);
    }
    worklist.extend(graph.wide_entry.values().copied());

    // real-frame-deopt (#5): every value a safepoint snapshot names IS a DCE
    // root. A snapshot slot is what deopt rebuilds an interpreter local or
    // operand-stack entry from, so a slot naming a node this pass removed is a
    // deoptimization correctness bug — the frame is reconstructed from a node
    // that no longer computes anything — not untidiness. "Reachable from a
    // Return" is the wrong liveness question for these values: a local that
    // the compiled code never reads again is still live to the interpreter it
    // may deopt back into.
    //
    // This deliberately reverses the previous policy ("snapshots are NOT
    // seeded as DCE roots … a value killed here resolves to `Undefined`"). The
    // cost that policy was buying is real and unchanged — the builder records
    // a snapshot at *every* bytecode boundary, so this pins essentially every
    // operand the method computes and DCE reclaims much less — but the cost of
    // the alternative is a wrong interpreter frame, and code size is not
    // tradeable against that. The way to get the reclamation back is the model
    // change the old comment already named: record snapshots only at real
    // deopt sites (guard bcis, call returns), or recompute snapshot liveness
    // after optimization. Narrowing what is *recorded* is safe; dropping what
    // is recorded but not rooted is not.
    // The monitor stack is rooted with the locals: a resumed frame re-acquires
    // or keeps holding the lock on exactly the reference it names.
    //
    // Round 9 wave 2 (`ir-snapshots-at-every-bci-pin-values-and-now-stores`):
    // with `CRATONVM_JIT_IR_SNAPSHOT_LIVENESS=1`, only the snapshots a deopt
    // can CONSULT are roots — see [`consultable_snapshots`]. The rest are the
    // ones `ea_ir_bridge::ir_prune_unconsumable_snapshots` drops after escape
    // analysis anyway; rooting them here only kept alive values no resume can
    // ever read. A slot of such a snapshot whose node this pass then kills is
    // normalised to `NO_NODE` at the end, exactly like the stale slots above.
    //
    // JIT round 19 wave 8 (lane osr8, OSR7-2): a promoted loop's forced join
    // value phis (`Graph::promoted_join_phis`, empty unless
    // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_JOIN_WALK` built some) are read by
    // the lowering's deopt write-back pads, which no edge shows.
    for &phi in &graph.promoted_join_phis {
        if graph.node_opt(phi).is_some_and(|n| n.op == Op::Phi) {
            worklist.push(phi);
        }
    }
    let snapshot_mask = live_snapshot_mask(graph);
    for (si, sp) in graph.safepoints.iter().enumerate() {
        if snapshot_mask
            .as_ref()
            .is_some_and(|m| !m.get(si).copied().unwrap_or(true))
        {
            continue;
        }
        for &slot in sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
        {
            // Stale slots were normalised to `NO_NODE` above, so anything that
            // survives `is_valid_id` here names a real, live node.
            if graph.is_valid_id(slot) {
                worklist.push(slot);
            }
        }
    }

    // Every control node forward-reachable from `Start` is a root (r9 wave 2,
    // `ir-dce-deletes-the-control-of-an-effect-free-infinite-loop`).
    //
    // Backward reachability from the roots above keeps a control path only if
    // it LEADS to a root, and one path never does: an infinite loop whose body
    // has no memory op, no call, no guard and no value a snapshot names
    // (`if (spin) { while (true) {} } return 1;`). `Return` roots `Proj(false)`
    // → `If` → `Start`; nothing rooted `Proj(true)` or the loop's `Merge`, so
    // both were killed and the `If` survived with one live projection — a
    // two-way branch with an edge going nowhere, which `ir_lower` has no block
    // for. Java has no forward-progress license: the thread must spin (and
    // `ir_lower` polls on every back edge, so it spins at a safepoint).
    //
    // Rooting the forward-reachable control set changes nothing for a graph
    // whose every control path ends at a root (all of them, but this shape) and
    // still reclaims control that is unreachable from `Start` — the only control
    // this pass has any business deleting. The walk follows control-typed
    // STRUCTURE nodes only; a guard or a call consumes control but is a root in
    // its own right, and the builder never threads control THROUGH one.
    if graph.is_valid_id(graph.entry) && graph.nodes[graph.entry as usize].op != Op::Dead {
        let users = build_users(graph);
        let mut seen = vec![false; graph.nodes.len()];
        let mut stack: Vec<NodeId> = vec![graph.entry];
        seen[graph.entry as usize] = true;
        while let Some(c) = stack.pop() {
            worklist.push(c);
            for &u in &users[c as usize] {
                let un = &graph.nodes[u as usize];
                if !seen[u as usize] && is_forward_control(un) {
                    seen[u as usize] = true;
                    stack.push(u);
                }
            }
        }
    }

    // Also keep all control nodes reachable from exit
    while let Some(id) = worklist.pop() {
        match reachable.get_mut(id as usize) {
            Some(seen) if !*seen => *seen = true,
            _ => continue,
        }
        let node = &graph.nodes[id as usize];
        for &inp in &node.inputs {
            if graph.is_valid_id(inp) {
                worklist.push(inp);
            }
        }
    }

    // Kill unreachable nodes (except Start which is always kept)
    let mut killed = 0u64;
    for id in 0..graph.nodes.len() {
        if !reachable[id] && graph.nodes[id].op != Op::Dead {
            // Keep Start alive always
            if id as NodeId == graph.entry {
                continue;
            }
            graph.kill(id as NodeId);
            killed += 1;
        }
    }
    if killed > 0 {
        IR_DCE_NODES_KILLED.fetch_add(killed, std::sync::atomic::Ordering::Relaxed);
    }

    // A snapshot the mask did not root may name a node just killed. It is
    // unconsultable by construction, so "undefined here" is the honest answer
    // and the one `ir_verify`'s frame-state lane accepts; leaving the dead id
    // would fail that lane at `"post-optimize"` and fall the method back. A
    // CONSULTABLE snapshot's slots were all roots and cannot be affected.
    if snapshot_mask.is_some() {
        let node_is_live: Vec<bool> = graph.nodes.iter().map(|n| n.op != Op::Dead).collect();
        // Tracked clear, as above (`clear_safepoint_slots_where` skips
        // `NO_NODE` slots itself).
        let _ = graph.clear_safepoint_slots_where(|_, _, v| {
            !node_is_live.get(v as usize).copied().unwrap_or(false)
        });
    }
}

// ── Snapshot liveness (round 9 wave 2) ──────────────────────────────

thread_local! {
    /// Whether the current [`optimize`] call runs with snapshot liveness
    /// (`CRATONVM_JIT_IR_SNAPSHOT_LIVENESS=1`). Set at the top of `optimize`
    /// and cleared at its end, so a pass called OUTSIDE `optimize` (a unit
    /// test, a pipeline helper) keeps the conservative every-snapshot-is-a-root
    /// behaviour, and the flag is read once per compile rather than once per
    /// dead-node sweep.
    pub(in crate::ir_optimize) static SNAPSHOT_LIVENESS_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The [`crate::ir::IrSpliceResumeMap`] of the graph the current
    /// [`optimize`] call is optimizing, when snapshot liveness is on and the
    /// build spliced something. Set and cleared with
    /// [`SNAPSHOT_LIVENESS_ON`]; `None` outside `optimize`.
    pub(in crate::ir_optimize) static SPLICE_RESUME: std::cell::RefCell<Option<crate::ir::IrSpliceResumeMap>> =
        const { std::cell::RefCell::new(None) };
}

/// `CRATONVM_JIT_IR_SNAPSHOT_LIVENESS=1` lets `eliminate_dead_nodes` and the
/// write-only phase of dead-store elimination treat only the CONSULTABLE
/// safepoint snapshots ([`consultable_snapshots`]) as uses. **Default OFF.**
///
/// The criterion is not new: `ea_ir_bridge::ir_prune_unconsumable_snapshots`
/// applies it, unconditionally, after escape analysis and drops every other
/// snapshot. What is new is applying it BEFORE the optimizer's own two
/// consumers, which is where the reclamation is. It lands off because this is
/// deopt metadata — a wrong frame does not crash, it answers — and earns its
/// default on a checksum-parity soak, as `ir_per_copy_frames_enabled` must.
pub(in crate::ir_optimize) fn snapshot_liveness_enabled() -> bool {
    false
}

// ── Snapshot-liveness census (round 9 wave 6) ───────────────────────
//
// The "How to verify" section of
// `ir-snapshots-at-every-bci-pin-values-and-now-stores-20260918.md` asks for a
// count of what the two consumers reclaim, before and after, so the flag's
// default can be decided on evidence rather than on wall-clock noise. The two
// reclamation counters run in BOTH arms, so an A/B of
// `CRATONVM_JIT_IR_SNAPSHOT_LIVENESS=0/1` over one workload reads the effect as
// a difference; the snapshot counters run only under liveness, once per
// `optimize`, over the final graph.

/// Nodes `eliminate_dead_nodes` killed, process-wide, both arms.
pub(in crate::ir_optimize) static IR_DCE_NODES_KILLED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Stores `eliminate_write_only_stores` removed, process-wide, both arms.
pub(in crate::ir_optimize) static IR_WRITE_ONLY_STORES_REMOVED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Graphs `optimize` finished under liveness whose snapshots could be
/// attributed (the mask was computed).
pub(in crate::ir_optimize) static IR_LIVENESS_GRAPHS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Graphs `optimize` finished under liveness that fell back to "every snapshot
/// is a use" (a trapping node with no pc, or a pc no snapshot describes).
pub(in crate::ir_optimize) static IR_LIVENESS_UNATTRIBUTABLE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Snapshots in the attributed graphs above.
pub(in crate::ir_optimize) static IR_LIVENESS_SNAPSHOTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// ...of which no deopt can consult.
pub(in crate::ir_optimize) static IR_LIVENESS_UNCONSULTABLE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// What snapshot liveness did, process-wide, since the process started. Read
/// by the VM census (`[c2-supersede]`). Monotone; read a delta for one run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotLivenessCensus {
    /// Nodes dead-node elimination killed (both arms of the flag).
    pub dce_nodes_killed: u64,
    /// Stores write-only dead-store elimination removed (both arms).
    pub write_only_stores_removed: u64,
    /// Graphs optimized under liveness whose snapshots were attributed.
    pub graphs: u64,
    /// Graphs optimized under liveness that could not be attributed.
    pub unattributable: u64,
    /// Snapshots in the attributed graphs.
    pub snapshots: u64,
    /// Of those, the ones no deopt can consult (not rooted, not a use).
    pub unconsultable: u64,
}

/// The [`SnapshotLivenessCensus`] so far.
pub fn ir_snapshot_liveness_census() -> SnapshotLivenessCensus {
    use std::sync::atomic::Ordering::Relaxed;
    SnapshotLivenessCensus {
        dce_nodes_killed: IR_DCE_NODES_KILLED.load(Relaxed),
        write_only_stores_removed: IR_WRITE_ONLY_STORES_REMOVED.load(Relaxed),
        graphs: IR_LIVENESS_GRAPHS.load(Relaxed),
        unattributable: IR_LIVENESS_UNATTRIBUTABLE.load(Relaxed),
        snapshots: IR_LIVENESS_SNAPSHOTS.load(Relaxed),
        unconsultable: IR_LIVENESS_UNCONSULTABLE.load(Relaxed),
    }
}

/// Record the final graph's consultable-snapshot mask in the census. Called
/// once, at the end of [`optimize`], only under liveness (the thread-local
/// state [`consultable_snapshots`] reads is still set there).
pub(in crate::ir_optimize) fn note_snapshot_liveness_census(graph: &Graph) {
    use std::sync::atomic::Ordering::Relaxed;
    match consultable_snapshots(graph) {
        Some(mask) => {
            let unconsultable = mask.iter().filter(|&&c| !c).count() as u64;
            IR_LIVENESS_GRAPHS.fetch_add(1, Relaxed);
            IR_LIVENESS_SNAPSHOTS.fetch_add(mask.len() as u64, Relaxed);
            IR_LIVENESS_UNCONSULTABLE.fetch_add(unconsultable, Relaxed);
        }
        None => {
            IR_LIVENESS_UNATTRIBUTABLE.fetch_add(1, Relaxed);
        }
    }
}

/// The consultable-snapshot mask to apply now, or `None` for "every snapshot
/// is a use" (liveness off, or the graph cannot be attributed).
pub(in crate::ir_optimize) fn live_snapshot_mask(graph: &Graph) -> Option<Vec<bool>> {
    if !SNAPSHOT_LIVENESS_ON.with(|c| c.get()) {
        return None;
    }
    consultable_snapshots(graph)
}

/// `mask[si]` is true when a deopt of `graph` can consult
/// `graph.safepoints[si]`; `None` when that cannot be attributed and every
/// snapshot must be treated as consultable.
///
/// The same rule as `ea_ir_bridge::ir_consumable_snapshot_bcis`, with the
/// spliced bodies resolved through the build's
/// [`crate::ir::IrSpliceResumeMap`] when one was published (see
/// [`consultable_snapshots_with`]). A snapshot is consultable when
///
/// * its bci is the `bytecode_pc` of a live node that can transfer to the
///   interpreter (`!ir_lower::node_cannot_deopt` -- graph-aware since round 9
///   wave 4, so a `putfield` into a fresh allocation, whose lowering emits no
///   null check, is not a transfer site), or an `Op::Guard`'s own `bci`
///   — every deopt stub and exceptional exit the lowerer emits is emitted while
///   lowering such a node, at that node's bci;
/// * its bci is a live `Op::Merge`/`Op::Region`'s — an optimizing OSR entry is
///   planned at a join and seeds the frame from the snapshot there;
/// * a live node claims it through `Node::frame_snapshot` (per-copy frames).
///
/// `None` when a node that can deopt carries no `bytecode_pc`, or one whose bci
/// has no snapshot and cannot be resolved to one — a node of a SPLICED callee
/// body (combined-buffer pcs, never snapshotted) when no splice resume map is
/// available, since its resume bci is the enclosing `invoke`'s.
///
/// # Why it is monotone across the rest of the pipeline
///
/// Every pass after this point only REMOVES trapping nodes and joins, or
/// copies/moves them keeping their `bytecode_pc` (unroll, LICM); escape
/// analysis adds constants only. So the set a later consumer computes — the
/// post-EA prune, `ir_lower` — is a subset of this one, and a snapshot this
/// mask leaves unrooted is never consulted.
///
/// The two graph-aware clauses keep that property because nothing later
/// re-arms them: no pass rewrites a store's receiver away from the
/// `Op::New`/`Op::NewArray` it names (allocations are never value-numbered, a
/// clone of the store names the clone of its allocation, and scalar
/// replacement kills the allocation and its stores together), nor a constant
/// field offset or divisor into a non-constant. (The post-EA prune still asks
/// `op_cannot_deopt`, a superset of transfer sites; keeping MORE snapshots
/// than this mask is harmless, because `ir_lower` resolves a frame only at a
/// deopt it actually emits.)
pub(in crate::ir_optimize) fn consultable_snapshots(graph: &Graph) -> Option<Vec<bool>> {
    SPLICE_RESUME.with(|c| consultable_snapshots_with(graph, c.borrow().as_ref()))
}

/// [`consultable_snapshots`] with the splice resume map made explicit.
///
/// # Spliced bodies (round 9 wave 3)
///
/// With `splices == None` a node of a spliced body (combined-buffer pc, no
/// snapshot) makes the whole graph unattributable, which is the conservative
/// arm and what every graph with a splice got in wave 2 — i.e. liveness never
/// applied to a method the IR inliner had touched. With the map, such a node's
/// bci is resolved the way `ir_lower`'s `resume_bci` resolves it (the
/// outermost enclosing `invoke`, whose snapshot the main walk recorded before
/// the splice began), and that snapshot is consultable. A join inside a body
/// marks its resume bci as well as its own pc. A pc the map cannot place, or
/// that resolves to a bci with no snapshot, is still `None`.
pub(in crate::ir_optimize) fn consultable_snapshots_with(
    graph: &Graph,
    splices: Option<&crate::ir::IrSpliceResumeMap>,
) -> Option<Vec<bool>> {
    // Round 13 wave 3 (lane framestate): the by-bci rule below is about frames
    // of the compiling method. A chain snapshot (`ir::SpliceScopeChain`, at a
    // spliced pc) is consulted IN ADDITION to the flat frame at the outermost
    // invoke, never instead of it: `ir_lower` falls back to the flat frame for
    // any guard whose chain the VM could not resume. So a spliced trapping pc
    // marks both.
    let snapshot_bcis: FxHashSet<usize> = graph
        .safepoints
        .iter()
        .filter(|sp| !sp.is_splice_state())
        .map(|sp| sp.bci)
        .collect();
    let chain_bcis: FxHashSet<usize> = graph
        .safepoints
        .iter()
        .filter(|sp| sp.is_guard_splice_state())
        .map(|sp| sp.bci)
        .collect();
    // The snapshot bci a deopt at `pc` consults, or `None` when that cannot be
    // attributed.
    let resume = |pc: usize| -> Option<usize> {
        if snapshot_bcis.contains(&pc) {
            return Some(pc);
        }
        let at = splices?.resume_bci(pc)?;
        snapshot_bcis.contains(&at).then_some(at)
    };
    let mut bcis: FxHashSet<usize> = FxHashSet::default();
    let mut claimed: FxHashSet<u32> = FxHashSet::default();
    for n in &graph.nodes {
        if n.op == Op::Dead {
            continue;
        }
        if let Some(si) = n.frame_snapshot {
            claimed.insert(si);
        }
        if matches!(n.op, Op::Merge | Op::Region) {
            if let Some(pc) = n.bytecode_pc {
                bcis.insert(pc);
                if let Some(at) = splices.and_then(|m| m.resume_bci(pc)) {
                    bcis.insert(at);
                }
            }
            continue;
        }
        // A chain snapshot is rooted by the COARSE predicate (`op_cannot_deopt`,
        // the superset the post-EA prune keeps by): the lowerer consults it at
        // the raw pc of whichever node it emits a guard for, and a chain slot
        // this pass let go would be normalised to `NO_NODE` -- an `Undefined`
        // local no resume sink can tell from a genuinely unset one.
        if let Some(pc) = n.bytecode_pc {
            if chain_bcis.contains(&pc) && !crate::ir_lower::op_cannot_deopt(&n.op) {
                bcis.insert(pc);
            }
        }
        if let Op::Guard { bci } = n.op {
            if chain_bcis.contains(&bci) {
                bcis.insert(bci);
            }
        }
        // Graph-aware (round 9 wave 4, request L3): a store into a fresh
        // allocation and a division by a non-zero constant emit no deopt, and
        // `node_cannot_deopt` is the predicate the `ir_lower` emission reads.
        if crate::ir_lower::node_cannot_deopt(graph, n) {
            continue;
        }
        let pc = n.bytecode_pc?;
        bcis.insert(resume(pc)?);
        if let Op::Guard { bci } = n.op {
            bcis.insert(resume(bci)?);
        }
    }
    Some(
        graph
            .safepoints
            .iter()
            .enumerate()
            .map(|(si, sp)| {
                bcis.contains(&sp.bci)
                    || claimed.contains(&(si as u32))
                    // A post-call chain snapshot (interpreter round i1 wave
                    // 47, lane L3) is consulted by its call's exit, not at
                    // its pc: a value only it names must stay described.
                    || sp.post_call_of != crate::ir::NO_NODE
            })
            .collect(),
    )
}

// ── Loop unrolling (activate-ir-optimizer increment 3) ───────────────────

/// `true` (default) when full unrolling of small constant-trip-count counted
/// loops runs. Flipped default-ON after the increment-3 unroll pass soaked clean
/// (validated live against HotSpot: bt10/14/16/18 + UnrollProbe/UnrollProbe2 +
/// loop-heavy bench differential gate-ON ≡ gate-OFF == HotSpot).
/// `CRATONVM_JIT_UNROLL=0` is the opt-out that restores the pre-flip (no-unroll)
/// behaviour as the safety net.
/// The opt-out accepts `0|false|off|no`, not just the literal `0`: this gate
/// was `map_or(true, |v| v != "0")` until 2026-09-16, so
/// `CRATONVM_JIT_UNROLL=false` left the unroller running. See
/// [`licm_enabled`] for the full note; both switches moved together because
/// they are the two `x64/single_pass_only.rs` cites as the reason the
/// single-pass loop transforms are NOT on its veto list, and a veto list must
/// not reason from a switch that cannot be turned off.
pub fn unroll_enabled() -> bool {
    true
}

/// Why each candidate loop header did or did not unroll.
///
/// # Why this exists
///
/// `docs/JIT_OPTIMIZATION.md` has said since 2026-09-03 that *"the optimizing
/// tier does not unroll"*, and priced that at about 1.12x on a counted loop.
/// Both halves needed checking before anything was built, and only one of them
/// survived: this pass EXISTS, is correct, and recognises a javac counted loop
/// (`analyze_counted_loop` returns `trip=5 init=0 stride=1` on one). What it
/// then does is refuse it.
///
/// A bare "it did not fire" cannot distinguish the two reasons that matter,
/// and they call for opposite work:
///
///   * [`Self::runtime_bound`] — a counted loop whose bound is a VALUE rather
///     than a constant, which is most Java loops and every one this pass was
///     never meant to serve. Full unrolling can never apply to it; PARTIAL
///     unrolling is the thing that would, and it is unbuilt. This counter is
///     how big that prize is.
///   * [`Self::safepoint_named`] — a loop this pass fully models and declines
///     anyway, because `SafepointSnapshot` is keyed by one bci and an unrolled
///     body has several copies of each. That refusal has an escape hatch for a
///     trap-free graph, and the hatch is gated on
///     `CRATONVM_JIT_IR_DROP_UNREACHABLE_HOMES`, which is **default OFF** — so
///     on a default run this pass refuses essentially every loop it
///     understands. This counter is how much is being left on the floor by a
///     flag that is off for an unrelated reason.
///
/// Every candidate header falls into exactly one bucket, which
/// [`Self::closes`] checks and `unroll` debug-asserts, for the reason
/// `c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md` §10.1
/// gives: a census that computes its own answer instead of reading the one the
/// code used will drift, and it drifts towards zero.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UnrollCensus {
    /// Two-input control merges examined — **not** a count of loops.
    ///
    /// `unroll` scans every `Region`/`Merge` with at least two control inputs,
    /// and most of those are if/else joins rather than loop headers. They land
    /// in [`Self::not_single_backedge`], which is therefore mostly "not a loop"
    /// rather than "a loop of a shape this pass declines". The count of loops
    /// actually FOUND is `headers - not_single_backedge`, and the honest
    /// denominator for anything about unrolling is that, not this.
    pub headers: usize,
    /// Unrolled.
    pub unrolled: usize,
    /// Not a single-back-edge loop, or no entry predecessor. **Mostly not a
    /// loop at all** — see [`Self::headers`].
    pub not_single_backedge: usize,
    /// No constant-stride induction phi, or no single loop-exit `If`.
    pub not_counted: usize,
    /// A counted loop whose BOUND is a runtime value. The partial-unroll
    /// population, and the one thing on this list that is a missing feature
    /// rather than a refusal.
    pub runtime_bound: usize,
    /// Constant trip count above `UNROLL_MAX_TRIP`.
    pub trip_over_cap: usize,
    /// Not a single-block loop (region/if/proj shape).
    pub control_shape: usize,
    /// A store, call, allocation or array access in the loop.
    pub side_effect: usize,
    /// A body node this pass cannot clone, or a body over `UNROLL_MAX_BODY`.
    pub body_unclonable: usize,
    /// A safepoint snapshot names the body, and the trap-free escape was not
    /// available. **On a default run this is the dominant refusal.**
    pub safepoint_named: usize,
    /// A body value escapes the loop, or an invariant load is pinned to the
    /// header.
    pub escapes_or_pinned: usize,
    /// A safepoint snapshot names the body at a bci **no cloned node carries**,
    /// so per-copy frames could not be built and the loop was refused.
    ///
    /// Distinct from [`Self::safepoint_named`], which is the refusal taken when
    /// per-copy frames are switched OFF. This one is taken with them ON: the
    /// mechanism was available and the loop still could not use it, because a
    /// copy's frame has to hang on a program point the copy actually has. See
    /// [`plan_copy_frames`].
    pub frame_uncopyable: usize,
    /// Of [`Self::runtime_bound`], how many sit in a graph NOTHING can deopt
    /// from. **Not part of the closing identity** — it is a sub-count of
    /// `runtime_bound`, not a bucket of its own.
    ///
    /// This is the number that decides whether a partial unroller is worth
    /// building, and it is the only honest denominator for one. A partial
    /// unroll clones the body, a deopt out of copy `k` needs a frame state
    /// naming copy `k`'s values, and `SafepointSnapshot` is keyed by one bci —
    /// so on a graph that CAN deopt, partial unrolling needs the same
    /// per-iteration snapshot index the full unroller's own comment calls "a
    /// lowerer change". On a trap-free graph no snapshot can ever be consulted
    /// and that whole obligation is discharged by construction.
    pub runtime_bound_trap_free: usize,
    /// Of [`Self::runtime_bound`], how many have a body a partial unroller
    /// could clone whose clones can none of them DEOPT. **Not part of the
    /// closing identity.**
    ///
    /// The honest denominator for a partial unroller, and a different question
    /// from [`Self::runtime_bound_trap_free`]: that one asks whether the whole
    /// METHOD is trap-free, this one whether the cloned NODES are. See
    /// [`partial_unroll_body_is_pure`].
    pub runtime_bound_pure_body: usize,
    /// Of [`Self::unrolled`], how many were unrolled over a body a safepoint
    /// snapshot names — i.e. how many took per-copy frames. **Not part of the
    /// closing identity**; it is a sub-count of `unrolled`.
    ///
    /// The engagement number for [`ir_per_copy_frames_enabled`]. Zero with the
    /// flag off, and zero with it on until a loop is met that the old
    /// [`Self::safepoint_named`] refusal would have declined — which is the
    /// pair of counters to read together, because one moving without the other
    /// falling means the mechanism is firing on loops that never needed it.
    pub per_copy_frames: usize,
    /// Runtime-bound loops declined for being runtime-bound **and nothing
    /// else** — i.e. with `CRATONVM_JIT_IR_PARTIAL_UNROLL` off. Part of the
    /// closing identity; [`Self::runtime_bound`] is its non-terminal superset.
    ///
    /// Equal to `runtime_bound` on a default run. The gap between them, when
    /// the flag is on, is the number of loops the partial unroller took
    /// responsibility for — whether it then unrolled them or refused them for a
    /// second reason.
    pub runtime_bound_refused: usize,
    /// Loops given a PARTIAL unroll: the body emitted `factor` times, each copy
    /// behind its own copy of the loop test, the loop itself kept. Part of the
    /// closing identity, and disjoint from [`Self::unrolled`], which counts only
    /// loops that were retired entirely.
    pub partially_unrolled: usize,
}

impl UnrollCensus {
    /// Does every candidate header fall into exactly one bucket?
    ///
    /// [`Self::runtime_bound`] is deliberately NOT a term, and it stopped being
    /// one when the partial unroller landed. It counts a loop SHAPE — "the bound
    /// is a value" — which is no longer a disposition: such a loop now goes on
    /// to the same control-shape, side-effect, clonability and frame checks a
    /// constant-trip one does, and lands in whichever of those buckets, or in
    /// [`Self::partially_unrolled`], actually disposed of it. Its terminal
    /// counterpart is [`Self::runtime_bound_refused`], which is the count of
    /// runtime-bound loops declined for being runtime-bound and nothing else.
    ///
    /// With `CRATONVM_JIT_IR_PARTIAL_UNROLL` off (the default) the two are
    /// equal and every published number is what it was before.
    pub fn closes(&self) -> bool {
        self.unrolled
            + self.partially_unrolled
            + self.not_single_backedge
            + self.not_counted
            + self.runtime_bound_refused
            + self.trip_over_cap
            + self.control_shape
            + self.side_effect
            + self.body_unclonable
            + self.safepoint_named
            + self.escapes_or_pinned
            + self.frame_uncopyable
            == self.headers
    }
}

/// Process-wide unroll totals, in [`UnrollCensus`] field order.
///
/// Unconditional and cheap -- at most one relaxed add per candidate loop
/// header per compile. A census you have to switch on is one nobody has for
/// the run they already did.
pub(in crate::ir_optimize) static IR_UNROLL_CENSUS: [std::sync::atomic::AtomicU64; 17] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Add one compile's tally to the process-wide totals.
///
/// Published once per `unroll` call rather than per header, so the identity
/// below can be asserted on a tally no other thread is touching: background
/// compilation is default-ON, and a process-wide check would race a sibling
/// thread that has counted a header and not yet its bucket.
pub(in crate::ir_optimize) fn publish_unroll_census(local: &[usize; 17]) {
    debug_assert!(
        {
            // Spelled out rather than taken as a slice range, because the
            // terminal buckets are no longer contiguous: `RUNTIME_BOUND` sits
            // among them and is a SHAPE count, not a disposition (see
            // `UnrollCensus::closes`), while `PARTIALLY_UNROLLED` and
            // `RUNTIME_BOUND_REFUSED` were appended after the sub-counts. A
            // range would silently take the wrong terms.
            let total: usize = local[unroll_bucket::UNROLLED]
                + local[unroll_bucket::PARTIALLY_UNROLLED]
                + local[unroll_bucket::NOT_SINGLE_BACKEDGE]
                + local[unroll_bucket::NOT_COUNTED]
                + local[unroll_bucket::RUNTIME_BOUND_REFUSED]
                + local[unroll_bucket::TRIP_OVER_CAP]
                + local[unroll_bucket::CONTROL_SHAPE]
                + local[unroll_bucket::SIDE_EFFECT]
                + local[unroll_bucket::BODY_UNCLONABLE]
                + local[unroll_bucket::SAFEPOINT_NAMED]
                + local[unroll_bucket::ESCAPES_OR_PINNED]
                + local[unroll_bucket::FRAME_UNCOPYABLE];
            total == local[unroll_bucket::HEADERS]
        },
        "UnrollCensus does not close: {local:?} -- a `continue` in `unroll` has \
         no counter",
    );
    for (slot, v) in IR_UNROLL_CENSUS.iter().zip(local.iter()) {
        slot.fetch_add(*v as u64, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The process-wide unroll census since process start.
pub fn ir_unroll_census() -> UnrollCensus {
    use std::sync::atomic::Ordering::Relaxed;
    let v: Vec<usize> = IR_UNROLL_CENSUS
        .iter()
        .map(|a| a.load(Relaxed) as usize)
        .collect();
    UnrollCensus {
        headers: v[0],
        unrolled: v[1],
        not_single_backedge: v[2],
        not_counted: v[3],
        runtime_bound: v[4],
        trip_over_cap: v[5],
        control_shape: v[6],
        side_effect: v[7],
        body_unclonable: v[8],
        safepoint_named: v[9],
        escapes_or_pinned: v[10],
        frame_uncopyable: v[11],
        runtime_bound_trap_free: v[12],
        runtime_bound_pure_body: v[13],
        per_copy_frames: v[14],
        runtime_bound_refused: v[15],
        partially_unrolled: v[16],
    }
}

/// Only fully unroll loops whose constant trip count is at most this (bounds
/// code growth; larger counted loops are left rolled).
pub(in crate::ir_optimize) const UNROLL_MAX_TRIP: i64 = 8;
/// Skip loops whose per-iteration computation exceeds this many nodes.
pub(in crate::ir_optimize) const UNROLL_MAX_BODY: usize = 48;
/// Round nio20 wave 5 (lane L3), under [`unroll_wide_counted_enabled`]: the
/// trip cap of a full unroll whose body is SMALL -- `trip * body` nodes at most
/// [`UNROLL_MAX_TOTAL_WIDE`]. HotSpot's `LoopMaxUnroll` is 16 and C2 fully
/// unrolls `for (k = 0; k < 16; k++) h += k ^ i` (`Nio20FenceRow noload`,
/// HotSpot 4 ns against our 13 with the loop kept rolled). A trip of 9..=16
/// over a larger body is refused as before (`trip_over_cap`).
pub(in crate::ir_optimize) const UNROLL_MAX_TRIP_WIDE: i64 = 16;
/// The emitted-node budget of a full unroll past [`UNROLL_MAX_TRIP`]: 16 trips
/// of a 10-node body. The cap at or below `UNROLL_MAX_TRIP` is unchanged (8 x
/// [`UNROLL_MAX_BODY`]).
pub(in crate::ir_optimize) const UNROLL_MAX_TOTAL_WIDE: usize = 160;

/// Round nio20 wave 5 (lane L3): `CRATONVM_JIT_IR_UNROLL_WIDE_COUNTED`,
/// **default ON** (`0` restores the old recogniser byte for byte). Widens what
/// [`analyze_counted_loop`] recognises as a counted loop:
///
/// * the induction phi's entry and back-edge values are read from the slots
///   `unroll` DERIVED from the header's predecessor order, not from the literal
///   `inputs[1]` / `inputs[2]` (the derivation's own comment names this as
///   the next improvement; a flipped header used to leave as "no constant-stride
///   induction phi");
/// * a phi whose ENTRY value is not a constant (an OSR-seeded counter, a
///   parameter, `for (k = from; ...)`) is still an induction variable: the trip
///   count is then unknown, so the loop is reported as runtime-bound -- the
///   population the main/post partial unroller serves -- instead of "not a
///   counted loop";
/// * `phi - c` is a constant stride `-c` (`i = i - 1` compiles to `isub`);
/// * a full unroll takes a constant trip of up to [`UNROLL_MAX_TRIP_WIDE`] when
///   the body is small ([`UNROLL_MAX_TOTAL_WIDE`]);
/// * a constant trip past that cap is offered to the main/post partial
///   unroller (a constant bound is a loop-invariant one) instead of being
///   refused outright.
///
/// Read per loop, live: the OFF arm must be testable in the same process.
pub(in crate::ir_optimize) fn unroll_wide_counted_enabled() -> bool {
    true
}

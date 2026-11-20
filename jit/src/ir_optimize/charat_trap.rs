// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! nio21 wave 5 (lane L5): `CharSequence.charAt` as a TRAP rather than a call
//! inside a class-predicated loop (`CRATONVM_JIT_IR_SEQ_CHARAT_TRAP`).
//!
//! # Why
//!
//! `UTF_8$Encoder.encodeBufferLoop` splices `StringCharBuffer.get()`, whose
//! `str.charAt(i)` is an `invokeinterface CharSequence`. The optimizing tier
//! answers it with the `SeqCharAt` call PREFIX
//! (`ir_lower::Lowerer::try_emit_ir_string_search_prefix`): a String receiver
//! and an in-range index are read inline, anything else DECLINES to the
//! interface call. In the IR the site therefore stays an `Op::Call`, whose
//! memory effect is `Any`, and `loop_field_forward` refuses every loop that
//! holds one -- so the buffers' `position` / `limit` / `hb` / `offset` / `str`
//! are re-read per char even after `class_predicate` has proven both buffers'
//! classes at the loop entry
//! (`r21-encode-loop-field-hoist-and-buffer-bce-need-a-call-free-loop-FIXED-20261012.md`).
//!
//! # What
//!
//! [`seq_charat_traps_on_miss`] names the `SeqCharAt` calls whose prefix
//! DEOPTS on a miss instead of calling: the call sits in the body of a loop
//! whose entry carries a `class_predicate` guard (the
//! `Guard{bci: header}(ExactClassIs(..))` that pass plants on the loop's entry
//! control). `class_predicate` plants no such guard for a loop holding a call
//! whose pc has no frame a deopt there can resume (see below), or once the
//! method has de-specialised the trap's site ([`class_predicate_withheld`]). For such a
//! call the lowering emits the prefix with every
//! decline edge landing on one deopt at the call's pc
//! (`DeoptReason::ReceiverTypeChanged`), and the field forward admits the call:
//! it then runs no Java and writes no memory, so it is a READ of the String's
//! `value` / `coder` and a possible exit, exactly like the guards the forward
//! already accepts.
//!
//! ONE predicate, asked by both sides (`loop_field_forward`'s census and the
//! lowering), so the forward can only rely on a call the lowering makes trap:
//! the store-clause pattern of `ir_lower::store_cannot_deopt`. The lowering
//! refuses the compile (a bailout, never a silent call) when it cannot emit the
//! trapping form for a call the predicate names.
//!
//! # Why it is sound
//!
//! * A miss runs nothing: the prefix's tests (null receiver, a non-String
//!   class, a `null` `value`, an index outside `[0, length)`, a layout epoch
//!   change) all precede any write, and the deopt resumes the interpreter AT the
//!   `invokeinterface` with the frame the builder recorded at the top of that pc
//!   (the chain snapshot inside a splice, whose callee frames are parked
//!   mid-`invoke`; the flat snapshot for a call of the compiling method itself).
//!   The interpreter re-runs the call, so every receiver gets the method the
//!   bytecode dispatches to, and an out-of-range index throws the JDK's own
//!   `StringIndexOutOfBoundsException`.
//! * The FLAT frame of the outermost invoke is never used for a call inside a
//!   splice: it would re-run the spliced body's committed stores (`nextGetIndex`
//!   has already advanced `position`). `class_predicate` requires a
//!   guard-capable chain snapshot at a pc any chain snapshot names, and the
//!   lowering checks that the chain resolves before it emits (and refuses the
//!   compile otherwise: a snapshot pruned after optimize, or a chain whose
//!   values cannot be published).
//! * A hit reads only `String.value` (final, its array never written after
//!   construction) and `coder`; the forward's argument (no synchronisation
//!   action, no write to a forwarded field in the loop) is unchanged.
//!
//! # Cost when the speculation is wrong
//!
//! A non-String receiver deopts at the call per miss, where the call used to
//! cost one compare. The predicate applies only inside a loop
//! `class_predicate` already speculated on (one profiled class per invariant
//! receiver), and `class_predicate` plants nothing for a loop holding such a
//! call once the VM has de-specialised the site the trap is charged at
//! ([`withheld_by_despec`]; per site since nio21 wave 6, the outermost
//! invoke of a spliced call's chain), so the next compile keeps the call.
//! The VM records that de-spec after `PER_BCI_DESPEC_LIMIT` (4) charged traps
//! at the site, on every sink that charges a trap of the body's own
//! (`deopt_resume::real_frame_deopt_resume_or_throw_and_despeculate`,
//! `helpers::despeculate_trapped_method`, and since nio21 wave 7 the doors'
//! re-run and IR-resume arms and the OSR door's rejected bail, through
//! `deopt_resume::despec_own_stash_site_after_charge`;
//! `r21-charat-trap-despec-skips-the-rerun-sinks-FIXED-20261012.md`).
//!
//! # Armed call-site exception edges
//!
//! A trapping call inside a `try` may also carry call-site exception edges
//! (`Op::CallThrew`, `CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES`). The trap
//! makes no call, so those edges' pad is never entered, and an out-of-range
//! index deopts where the edge would have caught the JDK's
//! `StringIndexOutOfBoundsException` in compiled code. That is deliberate
//! (nio21 wave 7, lane L2; `r21-ir-charat-trap-deopts-a-call-whose-exception-edges-are-armed-FIXED-20261012.md`):
//! the predicate does NOT refuse such a call, because catch-all rows arm
//! edges too (`US_ASCII` / `ISO_8859_1`'s `encodeBufferLoop` loop inside
//! `try { .. } finally`, a row that covers the `charAt` compiled or spliced);
//! refusing would cost those encode loops their forwarding the day the edges
//! default on. The decline is charged as the trap's own
//! `ReceiverTypeChanged` at the site, so a loop that keeps indexing out of
//! range pays at most `PER_BCI_DESPEC_LIMIT` deopts before
//! [`withheld_by_despec`] keeps the call (and its armed edges) on the next
//! compile.

use super::{build_users, loop_body_with, loop_headers};
use crate::ir::{CmpOp, Graph, IrType, Node, NodeId, Op, NO_NODE};
use rustc_hash::FxHashSet;

/// Returns `true` when every back-edge path from `back_ctrls` back to `region`
/// passes through `target` within a bounded control walk.
fn ctrl_reaches_from(
    graph: &Graph,
    mut cur: NodeId,
    target: NodeId,
    region: NodeId,
    depth: u8,
) -> bool {
    let mut d = depth;
    loop {
        if cur == target {
            return true;
        }
        if cur == region || d == 0 {
            return false;
        }
        d -= 1;
        let Some(n) = graph.node_opt(cur) else {
            return false;
        };
        match n.op {
            Op::Proj(_) => {
                let Some(iff) = n.input_opt(0) else {
                    return false;
                };
                cur = iff;
            }
            Op::Merge => {
                return !n.inputs.is_empty()
                    && n.inputs
                        .iter()
                        .all(|&p| ctrl_reaches_from(graph, p, target, region, d));
            }
            _ => {
                if n.op == Op::If {
                    let Some(up) = n.input_opt(0) else {
                        return false;
                    };
                    cur = up;
                } else {
                    return false;
                }
            }
        }
    }
}

fn ctrl_executes_every_iteration(
    graph: &Graph,
    target: NodeId,
    region: NodeId,
    back_ctrls: &[NodeId],
) -> bool {
    !back_ctrls.is_empty()
        && back_ctrls
            .iter()
            .all(|&bc| ctrl_reaches_from(graph, bc, target, region, 12))
}

fn guard_is_null_check_on(graph: &Graph, guard_id: NodeId, recv: NodeId) -> bool {
    let Some(gn) = graph.node_opt(guard_id) else {
        return false;
    };
    let Op::Guard { .. } = gn.op else {
        return false;
    };
    let Some(cond_id) = gn.input_opt(1) else {
        return false;
    };
    let Some(cn) = graph.node_opt(cond_id) else {
        return false;
    };
    let Op::Cmp(CmpOp::Ne) = cn.op else {
        return false;
    };
    let is_null = |id: NodeId| {
        graph
            .node_opt(id)
            .is_some_and(|z| z.op == Op::Const(0) && z.ty == IrType::Ref)
    };
    matches!(
        cn.inputs.as_slice(),
        &[a, b] if (a == recv && is_null(b)) || (b == recv && is_null(a))
    )
}

fn splice_out_hoisted_mem_read(
    graph: &mut Graph,
    old_id: NodeId,
    new_val: NodeId,
    left_frames: &mut Vec<u32>,
    replaced: &mut usize,
    spliced_old_nodes: &mut FxHashSet<NodeId>,
) {
    if !spliced_old_nodes.insert(old_id) {
        return;
    }
    let Some(in_mem) = graph.node_opt(old_id).and_then(|n| n.input_opt(1)) else {
        return;
    };
    for user in graph.users_of(old_id) {
        let Some(un) = graph.node_opt(user) else {
            continue;
        };
        let slots: Vec<usize> = un
            .inputs
            .as_slice()
            .iter()
            .enumerate()
            .filter(|&(i, &x)| x == old_id && crate::ir::is_memory_token_slot(un, i))
            .map(|(i, _)| i)
            .collect();
        for slot in slots {
            graph.set_input(user, slot, in_mem);
        }
    }
    if let Some(fs) = graph.node_opt(old_id).and_then(|n| n.frame_snapshot) {
        left_frames.push(fs);
    }
    graph.replace_all_uses(old_id, new_val);
    graph.kill(old_id);
    *replaced += 1;
}

fn attach_guard_token(graph: &mut Graph, node: NodeId, guard: NodeId) {
    if graph.node_opt(node).is_some_and(|n| n.inputs.len() >= 5) {
        let _ = graph.set_input(node, 4, guard);
    } else {
        graph.push_input(node, guard);
    }
}

fn find_preheader_array_length(
    graph: &mut Graph,
    body: &FxHashSet<NodeId>,
    arr: NodeId,
    guard: NodeId,
) -> Option<NodeId> {
    let al = graph.users_of(arr).into_iter().find(|&u| {
        !body.contains(&u)
            && graph
                .node_opt(u)
                .is_some_and(|n| n.op == Op::ArrayLength && n.input_opt(2) == Some(arr))
    })?;
    if graph
        .node_opt(al)
        .and_then(|n| n.input_opt(3))
        .is_some_and(|g| body.contains(&g))
    {
        let _ = graph.set_input(al, 3, guard);
    }
    Some(al)
}

fn loop_entry_mem_state(
    graph: &Graph,
    region: NodeId,
    entry_pred: NodeId,
    body: &FxHashSet<NodeId>,
    entry_mem: Option<NodeId>,
    fallback_mem_node: NodeId,
) -> Option<(Option<usize>, Option<NodeId>, NodeId)> {
    let entry_slot = graph.nodes[region as usize]
        .inputs
        .iter()
        .position(|&p| p == entry_pred);
    let header_mem = super::anchored_at(graph, region).into_iter().find(|&id| {
        graph
            .node_opt(id)
            .is_some_and(|n| n.op == Op::Phi && n.ty == IrType::Memory)
    });
    let current_entry_mem = header_mem
        .and_then(|hm| entry_slot.and_then(|slot| graph.node_opt(hm)?.input_opt(1 + slot)))
        .or(entry_mem);
    let initial_pre_mem = match current_entry_mem {
        Some(m) => m,
        None => {
            let mut m = fallback_mem_node;
            let mut hops = 0usize;
            while body.contains(&m) && hops < 64 {
                hops += 1;
                let Some(next_m) = graph.node_opt(m).and_then(|n| n.input_opt(1)) else {
                    break;
                };
                m = next_m;
            }
            if body.contains(&m) {
                return None;
            }
            m
        }
    };
    Some((entry_slot, header_mem, initial_pre_mem))
}

fn collect_guards_on_cmp(
    graph: &Graph,
    body: &FxHashSet<NodeId>,
    target: NodeId,
    matcher: impl Fn(&Node) -> bool,
) -> Vec<NodeId> {
    graph
        .users_of(target)
        .into_iter()
        .filter(|&u| graph.node_opt(u).is_some_and(&matcher))
        .flat_map(|cmp_id| {
            graph
                .users_of(cmp_id)
                .into_iter()
                .filter(|&u| {
                    body.contains(&u)
                        && graph
                            .node_opt(u)
                            .is_some_and(|n| matches!(n.op, Op::Guard { .. }))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Hoists loop-invariant `java/util/ArrayList` / `List.get(int)` metadata
/// (`size`, `elementData`, `elementData != null`, `ArrayLength(elementData)`)
/// and masked bounds checks (`m < size`, `m < arr_len`) into `entry_pred`.
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_optimize) fn hoist_invariant_arraylist_get(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    entry_mem: Option<NodeId>,
    header_bci: usize,
    si: u32,
    spliced: bool,
    left_frames: &mut Vec<u32>,
    replaced: &mut usize,
) -> usize {
    use crate::ir::{with_licensing_guard, CmpOp, MemKind};

    let users = build_users(graph);
    let body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);
    for &id in &body {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        if matches!(
            n.op,
            Op::Call { .. }
                | Op::IndyBridge { .. }
                | Op::MultiNewArray { .. }
                | Op::NewCp { .. }
                | Op::Store(_)
                | Op::MonitorEnter
                | Op::MonitorExit
                | Op::ArrayStore(MemKind::Ref)
        ) {
            return 0;
        }
    }

    let entry_slot = graph.nodes[region as usize]
        .inputs
        .iter()
        .position(|&p| p == entry_pred);
    let header_mem = super::anchored_at(graph, region).into_iter().find(|&id| {
        graph
            .node_opt(id)
            .is_some_and(|n| n.op == Op::Phi && n.ty == IrType::Memory)
    });

    #[derive(Clone, Copy)]
    struct Candidate {
        elem_id: NodeId,
        old_ed: NodeId,
        old_size: Option<NodeId>,
        idx: NodeId,
        recv: NodeId,
        ed_field_idx: i64,
        size_field_idx: i64,
    }

    let mut candidates = Vec::new();
    for &id in &body {
        let Some(aload) = graph.node_opt(id) else {
            continue;
        };
        if aload.op != Op::ArrayLoad(MemKind::Ref) || aload.inputs.len() < 4 {
            continue;
        }
        let (dec_ctrl, old_ed, idx) = (aload.inputs[0], aload.inputs[2], aload.inputs[3]);
        if !body.contains(&dec_ctrl)
            || !ctrl_executes_every_iteration(graph, dec_ctrl, region, back_ctrls)
        {
            continue;
        }
        let Some(edn) = graph.node_opt(old_ed) else {
            continue;
        };
        if edn.op != Op::Load(MemKind::Ref) || edn.inputs.len() < 4 {
            continue;
        }
        let recv = edn.inputs[2];
        if body.contains(&recv) {
            continue;
        }
        let Some(ed_field_idx) = edn
            .input_opt(3)
            .and_then(|x| graph.node_opt(x))
            .and_then(|x| match x.op {
                Op::Const(v) => Some(v),
                _ => None,
            })
        else {
            continue;
        };
        let mut old_size = None;
        let mut size_field_idx = 0;
        if let Some(prev) = edn.input_opt(1) {
            if body.contains(&prev) || !body.contains(&old_ed) {
                if let Some(sn) = graph.node_opt(prev) {
                    if sn.op == Op::Load(MemKind::Int)
                        && sn.inputs.len() >= 4
                        && sn.inputs[2] == recv
                    {
                        if let Some(sf_idx) = sn
                            .input_opt(3)
                            .and_then(|x| graph.node_opt(x))
                            .and_then(|x| match x.op {
                                Op::Const(v) => Some(v),
                                _ => None,
                            })
                        {
                            old_size = Some(prev);
                            size_field_idx = sf_idx;
                        }
                    }
                }
            }
        }
        candidates.push(Candidate {
            elem_id: id,
            old_ed,
            old_size,
            idx,
            recv,
            ed_field_idx,
            size_field_idx,
        });
    }
    if candidates.is_empty() {
        return 0;
    }

    let mut pre_mem_tail = match entry_mem {
        Some(m) => m,
        None => {
            let mut m = candidates[0].old_size.unwrap_or(candidates[0].old_ed);
            let mut hops = 0usize;
            while body.contains(&m) && hops < 64 {
                hops += 1;
                let Some(next_m) = graph.node_opt(m).and_then(|n| n.input_opt(1)) else {
                    break;
                };
                m = next_m;
            }
            if body.contains(&m) {
                return 0;
            }
            m
        }
    };
    let initial_pre_mem = pre_mem_tail;

    let hpc = Some(header_bci);
    let mut guards_planted = 0usize;
    let mut hoisted: Vec<(
        NodeId,
        i64,
        Option<NodeId>,
        NodeId,
        NodeId,
        Option<(i64, NodeId)>,
    )> = Vec::new();
    let mut spliced_old_nodes: FxHashSet<NodeId> = FxHashSet::default();

    for cand in candidates {
        let already = hoisted
            .iter()
            .position(|h| h.0 == cand.recv && h.1 == cand.ed_field_idx);
        let (new_size, new_ed, new_arr_len, mask_cache) = if let Some(pos) = already {
            (
                hoisted[pos].2,
                hoisted[pos].3,
                hoisted[pos].4,
                &mut hoisted[pos].5,
            )
        } else {
            let old_ed_lic = graph.node_opt(cand.old_ed).and_then(|n| n.input_opt(4));
            let Some(pre_recv_guard) = plant_preheader_recv_guard(
                graph,
                entry_pred,
                header_bci,
                si,
                spliced,
                cand.recv,
                old_ed_lic,
                &body,
                left_frames,
                replaced,
                &mut guards_planted,
            ) else {
                continue;
            };

            let in_loop_body = |nid: NodeId| {
                body.contains(&nid)
                    && graph.node_opt(nid).and_then(|n| n.input_opt(0)) != Some(entry_pred)
            };
            let ed_in_body = in_loop_body(cand.old_ed);
            let new_sz = if let Some(old_sz) = cand.old_size {
                if in_loop_body(old_sz) {
                    let sz_row = graph.field_rows.get(&old_sz).copied();
                    let sz_c = graph.add(Op::Const(cand.size_field_idx), IrType::Int, vec![], None);
                    let ns = graph.add(
                        Op::Load(MemKind::Int),
                        IrType::Int,
                        with_licensing_guard(
                            vec![entry_pred, pre_mem_tail, cand.recv, sz_c],
                            pre_recv_guard,
                        ),
                        hpc,
                    );
                    let _ = graph.set_node_frame_snapshot(ns, si);
                    if let Some(r) = sz_row {
                        graph.field_rows.insert(ns, r);
                    }
                    pre_mem_tail = ns;
                    Some((old_sz, ns, true))
                } else {
                    if graph
                        .node_opt(old_sz)
                        .and_then(|n| n.input_opt(4))
                        .is_some_and(|g| body.contains(&g))
                    {
                        let _ = graph.set_input(old_sz, 4, pre_recv_guard);
                    }
                    Some((old_sz, old_sz, false))
                }
            } else {
                None
            };

            let ned = if ed_in_body {
                let ed_row = graph.field_rows.get(&cand.old_ed).copied();
                let ed_c = graph.add(Op::Const(cand.ed_field_idx), IrType::Int, vec![], None);
                let ned = graph.add(
                    Op::Load(MemKind::Ref),
                    IrType::Ref,
                    with_licensing_guard(
                        vec![entry_pred, pre_mem_tail, cand.recv, ed_c],
                        pre_recv_guard,
                    ),
                    hpc,
                );
                let _ = graph.set_node_frame_snapshot(ned, si);
                if let Some(r) = ed_row {
                    graph.field_rows.insert(ned, r);
                }
                graph.synthetic_field_access_pcs.insert(header_bci);
                ned
            } else {
                if graph
                    .node_opt(cand.old_ed)
                    .and_then(|n| n.input_opt(4))
                    .is_some_and(|g| body.contains(&g))
                {
                    let _ = graph.set_input(cand.old_ed, 4, pre_recv_guard);
                }
                cand.old_ed
            };

            let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
            let ed_ne = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![ned, null], None);
            let nedg = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, ed_ne],
                hpc,
            );
            let _ = graph.set_node_frame_snapshot(nedg, si);
            if spliced {
                graph
                    .chain_predicate_guards
                    .push((nedg, crate::deopt::PredicateKind::Length as u8));
            }
            guards_planted += 1;

            let nalen = if !ed_in_body {
                find_preheader_array_length(graph, &body, ned, nedg)
            } else {
                None
            }
            .unwrap_or_else(|| {
                let nalen_mem = if ed_in_body { ned } else { pre_mem_tail };
                let al = graph.add(
                    Op::ArrayLength,
                    IrType::Int,
                    with_licensing_guard(vec![entry_pred, nalen_mem, ned], nedg),
                    hpc,
                );
                let _ = graph.set_node_frame_snapshot(al, si);
                pre_mem_tail = al;
                al
            });

            let old_ed_guards: Vec<NodeId> = graph
                .users_of(cand.old_ed)
                .into_iter()
                .filter(|&u| {
                    body.contains(&u)
                        && graph
                            .node_opt(u)
                            .is_some_and(|n| matches!(n.op, Op::Cmp(CmpOp::Ne)))
                })
                .flat_map(|cmp_id| {
                    graph
                        .users_of(cmp_id)
                        .into_iter()
                        .filter(|&u| {
                            body.contains(&u)
                                && graph
                                    .node_opt(u)
                                    .is_some_and(|n| matches!(n.op, Op::Guard { .. }))
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            let old_als: Vec<NodeId> = graph
                .users_of(cand.old_ed)
                .into_iter()
                .filter(|&u| {
                    body.contains(&u) && graph.node_opt(u).is_some_and(|n| n.op == Op::ArrayLength)
                })
                .collect();

            if let Some((old_sz, ns, true)) = new_sz {
                splice_out_hoisted_mem_read(
                    graph,
                    old_sz,
                    ns,
                    left_frames,
                    replaced,
                    &mut spliced_old_nodes,
                );
            }
            if ed_in_body {
                splice_out_hoisted_mem_read(
                    graph,
                    cand.old_ed,
                    ned,
                    left_frames,
                    replaced,
                    &mut spliced_old_nodes,
                );
            }
            for g in old_ed_guards {
                if let Some(fs) = graph.node_opt(g).and_then(|n| n.frame_snapshot) {
                    left_frames.push(fs);
                }
                graph.replace_all_uses(g, nedg);
                graph.kill(g);
                *replaced += 1;
            }
            for al in old_als {
                splice_out_hoisted_mem_read(
                    graph,
                    al,
                    nalen,
                    left_frames,
                    replaced,
                    &mut spliced_old_nodes,
                );
            }
            *replaced += 1;
            let ns_opt = new_sz.map(|(_, ns, _)| ns);
            hoisted.push((cand.recv, cand.ed_field_idx, ns_opt, ned, nalen, None));
            let pos = hoisted.len() - 1;
            (ns_opt, ned, nalen, &mut hoisted[pos].5)
        };

        if let Some(m) = crate::ir_check_elim::const_mask_of(graph, cand.idx) {
            if (0..=i64::from(i32::MAX)).contains(&m) {
                let mask_guard = match *mask_cache {
                    Some((cached_m, mg)) if cached_m >= m => mg,
                    _ => {
                        if let Some(ns) = new_size {
                            let mk_sz = graph.add(Op::Const(m), IrType::Int, vec![], None);
                            let lt_sz =
                                graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![mk_sz, ns], None);
                            let sg = graph.add(
                                Op::Guard { bci: header_bci },
                                IrType::Void,
                                vec![entry_pred, lt_sz],
                                hpc,
                            );
                            let _ = graph.set_node_frame_snapshot(sg, si);
                            if spliced {
                                graph
                                    .chain_predicate_guards
                                    .push((sg, crate::deopt::PredicateKind::Length as u8));
                            }
                            guards_planted += 1;
                        }
                        let mk = graph.add(Op::Const(m), IrType::Int, vec![], None);
                        let lt_len =
                            graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![mk, new_arr_len], None);
                        let mg = graph.add(
                            Op::Guard { bci: header_bci },
                            IrType::Void,
                            vec![entry_pred, lt_len],
                            hpc,
                        );
                        let _ = graph.set_node_frame_snapshot(mg, si);
                        if spliced {
                            graph
                                .chain_predicate_guards
                                .push((mg, crate::deopt::PredicateKind::Length as u8));
                        }
                        guards_planted += 1;
                        *mask_cache = Some((m, mg));
                        mg
                    }
                };
                if header_mem.is_none()
                    && graph.node_opt(cand.elem_id).and_then(|n| n.input_opt(1))
                        == Some(initial_pre_mem)
                {
                    graph.set_input(cand.elem_id, 1, pre_mem_tail);
                }
                let mut kill_guards: Vec<NodeId> = Vec::new();
                if let Some(tok) = graph.node_opt(cand.elem_id).and_then(|n| n.input_opt(4)) {
                    if body.contains(&tok)
                        && graph
                            .node_opt(tok)
                            .is_some_and(|n| matches!(n.op, Op::Guard { .. }))
                    {
                        kill_guards.push(tok);
                    }
                }
                for &u in &users[cand.idx as usize] {
                    if !body.contains(&u) {
                        continue;
                    }
                    let Some(un) = graph.node_opt(u) else {
                        continue;
                    };
                    let is_idx_cmp = match un.op {
                        Op::Cmp(CmpOp::Ge) => {
                            un.input_opt(0) == Some(cand.idx)
                                && un
                                    .input_opt(1)
                                    .and_then(|k| graph.node_opt(k))
                                    .is_some_and(|kn| kn.op == Op::Const(0))
                        }
                        Op::Cmp(CmpOp::Lt) => {
                            un.input_opt(0) == Some(cand.idx)
                                && un.input_opt(1).is_some_and(|b| {
                                    b == new_arr_len
                                        || new_size == Some(b)
                                        || graph.node_opt(b).is_some_and(|bn| {
                                            bn.op == Op::ArrayLength
                                                && matches!(
                                                    bn.input_opt(2),
                                                    Some(arr) if arr == new_ed || arr == cand.old_ed
                                                )
                                        })
                                })
                        }
                        _ => false,
                    };
                    if !is_idx_cmp {
                        continue;
                    }
                    for &gu in &users[u as usize] {
                        if body.contains(&gu)
                            && graph
                                .node_opt(gu)
                                .is_some_and(|gn| matches!(gn.op, Op::Guard { .. }))
                        {
                            kill_guards.push(gu);
                        }
                    }
                }
                attach_guard_token(graph, cand.elem_id, mask_guard);
                for kg in kill_guards {
                    if let Some(fs) = graph.node_opt(kg).and_then(|n| n.frame_snapshot) {
                        left_frames.push(fs);
                    }
                    graph.replace_all_uses(kg, mask_guard);
                    graph.kill(kg);
                    *replaced += 1;
                }
            }
        }
        let _ = new_ed;
    }

    if let (Some(hm), Some(slot)) = (header_mem, entry_slot) {
        if pre_mem_tail != initial_pre_mem {
            graph.set_input(hm, 1 + slot, pre_mem_tail);
        }
    }
    guards_planted
}

/// Hoists loop-invariant `java/lang/String.charAt` metadata (`coder`, `value`,
/// `value != null`, `ArrayLength(value)`) out of a single-entry loop into
/// `entry_pred`, and when the index is masked (`i & m`), plants `coder == 0`
/// and `m < byte_len` pre-header guards so the in-loop `Op::StrCharDecode`
/// lowers to a bounds-proven `ArrayLoad(MemKind::Byte) & 0xFF`.
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_optimize) fn hoist_invariant_string_charat(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    entry_mem: Option<NodeId>,
    header_bci: usize,
    si: u32,
    spliced: bool,
    left_frames: &mut Vec<u32>,
    replaced: &mut usize,
) -> usize {
    use crate::ir::{with_licensing_guard, CmpOp, MemKind};

    let users = build_users(graph);
    let body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);
    let mut guards_planted = hoist_invariant_string_builder(
        graph,
        region,
        entry_pred,
        back_ctrls,
        entry_mem,
        header_bci,
        si,
        spliced,
        left_frames,
        replaced,
        &body,
    );
    // Conservative alias screen: bail if the loop may mutate objects, bytes, or call out.
    let mut has_unsafe_write = false;
    for &id in &body {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        if matches!(
            n.op,
            Op::Call { .. }
                | Op::IndyBridge { .. }
                | Op::MultiNewArray { .. }
                | Op::NewCp { .. }
                | Op::Store(_)
                | Op::MonitorEnter
                | Op::MonitorExit
                | Op::ArrayStore(MemKind::Byte)
        ) {
            has_unsafe_write = true;
            break;
        }
    }
    if has_unsafe_write {
        return guards_planted;
    }
    guards_planted += hoist_invariant_hashmap_get(
        graph,
        region,
        entry_pred,
        back_ctrls,
        entry_mem,
        header_bci,
        si,
        spliced,
        left_frames,
        replaced,
        &body,
    );

    let entry_slot = graph.nodes[region as usize]
        .inputs
        .iter()
        .position(|&p| p == entry_pred);
    let header_mem = super::anchored_at(graph, region).into_iter().find(|&id| {
        graph
            .node_opt(id)
            .is_some_and(|n| n.op == Op::Phi && n.ty == IrType::Memory)
    });

    #[derive(Clone, Copy)]
    struct Candidate {
        decode_id: NodeId,
        dec_ctrl: NodeId,
        old_value: NodeId,
        idx: NodeId,
        old_coder: NodeId,
        coder_kind: MemKind,
        recv: NodeId,
        coder_field_idx: i64,
        value_field_idx: i64,
    }

    let mut candidates = Vec::new();
    for &id in &body {
        let Some(dec) = graph.node_opt(id) else {
            continue;
        };
        if dec.op != Op::StrCharDecode || dec.inputs.len() < 5 {
            continue;
        }
        let (dec_ctrl, old_value, idx, old_coder) =
            (dec.inputs[0], dec.inputs[2], dec.inputs[3], dec.inputs[4]);
        if !body.contains(&dec_ctrl)
            || !ctrl_executes_every_iteration(graph, dec_ctrl, region, back_ctrls)
        {
            continue;
        }
        let (Some(vn), Some(cn)) = (graph.node_opt(old_value), graph.node_opt(old_coder)) else {
            continue;
        };
        let (&Op::Load(MemKind::Ref), &Op::Load(coder_kind)) = (&vn.op, &cn.op) else {
            continue;
        };
        if !matches!(coder_kind, MemKind::Int | MemKind::Byte)
            || vn.inputs.len() < 4
            || cn.inputs.len() < 4
        {
            continue;
        }
        let recv = vn.inputs[2];
        if cn.inputs[2] != recv || body.contains(&recv) {
            continue;
        }
        let (Some(v_idx_n), Some(c_idx_n)) =
            (graph.node_opt(vn.inputs[3]), graph.node_opt(cn.inputs[3]))
        else {
            continue;
        };
        let (&Op::Const(value_field_idx), &Op::Const(coder_field_idx)) = (&v_idx_n.op, &c_idx_n.op)
        else {
            continue;
        };
        candidates.push(Candidate {
            decode_id: id,
            dec_ctrl,
            old_value,
            idx,
            old_coder,
            coder_kind,
            recv,
            coder_field_idx,
            value_field_idx,
        });
    }
    if candidates.is_empty() {
        return guards_planted;
    }

    let Some((_es, _hm, initial_pre_mem_val)) = loop_entry_mem_state(
        graph,
        region,
        entry_pred,
        &body,
        entry_mem,
        candidates[0].old_coder,
    ) else {
        return guards_planted;
    };
    let mut pre_mem_tail = initial_pre_mem_val;
    let initial_pre_mem = pre_mem_tail;

    let hpc = Some(header_bci);
    let mut hoisted: Vec<(
        NodeId,
        i64,
        i64,
        NodeId,
        NodeId,
        NodeId,
        NodeId,
        Option<(NodeId, NodeId)>,
    )> = Vec::new();
    let mut spliced_old_nodes: FxHashSet<NodeId> = FxHashSet::default();

    for cand in candidates {
        let already = hoisted.iter().position(|h| {
            h.0 == cand.recv && h.1 == cand.coder_field_idx && h.2 == cand.value_field_idx
        });
        let (new_coder, new_value, new_value_guard, new_byte_len, latin1_info) = if let Some(pos) =
            already
        {
            (
                hoisted[pos].3,
                hoisted[pos].4,
                hoisted[pos].5,
                hoisted[pos].6,
                &mut hoisted[pos].7,
            )
        } else {
            let coder_in_body = body.contains(&cand.old_coder);
            let value_in_body = body.contains(&cand.old_value);
            let old_coder_lic = graph.node_opt(cand.old_coder).and_then(|n| n.input_opt(4));
            let old_value_lic = graph.node_opt(cand.old_value).and_then(|n| n.input_opt(4));
            let existing_recv_guard = old_coder_lic.or(old_value_lic).filter(|&g| {
                graph.node_opt(g).is_some_and(|gn| {
                    matches!(gn.op, Op::Guard { .. })
                        && (gn.input_opt(0) == Some(entry_pred) || !body.contains(&g))
                })
            });
            let lic_superseded = old_coder_lic
                .is_some_and(|g| body.contains(&g) && guard_is_null_check_on(graph, g, cand.recv));
            if old_coder_lic.is_some() && existing_recv_guard.is_none() && !lic_superseded {
                continue;
            }
            let pre_recv_guard = if let Some(g) = existing_recv_guard {
                g
            } else {
                let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
                let cond = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![cand.recv, null], None);
                let g = graph.add(
                    Op::Guard { bci: header_bci },
                    IrType::Void,
                    vec![entry_pred, cond],
                    hpc,
                );
                if !graph.set_node_frame_snapshot(g, si) {
                    graph.kill(g);
                    graph.kill(cond);
                    graph.kill(null);
                    continue;
                }
                if spliced {
                    graph
                        .chain_predicate_guards
                        .push((g, crate::deopt::PredicateKind::Length as u8));
                }
                guards_planted += 1;
                g
            };

            let coder_row = graph.field_rows.get(&cand.old_coder).copied();
            let value_row = graph.field_rows.get(&cand.old_value).copied();
            let nc = if coder_in_body {
                let c_const = graph.add(Op::Const(cand.coder_field_idx), IrType::Int, vec![], None);
                let nc = graph.add(
                    Op::Load(cand.coder_kind),
                    IrType::Int,
                    with_licensing_guard(
                        vec![entry_pred, pre_mem_tail, cand.recv, c_const],
                        pre_recv_guard,
                    ),
                    hpc,
                );
                let _ = graph.set_node_frame_snapshot(nc, si);
                if let Some(r) = coder_row {
                    graph.field_rows.insert(nc, r);
                }
                nc
            } else {
                cand.old_coder
            };
            let nv = if value_in_body {
                let v_const = graph.add(Op::Const(cand.value_field_idx), IrType::Int, vec![], None);
                let nv_mem = if coder_in_body { nc } else { pre_mem_tail };
                let nv = graph.add(
                    Op::Load(MemKind::Ref),
                    IrType::Ref,
                    with_licensing_guard(
                        vec![entry_pred, nv_mem, cand.recv, v_const],
                        pre_recv_guard,
                    ),
                    hpc,
                );
                let _ = graph.set_node_frame_snapshot(nv, si);
                if let Some(r) = value_row {
                    graph.field_rows.insert(nv, r);
                }
                nv
            } else {
                cand.old_value
            };
            if coder_in_body || value_in_body {
                graph.synthetic_field_access_pcs.insert(header_bci);
            }

            let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
            let val_ne = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![nv, null], None);
            let nvg = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, val_ne],
                hpc,
            );
            let _ = graph.set_node_frame_snapshot(nvg, si);
            if spliced {
                graph
                    .chain_predicate_guards
                    .push((nvg, crate::deopt::PredicateKind::Length as u8));
            }
            guards_planted += 1;

            let nbl = if !value_in_body {
                find_preheader_array_length(graph, &body, nv, nvg)
            } else {
                None
            }
            .unwrap_or_else(|| {
                let nbl_mem = if value_in_body { nv } else { pre_mem_tail };
                let al = graph.add(
                    Op::ArrayLength,
                    IrType::Int,
                    with_licensing_guard(vec![entry_pred, nbl_mem, nv], nvg),
                    hpc,
                );
                let _ = graph.set_node_frame_snapshot(al, si);
                pre_mem_tail = al;
                al
            });

            let old_val_guards: Vec<NodeId> = graph
                .users_of(cand.old_value)
                .into_iter()
                .filter(|&u| {
                    body.contains(&u)
                        && graph
                            .node_opt(u)
                            .is_some_and(|n| matches!(n.op, Op::Cmp(CmpOp::Ne)))
                })
                .flat_map(|cmp_id| {
                    graph
                        .users_of(cmp_id)
                        .into_iter()
                        .filter(|&u| {
                            body.contains(&u)
                                && graph
                                    .node_opt(u)
                                    .is_some_and(|n| matches!(n.op, Op::Guard { .. }))
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            let old_als: Vec<NodeId> = graph
                .users_of(cand.old_value)
                .into_iter()
                .filter(|&u| {
                    body.contains(&u) && graph.node_opt(u).is_some_and(|n| n.op == Op::ArrayLength)
                })
                .collect();
            if coder_in_body {
                splice_out_hoisted_mem_read(
                    graph,
                    cand.old_coder,
                    nc,
                    left_frames,
                    replaced,
                    &mut spliced_old_nodes,
                );
            }
            if value_in_body {
                splice_out_hoisted_mem_read(
                    graph,
                    cand.old_value,
                    nv,
                    left_frames,
                    replaced,
                    &mut spliced_old_nodes,
                );
            }
            for g in old_val_guards {
                if let Some(fs) = graph.node_opt(g).and_then(|n| n.frame_snapshot) {
                    left_frames.push(fs);
                }
                graph.replace_all_uses(g, nvg);
                graph.kill(g);
                *replaced += 1;
            }
            for al in old_als {
                splice_out_hoisted_mem_read(
                    graph,
                    al,
                    nbl,
                    left_frames,
                    replaced,
                    &mut spliced_old_nodes,
                );
            }
            *replaced += 1;
            hoisted.push((
                cand.recv,
                cand.coder_field_idx,
                cand.value_field_idx,
                nc,
                nv,
                nvg,
                nbl,
                None,
            ));
            let pos = hoisted.len() - 1;
            (nc, nv, nvg, nbl, &mut hoisted[pos].7)
        };

        let _ = new_value_guard;
        if let Some(m) = crate::ir_check_elim::const_mask_of(graph, cand.idx) {
            if (0..=i64::from(i32::MAX)).contains(&m) {
                let (_latin1_guard, mask_guard) = match *latin1_info {
                    Some((lg, mg)) => (lg, mg),
                    None => {
                        let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
                        let eq0 =
                            graph.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![new_coder, zero], None);
                        let lg = graph.add(
                            Op::Guard { bci: header_bci },
                            IrType::Void,
                            vec![entry_pred, eq0],
                            hpc,
                        );
                        let _ = graph.set_node_frame_snapshot(lg, si);
                        let mk = graph.add(Op::Const(m), IrType::Int, vec![], None);
                        let lt_len = graph.add(
                            Op::Cmp(CmpOp::Lt),
                            IrType::Int,
                            vec![mk, new_byte_len],
                            None,
                        );
                        let mg = graph.add(
                            Op::Guard { bci: header_bci },
                            IrType::Void,
                            vec![entry_pred, lt_len],
                            hpc,
                        );
                        let _ = graph.set_node_frame_snapshot(mg, si);
                        if spliced {
                            let kind = crate::deopt::PredicateKind::Length as u8;
                            graph.chain_predicate_guards.push((lg, kind));
                            graph.chain_predicate_guards.push((mg, kind));
                        }
                        guards_planted += 2;
                        *latin1_info = Some((lg, mg));
                        (lg, mg)
                    }
                };
                let Some((cur_dec_mem, dec_tok, dec_bci, dec_fs)) =
                    graph.node_opt(cand.decode_id).map(|n| {
                        (
                            n.inputs[1],
                            n.inputs.get(5).copied(),
                            n.bytecode_pc,
                            n.frame_snapshot,
                        )
                    })
                else {
                    continue;
                };
                let dec_mem_use = if header_mem.is_none() && cur_dec_mem == initial_pre_mem {
                    pre_mem_tail
                } else {
                    cur_dec_mem
                };
                let mut kill_guards: Vec<NodeId> = Vec::new();
                if let Some(tok) = dec_tok {
                    if body.contains(&tok)
                        && graph
                            .node_opt(tok)
                            .is_some_and(|n| matches!(n.op, Op::Guard { .. }))
                    {
                        kill_guards.push(tok);
                    }
                }
                for &u in &users[cand.idx as usize] {
                    if !body.contains(&u) {
                        continue;
                    }
                    let Some(un) = graph.node_opt(u) else {
                        continue;
                    };
                    let is_idx_cmp = match un.op {
                        Op::Cmp(CmpOp::Ge) => {
                            un.input_opt(0) == Some(cand.idx)
                                && un
                                    .input_opt(1)
                                    .and_then(|k| graph.node_opt(k))
                                    .is_some_and(|kn| kn.op == Op::Const(0))
                        }
                        Op::Cmp(CmpOp::Lt) => {
                            let is_val_len = |id: NodeId| {
                                id == new_byte_len
                                    || graph.node_opt(id).is_some_and(|n| {
                                        n.op == Op::ArrayLength
                                            && matches!(
                                                n.input_opt(2),
                                                Some(v) if v == new_value || v == cand.old_value
                                            )
                                    })
                            };
                            un.input_opt(0) == Some(cand.idx)
                                && un.input_opt(1).is_some_and(|bound| {
                                    is_val_len(bound) || graph.node_opt(bound).is_some_and(|bn| {
                                        matches!(bn.op, Op::UShr | Op::Shr)
                                            && bn.input_opt(0).is_some_and(is_val_len)
                                            && matches!(
                                                bn.input_opt(1),
                                                Some(sh) if sh == new_coder || sh == cand.old_coder
                                            )
                                    })
                                })
                        }
                        _ => false,
                    };
                    if !is_idx_cmp {
                        continue;
                    }
                    for &gu in &users[u as usize] {
                        if body.contains(&gu)
                            && graph
                                .node_opt(gu)
                                .is_some_and(|gn| matches!(gn.op, Op::Guard { .. }))
                        {
                            kill_guards.push(gu);
                        }
                    }
                }
                let raw = graph.add(
                    Op::ArrayLoad(MemKind::Byte),
                    IrType::Int,
                    with_licensing_guard(
                        vec![cand.dec_ctrl, dec_mem_use, new_value, cand.idx],
                        mask_guard,
                    ),
                    dec_bci,
                );
                if let Some(d_si) = dec_fs {
                    let _ = graph.set_node_frame_snapshot(raw, d_si);
                }
                let mask_ff = graph.add(Op::Const(0xFF), IrType::Int, vec![], None);
                let ch = graph.add(Op::And, IrType::Int, vec![raw, mask_ff], None);
                rewire_charat_call(graph, cand.decode_id, raw, ch);
                for kg in kill_guards {
                    if let Some(fs) = graph.node_opt(kg).and_then(|n| n.frame_snapshot) {
                        left_frames.push(fs);
                    }
                    graph.replace_all_uses(kg, mask_guard);
                    graph.kill(kg);
                    *replaced += 1;
                }
                *replaced += 1;
            }
        }
    }

    if let (Some(hm), Some(slot)) = (header_mem, entry_slot) {
        if pre_mem_tail != initial_pre_mem {
            graph.set_input(hm, 1 + slot, pre_mem_tail);
        }
    }
    guards_planted
}

fn guard_exact_class_on(graph: &Graph, guard_id: NodeId, recv: NodeId) -> Option<u32> {
    let gn = graph.node_opt(guard_id)?;
    let Op::Guard { .. } = gn.op else {
        return None;
    };
    let cond = graph.node_opt(gn.input_opt(1)?)?;
    match cond.op {
        Op::ExactClassIs { class_id } if cond.input_opt(0) == Some(recv) => Some(class_id),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn plant_preheader_recv_guard(
    graph: &mut Graph,
    entry_pred: NodeId,
    header_bci: usize,
    si: u32,
    spliced: bool,
    recv: NodeId,
    old_lic: Option<NodeId>,
    body: &FxHashSet<NodeId>,
    left_frames: &mut Vec<u32>,
    replaced: &mut usize,
    guards_planted: &mut usize,
) -> Option<NodeId> {
    use crate::ir::CmpOp;
    let existing = old_lic.filter(|&g| {
        graph.node_opt(g).is_some_and(|gn| {
            matches!(gn.op, Op::Guard { .. })
                && (gn.input_opt(0) == Some(entry_pred) || !body.contains(&g))
        })
    });
    if let Some(g) = existing {
        return Some(g);
    }
    let is_null_lic =
        old_lic.is_some_and(|g| body.contains(&g) && guard_is_null_check_on(graph, g, recv));
    let exact_cid = old_lic.and_then(|g| {
        body.contains(&g)
            .then(|| guard_exact_class_on(graph, g, recv))
            .flatten()
    });
    if old_lic.is_some() && !is_null_lic && exact_cid.is_none() {
        return None;
    }

    let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
    let cond = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![recv, null], None);
    let null_guard = graph.add(
        Op::Guard { bci: header_bci },
        IrType::Void,
        vec![entry_pred, cond],
        Some(header_bci),
    );
    if !graph.set_node_frame_snapshot(null_guard, si) {
        graph.kill(null_guard);
        graph.kill(cond);
        graph.kill(null);
        return None;
    }
    if spliced {
        graph
            .chain_predicate_guards
            .push((null_guard, crate::deopt::PredicateKind::Length as u8));
    }
    *guards_planted += 1;

    let license = if let Some(class_id) = exact_cid {
        let exact = graph.add(Op::ExactClassIs { class_id }, IrType::Int, vec![recv], None);
        let eg = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, exact],
            Some(header_bci),
        );
        if !graph.set_node_frame_snapshot(eg, si) {
            graph.kill(eg);
            graph.kill(exact);
            return Some(null_guard);
        }
        if spliced {
            graph
                .chain_predicate_guards
                .push((eg, crate::deopt::PredicateKind::Length as u8));
        }
        *guards_planted += 1;
        let old_exact_guards = collect_guards_on_cmp(
            graph,
            body,
            recv,
            |n| matches!(n.op, Op::ExactClassIs { class_id: cid } if cid == class_id),
        );
        for og in old_exact_guards {
            if let Some(fs) = graph.node_opt(og).and_then(|n| n.frame_snapshot) {
                left_frames.push(fs);
            }
            graph.replace_all_uses(og, eg);
            graph.kill(og);
            *replaced += 1;
        }
        eg
    } else {
        null_guard
    };

    let old_null_guards = collect_guards_on_cmp(graph, body, recv, |n| {
        matches!(n.op, Op::Cmp(CmpOp::Ne))
            && n.inputs.iter().any(|&x| {
                graph
                    .node_opt(x)
                    .is_some_and(|z| z.op == Op::Const(0) && z.ty == IrType::Ref)
            })
    });
    for og in old_null_guards {
        if let Some(fs) = graph.node_opt(og).and_then(|n| n.frame_snapshot) {
            left_frames.push(fs);
        }
        graph.replace_all_uses(og, null_guard);
        graph.kill(og);
        *replaced += 1;
    }
    Some(license)
}

fn is_const_cmp_eq(graph: &Graph, n: &Node, target: NodeId, cval: i64) -> bool {
    n.op == crate::ir::CmpOp::Eq.into_op()
        && matches!(
            n.inputs.as_slice(),
            &[a, b] if (a == target && graph.node_opt(b).is_some_and(|k| k.op == Op::Const(cval)))
                || (b == target && graph.node_opt(a).is_some_and(|k| k.op == Op::Const(cval)))
        )
}

trait CmpOpExt {
    fn into_op(self) -> Op;
}

impl CmpOpExt for crate::ir::CmpOp {
    fn into_op(self) -> Op {
        Op::Cmp(self)
    }
}

/// Hoists loop-invariant `StringBuilder` metadata (`coder`, `coder == 0`,
/// `value`, `value != null`, `ArrayLength(value)`) when the loop only writes
/// `StringBuilder.count` (`Op::Store(MemKind::Int)`) and `value` bytes
/// (`Op::ArrayStore(MemKind::Byte)`).
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_optimize) fn hoist_invariant_string_builder(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    entry_mem: Option<NodeId>,
    header_bci: usize,
    si: u32,
    spliced: bool,
    left_frames: &mut Vec<u32>,
    replaced: &mut usize,
    body: &FxHashSet<NodeId>,
) -> usize {
    use crate::ir::{with_licensing_guard, CmpOp, MemKind};

    let mut written_fields: Vec<(NodeId, i64)> = Vec::new();
    for &id in body {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        match n.op {
            Op::Call { .. }
            | Op::IndyBridge { .. }
            | Op::MultiNewArray { .. }
            | Op::NewCp { .. }
            | Op::New { .. }
            | Op::NewArray { .. }
            | Op::StoreStatic { .. }
            | Op::MonitorEnter
            | Op::MonitorExit
            | Op::ByteArrayCopy => return 0,
            Op::Store(MemKind::Int) => {
                let (Some(recv), Some(fidx)) = (
                    n.input_opt(2).filter(|r| !body.contains(r)),
                    n.input_opt(3)
                        .and_then(|x| graph.node_opt(x))
                        .and_then(|x| match x.op {
                            Op::Const(v) => Some(v),
                            _ => None,
                        }),
                ) else {
                    return 0;
                };
                if !written_fields.contains(&(recv, fidx)) {
                    written_fields.push((recv, fidx));
                }
            }
            Op::Store(_) => return 0,
            Op::ArrayStore(MemKind::Byte) => {}
            Op::ArrayStore(_) => return 0,
            _ => {}
        }
    }
    if written_fields.is_empty() {
        return 0;
    }

    #[derive(Clone, Copy)]
    struct Cand {
        recv: NodeId,
        old_coder: NodeId,
        coder_kind: MemKind,
        coder_field_idx: i64,
        old_value: NodeId,
        value_field_idx: i64,
    }

    let mut candidates: Vec<Cand> = Vec::new();
    for &id in body {
        let Some(st) = graph.node_opt(id) else {
            continue;
        };
        if st.op != Op::ArrayStore(MemKind::Byte) || st.inputs.len() < 5 {
            continue;
        }
        let (st_ctrl, old_value) = (st.inputs[0], st.inputs[2]);
        if !body.contains(&st_ctrl)
            || !ctrl_executes_every_iteration(graph, st_ctrl, region, back_ctrls)
        {
            continue;
        }
        let Some(vn) = graph.node_opt(old_value) else {
            continue;
        };
        if vn.op != Op::Load(MemKind::Ref) || vn.inputs.len() < 4 {
            continue;
        }
        let recv = vn.inputs[2];
        if body.contains(&recv) {
            continue;
        }
        let Some(Op::Const(value_field_idx)) = vn
            .input_opt(3)
            .and_then(|x| graph.node_opt(x))
            .map(|x| &x.op)
        else {
            continue;
        };
        let value_field_idx = *value_field_idx;
        if written_fields.contains(&(recv, value_field_idx)) {
            continue;
        }

        // Find matching `coder` load on `recv` with an in-body `coder == 0` guard.
        let mut found_coder = None;
        for &cid in body {
            let Some(cn) = graph.node_opt(cid) else {
                continue;
            };
            let Op::Load(coder_kind) = cn.op else {
                continue;
            };
            if !matches!(coder_kind, MemKind::Byte | MemKind::Int)
                || cn.inputs.len() < 4
                || cn.inputs[2] != recv
            {
                continue;
            }
            let Some(Op::Const(coder_fidx)) = cn
                .input_opt(3)
                .and_then(|x| graph.node_opt(x))
                .map(|x| &x.op)
            else {
                continue;
            };
            let coder_fidx = *coder_fidx;
            if coder_fidx == value_field_idx || written_fields.contains(&(recv, coder_fidx)) {
                continue;
            }
            if !collect_guards_on_cmp(graph, body, cid, |n| is_const_cmp_eq(graph, n, cid, 0))
                .is_empty()
            {
                found_coder = Some((cid, coder_kind, coder_fidx));
                break;
            }
        }
        let Some((old_coder, coder_kind, coder_field_idx)) = found_coder else {
            continue;
        };
        candidates.push(Cand {
            recv,
            old_coder,
            coder_kind,
            coder_field_idx,
            old_value,
            value_field_idx,
        });
    }
    if candidates.is_empty() {
        return 0;
    }

    let Some((entry_slot, header_mem, initial_pre_mem)) = loop_entry_mem_state(
        graph,
        region,
        entry_pred,
        body,
        entry_mem,
        candidates[0].old_coder,
    ) else {
        return 0;
    };
    let mut pre_mem_tail = initial_pre_mem;
    let hpc = Some(header_bci);
    let mut guards_planted = 0usize;
    let mut seen_recvs: Vec<(NodeId, i64, i64)> = Vec::new();
    let mut spliced_old_nodes: FxHashSet<NodeId> = FxHashSet::default();

    for cand in candidates {
        if seen_recvs.contains(&(cand.recv, cand.coder_field_idx, cand.value_field_idx)) {
            continue;
        }
        let old_lic = graph
            .node_opt(cand.old_coder)
            .and_then(|n| n.input_opt(4))
            .or_else(|| graph.node_opt(cand.old_value).and_then(|n| n.input_opt(4)));
        let Some(pre_recv_guard) = plant_preheader_recv_guard(
            graph,
            entry_pred,
            header_bci,
            si,
            spliced,
            cand.recv,
            old_lic,
            body,
            left_frames,
            replaced,
            &mut guards_planted,
        ) else {
            continue;
        };
        seen_recvs.push((cand.recv, cand.coder_field_idx, cand.value_field_idx));

        let in_loop_body = |nid: NodeId| {
            body.contains(&nid)
                && graph.node_opt(nid).and_then(|n| n.input_opt(0)) != Some(entry_pred)
        };
        let coder_in_body = in_loop_body(cand.old_coder);
        let value_in_body = in_loop_body(cand.old_value);
        let coder_row = graph.field_rows.get(&cand.old_coder).copied();
        let value_row = graph.field_rows.get(&cand.old_value).copied();

        let nc = if coder_in_body {
            let c_const = graph.add(Op::Const(cand.coder_field_idx), IrType::Int, vec![], None);
            let nc = graph.add(
                Op::Load(cand.coder_kind),
                IrType::Int,
                with_licensing_guard(
                    vec![entry_pred, pre_mem_tail, cand.recv, c_const],
                    pre_recv_guard,
                ),
                hpc,
            );
            let _ = graph.set_node_frame_snapshot(nc, si);
            if let Some(r) = coder_row {
                graph.field_rows.insert(nc, r);
            }
            pre_mem_tail = nc;
            nc
        } else {
            cand.old_coder
        };

        let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
        let eq0 = graph.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![nc, zero], None);
        let lg = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, eq0],
            hpc,
        );
        let _ = graph.set_node_frame_snapshot(lg, si);
        if spliced {
            graph
                .chain_predicate_guards
                .push((lg, crate::deopt::PredicateKind::Length as u8));
        }
        guards_planted += 1;

        let nv = if value_in_body {
            let v_const = graph.add(Op::Const(cand.value_field_idx), IrType::Int, vec![], None);
            let nv = graph.add(
                Op::Load(MemKind::Ref),
                IrType::Ref,
                with_licensing_guard(
                    vec![entry_pred, pre_mem_tail, cand.recv, v_const],
                    pre_recv_guard,
                ),
                hpc,
            );
            let _ = graph.set_node_frame_snapshot(nv, si);
            if let Some(r) = value_row {
                graph.field_rows.insert(nv, r);
            }
            pre_mem_tail = nv;
            nv
        } else {
            cand.old_value
        };
        if coder_in_body || value_in_body {
            graph.synthetic_field_access_pcs.insert(header_bci);
        }

        let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
        let val_ne = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![nv, null], None);
        let nvg = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, val_ne],
            hpc,
        );
        let _ = graph.set_node_frame_snapshot(nvg, si);
        if spliced {
            graph
                .chain_predicate_guards
                .push((nvg, crate::deopt::PredicateKind::Length as u8));
        }
        guards_planted += 1;

        let ncap = if !value_in_body {
            find_preheader_array_length(graph, body, nv, nvg)
        } else {
            None
        }
        .unwrap_or_else(|| {
            let al = graph.add(
                Op::ArrayLength,
                IrType::Int,
                with_licensing_guard(vec![entry_pred, pre_mem_tail, nv], nvg),
                hpc,
            );
            let _ = graph.set_node_frame_snapshot(al, si);
            pre_mem_tail = al;
            al
        });

        let old_coder_guards = collect_guards_on_cmp(graph, body, cand.old_coder, |n| {
            is_const_cmp_eq(graph, n, cand.old_coder, 0)
        });
        let old_val_guards = collect_guards_on_cmp(graph, body, cand.old_value, |n| {
            matches!(n.op, Op::Cmp(CmpOp::Ne))
        });
        let old_als: Vec<NodeId> = graph
            .users_of(cand.old_value)
            .into_iter()
            .filter(|&u| {
                body.contains(&u) && graph.node_opt(u).is_some_and(|n| n.op == Op::ArrayLength)
            })
            .collect();

        for g in old_coder_guards {
            if let Some(fs) = graph.node_opt(g).and_then(|n| n.frame_snapshot) {
                left_frames.push(fs);
            }
            graph.replace_all_uses(g, lg);
            graph.kill(g);
            *replaced += 1;
        }
        for g in old_val_guards {
            if let Some(fs) = graph.node_opt(g).and_then(|n| n.frame_snapshot) {
                left_frames.push(fs);
            }
            graph.replace_all_uses(g, nvg);
            graph.kill(g);
            *replaced += 1;
        }
        for al in old_als {
            splice_out_hoisted_mem_read(
                graph,
                al,
                ncap,
                left_frames,
                replaced,
                &mut spliced_old_nodes,
            );
        }
        if coder_in_body {
            splice_out_hoisted_mem_read(
                graph,
                cand.old_coder,
                nc,
                left_frames,
                replaced,
                &mut spliced_old_nodes,
            );
        }
        if value_in_body {
            splice_out_hoisted_mem_read(
                graph,
                cand.old_value,
                nv,
                left_frames,
                replaced,
                &mut spliced_old_nodes,
            );
        }
    }

    if let (Some(hm), Some(slot)) = (header_mem, entry_slot) {
        if pre_mem_tail != initial_pre_mem {
            graph.set_input(hm, 1 + slot, pre_mem_tail);
        }
    }
    guards_planted
}

/// Hoists loop-invariant `HashMap.get` table metadata (`table`, `table != null`,
/// `len = ArrayLength(table)`, `mask = len - 1`, `mask >= 0`, `(len & mask) == 0`)
/// into `entry_pred`.
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_optimize) fn hoist_invariant_hashmap_get(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    entry_mem: Option<NodeId>,
    header_bci: usize,
    si: u32,
    spliced: bool,
    left_frames: &mut Vec<u32>,
    replaced: &mut usize,
    body: &FxHashSet<NodeId>,
) -> usize {
    use crate::ir::{with_licensing_guard, CmpOp, MemKind};

    #[derive(Clone, Copy)]
    struct Cand {
        aload_id: NodeId,
        recv: NodeId,
        old_table: NodeId,
        table_field_idx: i64,
        old_len: NodeId,
        old_mask: NodeId,
    }

    let mut candidates: Vec<Cand> = Vec::new();
    for &id in body {
        let Some(al) = graph.node_opt(id) else {
            continue;
        };
        if al.op != Op::ArrayLoad(MemKind::Ref) || al.inputs.len() < 4 {
            continue;
        }
        let (al_ctrl, old_table, idx) = (al.inputs[0], al.inputs[2], al.inputs[3]);
        if !body.contains(&al_ctrl)
            || !ctrl_executes_every_iteration(graph, al_ctrl, region, back_ctrls)
        {
            continue;
        }
        let Some(tn) = graph.node_opt(old_table) else {
            continue;
        };
        if tn.op != Op::Load(MemKind::Ref) || tn.inputs.len() < 4 {
            continue;
        }
        let recv = tn.inputs[2];
        if body.contains(&recv) {
            continue;
        }
        let Some(Op::Const(table_field_idx)) = tn
            .input_opt(3)
            .and_then(|x| graph.node_opt(x))
            .map(|x| &x.op)
        else {
            continue;
        };
        let table_field_idx = *table_field_idx;
        let Some(idx_n) = graph.node_opt(idx) else {
            continue;
        };
        if idx_n.op != Op::And || idx_n.inputs.len() != 2 {
            continue;
        }
        let mask_len = [idx_n.inputs[0], idx_n.inputs[1]]
            .into_iter()
            .find_map(|m| {
                let mn = graph.node_opt(m)?;
                if mn.op != Op::Sub || mn.inputs.len() != 2 {
                    return None;
                }
                if !graph
                    .node_opt(mn.inputs[1])
                    .is_some_and(|k| k.op == Op::Const(1))
                {
                    return None;
                }
                let len_id = mn.inputs[0];
                let ln = graph.node_opt(len_id)?;
                if ln.op == Op::ArrayLength && ln.input_opt(2) == Some(old_table) {
                    Some((m, len_id))
                } else {
                    None
                }
            });
        let Some((old_mask, old_len)) = mask_len else {
            continue;
        };
        if !body.contains(&old_len) && !body.contains(&old_table) {
            continue;
        }
        candidates.push(Cand {
            aload_id: id,
            recv,
            old_table,
            table_field_idx,
            old_len,
            old_mask,
        });
    }
    if candidates.is_empty() {
        return 0;
    }

    let Some((entry_slot, header_mem, initial_pre_mem)) = loop_entry_mem_state(
        graph,
        region,
        entry_pred,
        body,
        entry_mem,
        candidates[0].old_table,
    ) else {
        return 0;
    };
    let mut pre_mem_tail = initial_pre_mem;
    let hpc = Some(header_bci);
    let mut guards_planted = 0usize;
    let mut hoisted: Vec<(NodeId, i64, NodeId, NodeId, NodeId)> = Vec::new();
    let mut spliced_old_nodes: FxHashSet<NodeId> = FxHashSet::default();

    for cand in candidates {
        let mask_guard = if let Some(&(_, _, _, _, mg)) = hoisted
            .iter()
            .find(|h| h.0 == cand.recv && h.1 == cand.table_field_idx)
        {
            mg
        } else {
            let old_lic = graph.node_opt(cand.old_table).and_then(|n| n.input_opt(4));
            let Some(pre_recv_guard) = plant_preheader_recv_guard(
                graph,
                entry_pred,
                header_bci,
                si,
                spliced,
                cand.recv,
                old_lic,
                body,
                left_frames,
                replaced,
                &mut guards_planted,
            ) else {
                continue;
            };
            let in_loop_body = |nid: NodeId| {
                body.contains(&nid)
                    && graph.node_opt(nid).and_then(|n| n.input_opt(0)) != Some(entry_pred)
            };
            let table_in_body = in_loop_body(cand.old_table);
            let len_in_body = in_loop_body(cand.old_len);
            let mask_in_body = table_in_body || len_in_body;
            let table_row = graph.field_rows.get(&cand.old_table).copied();
            let new_table = if table_in_body {
                let t_const = graph.add(Op::Const(cand.table_field_idx), IrType::Int, vec![], None);
                let nt = graph.add(
                    Op::Load(MemKind::Ref),
                    IrType::Ref,
                    with_licensing_guard(
                        vec![entry_pred, pre_mem_tail, cand.recv, t_const],
                        pre_recv_guard,
                    ),
                    hpc,
                );
                let _ = graph.set_node_frame_snapshot(nt, si);
                if let Some(r) = table_row {
                    graph.field_rows.insert(nt, r);
                }
                graph.synthetic_field_access_pcs.insert(header_bci);
                pre_mem_tail = nt;
                nt
            } else {
                cand.old_table
            };

            let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
            let tab_ne = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![new_table, null], None);
            let ntg = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, tab_ne],
                hpc,
            );
            let _ = graph.set_node_frame_snapshot(ntg, si);
            if spliced {
                graph
                    .chain_predicate_guards
                    .push((ntg, crate::deopt::PredicateKind::Length as u8));
            }
            guards_planted += 1;

            let new_len = if !table_in_body {
                find_preheader_array_length(graph, body, new_table, ntg)
            } else {
                None
            }
            .unwrap_or_else(|| {
                let al = graph.add(
                    Op::ArrayLength,
                    IrType::Int,
                    with_licensing_guard(vec![entry_pred, pre_mem_tail, new_table], ntg),
                    hpc,
                );
                let _ = graph.set_node_frame_snapshot(al, si);
                pre_mem_tail = al;
                al
            });

            let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
            let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
            let new_mask = graph.add(Op::Sub, IrType::Int, vec![new_len, one], None);
            let ge0 = graph.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![new_mask, zero], None);
            let nmg = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, ge0],
                hpc,
            );
            let _ = graph.set_node_frame_snapshot(nmg, si);

            let landm = graph.add(Op::And, IrType::Int, vec![new_len, new_mask], None);
            let pow2 = graph.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![landm, zero], None);
            let npow2g = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, pow2],
                hpc,
            );
            let _ = graph.set_node_frame_snapshot(npow2g, si);
            if spliced {
                let kind = crate::deopt::PredicateKind::Length as u8;
                graph.chain_predicate_guards.push((nmg, kind));
                graph.chain_predicate_guards.push((npow2g, kind));
            }
            guards_planted += 2;

            let old_tab_guards = collect_guards_on_cmp(graph, body, cand.old_table, |n| {
                matches!(n.op, Op::Cmp(CmpOp::Ne))
            });
            let old_mask_guards = collect_guards_on_cmp(graph, body, cand.old_mask, |n| {
                n.op == Op::Cmp(CmpOp::Ge)
                    && n.input_opt(0) == Some(cand.old_mask)
                    && n.input_opt(1)
                        .and_then(|k| graph.node_opt(k))
                        .is_some_and(|kn| kn.op == Op::Const(0))
            });
            let old_pow2_ands: Vec<NodeId> = graph
                .users_of(cand.old_mask)
                .into_iter()
                .filter(|&u| {
                    graph.node_opt(u).is_some_and(|un| {
                        un.op == Op::And
                            && ((un.input_opt(0) == Some(cand.old_len)
                                && un.input_opt(1) == Some(cand.old_mask))
                                || (un.input_opt(1) == Some(cand.old_len)
                                    && un.input_opt(0) == Some(cand.old_mask)))
                    })
                })
                .collect();
            let mut old_pow2_guards = Vec::new();
            for and_id in old_pow2_ands {
                old_pow2_guards.extend(collect_guards_on_cmp(graph, body, and_id, |n| {
                    is_const_cmp_eq(graph, n, and_id, 0)
                }));
            }

            for g in old_tab_guards {
                if let Some(fs) = graph.node_opt(g).and_then(|n| n.frame_snapshot) {
                    left_frames.push(fs);
                }
                graph.replace_all_uses(g, ntg);
                graph.kill(g);
                *replaced += 1;
            }
            for g in old_mask_guards {
                if let Some(fs) = graph.node_opt(g).and_then(|n| n.frame_snapshot) {
                    left_frames.push(fs);
                }
                graph.replace_all_uses(g, nmg);
                graph.kill(g);
                *replaced += 1;
            }
            for g in old_pow2_guards {
                if let Some(fs) = graph.node_opt(g).and_then(|n| n.frame_snapshot) {
                    left_frames.push(fs);
                }
                graph.replace_all_uses(g, npow2g);
                graph.kill(g);
                *replaced += 1;
            }
            if mask_in_body {
                graph.replace_all_uses(cand.old_mask, new_mask);
                graph.kill(cand.old_mask);
                *replaced += 1;
            }
            if len_in_body {
                splice_out_hoisted_mem_read(
                    graph,
                    cand.old_len,
                    new_len,
                    left_frames,
                    replaced,
                    &mut spliced_old_nodes,
                );
            }
            if table_in_body {
                splice_out_hoisted_mem_read(
                    graph,
                    cand.old_table,
                    new_table,
                    left_frames,
                    replaced,
                    &mut spliced_old_nodes,
                );
            }
            hoisted.push((cand.recv, cand.table_field_idx, new_table, new_len, nmg));
            nmg
        };

        if header_mem.is_none()
            && graph.node_opt(cand.aload_id).and_then(|n| n.input_opt(1)) == Some(initial_pre_mem)
        {
            graph.set_input(cand.aload_id, 1, pre_mem_tail);
        }
        let existing_tok = graph.node_opt(cand.aload_id).and_then(|n| n.input_opt(4));
        if existing_tok.is_none() || existing_tok.is_some_and(|tok| body.contains(&tok)) {
            attach_guard_token(graph, cand.aload_id, mask_guard);
        }
    }

    if let (Some(hm), Some(slot)) = (header_mem, entry_slot) {
        if pre_mem_tail != initial_pre_mem {
            graph.set_input(hm, 1 + slot, pre_mem_tail);
        }
    }
    guards_planted
}

/// `CRATONVM_JIT_IR_SEQ_CHARAT_TRAP` (nio21 wave 5, lane L5) -- **default
/// ON**, `=0` is the kill switch (the prefix then always declines to the call,
/// and the field forward refuses the loop, as in wave 4). Read per asked call.
pub(crate) fn seq_charat_trap_enabled() -> bool {
    true
}

/// Is `node` a live `CharSequence.charAt(I)C` interface call the optimizing
/// tier's `SeqCharAt` prefix answers (`crate::ir_string_search_prefix_kind`)?
/// The cheap name screen runs before the resolver.
pub(crate) fn is_seq_charat_call(node: &Node) -> bool {
    let Op::Call { info_ptr } = node.op else {
        return false;
    };
    if info_ptr == 0 || node.inputs.len() != 4 || node.ty != IrType::Int {
        return false;
    }
    // SAFETY: a non-zero `info_ptr` on an `Op::Call` addresses a
    // `JitInvokeInfo` the planner boxed and keeps alive on the artifact for the
    // life of the compiled method (tests leak theirs); the call lowering bakes
    // the same pointer (`ir::is_holds_lock_info_row` reads it the same way).
    let info: &crate::JitInvokeInfo = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
    info.invoke_kind == 2
        && info.method_name == "charAt"
        && info.descriptor == "(I)C"
        && crate::ir_string_search_prefix_kind(info.class_name, info.method_name, info.descriptor)
            == Some(crate::IrStringSearchKind::SeqCharAt)
}

/// Does the graph hold a frame a deopt AT `pc` (re-executing the invoke) can
/// resume? Inside a splice (some snapshot at `pc` carries a scope chain, or
/// the build's splice map places `pc` in a relocated body) it must be a
/// guard-capable chain snapshot the lowering can publish; outside one, the
/// method's own flat snapshot at `pc`.
///
/// nio21 wave 6 (lane L5): the chain arm now also asks, from the graph
/// alone, the refusals `ir_lower::Lowerer::seq_charat_trap_frame_ok` would
/// otherwise meet only at lowering, where they cost the whole optimizing
/// compile (`refuse_seq_charat_trap`): a `pc` inside a splice with no chain
/// snapshot (the splice frame states off), no compiling-method key (no chain
/// is ever built), a synchronized splice window in the graph (its sites
/// publish the flat frame), a chain that does not split, holds a monitor, or
/// is deeper than the VM's resume budget (`deopt::inlined_chain_refusal`).
/// Fail closed: a `false` here only withholds the class predicate for that
/// loop (the wave-4 shape, the call kept).
fn trap_frame_exists(graph: &Graph, pc: usize, map: Option<&crate::ir::IrSpliceResumeMap>) -> bool {
    let mut chain = false;
    let mut spliced = false;
    let mut flat = false;
    for s in graph.safepoints.iter().filter(|s| s.bci == pc) {
        if s.is_splice_state() {
            spliced = true;
            chain |= s.is_guard_splice_state() && chain_snapshot_publishable(s);
        } else if !s.post_call_only {
            flat = true;
        }
    }
    // A relocated pc the map attributes to an enclosing invoke is inside a
    // splice whatever its snapshots say.
    if map.is_some_and(|m| m.resume_bci(pc) != Some(pc)) {
        spliced = true;
    }
    if spliced {
        chain && !graph.scope_method_key.is_empty() && graph.sync_splice_windows.is_empty()
    } else {
        flat
    }
}

/// The graph-only half of `ir_lower`'s chain publication check for chain
/// snapshot `s`: the scope chain splits the snapshot's slots, no monitor is
/// held in it, and it is no deeper than the VM's resume budget.
fn chain_snapshot_publishable(s: &crate::ir::SafepointSnapshot) -> bool {
    let Some(chain) = s.splice_scope.as_deref() else {
        return false;
    };
    s.monitors.is_empty()
        && chain.scopes.len() >= 2
        && chain.scopes.len() - 1 <= crate::deopt::max_osr_inline_resume_depth()
        && chain.split(&s.locals, &s.stack).is_some()
}

/// nio21 wave 6 (lane L5): the bci the VM charges a failing trap at `pc` to,
/// and so the bci its per-site de-spec is recorded at
/// (`deopt_resume::real_frame_deopt_resume_or_throw_and_despeculate`: an
/// inlined chain at its OUTERMOST scope's bci, the call site of the top-level
/// splice in the compiling method; a flat frame at its own bci). The build's
/// splice map answers it for a relocated `pc` ([`crate::ir::IrSpliceResumeMap::resume_bci`],
/// the rule `ir_lower`'s `resume_bci` applies); with no map (the build
/// spliced nothing), `pc` is the method's own unless a chain snapshot sits
/// there. `None` when it cannot be attributed.
fn trap_charge_bci(
    graph: &Graph,
    pc: usize,
    map: Option<&crate::ir::IrSpliceResumeMap>,
) -> Option<usize> {
    match map {
        Some(m) => m.resume_bci(pc),
        None => (!graph
            .safepoints
            .iter()
            .any(|s| s.bci == pc && s.is_splice_state()))
        .then_some(pc),
    }
}

/// `CRATONVM_JIT_IR_SEQ_CHARAT_TRAP_SITE_DESPEC` (nio21 wave 6, lane L5) --
/// **default ON**; `=0` restores wave 5's per-method withdrawal rule (any
/// de-spec in the method withholds the trap). Read per asked loop.
pub(super) fn site_despec_enabled() -> bool {
    true
}

/// `(entry control, header bci)` of every live loop-entry class guard
/// `class_predicate` plants: `Guard{bci: b}` at pc `b`, condition
/// `ExactClassIs`, control the loop's entry predecessor.
///
/// JIT round 20 (lane strings, page
/// `r21-charat-trap-reads-a-length-predicate-class-guard`): only at a header
/// `class_predicate` itself planted for ([`Graph::seq_charat_trap_headers`]),
/// i.e. a loop that passed [`class_predicate_withheld`]'s frame and de-spec
/// screen. Before, the SHAPE was the licence, and any other planter of it
/// (`plant_length_predicates` moving an in-body trap-form receiver guard to
/// the pre-header, with `CRATONVM_JIT_IR_LENGTH_PREDICATE_CHARAT_SCREEN=0` or
/// any future pass) licensed an unscreened trap: a compile refused for a
/// missing frame, or a trap re-planted after the site's de-spec. Unconditional
/// (it only withholds traps nothing screened).
fn class_guarded_entries(graph: &Graph) -> FxHashSet<(NodeId, usize)> {
    let mut out = FxHashSet::default();
    if graph.seq_charat_trap_headers.is_empty() {
        return out;
    }
    for n in graph.nodes.iter() {
        let Op::Guard { bci } = n.op else {
            continue;
        };
        if n.bytecode_pc != Some(bci) || !graph.seq_charat_trap_headers.contains(&bci) {
            continue;
        }
        let (Some(ctrl), Some(cond)) = (n.input_opt(0), n.input_opt(1)) else {
            continue;
        };
        if ctrl == NO_NODE
            || !graph
                .node_opt(cond)
                .is_some_and(|c| matches!(c.op, Op::ExactClassIs { .. }))
        {
            continue;
        }
        out.insert((ctrl, bci));
    }
    out
}

/// Is `id` a `SeqCharAt` call whose prefix deopts on a miss (see the module
/// notes)? The one predicate the field forward and the lowering share.
pub(crate) fn seq_charat_traps_on_miss(graph: &Graph, id: NodeId) -> bool {
    // The narrow-oop screen is the prefix's own compile-time refusal
    // (`try_emit_ir_string_search_prefix`); asking it here keeps the lowering's
    // bailout for a named call to the per-compile shapes alone.
    if !seq_charat_trap_enabled() || crate::x64::narrow_oops_block_inline_fields() {
        return false;
    }
    let Some(node) = graph.node_opt(id) else {
        return false;
    };
    // Structure first: the info row is read only for a 4-input `int` call
    // with a pc (the trap resumes at it).
    //
    // No frame test here, deliberately: the predicate must answer the same
    // after optimize as at lowering, and snapshot pruning runs in between.
    // `class_predicate` refuses a loop whose call has no trap frame
    // ([`class_predicate_withheld`]), and the lowering refuses the compile
    // when a named call's frame has gone since.
    let Some(ctrl) = node.input_opt(0) else {
        return false;
    };
    if node.bytecode_pc.is_none()
        || !matches!(node.op, Op::Call { .. })
        || ctrl == NO_NODE
        || !is_seq_charat_call(node)
    {
        return false;
    }
    let entries = class_guarded_entries(graph);
    if entries.is_empty() {
        return false;
    }
    let users = build_users(graph);
    loop_headers(graph)
        .into_iter()
        .any(|(region, entry_pred, back_ctrls)| {
            let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
                return false;
            };
            if !entries.contains(&(entry_pred, header_bci)) {
                return false;
            }
            let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
            body.contains(&id) || body.contains(&ctrl)
        })
}

/// Why `class_predicate` must plant nothing for the loop at `region`, when it
/// holds a `SeqCharAt` call that a planted guard would turn into a trap
/// ([`seq_charat_traps_on_miss`]); `None` when it may plant.
///
/// * The VM has de-specialised the site a call's trap is charged at
///   ([`withheld_by_despec`]).
/// * A call has no frame a trap at its pc could resume (`trap_frame_exists`):
///   the lowering would have to refuse the compile.
///
/// `map` is the build's splice resume map
/// (`crate::ir::splice_resume_map_this_build`, read once by the caller).
#[allow(clippy::too_many_arguments)]
pub(super) fn class_predicate_withheld(
    graph: &Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    map: Option<&crate::ir::IrSpliceResumeMap>,
) -> Option<&'static str> {
    if !seq_charat_trap_enabled() || crate::x64::narrow_oops_block_inline_fields() {
        return None;
    }
    let users = build_users(graph);
    let calls: Vec<NodeId> = loop_body_with(graph, &users, region, entry_pred, back_ctrls)
        .into_iter()
        .filter(|&id| graph.node_opt(id).is_some_and(is_seq_charat_call))
        .collect();
    if calls.is_empty() {
        return None;
    }
    if withheld_by_despec(despec, method_key, graph, &calls, map) {
        return Some("a charAt trap site was de-specialised");
    }
    let framed = calls.iter().all(|&id| {
        graph
            .node_opt(id)
            .and_then(|n| n.bytecode_pc)
            .is_some_and(|pc| trap_frame_exists(graph, pc, map))
    });
    (!framed).then_some("a charAt call has no trap frame")
}

/// `class_predicate`'s withdrawal rule for a loop holding the `SeqCharAt`
/// calls `calls`.
///
/// nio21 wave 6 (lane L5, proposal L5W5-3): per SITE. A failing trap is
/// charged, and after the VM's per-bci limit de-specialised, at
/// [`trap_charge_bci`] of its call's pc (the outermost invoke of the chain
/// for a call inside a splice), which the splice map names ahead of time; the
/// trap is withheld when ANY call's site is de-specialised there (any
/// speculation id: the chain charge records the wildcard). A call whose site
/// cannot be attributed falls back to wave 5's per-method rule (the VM has
/// de-specialised SOMETHING in this method), the answer that is never too
/// weak; so does `CRATONVM_JIT_IR_SEQ_CHARAT_TRAP_SITE_DESPEC=0` for every
/// call. Either way the next compile keeps the loop's in-body class tests and
/// the call.
pub(super) fn withheld_by_despec(
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    graph: &Graph,
    calls: &[NodeId],
    map: Option<&crate::ir::IrSpliceResumeMap>,
) -> bool {
    if !seq_charat_trap_enabled() {
        return false;
    }
    let per_method = || despec.count_for(method_key) > 0;
    if !site_despec_enabled() {
        return per_method();
    }
    calls.iter().any(|&id| {
        let site = graph
            .node_opt(id)
            .and_then(|n| n.bytecode_pc)
            .and_then(|pc| trap_charge_bci(graph, pc, map))
            .and_then(|b| u32::try_from(b).ok());
        match site {
            Some(bci) => despec.contains(method_key, bci),
            None => per_method(),
        }
    })
}

// ── nio21 wave 21 (lane L3): the trapping charAt's String reads, hoisted ──
//
// `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST` (opt-in `=1` after the m5 A/B; off keeps the call and its
// per-char prefix).
//
// # What the prefix still costs per char
//
// A trapping `SeqCharAt` call (above) is lowered as the String prefix
// (`ir_lower::Lowerer::try_emit_ir_string_search_prefix`). On EVERY call the
// prefix re-proves the receiver and re-reads its fields: the exact `String`
// class-id compare, then for `value` and for `coder` the per-object
// `GC_FLAG_COMPACT` dispatch and the load, then `value.length`, then the
// bounds compare and the element load. In `encodeBufferLoop` the receiver is
// `src.str`, which the field forward makes ONE pre-header value. So that whole
// chain is loop-invariant except the index compare and the element load, yet
// it is machine code inside the call's lowering, where no pass can hoist it.
//
// # What this pass does
//
// For each trapping charAt call in a class-predicated loop whose receiver is
// loop-invariant, it rewrites the call as IR:
//
// * **Pre-header** (on the loop's entry control, threaded on the forward
//   memory edge, after the class predicate's guard and the forward's reads):
//   `Guard{bci: header}(ExactClassIs(recv, String))` with the loop-entry frame;
//   `coder = recv.coder`, `value = recv.value`, `Guard(value != null)`, and
//   `count = value.length >>> coder`. The loads carry the String layout's
//   compact rows (`Graph::field_rows`, the rows the compile door gives
//   `String` accessor sites).
// * **At the call**: `Guard{bci: call}(0 <= idx)`, `Guard{bci: call}(idx < count)`,
//   then the branchless LATIN1/UTF16 decode `IrBuilder::try_string_access_intrinsic`
//   builds (two byte loads at `idx << coder` and `+ coder`).
//
// The call's value uses take the decoded char, its memory users the last
// element load. The call is killed.
//
// # Why it is exact
//
// * `String.value` and `String.coder` are final fields of an immutable class,
//   written only by the constructor before the String is published (HotSpot
//   trusts both as stable). So reading them once at the loop entry gives every
//   iteration the values the call would have read, whatever the loop calls in
//   between. `value.length` is an array's length and never changes.
// * The pre-header guard fails exactly when the receiver is not a `String`
//   (`ExactClassIs` of null is `0`). It deopts to the LOOP ENTRY with the
//   loop-entry frame, and the interpreter runs the loop and every charAt
//   itself. A failing guard is charged at the header bci, where
//   `class_predicate` reads the de-spec registry, so after
//   `PER_BCI_DESPEC_LIMIT` charges the next compile plants no class predicate,
//   no trap and no hoist.
// * The two in-body guards fail exactly where the prefix declined (an index
//   outside `[0, length)`). They deopt AT the call's pc with the frame the trap
//   already required (`class_predicate`'s `trap_frame_exists`), and the
//   interpreter re-executes `charAt`, which throws the JDK's own exception.
//   Nothing has been committed between the call's pc and the guard.
// * The decode reads `value[idx << coder]` and `value[(idx << coder) + coder]`.
//   For `0 <= idx < length >>> coder`, both are inside the array (the builder
//   expansion's argument).
//
// Refused (the call stays): the switch off, no String layout with a `coder`
// and a class id, narrow oops, a receiver defined in the loop, a call with an
// exceptional continuation (`CallThrew` / `CatchEntryFlag`), no loop-entry
// frame, a header the method has de-specialised, and more than
// `MAX_CHARAT_HOISTS` calls per compile.

/// `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST` (nio21 wave 21, lane L3) -- **opt-in**
/// (`=1`): the m5 host A/B measured it slower on the encode rows
/// (`Nio20EncodeLoop encode-ascii` 72 306 ns/op on vs 57 318 off), so every
/// trapping charAt stays a call (the wave-5 prefix) unless it is asked for.
pub(super) fn seq_charat_hoist_enabled() -> bool {
    false
        // JIT r25 wave 11 (W11-4): or the chain loop-proof bundle.
        || super::r25_w11_4_chain_loop_proofs::bundled("CRATONVM_JIT_IR_SEQ_CHARAT_HOIST")
        // JIT rx27 wave 3 (lane scanloop): or the outer-loop hoist, which
        // yields to an explicit kill of this key as the bundle does.
        || (outer_hoist_enabled()
            && !super::r25_w11_4_chain_loop_proofs::explicitly_off(
                "CRATONVM_JIT_IR_SEQ_CHARAT_HOIST",
            ))
}

// ── JIT rx27 wave 3 (lane scanloop): the outer position loop's String reads ──
//
// `CRATONVM_JIT_IR_SEQ_CHARAT_OUTER_HOIST` (default ON, `=0` kills), page
// `docs/feature-designs/jit-ir-optimizations.md`.
//
// The hoist above already moves a trapping `charAt`'s `value` / `coder` /
// length reads to the entry of the String-guarded loop, which in
// `Pattern$Start.match` is the OUTER position loop: the right place, because
// the inner `BmpCharPropertyGreedy` scan runs about one iteration per text
// position, so the outer body IS the per-character code. W11-4 / W13-4
// measured it 3-8 % SLOWER than the prefix. Of what it leaves per character,
// two parts are larger than the prefix's LATIN1 path, and this arm removes
// both:
//
// * the branchless LATIN1/UTF16 decode (shift, two byte loads, add, two masks,
//   shift, negate, and, or: about ten instructions against the prefix's coder
//   test and one `MOVZX`). Under the switch the entry also guards `coder ==
//   LATIN1` (deopt to the loop entry with the loop-entry frame, charged like
//   the String guard), and the call becomes ONE byte load at `idx` and a
//   `& 0xFF`. A UTF16 subject deopts at the entry; after the de-spec limit
//   the next compile plants no predicate at that header and keeps the call
//   (the class kind the String guard is charged as). `=0` of
//   `CRATONVM_JIT_IR_SEQ_CHARAT_OUTER_HOIST_LATIN1` keeps the decode.
// * the two per-character index guards (`0 <= i`, `i < count`). The switch
//   turns the entry bound (W6C-P3) on with the hoist, offered every loop
//   nested in the guarded one (W12-4): the greedy scan's own `i < to` header
//   test, with `to` read once per position, answers both by one `to <= count`
//   guard (and `0 <= init`) at the INNER loop's entry, once per position.
//
// Soundness is the hoist's own (above) plus one guard. `coder` is a final
// field of an immutable class, read once at the entry; a String whose coder
// is `0` is LATIN1, one byte per char, so `value[idx] & 0xFF` for `0 <= idx <
// value.length >>> 0` is exactly what `StringLatin1.charAt` returns. The
// coder guard fails exactly when that premise does, before the loop runs.

/// `CRATONVM_JIT_IR_SEQ_CHARAT_OUTER_HOIST` (JIT rx27 wave 3, lane
/// scanloop) -- **default ON** since the rx27 A/Bs (t7 on b4, t8 on b5:
/// find-ci 0.91x, find-email 0.93x both runs; cached, matchonly, split-ws
/// lower; regex-matches +1.2 %), `=0` kills. Turns on the hoist, its LATIN1
/// speculation and the entry bound together. Read per compile.
pub(super) fn outer_hoist_enabled() -> bool {
    true
}

/// The LATIN1 speculation of the outer hoist: ON under it unless
/// `CRATONVM_JIT_IR_SEQ_CHARAT_OUTER_HOIST_LATIN1=0`.
pub(super) fn outer_hoist_latin1_enabled() -> bool {
    outer_hoist_enabled() && true
}

/// The most charAt calls one compile rewrites.
const MAX_CHARAT_HOISTS: usize = 4;

/// Rewrite the trapping charAt calls this section describes. Returns how many
/// were rewritten.
pub(super) fn hoist_seq_charat_string_reads(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> usize {
    if !seq_charat_hoist_enabled() || crate::x64::narrow_oops_block_inline_fields() {
        return 0;
    }
    let Some(layout) = graph.string_layout else {
        return 0;
    };
    if !layout.has_coder || layout.string_class_id == 0 {
        return 0;
    }
    let mut done = 0usize;
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if done >= MAX_CHARAT_HOISTS {
            break;
        }
        let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
            continue;
        };
        if !class_guarded_entries(graph).contains(&(entry_pred, header_bci)) {
            continue;
        }
        if u32::try_from(header_bci).is_ok_and(|b| despec.contains(method_key, b)) {
            continue;
        }
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let mut calls: Vec<NodeId> = body
            .iter()
            .copied()
            .filter(|&id| {
                !seen.contains(&id)
                    && graph.node_opt(id).is_some_and(is_seq_charat_call)
                    && seq_charat_traps_on_miss(graph, id)
            })
            .collect();
        calls.sort_unstable();
        for call in calls {
            if done >= MAX_CHARAT_HOISTS {
                break;
            }
            seen.insert(call);
            let latin1 = outer_hoist_latin1_enabled();
            let hoisted = hoist_one(
                graph, despec, method_key, layout, region, entry_pred, header_bci, &body, call,
                latin1,
            );
            // JIT rx27 wave 3 (lane scanloop): one line per asked call.
            if outer_hoist_enabled() && crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-seq-charat-outer-hoist {method_key} header n{region} bci \
                     {header_bci} n{call}: {}",
                    if !hoisted {
                        "kept (receiver not invariant, an exception arm, no loop-entry frame, or \
                         a withdrawn header)"
                    } else if latin1 {
                        "value/coder/length at the entry, coder == LATIN1 guarded, one byte load"
                    } else {
                        "value/coder/length at the entry, branchless decode"
                    }
                );
            }
            if hoisted {
                done += 1;
                if super::loop_pass_report_enabled() {
                    eprintln!(
                        "[ir-licm] charAt hoist: {method_key} header bci {header_bci}: the String \
                         reads of n{call} moved to the loop entry"
                    );
                }
            } else if super::loop_pass_report_enabled() {
                eprintln!(
                    "[ir-licm] charAt hoist: {method_key} header bci {header_bci}: n{call} kept \
                     (receiver not invariant, an exception arm, or no loop-entry frame)"
                );
            }
        }
    }
    done
}

/// The compact rows the String layout gives `value` and `coder`, under the
/// compile door's own conditions for its String accessor rows.
fn string_rows(layout: &crate::StringFieldLayout) -> Option<((u32, bool, u8), (u32, bool, u8))> {
    if !cratonvm_types::compact_ref_fields_enabled()
        || layout.value_compact_is_narrow
        || layout.value_compact_offset < 0
        || layout.coder_compact_offset < 0
    {
        return None;
    }
    let coder_tag = if layout.coder_compact_is_byte {
        b'B'
    } else {
        b'I'
    };
    Some((
        (layout.value_compact_offset as u32, true, b'['),
        (layout.coder_compact_offset as u32, false, coder_tag),
    ))
}

#[allow(clippy::too_many_arguments)]
fn hoist_one(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    layout: crate::StringFieldLayout,
    region: NodeId,
    entry_pred: NodeId,
    header_bci: usize,
    body: &FxHashSet<NodeId>,
    call: NodeId,
    latin1: bool,
) -> bool {
    let Some(node) = graph.node_opt(call).cloned() else {
        return false;
    };
    let (Some(ctrl), Some(call_mem), Some(recv), Some(idx), Some(pc)) = (
        node.input_opt(0),
        node.input_opt(1),
        node.input_opt(2),
        node.input_opt(3),
        node.bytecode_pc,
    ) else {
        return false;
    };
    if body.contains(&recv) || !super::is_loop_invariant(graph, recv, region, body) {
        return false;
    }
    let exception_arm = graph.nodes.iter().any(|n| match n.op {
        Op::CallThrew { .. } => n.input_opt(1) == Some(call),
        Op::CatchEntryFlag { call_bci, .. } => call_bci as usize == pc,
        _ => false,
    });
    if exception_arm {
        return false;
    }
    // The forward memory edge of the header's one memory phi.
    let Some(fwd) = graph
        .node_opt(region)
        .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
    else {
        return false;
    };
    let mem_phis: Vec<NodeId> = graph
        .users_of(region)
        .into_iter()
        .filter(|&u| {
            graph.node_opt(u).is_some_and(|n| {
                n.op == Op::Phi && n.ty == IrType::Memory && n.input_opt(0) == Some(region)
            })
        })
        .collect();
    let [header_mem] = mem_phis.as_slice() else {
        return false;
    };
    let header_mem = *header_mem;
    let Some(mut pre_mem) = graph
        .node_opt(header_mem)
        .and_then(|n| n.input_opt(fwd + 1))
    else {
        return false;
    };
    // JIT round 25 wave 7 (lane W7-D, `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_SPLICED`):
    // a loop spliced from a callee has only the inlined chain at its header,
    // so its entry frame is the loop-entry CHAIN and its withdrawal the call
    // site's chain id (`seq_charat_predicate::w7d`). Off, `spliced` is false.
    let spliced = super::seq_charat_predicate::w7d::spliced_header(graph, region);
    let entry = if spliced {
        super::loop_entry_chain_frame(graph, region, fwd + 1, body)
    } else {
        super::loop_entry_frame(graph, region, fwd + 1, body)
    };
    let Some((bci, frame)) = entry else {
        return false;
    };
    if bci != header_bci {
        return false;
    }
    if spliced
        && super::seq_charat_predicate::w7d::withdrawn(
            despec,
            method_key,
            header_bci,
            &frame,
            crate::deopt::PredicateKind::Class,
        )
    {
        return false;
    }
    let Ok(si) = u32::try_from(graph.safepoints.len()) else {
        return false;
    };
    graph.push_safepoint(frame);
    let rows = string_rows(&layout);

    // ── The pre-header. ──
    let hpc = Some(header_bci);
    let is_string = graph.add(
        Op::ExactClassIs {
            class_id: layout.string_class_id,
        },
        IrType::Int,
        vec![recv],
        None,
    );
    let class_guard = graph.add(
        Op::Guard { bci: header_bci },
        IrType::Void,
        vec![entry_pred, is_string],
        hpc,
    );
    if !graph.set_node_frame_snapshot(class_guard, si) {
        graph.kill(class_guard);
        graph.kill(is_string);
        return false;
    }
    let coder_idx = graph.add(
        Op::Const(layout.coder_field_index as i64),
        IrType::Int,
        vec![],
        None,
    );
    let coder = graph.add(
        Op::Load(crate::ir::MemKind::Int),
        IrType::Int,
        crate::ir::with_licensing_guard(vec![entry_pred, pre_mem, recv, coder_idx], class_guard),
        hpc,
    );
    pre_mem = coder;
    let value_idx = graph.add(
        Op::Const(layout.value_field_index as i64),
        IrType::Int,
        vec![],
        None,
    );
    let value = graph.add(
        Op::Load(crate::ir::MemKind::Ref),
        IrType::Ref,
        crate::ir::with_licensing_guard(vec![entry_pred, pre_mem, recv, value_idx], class_guard),
        hpc,
    );
    pre_mem = value;
    for load in [coder, value] {
        // Unreachable refusal: the loads' pc is the frame's bci.
        let _ = graph.set_node_frame_snapshot(load, si);
    }
    graph.synthetic_field_access_pcs.insert(header_bci);
    if let Some((value_row, coder_row)) = rows {
        graph.field_rows.insert(value, value_row);
        graph.field_rows.insert(coder, coder_row);
    }
    let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
    let value_ok = graph.add(
        Op::Cmp(crate::ir::CmpOp::Ne),
        IrType::Int,
        vec![value, null],
        None,
    );
    let value_guard = graph.add(
        Op::Guard { bci: header_bci },
        IrType::Void,
        vec![entry_pred, value_ok],
        hpc,
    );
    let _ = graph.set_node_frame_snapshot(value_guard, si);
    if spliced {
        // The lowering gives a chain point listed here its chain id, which
        // the VM's chain sinks record at the outermost call site.
        let kind = crate::deopt::PredicateKind::Class as u8;
        graph.chain_predicate_guards.push((class_guard, kind));
        graph.chain_predicate_guards.push((value_guard, kind));
    }
    // JIT rx27 wave 3 (lane scanloop, `CRATONVM_JIT_IR_SEQ_CHARAT_OUTER_HOIST`):
    // `coder == LATIN1` at the entry, so the call below is one byte load.
    if latin1 {
        let latin1_coder = graph.add(Op::Const(0), IrType::Int, vec![], None);
        let is_latin1 = graph.add(
            Op::Cmp(crate::ir::CmpOp::Eq),
            IrType::Int,
            vec![coder, latin1_coder],
            None,
        );
        let coder_guard = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, is_latin1],
            hpc,
        );
        let _ = graph.set_node_frame_snapshot(coder_guard, si);
        if spliced {
            let kind = crate::deopt::PredicateKind::Class as u8;
            graph.chain_predicate_guards.push((coder_guard, kind));
        }
    }
    let byte_len = graph.add(
        Op::ArrayLength,
        IrType::Int,
        crate::ir::with_licensing_guard(vec![entry_pred, pre_mem, value], value_guard),
        hpc,
    );
    let _ = graph.set_node_frame_snapshot(byte_len, si);
    let count = if latin1 {
        byte_len
    } else {
        graph.add(Op::UShr, IrType::Int, vec![byte_len, coder], None)
    };
    graph.set_input(header_mem, fwd + 1, pre_mem);

    // ── At the call. ──
    let cpc = Some(pc);
    let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
    let non_negative = graph.add(
        Op::Cmp(crate::ir::CmpOp::Ge),
        IrType::Int,
        vec![idx, zero],
        None,
    );
    graph.add(
        Op::Guard { bci: pc },
        IrType::Void,
        vec![ctrl, non_negative],
        cpc,
    );
    let in_bounds = graph.add(
        Op::Cmp(crate::ir::CmpOp::Lt),
        IrType::Int,
        vec![idx, count],
        None,
    );
    let bounds_guard = graph.add(
        Op::Guard { bci: pc },
        IrType::Void,
        vec![ctrl, in_bounds],
        cpc,
    );
    let byte_mask = graph.add(Op::Const(0xFF), IrType::Int, vec![], None);
    if latin1 {
        // LATIN1 proven at the entry: `value[idx] & 0xFF`, the whole decode.
        let raw = graph.add(
            Op::ArrayLoad(crate::ir::MemKind::Byte),
            IrType::Int,
            crate::ir::with_licensing_guard(vec![ctrl, call_mem, value, idx], bounds_guard),
            cpc,
        );
        let ch = graph.add(Op::And, IrType::Int, vec![raw, byte_mask], None);
        rewire_charat_call(graph, call, raw, ch);
        return true;
    }
    let off = graph.add(Op::Shl, IrType::Int, vec![idx, coder], None);
    let lo_raw = graph.add(
        Op::ArrayLoad(crate::ir::MemKind::Byte),
        IrType::Int,
        crate::ir::with_licensing_guard(vec![ctrl, call_mem, value, off], bounds_guard),
        cpc,
    );
    let hi_off = graph.add(Op::Add, IrType::Int, vec![off, coder], None);
    let hi_raw = graph.add(
        Op::ArrayLoad(crate::ir::MemKind::Byte),
        IrType::Int,
        crate::ir::with_licensing_guard(vec![ctrl, lo_raw, value, hi_off], bounds_guard),
        cpc,
    );
    let lo = graph.add(Op::And, IrType::Int, vec![lo_raw, byte_mask], None);
    let hi = graph.add(Op::And, IrType::Int, vec![hi_raw, byte_mask], None);
    let eight = graph.add(Op::Const(8), IrType::Int, vec![], None);
    let hi_shifted = graph.add(Op::Shl, IrType::Int, vec![hi, eight], None);
    let coder_mask = graph.add(Op::Sub, IrType::Int, vec![zero, coder], None);
    let hi_selected = graph.add(Op::And, IrType::Int, vec![hi_shifted, coder_mask], None);
    let ch = graph.add(Op::Or, IrType::Int, vec![lo, hi_selected], None);
    rewire_charat_call(graph, call, hi_raw, ch);
    true
}

/// The hoisted call's uses: memory users take `last_load` (the last element
/// load, whose own token chain already starts at the call's), value users
/// `ch`; the call dies.
fn rewire_charat_call(graph: &mut Graph, call: NodeId, last_load: NodeId, ch: NodeId) {
    for user in graph.users_of(call) {
        if user == last_load {
            continue;
        }
        let Some(un) = graph.node_opt(user) else {
            continue;
        };
        let slots: Vec<usize> = un
            .inputs
            .as_slice()
            .iter()
            .enumerate()
            .filter(|&(i, &x)| x == call && crate::ir::is_memory_token_slot(un, i))
            .map(|(i, _)| i)
            .collect();
        for slot in slots {
            graph.set_input(user, slot, last_load);
        }
    }
    graph.replace_all_uses(call, ch);
    graph.kill(call);
}

// ── JIT rx27 wave 1 (lane scanloop): the receiver tests of a proven String ──
//
// `CRATONVM_JIT_IR_SEQ_CHARAT_PROVEN_STRING` (opt-in `=1`).
//
// # What the trapping prefix still re-proves per character
//
// A trapping `SeqCharAt` call keeps the whole String prefix
// (`ir_lower::Lowerer::try_emit_ir_string_search_prefix`): per CALL it tests
// the receiver for null, for a plain-object kind tag and for the exact `String`
// class id (six instructions, three of them branches), and only then reads
// `value` / `coder` and indexes. In the regex scan loops (`Pattern$Start.match`
// around the spliced `BmpCharPropertyGreedy` scan, the r23-w2 page's
// "75 instructions per character") the receiver is the invariant `seq`, and
// `seq_charat_predicate` has already planted `Guard{bci: header}(ExactClassIs(seq,
// String))` at the loop's entry. Those three tests answer, every character, a
// question that guard answered once.
//
// # Why not the hoist
//
// `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST` answered the same tests once, and the
// `value` / `coder` / length reads with them, and LOST 3-8 % on the `\s+` rows
// (r23-w2 "Wave 13 W13-4"): it moves the reads to the OUTER position loop's
// entry, where they live across the whole position loop and its kept calls,
// and leaves two per-character index guards in place of the prefix's one
// unsigned compare. A hoist to the INNER scan loop's entry would not help
// either: on these rows the scan loop runs about one iteration per text
// position (a non-matching character ends it), so its pre-header runs once per
// character too. This arm adds no value, no live range and no guard: it only
// drops machine code the dominating guard already proved.
//
// # Why it is exact
//
// [`seq_charat_receiver_proven_string`] answers only for a call that
// [`seq_charat_traps_on_miss`] names (so the guard was screened by
// `class_predicate_withheld`), whose receiver node is the operand of a live
// `Guard{bci: header}(ExactClassIs(recv, String))` planted on the entry control
// of a loop whose body holds the call, at a header the trap licence lists
// ([`class_guarded_entries`]). `ExactClassIs` answers `0` for null and for an
// array, so a passing guard proves the three facts the dropped tests check;
// `recv` is one SSA value defined before the loop, so it is the same object at
// every character. The loop has one entry and its back edges are dominated by
// the header (`loop_headers`), so every execution of the call ran the guard.
// An OSR entry stub into the loop (or into a loop nested in it) would skip the
// guard: the lowering refuses such an entry, or re-tests the seeded receiver,
// under `CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL` and
// `CRATONVM_JIT_IR_OSR_INNER_PREDICATE_REFUSAL` (both default ON, the rule the
// class predicate's folded in-body tests already rely on), so the answer is
// `None` when either is off. Every other test of the prefix stays: the layout
// epoch, the per-object compact dispatch, the `value` null test, the coder
// and the index compare, each still a deopt at the call's pc.

/// `CRATONVM_JIT_IR_SEQ_CHARAT_PROVEN_STRING` (JIT rx27 wave 1, lane
/// scanloop) -- **default on** (`=1`) until the host A/B. Read per asked call.
pub(crate) fn seq_charat_proven_string_enabled() -> bool {
    true
}

/// For a trapping `SeqCharAt` call `id` whose receiver a dominating
/// loop-entry `String` guard proves (the section above): `(guard, header
/// bci)`. `None` when the switch is off, the call does not trap, or no such
/// guard covers its receiver. The one predicate the lowering asks; it reads
/// only the graph, so it answers the same at lowering as after optimize.
pub(crate) fn seq_charat_receiver_proven_string(
    graph: &Graph,
    id: NodeId,
) -> Option<(NodeId, usize)> {
    if !seq_charat_proven_string_enabled() {
        return None;
    }
    receiver_string_guard(graph, id)
}

/// [`seq_charat_receiver_proven_string`] whatever its switch says (the
/// census asks it with the switch off too).
fn receiver_string_guard(graph: &Graph, id: NodeId) -> Option<(NodeId, usize)> {
    let string_class = graph
        .string_layout
        .filter(|l| l.has_coder && l.string_class_id != 0)?
        .string_class_id;
    if !seq_charat_traps_on_miss(graph, id) {
        return None;
    }
    let node = graph.node_opt(id)?;
    let (ctrl, recv) = (node.input_opt(0)?, node.input_opt(2)?);
    if recv == NO_NODE {
        return None;
    }
    // `(entry control, header bci, guard)` of every licensed String guard on
    // `recv`.
    let guards: Vec<(NodeId, usize, NodeId)> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(gid, n)| {
            let Op::Guard { bci } = n.op else {
                return None;
            };
            if n.bytecode_pc != Some(bci) || !graph.seq_charat_trap_headers.contains(&bci) {
                return None;
            }
            let (g_ctrl, cond) = (n.input_opt(0)?, n.input_opt(1)?);
            let c = graph.node_opt(cond)?;
            let proves =
                c.op == Op::ExactClassIs {
                    class_id: string_class,
                } && c.input_opt(0) == Some(recv);
            // Cast: arena index to node id.
            (proves && g_ctrl != NO_NODE).then_some((g_ctrl, bci, gid as NodeId))
        })
        .collect();
    if guards.is_empty() {
        return None;
    }
    let users = build_users(graph);
    loop_headers(graph)
        .into_iter()
        .find_map(|(region, entry_pred, back_ctrls)| {
            let header_bci = graph.node_opt(region)?.bytecode_pc?;
            let &(_, _, guard) = guards
                .iter()
                .find(|&&(g_ctrl, bci, _)| g_ctrl == entry_pred && bci == header_bci)?;
            let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
            ((body.contains(&id) || body.contains(&ctrl)) && !body.contains(&recv))
                .then_some((guard, header_bci))
        })
}

/// JIT rx27 wave 1 (lane scanloop): one `CRATONVM_DBG_JITC` line per loop
/// that holds a `CharSequence.charAt` call, after the loop proofs ran: what
/// the per-character code of that loop still pays, and what was answered at
/// its entry. Zeros are printed (a zero is a claim about the workload).
///
/// * `spliced`: the header comes from a callee (the greedy scan inside
///   `Start.match`).
/// * `charAt calls` / `trapping` / `receiver proven`: the calls, those that
///   deopt on a miss, and those whose receiver a loop-entry `String` guard
///   proves (the prefix drops its receiver tests when
///   `CRATONVM_JIT_IR_SEQ_CHARAT_PROVEN_STRING=1`, `elided`).
/// * `entry guards`: guards planted on the loop's entry control at its header
///   bci (the String predicate, the field-class predicate, a hoist).
/// * `class-test branches`: `If`s and `Guard`s left in the body on an
///   `ExactClassIs` (the lambda and node class tests the field-class
///   predicate did not fold).
/// * `ref field reads`: reference `Load`s left in the body (`predicate`,
///   `next`, the prefix's `value` once hoisted).
pub(super) fn note_scan_loop_census(graph: &Graph, method_key: &str) {
    if !crate::x64::dbg_jitc_enabled() {
        return;
    }
    let users = build_users(graph);
    let elide = seq_charat_proven_string_enabled();
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let mut ids: Vec<NodeId> = body.iter().copied().collect();
        ids.sort_unstable();
        let calls: Vec<NodeId> = ids
            .iter()
            .copied()
            .filter(|&id| graph.node_opt(id).is_some_and(is_seq_charat_call))
            .collect();
        if calls.is_empty() {
            continue;
        }
        let trapping = calls
            .iter()
            .filter(|&&c| seq_charat_traps_on_miss(graph, c))
            .count();
        let proven = calls
            .iter()
            .filter(|&&c| receiver_string_guard(graph, c).is_some())
            .count();
        let header_bci = graph.node_opt(region).and_then(|r| r.bytecode_pc);
        let entry_guards = graph
            .nodes
            .iter()
            .filter(|n| {
                matches!(n.op, Op::Guard { bci } if Some(bci) == header_bci)
                    && n.input_opt(0) == Some(entry_pred)
            })
            .count();
        let mut class_tests = 0usize;
        let mut ref_reads = 0usize;
        for &id in &ids {
            let Some(n) = graph.node_opt(id) else {
                continue;
            };
            match n.op {
                Op::If | Op::Guard { .. } => {
                    if n.input_opt(1)
                        .and_then(|c| graph.node_opt(c))
                        .is_some_and(|c| matches!(c.op, Op::ExactClassIs { .. }))
                    {
                        class_tests += 1;
                    }
                }
                Op::Load(crate::ir::MemKind::Ref) => ref_reads += 1,
                _ => {}
            }
        }
        eprintln!(
            "[cratonvm-jitc] ir-scan-loop {} header n{region} bci {:?} spliced={}: charAt calls={} \
             trapping={trapping} receiver proven={proven} elided={} entry guards={entry_guards} \
             class-test branches={class_tests} ref field reads={ref_reads}",
            if method_key.is_empty() {
                "?"
            } else {
                method_key
            },
            header_bci,
            super::header_is_spliced(graph, region),
            calls.len(),
            if elide { proven } else { 0 },
        );
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod unit_tests;
#[cfg(test)]
mod wave4_tests;

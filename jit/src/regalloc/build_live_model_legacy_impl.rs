#[allow(unused_imports)]
use super::*;

/// [`build_live_model_legacy_with`] with an optional [`LivenessScratch`]
/// (JIT round 19 wave 10, lane lv10). `None` is the legacy copy exactly.
pub(crate) fn build_live_model_legacy_impl(
    graph: &Graph,
    schedule: &Schedule,
    rules: LivenessRules,
    max_iterations: usize,
    fixed_point: LivenessFixedPoint,
    trace: Option<&mut LsLegacyRun>,
    mut scratch: Option<&mut LivenessScratch>,
) -> LiveModel {
    let token_edges_not_uses = rules.token_edges_not_uses;
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();

    // ── 1. Which values need a location ──────────────────────────────
    let wants_loc = live_model_wants_loc(graph, schedule);

    // ── 2. Linear positions and block spans ──────────────────────────
    // JIT round 19 wave 10 (lane lv10): from the per-compile scratch when
    // there is one (the same walk, `LsPositionModel::build`).
    let (pos_of, span, block_of_pos, total_positions) = match scratch.as_deref_mut() {
        Some(s) => {
            let m = s.positions(graph, schedule);
            (
                m.pos_of.clone(),
                m.span.clone(),
                m.block_of_pos.clone(),
                m.total_positions,
            )
        }
        None => live_model_positions_legacy(n, schedule),
    };

    // ── 3. Loop depth and per-position frequency weight ──────────────
    let loop_depth = ls_loop_depths(schedule);
    let pos_weight = |p: usize| -> u64 {
        let b = block_of_pos.get(p).copied().unwrap_or(0);
        let depth = loop_depth.get(b).copied().unwrap_or(0);
        LOOP_WEIGHT_PER_DEPTH
            .checked_pow(depth)
            .unwrap_or(MAX_LOOP_DEPTH_WEIGHT)
            .min(MAX_LOOP_DEPTH_WEIGHT) as u64
    };

    // ── 4. Pinned classes ────────────────────────────────────────────
    //
    // Same two sources `plan_slots` pins: phis (their home is what the edge
    // copies write, and `zero_ref_phi_slots` publishes `Ref` phi slots as GC
    // roots from the prologue) and every value a deopt frame names (the slot
    // must still hold the value at *any* recorded bci, which is not a property
    // a register allocator can establish).
    let pinned = live_model_pins(graph, &wants_loc);

    // ── 5. Uses, definitions and the backward fixed point ────────────
    let mut lo = vec![usize::MAX; n];
    let mut hi = vec![0usize; n];
    let mut uses: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut weight = vec![0u64; n];
    let mut converged = n.saturating_mul(nb) <= LIVE_MODEL_WORK_BUDGET;
    // Filled only once the fixed point has settled; see the field's doc.
    let mut block_live_in: Vec<Vec<u64>> = Vec::new();

    if converged {
        let words = n.div_ceil(64).max(1);
        // JIT round 19 wave 10 (lane lv10): pooled when a scratch is present.
        let mut def_bits = ls_zeroed(&mut scratch, words * nb);
        let mut use_bits = ls_zeroed(&mut scratch, words * nb);
        let mut phi_out_bits = ls_zeroed(&mut scratch, words * nb);

        // OD16-1 token-edge half (opt-in): `token_edges_not_uses` is the
        // caller's, read once per model by `build_live_model`.
        let mut local_def = ls_zeroed(&mut scratch, words);
        for b in 0..nb {
            let base = b * words;
            local_def.fill(0);
            let block = &schedule.blocks[b];
            for u in block.nodes.iter().copied().chain(block.terminator) {
                let ui = u as usize;
                let node = match graph.nodes.get(ui) {
                    Some(node) => node,
                    None => continue,
                };
                let upos = pos_of.get(ui).copied().flatten().unwrap_or(span[b].0);
                // A phi's value inputs are NOT read here — the predecessor's
                // edge copy reads them, below; `inputs[0]` is control.
                if !matches!(node.op, Op::Phi) {
                    for (slot, &inp) in node.inputs.iter().enumerate() {
                        let ii = inp as usize;
                        if inp == NO_NODE || ii >= n || !wants_loc[ii] {
                            continue;
                        }
                        // JIT round 17 wave 3 (lane irlower, OD16-1): an
                        // ordering edge, not a read. See
                        // `ls_token_edges_not_uses_enabled`.
                        if token_edges_not_uses && crate::ir::is_memory_token_slot(node, slot) {
                            continue;
                        }
                        lo[ii] = lo[ii].min(upos);
                        hi[ii] = hi[ii].max(upos);
                        uses[ii].push(upos);
                        weight[ii] = weight[ii].saturating_add(pos_weight(upos));
                        if !ls_bit_get(&local_def, ii) {
                            ls_bit_set(&mut use_bits[base..base + words], ii);
                        }
                    }
                }
                if wants_loc[ui] {
                    ls_bit_set(&mut local_def, ui);
                    ls_bit_set(&mut def_bits[base..base + words], ui);
                    lo[ui] = lo[ui].min(upos);
                    hi[ui] = hi[ui].max(upos);
                }
            }
        }

        // Phi edge copies: a phi's k-th value input is read at the outgoing-edge
        // position of the k-th predecessor, not at the phi.
        // Read ONCE per model, not once per phi per edge. It is an uncached
        // `runtime_var` read (a declared-name lookup plus an `OsString` clone),
        // and this loop runs `phis x predecessors` times on every IR compile —
        // the shape `flags::runtime_var_os`' read census exists to catch.
        // (Since wave 65 the read is the caller's, in `rules`.)
        let phi_edge_interfere = rules.phi_edge_interfere;
        // JIT round 19 wave 10 (lane lv10): one incoming edge's reads, as a
        // closure so the legacy walk below and the scratch's prebuilt table
        // (`LsPhiEdges`, the same edges in the same order) share it.
        let mut on_edge = |pred: usize, k: usize, phis: &[NodeId]| {
            let at = span[pred].1;
            for &pid in phis {
                let v = match graph
                    .nodes
                    .get(pid as usize)
                    .and_then(|p| p.inputs.get(k + 1))
                {
                    Some(&v) if v != NO_NODE => v,
                    _ => continue,
                };
                let vi = v as usize;
                if vi >= n || !wants_loc[vi] {
                    continue;
                }
                // JIT round 17 wave 3 (lane irlower, OD16-1): a memory φ
                // gets no edge copy (`ir_lower::gather_phi_copies`), so
                // its source is not read here. The φ's own interval below
                // is left exactly as it was.
                let source_unread = token_edges_not_uses
                    && graph
                        .nodes
                        .get(pid as usize)
                        .is_some_and(|p| p.ty == IrType::Memory);
                if !source_unread {
                    lo[vi] = lo[vi].min(at);
                    hi[vi] = hi[vi].max(at);
                    uses[vi].push(at);
                    weight[vi] = weight[vi].saturating_add(pos_weight(at));
                    ls_bit_set(&mut phi_out_bits[pred * words..(pred + 1) * words], vi);
                }

                // DEFAULT ON (`CRATONVM_JIT_IR_PHI_EDGE_INTERFERE=0` is the
                // kill switch -- see `phi_edge_interfere_enabled`, which is
                // `runtime_flag_default_on`): the phi is DEFINED at this
                // same position, so extend its interval to cover it and let
                // the allocator see that it overlaps every source read on
                // this edge.
                //
                // Without this the two do not interfere by the allocator's
                // reckoning -- a phi's range begins at its def in the merge
                // block, a dying source's ends here -- so one register can
                // serve both, and `ir_lower::emit_copy_op`'s publish then
                // overwrites a register a later copy still has to read.
                // That is the 2026-09-05 wrong-code bug two lanes fixed
                // downstream (`emit_phi_copies`'s deferral screen, and
                // `gp_reg_owner`'s reader interlock). This is the same bug
                // taken at the source, where the false "no interference"
                // is minted.
                //
                // What moves, and what deliberately does not.
                //
                // BOTH ENDS of the interval move: `lo` down to `at` and
                // `hi` up to it. For a forward edge `at` precedes the
                // phi's def and only `lo` changes; for a BACK edge `at` is
                // later than the def, so `hi` is what moves and the phi's
                // range is extended forward to the latch. Both directions
                // are the same statement -- the phi's location is spoken
                // for at the position its incoming copies run -- and both
                // are over-approximations, which is the safe direction for
                // an interference question. The loop-carried case is the
                // one that matters: that is exactly where a phi and the
                // source feeding it back are simultaneously live.
                //
                // (This paragraph used to say "only `lo` moves", which had
                // stopped describing the code. Corrected 2026-09; the flag
                // is default-ON, so the wrong reasoning was the live one.)
                //
                // What does NOT move: no USE is pushed -- the phi is not
                // read here, and a spurious use would distort the spill
                // heuristics that weight by use count -- and `phi_out_bits`
                // is NOT set for the phi, because that would make it
                // live-OUT of a block that does not define it, which the
                // backward dataflow below would propagate live-IN through
                // every predecessor and turn a bounded extension into a
                // whole-CFG one.
                if phi_edge_interfere {
                    let pi = pid as usize;
                    if pi < n && wants_loc[pi] {
                        lo[pi] = lo[pi].min(at);
                        hi[pi] = hi[pi].max(at);
                    }
                }
            }
        };
        match scratch.as_deref_mut() {
            Some(s) => {
                let edges = s.take_phi_edges(graph, schedule);
                for e in &edges.edges {
                    on_edge(e.pred, e.k, edges.phis_of(e));
                }
                s.put_phi_edges(edges);
            }
            None => {
                let mut phis_of: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
                for (id, node) in graph.nodes.iter().enumerate() {
                    if !matches!(node.op, Op::Phi) {
                        continue;
                    }
                    if let Some(&merge) = node.inputs.first() {
                        if merge != NO_NODE {
                            phis_of.entry(merge).or_default().push(id as NodeId);
                        }
                    }
                }
                for succ in 0..nb {
                    let merge_ctrl = schedule.blocks[succ].ctrl;
                    let merge_node = match graph.nodes.get(merge_ctrl as usize) {
                        Some(m) if matches!(m.op, Op::Merge | Op::Region) => m,
                        _ => continue,
                    };
                    let phis = match phis_of.get(&merge_ctrl) {
                        Some(phis) => phis,
                        None => continue,
                    };
                    for (k, &ctrl_in) in merge_node.inputs.iter().enumerate() {
                        let pred = match ls_ctrl_block_of(graph, schedule, ctrl_in) {
                            Some(pred) if pred < nb => pred,
                            _ => continue,
                        };
                        on_edge(pred, k, phis);
                    }
                }
            }
        }

        let mut live_in = ls_zeroed(&mut scratch, words * nb);
        let mut live_out = ls_zeroed(&mut scratch, words * nb);
        let mut visits = 0usize;
        let settled = match fixed_point {
            LivenessFixedPoint::RoundRobin => {
                let mut scratch = vec![0u64; words];
                let mut settled = false;
                for _ in 0..max_iterations {
                    let mut changed = false;
                    visits += nb;
                    for b in (0..nb).rev() {
                        let base = b * words;
                        scratch.copy_from_slice(&phi_out_bits[base..base + words]);
                        for &s in &schedule.blocks[b].successors {
                            if s >= nb {
                                continue;
                            }
                            for w in 0..words {
                                scratch[w] |= live_in[s * words + w];
                            }
                        }
                        for w in 0..words {
                            let merged_out = live_out[base + w] | scratch[w];
                            if merged_out != live_out[base + w] {
                                live_out[base + w] = merged_out;
                                changed = true;
                            }
                            let entering = use_bits[base + w] | (merged_out & !def_bits[base + w]);
                            let merged_in = live_in[base + w] | entering;
                            if merged_in != live_in[base + w] {
                                live_in[base + w] = merged_in;
                                changed = true;
                            }
                        }
                    }
                    if !changed {
                        settled = true;
                        break;
                    }
                }
                settled
            }
            // Interpreter round i2 wave 70, lane L2: the same answer, pass for
            // pass, visiting only dirty blocks (wave 71: the predecessor
            // lists per `fixed_point`). JIT round 19 wave 10 (lane lv10): the
            // round robin through the shared driver too.
            LivenessFixedPoint::Worklist
            | LivenessFixedPoint::WorklistSchedulePreds
            | LivenessFixedPoint::WorklistPostorder { .. }
            | LivenessFixedPoint::RoundRobinDriver => ls_fixed_point_on_schedule(
                schedule.blocks.as_slice(),
                fixed_point,
                LsFixedPointSets {
                    nb,
                    words,
                    phi_out: &phi_out_bits,
                    uses: &use_bits,
                    defs: &def_bits,
                },
                &mut live_in,
                &mut live_out,
                max_iterations,
                &mut visits,
            ),
        };
        converged = settled;
        if let Some(trace) = trace {
            *trace = LsLegacyRun {
                ran: true,
                converged: settled,
                live_in: live_in.clone(),
                live_out: live_out.clone(),
                visits,
            };
        }

        // Widen each range over every block it is live in. This is where loops
        // are handled: a value used inside a loop is live-out of every block on
        // the back edge's path, so its range covers the whole loop without the
        // model ever knowing what a loop is.
        if converged {
            block_live_in = (0..nb)
                .map(|b| live_in[b * words..(b + 1) * words].to_vec())
                .collect();
            for b in 0..nb {
                let base = b * words;
                let (start, end) = span[b];
                ls_bits_for_each(&live_in[base..base + words], |v| {
                    if v < n && wants_loc[v] {
                        lo[v] = lo[v].min(start);
                    }
                });
                ls_bits_for_each(&live_out[base..base + words], |v| {
                    if v < n && wants_loc[v] {
                        hi[v] = hi[v].max(end);
                    }
                });
            }
        }
        // JIT round 19 wave 10 (lane lv10): back to the pool, if any.
        for buf in [
            def_bits,
            use_bits,
            phi_out_bits,
            local_def,
            live_in,
            live_out,
        ] {
            ls_give(&mut scratch, buf);
        }
    }

    finish_live_model(
        graph,
        schedule,
        LiveModelHead {
            pos_of,
            span,
            total_positions,
            block_of_pos,
            wants_loc,
            pinned,
            lo,
            hi,
            uses,
            weight,
            loop_depth,
            converged,
            block_live_in,
        },
    )
}

/// What [`finish_live_model`] needs from the analysis half of a
/// [`LiveModel`] build: the legacy copy ([`build_live_model_legacy`]) and the
/// shared core ([`build_live_model_from_core`]) both produce one.
pub(in crate::regalloc) struct LiveModelHead {
    pub(in crate::regalloc) pos_of: Vec<Option<usize>>,
    pub(in crate::regalloc) span: Vec<(usize, usize)>,
    pub(in crate::regalloc) total_positions: usize,
    pub(in crate::regalloc) block_of_pos: Vec<usize>,
    pub(in crate::regalloc) wants_loc: Vec<bool>,
    pub(in crate::regalloc) pinned: Vec<bool>,
    /// Per value, `usize::MAX` when nothing placed it.
    pub(in crate::regalloc) lo: Vec<usize>,
    pub(in crate::regalloc) hi: Vec<usize>,
    /// Unsorted, possibly repeated read positions.
    pub(in crate::regalloc) uses: Vec<Vec<usize>>,
    pub(in crate::regalloc) weight: Vec<u64>,
    pub(in crate::regalloc) loop_depth: Vec<u32>,
    pub(in crate::regalloc) converged: bool,
    pub(in crate::regalloc) block_live_in: Vec<Vec<u64>>,
}

/// [`build_live_model_from_core`]: the model over [`liveness_core`]'s answer.
/// Uses and weights are a layer on top of the core's read list; ranges,
/// positions and the live-in sets are the core's.
pub(in crate::regalloc) fn build_live_model_from_core(
    graph: &Graph,
    schedule: &Schedule,
    rules: LivenessRules,
) -> LiveModel {
    let wants_loc = live_model_wants_loc(graph, schedule);
    let core = liveness_core(
        graph,
        schedule,
        &wants_loc,
        LivenessRules {
            collect_reads: true,
            ..rules
        },
        LIVE_MODEL_WORK_BUDGET,
        LIVE_MODEL_MAX_ITERATIONS,
    );
    live_model_from_core(graph, schedule, wants_loc, core)
}

/// Build allocation liveness from the same selected uses as the frame plan.
pub(crate) fn build_live_model_with_value_flow(
    graph: &Graph, schedule: &Schedule, flow: &ValueFlow,
) -> LiveModel {
    let mut wants = live_model_wants_loc(graph, schedule);
    flow.apply_wants(&mut wants);
    let core = liveness_core_with_value_flow(graph, schedule, &wants, live_model_rules(),
        LIVE_MODEL_WORK_BUDGET, LIVE_MODEL_MAX_ITERATIONS, flow);
    live_model_from_core(graph, schedule, wants, core)
}

/// The live model over a [`liveness_core`] answer the caller already holds
/// (interpreter round i2 wave 66, lane L2; split out of
/// [`build_live_model_from_core`] so the lowering entry can run the core once
/// per compile, [`one_liveness_pass_enabled`]).
///
/// `wants_loc` is the graph value set, filtered by the selected value flow when
/// present. `core` must describe that same value set and operand flow and have
/// been computed over it with [`LivenessRules::collect_reads`] on and the
/// live model's work budget and iteration cap; the uses and weights are built
/// from `core.reads`, which is empty otherwise.
pub(crate) fn live_model_from_core(
    graph: &Graph,
    schedule: &Schedule,
    wants_loc: Vec<bool>,
    core: LivenessCore,
) -> LiveModel {
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();
    let block_of_pos = block_of_pos_from_spans(&core.span);
    let loop_depth = ls_loop_depths(schedule);
    let pos_weight = |p: usize| -> u64 {
        let b = block_of_pos.get(p).copied().unwrap_or(0);
        let depth = loop_depth.get(b).copied().unwrap_or(0);
        LOOP_WEIGHT_PER_DEPTH
            .checked_pow(depth)
            .unwrap_or(MAX_LOOP_DEPTH_WEIGHT)
            .min(MAX_LOOP_DEPTH_WEIGHT) as u64
    };
    let pinned = live_model_pins(graph, &wants_loc);
    let mut uses: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut weight = vec![0u64; n];
    for &(v, at) in &core.reads {
        let vi = v as usize;
        if let (Some(list), Some(w)) = (uses.get_mut(vi), weight.get_mut(vi)) {
            list.push(at);
            *w = w.saturating_add(pos_weight(at));
        }
    }
    let words = core.words;
    let block_live_in: Vec<Vec<u64>> = if core.converged && core.live_in.len() == words * nb {
        (0..nb)
            .map(|b| core.live_in[b * words..(b + 1) * words].to_vec())
            .collect()
    } else {
        Vec::new()
    };
    let LivenessCore {
        pos_of,
        span,
        total_positions,
        lo,
        hi,
        converged,
        ..
    } = core;
    finish_live_model(
        graph,
        schedule,
        LiveModelHead {
            pos_of,
            span,
            total_positions,
            block_of_pos,
            wants_loc,
            pinned,
            lo,
            hi,
            uses,
            weight,
            loop_depth,
            converged,
            block_live_in,
        },
    )
}

/// `block_of_pos[p]` from the block spans: block `b` owns `span[b].0 ..=
/// span[b].1` (its nodes, its terminator and its edge position), and the
/// spans are contiguous in block order.
pub(in crate::regalloc) fn block_of_pos_from_spans(span: &[(usize, usize)]) -> Vec<usize> {
    let mut out = Vec::with_capacity(span.last().map_or(0, |&(_, e)| e + 1));
    for (b, &(start, end)) in span.iter().enumerate() {
        for _ in start..=end {
            out.push(b);
        }
    }
    out
}

/// The rules [`liveness_core`] applies on top of "every input is a read".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LivenessRules {
    /// A memory-token input (`ir::is_memory_token_slot`) and a memory φ's
    /// incoming edge are not reads (`CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES`,
    /// `CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES`).
    pub(crate) token_edges_not_uses: bool,
    /// A φ's interval covers each incoming edge position whose source is a
    /// located value (`CRATONVM_JIT_IR_PHI_EDGE_INTERFERE`; see the comment at
    /// its use in [`build_live_model_legacy`]).
    pub(crate) phi_edge_interfere: bool,
    /// Record every read in [`LivenessCore::reads`] (the live model's uses
    /// and weights). Off for the slot plan, which has no use for them.
    pub(crate) collect_reads: bool,
}

/// [`liveness_core`]'s answer: the position model and the value intervals
/// both `ir_lower::plan_slots` and [`build_live_model`] are built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LivenessCore {
    /// The linear emission position of each node, `None` when unscheduled.
    pub(crate) pos_of: Vec<Option<usize>>,
    /// `(first position, outgoing-edge position)` per block.
    pub(crate) span: Vec<(usize, usize)>,
    /// One past the last position.
    pub(crate) total_positions: usize,
    /// Per value: the lowest position it is placed, read, or live at;
    /// `usize::MAX` when nothing placed it. Meaningful only when `converged`.
    pub(crate) lo: Vec<usize>,
    /// Per value: the highest such position (0 when nothing placed it).
    pub(crate) hi: Vec<usize>,
    /// Per value: `lo` was `usize::MAX` until the φ edge rule set it, i.e. a
    /// φ no code reads, that no block defines and no block has live, whose
    /// interval is its incoming edges alone. All false without the rule.
    /// `ir_lower`'s `phi_code_unused` asks the pre-rule question.
    pub(crate) set_only_by_phi_rule: Vec<bool>,
    /// The fixed point ran (the graph was within the work budget) and settled.
    pub(crate) converged: bool,
    /// Words per block bit set: `max(1, ceil(nodes / 64))`.
    pub(crate) words: usize,
    /// `words × blocks` live-in bits, flattened; empty when the work budget
    /// skipped the analysis. Valid only when `converged`.
    pub(crate) live_in: Vec<u64>,
    /// `words × blocks` live-out bits, as `live_in`.
    pub(crate) live_out: Vec<u64>,
    /// `(value, position)` for every read, in visiting order, repeats kept;
    /// only with [`LivenessRules::collect_reads`].
    pub(crate) reads: Vec<(NodeId, usize)>,
}

impl LivenessCore {
    /// The part of this answer `ir_lower::plan_slots` reads: positions, spans,
    /// ranges, the φ-rule bit and the convergence flag, copied; the bit sets
    /// and the read list left empty (interpreter round i2 wave 66, lane L2,
    /// [`one_liveness_pass_enabled`]). `O(nodes + blocks)`; the fixed point is
    /// not rerun. The slot plan consumes its core by value, and the live model
    /// keeps the original.
    pub(crate) fn slot_view(&self) -> LivenessCore {
        LivenessCore {
            pos_of: self.pos_of.clone(),
            span: self.span.clone(),
            total_positions: self.total_positions,
            lo: self.lo.clone(),
            hi: self.hi.clone(),
            set_only_by_phi_rule: self.set_only_by_phi_rule.clone(),
            converged: self.converged,
            words: self.words,
            live_in: Vec::new(),
            live_out: Vec::new(),
            reads: Vec::new(),
        }
    }
}

/// One liveness computation for one scheduled graph, over the value set
/// `wants` (interpreter round i2 wave 65, lane L2,
/// `i64-L2-proposal-one-liveness-core-for-plan-slots-and-the-live-model`).
///
/// The position numbering (each scheduled node, then the terminator, then one
/// outgoing-edge position per block), the upward-exposed use pass (a φ's
/// value inputs are read at its predecessors' edge positions, not at the φ),
/// the backward fixed point in reverse block order, and the per-block range
/// widening -- the analysis `ir_lower::plan_slots` and
/// [`build_live_model_legacy`] each spelled out by hand. `rules` holds the
/// two places they may differ.
///
/// Total: never panics. A graph past `work_budget` (`nodes × blocks`) is not
/// analysed; a fixed point that has not settled after `max_iterations` passes
/// is reported unconverged. Either way the caller treats every range as the
/// whole method.
///
/// Interpreter round i2 wave 67, lane L2: with
/// [`liveness_dense_values_enabled`] (or [`liveness_budget_by_values_enabled`])
/// the bit sets are indexed by a dense value numbering
/// ([`liveness_core_dense`]); the answer is the same.
pub(crate) fn liveness_core(
    graph: &Graph, schedule: &Schedule, wants: &[bool], rules: LivenessRules,
    work_budget: usize, max_iterations: usize,
) -> LivenessCore {
    liveness_core_using(graph, schedule, wants, rules, work_budget, max_iterations, &GraphValues)
}

pub(crate) fn liveness_core_with_value_flow(
    graph: &Graph, schedule: &Schedule, wants: &[bool], rules: LivenessRules,
    work_budget: usize, max_iterations: usize, flow: &ValueFlow,
) -> LivenessCore {
    liveness_core_using(graph, schedule, wants, rules, work_budget, max_iterations, flow)
}

pub(in crate::regalloc) fn liveness_core_using<S: ValueSource>(
    graph: &Graph, schedule: &Schedule, wants: &[bool], rules: LivenessRules,
    work_budget: usize, max_iterations: usize, source: &S,
) -> LivenessCore {
    let budget_by_values = liveness_budget_by_values_enabled();
    let dense = budget_by_values || liveness_dense_values_enabled();
    let fixed_point = if liveness_worklist_enabled() {
        worklist_fixed_point_for(schedule.blocks.len())
    } else { LivenessFixedPoint::RoundRobin };
    if dense {
        liveness_core_body_using::<true, _>(graph, schedule, wants, rules, work_budget,
            max_iterations, budget_by_values, fixed_point, &mut 0usize, source)
    } else {
        liveness_core_body_using::<false, _>(graph, schedule, wants, rules, work_budget,
            max_iterations, budget_by_values, fixed_point, &mut 0usize, source)
    }
}

/// How [`liveness_core`] runs its backward fixed point (interpreter round i2
/// wave 69, lane L2). Both give the same answer, pass for pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LivenessFixedPoint {
    /// Every block, every pass ([`ls_fixed_point_round_robin`]).
    #[default]
    RoundRobin,
    /// Only the blocks a successor's live-in change made dirty, in the same
    /// pass order ([`ls_fixed_point_worklist`]).
    Worklist,
    /// [`Self::Worklist`] reading a schedule's own `predecessors` lists
    /// instead of building a predecessor index per call (interpreter round
    /// i3 wave 71, lane L2, [`liveness_schedule_preds_enabled`]). Over a bare
    /// adjacency list (the tests and the bench) the index is built once by
    /// the caller ([`LsPredIndex`]). Same answer, pass for pass.
    WorklistSchedulePreds,
    /// The worklist with its passes in CFG postorder ([`ls_postorder`],
    /// [`ls_fixed_point_worklist_ordered`]; interpreter round i3 wave 71,
    /// lane L2, [`liveness_postorder_enabled`]), the predecessor lists the
    /// schedule's own when `schedule_preds`. The same converged answer; the
    /// pass count, and so a cut-off run, can differ.
    WorklistPostorder { schedule_preds: bool },
    /// [`Self::RoundRobin`] through the shared driver
    /// ([`ls_fixed_point_round_robin`]) in the legacy copies, which otherwise
    /// run their own hand-written whole-pass loop for `RoundRobin` (JIT round
    /// 19 wave 10, lane lv10, [`liveness_one_driver_enabled`]). The same
    /// answer and visit count, pass for pass. Everywhere else it is
    /// `RoundRobin`.
    RoundRobinDriver,
}

/// [`liveness_core`] with every choice explicit, whatever the switches say:
/// the dense numbering (`dense`), the value budget (`budget_by_values`,
/// implies nothing here: pass `dense` too, as [`liveness_core`] does) and the
/// fixed point. Also returns the number of block visits the fixed point made
/// (0 when the work budget skipped it), for the tests and the bench.
#[allow(clippy::too_many_arguments)]
pub(crate) fn liveness_core_with(
    graph: &Graph,
    schedule: &Schedule,
    wants: &[bool],
    rules: LivenessRules,
    work_budget: usize,
    max_iterations: usize,
    dense: bool,
    budget_by_values: bool,
    fixed_point: LivenessFixedPoint,
) -> (LivenessCore, usize) {
    let mut visits = 0usize;
    let core = if dense {
        liveness_core_body::<true>(
            graph,
            schedule,
            wants,
            rules,
            work_budget,
            max_iterations,
            budget_by_values,
            fixed_point,
            &mut visits,
        )
    } else {
        liveness_core_body::<false>(
            graph,
            schedule,
            wants,
            rules,
            work_budget,
            max_iterations,
            budget_by_values,
            fixed_point,
            &mut visits,
        )
    };
    (core, visits)
}

/// [`liveness_core`] with its bit sets indexed by node id, `ceil(nodes / 64)`
/// words per block: the pre-wave-67 core, whatever the switches say.
pub(crate) fn liveness_core_sparse(
    graph: &Graph,
    schedule: &Schedule,
    wants: &[bool],
    rules: LivenessRules,
    work_budget: usize,
    max_iterations: usize,
) -> LivenessCore {
    liveness_core_body::<false>(
        graph,
        schedule,
        wants,
        rules,
        work_budget,
        max_iterations,
        false,
        LivenessFixedPoint::RoundRobin,
        &mut 0usize,
    )
}

/// [`liveness_core`] with its bit sets indexed by a dense numbering of the
/// wanted values, `ceil(values / 64)` words per block, whatever the switches
/// say (interpreter round i2 wave 67, lane L2). `budget_by_values` makes the
/// work budget count `values × blocks` (a behaviour change; false gives the
/// sparse core's answer exactly).
pub(crate) fn liveness_core_dense(
    graph: &Graph,
    schedule: &Schedule,
    wants: &[bool],
    rules: LivenessRules,
    work_budget: usize,
    max_iterations: usize,
    budget_by_values: bool,
) -> LivenessCore {
    liveness_core_body::<true>(
        graph,
        schedule,
        wants,
        rules,
        work_budget,
        max_iterations,
        budget_by_values,
        LivenessFixedPoint::RoundRobin,
        &mut 0usize,
    )
}

/// Reads a block's successor list: the schedule's blocks, or (in the tests
/// and the bench) a bare adjacency list (interpreter round i2 wave 69, lane
/// L2). An index `>= nb` is ignored by both fixed points.
pub(crate) trait LsSuccessors {
    fn ls_successors(&self, b: usize) -> &[usize];
}

impl LsSuccessors for [crate::ir_schedule::Block] {
    #[inline]
    fn ls_successors(&self, b: usize) -> &[usize] {
        &self[b].successors
    }
}

impl LsSuccessors for [Vec<usize>] {
    #[inline]
    fn ls_successors(&self, b: usize) -> &[usize] {
        &self[b]
    }
}

/// The per-block sets the backward fixed point runs over, `words` per block,
/// `nb` blocks, flattened: what a block's outgoing edge reads (`phi_out`),
/// its upward-exposed reads (`uses`) and its definitions (`defs`).
#[derive(Clone, Copy)]
pub(crate) struct LsFixedPointSets<'a> {
    pub(crate) nb: usize,
    pub(crate) words: usize,
    pub(crate) phi_out: &'a [u64],
    pub(crate) uses: &'a [u64],
    pub(crate) defs: &'a [u64],
}

/// One block's transfer: `lout |= phi_out | li[succ]…`, `li |= uses | (lout &
/// !defs)`. Returns `(lout changed, li changed)`. `scratch` is `words` long.
#[inline]
pub(in crate::regalloc) fn ls_fixed_point_visit<S: LsSuccessors + ?Sized>(
    succ: &S,
    sets: LsFixedPointSets<'_>,
    b: usize,
    li: &mut [u64],
    lout: &mut [u64],
    scratch: &mut [u64],
) -> (bool, bool) {
    let (nb, words) = (sets.nb, sets.words);
    let base = b * words;
    scratch.copy_from_slice(&sets.phi_out[base..base + words]);
    for &s in succ.ls_successors(b) {
        if s >= nb {
            continue;
        }
        for w in 0..words {
            scratch[w] |= li[s * words + w];
        }
    }
    let (mut out_changed, mut in_changed) = (false, false);
    for w in 0..words {
        let merged_out = lout[base + w] | scratch[w];
        if merged_out != lout[base + w] {
            lout[base + w] = merged_out;
            out_changed = true;
        }
        let entering = sets.uses[base + w] | (merged_out & !sets.defs[base + w]);
        let merged_in = li[base + w] | entering;
        if merged_in != li[base + w] {
            li[base + w] = merged_in;
            in_changed = true;
        }
    }
    (out_changed, in_changed)
}

/// The backward fixed point as whole passes: each pass visits every block in
/// reverse index order; it stops after the first pass that changes nothing
/// (`true`), or after `max_iterations` passes (`false`, unconverged). `li` and
/// `lout` start as given (all zero in the core). Counts block visits.
#[inline]
pub(crate) fn ls_fixed_point_round_robin<S: LsSuccessors + ?Sized>(
    succ: &S,
    sets: LsFixedPointSets<'_>,
    li: &mut [u64],
    lout: &mut [u64],
    max_iterations: usize,
    visits: &mut usize,
) -> bool {
    let mut scratch = vec![0u64; sets.words];
    for _ in 0..max_iterations {
        let mut changed = false;
        for b in (0..sets.nb).rev() {
            *visits += 1;
            let (out_changed, in_changed) =
                ls_fixed_point_visit(succ, sets, b, li, lout, &mut scratch);
            changed |= out_changed | in_changed;
        }
        if !changed {
            return true;
        }
    }
    false
}

/// The highest set bit strictly below `limit`, if any.
#[inline]
pub(in crate::regalloc) fn ls_highest_set_below(bits: &[u64], limit: usize) -> Option<usize> {
    if limit == 0 {
        return None;
    }
    let last = limit - 1;
    let mut wi = last / 64;
    let keep = last % 64 + 1;
    let mask = if keep >= 64 {
        u64::MAX
    } else {
        (1u64 << keep) - 1
    };
    let mut word = bits.get(wi).copied().unwrap_or(0) & mask;
    loop {
        if word != 0 {
            return Some(wi * 64 + 63 - word.leading_zeros() as usize);
        }
        if wi == 0 {
            return None;
        }
        wi -= 1;
        word = bits.get(wi).copied().unwrap_or(0);
    }
}

/// [`ls_fixed_point_round_robin`] that skips clean blocks (interpreter round
/// i2 wave 69, lane L2, [`liveness_worklist_enabled`]).
///
/// A block is dirty until visited; visiting it clears it, and a change to its
/// `li` dirties each predecessor (from the successor lists, inverted here,
/// self loops and duplicate edges included). Each pass still runs from the
/// highest block index down, visiting only dirty blocks: a predecessor below
/// the changed block is visited later in the same pass, one at or above it in
/// the next, which is exactly when the round-robin pass would first see the
/// change. A clean block's successors' `li` are unchanged since its last
/// visit, so recomputing it would change nothing. Hence the state after each
/// pass is the round-robin state after that pass, and so are the pass count,
/// the convergence verdict (a pass with nothing dirty changes nothing) and
/// the sets of a cut-off run.
pub(crate) fn ls_fixed_point_worklist<S: LsSuccessors + ?Sized>(
    succ: &S,
    sets: LsFixedPointSets<'_>,
    li: &mut [u64],
    lout: &mut [u64],
    max_iterations: usize,
    visits: &mut usize,
) -> bool {
    let preds = LsPredIndex::build(succ, sets.nb);
    ls_fixed_point_worklist_preds(succ, &preds, sets, li, lout, max_iterations, visits)
}

/// Reads a block's predecessor list for [`ls_fixed_point_worklist_preds`]
/// (interpreter round i3 wave 71, lane L2). Entries `>= nb` are ignored;
/// duplicates and extra entries cost visits, never the answer; a MISSING
/// predecessor would (a block would not be revisited when a successor's
/// live-in grows), so an implementation must list at least every `p` with
/// `b` among `p`'s in-range successors.
pub(crate) trait LsPredecessors {
    fn ls_predecessors(&self, b: usize) -> &[usize];
}

/// The predecessor index inverted from successor lists, CSR (what
/// [`ls_fixed_point_worklist`] builds on every call; interpreter round i3
/// wave 71, lane L2 split it out so a caller can build it once).
pub(crate) struct LsPredIndex {
    pub(in crate::regalloc) start: Vec<usize>,
    pub(in crate::regalloc) preds: Vec<usize>,
}

impl LsPredIndex {
    /// Invert `succ`'s first `nb` lists, ignoring successors `>= nb`; self
    /// loops and duplicate edges kept.
    pub(crate) fn build<S: LsSuccessors + ?Sized>(succ: &S, nb: usize) -> Self {
        let mut start = vec![0usize; nb + 1];
        for b in 0..nb {
            for &s in succ.ls_successors(b) {
                if s < nb {
                    start[s + 1] += 1;
                }
            }
        }
        for b in 0..nb {
            start[b + 1] += start[b];
        }
        let mut fill = start.clone();
        let mut preds = vec![0usize; start[nb]];
        for b in 0..nb {
            for &s in succ.ls_successors(b) {
                if s < nb {
                    preds[fill[s]] = b;
                    fill[s] += 1;
                }
            }
        }
        LsPredIndex { start, preds }
    }
}

impl LsPredecessors for LsPredIndex {
    #[inline]
    fn ls_predecessors(&self, b: usize) -> &[usize] {
        match (self.start.get(b), self.start.get(b + 1)) {
            (Some(&lo), Some(&hi)) => &self.preds[lo..hi],
            _ => &[],
        }
    }
}

/// A schedule's own predecessor lists (`Block::predecessors`), which
/// `ir_schedule` pushes beside every successor edge and remaps with them in
/// the layout pass (interpreter round i3 wave 71, lane L2,
/// [`liveness_schedule_preds_enabled`]).
pub(crate) struct LsSchedulePreds<'a>(pub(crate) &'a [crate::ir_schedule::Block]);

impl LsPredecessors for LsSchedulePreds<'_> {
    #[inline]
    fn ls_predecessors(&self, b: usize) -> &[usize] {
        match self.0.get(b) {
            Some(block) => block.predecessors.as_slice(),
            None => &[],
        }
    }
}

/// The fixed point `fixed_point` names over a schedule's blocks: the round
/// robin, the worklist with a per-call predecessor index, or the worklist
/// over the blocks' own predecessor lists (interpreter round i3 wave 71,
/// lane L2). The one entry the worklist arms of every liveness copy call.
pub(crate) fn ls_fixed_point_on_schedule(
    blocks: &[crate::ir_schedule::Block],
    fixed_point: LivenessFixedPoint,
    sets: LsFixedPointSets<'_>,
    li: &mut [u64],
    lout: &mut [u64],
    max_iterations: usize,
    visits: &mut usize,
) -> bool {
    match fixed_point {
        LivenessFixedPoint::RoundRobin | LivenessFixedPoint::RoundRobinDriver => {
            ls_fixed_point_round_robin(blocks, sets, li, lout, max_iterations, visits)
        }
        LivenessFixedPoint::Worklist => {
            ls_fixed_point_worklist(blocks, sets, li, lout, max_iterations, visits)
        }
        LivenessFixedPoint::WorklistSchedulePreds => ls_fixed_point_worklist_preds(
            blocks,
            &LsSchedulePreds(blocks),
            sets,
            li,
            lout,
            max_iterations,
            visits,
        ),
        LivenessFixedPoint::WorklistPostorder { schedule_preds } => {
            let (order, rank) = ls_postorder(blocks, sets.nb);
            if schedule_preds {
                ls_fixed_point_worklist_ordered(
                    blocks,
                    &LsSchedulePreds(blocks),
                    &order,
                    &rank,
                    sets,
                    li,
                    lout,
                    max_iterations,
                    visits,
                )
            } else {
                let preds = LsPredIndex::build(blocks, sets.nb);
                ls_fixed_point_worklist_ordered(
                    blocks,
                    &preds,
                    &order,
                    &rank,
                    sets,
                    li,
                    lout,
                    max_iterations,
                    visits,
                )
            }
        }
    }
}

/// A pass order for the backward fixed point (interpreter round i3 wave 71,
/// lane L2, [`liveness_postorder_enabled`]): `order[k]` is the block a pass
/// visits at step `nb - 1 - k` (the drivers walk `k` downward), `rank` its
/// inverse. The visiting sequence is a DFS postorder over the in-range
/// successors, from block 0 first, then from each block not yet reached in
/// index order (unreachable code, which no reachable block depends on): every
/// block after its DFS successors, so only retreating (loop) edges carry a
/// change into a later pass.
pub(crate) fn ls_postorder<S: LsSuccessors + ?Sized>(
    succ: &S,
    nb: usize,
) -> (Vec<usize>, Vec<usize>) {
    let mut seen = vec![false; nb];
    let mut post: Vec<usize> = Vec::with_capacity(nb);
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for root in 0..nb {
        if seen[root] {
            continue;
        }
        seen[root] = true;
        stack.push((root, 0));
        while let Some(top) = stack.last_mut() {
            let (b, next) = *top;
            let succs = succ.ls_successors(b);
            if next < succs.len() {
                top.1 += 1;
                let s = succs[next];
                if s < nb && !seen[s] {
                    seen[s] = true;
                    stack.push((s, 0));
                }
            } else {
                post.push(b);
                stack.pop();
            }
        }
    }
    // The driver visits `order[nb - 1]` first: the first block finished.
    let mut order = vec![0usize; nb];
    let mut rank = vec![0usize; nb];
    for (i, &b) in post.iter().enumerate() {
        let k = nb - 1 - i;
        order[k] = b;
        rank[b] = k;
    }
    (order, rank)
}

/// [`ls_fixed_point_worklist_preds`] with the pass order given (interpreter
/// round i3 wave 71, lane L2): each pass visits the dirty blocks by
/// decreasing `rank` (`order[rank[b]] == b`, a permutation of `0..nb`).
/// The same argument as the index-order worklist makes it the round robin in
/// that order, pass for pass; with the identity order it IS the index-order
/// worklist. Any order converges to the same least fixed point.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ls_fixed_point_worklist_ordered<S, P>(
    succ: &S,
    preds: &P,
    order: &[usize],
    rank: &[usize],
    sets: LsFixedPointSets<'_>,
    li: &mut [u64],
    lout: &mut [u64],
    max_iterations: usize,
    visits: &mut usize,
) -> bool
where
    S: LsSuccessors + ?Sized,
    P: LsPredecessors + ?Sized,
{
    let nb = sets.nb.min(order.len()).min(rank.len());
    let mut dirty_one = [0u64; 1];
    let mut dirty_many: Vec<u64>;
    let dirty: &mut [u64] = if nb <= 64 {
        &mut dirty_one[..]
    } else {
        dirty_many = vec![0u64; nb.div_ceil(64)];
        &mut dirty_many[..]
    };
    for k in 0..nb {
        ls_bit_set(&mut *dirty, k);
    }
    let mut scratch = vec![0u64; sets.words];
    for _ in 0..max_iterations {
        let mut changed = false;
        let mut limit = nb;
        while let Some(k) = ls_highest_set_below(&*dirty, limit) {
            limit = k;
            dirty[k / 64] &= !(1u64 << (k % 64));
            let b = order[k];
            if b >= nb {
                continue;
            }
            *visits += 1;
            let (out_changed, in_changed) =
                ls_fixed_point_visit(succ, sets, b, li, lout, &mut scratch);
            changed |= out_changed | in_changed;
            if in_changed {
                for &p in preds.ls_predecessors(b) {
                    if p < nb {
                        ls_bit_set(&mut *dirty, rank[p]);
                    }
                }
            }
        }
        if !changed {
            return true;
        }
    }
    false
}

/// [`ls_fixed_point_worklist`] over a predecessor index the caller supplies
/// (interpreter round i3 wave 71, lane L2). The dirty set lives on the stack
/// for graphs of up to 64 blocks (one word), so on method-sized graphs the
/// driver allocates only the transfer scratch, as the round robin does.
pub(crate) fn ls_fixed_point_worklist_preds<S, P>(
    succ: &S,
    preds: &P,
    sets: LsFixedPointSets<'_>,
    li: &mut [u64],
    lout: &mut [u64],
    max_iterations: usize,
    visits: &mut usize,
) -> bool
where
    S: LsSuccessors + ?Sized,
    P: LsPredecessors + ?Sized,
{
    let nb = sets.nb;
    let mut dirty_one = [0u64; 1];
    let mut dirty_many: Vec<u64>;
    let dirty: &mut [u64] = if nb <= 64 {
        &mut dirty_one[..]
    } else {
        dirty_many = vec![0u64; nb.div_ceil(64)];
        &mut dirty_many[..]
    };
    for b in 0..nb {
        ls_bit_set(&mut *dirty, b);
    }
    let mut scratch = vec![0u64; sets.words];
    for _ in 0..max_iterations {
        let mut changed = false;
        let mut limit = nb;
        while let Some(b) = ls_highest_set_below(&*dirty, limit) {
            limit = b;
            dirty[b / 64] &= !(1u64 << (b % 64));
            *visits += 1;
            let (out_changed, in_changed) =
                ls_fixed_point_visit(succ, sets, b, li, lout, &mut scratch);
            changed |= out_changed | in_changed;
            if in_changed {
                for &p in preds.ls_predecessors(b) {
                    if p < nb {
                        ls_bit_set(&mut *dirty, p);
                    }
                }
            }
        }
        if !changed {
            return true;
        }
    }
    false
}

/// The lowest set bit at or above `from` and below `limit`, if any.
#[inline]
pub(in crate::regalloc) fn ls_lowest_set_from(bits: &[u64], from: usize, limit: usize) -> Option<usize> {
    if from >= limit {
        return None;
    }
    let mut wi = from / 64;
    let mut word = bits.get(wi).copied().unwrap_or(0) & (u64::MAX << (from % 64));
    loop {
        if word != 0 {
            let b = wi * 64 + word.trailing_zeros() as usize;
            return (b < limit).then_some(b);
        }
        wi += 1;
        if wi * 64 >= limit {
            return None;
        }
        word = bits.get(wi).copied().unwrap_or(0);
    }
}

/// A dirty set of `nb` bits, on the stack up to 64 blocks.
pub(in crate::regalloc) enum LsDirty {
    One([u64; 1]),
    Many(Vec<u64>),
}

impl LsDirty {
    pub(in crate::regalloc) fn all(nb: usize) -> Self {
        let mut d = if nb <= 64 {
            LsDirty::One([0u64; 1])
        } else {
            LsDirty::Many(vec![0u64; nb.div_ceil(64)])
        };
        for b in 0..nb {
            ls_bit_set(d.bits(), b);
        }
        d
    }

    #[inline]
    pub(in crate::regalloc) fn bits(&mut self) -> &mut [u64] {
        match self {
            LsDirty::One(w) => &mut w[..],
            LsDirty::Many(v) => &mut v[..],
        }
    }
}

/// The backward dirty-block driver with the transfer as a closure (JIT round
/// 19 proposal wave 10, lane lv10;
/// `i70-L2-proposal-closure-transfer-dirty-driver-for-the-dead-ref-planner`,
/// `CRATONVM_JIT_IR_DEAD_REF_LIVENESS_WORKLIST`).
///
/// `visit(b)` recomputes block `b` from its own state and its successors'
/// published sets and returns whether `b`'s published set changed. Each pass
/// visits the dirty blocks from the highest index down; a change dirties
/// `b`'s predecessors (`preds`). The argument of [`ls_fixed_point_worklist`]
/// holds for any such transfer: a clean block's successors have not changed
/// since its last visit, so recomputing it would change nothing, and the
/// state after every pass, the pass count and the verdict are the whole-pass
/// loop's (`for b in (0..nb).rev()`, stop after a pass with no change, at
/// most `max_iterations` passes), converged or cut off.
pub(crate) fn ls_closure_fixed_point_backward<P: LsPredecessors + ?Sized>(
    preds: &P,
    nb: usize,
    max_iterations: usize,
    visits: &mut usize,
    mut visit: impl FnMut(usize) -> bool,
) -> bool {
    let mut dirty = LsDirty::all(nb);
    for _ in 0..max_iterations {
        let mut changed = false;
        let mut limit = nb;
        while let Some(b) = ls_highest_set_below(dirty.bits(), limit) {
            limit = b;
            dirty.bits()[b / 64] &= !(1u64 << (b % 64));
            *visits += 1;
            if visit(b) {
                changed = true;
                for &p in preds.ls_predecessors(b) {
                    if p < nb {
                        ls_bit_set(dirty.bits(), p);
                    }
                }
            }
        }
        if !changed {
            return true;
        }
    }
    false
}

/// The forward mirror of [`ls_closure_fixed_point_backward`] (JIT round 19
/// wave 10, lane lv10): `visit(b)` recomputes `b` from its predecessors'
/// published sets; each pass visits the dirty blocks from the LOWEST index
/// up, and a change dirties `b`'s successors (`succs`, read through
/// [`LsPredecessors`] as a plain per-block list). The same pass-for-pass
/// argument makes it the whole-pass loop `for b in 0..nb`.
pub(crate) fn ls_closure_fixed_point_forward<S: LsPredecessors + ?Sized>(
    succs: &S,
    nb: usize,
    max_iterations: usize,
    visits: &mut usize,
    mut visit: impl FnMut(usize) -> bool,
) -> bool {
    let mut dirty = LsDirty::all(nb);
    for _ in 0..max_iterations {
        let mut changed = false;
        let mut from = 0usize;
        while let Some(b) = ls_lowest_set_from(dirty.bits(), from, nb) {
            from = b + 1;
            dirty.bits()[b / 64] &= !(1u64 << (b % 64));
            *visits += 1;
            if visit(b) {
                changed = true;
                for &s in succs.ls_predecessors(b) {
                    if s < nb {
                        ls_bit_set(dirty.bits(), s);
                    }
                }
            }
        }
        if !changed {
            return true;
        }
    }
    false
}

/// Copy a dense per-block bit set (`dense_words` per block, bit `k` = the
/// value `sparse_of[k]`) into the node-id layout (`words` per block, bit `id`
/// of word `id / 64`) the core publishes.
pub(in crate::regalloc) fn ls_dense_bits_to_node_bits(
    dense: &[u64],
    dense_words: usize,
    words: usize,
    nb: usize,
    sparse_of: &[usize],
) -> Vec<u64> {
    let mut out = vec![0u64; words * nb];
    for b in 0..nb {
        let from = match dense.get(b * dense_words..(b + 1) * dense_words) {
            Some(from) => from,
            None => continue,
        };
        let to = &mut out[b * words..(b + 1) * words];
        ls_bits_for_each(from, |k| {
            if let Some(&id) = sparse_of.get(k) {
                ls_bit_set(&mut *to, id);
            }
        });
    }
    out
}

/// The one body behind [`liveness_core_sparse`] (`DENSE = false`: bit `i` is
/// node `i`) and [`liveness_core_dense`] (`DENSE = true`: bit `k` is the k-th
/// wanted node). Monomorphised, so the sparse core pays nothing for the
/// numbering. Every bit operation is guarded by `wanted`, so only wanted
/// values ever reach a set, and the dense sets are an exact relabelling of the
/// sparse ones: the same bits change on the same pass, the fixed point runs
/// the same number of passes, and mapping the sets back gives the sparse sets.
#[allow(clippy::too_many_arguments)]
pub(in crate::regalloc) fn liveness_core_body<const DENSE: bool>(
    graph: &Graph,
    schedule: &Schedule,
    wants: &[bool],
    rules: LivenessRules,
    work_budget: usize,
    max_iterations: usize,
    budget_by_values: bool,
    fixed_point: LivenessFixedPoint,
    visits: &mut usize,
) -> LivenessCore {
    liveness_core_body_using::<DENSE, _>(graph, schedule, wants, rules, work_budget,
        max_iterations, budget_by_values, fixed_point, visits, &GraphValues)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::regalloc) fn liveness_core_body_using<const DENSE: bool, S: ValueSource>(
    graph: &Graph,
    schedule: &Schedule,
    wants: &[bool],
    rules: LivenessRules,
    work_budget: usize,
    max_iterations: usize,
    budget_by_values: bool,
    fixed_point: LivenessFixedPoint,
    visits: &mut usize,
    source: &S,
) -> LivenessCore {
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();
    let wanted = |i: usize| i < n && wants.get(i).copied().unwrap_or(false);

    // ── The value numbering (wave 67) ────────────────────────────────
    // `dense_of[i]` is node `i`'s bit when wanted (`usize::MAX`, which every
    // bit helper ignores, otherwise); `sparse_of[k]` is bit `k`'s node.
    let mut dense_of: Vec<usize> = Vec::new();
    let mut sparse_of: Vec<usize> = Vec::new();
    if DENSE {
        dense_of = vec![usize::MAX; n];
        for (i, cell) in dense_of.iter_mut().enumerate() {
            if wanted(i) {
                *cell = sparse_of.len();
                sparse_of.push(i);
            }
        }
    }
    let values = if DENSE { sparse_of.len() } else { n };
    let bit = |i: usize| -> usize {
        if DENSE {
            dense_of.get(i).copied().unwrap_or(usize::MAX)
        } else {
            i
        }
    };
    let node_of_bit = |k: usize| -> usize {
        if DENSE {
            sparse_of.get(k).copied().unwrap_or(usize::MAX)
        } else {
            k
        }
    };

    // ── Positions and block spans ────────────────────────────────────
    let mut pos_of: Vec<Option<usize>> = vec![None; n];
    let mut span: Vec<(usize, usize)> = Vec::with_capacity(nb);
    let mut seq = 0usize;
    for block in &schedule.blocks {
        let start = seq;
        for &nid in &block.nodes {
            if let Some(cell) = pos_of.get_mut(nid as usize) {
                *cell = Some(seq);
            }
            seq += 1;
        }
        if let Some(term) = block.terminator {
            if let Some(cell) = pos_of.get_mut(term as usize) {
                *cell = Some(seq);
            }
            seq += 1;
        }
        // The outgoing edge, where `emit_phi_copies` reads this block's φ
        // arguments.
        let end = seq;
        seq += 1;
        span.push((start, end));
    }
    let total_positions = seq;

    let words = n.div_ceil(64).max(1);
    let mut lo = vec![usize::MAX; n];
    let mut hi = vec![0usize; n];
    let mut set_only_by_phi_rule = vec![false; n];
    let mut reads: Vec<(NodeId, usize)> = Vec::new();
    let mut live_in: Vec<u64> = Vec::new();
    let mut live_out: Vec<u64> = Vec::new();
    let mut converged = false;

    // The published `words` stays node-indexed; the sets themselves use
    // `bit_words` (equal to `words` in the sparse core).
    let bit_words = values.div_ceil(64).max(1);
    let budget_units = if budget_by_values { values } else { n };
    if budget_units.saturating_mul(nb) <= work_budget {
        let node_words = words;
        let words = bit_words;
        let mut def_bits = vec![0u64; words * nb];
        let mut use_bits = vec![0u64; words * nb];
        let mut phi_out_bits = vec![0u64; words * nb];

        // ── Direct uses and definitions, in block order ──────────────
        let mut local_def = vec![0u64; words];
        for b in 0..nb {
            let base = b * words;
            local_def.fill(0);
            let block = &schedule.blocks[b];
            for u in block.nodes.iter().copied().chain(block.terminator) {
                let ui = u as usize;
                let node = match graph.nodes.get(ui) {
                    Some(node) => node,
                    None => continue,
                };
                let upos = pos_of.get(ui).copied().flatten().unwrap_or(span[b].0);
                if !matches!(node.op, Op::Phi) {
                    for inp in source.operands(u, node, rules.token_edges_not_uses) {
                        let ii = inp as usize;
                        if inp == NO_NODE || !wanted(ii) {
                            continue;
                        }
                        lo[ii] = lo[ii].min(upos);
                        hi[ii] = hi[ii].max(upos);
                        if rules.collect_reads {
                            reads.push((inp, upos));
                        }
                        if !ls_bit_get(&local_def, bit(ii)) {
                            ls_bit_set(&mut use_bits[base..base + words], bit(ii));
                        }
                    }
                }
                if wanted(ui) {
                    ls_bit_set(&mut local_def, bit(ui));
                    ls_bit_set(&mut def_bits[base..base + words], bit(ui));
                    lo[ui] = lo[ui].min(upos);
                    hi[ui] = hi[ui].max(upos);
                }
            }
        }

        // ── φ edge reads (and the φ edge rule) ───────────────────────
        let mut phis_of: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for (id, node) in graph.nodes.iter().enumerate() {
            if !matches!(node.op, Op::Phi) {
                continue;
            }
            if let Some(&merge) = node.inputs.first() {
                if merge != NO_NODE {
                    phis_of.entry(merge).or_default().push(id as NodeId);
                }
            }
        }
        // `(φ, edge position)` pairs, applied after the widening so the
        // pre-rule "placed by nothing" question can still be asked; `lo`/`hi`
        // are a min/max, so the order is not otherwise observable.
        let mut phi_edges: Vec<(usize, usize)> = Vec::new();
        for succ in 0..nb {
            let merge_ctrl = schedule.blocks[succ].ctrl;
            let merge_node = match graph.nodes.get(merge_ctrl as usize) {
                Some(m) if matches!(m.op, Op::Merge | Op::Region) => m,
                _ => continue,
            };
            let phis = match phis_of.get(&merge_ctrl) {
                Some(phis) => phis,
                None => continue,
            };
            // merge.inputs[k] is the control token for φ value k (= input k+1).
            for (k, &ctrl_in) in merge_node.inputs.iter().enumerate() {
                let pred = match ls_ctrl_block_of(graph, schedule, ctrl_in) {
                    Some(pred) if pred < nb => pred,
                    _ => continue,
                };
                let at = span[pred].1;
                for &pid in phis {
                    let phi = graph.nodes.get(pid as usize);
                    let v = match phi.and_then(|p| p.inputs.get(k + 1)) {
                        Some(&v) if v != NO_NODE => v,
                        _ => continue,
                    };
                    let vi = v as usize;
                    if !wanted(vi) {
                        continue;
                    }
                    // A memory φ gets no edge copy, so under the token rule
                    // its source is not read here.
                    let source_unread =
                        rules.token_edges_not_uses && phi.is_some_and(|p| p.ty == IrType::Memory);
                    if !source_unread {
                        lo[vi] = lo[vi].min(at);
                        hi[vi] = hi[vi].max(at);
                        if rules.collect_reads {
                            reads.push((v, at));
                        }
                        ls_bit_set(&mut phi_out_bits[pred * words..(pred + 1) * words], bit(vi));
                    }
                    if rules.phi_edge_interfere {
                        let pi = pid as usize;
                        if wanted(pi) {
                            phi_edges.push((pi, at));
                        }
                    }
                }
            }
        }

        // ── The backward fixed point ─────────────────────────────────
        let mut li = vec![0u64; words * nb];
        let mut lout = vec![0u64; words * nb];
        let sets = LsFixedPointSets {
            nb,
            words,
            phi_out: &phi_out_bits,
            uses: &use_bits,
            defs: &def_bits,
        };
        let blocks: &[crate::ir_schedule::Block] = &schedule.blocks;
        converged = ls_fixed_point_on_schedule(
            blocks,
            fixed_point,
            sets,
            &mut li,
            &mut lout,
            max_iterations,
            visits,
        );

        // ── Widen each range over every block it is live in ──────────
        if converged {
            for b in 0..nb {
                let base = b * words;
                let (start, end) = span[b];
                ls_bits_for_each(&li[base..base + words], |k| {
                    let v = node_of_bit(k);
                    if wanted(v) {
                        lo[v] = lo[v].min(start);
                    }
                });
                ls_bits_for_each(&lout[base..base + words], |k| {
                    let v = node_of_bit(k);
                    if wanted(v) {
                        hi[v] = hi[v].max(end);
                    }
                });
            }
        }

        // ── The φ edge rule ──────────────────────────────────────────
        for &(pi, _) in &phi_edges {
            if lo[pi] == usize::MAX {
                set_only_by_phi_rule[pi] = true;
            }
        }
        for &(pi, at) in &phi_edges {
            lo[pi] = lo[pi].min(at);
            hi[pi] = hi[pi].max(at);
        }

        if DENSE {
            // Publish in the node-id layout, so every consumer (the live
            // model's `block_live_in`, the OSR live-in checks) reads the bits
            // it always read.
            live_in = ls_dense_bits_to_node_bits(&li, words, node_words, nb, &sparse_of);
            live_out = ls_dense_bits_to_node_bits(&lout, words, node_words, nb, &sparse_of);
        } else {
            live_in = li;
            live_out = lout;
        }
    }

    LivenessCore {
        pos_of,
        span,
        total_positions,
        lo,
        hi,
        set_only_by_phi_rule,
        converged,
        words,
        live_in,
        live_out,
        reads,
    }
}

/// The shared tail of a [`LiveModel`] build: finalised ranges, register
/// classes, sorted uses, `peak_live`, and the back-edge (`carried`) facts.
pub(in crate::regalloc) fn finish_live_model(graph: &Graph, schedule: &Schedule, head: LiveModelHead) -> LiveModel {
    let LiveModelHead {
        pos_of,
        span,
        total_positions,
        block_of_pos,
        wants_loc,
        mut pinned,
        lo,
        hi,
        mut uses,
        weight,
        loop_depth,
        converged,
        block_live_in,
    } = head;
    let n = graph.nodes.len();

    // ── 6. Finalise ──────────────────────────────────────────────────
    let whole_method = PosRange {
        lo: 0,
        hi: total_positions.saturating_sub(1),
    };
    let mut range: Vec<Option<PosRange>> = vec![None; n];
    let mut class: Vec<Option<RegClass>> = vec![None; n];
    let mut is_ref = vec![false; n];
    for id in 0..n {
        if !wants_loc[id] {
            continue;
        }
        if !converged {
            pinned[id] = true;
        }
        range[id] = Some(if lo[id] == usize::MAX || !converged {
            whole_method
        } else {
            PosRange {
                lo: lo[id],
                hi: hi[id].max(lo[id]),
            }
        });
        let ty = graph.nodes[id].ty;
        class[id] = RegClass::of(ty);
        is_ref[id] = ty == IrType::Ref;
        uses[id].sort_unstable();
        uses[id].dedup();
    }

    // Peak simultaneous liveness, by sweeping the interval endpoints.
    let mut delta = vec![0i64; total_positions + 2];
    for r in range.iter().flatten() {
        if r.lo < delta.len() && r.hi + 1 < delta.len() {
            delta[r.lo] += 1;
            delta[r.hi + 1] -= 1;
        }
    }
    let mut running = 0i64;
    let mut peak_live = 0i64;
    for d in &delta {
        running += d;
        peak_live = peak_live.max(running);
    }

    // Which positions are back edges: the outgoing edge of a block that
    // branches to itself or to any earlier block. Same test
    // `MachineModel::for_graph` uses to place its safepoint polls, because it
    // is the same event — the point an iteration ends.
    //
    // **Deliberately NOT the dominator test `ls_loop_depths` uses (R10).** That
    // one answers "is this block inside a loop", a question about the graph, and
    // an order-based answer to it is simply wrong. This one answers "does a
    // safepoint poll get emitted at this block's outgoing edge", a question
    // about what `ir_lower::lower_block` and `MachineModel::for_graph` actually
    // do — and both of them use the index comparison. Replacing it here would
    // make `carried` name edges that carry no poll and miss edges that do,
    // which is a worse answer however much more principled the test looks.
    //
    // Each back edge is kept with its WINDOW, `[first position of the lowest
    // target block, edge]` — the positions the index test counts as the loop.
    let mut back_edges: Vec<(usize, usize)> = Vec::new();
    for (b, block) in schedule.blocks.iter().enumerate() {
        let window_lo = block
            .successors
            .iter()
            .filter(|&&s| s <= b)
            .filter_map(|&s| span.get(s).map(|&(start, _)| start))
            .min();
        if let (Some(window_lo), Some(&(_, edge))) = (window_lo, span.get(b)) {
            back_edges.push((edge, window_lo));
        }
    }
    // `carried` asks only whether the range covers a back edge;
    // `carried_and_read` also asks whether the value is READ inside that
    // edge's window, i.e. whether evicting it costs a reload per iteration
    // (round 11 wave 5; see the field's doc for the eviction it corrects).
    //
    // `back_edges` is ascending (block spans are laid out in block order), so
    // the edges inside `[lo, hi]` start at a binary-searched index (round 11
    // wave 4). A value spanning more than `CARRIED_EDGE_SCAN` edges is
    // answered `carried_and_read = carried`, which bounds the refinement at
    // O(values × 64 × log uses); so is every value of an unconverged model,
    // which has no uses to consult.
    const CARRIED_EDGE_SCAN: usize = 64;
    let mut carried = vec![false; n];
    let mut carried_and_read = vec![false; n];
    if !back_edges.is_empty() {
        for (id, r) in range.iter().enumerate() {
            let Some(r) = r else { continue };
            let first = back_edges.partition_point(|&(e, _)| e < r.lo);
            let in_range = back_edges
                .get(first..)
                .unwrap_or(&[])
                .iter()
                .take_while(|&&(e, _)| e <= r.hi);
            let value_uses = uses.get(id).map(Vec::as_slice).unwrap_or(&[]);
            for (scanned, &(edge, window_lo)) in in_range.enumerate() {
                carried[id] = true;
                if scanned >= CARRIED_EDGE_SCAN || !converged {
                    carried_and_read[id] = true;
                    break;
                }
                let at = value_uses.partition_point(|&u| u < window_lo);
                if value_uses.get(at).is_some_and(|&u| u <= edge) {
                    carried_and_read[id] = true;
                    break;
                }
            }
        }
    }

    LiveModel {
        pos_of,
        span,
        total_positions,
        block_of_pos,
        wants_loc,
        range,
        class,
        is_ref,
        pinned,
        uses,
        weight,
        loop_depth,
        carried,
        carried_and_read,
        peak_live: peak_live.max(0) as usize,
        converged,
        block_live_in,
    }
}

/// `CRATONVM_DBG_IR_LS_TOKEN_TRIM_CENSUS=1` (opt-in, diagnostic only;
/// interpreter round i2 wave 63, lane L2, the first measurement of
/// `i58-L2-proposal-the-live-model-skips-memory-token-reads`): print one
/// [`token_trim_census`] line per optimizing compile. Changes no byte of any
/// compiled body; costs a second [`build_live_model_with_token_edges`] pass
/// per compile while on.
pub fn ls_token_trim_census_enabled() -> bool {
    false
}

/// How many examples [`TokenTrimCensus::examples`] keeps.
pub const TOKEN_TRIM_CENSUS_EXAMPLES: usize = 4;

/// What `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES` would change in one graph's
/// liveness model: the population whose interval END (or start, through the
/// loop dataflow) is set only by a memory-token read or a memory φ's edge.
///
/// The i58-L2 proposal names this as its first measurement ("zero means close
/// this"): the switch it proposes already exists (JIT round 17 wave 3, KEEP
/// OFF "within noise" on `R17IrLowerCopies`), and that triage never counted
/// how many values the switch actually moves in shipped code. This answers
/// it per compile without timing anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenTrimCensus {
    /// Values that want a location (identical in both models).
    pub values: usize,
    /// Values whose interval is strictly shorter with the switch on.
    pub trimmed: usize,
    /// Of [`Self::trimmed`], the value-typed ones (`Int` / `Long` / `Float` /
    /// `Double` / `Ref`): the population the proposal is about, a volatile
    /// `getstatic`, an `instanceof` / `checkcast` result, a call's result
    /// reused as the memory chain's producer.
    pub trimmed_value_typed: usize,
    /// Of [`Self::trimmed`], memory φs (their source edges stop being reads).
    pub trimmed_memory: usize,
    /// Sum over trimmed values of the positions removed from the interval.
    pub positions_trimmed: usize,
    /// Values that span a back edge with the switch off and none with it on.
    pub carried_dropped: usize,
    /// `(loop header, value)` pairs live into the header with the switch off
    /// and not with it on: the OSR-entry refusal population
    /// (`CRATONVM_JIT_IR_OSR_EXCUSE_MEMORY_ORDER_USES` excuses these at the
    /// entry only).
    pub header_live_in_dropped: usize,
    /// [`LiveModel::peak_live`] with the switch off.
    pub peak_live_off: usize,
    /// [`LiveModel::peak_live`] with the switch on.
    pub peak_live_on: usize,
    /// The first [`TOKEN_TRIM_CENSUS_EXAMPLES`] trimmed values, ascending id:
    /// `(id, type, range off, range on)`.
    pub examples: Vec<(NodeId, IrType, PosRange, PosRange)>,
    /// Interpreter round i2 wave 64 (lane L2,
    /// `i63-L2-proposal-frame-home-ranges-skip-memory-token-reads`): the
    /// slot-plan column, what `CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES`
    /// would change in the FRAME. Filled by `ir_lower` (which owns
    /// `plan_slots`); `None` from [`token_trim_census`] alone.
    pub slot_plan: Option<TokenTrimSlotColumn>,
}

/// Frame words with and without the slot-plan token-edge rule (see
/// [`TokenTrimCensus::slot_plan`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenTrimSlotColumn {
    /// `SlotPlan::slots` (8-byte spill words) without the rule.
    pub words_off: usize,
    /// `SlotPlan::slots` with the rule.
    pub words_on: usize,
    /// Of [`Self::words_off`], the colours that may hold a reference (every
    /// colour that is not a primitive-only one): what the prologue zeroes and
    /// the oop maps can name.
    pub ref_words_off: usize,
    /// Of [`Self::words_on`], the same.
    pub ref_words_on: usize,
    /// Values whose frame-home range is strictly shorter with the rule.
    pub homes_trimmed: usize,
}

impl TokenTrimCensus {
    /// One `[ir-ls]` line.
    pub fn render(&self) -> String {
        let examples: Vec<String> = self
            .examples
            .iter()
            .map(|(id, ty, off, on)| {
                format!("n{id} {ty:?} {}..{}->{}..{}", off.lo, off.hi, on.lo, on.hi)
            })
            .collect();
        // Wave 64: the slot-plan column goes LAST so a reader of the wave-63
        // fields (everything up to `examples=[..]`) parses this line as before.
        let slot_plan = match &self.slot_plan {
            Some(c) => format!(
                " slot-words={}->{} ref-words={}->{} homes-trimmed={}",
                c.words_off, c.words_on, c.ref_words_off, c.ref_words_on, c.homes_trimmed
            ),
            None => String::new(),
        };
        format!(
            "[ir-ls] token-trim census: values={} trimmed={} (value-typed={} memory={}) \
             positions={} carried-dropped={} header-live-in-dropped={} peak-live={}->{} \
             examples=[{}]{}",
            self.values,
            self.trimmed,
            self.trimmed_value_typed,
            self.trimmed_memory,
            self.positions_trimmed,
            self.carried_dropped,
            self.header_live_in_dropped,
            self.peak_live_off,
            self.peak_live_on,
            examples.join(", "),
            slot_plan,
        )
    }
}

/// Build the liveness model both ways and compare (see [`TokenTrimCensus`]).
/// `None` when either model did not converge (every range is then the whole
/// method, and a difference would mean nothing).
pub fn token_trim_census(graph: &Graph, schedule: &Schedule) -> Option<TokenTrimCensus> {
    let off = build_live_model_with_token_edges(graph, schedule, false);
    let on = build_live_model_with_token_edges(graph, schedule, true);
    token_trim_census_of(graph, schedule, &off, &on)
}

/// [`token_trim_census`] over two models the caller built (`off` without the
/// token-edge rule, `on` with it).
pub fn token_trim_census_of(
    graph: &Graph,
    schedule: &Schedule,
    off: &LiveModel,
    on: &LiveModel,
) -> Option<TokenTrimCensus> {
    if !off.converged || !on.converged {
        return None;
    }
    let n = graph.nodes.len();
    let mut census = TokenTrimCensus {
        peak_live_off: off.peak_live,
        peak_live_on: on.peak_live,
        ..TokenTrimCensus::default()
    };
    for id in 0..n {
        let (Some(Some(r_off)), Some(Some(r_on))) = (off.range.get(id), on.range.get(id)) else {
            continue;
        };
        census.values += 1;
        let len_off = r_off.hi.saturating_sub(r_off.lo);
        let len_on = r_on.hi.saturating_sub(r_on.lo);
        if r_off != r_on && len_on < len_off {
            census.trimmed += 1;
            census.positions_trimmed += len_off - len_on;
            let ty = graph.nodes[id].ty;
            match ty {
                IrType::Int | IrType::Long | IrType::Float | IrType::Double | IrType::Ref => {
                    census.trimmed_value_typed += 1;
                }
                IrType::Memory => census.trimmed_memory += 1,
                _ => {}
            }
            if census.examples.len() < TOKEN_TRIM_CENSUS_EXAMPLES {
                // Cast: an index into the node arena is a `NodeId`.
                census.examples.push((id as NodeId, ty, *r_off, *r_on));
            }
        }
        if off.carried.get(id).copied().unwrap_or(false)
            && !on.carried.get(id).copied().unwrap_or(false)
        {
            census.carried_dropped += 1;
        }
    }
    // Loop headers by the same index test the back-edge positions use: a
    // block some block at or after it branches to.
    let mut is_header = vec![false; schedule.blocks.len()];
    for (p, blk) in schedule.blocks.iter().enumerate() {
        for &s in &blk.successors {
            if s <= p {
                if let Some(cell) = is_header.get_mut(s) {
                    *cell = true;
                }
            }
        }
    }
    for (h, &header) in is_header.iter().enumerate() {
        if !header {
            continue;
        }
        for id in 0..n {
            // Cast: an index into the node arena is a `NodeId`.
            let v = id as NodeId;
            if off.live_in_at(h, v) == Some(true) && on.live_in_at(h, v) == Some(false) {
                census.header_live_in_dropped += 1;
            }
        }
    }
    Some(census)
}

impl LiveModel {
    /// The first read of `node` at or after `pos`, if any.
    pub fn next_use_at_or_after(&self, node: NodeId, pos: usize) -> Option<usize> {
        let uses = self.uses.get(node as usize)?;
        let idx = uses.partition_point(|&u| u < pos);
        uses.get(idx).copied()
    }

    /// The first read of `node` strictly after `pos`, if any.
    pub fn next_use_after(&self, node: NodeId, pos: usize) -> Option<usize> {
        self.next_use_at_or_after(node, pos.saturating_add(1))
    }

    /// Is `node` live on entry to `block`? `None` when the model does not know
    /// — it did not converge, or it was built by hand without
    /// [`LiveModel::block_live_in`] — so a caller can fall back to its own
    /// conservative answer instead of mistaking "unknown" for "dead".
    pub fn live_in_at(&self, block: usize, node: NodeId) -> Option<bool> {
        if !self.converged || self.block_live_in.is_empty() {
            return None;
        }
        let words = self.block_live_in.get(block)?;
        Some(ls_bit_get(words, node as usize))
    }

    /// Drop the **deopt-frame** pins, keeping the phi pins. Returns how many
    /// values were released.
    ///
    /// ## Why this exists
    ///
    /// `build_step_4` pins every value a `SafepointSnapshot` names, because the
    /// home word must hold the value at any recorded bci and a general
    /// allocator cannot establish that. On a graph built from real bytecode
    /// that is *every value*: `ir::IrBuilder` records a snapshot of the locals
    /// and the operand stack at **every bytecode boundary**, so a temporary
    /// sitting on the stack across one boundary — which is every temporary —
    /// is named by a frame state. Measured on any `IrBuilder` graph, the pin
    /// set is the whole value set and [`allocate_linear_scan`] promotes
    /// nothing. The allocator is not conservative here, it is inert.
    ///
    /// ## When it is sound to release them
    ///
    /// Exactly when the consumer is **write-through**: it emits the home store
    /// at every definition regardless of the register it also assigns, so the
    /// frame image is complete at every instruction boundary and a deopt frame
    /// built from home words is bit-identical to the one a register-free
    /// lowering would build. `ir_lower`'s wiring is that consumer — see
    /// `docs/jit/linear-scan-wiring.md`.
    ///
    /// ## What a caller gives up
    ///
    /// [`Allocation::stack_slot`] and [`Allocation::stack_slots`]. Releasing a
    /// pin also moves the value out of the `Pinned` home pool, so the home
    /// colouring this model produces may put two deopt-named values in one
    /// word — correct for their live ranges, and NOT what a deopt frame
    /// reconstructor expecting one word per named value assumes. A caller that
    /// calls this **must** take its home layout from somewhere else
    /// (`ir_lower` takes it from `plan_slots`, which keeps every pin). Nothing
    /// enforces that; it is why this is an explicit call and not a default.
    ///
    /// A no-op on an unconverged model: there every value is pinned *because
    /// nothing was analysed*, which no write-through property repairs.
    pub fn release_deopt_pins(&mut self, graph: &Graph) -> usize {
        if !self.converged {
            return 0;
        }
        let mut released = 0usize;
        for (id, node) in graph.nodes.iter().enumerate() {
            if !self.pinned.get(id).copied().unwrap_or(false) {
                continue;
            }
            // A phi's home is what the edge copies write; its pin is structural
            // and has nothing to do with deopt.
            if matches!(node.op, Op::Phi) {
                continue;
            }
            self.pinned[id] = false;
            released += 1;
        }
        released
    }
    /// Release the STRUCTURAL pin on every phi, for a consumer that publishes
    /// a promoted phi's register itself at every incoming edge (`ir_lower`'s
    /// `emit_phi_copies` does, behind `CRATONVM_JIT_IR_PHI_RESIDENCY`). The
    /// pin exists because no definition arm ever writes a phi; a consumer
    /// that supplies that write at the edges has discharged the reason.
    ///
    /// Same caveat as [`Self::release_deopt_pins`]: the home layout must
    /// still come from a colouring that keeps every phi in its own word,
    /// which `ir_lower::plan_slots` does. A no-op on an unconverged model.
    pub fn release_phi_pins(&mut self, graph: &Graph) -> usize {
        if !self.converged {
            return 0;
        }
        let mut released = 0usize;
        for (id, node) in graph.nodes.iter().enumerate() {
            if !matches!(node.op, Op::Phi) {
                continue;
            }
            if self.pinned.get(id).copied().unwrap_or(false) {
                self.pinned[id] = false;
                released += 1;
            }
        }
        released
    }

    /// Pin values this model cannot see a reason to pin but the caller can —
    /// today, the scalar-replacement field values `ir_lower::plan_slots` pins
    /// from its `sr_map`, which [`build_live_model`] is never handed. Returns
    /// how many were newly pinned.
    ///
    /// Call it BEFORE [`Self::release_deopt_pins`], so the extra pins are
    /// released on exactly the same write-through argument as the deopt pins
    /// they are a kind of, and a consumer that does not release keeps them.
    /// Ids that name no value (`!wants_loc`) are ignored: a pin on a node with
    /// no location means nothing. See
    /// `docs/feature-designs/jit-ir-optimizations.md`.
    pub fn pin_values(&mut self, ids: impl IntoIterator<Item = NodeId>) -> usize {
        let mut pinned = 0usize;
        for id in ids {
            let i = id as usize;
            if id == NO_NODE || !self.wants_loc.get(i).copied().unwrap_or(false) {
                continue;
            }
            if let Some(cell) = self.pinned.get_mut(i) {
                if !*cell {
                    *cell = true;
                    pinned += 1;
                }
            }
        }
        pinned
    }
}

/// JIT round 19 wave 4 (lane irops3, IROPS2-3): every general-purpose
/// register `ir_lower`'s `Op::ScalarIntrinsic` arm writes for `sop` -- the
/// operand loads (`a` into RAX, `b` into RCX) and each arm's scratch (RDX) --
/// so [`MachineModel::for_graph`] can clobber all of them at the node's
/// position. A superset where the arm has two forms (the `LZCNT`/`TZCNT` row
/// writes only RAX, the `BSR`/`BSF` one RAX, RCX and RDX). The FP families
/// write XMM0/XMM1 only, which no file this crate builds contains.
///
/// A pure modelling fix: `ir_lower`'s own file excludes RAX/RCX/RDX and
/// `destroyed.retain(|r| regs.contains(*r))` drops these clobbers for it, so
/// production allocations are unchanged. It is what keeps a value out of RCX
/// across `multiplyHigh` (and out of RDX across `abs`, ...) the day the IR
/// file gains the [`FIXED_OPERAND_GPRS`]. Exhaustive on purpose: a new
/// family is a compile error here until its writes are stated.
pub(crate) fn scalar_op_gp_writes(sop: crate::ir::ScalarOp) -> &'static [PhysReg] {
    use crate::ir::ScalarOp as S;
    const A: &[PhysReg] = &[X86_RAX];
    const AC: &[PhysReg] = &[X86_RAX, X86_RCX];
    const AD: &[PhysReg] = &[X86_RAX, X86_RDX];
    const ACD: &[PhysReg] = &[X86_RAX, X86_RCX, X86_RDX];
    const NONE: &[PhysReg] = &[];
    match sop {
        S::MinI | S::MaxI | S::MinL | S::MaxL => AC,
        S::AbsI | S::AbsL => AD,
        S::CompareI | S::CompareL => ACD,
        S::NlzI | S::NlzL | S::NtzI | S::NtzL => ACD,
        S::ReverseBytesI | S::ReverseBytesL => A,
        S::LowestOneBitI | S::LowestOneBitL => AC,
        S::HighestOneBitI | S::HighestOneBitL => ACD,
        S::RotateLeftI | S::RotateLeftL | S::RotateRightI | S::RotateRightL => AC,
        // The sign mask is staged through RAX.
        S::AbsF | S::AbsD => A,
        S::DoubleToRawLongBits | S::LongBitsToDouble => A,
        S::FloatToRawIntBits | S::IntBitsToFloat => A,
        S::MultiplyHighL | S::UnsignedMultiplyHighL => ACD,
        S::BitCountI | S::BitCountL => A,
        // nio20 wave 6 (dev): `SUB EAX, lo; CMP EAX, 0x400; SETB AL; MOVZX`.
        S::IsHighSurrogate | S::IsLowSurrogate => A,
        // JIT r20 wave 11 (lane pIR): `CMP EAX, imm; SETcc AL; MOVZX [; INC]`.
        S::CharCount | S::IsBmpCodePoint | S::IsValidCodePoint => A,
        S::IsIsoControl => AC,
        // JIT r22 wave 1 (lane intrinsics): `Math.round`'s bit arithmetic
        // stages the bits in RAX, the exponent / shift in RCX / RDX.
        S::RoundD | S::RoundF => ACD,
        // j27 wave 6 (lane T1BCE): `PEXT` / `PDEP RAX, RAX, RCX`, the mask
        // staged in RCX.
        S::CompressI | S::CompressL | S::ExpandI | S::ExpandL => AC,
        S::SqrtD
        | S::MinF
        | S::MaxF
        | S::MinD
        | S::MaxD
        | S::FloorD
        | S::CeilD
        | S::RintD
        | S::FmaD
        | S::FmaF | S::MathCall(_) => NONE,
    }
}

impl MachineModel {
    pub(in crate::regalloc) fn write_at(&self, pos: usize) -> Option<&ReadBeforeWrite> {
        self.read_before_write.binary_search_by_key(&pos, |w| w.pos)
            .ok().map(|i| &self.read_before_write[i])
    }

    /// Both intervals may contain the boundary because the input is read first.
    pub(crate) fn permits_register_handoff(
        &self, live: &LiveModel, input: NodeId, result: NodeId,
        input_range: PosRange, result_range: PosRange,
    ) -> bool {
        let pos = result_range.lo;
        input != result && input_range.lo < pos && input_range.hi == pos
            && live.range.get(input as usize).copied().flatten().is_some_and(|r| r.hi == pos)
            && live.range.get(result as usize).copied().flatten().is_some_and(|r| r.lo == pos)
            && live.pos_of.get(result as usize).copied().flatten() == Some(pos)
            && live.uses.get(input as usize).is_some_and(|uses| uses.last() == Some(&pos))
            && self.write_at(pos).is_some_and(|w| w.result == result && w.inputs.contains(&input))
    }

    pub(in crate::regalloc) fn verify_read_before_write(&self, graph: &Graph, live: &LiveModel) -> CompileResult<()> {
        if self.read_before_write.windows(2).any(|w| w[0].pos >= w[1].pos) {
            return Err(Bailout::new(BailoutReason::Internal("regalloc: read/write boundaries are not strictly ordered")));
        }
        for write in &self.read_before_write {
            let integer = |id: NodeId| graph.nodes.get(id as usize)
                .is_some_and(|n| matches!(n.ty, IrType::Int | IrType::Long));
            if !integer(write.result)
                || live.pos_of.get(write.result as usize).copied().flatten() != Some(write.pos)
                || write.inputs.is_empty()
                || write.inputs.iter().any(|&id| id == write.result || !integer(id)
                    || !live.uses.get(id as usize).is_some_and(|u| u.binary_search(&write.pos).is_ok()))
            {
                return Err(Bailout::new(BailoutReason::Internal("regalloc: invalid read/write boundary")));
            }
        }
        Ok(())
    }

    /// Derive the machine model for one scheduled graph over `regs`.
    ///
    /// Produces the clobber set (every call position destroys every
    /// caller-saved register in the file), the x86 **fixed-operand** constraints
    /// and the safepoint set. There are no entry-ABI constraints: a backend
    /// that wants a `Param` pinned to its incoming register adds a
    /// [`FixedConstraint`] at the `Param`'s own position (`live.pos_of`).
    ///
    /// # R6: the fixed-operand rules are constraints, not clobbers
    ///
    /// This function used to push RAX/RDX (for `Div`/`Rem`) and RCX (for the
    /// shifts) onto the *clobber* list, with a comment saying they mattered
    /// "only if a caller puts those in the file" — and `RegFile::x86_64` did
    /// not, so `destroyed.retain(|r| regs.contains(*r))` deleted every one of
    /// them on the next line. The whole constraint path was unreachable and the
    /// question `idiv` asks was answered by removing five GPRs from the file.
    ///
    /// The two forms are not interchangeable and only one of them is true:
    ///
    /// * `idiv r/m` reads the dividend in **RDX:RAX** and writes the quotient to
    ///   RAX and the remainder to RDX. So RAX and RDX *are* destroyed — the
    ///   clobber is right — **and** the dividend must be in RAX, which only a
    ///   [`FixedConstraint`] can say. Both are emitted, and they meet at one
    ///   position: see the early-clobber rule on [`ls_select_register`].
    /// * `shl r/m, cl` reads **CL** and writes only `r/m`. RCX is *not*
    ///   destroyed, so the clobber was an over-approximation that forced every
    ///   value out of RCX for nothing. Only the constraint is emitted.
    ///
    /// Every constraint is emitted **only when the register it names is in
    /// `regs`**. A backend that keeps RAX/RCX/RDX as private scratch (which is
    /// what `ir_lower` does — see [`RegFile::x86_64_scratch_reserved`]) gets a
    /// model byte-for-byte identical to the one it got before R6, because none
    /// of those registers is in its file.
    ///
    /// The `Div`/`Rem` rule is also gated on an **integral** result type, which
    /// the old clobber rule was not: `Op::Div` on `IrType::Float`/`Double`
    /// lowers to `divss`/`divsd` and touches no GPR at all. That was harmless
    /// while the registers were filtered out and is a live wrong answer now.
    ///
    /// **The clobber set is derived from ops alone.** It is complete for calls
    /// a *node* makes (`ir_op_is_call` is a superset of those, helper calls
    /// included) and necessarily silent about calls a backend emits *between*
    /// nodes. `ir_lower`'s cooperative safepoint poll is one: on a loop back
    /// edge it calls its slow path at the block's outgoing-edge position, which
    /// belongs to no node. This function marks that position a **safepoint**
    /// but not a clobber. A backend that allocates over caller-saved registers
    /// must add its own emission's clobbers on top — see
    /// `ir_lower::ir_lower_machine_model`, which does exactly that.
    pub fn for_graph(graph: &Graph, schedule: &Schedule, live: &LiveModel, regs: RegFile) -> Self {
        Self::for_graph_with_selected_fixed(graph, schedule, live, regs, |_| false)
    }

    /// Use the backend's selected constraints at positions that have them.
    /// The graph still owns liveness, safepoints and call effects.
    pub(crate) fn for_graph_with_selected_fixed(
        graph: &Graph,
        schedule: &Schedule,
        live: &LiveModel,
        regs: RegFile,
        selected_fixed: impl Fn(NodeId) -> bool,
    ) -> Self {
        let caller_saved: Vec<PhysReg> = regs
            .specs()
            .iter()
            .filter(|s| s.caller_saved)
            .map(|s| s.reg)
            .collect();
        let mut clobbers: Vec<(usize, Vec<PhysReg>)> = Vec::new();
        let mut safepoints: Vec<usize> = Vec::new();
        let mut fixed: Vec<FixedConstraint> = Vec::new();

        // Pin `node`'s `k`-th input to `reg` at `pos`, if that is a thing this
        // model can say. Three refusals, each of which makes the constraint
        // meaningless rather than merely unhelpful:
        //
        //   * `reg` is not allocatable — the emitter owns it, and a constraint
        //     naming a register the allocator cannot hand out is filtered out
        //     again by `allocate_linear_scan` anyway;
        //   * the input edge is absent or out of range — a malformed graph must
        //     not produce a constraint on node 0;
        //   * the input's register class is not `reg`'s (`RegClass::of`): an
        //     `IrType` with no GP class cannot be required to sit in a GPR.
        //
        // A constraint on a value that is never promoted (a phi, or anything a
        // deopt frame names) is still emitted and is still worth emitting: the
        // allocator gives such a value no interval, so nothing honours it, but
        // proof 4b's second half still keeps every OTHER value out of `reg` at
        // `pos` — which is the half the emitter depends on when it loads the
        // operand from the home slot itself.
        fn pin_input(
            fixed: &mut Vec<FixedConstraint>,
            graph: &Graph,
            regs: &RegFile,
            node: &crate::ir::Node,
            k: usize,
            pos: usize,
            reg: PhysReg,
        ) {
            if !regs.contains(reg) {
                return;
            }
            let Some(&input) = node.inputs.as_slice().get(k) else {
                return;
            };
            if input == NO_NODE {
                return;
            }
            let Some(operand) = graph.nodes.get(input as usize) else {
                return;
            };
            if RegClass::of(operand.ty) != Some(reg.class) {
                return;
            }
            fixed.push(FixedConstraint {
                node: input,
                pos,
                reg,
            });
        }

        for (b, block) in schedule.blocks.iter().enumerate() {
            for &nid in block.nodes.iter().chain(block.terminator.iter()) {
                let node = match graph.nodes.get(nid as usize) {
                    Some(node) => node,
                    None => continue,
                };
                let pos = match live.pos_of.get(nid as usize).copied().flatten() {
                    Some(pos) => pos,
                    None => continue,
                };
                if ir_op_is_safepoint(&node.op) {
                    safepoints.push(pos);
                }
                let mut destroyed: Vec<PhysReg> = Vec::new();
                if ir_op_is_call(&node.op) && !is_inline_integer_rem(node) {
                    destroyed.extend_from_slice(&caller_saved);
                }
                // ── Fixed-operand x86 instructions (R6) ──────────────
                match node.op {
                    // `idiv r/m` (and `div`): dividend in RDX:RAX, quotient to
                    // RAX, remainder to RDX. The `cqo`/`cdq` that sign-extends
                    // into RDX is part of the same expansion and destroys RDX
                    // before the divide, so RDX is unusable across `pos` even
                    // for a value the divide itself would not have touched.
                    //
                    // Integral results only: `Op::Div` on `Float`/`Double` is
                    // `divss`/`divsd`, which reads and writes one XMM and no
                    // GPR whatsoever.
                    Op::Div | Op::Rem
                        if matches!(node.ty, IrType::Int | IrType::Long) && !selected_fixed(nid) =>
                    {
                        destroyed.push(X86_RAX);
                        destroyed.push(X86_RDX);
                        // Input 0 is the dividend (`add_data(Op::Div, ty,
                        // vec![a, b])`), input 1 the divisor. Only the dividend
                        // is placeable: the divisor is the `r/m` operand and may
                        // be any register except the two above, which the
                        // clobber at `pos` already achieves.
                        //
                        // The RESULT is deliberately NOT pinned. It would want
                        // RAX (quotient) or RDX (remainder) at the same `pos`
                        // the dividend wants RAX, and two values requiring one
                        // register at one position is the unsatisfiable model
                        // `allocate_linear_scan` bails on. The clobber keeps the
                        // result out of RAX/RDX, so the emitter moves it out —
                        // one `mov`, which is what every x86 backend emits here
                        // anyway.
                        //
                        // A backend that also pins the DIVISOR — `ir_lower`
                        // emits `CQO` + `IDIV RCX` for a `Long` divide, so it
                        // does — owes that its own `add_clobbers(pos, &[RCX])`
                        // or its own constraint. That is a choice of *that
                        // emitter*, not a requirement of `idiv`, whose `r/m`
                        // operand is any register or memory word; this function
                        // states only what the instruction demands.
                        pin_input(&mut fixed, graph, &regs, node, 0, pos, X86_RAX);
                    }
                    // `shl`/`shr`/`sar r/m, cl`: the count is read from CL and
                    // **nothing writes RCX**. Emitting a clobber here (which is
                    // what this function used to do) forced every value out of
                    // RCX across every shift for no machine reason; the count
                    // being IN RCX is the only real requirement, and a count
                    // used by two shifts now keeps its register between them.
                    //
                    // Input 1 is the count. Input 0 is the two-address operand:
                    // `shl` reads and writes it, so in SSA the emitter always
                    // emits `mov result, in0` first and neither end of that pair
                    // is constrained to any particular register.
                    Op::Shl | Op::Shr | Op::UShr => {
                        pin_input(&mut fixed, graph, &regs, node, 1, pos, X86_RCX);
                    }
                    // JIT round 19 (lane irops19, R15B-31): one-operand
                    // `imul`/`mul r/m64` writes the 128-bit product to RDX:RAX.
                    // JIT round 19 wave 4 (lane irops3, IROPS2-3): and every
                    // other GP register `ir_lower`'s arm writes, RCX (`b`)
                    // included -- see `scalar_op_gp_writes`.
                    Op::ScalarIntrinsic(sop) => {
                        destroyed.extend_from_slice(scalar_op_gp_writes(sop));
                    }
                    _ => {}
                }
                destroyed.retain(|r| regs.contains(*r));
                if !destroyed.is_empty() {
                    destroyed.sort_unstable();
                    destroyed.dedup();
                    clobbers.push((pos, destroyed));
                }
            }
            // `lower_terminator` / the fall-through edge emit a safepoint poll
            // on any back edge, which publishes the oop map exactly as a call
            // site does. Both the terminator position and the edge position are
            // covered because the poll sits between them.
            if block.successors.iter().any(|&s| s <= b) {
                if let Some(&(_, edge)) = live.span.get(b) {
                    safepoints.push(edge.saturating_sub(1));
                    safepoints.push(edge);
                }
            }
        }

        safepoints.sort_unstable();
        safepoints.dedup();
        // Sorted by `(pos, reg, node)`, so a second producer can merge into
        // the same model and `allocate_linear_scan`'s per-node index sees each node's constraints
        // in position order — which is what makes "the earliest constraint in
        // this interval" a well-defined choice.
        fixed.sort_by_key(|f| (f.pos, f.reg, f.node));
        fixed.dedup();
        let mut model = MachineModel {
            regs,
            clobbers,
            fixed,
            safepoints,
            refs_may_cross_safepoints: false,
            reads_precede_clobber: Vec::new(),
            read_before_write: Vec::new(),
        };
        // The loop above pushes one entry per NODE, and two nodes can share a
        // position (`live.pos_of` is not injective for every op). A plain
        // `sort_by_key` left those as two entries at one position, which
        // `clobbered_at`'s binary search resolves to an arbitrary one of them.
        // Canonicalise instead of sorting, so the invariant the field's doc
        // comment states is established by CONSTRUCTION here rather than
        // remembered by every caller.
        model.canonicalize_clobbers();
        model
    }

    /// Declare that every CALL node `admit` accepts — an op [`ir_op_is_call`]
    /// lists, less the inline integer remainder — reads its register operands
    /// before the call destroys the caller-saved registers, i.e. add its
    /// position to [`MachineModel::reads_precede_clobber`]. Returns how many
    /// positions were added.
    ///
    /// **Opt-in, and the caller's claim.** It is true of a call that
    /// marshals its arguments and then executes `CALL`; it is the emitter's to
    /// prove for its own lowering of every op it admits (a frame record or a
    /// safepoint map emitted BEFORE the marshalling, if it calls, breaks it).
    /// Only positions that already carry a clobber are added — anything else
    /// would be a no-op entry. Call it AFTER every clobber the backend adds on
    /// top of [`MachineModel::for_graph`] (`add_clobbers`).
    ///
    /// Round 9 wave 4: `ir_lower` admits the ops that read every operand before
    /// their only `CALL`, and not `Op::Call`, whose safepoint map calls out
    /// before the arguments are marshalled. (The admit-everything form
    /// `mark_call_operand_reads_first` had no production caller and was
    /// deleted in round 11 wave 2; `|_| true` is that form.)
    pub fn mark_operand_reads_first_where(
        &mut self,
        graph: &Graph,
        live: &LiveModel,
        admit: impl Fn(&crate::ir::Node) -> bool,
    ) -> usize {
        let mut added = 0usize;
        for (id, node) in graph.nodes.iter().enumerate() {
            if !ir_op_is_call(&node.op) || is_inline_integer_rem(node) || !admit(node) {
                continue;
            }
            let Some(pos) = live.pos_of.get(id).copied().flatten() else {
                continue;
            };
            if self.clobbered_at(pos).is_empty() {
                continue;
            }
            if let Err(idx) = self.reads_precede_clobber.binary_search(&pos) {
                self.reads_precede_clobber.insert(idx, pos);
                added += 1;
            }
        }
        added
    }

    /// May `node` hold a register clobbered at `pos` THROUGH `pos`? Only when
    /// `pos` is listed in [`MachineModel::reads_precede_clobber`] and is both
    /// the end of `node`'s live range and its last use — the operand is read,
    /// and nothing reads it again. See that field.
    ///
    /// `pub(crate)` so a consumer's second opinion on the clobber property
    /// (`ir_lower::plan_register_residency`) applies the SAME exemption the
    /// scan and `verify_allocation` apply, rather than a restatement of it.
    pub(crate) fn read_before_clobber(&self, live: &LiveModel, node: NodeId, pos: usize) -> bool {
        let id = node as usize;
        self.reads_precede_clobber.binary_search(&pos).is_ok()
            && live
                .range
                .get(id)
                .copied()
                .flatten()
                .is_some_and(|r| r.hi == pos)
            && live.uses.get(id).and_then(|u| u.last()) == Some(&pos)
    }

    /// Record that `regs` are destroyed at `pos`, preserving the
    /// [`MachineModel::clobbers`] invariant.
    ///
    /// This is the API `MachineModel::for_graph`'s doc comment means when it
    /// says "a backend that allocates over caller-saved registers must add its
    /// own emission's clobbers on top". The obvious spelling —
    /// `model.clobbers.push((pos, regs))` — is WRONG whenever `for_graph`
    /// already recorded something at `pos` (a call position, exactly where a
    /// backend's extra clobbers land): the list then holds two entries for one
    /// position and `clobbered_at`'s binary search sees only one of them.
    ///
    /// Registers outside the allocatable file are dropped, matching what
    /// `for_graph` does with its own fixed-operand rules: the allocator has
    /// nothing to say about a register it cannot hand out.
    ///
    /// Insertion is `O(n)` in the worst case, which is the right trade for a
    /// list built once per compile and then binary-searched per interval. A
    /// producer adding many entries may instead push and call
    /// [`MachineModel::canonicalize_clobbers`] once at the end.
    pub fn add_clobbers(&mut self, pos: usize, regs: &[PhysReg]) {
        let mut add: Vec<PhysReg> = regs
            .iter()
            .copied()
            .filter(|r| self.regs.contains(*r))
            .collect();
        if add.is_empty() {
            return;
        }
        match self.clobbers.binary_search_by_key(&pos, |(p, _)| *p) {
            Ok(idx) => {
                let entry = &mut self.clobbers[idx].1;
                entry.append(&mut add);
                entry.sort_unstable();
                entry.dedup();
            }
            Err(idx) => {
                add.sort_unstable();
                add.dedup();
                self.clobbers.insert(idx, (pos, add));
            }
        }
    }

    /// Restore the [`MachineModel::clobbers`] invariant: sort by position,
    /// MERGE entries that share a position, and sort/dedup each entry's
    /// registers.
    ///
    /// For a producer that built the list by pushing. Idempotent, and a no-op
    /// on a list that already holds the invariant.
    pub fn canonicalize_clobbers(&mut self) {
        // `sort_by_key` is stable, so entries that share a position keep their
        // insertion order and the merge below is deterministic.
        self.clobbers.sort_by_key(|(pos, _)| *pos);
        let mut merged: Vec<(usize, Vec<PhysReg>)> = Vec::with_capacity(self.clobbers.len());
        for (pos, regs) in self.clobbers.drain(..) {
            match merged.last_mut() {
                Some((last_pos, last_regs)) if *last_pos == pos => last_regs.extend(regs),
                _ => merged.push((pos, regs)),
            }
        }
        for (_, regs) in merged.iter_mut() {
            regs.sort_unstable();
            regs.dedup();
        }
        // An entry with no registers is indistinguishable from no entry, and
        // dropping it keeps `clobbered_at`'s `Ok`/`Err` split meaningful.
        merged.retain(|(_, regs)| !regs.is_empty());
        self.clobbers = merged;
    }

    /// Every register destroyed at `pos`.
    ///
    /// Correct only because [`MachineModel::clobbers`] holds one entry per
    /// position: `binary_search_by_key` promises *an* index among equal keys,
    /// not the first or the only one.
    /// The invariant is checked once per compile by [`verify_allocation`]
    /// rather than asserted here: this runs once per position per interval,
    /// and an `O(n)` assertion in it would make the verifier quadratic.
    pub(in crate::regalloc) fn clobbered_at(&self, pos: usize) -> &[PhysReg] {
        match self.clobbers.binary_search_by_key(&pos, |(p, _)| *p) {
            Ok(idx) => &self.clobbers[idx].1,
            Err(_) => &[],
        }
    }

    /// Is any position in `range` a safepoint?
    pub(in crate::regalloc) fn range_covers_safepoint(&self, range: PosRange) -> bool {
        let idx = self.safepoints.partition_point(|&p| p < range.lo);
        self.safepoints.get(idx).is_some_and(|&p| p <= range.hi)
    }
}

/// [`MachineModel::clobbers`] transposed: for each register, the sorted,
/// deduplicated positions at which it is destroyed.
///
/// Round 11 wave 9, compile time. `ls_select_register::first_block` and
/// [`verify_allocation`]'s proof 4 both asked "the first position in
/// `[lo, hi]` at which `reg` is clobbered" by walking EVERY clobber site in
/// the range and testing each entry for `reg`. A callee-saved register is
/// clobbered nowhere, so for it that walk visited every call and every
/// back-edge poll the interval spans and found nothing — per register, per
/// attempt, per interval in the scan, and once more per segment in the proof.
/// A long-lived value in a call-heavy method paid O(calls) per question. One
/// binary search in the register's own list answers the same question.
///
/// Same answers as the walk on every model that holds the `clobbers`
/// invariant (strictly increasing positions), which `verify_allocation`
/// checks before it builds one of these; on a model that does not, both the
/// walk and this are meaningless and the verifier refuses the model.
pub(in crate::regalloc) struct ClobberIndex {
    pub(in crate::regalloc) by_reg: HashMap<PhysReg, Vec<usize>>,
}

impl ClobberIndex {
    pub(in crate::regalloc) fn of(model: &MachineModel) -> Self {
        let mut by_reg: HashMap<PhysReg, Vec<usize>> = HashMap::new();
        for (pos, regs) in &model.clobbers {
            for &reg in regs {
                by_reg.entry(reg).or_default().push(*pos);
            }
        }
        for positions in by_reg.values_mut() {
            positions.sort_unstable();
            positions.dedup();
        }
        ClobberIndex { by_reg }
    }

    /// The first position in `[lo, hi]` at which `reg` is clobbered, not
    /// counting `hi` itself when `exempt_hi`.
    pub(in crate::regalloc) fn first_in(&self, reg: PhysReg, lo: usize, hi: usize, exempt_hi: bool) -> Option<usize> {
        let positions = self.by_reg.get(&reg)?;
        let at = positions.partition_point(|&p| p < lo);
        let p = *positions.get(at)?;
        // Positions are unique, so when `hi` itself is exempt the next one is
        // already past the range.
        if p > hi || (exempt_hi && p == hi) {
            None
        } else {
            Some(p)
        }
    }
}

// ── Allocation result ────────────────────────────────────────────────

/// Where a value is at some position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueLoc {
    /// In a physical register.
    Reg(PhysReg),
    /// In its home frame slot, as an 8-byte word index.
    ///
    /// **Whose numbering** is the [`Allocation`] this came out of: by default
    /// [`ls_color_homes`]', and the consumer's own after
    /// [`Allocation::adopt_home_colouring`]. It is NOT automatically
    /// `SlotPlan::node_color`'s, which this used to claim — see the R7 note on
    /// [`Allocation::stack_slot`].
    ///
    /// A backend building [`ValueLoc`] values itself (rather than reading them
    /// out of an allocation) is of course free to put whatever word index its
    /// own frame uses in here; `ir_lower` does, in two different currencies, and
    /// neither is this one.
    Slot(u32),
}

/// One stretch of a value's life spent in one location.
///
/// A value's segments tile its whole live range with no gaps and no overlaps —
/// [`verify_allocation`] checks exactly that. `reg == None` means "in the home
/// slot", which is the location `ir_lower` uses today for every value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The positions this segment covers.
    pub range: PosRange,
    /// The register held over `range`, or `None` for the home slot.
    pub reg: Option<PhysReg>,
}

/// What the backend must emit at a location transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpillKind {
    /// Register → home slot. Counted in [`Allocation::spills`]. At most one per
    /// value: the IR is SSA, so once the home is written it stays correct.
    Store,
    /// Home slot → register. Counted in [`Allocation::reloads`].
    Load,
    /// Value recomputed into a register instead of being loaded. Counted in
    /// [`Allocation::remats`]; costs no memory traffic and no preceding store.
    Remat,
    /// Register → a different register. Counted in [`Allocation::reg_moves`].
    Move,
}

/// One transition the backend must materialise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpillEvent {
    /// The value.
    pub node: NodeId,
    /// For [`SpillKind::Store`], the **last** position at which the register
    /// still holds the value; the store may be sunk anywhere between the
    /// definition and here. For every other kind, the position at which the
    /// value must already be in its new location.
    pub pos: usize,
    /// What to emit.
    pub kind: SpillKind,
    /// Source register, when there is one.
    pub from: Option<PhysReg>,
    /// Destination register, when there is one.
    pub to: Option<PhysReg>,
}

/// The result of [`allocate_linear_scan`].
#[derive(Clone, Debug, Default)]
pub struct Allocation {
    /// `segments[id]` = node `id`'s location timeline, ordered and contiguous.
    /// Empty for nodes that produce no value.
    pub segments: Vec<Vec<Segment>>,
    /// `stack_slot[id]` = the home word index, or `None` when the value never
    /// touches memory.
    ///
    /// # R7: this is **not** a drop-in replacement for `SlotPlan::node_color`
    ///
    /// It said it was — "same numbering, same Ref/Prim/pinned pool separation" —
    /// and the pool separation half is true. The *numbering* half is false, and
    /// false in a way that cannot be fixed by tuning [`ls_color_homes`], because
    /// the two colourings are answering different questions:
    ///
    /// * `ir_lower::plan_slots` gives **every** value a home word; the frame it
    ///   lays out has to hold every value, promoted or not, because that file's
    ///   register file is a write-through cache over the home.
    /// * [`ls_color_homes`] gives a home only to a value that spends some
    ///   position in memory. A value that lives its whole life in a register
    ///   consumes no colour — which is the frame-size win the allocator exists
    ///   to produce — and every colour after it therefore shifts down.
    ///
    /// So the two agree exactly when nothing was promoted for its whole range,
    /// and disagree by construction the moment anything was. A `debug_assert!`
    /// that they match, which is what `NOTES-regalloc.md` §4 proposed, would
    /// fire on every successful allocation.
    ///
    /// **What this means for a consumer:** a `u32` coming out of
    /// [`Allocation::location_at`] as [`ValueLoc::Slot`] is in *this* numbering
    /// and addresses this plan's frame, not the emitter's. A consumer with a
    /// frame of its own calls [`Allocation::adopt_home_colouring`] and gets an
    /// allocation whose `Slot` payloads name words its encoder can address; a
    /// consumer with no frame of its own uses this one as is. What it must not
    /// do is mix them, which is what the old contract invited.
    pub stack_slot: Vec<Option<u32>>,
    /// Distinct home words the plan needs. In the same numbering as
    /// [`Self::stack_slot`] — see the R7 note there.
    pub stack_slots: usize,
    /// Register → memory transitions **this plan calls for**.
    ///
    /// # R5: this is the PLANNED number and it does not feed the report
    ///
    /// `CompilationReport::spills` is fed by `metrics::note_current_spills`,
    /// which `ir_lower` calls with its own `ls_spills` — what it **emitted**.
    /// That is the smaller number: `plan_register_residency` executes only the
    /// subset of this plan it can prove, so the difference between the two is
    /// exactly the gap between what the allocator decided and what the backend
    /// was able to carry out.
    ///
    /// Nothing reports that gap today, and pointing both numbers at one
    /// `Measured<u32>` field would make the report mean whichever producer ran
    /// last. The fix is a second pair of report fields (`planned_spills` /
    /// `planned_reloads`) in `metrics.rs`; until then this number is for a
    /// caller that asks the allocation directly, and the producer that used to
    /// publish it to the shared field is gone — see the R5 note in the metrics
    /// section at the bottom of this file.
    pub spills: usize,
    /// Memory → register transitions this plan calls for. Planned, not emitted;
    /// see [`Self::spills`].
    pub reloads: usize,
    /// Values recomputed rather than reloaded. Reported separately so a
    /// dashboard cannot mistake a free re-materialisation for memory traffic.
    pub remats: usize,
    /// Register → register transitions.
    pub reg_moves: usize,
    /// Interval splits performed.
    pub splits: usize,
    /// Values that spent at least one position in a register.
    pub promoted: usize,
    /// Every transition, in position order.
    ///
    /// Read by `ir_lower::ls_split_transitions` behind
    /// `CRATONVM_JIT_IR_LS_SPLITS`, which is what turns this list into machine
    /// code; by [`verify_allocation`], which proves it against the timeline;
    /// and by nothing else. A consumer that follows this list must also read
    /// [`Self::segments`], because there is no event for "the register stops
    /// being good": a [`SpillKind::Store`]'s `pos` is the last position at
    /// which it still IS good, and a rematerializable value produces no
    /// `Store` at all while still leaving its register.
    pub events: Vec<SpillEvent>,
    /// Copied from [`LiveModel::peak_live`] so a report has the pressure and
    /// the outcome in one place.
    pub peak_live: usize,
}

impl Allocation {
    /// Where is `node`'s value at `pos`? `None` when `pos` is outside its live
    /// range or the node produces no value.
    ///
    /// # A `Slot` answer assumes a write-through consumer for remat values
    ///
    /// A rematerializable value (`Const`, `ConstF`, `Param` — [`remat_of`])
    /// gets NO [`SpillKind::Store`] when it leaves a register, by design: its
    /// reload is a [`SpillKind::Remat`]. But a memory segment of such a value
    /// still answers `Slot(home)` here, and [`split_edge_resolution`] turns a
    /// memory delivery into a reload from that word. That word holds the value
    /// only because every consumer in the tree (`ir_lower`) writes every
    /// definition to its home. A consumer that follows [`Allocation::events`]
    /// literally and writes homes only at `Store` events must rematerialise
    /// such a value instead of loading `Slot` — the one place this plan is not
    /// self-sufficient (round 11 wave 2 review; see
    /// `docs/feature-designs/jit-ir-optimizations.md`).
    /// At a declared [`ReadBeforeWrite`] boundary, an input's location names
    /// its pre-write value and the result's location names the post-write value.
    pub fn location_at(&self, node: NodeId, pos: usize) -> Option<ValueLoc> {
        let segs = self.segments.get(node as usize)?;
        let seg = segs.iter().find(|s| s.range.contains(pos))?;
        match seg.reg {
            Some(reg) => Some(ValueLoc::Reg(reg)),
            None => self
                .stack_slot
                .get(node as usize)
                .copied()
                .flatten()
                .map(ValueLoc::Slot),
        }
    }

    /// Does `node` ever occupy a register? Test-only (round 11 wave 4).
    #[cfg(test)]
    pub fn is_promoted(&self, node: NodeId) -> bool {
        self.segments
            .get(node as usize)
            .is_some_and(|segs| segs.iter().any(|s| s.reg.is_some()))
    }

    /// The register `node` holds over its first segment, if any. Test-only
    /// (round 11 wave 4).
    #[cfg(test)]
    pub fn first_reg(&self, node: NodeId) -> Option<PhysReg> {
        self.segments.get(node as usize)?.first()?.reg
    }

    /// Does `node` spend any position of its life in memory?
    ///
    /// Exactly the population that needs a home word, and therefore exactly the
    /// population [`Self::adopt_home_colouring`] checks a candidate layout
    /// against.
    pub fn needs_home(&self, node: NodeId) -> bool {
        self.segments
            .get(node as usize)
            .is_some_and(|segs| segs.iter().any(|s| s.reg.is_none()))
    }

    /// Replace this plan's home colouring with the consumer's authoritative one.
    ///
    /// # R7: the answer to "two frame colourings and nothing compares them"
    ///
    /// [`ls_color_homes`] and `ir_lower::plan_slots` both colour home words, and
    /// only the second one lays out a frame anybody addresses. They cannot be
    /// *compared* — see the R7 note on [`Self::stack_slot`] for why a
    /// `stack_slot[id] == node_color[id]` assertion is wrong rather than merely
    /// unimplemented — and [`ls_color_homes`] cannot be *deleted*, because an
    /// [`Allocation`] has to be able to answer [`Self::location_at`] for a
    /// consumer that has no frame of its own.
    ///
    /// So: adopt instead of compare. A consumer that lays out its own frame
    /// hands it over here, and from that point every [`ValueLoc::Slot`] this
    /// allocation produces names a word that consumer's encoder can address.
    /// Re-running [`verify_allocation`] afterwards then makes proof 6 — two
    /// values sharing a home word have disjoint ranges, and a word never mixes
    /// [`HomeClass`] pools — a statement about **the frame that is emitted**
    /// rather than about a parallel invention.
    ///
    /// # What is refused
    ///
    /// A layout with no word for a value that spends time in memory. That is
    /// not a divergence to record, it is a value with nowhere to be spilled to,
    /// and accepting it would make `location_at` return `None` at a position
    /// inside the value's own live range. `colours.len()` must also match the
    /// graph, for the same reason [`verify_allocation`] checks it.
    ///
    /// A value that needs NO home and is given a colour anyway is accepted
    /// without comment: that is the normal shape of a write-through consumer,
    /// which writes every definition to its home whether or not the allocator
    /// asked for a word there.
    pub fn adopt_home_colouring(
        &mut self,
        colours: &[Option<u32>],
        slots: usize,
    ) -> CompileResult<()> {
        if colours.len() != self.segments.len() {
            return Err(Bailout::with_context(
                BailoutReason::Internal("regalloc: a home colouring is not sized for the graph"),
                format!(
                    "{} colours, {} timelines",
                    colours.len(),
                    self.segments.len()
                ),
            ));
        }
        for (id, &colour) in colours.iter().enumerate() {
            let node = id as NodeId;
            if !self.needs_home(node) {
                continue;
            }
            match colour {
                Some(c) if (c as usize) < slots => {}
                Some(c) => {
                    return Err(Bailout::with_context(
                        BailoutReason::Internal("regalloc: a home word is outside the frame plan"),
                        format!("n{node} at word {c} of {slots}"),
                    ))
                }
                None => {
                    return Err(Bailout::with_context(
                        BailoutReason::Internal("regalloc: a spilled value has no home word"),
                        format!("n{node} spends part of its life in memory"),
                    ))
                }
            }
        }
        self.stack_slot.clear();
        self.stack_slot.extend_from_slice(colours);
        self.stack_slots = slots;
        Ok(())
    }
}

// ── Spill cost model ─────────────────────────────────────────────────

/// Scale factor for the eviction score, so integer division by the frequency
/// weight keeps useful resolution.
pub(in crate::regalloc) const SPILL_DISTANCE_SCALE: u64 = 1024;

/// How much a LOOP-CARRIED value's next-use distance is discounted when
/// choosing a spill victim -- **default 64, i.e. ON** since 2026-09-05;
/// `CRATONVM_JIT_LS_CARRY_RELIEF=0` restores the distance-only rule and is the
/// kill switch.
///
/// It moves the residency census deterministically (`resident=1` to `2`,
/// `split_or_spilled=3` to `2` on `probes/OsrTierBench.java`, saturating by
/// 64), and it changes register allocation on every IR compile. Its timing
/// benefit is still unmeasured: the arm that would settle it was attempted at
/// host load 22 on 8 cores and is not usable; see `JIT_OPTIMIZATION.md`. (This
/// paragraph used to open "Off by default", which stopped being true on
/// 2026-09-05 — the code below defaults to 64.)
///
/// Not a tuning knob so much as a units correction. The score compares "how
/// long until this value is needed again", which prices a reload paid ONCE. A
/// value that crosses the back edge is needed again every iteration, so its
/// distance overstates its availability by roughly the trip count -- and the
/// heuristic then evicts precisely the values whose eviction is paid the most
/// times. Dividing keeps the ordering AMONG carried values intact while moving
/// all of them behind the uncarried ones.
pub(in crate::regalloc) fn ls_carry_relief() -> u64 {
    use std::sync::OnceLock;
    static G: OnceLock<u64> = OnceLock::new();
    *G.get_or_init(|| {
        // 2026-09-05: DEFAULT 64, i.e. ON. `=0` restores the old
        // distance-only rule and is the kill switch.
        match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) => v.trim().parse::<u64>().unwrap_or(64),
            Err(_) => 64,
        }
    })
}

/// Score handed to a rematerializable value, which is always the first thing
/// evicted: dropping it costs no store and its reload is an immediate.
pub(in crate::regalloc) const SPILL_SCORE_REMAT: u64 = u64::MAX;

/// Splits allowed before the allocator gives up.
///
/// Every re-queue strictly advances the re-queued interval's start, so the scan
/// terminates regardless — see the termination section on
/// [`allocate_linear_scan`], which names the guard that makes that true of the
/// eviction path. This budget is about *compile time*. A graph that needs more
/// splits than this over the given register file is one where every promotion
/// immediately costs a reload, i.e. one where linear scan buys nothing — so it
/// bails with [`BailoutReason::RegisterPressure`] rather than spending the
/// compile budget arriving at the frame layout `ir_lower` already had.
pub(in crate::regalloc) fn split_budget(intervals: usize) -> usize {
    intervals.saturating_mul(4).saturating_add(16)
}

/// How `node` can be recomputed, if it can.
///
/// `pub` because a backend consuming [`SpillKind::Remat`] needs it: the event
/// says "recompute this value into `to`" and this is the only thing that says
/// *how*.
///
/// **The consumer exists since R4** — `ir_lower::ls_split_transitions`, behind
/// `CRATONVM_JIT_IR_LS_SPLITS` — and it takes only the [`Remat::Const`] arm as
/// a true re-materialisation. [`Remat::ConstF`] would need a GP scratch for the
/// immediate-plus-`movq` sequence and [`Remat::Param`] is a second home word
/// for the same value; both lower there as a load from the value's own home
/// word instead, which is bit-identical because that backend writes every home
/// at the definition. The classification here is unchanged by that: what a
/// value CAN be recomputed from is a property of the graph, not of one
/// backend's scratch budget.
///
/// `x + c` is deliberately NOT here (j27 wave 4): it is recomputable only
/// while `x` is in a register, so the scan must still store it and score it
/// like any other value. Its classification is [`remat_add_of`], read by the
/// split consumer at a planned `Load`.
pub fn remat_of(graph: &Graph, node: NodeId) -> Option<Remat> {
    match graph.nodes.get(node as usize)?.op {
        Op::Const(v) => Some(Remat::Const(v)),
        Op::ConstF(bits) => Some(Remat::ConstF(bits)),
        Op::Param(i) => Some(Remat::Param(i)),
        _ => None,
    }
}

/// An interval waiting to be allocated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::regalloc) struct Pending {
    pub(in crate::regalloc) lo: usize,
    pub(in crate::regalloc) hi: usize,
    pub(in crate::regalloc) node: NodeId,
}

/// An interval currently holding a register.
#[derive(Clone, Copy, Debug)]
pub(in crate::regalloc) struct Active {
    pub(in crate::regalloc) reg: PhysReg,
    pub(in crate::regalloc) lo: usize,
    pub(in crate::regalloc) hi: usize,
    pub(in crate::regalloc) node: NodeId,
    /// Index into `reg_segments[node]`, so an eviction can shorten the segment
    /// that was already recorded.
    pub(in crate::regalloc) seg: usize,
}

/// Which interval loses its register when nothing is free.
pub(in crate::regalloc) enum Victim {
    /// The interval being allocated stays in memory until its next use.
    Current,
    /// An active interval is cut short at the current position.
    Active(usize),
}

/// Linear-scan register allocation over a scheduled IR graph.
///
/// ## Algorithm
///
/// Poletto–Sarkar linear scan, extended with interval splitting, a
/// frequency-weighted spill cost model and re-materialisation:
///
///  1. Intervals are the [`LiveModel`]'s contiguous ranges, processed in
///     increasing start order.
///  2. Actives whose range ends before the current start expire (the overlap
///     rule is endpoint-inclusive, so an active expires at `hi < lo`).
///  3. A register is chosen from the free set, preferring one with no clobber
///     and no foreign fixed constraint anywhere in the interval. If only a
///     partially-usable register is free, the interval is **split** at the
///     first blocking position: the prefix keeps the register, the suffix is
///     re-queued at its next use. This is what makes a call cost a spill/reload
///     pair for exactly the values that could not be given a callee-saved
///     register, and nothing for the ones that could.
///  4. With nothing free, a victim is chosen among the current interval and the
///     actives of the same class that started strictly earlier, maximising
///     `next_use_distance × 1024 / frequency_weight`, with rematerializable
///     values scored [`SPILL_SCORE_REMAT`] so they always go first. The victim
///     is cut at the current position and its suffix re-queued at its next use.
///
/// ## Termination (R12)
///
/// The claim is the one this paragraph always made — **every re-queue strictly
/// advances the re-queued interval's start** — and it is true of all three
/// re-queue sites. What was missing is *why* it is true of the third one, which
/// is the only one where it is not obvious from the line that does the pushing:
///
/// * the **clobber split** pushes at `next_use_at_or_after(node, split_at + 1)`,
///   and `split_at ≥ lo`, so the new start is `> lo` by inspection;
/// * the **lost-contest** path pushes at `next_use_after(node, lo)`, which is
///   strictly after `lo` by that method's definition;
/// * the **eviction** path pushes the victim at
///   `next_use_at_or_after(victim.node, lo)` — `lo` being the *current*
///   interval's start, not the victim's — so nothing on this line bounds the
///   result by `victim.lo` at all. It advances because
///   [`ls_pick_victim`] refuses to nominate an active with `a.lo >= current.lo`,
///   which puts `victim.lo < lo ≤ u`.
///
/// That guard is therefore **load-bearing for termination**, not the local
/// tidiness its own comment makes it sound like ("evicting it would not advance
/// anything"), and removing it would give this loop a way to evict an interval
/// and re-queue it at its own unchanged start forever.
/// `ls_pick_victim_never_nominates_an_active_that_starts_here` pins it.
///
/// [`split_budget`] is a **compile-time** bound layered on top, not the
/// termination argument: it exists so that a graph needing thousands of splits
/// bails out instead of spending the compile budget arriving at the frame layout
/// `ir_lower` already had.
///
/// ## The subset that is promoted
///
/// A value is a candidate only if **all** of these hold. Everything else keeps
/// the home slot `ir_lower` gives it today, which is always correct:
///
///   * the liveness fixed point converged (otherwise nothing is promoted);
///   * it is not pinned — not a phi (its home is what the edge copies write)
///     and not named by any deopt frame state (the home must hold the value at
///     *any* recorded bci, which is not a property this pass establishes);
///   * its type has a register class (`Void`/`Control`/`Memory` do not);
///   * **if it is a `Ref`, its live range contains no safepoint.**
///
/// ## The `Ref`-at-safepoint decision
///
/// References are kept **in memory across every safepoint**. A `Ref` whose
/// range covers a safepoint position is never promoted at all; one that lives
/// and dies strictly between safepoints may take a register.
///
/// The alternative — allowing a `Ref` in a register across a safepoint and
/// naming that register in the oop map — requires a register bank in the oop
/// map, in the GC's frame walker, and in the deopt frame reconstructor, and it
/// requires the collector to be able to *update* the register on a moving
/// collection. `ir_lower::emit_safepoint_map` publishes frame slots and nothing
/// else. Promoting a reference across a safepoint without that would either
/// hide a live oop from the collector or hand it a stale one after evacuation;
/// both are silent heap corruption. The conservative rule costs GP registers in
/// reference-heavy code and costs nothing in the arithmetic and loop kernels
/// this pass exists for.
pub fn allocate_linear_scan(
    graph: &Graph,
    live: &LiveModel,
    model: &MachineModel,
) -> CompileResult<Allocation> {
    let n = graph.nodes.len();
    model.verify_read_before_write(graph, live)?;

    // ── Fixed constraints: index them and refuse the unsatisfiable ───
    //
    // Two values requiring the same register at the same position is not a
    // heuristic failure, it is an unsatisfiable model. That is what
    // `RegisterPressure` means.
    let mut fixed_at: HashMap<(usize, PhysReg), NodeId> = HashMap::new();
    let mut fixed_of: HashMap<NodeId, Vec<(usize, PhysReg)>> = HashMap::new();
    for f in &model.fixed {
        if !model.regs.contains(f.reg) {
            // Outside the allocatable file: the emitter's fixed scratch. This is
            // the arm every production model takes today, and it is why
            // `for_graph`'s new fixed-operand rules change nothing for a backend
            // that keeps RAX/RCX/RDX for itself.
            continue;
        }
        if let Some(&other) = fixed_at.get(&(f.pos, f.reg)) {
            if other != f.node {
                return Err(Bailout::with_context(
                    BailoutReason::RegisterPressure,
                    format!(
                        "n{} and n{other} both require {} at position {}",
                        f.node, f.reg, f.pos
                    ),
                ));
            }
        }
        fixed_at.insert((f.pos, f.reg), f.node);
        let slots = fixed_of.entry(f.node).or_default();
        if let Some(&(pos, reg)) = slots.iter().find(|(pos, _)| *pos == f.pos) {
            if reg != f.reg {
                return Err(Bailout::with_context(
                    BailoutReason::RegisterPressure,
                    format!(
                        "n{} requires both {reg} and {} at position {pos}",
                        f.node, f.reg
                    ),
                ));
            }
        }
        slots.push((f.pos, f.reg));
    }

    // ── Candidate set ────────────────────────────────────────────────
    let mut promotable = vec![false; n];
    let mut unhandled: BinaryHeap<Reverse<Pending>> = BinaryHeap::new();
    for id in 0..n {
        if !live.wants_loc.get(id).copied().unwrap_or(false) {
            continue;
        }
        let Some(range) = live.range.get(id).copied().flatten() else {
            continue;
        };
        if !live.converged || live.pinned[id] || live.class[id].is_none() {
            continue;
        }
        if live.is_ref[id]
            && !model.refs_may_cross_safepoints
            && model.range_covers_safepoint(range)
        {
            continue;
        }
        if model
            .regs
            .class_size(live.class[id].unwrap_or(RegClass::Gp))
            == 0
        {
            continue;
        }
        promotable[id] = true;
        unhandled.push(Reverse(Pending {
            lo: range.lo,
            hi: range.hi,
            node: id as NodeId,
        }));
    }

    let budget = split_budget(unhandled.len());
    // j27 wave 2 (lane RESIDENCY): read once per scan, see `loop_residency`.
    let loop_aware = loop_phi_residency_enabled();
    let clobbers = ClobberIndex::of(model);
    let mut reg_segments: Vec<Vec<(PosRange, PhysReg)>> = vec![Vec::new(); n];
    let mut active: Vec<Active> = Vec::new();
    let mut splits = 0usize;

    // ── The scan ─────────────────────────────────────────────────────
    while let Some(Reverse(current)) = unhandled.pop() {
        let Pending { lo, hi, node } = current;
        if lo > hi {
            continue;
        }
        let Some(class) = live.class.get(node as usize).copied().flatten() else {
            continue;
        };
        active.retain(|a| a.hi >= lo);

        // A register this value is *required* to hold somewhere in this
        // interval overrides free choice. The EARLIEST such constraint wins:
        // `model.fixed` is sorted by position by both producers, so `min_by_key`
        // is only defensive against a hand-built model.
        //
        // A second constraint later in the same interval — a value used as
        // `idiv`'s dividend and then as a shift count, say — is not honoured by
        // this assignment and is not meant to be. It shows up instead as a
        // BLOCKING position inside `ls_select_register::first_block`, which cuts
        // the register segment before it, so the value is in its home word at
        // the second constraint's position and the emitter loads it into the
        // register that one names. The alternative — pretending one interval can
        // satisfy two different registers — is what proof 4b would reject.
        let required: Option<(usize, PhysReg)> = fixed_of.get(&node).and_then(|slots| {
            slots
                .iter()
                .filter(|(pos, _)| lo <= *pos && *pos <= hi)
                .min_by_key(|(pos, _)| *pos)
                .copied()
        });

        let mut attempts = 0usize;
        let choice = loop {
            attempts += 1;
            if attempts > model.regs.class_size(class).saturating_add(2) {
                break None;
            }
            if let Some(found) =
                ls_select_register(model, &clobbers, live, &active, class, required, current)?
            {
                break Some(found);
            }
            // With a required register, only ITS holder is worth evicting:
            // `ls_select_register` answered `None` because that one register is
            // busy (or blocked, which no eviction fixes), and cutting any other
            // active short frees a register the retry will not take. Every such
            // eviction used to be a split and a spill paid for nothing, up to
            // `class_size + 2` of them per interval.
            let only_reg = required.map(|(_, reg)| reg);
            match ls_pick_victim_with(
                graph,
                live,
                &active,
                class,
                current,
                only_reg,
                loop_aware,
            ) {
                Victim::Current => break None,
                Victim::Active(idx) => {
                    let victim = active.remove(idx);
                    splits += 1;
                    if splits > budget {
                        return Err(Bailout::with_context(
                            BailoutReason::RegisterPressure,
                            format!(
                                "linear scan exceeded {budget} splits over {} registers \
                                 (peak live {})",
                                model.regs.class_size(class),
                                live.peak_live
                            ),
                        ));
                    }
                    // The victim keeps the register up to, but not including,
                    // the position that displaced it.
                    if let Some(seg) = reg_segments
                        .get_mut(victim.node as usize)
                        .and_then(|segs| segs.get_mut(victim.seg))
                    {
                        seg.0.hi = lo.saturating_sub(1);
                    }
                    if let Some(u) = live.next_use_at_or_after(victim.node, lo) {
                        if u <= victim.hi {
                            unhandled.push(Reverse(Pending {
                                lo: u,
                                hi: victim.hi,
                                node: victim.node,
                            }));
                        }
                    }
                }
            }
        };

        match choice {
            Some((reg, until)) => {
                // `until` is the first position at which the register is NOT
                // available (a clobber, or somebody else's fixed constraint),
                // so the segment must stop one short of it. Ending *at* it
                // would leave the value live in a register a call destroys —
                // which `verify_allocation` now rejects outright.
                // An OWN constraint this register does not satisfy blocks it
                // too, and for a sharper reason than somebody else's does.
                //
                // R6 demotes an unhonourable constraint instead of bailing, on
                // the ground that `FixedConstraint` promises only that nobody
                // else is in `reg` there AND that this value is not somewhere
                // else in a register -- both true of a value sitting in its
                // home word. The second half is the one that has to be MADE
                // true: if the value keeps a different register across `pos`,
                // the allocation reports it as resident in a register the
                // instruction cannot use, and an emitter that trusts that
                // reads the wrong operand rather than loading the fixed one
                // from the home word.
                //
                // So an own constraint at `pos` that this register does not
                // satisfy is treated exactly like a clobber at `pos`: the
                // segment stops one short, the value is home across `pos`, and
                // the suffix resumes at the next use. That reuses the split
                // bookkeeping below rather than inventing a second way to end
                // a segment.
                // Bounded by `hi` as well as `lo`. This interval can be a
                // re-queued SUFFIX whose `hi` is short of the value's range end
                // (an evicted victim is re-queued up to the end of the segment
                // it lost, and a later suffix is queued separately). An own
                // constraint beyond `hi` belongs to that later suffix; letting
                // it set `until` here made `end = until - 1` run PAST `hi`, so
                // the segment escaped this interval and overlapped the suffix —
                // an `Internal` error out of `ls_finish` instead of an
                // allocation.
                let own_unhonoured = model
                    .fixed
                    .iter()
                    .filter(|f| f.node == node && f.reg != reg && f.pos >= lo && f.pos <= hi)
                    .map(|f| f.pos)
                    .min();
                let until = match (until, own_unhonoured) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                let end = match until {
                    Some(blocked) => blocked.saturating_sub(1),
                    None => hi,
                };
                // The old input remains located at this read position; only the
                // active ownership passes to the result after its last read.
                active.retain(|a| a.reg != reg || !model.permits_register_handoff(
                    live, a.node, node, PosRange { lo: a.lo, hi: a.hi }, PosRange { lo, hi: end }));
                let segs = &mut reg_segments[node as usize];
                segs.push((PosRange { lo, hi: end }, reg));
                active.push(Active {
                    reg,
                    lo,
                    hi: end,
                    node,
                    seg: segs.len() - 1,
                });
                if let Some(split_at) = until {
                    splits += 1;
                    if splits > budget {
                        return Err(Bailout::with_context(
                            BailoutReason::RegisterPressure,
                            format!("linear scan exceeded {budget} splits at a clobber point"),
                        ));
                    }
                    // The suffix resumes at the first use at or after the
                    // blocking position; between the split and that use the
                    // value simply sits in its home slot.
                    if let Some(u) = live.next_use_at_or_after(node, split_at + 1) {
                        if u <= hi {
                            unhandled.push(Reverse(Pending { lo: u, hi, node }));
                        }
                    }
                }
            }
            None => {
                // Nothing free and this interval lost the eviction contest: it
                // stays in memory until its next use, then competes again.
                if let Some(u) = live.next_use_after(node, lo) {
                    if u <= hi {
                        splits += 1;
                        if splits > budget {
                            return Err(Bailout::with_context(
                                BailoutReason::RegisterPressure,
                                format!(
                                    "linear scan exceeded {budget} splits; {} registers of \
                                     class {class:?} for peak live {}",
                                    model.regs.class_size(class),
                                    live.peak_live
                                ),
                            ));
                        }
                        unhandled.push(Reverse(Pending { lo: u, hi, node }));
                    }
                }
                // A required register that could not be honoured used to be a
                // hard `RegisterPressure` bailout here, on the reading that "the
                // value must be in that register at that position and it is
                // not".
                //
                // **R6 corrected that.** A [`FixedConstraint`] does not promise
                // the value is in a register at `pos` — see its doc comment. It
                // promises that nobody ELSE is in `reg` there and that this
                // value is not somewhere else *in a register*, both of which
                // still hold when the value simply sits in its home word: proof
                // 4b's first arm only fires on `ValueLoc::Reg`. An emitter has
                // to be able to load a fixed operand out of a frame slot in any
                // case, because a pinned or unpromoted value never had a
                // register to begin with.
                //
                // With `for_graph` now emitting a constraint per integer
                // `Div`/`Rem` and per variable shift, keeping the bailout would
                // have turned every register-pressured method containing a
                // division into a refused compile — trading a `mov` for the
                // whole optimized body. The value keeps its home word, which is
                // exactly today's behaviour for it.
                let _ = required;
            }
        }
    }

    // The allocation is checked before it is returned, so no caller can use an
    // unverified one: `verify_allocation` is not an opt-in debug pass.
    let alloc = ls_finish(graph, live, model, reg_segments, splits)?;
    verify_allocation(graph, live, model, &alloc)?;
    Ok(alloc)
}

/// Pick a free register for `current`, or `None` when nothing is usable.
///
/// Returns `(register, split_position)`: `split_position` is `Some(p)` when the
/// register is only usable up to `p - 1` because a clobber or a fixed constraint
/// claims it at `p`.
///
/// `required` is `(position, register)` — the earliest [`FixedConstraint`] on
/// `current.node` that falls inside `current`'s range, if any.
#[allow(clippy::type_complexity)]
pub(in crate::regalloc) fn ls_select_register(
    model: &MachineModel,
    clobbers: &ClobberIndex,
    live: &LiveModel,
    active: &[Active],
    class: RegClass,
    required: Option<(usize, PhysReg)>,
    current: Pending,
) -> CompileResult<Option<(PhysReg, Option<usize>)>> {
    let range = PosRange {
        lo: current.lo,
        hi: current.hi,
    };
    // Compile time (round 11 wave 4): this runs once per interval per attempt
    // and asks every register of the class. `active` holds at most one entry
    // per register, so a linear probe replaces a per-call `HashSet` build, and
    // the fixed constraints inside `range` — the only ones `first_block` can
    // act on — are filtered once here instead of rescanned for each register.
    // Same answers: both are pure re-expressions of the old filters.
    let busy = |reg: PhysReg| active.iter().any(|a| a.reg == reg
        && !model.permits_register_handoff(live, a.node, current.node,
            PosRange { lo: a.lo, hi: a.hi }, range));
    let fixed_in_range: Vec<&FixedConstraint> = model
        .fixed
        .iter()
        .filter(|f| range.contains(f.pos))
        .collect();

    // The first position in `range` at which `reg` is unavailable to
    // `current.node`: a clobber, a fixed constraint belonging to someone else,
    // or — since R6 — a fixed constraint belonging to `current.node` itself that
    // names a DIFFERENT register. Clobbers are found by one binary search in
    // `reg`'s own position list (`ClobberIndex`, round 11 wave 9 — it used to
    // walk every clobber site in `range`, which for a callee-saved register
    // visited every call the interval spans and found nothing), fixed
    // constraints by a scan of the (small) in-range fixed list.
    //
    // That third case is the one R6 added, and it is what keeps a value with two
    // fixed operand roles honest. Without it, a value pinned to RAX at position
    // 10 and to RCX at position 20 would be given RAX for [lo, hi] ⊇ {20} and
    // `verify_allocation`'s proof 4b would reject the result as an internal
    // error — costing the method its optimized body over a shape the allocator
    // can simply split. With it, the segment ends at 19, the value is in its
    // home word at 20, and the emitter loads RCX from there.
    //
    // The index answers "the first clobber of `reg` at or after `lo`", which
    // is the earliest one in `range` because the list is canonically unique
    // and sorted (`MachineModel::canonicalize_clobbers`; `verify_allocation`
    // refuses a model that is not).
    //
    // One clobber is not a block: the one at the value's LAST USE, when the
    // consumer has declared that position reads its operands before it
    // destroys anything (`MachineModel::reads_precede_clobber`, empty unless a
    // consumer opts in). The value is read and dies there, so holding the
    // register through it is the early-clobber shape again, and proof 4
    // exempts it under the same three conditions.
    let last_use_exempt = model.read_before_clobber(live, current.node, range.hi);
    let first_block = |reg: PhysReg| -> Option<usize> {
        let mut earliest: Option<usize> =
            clobbers.first_in(reg, range.lo, range.hi, last_use_exempt);
        for f in fixed_in_range.iter() {
            let blocks = if f.node == current.node {
                f.reg != reg
            } else {
                f.reg == reg
            };
            if blocks {
                earliest = Some(earliest.map_or(f.pos, |e: usize| e.min(f.pos)));
            }
        }
        earliest
    };

    if let Some((req_pos, reg)) = required {
        if busy(reg) {
            return Ok(None);
        }
        return match first_block(reg) {
            // Nothing in the way at all.
            None => Ok(Some((reg, None))),
            // ── Early clobber ────────────────────────────────────────
            //
            // A block landing exactly ON the constraint's own position is the
            // `idiv` shape: RAX carries the dividend IN and the quotient OUT, so
            // the register is both required at `req_pos` and destroyed at
            // `req_pos`. Those are consistent — the read happens before the
            // write inside one instruction — and the endpoint-inclusive overlap
            // rule this file uses says exactly that: a segment ending at
            // `req_pos` does not survive `req_pos`, which is all "clobbered at
            // `req_pos`" forbids.
            //
            // So the value keeps `reg` through `req_pos` and gives it up at
            // `req_pos + 1`. `verify_allocation`'s proof 4 exempts this one
            // segment, and only this one: it must end at the constrained
            // position and the constraint must name this exact node and
            // register.
            //
            // The block at `req_pos` can only be a clobber. A FOREIGN fixed
            // constraint at `(req_pos, reg)` was already rejected by
            // `allocate_linear_scan`'s duplicate scan ("n and n' both require
            // r"), and one of `current.node`'s OWN at `req_pos` naming a
            // different register was rejected by the same scan ("n requires both
            // r and r'"). `clobbered_at` is asked anyway rather than assumed,
            // because this is the one place where being wrong produces a value
            // living in a register a call destroyed; a model built by hand that
            // slips past the duplicate scan falls through to `Ok(None)` and
            // costs the method its promotion instead.
            Some(p) if p == req_pos => {
                if !model.clobbered_at(req_pos).contains(&reg) {
                    return Ok(None);
                }
                if req_pos >= range.hi {
                    Ok(Some((reg, None)))
                } else {
                    Ok(Some((reg, Some(req_pos + 1))))
                }
            }
            // A block strictly BEFORE the constraint: this interval cannot carry
            // the value as far as the position that needs it, so there is no
            // assignment to make. The caller evicts or leaves the value in
            // memory; either way the emitter loads `reg` from the home word.
            Some(p) if p < req_pos => Ok(None),
            // A block strictly after: usable through the constraint, split there.
            Some(p) => Ok(Some((reg, Some(p)))),
        };
    }

    if let Some(write) = model.write_at(range.lo).filter(|w| w.result == current.node) {
        for input in &write.inputs {
            for holder in active.iter().filter(|a| a.node == *input && a.reg.class == class) {
                if !busy(holder.reg) && first_block(holder.reg).is_none() {
                    return Ok(Some((holder.reg, None)));
                }
            }
        }
    }
    let mut fallback: Option<(PhysReg, usize)> = None;
    // A register nothing blocks over the whole interval, in preference order —
    // except that a CALLER-saved one wins over a callee-saved one. An interval
    // with no blocking position in a caller-saved register crosses no call, so
    // it does not need a register that survives one; handing it the
    // callee-saved register anyway (because the file lists those first, as
    // `RegFile::x86_64` and `RegFile::aarch64` do) leaves the next interval that
    // DOES cross a call to split, and makes the prologue save a register a
    // volatile one would have served.
    //
    // A no-op for `ir_lower`'s files as they stand: its GP file is all
    // callee-saved, and its XMM file lists the volatile registers first on both
    // ABIs, so "first unblocked" and "first unblocked caller-saved, else first
    // unblocked" are the same register there.
    let mut free_callee_saved: Option<PhysReg> = None;
    for spec in model.regs.specs().iter().filter(|s| s.reg.class == class) {
        let reg = spec.reg;
        if busy(reg) {
            continue;
        }
        match first_block(reg) {
            None if spec.caller_saved => return Ok(Some((reg, None))),
            None => {
                if free_callee_saved.is_none() {
                    free_callee_saved = Some(reg);
                }
            }
            Some(p) if p > range.lo => {
                // Usable for the prefix. Prefer the register that survives
                // longest — it is the one that needs the fewest splits.
                if fallback.is_none_or(|(_, best)| p > best) {
                    fallback = Some((reg, p));
                }
            }
            Some(_) => {}
        }
    }
    if let Some(reg) = free_callee_saved {
        return Ok(Some((reg, None)));
    }
    Ok(fallback.map(|(reg, p)| (reg, Some(p))))
}

/// The eviction score of `node` asked at `from` (see [`ls_pick_victim_with`]).
/// `loop_aware` is `CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY`: a loop invariant then
/// keeps no carry relief and a per-trip value's weight counts its definition
/// (`loop_residency`); off, the pre-j27 score exactly.
pub(in crate::regalloc) fn ls_spill_score(
    graph: &Graph,
    live: &LiveModel,
    node: NodeId,
    from: usize,
    end: usize,
    loop_aware: bool,
) -> u64 {
    if remat_of(graph, node).is_some() {
        return SPILL_SCORE_REMAT;
    }
    let next = live.next_use_at_or_after(node, from).unwrap_or(end);
    let mut dist = next.saturating_sub(from) as u64;
    // See `ls_carry_relief`: a carried value's distance is measured in the
    // wrong units for the cost it represents. Only one READ in the loop
    // pays per iteration (`LiveModel::carried_and_read`, round 11 wave 5).
    if loop_residency::keeps_carry_relief(live, node as usize, loop_aware) {
        let relief = ls_carry_relief();
        if relief > 1 {
            dist /= relief;
        }
    }
    let w = ranking_weight(live, node as usize, loop_aware).max(1);
    dist.saturating_mul(SPILL_DISTANCE_SCALE) / w
}

/// Choose what loses its register when nothing is free -- the rule documented
/// on `ls_pick_victim` (the test-only switch-off form), with the loop switch
/// passed in (read once per scan by
/// [`allocate_linear_scan`]). Under it a carried phi redefined every trip
/// loses every tie on score: `evictable`, right after the score, is 0 for it
/// (`loop_residency::needed_every_trip` says why it is a key of its own and
/// not a zero distance; j27 wave 4, lane RESIDFLIP). Off, `evictable` is 1 for
/// every candidate and the order is the pre-j27 one. j27 wave 5 (lane
/// UNROLLRES): on, it is `loop_residency::eviction_rank` -- a remainder loop's
/// phi ranks between a main loop's phi and every other value.
pub(in crate::regalloc) fn ls_pick_victim_with(
    graph: &Graph,
    live: &LiveModel,
    active: &[Active],
    class: RegClass,
    current: Pending,
    only_reg: Option<PhysReg>,
    loop_aware: bool,
) -> Victim {
    let score = |node: NodeId, from: usize, end: usize| -> u64 {
        ls_spill_score(graph, live, node, from, end, loop_aware)
    };
    let evictable = |node: NodeId| -> u8 {
        loop_residency::eviction_rank(graph, live, node as usize, loop_aware)
    };

    // The current interval's own next use is strictly after its start: it is
    // being defined (or reloaded) at `lo`, so a use *at* `lo` cannot be served
    // from memory.
    let self_next = live
        .next_use_after(current.node, current.lo)
        .unwrap_or(current.hi);
    let mut best_key = (
        score(current.node, current.lo, current.hi),
        evictable(current.node),
        self_next.saturating_sub(current.lo),
        0u8,
        u32::MAX - current.node,
    );
    let mut best = Victim::Current;

    for (idx, a) in active.iter().enumerate() {
        if a.reg.class != class {
            continue;
        }
        if only_reg.is_some_and(|r| r != a.reg) {
            continue;
        }
        // An active that starts at the same position cannot be cut short —
        // there is no prefix to keep — and evicting it would not advance
        // anything.
        //
        // **This is the termination argument for the eviction path, not a
        // tidiness rule.** The caller re-queues an evicted victim at
        // `next_use_at_or_after(victim.node, current.lo)`, a position bounded
        // below by `current.lo` and by nothing else; only `victim.lo <
        // current.lo`, which is what this line establishes, makes that a strict
        // advance on the victim's own start. Drop it and the scan can evict one
        // interval and re-queue it unchanged, for ever, with only
        // `split_budget` between that and a hung compile. See the termination
        // section on `allocate_linear_scan` (R12).
        if a.lo >= current.lo {
            continue;
        }
        let next = live
            .next_use_at_or_after(a.node, current.lo)
            .unwrap_or(a.hi);
        let key = (
            score(a.node, current.lo, a.hi),
            evictable(a.node),
            next.saturating_sub(current.lo),
            1u8,
            u32::MAX - a.node,
        );
        if key > best_key {
            best_key = key;
            best = Victim::Active(idx);
        }
    }
    best
}

/// Turn the raw register segments into a checked [`Allocation`]: tile every
/// live range, colour the home slots, and derive the transition events.
pub(in crate::regalloc) fn ls_finish(
    graph: &Graph,
    live: &LiveModel,
    model: &MachineModel,
    mut reg_segments: Vec<Vec<(PosRange, PhysReg)>>,
    splits: usize,
) -> CompileResult<Allocation> {
    let n = graph.nodes.len();
    let mut segments: Vec<Vec<Segment>> = vec![Vec::new(); n];
    let mut needs_home = vec![false; n];
    let mut promoted = 0usize;

    for id in 0..n {
        if !live.wants_loc.get(id).copied().unwrap_or(false) {
            continue;
        }
        let Some(range) = live.range.get(id).copied().flatten() else {
            continue;
        };
        let segs = &mut reg_segments[id];
        segs.retain(|(r, _)| r.lo <= r.hi);
        segs.sort_by_key(|(r, _)| (r.lo, r.hi));
        // Two register segments of the same value may not overlap: that would
        // mean the value is in two registers at once and the later store wins.
        for pair in segs.windows(2) {
            if pair[0].0.hi >= pair[1].0.lo {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a value holds two registers at once"),
                    format!(
                        "n{id}: [{}, {}] and [{}, {}]",
                        pair[0].0.lo, pair[0].0.hi, pair[1].0.lo, pair[1].0.hi
                    ),
                ));
            }
        }

        let mut timeline: Vec<Segment> = Vec::with_capacity(segs.len() * 2 + 1);
        let mut cursor = range.lo;
        for &(r, reg) in segs.iter() {
            if r.lo < range.lo || r.hi > range.hi {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a register segment escapes its live range"),
                    format!(
                        "n{id}: segment [{}, {}] outside [{}, {}]",
                        r.lo, r.hi, range.lo, range.hi
                    ),
                ));
            }
            if r.lo > cursor {
                timeline.push(Segment {
                    range: PosRange {
                        lo: cursor,
                        hi: r.lo - 1,
                    },
                    reg: None,
                });
                needs_home[id] = true;
            }
            timeline.push(Segment {
                range: r,
                reg: Some(reg),
            });
            cursor = r.hi.saturating_add(1);
        }
        if cursor <= range.hi {
            timeline.push(Segment {
                range: PosRange {
                    lo: cursor,
                    hi: range.hi,
                },
                reg: None,
            });
            needs_home[id] = true;
        }
        if timeline.iter().any(|s| s.reg.is_some()) {
            promoted += 1;
        }
        segments[id] = timeline;
    }

    let (stack_slot, stack_slots) = ls_color_homes(graph, live, &needs_home);

    // ── Transition events ────────────────────────────────────────────
    let mut events: Vec<SpillEvent> = Vec::new();
    let (mut spills, mut reloads, mut remats, mut reg_moves) = (0usize, 0usize, 0usize, 0usize);
    for id in 0..n {
        let timeline = &segments[id];
        if timeline.len() < 2 {
            continue;
        }
        let node = id as NodeId;
        let rematerializable = remat_of(graph, node).is_some();
        // Where the value is at each position, for the early-clobber store
        // below.
        let reg_at = |p: usize| -> Option<Option<PhysReg>> {
            timeline.iter().find(|s| s.range.contains(p)).map(|s| s.reg)
        };
        // SSA: the home is written once and stays correct, so a value never
        // needs a second store.
        //
        // …and a value DEFINED into its home needs none at all (round 11 wave
        // 2): when the timeline is in memory at the definition's own position,
        // the defining instruction writes the home, and a later register →
        // memory transition finds it already correct. This used to start
        // `false`, so a `memory → register → memory` timeline planned a Store
        // nothing needs and inflated `Allocation::spills`. Keyed on the
        // DEFINITION, not on the first segment: a range widened to block spans
        // can begin in memory before the definition, and then lives in a
        // register when it is defined.
        let mut stored = live
            .pos_of
            .get(id)
            .copied()
            .flatten()
            .and_then(reg_at)
            .is_some_and(|r| r.is_none());
        for pair in timeline.windows(2) {
            let (prev, cur) = (pair[0], pair[1]);
            match (prev.reg, cur.reg) {
                (Some(a), Some(b)) if a != b => {
                    reg_moves += 1;
                    events.push(SpillEvent {
                        node,
                        pos: cur.range.lo,
                        kind: SpillKind::Move,
                        from: Some(a),
                        to: Some(b),
                    });
                }
                (Some(a), None) => {
                    if !rematerializable && !stored {
                        stored = true;
                        // An EARLY-CLOBBER segment (round 11 wave 2) ends AT
                        // the fixed-operand instruction that destroys `a` —
                        // `idiv` reads its dividend in RAX and writes the
                        // quotient there. The register holds the value going
                        // INTO that instruction, not out of it, so a store at
                        // `prev.range.hi` would save the quotient. It belongs
                        // one position earlier, from whatever register holds
                        // the value there; if the value is already in memory
                        // there, its home is valid and nothing is stored.
                        let early_clobber = model.fixed.iter().any(|f| {
                            f.node == node
                                && f.reg == a
                                && f.pos == prev.range.hi
                                && model.regs.contains(f.reg)
                        }) && model.clobbered_at(prev.range.hi).contains(&a);
                        let store = if early_clobber {
                            prev.range
                                .hi
                                .checked_sub(1)
                                .and_then(|p| reg_at(p).flatten().map(|r| (p, r)))
                        } else {
                            Some((prev.range.hi, a))
                        };
                        if let Some((pos, from)) = store {
                            spills += 1;
                            events.push(SpillEvent {
                                node,
                                pos,
                                kind: SpillKind::Store,
                                from: Some(from),
                                to: None,
                            });
                        }
                    }
                }
                (None, Some(b)) => {
                    if rematerializable {
                        remats += 1;
                        events.push(SpillEvent {
                            node,
                            pos: cur.range.lo,
                            kind: SpillKind::Remat,
                            from: None,
                            to: Some(b),
                        });
                    } else {
                        reloads += 1;
                        events.push(SpillEvent {
                            node,
                            pos: cur.range.lo,
                            kind: SpillKind::Load,
                            from: None,
                            to: Some(b),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    events.sort_by_key(|e| (e.pos, e.node));

    Ok(Allocation {
        segments,
        stack_slot,
        stack_slots,
        spills,
        reloads,
        remats,
        reg_moves,
        splits,
        promoted,
        events,
        peak_live: live.peak_live,
    })
}

/// Which home-slot pool a value draws from. Mirror of `ir_lower`'s `SlotClass`:
/// a colour's class is fixed at its first assignment and the pools are
/// disjoint, so a word the oop map names can never come to hold a primitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::regalloc) enum HomeClass {
    Ref,
    Prim,
    Pinned,
}

/// Colour the home frame words for every value that needs one.
///
/// The same expiry-driven colouring `ir_lower::plan_slots` performs, reproduced
/// here because the register allocator changes *which* values need a home: a
/// value that lives its whole life in a register needs none at all, which is
/// the frame-size win on top of the colouring's.
///
/// # R7: why this is not the frame anybody emits, and why it stays anyway
///
/// `ir_lower::plan_slots` lays out the frame the encoder addresses, and it
/// colours a strictly larger value set (every value, not only the ones that
/// touch memory), so the two numberings diverge the moment anything is promoted
/// for its whole range. That is by design on both sides and is spelled out at
/// [`Allocation::stack_slot`].
///
/// The exit recorded for R7 is neither "assert they agree" (they must not) nor
/// "delete this" — an [`Allocation`] has to answer [`Allocation::location_at`]
/// for a consumer that has no frame of its own, and proof 6 already validates a
/// frame that IS emitted, because `ir_lower::mir_alloc_verdict` builds an
/// `Allocation` around `plan.node_color` and runs [`verify_allocation`] over it.
/// The exit is [`Allocation::adopt_home_colouring`]: a consumer with a frame
/// hands it over, and proof 6 then speaks about that frame for the linear-scan
/// path too.
pub(in crate::regalloc) fn ls_color_homes(
    graph: &Graph,
    live: &LiveModel,
    needs_home: &[bool],
) -> (Vec<Option<u32>>, usize) {
    let n = graph.nodes.len();
    let mut color: Vec<Option<u32>> = vec![None; n];
    let mut next_color: u32 = 0;

    // Pinned values first, in node-id order — the order `prealloc_phi_slots`
    // walks — so a phi web's words stay where they have always been.
    for id in 0..n {
        if needs_home.get(id).copied().unwrap_or(false) && live.pinned[id] {
            color[id] = Some(next_color);
            next_color = next_color.saturating_add(1);
        }
    }

    // `Ref` results of `Op::Call` may donate a colour but never receive a
    // recycled one: `emit_safepoint_map` publishes every `Ref` slot defined so
    // far, and the self-recursive route's shadow reload would overwrite a
    // recycled word after the call result was stored into it.
    //
    // The same four ops `ir_lower::plan_slots` treats this way (cov-01): the
    // two `ldc` constants and `getstatic` produce a `Ref` from a helper call
    // too, and publish the map before it. This list named `Op::Call` alone,
    // so for those three the two colourings disagreed about exactly the rule
    // that keeps a word the map already named from being recycled — invisible
    // only because `ir_lower` adopts `plan_slots`' layout
    // (`Allocation::adopt_home_colouring`), and wrong for any consumer that
    // uses this one as is. Pinned by `a_helper_produced_reference_never_recycles_a_home_word`.
    // j27 wave 2 (lane GCMAP): ONE predicate for both colourings, so the list
    // cannot drift again (it is `ir_lower`'s, which `plan_slots_from_core` and
    // `recolour_slot_plan` already share).
    let fresh_only = |id: usize| -> bool {
        graph
            .nodes
            .get(id)
            .is_some_and(crate::ir_lower::colour_is_fresh_only)
    };

    let mut order: Vec<usize> = (0..n)
        .filter(|&id| needs_home.get(id).copied().unwrap_or(false) && !live.pinned[id])
        .collect();
    order.sort_by_key(|&id| (live.range[id].map_or(0, |r| r.lo), id));

    let mut active: BinaryHeap<Reverse<(usize, u32, bool)>> = BinaryHeap::new();
    let mut free_ref: Vec<u32> = Vec::new();
    let mut free_prim: Vec<u32> = Vec::new();
    for id in order {
        let Some(r) = live.range[id] else { continue };
        while let Some(&Reverse((active_hi, active_color, active_is_ref))) = active.peek() {
            if active_hi >= r.lo {
                break;
            }
            active.pop();
            if active_is_ref {
                free_ref.push(active_color);
            } else {
                free_prim.push(active_color);
            }
        }
        let is_ref = live.is_ref.get(id).copied().unwrap_or(false);
        let recycled = if fresh_only(id) {
            None
        } else if is_ref {
            free_ref.pop()
        } else {
            free_prim.pop()
        };
        let c = match recycled {
            Some(c) => c,
            None => {
                let c = next_color;
                next_color = next_color.saturating_add(1);
                c
            }
        };
        color[id] = Some(c);
        active.push(Reverse((r.hi, c, is_ref)));
    }

    (color, next_color as usize)
}

/// The home-slot pool a value belongs to, for the aliasing check.
pub(in crate::regalloc) fn ls_home_class(live: &LiveModel, id: usize) -> HomeClass {
    if live.pinned.get(id).copied().unwrap_or(false) {
        HomeClass::Pinned
    } else if live.is_ref.get(id).copied().unwrap_or(false) {
        HomeClass::Ref
    } else {
        HomeClass::Prim
    }
}

// ── Verification ─────────────────────────────────────────────────────

/// Reject overlapping ranges within one shared resource.
///
/// `group` is every `(range, owner)` pair that occupies the resource. Sorting
/// by start and comparing each entry against the furthest-reaching predecessor
/// is linear and still catches a *nested* range, which a naive
/// compare-with-the-previous-entry sweep misses.
pub(in crate::regalloc) fn ls_check_disjoint(
    what: &'static str,
    resource: &str,
    mut group: Vec<(PosRange, NodeId)>,
    handoff: impl Fn(NodeId, NodeId, PosRange, PosRange) -> bool,
) -> CompileResult<()> {
    group.sort_by_key(|(r, id)| (r.lo, r.hi, *id));
    let mut furthest: Option<(PosRange, NodeId)> = None;
    for (r, id) in group {
        if let Some((p, pid)) = furthest {
            if p.overlaps(r) && !handoff(pid, id, p, r) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(what),
                    format!(
                        "{resource}: n{pid} over [{}, {}] and n{id} over [{}, {}]",
                        p.lo, p.hi, r.lo, r.hi
                    ),
                ));
            }
        }
        if furthest.is_none_or(|(p, _)| r.hi > p.hi) {
            furthest = Some((r, id));
        }
    }
    Ok(())
}


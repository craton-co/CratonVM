#[allow(unused_imports)]
use super::*;

/// Prove an [`Allocation`] is one a backend may emit.
///
/// This is the pass that turns "the heuristic looked right" into "the heuristic
/// is right on this graph". [`allocate_linear_scan`] runs it before returning,
/// so no caller can obtain an unverified allocation; it is exported because a
/// consumer that *builds* or *rewrites* an allocation (a coalescer, a
/// hand-written test fixture) has to be able to re-check its work.
///
/// Every failure is [`BailoutReason::Internal`] — a violation here is a
/// compiler bug, not a property of the input program, and the pressure case is
/// already reported as [`BailoutReason::RegisterPressure`] by the allocator
/// itself. The method loses its optimized body; the VM does not die.
///
/// ## What is proved
///
/// 1. **Shape.** One timeline per graph node, and a value's segments tile its
///    whole live range in order with no gap and no overlap.
/// 2. **Register class.** Every register a value holds is in that value's class
///    ([`RegClass::of`] its type) and is in the allocatable [`RegFile`].
/// 3. **No aliasing.** Register ranges are disjoint except for a declared
///    [`ReadBeforeWrite`] boundary: a source's last read precedes the result's
///    write there. A source live after that boundary cannot share the register.
///    Frame-home ranges remain disjoint under the endpoint-inclusive rule.
/// 4. **ABI.** No value is live in a register across a position that clobbers
///    it, no value holds a register another value is pinned to at that
///    position, and a pinned value never holds a *different* register at its
///    pinned position. (A pinned value that was not promoted at all is not a
///    violation: the prologue leaves it in its home slot, which is where every
///    reader looks today.) The clobber half has exactly one exemption, the
///    early-clobber shape `idiv` needs — see [`FixedConstraint`] and the comment
///    at the check.
/// 5. **The GC rule.** A `Ref` never holds a register across a safepoint — see
///    the decision recorded on [`allocate_linear_scan`].
/// 6. **Homes.** Two values that share a home word have disjoint live ranges,
///    and a home word never mixes [`HomeClass`] pools, so the word the oop map
///    names can never come to hold a primitive.
/// 7. **Bookkeeping.** The event list is in position order and its kinds sum to
///    the reported spill / reload / remat / move counts, so a metrics consumer
///    cannot be handed a number the events do not support.
pub fn verify_allocation(
    graph: &Graph,
    live: &LiveModel,
    model: &MachineModel,
    alloc: &Allocation,
) -> CompileResult<()> {
    let n = graph.nodes.len();
    model.verify_read_before_write(graph, live)?;
    if alloc.segments.len() != n || alloc.stack_slot.len() != n {
        return Err(Bailout::with_context(
            BailoutReason::Internal("regalloc: allocation is not sized for the graph"),
            format!(
                "{n} nodes, {} timelines, {} homes",
                alloc.segments.len(),
                alloc.stack_slot.len()
            ),
        ));
    }
    // The clobber list is read through two access patterns — `clobbered_at`
    // binary-searches it, and `ls_select_register::first_block` and proof 4
    // below read its per-register transpose (`ClobberIndex`, round 11 wave 9;
    // both used to forward-scan it from a `partition_point`) — and only the
    // transpose tolerates two entries at one position. The
    // binary search returns *an* arbitrary match among equals, so a duplicate
    // position silently hides whichever entry it did not land on: a value then
    // holds a caller-saved register across a call that destroys it.
    //
    // Hence STRICTLY increasing, not merely sorted. Checking only `sorted`
    // (`>` rather than `>=`) is what let the invariant that
    // `MachineModel::for_graph`'s own doc comment asks second producers to
    // maintain go unenforced; `canonicalize_clobbers` now establishes it and
    // this rejects any model that lost it. See R2/R3.
    if model.clobbers.windows(2).any(|w| w[0].0 >= w[1].0) {
        return Err(Bailout::with_context(
            BailoutReason::Internal(
                "regalloc: the clobber list is not strictly increasing by position",
            ),
            format!("{} clobber sites", model.clobbers.len()),
        ));
    }
    let clobbers = ClobberIndex::of(model);

    let mut by_reg: BTreeMap<PhysReg, Vec<(PosRange, NodeId)>> = BTreeMap::new();
    let mut by_home: BTreeMap<u32, Vec<(PosRange, NodeId)>> = BTreeMap::new();
    let mut home_pool: BTreeMap<u32, (HomeClass, NodeId)> = BTreeMap::new();

    for id in 0..n {
        let node = id as NodeId;
        let segs = &alloc.segments[id];
        let wants = live.wants_loc.get(id).copied().unwrap_or(false);
        let Some(range) = live.range.get(id).copied().flatten().filter(|_| wants) else {
            if !segs.is_empty() {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a value-less node was given a location"),
                    format!("n{node} has {} segments", segs.len()),
                ));
            }
            continue;
        };

        // ── 1. The timeline tiles the live range ─────────────────────
        let (Some(first), Some(last)) = (segs.first(), segs.last()) else {
            return Err(Bailout::with_context(
                BailoutReason::Internal("regalloc: a live value has no location timeline"),
                format!("n{node} over [{}, {}]", range.lo, range.hi),
            ));
        };
        if first.range.lo != range.lo || last.range.hi != range.hi {
            return Err(Bailout::with_context(
                BailoutReason::Internal("regalloc: a timeline does not cover the live range"),
                format!(
                    "n{node}: timeline [{}, {}] vs range [{}, {}]",
                    first.range.lo, last.range.hi, range.lo, range.hi
                ),
            ));
        }
        for w in segs.windows(2) {
            if w[0].range.hi.saturating_add(1) != w[1].range.lo {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a timeline has a gap or an overlap"),
                    format!(
                        "n{node}: [{}, {}] then [{}, {}]",
                        w[0].range.lo, w[0].range.hi, w[1].range.lo, w[1].range.hi
                    ),
                ));
            }
        }

        // ── 2/4/5. Per-segment class, ABI and GC rules ───────────────
        let mut needs_home = false;
        for seg in segs {
            if seg.range.lo > seg.range.hi {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: an empty segment"),
                    format!("n{node}: [{}, {}]", seg.range.lo, seg.range.hi),
                ));
            }
            let Some(reg) = seg.reg else {
                needs_home = true;
                continue;
            };
            if live.class.get(id).copied().flatten() != Some(reg.class) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a value is in the wrong register class"),
                    format!(
                        "n{node} is {:?} but holds {reg}",
                        live.class.get(id).copied().flatten()
                    ),
                ));
            }
            if !model.regs.contains(reg) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a value holds a non-allocatable register"),
                    format!("n{node} holds {reg}"),
                ));
            }
            if live.pinned.get(id).copied().unwrap_or(false) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a pinned value was promoted"),
                    format!(
                        "n{node} holds {reg} over [{}, {}]",
                        seg.range.lo, seg.range.hi
                    ),
                ));
            }
            // The same rule the candidate set applies, checked again over what
            // was actually produced — and lifted by the same knob, so the
            // verifier cannot be the thing that quietly forbids what the model
            // permits. `MachineModel::refs_may_cross_safepoints` documents who
            // may set it and what they owe for it.
            if live.is_ref.get(id).copied().unwrap_or(false)
                && !model.refs_may_cross_safepoints
                && model.range_covers_safepoint(seg.range)
            {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "regalloc: a reference holds a register across a safepoint",
                    ),
                    format!(
                        "n{node} holds {reg} over [{}, {}], which the oop map cannot describe",
                        seg.range.lo, seg.range.hi
                    ),
                ));
            }
            // No value may be live in a register a call (or a fixed-operand
            // instruction) destroys.
            //
            // ── The one exemption: early clobber ─────────────────────
            //
            // `idiv` reads its dividend in RAX and writes its quotient there, so
            // the model carries both a [`FixedConstraint`] on the dividend at
            // the divide's position and a clobber of RAX at that same position.
            // A segment that ENDS at that position is legal: under this file's
            // endpoint-inclusive overlap rule a segment `[lo, p]` does not
            // survive `p`, and "clobbered at `p`" forbids nothing else.
            //
            // The exemption is deliberately the narrowest statement that covers
            // the case. All four must hold — this exact node, this exact
            // register, this exact position, and the segment must end there — so
            // it cannot be reached by a value that merely happens to be live
            // across a call that also clobbers a register it is pinned to
            // somewhere else in its life.
            //
            // The second disjunct is the same shape without a constraint: a
            // position the consumer declared reads its operands before it
            // clobbers (`MachineModel::reads_precede_clobber`, empty by
            // default), which is this value's last use and the end of its live
            // range. The segment must still END there — `seg.range.hi` is what
            // is tested, so a segment running past the call is still rejected.
            let early_clobber = model.fixed.iter().any(|f| {
                f.node == node
                    && f.reg == reg
                    && f.pos == seg.range.hi
                    && model.regs.contains(f.reg)
            }) || model.read_before_clobber(live, node, seg.range.hi);
            // Round 11 wave 9: one binary search in `reg`'s clobber positions
            // (the list's invariant is checked above), not a walk over every
            // clobber site the segment spans.
            if let Some(pos) = clobbers.first_in(reg, seg.range.lo, seg.range.hi, early_clobber) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a value is live in a clobbered register"),
                    format!(
                        "n{node} holds {reg} over [{}, {}], clobbered at {pos}",
                        seg.range.lo, seg.range.hi
                    ),
                ));
            }
            by_reg.entry(reg).or_default().push((seg.range, node));
        }

        // ── 6. Home words ────────────────────────────────────────────
        if needs_home {
            let Some(color) = alloc.stack_slot.get(id).copied().flatten() else {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a spilled value has no home word"),
                    format!("n{node} over [{}, {}]", range.lo, range.hi),
                ));
            };
            if color as usize >= alloc.stack_slots {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("regalloc: a home word is outside the frame plan"),
                    format!("n{node} at word {color} of {}", alloc.stack_slots),
                ));
            }
            let pool = ls_home_class(live, id);
            // Copied out before the match so the `_` arm can insert: an
            // outstanding `&` into the map would make that arm a borrow error.
            let seen = home_pool.get(&color).copied();
            match seen {
                Some((existing, owner)) if existing != pool => {
                    return Err(Bailout::with_context(
                        BailoutReason::Internal("regalloc: a home word mixes two slot pools"),
                        format!("word {color}: n{owner} is {existing:?}, n{node} is {pool:?}"),
                    ));
                }
                _ => {
                    home_pool.insert(color, (pool, node));
                }
            }
            by_home.entry(color).or_default().push((range, node));
        }
    }

    // ── 3. No two values in one register at one position ─────────────
    for (reg, group) in by_reg {
        ls_check_disjoint(
            "regalloc: two values hold one register at the same position",
            &reg.to_string(),
            group,
            |a, b, ra, rb| model.permits_register_handoff(live, a, b, ra, rb),
        )?;
    }
    for (color, group) in by_home {
        ls_check_disjoint(
            "regalloc: two values share one home word",
            &format!("word {color}"),
            group,
            |_, _, _, _| false,
        )?;
    }

    // ── 4b. Fixed (ABI) constraints ──────────────────────────────────
    for f in &model.fixed {
        if !model.regs.contains(f.reg) {
            continue;
        }
        // The constrained value must not be sitting somewhere else.
        if let Some(ValueLoc::Reg(actual)) = alloc.location_at(f.node, f.pos) {
            if actual != f.reg {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "regalloc: an ABI-pinned value is in the wrong register",
                    ),
                    format!(
                        "n{} needs {} at {} but holds {actual}",
                        f.node, f.reg, f.pos
                    ),
                ));
            }
        }
        // …and nobody else may be occupying the register it is pinned to.
        for id in 0..n {
            let other = id as NodeId;
            if other == f.node {
                continue;
            }
            if alloc.location_at(other, f.pos) == Some(ValueLoc::Reg(f.reg)) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "regalloc: an ABI-pinned register is held by another value",
                    ),
                    format!(
                        "n{other} holds {} at {}, pinned to n{}",
                        f.reg, f.pos, f.node
                    ),
                ));
            }
        }
    }

    // ── 7. The event list supports the reported counts ───────────────
    let (mut stores, mut loads, mut remats, mut moves) = (0usize, 0usize, 0usize, 0usize);
    let mut prev_pos = 0usize;
    for (i, e) in alloc.events.iter().enumerate() {
        if i > 0 && e.pos < prev_pos {
            return Err(Bailout::with_context(
                BailoutReason::Internal("regalloc: spill events are not in position order"),
                format!("event {i} at {} follows {prev_pos}", e.pos),
            ));
        }
        prev_pos = e.pos;
        match e.kind {
            SpillKind::Store => stores += 1,
            SpillKind::Load => loads += 1,
            SpillKind::Remat => remats += 1,
            SpillKind::Move => moves += 1,
        }
    }
    let reported = (alloc.spills, alloc.reloads, alloc.remats, alloc.reg_moves);
    if (stores, loads, remats, moves) != reported {
        return Err(Bailout::with_context(
            BailoutReason::Internal("regalloc: the reported counts do not match the events"),
            format!(
                "events say {stores}/{loads}/{remats}/{moves}, report says {}/{}/{}/{}",
                alloc.spills, alloc.reloads, alloc.remats, alloc.reg_moves
            ),
        ));
    }

    Ok(())
}

// ── Phi resolution: the parallel copy ────────────────────────────────

/// One machine-level move in a *sequentialised* parallel copy.
///
/// The scratch is unnamed on purpose: it is whatever the backend already keeps
/// free at an edge. For `ir_lower::emit_copy_op` it is a reserved FRAME WORD
/// (`phi_copy_scratch_slot_off`), not a register — RAX is only the staging
/// temporary every memory-to-memory move passes through, so a backend that
/// parked a `Save` in RAX would lose it on the next `Move`. (Round 11: this
/// said "RAX", which is the one choice that does not work.) Naming a location
/// here would make the sequence wrong on any backend with a different scratch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyOp {
    /// `to ← from`.
    Move {
        /// Where the value is read from.
        from: ValueLoc,
        /// Where it is written to.
        to: ValueLoc,
    },
    /// `scratch ← from`. Emitted only to break a cycle, always followed later
    /// by exactly one [`CopyOp::Restore`].
    Save {
        /// The location whose value is about to be overwritten.
        from: ValueLoc,
    },
    /// `to ← scratch`.
    Restore {
        /// Where the saved value lands.
        to: ValueLoc,
    },
}

/// Sequentialise a parallel copy: `(destination, source)` pairs that all read
/// their sources **before** any destination is written.
///
/// A phi web on one CFG edge is exactly such a copy. Emitting the pairs in
/// order is wrong whenever a destination is also somebody's source — the
/// classic case being a loop that swaps two locals, whose two phis are each
/// other's incoming value:
///
/// ```text
///   a' ← b
///   b' ← a          emitted naively: a' ← b ; b' ← a'   →   both become b
/// ```
///
/// This returns an order in which every read still sees the pre-copy value:
/// destinations that nothing else reads go first, and when only cycles remain
/// one element is parked in the scratch, which frees its predecessor and
/// unwinds the rest of the cycle. Costs at most one [`CopyOp::Save`] /
/// [`CopyOp::Restore`] pair per cycle and nothing at all on the acyclic
/// majority.
///
/// Errors (as [`BailoutReason::Internal`]) when one destination is written
/// twice, which is not a copy this can sequentialise — it is a malformed web.
pub fn resolve_parallel_copy(copies: &[(ValueLoc, ValueLoc)]) -> CompileResult<Vec<CopyOp>> {
    // `None` as a source means "the scratch", which is where a broken cycle
    // parks the value the last move of that cycle has to read.
    let mut pending: Vec<(ValueLoc, Option<ValueLoc>)> = Vec::with_capacity(copies.len());
    // Round 11 wave 8: the two scans below used to be linear searches of
    // `pending` — a duplicate-destination search per input pair and a
    // "does anyone still read this destination" search per candidate per
    // pass — which made one edge O(n³) in its copy count. A merge's copy
    // count is its φ count, which a 20 000-node graph can put in the
    // hundreds on every edge of a switch join. Two maps made it O(n²) (the
    // lowest-index ready scan and `Vec::remove` stayed until wave 9, below),
    // and the emitted sequence is unchanged: the same entry is chosen at
    // every step.
    //
    // `src_of[dst]` = the source already recorded for `dst`.
    let mut src_of: HashMap<ValueLoc, ValueLoc> = HashMap::with_capacity(copies.len());
    for &(dst, src) in copies {
        if dst == src {
            // A copy to itself is not a move, and treating it as one would
            // invent a false dependency that forces a scratch.
            continue;
        }
        match src_of.get(&dst).copied() {
            // The same pair listed twice is idempotent — a merge reached along
            // two edges of the same predecessor asks for the identical write.
            Some(s) if s == src => continue,
            Some(_) => {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "regalloc: a parallel copy writes one destination twice",
                    ),
                    format!("{dst:?}"),
                ));
            }
            None => {
                src_of.insert(dst, src);
                pending.push((dst, Some(src)));
            }
        }
    }
    // `readers[loc]` = how many pending entries still read `loc` (a `Some`
    // source). A destination is ready exactly when its count is zero.
    let mut readers: HashMap<ValueLoc, usize> = HashMap::with_capacity(pending.len());
    for &(_, src) in &pending {
        if let Some(s) = src {
            *readers.entry(s).or_insert(0) += 1;
        }
    }

    // Round 11 wave 9 (irback proposal W8-2): the step that used to rescan
    // `pending` for the lowest-index ready entry and then `Vec::remove` it is
    // now a min-heap of ready indices, so one edge is O(n log n). A MIN-heap
    // rather than a FIFO keeps the emitted sequence the one the rescanning
    // version chose, op for op (`ir_lower`'s φ-edge bytes do not move): an
    // entry enters the heap exactly when its destination's reader count
    // reaches zero (counts only fall), so the heap is always the ready set,
    // and its minimum is the lowest-index ready entry. The cycle breaker
    // takes the lowest-index live entry, as `pending.first()` did.
    //
    // `dst_index[loc]` = the entry writing `loc`; `reads[loc]` = the entries
    // whose source is `loc`.
    let n = pending.len();
    let mut dst_index: HashMap<ValueLoc, usize> = HashMap::with_capacity(n);
    let mut reads: HashMap<ValueLoc, Vec<usize>> = HashMap::with_capacity(n);
    for (i, &(dst, src)) in pending.iter().enumerate() {
        dst_index.insert(dst, i);
        if let Some(s) = src {
            reads.entry(s).or_default().push(i);
        }
    }
    let mut done = vec![false; n];
    let mut queued = vec![false; n];
    let mut ready: BinaryHeap<Reverse<usize>> = BinaryHeap::new();
    for (i, &(dst, _)) in pending.iter().enumerate() {
        if readers.get(&dst).copied().unwrap_or(0) == 0 {
            queued[i] = true;
            ready.push(Reverse(i));
        }
    }
    let mut first_live = 0usize;
    let mut left = n;

    let mut out: Vec<CopyOp> = Vec::with_capacity(n + 2);
    // Each pass either retires an entry or breaks one cycle (which immediately
    // makes an entry retirable), so twice the entry count plus a constant
    // bounds the loop even if the invariant above were ever violated.
    let mut guard = n.saturating_mul(2).saturating_add(4);
    while left > 0 {
        if guard == 0 {
            return Err(Bailout::with_context(
                BailoutReason::Internal("regalloc: parallel copy sequencing did not terminate"),
                format!("{left} copies left"),
            ));
        }
        guard -= 1;

        // A destination nothing still reads can be written now.
        match ready.pop() {
            Some(Reverse(i)) => {
                let Some(&(dst, src)) = pending.get(i) else {
                    return Err(Bailout::with_context(
                        BailoutReason::Internal("regalloc: parallel copy lost its state"),
                        format!("ready index {i} of {n}"),
                    ));
                };
                if done[i] {
                    continue;
                }
                done[i] = true;
                left -= 1;
                out.push(match src {
                    Some(from) => {
                        if let Some(r) = readers.get_mut(&from) {
                            *r = r.saturating_sub(1);
                            if *r == 0 {
                                if let Some(&j) = dst_index.get(&from) {
                                    if !done[j] && !queued[j] {
                                        queued[j] = true;
                                        ready.push(Reverse(j));
                                    }
                                }
                            }
                        }
                        CopyOp::Move { from, to: dst }
                    }
                    None => CopyOp::Restore { to: dst },
                });
            }
            None => {
                // Every remaining destination is also a source. Each has
                // exactly one source (destinations are unique) and at least one
                // reader, so what is left is a disjoint union of pure cycles.
                // Park one element and its cycle unwinds.
                while first_live < n && done[first_live] {
                    first_live += 1;
                }
                let Some(&(cycle, _)) = pending.get(first_live) else {
                    // Unreachable: `left > 0` means some entry is not done.
                    return Err(Bailout::with_context(
                        BailoutReason::Internal("regalloc: parallel copy lost its state"),
                        format!("{left} copies left, none live"),
                    ));
                };
                out.push(CopyOp::Save { from: cycle });
                if let Some(readers_of_cycle) = reads.get(&cycle) {
                    for &j in readers_of_cycle {
                        if !done[j] {
                            if let Some(entry) = pending.get_mut(j) {
                                if entry.1 == Some(cycle) {
                                    entry.1 = None;
                                }
                            }
                        }
                    }
                }
                readers.insert(cycle, 0);
                if !queued[first_live] {
                    queued[first_live] = true;
                    ready.push(Reverse(first_live));
                }
            }
        }
    }
    Ok(out)
}

// `phi_edge_copies` — the per-edge phi parallel copy derived from an
// `Allocation` — was deleted in round 11 wave 2. It had no production caller
// (`ir_lower::gather_phi_copies` + `emit_copy_op` are the live path, and are
// tested there), its doc said a phi is never promoted (false since
// `LiveModel::release_phi_pins`, where it failed with "a phi has no home
// word"), and it rescanned every node per predecessor block.

// ── CFG-edge consistency of a split timeline ─────────────────────────

/// A layout-order emitter's compile-time belief about one value at one linear
/// position, as [`split_edge_conflicts`] and [`split_edge_resolution`] model it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::regalloc) enum LsLinearState {
    /// Outside the value's live range.
    Dead,
    /// In its home word — or not yet defined in LAYOUT order, which a
    /// write-through emitter treats the same way (it has published nothing).
    Memory,
    /// In this register.
    Reg(PhysReg),
}

/// What a layout-order emitter believes about `id` at `pos`.
///
/// The timeline, except that a position laid out BEFORE the value's own
/// definition is [`LsLinearState::Memory`]: liveness is widened to whole block
/// spans, so a loop-carried value's range (and its first register segment) can
/// start before the node that defines it, and an emitter walking the blocks in
/// layout order has published nothing there. `ir_lower::ls_split_transitions`
/// refuses every transition before the definition for the same reason. Reading
/// "the register the timeline names" there would be reading a register the
/// emitter never filled on that path.
pub(in crate::regalloc) fn ls_linear_state(live: &LiveModel, segs: &[Segment], id: usize, pos: usize) -> LsLinearState {
    let Some(seg) = segs.iter().find(|s| s.range.contains(pos)) else {
        return LsLinearState::Dead;
    };
    if live
        .pos_of
        .get(id)
        .copied()
        .flatten()
        .is_some_and(|def| pos < def)
    {
        return LsLinearState::Memory;
    }
    match seg.reg {
        Some(reg) => LsLinearState::Reg(reg),
        None => LsLinearState::Memory,
    }
}

/// One value whose location on a CFG edge does not match what the code at the
/// edge's target was emitted against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::regalloc) struct LsEdgeMismatch {
    pub(in crate::regalloc) pred: usize,
    pub(in crate::regalloc) succ: usize,
    pub(in crate::regalloc) node: usize,
    /// The register the target block's code reads the value from.
    pub(in crate::regalloc) want: PhysReg,
    /// Where the edge actually delivers it: `Reg`, `Memory`, or `Dead` — the
    /// last only when the model says the value IS live-in at the target and
    /// the timeline has no location for it at the predecessor's edge, which
    /// no well-formed allocation produces and which no copy can repair.
    pub(in crate::regalloc) have: LsLinearState,
}

/// Every [`LsEdgeMismatch`] of every multi-segment value, in `(pred, succ)`
/// edge order. The one enumeration both [`split_edge_conflicts`] and
/// [`split_edge_resolution`] read, so the value set the gate refuses and the
/// value set the resolver repairs cannot drift apart.
pub(in crate::regalloc) fn ls_edge_mismatches(
    schedule: &Schedule,
    live: &LiveModel,
    alloc: &Allocation,
) -> Vec<LsEdgeMismatch> {
    let mut out: Vec<LsEdgeMismatch> = Vec::new();
    // Only a value with more than one segment can change location, and that is
    // a handful per method: gather them once rather than scanning every node
    // on every edge.
    let multi: Vec<usize> = alloc
        .segments
        .iter()
        .enumerate()
        .filter(|(_, segs)| segs.len() > 1)
        .map(|(id, _)| id)
        .collect();
    if multi.is_empty() {
        return out;
    }
    for (p, block) in schedule.blocks.iter().enumerate() {
        let Some(&(_, p_edge)) = live.span.get(p) else {
            continue;
        };
        for &s in &block.successors {
            let Some(&(s_start, _)) = live.span.get(s) else {
                continue;
            };
            // Position 0 has no linear predecessor (the emitter enters it with
            // nothing resident, which every edge satisfies); and the layout
            // fall-through is the one edge a linear emitter follows exactly.
            if s_start == 0 || p_edge.saturating_add(1) == s_start {
                continue;
            }
            let carried = s_start - 1;
            for &id in &multi {
                let segs = alloc.segments[id].as_slice();
                // A register claim at the top of `s` is the only belief an
                // edge can contradict: memory is always right for a
                // write-through emitter, and so is "not resident".
                let LsLinearState::Reg(want) = ls_linear_state(live, segs, id, carried) else {
                    continue;
                };
                let live_in = live.live_in_at(s, id as NodeId);
                if live_in == Some(false) {
                    // Nothing on any path from `s` reads it before it is dead:
                    // a stale belief about its register is never acted on.
                    continue;
                }
                let have = ls_linear_state(live, segs, id, p_edge);
                match have {
                    LsLinearState::Reg(r) if r == want => {}
                    // Not live on this edge by the range, and the model has no
                    // better answer: a value live-out of `p` has `span[p].1`
                    // inside its range by the construction of
                    // `build_live_model`, so nothing on this path reads it.
                    LsLinearState::Dead if live_in.is_none() => {}
                    _ => out.push(LsEdgeMismatch {
                        pred: p,
                        succ: s,
                        node: id,
                        want,
                        have,
                    }),
                }
            }
        }
    }
    out
}

/// Which values a consumer that follows the allocation in **linear** order
/// would get wrong on some CFG edge. `result[id]` is `true` for such a value.
///
/// # The gap this names
///
/// An [`Allocation`] is a timeline over LINEAR positions — block layout order —
/// and [`allocate_linear_scan`] performs no edge resolution during the scan:
/// when it splits a value, the location it records at the start of a block is
/// simply whatever the previous position in layout order had. That is the
/// location on the fall-through edge only. On any other incoming edge the value
/// may be somewhere else:
///
/// ```text
///   header (pos 3..)      v in r0          <- timeline carried in from the preheader
///   body   (pos 6)        v evicted; r0 handed to w
///   latch  (edge 9)       v in memory      -> back edge to the header
/// ```
///
/// The header's code reads `v` from `r0` on every iteration, and from the
/// second iteration on `r0` holds `w`. The same shape arises at any join whose
/// predecessors are not all laid out immediately before it.
///
/// A consumer that emits ONE register for a value's whole range (every
/// production path by default) never sees this: a single segment has one
/// location everywhere. A consumer that follows the per-position timeline —
/// `ir_lower`'s split residency, `CRATONVM_JIT_IR_LS_SPLITS`, default off — does,
/// because it carries its residency state across block boundaries in layout
/// order too.
///
/// # The rule
///
/// For every edge `p -> s` that is NOT the layout fall-through into `s`
/// (`span[p].1 + 1 != span[s].0`), and every value with more than one segment:
/// if the emitter believes the value is in register `r` at `span[s].0 - 1` —
/// the position whose state a linear emitter carries into `s` — then it must be
/// in `r` at `p`'s edge position too. Being in memory at `s` is always
/// consistent for a write-through consumer (the home word is written at the
/// definition); being in a register there is consistent only if every
/// predecessor put it there.
///
/// "Believes" is the timeline with one correction (round 9 wave 2): at a
/// position laid out before the value's own definition the emitter has
/// published nothing, so it believes "memory" there whatever the timeline's
/// register says — see `ls_linear_state`. That makes a predecessor laid out
/// before the definition deliver the value in memory (flagged if the target
/// claims a register; the wave-1 rule compared raw timelines and passed it),
/// and a target laid out before the definition claim nothing (never flagged).
///
/// A value the model knows is not live-in at `s` ([`LiveModel::live_in_at`])
/// is skipped: nothing on a path from `s` reads it. When liveness is unknown,
/// a value whose range does not cover `p`'s edge position is skipped instead
/// (a value live-out of `p` has `span[p].1` inside its range by the
/// construction of [`build_live_model`]).
///
/// Conservative in one direction only: a value this returns `false` for is
/// consistent on every edge; one it returns `true` for may still have been
/// safe (a register claim immediately dropped at the block's first position,
/// say). The cost of that imprecision is one declined promotion.
///
/// # The fix
///
/// [`split_edge_resolution`] computes, for exactly the mismatches this flags
/// (they share one enumeration), the per-edge parallel copy that repairs them.
/// A consumer that emits those copies may keep the values this flags; one that
/// does not must refuse them.
pub fn split_edge_conflicts(
    schedule: &Schedule,
    live: &LiveModel,
    alloc: &Allocation,
) -> Vec<bool> {
    let mut conflict = vec![false; alloc.segments.len()];
    for m in ls_edge_mismatches(schedule, live, alloc) {
        if let Some(cell) = conflict.get_mut(m.node) {
            *cell = true;
        }
    }
    conflict
}

/// Where the copies of one CFG edge may be emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeCopyPlacement {
    /// At the end of the predecessor, after its phi copies' reads: the
    /// predecessor has this one successor, so every execution of that point
    /// takes this edge.
    PredEnd,
    /// At the start of the successor, before its first node AND before the
    /// timeline's own transitions at that node (`Allocation::events` at
    /// `span[s].0`, which assume the carried state these copies establish):
    /// the successor has this one predecessor, so every entry into it runs
    /// them.
    SuccStart,
    /// A critical edge (several successors, several predecessors). The copies
    /// need a block of their own on the edge; a consumer that cannot split an
    /// edge must refuse every value in [`EdgeResolution::nodes`].
    Critical,
}

/// The parallel copy one CFG edge needs so that the successor's code — emitted
/// in layout order against the fall-through state — finds every value where it
/// looks for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeResolution {
    /// Source block of the edge.
    pub pred: usize,
    /// Target block of the edge.
    pub succ: usize,
    /// Where the copies may run.
    pub placement: EdgeCopyPlacement,
    /// `(destination, source)` pairs, simultaneous — the convention a phi
    /// edge's parallel copy uses, so a consumer can append these to the edge's phi
    /// copies and sequence both through ONE [`resolve_parallel_copy`] (a phi
    /// copy may read a register a resolution copy writes, and vice versa).
    /// Destinations are always registers; a source is a register or the
    /// value's home word.
    pub copies: Vec<(ValueLoc, ValueLoc)>,
    /// `nodes[i]` = the value `copies[i]` moves.
    pub nodes: Vec<NodeId>,
}

/// [`split_edge_resolution`]'s answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SplitEdgeResolution {
    /// One entry per edge that needs at least one copy, in `(pred, succ)`
    /// order. Every copy of an `unresolvable` value has been removed.
    pub edges: Vec<EdgeResolution>,
    /// `unresolvable[id]` = some edge needs a copy for `id` that cannot be
    /// expressed or cannot be emitted without destroying another value (see
    /// [`split_edge_resolution`]). A consumer must refuse split residency for
    /// such a value exactly as it refuses a [`split_edge_conflicts`] value.
    pub unresolvable: Vec<bool>,
}

/// **Edge resolution** for a split timeline — the second half of Wimmer's
/// linear scan, which [`allocate_linear_scan`] does not perform during the scan
/// and [`split_edge_conflicts`] only detects.
///
/// For every mismatch [`split_edge_conflicts`] flags on an edge `p -> s` — the
/// code of `s` reads value `v` from register `want`, and the edge delivers it
/// elsewhere — this emits the copy that puts it there:
///
/// * delivered in another register `r` → `(Reg(want), Reg(r))`;
/// * delivered in memory (a memory segment at `p`'s edge position, or `p` laid
///   out before `v`'s definition) → `(Reg(want), Slot(home))`, a reload from
///   the home word. Correct for a **write-through** consumer, which writes the
///   home at the definition, and the definition dominates every block `v` is
///   live-in at. A non-write-through consumer additionally needs its one
///   `SpillKind::Store` to dominate that reload, which `ls_finish` does not
///   arrange (it places the store at the first register → memory transition in
///   layout order).
///
/// The copies target the state at `span[s].0 - 1`, NOT at `span[s].0`: a
/// layout-order emitter carries its state from the previous position and then
/// applies the timeline's own events at `span[s].0` (a `Load` or `Move` there
/// is in [`Allocation::events`]), so the edge must reproduce the carried state
/// and nothing more.
///
/// # What makes a value unresolvable
///
/// * The edge delivers it nowhere the timeline can name (`Dead` while the model
///   says it is live-in) — no source to copy from.
/// * It must be reloaded and has no home word in this allocation
///   ([`Allocation::stack_slot`] is `None`; never the case after
///   [`Allocation::adopt_home_colouring`] with a write-through consumer's
///   frame).
/// * Two values would be copied into one register on one edge (a malformed
///   allocation; [`verify_allocation`]'s proof 3 excludes it for a verified one).
/// * **The destination register belongs to another value at the top of `s`.**
///   A value `y` whose timeline puts it in `want` at `span[s].0` itself (its
///   range, widened to block spans, can begin exactly there) would be destroyed
///   by the copy — on a path where, without the copy, `want` still held `y`.
///
/// Every copy of an unresolvable value is removed from every edge, because a
/// consumer that refuses the value must not then write its register.
///
/// # What the consumer still owes
///
/// * Filter [`EdgeResolution::nodes`] to the values it actually keeps in split
///   residency before sequencing: a copy for a value the consumer refused for
///   its own reasons writes a register that value no longer owns.
/// * Run the copies AFTER anything on the edge that can invalidate a register
///   (a back edge's safepoint poll calls into the runtime, and a `Ref` reloaded
///   before it would be a stale root), and sequence them together with the
///   edge's phi copies.
/// * Honour [`EdgeResolution::placement`]; a [`EdgeCopyPlacement::Critical`]
///   edge needs a split block or a refusal.
///
/// Deterministic: edges in `(pred, succ)` enumeration order, copies in node
/// order within an edge.
pub fn split_edge_resolution(
    schedule: &Schedule,
    live: &LiveModel,
    alloc: &Allocation,
) -> SplitEdgeResolution {
    let n = alloc.segments.len();
    let mut unresolvable = vec![false; n];
    let mismatches = ls_edge_mismatches(schedule, live, alloc);
    if mismatches.is_empty() {
        return SplitEdgeResolution {
            edges: Vec::new(),
            unresolvable,
        };
    }

    // Does any value other than `except` hold `reg` at `pos` by the timeline —
    // split or not?
    let held_by_other = |reg: PhysReg, pos: usize, except: usize| -> bool {
        alloc.segments.iter().enumerate().any(|(id, segs)| {
            id != except
                && segs
                    .iter()
                    .any(|seg| seg.reg == Some(reg) && seg.range.contains(pos))
        })
    };

    let mut edges: Vec<EdgeResolution> = Vec::new();
    for m in &mismatches {
        let src = match m.have {
            LsLinearState::Reg(r) => ValueLoc::Reg(r),
            LsLinearState::Memory => match alloc.stack_slot.get(m.node).copied().flatten() {
                Some(word) => ValueLoc::Slot(word),
                None => {
                    unresolvable[m.node] = true;
                    continue;
                }
            },
            LsLinearState::Dead => {
                unresolvable[m.node] = true;
                continue;
            }
        };
        let Some(&(s_start, _)) = live.span.get(m.succ) else {
            unresolvable[m.node] = true;
            continue;
        };
        if held_by_other(m.want, s_start, m.node) {
            unresolvable[m.node] = true;
            continue;
        }
        let same_edge = edges
            .last()
            .is_some_and(|e| (e.pred, e.succ) == (m.pred, m.succ));
        if !same_edge {
            let placement = if schedule.blocks[m.pred].successors.len() == 1 {
                EdgeCopyPlacement::PredEnd
            } else if schedule
                .blocks
                .get(m.succ)
                .is_some_and(|b| b.predecessors.len() == 1)
            {
                EdgeCopyPlacement::SuccStart
            } else {
                EdgeCopyPlacement::Critical
            };
            edges.push(EdgeResolution {
                pred: m.pred,
                succ: m.succ,
                placement,
                copies: Vec::new(),
                nodes: Vec::new(),
            });
        }
        if let Some(edge) = edges.last_mut() {
            edge.copies.push((ValueLoc::Reg(m.want), src));
            edge.nodes.push(m.node as NodeId);
        }
    }

    // One destination written twice on one edge: every writer is refused.
    for edge in &edges {
        for (i, (dst, _)) in edge.copies.iter().enumerate() {
            if edge.copies[..i].iter().any(|(d, _)| d == dst) {
                for (j, (d, _)) in edge.copies.iter().enumerate() {
                    if d == dst {
                        unresolvable[edge.nodes[j] as usize] = true;
                    }
                }
            }
        }
    }

    // A refused value's copies go everywhere, not only on the edge that
    // refused it.
    for edge in edges.iter_mut() {
        let (copies, nodes): (Vec<(ValueLoc, ValueLoc)>, Vec<NodeId>) = edge
            .copies
            .iter()
            .copied()
            .zip(edge.nodes.iter().copied())
            .filter(|&(_, v)| !unresolvable[v as usize])
            .unzip();
        edge.copies = copies;
        edge.nodes = nodes;
    }
    edges.retain(|e| !e.copies.is_empty());

    SplitEdgeResolution {
        edges,
        unresolvable,
    }
}


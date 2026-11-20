#[allow(unused_imports)]
use super::*;

/// Which of `n` candidates are written on every path from block 0 before every
/// GC point: a must-written dataflow (the greatest fixpoint: every block starts
/// at "all written", block 0 at "none", any other block with no predecessor --
/// entered only by a transfer from a GC point -- at "all"), then one check of
/// every GC point. `None` when it does not
/// converge within `max_rounds` sweeps (nothing is left out then). Pure.
pub(in crate::ir_lower) fn block_lazy_clear_survivors(
    blocks: &[LazyClearBlock],
    n: usize,
    max_rounds: usize,
) -> Option<Vec<bool>> {
    let nb = blocks.len();
    if nb == 0 || n == 0 {
        return Some(vec![false; n]);
    }
    let block_in = |b: usize, out: &[Vec<bool>]| -> Vec<bool> {
        if b == 0 {
            return vec![false; n];
        }
        let preds: Vec<usize> = blocks[b].preds.iter().copied().filter(|&p| p < nb).collect();
        if preds.is_empty() {
            return vec![true; n];
        }
        let mut acc = vec![true; n];
        for p in preds {
            for (a, o) in acc.iter_mut().zip(out[p].iter()) {
                *a &= *o;
            }
        }
        acc
    };
    let mut out: Vec<Vec<bool>> = vec![vec![true; n]; nb];
    let mut converged = false;
    for _ in 0..max_rounds {
        let mut changed = false;
        for b in 0..nb {
            let mut cur = block_in(b, &out);
            for step in &blocks[b].steps {
                if let LazyClearStep::Write(c) = *step {
                    if let Some(cell) = cur.get_mut(c) {
                        *cell = true;
                    }
                }
            }
            if cur != out[b] {
                out[b] = cur;
                changed = true;
            }
        }
        if !changed {
            converged = true;
            break;
        }
    }
    if !converged {
        return None;
    }
    let mut alive = vec![true; n];
    let kill = |alive: &mut [bool], cur: &[bool]| {
        for (a, c) in alive.iter_mut().zip(cur.iter()) {
            *a &= *c;
        }
    };
    for b in 0..nb {
        let mut cur = block_in(b, &out);
        for step in &blocks[b].steps {
            match *step {
                LazyClearStep::Gc => kill(alive.as_mut_slice(), &cur),
                LazyClearStep::Write(c) => {
                    if let Some(cell) = cur.get_mut(c) {
                        *cell = true;
                    }
                }
            }
        }
        if blocks[b].gc_at_end {
            kill(alive.as_mut_slice(), &cur);
        }
    }
    Some(alive)
}

/// Leave the lazy words (`[rbp - off]`) out of the fresh-frame clear's mask
/// `skip` (indexed by word `w` = `[rbp - 8(w + 1)]`, empty for "none"), except
/// a lazy word whose two neighbours are both cleared: splitting a run for one
/// word costs a store rather than saving one, and clearing it is always
/// correct. Pure.
pub(in crate::ir_lower) fn merge_block_lazy_clear_skip(skip: &mut Vec<bool>, lazy: &[i16], frame_size: i32) {
    if lazy.is_empty() || frame_size <= 0 || frame_size % 8 != 0 {
        return;
    }
    // Cast: a positive frame size over 8.
    let words = (frame_size / 8) as usize;
    if skip.len() != words {
        if !skip.is_empty() {
            // A mask of another shape: leave it as the frame lane built it.
            return;
        }
        *skip = vec![false; words];
    }
    let mut mine: Vec<usize> = Vec::new();
    for &off in lazy {
        let off = i32::from(off);
        if off <= 0 || off % 8 != 0 || off > frame_size {
            continue;
        }
        // Cast: a positive frame offset over 8.
        let w = (off / 8 - 1) as usize;
        if let Some(cell) = skip.get_mut(w) {
            if !*cell {
                *cell = true;
                mine.push(w);
            }
        }
    }
    let lone: Vec<usize> = mine
        .iter()
        .copied()
        .filter(|&w| {
            let below = w.checked_sub(1).is_some_and(|v| skip[v]);
            let above = skip.get(w + 1).copied().unwrap_or(false);
            !below && !above
        })
        .collect();
    for w in lone {
        skip[w] = false;
    }
    if !skip.iter().any(|&s| s) {
        skip.clear();
    }
}

/// The receiver section (above [`ir_shadow_live_receiver_enabled`]) as a
/// predicate: the switch, an instance method
/// (`receiver_param == Some(0)`), known not to be synchronized, and no
/// self-lock. Pure.
pub(in crate::ir_lower) fn shadow_live_receiver_admitted(
    receiver_param: Option<u16>,
    method_synchronized: Option<bool>,
    self_locks: bool,
    switch_on: bool,
) -> bool {
    switch_on && receiver_param == Some(0) && method_synchronized == Some(false) && !self_locks
}

/// JIT round 25 item 2: does the copy of a reference parameter scheduled at
/// `param = (block, position)` run before the GC point at `site = (block,
/// position)` on every path to it? A later position in the same block, or a
/// block the parameter's block dominates -- the rule
/// [`lazy_ref_param_home_zero_plan`] places a lazy clear by. Pure.
pub(in crate::ir_lower) fn param_copy_precedes_site(
    param: (usize, usize),
    site: (usize, usize),
    dominates: impl Fn(usize, usize) -> bool,
) -> bool {
    if param.0 == site.0 {
        site.1 > param.1
    } else {
        dominates(param.0, site.0)
    }
}

/// The pure half of `Lowerer::plan_lazy_ref_param_home_zero`. `params`: per
/// candidate `(node, prologue offset, colour home offset, block, position)`;
/// `clears`: the planned first-byte clears per site (each list ascending);
/// `site_at(site)`: the site's `(block, position)`; `dominates(a, b)`.
/// Returns the `(site, prologue offset)` stores to add and the candidates
/// handled lazily (ascending as `params`): every candidate whose every
/// planned clear of its home lies after its copy. A candidate with a planned
/// clear that does not (or that cannot be placed) is left out of both: it
/// keeps the eager store.
pub(in crate::ir_lower) fn lazy_ref_param_home_zero_plan(
    params: &[(NodeId, i32, i32, usize, usize)],
    clears: &HashMap<NodeId, Vec<i32>>,
    site_at: impl Fn(NodeId) -> Option<(usize, usize)>,
    dominates: impl Fn(usize, usize) -> bool,
) -> (Vec<(NodeId, i32)>, Vec<NodeId>) {
    let mut sites: Vec<NodeId> = clears.keys().copied().collect();
    sites.sort_unstable();
    let mut adds: Vec<(NodeId, i32)> = Vec::new();
    let mut lazy: Vec<NodeId> = Vec::new();
    'param: for &(id, param_off, home_off, block, pos) in params {
        let mut mine: Vec<(NodeId, i32)> = Vec::new();
        for &site in &sites {
            let Some(offs) = clears.get(&site) else {
                continue;
            };
            if !offs.contains(&home_off) {
                continue;
            }
            let Some((site_block, site_pos)) = site_at(site) else {
                continue 'param;
            };
            let after_copy = if site_block == block {
                site_pos > pos
            } else {
                dominates(block, site_block)
            };
            if !after_copy {
                continue 'param;
            }
            mine.push((site, param_off));
        }
        adds.extend(mine);
        lazy.push(id);
    }
    (adds, lazy)
}

/// The reference `Op::Param` nodes whose prologue word may be zeroed after
/// their copy (the section above), ascending: index >= 1 (or index 0 too when
/// `param0_admitted`: a static method's first parameter, gci i4/ir), the only
/// `Op::Param` of its index, scheduled in block 0, which must be the entry
/// block (`entry_block`, [`scheduled_entry_block`]) and have no predecessor.
/// Pure.
pub(in crate::ir_lower) fn ref_param_home_zero_candidates(
    graph: &Graph,
    node_to_block: &[usize],
    entry_block: usize,
    entry_has_predecessors: bool,
    param0_admitted: bool,
) -> Vec<NodeId> {
    if entry_block != 0 || entry_has_predecessors {
        return Vec::new();
    }
    let mut per_index: HashMap<u16, u32> = HashMap::new();
    for node in &graph.nodes {
        if let Op::Param(idx) = node.op {
            *per_index.entry(idx).or_insert(0) += 1;
        }
    }
    let mut out: Vec<NodeId> = Vec::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        let Op::Param(idx) = node.op else {
            continue;
        };
        if node.ty != IrType::Ref
            || (idx == 0 && !param0_admitted)
            || per_index.get(&idx).copied() != Some(1)
        {
            continue;
        }
        if node_to_block.get(id).copied() != Some(0) {
            continue;
        }
        if let Ok(id) = NodeId::try_from(id) {
            out.push(id);
        }
    }
    out
}

// hga (`r16-codegen-hashmap-get-anatomy`): the clears of a call that may take
// an inline prefix run on its call path, not at its first byte. The prefixes
// (`Integer`/`Long.valueOf`'s cache read and box, `Integer.hashCode` /
// `equals`, `ArrayList.get` / `size`) answer the call inline and skip it, and
// none of their hit paths is a GC point, so nothing there needs a dead home
// zero. The stale-set union then treats such a site as zeroing nothing for
// the code after it (`StaleEvent::Site::cleans`), so the next site that does
// clean zeroes what the skipped one would have. The dense staging clear of
// `CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS` (`emit_dead_arg_stage_clears`) is held
// the same way: it zeroes every staging word at each call it runs at, so the
// next real call still finds them zero at its GC point. On `HashMap.get(i &
// 63)` the valueOf site was 14 stores on every iteration of the hit loop (8
// homes, 3 staging words planned, the same 3 again densely).
//
// `CRATONVM_JIT_IR_PREFIX_SLOW_PATH_CLEARS=0` restores the first-byte clears
// and the cleaning site byte for byte. Read per compile, no static
// (`process_global_statics_ratchet`).



// ── Dead-home clears at allocation slow paths (JIT round 20, I-9) ─────
//
// Only an `Op::Call` used to arm the clears, so a collection reached from an
// `Op::New` / `Op::NewArray` helper saw the homes as the last call left them:
// `GC21IAllocDeadHome`'s `param-box` row, where `paramBox` reads the array out
// of a box, clears the box, uses the array and then allocates, threw an
// `OutOfMemoryError` HotSpot 25 does not (`p`'s home still held the array and
// the allocation's map published it).
// `docs/feature-designs/jit-ir-optimizations.md` item I-9.
//
// With this switch an allocation node is an armed site
// (`Lowerer::dead_ref_clear_site_for`) with three differences from a call:
//
//   * its planned clears (and the opt-in dense staging clear) are HELD, as a
//     prefixed call's are, and emitted only where nothing but the helper call
//     follows: on the deferred slow path of `Op::New` (before its map), in
//     front of a stub-only `Op::New` / `Op::NewArray`, and on the slow path of
//     the primitive-array bump, which this arm now lands itself
//     (`emit_newarray_bump_with_slow_path_clears`). The bump paths carry no
//     store. A route whose helper call sits inside an emitter that also emits
//     the fast path (the post-init `Op::New` bump) disarms the site before its
//     map instead (`disarm_alloc_site`): no clear and no liveness claim, the
//     bytes it had before;
//   * the stale-set union never counts it as cleaning
//     (`StaleEvent::Site::cleans`), so a home a fast path left alone is still
//     "may be stale" at the next site;
//   * its reachability keep set takes no def kills (`DeadRefSlotClear::
//     alloc_sites`): an allocation is not a memory token, so the load-kill
//     argument, which leans on a call ordering the load, does not hold there.
//
// Why the cleared set holds nothing live after the allocation: it is
// `dead_colours_at` at the allocation's position, i.e. no value range covers
// that position (the allocation's own inputs, the length, end AT it and are
// kept; its own result starts there and is kept) and the keep set does not
// name it. The keep set is the schedule liveness after the allocation PLUS
// the allocation's own reads -- the frame state at its bci (raw and resolved
// out of a splice) and its per-copy frame -- which is exactly what its
// exceptional exits use: an `OutOfMemoryError`, a `NegativeArraySizeException`
// or an `ExceptionInInitializerError` leaves through `push_call_exc_patch`
// (no call-throw plan and no catch arm for an allocation), i.e. a rethrow pad
// at `resume_bci(cur_bci)` inside a protected range (locals narrowed by the
// handler-aware liveness, so a local the handler reads is kept) or the
// propagating exit, which reads only the monitors (never narrowed). Neither
// arm emits a deopt. Only zero is written.
//
// Why the claim (`precise_liveness_claim`, made at the armed map) is matched
// by a zeroing: every GC this site can reach is inside the helper call, and
// on every route that keeps the site armed the held clears run before that
// call; a dead home the claim leaves out is therefore zero (cleared here, or
// not stale since its last clear) at every collection that reads the claim.
//
// `CRATONVM_JIT_IR_ALLOC_DEAD_HOME_CLEAR=0` restores the call-only sites byte
// for byte. Read per compile, no static (`process_global_statics_ratchet`).
// Default ON as a retention fix; positive control `GC21IAllocDeadHome
// param-box` (HotSpot 25: `false`), and `CRATONVM_DBG_IR_SLOTS=1` names the
// allocation node in `dead-home clears at n<id>`.



// ── Dead-home clears at the remaining helper GC points (JIT round 21, oom) ─
//
// HotSpot's oop maps are liveness-exact at every safepoint. After I-9 three
// GC-capable helper sites of this tier still published a dead home (their map
// is an unarmed one: every defined node's home):
//
//   * the post-init `Op::New` bump -- the route of EVERY `new` under ZGC, the
//     default collector (`InlineTlabPlan::skip_post_init` is false whenever the
//     collector requires registration), and of a class with post-init work
//     elsewhere. I-9 disarmed it because its helper call sits inside the
//     emitter that also emits the fast path. Its fast path makes a helper call
//     anyway (`jit_post_tlab_init` / the ZGC announce), so the held clears now
//     run in front of the map on BOTH paths, as on the stub-only route;
//   * `Op::NewCp` (a `new` of a class not loaded at compile time): all slow
//     path, so its clears run at its first byte, as a call's do. Its helper
//     resolves the class and can run `<clinit>` -- arbitrary Java, any number
//     of collections -- with this frame below it;
//   * `Op::MonitorEnter`'s slow path: a contended enter parks this thread for
//     whole collections. The clears are held past the inline thin lock and run
//     in front of the map, so the uncontended enter carries no store.
//
// Each is a non-cleaning site for the stale-set union (only an `Op::Call`
// cleans, `StaleEvent::Site::cleans`), takes the plain reachability union
// without def kills (`is_plain_union_site_op`), and keeps its own frame state
// (`is_dead_home_site_op`), which is what its exceptional exit reads: the
// `push_call_exc_patch` route at its bci, as for an allocation. The argument
// for why the cleared set holds nothing live is the I-9 one above, word for
// word: the cleared colours are `dead_colours_at` at the site.
//
// `CRATONVM_JIT_IR_HELPER_SITE_DEAD_HOME_CLEAR=0` restores the I-9 sites byte
// for byte (and `CRATONVM_JIT_IR_ALLOC_DEAD_HOME_CLEAR=0` turns these off with
// them). Read per compile, no static. Correctness (a HotSpot-visible retention),
// default ON; it is a switch because the post-init route now pays its stores
// on the fast path. Evidence: `CRATONVM_DBG_IR_SLOTS=1` lists the `NewCp` /
// `MonitorEnter` / post-init `New` node in `dead-home clears at n<id>`; probe
// `R21OomHelperSites` (`C:\craton\jr21-ssh\probes\`).



// ── The optimizing tier's owned-arguments hand-off and callee window ────────
//
// JIT round 21, lane argwin (OOM21-1 of
// `docs/feature-designs/jit-ir-optimizations.md`; page
// `jr21-compiled-caller-roots-its-outgoing-arguments-for-the-whole-callee-FIXED-20261026.md`,
// I-8). A compiled caller kept every reference argument of a direct call
// rooted for the whole callee -- its home in the call's map and shadow push,
// re-read by the callee-deopt service after the call -- and the optimizing
// callee kept each reference PARAMETER's prologue home in every map of its
// body. So an object passed and dead in the callee stayed reachable where
// HotSpot (whose outgoing arguments belong to the callee's liveness-exact
// map) collects it: an `OutOfMemoryError` HotSpot does not throw.
//
// Two steps, each its own switch, in build order:
//
// 1. `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS` -- the service half, the twin of the
//    single-pass gcd d9/d hand-off (`x64/op_invoke.rs`,
//    `sp_direct_call_args_owned_by_callee`). A statically bound direct cross
//    call (`invokestatic` kind 3, or an instance opcode of kind 1) whose
//    callee owns its arguments (`crate::direct_callee_owns_its_arguments`: no
//    exception table, not synchronized, no frameless trap stub, no indy trap)
//    passes `SERVICE_ARGS_OWNED_BY_CALLEE` and stages nothing on its cold
//    side. The service then never re-runs the callee from entry nor runs its
//    handler with the arguments -- the two readers of the arguments after the
//    call -- and leaves a declined or frameless callee trap as THIS method's
//    frameless deopt, which its own caller answers by re-running it from
//    entry (`FramelessTrapSite::Handoff`, tier-agnostic in the VM). So the site
//    is admitted only where that re-run commits nothing twice and nothing of
//    this frame is needed after the trap: a method-entry (not OSR) compile, not
//    a self-locking body, no debugger, the call not inside a splice, no
//    exception-table row covering its bci (and no reason-9 pad, call-site pad
//    or compiled-handler arm at the site, whose exits read this frame), and
//    the bytecode before it, closed over loops, makes no store, invoke or
//    monitor operation (`deopt::replay_from_entry_commits_nothing_with_handlers`,
//    the single-pass rule's own function and arguments). Alone it changes no
//    retention: the homes stay mapped.
// 2. `CRATONVM_JIT_IR_CALLEE_ARG_WINDOW` (needs step 1's admission) -- the map
//    half. At an owned site, between the argument marshal and the `CALL`, the
//    home of every ARGUMENT-ONLY reference argument is zeroed, and so is its
//    word of the site's shadow push (the frame-block form publishes the home
//    itself, indirectly). The map keeps naming the home, which now holds null,
//    and the post-call reload copies the null back. "Argument-only"
//    ([`Lowerer::owned_call_arg_window`]): a `Ref` argument with a home in
//    the spill band, used once by the call and by no phi, whose value range
//    ends at the call, in a call block on no cycle, and that NO frame state
//    readable after the call names -- neither one a node scheduled after the
//    call can read (by its own snapshot, its pc, or its resolved pc), nor one
//    at a bci reachable from the invoke's successors over normal and
//    exception edges, nor a splice or per-copy state that a later bci could
//    resume through. That last rule is the split lane oom asked for: the
//    invoke's OWN snapshot names the arguments on its operand stack, but
//    nothing after the transfer reads that snapshot at an owned site (its
//    exits are frameless, its post-call exit reads the successor's state), and
//    a node after the call that did read it refuses the argument.
//
// The callee half (OOM21-3: a reference PARAMETER's prologue home kept in
// every map of the callee) needs no step of its own: the default
// `CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO` (gci i3/ir, the section above
// `ir_ref_param_home_zero_enabled`) makes the parameter's colour its only
// frame word, which the dead-home clears zero where it is dead.
//
// Each is read per compile, no static. Census: `CRATONVM_DBG_IR_SLOTS=1`
// prints one `[ir-owned-args] site` line per admitted or refused direct call
// site and one `[ir-owned-args] window` line per owned site.

/// `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS` -- default ON, `=0` off (OOM21-1 step
/// 1; WARNING argwin-flip-20261026). Also off whenever the single-pass
/// hand-off's kill switch `CRATONVM_JIT_CALLEE_OWNED_ARGS=0` is set, so one
/// word withdraws both tiers' hand-offs. See the section above.
pub(in crate::ir_lower) fn ir_callee_owned_args_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY` -- opt-in, default OFF, `=0` off
/// (j27 wave 4, lane ARGFIX2; proposal
/// `j27-argfix-spliced-owned-args-two-level-replay-proposal-FIXED-20261029.md`).
/// Acts only under [`ir_callee_owned_args_enabled`] (no bytecode otherwise):
/// a direct call INSIDE a spliced body, refused by step 1's `in_splice` term,
/// is admitted when re-running the OUTER method from bci 0 is exact at every
/// level of the chain (`crate::owned_args_splice_replay`). Missing or masked
/// body code still refuses the site. Read per site, no static.
pub(in crate::ir_lower) fn ir_callee_owned_args_splice_replay_enabled() -> bool {
    false
}



/// Round 21 also built that half as its own opt-in step, kept as built (default
/// OFF; the gci i3/ir default above covers it):
/// `CRATONVM_JIT_IR_PARAM_HOME_LIVENESS=1` -- opt-in (OOM21-3). See the
/// section above.
pub(in crate::ir_lower) fn ir_param_home_liveness_enabled() -> bool {
    false
}

/// OOM21-1: the SITE half of the optimizing tier's owned-arguments rule, pure
/// so the table is testable without a compile (the twin of the single-pass
/// `sp_direct_call_args_owned_by_callee`). `handlers` are the method's
/// `(start_pc, end_pc, handler_pc)` rows; `replay_exact` is asked last (it
/// scans the bytecode).
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_lower) fn ir_direct_call_site_admits_owned_args(
    opcode: Option<u8>,
    invoke_kind: u8,
    bci: usize,
    handlers: &[(usize, usize, usize)],
    osr_compile: bool,
    self_locking: bool,
    in_splice: bool,
    armed_exits: bool,
    replay_exact: impl FnOnce() -> bool,
) -> bool {
    ir_direct_call_site_owned_args_refusal(
        opcode,
        invoke_kind,
        bci,
        handlers,
        osr_compile,
        self_locking,
        in_splice,
        armed_exits,
        replay_exact,
    )
    .is_none()
}

/// The first site term that refuses an owned-argument operation. Keeping this
/// separate from the boolean wrapper makes debug diagnostics and precedence
/// tests use the same decision as code generation.
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_lower) fn ir_direct_call_site_owned_args_refusal(
    opcode: Option<u8>,
    invoke_kind: u8,
    bci: usize,
    handlers: &[(usize, usize, usize)],
    osr_compile: bool,
    self_locking: bool,
    in_splice: bool,
    armed_exits: bool,
    replay_exact: impl FnOnce() -> bool,
) -> Option<&'static str> {
    let statically_bound = matches!(
        (opcode, invoke_kind),
        (Some(0xb8), 3) | (Some(0xb6 | 0xb7 | 0xb9), 1)
    );
    if !statically_bound {
        return Some("not statically bound");
    }
    if osr_compile {
        return Some("OSR compile");
    }
    if self_locking {
        return Some("self-locking body");
    }
    if in_splice {
        return Some("inside a splice");
    }
    if armed_exits {
        return Some("armed exit");
    }
    let protected = handlers
        .iter()
        .any(|&(start, end, _)| start <= bci && bci < end);
    if protected {
        return Some("protected handler");
    }
    if !replay_exact() {
        return Some("caller replay is not exact");
    }
    None
}

/// Whether a state is reachable from a call without executing its fresh
/// allocation. Exception edges from the allocation remain paths because the
/// allocation may fail before producing the new value.
pub(in crate::ir_lower) fn ir_owned_cycle_bytecode_path_without_array(
    call_pc: usize,
    snapshot_pc: usize,
    array_pc: usize,
    successors: impl Fn(usize) -> Vec<usize>,
    handlers: &[(usize, usize, usize)],
) -> bool {
    let mut work = successors(call_pc);
    for &(start, end, handler) in handlers {
        if start <= call_pc && call_pc < end {
            work.push(handler);
        }
    }
    let mut seen = std::collections::HashSet::new();
    while let Some(pc) = work.pop() {
        if pc == snapshot_pc {
            return true;
        }
        if !seen.insert(pc) {
            continue;
        }
        if pc != array_pc {
            work.extend(successors(pc));
        }
        for &(start, end, handler) in handlers {
            if start <= pc && pc < end {
                work.push(handler);
            }
        }
    }
    false
}

/// A cross-block call cycle is safe only when every re-entry path passes
/// through the allocation block. The caller separately proves that the
/// allocation block dominates the call block.
pub(in crate::ir_lower) fn ir_owned_cycle_reentry_crosses_definition(
    definition_block: usize,
    call_block: usize,
    successors: &[Vec<usize>],
) -> bool {
    if definition_block == call_block || call_block >= successors.len() {
        return false;
    }
    let mut seen = vec![[false; 2]; successors.len()];
    let mut work: Vec<(usize, bool)> = successors[call_block]
        .iter()
        .copied()
        .map(|block| (block, block == definition_block))
        .collect();
    let mut reentry = false;
    while let Some((block, crossed_definition)) = work.pop() {
        if block >= successors.len() {
            return false;
        }
        let crossed_definition = crossed_definition || block == definition_block;
        if block == call_block {
            if !crossed_definition {
                return false;
            }
            reentry = true;
            continue;
        }
        let state = &mut seen[block][usize::from(crossed_definition)];
        if std::mem::replace(state, true) {
            continue;
        }
        work.extend(
            successors[block]
                .iter()
                .copied()
                .map(|next| (next, crossed_definition || next == definition_block)),
        );
    }
    reentry
}

/// The narrow cycle-reentry proof used by the service-copy window: only a
/// reference array whose allocation is guaranteed to execute again before
/// every repeated call may be treated as a fresh value.
pub(in crate::ir_lower) fn ir_owned_cycle_arg_is_fresh_array(ty: IrType, op: &Op, reexecutes_each_cycle: bool) -> bool {
    ty == IrType::Ref && matches!(op, Op::NewArray { .. }) && reexecutes_each_cycle
}



// ── The liveness-exact keep set, on by default (gen r5w6/oomjit10) ───
//
// The gen r5w5 keep set was liveness-BLIND in three ways, and each left a
// probe's dropped data rooted by an optimizing frame's home
// (`gengc-r5w5-oomjit9-ir-dead-homes-residuals-CLOSED-20261012.md` items 2 and 3):
//
//   * a colour was kept at a call when ANY snapshot a later node reads names
//     it -- so a value a loop recomputes every round (the `ref.get()` of
//     `cleared(ref)`'s loop, named by the `ifnull` at the top of the next
//     round) was kept across the `System.gc()` of this round. The keep set is
//     now a backward LIVENESS over the schedule that kills a colour at its
//     sole occupant's definition ([`snapshot_colours_live_after_each_call`]);
//     the bytecode-reachability union survives only for the snapshots no
//     scheduled node reads;
//   * every snapshot named every LOCAL, dead or not -- so a local the method
//     is done with (`NativeGrowthReclaimProbe`'s `s` and `cs` after their
//     loop) stayed named at every later bci. Locals are now narrowed by the
//     handler-aware bytecode liveness (`regalloc::live_locals_per_pc_all`)
//     at the method's own instructions;
//   * phi homes were never cleared -- so a loop-carried local kept its last
//     round's value in the phi home after the loop. Phi colours are now
//     clearable: dead where no range covers the call and the keep set does
//     not name them, zeroed when an edge copy may have written them since
//     (`plan_dead_home_clears`' phi `Def`s).
//
// What makes each sound is written at the function that does it. Every one
// only ever NARROWS what a frame state keeps for a value no reader reads
// before a redefinition, and the three GC channels (frame block, band scan,
// maps) read the home -- which the default-on clears zero -- so they follow.
//
// `CRATONVM_JIT_IR_PRECISE_KEEP_SET=0` restores the gen r5w5 keep set byte
// for byte (all three). Read per compile, no static
// (`process_global_statics_ratchet`). Default ON as a retention fix.



// ── Kills in the bytecode-reachability union (gcd d4/o) ───────────────
//
// Under the precise keep set, the snapshots no scheduled node reads are still
// kept through a liveness-BLIND union: every colour such a snapshot names is
// kept at every call from which its bci is reachable over the method's own
// bytecode (`snapshot_colours_reachable_with_splices`). The snapshots a
// splice records at pcs no node carries -- a spliced accessor's `areturn`,
// naming the returned value -- are all of that kind, and they are credited
// to the enclosing invoke's own bci (`splice_named`). So a value a loop
// recomputes through a spliced call (`cleared(ref)`'s `ref.get()`, where
// `Reference.get` is spliced into the loop) is named at the invoke that
// recomputes it, which the back edge makes reachable from every later call
// of the loop body: it stayed kept across the loop's `System.gc()`, the
// default-on dead-home clears never zeroed its home, and the frame block
// published it (`GenR5W2OsrDeadSlotProbe`, census `cleared off=160
// region=operand-spill tier=ir in_map=true`, `jit-shadow-stack-indirect`).
//
// The kill: a colour whose sole occupant `v` is defined at own bci `D` is
// not credited from a snapshot reached only THROUGH `D` (the walk does not
// pass `D`, and a snapshot at `D` or credited to `D` counts only when `D` is
// the walk's own start). Why that is sound: every snapshot naming `v` is
// dominated by `v` (SSA; a loop-carried value reaches a later round through a
// phi, which is a different node), so on any path from the call through `D`
// to such a snapshot, `v` is executed again after `D` is entered and the
// snapshot reads the NEW value; the old one in the home has no reader
// there. That needs `v`'s definition to execute whenever `D` does, so it is
// taken only for nodes that cannot move off their bci: a call or allocation
// (control-pinned, `ir_schedule::pinned_anchor`) or a memory-ordered load
// (`Load` / `ArrayLoad` / `LoadStatic`: a call anywhere between the site and
// `D` orders it after that call), with no per-copy frame (`frame_snapshot`:
// not a hoisted or cloned copy). Everything else keeps the union unchanged.
//
// `CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS=0` restores the union byte for byte.
// Read per compile, no static (`process_global_statics_ratchet`).
//
// gcd d5/u (2026-09-28): the same switch also gates the PHI half of the
// kills, in the liveness-exact keep set. d4/o's kill did not change the
// probe (orchestrator, `d916d1c40`: `GenR5W2OsrDeadSlotProbe` `FAIL 3 of 3`
// with the switch on and off), because the value the `ifnull` of `cleared`'s
// loop names is not the spliced load: `ref`'s static type is not exact, so
// `ref.get()` is a GUARDED splice (`begin_guarded_receiver_splice`), whose
// result is `Phi(call on the miss edge, the body's return)` at the splice's
// join (`finish_multi_return_splice`). The keep-set liveness
// ([`snapshot_colours_live_after_each_call`]) never killed a phi's colour
// ("their homes are written by edge copies, which this keeps no position
// for"), so the `ifnull` snapshot at the top of the NEXT round kept this
// round's result live across the `System.gc()` through the back edge, and
// the phi's home (pinned, so named by the frame block) stayed a root.
//
// The kill: a `Ref` phi's colour is killed at the ENTRY of its merge block
// ([`phi_entry_kills`]) when every incoming edge writes its home -- one
// value per edge, none `NO_NODE`, none the phi itself, none sharing its
// colour. Why that is exact: the edge copies (`emit_phi_copies`) run at the
// end of every predecessor, after its last node and after the edge's
// back-edge poll (whose mode exit reads the header phis' EDGE INPUTS, never
// the phi homes: `back_edge_mode_exit_state`), so on every path from a call
// through the merge the home is rewritten before any node of the merge
// block, and every later reader reads the NEW value. A phi is pinned (its
// colour is its own), so no other value lives in that home. A self-copy (the
// phi as its own input, or an input in the same home) writes nothing and is
// excluded; so is an edge with no input. The phi's compiled-code uses keep
// their protection: this narrows only the snapshot keep set, and the
// clearing rule still spares every colour a live range covers.



// ── Shared-colour kills in the keep-set liveness (gci i40/os) ─────────
//
// The liveness-exact keep set ([`snapshot_colours_live_after_each_call`])
// killed a colour at a definition only when that node was the colour's SOLE
// occupant. Its argument was "a colour a snapshot names is pinned, hence one
// node's alone" -- true of a flat snapshot (`plan_slots` pins every value one
// names), false since round 14 wave 1 for a value only CHAIN snapshots name
// (`CRATONVM_JIT_IR_CHAIN_SNAPSHOT_RANGE_PINS`, default ON: such a value gets a
// widened colouring range instead of a pin, so it SHARES its word with other
// short-lived references). Every value of a spliced body is of that kind. So
// in an optimizing body that splices a loop -- `cleared(ref)` spliced into
// `caseDropWarm`'s OSR body (`gci-i31om`, Batch 9a K: the holder is that
// frame's `off=272 region=operand-spill in_map=true`, sp 12, inside the
// splice of the invoke at bci 65) -- the spliced `ref.get()` result's colour
// had two occupants, the kill at its definition never fired, and the next
// round's `ifnull` (whose chain snapshot names it) kept it live backwards
// through the definition and the back edge across the round's
// `System.gc()`: the home was neither zeroed by the dead-home clears nor
// dropped from the map, and the dropped list stayed a root. The same body
// compiled on its own (`cleared` as a separate frame, the passing
// `drop-warm`-alone row) has no chain snapshot, so every value is pinned and
// the d4/o / d5/u / d6/u rules apply; that is the only difference between
// the two doors of vi38-ds K.
//
// The kill: a non-φ `Ref` node of a call, allocation or memory-ordered load
// ([`def_kill_op`]) kills its colour at its own step whatever the colour's
// occupancy. Why that is exact: the question the liveness answers at a site
// S is "may a frame-state reader after S read the word as S leaves it?". On a
// path from S through such a definition the word is rewritten at the
// definition (a reference home is authoritative: no lowering drops a `Ref`
// home store, `value_home_droppable` admits `Int` / `Long` only), so every
// reader past it reads that write or a later one, whichever occupant it
// names. And a reader of ANOTHER occupant whose value predates the
// definition cannot exist on such a path: that occupant would be live across
// the definition, its colouring range would cover the definition's position,
// and `verify_slot_colouring` refuses two overlapping ranges in one colour.
// Zeroing the word at S or claiming it dead there therefore changes nothing
// any reader reads. φs keep their own rule (`phi_entry_kills`: their homes
// are written by edge copies, never at their step); `Param` / `OsrLocal`
// and every other op keep the sole-occupant rule. Only the schedule-side
// liveness changes; the bytecode-reachability union's kills
// (`refine_reach_by_def_kills`, `def_pc`) still require a sole occupant,
// because passing a spliced definition's OWN bci (its enclosing invoke) does
// not imply the definition executed.
//
// `CRATONVM_JIT_IR_KEEP_SET_SHARED_COLOUR_KILLS=0` restores the sole-occupant
// rule byte for byte. Read once per keep-set build, no static
// (`process_global_statics_ratchet`). Default ON as a retention fix (HotSpot
// C2's oop map at that call does not hold the dead list).



// ── An OSR arm's locals are read on the arm only (gcj hmg) ──────────
//
// `build_opaque_osr_merge` gives an OSR arm one `Op::OsrLocal` per live local.
// Its readers are the arm's own nodes -- the class guards of
// `guard_osr_arm_exact_locals`, whose snapshot names it, and the edge copy into
// the header's pre-merge φ -- and the arm reaches no GC point before that
// copy. Past the merge the local is the φ. But the keep set credits a snapshot
// to every node at its bci (`named[bci]`), and the arm's guard snapshot sits at
// the loop header's bci, which every round of the loop passes; so the
// `OsrLocal`'s colour was kept across every call of the activation, its home
// was never zeroed, and the interpreter's value at entry stayed a root until
// the OSR'd method returned. `GcfF3qPromoteKeepProbe hashmap-grow` (OSR at its
// inner `put` loop) kept the first batch's 90 MB `HashMap` through every major
// of the call (`oldmark-root-census`: `region=operand-spill tier=ir`, the
// `OsrLocal(6)` colour) and threw `OutOfMemoryError` at `-Xmx256m`, where
// HotSpot and `--nojit` complete.
//
// With the switch a snapshot entry that is an `OsrLocal` sole occupant names
// nothing in the keep set (`plan_dead_ref_slot_clear`'s `sp_bits`), so the
// planned clears zero its home at the first GC point past the arm and the
// liveness claim calls it dead there. The arm's own deopt reads the home before
// any such point. A shared colour keeps the old rule (another occupant may be
// the one named). Read once per keep-set build, no static.



/// Is snapshot entry `v` an OSR arm's `Op::OsrLocal` whose colour it alone
/// occupies? See the section above [`ir_keep_set_osr_locals_arm_only_enabled`].
pub(in crate::ir_lower) fn osr_local_named_on_its_arm_only(
    graph: &Graph,
    plan: &SlotPlan,
    occupants: &[u32],
    v: NodeId,
) -> bool {
    if !matches!(graph.nodes.get(v as usize).map(|n| &n.op), Some(Op::OsrLocal(_))) {
        return false;
    }
    match plan.node_color.get(v as usize).copied().flatten() {
        Some(c) => occupants.get(c as usize).copied() == Some(1),
        None => false,
    }
}

/// gci i40/os: the colour the keep-set liveness kills at the step of a node
/// with op `op`, type `ty` and colour `colour`, given each colour's occupant
/// count. A φ kills nothing here (its entry kill is `phi_entry_kills`); a sole
/// occupant kills its colour (gen r5w6/oomjit10); under `shared_kills` a
/// `Ref` [`def_kill_op`] node kills its colour whatever the occupancy (see
/// the section above). `None` for a node with no colour. Pure, for the unit
/// tests.
pub(in crate::ir_lower) fn keep_set_step_kill(
    op: &Op,
    ty: IrType,
    colour: Option<u32>,
    occupants: &[u32],
    shared_kills: bool,
) -> Option<usize> {
    if matches!(op, Op::Phi) {
        return None;
    }
    let c = colour? as usize;
    if occupants.get(c).copied() == Some(1) {
        return Some(c);
    }
    (shared_kills && ty == IrType::Ref && def_kill_op(op)).then_some(c)
}

// ── Exitless steps at a spliced pc read no snapshot (gci i47w/ks47) ───
//
// Every step of [`snapshot_colours_live_after_each_call`] read the union of
// every snapshot recorded at its pc (`named[pc]`) and, at a spliced pc, at its
// enclosing own invoke (`named[own]`). For a step that can publish an exit
// that is the right over-approximation (the exit resolves its frame by that
// pc). A φ cannot, and the join of a GUARDED splice puts its φs at a pc that
// belongs to ONE incoming path: `finish_multi_return_splice` stamps the join's
// `Merge` and value / memory φs with `exit_bci`, the hit body's `areturn`, and
// the chain snapshot recorded at the top of that `areturn` names the hit
// path's result -- the spliced `Reference.referent` load. So the join φ of
// `ref.get()` read the load's colour on the MISS edge too, where this round
// never defined it: the previous round's value stayed live round the loop
// across `System.gc()` (`gci-i31om`: colour 24, `off=272`, `cleared(ref)`
// spliced into `caseDropWarm`'s OSR body, `after` at n64 / n73 / n74; the
// exit's `ref.get()` join kept colour 25 at the miss call n81 the same way;
// `gci-i32ec`: `gone(wc)`'s `wc.get()` result kept across `spin`'s poll).
// HotSpot's oop map at those calls does not hold the dead result.
//
// The rule: a φ or a pure op ([`keep_set_step_is_exitless`]) at a SPLICED pc
// reads no snapshot by pc. Why that is exact, by reading:
// * its lowering publishes nothing that reads a frame state -- `Op::Phi`
//   emits no byte (the edge copies at the predecessors' ends are value reads
//   of the φ's inputs, which the value ranges cover), and a pure op is a
//   deterministic computation with no control edge, so no deopt, rethrow pad,
//   call, poll or mode exit;
// * a chain snapshot (every snapshot at a combined-buffer pc) is read only
//   through an explicit resolution at the raw pc of a node that can exit
//   (`splice_chain_snapshot_for_site`), never through a by-bci table point --
//   every by-bci consumer, `build_deopt_points` included, skips chain
//   snapshots -- so the by-bci anchor the step places at that pc publishes
//   nothing; and the flat fallback at the own invoke is a trapping node's;
// * `carry` is unchanged: a later pc-less node that can exit still reads the
//   φ's pc, exactly as its lowering resolves its frame through `cur_bci`,
//   which the φ's lowering set; and the read set (`read_pcs`) is unchanged,
//   so the bytecode-reachability union credits exactly the snapshots it
//   credited before.
// A step at one of this method's own pcs keeps the union (its pc may anchor a
// by-bci deopt point), as does a step with a per-copy frame (that read stays).
//
// JIT round 21 wave 1 (lane keepset): the second bullet missed one reader. A
// spliced loop header's chain snapshot is also read by pc, at the LATCH, by
// the spliced back-edge mode exit (`spliced_header_chain_snapshot`); its φ
// steps were the only steps at that pc, so once they went silent nothing
// kept what only that snapshot names, and the poll's slow path zeroed it
// right before the exit read it. The latch's poll step now reads it
// (`ir_spliced_latch_exit_reads_enabled`, default ON).
//
// `CRATONVM_JIT_IR_KEEP_SET_SPLICED_EXITLESS_STEPS=0` restores the per-pc
// union at every step byte for byte. Read once per keep-set build, no static
// (`process_global_statics_ratchet`). Default ON as a retention fix.

/// `CRATONVM_JIT_IR_KEEP_SET_SPLICED_EXITLESS_STEPS` -- default on, `=0` off.
/// See the section above.
pub(in crate::ir_lower) fn ir_keep_set_spliced_exitless_steps_enabled() -> bool {
    true
}

// ARGWIN21-8 (same page): the home of an armed site's own `Ref` result,
// while the site runs. Colours are shared by values whose ranges do not
// overlap, so the result's colour may hold an EARLIER occupant -- in
// `R21OomArgWindow temp` the 55 % temporary passed to `drop` and the array
// `drop` allocates share one -- and the result's range starts AT the site, so
// the dead-home rule saw the colour as live there and neither cleared it nor
// left it out of the site's map: the allocation's own collection rooted the
// dead temporary through the home the result had not written yet.
//
// Under this switch the site's own result colour is dead at the site (cleared
// with its planned clears, left out of its liveness claim) when the site's
// OWN step reads no snapshot naming that colour. Why that is exact: the
// result overwrites the home right after the GC point, so every reader after
// the site reads the result (a later reader of the earlier occupant would
// already read the wrong value -- the colouring forbids it); the earlier
// occupant's compiled-code readers are before the site (its range ended
// there); and the only readers left are the site's own exits -- its deopt,
// rethrow pad or helper exit, which read exactly the snapshots its step reads
// (by pc, by resolved pc, per-copy). Inputs never share the result's colour
// (their ranges reach the site).
//
// Precise keep set only; read once per keep-set build, no static. `=0`
// restores the old answer byte for byte.

/// `CRATONVM_JIT_IR_SITE_RESULT_HOME_CLEAR` -- default on, `=0` off. See the
/// section above.
pub(in crate::ir_lower) fn ir_site_result_home_clear_enabled() -> bool {
    true
}

// ARGWIN21-7 (page `jr21-compiled-caller-roots-its-outgoing-arguments-for-the-whole-callee-FIXED-20261026.md`):
// a step at a SPLICED pc also reads, by pc, every snapshot at its enclosing
// own `invoke` (`keep_set_step_pc_reads`'s `named[own]`). One of them is the
// invoke's own snapshot, taken at the top of the bytecode, BEFORE the invoke
// pops its arguments -- so its operand stack names the call's arguments, and
// every GC point inside the spliced callee kept them, even after the callee's
// last use: `drop(byte[] p, int n) { int k = p.length; byte[] q = new byte[n];
// ... }` spliced into `drop(new byte[big], big)` kept the big array through
// the second allocation (`R21OomArgWindow temp`, an `OutOfMemoryError`
// HotSpot 25 does not throw; HotSpot's inlined JVMState at the callee's
// allocation has the arguments popped).
//
// That operand stack has one reader from inside the splice: a deopt that
// resumes FLAT at the invoke (re-executing it), which `splice_chain_for_site`
// chooses only when no chain snapshot describes the site. A reason-9 rethrow
// pad publishes an empty operand stack (`plan_rethrow_pad`), the propagating
// exit reads only the monitors, a spliced loop's mode exit leaves through a
// chain or refuses, and a chain deopt reads the chain snapshot at the raw pc
// -- whose caller scopes carry their operand stacks with the arguments
// already popped (`ir::SpliceScopeChain`) and which this step reads too
// (`named[pc]`). So under this switch a step at a spliced pc that some chain
// snapshot describes reads its invoke's snapshots' LOCALS and MONITORS only,
// and the pc is recorded (`DeadRefSlotClear::chain_only_pcs`); a deopt
// emitted at such a pc that still resolves flat latches a bailout, and the
// ladder lowers once more with the keep-everything attempt, which never uses
// the rule (`*image_dropped`).
//
// Precise keep set only; read once per keep-set build, no static
// (`process_global_statics_ratchet`). `=0` restores the full read byte for
// byte.

/// `CRATONVM_JIT_IR_KEEP_SET_SPLICED_INVOKE_ARGS` -- default on, `=0` off.
/// See the section above.
pub(in crate::ir_lower) fn ir_keep_set_spliced_invoke_args_enabled() -> bool {
    true
}

/// gkr (`docs/feature-designs/native-dispatch-architecture.md`):
/// `CRATONVM_JIT_IR_KEEP_SET_SPLICED_INVOKE_ARGS_EVERY_PC` -- default on, `=0`
/// off (only with the switch above). ARGWIN21-7 drops the enclosing invoke's
/// operand stack from a spliced step's reads only at a pc some chain snapshot
/// describes. A call deeper in a NESTED splice (`cleared`'s `ref.get()` and
/// `System.gc()` inside `dropAndCheck` inside `caller`) has none, so every
/// such step still read `caller`'s `dropAndCheck` arguments and kept the
/// 16 MB list through the callee's collections. The stack's only reader at a
/// spliced pc is a deopt that resumes flat at the invoke, and every pc this
/// rule reads without the stack goes into `assumed`, where such a deopt latches
/// the keep-everything retry (`refuse_flat_at_chain_only_pc`). So the rule
/// holds at every spliced pc; a pc that does deopt flat costs the retry.
/// Read once per keep-set build, no static.
pub(in crate::ir_lower) fn ir_keep_set_spliced_invoke_args_every_pc_enabled() -> bool {
    true
}

/// ARGWIN21-7's inputs to [`snapshot_colours_live_after_each_call`], the pcs
/// it relied on, and ARGWIN21-8's per-site own reads.
pub(in crate::ir_lower) struct SplicedInvokeArgs {
    /// `bci` -> the colours the snapshots at `bci` name in their locals
    /// (narrowed as the keep set narrows them) and monitors. Empty when
    /// ARGWIN21-7 is off.
    pub(in crate::ir_lower) named_no_stack: HashMap<usize, Vec<u64>>,
    /// Every pc a chain snapshot (`SafepointSnapshot::is_splice_state`) sits
    /// at. Empty when ARGWIN21-7 is off, which turns the rule off.
    pub(in crate::ir_lower) chain_pcs: std::collections::HashSet<usize>,
    /// The spliced pcs whose steps read `named_no_stack`.
    pub(in crate::ir_lower) assumed: std::collections::HashSet<usize>,
    /// ARGWIN21-8 (when `Some`): per armed site, the colours its OWN step
    /// reads -- the snapshots its exits can resume or rethrow with.
    pub(in crate::ir_lower) own_reads: Option<HashMap<NodeId, Vec<u64>>>,
}

/// gci i47w/ks47: can a node with op `op` publish no exit that reads a frame
/// state? A φ (no code) and the pure value ops of [`Op::is_pure`], less
/// `ScalarIntrinsic` (its lowering may call a helper) and `Param` (never in a
/// splice). Pure, for the unit tests.
pub(in crate::ir_lower) fn keep_set_step_is_exitless(op: &Op) -> bool {
    matches!(op, Op::Phi)
        || (op.is_pure() && !matches!(op, Op::ScalarIntrinsic(_) | Op::Param(_)))
}

/// The snapshot colours a keep-set step reads BY PC, recording the pcs read
/// in `read_pcs`: `named[eff]` (unless `own_frame_only`) and, at a spliced
/// pc, `named[own]` for the enclosing own invoke -- the gen r5w6/oomjit10
/// rule -- except that under `exitless_steps` an exitless op
/// ([`keep_set_step_is_exitless`]) at a spliced pc reads neither (gci
/// i47w/ks47, see the section above). `read_pcs` gets the same pcs either
/// way. Pure, for the unit tests.
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_lower) fn keep_set_step_pc_reads(
    op: &Op,
    eff: Option<usize>,
    named: &HashMap<usize, Vec<u64>>,
    spliced_ranges: &[(usize, usize, usize)],
    own_frame_only: bool,
    exitless_steps: bool,
    words: usize,
    read_pcs: &mut std::collections::HashSet<usize>,
    own_named: Option<&HashMap<usize, Vec<u64>>>,
) -> Vec<u64> {
    let mut reads = vec![0u64; words];
    let Some(pc) = eff else {
        return reads;
    };
    read_pcs.insert(pc);
    let own = resolve_spliced_pc(spliced_ranges, pc);
    let silent = exitless_steps && own != pc && keep_set_step_is_exitless(op);
    if !silent {
        if let Some(bits) = named.get(&pc).filter(|_| !own_frame_only) {
            bits_or(&mut reads, bits);
        }
    }
    if own != pc {
        read_pcs.insert(own);
        if !silent {
            // ARGWIN21-7: `own_named` (when given) is the enclosing invoke's
            // snapshots WITHOUT their operand stacks (see the section above
            // `ir_keep_set_spliced_invoke_args_enabled`).
            if let Some(bits) = own_named.unwrap_or(named).get(&own) {
                bits_or(&mut reads, bits);
            }
        }
    }
    reads
}

// ── Value ranges for the dead-home rule (gcd d6/u) ───────────────────
//
// The rule spares every colour a live range covers at the call. Those ranges
// were the SLOT PLAN's, and the slot plan counts every input edge as a read:
// including the MEMORY-token edge. The builder reuses a call or a load as the
// memory token of the next memory node (`self.mem = call`, see
// `TypeReq::Memory`), so `cleared(ref)`'s `ref.get()` call (bci 8) was
// "read" by the `System.gc()` call at bci 14 -- as that call's memory input
// -- and its range reached exactly the call it had to be cleared at
// (orchestrator, d5 build: colour 11, home `[rbp-160]`, in neither the keep
// set nor the dead set at n26, cleared only at n30, after the collection).
// A memory phi naming the call carried it round a loop the same way.
//
// [`value_use_ranges`] recomputes the ranges of the coloured `Ref` values
// with the slot plan's own liveness, counting only VALUE reads: an input slot
// `ir::expected_input_type` calls `Control` or `Memory` is not a read (no
// lowering reads a token's slot -- a token orders, it is never loaded), and
// only value phis' edge copies read their inputs (`gather_phi_copies`' own
// filter). Every value range is a subset of the slot plan's, so the plan's
// disjointness per colour -- which `dead_ref_colours_at` relies on -- holds.
// A compiled-code reader of a home is always a value read of its node, so a
// home no value range covers at a call has no compiled-code reader after it;
// snapshots, rethrow pads and mode exits are the keep set's business, as
// before. `None` (over budget, no convergence) falls back to the plan's.
//
// `CRATONVM_JIT_IR_DEAD_HOME_VALUE_RANGES=0` restores the slot plan's ranges.
// Read per compile, no static (`process_global_statics_ratchet`).



/// Is input slot `idx` of `node` an ordering token (control or memory) rather
/// than a value the lowering reads? See the section above.
pub(in crate::ir_lower) fn input_is_token(node: &Node, idx: usize) -> bool {
    matches!(
        crate::ir::expected_input_type(&node.op, idx, node.inputs.len()),
        crate::ir::TypeReq::Control | crate::ir::TypeReq::Memory
    )
}

/// gcd d6/u: per node, the live range of each coloured `Ref` value counting
/// VALUE reads only (`None` for every other node), in [`plan_slots`]' position
/// numbering; or `None` when the analysis is over [`SLOT_PLAN_WORK_BUDGET`] or
/// does not converge. See the section above.
pub(in crate::ir_lower) fn value_use_ranges(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
) -> Option<Vec<Option<LiveRange>>> {
    value_use_ranges_with(
        graph,
        schedule,
        plan,
        SLOT_PLAN_MAX_ITERATIONS,
        crate::regalloc::legacy_liveness_fixed_point(schedule.blocks.len()),
        None,
    )
}

/// [`value_use_ranges`] through an optional per-compile
/// `regalloc::LivenessScratch` (JIT round 19 wave 10, lane lv10,
/// `CRATONVM_JIT_IR_LIVENESS_SHARED_SCRATCH`). `None` is
/// [`value_use_ranges`] exactly.
pub(in crate::ir_lower) fn value_use_ranges_scratch(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    scratch: Option<&mut crate::regalloc::LivenessScratch>,
) -> Option<Vec<Option<LiveRange>>> {
    value_use_ranges_impl(
        graph,
        schedule,
        plan,
        SLOT_PLAN_MAX_ITERATIONS,
        crate::regalloc::legacy_liveness_fixed_point(schedule.blocks.len()),
        None,
        scratch,
    )
}

/// [`value_use_ranges`] with the iteration cap and the fixed point explicit,
/// and the fixed point's raw result in `trace` when asked for (interpreter
/// round i2 wave 70, lane L2, `CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST`). The
/// round-robin arm is the legacy loop, unchanged.
pub(in crate::ir_lower) fn value_use_ranges_with(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    max_iterations: usize,
    fixed_point: crate::regalloc::LivenessFixedPoint,
    trace: Option<&mut crate::regalloc::LsLegacyRun>,
) -> Option<Vec<Option<LiveRange>>> {
    value_use_ranges_impl(
        graph,
        schedule,
        plan,
        max_iterations,
        fixed_point,
        trace,
        None,
    )
}

/// [`value_use_ranges_with`] with an optional per-compile
/// `regalloc::LivenessScratch` (JIT round 19 wave 10, lane lv10). `None` is
/// the legacy copy exactly.
pub(in crate::ir_lower) fn value_use_ranges_impl(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    max_iterations: usize,
    fixed_point: crate::regalloc::LivenessFixedPoint,
    trace: Option<&mut crate::regalloc::LsLegacyRun>,
    mut scratch: Option<&mut crate::regalloc::LivenessScratch>,
) -> Option<Vec<Option<LiveRange>>> {
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();
    let mut track: Vec<Option<usize>> = vec![None; n];
    let mut tracked = 0usize;
    for (id, node) in graph.nodes.iter().enumerate() {
        if node.ty == IrType::Ref && plan.node_color.get(id).copied().flatten().is_some() {
            track[id] = Some(tracked);
            tracked += 1;
        }
    }
    let mut out: Vec<Option<LiveRange>> = vec![None; n];
    if tracked == 0 {
        return Some(out);
    }
    if tracked.saturating_mul(nb) > SLOT_PLAN_WORK_BUDGET {
        return None;
    }
    let words = tracked.div_ceil(64).max(1);
    // `plan_slots`' numbering: one position per node and terminator, then one
    // for the outgoing edge.
    let mut span: Vec<(usize, usize)> = Vec::with_capacity(nb);
    let mut seq = 0usize;
    for block in &schedule.blocks {
        let start = seq;
        seq += block.nodes.len() + usize::from(block.terminator.is_some());
        span.push((start, seq));
        seq += 1;
    }
    let total_positions = seq;
    let mut lo = vec![usize::MAX; tracked];
    let mut hi = vec![0usize; tracked];
    // JIT round 19 wave 10 (lane lv10): pooled when a scratch is present.
    let mut def_bits = crate::regalloc::ls_zeroed(&mut scratch, words * nb);
    let mut use_bits = crate::regalloc::ls_zeroed(&mut scratch, words * nb);
    let mut phi_out_bits = crate::regalloc::ls_zeroed(&mut scratch, words * nb);
    let mut local_def = crate::regalloc::ls_zeroed(&mut scratch, words);
    for (b, block) in schedule.blocks.iter().enumerate() {
        let base = b * words;
        local_def.fill(0);
        for u in block.nodes.iter().copied().chain(block.terminator) {
            let ui = u as usize;
            let Some(node) = graph.nodes.get(ui) else {
                continue;
            };
            let upos = plan.pos_of.get(ui).copied().flatten().unwrap_or(span[b].0);
            if !matches!(node.op, Op::Phi) {
                for (k, &inp) in node.inputs.iter().enumerate() {
                    if inp == NO_NODE || input_is_token(node, k) {
                        continue;
                    }
                    let Some(Some(ti)) = track.get(inp as usize).copied() else {
                        continue;
                    };
                    lo[ti] = lo[ti].min(upos);
                    hi[ti] = hi[ti].max(upos);
                    if !bits_contains(&local_def, ti) {
                        bits_insert(&mut use_bits[base..base + words], ti);
                    }
                }
            }
            if let Some(Some(ti)) = track.get(ui).copied() {
                bits_insert(&mut local_def, ti);
                bits_insert(&mut def_bits[base..base + words], ti);
                lo[ti] = lo[ti].min(upos);
                hi[ti] = hi[ti].max(upos);
            }
        }
    }
    // Value phis' edge copies: the only phis whose inputs a lowering reads.
    // JIT round 19 wave 10 (lane lv10): one edge's reads as a closure, so the
    // walk below and the scratch's prebuilt table share it; the table lists
    // every φ of a merge, so the value-φ filter is repeated at the visit
    // (which leaves the order of the value φs unchanged).
    let mut on_edge = |pred: usize, k: usize, phis: &[NodeId], filter: bool| {
        let at = span[pred].1;
        for &pid in phis {
            if filter
                && graph.nodes.get(pid as usize).is_some_and(|p| {
                    matches!(p.ty, IrType::Memory | IrType::Control | IrType::Void)
                })
            {
                continue;
            }
            let v = match graph
                .nodes
                .get(pid as usize)
                .and_then(|p| p.inputs.get(k + 1))
            {
                Some(&v) if v != NO_NODE => v,
                _ => continue,
            };
            let Some(Some(ti)) = track.get(v as usize).copied() else {
                continue;
            };
            lo[ti] = lo[ti].min(at);
            hi[ti] = hi[ti].max(at);
            bits_insert(&mut phi_out_bits[pred * words..(pred + 1) * words], ti);
        }
    };
    match scratch.as_deref_mut() {
        Some(s) => {
            let edges = s.take_phi_edges(graph, schedule);
            for e in &edges.edges {
                on_edge(e.pred, e.k, edges.phis_of(e), true);
            }
            s.put_phi_edges(edges);
        }
        None => {
            let mut phis_of: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
            for (id, node) in graph.nodes.iter().enumerate() {
                if !matches!(node.op, Op::Phi)
                    || matches!(node.ty, IrType::Memory | IrType::Control | IrType::Void)
                {
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
                let Some(phis) = phis_of.get(&merge_ctrl) else {
                    continue;
                };
                for (k, &ctrl_in) in merge_node.inputs.iter().enumerate() {
                    let pred = match ctrl_block_of(graph, schedule, ctrl_in) {
                        Some(pred) if pred < nb => pred,
                        _ => continue,
                    };
                    on_edge(pred, k, phis, false);
                }
            }
        }
    }
    let mut live_in = crate::regalloc::ls_zeroed(&mut scratch, words * nb);
    let mut live_out = crate::regalloc::ls_zeroed(&mut scratch, words * nb);
    let mut visits = 0usize;
    let converged = match fixed_point {
        crate::regalloc::LivenessFixedPoint::RoundRobin => {
            let mut scratch = vec![0u64; words];
            let mut converged = false;
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
                    converged = true;
                    break;
                }
            }
            converged
        }
        // Interpreter round i2 wave 70, lane L2: the same answer, pass for
        // pass, visiting only dirty blocks (wave 71: the predecessor lists
        // per `fixed_point`).
        crate::regalloc::LivenessFixedPoint::Worklist
        | crate::regalloc::LivenessFixedPoint::WorklistSchedulePreds
        | crate::regalloc::LivenessFixedPoint::WorklistPostorder { .. }
        | crate::regalloc::LivenessFixedPoint::RoundRobinDriver => {
            crate::regalloc::ls_fixed_point_on_schedule(
                schedule.blocks.as_slice(),
                fixed_point,
                crate::regalloc::LsFixedPointSets {
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
            )
        }
    };
    if let Some(trace) = trace {
        *trace = crate::regalloc::LsLegacyRun {
            ran: true,
            converged,
            live_in: live_in.clone(),
            live_out: live_out.clone(),
            visits,
        };
    }
    if !converged {
        return None;
    }
    for b in 0..nb {
        let base = b * words;
        let (start, end) = span[b];
        bits_for_each(&live_in[base..base + words], |t| {
            if t < tracked {
                lo[t] = lo[t].min(start);
            }
        });
        bits_for_each(&live_out[base..base + words], |t| {
            if t < tracked {
                hi[t] = hi[t].max(end);
            }
        });
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
        crate::regalloc::ls_give(&mut scratch, buf);
    }
    let whole_method = LiveRange {
        lo: 0,
        hi: total_positions.saturating_sub(1),
    };
    for (id, slot) in track.iter().enumerate() {
        if let Some(ti) = *slot {
            out[id] = Some(if lo[ti] == usize::MAX {
                whole_method
            } else {
                LiveRange {
                    lo: lo[ti],
                    hi: hi[ti].max(lo[ti]),
                }
            });
        }
    }
    Some(out)
}

/// JIT round 20 wave 17, lane gcDead (I-9): the ops the dead-home rule may
/// arm as a site (`Lowerer::dead_ref_clear_site_for`) and therefore the ops
/// the keep-set builders give an `after_kept` entry. An entry for a node that
/// is never armed is never read.
///
/// JIT round 21, lane oom: `Op::NewCp` and `Op::MonitorEnter` too (armed under
/// `CRATONVM_JIT_IR_HELPER_SITE_DEAD_HOME_CLEAR`). Each keeps, like an
/// allocation, its own frame state (what its exceptional exit rethrows with)
/// and the plain reachability union ([`is_plain_union_site_op`]).
pub(in crate::ir_lower) fn is_dead_home_site_op(op: &Op) -> bool {
    matches!(
        op,
        Op::Call { .. }
            | Op::New { .. }
            | Op::NewArray { .. }
            | Op::NewCp { .. }
            | Op::MonitorEnter
    )
}

/// JIT round 20 wave 17 (I-9) / round 21 lane oom: the armed sites whose
/// reachability keep set takes no def kills (`DeadRefSlotClear::alloc_sites`):
/// every site that is not an `Op::Call`. The load-kill argument leans on a
/// call ordering the load; these keep the whole union instead, which can only
/// keep more.
pub(in crate::ir_lower) fn is_plain_union_site_op(op: &Op) -> bool {
    is_dead_home_site_op(op) && !matches!(op, Op::Call { .. })
}

/// gcd d4/o: may the reachability union kill a colour at its definition's
/// own bci? See the section above.
pub(in crate::ir_lower) fn def_kill_op(op: &Op) -> bool {
    matches!(
        op,
        Op::Call { .. }
            | Op::New { .. }
            | Op::NewArray { .. }
            | Op::Load(_)
            | Op::ArrayLoad(_)
            | Op::LoadStatic { .. }
    )
}

/// gcd d5/u: per block of the schedule (`nb` blocks), the `(phi, colour)`
/// pairs whose colour the keep-set liveness kills at the block's ENTRY: the
/// `Ref` phis of the merge that heads it (`block_of_merge`: merge control ->
/// block) whose every incoming edge writes the phi's home. See the section
/// above [`ir_keep_set_def_kills_enabled`]. Pure, for the unit tests.
pub(in crate::ir_lower) fn phi_entry_kills(
    graph: &Graph,
    block_of_merge: &HashMap<NodeId, usize>,
    node_color: &[Option<u32>],
    occupants: &[u32],
    nb: usize,
) -> Vec<Vec<(NodeId, usize)>> {
    let mut out: Vec<Vec<(NodeId, usize)>> = vec![Vec::new(); nb];
    for (id, node) in graph.nodes.iter().enumerate() {
        if !matches!(node.op, Op::Phi) || node.ty != IrType::Ref {
            continue;
        }
        let phi = id as NodeId;
        let Some(&merge) = node.inputs.first() else {
            continue;
        };
        let Some(&b) = block_of_merge.get(&merge) else {
            continue;
        };
        let Some(merge_node) = graph.nodes.get(merge as usize) else {
            continue;
        };
        if b >= nb || !matches!(merge_node.op, Op::Merge | Op::Region) {
            continue;
        }
        let Some(Some(colour)) = node_color.get(id).copied() else {
            continue;
        };
        let c = colour as usize;
        if occupants.get(c).copied() != Some(1) {
            continue;
        }
        // One value per incoming edge (`merge.inputs[k]` is the control of
        // value `k`, the phi's input `k + 1`), each another node in another
        // home: every edge's copy then writes this home.
        if node.inputs.len() != merge_node.inputs.len() + 1 {
            continue;
        }
        let every_edge_writes = node.inputs.iter().skip(1).all(|&v| {
            v != NO_NODE
                && v != phi
                && node_color.get(v as usize).copied().flatten() != Some(colour)
        });
        if every_edge_writes {
            out[b].push((phi, c));
        }
    }
    out
}

/// Upper bound on the bytecode visits one [`refine_reach_by_def_kills`] may
/// spend; past it the remaining colours stay kept (the union's answer).
pub(in crate::ir_lower) const DEF_KILL_VISIT_BUDGET: usize = 1 << 20;

/// gcd d4/o: drop from `kept` (the union of snapshot colours reachable from
/// `start`) every colour of `def_pc` that no snapshot names on a path from
/// `start` that avoids the colour's defining bci. See the section above.
pub(in crate::ir_lower) fn refine_reach_by_def_kills(
    kept: &mut [u64],
    def_pc: &HashMap<u32, usize>,
    verified: &cratonvm_reader::VerifiedCode,
    named: &HashMap<usize, Vec<u64>>,
    splice_named: &HashMap<usize, Vec<u64>>,
    start: usize,
) {
    let mut colours: Vec<(u32, usize)> = def_pc
        .iter()
        .filter(|&(&c, _)| bits_contains(kept, c as usize))
        .map(|(&c, &d)| (c, d))
        .collect();
    colours.sort_unstable();
    let mut spent = 0usize;
    for (colour, def) in colours {
        let names = |pc: usize| {
            named
                .get(&pc)
                .is_some_and(|b| bits_contains(b, colour as usize))
                || splice_named
                    .get(&pc)
                    .is_some_and(|b| bits_contains(b, colour as usize))
        };
        let succ = |pc: usize| verified.successors(pc);
        // Reachable (`Some(true)`), or the budget ran out (`None`): keep it.
        if colour_named_reachable_avoiding(start, def, names, succ, &mut spent) == Some(false) {
            bits_remove(kept, colour as usize);
        }
    }
}

// ── φ kills in the reachability union (gcj hmg) ──────────────────────
//
// The union a call keeps for the snapshots no scheduled node reads walks the
// method's bytecode from the call and credits every colour a snapshot names on
// the way. `refine_reach_by_def_kills` stops crediting a colour past the bci
// that redefines it, but only for the ops `def_kill_op` admits, never a φ; and
// the schedule-side liveness kills a φ's colour at its merge only there
// (`phi_entry_kills`). So a φ whose home holds a value from an EARLIER round of
// an enclosing loop stayed kept at every call of the loop that runs before the
// φ's merge: `GcfF3qPromoteKeepProbe hashmap-grow`'s OSR body keeps the `get`
// loop's φ of `m` (bci 79) at the next batch's `put` calls (bci 66, a loop
// that runs first), so the previous batch's `HashMap` (90 MB) stayed rooted
// through every major of that batch (`oldmark-root-census`: `off=816
// region=operand-spill tier=ir`).
//
// A `Ref` φ that is its colour's sole occupant, at a merge whose every
// incoming edge carries another node in another home (`phi_entry_kills`'s
// test), is rewritten by the edge copy of each edge the merge has. Which
// bytecode arrivals at the merge's bci are those edges is the subtle part: an
// OSR arm's pre-merge (`build_opaque_osr_merge`) takes the header's FORWARD
// entries and the arm, while the loop's back edge goes to the header region
// behind it and leaves the pre-merge φ's home as it is. A forward arrival
// (`from < bci`, the builder's walk order) reaches every such merge through
// one of its edges; a back edge may not. So the walk refuses forward arrivals
// at the φ's bci and lets back edges through: a call inside the φ's own loop
// still reaches the φ's readers round the back edge and keeps it. Handler
// entries and spliced merges (whose bci is a callee's) are not candidates, and
// the walk does not follow exception edges at all (`successors`).
//
// `CRATONVM_JIT_IR_KEEP_SET_PHI_REACH_KILLS=0` keeps every φ colour the union
// names, byte for byte the old rule. Needs the def kills on
// (`CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS`). Read once per keep-set build, no
// static.



/// gcj hmg: `Some((colour, merge bci))` when node `id` is a φ the
/// reachability union may kill on a forward entry to its merge bci (the
/// section above [`ir_keep_set_phi_reach_kills_enabled`]).
pub(in crate::ir_lower) fn phi_reach_kill_pc(
    graph: &Graph,
    node_color: &[Option<u32>],
    occupants: &[u32],
    spliced_ranges: &[(usize, usize, usize)],
    handlers: &[(usize, usize, usize)],
    verified: &cratonvm_reader::VerifiedCode,
    id: NodeId,
) -> Option<(u32, usize)> {
    let node = graph.nodes.get(id as usize)?;
    if !matches!(node.op, Op::Phi) || node.ty != IrType::Ref || node.frame_snapshot.is_some() {
        return None;
    }
    let colour = node_color.get(id as usize).copied().flatten()?;
    if occupants.get(colour as usize).copied() != Some(1) {
        return None;
    }
    let &merge = node.inputs.first()?;
    let merge_node = graph.nodes.get(merge as usize)?;
    if !matches!(merge_node.op, Op::Merge | Op::Region)
        || node.inputs.len() != merge_node.inputs.len() + 1
    {
        return None;
    }
    let every_edge_writes = node.inputs.iter().skip(1).all(|&v| {
        v != NO_NODE && v != id && node_color.get(v as usize).copied().flatten() != Some(colour)
    });
    if !every_edge_writes {
        return None;
    }
    let pc = node.bytecode_pc?;
    if spliced_ranges
        .iter()
        .any(|&(start, end, _)| pc >= start && pc < end)
        || handlers.iter().any(|&(_, _, h)| h == pc)
        || !verified.is_instruction_start(pc)
    {
        return None;
    }
    Some((colour, pc))
}

/// gcj hmg: drop from `kept` every colour of `phi_def_pc` that no snapshot
/// names on a path from `start` that does not enter the φ's merge bci from
/// below. See the section above [`ir_keep_set_phi_reach_kills_enabled`].
pub(in crate::ir_lower) fn refine_reach_by_phi_kills(
    kept: &mut [u64],
    phi_def_pc: &HashMap<u32, usize>,
    verified: &cratonvm_reader::VerifiedCode,
    named: &HashMap<usize, Vec<u64>>,
    splice_named: &HashMap<usize, Vec<u64>>,
    start: usize,
) {
    let mut colours: Vec<(u32, usize)> = phi_def_pc
        .iter()
        .filter(|&(&c, _)| bits_contains(kept, c as usize))
        .map(|(&c, &d)| (c, d))
        .collect();
    colours.sort_unstable();
    let mut spent = 0usize;
    for (colour, merge) in colours {
        let names = |pc: usize| {
            named
                .get(&pc)
                .is_some_and(|b| bits_contains(b, colour as usize))
                || splice_named
                    .get(&pc)
                    .is_some_and(|b| bits_contains(b, colour as usize))
        };
        let succ = |pc: usize| verified.successors(pc);
        if colour_named_reachable_past_forward_entry(start, merge, names, succ, &mut spent)
            == Some(false)
        {
            bits_remove(kept, colour as usize);
        }
    }
}

/// gcj hmg: is a pc whose snapshots name the colour (`names`) reachable from
/// `start` without a FORWARD arrival at `merge` (an edge `from -> merge` with
/// `from < merge`)? A back edge into `merge` is followed. `start` itself is
/// always visited. `None` when the shared visit budget (`spent`,
/// [`DEF_KILL_VISIT_BUDGET`]) runs out. Pure, for the unit tests.
pub(in crate::ir_lower) fn colour_named_reachable_past_forward_entry(
    start: usize,
    merge: usize,
    names: impl Fn(usize) -> bool,
    successors: impl Fn(usize) -> Vec<usize>,
    spent: &mut usize,
) -> Option<bool> {
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut work = vec![start];
    seen.insert(start);
    while let Some(pc) = work.pop() {
        *spent += 1;
        if *spent > DEF_KILL_VISIT_BUDGET {
            return None;
        }
        if names(pc) {
            return Some(true);
        }
        for s in successors(pc) {
            if s == merge && pc < merge {
                continue;
            }
            if seen.insert(s) {
                work.push(s);
            }
        }
    }
    Some(false)
}

/// gcd d4/o: is a pc whose snapshots name the colour (`names`) reachable
/// from `start` without passing `avoid`? `start` itself is always visited
/// (even when it is `avoid`); `avoid` is never entered again. `None` when the
/// shared visit budget (`spent`, [`DEF_KILL_VISIT_BUDGET`]) runs out. Pure,
/// for the unit tests.
pub(in crate::ir_lower) fn colour_named_reachable_avoiding(
    start: usize,
    avoid: usize,
    names: impl Fn(usize) -> bool,
    successors: impl Fn(usize) -> Vec<usize>,
    spent: &mut usize,
) -> Option<bool> {
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut work = vec![start];
    seen.insert(start);
    while let Some(pc) = work.pop() {
        *spent += 1;
        if *spent > DEF_KILL_VISIT_BUDGET {
            return None;
        }
        if names(pc) {
            return Some(true);
        }
        for s in successors(pc) {
            if s != avoid && seen.insert(s) {
                work.push(s);
            }
        }
    }
    Some(false)
}

/// One step of a block, in emission order, as [`stale_sets_at_sites`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum StaleEvent {
    /// An armed call site: `dead` are the colours dead there. Everything
    /// stale in `dead`, and every stale staging word, is zeroed ahead of its
    /// GC point. `cleans` is false for a site whose call may take an inline
    /// prefix ([`Lowerer::call_site_may_take_inline_prefix`]): the zeroing
    /// runs on the prefix's call path only, so after the site nothing is
    /// known to be clean.
    Site {
        node: NodeId,
        dead: Vec<u64>,
        cleans: bool,
    },
    /// A call stages a reference argument in staging word `i` (after its
    /// own site event, if it has one).
    Staged(usize),
    /// A value of clearable colour `c` is written to its home.
    Def(usize),
}



/// TLGET21-3 close: take off each armed site the reference homes it zeroes
/// only because they went stale BEFORE the loop it sits in, and answer them
/// per loop entry block, to be zeroed once at that block's end
/// (`Lowerer::loop_entry_dead_home_clears`).
///
/// The shape: a loop-entry predicate (`ir_optimize::tl_chain_predicate`'s
/// `ThreadLocal.get` chain, a null-base or array-load hoist) defines
/// references in the pre-header that are dead in the loop. The forward union
/// of [`stale_sets_at_sites`] cannot tell iterations apart, so every armed
/// site in the body (a `checkcast` of the value read, a helper `getstatic`)
/// zeroed all of them on EVERY iteration: seventeen stores per trip of
/// `TL.get()[0]` in a 16-trip loop, more than the loop's own work.
///
/// A home of colour `c` cleared at site `S` moves to the end of block `P`
/// when `S` lies in the natural loop of header `h` (`h` dominates a back
/// edge's source; the body is every block that reaches one without passing
/// `h`), `P` is that loop's ONLY entry predecessor, `P`'s only successor is
/// `h`, and no block of the loop writes colour `c` (no [`StaleEvent::Def`],
/// the header phis' edge copies included). It repeats outwards while the same
/// holds of the loop around `P`. Why it is sound:
///
/// * `c` is dead at `S`, which reaches every block of the loop and, through
///   them, everything the loop reaches: no use, and no frame state reachable
///   from `S`, names any value of colour `c` anywhere from the loop on. Every
///   path from `P`'s end enters the loop, so `c` is dead there too.
/// * Nothing in the loop writes `c`, so the word zeroed at `P`'s end is still
///   zero at every visit of `S`, which then needed no store of its own. An
///   OSR entry into the header bypasses `P`, but its stub zeroes every
///   reference home it does not seed, and the seeded ones are header phis
///   (written in the loop, so never moved).
/// * Staging words and parameter homes stay where they were (no colour).
/// * Nor does the colour of any input of a phi of the header: `P`'s own edge
///   copies into those phis run AFTER the clears and read their source homes.
///   A value the loop only carries (a local its body never touches, flowing
///   into the header phi and nowhere else) is dead at every site in the body
///   and written nowhere in it, so it qualified -- and was zeroed before the
///   copy read it: the phi turned null, and a collection let go of the weak
///   key it held (`R21Tl3Rows --compatible`, `WeakHashMap.get(strong)`).
pub(in crate::ir_lower) fn move_loop_entry_dead_home_clears(
    graph: &Graph,
    schedule: &Schedule,
    node_color: &[Option<u32>],
    blocks: &[StaleBlock],
    sites: &mut HashMap<NodeId, Vec<i32>>,
    colour_of_off: &HashMap<i32, usize>,
) -> HashMap<usize, Vec<i32>> {
    struct NaturalLoop {
        body: std::collections::HashSet<usize>,
        entry: usize,
        defs: std::collections::HashSet<usize>,
        /// The colours of every input of every phi of the header.
        phi_inputs: std::collections::HashSet<usize>,
    }
    // Header control node -> the colours its phis read on any edge.
    let mut phi_inputs_of: HashMap<NodeId, std::collections::HashSet<usize>> = HashMap::new();
    for node in graph.nodes.iter() {
        if node.op != Op::Phi {
            continue;
        }
        let Some(&anchor) = node.inputs.first() else {
            continue;
        };
        let cols = phi_inputs_of.entry(anchor).or_default();
        for &v in node.inputs.iter().skip(1) {
            if let Some(Some(c)) = node_color.get(v as usize).copied() {
                cols.insert(c as usize);
            }
        }
    }
    let nb = schedule.blocks.len();
    let mut loops: Vec<NaturalLoop> = Vec::new();
    for h in 0..nb {
        let preds = &schedule.blocks[h].predecessors;
        let back: Vec<usize> = preds
            .iter()
            .copied()
            .filter(|&p| p < nb && schedule.dom.dominates(h, p))
            .collect();
        if back.is_empty() {
            continue;
        }
        let mut body: std::collections::HashSet<usize> = std::collections::HashSet::from([h]);
        let mut work = back;
        while let Some(b) = work.pop() {
            if body.insert(b) {
                work.extend(
                    schedule.blocks[b]
                        .predecessors
                        .iter()
                        .copied()
                        .filter(|&q| q < nb),
                );
            }
        }
        let entries: Vec<usize> = preds
            .iter()
            .copied()
            .filter(|p| !body.contains(p))
            .collect();
        let [entry] = entries[..] else {
            continue;
        };
        if entry >= nb || schedule.blocks[entry].successors.as_slice() != [h] {
            continue;
        }
        let defs = body
            .iter()
            .filter_map(|&b| blocks.get(b))
            .flat_map(|blk| blk.events.iter())
            .filter_map(|e| match e {
                StaleEvent::Def(c) => Some(*c),
                _ => None,
            })
            .collect();
        let phi_inputs = phi_inputs_of
            .get(&schedule.blocks[h].ctrl)
            .cloned()
            .unwrap_or_default();
        loops.push(NaturalLoop {
            body,
            entry,
            defs,
            phi_inputs,
        });
    }
    let mut out: HashMap<usize, Vec<i32>> = HashMap::new();
    if loops.is_empty() {
        return out;
    }
    // The innermost loop holding block `b`.
    let innermost = |b: usize| {
        loops
            .iter()
            .filter(|l| l.body.contains(&b))
            .min_by_key(|l| l.body.len())
    };
    for (&nid, offs) in sites.iter_mut() {
        let Some(&b) = schedule.node_to_block.get(nid as usize) else {
            continue;
        };
        if b >= nb {
            continue;
        }
        let mut keep: Vec<i32> = Vec::with_capacity(offs.len());
        for &off in offs.iter() {
            let Some(&c) = colour_of_off.get(&off) else {
                keep.push(off);
                continue;
            };
            let mut at = b;
            let mut dest = None;
            while let Some(l) = innermost(at) {
                if l.defs.contains(&c) || l.phi_inputs.contains(&c) || l.entry == at {
                    break;
                }
                dest = Some(l.entry);
                at = l.entry;
            }
            match dest {
                Some(p) => out.entry(p).or_default().push(off),
                None => keep.push(off),
            }
        }
        *offs = keep;
    }
    sites.retain(|_, offs| !offs.is_empty());
    for offs in out.values_mut() {
        offs.sort_unstable();
        offs.dedup();
    }
    out
}

/// A block of [`stale_sets_at_sites`]'s graph.
#[derive(Clone, Debug)]
pub(in crate::ir_lower) struct StaleBlock {
    pub(in crate::ir_lower) events: Vec<StaleEvent>,
    pub(in crate::ir_lower) predecessors: Vec<usize>,
}

/// gen r5w5/oomjit9: the forward "may hold a value not zeroed since" union
/// over the block graph, and, for every [`StaleEvent::Site`], `(node, the
/// dead colours stale there, the staging words stale there)` -- exactly what
/// that site must zero. `None` when the union does not converge within
/// [`SLOT_PLAN_MAX_ITERATIONS`] passes (the caller then zeroes nothing).
///
/// A block with no predecessor other than `entry` starts with every colour of
/// `all_colours` and every staging word stale: nothing here knows what
/// reached it. `entry` starts clean (the prologue zeroes the homes). Pure,
/// for the unit tests.
pub(in crate::ir_lower) fn stale_sets_at_sites(
    blocks: &[StaleBlock],
    entry: usize,
    all_colours: &[u64],
    stage_words: usize,
) -> Option<Vec<(NodeId, Vec<u64>, Vec<u64>)>> {
    stale_sets_at_sites_with(
        blocks,
        entry,
        all_colours,
        stage_words,
        SLOT_PLAN_MAX_ITERATIONS,
        ir_dead_ref_liveness_worklist_enabled(),
        &mut 0usize,
    )
}

/// `CRATONVM_JIT_IR_DEAD_REF_LIVENESS_WORKLIST=1` (opt-in; JIT round 19
/// proposal wave 10, lane lv10;
/// `i70-L2-proposal-closure-transfer-dirty-driver-for-the-dead-ref-planner`):
/// the three whole-pass fixed points of the dead-ref clear planner
/// ([`stale_sets_at_sites`], the reach union of
/// [`snapshot_colours_after_each_call`], [`live_colours_after_calls`]) visit
/// only dirty blocks (`regalloc::ls_closure_fixed_point_forward` /
/// `_backward`), in the same pass order, and recompute each block into
/// reused buffers compared in place instead of fresh `Vec`s per block per
/// pass.
///
/// A pure refactor: each transfer depends only on the block's own state and
/// its predecessors' (forward) or successors' (backward) published sets, so
/// the state after every pass, the pass count and the convergence verdict
/// are the whole-pass loop's, converged or cut off (`lv10_*` unit tests:
/// fixtures and random CFGs, caps 0 to 3 and the real cap). The whole-pass
/// loops stay as the switch-off arm. Read once per fixed point.
pub(in crate::ir_lower) fn ir_dead_ref_liveness_worklist_enabled() -> bool {
    false
}

/// [`stale_sets_at_sites`] with the pass cap and the fixed point explicit
/// (JIT round 19 wave 10, lane lv10): `worklist` false is the whole-pass loop,
/// unchanged; `visits` counts block visits. `None` when the union does not
/// settle within `max_iterations` passes.
pub(in crate::ir_lower) fn stale_sets_at_sites_with(
    blocks: &[StaleBlock],
    entry: usize,
    all_colours: &[u64],
    stage_words: usize,
    max_iterations: usize,
    worklist: bool,
    visits: &mut usize,
) -> Option<Vec<(NodeId, Vec<u64>, Vec<u64>)>> {
    let nb = blocks.len();
    let words = all_colours.len();
    let block_in = |b: usize, out_c: &[Vec<u64>], out_s: &[Vec<u64>]| {
        let mut cur = vec![0u64; words];
        let mut stage = vec![0u64; stage_words];
        if blocks[b].predecessors.is_empty() && b != entry {
            cur.copy_from_slice(all_colours);
            stage.fill(!0u64);
        }
        for &p in &blocks[b].predecessors {
            if p < nb {
                bits_or(cur.as_mut_slice(), out_c[p].as_slice());
                bits_or(stage.as_mut_slice(), out_s[p].as_slice());
            }
        }
        (cur, stage)
    };
    // The transfer; `record` gets each site's sets before the site zeroes.
    let walk = |b: usize,
                cur: &mut [u64],
                stage: &mut [u64],
                record: &mut Vec<(NodeId, Vec<u64>, Vec<u64>)>,
                recording: bool| {
        for event in &blocks[b].events {
            match event {
                StaleEvent::Site { node, dead, cleans } => {
                    if recording {
                        let homes: Vec<u64> =
                            cur.iter().zip(dead.iter()).map(|(c, d)| *c & *d).collect();
                        record.push((*node, homes, stage.to_vec()));
                    }
                    if !*cleans {
                        continue;
                    }
                    for (c, d) in cur.iter_mut().zip(dead.iter()) {
                        *c &= !*d;
                    }
                    stage.fill(0);
                }
                StaleEvent::Staged(i) => bits_insert(stage, *i),
                StaleEvent::Def(c) => bits_insert(cur, *c),
            }
        }
    };
    let mut out_c: Vec<Vec<u64>> = vec![vec![0u64; words]; nb];
    let mut out_s: Vec<Vec<u64>> = vec![vec![0u64; stage_words]; nb];
    let mut unused: Vec<(NodeId, Vec<u64>, Vec<u64>)> = Vec::new();
    let mut converged = false;
    if worklist {
        // JIT round 19 wave 10 (lane lv10): the forward dirty-block driver,
        // the predecessor lists inverted into successor lists once; the
        // block's input built into reused buffers and compared in place.
        let pred_lists: Vec<Vec<usize>> = blocks.iter().map(|b| b.predecessors.clone()).collect();
        let succs = crate::regalloc::LsPredIndex::build(pred_lists.as_slice(), nb);
        let mut cur = vec![0u64; words];
        let mut stage = vec![0u64; stage_words];
        converged = crate::regalloc::ls_closure_fixed_point_forward(
            &succs,
            nb,
            max_iterations,
            visits,
            |b| {
                cur.fill(0);
                stage.fill(0);
                if blocks[b].predecessors.is_empty() && b != entry {
                    cur.copy_from_slice(all_colours);
                    stage.fill(!0u64);
                }
                for &p in &blocks[b].predecessors {
                    if p < nb {
                        bits_or(cur.as_mut_slice(), out_c[p].as_slice());
                        bits_or(stage.as_mut_slice(), out_s[p].as_slice());
                    }
                }
                walk(
                    b,
                    cur.as_mut_slice(),
                    stage.as_mut_slice(),
                    &mut unused,
                    false,
                );
                let mut changed = false;
                if cur != out_c[b] {
                    out_c[b].copy_from_slice(&cur);
                    changed = true;
                }
                if stage != out_s[b] {
                    out_s[b].copy_from_slice(&stage);
                    changed = true;
                }
                changed
            },
        );
    } else {
        for _ in 0..max_iterations {
            let mut changed = false;
            *visits += nb;
            for b in 0..nb {
                let (mut cur, mut stage) = block_in(b, out_c.as_slice(), out_s.as_slice());
                walk(
                    b,
                    cur.as_mut_slice(),
                    stage.as_mut_slice(),
                    &mut unused,
                    false,
                );
                if cur != out_c[b] {
                    out_c[b] = cur;
                    changed = true;
                }
                if stage != out_s[b] {
                    out_s[b] = stage;
                    changed = true;
                }
            }
            if !changed {
                converged = true;
                break;
            }
        }
    }
    if !converged {
        return None;
    }
    let mut at_sites = Vec::new();
    for b in 0..nb {
        let (mut cur, mut stage) = block_in(b, out_c.as_slice(), out_s.as_slice());
        walk(
            b,
            cur.as_mut_slice(),
            stage.as_mut_slice(),
            &mut at_sites,
            true,
        );
    }
    Some(at_sites)
}

/// The compile-time facts [`Lowerer::emit_dead_ref_slot_clears`] reads. Built
/// by [`plan_dead_ref_slot_clear`]; `None` on the lowerer means "clear
/// nothing", which is every compile without the opt-in.
pub(in crate::ir_lower) struct DeadRefSlotClear {
    /// `ranges[c]` = the live ranges of every value coloured `c`, sorted by
    /// `lo`. Disjoint, because `verify_slot_colouring` refused any overlap.
    pub(in crate::ir_lower) ranges: Vec<Vec<LiveRange>>,
    /// The colours that may be cleared: every colour some `Ref` value lives
    /// in, minus the phi colours. Ascending.
    pub(in crate::ir_lower) ref_colours: Vec<u32>,
    /// Bitset width, in `u64` words, of every colour set below.
    pub(in crate::ir_lower) words: usize,
    /// `named[bci]` = the colours of the values any snapshot at `bci` names
    /// (locals, operand stack and monitors).
    ///
    /// gen r5w6/oomjit10: under the precise keep set, only the snapshots no
    /// scheduled node reads (the rest are in `after_kept`, liveness-exact),
    /// with each own-instruction snapshot's locals narrowed by bytecode
    /// liveness; `splice_named` and `always_kept` likewise.
    pub(in crate::ir_lower) named: HashMap<usize, Vec<u64>>,
    /// The method's own bytecode, decoded (normal edges only).
    pub(in crate::ir_lower) verified: std::sync::Arc<cratonvm_reader::VerifiedCode>,
    /// `bci` -> the union of `named` over every bci reachable from it, plus
    /// `splice_named` of every reachable invoke and `always_kept`.
    pub(in crate::ir_lower) reach_memo: HashMap<usize, Vec<u64>>,
    /// gen r5w5/oomjit9: own invoke bci -> the colours named by every
    /// snapshot inside a splice that resolves to that invoke
    /// ([`resolve_spliced_pc`]). A splice's snapshots are keyed by
    /// combined-buffer pcs, which no walk over this method's own bytecode
    /// visits; without this a value only a LATER splice's frame state names
    /// (a caller local its deopt re-materialises) would read as dead.
    pub(in crate::ir_lower) splice_named: HashMap<usize, Vec<u64>>,
    /// gen r5w5/oomjit9: the colours named by a snapshot whose bci is neither
    /// an instruction of this method nor inside a splice (a per-copy snapshot
    /// of an unrolled body, say). Nothing places them in the program, so they
    /// are kept everywhere.
    pub(in crate::ir_lower) always_kept: Vec<u64>,
    /// gen r5w5/oomjit9: per scheduled `Op::Call`, the colours named by any
    /// snapshot a node scheduled AFTER it can read -- the rest of its block
    /// and every block reachable from it, by bci, resolved bci and per-copy
    /// snapshot. The bytecode-reachability rule argues that nothing after a
    /// call belongs to an earlier bytecode; this is the schedule's own answer
    /// to the same question, and the keep set is the union of both, so a node
    /// the scheduler sank past the call with an earlier bci keeps what it
    /// reads. A call with no entry claims nothing.
    ///
    /// gen r5w6/oomjit10: under the precise keep set, the LIVE colours only
    /// ([`snapshot_colours_live_after_each_call`]).
    pub(in crate::ir_lower) after_kept: HashMap<NodeId, Vec<u64>>,
    /// gcd d4/o: colour -> the own bci at which its sole occupant is
    /// defined, for the colours whose definition cannot move away from that
    /// bci ([`def_kill_op`], no per-copy frame). The reachability union stops
    /// crediting such a colour past that bci ([`refine_reach_by_def_kills`]).
    /// Empty off the precise keep set or under
    /// `CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS=0`.
    pub(in crate::ir_lower) def_pc: HashMap<u32, usize>,
    /// gcj hmg: colour -> the merge bci at which its sole-occupant `Ref` φ is
    /// rewritten on every FORWARD arrival ([`phi_reach_kill_pc`]). The
    /// reachability union stops crediting such a colour past a forward entry
    /// to that bci ([`refine_reach_by_phi_kills`]). Empty off the precise keep
    /// set, under `CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS=0` or under
    /// `CRATONVM_JIT_IR_KEEP_SET_PHI_REACH_KILLS=0`.
    pub(in crate::ir_lower) phi_def_pc: HashMap<u32, usize>,
    /// JIT round 20 wave 17, lane gcDead (I-9): the allocation nodes. At such
    /// a site the reachability union is taken WITHOUT `def_pc`'s kills
    /// (`plain_reach_memo`): a kill of a load's colour is argued from "a
    /// call anywhere between the site and D orders the load after that
    /// call", and an allocation is no memory token (the builder does not
    /// thread `mem` through it), so a load at a later bci may be scheduled
    /// above it and not re-run on the way to a snapshot that names it.
    pub(in crate::ir_lower) alloc_sites: std::collections::HashSet<NodeId>,
    /// I-9: `reach_memo` without the def kills, for `alloc_sites`.
    pub(in crate::ir_lower) plain_reach_memo: HashMap<usize, Vec<u64>>,
    /// ARGWIN21-7: the spliced pcs whose steps read their enclosing invoke's
    /// snapshots without the operand stack, on the promise that a deopt there
    /// resumes through a chain snapshot (the section above
    /// [`ir_keep_set_spliced_invoke_args_enabled`]). Empty off the rule.
    pub(in crate::ir_lower) chain_only_pcs: std::collections::HashSet<usize>,
    /// ARGWIN21-8: per armed site with a `Ref` result whose colour has other
    /// occupants, `(that colour, the colours the site's own step reads)`
    /// ([`ir_site_result_home_clear_enabled`]). Empty off the rule.
    pub(in crate::ir_lower) site_result: HashMap<NodeId, (u32, Vec<u64>)>,
}

/// gen r5w5/oomjit9: the pc of this method's own code that `pc` stands for --
/// `pc` itself outside every splice, else the enclosing invoke's, followed
/// outward through nested splices. Bounded by the range count, so a malformed
/// (cyclic) table stops at a combined-buffer pc, which no caller treats as an
/// instruction of the method (they then claim nothing).
pub(in crate::ir_lower) fn resolve_spliced_pc(spliced_ranges: &[(usize, usize, usize)], pc: usize) -> usize {
    let mut pc = pc;
    for _ in 0..=spliced_ranges.len() {
        match spliced_ranges
            .iter()
            .find(|&&(start, end, _)| pc >= start && pc < end)
        {
            Some(&(_, _, invoke)) if invoke != pc => pc = invoke,
            _ => return pc,
        }
    }
    pc
}

/// `dst |= src`, word by word over the shorter of the two.
#[inline]
pub(in crate::ir_lower) fn bits_or(dst: &mut [u64], src: &[u64]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d |= *s;
    }
}

/// Build the [`DeadRefSlotClear`] facts for one compile, or `None` (clear
/// nothing) when the method cannot be analysed: a colour or range the plan
/// does not have, bytecode that does not decode, no clearable colour at all,
/// or a schedule whose after-call keep sets do not converge. (Scalar-replaced
/// objects no longer refuse the method: their field values' colours are kept
/// everywhere, gen r5w5/oomjit9.)
///
/// `precise` (gen r5w6/oomjit10, [`ir_precise_keep_set_enabled`]) selects the
/// liveness-exact keep set described in the section above that flag; `false`
/// is the gen r5w5 construction byte for byte.
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_lower) fn plan_dead_ref_slot_clear(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    bytecode: DeadRefClearBytecode<'_>,
    spliced_ranges: &[(usize, usize, usize)],
    precise: bool,
    liveness_scratch: Option<&mut crate::regalloc::LivenessScratch>,
    spliced_invoke_args: bool,
) -> Option<DeadRefSlotClear> {
    let code = bytecode.code;
    let verified = cratonvm_reader::verified_code(code).ok()?;
    let colours = plan.slots;
    let words = colours.div_ceil(64).max(1);
    let mut ranges: Vec<Vec<LiveRange>> = vec![Vec::new(); colours];
    let mut holds_ref = vec![false; colours];
    let mut holds_phi = vec![false; colours];
    // gen r5w6/oomjit10: how many nodes share each colour, so a definition
    // kills its colour in the keep-set liveness only when it is the colour's
    // sole occupant (a pinned value's colour always is: `verify_slot_colouring`
    // refuses a shared pinned colour).
    let mut occupants = vec![0u32; colours];
    // gcd d6/u: a `Ref` value's range counts its VALUE reads only (see the
    // section above `ir_dead_home_value_ranges_enabled`); `None` keeps the
    // slot plan's ranges, byte for byte the old rule.
    let value_ranges = {
        value_use_ranges_scratch(graph, schedule, plan, liveness_scratch)
    };
    for (id, colour) in plan.node_color.iter().enumerate() {
        let Some(colour) = *colour else {
            continue;
        };
        let c = colour as usize;
        let plan_range = plan.range.get(id).copied().flatten()?;
        let range = value_ranges
            .as_ref()
            .and_then(|v| v.get(id).copied().flatten())
            .unwrap_or(plan_range);
        if range != plan_range && false {
            eprintln!(
                "[ir-slots] value range n{id} colour {c}: [{}, {}] (slot plan [{}, {}])",
                range.lo, range.hi, plan_range.lo, plan_range.hi
            );
        }
        // gen r5w6/oomjit10: a phi no compiled code reads has no range the
        // rule must respect; its whole-method range only pins its colour.
        let code_unused_phi = precise && plan.phi_code_unused.get(id).copied().unwrap_or(false);
        if !code_unused_phi {
            ranges.get_mut(c)?.push(range);
        }
        *occupants.get_mut(c)? += 1;
        let node = graph.nodes.get(id)?;
        if node.ty == IrType::Ref {
            holds_ref[c] = true;
        }
        if matches!(node.op, Op::Phi) {
            holds_phi[c] = true;
        }
    }
    for list in &mut ranges {
        list.sort_unstable_by_key(|r| r.lo);
    }
    // gen r5w6/oomjit10: phi colours are clearable under `precise` (item 2 of
    // `gengc-r5w5-oomjit9-ir-dead-homes-residuals-CLOSED-20261012.md`): a phi's home
    // is written only by the edge copies (`plan_dead_home_clears` records a
    // `Def` at the end of each predecessor), read by compiled code only
    // inside its range, and read by a frame state only where the keep set
    // says; so a phi dead at a call holds nothing anyone reads before the
    // next edge copy writes it.
    let ref_colours: Vec<u32> = (0..colours)
        .filter(|&c| holds_ref[c] && (precise || !holds_phi[c]))
        .filter_map(|c| u32::try_from(c).ok())
        .collect();
    if ref_colours.is_empty() {
        return None;
    }
    // gen r5w6/oomjit10: bytecode local liveness, handler edges included,
    // for narrowing the snapshots' locals (item 3 of the same page). `None`
    // off `precise`, or when the code length does not describe the buffer.
    let local_live = precise.then(|| {
        crate::regalloc::live_locals_per_pc_all(
            code,
            code.len(),
            bytecode.num_params,
            &[],
            bytecode.handlers,
            bytecode.num_locals,
        )
    });
    // t1 ARGFIX item 2 (`CRATONVM_JIT_IR_KEEP_SET_SPLICE_LOCAL_LIVENESS`,
    // opt-in): each spliced method's own liveness, for a chain snapshot's
    // scope locals ([`splice_snapshot_dead_slots`]). `None` off the rule.
    let splice_liveness: Option<Vec<(&str, (Vec<u64>, Vec<bool>, usize), usize)>> = (precise
        && !bytecode.spliced_bodies.is_empty()
        && ir_keep_set_splice_local_liveness_enabled())
    .then(|| {
        bytecode
            .spliced_bodies
            .iter()
            .map(|b| (b.method_key.as_str(), spliced_body_liveness(b), b.num_locals))
            .collect()
    });
    let splice_bodies: HashMap<&str, (&(Vec<u64>, Vec<bool>, usize), usize)> = splice_liveness
        .iter()
        .flatten()
        .map(|(key, live, n)| (*key, (live, *n)))
        .collect();
    let splice_root = local_live.as_ref().map(|live| (live, bytecode.num_locals));
    // The colours each snapshot names (locals, operand stack and monitors),
    // by snapshot index: `named` merges them per bci, and a node's per-copy
    // snapshot (`Node::frame_snapshot`) is found by index.
    //
    // gen r5w6/oomjit10: under `precise`, a LOCAL of a snapshot at one of
    // this method's own instructions is left out when bytecode liveness calls
    // it dead there -- read on no path from that bci before it is written,
    // exception edges included. A deopt, rethrow pad or mode exit at that bci
    // then resumes the interpreter with whatever the (possibly zeroed) home
    // holds for a local the interpreter never reads. The operand stack and
    // the monitors are never narrowed, nor is a splice's or a non-instruction
    // bci's snapshot (not this method's bytecode).
    //
    // gcj hmg: an OSR arm's `Op::OsrLocal` is named by no snapshot a GC point
    // can reach (the section above `ir_keep_set_osr_locals_arm_only_enabled`).
    let osr_arm_only = true;
    let sp_bits_split: Vec<(Vec<u64>, Vec<u64>)> = graph
        .safepoints
        .iter()
        .map(|sp| {
            let mut bits = vec![0u64; words];
            let own_instruction = verified.is_instruction_start(sp.bci)
                && !spliced_ranges
                    .iter()
                    .any(|&(start, end, _)| sp.bci >= start && sp.bci < end);
            // t1 ARGFIX item 2: a chain snapshot's dead scope locals.
            let (dead_locals, dead_stack) = if splice_bodies.is_empty() {
                (Vec::new(), Vec::new())
            } else {
                splice_snapshot_dead_slots(sp, &splice_bodies, splice_root).unwrap_or_default()
            };
            for (k, &v) in sp.locals.iter().enumerate() {
                if v == NO_NODE {
                    continue;
                }
                if osr_arm_only && osr_local_named_on_its_arm_only(graph, plan, &occupants, v) {
                    continue;
                }
                if own_instruction
                    && local_live
                        .as_ref()
                        .is_some_and(|live| local_dead_at(live, bytecode.num_locals, sp.bci, k))
                {
                    continue;
                }
                if dead_locals.get(k).copied().unwrap_or(false) {
                    continue;
                }
                if let Some(Some(c)) = plan.node_color.get(v as usize).copied() {
                    bits_insert(&mut bits, c as usize);
                }
            }
            // ARGWIN21-7: the locals and monitors alone (below).
            let mut no_stack = bits.clone();
            for &v in &sp.monitors {
                if v == NO_NODE {
                    continue;
                }
                if let Some(Some(c)) = plan.node_color.get(v as usize).copied() {
                    bits_insert(&mut no_stack, c as usize);
                }
            }
            // A chain's caller-scope locals ride in `stack`; the dead ones
            // (t1 ARGFIX item 2) are skipped like scope 0's.
            let live_stack = sp
                .stack
                .iter()
                .enumerate()
                .filter(|&(i, _)| !dead_stack.get(i).copied().unwrap_or(false))
                .map(|(_, v)| v);
            for &v in live_stack.chain(sp.monitors.iter()) {
                if v == NO_NODE {
                    continue;
                }
                if let Some(Some(c)) = plan.node_color.get(v as usize).copied() {
                    bits_insert(&mut bits, c as usize);
                }
            }
            (bits, no_stack)
        })
        .collect();
    let (sp_bits, sp_bits_no_stack): (Vec<Vec<u64>>, Vec<Vec<u64>>) =
        sp_bits_split.into_iter().unzip();
    // ARGWIN21-7: the same bits without each snapshot's operand stack, by
    // bci, and the spliced pcs a chain snapshot describes -- for the steps
    // inside a splice (the section above
    // `ir_keep_set_spliced_invoke_args_enabled`). Precise keep set only.
    let chain_rule = spliced_invoke_args && !spliced_ranges.is_empty();
    let result_rule = ir_site_result_home_clear_enabled();
    let mut invoke_args = (precise && (chain_rule || result_rule)).then(|| {
        let mut named_no_stack: HashMap<usize, Vec<u64>> = HashMap::new();
        let mut chain_pcs = std::collections::HashSet::new();
        if chain_rule {
            for (sp, bits) in graph.safepoints.iter().zip(sp_bits_no_stack.iter()) {
                bits_or(
                    named_no_stack.entry(sp.bci).or_insert_with(|| vec![0u64; words]),
                    bits,
                );
            }
            chain_pcs = graph
                .safepoints
                .iter()
                .filter(|sp| sp.is_splice_state())
                .map(|sp| sp.bci)
                .collect();
        }
        SplicedInvokeArgs {
            named_no_stack,
            chain_pcs,
            assumed: std::collections::HashSet::new(),
            own_reads: result_rule.then(HashMap::new),
        }
    });
    // gen r5w6/oomjit10: under `precise` the schedule-side liveness below is
    // exact for every snapshot a scheduled node reads, so the bytecode-side
    // reachability union keeps only the snapshots NO scheduled node reads --
    // a consumer the schedule does not show (see
    // `snapshot_colours_live_after_each_call`).
    let (kill_after, snapshot_read) = if precise {
        // JIT round 21 wave 1 (lane keepset): the compiling method's
        // liveness, for the spliced latch exit's outermost-scope locals.
        let outer_local_dead = |bci: usize, slot: usize| {
            local_live
                .as_ref()
                .is_some_and(|live| local_dead_at(live, bytecode.num_locals, bci, slot))
        };
        let (after, read) = snapshot_colours_live_after_each_call(
            graph,
            schedule,
            plan,
            &sp_bits,
            spliced_ranges,
            words,
            &occupants,
            // gcd d5/u: the phi half of the kills (same switch).
            true,
            Some(&outer_local_dead as &dyn Fn(usize, usize) -> bool),
            invoke_args.as_mut(),
        )?;
        (Some(after), read)
    } else {
        (None, Vec::new())
    };
    let unread = |i: usize| !snapshot_read.get(i).copied().unwrap_or(false);
    let mut named: HashMap<usize, Vec<u64>> = HashMap::new();
    let mut splice_named: HashMap<usize, Vec<u64>> = HashMap::new();
    let mut always_kept = vec![0u64; words];
    for (i, (sp, bits)) in graph.safepoints.iter().zip(sp_bits.iter()).enumerate() {
        if !unread(i) {
            continue;
        }
        bits_or(
            named.entry(sp.bci).or_insert_with(|| vec![0u64; words]),
            bits,
        );
        let in_splice = spliced_ranges
            .iter()
            .any(|&(start, end, _)| sp.bci >= start && sp.bci < end);
        if in_splice {
            let own = resolve_spliced_pc(spliced_ranges, sp.bci);
            if verified.is_instruction_start(own) {
                bits_or(
                    splice_named.entry(own).or_insert_with(|| vec![0u64; words]),
                    bits,
                );
            } else {
                bits_or(&mut always_kept, bits);
            }
        } else if !verified.is_instruction_start(sp.bci) {
            bits_or(&mut always_kept, bits);
        }
    }
    // gen r5w5/oomjit9: a scalar-replaced object's field values are read by
    // the materialisation a deopt performs, from their homes, at whatever
    // snapshot names the object -- a read this analysis does not place. Their
    // colours are kept everywhere (they are pinned, so they share with
    // nothing). Until this wave such a method was refused outright, which
    // left every other home of the body uncleared too.
    for &v in &plan.sr_field_values {
        if let Some(Some(c)) = plan.node_color.get(v as usize).copied() {
            bits_insert(&mut always_kept, c as usize);
        }
    }
    let after_kept = match kill_after {
        Some(after) => after,
        None => snapshot_colours_after_each_call(
            graph,
            schedule,
            &named,
            &sp_bits,
            spliced_ranges,
            words,
        )?,
    };
    // gcd d4/o: the own bci at which each killable colour is (re)defined, for
    // the bytecode-reachability union's kills (`refine_reach_by_def_kills`).
    let mut def_pc: HashMap<u32, usize> = HashMap::new();
    if precise {
        // gcd d5/u: `CRATONVM_DBG_IR_SLOTS=1` says, per coloured `Ref` value,
        // whether the union may kill its colour at its definition and why not.
        let dbg = false;
        for (id, node) in graph.nodes.iter().enumerate() {
            let why_not = if node.ty != IrType::Ref {
                continue;
            } else if node.frame_snapshot.is_some() {
                "per-copy frame"
            } else if !def_kill_op(&node.op) {
                "op"
            } else if plan.pos_of.get(id).copied().flatten().is_none() {
                "unscheduled"
            } else {
                ""
            };
            let Some(Some(colour)) = plan.node_color.get(id).copied() else {
                continue;
            };
            let why_not = if !why_not.is_empty() {
                why_not
            } else if occupants.get(colour as usize).copied() != Some(1) {
                "shared colour"
            } else {
                match node.bytecode_pc {
                    None => "no pc",
                    Some(pc) => {
                        let own = resolve_spliced_pc(spliced_ranges, pc);
                        if verified.is_instruction_start(own) {
                            def_pc.insert(colour, own);
                            ""
                        } else {
                            "pc not an instruction"
                        }
                    }
                }
            };
            if dbg {
                match def_pc.get(&colour) {
                    Some(own) if why_not.is_empty() => eprintln!(
                        "[ir-slots] def-kill n{id} {:?} colour {colour} at bci {own}",
                        node.op
                    ),
                    _ => eprintln!(
                        "[ir-slots] def-kill n{id} {:?} colour {colour} pc={:?}: no ({why_not})",
                        node.op, node.bytecode_pc
                    ),
                }
            }
        }
    }
    // gcj hmg: the merge bci at which each killable φ colour is rewritten on
    // every forward arrival (the section above
    // `ir_keep_set_phi_reach_kills_enabled`).
    let mut phi_def_pc: HashMap<u32, usize> = HashMap::new();
    if precise {
        let dbg = false;
        for id in 0..graph.nodes.len() {
            let Some((colour, pc)) = phi_reach_kill_pc(
                graph,
                &plan.node_color,
                &occupants,
                spliced_ranges,
                bytecode.handlers,
                &verified,
                id as NodeId,
            ) else {
                continue;
            };
            phi_def_pc.insert(colour, pc);
            if dbg {
                eprintln!(
                    "[ir-slots] phi reach kill n{id} colour {colour} at forward entry to bci {pc}"
                );
            }
        }
    }
    Some(DeadRefSlotClear {
        ranges,
        ref_colours,
        words,
        named,
        verified,
        reach_memo: HashMap::new(),
        splice_named,
        always_kept,
        after_kept,
        def_pc,
        phi_def_pc,
        alloc_sites: graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| is_plain_union_site_op(&n.op))
            .filter_map(|(id, _)| NodeId::try_from(id).ok())
            .collect(),
        plain_reach_memo: HashMap::new(),
        site_result: invoke_args
            .as_mut()
            .and_then(|ia| ia.own_reads.take())
            .map(|own| {
                own.into_iter()
                    .filter_map(|(site, reads)| {
                        let node = graph.nodes.get(site as usize)?;
                        let c = plan.node_color.get(site as usize).copied().flatten()?;
                        (node.ty == IrType::Ref
                            && occupants.get(c as usize).is_some_and(|&n| n > 1))
                        .then_some((site, (c, reads)))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        chain_only_pcs: invoke_args.map(|ia| ia.assumed).unwrap_or_default(),
    })
}

/// gen r5w5/oomjit9: [`DeadRefSlotClear::after_kept`] for every scheduled
/// `Op::Call`, or `None` when the backward union over the block graph does
/// not converge within [`SLOT_PLAN_MAX_ITERATIONS`] passes (then nothing is
/// claimed anywhere in the body).
///
/// What a node may read at a deopt, a rethrow pad or a mode exit is the
/// snapshot at its bci (raw, and resolved out of a splice), its per-copy
/// snapshot, and -- for a node that carries no bci of its own -- the bci the
/// lowerer carries over from the node emitted before it (`cur_bci`), which at
/// a block's start is the last bci of the block laid out before it.
pub(in crate::ir_lower) fn snapshot_colours_after_each_call(
    graph: &Graph,
    schedule: &Schedule,
    named: &HashMap<usize, Vec<u64>>,
    sp_bits: &[Vec<u64>],
    spliced_ranges: &[(usize, usize, usize)],
    words: usize,
) -> Option<HashMap<NodeId, Vec<u64>>> {
    let nb = schedule.blocks.len();
    // gci i1/fr: the armed sites' rule (`dead_ref_clear_arms`), per compile.
    let allocs = true;
    // gci i4/ir: and the pinned non-call GC points, opt-in.
    let others = true;
    let pc_bits = |pc: usize, out: &mut [u64]| {
        if let Some(bits) = named.get(&pc) {
            bits_or(out, bits);
        }
        let own = resolve_spliced_pc(spliced_ranges, pc);
        if own != pc {
            if let Some(bits) = named.get(&own) {
                bits_or(out, bits);
            }
        }
    };
    // JIT round 17 wave 3 (lane irlower, MON17W2-5; opt-in): see
    // `ir_keep_set_per_copy_frame_enabled`.
    let per_copy_frame_only = ir_keep_set_per_copy_frame_enabled();
    let node_bits = |id: NodeId, out: &mut [u64]| {
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        if let Some(pc) = node.bytecode_pc {
            if !(per_copy_frame_only && node_names_own_frame_at_its_pc(graph, node)) {
                pc_bits(pc, &mut *out);
            }
        }
        if let Some(si) = node.frame_snapshot {
            if let Some(bits) = sp_bits.get(si as usize) {
                bits_or(out, bits);
            }
        }
    };
    // Per block, in layout order: everything its nodes and terminator may
    // read, plus the carried-over bci when its first node has none of its
    // own. `cur_bci` is written by every node and terminator that has a bci
    // and read by one that has none, across block boundaries too.
    let mut block_bits: Vec<Vec<u64>> = vec![vec![0u64; words]; nb];
    let mut carry: Option<usize> = None;
    for (b, block) in schedule.blocks.iter().enumerate() {
        let first_lacks_pc = block
            .nodes
            .iter()
            .copied()
            .chain(block.terminator)
            .next()
            .and_then(|id| graph.nodes.get(id as usize))
            .is_some_and(|n| n.bytecode_pc.is_none());
        if first_lacks_pc {
            if let Some(pc) = carry {
                pc_bits(pc, block_bits[b].as_mut_slice());
            }
        }
        for id in block.nodes.iter().copied().chain(block.terminator) {
            node_bits(id, block_bits[b].as_mut_slice());
            if let Some(pc) = graph.nodes.get(id as usize).and_then(|n| n.bytecode_pc) {
                carry = Some(pc);
            }
        }
    }
    // reach[b] = block_bits[b] | reach[s] for every successor s.
    let mut reach: Vec<Vec<u64>> = block_bits;
    let converged = snapshot_reach_fixed_point(
        schedule.blocks.as_slice(),
        &mut reach,
        SLOT_PLAN_MAX_ITERATIONS,
        ir_dead_ref_liveness_worklist_enabled(),
        &mut 0usize,
    );
    if !converged {
        return None;
    }
    // gci i5/ir: the back-edge polls, opt-in (see `ir_poll_gc_point_clears_enabled`).
    let polls = ir_poll_gc_point_clears_enabled();
    let mut after: HashMap<NodeId, Vec<u64>> = HashMap::new();
    for (b, block) in schedule.blocks.iter().enumerate() {
        // The successors' share, then the block's own suffix, walked
        // backwards so each call sees itself and everything after it.
        let mut acc = vec![0u64; words];
        for &s in &block.successors {
            if s < nb {
                bits_or(acc.as_mut_slice(), reach[s].as_slice());
            }
        }
        if let Some(t) = block.terminator {
            node_bits(t, acc.as_mut_slice());
        }
        // gci i5/ir: a back-edge poll runs before the terminator's compare
        // (or after the last node of a `goto` block): it keeps what the
        // terminator and the successors read, plus the header's snapshots
        // its mode exit may name.
        if polls {
            if let (Some((_, header_pc)), Some(key)) =
                (back_edge_poll_header(graph, schedule, b), poll_site_key(graph, b))
            {
                let mut at_poll = acc.clone();
                pc_bits(header_pc, at_poll.as_mut_slice());
                after.insert(key, at_poll);
            }
        }
        for &id in block.nodes.iter().rev() {
            node_bits(id, acc.as_mut_slice());
            // JIT round 20 wave 17, lane gcDead (I-9): an allocation is a
            // site too (`Lowerer::dead_ref_clear_site_for`).
            let is_call = graph
                .nodes
                .get(id as usize)
                .is_some_and(|n| {
                    is_dead_home_site_op(&n.op) || dead_ref_clear_arms_node(graph, n, allocs, others)
                });
            if is_call {
                after.insert(id, acc.clone());
            }
        }
    }
    Some(after)
}

/// The backward reach union of [`snapshot_colours_after_each_call`]:
/// `reach[b] |= reach[s]` for every in-range successor `s != b`, until a pass
/// changes nothing (`true`) or `max_iterations` passes ran (`false`). Split
/// out in JIT round 19 wave 10 (lane lv10, `worklist` =
/// [`ir_dead_ref_liveness_worklist_enabled`]): `false` is the whole-pass loop,
/// unchanged; `true` visits only dirty blocks
/// (`regalloc::ls_closure_fixed_point_backward`) and merges into one reused
/// buffer compared in place. `succ` is any successor list source (the
/// schedule's blocks, or a bare list in the tests).
pub(in crate::ir_lower) fn snapshot_reach_fixed_point<S: crate::regalloc::LsSuccessors + ?Sized>(
    succ: &S,
    reach: &mut [Vec<u64>],
    max_iterations: usize,
    worklist: bool,
    visits: &mut usize,
) -> bool {
    let nb = reach.len();
    if worklist {
        let preds = crate::regalloc::LsPredIndex::build(succ, nb);
        let mut acc: Vec<u64> = Vec::new();
        return crate::regalloc::ls_closure_fixed_point_backward(
            &preds,
            nb,
            max_iterations,
            visits,
            |b| {
                acc.clear();
                acc.extend_from_slice(&reach[b]);
                for &s in succ.ls_successors(b) {
                    if s < nb && s != b {
                        bits_or(acc.as_mut_slice(), reach[s].as_slice());
                    }
                }
                if acc != reach[b] {
                    reach[b].copy_from_slice(&acc);
                    true
                } else {
                    false
                }
            },
        );
    }
    for _ in 0..max_iterations {
        let mut changed = false;
        *visits += nb;
        for b in (0..nb).rev() {
            let mut acc = reach[b].clone();
            for &s in succ.ls_successors(b) {
                if s < nb && s != b {
                    bits_or(acc.as_mut_slice(), reach[s].as_slice());
                }
            }
            if acc != reach[b] {
                reach[b] = acc;
                changed = true;
            }
        }
        if !changed {
            return true;
        }
    }
    false
}

/// Clear bit `i` (out-of-range indices ignored, like [`bits_insert`]).
#[inline]
pub(in crate::ir_lower) fn bits_remove(bits: &mut [u64], i: usize) {
    if let Some(word) = bits.get_mut(i / 64) {
        *word &= !(1u64 << (i % 64));
    }
}

/// gen r5w6/oomjit10: the liveness-exact twin of
/// [`snapshot_colours_after_each_call`] (under [`ir_precise_keep_set_enabled`]).
///
/// Per scheduled `Op::Call`, the colours some snapshot a node scheduled at or
/// after it READS on a path that does not first REDEFINE them: a backward
/// liveness over the schedule's blocks where every node reads the snapshots
/// the union form credits it with (its bci raw and resolved out of a splice,
/// the carried `cur_bci` when it has none, its per-copy snapshot) and kills
/// the colour it defines when it is that colour's sole occupant. The union
/// form kept a colour when ANY later node's snapshot named it, which in a loop
/// is every value the loop recomputes -- `cleared(ref)`'s `ref.get()` result
/// is named by the `ifnull` at the top of the NEXT round, so it stayed a root
/// across the `System.gc()` of this one (`GenR5W2OsrDeadSlotProbe`, census
/// `region=operand-spill`).
///
/// Why the kill is exact: a colour a snapshot names is pinned, hence one
/// node's alone; a `Ref` node writes its home at its scheduled position (the
/// optimizing tier's reference homes are authoritative); so on any path from
/// the call through that node, a later reader of the snapshot reads the NEW
/// value, and the old one in the home has no reader. Phis kill nothing (their
/// homes are written by edge copies, which this keeps no position for).
///
/// gci i40/os: "pinned, hence one node's alone" does not hold for a value
/// only chain snapshots name (it shares its colour). Under
/// `CRATONVM_JIT_IR_KEEP_SET_SHARED_COLOUR_KILLS` (default ON) a `Ref` call,
/// allocation or load kills its colour whatever the occupancy
/// ([`keep_set_step_kill`]; the argument is in the section above
/// [`ir_keep_set_shared_colour_kills_enabled`]).
///
/// Also returns, per snapshot index, whether some scheduled node reads it.
/// The caller keeps the bytecode-reachability union only for the snapshots no
/// node reads: a consumer the schedule does not show (a lowering-generated
/// exit at a bci no node carries) still keeps everything it could name.
///
/// `None` when the fixed point does not converge within
/// [`SLOT_PLAN_MAX_ITERATIONS`] passes (the caller then claims nothing).
///
/// gcd d5/u: `phi_kills` also kills each [`phi_entry_kills`] colour at the
/// entry of its merge block (a step with no reads, before the block's first
/// node), so a value a loop recomputes through a phi -- a guarded splice's
/// result -- is not kept across the loop's calls by the next round's reader.
/// `false` is the gen r5w6 liveness byte for byte.
///
/// JIT round 21 wave 1 (lane keepset): a latch's poll step reads what its
/// spliced back-edge mode exit reads (the header's chain snapshot, header φs
/// by their edge inputs; [`ir_spliced_latch_exit_reads_enabled`]), and under
/// [`ir_keep_set_modelled_chain_reads_enabled`] a call's post-call chain
/// snapshot is read by a step right after the call, and those snapshots plus
/// every unread spliced-merge chain snapshot count as read (they leave the
/// bytecode-reachability union). `outer_local_dead(bci, slot)` (the compiling
/// method's bytecode liveness, `None` when unknown) narrows the latch read's
/// outermost-scope locals as [`spliced_latch_exit_reads`] describes.
///
/// ARGWIN21-7: `invoke_args`, when given, makes a step inside a splice whose
/// raw pc a chain snapshot describes read its enclosing invoke's snapshots
/// without their operand stacks, and records that pc in `assumed` (the
/// section above [`ir_keep_set_spliced_invoke_args_enabled`]).
#[allow(clippy::too_many_arguments)]
pub(in crate::ir_lower) fn snapshot_colours_live_after_each_call(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    sp_bits: &[Vec<u64>],
    spliced_ranges: &[(usize, usize, usize)],
    words: usize,
    occupants: &[u32],
    phi_kills: bool,
    outer_local_dead: Option<&dyn Fn(usize, usize) -> bool>,
    mut invoke_args: Option<&mut SplicedInvokeArgs>,
) -> Option<(HashMap<NodeId, Vec<u64>>, Vec<bool>)> {
    let nb = schedule.blocks.len();
    // gci i1/fr: the armed sites' rule (`dead_ref_clear_arms`), per compile.
    let allocs = true;
    // gci i4/ir: and the pinned non-call GC points, opt-in.
    let others = true;
    let mut named: HashMap<usize, Vec<u64>> = HashMap::new();
    for (sp, bits) in graph.safepoints.iter().zip(sp_bits.iter()) {
        bits_or(
            named.entry(sp.bci).or_insert_with(|| vec![0u64; words]),
            bits,
        );
    }
    let entry_kills: Vec<Vec<(NodeId, usize)>> = if phi_kills {
        let block_of_merge: HashMap<NodeId, usize> = schedule
            .blocks
            .iter()
            .enumerate()
            .map(|(b, blk)| (blk.ctrl, b))
            .collect();
        phi_entry_kills(graph, &block_of_merge, &plan.node_color, occupants, nb)
    } else {
        vec![Vec::new(); nb]
    };
    if phi_kills && false {
        for (b, kills) in entry_kills.iter().enumerate() {
            for &(phi, c) in kills {
                eprintln!("[ir-slots] phi-entry kill: block {b} n{phi} colour {c}");
            }
        }
    }
    let mut read_pcs: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut read_snapshot = vec![false; graph.safepoints.len()];
    let per_copy_frame_only = ir_keep_set_per_copy_frame_enabled();
    // gci i40/os: the shared-colour kills, default ON (`=0` is the
    // sole-occupant rule); `CRATONVM_DBG_IR_SLOTS=1` names each one taken on
    // a colour with more than one occupant.
    let shared_kills = true;
    let dbg_shared_kills =
        shared_kills && false;
    // gci i47w/ks47: φ and pure steps at a spliced pc read no snapshot by pc,
    // default ON (`=0` is the per-pc union at every step); under
    // `CRATONVM_DBG_IR_SLOTS=1` one line per such step whose union was not empty.
    let exitless_steps = ir_keep_set_spliced_exitless_steps_enabled();
    let spliced_args_every_pc = ir_keep_set_spliced_invoke_args_every_pc_enabled();
    let dbg_exitless =
        exitless_steps && false;
    // gci i5/ir: the back-edge polls, opt-in (see `ir_poll_gc_point_clears_enabled`).
    let polls = ir_poll_gc_point_clears_enabled();
    // JIT round 21 wave 1 (lane keepset): the spliced back-edge mode exit's
    // reads at its latch's poll step (`ir_spliced_latch_exit_reads_enabled`),
    // and the post-call exits' after their calls
    // (`ir_keep_set_modelled_chain_reads_enabled`). Read once per build.
    let latch_exit_reads = true;
    let modelled_chain_reads = latch_exit_reads;
    let dbg_latch_exit =
        latch_exit_reads && false;
    let mut chain_by_pc: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut post_call_reads_of: HashMap<NodeId, Vec<usize>> = HashMap::new();
    if latch_exit_reads {
        for (si, sp) in graph.safepoints.iter().enumerate() {
            if sp.is_guard_splice_state() {
                chain_by_pc.entry(sp.bci).or_default().push(si);
            }
            if modelled_chain_reads && sp.post_call_of != NO_NODE {
                post_call_reads_of.entry(sp.post_call_of).or_default().push(si);
            }
        }
    }
    // Per block, in layout order: one step per node and terminator.
    let mut steps: Vec<Vec<LiveStep>> = Vec::with_capacity(nb);
    let mut carry: Option<usize> = None;
    for (b, block) in schedule.blocks.iter().enumerate() {
        let mut row = Vec::with_capacity(block.nodes.len() + 2);
        // gci i5/ir: where the terminator's step went, for the poll's step.
        let mut term_step: Option<usize> = None;
        // gcd d5/u: the phi homes every incoming edge rewrote, before any
        // node of the block reads them (see `phi_entry_kills`).
        for &(phi, c) in entry_kills.get(b).map(Vec::as_slice).unwrap_or(&[]) {
            row.push(LiveStep {
                reads: vec![0u64; words],
                kill: Some(c),
                node: phi,
                is_call: false,
            });
        }
        // JIT round 20 wave 21 (lane pollroot,
        // `jr20-inner-loop-poll-roots-dead-outer-loop-values-FIXED-20261013.md`): a
        // latch's back-edge poll sits between its last node and its
        // terminator. A step there that reads and kills nothing records, under
        // the latch's head node (`block.ctrl`, never a call), the colours a
        // frame state can read after the poll -- what
        // `DeadRefSlotClear::dead_colours_at_back_edge_poll` keeps. Adds an
        // entry to the after map and changes no other answer.
        let latch = block.successors.iter().any(|&s| s <= b);
        // JIT round 21 wave 1 (lane keepset): what this latch's spliced
        // back-edge mode exit reads, the header φs by their edge inputs
        // (`spliced_latch_exit_reads`). Empty -- the round 20 step -- for
        // every other latch, and under `=0`.
        let mut exit_reads = vec![0u64; words];
        if latch && latch_exit_reads {
            if let Some((sis, values)) =
                spliced_latch_exit_reads(graph, schedule, b, &chain_by_pc, outer_local_dead)
            {
                for v in values {
                    if let Some(Some(c)) = plan.node_color.get(v as usize).copied() {
                        bits_insert(&mut exit_reads, c as usize);
                    }
                }
                if dbg_latch_exit {
                    eprintln!(
                        "[ir-slots] spliced latch exit reads: block {b} snapshots {sis:?} colours {:?}",
                        bits_list(&exit_reads)
                    );
                }
                if modelled_chain_reads {
                    for si in sis {
                        if let Some(flag) = read_snapshot.get_mut(si) {
                            *flag = true;
                        }
                    }
                }
            }
        }
        let poll_step = || LiveStep {
            reads: exit_reads.clone(),
            kill: None,
            node: block.ctrl,
            is_call: true,
        };
        for id in block.nodes.iter().copied().chain(block.terminator) {
            if latch && block.terminator == Some(id) {
                row.push(poll_step());
            }
            let Some(node) = graph.nodes.get(id as usize) else {
                continue;
            };
            let eff = node.bytecode_pc.or(carry);
            if node.bytecode_pc.is_some() {
                carry = node.bytecode_pc;
            }
            // JIT round 17 wave 3 (lane irlower, MON17W2-5; opt-in): a node
            // that names its own frame at its own pc reads that frame (below)
            // and not the union of every snapshot at the pc.
            let own_frame_only = per_copy_frame_only && node_names_own_frame_at_its_pc(graph, node);
            // ARGWIN21-7: a step at a spliced pc a chain snapshot describes
            // reads its invoke's snapshots without their operand stacks.
            // gkr (`Gcd1ArgPinProbe` warm caller): and, under
            // `ir_keep_set_spliced_invoke_args_every_pc_enabled`, a step at any
            // spliced pc (the read's one other reader, a flat deopt there,
            // latches the keep-everything retry through `assumed`).
            let chain_site = eff.filter(|&pc| {
                invoke_args
                    .as_deref()
                    .is_some_and(|ia| ia.chain_pcs.contains(&pc) || spliced_args_every_pc)
                    && resolve_spliced_pc(spliced_ranges, pc) != pc
                    && !(exitless_steps && keep_set_step_is_exitless(&node.op))
            });
            // gci i47w/ks47: a φ or pure step at a spliced pc reads no
            // snapshot by pc (see `ir_keep_set_spliced_exitless_steps_enabled`).
            let mut reads = keep_set_step_pc_reads(
                &node.op,
                eff,
                &named,
                spliced_ranges,
                own_frame_only,
                exitless_steps,
                words,
                &mut read_pcs,
                chain_site
                    .and(invoke_args.as_deref())
                    .map(|ia| &ia.named_no_stack),
            );
            if let (Some(pc), Some(ia)) = (chain_site, invoke_args.as_deref_mut()) {
                ia.assumed.insert(pc);
            }
            if dbg_exitless {
                if let Some(pc) = eff {
                    let own = resolve_spliced_pc(spliced_ranges, pc);
                    if own != pc && keep_set_step_is_exitless(&node.op) {
                        let mut skipped = vec![0u64; words];
                        for key in [pc, own] {
                            if let Some(bits) = named.get(&key) {
                                bits_or(&mut skipped, bits);
                            }
                        }
                        if skipped.iter().any(|&w| w != 0) {
                            eprintln!(
                                "[ir-slots] exitless spliced step: n{id} {:?} pc={pc} own={own} reads none (by-pc union {:?})",
                                node.op,
                                bits_list(&skipped)
                            );
                        }
                    }
                }
            }
            if let Some(si) = node.frame_snapshot {
                if let Some(bits) = sp_bits.get(si as usize) {
                    bits_or(&mut reads, bits);
                }
                if let Some(flag) = read_snapshot.get_mut(si as usize) {
                    *flag = true;
                }
            }
            // gci i40/os: a `Ref` call / allocation / load kills its colour
            // even when the colour has other occupants (chain-named values are
            // not pinned); see the section above
            // `ir_keep_set_shared_colour_kills_enabled`.
            let kill = keep_set_step_kill(
                &node.op,
                node.ty,
                plan.node_color.get(id as usize).copied().flatten(),
                occupants,
                shared_kills,
            );
            if dbg_shared_kills {
                if let Some(c) = kill {
                    let n = occupants.get(c).copied().unwrap_or(0);
                    if n > 1 {
                        eprintln!(
                            "[ir-slots] shared-colour kill: n{id} {:?} colour {c} occupants {n} pc={:?}",
                            node.op, node.bytecode_pc
                        );
                    }
                }
            }
            // JIT round 20 wave 17, lane gcDead (I-9): an allocation is a
            // site too; its own reads (the frame state at its bci, which an
            // exceptional exit of its helper resumes or rethrows with) are in
            // its entry, as a call's are.
            // gci i1/fr + i4/ir: and the sites the gci rules arm (an entry for a
            // node that is never armed is never read).
            let is_call = is_dead_home_site_op(&node.op)
                || dead_ref_clear_arms_node(graph, node, allocs, others);
            if block.terminator == Some(id) {
                term_step = Some(row.len());
            }
            if is_call {
                if let Some(own) = invoke_args.as_deref_mut().and_then(|ia| ia.own_reads.as_mut()) {
                    own.insert(id, reads.clone());
                }
            }
            row.push(LiveStep {
                reads,
                kill,
                node: id,
                is_call,
            });
            // JIT round 21 wave 1 (lane keepset): a call's post-call chain
            // snapshot is read by its post-call exit right after it returns:
            // a step after the call's own, so the call's kill of its result
            // stays ahead of the read (`ir_keep_set_modelled_chain_reads_enabled`).
            if let Some(sis) = post_call_reads_of.get(&id) {
                let mut post_reads = vec![0u64; words];
                for &si in sis {
                    if let Some(bits) = sp_bits.get(si) {
                        bits_or(&mut post_reads, bits);
                    }
                    if let Some(flag) = read_snapshot.get_mut(si) {
                        *flag = true;
                    }
                }
                row.push(LiveStep {
                    reads: post_reads,
                    kill: None,
                    node: id,
                    is_call: false,
                });
            }
        }
        if latch && block.terminator.is_none() {
            row.push(poll_step());
        }
        // gci i5/ir: the back-edge poll's step -- before the terminator's (its
        // poll runs before the compare), or last in a `goto` block. It reads
        // the header's snapshots (its mode exit's state) and kills nothing.
        if polls && (block.terminator.is_none() || term_step.is_some()) {
            if let (Some((_, header_pc)), Some(key)) =
                (back_edge_poll_header(graph, schedule, b), poll_site_key(graph, b))
            {
                let mut reads = vec![0u64; words];
                if let Some(bits) = named.get(&header_pc) {
                    bits_or(&mut reads, bits);
                }
                let own = resolve_spliced_pc(spliced_ranges, header_pc);
                if own != header_pc {
                    if let Some(bits) = named.get(&own) {
                        bits_or(&mut reads, bits);
                    }
                }
                let step = LiveStep {
                    reads,
                    kill: None,
                    node: key,
                    is_call: true,
                };
                match term_step {
                    Some(i) => row.insert(i, step),
                    None => row.push(step),
                }
            }
        }
        steps.push(row);
    }
    for (i, sp) in graph.safepoints.iter().enumerate() {
        if read_pcs.contains(&sp.bci) {
            read_snapshot[i] = true;
        }
    }
    // JIT round 21 wave 1 (lane keepset): a guard-chain snapshot at a spliced
    // `Merge` / `Region` pc that no step reads has no reader but a header
    // exit, which the latch steps above read (see
    // `ir_keep_set_modelled_chain_reads_enabled`): it leaves the union.
    if modelled_chain_reads {
        let merge_pcs: std::collections::HashSet<usize> = graph
            .nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Merge | Op::Region))
            .filter_map(|n| n.bytecode_pc)
            .filter(|&pc| resolve_spliced_pc(spliced_ranges, pc) != pc)
            .collect();
        for (i, sp) in graph.safepoints.iter().enumerate() {
            if sp.is_guard_splice_state()
                && sp.post_call_of == NO_NODE
                && merge_pcs.contains(&sp.bci)
            {
                read_snapshot[i] = true;
            }
        }
    }
    let successors: Vec<Vec<usize>> = schedule
        .blocks
        .iter()
        .map(|b| b.successors.clone())
        .collect();
    let after = live_colours_after_calls(&steps, &successors, words)?;
    Some((after, read_snapshot))
}

/// `CRATONVM_JIT_IR_KEEP_SET_PER_COPY_FRAME=1` (opt-in; JIT round 17 wave 3,
/// lane irlower; MON17W2-5, item 2 of
/// `r17-monitor2-by-bci-readers-that-stay-by-bci-FIXED-20261005.md`): in the precise
/// keep set ([`snapshot_colours_live_after_each_call`]) and its union twin
/// ([`snapshot_colours_after_each_call`]), a node that names its OWN frame at
/// its own pc ([`node_names_own_frame_at_its_pc`]) reads that frame alone,
/// not `named[pc]` -- the union of every snapshot at the pc, which after an
/// unroll or a coarsening holds every other copy's (and the original's)
/// colours, so each copy kept them all alive across its calls.
///
/// Why the narrowing is exact: every exit that node's lowering can publish
/// resolves its frame through `cur_node_frame` first
/// (`resolve_frame_state_for_site` behind `emit_deopt_unless`,
/// `push_rethrow_pad`, `push_call_exc_patch`, `site_snapshot_may_hold_monitor`;
/// round 17 wave 2, lane monitor2's table on the forked-coarsening page), and
/// the node does not anchor its bci (`Lowerer::note_bci_anchor`), so the
/// by-bci table point is never placed inside it. The by-bci snapshot is still
/// counted as READ (its pc stays in the schedule's read set), so the
/// bytecode-reachability union does not start crediting it either; it is
/// credited to the unnamed nodes at the pc, and to a pc-less node after a
/// named one (`carry`), exactly as before. Opt-in: it shrinks a GC keep set.
/// Read once per keep-set build (no `static`).
pub(in crate::ir_lower) fn ir_keep_set_per_copy_frame_enabled() -> bool {
    false
}

/// Does `node` name, through [`Node::frame_snapshot`], a snapshot recorded at
/// its OWN bytecode pc (a per-copy frame of an unrolled or coarsened body, or
/// an LICM pre-header frame), rather than none or one at another bci (a
/// spliced node's resume frame)? The predicate `Lowerer::note_bci_anchor`
/// uses to skip the by-bci anchor.
pub(in crate::ir_lower) fn node_names_own_frame_at_its_pc(graph: &Graph, node: &Node) -> bool {
    node.bytecode_pc.is_some_and(|pc| {
        node.frame_snapshot
            .and_then(|si| graph.safepoints.get(si as usize))
            .is_some_and(|sp| sp.bci == pc)
    })
}

/// One node of [`live_colours_after_calls`]' block graph: the colours it
/// reads through a frame state, the colour its definition overwrites (if the
/// kill is exact), and whether it is an `Op::Call`.
#[derive(Clone, Debug)]
pub(in crate::ir_lower) struct LiveStep {
    pub(in crate::ir_lower) reads: Vec<u64>,
    pub(in crate::ir_lower) kill: Option<usize>,
    pub(in crate::ir_lower) node: NodeId,
    pub(in crate::ir_lower) is_call: bool,
}

/// gen r5w6/oomjit10: the backward liveness of
/// [`snapshot_colours_live_after_each_call`] over an abstract block graph --
/// `steps[b]` in layout order, `successors[b]` -- returning for every call
/// step the colours live AT it: its own reads, plus everything live after it
/// minus its own kill. A node's transfer is `(live - kill) | reads` (it reads
/// its frame state before its result is written). `None` when the fixed point
/// does not converge within [`SLOT_PLAN_MAX_ITERATIONS`] passes. Pure, for the
/// unit tests.
pub(in crate::ir_lower) fn live_colours_after_calls(
    steps: &[Vec<LiveStep>],
    successors: &[Vec<usize>],
    words: usize,
) -> Option<HashMap<NodeId, Vec<u64>>> {
    live_colours_after_calls_with(
        steps,
        successors,
        words,
        SLOT_PLAN_MAX_ITERATIONS,
        ir_dead_ref_liveness_worklist_enabled(),
        &mut 0usize,
    )
}

/// [`live_colours_after_calls`] with the pass cap and the fixed point
/// explicit (JIT round 19 wave 10, lane lv10): `worklist` false is the
/// whole-pass loop, unchanged; true is the dirty-block driver
/// (`regalloc::ls_closure_fixed_point_backward`) with the live-out and the
/// transfer built into two reused buffers. `visits` counts block visits.
pub(in crate::ir_lower) fn live_colours_after_calls_with(
    steps: &[Vec<LiveStep>],
    successors: &[Vec<usize>],
    words: usize,
    max_iterations: usize,
    worklist: bool,
    visits: &mut usize,
) -> Option<HashMap<NodeId, Vec<u64>>> {
    let nb = steps.len();
    let transfer = |b: usize, out: &[u64]| -> Vec<u64> {
        let mut live = out.to_vec();
        for step in steps[b].iter().rev() {
            if let Some(c) = step.kill {
                bits_remove(&mut live, c);
            }
            bits_or(&mut live, &step.reads);
        }
        live
    };
    let live_out = |b: usize, live_in: &[Vec<u64>]| -> Vec<u64> {
        let mut out = vec![0u64; words];
        for &s in successors.get(b).map(Vec::as_slice).unwrap_or(&[]) {
            if s < nb {
                bits_or(out.as_mut_slice(), live_in[s].as_slice());
            }
        }
        out
    };
    let mut live_in: Vec<Vec<u64>> = vec![vec![0u64; words]; nb];
    let mut converged = false;
    if worklist {
        // The successor lists padded to `nb` (a missing list is no
        // successors, as `live_out` reads it), then inverted once.
        let padded: Vec<Vec<usize>>;
        let succ: &[Vec<usize>] = if successors.len() >= nb {
            successors
        } else {
            padded = (0..nb)
                .map(|b| successors.get(b).cloned().unwrap_or_default())
                .collect();
            &padded
        };
        let preds = crate::regalloc::LsPredIndex::build(succ, nb);
        let mut live = vec![0u64; words];
        converged = crate::regalloc::ls_closure_fixed_point_backward(
            &preds,
            nb,
            max_iterations,
            visits,
            |b| {
                live.fill(0);
                for &s in succ[b].iter() {
                    if s < nb {
                        bits_or(live.as_mut_slice(), live_in[s].as_slice());
                    }
                }
                for step in steps[b].iter().rev() {
                    if let Some(c) = step.kill {
                        bits_remove(&mut live, c);
                    }
                    bits_or(&mut live, &step.reads);
                }
                if live != live_in[b] {
                    live_in[b].copy_from_slice(&live);
                    true
                } else {
                    false
                }
            },
        );
    } else {
        for _ in 0..max_iterations {
            let mut changed = false;
            *visits += nb;
            for b in (0..nb).rev() {
                let inn = transfer(b, &live_out(b, &live_in));
                if inn != live_in[b] {
                    live_in[b] = inn;
                    changed = true;
                }
            }
            if !changed {
                converged = true;
                break;
            }
        }
    }
    if !converged {
        return None;
    }
    let mut after: HashMap<NodeId, Vec<u64>> = HashMap::new();
    for b in 0..nb {
        let mut live = live_out(b, &live_in);
        for step in steps[b].iter().rev() {
            if let Some(c) = step.kill {
                bits_remove(&mut live, c);
            }
            bits_or(&mut live, &step.reads);
            if step.is_call {
                after.insert(step.node, live.clone());
            }
        }
    }
    Some(after)
}

impl DeadRefSlotClear {
    /// JIT round 20 wave 21 follow-up (lane pollroot): the colours named by any
    /// snapshot at a bci reachable from own bci `bci` -- the plain union
    /// (no def kills) plus `always_kept`, as an allocation site keeps it --
    /// or `None` when `bci` is not an instruction of the method. Read by the
    /// back-edge exit rule ([`back_edge_dead_ref_homes_with`]).
    pub(in crate::ir_lower) fn plain_reach_kept(&mut self, bci: usize) -> Option<Vec<u64>> {
        if !self.verified.is_instruction_start(bci) {
            return None;
        }
        if !self.plain_reach_memo.contains_key(&bci) {
            let mut kept = snapshot_colours_reachable_with_splices(
                &self.verified,
                &self.named,
                &self.splice_named,
                self.words,
                bci,
            );
            bits_or(&mut kept, &self.always_kept);
            self.plain_reach_memo.insert(bci, kept);
        }
        self.plain_reach_memo.get(&bci).cloned()
    }

    /// JIT round 20 wave 21 (lane pollroot,
    /// `jr20-inner-loop-poll-roots-dead-outer-loop-values-FIXED-20261013.md`): the
    /// clearable colours dead at the back-edge poll of the latch whose head
    /// node is `latch_ctrl`, at linear position `pos` and own bci `bci` -- the
    /// call-site rule with the poll as the site: no (value) range of the
    /// colour covers `pos`, and no frame state reachable after the poll names
    /// it (the plain bytecode-reachability union from `bci`, `always_kept`
    /// included, plus the schedule-side liveness the precise keep set
    /// recorded at the poll). Empty -- clear nothing -- when `bci` is not an
    /// instruction or no poll entry was recorded (the precise keep set is
    /// off, or the block is no latch).
    pub(in crate::ir_lower) fn dead_colours_at_back_edge_poll(
        &mut self,
        pos: usize,
        bci: usize,
        latch_ctrl: NodeId,
    ) -> Vec<u32> {
        let Some(mut kept) = self.plain_reach_kept(bci) else {
            return Vec::new();
        };
        let Some(after) = self.after_kept.get(&latch_ctrl) else {
            return Vec::new();
        };
        if kept.len() < after.len() {
            kept.resize(after.len(), 0);
        }
        bits_or(&mut kept, after);
        dead_ref_colours_at(&self.ranges, &self.ref_colours, &kept, pos)
    }

    /// The clearable colours dead at linear position `pos` of the call `site`
    /// at method bci `bci` (the enclosing own invoke's for a call inside a
    /// splice), ascending. Empty when `bci` is not an instruction of the
    /// method or `site` has no after-call keep set (clear nothing).
    pub(in crate::ir_lower) fn dead_colours_at(&mut self, pos: usize, bci: usize, site: NodeId) -> Vec<u32> {
        if !self.verified.is_instruction_start(bci) {
            return Vec::new();
        }
        // gcd d5/u: `CRATONVM_DBG_IR_SLOTS=1` names each input of the answer,
        // so a home still kept at a call can be traced to the rule that kept
        // it (the orchestrator's d4 run could not tell whether d4/o's kill
        // fired at `cleared`'s `System.gc()`).
        let dbg = false;
        // JIT round 20 wave 17, lane gcDead (I-9): an allocation site keeps
        // the whole union (see `alloc_sites`).
        let plain = self.alloc_sites.contains(&site);
        if plain {
            if !self.plain_reach_memo.contains_key(&bci) {
                let mut kept = snapshot_colours_reachable_with_splices(
                    &self.verified,
                    &self.named,
                    &self.splice_named,
                    self.words,
                    bci,
                );
                bits_or(&mut kept, &self.always_kept);
                self.plain_reach_memo.insert(bci, kept);
            }
        } else if !self.reach_memo.contains_key(&bci) {
            let mut kept = snapshot_colours_reachable_with_splices(
                &self.verified,
                &self.named,
                &self.splice_named,
                self.words,
                bci,
            );
            let union = dbg.then(|| kept.clone());
            if !self.def_pc.is_empty() {
                refine_reach_by_def_kills(
                    &mut kept,
                    &self.def_pc,
                    &self.verified,
                    &self.named,
                    &self.splice_named,
                    bci,
                );
            }
            if !self.phi_def_pc.is_empty() {
                refine_reach_by_phi_kills(
                    &mut kept,
                    &self.phi_def_pc,
                    &self.verified,
                    &self.named,
                    &self.splice_named,
                    bci,
                );
            }
            if let Some(union) = union {
                let killed: Vec<usize> = bits_list(&union)
                    .into_iter()
                    .filter(|&c| !bits_contains(&kept, c))
                    .collect();
                eprintln!(
                    "[ir-slots] keep-union bci={bci}: union {:?} def-killed {killed:?} always {:?}",
                    bits_list(&union),
                    bits_list(&self.always_kept),
                );
            }
            bits_or(&mut kept, &self.always_kept);
            self.reach_memo.insert(bci, kept);
        }
        let memo = if plain {
            &self.plain_reach_memo
        } else {
            &self.reach_memo
        };
        let Some(reach) = memo.get(&bci) else {
            return Vec::new();
        };
        let Some(after) = self.after_kept.get(&site) else {
            if dbg {
                eprintln!("[ir-slots] keep n{site} pos={pos} bci={bci}: no after-call set, clears nothing");
            }
            return Vec::new();
        };
        let mut kept = reach.clone();
        bits_or(&mut kept, after);
        let mut dead = dead_ref_colours_at(&self.ranges, &self.ref_colours, &kept, pos);
        // ARGWIN21-8: the site's own result colour, while it still holds the
        // value of an earlier occupant (see the section above
        // `ir_site_result_home_clear_enabled`).
        if let Some(&(c, ref own)) = self.site_result.get(&site) {
            // The one range of `c` covering `pos` must be the result's own,
            // starting AT the site, with every earlier one ended before it.
            let own_range_only = self.ranges.get(c as usize).is_some_and(|list| {
                let idx = list.partition_point(|r| r.lo <= pos);
                idx > 0 && list[idx - 1].lo == pos && (idx < 2 || list[idx - 2].hi < pos)
            });
            if own_range_only
                && !bits_contains(own, c as usize)
                && self.ref_colours.binary_search(&c).is_ok()
                && !dead.contains(&c)
            {
                dead.push(c);
                dead.sort_unstable();
                if dbg {
                    eprintln!("[ir-slots] site n{site}: own result colour {c} cleared before its GC point");
                }
            }
        }
        if dbg {
            eprintln!(
                "[ir-slots] keep n{site} pos={pos} bci={bci}: reach {:?} after {:?} dead {dead:?}",
                bits_list(reach),
                bits_list(after),
            );
        }
        dead
    }
}

/// gcd d5/u: the set bits of `bits`, ascending -- for the `[ir-slots]`
/// diagnostics.
pub(in crate::ir_lower) fn bits_list(bits: &[u64]) -> Vec<usize> {
    let mut out = Vec::new();
    bits_for_each(bits, |c| out.push(c));
    out
}

/// [`snapshot_colours_reachable_from`], plus `splice_named[b]` for every
/// reachable `b` (gen r5w5/oomjit9): the frame states of the splices whose
/// enclosing own invoke is reachable.
pub(in crate::ir_lower) fn snapshot_colours_reachable_with_splices(
    verified: &cratonvm_reader::VerifiedCode,
    named: &HashMap<usize, Vec<u64>>,
    splice_named: &HashMap<usize, Vec<u64>>,
    words: usize,
    start: usize,
) -> Vec<u64> {
    let mut kept = vec![0u64; words];
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut work = vec![start];
    seen.insert(start);
    while let Some(pc) = work.pop() {
        if let Some(bits) = named.get(&pc) {
            bits_or(&mut kept, bits);
        }
        if let Some(bits) = splice_named.get(&pc) {
            bits_or(&mut kept, bits);
        }
        for succ in verified.successors(pc) {
            if seen.insert(succ) {
                work.push(succ);
            }
        }
    }
    kept
}

/// The colours of `ref_colours` that no range of theirs covers `pos` and
/// whose bit is clear in `kept`, ascending. Pure, for the unit tests.
pub(in crate::ir_lower) fn dead_ref_colours_at(
    ranges: &[Vec<LiveRange>],
    ref_colours: &[u32],
    kept: &[u64],
    pos: usize,
) -> Vec<u32> {
    ref_colours
        .iter()
        .copied()
        .filter(|&c| !bits_contains(kept, c as usize))
        .filter(|&c| {
            let Some(list) = ranges.get(c as usize) else {
                // No range recorded: nothing proves it dead, keep it.
                return false;
            };
            // Sorted by `lo` and disjoint: the only range that can cover `pos`
            // is the last one starting at or before it.
            let idx = list.partition_point(|r| r.lo <= pos);
            idx == 0 || list[idx - 1].hi < pos
        })
        .collect()
}

/// gen r5w3/live7: the argument-staging words a liveness claim covers, and
/// of those the ones dead at a call whose lowering writes at most
/// `stage_live` of them (`arg[i]` for `i < stage_live`). `arg[i]` lives at
/// `[rbp - (args_stage_top_off - 8i)]`. Covering stops at the first word that
/// is not strictly above `spill_cap_off` (it would be a colour's home, which
/// this claim must not speak for) or does not fit an `i16`. Both vectors
/// ascend by index, i.e. DESCEND by offset. Pure, for the unit tests.
pub(in crate::ir_lower) fn staging_liveness_words(
    args_stage_top_off: i32,
    args_stage_words: usize,
    spill_cap_off: i32,
    stage_live: usize,
) -> (Vec<i16>, Vec<i16>) {
    let mut covered = Vec::with_capacity(args_stage_words);
    let mut dead = Vec::new();
    for i in 0..args_stage_words {
        let Some(off) = i32::try_from(i)
            .ok()
            .and_then(|i| i.checked_mul(8))
            .and_then(|delta| args_stage_top_off.checked_sub(delta))
        else {
            break;
        };
        if off <= spill_cap_off {
            break;
        }
        let Ok(off) = i16::try_from(off) else {
            break;
        };
        covered.push(off);
        if i >= stage_live {
            dead.push(off);
        }
    }
    (covered, dead)
}


/// Refuse, before a single byte is emitted, a colouring that could alias two
/// simultaneously live values onto one frame word.
///
/// This is the counterpart of [`verify_data_locations`]: that one asks whether
/// every value *has* a location, this one asks whether the locations are
/// *distinct where they must be*. Both failures are silent wrong code — the
/// first reads the saved caller RBP as program data, the second reads one
/// value where another was written — so neither is assumed.
///
/// Four properties, in the order a violation would bite:
///
///   1. every value the lowerer will allocate for has a colour (a plan that
///      missed one would take `alloc_slot`'s internal-bailout path mid-emission
///      instead of declining up front);
///   2. every colour is inside the reservation the frame was sized from;
///   3. **no two values with overlapping live ranges share a colour** — the
///      wrong-code property;
///   4. a colour is never shared across [`SlotClass`]es, so a slot an oop map
///      names can never hold a primitive, and a pinned value never shares at
///      all.
pub(in crate::ir_lower) fn verify_slot_colouring(graph: &Graph, schedule: &Schedule, plan: &SlotPlan) -> CompileResult<()> {
    verify_slot_colouring_with_value_flow(graph, schedule, plan, &crate::regalloc::ValueFlow::default())
}

pub(in crate::ir_lower) fn verify_slot_colouring_with_value_flow(
    graph: &Graph, schedule: &Schedule, plan: &SlotPlan, flow: &crate::regalloc::ValueFlow,
) -> CompileResult<()> {
    let color_of = |id: usize| plan.node_color.get(id).copied().flatten();

    // (1) Coverage — the same two sources `plan_slots` enumerates.
    for (id, node) in graph.nodes.iter().enumerate() {
        if matches!(node.op, Op::Phi) && color_of(id).is_none() {
            return Err(Bailout::with_context(
                BailoutReason::UnallocatedValue { node: id as NodeId },
                format!("phi n{id} has no planned frame slot"),
            ));
        }
    }
    for block in &schedule.blocks {
        for &nid in &block.nodes {
            let node = match graph.nodes.get(nid as usize) {
                Some(node) => node,
                None => {
                    return Err(Bailout::new(BailoutReason::Internal(
                        "ir_lower: schedule names a node outside the graph",
                    )))
                }
            };
            if !flow.is_absorbed(nid) && op_defines_result_slot(&node.op) && color_of(nid as usize).is_none() {
                return Err(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: nid },
                    format!("scheduled n{nid} ({:?}) has no planned frame slot", node.op),
                ));
            }
        }
    }

    // (2) Every colour is inside the reservation.
    for (id, color) in plan.node_color.iter().enumerate() {
        if let Some(color) = color {
            if *color as usize >= plan.slots {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "ir_lower: a planned colour is outside the spill reservation",
                    ),
                    format!("n{id} coloured {color} with only {} slots", plan.slots),
                ));
            }
        }
    }

    // (3) + (4) Group by colour and check the members pairwise. Sorting each
    // group by range start makes the adjacent pairs sufficient: if any pair
    // overlaps, the earliest offender's predecessor in sort order overlaps it
    // too, so an adjacent pair always witnesses the violation.
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); plan.slots];
    for (id, color) in plan.node_color.iter().enumerate() {
        if let Some(color) = color {
            if let Some(bucket) = members.get_mut(*color as usize) {
                bucket.push(id);
            }
        }
    }
    for (color, bucket) in members.iter().enumerate() {
        if bucket.len() < 2 {
            continue;
        }
        for &id in bucket {
            if plan.class.get(id).copied().flatten() == Some(SlotClass::Pinned) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("ir_lower: a pinned value shares its frame slot"),
                    format!(
                        "slot {color} is shared by {} values, one pinned",
                        bucket.len()
                    ),
                ));
            }
        }
        let classes: Vec<Option<SlotClass>> = bucket
            .iter()
            .map(|&id| plan.class.get(id).copied().flatten())
            .collect();
        if classes.windows(2).any(|w| w[0] != w[1]) {
            return Err(Bailout::with_context(
                BailoutReason::Internal(
                    "ir_lower: a frame slot is shared across reference and primitive values",
                ),
                format!("slot {color} mixes {classes:?}"),
            ));
        }
        // The colouring's own ranges (round 14 wave 1): each contains the
        // value range, so this is the stronger check.
        let colouring = plan.colouring_ranges();
        let mut intervals: Vec<(LiveRange, usize)> = bucket
            .iter()
            .filter_map(|&id| colouring.get(id).copied().flatten().map(|r| (r, id)))
            .collect();
        if intervals.len() != bucket.len() {
            return Err(Bailout::new(BailoutReason::Internal(
                "ir_lower: a coloured value has no live range",
            )));
        }
        intervals.sort_by_key(|(r, id)| (r.lo, r.hi, *id));
        for pair in intervals.windows(2) {
            let ((a, ai), (b, bi)) = (pair[0], pair[1]);
            if a.overlaps(b) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "ir_lower: two simultaneously live values share a frame slot",
                    ),
                    format!(
                        "slot {color}: n{ai} live [{}, {}] overlaps n{bi} live [{}, {}]",
                        a.lo, a.hi, b.lo, b.hi
                    ),
                ));
            }
        }
    }
    Ok(())
}

// ── Linear-scan register residency (`CRATONVM_JIT_IR_LINEAR_SCAN`) ───
//
// `regalloc::allocate_linear_scan` is a complete, self-verifying linear-scan
// allocator that had no production consumer. This section is its first one.
//
// It is deliberately NOT a replacement for the frame-slot colouring. The
// colourer still runs, still decides every value's home word, and every value
// is still WRITTEN to that home. What the allocator adds is a *read cache*: a
// value the allocation keeps in one register for its entire live range is also
// copied into that register at its definition, and its later reads take the
// register instead of the frame.
//
// ## Why write-through, and why that is the safe shape
//
// The frame image stays authoritative at every instruction boundary. That is
// what makes this wiring cheap to reason about:
//
//   * `emit_safepoint_map` publishes frame slots. Every live value is still in
//     its slot, so the oop map is exactly as complete as it was — there is no
//     register a collector would have to know about, walk, or *update* on an
//     evacuation. (The register file below is XMM-only, so a reference cannot
//     be register-resident at all; see `IR_LOWER_LS_XMMS`. Two independent
//     reasons, either one sufficient.)
//   * `build_deopt_points` / `FrameState` name frame words. Unchanged.
//   * `emit_phi_copies`, call-argument marshalling and the shadow push all read
//     home slots. Unchanged.
//   * A definition site this wave did not convert simply never publishes
//     residency (see `Lowerer::reg_live`), so all of its reads stay memory
//     reads. A missed *use* site is a missed optimization; there is no edit
//     that turns into wrong code by omission.
//
// The cost is that the store side is not eliminated: this buys loads, not
// stores. Eliminating the store requires the whole lowerer to stop treating the
// frame as the value's identity, which is the large change this one is the
// increment towards. See `docs/jit/linear-scan-wiring.md`.

/// The registers this wiring may hand out.
///
/// XMM2–XMM7, and the ceiling is forced rather than tuned:
///
///   * **Never touched by this emitter.** The FP value tier is XMM0/XMM1; the
///     GP tier is RAX/RCX/RDX with R10/R11 as safepoint and shadow-stack
///     scratch and R8/R9 as call-argument registers. XMM2–XMM7 appear nowhere.
///   * **Encodable without REX**, which `fp_load` / `fp_store` / `fp_binop`
///     require: they emit ModRM with `(xmm & 7) << 3` and no REX.R, so only
///     XMM0–XMM7 are addressable by them at all. *This* is what stops the file
///     at XMM7 — raising it is a REX change in those three functions, not a
///     frame change.
///   * **Callee-saved registers are now paid for.** Win64 makes XMM0–XMM5
///     volatile and XMM6–XMM15 non-volatile; System V makes every XMM volatile.
///     XMM6/XMM7 were unusable here until [`IR_LOWER_SAVED_XMMS`] and the
///     prologue save area landed (2026-08-04): before that `emit_prologue`
///     pushed RBP and saved nothing, so a value parked in XMM6 corrupted the
///     caller's floating-point state on one platform and not the other.
///
/// The consequence worth stating plainly: this wiring is still **FP-only**. An
/// `int` loop counter gets nothing out of it — that needs a GP file, and a GP
/// file has to discharge the safepoint obligation per site instead of
/// structurally (`docs/feature-designs/jit-machine-level-and-instruction-selection.md`,
/// "Where the safepoint / oop-map obligation lives").
///
/// The literal lives in `regalloc::xmm_roles`, with the other two XMM
/// authorities — `ir_lower`'s own scratch pair below and `vec_emit`'s vector
/// pool — so that a wiring which puts two of them on one register is a visible
/// fact rather than a discovery. All three are disjoint as of 2026-08-04;
/// `xmm_roles::disjointness_violation` is the check.
pub(in crate::ir_lower) const IR_LOWER_LS_XMMS: [u8; 6] = crate::regalloc::xmm_roles::IR_LINEAR_SCAN;

/// VEC21-7: the initial `ls_saved_fp` snapshot -- the vector registers of
/// [`IR_LOWER_SAVED_XMMS_VECTOR`] that are not in [`IR_LOWER_SAVED_XMMS`]
/// when the body saves them, else none.
pub(in crate::ir_lower) fn vector_saved_fp(on: bool) -> [bool; 16] {
    let mut cells = [false; 16];
    if on {
        for &r in IR_LOWER_SAVED_XMMS_VECTOR {
            if !IR_LOWER_SAVED_XMMS.contains(&r) {
                if let Some(c) = cells.get_mut(r as usize) {
                    *c = true;
                }
            }
        }
    }
    cells
}

/// The XMM registers [`Lowerer::emit_prologue`] saves and every exit restores.
///
/// Empty on System V, where the ABI makes every XMM volatile and there is
/// nothing to save; XMM6/XMM7 on Windows, where it does not.
///
/// **This is the prerequisite the level-2 lane was blocked on**, and it is
/// deliberately its smallest useful form: two registers, saved only when a
/// method actually parks a value in one, in a frame band the GC verifier
/// already skips (`callee_saved_lo`). It is what lets [`IR_LOWER_LS_XMMS`]
/// reach XMM7 and what moved `vec_emit`'s pool off the scalar file entirely.
///
/// Sizing is static — the frame is laid out in [`Lowerer::new`], before the
/// allocation runs — but *emission* is dynamic: [`Lowerer::saved_xmm_regs`]
/// yields only the registers the residency plan actually used, so a method that
/// promotes none of them emits no save instruction and no restore.
pub(in crate::ir_lower) const IR_LOWER_SAVED_XMMS: &[u8] = crate::regalloc::xmm_roles::IR_PROLOGUE_SAVED;

/// Bytes the frame reserves for [`IR_LOWER_SAVED_XMMS`].
///
/// 16 per register: a caller's value may be a full 128-bit vector, and saving
/// only its low 64 bits restores a register that is *almost* right — the
/// failure mode hardest to attribute. `MOVUPS` has no alignment requirement, so
/// the band needs no padding.
///
/// Zero unless the linear-scan path is on, because nothing else can name one of
/// these registers yet and a frame must not pay 32 bytes for a register it
/// cannot hand out. [`estimate_frame_bytes`] and [`Lowerer::new`] both go
/// through this one function for exactly the reason the `bookkeeping_size`
/// comment gives: two copies of a frame term drift, and the drift is a silent
/// wrong offset.
pub(in crate::ir_lower) fn ir_saved_xmm_bytes() -> i32 {
    if IR_LOWER_SAVED_XMMS.is_empty() || !linear_scan_enabled() {
        return 0;
    }
    // Cast: a two-element compile-time constant.
    IR_LOWER_SAVED_XMMS.len() as i32 * 16
}

/// The XMM registers a VEC21-7 frame saves (empty on System V).
pub(in crate::ir_lower) const IR_LOWER_SAVED_XMMS_VECTOR: &[u8] = crate::regalloc::xmm_roles::IR_PROLOGUE_SAVED_VECTOR;

/// [`ir_saved_xmm_bytes`], or, for a body that saves the vector registers
/// too ([`vector_xmm_saves_wanted`]), 16 per register of
/// [`IR_LOWER_SAVED_XMMS_VECTOR`] -- reserved whether or not the linear scan
/// is on, since the pre-loop's registers need the save either way.
pub(in crate::ir_lower) fn ir_saved_xmm_bytes_with(vector_xmm_saves: bool) -> i32 {
    if vector_xmm_saves && !IR_LOWER_SAVED_XMMS_VECTOR.is_empty() {
        // Cast: a six-element compile-time constant.
        return IR_LOWER_SAVED_XMMS_VECTOR.len() as i32 * 16;
    }
    ir_saved_xmm_bytes()
}

/// The **general-purpose** linear-scan file: the registers an `int` or `long`
/// value may live in for a whole method.
///
/// This is the half the XMM wiring's own doc comment named as missing —
/// *"this wiring is still FP-only. An `int` loop counter gets nothing out of
/// it"* — and it is what made the optimizing tier emit slower code than the
/// baseline tier on every loop measured: `x64.rs` colours Java locals into
/// callee-saved GPRs, while this backend read every value out of a frame word.
///
/// Same shape as the FP file and for the same reasons: write-through, so the
/// frame image stays authoritative at every instruction boundary and no oop
/// map, deopt frame state or phi copy changes; single-segment allocations
/// only, because there is no reload machinery; and a definition site that does
/// not publish simply leaves its reads in memory.
///
/// The one obligation that is genuinely new is the **safepoint** one, and it is
/// discharged by type rather than by structure: see
/// `regalloc::xmm_roles::IR_GP_LINEAR_SCAN`.
pub(in crate::ir_lower) const IR_LOWER_LS_GPRS: &[u8] = &crate::regalloc::xmm_roles::IR_GP_LINEAR_SCAN;

/// The caller-saved GPRs `CRATONVM_JIT_IR_LS_CALLER_SAVED` hands to block-local
/// values (JIT round 25, item 1b): `regalloc::xmm_roles::
/// IR_GP_LINEAR_SCAN_CALLER_SAVED`. Disjoint from [`IR_LOWER_LS_GPRS`], never
/// saved by the prologue and never named by a frame state; see
/// [`plan_caller_saved_residency`].
pub(in crate::ir_lower) const IR_LS_CALLER_SAVED_GPRS: &[u8] = &crate::regalloc::xmm_roles::IR_GP_LINEAR_SCAN_CALLER_SAVED;

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK=1` (JIT round 19
/// proposal wave 4, lane osr4, OSR2-1). **Default ON** since the round-19 proposal round (`=0` turns it off). With
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE`, a promoted loop header's back-edge
/// poll keeps its mode exit: the exit's pad writes every promoted cell back
/// before the frame leaves (`Lowerer::promoted_cell_poll_writebacks`,
/// [`emit_cell_writeback`]). Off: the header's polls are refused
/// (`promoted-field-cells`), as in wave 2. Read without a `static`
/// (`process_global_statics_ratchet`).
pub(crate) fn ir_loop_field_promote_poll_writeback_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PREDICT=1` (JIT round 19 proposal wave
/// 6, lane osr6, OSR5-2). **Default OFF**. Under
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT`, a promoted loop whose
/// deopt point the lowering cannot give a write-back pad refuses the WHOLE
/// compile (the method falls to single-pass). With this switch:
///
/// * the promotion refuses such a loop up front for the causes the graph
///   shows (`loop_field_forward::promote_loop`): a deopt point inside a
///   spliced body (it resumes at the enclosing invoke, whose memory is not
///   the tracked one), and -- unless [`ir_loop_field_promote_pad_xmm_save_enabled`]
///   -- a deopt frame naming a `float` / `double` value (an XMM register the
///   pad's helper call may clobber);
/// * every other refusal (`promoted field cells: ...`: an operand the pad
///   cannot describe, a plan the schedule refutes, a point outside the
///   per-node check) costs one more attempt instead of the method: the IR
///   door rebuilds once with [`Graph::promote_pending_withheld`] set, which
///   promotes only the loops wave 2 admits (`lib.rs`, `ir_tier`).
///
/// Read without a `static` (`process_global_statics_ratchet`).
pub(crate) fn ir_loop_field_promote_predict_enabled() -> bool {
    false
}

/// Is `reason` (a [`last_lower_bail`] label) a promoted loop's write-back
/// refusal, which the OSR5-2 retry answers?
pub(crate) fn is_promoted_cells_lower_bail(reason: &str) -> bool {
    reason.starts_with("promoted field cells")
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PAD_XMM_SAVE=1` (JIT round 19 proposal
/// wave 6, lane osr6, OSR5-4). **Default OFF**. A promoted loop's write-back
/// pads (a poll exit's, OSR2-1; a guard's, OSR2-3) call the layout-aware
/// `jit_putfield_{int,long}`, which may clobber every caller-saved XMM
/// register, so a frame naming an XMM register was refused
/// (`promoted-field-cells-xmm`, and the `xmm=true` compile refusal). With
/// this switch the pad saves each XMM register its point's frame names below
/// `RSP` (above the call's home space) before the write-backs and restores
/// it after them ([`emit_pad_xmm_save`]), so the deopt stub reads the frame's
/// values intact and FP loops are admitted. The helper call stays: an inline
/// compact store needs it anyway for a replaced layout or a legacy object,
/// and a pad cannot deopt again. Read without a `static`.
pub(crate) fn ir_loop_field_promote_pad_xmm_save_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_RESIDENT=1` (JIT round 19
/// proposal wave 7, lane osr7, OSR6-1). **Default OFF**; inert without
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT`.
///
/// Under `PENDING_DEOPT` every value a promoted loop's deopt pads may write
/// back (each silent store's value, each cell's header value phi) was added
/// to the deopt-named set ([`promoted_pending_pin_values`] at
/// `compute_deopt_named_reachable`), so its home word could drop only when
/// it owned its register for the WHOLE method (`deopt_nameable`) -- which in
/// an OSR body with a five- or seven-register file nearly never holds. The
/// home then took a store at every definition and the header phi's edge
/// copy wrote its home every iteration: one store per iteration, the cost
/// the promotion exists to remove (`pending-div-guard` 1.5 -> 2.5 ns).
///
/// With this switch those values are no longer deopt-named (the cell's
/// entry read and its base still are: they are written once, before the
/// loop), and a pad describes a value by its RESIDENT register
/// ([`Lowerer::pending_resident_reg`]) when the allocator's live range of
/// the value strictly contains the point's position, which proves that no
/// other value holds the register there. Otherwise the existing description
/// applies: a kept home is still a word nothing else shares (the slot pin
/// stays, `plan_slots`), and a dropped home with no proven register refuses
/// the compile (fail closed, as every undescribable pad does).
pub(crate) fn ir_loop_field_promote_pending_resident_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_CHAIN=1` (JIT round 19
/// proposal wave 7, lane osr7, OSR5-1). **Default OFF**; inert without
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT`.
///
/// A deopt point inside a spliced body of a promoted loop had no write-back
/// pad: a FLAT point resumes at the enclosing `invoke`, which re-runs the
/// callee's silent stores that precede the point, so writing the tracked
/// cell values back first would apply them twice. A CHAIN point
/// (`splice_chain_for_site`, `CRATONVM_JIT_IR_SPLICE_FRAME_STATES`) resumes
/// at the callee's OWN bci over its parked callers, where the memory state
/// the plan tracks is exactly the program's. Under this switch such a node
/// is given the pad, and [`Lowerer::end_promoted_pending`] checks that every
/// point it published carries a caller chain (`FrameState::caller`); a flat
/// one still refuses the compile.
pub(crate) fn ir_loop_field_promote_pending_chain_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_BRANCH=1` (JIT round 19
/// proposal wave 7, lane osr7, OSR5-3). **Default OFF**; inert without
/// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT`.
///
/// The promotion refused a loop that holds a deopt point and a body memory
/// phi (a cell store under a branch), because [`plan_promoted_pending`]
/// needs one value per cell at every block entry. Under this switch the
/// promotion admits it, and at a join whose predecessors disagree the plan
/// takes the value phi the promotion's memory walk built at that join
/// (`loop_field_forward::Walk::phi_value`) when its inputs are exactly the
/// predecessors' values, edge by edge. No such phi (the walk never crossed
/// the join, or a later pass removed it) leaves the cell unknown there, and a
/// deopt point reached with it unknown refuses the compile, as before.
pub(crate) fn ir_loop_field_promote_pending_branch_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_LOOP_PROMOTE_DEOPT_PADS` (JIT round 21, lane promote,
/// TYPECHECK21-9; page
/// `docs/feature-designs/real-frame-deopt.md`).
/// **Default ON** since the w3o A/B (static-array-acc 814 vs 2759 ns, facts
/// equal to HotSpot in both modes), `=0` restores the refusal. The pending
/// write-back pads it reuses (`..._PENDING_DEOPT`'s, round 19 OSR2-3) are
/// still opt-in for the field promotion: their A/B lost on the pads' home
/// pins, which this path pays too unless `..._PENDING_RESIDENT` is on.
///
/// What it does: the STATIC and FRESH-OBJECT loop promotions
/// (`loop_field_forward::promote_loop_statics` / `promote_fresh_loop_fields`)
/// admit a loop holding a deopt point (a guard, a primitive array access or
/// length, an integer division) instead of refusing it (`a node that can
/// deopt`). Their in-loop cell stores are then kept as SILENT stores
/// (`Graph::promoted_sunk_stores`), and every deopt pad inside the loop
/// writes each cell's current value back before the frame leaves -- the
/// field promotion's pending machinery ([`plan_promoted_pending`],
/// `Lowerer::describe_pending_writebacks`), here also for a static cell
/// (the inline `putstatic`'s two stores, [`emit_static_cell_store`]). An
/// undescribable pad refuses the compile, and the IR door then retries once
/// without such promotions (`Graph::promote_pending_withheld`, `lib.rs`
/// `ir_tier`). Read without a `static` (`process_global_statics_ratchet`).
pub(crate) fn ir_loop_promote_deopt_pads_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_PROMOTE_DEOPT_PADS_BRANCH` (PROMOTE21-1, the lane
/// promote residual of TYPECHECK21-9). **Default ON since j27 2026-10-28**
/// (`=0` kills; was opt-in): correct (facts
/// equal HotSpot's in every arm) and engaged, but flat on its rows (host
/// 64.177.43.150, 2026-10-26: `R21PromoteResiduals` static-branch-count
/// 1855 ns on against 1851 with `=0`, fresh-branch-count 2380 / 2534 -- the
/// join value phis' home pins cost what the deleted accesses saved); inert
/// without [`ir_loop_promote_deopt_pads_enabled`]. The static and fresh-object
/// promotions admit a loop holding a deopt point AND a store under a branch
/// (`if (a[i] > 0) COUNT++`; a body memory φ, refused before): the pending
/// plan takes each join's value φ, which the promotion's walk builds
/// (`StaticWalk::phi_value` / `FreshWalk::phi_value`) and roots
/// (`Graph::promoted_join_phis`), and pins it like the other pending values
/// (`promoted_pending_slot_pins`) -- the field promotion's OSR5-3 path
/// (`..._PENDING_BRANCH`), here for the static / fresh cells. A join left
/// unknown refuses its deopt points in the lowering, and the IR door retries
/// once without pads. Read without a `static`.
pub(crate) fn ir_loop_promote_deopt_pads_branch_enabled() -> bool {
    ir_loop_promote_deopt_pads_enabled()
        // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
        && true
}

/// May the pending plan read a join's value φ (OSR5-3's `..._PENDING_BRANCH`
/// for the field promotion, PROMOTE21-1's `..._PROMOTE_DEOPT_PADS_BRANCH` for
/// the static / fresh ones)?
pub(in crate::ir_lower) fn pending_join_values_enabled() -> bool {
    ir_loop_field_promote_pending_branch_enabled() || ir_loop_promote_deopt_pads_branch_enabled()
}

/// JIT round 21 (lane promote, TYPECHECK21-9): the parts of a promoted cell
/// the pending plan and its pins need, for a FIELD cell
/// ([`promoted_cell_shape_of`]) or a STATIC one
/// ([`promoted_static_cell_shape_of`]).
pub(in crate::ir_lower) struct PendingCell {
    /// The cell's value at the loop header (the exit write-back's `v`).
    pub(in crate::ir_lower) value: NodeId,
    /// The pre-header read.
    pub(in crate::ir_lower) init: NodeId,
    /// The object, for a field cell; `None` for a static cell.
    pub(in crate::ir_lower) base: Option<NodeId>,
    /// A field cell's constant field index (`0` for a static cell).
    pub(in crate::ir_lower) field_index: i64,
    /// The input slot of the cell's stores that holds the stored value:
    /// `4` for `Op::Store` (`[ctrl, mem, base, idx, value]`), `2` for
    /// `Op::StoreStatic` (`[ctrl, mem, value]`).
    pub(in crate::ir_lower) value_slot: usize,
}

/// [`PendingCell`] of the exit write-back store `exit`: the field shape, else
/// (TYPECHECK21-9) the static one.
pub(in crate::ir_lower) fn pending_cell_of(graph: &Graph, exit: NodeId) -> Option<PendingCell> {
    if let Some(c) = promoted_cell_shape_of(graph, exit) {
        return Some(PendingCell {
            value: c.value,
            init: c.init,
            base: Some(c.base),
            field_index: c.field_index,
            value_slot: 4,
        });
    }
    let s = promoted_static_cell_shape_of(graph, exit)?;
    Some(PendingCell {
        value: s.value,
        init: s.init,
        base: None,
        field_index: 0,
        value_slot: 2,
    })
}

/// A promoted cell as its exit write-back store names it
/// (`Lowerer::promoted_cell_shape`).
pub(in crate::ir_lower) struct PromotedCellShape {
    pub(in crate::ir_lower) base: NodeId,
    pub(in crate::ir_lower) field_index: i64,
    pub(in crate::ir_lower) long: bool,
    pub(in crate::ir_lower) value: NodeId,
    pub(in crate::ir_lower) init: NodeId,
}

/// [`Lowerer::promoted_cell_shape`] over a bare graph (the slot plan and the
/// OSR2-3 pending-store plan ask it before a lowerer exists).
pub(in crate::ir_lower) fn promoted_cell_shape_of(graph: &Graph, store: NodeId) -> Option<PromotedCellShape> {
    let node_of = |id: NodeId| graph.nodes.get(id as usize);
    let st = node_of(store)?;
    let long = match st.op {
        Op::Store(MemKind::Long) => true,
        Op::Store(MemKind::Int) => false,
        _ => return None,
    };
    let (ctrl, base, idx, value) = (
        st.input_opt(0)?,
        st.input_opt(2)?,
        st.input_opt(3)?,
        st.input_opt(4)?,
    );
    let Op::Const(field_index) = node_of(idx)?.op else {
        return None;
    };
    let taken = node_of(ctrl)?;
    if taken.op != Op::Proj(0) {
        return None;
    }
    let iff = node_of(taken.input_opt(0)?)?;
    if iff.op != Op::If {
        return None;
    }
    let cond = node_of(iff.input_opt(1)?)?;
    if cond.op != Op::Cmp(crate::ir::CmpOp::Ne) {
        return None;
    }
    let (a, b) = (cond.input_opt(0)?, cond.input_opt(1)?);
    let init = if long {
        let three_way = node_of(a)?;
        let zero = node_of(b)?;
        if three_way.op != Op::LCmp || zero.op != Op::Const(0) {
            return None;
        }
        if three_way.input_opt(0)? != value {
            return None;
        }
        three_way.input_opt(1)?
    } else {
        if a != value {
            return None;
        }
        b
    };
    Some(PromotedCellShape {
        base,
        field_index,
        long,
        value,
        init,
    })
}

/// Where a write-back operand is at a poll exit's pad (OSR2-1): a frame word
/// `[RBP + off]`, a callee-saved residency register, or an immediate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum CellWriteBackSrc {
    Frame(i32),
    Reg(u8),
    Imm(i64),
}

/// One promoted cell's write-back at a poll exit (OSR2-1): `if (value !=
/// init) helper(base, field_index, value)`, `base` read from the reference
/// frame word `[RBP + base_off]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct CellWriteBack {
    pub(in crate::ir_lower) base_off: i32,
    pub(in crate::ir_lower) field_index: i64,
    pub(in crate::ir_lower) long: bool,
    pub(in crate::ir_lower) value: CellWriteBackSrc,
    pub(in crate::ir_lower) init: CellWriteBackSrc,
    pub(in crate::ir_lower) helper: u64,
    /// JIT round 20 wave 11 (lane pLoop, OSR4-2): a promoted STATIC cell,
    /// stored inline (`base_off` and `helper` unused). `None` for a field.
    pub(in crate::ir_lower) static_cell: Option<StaticCellWriteBack>,
}

/// JIT round 20 wave 11 (lane pLoop, OSR4-2): a promoted static cell's
/// address, as `Lowerer::emit_inline_putstatic` bakes it -- the class's
/// statics-base cell (re-read at the pad, so a republished block is
/// followed), the cell's word-0 and payload-word offsets in the block, and
/// its width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct StaticCellWriteBack {
    pub(in crate::ir_lower) base_cell: u64,
    pub(in crate::ir_lower) cell_off: i32,
    pub(in crate::ir_lower) hi_off: i32,
    pub(in crate::ir_lower) long: bool,
}

/// A promoted static cell as its exit write-back store names it
/// ([`promoted_static_cell_shape_of`]).
pub(in crate::ir_lower) struct PromotedStaticCellShape {
    pub(in crate::ir_lower) class_id: u32,
    pub(in crate::ir_lower) field_index: u32,
    pub(in crate::ir_lower) long: bool,
    pub(in crate::ir_lower) value: NodeId,
    pub(in crate::ir_lower) init: NodeId,
}

/// JIT round 20 wave 11 (lane pLoop, OSR4-2): the static cell an exit
/// write-back store of `loop_field_forward::promote_loop_statics` names --
/// `None` unless `store` is a live non-volatile `StoreStatic` of an
/// `int`-category or `long` field, controlled by the taken edge of
/// `If(Cmp(Ne, v, init))` (`If(Cmp(Ne, LCmp(v, init), 0))` for a `long`)
/// where `v` is the value it stores: the shape that pass builds.
pub(in crate::ir_lower) fn promoted_static_cell_shape_of(graph: &Graph, store: NodeId) -> Option<PromotedStaticCellShape> {
    let node_of = |id: NodeId| graph.nodes.get(id as usize);
    let st = node_of(store)?;
    let (class_id, field_index, long) = match st.op {
        Op::StoreStatic {
            class_id,
            field_index,
            type_tag,
            is_volatile: false,
        } => match type_tag {
            b'J' => (class_id, field_index, true),
            b'I' | b'Z' | b'B' | b'C' | b'S' => (class_id, field_index, false),
            _ => return None,
        },
        _ => return None,
    };
    let (ctrl, value) = (st.input_opt(0)?, st.input_opt(2)?);
    let taken = node_of(ctrl)?;
    if taken.op != Op::Proj(0) {
        return None;
    }
    let iff = node_of(taken.input_opt(0)?)?;
    if iff.op != Op::If {
        return None;
    }
    let cond = node_of(iff.input_opt(1)?)?;
    if cond.op != Op::Cmp(crate::ir::CmpOp::Ne) {
        return None;
    }
    let (a, b) = (cond.input_opt(0)?, cond.input_opt(1)?);
    let init = if long {
        let three_way = node_of(a)?;
        let zero = node_of(b)?;
        if three_way.op != Op::LCmp || zero.op != Op::Const(0) {
            return None;
        }
        if three_way.input_opt(0)? != value {
            return None;
        }
        three_way.input_opt(1)?
    } else {
        if a != value {
            return None;
        }
        b
    };
    Some(PromotedStaticCellShape {
        class_id,
        field_index,
        long,
        value,
        init,
    })
}

/// The map entry created for one safepoint publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct SafepointMapToken {
    pub(in crate::ir_lower) index: usize,
    pub(in crate::ir_lower) id: u32,
    pub(in crate::ir_lower) id_stored: bool,
    pub(in crate::ir_lower) last_return: u32,
}

/// A poll's mode exit: its frame state, and (OSR2-1) the promoted cells its
/// pad writes back first (empty everywhere but a promoted loop header).
pub(in crate::ir_lower) struct PollModeExit {
    pub(in crate::ir_lower) frame_state: FrameState,
    pub(in crate::ir_lower) cell_writebacks: Vec<CellWriteBack>,
}


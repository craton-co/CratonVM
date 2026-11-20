#[allow(unused_imports)]
use super::super::*;

impl CompiledMethod {
    /// [`Self::debugger_home_locals`] at `bci`, if the compile recorded it.
    pub fn debugger_home_locals_at(&self, bci: u32) -> Option<&[DebuggerHomeLocal]> {
        let at = self
            .debugger_home_locals
            .binary_search_by_key(&bci, |(b, _)| *b)
            .ok()?;
        Some(&self.debugger_home_locals[at].1)
    }

    /// Does a kept post-call exit site follow the call at `call_bci`
    /// ([`Self::debugger_exit_call_bcis`])? Stage 3a of
    /// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`
    /// (interpreter round i6 wave 121, lane L2): the question a write to a
    /// compiled activation suspended in that call must answer `true` to before
    /// stage 3b may serve it. `false` for every compile made without a
    /// debugger, so a write stays `OPAQUE_FRAME` there. Test-only since JIT round 22 wave 7 (lane review):
    /// the VM asks [`Self::debugger_exit_bci_after_call`].
    #[cfg(test)]
    pub fn debugger_exit_after_call(&self, call_bci: u32) -> bool {
        self.debugger_exit_bci_after_call(call_bci).is_some()
    }

    /// The bci of the `OsrExit` map of the kept post-call exit after the call
    /// at `call_bci` ([`Self::debugger_exit_call_bcis`]), if any. Interpreter
    /// round i6 wave 122, lane L2.
    pub fn debugger_exit_bci_after_call(&self, call_bci: u32) -> Option<u32> {
        let at = self
            .debugger_exit_call_bcis
            .partition_point(|&(call, _)| call < call_bci);
        match self.debugger_exit_call_bcis.get(at) {
            Some(&(call, exit)) if call == call_bci => Some(exit),
            _ => None,
        }
    }

    /// Stage 3b, first slice, of
    /// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`
    /// (interpreter round i6 wave 122, lane L2): could a debugger write to the
    /// first `locals` locals of an activation of this body suspended in the
    /// call at `call_bci` be carried to the interpreter frame the call's
    /// post-call exit rebuilds? Fail closed, in order:
    /// * a kept exit must follow the call ([`Self::debugger_exit_bci_after_call`]);
    /// * a redefinition must not have spared the body's exits as its class's
    ///   obsolete activation (such a body never leaves);
    /// * every `OsrExit` map filed at the exit bci must be a single frame (no
    ///   caller chain: a splice's exit rebuilds two frames) and describe at
    ///   least `locals` locals, and there must be one.
    ///
    /// The map's own value of a local does not matter: the write replaces it,
    /// and a local the map holds `Undefined` is a slot the rebuilt frame has.
    /// Nothing is written or forced here; stage 3b's later slices do that.
    pub fn debugger_write_door(&self, call_bci: u32, locals: usize) -> DebuggerWriteDoor {
        use std::sync::atomic::Ordering::Acquire;
        let Some(exit_bci) = self.debugger_exit_bci_after_call(call_bci) else {
            return DebuggerWriteDoor::NoExit;
        };
        if self.exits_spared_as_obsolete.load(Acquire) {
            return DebuggerWriteDoor::SparedAsObsolete;
        }
        let mut maps = 0usize;
        for point in self
            .deopt_points
            .iter()
            .chain(self._deopt_point_boxes.iter().map(|b| &**b))
            .filter(|p| p.bci == exit_bci && p.reason == deopt::DeoptReason::OsrExit)
        {
            if point.frame_state.caller.is_some() {
                return DebuggerWriteDoor::ChainExit { exit_bci };
            }
            if point.frame_state.locals.len() < locals {
                return DebuggerWriteDoor::ShortMap {
                    exit_bci,
                    described: point.frame_state.locals.len(),
                };
            }
            maps += 1;
        }
        if maps == 0 {
            return DebuggerWriteDoor::NoMap { exit_bci };
        }
        DebuggerWriteDoor::Open { exit_bci }
    }
}

/// [`CompiledMethod::debugger_write_door`]'s answer. Only `Open` may carry a
/// write; every other answer keeps `OPAQUE_FRAME`. Interpreter round i6 wave
/// 122, lane L2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebuggerWriteDoor {
    /// A kept single-frame exit at `exit_bci` describes every local.
    Open { exit_bci: u32 },
    /// No kept post-call exit follows the call.
    NoExit,
    /// A redefinition spared the body's exits; its frames never leave.
    SparedAsObsolete,
    /// The exit rebuilds a caller chain (a splice), not one frame.
    ChainExit { exit_bci: u32 },
    /// The exit's map describes only `described` locals.
    ShortMap { exit_bci: u32, described: usize },
    /// No `OsrExit` map stands at the exit bci (it was dropped or shared away).
    NoMap { exit_bci: u32 },
}

impl DebuggerWriteDoor {
    /// A short name for the census line.
    pub fn name(self) -> &'static str {
        match self {
            Self::Open { .. } => "exit-after-call",
            Self::NoExit => "refused:no-exit",
            Self::SparedAsObsolete => "refused:spared-as-obsolete",
            Self::ChainExit { .. } => "refused:chain-exit",
            Self::ShortMap { .. } => "refused:short-map",
            Self::NoMap { .. } => "refused:no-map",
        }
    }
}

impl CompiledMethod {
    /// Has [`JitCache::force_exit_polls_for_the_interpreter`] (or a
    /// redefinition's force) written every post-call exit site of this body?
    /// Interpreter round i6 wave 123, lane L2.
    pub fn post_call_exits_forced(&self) -> bool {
        self.post_call_exits_forced
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Stage 3b, force slice, of
    /// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`
    /// (interpreter round i6 wave 123, lane L2): after the VM withdrew this
    /// body's class for the interpreter
    /// (`JitRealm::withdraw_class_for_the_interpreter`, the breakpoint's scoped
    /// withdrawal), will an activation suspended in the call at `call_bci`
    /// leave for the interpreter at the post-call exit right after it, with a
    /// map a write to its first `locals` locals can be applied to?
    ///
    /// The post-call stub's verdict (the VM's `jit_safepoint_loop_exit_verdict`)
    /// sends a withdrawn body out with no agent gate
    /// (`withdrawn_body_may_leave`), so no further hold is needed: the
    /// withdrawal mark is never cleared. Fail closed, in order:
    /// * the write door must be open ([`Self::debugger_write_door`]);
    /// * `withdrawn`: the VM's cache must count the body withdrawn
    ///   (`JitCache::body_withdrawn_by_redefinition`; an unarmed not-entrant
    ///   hook marks nothing);
    /// * `obsolete`: the VM's verdict must not refuse it as an obsolete body
    ///   (its class redefined since it was compiled);
    /// * every post-call site must have been written
    ///   ([`Self::post_call_exits_forced`]).
    pub fn debugger_force_verdict(
        &self,
        call_bci: u32,
        locals: usize,
        withdrawn: bool,
        obsolete: bool,
    ) -> DebuggerForceVerdict {
        let door = self.debugger_write_door(call_bci, locals);
        let DebuggerWriteDoor::Open { exit_bci } = door else {
            return DebuggerForceVerdict::Door(door);
        };
        if !withdrawn {
            return DebuggerForceVerdict::NotWithdrawn { exit_bci };
        }
        if obsolete {
            return DebuggerForceVerdict::Obsolete { exit_bci };
        }
        if !self.post_call_exits_forced() {
            return DebuggerForceVerdict::SitesNotForced { exit_bci };
        }
        DebuggerForceVerdict::Leaves { exit_bci }
    }
}

/// [`CompiledMethod::debugger_force_verdict`]'s answer. Only `Leaves` may
/// carry a write; every other answer keeps `OPAQUE_FRAME`. Interpreter round
/// i6 wave 123, lane L2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebuggerForceVerdict {
    /// The activation leaves at the post-call exit at `exit_bci`.
    Leaves { exit_bci: u32 },
    /// The write door is not open; nothing was asked of the body.
    Door(DebuggerWriteDoor),
    /// The body is not withdrawn, so its verdict says "stay".
    NotWithdrawn { exit_bci: u32 },
    /// The body is an obsolete activation; its verdict says "stay".
    Obsolete { exit_bci: u32 },
    /// Some post-call site was not written (refused, or never forced).
    SitesNotForced { exit_bci: u32 },
}

impl DebuggerForceVerdict {
    /// A short name for the census line.
    pub fn name(self) -> &'static str {
        match self {
            Self::Leaves { .. } => "leaves",
            Self::Door(door) => door.name(),
            Self::NotWithdrawn { .. } => "refused:not-withdrawn",
            Self::Obsolete { .. } => "refused:obsolete",
            Self::SitesNotForced { .. } => "refused:sites-not-forced",
        }
    }
}

/// Reset the fall-through signal at the start of a compile.
pub(crate) fn reset_ir_fall_through_signal() {
    IR_PIPELINE_ENTERED.with(|c| c.set(false));
    IR_FELL_THROUGH_TO_SINGLE_PASS.with(|c| c.set(false));
    IR_FALL_THROUGH_CAUSE.with(|c| c.set(IrFallThroughCause::Unclassified));
}

pub(crate) fn note_ir_pipeline_entered() {
    IR_PIPELINE_ENTERED.with(|c| c.set(true));
    // Each optimizing attempt answers for itself: a cause left by an earlier
    // attempt of the same compile must not describe a later one's bail.
    IR_FALL_THROUGH_CAUSE.with(|c| c.set(IrFallThroughCause::Unclassified));
}

/// The IR builder's row-less field-site refusal at `pc` of the method's own
/// code ([`IrFallThroughCause::RowlessFieldSite`]). Cold: once per refused
/// build. The builder stops at this refusal, so the cause is the build's.
#[cold]
pub(crate) fn note_ir_rowless_field_site_bail(pc: usize, opcode: u8) {
    let pc = u32::try_from(pc).unwrap_or(u32::MAX);
    IR_FALL_THROUGH_CAUSE.with(|c| c.set(IrFallThroughCause::RowlessFieldSite { pc, opcode }));
}

/// Record that this thread's optimizing attempt lowered a body the acceptance
/// gate then refused ([`IrFallThroughCause::AcceptanceRefused`]; JIT r22 wave
/// 2, lane inline).
pub(crate) fn note_ir_acceptance_refused() {
    IR_FALL_THROUGH_CAUSE.with(|c| c.set(IrFallThroughCause::AcceptanceRefused));
}

/// Why the compile that just finished on this thread fell through to the
/// single-pass backend: `None` when it did not
/// ([`last_compile_fell_through_to_single_pass`] is `false`), else the
/// builder's classification. Read on the same thread, right after the compile
/// call, as the bool is.
pub fn last_compile_fall_through_cause() -> Option<IrFallThroughCause> {
    if !last_compile_fell_through_to_single_pass() {
        return None;
    }
    Some(IR_FALL_THROUGH_CAUSE.with(|c| c.get()))
}


pub(crate) fn note_single_pass_entered() {
    if IR_PIPELINE_ENTERED.with(|c| c.get()) {
        IR_FELL_THROUGH_TO_SINGLE_PASS.with(|c| c.set(true));
    }
}

/// Did the compile that just finished on this thread run the whole optimizing
/// pipeline and then produce a single-pass body anyway?
///
/// True means the C2 task paid for an IR build, optimize and schedule, threw
/// them away (`ir_lower::lower_inner` returning `None` is the common route),
/// and recompiled the method with the backend that had already compiled it.
pub fn last_compile_fell_through_to_single_pass() -> bool {
    IR_FELL_THROUGH_TO_SINGLE_PASS.with(|c| c.get())
}

/// May the branch at `pc` be offered to `IrBuilder::set_pruned_branches`?
///
/// The profile feed asks two questions and this is the second one. The first
/// — "was the cold arm taken zero times over a big enough sample" — is about
/// the counters and lives at the call site. This one is about the *shape of
/// the branch*, and it exists because a zero count means two different things
/// depending on which way the branch points.
///
/// # Why a BACKWARD branch is never a candidate
///
/// A bottom-tested loop (`do { … } while (cond)`, a retry or spin loop, any
/// method that tripped the OSR threshold while the loop was still running)
/// has a back edge whose cold arm is the loop **exit**. That arm has been
/// taken zero times precisely because the loop has not finished yet, so
/// `not_taken == 0 && taken >= MIN_OBSERVATIONS_TO_PRUNE` holds on it in
/// every such profile — and speculating on it plants a guard that deopts on
/// the final iteration of every loop it ever fires on. The counter is
/// evidence about *when the profile was taken*, not about where control goes.
///
/// `IrBuilder::prune_always_taken_branch` already declines any branch whose
/// target is at or below its own pc (that refusal is also what stopped the
/// builder looping forever on one). This filter is not a second safety net
/// for the hang: it is the feed refusing to *say* something it knows is
/// false. A candidate the builder is guaranteed to decline is a lie in the
/// profile plumbing and it inflates nothing visible — `branches_pruned`
/// counts prunes, not offers — which is exactly what makes it the kind of
/// divergence nobody notices until it is load-bearing.
///
/// The target is also required to land inside the compiling method
/// (`t < code_len`). A pc past the end cannot be walked, so offering it is
/// the same lie in a different register; a stale profile keyed to a
/// since-redefined method is the way that happens.
pub(crate) fn speculatable_forward_branch(code: &[u8], code_len: usize, pc: usize) -> bool {
    // `None` for anything that is not an offset branch at all, which a stale
    // profile can also produce.
    bytecode_analysis::offset_branch_target(code, pc).is_some_and(|t| t > pc && t < code_len)
}

/// nio21 wave 17 (lane L4): the branch pcs `IrBuilder::set_never_taken_branches`
/// is offered -- the profile feed of `IrBuilder::prune_never_taken_branch`
/// (HotSpot's `unstable_if`), under `ir::ir_prune_never_taken_enabled`.
///
/// A pc qualifies when its TAKEN edge was never observed over at least
/// [`ir::MIN_OBSERVATIONS_TO_PRUNE`] executions, it is a conditional forward
/// branch inside the compiling method ([`never_taken_prune_candidate`]: never
/// a loop exit), and this VM's de-spec registry has not withdrawn the pc
/// (the trap fired `PER_BCI_DESPEC_LIMIT` times there; the method's next
/// compile keeps the branch).
pub(crate) fn ir_never_taken_branches(
    profile: Option<&profile::MethodProfile>,
    code: &[u8],
    code_len: usize,
    despec: Option<&deopt::DespecRegistry>,
    method_key: &str,
) -> std::collections::HashSet<usize> {
    let Some(prof) = profile else {
        return std::collections::HashSet::new();
    };
    let report = false;
    let mut out = std::collections::HashSet::new();
    for (&pc, c) in prof.branches.iter() {
        // CF-W9-2 (`CRATONVM_JIT_IR_PRUNE_RARE_ARM`): or taken rarely enough.
        if !branch_edge_prunable(c.taken, c.not_taken) {
            continue;
        }
        if !never_taken_prune_candidate(code, code_len, pc) {
            continue;
        }
        let withdrawn =
            u32::try_from(pc).map_or(true, |b| despec.is_some_and(|d| d.contains(method_key, b)))
                // j27 wave 5 (lane TRAPPROF): or the bci's trap bit
                // (`CRATONVM_JIT_PROF_BRANCH_TRAP_BIT`), set before the
                // eager re-queue that the de-spec insert above follows.
                || prof.branch_trapped(pc);
        if withdrawn {
            if report {
                eprintln!(
                    "[ir] never-taken branch at pc {pc} in {method_key} WITHDRAWN (de-spec): \
                     the trap fired there; the branch is built"
                );
            }
            continue;
        }
        out.insert(pc);
    }
    // No loop may be cut off by the pruned edges: a header the walk no longer
    // reaches loses its OSR arm (the door can enter a body at any header, and a
    // profile taken while that loop ran mid-invocation never saw the edge into
    // it). Greedily, in pc order: a candidate whose removal (with the ones
    // already kept) would leave a loop header unreachable is dropped.
    let headers: Vec<usize> = bytecode_analysis::back_edges(code, code_len)
        .iter()
        .map(|&(t, _)| t)
        .collect();
    if !headers.is_empty() && !out.is_empty() {
        let mut sorted: Vec<usize> = out.iter().copied().collect();
        sorted.sort_unstable();
        let mut kept: Vec<(usize, usize)> = Vec::new();
        for pc in sorted {
            let Some(target) = bytecode_analysis::offset_branch_target(code, pc) else {
                out.remove(&pc);
                continue;
            };
            kept.push((pc, target));
            if !pcs_reachable_without(code, code_len, &kept, &headers) {
                kept.pop();
                out.remove(&pc);
                if report {
                    eprintln!(
                        "[ir] never-taken branch at pc {pc} in {method_key} NOT offered: its \
                         taken edge is the only way into a loop"
                    );
                }
            }
        }
    }
    if report && !out.is_empty() {
        let mut pcs: Vec<usize> = out.iter().copied().collect();
        pcs.sort_unstable();
        eprintln!(
            "[ir] never-taken feed in {method_key}: {} profiled branch(es), offered {pcs:?}",
            prof.branches.len()
        );
    }
    // j27 wave 8 (lane W8LOOP): race B's evidence -- printed for every
    // compile with branch rows, the thin-row ones that offer nothing included.
    if report && !prof.branches.is_empty() {
        let census = never_taken_row_census::row_census(&prof.branches);
        eprintln!("{}", never_taken_row_census::census_line(method_key, census, out.len()));
    }
    out
}

/// nio21 wave 17 (lane L4): is every pc of `wanted` reachable from pc 0 along
/// normal flow ([`bytecode_analysis::lenient_successors`]) once the `(from,
/// to)` edges of `removed` are deleted? A pc that is not reachable even with
/// every edge (handler-only code) does not count against the answer.
pub(crate) fn pcs_reachable_without(
    code: &[u8],
    code_len: usize,
    removed: &[(usize, usize)],
    wanted: &[usize],
) -> bool {
    let code_len = code_len.min(code.len());
    let walk = |skip: &[(usize, usize)]| {
        let mut seen = vec![false; code_len + 1];
        let mut work = vec![0usize];
        if code_len > 0 {
            seen[0] = true;
        }
        while let Some(pc) = work.pop() {
            bytecode_analysis::for_each_lenient_successor(code, code_len, pc, |succ| {
                if succ < code_len && !seen[succ] && !skip.contains(&(pc, succ)) {
                    seen[succ] = true;
                    work.push(succ);
                }
            });
        }
        seen
    };
    let all = walk(&[]);
    let cut = walk(removed);
    wanted.iter().all(|&h| h >= code_len || !all[h] || cut[h])
}

/// nio21 wave 17 (lane L4): may the never-taken TAKEN edge of the conditional
/// branch at `pc` be speculated away? Only a conditional (`if*`, not `goto`)
/// forward branch inside the method ([`speculatable_forward_branch`]) that is
/// NOT a loop exit: for no textual back edge `t <= s` ([`bytecode_analysis::back_edges`])
/// does `t <= pc < s` hold with the target past `s`.
///
/// # Why a loop exit is never a candidate
///
/// The same reason [`speculatable_forward_branch`] gives for an always-taken
/// back edge: a profile taken while a long loop is still running (an OSR
/// trigger, the C2 window opened mid-loop) has seen the loop's exit zero times
/// because the loop has not finished, not because it never exits. Pruning it
/// would plant a guard that deopts on the last iteration of every run.
/// `CRATONVM_JIT_IR_SPLICE_NEVER_TAKEN` (JIT round 23 wave 4, lane L1, CF23-2;
/// perf, **default ON** since the JIT round 25 review, `=0` kills; see the
/// note at the end): HotSpot's `unstable_if` inside an
/// inlined callee. A conditional forward branch of a SPLICED body whose taken
/// edge the callee's own profile never saw ([`splice_never_taken_pcs`], filled
/// by the VM's resolver into [`InlineSite::callee_never_taken_pcs`]) is built
/// as a guard on its fall-through (`IrBuilder::prune_never_taken_branch`),
/// and the code only that edge reached is not built, so escape analysis sees
/// one path (`R21CfChain`: `uniApplyStage`'s pending arm, which made the new
/// stage and its `UniApply` escape on the done rows).
///
/// The guard is chain-framed (the callee at the branch, every caller parked
/// at its invoke; refused without a chain snapshot there), so a taken edge
/// deopts into the callee's own frame and the interpreter takes the branch. It
/// is listed in `Graph::chain_predicate_guards` under
/// [`deopt::PredicateKind::UnstableIf`]: its point carries
/// [`deopt::chain_predicate_speculation_id`] of the callee's branch, the VM's
/// chain sinks charge it at the outermost call site under that id, and the
/// next compile drops a withdrawn row ([`deopt::chain_predicate_withdrawn`],
/// asked where the rows are handed to the builder). Read per compile and per
/// resolved body.
///
/// JIT round 25 review
/// (`docs/feature-designs/deopt-osr.md`):
/// from JIT r23 wave 9 to the review the feed was this switch OR
/// [`ir_prune_rare_arm_enabled`] (default ON), so the spliced prune was ON on
/// every default run and this key, read opt-in, gated nothing: `=0` did not
/// stop it. Now this key IS the spliced prune's own switch, default ON (the
/// default the feed already had), `=0` the kill; `CRATONVM_JIT_IR_PRUNE_RARE_ARM`
/// decides only the rare-arm RULE (`spliced_branch_edge_prunable`), so with it
/// at `0` the spliced feed keeps the "never taken" rows. Read per compile and
/// per resolved body.
pub fn ir_splice_never_taken_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_PRUNE_RARE_ARM` (JIT round 23 wave 9, lane W9-L2, CF-W9-2;
/// perf, default ON, `=0` restores "taken exactly 0 times"): the never-taken
/// prunes -- the compiling method's own (`ir_never_taken_branches`, nio21
/// wave 17) and a spliced body's (CF23-2, [`splice_never_taken_pcs`]) -- also
/// take a branch whose TAKEN edge the profile saw at most once in
/// [`RARE_ARM_RATIO`] executions, over at least
/// [`ir::MIN_OBSERVATIONS_TO_PRUNE`] ([`branch_edge_prunable`]).
///
/// Why: a handful of early executions keep an arm alive for the run.
/// `R21CfChain`'s facts prologue completes a few pending stages, so
/// `uniApplyStage`'s pending arm (`new UniApply(.., this, ..)`, which makes
/// both futures escape) had `taken > 0` on the done rows and was never pruned.
///
/// The prune is the existing one: a guard on the fall-through whose taken
/// edge deopts precisely (chain-framed in a splice, the method's own frame
/// otherwise) so the interpreter takes the branch. The de-spec registry
/// withdraws a pc whose guard keeps firing, and the next compile builds that
/// branch. In a spliced body it widens the rule only; whether the spliced
/// feed runs at all is `CRATONVM_JIT_IR_SPLICE_NEVER_TAKEN`'s
/// ([`ir_splice_prune_feed_enabled`], JIT round 25 review). Read per compile
/// and per resolved body.
pub fn ir_prune_rare_arm_enabled() -> bool {
    true
}

/// CF-W9-2: the taken share at or below which an arm is pruned: one in this
/// many executions (HotSpot's `PROB_MIN` is 1e-6; this is a little looser
/// because the profile here counts the interpreter and the first tier only).
pub const RARE_ARM_RATIO: u64 = 100_000;

/// CF-W9-2: is the spliced-body prune feed on? `CRATONVM_JIT_IR_SPLICE_NEVER_TAKEN`
/// (default ON, `=0` kills) alone since the JIT round 25 review; it was this
/// switch OR `CRATONVM_JIT_IR_PRUNE_RARE_ARM`, which left `=0` here no kill.
pub fn ir_splice_prune_feed_enabled() -> bool {
    ir_splice_never_taken_enabled()
}

/// CF-W9-2: may an arm reached by a branch edge seen `edge` times (against
/// `other` for the opposite edge) be pruned? Never seen over at least
/// [`ir::MIN_OBSERVATIONS_TO_PRUNE`] executions (the old rule, always); or,
/// under [`ir_prune_rare_arm_enabled`], seen at most once in
/// [`RARE_ARM_RATIO`] over at least that many.
pub fn branch_edge_prunable(edge: u32, other: u32) -> bool {
    if edge == 0 {
        return other >= ir::MIN_OBSERVATIONS_TO_PRUNE;
    }
    let total = u64::from(edge) + u64::from(other);
    ir_prune_rare_arm_enabled()
        && total >= u64::from(ir::MIN_OBSERVATIONS_TO_PRUNE)
        && u64::from(edge).saturating_mul(RARE_ARM_RATIO) <= total
}

/// CF23-2: the callee pcs of `code` (a spliced body's own bytecode) whose
/// taken edge `profile` (the CALLEE's method profile) never saw over at least
/// [`ir::MIN_OBSERVATIONS_TO_PRUNE`] executions, each a conditional forward
/// branch that exits no loop ([`never_taken_prune_candidate`], the caller's own
/// rule). Sorted. Empty with [`ir_splice_never_taken_enabled`] off.
pub fn splice_never_taken_pcs(
    profile: &profile::MethodProfile,
    code: &[u8],
    code_len: usize,
) -> Vec<usize> {
    splice_never_taken_pcs_frozen(profile, code, code_len, false)
}

/// [`splice_never_taken_pcs`] for a callee whose profile is FROZEN: it already
/// has a published compiled body, and compiled code records no branch
/// observations, so its counts will never grow past what the interpreter saw
/// (JIT r24, CF residual page; [`ir_splice_prune_frozen_floor_enabled`]).
pub fn splice_never_taken_pcs_frozen(
    profile: &profile::MethodProfile,
    code: &[u8],
    code_len: usize,
    frozen: bool,
) -> Vec<usize> {
    // CF-W9-2: either switch feeds the rows, and a rare arm qualifies too.
    if !ir_splice_prune_feed_enabled() {
        return Vec::new();
    }
    let frozen = frozen && ir_splice_prune_frozen_floor_enabled();
    let mut out: Vec<usize> = profile
        .branches
        .iter()
        .filter(|(_, c)| {
            spliced_branch_edge_prunable(c.taken, c.not_taken)
                || (frozen && c.taken == 0 && c.not_taken >= SPLICE_PRUNE_FROZEN_FLOOR)
        })
        .map(|(&pc, _)| pc)
        .filter(|&pc| never_taken_prune_candidate(code, code_len, pc))
        .collect();
    out.sort_unstable();
    out
}

/// [`splice_never_taken_pcs_frozen`]'s twin for branches whose FALL-THROUGH
/// edge the callee's own profile never saw (`c.not_taken == 0`).
pub fn splice_always_taken_pcs_frozen(
    profile: &profile::MethodProfile,
    code: &[u8],
    code_len: usize,
    frozen: bool,
) -> Vec<usize> {
    if !ir_splice_prune_feed_enabled() {
        return Vec::new();
    }
    let frozen = frozen && ir_splice_prune_frozen_floor_enabled();
    let mut out: Vec<usize> = profile
        .branches
        .iter()
        .filter(|(_, c)| {
            spliced_branch_edge_prunable(c.not_taken, c.taken)
                || (frozen && c.not_taken == 0 && c.taken >= SPLICE_PRUNE_FROZEN_FLOOR)
        })
        .map(|(&pc, _)| pc)
        .filter(|&pc| never_taken_prune_candidate(code, code_len, pc))
        .collect();
    out.sort_unstable();
    out
}

/// `CRATONVM_JIT_IR_SPLICE_PRUNE_LOW_FLOOR` (JIT round 23 wave 10, lane L2,
/// CF-W10-1; perf, **opt-in** `=1`, else [`ir::MIN_OBSERVATIONS_TO_PRUNE`]
/// for spliced bodies too; n23-w10a A/B, `R21CfChain` thenapply-done
/// `--real-jdk` 74 / 98 / 132 ns on against 73 / 77 / 158 off, mixed): a
/// SPLICED callee's branch may be pruned
/// ([`splice_never_taken_pcs`]) once the callee's own profile saw it
/// [`SPLICE_PRUNE_LOW_FLOOR`] times, not 2000.
///
/// Why: the caller's optimizing compile is requested when the CALLER's site
/// is hot, which is typically about 1000 executions -- `CfSeq.done` compiled at
/// 1013 (`ir-splice-profile-order ... 24:L:1013`), so `uniApplyStage`'s pending
/// arm, never taken on the done rows, was never pruned and both futures escaped
/// through `new UniApply(.., this, ..)` / `unipush`. Waiting for a recompile
/// past 2000 would cost a second compile per caller and keep the slow body for
/// the window between; lowering the floor for spliced arms only keeps the
/// method's own feed (`ir_never_taken_branches`) unchanged.
///
/// Sound for the same reason the 2000 floor is: a pruned arm is a precise
/// guard that deopts into the callee's frame and the interpreter takes the
/// branch; the de-spec registry withdraws a guard that keeps firing and the
/// next compile builds the arm. A lower floor only makes a wrong guess more
/// likely, which the registry bounds. **Default ON** since R23-W11-FLIP (`=0` keeps
/// the 2000 floor), with the CF family; see `ir::ir_splice_athrow_exit_enabled`
/// for the n23-w11b A/B. Read per resolved body.
pub fn ir_splice_prune_low_floor_enabled() -> bool {
    true
}

/// CF-W10-1: the observation floor of a spliced callee's prune rows under
/// [`ir_splice_prune_low_floor_enabled`].
pub const SPLICE_PRUNE_LOW_FLOOR: u32 = 256;

/// CF-W10-1: [`branch_edge_prunable`] for a SPLICED callee's branch: the same
/// rule, at [`SPLICE_PRUNE_LOW_FLOOR`] observations instead of
/// [`ir::MIN_OBSERVATIONS_TO_PRUNE`] when the switch is on. With the floor
/// that low only `edge == 0` can qualify (one take in 256 is far above one in
/// [`RARE_ARM_RATIO`]).
pub fn spliced_branch_edge_prunable(edge: u32, other: u32) -> bool {
    if !ir_splice_prune_low_floor_enabled() {
        return branch_edge_prunable(edge, other);
    }
    if edge == 0 {
        return other >= SPLICE_PRUNE_LOW_FLOOR;
    }
    let total = u64::from(edge) + u64::from(other);
    ir_prune_rare_arm_enabled()
        && total >= u64::from(SPLICE_PRUNE_LOW_FLOOR)
        && u64::from(edge).saturating_mul(RARE_ARM_RATIO) <= total
}

/// `CRATONVM_JIT_IR_SPLICE_PRUNE_FROZEN_FLOOR` (JIT r24, CompletableFuture
/// residual page; perf, default ON, `=0` keeps [`SPLICE_PRUNE_LOW_FLOOR`] for
/// every callee): a spliced callee whose profile is frozen (it has a published
/// compiled body, and only the interpreter records branches) may have a
/// never-taken arm pruned at [`SPLICE_PRUNE_FROZEN_FLOOR`] observations
/// ([`splice_never_taken_pcs_frozen`]).
///
/// Why: `uniApplyStage`'s pending arm on `CfSeq.done` was pruned or not by a
/// race. Its C1 body installs after about 200-370 interpreted calls, and the
/// count the prune reads stops there: 217 in a slow run (no prune, both
/// futures escape, 57 ns/op), 257-374 in fast ones (19 ns/op). A frozen count
/// never reaches the 256 floor however long the caller waits, so the floor
/// only made the outcome depend on when the background compile landed. The
/// arm is a precise guard either way, and the de-spec registry withdraws one
/// that keeps firing. Read per resolved body.
pub fn ir_splice_prune_frozen_floor_enabled() -> bool {
    true
}

/// [`ir_splice_prune_frozen_floor_enabled`]'s floor. Taken-0 rows only.
///
/// A frozen profile never grows, so waiting buys nothing: the verdict is a
/// guess whatever the floor, and a floor above what the window held makes the
/// verdict depend on when the callee's C1 body landed. rx27 bimodal
/// (`matchonly`): `BmpCharPropertyGreedy.match`'s backoff branch (pc 73) is
/// reached by ~16 % of the calls in every process, but the interpreted window
/// ends after 60 to 3 700 calls, i.e. 10 to 600 observations. At the old 100
/// the 37-, 55- and 10-observation processes kept the arm (28.1-29.3k
/// instructions per op); every longer window pruned it (25.4k). 8 is where
/// eight one-sided observations of an arm taken half the time are a 1-in-256
/// event; a wrong guess is a precise guard that the de-spec registry withdraws.
pub const SPLICE_PRUNE_FROZEN_FLOOR: u32 = 8;

/// `CRATONVM_JIT_IR_SPLICE_COLD_NESTED_HOT_PATH` (JIT round 23 wave 10, lane
/// L2, CF-W10-2; perf, **opt-in** `=1`, else the static rule alone; n23-w10a
/// A/B, `R21CfChain` thenapply-done 74 / 98 / 132 ns on against 82 / 92 / 72
/// off, mixed): the
/// cold-nested rule ([`ir_splice_prune_cold_nested`]) keeps spliced a nested
/// body whose call lies on its enclosing callee's profiled DOMINANT path
/// ([`splice_hot_path_pcs`], filled by the VM into
/// [`InlineSite::callee_hot_path_pcs`]).
///
/// Why: the rule's stand-in for call-site frequency is "the enclosing body
/// always reaches the call" (its entry prefix). `uniApplyStage` reaches
/// `uniApplyNow` only after `if ((r = result) == null)`, so on every done row
/// -- where that branch is never taken -- the hottest call of the stage was
/// kept as a call (`ir-splice-cold-nested ... kept as calls`), and its
/// arguments made both futures escape. The callee's own profile says which
/// way that branch goes. **Default ON** since R23-W11-FLIP (`=0`, the static rule
/// alone), with the CF family; see `ir::ir_splice_athrow_exit_enabled` for the
/// n23-w11b A/B. Read per resolved body.
pub fn ir_splice_cold_nested_hot_path_enabled() -> bool {
    true
}

/// CF-W10-2: the share (in eighths) one edge of a profiled conditional branch
/// must hold for [`splice_hot_path_pcs`] to follow it.
pub(crate) const HOT_PATH_SHARE_EIGHTHS: u64 = 7;

/// CF-W10-2: the instruction pcs of `code` (a callee's own bytecode) on its
/// profiled dominant path: from pc 0, straight-line code and forward `goto`s,
/// and at each conditional forward branch the edge `profile` (the CALLEE's
/// method profile) saw at least 7 times in 8 over at least
/// [`profile::BRANCH_BIAS_MIN_SAMPLES`] observations. The walk stops at a
/// return, `athrow`, a switch, a backward edge, a branch the profile does not
/// decide, or an instruction it cannot step over. Sorted. Empty with
/// [`ir_splice_cold_nested_hot_path_enabled`] off.
pub fn splice_hot_path_pcs(
    profile: &profile::MethodProfile,
    code: &[u8],
    code_len: usize,
) -> Vec<usize> {
    if !ir_splice_cold_nested_hot_path_enabled() {
        return Vec::new();
    }
    let len = code_len.min(code.len());
    let mut out: Vec<usize> = Vec::new();
    let mut pc = 0usize;
    // A forward-only walk visits each pc at most once.
    while pc < len {
        out.push(pc);
        let op = code[pc];
        match op {
            // ireturn..return, athrow, tableswitch, lookupswitch, jsr, ret,
            // jsr_w, goto_w (a wide goto is rare enough not to follow).
            0xac..=0xb1 | 0xbf | 0xaa | 0xab | 0xa8 | 0xa9 | 0xc8 | 0xc9 => break,
            // goto: follow it forward.
            0xa7 => match bytecode_analysis::offset_branch_target(code, pc) {
                Some(t) if t > pc && t < len => pc = t,
                _ => break,
            },
            // The conditional branches.
            0x99..=0xa6 | 0xc6 | 0xc7 => {
                let Some(c) = profile.branches.get(&pc) else {
                    break;
                };
                let total = u64::from(c.taken) + u64::from(c.not_taken);
                if total < u64::from(profile::BRANCH_BIAS_MIN_SAMPLES) {
                    break;
                }
                if u64::from(c.taken) * 8 >= HOT_PATH_SHARE_EIGHTHS * total {
                    match bytecode_analysis::offset_branch_target(code, pc) {
                        Some(t) if t > pc && t < len => pc = t,
                        _ => break,
                    }
                } else if u64::from(c.not_taken) * 8 >= HOT_PATH_SHARE_EIGHTHS * total {
                    pc += bytecode_analysis::step(code, pc).max(1);
                } else {
                    break;
                }
            }
            _ => {
                let step = bytecode_analysis::step(code, pc);
                if step == 0 {
                    break;
                }
                pc += step;
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// CF23-2: the combined pcs of `rows` (`(combined branch pc, callee key,
/// callee branch pc)`, `ir::IrInlineTables::spliced_never_taken`) the builder
/// may prune: those inside a planned site's spliced range (`ranges`, `(start,
/// end, outermost call-site pc)`) whose guard `despec` has not withdrawn at
/// that call site ([`deopt::chain_predicate_withdrawn`] under
/// [`deopt::PredicateKind::UnstableIf`]). A row in no range is dropped.
pub(crate) fn splice_pruned_switch_edges_admitted(
    rows: &[(usize, usize, String, u32)],
    ranges: &[(usize, usize, usize)],
    despec: Option<&deopt::DespecRegistry>,
    method_key: &str,
) -> Vec<(usize, usize)> {
    rows.iter()
        .filter_map(|(sw_pc, target_pc, key, bci)| {
            let &(_, _, site_pc) = ranges.iter().find(|&&(s, e, _)| s <= *sw_pc && *sw_pc < e)?;
            let site_bci = u32::try_from(site_pc).ok()?;
            let withdrawn = despec.is_some_and(|d| {
                deopt::chain_predicate_withdrawn(
                    d,
                    method_key,
                    site_bci,
                    key,
                    *bci,
                    deopt::PredicateKind::UnstableIf,
                )
            });
            (!withdrawn).then_some((*sw_pc, *target_pc))
        })
        .collect()
}

pub fn splice_pruned_switch_edges_frozen(
    profile: &profile::MethodProfile,
    code: &[u8],
    code_len: usize,
    frozen: bool,
) -> Vec<(usize, usize)> {
    if !ir_splice_prune_feed_enabled() {
        return Vec::new();
    }
    let len = code_len.min(code.len());
    let floor = if frozen && ir_splice_prune_frozen_floor_enabled() {
        SPLICE_PRUNE_FROZEN_FLOOR
    } else {
        100
    };
    if !profile
        .branches
        .values()
        .any(|c| c.taken + c.not_taken >= floor)
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < len {
        let step = bytecode_analysis::step(code, pc);
        if step == 0 {
            break;
        }
        if matches!(code[pc], 0xaa | 0xab) {
            if let Some(table) = bytecode_analysis::switch_table(code, len, pc) {
                let mut seen_targets = std::collections::HashSet::new();
                for &(_, target) in &table.cases {
                    if target == table.default || !seen_targets.insert(target) {
                        continue;
                    }
                    let mut cur = target;
                    let mut steps = 0usize;
                    let mut gotos = 0usize;
                    let mut is_cold = false;
                    while cur < len && steps < 16 {
                        let op = code[cur];
                        if never_taken_prune_candidate(code, len, cur) {
                            if profile.branches.get(&cur).is_none() {
                                is_cold = true;
                            }
                            break;
                        }
                        if op == 0xa7 && gotos < 2 {
                            if let Some(next) = bytecode_analysis::offset_branch_target(code, cur) {
                                cur = next;
                                gotos += 1;
                                steps += 1;
                                continue;
                            }
                            break;
                        }
                        if matches!(op, 0xaa | 0xab | 0xac..=0xb1 | 0xbf | 0xc8 | 0xa8 | 0xa9) {
                            break;
                        }
                        let st = bytecode_analysis::step(code, cur);
                        if st == 0 {
                            break;
                        }
                        cur += st;
                        steps += 1;
                    }
                    if is_cold {
                        out.push((pc, target));
                    }
                }
            }
        }
        pc += step;
    }
    out
}

pub(crate) fn splice_never_taken_admitted(
    rows: &[(usize, String, u32)],
    ranges: &[(usize, usize, usize)],
    despec: Option<&deopt::DespecRegistry>,
    method_key: &str,
) -> std::collections::HashSet<usize> {
    rows.iter()
        .filter_map(|(cpc, key, bci)| {
            let &(_, _, site_pc) = ranges.iter().find(|&&(s, e, _)| s <= *cpc && *cpc < e)?;
            let site_bci = u32::try_from(site_pc).ok()?;
            let withdrawn = despec.is_some_and(|d| {
                deopt::chain_predicate_withdrawn(
                    d,
                    method_key,
                    site_bci,
                    key,
                    *bci,
                    deopt::PredicateKind::UnstableIf,
                )
            });
            (!withdrawn).then_some(*cpc)
        })
        .collect()
}

/// CF23-2: the pcs of `code` that normal flow reaches only through the TAKEN
/// edges of `branches` (callee pcs), i.e. reachable from pc 0 with every edge
/// but not without those -- the code a pruned branch leaves unbuilt. Empty
/// when the body's flow is not statically known (`jsr`/`ret`, a malformed
/// encoding): the builder then refuses at such a pc rather than step over it.
pub(crate) fn splice_never_taken_dead_pcs(
    code: &[u8],
    code_len: usize,
    branches: &[usize],
) -> Vec<usize> {
    let removed: Vec<(usize, usize)> = branches
        .iter()
        .filter_map(|&pc| bytecode_analysis::offset_branch_target(code, pc).map(|t| (pc, t)))
        .collect();
    if removed.is_empty() {
        return Vec::new();
    }
    let Some(live) = bytecode_analysis::reachable_pcs_without(code, code_len, &removed) else {
        return Vec::new();
    };
    (0..code_len)
        .filter(|&pc| !live.get(pc).copied().unwrap_or(false))
        .collect()
}

pub(crate) fn splice_always_taken_dead_pcs(
    code: &[u8],
    code_len: usize,
    branches: &[usize],
) -> Vec<usize> {
    let removed: Vec<(usize, usize)> = branches
        .iter()
        .filter_map(|&pc| {
            let step = bytecode_analysis::step(code, pc);
            (step > 0 && pc + step < code_len).then_some((pc, pc + step))
        })
        .collect();
    if removed.is_empty() {
        return Vec::new();
    }
    let Some(live) = bytecode_analysis::reachable_pcs_without(code, code_len, &removed) else {
        return Vec::new();
    };
    (0..code_len)
        .filter(|&pc| !live.get(pc).copied().unwrap_or(false))
        .collect()
}

pub fn bytecode_insn_step(code: &[u8], pc: usize) -> usize {
    bytecode_analysis::step(code, pc).max(1)
}

pub fn splice_live_pcs_without_pruned(
    code: &[u8],
    code_len: usize,
    always_taken: &[usize],
    never_taken: &[usize],
) -> Option<(Vec<bool>, usize)> {
    splice_live_pcs_without_pruned_and_switch(code, code_len, always_taken, never_taken, &[])
}

pub fn splice_live_pcs_without_pruned_and_switch(
    code: &[u8],
    code_len: usize,
    always_taken: &[usize],
    never_taken: &[usize],
    pruned_switch_edges: &[(usize, usize)],
) -> Option<(Vec<bool>, usize)> {
    let mut removed: Vec<(usize, usize)> = Vec::new();
    for &pc in always_taken {
        let s = bytecode_analysis::step(code, pc);
        if s > 0 && pc + s < code_len {
            removed.push((pc, pc + s));
        }
    }
    for &pc in never_taken {
        if let Some(t) = bytecode_analysis::offset_branch_target(code, pc) {
            removed.push((pc, t));
        }
    }
    removed.extend_from_slice(pruned_switch_edges);
    if removed.is_empty() {
        return None;
    }
    let live = bytecode_analysis::reachable_pcs_without(code, code_len, &removed)?;
    let mut live_bytes = 0usize;
    let mut pc = 0usize;
    while pc < code_len {
        let s = bytecode_analysis::step(code, pc).max(1);
        if live.get(pc).copied().unwrap_or(false) {
            live_bytes += s;
        }
        pc += s;
    }
    Some((live, live_bytes))
}

/// JIT round 23 wave 8 (lane W8-L2, CF-W8-3;
/// [`ir_evidence::cold_blind_dispatch_weight_enabled`]): the callee-pc ranges
/// `(start, end, permille)` of `code` (a spliced body's own bytecode) that only
/// the taken edge of a RARELY taken branch reaches, by `profile` (the
/// CALLEE's method profile). A branch qualifies with at least
/// [`ir::MIN_OBSERVATIONS_TO_PRUNE`] observations, a taken share of at most
/// 1/8, and the prune candidate shape ([`never_taken_prune_candidate`]: a
/// conditional forward branch that exits no loop). Its weight is the taken
/// share in permille, floored at
/// [`ir_evidence::COLD_ARM_WEIGHT_FLOOR_PERMILLE`]. Used only to price an
/// in-splice blind dispatch; the builder never reads it. `R21CfChain`'s
/// `uniApplyStage` pending arm (`new UniApply(..)`), taken a handful of times
/// in the facts prologue and never on the done rows, refused the done loop's
/// optimizing body at 175 ns per execution.
pub fn splice_cold_arm_ranges(
    profile: &profile::MethodProfile,
    code: &[u8],
    code_len: usize,
) -> Vec<(usize, usize, u16)> {
    if !ir_evidence::cold_blind_dispatch_weight_enabled() {
        return Vec::new();
    }
    let mut arms: Vec<(usize, u16)> = profile
        .branches
        .iter()
        .filter_map(|(&pc, c)| cold_arm_weight(c.taken, c.not_taken).map(|w| (pc, w)))
        .filter(|&(pc, _)| never_taken_prune_candidate(code, code_len, pc))
        .collect();
    arms.sort_unstable();
    let mut out: Vec<(usize, usize, u16)> = Vec::new();
    for &(pc, w) in &arms {
        out.extend(
            taken_edge_only_ranges(code, code_len, pc)
                .into_iter()
                .map(|(s, e)| (s, e, w)),
        );
    }
    // 2026-11-16: the rare edges together -- taken edges as above and rare
    // FALL-THROUGH edges (a `throw` arm after an always-taken check) -- for code
    // no single arm holds, or that another rare path also reaches. `AccessibleObject.verifyAccess` reaches
    // `slowVerifyAccess` from the access-cache miss and from the protected-member
    // arm, which a public member never enters, so the call was charged in full
    // and refused a reflective loop's optimizing body. Priced at the sum of the
    // removed edges' weights, an upper bound on that code's share.
    let mut removed: Vec<(usize, usize)> = Vec::new();
    let mut sum = 0u32;
    for (&pc, c) in &profile.branches {
        if !never_taken_prune_candidate(code, code_len, pc) {
            continue;
        }
        let Some(target) = bytecode_analysis::offset_branch_target(code, pc) else {
            continue;
        };
        // A candidate is a 3-byte conditional branch: its fall-through is pc + 3.
        let (edge, w) = match (
            cold_arm_weight(c.taken, c.not_taken),
            cold_arm_weight(c.not_taken, c.taken),
        ) {
            (Some(w), _) => ((pc, target), w),
            (None, Some(w)) => ((pc, pc + 3), w),
            (None, None) => continue,
        };
        removed.push(edge);
        sum = sum.saturating_add(u32::from(w));
    }
    if !removed.is_empty() {
        let w = u16::try_from(sum.min(1000)).unwrap_or(1000);
        let covered = |pc: usize| out.iter().any(|&(s, e, _)| s <= pc && pc < e);
        let extra = dead_runs(code, code_len, &removed, &covered);
        out.extend(extra.into_iter().map(|(s, e)| (s, e, w)));
    }
    out
}

/// CF-W8-3: the weight (permille) of a branch's taken arm when the arm is
/// rare (`taken` at most 1/8 of at least [`ir::MIN_OBSERVATIONS_TO_PRUNE`]
/// observations), floored; `None` for an arm that is not rare.
pub(crate) fn cold_arm_weight(taken: u32, not_taken: u32) -> Option<u16> {
    let total = u64::from(taken) + u64::from(not_taken);
    if total < u64::from(ir::MIN_OBSERVATIONS_TO_PRUNE) || u64::from(taken) * 8 > total {
        return None;
    }
    let w = (u64::from(taken) * 1000 / total)
        .max(u64::from(ir_evidence::COLD_ARM_WEIGHT_FLOOR_PERMILLE));
    u16::try_from(w).ok()
}

/// CF-W8-3: the maximal runs `[start, end)` of instruction starts that normal
/// flow reaches from pc 0 only through the taken edge of `branch_pc`. Empty
/// when the flow is not statically known or `branch_pc` is no offset branch.
pub(crate) fn taken_edge_only_ranges(code: &[u8], code_len: usize, branch_pc: usize) -> Vec<(usize, usize)> {
    let Some(target) = bytecode_analysis::offset_branch_target(code, branch_pc) else {
        return Vec::new();
    };
    dead_runs(code, code_len, &[(branch_pc, target)], &|_| false)
}

/// CF-W8-3: the maximal runs `[start, end)` of instruction starts that normal
/// flow reaches from pc 0 only through the `removed` edges, leaving out the
/// pcs `skip` names. Empty when the flow is not statically known.
fn dead_runs(
    code: &[u8],
    code_len: usize,
    removed: &[(usize, usize)],
    skip: &dyn Fn(usize) -> bool,
) -> Vec<(usize, usize)> {
    let (Some(all), Some(live)) = (
        bytecode_analysis::reachable_pcs(code, code_len, &[]),
        bytecode_analysis::reachable_pcs_without(code, code_len, removed),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for pc in 0..code_len {
        if !all.get(pc).copied().unwrap_or(false) {
            continue;
        }
        let dead = !live.get(pc).copied().unwrap_or(false) && !skip(pc);
        match (dead, start) {
            (true, None) => start = Some(pc),
            (false, Some(s)) => {
                out.push((s, pc));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, code_len));
    }
    out
}

pub(crate) fn never_taken_prune_candidate(code: &[u8], code_len: usize, pc: usize) -> bool {
    let code_len = code_len.min(code.len());
    let Some(&op) = code.get(pc) else {
        return false;
    };
    // Conditional branches only: `goto` / `goto_w` / `jsr` have no not-taken
    // edge to continue on.
    if !matches!(op, 0x99..=0xa6 | 0xc6 | 0xc7) {
        return false;
    }
    if !speculatable_forward_branch(code, code_len, pc) {
        return false;
    }
    let Some(target) = bytecode_analysis::offset_branch_target(code, pc) else {
        return false;
    };
    // A branch to its own fall-through has no separate taken edge.
    if target <= pc + 3 {
        return false;
    }
    !bytecode_analysis::back_edges(code, code_len)
        .iter()
        .any(|&(t, s)| t <= pc && pc < s && target > s)
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
/// The resolver-side screen for `String.indexOf(int)` (round 9 wave 2).
///
/// `true` for every entry other than [`JitIntrinsic::StringIndexOfChar`]; for
/// that one, `true` only when the needle is PROVABLY in `0..=0xFFFF` — the
/// range on which the inline single-code-unit scan and the JDK's code-point
/// search are the same function — and `pc` is not an explicit branch target
/// (a join can deliver a different, variable needle than the fall-through
/// path the linear walk looked at). Proof is the instruction linearly before
/// `pc`:
///
/// * a constant push (`iconst_*`/`bipush`/`sipush`) in range — the backend's
///   own `prev_insn_int_const` screen;
/// * `i2c` (`0x92`) or `caload` (`0x34`), whose result is a zero-extended
///   `char` by the JVMS, so every value they can produce is in range. This is
///   the `s.indexOf(chars[i])` / `s.indexOf((char) x)` shape, which would
///   otherwise lose the intrinsic now that the screen declines.
///
/// A `false` leaves the site unregistered, so it gets ordinary dispatch
/// metadata (`invoke_info`) and its MIC like any other invoke.
pub(crate) fn string_index_of_char_site_admitted(
    entry: usize,
    code: &[u8],
    code_len: usize,
    pc: usize,
) -> bool {
    if entry != JitIntrinsic::StringIndexOfChar.as_entry() {
        return true;
    }
    let prev_const = crate::x64::prev_insn_int_const_for_test(code, code_len, pc);
    // JIT r22 wave 2, lane intrinsics (INTRINSICS21-2,
    // `CRATONVM_JIT_SP_STRING_INDEXOF_CHAR`, opt-in): a VARIABLE needle is
    // admitted too. The arm's own run-time screen (emitted whenever the
    // previous instruction is not a constant in range) sends a needle outside
    // `0..=0xFFFF` to its own inline answer (wave 3: the JDK body's -1 / pair
    // search; before, the same deopt as a null receiver), so the inline
    // single-unit scan only ever answers the BMP range. A constant OUTSIDE the
    // range would deopt on every call and stays unregistered, and the
    // branch-target rule below still holds: it is what keeps an in-range
    // constant on the fall-through path from switching the screen off for a
    // different needle arriving by a jump.
    let variable_ok = crate::sp_string_index_of_char_enabled()
        && !matches!(prev_const, Some(c) if !(0..=0xFFFF).contains(&c));
    let needle_in_bmp = variable_ok
        || matches!(prev_const, Some(0..=0xFFFF))
        || matches!(prev_insn_opcode(code, code_len, pc), Some(0x92 | 0x34));
    if !needle_in_bmp {
        return false;
    }
    !crate::bytecode_analysis::branch_target_map(code, code_len)
        .get(pc)
        .copied()
        .unwrap_or(true)
}

/// The opcode of the instruction that linearly precedes `pc`, or `None` when
/// `pc` is 0, out of range, or not an instruction boundary on the linear
/// decode. Same walk as `x64::bytecode_walk::prev_insn_int_const`.
pub(crate) fn prev_insn_opcode(code: &[u8], code_len: usize, pc: usize) -> Option<u8> {
    if pc == 0 || pc > code_len {
        return None;
    }
    let mut cur = 0usize;
    let mut prev: Option<usize> = None;
    while cur < pc {
        let next = cur + crate::bytecode_analysis::step(code, cur);
        if next > pc {
            return None;
        }
        prev = Some(cur);
        cur = next;
    }
    code.get(prev?).copied()
}

pub(crate) fn try_compile_inner(
    req: &CompileRequest<'_>,
    backend_attempted: &mut bool,
    self_call_identity_stable: bool,
    admission: &compile_gate::CompileAdmission,
    // The door's local-handler retry: `true` compiles with handler bodies
    // suppressed. Replaces the `disarm_local_handlers_once` thread-local.
    local_handlers_disarmed: bool,
) -> Option<CompiledMethod> {
    let CompileRequest {
        branch_counter_resolver: _,
        cached,
        cp_class_name_resolver,
        cp_field_resolver,
        cp_static_field_resolver,
        // Read straight from `req` by `ir_tier_attempt`'s static-field feed
        // (round 17 wave 2, lane irfeed).
        cp_static_field_final_resolver: _,
        // Read straight from `req` by `ir_tier_attempt`'s array-length feed
        // (JIT r20 wave 11, lane pIR).
        static_final_array_length_resolver: _,
        // Read straight from `req` by `ir_tier_attempt`'s exact-class feed.
        static_final_ref_class_resolver: _,
        frozen_facts: _,
        frozen_state: _,
        frozen_receivers: _,
        // Read straight from `req` by `ir_tier_attempt`'s throw-edge feed.
        exception_event_flag_addrs: _,
        // Read by the two stages that build field tables (`ir_tier`,
        // `build_single_pass_tables`) straight from `req`.
        cp_field_volatile_resolver: _,
        // nio21 wave 2 (lane L3): read straight from `req` by `ir_tier`'s
        // field feed.
        cp_field_final_resolver: _,
        // JIT round 20 (lane memory): read straight from `req` by `ir_tier`'s
        // field feed.
        cp_field_byte_array_resolver: _,
        // Wave 92 (lane L2): read straight from `req` by
        // `build_single_pass_tables`.
        cp_field_deferred_resolver: _,
        // Read straight from `req` by `resolve_indy_site` and the IR tier's
        // indy-direct plan (round 16 wave 10, lane indyir).
        cp_invokedynamic_site_resolver: _,
        cp_indy_direct_resolver: _,
        // Read straight from `req` by the IR tier's guarded mutable-site plan
        // (round 20 wave 11, lane pIndy).
        cp_indy_mutable_guard_resolver: _,
        // Read straight from `req` by the IR tier's record-fold plan (round 21
        // wave 17, lane records).
        cp_indy_record_fold_resolver: _,
        // Read straight from `req` by the IR tier's invoke and inline plans at
        // an indy-planned site (round 21, lane indylate2).
        indy_context_callee_compiler: _,
        indy_context_ir_inline_resolver: _,
        // Read straight from `req` by the IR tier's lambda-word plan (round 20
        // wave 11, lane pIndy).
        indy_constant_word_resolver: _,
        // Read straight from `req` by the IR tier's capture-new plan (round 20
        // wave 12, lane qLamNew).
        indy_capture_alloc_resolver: _,
        // Read straight from `req` by the IR tier's late-lambda bridge plan
        // (t1 wave 11, lane JLAM).
        indy_constant_lambda_site_resolver: _,
        // Read straight from `req` by the IR tier's lambda impl-call plan
        // (round 21, lane lambda2).
        indy_lambda_impl_resolver: _,
        // Read straight from `req` by the IR tier's StringBuilder-chain plan
        // and the String pin (round 21 wave 20, lane sbfuse).
        sb_concat_site_resolver: _,
        // Read straight from `req` by the IR tier's constant-handle plan
        // (round 16 wave 17, lane mh7).
        cp_mh_constant_resolver: _,
        // The same plan's bound-handle rows (round 21 wave 19, lane mhbind2).
        cp_mh_constant_bound_resolver: _,
        // And its filter species' second calls (round 21 wave 20, lane mhfilter).
        cp_mh_constant_filter_resolver: _,
        cp_mh_constant_convert_resolver: _,
        // Round 15 wave 3 (lane chain3, CHN15-2 producer): read by
        // `ir_tier_attempt` straight from `req`.
        cp_field_declaring_class_resolver: _,
        cp_invoke_resolver,
        // Read by both tiers' planners straight from `req` (`first_call_cells_of`).
        cp_invoke_first_call: _,
        cp_invokespecial_owner_resolver,
        callee_compiler,
        cp_new_resolver,
        cp_ldc_resolver,
        cp_ldc2w_resolver,
        profile,
        helpers,
        inline_resolver,
        string_layout_resolver,
        cp_invoke_class_id_resolver,
        cp_elidable_init_resolver,
        optimize,
        ir_emit_calls,
        ir_emit_special_calls,
        ir_emit_long,
        ir_emit_virtual_calls,
        ir_emit_fp,
        cp_invokedynamic_descriptor_resolver,
        class_id_name_resolver,
        receiver_inline_resolver,
        unique_concrete_resolver: _,
        ir_inline_resolver,
        // Read by `ir_tier`'s inline planner straight from `req`.
        ir_exact_receiver_resolver: _,
        // Read by `ir_tier`'s inline planner straight from `req`.
        ir_receiver_inline_resolver: _,
        jdk_only,
        direct_bind_admission,
        despec,
        runtime_despec: _,
        cp_invoke_declaring_class_resolver,
        // Read by the two bind ladders (`ir_tier`, `build_single_pass_tables`)
        // straight from `req`.
        cp_invoke_declaring_class_id_resolver: _,
        self_call_identity_stable: _,
        direct_helpers,
        // Read by the single-pass planner straight from `req`.
        cycle_edge_admission: _,
        // Read by the single-pass type-check table straight from `req`.
        class_is_final_resolver: _,
        // Read by `ir_tier_attempt`'s local-throw-row block straight from `req`
        // (JIT r20 wave 11, lane pIR).
        class_is_subclass_resolver: _,
        // Read by the optimizing tier's invoke planner straight from `req`.
        sync_direct_lookup: _,
        // Read by `ir_tier_attempt`'s ldc loop straight from `req` (I7-4).
        cp_ldc_string_hash_resolver: _,
        // Read by `ir_tier_attempt`'s ldc loop straight from `req` (C14W3-4).
        cp_ldc_string_utf16_resolver: _,
        // Read by `ir_tier_attempt`'s ldc loop straight from `req` (JIT r20
        // wave 12, lane qMisc, NIO20-L3-1).
        getclass_exact_fold_resolver: _,
        // Read by both planners' inline-cache allocation straight from `req`
        // (JIT round 21 wave 4, lane ic, IC21-2).
        mega_site_binder: _,
        // Read by the optimizing tier's deferred-`new` recorder from `req`.
        deferred_new_retries: _,
        // Read through `req.verdicts()` by the planners that ask it.
        verdicts: _,
        // Read by `ir_tier` straight from `req`.
        osr_mode_exit_polls: _,
        // Read by `try_compile_request`'s bail-list decision from `req`.
        osr_optimizing_route: _,
        // Read by `ir_tier_attempt` straight from `req`.
        osr_requested_bci: _,
        // Read by `ir_tier` and `build_single_pass_tables` straight from `req`.
        debugger_observes_locals: _,
        // Read by `ir_tier`'s lowering scope straight from `req` (JIT round 21
        // wave 24, lane flipprep).
        jvmti_locals_accessible: _,
        // Read by `ir_tier_attempt`'s escape-analysis rounds and by the
        // single-pass `BackendRequest` builder straight from `req` (t1 wave 2,
        // lane M).
        monitor_observer_at_compile: _,
        // The same readers, for scalar replacement (t1 wave 3, lane M).
        scalar_observer_at_compile: _,
        // Read by `ir_tier_attempt`'s local-throw-row gate straight from `req`.
        exception_events_armed_at_compile: _,
        // Read by the spliced-callee-handler gate straight from `req` (JIT
        // round 23 wave 4).
        exception_listener_armed_at_compile: _,
        // Read by the self-lock admission (`single_pass_self_lock_preferred`
        // below, `build_single_pass_tables`) straight from `req`.
        self_lock_mirror_slot: _,
        // Read by `ir_tier_attempt` straight from `req`.
        static_varhandle_resolver: _,
        static_final_prim_resolver: _,
        // Read by `ir_tier_attempt`'s `new` loop straight from `req` (EAGER19-1).
        cp_uninit_new_trap_resolver: _,
        // Likewise (EAGER2-3).
        cp_uninit_new_guard_resolver: _,
        // Read by `ir_tier_attempt`'s field-site feed straight from `req`
        // (interpreter round i4 wave 94, lane L2).
        cp_field_trap_resolver: _,
        // Scoped by `try_compile_request` itself (t1 wave 3, lane Y).
        jdk_only_sinks: _,
        // Read by `ir_tier_attempt` straight from `req` (t1 wave 10, LCAP2).
        installed_body_needs_context: _,
        caller_mirror_slot: _,
        caller_security_walk_ignored: _,
        const_folds: _,
        const_folds_refresher: _,
        object_class_id: _,
        class_is_cha_leaf_resolver: _,
    } = *req;
    // One compile, one verdict: the fall-through signal describes THIS call.
    reset_ir_fall_through_signal();
    // H20-1 N3, completed 2026-09-22 with `H12-1` O1/O2. This door has always
    // ASKED the policy -- every bind in its two ladders goes through
    // `direct_native_helper`, which is what the other two doors did not do --
    // but it never SAID so on the token, so `undeclared_direct_bind_rows`
    // charged all seven of its rows to "never asked". The counter could not
    // tell the one door that was right from the two that were not. Saying it
    // here is what makes a non-zero reading mean what the counter's doc says
    // it means.
    admission.declare_direct_call_policy(compile_gate::DirectCallPolicy::from_jdk_only(jdk_only));
    // C2-review P0 "Measure compilation quality": one structured
    // `metrics::CompilationReport` per compilation, published when this handle
    // drops. `Drop` is deliberate — this function has ~40 `return None` exits
    // (every `jitc_bail!`, the scan reject, the `?` on the single-pass
    // backend), and publishing from the destructor covers all of them without
    // touching a single control-flow edge. Off unless `CRATONVM_JIT_METRICS=1`,
    // in which case `begin` is one relaxed atomic load and no allocation.
    let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::Compilation);
    let metrics = metrics::CompileRecorder::begin(
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        optimize,
    );

    // Architecture-specific backend selection. On aarch64 the block below
    // compiles with `aarch64_backend` and returns, bypassing the IR pipeline
    // and the x64 backend, and only when `CRATONVM_JIT_ARM64` is set. That
    // backend lowers leaf arithmetic, locals, conversions, compares, branches
    // and switches; it refuses every invoke, field, array, allocation, monitor
    // and exception opcode (see its module header).
    //
    // Total incoming argument SLOTS for this method's own prologue.
    //
    // `count_param_slots` counts only the *declared* descriptor
    // parameters. An instance method additionally receives the implicit
    // `this` reference as JVM local 0, ahead of the declared params, so
    // the prologue must load `1 + declared` argument registers. Omitting
    // the `this` slot made the prologue zero-initialize local 0 instead
    // of loading the receiver — a JIT-compiled instance method then
    // operated on a null `this` (constructor field writes silently lost;
    // boxed values / `String` fields read back as 0). The early-compile
    // path already gets this right because it derives the count from the
    // live `args.len()`; the callee-compile path used here did not.
    let prologue_param_slots: usize =
        count_param_slots(&cached.method_descriptor) + if cached.is_static { 0 } else { 1 };

    #[cfg(target_arch = "aarch64")]
    {
        use std::collections::HashMap;

        // OFF unless `CRATONVM_JIT_ARM64` is set, so "the JIT is disabled off
        // x86-64" stays true until the backend has run on hardware. Marked as
        // attempted so the method is permanently bail-listed: the switch is a
        // process snapshot and a retry cannot change the answer.
        if !aarch64_backend::arm64_jit_enabled() {
            *backend_attempted = true;
            return None;
        }

        let code = &cached.code;
        let num_params = prologue_param_slots;

        // Build method_info map: scan bytecode for invokestatic operands,
        // resolve each CP index to an argument count via the invoke resolver.
        //
        // PERF (jit-lib-perf): this runs on the hot JIT compile path. The
        // `aarch64` 0xb8 handler still consumes `method_info` to recover each
        // invokestatic's argument count (see `aarch64_backend::compile_method`),
        // so the scan cannot be elided — but the *redundant* work inside it can.
        // A hot method commonly calls the same static target many times (loops,
        // repeated helper calls); the previous code re-invoked the CP resolver
        // (which allocates three `String`s per call) and re-parsed the
        // descriptor (`count_param_slots`) at *every* call site, including
        // duplicates. Skipping CP indices already resolved makes the resolver +
        // descriptor parse run once per *distinct* index instead of once per
        // occurrence. The resolved value for a given CP index is invariant, so
        // `method_info`'s final contents are byte-for-byte identical.
        let mut method_info: HashMap<u16, usize> = HashMap::new();
        if let Some(resolver) = cp_invoke_resolver {
            let mut scan_pc = 0usize;
            while scan_pc < code.len() {
                if code[scan_pc] == 0xb8 && scan_pc + 2 < code.len() {
                    let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16;
                    // Only resolve indices not seen yet — duplicates would
                    // re-insert the identical value, so dedupe and skip the
                    // resolver allocation + descriptor parse for them.
                    if let std::collections::hash_map::Entry::Vacant(slot) =
                        method_info.entry(cp_idx)
                    {
                        if let Some((_class, _name, descriptor)) = resolver(cp_idx) {
                            slot.insert(count_param_slots(&descriptor));
                        }
                    }
                    scan_pc += 3;
                } else {
                    scan_pc += 1;
                }
            }
        }

        // Interpreter round i5 wave 109, lane L2
        // (`CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT`, default OFF; page
        // `i107-L2-aarch64-unloaded-field-owner-bail-lists-the-method`): a
        // field site whose owner is not loaded (or, for a static, not
        // initialized) YET is absent from the maps below, and the backend
        // refuses an absent site -- a permanent bail once `backend_attempted`
        // is set. With the switch, such a method bails transiently here, as
        // `jitc_bail!("cp_invoke_resolver")` does, and the hot door compiles it
        // once the owner has loaded. NOT BUILT on aarch64 (no host); the
        // decision is `aarch64_backend::arm64_first_cold_field_site`, whose
        // tests run on every host.
        if aarch64_backend::arm64_field_site_transient_enabled() {
            let static_ready = cp_static_field_resolver.map(|resolver| {
                move |cp_idx: u16| {
                    resolver(cp_idx).is_some_and(|(class_id, field_index, _, _)| {
                        direct_helpers
                            .resolve_static_base(class_id, field_index)
                            .is_some()
                    })
                }
            });
            let instance_known =
                cp_field_resolver.map(|resolver| move |cp_idx: u16| resolver(cp_idx).is_some());
            if let Some((pc, op)) = aarch64_backend::arm64_first_cold_field_site(
                code,
                static_ready.as_ref().map(|f| f as &dyn Fn(u16) -> bool),
                instance_known.as_ref().map(|f| f as &dyn Fn(u16) -> bool),
            ) {
                crate::note_jit_bail_site_at("arm64_cold_field_site", pc, op);
                // Interpreter round i6 wave 116, lane L2 (page `i107-L2`): on a
                // background worker this completion is parked until a class is
                // defined, not charged to the method's failure budget
                // (`tiered::MAX_TIER_FAIL_RETRIES`), which settled it for good
                // three offers after warm-up. Inert on any other thread.
                crate::tiered::note_compile_waiting_on_class();
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] resolver-bail site=arm64_cold_field_site(pc={},op=0x{:02x}) {}.{}{}",
                        pc,
                        op,
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor
                    );
                }
                return None;
            }
        }

        // round-7 fix (bug 1): aarch64 backend is also a "heavy"
        // pipeline — a `result.success == false` is a permanent bail
        // for the same reason as the x64 backend bails.  Set the
        // flag before invoking so the outer wrapper records it.
        *backend_attempted = true;

        let mut backend = aarch64_backend::Arm64Backend::new();
        // Without this the backend's `safepoint_flag_addr` stays 0 and
        // `emit_safepoint_poll` emits nothing — the same optional-helper
        // contract x64 uses, so an unwired build is byte-identical.
        backend.set_helpers(*helpers);
        // Seeds the reference-parameter mask, which the entry poll names its
        // live oops from and the local dataflow starts at bci 0 with.
        backend.set_method_descriptor(&cached.method_descriptor, cached.is_static);
        // Round 9 wave 11 (aarch11): `idiv`/`irem`/`ldiv`/`lrem` lower with a
        // direct `ArithmeticException` stub (`helpers.throw_arithmetic`, handed
        // over by `set_helpers` above) only when no handler in this method
        // could catch it -- the interpreter's drain throws with an unknown pc.
        backend.set_exception_table_empty(cached.exception_table.is_empty());
        // Round 9 wave 10 (aarch10; landed by wave 10b, wire10b): resolved
        // `getstatic` sites, and since wave 17 `putstatic` sites too. The
        // backend lowers a primitive static of an initialized class and
        // refuses any site absent from the map. `resolve_static_base` answers
        // only for an initialized class, which is what the READ dereferences
        // and what the WRITE relies on to keep `<clinit>` -- a safepoint --
        // out of its helper; see `Arm64Backend::emit_putstatic`.
        if let Some(resolver) = cp_static_field_resolver {
            let mut statics: HashMap<usize, aarch64_backend::Arm64StaticField> = HashMap::new();
            for (pc, _op, cp_idx) in aarch64_backend::static_field_sites(code) {
                if let Some((class_id, field_index, type_tag, is_volatile)) = resolver(cp_idx) {
                    if let Some(base_cell) =
                        direct_helpers.resolve_static_base(class_id, field_index)
                    {
                        statics.insert(
                            pc,
                            aarch64_backend::Arm64StaticField {
                                class_id,
                                field_index,
                                // Cast: an address, carried as its 64-bit pattern.
                                base_cell: base_cell as u64,
                                type_tag,
                                is_volatile,
                            },
                        );
                    }
                }
            }
            backend.set_static_field_info(statics);
        }
        // Round 9 waves 15-16 (aarch15, aarch16): resolved `getfield` and
        // `putfield` sites. The backend lowers a PRIMITIVE `putfield` through
        // `jit_putfield_*` (wave 15) and a primitive `getfield` through
        // `jit_getfield` (wave 16), the latter taking the VM context pointer
        // as an extra leading argument — which is why a method containing a
        // `getfield` is published with `CompiledMethod::needs_context`. Any
        // site absent from this map refuses the method, as does every
        // reference field.
        //
        // `cp_field_volatile_resolver` is read from `req` rather than from
        // the destructuring above, which binds it to `_`: binding it by name
        // there would leave it unused on x86-64, where this whole block does
        // not exist.
        if let Some(resolver) = cp_field_resolver {
            let volatile_of = req.cp_field_volatile_resolver;
            let mut fields: HashMap<usize, aarch64_backend::Arm64InstanceField> = HashMap::new();
            for (pc, _op, cp_idx) in aarch64_backend::instance_field_sites(code) {
                // Marker tag `0` (a read that must stay on the checked helper,
                // SBR4 satb) is not a field this backend can describe: no row.
                if let Some((field_index, type_tag, _compact)) =
                    resolver(cp_idx).filter(|r| r.1 != 0)
                {
                    fields.insert(
                        pc,
                        aarch64_backend::Arm64InstanceField {
                            field_index,
                            type_tag,
                            // No volatility information means "unknown", and
                            // unknown must compile as `volatile`: a plain
                            // store to a field that turns out to be volatile
                            // is the miscompile, not the other way round. The
                            // cost of being wrong in this direction is two
                            // `DMB ISH`s.
                            is_volatile: volatile_of.is_none_or(|f| f(cp_idx)),
                        },
                    );
                }
            }
            backend.set_instance_field_info(fields);
        }
        // Round 9 wave 18 (aarch18): resolved `new`/`anewarray` sites. Both
        // call a helper that can SAFEPOINT, so the backend emits them through
        // `emit_helper_call_at_safepoint`, which records an oop map at the
        // return address -- and sizes an allocating method's frame with the
        // safepoint homes and the id word whether or not polls are on.
        // `newarray` needs nothing from here: its `atype` is an operand byte.
        // A `JitNewSite::Deferred` site is NOT passed on; the CP-indexed
        // helper that resolves it at run time is a second entry point this
        // backend does not lower, so the site refuses and the method bails.
        if let Some(resolver) = cp_new_resolver {
            let mut sites: HashMap<usize, aarch64_backend::Arm64NewSite> = HashMap::new();
            for (pc, op, operand) in aarch64_backend::allocation_sites(code) {
                if op == 0xbc {
                    continue;
                }
                if let Some(JitNewSite::Resolved {
                    class_id,
                    num_fields,
                    ..
                }) = resolver(operand)
                {
                    sites.insert(
                        pc,
                        aarch64_backend::Arm64NewSite {
                            class_id,
                            num_fields,
                        },
                    );
                }
            }
            backend.set_new_site_info(sites);
        }
        // h23c: resolved `multianewarray` sites. The descriptor is the same
        // packed `(holder_class_id | cp_idx << 32)` the x64 door builds, and
        // the resolver call is kept for the same reason it is there: a CP
        // index that does not name a class is a refusal, not a site to emit
        // a call for. Arity does not appear here -- `multianewarray_n` takes
        // it separately and the backend reads it off the opcode's operand
        // byte -- so one map serves every dimension count.
        if let Some(resolver) = cp_class_name_resolver {
            let mut sites: HashMap<usize, i64> = HashMap::new();
            for (pc, cp_idx) in aarch64_backend::multianewarray_sites(code) {
                if resolver(cp_idx).is_some() {
                    sites.insert(
                        pc,
                        pack_multianewarray_site(cached.declaring_class_id.as_u32(), cp_idx),
                    );
                }
            }
            backend.set_multianewarray_site_info(sites);
        }
        // Round 9 wave 23 (aarch23): resolved `checkcast`/`instanceof`
        // targets. The helpers take a NAME, so this is the class-name
        // resolver rather than a class-id one -- and the name is the
        // PROCESS-INTERNED copy, because `jit_checkcast` /
        // `jit_instanceof_check` memoize their verdict on `(ptr, len)` in
        // thread-locals that outlive any one compilation. An artifact-owned
        // copy would miss that memo on every call and leave a dangling key in
        // it afterwards. Same interning the x64 site scan uses, including the
        // resolved-id and finality hints, which cost nothing here and make the
        // helper compare ids rather than re-resolve a bare name.
        if let Some(resolver) = cp_class_name_resolver.as_ref() {
            let mut sites: HashMap<usize, aarch64_backend::Arm64TypecheckSite> = HashMap::new();
            for (pc, _op, cp_idx) in aarch64_backend::typecheck_sites(code) {
                let Some(class_name) = resolver(cp_idx) else {
                    continue;
                };
                let (ptr, len) = intern_typecheck_for_new_site(
                    &class_name,
                    cp_new_resolver.and_then(|r| r(cp_idx)),
                    req.class_is_final_resolver,
                );
                sites.insert(
                    pc,
                    aarch64_backend::Arm64TypecheckSite {
                        // Cast: an interned address, as its 64-bit pattern.
                        name_ptr: ptr as u64,
                        name_len: len,
                    },
                );
            }
            backend.set_typecheck_site_info(sites);
        }
        // Round 9 wave 22 (aarch22): resolved `invoke*` sites -- the first
        // bytecode CALL this backend compiles.
        //
        // There is still no call-target RESOLUTION here, and there does not
        // need to be. Every site goes through `jit_invoke_dispatch`, which
        // takes a `JitInvokeInfo` and resolves the target itself at run time:
        // the receiver's class for a virtual or interface call, the compiled
        // entry when the callee has one and the interpreter when it does not.
        // So what this block produces is a NAME, not an address, and the four
        // pieces x64 needs for a direct call (the entry, the inline cache, an
        // outgoing Java argument ABI, the self-recursion guard) are all inside
        // the helper.
        //
        // The boxes have to outlive the artifact, because their addresses are
        // baked into its code as immediates. They are parked in
        // `CompiledMethod::_jit_strings` / `_jit_invoke_infos` below, which is
        // exactly what every x64 dispatch site does with its own.
        let mut owned_strings: Vec<Box<str>> = Vec::new();
        let mut owned_invoke_infos: Vec<Box<JitInvokeInfo>> = Vec::new();
        if let Some(resolver) = cp_invoke_resolver {
            let mut sites: HashMap<usize, aarch64_backend::Arm64InvokeSite> = HashMap::new();
            for (pc, op, cp_idx) in aarch64_backend::invoke_sites(code) {
                // `invokedynamic` is not resolvable this way: `invoke_kind`
                // encodes virtual/special/interface/static and a call site is
                // none of them. The backend refuses `0xba` in its own arm too.
                if op == 0xba {
                    continue;
                }
                let Some((class_name, method_name, descriptor)) = resolver(cp_idx) else {
                    continue;
                };
                // JVMS §6.5's super-call redirect, applied for the same reason
                // the x64 site scan applies it: the constant pool names the
                // symbolic reference, and selection for `invokespecial` starts
                // at the direct superclass when the caller's `ACC_SUPER`
                // applies. Baking the unredirected name calls the wrong method.
                // The redirect's class id rides along (`owner_class_id`).
                let redirect = if op == 0xb7 {
                    cp_invokespecial_owner_resolver.and_then(|r| r(cp_idx, op))
                } else {
                    None
                };
                let (class_name, owner_class_id) = match redirect {
                    Some((owner, owner_id)) => (owner, owner_id),
                    None => (class_name, 0),
                };
                let invoke_kind: u8 = match op {
                    0xb6 => 0, // virtual
                    0xb7 => 1, // special
                    0xb9 => 2, // interface
                    _ => 3,    // static
                };
                // One argument per VALUE (this VM's compact convention: a
                // `long` is one), plus the receiver for everything but
                // `invokestatic`. The same count the backend pops and the
                // helper indexes its buffer by, which is why both read it off
                // this one field.
                let num_jit_args = count_param_slots(&descriptor) + usize::from(op != 0xb8);
                let ret_type = return_type(&descriptor);
                let class_ref = intern_jit_invoke_str(&class_name);
                let method_ref = intern_jit_invoke_str(&method_name);
                let desc_ref = intern_jit_invoke_str(&descriptor);
                let info = Box::new(JitInvokeInfo {
                    class_name: class_ref,
                    method_name: method_ref,
                    descriptor: desc_ref,
                    num_jit_args,
                    return_type: ret_type,
                    invoke_kind,
                    // A class NAME is not a class identity -- two loaders can
                    // each define one -- and `jit_invoke_dispatch` resolves
                    // `invokespecial` by name. This is what says which copy
                    // the caller's constant pool meant.
                    declaring_class_id: cached.declaring_class_id.as_u32(),
                    owner_class_id,
                });
                let info_ptr: *const JitInvokeInfo = &*info;
                owned_invoke_infos.push(info);
                sites.insert(
                    pc,
                    aarch64_backend::Arm64InvokeSite {
                        // Cast: an address, carried as its 64-bit pattern.
                        info_ptr: info_ptr as u64,
                        num_jit_args,
                        return_type: ret_type,
                    },
                );
            }
            backend.set_invoke_site_info(sites);
        }
        let result = backend.compile_method_with_info(
            cached.max_locals as usize,
            num_params,
            cached.max_stack as usize,
            code,
            method_info,
        );
        // WHY A REFUSAL HAPPENED, which nothing could ask before (round 9 wave
        // 23). Every arm of this backend that declines emits an
        // `Arm64Instruction::Comment` naming what it wanted, and every one of
        // those was discarded here: `try_compile_inner`'s aarch64 branch
        // returns `None` before any pipeline stage stamps its name, so
        // `CRATONVM_DBG_JITC` could only ever report the fallback
        // `no-site-after-entry` -- "it refused somewhere in the backend".
        //
        // Measured on the first whole-VM aarch64 run that reached a method
        // shaped for this backend: the compile bailed, the trace named the
        // fallback, and localising it further meant reading the bytecode by
        // hand. The comments were already written; they just had no reader.
        if !result.success && crate::x64::dbg_jitc_enabled() {
            let why = result
                .instructions
                .iter()
                .rev()
                .find_map(|i| match i {
                    aarch64_backend::Arm64Instruction::Comment(c) => Some(c.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "no arm named a reason".to_string());
            eprintln!(
                "[cratonvm-jitc] arm64-refused {}.{}{} — {why}",
                cached.class_name, cached.method_name, cached.method_descriptor
            );
        }
        if result.success {
            // One line on purpose: everything this used to do inline lives in
            // `publish_compiled_method`, which has no `cfg` on it and so is
            // type-checked and unit-tested on every host. This block is not
            // compiled on x86-64, which is how it came to build its
            // `CompiledMethod` without ever transferring `oop_maps`.
            if let Some(mut cm) = aarch64_backend::publish_compiled_method(&result) {
                // The arenas whose ADDRESSES the code above baked in. Moved
                // here rather than leaked: an artifact that is dropped takes
                // its own infos with it, and nothing else can be holding one,
                // because only its code names them.
                cm._jit_strings = owned_strings;
                cm._jit_invoke_infos = owned_invoke_infos;
                return Some(cm);
            }
        }
        return None;
    }

    let code = &cached.code;
    let code_len = code.len().saturating_sub(2);

    if code_len == 0 {
        return None;
    }

    /// [`jitc_bail`] for a refusal attributable to ONE bytecode, so the stats
    /// table prints `site(pc=N,op=0xNN)` instead of a bare policy name. A
    /// refusal that names only the policy tells the reader which gate fired but
    /// not which lowering has to change to pass it.
    macro_rules! jitc_bail_at {
        ($site:expr, $pc:expr, $op:expr) => {
            return {
                crate::note_jit_bail_site_at($site, $pc, $op);
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] resolver-bail site={}(pc={},op=0x{:02x}) {}.{}{}",
                        $site,
                        $pc,
                        $op,
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor
                    );
                }
                None
            }
        };
    }

    /// A bail the compiler will never take back: the offending property is
    /// fixed by the class file, so retrying cannot change the answer. Sets
    /// `backend_attempted`, which is what routes the method onto the permanent
    /// bail-list — the flag's real meaning, despite its name.
    ///
    /// Named exactly like [`jitc_bail`] so the two read the same in a trace;
    /// the `permanent-bail` prefix is what tells them apart.
    macro_rules! jitc_permanent_bail {
        ($site:expr) => {
            return {
                *backend_attempted = true;
                crate::note_jit_bail_site($site);
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] permanent-bail site={} {}.{}{}",
                        $site, cached.class_name, cached.method_name, cached.method_descriptor
                    );
                }
                None
            }
        };
    }

    // Phase 1 (scan/parse). The guard also covers the `return None` arm below:
    // a scan reject is a real compilation that spent real time, and its report
    // should say so.
    note_jit_pipeline_stage(JIT_STAGE_SCAN);
    let metrics_scan = metrics.phase(metrics::Phase::Scan);
    let scan = match x64::jit_scan(code, code_len, &cached.method_descriptor) {
        Some(s) => s,
        None => {
            if false {
                eprintln!(
                    "[rbc6-dbg] try_compile_inner: jit_scan returned None for {}.{}{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                );
            }
            // RBC.4 — a scan reject (unsupported opcode, e.g. `athrow`) is
            // just as permanent as a backend bail: the bytecode never
            // changes. Without marking it, a hot uncompilable method re-ran
            // the whole upgrade gauntlet (skip-list + native-shadow
            // hierarchy walks under the class_manager lock + this scan)
            // every JIT_RETRY_STRIDE calls forever — observed 40,039
            // attempts on `DefiniteLengthInputStream.readAllIntoByteArray`
            // in ONE asn1 RegressionTest run. Route through the existing
            // permanent bail-list machinery.
            jitc_permanent_bail!("jit-scan-reject")
        }
    };
    drop(metrics_scan);

    // RBC.6 (RELAXED, see docs/feature-designs/jit-local-exception-handlers.md)
    // — this gate used to unconditionally refuse any method that combines
    // `athrow` with a local exception handler, on the theory that "the
    // athrow lowering stashes the exception and returns the deopt sentinel,
    // which cannot dispatch to an in-method handler." That was true when
    // this gate was written (`8d0f029ed`, 2026-06-11), but every fix that
    // landed the day after and later — BUG-H (`82b9bdf62`), KCFULL-13,
    // Round-8/9/10/11 (NPE/AIOOBE/arithmetic/general-exception leak fixes),
    // and the wildfly-bug-05 this/params restore (`549a2c161`) — built
    // exactly the missing dispatch, generically, and never revisited this
    // gate:
    //   - `emitted_athrow` unconditionally forces `has_dispatch = true`
    //     (`x64.rs`, `RBC.6` comment at the `has_dispatch` computation), so
    //     any method containing `athrow` is ALWAYS entered through
    //     `execute_jit_call`'s dispatch-aware slow path, never the raw
    //     fast-path that would leak the sentinel as a return value.
    //   - That slow path drains `JIT_PENDING_EXCEPTION` (and NPE/AIOOBE/
    //     arithmetic) after every JIT return and, whenever the invoked
    //     method itself declares a non-empty `exception_table`, routes the
    //     exception through `route_jit_exception_through_method` — a real
    //     handler search (typed catch-class matching with subclass checks,
    //     first-match-wins, `finally`/catch-all handling) that pushes a
    //     fresh interpreter frame at the resolved handler pc with `this`
    //     and the declared params restored and the exception object on the
    //     stack, and otherwise propagates to the caller exactly like an
    //     uncaught throw. This is reached identically whether the pending
    //     exception came from `athrow` in THIS method or propagated up from
    //     a callee — the sentinel-return shape is the same either way.
    //   - `callee_has_exception_table`/`route_implicit_exc_through_callee`
    //     provide the JIT-to-JIT direct-call sibling of the same routing.
    //
    // In other words: the "cannot dispatch to an in-method handler" premise
    // this gate was built on stopped being true within a day of it landing,
    // and nothing since has depended on athrow+handler staying uncompiled —
    // methods that only *declare* a handler (no local `athrow`) already
    // compile today and already rely on this exact runtime routing (that's
    // what BUG-H fixed). Only the compile-time refusal itself was stale.
    // Confirmed root cause of `Response.toAbsolute()` (Tomcat hot path,
    // `try { ... } catch (IOException) { throw new
    // IllegalArgumentException(...) }`) never JIT-compiling — see the doc
    // above for the full trace and validation plan. The IR (optimizing)
    // path keeps its own independent, unaffected
    // `cached.exception_table.is_empty()` admission check below, so this
    // change only widens single-pass (`x64::compile`) eligibility — the
    // existing, well-tested fallback backend that already carries every
    // other non-`ir_compatible` method.
    //
    // RBC.6 local-handler-safety fix — `route_jit_exception_through_method`
    // (vm/src/runtime/interpreter.rs) reconstructs the handler frame from
    // ONLY `this` + the method's declared incoming params (a documented,
    // pre-existing limitation — see that function's own doc comment). A
    // handler that reads any OTHER local — one first assigned earlier in
    // the method, whether inside this try, a DIFFERENT try/catch construct,
    // or straight-line code before either — observes a stale zero/null
    // instead of the value the compiled code actually computed. Confirmed
    // via a differential repro: `AthrowCountBisect.twoThrowsSequential` (two
    // sequential, non-nested try/catch blocks in one method; the second
    // handler's own code reads a local last assigned by the FIRST try's
    // successful path) silently computed a wrong checksum once compiled —
    // this affects declares-a-handler-no-athrow methods exactly as much as
    // the newly-relaxed has-athrow ones, since the frame-reconstruction gap
    // is in the shared runtime routing, not anything athrow-specific. Gate
    // BOTH populations (any non-empty `exception_table`, not just the
    // has_athrow case above) on `local_handler_reads_unsafe_local`, a real
    // CFG-based "definitely assigned" dataflow (dominance-respecting, not a
    // raw-pc-order approximation — an earlier version of this check used
    // exactly that simpler approximation and was found, via the REAL
    // `org.apache.catalina.connector.Response.toAbsolute()` bytecode on the
    // Tomcat suite fixture, to over-reject it: it kept scanning past the
    // handler's own `athrow` terminator into unrelated later code in the
    // same method). See `local_handler_reads_unsafe_local`'s own doc
    // comment, and `regalloc::handler_has_unsafe_local_read`'s, for the
    // full algorithm and soundness argument.
    let mut precise_exception_frames = false;
    if !cached.exception_table.is_empty() {
        let unsafe_local = local_handler_reads_unsafe_local(
            code,
            code_len,
            &cached.exception_table,
            &cached.method_descriptor,
            cached.is_static,
        );
        if false {
            eprintln!(
                "[rbc6-dbg] try_compile_inner: local_handler_reads_unsafe_local={} for {}.{}{}",
                unsafe_local, cached.class_name, cached.method_name, cached.method_descriptor
            );
        }
        if unsafe_local {
            #[cfg(target_arch = "x86_64")]
            {
                // Shared with the eager first-call door (interpreter round i1
                // wave 28, lane L2): see the function.
                let blocking_site =
                    precise_handler_frame_blocking_site(code, code_len, &cached.exception_table);
                if let Some((pc, op)) = blocking_site {
                    // A handler that reads a later local remains interpreted
                    // unless every throwing site in its protected ranges can
                    // publish that local through the precise exceptional-frame
                    // handoff. Compiling any broader shape would reset the
                    // handler local to null/zero on an exception.
                    //
                    // Report the REASON on the default trace, not just under
                    // `CRATONVM_DBG_RBC6`. Without this the outer wrapper only
                    // prints `compile-bail ... backend_attempted=false`, which
                    // reads like a transient resolver miss — and a permanently
                    // interpreted hot method looks identical to one that is
                    // simply still warming up. (Found chasing Tomcat's
                    // `StringCache.toString`, refused for its `synchronized`
                    // block's javac-generated monitor handler; it sits in the
                    // middle of a 600M-call hot chain whose neighbours both
                    // compile. See 30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md.)
                    //
                    // The pc/opcode is the actionable half: it names the ONE
                    // lowering that would have to publish a precise frame for
                    // this method to compile. See
                    // `first_unsupported_precise_frame_site`.
                    jitc_bail_at!("rbc6-handler-reads-unsafe-local", pc, op);
                }
                precise_exception_frames = true;
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                // `jitc_bail_at!` with `(0, 0)`, not `jitc_bail!`: this
                // function's scope defines only the site-naming macro, and the
                // plain one lives in a DIFFERENT function -- so this arm did
                // not compile at all on aarch64 (round 9 wave 12; the wave-10b
                // note predicted exactly this, "the block only compiles on an
                // aarch64 target, which the integrator's x86-64 build does not
                // check"). `(0, 0)` is the same pair the x86-64 arm above
                // passes when the relaxation is off wholesale: no single
                // bytecode is at fault here either -- the backend is.
                jitc_bail_at!("rbc6-handler-reads-unsafe-local-non-x64", 0, 0);
            }
        }
    }
    // A protected deopt guard after a committed effect normally makes whole-
    // method replay unsafe (the refusal reported by
    // `ir_unresumable_protected_trap_in`). The IR lowerer can instead publish
    // a precise frame at that guard and resume the interpreter at its exact
    // bci. Request that route for this narrow population; the post-lowering
    // `ir_precise_frames_trap_post_check` remains the admission backstop and
    // discards any artifact whose exits cannot all resume precisely. This
    // lets a protected `arraylength` after a byte-producing call compile
    // without weakening the replay guard or duplicating the call.
    #[cfg(target_arch = "x86_64")]
    if !precise_exception_frames
        && !cached.exception_table.is_empty()
        && ir_precise_handler_frames_enabled()
        && (!cached.is_synchronized || ir_precise_frames_synchronized_enabled())
    {
        if let Some((pc, op)) = ir_unresumable_protected_trap_in(
            code,
            code_len,
            &cached.exception_table,
            cached.is_static,
        ) {
            precise_exception_frames = true;
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] replay-trap precise-frames {}.{}{} pc={pc} opcode={op:#04x}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                );
            }
        }
    }
    // JIT round 17 wave 2 (lane syncdoor, SY16-4): a single-pass SELF-LOCKING
    // body of a method with an exception table needs its catches compiled into
    // the frame that holds the monitor, i.e. the local handlers, which ride
    // precise exception frames. Default ON since the round-18 close
    // (`CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS`); never on the handler-
    // suppressed retry, whose body check would refuse the self-lock anyway.
    #[cfg(target_arch = "x86_64")]
    {
        if !precise_exception_frames
            && !local_handlers_disarmed
            && self_lock_handlers_need_precise_frames(req, cached, code, code_len, admission.door())
        {
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] self-lock-handlers precise-frames {}.{}{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                );
            }
            precise_exception_frames = true;
        }
    }
    // The optimizing OSR door routes an exception that leaves its artifact by
    // the reason-9 frame the artifact publishes at the throwing site, and reads
    // NO frame as "the throw lies outside every protected range"
    // (`route_osr_exception_out_of_artifact`). A body compiled without the
    // precise-frame route publishes none, so a handler that reads no unsafe
    // local (the usual `catch (Throwable t) { sink.set(t); }`) was never reached:
    // the exception left the method past a `catch` that covers the call
    // (`CatchEscape`; Netty's `NioEventLoopTest.testTaskRemovalOnShutdown...`
    // saw `null` where HotSpot had a `RejectedExecutionException`). So the OSR
    // door's compile of a method with handlers takes the precise-frame route
    // whenever it can, and refuses (the loop stays interpreted) where a throwing
    // site cannot publish its frame.
    if !precise_exception_frames && req.osr_mode_exit_polls && !cached.exception_table.is_empty() {
        #[cfg(target_arch = "x86_64")]
        {
            if let Some((pc, op)) =
                precise_handler_frame_blocking_site(code, code_len, &cached.exception_table)
            {
                jitc_bail_at!("osr-handlers-need-precise-frames", pc, op);
            }
            precise_exception_frames = true;
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            jitc_bail_at!("osr-handlers-need-precise-frames-non-x64", 0, 0);
        }
    }
    // Round 11 wave 10 (lane irexc): may the OPTIMIZING tier take this RBC.6
    // method too? Only under the switch, and never for an `ACC_SYNCHRONIZED`
    // one (see `ir_precise_handler_frames_enabled`). When this is true the IR
    // lowerer is handed the protected ranges and publishes a reason-9 frame at
    // every call-exception exit inside them, which is the promise
    // `precise_exception_frames` stands for.
    // Round 14 wave 1 (lane sync, backlog #9 / irexc proposal W11-2): an
    // `ACC_SYNCHRONIZED` method is admitted too under
    // `ir_precise_frames_synchronized_enabled` (default OFF).
    let ir_precise_frames = precise_exception_frames
        && (!cached.is_synchronized || ir_precise_frames_synchronized_enabled())
        && ir_precise_handler_frames_enabled();

    // Try IR compilation for simple integer-only methods. The IR pipeline
    // types every value as 32-bit `IrType::Int` and lays parameters out by
    // JIT-argument index rather than JVM local slot, so it cannot represent
    // 64-bit longs/doubles nor the two-slot category-2 parameter layout.
    // Methods touching long/double fall through to the bytecode-x64 backend,
    // which models them correctly. (Before this guard, a method like
    // `boolean eq(long, long)` truncated the second long parameter — read
    // from the never-populated `locals[2]` slot — and panicked with a
    // `u32::MAX` slot index; bc-java InterleaveTest failed 2/4 under JIT.)
    // wire-tiered-manager Step 3: `optimize == false` is the C1 (fast) tier —
    // skip the whole optimizing IR pipeline (build → optimize → escape analysis
    // → schedule → lower) and fall through to the single-pass `x64::compile`
    // backend below. The single-pass backend is the existing, well-tested
    // fallback (it already serves every category-2 / non-`ir_compatible`
    // method), so routing more methods to it is a throughput trade-off, never a
    // correctness risk. C2 (`optimize == true`, every non-tiered caller)
    // keeps the historical IR-first behaviour.
    // `CRATONVM_JIT_FORCE_C2=1` (default off): treat every compile request as a
    // C2 request. It exists because `optimize=false` is by far the commonest
    // reason a method never reaches the optimizing tier, and it is decided
    // OUTSIDE this crate by the tier-selection paths — so a probe written to
    // exercise IR codegen could not reach it at all, no matter how it was
    // shaped. That is not a hypothetical: the whole IR relocation-map contract
    // could not be measured for want of one live IR frame at a young collection
    // (`jit-ir-relocation-map-contract.md`), and every probe
    // written for it was defeated by this one bit.
    //
    // It changes WHICH tier compiles a method, never what a compiled method
    // does: the optimizing pipeline falls back to the same single-pass backend
    // for anything it cannot lower, exactly as it does when the tier manager
    // asks for C2 of its own accord.
    let optimize = optimize || force_c2_enabled();
    // `java/lang/String`'s field layout, resolved ONCE for the whole
    // compilation. THREE consumers: the printed admission verdict just below,
    // the optimizing-tier gate after it (`string_intrinsic_pin_declines`) and
    // `try_resolve_string_intrinsic` in the single-pass invoke loop near the end
    // of this function. The SAME resolver feeds `x64::compile`'s
    // `compiler.string_layout`, so the matcher's "registered" decision and the
    // codegen's "can emit inline" decision never disagree — a String sentinel is
    // never registered for a site whose codegen would then bail to a raw `CALL`.
    //
    // Resolved here, above the verdict block, rather than between the verdict
    // and the gate: the verdict has to be able to report what the gate decided,
    // and it cannot do that without the gate's own input. The move costs
    // nothing — this line already ran unconditionally on every compile a few
    // statements further down.
    let resolved_string_layout: Option<StringFieldLayout> =
        string_layout_resolver.and_then(|r| r());
    // The builder gets THIS compile's layout directly
    // (`IrBuilder::set_string_layout` in `ir_tier`, round 11 wave 3). The
    // publish below no longer seeds any builder — a process-wide `OnceLock`
    // keeps the FIRST VM's field indices, which are per VM, not per process
    // (`r11w2-rt-ir-string-layout-is-process-global`). It is kept for its one
    // remaining effect: the first resolvable layout bumps the acceptance
    // gate's inputs generation.
    if let Some(layout) = resolved_string_layout {
        ir::publish_string_layout(layout);
    }
    // The admission chain below is a conjunction of six independent terms, and
    // failing any one of them falls silently through to the single-pass
    // backend. That silence is what left "why does the optimizing tier produce
    // no bodies?" unanswerable for the whole relocation-contract investigation
    // (`jit-ir-relocation-map-contract.md`) — every term's
    // failure looks exactly like every other's, and like the tier being off.
    // Under `CRATONVM_DBG_JITC` / `CRATONVM_DBG_IR_COMPILES` each candidate now
    // reports which term declined it. Evaluated lazily (and only under the
    // flag) so the default path pays nothing.
    //
    // C2-review P0 "Measure compilation quality": the same verdict is the
    // per-method report's `admission` field, so "why did the optimizing tier
    // never see method X?" is answerable structurally instead of by grepping
    // this line out of stderr. `metrics.is_enabled()` is one relaxed atomic
    // load, and the verdict is still built only when somebody will read it.
    //
    // Third disjunct: a JFR consumer of `cratonvm.JitCompileDecision` is the
    // third reader of this verdict, and without it here the verdict would never
    // be BUILT for that consumer, so every decision event would carry the
    // fallback reason "compiled" and the event would answer nothing.
    // `jit_decision_enabled()` is one `Acquire` load of a `bool` that is false
    // forever on a default run.
    if ir_stage_reporting()
        || metrics.is_enabled()
        || cratonvm_jfr::jit_decision::jit_decision_enabled()
    {
        let verdict = if !optimize {
            "optimize=false — the C1/fast tier was requested, not C2".to_string()
        } else if moving_young_disables_optimizing_tier() {
            "moving-young relocates compiled frames".to_string()
        } else if dbg_jit_single_pass_only(&cached.class_name, &cached.method_name) {
            "pinned to the single-pass backend by CRATONVM_DBG_JIT_SINGLE_PASS_ONLY".to_string()
        } else if code_len > ir::IR_MAX_BYTECODE_SIZE {
            format!("bytecode length {code_len} exceeds IR_MAX_BYTECODE_SIZE")
        } else if !ir::ir_compatible(&scan) {
            // `ir_compatible` has already printed the refused conjunct.
            "ir_compatible refused (conjunct named above)".to_string()
        } else if exc_table_c2_disabled() && !cached.exception_table.is_empty() {
            "CRATONVM_JIT_NO_EXC_TABLE_C2=1 and the method has an exception table".to_string()
        } else if precise_exception_frames && !ir_precise_frames {
            "precise exception frames required (RBC.6: a handler reads a non-parameter local; \
             CRATONVM_JIT_IR_PRECISE_HANDLER_FRAMES=1 lets this tier publish them)"
                .to_string()
        } else if let Some((pc, op)) = (ir_unresumable_trap_guard_enabled() && !ir_precise_frames)
            .then(|| {
                ir_unresumable_protected_trap_in(
                    code,
                    code_len,
                    &cached.exception_table,
                    cached.is_static,
                )
            })
            .flatten()
        {
            format!(
                "an inline trap this tier deopts on (pc={pc}, opcode={op:#04x}) sits in a \
                 replay prefix can repeat a side effect; whole-method replay is unsafe, so \
                 the single-pass backend takes it"
            )
        } else if let Some(k) = single_pass_only_lowering_for(code, code_len, cached) {
            format!(
                "the single-pass backend has {} here and the IR tier has no equivalent",
                k.label()
            )
        } else {
            // The String-intrinsic pin, in the position it occupies in the real
            // conjunction below: immediately after `single_pass_only_lowering_for`,
            // which is the last refusal this chain tests before the value shape.
            // (This chain reports the exception-table and precise-frame terms
            // EARLIER than the conjunction evaluates them; that mismatch predates
            // this term and only decides which reason gets printed when two apply
            // at once, so it is left alone.)
            //
            // It was missing from this chain entirely, so a method the pin
            // declined and a method the pin never saw printed the same `admitted
            // to the optimizing pipeline` — which is how five hypotheses about
            // `String.charAt` could each be refuted without ever converging. See
            // `StringPinVerdict` and
            // string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901.
            //
            // Evaluated here rather than at the top of the block so the earlier
            // arms still short-circuit before it: this one calls the caller's
            // constant-pool resolver once per invoke site, which is the most
            // expensive step in the chain and pointless for a method already
            // refused. `string_intrinsic_pin_verdict` is deliberately the PURE
            // half of the pair — the counters live in
            // `string_intrinsic_pin_declines`, which the real conjunction calls,
            // so switching a diagnostic flag on cannot move a census.
            // JIT round 21 wave 20 (lane sbfuse): a fused builder chain's
            // sites are not weighed (`invoke_ops_for_string_pin`).
            let string_pin = string_intrinsic_pin_verdict(
                &invoke_ops_for_string_pin(
                    req,
                    code,
                    code_len,
                    &string_pin_invoke_ops(code, code_len, &scan.invoke_ops),
                ),
                cp_invoke_resolver,
                resolved_string_layout,
            );
            if string_pin == StringPinVerdict::Pinned {
                "pinned to the single-pass backend: it has a String access intrinsic here \
                 and the IR tier has none"
                    .to_string()
            } else if arraylist_prefix_pins_single_pass(
                &scan.invoke_ops,
                cp_invoke_resolver,
                resolved_string_layout,
            ) {
                "pinned to the single-pass backend: it has an exact-ArrayList get/size site \
                 the single-pass prefix answers inline, and CRATONVM_JIT_IR_ARRAYLIST_PREFIX=0 \
                 keeps the IR tier's own prefix out of the decision \
                 (CRATONVM_JIT_NO_ARRAYLIST_PIN=1 opts out)"
                    .to_string()
            } else {
                // The same predicate the conjunction below evaluates, so the
                // printed reason cannot drift from the decision. The two shape
                // facts are re-derived only to NAME a refusal.
                let shape = if ir_value_shape_admitted(
                    code,
                    code_len,
                    &cached.method_descriptor,
                    ir_emit_long,
                    ir_emit_fp,
                ) {
                    "admitted to the optimizing pipeline".to_string()
                } else {
                    let cat2 = method_uses_category2(code, code_len, &cached.method_descriptor);
                    let fp = method_uses_fp(code, code_len, &cached.method_descriptor);
                    format!(
                        "value shape not admitted (category2={cat2} fp={fp} \
                         ir_emit_long={ir_emit_long} ir_emit_fp={ir_emit_fp})"
                    )
                };
                // The remaining three pin states do not REFUSE the method, they
                // qualify what happened to it on the way through — the pin was
                // switched off by flag, or it was asked with an input missing and
                // could not see the sites at all. Appended rather than
                // substituted so `admission` keeps meaning "admitted", and so the
                // two silent failures the whole investigation turned on are named
                // in the same line that used to hide them.
                match string_pin.admission_note() {
                    Some(note) => format!("{shape} ({note})"),
                    None => shape,
                }
            }
        };
        metrics.set_admission(&verdict);
        // Carry the verdict to the compile funnel in
        // `try_compile_with_invokespecial_resolver`, which is where this
        // compile's single `cratonvm.JitCompileDecision` is emitted. See
        // `JIT_ADMISSION_VERDICT` for why the event is emitted THERE and not
        // here: only the funnel knows `used_ir_backend`, and this verdict is a
        // prediction of it, not the fact. A no-op unless the event is armed.
        note_jit_admission_verdict(&verdict);
        if ir_stage_reporting() {
            eprintln!(
                "[ir] admission {}.{}{}: {verdict}",
                cached.class_name, cached.method_name, cached.method_descriptor,
            );
        }
    }
    // A method the acceptance gate has already refused for want of evidence is
    // not offered to the IR pipeline again. The verdict is a property of the
    // BYTECODE and the PASSES, both of which are the same on every attempt, so
    // re-deriving it means building an artifact only to throw it away.
    // Measured before this: 577 refusals on the H2 JDBC workload drove
    // `fell_through_to_single_pass` from 12 compiles (44 ms) to 150 (586 ms).
    //
    // A conjunct of the admission `if` rather than an early return, because
    // this must skip the IR ATTEMPT and fall through to the single-pass
    // backend below — returning here would leave the method interpreted, which
    // is a far worse outcome than the one the gate exists to avoid.
    let ir_method_hash = compute_jit_key_hash(
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        cratonvm_types::ClassId::new(0),
    );
    // The refusal memo is keyed per declaring class identity and per redefine
    // epoch, so neither a same-named class in another loader nor a verdict
    // about replaced bytecode skips this body's IR attempt. See
    // `ir_refusal_memo_key`.
    // Both are the compiling VM's (interpreter round i1 wave 23): its refusal
    // memo, keyed with its own redefine epoch.
    let ir_refusal_key = req
        .verdicts()
        .ir_refusal_key(ir_method_hash, cached.declaring_class_id);
    // ACCEPT21-3 (t1 wave 7 lane NBA, `CRATONVM_JIT_OSR_ROUTE_OWN_REFUSAL_KEY`,
    // default ON since j27): the optimizing OSR route's own memo key; a sticky site-trap ban
    // on the shared key still holds. See `osr_route_own_refusal_key_enabled`.
    let ir_memo_key = ir_refusal_memo_route_key(ir_refusal_key, req.osr_optimizing_route);
    // JIT round 21 lane tiermgr (INDYLATE21-1,
    // `CRATONVM_JIT_IR_MEMO_METHOD_INPUT_STAMP`): the input stamps this build
    // reads under, taken BEFORE the memo is asked; a refusal is recorded with
    // them, not with the stamps current at its end (`ir_evidence::IrRefusalTicket`).
    let ir_refusal_ticket = req.verdicts().ir_refusals().ticket(ir_refusal_key);
    let ir_refused_before = {
        let memo = req.verdicts().ir_refusals();
        let own = memo.already_refused(ir_memo_key);
        let shared_ban = ir_memo_key != ir_refusal_key
            && memo.already_refused(ir_refusal_key)
            && memo.holds_sticky_ban(ir_refusal_key);
        if ir_memo_key != ir_refusal_key
            && !own
            && !shared_ban
            && memo.already_refused(ir_refusal_key)
            && false
        {
            eprintln!(
                "[cratonvm-jitc] osr-route refusal memo: {}.{}{} not skipped by a method-entry refusal (ACCEPT21-3)",
                cached.class_name, cached.method_name, cached.method_descriptor,
            );
        }
        own || shared_ban
    };
    if ir_refused_before {
        ir_evidence::note_memo_skip();
        // nio21 wave 18, lane L4 (`CRATONVM_JIT_MEMO_SKIP_IS_A_REFUSAL`): the
        // memo IS the gate's earlier refusal, so an optimizing compile it
        // skips reports that verdict to its door, exactly as the gate would
        // have. See [`memo_skip_publishes_refusal`].
        if memo_skip_publishes_refusal(optimize) {
            ir_evidence::publish_verdict(false);
        }
        if ir_stage_reporting() {
            eprintln!(
                "[ir] acceptance {}.{}{}: SKIPPED (a previous compile carried no evidence)",
                cached.class_name, cached.method_name, cached.method_descriptor,
            );
        }
    }
    // Which admission conjunct refused, for the funnel census
    // (`ir_evidence::AdmissionExit`). Written only by the `ir_compatible_sized`
    // conjunct, so it stays `false` when an earlier term short-circuited.
    let ir_compatible_refused = std::cell::Cell::new(false);
    // zs s7/vl (`CRATONVM_DBG_JIT_SINGLE_PASS_ONLY`, unset = off): a method
    // the measurement filter names never enters the optimizing pipeline; it
    // falls through to the single-pass backend below like any other refusal.
    let single_pass_pinned =
        optimize && dbg_jit_single_pass_only(&cached.class_name, &cached.method_name);
    if single_pass_pinned && x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] single-pass-only {}.{}{}: optimizing tier refused (CRATONVM_DBG_JIT_SINGLE_PASS_ONLY)",
            cached.class_name, cached.method_name, cached.method_descriptor,
        );
    }
    let ir_admitted = optimize
        && !ir_refused_before
        && !single_pass_pinned
        // Round 13 wave 4 (lane sync2): an admitted synchronized instance
        // method stays on the single-pass tier, whose body locks itself
        // (`single_pass_self_lock_preferred`; round 13 wave 6, lane sync4:
        // unless this VM's backend already refused that body; round 13 wave
        // 10, lane sync6: a `static synchronized` one too, given its class's
        // mirror slot).
        && !single_pass_self_lock_preferred(
            req.verdicts(),
            cached,
            code,
            code_len,
            admission.door(),
            req.self_lock_mirror_slot,
        )
        // IR lowering has no exact-RBP or safepoint-map publication, so a
        // mapless IR frame must never be live while the young collector
        // RELOCATES. The question this gate has to ask is therefore whether
        // relocation can actually observe a compiled frame — NOT whether the
        // moving-young flag happens to be set.
        //
        // Keyed on the flag alone it fired on every default build
        // (`DEFAULT_MOVING_YOUNG` is `true`), switching the optimizing tier off
        // in exchange for a frame that cannot occur: the runtime vetoes
        // moving-young whenever an un-rewritable compiled frame is live, so a
        // relocating cycle never sees one. See
        // `jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md`
        // and `moving_young_relocates_compiled_frames` for the invariant.
        //
        // This scopes the gate; it does not remove it. When the JIT publishes a
        // real relocation contract, `JIT_PUBLISHES_RELOCATION_CONTRACT` flips,
        // the runtime veto lifts and this gate re-arms together with it — and
        // `moving_young_disables_optimizing_tier` then announces itself again.
        && !moving_young_disables_optimizing_tier()
        // The byte budget applies to every optimizing door, not just OSR.
        && {
            let ok = ir::ir_compatible_sized(&scan, code_len);
            ir_compatible_refused.set(!ok);
            ok
        }
        // PERF-01. The single-pass backend has three bulk-byte loop lowerings
        // the IR tier does not: it replaces a scalar `boolean[]`/`byte[]`
        // element loop with a vectorised pre-header. Where those fire, an IR
        // body is a DOWNGRADE, not an optimization.
        //
        // Measured, not assumed: `CratonBench.sieve([ZI)I` went 2,462 ms ->
        // 15,823 ms (6.4x) the day `cov-02` taught `IrBuilder::build` to lower
        // `bastore`, because the method stopped falling through to the backend
        // that vectorises it. CratonVM had been FASTER than HotSpot C2 on that
        // phase. Nothing else in CratonBench moved, and the checksum never
        // changed — it was purely a worse body for the same answer.
        //
        // This is the narrow fix, and it is deliberately narrow: it asks the
        // single-pass backend's OWN detectors, through the one function the
        // emission path also calls, so it can never veto a method that backend
        // would not actually vectorise. The general problem it is a special
        // case of — the optimizing tier replaces a C1 body whenever it CAN,
        // with no evidence the replacement is faster, and every `cov-*` lane
        // widens the set of methods that happens to — is the open policy
        // question kept in perf-01-sieve-ir-body-6x-slower-than-c1.md, and is
        // not solved here.
        //
        // The `sieve` REGRESSION that brief was opened for is FIXED (closeout:
        // perf-01-sieve-ir-body-slower-than-c1-FIXED-20260804.md) — by this
        // veto — and re-verified 2026-08-17 on CratonBench: checksum `9592` on
        // HotSpot 25 and on CratonVM, at parity rather than 6.4x
        // (8.2-10.0 s against HotSpot's 8.9 s on the same host). Only the
        // policy question above is still open; do not read the brief's
        // measurement as current.
        && single_pass_only_lowering_for(code, code_len, cached).is_none()
        // Same species as PERF-01 directly above, and measured the same way:
        // where a `java/lang/String` access intrinsic fires, an IR body is a
        // DOWNGRADE. Every call-site intrinsic is registered in ONE place —
        // the single-pass invoke loop at the end of this function — and the IR
        // lowerer binds direct calls to real function addresses only, so it has
        // no route to an intrinsic sentinel at all. A method that tiers up
        // therefore SILENTLY LOSES its intrinsics.
        //
        // Measured on this branch, `java/nio/StringCharBuffer.get()C`
        // (`return str.charAt(nextGetIndex() + offset)`, the inner loop of
        // Tomcat's `Utf8Encoder.encodeNotHasArray` and so of every WebSocket
        // text frame the server sends):
        //
        //     C2/IR body, intrinsic dropped   504 ns/call
        //     C1 body, intrinsic emitted      135 ns/call   (3.7x)
        //
        // The delta is that large because the intrinsic does not replace one
        // call with one instruction — it replaces the whole
        // `charAt → isLatin1 → StringLatin1.charAt → String.checkIndex →
        // Preconditions.checkIndex` chain, whose tail is a registered NATIVE,
        // with an inline coder-branch decode. No scheduling the optimizing tier
        // can do recovers that.
        //
        // Deliberately narrow, exactly like PERF-01: only the String family,
        // and only when a layout actually resolved (without one the single-pass
        // backend would not emit the intrinsic either, so pinning would cost a
        // C2 body and buy nothing). The general problem — the optimizing tier
        // replaces a C1 body whenever it CAN, with no evidence the replacement
        // is faster — is the one PERF-01 already names. Opt out with
        // `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` to A/B one binary against
        // itself.
        //
        // The predicate moved from a bare `has_string_intrinsic_site` to
        // `string_intrinsic_pin_declines`, which is the same test plus two
        // things it could not do: it COUNTS (so "the pin never fires" stops
        // being a belief — the A/B that motivated this reads 326.3 ns/char with
        // the pin off against 329.5 with it on, and a null result like that is
        // exactly what a pin that never engages looks like), and it FAILS CLOSED
        // when the door supplied no constant-pool invoke resolver. It shares its
        // classification with the printed verdict above, so the reason a run
        // reports and the decision a run takes cannot drift apart again.
        //
        // Asked THROUGH the admission token since 2026-09-01, rather than as a
        // free call. The decision is the same function and the same answer;
        // what the token adds is that the question is now a PER-DOOR fact, and
        // that a door which never asks it is a number instead of an inference.
        // `fired=0` on `probes/CharAtWarmShape.java` — the workload this pin
        // exists to govern, and one the OSR door compiles — is what a one-door
        // counter looks like from three doors away. See
        // `string_intrinsic_pin_enabled` for the three-arm measurement and
        // `compile_gate`'s "installed at ONE door" section for the topology.
        // JIT round 21 wave 20 (lane sbfuse, STRINTR21-4): minus the builder
        // sites of a chain the optimizing tier fuses into one concat
        // (`invoke_ops_for_string_pin`; `CRATONVM_JIT_IR_SB_CHAIN_FUSE=0`
        // weighs every site again).
        && !admission.string_intrinsic_pin_declines(
            &invoke_ops_for_string_pin(
                req,
                code,
                code_len,
                &string_pin_invoke_ops(code, code_len, &scan.invoke_ops),
            ),
            cp_invoke_resolver,
            resolved_string_layout,
        )
        // Round 9 wave 9 (`evict9`): the same species for the exact-`ArrayList`
        // `get`/`size` prefix, which only the single-pass backend emits. See
        // `arraylist_prefix_pins_single_pass` for the measurements;
        // `CRATONVM_JIT_NO_ARRAYLIST_PIN=1` opts out.
        && !arraylist_prefix_pins_single_pass(
            &scan.invoke_ops,
            cp_invoke_resolver,
            resolved_string_layout,
        )
        // JIT round 18 wave 9 (lane osrpoly) declined the IR attempt here for
        // an OSR-door compile whose in-loop polymorphic site the single-pass OSR
        // body splices (`CRATONVM_JIT_OSR_PREFER_SPLICED_SINGLE_PASS`). Since
        // round 18 wave 11 (lane osrfollow, OSRPOLY18-4 / OSRFOLLOW18-4) the VM's
        // optimizing OSR door asks that BEFORE it compiles, with the single-pass
        // planner's own gates (`osr_door_single_pass_polymorphic_splice_pc`,
        // from `jit_bridge.rs` `osr_optimizing_build_declined`), and every
        // OSR-door compile passes that door first. The profile-only question
        // that stood here named sites the planner then refused, so it is gone:
        // one decision, made where the planner's inputs (entry pc, resolvers)
        // exist.
        // STUB-S8 (was: `cached.exception_table.is_empty()`) — the optimizing
        // tier used to refuse EVERY method with a `try`/`catch`, which is an
        // enormous population of ordinary Java and cost ~7x on each of them
        // (measured: a clone of Tomcat's `CharsetCache.getCharset` at 8252
        // ns/op, 1094 with only the try/catch deleted). The stated reason was
        // that the IR builder walked handler bytecode with stale
        // `ctrl`/`locals`/`stack` and emitted orphan nodes referencing
        // `NO_NODE` that panicked `ir_lower::slot_of`. `IrBuilder::build` now
        // skips handler bodies outright (they are unreachable in a compiled
        // frame — see the STUB-S8 comment there), so the orphans cannot arise
        // and the table itself is no longer disqualifying.
        //
        // What still is: `precise_exception_frames`. When RBC.6 fires (a
        // handler reads a non-parameter local) the single-pass backend keeps
        // its promise by publishing a reason-9 frame at every throwing site, so
        // the interpreter RESUMES mid-method with that local intact. Methods
        // below that line (handler reads only parameters, or no handler local
        // at all) exit exceptionally through the identical `i64::MIN` sentinel
        // + epilogue protocol in both tiers, so moving them to C2 changes code
        // quality only.
        //
        // Round 11 wave 10 (lane irexc): the IR lowerer now has the
        // equivalent — a reason-9 rethrow pad at every call-exception exit in
        // a protected range (`Lowerer::push_rethrow_pad`) — so under
        // `CRATONVM_JIT_IR_PRECISE_HANDLER_FRAMES=1` (`ir_precise_frames`) the
        // precise-frame population is admitted too, javac `synchronized`
        // blocks first. With the switch off this conjunct is exactly the old
        // `!precise_exception_frames`.
        //
        // `CRATONVM_JIT_NO_EXC_TABLE_C2=1` restores the old blanket exclusion.
        // It exists so one binary can be A/B'd against its own pre-change
        // behaviour — the only rigorous control, since comparing against a
        // separately built branch confounds this change with everything else
        // that landed — and as an escape hatch if a workload ever regresses.
        && !(exc_table_c2_disabled() && !cached.exception_table.is_empty())
        && (!precise_exception_frames || ir_precise_frames)
        // An inline trap inside a protected range that this tier lowers to an
        // unresumable deopt — see `ir_unresumable_protected_trap`. Falls
        // through to the single-pass backend, which throws and routes through
        // the exception table instead of deopting.
        //
        // WAIVED for the precise-frame population (`ir_precise_frames`), and
        // not unconditionally: every javac `synchronized` block commits a side
        // effect in its range (its own `monitorexit`), so this gate alone
        // refused every block that reads a field. The premise it encodes —
        // "such a guard deopt can only be replayed from entry" — is CHECKED
        // after lowering instead (`ir_precise_frames_trap_post_check` in
        // `ir_tier`), which discards the artifact unless every guard exit
        // resumes precisely. Short-circuited, so the waived methods are not
        // counted in the gate's census.
        && (ir_precise_frames
            || !ir_unresumable_trap_declines(
                code,
                code_len,
                &cached.exception_table,
                cached.is_static,
            ))
        // The value-shape term, via `ir_value_shape_admitted` (the one
        // spelling shared with `c2_upgrade_would_engage` and the verdict
        // above). Its three disjuncts, and why each is sound:
        //
        // (1) no category-2 value and no FP anywhere: the original IR shape.
        //
        // inc 30: the pure int/long/ref IR path stays FP-free, so a
        // float-using (cat-1) method is no longer admitted here — it
        // routes through the FP clause below (or bails to single-pass
        // when the FP gate is off, exactly as it does today).
        //
        // (2) `ir_emit_long` and FP-free.
        //
        // inc 25/30(ldiv): admit a long-using method when the long gate is
        // on, as long as it is double/float-free. (inc 25 also required
        // int-div/rem-free, because a `long` live at a div guard's deopt
        // could not be reconstructed; long deopt-resume now makes `long` a
        // real `FrameValue` width — `StackSlotLong`/`Long` → `Value::Long`,
        // the locals mapper collapsing the cat-2 two-slot snapshot — so a
        // `long` may now be live at an `idiv`/`irem`/`ldiv`/`lrem` deopt.
        // The precise resume reconstructs it; an unmappable frame falls back
        // to the safe whole-method re-run.)
        //
        // (3) `ir_emit_fp` and an FP opcode in the body.
        //
        // inc 30: admit a float/double-using method when the FP gate is on.
        // The VM uses the COMPACT all-GPR i64 ABI (`execute_jit_call` /
        // `jit_invoke_dispatch` marshal each FP value as `to_bits() as i64`
        // into an INTEGER arg register, NOT XMM), so NOTHING about FP
        // params/returns/call-args needs XMM register marshalling:
        //   - PARAMS (inc 34): an FP param arrives as bits in a GPR; the
        //     prologue stores it to the param slot like any other param, and
        //     `ir_param_types`/`set_param_types` already type it Float/Double
        //     (the cat-2 two-slot layout for `D`, as for `J`), so a later
        //     `dload`/`fload` reads the slot via `fp_load`. The whole gate is
        //     now FP-signature-agnostic — `fp_in_body` identifies FP methods.
        //   - RETURNS (inc 32/33): the FP result's bits ride RAX (interpreter
        //     reads `result as u64`/`as u32` → `from_bits`).
        //   - CALL-ARGS (inc 34): `static_call_shape` admits `D`/`F` args
        //     (one GPR slot each); the marshaller stores the slot bits to the
        //     staging region and `decode_dispatch_values` reads them back.
        // Slice C lifted the int-div exclusion: the IR deopt resume now
        // reconstructs FP slots (`StackSlotFloat`/`StackSlotDouble` →
        // `Value::Float`/`Value::Double`, `Double` cat-2 like `Long`), so an
        // FP value live at an `idiv`/`irem` div-by-zero deopt is restored
        // precisely rather than stranded. inc 35 lifted the `ldc2_w`
        // exclusion: the resolver reports `is_double`, so the builder lowers a
        // `double` constant to `dconst` (a `long` ldc2_w stays `lconst`) —
        // double literals (`1.5`, `3.14`, …) no longer bail.
        && ir_value_shape_admitted(
            code,
            code_len,
            &cached.method_descriptor,
            ir_emit_long,
            ir_emit_fp,
        );
    // The admission funnel: every optimizing request that got past the refusal
    // memo is counted at exactly one exit, so the population the IR tier never
    // sees is a number rather than an inference. See `ir_evidence::AdmissionExit`.
    if optimize && !ir_refused_before && !ir_admitted {
        ir_evidence::note_admission_exit(if ir_compatible_refused.get() {
            ir_evidence::AdmissionExit::IrCompatible
        } else {
            ir_evidence::AdmissionExit::OtherGate
        });
    }
    if ir_admitted {
        // Stage: the optimizing tier. See `ir_tier`.
        //
        // Bound to a `let` rather than tested inline so the `&scan` borrow
        // provably ends on this statement: the arm below MOVES `scan` into the
        // parity shadow compile, and an `if let` scrutinee extends its
        // temporaries over the whole arm.
        let enclosing_exit = ir_evidence::arm_ir_exit_mark();
        let optimizing = ir_tier(
            req,
            admission,
            &metrics,
            code,
            code_len,
            &scan,
            prologue_param_slots,
            optimize,
            precise_exception_frames,
            resolved_string_layout,
            ir_method_hash,
            ir_refusal_key,
            ir_refusal_ticket,
            ir_refused_before,
        );
        let declined_at = ir_evidence::take_ir_exit_mark(enclosing_exit);
        ir_evidence::note_admission_exit(if optimizing.is_some() {
            ir_evidence::AdmissionExit::Published
        } else {
            declined_at
        });
        if let Some(compiled) = optimizing {
            // Round 15 wave 4 (lane sync3, SY2-6): an optimizing self-locking
            // body handed to the VM to publish.
            if compiled.self_locks_monitor {
                ir::note_sync_splice_census(ir::SYNC_CENSUS_IR_SELF_LOCK_ACCEPTED);
            }
            // Backend parity, diagnostic and default OFF. The optimizing tier
            // just produced the body that is about to be installed; with
            // `CRATONVM_JIT_BACKEND_PARITY=1` we additionally compile the same
            // method with the single-pass backend, compare what the two bodies
            // CONTAIN, and drop the second one. `scan` is moved in because this
            // arm returns — the single-pass call below is unreachable from
            // here. Nothing about the installed artifact changes; see
            // `backend_parity_shadow_compile` for the containment and for the
            // limits of what this can compare.
            if backend_parity_enabled() {
                backend_parity_shadow_compile(
                    req,
                    self_call_identity_stable,
                    admission,
                    local_handlers_disarmed,
                    code,
                    code_len,
                    scan,
                    prologue_param_slots,
                    precise_exception_frames,
                    resolved_string_layout,
                    &compiled,
                );
            }
            return Some(compiled);
        }
        // Round 15 wave 1 (lane sync, SS8-2): the optimizing self-locking body
        // was not published; the single-pass tier below builds the
        // self-locking body, and later compiles go straight to it.
        if ir_self_lock_candidate(
            req.verdicts(),
            cached,
            code,
            code_len,
            admission.door(),
            req.self_lock_mirror_slot,
        ) {
            // Round 15 wave 4 (lane sync3, SY2-6): whatever declined it.
            ir::note_sync_splice_census(ir::SYNC_CENSUS_IR_SELF_LOCK_FELL_THROUGH);
            note_ir_self_lock_refused(req.verdicts(), cached);
        }
    }
    // Stage: the single-pass tier. See `single_pass_tier`.
    let single_pass = single_pass_tier(
        req,
        backend_attempted,
        self_call_identity_stable,
        admission,
        local_handlers_disarmed,
        &metrics,
        code,
        code_len,
        scan,
        prologue_param_slots,
        precise_exception_frames,
        resolved_string_layout,
    );
    // JIT round 23 wave 4 (lane L1): the single-pass frame, for a later
    // optimizing compile's frame evidence (`ir_selfrec_frame_evidence`).
    if let Some(cm) = single_pass.as_ref() {
        req.verdicts()
            .note_sp_frame_size(ir_refusal_key, cm.osr_frame_size);
    }
    single_pass
}



/// Has the call site at `pc` of `method_key` been withdrawn as a whole
/// (a stored [`deopt::SPECULATION_ID_ANY`] at that bci)?
///
/// A guard inside a spliced callee publishes an inlined chain, and every chain
/// sink charges its trap at the OUTERMOST scope's bci -- the call site -- and,
/// after four charges, records the wildcard there (`deopt_resume.rs`: the
/// doors' chain arm and `charge_osr_chain_guard_exit`). The spliced guard
/// itself publishes in the CALLEE's bci space, so no guard-level consumer ever
/// reads that entry: without this question the recompile splices the same
/// body, the guard fails again, and the charges alone retire the method. The
/// planner leaves such a site as the ordinary call; the callee's own compiled
/// body then traps under its own name, where the speculation lives.
///
/// `contains_speculation(.., SPECULATION_ID_ANY)` is true exactly for a stored
/// wildcard (a derived id is never 0), so a NAMED withdrawal at the invoke bci
/// (the receiver guard's `(bci, UncommonTrap)`, the trap miss) keeps the
/// splice and is honoured by its own consumer instead.
pub(crate) fn ir_splice_site_withdrawn(
    despec: Option<&deopt::DespecRegistry>,
    method_key: &str,
    pc: usize,
) -> bool {
    let Ok(bci) = u32::try_from(pc) else {
        return false;
    };
    despec.is_some_and(|registry| {
        registry.contains_speculation(method_key, bci, deopt::SPECULATION_ID_ANY)
    })
}

/// Round nio20 wave 4 (lane L3, W3L4-4 of `jit-r20-nio-proposals-w3-L4.md`):
/// `CRATONVM_JIT_IR_NESTED_TRAP_OWN_CHARGE` (perf, **default ON**; `0` keeps
/// the wildcard). A nested trap guard of a spliced body
/// (`CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP`: interface and nest-exact
/// virtual rows) that keeps firing is charged at
/// the outer call site under a NAMED id ([`nested_trap_speculation_id`] of the
/// innermost scope's key and its invoke bci; the VM's chain sink,
/// `deopt_resume.rs` `nested_trap_charge_id`), and the planner then turns that
/// one nested row back into its call ([`ir_splice_strip_withdrawn_nested_traps`])
/// instead of losing the whole outer splice to the wildcard
/// ([`ir_splice_site_withdrawn`]). Twice the trap limit still records the
/// wildcard (the sink's backstop). Read per trap and per planned site.
pub fn ir_nested_trap_own_charge_enabled() -> bool {
    true
}

/// W3L4-4: the named speculation id a nested trap guard at bci `bci` of the
/// spliced body `scope_key` (`"class.name:descriptor"`, the chain scope's
/// `method_key`) is charged under at its outermost call site. A domain-tagged
/// FxHash (deterministic across runs and processes), never
/// [`deopt::SPECULATION_ID_ANY`]. The VM's chain sink and
/// [`ir_splice_strip_withdrawn_nested_traps`] derive it from the same pair.
pub fn nested_trap_speculation_id(scope_key: &str, bci: u32) -> u32 {
    use std::hash::Hasher;
    let mut h = FxHasher::default();
    h.write(b"nested-trap\0");
    h.write(scope_key.as_bytes());
    h.write_u32(bci);
    // Truncating: a 64-bit hash to the field's 32 bits.
    let id = h.finish() as u32;
    if id == deopt::SPECULATION_ID_ANY {
        1
    } else {
        id
    }
}

/// W3L4-4: drop, at every depth of `site`'s nested tree, each exact-receiver
/// row (`guard_class_id != 0`, the rows a nested trap guard is built for)
/// whose named nested-trap speculation ([`nested_trap_speculation_id`] of the
/// enclosing body's key and the row's `callee_pc`) is withdrawn at
/// `(method_key, pc)` -- the outer call site the chain sink charged. The pc
/// keeps the virtual / interface call row it always had, exactly as
/// [`ir_splice_strip_unadmitted_exact_nested`] leaves it. Returns how many
/// rows were dropped.
pub(crate) fn ir_splice_strip_withdrawn_nested_traps(
    site: &mut InlineSite,
    despec: &deopt::DespecRegistry,
    method_key: &str,
    pc: usize,
) -> usize {
    let Ok(bci) = u32::try_from(pc) else {
        return 0;
    };
    let scope_key = format!(
        "{}.{}:{}",
        site.class_name, site.method_name, site.descriptor
    );
    let mut dropped = 0usize;
    let nested = std::mem::take(&mut site.nested_sites);
    let mut kept = Vec::with_capacity(nested.len());
    for mut n in nested {
        if n.guard_class_id != 0 {
            if let Ok(nested_bci) = u32::try_from(n.callee_pc) {
                let id = nested_trap_speculation_id(&scope_key, nested_bci);
                if despec.contains_speculation(method_key, bci, id) {
                    dropped += 1;
                    continue;
                }
            }
        }
        dropped += ir_splice_strip_withdrawn_nested_traps(&mut n.site, despec, method_key, pc);
        kept.push(n);
    }
    site.nested_sites = kept.into();
    dropped
}


/// The optimizing (IR) tier of [`try_compile_inner`]: build the graph, optimize
/// it, schedule, allocate and lower it.
///
/// `Some` is an installed-ready artifact and the driver returns it. `None` means
/// the tier declined (an unbuildable graph, a verifier rejection, a lowerer
/// bail); the driver then falls through to [`single_pass_tier`]. The inputs are
/// what the admission and scan stages above produced, passed by value or
/// shared reference so the stage can neither see nor change anything else.
///
/// Round 12 wave 6 (lane iropt5, proposal W5-2): a build the builder refuses
/// INSIDE a spliced body no longer costs the method. [`ir_tier_attempt`]
/// reports the top-level site the refusal belongs to
/// (`IrBuilder::set_splice_refusal_sink`, [`ir_splice_rebuild_site`]), and the
/// tier runs once more with that site left as the ordinary call, up to
/// [`IR_SPLICE_REBUILD_MAX`] times. Dropping a splice is always sound: it is
/// exactly what the planner does for every site a splice rule refuses, and the
/// call it leaves keeps its own `invoke_info` row and inline cache. Every
/// planner/builder disagreement (the store fence W5-1 cannot see, a receiver
/// guard inside an impure nested splice, a body the walk refuses for any other
/// reason) then costs one splice and one extra build instead of the optimizing
/// compile. `CRATONVM_JIT_IR_SPLICE_REBUILD=0` restores the old answer.
///
/// What a refused attempt leaves behind is per-compile and is rebuilt or
/// dropped: its builder, plan tables, inline-cache boxes and evidence scope
/// die with it. Two process-visible traces remain and are harmless: the
/// bailout census counts the refused build (it was one), and the compile
/// report keeps that bailout reason even when the rebuild publishes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_tier(
    req: &CompileRequest<'_>,
    admission: &compile_gate::CompileAdmission,
    metrics: &metrics::CompileRecorder,
    code: &[u8],
    code_len: usize,
    scan: &x64::JitScanResult,
    prologue_param_slots: usize,
    optimize: bool,
    precise_exception_frames: bool,
    resolved_string_layout: Option<StringFieldLayout>,
    ir_method_hash: u64,
    ir_refusal_key: u64,
    ir_refusal_ticket: ir_evidence::IrRefusalTicket,
    ir_refused_before: bool,
) -> Option<CompiledMethod> {
    let mut skip: Vec<usize> = Vec::new();
    // JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-2): an OSR-door compile
    // under `CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR` builds without the door's own
    // every-header opaque merges first; see `osr_narrow_opaque_door_enabled`
    // for why that is sound. Cleared (once) when that body's guard exits turn
    // out to need opaque merges, and the next attempt is 44df838b5f's body.
    let mut osr_narrow_opaque = req.osr_mode_exit_polls && osr_narrow_opaque_door_enabled();
    // JIT round 19 wave 5 (lane handler5, SPLICE4-1): compiled handler entries
    // walk handler bytecode the build used to skip, so a handler the builder
    // cannot lower (or a lowering refusal they cause) would cost the method its
    // optimizing compile. One retry without them restores today's attempt.
    let mut catch_arms = ir::ir_compiled_handler_entry_enabled();
    // JIT round 19 wave 6 (lane osr6, OSR5-2,
    // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PREDICT`): a lowering that refused a
    // promoted loop's deopt write-back (`promoted field cells: ...`) costs one
    // more attempt, built without such promotions
    // (`ir::Graph::promote_pending_withheld`), instead of the method.
    let mut promote_pending_withheld = false;
    // JIT round 21 (lane lambda3, `CRATONVM_JIT_IR_LAMBDA_IMPL_SPLICED_CAPTURES`):
    // an attempt refused while a lambda impl-call plan was in force is retried
    // once without the plan (the `CRATONVM_JIT_IR_LAMBDA_IMPL_CALL=0` body),
    // after every other retry has had its turn.
    let mut lambda_plan_withheld = false;
    // j27 wave 4 (lane SPLICEFFM2): the one re-plan without the splices whose
    // `MemorySegment` element calls a build declined to expand.
    let mut ffm_replanned = false;
    loop {
        let mut rebuild_without: Option<usize> = None;
        let mut catch_arms_staged = false;
        let mut lambda_plan_built = false;
        let mut ffm_splice_declined: Vec<usize> = Vec::new();
        // OSR5-2: the retry below reads this attempt's lowering refusal only
        // (under its switch; off, nothing here changes).
        // JIT round 21 (lane promote, TYPECHECK21-9): and under
        // `CRATONVM_JIT_IR_LOOP_PROMOTE_DEOPT_PADS`, whose static / fresh pads
        // are refused by the same `promoted field cells: ...` bails.
        if ir_lower::ir_loop_field_promote_predict_enabled()
            || ir_lower::ir_loop_promote_deopt_pads_enabled()
        {
            ir_lower::clear_lower_bail();
        }
        let compiled = ir_tier_attempt(
            req,
            admission,
            metrics,
            code,
            code_len,
            scan,
            prologue_param_slots,
            optimize,
            precise_exception_frames,
            resolved_string_layout,
            ir_method_hash,
            ir_refusal_key,
            ir_refusal_ticket,
            ir_refused_before,
            osr_narrow_opaque,
            &skip,
            &mut rebuild_without,
            catch_arms,
            &mut catch_arms_staged,
            promote_pending_withheld,
            lambda_plan_withheld,
            &mut lambda_plan_built,
            &mut ffm_splice_declined,
        );
        if compiled.is_none() && !ffm_replanned && !ffm_splice_declined.is_empty() {
            ffm_replanned = true;
            ffm_splice_rebuild_skip(&mut skip, ffm_splice_declined);
            continue;
        }
        if compiled.is_none() && catch_arms_staged {
            if x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-catch-arm retry without compiled handler entries in {}.{}{}",
                    req.cached.class_name, req.cached.method_name, req.cached.method_descriptor,
                );
            }
            catch_arms = false;
            continue;
        }
        if let Some(cm) = compiled {
            // The narrowed body has a speculative guard exit, sound only
            // behind an opaque merge at every entry: the door could never
            // enter it as built. Drop it unpublished (as the acceptance gate
            // drops a refused body) and build the every-header-opaque body.
            if osr_narrow_opaque && cm.ir_osr_guard_fact_needs_opaque_entries {
                if x64::dbg_jitc_enabled() || ir_stage_reporting() {
                    eprintln!(
                        "[cratonvm-jitc] osr-door {}.{}{}: narrowed body has speculative guard \
                         exits -- rebuilding with opaque merges at every header \
                         (CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR)",
                        req.cached.class_name, req.cached.method_name, req.cached.method_descriptor,
                    );
                }
                drop(cm);
                osr_narrow_opaque = false;
                continue;
            }
            // JIT round 19 proposal wave (lane osr19, OSRFOLLOW18-1): the
            // narrowed body's guard exits are exact, but the header the door
            // fired at (`req.osr_requested_bci`) got no entry (a value live
            // there that no local names, usually its memory phi). The
            // every-header-opaque body (44df838b5f's) enters there, so the
            // narrowed door would otherwise enter fewer loops than the
            // always-opaque one. Opt-in; see
            // `osr_narrow_door_entry_rebuild_enabled`.
            if osr_narrow_opaque
                && osr_narrow_door_entry_rebuild_enabled()
                && narrowed_body_lacks_door_entry(&cm, req.osr_requested_bci)
            {
                if x64::dbg_jitc_enabled() || ir_stage_reporting() {
                    eprintln!(
                        "[cratonvm-jitc] osr-door {}.{}{}: narrowed body has no entry at the \
                         door's header pc={} -- rebuilding with opaque merges at every header \
                         (CRATONVM_JIT_OSR_NARROW_DOOR_ENTRY_REBUILD)",
                        req.cached.class_name,
                        req.cached.method_name,
                        req.cached.method_descriptor,
                        req.osr_requested_bci.unwrap_or(usize::MAX),
                    );
                }
                drop(cm);
                osr_narrow_opaque = false;
                continue;
            }
            return Some(cm);
        }
        if !promote_pending_withheld
            && (ir_lower::ir_loop_field_promote_predict_enabled()
                || ir_lower::ir_loop_promote_deopt_pads_enabled())
            && ir_lower::last_lower_bail().is_some_and(ir_lower::is_promoted_cells_lower_bail)
        {
            if x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-promote-retry without deopt-pad promotions in {}.{}{} (CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PREDICT)",
                    req.cached.class_name, req.cached.method_name, req.cached.method_descriptor,
                );
            }
            // Wave 7 (lane osr7, OSR6-2): the predict switch's second layer.
            ir_check_elim::note_promote_census(ir_check_elim::PROMOTE_CENSUS_RETRIED, 1);
            promote_pending_withheld = true;
            continue;
        }
        let Some(site) =
            rebuild_without.filter(|pc| !skip.contains(pc) && skip.len() < IR_SPLICE_REBUILD_MAX)
        else {
            if lambda_plan_built
                && !lambda_plan_withheld
                && ir_lambda_impl_spliced_captures_enabled()
            {
                if x64::dbg_jitc_enabled() || ir_stage_reporting() {
                    eprintln!(
                        "[ir] lambda-impl-call retry without the plan in {}.{}{} \
                         (CRATONVM_JIT_IR_LAMBDA_IMPL_SPLICED_CAPTURES)",
                        req.cached.class_name, req.cached.method_name, req.cached.method_descriptor,
                    );
                }
                lambda_plan_withheld = true;
                continue;
            }
            return None;
        };
        if x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-splice-rebuild without the site at pc={site} in {}.{}{}",
                req.cached.class_name, req.cached.method_name, req.cached.method_descriptor,
            );
        }
        skip.push(site);
    }
}

/// JIT round 19 wave 5 (lane handler5, SPLICE4-1): the method's exception
/// table for [`ir::IrBuilder::set_catch_arm_table`], or `None` when a catch
/// type does not resolve.
pub(crate) fn ir_catch_arm_rows(
    cached: &CachedBytecodeMethod,
    cp_class_name_resolver: Option<&dyn Fn(u16) -> Option<String>>,
) -> Option<Vec<ir::CatchArmRow>> {
    // JIT round 19 wave 5 (lane handler5, SPLICE4-1): the whole table or
    // nothing, as the single-pass tier stages its local-handler table: a site
    // built from a partial table could answer for a row out of order. A
    // catch-all (`catch_type == 0`) carries the empty name.
    let mut rows = Vec::with_capacity(cached.exception_table.len());
    for e in cached.exception_table.iter() {
        let catch_name: &'static str = if e.catch_type == 0 {
            ""
        } else {
            intern_catch_type_name(&cp_class_name_resolver?(e.catch_type)?)
        };
        rows.push(ir::CatchArmRow {
            start_pc: usize::from(e.start_pc),
            end_pc: usize::from(e.end_pc),
            handler_pc: usize::from(e.handler_pc),
            catch_name,
        });
    }
    Some(rows)
}

/// Round 15 wave 3 (lane chain3, CHN15-2 producer): `pc -> declaring class
/// id` of the compiling method's own `getfield` / `putfield` sites, for
/// [`ir::IrBuilder::set_field_decl_classes`].
///
/// A row is kept only where it certainly describes the field the builder
/// lowers at that pc: the pc has a `field_info` row (`fm`, from
/// `cp_field_resolver`) and the declaring-class resolver's own field index is
/// that row's. The resolver is asked once per constant-pool index, so every pc
/// naming one entry carries one class. A wrong class would let the alias model
/// call two accesses of ONE field disjoint (a miscompile); a missing row only
/// costs precision. Class id `0` (the "unknown" sentinel elsewhere) is dropped.
pub(crate) fn ir_field_decl_rows(
    field_ops: &[(usize, u16)],
    fm: &std::collections::HashMap<usize, (usize, u8)>,
    decl_of: &dyn Fn(u16) -> Option<(usize, u32)>,
) -> std::collections::HashMap<usize, u32> {
    let mut asked: std::collections::HashMap<u16, Option<(usize, u32)>> =
        std::collections::HashMap::new();
    let mut rows = std::collections::HashMap::new();
    for &(pc, cp_idx) in field_ops {
        let Some(&(field_index, _)) = fm.get(&pc) else {
            continue;
        };
        let answer = *asked.entry(cp_idx).or_insert_with(|| decl_of(cp_idx));
        if let Some((decl_index, decl)) = answer {
            if decl_index == field_index && decl != 0 {
                rows.insert(pc, decl);
            }
        }
    }
    rows
}


/// JIT round 16 wave 10 (lane indyir): one `invokedynamic` site's descriptor
/// and bridge site, asked per INSTRUCTION when the request carries the
/// per-instruction resolver (`CompileRequest::cp_invokedynamic_site_resolver`),
/// else of the CP-only `resolver` -- the answer every door had before.
pub(crate) fn resolve_indy_site(
    req: &CompileRequest<'_>,
    resolver: &dyn Fn(u16) -> Option<(String, usize)>,
    cp_idx: u16,
    pc: usize,
) -> Option<(String, usize)> {
    match req.cp_invokedynamic_site_resolver {
        Some(at) => at(cp_idx, pc),
        None => resolver(cp_idx),
    }
}

/// JIT round 22 wave 2 (lane throws, THROWS22-2): does the method have at
/// least one loop, and does every `invokedynamic` in `indy_ops` lie OUTSIDE
/// every textual loop range `[target, source]` of
/// [`bytecode_analysis::back_edges`]?
///
/// Sound as "not on any cycle": every cycle of the control-flow graph has, for
/// each of its pcs `p`, a backward edge whose source is above `p` and whose
/// target is at or below it (the walk around the cycle must come back down
/// past `p`), so a pc no textual range covers is on no cycle. An exception
/// handler that jumps back into its `try` closes its loop with a backward
/// `goto`, which is a back edge too. A method with no back edge answers
/// `false`: its cost IS the call, and a bare bridge there is CONCAT5-3's 8x.
///
/// JIT round 22 wave 3 (lane throws; item 3 of
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// a cycle can also be closed by an EXCEPTION edge, which `back_edges` does
/// not list -- a handler placed at or before the end of its own range (below
/// its `try`, or inside it, as `synchronized`'s self-covering row is) can be
/// re-entered from the range it protects without any backward branch. Each
/// such row contributes the range `[min(handler, start), end)` as a loop too
/// (non-javac producers; javac emits only the self-covering row). A row whose
/// handler lies past its range closes a cycle only through a backward branch,
/// which is already counted.
pub(crate) fn indy_sites_all_outside_loops(
    code: &[u8],
    code_len: usize,
    indy_ops: &[(usize, u16)],
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> bool {
    let mut ranges = bytecode_analysis::back_edges(code, code_len);
    for row in exception_table {
        let (start, end, handler) = (
            usize::from(row.start_pc),
            usize::from(row.end_pc),
            usize::from(row.handler_pc),
        );
        if handler < end && end > 0 {
            ranges.push((handler.min(start), end - 1));
        }
    }
    !ranges.is_empty()
        && indy_ops.iter().all(|&(pc, _)| {
            !ranges
                .iter()
                .any(|&(target, source)| target <= pc && pc <= source)
        })
}

/// JIT round 20 wave 11 (lane pIndy, W10L3-3 / W11L3-3 of
/// `jit-r21-nio-proposals-w10-L3.md` / `-w11-L3.md`): the method's own
/// non-capturing lambda sites whose singleton slot is published
/// (`words(bridge site) != 0`), as `pc -> (bridge site, slot address)`, and --
/// only when EVERY `invokedynamic` of the method is such a site -- the stack
/// effects of all of them (`Some`), so a method whose only calls are these
/// sites gets its shapes; otherwise `None`, which leaves whatever shapes the
/// invoke plan gave (or none, and the same bail as before). This never makes
/// the builder plant a trap the default arm would not have planted.
///
/// A site qualifies on its stack effect alone: no arguments, a reference
/// result, a non-zero bridge site (the miss edge calls it) and a published
/// word. Per instruction: the descriptor and site come from
/// [`resolve_indy_site`], as every door asks.
pub(crate) fn ir_indy_lambda_word_plan(
    req: &CompileRequest<'_>,
    resolver: &dyn Fn(u16) -> Option<(String, usize)>,
    words: &dyn Fn(usize) -> usize,
    indy_ops: &[(usize, u16)],
) -> (
    HashMap<usize, (usize, usize)>,
    Option<HashMap<usize, (usize, u8)>>,
) {
    let mut rows: HashMap<usize, (usize, usize)> = HashMap::new();
    let mut shapes: HashMap<usize, (usize, u8)> = HashMap::new();
    for &(pc, cp_idx) in indy_ops {
        let Some((descriptor, bridge_site)) = resolve_indy_site(req, resolver, cp_idx, pc) else {
            continue;
        };
        let (args, ret) = (count_param_slots(&descriptor), return_type(&descriptor));
        shapes.insert(pc, (args, ret));
        if args != 0 || ret != b'L' || bridge_site == 0 {
            continue;
        }
        let slot = words(bridge_site);
        if slot != 0 {
            rows.insert(pc, (bridge_site, slot));
        }
    }
    let every = !rows.is_empty() && rows.len() == indy_ops.len();
    (rows, every.then_some(shapes))
}

/// t1 wave 11, lane JLAM (LAMBDATRAP21-4;
/// `CRATONVM_JIT_IR_INDY_LATE_LAMBDA_BRIDGE`): the method's own
/// NON-CAPTURING `LambdaMetafactory` sites whose singleton is NOT published
/// yet (`is_lambda` says lambda, `published` says no word), as `pc -> bridge
/// site`, and -- only when EVERY `invokedynamic` of the method is such a site,
/// a published-word site an earlier block built (`published_covered`), or a
/// pc in `covered_elsewhere` (the capture-new rows) -- the stack effects of
/// all of them (`Some`), as [`ir_indy_lambda_word_plan`] answers for its rows.
/// `site_of(cp index, pc)` is [`resolve_indy_site`]. A published site is
/// never a row here: it belongs to the published-word block, whose kill
/// switch this plan must not override.
pub(crate) fn ir_indy_late_lambda_bridge_plan(
    site_of: &dyn Fn(u16, usize) -> Option<(String, usize)>,
    is_lambda: &dyn Fn(usize) -> bool,
    published: &dyn Fn(usize) -> bool,
    published_covered: bool,
    covered_elsewhere: &std::collections::HashSet<usize>,
    indy_ops: &[(usize, u16)],
) -> (HashMap<usize, usize>, Option<HashMap<usize, (usize, u8)>>) {
    let mut rows: HashMap<usize, usize> = HashMap::new();
    let mut shapes: HashMap<usize, (usize, u8)> = HashMap::new();
    let mut covered = 0usize;
    for &(pc, cp_idx) in indy_ops {
        let Some((descriptor, bridge_site)) = site_of(cp_idx, pc) else {
            continue;
        };
        let (args, ret) = (count_param_slots(&descriptor), return_type(&descriptor));
        shapes.insert(pc, (args, ret));
        if covered_elsewhere.contains(&pc) {
            covered += 1;
            continue;
        }
        if args != 0 || ret != b'L' || bridge_site == 0 || !is_lambda(bridge_site) {
            continue;
        }
        if published(bridge_site) {
            if published_covered {
                covered += 1;
            }
            continue;
        }
        rows.insert(pc, bridge_site);
        covered += 1;
    }
    let every = !rows.is_empty() && covered == indy_ops.len();
    (rows, every.then_some(shapes))
}



/// JIT round 20 wave 12 (lane qLamNew, W10L3-2 / W11L3-2 second half / W9L2-1
/// part 2 of `docs/feature-designs/jit-ir-optimizations.md`;
/// `CRATONVM_JIT_IR_INDY_CAPTURE_NEW`): the method's own linked CAPTURING
/// lambda sites, as `pc -> (proxy class id, per-capture tags)`, and -- only when
/// EVERY `invokedynamic` of the method is such a site or a published-word site
/// (`words`, the lambda-word plan's own test, `None` when that plan is off) --
/// the stack effects of all of them (`Some`), exactly as
/// [`ir_indy_lambda_word_plan`] answers for its rows.
///
/// A site qualifies when it takes arguments, returns a reference, has a bridge
/// site, and the VM names its proxy (`captures`) with a capture count equal to
/// the descriptor's argument count; the tags are the descriptor's own
/// ([`indy_arg_type_tags`]: `I` / `J` / `F` / `D` / `L`), one per capture, in
/// field order -- the order `allocate_lambda_proxy_tlab_fast` stores them in.
pub(crate) fn ir_indy_capture_new_plan(
    req: &CompileRequest<'_>,
    resolver: &dyn Fn(u16) -> Option<(String, usize)>,
    captures: &dyn Fn(usize) -> Option<(u32, usize)>,
    words: Option<&dyn Fn(usize) -> usize>,
    indy_ops: &[(usize, u16)],
) -> (
    HashMap<usize, ir::IndyCaptureNew>,
    Option<HashMap<usize, (usize, u8)>>,
) {
    let mut rows: HashMap<usize, ir::IndyCaptureNew> = HashMap::new();
    let mut shapes: HashMap<usize, (usize, u8)> = HashMap::new();
    let mut covered = 0usize;
    for &(pc, cp_idx) in indy_ops {
        let Some((descriptor, bridge_site)) = resolve_indy_site(req, resolver, cp_idx, pc) else {
            continue;
        };
        let (args, ret) = (count_param_slots(&descriptor), return_type(&descriptor));
        shapes.insert(pc, (args, ret));
        if ret != b'L' || bridge_site == 0 {
            continue;
        }
        if args == 0 {
            // A published-word site (the lambda-word plan builds it).
            if words.is_some_and(|w| w(bridge_site) != 0) {
                covered += 1;
            }
            continue;
        }
        let Some((proxy_class_id, n)) = captures(bridge_site) else {
            continue;
        };
        let capture_tags = indy_arg_type_tags(&descriptor);
        if proxy_class_id == 0 || n != args || capture_tags.len() != args {
            continue;
        }
        rows.insert(
            pc,
            ir::IndyCaptureNew {
                proxy_class_id,
                capture_tags,
            },
        );
        covered += 1;
    }
    let every = !rows.is_empty() && covered == indy_ops.len();
    (rows, every.then_some(shapes))
}

/// JIT round 16 wave 10 (lane indyir, IN16-8): one constant-pool entry's
/// planned plain call (see [`ir_indy_direct_plan`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IrIndyDirectSite {
    /// The `invokedynamic` pcs of this entry (all answered the same call).
    pub(crate) pcs: Vec<usize>,
    pub(crate) class: String,
    pub(crate) name: String,
    pub(crate) desc: String,
    /// `3` (static) or `0` (unbound virtual).
    pub(crate) invoke_kind: u8,
    /// The target class: the call's class context.
    pub(crate) class_id: u32,
    /// JIT round 20 wave 11 (lane pIndy, SPLICE19-1): `Some` for a MUTABLE
    /// call site's planned target, built as a guarded call
    /// ([`ir::IrBuilder::set_indy_mutable_guards`]), never by the invoke arm.
    pub(crate) guard: Option<ir::IndyMutableGuard>,
    /// JIT round 21 wave 17 (lane records, IN16-6): `Some` for a record
    /// `ObjectMethods` site built as a field-wise fold
    /// ([`ir::IrBuilder::set_indy_record_folds`]), never by the invoke arm;
    /// the planned call is the fold's per-reference-component `Objects` call.
    pub(crate) record_fold: Option<ir::IndyRecordFold>,
}

/// JIT round 16 wave 10 (lane indyir, IN16-8): the `invokedynamic` sites the
/// optimizing tier plans as a plain call of their constant call site's
/// target, keyed by constant-pool index -- the key every resolver of the
/// invoke plan is asked by. An `InvokeDynamic` entry is never a `Methodref`,
/// so a key here never shadows an ordinary invoke's answer.
#[derive(Default, Debug)]
pub(crate) struct IrIndyDirectPlan {
    pub(crate) by_cp: HashMap<u16, IrIndyDirectSite>,
}

impl IrIndyDirectPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.by_cp.is_empty()
    }

    pub(crate) fn site_count(&self) -> usize {
        self.by_cp.values().map(|s| s.pcs.len()).sum()
    }

    /// The call's `(class, name, descriptor)`, as `cp_invoke_resolver` answers.
    pub(crate) fn names(&self, cp_idx: u16) -> Option<(String, String, String)> {
        self.by_cp
            .get(&cp_idx)
            .map(|s| (s.class.clone(), s.name.clone(), s.desc.clone()))
    }

    /// The target class's id, as `cp_invoke_class_id_resolver` answers.
    pub(crate) fn class_id(&self, cp_idx: u16) -> Option<u32> {
        self.by_cp.get(&cp_idx).map(|s| s.class_id)
    }

    /// The class context of the call planned at `pc`: the TARGET class, so a
    /// by-name resolution of the call (the dispatch, a deopting target's
    /// callee resume) asks that class's loader -- never `0`, the bootstrap
    /// loader, and never a caller whose loader might name another class.
    pub(crate) fn class_context_at(&self, pc: usize) -> Option<u32> {
        self.by_cp
            .values()
            .find(|s| s.pcs.contains(&pc))
            .map(|s| s.class_id)
    }

    /// `pc -> the invoke opcode whose builder arm builds it`. A guarded site
    /// (JIT round 20 wave 11, lane pIndy) is not here: the `0xba` arm builds
    /// it ([`Self::mutable_guards`]).
    pub(crate) fn pseudo_opcodes(&self) -> HashMap<usize, u8> {
        self.by_cp
            .values()
            .filter(|s| s.guard.is_none() && s.record_fold.is_none())
            .flat_map(|s| {
                let op = if s.invoke_kind == 3 { 0xb8 } else { 0xb6 };
                s.pcs.iter().map(move |&pc| (pc, op))
            })
            .collect()
    }

    /// JIT round 20 wave 11 (lane pIndy, SPLICE19-1): `pc -> guard` of every
    /// guarded (mutable call site) instruction.
    pub(crate) fn mutable_guards(&self) -> HashMap<usize, ir::IndyMutableGuard> {
        self.by_cp
            .values()
            .filter_map(|s| s.guard.map(|g| (s, g)))
            .flat_map(|(s, g)| s.pcs.iter().map(move |&pc| (pc, g)))
            .collect()
    }

    /// JIT round 21 wave 17 (lane records, IN16-6): `pc -> fold` of every
    /// record `ObjectMethods` site planned as a field-wise fold.
    pub(crate) fn record_folds(&self) -> HashMap<usize, ir::IndyRecordFold> {
        self.by_cp
            .values()
            .filter_map(|s| s.record_fold.as_ref().map(|f| (s, f)))
            .flat_map(|(s, f)| s.pcs.iter().map(move |&pc| (pc, f.clone())))
            .collect()
    }

    /// JIT round 21 wave 17 (lane records, IN16-6): is `pc` a record fold's
    /// site? Its planned `Objects` call is never spliced there: the builder
    /// builds the fold, not the call at the instruction.
    pub(crate) fn is_record_fold_pc(&self, pc: usize) -> bool {
        self.by_cp
            .values()
            .any(|s| s.record_fold.is_some() && s.pcs.contains(&pc))
    }

    /// `invoke_ops` with every planned site added as its invoke, in pc order.
    pub(crate) fn merged_invoke_ops(&self, invoke_ops: &[(usize, u16, u8)]) -> Vec<(usize, u16, u8)> {
        let mut ops = invoke_ops.to_vec();
        if self.by_cp.is_empty() {
            return ops;
        }
        for (&cp_idx, s) in &self.by_cp {
            let op = if s.invoke_kind == 3 { 0xb8 } else { 0xb6 };
            ops.extend(s.pcs.iter().map(|&pc| (pc, cp_idx, op)));
        }
        ops.sort_unstable_by_key(|&(pc, _, _)| pc);
        ops
    }
}

/// JIT round 16 wave 10 (lane indyir, IN16-8): which of `indy_ops` the
/// optimizing tier plans as a plain call, from the VM's per-instruction answer
/// (`CompileRequest::cp_indy_direct_resolver`).
///
/// Admitted only where joining the invoke plan cannot cost the method its
/// plan: the call kind's gate is on (a gated-off kind would refuse EVERY
/// invoke of the method), `static_call_shape` admits the descriptor (its
/// refusal does the same), and the target is not this method itself (a
/// self-call has its own refusals there). A virtual site whose receiver null
/// guard already failed in an earlier body (`guard_trapped(pc)`, the de-spec
/// registry's `(pc, UncommonTrap)` row) keeps the bridge, so a site that does
/// see null receivers is not guarded into deopting again. Instructions sharing
/// an entry are admitted only when every one of them answers the same call:
/// the plan's resolvers are keyed by the entry.
pub(crate) fn ir_indy_direct_plan(
    resolver: Option<&dyn Fn(u16, usize) -> Option<(String, String, String, u8, u32)>>,
    indy_ops: &[(usize, u16)],
    static_calls: bool,
    virtual_calls: bool,
    own: (&str, &str, &str),
    guard_trapped: &dyn Fn(usize) -> bool,
) -> IrIndyDirectPlan {
    let Some(resolver) = resolver else {
        return IrIndyDirectPlan::default();
    };
    let mut by_cp: HashMap<u16, Option<IrIndyDirectSite>> = HashMap::new();
    for &(pc, cp_idx) in indy_ops {
        let answer = resolver(cp_idx, pc)
            .filter(|(class, name, desc, kind, class_id)| {
                *class_id != 0
                    && match *kind {
                        3 => static_calls,
                        0 => virtual_calls,
                        _ => false,
                    }
                    && static_call_shape(desc).is_some()
                    && (class.as_str(), name.as_str(), desc.as_str()) != own
                    && (*kind != 0 || !guard_trapped(pc))
            })
            .map(
                |(class, name, desc, invoke_kind, class_id)| IrIndyDirectSite {
                    pcs: vec![pc],
                    class,
                    name,
                    desc,
                    invoke_kind,
                    class_id,
                    guard: None,
                    record_fold: None,
                },
            );
        match by_cp.entry(cp_idx) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(answer);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let same = match (slot.get(), &answer) {
                    (Some(a), Some(b)) => {
                        (&a.class, &a.name, &a.desc, a.invoke_kind, a.class_id)
                            == (&b.class, &b.name, &b.desc, b.invoke_kind, b.class_id)
                    }
                    _ => false,
                };
                match slot.get_mut() {
                    Some(site) if same => site.pcs.push(pc),
                    other => *other = None,
                }
            }
        }
    }
    IrIndyDirectPlan {
        by_cp: by_cp
            .into_iter()
            .filter_map(|(cp_idx, site)| site.map(|s| (cp_idx, s)))
            .collect(),
    }
}

/// The answer of `CompileRequest::cp_indy_mutable_guard_resolver` for one
/// instruction: the call `(class, name, descriptor, invoke_kind, class id)`
/// and the guard's `(bridge site, call-site slot, target slot, field index)`.
pub(crate) type IrIndyMutableAnswer = compile_request::IndyMutableGuardAnswer;

/// JIT round 20 wave 11 (lane pIndy, SPLICE19-1 of
/// `jit-r19-splice19-proposals.md`): add to `plan` the `invokedynamic` sites
/// of a MUTABLE call site the VM answers a planned target for
/// (`CompileRequest::cp_indy_mutable_guard_resolver`), each as a guarded
/// site ([`IrIndyDirectSite::guard`]): it joins the invoke plan as its invoke
/// (so the plain call has its row, direct bind and inline cache) but is built
/// by the `0xba` arm as `guard ? call : bridge`
/// (`IrBuilder::try_indy_mutable_guarded_call`).
///
/// The admission terms of [`ir_indy_direct_plan`] (the kind's gate, a shape
/// `static_call_shape` admits, not a self-call), except the null-guard trap
/// rule: a null receiver takes the bridge, never a deopt. Only entries the
/// constant plan left alone, only with the bridge helper wired (the miss
/// edge calls it), and only when every instruction of an entry answers the
/// same call AND the same guard.
pub(crate) fn ir_indy_mutable_plan_into(
    plan: &mut IrIndyDirectPlan,
    resolver: Option<&dyn Fn(u16, usize) -> Option<IrIndyMutableAnswer>>,
    indy_ops: &[(usize, u16)],
    static_calls: bool,
    virtual_calls: bool,
    own: (&str, &str, &str),
    bridge_wired: bool,
) {
    let Some(resolver) = resolver else {
        return;
    };
    if !bridge_wired {
        return;
    }
    let mut by_cp: HashMap<u16, Option<IrIndyDirectSite>> = HashMap::new();
    for &(pc, cp_idx) in indy_ops {
        if plan.by_cp.contains_key(&cp_idx) {
            continue;
        }
        let answer = resolver(cp_idx, pc)
            .filter(
                |((class, name, desc, kind, class_id), (site, cs_slot, target_slot, _), _, _)| {
                    *class_id != 0
                        && *site != 0
                        && *cs_slot != 0
                        && *target_slot != 0
                        && match *kind {
                            3 => static_calls,
                            0 => virtual_calls,
                            _ => false,
                        }
                        && static_call_shape(desc).is_some()
                        && (class.as_str(), name.as_str(), desc.as_str()) != own
                },
            )
            .map(
                |(
                    (class, name, desc, invoke_kind, class_id),
                    (site, cs_slot, target_slot, field),
                    (second_slot, second_site),
                    field_row,
                )| {
                    // INDY21-1: a second target only as a pair (both words).
                    let (second_target_slot, second_site) = if second_slot != 0 && second_site != 0
                    {
                        (second_slot, second_site)
                    } else {
                        (0, 0)
                    };
                    IrIndyDirectSite {
                        pcs: vec![pc],
                        class,
                        name,
                        desc,
                        invoke_kind,
                        class_id,
                        guard: Some(ir::IndyMutableGuard {
                            bridge_site: site,
                            call_site_slot: cs_slot,
                            target_slot,
                            field_index: field,
                            virtual_target: invoke_kind != 3,
                            second_target_slot,
                            second_site,
                            field_row,
                        }),
                        record_fold: None,
                    }
                },
            );
        match by_cp.entry(cp_idx) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(answer);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let same = match (slot.get(), &answer) {
                    (Some(a), Some(b)) => {
                        (&a.class, &a.name, &a.desc, a.invoke_kind, a.class_id)
                            == (&b.class, &b.name, &b.desc, b.invoke_kind, b.class_id)
                            // The whole guard: the bridge site and the call
                            // site are the instruction's own, so two
                            // instructions agree only when they share both.
                            && a.guard == b.guard
                    }
                    _ => false,
                };
                match slot.get_mut() {
                    Some(site) if same => site.pcs.push(pc),
                    other => *other = None,
                }
            }
        }
    }
    plan.by_cp.extend(
        by_cp
            .into_iter()
            .filter_map(|(cp_idx, site)| site.map(|s| (cp_idx, s))),
    );
}

/// JIT round 21 wave 17 (lane records, IN16-6;
/// `CRATONVM_JIT_IR_INDY_RECORD_FOLD`): add to `plan` the record
/// `ObjectMethods` sites the VM answers a fold for
/// (`CompileRequest::cp_indy_record_fold_resolver`), each as a fold site
/// ([`IrIndyDirectSite::record_fold`]) whose planned call is the fold's
/// `Objects.hashCode(Object)` / `Objects.equals(Object, Object)` (an
/// `invokestatic`, the call the handle chain makes per reference component):
/// it joins the invoke plan so that call has its row and direct bind, and the
/// `0xba` arm builds the fold (`IrBuilder::try_indy_record_fold`).
///
/// Only entries no earlier plan took, only under the static-call gate (a
/// gated-off kind refuses every invoke of the method), only a shape
/// `static_call_shape` admits, and only when every instruction of an entry
/// answers the same fold.
pub(crate) fn ir_indy_record_fold_plan_into(
    plan: &mut IrIndyDirectPlan,
    resolver: Option<&dyn Fn(u16, usize) -> Option<compile_request::IndyRecordFoldAnswer>>,
    indy_ops: &[(usize, u16)],
    static_calls: bool,
) {
    let Some(resolver) = resolver else {
        return;
    };
    if !static_calls {
        return;
    }
    let mut by_cp: HashMap<u16, Option<IrIndyDirectSite>> = HashMap::new();
    for &(pc, cp_idx) in indy_ops {
        if plan.by_cp.contains_key(&cp_idx) {
            continue;
        }
        let answer = resolver(cp_idx, pc)
            .filter(|(fold, objects_class_id)| *objects_class_id != 0 && fold.record_class_id != 0)
            .map(|(fold, objects_class_id)| {
                let (name, desc) = if fold.equals {
                    ("equals", "(Ljava/lang/Object;Ljava/lang/Object;)Z")
                } else {
                    ("hashCode", "(Ljava/lang/Object;)I")
                };
                IrIndyDirectSite {
                    pcs: vec![pc],
                    class: "java/util/Objects".to_string(),
                    name: name.to_string(),
                    desc: desc.to_string(),
                    invoke_kind: 3,
                    class_id: objects_class_id,
                    guard: None,
                    record_fold: Some(fold),
                }
            })
            .filter(|s| static_call_shape(&s.desc).is_some());
        match by_cp.entry(cp_idx) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(answer);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let same = match (slot.get(), &answer) {
                    (Some(a), Some(b)) => {
                        a.class_id == b.class_id && a.record_fold == b.record_fold
                    }
                    _ => false,
                };
                match slot.get_mut() {
                    Some(site) if same => site.pcs.push(pc),
                    other => *other = None,
                }
            }
        }
    }
    plan.by_cp.extend(
        by_cp
            .into_iter()
            .filter_map(|(cp_idx, site)| site.map(|s| (cp_idx, s))),
    );
}

/// JIT round 16 wave 17 (lane mh7; MH16-8 at compile time): one `static
/// final` field's planned plain call -- the `invokevirtual
/// MethodHandle.invokeExact` / `invoke` sites whose receiver is that field,
/// compiled as an `invokestatic` of the handle's member (see
/// [`ir_mh_constant_plan`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IrMhConstantSite {
    /// `(invoke pc, getstatic pc)` of every site of this field (all answered
    /// the same call).
    pub(crate) pcs: Vec<(usize, usize)>,
    pub(crate) class: String,
    pub(crate) name: String,
    pub(crate) desc: String,
    /// The member's class: the call's class context.
    pub(crate) class_id: u32,
    /// `3` (static: an `invokestatic` row) or `0` (JIT round 17 wave 3, lane
    /// mh4, MHCONST17-2: an unbound virtual handle, an `invokevirtual` row
    /// whose receiver is the site's first argument) or `2` (JIT round 19
    /// wave 1, lane mhiface, MHRESID18-4: an unbound interface handle, an
    /// `invokeinterface` row whose receiver the builder guards with an
    /// `instanceof` of the interface) or `4` (JIT round 21 wave 19, lane
    /// mhbind2, MHBIND21-1: a REAL `bindTo` result of a virtual member, an
    /// `invokevirtual` row whose receiver the builder reads off the handle,
    /// [`Self::bound`]).
    pub(crate) invoke_kind: u8,
    /// The `invokeExact` constant-pool index of the field's first site (the
    /// bound resolver's key, MHBIND21-1).
    pub(crate) invoke_cp: u16,
    /// `invoke_kind 4` only: `(argL1 slot, bound value's exact class id)`
    /// ([`IrMhConstantPlan::with_bound_rows`]); `None` until then.
    pub(crate) bound: Option<(u32, u32)>,
    /// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): the `invokeExact`
    /// site's own descriptor (the filter row's shape check).
    pub(crate) site: String,
    /// `invoke_kind 5` only: the filter species' PRE call
    /// ([`IrMhConstantPlan::with_filter_rows`]); `None` until then.
    pub(crate) filter: Option<crate::compile_request::MhConstantFilterRow>,
    /// `invoke_kind 7` only: converting REAL `asType` handle
    /// ([`IrMhConstantPlan::with_convert_rows`]); `None` until then.
    pub(crate) convert: Option<crate::compile_request::MhConstantConvertRow>,
    /// mh21 (MHFILTER21-1): the synthetic constant-pool key the PRE call is
    /// planned under ([`MH_FILTER_PRE_KEY`] and the keys below it); `0` while
    /// [`Self::filter`] is `None`.
    pub(crate) pre_key: u16,
}

/// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): the constant-pool key the
/// PRE call of a filter species site is planned under
/// ([`IrMhConstantPlan::with_filter_rows`]). `0xFFFF` is never a valid
/// constant-pool index (a pool has at most 65535 entries, numbered from 1), so
/// no resolver of the VM answers it and no ordinary invoke is keyed by it.
///
/// mh21 (MHFILTER21-1): the FIRST filter field of a method is planned under
/// this key; the next ones under the keys just below it, as many as
/// [`MH_FILTER_MAX_FIELDS`] allows, each only if nothing else of the method
/// answers it (`with_filter_rows`' `key_free`). A real pool of 65535 entries
/// could own `0xFFFE`, which is why a key below `0xFFFF` is asked first.
pub(crate) const MH_FILTER_PRE_KEY: u16 = 0xFFFF;

/// mh21 (MHFILTER21-1): the most filter species fields one compiled method
/// plans (each takes one key from [`MH_FILTER_PRE_KEY`] down). A further field
/// keeps its `invokeExact` link chain.
pub(crate) const MH_FILTER_MAX_FIELDS: usize = 4;

/// JIT round 17 wave 3 (lane mh4; MHCONST17-2): is `member` (an instance
/// method's descriptor) the call an `invokeExact` site of descriptor `site`
/// makes on an unbound virtual handle? The site passes the receiver first --
/// one reference (`L...;`) -- and then exactly the member's parameters, and
/// expects the member's return. The receiver's class is the handle type's,
/// which the VM door checks against both loaders.
///
/// j27 wave 6 (lane T1LAM2): `pub` so the VM door asks the same question of a
/// bound STATIC handle (`invoke_kind 6`) with the arguments swapped: there the
/// MEMBER carries the extra leading reference (the bound value) and the site
/// does not.
pub fn mh_constant_virtual_site_matches(site: &str, member: &str) -> bool {
    let Some(rest) = site.strip_prefix("(L") else {
        return false;
    };
    let Some(end) = rest.find(';') else {
        return false;
    };
    end > 0 && member.strip_prefix('(') == Some(&rest[end + 1..])
}

/// JIT round 16 wave 17 (lane mh7): the constant-handle sites the optimizing
/// tier plans as a plain call of the handle's member, keyed by the
/// constant-pool index of the `getstatic` whose field holds the handle. Every
/// resolver of the invoke plan is asked by that key: a `Fieldref` is never a
/// `Methodref`, so the VM's own invoke resolvers answer nothing for it, and
/// no ordinary invoke's answer is shadowed. The same shape as
/// [`IrIndyDirectPlan`].
#[derive(Default, Debug)]
pub(crate) struct IrMhConstantPlan {
    pub(crate) by_cp: HashMap<u16, IrMhConstantSite>,
}

impl IrMhConstantPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.by_cp.is_empty()
    }

    pub(crate) fn site_count(&self) -> usize {
        self.by_cp.values().map(|s| s.pcs.len()).sum()
    }

    /// The call's `(class, name, descriptor)`, as `cp_invoke_resolver` answers.
    /// JIT round 21 wave 20 (lane mhfilter): [`MH_FILTER_PRE_KEY`] names the
    /// filter species' PRE call.
    pub(crate) fn convert_box_by_key(&self, key: u16) -> Option<(u8, u32)> {
        if key == 0 {
            return None;
        }
        self.by_cp
            .values()
            .filter(|s| s.pre_key == key)
            .find_map(|s| s.convert.as_ref().filter(|c| c.box_return != 0).map(|c| (c.box_return, c.box_class_id)))
    }

    pub(crate) fn names(&self, cp_idx: u16) -> Option<(String, String, String)> {
        if let Some(f) = self.filter_row_by_key(cp_idx) {
            return Some((f.pre_class.clone(), f.pre_name.clone(), f.pre_desc.clone()));
        }
        if let Some((box_ret, _)) = self.convert_box_by_key(cp_idx) {
            return Some(if box_ret == 2 {
                ("java/lang/Long".into(), "valueOf".into(), "(J)Ljava/lang/Long;".into())
            } else {
                ("java/lang/Integer".into(), "valueOf".into(), "(I)Ljava/lang/Integer;".into())
            });
        }
        self.by_cp
            .get(&cp_idx)
            .map(|s| (s.class.clone(), s.name.clone(), s.desc.clone()))
    }

    /// The member class's id, as `cp_invoke_class_id_resolver` answers.
    pub(crate) fn class_id(&self, cp_idx: u16) -> Option<u32> {
        if let Some(f) = self.filter_row_by_key(cp_idx) {
            return Some(f.pre_class_id);
        }
        if let Some((_, box_id)) = self.convert_box_by_key(cp_idx) {
            return Some(box_id);
        }
        self.by_cp.get(&cp_idx).map(|s| s.class_id)
    }

    /// The class context of the call planned at `pc`: the member's class (see
    /// [`IrIndyDirectPlan::class_context_at`]); at a filter site's PRE pc (its
    /// `getstatic`, JIT round 21 wave 20, lane mhfilter) the PRE member's.
    pub(crate) fn class_context_at(&self, pc: usize) -> Option<u32> {
        if let Some(s) = self.by_cp.values().find(|s| s.pcs.iter().any(|&(at, _)| at == pc)) {
            if let Some(c) = s.convert.as_ref().filter(|c| c.box_return != 0) {
                return Some(c.box_class_id);
            }
            return Some(s.class_id);
        }
        self.by_cp
            .values()
            .filter(|s| s.pcs.iter().any(|&(_, pre)| pre == pc))
            .find_map(|s| {
                s.filter
                    .as_ref()
                    .map(|f| f.pre_class_id)
                    .or_else(|| s.convert.as_ref().filter(|c| c.box_return != 0).map(|_| s.class_id))
            })
    }

    /// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1), mh21 (MHFILTER21-1):
    /// the planned filter species' row whose PRE call is named under the
    /// synthetic key `key` ([`Self::with_filter_rows`] hands out up to
    /// [`MH_FILTER_MAX_FIELDS`], one per field); `None` for any other key.
    pub(crate) fn filter_row_by_key(&self, key: u16) -> Option<&crate::compile_request::MhConstantFilterRow> {
        if key == 0 {
            return None;
        }
        self.by_cp
            .values()
            .filter(|s| s.pre_key == key)
            .find_map(|s| s.filter.as_ref())
    }

    /// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1,
    /// `CRATONVM_JIT_MH_CONSTANT_COMPILE_FILTER` in the VM): every `invoke_kind
    /// 5` site's PRE call from `resolver`
    /// (`CompileRequest::cp_mh_constant_filter_resolver`), kept only when:
    ///
    /// * both calls pass their gate (`static_calls` / `virtual_calls`), the
    ///   PRE member is not this method, and the site has the shape the JDK
    ///   gives the species ([`crate::compile_request::mh_filter_row_fits`]);
    /// * every site of the field has a window [`mh_filter_window_admits`]:
    ///   the PRE call is keyed by the site's `getstatic` pc (its own rows --
    ///   inline cache, direct entry -- live under that pc, which no invoke
    ///   has), so no exception handler may cover the window and no local may
    ///   be written in it;
    /// * (mh21, MHFILTER21-1) it is one of the first [`MH_FILTER_MAX_FIELDS`]
    ///   such fields by constant-pool index whose windows do not overlap an
    ///   earlier kept field's, and a key is left for it: its PRE call is named
    ///   under [`MH_FILTER_PRE_KEY`] for the first field, then under the next
    ///   key down that `key_free` accepts (nothing else of the method answers
    ///   it);
    /// * (JIT round j27, lane MHINDY) the resolver answers EVERY site of the
    ///   field, asked with that site's `(invoke pc, getstatic pc)`: the VM
    ///   refuses a site whose two pcs name different source lines (the PRE
    ///   call's frame reports the `getstatic` pc), and the first site's
    ///   answer is the row.
    ///
    /// Every other `invoke_kind 5` field loses its sites (they keep the
    /// `invokeExact`); every other site is untouched.
    pub(crate) fn with_filter_rows(
        mut self,
        resolver: Option<
            &dyn Fn(u16, usize, u16, usize) -> Option<crate::compile_request::MhConstantFilterRow>,
        >,
        code: &[u8],
        protected: &dyn Fn(usize) -> bool,
        (static_calls, virtual_calls): (bool, bool),
        own: (&str, &str, &str),
        key_free: &dyn Fn(u16) -> bool,
    ) -> Self {
        let mut fields: Vec<u16> = self
            .by_cp
            .iter()
            .filter(|(_, s)| s.invoke_kind == 5)
            .map(|(&cp, _)| cp)
            .collect();
        fields.sort_unstable();
        // mh21 (MHFILTER21-1): up to `MH_FILTER_MAX_FIELDS` fields, each with its
        // own synthetic key (`MH_FILTER_PRE_KEY` and the ones below it that
        // nothing else of the method answers), and only fields whose
        // `getstatic .. invokeExact` windows do not overlap (a nested
        // `F.invokeExact(G.invokeExact(x), y)` keeps the chain: two PRE calls
        // inside one another's window are unproven).
        let field_keys: std::collections::HashSet<u16> = self.by_cp.keys().copied().collect();
        let mut next_key_offset = 0usize;
        let mut chosen = 0usize;
        let mut taken: Vec<(usize, usize)> = Vec::new();
        for field_cp in fields {
            let windows: Vec<(usize, usize)> = self
                .by_cp
                .get(&field_cp)
                .map(|s| {
                    s.pcs
                        .iter()
                        .map(|&(at, getstatic_pc)| (getstatic_pc, at))
                        .collect()
                })
                .unwrap_or_default();
            let overlaps = windows.iter().any(|&(lo, hi)| {
                taken
                    .iter()
                    .any(|&(other_lo, other_hi)| lo <= other_hi && other_lo <= hi)
            });
            let keep = chosen < MH_FILTER_MAX_FIELDS
                && !overlaps
                && self.by_cp.get_mut(&field_cp).is_some_and(|s| {
                    let Some(&(pc, getstatic_pc)) = s.pcs.first() else {
                        return false;
                    };
                    let row = resolver.and_then(|r| r(s.invoke_cp, pc, field_cp, getstatic_pc));
                    let every_site_answered = s.pcs.iter().skip(1).all(|&(at, getstatic_at)| {
                        resolver
                            .and_then(|r| r(s.invoke_cp, at, field_cp, getstatic_at))
                            .is_some()
                    });
                    let gate = |kind: u8| {
                        if kind == 3 {
                            static_calls
                        } else {
                            virtual_calls
                        }
                    };
                    let fits = row.as_ref().is_some_and(|row| {
                        gate(row.main_kind)
                            && gate(row.pre_kind)
                            && row.pre_class_id != 0
                            && static_call_shape(&row.pre_desc).is_some()
                            && (
                                row.pre_class.as_str(),
                                row.pre_name.as_str(),
                                row.pre_desc.as_str(),
                            ) != own
                            && crate::compile_request::mh_filter_row_fits(
                                &s.site, &s.class, &s.desc, row,
                            )
                            && s.pcs.iter().all(|&(at, getstatic_pc)| {
                                mh_filter_window_admits(code, getstatic_pc, at, protected)
                            })
                    });
                    s.filter = row.filter(|_| fits && every_site_answered);
                    if s.filter.is_none() {
                        return false;
                    }
                    // The next key nothing else answers (`MH_FILTER_PRE_KEY`
                    // first); none left: the field keeps its chain.
                    while next_key_offset < MH_FILTER_MAX_FIELDS {
                        let Some(key) = u16::try_from(next_key_offset)
                            .ok()
                            .and_then(|offset| MH_FILTER_PRE_KEY.checked_sub(offset))
                        else {
                            break;
                        };
                        next_key_offset += 1;
                        if !field_keys.contains(&key) && key_free(key) {
                            s.pre_key = key;
                            return true;
                        }
                    }
                    s.filter = None;
                    false
                });
            if keep {
                chosen += 1;
                taken.extend(windows);
            } else {
                self.by_cp.remove(&field_cp);
            }
        }
        self
    }

    /// `invoke pc -> the builder's filter row` of every filter species site
    /// (JIT round 21 wave 20, lane mhfilter), for
    /// `IrBuilder::set_mh_constant_filter_sites`.
    pub(crate) fn with_convert_rows(
        mut self,
        resolver: Option<
            &dyn Fn(u16, usize, u16, usize) -> Option<crate::compile_request::MhConstantConvertRow>,
        >,
        code: &[u8],
        protected: &dyn Fn(usize) -> bool,
        (static_calls, virtual_calls): (bool, bool),
        key_free: &dyn Fn(u16) -> bool,
    ) -> Self {
        let mut fields: Vec<u16> = self
            .by_cp
            .iter()
            .filter(|(_, s)| s.invoke_kind == 7)
            .map(|(&cp, _)| cp)
            .collect();
        fields.sort_unstable();
        let field_keys: std::collections::HashSet<u16> = self.by_cp.keys().copied().collect();
        let used_pre_keys: std::collections::HashSet<u16> = self
            .by_cp
            .values()
            .filter(|s| s.pre_key != 0)
            .map(|s| s.pre_key)
            .collect();
        let mut taken: Vec<(usize, usize)> = self
            .by_cp
            .values()
            .filter(|s| s.filter.is_some())
            .flat_map(|s| s.pcs.iter().map(|&(at, pre)| (pre, at)))
            .collect();
        let mut next_key_offset = 0usize;
        let mut chosen = self.by_cp.values().filter(|s| s.filter.is_some()).count();
        for field_cp in fields {
            let windows: Vec<(usize, usize)> = self
                .by_cp
                .get(&field_cp)
                .map(|s| s.pcs.iter().map(|&(at, getstatic_pc)| (getstatic_pc, at)).collect())
                .unwrap_or_default();
            let keep = self.by_cp.get_mut(&field_cp).is_some_and(|s| {
                let Some(&(pc, getstatic_pc)) = s.pcs.first() else {
                    return false;
                };
                let row = resolver.and_then(|r| r(s.invoke_cp, pc, field_cp, getstatic_pc));
                let every_site_answered = s.pcs.iter().skip(1).all(|&(at, getstatic_at)| {
                    resolver
                        .and_then(|r| r(s.invoke_cp, at, field_cp, getstatic_at))
                        .is_some()
                });
                let fits = row.as_ref().is_some_and(|row| {
                    let gate_ok = if row.main_kind == 3 { static_calls } else { virtual_calls };
                    let box_ok = row.box_return == 0
                        || (static_calls
                            && row.box_class_id != 0
                            && s.pcs.iter().all(|&(at, gpc)| mh_filter_window_admits(code, gpc, at, protected)));
                    gate_ok
                        && matches!(row.main_kind, 0 | 2 | 3)
                        && usize::from(row.arity) <= 8
                        && box_ok
                });
                s.convert = row.filter(|_| fits && every_site_answered);
                let Some(ref conv) = s.convert else {
                    return false;
                };
                if conv.box_return == 0 {
                    return true;
                }
                let overlaps = windows.iter().any(|&(lo, hi)| {
                    taken.iter().any(|&(other_lo, other_hi)| lo <= other_hi && other_lo <= hi)
                });
                if chosen >= MH_FILTER_MAX_FIELDS || overlaps {
                    s.convert = None;
                    return false;
                }
                while next_key_offset < MH_FILTER_MAX_FIELDS {
                    let Some(key) = u16::try_from(next_key_offset)
                        .ok()
                        .and_then(|offset| MH_FILTER_PRE_KEY.checked_sub(offset))
                    else {
                        break;
                    };
                    next_key_offset += 1;
                    if !field_keys.contains(&key) && !used_pre_keys.contains(&key) && key_free(key) {
                        s.pre_key = key;
                        return true;
                    }
                }
                s.convert = None;
                false
            });
            if keep {
                if self.by_cp.get(&field_cp).is_some_and(|s| s.convert.as_ref().is_some_and(|c| c.box_return != 0)) {
                    chosen += 1;
                    taken.extend(windows);
                }
            } else {
                self.by_cp.remove(&field_cp);
            }
        }
        self
    }

    pub(crate) fn convert_sites(&self) -> HashMap<usize, ir::IrMhConvertSite> {
        self.by_cp
            .values()
            .filter_map(|s| s.convert.as_ref().map(|c| (s, c)))
            .flat_map(|(s, c)| {
                s.pcs.iter().map(move |&(pc, getstatic_pc)| {
                    (
                        pc,
                        ir::IrMhConvertSite {
                            pre_pc: getstatic_pc,
                            main_kind: c.main_kind,
                            arity: c.arity,
                            cast_class_ids: c.cast_class_ids,
                            cast_class_names: c.cast_class_names.clone(),
                            unbox_op: c.unbox_op,
                            unbox_class_ids: c.unbox_class_ids,
                            box_return: c.box_return,
                        },
                    )
                })
            })
            .collect()
    }

    /// The `invokeExact` pc of the box-returning converting site whose
    /// member call is planned at `getstatic_pc` (the PRE row,
    /// [`Self::rewritten_invoke_ops`]); the planner may splice that member
    /// there (`IrBuilder::convert_member_splice_admitted` takes it only when
    /// its body cannot raise).
    pub(crate) fn convert_member_invoke_pc(&self, getstatic_pc: usize) -> Option<usize> {
        self.by_cp.values().find_map(|s| {
            s.convert.as_ref().filter(|c| c.box_return != 0 && matches!(c.main_kind, 0 | 2 | 3))?;
            s.pcs.iter().find(|&&(_, pre)| pre == getstatic_pc).map(|&(at, _)| at)
        })
    }

    /// May a convert member's resolved body be planned at its `getstatic`
    /// pc? Only one that raises nothing ([`ir::splice_op_cannot_raise`]), a
    /// call in it allowed while `nest` is above zero when it is an
    /// `invokestatic` of a nested body that passes too: the builder would
    /// keep the call of any other, and a planned call loses its inline cache.
    pub(crate) fn convert_member_body_cannot_raise(site: &InlineSite, nest: u8) -> bool {
        let end = site.callee_code_len.min(site.callee_code.len());
        let code = &site.callee_code[..end];
        let mut at = 0;
        while at < end {
            let ok = match code[at] {
                0xb8 => {
                    nest > 0
                        && site.nested_sites.iter().find(|n| n.callee_pc == at).is_some_and(|n| {
                            n.bimorphic_second.is_none() && Self::convert_member_body_cannot_raise(&n.site, nest - 1)
                        })
                }
                _ => ir::splice_op_cannot_raise(code, at, end),
            };
            if !ok {
                return false;
            }
            at += crate::bytecode_analysis::step(code, at).max(1);
        }
        !site.callee_is_synchronized && end > 0
    }

    pub(crate) fn is_convert_two_call_pc(&self, pc: usize) -> bool {
        self.by_cp.values().any(|s| {
            s.convert.as_ref().is_some_and(|c| c.box_return != 0)
                && s.pcs.iter().any(|&(at, pre)| at == pc || pre == pc)
        })
    }

        pub(crate) fn filter_sites(&self) -> HashMap<usize, ir::IrMhFilterSite> {
        self.by_cp
            .values()
            .filter_map(|s| s.filter.as_ref().map(|f| (s, f)))
            .flat_map(|(s, f)| {
                s.pcs.iter().map(move |&(pc, getstatic_pc)| {
                    (
                        pc,
                        ir::IrMhFilterSite {
                            pre_pc: getstatic_pc,
                            position: f.position,
                            main_virtual: f.main_kind == 0,
                            pre_virtual: f.pre_kind == 0,
                            target_slot: f.target_slot,
                        },
                    )
                })
            })
            .collect()
    }

    /// Is `pc` a filter species site's `invokeExact` pc or its PRE call's
    /// (`getstatic`) pc (JIT round 21 wave 20, lane mhfilter)? Neither call
    /// is ever spliced: a deopt inside a spliced body re-executes the
    /// `invokeExact`, which would run the other call a second time.
    pub(crate) fn is_filter_pc(&self, pc: usize) -> bool {
        self.by_cp
            .values()
            .any(|s| s.filter.is_some() && s.pcs.iter().any(|&(at, pre)| at == pc || pre == pc))
    }

    /// `invoke pc -> getstatic pc`, for the builder's receiver check
    /// (`IrBuilder::set_mh_constant_sites`).
    pub(crate) fn receiver_pcs(&self) -> HashMap<usize, usize> {
        self.by_cp
            .values()
            .flat_map(|s| s.pcs.iter().copied())
            .collect()
    }

    /// The invoke pcs of the INTERFACE handles' sites (`invoke_kind 2`), for
    /// the builder's receiver type guard (JIT round 19 wave 1, lane mhiface;
    /// `IrBuilder::set_mh_constant_interface_sites`).
    pub(crate) fn interface_pcs(&self) -> std::collections::HashSet<usize> {
        self.by_cp
            .values()
            .filter(|s| s.invoke_kind == 2)
            .flat_map(|s| s.pcs.iter().map(|&(pc, _)| pc))
            .collect()
    }

    /// JIT round 21 wave 19 (lane mhbind2; MHBIND21-1): every `invoke_kind 4`
    /// site's `(argL1 slot, bound value's exact class id)` from `resolver`
    /// (`CompileRequest::cp_mh_constant_bound_resolver`); a field with no
    /// answer (or no resolver) loses its sites. Every other site is untouched.
    pub(crate) fn with_bound_rows(
        mut self,
        resolver: Option<&dyn Fn(u16, usize, u16) -> Option<(u32, u32)>>,
    ) -> Self {
        self.by_cp.retain(|&field_cp, s| {
            // j27 wave 6 (lane T1LAM2): a bound STATIC site (`6`) needs its
            // row as well; without it the plain `invokestatic` would drop the
            // bound argument.
            if !matches!(s.invoke_kind, 4 | 6) {
                return true;
            }
            let Some(&(pc, _)) = s.pcs.first() else {
                return false;
            };
            s.bound = resolver.and_then(|r| r(s.invoke_cp, pc, field_cp));
            s.bound.is_some()
        });
        self
    }

    /// The invoke pcs of the bound STATIC sites (`invoke_kind 6`, j27 wave 6,
    /// lane T1LAM2) that kept their row, for the builder
    /// (`IrBuilder::set_mh_constant_bound_static_sites`).
    pub(crate) fn bound_static_pcs(&self) -> std::collections::HashSet<usize> {
        self.by_cp
            .values()
            .filter(|s| s.invoke_kind == 6 && s.bound.is_some())
            .flat_map(|s| s.pcs.iter().map(|&(pc, _)| pc))
            .collect()
    }

    /// `invoke pc -> argL1 slot` of every bound site (MHBIND21-1), for the
    /// builder (`IrBuilder::set_mh_constant_bound_sites`).
    pub(crate) fn bound_slots(&self) -> HashMap<usize, u32> {
        self.by_cp
            .values()
            .filter_map(|s| s.bound.map(|(slot, _)| (s, slot)))
            .flat_map(|(s, slot)| s.pcs.iter().map(move |&(pc, _)| (pc, slot)))
            .collect()
    }

    /// The bound value's exact class id at the bound site planned at `pc`
    /// (MHBIND21-1), the class the planner splices the member for.
    pub(crate) fn bound_receiver_class_at(&self, pc: usize) -> Option<u32> {
        self.by_cp
            .values()
            .find(|s| s.pcs.iter().any(|&(at, _)| at == pc))
            // j27 wave 6 (lane T1LAM2): a bound STATIC site has no receiver.
            .filter(|s| s.invoke_kind == 4)
            .and_then(|s| s.bound)
            .map(|(_, class)| class)
            .filter(|&class| class != 0)
    }

    /// `(invoke pc, interface class id)` of every INTERFACE handle site, for
    /// its guard's seen-class cell (JIT round 20, MHIFACE19-1).
    pub(crate) fn interface_sites(&self) -> Vec<(usize, u32)> {
        self.by_cp
            .values()
            .filter(|s| s.invoke_kind == 2)
            .flat_map(|s| s.pcs.iter().map(move |&(pc, _)| (pc, s.class_id)))
            .collect()
    }

    /// `invoke_ops` with every planned site's `invokevirtual` row replaced by
    /// an `invokestatic` row keyed by its field's constant-pool index -- or,
    /// for a virtual handle (JIT round 17 wave 3, lane mh4), an
    /// `invokevirtual` row of the member keyed the same way.
    pub(crate) fn rewritten_invoke_ops(&self, invoke_ops: &[(usize, u16, u8)]) -> Vec<(usize, u16, u8)> {
        if self.by_cp.is_empty() {
            return invoke_ops.to_vec();
        }
        let planned: HashMap<usize, (u16, u8)> = self
            .by_cp
            .iter()
            .flat_map(|(&cp, s)| {
                let (key, op) = match (&s.filter, &s.convert, s.invoke_kind) {
                    (Some(f), _, _) if f.main_kind == 3 => (cp, 0xb8),
                    (Some(_), _, _) => (cp, 0xb6),
                    (_, Some(c), _) if c.box_return != 0 => (s.pre_key, 0xb8),
                    (_, Some(c), _) => (
                        cp,
                        match c.main_kind {
                            3 => 0xb8,
                            2 => 0xb9,
                            _ => 0xb6,
                        },
                    ),
                    (None, None, 3 | 6) => (cp, 0xb8),
                    (None, None, 2) => (cp, 0xb9),
                    _ => (cp, 0xb6),
                };
                s.pcs.iter().map(move |&(pc, _)| (pc, (key, op)))
            })
            .collect();
        let pre: HashMap<usize, (usize, u16, u8)> = self
            .by_cp
            .iter()
            .flat_map(|(&cp, s)| {
                let mut rows = Vec::new();
                if let Some(f) = s.filter.as_ref() {
                    let op = if f.pre_kind == 3 { 0xb8 } else { 0xb6 };
                    let key = s.pre_key;
                    for &(pc, getstatic_pc) in &s.pcs {
                        rows.push((pc, (getstatic_pc, key, op)));
                    }
                } else if let Some(c) = s.convert.as_ref().filter(|c| c.box_return != 0) {
                    let op = match c.main_kind {
                        3 => 0xb8,
                        2 => 0xb9,
                        _ => 0xb6,
                    };
                    for &(pc, getstatic_pc) in &s.pcs {
                        rows.push((pc, (getstatic_pc, cp, op)));
                    }
                }
                rows
            })
            .collect();
        let mut out = Vec::with_capacity(invoke_ops.len() + pre.len());
        for &(pc, cp_idx, op) in invoke_ops {
            match planned.get(&pc) {
                Some(&(field_cp, planned_op)) if op == 0xb6 => {
                    if let Some(&(pre_pc, pre_key, pre_op)) = pre.get(&pc) {
                        out.push((pre_pc, pre_key, pre_op));
                    }
                    out.push((pc, field_cp, planned_op));
                }
                _ => out.push((pc, cp_idx, op)),
            }
        }
        out
    }
}

/// `CRATONVM_JIT_MH_INTERFACE_GUARD_SEEN_CLASS` -- perf, **default ON** (JIT
/// round 20, MHIFACE19-1; `=0` is the kill switch): the `instanceof` guarding
/// an interface constant handle's receiver (`CRATONVM_JIT_MH_CONSTANT_INTERFACE`)
/// remembers the first two classes the helper accepted in a per-site cell and
/// answers a receiver of exactly one of them inline (`Op::InstanceOf::seen_cell`),
/// so a monomorphic or bimorphic site pays a load and a compare or two instead
/// of the `jit_instanceof` call. Off, the guard calls the helper on every call, as in round 19. Read
/// per compile, never on a hot path.
pub(crate) fn mh_interface_guard_seen_class_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_MH_INTERFACE_CHA_SPLICE` -- perf, **default ON** (JIT round
/// 20, MHIFACE19-2; `=0` is the kill switch): an interface constant handle's
/// site with no receiver profile takes the class-hierarchy answer (the single
/// loaded implementor of the member) as its guarded splice's class, so a
/// monomorphic hierarchy runs the member inline behind one exact class test.
/// Off, the site stays the guarded `invokeinterface` call. Read per compile.
pub(crate) fn mh_interface_cha_splice_enabled() -> bool {
    true
}

/// The most instructions [`mh_constant_receiver_getstatic`] walks back.
pub(crate) const MH_CONSTANT_RECEIVER_WINDOW: usize = 64;

/// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): may a filter species
/// site's PRE call be built under the pc of its `getstatic` (`getstatic_pc`),
/// the `invokeExact` at `pc`? The call's exception exit and every by-pc table
/// read it at that pc, so no instruction start from the `getstatic` to the
/// `invokeExact` (both included) may be covered by an exception handler
/// (`protected`), and no instruction between them may write a local (a
/// `*store`, `iinc` or `wide`): the frame at the `getstatic` then holds the
/// locals the `invokeExact` sees. Pure.
pub(crate) fn mh_filter_window_admits(
    code: &[u8],
    getstatic_pc: usize,
    pc: usize,
    protected: &dyn Fn(usize) -> bool,
) -> bool {
    if getstatic_pc >= pc || code.get(getstatic_pc).copied() != Some(0xb2) {
        return false;
    }
    let mut at = getstatic_pc;
    while at <= pc {
        if protected(at) {
            return false;
        }
        if at == pc {
            return true;
        }
        if matches!(
            code.get(at).copied(),
            Some(0x36..=0x4e | 0x84 | 0xc4) | None
        ) {
            return false;
        }
        at += crate::bytecode_analysis::step(code, at).max(1);
    }
    false
}

/// `(values popped, values pushed)` of the instruction at `pc`, counting one
/// per value (a `long` / `double` is one), for the straight-line walk of
/// [`mh_constant_receiver_getstatic`]; `None` for anything it does not model
/// (every branch, return, throw, switch, `new`, `wide`, `dup_x*`, `dup2*`,
/// `swap`, monitor and allocation). `invoke_shape(cp)` answers an invoke's
/// `(parameters, returns a value)`.
pub(crate) fn mh_constant_window_effect(
    code: &[u8],
    pc: usize,
    invoke_shape: &dyn Fn(u16) -> Option<(usize, bool)>,
) -> Option<(usize, usize)> {
    let op = *code.get(pc)?;
    let cp =
        || -> Option<u16> { Some(u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?])) };
    Some(match op {
        0x00 | 0x84 => (0, 0),
        // aconst_null .. ldc2_w; the loads.
        0x01..=0x14 | 0x15..=0x2d => (0, 1),
        // The array loads.
        0x2e..=0x35 => (2, 1),
        // The stores and `pop`.
        0x36..=0x4e | 0x57 => (1, 0),
        0x59 => (1, 2),
        // Binary arithmetic, negation, shifts and bit operations.
        0x60..=0x73 | 0x78..=0x83 => (2, 1),
        0x74..=0x77 => (1, 1),
        // Conversions; the comparisons.
        0x85..=0x93 => (1, 1),
        0x94..=0x98 => (2, 1),
        0xb2 => (0, 1),
        0xb4 | 0xbe | 0xc0 | 0xc1 => (1, 1),
        0xb6 | 0xb7 | 0xb8 | 0xb9 => {
            let (params, returns) = invoke_shape(cp()?)?;
            let receiver = usize::from(op != 0xb8);
            (params + receiver, usize::from(returns))
        }
        _ => return None,
    })
}

/// The `getstatic` whose value is the receiver of the `invokevirtual` at
/// `pc`, which takes `site_params` values after its receiver, as `(getstatic
/// pc, field constant-pool index)` -- or `None` unless straight-line code
/// proves it (JIT round 16 wave 17, lane mh7).
///
/// The walk goes back from `pc` one instruction at a time, counting the
/// values above the receiver, through instructions whose stack effect
/// [`mh_constant_window_effect`] models. It succeeds at the instruction that
/// pushes the receiver, which must be a `getstatic` (a `getstatic` pushes
/// exactly one value). It fails at:
///
/// * any instruction start after the `getstatic`, `pc` included, that a
///   branch or an exception handler can reach (`leaders`): another path could
///   bring another receiver;
/// * any instruction it does not model, one that would pop the receiver, or
///   one that pushes it and is not a `getstatic`;
/// * [`MH_CONSTANT_RECEIVER_WINDOW`] instructions.
pub(crate) fn mh_constant_receiver_getstatic(
    code: &[u8],
    code_len: usize,
    pc: usize,
    site_params: usize,
    leaders: &[bool],
    invoke_shape: &dyn Fn(u16) -> Option<(usize, bool)>,
) -> Option<(usize, u16)> {
    mh_constant_receiver_push(code, code_len, pc, site_params, leaders, invoke_shape, false)
}

/// JIT r23 wave 4b, lane L2 (L2W4B-1, `CRATONVM_JIT_MH_CONSTANT_CONDY`):
/// whether the receiver walk also accepts an `ldc` / `ldc_w` as the
/// instruction that pushed the receiver. The VM tells a condy's index from a
/// field's by its constant-pool entry (`CpResolvers::mh_constant`), and the
/// builder admits the site only when that `ldc` was built as the slot read of
/// a resolved condy (`IrBuilder::mh_constant_opcode`). Read per compile.
pub(crate) fn mh_constant_condy_receiver_enabled() -> bool {
    false
}


/// 2026-11-16: [`splice_cold_arm_ranges`] prices code that only several rare
/// edges reach together (`AccessibleObject.verifyAccess`'s `slowVerifyAccess`).
#[cfg(test)]
mod cold_arm_union_tests;

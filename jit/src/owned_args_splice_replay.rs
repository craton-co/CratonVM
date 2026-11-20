// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4 (lane ARGFIX2): the two-level replay rule for the optimizing
//! tier's owned-arguments hand-off at a direct call INSIDE a spliced body
//! (`CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY`, opt-in, a sub-switch of
//! `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS`; proposal
//! `docs/feature-designs/native-dispatch-architecture.md`).
//!
//! An owned site's callee decline becomes THIS physical frame's frameless
//! deopt, which its caller answers by re-running the frame from bci 0. For a
//! non-OSR body the physical frame is the OUTER method's, so the re-run runs
//! the outer method up to the `invoke` its splice is parked at, then every
//! spliced body of the chain up to its own inner `invoke`. It replaces the
//! attempt exactly when no level commits anything on the way:
//!
//! * every level's prefix (closed over loops) passes
//!   [`crate::deopt::owned_args_caller_replay_is_exact`] over that level's own
//!   interpreter code -- the outer method's, and each spliced body's UNMASKED
//!   copy (`lib.rs` `spliced_body_codes`; a key without one refuses);
//! * no exception row of a level covers its parked pc;
//! * no level holds a lock (the caller passes whether any synchronized splice
//!   or self lock exists in the compile; explicit monitor ops are commits);
//! * the compile is not OSR (no bci-0 state to re-run).
//!
//! Pure: the lowerer hands in the chain (`IrInlineFrameSites::bodies_enclosing`)
//! and the codes; nothing here reads a switch.

use crate::ir_lower::SplicedBodyCode;

/// One level of the chain a re-run from the outer method's bci 0 replays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplayLevel<'a> {
    /// The level's interpreter code; `None` for a spliced body with no
    /// unmasked copy.
    pub(crate) code: Option<&'a [u8]>,
    /// `(start_pc, end_pc, handler_pc)`, in the level's own coordinates.
    pub(crate) handlers: &'a [(usize, usize, usize)],
    /// The `invoke` the level is parked at (the call itself for the innermost).
    pub(crate) parked_bci: usize,
    /// The combined-buffer pc of the level's bci 0 (`0` for the outer method).
    pub(crate) base: usize,
    /// Parameter slots, receiver included (`SplicedBodyCode::num_params`);
    /// `0` for the outer method and for a body with no unmasked copy. Read
    /// only by [`level_replay_plan`].
    pub(crate) num_params: usize,
}

/// Why a spliced call keeps its arguments. Level `0` is the outer method,
/// `1` the outermost spliced body, the last one the body making the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChainRefusal {
    Osr,
    Lock,
    /// The site tables do not describe a chain this rule can read.
    NoChain,
    NoUnmaskedCopy(usize),
    NotParkedAtInvoke(usize),
    Handler(usize),
    Commit(usize),
    /// j27 wave 6 ([`level_replay_plan`]): the level's prefix stores into one
    /// of its parameter slots, so its entry state is not in its frame state.
    ParamWritten(usize),
}

/// The chain for the call at combined pc `pc`, OUTER FIRST, or `None` when
/// `enclosing` (innermost first, `(base, code_len, caller_pc, method_key)`)
/// is empty or inconsistent: each body must contain the pc its child is
/// parked at, and the outermost caller pc must lie in `outer_code`.
pub(crate) fn replay_levels<'a>(
    pc: usize,
    enclosing: &[(usize, usize, usize, &str)],
    outer_code: &'a [u8],
    outer_handlers: &'a [(usize, usize, usize)],
    bodies: &'a [SplicedBodyCode],
) -> Option<Vec<ReplayLevel<'a>>> {
    if enclosing.is_empty() {
        return None;
    }
    let mut levels = Vec::with_capacity(enclosing.len() + 1);
    let mut child_pc = pc;
    for &(base, code_len, caller_pc, key) in enclosing {
        if child_pc < base || child_pc >= base.checked_add(code_len)? {
            return None;
        }
        let body = bodies
            .iter()
            .find(|b| b.method_key == key && b.code.len() == code_len);
        levels.push(ReplayLevel {
            code: body.map(|b| b.code.as_slice()),
            handlers: body.map_or(&[][..], |b| b.handlers.as_slice()),
            parked_bci: child_pc - base,
            base,
            num_params: body.map_or(0, |b| b.num_params),
        });
        child_pc = caller_pc;
    }
    if child_pc >= outer_code.len() {
        return None;
    }
    levels.push(ReplayLevel {
        code: Some(outer_code),
        handlers: outer_handlers,
        parked_bci: child_pc,
        base: 0,
        num_params: 0,
    });
    levels.reverse();
    Some(levels)
}

/// `None` when a re-run from the outer method's bci 0 replaces an attempt
/// stopped at the innermost level's call; else the first refusal found.
pub(crate) fn chain_replay_refusal(
    levels: &[ReplayLevel<'_>],
    osr_compile: bool,
    any_level_locks: bool,
) -> Option<ChainRefusal> {
    if osr_compile {
        return Some(ChainRefusal::Osr);
    }
    if any_level_locks {
        return Some(ChainRefusal::Lock);
    }
    if levels.len() < 2 {
        return Some(ChainRefusal::NoChain);
    }
    levels
        .iter()
        .enumerate()
        .find_map(|(i, level)| level_refusal(i, level))
}

/// The single-level terms for level `i`.
fn level_refusal(i: usize, level: &ReplayLevel<'_>) -> Option<ChainRefusal> {
    let Some(code) = level.code else {
        return Some(ChainRefusal::NoUnmaskedCopy(i));
    };
    let bci = level.parked_bci;
    if !code.get(bci).is_some_and(|&op| (0xb6..=0xba).contains(&op)) {
        return Some(ChainRefusal::NotParkedAtInvoke(i));
    }
    if level.handlers.iter().any(|&(start, end, _)| start <= bci && bci < end) {
        return Some(ChainRefusal::Handler(i));
    }
    let exact = u32::try_from(bci).is_ok_and(|bci| {
        crate::deopt::owned_args_caller_replay_is_exact(code, code.len(), bci, level.handlers)
    });
    (!exact).then_some(ChainRefusal::Commit(i))
}

/// The whole rule for the lowerer: `Ok(the call's opcode)` (read from the
/// innermost body's unmasked code) when the chain admits the hand-off.
pub(crate) fn spliced_call_replay(
    pc: usize,
    enclosing: &[(usize, usize, usize, &str)],
    outer_code: &[u8],
    outer_handlers: &[(usize, usize, usize)],
    bodies: &[SplicedBodyCode],
    osr_compile: bool,
    any_level_locks: bool,
) -> Result<u8, ChainRefusal> {
    let levels = replay_levels(pc, enclosing, outer_code, outer_handlers, bodies)
        .ok_or(ChainRefusal::NoChain)?;
    if let Some(refusal) = chain_replay_refusal(&levels, osr_compile, any_level_locks) {
        return Err(refusal);
    }
    levels
        .last()
        .and_then(|l| l.code?.get(l.parked_bci).copied())
        .ok_or(ChainRefusal::NoChain)
}

// ── The level replay (j27 wave 6, lane T1SPLICE; census only) ───────────────
//
// t1-ARGFIX item 3's other route. The rule above re-runs the PHYSICAL frame,
// so it needs an exact outer prefix and refuses every OSR body. A decline
// could instead resume the interpreter with levels `0..k` rebuilt from the
// call's chain state, every one parked at the invoke it already stands at
// (nothing ran in those frames since), and level `k` (a spliced body, `k >= 1`)
// rebuilt at bci 0 with only its parameter slots, taken from its own scope of
// that state. Levels `k..` are then re-run by the interpreter. That replaces
// the attempt exactly when:
//
// * every level from `k` to the call passes the single-level terms
//   ([`level_refusal`]: `deopt::owned_args_caller_replay_is_exact` over its own
//   code, no handler row at its parked pc) -- the same one rule, asked only of
//   the levels that are re-run;
// * level `k`'s prefix (closed over loops and its handler rows) writes none of
//   its parameter slots, so its scope's slots at the call still hold its entry
//   arguments ([`prefix_keeps_param_slots`]);
// * no lock exists in the compile (as above).
//
// An OSR outer frame is never re-run here, so OSR does not refuse. Level `0`
// is the physical re-run itself (the two-level rule), answered when it holds.
// NOT WIRED: nothing in the emitter or the VM resumes this way yet (the owned
// site's decline is still the frameless `Handoff`), so the plan is printed
// on the `[ir-owned-args] site` line and decides nothing. The wiring is listed
// on `docs/feature-designs/native-dispatch-architecture.md`.

/// The local slot an instruction writes (`*store*`, `iinc`, plain or `wide`),
/// `None` for every other instruction. A two-word store writes `slot` and
/// `slot + 1`; a caller comparing against a parameter count needs only `slot`.
fn written_local_slot(code: &[u8], insn: &crate::bytecode_analysis::Insn) -> Option<usize> {
    let pc = insn.pc;
    let index = |at: usize| -> Option<usize> {
        if insn.wide {
            let hi = usize::from(*code.get(at + 1)?);
            Some((hi << 8) | usize::from(*code.get(at + 2)?))
        } else {
            code.get(at + 1).map(|&b| usize::from(b))
        }
    };
    // With `wide` the operand starts one byte later (after the prefix).
    let at = if insn.wide { pc + 1 } else { pc };
    match insn.op {
        0x36..=0x3a | 0x84 => index(at),
        0x3b..=0x4e => Some(usize::from((insn.op - 0x3b) % 4)),
        _ => None,
    }
}

/// Does `code[..extent]` -- every instruction that can run before control
/// (re)arrives at `parked`, over loops and `handlers` -- leave the slots
/// `0..num_params` unwritten? `false` when the walk loses instruction sync.
pub(crate) fn prefix_keeps_param_slots(
    code: &[u8],
    parked: usize,
    num_params: usize,
    handlers: &[(usize, usize, usize)],
) -> bool {
    let len = code.len();
    if parked >= len {
        return false;
    }
    let extent = crate::ir::replay_committed_extent_with_handlers(code, len, parked, handlers);
    let mut pc = 0usize;
    while pc < extent {
        let Some(insn) = crate::bytecode_analysis::decode_at(code, len, pc) else {
            return false;
        };
        if written_local_slot(code, &insn).is_some_and(|slot| slot < num_params) {
            return false;
        }
        pc = insn.next_pc();
    }
    true
}

/// The outermost level `k` a decline at the innermost call could resume from
/// (see the section above), outer first as [`replay_levels`] orders them:
/// `Ok(0)` exactly when [`chain_replay_refusal`] admits the physical re-run,
/// else the smallest spliced level `k >= 1` whose re-run is exact, else the
/// refusal that bounds every `k`.
pub(crate) fn level_replay_plan(
    levels: &[ReplayLevel<'_>],
    osr_compile: bool,
    any_level_locks: bool,
) -> Result<usize, ChainRefusal> {
    if any_level_locks {
        return Err(ChainRefusal::Lock);
    }
    if levels.len() < 2 {
        return Err(if osr_compile { ChainRefusal::Osr } else { ChainRefusal::NoChain });
    }
    if chain_replay_refusal(levels, osr_compile, false).is_none() {
        return Ok(0);
    }
    // The re-run levels form a suffix: every `k` needs `k..` to pass.
    let mut k_min = levels.len();
    for j in (1..levels.len()).rev() {
        if let Some(refusal) = level_refusal(j, &levels[j]) {
            if k_min == levels.len() {
                return Err(refusal);
            }
            break;
        }
        k_min = j;
    }
    (k_min..levels.len())
        .find(|&k| {
            let l = &levels[k];
            l.code.is_some_and(|c| prefix_keeps_param_slots(c, l.parked_bci, l.num_params, l.handlers))
        })
        .ok_or(ChainRefusal::ParamWritten(k_min))
}

/// [`level_replay_plan`] for the lowerer's census, with the inputs of
/// [`spliced_call_replay`]. A site in no splice answers `Osr` in an OSR
/// compile (its only level is the frame that cannot re-run), else `NoChain`.
pub(crate) fn spliced_call_level_replay(
    pc: usize,
    enclosing: &[(usize, usize, usize, &str)],
    outer_code: &[u8],
    outer_handlers: &[(usize, usize, usize)],
    bodies: &[SplicedBodyCode],
    osr_compile: bool,
    any_level_locks: bool,
) -> Result<usize, ChainRefusal> {
    if enclosing.is_empty() && osr_compile {
        return Err(ChainRefusal::Osr);
    }
    let levels = replay_levels(pc, enclosing, outer_code, outer_handlers, bodies)
        .ok_or(ChainRefusal::NoChain)?;
    level_replay_plan(&levels, osr_compile, any_level_locks)
}

// ── The window half (j27 wave 4b, lane ARGFIX2) ─────────────────────────────
//
// `ir_lower`'s `snapshots_readable_after_call` / `owned_call_arg_window` at a
// spliced owned site, under the same sub-switch (page
// `j27-argfix2-owned-arg-window-refuses-every-spliced-call-FIXED-20261029.md`): the
// frame states readable after the call are judged per level of the chain, so
// the call's own chain snapshot (its arguments on the operand stack) is no
// longer read just because a later node shares the splice's outer `invoke`.

/// The bcis reachable from the successors of the `invoke` at `parked`, over
/// normal edges and the `handlers` rows covering a reached bci, or `None`
/// when the walk comes back to `parked` (the call is on a cycle of its level).
/// `parked` itself is never in the set. The walk the single-level window
/// always made over the compiling method's code, shared by every level.
pub(crate) fn reach_after_invoke(
    parked: usize,
    successors: impl Fn(usize) -> Vec<usize>,
    handlers: &[(usize, usize, usize)],
) -> Option<std::collections::HashSet<usize>> {
    let (reach, call_cycle) = reach_after_invoke_with_cycle(parked, successors, handlers)?;
    (!call_cycle).then_some(reach)
}

/// The bytecode tail after `parked`, with a separate bit for a path that
/// returns to `parked`. Unlike [`reach_after_invoke`], this keeps the
/// reachable tail when the invoke is in a loop so per-argument liveness can
/// prove that a same-iteration definition kills the prior iteration's value.
pub(crate) fn reach_after_invoke_with_cycle(
    parked: usize,
    successors: impl Fn(usize) -> Vec<usize>,
    handlers: &[(usize, usize, usize)],
) -> Option<(std::collections::HashSet<usize>, bool)> {
    let mut reach = std::collections::HashSet::new();
    let mut work = successors(parked);
    let mut call_cycle = false;
    while let Some(p) = work.pop() {
        if p == parked {
            call_cycle = true;
            continue;
        }
        if !reach.insert(p) {
            continue;
        }
        work.extend(successors(p));
        for &(start, end, handler) in handlers {
            if start <= p && p < end {
                work.push(handler);
            }
        }
    }
    Some((reach, call_cycle))
}

/// Is the subgraph of the blocks marked `seen` free of cycles? (Kahn's walk
/// over the edges between seen blocks; an edge counted twice is released
/// twice.) The per-level readable set is used only after the call's block on
/// an acyclic tail, where no back-edge poll can re-read a state of the splice.
pub(crate) fn seen_blocks_acyclic<'s>(
    seen: &[bool],
    successors: impl Fn(usize) -> &'s [usize],
) -> bool {
    let n = seen.len();
    let inside = |s: usize| s < n && seen[s];
    let mut indegree = vec![0usize; n];
    for b in (0..n).filter(|&b| seen[b]) {
        for &s in successors(b).iter().filter(|&&s| inside(s)) {
            indegree[s] += 1;
        }
    }
    let mut ready: Vec<usize> = (0..n).filter(|&b| seen[b] && indegree[b] == 0).collect();
    let mut released = 0usize;
    while let Some(b) = ready.pop() {
        released += 1;
        for &s in successors(b).iter().filter(|&&s| inside(s)) {
            indegree[s] -= 1;
            if indegree[s] == 0 {
                ready.push(s);
            }
        }
    }
    released == seen.iter().filter(|&&x| x).count()
}

/// Does a frame state name `a` where a resume reads it? A local slot `k` is
/// not read when `dead_locals[k]` (a chain scope 0 local its body's liveness
/// calls dead) or `own_local_dead(k)` (the compiling method's own liveness at
/// its own instruction); an operand-stack entry `k` is not read when
/// `dead_stack[k]` (a caller scope's local riding in the stack). Empty slices
/// narrow nothing. Monitors are always read.
pub(crate) fn frame_names_value(
    locals: &[crate::ir::NodeId],
    stack: &[crate::ir::NodeId],
    monitors: &[crate::ir::NodeId],
    a: crate::ir::NodeId,
    dead: (&[bool], &[bool]),
    own_local_dead: impl Fn(usize) -> bool,
) -> bool {
    let (dead_locals, dead_stack) = dead;
    let read = |slots: &[bool], k: usize| !slots.get(k).copied().unwrap_or(false);
    locals
        .iter()
        .enumerate()
        .any(|(k, &v)| v == a && read(dead_locals, k) && !own_local_dead(k))
        || stack
            .iter()
            .enumerate()
            .any(|(k, &v)| v == a && read(dead_stack, k))
        || monitors.contains(&a)
}

/// Is a CHAIN snapshot at combined pc `bci` readable after a spliced owned
/// call, beyond the exact-pc readers? When `bci` or any caller pc of the
/// bodies enclosing it (`ancestors`) is a bci some level can reach after the
/// call (`reach`, combined coordinates): a body spliced at a later `invoke`
/// is reached through that `invoke`.
pub(crate) fn chain_state_reached(
    bci: usize,
    ancestors: &[usize],
    reach: &std::collections::HashSet<usize>,
) -> bool {
    reach.contains(&bci) || ancestors.iter().any(|p| reach.contains(p))
}

/// j27 OOM21 (`CRATONVM_JIT_OWNED_ARGS_CONST_STATIC_STORES`): the
/// optimizing tier's `plain_static` answer for
/// `crate::deopt::owned_args_caller_replay_is_exact_with_statics`, from the
/// ops of the graph nodes the builder gave the `putstatic`'s pc: `true` when
/// at least one is an `Op::StoreStatic` and none is volatile. No node (a
/// rowless site the builder trapped, a store it never built) answers `false`.
pub(crate) fn plain_static_store<'a>(
    ops_at_pc: impl IntoIterator<Item = &'a crate::ir::Op>,
) -> bool {
    let mut seen = false;
    for op in ops_at_pc {
        if let crate::ir::Op::StoreStatic { is_volatile, .. } = op {
            if *is_volatile {
                return false;
            }
            seen = true;
        }
    }
    seen
}

#[cfg(test)]
mod j27_oom21_plain_static_tests;

#[cfg(test)]
mod j27_argfix2_window_tests;

#[cfg(test)]
mod j27_argfix2_splice_replay_tests;

#[cfg(test)]
mod j27_t1splice_level_replay_tests;

#[cfg(test)]
mod j27_w8arg_window_coupling_tests;

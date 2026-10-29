//! j27 wave 4 (lane ARGFIX2): the two-level replay rule for the optimizing
//! tier's owned-arguments hand-off at a direct call INSIDE a spliced body
//! (`CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY`, opt-in, a sub-switch of
//! `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS`; proposal
//! `docs/known-issues/jit/j27-argfix-spliced-owned-args-two-level-replay-proposal-20261028.md`).
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
// on `docs/known-issues/jit/t1-ARGFIX-ir-owned-args-never-reach-osr-or-spliced-callers-20261027.md`.

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
    let mut reach = std::collections::HashSet::new();
    let mut work = successors(parked);
    while let Some(p) = work.pop() {
        if p == parked {
            return None;
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
    Some(reach)
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
mod j27_oom21_plain_static_tests {
    use super::plain_static_store;
    use crate::ir::Op;

    fn store(is_volatile: bool) -> Op {
        Op::StoreStatic {
            class_id: 3,
            field_index: 1,
            type_tag: b'L',
            is_volatile,
        }
    }

    #[test]
    fn only_a_built_non_volatile_store_vouches() {
        assert!(plain_static_store([&store(false)]));
        assert!(!plain_static_store([&store(true)]));
        assert!(!plain_static_store([&store(false), &store(true)]));
        assert!(!plain_static_store(std::iter::empty::<&Op>()));
    }
}

#[cfg(test)]
mod j27_argfix2_window_tests {
    use super::*;

    fn verified(code: &[u8]) -> std::sync::Arc<cratonvm_reader::VerifiedCode> {
        cratonvm_reader::verified_code(code).expect("test bytecode verifies")
    }

    /// `mid`: `0 aload_0  1 iload_1  2 invokestatic #1  5 iconst_1  6 iadd
    /// 7 ireturn` -- after the call only `5..8` runs.
    #[test]
    fn j27_argfix2_reach_after_invoke_is_the_tail_of_the_level() {
        let code = [0x2a, 0x1b, 0xb8, 0x00, 0x01, 0x04, 0x60, 0xac];
        let v = verified(&code);
        let reach = reach_after_invoke(2, |p| v.successors(p), &[]).unwrap();
        let mut got: Vec<usize> = reach.into_iter().collect();
        got.sort_unstable();
        assert_eq!(got, vec![5, 6, 7]);
    }

    /// `0 aload_0  1 invokestatic #1  4 goto 0` -- the call is on a cycle.
    #[test]
    fn j27_argfix2_reach_after_invoke_refuses_a_cycle_and_follows_handlers() {
        let looping = [0x2a, 0xb8, 0x00, 0x01, 0xa7, 0xff, 0xfc];
        let v = verified(&looping);
        assert!(reach_after_invoke(1, |p| v.successors(p), &[]).is_none());
        // A handler row covering the tail and pointing back at the invoke
        // closes the same cycle over an exception edge.
        let edges = |p: usize| if p == 1 { vec![4] } else { Vec::new() };
        assert!(reach_after_invoke(1, edges, &[(4, 5, 1)]).is_none());
        assert_eq!(reach_after_invoke(1, edges, &[(4, 5, 6)]).map(|r| r.len()), Some(2));
    }

    #[test]
    fn j27_argfix2_seen_blocks_acyclic() {
        let succ: Vec<Vec<usize>> = vec![vec![1, 2], vec![3], vec![3], vec![]];
        let at = |b: usize| succ[b].as_slice();
        assert!(seen_blocks_acyclic(&[false, true, true, true], at));
        let looped: Vec<Vec<usize>> = vec![vec![1], vec![2], vec![1, 3], vec![]];
        let at = |b: usize| looped[b].as_slice();
        assert!(!seen_blocks_acyclic(&[false, true, true, true], at));
        // A cycle through an unseen block is not in the subgraph.
        assert!(seen_blocks_acyclic(&[false, false, true, true], at));
    }

    #[test]
    fn j27_argfix2_frame_names_value_narrows_only_dead_slots() {
        let none = |_: usize| false;
        // On the stack of the call's own state: named.
        assert!(frame_names_value(&[], &[7], &[], 7, (&[], &[]), none));
        // A caller scope's local riding in the stack, dead there: not named.
        assert!(!frame_names_value(&[], &[7], &[], 7, (&[], &[true]), none));
        // A scope-0 local dead by the body's liveness: not named.
        assert!(!frame_names_value(&[7, 8], &[], &[], 7, (&[true, false], &[]), none));
        assert!(frame_names_value(&[7, 8], &[], &[], 8, (&[true, false], &[]), none));
        // The own-instruction liveness, as the single-level window had it.
        assert!(!frame_names_value(&[7], &[], &[], 7, (&[], &[]), |k| k == 0));
        // A monitor is always read.
        assert!(frame_names_value(&[], &[], &[7], 7, (&[], &[true]), none));
        assert!(!frame_names_value(&[1], &[2], &[3], 7, (&[], &[]), none));
    }

    #[test]
    fn j27_argfix2_chain_state_reached_through_an_ancestor() {
        let reach: std::collections::HashSet<usize> = [105, 106, 107].into_iter().collect();
        assert!(chain_state_reached(106, &[2], &reach), "the body's own tail");
        assert!(chain_state_reached(203, &[106, 2], &reach), "a body spliced after the call");
        assert!(!chain_state_reached(102, &[2], &reach), "the call's own state");
        assert!(!chain_state_reached(100, &[2], &reach), "a state before the call");
    }

    #[test]
    fn j27_argfix2_levels_carry_their_combined_base() {
        let mid = SplicedBodyCode {
            method_key: "P.mid:(Ljava/lang/Object;)I".to_string(),
            code: vec![0x2a, 0xb8, 0x00, 0x01, 0xac],
            num_params: 1,
            num_locals: 1,
            handlers: Vec::new(),
        };
        let outer = [0x2a, 0xb8, 0x00, 0x01, 0xac];
        let enclosing = [(100usize, 5usize, 1usize, "P.mid:(Ljava/lang/Object;)I")];
        let levels = replay_levels(101, &enclosing, &outer, &[], std::slice::from_ref(&mid)).unwrap();
        assert_eq!(levels.iter().map(|l| l.base).collect::<Vec<_>>(), vec![0, 100]);
    }
}

#[cfg(test)]
mod j27_argfix2_splice_replay_tests {
    use super::*;

    /// `static int outer(Object o) { return mid(o); }`:
    /// `0 aload_0  1 invokestatic #1  4 ireturn`.
    const PURE: [u8; 5] = [0x2a, 0xb8, 0x00, 0x01, 0xac];
    /// `{ S.f = 1; return mid(o); }`:
    /// `0 iconst_1  1 putstatic #2  4 aload_0  5 invokestatic #1  8 ireturn`.
    const COMMITS: [u8; 9] = [0x04, 0xb3, 0x00, 0x02, 0x2a, 0xb8, 0x00, 0x01, 0xac];
    /// `{ synchronized (o) { ... } }` prefix: `0 aload_0  1 monitorenter
    ///  2 aload_0  3 invokestatic #1  6 ireturn`.
    const LOCKS: [u8; 7] = [0x2a, 0xc2, 0x2a, 0xb8, 0x00, 0x01, 0xac];
    const MID: &str = "P.mid:(Ljava/lang/Object;)I";
    const LEAF: &str = "P.leaf:(Ljava/lang/Object;)I";

    fn body(key: &str, code: &[u8], handlers: Vec<(usize, usize, usize)>) -> SplicedBodyCode {
        SplicedBodyCode {
            method_key: key.to_string(),
            code: code.to_vec(),
            num_params: 1,
            num_locals: 1,
            handlers,
        }
    }

    /// `mid` spliced at base 100, its call to `leaf` at body bci `bci`.
    fn run(
        outer: &[u8],
        outer_handlers: &[(usize, usize, usize)],
        mid: Option<SplicedBodyCode>,
        bci: usize,
        osr: bool,
        locks: bool,
    ) -> Result<u8, ChainRefusal> {
        let len = mid.as_ref().map_or(PURE.len(), |b| b.code.len());
        let outer_bci = outer.iter().position(|&op| op == 0xb8).unwrap();
        let enclosing = [(100usize, len, outer_bci, MID)];
        let bodies: Vec<SplicedBodyCode> = mid.into_iter().collect();
        spliced_call_replay(100 + bci, &enclosing, outer, outer_handlers, &bodies, osr, locks)
    }

    #[test]
    fn j27_argfix2_two_level_pure_chain_is_admitted() {
        let mid = body(MID, &PURE, Vec::new());
        assert_eq!(run(&PURE, &[], Some(mid), 1, false, false), Ok(0xb8));
    }

    #[test]
    fn j27_argfix2_three_level_pure_chain_is_admitted_and_orders_outer_first() {
        // outer@1 -> mid at 100 (its call at 101) -> leaf at 200 (call at 201).
        let bodies = [body(MID, &PURE, Vec::new()), body(LEAF, &PURE, Vec::new())];
        let enclosing = [(200usize, 5usize, 101usize, LEAF), (100, 5, 1, MID)];
        let levels = replay_levels(201, &enclosing, &PURE, &[], &bodies).unwrap();
        assert_eq!(levels.iter().map(|l| l.parked_bci).collect::<Vec<_>>(), vec![1, 1, 1]);
        assert_eq!(chain_replay_refusal(&levels, false, false), None);
        assert_eq!(
            spliced_call_replay(201, &enclosing, &PURE, &[], &bodies, false, false),
            Ok(0xb8)
        );
    }

    #[test]
    fn j27_argfix2_commit_at_outer_level_refuses() {
        let mid = body(MID, &PURE, Vec::new());
        assert_eq!(run(&COMMITS, &[], Some(mid), 1, false, false), Err(ChainRefusal::Commit(0)));
    }

    #[test]
    fn j27_argfix2_commit_at_inner_level_refuses() {
        let mid = body(MID, &COMMITS, Vec::new());
        assert_eq!(run(&PURE, &[], Some(mid), 5, false, false), Err(ChainRefusal::Commit(1)));
    }

    #[test]
    fn j27_argfix2_handler_at_either_level_refuses() {
        let mid = body(MID, &PURE, Vec::new());
        assert_eq!(
            run(&PURE, &[(0, 4, 4)], Some(mid), 1, false, false),
            Err(ChainRefusal::Handler(0))
        );
        let guarded = body(MID, &PURE, vec![(1, 4, 4)]);
        assert_eq!(run(&PURE, &[], Some(guarded), 1, false, false), Err(ChainRefusal::Handler(1)));
    }

    #[test]
    fn j27_argfix2_lock_refuses() {
        let mid = body(MID, &PURE, Vec::new());
        assert_eq!(run(&PURE, &[], Some(mid), 1, false, true), Err(ChainRefusal::Lock));
        // An explicit monitorenter on the way is a commit at its level.
        let locking = body(MID, &LOCKS, Vec::new());
        assert_eq!(run(&PURE, &[], Some(locking), 3, false, false), Err(ChainRefusal::Commit(1)));
    }

    #[test]
    fn j27_argfix2_osr_refuses() {
        let mid = body(MID, &PURE, Vec::new());
        assert_eq!(run(&PURE, &[], Some(mid), 1, true, false), Err(ChainRefusal::Osr));
    }

    #[test]
    fn j27_argfix2_missing_unmasked_copy_refuses() {
        assert_eq!(run(&PURE, &[], None, 1, false, false), Err(ChainRefusal::NoUnmaskedCopy(1)));
        // A copy whose length disagrees with the spliced extent is no copy.
        let short = body(MID, &PURE[..4], Vec::new());
        let enclosing = [(100usize, 5usize, 1usize, MID)];
        assert_eq!(
            spliced_call_replay(101, &enclosing, &PURE, &[], &[short], false, false),
            Err(ChainRefusal::NoUnmaskedCopy(1))
        );
    }

    #[test]
    fn j27_argfix2_malformed_chain_refuses() {
        let bodies = [body(MID, &PURE, Vec::new())];
        // pc outside the innermost body; outer caller pc past the outer code;
        // no splice at all.
        for (pc, enclosing) in [
            (99usize, vec![(100usize, 5usize, 1usize, MID)]),
            (101, vec![(100, 5, 40, MID)]),
            (101, Vec::new()),
        ] {
            assert_eq!(
                spliced_call_replay(pc, &enclosing, &PURE, &[], &bodies, false, false),
                Err(ChainRefusal::NoChain)
            );
        }
        // The call byte itself must be an invoke in the unmasked copy.
        let enclosing = [(100usize, 5usize, 1usize, MID)];
        assert_eq!(
            spliced_call_replay(100, &enclosing, &PURE, &[], &bodies, false, false),
            Err(ChainRefusal::NotParkedAtInvoke(1))
        );
    }
}

#[cfg(test)]
mod j27_t1splice_level_replay_tests {
    use super::*;

    /// `0 aload_0  1 invokestatic #1  4 ireturn` (call at 1).
    const PURE: [u8; 5] = [0x2a, 0xb8, 0x00, 0x01, 0xac];
    /// `0 iconst_1  1 putstatic #2  4 aload_0  5 invokestatic #1  8 ireturn`.
    const COMMITS: [u8; 9] = [0x04, 0xb3, 0x00, 0x02, 0x2a, 0xb8, 0x00, 0x01, 0xac];
    /// `0 aload_0  1 astore_0  2 aload_0  3 invokestatic #1  6 ireturn`.
    const WRITES_PARAM: [u8; 7] = [0x2a, 0x4b, 0x2a, 0xb8, 0x00, 0x01, 0xac];
    /// `0 aload_0  1 wide astore 0  5 aload_0  6 invokestatic #1  9 ireturn`.
    const WIDE_WRITE: [u8; 10] = [0x2a, 0xc4, 0x3a, 0x00, 0x00, 0x2a, 0xb8, 0x00, 0x01, 0xac];
    /// `0 iinc 0 1  3 iload_0  4 invokestatic #1  7 ireturn`.
    const IINC: [u8; 8] = [0x84, 0x00, 0x01, 0x1a, 0xb8, 0x00, 0x01, 0xac];
    /// `0 aload_0  1 astore_1  2 aload_1  3 invokestatic #1  6 ireturn`.
    const WRITES_LOCAL: [u8; 7] = [0x2a, 0x4c, 0x2b, 0xb8, 0x00, 0x01, 0xac];
    /// `0 aload_0  1 invokestatic #1  4 pop  5 aload_0  6 astore_0  7 goto 0`.
    const LOOP: [u8; 10] = [0x2a, 0xb8, 0x00, 0x01, 0x57, 0x2a, 0x4b, 0xa7, 0xff, 0xf9];
    const MID: &str = "P.mid:(Ljava/lang/Object;)I";
    const LEAF: &str = "P.leaf:(Ljava/lang/Object;)I";

    fn body(key: &str, code: &[u8], handlers: Vec<(usize, usize, usize)>) -> SplicedBodyCode {
        SplicedBodyCode {
            method_key: key.to_string(),
            code: code.to_vec(),
            num_params: 1,
            num_locals: 2,
            handlers,
        }
    }

    fn call_bci(code: &[u8]) -> usize {
        code.iter().position(|&op| op == 0xb8).unwrap()
    }

    /// `outer -> mid` (base 100), the call in `mid`.
    fn two(outer: &[u8], mid: SplicedBodyCode, osr: bool) -> Result<usize, ChainRefusal> {
        let enclosing = [(100usize, mid.code.len(), call_bci(outer), MID)];
        let pc = 100 + call_bci(&mid.code);
        spliced_call_level_replay(pc, &enclosing, outer, &[], &[mid], osr, false)
    }

    /// `outer -> mid` (base 100) `-> leaf` (base 200), the call in `leaf`.
    fn three(outer: &[u8], mid: &[u8], leaf: &[u8], osr: bool) -> Result<usize, ChainRefusal> {
        let bodies = [body(MID, mid, Vec::new()), body(LEAF, leaf, Vec::new())];
        let enclosing = [
            (200usize, leaf.len(), 100 + call_bci(mid), LEAF),
            (100, mid.len(), call_bci(outer), MID),
        ];
        let pc = 200 + call_bci(leaf);
        spliced_call_level_replay(pc, &enclosing, outer, &[], &bodies, osr, false)
    }

    #[test]
    fn j27_t1splice_osr_pure_chain_resumes_at_the_spliced_level() {
        assert_eq!(two(&PURE, body(MID, &PURE, Vec::new()), true), Ok(1));
        // The physical re-run refuses the same site.
        let mid = body(MID, &PURE, Vec::new());
        let enclosing = [(100usize, 5usize, 1usize, MID)];
        assert_eq!(
            spliced_call_replay(101, &enclosing, &PURE, &[], &[mid], true, false),
            Err(ChainRefusal::Osr)
        );
    }

    #[test]
    fn j27_t1splice_an_exact_chain_is_the_physical_rerun() {
        assert_eq!(two(&PURE, body(MID, &PURE, Vec::new()), false), Ok(0));
        // A written parameter does not matter to a re-run of the whole frame.
        assert_eq!(two(&PURE, body(MID, &WRITES_PARAM, Vec::new()), false), Ok(0));
    }

    #[test]
    fn j27_t1splice_an_outer_commit_moves_the_plan_to_level_1() {
        assert_eq!(two(&COMMITS, body(MID, &PURE, Vec::new()), false), Ok(1));
    }

    #[test]
    fn j27_t1splice_a_written_parameter_slot_refuses_its_level() {
        for code in [&WRITES_PARAM[..], &WIDE_WRITE[..], &IINC[..]] {
            assert_eq!(
                two(&PURE, body(MID, code, Vec::new()), true),
                Err(ChainRefusal::ParamWritten(1)),
                "{code:02x?}"
            );
        }
        // A store into a non-parameter local keeps the entry state.
        assert_eq!(two(&PURE, body(MID, &WRITES_LOCAL, Vec::new()), true), Ok(1));
    }

    #[test]
    fn j27_t1splice_three_levels_pick_the_outermost_recoverable_level() {
        assert_eq!(three(&COMMITS, &WRITES_PARAM, &PURE, true), Ok(2));
        assert_eq!(three(&COMMITS, &PURE, &PURE, true), Ok(1));
        // A committing middle level is resumed, not re-run.
        assert_eq!(three(&PURE, &COMMITS, &PURE, true), Ok(2));
        // The innermost level is re-run in every plan.
        assert_eq!(three(&PURE, &PURE, &COMMITS, true), Err(ChainRefusal::Commit(2)));
    }

    #[test]
    fn j27_t1splice_handler_lock_and_unspliced_sites_refuse() {
        assert_eq!(
            two(&PURE, body(MID, &PURE, vec![(1, 4, 4)]), true),
            Err(ChainRefusal::Handler(1))
        );
        let mid = body(MID, &PURE, Vec::new());
        let enclosing = [(100usize, 5usize, 1usize, MID)];
        assert_eq!(
            spliced_call_level_replay(101, &enclosing, &PURE, &[], &[mid], true, true),
            Err(ChainRefusal::Lock)
        );
        assert_eq!(spliced_call_level_replay(1, &[], &PURE, &[], &[], true, false), Err(ChainRefusal::Osr));
        assert_eq!(
            spliced_call_level_replay(1, &[], &PURE, &[], &[], false, false),
            Err(ChainRefusal::NoChain)
        );
    }

    #[test]
    fn j27_t1splice_param_slots_are_checked_over_the_loop_closed_extent() {
        // Textually the prefix `0..1` writes nothing; the loop brings the
        // `astore_0` at 6 back before the call.
        assert!(!prefix_keeps_param_slots(&LOOP, 1, 1, &[]));
        assert!(prefix_keeps_param_slots(&WRITES_PARAM, 3, 0, &[]));
        assert!(!prefix_keeps_param_slots(&WRITES_PARAM, 3, 1, &[]));
        assert!(prefix_keeps_param_slots(&WRITES_LOCAL, 3, 1, &[]));
        // A parked pc past the code is no answer.
        assert!(!prefix_keeps_param_slots(&PURE, 9, 1, &[]));
    }
}

#[cfg(test)]
mod j27_w8arg_window_coupling_tests {
    //! j27 wave 8 (lane W8ARG). dev's service-copy window (gkr,
    //! `deopt::rerun_window_closed_at` on origin/dev) places a safepoint inside
    //! a splice at its enclosing `invoke`'s bci, and since hotfix 8335e345ff
    //! calls that `invoke` an exact re-run point when
    //! `deopt::owned_args_caller_replay_is_exact(outer, .., bci, &[])` holds.
    //! A spliced owned call's hand-off re-runs the OUTER frame from bci 0 while
    //! it stands there, so every chain the two-level rule admits must satisfy
    //! that predicate at level 0, or the window would clear the outer caller's
    //! argument copies that the re-run reads. These pin the coupling.
    use super::*;

    const MID: &str = "P.mid:(Ljava/lang/Object;)I";
    /// `0 aload_0  1 invokestatic #1  4 ireturn`.
    const PURE: [u8; 5] = [0x2a, 0xb8, 0x00, 0x01, 0xac];
    /// ARGWIN21-4's fresh-array prefix: `0 iconst_1  1 newarray int  3 dup
    /// 4 iconst_0  5 iconst_1  6 iastore  7 invokestatic #1  10 ireturn`.
    const FRESH: [u8; 11] = [0x04, 0xbc, 0x0a, 0x59, 0x03, 0x04, 0x4f, 0xb8, 0x00, 0x01, 0xac];
    /// A constant into a static first: `0 iconst_1  1 putstatic #2
    /// 4 aload_0  5 invokestatic #1  8 ireturn`.
    const CONST_STATIC: [u8; 9] = [0x04, 0xb3, 0x00, 0x02, 0x2a, 0xb8, 0x00, 0x01, 0xac];

    fn outer_call(outer: &[u8]) -> usize {
        outer.iter().position(|&op| op == 0xb8).unwrap()
    }

    /// `mid` (PURE) spliced at base 100 under `outer`'s call, its own call at 101.
    fn admitted(outer: &[u8]) -> Result<u8, ChainRefusal> {
        let mid = SplicedBodyCode {
            method_key: MID.to_string(),
            code: PURE.to_vec(),
            num_params: 1,
            num_locals: 1,
            handlers: Vec::new(),
        };
        let enclosing = [(100usize, PURE.len(), outer_call(outer), MID)];
        spliced_call_replay(101, &enclosing, outer, &[], &[mid], false, false)
    }

    /// dev's window closure's owned-arguments term at the resolved `invoke`.
    fn window_counts_exact(outer: &[u8]) -> bool {
        let bci = u32::try_from(outer_call(outer)).unwrap();
        crate::deopt::owned_args_caller_replay_is_exact(outer, outer.len(), bci, &[])
    }

    #[test]
    fn j27_w8arg_every_admitted_chain_is_an_exact_rerun_point_of_the_window() {
        for outer in [&PURE[..], &FRESH[..]] {
            assert_eq!(admitted(outer), Ok(0xb8), "{outer:02x?}");
            assert!(window_counts_exact(outer), "{outer:02x?}");
        }
    }

    /// The OOM21 constant-static rule widens only the compiling method's own
    /// site, so level 0 of a spliced chain keeps refusing them even with the
    /// rule's switch on. (Since j27 wave 9 the window counts those points
    /// through `owned_args_caller_replay_may_be_exact`, a superset, so it
    /// would keep the copies either way; level 0 was not widened.)
    #[test]
    fn j27_w8arg_a_constant_static_outer_prefix_stays_refused_under_its_switch() {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_OWNED_ARGS_CONST_STATIC_STORES", Some("1")),
                ("CRATONVM_JIT_SERVICE_COPY_WINDOW", Some("0")),
            ],
            || {
                assert_eq!(admitted(&CONST_STATIC), Err(ChainRefusal::Commit(0)));
                assert!(!window_counts_exact(&CONST_STATIC));
            },
        );
    }
}

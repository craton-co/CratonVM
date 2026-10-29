// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The optimizing (IR) tier's splice-planning ORDER (JIT round 18 wave 10,
//! lane buffers).
//!
//! The IR planner in `ir_tier_attempt` visits the compiling method's invoke
//! sites in bytecode order and stops at `IR_INLINE_MAX_SITES` (24) planned
//! sites or an exhausted `IR_INLINE_MAX_TOTAL_BYTES` budget, first come first
//! served. A method whose cold exits come early in the bytecode spends that
//! cap on them, and the hot sites further down stay calls.
//!
//! `sun.nio.cs.UTF_8$Decoder.decodeArrayLoop` (JDK 25) is the measured shape:
//! 33 invoke sites, 13 of them `invokestatic xflow / malformed /
//! malformedForLength` immediately followed by `areturn`, interleaved with the
//! per-byte helpers (`isNotContinuation`, `isMalformed3`, `Character.isSurrogate`,
//! `isMalformed4`, `Character.isSupplementaryCodePoint`, `highSurrogate`,
//! `lowSurrogate`). Under `--compatible` with the buffer read accessors
//! registered, the 4 `position()` / `limit()` sites in the prologue are
//! refused (`native-shadow`) and the plan reaches `lowSurrogate` as site 23;
//! with the accessors left to `Buffer`'s bytecode (`--jdk-only`, or
//! the since-deleted `CRATONVM_COMPAT_REAL_BUFFER_ACCESSORS=1`) those 4 sites are spliced first
//! and the 4-byte path's helpers fall off the cap -- the `utfdecode` row of
//! `R17NioRows` goes 35 474 -> 49 388 ns (`--jdk-only`: 44 634).
//!
//! A call whose next instruction is a `return` runs at most once per
//! invocation of the method, so splicing it saves one call per invocation; a
//! site inside the loop body saves one per iteration. Visiting the tail-exit
//! sites LAST lets them take only what the other sites leave. The plan's
//! correctness does not depend on the order: every site is planned
//! all-or-nothing, keyed by its own pc, and the spliced ranges are sorted
//! after the loop.

/// HISTORY (superseded by the round-19 measurement below): MEASURED on the r18
/// w10 binary (`--jdk-only`, `R18BuffersTailExits`) as MUST STAY OFF. `decode-supp` 299 638 -> 260 335 ns, but `encode-mixed` 246 477 ->
/// 1 571 025 and `tail-exits` 45 828 -> 495 233 (the cause is not identified
/// yet; see `r18-ir-splice-plan-spends-its-site-cap-on-tail-exits-CLOSED-20261006.md`).
///
/// `CRATONVM_JIT_IR_SPLICE_TAIL_EXITS_LAST` (perf, default ON since JIT round 19
/// proposal round, 2026-10-08: R18BuffersTailExits decode-supp -28 % in two
/// interleaved runs, no row worse; kill word `0`): the IR
/// planner visits tail-exit invoke sites ([`is_tail_exit_invoke`]) after every
/// other site. Read once per IR compile that reaches the planner.
pub(crate) fn ir_splice_tail_exits_last_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SPLICE_TAIL_EXITS_LAST")
}

/// Is the invoke at `pc` immediately followed by one of the caller's own
/// `return` instructions (`ireturn` .. `return`, `0xac..=0xb1`)?
///
/// Decided on the caller's own bytecode only (`pc < code_len`); an opcode at
/// `pc` that is not a standard invoke (a rewritten or quickened form) answers
/// `false`, which keeps the site in bytecode order.
pub(crate) fn is_tail_exit_invoke(code: &[u8], code_len: usize, pc: usize) -> bool {
    let limit = code_len.min(code.len());
    if pc >= limit {
        return false;
    }
    let len = match code[pc] {
        0xb6..=0xb8 => 3,
        0xb9 | 0xba => 5,
        _ => return false,
    };
    let next = pc + len;
    next < limit && matches!(code[next], 0xac..=0xb1)
}

/// The planner's visiting order: the non-tail-exit sites in bytecode order,
/// then the tail-exit sites in bytecode order (a stable partition). Returns
/// the order and how many sites were moved to the back.
pub(crate) fn tail_exits_last(
    code: &[u8],
    code_len: usize,
    ops: &[(usize, u16, u8)],
) -> (Vec<(usize, u16, u8)>, usize) {
    let mut front: Vec<(usize, u16, u8)> = Vec::with_capacity(ops.len());
    let mut back: Vec<(usize, u16, u8)> = Vec::new();
    for &op in ops {
        if is_tail_exit_invoke(code, code_len, op.0) {
            back.push(op);
        } else {
            front.push(op);
        }
    }
    let moved = back.len();
    front.extend(back);
    (front, moved)
}

// ---------------------------------------------------------------------------
// Profile-ordered planning (round nio20 wave 1, lane L5; NIO19-4)
// ---------------------------------------------------------------------------
//
// The tail-exit partition above is structural only. The planner below ranks
// every site by what the method's own profile says about it:
//
// * a site the branch profile shows UNREACHED (every path to it goes through
//   a conditional branch direction observed zero times out of at least
//   `MIN_BRANCH_EVIDENCE`, or only through an exception handler) is visited
//   last -- it is the cold `malformed` / `overflow` exit of the UTF-8 loops;
// * a reached tail exit (`invoke; *return`) runs at most once per invocation;
// * a reached site inside a loop (textual back-edge ranges, nested depth)
//   runs once per iteration and is visited first, deepest first;
// * ties break on the site's execution count (`MethodProfile::call_site_count`,
//   a hint: the interpreter records it on its slow paths), then on pc.
//
// The order is a VISIT order only. Every site is still planned all-or-nothing
// and keyed by its own pc, so nothing downstream depends on it (see the
// module header); a demoted site still splices when the cap and budget leave
// room. The one order-dependent hint the planner carries, the factory-result
// local (`ir_factory_receiver_class` in `lib.rs`: `it = c.iterator(); while
// (it.hasNext())`), is kept by hoisting a site's factory PRODUCER right in
// front of it, so a for-each loop's in-loop `hasNext` / `next` still see the
// `iterator()` site that precedes them in pc order.
//
// A method whose loop is hot (`HOT_LOOP_MIN_BACKEDGES` back edges recorded,
// or no profile at all) gets `IR_INLINE_MAX_SITES_HOT_LOOP` sites instead of
// the flat cap, the extra slots going to in-loop sites only, under the same
// byte budget.

use crate::profile::MethodProfile;

/// The site cap a method with a hot loop gets ([`ProfilePlanOrder::hot_loop`]),
/// in place of the flat `IR_INLINE_MAX_SITES` (24). Only in-loop sites
/// ([`SiteClass::InLoop`]) may take the slots past the flat cap; the byte
/// budget (`IR_INLINE_MAX_TOTAL_BYTES`) is unchanged and still bounds the walk.
/// 40 covers `UTF_8$Decoder.decodeArrayLoop` (33 sites) and
/// `UTF_8$Encoder.encodeBufferLoop` (27).
pub(crate) const IR_INLINE_MAX_SITES_HOT_LOOP: usize = 40;

/// Recorded back edges (any one loop of the method) that make a loop hot.
const HOT_LOOP_MIN_BACKEDGES: u64 = 256;

/// Observations a conditional branch needs before a zero direction counts as
/// "never taken" for [`profile_plan_order`]'s reachability walk.
const MIN_BRANCH_EVIDENCE: u32 = 16;

/// `CRATONVM_JIT_IR_SPLICE_PROFILE_ORDER` (perf, **default ON**; `0` restores
/// the bytecode order, or the tail-exit order when
/// `CRATONVM_JIT_IR_SPLICE_TAIL_EXITS_LAST` is on): the IR planner visits
/// invoke sites in [`profile_plan_order`]'s order. Read once per IR compile
/// that reaches the planner.
pub(crate) fn ir_splice_profile_order_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SPLICE_PROFILE_ORDER")
}

/// `CRATONVM_JIT_IR_SPLICE_HANDLER_SITES_LAST` (perf, **default ON**; `0`
/// ranks them by their counts as before): within each class of
/// [`profile_plan_order`], a site no normal edge reaches from pc 0 -- code an
/// exception handler alone enters -- is visited after the others. A loop's
/// `catch` arm looks hot whenever the profile was taken during a run of
/// throws (`HttpHeaderValidationUtilTest`'s value loop starts on 2^24 values
/// with a zero byte, so its OSR compile saw the handler's two
/// `validateValidHeaderValue` calls 488 times each), and its sites then spent
/// the byte budget before the call every iteration makes. Read once per
/// ordering.
pub(crate) fn ir_splice_handler_sites_last_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SPLICE_HANDLER_SITES_LAST")
}

/// `CRATONVM_JIT_IR_SPLICE_ORDER_INSN_START` (JIT r22 wave 1, lane inline;
/// correctness of the planner's own analysis, **default ON**, `0` restores the
/// byte-only read): the factory-producer hoist of [`profile_plan_order`] takes
/// a site's receiver local only from an `aload` that BEGINS an instruction
/// (`receiver_local`). Visit order only; no Java-visible answer depends on it.
/// Read once per ordering.
pub(crate) fn ir_splice_order_insn_start_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SPLICE_ORDER_INSN_START")
}

/// The instructions of `code[..len]` reachable from pc 0 along normal edges
/// (fall-through and explicit branch / switch targets; no handler edge), or
/// `None` when the body does not decode or a successor set is not static.
fn normally_reachable(code: &[u8], len: usize) -> Option<Vec<bool>> {
    use crate::bytecode_analysis as ba;
    let insns = ba::decode_method(code, len)?;
    let index: rustc_hash::FxHashMap<usize, usize> =
        insns.iter().enumerate().map(|(i, insn)| (insn.pc, i)).collect();
    let mut seen = vec![false; len];
    let mut work = vec![0usize];
    let mut targets = Vec::new();
    while let Some(pc) = work.pop() {
        if pc >= len || seen[pc] {
            continue;
        }
        seen[pc] = true;
        let &i = index.get(&pc)?;
        targets.clear();
        if !ba::explicit_targets(code, len, pc, &mut targets) {
            return None;
        }
        work.extend(targets.iter().copied());
        if ba::falls_through(insns[i].op) {
            if let Some(next) = insns.get(i + 1) {
                work.push(next.pc);
            }
        }
    }
    Some(seen)
}

/// `CRATONVM_JIT_IR_SPLICE_HOT_LOOP_CAP` (perf, **default ON**; `0` keeps the
/// flat 24-site cap): a method with a hot loop may plan up to
/// [`IR_INLINE_MAX_SITES_HOT_LOOP`] sites, the extra ones in-loop only.
/// Independent of the order switch, so the two can be A/B'd separately.
pub(crate) fn ir_splice_hot_loop_cap_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SPLICE_HOT_LOOP_CAP")
}

/// Where a site ranks. Declaration order IS visit order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SiteClass {
    /// Reached, not a tail exit, inside at least one loop.
    InLoop,
    /// Reached, not a tail exit, outside every loop (or a pc past the
    /// caller's own code, which the walk cannot judge).
    Straight,
    /// Reached, immediately followed by a `return` ([`is_tail_exit_invoke`]).
    TailExit,
    /// Not reached under the branch profile.
    Cold,
}

impl SiteClass {
    fn tag(self) -> char {
        match self {
            SiteClass::InLoop => 'L',
            SiteClass::Straight => 'S',
            SiteClass::TailExit => 'T',
            SiteClass::Cold => 'C',
        }
    }
}

/// One invoke site with its rank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RankedSite {
    /// `(pc, cp_idx, opcode)`, as in `ir_invoke_ops`.
    pub(crate) op: (usize, u16, u8),
    pub(crate) class: SiteClass,
    /// Loops enclosing the site (0 outside every loop).
    pub(crate) depth: u32,
    /// Observed executions (`call_site_count`), 0 when none were recorded.
    pub(crate) count: u32,
}

/// [`profile_plan_order`]'s answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProfilePlanOrder {
    /// Every site, in visit order.
    pub(crate) sites: Vec<RankedSite>,
    /// The method has a hot loop with at least one in-loop site, so it may
    /// take [`IR_INLINE_MAX_SITES_HOT_LOOP`] sites.
    pub(crate) hot_loop: bool,
    /// The branch profile judged reachability (`false`: no usable profile,
    /// every site counted reached).
    pub(crate) profiled: bool,
}

impl ProfilePlanOrder {
    /// The visit order as `ir_invoke_ops` rows.
    pub(crate) fn ops(&self) -> Vec<(usize, u16, u8)> {
        self.sites.iter().map(|s| s.op).collect()
    }

    /// May the site at `pc` take a slot past the flat cap?
    pub(crate) fn takes_hot_loop_slot(&self, pc: usize) -> bool {
        self.hot_loop
            && self
                .sites
                .iter()
                .any(|s| s.op.0 == pc && s.class == SiteClass::InLoop)
    }

    /// The rank of the site at `pc`, `None` for a pc the order does not list
    /// (nio21 wave 6, lane L4: the guarded splice's hot tier,
    /// `lib.rs` `ir_guarded_site_is_hot`).
    pub(crate) fn class_of(&self, pc: usize) -> Option<SiteClass> {
        self.sites.iter().find(|s| s.op.0 == pc).map(|s| s.class)
    }

    /// How many sites rank in `class`.
    pub(crate) fn count_of(&self, class: SiteClass) -> usize {
        self.sites.iter().filter(|s| s.class == class).count()
    }

    /// `pc:class[:count]` per site, in visit order, for the DBG line.
    pub(crate) fn describe(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.sites.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(&s.op.0.to_string());
            out.push(':');
            out.push(s.class.tag());
            if s.count > 0 {
                out.push(':');
                out.push_str(&s.count.to_string());
            }
        }
        out
    }
}

/// Is `profile` about this body? A profile stamped with another length is
/// another version of the method (or a frame length with the 2 sentinel
/// bytes, which is the same body).
fn profile_matches(profile: &MethodProfile, code_len: usize) -> bool {
    let rec = profile.recorded_code_len();
    if rec == crate::profile::CODE_LEN_CONFLICT {
        return false;
    }
    let rec = rec as usize; // Cast: u32 -> usize widens
    rec == 0 || rec == code_len || rec == code_len + 2 || rec + 2 == code_len
}

/// One `(header, last back-edge source)` range per textual loop header.
fn loop_ranges(code: &[u8], limit: usize) -> Vec<(usize, usize)> {
    let mut last: rustc_hash::FxHashMap<usize, usize> = rustc_hash::FxHashMap::default();
    for (header, src) in crate::bytecode_analysis::back_edges(code, limit) {
        let e = last.entry(header).or_insert(src);
        if src > *e {
            *e = src;
        }
    }
    let mut out: Vec<(usize, usize)> = last.into_iter().collect();
    out.sort_unstable();
    out
}

fn loop_depth(ranges: &[(usize, usize)], pc: usize) -> u32 {
    ranges.iter().filter(|&&(h, s)| h <= pc && pc <= s).count() as u32 // Cast: count <= ranges.len() <= code_len < 2^16
}

/// Pcs reachable from pc 0 along the directions the branch profile observed.
/// A conditional branch with fewer than [`MIN_BRANCH_EVIDENCE`] observations
/// keeps both directions; exception handlers are not roots. `None` when the
/// profile has no branch evidence at all.
fn profile_reach(code: &[u8], limit: usize, profile: &MethodProfile) -> Option<Vec<bool>> {
    if limit == 0 || profile.branches.is_empty() {
        return None;
    }
    let mut seen = vec![false; limit];
    let mut stack = vec![0usize];
    while let Some(pc) = stack.pop() {
        if pc >= limit || seen[pc] {
            continue;
        }
        seen[pc] = true;
        let op = code[pc];
        let judged = if matches!(op, 0x99..=0xa6 | 0xc6 | 0xc7) {
            profile
                .branches
                .get(&pc)
                .filter(|c| c.total() >= MIN_BRANCH_EVIDENCE)
                .map(|c| (c.taken, c.not_taken))
        } else {
            None
        };
        match judged {
            Some((taken, not_taken)) => {
                if taken > 0 {
                    if let Some(t) =
                        crate::bytecode_analysis::offset_branch_target(&code[..limit], pc)
                            .filter(|&t| t < limit)
                    {
                        stack.push(t);
                    }
                }
                let next = pc + crate::bytecode_analysis::step(code, pc);
                if not_taken > 0 && next < limit {
                    stack.push(next);
                }
            }
            None => stack.extend(crate::bytecode_analysis::lenient_successors(
                code, limit, pc,
            )),
        }
    }
    Some(seen)
}

/// The local an `invokevirtual` / `invokeinterface` at `pc` takes its receiver
/// from (`aload k` / `aload_<k>` right before it), the shape
/// `ir_factory_receiver_class` reads.
///
/// JIT r22 wave 1 (lane inline; the r21 splice book's review note): with
/// `starts` (`CRATONVM_JIT_IR_SPLICE_ORDER_INSN_START`, default ON) the byte
/// read as the `aload` must begin an instruction, so `bipush 42;
/// invokevirtual` (operand byte `0x2a`) no longer reads as `aload_0`, nor
/// `sipush 0x19xx` as `aload xx`. `None` keeps the old byte-only read.
fn receiver_local(
    code: &[u8],
    pc: usize,
    opcode: u8,
    starts: Option<&[bool]>,
) -> Option<usize> {
    if !matches!(opcode, 0xb6 | 0xb9) {
        return None;
    }
    if let Some(starts) = starts {
        let starts_at = |p: usize| starts.get(p).copied().unwrap_or(false);
        let aload_at = pc
            .checked_sub(2)
            .filter(|&p| starts_at(p) && code.get(p) == Some(&0x19));
        if let Some(p) = aload_at {
            return code.get(p + 1).map(|&k| usize::from(k));
        }
        if let Some(p) = pc.checked_sub(1).filter(|&p| starts_at(p)) {
            if let Some(&op @ 0x2a..=0x2d) = code.get(p) {
                return Some(usize::from(op - 0x2a));
            }
        }
        return None;
    }
    match (
        pc.checked_sub(2).and_then(|p| code.get(p)),
        pc.checked_sub(1).and_then(|p| code.get(p)),
    ) {
        (Some(0x19), Some(&k)) => Some(usize::from(k)),
        (_, Some(&op @ 0x2a..=0x2d)) => Some(usize::from(op - 0x2a)),
        _ => None,
    }
}

/// Does the invoke at `pc` store its result straight into local `k`
/// (`astore k` / `astore_<k>` as its next instruction)?
fn stores_result_to(code: &[u8], limit: usize, pc: usize, opcode: u8, k: usize) -> bool {
    let next = pc + if matches!(opcode, 0xb9 | 0xba) { 5 } else { 3 };
    if next >= limit {
        return false;
    }
    match code[next] {
        0x3a => code.get(next + 1).is_some_and(|&b| usize::from(b) == k) && next + 1 < limit,
        op @ 0x4b..=0x4e => usize::from(op - 0x4b) == k,
        _ => false,
    }
}

/// `CRATONVM_JIT_IR_PSPLICE_LOOP_COUNT_RANK` (JIT round 20 wave 11, lane
/// pSplice; L3W2-4 of `jit-r20-nio-proposals-w2-L3.md`, with NIO20-L5-3's
/// "rank by frequency" in its simplest form) -- **default ON**, `=0` ranks
/// in-loop sites by loop depth first, as before. Read once per IR compile.
pub(crate) fn ir_psplice_loop_count_rank_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_PSPLICE_LOOP_COUNT_RANK")
}

/// Do [`profile_plan_order`]'s in-loop sites rank by execution count before
/// loop depth? Only under [`ir_psplice_loop_count_rank_enabled`], and only
/// when there are at least two in-loop sites and EVERY one has a non-zero
/// count: with the fast-path invoke counters
/// (`CRATONVM_JIT_PROFILE_FAST_CALL_COUNTS`, default ON) the count is a
/// per-execution frequency, so a depth-1 site that runs every iteration
/// outranks a depth-2 site of a rarely entered inner loop. A site with no
/// count (counters off, or a site the interpreter never reached before the
/// compile) makes counts incomparable with depths, so the whole method keeps
/// the depth-first rank -- one answer per method keeps the sort a total order.
fn in_loop_count_rank_applies(sites: &[RankedSite]) -> bool {
    let mut in_loop = sites.iter().filter(|s| s.class == SiteClass::InLoop);
    sites
        .iter()
        .filter(|s| s.class == SiteClass::InLoop)
        .count()
        >= 2
        && in_loop.all(|s| s.count > 0)
        && ir_psplice_loop_count_rank_enabled()
}

/// The profile-ordered visit order of `ops` (the planner's `ir_invoke_ops`,
/// pc order). See the section comment above for the ranking; `profile` is the
/// compiling method's snapshot (`CompileRequest::profile`), ignored when it
/// was recorded against a body of another length.
pub(crate) fn profile_plan_order(
    code: &[u8],
    code_len: usize,
    ops: &[(usize, u16, u8)],
    profile: Option<&MethodProfile>,
) -> ProfilePlanOrder {
    let limit = code_len.min(code.len());
    let profile = profile.filter(|p| profile_matches(p, code_len));
    let ranges = loop_ranges(code, limit);
    let reach = profile.and_then(|p| profile_reach(code, limit, p));
    let mut sites: Vec<RankedSite> = ops
        .iter()
        .map(|&op| {
            let pc = op.0;
            let count = profile.map_or(0, |p| p.call_site_count(pc).count_or_zero());
            if pc >= limit {
                return RankedSite {
                    op,
                    class: SiteClass::Straight,
                    depth: 0,
                    count,
                };
            }
            let depth = loop_depth(&ranges, pc);
            let reached = count > 0 || reach.as_ref().is_none_or(|r| r[pc]);
            let class = if !reached {
                SiteClass::Cold
            } else if is_tail_exit_invoke(code, limit, pc) {
                SiteClass::TailExit
            } else if depth > 0 {
                SiteClass::InLoop
            } else {
                SiteClass::Straight
            };
            RankedSite {
                op,
                class,
                depth,
                count,
            }
        })
        .collect();
    // JIT round 20 wave 11 (lane pSplice, L3W2-4): the in-loop sites rank by
    // count before depth when EVERY one of them has a count (decided once per
    // method, so the comparator stays a total order).
    let count_first = in_loop_count_rank_applies(&sites);
    // `CRATONVM_JIT_IR_SPLICE_HANDLER_SITES_LAST`: handler-only sites last
    // within their class.
    let normal = ir_splice_handler_sites_last_enabled()
        .then(|| normally_reachable(code, limit))
        .flatten();
    let handler_only = |pc: usize| -> bool {
        normal
            .as_ref()
            .is_some_and(|n| pc < limit && !n.get(pc).copied().unwrap_or(true))
    };
    sites.sort_by(|a, b| {
        let by_rank = if count_first && a.class == SiteClass::InLoop && b.class == SiteClass::InLoop
        {
            b.count.cmp(&a.count).then(b.depth.cmp(&a.depth))
        } else {
            b.depth.cmp(&a.depth).then(b.count.cmp(&a.count))
        };
        a.class
            .cmp(&b.class)
            .then(handler_only(a.op.0).cmp(&handler_only(b.op.0)))
            .then(by_rank)
            .then(a.op.0.cmp(&b.op.0))
    });

    // Hoist each site's factory producer in front of it (see above).
    let by_pc: rustc_hash::FxHashMap<usize, RankedSite> =
        sites.iter().map(|s| (s.op.0, *s)).collect();
    let mut pcs_in_order: Vec<usize> = ops.iter().map(|op| op.0).collect();
    pcs_in_order.sort_unstable();
    // JIT r22 (lane inline, `CRATONVM_JIT_IR_SPLICE_ORDER_INSN_START`): the
    // instruction starts `receiver_local` checks its `aload` against. A body
    // that does not decode gets an all-`false` table: no producer is hoisted.
    let insn_starts: Option<Vec<bool>> = ir_splice_order_insn_start_enabled().then(|| {
        let mut starts = vec![false; limit];
        if let Some(insns) = crate::bytecode_analysis::decode_method(code, limit) {
            for insn in insns {
                if let Some(s) = starts.get_mut(insn.pc) {
                    *s = true;
                }
            }
        }
        starts
    });
    let producer_of = |site: &RankedSite| -> Option<usize> {
        let (pc, _, opcode) = site.op;
        if pc >= limit {
            return None;
        }
        let k = receiver_local(code, pc, opcode, insn_starts.as_deref())?;
        let before = pcs_in_order.partition_point(|&p| p < pc);
        pcs_in_order[..before].iter().rev().copied().find(|&p| {
            by_pc
                .get(&p)
                .is_some_and(|s| p < limit && stores_result_to(code, limit, p, s.op.2, k))
        })
    };
    let mut placed: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    let mut ordered: Vec<RankedSite> = Vec::with_capacity(sites.len());
    for site in &sites {
        if placed.contains(&site.op.0) {
            continue;
        }
        let mut chain: Vec<usize> = Vec::new();
        let mut cur = *site;
        while let Some(p) = producer_of(&cur) {
            if placed.contains(&p) || chain.contains(&p) {
                break;
            }
            chain.push(p);
            cur = by_pc[&p];
        }
        for p in chain.into_iter().rev() {
            placed.insert(p);
            ordered.push(by_pc[&p]);
        }
        placed.insert(site.op.0);
        ordered.push(*site);
    }

    let has_loop_site = ordered.iter().any(|s| s.class == SiteClass::InLoop);
    let hot_loop = has_loop_site
        && match profile {
            Some(p) if !p.loops.is_empty() || !p.branches.is_empty() => {
                p.loops
                    .values()
                    .any(|l| l.backedge_count >= HOT_LOOP_MIN_BACKEDGES)
                    || p.branches.iter().any(|(&pc, c)| {
                        u64::from(c.total()) >= HOT_LOOP_MIN_BACKEDGES
                            && loop_depth(&ranges, pc) > 0
                    })
            }
            // No profile evidence at all (profiling off): the method reached
            // the optimizing tier, and its loop is the likeliest reason.
            _ => true,
        };
    ProfilePlanOrder {
        sites: ordered,
        hot_loop,
        profiled: reach.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // pc 0: invokestatic #1; pc 3: areturn; pc 4: invokevirtual #2;
    // pc 7: istore_1; pc 8: invokeinterface #3 n 0; pc 13: ireturn;
    // pc 14: invokestatic #4; pc 17: pop; pc 18: return
    const CODE: [u8; 19] = [
        0xb8, 0, 1, 0xb0, 0xb6, 0, 2, 0x3c, 0xb9, 0, 3, 1, 0, 0xac, 0xb8, 0, 4, 0x57, 0xb1,
    ];

    #[test]
    fn tail_exit_is_an_invoke_followed_by_a_return() {
        assert!(is_tail_exit_invoke(&CODE, CODE.len(), 0));
        assert!(!is_tail_exit_invoke(&CODE, CODE.len(), 4));
        assert!(
            is_tail_exit_invoke(&CODE, CODE.len(), 8),
            "5-byte invokeinterface"
        );
        assert!(
            !is_tail_exit_invoke(&CODE, CODE.len(), 14),
            "a pop sits between"
        );
        // Not an invoke, and out of range.
        assert!(!is_tail_exit_invoke(&CODE, CODE.len(), 3));
        assert!(!is_tail_exit_invoke(&CODE, CODE.len(), 40));
        // A spliced pc past the caller's own code never counts.
        assert!(!is_tail_exit_invoke(&CODE, 3, 0));
    }

    #[test]
    fn order_is_a_stable_partition() {
        let ops = [
            (0usize, 1u16, 0xb8u8),
            (4, 2, 0xb6),
            (8, 3, 0xb9),
            (14, 4, 0xb8),
        ];
        let (order, moved) = tail_exits_last(&CODE, CODE.len(), &ops);
        assert_eq!(moved, 2);
        assert_eq!(
            order,
            vec![(4, 2, 0xb6), (14, 4, 0xb8), (0, 1, 0xb8), (8, 3, 0xb9)]
        );
    }

    // A decodeArrayLoop-shaped method:
    //  0: invokestatic #1          pre-loop site A
    //  3: iload_1                  loop header
    //  4: ifge -> 25               loop exit
    //  7: iload_2
    //  8: ifne -> 15               profile: always taken
    // 11: invokestatic #2          B: in-loop malformed exit (cold, tail exit)
    // 14: areturn
    // 15: invokestatic #3          C: the loop's hot helper
    // 18: pop
    // 19: iinc 1 -1
    // 22: goto -> 3
    // 25: invokestatic #4          D: post-loop site
    // 28: invokestatic #5          E: tail exit
    // 31: areturn
    const LOOP: [u8; 32] = [
        0xb8, 0, 1, 0x1b, 0x9c, 0, 21, 0x1c, 0x9a, 0, 7, 0xb8, 0, 2, 0xb0, 0xb8, 0, 3, 0x57, 0x84,
        1, 0xff, 0xa7, 0xff, 0xed, 0xb8, 0, 4, 0xb8, 0, 5, 0xb0,
    ];
    const LOOP_OPS: [(usize, u16, u8); 5] = [
        (0, 1, 0xb8),
        (11, 2, 0xb8),
        (15, 3, 0xb8),
        (25, 4, 0xb8),
        (28, 5, 0xb8),
    ];

    fn pcs(order: &ProfilePlanOrder) -> Vec<usize> {
        order.sites.iter().map(|s| s.op.0).collect()
    }

    fn loop_profile(backedges: u64) -> MethodProfile {
        let mut p = MethodProfile::default();
        let b = p.branches.entry(4).or_default();
        b.taken = 1;
        b.not_taken = 500;
        let b = p.branches.entry(8).or_default();
        b.taken = 500;
        b.not_taken = 0;
        p.loops.entry(22).or_default().backedge_count = backedges;
        p
    }

    #[test]
    fn profile_order_ranks_loop_then_straight_then_tail_then_cold() {
        let p = loop_profile(500);
        let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p));
        assert!(order.profiled);
        assert_eq!(pcs(&order), vec![15, 0, 25, 28, 11]);
        let classes: Vec<SiteClass> = order.sites.iter().map(|s| s.class).collect();
        assert_eq!(
            classes,
            vec![
                SiteClass::InLoop,
                SiteClass::Straight,
                SiteClass::Straight,
                SiteClass::TailExit,
                SiteClass::Cold
            ]
        );
        assert_eq!(order.sites[0].depth, 1);
        assert!(order.hot_loop);
        assert!(order.takes_hot_loop_slot(15));
        assert!(!order.takes_hot_loop_slot(0));
        assert!(
            !order.takes_hot_loop_slot(11),
            "a cold in-loop site gets no extra slot"
        );
        assert_eq!(order.count_of(SiteClass::Cold), 1);
        assert_eq!(order.class_of(15), Some(SiteClass::InLoop));
        assert_eq!(order.class_of(11), Some(SiteClass::Cold));
        assert_eq!(order.class_of(28), Some(SiteClass::TailExit));
        assert_eq!(order.class_of(99), None);
        assert_eq!(order.describe(), "15:L 0:S 25:S 28:T 11:C");
        assert_eq!(order.ops()[0], (15, 3, 0xb8));
    }

    #[test]
    fn without_a_profile_only_structure_ranks() {
        let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, None);
        assert!(!order.profiled);
        // B is reached now; as a tail exit it ranks with E, deeper first.
        assert_eq!(pcs(&order), vec![15, 0, 25, 11, 28]);
        // No evidence either way: the loop is presumed hot.
        assert!(order.hot_loop);
    }

    #[test]
    fn thin_branch_evidence_keeps_both_directions() {
        let mut p = loop_profile(500);
        let b = p.branches.get_mut(&8).unwrap();
        b.taken = MIN_BRANCH_EVIDENCE - 1;
        let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p));
        assert_eq!(order.sites.last().unwrap().op.0, 28);
        assert_eq!(order.count_of(SiteClass::Cold), 0);
    }

    #[test]
    fn an_observed_call_count_overrides_the_branch_walk_and_breaks_ties() {
        let mut p = loop_profile(500);
        p.call_sites.insert(11, 3);
        p.call_sites.insert(25, 9);
        let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p));
        // B is reached (it ran), D outranks A on its count.
        assert_eq!(pcs(&order), vec![15, 25, 0, 11, 28]);
        assert_eq!(order.sites[1].count, 9);
    }

    // Two nested loops (JIT round 20 wave 11, lane pSplice, L3W2-4):
    //  0: invokestatic #1          A: outer loop (depth 1)
    //  3: iload_1                  inner header
    //  4: invokestatic #2          B: inner loop (depth 2)
    //  7: pop
    //  8: iinc 1 -1
    // 11: iload_1
    // 12: ifgt -> 3
    // 15: goto -> 0
    // 18: return
    const NESTED: [u8; 19] = [
        0xb8, 0, 1, 0x1b, 0xb8, 0, 2, 0x57, 0x84, 1, 0xff, 0x1b, 0x9d, 0xff, 0xf7, 0xa7, 0xff,
        0xf1, 0xb1,
    ];
    const NESTED_OPS: [(usize, u16, u8); 2] = [(0, 1, 0xb8), (4, 2, 0xb8)];

    #[test]
    fn jr20w11_in_loop_sites_rank_by_count_when_every_one_has_one() {
        const KEY: &str = "CRATONVM_JIT_IR_PSPLICE_LOOP_COUNT_RANK";
        let mut p = MethodProfile::default();
        p.call_sites.insert(0, 1000);
        p.call_sites.insert(4, 10);
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            // No counts: depth first, the inner loop's site leads.
            let order = profile_plan_order(&NESTED, NESTED.len(), &NESTED_OPS, None);
            assert_eq!(pcs(&order), vec![4, 0]);
            assert_eq!((order.sites[0].depth, order.sites[1].depth), (2, 1));
            // Both counted: the site that ran 100x more leads.
            let order = profile_plan_order(&NESTED, NESTED.len(), &NESTED_OPS, Some(&p));
            assert_eq!(pcs(&order), vec![0, 4]);
            // One site without a count: the whole method keeps depth first.
            let mut thin = MethodProfile::default();
            thin.call_sites.insert(0, 1000);
            let order = profile_plan_order(&NESTED, NESTED.len(), &NESTED_OPS, Some(&thin));
            assert_eq!(pcs(&order), vec![4, 0]);
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            let order = profile_plan_order(&NESTED, NESTED.len(), &NESTED_OPS, Some(&p));
            assert_eq!(pcs(&order), vec![4, 0]);
        });
    }

    #[test]
    fn a_cool_loop_keeps_the_flat_cap() {
        let mut p = loop_profile(10);
        // Branch totals inside the loop below the threshold too.
        p.branches.get_mut(&4).unwrap().not_taken = 20;
        p.branches.get_mut(&8).unwrap().taken = 20;
        let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p));
        assert!(!order.hot_loop);
        assert!(!order.takes_hot_loop_slot(15));
    }

    #[test]
    fn a_profile_of_another_body_is_ignored() {
        let mut p = loop_profile(500);
        p.note_code_len(LOOP.len() + 7);
        let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p));
        assert!(!order.profiled);
        assert_eq!(pcs(&order), vec![15, 0, 25, 11, 28]);
        // The frame length (two sentinel bytes) is the same body.
        let mut p = loop_profile(500);
        p.note_code_len(LOOP.len() + 2);
        assert!(profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p)).profiled);
    }

    // A for-each loop:
    //  0: invokevirtual #1        c.iterator()
    //  3: astore_2
    //  4: aload_2                 header
    //  5: invokeinterface #2      it.hasNext()
    // 10: ifeq -> 23
    // 13: aload_2
    // 14: invokeinterface #3      it.next()
    // 19: pop
    // 20: goto -> 4
    // 23: return
    const FOR_EACH: [u8; 24] = [
        0xb6, 0, 1, 0x4d, 0x2c, 0xb9, 0, 2, 1, 0, 0x99, 0, 13, 0x2c, 0xb9, 0, 3, 1, 0, 0x57, 0xa7,
        0xff, 0xf0, 0xb1,
    ];

    #[test]
    fn a_factory_producer_is_hoisted_in_front_of_its_loop_consumer() {
        let ops = [(0usize, 1u16, 0xb6u8), (5, 2, 0xb9), (14, 3, 0xb9)];
        let order = profile_plan_order(&FOR_EACH, FOR_EACH.len(), &ops, None);
        assert_eq!(pcs(&order), vec![0, 5, 14]);
        assert_eq!(order.sites[0].class, SiteClass::Straight);
        assert_eq!(order.sites[1].class, SiteClass::InLoop);
        // A different receiver local: no producer, plain rank order.
        let mut code = FOR_EACH;
        code[3] = 0x4e; // astore_3
        let order = profile_plan_order(&code, code.len(), &ops, None);
        assert_eq!(pcs(&order), vec![5, 14, 0]);
    }

    #[test]
    fn profile_switches_default_on() {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_SPLICE_PROFILE_ORDER", None),
                ("CRATONVM_JIT_IR_SPLICE_HOT_LOOP_CAP", None),
            ],
            || {
                assert!(ir_splice_profile_order_enabled());
                assert!(ir_splice_hot_loop_cap_enabled());
            },
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_SPLICE_PROFILE_ORDER", Some("0"))],
            || assert!(!ir_splice_profile_order_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_SPLICE_HOT_LOOP_CAP", Some("0"))],
            || assert!(!ir_splice_hot_loop_cap_enabled()),
        );
    }

    //  0: invokevirtual #1        producer, result -> local 2
    //  3: astore_2
    //  4: aload_1                 loop header; the REAL receiver is local 1
    //  5: bipush 44               operand byte 0x2c reads as `aload_2`
    //  7: invokeinterface #2 n 2
    // 12: ifeq -> 18
    // 15: goto -> 4
    // 18: return
    const BIPUSH_ARG: [u8; 19] = [
        0xb6, 0, 1, 0x4d, 0x2b, 0x10, 0x2c, 0xb9, 0, 2, 2, 0, 0x99, 0, 6, 0xa7, 0xff, 0xf5, 0xb1,
    ];

    /// JIT r22 (lane inline): an operand byte is not a receiver load.
    #[test]
    fn r22_inline_an_operand_byte_is_not_read_as_the_receiver_aload() {
        let ops = [(0usize, 1u16, 0xb6u8), (7, 2, 0xb9)];
        const KEY: &str = "CRATONVM_JIT_IR_SPLICE_ORDER_INSN_START";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(ir_splice_order_insn_start_enabled());
            let order = profile_plan_order(&BIPUSH_ARG, BIPUSH_ARG.len(), &ops, None);
            assert_eq!(pcs(&order), vec![7, 0], "no producer: plain rank order");
            // The real shape still hoists.
            let each = [(0usize, 1u16, 0xb6u8), (5, 2, 0xb9), (14, 3, 0xb9)];
            let order = profile_plan_order(&FOR_EACH, FOR_EACH.len(), &each, None);
            assert_eq!(pcs(&order), vec![0, 5, 14]);
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(!ir_splice_order_insn_start_enabled());
            let order = profile_plan_order(&BIPUSH_ARG, BIPUSH_ARG.len(), &ops, None);
            assert_eq!(pcs(&order), vec![0, 7], "the byte-only read hoists the wrong producer");
        });
    }

    #[test]
    fn switch_is_default_on_with_kill_word() {
        const KEY: &str = "CRATONVM_JIT_IR_SPLICE_TAIL_EXITS_LAST";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(ir_splice_tail_exits_last_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(!ir_splice_tail_exits_last_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            assert!(ir_splice_tail_exits_last_enabled())
        });
    }
}

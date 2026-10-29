// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 2 (lane STRIPMINE; proposal
//! `docs/known-issues/jit/j27-loopcg-counted-loop-poll-and-residency-proposal-20261028.md` item 1,
//! parent `jr21-promoted-loop-bodies-spill-and-poll-every-trip-20261026.md`):
//! strip-mine the safepoint poll of a main/post MAIN loop, as C2's loop strip
//! mining does (`UseCountedLoopSafepoints`, `LoopStripMiningIter=1000`).
//! `CRATONVM_JIT_IR_STRIP_MINE` (opt-in), `CRATONVM_JIT_IR_STRIP_MINE_ITER`
//! (strip length in inner trips, default 1000).
//!
//! j27 wave 4 (lane STRIPOSR; proposal
//! `docs/known-issues/jit/j27-stripmine-osr-and-array-bodies-proposal-20261028.md`),
//! two opt-in sub-switches:
//!
//! * `CRATONVM_JIT_IR_STRIP_MINE_ARRAYS` (item 2): a body with primitive
//!   array reads whose checks `ir_check_elim` proves BEFORE the strip mine
//!   ("Array reads" below).
//! * `CRATONVM_JIT_IR_STRIP_MINE_LONG` (item 3): a `long` induction, with a
//!   saturating strip end ("A `long` induction" below).
//!
//! j27 wave 5 (lane STRIPOSR2, item 1): `CRATONVM_JIT_IR_STRIP_MINE_OSR`
//! (opt-in), an OSR compile's loops whose header carries a bci ([`osr`]).
//!
//! # Why
//!
//! Every back edge polls (`Lowerer::emit_safepoint_poll`): five instructions
//! per main trip of `counterInt`'s four-copy body, about a fifth of its code.
//! C2 polls once per strip of 1000 iterations.
//!
//! # What
//!
//! The main loop of [`super::main_post_unroll_loop`]
//!
//! ```text
//! H: i = phi(i0, i + S) ; if (i < lim) { BODY ; goto H }   X: -> join -> post loop
//! ```
//!
//! becomes
//!
//! ```text
//! lim2 = min(lim, MAX - S + 1)
//! O: q = phi(i0, i)                               // outer: polls on its back edge
//!    slim = (int) min((long) q + N * S, (long) lim2)
//!    H: i = phi(q, i + S) ; if (i < slim) { BODY ; goto H }   // inner: no poll
//!    X: if (i < lim2) goto O                       // OB, the outer back edge
//!    OX: -> join -> post loop
//! ```
//!
//! with every header phi of `H` given an outer twin `q`. `H`, its phis, its
//! test and its body keep their identities; only the test's compare and the
//! entry inputs change, so every reader after the loop still reads `H`'s phis.
//!
//! # Why it is exact
//!
//! * The body runs only when `i < slim <= lim2 <= lim`: never on an iteration
//!   the original main loop would not run.
//! * The inner loop stops at `i >= slim`. If `slim == lim2`, then `i >= lim2`
//!   and the outer test exits too. Otherwise `slim = q + N*S < lim2` and the
//!   outer test re-enters the outer loop exactly when `i < lim2`.
//! * `q + N*S` is computed in `long` (`|q| < 2^31`, `N*S < 2^37`): it cannot
//!   wrap, so `slim > q` whenever `q < lim2`, and every strip runs at least one
//!   trip -- the outer loop always progresses. When `q >= lim2` (the first
//!   strip of a main loop entered at or past `lim2`), `slim <= lim2 <= q`: the
//!   inner test fails at once and the outer test exits.
//! * `lim2 <= MAX - S + 1` keeps the inner loop from wrapping `i`: from
//!   `i < slim <= MAX - S + 1`, `i + S <= MAX`. When `lim > lim2` (a trip that
//!   could wrap) the main loop stops early, at `i >= lim2`, and its post loop
//!   -- the original loop, entered through the join with `H`'s phis -- runs
//!   the remaining iterations under the original test, wrap included, with a
//!   poll per trip. Stopping a main loop early is always exact: that is what
//!   its post loop is for (`main_post_unroll_loop`'s "Why it is exact"), and
//!   [`exits_into_post_loop`] checks the main exit still leads there.
//!
//! # A `long` induction (wave 4, item 3)
//!
//! `q + N*S` can wrap a `long`, and nothing is wider. The strip end saturates
//! without a select: `slim = min(min(q, Long.MAX - N*S) + N*S, lim2)`, with
//! `lim2 = min(lim, Long.MAX - S + 1)`. The inner add cannot wrap (its left
//! operand is at most `Long.MAX - N*S`). `slim <= lim2` always. When `q <
//! lim2`: if `q <= Long.MAX - N*S` then `slim = min(q + N*S, lim2) > q`, at
//! most `N*S` past `q`; otherwise the sum is `Long.MAX` and `slim = lim2 > q`,
//! `lim2 - q <= Long.MAX - q < N*S`. Every bullet above then holds at 64 bits.
//!
//! # Array reads (wave 4, item 2)
//!
//! `ir_check_elim` proves `a[i + k]` in range from the main test `i < n - d`;
//! the inner test `i < slim` hides `n - d` behind the minimum. C2 strip-mines
//! after its range-check elimination; here the check elimination is asked
//! FIRST, on the graph as it stands, and the strip mine is refused unless it
//! proved every read of the body (null and bounds). The proven reads are
//! recorded in [`Graph::strip_mine_proven_accesses`], which
//! `ir_check_elim::analyze` marks elided when the lowering asks it again.
//! Exact because every run of a body node in the strip-mined graph is a run
//! of it in the graph the proof was taken on, with the same index, the same
//! array and the same length (the bullets above), and a check elision is a
//! claim about the values at each run. With both checks gone a read is one
//! `MOV` (`emit_array_null_bounds_guards_for`): nothing that transfers or
//! waits. Only PRIMITIVE element reads (no reference decode in a poll-free
//! loop) and `arraylength`, with no exception edge, are taken; a store
//! makes a memory phi, which is refused.
//!
//! # Why the frames, OSR and safepoints stay right
//!
//! * Only a PURE body is taken (arithmetic, compares, conversions, constant
//!   divisions, `H`'s phis; no memory write, no call, no deopt; under the
//!   arrays switch, proven array reads): nothing inside the inner loop
//!   transfers to the interpreter. A snapshot naming one of its values is
//!   refused, unless it is a per-copy frame ([`read_only_inside`]) named
//!   only by inner-loop nodes -- none of which can transfer, so nothing
//!   consults it; the strip mine leaves every snapshot as it was.
//! * `H` carries no bci (a main header never does), so it is no OSR entry and
//!   its back-edge poll had no mode exit (`back_edge_mode_exit_state`'s
//!   `header-without-bci`). The outer header `O` carries none either: the OSR
//!   door keeps entering the original header (the post loop), as before.
//! * Time to safepoint: a thread asked to stop reaches the outer poll within
//!   `N` inner trips (each `F` body copies of a register-only body, no slow
//!   operation: that is the purity rule). `N` defaults to C2's 1000.
//! * The lowering omits the poll only on a back edge whose every retreating
//!   successor is an inner header recorded in
//!   `Graph::strip_mined_inner_headers` ([`latch_enters_only_strip_inner`]),
//!   one decision in one place -- and only in a VALIDATED block layout
//!   (wave 4, SOUNDREV's first risk): in creation order
//!   (`CRATONVM_JIT_IR_RPO_LAYOUT=0`, or a layout its validator refused) the
//!   outer back edge `X -> O` can read as a forward edge, and then neither
//!   loop would poll.
//!
//! Not taken (yet): a body with a field access or a store. A loop with a bci
//! (an OSR entry: an ordinary counted loop, a post loop, an OSR-door body) is
//! taken only in an OSR compile under `CRATONVM_JIT_IR_STRIP_MINE_OSR` (j27
//! wave 5, lane STRIPOSR2), in the trip-distance form of [`osr`], after the
//! main loops: there the OUTER header takes the bci and the header snapshot.

use super::{build_users, proj_user, FxHashSet};
use crate::ir::{CmpOp, Graph, IrType, MemKind, Node, NodeId, Op, SafepointSnapshot, ScalarOp, NO_NODE};
use crate::ir_check_elim::CheckElision;
use crate::ir_schedule::Schedule;
use rustc_hash::FxHashMap;

/// C2's `LoopStripMiningIter`.
const DEFAULT_ITER: i64 = 1000;
/// Largest strip length accepted from the tunable.
const MAX_ITER: i64 = 1 << 20;
/// Largest per-trip step taken (`N * S` must stay far inside `long`).
const MAX_STEP: i64 = 1 << 16;

/// `CRATONVM_JIT_IR_STRIP_MINE` (j27 wave 2, lane STRIPMINE; performance,
/// **opt-in** `=1`: built without a build or a host A/B). Read per compile.
pub(super) fn strip_mine_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_STRIP_MINE")
}

/// `CRATONVM_JIT_IR_STRIP_MINE_ARRAYS` (j27 wave 4, lane STRIPOSR; opt-in
/// `=1` under `CRATONVM_JIT_IR_STRIP_MINE`): take a body whose primitive array
/// reads `ir_check_elim` proves before the strip mine. Read per loop.
pub(super) fn strip_mine_arrays_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_STRIP_MINE_ARRAYS")
}

/// `CRATONVM_JIT_IR_STRIP_MINE_LONG` (j27 wave 4, lane STRIPOSR; opt-in `=1`
/// under `CRATONVM_JIT_IR_STRIP_MINE`): take a `long` induction, with the
/// saturating strip end. Read per loop.
pub(super) fn strip_mine_long_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_STRIP_MINE_LONG")
}

/// `CRATONVM_JIT_IR_STRIP_MINE_OSR` (j27 wave 5, lane STRIPOSR2; opt-in `=1`
/// under `CRATONVM_JIT_IR_STRIP_MINE`): in an OSR compile, strip-mine a loop
/// whose header carries a bci ([`osr`]). Read per compile.
pub(super) fn strip_mine_osr_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_STRIP_MINE_OSR")
}

/// `CRATONVM_JIT_IR_STRIP_MINE_ITER`: inner trips per strip, default 1000
/// (C2's `LoopStripMiningIter`), clamped to `1..=2^20`; an unparsable value
/// is the default. Read per compile.
pub(super) fn strip_mine_iter() -> i64 {
    parse_iter(
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_STRIP_MINE_ITER")
            .ok()
            .as_deref(),
    )
}

fn parse_iter(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_ITER)
        .clamp(1, MAX_ITER)
}

/// The pipeline's entry: under the switch, strip-mine and drop the inner
/// tests' old compares (dead now).
pub(super) fn run(graph: &mut Graph) {
    if !strip_mine_enabled() {
        return;
    }
    let iter = strip_mine_iter();
    let done = strip_mine_loops(graph, iter);
    if done > 0 {
        super::eliminate_dead_nodes(graph);
        if super::loop_pass_report_enabled() {
            eprintln!("[ir] strip mine: {done} loop(s), {iter} trip(s) per poll");
        }
    }
}

/// A main loop [`recognise`] accepted.
#[derive(Debug)]
struct MainLoop {
    header: NodeId,
    entry: NodeId,
    test: NodeId,
    exit: NodeId,
    /// `H`'s phis, the induction variable among them.
    phis: Vec<NodeId>,
    iv: NodeId,
    step: i64,
    lim: NodeId,
    /// The induction's width: `Int`, or `Long` under the long switch.
    ty: IrType,
    /// The body's array reads, every one proven (empty for a pure body).
    accesses: Vec<NodeId>,
}

/// Strip-mine every main loop of `graph` this module's doc describes, with
/// strips of `iter` inner trips. Returns how many were. Idempotent: an inner
/// header is never offered again, and the outer header is no main header.
pub(super) fn strip_mine_loops(graph: &mut Graph, iter: i64) -> usize {
    let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_UNROLL");
    let iter = iter.clamp(1, MAX_ITER);
    let candidates: Vec<NodeId> = graph
        .partially_unrolled_headers
        .iter()
        .copied()
        .filter(|&h| graph.node_opt(h).is_some_and(|n| n.bytecode_pc.is_none()))
        .filter(|h| !graph.strip_mined_inner_headers.contains(h))
        .collect();
    // The check elimination's verdicts, taken once, on first need. A rewrite
    // of an earlier loop is exact for every other loop's runs, so a verdict
    // taken before it still holds after it.
    let mut proofs: Option<CheckElision> = None;
    let mut done = 0usize;
    for h in candidates {
        match recognise(graph, h, &mut proofs) {
            Ok(m) => {
                rewrite(graph, &m, iter);
                done += 1;
                if dbg {
                    eprintln!(
                        "[DBG_UNROLL] region {h}: strip-mined ({:?}, step {}, {} array read(s), {iter} trip(s) per poll)",
                        m.ty,
                        m.step,
                        m.accesses.len()
                    );
                }
            }
            Err(why) => {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {h}: strip mine refused -- {why}");
                }
            }
        }
    }
    // After the main loops: their test reads the post loop's entry
    // (`exits_into_post_loop`), which an OSR-header strip mine moves.
    if graph.osr_compile && strip_mine_osr_enabled() {
        done += osr::strip_mine_bci_headers(graph, iter, dbg);
    }
    done
}

/// Recognise the main loop at `h`, or say why not.
fn recognise(graph: &Graph, h: NodeId, proofs: &mut Option<CheckElision>) -> Result<MainLoop, &'static str> {
    let users = build_users(graph);
    let (entry, back, test, exit) = loop_control(graph, &users, h)?;
    let phis = header_phis(graph, &users, h)?;
    let (iv, step, lim) = induction(graph, test, &phis)?;
    let body = scan_body(graph, &users, h, back, &phis, None)?;
    if body.variant.contains(&lim) || phis.contains(&lim) {
        return Err("the bound varies in the loop");
    }
    if frames_name_inner_values(graph, &body.variant) {
        return Err("a frame names a loop value");
    }
    if !exits_into_post_loop(graph, &users, exit) {
        return Err("the main exit does not reach its post loop");
    }
    accesses_proven(graph, &body.accesses, proofs)?;
    Ok(MainLoop {
        header: h,
        entry,
        test,
        exit,
        phis,
        iv,
        step,
        lim,
        ty: graph.nodes[iv as usize].ty,
        accesses: body.accesses,
    })
}

/// The control users of `id`.
fn control_users(graph: &Graph, users: &[Vec<NodeId>], id: NodeId) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = users[id as usize]
        .iter()
        .copied()
        .filter(|&u| graph.node_opt(u).is_some_and(|n| n.op.is_control()))
        .collect();
    out.dedup();
    out
}

/// `(entry, back, test, exit)` of a single-block loop `H: Merge(entry, back)`,
/// `test = If(H, cond)`, `back = Proj(0)(test)` going straight to `H`, `exit =
/// Proj(1)(test)`.
fn loop_control(
    graph: &Graph,
    users: &[Vec<NodeId>],
    h: NodeId,
) -> Result<(NodeId, NodeId, NodeId, NodeId), &'static str> {
    let hn = graph.node_opt(h).ok_or("no header")?;
    let &[entry, back] = hn.inputs.as_slice() else {
        return Err("not a two-input header");
    };
    if !matches!(hn.op, Op::Merge | Op::Region) || entry == NO_NODE || back == NO_NODE {
        return Err("not a header with an entry and a back edge");
    }
    let bn = graph.node_opt(back).ok_or("no back edge")?;
    let test = bn.input_opt(0).unwrap_or(NO_NODE);
    if bn.op != Op::Proj(0) || graph.node_opt(test).map(|n| &n.op) != Some(&Op::If) {
        return Err("the back edge is not the true edge of a test");
    }
    if graph.nodes[test as usize].input_opt(0) != Some(h) {
        return Err("the test is not on the header");
    }
    if control_users(graph, users, h) != [test] || control_users(graph, users, back) != [h] {
        return Err("the body is not one block");
    }
    let exit = proj_user(graph, test, 1);
    if exit == NO_NODE {
        return Err("the test has no exit edge");
    }
    Ok((entry, back, test, exit))
}

/// `H`'s phis, each `[H, entry value, back value]` and none of memory.
fn header_phis(graph: &Graph, users: &[Vec<NodeId>], h: NodeId) -> Result<Vec<NodeId>, &'static str> {
    let mut phis: Vec<NodeId> = users[h as usize]
        .iter()
        .copied()
        .filter(|&u| {
            graph
                .node_opt(u)
                .is_some_and(|n| n.op == Op::Phi && n.input_opt(0) == Some(h))
        })
        .collect();
    phis.sort_unstable();
    phis.dedup();
    for &p in &phis {
        let n = &graph.nodes[p as usize];
        if n.inputs.len() != 3 || n.inputs[1] == NO_NODE || n.inputs[2] == NO_NODE {
            return Err("a header phi without two values");
        }
        if n.ty == IrType::Memory {
            return Err("a memory phi (the body touches memory)");
        }
    }
    Ok(phis)
}

/// `(iv, step, lim)` of the test `iv < lim`: `iv` a phi of `H` of the width
/// [`induction_width`] takes, stepping by a constant `0 < step <= MAX_STEP`,
/// `lim` of the same width.
fn induction(graph: &Graph, test: NodeId, phis: &[NodeId]) -> Result<(NodeId, i64, NodeId), &'static str> {
    let cond = graph.nodes[test as usize].input_opt(1).unwrap_or(NO_NODE);
    let cn = graph.node_opt(cond).ok_or("no test condition")?;
    let (iv, lim) = match (&cn.op, cn.inputs.as_slice()) {
        (Op::Cmp(CmpOp::Lt), &[a, b]) => (a, b),
        (Op::Cmp(CmpOp::Gt), &[a, b]) => (b, a),
        _ => return Err("the stay condition is not `iv < lim`"),
    };
    if !phis.contains(&iv) {
        return Err("the compared value is not a phi of the header");
    }
    let ty = induction_width(graph, iv, lim)?;
    let step = constant_step(graph, iv, ty).ok_or("the induction does not step by a constant")?;
    if !(1..=MAX_STEP).contains(&step) {
        return Err("the step is out of range");
    }
    Ok((iv, step, lim))
}

/// The width of `iv < lim`: `Int`, or `Long` under the long switch; both
/// operands of it.
fn induction_width(graph: &Graph, iv: NodeId, lim: NodeId) -> Result<IrType, &'static str> {
    let ty = graph.node_opt(iv).map(|n| n.ty).ok_or("no induction")?;
    if graph.node_opt(lim).map(|n| n.ty) != Some(ty) {
        return Err("the bound is not as wide as the induction");
    }
    match ty {
        IrType::Int => Ok(ty),
        IrType::Long if strip_mine_long_enabled() => Ok(ty),
        IrType::Long => Err("a long induction (CRATONVM_JIT_IR_STRIP_MINE_LONG is off)"),
        _ => Err("the compared value is not an int or a long"),
    }
}

/// The constant `c` of `iv`'s back value `iv + c` (either operand order), as
/// the `ty` value it is.
fn constant_step(graph: &Graph, iv: NodeId, ty: IrType) -> Option<i64> {
    let next = graph.node_opt(graph.nodes[iv as usize].input_opt(2)?)?;
    let other = match (&next.op, next.inputs.as_slice()) {
        (Op::Add, &[x, y]) if x == iv => y,
        (Op::Add, &[x, y]) if y == iv => x,
        _ => return None,
    };
    match graph.node_opt(other)?.op {
        Op::Const(c) if ty == IrType::Long => Some(c),
        // Cast: an `int` constant is its low 32 bits.
        Op::Const(c) => Some(i64::from(c as i32)),
        _ => None,
    }
}

/// May `n` run inside a poll-free inner loop? Pure arithmetic, compares and
/// conversions, and a division by a non-zero constant; nothing that reads or
/// writes memory, calls, or can transfer to the interpreter.
fn inner_loop_node(graph: &Graph, n: &Node) -> bool {
    let pure = n.op.is_pure() || matches!(n.op, Op::Div | Op::Rem);
    pure && n.ty != IrType::Memory && crate::ir_lower::node_cannot_deopt(graph, n)
}

/// A body read the arrays switch may take once its checks are proven: a
/// primitive element read or an `arraylength`, with no control user (no
/// exception edge of its own).
fn array_read(graph: &Graph, users: &[Vec<NodeId>], id: NodeId, n: &Node) -> bool {
    let read = matches!(n.op, Op::ArrayLoad(_) | Op::ArrayLength) && !matches!(n.op, Op::ArrayLoad(MemKind::Ref));
    read && users[id as usize]
        .iter()
        .all(|&u| graph.node_opt(u).is_none_or(|x| !x.op.is_control()))
}

/// What [`scan_body`] found.
struct BodyScan {
    /// The loop-variant values (array reads included).
    variant: FxHashSet<NodeId>,
    /// The array reads among them, each still to be proven.
    accesses: Vec<NodeId>,
}

/// The loop-variant values: everything computed from `H`'s phis (up to the
/// next phi) plus what is anchored at `H` or the back edge. `Err` when one of
/// them may not run in a poll-free loop; an array read is collected for
/// [`accesses_proven`].
fn scan_body(
    graph: &Graph,
    users: &[Vec<NodeId>],
    h: NodeId,
    back: NodeId,
    phis: &[NodeId],
    inside: Option<&[NodeId]>,
) -> Result<BodyScan, &'static str> {
    // j27 wave 5 (lane STRIPOSR2): with `inside` (the loop's own control
    // nodes), a node pinned to any other control node runs outside the loop
    // -- the exit's code reading `H`'s phis, a promoted cell's exit
    // write-back -- and is neither scanned nor followed.
    let outside = |n: &Node| {
        inside.is_some_and(|ctrls| {
            n.input_opt(0)
                .filter(|&c| graph.node_opt(c).is_some_and(|x| x.op.is_control()))
                .is_some_and(|c| !ctrls.contains(&c))
        })
    };
    let mut scan = BodyScan {
        variant: FxHashSet::default(),
        accesses: Vec::new(),
    };
    let mut work: Vec<NodeId> = phis.to_vec();
    work.extend(users[h as usize].iter().chain(users[back as usize].iter()).copied());
    while let Some(v) = work.pop() {
        let Some(n) = graph.node_opt(v) else {
            continue;
        };
        let skipped =
            n.op == Op::Dead || n.op.is_control() || (n.op == Op::Phi && !phis.contains(&v)) || outside(n);
        if skipped || scan.variant.contains(&v) {
            continue;
        }
        if array_read(graph, users, v, n) {
            scan.accesses.push(v);
        } else if n.op != Op::Phi && !inner_loop_node(graph, n) {
            return Err("a body node that is not pure arithmetic or an array read");
        }
        scan.variant.insert(v);
        work.extend(users[v as usize].iter().copied());
    }
    Ok(scan)
}

/// Are all of `accesses` proven by the check elimination on `graph` as it
/// stands (null and, for an element read, bounds)? Asks it at most once per
/// pass ([`strip_mine_loops`]), and only under the arrays switch.
fn accesses_proven(
    graph: &Graph,
    accesses: &[NodeId],
    proofs: &mut Option<CheckElision>,
) -> Result<(), &'static str> {
    if accesses.is_empty() {
        return Ok(());
    }
    if !strip_mine_arrays_enabled() {
        return Err("an array read (CRATONVM_JIT_IR_STRIP_MINE_ARRAYS is off)");
    }
    let p = proofs
        .get_or_insert_with(|| crate::ir_check_elim::analyze(graph, &crate::ir_schedule::schedule(graph)));
    let proven = |a: NodeId| {
        let bounds = graph.nodes[a as usize].op == Op::ArrayLength || p.bounds_elided(a);
        p.null_elided(a) && bounds
    };
    if accesses.iter().all(|&a| proven(a)) {
        Ok(())
    } else {
        Err("an array read whose check the range pass keeps")
    }
}

/// Does a snapshot that something may consult name one of `values`?
fn frames_name_inner_values(graph: &Graph, values: &FxHashSet<NodeId>) -> bool {
    let namers = snapshot_namers(graph);
    graph.safepoints.iter().enumerate().any(|(si, sp)| {
        names_any_of(sp, values) && !read_only_inside(sp, namers.get(&si).map(Vec::as_slice), values)
    })
}

/// Does `sp` name one of `values`?
fn names_any_of(sp: &SafepointSnapshot, values: &FxHashSet<NodeId>) -> bool {
    sp.locals
        .iter()
        .chain(sp.stack.iter())
        .chain(sp.monitors.iter())
        .any(|v| values.contains(v))
}

/// Is `sp` a per-copy frame of the main loop (`install_copy_frames`) that
/// only inner-loop nodes name? Such a frame is read only when one of its
/// namers transfers, and no inner-loop node can ([`scan_body`] took only
/// those; an array read only once proven). A frame looked up by pc, a chain
/// frame or a post-call one is never this.
fn read_only_inside(sp: &SafepointSnapshot, namers: Option<&[NodeId]>, inner: &FxHashSet<NodeId>) -> bool {
    sp.copy_frame
        && !sp.post_call_only
        && sp.post_call_of == NO_NODE
        && sp.splice_scope.is_none()
        && namers.is_some_and(|ns| ns.iter().all(|n| inner.contains(n)))
}

/// The live nodes naming each snapshot through `frame_snapshot`.
fn snapshot_namers(graph: &Graph) -> FxHashMap<usize, Vec<NodeId>> {
    let mut out: FxHashMap<usize, Vec<NodeId>> = FxHashMap::default();
    for (id, n) in graph.nodes.iter().enumerate() {
        if n.op == Op::Dead {
            continue;
        }
        if let Some(si) = n.frame_snapshot {
            // Cast: arena index to node id.
            out.entry(si as usize).or_default().push(id as NodeId);
        }
    }
    out
}

/// Does the main exit `x` lead only to a join whose one control user is a
/// post loop (a partially unrolled header with a bci)?
fn exits_into_post_loop(graph: &Graph, users: &[Vec<NodeId>], x: NodeId) -> bool {
    let after_exit = control_users(graph, users, x);
    let &[join] = after_exit.as_slice() else {
        return false;
    };
    if graph.node_opt(join).map(|n| &n.op) != Some(&Op::Merge) {
        return false;
    }
    let after_join = control_users(graph, users, join);
    let &[post] = after_join.as_slice() else {
        return false;
    };
    graph.partially_unrolled_headers.contains(&post)
        && graph
            .node_opt(post)
            .is_some_and(|n| n.bytecode_pc.is_some() && n.input_opt(0) == Some(join))
}

fn constant(graph: &mut Graph, v: i64, ty: IrType) -> NodeId {
    graph.add(Op::Const(v), ty, vec![], None)
}

/// `(lim2, slim)` from the outer induction value `q`, at the loop's width.
fn strip_limits(graph: &mut Graph, m: &MainLoop, q: NodeId, iter: i64) -> (NodeId, NodeId) {
    if m.ty == IrType::Long {
        long_strip_limits(graph, m, q, iter)
    } else {
        int_strip_limits(graph, m, q, iter)
    }
}

/// `lim2 = min(lim, MAX - step + 1)` and `slim = (int) min((long) q + iter *
/// step, (long) lim2)`.
fn int_strip_limits(graph: &mut Graph, m: &MainLoop, q: NodeId, iter: i64) -> (NodeId, NodeId) {
    let cap = constant(graph, i64::from(i32::MAX) - m.step + 1, IrType::Int);
    let lim2 = graph.add(Op::ScalarIntrinsic(ScalarOp::MinI), IrType::Int, vec![m.lim, cap], None);
    let wide_q = graph.add(Op::I2L, IrType::Long, vec![q], None);
    // `iter <= 2^20` and `step <= 2^16`: the span is far inside `long`.
    let span = constant(graph, iter * m.step, IrType::Long);
    let end = graph.add(Op::Add, IrType::Long, vec![wide_q, span], None);
    let wide_lim2 = graph.add(Op::I2L, IrType::Long, vec![lim2], None);
    let wide_slim = graph.add(Op::ScalarIntrinsic(ScalarOp::MinL), IrType::Long, vec![end, wide_lim2], None);
    let slim = graph.add(Op::L2I, IrType::Int, vec![wide_slim], None);
    (lim2, slim)
}

/// `lim2 = min(lim, Long.MAX - step + 1)` and the saturating `slim =
/// min(min(q, Long.MAX - span) + span, lim2)`, `span = iter * step` (the
/// module doc's "A `long` induction").
fn long_strip_limits(graph: &mut Graph, m: &MainLoop, q: NodeId, iter: i64) -> (NodeId, NodeId) {
    let long = IrType::Long;
    // `iter <= 2^20` and `step <= 2^16`: the span is far inside `long`.
    let span = iter * m.step;
    let cap = constant(graph, i64::MAX - m.step + 1, long);
    let lim2 = min_long(graph, m.lim, cap);
    let q_cap = constant(graph, i64::MAX - span, long);
    let q_low = min_long(graph, q, q_cap);
    let span_c = constant(graph, span, long);
    let end = graph.add(Op::Add, long, vec![q_low, span_c], None);
    let slim = min_long(graph, end, lim2);
    (lim2, slim)
}

fn min_long(graph: &mut Graph, a: NodeId, b: NodeId) -> NodeId {
    graph.add(Op::ScalarIntrinsic(ScalarOp::MinL), IrType::Long, vec![a, b], None)
}

/// The edit (nothing below refuses): the outer header and its phis, the strip
/// limit as the inner test's bound, the outer test at the inner exit.
fn rewrite(graph: &mut Graph, m: &MainLoop, iter: i64) {
    let outer = open_outer_loop(graph, m);
    let (lim2, slim) = strip_limits(graph, m, outer.iv, iter);
    let inner_cond = graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![m.iv, slim], None);
    graph.set_input(m.test, 1, inner_cond);
    let outer_cond = graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![m.iv, lim2], None);
    close_outer_loop(graph, m, &outer, outer_cond);
    graph.strip_mine_proven_accesses.extend(m.accesses.iter().copied());
}

/// The outer header [`open_outer_loop`] made.
struct OuterLoop {
    header: NodeId,
    /// The outer twin `q` of each of `H`'s phis, in [`MainLoop::phis`] order.
    phis: Vec<NodeId>,
    /// The induction's twin.
    iv: NodeId,
}

/// The outer header `O` in front of `H`, with a twin `q = phi(entry value)` for
/// each of `H`'s phis, which now enter from it. The back edge comes in
/// [`close_outer_loop`].
fn open_outer_loop(graph: &mut Graph, m: &MainLoop) -> OuterLoop {
    let header = graph.add(Op::Merge, IrType::Control, vec![m.entry], None);
    let phis: Vec<NodeId> = m
        .phis
        .iter()
        .map(|&p| {
            let (ty, init) = (graph.nodes[p as usize].ty, graph.nodes[p as usize].inputs[1]);
            graph.add(Op::Phi, ty, vec![header, init], None)
        })
        .collect();
    graph.set_input(m.header, 0, header);
    for (&p, &q) in m.phis.iter().zip(phis.iter()) {
        graph.set_input(p, 1, q);
    }
    let iv = phis[m.phis.iter().position(|&p| p == m.iv).unwrap_or(0)];
    OuterLoop { header, phis, iv }
}

/// The outer test `if (outer_cond) goto O` on `H`'s exit, its exit taking
/// over every reader of `H`'s, and `H` recorded as a strip-mined inner header.
fn close_outer_loop(graph: &mut Graph, m: &MainLoop, outer: &OuterLoop, outer_cond: NodeId) {
    let outer_test = graph.add(Op::If, IrType::Control, vec![m.exit, outer_cond], None);
    let outer_back = graph.add(Op::Proj(0), IrType::Control, vec![outer_test], None);
    let outer_exit = graph.add(Op::Proj(1), IrType::Control, vec![outer_test], None);
    // Every reader of the inner exit now reads the outer one; the outer test
    // itself stays on the inner exit.
    graph.replace_all_uses(m.exit, outer_exit);
    graph.set_input(outer_test, 0, m.exit);
    graph.push_input(outer.header, outer_back);
    for (&p, &q) in m.phis.iter().zip(outer.phis.iter()) {
        graph.push_input(q, p);
    }
    graph.strip_mined_inner_headers.push(m.header);
}

/// j27 wave 5 (lane STRIPOSR2): the outer twin `q` of `v` when `v` is a phi
/// of a strip-mined inner header whose outer header is `outer` (`q` is `v`'s
/// entry input, a phi of `outer`; [`open_outer_loop`]). A promoted cell's
/// header value is such a phi once its loop is strip-mined in the OSR form
/// ([`osr`]); at the outer poll its current value is `q`'s input on that edge,
/// which is `v` itself. `None` for every other value.
pub(crate) fn strip_outer_twin(graph: &Graph, v: NodeId, outer: NodeId) -> Option<NodeId> {
    let n = graph.node_opt(v)?;
    let inner = n.input_opt(0)?;
    let nested = n.op == Op::Phi
        && graph.strip_mined_inner_headers.contains(&inner)
        && graph.node_opt(inner)?.input_opt(0) == Some(outer);
    let q = n.input_opt(1).filter(|_| nested)?;
    graph
        .node_opt(q)
        .is_some_and(|x| x.op == Op::Phi && x.input_opt(0) == Some(outer))
        .then_some(q)
}

/// May the back-edge poll of block `latch` be left out? Exactly when the
/// block layout was validated (its retreating edges are the loops' back
/// edges, the outer one included) and every successor `latch` retreats to
/// (`succ <= latch`, the lowerer's back-edge rule) starts at a strip-mined
/// inner header: the outer loop polls once per strip.
pub(crate) fn latch_enters_only_strip_inner(graph: &Graph, schedule: &Schedule, latch: usize) -> bool {
    if graph.strip_mined_inner_headers.is_empty() || !schedule.layout.applied {
        return false;
    }
    let Some(block) = schedule.blocks.get(latch) else {
        return false;
    };
    let retreating: Vec<usize> = block.successors.iter().copied().filter(|&s| s <= latch).collect();
    !retreating.is_empty()
        && retreating.iter().all(|&s| {
            schedule
                .blocks
                .get(s)
                .is_some_and(|b| graph.strip_mined_inner_headers.contains(&b.ctrl))
        })
}

mod osr;

#[cfg(test)]
mod tests;

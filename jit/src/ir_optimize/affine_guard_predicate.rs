//! JIT round 23 wave 8 (lane W8-L7; `CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE`,
//! perf, default ON, `=0` kills; LOSS on the n23-w8g A/B before the dead-compare
//! cleanup -- see "Why it did not pay" below -- then a WIN on n23-w8h with it,
//! `R23W4SegMono` heapget 211/125/222 ns/op on vs 232/185/266 off, heapput
//! 155/138/200 vs 223/154/221): the counted-loop range predicate for a
//! generic body `Guard(Cmp)` on an affine function of the induction variable.
//!
//! # What
//!
//! The inline `MemorySegment` element access (`IrBuilder::
//! try_emit_ffm_element_inline`) guards every access with
//! `Guard(off >= 0)` and `Guard(off <= length - w)`, `off = (long) k * w`.
//! In `for (int k = init; k < n; k++)` both stay in the loop: the range,
//! monotone and offset predicates answer array bounds checks only, and the
//! length predicate moves only INVARIANT conditions
//! (`r23-w2-memorysegment-heap-accessors-10000x-FIXED-20261024.md`, wave 8c).
//!
//! For such a loop, a body guard `Guard(Cmp(op, A, X))` (either operand
//! order) with
//!
//! * `A = c * (long) k + d` built from `k` itself (an `int` compare), or from
//!   `I2L(k)` by `long` `Mul` / `Shl` / `Add` / `Sub` with constants
//!   (`affine`), `c != 0`;
//! * `X` loop-invariant, `op` one of `< <= > >=`;
//! * the guard reached on every iteration (`anchored_on_every_iteration`)
//!
//! is replaced by ONE pre-header guard, named by the loop-entry frame
//! (`loop_entry_frame`) at the header bci:
//!
//! ```text
//!   Guard{header}( init >= n  ||  A(k*) op X )      (all in long)
//! ```
//!
//! where `k*` is the extreme the compare needs: for `>`/`>=` the minimum of
//! `A` over `k in [init, n - 1]`, for `<`/`<=` the maximum, i.e. `init` or
//! `n - 1` by the sign of `c`.
//!
//! # Why sound
//!
//! * In the body `init <= k <= n - 1`: the header test `k < n` holds on the
//!   stay edge, `k` starts at `init` and only steps by `+1` while `k < n`, so
//!   it never wraps (`n - 1 < i32::MAX`).
//! * `A` is affine with no wrap: `k` is an `int` widened before any `long`
//!   arithmetic, `|c| <= 2^31` and `|d| <= 2^61` are checked here, so every
//!   `A(k)` is the exact integer; it is monotone in `k`, so the compare holds
//!   for every `k` of the range iff it holds at the extreme.
//! * `init >= n` (computed in long, no wrap): the body never runs, nothing to
//!   check -- so the predicate speculates nothing about the trip count.
//! * A failing pre-header guard only DEOPTS, to the loop entry with the entry
//!   values: the interpreter runs the loop itself and raises whatever the body
//!   raises, at the iteration it raises it (the segment accessor's
//!   `IndexOutOfBoundsException`, after the earlier iterations' effects). A
//!   loop whose predicate keeps failing (an early `break` before the failing
//!   index, say) is withdrawn by the de-spec registry at the header bci
//!   (`PredicateKind::Range`), after which the body guards stay.
//! * Nodes the body guard licensed as their token now name the pre-header
//!   guard, which dominates them (the length predicate's rule).
//!
//! # Not planted
//!
//! The optimizing OSR route and OSR-door graphs (their entries skip the
//! pre-header), a spliced header (no flat frame), a header the de-spec
//! registry withdrew, a guard at a division's bci (its zero test keys on the
//! anchor), an exception-edge anchor, a pre-header that may trap first.

use super::{
    anchored_at, anchored_on_every_iteration, build_users, chain_predicate_guard_frame,
    header_is_spliced, is_loop_invariant, loop_body_with, loop_entry_chain_frame, loop_entry_frame,
    loop_entry_memory, loop_headers, preheader_traps_block_entry_predicate, proj_user,
};
use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, NO_NODE};
use rustc_hash::FxHashSet;

/// At most this many pre-header guards per compile.
const MAX_AFFINE_GUARDS: usize = 16;
/// `|c|` bound: `c * k` stays exact in `i64` for any `int` `k`.
const MAX_SCALE: i64 = 1 << 31;
/// `|d|` bound: `c * k + d` stays exact in `i64`.
const MAX_OFFSET: i64 = 1 << 61;
/// Expression depth the affine walk accepts.
const MAX_DEPTH: u32 = 6;

/// `CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE` -- default ON, `=0` kills (the
/// n23-w8h A/B in the module notes). Read per compile; no `static`.
///
/// Why it did not pay (n23-w8g `heapGet`, `CRATONVM_DBG=licm`): it engages
/// (`2 body guard(s) -> 2 pre-header guard(s)`, no de-spec line), but the
/// retired guards' `Cmp` nodes stay in the loop: nothing runs dead-node
/// elimination after the predicate section, so each is still lowered as
/// `cmp; setcc; movzx; store` into a dead home (and, with no guard reading
/// it any more, L4's compare-to-guard sink no longer applies). The loop kept
/// its instruction count; only the `test; je` pair per guard went. Since
/// wave 8e the call site runs `eliminate_dead_nodes` whenever this planter
/// replaced a guard, so those compares die; n23-w8h measured the win.
pub(super) fn affine_guard_predicate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE")
}

/// `CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE_INT` (the r23 L8 heap-buffer close;
/// perf, **default ON**, `=0` kills): the compared side may also be an `int`
/// expression of `k` -- `int` `Mul` / `Shl` / `Add` / `Sub` by constants
/// ([`affine_int`]), `Buffer.checkIndex`'s `k << 2` in `b.getInt(k << 2)`.
///
/// An `int` expression may wrap where the `long` one does not. Every one of
/// those four ops is a ring homomorphism mod 2^32, so the body's value is
/// `wrap32(c * k + d)` however the intermediate steps wrapped; and since
/// `c * k + d` is monotone in `k`, it fits in an `int` over the whole range
/// iff it fits at both extremes. The pre-header guard checks exactly that
/// beside the compare (`init >= n || (A(lo) >= MIN && A(hi) <= MAX && A(k*)
/// op X)`), so wherever the loop runs past it every body value IS the exact
/// one the `long` compare used. Read per compile; no static.
pub(super) fn affine_int_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE_INT")
}

/// `CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE_CHAIN` (the r23 L8 heap-buffer
/// close; perf, **default ON**, `=0` kills): the predicates for a loop SPLICED
/// from a callee too -- `absGet`'s loop inside the probe's `loop`, the shape
/// every buffer kernel called from a hotter method has. The frame is the
/// loop-entry CHAIN (`loop_entry_chain_frame`, or the one an earlier chain
/// predicate of the same loop already named), the guard is listed in
/// `Graph::chain_predicate_guards` under `PredicateKind::Range`, so the VM
/// charges a failure at the compiling method's call site under the chain id
/// of the callee's header, and the withdrawal is asked there
/// (`crate::deopt::chain_predicate_withdrawn`) -- the null-base predicate's
/// discipline (`null_base_load_predicate_chain_enabled`). Read per compile.
pub(super) fn affine_chain_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE_CHAIN")
}

/// `CRATONVM_JIT_IR_PREDICATES_AFTER_FIELD_FORWARD` (the r23 L8 heap-buffer
/// close; perf, **default ON**, `=0` kills): once more after the heap-buffer
/// loop field forward, for the loops it changed, with the invariant body
/// guards moved too ([`plant_affine_guard_predicates_after_forward`]).
///
/// Why: the length predicate (which moves a guard whose condition is
/// invariant) and this planter run BEFORE the forward that makes a buffer's
/// `limit`, `segment` and `hb` pre-header values, so `absGet`'s
/// `Buffer.session()` test `segment != null` and its `checkIndex` bound
/// `k << 2 < limit - 3` were still in-body conditions then, and stayed in
/// the loop (`CRATONVM_DBG_LICM`: `length predicate ... guard n38 -> n76`,
/// then `loop field forward`, and nothing after it). Read per compile.
pub(crate) fn predicates_after_field_forward_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_PREDICATES_AFTER_FIELD_FORWARD")
}

fn const_of(graph: &Graph, id: NodeId, ty: IrType) -> Option<i64> {
    let n = graph.node_opt(id)?;
    match n.op {
        Op::Const(v) if n.ty == ty => Some(v),
        _ => None,
    }
}

fn bounded(c: i64, d: i64) -> Option<(i64, i64)> {
    (c != 0 && c.abs() <= MAX_SCALE && d.abs() <= MAX_OFFSET).then_some((c, d))
}

/// `(c, d)` with `v == c * (long) iv + d` exactly, for a `long` `v` built
/// from `I2L(iv)` by constant `Mul` / `Shl` / `Add` / `Sub`.
pub(super) fn affine_long(graph: &Graph, v: NodeId, iv: NodeId, depth: u32) -> Option<(i64, i64)> {
    if depth > MAX_DEPTH {
        return None;
    }
    let n = graph.node_opt(v)?;
    if n.ty != IrType::Long {
        return None;
    }
    let &[a, b] = n.inputs.as_slice() else {
        return match n.op {
            Op::I2L if n.inputs.first() == Some(&iv) && n.inputs.len() == 1 => Some((1, 0)),
            _ => None,
        };
    };
    let sub = |x: NodeId| affine_long(graph, x, iv, depth + 1);
    let k = |x: NodeId| const_of(graph, x, IrType::Long);
    match n.op {
        Op::Mul => {
            let ((c, d), m) = match (k(a), k(b)) {
                (Some(m), None) => (sub(b)?, m),
                (None, Some(m)) => (sub(a)?, m),
                _ => return None,
            };
            bounded(c.checked_mul(m)?, d.checked_mul(m)?)
        }
        Op::Shl => {
            let s = graph
                .node_opt(b)
                .and_then(|x| match x.op {
                    Op::Const(s) if (0..=31).contains(&s) => Some(s),
                    _ => None,
                })?;
            let (c, d) = sub(a)?;
            let m = 1i64 << s;
            bounded(c.checked_mul(m)?, d.checked_mul(m)?)
        }
        Op::Add => {
            let ((c, d), e) = match (k(a), k(b)) {
                (Some(e), None) => (sub(b)?, e),
                (None, Some(e)) => (sub(a)?, e),
                _ => return None,
            };
            bounded(c, d.checked_add(e)?)
        }
        Op::Sub => {
            let e = k(b)?;
            let (c, d) = sub(a)?;
            bounded(c, d.checked_sub(e)?)
        }
        _ => None,
    }
}

/// `(c, d)` with `v == wrap32(c * iv + d)`, for an `int` `v` built from `iv`
/// itself by `int` `Mul` / `Shl` / `Add` / `Sub` with constants (see
/// [`affine_int_enabled`] for why the wrap is answered in the pre-header).
fn affine_int(graph: &Graph, v: NodeId, iv: NodeId, depth: u32) -> Option<(i64, i64)> {
    if v == iv {
        return Some((1, 0));
    }
    if depth > MAX_DEPTH {
        return None;
    }
    let n = graph.node_opt(v)?;
    if n.ty != IrType::Int {
        return None;
    }
    let &[a, b] = n.inputs.as_slice() else {
        return None;
    };
    let sub = |x: NodeId| affine_int(graph, x, iv, depth + 1);
    let k = |x: NodeId| const_of(graph, x, IrType::Int);
    match n.op {
        Op::Mul => {
            let ((c, d), m) = match (k(a), k(b)) {
                (Some(m), None) => (sub(b)?, m),
                (None, Some(m)) => (sub(a)?, m),
                _ => return None,
            };
            bounded(c.checked_mul(m)?, d.checked_mul(m)?)
        }
        Op::Shl => {
            let s = k(b).filter(|s| (0..=31).contains(s))?;
            let (c, d) = sub(a)?;
            let m = 1i64 << s;
            bounded(c.checked_mul(m)?, d.checked_mul(m)?)
        }
        Op::Add => {
            let ((c, d), e) = match (k(a), k(b)) {
                (Some(e), None) => (sub(b)?, e),
                (None, Some(e)) => (sub(a)?, e),
                _ => return None,
            };
            bounded(c, d.checked_add(e)?)
        }
        Op::Sub => {
            let e = k(b)?;
            let (c, d) = sub(a)?;
            bounded(c, d.checked_sub(e)?)
        }
        _ => None,
    }
}

/// `CRATONVM_JIT_IR_RANGE_GUARD_PREDICATE` (the r23 L8 heap-buffer close;
/// perf, **default ON**, `=0` kills): a body guard on a value whose range the
/// expression itself bounds, whatever its inputs -- `(i & 7) << 4`, `fieldmix`'s
/// record offset ([`static_int_range`]) -- is planted once at the range's
/// extreme against its invariant bound, as an affine one is at `k*`, and a
/// scoped access at such an offset is proven over that range
/// ([`scoped_access_predicate_enabled`]). Sound for the reason the affine
/// form is: every value the body computes lies in the range, so the compare
/// holds for all of them iff it holds at the extreme. Read per compile.
pub(super) fn range_guard_predicate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_RANGE_GUARD_PREDICATE")
}

/// The exact range `[lo, hi]` of every value the `int` node `v` can take,
/// from its own shape: a constant, `x & m` (`m >= 0`: `[0, m]` whatever `x`),
/// or a constant `<<`, `+`, `-`, `*` (`>= 0`) of a ranged value that stays in
/// `int`. `None` for anything else.
fn static_int_range(graph: &Graph, v: NodeId, depth: u32) -> Option<(i64, i64)> {
    if depth > MAX_DEPTH {
        return None;
    }
    let n = graph.node_opt(v)?;
    if n.ty != IrType::Int {
        return None;
    }
    if let Op::Const(c) = n.op {
        return Some((c, c));
    }
    let &[a, b] = n.inputs.as_slice() else {
        return None;
    };
    let k = |x: NodeId| const_of(graph, x, IrType::Int);
    let sub = |x: NodeId| static_int_range(graph, x, depth + 1);
    let fits = |lo: i64, hi: i64| {
        (i64::from(i32::MIN) <= lo && hi <= i64::from(i32::MAX) && lo <= hi).then_some((lo, hi))
    };
    match n.op {
        Op::And => {
            let m = k(b).or_else(|| k(a)).filter(|&m| m >= 0)?;
            Some((0, m))
        }
        Op::Shl => {
            let s = k(b).filter(|s| (0..=30).contains(s))?;
            let (lo, hi) = sub(a)?;
            if lo < 0 {
                return None;
            }
            fits(lo << s, hi << s)
        }
        Op::Add => {
            let ((lo, hi), e) = match (k(a), k(b)) {
                (Some(e), None) => (sub(b)?, e),
                (None, Some(e)) => (sub(a)?, e),
                _ => return None,
            };
            fits(lo + e, hi + e)
        }
        Op::Sub => {
            let e = k(b)?;
            let (lo, hi) = sub(a)?;
            fits(lo - e, hi - e)
        }
        Op::Mul => {
            let ((lo, hi), m) = match (k(a), k(b)) {
                (Some(m), None) => (sub(b)?, m),
                (None, Some(m)) => (sub(a)?, m),
                _ => return None,
            };
            if m < 0 {
                return None;
            }
            fits(lo.checked_mul(m)?, hi.checked_mul(m)?)
        }
        _ => None,
    }
}

/// [`static_int_range`] of a `long` offset part: `I2L` of a ranged `int`.
fn static_long_range(graph: &Graph, v: NodeId) -> Option<(i64, i64)> {
    let n = graph.node_opt(v)?;
    match n.op {
        Op::Const(c) if n.ty == IrType::Long => Some((c, c)),
        Op::I2L if n.ty == IrType::Long => static_int_range(graph, n.input_opt(0)?, 0),
        _ => None,
    }
}

/// The compared side `v` as `(c, d)`: `iv` itself (an `int` compare), or a
/// `long` affine expression of `I2L(iv)`.
fn affine(graph: &Graph, v: NodeId, iv: NodeId) -> Option<(i64, i64)> {
    if v == iv {
        return Some((1, 0));
    }
    affine_long(graph, v, iv, 0)
}

/// [`affine`], or under [`affine_int_enabled`] an `int` expression of `iv`
/// ([`affine_int`]), as `(c, d, wraps)`: `wraps` when the body computes it in
/// `int` arithmetic, whose range the pre-header guard must check too.
fn affine_form(graph: &Graph, v: NodeId, iv: NodeId, int_on: bool) -> Option<(i64, i64, bool)> {
    if let Some((c, d)) = affine(graph, v, iv) {
        return Some((c, d, false));
    }
    if !int_on {
        return None;
    }
    affine_int(graph, v, iv, 0).map(|(c, d)| (c, d, true))
}

/// `CRATONVM_JIT_IR_SCOPED_ACCESS_PREDICATE` (the r23 L8 heap-buffer close;
/// perf, **default ON**, `=0` kills): a counted loop's raw scoped heap access
/// (`Op::Unbox` `ScopedGet` / `ScopedPut` over a `byte[]` base,
/// `HeapByteBuffer.getInt(i)`'s fold) is proven once at the loop entry
/// ([`ScopedPlan`]) and filed in `Graph::ffm_proven_heap_accesses`, so the
/// lowering drops its per-access receiver null test, off-heap arm and payload
/// bounds check (`ir_lower::Lowerer::emit_scoped_raw_tail`'s `proven` arm).
///
/// The pre-header guards, named by the loop-entry frame (or chain), each
/// deopting to the loop entry:
///
/// * `sma != null && base != null`, then `ArrayLength(base)`;
/// * unless the loop runs zero times: the offset `D + c * k + d` (`D` an
///   invariant `long`, or none) at both extremes of `k` lies in the payload,
///   `16 <= off` and `off + width <= 16 + length`; `D` is in `[0, 2^40]`, so
///   the `long` sums cannot wrap; an `int` index expression fits at both
///   extremes ([`affine_int_enabled`]'s argument).
///
/// Every offset of the loop is between the two extremes (the expression is
/// monotone in `k`), so every access is in the array -- the facts the lowering
/// tested per access, established once; nothing about `HeapByteBuffer`'s own
/// invariants is assumed. HotSpot's `Unsafe` access, the JDK method these
/// folds replace, checks nothing at all. Read per compile.
pub(super) fn scoped_access_predicate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SCOPED_ACCESS_PREDICATE")
}

/// [`scoped_access_predicate_enabled`]: one scoped access to prove, `width`
/// bytes of the `byte[]` `base` at `addend + c * k + d` (`addend` an
/// invariant `long`, or [`NO_NODE`]), the index part in `int` arithmetic when
/// `wraps`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ScopedPlan {
    node: NodeId,
    sma: NodeId,
    base: NodeId,
    addend: NodeId,
    c: i64,
    d: i64,
    wraps: bool,
    width: u8,
    /// [`range_guard_predicate_enabled`]: the index part is not affine but
    /// lies in this exact range (`c`, `d` and `wraps` unused).
    range: Option<(i64, i64)>,
    /// As [`AffineGuard::iv_slot`].
    iv_slot: usize,
    /// A receiver read IN the body by a direct `getstatic` (`sma` is then
    /// [`NO_NODE`]): the pre-header reads the same static once more and tests
    /// that copy, which the body's read equals because nothing in the body
    /// can write a static ([`body_writes_no_static`]).
    sma_reread: NodeId,
}

/// Can nothing in `body` change a static field? Its writes are field and
/// array stores and raw scoped accesses only (no `putstatic`, call, monitor
/// or other opaque write).
fn body_writes_no_static(graph: &Graph, body: &FxHashSet<NodeId>) -> bool {
    body.iter().all(|&id| {
        graph.node_opt(id).is_none_or(|n| {
            matches!(
                n.op,
                Op::Phi | Op::Dead | Op::OsrMemory | Op::Store(_) | Op::ArrayStore(_)
            ) || n.op.is_control()
                || !crate::ir::memory_shape_of(&n.op).may_write()
                // A direct `getstatic` reads (its `Opaque` effect is the
                // helper route's, which a direct read never takes).
                || super::is_direct_static_read(graph, n)
                || (matches!(n.op, Op::Unbox { .. })
                    && graph.scoped_raw_alias
                    && crate::ir::scoped_raw_effect(n).is_some())
        })
    })
}

/// The `long` offset part of a scoped access as `(c, d, wraps)`: a `long`
/// affine expression of `iv` ([`affine`]), or the `I2L` of an `int` one
/// ([`affine_int`], `wraps`).
fn affine_long_widened(
    graph: &Graph,
    v: NodeId,
    iv: NodeId,
    int_on: bool,
) -> Option<(i64, i64, bool)> {
    if let Some((c, d)) = affine_long(graph, v, iv, 0) {
        return Some((c, d, false));
    }
    let n = graph.node_opt(v)?;
    if !int_on || n.op != Op::I2L || n.ty != IrType::Long {
        return None;
    }
    let x = n.input_opt(0)?;
    if x == iv {
        return Some((1, 0, false));
    }
    affine_int(graph, x, iv, 0).map(|(c, d)| (c, d, true))
}

/// The loop's scoped heap accesses [`scoped_access_predicate_enabled`] can
/// prove: reached on every iteration, not proven already, an invariant
/// receiver and `byte[]` base (`Graph::byte_array_nodes`), and an offset
/// `[D +] affine(k)`.
fn scoped_plans(
    graph: &Graph,
    region: NodeId,
    iv: NodeId,
    body: &FxHashSet<NodeId>,
    int_on: bool,
    dbg: bool,
) -> Vec<ScopedPlan> {
    let mut out = Vec::new();
    let mut ids: Vec<NodeId> = body.iter().copied().collect();
    ids.sort_unstable();
    let range_on = range_guard_predicate_enabled();
    let invariant =
        |x: NodeId| x != NO_NODE && !body.contains(&x) && is_loop_invariant(graph, x, region, body);
    let mut statics_unwritten: Option<bool> = None;
    for id in ids {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        let width = match n.op {
            Op::Unbox {
                op:
                    crate::ir::UnboxOp::ScopedGet { width, .. }
                    | crate::ir::UnboxOp::ScopedPut { width, .. },
                ..
            } => width,
            _ => continue,
        };
        let refuse = |why: &str| {
            if dbg {
                eprintln!(
                    "[ir-licm] scoped access predicate: n{id} in header {region} not planned: {why}"
                );
            }
        };
        if graph.ffm_proven_heap_accesses.contains(&id) || n.inputs.len() < 6 {
            continue;
        }
        if !anchored_on_every_iteration(graph, n.inputs[0], region) {
            refuse("not reached on every iteration");
            continue;
        }
        let (sma, base, off) = (n.inputs[2], n.inputs[4], n.inputs[5]);
        // A trusted `static final` receiver the VM filed non-null
        // (`SCOPED_MEMORY_ACCESS`) is non-null wherever it is read: no test,
        // whether or not its read left the loop.
        let sma_static = graph.node_opt(sma).is_some_and(|sn| match sn.op {
            Op::LoadStatic {
                class_id,
                field_index,
                type_tag: b'L',
                is_volatile: false,
            } => graph
                .static_final_non_null
                .contains(&(class_id, field_index)),
            _ => false,
        });
        let sma = if sma_static { NO_NODE } else { sma };
        // A direct `getstatic` in the body, in a body that writes no static:
        // re-read and tested in the pre-header.
        let direct_in_body = sma != NO_NODE
            && !invariant(sma)
            && graph
                .node_opt(sma)
                .is_some_and(|sn| super::is_direct_static_read(graph, sn));
        let sma_reread = if direct_in_body
            && *statics_unwritten.get_or_insert_with(|| body_writes_no_static(graph, body))
        {
            sma
        } else {
            NO_NODE
        };
        let sma = if sma_reread != NO_NODE { NO_NODE } else { sma };
        if (sma != NO_NODE && !invariant(sma)) || !invariant(base) {
            if dbg {
                let op = |x: NodeId| graph.node_opt(x).map(|n| format!("{:?}", n.op));
                refuse(&format!(
                    "receiver or base varies in the loop (sma n{sma} {:?} in body {}, base n{base} {:?} in body {})",
                    op(sma),
                    body.contains(&sma),
                    op(base),
                    body.contains(&base)
                ));
            }
            continue;
        }
        if !graph.byte_array_nodes.contains(&base)
            || graph.node_opt(base).is_none_or(crate::ir::is_null_const)
        {
            refuse("base not a known byte[]");
            continue;
        }
        // The index part: affine in `k`, or (`range_guard_predicate_enabled`)
        // of a statically bounded range.
        let index = |x: NodeId| -> Option<((i64, i64, bool), Option<(i64, i64)>)> {
            if let Some(f) = affine_long_widened(graph, x, iv, int_on) {
                return Some((f, None));
            }
            range_on
                .then(|| static_long_range(graph, x))
                .flatten()
                .map(|r| ((0, 0, false), Some(r)))
        };
        let shaped = match index(off) {
            Some(f) => Some((NO_NODE, f)),
            None => match graph.node_opt(off) {
                Some(on) if on.op == Op::Add && on.ty == IrType::Long && on.inputs.len() == 2 => {
                    let (a, b) = (on.inputs[0], on.inputs[1]);
                    let long_inv = |x: NodeId| {
                        invariant(x) && graph.node_opt(x).is_some_and(|xn| xn.ty == IrType::Long)
                    };
                    if long_inv(a) {
                        index(b).map(|f| (a, f))
                    } else if long_inv(b) {
                        index(a).map(|f| (b, f))
                    } else {
                        None
                    }
                }
                _ => None,
            },
        };
        if shaped.is_none() {
            refuse("offset neither affine nor ranged");
        }
        if let Some((addend, ((c, d, wraps), range))) = shaped {
            out.push(ScopedPlan {
                node: id,
                sma,
                base,
                addend,
                c,
                d,
                wraps,
                width,
                range,
                iv_slot: 0,
                sma_reread,
            });
        }
    }
    out
}

/// One body guard to replace: `A op X`, `A = c * k + d`, computed in `int`
/// arithmetic when `wraps`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct AffineGuard {
    guard: NodeId,
    op: CmpOp,
    c: i64,
    d: i64,
    x: NodeId,
    wraps: bool,
    /// Which induction `A` is affine in: `0` the counted `k`, `i + 1` the
    /// loop's `i`-th derived induction ([`IvShape`]).
    iv_slot: usize,
    /// An invariant `int` added to `A` (`inv - A'` parsed as `-A' + inv`,
    /// `nextPutIndex`'s `limit - p`), or [`NO_NODE`]. Set only with `wraps`.
    addend: NodeId,
}

/// [`affine_form`], or `inv - B` with `inv` an invariant `int` (`invariant`)
/// and `B` affine: `(-c, -d, true, inv)` -- the `int` subtraction wraps like
/// the other ops, so [`affine_int_enabled`]'s fits check covers it.
fn affine_with_addend(
    graph: &Graph,
    v: NodeId,
    iv: NodeId,
    int_on: bool,
    invariant: &dyn Fn(NodeId) -> bool,
) -> Option<(i64, i64, bool, NodeId)> {
    if let Some((c, d, w)) = affine_form(graph, v, iv, int_on) {
        return Some((c, d, w, NO_NODE));
    }
    if !int_on {
        return None;
    }
    let n = graph.node_opt(v)?;
    if n.op != Op::Sub || n.ty != IrType::Int {
        return None;
    }
    let &[a, b] = n.inputs.as_slice() else {
        return None;
    };
    if !invariant(a) || graph.node_opt(a).is_none_or(|an| an.ty != IrType::Int) {
        return None;
    }
    let (c, d, _) = affine_form(graph, b, iv, int_on)?;
    bounded(c.checked_neg()?, d.checked_neg()?).map(|(c, d)| (c, d, true, a))
}

/// `CRATONVM_JIT_IR_AFFINE_DERIVED_IV` (the r23 L8 heap-buffer close; perf,
/// **default ON**, `=0` kills): the affine and scoped-access predicates also
/// take a DERIVED induction of the counted loop as the leaf -- a header `int`
/// φ `v = φ(v0, v + s)` (`s` a nonzero constant, `v0` invariant) updated on
/// the loop's single back edge, so `v == v0 + s * (k - init)` on every
/// iteration: a relative buffer accessor's `position` once the heap-buffer
/// forward carries it in a register (`nextPutIndex`'s `limit - p >= 4`, the
/// access at `address + p`).
///
/// Its range is `[v0, v0 + s * (n - 1 - init)]` (ordered by the sign of
/// `s`), computed in `long`; the pre-header also checks that both ends fit in
/// an `int`, so the body's wrapping `v + s` never wrapped and every `v` is the
/// exact value the range bounds. Read per compile.
pub(super) fn affine_derived_iv_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_AFFINE_DERIVED_IV")
}

/// An induction the planner keys guards on ([`AffineGuard::iv_slot`]).
#[derive(Clone, Copy, Debug)]
enum IvShape {
    /// The counted `k`: `[init, n - 1]`.
    Counted,
    /// [`affine_derived_iv_enabled`]: `v = φ(v0, v + s)`.
    Derived { v0: NodeId, s: i64 },
}

/// The derived inductions of the counted loop at `region` whose counted
/// induction is `iv` ([`affine_derived_iv_enabled`]), as `(φ, v0, s)`.
fn derived_ivs(
    graph: &Graph,
    region: NodeId,
    iv: NodeId,
    entry_slot: usize,
    back_slot: usize,
    body: &FxHashSet<NodeId>,
) -> Vec<(NodeId, NodeId, i64)> {
    let mut out = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let id = id as NodeId;
        if id == iv
            || n.op != Op::Phi
            || n.ty != IrType::Int
            || n.inputs.len() != 3
            || n.inputs.first() != Some(&region)
        {
            continue;
        }
        let (Some(v0), Some(back)) = (n.input_opt(entry_slot), n.input_opt(back_slot)) else {
            continue;
        };
        if v0 == NO_NODE || body.contains(&v0) || !is_loop_invariant(graph, v0, region, body) {
            continue;
        }
        let Some(bn) = graph.node_opt(back) else {
            continue;
        };
        let &[a, b] = bn.inputs.as_slice() else {
            continue;
        };
        let k = |x: NodeId| const_of(graph, x, IrType::Int);
        let s = match bn.op {
            Op::Add if a == id => k(b),
            Op::Add if b == id => k(a),
            Op::Sub if a == id => k(b).and_then(|s| s.checked_neg()),
            _ => None,
        };
        if let Some(s) = s.filter(|&s| s != 0 && s.abs() <= MAX_SCALE) {
            out.push((id, v0, s));
        }
    }
    out
}

/// The counted loop at `region`: `(iv, init, n)` for `iv = φ(init, iv + 1)`
/// tested `iv < n` on the stay edge, `n` an invariant `int`.
pub(super) fn counted_loop(
    graph: &Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    body: &FxHashSet<NodeId>,
) -> Option<(NodeId, NodeId, NodeId)> {
    let [back] = back_ctrls else {
        return None;
    };
    let rn = graph.node_opt(region)?;
    if rn.inputs.len() != 2 {
        return None;
    }
    let entry_slot = rn.inputs.iter().position(|&p| p == entry_pred)? + 1;
    let back_slot = rn.inputs.iter().position(|&p| p == *back)? + 1;
    let mut test: Option<NodeId> = None;
    for id in anchored_at(graph, region) {
        let n = graph.node_opt(id)?;
        if n.op == Op::Dead || !n.op.is_control() {
            continue;
        }
        if n.op != Op::If || test.is_some() {
            return None;
        }
        test = Some(id);
    }
    let test = test?;
    let cond = graph.node_opt(graph.node_opt(test)?.input_opt(1)?)?;
    let Op::Cmp(op) = cond.op else {
        return None;
    };
    let &[lhs, rhs] = cond.inputs.as_slice() else {
        return None;
    };
    let p0 = proj_user(graph, test, 0);
    let p1 = proj_user(graph, test, 1);
    let stays = |p: NodeId| p != NO_NODE && body.contains(&p);
    let holds = match (stays(p0), stays(p1)) {
        (true, false) => true,
        (false, true) => false,
        _ => return None,
    };
    let op = if holds { op } else { op.negate() };
    let (iv, n) = match op {
        CmpOp::Lt => (lhs, rhs),
        CmpOp::Gt => (rhs, lhs),
        _ => return None,
    };
    let ivn = graph.node_opt(iv)?;
    if ivn.op != Op::Phi || ivn.ty != IrType::Int || ivn.inputs.first() != Some(&region) || ivn.inputs.len() != 3 {
        return None;
    }
    let init = ivn.input_opt(entry_slot)?;
    let next = graph.node_opt(ivn.input_opt(back_slot)?)?;
    let &[a, b] = next.inputs.as_slice() else {
        return None;
    };
    let step = match (next.op == Op::Add, a == iv, b == iv) {
        (true, true, false) => b,
        (true, false, true) => a,
        _ => return None,
    };
    if const_of(graph, step, IrType::Int) != Some(1) {
        return None;
    }
    let invariant = |x: NodeId| {
        x != NO_NODE
            && !body.contains(&x)
            && is_loop_invariant(graph, x, region, body)
            && graph.node_opt(x).is_some_and(|xn| xn.ty == IrType::Int)
    };
    (invariant(n) && invariant(init)).then_some((iv, init, n))
}

/// The body guards of the loop the module notes describe; with
/// `invariant_out`, also every guard reached on every iteration whose
/// condition is loop-invariant (not a constant, not a class test) -- the
/// length predicate's guard move, for [`plant_affine_guard_predicates_after_forward`].
#[allow(clippy::too_many_arguments)]
fn affine_guards(
    graph: &Graph,
    region: NodeId,
    iv: NodeId,
    body: &FxHashSet<NodeId>,
    anchors: &FxHashSet<NodeId>,
    division_bcis: &FxHashSet<usize>,
    int_on: bool,
    mut invariant_out: Option<&mut Vec<NodeId>>,
) -> Vec<AffineGuard> {
    let range_on = range_guard_predicate_enabled();
    let mut out = Vec::new();
    let mut ids: Vec<NodeId> = body.iter().copied().collect();
    ids.sort_unstable();
    for g in ids {
        let Some(n) = graph.node_opt(g) else {
            continue;
        };
        let Op::Guard { bci } = n.op else {
            continue;
        };
        if n.inputs.len() != 2
            || anchors.contains(&g)
            || division_bcis.contains(&bci)
            || !anchored_on_every_iteration(graph, n.inputs[0], region)
        {
            continue;
        }
        let cond = n.inputs[1];
        let Some(cn) = graph.node_opt(cond) else {
            continue;
        };
        if let Some(inv) = invariant_out.as_deref_mut() {
            if !body.contains(&cond)
                && !matches!(cn.op, Op::Const(_) | Op::ExactClassIs { .. })
                && is_loop_invariant(graph, cond, region, body)
            {
                inv.push(g);
                continue;
            }
        }
        let Op::Cmp(op) = cn.op else {
            continue;
        };
        if matches!(op, CmpOp::Eq | CmpOp::Ne) {
            continue;
        }
        let &[a, b] = cn.inputs.as_slice() else {
            continue;
        };
        let invariant = |x: NodeId| {
            x != NO_NODE
                && !body.contains(&x)
                && is_loop_invariant(graph, x, region, body)
                && graph
                    .node_opt(x)
                    .is_some_and(|xn| matches!(xn.ty, IrType::Int | IrType::Long))
        };
        let inv_int = |x: NodeId| {
            x != NO_NODE && !body.contains(&x) && is_loop_invariant(graph, x, region, body)
        };
        let shaped = match (
            affine_with_addend(graph, a, iv, int_on, &inv_int),
            affine_with_addend(graph, b, iv, int_on, &inv_int),
        ) {
            (Some((c, d, w, add)), None) if invariant(b) => Some((op, c, d, b, w, add)),
            (None, Some((c, d, w, add))) if invariant(a) => Some((op.swapped(), c, d, a, w, add)),
            _ => None,
        };
        // `range_guard_predicate_enabled`: a ranged side, planted at the
        // extreme the compare needs (`c = 0`: the constant `d`).
        let shaped = shaped.or_else(|| {
            if !range_on {
                return None;
            }
            let ranged = |x: NodeId| {
                graph
                    .node_opt(x)
                    .filter(|xn| !matches!(xn.op, Op::Const(_)))
                    .and_then(|_| static_int_range(graph, x, 0))
            };
            let (op, (lo, hi), x) = match (ranged(a), ranged(b)) {
                (Some(r), _) if invariant(b) => (op, r, b),
                (_, Some(r)) if invariant(a) => (op.swapped(), r, a),
                _ => return None,
            };
            let d = if matches!(op, CmpOp::Lt | CmpOp::Le) {
                hi
            } else {
                lo
            };
            Some((op, 0, d, x, false, NO_NODE))
        });
        if let Some((op, c, d, x, wraps, addend)) = shaped {
            out.push(AffineGuard {
                guard: g,
                op,
                c,
                d,
                x,
                wraps,
                iv_slot: 0,
                addend,
            });
        }
    }
    out
}

/// Plant the predicates the module notes describe, unless `despec` withdrew
/// the `Range` kind at a loop's header. Returns the number of body guards
/// replaced.
pub(super) fn plant_affine_guard_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> usize {
    plant_with(graph, despec, method_key, None, false)
}

/// [`predicates_after_field_forward_enabled`]: the planter once more, for the
/// loops at `headers` only (the ones the heap-buffer field forward changed),
/// moving their invariant body guards too -- each to the same pre-header
/// guard form (`Guard{header}(cond)`, the loop-entry frame, `Range`'s
/// withdrawal), which the length predicate gives them on a flat header.
/// Returns the number of body guards replaced.
pub(crate) fn plant_affine_guard_predicates_after_forward(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    headers: &FxHashSet<NodeId>,
) -> usize {
    plant_with(graph, despec, method_key, Some(headers), true)
}

fn plant_with(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    only: Option<&FxHashSet<NodeId>>,
    with_invariant: bool,
) -> usize {
    if graph.safepoints.is_empty() || graph.osr_compile || graph.optimize_osr_route {
        return 0;
    }
    let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LICM");
    let int_on = affine_int_enabled();
    let chain_on = affine_chain_enabled();
    let scoped_on = scoped_access_predicate_enabled();
    let anchors: FxHashSet<NodeId> = crate::ir::exception_edge_anchors(graph).into_iter().collect();
    let division_bcis: FxHashSet<usize> = graph
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Div | Op::Rem))
        .filter_map(|n| n.bytecode_pc)
        .collect();
    let mut replaced = 0usize;
    let mut planted = 0usize;
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if planted >= MAX_AFFINE_GUARDS {
            break;
        }
        if entry_pred == NO_NODE
            || only.is_some_and(|h| !h.contains(&region))
            || !matches!(graph.node_opt(region).map(|n| &n.op), Some(Op::Region | Op::Merge))
        {
            continue;
        }
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        if body.contains(&entry_pred) {
            continue;
        }
        let Some((iv, init, n)) = counted_loop(graph, region, entry_pred, &back_ctrls, &body)
        else {
            if dbg && only.is_some() {
                eprintln!("[ir-licm] affine guard predicate (after forward): header {region} not a counted loop");
            }
            continue;
        };
        let mut invariant: Vec<NodeId> = Vec::new();
        let mut guards = affine_guards(
            graph,
            region,
            iv,
            &body,
            &anchors,
            &division_bcis,
            int_on,
            with_invariant.then_some(&mut invariant),
        );
        let mut scoped = if scoped_on {
            scoped_plans(graph, region, iv, &body, int_on, dbg)
        } else {
            Vec::new()
        };
        // `affine_derived_iv_enabled`: the same, keyed on each derived
        // induction, for what the counted one did not take.
        let mut shapes = vec![IvShape::Counted];
        let slots = graph.node_opt(region).and_then(|r| {
            let e = r.inputs.iter().position(|&p| p == entry_pred)? + 1;
            let [back] = back_ctrls.as_slice() else {
                return None;
            };
            let b = r.inputs.iter().position(|p| p == back)? + 1;
            Some((e, b))
        });
        if let (true, Some((e, b))) = (affine_derived_iv_enabled(), slots) {
            for (phi, v0, s) in derived_ivs(graph, region, iv, e, b, &body) {
                let slot = shapes.len();
                let mut more = affine_guards(
                    graph,
                    region,
                    phi,
                    &body,
                    &anchors,
                    &division_bcis,
                    int_on,
                    None,
                );
                more.retain(|g| !guards.iter().any(|h| h.guard == g.guard));
                let mut more_scoped = if scoped_on {
                    scoped_plans(graph, region, phi, &body, int_on, false)
                } else {
                    Vec::new()
                };
                more_scoped.retain(|p| !scoped.iter().any(|q| q.node == p.node));
                if more.is_empty() && more_scoped.is_empty() {
                    continue;
                }
                for g in &mut more {
                    g.iv_slot = slot;
                }
                for p in &mut more_scoped {
                    p.iv_slot = slot;
                }
                guards.extend(more);
                scoped.extend(more_scoped);
                shapes.push(IvShape::Derived { v0, s });
            }
        }
        if dbg && only.is_some() {
            eprintln!(
                "[ir-licm] affine guard predicate (after forward): header {region}: {} affine, {} \
                 invariant, {} scoped, {} induction shape(s)",
                guards.len(),
                invariant.len(),
                scoped.len(),
                shapes.len()
            );
        }
        if guards.is_empty() && invariant.is_empty() && scoped.is_empty() {
            continue;
        }
        let entry_mem = loop_entry_memory(graph, region, entry_pred);
        let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
            continue;
        };
        if preheader_traps_block_entry_predicate(graph, entry_pred, header_bci) {
            if dbg {
                let culprits: Vec<String> = anchored_at(graph, entry_pred)
                    .into_iter()
                    .filter_map(|id| graph.node_opt(id).map(|n| (id, n)))
                    .filter(|(_, n)| !n.op.is_control() && crate::ir::may_raise(&n.op))
                    .map(|(id, n)| format!("n{id} {:?} frame={:?}", n.op, n.frame_snapshot))
                    .collect();
                eprintln!(
                    "[ir-licm] affine guard predicate: header {region} pre-header may trap ({})",
                    culprits.join("; ")
                );
            }
            continue;
        }
        let Some(entry_slot) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
            .map(|p| p + 1)
        else {
            continue;
        };
        // The r23 L8 heap-buffer close (`affine_chain_enabled`): a loop
        // spliced from a callee takes the loop-entry chain, or the one an
        // earlier chain predicate of this loop already named.
        let spliced = chain_on && header_is_spliced(graph, region);
        let reused = if spliced {
            chain_predicate_guard_frame(graph, entry_pred, header_bci)
                .or_else(|| preheader_entry_chain_frame(graph, entry_pred, header_bci))
        } else {
            None
        };
        let entry_frame = match reused {
            Some(si) => graph
                .safepoints
                .get(si as usize)
                .map(|f| (header_bci, f.clone())),
            None if spliced => loop_entry_chain_frame(graph, region, entry_slot, &body),
            None => loop_entry_frame(graph, region, entry_slot, &body),
        };
        let Some((header_bci, frame)) = entry_frame else {
            if dbg {
                eprintln!(
                    "[ir-licm] affine guard predicate: header {region}{} has no loop-entry frame",
                    if spliced { " (spliced)" } else { "" }
                );
            }
            continue;
        };
        let Ok(bci32) = u32::try_from(header_bci) else {
            continue;
        };
        let withdrawn = match frame.splice_scope.as_deref() {
            Some(chain) => match (chain.scopes.first(), chain.scopes.last()) {
                (Some(inner), Some(outer)) => crate::deopt::chain_predicate_withdrawn(
                    despec,
                    method_key,
                    outer.bci,
                    &inner.method_key,
                    inner.bci,
                    crate::deopt::PredicateKind::Range,
                ),
                _ => true,
            },
            None => crate::deopt::predicate_withdrawn(
                despec,
                method_key,
                bci32,
                crate::deopt::PredicateKind::Range,
            ),
        };
        if withdrawn {
            if dbg {
                eprintln!(
                    "[ir-licm] affine guard predicate: header {region} bci {header_bci} \
                     de-specialised"
                );
            }
            continue;
        }
        let frame = match reused {
            Some(si) => EntryFrame::Named(si),
            None => EntryFrame::New(frame),
        };
        let n_moved = install(
            graph,
            entry_pred,
            header_bci,
            frame,
            spliced,
            init,
            n,
            &shapes,
            &guards,
            &invariant,
            &scoped,
            entry_mem,
            MAX_AFFINE_GUARDS - planted,
        );
        if dbg && n_moved.0 > 0 {
            eprintln!(
                "[ir-licm] affine guard predicate: header {region} bci {header_bci}{}: {} body \
                 guard(s) -> {} pre-header guard(s)",
                if spliced { " (spliced)" } else { "" },
                n_moved.0,
                n_moved.1
            );
        }
        replaced += n_moved.0;
        planted += n_moved.1;
    }
    replaced
}

/// The snapshot index a pre-header node of the spliced loop at combined pc
/// `header_bci` already names as its frame when it is the loop-entry chain
/// there: a guard-usable chain snapshot at exactly that pc, named by a node
/// anchored at `entry_pred` -- the heap-buffer field forward's pre-header
/// reads and null guards (`loop_field_forward::preheader_snapshot`, the
/// header's chain with each header φ at its entry value). After the forward
/// the header's pc carries two chain snapshots, which
/// `loop_entry_chain_frame`'s one-chain rule refuses.
fn preheader_entry_chain_frame(
    graph: &Graph,
    entry_pred: NodeId,
    header_bci: usize,
) -> Option<u32> {
    anchored_at(graph, entry_pred).into_iter().find_map(|id| {
        let si = graph.node_opt(id)?.frame_snapshot?;
        let sp = graph.safepoints.get(si as usize)?;
        (sp.bci == header_bci && sp.is_guard_splice_state()).then_some(si)
    })
}

/// The loop-entry frame a loop's pre-header guards name: a new snapshot, or
/// (a spliced loop) the one an earlier chain predicate already named.
enum EntryFrame {
    New(crate::ir::SafepointSnapshot),
    Named(u32),
}

/// `c * at + d` in `long`, for the pre-header.
fn affine_at(graph: &mut Graph, at: NodeId, c: i64, d: i64, pc: Option<usize>) -> NodeId {
    let mut a = at;
    if c != 1 {
        let c = graph.add(Op::Const(c), IrType::Long, vec![], None);
        a = graph.add(Op::Mul, IrType::Long, vec![a, c], pc);
    }
    if d != 0 {
        let d = graph.add(Op::Const(d), IrType::Long, vec![], None);
        a = graph.add(Op::Add, IrType::Long, vec![a, d], pc);
    }
    a
}

/// Plant one loop's pre-header guards and retire its body guards (`guards`
/// at the range's extreme, `invariant` as they are). A `spliced` loop's
/// guards are listed in `Graph::chain_predicate_guards`. Returns `(body
/// guards replaced, pre-header guards planted)`.
#[allow(clippy::too_many_arguments)]
fn install(
    graph: &mut Graph,
    entry_pred: NodeId,
    header_bci: usize,
    frame: EntryFrame,
    spliced: bool,
    init: NodeId,
    n: NodeId,
    shapes: &[IvShape],
    guards: &[AffineGuard],
    invariant: &[NodeId],
    scoped: &[ScopedPlan],
    entry_mem: Option<NodeId>,
    budget: usize,
) -> (usize, usize) {
    let pc = Some(header_bci);
    let (si, pushed) = match frame {
        EntryFrame::Named(si) => (si, false),
        EntryFrame::New(frame) => {
            graph.push_safepoint(frame);
            // Cast: a snapshot index; `push_safepoint` just grew the list.
            ((graph.safepoints.len() - 1) as u32, true)
        }
    };
    let init_l = graph.add(Op::I2L, IrType::Long, vec![init], pc);
    let n_l = graph.add(Op::I2L, IrType::Long, vec![n], pc);
    let one = graph.add(Op::Const(1), IrType::Long, vec![], None);
    let last_l = graph.add(Op::Sub, IrType::Long, vec![n_l, one], pc);
    let zero_trip = graph.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![init_l, n_l], pc);
    let lconst = |graph: &mut Graph, v: i64| graph.add(Op::Const(v), IrType::Long, vec![], None);
    // Each induction shape's `(min, max)` over the loop's iterations, in
    // `long`, and (a derived one) the check that both fit in an `int`.
    let mut extremes: Vec<(NodeId, NodeId, Option<NodeId>)> = Vec::with_capacity(shapes.len());
    for shape in shapes {
        match *shape {
            IvShape::Counted => extremes.push((init_l, last_l, None)),
            IvShape::Derived { v0, s } => {
                let first = graph.add(Op::I2L, IrType::Long, vec![v0], pc);
                let trips = graph.add(Op::Sub, IrType::Long, vec![last_l, init_l], pc);
                let sc = lconst(graph, s);
                let span = graph.add(Op::Mul, IrType::Long, vec![trips, sc], pc);
                let last = graph.add(Op::Add, IrType::Long, vec![first, span], pc);
                let (lo, hi) = if s > 0 { (first, last) } else { (last, first) };
                let min = lconst(graph, i64::from(i32::MIN));
                let max = lconst(graph, i64::from(i32::MAX));
                let lo_ok = graph.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![lo, min], pc);
                let hi_ok = graph.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![hi, max], pc);
                let fits = graph.add(Op::And, IrType::Int, vec![lo_ok, hi_ok], pc);
                extremes.push((lo, hi, Some(fits)));
            }
        }
    }
    // `(op, c, d, x, wraps, iv_slot, addend)` -> the pre-header guard planted
    // for it; an invariant guard's condition -> its copy.
    let mut planted: Vec<((CmpOp, i64, i64, NodeId, bool, usize, NodeId), NodeId)> = Vec::new();
    // `A` at `at`, with the guard's invariant addend.
    let a_at = |graph: &mut Graph, at: NodeId, g: &AffineGuard| -> NodeId {
        let a = affine_at(graph, at, g.c, g.d, pc);
        if g.addend == NO_NODE {
            return a;
        }
        let add = graph.add(Op::I2L, IrType::Long, vec![g.addend], pc);
        graph.add(Op::Add, IrType::Long, vec![a, add], pc)
    };
    let mut moved: Vec<(NodeId, NodeId)> = Vec::new();
    let mut replaced = 0usize;
    let new_guard = |graph: &mut Graph, cond: NodeId| -> Option<NodeId> {
        let new = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, cond],
            pc,
        );
        if !graph.set_node_frame_snapshot(new, si) {
            graph.kill(new);
            return None;
        }
        if spliced {
            graph
                .chain_predicate_guards
                .push((new, crate::deopt::PredicateKind::Range as u8));
        }
        Some(new)
    };
    for g in guards {
        let key = (g.op, g.c, g.d, g.x, g.wraps, g.iv_slot, g.addend);
        let Some(&(v_min, v_max, v_fits)) = extremes.get(g.iv_slot) else {
            continue;
        };
        let new = match planted.iter().find(|(k, _)| *k == key) {
            Some(&(_, new)) => new,
            None => {
                if planted.len() + moved.len() >= budget {
                    break;
                }
                // The extreme `k*`: the minimum of `A` for `>`/`>=`, the
                // maximum for `<`/`<=`, by the sign of `c`.
                let wants_min = matches!(g.op, CmpOp::Gt | CmpOp::Ge);
                let at = if wants_min == (g.c > 0) { v_min } else { v_max };
                let a = a_at(graph, at, g);
                let x = match graph.node_opt(g.x).map(|xn| xn.ty) {
                    Some(IrType::Long) => g.x,
                    _ => graph.add(Op::I2L, IrType::Long, vec![g.x], pc),
                };
                let mut holds = graph.add(Op::Cmp(g.op), IrType::Int, vec![a, x], pc);
                if let Some(fits) = v_fits {
                    // A derived induction: every `v` the body computes is the
                    // exact one (`affine_derived_iv_enabled`).
                    holds = graph.add(Op::And, IrType::Int, vec![fits, holds], pc);
                }
                if g.wraps {
                    // `affine_int_enabled`: the `int` body value is the exact
                    // one iff `A` fits at both extremes (it is monotone).
                    let (lo_at, hi_at) = if g.c > 0 {
                        (v_min, v_max)
                    } else {
                        (v_max, v_min)
                    };
                    let lo = a_at(graph, lo_at, g);
                    let hi = a_at(graph, hi_at, g);
                    let min = graph.add(Op::Const(i64::from(i32::MIN)), IrType::Long, vec![], None);
                    let max = graph.add(Op::Const(i64::from(i32::MAX)), IrType::Long, vec![], None);
                    let lo_ok = graph.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![lo, min], pc);
                    let hi_ok = graph.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![hi, max], pc);
                    let fits = graph.add(Op::And, IrType::Int, vec![lo_ok, hi_ok], pc);
                    holds = graph.add(Op::And, IrType::Int, vec![fits, holds], pc);
                }
                let cond = graph.add(Op::Or, IrType::Int, vec![zero_trip, holds], pc);
                let Some(new) = new_guard(graph, cond) else {
                    continue;
                };
                planted.push((key, new));
                new
            }
        };
        // Tokens the body guard licensed now name the pre-header guard.
        graph.replace_all_uses(g.guard, new);
        graph.kill(g.guard);
        replaced += 1;
    }
    for &g in invariant {
        let Some(cond) = graph.node_opt(g).and_then(|n| n.inputs.get(1).copied()) else {
            continue;
        };
        let new = match moved.iter().find(|&&(c, _)| c == cond) {
            Some(&(_, new)) => new,
            None => {
                if planted.len() + moved.len() >= budget {
                    break;
                }
                let Some(new) = new_guard(graph, cond) else {
                    continue;
                };
                moved.push((cond, new));
                new
            }
        };
        graph.replace_all_uses(g, new);
        graph.kill(g);
        replaced += 1;
    }
    // `scoped_access_predicate_enabled`: per `(sma, base)` the non-null guard
    // and the length, per distinct offset shape the range guard; then the
    // access is filed as proven.
    let mut nonnull: Vec<((NodeId, NodeId, NodeId), NodeId)> = Vec::new();
    let mut ranges: Vec<(ScopedPlan, NodeId)> = Vec::new();
    for p in scoped {
        // An array's length never changes, so any pre-header memory state
        // orders the `ArrayLength`: the loop's entry state, or (a header the
        // field forward reshaped) the one the base's own pre-header read took.
        let base_mem = graph
            .node_opt(p.base)
            .filter(|b| matches!(b.op, Op::Load(_)) && b.input_opt(0) == Some(entry_pred))
            .and_then(|b| b.input_opt(1));
        let Some(mem) = entry_mem.or(base_mem) else {
            continue;
        };
        let key = ScopedPlan {
            node: NO_NODE,
            ..*p
        };
        if ranges.iter().any(|(k, _)| *k == key) {
            graph.ffm_proven_heap_accesses.insert(p.node);
            replaced += 1;
            continue;
        }
        if planted.len() + moved.len() + nonnull.len() + ranges.len() + 2 > budget {
            break;
        }
        let len = match nonnull
            .iter()
            .find(|(k, _)| *k == (p.sma, p.base, p.sma_reread))
        {
            Some(&(_, len)) => len,
            None => {
                let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
                let base_ok = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![p.base, null], pc);
                // The receiver as the pre-header sees it: itself, a fresh
                // read of the body's direct `getstatic` (`sma_reread`), or
                // none (a non-null trusted `static final`).
                let sma = if p.sma_reread != NO_NODE {
                    match graph.node_opt(p.sma_reread).map(|n| n.op.clone()) {
                        Some(op) => graph.add(op, IrType::Ref, vec![entry_pred, mem], pc),
                        None => continue,
                    }
                } else {
                    p.sma
                };
                let both = if sma == NO_NODE {
                    base_ok
                } else {
                    let sma_ok = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![sma, null], pc);
                    graph.add(Op::And, IrType::Int, vec![sma_ok, base_ok], pc)
                };
                let Some(guard) = new_guard(graph, both) else {
                    continue;
                };
                // A null base deopts to the loop entry here too, whichever of
                // the two runs first: the same frame, the same resume.
                let len = graph.add(
                    Op::ArrayLength,
                    IrType::Int,
                    vec![entry_pred, mem, p.base],
                    pc,
                );
                if !graph.set_node_frame_snapshot(len, si) {
                    graph.kill(len);
                    graph.kill(guard);
                    continue;
                }
                nonnull.push(((p.sma, p.base, p.sma_reread), len));
                len
            }
        };
        // In `long`: `D` bounded, then both extremes of the offset.
        let Some(&(v_min, v_max, v_fits)) = extremes.get(p.iv_slot) else {
            continue;
        };
        let (mut lo, mut hi) = match p.range {
            Some((rlo, rhi)) => (lconst(graph, rlo), lconst(graph, rhi)),
            None => {
                let (lo_at, hi_at) = if p.c > 0 {
                    (v_min, v_max)
                } else {
                    (v_max, v_min)
                };
                (
                    affine_at(graph, lo_at, p.c, p.d, pc),
                    affine_at(graph, hi_at, p.c, p.d, pc),
                )
            }
        };
        let mut ok: Vec<NodeId> = Vec::new();
        if let (None, Some(fits)) = (p.range, v_fits) {
            ok.push(fits);
        }
        if p.wraps {
            let min = lconst(graph, i64::from(i32::MIN));
            let max = lconst(graph, i64::from(i32::MAX));
            ok.push(graph.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![lo, min], pc));
            ok.push(graph.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![hi, max], pc));
        }
        if p.addend != NO_NODE {
            let zero = lconst(graph, 0);
            let cap = lconst(graph, 1i64 << 40);
            ok.push(graph.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![p.addend, zero], pc));
            ok.push(graph.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![p.addend, cap], pc));
            lo = graph.add(Op::Add, IrType::Long, vec![lo, p.addend], pc);
            hi = graph.add(Op::Add, IrType::Long, vec![hi, p.addend], pc);
        }
        // Cast: the payload offset, a small constant (16).
        let data = cratonvm_types::ARRAY_DATA_OFFSET as i64;
        let first = lconst(graph, data);
        ok.push(graph.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![lo, first], pc));
        let w = lconst(graph, i64::from(p.width));
        let end = graph.add(Op::Add, IrType::Long, vec![hi, w], pc);
        let len_l = graph.add(Op::I2L, IrType::Long, vec![len], pc);
        let limit = graph.add(Op::Add, IrType::Long, vec![len_l, first], pc);
        ok.push(graph.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![end, limit], pc));
        let mut all = ok[0];
        for &c in &ok[1..] {
            all = graph.add(Op::And, IrType::Int, vec![all, c], pc);
        }
        let cond = graph.add(Op::Or, IrType::Int, vec![zero_trip, all], pc);
        let Some(guard) = new_guard(graph, cond) else {
            continue;
        };
        ranges.push((key, guard));
        graph.ffm_proven_heap_accesses.insert(p.node);
        replaced += 1;
    }
    let planted_scoped = nonnull.len() + ranges.len();
    if pushed && planted.is_empty() && moved.is_empty() && planted_scoped == 0 {
        let keep: Vec<bool> = (0..graph.safepoints.len())
            .map(|i| i != si as usize)
            .collect();
        let _ = graph.retain_safepoints(&keep);
    }
    (replaced, planted.len() + moved.len() + planted_scoped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::SafepointSnapshot;

    const SW: &str = "CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE";

    /// `for (int k = init; k < n; k++) { off = (long) k * 4 + d;
    /// Guard(off >= 0); Guard(off <= last); }` with a header snapshot.
    /// Returns `(graph, region, entry, [g_lo, g_hi], [init, n, last])`.
    fn segment_loop(d: i64, every_iteration: bool) -> (Graph, NodeId, NodeId, [NodeId; 2], [NodeId; 3]) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let init = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let last = g.add(Op::Param(2), IrType::Long, vec![start], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let four = g.add(Op::Const(4), IrType::Long, vec![], None);
        let dl = g.add(Op::Const(d), IrType::Long, vec![], None);
        let lzero = g.add(Op::Const(0), IrType::Long, vec![], None);
        let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(4));
        let k = g.add(Op::Phi, IrType::Int, vec![region, init], None);
        let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![k, n], Some(6));
        let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(7));
        let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
        let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
        // An inner branch, for the not-every-iteration variant.
        let at = if every_iteration {
            stay
        } else {
            let c = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![k, one], None);
            let iff = g.add(Op::If, IrType::Control, vec![stay, c], Some(9));
            g.add(Op::Proj(0), IrType::Control, vec![iff], None)
        };
        let kl = g.add(Op::I2L, IrType::Long, vec![k], Some(10));
        let mul = g.add(Op::Mul, IrType::Long, vec![kl, four], Some(10));
        let off = g.add(Op::Add, IrType::Long, vec![mul, dl], Some(10));
        let lo = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![off, lzero], Some(12));
        let g_lo = g.add(Op::Guard { bci: 12 }, IrType::Void, vec![at, lo], Some(12));
        let hi = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![off, last], Some(12));
        let g_hi = g.add(Op::Guard { bci: 12 }, IrType::Void, vec![at, hi], Some(12));
        let k1 = g.add(Op::Add, IrType::Int, vec![k, one], None);
        let latch = if every_iteration {
            stay
        } else {
            // Both arms of the inner branch rejoin before the back edge.
            let iff = g.nodes[at as usize].inputs[0];
            let other = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
            g.add(Op::Merge, IrType::Control, vec![at, other], Some(20))
        };
        g.nodes[region as usize].inputs.push(latch);
        g.nodes[k as usize].inputs.push(k1);
        g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(30));
        g.push_safepoint(SafepointSnapshot::new(4, vec![init, n, k], vec![], vec![]));
        g.push_safepoint(SafepointSnapshot::new(12, vec![init, n, k], vec![], vec![]));
        (g, region, entry, [g_lo, g_hi], [init, n, last])
    }

    fn plant(g: &mut Graph) -> usize {
        let despec = crate::deopt::DespecRegistry::new();
        plant_affine_guard_predicates(g, &despec, "T.heapGet:(LS;I)I")
    }

    #[test]
    fn r23_w8_l7_affine_parse() {
        let (g, region, _, [g_lo, _], _) = segment_loop(1, true);
        let k = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Phi && n.inputs.first() == Some(&region))
            .unwrap() as NodeId;
        let off = g.nodes[g.nodes[g_lo as usize].inputs[1] as usize].inputs[0];
        assert_eq!(affine(&g, off, k), Some((4, 1)));
        assert_eq!(affine(&g, k, k), Some((1, 0)));
        // A constant alone is not affine in `k`.
        let c = g.nodes[off as usize].inputs[1];
        assert_eq!(affine(&g, c, k), None);
    }

    #[test]
    fn r23_w8_l7_both_segment_bounds_guards_leave_the_loop() {
        let (mut g, _, entry, [g_lo, g_hi], [init, n, last]) = segment_loop(0, true);
        let before = g.safepoints.len();
        assert_eq!(plant(&mut g), 2);
        assert_eq!(g.nodes[g_lo as usize].op, Op::Dead);
        assert_eq!(g.nodes[g_hi as usize].op, Op::Dead);
        assert_eq!(g.safepoints.len(), before + 1, "one loop-entry frame");
        let new: Vec<&crate::ir::Node> = g
            .nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Guard { bci: 4 }) && n.inputs.first() == Some(&entry))
            .collect();
        assert_eq!(new.len(), 2);
        for guard in &new {
            assert!(guard.frame_snapshot.is_some());
            // `init >= n || ...`
            let or = &g.nodes[guard.inputs[1] as usize];
            assert_eq!(or.op, Op::Or);
            let zt = &g.nodes[or.inputs[0] as usize];
            assert_eq!(zt.op, Op::Cmp(CmpOp::Ge));
            assert_eq!(g.nodes[zt.inputs[0] as usize].inputs.as_slice(), &[init]);
            assert_eq!(g.nodes[zt.inputs[1] as usize].inputs.as_slice(), &[n]);
        }
        // The upper check compares `(n - 1) * 4` with `last`; the lower
        // `init * 4` with 0.
        let hi = new
            .iter()
            .map(|gd| &g.nodes[g.nodes[gd.inputs[1] as usize].inputs[1] as usize])
            .find(|c| c.op == Op::Cmp(CmpOp::Le))
            .expect("the upper check");
        assert_eq!(hi.inputs[1], last);
        let mul = &g.nodes[hi.inputs[0] as usize];
        assert_eq!(mul.op, Op::Mul);
        assert_eq!(g.nodes[mul.inputs[0] as usize].op, Op::Sub, "n - 1");
        let lo = new
            .iter()
            .map(|gd| &g.nodes[g.nodes[gd.inputs[1] as usize].inputs[1] as usize])
            .find(|c| c.op == Op::Cmp(CmpOp::Ge))
            .expect("the lower check");
        let mul = &g.nodes[lo.inputs[0] as usize];
        assert_eq!(g.nodes[mul.inputs[0] as usize].op, Op::I2L, "init");
        assert_eq!(g.nodes[mul.inputs[0] as usize].inputs.as_slice(), &[init]);
    }

    /// The n23-w8g loss: after planting, the call site's dead-node pass must
    /// leave no `Cmp` whose only reader was a retired body guard (they were
    /// lowered in the loop as `cmp; setcc; movzx; store`). The new
    /// pre-header guards and their conditions survive it.
    #[test]
    fn r23_w8_l7_no_retired_guard_compare_survives_the_cleanup() {
        let (mut g, _, entry, [g_lo, g_hi], _) = segment_loop(0, true);
        let lo = g.nodes[g_lo as usize].inputs[1];
        let hi = g.nodes[g_hi as usize].inputs[1];
        assert_eq!(plant(&mut g), 2);
        // What `optimize_with` runs when the planter replaced something.
        super::super::eliminate_dead_nodes(&mut g);
        assert_eq!(g.nodes[lo as usize].op, Op::Dead, "the retired lower compare is gone");
        assert_eq!(g.nodes[hi as usize].op, Op::Dead, "the retired upper compare is gone");
        // No live `Cmp` is left without a reader (every retired guard's
        // compare died; the pre-header ones feed their `Or`).
        for (id, n) in g.nodes.iter().enumerate() {
            if !matches!(n.op, Op::Cmp(_)) {
                continue;
            }
            assert!(
                g.nodes.iter().any(|u| u.op != Op::Dead && u.inputs.contains(&(id as NodeId))),
                "n{id}: a live compare with no reader"
            );
        }
        let live_new = g
            .nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Guard { bci: 4 }) && n.inputs.first() == Some(&entry))
            .count();
        assert_eq!(live_new, 2, "the pre-header guards survive");
    }

    #[test]
    fn r23_w8_l7_a_guard_off_the_spine_or_osr_stays() {
        let (mut g, _, _, [g_lo, g_hi], _) = segment_loop(0, false);
        assert_eq!(plant(&mut g), 0);
        assert!(matches!(g.nodes[g_lo as usize].op, Op::Guard { .. }));
        assert!(matches!(g.nodes[g_hi as usize].op, Op::Guard { .. }));
        let (mut g, ..) = segment_loop(0, true);
        g.osr_compile = true;
        assert_eq!(plant(&mut g), 0);
        let (mut g, ..) = segment_loop(0, true);
        g.optimize_osr_route = true;
        assert_eq!(plant(&mut g), 0);
    }

    #[test]
    fn r23_w8_l7_the_kill_switch_and_default() {
        cratonvm_types::flags::with_thread_overrides(&[(SW, Some("0"))], || {
            assert!(!affine_guard_predicate_enabled());
        });
        cratonvm_types::flags::with_thread_overrides(&[(SW, None)], || {
            assert!(affine_guard_predicate_enabled(), "default ON");
        });
        cratonvm_types::flags::with_thread_overrides(&[(SW, Some("1"))], || {
            assert!(affine_guard_predicate_enabled());
        });
    }

    /// `for (int k = init; k < n; k++) { Guard((k << 2) >= 0); Guard((k << 2)
    /// < x); }` in `int` arithmetic (`Buffer.checkIndex(k << 2, 4)`), with a
    /// header snapshot. Returns `(graph, entry, [g_lo, g_hi])`.
    fn int_loop() -> (Graph, NodeId, [NodeId; 2]) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let init = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let x = g.add(Op::Param(2), IrType::Int, vec![start], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let two = g.add(Op::Const(2), IrType::Int, vec![], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(4));
        let k = g.add(Op::Phi, IrType::Int, vec![region, init], None);
        let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![k, n], Some(6));
        let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(7));
        let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
        let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
        let sh = g.add(Op::Shl, IrType::Int, vec![k, two], Some(10));
        let lo = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![sh, zero], Some(12));
        let g_lo = g.add(
            Op::Guard { bci: 12 },
            IrType::Void,
            vec![stay, lo],
            Some(12),
        );
        let hi = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![sh, x], Some(14));
        let g_hi = g.add(
            Op::Guard { bci: 14 },
            IrType::Void,
            vec![stay, hi],
            Some(14),
        );
        let k1 = g.add(Op::Add, IrType::Int, vec![k, one], None);
        g.nodes[region as usize].inputs.push(stay);
        g.nodes[k as usize].inputs.push(k1);
        g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(30));
        g.push_safepoint(SafepointSnapshot::new(4, vec![init, n, k], vec![], vec![]));
        g.push_safepoint(SafepointSnapshot::new(12, vec![init, n, k], vec![], vec![]));
        (g, entry, [g_lo, g_hi])
    }

    /// The r23 L8 heap-buffer close (`CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE_INT`):
    /// both `int` guards on `k << 2` leave the loop, each behind the range
    /// check of `A` at both extremes; with the switch off neither does.
    #[test]
    fn r23_l8_int_affine_guards_leave_the_loop_behind_a_wrap_check() {
        const INT_SW: &str = "CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE_INT";
        cratonvm_types::flags::with_thread_overrides(&[(INT_SW, Some("0"))], || {
            let (mut g, _, [g_lo, g_hi]) = int_loop();
            assert_eq!(plant(&mut g), 0);
            assert!(matches!(g.nodes[g_lo as usize].op, Op::Guard { .. }));
            assert!(matches!(g.nodes[g_hi as usize].op, Op::Guard { .. }));
        });
        cratonvm_types::flags::with_thread_overrides(&[(INT_SW, None)], || {
            let (g, _, [g_lo, _]) = int_loop();
            let sh = g.nodes[g.nodes[g_lo as usize].inputs[1] as usize].inputs[0];
            let k = g.nodes[sh as usize].inputs[0];
            assert_eq!(affine_form(&g, sh, k, true), Some((4, 0, true)));
            assert_eq!(affine_form(&g, sh, k, false), None);
            assert_eq!(affine_form(&g, k, k, true), Some((1, 0, false)));
            let (mut g, entry, [g_lo, g_hi]) = int_loop();
            assert_eq!(plant(&mut g), 2);
            assert_eq!(g.nodes[g_lo as usize].op, Op::Dead);
            assert_eq!(g.nodes[g_hi as usize].op, Op::Dead);
            let new: Vec<&crate::ir::Node> = g
                .nodes
                .iter()
                .filter(|n| {
                    matches!(n.op, Op::Guard { bci: 4 }) && n.inputs.first() == Some(&entry)
                })
                .collect();
            assert_eq!(new.len(), 2);
            for guard in &new {
                // `init >= n || ((fits_lo & fits_hi) & holds)`
                let or = &g.nodes[guard.inputs[1] as usize];
                assert_eq!(or.op, Op::Or);
                let and = &g.nodes[or.inputs[1] as usize];
                assert_eq!(and.op, Op::And, "the wrap check rides on the compare");
                let fits = &g.nodes[and.inputs[0] as usize];
                assert_eq!(fits.op, Op::And);
                let bound = |id: NodeId| match g.nodes[id as usize].op {
                    Op::Cmp(_) => g.nodes[g.nodes[id as usize].inputs[1] as usize].op.clone(),
                    ref other => other.clone(),
                };
                assert_eq!(bound(fits.inputs[0]), Op::Const(i64::from(i32::MIN)));
                assert_eq!(bound(fits.inputs[1]), Op::Const(i64::from(i32::MAX)));
            }
        });
    }

    /// The r23 L8 heap-buffer close (`CRATONVM_JIT_IR_RANGE_GUARD_PREDICATE`):
    /// `(k & 7) << 4` is in `[0, 112]` whatever `k` is, `+ 8` in `[8, 120]`;
    /// a negative mask, an unbounded operand or an `int` overflow has no range.
    #[test]
    fn r23_l8_static_ranges() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let k = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
        let four = g.add(Op::Const(4), IrType::Int, vec![], None);
        let eight = g.add(Op::Const(8), IrType::Int, vec![], None);
        let neg = g.add(Op::Const(-1), IrType::Int, vec![], None);
        let thirty = g.add(Op::Const(30), IrType::Int, vec![], None);
        let and = g.add(Op::And, IrType::Int, vec![k, seven], None);
        let shl = g.add(Op::Shl, IrType::Int, vec![and, four], None);
        let add = g.add(Op::Add, IrType::Int, vec![shl, eight], None);
        assert_eq!(static_int_range(&g, and, 0), Some((0, 7)));
        assert_eq!(static_int_range(&g, shl, 0), Some((0, 112)));
        assert_eq!(static_int_range(&g, add, 0), Some((8, 120)));
        let wide = g.add(Op::And, IrType::Int, vec![k, neg], None);
        assert_eq!(
            static_int_range(&g, wide, 0),
            None,
            "a negative mask keeps the sign"
        );
        assert_eq!(static_int_range(&g, k, 0), None);
        let big = g.add(Op::Const(1 << 20), IrType::Int, vec![], None);
        let masked = g.add(Op::And, IrType::Int, vec![k, big], None);
        let over = g.add(Op::Shl, IrType::Int, vec![masked, thirty], None);
        assert_eq!(static_int_range(&g, over, 0), None, "2^20 << 30 leaves int");
        let kl = g.add(Op::I2L, IrType::Long, vec![shl], None);
        assert_eq!(static_long_range(&g, kl), Some((0, 112)));
    }

    /// `int_loop`'s shape with the guard on `(k & 7) << 4 < x`: planted once
    /// at 112, `c = 0`; with the switch off it stays.
    #[test]
    fn r23_l8_a_ranged_guard_leaves_the_loop_at_its_bound() {
        const SW_RANGE: &str = "CRATONVM_JIT_IR_RANGE_GUARD_PREDICATE";
        let build = || {
            let (mut g, entry, [g_lo, g_hi]) = int_loop();
            // Re-point the upper guard's compare at `(k & 7) << 4`.
            let hi = g.nodes[g_hi as usize].inputs[1];
            let sh = g.nodes[hi as usize].inputs[0];
            let k = g.nodes[sh as usize].inputs[0];
            let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
            let four = g.add(Op::Const(4), IrType::Int, vec![], None);
            let and = g.add(Op::And, IrType::Int, vec![k, seven], None);
            let masked = g.add(Op::Shl, IrType::Int, vec![and, four], None);
            g.nodes[hi as usize].inputs[0] = masked;
            g.kill(g_lo);
            (g, entry, g_hi)
        };
        cratonvm_types::flags::with_thread_overrides(
            &[
                (SW_RANGE, Some("0")),
                ("CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE_INT", None),
            ],
            || {
                let (mut g, _, g_hi) = build();
                assert_eq!(plant(&mut g), 0);
                assert!(matches!(g.nodes[g_hi as usize].op, Op::Guard { .. }));
            },
        );
        cratonvm_types::flags::with_thread_overrides(&[(SW_RANGE, None)], || {
            let (mut g, entry, g_hi) = build();
            assert_eq!(plant(&mut g), 1);
            assert_eq!(g.nodes[g_hi as usize].op, Op::Dead);
            let new: Vec<&crate::ir::Node> = g
                .nodes
                .iter()
                .filter(|n| {
                    matches!(n.op, Op::Guard { bci: 4 }) && n.inputs.first() == Some(&entry)
                })
                .collect();
            assert_eq!(new.len(), 1);
            // `... || (0 * at + 112) < x`
            let or = &g.nodes[new[0].inputs[1] as usize];
            let holds = &g.nodes[or.inputs[1] as usize];
            assert_eq!(holds.op, Op::Cmp(CmpOp::Lt));
            let a = &g.nodes[holds.inputs[0] as usize];
            assert_eq!(a.op, Op::Add);
            assert_eq!(g.nodes[a.inputs[1] as usize].op, Op::Const(112));
        });
    }

    /// The r23 L8 heap-buffer close (`CRATONVM_JIT_IR_AFFINE_DERIVED_IV`): a
    /// relative accessor's `p = φ(p0, p + 4)` beside the counted `k`, guarded
    /// `limit - p >= 4` (`nextPutIndex`). Planted once at the pre-header with
    /// `p`'s own range and `int` check; off, it stays.
    #[test]
    fn r23_l8_a_derived_induction_guard_leaves_the_loop() {
        const SW_DERIVED: &str = "CRATONVM_JIT_IR_AFFINE_DERIVED_IV";
        let build = || {
            let mut g = Graph::empty(0);
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            g.entry = start;
            let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let _mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
            let init = g.add(Op::Param(0), IrType::Int, vec![start], None);
            let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
            let limit = g.add(Op::Param(2), IrType::Int, vec![start], None);
            let p0 = g.add(Op::Param(3), IrType::Int, vec![start], None);
            let one = g.add(Op::Const(1), IrType::Int, vec![], None);
            let four = g.add(Op::Const(4), IrType::Int, vec![], None);
            let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(4));
            let k = g.add(Op::Phi, IrType::Int, vec![region, init], None);
            let p = g.add(Op::Phi, IrType::Int, vec![region, p0], None);
            let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![k, n], Some(6));
            let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(7));
            let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
            let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
            let room = g.add(Op::Sub, IrType::Int, vec![limit, p], Some(10));
            let ok = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![room, four], Some(12));
            let guard = g.add(Op::Guard { bci: 12 }, IrType::Void, vec![stay, ok], Some(12));
            let k1 = g.add(Op::Add, IrType::Int, vec![k, one], None);
            let p4 = g.add(Op::Add, IrType::Int, vec![p, four], None);
            g.nodes[region as usize].inputs.push(stay);
            g.nodes[k as usize].inputs.push(k1);
            g.nodes[p as usize].inputs.push(p4);
            g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(30));
            g.push_safepoint(SafepointSnapshot::new(4, vec![init, n, k, p], vec![], vec![]));
            g.push_safepoint(SafepointSnapshot::new(12, vec![init, n, k, p], vec![], vec![]));
            (g, entry, guard)
        };
        cratonvm_types::flags::with_thread_overrides(&[(SW_DERIVED, Some("0"))], || {
            let (mut g, _, guard) = build();
            assert_eq!(plant(&mut g), 0);
            assert!(matches!(g.nodes[guard as usize].op, Op::Guard { .. }));
        });
        cratonvm_types::flags::with_thread_overrides(&[(SW_DERIVED, None)], || {
            let (mut g, entry, guard) = build();
            assert_eq!(plant(&mut g), 1);
            assert_eq!(g.nodes[guard as usize].op, Op::Dead);
            let new: Vec<&crate::ir::Node> = g
                .nodes
                .iter()
                .filter(|n| matches!(n.op, Op::Guard { bci: 4 }) && n.inputs.first() == Some(&entry))
                .collect();
            assert_eq!(new.len(), 1);
            // `zero_trip || (p fits && (limit - p fits && (limit - p_max) >= 4))`
            let or = &g.nodes[new[0].inputs[1] as usize];
            assert_eq!(or.op, Op::Or);
            let and = &g.nodes[or.inputs[1] as usize];
            assert_eq!(and.op, Op::And, "the wrap checks ride on the compare");
        });
    }

    #[test]
    fn r23_w8_l7_overflowing_scales_are_refused() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let iv = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let kl = g.add(Op::I2L, IrType::Long, vec![iv], None);
        let big = g.add(Op::Const(1 << 40), IrType::Long, vec![], None);
        let m = g.add(Op::Mul, IrType::Long, vec![kl, big], None);
        assert_eq!(affine(&g, m, iv), None, "|c| > 2^31");
        // An `int` multiply may wrap: never affine.
        let four = g.add(Op::Const(4), IrType::Int, vec![], None);
        let im = g.add(Op::Mul, IrType::Int, vec![iv, four], None);
        let iml = g.add(Op::I2L, IrType::Long, vec![im], None);
        assert_eq!(affine(&g, iml, iv), None);
    }
}

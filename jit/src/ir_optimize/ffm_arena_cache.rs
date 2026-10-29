// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 24 item B2 (`jit-r24-alloc-and-ffm-loop-residuals-proposals-RETIRED-20261026.md`;
//! `CRATONVM_JIT_IR_FFM_ARENA_CACHE`, perf, opt-in): a native `MemorySegment`
//! element loop resolves its arena handle once, at the loop entry.
//!
//! # Why
//!
//! After the loop-entry session pre-check (`CRATONVM_JIT_IR_FFM_SESSION_PRECHECK`)
//! a native access still decodes its `min` handle through the JIT-readable
//! arena table on every iteration: slot address, key compare, offset, length
//! compare, pointer, key compare again -- about 25 instructions
//! (`ir_lower::emit_scoped_arena_resolve`). `R23SegMono nativeget` pays it per
//! element (about 290 ns per 64 against 130 for the heap twin).
//!
//! # What
//!
//! For a counted loop `for (k = init; k < n; k++)` whose pre-header got a
//! session pre-check `p` (so the loop runs no Java), every native scoped
//! access of the body with session `p`, the literal-null base and the address
//! `min + delta`, `min` loop-invariant and `delta = c * (long) k + d` with
//! `c > 0` ([`affine_long`]), gets four pre-header values appended
//! (`Graph::ffm_arena_cached`):
//!
//! * `entry = ArenaCacheEntry(min, span, p)` with `span = c * (n - 1) + d + w`
//!   (`w` the access width): the table slot of `min`'s block when `p` reads
//!   null, `min` is a live slotted handle and `[min, min + span)` lies in the
//!   block (the double key compare of the scalar resolve), else 0;
//! * `key = min & !(2^30 - 1)`, the slot key that handle owns;
//! * `base = ArenaCacheBase(entry, min)`, the real address of `min`;
//! * `delta` itself.
//!
//! The access then lowers as `entry != 0 && [entry].key == key ? [base +
//! delta] : <today's full path>`.
//!
//! # Why it is exact
//!
//! * Every element the loop touches is in the block: the segment's own
//!   bounds guards keep `delta >= 0` before each access, and `k <= n - 1` on
//!   the stay edge keeps `delta + w <= span`.
//! * The block is the same one on every iteration while its key is: a key is
//!   published once in the process, and `free` / `reallocate` clear it BEFORE
//!   they touch the block (`native-builtins` `unsafe_arena`). Each access
//!   compares the key again, so a free ordered before it (the same thread, or
//!   a happens-before edge) leaves for the full path, which answers exactly as
//!   today. A free on another thread with no such edge races every scalar
//!   access the same way (the race that module documents).
//! * The session check is skipped only behind a hit: `p` read null at the
//!   entry (a never-closing session, or a confined one this thread owns and
//!   that reads open), and the loop runs no Java, so it cannot close; the
//!   access's own check would pass on every iteration.
//! * None of the four values can trap, and none needs a frame: a miss is a
//!   0 entry, which every access answers with its full path.
//!
//! Planted only where `ffm_field_hoist` planted the pre-check, so only on a
//! loop every entry of which runs its pre-header (an OSR entry at the header
//! is refused there, for every value that pass moves). A put the vector
//! pre-loop may serve is cached too (its scalar remainder benefits; the
//! planner skips the appended inputs).

use super::affine_guard_predicate::{affine_long, counted_loop};
use crate::ir::{Graph, IrType, NodeId, Op, UnboxOp, NO_NODE};
use rustc_hash::FxHashSet;

/// `CRATONVM_JIT_IR_FFM_ARENA_CACHE` (JIT round 24 item B2): **default ON**
/// since the round-24 close (perf, `=0` kills). Close battery (5 rotated reps,
/// facts equal HotSpot's): `R26SegLongLoops` native-sum 12947 -> 8223 ns/op,
/// native-put 0.72x; `R23SegMono` nativeget 0.42x, nativeput 0.65x;
/// `R26SegVecPut` native-get-sum 0.68x. Read once per loop; no `static`.
pub(super) fn ffm_arena_cache_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_FFM_ARENA_CACHE")
}

/// The slot-key mask of an arena handle (`ir_lower::SCOPED_ARENA_SLOT_SHIFT`).
const KEY_MASK: i64 = !((1i64 << 30) - 1);

/// Cache the arena resolution of every eligible native access of the loop at
/// `region` (see the module notes). `prechecks` are the session pre-checks
/// just planted in its pre-header (`entry_pred`). Returns how many accesses
/// were cached.
pub(super) fn plant_arena_caches(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    body: &FxHashSet<NodeId>,
    prechecks: &[NodeId],
    header_bci: usize,
) -> usize {
    if prechecks.is_empty() || !ffm_arena_cache_enabled() {
        return 0;
    }
    let dbg = super::loop_pass_report_enabled();
    let Some((iv, _init, n)) = counted_loop(graph, region, entry_pred, back_ctrls, body) else {
        if dbg {
            eprintln!("[ir-licm] ffm arena cache: header {region}: not a counted `k < n` loop");
        }
        return 0;
    };
    let is_null = |g: &Graph, x: NodeId| g.node_opt(x).is_some_and(crate::ir::is_null_const);
    let mut sites: Vec<(NodeId, NodeId, NodeId, NodeId, i64, i64, i64)> = Vec::new();
    let mut ids: Vec<NodeId> = body.iter().copied().collect();
    ids.sort_unstable();
    for u in ids {
        let Some(un) = graph.node_opt(u) else {
            continue;
        };
        let width = match un.op {
            Op::Unbox {
                op:
                    UnboxOp::ScopedGet {
                        width,
                        ordered: true,
                        ..
                    }
                    | UnboxOp::ScopedPut {
                        width,
                        ordered: true,
                    },
                ..
            } => i64::from(width),
            _ => continue,
        };
        if graph.ffm_arena_cached.contains(&u) || un.inputs.len() != 3 + scoped_arity(&un.op) {
            continue;
        }
        let (session, base, addr) = (un.inputs[3], un.inputs[4], un.inputs[5]);
        if !prechecks.contains(&session) || !is_null(graph, base) {
            continue;
        }
        let Some(an) = graph.node_opt(addr) else {
            continue;
        };
        if an.op != Op::Add || an.ty != IrType::Long || an.inputs.len() != 2 {
            continue;
        }
        // Pure nodes float, so neither operand is in `body` (a loop body
        // holds control, memory and pinned nodes only): `min` is the
        // operand invariant in the loop, `delta` the one affine in `k`.
        let (a, b) = (an.inputs[0], an.inputs[1]);
        let split = [(a, b), (b, a)].into_iter().find_map(|(min, delta)| {
            if !super::is_loop_invariant(graph, min, region, body) {
                return None;
            }
            affine_long(graph, delta, iv, 0).map(|(c, d)| (min, delta, c, d))
        });
        let Some((min, delta, c, d)) = split else {
            continue;
        };
        if c <= 0 {
            continue;
        }
        sites.push((u, min, delta, session, c, d, width));
    }
    if sites.is_empty() {
        if dbg {
            eprintln!(
                "[ir-licm] ffm arena cache: header {region}: no native access with session \
                 {prechecks:?}, a null base and an address `min + c * k + d`"
            );
        }
        return 0;
    }
    // `((min, c, d, w, session), [entry, key, base])`, shared by the accesses
    // of one loop that name the same span.
    let mut planted: Vec<((NodeId, i64, i64, i64, NodeId), [NodeId; 3])> = Vec::new();
    let pc = Some(header_bci);
    let mut cached = 0usize;
    for (u, min, delta, session, c, d, w) in sites {
        let Some(mem) = graph.node_opt(session).and_then(|p| p.input_opt(1)) else {
            continue;
        };
        // `span = c * (n - 1) + d + w`, exact in `long`: `affine_long` bounds
        // `|c| <= 2^31` and `|d| <= 2^61`, and `n` is an `int`.
        let Some(tail) = d.checked_add(w).and_then(|x| x.checked_sub(c)) else {
            continue;
        };
        let key_of = (min, c, d, w, session);
        let [entry, key, base] = match planted.iter().find(|(k, _)| *k == key_of) {
            Some(&(_, vals)) => vals,
            None => {
                let nl = graph.add(Op::I2L, IrType::Long, vec![n], pc);
                let cc = graph.add(Op::Const(c), IrType::Long, vec![], None);
                let scaled = graph.add(Op::Mul, IrType::Long, vec![nl, cc], pc);
                let tc = graph.add(Op::Const(tail), IrType::Long, vec![], None);
                let span = graph.add(Op::Add, IrType::Long, vec![scaled, tc], pc);
                let km = graph.add(Op::Const(KEY_MASK), IrType::Long, vec![], None);
                let key = graph.add(Op::And, IrType::Long, vec![min, km], pc);
                let entry = graph.add(
                    Op::Unbox {
                        op: UnboxOp::ArenaCacheEntry,
                        class_id: 0,
                    },
                    IrType::Long,
                    vec![entry_pred, mem, min, span, session],
                    pc,
                );
                let base = graph.add(
                    Op::Unbox {
                        op: UnboxOp::ArenaCacheBase,
                        class_id: 0,
                    },
                    IrType::Long,
                    vec![entry_pred, mem, entry, min],
                    pc,
                );
                planted.push((key_of, [entry, key, base]));
                [entry, key, base]
            }
        };
        for x in [entry, key, base, delta] {
            graph.push_input(u, x);
        }
        graph.ffm_arena_cached.insert(u);
        cached += 1;
    }
    // Each cached access skips its session check and the scalar resolve's
    // length and second key compares, checks the single-pass body keeps on
    // every iteration: the acceptance gate's `GuardElided`. Without it the
    // gate saw `licm` alone (the planted values also mask `Simplified`'s net
    // shrink) and kept the single-pass body, 17x slower on `R26SegLongLoops
    // native-*`.
    if cached > 0 {
        crate::ir_evidence::note(crate::ir_evidence::Transform::GuardElided);
    }
    cached
}

/// The data arity of a scoped access op (`UnboxOp::arity`), 0 otherwise.
fn scoped_arity(op: &Op) -> usize {
    match op {
        Op::Unbox { op, .. } => op.arity(),
        _ => 0,
    }
}

/// The four cache inputs of a cached access, `[entry, key, base, delta]`, or
/// `None` for an access `plant_arena_caches` did not touch.
pub(crate) fn cache_inputs(graph: &Graph, id: NodeId) -> Option<[NodeId; 4]> {
    if !graph.ffm_arena_cached.contains(&id) {
        return None;
    }
    let n = graph.node_opt(id)?;
    let at = n
        .inputs
        .len()
        .checked_sub(crate::ir::SCOPED_ARENA_CACHE_INPUTS)?;
    let s = &n.inputs.as_slice()[at..];
    let vals = [s[0], s[1], s[2], s[3]];
    (!vals.contains(&NO_NODE)).then_some(vals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::CmpOp;

    const KEY: &str = "CRATONVM_JIT_IR_FFM_ARENA_CACHE";

    /// `for (k = 0; k < n; k++) h += s.getAtIndex(LE_INT, k)` over a native
    /// segment once the session pre-check is planted: returns `(graph, region,
    /// entry_pred, back, body, precheck, get, delta)`.
    #[allow(clippy::type_complexity)]
    fn native_get_loop(
        precheck_session: bool,
    ) -> (
        Graph,
        NodeId,
        NodeId,
        NodeId,
        FxHashSet<NodeId>,
        NodeId,
        NodeId,
        NodeId,
    ) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let seg = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let min = g.add(
            Op::Unbox {
                op: UnboxOp::FieldLong(3),
                class_id: 787,
            },
            IrType::Long,
            vec![ctrl, mem, seg],
            Some(2),
        );
        let scope = g.add(
            Op::Unbox {
                op: UnboxOp::FieldRef(2),
                class_id: 787,
            },
            IrType::Ref,
            vec![ctrl, mem, seg],
            Some(2),
        );
        let p = if precheck_session {
            g.add(
                Op::Unbox {
                    op: UnboxOp::SessionPrecheck,
                    class_id: 0,
                },
                IrType::Ref,
                vec![ctrl, mem, scope],
                Some(2),
            )
        } else {
            scope
        };
        let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let region = g.add(Op::Merge, IrType::Control, vec![ctrl, ctrl], Some(2));
        let mphi = g.add(Op::Phi, IrType::Memory, vec![region, mem, mem], Some(2));
        let iv = g.add(Op::Phi, IrType::Int, vec![region, zero, zero], Some(2));
        let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, n], Some(4));
        let iff = g.add(Op::If, IrType::Control, vec![region, cmp], Some(4));
        let body_ctrl = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(4));
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(4));
        let i2l = g.add(Op::I2L, IrType::Long, vec![iv], Some(12));
        let four = g.add(Op::Const(4), IrType::Long, vec![], None);
        let delta = g.add(Op::Mul, IrType::Long, vec![i2l, four], Some(16));
        let addr = g.add(Op::Add, IrType::Long, vec![min, delta], Some(16));
        let le = g.add(Op::Const(0), IrType::Int, vec![], None);
        let get = g.add(
            Op::Unbox {
                op: UnboxOp::ScopedGet {
                    width: 4,
                    signed: true,
                    ordered: true,
                },
                class_id: 0,
            },
            IrType::Int,
            vec![body_ctrl, mphi, seg, p, null, addr, le],
            Some(16),
        );
        let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(21));
        g.nodes[region as usize].inputs[1] = body_ctrl;
        g.nodes[mphi as usize].inputs[2] = get;
        g.nodes[iv as usize].inputs[2] = next;
        g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(27));
        // As `loop_body_with` builds it: control, memory and pinned nodes
        // only. The pure address arithmetic (`i2l`, `delta`, `addr`, `cmp`,
        // `next`) floats; a first build matched `min` / `delta` by body
        // membership and so never planted on a real graph.
        let body: FxHashSet<NodeId> = [region, mphi, iv, iff, body_ctrl, get]
            .into_iter()
            .collect();
        (g, region, ctrl, body_ctrl, body, p, get, delta)
    }

    /// The access gets `[entry, key, base, delta]` under the switch, the
    /// entry from `min`, the span `4 * (n - 1) + 4 = 4 * n` and the pre-check; nothing
    /// without the switch or without a pre-check.
    #[test]
    fn r24_b2_a_native_get_loop_caches_its_arena_resolution() {
        let (mut g, region, entry, back, body, p, get, delta) = native_get_loop(true);
        let n = cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            plant_arena_caches(&mut g, region, entry, &[back], &body, &[p], 2)
        });
        assert_eq!(n, 1);
        let [e, k, b, d] = cache_inputs(&g, get).expect("cached");
        assert_eq!(d, delta);
        let en = &g.nodes[e as usize];
        assert!(matches!(
            en.op,
            Op::Unbox {
                op: UnboxOp::ArenaCacheEntry,
                ..
            }
        ));
        assert_eq!(en.inputs[0], entry, "in the pre-header");
        assert_eq!(en.inputs[4], p, "keyed on the pre-check");
        let span = &g.nodes[en.inputs[3] as usize];
        assert_eq!(span.op, Op::Add);
        // span = 4 * n + (d + w - c) = 4 * n + (0 + 4 - 4): `[min, min + 4n)`.
        assert_eq!(g.nodes[span.inputs[1] as usize].op, Op::Const(0));
        assert_eq!(g.nodes[k as usize].op, Op::And);
        assert!(matches!(
            g.nodes[b as usize].op,
            Op::Unbox {
                op: UnboxOp::ArenaCacheBase,
                ..
            }
        ));
        assert_eq!(g.nodes[get as usize].inputs.len(), 7 + 4);
        // The operand-type table agrees with the planted shapes (a first build
        // planted `long`s where the general `Unbox` row wants a reference, and
        // the verifier refused every cached method).
        for id in [e, b] {
            let node = &g.nodes[id as usize];
            for slot in 0..node.inputs.len() {
                let ty_of = |x: NodeId| g.node_opt(x).map(|xn| xn.ty);
                let want =
                    crate::ir::required_input_type(&node.op, node.ty, &node.inputs, slot, &ty_of);
                if let Some(want) = want {
                    assert_eq!(
                        g.nodes[node.inputs[slot] as usize].ty, want,
                        "n{id} slot {slot}"
                    );
                }
            }
        }
        // Off, no pre-check: untouched.
        for (on, pre) in [(false, true), (true, false)] {
            let (mut g, region, entry, back, body, p, get, _) = native_get_loop(pre);
            let prechecks: Vec<NodeId> = if pre { vec![p] } else { Vec::new() };
            let n = cratonvm_types::flags::with_thread_overrides(
                &[(KEY, if on { Some("1") } else { Some("0") })],
                || plant_arena_caches(&mut g, region, entry, &[back], &body, &prechecks, 2),
            );
            assert_eq!(n, 0, "on={on} pre={pre}");
            assert!(cache_inputs(&g, get).is_none());
        }
    }
}

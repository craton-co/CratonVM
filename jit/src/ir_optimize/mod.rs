// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Optimization passes on the Sea-of-Nodes IR graph.
//!
//! Each pass takes `&mut Graph` and transforms it in place.
//! The scalar passes (fold, simplify, GVN, the eliminators) and LICM are
//! idempotent and may be re-run. The unroller is not: a partial unroll keeps
//! the loop, so offering it again copies the copies, and [`optimize`] runs it
//! at most once per compile.

use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use super::ir::{
    may_raise, memory_shape_of, CmpOp, Graph, IrType, MemKind, MemoryShape, Node, NodeId, Op,
    SafepointSlotKind, SafepointSnapshot, NO_NODE,
};

// nio21 wave 4 (lane L5): the loop-entry class guard for an invariant
// receiver whose in-loop guarded splices it folds (`encodeBufferLoop`'s
// `src.get()` / `dst.put(b)`).
pub(super) mod class_predicate;
// t1 wave 8 (lane FFM27, FFM27-2): loop versioning on an invariant class test
// for a bimorphic receiver (heap vs native MemorySegment).
pub(crate) mod class_unswitch;
// nio21 wave 5 (lane L5): `CharSequence.charAt` deopting on a miss inside a
// class-predicated loop, so the field forward sees a call-free body.
mod charat_trap;
// JIT round 23 wave 8 (lane W8-L4): the FFM element inline expansion's final
// segment-field reads read once per loop entry.
mod ffm_field_hoist;
// nio21 wave 16 (lane L3): a field read answered by the earlier read or store
// of the same cell through forward merges (`encodeBufferLoop`'s spliced
// `hasRemaining()` + `get()` / `put(b)` pairs).
mod join_load_forward;
mod loop_field_forward;
// JIT round 22 wave 1 (lane loops, LOOPS22-1): a counted loop whose carried
// values are all linear is replaced by its exit values
// (`CRATONVM_JIT_IR_LOOP_RETIRE_ACCUMULATORS`, opt-in).
mod loop_retire;
// nio20 wave 6 (lane L4): a direct static read out of a write-free loop, and
// the loop-entry predicate for a constant-masked index.
mod long_induction_narrow;
mod loop_static_hoist;
mod mask_predicate;
// nio21 wave 3 (lane L5): the loop-entry guards for a monotone, non-counted
// loop index (`UTF_8$Decoder.decodeArrayLoop`'s `sp` / `dp`).
mod monotone_predicate;
// nio21 wave 6 (lane L5): the loop-entry guards for a variable-offset index
// `a[i + o]` (`HeapByteBuffer.put`'s `hb[p + offset]`).
mod offset_predicate;
// JIT round 20 wave 18 (lane xSplitIf, LOOPOPT20-1): split-if for a branch on
// a phi of constants (`while (buf.hasRemaining())`'s spliced boolean join).
mod split_if;
// JIT round 25 wave 2 (lane branch-fold): boolean φs, boolean compares, empty
// diamonds and branch-implied `If`s the graph already decides (opt-in).
mod bool_branch;
// JIT round 23 wave 8 (lane W8-L7): a spliced loop's entry predicates step over
// LICM's pre-header copy of that loop's length / element read.
mod spliced_hoist_trap;
// JIT round 23 wave 8 (lane W8-L7, TYPECHECK21-1 part C): a `checkcast`'s
// readers read its operand (`CRATONVM_JIT_IR_CHECKCAST_VALUE_FORWARD`).
mod checkcast_forward;
mod final_typecheck_fold;
// JIT round 21, lane tcfuse (TCPERF21-3): one shared class word per value two
// or more type checks test (`CRATONVM_JIT_TYPECHECK_SHARED_CLASS_WORD`, opt-in).
mod typecheck_class_word;
// JIT round 23 wave 8 (lane W8-L7): the counted-loop range predicate for a
// generic body `Guard(Cmp)` on an affine function of the induction variable.
mod affine_guard_predicate;
// JIT round 25 wave 6 (lane W6-C): a loop-entry `String` guard for an
// invariant `CharSequence.charAt` receiver, in method-entry and OSR-door
// graphs (`CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE`, opt-in).
mod seq_charat_predicate;
// JIT round 24 item C: full unroll of a constant-trip loop whose body branches.
mod cfg_unroll;
pub(crate) mod ffm_arena_cache;

// TLGET21-3: a trap-form `ThreadLocal.get` chain moved to its loop's pre-header.
mod tl_chain_predicate;
// j27 wave 1 (lane FRAMES): collect the frame copies a pass orphaned
// (`CRATONVM_JIT_IR_DROP_ORPHANED_FRAME_COPIES`, default ON).
mod orphan_frames;
// j27 wave 1 (lane LOOPCG): multi-root linear reassociation of int/long trees
// (`CRATONVM_JIT_IR_LINEAR_REASSOC`, default ON).
mod linear_reassoc;
// j27 wave 6 (lane T1LOOPSTATIC, LOOPSTATIC21-3): the post-promotion unroll's
// `CRATONVM_DBG_UNROLL` census line.
mod promoted_unroll_census;
// j27 wave 8 (lane W8LOOP): the main/post factor of a body that cannot deopt
// (`CRATONVM_JIT_IR_UNROLL_WIDE_PURE`, opt-in).
mod unroll_wide_pure;
// Wave 7b (lane ir-opt-inline): the late trap-free pre-header of a loop that
// only reads (`CRATONVM_JIT_IR_TRAP_FREE_PREHEADER`, default ON).
mod w7_trap_free_preheader;
// j27 wave 5 (lane OPTHUNT): tests written while hunting pass miscompiles.
#[cfg(test)]
mod j27_opthunt_tests;
// j27 wave 5 (lane CSEHUNT): the points-to walk's `(node, depth)` memo.
#[cfg(test)]
mod j27_csehunt_tests;
// j27 wave 2 (lane STRIPMINE): the main loop's poll once per strip
// (`CRATONVM_JIT_IR_STRIP_MINE`, opt-in); the lowering asks the latch rule.
mod strip_mine;
pub(crate) use strip_mine::{latch_enters_only_strip_inner, strip_outer_twin};
// j27 wave 4 (lane MONROWS): the block coarsening's array-op admission
// (MON17-5, `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_ARRAY_OPS`, opt-in) and the
// caller-held CALL pairs the adjacent-pair coarsening keeps whole (SY16-3),
// and the dominated field-monitor base the lowering seeds (MON17W2-2).
pub(crate) mod monitor_rows;

pub(crate) use charat_trap::seq_charat_traps_on_miss;
pub(crate) use redundant_loads_pass::control_not_dominated_by;
pub(crate) use unroll_pure_body_enabled::build_users;
// JIT rx27 wave 1 (lane scanloop): read by `ir_lower`'s String prefix.
pub(crate) use charat_trap::{seq_charat_proven_string_enabled, seq_charat_receiver_proven_string};
pub(crate) use loop_static_hoist::{loop_static_hoist_enabled, static_final_hoist_enabled};
pub(crate) use seq_charat_predicate::w7d::seed_seq_charat_own_receiver_rows;
pub(crate) use typecheck_class_word::share_typecheck_class_words;
// JIT round 20 wave 11 (lane pLoop, OSR4-2): read by `lib.rs`'s direct-static feed.
pub(crate) use loop_field_forward::static_promote_enabled as loop_static_promote_enabled;
// JIT round 21 wave 1 (lane loops, LOOPS21-3): read by the same feed.
pub(crate) use loop_field_forward::fresh_promote_static_feed_enabled;
pub(crate) use mask_predicate::{
    mask_guard_elide_enabled, mask_guard_proves_access, MaskGuardProofs,
};

#[cfg(test)]
mod proven_compact_withdraw_tests;
#[cfg(test)]
mod r15w1_sync_self_lock_pass_tests;
#[cfg(test)]
mod r25_item4_proven_compact_withdrawal_tests;
/// Index into [`IR_UNROLL_CENSUS`], in [`UnrollCensus`] field order.
mod unroll_bucket {
    pub const HEADERS: usize = 0;
    pub const UNROLLED: usize = 1;
    pub const NOT_SINGLE_BACKEDGE: usize = 2;
    pub const NOT_COUNTED: usize = 3;
    pub const RUNTIME_BOUND: usize = 4;
    pub const TRIP_OVER_CAP: usize = 5;
    pub const CONTROL_SHAPE: usize = 6;
    pub const SIDE_EFFECT: usize = 7;
    pub const BODY_UNCLONABLE: usize = 8;
    pub const SAFEPOINT_NAMED: usize = 9;
    pub const ESCAPES_OR_PINNED: usize = 10;
    /// A real refusal, so it sits INSIDE the identity -- and it must sit below
    /// `RUNTIME_BOUND_TRAP_FREE`, because that is where the identity's sum
    /// stops.
    pub const FRAME_UNCOPYABLE: usize = 11;
    /// A sub-count of `RUNTIME_BOUND`, deliberately outside the identity.
    pub const RUNTIME_BOUND_TRAP_FREE: usize = 12;
    /// Also a sub-count of `RUNTIME_BOUND`, also outside the identity.
    pub const RUNTIME_BOUND_PURE_BODY: usize = 13;
    /// A sub-count of `UNROLLED`, also outside the identity.
    pub const PER_COPY_FRAMES: usize = 14;
    /// A runtime-bound loop declined for being runtime-bound and nothing else.
    /// The TERMINAL counterpart of `RUNTIME_BOUND`, and the one in the identity.
    pub const RUNTIME_BOUND_REFUSED: usize = 15;
    /// A loop given a partial unroll. In the identity, disjoint from `UNROLLED`.
    pub const PARTIALLY_UNROLLED: usize = 16;
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;

// ── Per-copy deopt metadata ──────────────────────────────────────────
//
// These two tests are deliberately OPPOSED. The first asserts the loop
// unrolls with per-copy frames and that each copy's frame holds that copy's
// values; the second asserts the SAME fixture is refused with the flag off.
// Neither can pass vacuously while the other holds: if the fixture were one
// the unroller took anyway, the second would fail, and the first would then be
// proving nothing about the mechanism.

// ── Redundant-load elimination ─────────────────────────────────

/// Regressions from the 2026-09-12 JIT review: algebraic identities that
/// ignored the node's type, and a compare fold keyed on the result type.
#[cfg(test)]
mod typed_identity_tests;

/// Round-9 review (`docs/feature-designs/jit-ir-optimizations.md`): folds and
/// identities that were missing, the trivial-guard sweep, the null-compare
/// fold, and the write-only-store / safepoint soundness fix.
#[cfg(test)]
mod r9_iropt_tests;

/// Round 9 wave 2, lane `irmid`: the unroller's unreachable-frames net, DCE of
/// an effect-free infinite loop's control, and snapshot liveness.
#[cfg(test)]
mod r9w2_irmid_tests;

/// Round 9 wave 4, lane `irlower4`: request L3 (NOTES-w3-irmid3) -- a store
/// into a fresh allocation emits no deopt, so the snapshot at its own bci is
/// not consultable and no longer pins the allocation for write-only DSE.
#[cfg(test)]
mod r9w4_irlower4_tests;

/// Round 9 wave 10 (irl10), `perf-long-counted-loops-lose-the-optimizing-osr-body`:
/// a `long` counted loop -- `lload; lload; lcmp; if<cc>`, i.e.
/// `Cmp(cc, LCmp(i, n), 0)` -- is recognised as a counted loop (full-unroll
/// candidate with a constant bound, partial-unroll candidate with a runtime
/// one) instead of "the exit test names none of the constant-stride phis".
#[cfg(test)]
mod r9w10_irl10_tests;

#[cfg(test)]
mod r9w11_irl11_tests;

// ── Round 11 (lane iropt) ────────────────────────────────────────────

#[cfg(test)]
mod r11_iropt_tests;

#[cfg(test)]
mod r11w2_mid_tests;

#[cfg(test)]
mod r11w3_mid_tests;

// ── Round 11 wave 5 (lane mid) ───────────────────────────────────────

// ── Round 11 wave 6 (lane mid) ───────────────────────────────────────

#[cfg(test)]
mod r11w6_mid_tests;

// ── Round 11 wave 7 (lane mid) ───────────────────────────────────────

#[cfg(test)]
mod r11w7_mid_tests;

// ── Round 11 wave 8 (lane mid) ───────────────────────────────────────

#[cfg(test)]
mod r11w8_mid_tests;

// ── Round 11 wave 9 (lane mid): lock coarsening ──────────────────────

// ── Round 11 wave 14 (lane fib): nested-lock elision ─────────────────

// ── Round 14 wave 3 (lane sync): synchronized-splice windows in the monitor passes ──

#[cfg(test)]
mod r14w3_sync_monitor_pass_tests;

// ── Round 14 wave 4 (lane sync4): SY3-2, a window inside a held region ──

#[cfg(test)]
mod r14w4_sync4_window_elision_tests;

// ── Round 14 wave 6 (lane sync6): S5-2, `holdsLock` on the finished graph ──

#[cfg(test)]
mod r12w2_iropt_tests;

/// Round 12 wave 3 (lane iropt2): the W2-6 scalar identities.
#[cfg(test)]
mod r12w3_iropt2_identity_tests;

/// Round 12 wave 3 (lane iropt2): W2-4, compares the range lattice decides.
#[cfg(test)]
mod r12w3_iropt2_range_fold_tests;

/// Round 12 wave 3 (lane iropt2): W2-3, the anchored-node index.
#[cfg(test)]
mod r12w3_iropt2_anchor_index_tests;

/// Round 12 wave 6 (lane iropt5): the optimizer half of the constant `If`
/// (`prune_constant_ifs`). Every positive case left both arms in the graph
/// before this wave.

/// Round 12 wave 7 (lane iropt6): [`eliminate_hoisted_duplicate_loads`], the
/// read merge LICM's clean-up always meant to make and did not on the default
/// path (`CRATONVM_JIT_IR_LOAD_CSE` off).

/// Round 13 wave 3 (lane framestate): the passes that copy or root snapshots
/// treat a chain snapshot (`ir::SpliceScopeChain`, recorded inside a spliced
/// body) as what it is -- an additional frame of the spliced pc, never a frame
/// of the compiling method.
#[cfg(test)]
mod r13w3_framestate_tests;

// ── Round 16 (lane matrix): invariant array-element loads ────────────

// Round 16 wave 12 (lane syncco, BN2-3 / MON16-8): loop lock coarsening
// ([`coarsen_loop_sync_windows`]) -- the shapes it takes and refuses on
// hand-built `R16SyncSyncM.runNs`-shaped graphs, the builder's own splice of
// a synchronized leaf in a counted loop, and the coarsened body executed
// against counting monitor helpers.

// ── Round 17 (CG16P-2): loop-predicated array lengths ────────────────

#[cfg(test)]
mod r17_length_predicate_tests;

/// JIT r23 L1W3-2: a loop nest's inner range predicate, moved out again by
/// the outer loop's length predicate, leaves no unnamed frame behind.
#[cfg(test)]
mod l1w3_2_nested_predicate_frame_tests;

// ── JIT round 19 wave 5 (lane osr5): IROPS3-1 clamp length predicates ──

// ── Round nio20 wave 5, lane L3: the counted-loop recogniser and the ─────
// ── pure-body full unroll ─────────────────────────────────────────────────
//
// `Nio20FenceRow noload` -- `for (int k = 0; k < 16; k++) h += k ^ i;` -- ran
// 13 ns against HotSpot's 4 with the loop kept rolled: the trip was over the
// old cap of 8, and a javac body is named by a frame at every bci, which the
// full unroller refused unless the WHOLE METHOD was trap-free (and
// `CRATONVM_JIT_IR_DROP_UNREACHABLE_HOMES`, default OFF, was on).

/// JIT round 20 wave 12 (lane qLoop, W5L3-3): the main/post partial unroll
/// takes a `long` induction variable, its test read through `lcmp`.

/// JIT round 20 wave 11 (lane pLoop, LOOPOPT20-4 / L5W7-3): a direct,
/// non-volatile `getstatic` is no hard LICM barrier; every other static access
/// still is.

/// Interpreter round i5 wave 105 (lane L2): `decide_branch_implied_guards`'s
/// sense rule, and its refusal of a zero-divisor guard
/// (`jit-ncw-branch-decided-guards-sigfpe-in-a-freed-code-buffer-FIXED-20261016.md`).

// ── JIT round 21 wave 4 (lane bce, BCE21-2) ─────────────────────────────

// ── JIT round 23 wave 2d (lane L1): TCL23-1 / TCL23-2 ────────────────

#[cfg(test)]
mod r23_l1_null_base_load_predicate_tests;

/// JIT round 23 wave 4 (lane L1; `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_CHAIN`):
/// the null-base load predicate of a loop SPLICED from a callee, named by the
/// loop-entry CHAIN frame.

/// JIT round 23 wave 5 (lane L1, TCL23-3; `CRATONVM_JIT_IR_CHECKCAST_PREDICATE`).

/// JIT r23 w2c close (`CRATONVM_JIT_IR_CAST_AFTER_INSTANCEOF`).
#[cfg(test)]
mod r23_w2c_cast_after_instanceof_tests;

/// JIT r23 w2c close (`CRATONVM_JIT_IR_UNROLL_AFTER_PREDICATES`).
#[cfg(test)]
mod r23_w2c_unroll_after_predicates_tests;

/// JIT r23 w2c close (`CRATONVM_JIT_IR_CHECKCAST_PREDICATE_CHAIN`): the
/// checkcast predicate at a loop spliced from a callee, and the null-base
/// predicate behind it testing the class it proved.

/// JIT round 23 wave 6 (lane L1; `CRATONVM_JIT_IR_LICM_ARRAY_LOAD_AFTER_PREDICATES`).
#[cfg(test)]
mod r23_l1_array_load_after_predicates_tests;

/// JIT r25 wave 11, lane W11-4 (`CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS`, default ON since j27 2026-10-28):
/// the loop proofs of the regex scan loop, as one switch now that the chain is
/// spliced (`docs/feature-designs/regex-jit-contract.md`,
/// "Wave 11 W11-4").
///
/// The c24 listing of `Pattern$Start.match`, the hot body of every `\s+` row,
/// runs about 75 instructions per character in the spliced
/// `BmpCharPropertyGreedy` scan loop. Of those, the loop-invariant ones are:
///
/// * the `predicate` field read behind the layout-flag check, every character;
/// * the lambda class test, materialised to 0/1, stored, reloaded and tested;
/// * the `String` class test, coder byte and `value` read of the `charAt`
///   prefix.
///
/// HotSpot's loop is 8-12. The passes that answer exactly these once, at the
/// loop entry, already exist, each behind its own opt-in switch:
///
/// * the `String` loop-entry guard (W6-C, `_SEQ_CHARAT_PREDICATE`);
/// * its spliced-header arm (W7-D, `_SPLICED`, the chain frame: "see through a
///   splice");
/// * the hoist of the prefix's `String` reads (`_SEQ_CHARAT_HOIST`);
/// * the field-class predicate, which reads `this.predicate` / `this.next` once
///   behind one class guard and folds the in-loop class tests and their miss
///   arms (W7-D P2, `_SEQ_CHARAT_FIELD_CLASS_PREDICATE`);
/// * the entry bound (W7-D P3, `_SEQ_CHARAT_ENTRY_BOUND`).
///
/// They were measured before the chain was spliced, and before W10-3b kept
/// the scan loop's `charAt` as the prefix CALL these passes key on (spliced as
/// bytecode, it gave them no call to see). So none of them could reach the hot
/// loop. This switch turns all five on together. Each of them still yields to
/// its own `=0` (an explicit kill wins), and an explicit `=1` of one of them is
/// unchanged.
pub(crate) mod r25_w11_4_chain_loop_proofs {
    /// `CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS` (default ON since j27 2026-10-28,
    /// `=0` kills). Read per compile, no
    /// `static`.
    pub(crate) fn chain_loop_proofs_enabled() -> bool {
        // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
        true
    }

    /// Is `key` (one of the five switches above, whose own reader already
    /// returned `false`) on through the bundle? Not when it is set to an off
    /// word explicitly (`0`, `false`, `off`, `no`: the default-ON kill rule,
    /// `flags::value_is_off`; rx27 lane R -- only `0` was honoured, so
    /// `=false` read as on).
    pub(crate) fn bundled(key: &str) -> bool {
        chain_loop_proofs_enabled()
            && !explicitly_off(key)
            // JIT r25 wave 13 (W13-4): the hoist and the entry bound it feeds
            // are no longer in the bundle (`super::r25_w13_4`).
            && !super::r25_w13_4::left_out_of_the_bundle(key)
    }

    /// Is `key` set to an off word? Shared with the rx27 outer-loop hoist
    /// (`charat_trap::outer_hoist_enabled`), which turns on the same keys.
    pub(crate) fn explicitly_off(key: &str) -> bool {
        cratonvm_types::flags::runtime_var(key)
            .ok()
            .is_some_and(|v| cratonvm_types::flags::value_is_off(&v))
    }

    #[cfg(test)]
    mod tests;
}

/// JIT r25 wave 12, lane W12-4 (`CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS`,
/// opt-in by its own key, also under `CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS`, which
/// is default ON since j27 2026-10-28, so ON unless `=0`): the loop proofs
/// for the INNER loop of a guarded outer loop
/// (`docs/feature-designs/regex-jit-contract.md`,
/// "Wave 12 W12-4").
///
/// On `split-ws` (c29), the `String` predicate plants at `Start.match`'s
/// position loop (combined bci 31), which holds the spliced
/// `BmpCharPropertyGreedy` scan loop. The hoist moves the scan loop's `charAt`
/// `String` reads to that OUTER entry. But the field-class predicate (W7-D P2)
/// and the entry bound (P3) are offered only the planted headers, and on the
/// outer loop both refuse:
///
/// * P2: an opaque writer, the greedy node's back-off `next.match`, whose link
///   the chain rule refused as rare. It is a plain call in the outer loop,
///   outside the scan loop.
/// * P3: no dominating `i < x` with an invariant `x`. The scan loop's index is
///   the greedy's own `i`, bounded by `i < to`, with `to` read once per
///   position, invariant only in the inner loop.
///
/// The scan loop itself, where the per-character work is, was never asked.
/// Under the switch both passes are also offered every loop nested in a
/// planted one:
///
/// * Its `charAt` calls trap: the outer licence covers them, which is why the
///   hoist moved them.
/// * Its writers are the trapping `charAt`, the predicate's miss-arm call and
///   the lambda body.
/// * Its bound is its own header test.
///
/// Two premise readings widen with it, each sound for the reason given at the
/// site:
///
/// * a miss-arm call reached from the test's miss projection through
///   straight-line control (guards, no merge) is folded with the test;
/// * the bound walk passes through guards on its way up.
///
/// Every soundness argument of the two passes is the per-loop one, unchanged.
/// An inner loop's pre-header is a block of the outer body, and the inner
/// loop-entry frame (chain form for a spliced header) is what the deopt
/// resumes with.
pub(crate) mod r25_w12_4 {
    use super::{build_users, loop_body_with, loop_headers};
    use crate::ir::{Graph, NodeId, Op, NO_NODE};
    use rustc_hash::FxHashSet;

    /// `CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS` (`=1`, or the W11-4 bundle,
    /// default ON since j27 2026-10-28; `=0` kills). Read per compile, no `static`.
    pub(crate) fn inner_loop_proofs_enabled() -> bool {
        false
            || super::r25_w11_4_chain_loop_proofs::bundled(
                "CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS",
            )
    }

    /// `planted`, plus (under the switch) every loop header nested in one of
    /// them: its header, or its entry control, lies in the planted loop's body.
    pub(crate) fn with_nested(graph: &Graph, planted: &FxHashSet<NodeId>) -> FxHashSet<NodeId> {
        let mut out = planted.clone();
        if !inner_loop_proofs_enabled() || planted.is_empty() {
            return out;
        }
        let users = build_users(graph);
        let headers = loop_headers(graph);
        for (region, entry_pred, back) in &headers {
            if !planted.contains(region) || *entry_pred == NO_NODE {
                continue;
            }
            let body = loop_body_with(graph, &users, *region, *entry_pred, back);
            for (inner, inner_entry, _) in &headers {
                if inner != region
                    && (body.contains(inner)
                        || (*inner_entry != NO_NODE && body.contains(inner_entry)))
                {
                    out.insert(*inner);
                }
            }
        }
        out
    }

    /// The plan of the test whose miss projection reaches `call` through
    /// straight-line control. Walk `call`'s control input up over guards (they
    /// only narrow) to a `Proj(1)` of an `If` that `plan_of` names. `None` at a
    /// merge, any other node, or after 16 steps.
    pub(crate) fn miss_arm_plan(
        graph: &Graph,
        call: NodeId,
        plan_of: impl Fn(NodeId) -> Option<usize>,
    ) -> Option<usize> {
        let mut cur = graph.node_opt(call)?.input_opt(0)?;
        for _ in 0..16 {
            let n = graph.node_opt(cur)?;
            match n.op {
                Op::Guard { .. } => cur = n.input_opt(0)?,
                Op::Proj(1) => {
                    let iff = n.input_opt(0)?;
                    if graph.node_opt(iff)?.op != Op::If {
                        return None;
                    }
                    return plan_of(iff);
                }
                _ => return None,
            }
        }
        None
    }

    /// A one-line description of node `id` for the `CRATONVM_DBG_LICM`
    /// refusal lines: its op, bci and, for a call, the target it names.
    pub(crate) fn describe_node(graph: &Graph, id: NodeId) -> String {
        let Some(n) = graph.node_opt(id) else {
            return format!("n{id} (gone)");
        };
        let target = match n.op {
            Op::Call { info_ptr } if info_ptr != 0 => {
                // SAFETY: as `charat_trap::is_seq_charat_call`: a non-zero
                // `info_ptr` on an `Op::Call` addresses a `JitInvokeInfo` the
                // planner keeps alive for the compiled method's life.
                let info = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
                format!(
                    " -> {}.{}{} (kind {})",
                    info.class_name, info.method_name, info.descriptor, info.invoke_kind
                )
            }
            _ => String::new(),
        };
        let ctrl = n
            .input_opt(0)
            .and_then(|c| {
                graph
                    .node_opt(c)
                    .map(|cn| format!(", control n{c} {:?}", cn.op))
            })
            .unwrap_or_default();
        format!("n{id} {:?} at bci {:?}{target}{ctrl}", n.op, n.bytecode_pc)
    }

    #[cfg(test)]
    mod tests;
}

/// JIT r25 wave 13, lane W13-4: what the c30 census of
/// `CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS` says, and the two changes it
/// justifies (`docs/feature-designs/regex-jit-contract.md`,
/// "Wave 13 W13-4").
///
/// * **The field-class predicate works on the inner scan loop.** With W12-4's
///   inner-loop offer it plants at the spliced greedy scan loop (`Start.match`
///   region 24, bci 99: `planted 1 guard(s)`, `tests folded=1 reads
///   replaced=1`). The outer position loop stays refused, and the writers are
///   now named: the predicate's miss-arm `CharPredicate.is` call (its test is
///   not a plan of the OUTER loop, whose `this.next.predicate` base varies per
///   position), and the greedy's kept back-off `Node.match`. That refusal is
///   correct.
/// * **The hoist is the loss.** `=1` against `=1 HOIST=0` (c29), and the c30
///   rows: the bundle loses 3-8 % while the hoist is in it.
///   * The hoist moves the scan loop's `String` reads to the OUTER loop's
///     entry, so they are live across the whole position loop and its calls.
///   * It leaves two per-character guards in place of the prefix's one
///     compare.
///   * The entry bound that would retire those guards never answers
///     (`entry-bound loops=2 bounds=0 refused: fact=1`).
///
///   So the bundle no longer includes `_SEQ_CHARAT_HOIST`, nor
///   `_SEQ_CHARAT_ENTRY_BOUND`, which only rewrites the hoist's guards.
///   Either can still be turned on alone with `=1`. The census now prints
///   where the entry bound's walk stopped, the next question for the hoist
///   (`[ir-licm] charAt entry bound: header R: nG unanswered: ...`).
pub(crate) mod r25_w13_4 {
    use crate::ir::{Graph, NodeId, Op};

    const LEFT_OUT: [&str; 2] = [
        "CRATONVM_JIT_IR_SEQ_CHARAT_HOIST",
        "CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND",
    ];

    /// Is `key` one of the switches the W11-4 bundle no longer turns on?
    pub(crate) fn left_out_of_the_bundle(key: &str) -> bool {
        LEFT_OUT.contains(&key)
    }

    /// For the `CRATONVM_DBG_LICM` line: the index node, and the control
    /// chain from `ctrl` upwards (at most 12 nodes, stopping at `region`):
    /// each node's id and op, an `If` with its condition.
    pub(crate) fn describe_bound_walk(
        graph: &Graph,
        region: NodeId,
        ctrl: NodeId,
        idx: NodeId,
    ) -> String {
        let mut out = format!("idx n{idx} {:?}; walk", graph.node_opt(idx).map(|n| &n.op));
        let mut cur = ctrl;
        for _ in 0..12 {
            let Some(n) = graph.node_opt(cur) else {
                out.push_str(" -> (gone)");
                break;
            };
            out.push_str(&format!(" -> n{cur} {:?}", n.op));
            if n.op == Op::If {
                if let Some(c) = n
                    .input_opt(1)
                    .and_then(|c| graph.node_opt(c).map(|cn| (c, cn)))
                {
                    out.push_str(&format!(
                        " [cond n{} {:?} {:?}]",
                        c.0,
                        c.1.op,
                        c.1.inputs.as_slice()
                    ));
                }
            }
            if cur == region {
                out.push_str(" (header)");
                break;
            }
            match n.input_opt(0) {
                Some(next) => cur = next,
                None => break,
            }
        }
        out
    }

    #[cfg(test)]
    mod tests;
}

#[cfg(test)]
mod strength_reduce_shape_regression_tests;

#[cfg(test)]
mod collapse_merge_effect_order_regression_tests;

#[cfg(test)]
mod opcost_loop_baseline_tests;

#[cfg(test)]
mod opcost_sync_tests;

/// Reuse an existing identical `Op::Phi` anchored at `ctrl` instead of keeping
/// a duplicate built by a fresh field-forwarding/promotion walk.
pub(super) fn dedup_walk_phi(
    graph: &mut Graph,
    ctrl: NodeId,
    value: NodeId,
    memo: &mut FxHashMap<NodeId, NodeId>,
) -> NodeId {
    let Some((ty, built_inputs)) = graph
        .node_opt(value)
        .map(|n| (n.ty, n.inputs.as_slice().to_vec()))
    else {
        return value;
    };
    let existing = graph.users_of(ctrl).into_iter().find(|&u| {
        u != value
            && graph.node_opt(u).is_some_and(|n| {
                n.op == Op::Phi
                    && n.ty == ty
                    && n.inputs.len() == built_inputs.len()
                    && n.inputs
                        .iter()
                        .zip(built_inputs.iter())
                        .all(|(&a, &b)| a == b || (a == u && b == value))
            })
    });
    let Some(existing) = existing else {
        return value;
    };
    graph.replace_all_uses(value, existing);
    for slot in memo.values_mut() {
        if *slot == value {
            *slot = existing;
        }
    }
    graph.kill(value);
    existing
}

mod optimize_context;
#[allow(unused_imports)]
pub use optimize_context::*;
#[allow(unused_imports)]
use optimize_context::*;
mod apply_loop_block_coarsening_in_place;
#[allow(unused_imports)]
pub use apply_loop_block_coarsening_in_place::*;
#[allow(unused_imports)]
use apply_loop_block_coarsening_in_place::*;
mod redundant_loads_pass;
#[allow(unused_imports)]
pub use redundant_loads_pass::*;
#[allow(unused_imports)]
use redundant_loads_pass::*;
mod plant_null_base_load_predicates;
#[allow(unused_imports)]
pub use plant_null_base_load_predicates::*;
#[allow(unused_imports)]
use plant_null_base_load_predicates::*;
mod unroll_pure_body_enabled;
#[allow(unused_imports)]
pub use unroll_pure_body_enabled::*;
#[allow(unused_imports)]
use unroll_pure_body_enabled::*;
#[cfg(test)]
mod wave4_opt_tests;

// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 27 wave 1 (lane FRAMES,
//! `jr21-verify-each-pass-unclaimed-duplicate-snapshot-20261026`): collect the
//! frame copies a pass ORPHANED.
//!
//! # The defect
//!
//! Several passes push a second snapshot at a bci that already has one and
//! bind it to the nodes it was made for (`Node::frame_snapshot`): the
//! unroller's per-copy frames, the field forward's pre-header and post-writer
//! frames, LICM's pre-header frame. When a later edit removes every node that
//! names such a copy -- the cleanup folds an unrolled copy's read, GVN merges a
//! pre-header read into an earlier one, the forward kills the in-loop read that
//! carried it -- the copy stays behind, unnamed, BEHIND the original at its
//! bci. `ir_verify::check_frame_states` then sees two unclaimed snapshots at
//! one emitted bci and reports it (`[ir-verify-each-pass] ... describe bci
//! 200 without being named`, AbstractSet.hashCode after the forward's cleanup;
//! `R21FuzzGen16.main` after the late unroll's cleanup). The pipeline's last
//! `drop_unnamed_copy_frames` collected the unroll's kind before lowering, but
//! nothing collected the others, and between passes the graph was ambiguous.
//!
//! # Why dropping one is unobservable
//!
//! A dropped snapshot here satisfies all of:
//!
//! * no live node names it (`frame_snapshot`), and it is no post-call exit's
//!   snapshot (`post_call_only`, or a `post_call_of` naming a live call);
//! * an EARLIER snapshot of the same kind (flat, or chain) sits at its bci.
//!
//! Every consumer that does not go through a node's name resolves a bci to
//! its FIRST snapshot of that kind (`ir_lower`'s `safepoint_first_by_bci` /
//! `splice_state_first_by_pc`, `preheader_snapshot`, `plan_copy_frames`), and
//! the deopt table keeps the first point per native offset
//! (`build_deopt_points`' stable sort + dedup). So nothing could ever read the
//! dropped snapshot. What dropping it changes is that its slots stop being DCE
//! roots and use-list entries (a value only the orphan named was kept alive
//! and given a home for nothing), and that the table builder no longer
//! resolves a frame nobody reads (whose dropped-home latch could refuse the
//! compile).
//!
//! # What is NOT collected
//!
//! A snapshot that was unnamed BEFORE the pass ran is left alone, so the
//! verifier still reports the case its rule exists for: a half-finished copy,
//! whose snapshot was pushed and whose nodes were never stamped with it (they
//! resolve by bci to the original, the wrong iteration's frame). Only a
//! snapshot the pass found NAMED, or one a caller says it pushed itself and
//! always names at the push (`pushed_since`), is a candidate.
//!
//! `CRATONVM_JIT_IR_DROP_ORPHANED_FRAME_COPIES=0` turns the collection off
//! (then the pipeline's last `drop_unnamed_copy_frames` is the only collector,
//! as before).

use crate::ir::{Graph, Op, NO_NODE};

/// `CRATONVM_JIT_IR_DROP_ORPHANED_FRAME_COPIES` (default ON, `=0` kills).
pub(super) fn drop_orphaned_frame_copies_enabled() -> bool {
    true}

/// One entry per snapshot of `graph`: is it NAMED -- by a live node's
/// `frame_snapshot`, or (a post-call exit's snapshot) by the live call its
/// `post_call_of` names?
pub(super) fn snapshot_named_mask(graph: &Graph) -> Vec<bool> {
    let mut named = vec![false; graph.safepoints.len()];
    for n in graph.nodes.iter() {
        if n.op == Op::Dead {
            continue;
        }
        if let Some(slot) = n.frame_snapshot.and_then(|si| named.get_mut(si as usize)) {
            *slot = true;
        }
    }
    for (si, sp) in graph.safepoints.iter().enumerate() {
        if sp.post_call_of != NO_NODE
            && graph
                .node_opt(sp.post_call_of)
                .is_some_and(|n| n.op != Op::Dead)
        {
            named[si] = true;
        }
    }
    named
}

/// Drop every snapshot that a pass orphaned (module notes): named in
/// `named_before` (a [`snapshot_named_mask`] taken before the pass), or at an
/// index `>= pushed_since` when given, unnamed now, not a post-call exit's,
/// and shadowed by an earlier snapshot of its own kind at its bci. Returns
/// how many were dropped.
///
/// Renumbers every `frame_snapshot` (`Graph::retain_safepoints`), so a caller
/// must hold no snapshot index across it.
pub(super) fn drop_orphaned_frame_copies(
    graph: &mut Graph,
    named_before: &[bool],
    pushed_since: Option<usize>,
) -> usize {
    let len = graph.safepoints.len();
    let candidate = |si: usize| is_candidate(si, named_before, pushed_since);
    if len < 2 || !(0..len).any(candidate) {
        return 0;
    }
    let keep = orphan_keep_mask(graph, candidate);
    // `retain_safepoints` answers 0 and changes nothing when all are kept.
    graph.retain_safepoints(&keep)
}

/// May snapshot `si` be collected at all: named before the pass, or pushed
/// by it (`pushed_since`)?
fn is_candidate(si: usize, named_before: &[bool], pushed_since: Option<usize>) -> bool {
    named_before.get(si).copied().unwrap_or(false) || pushed_since.is_some_and(|p| si >= p)
}

/// One entry per snapshot, `false` for each [`drop_orphaned_frame_copies`]
/// drops: a `candidate`, unnamed now, not a post-call exit's, and behind an
/// earlier snapshot of its own kind (flat / chain) at its bci.
fn orphan_keep_mask(graph: &Graph, candidate: impl Fn(usize) -> bool) -> Vec<bool> {
    let named_now = snapshot_named_mask(graph);
    // (bci, chain?) of every snapshot a bci-keyed consumer could find first.
    let mut seen: rustc_hash::FxHashSet<(usize, bool)> = rustc_hash::FxHashSet::default();
    let mut keep = vec![true; graph.safepoints.len()];
    for (si, sp) in graph.safepoints.iter().enumerate() {
        if sp.post_call_only {
            continue;
        }
        let shadowed = !seen.insert((sp.bci, sp.is_splice_state()));
        keep[si] = !(shadowed && candidate(si) && !named_now[si]);
    }
    keep
}

/// Run `pass` and collect the frame copies it orphaned: those it found named
/// and left unnamed, and those it pushed itself and left unnamed. For a pass
/// that stamps every node it makes at a copy's bci with that copy, or kills
/// the node -- the loop field forward and the cell promotions -- so no copy it
/// pushed can be a half-finished one (module notes). Off under
/// [`drop_orphaned_frame_copies_enabled`]`() == false`: `pass` alone.
pub(super) fn collecting_orphaned_frames<R>(
    graph: &mut Graph,
    pass: impl FnOnce(&mut Graph) -> R,
) -> R {
    if !drop_orphaned_frame_copies_enabled() {
        return pass(graph);
    }
    let named_before = snapshot_named_mask(graph);
    let pushed_since = graph.safepoints.len();
    let out = pass(graph);
    drop_orphaned_frame_copies(graph, &named_before, Some(pushed_since));
    out
}



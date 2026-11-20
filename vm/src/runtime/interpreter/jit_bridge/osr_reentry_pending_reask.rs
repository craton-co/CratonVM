// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 lane ONESHOT (`CRATONVM_JIT_OSR_REENTRY_PENDING_REASK`, opt-in;
//! page `docs/feature-designs/jit-osr-exit-and-recompile.md`):
//! a re-entry memo entry put while the worker still owed its optimizing OSR
//! verdict (`OsrReentryMemo::optimizing_pending`) re-asks that verdict now and
//! then, instead of waiting for an answer that may never come.
//!
//! The race. `try_osr`'s background arm queues an OSR task for the pc and
//! enters the single-pass body; the memo entry it puts is marked pending.
//! Every later hit asks `osr_reentry_memo_defers_to_optimizing`, which yields
//! only when the worker CACHED an accepted body, and clears the mark only
//! when the worker memoised a REFUSAL. A worker that declined for a transient
//! reason (a resolver miss, a transient compile decline, an unpinned baked
//! callee, a stale epoch, a dependency change: each "not memoised; retried
//! later"), or a request the manager refused because the OSR slot was held,
//! leaves neither. The entry then stays pending, every hit stands, and a hit
//! skips the whole optimizing arm, the only place that queues the task again.
//! Only a JIT-cache generation move or a deopt anywhere in the VM ends it.
//! The worker answering before the slot frees is the good order. The other
//! order leaves the loop on its single-pass body.
//!
//! The fix. Re-ask the worker on the 16th, 32nd, 64th, ... pending hit of one
//! entry ([`pending_hit_reasks`]): at most about 28 asks per entry over a
//! `u32` count. Each ask is the arm's own idempotent `request_osr`. The hit
//! still stands, so the re-entry stays as cheap as the memo makes it. Once the
//! worker caches a body, the next hit yields to it as before.

use super::*;

/// The first pending hit that re-asks. Small hits are the worker still
/// building: asking then would only be refused while the slot is held.
const FIRST_REASK_HIT: u32 = 16;

/// `CRATONVM_JIT_OSR_REENTRY_PENDING_REASK` (opt-in, `=1` enables): opt-in
/// because the extra worker builds it can cost were not measured. Read only on
/// a hit that would re-ask (cold).
fn pending_reask_enabled() -> bool {
    false}

/// Does pending hit number `n` (1-based) re-ask the worker? Pure.
pub(super) fn pending_hit_reasks(n: u32) -> bool {
    n >= FIRST_REASK_HIT && n.is_power_of_two()
}

/// Is the manager's answer to a re-ask final (the verdict is no longer owed)?
/// Only `Refused`: `Pending` is a held slot, `Queued` a build still to judge.
fn reask_answer_is_final(answer: &crate::jit::tiered::OsrRequest) -> bool {
    matches!(answer, crate::jit::tiered::OsrRequest::Refused)
}

/// A pending hit of the thread's current memo entry whose verdict is still
/// owed (nothing cached, nothing refused). The hit is counted on the entry
/// itself (`OsrReentryMemo::pending_hits`), so a new entry restarts the
/// count. `Some(queued)` when this hit re-asked the worker, `queued` saying
/// whether the manager took the task; `None` otherwise.
pub(super) fn reask_owed_optimizing_verdict(
    shared: &SharedVm,
    class_id: ClassId,
    (class_name, method_name, descriptor): (&str, &str, &str),
    entry_pc: usize,
) -> Option<bool> {
    // 0 without an entry: never a re-ask.
    let n = osr_reentry_memo_note_pending_hit();
    if !pending_hit_reasks(n) || !pending_reask_enabled() {
        return None;
    }
    // The pending mark is put only by the background arm, which started the
    // compiler, so this is that arm's request and nothing more.
    let answer = shared.jit.tiered_manager.request_osr_classified(
        &crate::jit::tiered::MethodKey::with_class_id(
            class_id,
            class_name,
            method_name,
            descriptor,
        ),
        // Cast: a bci fits u32 (`IR_MAX_BYTECODE_SIZE` is far below it).
        entry_pc as u32,
    );
    let queued = matches!(answer, crate::jit::tiered::OsrRequest::Queued(_));
    // j27 wave 8 (lane W8OSR): a `Refused` answer (OSR denied, `c2_bailout`,
    // retry budget spent, tiering off) is the manager's final verdict until a
    // redefinition or an install-epoch move, the same as a memoised refusal:
    // clear the mark so later hits stop probing and stop re-asking.
    if reask_answer_is_final(&answer) {
        osr_reentry_memo_set_optimizing_pending(false);
    }
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr reentry memo RE-ASKS the owed optimizing verdict \
             {class_name}.{method_name}{descriptor} pc={entry_pc} (pending hit #{n}): OSR task {}",
            if queued { "queued" } else { "not queued" }
        );
    }
    Some(queued)
}

#[cfg(test)]
mod tests;

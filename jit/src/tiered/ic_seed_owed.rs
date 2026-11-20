// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
//! j27 lane SEEDOWE (`docs/feature-designs/jit-ir-optimizations.md`):
//! the inline-cache profile seed's C2 retry (door `ic-profile-seed`), when the
//! tier manager refuses it for a reason that can lift, is OWED and paid by the
//! manager once that reason lifts, instead of waiting for a later seed event
//! that a monomorphic site never produces.
//!
//! Glossary:
//! - *owe*: remember one refused seed retry on the method's state, with the
//!   refusal that held it ([`OwedSeedRetry`]).
//! - *pay*: ask that retry again from the invocation verdict (interpreter
//!   stride or compiled-entry drain), once the refusal has lifted.
//! - *answered*: any method-entry C2 task admitted after the owe reads the
//!   seeded row, so it clears the owe ([`answered_by_admission`]).
//!
//! Bounds: at most [`MAX_OWED_IC_SEED_RETRIES`] owes per method per bytecode
//! (a redefinition resets it), so at most that many paid asks. A permanent
//! refusal (`c2_bailout`, `MakeNotCompilable`'s ineligible marks, a spent
//! retry budget, tiering disabled) never owes, and drops an owe it finds at
//! pay time. No timer: only the hotness count reaching the gate, or the
//! settled generation moving (room may have come back under the cap), lifts
//! an owe.

use std::sync::atomic::Ordering;

use super::{
    CompilationPriority, CompilationTask, CompilationTier, CompilerCore, InvocationVerdict,
    MethodState,
};

/// The retry door whose refusals are owed (`request_ic_seed_recompile`).
pub(super) const IC_SEED_DOOR: &str = "ic-profile-seed";

/// Owed seed retries per method per bytecode, paid or not.
pub const MAX_OWED_IC_SEED_RETRIES: u8 = 2;

/// `CRATONVM_JIT_IR_IC_SEED_OWED_RETRY` (j27 lane SEEDOWE; opt-in, `=1`
/// enables): owe a refused seed retry instead of dropping it. Opt-in because
/// the extra compile it buys could not be measured without a build. Read once
/// per refused seed retry (cold); no static.
pub fn ic_seed_owed_retry_enabled() -> bool {
    false}

/// The other one-shot C2 retry doors whose VM caller spends its claim BEFORE
/// the manager answers, and never asks again after a `Refused` (j27 wave 5,
/// lane PUBHUNT; `docs/feature-designs/jit-ir-optimizations.md`):
/// the deferred-`new` memo (`take_deferred_new_retry_if_resolvable`), the
/// static self-lock row (`request_static_self_lock_recompile` marks the body
/// asked), and the unenterable-OSR promotion (one build per header per
/// epoch). The same shape HANGHUNT fixed for the seed. Each string is the
/// `door` its `TieredCompilationManager` method passes to `request_c2_retry`.
pub(super) const ONE_SHOT_CLAIM_DOORS: [&str; 3] =
    ["deferred-new", "static self-lock", "osr-unenterable"];

/// `CRATONVM_JIT_ONE_SHOT_REFUSED_RETRY_OWED` (j27 wave 5, lane PUBHUNT;
/// opt-in, `=1` enables): owe a [`ONE_SHOT_CLAIM_DOORS`] retry refused by the
/// hotness gate or the code-cache cap, paid as the seed's is. Opt-in for the
/// seed switch's reason: the extra compile could not be measured without a
/// build. Read once per refused retry (cold); no static.
pub fn one_shot_refused_retry_owed_enabled() -> bool {
    false}

/// Does a liftable refusal of `door` owe its retry?
fn door_owes(door: &str) -> bool {
    if door == IC_SEED_DOOR {
        return ic_seed_owed_retry_enabled();
    }
    ONE_SHOT_CLAIM_DOORS.contains(&door) && one_shot_refused_retry_owed_enabled()
}

/// The refusal an owed seed retry waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OwedSeedRetry {
    /// `c2_upgrade_min_invocations`: paid once the count reaches the gate.
    HotnessGate,
    /// The code-cache cap: paid once the settled generation moves from the
    /// one read when it was refused (`note_body_withdrawn` /
    /// `note_code_cache_pressure` bump it when room may have come back).
    CodeCacheCap { generation: u32 },
}

/// Has the refusal `owed` waits on lifted? Pure.
pub(super) fn lifted(owed: OwedSeedRetry, gate: u64, invocations: u64, generation: u32) -> bool {
    match owed {
        OwedSeedRetry::HotnessGate => gate == 0 || invocations >= gate,
        OwedSeedRetry::CodeCacheCap { generation: at } => generation != at,
    }
}

/// Will no later state change admit a C2 retry of this method?
fn permanently_refused(state: &MethodState) -> bool {
    state.c2_bailout || state.ineligible || state.entry_ineligible || state.entry_out_of_retries()
}

/// Owe the refused retry of `door` when that door owes ([`door_owes`]: the
/// seed's under its switch, a [`ONE_SHOT_CLAIM_DOORS`] one under its own) and
/// the method's owes are not spent. A refusal while one is already owed
/// spends nothing: it only names the newest refusal. Returns whether it is
/// owed.
pub(super) fn owe(state: &mut MethodState, door: &str, owed: OwedSeedRetry) -> bool {
    if !door_owes(door) {
        return false;
    }
    record_owe(state, owed)
}

/// [`owe`] once the door is known to owe (also `pay`'s re-owe of a retry
/// that was owed already, whichever door owed it).
fn record_owe(state: &mut MethodState, owed: OwedSeedRetry) -> bool {
    if state.ic_seed_owed.is_none() {
        if state.ic_seed_owes >= MAX_OWED_IC_SEED_RETRIES {
            return false;
        }
        state.ic_seed_owes += 1;
    }
    state.ic_seed_owed = Some(owed);
    trace("OWED", state, owed);
    true
}

/// A method-entry C2 task was just admitted for `state`: it answers the owe.
pub(super) fn answered_by_admission(state: &mut MethodState, task: &CompilationTask) {
    if task.osr_bci.is_none() && task.target_tier == CompilationTier::C2 {
        state.ic_seed_owed = None;
    }
}

/// Pay `state`'s owed seed retry if its refusal lifted. Called under the
/// `methods` lock from the invocation verdict, with no slot held. `Some` when
/// a C2 task was queued.
pub(super) fn pay(core: &CompilerCore, state: &mut MethodState) -> Option<InvocationVerdict> {
    let owed = state.ic_seed_owed?;
    if core.tiering_is_disabled() || permanently_refused(state) {
        state.ic_seed_owed = None;
        trace("DROPPED (permanent refusal)", state, owed);
        return None;
    }
    let gate = core.c2_upgrade_min_invocations.load(Ordering::Relaxed);
    let generation = core.settled_generation.load(Ordering::Acquire);
    if !lifted(owed, gate, state.invocation_count, generation) {
        return None;
    }
    state.ic_seed_owed = None;
    if !core.admit_one_shot_c2(state) {
        trace("REFUSED (admit)", state, owed);
        if state.deferred_at_cap {
            record_owe(state, OwedSeedRetry::CodeCacheCap { generation });
        }
        return None;
    }
    trace("PAID", state, owed);
    Some(InvocationVerdict {
        recommended: Some(CompilationTier::C2),
        settled_generation: 0,
    })
}

fn trace(what: &str, state: &MethodState, owed: OwedSeedRetry) {
    if false {
        let key = &state.method_key;
        eprintln!(
            "[cratonvm-jitc] ic-profile-seed owed retry {what} {}.{}{}: {owed:?} invocations={} owes={}/{}",
            key.class_name,
            key.method_name,
            key.descriptor,
            state.invocation_count,
            state.ic_seed_owes,
            MAX_OWED_IC_SEED_RETRIES,
        );
    }
}

impl CompilerCore {
    /// The admission every one-shot C2 retry ends with: end a waiting-on-class
    /// park (the retry is the event it waits for), open the branch-profile
    /// window, and admit a low-priority method-entry C2 task. `false` when
    /// `admit` refused (the cap; a closed window is closed by `admit`).
    pub(super) fn admit_one_shot_c2(&self, state: &mut MethodState) -> bool {
        state.waiting_on_class_epoch = None;
        state.waiting_on_class_offers = 0;
        self.open_branch_window(state);
        let task = CompilationTask {
            method_key: state.method_key.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::Low,
            enqueue_time_ms: 0,
            osr_bci: None,
        };
        self.admit(state, task)
    }
}



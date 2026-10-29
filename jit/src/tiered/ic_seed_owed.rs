// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
//! j27 lane SEEDOWE (`docs/internal/jit/j27-ic-seed-released-claim-needs-a-later-seed-event-FIXED-20261029.md`):
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_IC_SEED_OWED_RETRY")
}

/// The other one-shot C2 retry doors whose VM caller spends its claim BEFORE
/// the manager answers, and never asks again after a `Refused` (j27 wave 5,
/// lane PUBHUNT; `docs/internal/jit/j27-one-shot-retry-claims-spent-on-a-refused-answer-FIXED-20261029.md`):
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_ONE_SHOT_REFUSED_RETRY_OWED")
}

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
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiered::{C2RetryAnswer, MethodKey, TieredCompilationManager};

    const SW: &str = "CRATONVM_JIT_IR_IC_SEED_OWED_RETRY";
    const GATE: u64 = 1_000;

    /// A C2 method below the hotness gate, the shape the seed's starved body
    /// is in when its ask is refused by the gate.
    fn starved_below_gate(name: &str) -> (TieredCompilationManager, MethodKey) {
        let mgr = TieredCompilationManager::with_default_policy();
        mgr.core.c2_upgrade_min_invocations.store(GATE, Ordering::Relaxed);
        let key = MethodKey::new("j27/SeedOwe", name, "()V");
        let mut methods = mgr.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        state.current_tier = CompilationTier::C2;
        state.invocation_count = 10;
        drop(methods);
        (mgr, key)
    }

    fn queued(mgr: &TieredCompilationManager, key: &MethodKey) -> bool {
        mgr.core.methods.lock()[key].queued_for_compilation
    }

    /// As if the paid compile finished: the slot is free again.
    fn free_slot(mgr: &TieredCompilationManager, key: &MethodKey) {
        mgr.core.methods.lock().get_mut(key).unwrap().queued_for_compilation = false;
    }

    fn on<R>(f: impl FnOnce() -> R) -> R {
        cratonvm_types::flags::with_thread_overrides(&[(SW, Some("1"))], f)
    }

    #[test]
    fn the_switch_is_opt_in() {
        let read = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(&[(SW, v)], ic_seed_owed_retry_enabled)
        };
        assert!(!read(None), "default OFF");
        assert!(read(Some("1")));
        assert!(!read(Some("0")));
    }

    #[test]
    fn a_gate_owe_lifts_at_the_gate_and_a_cap_owe_when_the_generation_moves() {
        let gate = OwedSeedRetry::HotnessGate;
        assert!(!lifted(gate, GATE, GATE - 1, 7));
        assert!(lifted(gate, GATE, GATE, 7));
        assert!(lifted(gate, 0, 0, 7), "a gate set to 0 since the owe");
        let cap = OwedSeedRetry::CodeCacheCap { generation: 7 };
        assert!(!lifted(cap, 0, u64::MAX, 7), "no state change: no re-ask");
        assert!(lifted(cap, 0, 0, 8));
    }

    #[test]
    fn with_the_switch_off_a_gate_refusal_is_dropped_as_before() {
        cratonvm_types::flags::with_thread_overrides(&[(SW, None)], || {
            let (mgr, key) = starved_below_gate("off");
            assert_eq!(mgr.request_ic_seed_recompile(&key), C2RetryAnswer::Refused);
            assert_eq!(mgr.core.methods.lock()[&key].ic_seed_owed, None);
            assert_eq!(mgr.on_observed_invocations(&key, GATE).recommended, None);
            assert!(!queued(&mgr, &key));
        });
    }

    #[test]
    fn a_gate_refusal_is_paid_when_the_count_reaches_the_gate() {
        on(|| {
            let (mgr, key) = starved_below_gate("paid");
            assert_eq!(mgr.request_ic_seed_recompile(&key), C2RetryAnswer::Refused);
            assert_eq!(mgr.on_observed_invocations(&key, GATE - 1).recommended, None);
            assert!(!queued(&mgr, &key), "below the gate: nothing asked");
            let v = mgr.on_observed_invocations(&key, GATE);
            assert_eq!(v.recommended, Some(CompilationTier::C2));
            assert!(queued(&mgr, &key));
            assert_eq!(mgr.core.methods.lock()[&key].ic_seed_owed, None, "paid once");
        });
    }

    #[test]
    fn owes_are_bounded_per_method() {
        on(|| {
            let (mgr, key) = starved_below_gate("bound");
            let mut gate = GATE;
            for _ in 0..MAX_OWED_IC_SEED_RETRIES {
                mgr.core.c2_upgrade_min_invocations.store(gate, Ordering::Relaxed);
                assert_eq!(mgr.request_ic_seed_recompile(&key), C2RetryAnswer::Refused);
                let v = mgr.on_observed_invocations(&key, gate);
                assert_eq!(v.recommended, Some(CompilationTier::C2), "owe paid");
                free_slot(&mgr, &key);
                gate += GATE;
            }
            // Spent: the next refusal is dropped, nothing pays it.
            mgr.core.c2_upgrade_min_invocations.store(gate, Ordering::Relaxed);
            assert_eq!(mgr.request_ic_seed_recompile(&key), C2RetryAnswer::Refused);
            assert_eq!(mgr.on_observed_invocations(&key, gate).recommended, None);
            assert!(!queued(&mgr, &key));
            // Repeated refusals before a pay owe one retry, not one each.
            let (mgr, key) = starved_below_gate("storm");
            for _ in 0..10 {
                assert_eq!(mgr.request_ic_seed_recompile(&key), C2RetryAnswer::Refused);
            }
            assert_eq!(mgr.core.methods.lock()[&key].ic_seed_owes, 1);
            assert_eq!(mgr.on_observed_invocations(&key, GATE).recommended, Some(CompilationTier::C2));
            free_slot(&mgr, &key);
            assert_eq!(mgr.on_observed_invocations(&key, GATE + 1).recommended, None, "one pay");
        });
    }

    #[test]
    fn a_permanent_refusal_never_owes_and_drops_an_owe() {
        on(|| {
            let (mgr, key) = starved_below_gate("bailout-first");
            mgr.core.methods.lock().get_mut(&key).unwrap().c2_bailout = true;
            assert_eq!(mgr.request_ic_seed_recompile(&key), C2RetryAnswer::Refused);
            assert_eq!(mgr.core.methods.lock()[&key].ic_seed_owed, None);

            let (mgr, key) = starved_below_gate("bailout-after");
            assert_eq!(mgr.request_ic_seed_recompile(&key), C2RetryAnswer::Refused);
            mgr.core.methods.lock().get_mut(&key).unwrap().ineligible = true;
            let _ = mgr.on_observed_invocations(&key, GATE);
            assert!(!queued(&mgr, &key), "MakeNotCompilable: never re-asked");
            assert_eq!(mgr.core.methods.lock()[&key].ic_seed_owed, None, "dropped");
        });
    }

    #[test]
    fn only_the_seed_door_owes() {
        on(|| {
            let (mgr, key) = starved_below_gate("late-bind");
            assert_eq!(mgr.request_late_bind_recompile(&key), C2RetryAnswer::Refused);
            assert_eq!(mgr.core.methods.lock()[&key].ic_seed_owed, None);
        });
    }

    #[test]
    fn another_admitted_c2_compile_answers_the_owe() {
        on(|| {
            let (mgr, key) = starved_below_gate("answered");
            assert_eq!(mgr.request_ic_seed_recompile(&key), C2RetryAnswer::Refused);
            mgr.core.c2_upgrade_min_invocations.store(0, Ordering::Relaxed);
            assert_eq!(mgr.request_late_bind_recompile(&key), C2RetryAnswer::Enqueued);
            assert_eq!(mgr.core.methods.lock()[&key].ic_seed_owed, None);
        });
    }

    /// j27 wave 5, lane PUBHUNT: the one-shot claim doors.
    const ONE_SHOT_SW: &str = "CRATONVM_JIT_ONE_SHOT_REFUSED_RETRY_OWED";

    fn one_shot<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
        cratonvm_types::flags::with_thread_overrides(&[(SW, None), (ONE_SHOT_SW, value)], f)
    }

    /// Each door through its own manager method, so a renamed door string
    /// fails here rather than silently leaving the list.
    fn ask_each_door(mgr: &TieredCompilationManager, key: &MethodKey, door: usize) {
        match door {
            0 => assert_eq!(mgr.request_deferred_new_retry(key), C2RetryAnswer::Refused),
            1 => mgr.request_self_lock_recompile(key),
            _ => mgr.request_osr_unenterable_promotion(key),
        }
    }

    #[test]
    fn the_one_shot_switch_is_opt_in() {
        assert!(!one_shot(None, one_shot_refused_retry_owed_enabled), "default OFF");
        assert!(one_shot(Some("1"), one_shot_refused_retry_owed_enabled));
        assert!(!one_shot(Some("0"), one_shot_refused_retry_owed_enabled));
    }

    #[test]
    fn a_one_shot_doors_gate_refusal_is_paid_at_the_gate_with_the_switch() {
        for door in 0..ONE_SHOT_CLAIM_DOORS.len() {
            one_shot(Some("1"), || {
                let (mgr, key) = starved_below_gate(ONE_SHOT_CLAIM_DOORS[door]);
                ask_each_door(&mgr, &key, door);
                assert!(!queued(&mgr, &key), "refused below the gate");
                let v = mgr.on_observed_invocations(&key, GATE);
                assert_eq!(v.recommended, Some(CompilationTier::C2), "door {door} paid");
                assert!(queued(&mgr, &key));
            });
        }
    }

    #[test]
    fn without_the_switch_a_one_shot_doors_refusal_is_lost_as_before() {
        for door in 0..ONE_SHOT_CLAIM_DOORS.len() {
            one_shot(None, || {
                let (mgr, key) = starved_below_gate(ONE_SHOT_CLAIM_DOORS[door]);
                ask_each_door(&mgr, &key, door);
                assert_eq!(mgr.core.methods.lock()[&key].ic_seed_owed, None);
                assert_eq!(mgr.on_observed_invocations(&key, GATE).recommended, None);
                assert!(!queued(&mgr, &key));
            });
        }
    }

    #[test]
    fn the_seed_switch_alone_does_not_owe_the_other_doors() {
        on(|| {
            let (mgr, key) = starved_below_gate("seed-switch-only");
            assert_eq!(mgr.request_deferred_new_retry(&key), C2RetryAnswer::Refused);
            assert_eq!(mgr.core.methods.lock()[&key].ic_seed_owed, None);
        });
    }
}

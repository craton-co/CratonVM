// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
//! j27 wave 6, lane T1TIERMGR (TIERMGR21-1 of
//! `docs/feature-designs/wire-tiered-manager.md`): one
//! owner per owed C2 retry.
//!
//! A C2 retry the manager answers `AlreadyQueued` is owed: somebody must ask
//! it again once the task holding the slot is done, because that task may
//! have read the method before the event the retry answers. Today two parties
//! can own the same debt:
//!
//! - the manager: [`super::MethodState::late_bind_retry_owed`] for the
//!   late-bind doors (`CRATONVM_JIT_LATE_BIND_RETRY_OWED`, default ON) and
//!   [`super::MethodState::retry_owed`] for every other door
//!   (`CRATONVM_JIT_TIER_RETRY_OWED`);
//! - the VM: `JitBridgeState::late_bind_owed` (`vm/src/runtime/interpreter/jit_bridge.rs`),
//!   filled by EAGER19-3's older-publication path, EAGER2-1's
//!   refused-put restore and round 21's one-shot latches
//!   (`owe_one_shot_retry_if_already_queued`), and asked as the worker's
//!   `deferred-new` retry.
//!
//! Both owning one debt costs a redundant optimizing compile: the VM's ask
//! (`deferred-new`) is `Enqueued`, then the manager's `late-bind-owed` ask
//! finds that new compile in the slot, answers `AlreadyQueued` and owes AGAIN,
//! so a third compile runs after the second -- which was dispatched after the
//! event and had already answered it.
//!
//! The one decision ("does the manager own this answer?") is
//! [`manager_owes_answer`]; the VM copies ask it before they remember
//! anything. With [`one_owed_retry_enabled`] the late-bind copies stand down
//! for a debt the manager owns, and the manager's own late-bind bit is
//! settled by an optimizing compile the worker queued after the task
//! ([`CompilerCore::ask_late_bind_owed`]), exactly as `retry_owed` already is.
//! No new owe state: this module only removes duplicates of the existing ones
//! (SEEDOWE / PUBHUNT's liftable-refusal owe in `ic_seed_owed` is a different
//! answer, `Refused`, and is untouched).

use super::{
    late_bind_retry_owed_enabled, tier_retry_owed_enabled, C2RetryAnswer, CompilerCore, MethodKey,
    LATE_BIND_RETRY_DOORS,
};

/// `CRATONVM_JIT_ONE_OWED_RETRY` (j27 wave 6, lane T1TIERMGR, TIERMGR21-1;
/// default ON since 2026-10-28 (j27 WARNING flip; =0 restores)): an
/// `AlreadyQueued` C2 retry has ONE owner. The VM's
/// late-bind owe copies stand down when the manager owns the debt
/// ([`manager_owes_answer`]), and the manager's late-bind owed bit is
/// answered by an optimizing compile the worker already queued after the
/// task. Was opt-in until the flip (the compile count it removes was
/// unmeasured). Read once per owed answer / per completion that holds a
/// late-bind bit (cold); no static.
pub fn one_owed_retry_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true}

/// Does the tier manager itself remember a `door` C2 retry it answered
/// `AlreadyQueued`? The late-bind doors under
/// `CRATONVM_JIT_LATE_BIND_RETRY_OWED`, every other door under
/// `CRATONVM_JIT_TIER_RETRY_OWED` (`request_c2_retry`'s two owe arms, in the
/// same order). `door` is the manager's door name; a VM latch name that is
/// not a late-bind door (`deferred-new`, `ir-ic-profile-seed`) reads the
/// general arm, as the manager does.
pub fn manager_owes_already_queued(door: &str) -> bool {
    if LATE_BIND_RETRY_DOORS.contains(&door) && late_bind_retry_owed_enabled() {
        return true;
    }
    tier_retry_owed_enabled()
}

/// Did the manager take ownership of `answer`, the reply to a `door` retry?
/// Only an `AlreadyQueued` answer is owed at all; `Enqueued` needs no owe and
/// `Refused` is `ic_seed_owed`'s business. A VM-side owe copy remembers the
/// request only when this is `false`.
pub fn manager_owes_answer(answer: C2RetryAnswer, door: &str) -> bool {
    answer == C2RetryAnswer::AlreadyQueued && manager_owes_already_queued(door)
}

/// Should the worker ask the late-bind owed retry it just took, given that
/// the post-completion requests before it already queued an optimizing
/// compile (`queued_c2`)? Such a compile was admitted after the bit was set,
/// so it is dispatched after the event and answers it; asking would only find
/// it in the slot and owe a third compile. Without [`one_owed_retry_enabled`]
/// the bit is asked as before.
pub(super) fn late_bind_owed_ask_needed(queued_c2: bool) -> bool {
    !(queued_c2 && one_owed_retry_enabled())
}

impl CompilerCore {
    /// The worker's post-completion late-bind owed ask for `key`'s
    /// method-entry task: take [`super::MethodState::late_bind_retry_owed`]
    /// and ask it as the `late-bind-owed` door unless an optimizing compile
    /// queued by this completion answers it ([`late_bind_owed_ask_needed`]).
    /// Returns whether the ask queued an optimizing compile. Call with
    /// `methods` NOT held, after `finish`.
    pub(super) fn ask_late_bind_owed(&self, key: &MethodKey, queued_c2: bool) -> bool {
        if !self.take_late_bind_retry_owed(key) {
            return false;
        }
        if !late_bind_owed_ask_needed(queued_c2) {
            if false {
                eprintln!(
                    "[cratonvm-jitc] late-bind-owed retry {}.{}{}: answered by the optimizing \
                     compile this completion queued",
                    key.class_name, key.method_name, key.descriptor
                );
            }
            return false;
        }
        self.request_c2_retry(key, "late-bind-owed") == C2RetryAnswer::Enqueued
    }
}



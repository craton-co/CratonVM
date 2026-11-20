// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 7 (lane T1LAM3; page
//! `docs/feature-designs/real-frame-deopt.md`;
//! `CRATONVM_JIT_STALE_TRAP_NOT_ENTRANT`, **opt-in**): a trap out of a body
//! the JIT cache no longer publishes makes THAT body not entrant.
//!
//! The call-site service (`thread.rs`, the precise resume of a trapped
//! callee) already recognises such a trap (`trapped_a_superseded_body`: the
//! stash names a body other than the published one) and, rightly, neither
//! charges it nor evicts the published body. But nothing withdrew the stale
//! body itself: the baked `CALL` that reached it (an optimizing direct bind
//! has no retire cell; a superseded C1 body whose forward the entry-ABI check
//! refused stays entrant on purpose) enters it again on the next call, traps
//! again, and is resumed interpreted again -- one trap per call, the
//! `evict9` `TryLoop` shape. Making it not entrant (the deopt eviction's own
//! mechanism, `make_trap_evicted_body_not_entrant`) sends the next call
//! through the not-entrant stub: forwarded to the current body, or
//! re-dispatched by name. Once per stale body; the trap that found it is
//! still resumed as before.

use std::sync::Arc;

use cratonvm_jit::CompiledMethod;
use cratonvm_types::ClassId;

use crate::vm::SharedVm;

/// The body the stash named for the trap at `point_addr`
/// (`cratonvm_jit::deopt::taken_trapping_body`), held as a [`RetainedCode`]
/// so its release goes through the retirement queue
/// (`published_code_free_audit`). The stash keeps only a `Weak`, so the
/// upgraded `Arc` may be the body's LAST owner. j27 wave 7 (lane T1LAM3):
/// the one way the VM takes that body; a source pin keeps it so
/// (`tests::taken_trapping_body_is_only_taken_retained`).
///
/// [`RetainedCode`]: cratonvm_jit::RetainedCode
pub fn taken_trapping_body_retained(point_addr: usize) -> Option<cratonvm_jit::RetainedCode> {
    cratonvm_jit::deopt::taken_trapping_body(point_addr).map(cratonvm_jit::RetainedCode::new)
}

/// The switch: opt-in until the probe's trap counts are measured. Read per
/// trap from a stale body (cold), never cached.
pub fn enabled() -> bool {
    true
}

/// The decision, on what the trap service knows.
///
/// `named`: the stash named the trapping body, as `(it is the body the
/// cache publishes now, it is already not entrant)`; `None` when it did not.
/// `own_source`: the trap came out of a body compiled before a redefinition
/// (the redefinition patches those itself). `sibling`: the trapping body is
/// the live body of its own (receiver) key, evicted by that key's path.
pub fn should_patch(
    enabled: bool,
    own_source: bool,
    sibling: bool,
    named: Option<(bool, bool)>,
) -> bool {
    enabled
        && !own_source
        && !sibling
        && matches!(named, Some((false, false)))
}

/// The stale body to make not entrant for a trap at `stashed_point` of the
/// method `(class, method, descriptor, class_id)`, or `None`
/// ([`should_patch`]). The body is the one the stash named, compared by
/// pointer with the one the cache publishes; an empty slot answers `None`
/// (nothing says the trapping body is stale).
#[allow(clippy::too_many_arguments)]
pub fn body_to_patch(
    vm: &SharedVm,
    stashed_point: usize,
    own_source: bool,
    sibling: bool,
    class: &str,
    method: &str,
    descriptor: &str,
    class_id: ClassId,
) -> Option<Arc<CompiledMethod>> {
    // Both handles are `RetainedCode`: released through the retirement
    // queue, never dropped bare.
    let named = cratonvm_jit::deopt::stash_names_trapping_body_enabled()
        .then(|| taken_trapping_body_retained(stashed_point))
        .flatten()?;
    let live = vm
        .jit
        .jit_cache
        .read()
        .get(class, method, descriptor, class_id)
        .map(cratonvm_jit::RetainedCode::new)?;
    let is_live = Arc::ptr_eq(live.arc(), named.arc());
    should_patch(true, own_source, sibling, Some((is_live, named.is_not_entrant())))
        .then(|| Arc::clone(named.arc()))
}

#[cfg(test)]
mod stale_trap_not_entrant_tests;

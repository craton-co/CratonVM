// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 6 (lane T1OSRREENTRY, book
//! `docs/internal/proposals/jit/tier1/jit-r21-osrreentry-proposals.md`,
//! OSRREENTRY21-1): an OSR entry asks for the method-entry compile itself.
//!
//! HotSpot's `CompilationPolicy::method_back_branch_event` asks the
//! method-entry predicate on every back-edge event, so a method that loops in
//! OSR code still gets its normal-entry body. Here the method-entry nomination
//! of a method whose activations run in OSR code came only from the frame
//! pop's loop-work credit (`credit_loop_work`): at the counter's crossing of
//! the invocation threshold, or at a retry stride of 64 counts. A method whose
//! count is already past the threshold and whose method-entry body is missing
//! -- the nomination was dropped or refused, or a trap evicted the body -- was
//! asked again only when the credit arithmetic happened to land on a stride.
//!
//! Under `CRATONVM_JIT_OSR_ENTRY_NOMINATES=1` the background arm's OSR entry
//! (`try_osr_offer`, just before `try_osr` enters the cached body) offers such
//! a method to the tiered manager through the interpreter's ordinary tier-up
//! door (`jit_bridge::offer_invocation_to_tiered_manager`: settled stamp,
//! breakpoint and finalizer bars, `ldc` slot mint), one activation earlier than
//! the pop and independent of the stride. The compile runs on the worker while
//! the OSR body runs the activation.

use crate::runtime::frame::Frame;
use crate::vm::SharedVm;

/// `CRATONVM_JIT_OSR_ENTRY_NOMINATES` (perf, opt-in, since 2026-10-28): read
/// once per background-arm OSR entry, never on a back edge.
pub(super) const OSR_ENTRY_NOMINATES_KEY: &str = "CRATONVM_JIT_OSR_ENTRY_NOMINATES";

/// Is the nomination armed? Only with background compilation, whose worker
/// compiles what the door queues (the inline arm never reaches here with a
/// body to run beside).
#[inline]
pub(super) fn osr_entry_nomination_armed() -> bool {
    crate::runtime::env_cache::bg_compile()
        && cratonvm_types::flags::runtime_flag_on(OSR_ENTRY_NOMINATES_KEY)
}

/// The decision: a method whose profile-store `count` reached `threshold` is
/// nominated unless a method-entry body is already published for it
/// (`has_entry_body`, asked only past the threshold).
#[inline]
pub(super) fn osr_entry_nominates(
    count: u32,
    threshold: u32,
    has_entry_body: impl FnOnce() -> bool,
) -> bool {
    count >= threshold && !has_entry_body()
}

/// Offer `f`'s method for its method-entry compile from the OSR entry that is
/// about to run its loop (OSRREENTRY21-1). Returns the tier the manager queued,
/// if any. An `Owned` frame (no shared method entry to carry the settled
/// stamp) is left to the pop's credit, as before.
pub(super) fn nominate_method_entry_at_osr_entry(
    shared: &SharedVm,
    f: &Frame,
) -> Option<crate::jit::tiered::CompilationTier> {
    if !osr_entry_nomination_armed() {
        return None;
    }
    let cached = f.cached_method()?;
    let count = shared.jit.profile_store.invocation_count(cached.invoc_key());
    let threshold = shared.config.jit_invocation_threshold;
    let due = osr_entry_nominates(count, threshold, || {
        shared
            .jit
            .jit_cache
            .read()
            .get(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
                cached.declaring_class_id,
            )
            .is_some()
    });
    if !due {
        return None;
    }
    super::jit_bridge::ensure_bg_compiler_started(shared);
    let tier =
        super::jit_bridge::offer_invocation_to_tiered_manager(shared, cached, u64::from(count));
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr entry nominates {}.{}{} count={count} queued={tier:?}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
        );
    }
    tier
}

#[cfg(test)]
mod tests {
    use super::{
        nominate_method_entry_at_osr_entry, osr_entry_nomination_armed, osr_entry_nominates,
        OSR_ENTRY_NOMINATES_KEY,
    };
    use crate::config::VmConfig;
    use crate::runtime::frame::Frame;
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;

    /// Below the threshold the published-body question is never asked; at or
    /// past it a published method-entry body stands the nomination down.
    #[test]
    fn only_a_method_past_the_threshold_without_an_entry_body_is_nominated() {
        assert!(!osr_entry_nominates(499, 500, || panic!(
            "below the threshold the cache must not be read"
        )));
        assert!(osr_entry_nominates(500, 500, || false));
        assert!(osr_entry_nominates(9_000, 500, || false));
        assert!(!osr_entry_nominates(500, 500, || true));
    }

    /// Opt-in: off unless the switch is set (and background compilation is
    /// on, the default).
    #[test]
    fn the_nomination_is_opt_in() {
        assert!(!cratonvm_types::flags::with_thread_overrides(
            &[(OSR_ENTRY_NOMINATES_KEY, None)],
            osr_entry_nomination_armed
        ));
        if !crate::runtime::env_cache::bg_compile() {
            return;
        }
        assert!(cratonvm_types::flags::with_thread_overrides(
            &[(OSR_ENTRY_NOMINATES_KEY, Some("1"))],
            osr_entry_nomination_armed
        ));
    }

    /// An `Owned` frame carries no method entry for the settled stamp, so it
    /// is never offered from here, even past the threshold with the switch on.
    #[test]
    fn an_owned_frame_is_left_to_the_pop() {
        use crate::jit::profile::LOOP_WORK_PER_INVOCATION;
        let shared = SharedVm::new(VmConfig::default());
        let frame = Frame::new(
            ClassId::new(0x27e1),
            "cratonvm/test/J27OsrEntryNominateHolder".to_string(),
            "spin".to_string(),
            "(I)J".to_string(),
            None,
            vec![0xb1],
            vec![],
            1,
            1,
            &[],
        );
        let key = cratonvm_jit_api::invoc_key_parts(0x27e1, "spin", "(I)J");
        let threshold = shared.config.jit_invocation_threshold;
        let iterations = LOOP_WORK_PER_INVOCATION.saturating_mul(threshold.saturating_add(1));
        let _ = shared.jit.profile_store.add_loop_work(key, iterations);
        assert!(
            shared.jit.profile_store.invocation_count(key) >= threshold,
            "precondition: the method is past the threshold"
        );
        let tier = cratonvm_types::flags::with_thread_overrides(
            &[(OSR_ENTRY_NOMINATES_KEY, Some("1"))],
            || nominate_method_entry_at_osr_entry(&shared, &frame),
        );
        assert!(tier.is_none());
    }

    /// Source pin: the nomination sits after the background arm (so only an
    /// offer that found an enterable body reaches it) and before `try_osr`.
    #[test]
    fn the_offer_nominates_after_the_background_arm_and_before_the_entry() {
        let src = include_str!("../interpreter.rs").replace("\r\n", "\n");
        let body = src
            .split("\nfn try_osr_offer(")
            .nth(1)
            .and_then(|rest| rest.split("\n\u{7d}\n").next())
            .expect("try_osr_offer");
        let request = body.find("request_osr_classified").expect("the request");
        let wait = body
            .find(".record_osr_background_pending();")
            .expect("the background wait");
        let nominate = body
            .find("osr_entry_nominate::nominate_method_entry_at_osr_entry(")
            .expect("the nomination");
        let entry = body.find("let osr_result = try_osr(").expect("the entry");
        assert!(request < wait && wait < nominate && nominate < entry);
    }

    /// OSRREENTRY21-3's census (lives with this book's other pins): the
    /// early-window miss is counted only for an ask the window let through,
    /// and only when the cache answered "no body".
    #[test]
    fn the_early_window_miss_is_counted_only_inside_the_window() {
        let src = include_str!("../interpreter.rs").replace("\r\n", "\n");
        let body = src
            .split("\nfn osr_early_reentry_due(")
            .nth(1)
            .and_then(|rest| rest.split("\n\u{7d}\n").next())
            .expect("osr_early_reentry_due");
        let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
        let window = flat
            .find("if !osr_early_reentry_window(")
            .expect("the window gate");
        let ask = flat
            .find("let enterable = osr_cached_body_enterable(")
            .expect("the cache ask");
        let miss = flat
            .find("if !enterable { cratonvm_jit::metrics::record_osr_event(\"osr_early_window_miss\");")
            .expect("the miss row");
        assert!(window < ask && ask < miss);
        assert!(cratonvm_jit::metrics::OSR_EVENTS.contains(&"osr_early_window_miss"));
    }
}

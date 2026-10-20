// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Why a blocked compiled peer's helper window REFUSED the collection
//! (gch h3/fd, step 1 of
//! `docs/internal/gc/gcd-d9c-proposal-refusing-helper-window-census-and-precise-blocking-deposits-CLOSED-20261015.md`).
//!
//! A refusing window costs a JIT-warm Generational pause twice (no young copy,
//! and through `gc_quiescence::unrewritable_peer_state` no promotion either),
//! and the run records none of the reasons. The helper-window passes of both
//! OS arms (`xt_root_scan`) call [`note_helper_window_refusal`] /
//! [`note_unreadable_helper_peer`] at their refusal points; under
//! `CRATONVM_DBG_XT_REFUSAL_CENSUS` (token `xt-refusal-census`, default off)
//! each refusal prints one line
//!
//! ```text
//! [xt-refusal] arm=<win|linux> tid=<os tid> reason=<token>
//! ```
//!
//! so a whole-file count by reason (and `sort -u` on `tid=`, a persistent
//! refuser being one tid with N lines) tells step 2 which fix to build. No
//! counter, no static: the lines are the census (the statics ratchet forbids
//! a new process global, and the counts are only ever read off a `.err`).

/// One reason a helper window refused (the page's four, with reason 2 split
/// by what the peer published, and the pin switch named apart).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HelperWindowRefusal {
    /// Reason 1: the machine band was cut (`snapshot_*`: `complete = false`).
    BandIncomplete,
    /// Reason 2a: the peer never published a shadow window
    /// (`gc_quiescence::shadow_window_of_tid` is `None`): the entry door that
    /// ran compiled code without `publish_self_shadow_addr_once`.
    ShadowUnpublished,
    /// Reason 3: the window holds INDIRECT entries and the peer published no
    /// stack band (`BandlessIndirect`).
    ShadowBandless,
    /// Reason 2b: a published window that failed its identity or `top`
    /// check (stale or torn).
    ShadowUntrusted,
    /// The pin itself is switched off (`CRATONVM_XT_HELPER_WINDOW_PIN=0`).
    PinOff,
    /// Reason 4: the peer could not be read at all (`Snapshot::Failed`, an
    /// empty band with no JIT address in its registers, or a cancelled signal).
    Unreadable,
}

impl HelperWindowRefusal {
    /// The `reason=` token of the census line.
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::BandIncomplete => "band-incomplete",
            Self::ShadowUnpublished => "shadow-unpublished",
            Self::ShadowBandless => "shadow-bandless",
            Self::ShadowUntrusted => "shadow-untrusted",
            Self::PinOff => "pin-off",
            Self::Unreadable => "unreadable",
        }
    }
}

/// The pure rule: why a window the pass found (a JIT address on a blocked
/// peer's stack) was not pinned, or `None` when it was. `shadow_ok` is the
/// pass's own verdict on the peer's shadow window; `shadow_published` /
/// `stack_band_published` say what the peer had published, which splits a
/// refused shadow window three ways. The first failing gate wins, in the
/// pass's own order (pin switch, shadow window, band).
pub(crate) fn classify_helper_window_refusal(
    pin_enabled: bool,
    shadow_ok: bool,
    band_complete: bool,
    shadow_published: bool,
    stack_band_published: bool,
) -> Option<HelperWindowRefusal> {
    if !pin_enabled {
        Some(HelperWindowRefusal::PinOff)
    } else if !shadow_ok {
        Some(if !shadow_published {
            HelperWindowRefusal::ShadowUnpublished
        } else if !stack_band_published {
            HelperWindowRefusal::ShadowBandless
        } else {
            HelperWindowRefusal::ShadowUntrusted
        })
    } else if !band_complete {
        Some(HelperWindowRefusal::BandIncomplete)
    } else {
        None
    }
}

/// Is the refusal census on? Read per refusal (a pause sees a handful), so no
/// cache and no static. Also gates the take-over driver's
/// `[xt-promote-licence]` line (gci i3/tk), one per refusing pause with a
/// promotion switch on.
pub(crate) fn census_on() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_XT_REFUSAL_CENSUS")
}

/// A window the pass found and did not pin: under the census, classify it
/// (reading what the peer published) and print its line. Called by both arms'
/// `helper_window_pass` in the `unpinned_windows += 1` arm. A window every
/// gate passes (a caller whose own gate differs from this rule) prints
/// `reason=unclassified`, which is itself a finding.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) fn note_helper_window_refusal(
    arm: &str,
    tid: u32,
    pin_enabled: bool,
    shadow_ok: bool,
    band_complete: bool,
) {
    if !census_on() {
        return;
    }
    let window = cratonvm_gc::gc_quiescence::shadow_window_of_tid(tid).is_some();
    let band = cratonvm_gc::shadow_stack::thread_stack_band(tid).is_some();
    let reason =
        classify_helper_window_refusal(pin_enabled, shadow_ok, band_complete, window, band)
            .map_or("unclassified", HelperWindowRefusal::token);
    // gch h3/fd (vh3-fd block C: 67 `shadow-unpublished` refusals of ONE tid):
    // `depth=None` says the peer's JIT-depth slot is gone too -- both are
    // dropped together by `unregister_jit_depth_slot`, i.e. at the thread's
    // TLS teardown -- while `depth=Some(_)` says it registered and never
    // published a window (an entry door that skipped `set_jit_thread`).
    // `band=` is the stack band published beside the window.
    eprintln!(
        "[xt-refusal] arm={arm} tid={tid} reason={reason} depth={:?} band={band}",
        cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid)
    );
}

/// A blocked peer the pass could not read at all (reason 4): under the
/// census, its line. `why` names the arm's own path (`snapshot-failed`,
/// `empty-band`, `cancelled`) after the reason token.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) fn note_unreadable_helper_peer(arm: &str, tid: u32, why: &str) {
    if census_on() {
        eprintln!(
            "[xt-refusal] arm={arm} tid={tid} reason={} why={why}",
            HelperWindowRefusal::Unreadable.token()
        );
    }
}

#[cfg(test)]
mod gch_h3fd_helper_window_refusal_tests {
    use super::{classify_helper_window_refusal as classify, HelperWindowRefusal as R};

    /// gch h3/fd (d9c step 1): one reason per failing gate, in the pass's
    /// order; a window every gate passes is no refusal.
    #[test]
    fn gch_h3fd_each_refusing_gate_names_its_reason() {
        assert_eq!(
            classify(true, true, true, true, true),
            None,
            "a pinned window"
        );
        assert_eq!(classify(false, true, true, true, true), Some(R::PinOff));
        assert_eq!(
            classify(false, false, false, false, false),
            Some(R::PinOff),
            "first gate wins"
        );
        assert_eq!(
            classify(true, false, true, false, true),
            Some(R::ShadowUnpublished)
        );
        assert_eq!(
            classify(true, false, true, false, false),
            Some(R::ShadowUnpublished)
        );
        assert_eq!(
            classify(true, false, true, true, false),
            Some(R::ShadowBandless)
        );
        assert_eq!(
            classify(true, false, true, true, true),
            Some(R::ShadowUntrusted)
        );
        assert_eq!(
            classify(true, false, false, true, true),
            Some(R::ShadowUntrusted),
            "shadow before band"
        );
        assert_eq!(
            classify(true, true, false, true, true),
            Some(R::BandIncomplete)
        );
    }

    /// Every reason has its own token (the host greps count by token).
    #[test]
    fn gch_h3fd_refusal_tokens_are_distinct() {
        let all = [
            R::BandIncomplete,
            R::ShadowUnpublished,
            R::ShadowBandless,
            R::ShadowUntrusted,
            R::PinOff,
            R::Unreadable,
        ];
        let mut tokens: Vec<&str> = all.iter().map(|r| r.token()).collect();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), all.len());
    }

    /// The census is default OFF: nothing printed, the classification skipped.
    #[test]
    fn gch_h3fd_the_refusal_census_is_default_off() {
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DBG_XT_REFUSAL_CENSUS", None)],
            super::census_on,
        );
        assert!(!on);
    }
}

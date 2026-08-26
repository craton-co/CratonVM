// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Whether a `Map.values()` / `entrySet()` view can still be minted under the
//! class `java/util/ArrayList` itself, rather than under its own carrier class.
//!
//! # What this guards
//!
//! `java/util/ArrayList` is on `real_protected_stub_class_common`, so the
//! five-term `SyntheticStub` predicate arbitrates its accessors and answers
//! "real bytecode wins" — worth 8-24x (`probes/AlGetDoorProbe`). That is only
//! sound while no `Map` view is an `ArrayList`: a view stashes its source map in
//! the last capacity slot of its element array and `native_al_*` re-syncs
//! against it on read, which the real `ArrayList` body cannot do. Serving a view
//! from real bytecode returns whatever the source held when the view was
//! created — the `Schema.getAllSequences()` shape (H2 `TestAlter`).
//!
//! Views have been minted under `MAP_VIEW_CARRIERS` (`java/util/HashMap$Values`
//! and friends) since 2026-08-13, and those carriers are NOT allow-listed and
//! ARE force-listed, so they keep their natives. Exactly one producer of the old
//! shape survives: `alloc_view_carrier`'s last-resort arm, which degrades to
//! `java/util/ArrayList` when the carrier class cannot be had at all — a
//! stripped image, a `--jdk-only` policy refusal, or a synthetic-JDK build that
//! has not bootstrapped the carriers.
//!
//! # Why a process-global, and why THIS default
//!
//! `real_protected_stub_class_common` is a pure function of a class name; it
//! takes no `SharedVm`, and both dispatch paths read it. So the question has to
//! reach it as a process-global, the same way `enforce_shadow_scope()` reaches
//! `force_native_over_real_jdk_bytecode`.
//!
//! The default is **"no such view exists"**, i.e. the yield is ON, and that
//! choice is deliberate. The alternative — default OFF, switched on by the first
//! successful carrier mint — was built first and measured INERT: a program that
//! never calls `values()` never publishes anything, so it never yields, and the
//! answer is memoized per call site the moment each site warms. Most of a
//! program's `ArrayList.size()` sites warm long before its first `values()`.
//!
//! # The residual, stated exactly
//!
//! [`note_arraylist_classed_view_minted`] is sticky and one-way, but it fires
//! when the fallback view is MINTED, which is later than the call sites that may
//! already be bound to real bytecode. Those sites stay bound. So a run is exposed
//! iff its image has a real, non-stub `java/util/ArrayList` **and** lacks one of
//! its `java/util/HashMap$Values` siblings — both live in `java.base`, and no JDK
//! ships one without the other. The two configurations the fallback's own comment
//! names are already covered elsewhere: a synthetic-JDK build makes
//! `java/util/ArrayList` a compatibility stub, which fails term 3 of the
//! five-term predicate, and `--jdk-only` does not register the `ArrayList`
//! natives at all (measured: zero `java/util/ArrayList` rows in
//! `--dump-native-registry`).
//!
//! [`arraylist_view_fallback_count`] is what turns that reasoning into something
//! checkable: non-zero means this run took the arm, and any stale-view report
//! from it is explained.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static ARRAYLIST_CLASSED_VIEW: AtomicBool = AtomicBool::new(false);
static FALLBACK_MINTS: AtomicU64 = AtomicU64::new(0);

/// `true` once this run has minted a `Map` view whose class is exactly
/// `java/util/ArrayList`, so `ArrayList`'s natives must keep winning.
#[inline]
pub fn arraylist_classed_view_possible() -> bool {
    ARRAYLIST_CLASSED_VIEW.load(Ordering::Relaxed)
}

/// Record that `alloc_view_carrier` fell back to minting a view under
/// `java/util/ArrayList`. Sticky and one-way.
#[inline]
pub fn note_arraylist_classed_view_minted() {
    ARRAYLIST_CLASSED_VIEW.store(true, Ordering::Relaxed);
    FALLBACK_MINTS.fetch_add(1, Ordering::Relaxed);
}

/// How many views this run minted under `java/util/ArrayList`. Zero on every
/// image that has the carrier classes, which is every JDK.
pub fn arraylist_view_fallback_count() -> u64 {
    FALLBACK_MINTS.load(Ordering::Relaxed)
}

/// Test-only: put the latch back to its start state.
#[doc(hidden)]
pub fn reset_for_test() {
    ARRAYLIST_CLASSED_VIEW.store(false, Ordering::Relaxed);
    FALLBACK_MINTS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default licenses the yield. See the module header for why this is the
    /// right default and what the previous, inert one was.
    #[test]
    fn the_default_licenses_the_yield() {
        reset_for_test();
        assert!(!arraylist_classed_view_possible());
        assert_eq!(arraylist_view_fallback_count(), 0);
    }

    /// One-way: a fallback mint revokes the yield and nothing restores it.
    #[test]
    fn a_fallback_mint_revokes_the_yield_permanently() {
        reset_for_test();
        note_arraylist_classed_view_minted();
        assert!(arraylist_classed_view_possible());
        assert_eq!(arraylist_view_fallback_count(), 1);
        note_arraylist_classed_view_minted();
        assert!(arraylist_classed_view_possible());
        assert_eq!(arraylist_view_fallback_count(), 2);
        reset_for_test();
    }
}

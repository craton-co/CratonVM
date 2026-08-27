// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The validity epoch for FFM element-accessor fast-path verdicts.
//!
//! The verdicts themselves, and the reasoning for why a memo is the right
//! shape here at all, live in `cratonvm_native_builtins::ffm_fast`. Only the
//! counter lives here, because the two things that must retire a verdict sit in
//! crates that cannot see each other:
//!
//! * **freeing native memory** (`vm`), which pulls the block out from under a
//!   carrier that still points at it;
//! * **a collection** (`gc`), after which an object address no longer
//!   identifies the object it identified before — a reclaimed carrier's address
//!   can be handed to a different object, and a verdict is keyed by address.
//!
//! `types` is the leaf both depend on, so it is the only place a single counter
//! can live. Putting a second copy in either crate is the failure this whole
//! design exists to avoid.
//!
//! Both bumps are per-EVENT — one relaxed increment per free and per GC cycle —
//! never per access, and both are conservative: an unnecessary bump costs one
//! re-validation through the ordinary native path.

use std::sync::atomic::{AtomicU64, Ordering};

/// Starts at 1 so a zeroed or `Default`-constructed verdict can never match.
static FFM_EPOCH: AtomicU64 = AtomicU64::new(1);

/// The current epoch. A verdict is usable only while this is unchanged.
#[inline]
pub fn ffm_epoch() -> u64 {
    FFM_EPOCH.load(Ordering::Acquire)
}

/// Retire every published FFM fast-path verdict, on every thread.
#[inline]
pub fn bump_ffm_epoch() {
    FFM_EPOCH.fetch_add(1, Ordering::AcqRel);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_starts_non_zero_and_advances() {
        // Non-zero start is load-bearing: a verdict struct that was never
        // published reads as all-zero, and must not compare equal to a live
        // epoch.
        assert!(ffm_epoch() >= 1);
        let before = ffm_epoch();
        bump_ffm_epoch();
        assert!(
            ffm_epoch() > before,
            "a bump must make every prior verdict stale"
        );
    }
}

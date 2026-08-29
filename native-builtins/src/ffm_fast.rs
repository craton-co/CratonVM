// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The FFM element-accessor fast path: a validation memo the JIT can consult.
//!
//! # The problem this exists to solve
//!
//! `MemorySegment.getAtIndex`/`setAtIndex` are the FFM ELEMENT accessors — any
//! segment-backed array drives them one element at a time. Reached through the
//! ordinary native dispatch funnel they cost ~1158 ns/element, against ~0.8 ns
//! for a `short[]` element and ~303 ns for `Unsafe.getShort(long)` (a
//! maximally lean native through the SAME funnel). So ~300 ns is the funnel and
//! ~850 ns is [`crate::panama::pe_segment_access_addr`] and its callees, which
//! are ~10 `NativeContext` round-trips per element — a scope-liveness walk
//! (segment → arena → session → state), then the address and the byte size,
//! each re-probing the carrier's shape, each paying heap validation.
//!
//! Measured consequence: kfusion's 256³ TSDF volume is a segment-backed
//! `ShortArray`, so every voxel is one of these, and that app runs ~126x slower
//! than HotSpot.
//!
//! # Why this is a memo and not a second copy of the checks
//!
//! The obvious fix — teach the JIT to do the checks itself — is the one this
//! module deliberately does NOT take. The liveness model spans two synthetic
//! classes whose slot conventions are owned by two different files
//! (`Arena{[0]=open,[1]=session}` here, the session's own slots in
//! `phases_late::foreign_ffm`), a segment's slot 2 and slot 4 are REUSED with
//! different meanings by `ofArray` carriers, and the carrier shapes (2, 3, 6
//! and 8 slots) are told apart by `object_num_fields` rather than by class.
//!
//! That is not a hypothetical hazard. The W7-89 note on `PE_ARENA_CLASS`
//! records that this file once kept its own copy of the session's `state` slot
//! index, and that second copy is what made the liveness check silently DEAD in
//! Compatible mode. A copy of it in the JIT would fail the same way, and the
//! failure mode is a read of freed native memory.
//!
//! So the checks stay in exactly one place — the native — and this module only
//! records THAT they passed:
//!
//! * the native publishes [`note_validated`] at the point where it has already
//!   proven the carrier is a plain native segment with a live scope;
//! * the JIT consults [`is_validated`] and, on a hit, re-reads the carrier's
//!   address/size slots itself and does the load.
//!
//! The JIT therefore never encodes the liveness model, only the three slot
//! indices it re-reads — which `ffm_fast_slot_indices_match_the_carrier` pins
//! against this file's own constants.
//!
//! # Why re-reading the slots is required, not an optimisation
//!
//! The memo caches the VERDICT, never the address or the size. A carrier's
//! `ptr`/`size`/`offset` slots are ordinary mutable slots, so caching their
//! values would go stale the moment anything rewrites them; re-reading costs
//! three loads and removes that entire class of staleness from the design.
//!
//! # What makes the verdict safe to reuse
//!
//! A verdict is keyed by `(carrier address, epoch)` and is per-thread.
//!
//! * **The address identifies the object** only for as long as no GC has run:
//!   a reclaimed-and-reallocated object can land on an address a stale verdict
//!   names. So a GC cycle bumps the epoch.
//! * **The scope can close** under the carrier, which frees the native block
//!   while the carrier still points at it. So freeing native memory bumps the
//!   epoch.
//!
//! Both are coarse — one relaxed increment per GC and per free, never per
//! access — and both are conservative: a bump only ever costs a re-validation,
//! which is the ordinary native path.
//!
//! Per-thread because a verdict is only ever produced by a full checked access
//! this thread performed, which keeps the memo a plain `Cell` with no sharing
//! and no lock on the hot path.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// The current validation epoch.
///
/// The counter itself lives in `cratonvm_types::ffm_epoch` because the two
/// events that must retire a verdict — freeing native memory, and a collection
/// — happen in `vm` and `gc`, crates that cannot see each other. See that
/// module for why there is exactly one counter and not one per crate.
#[inline]
pub fn epoch() -> u64 {
    cratonvm_types::ffm_epoch::ffm_epoch()
}

/// Invalidate every published verdict, on every thread.
///
/// Call this from anything that can (a) free native memory a carrier points at,
/// or (b) move or reclaim the carrier object itself. Both are covered at their
/// choke points: `free_native_memory` and the collector's cycle end.
///
/// Deliberately blunt. A verdict is cheap to rebuild — it costs one ordinary
/// native access — and the cost of being too clever here is a read of freed
/// memory.
#[inline]
pub fn bump_epoch() {
    cratonvm_types::ffm_epoch::bump_ffm_epoch();
}

/// One published verdict.
#[derive(Clone, Copy)]
struct Validated {
    /// The carrier object's address.
    carrier: u64,
    /// The epoch the verdict was published at.
    epoch: u64,
    /// A full checked READ of this carrier succeeded at that epoch.
    read: bool,
    /// A full checked WRITE succeeded — i.e. the carrier is also not read-only.
    write: bool,
}

thread_local! {
    /// The last carrier a full checked access succeeded on, on this thread.
    ///
    /// One entry, not a map: the workloads this exists for sweep ONE segment in
    /// a loop (a TSDF volume, a tensor, a pooled buffer), so a single slot has
    /// the same hit rate as a map and costs a compare instead of a hash. A
    /// second interleaved segment simply keeps missing, which is the ordinary
    /// native path and therefore correct, only not faster.
    static LAST_VALIDATED: Cell<Option<Validated>> = const { Cell::new(None) };
}

/// Record that a FULL, checked access to `carrier` just succeeded.
///
/// `write` says the access also cleared the read-only check. Called from the
/// native, at a point where every shape and liveness check has already passed.
///
/// A later `write` verdict on a carrier already validated for reads is merged
/// rather than replacing it, so a read-then-write loop does not alternate
/// between two verdicts and miss on every access.
pub fn note_validated(carrier: u64, write: bool) {
    if carrier == 0 {
        return;
    }
    let now = epoch();
    LAST_VALIDATED.with(|slot| {
        let merged_write = match slot.get() {
            Some(prev) if prev.carrier == carrier && prev.epoch == now => prev.write || write,
            _ => write,
        };
        slot.set(Some(Validated {
            carrier,
            epoch: now,
            read: true,
            write: merged_write,
        }));
    });
}

/// Has `carrier` been fully validated on this thread, at the current epoch?
///
/// `want_write` asks for the stronger verdict (not read-only). Returns `false`
/// for anything not positively known, which is what makes every unknown case
/// fall back to the ordinary native path.
#[inline]
pub fn is_validated(carrier: u64, want_write: bool) -> bool {
    if carrier == 0 {
        return false;
    }
    LAST_VALIDATED.with(|slot| match slot.get() {
        Some(v) => v.carrier == carrier && v.epoch == epoch() && v.read && (!want_write || v.write),
        None => false,
    })
}

/// Drop this thread's verdict. For tests, and for any embedder that needs a
/// hard reset without waiting for an epoch bump.
pub fn forget_validated() {
    LAST_VALIDATED.with(|slot| slot.set(None));
}

// ---------------------------------------------------------------------------
// Counters — engagement, not decoration
// ---------------------------------------------------------------------------
//
// A fast path that is structurally present but never taken looks exactly like
// a fast path that works, right up until someone prices it. These separate
// "the JIT asked" from "the JIT was allowed", so a disappointing measurement
// can be attributed instead of guessed at.

static FAST_HITS: AtomicU64 = AtomicU64::new(0);
static FAST_MISSES: AtomicU64 = AtomicU64::new(0);

/// Count one consult of [`is_validated`] from compiled code.
///
/// Two relaxed increments and nothing else. This runs per ELEMENT, so it must
/// stay that cheap — an earlier version also did a modulo and an env lookup
/// here to print progress, which is real work on the hot path this exists to
/// make fast. Read the totals with [`fast_path_counts`] instead.
#[inline]
pub fn note_fast_consult(hit: bool) {
    if hit {
        FAST_HITS.fetch_add(1, Ordering::Relaxed);
    } else {
        FAST_MISSES.fetch_add(1, Ordering::Relaxed);
    }
}

static PUBLISHES: AtomicU64 = AtomicU64::new(0);

/// Count one verdict publish from the native. One relaxed increment.
#[inline]
pub fn note_publish() {
    PUBLISHES.fetch_add(1, Ordering::Relaxed);
}

/// Verdicts published by the native — the denominator that says whether the
/// compiled side ever ASKED. `publishes` high with `fast_hits` zero is the
/// signature of an intrinsic registered in a door the workload does not use,
/// which is how both wiring defects in this feature were found.
pub fn publish_count() -> u64 {
    PUBLISHES.load(Ordering::Relaxed)
}

/// `(hits, misses)` for the compiled FFM element fast path.
pub fn fast_path_counts() -> (u64, u64) {
    (
        FAST_HITS.load(Ordering::Relaxed),
        FAST_MISSES.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_is_scoped_to_its_carrier_and_epoch() {
        forget_validated();
        note_validated(0x1000, false);
        assert!(is_validated(0x1000, false), "the published carrier hits");
        assert!(
            !is_validated(0x2000, false),
            "a different carrier must never hit a single-slot memo"
        );
        assert!(
            !is_validated(0x1000, true),
            "a read verdict must not answer a write question"
        );

        // Anything that could free the block or move the carrier invalidates.
        bump_epoch();
        assert!(
            !is_validated(0x1000, false),
            "an epoch bump must retire every published verdict"
        );
    }

    #[test]
    fn a_write_verdict_merges_rather_than_replacing_the_read_one() {
        forget_validated();
        note_validated(0x1000, false);
        note_validated(0x1000, true);
        assert!(is_validated(0x1000, false));
        assert!(
            is_validated(0x1000, true),
            "the write verdict must be retained"
        );
        // And the merge must not manufacture a write verdict for a carrier that
        // only ever passed the read checks.
        note_validated(0x3000, false);
        assert!(!is_validated(0x3000, true));
    }

    #[test]
    fn a_zero_carrier_is_never_validated() {
        forget_validated();
        note_validated(0, true);
        assert!(!is_validated(0, false), "null must not be fast-pathed");
    }
}

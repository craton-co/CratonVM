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

/// Verdict slots per thread. See [`VALIDATED`].
const VERDICT_WAYS: usize = 4;

/// `CRATONVM_FFM_VERDICT_WAYS` clamps how many of [`VERDICT_WAYS`] are used.
///
/// `1` restores the single-slot behaviour this cache replaced, which is the
/// control arm for pricing the change on one binary. Anything outside
/// `1..=VERDICT_WAYS` is clamped into it.
fn effective_ways() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_FFM_VERDICT_WAYS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(VERDICT_WAYS)
            .clamp(1, VERDICT_WAYS)
    })
}

thread_local! {
    /// Carriers a full checked access has succeeded on, on this thread.
    ///
    /// # This was ONE slot, and the workload it was written for thrashed it
    ///
    /// AUDIT 2026-09-02. The original comment here read: "One entry, not a
    /// map: the workloads this exists for sweep ONE segment in a loop (a TSDF
    /// volume, a tensor, a pooled buffer), so a single slot has the same hit
    /// rate as a map... A second interleaved segment simply keeps missing,
    /// which is the ordinary native path and therefore correct, only not
    /// faster."
    ///
    /// The TSDF volume is kfusion, and kfusion is the app this whole fast
    /// path was built for. Measured there once the engagement census had a
    /// reporter (three frames of `kfusion.java.Benchmark`):
    ///
    /// ```text
    ///   consults=129,445,168  hits=92,226,077 (71.2%)  misses=37,219,091
    ///   native verdicts published=38,306,106
    /// ```
    ///
    /// 38.3M publishes against 129M consults — better than one full native
    /// verdict for every four elements. The same bench that sweeps a SINGLE
    /// segment publishes once for 10.5M consults, so this is not the fast
    /// path failing, it is the single slot being evicted by the second
    /// carrier and rebuilt, forever. Integration alternates the volume with
    /// the images it reads, and "only not faster" turned out to mean "pays
    /// the expensive path 28.8% of the time".
    ///
    /// Four ways, checked in order, most-recently-published first. A hit is
    /// at most four `u64` compares against a `Cell` copy, which is still far
    /// cheaper than the native round-trip it avoids, and a workload that
    /// really does sweep one segment still hits on the first compare.
    ///
    /// An empty slot is `carrier == 0`, which [`note_validated`] refuses to
    /// store, so no real carrier can collide with it.
    static VALIDATED: [Cell<Validated>; VERDICT_WAYS] = const {
        [const {
            Cell::new(Validated { carrier: 0, epoch: 0, read: false, write: false })
        }; VERDICT_WAYS]
    };
    /// Next way to evict, round-robin. Round-robin rather than
    /// least-recently-used: LRU needs a per-access write to record the use,
    /// and this path is per ELEMENT.
    static VICTIM: Cell<usize> = const { Cell::new(0) };
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
    let ways = effective_ways();
    VALIDATED.with(|slots| {
        // Refresh this carrier's own way if it has one, so a read-then-write
        // loop merges into one entry instead of consuming two ways.
        for slot in slots.iter().take(ways) {
            let prev = slot.get();
            if prev.carrier == carrier && prev.epoch == now {
                slot.set(Validated {
                    carrier,
                    epoch: now,
                    read: true,
                    write: prev.write || write,
                });
                return;
            }
        }
        // Otherwise take a free way, preferring one that is empty or stale
        // before evicting a live verdict.
        let victim = slots
            .iter()
            .take(ways)
            .position(|s| {
                let v = s.get();
                v.carrier == 0 || v.epoch != now
            })
            .unwrap_or_else(|| {
                VICTIM.with(|v| {
                    let i = v.get() % ways;
                    v.set((i + 1) % ways);
                    i
                })
            });
        slots[victim].set(Validated {
            carrier,
            epoch: now,
            read: true,
            write,
        });
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
    let now = epoch();
    let ways = effective_ways();
    VALIDATED.with(|slots| {
        for slot in slots.iter().take(ways) {
            let v = slot.get();
            if v.carrier == carrier && v.epoch == now && v.read && (!want_write || v.write) {
                return true;
            }
        }
        false
    })
}

/// Drop this thread's verdict. For tests, and for any embedder that needs a
/// hard reset without waiting for an epoch bump.
pub fn forget_validated() {
    VALIDATED.with(|slots| {
        for slot in slots.iter() {
            slot.set(Validated {
                carrier: 0,
                epoch: 0,
                read: false,
                write: false,
            });
        }
    });
    VICTIM.with(|v| v.set(0));
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

/// One line at exit, when this process touched an FFM segment at all.
///
/// # This existed as three counters that nothing printed
///
/// AUDIT 2026-09-02. The header of this module says it plainly -- "a fast
/// path that is structurally present but never taken looks exactly like a
/// fast path that works, right up until someone prices it" -- and then
/// `fast_path_counts` and `publish_count` had no caller anywhere in the
/// workspace. The counters were correct and invisible, which is the same
/// state as not having them.
///
/// How to read it:
///
/// * `publishes` is the denominator: verdicts the NATIVE published. High
///   publishes with zero consults means compiled code never asked, i.e.
///   the intrinsic is registered in a door this workload does not use, or
///   the hot code is not compiled at all.
/// * `hits` vs `misses` is whether the fast path, once asked, was
///   ALLOWED. Misses are the declines: a heap carrier, a closed scope, an
///   index the bounds check refused.
///
/// Silent when nothing published and nothing consulted, so a run that
/// never touches FFM does not grow a line.
pub fn exit_summary() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let (hits, misses) = fast_path_counts();
    let publishes = publish_count();
    if hits + misses + publishes == 0 {
        return;
    }
    ONCE.call_once(|| {
        let consults = hits + misses;
        eprintln!(
            "[cratonvm] ffm element fast path: consults={consults} hits={hits} \
             misses={misses} ({:.1}% of consults hit); native verdicts \
             published={publishes}",
            100.0 * hits as f64 / consults.max(1) as f64,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests in this module.
    ///
    /// The verdict table is per-THREAD but the epoch is a process-global,
    /// and `a_verdict_is_scoped_to_its_carrier_and_epoch` bumps it. Without
    /// this, that bump lands in the middle of a sibling running on another
    /// thread and invalidates verdicts it just published — which is exactly
    /// how the two carrier tests below failed in the suite and passed when
    /// run alone.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the lock and start from an empty table.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        forget_validated();
        g
    }

    #[test]
    fn a_verdict_is_scoped_to_its_carrier_and_epoch() {
        let _g = guard();
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
        let _g = guard();
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

    /// The defect the census found: two carriers used alternately, which is
    /// what kfusion's integration stage does (the TSDF volume and the images
    /// it reads). With one slot each switch evicted the other and republished.
    ///
    /// Verified able to fail: with `CRATONVM_FFM_VERDICT_WAYS=1` — the
    /// single-slot behaviour this replaced — the second assertion fails on
    /// the first alternation.
    #[test]
    fn two_carriers_used_alternately_both_stay_validated() {
        let _g = guard();
        let a = 0x1_0000u64;
        let b = 0x2_0000u64;
        note_validated(a, false);
        note_validated(b, false);
        for _ in 0..8 {
            assert!(is_validated(a, false), "carrier A was evicted by B");
            assert!(is_validated(b, false), "carrier B was evicted by A");
        }
    }

    /// Four is the width, so a fifth carrier must cost one of the others —
    /// but only one, and the survivors must stay valid. A cache that dropped
    /// everything on an overflow would be the single slot again with extra
    /// steps.
    #[test]
    fn a_fifth_carrier_evicts_one_way_not_the_table() {
        let _g = guard();
        for i in 1..=(VERDICT_WAYS as u64 + 1) {
            note_validated(i * 0x1000, false);
        }
        let live = (1..=(VERDICT_WAYS as u64 + 1))
            .filter(|i| is_validated(i * 0x1000, false))
            .count();
        assert_eq!(
            live, VERDICT_WAYS,
            "expected exactly {VERDICT_WAYS} carriers to survive, got {live}"
        );
        assert!(
            is_validated((VERDICT_WAYS as u64 + 1) * 0x1000, false),
            "the most recent carrier must always be the one that is kept"
        );
    }

    #[test]
    fn a_zero_carrier_is_never_validated() {
        let _g = guard();
        note_validated(0, true);
        assert!(!is_validated(0, false), "null must not be fast-pathed");
    }
}

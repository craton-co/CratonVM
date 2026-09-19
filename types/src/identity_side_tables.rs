// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Eviction channel for native side tables keyed by identity hash code.
//!
//! A native builtin that needs per-instance state it cannot put in a Java field
//! keeps that state in a process-global map keyed by
//! `NativeContext::identity_hash_code`. The identity hash is the right key --
//! it is stable across relocation, where a raw address is not -- but it comes
//! with an obligation nothing in the tree discharged: **the entry has to die
//! when the object does.**
//!
//! Until this module existed, none of them did. Measured on
//! `probes/RandomLeak.java` (allocate N `java.util.Random`, drop every one, full
//! GC each round): the Java heap stayed flat to the kilobyte across 8,000,000
//! dead instances -- they are collected, correctly -- while process private
//! bytes climbed ~39 bytes per instance, forever. That is invisible to `-Xmx`,
//! to `Runtime.freeMemory`, and to every Java-side heap metric an operator would
//! look at, and it makes any long-lived service that calls `new Random()` per
//! request grow without bound. See
//! `jpalargeblob-random-state-side-table-FIXED-20260830.md`.
//!
//! # Why the eviction signal comes from the collector
//!
//! The object's death is the event, and the collector is the only subsystem
//! that observes it. This registry lives in `cratonvm-types` for the same
//! reason [`crate::loader_pin`] does: `gc` and `native-builtins` both depend on
//! this crate and on neither each other, so a global here is the one place they
//! can meet without a new crate dependency.
//!
//! The direction of travel is: `native-builtins` registers an evictor at table
//! construction; the collector calls [`evict_dead`] once per cycle with the
//! identity hashes of the objects it just reclaimed.
//!
//! # Why the hashes come from the mark word, and what that rules out
//!
//! A prior write-up of this defect prescribed hooking
//! `gc::compact_header::HashCodeTable::update_after_gc`, on the grounds that it
//! is "the one site that knows which hashes just died". It is not: that table
//! has no production consumer (its own doc comment says so). The identity hash
//! this VM actually hands out lives in the object's **mark word**, installed
//! lazily by one CAS in `ObjectHeader::mark_word_identity_hash`. There is no
//! hash table to prune, so the collector has to read the dying object's header
//! before it destroys it.
//!
//! Two consequences fall out of that, and both are load-bearing:
//!
//! * **The filter is free and exact.** `ObjectHeader::neutral_hash` returns `0`
//!   for an object whose hash was never requested, and an object whose identity
//!   hash was never requested cannot be a key in any of these tables. So the
//!   collector reports only objects that were actually hashed, which on a
//!   typical heap is a small minority.
//! * **A hash displaced by monitor inflation is not reported.** `neutral_hash`
//!   is `0` for every non-`NEUTRAL` mark word, because a `THIN_LOCKED` payload
//!   is an owner and recursion count and an `INFLATED` one is a pointer.
//!   Recovering those would mean following a dying object's monitor pointer
//!   during the sweep, which is exactly the use-after-free the sweep's ordering
//!   rules exist to prevent. The cost of not doing it is that a hashed object
//!   that was *also* monitor-inflated leaks its side-table entry, as before.
//!   That is a strict improvement on leaking all of them, and the shape is rare:
//!   it needs a `synchronized` block on the very object whose native state is
//!   side-tabled.
//!
//! # Hash reuse
//!
//! Identity hashes are minted from a counter truncated to 31 bits, so a process
//! that mints more than `2^31` of them wraps and a live object can share a hash
//! with a dead one. Evicting on the dead one's death then drops the live one's
//! entry, and its native state resets.
//!
//! This is not a hazard eviction introduces -- it is one eviction *reduces*.
//! Without eviction the same wrap makes a new object silently inherit a dead
//! stranger's generator state, which is the worse of the two failures and lasts
//! for the rest of the process. Neither is reachable before 2^31 hashed objects.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::RwLock;

/// An eviction callback: "these identity hashes belong to objects that have
/// just been reclaimed; drop any entry you hold for them".
///
/// A plain `fn` pointer rather than a boxed closure -- every registrant is a
/// free function over a `static` table, so there is no captured state to hold,
/// and this keeps the registry allocation-free and `const`-constructible.
pub type Evictor = fn(&[i32]);

static EVICTORS: RwLock<Vec<Evictor>> = RwLock::new(Vec::new());

/// Fast path for the collector: `false` until the first registration, so a
/// sweep on a VM that never touched one of these tables pays one relaxed load
/// for the whole cycle rather than a lock per dead object.
static ANY_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Total identity hashes handed to [`evict_dead`] since process start.
static REPORTED: AtomicU64 = AtomicU64::new(0);

/// Total entries actually removed, summed across evictors.
static EVICTED: AtomicU64 = AtomicU64::new(0);

/// Register `evictor` to be called with the identity hashes of reclaimed
/// objects.
///
/// Idempotence is the caller's business: registering the same function twice
/// makes it run twice per cycle, which for a `remove`-shaped evictor is
/// harmless but wasteful. Every in-tree registrant calls this from a
/// `OnceLock`/`Once` init path.
pub fn register(evictor: Evictor) {
    EVICTORS.write().push(evictor);
    // RELEASE so that a collector thread observing `true` also observes the
    // push above; `evict_dead`'s read of `EVICTORS` is behind the lock, so this
    // only has to order the flag against the vector's contents.
    ANY_REGISTERED.store(true, Ordering::Release);
}

/// Whether any side table has registered an evictor.
///
/// The collector calls this *before* building its list of dead hashes: with no
/// registrant there is nothing to tell, and the per-dead-object header read
/// that builds the list should not happen at all.
#[inline]
pub fn any_registered() -> bool {
    ANY_REGISTERED.load(Ordering::Acquire)
}

/// Hand every registered side table the identity hashes of objects the
/// collector has just reclaimed.
///
/// Called once per GC cycle with the whole batch rather than once per object:
/// each evictor takes its table's write lock exactly once, which is what keeps
/// this off the per-object cost of a sweep.
pub fn evict_dead(hashes: &[i32]) {
    if hashes.is_empty() || !any_registered() {
        return;
    }
    REPORTED.fetch_add(hashes.len() as u64, Ordering::Relaxed);
    // Cloned out of the lock: an evictor takes its own table's lock, and
    // holding the registry lock across that would put two unrelated locks in a
    // fixed order for no reason. The vector is a handful of fn pointers written
    // only at init.
    let evictors: Vec<Evictor> = EVICTORS.read().clone();
    for e in evictors {
        e(hashes);
    }
}

/// Record `n` removed entries, for [`census`]. Evictors call this so the
/// counter reflects entries that really existed, not hashes offered.
#[inline]
pub fn note_evicted(n: usize) {
    if n != 0 {
        EVICTED.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// `(hashes reported by the collector, entries actually removed)`.
///
/// The gap between the two is the whole point of reading it: `reported` counts
/// dead objects that carried an identity hash, `evicted` counts the ones some
/// table was really holding state for. A zero `evicted` beside a large
/// `reported` says the channel is wired and the tables are empty; a zero
/// `reported` says the collector is not reporting, which is a different bug.
pub fn census() -> (u64, u64) {
    (
        REPORTED.load(Ordering::Relaxed),
        EVICTED.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static SEEN: AtomicUsize = AtomicUsize::new(0);

    fn counting_evictor(hashes: &[i32]) {
        SEEN.fetch_add(hashes.len(), Ordering::Relaxed);
        note_evicted(hashes.len());
    }

    /// ONE test, not several.
    ///
    /// The registry is a process-global and `cargo test` runs a crate's tests
    /// on parallel threads in ONE process, so a second test that also called
    /// `register(counting_evictor)` would leave the evictor registered twice
    /// and every exact count here would be doubled -- an order-dependent
    /// failure that looks like a flake. Keeping the registration in a single
    /// test is what licenses asserting exact numbers below.
    #[test]
    fn the_evictor_sees_each_batch_once_and_an_empty_batch_is_skipped() {
        // Nothing else in this crate registers, so this is the only registrant
        // and each `evict_dead` below calls it exactly once.
        register(counting_evictor);
        assert!(any_registered(), "registration must arm the fast-path flag");

        let (r0, e0) = census();
        let seen0 = SEEN.load(Ordering::Relaxed);

        // An empty batch must not reach the evictor, nor move the census: the
        // sweep calls `evict_dead` at the end of EVERY cycle, and most cycles
        // reclaim nothing that was ever hashed.
        evict_dead(&[]);
        assert_eq!(
            SEEN.load(Ordering::Relaxed),
            seen0,
            "empty batch must not call back"
        );
        assert_eq!(census(), (r0, e0), "empty batch must not move the census");

        evict_dead(&[7, 8]);
        assert_eq!(
            SEEN.load(Ordering::Relaxed),
            seen0 + 2,
            "each hash in the batch reaches the evictor exactly once"
        );

        // `reported` counts what the collector offered; `evicted` counts what an
        // evictor said it really removed. `counting_evictor` claims all of them,
        // so here the two move together -- the production evictors note only
        // real removals, which is what makes the gap between them readable.
        let (r1, e1) = census();
        assert_eq!(r1, r0 + 2, "reported must count the offered hashes");
        assert_eq!(e1, e0 + 2, "evicted must count what the evictor removed");
    }
}
